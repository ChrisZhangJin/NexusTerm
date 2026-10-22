/**
 * 行内灰字（ghost text）：把「补全建议」直接画在光标后面，Tab 接受。
 *
 * ## 为什么不用覆盖层
 *
 * 灰字必须**逐格对齐**在光标之后，而 xterm 的字符网格里，CJK / emoji 占两格。
 * 用 DOM 覆盖层去对齐就得反解终端的字体度量（那是一堆私有 API，版本一动就歪）。
 * 往终端里写一段带暗色属性的文本再让光标退回去，这个代价要小得多，
 * 而且天然支持宽字符、换行与滚动 —— 这些都是终端自己算好的。
 *
 * ## 唯一的危险
 *
 * 写进 terminal 的字符是**显示缓冲**里的字节，不会发到 pty，
 * 所以灰字永远不会被误当成输入执行。但只要远端又吐了新输出，
 * 那段灰字就会变成"真实"文本赖在屏幕上。因此调用方必须在
 * **任何一次 `term.write(远端数据)` 之前**先 `clear()`。
 */

import type { Terminal } from "@xterm/xterm";
import { cellWidth } from "./termRegistry";

/** ESC 与 CSI 引导序列。用码点拼出来，避免源码里的转义被工具链二次处理。 */
const ESC = String.fromCharCode(27);
const CSI = ESC + "[";

/** 灰字最多占多少列。长命令补全只显示能塞下的部分。 */
const MAX_GHOST_COLS = 80;

/**
 * 光标之前的那半行，是不是正好以 `text` 结尾？
 *
 * 用途：pty 会把人敲的每个字符回显回来，回显一到就得擦掉灰字再重画。
 * 但**别的输出**（命令结果、后台任务）也会走同一条路 —— 那种时候光标早就
 * 不在「刚敲完的那半句」后面了，重画就会把灰字甩到一坨输出中间。
 * 所以在写之前先问一句：光标还贴着我们记下的那半行吗？
 */
export function cursorFollowsTyped(term: Terminal, text: string): boolean {
  if (!text) return false;
  const buf = term.buffer.active;
  // 光标那一行的绝对下标。与 termRegistry.textBeforeCursor 用的是同一个式子 ——
  // 这个换算在登录脚本录制 / 还原命令里已经跑了很多次，别在这里另起一套。
  const line = buf.getLine(buf.baseY + buf.cursorY);
  if (!line) return false;

  let seen = "";
  for (let x = 0; x < buf.cursorX; x++) {
    const ch = line.getCell(x)?.getChars();
    if (ch) seen += ch;
  }
  return seen.endsWith(text);
}

export class GhostText {
  /** 当前画在屏幕上的灰字（用于计算光标回退量） */
  private shown = "";
  /** 灰字占了多少**列**。CJK / emoji 占两格，不能用字符数替代 */
  private shownCols = 0;

  get text(): string {
    return this.shown;
  }

  get active(): boolean {
    return this.shown.length > 0;
  }

  /** 灰字占了多少列（用于自测：必须等于 `CSI n D` 里退回去的格数） */
  get cols(): number {
    return this.shownCols;
  }

  /** 擦掉屏幕上的灰字。光标此刻停在它的开头，`EL` 从光标擦到行尾正合适。 */
  clear(term: Terminal): void {
    if (!this.shown) return;
    term.write(CSI + "K");
    this.shown = "";
    this.shownCols = 0;
  }

  /**
   * 在光标后画出灰字并把光标退回原处。
   *
   * 只在「整段都放得下」时才画：灰字一旦换行，屏幕行数会变、shell 的回显位置
   * 也会跟着动，那点提示的价值远不如它带来的混乱。
   *
   * @param room 光标之后还剩多少列（由调用方按 cols 算好）
   * @returns 真正画出来的那部分文本
   */
  show(term: Terminal, suggestion: string, room: number): string {
    this.clear(term);
    if (!suggestion || room <= 1) return "";

    let budget = Math.min(room - 1, MAX_GHOST_COLS);
    let used = "";
    let cols = 0;
    for (const ch of suggestion) {
      const w = cellWidth(ch);
      if (w > budget) break;
      budget -= w;
      used += ch;
      cols += w;
    }
    if (!used) return "";

    // 先切暗色 → 写字 → 关暗色 → 光标左移回原位。
    // 顺序不能反：属性码本身不占列，但写文本会把光标推走，
    // 不退回来的话下一次按键就会落到灰字后面。
    // 关暗色用 22m 而不是 0m：0m 会把前景/背景一起重置，
    // 于是在彩色提示符后面画过一次灰字，提示符就掉色了。
    // 回退格数用列数而非字符数 —— 一个中文字要退两格。
    term.write(`${CSI}2m${used}${CSI}22m${CSI}${cols}D`);
    this.shown = used;
    this.shownCols = cols;
    return used;
  }
}
