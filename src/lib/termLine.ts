/**
 * 录制用的「终端输入补全」纯逻辑（Phase 3.2）。
 *
 * 行状态机（`lib/complete.feedKeys`）只认「自己一个字一个字敲出来」的行：用了 ↑ 历史回溯、Tab 补全、
 * 光标移动之后行就是「未知」的，回车时交不出命令，时间线上就缺了这条（录制出来的草稿少一步）。
 * 这里补两件事：
 *
 * 1. `readLogicalLine`：从 xterm 屏幕缓冲里把「提示符之后、光标之前」的那段读回来
 *    （跨自动折行的多行也行）。**只在光标位于行尾时采信**，读不准宁可返回 null、由后端告警，
 *    也不记一条可能不是用户真正执行的命令。
 * 2. `recorderTapAction`：回车 / Ctrl-C 等时刻，决定要向录制器上报什么。
 *
 * 全是不碰 xterm / React / Tauri 的纯函数（缓冲用结构类型接口，`term.buffer.active` 直接满足）。
 */

import { specialKeyFor } from "./recorder";

/** xterm `IBufferLine` 里用到的部分 */
export interface LineLike {
  readonly isWrapped: boolean;
  translateToString(trimRight?: boolean, startColumn?: number, endColumn?: number): string;
}

/** xterm `IBuffer` 里用到的部分 */
export interface BufLike {
  readonly cursorX: number;
  readonly cursorY: number;
  readonly baseY: number;
  getLine(y: number): LineLike | undefined;
}

/** 一行输入的起点：绝对行号（含 scrollback）与列 */
export interface LineStart {
  x: number;
  y: number;
}

/** 此刻光标位置，作为「新一行输入的起点」（提示符刚画完、用户还没敲时调用） */
export function captureLineStart(buf: BufLike): LineStart {
  return { x: buf.cursorX, y: buf.baseY + buf.cursorY };
}

/** 读回的行最长字符数（与上报上限同量级，防止异常屏幕塞爆 IPC） */
const MAX_READ_CHARS = 4096;

/**
 * 读 `start` 到光标之间的文字。读不准返回 null：
 * - 起点在光标之后 / 起点所在行已不在缓冲里；
 * - 后续行不是自动折行（`isWrapped`）接上来的——中间换过行 / 被清屏，不是同一条输入；
 * - 光标后面还有内容（光标没在行尾，说明用户在行中编辑，或有右侧提示 RPROMPT）。
 */
export function readLogicalLine(buf: BufLike, start: LineStart | null): string | null {
  if (!start) return null;
  const cy = buf.baseY + buf.cursorY;
  const cx = buf.cursorX;
  if (start.y > cy || (start.y === cy && start.x > cx)) return null;

  // 光标之后必须是空白
  const cur = buf.getLine(cy);
  if (!cur) return null;
  if (cur.translateToString(false, cx).trim() !== "") return null;

  let out = "";
  for (let y = start.y; y <= cy; y++) {
    const line = buf.getLine(y);
    if (!line) return null;
    if (y > start.y && !line.isWrapped) return null;
    const from = y === start.y ? start.x : 0;
    const to = y === cy ? cx : undefined;
    out += line.translateToString(false, from, to);
    if (out.length > MAX_READ_CHARS) return null;
  }
  return out.trimEnd();
}

/** 要上报给录制器的动作 */
export type TapAction =
  | { kind: "keys"; keys: string }
  | { kind: "command"; line: string }
  | null;

/**
 * 一次输入之后，要不要向录制器补报点什么。**只在录制中调用**（调用方先问 `isRecording`）。
 *
 * - 整段恰为 Ctrl-C / Ctrl-D / Ctrl-Z：上报按键（它们不产生「命令」，但对回放是有意义的操作）；
 * - 单独一下回车且行状态机已经交出了命令（`committed`）：已由原路径上报，这里什么都不补；
 * - 单独一下回车、没交出命令：
 *   - 提交前行是已知的（空行 / 纯空白）→ 上报一次空回车（分页器、确认提示上的回车）；
 *   - 提交前行是未知的（↑ 历史 / Tab / 光标编辑）→ 尝试从屏幕读回，读到了当命令上报；
 *   - 全屏程序（备用屏）里一律不补。
 */
export function recorderTapAction(o: {
  data: string;
  committed: string | null;
  /** 提交前行状态机是否「已知」 */
  wasKnown: boolean;
  altScreen: boolean;
  /** 读屏回溯，仅在需要时才调用 */
  recover: () => string | null;
}): TapAction {
  if (o.altScreen) return null;
  const key = specialKeyFor(o.data);
  if (key) return { kind: "keys", keys: key };
  // 只认「单独一下回车」：粘贴 / 带转义序列的输入里的换行不算（shell 不会因为它们执行）
  if (o.data !== "\r" && o.data !== "\n") return null;
  if (o.committed !== null) return null;
  if (o.wasKnown) return { kind: "keys", keys: "<Enter>" };
  const line = o.recover();
  if (line && line.trim()) return { kind: "command", line: line.trim() };
  return null;
}
