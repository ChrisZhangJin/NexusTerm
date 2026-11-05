/**
 * Agent 面板对话的持久化辅助（Phase 2.6）。纯逻辑，不碰 `api`，便于在 node 里自测。
 *
 * 范围：面板对话是**全局唯一一份**（切标签不换聊天记录），所以持久化也是全局一份，
 * 不按标签 / 会话分桶 —— 标签 id 每次启动都会变，分桶重启后一份都对不上。
 *
 * 落盘前的脱敏、截断、条数 / 体积上限由后端统一做（`agent::history::sanitize_messages`）；
 * 这里只负责「哪些消息值得存」和「读回来之后怎样不骗人」：
 * - 流式中的半截回答不存（不可信，和切会话时被打断的处理一致）；
 * - 读回来时，重载前还没收尾的东西要收尾 —— 否则界面上会永远转圈 / 永远等一个已不存在的审批。
 */

import type { AgentMessage } from "../types";

/** 前端先砍一刀，省得白白把一大坨 IPC 发过去；权威上限在后端。 */
export const MAX_PERSISTED_MESSAGES = 200;

/** 重载前被打断的步骤卡上补的说明 */
export const INTERRUPTED_NOTE = "（面板重载前被中断，没有拿到结果）";

/** 选出要落盘的消息：去掉还在流式追加的半截回答，只留最近 N 条。 */
export function toPersisted(messages: AgentMessage[]): AgentMessage[] {
  const kept = messages.filter((m) => !m.streaming);
  return kept.length > MAX_PERSISTED_MESSAGES
    ? kept.slice(kept.length - MAX_PERSISTED_MESSAGES)
    : kept;
}

function isMessage(x: unknown): x is AgentMessage {
  if (!x || typeof x !== "object") return false;
  const m = x as Record<string, unknown>;
  return (
    typeof m.id === "string" &&
    (m.role === "user" || m.role === "assistant") &&
    typeof m.text === "string" &&
    typeof m.ts === "number"
  );
}

/**
 * 把后端读回的原始数组还原成消息，并把「重载前没收尾的东西」收尾：
 * - 结构不对的条目丢弃（库里的数据可能来自别的版本）；
 * - 不会有 `streaming`；
 * - 没有答复的审批卡 → 按「未批准」（后端的审批 300s 超时本来就按拒绝处理，那一轮早已不在了）；
 * - 没有结局的密码卡 → `expired`；没有结局的密码步骤 → `cancelled`；
 * - 进行中的 exec / keys / wait / screen 步骤 → 补成「被中断」的已收尾态。
 */
export function fromPersisted(raw: unknown): AgentMessage[] {
  if (!Array.isArray(raw)) return [];
  const out: AgentMessage[] = [];
  for (const item of raw) {
    if (!isMessage(item)) continue;
    let m: AgentMessage = { ...item };
    delete m.streaming;
    if (m.approve && m.approveResult === undefined) {
      m = { ...m, approveResult: false };
    }
    if (m.secret && m.secretResult === undefined) {
      m = { ...m, secretResult: "expired" };
    }
    if (m.step) {
      const s = m.step;
      if (s.kind === "exec" && s.output === undefined) {
        m = { ...m, step: { ...s, kind: "result", output: INTERRUPTED_NOTE } };
      } else if (
        (s.kind === "keys" || s.kind === "wait" || s.kind === "screen") &&
        s.output === undefined
      ) {
        m = { ...m, step: { ...s, output: "", text: s.text ?? INTERRUPTED_NOTE } };
      } else if (s.kind === "secret" && s.secretStatus === undefined) {
        m = { ...m, step: { ...s, secretStatus: "cancelled" } };
      }
    }
    out.push(m);
  }
  return out.length > MAX_PERSISTED_MESSAGES
    ? out.slice(out.length - MAX_PERSISTED_MESSAGES)
    : out;
}

/**
 * 把读回的历史并进当前对话。读盘是异步的：等它回来时用户可能已经发了新消息，
 * 这时旧历史排在前面、新消息接在后面，谁也不丢。
 */
export function mergeRestored(
  current: AgentMessage[],
  restored: AgentMessage[]
): AgentMessage[] {
  if (restored.length === 0) return current;
  // 面板还是空的：原样返回读回的那份（同一个引用），调用方据此认出「没被改动过」
  if (current.length === 0) return restored;
  const have = new Set(current.map((m) => m.id));
  return [...restored.filter((m) => !have.has(m.id)), ...current];
}

/**
 * 防抖保存器：消息频繁变化时（流式 token、步骤卡）合并成一次写盘。
 * `flush` 立刻把挂起的那次写出去（卸载 / 页面关闭时用）；`cancel` 丢弃挂起的写入（清空对话时用）。
 */
export function createDebouncedSaver<T>(
  save: (v: T) => void,
  delayMs: number,
  timers: {
    set: (fn: () => void, ms: number) => unknown;
    clear: (h: unknown) => void;
  } = {
    set: (fn, ms) => setTimeout(fn, ms),
    clear: (h) => clearTimeout(h as ReturnType<typeof setTimeout>),
  }
) {
  let handle: unknown = null;
  let latest: { v: T } | null = null;
  const fire = () => {
    handle = null;
    if (!latest) return;
    const { v } = latest;
    latest = null;
    save(v);
  };
  return {
    schedule(v: T) {
      latest = { v };
      if (handle !== null) timers.clear(handle);
      handle = timers.set(fire, delayMs);
    },
    flush() {
      if (handle !== null) timers.clear(handle);
      fire();
    },
    cancel() {
      if (handle !== null) timers.clear(handle);
      handle = null;
      latest = null;
    },
    pending: () => latest !== null,
  };
}
