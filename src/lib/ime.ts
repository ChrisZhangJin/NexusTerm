/**
 * 判断一次「回车」到底是**发送**，还是**确认输入法候选**。
 *
 * 现象：AI 面板里打拼音，按回车本来是想把候选字上屏，结果消息被直接发出去了。
 * 输入框里已经写了 `!e.nativeEvent.isComposing`，但它在这台机器上不起作用。
 *
 * 根因不是我们的错，是 WebKit 的事件顺序（bug 165004）：
 *
 *   规范 / Chrome / Firefox：keydown(isComposing=true) → compositionend
 *   Safari / WKWebView：      compositionend            → keydown(isComposing=false)
 *
 * 也就是说 Safari 先把「组合结束」告诉我们，再派发那个回车。等我们的
 * keydown 处理函数跑起来时，`isComposing` 已经被清成 false 了 —— 这一判断
 * 永远为假。Tauri 在 macOS 上用的就是 WKWebView，所以只有 mac 版有这个毛病。
 *
 * 可靠的信号是 `keyCode === 229`（VK_PROCESS）：被输入法接管的按键都会带上
 * 这个键码，而任何物理按键都不会产生它。再叠两道保险：
 * - `compositionstart/end` 期间（keydown 先到时能兜住）；
 * - 组合刚结束的极短窗口内（个别第三方输入法不给 229 时的兜底）。
 *
 * 这三条只用来**判定**，不碰输入框的值 —— 中文上屏本身走浏览器自己的路径，
 * 我们一旦插手去改 value，就会出现重复字、光标乱跳。
 */

/** 参与判定的键盘事件字段（只取用得到的，便于在 node 里构造用例） */
export interface KeyLike {
  key: string;
  shiftKey: boolean;
  isComposing: boolean;
  /** 已废弃，但这里它是 WebKit 下唯一可靠的输入法信号，见文件头 */
  keyCode: number;
  /** 必须取 `nativeEvent.timeStamp`：它与 compositionend 的时基一致 */
  timeStamp: number;
}

/**
 * `send`            真回车 → 发送
 * `ime`             输入法正在处理这一个键（229 / isComposing / 组合中）
 * `ime-after-commit` 组合刚刚结束，这个回车大概率是那次确认（兜底窗口）
 * `pass`            不是回车，或带了 Shift → 交给浏览器（换行）
 */
export type EnterVerdict = "send" | "ime" | "ime-after-commit" | "pass";

/** Safari 的 compositionend 与确认回车相隔常在 5ms 内；50ms 是上游建议的余量 */
export const COMMIT_WINDOW_MS = 50;

const VK_PROCESS = 229;

/**
 * 跟踪输入法状态。刻意做成普通对象而不是读 DOM：
 * 这样它能在 node 里被单测，也不会把组件状态搅进来。
 */
export class ImeGuard {
  private composing = false;
  private endedAt: number | null = null;

  /** compositionstart */
  begin(): void {
    this.composing = true;
    this.endedAt = null;
  }

  /**
   * compositionend。
   *
   * @param timeStamp 取 `nativeEvent.timeStamp`。Safari 可能把确认回车的
   *                  keydown 标得比这个还早，所以判窗口时要用绝对值。
   */
  finish(timeStamp: number): void {
    this.composing = false;
    this.endedAt = timeStamp;
  }

  /** 失焦等场景下清干净，避免 compose 中被切走后再也解不开 */
  reset(): void {
    this.composing = false;
    this.endedAt = null;
  }

  get isComposing(): boolean {
    return this.composing;
  }

  /**
   * 这个键是不是被输入法接管了。
   *
   * 注意它对**任何**键都成立，不只是回车：拼音组合中按上下键是在选候选词，
   * 界面上的联想列表绝不能被同时挪动，否则一次选词会顺带改掉要发给谁。
   */
  captures(ev: KeyLike): boolean {
    if (ev.isComposing) return true;
    if (this.composing) return true;
    // 被输入法接管：物理键盘永远产生不了 229
    return ev.keyCode === VK_PROCESS;
  }

  /** 判定这次按键 */
  verdict(ev: KeyLike): EnterVerdict {
    if (ev.key !== "Enter" || ev.shiftKey) return "pass";
    // 输入法自己正在处理这个键
    if (this.captures(ev)) return "ime";
    // 兜底：组合刚结束。用绝对值是因为 Safari 下 keydown 的时间戳可能更早
    if (this.endedAt !== null && Math.abs(ev.timeStamp - this.endedAt) < COMMIT_WINDOW_MS) {
      return "ime-after-commit";
    }
    return "send";
  }
}

/** 组件里直接用的便捷函数 */
export function enterVerdict(ev: KeyLike, guard: ImeGuard): EnterVerdict {
  return guard.verdict(ev);
}

/** 这个按键是否正被输入法占用（任意键） */
export function isImeKey(ev: KeyLike, guard: ImeGuard): boolean {
  return guard.captures(ev);
}

/**
 * 把 React 的合成键盘事件压成判定用的形状。
 *
 * 两个坑：
 * - React 会把 `keyCode` 归一化，某些版本里它恒为 0，所以要和 `nativeEvent`
 *   取或；真值在 nativeEvent 上。
 * - `timeStamp` 必须取 `nativeEvent` 的：合成事件的 timeStamp 与
 *   compositionend 不在同一时基上，算窗口会算错。
 */
export function keyInfoFrom(e: {
  key: string;
  shiftKey: boolean;
  keyCode?: number;
  nativeEvent: { isComposing: boolean; keyCode?: number; timeStamp: number };
}): KeyLike {
  return {
    key: e.key,
    shiftKey: e.shiftKey,
    isComposing: e.nativeEvent.isComposing,
    keyCode: e.keyCode || e.nativeEvent.keyCode || 0,
    timeStamp: e.nativeEvent.timeStamp,
  };
}
