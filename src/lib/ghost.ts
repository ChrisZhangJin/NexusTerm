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
 * 判断「光标还贴在输入行上」时，最多往回看多少个字符。
 *
 * 回显经过一次网络往返才到，本地行状态会短暂领先；领先几个字符是常态，
 * 领先几十个字符说明人已经不是在「打字」而是在粘贴/狂奔了 ——
 * 那种场景下不显示灰字也不影响什么。
 */
const ECHO_LOOKBACK = 32;

/**
 * 光标之前的那半行，是不是正好以 `text` 结尾？
 *
 * 用途：pty 会把人敲的每个字符回显回来，回显一到就得擦掉灰字再重画。
 * 但**别的输出**（命令结果、后台任务）也会走同一条路 —— 那种时候光标早就
 * 不在「刚敲完的那半句」后面了，重画就会把灰字甩到一坨输出中间。
 * 所以在写之前先问一句：光标还贴着我们记下的那半行吗？
 *
 * ## 为什么不能只比「整行以 text 结尾」
 *
 * 回显是**滞后**的：本地的行状态在按键的瞬间就更新了，而屏幕上那半行要等
 * 一次网络往返才补齐。按「必须整段匹配」来判断，敲每一个字都会得到「还没到齐」，
 * 于是灰字在「按键画上 / 回显擦掉」之间来回闪 —— 这既是视觉上的抖动，
 * 也是每个字符两次整行重绘的来源。
 *
 * 所以判据放宽成：光标前的文本结尾是 `text` 的**某一段前缀**。
 * 屏幕上显示的是我们敲过的东西的某一段，就说明这一行还是输入行。
 *
 * @param minMatch 至少要匹配到多长才算数（默认 1，贴合灰字重画「宁可信
 *   其有」的用法）。调用方若拿这个判据当**安全边界**用（比如判断一行是不是
 *   被回显过、要不要记进历史），1 个字符的巧合匹配太容易撞上——密码的第一个
 *   字符恰好和提示符结尾、或者上一行残留的字符相同，就会被误判成「回显过」。
 *   这种场景应该传一个更大的下限。
 */
