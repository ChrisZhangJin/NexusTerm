import type { TerminalTab } from "../types";

interface Props {
  tabs: TerminalTab[];
  activeTab: string | null;
  onSelect: (id: string) => void;
  onClose: (id: string) => void;
}

export default function TabBar({ tabs, activeTab, onSelect, onClose }: Props) {
  return (
    <div className="tabbar">
      <div className="tabs">
        {tabs.map((t) => (
          <div
            key={t.id}
            className={`tab ${t.id === activeTab ? "active" : ""} ${
              t.status === "error" ? "tab-error" : ""
            }`}
            onClick={() => onSelect(t.id)}
          >
            <span
              className={`status status-${t.status}`}
              title={t.error ?? t.status}
            />
            <span className="tab-title">{t.title}</span>
            <button
              className="tab-close"
              onClick={(e) => {
                e.stopPropagation();
                onClose(t.id);
              }}
              title="关闭"
            >
              ×
            </button>
          </div>
        ))}
        {tabs.length === 0 && (
          <span className="tab-empty">双击左侧会话新建连接</span>
        )}
      </div>
    </div>
  );
}
