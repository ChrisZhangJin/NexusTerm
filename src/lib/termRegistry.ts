import type { Terminal } from "@xterm/xterm";

/**
 * tabId → xterm 实例。App 层要读终端状态（光标列、是否停在提示符）来决定怎么注入还原
 * 命令，而 Terminal 由 TerminalView 持有 —— 只读登记表，避免把实例层层往上传。
 */
const registry = new Map<string, Terminal>();
/** 最近一次收到远端输出的时间（判断「输出已静默」用） */
const lastOutput = new Map<string, number>();

export function registerTerm(id: string, term: Terminal): () => void {
  registry.set(id, term);
  // P0 会话织物：屏幕帧采集挂在这里（TerminalView 之下、App 之上），
  // 与 AgentPanel 是否打开完全无关 —— 面板关掉、窗口最小化都照常上报。
  // 用 typeof 守卫：测试里塞进来的假 Terminal 没有这些事件。
  const disposables: Array<{ dispose(): void }> = [];
  const withCursor = term as Terminal & {
    onCursorMove?: (fn: () => void) => { dispose(): void };
    onWriteParsed?: (fn: () => void) => { dispose(): void };
  };
  if (typeof withCursor.onWriteParsed === "function") {
    disposables.push(withCursor.onWriteParsed(() => scheduleCapture(id)));
  }
  if (typeof withCursor.onCursorMove === "function") {
    disposables.push(withCursor.onCursorMove(() => scheduleCapture(id)));
  }
  return () => {
    for (const d of disposables) d.dispose();
    if (registry.get(id) === term) {
      registry.delete(id);
      lastOutput.delete(id);
      outputCount.delete(id);
      inputTaps.delete(id);
      userKeyLast.delete(id);
      dropCaptureState(id);
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
 * 用户按键的旁路监听（不影响正常发送）。按键处理在 TerminalView、宏录制在 App 层，
 * 与其把 TerminalView 改成受控组件，不如开一个只读 tap，多订阅方互不干扰。
 */
type InputTap = (data: string) => void;
const inputTaps = new Map<string, Set<InputTap>>();

/**
 * P3 接管夺权：用户敲键 → Steward 100ms 内转 ObservingOnly。
 * 与 screenDeltaSink 同模式：termRegistry 保持纯函数、不依赖 Tauri，
 * IPC 出口由 App 注入。节流到每 console 250ms 一次——夺权是一次性状态切换，
 * 连发字节里只有第一下有意义，后端 SLA 看的是首个键的处理延迟。
 */
type UserKeySink = (consoleId: string, keyAtMs: number) => void;
let userKeySink: UserKeySink | null = null;
const userKeyLast = new Map<string, number>();
const USER_KEY_MIN_GAP_MS = 250;

export function setUserKeySink(fn: UserKeySink | null): void {
  userKeySink = fn;
}

/**
 * xterm **自己**生成、却同样经 `onData` 冒出来的字节：焦点进出、鼠标上报、
 * 对远端查询的自动应答（光标位置 / 设备属性 / 状态 / 模式 / 窗口尺寸 / OSC / DCS）。
 * 这些不是用户动手，当成敲键会让 Steward 无缘无故被夺权（开了鼠标追踪的 TUI 里
 * 鼠标一晃就是一串）。一次 onData 可能连着好几段，整串都由这些组成才算。
 *
 * 与 `macro.isTerminalReply` 同一思路、覆盖面更大；放这里是因为 macro 依赖本模块，
 * 反过来引用会成环。只排除结构明确的形状，方向键 `ESC[A`、`ESC O P` 等仍算用户。
 */
const XTERM_GENERATED = new RegExp(
  "^(?:" +
    [
      "\\x1b\\[[IO]", // 焦点进出（DECSET 1004）
      "\\x1b\\[M[\\s\\S]{3}", // X10 / normal 鼠标：ESC [ M Cb Cx Cy
      "\\x1b\\[<[0-9]+;[0-9]+;[0-9]+[Mm]", // SGR 鼠标（1006）
      "\\x1b\\[[0-9]+;[0-9]+;[0-9]+M", // urxvt 鼠标（1015）
      "\\x1b\\[\\??[0-9]+;[0-9]+R", // CPR：光标位置报告
      "\\x1b\\[[?>][0-9;]*c", // DA1 / DA2：设备属性
      "\\x1b\\[\\??[0-9;]*n", // DSR：状态报告
      "\\x1b\\[\\??[0-9;]*\\$y", // DECRPM：模式应答
      "\\x1b\\[[0-9;]*t", // 窗口尺寸报告
      "\\x1b\\][\\s\\S]*?(?:\\x07|\\x1b\\\\)", // OSC 应答（标题 / 颜色查询）
      "\\x1bP[\\s\\S]*?\\x1b\\\\", // DCS 应答
    ].join("|") +
    ")+$"
);

export function isXtermGenerated(data: string): boolean {
  return data.length > 0 && XTERM_GENERATED.test(data);
}

/**
 * 当前有托管契约的 console。只有它们才采屏幕帧、发心跳、上报敲键夺权 ——
 * 没签契约的终端没人读这些帧，每 2s 一次的全屏重建 + 哈希 + IPC 纯属浪费。
 * App / 托管面板轮询契约列表后调 `setStewardConsoles` 喂进来。
 */
const stewardConsoles = new Set<string>();

export function setStewardConsoles(ids: Iterable<string>): void {
  const next = new Set(ids);
  const added = [...next].filter((id) => !stewardConsoles.has(id));
  const removed = [...stewardConsoles].filter((id) => !next.has(id));
  if (added.length === 0 && removed.length === 0) return;
  for (const id of removed) {
    stewardConsoles.delete(id);
    userKeyLast.delete(id);
    dropCaptureState(id);
  }
  for (const id of added) stewardConsoles.add(id);
  // 新签的契约立刻补一帧全量：后端马上就有屏幕可读，不必等下一次输出或心跳
  for (const id of added) {
    if (!registry.has(id)) continue;
    pendingFull.add(id);
    scheduleCapture(id, 0);
  }
  syncCaptureHeartbeat();
}

export function isStewardConsole(id: string): boolean {
  return stewardConsoles.has(id);
}

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
  // 只有真·用户输入、且这个 console 正在被托管时才上报；其余情况后端也只会回 -1
  if (userKeySink && stewardConsoles.has(id) && data.length > 0 && !isXtermGenerated(data)) {
    const now = Date.now();
    if (now - (userKeyLast.get(id) ?? 0) >= USER_KEY_MIN_GAP_MS) {
      userKeyLast.set(id, now);
      userKeySink(id, now);
    }
  }
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
 * 抓当前屏幕快照（scrollback + 视口摊平成纯文本）。这是 AI 助手上下文的首选来源 ——
 * 旁路缓冲只记开旁路之后的字节，刚连上就提问时往往是空的，而屏幕上写满了东西。
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

// ————————————————————————————————————————————————————————————————————————
// P0 会话织物：结构化屏幕帧采集
//
// 现状的 `snapshotConsole` 是「一轮工具调用里抓一次」的请求-回执模型；
// 主动托管需要的是后端常驻的屏幕状态。这里把 xterm 的活动缓冲摊成
// 「尾部 200 行 + 帧指纹」，只推变化行；后端 `ScreenStore` 合并并校验。
//
// 三条约束：
// 1. 采集失败绝不能影响终端（sink 抛错、buffer 读失败都吞掉）；
// 2. 不去重掉光标帧 —— 后端要靠它数 idle_frames 做空转判定；
// 3. 纯模块：不 import Tauri，IPC 由 App 注入 sink。
// ————————————————————————————————————————————————————————————————————————

/** 抓屏窗口内的行下标（0 = 窗口最旧一行），不是终端绝对行号 */
export interface ScreenLine {
  y: number;
  text: string;
}

export interface ScreenDelta {
  consoleId: string;
  frameId: number;
  /** 抓屏窗口首行的绝对行号；窗口滑动后必须发全量帧 */
  windowStart: number;
  total: number;
  cols: number;
  rows: number;
  alt: boolean;
  cursorX: number;
  cursorY: number;
  hash: string;
  full: boolean;
  lines: ScreenLine[];
  tsMs: number;
}

export interface ScreenDeltaAck {
  applied: boolean;
  /** 后端与前端不同步，下一帧必须发全量 */
  needFull: boolean;
  changed: boolean;
  frameId: number;
}

interface CaptureFrame {
  windowStart: number;
  lines: string[];
  hash: string;
  alt: boolean;
  cols: number;
  rows: number;
  cursorX: number;
  cursorY: number;
  frameId: number;
}

/** 与后端 `agent::steward::frame_hash` 完全一致（校验向量见 steward.test.ts） */
export function frameHash(
  lines: string[],
  alt: boolean,
  cols: number,
  rows: number,
  cursorX: number,
  cursorY: number
): string {
  const payload =
    lines.join("\n") +
    `\n#${alt ? 1 : 0}:${cols}:${rows}:${cursorX}:${cursorY}`;
  const bytes = new TextEncoder().encode(payload);
  let h = 0x811c9dc5 >>> 0;
  for (let i = 0; i < bytes.length; i++) {
    h ^= bytes[i];
    h = Math.imul(h, 0x01000193) >>> 0;
  }
  return h.toString(16).padStart(8, "0");
}

const CAPTURE_LINES = 200;
const CAPTURE_DEBOUNCE_MS = 180;
/**
 * P1：静默期也要发心跳帧。P0 只在 onWriteParsed / onCursorMove 时上报，
 * 后端分不清「屏幕没动」和「前端不在（浮窗 / 最小化 / 锁屏）」，
 * 空转计数也只会在有输出时增长。这里每 2s 补一帧同哈希帧（只带 frameId，
 * 不带行内容），后端 ScreenStore 据此累 idleFrames、Steward 据此判断流。
 * 与后端 stale_ms 默认 6s 配套：连续 3 次心跳没到才算断流。
 */
const CAPTURE_HEARTBEAT_MS = 2000;
type DeltaSink = (d: ScreenDelta) => void;

let captureEnabled = false;
let deltaSink: DeltaSink | null = null;
const captureFrames = new Map<string, CaptureFrame>();
const captureTimers = new Map<string, number>();
const pendingFull = new Set<string>();
let captureHeartbeat: number | null = null;

/** 注入 IPC 出口。App 启动时调一次；不注入时采集为空操作（前端单测不受影响）。 */
export function setScreenDeltaSink(fn: DeltaSink | null): void {
  deltaSink = fn;
}

/**
 * 本地总开关。采集是纯本地行为，不出网、不调模型；实际只对有托管契约的 console
 * 生效（见 `setStewardConsoles`），没有契约时连心跳定时器都不起。
 */
export function setScreenCaptureEnabled(on: boolean): void {
  captureEnabled = on;
  if (on) {
    for (const id of registry.keys()) {
      if (!stewardConsoles.has(id)) continue;
      pendingFull.add(id);
      scheduleCapture(id);
    }
  }
  syncCaptureHeartbeat();
}

/** 有开关、且至少一个托管中的 console 时才跑心跳 */
function syncCaptureHeartbeat(): void {
  if (captureEnabled && stewardConsoles.size > 0) startCaptureHeartbeat();
  else stopCaptureHeartbeat();
}

function startCaptureHeartbeat(): void {
  if (captureHeartbeat !== null) return;
  captureHeartbeat = window.setInterval(() => {
    if (!captureEnabled || !deltaSink) return;
    // hash 不变的帧只带 frameId + 空 lines，后端仍会累 idleFrames 并刷新 updatedAt
    for (const id of stewardConsoles) {
      if (registry.has(id)) scheduleCapture(id, 0);
    }
  }, CAPTURE_HEARTBEAT_MS);
}

function stopCaptureHeartbeat(): void {
  if (captureHeartbeat !== null) {
    window.clearInterval(captureHeartbeat);
    captureHeartbeat = null;
  }
}

/** 后端回报 needFull 时调用：下一帧改发全量，立刻重采一次。 */
export function requestFullFrame(id: string): void {
  pendingFull.add(id);
  scheduleCapture(id, 0);
}

/** 供面板手动刷新 / 验证用：显式操作，不受托管门控限制 */
export function captureNow(id: string): void {
  pendingFull.add(id);
  captureFrame(id, true);
}

function dropCaptureState(id: string): void {
  captureFrames.delete(id);
  pendingFull.delete(id);
  const timer = captureTimers.get(id);
  if (timer !== undefined) {
    window.clearTimeout(timer);
    captureTimers.delete(id);
  }
}

function scheduleCapture(id: string, delay = CAPTURE_DEBOUNCE_MS): void {
  // 没签契约的终端连定时器都不排：onWriteParsed 是热路径，这里只读两个布尔 + 一次 Set 查找
  if (!captureEnabled || !deltaSink || !stewardConsoles.has(id)) return;
  const existing = captureTimers.get(id);
  if (existing !== undefined) window.clearTimeout(existing);
  const timer = window.setTimeout(() => {
    captureTimers.delete(id);
    captureFrame(id);
  }, delay);
  captureTimers.set(id, timer);
}

function captureFrame(id: string, force = false): void {
  const term = registry.get(id);
  if (!term || !captureEnabled || !deltaSink) return;
  if (!force && !stewardConsoles.has(id)) return;
  try {
    const buf = term.buffer.active;
    const alt = buf.type !== "normal";
    const start = Math.max(0, buf.length - CAPTURE_LINES);
    const total = buf.length - start;
    const lines: string[] = [];
    for (let y = start; y < buf.length; y++) {
      const line = buf.getLine(y);
      lines.push(line ? line.translateToString(true) : "");
    }
    const cols = term.cols;
    const rows = term.rows;
    const cursorX = buf.cursorX;
    const cursorY = buf.cursorY;
    const hash = frameHash(lines, alt, cols, rows, cursorX, cursorY);
    const prev = captureFrames.get(id);
    const structural =
      !prev ||
      prev.windowStart !== start ||
      prev.lines.length !== total ||
      prev.alt !== alt ||
      prev.cols !== cols ||
      prev.rows !== rows;
    const full = structural || pendingFull.has(id);
    const changes: ScreenLine[] = [];
    if (full) {
      for (let i = 0; i < total; i++) changes.push({ y: i, text: lines[i] });
    } else if (prev) {
      for (let i = 0; i < total; i++) {
        if (lines[i] !== prev.lines[i]) changes.push({ y: i, text: lines[i] });
      }
    }
    const frameId = (prev?.frameId ?? 0) + 1;
    const delta: ScreenDelta = {
      consoleId: id,
      frameId,
      windowStart: start,
      total,
      cols,
      rows,
      alt,
      cursorX,
      cursorY,
      hash,
      full,
      lines: changes,
      tsMs: Date.now(),
    };
    captureFrames.set(id, {
      windowStart: start,
      lines,
      hash,
      alt,
      cols,
      rows,
      cursorX,
      cursorY,
      frameId,
    });
    pendingFull.delete(id);
    // sink 是 IPC 边界，抛错只记、绝不让终端渲染链路受影响
    try {
      deltaSink(delta);
    } catch {
      /* 旁路失败不影响主链路 */
    }
  } catch {
    /* buffer 正在被重建等瞬时状态，等下一次 tick */
  }
}
