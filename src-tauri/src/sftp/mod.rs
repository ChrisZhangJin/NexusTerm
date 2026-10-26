use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use russh::client::Handle;
use russh_sftp::client::SftpSession;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::ssh::session::{ConnectRequest, SharedConns};
use crate::ssh::{ClientHandler, KnownHosts};

/// 单次读写的块大小：够大以摊薄每次系统调用/IPC 开销，又不至于让一次 `read`
/// 读太久才检查一次取消标志。
const CHUNK_SIZE: usize = 256 * 1024;
/// 进度事件节流：传输期间最多每隔这么久往前端推一次，避免几十万次 IPC。
const PROGRESS_INTERVAL_MS: u128 = 120;

/// 推给前端的传输进度；`transferId` 由前端生成，贯穿一次上传/下载的始终。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferProgress {
    pub transfer_id: String,
    pub transferred: u64,
    pub total: u64,
}

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
    /// 用户主动取消，不是真正的传输失败——调用方据此区分"已取消"与"出错"
    #[error("已取消")]
    Canceled,
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
    /// 进行中传输的取消标志，按前端生成的 transfer id 登记。
    /// 置位由 `cancel_transfer` 做；拷贝循环每轮检查一次；传输结束（无论成败）
    /// 都要 `end_transfer` 清掉，不然标志会无限堆积。
    transfers: parking_lot::Mutex<HashMap<String, Arc<AtomicBool>>>,
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
            transfers: parking_lot::Mutex::new(HashMap::new()),
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

    /// 登记一次传输的取消标志；调用方全程持有返回的 `Arc` 在拷贝循环里轮询。
    fn register_transfer(&self, transfer_id: &str) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        self.transfers.lock().insert(transfer_id.to_string(), flag.clone());
        flag
    }

    /// 取消一次正在进行的传输；对已经结束/不存在的 id 是no-op。
    pub fn cancel_transfer(&self, transfer_id: &str) {
        if let Some(flag) = self.transfers.lock().get(transfer_id) {
            flag.store(true, Ordering::Relaxed);
        }
    }

    /// 传输结束（成功/失败/取消）都要调用，否则取消标志会无限堆积。
    fn end_transfer(&self, transfer_id: &str) {
        self.transfers.lock().remove(transfer_id);
    }

    /// 按节流间隔把进度推给前端；`force` 用于第一帧和最后一帧，不受节流限制。
    fn emit_progress(
        app: &AppHandle,
        transfer_id: &str,
        transferred: u64,
        total: u64,
        last_emit: &mut Instant,
        force: bool,
    ) {
        if !force && last_emit.elapsed().as_millis() < PROGRESS_INTERVAL_MS {
            return;
        }
        *last_emit = Instant::now();
        let _ = app.emit(
            "nx://sftp/progress",
            TransferProgress { transfer_id: transfer_id.to_string(), transferred, total },
        );
    }

    /// 下载远端文件到本地路径，返回写入字节数。
    ///
    /// 分块拷贝而不是整体 `io::copy`：一是能按 `transfer_id` 周期性推进度事件，
    /// 二是每轮能检查一次取消标志及时中止——两者都要求拷贝循环自己掌控节奏。
    pub async fn download(
        &self,
        id: &str,
        remote: &str,
        local: &str,
        transfer_id: &str,
        app: &AppHandle,
    ) -> Result<u64, SftpError> {
        let cancel = self.register_transfer(transfer_id);
        let result: Result<u64, SftpError> = async {
            // File 内部持有 Arc<RawSftpSession>，不借用 &self，因此拿到手就能释放锁，
            // 之后的分块读写不会卡住其他 SFTP 操作（列目录等）。
            let (mut file, total) = {
                let map = self.inner.lock().await;
                let s = map
                    .get(id)
                    .ok_or_else(|| SftpError::NotFound(id.to_string()))?;
                let meta = s
                    .metadata(remote.to_string())
                    .await
                    .map_err(|e| SftpError::Remote(e.to_string()))?;
                let total = meta.size.unwrap_or(0);
                let file = s
                    .open(remote.to_string())
                    .await
                    .map_err(|e| SftpError::Remote(e.to_string()))?;
                (file, total)
            };

            let mut out = tokio::fs::File::create(local)
                .await
                .map_err(|e| SftpError::Remote(format!("无法创建本地文件 {}: {}", local, e)))?;

            let mut buf = vec![0u8; CHUNK_SIZE];
            let mut transferred = 0u64;
            let mut last_emit = Instant::now();
            Self::emit_progress(app, transfer_id, 0, total, &mut last_emit, true);
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err(SftpError::Canceled);
                }
                let n = file
                    .read(&mut buf)
                    .await
                    .map_err(|e| SftpError::Remote(e.to_string()))?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n])
                    .await
                    .map_err(|e| SftpError::Remote(e.to_string()))?;
                transferred += n as u64;
                Self::emit_progress(app, transfer_id, transferred, total, &mut last_emit, false);
            }
            Self::emit_progress(app, transfer_id, transferred, total, &mut last_emit, true);

            out.flush()
                .await
                .map_err(|e| SftpError::Remote(e.to_string()))?;
            file.close()
                .await
                .map_err(|e| SftpError::Remote(e.to_string()))?;

            Ok(transferred)
        }
        .await;
        self.end_transfer(transfer_id);
        result
    }

    /// 把远端文件整个读进内存（浏览窗口用），超过 `max` 字节直接拒绝。
    pub async fn read(&self, id: &str, path: &str, max: usize) -> Result<Vec<u8>, SftpError> {
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

    /// 上传本地文件到远端路径，返回写入字节数。分块拷贝的理由同 `download`。
    pub async fn upload(
        &self,
        id: &str,
        local: &str,
        remote: &str,
        transfer_id: &str,
        app: &AppHandle,
    ) -> Result<u64, SftpError> {
        let cancel = self.register_transfer(transfer_id);
        let result: Result<u64, SftpError> = async {
            let mut src = tokio::fs::File::open(local)
                .await
                .map_err(|e| SftpError::Remote(format!("无法读取本地文件 {}: {}", local, e)))?;
            let total = src
                .metadata()
                .await
                .map_err(|e| SftpError::Remote(e.to_string()))?
                .len();

            let mut file = {
                let map = self.inner.lock().await;
                let s = map
                    .get(id)
                    .ok_or_else(|| SftpError::NotFound(id.to_string()))?;
                s.create(remote.to_string())
                    .await
                    .map_err(|e| SftpError::Remote(e.to_string()))?
            };

            let mut buf = vec![0u8; CHUNK_SIZE];
            let mut transferred = 0u64;
            let mut last_emit = Instant::now();
            Self::emit_progress(app, transfer_id, 0, total, &mut last_emit, true);
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err(SftpError::Canceled);
                }
                let n = src
                    .read(&mut buf)
                    .await
                    .map_err(|e| SftpError::Remote(e.to_string()))?;
                if n == 0 {
                    break;
                }
                file.write_all(&buf[..n])
                    .await
                    .map_err(|e| SftpError::Remote(e.to_string()))?;
                transferred += n as u64;
                Self::emit_progress(app, transfer_id, transferred, total, &mut last_emit, false);
            }
            Self::emit_progress(app, transfer_id, transferred, total, &mut last_emit, true);

            file.close()
                .await
                .map_err(|e| SftpError::Remote(e.to_string()))?;

            Ok(transferred)
        }
        .await;
        self.end_transfer(transfer_id);
        result
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个不连任何东西的 `SftpManager`：三个依赖都能零 I/O 造出来，
    /// 足够单测 `register_transfer`/`cancel_transfer`/`end_transfer` 这套
    /// 纯内存登记表的行为，不需要真实的 SSH/SFTP 连接。
    fn make_manager() -> SftpManager {
        SftpManager::new(
            Arc::new(KnownHosts::new()),
            Arc::new(crate::ssh::prompt::AuthPrompts::new()),
            Arc::new(parking_lot::Mutex::new(HashMap::new())),
        )
    }

    #[test]
    fn transfer_registry_tracks_and_clears_cancel_flags() {
        let mgr = make_manager();
        let flag = mgr.register_transfer("t1");
        assert!(!flag.load(Ordering::Relaxed), "刚登记不该是已取消状态");

        mgr.cancel_transfer("t1");
        assert!(flag.load(Ordering::Relaxed), "取消要能让调用方已经拿到手的 Arc 看到置位");

        mgr.end_transfer("t1");
        assert!(
            mgr.transfers.lock().get("t1").is_none(),
            "传输结束要把登记表里的条目清掉，否则取消标志会无限堆积"
        );
    }

    #[test]
    fn cancel_unknown_transfer_is_noop() {
        let mgr = make_manager();
        // 不存在的 id：不能 panic，也不能凭空插入一条新记录
        mgr.cancel_transfer("ghost");
        assert!(mgr.transfers.lock().is_empty());
    }

    #[test]
    fn two_transfers_have_independent_cancel_flags() {
        let mgr = make_manager();
        let a = mgr.register_transfer("a");
        let b = mgr.register_transfer("b");
        mgr.cancel_transfer("a");
        assert!(a.load(Ordering::Relaxed));
        assert!(!b.load(Ordering::Relaxed), "取消一个不该连带取消另一个");
        mgr.end_transfer("a");
        assert!(mgr.transfers.lock().contains_key("b"), "结束 a 不该带走 b 的登记");
    }
}
