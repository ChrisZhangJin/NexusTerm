import type { Terminal } from "@xterm/xterm";
import { api } from "../api";

/**
 * 修 macOS(WKWebView) 上「快速连打偶发丢一个字」。
 *
 * 现象：输入 `config` 得到 `cnfig`，只在 mac 版、只在打得快时出现。
 *
 * ## 前两轮修错了地方
 *
 * 之前两版都盯着 `_keyPress` / `_inputEvent` 的去重标志（`_keyDownSeen` /
 * `_keyPressHandled`），假设「字符是从 keypress 或 input 出去的」。把 xterm
 * 5.5 的 `_keyDown` 完整读一遍才发现前提就错了：
 *
 * ```js
 * // _keyDown 里，evaluateKeyboardEvent 对「keyCode>=48 的单字符」直接
 * // 返回 o.key = e.key，于是 keydown 这一层就已经把字符发出去了：
 * this.coreService.triggerDataEvent(i.key, true);
 * ...
 * this.cancel(e, true);        // ← 顺带 preventDefault
 * ```
 *
 * 也就是说**普通字符走的是 keydown，keypress 与 input 根本不会发生**
 * （preventDefault 之后浏览器不再派发它们）。那两版包装的 `_keyPress` 是
 * 死代码，`_inputEvent` 在正常打字时也永远进不来 —— 这就是「改了两次还是丢」。
 *
 * ## 真正的丢字在这条缝里
 *
 * `_keyDown` 开头的顺序是：
 *
 * ```js
 * this._keyDownSeen = true;                        // ① 先置位
 * if (!this._compositionHelper.keydown(e)) return; // ② 可能在这里提前返回
 * ```
 *
 * 而 `CompositionHelper.keydown` 对 **keyCode 229** 一律返回 false：
 *
 * ```js
 * return 229 !== e.keyCode || (this._handleAnyTextareaChanges(), false);
 * ```
 *
 * WebKit 在「输入法上下文还热着」的时候（刚用过中文输入法、或 Shift 切的
 * 英文模式），会把普通字母的 keydown 也标成 229。于是：
 *
 * 1. ①把 `_keyDownSeen` 置成 true，② 立刻返回 —— 这颗键**什么都没发**；
 * 2. 浏览器照常把字符塞进隐藏 textarea，派发 `input`；
 * 3. `_inputEvent` 的第一道门是 `(!e.composed || !this._keyDownSeen)` ——
 *    composed 为 true、`_keyDownSeen` 又是 true，**整条 input 被丢掉**；
 * 4. 这些 input 往往还带着 `isComposing: true`（输入法上下文没退干净），
 *    于是上一版守卫里 `!ev.isComposing` 那道判断也拦不到它。
 *
 * 一个字节都没进 pty，屏幕上自然少一个字母 —— 而且只在「输入法刚被用过」
 * 的窗口里出现，正好对上「偶尔」。
 *
 * ## 所以不再修 xterm，直接接管
 *
 * 在容器上以**捕获阶段**拦 keydown（早于 xterm 挂在 textarea 上的监听），
 * 对「单个可打印 ASCII、无修饰键、且当下没有真的在组合」的按键：
 *
 *   - `preventDefault` + `stopPropagation`：浏览器不再产生 keypress / input，
 *     xterm 的 `_keyDown` 也不会跑 —— 那颗键彻底不经过它的三条路径；
 *   - 由我们直接 `send(ev.key)`。
 *
 * 再补一个 input 兜底：万一 WebKit 无视 preventDefault 仍然派发了 input，
 * 我们也在捕获阶段接住（否则 xterm 会因为 `_keyDownSeen` 把它丢掉）。
 *
 * 两条路都指向同一个 `send`，所以必须有账本去重 —— 见 `KeyLedger`。
 *
 * ## 第二轮：修完丢字，多出来了空格
 *
 * 丢字确实没了，但开始「偶尔多一个空格」。去重账本本身没问题，问题出在
 * **清账的时机**和**textarea 没人扫**这两件事上：
 *
 * ### ① `compositionstart` 清空账本 → 一颗空格发两遍
 *
 * 上一版在 `compositionstart` 里 `ledger.clear()`，理由是「组合开始前发出去的
 * 字符，不该被组合期间的 input 认领」。但 WebKit 在输入法上下文还热着的时候，
 * 会给**普通按键**补一对 compositionstart / compositionend（尤其空格 —— 它
 * 本来就是输入法的上屏键）。于是：
 *
 * 1. 敲空格 → `keydown` 里 `send(" ")`，账本记下这一颗；
 * 2. `compositionstart` 到了 → **账本被清空**，那颗 " " 的记录没了；
 * 3. `compositionend`；
 * 4. WebKit 照旧派发 `input`（data=" ", isComposing=true）→ 账本空白 →
 *    认领失败 → `send(" ")` 第二遍。
 *
 * 账本存在的意义就是「等那条可能永远不来的 input」，组合开始时把它清掉，
 * 等于把最需要配对的那几颗键直接放生。而「组合期间的 input 误认领」这个
 * 担心根本不成立 —— `composing` 为真时 `onInput` 第一行就返回了。
 *
 * ### ② 接管的键不再经过 xterm 的 `_keyDown`，textarea 就没人清了
 *
 * `_keyDown` 里有一句 `this.textarea.value = ""`（除 ETX / CR 外都清）。我们
 * 把整条 keydown 拦在捕获阶段，这句也就不执行了 —— **在我们接管的路上，
 * textarea 从不清空**。而 WebKit 遇到 keyCode 229 会无视 `preventDefault`
 * 把字符插进去，于是一点点攒着。等下一次真的走组合路径，
 * `_handleAnyTextareaChanges` 拿 `newValue.replace(oldValue, "")` 算 diff、
 * `_finalizeComposition` 拿 `textarea.value.substring(start)` 算上屏范围 ——
 * 那段攒下来的脏字符串就从这儿漏进 pty。
 *
 * ### ③ 多字符的 `insertText` 被逐字重放
 *
 * `input` 的 data 不总是「一颗键」。macOS 的「连按两下空格插入句号」会把
 * `"  "` 替换成 `". "` 再派发一条两字符的 input；快速连打时 WebKit 也可能
 * 把几颗键合并成一条。上一版把 data 逐字符过账本再补发，`". "` 里的 "." 没
 * 被认领 → 补发一个 "."，屏幕上就成了三个字符。这类 data 对应的 keydown
 * 早就各自发过了，**整条丢掉才是对的**。
 *
 * ## 第三轮：选候选的数字也被当成输入发了出去
 *
 * 和空格同源：`CompositionHelper.keydown` 只认 229，而选候选的「2」keyCode 是
 * 50，于是「da」+「2」变成「大2」。组合一旦开着，键盘就不归 shell 管了 ——
 * 拼音字母进预编辑、数字选候选、`, . - =` 翻页、空格 / 回车确认，**任何可打印
 * 键都属于 IME**。所以判据从「确认键」扩成「组合态里被 IME 吃掉的键」
 * （`isImeKey`），只有 229 那类放行 —— xterm 自己会对它们 return false，而且
 * 顺带调 `_handleAnyTextareaChanges()` 刷新预编辑，抢过来反而让「da」不显示。
 *
 * 拦的方式只有 `stopPropagation`，**不能 preventDefault**：默认行为是 IME 选词
 * 上屏，拦了候选就卡住。代价是那颗键可能仍被写进隐藏 textarea，而
 * `_finalizeComposition(true)` 是 `substring(start)` 取到末尾 —— 所以在
 * `compositionend` 时按 `ev.data`（IME 真正提交上去的文本）判定要不要把尾部
 * 那个同款字符剥掉：上屏文本自己就以它结尾时（选了「第2」）不动。
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
 * 这颗 keydown 该不该由我们接管。
 *
 * 判据只有四条，且都朝「宁可不接管」的方向收紧：
 *
 * 1. **无 Ctrl / Alt / ⌘** —— 组合键由 xterm 发转义序列，Option 组合出的
 *    é / 死键也在这条路上，碰了就是两份输入；
 * 2. **`ev.key` 恰好一个字符** —— 真正的输入法按键 `key` 是 `"Process"` /
 *    `"Dead"` / `"Unidentified"`，长度不为 1，天然被排除；
 * 3. **该字符是可打印 ASCII** —— 中文、emoji 一律留给 xterm 的组合路径；
 * 4. **当下没有真的在组合**（`composing` 由 compositionstart/end 亲自跟踪）。
 *
 * 注意这里**不看 `keyCode === 229`**：229 恰恰是丢字现场的特征（输入法上下文
 * 残留），把它排除掉就等于把要修的那一类键又交回给会丢字的路径。真正的组合
 * 由 `composing` 拦住 —— 那是事实，229 只是浏览器的心情。
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
 * 输入法的「确认键」：空格选词、回车选词。
 *
 * 这两个键在组合态里属于 IME，一颗字节都不该进 shell。但 WKWebView 给空格的
 * `keyCode` 是 **32** 而不是 229，而 xterm 的 `CompositionHelper.keydown` 只对
 * 229 提前返回：
 *
 * ```js
 * keydown(e) {
 *   if (this._isComposing || this._isSendingComposition) {
 *     if (229 === e.keyCode) return false;        // ← 拼音字母从这儿出去
 *     if (16 === e.keyCode || 17 === ... ) return false;
 *     this._finalizeComposition(false);           // ← 空格落到这儿
 *   }
 *   return 229 !== e.keyCode || (...);            // ← 32 !== 229 于是返回 true
 * }
 * ```
 *
 * `keydown` 返回 true，`_keyDown` 就继续走到末尾的 `triggerDataEvent(i.key)` ——
 * 中文照常上屏，**后面还多出一个空格**。这就是「pin + 空格 → 拼 」的来历。
 */
