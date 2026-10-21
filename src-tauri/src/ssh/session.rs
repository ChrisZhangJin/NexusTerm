use base64::Engine as _;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

const BASE64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

use russh::client::Handle;
use russh::{ChannelMsg, Disconnect};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc;

use super::{ClientHandler, KnownHosts};
use crate::errors;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// 内容已是完整的中文提示（含原始错误小字），故 Display 不再加前缀
    #[error("{0}")]
    Connect(String),
    #[error("{0}")]
    Auth(String),
    #[error("通道错误: {0}")]
    Channel(String),
    #[error("会话不存在: {0}")]
    NotFound(String),
    #[error("主机公钥指纹不匹配，疑似中间人攻击")]
    HostKeyMismatch,
}

// thiserror 派生 Display，这里再补一个统一的序列化形态给前端
impl Serialize for SessionError {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

/// 前端 → SSH 的输入指令
#[derive(Debug)]
pub enum SessionInput {
    Data(Vec<u8>),
    Resize {
        cols: u16,
        rows: u16,
        pix_w: u16,
        pix_h: u16,
    },
    Close,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectRequest {
    pub host: String,
    pub port: Option<u16>,
    pub username: String,
    pub auth: AuthMethod,
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
    /// TCP 连接超时（秒）。缺省为 [`errors::DEFAULT_CONNECT_TIMEOUT_SECS`]。
    #[serde(default)]
    pub connect_timeout_secs: Option<u32>,
    /// 旧服务器兼容开关。`None`（默认）= 自动，先按现代算法握手，
    /// KEX 协商失败时自动用 SHA-1 系列的旧算法重试一次；`Some(true)` =
    /// 一开始就带旧算法；`Some(false)` = 只用现代算法、绝不降级。
    #[serde(default)]
    pub legacy_crypto: Option<bool>,
}

/// 连接成功后的协商结果，回给前端以便提示「本次用了旧算法」。
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectOutcome {
    /// 是否协商到了 SHA-1 系列的旧密钥交换算法
    pub legacy_crypto: bool,
}

fn default_cols() -> u16 {
    120
}
fn default_rows() -> u16 {
    32
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum AuthMethod {
    Password { password: String },
    PrivateKey { path: String, passphrase: Option<String> },
    Agent,
}

/// 一个 SSH 会话的运行时句柄
struct SessionRuntime {
    /// 连接句柄：用于在同一连接上开旁路 exec 通道（探测 pwd / tmux / docker）
    handle: Arc<Handle<ClientHandler>>,
    input_tx: mpsc::UnboundedSender<SessionInput>,
}

/// 探测脚本：在同一 SSH 连接上开 exec 通道执行（stdin 喂给 `sh -s`，不依赖用户登录 shell 类型）。
///
/// 原理：同一 TCP 连接上的所有通道共享同一个 `SSH_CONNECTION`（含客户端端口，全局唯一）。
/// 据此在 /proc 里找出「父进程是 sshd、且 stdin 是 pts」的进程 —— 就是本标签的交互 shell；
/// 再从其 stat 的 tpgid 拿到前台进程组，读出 cwd / 命令行；tmux 则按 tty 查询所附着的 session。
/// 只读、不修改远端任何东西；非 Linux（无 /proc）时输出为空，前端按「未知」处理。
const PROBE_SCRIPT: &str = r#"
sc="$SSH_CONNECTION"
[ -n "$sc" ] || exit 0
[ -d /proc/self ] || exit 0
shell=""; tty=""
for p in /proc/[0-9]*; do
  [ -O "$p" ] || continue
  pid=${p#/proc/}
  [ "$pid" = "$$" ] && continue
  t=$(readlink "$p/fd/0" 2>/dev/null) || continue
  case "$t" in /dev/pts/*) ;; *) continue ;; esac
  tr '\0' '\n' < "$p/environ" 2>/dev/null | grep -qxF "SSH_CONNECTION=$sc" || continue
  pp=$(sed 's/.*) //' "$p/stat" 2>/dev/null | awk '{print $2}')
  case "$(cat /proc/$pp/comm 2>/dev/null)" in sshd*) shell=$pid; tty=$t; break ;; esac
done
[ -n "$shell" ] || exit 0
echo "TTY=$tty"
echo "SHELL_CWD=$(readlink /proc/$shell/cwd 2>/dev/null)"
fg=$(sed 's/.*) //' /proc/$shell/stat 2>/dev/null | awk '{print $6}')
if [ -n "$fg" ] && [ "$fg" -gt 0 ] 2>/dev/null && [ -r "/proc/$fg/cmdline" ]; then
  echo "FG_COMM=$(cat /proc/$fg/comm 2>/dev/null)"
  echo "FG_CWD=$(readlink /proc/$fg/cwd 2>/dev/null)"
  printf 'FG_ARGS='; tr '\0' '\037' < /proc/$fg/cmdline 2>/dev/null; echo
fi
if command -v tmux >/dev/null 2>&1; then
  # list-clients 按 tty 匹配（display-message -p -c 在 tmux 3.2 不可用）
  tmux list-clients -F '#{client_tty}	#{session_name}	#{pane_current_path}' 2>/dev/null |
    awk -F '\t' -v t="$tty" '$1==t { print "TMUX_SESSION=" $2; print "TMUX_PATH=" $3; exit }'
fi
"#;

/// 资源快照脚本（Linux /proc）。CPU 占用率需两次采样 /proc/stat 做差（间隔 0.5s）；
/// 内存取 MemAvailable（旧内核没有时用 MemFree+Buffers+Cached 近似）；磁盘取根分区 df -kP。
/// 只读，输出 KEY=VALUE；非 Linux 时各项缺省，前端显示「—」。
const STATS_SCRIPT: &str = r#"
LC_ALL=C; export LC_ALL
cpu() { awk '/^cpu /{ idle=$5+$6; t=0; for(i=2;i<=NF;i++) t+=$i; print t, idle; exit }' /proc/stat 2>/dev/null; }
a=$(cpu); sleep 0.5; b=$(cpu)
if [ -n "$a" ] && [ -n "$b" ]; then
  echo "$a $b" | awk '{ dt=$3-$1; di=$4-$2; if (dt>0) printf "CPU_PCT=%.1f\n", (dt-di)*100/dt }'
fi
n=$(grep -c '^processor' /proc/cpuinfo 2>/dev/null)
[ -n "$n" ] && [ "$n" -gt 0 ] 2>/dev/null || n=$(nproc 2>/dev/null)
echo "CPUS=$n"
[ -r /proc/loadavg ] && awk '{ print "LOAD=" $1 " " $2 " " $3 }' /proc/loadavg
awk '/^MemTotal:/{t=$2} /^MemAvailable:/{a=$2} /^MemFree:/{f=$2} /^Buffers:/{b=$2} /^Cached:/{c=$2}
  END { if (t) { if (!a) a=f+b+c; print "MEM_TOTAL=" t*1024; print "MEM_AVAIL=" a*1024 } }' /proc/meminfo 2>/dev/null
df -kP / 2>/dev/null | awk 'NR==2 { print "DISK_TOTAL=" $2*1024; print "DISK_USED=" $3*1024; print "DISK_MOUNT=" $6 }'
"#;

/// POSIX 单引号转义
fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// `cat` 命令：`~/x` 展开为 "$HOME"/'x'，其余整体单引号包裹，防注入。
/// 只用 sh / bash / zsh / fish 通用的语法。
fn cat_command(path: &str) -> String {
    if path == "~" {
        return "cat -- \"$HOME\"".into();
    }
    match path.strip_prefix("~/") {
        Some(rest) => format!("cat -- \"$HOME\"/{}", shq(rest)),
        None => format!("cat -- {}", shq(path)),
    }
}

#[cfg(test)]
mod cat_tests {
    use super::cat_command;
    #[test]
    fn quoting() {
        assert_eq!(cat_command("/a b/c.md"), "cat -- '/a b/c.md'");
        assert_eq!(cat_command("/x/it's.md"), "cat -- '/x/it'\\''s.md'");
        assert_eq!(cat_command("~/d/r.md"), "cat -- \"$HOME\"/'d/r.md'");
        assert_eq!(cat_command("/x/$(rm -rf).md"), "cat -- '/x/$(rm -rf).md'");
    }
}

/// 探测结果：KEY=VALUE 原样回传，由前端解释（规则改动不必重编 Rust）
pub type ProbeInfo = HashMap<String, String>;

/// 全局会话管理器
pub struct SessionManager {
    inner: Arc<Mutex<HashMap<String, SessionRuntime>>>,
    known_hosts: Arc<KnownHosts>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            known_hosts: Arc::new(KnownHosts::new()),
        }
    }

    /// 共享的主机指纹表。SFTP 连接复用同一份，保证 TOFU 策略一致。
    pub fn known_hosts(&self) -> Arc<KnownHosts> {
        self.known_hosts.clone()
    }

    pub fn close(&self, id: &str) {
        if let Some(rt) = self.inner.lock().remove(id) {
            let _ = rt.input_tx.send(SessionInput::Close);
        }
    }

    /// 建立 SSH 连接并启动读写循环
    pub async fn connect(
        &self,
        app: AppHandle,
        id: String,
        req: ConnectRequest,
    ) -> Result<ConnectOutcome, SessionError> {
        let port = req.port.unwrap_or(22);

        // 指纹冲突标记由 handshake 内部判别并映射成 HandshakeError::HostKeyMismatch，
        // 这里不再需要自己持有。
        let handler = ClientHandler {
            known_hosts: self.known_hosts.clone(),
            host: req.host.clone(),
            port,
            host_key_rejected: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };

        // 内核默认的 TCP 超时约 75 秒，用户会以为软件卡死。显式给上限，
        // 超时后立刻返回可读的中文提示，而不是干等。
        let timeout = errors::resolve_timeout(req.connect_timeout_secs);

        // 握手统一走 ssh::handshake：算法协商（含老服务器自动降级）、
        // keepalive 配置、超时预算都在那里处理，SFTP 通道复用同一套逻辑。
        let hs = match super::handshake(
            &req.host,
            port,
            handler,
            timeout,
            super::LegacyPolicy::from_flag(req.legacy_crypto),
            Some(std::time::Duration::from_secs(10)),
        )
        .await
        {
            Ok(hs) => hs,
            Err(super::HandshakeError::Timeout) => {
                return Err(SessionError::Connect(errors::compose(
                    &errors::timeout_message(&req.host, port, timeout),
                    &format!("等待握手超过 {} 秒", timeout.as_secs()),
                )));
            }
            Err(super::HandshakeError::HostKeyMismatch) => {
                return Err(SessionError::HostKeyMismatch);
            }
            Err(super::HandshakeError::Russh { err, legacy_tried }) => {
                return Err(SessionError::Connect(errors::handshake_error(
                    &req.host,
                    port,
                    &err,
                    legacy_tried,
                )));
            }
        };

        let legacy_crypto = hs.legacy;
        let mut session = hs.session;

        authenticate(&mut session, &req.username, &req.auth).await?;

        let channel = session
            .channel_open_session()
            .await
            .map_err(|e| SessionError::Channel(e.to_string()))?;

        channel
            .request_pty(
                false,
                "xterm-256color",
                req.cols as u32,
                req.rows as u32,
                0,
                0,
                &[],
            )
            .await
            .map_err(|e| SessionError::Channel(e.to_string()))?;

        channel
            .request_shell(false)
            .await
            .map_err(|e| SessionError::Channel(e.to_string()))?;

        let (input_tx, input_rx) = mpsc::unbounded_channel();

        self.inner.lock().insert(
            id.clone(),
            SessionRuntime {
                handle: Arc::new(session),
                input_tx,
            },
        );

        tokio::spawn(read_loop(app, id, channel, input_rx));

        Ok(ConnectOutcome { legacy_crypto })
    }

    pub fn write(&self, id: &str, data: Vec<u8>) -> Result<(), SessionError> {
        let map = self.inner.lock();
        let rt = map
            .get(id)
            .ok_or_else(|| SessionError::NotFound(id.into()))?;
        rt.input_tx
            .send(SessionInput::Data(data))
            .map_err(|_| SessionError::Channel("输入通道已关闭".into()))
    }

    /// 探测该标签交互 shell 的工作目录 / 前台进程 / tmux 会话。
    /// 走旁路 exec 通道，不会往用户终端里写任何东西。
    pub async fn probe(&self, id: &str) -> Result<ProbeInfo, SessionError> {
        self.run_kv_script(id, PROBE_SCRIPT, "探测超时").await
    }

    /// 远端资源快照（CPU / 内存 / 磁盘），供状态栏每 5 秒刷新。同样走旁路 exec 通道。
    pub async fn stats(&self, id: &str) -> Result<ProbeInfo, SessionError> {
        self.run_kv_script(id, STATS_SCRIPT, "资源采集超时").await
    }

    /// 在同一连接上开 exec 通道跑 `sh -s` 脚本，解析 `k=v` 行输出。
    async fn run_kv_script(
        &self,
        id: &str,
        script: &'static str,
        timeout_msg: &'static str,
    ) -> Result<ProbeInfo, SessionError> {
        let handle = {
            let map = self.inner.lock();
            map.get(id)
                .ok_or_else(|| SessionError::NotFound(id.into()))?
                .handle
                .clone()
        };
        let run = async move {
            let mut ch = handle
                .channel_open_session()
                .await
                .map_err(|e| SessionError::Channel(e.to_string()))?;
            ch.exec(true, "sh -s")
                .await
                .map_err(|e| SessionError::Channel(e.to_string()))?;
            ch.data(script.as_bytes())
                .await
                .map_err(|e| SessionError::Channel(e.to_string()))?;
            ch.eof().await.map_err(|e| SessionError::Channel(e.to_string()))?;
            let mut out = Vec::new();
            while let Some(msg) = ch.wait().await {
                match msg {
                    ChannelMsg::Data { data } => out.extend_from_slice(&data),
                    ChannelMsg::Close => break,
                    _ => {}
                }
            }
            Ok::<_, SessionError>(out)
        };
        let out = tokio::time::timeout(std::time::Duration::from_secs(8), run)
            .await
            .map_err(|_| SessionError::Channel(timeout_msg.into()))??;
        let text = String::from_utf8_lossy(&out);
        let mut info = ProbeInfo::new();
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                if !v.is_empty() {
                    info.insert(k.trim().to_string(), v.to_string());
                }
            }
        }
        Ok(info)
    }

    /// 在同一连接上开 exec 通道 `cat` 一个文件（阅读窗口用，不依赖 SFTP 子系统）。
    /// `path` 为绝对路径或 `~/` 开头；超过 `max` 字节即中止。
    pub async fn read_file(
        &self,
        id: &str,
        path: &str,
        max: usize,
    ) -> Result<Vec<u8>, SessionError> {
        let handle = {
            let map = self.inner.lock();
            map.get(id)
                .ok_or_else(|| SessionError::NotFound(id.into()))?
                .handle
                .clone()
        };
        let cmd = cat_command(path);
        let run = async move {
            let mut ch = handle
                .channel_open_session()
                .await
                .map_err(|e| SessionError::Channel(e.to_string()))?;
            ch.exec(true, cmd.as_bytes())
                .await
                .map_err(|e| SessionError::Channel(e.to_string()))?;
            let mut out = Vec::new();
            let mut err = Vec::new();
            let mut code: Option<u32> = None;
            while let Some(msg) = ch.wait().await {
                match msg {
                    ChannelMsg::Data { data } => {
                        out.extend_from_slice(&data);
                        if out.len() > max {
                            let _ = ch.close().await;
                            return Err(SessionError::Channel(format!(
                                "文件超过 {} MB，不适合阅读",
                                max / 1024 / 1024
                            )));
                        }
                    }
                    ChannelMsg::ExtendedData { data, .. } => {
                        if err.len() < 4096 {
                            err.extend_from_slice(&data);
                        }
                    }
                    ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
                    ChannelMsg::Close => break,
                    _ => {}
                }
            }
            if code.unwrap_or(0) != 0 {
                let msg = String::from_utf8_lossy(&err).trim().to_string();
                return Err(SessionError::Channel(if msg.is_empty() {
                    format!("读取失败（退出码 {}）", code.unwrap_or(0))
                } else {
                    msg
                }));
            }
            Ok::<_, SessionError>(out)
        };
        tokio::time::timeout(std::time::Duration::from_secs(30), run)
            .await
            .map_err(|_| SessionError::Channel("读取文件超时".into()))?
    }

    pub fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<(), SessionError> {
        let map = self.inner.lock();
        let rt = map
            .get(id)
            .ok_or_else(|| SessionError::NotFound(id.into()))?;
        rt.input_tx
            .send(SessionInput::Resize {
                cols,
                rows,
                pix_w: 0,
                pix_h: 0,
            })
            .map_err(|_| SessionError::Channel("会话已关闭".into()))
    }
}

/// 对已建立的连接执行认证。
///
/// 抽成独立函数是为了让 SSH 终端与 SFTP 子系统复用同一套逻辑 ——
/// 两者若各写一份，认证规则一变动就必然出现漂移。
pub async fn authenticate<H>(
    handle: &mut Handle<H>,
    username: &str,
    auth: &AuthMethod,
) -> Result<(), SessionError>
where
    H: russh::client::Handler,
{
    match auth {
        AuthMethod::Password { password } => {
            let ok = handle
                .authenticate_password(username, password)
                .await
                .map_err(|e| SessionError::Auth(errors::auth_error(&e.to_string())))?;
            if !ok {
                return Err(SessionError::Auth(errors::auth_error("密码认证被拒绝")));
            }
        }
        AuthMethod::PrivateKey { path, passphrase } => {
            let key = crate::keys::load_private_key(path, passphrase.clone())
                .await
                .map_err(|e| SessionError::Auth(errors::auth_error(&e)))?;

            // RSA 需显式指定签名算法以启用 rsa-sha2-256；
            // 非 RSA（ed25519 / ecdsa）算法本身即绑定，传 None，否则会报 InvalidParameters
            let hash_alg = if key.algorithm().is_rsa() {
                Some(russh::keys::HashAlg::Sha256)
            } else {
                None
            };

            let key_with_alg =
                russh::keys::key::PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg)
                    .map_err(|e| SessionError::Auth(errors::auth_error(&e.to_string())))?;

            let ok = handle
                .authenticate_publickey(username, key_with_alg)
                .await
                .map_err(|e| SessionError::Auth(errors::auth_error(&e.to_string())))?;
            if !ok {
                return Err(SessionError::Auth(errors::auth_error("公钥认证被拒绝")));
            }
        }
        AuthMethod::Agent => {
            let ok = handle
                .authenticate_none(username)
                .await
                .map_err(|e| SessionError::Auth(errors::auth_error(&e.to_string())))?;
            if !ok {
                return Err(SessionError::Auth(errors::auth_error(
                    "无凭据认证被拒绝，请改用密码或私钥",
                )));
            }
        }
    }
    Ok(())
}

/// 会话结束事件负载：前端据此决定提示文案以及是否自动重连。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClosedInfo {
    pub code: i32,
    /// exit = 远端 shell 正常退出；eof = 远端主动关闭通道；
    /// lost = 连接丢失（网络断开 / keepalive 超时 / 写失败）；user = 本地主动关闭
    pub reason: &'static str,
}

/// 读写主循环。select! 同时处理「远端数据」与「本地输入」两条流。
async fn read_loop(
    app: AppHandle,
    id: String,
    mut channel: russh::Channel<russh::client::Msg>,
    mut input_rx: mpsc::UnboundedReceiver<SessionInput>,
) {
    let mut info = ClosedInfo { code: 0, reason: "lost" };
    loop {
        tokio::select! {
            msg = channel.wait() => {
                match msg {
                    Some(ChannelMsg::Data { data, .. }) => {
                        // 必须走 base64：SSH 数据流按网络包切分，可能切断 UTF-8 多字节序列。
                        // 若在此处做 lossy UTF-8 转换，中文/emoji 会乱码。
                        let payload = BASE64.encode(&data);
                        if app
                            .emit(&format!("nx://term/data/{}", id), payload)
                            .is_err()
                        {
                            info.reason = "user"; // 前端已不在，无需提示
                            break;
                        }
                        // 旁路：主链路已经把数据投到屏幕上了，这里只是顺手复制一份。
                        // 拿不到状态（AppState 尚未托管）就安静跳过 —— 绝不能因为
                        // agent 缓冲出错而让终端断连。
                        if let Some(st) = app.try_state::<crate::cmd::AppState>() {
                            st.agent.feed(&id, &data, crate::agent::StreamKind::Out);
                        }
                    }
                    Some(ChannelMsg::ExtendedData { data, ext, .. }) => {
                        if ext == 1 {
                            tracing::warn!(session = id, "stderr: {}", String::from_utf8_lossy(&data));
                        }
                    }
                    Some(ChannelMsg::ExitStatus { exit_status }) => {
                        // 先别 break：ExitStatus 之后通常还跟着 Eof/Close，统一在循环外发一次
                        tracing::info!(session = id, exit_status, "远端 shell 退出");
                        info = ClosedInfo { code: exit_status as i32, reason: "exit" };
                    }
                    // OpenSSH 的顺序通常是 Eof → ExitStatus → Close，所以 Eof 不能立刻退出，
                    // 否则会漏掉退出码；以 Close（或通道流结束）为准
                    Some(ChannelMsg::Eof) => {
                        if info.reason != "exit" {
                            info.reason = "eof";
                        }
                    }
                    Some(ChannelMsg::Close) => {
                        if info.reason != "exit" {
                            info.reason = "eof";
                        }
                        break;
                    }
                    Some(ChannelMsg::Success) | Some(ChannelMsg::Failure) => {}
                    Some(other) => {
                        tracing::trace!(session = id, "未处理的通道消息: {:?}", other);
                    }
                    // None：底层连接已断（keepalive 超时 / TCP 重置）。若之前收到过
                    // ExitStatus 则仍算正常退出
                    None => break,
                }
            }
            Some(input) = input_rx.recv() => {
                match input {
                    SessionInput::Data(bytes) => {
                        if channel.data(&bytes[..]).await.is_err() {
                            info.reason = "lost";
                            break;
                        }
                    }
                    SessionInput::Resize { cols, rows, pix_w, pix_h } => {
                        // 同步远端 COLUMNS/LINES，否则 vim / top 显示会错乱
                        let _ = channel
                            .window_change(cols as u32, rows as u32, pix_w as u32, pix_h as u32)
                            .await;
                    }
                    SessionInput::Close => {
                        let _ = channel.close().await;
                        info.reason = "user";
                        break;
                    }
                }
            }
            else => break,
        }
    }
    tracing::info!(session = id, reason = info.reason, code = info.code, "会话结束");
    let _ = app.emit(&format!("nx://term/closed/{}", id), info);
}

#[allow(dead_code)]
async fn graceful_close(handle: &Handle<ClientHandler>) {
    let _ = handle
        .disconnect(Disconnect::ByApplication, "session closed", "en-US")
        .await;
}
