import { modKeyLabel } from "../lib/platform";
import IOMenu from "./IOMenu";

interface Props {
  canConnect: boolean;
  canDisconnect: boolean;
  canSftp: boolean;
  sftpOpen: boolean;
  settingsOpen: boolean;
  sidebarVisible: boolean;
  canExportSession: boolean;
  onCreate: () => void;
  onConnect: () => void;
  onDisconnect: () => void;
  onToggleSftp: () => void;
  onToggleSettings: () => void;
  onToggleSidebar: () => void;
  onImport: () => void;
  onExportSession: () => void;
  onExportAll: () => void;
}

/** Xshell 风格工具栏：会话与连接的常用动作。 */
export default function Toolbar({
  canConnect,
  canDisconnect,
  canSftp,
  sftpOpen,
  settingsOpen,
  sidebarVisible,
  canExportSession,
  onCreate,
  onConnect,
  onDisconnect,
  onToggleSftp,
  onToggleSettings,
  onToggleSidebar,
  onImport,
  onExportSession,
  onExportAll,
}: Props) {
  const modKey = modKeyLabel();
  return (
    <div className="toolbar">
      <button
        className={`tb-action ${sidebarVisible ? "active" : ""}`}
        onClick={onToggleSidebar}
        title={`${sidebarVisible ? "收起" : "展开"}会话侧栏（${modKey}+B）`}
      >
        <span className="tb-glyph">☰</span>
        会话
      </button>

      <span className="tb-sep" />

      <button className="tb-action" onClick={onCreate} title="新建会话">
        <span className="tb-glyph">＋</span>
        新建连接
      </button>

      <span className="tb-sep" />

      <button
        className="tb-action"
        onClick={onConnect}
        disabled={!canConnect}
        title="连接左侧选中的会话"
      >
        <span className="tb-glyph">⌁</span>
        连接
      </button>

      <button
        className="tb-action"
        onClick={onDisconnect}
        disabled={!canDisconnect}
        title="断开当前标签"
      >
        <span className="tb-glyph">⏏</span>
        断开
      </button>

      <span className="tb-sep" />

      <button
        className={`tb-action ${sftpOpen ? "active" : ""}`}
        onClick={onToggleSftp}
        disabled={!canSftp && !sftpOpen}
        title="在终端与 SFTP 文件管理器之间切换"
      >
        <span className="tb-glyph">⇅</span>
        SFTP
      </button>

      <span className="tb-spacer" />

      <IOMenu
        canExportSession={canExportSession}
        onImport={onImport}
        onExportSession={onExportSession}
        onExportAll={onExportAll}
      />

      <button
        className={`tb-action ${settingsOpen ? "active" : ""}`}
        onClick={onToggleSettings}
        title="外观设置"
      >
        <span className="tb-glyph">⚙</span>
        外观
      </button>
    </div>
  );
}