export function isImeConfirmKey(ev: { key: string }): boolean {
  return ev.key === " " || ev.key === "Spacebar" || ev.key === "Enter";
}

/**
 * 组合态里**被 IME 吃掉**的键：拼音字母、选候选的数字（1–9）、翻页用的
 * `, . - =`、确认用的空格 / 回车。
 *
 * 判据刻意宽：组合一旦开着，键盘就不归 shell 管了 —— 任何可打印键都是先给
 * 输入法，输入法决定它是进预编辑、选候选，还是上屏。交给 xterm 的唯一后果是
 * 它把「选候选的那个 2」当成真输入发进 pty：`da` + `2` 得到「大2」。
 *
 * Enter 不是可打印字符，单独列出来（它同样是 IME 的确认键）。
 */
export function isImeKey(ev: { key: string }): boolean {
  return ev.key === "Enter" || isPlainChar(ev.key);
}

/**
 * xterm 会不会把这颗键**当成真输入发出去**。
 *
 * `CompositionHelper.keydown` 只对 229 提前返回，所以非 229 的可打印键在
 * 组合态里都会被 `_keyDown` 走到 `triggerDataEvent` —— 那是我们要拦的对象。
 *
 * 229 那类（拼音字母）xterm 自己会 `return false`，而且它顺带调一句
 * `_handleAnyTextareaChanges()` 刷新预编辑，**拦了反而可能让「da」敲出来不
 * 显示**，所以放行。
 *
 * 确认键无条件算「会发出去」：空格 / 回车的 keyCode 在 WKWebView 上有时候是
 * 229、有时候不是，两头都得拦（见 `isImeConfirmKey`）。
 */
