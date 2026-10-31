/**
 * 行内灰字（ghost text）：把「补全建议」直接画在光标后面，Tab 接受。
 *
 * ## 为什么不用 DOM 覆盖层
 *
 * 灰字要**逐格对齐**在光标之后，而 CJK / emoji 在 xterm 网格里占两格；覆盖层
 * 就得反解终端字体度量（私有 API，版本一动就歪）。写一段暗色文本再把光标退
 * 回去代价小得多，宽字符、换行与滚动都由终端自己算好。
 *
 * ## 唯一的危险
 *
 * 灰字只进**显示缓冲**、不发到 pty，所以不会被当成输入执行；但远端一吐新输出，
 * 那段灰字就变成"真实"文本赖在屏幕上。调用方必须在**任何一次
 * `term.write(远端数据)` 之前**先 `clear()`。
 */

import type { Terminal } from "@xterm/xterm";
import { cellWidth } from "./termRegistry";

/** ESC 与 CSI 引导序列。用码点拼出来，避免源码里的转义被工具链二次处理。 */
const ESC = String.fromCharCode(27);
const CSI = ESC + "[";

/** 灰字最多占多少列。长命令补全只显示能塞下的部分。 */
const MAX_GHOST_COLS = 80;

/**
 * 判断「光标还贴在输入行上」时最多往回看多少个字符。
 *
 * 必须 ≥ 一整行命令的长度：判据是「光标前的文本以 `text` 的某段前缀结尾」，
 * 回显完整到达时那段前缀就是 `text` 自己，窗口比 `text` 短就永远对不上
 * （曾经是 32：超过 32 个字符的命令，回显一到灰字就被判「输入已结束」收掉）。
 * 光标只在当前这一屏行上读，所以真正的上限是终端列数；256 足够宽松。
 */
const ECHO_LOOKBACK = 256;

/**
 * 光标之前的那半行，是不是正好以 `text` 结尾？
 *
 * 用途：回显一到就得擦掉灰字再重画，但**别的输出**（命令结果、后台任务）走
 * 同一条路 —— 那时光标早就不在「刚敲完的那半句」后面，重画会把灰字甩到一坨
 * 输出中间。
 *
 * ## 为什么不能只比「整行以 text 结尾」
 *
 * 回显是**滞后**的：本地行状态按键瞬间就更新，屏幕上那半行要等一次网络往返。
 * 要求整段匹配的话每敲一个字都判「还没到齐」，灰字就在「按键画上 / 回显擦掉」
 * 之间来回闪，还带来每字符两次整行重绘。故判据放宽成：光标前的文本结尾是
 * `text` 的**某一段前缀**。
 *
 * @param minMatch 至少要匹配到多长才算数（默认 1，贴合灰字重画「宁可信其有」）。
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

/**
 * 光标是不是停在这一行**内容的末尾**（它后面没有别的字）。
 *
 * 灰字画在光标之后、擦除也从光标起算，所以光标后面还留着真实字符时（人在行
 * 中间删改、或 shell 重绘了整行）画/擦都会把那半截真字一起抹掉。而抹掉只是
 * **显示缓冲**的事，pty 行缓冲没动 —— 这就是「屏幕上看着没了、回车一按又原样
 * 冒出来」的由来。
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

/**
 * 逐格核对绝对行 `y`、列 `x` 起的那段缓冲，是不是正好是我们画的灰字。
 * 宽字符占两格（后半格 getChars 为空），按 `cellWidth` 走位比对。
 * 灰字从不进 pty，shell 输出不可能在原位恰好排出同一段字 —— 全文逐格
 * 相同就足以认定是我们的，不必再验前后界。
 */
function ghostCellsMatch(
  term: Terminal,
  y: number,
  x: number,
  text: string,
  cols: number
): boolean {
  if (cols <= 0) return false;
  const line = term.buffer.active.getLine(y);
  if (!line) return false;
  let cursor = x;
  for (const ch of text) {
    const cell = line.getCell(cursor);
    if (!cell || cell.getChars() !== ch) return false;
    cursor += cellWidth(ch);
  }
  return true;
}

/**
 * `clear()` 的结果。调用方**必须**用它而不是紧接着再读一遍缓冲：
 * xterm 的 `write()` 是异步的（见下），刚写出去的擦除序列此刻还没进缓冲，
 * 再读 `cursorAtLineEnd` 看到的是自己刚要擦掉的灰字 —— 「按 Tab 补全失灵」
 * 就是这么来的。
 */
