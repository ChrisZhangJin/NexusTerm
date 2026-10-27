import type { Theme } from "../types";

/**
 * 把终端配色翻译成外壳（chrome）配色。主题只定义终端的 16 色，而标题栏/工具栏/侧栏/
 * 状态栏共用同一套 CSS 变量 —— 从终端色派生，新增主题就不必再手写 CSS。
 */

function hexToRgb(hex: string): [number, number, number] {
  const s = hex.trim().replace("#", "");
  const full =
    s.length === 3
      ? s
          .split("")
          .map((c) => c + c)
          .join("")
      : s;
  const n = Number.parseInt(full, 16);
  if (!Number.isFinite(n)) return [0, 0, 0];
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

function rgbToHex(r: number, g: number, b: number): string {
  const c = (v: number) =>
    Math.max(0, Math.min(255, Math.round(v))).toString(16).padStart(2, "0");
  return `#${c(r)}${c(g)}${c(b)}`;
}

/** ratio=0 得到 a，ratio=1 得到 b。 */
function mix(a: string, b: string, ratio: number): string {
  const [ar, ag, ab] = hexToRgb(a);
  const [br, bg, bb] = hexToRgb(b);
  const t = Math.max(0, Math.min(1, ratio));
  return rgbToHex(ar + (br - ar) * t, ag + (bg - ag) * t, ab + (bb - ab) * t);
}

/** 感知亮度（sRGB 加权），用于判断深浅底。 */
function luma(hex: string): number {
  const [r, g, b] = hexToRgb(hex);
  return (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255;
}

/**
 * 返回可写入 `document.documentElement.style` 的 CSS 变量名值对。
 *
 * 深色底：面板/边框靠「向前景色混合」提亮，层次自然；
 * 浅色底：反过来向黑混合，避免面板比终端更亮而糊成一片。
 */
export function themeToChromeVars(t: Theme): Record<string, string> {
  const bg = t.background;
  const fg = t.foreground;
  const dark = luma(bg) < 0.5;
  const shade = dark ? fg : "#000000";

  return {
    "--bg": bg,
    "--bg-panel": mix(bg, shade, dark ? 0.06 : 0.05),
    "--bg-elev": mix(bg, shade, dark ? 0.12 : 0.08),
    "--border": mix(bg, shade, dark ? 0.22 : 0.16),
    "--text": fg,
    "--text-dim": mix(bg, fg, 0.6),
    "--accent": t.green,
    "--danger": t.red,
  };
}

/** 应用到根元素。CSS 里的 :root 默认值只是首帧兜底。 */
export function applyChromeVars(t: Theme): void {
  const root = document.documentElement;
  for (const [k, v] of Object.entries(themeToChromeVars(t))) {
    root.style.setProperty(k, v);
  }
}
