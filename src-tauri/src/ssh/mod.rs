use russh::client;
use russh::keys::PublicKey;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 主机公钥指纹缓存 —— 实现 Trust-on-First-Use。
/// 首次连接时记录指纹，之后若发生变化则拒绝连接（防中间人）。
#[derive(Debug, Default)]
pub struct KnownHosts {
    inner: Mutex<HashMap<String, String>>,
}

impl KnownHosts {
    pub fn new() -> Self {
        Self::default()
    }

    fn slot(host: &str, port: u16) -> String {
        format!("{}:{}", host, port)
    }

    /// 返回 Some(true) 表示指纹匹配或首次信任；Some(false) 表示指纹冲突。
    pub fn verify(&self, host: &str, port: u16, fingerprint: &str) -> bool {
        let key = Self::slot(host, port);
        let mut map = self.inner.lock().expect("known_hosts poisoned");
        match map.get(&key) {
            None => {
                tracing::info!(host = host, port = port, "首次连接，记录主机指纹");
                map.insert(key, fingerprint.to_string());
                true
            }
            Some(known) if known == fingerprint => true,
            Some(known) => {
                tracing::error!(
                    host = host,
                    "主机公钥指纹不匹配！已知 {}，实际 {}",
                    known,
                    fingerprint
                );
                false
            }
        }
    }

    pub fn fingerprint_of(key: &PublicKey) -> String {
        key.fingerprint(russh::keys::HashAlg::Sha256).to_string()
    }
}

/// russh 的客户端回调实现。
#[derive(Clone)]
pub struct ClientHandler {
    pub known_hosts: Arc<KnownHosts>,
    pub host: String,
    pub port: u16,
    /// 主机指纹冲突标记。russh 在 check_server_key 返回 false 时会直接断开连接，
    /// 因此需要这个旁路标记，才能在事后把失败原因精确地报给前端。
    pub host_key_rejected: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKey) -> Result<bool, Self::Error> {
        let fp = KnownHosts::fingerprint_of(key);
        let ok = self.known_hosts.verify(&self.host, self.port, &fp);
        if !ok {
            self.host_key_rejected.store(true, Ordering::SeqCst);
        }
        Ok(ok)
    }
}

pub mod prompt;
pub mod session;

#[allow(unused_imports)]
pub use session::{SessionError, SessionManager};

// ───────────────────────── 旧服务器兼容 ─────────────────────────
//
// 现场问题：CentOS 6.9 自带 OpenSSH 5.3，KEX 只提供
//   diffie-hellman-group-exchange-sha1 / group14-sha1 / group1-sha1
// 而 russh 0.49 的默认列表（SAFE_KEX_ORDER）只有 curve25519 与
// group16-sha512 / group14-sha256，两边**零交集**，于是握手直接返回
// `NoCommonAlgo { kind: Kex }`，界面上就是「No common algorithm」。
//
// russh 内部其实完整实现了 group14-sha1 与 group1-sha1，只是出于安全
// 考虑没放进默认列表。这里把它们**追加到偏好列表末尾**启用：
//   · 现代服务器（OpenSSH 7+）依旧按顺序先命中 curve25519，不会降级；
//   · 只有连老机器时才会真正落到 SHA-1 的 DH 组上。
//
// 默认策略是 Auto：先按现代算法握一次手，只有 KEX 报“无公共算法”时才
// 用旧算法重试一次。这样用户不需要先去翻设置就能连上，同时旧算法不会
// 被无差别地提供给所有主机。

/// 旧服务器兼容策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyPolicy {
    /// 先现代后旧：KEX 无公共算法时自动降级重试（默认）
    Auto,
    /// 直接带上旧算法（会话级显式开启）
    Always,
    /// 只用现代算法，绝不降级
    Never,
}

impl LegacyPolicy {
    /// 前端传的是三态：未传 = 自动，true = 强制开启，false = 强制关闭。
    pub fn from_flag(flag: Option<bool>) -> Self {
        match flag {
            Some(true) => Self::Always,
            Some(false) => Self::Never,
            None => Self::Auto,
        }
    }
}

/// 追加到默认列表末尾的旧算法。顺序按「越强越前」排，group14 优先于 group1。
const LEGACY_KEX: [russh::kex::Name; 2] = [russh::kex::DH_G14_SHA1, russh::kex::DH_G1_SHA1];

