/**
 * 浮动 Nexus AI Agent 独立窗口。由后端 `agent_open_window` 开出，注入
 * `window.__NX_AGENT__ = sessionId` 作为初始会话，之后跟随 `nx://agent/active-session` 事件切换。
 *
 * 与内嵌面板的差别：这里没有 xterm 实例，「写入终端」「去设置」都改成发事件回主窗口执行。
 */
import { useEffect, useState } from "react";
import { emit, listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { AgentPanel } from "../components/AgentPanel";
import type { AgentTarget } from "../types";
import "../styles.css";

/** 主窗口推过来的「当前会话 + 可操作会话清单」 */
interface ActiveSessionPayload {
  sessionId?: string;
  sessions?: AgentTarget[];
}

export default function AgentWindow({ initialSession }: { initialSession: string }) {
  const [sessionId, setSessionId] = useState(initialSession);
  /**
   * 可操作会话清单。浮窗里没有标签树，这份清单由主窗口随 `active-session` 一起推来，
   * 「执行」模式的作用范围选择器全靠它 —— 推不来就退化成「只能操作当前会话」，
   * 那也是安全的（后端同样只认这一个），不该因为清单没到就挡住整个面板。
   */
  const [sessions, setSessions] = useState<AgentTarget[]>([]);

  useEffect(() => {
    // 主窗口切活动会话 → 浮窗跟随
    const un = listen<ActiveSessionPayload | string>("nx://agent/active-session", (e) => {
      const p = e.payload;
      // 兼容老形状（纯 sessionId 字符串）：后端在复用已开窗口时也会发一条
      if (typeof p === "string") {
        if (p) setSessionId(p);
        return;
      }
      if (p?.sessionId) setSessionId(p.sessionId);
      if (Array.isArray(p?.sessions)) setSessions(p.sessions);
    });
    // 窗口被关时通知主窗口恢复「未浮出」状态 —— 这件事由 Rust 侧的
    // Destroyed 事件做（见 agent_open_window），这里不注册 onCloseRequested：
    // 它会让原生关闭改由 JS destroy() 兜底，JS 一旦没响应窗口就关不掉。
    return () => {
      void un.then((f) => f());
    };
  }, []);

  // 浮窗里「写入终端」交回主窗口执行（主窗口才有 xterm 与标签类型信息）
  const sendToTerminal = async (sid: string, text: string) => {
    await emit("nx://agent/send-to-terminal", { sessionId: sid, text });
  };

  const openSettings = () => {
    void emit("nx://agent/open-settings");
  };

  // 独立窗口的「关闭面板」= 关掉这个窗口
  const close = () => {
    void getCurrentWindow().close();
  };

  return (
    <div className="agent-window-root">
      <AgentPanel
        sessionId={sessionId}
        visible={!!sessionId}
        onClose={close}
        onSendToTerminal={sendToTerminal}
        onOpenSettings={openSettings}
        sessions={sessions}
      />
    </div>
  );
}
