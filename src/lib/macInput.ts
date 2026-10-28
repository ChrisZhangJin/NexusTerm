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
 * 4. **标点重复**（英文输入法敲 `.` 出两个）：上面第 3 类的拦法是「只
 *    `stopPropagation` 不 `preventDefault`」，而 xterm 的 `keypress` 监听独立挂在
 *    textarea 上 —— 拦掉 keydown 拦不住它。它的自保标志 `_keyDownHandled` 恰恰是
 *    在被我们拦掉的 `_keyDown` 里置的，于是 xterm 照发一份。中文输入法下有
 *    composition 事件岔开，英文输入法下没有，每个标点必重复。见
 *    `keydownStoppedWithoutDefault`。
 *
 * 5. **组合永不结束 → 整个终端失聪**（`IM_GONE_KEYS` / `revive`）：组合进行到一半
 *    切走输入法（第三方输入法尤其容易），WebKit 这一焦点的 composition 再也不会
 *    end，于是之后每颗键都是 `keyCode 229 + isComposing`，两路人都不发 —— 一个
 *    字节都不出去，直到用户切走 app 再回来。这是「中文没问题、英文丢一大片」的
 *    根因之一：中文每轮组合都能正常收尾，出事的全是那些没收尾的会话。
 *
 * 上面四条（连同第 5 条）都是「猜 xterm 这一路会不会也发」，每条对应一次真机翻车。猜不完：
 * 发送方有两个（xterm 的 `onData` 和这一层），而 WKWebView 的事件序列按输入法
 * 变，换一种输入法就多一条没覆盖的路。所以**最后兜底的不是分支，是出口**——
 * 见 [`OnceGuard`]：一颗键只许放一个可见字符出去，谁先到算谁的。
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

