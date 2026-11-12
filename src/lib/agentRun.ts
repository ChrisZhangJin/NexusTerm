/**
 * Agent 一轮运行的对话状态与纯 reducer（从 AgentPanel 抽出，供「对话」与「托管」两个视图共用）。
 *
 * 全是纯函数：事件 → 新的 `Conversation`。订阅与 IPC 在 `useAgentRun`，渲染在 `RunFeed`。
 */

import { nextPause, type PauseKind } from "./agentBudget";
import type {
  AgentApproveRequest,
  AgentDoneReason,
  AgentMessage,
  AgentSecretRequest,
  AgentStep,
} from "../types";

/** 单个会话的对话状态。按 sessionId 分桶，切会话时彼此完全隔离。 */
export interface Conversation {
  messages: AgentMessage[];
  /** 这个会话正在等一次回答的流结束 */
  busy: boolean;
  /** 历史遗留：曾用于「收起面板打断了流」。订阅现已不随面板可见性断开，正常路径不再置位 */
  interrupted: boolean;
  /**
   * 这一轮此刻停在暂停上（预算用尽 / 手动暂停）。后端同一轮还占着会话、等「继续」或「停止」。
   * 只从 `paused` / `resumed` 步骤推出；一轮结束时对话状态被整体重建，标记随之消失。
   */
  paused?: PauseKind;
}

export const EMPTY_CONV: Conversation = { messages: [], busy: false, interrupted: false };

/** 撞到 max_tokens 时追加在答案末尾的说明 */
export const TRUNCATED_HINT = "\n\n— 回答被长度限制截断，可以让它继续，或缩小问题范围再问一次。";
/** 一个字都没收到就 done 了（被过滤 / 空回复）：不能给用户留一个空气泡 */
export const EMPTY_ANSWER = "（本次没有返回内容）";
export const CANCELLED_ANSWER = "（已取消）";
/** 一轮跑完却没等到结论（多半是用户点了停止）时补的那句话 */
export const NO_CONCLUSION = "（这一轮没有给出结论：上面的步骤就是它做过的全部事情。）";

let seq = 0;
/** 纯前端的消息 id。不用 crypto.randomUUID：Tauri 的 WebView 不保证是安全上下文。 */
export const uid = () => `m${Date.now().toString(36)}-${(seq++).toString(36)}`;

/** 「手和眼」类步骤：发按键 / 等输出 / 读屏幕 / 盯屏等待（托管模式的 `watch`）。output 为空 = 进行中。 */
export function isHandEyeKind(kind: string): boolean {
  return kind === "keys" || kind === "wait" || kind === "screen" || kind === "watch";
}

/**
 * 托管视图只展示「托管那一轮」的消息：从 `fromId`（托管起手时挂上的那条目标消息）起往后。
 * 找不到（对话被清空 / 历史被替换）就当没有，宁可空也不把无关的聊天记录冒充成托管过程。
 * 起手那条用户消息本身不返回（目标已在输入框里）。
 */
export function sliceFromMessage(messages: AgentMessage[], fromId: string | null): AgentMessage[] {
  if (!fromId) return [];
  const i = messages.findIndex((m) => m.id === fromId);
  return i < 0 ? [] : messages.slice(i + 1);
}

/** 还在等用户点头 / 输入的卡片数（审批 + 密码），托管视图顶上提示用 */
export function pendingPrompts(messages: AgentMessage[]): number {
  let n = 0;
  for (const m of messages) {
    if (m.approve && m.approveResult === undefined) n++;
    else if (m.secret && m.secretResult === undefined) n++;
  }
  return n;
}

/** 收到一个增量文本块：追加到当前正在流的那条助手消息上 */
export function withToken(c: Conversation, chunk: string): Conversation {
  const msgs = c.messages.slice();
  const last = msgs[msgs.length - 1];
  if (last && last.role === "assistant" && last.streaming) {
    msgs[msgs.length - 1] = { ...last, text: last.text + chunk };
    return { ...c, messages: msgs };
  }
  // 正常路径下提问时已经先建好占位消息；真走到这儿说明后端漏了顺序，补一条别丢字
  msgs.push({ id: uid(), role: "assistant", text: chunk, ts: Date.now(), streaming: true });
  return { ...c, messages: msgs };
}

