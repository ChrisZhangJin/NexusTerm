/**
 * Agent 面板对话的持久化 hook：**按连接分段**（见 `convKeyOf`），每段各存一份。
 *
 * - 某段对话第一次被看到时从盘里读回；
 * - 任何一段的消息变了就防抖写盘（包括面板此刻没在看、但还在跑的那段）；
 * - 「清空对话」只清面板此刻在看的那一段。
 *
 * 逻辑都在 `agentHistory.ts`（纯函数，有自测）；这里只做与 React / `api` 的接线。
 */

import { useCallback, useEffect, useRef } from "react";
import { api } from "../api";
import type { AgentMessage } from "../types";
import type { Conversation } from "./agentRun";
import { createDebouncedSaver, fromPersisted, mergeRestored, toPersisted } from "./agentHistory";

/** 消息停止变化多久后写盘。流式 token 一直在变，防抖把它合成一次写。 */
const SAVE_DELAY_MS = 1500;

type Saver = ReturnType<typeof createDebouncedSaver<AgentMessage[]>>;

export function useAgentHistory(
  convs: Record<string, Conversation>,
  /** 面板此刻在看的那一段 */
  viewKey: string,
  /** 以「函数式更新」的方式改某一段对话 */
  patchIn: (key: string, fn: (c: Conversation) => Conversation) => void
) {
  /** 已经从盘里读回过的段（读回之前不写，免得空的盖掉盘上的） */
  const hydrated = useRef(new Set<string>());
  /** 每段上一次写过 / 读回的那份消息数组：没变就不写 */
  const last = useRef(new Map<string, AgentMessage[]>());
  /** 每段盘上此刻有没有内容（没有且要存的也是空的就别写） */
  const stored = useRef(new Map<string, boolean>());
  const savers = useRef(new Map<string, Saver>());

  const saverFor = useCallback((key: string): Saver => {
    let s = savers.current.get(key);
    if (!s) {
      s = createDebouncedSaver<AgentMessage[]>((list) => {
        void api
          .agentHistorySave(list, key)
          .then(() => stored.current.set(key, list.length > 0))
          .catch((e) => console.warn("保存 Agent 对话失败", e));
      }, SAVE_DELAY_MS);
      savers.current.set(key, s);
    }
    return s;
  }, []);

  // 看到一段新的对话：读回它
  useEffect(() => {
    const key = viewKey;
    if (hydrated.current.has(key)) return;
    let alive = true;
    void api
      .agentHistoryLoad(key)
      .then((raw) => {
        if (!alive) return;
        const restored = fromPersisted(raw);
        stored.current.set(key, restored.length > 0);
        patchIn(key, (c) => {
          const merged = mergeRestored(c.messages, restored);
          // 这段此前是空的 → 合并结果就是盘里的原样，不必再写回
          if (c.messages.length === 0) last.current.set(key, merged);
          return { ...c, messages: merged };
        });
        hydrated.current.add(key);
      })
      .catch((e) => {
        console.warn("读取 Agent 对话失败", e);
        // 读不出来也要放行写盘，否则这一段的对话永远存不上
        if (alive) hydrated.current.add(key);
      });
    return () => {
      alive = false;
    };
  }, [viewKey, patchIn]);

  // 任何一段变了 → 防抖写盘
  useEffect(() => {
    for (const [key, c] of Object.entries(convs)) {
      if (!hydrated.current.has(key)) continue;
      if (c.messages === last.current.get(key)) continue;
      last.current.set(key, c.messages);
      const list = toPersisted(c.messages);
      if (list.length === 0 && !stored.current.get(key)) continue;
      saverFor(key).schedule(list);
    }
  }, [convs, saverFor]);

  // 卸载 / 页面关闭时把挂起的写入都写出去
  useEffect(() => {
    const flush = () => savers.current.forEach((s) => s.flush());
    window.addEventListener("pagehide", flush);
    return () => {
      window.removeEventListener("pagehide", flush);
      flush();
    };
  }, []);

  /** 「清空对话」：只清面板此刻在看的那一段（盘上的对话、模型历史与运行摘要）。失败会抛。 */
  const clear = useCallback(async () => {
    const key = viewKey;
    savers.current.get(key)?.cancel();
    await api.agentHistoryClear(key);
    stored.current.set(key, false);
    last.current.delete(key);
  }, [viewKey]);

  return { clear };
}
