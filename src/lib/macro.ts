import type { MacroStep } from "../types";
import { api, stringToBytes } from "../api";
import { getTerm, lastOutputAt, onInput } from "./termRegistry";

/**
 * 宏：一串「等什么、发什么」的操作，可手动一键重放，也可以绑定到会话在连接后自动执行。
 *
 * 为什么执行放在前端而不是后端：
 * 判断「现在能不能发」靠的是**屏幕内容**（xterm 已经解好了光标移动与控制码），
 * 后端只有原始字节流，自己再解一遍 ANSI 是重复劳动，还容易和渲染结果不一致。
 * 后端只负责持久化。
 *
 * 设计上刻意只有三种原语：
 * - `send`  发文本（可选回车）
 * - `wait`  等屏幕上出现某个模式；模式为空 = 等输出静默
 * - `sleep` 无条件等
 * 表达力靠组合，而不是把宏做成一个小语言 —— 需要条件的场景，用户应该写脚本。
 */

/** 等模式出现的默认超时：跨公网 + 慢速堡垒机，20 秒是经验值 */
export const DEFAULT_WAIT_TIMEOUT = 20_000;
/** 「输出静默」的判定窗口：太小会在多包输出中间误判，太大连接慢时白等 */
export const QUIET_WINDOW = 800;
/** 等静默的默认超时 */
export const QUIET_TIMEOUT = 20_000;
/** 两条发送之间留一点缝，避免并成一个包 */
const SEND_GAP = 60;
/** 轮询屏幕的间隔 */
const POLL = 120;
/** 「一开始就已经匹配」时，最多再等这么久的新输出 */
const ALREADY_MATCHED_GRACE = 1000;
/** 静默步骤在完全没有输出时的兜底：说明服务端根本不会说话 */
const NO_OUTPUT_GRACE = 3000;

/** 可被 abort 打断的 sleep */
function sleepAbortable(ms: number, signal?: AbortSignal): Promise<boolean> {
  if (signal?.aborted) return Promise.resolve(false);
  return new Promise((resolve) => {
    const t = globalThis.setTimeout(() => {
      signal?.removeEventListener("abort", onAbort);
      resolve(true);
    }, ms);
    const onAbort = () => {
      globalThis.clearTimeout(t);
      resolve(false);
    };
    signal?.addEventListener("abort", onAbort, { once: true });
  });
}

/**
 * 把编辑器里的转义写法还原成真实字节。
 *
 * 支持 `\r` `\n` `\t` `\e` `\\` `\xNN`；`\\` 必须最先处理，否则 `\\n`
 * 会被拆成「反斜杠 + n 两个字符」。
 */
export function unescapeText(s: string): string {
  let out = "";
  for (let i = 0; i < s.length; i++) {
    const ch = s[i];
    if (ch !== "\\" || i === s.length - 1) {
      out += ch;
      continue;
    }
    const nx = s[i + 1];
    switch (nx) {
      case "\\":
        out += "\\";
        i++;
        break;
      case "r":
        out += "\r";
        i++;
        break;
      case "n":
        out += "\n";
        i++;
        break;
      case "t":
        out += "\t";
        i++;
        break;
      case "e":
        out += "\x1b";
        i++;
        break;
      case "0":
        out += "\x00";
        i++;
        break;
      case "x": {
        const hex = s.slice(i + 2, i + 4);
        if (/^[0-9a-fA-F]{2}$/.test(hex)) {
          out += String.fromCharCode(parseInt(hex, 16));
          i += 3;
        } else {
          out += ch;
        }
        break;
      }
      default:
        out += ch;
    }
  }
  return out;
}

/**
 * `unescapeText` 的逆运算：把控制字符写成可读的转义，供编辑器显示。
 *
 * 录制器录到的是原始按键（方向键是 `ESC [ A`），直接塞进输入框会显示成一堆
 * 不可见字符、也没法手改，所以统一转义。换行不在此列 —— 换行由「发送 + 回车」
 * 表达，不会出现在文本里。
 */
export function escapeText(s: string): string {
  let out = "";
  for (const ch of s) {
    const code = ch.codePointAt(0) ?? 0;
    if (ch === "\\") out += "\\\\";
    else if (ch === "\r") out += "\\r";
    else if (ch === "\n") out += "\\n";
    else if (ch === "\t") out += "\\t";
    else if (ch === "\x1b") out += "\\e";
    else if (code < 0x20 || code === 0x7f) {
      out += "\\x" + code.toString(16).padStart(2, "0");
    } else out += ch;
  }
  return out;
}

/**
 * 编译等待模式。
 *
 * - 空串：不匹配任何内容（调用方会走「等静默」分支）
 * - `/re/` 或 `/re/i`：正则
 * - 其余：子串
 * 正则写坏时退化成子串匹配 —— 宏是给运维现场用的，宁可变笨也不能在
 * 执行中途抛一个用户看不懂的异常。
 */
