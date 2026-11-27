//! 「浏览」窗口：在独立窗口里查看 Markdown / HTML（渲染）、图片、音频 / 视频、文本与配置文件（语法高亮）。
//!
//! 数据流：读文件（SFTP / SSH exec / 本机）→ 放进 [`DocStore`] → 开 `viewer-<id>` 窗口。
//! 浏览窗口加载的仍是本应用的 index.html（由初始化脚本注入 `__NX_VIEWER__` 切到浏览界面），
//! 再用 `viewer_doc` 取内容：
//! - Markdown：前端 marked + DOMPurify 渲染，不执行任何脚本；
//! - HTML：放进 sandbox iframe，iframe 的 src 指向自定义协议 `nxdoc`，
//!   由本模块按需返回带 CSP 头的页面 —— 默认禁止脚本，用户在窗口里勾选后才放开。
//!   iframe 永远不给 allow-same-origin，页面是不透明源，碰不到应用的 IPC。
//! - 图片 / 音频 / 视频：前端用 Blob URL 接到 <img>/<audio>/<video> 标签上。

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use parking_lot::Mutex;
use serde::Serialize;
use tauri::http::{header, Response, StatusCode};
use tauri::{
    AppHandle, Manager, State, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent,
};
use tokio::io::AsyncReadExt;

use crate::cmd::AppState;

/// 单文件上限：浏览用，不是传输工具；过大的文件渲染也没有意义
pub const MAX_BYTES: usize = 20 * 1024 * 1024;
/// 图片单独放宽（相机原图常见 10~30 MB）
pub const MAX_IMAGE_BYTES: usize = 50 * 1024 * 1024;
/// 音频单独放宽，与图片同档
pub const MAX_AUDIO_BYTES: usize = 50 * 1024 * 1024;
/// 视频单独收紧：体积动辄上百 MB，整个读进内存再走 IPC base64 不划算
pub const MAX_VIDEO_BYTES: usize = 20 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DocKind {
    Markdown,
    Html,
    /// 图片：前端用 Blob URL 显示（SVG 也走 <img>，其中脚本不会执行）
    Image,
    /// 音频：前端用 Blob URL 接到 <audio>
    Audio,
    /// 视频：前端用 Blob URL 接到 <video>
    Video,
    /// 配置 / 文本：前端语法高亮
    Code,
}

impl DocKind {
    pub fn max_bytes(self) -> usize {
        match self {
            DocKind::Image => MAX_IMAGE_BYTES,
            DocKind::Audio => MAX_AUDIO_BYTES,
            DocKind::Video => MAX_VIDEO_BYTES,
            _ => MAX_BYTES,
        }
    }
}

/// 按文件名判断可浏览类型（与前端 src/lib/viewable.ts 保持一致）
pub fn kind_of(path: &str) -> Option<DocKind> {
    let name = path
        .rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or(path)
        .to_ascii_lowercase();
    const CODE_NAMES: &[&str] = &[
        ".env",
        ".gitconfig",
        ".gitmodules",
        ".editorconfig",
        ".npmrc",
        ".bashrc",
        ".zshrc",
        ".profile",
        ".bash_profile",
        "dockerfile",
        "makefile",
    ];
    if CODE_NAMES.contains(&name.as_str()) || name.starts_with(".env.") {
        return Some(DocKind::Code);
    }
    let Some((stem, ext)) = name.rsplit_once('.') else {
        // 没有扩展名：按纯文本处理（README、日志、无后缀导出文件）
        return Some(DocKind::Code);
    };
    if stem.is_empty() || ext.is_empty() {
        return None; // ".foo" 这类隐藏文件不算扩展名
    }
    match ext {
        "md" | "markdown" | "mdown" | "mkd" | "mkdn" => Some(DocKind::Markdown),
        "html" | "htm" | "xhtml" => Some(DocKind::Html),
        "png" | "jpg" | "jpeg" | "jfif" | "gif" | "webp" | "bmp" | "ico" | "svg" | "avif" => {
            Some(DocKind::Image)
        }
        "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" | "opus" | "wma" => Some(DocKind::Audio),
        "mp4" | "webm" | "mov" | "mkv" | "avi" | "m4v" => Some(DocKind::Video),
        "json" | "jsonc" | "json5" | "yaml" | "yml" | "toml" | "ini" | "cfg" | "conf" | "cnf"
        | "env" | "service" | "properties" | "xml" | "plist" | "sql" | "txt" | "log" => {
            Some(DocKind::Code)
        }
        _ => None,
    }
}

