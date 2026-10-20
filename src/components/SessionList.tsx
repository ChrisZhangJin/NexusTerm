import { useEffect, useMemo, useRef, useState } from "react";
import type { SessionRecord } from "../types";
import IOMenu from "./IOMenu";

interface Props {
  sessions: SessionRecord[];
  onConnect: (s: SessionRecord) => void;
  onCreate: () => void;
  onEdit: (s: SessionRecord) => void;
  onDelete: (s: SessionRecord) => void;
  onClose: () => void;
  /** 拖拽后持久化：把受影响连接的新 sortOrder + folder 写回 */
  onReorder: (items: [string, number, string][]) => void;
  /** 导出单个连接为 .xsh */
  onExport: (s: SessionRecord) => void;
  /** 导出整个分组（递归）下的所有连接 */
  onExportFolder: (sessions: SessionRecord[], folderName: string) => void;
  /** 分组改名：把 oldPath 整体改成以 newName 为末段 */
  onRenameFolder: (oldPath: string, newName: string) => void;
  /** 导入 / 导出（原工具栏按钮，收编到这里） */
  io: {
    canExportSession: boolean;
    exportSessionHint?: string;
    onImport: () => void;
    onExportSession: () => void;
    onExportAll: () => void;
  };
}

/** 规整 folder：去掉首尾斜杠，空/`/` 视为根（""）。 */
function normFolder(raw: string): string {
  return (raw || "").replace(/^\/+|\/+$/g, "").trim();
}

/** 显示名：有名字用名字，没名字用 IP。 */
function displayName(s: SessionRecord): string {
  if (s.name && s.name !== s.host) return s.name;
  return s.host;
}

/** 按 host+username 去重，保留 updatedAt 最新的。 */
function dedupe(sessions: SessionRecord[]): SessionRecord[] {
  const map = new Map<string, SessionRecord>();
  for (const s of sessions) {
    const key = `${s.host}\0${s.username}`;
    const existing = map.get(key);
    if (!existing || s.updatedAt > existing.updatedAt) map.set(key, s);
  }
  return [...map.values()];
}

// ——— 树模型 ———
interface TreeNode {
  name: string; // 本层分组名（根为 ""）
  path: string; // 完整路径，如 "生产环境/华东"
  children: TreeNode[];
  sessions: SessionRecord[];
}

function buildTree(sessions: SessionRecord[]): TreeNode {
  const root: TreeNode = { name: "", path: "", children: [], sessions: [] };
  const getNode = (segments: string[]): TreeNode => {
    let node = root;
    let acc = "";
    for (const seg of segments) {
      acc = acc ? `${acc}/${seg}` : seg;
      let child = node.children.find((c) => c.name === seg);
      if (!child) {
        child = { name: seg, path: acc, children: [], sessions: [] };
        node.children.push(child);
      }
      node = child;
    }
    return node;
  };

  for (const s of sessions) {
    const folder = normFolder(s.folder);
    const segments = folder ? folder.split("/") : [];
    getNode(segments).sessions.push(s);
  }

  const sortNode = (n: TreeNode) => {
    n.children.sort((a, b) => a.name.localeCompare(b.name));
    n.sessions.sort(
      (a, b) =>
        a.sortOrder - b.sortOrder || displayName(a).localeCompare(displayName(b))
    );
    n.children.forEach(sortNode);
  };
  sortNode(root);
  return root;
}

function allFolderPaths(node: TreeNode, acc: Set<string>) {
  for (const c of node.children) {
    acc.add(c.path);
    allFolderPaths(c, acc);
  }
}

function countSessions(node: TreeNode): number {
  return (
    node.sessions.length +
    node.children.reduce((sum, c) => sum + countSessions(c), 0)
  );
}

function collectSessions(node: TreeNode): SessionRecord[] {
  return [...node.sessions, ...node.children.flatMap((c) => collectSessions(c))];
}

/** 文件夹 SVG 图标 —— 用 currentColor，跟随主题色系。 */
function FolderIcon() {
  return (
    <svg className="folder-svg" viewBox="0 0 16 16" aria-hidden>
      <path
        fill="currentColor"
        d="M1.5 3.5A1.5 1.5 0 0 1 3 2h3.2c.4 0 .78.16 1.06.44L8.3 3.2c.1.1.23.15.36.15H13a1.5 1.5 0 0 1 1.5 1.5v7A1.5 1.5 0 0 1 13 13.3H3a1.5 1.5 0 0 1-1.5-1.5v-8.3Z"
      />
    </svg>
  );
}

// 右键菜单目标
type CtxTarget =
  | { kind: "session"; session: SessionRecord }
  | { kind: "folder"; node: TreeNode };

/**
 * 弹出式连接选择器 —— 多级分组树 + 拖拽 + 右键菜单。
 */
