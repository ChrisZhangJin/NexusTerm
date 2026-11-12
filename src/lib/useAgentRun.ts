/**
 * Agent 一轮运行的状态 + 事件订阅 + 控制动作（从 AgentPanel 抽出）。
 *
 * **全应用只有一份实例**，由 AgentPanel 持有：「对话」与「托管」两个视图读同一份 `conv`、
 * 调同一组动作。这样事件只被订阅一次、审批 / 密码卡片只会被答一次 —— 不存在两处各订阅一份、
 * 同一个审批弹两次的情况。托管视图只是把 `conv.messages` 的一段切出来画（见 `sliceFromMessage`）。
 */

import { useCallback, useEffect, useState } from "react";
import type { UnlistenFn } from "@tauri-apps/api/event";
import {
  api,
  onAgentApprove,
  onAgentControl,
  onAgentDone,
  onAgentError,
  onAgentSecret,
  onAgentStep,
  onAgentToken,
} from "../api";
import { toFriendlyError } from "./errors";
import { applyControl, drivingSessions, pausedSessions, type ControlState } from "./control";
import {
  EMPTY_CONV,
  withApprove,
  withConclusion,
  withDecision,
  withDone,
  withError,
  withPauseState,
  withSecret,
  withSecretResult,
  withStep,
  withToken,
  type Conversation,
} from "./agentRun";
import type { AgentApproveRequest, AgentSecretRequest } from "../types";

/** 「停止时同时发 Ctrl-C」的本机偏好（"1" = 开，默认关） */
const LS_STOP_CTRL_C = "nx.agent.stopCtrlC";

