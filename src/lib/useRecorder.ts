/**
 * 录制器（Phase 3.2）的前端状态：每个终端会话一份录制状态 + 最近一份草稿。
 *
 * 模块级的小 store，而不是组件 state：
 * - 终端组件（`TerminalView`）要**同步**知道「这个会话正在录制吗」来决定要不要上报按键，
 *   它不渲染任何录制界面，不该为此挂一个 React hook；
 * - 面板切标签 / 切视图时组件会卸载重挂，草稿不能跟着丢（3.3 / 3.4 要接着用它）。
 *
 * 后端是状态的唯一真相：`watchRecording` 订阅 `nx://agent/recorder/{session}` 并在挂上时对一次
 * `agent_record_status`。AI 面板浮出成独立窗口时，另一个窗口里的开始 / 结束同样会通过事件同步过来。
 * 草稿只存在于按下「结束录制」的那个窗口的内存里（后端不留存）。
 */

import { useCallback, useEffect, useSyncExternalStore } from "react";
import { api, onAgentRecorder } from "../api";
import { idleStatus, statusEqual, type RecorderStatus, type RecordingDraft } from "./recorder";

interface Slot {
  status: RecorderStatus;
  draft: RecordingDraft | null;
  /** 开始 / 结束的请求在途 */
  busy: boolean;
  error: string | null;
}

const errMsg = (e: unknown): string => (e instanceof Error ? e.message : String(e));

const slots = new Map<string, Slot>();
const listeners = new Set<() => void>();
const recording = new Set<string>();

function slotOf(session: string): Slot {
  let s = slots.get(session);
  if (!s) {
    s = { status: idleStatus(session), draft: null, busy: false, error: null };
    slots.set(session, s);
  }
  return s;
}

function patch(session: string, p: Partial<Slot>): void {
  const prev = slotOf(session);
  const next: Slot = { ...prev, ...p };
  if (
    next.draft === prev.draft &&
    next.busy === prev.busy &&
    next.error === prev.error &&
    statusEqual(next.status, prev.status)
  ) {
    return; // 没变：引用不动，订阅者不被无谓唤醒
  }
  slots.set(session, next);
  if (next.status.recording) recording.add(session);
  else recording.delete(session);
  listeners.forEach((l) => l());
}

/** 这个会话此刻是否在录制（同步，给终端的输入路径用，极便宜）。 */
export function isRecording(session: string): boolean {
  return recording.has(session);
}

/** 测试 / 会话关闭时清掉某会话的记录 */
export function forgetRecorder(session: string): void {
  if (slots.delete(session)) {
    recording.delete(session);
    listeners.forEach((l) => l());
  }
}

/**
 * 订阅某会话的录制状态并先对一次后端现状。返回取消函数（可在订阅完成前调用）。
 * 同一会话可以被多处同时 watch（终端 + 面板），各自独立取消。
 */
export function watchRecording(session: string): () => void {
  if (!session) return () => {};
  let disposed = false;
  let off: (() => void) | null = null;
  void onAgentRecorder(session, (st) => {
    if (!disposed) patch(session, { status: st });
  })
    .then((un) => {
      if (disposed) un();
      else off = un;
    })
    .catch(() => {});
  api
    .agentRecordStatus(session)
    .then((st) => {
      if (!disposed) patch(session, { status: st });
    })
    .catch(() => {});
  return () => {
    disposed = true;
    off?.();
  };
}

function subscribe(l: () => void): () => void {
  listeners.add(l);
  return () => listeners.delete(l);
}

export interface RecorderApi {
  status: RecorderStatus;
  /** 最近一次「结束录制」产出的草稿；`dismissDraft` 或下次开始录制时清掉 */
  draft: RecordingDraft | null;
  busy: boolean;
  error: string | null;
  start: (hostLabel?: string) => Promise<void>;
  /** 结束并拿到草稿；失败（如没在录制）返回 null，原因在 `error` */
  stop: () => Promise<RecordingDraft | null>;
  cancel: () => Promise<void>;
  dismissDraft: () => void;
}

/** 某会话的录制状态与操作。自带 `watchRecording`。 */
export function useRecorder(session: string): RecorderApi {
  const slot = useSyncExternalStore(
    subscribe,
    () => slotOf(session),
    () => slotOf(session)
  );

  useEffect(() => watchRecording(session), [session]);

  const start = useCallback(
    async (hostLabel?: string) => {
      if (!session || slotOf(session).busy) return;
      patch(session, { busy: true, error: null, draft: null });
      try {
        const st = await api.agentRecordStart(session, hostLabel);
        patch(session, { status: st, busy: false });
      } catch (e) {
        patch(session, { busy: false, error: errMsg(e) });
      }
    },
    [session]
  );

  const stop = useCallback(async () => {
    if (!session || slotOf(session).busy) return null;
    patch(session, { busy: true, error: null });
    try {
      const draft = await api.agentRecordStop(session);
      patch(session, { status: idleStatus(session), draft, busy: false });
      return draft;
    } catch (e) {
      patch(session, { busy: false, error: errMsg(e) });
      return null;
    }
  }, [session]);

  const cancel = useCallback(async () => {
    if (!session || slotOf(session).busy) return;
    patch(session, { busy: true, error: null });
    try {
      await api.agentRecordCancel(session);
      patch(session, { status: idleStatus(session), busy: false });
    } catch (e) {
      patch(session, { busy: false, error: errMsg(e) });
    }
  }, [session]);

  const dismissDraft = useCallback(() => patch(session, { draft: null, error: null }), [session]);

  return {
    status: slot.status,
    draft: slot.draft,
    busy: slot.busy,
    error: slot.error,
    start,
    stop,
    cancel,
    dismissDraft,
  };
}
