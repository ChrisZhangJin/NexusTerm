/**
 * 所有托管任务的总表：挂载时拉一次 `host_list`，之后跟 `nx://host/all` 增量更新。
 * AgentPanel 持有唯一一份，经运行桥交给 StewardPanel。
 */

import { useCallback, useEffect, useState } from "react";
import { api, onHostAll } from "../api";
import type { HostStatus } from "../types";
import { hostForConsole, isTerminal, needsReattach, upsertHost } from "./host";

export function useHostRuns(onEnded?: (st: HostStatus) => void) {
  const [map, setMap] = useState<Record<string, HostStatus>>({});

  useEffect(() => {
    let disposed = false;
    let off: (() => void) | null = null;
    void onHostAll((st) => {
      if (disposed) return;
      setMap((m) => upsertHost(m, st));
      if (isTerminal(st.phase)) onEnded?.(st);
    }).then((f) => {
      if (disposed) f();
      else off = f;
    });
    api
      .hostList()
      .then((list) => {
        if (disposed) return;
        // 事件可能先于列表到达：已有的（更新的）不被列表里的旧值覆盖
        setMap((m) => list.reduce((acc, st) => (acc[st.contractId] ? acc : upsertHost(acc, st)), m));
      })
      .catch(() => {});
    return () => {
      disposed = true;
      off?.();
    };
    // onEnded 只在结束时对一次账，不随渲染重订阅
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const forConsole = useCallback((consoleId: string | null | undefined) => (consoleId ? hostForConsole(map, consoleId) : null), [map]);
  /** 等重新接驳的托管（重启后的 console id 已经变了，不按 console 归，单独列出） */
  const awaitingReattach = Object.values(map).filter((s) => needsReattach(s.phase));
  return { hosts: map, forConsole, awaitingReattach };
}
