/**
 * 浮动 Nexus AI Agent 独立窗口。
 *
 * 由后端 `agent_open_window` 开出，注入 `window.__NX_AGENT__ = sessionId` 作为初始会话。
 * 之后主窗口切换活动标签时，后端/主窗口发 `nx://agent/active-session` 事件，这里跟着切。
 *
 * 与内嵌面板的唯一差别：
 * - 「把命令写入终端」在独立窗口里没有 xterm 实例，改为发 `nx://agent/send-to-terminal`
 *   事件给主窗口，由主窗口用它已有的 sendToTerminal（知道标签是本地还是 SSH）落地。
 * - 「去设置」发 `nx://agent/open-settings` 事件，让主窗口弹设置弹窗。
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
    // 窗口被关（点红灯 / Cmd+W / 代码 close）时通知主窗口恢复「未浮出」状态
    const unClose = getCurrentWindow().onCloseRequested(() => {
      void emit("nx://agent/window-closed");
    });
    return () => {
      void un.then((f) => f());
      void unClose.then((f) => f());
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