export type GhostClear =
  /** 本来就没画灰字，什么都没写 */
  | "idle"
  /** 光标仍在画灰字时的位置：光标之后那一段全是我们的灰字，擦了就是行尾 */
  | "erased"
  /** 光标已被挪走，但旧位置核实过确实是我们画的灰字，已原地擦干净 */
  | "cleaned"
  /** 光标已被挪走，旧位置的内容也认不出是我们画的：一个字节都没写，屏幕状态不明 */
  | "moved";

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

  /**
   * 光标是否还停在画灰字时的那个位置。
   *
   * 判「还能不能按当初的前提继续操作」只看**坐标**，绝不读缓冲 —— 灰字自己就是
   * 光标后面的真实字符，凡是「光标后面有没有字」的缓冲扫描（`cursorAtLineEnd`）
   * 在灰字画着的时候永远判「有」，拿来当守卫就会把刚画好的灰字当场收掉
   * （Tab 因此掉回给 shell，表现为「部分场景下 Tab 补不了」）。
   */
  atMark(term: Terminal): boolean {
    if (!this.active) return false;
    const buf = term.buffer.active;
    return this.atX === buf.cursorX && this.atY === buf.baseY + buf.cursorY;
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
  clear(term: Terminal): GhostClear {
    if (!this.shown) return "idle";
    const buf = term.buffer.active;
    const sameSpot = this.atX === buf.cursorX && this.atY === buf.baseY + buf.cursorY;
    const cols = this.shownCols;
    const text = this.shown;
    const oldX = this.atX;
    const oldY = this.atY;
    this.shown = "";
    this.shownCols = 0;
    this.atX = -1;
    this.atY = -1;
    if (sameSpot) {
      term.write(CSI + cols + "X");
      // 画灰字的前提就是「当时光标后没有真字」，光标又没动过，
      // 所以那一段（乃至到行尾）仍然是空的 —— 不用读缓冲就敢下这个结论
      return "erased";
    }
    // 光标已被挪走。但不能就此撒手：状态一清，调用方就当「屏幕干净」处理，
    // Tab 会掉回给 shell —— 而灰字还赖在原地（截图里那截擦不掉的残影）。
    // 逐格核对旧位置：确实还是我们画的那段字，就跳过去原地擦掉、再把光标送回
    // 现在的位置。认不出（被输出盖掉了/滚屏了）才是真正的"moved"，不敢写。
    if (ghostCellsMatch(term, oldY, oldX, text, cols)) {
      const row = oldY - buf.baseY; // CUP 用视口行号；滚出视口时上面已判 false
      if (row >= 0 && row < term.rows) {
        const restore = `${CSI}${buf.cursorY + 1};${buf.cursorX + 1}H`;
        term.write(`${CSI}${row + 1};${oldX + 1}H${CSI}${cols}X${restore}`);
        return "cleaned";
      }
    }
    return "moved";
  }

  /**
   * 在光标后画出灰字并把光标退回原处。
   *
   * 只在「整段都放得下」时才画：灰字一旦换行，屏幕行数会变、shell 的回显位置
   * 也会跟着动，那点提示的价值远不如它带来的混乱。
   *
   * **幂等**：屏幕上已经是这段字就直接返回。调用方每次按键 / 回显都会问一遍，
   * 多数时候答案没变，重画纯属白烧一帧渲染，还会让整行闪一下。
   *
   * @param room 光标之后还剩多少列（由调用方按 cols 算好）
   * @returns 真正画出来的那部分文本
   */
  show(term: Terminal, suggestion: string, room: number): string {
    const { text: used, cols } = this.fit(suggestion, room);
    // 这个快路径必须排在 `cursorAtLineEnd` **之前**：灰字自己就是光标后面的真实
    // 字符，它还在屏幕上时 `cursorAtLineEnd` 永远判「后面还有别的字」。顺序反了
    // 的话灰字一旦画出来过就再也画不回来（每次都被当成「在行中间」擦掉）。
    if (used === this.shown) return this.shown;
    // 先擦旧的：既不留残影，也让下面的 `cursorAtLineEnd` 看到的是真实内容。
    // 擦除是异步进缓冲的（xterm write 的性质），所以「erased」时不能再读缓冲
    // 判行尾 —— 那只会读到刚要擦掉的旧灰字；「erased」本身就说明是行尾。
    const cleared = this.clear(term);
    if (cleared !== "erased" && !cursorAtLineEnd(term)) return "";
    if (!used) return "";

    const buf = term.buffer.active;
    this.atX = buf.cursorX;
    this.atY = buf.baseY + buf.cursorY;

    // 关暗色用 22m 而不是 0m：0m 连前景/背景一起重置，在彩色提示符后面画过一次
    // 灰字，提示符就掉色了。回退格数用列数而非字符数 —— 一个中文字要退两格。
    term.write(`${CSI}2m${used}${CSI}22m${CSI}${cols}D`);
    this.shown = used;
    this.shownCols = cols;
    return used;
  }

  /**
   * 按剩余列数裁出真正放得下的那一段（CJK / emoji 占两格）。
   * 一次遍历同时算文本与列宽 —— `show` 在热路径上，不值得扫两趟。
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