#[derive(Clone)]
enum DocOrigin {
    Local(PathBuf),
    Sftp { id: String, path: String },
    Ssh { id: String, path: String },
}

impl DocOrigin {
    async fn read(&self, state: &AppState, kind: DocKind) -> Result<Vec<u8>, String> {
        match self {
            Self::Local(path) => read_local(path, kind).await,
            Self::Sftp { id, path } => state
                .sftp
                .read(id, path, kind.max_bytes())
                .await
                .map_err(|e| e.to_string()),
            Self::Ssh { id, path } => state
                .sessions
                .read_file(id, path, kind.max_bytes())
                .await
                .map_err(|e| e.to_string()),
        }
    }
}

struct Doc {
    name: String,
    /// 展示用来源，如 `user@host:/path/README.md`
    source: String,
    kind: DocKind,
    bytes: Vec<u8>,
    /// 仅 HTML 保留读取所需的原始来源（与展示用来源分开）。
    origin: Option<DocOrigin>,
    opened: bool,
    revision: u64,
    refresh_generation: u64,
}

impl Doc {
    fn new(name: String, source: String, kind: DocKind, bytes: Vec<u8>, origin: DocOrigin) -> Self {
        Self {
            name,
            source,
            kind,
            bytes,
            origin: (kind == DocKind::Html).then_some(origin),
            opened: false,
            revision: 0,
            refresh_generation: 0,
        }
    }

    fn payload(&self) -> DocPayload {
        DocPayload {
            name: self.name.clone(),
            source: self.source.clone(),
            kind: self.kind,
            data: base64::engine::general_purpose::STANDARD.encode(&self.bytes),
            revision: self.revision,
            refresh_error: None,
        }
    }
}

#[derive(Default)]
pub struct DocStore(Mutex<HashMap<String, Doc>>);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocPayload {
    name: String,
    source: String,
    kind: DocKind,
    /// 原始字节（base64）：编码由前端探测（UTF-8 优先，失败回退 GBK）
    data: String,
    revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    refresh_error: Option<String>,
}

fn closed_doc() -> String {
    crate::tr!("文档已关闭或不存在", "Document not found or already closed")
}

impl DocStore {
    /// 第一次加载直接用打开时读到的数据；HTML 窗口重载时再读原文件。
    async fn load<F>(
        &self,
        id: &str,
        read: impl FnOnce(DocOrigin) -> F,
    ) -> Result<DocPayload, String>
    where
        F: Future<Output = Result<Vec<u8>, String>>,
    {
        {
            let mut map = self.0.lock();
            let doc = map.get_mut(id).ok_or_else(closed_doc)?;
            if !doc.opened || doc.origin.is_none() {
                doc.opened = true;
                return Ok(doc.payload());
            }
        }
        match self.refresh(id, read).await {
            Ok(payload) => Ok(payload),
            Err(error) => {
                let map = self.0.lock();
                let mut payload = map.get(id).ok_or_else(closed_doc)?.payload();
                payload.refresh_error = Some(error);
                Ok(payload)
            }
        }
    }

