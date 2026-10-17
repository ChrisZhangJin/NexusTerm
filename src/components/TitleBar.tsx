import { useEffect, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { isMacOS } from "../lib/platform";
import appIcon from "../assets/app-icon.png";

interface Props {
  /** 窗口标题：通常为应用名 + 当前会话名 */
  title: string;
  themeName: string;
  onCycleTheme: () => void;
  onToggleSettings: () => void;
  settingsOpen: boolean;
}

const appWindow = getCurrentWindow();

/**
 * 自绘标题栏。
 *
 * 两个硬性约束：
 * 1. macOS 是 Overlay 标题栏（系统装饰隐藏），最左 80px 必须留空 ——
 *    红黄绿交通灯浮在这块区域上，放任何控件都会被压住。
 *    Windows / Linux 没有交通灯，这段留白要去掉，且必须自绘最小化 /
 *    最大化 / 关闭（tauri.conf 里关掉了系统装饰，否则会变成两层标题栏）。
 * 2. 其余区域要能拖动窗口。Tauri 的 drag.js 会沿事件冒泡路径找
 *    `data-tauri-drag-region`，值为 `deep` 时整棵子树都生效；而 BUTTON /
 *    INPUT 等可点击元素会自动阻断拖动，所以按钮天然可点，不必额外处理。
 */
export default function TitleBar({
  title,
  themeName,
  onCycleTheme,
  onToggleSettings,
  settingsOpen,
}: Props) {
  const mac = isMacOS();
  const [maximized, setMaximized] = useState(false);

  // 非 macOS 才有自绘的最大化按钮，因此要跟踪窗口当前是否最大化
  useEffect(() => {
    if (mac) return;
    const sync = () => {
      appWindow.isMaximized().then(setMaximized).catch(() => {});
    };
    sync();
    let unlisten: (() => void) | undefined;
    appWindow
      .onResized(() => sync())
      .then((f) => {
        unlisten = f;
      })
      .catch(() => {});
    return () => unlisten?.();
  }, [mac]);

  return (
    <header className="titlebar" data-tauri-drag-region="deep">
      {/* 交通灯预留区：只占位，不响应任何交互。仅 macOS 需要 */}
      {mac && <div className="titlebar-lights" aria-hidden />}

      {/* 非 macOS 没有系统标题栏图标，补一枚应用 logo + 间距，
          否则标题直接怼到窗口左上角，Windows 应用不长这样 */}
      {!mac && (
        <img className="titlebar-appicon" src={appIcon} alt="" aria-hidden />
      )}

      <div className="titlebar-title">{title}</div>

      <div className="titlebar-actions">
        <button
          className="tb-btn"
          onClick={onCycleTheme}
          title={`当前配色：${themeName}（点击切换）`}
        >
          {themeName}
        </button>
        <button
          className={`tb-btn ${settingsOpen ? "active" : ""}`}
          onClick={onToggleSettings}
          title="外观设置"
        >
          ⚙
        </button>
      </div>

      {/* 非 macOS 平台没有系统标题栏按钮，自绘一套放在最右（Windows 习惯） */}
      {!mac && (
        <div className="titlebar-controls">
          <button
            className="win-btn"
            onClick={() => appWindow.minimize().catch(() => {})}
            title="最小化"
            aria-label="最小化"
          >
            <svg viewBox="0 0 10 10" aria-hidden>
              <rect x="0" y="4.5" width="10" height="1" />
            </svg>
          </button>
          <button
            className="win-btn"
            onClick={() => appWindow.toggleMaximize().catch(() => {})}
            title={maximized ? "向下还原" : "最大化"}
            aria-label={maximized ? "向下还原" : "最大化"}
          >
            {maximized ? (
              <svg viewBox="0 0 10 10" aria-hidden>
                <rect
                  x="0.5"
                  y="2.5"
                  width="7"
                  height="7"
                  fill="none"
                  strokeWidth="1"
                />
                <path d="M2.5 2.5 V0.5 H9.5 V7.5 H7.5" fill="none" strokeWidth="1" />
              </svg>
            ) : (
              <svg viewBox="0 0 10 10" aria-hidden>
                <rect
                  x="0.5"
                  y="0.5"
                  width="9"
                  height="9"
                  fill="none"
                  strokeWidth="1"
                />
              </svg>
            )}
          </button>
          <button
            className="win-btn win-btn-close"
            onClick={() => appWindow.close().catch(() => {})}
            title="关闭"
            aria-label="关闭"
          >
            <svg viewBox="0 0 10 10" aria-hidden>
              <path d="M0.5 0.5 L9.5 9.5 M9.5 0.5 L0.5 9.5" strokeWidth="1" />
            </svg>
          </button>
        </div>
      )}
    </header>
  );
}