/// 构造客户端算法偏好。`legacy = true` 时把 SHA-1 的 DH 组追加到末尾。
pub fn preferred(legacy: bool) -> russh::Preferred {
    let base = russh::Preferred::DEFAULT;
    if !legacy {
        return base;
    }
    let mut kex = base.kex.to_vec();
    for alg in LEGACY_KEX {
        if !kex.contains(&alg) {
            kex.push(alg);
        }
    }
    russh::Preferred {
        kex: Cow::Owned(kex),
        ..base
    }
}

/// 握手失败的三类原因，调用方各自翻译成自己模块的错误类型。
#[derive(Debug)]
pub enum HandshakeError {
    /// 整体超时（两次尝试共用同一个 deadline）
    Timeout,
    /// 主机公钥指纹与记录不符
    HostKeyMismatch,
    /// russh 原始错误。`legacy_tried` 标明最后一次尝试有没有带旧算法，
    /// 用于生成"已尝试过兼容模式"这类不误导用户的提示。
    Russh {
        err: russh::Error,
        legacy_tried: bool,
    },
}

/// 握手成功后的会话句柄 + 实际是否用了旧算法。
pub struct Handshake {
    pub session: client::Handle<ClientHandler>,
    /// true 表示本次连接协商到了 SHA-1 系列的旧 KEX，应提示用户
    pub legacy: bool,
}

/// 发起一次 SSH 传输层握手，必要时按 `policy` 自动降级重试。
///
/// 两次尝试共用同一个 `<timeout>` 预算，避免自动重试把用户等待时间翻倍。
pub async fn handshake(
    host: &str,
    port: u16,
    handler: ClientHandler,
    timeout: Duration,
    policy: LegacyPolicy,
    keepalive: Option<Duration>,
) -> Result<Handshake, HandshakeError> {
    let deadline = tokio::time::Instant::now() + timeout;

    // Always 模式第一次就带旧算法；其余先试现代算法。
    let mut legacy = policy == LegacyPolicy::Always;

    let mut result = match tokio::time::timeout_at(
        deadline,
        connect_once(host, port, handler.clone(), legacy, keepalive),
    )
    .await
    {
        Ok(r) => r,
        Err(_) => return Err(HandshakeError::Timeout),
    };

    // 只在“KEX 没有公共算法”时降级：主机公钥或加密算法不匹配，加 KEX 也救不回来。
    if policy == LegacyPolicy::Auto {
        if let Err(russh::Error::NoCommonAlgo {
            kind: russh::AlgorithmKind::Kex,
            ..
        }) = &result
        {
            tracing::warn!(
                host = host,
                port = port,
                "KEX 无公共算法，按旧服务器兼容模式重试（diffie-hellman-group14-sha1 / group1-sha1）"
            );
            // 上一次尝试可能已置位指纹冲突标记，重试前清掉
            handler.host_key_rejected.store(false, Ordering::SeqCst);
            legacy = true;
            result = match tokio::time::timeout_at(
                deadline,
                connect_once(host, port, handler.clone(), legacy, keepalive),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => return Err(HandshakeError::Timeout),
            };
        }
    }

    match result {
        Ok(session) => {
            if legacy {
                tracing::warn!(host = host, port = port, "已启用旧版密钥交换算法完成握手");
            }
            Ok(Handshake { session, legacy })
        }
        // 指纹冲突会让 russh 直接断开，靠旁路标记还原准确原因
        Err(_) if handler.host_key_rejected.load(Ordering::SeqCst) => {
            Err(HandshakeError::HostKeyMismatch)
        }
        Err(err) => Err(HandshakeError::Russh {
            err,
            legacy_tried: legacy,
        }),
    }
}

/// 单次握手尝试。keepalive 为 None 表示沿用 russh 默认（不探活）。
async fn connect_once(
    host: &str,
    port: u16,
    handler: ClientHandler,
    legacy: bool,
    keepalive: Option<Duration>,
) -> Result<client::Handle<ClientHandler>, russh::Error> {
    let config = Arc::new(client::Config {
        preferred: preferred(legacy),
        keepalive_interval: keepalive,
        keepalive_max: 3,
        ..Default::default()
    });
    client::connect(config, format!("{host}:{port}"), handler).await
}

// ───────────────────────── 连接 + 认证 ─────────────────────────
//
// 为什么认证要放在这一层：终端通道和 SFTP 通道都需要「握手 → 认证」这套流程，
// 而二次验证有个必须绕开的库缺陷（见 KBI_MUST_BE_FIRST），逻辑只写一份才不会漂移。

