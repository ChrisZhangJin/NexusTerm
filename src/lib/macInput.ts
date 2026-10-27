import type { Terminal } from "@xterm/xterm";
import { api } from "../api";

/**
 * macOS(WKWebView) 输入接管层。xterm 的键盘路径在 WebKit 上有三类缺陷，这里
 * 在容器上以**捕获阶段**抢在 xterm 之前处理，下面每一条都是真机复现过的：
 *
 * 1. **丢字**（`config` → `cnfig`）：输入法上下文还热着时，WebKit 把普通字母的
 *    keydown 也标成 keyCode 229；xterm 的 `_keyDown` 先置 `_keyDownSeen=true`
 *    再被 `CompositionHelper.keydown`（对 229 一律 return false）提前返回 ——
 *    这颗键什么都没发；随后的 `input` 又被 `_inputEvent` 的
 *    `!e.composed || !this._keyDownSeen` 门整条丢掉。所以普通 ASCII 键由我们
 *    `preventDefault` + `stopPropagation` 后自己 `send`，另加一条 input 兜底
 *    （WebKit 可能无视 preventDefault 仍派发 input）。两条路去重见 `KeyLedger`。
 * 2. **多空格 / 多字符**：WebKit 会给普通按键（尤其空格）补一对 composition
 *    事件，所以 `compositionstart` **不清账本**；接管的键不经过 xterm 的
 *    `_keyDown`，隐藏 textarea 就没人清，攒下的脏字符会从
 *    `_finalizeComposition` 的 `substring` 漏进 pty —— 见 `clearTextarea`；
 *    多字符的 `insertText`（如 macOS 双空格变 ". "）整条丢掉，不逐字重放。
 * 3. **组合态里 IME 吃掉的键被当成真输入**（`da`+`2` → 「大2」）：组合一开，
 *    任何可打印键都属于 IME，见 `isImeKey`。拦法只能是 `stopPropagation`，
 *    `preventDefault` 会卡住候选上屏；代价是该键仍可能写进 textarea，故在
 *    `compositionend` 按 `ev.data` 决定是否剥掉尾部字符。
 */

/** xterm 5.x 挂在自己实例（不是 `_core`）上的私有状态 */
interface TerminalInternals {
  _keyDownSeen?: boolean;
  _keyPressHandled?: boolean;
  textarea?: HTMLTextAreaElement | null;
}

/** 单个可打印 ASCII 字符（0x20–0x7e） */
export function isPlainChar(ch: string | null | undefined): boolean {
  if (!ch || ch.length !== 1) return false;
  const c = ch.charCodeAt(0);
  return c >= 0x20 && c < 0x7f;
}

/**
 * 这颗 keydown 该不该由我们接管：无 Ctrl/Alt/⌘（组合键归 xterm 发转义序列，
 * Option 出的 é / 死键也在那条路上，碰了就是两份输入）、且是单个可打印 ASCII
 * （中文、emoji 留给 xterm 的组合路径；输入法按键的 `key` 是 `"Process"` /
 * `"Dead"`，长度不为 1 天然被排除）。
 *
 * 刻意**不看 `keyCode === 229`**：229 正是丢字现场的特征（输入法上下文残留），
 * 排除它等于把要修的那一类键交回给会丢字的路径。是否真在组合由调用方的
 * `composing` 判断 —— 那是事实，229 只是浏览器的心情。
 */
export function isPlainKeystroke(ev: {
  key: string;
  ctrlKey: boolean;
  altKey: boolean;
  metaKey: boolean;
  isComposing?: boolean;
}): boolean {
  if (ev.ctrlKey || ev.altKey || ev.metaKey) return false;
  return isPlainChar(ev.key);
}

/**
 * 输入法的「确认键」：空格 / 回车选词。组合态里它们属于 IME，一颗字节都不该进
 * shell，但 WKWebView 给空格的 `keyCode` 是 32 而非 229，xterm 的
 * `CompositionHelper.keydown` 只对 229 提前返回，于是 `_keyDown` 一路走到
 * `triggerDataEvent(i.key)` —— 中文上屏后还多一个空格（「pin + 空格 → 拼 」）。
 */
