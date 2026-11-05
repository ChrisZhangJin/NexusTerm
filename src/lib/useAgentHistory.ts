/**
 * Agent 面板对话的持久化 hook（Phase 2.6）：挂载时读回、变化时防抖写盘、提供「清空对话」。
 *
 * 逻辑都在 `agentHistory.ts`（纯函数，有自测）；这里只做与 React / `api` 的接线。
 * 单独成文件是为了让 `AgentPanel.tsx` 只多出一次 hook 调用。
 */

import { useCallback, useEffect, useRef } from "react";
import { api } from "../api";
import type { AgentMessage } from "../types";
import {
  createDebouncedSaver,
  fromPersisted,
  mergeRestored,
  toPersisted,
} from "./agentHistory";

/** 消息停止变化多久后写盘。流式 token 一直在变，防抖把它合成一次写。 */
const SAVE_DELAY_MS = 1500;

export function useAgentHistory(
  messages: AgentMessage[],
  /** 以「函数式更新」的方式改面板的消息列表 */
  setMessages: (fn: (prev: AgentMessage[]) => AgentMessage[]) => void
) {
  const hydrated = useRef(false);
  /** 刚从盘里读回、尚未被改动过的那份：它不需要再写回去（也避免多窗口互相覆盖） */
  const pristine = useRef<AgentMessage[] | null>(null);
  /** 盘里此刻是否有内容。没有且要存的也是空的就别写，省得清空后又冒出一行空数据 */
  const stored = useRef(false);

  const saver = useRef(
    createDebouncedSaver<AgentMessage[]>((list) => {
      void api
        .agentHistorySave(list)
        .then(() => {
          stored.current = list.length > 0;
        })
        .catch((e) => console.warn("保存 Agent 对话失败", e));
    }, SAVE_DELAY_MS)
  ).current;

  // 挂载时读回
  useEffect(() => {
    let alive = true;
    void api
      .agentHistoryLoad()
      .then((raw) => {
        if (!alive) return;
        const restored = fromPersisted(raw);
        stored.current = restored.length > 0;
        setMessages((prev) => {
          const merged = mergeRestored(prev, restored);
          // 面板此前是空的 → 合并结果就是盘里的原样，不必再写回
          if (prev.length === 0) pristine.current = merged;
          return merged;
        });
        hydrated.current = true;
      })
      .catch((e) => {
        console.warn("读取 Agent 对话失败", e);
        // 读不出来也要放行写盘，否则这一次运行的对话永远存不上
        hydrated.current = true;
      });
    return () => {
      alive = false;
    };
    // setMessages 由调用方保证稳定
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // 变化 → 防抖写盘
  useEffect(() => {
    if (!hydrated.current) return;
    if (messages === pristine.current) return;
    const list = toPersisted(messages);
    if (list.length === 0 && !stored.current) return;
    saver.schedule(list);
  }, [messages, saver]);

  // 卸载 / 页面关闭时把挂起的那次写出去
  useEffect(() => {
    const flush = () => saver.flush();
    window.addEventListener("pagehide", flush);
    return () => {
      window.removeEventListener("pagehide", flush);
      flush();
    };
  }, [saver]);

  /** 「清空对话」：丢掉挂起的写入，并真删盘上的对话与运行摘要。失败会抛。 */
  const clear = useCallback(async () => {
    saver.cancel();
    await api.agentHistoryClear();
    stored.current = false;
    pristine.current = null;
  }, [saver]);

  return { clear };
}
