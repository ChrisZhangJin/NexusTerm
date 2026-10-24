import type { Terminal } from "@xterm/xterm";

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
 * 现场记录：每一颗「我们决定发 / 决定不发」的字节连上下文一起存进环形缓冲。
 *
 * 只在有人手动打开开关时才写，平时一行都不记：
 *
 * ```js
 * globalThis.__NX_TRACE_INPUT = true      // 然后复现那几秒
 * JSON.stringify(globalThis.__nxInputLog.slice(-40))
 * ```
 *
 * 留这个口子的原因：丢字、多字、多空格这三类症状在用户机器上都只闪一下，
 * 靠截图和口头复述永远只能猜。有这份记录就能直接看出是「keydown 发了两遍」
 * 还是「input 补发了一遍」。
 */
type TraceRec = {
  t: number;
  src: "key" | "input";
  /** keydown 的 ev.key，或 input 的 ev.data */
  ch: string;
  keyCode?: number;
  isComposing?: boolean;
  composing: boolean;
  /** 清空之前的 textarea 残留（非空就说明 ② 正在发生） */
  ta?: string;
  sent: boolean;
};

const traceOn = (): boolean =>
  (globalThis as { __NX_TRACE_INPUT?: boolean }).__NX_TRACE_INPUT === true;

function trace(rec: Omit<TraceRec, "t">): void {
  if (!traceOn()) return;
  const g = globalThis as { __nxInputLog?: TraceRec[] };
  const log = (g.__nxInputLog ??= []);
  log.push({ t: nowMs(), ...rec });
  if (log.length > 500) log.splice(0, log.length - 500);
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

  // 组合状态由我们自己跟：`ev.isComposing` 在「输入法上下文残留」时是假的
  // （该 false 的时候 true、该 true 的时候 false），只有 compositionstart
  // 与 compositionend 这对事件说的是事实。
  let composing = false;
  /**
   * 组合期间按过确认键（空格/回车）。
   *
   * 用途只有一个：`compositionend` 时决定要不要把 textarea 尾部的半角空格剥掉。
   * 见 `onComposeEnd`。
   */
  let confirmDuringCompose = false;
  /** 最近一次 `compositionend` 的时间戳，用于识别「同一次按键的两半」 */
  let composeEndedTs = -Infinity;

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
  const sameAsComposeEnd = (ev: KeyboardEvent): boolean =>
    ev.timeStamp - composeEndedTs < 30;

  const onComposeStart = () => {
    // 只翻标志，**不清账本** —— 详见文件头「①」：
    // WebKit 会给空格这类普通按键补一对 composition 事件，此时清账本就会让
    // 已经发出去的那一颗在随后的 input 里被当成「没发过」，变成两个空格。
    // 组合期间不会有任何 input 走到发送逻辑（`composing` 当场返回），
    // 所以账本在组合期间根本不会被误用，没有任何理由清它。
    composing = true;
    // 上一轮如果因为异常路径没消费掉，别让它带到这一轮
    confirmDuringCompose = false;
  };

  const onComposeEnd = (ev: CompositionEvent) => {
    composing = false;
    composeEndedTs = ev.timeStamp;
    if (!confirmDuringCompose) return;
    confirmDuringCompose = false;

    // IME 用空格确认时，那颗空格**可能已经落进隐藏 textarea**（我们没法可靠地
    // preventDefault 掉它 —— 按键先被 IME 消费）。而 xterm 的
    // `_finalizeComposition(true)` 是在 setTimeout 里执行 `substring(start)`
    // ——**取到末尾**，于是「拼」后面那个空格会被一起发进 pty。
    //
    // 上屏文本不可能以半角空格结尾（那是确认键，不是词的一部分），所以只在
    // 「这一轮按过确认键」时把尾部空格剥掉。剥掉的只是显示缓冲里的影子文本，
    // 不影响已经上屏的字。
    const ta = internals.textarea;
    if (ta && ta.value.endsWith(" ")) ta.value = ta.value.replace(/ +$/, "");
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
    // 组合期间的确认键：拦在这里，别让它走到 xterm 的 `_keyDown`。
    // 详见 `isImeConfirmKey` —— 不拦的话「拼」上屏的同时会多一个空格。
    if (isImeConfirmKey(ev)) {
      if (composing || ev.isComposing) {
        // 组合还没结束：**不能 preventDefault** —— 上屏是 IME 的默认行为，
        // 拦掉会把候选词卡住。代价是那颗空格照样会写进隐藏 textarea，所以
        // 记一笔，等 compositionend 时把它剥掉（见 `onComposeEnd`）。
        ev.stopPropagation();
        markSent(); // 让 xterm 的 `_inputEvent` 也别去发上屏那一条（否则上屏两遍）
        confirmDuringCompose = true;
        return;
      }
      if (sameAsComposeEnd(ev)) {
        // 上屏已经完成，这颗只是同一次物理按键的另一半。这时候
        // preventDefault 只会拦住「把空格插进 textarea」，动不了已经上屏的字，
        // 所以可以放心拦死 —— 不留残留，也就没有后续 substring 带走它的问题。
        // 不要再置 confirmDuringCompose：那次 compositionend 早就过去了，
        // 置了只会留到**下一次**上屏时才消费，把下一句结尾剥掉。
        ev.preventDefault();
        ev.stopPropagation();
        markSent();
        return;
      }
    }
    // 真·组合态：这颗键属于拼音，一个字节都不该由我们发，textarea 也不能动
    if (composing) return;
    // 浏览器自称在组合、我们却没收到 compositionstart：输入法上下文残留。
    // 真组合的 compositionstart 一定先经过 host 的捕获阶段，收不到就说明它
    // 已经结束了 —— 这里的字母交给 xterm 会因 229 丢掉，但 `onInput` 那条
    // 兜底**刻意不看 isComposing**，会把它补发出去。所以这里只 return。
    if (ev.isComposing) return;
    if (!isPlainKeystroke(ev)) return;
    // 拦死默认行为：不产生 keypress / beforeinput，也不让 xterm 的 _keyDown 跑
    ev.preventDefault();
    ev.stopPropagation();
    // 刻意**不在这里按长度差判断**：`_handleAnyTextareaChanges` 会拿长度变短
    // 判成一次退格（发 DEL 给 shell），那等于亲手删掉一个字符。清除动作只做
    // 「整段清空」，而且是在我们自己接管的分支里 —— xterm 自己清也是这么做的。
    const taBefore = traceOn() ? internals.textarea?.value ?? "" : "";
    markSent();
    clearTextarea();
    const fresh = ledger.keyDown(ev.key, nowMs());
    if (fresh) send(ev.key); // input 已经先发过这一颗
    if (traceOn()) {
      trace({ src: "key", ch: ev.key, keyCode: ev.keyCode, isComposing: ev.isComposing, composing, ta: taBefore, sent: fresh });
    }
  };

  const onInput = (ev: InputEvent) => {
    // 只有「真的在组合」才放手。这里**刻意不看 `ev.isComposing`** ——
    // 输入法上下文残留时它会谎报 true，而那正是丢字发生的那一刻；
    // 是不是组合以我们自己的 compositionstart/end 为准。
    if (composing) return;
    if (ev.inputType !== "insertText") return; // 粘贴 / 组合上屏不碰
    const data = ev.data;
    if (!data) return;
    // 含非 ASCII 就整条不碰：中文 / emoji 上屏要走 xterm 的组合路径，
    // 连 textarea 都不能清（见 clearTextarea 的说明）。
    for (const ch of data) if (!isPlainChar(ch)) return;

    // 走到这里说明是普通打字。不管 xterm 会不会发，都由我们定夺：
    // 先拦住它的 _inputEvent（它此时多半会因 `_keyDownSeen` 把整条丢掉）。
    ev.stopPropagation();
    const taBefore = traceOn() ? internals.textarea?.value ?? "" : "";
    clearTextarea();

    // **一颗键只该产生一个字符。** 多字符的 insertText 都不是「一颗键」：
    //   · macOS「连按两下空格插入句号」把 "  " 换成 ". "（两个字符）；
    //   · 快速连打时 WebKit 会把几颗键合并成一条 input；
    //   · 输入法上屏一个英文单词。
    // 这些字符的 keydown 早就各自发过了，再按字符重放只会多出字节 ——
    // 空格这一条最明显：系统把两个空格变成 ". "，重放就成了 "  . "。
    const rest = data.length === 1 ? ledger.input(data, nowMs()) : "";
    const ok = rest !== "";
    if (ok) send(rest);
    if (traceOn()) {
      trace({ src: "input", ch: data, isComposing: ev.isComposing, composing, ta: taBefore, sent: ok });
    }
  };

  host.addEventListener("keydown", onKeyDown, true);
  // `input` 的事件类型在 lib.dom 里是 Event（只有 InputEvent 才有 data/isComposing），
  // 这里显式转一次；运行时拿到的确定是 InputEvent。
  host.addEventListener("input", onInput as unknown as EventListener, true);
  host.addEventListener("compositionstart", onComposeStart, true);
  host.addEventListener("compositionend", onComposeEnd, true);

  return () => {
    host.removeEventListener("keydown", onKeyDown, true);
    host.removeEventListener("input", onInput as unknown as EventListener, true);
    host.removeEventListener("compositionstart", onComposeStart, true);
    host.removeEventListener("compositionend", onComposeEnd, true);
    ledger.clear();
  };
}
