/**
 * Shift+Enter 的「聪明开关」。
 *
 * 现象：xterm.js 5.5 对 Enter 与 Shift+Enter 都只发 `\r`（只有 Alt+Enter 会发 `ESC \r`），
 * 于是在 Claude Code、nanopi 这类 Ink 写的 TUI 里，Shift+Enter 本想换行却直接提交了。
 * 这些程序都把 `ESC \r`（即 Alt/Meta+Enter）当作「插入换行」。
 *
 * 但 `ESC \r` 不能无条件发：停在 shell 提示符上时，bash/readline 会把 `M-C-m` 解释成
 * `vi-editing-mode`（悄悄切成 vi 模式），zsh 则是往命令行里塞一个字面换行 —— 都比
 * 今天「当回车用」更糟。所以：
 *
 * - 光标所在的逻辑行**确实**像 shell 提示符（且光标可见）→ 保持原样，当回车；
 * - 备用屏且光标可见（vim / less / htop）→ 也保持原样：`ESC` 会让 vim 退出插入模式；
 * - 其余（光标被关掉的 Ink 程序、普通屏上判断不了的）→ 发 `ESC \r`。详见 `wantsMetaEnter`。
 *
 * 为什么不用 `lineRef.known`：敲完 `claude⏎` 行状态就被重置成「已知的空行」，
 * Claude Code 跑起来后它一直是 true，分不出 shell 和前台 TUI。
 * 为什么不只看备用屏：Claude Code / nanopi 都跑在普通屏幕上；反过来 tmux 在备用屏里，
 * 里面可能是 shell、也可能是 Claude Code —— 所以提示符检测与「光标是否被关」对两种屏幕都做。
 *
 * 提示符的判定移植自后端 `agent/prompt.rs` 的 `shell_confidence`（只取高置信度档）。
 * 认错的代价不对称：把 TUI 当成 shell → 退回今天的行为（提交）；把 shell 当成 TUI →
 * bash 被切进 vi 模式。所以识别偏宽，但 Ink 程序的输入框前缀（`> `、`│ > `）必须不中。
 *
 * 全是纯函数；缓冲用 `lib/termLine` 的结构类型，`term.buffer.active` 直接满足。
 */

import type { BufLike } from "./termLine";

/** Shift+Enter 要发的字节：Meta+Enter */
export const SHIFT_ENTER_SEQ = "\x1b\r";