    async fn refresh<F>(
        &self,
        id: &str,
        read: impl FnOnce(DocOrigin) -> F,
    ) -> Result<DocPayload, String>
    where
        F: Future<Output = Result<Vec<u8>, String>>,
    {
        let (origin, generation) = {
            let mut map = self.0.lock();
            let doc = map.get_mut(id).ok_or_else(closed_doc)?;
            let origin = doc.origin.clone().ok_or_else(|| {
                crate::tr!(
                    "仅支持刷新 HTML 文档",
                    "Only HTML documents can be refreshed"
                )
            })?;
            doc.refresh_generation += 1;
            (origin, doc.refresh_generation)
        };
        let bytes = read(origin).await?;
        let mut map = self.0.lock();
        let doc = map.get_mut(id).ok_or_else(closed_doc)?;
        // 已关闭的文档不会重建；并发刷新也不能覆盖已更新的快照。
        if generation > doc.revision {
            doc.bytes = bytes;
            doc.revision = generation;
        }
        Ok(doc.payload())
    }
}

/// 放入文档并打开浏览窗口
fn open_window(app: &AppHandle, doc: Doc) -> Result<(), String> {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let label = format!("viewer-{id}");
    let title = crate::tr!("浏览 · {}", "View · {}", doc.name);
    app.state::<DocStore>().0.lock().insert(id.clone(), doc);

    let init = format!(
        "window.__NX_VIEWER__ = {};",
        serde_json::to_string(&id).unwrap()
    );
    let built = WebviewWindowBuilder::new(app, &label, WebviewUrl::App("index.html".into()))
        .title(&title)
        .inner_size(960.0, 780.0)
        .min_inner_size(420.0, 320.0)
        .center()
        .initialization_script(&init)
        .build();

    match built {
        Ok(win) => {
            // 窗口关掉即释放内容
            let app2 = app.clone();
            win.on_window_event(move |e| {
                if matches!(e, WindowEvent::Destroyed) {
                    app2.state::<DocStore>().0.lock().remove(&id);
                }
            });
            Ok(())
        }
        Err(e) => {
            app.state::<DocStore>().0.lock().remove(&id);
            Err(crate::tr!(
                "无法打开浏览窗口：{e}",
                "Cannot open the viewer window: {e}"
            ))
        }
    }
}

fn base_name(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or(path)
        .to_string()
}

fn require_kind(path: &str) -> Result<DocKind, String> {
    kind_of(path).ok_or_else(|| {
        crate::tr!(
            "不支持的文件类型。可浏览：Markdown、HTML、图片（png/jpg/gif/webp/bmp/ico/svg/avif）、\
             音频（mp3/wav/flac/ogg 等）、视频（mp4/webm/mov/mkv/avi 等，≤20MB）、\
             文本与配置文件（json/yaml/toml/ini/conf/properties/xml/sql/.env 等，无扩展名的文件按纯文本处理）",
            "Unsupported file type. Viewable: Markdown, HTML, images (png/jpg/gif/webp/bmp/ico/svg/avif), \
             audio (mp3/wav/flac/ogg, etc.), video (mp4/webm/mov/mkv/avi, etc., ≤20 MB), \
             text and config files (json/yaml/toml/ini/conf/properties/xml/sql/.env, etc.; files without an extension open as plain text)"
        )
    })
}

// ——— 命令 ———

/// SFTP 面板里的远端文件
#[tauri::command]
pub async fn viewer_open_sftp(
    app: AppHandle,
    sftp_id: String,
    path: String,
    source: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let kind = require_kind(&path)?;
    let name = base_name(&path);
    let origin = DocOrigin::Sftp { id: sftp_id, path };
    let bytes = origin.read(&state, kind).await?;
    open_window(&app, Doc::new(name, source, kind, bytes, origin))
}

/// SSH 终端里选中的文件名：在同一连接上开 exec 通道 cat 出来（不需要 SFTP 子系统）。
/// `path` 由前端解析好（绝对路径，或 `~/` 开头）。
#[tauri::command]
pub async fn viewer_open_ssh(
    app: AppHandle,
    id: String,
    path: String,
    source: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let kind = require_kind(&path)?;
    let name = base_name(&path);
    let origin = DocOrigin::Ssh { id, path };
    let bytes = origin.read(&state, kind).await?;
    open_window(&app, Doc::new(name, source, kind, bytes, origin))
}