export function useAgentRun(opts: {
  /** 当前绑定的会话（取消 / 暂停 / 继续在没有 runSession 时的兜底） */
  sessionId: string;
  /** 动作失败时的一行错误文案（由调用方决定怎么显示） */
  onError: (msg: string) => void;
}) {
  const { sessionId, onError } = opts;
  const [conv, setConv] = useState<Conversation>(EMPTY_CONV);
  /**
   * 这一轮实际发起的会话。事件订阅挂在它上面，而**不跟随 sessionId** ——
   * 用户切到别的标签时，正在跑的那条流仍要收得到，否则答案会断在半截。
   */
  const [runSession, setRunSession] = useState<string | null>(null);
  /** 本轮涉及的各会话的控制权状态（只留活跃的；userDriving 的会被删掉） */
  const [control, setControl] = useState<Record<string, ControlState>>({});
  const pausedIds = pausedSessions(control);
  const drivingIds = drivingSessions(control);
  /** 「停止」时是否同时给被驾驶的终端发 Ctrl-C。默认关，选择记在本机 */
  const [stopCtrlC, setStopCtrlC] = useState<boolean>(
    () => localStorage.getItem(LS_STOP_CTRL_C) === "1"
  );
  const toggleStopCtrlC = (on: boolean) => {
    setStopCtrlC(on);
    localStorage.setItem(LS_STOP_CTRL_C, on ? "1" : "0");
  };

  const patch = useCallback((fn: (c: Conversation) => Conversation) => {
    setConv((prev) => fn(prev));
  }, []);

  // ——— 事件订阅：挂在这一轮发起的会话上 ———
  // **不跟随面板可见性**：后端那一轮不会因为面板收起而停，收起期间发出的审批 / 密码卡片
  // 若没人订阅就丢了（审批干等到超时），「忙」标记也会被误清、允许在同一会话上再起一轮。
  // AgentPanel 始终挂载（App 里只切 visible），本 hook 在它的 early return 之前调用，
  // 订阅随组件整个生命周期存在；只有 runSession 变化 / 组件真卸载才退订。
  useEffect(() => {
    if (!runSession) return;
    let disposed = false;
    const unlisteners: UnlistenFn[] = [];

    void (async () => {
      try {
        const [offToken, offDone, offError, offStep, offApprove, offSecret, offControl] = await Promise.all([
          onAgentToken(runSession, (chunk) => {
            if (!disposed) patch((c) => withToken(c, chunk));
          }),
          onAgentDone(runSession, (p) => {
            if (!disposed) patch((c) => withDone(c, p.reason));
          }),
          onAgentError(runSession, (msg) => {
            if (!disposed) patch((c) => withError(c, msg));
          }),
          onAgentStep(runSession, (step) => {
            if (disposed) return;
            // kind=answer 是这一轮的总结，作为一条完整的助手消息落地并收掉 busy；
            // 其余都当作过程卡片逐条追加。
            patch((c) =>
              step.kind === "answer"
                ? withConclusion(c, step.text ?? "")
                : withPauseState(withStep(c, step), step)
            );
          }),
          onAgentApprove(runSession, (req) => {
            if (!disposed) patch((c) => withApprove(c, req));
          }),
          onAgentSecret(runSession, (req) => {
            if (!disposed) patch((c) => withSecret(c, req));
          }),
          onAgentControl(runSession, (p) => {
            if (!disposed) setControl((s) => applyControl(s, p));
          }),
        ]);
        // 竞态：listen 是异步的，等它 resolve 时这一轮可能已经结束了 —— 这时要立刻退订
        if (disposed) {
          offToken();
          offDone();
          offError();
          offStep();
          offApprove();
          offSecret();
          offControl();
          return;
        }
        unlisteners.push(offToken, offDone, offError, offStep, offApprove, offSecret, offControl);
        // 订阅之前就已经在跑的一轮（面板刚打开 / 重新挂载）：对一次账，补上发起会话的状态
        api
          .agentControlGet(runSession)
          .then((st) => {
            if (!disposed) setControl((s) => applyControl(s, { session: runSession, state: st }));
          })
          .catch(() => {});
      } catch {
        // 订阅失败：提问时会在 agentRun 上抛出真错误，这里不必再弹一次
      }
    })();

    return () => {
      disposed = true;
      unlisteners.forEach((f) => f());
      // 订阅断了就收不到后续变化，留着旧状态只会让横幅卡在屏幕上
      setControl({});
    };
  }, [runSession, patch]);

  // ——— 控制动作 ———

  const cancel = async () => {
    const sid = runSession ?? sessionId;
    if (!sid) return;
    try {
      // 幂等：没有在跑时后端什么都不做
      await api.agentRunCancel(sid, stopCtrlC);
    } catch (e) {
      onError(`取消失败：${toFriendlyError(e).message}`);
    }
  };

  /** 手动「暂停」：当前这一步做完后停下，把终端还给用户。 */
  const pauseRun = async () => {
    const sid = runSession ?? sessionId;
    if (!sid) return;
    try {
      await api.agentRunPause(sid);
    } catch (e) {
      onError(`暂停失败：${toFriendlyError(e).message}`);
    }
  };

  /** 「继续」：同一轮、同一段对话，换一份新预算接着做。 */
  const resumeRun = async () => {
    const sid = runSession ?? sessionId;
    if (!sid) return;
    try {
      await api.agentRunResume(sid);
    } catch (e) {
      onError(`继续失败：${toFriendlyError(e).message}`);
    }
  };

  /** 「接管」：让 Agent 立刻让路（等同于自己敲了键）。对所有正被驾驶的会话生效。 */
  const takeover = () => {
    for (const id of drivingIds) void api.agentControlTakeover(id).catch(() => {});
  };
  /** 「交还」：Paused → Agent 继续。 */
  const handback = () => {
    for (const id of pausedIds) void api.agentControlHandback(id).catch(() => {});
  };

  /** 用户在审批卡片上的选择。后端收不到（超时）也照样把卡片置成终态。 */
  const decide = async (req: AgentApproveRequest, approved: boolean) => {
    try {
      await api.agentApprove(req.requestId, approved);
    } catch (e) {
      onError(`提交审批结果失败：${toFriendlyError(e).message}`);
      return;
    }
    patch((c) => withDecision(c, req.requestId, approved));
  };

  /**
   * 用户在密码卡片上的选择：`value` 给了就是提交密码，`undefined` 是跳过。
   * 值经 `agent_secret_ack` 直达后端写进 pty —— 不进对话 state、不进模型上下文。
   * 返回 `null` 表示已处理（卡片已置成终态），否则返回要在卡片内联显示的错误文案
   * （例如密码含控制字符被后端拒绝：这种情况后端没消耗登记，卡片保持打开可重输）。
   */
  const answerSecret = async (
    req: AgentSecretRequest,
    value?: string
  ): Promise<string | null> => {
    try {
      const accepted = await api.agentSecretAck(req.requestId, value);
      // false：这个请求已经超时 / 被取消 / 重复提交，后端不再等了
      patch((c) =>
        withSecretResult(c, req.requestId, accepted ? (value === undefined ? "skipped" : "submitted") : "expired")
      );
      return null;
    } catch (e) {
      return toFriendlyError(e).message;
    }
  };

  return {
    conv,
    setConv,
    patch,
    runSession,
    setRunSession,
    pausedIds,
    drivingIds,
    stopCtrlC,
    toggleStopCtrlC,
    cancel,
    pauseRun,
    resumeRun,
    takeover,
    handback,
    decide,
    answerSecret,
  };
}

export type AgentRunApi = ReturnType<typeof useAgentRun>;
