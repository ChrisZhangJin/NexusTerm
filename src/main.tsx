import React from "react";
import ReactDOM from "react-dom/client";
import { initPlatform } from "./lib/platform";

declare global {
  interface Window {
    /** 阅读窗口由后端注入文档 id；主窗口没有该字段 */
    __NX_READER__?: string;
  }
}

const root = () => ReactDOM.createRoot(document.getElementById("root") as HTMLElement);

if (window.__NX_READER__) {
  // 阅读窗口：只加载阅读界面与它自己的样式，不挂主应用
  const id = window.__NX_READER__;
  import("./reader/Reader").then(({ default: Reader }) => {
    root().render(
      <React.StrictMode>
        <Reader id={id} />
      </React.StrictMode>
    );
  });
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
