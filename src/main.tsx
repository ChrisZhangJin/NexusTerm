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
  // 保险库状态同理：等它回来再挂载，才不会先闪一下「一条密码都没有」的界面。
  // 浮动 Agent 窗口那条分支不加闸 —— 主密码归主窗口管。
  Promise.all([
    import("./App"),
    import("./styles.css"),
    initPlatform().catch(() => null),
    import("./components/VaultGate"),
    import("./api")
      .then(({ api }) => api.vaultStatus())
      .catch((e) => {
        // 按「未启用」处理：这时后端多半也起不来，拦在闸口只会让用户连自救界面都打不开
        console.warn("读取保险库状态失败，按未启用处理：", e);
        return null;
      }),
  ]).then(([{ default: App }, , , { default: VaultGate }, vault]) => {
    root().render(
      <React.StrictMode>
        <VaultGate initial={vault}>
          <App />
        </VaultGate>
      </React.StrictMode>
    );
  });
}