export function xtermWouldEmit(ev: { key: string; keyCode?: number }): boolean {
  return isImeConfirmKey(ev) || (isPlainChar(ev.key) && ev.keyCode !== 229);
}

/**
 * 「已经发过」的账本。
 *
 * 原因：同一颗字符可能从 keydown 和 input 两条路各来一次，先后顺序在
 * WKWebView 上不保证（WebKit bug 165004：beforeinput 可能早于 keydown）。
 * 谁先到谁发，后到的那个在账本里认领掉自己，不再发第二遍。
 *
 * 必须带有效期：一颗键发出去却永远没有回声是常态（大部分时候浏览器根本
 * 不会派发 input）。账本项要是永不过期，十秒后用户用听写输入一个同样的
 * 字母就会被误认领 —— 变成新的丢字。
 *
 * 上限 8 条同理：快打时账本会短暂堆积，堆积到几十条说明两边根本不配对，
 * 这时候清空比继续猜更安全。
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
 * 现场记录：每一条输入事件（连同我们当时做了什么、最终有没有发字节）进环形缓冲。
 *
 * 只在有人手动打开开关时才写，平时一行都不记：
 *
 * ```js
 * globalThis.__NX_TRACE_INPUT = true      // 然后复现那几秒
 * JSON.stringify(globalThis.__nxInputLog.slice(-40))
 * ```
 *
 * 留这个口子的原因：丢字、多字、多空格、标点重复这四类症状在用户机器上都只闪
 * 一下，靠截图和口头复述永远只能猜 —— 前几轮就是这么连着猜错了三次。有这份
 * 记录就能直接看出是「keydown 发了两遍」还是「input 补发了一遍」。
 *
 * **经历了几次失败后这里刻意改成「全量记」**：不只记我们发了什么，也记我们
 * **没**接的那些事件（pass / drop），以及每条事件的 `keyCode`、`isComposing`、
 * textarea 内容。因为真正判断错的一直是「WKWebView 到底派了哪些事件」，
 * 只记自己发出的字节永远看不出漏在哪。
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
 * 把缓冲里的日志**落盘**，好让现场那几秒能被事后读到。
 *
 * 写文件这件事前端做不了（没有 fs 插件），借用的是 ZMODEM 下载那条
 * `append_file_b64` —— 它本来就是「前端攒一段、后端追加写」的形状，完全够用。
 *
 * 开关关着的时候一个字节都不写：终端里敲的东西包含密码，没有明确开记录就不该
 * 落到磁盘上。
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
 * @param on   开就记
 * @param path 落盘路径；给了就每 400ms 追加一次。不给只留在内存里
 *             （`globalThis.__nxInputLog`）。
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
  /**
   * 本轮组合里被 IME 吃掉的那颗键（`isImeKey` 且被我们拦下的）。
   *
   * 用途只有一个：`compositionend` 时决定要不要把 textarea 尾部的同款字符剥掉。
   * 见 `onComposeEnd`。
   */
  let imeConsumedKey: string | null = null;
  /** 最近一次 `compositionend` 的时间戳，用于识别「同一次按键的两半」 */
  let composeEndedTs = -Infinity;

  /**
   * 第四轮：中文输入法「智能全角标点」把 `,` `;` 这类键换成 `，` `；`，两个都发了出去。
   *
   * 这一版的判据是**照着真机日志定的**，前三版都是猜的 —— 猜错三次，三次构建
   * 出去都不对。真机（macOS + WKWebView + 系统拼音）敲一个中文逗号，
   * `<app_data>/logs/input.log` 里记到的实际顺序是：
   *
   * ```
   * beforeinput  insertText  data="，"  ic=false   ← 全角结果先到
   * input        insertText  data="，"  ic=false
   * send "，"    via=xterm                        ← xterm 的 _inputEvent 发了一遍
   * keydown      key="，"  kc=229  code=Comma     ← 键事件**后到**，key 已是全角
   * keyup        key="，"  kc=188  code=Comma
   * ```
   *
   * 两个事实推翻了前三版的假设：
   *
   * 1. **`input` 比 `keydown` 先到**（WebKit 的老毛病，165004）。所以「按下键 →
   *    等 input 来了再发」这类写法，在 input 先到时根本等不到，只会启动兜底
   *    定时器再补发一次 —— 「，,」里第二个字符就是**我们自己**补的。
   * 2. **`ev.key` 是被输入法替换后的全角字符，`keyCode` 是 229**（keyup 才是
   *    188）。按半角 `,` 去比对永远命中不了，9d4d3aa / 8fbcd26 改的都是空门。
   *
   * 结论反而很朴素：**这类键我们最好什么都别做**。全角结果由 `input` 事件送到，
   * xterm 的 `_inputEvent` 会把它发出去且只发一次；我们的接管层只要不插手
   * （`ev.key` 是非 ASCII 时 `isPlainKeystroke` 直接为假、`PUNCT_RISK` 也命中
   * 不了全角字符），就不会有第二份。真正要防的是**我们自己多发的那一次**。
   *
   * 所以下面这条分支只在「`ev.key` 还是半角」时生效（某些机型/某些 WebKit 给
   * 原始键码 188 配半角 key）：那时 `ev.key` 还不是最终结果，先不忙发，等
   * WebKit 写定的那条 `input`；若 80ms 内真没有 input 来，说明这颗键没被替换，
   * 再按原样补发 —— 不补就变成吞字。
   */
  const PUNCT_RISK = new Set([",", ".", ";", ":", "'", '"', "!", "?", "(", ")", "[", "]", "<", ">", "$", "\\", "^", "_", "`", "~"]);
  let pendingPunct: { key: string; at: number } | null = null;
  let pendingTimer: number | undefined;
  /**
   * 最近一次「WebKit 真的往 textarea 里插了字」的时刻。
   *
   * 半角标点那颗键要等 input 才能知道最终结果，但 input **可能已经先到了**
   * （见上面的真机日志）。这种情况下再去等一次、再补发一次，就是第二个字符的
   * 来历。所以只要刚刚（200ms 内）有过一次 insertText，就认定这颗键的结果
   * 已经交出去了，接管层彻底闭嘴。
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
   * 这颗键是不是刚刚那次上屏的**同一个物理按键**。
   *
   * WebKit 上「空格确认候选」有两种到达顺序：
   *
   * - keydown 先到：`composing` 还是 true，靠它拦；
   * - `compositionend` 先到：`composing` 已经翻回 false，只能靠时间。
   *
   * 同一次按键的两条事件几乎同时（几毫秒内），而人手下一次按键最快也在 80ms
   * 之外 —— 30ms 这个窗口足够把前者包住、又碰不到后者。
   */
  const sameAsComposeEnd = (ev: { timeStamp: number }): boolean =>
    ev.timeStamp - composeEndedTs < 30;

  const onComposeStart = (ev?: CompositionEvent) => {
    trace({ ev: "compstart", act: "state", data: ev?.data ?? undefined, comp: false, ta: taOf(internals.textarea) });
    // 只翻标志，**不清账本** —— 详见文件头「①」：
    // WebKit 会给空格这类普通按键补一对 composition 事件，此时清账本就会让
    // 已经发出去的那一颗在随后的 input 里被当成「没发过」，变成两个空格。
    // 组合期间不会有任何 input 走到发送逻辑（`composing` 当场返回），
    // 所以账本在组合期间根本不会被误用，没有任何理由清它。
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

    // 那颗键我们只 `stopPropagation`、**没有** `preventDefault`（拦默认行为会
    // 让 IME 上不了屏），所以它可能照样被 WebKit 写进了隐藏 textarea。而 xterm
    // 的 `_finalizeComposition` 是 `substring(start)` —— 一直取到末尾，多余的
    // 那个字符就会跟着上屏文本一起发进 pty。
    //
    // `ev.data` 是 IME 真正提交上去的文本，拿它当判据就不用担心误伤：
    // 上屏文本自己就以这个字符结尾（比如选了「第2」）时不碰它。
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
   * 清掉隐藏 textarea 里的残留。
   *
   * xterm 只在 `_keyDown` 里清它，而我们把这类键的 keydown 整条拦在捕获阶段
   * —— 也就是说**在我们接管的这条路上，textarea 永远不会被清**。WebKit 在
   * keyCode 229 时会无视 `preventDefault` 把字符插进去，一点点攒着；等到下次
   * 真走组合路径，`_handleAnyTextareaChanges` 的 diff、`_finalizeComposition`
   * 的上屏范围都是拿这段字符串算的，多出来的字节就从这儿漏进 pty。
   *
   * **只在确认是普通 ASCII 的路径上调用**：组合上屏时 xterm 要回头读 textarea
   * 算范围（`_finalizeComposition` 里的 `substring`），提前清掉就是把刚上屏的
   * 中文抹了。
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
    // 和丢字那条 bug 同源，这个状态在输入法上下文残留时会谎报 true，这里要是
    // 跟着走下面 `isImeKey`/`ev.isComposing` 那几条分支，要么被当成"组合态里的
    // 确认键"直接吞掉、要么在 517 行提前 return，都轮不到下面处理标点的代码。
    // 真实有没有在组合只认 `composing`（由 compositionstart/end 亲自维护）。
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
        // 组合还没结束：**不能 preventDefault** —— 上屏是 IME 的默认行为，
        // 拦掉会把候选词卡住。只 stopPropagation 掐断 xterm 那一路：
        //   · 空格 / 回车（确认键）不拦就多一个空格，见 `isImeConfirmKey`；
        //   · 数字（选候选的 1–9）同样不拦就多一个数字 —— 「da」+「2」变成「大2」。
        // 代价是那颗键照样可能写进隐藏 textarea，所以记一笔，等 compositionend
        // 时按 `ev.data` 判定要不要剥掉（见 `onComposeEnd`）。
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
        // 上屏已经完成，这颗只是同一次物理按键的另一半。这时候
        // preventDefault 只会拦住「把字符插进 textarea」，动不了已经上屏的字，
        // 所以可以放心拦死 —— 不留残留，也就没有后续 substring 带走它的问题。
        // 不要再记 imeConsumedKey：那次 compositionend 早就过去了，记了只会
        // 留到**下一次**上屏时才消费，把下一句结尾剥掉。
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
    // 浏览器自称在组合、我们却没收到 compositionstart：输入法上下文残留。
    // 真组合的 compositionstart 一定先经过 host 的捕获阶段，收不到就说明它
    // 已经结束了 —— 这里的字母交给 xterm 会因 229 丢掉，但 `onInput` 那条
    // 兜底**刻意不看 isComposing**，会把它补发出去。所以这里只 return。
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
    // 刻意**不在这里按长度差判断**：`_handleAnyTextareaChanges` 会拿长度变短
    // 判成一次退格（发 DEL 给 shell），那等于亲手删掉一个字符。清除动作只做
    // 「整段清空」，而且是在我们自己接管的分支里 —— xterm 自己清也是这么做的。
    const taBefore = taOf(internals.textarea);
    markSent();
    clearTextarea();
    const fresh = ledger.keyDown(ev.key, nowMs());
    if (fresh) out(ev.key); // input 已经先发过这一颗
    trace({ ev: "keydown", act: fresh ? "send" : "drop", key: ev.key, kc: ev.keyCode, code: ev.code, mods: modsOf(ev), ic: ev.isComposing, comp: composing, ta: taBefore, sent: fresh ? ev.key : undefined, via: fresh ? "mac" : undefined });
  };

  const onInput = (ev: InputEvent) => {
    // 刚才有一颗「可能被全角标点替换」的键在等——不管这条 input 的 data 是
    // 原样（没被替换）还是全角（被替换了），都是它唯一、最终的结果，发这一条
    // 就够，xterm 不该再碰（它没拦住的话，非 ASCII 会被当成中文组合上屏再发一遍）。
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
    // 只有「真的在组合」才放手。这里**刻意不看 `ev.isComposing`** ——
    // 输入法上下文残留时它会谎报 true，而那正是丢字发生的那一刻；
    // 是不是组合以我们自己的 compositionstart/end 为准。
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
    // 连 textarea 都不能清（见 clearTextarea 的说明）。
    // ⚠️ 这条正是「，,」的嫌疑人之一：全角标点那条 input 从这里整条放行给
    // xterm，而我们已经（或即将）自己发过一次。日志里它会是 act=pass + 非
    // ASCII 的 data，紧接着应该能看到一条 via=xterm 的 send。
    for (const ch of data) {
      if (!isPlainChar(ch)) {
        trace(seen);
        return;
      }
    }

    // 走到这里说明是普通打字。不管 xterm 会不会发，都由我们定夺：
    // 先拦住它的 _inputEvent（它此时多半会因 `_keyDownSeen` 把整条丢掉）。
    ev.stopPropagation();
    const taBefore = taOf(internals.textarea);
    clearTextarea();

    // **一颗键只该产生一个字符。** 多字符的 insertText 都不是「一颗键」：
    //   · macOS「连按两下空格插入句号」把 "  " 换成 ". "（两个字符）；
    //   · 快速连打时 WebKit 会把几颗键合并成一条 input；
    //   · 输入法上屏一个英文单词。
    // 这些字符的 keydown 早就各自发过了，再按字符重放只会多出字节 ——
    // 空格这一条最明显：系统把两个空格变成 ". "，重放就成了 "  . "。
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
  // `input` 的事件类型在 lib.dom 里是 Event（只有 InputEvent 才有 data/isComposing），
  // 这里显式转一次；运行时拿到的确定是 InputEvent。
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
