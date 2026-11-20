/**
 * 托管实况：**每个被托管的终端一份**（步骤 / 密码卡片 / 结论）。
 *
 * 对话页的 `useAgentRun` 全应用只有一份、跟着「最近一轮」走；托管却可以同时开在好几个终端上，
 * 共用那一份会让后开的托管把先开的实况顶掉、面板也只认最近那一轮。所以托管的实况单独按终端订阅：
 * 每个终端的 `nx://agent/step/{id}` 等事件进它自己的 `Conversation`（与对话页同一套 reducer）。
 *
 * 订阅谁由调用方给（活着的托管 + 本次会话里开过托管的终端）；终端从列表里消失就退订，实况保留到下次开始。
 */

import { useCallback, useEffect, useRef, useState } from "react";
import type { UnlistenFn } from "@tauri-apps/api/event";
import { api, onAgentDone, onAgentError, onAgentSecret, onAgentStep } from "../api";
import { toFriendlyError } from "./errors";
import {
  EMPTY_CONV,
  withConclusion,
  withDone,
  withError,
  withPauseState,
  withSecret,
  withSecretResult,
  withStep,
  type Conversation,
} from "./agentRun";
import type { AgentSecretRequest } from "../types";

let seq = 0;
const uid = () => `hf-${Date.now().toString(36)}-${(seq++).toString(36)}`;

export function useHostFeeds(consoles: readonly string[]) {
  const [feeds, setFeeds] = useState<Record<string, Conversation>>({});
  const subs = useRef(new Map<string, () => void>());

  const patch = useCallback((id: string, fn: (c: Conversation) => Conversation) => {
    setFeeds((all) => ({ ...all, [id]: fn(all[id] ?? EMPTY_CONV) }));
  }, []);

  // 订阅集合跟着 consoles 增减：新出现的订上，消失的退订（已订的不重订，事件不断流）
  const key = [...new Set(consoles)].sort().join("\n");
  useEffect(() => {
    const want = new Set(key ? key.split("\n") : []);
    for (const [id, off] of subs.current) {
      if (!want.has(id)) {
        off();
        subs.current.delete(id);
      }
    }
    for (const id of want) {
      if (subs.current.has(id)) continue;
      let disposed = false;
      let offs: UnlistenFn[] = [];
      subs.current.set(id, () => {
        disposed = true;
        offs.forEach((f) => f());
      });
      void Promise.all([
        onAgentStep(id, (step) => {
          if (!disposed)
            patch(id, (c) =>
              step.kind === "answer" ? withConclusion(c, step.text ?? "") : withPauseState(withStep(c, step), step)
            );
        }),
        onAgentSecret(id, (req) => {
          if (!disposed) patch(id, (c) => withSecret(c, req));
        }),
        onAgentError(id, (msg) => {
          if (!disposed) patch(id, (c) => withError(c, msg));
        }),
        onAgentDone(id, (p) => {
          if (!disposed) patch(id, (c) => withDone(c, p.reason));
        }),
      ])
        .then((list) => {
          // listen 是异步的：resolve 时可能已经退订了
          if (disposed) list.forEach((f) => f());
          else offs = list;
        })
        .catch(() => {});
    }
  }, [key, patch]);

  // 卸载时全部退订
  useEffect(
    () => () => {
      for (const off of subs.current.values()) off();
      subs.current.clear();
    },
    []
  );

  /** 新开一轮托管：这个终端的实况从一条「托管目标」开始（上一轮的清掉） */
  const begin = useCallback((id: string, text: string) => {
    setFeeds((all) => ({
      ...all,
      [id]: { busy: false, interrupted: false, messages: [{ id: uid(), role: "user", text, ts: Date.now() }] },
    }));
  }, []);

  /** 密码卡片：值经 `agent_secret_ack` 直达后端写进 pty，不进对话 state（与对话页同一条路） */
  const answerSecret = useCallback(
    async (id: string, req: AgentSecretRequest, value?: string): Promise<string | null> => {
      try {
        const accepted = await api.agentSecretAck(req.requestId, value);
        patch(id, (c) =>
          withSecretResult(c, req.requestId, accepted ? (value === undefined ? "skipped" : "submitted") : "expired")
        );
        return null;
      } catch (e) {
        return toFriendlyError(e).message;
      }
    },
    [patch]
  );

  return { feed: (id: string): Conversation => feeds[id] ?? EMPTY_CONV, begin, patch, answerSecret };
}
