import type { Terminal } from "@xterm/xterm";

/**
 * tabId → xterm 实例。
 * App 层需要读终端状态（光标列、当前行是否是提示符）来决定何时、以何种方式注入还原命令，
 * 但 Terminal 由 TerminalView 持有 —— 这里做一个只读登记表，避免把实例层层往上传。
 */
const registry = new Map<string, Terminal>();
/** 最近一次收到远端输出的时间（判断「输出已静默」用） */
const lastOutput = new Map<string, number>();

export function registerTerm(id: string, term: Terminal): () => void {
  registry.set(id, term);
  return () => {
    if (registry.get(id) === term) {
      registry.delete(id);
      lastOutput.delete(id);
      outputCount.delete(id);
      inputTaps.delete(id);
    }
  };
}

export function markOutput(id: string): void {
  lastOutput.set(id, Date.now());
}

/** 每个标签累计收到的输出量（字符数按 UTF-8 解码后计），状态栏展示、导出前参考大小 */
interface OutputCount {
  chars: number;
  bytes: number;
  decoder: TextDecoder;
}
const outputCount = new Map<string, OutputCount>();

export function countOutput(id: string, bytes: Uint8Array): void {
  let c = outputCount.get(id);
  if (!c) {
    c = { chars: 0, bytes: 0, decoder: new TextDecoder("utf-8") };
    outputCount.set(id, c);
  }
  c.bytes += bytes.length;
  // stream: true —— 多字节字符被拆在两个包之间时不会算成两个乱码
  const s = c.decoder.decode(bytes, { stream: true });
  // 按码点计（emoji 等代理对算 1 个字符）
  let n = 0;
  for (let i = 0; i < s.length; i++) {
    const cc = s.charCodeAt(i);
    if (cc < 0xdc00 || cc > 0xdfff) n++;
  }
  c.chars += n;
}

export function getOutputCount(id: string): { chars: number; bytes: number } {
  const c = outputCount.get(id);
  return c ? { chars: c.chars, bytes: c.bytes } : { chars: 0, bytes: 0 };
}

export function dropOutputCount(id: string): void {
  outputCount.delete(id);
}

const PROMPT_TAIL = /[$#%>❯➜»λ]\s?$/;

/** 最近一次收到远端输出的时刻（0 = 还没有过） */
export function lastOutputAt(id: string): number {
  return lastOutput.get(id) ?? 0;
}

/**
 * 用户按键的旁路监听（不影响正常发送）。
 *
 * 宏录制需要知道用户敲了什么，但按键处理在 TerminalView、录制/回放逻辑在
 * App 层 —— 与其把 TerminalView 改成受控组件，不如在这里开一个只读的 tap：
 * 多订阅方互不干扰，xterm 的 onData 本身也支持多监听。
 */
type InputTap = (data: string) => void;
const inputTaps = new Map<string, Set<InputTap>>();

export function onInput(id: string, fn: InputTap): () => void {
  let set = inputTaps.get(id);
  if (!set) {
    set = new Set();
    inputTaps.set(id, set);
  }
  set.add(fn);
  return () => {
    set?.delete(fn);
    if (set && set.size === 0) inputTaps.delete(id);
  };
}

export function emitInput(id: string, data: string): void {
  const set = inputTaps.get(id);
  if (!set) return;
  for (const fn of set) fn(data);
}

/**
 * 等远端 shell 停在提示符上：输出静默 ≥350ms 且光标前文本以常见提示符结尾。
 * `since` 之后必须至少来过一次输出（避免拿旧屏幕的提示符误判，比如重连场景）。
 * 超时返回 false：宁可不恢复（卡片提示手动执行），也不往不确定的屏幕状态里打字。
 */
export async function waitForPrompt(
  id: string,
  since: number,
  timeoutMs = 10_000
): Promise<boolean> {
  const t0 = Date.now();
  for (;;) {
    await new Promise((r) => window.setTimeout(r, 150));
    const term = registry.get(id);
    if (!term) return false;
    const last = lastOutput.get(id) ?? 0;
    const quiet = last >= since && Date.now() - last > 350;
    if (quiet && PROMPT_TAIL.test(textBeforeCursor(term))) return true;
    if (Date.now() - t0 > timeoutMs) return false;
  }
}

export function getTerm(id: string): Terminal | undefined {
  return registry.get(id);
}

/** 终端显示宽度：CJK / 全角 / emoji 占 2 格，其余 1 格 */
export function cellWidth(s: string): number {
  let w = 0;
  for (const ch of s) {
    const c = ch.codePointAt(0) ?? 0;
    if (c < 0x20) continue;
    if (
      c >= 0x1f000 ||
      (c >= 0x1100 && c <= 0x115f) ||
      (c >= 0x2e80 && c <= 0xa4cf) ||
      (c >= 0xac00 && c <= 0xd7a3) ||
      (c >= 0xf900 && c <= 0xfaff) ||
      (c >= 0xfe30 && c <= 0xfe4f) ||
      (c >= 0xff00 && c <= 0xff60) ||
      (c >= 0xffe0 && c <= 0xffe6)
    ) {
      w += 2;
    } else {
      w += 1;
    }
  }
  return w;
}

/** 光标所在行、光标之前的文本（判断是否停在 shell 提示符上用） */
export function textBeforeCursor(term: Terminal): string {
  const buf = term.buffer.active;
  const line = buf.getLine(buf.baseY + buf.cursorY);
  return line ? line.translateToString(false, 0, buf.cursorX) : "";
}

/**
 * 抓当前屏幕快照：把 xterm 的整个缓冲区（scrollback + 视口）摊平成纯文本。
 *
 * 这是 AI 助手「当前会话上下文」的首选来源 —— 旁路缓冲只记开旁路之后的字节，
 * 用户刚连上就问「这个报错什么意思」时，旁路里往往是空的，而屏幕上写满了他要看的东西。
 *
 * @param maxChars 只保留末尾这么多字符（模型上下文有预算，越早的输出价值越低）
 */
export function snapshotConsole(
  id: string,
  maxChars = 24 * 1024
): { text: string; alt: boolean } | null {
  const term = registry.get(id);
  if (!term) return null;
  const buf = term.buffer.active;
  const alt = buf.type !== "normal";
  // 一个正常终端历史几千行足以覆盖「刚才发生了什么」，再多只是喂垃圾
  const from = Math.max(0, buf.length - 2000);
  const lines: string[] = [];
  for (let y = from; y < buf.length; y++) {
    const line = buf.getLine(y);
    if (line) lines.push(line.translateToString(true));
  }
  // 连续空行压成一行：命令之间的空隙对模型没有信息量，却会吃掉不少预算
  let text = lines.join("\n").replace(/\n{3,}/g, "\n\n").trim();
  if (text.length > maxChars) text = text.slice(text.length - maxChars);
  return { text, alt };
}