/**
 * 收到 done：这是唯一的正常收尾信号（后端保证一定发，取消时 reason=cancelled）。
 * 前端不做任何超时兜底 —— 猜出来的结束会和后端状态打架。
 */
export function withDone(c: Conversation, reason: AgentDoneReason): Conversation {
  return {
    messages: c.messages.map((m) => {
      if (!m.streaming) return m;
      const text = reason === "length" ? m.text + TRUNCATED_HINT : m.text;
      return {
        ...m,
        streaming: false,
        text: text.trim()
          ? text
          : reason === "cancelled"
            ? CANCELLED_ANSWER
            : EMPTY_ANSWER,
      };
    }),
    busy: false,
    interrupted: false,
  };
}

/** 收到 error：终态，其后不会再有 done。把当前助手消息整条换成错误文案。 */
export function withError(c: Conversation, msg: string): Conversation {
  const msgs = c.messages.slice();
  const idx = msgs.findIndex((m) => m.role === "assistant" && m.streaming);
  if (idx >= 0) {
    msgs[idx] = { ...msgs[idx], streaming: false, error: true, text: msg };
  } else {
    msgs.push({ id: uid(), role: "assistant", text: msg, ts: Date.now(), error: true });
  }
  return { messages: msgs, busy: false, interrupted: false };
}

/**
 * 追加一个过程步骤。`result` 会回填到它对应的那条 `exec` 上合成**一张**卡片：
 * 拆成两张会把对话拉长一倍，也看不出哪个结果属于哪条命令。
 */
export function withStep(c: Conversation, step: AgentStep): Conversation {
  const msgs = c.messages.slice();
  if (step.kind === "result") {
    for (let i = msgs.length - 1; i >= 0; i--) {
      const m = msgs[i];
      const s = m.step;
      if (
        s &&
        s.kind === "exec" &&
        s.output === undefined &&
        s.target === step.target &&
        s.command === step.command
      ) {
        msgs[i] = {
          ...m,
          step: { ...s, kind: "result", output: step.output, promptKind: step.promptKind ?? s.promptKind },
        };
        return { ...c, messages: msgs };
      }
    }
  }
  // 密码通道：收尾步骤（带 secretStatus）并回同一目标、同一理由的进行中卡片。
  // 注意不能用 output 判断进行中——收尾时可能没有输出尾部（undefined）。
  if (step.kind === "secret" && step.secretStatus !== undefined) {
    for (let i = msgs.length - 1; i >= 0; i--) {
      const m = msgs[i];
      const s = m.step;
      if (
        s &&
        s.kind === "secret" &&
        s.secretStatus === undefined &&
        s.target === step.target &&
        s.command === step.command
      ) {
        msgs[i] = { ...m, step };
        return { ...c, messages: msgs };
      }
    }
  }
  // 盯屏心跳：后端每 ~60s 发一条同 target+command、output 为空的 watch 步骤。
  // 已有同款进行中卡片就原地更新（不另起卡，否则一小时 ~60 张转圈卡）；没有才是新开的一次盯屏。
  if (step.kind === "watch" && step.output === undefined) {
    for (let i = msgs.length - 1; i >= 0; i--) {
      const m = msgs[i];
      const s = m.step;
      if (
        s &&
        s.kind === "watch" &&
        s.output === undefined &&
        s.target === step.target &&
        s.command === step.command
      ) {
        msgs[i] = { ...m, step };
        return { ...c, messages: msgs };
      }
    }
  }
  // 「手和眼」四类卡（按键 / 等待 / 读屏 / 盯屏）：收尾（output 有值）合并回同一目标、同一内容的进行中卡片，
  // 与 exec → result 同理，别把一次动作拆成两张卡。
  if (
    isHandEyeKind(step.kind) &&
    step.output !== undefined
  ) {
    for (let i = msgs.length - 1; i >= 0; i--) {
      const m = msgs[i];
      const s = m.step;
      if (
        s &&
        s.kind === step.kind &&
        s.output === undefined &&
        s.target === step.target &&
        s.command === step.command
      ) {
        msgs[i] = { ...m, step };
        return { ...c, messages: msgs };
      }
    }
  }
  // 技能回放进度卡：同一步（同 command）的进行中卡片原地更新为收尾；重试时上一张已收尾，另起一张。
  if (step.kind === "skill") {
    for (let i = msgs.length - 1; i >= 0; i--) {
      const s = msgs[i].step;
      if (s && s.kind === "skill" && s.target === step.target && s.command === step.command) {
        if (s.output === undefined) msgs[i] = { ...msgs[i], step };
        else msgs.push({ id: uid(), role: "assistant", text: "", ts: Date.now(), step });
        return { ...c, messages: msgs };
      }
    }
  }
  // 纯展示用的「还在等」心跳：合并进对应的 exec 卡片，不单独起一张卡；
  // 对应的卡片已经收尾（或从没出现过，比如面板是后打开的）就直接丢弃，
  // 不能退化成下面的"推一条新消息"，那样会在对话里冒出一张没头没尾的孤卡。
  if (step.kind === "waiting") {
    for (let i = msgs.length - 1; i >= 0; i--) {
      const m = msgs[i];
      const s = m.step;
      if (
        s &&
        s.kind === "exec" &&
        s.output === undefined &&
        s.target === step.target &&
        s.command === step.command
      ) {
        msgs[i] = { ...m, step: { ...s, elapsedSecs: step.elapsedSecs } };
        return { ...c, messages: msgs };
      }
    }
    return c;
  }
  msgs.push({ id: uid(), role: "assistant", text: "", ts: Date.now(), step });
  return { ...c, messages: msgs };
}