export function compilePattern(pattern: string): ((text: string) => boolean) | null {
  if (!pattern) return null;
  const m = /^\/([\s\S]*)\/([a-z]*)$/.exec(pattern);
  if (m) {
    try {
      const re = new RegExp(m[1], m[2]);
      return (t) => re.test(t);
    } catch {
      // 落到子串分支
    }
  }
  return (t) => t.includes(pattern);
}

/**
 * 把屏幕摊平成文本给模式匹配用。
 *
 * 关键在 `isWrapped`：一行内容被折行时，xterm 存成多条 buffer 行，直接按行
 * 拼 `\n` 会把一个长串（比如整条命令行）切断，模式永远匹配不上。
 * 折行续行不插换行，逻辑行之间才插。
 */
export function screenText(tabId: string, maxRows = 300): string | null {
  const term = getTerm(tabId);
  if (!term) return null;
  const buf = term.buffer.active;
  const end = Math.max(0, buf.baseY + buf.cursorY + 1);
  const start = Math.max(0, end - maxRows);
  let out = "";
  for (let y = start; y < end; y++) {
    const line = buf.getLine(y);
    if (!line) continue;
    out += line.translateToString(true);
    const next = y + 1 < end ? buf.getLine(y + 1) : undefined;
    if (!next || !next.isWrapped) out += "\n";
  }
  return out;
}

/**
 * xterm 自己会对远端的查询自动应答（光标位置、设备属性、窗口尺寸…），
 * 这些应答同样经 `term.onData` 冒出来。两种情况都必须把它排除：
 *
 * - **回放**：宏运行中若把它当成「用户按键」，tmux / vim 一发查询就会把宏中止；
 * - **录制**：把它录成一步「发送」，重放时就是把 `ESC[12;40R` 当命令敲进去。
 *
 * 只忽略有明确结构的应答；其余（包括方向键 `ESC[A`）一律当作用户输入 ——
 * 宁可多中止一次，也不要把真实的插入动作当成机器应答而漏掉。
 */