export function isImeConfirmKey(ev: { key: string }): boolean {
  return ev.key === " " || ev.key === "Spacebar" || ev.key === "Enter";
}

/**
 * 组合态里**被 IME 吃掉**的键。判据刻意宽到「任何可打印键 + 回车」：组合一开，
 * 键盘就不归 shell 管，拼音字母进预编辑、数字选候选、`, . - =` 翻页、空格 /
 * 回车确认。交给 xterm 的后果是它把选候选的「2」当成真输入：`da`+`2` → 「大2」。
 */
export function isImeKey(ev: { key: string }): boolean {
  return ev.key === "Enter" || isPlainChar(ev.key);
}

/**
 * xterm 会不会把这颗键**当成真输入发出去**（即我们要拦的对象）。
 *
 * 229 那类（拼音字母）放行：xterm 自己 `return false`，且顺带调
 * `_handleAnyTextareaChanges()` 刷新预编辑，拦了反而让「da」敲出来不显示。
 * 确认键无条件算「会发出去」—— 空格 / 回车的 keyCode 在 WKWebView 上时而 229
 * 时而不是，两头都得拦。
 */
export function xtermWouldEmit(ev: { key: string; keyCode?: number }): boolean {
  return isImeConfirmKey(ev) || (isPlainChar(ev.key) && ev.keyCode !== 229);
}

/**
 * 「已经发过」的账本：同一颗字符可能从 keydown 和 input 两条路各来一次，先后
 * 顺序在 WKWebView 上不保证（WebKit bug 165004：beforeinput 可能早于 keydown）。
 * 谁先到谁发，后到的认领掉自己。
 *
 * 必须带有效期 + 条数上限：一颗键发出去却永远没有回声是常态，账本项若不过期，
 * 十秒后同一个字母就会被误认领 —— 变成新的丢字。
 */
export class KeyLedger {
  private fromKey: Array<{ ch: string; at: number }> = [];
  private fromInput: Array<{ ch: string; at: number }> = [];

  constructor(private readonly ttlMs = 250) {}

  clear(): void {
    this.fromKey = [];
    this.fromInput = [];
  }

  /**
   * keydown 要发这颗字符吗？
   *
   * @returns false 表示「input 已经先发过这一颗」—— 调用方仍要拦住 xterm，
   *          但不要再发一遍。
   */
  keyDown(ch: string, now: number): boolean {
    this.sweep(now);
    if (this.take(this.fromInput, ch, now)) return false;
    this.push(this.fromKey, ch, now);
    return true;
  }

  /**
   * 这条 input 还有哪些字符没发过？
   *
   * 多字符的 `data`（听写、某些 WebKit 合并）逐字核对，只补真正缺的那部分。
   */
  input(data: string, now: number): string {
    this.sweep(now);
    let rest = "";
    for (const ch of data) {
      if (this.take(this.fromKey, ch, now)) continue;
      rest += ch;
    }
    for (const ch of rest) this.push(this.fromInput, ch, now);
    return rest;
  }

  /** 账本规模（自测用） */
  get size(): number {
    return this.fromKey.length + this.fromInput.length;
  }

  private take(list: Array<{ ch: string; at: number }>, ch: string, now: number): boolean {
    const i = list.findIndex((e) => e.ch === ch && now - e.at <= this.ttlMs);
    if (i < 0) return false;
    list.splice(i, 1);
    return true;
  }

  private push(list: Array<{ ch: string; at: number }>, ch: string, now: number): void {
    list.push({ ch, at: now });
    if (list.length > 8) list.splice(0, list.length - 8);
  }

  private sweep(now: number): void {
    this.fromKey = this.fromKey.filter((e) => now - e.at <= this.ttlMs);
    this.fromInput = this.fromInput.filter((e) => now - e.at <= this.ttlMs);
  }
}

