/**
 * 可在「浏览」窗口打开的类型。规则与后端 viewer::kind_of 保持一致（改一边记得改另一边）。
 * - markdown / html：渲染
 * - image：图片查看器
 * - audio / video：&lt;audio&gt;/&lt;video&gt; 标签播放
 * - code：配置 / 文本文件，语法高亮 + 行号；没有扩展名的文件也按纯文本处理
 */
export type ViewableKind = "markdown" | "html" | "image" | "audio" | "video" | "code";

const MD = ["md", "markdown", "mdown", "mkd", "mkdn"];
const HTML = ["html", "htm", "xhtml"];

const IMAGE_MIME: Record<string, string> = {
  png: "image/png",
  jpg: "image/jpeg",
  jpeg: "image/jpeg",
  jfif: "image/jpeg",
  gif: "image/gif",
  webp: "image/webp",
  bmp: "image/bmp",
  ico: "image/x-icon",
  svg: "image/svg+xml",
  avif: "image/avif",
};

const AUDIO_MIME: Record<string, string> = {
  mp3: "audio/mpeg",
  wav: "audio/wav",
  flac: "audio/flac",
  ogg: "audio/ogg",
  m4a: "audio/mp4",
  aac: "audio/aac",
  opus: "audio/opus",
  wma: "audio/x-ms-wma",
};

const VIDEO_MIME: Record<string, string> = {
  mp4: "video/mp4",
  webm: "video/webm",
  mov: "video/quicktime",
  mkv: "video/x-matroska",
  avi: "video/x-msvideo",
  m4v: "video/mp4",
};

/** 扩展名 → highlight.js 语言（toml 用 ini 语法足够好） */
const CODE_EXT: Record<string, string> = {
  json: "json",
  jsonc: "json",
  json5: "json",
  yaml: "yaml",
  yml: "yaml",
  toml: "ini",
  ini: "ini",
  cfg: "ini",
  conf: "ini",
  cnf: "ini",
  env: "ini",
  service: "ini",
  properties: "properties",
  xml: "xml",
  plist: "xml",
  sql: "sql",
  txt: "plaintext",
  log: "plaintext",
};

/** 没有扩展名、靠文件名识别的常见配置文件 */
const CODE_NAMES: Record<string, string> = {
  ".env": "ini",
  ".gitconfig": "ini",
  ".gitmodules": "ini",
  ".editorconfig": "ini",
  ".npmrc": "ini",
  ".bashrc": "bash",
  ".zshrc": "bash",
  ".profile": "bash",
  ".bash_profile": "bash",
  dockerfile: "dockerfile",
  makefile: "makefile",
};

function baseName(path: string) {
  const parts = path.split(/[/\\]/).filter(Boolean);
  return (parts[parts.length - 1] ?? path).toLowerCase();
}

function extOf(name: string): string | null {
  const i = name.lastIndexOf(".");
  if (i <= 0 || i === name.length - 1) return null; // ".bashrc" 这类不算扩展名
  return name.slice(i + 1);
}

/**
 * @param opts.requireExt 为 true 时，没有扩展名一律当不可浏览（不走纯文本兜底）。
 *   给「鼠标落点自动取词」这类低置信度来源用：提示符整行没有空格会被当成一个词，
 *   `README` 式兜底会让它被误判成文本文件，拼出不存在的路径发去 `cat`。
 */
export function viewableKind(path: string, opts?: { requireExt?: boolean }): ViewableKind | null {
  const name = baseName(path);
  if (name in CODE_NAMES || name.startsWith(".env.")) return "code";
  const ext = extOf(name);
  if (!ext) {
    if (opts?.requireExt) return null;
    // `extOf` 对「真没有扩展名」（README）和 ".hidden" 都返回 null，但只有前者按纯文本
    // 兜底：后者维持不可浏览，与后端 `kind_of` 的 `rsplit_once('.')` 一致，否则菜单项
    // 看着可点、点下去却被后端拒绝。
    return name.includes(".") ? null : "code";
  }
  if (MD.includes(ext)) return "markdown";
  if (HTML.includes(ext)) return "html";
  if (ext in IMAGE_MIME) return "image";
  if (ext in AUDIO_MIME) return "audio";
  if (ext in VIDEO_MIME) return "video";
  if (ext in CODE_EXT) return "code";
  return null;
}

export function imageMime(path: string): string {
  return IMAGE_MIME[extOf(baseName(path)) ?? ""] ?? "application/octet-stream";
}

export function audioMime(path: string): string {
  return AUDIO_MIME[extOf(baseName(path)) ?? ""] ?? "application/octet-stream";
}

export function videoMime(path: string): string {
  return VIDEO_MIME[extOf(baseName(path)) ?? ""] ?? "application/octet-stream";
}

export function isSvg(path: string) {
  return extOf(baseName(path)) === "svg";
}

export function isJson(path: string) {
  return extOf(baseName(path)) === "json";
}

export function codeLang(path: string): string {
  const name = baseName(path);
  if (name in CODE_NAMES) return CODE_NAMES[name];
  if (name.startsWith(".env.")) return "ini";
  const ext = extOf(name) ?? "";
  if (ext === "conf" && name.includes("nginx")) return "nginx";
  return CODE_EXT[ext] ?? "plaintext";
}

/** 顶栏徽标文字 */
export function kindLabel(path: string, kind: ViewableKind): string {
  if (kind === "markdown") return "MD";
  if (kind === "html") return "HTML";
  const name = baseName(path);
  if (name.startsWith(".env")) return "ENV";
  if (name in CODE_NAMES) return name.replace(/^\./, "").toUpperCase().slice(0, 10);
  return (extOf(name) ?? "TXT").toUpperCase();
}

/** 文件列表图标 */
export function kindIcon(kind: ViewableKind | null): string {
  switch (kind) {
    case "markdown":
    case "html":
      return "📖";
    case "image":
      return "🖼️";
    case "audio":
      return "🎵";
    case "video":
      return "🎬";
    case "code":
      return "🧾";
    default:
      return "📄";
  }
}
