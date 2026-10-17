import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { initPlatform } from "./lib/platform";
import "./styles.css";

// 先问清平台再挂载：标题栏留白与窗口控制按钮都要按平台渲染，
// 若等 App 里异步拿到，首帧会先闪一下错误的布局。
initPlatform().finally(() => {
  ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
    <React.StrictMode>
      <App />
    </React.StrictMode>
  );
});
