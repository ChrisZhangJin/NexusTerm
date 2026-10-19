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
    }
  };
}

export function markOutput(id: string): void {
  lastOutput.set(id, Date.now());
}

const PROMPT_TAIL = /[$#%>❯➜»λ]\s?$/;

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