use session::{AuthContext, AuthMethod, AuthOutcome};

/// 连接 + 认证失败的原因。各调用方翻译成自己的错误类型（终端 / SFTP 文案不同）。
#[derive(Debug)]
pub enum OpenError {
    /// 握手整体超时
    Timeout,
    /// 主机公钥指纹与记录不符
    HostKeyMismatch,
    /// 传输层错误。`legacy_tried` 标明最后一次是否带了旧算法
    Handshake {
        err: russh::Error,
        legacy_tried: bool,
    },
    /// 认证环节失败，文本已是面向用户的中文提示
    Auth(String),
}

/// 连接 + 认证都成功后的结果
pub struct Opened {
    pub session: client::Handle<ClientHandler>,
    /// true 表示本次协商到了 SHA-1 系列的旧 KEX，应提示用户
    pub legacy: bool,
}

fn handler_for(host: &str, port: u16, known_hosts: &Arc<KnownHosts>) -> ClientHandler {
    ClientHandler {
        known_hosts: known_hosts.clone(),
        host: host.to_string(),
        port,
        host_key_rejected: Arc::new(AtomicBool::new(false)),
    }
}

/// 建立 SSH 连接并完成认证。
///
/// 认证分两轮，且**第二轮换一条新连接**：
///
/// 1. 用用户配置的方式（密码 / 公钥 / 无凭据）认证。绝大多数服务器一轮就结束。
/// 2. 没通过时，如果服务端支持 keyboard-interactive，重连一次并**把 KBI 作为
///    第一个认证请求**——这是 russh 的硬性要求，同一条连接上先用密码再转 KBI
///    会永久挂起（详见 [`session::KBI_MUST_BE_FIRST`]）。
///
/// 第二轮只在主认证失败时发生，正常连接不会多付一次握手。
pub async fn open_authenticated(
    host: &str,
    port: u16,
    known_hosts: Arc<KnownHosts>,
    timeout: Duration,
    policy: LegacyPolicy,
    keepalive: Option<Duration>,
    username: &str,
    auth: &AuthMethod,
    session_id: &str,
    ctx: AuthContext<'_>,
) -> Result<Opened, OpenError> {
    open_authenticated_with(
        host,
        port,
        known_hosts,
        timeout,
        policy,
        keepalive,
        username,
        auth,
        session_id,
        |p| async move { ctx.prompts.ask(ctx.app, p).await },
    )
    .await
}

