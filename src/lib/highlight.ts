import type { IDecoration, Terminal } from "@xterm/xterm";

/**
 * 终端关键字「标记」：把整个缓冲区（含滚动历史）里命中的文本用指定底色高亮。
 *
 * 实现要点：
 * - 按「逻辑行」匹配：被自动折行（isWrapped）的物理行先拼回一整行，跨行的关键字也能命中
 * - 先用 translateToString 粗筛，只有命中的逻辑行才逐格（getCell）建立「字符 → 列」映射，
 *   CJK / emoji 宽字符占 2 列，直接用字符串下标当列号会错位
 * - 用 registerMarker + registerDecoration 画底色；marker 随滚动历史裁剪自动释放
 */

export interface MarkRule {
  /** #RRGGBB（xterm 装饰只认这种格式） */
  color: string;
  text: string;
}

export const MARK_COLORS = [
  { key: "yellow", label: "高亮黄", color: "#ffd84d" },
  { key: "green", label: "高亮绿", color: "#6fdc8c" },
  { key: "blue", label: "高亮蓝", color: "#6cb6ff" },
] as const;

/** 防止超长缓冲区里命中过多拖垮渲染 */
const MAX_DECORATIONS = 3000;

interface CellRef {
  row: number; // 缓冲区绝对行号
  col: number;
  width: number;
}

/** 逻辑行起止物理行 */
function logicalLines(term: Terminal): Array<[number, number]> {
  const buf = term.buffer.active;
  const out: Array<[number, number]> = [];
  let start = 0;
  for (let y = 1; y <= buf.length; y++) {
    const next = y < buf.length ? buf.getLine(y) : undefined;
    if (!next || !next.isWrapped) {
      out.push([start, y - 1]);
      start = y;
    }
  }
  return out;
}

/** 逐格展开一段逻辑行：返回文本，以及每个 UTF-16 码元对应的单元格 */
function expand(term: Terminal, from: number, to: number): { text: string; map: CellRef[] } {
  const buf = term.buffer.active;
  let text = "";
  const map: CellRef[] = [];
  const cell = buf.getNullCell();
  for (let y = from; y <= to; y++) {
    const line = buf.getLine(y);
    if (!line) continue;
    for (let x = 0; x < line.length; x++) {
      line.getCell(x, cell);
      const w = cell.getWidth();
      if (w === 0) continue; // 宽字符的右半格
      const ch = cell.getChars() || " ";
      for (let k = 0; k < ch.length; k++) map.push({ row: y, col: x, width: w });
      text += ch;
    }
  }
  return { text, map };
}

function roughText(term: Terminal, from: number, to: number): string {
  const buf = term.buffer.active;
  let s = "";
  for (let y = from; y <= to; y++) s += buf.getLine(y)?.translateToString(y === to) ?? "";
  return s;
}

/**
 * 每个终端一个高亮器：apply(rules) 设定规则并立即重绘，refresh() 在有新输出/尺寸变化后重扫。
 */
export class Highlighter {
  private decos: IDecoration[] = [];
  private rules: MarkRule[] = [];
  private caseSensitive = false;
  private timer: number | undefined;

  constructor(private term: Terminal) {}

  get active(): MarkRule[] {
    return this.rules;
  }

  get isCaseSensitive(): boolean {
    return this.caseSensitive;
  }

  /** 设置规则并立即扫描，返回命中数 */
  apply(rules: MarkRule[], caseSensitive: boolean): number {
    this.rules = rules.filter((r) => r.text.length > 0);
    this.caseSensitive = caseSensitive;
    return this.scan();
  }

  clear(): void {
    this.rules = [];
    this.dispose();
  }

  /** 输出 / 重排后的去抖重扫（无规则时零开销） */
  refresh(): void {
    if (this.rules.length === 0) return;
    if (this.timer) window.clearTimeout(this.timer);
    this.timer = window.setTimeout(() => this.scan(), 250);
  }

  dispose(): void {
    if (this.timer) window.clearTimeout(this.timer);
    for (const d of this.decos) d.dispose();
    this.decos = [];
  }

  private scan(): number {
    for (const d of this.decos) d.dispose();
    this.decos = [];
    if (this.rules.length === 0) return 0;

    const term = this.term;
    const buf = term.buffer.active;
    // 备用屏（vim / less / tmux 全屏）不支持装饰，等回到主屏再画
    if (buf.type !== "normal") return 0;

    const norm = (s: string) => (this.caseSensitive ? s : s.toLowerCase());
    const needles = this.rules.map((r) => ({ ...r, n: norm(r.text) }));
    const cursorAbs = buf.baseY + buf.cursorY;
    let hits = 0;

    for (const [from, to] of logicalLines(term)) {
      const rough = norm(roughText(term, from, to));
      if (!needles.some((r) => rough.includes(r.n))) continue;

      const { text, map } = expand(term, from, to);
      const hay = norm(text);
      for (const r of needles) {
        let i = hay.indexOf(r.n);
        while (i >= 0) {
          hits++;
          if (this.decos.length < MAX_DECORATIONS) this.paint(map, i, r.n.length, r.color, cursorAbs);
          i = hay.indexOf(r.n, i + Math.max(1, r.n.length));
        }
      }
    }
    return hits;
  }

  /** 把 [start, start+len) 的码元区间按物理行切段画底色 */
  private paint(map: CellRef[], start: number, len: number, color: string, cursorAbs: number) {
    const segs = new Map<number, { x0: number; x1: number }>();
    for (let k = start; k < start + len && k < map.length; k++) {
      const c = map[k];
      const s = segs.get(c.row);
      const end = c.col + c.width;
      if (!s) segs.set(c.row, { x0: c.col, x1: end });
      else {
        s.x0 = Math.min(s.x0, c.col);
        s.x1 = Math.max(s.x1, end);
      }
    }
    for (const [row, { x0, x1 }] of segs) {
      const marker = this.term.registerMarker(row - cursorAbs);
      if (!marker) continue;
      const deco = this.term.registerDecoration({
        marker,
        x: x0,
        width: x1 - x0,
        backgroundColor: color,
        foregroundColor: "#111111",
        layer: "top",
      });
      if (deco) {
        deco.onDispose(() => marker.dispose());
        this.decos.push(deco);
      } else {
        marker.dispose();
      }
    }
  }
}

/** 导出整个主缓冲区为纯文本：折行拼回、行尾空白去掉、末尾空行收掉 */
export function bufferText(term: Terminal): string {
  const buf = term.buffer.normal;
  const lines: string[] = [];
  let cur = "";
  for (let y = 0; y < buf.length; y++) {
    const line = buf.getLine(y);
    if (!line) continue;
    const next = buf.getLine(y + 1);
    const continues = !!next?.isWrapped;
    cur += line.translateToString(!continues);
    if (!continues) {
      lines.push(cur);
      cur = "";
    }
  }
  if (cur) lines.push(cur);
  while (lines.length && lines[lines.length - 1].trim() === "") lines.pop();
  return lines.join("\n") + "\n";
}