export default function SessionList({
  sessions,
  onConnect,
  onCreate,
  onEdit,
  onDelete,
  onClose,
  onReorder,
  onExport,
  onExportFolder,
  onRenameFolder,
  io,
}: Props) {
  const [filter, setFilter] = useState("");
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const panelRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  const [dragId, setDragId] = useState<string | null>(null);
  const [dropFolder, setDropFolder] = useState<string | null>(null);

  const [ctx, setCtx] = useState<{
    x: number;
    y: number;
    target: CtxTarget;
  } | null>(null);
  const ctxRef = useRef<HTMLDivElement>(null);
  /** 正在行内改名的分组 path */
  const [renaming, setRenamingState] = useState<string | null>(null);
  const renamingRef = useRef<string | null>(null);
  const setRenaming = (p: string | null) => {
    renamingRef.current = p;
    setRenamingState(p);
  };

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  useEffect(() => {
    const onClick = (e: MouseEvent) => {
      const t = e.target as HTMLElement;
      if (t.closest?.(".sp-toggle")) return; // 开关按钮自己负责切换
      if (panelRef.current && !panelRef.current.contains(t)) {
        onClose();
      }
    };
    requestAnimationFrame(() => document.addEventListener("mousedown", onClick));
    return () => document.removeEventListener("mousedown", onClick);
  }, [onClose]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        if (renaming) return; // 交给改名输入框自己处理
        if (ctx) setCtx(null);
        else onClose();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onClose, ctx, renaming]);

  const unique = useMemo(() => dedupe(sessions), [sessions]);

  const q = filter.trim().toLowerCase();
  const filtered = useMemo(
    () =>
      q
        ? unique.filter(
            (s) =>
              displayName(s).toLowerCase().includes(q) ||
              s.host.toLowerCase().includes(q) ||
              s.username.toLowerCase().includes(q) ||
              normFolder(s.folder).toLowerCase().includes(q)
          )
        : unique,
    [unique, q]
  );

  const tree = useMemo(() => buildTree(filtered), [filtered]);

  const searchExpanded = useMemo(() => {
    if (!q) return null;
    const s = new Set<string>();
    allFolderPaths(tree, s);
    return s;
  }, [q, tree]);

  const isCollapsed = (path: string) =>
    searchExpanded ? false : collapsed.has(path);

  const toggle = (path: string) => {
    setCollapsed((prev) => {
      const next = new Set(prev);
      next.has(path) ? next.delete(path) : next.add(path);
      return next;
    });
  };

  /**
   * 把连接移到 targetFolder；给了 anchorId 则插在该连接前/后（同组内即为排序）。
   */
  const moveSession = (
    sessionId: string,
    targetFolder: string,
    anchorId?: string,
    after?: boolean
  ) => {
    const moved = unique.find((s) => s.id === sessionId);
    if (!moved || anchorId === sessionId) return;
    const target = normFolder(targetFolder);
    if (normFolder(moved.folder) === target && !anchorId) return;

    const siblings = unique
      .filter((s) => normFolder(s.folder) === target && s.id !== sessionId)
      .sort(
        (a, b) =>
          a.sortOrder - b.sortOrder || displayName(a).localeCompare(displayName(b))
      );
    let at = siblings.length;
    if (anchorId) {
      const j = siblings.findIndex((s) => s.id === anchorId);
      if (j >= 0) at = after ? j + 1 : j;
    }
    const ordered = [...siblings.slice(0, at), moved, ...siblings.slice(at)];
    onReorder(ordered.map((s, i) => [s.id, i, target === "" ? "/" : target]));
  };

  // —— 拖拽：pointer 事件实现 ——
  // HTML5 drag 在 Tauri/WebView2 里不可靠（宿主拦截、button 子元素抢事件），
  // 改成按下手柄后监听全局 pointermove，用 elementFromPoint 命中测试落点。
  const dragStart = useRef<{ id: string; x: number; y: number; started: boolean } | null>(null);
  const [dropHint, setDropHint] = useState<{ anchorId?: string; after?: boolean } | null>(null);
  const [ghost, setGhost] = useState<{ x: number; y: number; label: string } | null>(null);
  const dropRef = useRef<{ folder: string; anchorId?: string; after?: boolean } | null>(null);

  const startDrag = (e: React.PointerEvent, id: string) => {
    if (e.button !== 0) return;
    e.preventDefault(); // 防止选中文字
    e.stopPropagation();
    dragStart.current = { id, x: e.clientX, y: e.clientY, started: false };
  };

  useEffect(() => {
    const move = (e: PointerEvent) => {
      const st = dragStart.current;
      if (!st) return;
      if (!st.started) {
        if (Math.hypot(e.clientX - st.x, e.clientY - st.y) < 4) return;
        st.started = true;
        setDragId(st.id);
        setCtx(null);
      }
      const s = unique.find((x) => x.id === st.id);
      setGhost({ x: e.clientX, y: e.clientY, label: s ? displayName(s) : "" });

      const el = document.elementFromPoint(e.clientX, e.clientY) as HTMLElement | null;
      const row = el?.closest<HTMLElement>("[data-session-id]");
      const folderRow = el?.closest<HTMLElement>("[data-drop-folder]");
      const inList = el?.closest(".sp-list");
      let drop: typeof dropRef.current = null;
      if (row) {
        const r = row.getBoundingClientRect();
        drop = {
          folder: row.dataset.folder ?? "",
          anchorId: row.dataset.sessionId,
          after: e.clientY > r.top + r.height / 2,
        };
      } else if (folderRow) {
        drop = { folder: folderRow.dataset.dropFolder ?? "" };
      } else if (inList) {
        drop = { folder: "" };
      }
      dropRef.current = drop;
      setDropFolder(drop && !drop.anchorId ? drop.folder : null);
      setDropHint(drop?.anchorId ? { anchorId: drop.anchorId, after: drop.after } : null);
    };
    const up = () => {
      const st = dragStart.current;
      dragStart.current = null;
      if (st?.started && dropRef.current) {
        const d = dropRef.current;
        moveSession(st.id, d.folder, d.anchorId, d.after);
      }
      dropRef.current = null;
      setDragId(null);
      setDropFolder(null);
      setDropHint(null);
      setGhost(null);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
    return () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
    // moveSession 依赖 unique；每次 unique 变化重绑即可
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [unique]);

  const openCtx = (e: React.MouseEvent, target: CtxTarget) => {
    e.preventDefault();
    e.stopPropagation();
    const MENU_W = 160;
    const MENU_H = 180;
    setCtx({
      x: Math.min(e.clientX, window.innerWidth - MENU_W - 8),
      y: Math.min(e.clientY, window.innerHeight - MENU_H - 8),
      target,
    });
  };

  // 点菜单外部才关闭；点菜单内部必须放行，否则 mousedown 先把菜单卸载，click 永远到不了按钮
  useEffect(() => {
    if (!ctx) return;
    const hide = (e: MouseEvent) => {
      if (ctxRef.current && ctxRef.current.contains(e.target as Node)) return;
      setCtx(null);
    };
    const id = requestAnimationFrame(() =>
      document.addEventListener("mousedown", hide)
    );
    return () => {
      cancelAnimationFrame(id);
      document.removeEventListener("mousedown", hide);
    };
  }, [ctx]);

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
        <IOMenu {...io} />
      </div>

      <div
        className={`sp-list ${dropFolder === "" && dragId ? "drop-root" : ""} ${
          dragId ? "is-dragging" : ""
        }`}
      >
        <TreeView
          node={tree}
          depth={0}
          isCollapsed={isCollapsed}
          toggle={toggle}
          onConnect={(s) => {
            onConnect(s);
            onClose();
          }}
          dragId={dragId}
          dropFolder={dropFolder}
          dropHint={dropHint}
          onStartDrag={startDrag}
          onCtx={openCtx}
          renaming={renaming}
          onRenameDone={(path, next) => {
            if (renamingRef.current !== path) return; // Enter 后 blur 二次触发
            renamingRef.current = null;
            setRenaming(null);
            const old = path.split("/").pop() ?? "";
            const v = (next ?? "").trim().replace(/\//g, "");
            if (v && v !== old) onRenameFolder(path, v);
          }}
        />

        {filtered.length === 0 && (
          <p className="empty">
            {sessions.length === 0 ? "还没有连接，点 + 新建" : "没有匹配的连接"}
          </p>
        )}
      </div>

      {ghost && (
        <div className="drag-ghost" style={{ left: ghost.x + 12, top: ghost.y + 6 }}>
          {ghost.label}
        </div>
      )}

      {ctx &&
        (ctx.target.kind === "session"
          ? (() => {
              const s = ctx.target.session;
              return (
                <div ref={ctxRef} className="ctx-menu" style={{ left: ctx.x, top: ctx.y }}>
                  <button
                    className="ctx-item"
                    onClick={() => {
                      setCtx(null);
                      onConnect(s);
                      onClose();
                    }}
                  >
                    连接
                  </button>
                  <button
                    className="ctx-item"
                    onClick={() => {
                      setCtx(null);
                      onEdit(s);
                      onClose();
                    }}
                  >
                    编辑 / 改名
                  </button>
                  <button
                    className="ctx-item"
                    onClick={() => {
                      setCtx(null);
                      onExport(s);
                    }}
                  >
                    导出为 .xsh
                  </button>
                  <div className="ctx-sep" />
                  <button
                    className="ctx-item danger"
                    onClick={() => {
                      setCtx(null);
                      onDelete(s);
                    }}
                  >
                    删除
                  </button>
                </div>
              );
            })()
          : (() => {
              const node = ctx.target.node;
              return (
                <div ref={ctxRef} className="ctx-menu" style={{ left: ctx.x, top: ctx.y }}>
                  <button
                    className="ctx-item"
                    onClick={() => {
                      setCtx(null);
                      setRenaming(node.path);
                    }}
                  >
                    改名
                  </button>
                  <button
                    className="ctx-item"
                    onClick={() => {
                      setCtx(null);
                      onExportFolder(collectSessions(node), node.name);
                    }}
                  >
                    导出分组为 .xsh
                  </button>
                </div>
              );
            })())}
    </div>
  );
}

// ——— 递归渲染树 ———
interface TreeViewProps {
  node: TreeNode;
  depth: number;
  isCollapsed: (path: string) => boolean;
  toggle: (path: string) => void;
  onConnect: (s: SessionRecord) => void;
  dragId: string | null;
  dropFolder: string | null;
  dropHint: { anchorId?: string; after?: boolean } | null;
  onStartDrag: (e: React.PointerEvent, id: string) => void;
  onCtx: (e: React.MouseEvent, target: CtxTarget) => void;
  renaming: string | null;
  /** next 为 null 表示取消 */
  onRenameDone: (path: string, next: string | null) => void;
}

function TreeView(props: TreeViewProps) {
  const {
    node,
    depth,
    isCollapsed,
    toggle,
    onConnect,
    dragId,
    dropFolder,
    dropHint,
    onStartDrag,
    onCtx,
    renaming,
    onRenameDone,
  } = props;

  return (
    <>
      {/* 子分组 */}
      {node.children.map((child) => {
        const total = countSessions(child);
        const collapsedNow = isCollapsed(child.path);
        const isDropTarget = dropFolder === child.path && !!dragId;
        if (renaming === child.path) {
          return (
            <div key={child.path} className="folder-group">
              <div
                className="folder-row renaming"
                style={{ paddingLeft: 10 + depth * 14 }}
              >
                <span className="caret">{collapsedNow ? "▸" : "▾"}</span>
                <FolderIcon />
                <input
                  className="folder-rename"
                  autoFocus
                  defaultValue={child.name}
                  onFocus={(e) => e.currentTarget.select()}
                  onKeyDown={(e) => {
                    e.stopPropagation();
                    if (e.key === "Enter") onRenameDone(child.path, e.currentTarget.value);
                    else if (e.key === "Escape") onRenameDone(child.path, null);
                  }}
                  onBlur={(e) => onRenameDone(child.path, e.currentTarget.value)}
                />
              </div>
              {!collapsedNow && (
                <TreeView {...props} node={child} depth={depth + 1} />
              )}
            </div>
          );
        }
        return (
          <div key={child.path} className="folder-group">
            <button
              className={`folder-row ${isDropTarget ? "drop-target" : ""}`}
              style={{ paddingLeft: 10 + depth * 14 }}
              onClick={() => toggle(child.path)}
              onContextMenu={(e) => onCtx(e, { kind: "folder", node: child })}
              title={child.path}
              data-drop-folder={child.path}
            >
              <span className="caret">{collapsedNow ? "▸" : "▾"}</span>
              <FolderIcon />
              <span className="folder-name">{child.name}</span>
              <span className="count">{total}</span>
            </button>

            {!collapsedNow && (
              <TreeView {...props} node={child} depth={depth + 1} />
            )}
          </div>
        );
      })}

      {/* 本层直属会话 */}
      {node.sessions.map((s) => (
        <div
          key={s.id}
          className={[
            "session-row",
            dragId === s.id ? "dragging" : "",
            dragId && dropHint?.anchorId === s.id
              ? dropHint.after
                ? "drop-after"
                : "drop-before"
              : "",
          ].join(" ")}
          style={{ paddingLeft: 6 + depth * 14 }}
          onContextMenu={(e) => onCtx(e, { kind: "session", session: s })}
          data-session-id={s.id}
          data-folder={normFolder(s.folder)}
        >
          {/* 拖拽手柄：按住拖动（整行也可在按住手柄后拖到任意位置） */}
          <span
            className="drag-handle"
            title="按住拖动：调整顺序 / 移入分组"
            onPointerDown={(e) => onStartDrag(e, s.id)}
          >
            ⠿
          </span>
          <button
            className="session-main"
            onDoubleClick={() => onConnect(s)}
            title={`${s.username}@${s.host}:${s.port}  双击连接`}
          >
            <span className="dot" />
            <span className="s-name">{displayName(s)}</span>
            <span className="s-host">
              {s.username ? `${s.username}@` : ""}
              {s.host}
            </span>
          </button>
          <button className="btn-row" onClick={() => onConnect(s)} title="连接">
            ↵
          </button>
        </div>
      ))}
    </>
  );
}