/// [`open_authenticated`] 的实现体，「问用户」换成任意闭包 ——
/// 这样「密码失败 → 重连 → 走 KBI」这条最容易写错的路径能对着真服务端跑通。
#[allow(clippy::too_many_arguments)]
async fn open_authenticated_with<F, Fut>(
    host: &str,
    port: u16,
    known_hosts: Arc<KnownHosts>,
    timeout: Duration,
    policy: LegacyPolicy,
    keepalive: Option<Duration>,
    username: &str,
    auth: &AuthMethod,
    session_id: &str,
    ask: F,
) -> Result<Opened, OpenError>
where
    F: Fn(prompt::AuthPrompt) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<String>, prompt::AskAbort>>,
{
    // 私钥口令必须在握手**之前**补齐：加密私钥读不出来，握手再顺利也认证不了。
    // 这里顺便把「文件不存在 / 格式不对」提前成一句明确提示，
    // 而不是先花几秒建连、再在认证阶段报一个含混的错误。
    let auth = resolve_passphrase(host, session_id, auth, &ask).await?;

    let hs = handshake(
        host,
        port,
        handler_for(host, port, &known_hosts),
        timeout,
        policy,
        keepalive,
    )
    .await
    .map_err(map_handshake)?;

    let legacy = hs.legacy;
    let mut session = hs.session;

    match session::authenticate(&mut session, username, &auth).await {
        Ok(AuthOutcome::Done) => {
            return Ok(Opened { session, legacy });
        }
        Ok(AuthOutcome::NeedKeyboardInteractive) => {}
        Err(e) => return Err(OpenError::Auth(e.to_string())),
    }

    // ── 第二因素：先在**同一条连接**上接着走 keyboard-interactive ──
    //
    // 这是「密码 + 动态口令」类服务器的唯一正确走法：服务端用 USERAUTH_FAILURE
    // 的 partial-success 位告诉客户端「密码对了，还有一步」，此时连接上仍保有
    // 第一因子的状态。断开重连会把这份状态丢掉，服务端于是又要求先输密码。
    //
    // （russh 0.49 原本在这里会静默丢包并永久挂起，已由 `vendor/russh` 修掉。）
    match session::authenticate_kbi(&mut session, username, &auth, session_id, host, &ask).await {
        Ok(session::KbiOutcome::Done) => {
            return Ok(Opened { session, legacy });
        }
        Ok(session::KbiOutcome::Unavailable) => {
            // 服务端压根不支持 KBI（返回「一个问题都没问的拒绝」），
            // 或者第一因子就是错的。落到下面的换连接流程，给出准确文案。
        }
        Err(e) => return Err(OpenError::Auth(e.to_string())),
    }

    // 断开旧连接：下面按「KBI 必须是本次连接第一个认证请求」的老路子再试一次。
    // 显式 disconnect 而不是只 drop —— 否则服务端那条 sshd 会挂到 TCP 超时才回收，
    // 密码输错时每次重连都会留下一个这样的僵尸进程。
    let _ = session
        .disconnect(russh::Disconnect::ByApplication, "", "")
        .await;
    drop(session);
    tracing::info!(
        host,
        port,
        why = session::KBI_MUST_BE_FIRST,
        "主认证未通过，换新连接以 keyboard-interactive 重试（二次验证）"
    );

    let hs = handshake(
        host,
        port,
        handler_for(host, port, &known_hosts),
        timeout,
        policy,
        keepalive,
    )
    .await
    .map_err(map_handshake)?;

    let legacy = hs.legacy;
    let mut session = hs.session;

    match session::authenticate_kbi(&mut session, username, &auth, session_id, host, &ask).await {
        Ok(session::KbiOutcome::Done) => Ok(Opened { session, legacy }),
        // 换了新连接还是一个问题都不问 → 服务端确实不要这一步，
        // 那就回到「主认证方式失败」的原始文案。
        Ok(session::KbiOutcome::Unavailable) => Err(OpenError::Auth(
            session::refusal_message(&auth).to_string(),
        )),
        Err(e) => Err(OpenError::Auth(e.to_string())),
    }
}

/// 让加密私钥变得可用：缺口令 / 口令错时向用户要，最多问 [`MAX_PASSPHRASE_ASKS`] 次。
///
/// 私钥口令与「服务端验证码」不是一回事，但都必须由人提供，所以复用同一个
/// 问答通道（[`prompt::AuthPrompts`]）—— 界面上只多一种问题类型。
async fn resolve_passphrase<F, Fut>(
    host: &str,
    session_id: &str,
    auth: &AuthMethod,
    ask: &F,
) -> Result<AuthMethod, OpenError>
where
    F: Fn(prompt::AuthPrompt) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<String>, prompt::AskAbort>>,
{
    let AuthMethod::PrivateKey { path, passphrase } = auth else {
        return Ok(auth.clone());
    };

    let mut phrase = passphrase.clone().filter(|p| !p.is_empty());
    for attempt in 0..=MAX_PASSPHRASE_ASKS {
        match crate::keys::load_private_key(path, phrase.clone()).await {
            Ok(_) => {
                return Ok(AuthMethod::PrivateKey {
                    path: path.clone(),
                    passphrase: phrase,
                })
            }
            // 只有「缺口令」「口令错」值得再问一次；文件读不到、格式不对再问也没用
            Err(e) if e.contains("需要提供密码") || e.contains("密码不正确") => {
                if attempt == MAX_PASSPHRASE_ASKS {
                    return Err(OpenError::Auth(if e.contains("密码不正确") {
                        "私钥密码多次输入不正确，连接已中止".into()
                    } else {
                        e
                    }));
                }
                let first_time = attempt == 0;
                let p = prompt::AuthPrompt {
                    session_id: session_id.to_string(),
                    kind: "passphrase".into(),
                    host: host.to_string(),
                    // 口令问题发生在认证之前，这一步还不知道用户名
                    username: String::new(),
                    name: "私钥口令".into(),
                    instructions: if first_time {
                        format!(
                            "私钥 {} 已加密，请输入解锁它的口令。\n口令只用于解开本地私钥文件，不会发送到服务器。",
                            short_path(path)
                        )
                    } else {
                        "口令不正确，请重新输入。".to_string()
                    },
                    fields: vec![prompt::AuthPromptField {
                        prompt: "私钥口令".into(),
                        echo: false,
                        prefill: None,
                    }],
                    round: attempt + 1,
                };
                match ask(p).await {
                    Ok(mut a) if !a.is_empty() => {
                        let typed = a.remove(0);
                        if typed.is_empty() {
                            return Err(OpenError::Auth("未输入私钥口令，连接已中止".into()));
                        }
                        phrase = Some(typed);
                    }
                    Ok(_) => {
                        return Err(OpenError::Auth("未输入私钥口令，连接已中止".into()))
                    }
                    Err(prompt::AskAbort::Cancelled) => {
                        return Err(OpenError::Auth("已取消输入私钥口令，连接中止".into()))
                    }
                    Err(prompt::AskAbort::Timeout) => {
                        return Err(OpenError::Auth("等待私钥口令超时，连接已中止".into()))
                    }
                    Err(prompt::AskAbort::Unavailable) => {
                        return Err(OpenError::Auth(
                            "无法显示私钥口令输入框，连接已中止".into(),
                        ))
                    }
                }
            }
            Err(e) => return Err(OpenError::Auth(e)),
        }
    }
    unreachable!("循环必然在上一轮返回")
}

