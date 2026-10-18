import { useEffect, useRef, useState } from "react";
import type { SessionRecord } from "../types";

interface Props {
  sessions: SessionRecord[];
  onConnect: (s: SessionRecord) => void;
  onCreate: () => void;
  onEdit: (s: SessionRecord) => void;
  onDelete: (s: SessionRecord) => void;
  onClose: () => void;
}

/** 按 host+username 去重，保留最新的那条（updatedAt 最大）。 */
function dedupe(sessions: SessionRecord[]): SessionRecord[] {
  const map = new Map<string, SessionRecord>();
  for (const s of sessions) {
    const key = `${s.host}\0${s.username}`;
    const existing = map.get(key);
    if (!existing || s.updatedAt > existing.updatedAt) {
      map.set(key, s);
    }
  }
  return [...map.values()];
}

/** 显示名：有名字用名字，没有名字用 IP。 */
function displayName(s: SessionRecord): string {
  // 名字非空且不等于 host 时用名字
  if (s.name && s.name !== s.host) return s.name;
  return s.host;
}

/**
 * 弹出式连接选择器 —— 类似 Xshell 的「打开」按钮。
 * 按 host+username 去重，按名字排序（无名字用 IP），不分文件夹。
 */
export default function SessionList({
  sessions,
  onConnect,
  onCreate,
  onEdit,
  onDelete,
  onClose,
}: Props) {
  const [filter, setFilter] = useState("");
  const panelRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  // 打开时自动聚焦搜索框
  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  // 点击面板外部关闭
  useEffect(() => {
    const onClick = (e: MouseEvent) => {
      if (panelRef.current && !panelRef.current.contains(e.target as Node)) {
        onClose();
      }
    };
    requestAnimationFrame(() => document.addEventListener("mousedown", onClick));
    return () => document.removeEventListener("mousedown", onClick);
  }, [onClose]);

  // ESC 关闭
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onClose]);

  // 去重 + 搜索 + 排序
  const unique = dedupe(sessions);
  const q = filter.trim().toLowerCase();
  const filtered = q
    ? unique.filter(
        (s) =>
          displayName(s).toLowerCase().includes(q) ||
          s.host.toLowerCase().includes(q) ||
          s.username.toLowerCase().includes(q)
      )
    : unique;
  const sorted = [...filtered].sort((a, b) =>
    displayName(a).localeCompare(displayName(b))
  );

  return (
    <div className="session-popup" ref={panelRef}>
      <div className="sp-head">
        <input
          ref={inputRef}
          className="filter"
          placeholder="搜索连接…"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
        />
        <button className="btn-icon" onClick={onCreate} title="新建连接">
          +
        </button>
      </div>

      <div className="sp-list">
        {sorted.map((s) => (
          <div key={s.id} className="session-row">
            <button
              className="session-main"
              onDoubleClick={() => {
                onConnect(s);
                onClose();
              }}
              title={`${s.username}@${s.host}:${s.port}  双击连接`}
            >
              <span className="dot" />
              <span className="s-name">{displayName(s)}</span>
              <span className="s-host">
                {s.username ? `${s.username}@` : ""}
                {s.host}
              </span>
            </button>
            <button
              className="btn-row"
              onClick={() => {
                onConnect(s);
                onClose();
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
                onClose();
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

        {sorted.length === 0 && (
          <p className="empty">
            {sessions.length === 0 ? "还没有连接，点 + 新建" : "没有匹配的连接"}
          </p>
        )}
      </div>
    </div>
  );
}