/** `performance.now()` 在正常环境都有；测试与 node 里退回 Date.now 也无所谓 */
const nowMs = (): number =>
  typeof performance !== "undefined" && typeof performance.now === "function"
    ? performance.now()
    : Date.now();

/**
 * 现场记录：每一条输入事件（含我们的处置）进环形缓冲，只在手动打开开关时才写：
 *
 * ```js
 * globalThis.__NX_TRACE_INPUT = true      // 然后复现那几秒
 * JSON.stringify(globalThis.__nxInputLog.slice(-40))
 * ```
 *
 * 刻意**全量记**（连 pass / drop 的事件、keyCode、isComposing、textarea 一起记）：
 * 丢字 / 多字 / 多空格 / 标点重复这几类症状只闪一下，前几轮全靠猜，连错三次；
 * 真正判断错的一直是「WKWebView 到底派了哪些事件」，只记发出的字节看不出来。
 */
type TraceRec = {
  /** 事件类型：keydown / keyup / input / beforeinput / compstart / compupdate / compend */
  ev: string;
  /** 我们的处置：send / defer / stop / pass / drop */
  act: string;
  /** 实际发出的字节（只有 act=send 时才有） */
  sent?: string;
  /** 谁发的：`mac` 是接管层，`xterm` 是 xterm 自己走 onData */
  via?: string;
  /** ev.key */
  key?: string;
  /** ev.keyCode —— 一直靠猜的判断依据，必须记真值 */
  kc?: number;
  /** ev.code —— 物理键，不受 ⌥/输入法影响 */
  code?: string;
  /** 修饰键：C=Ctrl A=Alt M=Cmd S=Shift */
  mods?: string;
  /** ev.isComposing（浏览器自称） */
  ic?: boolean;
  /** 我们自己跟踪的 composing（由 compositionstart/end 维护） */
  comp?: boolean;
  /** InputEvent.inputType */
  it?: string;
  /** InputEvent.data / CompositionEvent.data */
  data?: string;
  /** 处置前隐藏 textarea 的内容（非空就说明有攒字） */
  ta?: string;
  t?: number;
};

const traceOn = (): boolean =>
  (globalThis as { __NX_TRACE_INPUT?: boolean }).__NX_TRACE_INPUT === true;

function trace(rec: TraceRec): void {
  if (!traceOn()) return;
  const g = globalThis as { __nxInputLog?: TraceRec[] };
  const log = (g.__nxInputLog ??= []);
  log.push({ t: nowMs(), ...rec });
  if (log.length > 5000) log.splice(0, log.length - 5000);
}

/** 记录一次「字节发往 pty」。`via` 区分是我们发的还是 xterm 自己发的。 */
export function traceSend(data: string, via: "mac" | "xterm"): void {
  if (!traceOn()) return;
  // 控制字节转成可见形式，不然 JSON 里一片 \u0001 没法看
  const vis = data.replace(/[\x00-\x1f\x7f]/g, (c) => `<${c.charCodeAt(0)}>`);
  trace({ ev: "send", act: "send", sent: vis, via });
}

/** textarea 内容截断，避免一条日志被整段粘贴撑爆 */
const taOf = (ta: HTMLTextAreaElement | null | undefined): string => {
  const v = ta?.value ?? "";
  return v.length > 40 ? `${v.slice(0, 40)}…(${v.length})` : v;
};

const modsOf = (ev: { ctrlKey?: boolean; altKey?: boolean; metaKey?: boolean; shiftKey?: boolean }): string =>
  (ev.ctrlKey ? "C" : "") + (ev.altKey ? "A" : "") + (ev.metaKey ? "M" : "") + (ev.shiftKey ? "S" : "");

/**
 * 把缓冲里的日志落盘。开关关着时一个字节都不写 —— 终端里敲的东西包含密码，
 * 没有明确开记录就不该落盘。
 */
let flushOn = false;
let flushTimer: number | undefined;
let flushing = false;
let flushed = 0;

