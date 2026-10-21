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
}