/// 本机文件（本地终端里选中的绝对路径）
#[tauri::command]
pub async fn viewer_open_local(app: AppHandle, path: String) -> Result<(), String> {
    let kind = require_kind(&path)?;
    let mut resolved = PathBuf::from(crate::keys::expand_tilde(&path));
    if kind == DocKind::Html && !resolved.is_absolute() {
        resolved = std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(resolved);
    }
    let bytes = read_local(&resolved, kind).await?;
    let source = resolved.display().to_string();
    open_window(
        &app,
        Doc::new(
            base_name(&path),
            source,
            kind,
            bytes,
            DocOrigin::Local(resolved),
        ),
    )
}

async fn read_local(p: &Path, kind: DocKind) -> Result<Vec<u8>, String> {
    let path = p.display();
    let read_error = |e| crate::tr!("无法读取「{path}」：{e}", "Cannot read \"{path}\": {e}");
    let max = kind.max_bytes();
    let too_big = || {
        crate::tr!(
            "文件超过 {} MB，不适合浏览",
            "File is larger than {} MB, too big to view",
            max / 1024 / 1024
        )
    };
    let meta = tokio::fs::metadata(p).await.map_err(read_error)?;
    if meta.is_dir() {
        return Err(crate::tr!("「{path}」是目录", "\"{path}\" is a folder"));
    }
    if meta.len() > max as u64 {
        return Err(too_big());
    }
    if kind != DocKind::Html {
        return tokio::fs::read(p).await.map_err(read_error);
    }
    // HTML 刷新时文件可能正在写入；即便通过了 metadata 检查也要限制实际读入的字节数。
    let mut file = tokio::fs::File::open(p)
        .await
        .map_err(read_error)?
        .take(max as u64 + 1);
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.read_to_end(&mut bytes).await.map_err(read_error)?;
    if bytes.len() > max {
        return Err(too_big());
    }
    Ok(bytes)
}

fn check_viewer(window: &WebviewWindow, id: &str) -> Result<(), String> {
    if window.label().strip_prefix("viewer-") != Some(id) {
        return Err(closed_doc());
    }
    Ok(())
}

/// 浏览窗口取内容；HTML 窗口重载时尝试重新读取原始文件。
#[tauri::command]
pub async fn viewer_doc(
    id: String,
    window: WebviewWindow,
    store: State<'_, DocStore>,
    state: State<'_, AppState>,
) -> Result<DocPayload, String> {
    check_viewer(&window, &id)?;
    let state = state.inner();
    store
        .load(&id, |origin| async move {
            origin.read(state, DocKind::Html).await
        })
        .await
}

/// 只允许刷新本浏览窗口已经打开的 HTML 文档，路径和连接均由后端保存。
#[tauri::command]
pub async fn viewer_refresh(
    id: String,
    window: WebviewWindow,
    store: State<'_, DocStore>,
    state: State<'_, AppState>,
) -> Result<DocPayload, String> {
    check_viewer(&window, &id)?;
    let state = state.inner();
    store
        .refresh(&id, |origin| async move {
            origin.read(state, DocKind::Html).await
        })
        .await
}

// ——— nxdoc:// 协议：给 HTML iframe 供页面 ———

/// 不允许脚本时：只放行静态资源（远程图片 / 样式 / 字体），
/// 允许脚本时：额外放开 script / connect。frame / object / form 一律禁止。
fn csp(allow_js: bool) -> String {
    let mut s = String::from(
        "default-src 'none'; img-src * data: blob:; media-src * data: blob:; \
         style-src * 'unsafe-inline'; font-src * data:; \
         frame-src 'none'; object-src 'none'; form-action 'none'; base-uri 'none'",
    );
    if allow_js {
        s.push_str("; script-src * 'unsafe-inline' 'unsafe-eval' data: blob:; connect-src *");
    } else {
        s.push_str("; script-src 'none'");
    }
    s
}