/** 一段文字（已 trim）像不像 shell 提示符。逻辑与后端 `shell_confidence` 的 ≥0.7 档一致。 */
export function looksLikeShellPrompt(raw: string): boolean {
  const t = raw.trim();
  if (!t || [...t].length > 200) return false;
  // PowerShell / cmd.exe
  if (/^PS(?:\s.*)?>$/.test(t) && !t.includes("PS>>")) return true;
  if (/^[A-Za-z]:\\[^<>"|?*]*>$/.test(t)) return true;
  // 箭头类：starship / pure / oh-my-zsh
  if (t.endsWith("❯") || t.endsWith("λ") || t.endsWith("»")) return true;
  if (t.startsWith("➜")) return true;
  // fish：`user@host ~>`
  if (t.endsWith(">") && t.includes("@") && !t.includes("<") && !t.includes(" >")) return true;

  const last = t[t.length - 1];
  if (last !== "$" && last !== "#" && last !== "%") return false;
  const body = t.slice(0, -1);
  if (!body.trim()) return true; // 裸 `$` / `#` / `%`
  const prev = body[body.length - 1];
  // 进度条、百分比、`$$`
  if (last === "%" && /\d/.test(prev)) return false;
  if (last === "#" && (prev === "#" || prev === "=" || prev === "-")) return false;
  if (last === "$" && prev === "$") return false;

  const hasAt = body.includes("@");
  if (hasAt && /[:~/ \]]/.test(body)) return true; // user@host:~$
  if (/^\[.*\]\s?$/.test(body)) return true; // [user@host ~]$
  if (/^-?(?:bash|sh|zsh|ksh|dash|ash|fish|csh|tcsh)[-\d.]*$/.test(body)) return true; // bash-5.1$
  if (/^[~/]\S*\s$/.test(body)) return true; // `/ # ` `~/p %`
  if ("~/])─".includes(prev)) return true;
  if (hasAt && last === "$") return true;
  return false;
}

/**
 * 光标前这段逻辑行里，有没有一个「提示符」前缀。
 *
 * 行可能已经敲了命令（还可能是 ↑ 翻出来的，我们不知道命令从哪儿开始），所以挨个
 * 试「到某个空格为止」的前缀，最短命中即算；整段（光标前没有命令）也试一次。
 * 命令里出现 `$ ` / `# ` 并不会误中：前缀还得整体像提示符（`echo a #` 不像）。
 */
export function lineHasShellPrompt(before: string): boolean {
  const s = before.replace(/\s+$/, "");
  if (!s) return false;
  for (let i = 0; i < s.length; i++) {
    if (s[i] === " " && i > 0 && s[i - 1] !== " " && looksLikeShellPrompt(s.slice(0, i))) return true;
  }
  return looksLikeShellPrompt(s);
}

/** 光标所在逻辑行（顺着自动折行往上接）在光标之前的文字；读不到返回空串 */
export function textBeforeCursor(buf: BufLike, maxRows = 20): string {
  const cy = buf.baseY + buf.cursorY;
  let y = cy;
  while (y > 0 && cy - y < maxRows && buf.getLine(y)?.isWrapped) y--;
  let out = "";
  for (let r = y; r <= cy; r++) {
    const line = buf.getLine(r);
    if (!line) return "";
    out += r === cy ? line.translateToString(false, 0, buf.cursorX) : line.translateToString(false);
  }
  return out;
}

export interface ShiftEnterCtx {
  /** 光标前那段逻辑行 */
  before: string;
  /** DECTCEM 关了光标（Ink 程序都会关，shell 不会）；拿不到传 false */
  cursorHidden: boolean;
  /** 当前是备用屏（vim / less / htop / tmux） */
  altScreen: boolean;
}

/** 是否「NexusTerm 认为终端正停在 shell 提示符」 */
export function atShellPrompt(ctx: ShiftEnterCtx): boolean {
  if (ctx.cursorHidden) return false;
  return lineHasShellPrompt(ctx.before);
}

/**
 * 屏幕状态是否要求把 Shift+Enter 改成 `ESC \r`：
 *
 * - shell 提示符（两种屏幕都算，tmux 里的 shell 也是）→ 否，当回车（含灰字路径）；
 * - 光标被关掉（Ink 程序：Claude Code / nanopi，tmux 里跑也一样）→ 是；
 * - 备用屏 + 光标可见（vim 插入模式、less、htop、tmux 里的 shell）→ 否：
 *   `ESC` 会让 vim 退出插入模式，保持今天的 `\r`；
 * - 普通屏 + 光标可见 + 不像提示符（保留光标的行编辑类 TUI）→ 是。
 */
export function wantsMetaEnter(ctx: ShiftEnterCtx): boolean {
  if (atShellPrompt(ctx)) return false;
  return ctx.cursorHidden || !ctx.altScreen;
}

/** 参与判定的修饰键与输入法状态 */
export interface ShiftEnterKey {
  key: string;
  shiftKey: boolean;
  ctrlKey: boolean;
  altKey: boolean;
  metaKey: boolean;
  /** 输入法占着这个键（`ImeGuard.captures`） */
  imeBusy: boolean;
}

/** 这一下是不是要改发 `ESC \r` 的 Shift+Enter。`screenWants` 惰性求值（通常是 `wantsMetaEnter`）：不是 Shift+Enter 就不读屏 */
export function shouldSendMetaEnter(k: ShiftEnterKey, screenWants: () => boolean): boolean {
  if (k.key !== "Enter" || !k.shiftKey) return false;
  if (k.ctrlKey || k.altKey || k.metaKey) return false;
  if (k.imeBusy) return false;
  return screenWants();
}
