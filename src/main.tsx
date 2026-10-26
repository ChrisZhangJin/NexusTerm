import React from "react";
import ReactDOM from "react-dom/client";
import { initPlatform } from "./lib/platform";

declare global {
  interface Window {
    /** 浏览窗口由后端注入文档 id；主窗口没有该字段 */
    __NX_VIEWER__?: string;
    /** 浮动 Agent 窗口由后端注入初始会话 id；主窗口没有该字段 */
    __NX_AGENT__?: string;
  }
}

const root = () => ReactDOM.createRoot(document.getElementById("root") as HTMLElement);

if (window.__NX_VIEWER__) {
  // 浏览窗口：只加载浏览界面与它自己的样式，不挂主应用
  const id = window.__NX_VIEWER__;
  import("./viewer/Viewer").then(({ default: Viewer }) => {
    root().render(
      <React.StrictMode>
        <Viewer id={id} />
      </React.StrictMode>
    );
  });
} else if (window.__NX_AGENT__ !== undefined) {
  // 浮动 Agent 窗口：只渲染 AI 面板，不挂主应用
  const initialSession = window.__NX_AGENT__;
  Promise.all([import("./viewer/AgentWindow"), initPlatform().catch(() => null)]).then(
    ([{ default: AgentWindow }]) => {
      root().render(
        <React.StrictMode>
          <AgentWindow initialSession={initialSession} />
        </React.StrictMode>
      );
    }
  );
} else {
  // 先问清平台再挂载：标题栏留白与窗口控制按钮都要按平台渲染，
  // 若等 App 里异步拿到，首帧会先闪一下错误的布局。
  Promise.all([import("./App"), import("./styles.css"), initPlatform().catch(() => null)]).then(
    ([{ default: App }]) => {
      root().render(
        <React.StrictMode>
          <App />
        </React.StrictMode>
      );
    }
  );
}