export function cursorFollowsTyped(term: Terminal, text: string, minMatch = 1): boolean {
  if (!text) return false;
  const buf = term.buffer.active;
  // 光标那一行的绝对下标。与 termRegistry.textBeforeCursor 用的是同一个式子 ——
  // 这个换算在登录脚本录制 / 还原命令里已经跑了很多次，别在这里另起一套。
  const line = buf.getLine(buf.baseY + buf.cursorY);
  if (!line) return false;

  // 只往回读到需要的长度就停：这一行可能很长，而回显落后最多几个字符。
  // 从光标往前读，比把整行摊平成字符串再 endsWith 便宜。
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

/**
 * 光标是不是停在这一行**内容的末尾**（它后面没有别的字）。
 *
 * 灰字画在光标之后，擦除也从光标起算。所以只要光标后面还留着真实字符
 * —— 人在行中间删改（按了 ← / Backspace / Home）、或者 shell 把整行重绘过
 * —— 画灰字、擦灰字都会把那半截真字一起抹掉。
 *
 * 抹掉只是**显示缓冲**上的事，pty 里的行缓冲一点没动，于是屏幕上看着没了、
 * 回车一按那些字又原样冒出来：这就是「回车后消失的部分还在」的由来。
 */
export function cursorAtLineEnd(term: Terminal): boolean {
  const buf = term.buffer.active;
  const line = buf.getLine(buf.baseY + buf.cursorY);
  if (!line) return false;
  for (let x = buf.cursorX; x < term.cols; x++) {
    const ch = line.getCell(x)?.getChars();
    // 没写过的位置是 ""，宽字符的后半格也是 ""；行尾的空格不算「有内容」
    if (ch && ch !== " " && ch !== "\u0000") return false;
  }
  return true;
}

export class GhostText {
  /** 当前画在屏幕上的灰字（用于计算光标回退量） */
  private shown = "";
  /** 灰字占了多少**列**。CJK / emoji 占两格，不能用字符数替代 */
  private shownCols = 0;
  /** 画灰字时光标在哪（用于判断还能不能照原样擦掉，见 `clear`） */
  private atX = -1;
  private atY = -1;

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

  /**
   * 擦掉屏幕上的灰字。
   *
   * 两个约束，缺一个都会删掉用户敲的字：
   * - **只擦自己画的那几列**：用 `ECH`（`CSI n X`，擦光标起的 n 个字符）
   *   而不是 `EL`（`CSI K`，擦到行尾）。光标后面万一还有真字，`EL` 会一路抹到行尾。
   * - **光标必须还在原地**：光标被挪走之后（shell 重绘了整行、输出把光标顶走），
   *   从当前光标往后那一段已经不是我们画的了，这时候一个字节都不能写。
   */
  clear(term: Terminal): void {
    if (!this.shown) return;
    const buf = term.buffer.active;
    const sameSpot = this.atX === buf.cursorX && this.atY === buf.baseY + buf.cursorY;
    if (sameSpot && this.shownCols > 0) {
      term.write(CSI + this.shownCols + "X");
    }
    this.shown = "";
    this.shownCols = 0;
    this.atX = -1;
    this.atY = -1;
  }

  /**
   * 在光标后画出灰字并把光标退回原处。
   *
   * 只在「整段都放得下」时才画：灰字一旦换行，屏幕行数会变、shell 的回显位置
   * 也会跟着动，那点提示的价值远不如它带来的混乱。
   *
   * **幂等**：屏幕上已经是这段字就直接返回，不再写一遍。调用方在每次回显、
   * 每次按键后都会来问一次「现在该显示什么」，绝大多数时候答案是「和现在一样」——
   * 那种情况下重画一遍纯属白烧一帧渲染，还会让整行闪一下。
   *
   * @param room 光标之后还剩多少列（由调用方按 cols 算好）
   * @returns 真正画出来的那部分文本
   */
  show(term: Terminal, suggestion: string, room: number): string {
    const { text: used, cols } = this.fit(suggestion, room);
    // 屏幕上已经是这一段：一个字节都不用写，也不去问 `cursorAtLineEnd`。
    // 打字时每一次按键、每一次回显都会来问一遍，而绝大多数时候答案是「没变」——
    // 重画一遍既白烧一帧渲染，又会让整行闪一下。
    //
    // 这个判断必须排在 `cursorAtLineEnd` **之前**：灰字自己就是画在光标后面的
    // 真实字符，它还在屏幕上的时候 `cursorAtLineEnd` 永远会判成「光标后面还有
    // 别的字」。顺序反了的话，只要灰字已经画出来过，这条「没变就不动」的快路径
    // 永远走不到——每次调用都先被当成「在行中间」擦掉、再也画不回来。
    if (used === this.shown) return this.shown;
    // 换内容之前先擦掉旧的：既是为了不留残影，也是让下面的 `cursorAtLineEnd`
    // 看到的是真实内容，不会把我们自己上一轮画的灰字误判成「别的字」。
    this.clear(term);
    // 光标后面还有真字：一个字节都别写（理由见 `cursorAtLineEnd`）。
    if (!cursorAtLineEnd(term)) return "";
    if (!used) return "";

    const buf = term.buffer.active;
    this.atX = buf.cursorX;
    this.atY = buf.baseY + buf.cursorY;

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

  /**
   * 按剩余列数裁出真正放得下的那一段（CJK / emoji 占两格）。
   *
   * 一次遍历同时算文本与列宽：`show` 在热路径上（每次按键、每次回显），
   * 分两趟扫同一个字符串没有必要。
   */
  private fit(suggestion: string, room: number): { text: string; cols: number } {
    if (!suggestion || room <= 1) return { text: "", cols: 0 };
    let budget = Math.min(room - 1, MAX_GHOST_COLS);
    let text = "";
    let cols = 0;
    for (const ch of suggestion) {
      const w = cellWidth(ch);
      if (w > budget) break;
      budget -= w;
      text += ch;
      cols += w;
    }
    return { text, cols };
  }
}