export function isTerminalReply(data: string): boolean {
  return (
    /^\x1b\[[0-9]+;[0-9]+R$/.test(data) || // DSR：光标位置报告
    /^\x1b\[[?>][0-9;]*c$/.test(data) || // DA：设备属性
    /^\x1b\[[0-9;]*t$/.test(data) || // 窗口尺寸报告
    /^\x1b\][\s\S]*(\x07|\x1b\\)$/.test(data) || // OSC 应答（标题/颜色查询）
    /^\x1bP[\s\S]*\x1b\\$/.test(data) // DCS 应答
  );
}

export type MacroFailure = "cancelled" | "timeout" | "closed";

export interface MacroRunOutcome {
  ok: boolean;
  /** 已经成功发出的步骤数 */
  sent: number;
  /** 失败所在步骤下标（从 0 开始） */
  step?: number;
  reason?: MacroFailure;
}

export interface MacroRunOptions {
  signal?: AbortSignal;
  /** 每开始一步回调一次，用于界面上的进度提示 */
  onStep?: (index: number, step: MacroStep) => void;
  /**
   * 写入口。默认走 SSH；本地终端传 `localWrite`。
   * 抽出来而不是在引擎里判断「这是不是本地标签」——引擎不该知道标签的种类。
   */
  write?: (tabId: string, data: number[]) => Promise<void>;
}

/**
 * 等屏幕上出现模式。
 *
 * 「已经匹配」的处理是这里最容易出错的地方：宏执行时屏幕往往还留着**上一条**
 * 命令留下的同样文字（典型：`Password:` 连着问两次、菜单重画）。如果一上来
 * 匹配成功就发，等于拿旧内容当就绪信号。
 *
 * 判据：先拍一张基线快照。只有当「匹配」并且「屏幕已经不同于基线」才算真命中；
 * 屏幕没变化时最多再等 `ALREADY_MATCHED_GRACE` —— 覆盖「本来就绪、屏幕不会再动」
 * 的场景（比如 shell 提示符早就在那儿），又不至于在旧提示符上抢跑。
 */
async function waitForPattern(
  tabId: string,
  test: (text: string) => boolean,
  timeoutMs: number,
  signal?: AbortSignal
): Promise<boolean> {
  const t0 = Date.now();
  const baseline = screenText(tabId);
  if (baseline === null) return false;
  for (;;) {
    if (signal?.aborted) return false;
    const text = screenText(tabId);
    if (text === null) return false;
    if (test(text)) {
      if (text !== baseline || Date.now() - t0 >= ALREADY_MATCHED_GRACE) return true;
    }
    if (Date.now() - t0 > timeoutMs) return false;
    if (!(await sleepAbortable(POLL, signal))) return false;
  }
}

/**
 * 等输出静默：屏幕上「没有新东西再来了」。
 *
 * 这是不依赖具体提示符的通用判据 —— 跳板机菜单、`Password:` 提示、
 * 慢速 shell 的登录 banner 都能用它等到「画完」那一刻。
 */
async function waitQuiet(
  tabId: string,
  quiet: number,
  timeoutMs: number,
  since: number,
  signal?: AbortSignal
): Promise<boolean> {
  const t0 = Date.now();
  for (;;) {
    if (signal?.aborted) return false;
    if (getTerm(tabId) === undefined) return false;
    const now = Date.now();
    const last = lastOutputAt(tabId);
    if (last >= since) {
      if (now - last >= quiet) return true;
    } else if (now - t0 >= Math.max(quiet, NO_OUTPUT_GRACE)) {
      // 这一步开始后压根没有输出：不是「输出没停」，而是「对方不说话」，
      // 再等下去只会把宏卡到超时
      return true;
    }
    if (now - t0 > timeoutMs) return false;
    if (!(await sleepAbortable(POLL, signal))) return false;
  }
}

/**
 * 顺序执行宏。
 *
 * 任何一步超时 / 会话消失 / 被中止都会立刻停下并返回原因 —— 宏最危险的失败
 * 模式是「继续往下发」：把本该发给菜单的选项打到目标机器的 shell 里。
 */
export async function runMacro(
  tabId: string,
  steps: MacroStep[],
  opts: MacroRunOptions = {}
): Promise<MacroRunOutcome> {
  const { signal } = opts;
  const write = opts.write ?? ((id: string, bytes: number[]) => api.sshWrite(id, bytes));
  let sent = 0;
  /**
   * 「上一件事发生在什么时候」：等待静默以它为起点。
   *
   * 不能在 wait 开始的瞬间取 now —— 发送之后服务端往往几毫秒就开始回包，
   * 等 wait 真正开始时那批输出已经过去了，会被误判成「这一步没有任何输出」，
   * 白白多等一个兜底窗口。以「最近一次发送」为锚点，才能正确判断
   * 「我这句发出去之后，它说完了没有」。
   */
  let anchor = Date.now();

  // 自动宏是在「连接就绪」那一刻开始跑的，而 xterm 实例由 TerminalView 在
  // 挂载时登记，可能差一帧。等它一会儿，别把「还没挂好」当成「连接已断开」。
  for (let i = 0; i < 15 && getTerm(tabId) === undefined; i++) {
    if (signal?.aborted) return { ok: false, sent, step: 0, reason: "cancelled" };
    if (!(await sleepAbortable(100, signal))) break;
  }

  for (let i = 0; i < steps.length; i++) {
    if (signal?.aborted) return { ok: false, sent, step: i, reason: "cancelled" };
    const step = steps[i];
    opts.onStep?.(i, step);

    if (step.kind === "send") {
      if (getTerm(tabId) === undefined) {
        return { ok: false, sent, step: i, reason: "closed" };
      }
      const text = unescapeText(step.text) + (step.enter ? "\r" : "");
      try {
        await write(tabId, stringToBytes(text));
      } catch {
        return { ok: false, sent, step: i, reason: "closed" };
      }
      sent++;
      anchor = Date.now();
      await sleepAbortable(SEND_GAP, signal);
      continue;
    }

    if (step.kind === "sleep") {
      const ms = Math.max(0, step.ms || 0);
      if (!(await sleepAbortable(ms, signal))) {
        return { ok: false, sent, step: i, reason: "cancelled" };
      }
      continue;
    }

    // wait
    const since = anchor;
    const test = compilePattern(step.pattern.trim());
    const defaultTimeout = test ? DEFAULT_WAIT_TIMEOUT : QUIET_TIMEOUT;
    const timeout = step.timeoutMs > 0 ? step.timeoutMs : defaultTimeout;
    const ok = test
      ? await waitForPattern(tabId, test, timeout, signal)
      : await waitQuiet(tabId, QUIET_WINDOW, timeout, since, signal);
    if (!ok) {
      if (signal?.aborted) return { ok: false, sent, step: i, reason: "cancelled" };
      // 会话被关掉和单纯超时要分开报：前者重连就行，后者要改宏
      const gone = getTerm(tabId) === undefined;
      return { ok: false, sent, step: i, reason: gone ? "closed" : "timeout" };
    }
  }

  return { ok: true, sent };
}

/** 编辑器里一行步骤的摘要，用于折叠态与提示 */
export function describeStep(step: MacroStep): string {
  switch (step.kind) {
    case "send": {
      const text = step.text === "" ? "（空行回车）" : step.text;
      return `发送 ${text}${step.enter ? " ⏎" : ""}`;
    }
    case "wait":
      return step.pattern.trim()
        ? `等待出现 ${step.pattern}`
        : `等待输出停止 ${((step.timeoutMs || QUIET_TIMEOUT) / 1000).toFixed(0)}s 内`;
    case "sleep":
      return `等待 ${(step.ms / 1000).toFixed(1)}s`;
  }
}

export interface MacroRecorder {
  /** 停止录制并交出步骤（空数组表示什么都没录到） */
  stop(): MacroStep[];
  /** 放弃本次录制 */
  cancel(): void;
  /** 已录到的步数（用于界面计数） */
  count(): number;
}

/**
 * 录制宏：把用户在终端里敲的东西变成步骤。
 *
 * 录制的核心不是「按键序列」，而是「在什么时机敲」。所以每提交一行前都会
 * 插一个 `wait`（等输出静默）—— 重放时不靠固定延时，而是等屏幕真正稳定，
 * 这也是跳板机菜单能可靠重放的原因。
 *
 * 控制键（方向键、Ctrl-C、Tab）单独成为一步：它们不发回车，且往往是
 * 「在菜单里上下移动」这类动作，混进文本里重放就会变形。
 */
export function startRecording(tabId: string): MacroRecorder {
  const steps: MacroStep[] = [];
  let line = "";
  let stopped = false;

  const pushWait = () => {
    // 连续两个 wait 没有意义，只留一个
    const last = steps[steps.length - 1];
    if (last && last.kind === "wait") return;
    steps.push({ kind: "wait", pattern: "", timeoutMs: 0 });
  };

  const flushStandalone = (raw: string) => {
    if (!raw) return;
    pushWait();
    steps.push({ kind: "send", text: escapeText(raw), enter: false });
  };

  const submit = () => {
    pushWait();
    steps.push({ kind: "send", text: escapeText(line), enter: true });
    line = "";
  };

  const off = onInput(tabId, (data: string) => {
    if (stopped) return;
    // 终端对远端查询的自动应答不是用户操作，录进去就是垃圾步骤
    if (isTerminalReply(data)) return;
    // `buf` 是这一批里连续的可打印字符。它和 `line` 是两个概念：
    // `line` 是「当前这一行已经敲了什么」，`buf` 是还没并进去的一小段。
    // 凡是会改动行内容的按键（回车 / 退格 / Ctrl-U），必须先把 buf 并进 line，
    // 否则就会出现「退格删不掉刚敲的字」这类错位。
    let buf = "";
    for (let i = 0; i < data.length; i++) {
      const ch = data[i];
      if (ch === "\r" || ch === "\n") {
        // 粘贴里的 \r\n 只算一次提交
        if (ch === "\r" && data[i + 1] === "\n") i++;
        line += buf;
        buf = "";
        submit();
        continue;
      }
      if (ch === "\x1b") {
        // ESC 序列（方向键 / F1...）整体当一个动作
        const m = /^\x1b\[[0-9;]*[A-Za-z]|^\x1bO[A-Za-z]/.exec(data.slice(i));
        const seq = m ? m[0] : "\x1b";
        flushStandalone(buf);
        buf = "";
        flushStandalone(seq);
        i += seq.length - 1;
        continue;
      }
      if (ch === "\x7f" || ch === "\b") {
        line += buf;
        buf = "";
        line = line.slice(0, -1);
        continue;
      }
      if (ch === "\x15") {
        // Ctrl-U：清掉当前行（远端也是这个行为）
        line += buf;
        buf = "";
        line = "";
        continue;
      }
      if (ch === "\x03" || ch === "\t" || ch.charCodeAt(0) < 0x20) {
        flushStandalone(buf);
        buf = "";
        flushStandalone(ch);
        continue;
      }
      buf += ch;
      if (line.length + buf.length > 4000) {
        // 明显不是「敲一行」，多半是往编辑器里粘贴大段文本：
        // 截断会把宏变成半截命令，直接放弃这一段更安全
        buf = "";
        line = "";
      }
    }
    if (buf) line += buf;
    if (steps.length > 200) {
      stopped = true;
      off();
    }
  });

  return {
    count: () => steps.length,
    stop() {
      if (!stopped) {
        stopped = true;
        off();
      }
      // 结尾没按回车的那半行也记下来 —— 用户可能就是想发个不带回车的命令
      if (line) {
        pushWait();
        steps.push({ kind: "send", text: escapeText(line), enter: false });
        line = "";
      }
      return steps;
    },
    cancel() {
      stopped = true;
      off();
      steps.length = 0;
      line = "";
    },
  };
}