/** 追加一张待批卡片。真正的执行由用户在卡片上点头后才继续。 */
export function withApprove(c: Conversation, req: AgentApproveRequest): Conversation {
  return {
    ...c,
    messages: [...c.messages, { id: uid(), role: "assistant", text: "", ts: Date.now(), approve: req }],
  };
}

/** 记录用户对某张待批卡片的答复（卡片自己据此变成「已允许 / 已拒绝」的终态）。 */
export function withDecision(
  c: Conversation,
  requestId: string,
  approved: boolean
): Conversation {
  return {
    ...c,
    messages: c.messages.map((m) =>
      m.approve?.requestId === requestId ? { ...m, approveResult: approved } : m
    ),
  };
}

/** 追加一张密码卡片（模型请求用户输入密码）。卡片里的输入值只在组件本地 state，不进会话。 */
export function withSecret(c: Conversation, req: AgentSecretRequest): Conversation {
  return {
    ...c,
    messages: [...c.messages, { id: uid(), role: "assistant", text: "", ts: Date.now(), secret: req }],
  };
}

/** 记录密码卡片的结局（卡片据此变成终态，输入框消失） */
export function withSecretResult(
  c: Conversation,
  requestId: string,
  result: "submitted" | "skipped" | "expired"
): Conversation {
  return {
    ...c,
    messages: c.messages.map((m) =>
      m.secret?.requestId === requestId ? { ...m, secretResult: result } : m
    ),
  };
}

/** 暂停 / 继续步骤改变对话的暂停标记（其余步骤原样，不产生新对象）。 */
export function withPauseState(c: Conversation, step: AgentStep): Conversation {
  const next = nextPause(c.paused, step);
  return next === c.paused ? c : { ...c, paused: next };
}

/** Agentic 一轮的结束语落地：既不置也不用补 markup，直接作为一条完整助手消息。 */
export function withConclusion(c: Conversation, text: string): Conversation {
  const trimmed = text.trim();
  return {
    busy: false,
    interrupted: false,
    messages: [
      ...c.messages,
      { id: uid(), role: "assistant", text: trimmed || EMPTY_ANSWER, ts: Date.now() },
    ],
  };
}

/**
 * 一轮结束却**没等到**结论时兜的那一步（多半是用户点了停止）。判据是「busy 仍为
 * true」：有结论的话 kind=answer 走 `withConclusion` 早把它置回 false 了。
 * 不补这一步界面会永远转圈。
 */
export function finishWithoutAnswer(c: Conversation): Conversation {
  return withConclusion(c, NO_CONCLUSION);
}
