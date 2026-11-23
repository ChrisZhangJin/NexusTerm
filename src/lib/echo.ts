/**
 * 这一行到底有没有被终端回显过：光标前的文字是不是正好以刚敲的那半句结尾。
 *
 * 用作**安全边界**：远端关掉回显（密码提示）时，敲的字永远不会出现在屏幕上，
 * 回车时就不该把这一行记进命令历史（否则密码会进 Shift+Tab 的历史面板）。
 * 原先是行内灰字（ghost text）的一部分；灰字已删除，只留这一个判定。
 */

import type { Terminal } from "@xterm/xterm";

/**
 * 判断「光标还贴在输入行上」时最多往回看多少个字符。
 *
 * 必须 ≥ 一整行命令的长度：判据是「光标前的文本以 `text` 的某段前缀结尾」，
 * 回显完整到达时那段前缀就是 `text` 自己，窗口比 `text` 短就永远对不上
 * 光标只在当前这一屏行上读，所以真正的上限是终端列数；256 足够宽松。
 */
const ECHO_LOOKBACK = 256;

/**
 * 光标之前的那半行，是不是正好以 `text` 结尾？
 *
 * 回显是**滞后**的：本地行状态按键瞬间就更新，屏幕上那半行要等一次网络往返，
 * 所以判据放宽成：光标前的文本结尾是 `text` 的**某一段前缀**。
 *
 * @param minMatch 至少要匹配到多长才算数（默认 1）。
 *   当**安全边界**用时（判断一行是否被回显过、要不要记进历史）必须传更大的值：
 *   1 个字符太容易巧合撞上（密码首字符恰好等于提示符结尾），会误判成「回显过」。
 */
export function cursorFollowsTyped(term: Terminal, text: string, minMatch = 1): boolean {
  if (!text) return false;
  const buf = term.buffer.active;
  // 与 termRegistry.textBeforeCursor 用同一个绝对行号换算，别在这里另起一套
  const line = buf.getLine(buf.baseY + buf.cursorY);
  if (!line) return false;

  // 只往回读需要的长度：从光标倒着读比把整行摊平成字符串再 endsWith 便宜
  const need = Math.min(text.length, ECHO_LOOKBACK);
  let seen = "";
  for (let x = buf.cursorX - 1; x >= 0 && seen.length < need; x--) {
    const ch = line.getCell(x)?.getChars();
    if (ch) seen = ch + seen;
  }

  const floor = Math.max(1, Math.min(minMatch, need));
  for (let k = Math.min(need, seen.length); k >= floor; k--) {
    if (seen.endsWith(text.slice(0, k))) return true;
  }
  return false;
}