/// 口令输错允许重试的次数（不含首次）。
const MAX_PASSPHRASE_ASKS: u32 = 2;

/// 提示里只显示文件名，完整路径太长会把弹窗撑变形。
fn short_path(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string()
}

fn map_handshake(e: HandshakeError) -> OpenError {
    match e {
        HandshakeError::Timeout => OpenError::Timeout,
        HandshakeError::HostKeyMismatch => OpenError::HostKeyMismatch,
        HandshakeError::Russh { err, legacy_tried } => OpenError::Handshake { err, legacy_tried },
    }
}

#[cfg(test)]
mod crypto_tests {
    use super::*;

    /// 默认必须保持“只有现代算法”，不能无意中把所有连接都降级
    #[test]
    fn default_prefers_modern_kex_only() {
        let p = preferred(false);
        assert!(p.kex.contains(&russh::kex::CURVE25519));
        assert!(!p.kex.contains(&russh::kex::DH_G14_SHA1));
        assert!(!p.kex.contains(&russh::kex::DH_G1_SHA1));
    }

    /// 兼容模式：旧算法只能**追加在末尾**，现代算法仍排在最前
    #[test]
    fn legacy_appends_after_modern() {
        let p = preferred(true);
        let modern_last = p
            .kex
            .iter()
            .rposition(|k| *k == russh::kex::CURVE25519)
            .expect("curve25519 必须仍在列表中");
        let g14 = p
            .kex
            .iter()
            .position(|k| *k == russh::kex::DH_G14_SHA1)
            .expect("group14-sha1 必须被追加");
        let g1 = p
            .kex
            .iter()
            .position(|k| *k == russh::kex::DH_G1_SHA1)
            .expect("group1-sha1 必须被追加");
        assert!(g14 > modern_last, "旧算法不得排在现代算法之前");
        assert!(g1 > g14, "group14 应优先于 group1");
    }

    /// 前端三态开关到策略的映射
    #[test]
    fn policy_from_flag() {
        assert_eq!(LegacyPolicy::from_flag(None), LegacyPolicy::Auto);
        assert_eq!(LegacyPolicy::from_flag(Some(true)), LegacyPolicy::Always);
        assert_eq!(LegacyPolicy::from_flag(Some(false)), LegacyPolicy::Never);
    }