async function flush(): Promise<void> {
  if (flushing || !flushOn) return;
  const g = globalThis as { __nxInputLog?: TraceRec[] };
  const log = g.__nxInputLog;
  if (!log || flushed >= log.length) return;
  const lines = log.slice(flushed).map((r) => JSON.stringify(r)).join("\n") + "\n";
  flushed = log.length;
  flushing = true;
  try {
    await api.appendInputLog(lines);
  } catch {
    // 写不进去就算了，绝不能因为记日志影响输入
  } finally {
    flushing = false;
  }
}

/**
 * 打开 / 关闭输入日志。
 *
 * @param on     开就记
 * @param toFile 真则每 400ms 追加落盘一次；否则只留在内存
 *               （`globalThis.__nxInputLog`）。
 */
export function setInputTrace(on: boolean, toFile = false): void {
  (globalThis as { __NX_TRACE_INPUT?: boolean }).__NX_TRACE_INPUT = on;
  if (!on) {
    flushOn = false;
    if (flushTimer !== undefined) {
      clearInterval(flushTimer);
      flushTimer = undefined;
    }
    flushed = 0;
    return;
  }
  if (toFile) {
    flushOn = true;
    // 立刻写一行抬头：文件在打开开关那一刻就建出来，这样「到底有没有在记」
    // 不用等第一次按键才知道 —— 排查时最容易白等的就是这一下。
    trace({ ev: "trace-on", act: "state", data: new Date().toISOString() });
    void flush();
    if (flushTimer === undefined) flushTimer = window.setInterval(() => void flush(), 400);
  }
}

/**
 * 挂上接管逻辑，返回卸载函数。
 *
 * @param host   终端容器（xterm 的 textarea 是它的后代，捕获阶段先到这里）
 * @param term   已 `open()` 的终端
 * @param send   把字节送到 pty 的唯一出口（与 `term.onData` 用的是同一个）
 * @param isMac  是否 macOS。非 mac 直接空转 —— 这个缺陷只在 WebKit 上出现，
 *               没有理由让 Windows / Linux 也走一条自己写的发送路径。
 */
