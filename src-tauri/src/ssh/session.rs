use base64::Engine as _;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

const BASE64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

use russh::client::Handle;
use russh::{ChannelMsg, Disconnect};
use serde::Serialize;
use tauri::{AppHandle, Emitter};
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
    // 保留 handle 以便优雅断开；目前由读写循环负责生命周期
    #[allow(dead_code)]
    handle: Handle<ClientHandler>,
    input_tx: mpsc::UnboundedSender<SessionInput>,
}

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
    ) -> Result<(), SessionError> {
        let port = req.port.unwrap_or(22);
        let addr = format!("{}:{}", req.host, port);

        let host_key_rejected = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let handler = ClientHandler {
            known_hosts: self.known_hosts.clone(),
            host: req.host.clone(),
            port,
            host_key_rejected: host_key_rejected.clone(),
        };

        let config = Arc::new(russh::client::Config::default());

        // 内核默认的 TCP 超时约 75 秒，用户会以为软件卡死。显式给上限，
        // 超时后立刻返回可读的中文提示，而不是干等。
        let timeout = errors::resolve_timeout(req.connect_timeout_secs);
        let connected = tokio::time::timeout(
            timeout,
            russh::client::connect(config, addr, handler),
        )
        .await;

        let mut session = match connected {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                // 指纹冲突会导致 russh 提前断开，此时把原因精确地识别出来
                if host_key_rejected.load(std::sync::atomic::Ordering::SeqCst) {
                    return Err(SessionError::HostKeyMismatch);
                }
                return Err(SessionError::Connect(errors::connect_error(
                    &req.host, port, &e,
                )));
            }
            Err(_) => {
                return Err(SessionError::Connect(errors::compose(
                    &errors::timeout_message(&req.host, port, timeout),
                    &format!("等待握手超过 {} 秒", timeout.as_secs()),
                )));
            }
        };

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
                handle: session,
                input_tx,
            },
        );

        tokio::spawn(read_loop(app, id, channel, input_rx));

        Ok(())
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

/// 读写主循环。select! 同时处理「远端数据」与「本地输入」两条流。
async fn read_loop(
    app: AppHandle,
    id: String,
    mut channel: russh::Channel<russh::client::Msg>,
    mut input_rx: mpsc::UnboundedReceiver<SessionInput>,
) {
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
                            break;
                        }
                    }
                    Some(ChannelMsg::ExtendedData { data, ext, .. }) => {
                        if ext == 1 {
                            tracing::warn!(session = id, "stderr: {}", String::from_utf8_lossy(&data));
                        }
                    }
                    Some(ChannelMsg::ExitStatus { exit_status }) => {
                        tracing::info!(session = id, exit_status, "远端 shell 退出");
                        let _ = app.emit(&format!("nx://term/closed/{}", id), exit_status);
                        break;
                    }
                    Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) => {
                        let _ = app.emit(&format!("nx://term/closed/{}", id), 0i32);
                        break;
                    }
                    Some(ChannelMsg::Success) | Some(ChannelMsg::Failure) => {}
                    Some(other) => {
                        tracing::trace!(session = id, "未处理的通道消息: {:?}", other);
                    }
                    None => break,
                }
            }
            Some(input) = input_rx.recv() => {
                match input {
                    SessionInput::Data(bytes) => {
                        if channel.data(&bytes[..]).await.is_err() { break; }
                    }
                    SessionInput::Resize { cols, rows, pix_w, pix_h } => {
                        // 同步远端 COLUMNS/LINES，否则 vim / top 显示会错乱
                        let _ = channel
                            .window_change(cols as u32, rows as u32, pix_w as u32, pix_h as u32)
                            .await;
                    }
                    SessionInput::Close => {
                        let _ = channel.close().await;
                        break;
                    }
                }
            }
            else => break,
        }
    }
    let _ = app.emit(&format!("nx://term/closed/{}", id), 0i32);
}

#[allow(dead_code)]
async fn graceful_close(handle: &mut Handle<ClientHandler>) {
    let _ = handle
        .disconnect(Disconnect::ByApplication, "session closed", "en-US")
        .await;
}