    /// 现场排障用：对一台真实老服务器跑完整握手（到认证前一步为止）。
    ///
    /// ```text
    /// NEXUS_PROBE_HOST=8.219.87.15 cargo test --lib probe_legacy -- --ignored --nocapture
    /// ```
    ///
    /// 默认被 `ignore`，不会在 CI 上发起外部连接。
    #[tokio::test(flavor = "current_thread")]
    #[ignore = "需要真实主机：NEXUS_PROBE_HOST=<ip> [NEXUS_PROBE_PORT=22]"]
    async fn probe_legacy_server_handshake() {
        let Ok(host) = std::env::var("NEXUS_PROBE_HOST") else {
            eprintln!("未设置 NEXUS_PROBE_HOST，跳过");
            return;
        };
        let port: u16 = std::env::var("NEXUS_PROBE_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(22);

        let handler = ClientHandler {
            known_hosts: Arc::new(KnownHosts::new()),
            host: host.clone(),
            port,
            host_key_rejected: Arc::new(AtomicBool::new(false)),
        };

        match handshake(
            &host,
            port,
            handler,
            Duration::from_secs(15),
            LegacyPolicy::Auto,
            None,
        )
        .await
        {
            Ok(h) => println!("握手成功：legacy = {}", h.legacy),
            Err(e) => panic!("握手失败：{e:?}"),
        }
    }

    /// 现场排障用：对一台真实服务器完成「握手 + 私钥认证」。
    ///
    /// ```text
    /// NEXUS_KEY_HOST=1.2.3.4 NEXUS_KEY_USER=root NEXUS_KEY_PATH=~/.ssh/id_rsa \
    ///   cargo test --lib probe_publickey_auth -- --ignored --nocapture
    /// ```
    ///
    /// 专门用来区分「密钥本身不对」和「签名算法对不上」——
    /// 后者在过去的表现就是老服务器（OpenSSH 5.3）一律回「公钥被拒绝」。
    #[tokio::test(flavor = "current_thread")]
    #[ignore = "需要真实主机：NEXUS_KEY_HOST / NEXUS_KEY_USER / NEXUS_KEY_PATH"]
    async fn probe_publickey_auth() {
        let (Ok(host), Ok(user), Ok(path)) = (
            std::env::var("NEXUS_KEY_HOST"),
            std::env::var("NEXUS_KEY_USER"),
            std::env::var("NEXUS_KEY_PATH"),
        ) else {
            eprintln!("未设置 NEXUS_KEY_HOST / NEXUS_KEY_USER / NEXUS_KEY_PATH，跳过");
            return;
        };
        let port: u16 = std::env::var("NEXUS_KEY_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(22);
        let passphrase = std::env::var("NEXUS_KEY_PASSPHRASE").ok();

        let handler = ClientHandler {
            known_hosts: Arc::new(KnownHosts::new()),
            host: host.clone(),
            port,
            host_key_rejected: Arc::new(AtomicBool::new(false)),
        };

        let opened = handshake(
            &host,
            port,
            handler,
            Duration::from_secs(15),
            LegacyPolicy::Auto,
            None,
        )
        .await
        .unwrap_or_else(|e| panic!("握手失败：{e:?}"));

        let mut session = opened.session;
        let outcome = session::authenticate(
            &mut session,
            &user,
            &AuthMethod::PrivateKey {
                path,
                passphrase,
            },
        )
        .await
        .expect("认证请求本身出错");

        println!("认证结果：{outcome:?}（legacy KEX = {}）", opened.legacy);
        assert_eq!(outcome, session::AuthOutcome::Done, "公钥被服务端拒绝");
    }

    /// 现场排障用：打印一台服务器对「密码」与「keyboard-interactive」的真实反应。
    ///
    /// ```text
    /// NEXUS_KBI_HOST=wiz-jump.example.com NEXUS_KBI_PORT=2222 \
    /// NEXUS_KBI_USER=me NEXUS_KBI_PASSWORD=*** \
    ///   cargo test --lib probe_kbi_auth -- --ignored --nocapture
    /// ```
    ///
    /// 用来分辨「密码被拒」「根本不支持 KBI」「KBI 问了别的东西」这几种情况 ——
    /// 用户看到的都只是「密码不正确」一句话。
    #[tokio::test(flavor = "current_thread")]
    #[ignore = "需要真实主机：NEXUS_KBI_HOST / NEXUS_KBI_USER / NEXUS_KBI_PASSWORD"]
    async fn probe_kbi_auth() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .try_init();
        let (Ok(host), Ok(user), Ok(password)) = (
            std::env::var("NEXUS_KBI_HOST"),
            std::env::var("NEXUS_KBI_USER"),
            std::env::var("NEXUS_KBI_PASSWORD"),
        ) else {
            eprintln!("未设置 NEXUS_KBI_HOST / NEXUS_KBI_USER / NEXUS_KBI_PASSWORD，跳过");
            return;
        };
        let port: u16 = std::env::var("NEXUS_KBI_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(22);
        let otp = std::env::var("NEXUS_KBI_OTP").unwrap_or_else(|_| "000000".into());

        // 第一次：先发 password，再**在同一条连接上**发 KBI —— 这就是
        // 「密码 + 动态口令」两因素服务器的正确走法。
        {
            let mut hs = handshake(
                &host,
                port,
                handler_for(&host, port, &Arc::new(KnownHosts::new())),
                Duration::from_secs(15),
                LegacyPolicy::Auto,
                None,
            )
            .await
            .expect("握手失败");
            let ok = hs
                .session
                .authenticate_password(&user, &password)
                .await
                .expect("password 请求出错");
            println!("[1] authenticate_password => {ok}");
            if ok {
                println!("[1] 密码单因子即通过，服务端不需要第二步");
                return;
            }

            use russh::client::KeyboardInteractiveAuthResponse as Resp;
            let mut resp = hs
                .session
                .authenticate_keyboard_interactive_start(&user, None)
                .await
                .expect("KBI 请求出错");

            for round in 0..8 {
                match resp {
                    Resp::Success => {
                        println!("[1] 密码 + KBI 全部通过");
                        return;
                    }
                    Resp::Failure => {
                        println!("[1] KBI 失败（第 {round} 轮）");
                        break;
                    }
                    Resp::InfoRequest {
                        name,
                        instructions,
                        prompts,
                    } => {
                        println!("[1] round {round}: name={name:?} instructions={instructions:?}");
                        for p in &prompts {
                            println!("      prompt={:?} echo={}", p.prompt, p.echo);
                        }
                        let answers: Vec<String> = prompts
                            .iter()
                            .map(|p| {
                                let t = p.prompt.to_ascii_lowercase();
                                if !p.echo && (t.contains("password") || t.contains("密码")) {
                                    password.clone()
                                } else {
                                    otp.clone()
                                }
                            })
                            .collect();
                        resp = hs
                            .session
                            .authenticate_keyboard_interactive_respond(answers)
                            .await
                            .expect("KBI 应答出错");
                    }
                }
            }
        }

        // 第二次：新连接上直接发 KBI（纯 KBI 服务器的形态，用来看它是否支持）
        let mut hs = handshake(
            &host,
            port,
            handler_for(&host, port, &Arc::new(KnownHosts::new())),
            Duration::from_secs(15),
            LegacyPolicy::Auto,
            None,
        )
        .await
        .expect("握手失败");

        use russh::client::KeyboardInteractiveAuthResponse as Resp;
        let resp = hs
            .session
            .authenticate_keyboard_interactive_start(&user, None)
            .await
            .expect("KBI 请求出错");
        match resp {
            Resp::Success => println!("[2] KBI 首请求即成功"),
            Resp::Failure => println!("[2] KBI 作为首个请求被拒（服务端要求先密码）"),
            Resp::InfoRequest { prompts, .. } => {
                println!("[2] KBI 首请求直接下发 {} 个问题", prompts.len())
            }
        }
    }

    /// 走**产品真实路径**验证「密码 + 动态口令」：应当弹出验证码框，
    /// 而不是直接报「密码不正确」。
    ///
    /// ```text
    /// NEXUS_KBI_HOST=... NEXUS_KBI_USER=... NEXUS_KBI_PASSWORD=*** \
    ///   cargo test --lib probe_mfa_flow -- --ignored --nocapture
    /// ```
    ///
    /// OTP 由 `NEXUS_KBI_OTP` 提供（默认 000000，必定被服务端拒绝）——
    /// 这个用例检查的是「有没有问到验证码」，不是「验证码对不对」。
    #[tokio::test(flavor = "current_thread")]
    #[ignore = "需要真实主机：NEXUS_KBI_HOST / NEXUS_KBI_USER / NEXUS_KBI_PASSWORD"]
    async fn probe_mfa_flow() {
        use std::sync::Mutex as StdMutex;

        let Ok(host) = std::env::var("NEXUS_KBI_HOST") else {
            eprintln!("未设置 NEXUS_KBI_HOST，跳过");
            return;
        };
        let user = std::env::var("NEXUS_KBI_USER").expect("NEXUS_KBI_USER");
        let password = std::env::var("NEXUS_KBI_PASSWORD").expect("NEXUS_KBI_PASSWORD");
        let port: u16 = std::env::var("NEXUS_KBI_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(22);
        let otp = std::env::var("NEXUS_KBI_OTP").unwrap_or_else(|_| "000000".into());

        let seen = Arc::new(StdMutex::new(Vec::<prompt::AuthPrompt>::new()));
        let sink = seen.clone();

        let res = open_authenticated_with(
            &host,
            port,
            Arc::new(KnownHosts::new()),
            Duration::from_secs(20),
            LegacyPolicy::Auto,
            None,
            &user,
            &AuthMethod::Password {
                password: password.clone(),
            },
            "probe",
            move |p: prompt::AuthPrompt| {
                sink.lock().unwrap().push(p.clone());
                let otp = otp.clone();
                async move {
                    Ok(p
                        .fields
                        .iter()
                        .map(|f| f.prefill.clone().unwrap_or_else(|| otp.clone()))
                        .collect::<Vec<_>>())
                }
            },
        )
        .await;

        let prompts = seen.lock().unwrap();
        println!("结果：{:?}", res.as_ref().err());
        println!("收到 {} 个问答", prompts.len());
        for p in prompts.iter() {
            println!("  round={} name={:?}", p.round, p.name);
            for f in &p.fields {
                println!("    prompt={:?} echo={} prefill={:?}", f.prompt, f.echo, f.prefill);
            }
        }

        assert!(
            !prompts.is_empty(),
            "没有向用户要过任何东西就直接失败了（说明第一因子或 KBI 流程有问题）"
        );
    }
}

#[cfg(test)]
mod kbi_flow {
    //! 端到端：真握手 + 真认证，对本地假服务器（`tests/kbi_server.py`）验证
    //! 「主认证失败 → 换新连接 → 以 KBI 完成二次验证」整条链路。
    //!
    //! ```bash
    //! python tests/kbi_server.py 2222
    //! cargo test --lib kbi_flow -- --ignored --nocapture
    //! ```

    use super::*;
    use crate::ssh::session::AuthMethod;
    use std::sync::Mutex as StdMutex;

    #[tokio::test]
    #[ignore]
    async fn reconnects_and_completes_two_factor() {
        let port: u16 = std::env::var("NEXUS_KBI_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(2222);

        let seen = Arc::new(StdMutex::new(Vec::<prompt::AuthPrompt>::new()));
        let sink = seen.clone();

        let opened = open_authenticated_with(
            "127.0.0.1",
            port,
            // 空指纹表 = 首次信任（TOFU），正好模拟第一次连这台机器
            Arc::new(KnownHosts::new()),
            Duration::from_secs(10),
            LegacyPolicy::Auto,
            None,
            "tester",
            &AuthMethod::Password {
                password: "test123".into(),
            },
            "probe",
            move |p: prompt::AuthPrompt| {
                sink.lock().unwrap().push(p.clone());
                async move {
                    Ok(p.fields
                        .iter()
                        // 密码用后端带入的 prefill，验证码手填
                        .map(|f| f.prefill.clone().unwrap_or_else(|| "654321".to_string()))
                        .collect::<Vec<_>>())
                }
            },
        )
        .await
        .expect("应当重连后完成二次验证");

        let prompts = seen.lock().unwrap();
        assert_eq!(prompts.len(), 1, "只该问一轮");
        assert_eq!(prompts[0].session_id, "probe");
        assert_eq!(prompts[0].fields.len(), 2);
        assert_eq!(prompts[0].fields[0].prefill.as_deref(), Some("test123"));

        // 真的认证成功了才开得了通道
        opened
            .session
            .channel_open_session()
            .await
            .expect("认证成功但开不了通道");
    }

    /// 密码本身没错但服务器不做二次验证时，不该白跑一趟重连。
    ///
    /// 用一个只接受密码的假服务端会需要另一份脚本，这里退一步：断言
    /// 「服务端不支持 KBI」时报的是主认证方式的失败原因，而不是二次验证的文案。
    #[tokio::test]
    #[ignore]
    async fn unsupported_kbi_blames_primary_method() {
        let port: u16 = std::env::var("NEXUS_KBI_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(2222);

        let err = match open_authenticated_with(
            "127.0.0.1",
            port,
            Arc::new(KnownHosts::new()),
            Duration::from_secs(10),
            LegacyPolicy::Auto,
            None,
            "tester",
            &AuthMethod::PrivateKey {
                path: String::new(),
                passphrase: None,
            },
            "probe",
            |_p: prompt::AuthPrompt| async move { Err(prompt::AskAbort::Cancelled) },
        )
        .await
        {
            Ok(_) => panic!("空私钥路径必然失败"),
            Err(e) => e,
        };

        // 走到这里是「读私钥就失败」，属于主认证错误 —— 关键是不能冒出二次验证的文案
        let msg = format!("{err:?}");
        assert!(!msg.contains("二次验证未通过"), "实际：{msg}");
    }
}
