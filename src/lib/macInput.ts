import type { Terminal } from "@xterm/xterm";

/**
 * 修 macOS(WKWebView) 上「快速连打偶发丢一个字」。
 *
 * 现象：输入 `abc` 得到 `ac`，中间那个字消失；只在 mac 版、只在打得快时出现，
 * 慢速逐字输入不丢，Windows 版没有。
 *
 * 根因（xterm issue #5374 已确认）：Safari/WKWebView 会在字符**自己的 keydown
 * 之前**先派发 `beforeinput` / `input`。xterm 用一个 `_keyDownSeen` 标志做去重，
 * 它以为「这个字符已经由 keydown 发过了」，于是把真正的 input 当成重复事件丢掉。
 * 上一个键还没抬起时（也就是快速连打），`_keyDownSeen` 一直是 true，这一次
 * input 必被丢 —— 正是丢字的那一下。
 *
 * 做法：在 `beforeinput` 阶段把 `_keyDownSeen` 复位，让紧随其后的 `input`
 * 能正常走 xterm 自己的 `triggerDataEvent`。**这里不自己发任何字节**，
 * 只把状态修正回来，所以不可能产生重复输入。
 */

/** xterm 5.x 直接在 Terminal 实例上挂的私有去重标志（已核对 5.5.0 源码与打包产物） */
interface TerminalInternals {
  _keyDownSeen?: boolean;
}

/**
 * 这个 `beforeinput` 是否属于「WKWebView 提前派发、会被 xterm 误判为重复」的那一类。
 *
 * 条件刻意收得很紧：只认 `composed` 的单次文本插入，且必须**不在输入法组合中**。
 * 中文/日文输入法的 composition 走 keyCode 229 那条完全不同的路径，绝不能碰，
 * 否则会把正常的中文上屏搅乱。纯函数，便于单测。
 */
export function shouldResyncKeyDownSeen(ev: {
  composed: boolean;
  data: string | null;
  inputType: string;
  isComposing: boolean;
}): boolean {
  return ev.composed && !!ev.data && ev.inputType === "insertText" && !ev.isComposing;
}

/**
 * 挂上修正逻辑，返回卸载函数。
 *
 * @param term   已 `open()` 的终端（`textarea` 此时才存在）
 * @param isMac  是否 macOS。由调用方传入，模块本身不依赖平台探测，
 *               这样它能在 node 里被单测，也不用把 Tauri 运行时拖进来。
 *
 * 非 macOS、或 `textarea` / 内部字段缺失（xterm 升级后改名）时返回空转函数 ——
 * 宁可不修，也绝不能在结构变化时误伤正常输入。
 */
export function installMacInputGuard(term: Terminal, isMac: boolean): () => void {
  if (!isMac) return () => {};
  const textarea = term.textarea;
  const internals = term as unknown as TerminalInternals;
  if (!textarea || typeof internals._keyDownSeen !== "boolean") return () => {};

  const onBeforeInput = (ev: InputEvent) => {
    if (!shouldResyncKeyDownSeen(ev)) return;
    internals._keyDownSeen = false;
  };

  // 捕获阶段：`beforeinput` 按规范先于 `input`，这里只是确保不被更早的
  // 冒泡处理器拦走；复位必须发生在 xterm 的 input 监听读到它之前。
  textarea.addEventListener("beforeinput", onBeforeInput, true);
  return () => textarea.removeEventListener("beforeinput", onBeforeInput, true);
}