/** 整串都是可打印 ASCII（输入法「原样交出」的字母 / 空格） */
function isAsciiRun(s: string | null | undefined): boolean {
  if (!s) return false;
  for (const ch of s) if (!isPlainChar(ch)) return false;
  return true;
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

/**
 * 单个「看得见的字符」：恰好一个码点，且不是控制字符。
 *
 * 转义序列（方向键）、`\r`、`\x03`、粘贴来的整段文本都不算 —— 那些**本来就该**
 * 原样放行，一个字节都不能去重。
 */
export function isSingleVisible(d: string): boolean {
  if (Array.from(d).length !== 1) return false;
  const c = d.codePointAt(0) ?? 0;
  return c >= 0x20 && c !== 0x7f;
}

/**
 * **一颗键只许产生一个可见字符。** 整个接管层唯一靠得住的不变量，也是最后一道闸。
 *
 * 前面那四条缺陷的修法都是「预测 xterm 这一路会不会也发一份」，而发送方有两个：
 * xterm 自己的 `onData`，和这一层。WKWebView 给每种输入法派的事件序列都不一样
 * （`input` 有时早于 `keydown`、组合事件有时凭空多一对、`keyCode` 时而 229），
 * 预测一定漏，于是「英文标点修好了换中文标点又重复」。
 *
 * 这里换个方向：不管字节是从哪条路来的，同一颗键里**同一个字符只放第一份过去**。
 * 账以 `keydown` 为界清空 —— 一次物理按键对应一颗 keydown，这是浏览器保证的，
 * 所以连打 `..` 或按住自动重复都不会被误吞（各自有各自的 keydown）。
 *
 * TTL 只是兜底，防的是「完全没有 keydown 的提交」（鼠标点候选词）把账留到下一次。
 *
 * 放行的判据刻意只有「单个可见字符」：多字符、控制字节、转义序列一律直通，
 * 这一层永远不会去动回车、Ctrl 组合、方向键和粘贴。
 *
 * **边界**：挡得住的是「重复那一份在下一颗 keydown 之前到」。`input` 早于
 * keydown 的那条真机路径上（WebKit bug 165004），迟到的 keydown 会把账清掉，
 * 之后再冒出来的重复就挡不住了。实际够用：那条路上 `onInput` 已经
 * `stopPropagation`，xterm 根本收不到 input，而迟到的 keydown 带着 keyCode 229
 * ——xterm 的 `_keyDown` 在 229 下什么都不发。换成按时间续命会更糟：按住自动
 * 重复时两次按键只隔十几毫秒，那样会把第二下吞掉，从多字变成丢字。
 */
export class OnceGuard {
  private seen: Array<{ ch: string; at: number }> = [];

  constructor(private readonly ttlMs = 200) {}

  /** 新的输入单元开始（每颗 keydown），上一颗键的账一笔勾销 */
  reset(): void {
    this.seen = [];
  }

  /** 这段字节该不该真的发出去。false = 同一颗键里已经发过一模一样的字符了 */
  allow(d: string, now: number): boolean {
    if (!isSingleVisible(d)) return true;
    this.seen = this.seen.filter((e) => now - e.at <= this.ttlMs);
    if (this.seen.some((e) => e.ch === d)) return false;
    this.seen.push({ ch: d, at: now });
    return true;
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
  /**
   * **事件自己的时间戳**（`ev.timeStamp`，与 `t` 同一时钟）。
   *
   * `t` 是处理时的时刻，`ts` 是事件产生的时刻，两者之差就是「事件在队列里躺了
   * 多久」——「打字发钝」这类纯延迟症状只有这个差值能证明，光看发出的字节
   * 一条不少、看不出慢。
   */
  ts?: number;
  t?: number;
};

const traceOn = (): boolean =>
  (globalThis as { __NX_TRACE_INPUT?: boolean }).__NX_TRACE_INPUT === true;

function trace(rec: TraceRec, ev?: Event): void {
  if (!traceOn()) return;
  if (ev !== undefined) rec.ts = ev.timeStamp;
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

/** [`installMacKeyboard`] 的返回值 */
export interface MacKeyboard {
  /** 卸下所有监听 */
  uninstall: () => void;
  /**
   * xterm 自己（`term.onData`）要发的这段字节，该不该放行。
   *
   * 调用方必须把 `term.onData` 接到这里 —— 否则 xterm 那一路绕过了 [`OnceGuard`]，
   * 重复字符的闸门就只剩「逐分支预测」那半套，也就回到了修不完的状态。
   */
  arbitrate: (data: string) => boolean;
}

/**
 * 挂上接管逻辑。
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
): MacKeyboard {
  if (!isMac) return { uninstall: () => {}, arbitrate: () => true };
  const internals = term as unknown as TerminalInternals;
  const ledger = new KeyLedger();
  /** 两个发送方共用的最后一道闸，见 `OnceGuard` */
  const guard = new OnceGuard();

  /** 发字节的唯一出口（本层内部）。顺手记一笔「谁发的、发了什么」。 */
  const out = (data: string, via: "mac" | "xterm" = "mac"): void => {
    if (!guard.allow(data, nowMs())) {
      trace({ ev: "send", act: "drop", sent: data, via });
      return;
    }
    traceSend(data, via);
    send(data);
  };

  /** xterm 那一路的入口，见 `MacKeyboard.arbitrate` */
  const arbitrate = (data: string): boolean => {
    if (guard.allow(data, nowMs())) return true;
    trace({ ev: "send", act: "drop", sent: data, via: "xterm" });
    return false;
  };

  // 组合状态由我们自己跟：`ev.isComposing` 在「输入法上下文残留」时是假的
  // （该 false 的时候 true、该 true 的时候 false），只有 compositionstart
  // 与 compositionend 这对事件说的是事实。
  let composing = false;

  /**
   * 第五类缺陷：**组合态永远不结束，整个终端失聪**。
   *
   * 真机路径（macOS + WKWebView + 第三方输入法，本机用豆包输入法复现）：
   * 拼音敲到一半（隐藏 textarea 里留着 marked text `ni`）→ 组合还没上屏就切
   * 走输入法 / 切走 app 把这次会话丢掉 → **WebKit 这一焦点的组合从没结束**，
   * 于是之后每颗键都是 `keyCode 229 + isComposing true`：
   *
   * - 我们这一层：看到 `composing` 为真就一律放行给 xterm；
   * - xterm 那一路：`CompositionHelper.keydown` 对 229 直接 return，`_keyDown`
   *   什么都不发，随后的 `input` 又被 `_inputEvent` 的 `!_keyDownSeen` 丢掉；
   *
   * 两路人都没有，**终端一个字节都不再出去**。用户侧的感觉就是「打英文一片字
   * 没了」；实测只有切到别的 app 再回来才恢复（WebKit 在可编辑元素失焦时才真正
   * 结束 IM 会话）。
   *
   * 判据刻意**不用时间**：真组合里用户停下来想三秒很正常，掐表必误杀中文。
   * 用「按键有没有回音」——真组合时每颗键都会带来 compositionupdate，或至少让
   * textarea 里的 marked text 变一变；连着好几颗键什么都没动，只可能是 IM 已经
   * 不接电话了，那时留着 `composing` 只会把整 tab 憋死。
   *
   * 证据见：`IM_GONE_KEYS` 定 3 颗 = 真机上一次也不会误伤（中文组合每颗键必有
   * update），同时用户最多丢 3 颗键就会被救回来。
   */
  const IM_GONE_KEYS = 3;
  /** 已判定 IM 失联：后续的 `composing` 一律不再当真 */
  let imGone = false;
  /** 组合态下连续几颗键没带来任何 IM 回应 */
  let quietKeys = 0;
  /** 上次看到的 marked text，用来发现「这次按键 IM 毫无反应」 */
  let composeTa = "";
  /**
   * 组合态里被我们按下不表的那几颗键。判定 IM 失联时把它们补发出去——用户真听到
   * 的「丢字母」往往就是自救前那两颗；补发的前提是从没人发出过（`markSent`
   * 之前一定顺手清掉这队），所以不可能是重复。
   */
  let swallowed: Array<{ ch: string; at: number }> = [];
  /** 补发的上限与有效期：只救「刚刚两颗」，不可能把十分钟前的键翻出来 */
  const SWALLOW_MAX = 3;
  const SWALLOW_TTL_MS = 400;

  /** 输入法确实收下了这颗键（它有回应了），压着的就作废 */
  const discardSwallowed = (): void => {
    swallowed = [];
  };

  /** 组合态里按下不表的键记一笔，等 ① 自救时补发 ② IM 有回应时作废 */
  const swallow = (ch: string): void => {
    const now = nowMs();
    swallowed = swallowed.filter((e) => now - e.at <= SWALLOW_TTL_MS);
    if (swallowed.length < SWALLOW_MAX) swallowed.push({ ch, at: now });
  };
  /** 本轮组合里被我们拦下的那颗 IME 键；只供 `onComposeEnd` 决定是否剥尾。 */
  let imeConsumedKey: string | null = null;
  /** 最近一次 `compositionend` 的时间戳，用于识别「同一次按键的两半」 */
  let composeEndedTs = -Infinity;
  /**
   * 刚刚这次上屏是不是「原样交出」：输入法把用户敲的 ASCII 原封不动交了出来
   *（豆包输入法的英文模式就是这样，一个词一次组合，`compend.data` 正是那几个字母）。
   *
   * 判它的原因：中文上屏（`nihao` → `你好`）时，那颗结束组合的**空格**属于输入法，
   * 一个字节都不该进 pty；但英文直通模式下同一个空格既是「结束这个词」，也是用户
   * 真正敲下的一个空格 —— 拦住它，"hello world" 就成了 "helloworld"（真机实测
   * 豆包英文模式打 `hello world angry birds` 只出来 `helloworldangry`）。
   */
  let lastCommitAscii = false;

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
   * 上一颗 keydown 被我们 `stopPropagation` 了、但**没有** `preventDefault`。
   *
   * 这种键 WebKit 会照常派发 `keypress`，而 xterm 的 `keypress` 监听在 textarea 上，
   * 捕获阶段的我们拦的是 keydown —— 两者各走各的，keypress 照样到得了 xterm：
   *
   * ```
   * _keyPress(e){ this._keyPressHandled=false; if(this._keyDownHandled) return false; … triggerDataEvent(key) }
   * ```
   *
   * `_keyDownHandled` 是在 `_keyDown` 里置的，而 `_keyDown` 正是被我们拦掉的那个，
   * 于是它停在上一次的 `false`，xterm 原样发一份；我们在 `onInput` 里又发一份
   * —— 每个标点两个字符。mac 中文输入法下有 composition 事件把这条路岔开，
   * **英文输入法下没有**，所以是「英文输入法敲标点必重复」。
   *
   * 修法是把 `keypress` 也在捕获阶段掐掉：xterm 的 `_keyPress` 除了发字节只剩
   * `onKey` 事件和重置光标闪烁，前者没人订阅，后者无关紧要。
   */
  let keydownStoppedWithoutDefault = false;
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
    trace({ ev: "compstart", act: "state", data: ev?.data ?? undefined, comp: false, ta: taOf(internals.textarea) }, ev);
    // **不清账本**：WebKit 会给空格这类普通按键补一对 composition 事件，清掉
    // 就会让已发出的那一颗在随后的 input 里被当成「没发过」，变成两个空格。
    // 组合期间 `composing` 会让 input 当场返回，账本也不会被误用。
    composing = true;
    // 新一轮组合开始 = 输入法又能接电话了，看门狗归零
    imAlive();
    composeTa = internals.textarea?.value ?? "";
    // 上一轮如果因为异常路径没消费掉，别让它带到这一轮
    imeConsumedKey = null;
    clearPending();
  };

  const onComposeEnd = (ev: CompositionEvent) => {
    trace({ ev: "compend", act: "state", data: ev.data ?? undefined, comp: true, ta: taOf(internals.textarea) }, ev);
    composing = false;
    // 自救（`revive`）是我们自己 `blur()` 掉 textarea，WebKit 会当场回一个
    // **data 为空**的 compositionend。它不是 IM 的回应，而是我们掐断会话的产物：
    // 认了它就会把 `imGone` 复位、还把 `composeEndedTs` 写成「刚刚」，于是紧跟着
    // 那颗真正属于用户的键被当成「上屏键的另一半」吞掉 —— 真机上正好丢一颗字母
    //（实测 `hello` 只剩 `helo`，丢的就是触发自救的那颗 `l`）。
    const selfTeardown = imGone && !(ev.data ?? "");
    if (!selfTeardown) {
      composeEndedTs = ev.timeStamp;
      // 无论这一轮是怎么结束的，存在的那一刻 IM 一定还活着
      imAlive();
    }
    composeTa = "";
    // 这一轮上屏是不是「原样交出」：交给 pty 的文本就是刚才敲的那几个 ASCII 字母。
    // 英文直通模式（豆包）走的就是这条，中文上屏（`nihao` → `你好`）不是。
    const committedText = ev.data ?? "";
    lastCommitAscii =
      isAsciiRun(committedText) && taOf(internals.textarea).endsWith(committedText);
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

  /**
   * 组合态是否还值得相信。IM 已判定失联时，残留的 `composing` 只剩副作用
   * （把所有键都放给一个不会发字的 xterm），所以一律按「没在组合」处理。
   */
  const activeCompose = (): boolean => composing && !imGone;

  /** 组合真的重新开始 ↔ IM 回来了 */
  const imAlive = (): void => {
    imGone = false;
    quietKeys = 0;
    discardSwallowed();
  };

  /**
   * 自救：把遗留的 IM 会话掐断，让键回到正常路径。
   *
   * 唯一有效的动作是**让隐藏 textarea 失焦再聚焦** —— 和「切走别的 app 再回来」
   * 是同一件事：WebKit 在可编辑元素失焦时会结束 IM 会话、丢掉 marked text，
   * 之后 WebKit 给回来的 keyCode 就从 229 变回真实值。
   *
   * 顺手清掉 textarea 里那截残留：它已经不可能上屏了，留着只会在某次
   * `_handleAnyTextareaChanges` 里按长度差算进 pty，变成一串幽灵字符。
   */
  const revive = (): void => {
    const ta = internals.textarea;
    trace({ ev: "revive", act: "state", comp: composing, ta: taOf(ta) });
    quietKeys = 0;
    imGone = true;
    // 判定不失联的那一刻之前被按下不表的那几颗键，趁现在补发出去 —— 用户嘴里
    // 的「丢字母」多半就是这几颗（判定要攒够几颗键，第一颗没法当场知道）。
    const replay = swallowed.splice(0, swallowed.length);
    try {
      ta?.blur();
      if (ta) ta.value = "";
      // 同一个「帧」里聚焦回去：拖到定时任务里可能被别的元素抢走焦点，
      // 那就只是丢了一次组合，反而更糟。
      if (typeof requestAnimationFrame === "function") {
        requestAnimationFrame(() => ta?.focus?.());
      }
    } catch {
      // 焦点操作失败也不能让终端继续失聪：`imGone` 已经置位，按键照样走我们的出口
    }
    for (const it of replay) {
      clearTextarea();
      out(it.ch);
      trace({ ev: "send", act: "replay", sent: it.ch, via: "mac" });
    }
  };

  /** 我们替 xterm 把这颗键标成「已发过」，防止它再从 input 补一遍 */
  const markSent = () => {
    internals._keyDownSeen = true;
    internals._keyPressHandled = true;
  };

  const onKeyDown = (ev: KeyboardEvent) => {
    // 一颗物理按键 = 一颗 keydown = 一个新的输入单元，上一颗的账到此为止。
    // 连打 `..` 和按住自动重复都各有各的 keydown，所以不会被 guard 误吞。
    guard.reset();
    // 每颗键重置：keypress 紧跟着它自己的 keydown 来，不会隔着下一颗键
    keydownStoppedWithoutDefault = false;
    // —— 看门狗：组合态下 IM 还在不在接电话（详见 `IM_GONE_KEYS` 的注释）——
    if (composing) {
      const taNow = internals.textarea?.value ?? "";
      // 只有「又有新的预编辑串被写进来」才算 IM 活着。`revive` 与 ASCII 分支都会
      // 自己清空 textarea，若把「变空」也当成 IM 的动静，自救会在下一颗键上被
      // 自己推翻 —— 实测那正好会让终端只在第 3 颗键出字、之后又哑掉。
      if (taNow.length > 0 && taNow !== composeTa) {
        composeTa = taNow;
        imAlive();
      } else if (!imGone && isPlainChar(ev.key) && ++quietKeys >= IM_GONE_KEYS) {
        revive();
      }
    } else {
      composeTa = internals.textarea?.value ?? "";
      quietKeys = 0;
    }
    // 中文输入法「智能全角标点」：必须排在最前面、且**不看 `ev.isComposing`**——
    // 它在输入法上下文残留时会谎报 true，跟着走下面 `isImeKey` / `ev.isComposing`
    // 那几条分支的话，这颗键要么被当成「组合态里的确认键」吞掉、要么提前 return，
    // 根本轮不到标点处理。真实组合状态只认 `composing`。
    if (
      !activeCompose() &&
      !ev.ctrlKey && !ev.altKey && !ev.metaKey &&
      PUNCT_RISK.has(ev.key)
    ) {
      // 不能 preventDefault：要让 WebKit 按它自己的判断正常插入（原样或替换成
      // 全角），我们只负责别让 xterm 也掺和进来抢着发一遍。
      ev.stopPropagation();
      // 没 preventDefault → 随后的 keypress 还会去找 xterm，必须一并掐掉
      keydownStoppedWithoutDefault = true;
      // 刚刚才有 insertText 交出过字符 → 这颗键的结果已经发出去了，绝不再补。
      // 少了这一句，「input 先到」的机型上每敲一个标点都会多出一个字符。
      if (nowMs() - lastTextInputAt < 200) {
        trace({ ev: "keydown", act: "drop", key: ev.key, kc: ev.keyCode, code: ev.code, mods: modsOf(ev), ic: ev.isComposing, comp: composing, ta: taOf(internals.textarea) }, ev);
        return;
      }
      clearPending();
      pendingPunct = { key: ev.key, at: nowMs() };
      pendingTimer = setTimeout(() => {
        if (pendingPunct?.key === ev.key) {
          clearTextarea();
          // 80ms 都没等到 input：说明 WebKit 根本没替换（preventDefault 生效了
          // 或这颗键根本没进 textarea），那就按原始半角发，不能把它吞了。
          // 必须记进账本：这条 input 可能只是**迟到**（机器卡一下就会），
          // 到了之后会走下面的通用分支，账本里没有就又发一遍。
          ledger.keyDown(ev.key, nowMs());
          out(ev.key);
          pendingPunct = null;
        }
        pendingTimer = undefined;
      }, 80);
      trace({ ev: "keydown", act: "defer", key: ev.key, kc: ev.keyCode, code: ev.code, mods: modsOf(ev), ic: ev.isComposing, comp: composing, ta: taOf(internals.textarea) }, ev);
      return;
    }
    // 组合态里被 IME 吃掉的键：拦在这里，别让它走到 xterm 的 `_keyDown`。
    if (isImeKey(ev)) {
      if (activeCompose() || (ev.isComposing && !imGone)) {
        // 交给 IM 之前先留底：万一它压根没收下（输入法已经走了），自救时把这颗
        // 补发回来。IM 有反应的话 `imAlive` 会立刻作废这份留底。
        if (activeCompose() && isPlainChar(ev.key)) swallow(ev.key);
        // 组合还没结束：**不能 preventDefault**，上屏是 IME 的默认行为，拦掉会
        // 把候选词卡住。只 stopPropagation 掐断 xterm 那一路（否则空格多一个
        // 空格、选候选的数字变成「大2」）。代价是那颗键照样可能写进隐藏
        // textarea，所以记一笔留给 `onComposeEnd` 判定要不要剥掉。
        if (xtermWouldEmit(ev)) {
          ev.stopPropagation();
          keydownStoppedWithoutDefault = true; // 同上：keypress 也要掐
          markSent(); // 让 xterm 的 `_inputEvent` 也别去发上屏那一条（否则上屏两遍）
          imeConsumedKey = ev.key;
          trace({ ev: "keydown", act: "stop", key: ev.key, kc: ev.keyCode, code: ev.code, mods: modsOf(ev), ic: ev.isComposing, comp: composing, ta: taOf(internals.textarea) }, ev);
        } else {
          trace({ ev: "keydown", act: "pass", key: ev.key, kc: ev.keyCode, code: ev.code, mods: modsOf(ev), ic: ev.isComposing, comp: composing, ta: taOf(internals.textarea) }, ev);
        }
        return;
      }
      if (sameAsComposeEnd(ev)) {
        // 上屏已完成，这颗只是同一次物理按键的另一半。
        //
        // 但「另一半」有两种：中文上屏时空格是**输入法的确认键**，它的字符绝不能进
        // pty（否则每句中文后面多一个空格）；英文直通模式（豆包）时空格既是「结束
        // 这个词」也是用户实打实敲的一个空格 —— 真机上把它一起拦掉，"hello world"
        // 就粘连成 "helloworld"。判据是 `lastCommitAscii`：输入法原样交出了 ASCII，
        // 说明它只是在替用户转手，这颗键的字符还得照常发出去。
        //
        // 「照常发」= 放行到下面那条通用路径由我们自己发。**不能指望 WebKit 的默认
        // 行为**：这类键带 `keyCode 229` 且 `isComposing=false`，真机上 WebKit 既不
        // 插进 textarea 也不派发 input（实测空格的 keydown 放行了，input 一条没有），
        // 光靠默认行为就是继续丢空格。
        if (!(lastCommitAscii && isPlainChar(ev.key))) {
          // 中文上屏：此时 preventDefault 只会拦住「插进 textarea」，动不了已上屏
          // 的字，可以放心拦死。
          // 不要再记 imeConsumedKey：那次 compositionend 已过去，记了只会留到
          // **下一次**上屏时才消费，把下一句结尾剥掉。
          ev.preventDefault();
          ev.stopPropagation();
          markSent();
          // 回车同理：英文直通模式下它既结束这个词、又是用户敲的那一下 —— 吞掉就是
          // **命令不执行**（真机实测 `pwd`+回车只上屏 `pwd`，要再按一次才跑）。
          if (lastCommitAscii && ev.key === "Enter") {
            out("\r");
            trace({ ev: "keydown", act: "send", key: ev.key, kc: ev.keyCode, code: ev.code, mods: modsOf(ev), ic: ev.isComposing, comp: composing, ta: taOf(internals.textarea), sent: "\\r", via: "mac" }, ev);
            return;
          }
          trace({ ev: "keydown", act: "stop", key: ev.key, kc: ev.keyCode, code: ev.code, mods: modsOf(ev), ic: ev.isComposing, comp: composing, ta: taOf(internals.textarea) }, ev);
          return;
        }
      }
    }
    // 真·组合态：这颗键属于拼音，一个字节都不该由我们发，textarea 也不能动。
    // 同样留底：判据是「IM 有没有回应」，此刻还没回应不代表它没有在听。
    if (activeCompose()) {
      if (isPlainChar(ev.key)) swallow(ev.key);
      trace({ ev: "keydown", act: "pass", key: ev.key, kc: ev.keyCode, code: ev.code, mods: modsOf(ev), ic: ev.isComposing, comp: composing, ta: taOf(internals.textarea) }, ev);
      return;
    }
    // 浏览器自称在组合、我们却没收到 compositionstart（真组合的 compositionstart
    // 一定先过 host 的捕获阶段）：输入法上下文残留。这里的字母交给 xterm 会因
    // 229 丢掉，但 `onInput` 那条兜底刻意不看 isComposing，会补发出去。
    if (ev.isComposing && !imGone) {
      trace({ ev: "keydown", act: "pass", key: ev.key, kc: ev.keyCode, code: ev.code, mods: modsOf(ev), ic: ev.isComposing, comp: composing, ta: taOf(internals.textarea) }, ev);
      return;
    }
    if (!isPlainKeystroke(ev)) {
      trace({ ev: "keydown", act: "pass", key: ev.key, kc: ev.keyCode, code: ev.code, mods: modsOf(ev), ic: ev.isComposing, comp: composing, ta: taOf(internals.textarea) }, ev);
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
    trace({ ev: "keydown", act: fresh ? "send" : "drop", key: ev.key, kc: ev.keyCode, code: ev.code, mods: modsOf(ev), ic: ev.isComposing, comp: composing, ta: taBefore, sent: fresh ? ev.key : undefined, via: fresh ? "mac" : undefined }, ev);
  };

  /**
   * 拦住「上一颗 keydown 我们只 stopPropagation、没 preventDefault」时漏给 xterm
   * 的那条 keypress。详见 `keydownStoppedWithoutDefault`。
   */
  const onKeyPress = (ev: KeyboardEvent) => {
    if (!keydownStoppedWithoutDefault) {
      trace({ ev: "keypress", act: "pass", key: ev.key, kc: ev.keyCode, code: ev.code, mods: modsOf(ev), ic: ev.isComposing, comp: composing, ta: taOf(internals.textarea) }, ev);
      return;
    }
    keydownStoppedWithoutDefault = false;
    // 只拦 xterm 这一路。**不 preventDefault**：全角替换和候选上屏都靠默认行为
    ev.stopPropagation();
    trace({ ev: "keypress", act: "stop", key: ev.key, kc: ev.keyCode, code: ev.code, mods: modsOf(ev), ic: ev.isComposing, comp: composing, ta: taOf(internals.textarea) }, ev);
  };

  const onInput = (ev: InputEvent) => {
    // 有半角标点键在等：不管这条 data 是原样还是全角，都是它唯一、最终的结果，
    // 发这一条就够，xterm 不该再碰（否则非 ASCII 会被当成组合上屏再发一遍）。
    if (pendingPunct && !activeCompose() && ev.data) {
      clearPending();
      ev.stopPropagation();
      clearTextarea();
      lastTextInputAt = nowMs();
      out(ev.data);
      trace({ ev: "input", act: "send", it: ev.inputType, data: ev.data, ic: ev.isComposing, comp: composing, ta: taOf(internals.textarea), sent: ev.data, via: "mac" }, ev);
      return;
    }
    if (ev.inputType === "insertText" && ev.data) lastTextInputAt = nowMs();
    // 刻意不看 `ev.isComposing`：输入法上下文残留时它会谎报 true，而那正是丢字
    // 发生的那一刻。是否在组合只以我们自己的 compositionstart/end 为准。
    const seen: TraceRec = { ev: "input", act: "pass", it: ev.inputType, data: ev.data ?? undefined, ic: ev.isComposing, comp: composing, ta: taOf(internals.textarea) };
    if (activeCompose()) {
      trace(seen, ev);
      return;
    }
    // 上屏刚过去就到的 insertText，是同一次 IME 操作的尾巴（选候选那颗键被
    // WebKit 顺手当成普通输入派发出来的），不是新的输入 —— 发出去就成了「大2」。
    // 例外：英文直通模式里「结束这个词」的那颗键（空格）的字符正是从这里到的，
    // 它本身就是用户敲的字，必须放行（判据见 `lastCommitAscii`）。
    if (sameAsComposeEnd(ev)) {
      const single = !!ev.data && Array.from(ev.data).length === 1;
      if (!(lastCommitAscii && single)) {
        seen.act = "drop";
        trace(seen);
        return;
      }
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
    // **单个**字符一律由我们发，全角标点也不例外（`？` `、` `。` `，` `《》`）。
    //
    // 以前这里见到非 ASCII 就整条交回 xterm，结果全角标点被吞：它不经过组合，
    // WebKit 给的 keydown 却带着 keyCode 229，xterm 的 `_keyDown` 置完
    // `_keyDownSeen=true` 就被 `CompositionHelper.keydown` 提前返回（什么都没发），
    // 随后这条 input 又被 `_inputEvent` 的 `!this._keyDownSeen` 门整条丢掉
    // —— 就是模块开头第 1 类缺陷，只不过换了个字符集。
    //
    // 真正要留给 xterm 组合路径的是**多字符**上屏（听写整句、一次上屏一个词）：
    // 那种连 textarea 都不能清，见 `clearTextarea`。
    const single = Array.from(data).length === 1;
    if (!single) {
      for (const ch of data) {
        if (!isPlainChar(ch)) {
          trace(seen);
          return;
        }
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
    const rest = single ? ledger.input(data, nowMs()) : "";
    const ok = rest !== "";
    if (ok) out(rest);
    trace({ ev: "input", act: ok ? "send" : "drop", it: ev.inputType, data, ic: ev.isComposing, comp: composing, ta: taBefore, sent: ok ? rest : undefined, via: ok ? "mac" : undefined }, ev);
  };

  /** 只记不看：把「我们没接的那些事件」也留下来，才知道漏了什么 */
  const onPassive = (ev: Event) => {
    // 刻意只用**结构**认事件：不能在 listener 里依赖 `ev.type`（自测的假事件对象
    // 连 type 都没有），更不能用 `ev instanceof KeyboardEvent` —— node 里这个全局
    // 根本不存在，`instanceof` 会直接抛 ReferenceError，记日志不该把输入炸掉。
    const keyboard = "keyCode" in ev;
    const input = "inputType" in ev;
    // compositionupdate = 「IM 还活着」的证据，与日志开关无关，必须先看
    if (!keyboard && !input) {
      composeTa = internals.textarea?.value ?? "";
      imAlive();
      if (!traceOn()) return;
      trace({ ev: "compupdate", act: "pass", data: (ev as CompositionEvent).data ?? undefined, comp: composing, ta: taOf(internals.textarea) }, ev);
      return;
    }
    if (!traceOn()) return;
    if (input) {
      const ie = ev as InputEvent;
      trace({ ev: "beforeinput", act: "pass", it: ie.inputType, data: ie.data ?? undefined, ic: ie.isComposing, comp: composing, ta: taOf(internals.textarea) }, ev);
      return;
    }
    const k = ev as KeyboardEvent;
    trace({ ev: k.type, act: "pass", key: k.key, kc: k.keyCode, code: k.code, mods: modsOf(k), ic: k.isComposing, comp: composing, ta: taOf(internals.textarea) }, ev);
  };

  host.addEventListener("keydown", onKeyDown, true);
  host.addEventListener("keypress", onKeyPress, true);
  // lib.dom 把 "input" 的事件类型标成 Event（没有 data/isComposing），显式转一次
  host.addEventListener("input", onInput as unknown as EventListener, true);
  host.addEventListener("compositionstart", onComposeStart, true);
  host.addEventListener("compositionend", onComposeEnd, true);
  // 下面这几个纯观测：不拦、不 preventDefault，只是把现场补完整
  host.addEventListener("keyup", onPassive, true);
  host.addEventListener("beforeinput", onPassive, true);
  host.addEventListener("compositionupdate", onPassive, true);

  const uninstall = () => {
    host.removeEventListener("keydown", onKeyDown, true);
    host.removeEventListener("keypress", onKeyPress, true);
    host.removeEventListener("input", onInput as unknown as EventListener, true);
    host.removeEventListener("compositionstart", onComposeStart, true);
    host.removeEventListener("compositionend", onComposeEnd, true);
    host.removeEventListener("keyup", onPassive, true);
    host.removeEventListener("beforeinput", onPassive, true);
    host.removeEventListener("compositionupdate", onPassive, true);
    ledger.clear();
    guard.reset();
    clearPending();
  };

  return { uninstall, arbitrate };
}
