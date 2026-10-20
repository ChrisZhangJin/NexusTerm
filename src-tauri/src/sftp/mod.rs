use std::collections::HashMap;
use std::sync::Arc;

use russh_sftp::client::SftpSession;
use serde::Serialize;
use tokio::io::{self, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::ssh::session::{authenticate, ConnectRequest};
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
/// 每个 SFTP 面板开一条独立的 SSH 连接 —— 与 Xshell/Xftp 进程分离同理：
/// 文件传输是长耗时操作，不应和交互式 shell 争抢同一连接的窗口。
pub struct SftpManager {
    inner: Mutex<HashMap<String, SftpSession>>,
    known_hosts: Arc<KnownHosts>,
}

impl SftpManager {
    pub fn new(known_hosts: Arc<KnownHosts>) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            known_hosts,
        }
    }

    /// 建立连接并请求 `sftp` 子系统。
    pub async fn connect(&self, id: String, req: ConnectRequest) -> Result<(), SftpError> {
        let port = req.port.unwrap_or(22);
        let addr = format!("{}:{}", req.host, port);

        let host_key_rejected = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let handler = ClientHandler {
            known_hosts: self.known_hosts.clone(),
            host: req.host.clone(),
            port,
            host_key_rejected: host_key_rejected.clone(),
        };

        // 与终端通道共用同一套超时策略：连不上就尽早失败，别让用户干等
        let timeout = crate::errors::resolve_timeout(req.connect_timeout_secs);
        let connected = tokio::time::timeout(
            timeout,
            russh::client::connect(
                Arc::new(russh::client::Config::default()),
                addr,
                handler,
            ),
        )
        .await;

        let mut session = match connected {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                if host_key_rejected.load(std::sync::atomic::Ordering::SeqCst) {
                    // 指纹冲突：russh 会直接断开，靠旁路标记还原准确原因
                    return Err(SftpError::Auth(
                        "主机公钥指纹不匹配，疑似中间人攻击".into(),
                    ));
                }
                return Err(SftpError::Connect(crate::errors::connect_error(
                    &req.host, port, &e,
                )));
            }
            Err(_) => {
                return Err(SftpError::Connect(crate::errors::compose(
                    &crate::errors::timeout_message(&req.host, port, timeout),
                    &format!("等待握手超过 {} 秒", timeout.as_secs()),
                )));
            }
        };

        authenticate(&mut session, &req.username, &req.auth)
            .await
            .map_err(|e| SftpError::Auth(e.to_string()))?;

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
        if let Some(s) = self.inner.lock().await.remove(id) {
            let _ = s.close().await;
        }
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

    /// 把远端文件整个读进内存（阅读窗口用），超过 `max` 字节直接拒绝。
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
                    "文件超过 {} MB，不适合阅读",
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
            return Err(SftpError::Remote("文件过大，不适合阅读".into()));
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
