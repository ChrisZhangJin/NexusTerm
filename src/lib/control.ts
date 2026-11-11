/**
 * 终端控制权（人 / Agent / 暂停）的前端纯逻辑。后端状态机见 `src-tauri/src/agent/control.rs`。
 *
 * 前端只做两件事：
 * 1. 记住后端推来的状态，开关一个**布尔闸**；
 * 2. 闸开着（Agent 驾驶中）时，用户一敲键就通知后端一次，随即把闸关上 ——
 *    之后每个键都只是一次布尔判断，不再有任何 IPC。没有 Agent 在跑时同样只是一次布尔判断。
 *
 * 键盘字节本身永远不经过这里：调用方先把字节发给 pty，再喂闸，闸只「看」不「拦」。
 */

import { isTerminalReply } from "./macro";
import { isXtermGenerated } from "./termRegistry";

export type ControlState = "userDriving" | "agentDriving" | "paused";

/** `nx://agent/control/{session}` 的载荷 */
export interface ControlPayload {
  session: string;
  state: ControlState;
}

/**
 * `onData` 给出的这串字节是不是「用户在打字」。
 *
 * xterm 的 `onData` 不只有键盘：终端程序发来查询（光标位置 `ESC[6n`、设备属性 `ESC[c`、
 * 颜色查询 OSC 等）时，xterm 会**自己**回一段应答也走 `onData`；开了焦点上报的程序
 * 还会收到 `ESC[I` / `ESC[O`，开了鼠标追踪的 TUI 还有鼠标上报。这些不是用户动手 ——
 * 把它们当键盘会让 Agent 无缘无故被暂停。
 *
 * 判定刻意保守地只排除已知的自动应答形状，其余（含方向键 `ESC[A`、`ESC O P` 等）都算用户。
 */
export function isUserTyping(d: string): boolean {
  if (d.length === 0) return false;
  if (isTerminalReply(d)) return false; // CPR / DA / 窗口尺寸 / OSC / DCS（与宏录制共用同一份判定）
  // 补充 macro 那份没覆盖的：焦点进出、鼠标上报、带 ? 的 CPR、DSR、DECRPM，以及几段连发
  // （与托管夺权共用 termRegistry 的同一份判定）
  if (isXtermGenerated(d)) return false;
  return true;
}

/**
 * 键盘闸。一个终端一个。
 *
 * - `set(state)`：后端推来的（或挂载时查询到的）状态；只有 `agentDriving` 才开闸；
 * - `onInput(d)`：每个用户字节调一次。闸关着（绝大多数时候）只是读一个布尔；
 *   闸开着且这确实是用户在打字，则关闸并调一次 `notify`（通知后端 → Paused）。
 */
export function createControlGate(notify: () => void) {
  let armed = false;
  return {
    set(state: ControlState): void {
      armed = state === "agentDriving";
    },
    onInput(d: string): void {
      if (!armed) return;
      if (!isUserTyping(d)) return;
      // 先关闸再通知：notify 若抛错 / 很慢，也不会让后面的每个键都再报一次
      armed = false;
      notify();
    },
    get armed(): boolean {
      return armed;
    },
  };
}

/** 在一个会话清单里挑出「处于暂停」的会话（面板的「交还」按钮用）。 */
export function pausedSessions(states: Record<string, ControlState>): string[] {
  return Object.keys(states).filter((s) => states[s] === "paused");
}

/** 「正在被 Agent 驾驶」的会话（面板的「接管」按钮用）。 */
export function drivingSessions(states: Record<string, ControlState>): string[] {
  return Object.keys(states).filter((s) => states[s] === "agentDriving");
}

/**
 * 并入一条状态更新。`userDriving` 直接删掉（表里只留活跃的），
 * 没变化时返回原对象 —— React 据此跳过无谓的重渲染。
 */
export function applyControl(
  states: Record<string, ControlState>,
  p: ControlPayload
): Record<string, ControlState> {
  if (p.state === "userDriving") {
    if (!(p.session in states)) return states;
    const { [p.session]: _gone, ...rest } = states;
    return rest;
  }
  if (states[p.session] === p.state) return states;
  return { ...states, [p.session]: p.state };
}
