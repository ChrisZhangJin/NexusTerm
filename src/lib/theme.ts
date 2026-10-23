import type { Theme } from "../types";

/**
 * 主题列表。字段形状对齐 Windows Terminal / iTerm2 的公开格式，
 * 便于日后直接导入社区现成主题，而不是维护私有格式。
 */
export const BUILTIN_THEMES: Record<string, Theme> = {
  "Nexus Dark": {
    name: "Nexus Dark",
    foreground: "#D6D6D6",
    background: "#181A20",
    cursor: "#5DCAA5",
    cursorAccent: "#181A20",
    selectionBackground: "#2C4048",
    black: "#181A20",
    red: "#E06C75",
    green: "#5DCAA5",
    yellow: "#E5C07B",
    blue: "#61AFEF",
    magenta: "#C678DD",
    cyan: "#56B6C2",
    white: "#D6D6D6",
    brightBlack: "#5C6370",
    brightRed: "#E06C75",
    brightGreen: "#5DCAA5",
    brightYellow: "#E5C07B",
    brightBlue: "#61AFEF",
    brightMagenta: "#C678DD",
    brightCyan: "#56B6C2",
    brightWhite: "#FFFFFF",
  },
  "Solarized Light": {
    name: "Solarized Light",
    foreground: "#657B83",
    background: "#FDF6E3",
    cursor: "#586E75",
    selectionBackground: "#EEE8D5",
    black: "#073642",
    red: "#DC322F",
    green: "#859900",
    yellow: "#B58900",
    blue: "#268BD2",
    magenta: "#D33682",
    cyan: "#2AA198",
    white: "#EEE8D5",
    brightBlack: "#002B36",
    brightRed: "#CB4B16",
    brightGreen: "#586E75",
    brightYellow: "#657B83",
    brightBlue: "#839496",
    brightMagenta: "#6C71C4",
    brightCyan: "#93A1A1",
    brightWhite: "#FDF6E3",
  },
  "Monokai": {
    name: "Monokai",
    foreground: "#F8F8F2",
    background: "#272822",
    cursor: "#F8F8F0",
    selectionBackground: "#49483E",
    black: "#272822",
    red: "#F92672",
    green: "#A6E22E",
    yellow: "#F4BF75",
    blue: "#66D9EF",
    magenta: "#AE81FF",
    cyan: "#A1EFE4",
    white: "#F8F8F2",
    brightBlack: "#75715E",
    brightRed: "#FD5FF0",
    brightGreen: "#A6E22E",
    brightYellow: "#F4BF75",
    brightBlue: "#66D9EF",
    brightMagenta: "#AE81FF",
    brightCyan: "#A1EFE4",
    brightWhite: "#F9F8F5",
  },
};

export const FONT_FAMILIES = [
  // 跨平台等宽字体。各项都带足回退链，确保在 Windows / macOS / Linux 上
  // 至少命中一款真实存在的字体 —— 否则切换「看不出变化」。
  //
  // 末尾统一追加 Liberation Mono / Noto Sans Mono / DejaVu Sans Mono：
  // 这几款在主流 Linux 发行版上几乎总是预装的。没有它们时，字体链会在
  // Windows/macOS 专属字体都缺失的 Linux 机器上落到泛型 monospace 关键字，
  // 而 WebKitGTK 的测量字体和实际渲染字体可能解析成不同的具体字体，
  // 导致终端里每个字符之间出现异常宽的间隙。
  {
    label: "Cascadia Code",
    value:
      "Cascadia Code, Cascadia Mono, Consolas, Menlo, Liberation Mono, Noto Sans Mono, DejaVu Sans Mono, monospace",
  },
  {
    label: "Consolas",
    value:
      "Consolas, Menlo, Liberation Mono, Noto Sans Mono, DejaVu Sans Mono, monospace",
  },
  {
    label: "JetBrains Mono",
    value:
      "JetBrains Mono, Consolas, Menlo, Liberation Mono, Noto Sans Mono, DejaVu Sans Mono, monospace",
  },
  {
    label: "Fira Code",
    value:
      "Fira Code, Consolas, Menlo, Liberation Mono, Noto Sans Mono, DejaVu Sans Mono, monospace",
  },
  {
    label: "Courier New",
    value:
      "Courier New, Courier, Liberation Mono, Noto Sans Mono, DejaVu Sans Mono, monospace",
  },
  {
    label: "SF Mono / Menlo",
    value:
      "SF Mono, Menlo, Consolas, Liberation Mono, Noto Sans Mono, DejaVu Sans Mono, monospace",
  },
];

export const DEFAULT_THEME = "Nexus Dark";

export function themeToXterm(t: Theme) {
  return {
    foreground: t.foreground,
    background: t.background,
    cursor: t.cursor,
    cursorAccent: t.cursorAccent,
    selectionBackground: t.selectionBackground,
    black: t.black,
    red: t.red,
    green: t.green,
    yellow: t.yellow,
    blue: t.blue,
    magenta: t.magenta,
    cyan: t.cyan,
    white: t.white,
    brightBlack: t.brightBlack,
    brightRed: t.brightRed,
    brightGreen: t.brightGreen,
    brightYellow: t.brightYellow,
    brightBlue: t.brightBlue,
    brightMagenta: t.brightMagenta,
    brightCyan: t.brightCyan,
    brightWhite: t.brightWhite,
  };
}
