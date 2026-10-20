/**
 * 可在「阅读」窗口打开的类型。规则与后端 reader::kind_of 保持一致（改一边记得改另一边）。
 * - markdown / html：渲染
 * - image：图片查看器
 * - code：配置 / 文本文件，语法高亮 + 行号
 */
export type ReadableKind = "markdown" | "html" | "image" | "code";

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

export function readableKind(path: string): ReadableKind | null {
  const name = baseName(path);
  if (name in CODE_NAMES || name.startsWith(".env.")) return "code";
  const ext = extOf(name);
  if (!ext) return null;
  if (MD.includes(ext)) return "markdown";
  if (HTML.includes(ext)) return "html";
  if (ext in IMAGE_MIME) return "image";
  if (ext in CODE_EXT) return "code";
  return null;
}

export function imageMime(path: string): string {
  return IMAGE_MIME[extOf(baseName(path)) ?? ""] ?? "application/octet-stream";
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
export function kindLabel(path: string, kind: ReadableKind): string {
  if (kind === "markdown") return "MD";
  if (kind === "html") return "HTML";
  const name = baseName(path);
  if (name.startsWith(".env")) return "ENV";
  if (name in CODE_NAMES) return name.replace(/^\./, "").toUpperCase().slice(0, 10);
  return (extOf(name) ?? "TXT").toUpperCase();
}

/** 文件列表图标 */
export function kindIcon(kind: ReadableKind | null): string {
  switch (kind) {
    case "markdown":
    case "html":
      return "📖";
    case "image":
      return "🖼️";
    case "code":
      return "🧾";
    default:
      return "📄";
  }
}
