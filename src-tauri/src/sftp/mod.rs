use std::collections::HashMap;
use std::sync::Arc;

use russh::client::Handle;
use russh_sftp::client::SftpSession;
use serde::Serialize;
use tokio::io::{self, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::ssh::session::{ConnectRequest, SharedConns};
use crate::ssh::{ClientHandler, KnownHosts};

/// 单个远端目录项。尺寸/时间统一成可空数字，避免前端处理 undefined。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: Option<u64>,
    pub permissions: Option<u32>,
}

#[derive(Debug, thiserror::Error)]
pub enum SftpError {
    /// 与 SessionError 同构：内容已是完整中文提示，Display 不再叠加前缀
    #[error("{0}")]
    Connect(String),
    #[error("{0}")]
    Auth(String),
    #[error("服务器未启用 SFTP 子系统: {0}")]
    Subsystem(String),
    #[error("远端操作失败: {0}")]
    Remote(String),
    #[error("会话不存在: {0}")]
    NotFound(String),
}

impl Serialize for SftpError {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

/// SFTP 会话池。
///
/// 一个 SFTP 面板优先**复用同配置下已认证的终端连接**：在它上面开一条新通道
/// 请求 `sftp` 子系统即可。对要求 keyboard-interactive 的服务器（OTP / 堡垒机）
/// 这一点是决定性 —— 独立连接意味着每打开一次文件面板就要再输一次验证码。
///
/// 只有在「该配置当前没有活着的终端连接」时才退回独立连接（要重新认证）。
pub struct SftpManager {
    inner: Mutex<HashMap<String, SftpSession>>,
    known_hosts: Arc<KnownHosts>,
    /// 与终端通道共享同一份二次验证登记表（AppState 里拿的是同一个 Arc）
    auth_prompts: Arc<crate::ssh::prompt::AuthPrompts>,
    /// 已认证连接登记表（与 `SessionManager` 共享同一个 Arc）
    shared: SharedConns,
    /// 复用了主连接的 SFTP 会话 → 把句柄一起攥着保活。
    ///
    /// 不复用的情况下这里没有条目。留着它是因为：主标签关掉后 `SessionRuntime`
    /// 里的 Arc 被释放，若文件面板还开着，连接会连带被回收 —— 传输到一半就断。
    /// 句柄在这里活到 SFTP 会话自己 close 为止。
    held: parking_lot::Mutex<HashMap<String, Arc<Handle<ClientHandler>>>>,
}

/// 复用已认证连接时的两种失败，处理方式完全不同。
enum ReuseError {
    /// 那条连接其实已经断了（或刚好断在中间）—— 退回独立连接即可
    Connection(String),
    /// 连接好好活着，但开不出 sftp 子系统（服务器禁用了）——
    /// 独立再连一次也是同样结果，没必要多花一次认证
    Subsystem(SftpError),
}

impl SftpManager {
    pub fn new(
        known_hosts: Arc<KnownHosts>,
        auth_prompts: Arc<crate::ssh::prompt::AuthPrompts>,
        shared: SharedConns,
    ) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            known_hosts,
            auth_prompts,
            shared,
            held: parking_lot::Mutex::new(HashMap::new()),
        }
    }

    /// 在同一条已认证连接上开 sftp 子系统。
    async fn open_shared(
        &self,
        id: &str,
        handle: Arc<Handle<ClientHandler>>,
    ) -> Result<(), ReuseError> {
        // 通道开不出来只可能是连接层面的问题，退回独立连接是对的
        let channel = handle
            .channel_open_session()
            .await
            .map_err(|e| ReuseError::Connection(e.to_string()))?;

        if let Err(e) = channel.request_subsystem(true, "sftp").await {
            // 通道已经开了，用不上就还回去，别在服务端留个半开通道
            let _ = channel.close().await;
            return Err(ReuseError::Subsystem(SftpError::Subsystem(e.to_string())));
        }

        let sftp = SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| ReuseError::Subsystem(SftpError::Subsystem(e.to_string())))?;

        self.inner.lock().await.insert(id.to_string(), sftp);
        self.held.lock().insert(id.to_string(), handle);
        Ok(())
    }

    /// 建立连接并请求 `sftp` 子系统。
    ///
    /// 先尝试复用同配置下已认证的终端连接；只有那条连接不存在或已断开时，
    /// 才自己拨一条新连接走完整认证。
    pub async fn connect(
        &self,
        app: &tauri::AppHandle,
        id: String,
        req: ConnectRequest,
    ) -> Result<(), SftpError> {
        let port = req.port.unwrap_or(22);

        // ① 复用路径：省掉整个握手 + 认证过程。
        //    对 OTP 服务器尤其重要 —— 否则每次打开文件面板都要再输一次验证码。
        //    四个条件缺一不可：同一配置、同一主机端口、同一用户、连接还活着。
        //    少掉主机校验就会出这种事：配置从 A 改成 B 后旧标签仍连着 A，
        //    文件面板会悄悄列出 A 的目录。
        if let Some(config_id) = req.config_id.as_deref() {
            let alive = self
                .shared
                .lock()
                .values()
                .find(|c| {
                    c.config_id == config_id
                        && c.host == req.host
                        && c.port == port
                        && c.username == req.username
                        && !c.handle.is_closed()
                })
                .map(|c| c.handle.clone());

            if let Some(handle) = alive {
                match self.open_shared(&id, handle).await {
                    Ok(()) => {
                        tracing::info!(session = %id, config = config_id, "SFTP 复用已认证连接");
                        return Ok(());
                    }
                    // 连接正好断了：当成没复用，老老实实自己连
                    Err(ReuseError::Connection(e)) => {
                        tracing::info!(
                            session = %id,
                            config = config_id,
                            "复用连接不可用，回退独立连接：{e}"
                        );
                    }
                    // 服务器不给 sftp 子系统，独立连接也一样，别白跑一次认证
                    Err(ReuseError::Subsystem(e)) => return Err(e),
                }
            }
        }

        // 与终端通道共用同一套超时、算法协商（含老服务器自动降级）与二次验证策略。
        // keepalive 走 None：SFTP 是短交互，探活包反而可能干扰老服务器。
        let timeout = crate::errors::resolve_timeout(req.connect_timeout_secs);

        // 走到这里说明没得复用，必须自己认证一次
        let auth = req.auth.as_ref().ok_or_else(|| {
            SftpError::Auth(
                "没有可复用的已登录连接，且本地没有保存登录凭据。请先打开该主机的终端标签，或编辑会话保存密码。"
                    .into(),
            )
        })?;

        let opened = crate::ssh::open_authenticated(
            &req.host,
            port,
            self.known_hosts.clone(),
            timeout,
            crate::ssh::LegacyPolicy::from_flag(req.legacy_crypto),
            None,
            &req.username,
            auth,
            &id,
            crate::ssh::session::AuthContext {
                app,
                prompts: &self.auth_prompts,
            },
        )
        .await
        .map_err(|e| match e {
            crate::ssh::OpenError::Timeout => SftpError::Connect(crate::errors::compose(
                &crate::errors::timeout_message(&req.host, port, timeout),
                &format!("等待握手超过 {} 秒", timeout.as_secs()),
            )),
            // 指纹冲突：russh 会直接断开，靠旁路标记还原准确原因
            crate::ssh::OpenError::HostKeyMismatch => SftpError::Auth(
                "主机公钥指纹不匹配，疑似中间人攻击".into(),
            ),
            crate::ssh::OpenError::Handshake { err, legacy_tried } => SftpError::Connect(
                crate::errors::handshake_error(&req.host, port, &err, legacy_tried),
            ),
            crate::ssh::OpenError::Auth(msg) => SftpError::Auth(msg),
        })?;

        let session = opened.session;


        let channel = session
            .channel_open_session()
            .await
            .map_err(|e| SftpError::Connect(e.to_string()))?;

        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| SftpError::Subsystem(e.to_string()))?;

        // ChannelStream 实现 AsyncRead + AsyncWrite，正是 SftpSession::new 的要求
        let sftp = SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| SftpError::Subsystem(e.to_string()))?;

        self.inner.lock().await.insert(id, sftp);
        Ok(())
    }

    pub async fn close(&self, id: &str) {
        // 认证可能正卡在等验证码上，先解除挂起
        self.auth_prompts.cancel(id);
        if let Some(s) = self.inner.lock().await.remove(id) {
            let _ = s.close().await;
        }
        // 释放保活引用。复用的那条连接属于终端，是否关闭由它自己决定，
        // 这里只负责别让文件面板替它续命。
        self.held.lock().remove(id);
    }

    /// 列出远端目录。目录在前、名称升序，符合文件管理器习惯。
    pub async fn list(&self, id: &str, path: &str) -> Result<Vec<RemoteEntry>, SftpError> {
        // read_dir 返回的是同步迭代器，先收干再排序，即可避免在持锁期间 await
        let mut entries = {
            let map = self.inner.lock().await;
            let s = map
                .get(id)
                .ok_or_else(|| SftpError::NotFound(id.to_string()))?;
            let dir = s
                .read_dir(path.to_string())
                .await
                .map_err(|e| SftpError::Remote(e.to_string()))?;

            dir.map(|e| {
                let meta = e.metadata();
                RemoteEntry {
                    name: e.file_name(),
                    path: e.path(),
                    is_dir: meta.is_dir(),
                    size: meta.size.unwrap_or(0),
                    mtime: meta.mtime.map(|t| t as u64),
                    permissions: meta.permissions,
                }
            })
            .collect::<Vec<_>>()
        };

        entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(entries)
    }

    /// 下载远端文件到本地路径，返回写入字节数。
    pub async fn download(&self, id: &str, remote: &str, local: &str) -> Result<u64, SftpError> {
        // File 内部持有 Arc<RawSftpSession>，不借用 &self，因此拿到手就能释放锁
        let mut file = {
            let map = self.inner.lock().await;
            let s = map
                .get(id)
                .ok_or_else(|| SftpError::NotFound(id.to_string()))?;
            s.open(remote.to_string())
                .await
                .map_err(|e| SftpError::Remote(e.to_string()))?
        };

        let mut out = tokio::fs::File::create(local)
            .await
            .map_err(|e| SftpError::Remote(format!("无法创建本地文件 {}: {}", local, e)))?;

        let n = io::copy(&mut file, &mut out)
            .await
            .map_err(|e| SftpError::Remote(e.to_string()))?;

        out.flush()
            .await
            .map_err(|e| SftpError::Remote(e.to_string()))?;
        file.close()
            .await
            .map_err(|e| SftpError::Remote(e.to_string()))?;

        Ok(n)
    }

    /// 把远端文件整个读进内存（浏览窗口用），超过 `max` 字节直接拒绝。
    pub async fn read(&self, id: &str, path: &str, max: usize) -> Result<Vec<u8>, SftpError> {
        use tokio::io::AsyncReadExt;
        let mut file = {
            let map = self.inner.lock().await;
            let s = map
                .get(id)
                .ok_or_else(|| SftpError::NotFound(id.to_string()))?;
            let meta = s
                .metadata(path.to_string())
                .await
                .map_err(|e| SftpError::Remote(e.to_string()))?;
            if meta.is_dir() {
                return Err(SftpError::Remote(format!("「{path}」是目录")));
            }
            if meta.size.unwrap_or(0) as usize > max {
                return Err(SftpError::Remote(format!(
                    "文件超过 {} MB，不适合浏览",
                    max / 1024 / 1024
                )));
            }
            s.open(path.to_string())
                .await
                .map_err(|e| SftpError::Remote(e.to_string()))?
        };
        let mut buf = Vec::new();
        (&mut file)
            .take(max as u64 + 1)
            .read_to_end(&mut buf)
            .await
            .map_err(|e| SftpError::Remote(e.to_string()))?;
        let _ = file.close().await;
        if buf.len() > max {
            return Err(SftpError::Remote("文件过大，不适合浏览".into()));
        }
        Ok(buf)
    }

    /// 上传本地文件到远端路径，返回写入字节数。
    pub async fn upload(&self, id: &str, local: &str, remote: &str) -> Result<u64, SftpError> {
        let mut file = {
            let map = self.inner.lock().await;
            let s = map
                .get(id)
                .ok_or_else(|| SftpError::NotFound(id.to_string()))?;
            s.create(remote.to_string())
                .await
                .map_err(|e| SftpError::Remote(e.to_string()))?
        };

        let mut src = tokio::fs::File::open(local)
            .await
            .map_err(|e| SftpError::Remote(format!("无法读取本地文件 {}: {}", local, e)))?;

        let n = io::copy(&mut src, &mut file)
            .await
            .map_err(|e| SftpError::Remote(e.to_string()))?;

        file.close()
            .await
            .map_err(|e| SftpError::Remote(e.to_string()))?;

        Ok(n)
    }

    pub async fn mkdir(&self, id: &str, path: &str) -> Result<(), SftpError> {
        let map = self.inner.lock().await;
        let s = map
            .get(id)
            .ok_or_else(|| SftpError::NotFound(id.to_string()))?;
        s.create_dir(path.to_string())
            .await
            .map_err(|e| SftpError::Remote(e.to_string()))
    }

    /// 删除远端条目。目录走 remove_dir，其余走 remove_file。
    pub async fn remove(&self, id: &str, path: &str, is_dir: bool) -> Result<(), SftpError> {
        let map = self.inner.lock().await;
        let s = map
            .get(id)
            .ok_or_else(|| SftpError::NotFound(id.to_string()))?;
        if is_dir {
            s.remove_dir(path.to_string()).await
        } else {
            s.remove_file(path.to_string()).await
        }
        .map_err(|e| SftpError::Remote(e.to_string()))
    }

    pub async fn rename(&self, id: &str, from: &str, to: &str) -> Result<(), SftpError> {
        let map = self.inner.lock().await;
        let s = map
            .get(id)
            .ok_or_else(|| SftpError::NotFound(id.to_string()))?;
        s.rename(from.to_string(), to.to_string())
            .await
            .map_err(|e| SftpError::Remote(e.to_string()))
    }

    /// 解析远端家目录，用作文件管理器的起始路径。
    pub async fn home(&self, id: &str) -> Result<String, SftpError> {
        let map = self.inner.lock().await;
        let s = map
            .get(id)
            .ok_or_else(|| SftpError::NotFound(id.to_string()))?;
        s.canonicalize(".".to_string())
            .await
            .map_err(|e| SftpError::Remote(e.to_string()))
    }
}

impl From<crate::ssh::session::SessionError> for SftpError {
    fn from(e: crate::ssh::session::SessionError) -> Self {
        SftpError::Auth(e.to_string())
    }
}
