import { useState } from "react";
import type { SessionRecord } from "../types";

interface Props {
  sessions: SessionRecord[];
  /** 当前选中项。工具栏的「连接 / SFTP」按钮作用于它 */
  selectedId: string | null;
  onSelect: (s: SessionRecord) => void;
  onConnect: (s: SessionRecord) => void;
  onCreate: () => void;
  onEdit: (s: SessionRecord) => void;
  onDelete: (s: SessionRecord) => void;
}

function groupByFolder(sessions: SessionRecord[]) {
  const map = new Map<string, SessionRecord[]>();
  for (const s of sessions) {
    const key = s.folder || "/";
    if (!map.has(key)) map.set(key, []);
    map.get(key)!.push(s);
  }
  return [...map.entries()].sort((a, b) => a[0].localeCompare(b[0]));
}

export default function SessionList({
  sessions,
  selectedId,
  onSelect,
  onConnect,
  onCreate,
  onEdit,
  onDelete,
}: Props) {
  const [filter, setFilter] = useState("");
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());

  const q = filter.trim().toLowerCase();
  const filtered = q
    ? sessions.filter(
        (s) =>
          s.name.toLowerCase().includes(q) ||
          s.host.toLowerCase().includes(q) ||
          s.username.toLowerCase().includes(q)
      )
    : sessions;

  const toggle = (f: string) => {
    setCollapsed((prev) => {
      const next = new Set(prev);
      next.has(f) ? next.delete(f) : next.add(f);
      return next;
    });
  };

  return (
    <aside className="sidebar">
      <div className="sidebar-head">
        <input
          className="filter"
          placeholder="搜索会话…"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
        />
        <button className="btn-icon" onClick={onCreate} title="新建会话">
          +
        </button>
      </div>

      <div className="session-tree">
        {groupByFolder(filtered).map(([folder, items]) => (
          <div key={folder} className="folder-group">
            <button
              className="folder-row"
              onClick={() => toggle(folder)}
              title={folder}
            >
              <span className="caret">{collapsed.has(folder) ? "▸" : "▾"}</span>
              <span className="folder-name">
                {folder === "/" ? "全部会话" : folder.replace(/^\//, "")}
              </span>
              <span className="count">{items.length}</span>
            </button>

            {!collapsed.has(folder) &&
              items.map((s) => (
                <div
                  key={s.id}
                  className={`session-row ${s.id === selectedId ? "selected" : ""}`}
                >
                  <button
                    className="session-main"
                    onDoubleClick={() => onConnect(s)}
                    onClick={() => onSelect(s)}
                    title={`${s.username}@${s.host}:${s.port}`}
                  >
                    <span className="dot" />
                    <span className="s-name">{s.name}</span>
                    <span className="s-host">
                      {s.username ? `${s.username}@` : ""}
                      {s.host}
                    </span>
                  </button>
                  <button
                    className="btn-row"
                    onClick={(e) => {
                      e.stopPropagation();
                      onConnect(s);
                    }}
                    title="连接"
                  >
                    ↵
                  </button>
                  <button
                    className="btn-row"
                    onClick={(e) => {
                      e.stopPropagation();
                      onEdit(s);
                    }}
                    title="编辑"
                  >
                    ✎
                  </button>
                  <button
                    className="btn-row danger"
                    onClick={(e) => {
                      e.stopPropagation();
                      onDelete(s);
                    }}
                    title="删除"
                  >
                    ×
                  </button>
                </div>
              ))}
          </div>
        ))}

        {filtered.length === 0 && (
          <p className="empty">
            {sessions.length === 0 ? "还没有会话，点 + 新建" : "没有匹配的会话"}
          </p>
        )}
      </div>
    </aside>
  );
}