pub fn protocol<R: tauri::Runtime>(
    ctx: tauri::UriSchemeContext<'_, R>,
    req: tauri::http::Request<Vec<u8>>,
) -> Response<Vec<u8>> {
    let uri = req.uri();
    let id = uri.path().trim_start_matches('/').to_string();
    let allow_js = uri
        .query()
        .is_some_and(|q| q.split('&').any(|kv| kv == "js=1"));

    let store = ctx.app_handle().state::<DocStore>();
    let map = store.0.lock();
    let Some(doc) = map.get(&id).filter(|d| d.kind == DocKind::Html) else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body("not found".as_bytes().to_vec())
            .unwrap();
    };
    // 合法 UTF-8 就明确声明；否则不写 charset，交给页面自己的 <meta charset> 决定（GBK 老页面）
    let ctype = if std::str::from_utf8(&doc.bytes).is_ok() {
        "text/html; charset=utf-8"
    } else {
        "text/html"
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, ctype)
        .header("Content-Security-Policy", csp(allow_js))
        .header("X-Content-Type-Options", "nosniff")
        .header("Referrer-Policy", "no-referrer")
        .header(header::CACHE_CONTROL, "no-store")
        .body(doc.bytes.clone())
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(kind_of("/a/README.md"), Some(DocKind::Markdown));
        assert_eq!(kind_of("x.MarkDown"), Some(DocKind::Markdown));
        assert_eq!(kind_of("C:\\x\\index.HTM"), Some(DocKind::Html));
        assert_eq!(kind_of("a.exe"), None);
        assert_eq!(kind_of(".hidden"), None);
        assert_eq!(kind_of("/img/Logo.PNG"), Some(DocKind::Image));
        assert_eq!(kind_of("icon.svg"), Some(DocKind::Image));
        assert_eq!(kind_of("/etc/nginx/nginx.conf"), Some(DocKind::Code));
        assert_eq!(kind_of("Cargo.toml"), Some(DocKind::Code));
        assert_eq!(kind_of("docker-compose.yml"), Some(DocKind::Code));
        assert_eq!(kind_of("/app/.env"), Some(DocKind::Code));
        assert_eq!(kind_of(".env.production"), Some(DocKind::Code));
        assert_eq!(kind_of("~/.bashrc"), Some(DocKind::Code));
        assert_eq!(kind_of("Dockerfile"), Some(DocKind::Code));
        assert_eq!(kind_of("schema.sql"), Some(DocKind::Code));
        assert_eq!(kind_of("notes.txt"), Some(DocKind::Code));
        // 没有扩展名：按纯文本处理
        assert_eq!(kind_of("README"), Some(DocKind::Code));
        assert_eq!(kind_of("/var/log/syslog"), Some(DocKind::Code));
        assert_eq!(kind_of("md"), Some(DocKind::Code));
        assert_eq!(kind_of("/a/song.mp3"), Some(DocKind::Audio));
        assert_eq!(kind_of("clip.MP4"), Some(DocKind::Video));
        assert_eq!(DocKind::Image.max_bytes(), MAX_IMAGE_BYTES);
        assert_eq!(DocKind::Audio.max_bytes(), MAX_AUDIO_BYTES);
        assert_eq!(DocKind::Video.max_bytes(), MAX_VIDEO_BYTES);
    }

    fn store_with_doc(kind: DocKind, origin: DocOrigin, bytes: Vec<u8>) -> DocStore {
        DocStore(Mutex::new(HashMap::from([(
            "doc".into(),
            Doc::new("test".into(), "display only".into(), kind, bytes, origin),
        )])))
    }

    fn payload_bytes(payload: &DocPayload) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(&payload.data)
            .unwrap()
    }

    async fn read_test_local(origin: DocOrigin) -> Result<Vec<u8>, String> {
        let DocOrigin::Local(path) = origin else {
            panic!("expected a local path")
        };
        read_local(&path, DocKind::Html).await
    }

    #[tokio::test]
    async fn html_refresh_and_window_reload_read_the_original_file() {
        struct TempDir(PathBuf);
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let dir = TempDir(std::env::temp_dir().join(format!("nx-viewer-{}", uuid::Uuid::new_v4())));
        std::fs::create_dir(&dir.0).unwrap();
        let path = dir.0.join("test.html");
        std::fs::write(&path, b"<p>first</p>").unwrap();
        let initial_bytes = read_local(&path, DocKind::Html).await.unwrap();
        let store = store_with_doc(DocKind::Html, DocOrigin::Local(path.clone()), initial_bytes);

        std::fs::write(&path, b"<p>second</p>").unwrap();
        let first = store
            .load("doc", |_| async {
                panic!("initial load should not read again")
            })
            .await
            .unwrap();
        assert_eq!(payload_bytes(&first), b"<p>first</p>");
        assert_eq!(first.revision, 0);
        let next = store.refresh("doc", read_test_local).await.unwrap();
        assert_eq!(payload_bytes(&next), b"<p>second</p>");
        assert!(next.revision > first.revision);
        // The protocol uses these same bytes for the rendered iframe.
        assert_eq!(store.0.lock()["doc"].bytes, b"<p>second</p>");

        let replacement = dir.0.join("replacement.html");
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&replacement, b"<p>third</p>").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let reload = store.load("doc", read_test_local).await.unwrap();
        assert_eq!(payload_bytes(&reload), b"<p>third</p>");
        assert!(reload.revision > next.revision);
        assert!(reload.refresh_error.is_none());

        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_BYTES as u64 + 1)
            .unwrap();
        assert!(store.refresh("doc", read_test_local).await.is_err());
        std::fs::remove_file(&path).unwrap();
        assert!(store.refresh("doc", read_test_local).await.is_err());
        let fallback = store.load("doc", read_test_local).await.unwrap();
        assert!(fallback.refresh_error.is_some());
        assert_eq!(payload_bytes(&fallback), b"<p>third</p>");
        assert_eq!(fallback.revision, reload.revision);
        assert_eq!(store.0.lock()["doc"].bytes, b"<p>third</p>");
    }

    #[tokio::test]
    async fn refresh_does_not_restore_a_closed_doc_or_overwrite_a_newer_refresh() {
        let origin = DocOrigin::Sftp {
            id: "connection".into(),
            path: "/test.html".into(),
        };
        let store = store_with_doc(DocKind::Html, origin, b"initial".to_vec());
        let latest = store.refresh("doc", |origin| async {
            assert!(matches!(origin, DocOrigin::Sftp { id, path } if id == "connection" && path == "/test.html"));
            store.refresh("doc", |_| async { Ok(b"newest".to_vec()) }).await.unwrap();
            Ok(b"outdated".to_vec())
        }).await.unwrap();
        assert_eq!(payload_bytes(&latest), b"newest");

        let closed = store
            .refresh("doc", |_| async {
                store.0.lock().remove("doc");
                Ok(b"late".to_vec())
            })
            .await;
        assert!(closed.is_err());
        assert!(!store.0.lock().contains_key("doc"));
        assert!(store
            .refresh("doc", |_| async {
                panic!("closed document should not be read")
            })
            .await
            .is_err());
    }

    #[tokio::test]
    async fn non_html_keeps_its_snapshot() {
        let origin = DocOrigin::Ssh {
            id: "connection".into(),
            path: "~/test.md".into(),
        };
        let store = store_with_doc(DocKind::Markdown, origin, b"snapshot".to_vec());
        for _ in 0..2 {
            let payload = store
                .load("doc", |_| async {
                    panic!("non-HTML should not be read again")
                })
                .await
                .unwrap();
            assert_eq!(payload_bytes(&payload), b"snapshot");
        }
        assert!(store
            .refresh("doc", |_| async { panic!("non-HTML cannot refresh") })
            .await
            .is_err());
    }

    #[test]
    fn csp_toggles_script() {
        assert!(csp(false).contains("script-src 'none'"));
        assert!(csp(true).contains("script-src *"));
        assert!(!csp(true).contains("allow-same-origin"));
    }
}