export function installMacKeyboard(
  host: HTMLElement,
  term: Terminal,
  send: (data: string) => void,
  isMac: boolean
): () => void {
  if (!isMac) return () => {};
  const internals = term as unknown as TerminalInternals;
  const ledger = new KeyLedger();

  /** 发字节的唯一出口（本层内部）。顺手记一笔「谁发的、发了什么」。 */
  const out = (data: string, via: "mac" | "xterm" = "mac"): void => {
    traceSend(data, via);
    send(data);
  };

  // 组合状态由我们自己跟：`ev.isComposing` 在「输入法上下文残留」时是假的
  // （该 false 的时候 true、该 true 的时候 false），只有 compositionstart
  // 与 compositionend 这对事件说的是事实。
  let composing = false;
  /** 本轮组合里被我们拦下的那颗 IME 键；只供 `onComposeEnd` 决定是否剥尾。 */
  let imeConsumedKey: string | null = null;
  /** 最近一次 `compositionend` 的时间戳，用于识别「同一次按键的两半」 */
  let composeEndedTs = -Infinity;

  /**
   * 中文输入法「智能全角标点」把 `,` `;` 换成 `，` `；`，曾经两个都发了出去。
   *
   * 真机日志（macOS + WKWebView + 系统拼音，敲一个中文逗号）推翻了前三版的猜测：
   *
   * ```
   * beforeinput/input  data="，"  ic=false   ← 全角结果先到，xterm 已发过一遍
   * keydown  key="，"  kc=229  code=Comma    ← 键事件后到，key 已是全角
   * keyup    key="，"  kc=188
   * ```
   *
   * 即 `input` 可能**早于** keydown（WebKit 165004），且 `ev.key` 已是替换后的
   * 全角字符、keyCode 是 229 —— 按半角 `,` 比对永远命中不了（9d4d3aa / 8fbcd26
   * 改的都是空门），而「等 input 再发」的写法会在 input 先到时超时补发第二个。
   *
   * 所以全角这条路我们完全不插手（非 ASCII 的 key 既不过 `isPlainKeystroke`
   * 也不在 `PUNCT_RISK` 里）。下面的分支只对「`ev.key` 仍是半角」的机型生效：
   * 先不发，等 WebKit 写定的那条 `input`；80ms 没等到说明没被替换，原样补发
   * —— 不补就是吞字。
   */
  const PUNCT_RISK = new Set([",", ".", ";", ":", "'", '"', "!", "?", "(", ")", "[", "]", "<", ">", "$", "\\", "^", "_", "`", "~"]);
  let pendingPunct: { key: string; at: number } | null = null;
  let pendingTimer: number | undefined;
  /**
   * 最近一次 insertText 的时刻：标点那颗键的 input 可能**已经先到**（见上面的
   * 真机日志），此时再等一轮、再补发就是第二个字符的来历。200ms 内有过
   * insertText 就认定结果已交出，接管层闭嘴。
   */
  let lastTextInputAt = -Infinity;
  const clearPending = () => {
    pendingPunct = null;
    if (pendingTimer !== undefined) {
      clearTimeout(pendingTimer);
      pendingTimer = undefined;
    }
  };

  /**
   * 这颗事件是不是刚刚那次上屏的**同一个物理按键**。「空格确认候选」时
   * `compositionend` 可能先于 keydown 到达，那时 `composing` 已翻回 false，
   * 只能靠时间判断。30ms：同一次按键的两条事件相隔几毫秒，而人手下一次按键
   * 最快也在 80ms 之外。
   */
  const sameAsComposeEnd = (ev: { timeStamp: number }): boolean =>
    ev.timeStamp - composeEndedTs < 30;

  const onComposeStart = (ev?: CompositionEvent) => {
    trace({ ev: "compstart", act: "state", data: ev?.data ?? undefined, comp: false, ta: taOf(internals.textarea) });
    // **不清账本**：WebKit 会给空格这类普通按键补一对 composition 事件，清掉
    // 就会让已发出的那一颗在随后的 input 里被当成「没发过」，变成两个空格。
    // 组合期间 `composing` 会让 input 当场返回，账本也不会被误用。
    composing = true;
    // 上一轮如果因为异常路径没消费掉，别让它带到这一轮
    imeConsumedKey = null;
    clearPending();
  };

  const onComposeEnd = (ev: CompositionEvent) => {
    trace({ ev: "compend", act: "state", data: ev.data ?? undefined, comp: true, ta: taOf(internals.textarea) });
    composing = false;
    composeEndedTs = ev.timeStamp;
    const consumed = imeConsumedKey;
    imeConsumedKey = null;
    const ta = internals.textarea;
    if (!ta || !consumed) return;

    // 那颗键只 `stopPropagation` 没 `preventDefault`（否则 IME 上不了屏），
    // 所以它可能照样被写进隐藏 textarea，而 `_finalizeComposition` 的
    // `substring(start)` 取到末尾，会把它跟着上屏文本一起发进 pty。
    // 以 `ev.data`（IME 真正提交的文本）为判据可避免误伤：上屏文本自己就以这个
    // 字符结尾（比如选了「第2」）时不碰它。
    const committed = ev.data ?? "";
    if (consumed === " " || consumed === "Spacebar") {
      // 空格是确认键，不可能属于上屏文本 —— 尾部有几个都剥掉
      if (ta.value.endsWith(" ")) ta.value = ta.value.replace(/ +$/, "");
      return;
    }
    if (committed.endsWith(consumed)) return;
    if (ta.value.endsWith(consumed)) {
      ta.value = ta.value.slice(0, ta.value.length - consumed.length);
    }
  };

  /**
   * 清掉隐藏 textarea 的残留：xterm 只在 `_keyDown` 里清它，而我们把这类键整条
   * 拦在捕获阶段，这条路上就没人清了。WebKit 在 keyCode 229 时会无视
   * `preventDefault` 把字符插进去，攒着的脏字符最终会被 `_handleAnyTextareaChanges`
   * 的 diff / `_finalizeComposition` 的范围算进去，漏进 pty。
   *
   * **只在确认是普通 ASCII 的路径上调用**：组合上屏时 xterm 要回头读 textarea
   * 算范围，提前清掉就是把刚上屏的中文抹了。
   */
  const clearTextarea = () => {
    const ta = internals.textarea;
    if (ta && ta.value) ta.value = "";
  };

  /** 我们替 xterm 把这颗键标成「已发过」，防止它再从 input 补一遍 */
  const markSent = () => {
    internals._keyDownSeen = true;
    internals._keyPressHandled = true;
  };

  const onKeyDown = (ev: KeyboardEvent) => {
    // 中文输入法「智能全角标点」：必须排在最前面、且**不看 `ev.isComposing`**——
    // 它在输入法上下文残留时会谎报 true，跟着走下面 `isImeKey` / `ev.isComposing`
    // 那几条分支的话，这颗键要么被当成「组合态里的确认键」吞掉、要么提前 return，
    // 根本轮不到标点处理。真实组合状态只认 `composing`。
    if (
      !composing &&
      !ev.ctrlKey && !ev.altKey && !ev.metaKey &&
      PUNCT_RISK.has(ev.key)
    ) {
      // 不能 preventDefault：要让 WebKit 按它自己的判断正常插入（原样或替换成
      // 全角），我们只负责别让 xterm 也掺和进来抢着发一遍。
      ev.stopPropagation();
      // 刚刚才有 insertText 交出过字符 → 这颗键的结果已经发出去了，绝不再补。
      // 少了这一句，「input 先到」的机型上每敲一个标点都会多出一个字符。
      if (nowMs() - lastTextInputAt < 200) {
        trace({ ev: "keydown", act: "drop", key: ev.key, kc: ev.keyCode, code: ev.code, mods: modsOf(ev), ic: ev.isComposing, comp: composing, ta: taOf(internals.textarea) });
        return;
      }
      clearPending();
      pendingPunct = { key: ev.key, at: nowMs() };
      pendingTimer = setTimeout(() => {
        if (pendingPunct?.key === ev.key) {
          clearTextarea();
          // 80ms 都没等到 input：说明 WebKit 根本没替换（preventDefault 生效了
          // 或这颗键根本没进 textarea），那就按原始半角发，不能把它吞了。
          out(ev.key);
          pendingPunct = null;
        }
        pendingTimer = undefined;
      }, 80);
      trace({ ev: "keydown", act: "defer", key: ev.key, kc: ev.keyCode, code: ev.code, mods: modsOf(ev), ic: ev.isComposing, comp: composing, ta: taOf(internals.textarea) });
      return;
    }
    // 组合态里被 IME 吃掉的键：拦在这里，别让它走到 xterm 的 `_keyDown`。
    if (isImeKey(ev)) {
      if (composing || ev.isComposing) {
        // 组合还没结束：**不能 preventDefault**，上屏是 IME 的默认行为，拦掉会
        // 把候选词卡住。只 stopPropagation 掐断 xterm 那一路（否则空格多一个
        // 空格、选候选的数字变成「大2」）。代价是那颗键照样可能写进隐藏
        // textarea，所以记一笔留给 `onComposeEnd` 判定要不要剥掉。
        if (xtermWouldEmit(ev)) {
          ev.stopPropagation();
          markSent(); // 让 xterm 的 `_inputEvent` 也别去发上屏那一条（否则上屏两遍）
          imeConsumedKey = ev.key;
          trace({ ev: "keydown", act: "stop", key: ev.key, kc: ev.keyCode, code: ev.code, mods: modsOf(ev), ic: ev.isComposing, comp: composing, ta: taOf(internals.textarea) });
        } else {
          trace({ ev: "keydown", act: "pass", key: ev.key, kc: ev.keyCode, code: ev.code, mods: modsOf(ev), ic: ev.isComposing, comp: composing, ta: taOf(internals.textarea) });
        }
        return;
      }
      if (sameAsComposeEnd(ev)) {
        // 上屏已完成，这颗只是同一次物理按键的另一半。此时 preventDefault 只会
        // 拦住「插进 textarea」，动不了已上屏的字，可以放心拦死。
        // 不要再记 imeConsumedKey：那次 compositionend 已过去，记了只会留到
        // **下一次**上屏时才消费，把下一句结尾剥掉。
        ev.preventDefault();
        ev.stopPropagation();
        markSent();
        trace({ ev: "keydown", act: "stop", key: ev.key, kc: ev.keyCode, code: ev.code, mods: modsOf(ev), ic: ev.isComposing, comp: composing, ta: taOf(internals.textarea) });
        return;
      }
    }
    // 真·组合态：这颗键属于拼音，一个字节都不该由我们发，textarea 也不能动
    if (composing) {
      trace({ ev: "keydown", act: "pass", key: ev.key, kc: ev.keyCode, code: ev.code, mods: modsOf(ev), ic: ev.isComposing, comp: composing, ta: taOf(internals.textarea) });
      return;
    }
    // 浏览器自称在组合、我们却没收到 compositionstart（真组合的 compositionstart
    // 一定先过 host 的捕获阶段）：输入法上下文残留。这里的字母交给 xterm 会因
    // 229 丢掉，但 `onInput` 那条兜底刻意不看 isComposing，会补发出去。
    if (ev.isComposing) {
      trace({ ev: "keydown", act: "pass", key: ev.key, kc: ev.keyCode, code: ev.code, mods: modsOf(ev), ic: ev.isComposing, comp: composing, ta: taOf(internals.textarea) });
      return;
    }
    if (!isPlainKeystroke(ev)) {
      trace({ ev: "keydown", act: "pass", key: ev.key, kc: ev.keyCode, code: ev.code, mods: modsOf(ev), ic: ev.isComposing, comp: composing, ta: taOf(internals.textarea) });
      return;
    }
    // 拦死默认行为：不产生 keypress / beforeinput，也不让 xterm 的 _keyDown 跑
    ev.preventDefault();
    ev.stopPropagation();
    // 清 textarea 只做「整段清空」，不按长度差：`_handleAnyTextareaChanges` 会把
    // 长度变短判成退格（发 DEL 给 shell），那等于亲手删掉一个字符。
    const taBefore = taOf(internals.textarea);
    markSent();
    clearTextarea();
    const fresh = ledger.keyDown(ev.key, nowMs());
    if (fresh) out(ev.key); // fresh=false 说明 input 已经先发过这一颗
    trace({ ev: "keydown", act: fresh ? "send" : "drop", key: ev.key, kc: ev.keyCode, code: ev.code, mods: modsOf(ev), ic: ev.isComposing, comp: composing, ta: taBefore, sent: fresh ? ev.key : undefined, via: fresh ? "mac" : undefined });
  };

  const onInput = (ev: InputEvent) => {
    // 有半角标点键在等：不管这条 data 是原样还是全角，都是它唯一、最终的结果，
    // 发这一条就够，xterm 不该再碰（否则非 ASCII 会被当成组合上屏再发一遍）。
    if (pendingPunct && !composing && ev.data) {
      clearPending();
      ev.stopPropagation();
      clearTextarea();
      lastTextInputAt = nowMs();
      out(ev.data);
      trace({ ev: "input", act: "send", it: ev.inputType, data: ev.data, ic: ev.isComposing, comp: composing, ta: taOf(internals.textarea), sent: ev.data, via: "mac" });
      return;
    }
    if (ev.inputType === "insertText" && ev.data) lastTextInputAt = nowMs();
    // 刻意不看 `ev.isComposing`：输入法上下文残留时它会谎报 true，而那正是丢字
    // 发生的那一刻。是否在组合只以我们自己的 compositionstart/end 为准。
    const seen: TraceRec = { ev: "input", act: "pass", it: ev.inputType, data: ev.data ?? undefined, ic: ev.isComposing, comp: composing, ta: taOf(internals.textarea) };
    if (composing) {
      trace(seen);
      return;
    }
    // 上屏刚过去就到的 insertText，是同一次 IME 操作的尾巴（选候选那颗键被
    // WebKit 顺手当成普通输入派发出来的），不是新的输入 —— 发出去就成了「大2」。
    if (sameAsComposeEnd(ev)) {
      seen.act = "drop";
      trace(seen);
      return;
    }
    if (ev.inputType !== "insertText") {
      trace(seen); // 粘贴 / 组合上屏不碰
      return;
    }
    const data = ev.data;
    if (!data) {
      trace(seen);
      return;
    }
    // 含非 ASCII 就整条不碰：中文 / emoji 上屏要走 xterm 的组合路径，
    // 连 textarea 都不能清（见 clearTextarea）。
    for (const ch of data) {
      if (!isPlainChar(ch)) {
        trace(seen);
        return;
      }
    }

    // 普通打字一律由我们定夺：先拦住 xterm 的 _inputEvent（它此时多半会因
    // `_keyDownSeen` 把整条丢掉）。
    ev.stopPropagation();
    const taBefore = taOf(internals.textarea);
    clearTextarea();

    // **一颗键只该产生一个字符。** 多字符的 insertText 都不是「一颗键」
    //（macOS 双空格变 ". "、WebKit 合并快打、输入法上屏整个单词），它们的
    // keydown 早就各自发过了，逐字重放只会多出字节："  " → ". " → "  . "。
    const rest = data.length === 1 ? ledger.input(data, nowMs()) : "";
    const ok = rest !== "";
    if (ok) out(rest);
    trace({ ev: "input", act: ok ? "send" : "drop", it: ev.inputType, data, ic: ev.isComposing, comp: composing, ta: taBefore, sent: ok ? rest : undefined, via: ok ? "mac" : undefined });
  };

  /** 只记不看：把「我们没接的那些事件」也留下来，才知道漏了什么 */
  const onPassive = (ev: Event) => {
    if (!traceOn()) return;
    if (ev instanceof KeyboardEvent) {
      trace({ ev: ev.type, act: "pass", key: ev.key, kc: ev.keyCode, code: ev.code, mods: modsOf(ev), ic: ev.isComposing, comp: composing, ta: taOf(internals.textarea) });
    } else if (ev.type === "compositionupdate") {
      trace({ ev: "compupdate", act: "pass", data: (ev as CompositionEvent).data ?? undefined, comp: composing, ta: taOf(internals.textarea) });
    } else if (ev.type === "beforeinput") {
      const ie = ev as InputEvent;
      trace({ ev: "beforeinput", act: "pass", it: ie.inputType, data: ie.data ?? undefined, ic: ie.isComposing, comp: composing, ta: taOf(internals.textarea) });
    }
  };

  host.addEventListener("keydown", onKeyDown, true);
  // lib.dom 把 "input" 的事件类型标成 Event（没有 data/isComposing），显式转一次
  host.addEventListener("input", onInput as unknown as EventListener, true);
  host.addEventListener("compositionstart", onComposeStart, true);
  host.addEventListener("compositionend", onComposeEnd, true);
  // 下面这几个纯观测：不拦、不 preventDefault，只是把现场补完整
  host.addEventListener("keyup", onPassive, true);
  host.addEventListener("beforeinput", onPassive, true);
  host.addEventListener("compositionupdate", onPassive, true);

  return () => {
    host.removeEventListener("keydown", onKeyDown, true);
    host.removeEventListener("input", onInput as unknown as EventListener, true);
    host.removeEventListener("compositionstart", onComposeStart, true);
    host.removeEventListener("compositionend", onComposeEnd, true);
    host.removeEventListener("keyup", onPassive, true);
    host.removeEventListener("beforeinput", onPassive, true);
    host.removeEventListener("compositionupdate", onPassive, true);
    ledger.clear();
    clearPending();
  };
}
