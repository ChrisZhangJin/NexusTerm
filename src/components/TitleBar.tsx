interface Props {
  /** 窗口标题：通常为应用名 + 当前会话名 */
  title: string;
  themeName: string;
  onCycleTheme: () => void;
  onToggleSettings: () => void;
  settingsOpen: boolean;
}

/**
 * macOS 隐藏标题栏（titleBarStyle = Overlay + hiddenTitle）下的自绘标题栏。
 *
 * 两个硬性约束：
 * 1. 最左 80px 必须留空 —— 红黄绿交通灯就浮在这块区域上，放任何控件都会被它们压住。
 * 2. 其余区域要能拖动窗口。Tauri 的 drag.js 会沿事件冒泡路径找 `data-tauri-drag-region`，
 *    值为 `deep` 时整棵子树都生效；而 BUTTON / INPUT 等可点击元素会自动阻断拖动，
 *    所以右侧按钮天然可点，不必额外处理。
 */
export default function TitleBar({
  title,
  themeName,
  onCycleTheme,
  onToggleSettings,
  settingsOpen,
}: Props) {
  return (
    <header className="titlebar" data-tauri-drag-region="deep">
      {/* 交通灯预留区：只占位，不响应任何交互 */}
      <div className="titlebar-lights" aria-hidden />

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
    </header>
  );
}
