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
use std::path::Path;

use base64::Engine as _;
use parking_lot::Mutex;
use serde::Serialize;
use tauri::http::{header, Response, StatusCode};
use tauri::{AppHandle, Manager, State, WebviewUrl, WebviewWindowBuilder, WindowEvent};

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
        ".env", ".gitconfig", ".gitmodules", ".editorconfig", ".npmrc",
        ".bashrc", ".zshrc", ".profile", ".bash_profile", "dockerfile", "makefile",
    ];
    if CODE_NAMES.contains(&name.as_str()) || name.starts_with(".env.") {
        return Some(DocKind::Code);
    }
    let Some((stem, ext)) = name.rsplit_once('.') else {
        // 没有扩展名：按纯文本处理（README、日志、无后缀导出文件都是这类）。
        // ".foo" 这种隐藏文件走的是上面 rsplit_once 能切出空 stem 的分支，不会到这里。
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

struct Doc {
    name: String,
    /// 展示用来源，如 `user@host:/path/README.md`
    source: String,
    kind: DocKind,
    bytes: Vec<u8>,
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
}

/// 放入文档并打开浏览窗口
fn open_window(app: &AppHandle, doc: Doc) -> Result<(), String> {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let label = format!("viewer-{id}");
    let title = format!("浏览 · {}", doc.name);
    app.state::<DocStore>().0.lock().insert(id.clone(), doc);

    let init = format!("window.__NX_VIEWER__ = {};", serde_json::to_string(&id).unwrap());
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
            Err(format!("无法打开浏览窗口：{e}"))
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
        "不支持的文件类型。可浏览：Markdown、HTML、图片（png/jpg/gif/webp/bmp/ico/svg/avif）、\
         音频（mp3/wav/flac/ogg 等）、视频（mp4/webm/mov/mkv/avi 等，≤20MB）、\
         文本与配置文件（json/yaml/toml/ini/conf/properties/xml/sql/.env 等，无扩展名的文件按纯文本处理）"
            .to_string()
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
    let bytes = state
        .sftp
        .read(&sftp_id, &path, kind.max_bytes())
        .await
        .map_err(|e| e.to_string())?;
    open_window(
        &app,
        Doc { name: base_name(&path), source, kind, bytes },
    )
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
    let bytes = state
        .sessions
        .read_file(&id, &path, kind.max_bytes())
        .await
        .map_err(|e| e.to_string())?;
    open_window(
        &app,
        Doc { name: base_name(&path), source, kind, bytes },
    )
}

/// 本机文件（本地终端里选中的绝对路径）
#[tauri::command]
pub async fn viewer_open_local(app: AppHandle, path: String) -> Result<(), String> {
    let kind = require_kind(&path)?;
    let p = crate::keys::expand_tilde(&path);
    let meta = tokio::fs::metadata(&p)
        .await
        .map_err(|e| format!("无法读取「{path}」：{e}"))?;
    if meta.is_dir() {
        return Err(format!("「{path}」是目录"));
    }
    if meta.len() as usize > kind.max_bytes() {
        return Err(format!("文件超过 {} MB，不适合浏览", kind.max_bytes() / 1024 / 1024));
    }
    let bytes = tokio::fs::read(&p)
        .await
        .map_err(|e| format!("无法读取「{path}」：{e}"))?;
    let source = Path::new(&p).display().to_string();
    open_window(
        &app,
        Doc { name: base_name(&path), source, kind, bytes },
    )
}

/// 浏览窗口取内容
#[tauri::command]
pub fn viewer_doc(id: String, store: State<'_, DocStore>) -> Result<DocPayload, String> {
    let map = store.0.lock();
    let d = map.get(&id).ok_or("文档已关闭或不存在")?;
    Ok(DocPayload {
        name: d.name.clone(),
        source: d.source.clone(),
        kind: d.kind,
        data: base64::engine::general_purpose::STANDARD.encode(&d.bytes),
    })
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
    let allow_js = uri.query().is_some_and(|q| q.split('&').any(|kv| kv == "js=1"));

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

    #[test]
    fn csp_toggles_script() {
        assert!(csp(false).contains("script-src 'none'"));
        assert!(csp(true).contains("script-src *"));
        assert!(!csp(true).contains("allow-same-origin"));
    }
}
