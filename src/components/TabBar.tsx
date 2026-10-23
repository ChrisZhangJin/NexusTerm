import { useEffect, useRef, useState } from "react";
import type { TerminalTab } from "../types";

interface Props {
  tabs: TerminalTab[];
  activeTab: string | null;
  onSelect: (id: string) => void;
  onClose: (id: string) => void;
  /** 新标签页（本地终端），插到 afterId 右侧 */
  onNewTab: (afterId?: string | null) => void;
  onReconnect: (id: string) => void;
  /** 复制会话：同一主机新开一个干净会话 */
  onDuplicate: (id: string) => void;
  /** 复制会话并携带状态：恢复当前目录与 tmux */
  onDuplicateWithState: (id: string) => void;
  /** 断开 SSH 但保留标签 */
  onDisconnect: (id: string) => void;
  /** 打开该标签对应连接的 SFTP 文件管理器 */
  onOpenSftp: (id: string) => void;
  onRename: (id: string, name: string) => void;
  /** 拖拽换位 */
  onMove: (fromId: string, toId: string, after: boolean) => void;
  /** 最左侧「☰ 连接」按钮（原工具栏唯一按钮，已并入标签栏） */
  sessionPopupOpen: boolean;
  onToggleSessionPopup: () => void;
  /** 右侧 AI 助手面板开关 */
  agentOpen: boolean;
  onToggleAgent: () => void;
  /** 宏管理器 */
  onMacros: () => void;
  macroOpen: boolean;
  /** 正在录制宏：按钮上打个红点，提醒用户「你现在的操作会被记住」 */
  recording: boolean;
}

const tabLabel = (t: TerminalTab) => t.customTitle || t.title;

/**
 * 标签栏。
 * 拖拽用 pointer 事件自己实现，不走 HTML5 drag：后者在 WebView2 里
 * 会被宿主窗口的拖放处理截走，且拖动时没有可控的视觉反馈。
 */
export default function TabBar({
  tabs,
  activeTab,
  onSelect,
  onClose,
  onNewTab,
  onReconnect,
  onDuplicate,
  onDuplicateWithState,
  onDisconnect,
  onOpenSftp,
  onRename,
  onMove,
  sessionPopupOpen,
  onToggleSessionPopup,
  agentOpen,
  onToggleAgent,
  onMacros,
  macroOpen,
  recording,
}: Props) {
  const [ctx, setCtx] = useState<{ x: number; y: number; id: string } | null>(null);
  const ctxRef = useRef<HTMLDivElement>(null);
  const [renaming, setRenaming] = useState<string | null>(null);
  const renamingRef = useRef<string | null>(null);

  // 拖拽状态
  const [drag, setDrag] = useState<{ id: string; overId: string | null; after: boolean } | null>(
    null
  );
  const dragStart = useRef<{ id: string; x: number; started: boolean } | null>(null);
  const tabEls = useRef(new Map<string, HTMLDivElement>());

  // 点菜单外关闭
  useEffect(() => {
    if (!ctx) return;
    const hide = (e: MouseEvent) => {
      if (ctxRef.current?.contains(e.target as Node)) return;
      setCtx(null);
    };
    const esc = (e: KeyboardEvent) => e.key === "Escape" && setCtx(null);
    const id = requestAnimationFrame(() => {
      document.addEventListener("mousedown", hide);
      document.addEventListener("keydown", esc);
    });
    return () => {
      cancelAnimationFrame(id);
      document.removeEventListener("mousedown", hide);
      document.removeEventListener("keydown", esc);
    };
  }, [ctx]);

  // 拖拽：全局监听移动与松开
  useEffect(() => {
    const move = (e: PointerEvent) => {
      const st = dragStart.current;
      if (!st) return;
      if (!st.started) {
        if (Math.abs(e.clientX - st.x) < 5) return; // 防误触：移动 5px 才算拖
        st.started = true;
      }
      let overId: string | null = null;
      let after = false;
      for (const [id, el] of tabEls.current) {
        const r = el.getBoundingClientRect();
        if (e.clientX >= r.left && e.clientX <= r.right) {
          overId = id;
          after = e.clientX > r.left + r.width / 2;
          break;
        }
      }
      setDrag({ id: st.id, overId, after });
    };
    const up = () => {
      const st = dragStart.current;
      dragStart.current = null;
      setDrag((d) => {
        if (st?.started && d && d.overId && d.overId !== d.id) {
          onMove(d.id, d.overId, d.after);
        }
        return null;
      });
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
    return () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
  }, [onMove]);

  const finishRename = (id: string, value: string | null) => {
    if (renamingRef.current !== id) return; // Enter 后 blur 二次触发
    renamingRef.current = null;
    setRenaming(null);
    if (value !== null) onRename(id, value);
  };

  const ctxTab = ctx ? tabs.find((t) => t.id === ctx.id) : null;
  const run = (fn: () => void) => () => {
    setCtx(null);
    fn();
  };

  return (
    <div className="tabbar">
      {/* sp-toggle：连接列表的「点外部关闭」会放行它，避免先关后开 */}
      <button
        className={`tb-action tabbar-conn sp-toggle ${sessionPopupOpen ? "active" : ""}`}
        onClick={onToggleSessionPopup}
        title="打开连接列表（新建 / 导入 / 导出也在这里）"
      >
        <span className="tb-glyph">☰</span>
        连接
      </button>
      <span className="tb-sep" />
      <div className="tabs">
        {tabs.map((t, i) => {
          const isDragging = drag?.id === t.id; // drag 只在越过阈值后才被设置
          const dropBefore = drag?.overId === t.id && drag.id !== t.id && !drag.after;
          const dropAfter = drag?.overId === t.id && drag.id !== t.id && drag.after;
          return (
            <div
              key={t.id}
              ref={(el) => {
                if (el) tabEls.current.set(t.id, el);
                else tabEls.current.delete(t.id);
              }}
              className={[
                "tab",
                t.id === activeTab ? "active" : "",
                t.status === "error" ? "tab-error" : "",
                isDragging ? "tab-dragging" : "",
                dropBefore ? "drop-before" : "",
                dropAfter ? "drop-after" : "",
              ].join(" ")}
              onPointerDown={(e) => {
                if (e.button !== 0 || renaming === t.id) return;
                if ((e.target as HTMLElement).closest(".tab-close")) return;
                dragStart.current = { id: t.id, x: e.clientX, started: false };
              }}
              onClick={() => onSelect(t.id)}
              onDoubleClick={() => {
                renamingRef.current = t.id;
                setRenaming(t.id);
              }}
              onContextMenu={(e) => {
                e.preventDefault();
                onSelect(t.id);
                setCtx({
                  x: Math.min(e.clientX, window.innerWidth - 200),
                  y: e.clientY,
                  id: t.id,
                });
              }}
              title={tabLabel(t)}
            >
              {t.status === "reconnecting" ? (
                <span className="spinner sm" title="正在重连" />
              ) : (
                <span className={`status status-${t.status}`} title={t.error ?? t.status} />
              )}
              <span className="tab-index">{i + 1}</span>
              {renaming === t.id ? (
                <input
                  className="tab-rename"
                  autoFocus
                  defaultValue={tabLabel(t)}
                  onFocus={(e) => e.currentTarget.select()}
                  onClick={(e) => e.stopPropagation()}
                  onKeyDown={(e) => {
                    e.stopPropagation();
                    if (e.key === "Enter") finishRename(t.id, e.currentTarget.value);
                    else if (e.key === "Escape") finishRename(t.id, null);
                  }}
                  onBlur={(e) => finishRename(t.id, e.currentTarget.value)}
                />
              ) : (
                <span className="tab-title">{tabLabel(t)}</span>
              )}
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
          );
        })}
        <button className="tab-new" title="新标签页（本地终端）" onClick={() => onNewTab()}>
          +
        </button>
      </div>

      {/* 宏：常驻按钮而不是只放右键菜单 —— 它是个「边试边改」的功能，
          藏进二级菜单没人找得到；录制中按钮带红点，避免用户忘了还在录 */}
      <button
        className={`tb-action tabbar-macro ${macroOpen ? "active" : ""} ${recording ? "rec" : ""}`}
        onClick={onMacros}
        title={
          recording
            ? "正在录制宏：单击打开宏管理器并停止录制"
            : "宏：录下一串操作（等什么、发什么），可一键重放或连接后自动执行"
        }
        aria-pressed={macroOpen}
      >
        <span className="tb-glyph">▤</span>
        宏
        {recording && <span className="tb-rec" aria-hidden />}
      </button>

      {/* AI 助手面板开关：靠右，与标签列表分开（.tabs 是 flex:1，把这里顶到最右） */}
      <button
        className={`tb-action tabbar-agent ${agentOpen ? "active" : ""}`}
        onClick={onToggleAgent}
        title={
          agentOpen
            ? "收起 Nexus AI Agent 面板"
            : "展开 Nexus AI Agent 面板（它能看到当前会话的终端输出，用来带上下文回答问题）"
        }
        aria-pressed={agentOpen}
      >
        <span className="tb-glyph">✦</span>
        AI
      </button>

      {ctx && ctxTab && (
        <div ref={ctxRef} className="ctx-menu" style={{ left: ctx.x, top: ctx.y }}>
          <button className="ctx-item" onClick={run(() => onNewTab(ctxTab.id))}>
            新标签页
          </button>
          <div className="ctx-sep" />
          {(ctxTab.status === "closed" || ctxTab.status === "error") && !ctxTab.local ? (
            <button className="ctx-item" onClick={run(() => onReconnect(ctxTab.id))}>
              重新连接
            </button>
          ) : !ctxTab.local ? (
            <button className="ctx-item" onClick={run(() => onDisconnect(ctxTab.id))}>
              断开
            </button>
          ) : null}
          {!ctxTab.local && (
            <button
              className="ctx-item"
              disabled={ctxTab.status !== "connected"}
              title={ctxTab.status === "connected" ? "" : "会话连接后可用"}
              onClick={run(onMacros)}
            >
              宏…
            </button>
          )}
          {!ctxTab.local && (
            <button
              className="ctx-item"
              disabled={ctxTab.status !== "connected"}
              title={ctxTab.status === "connected" ? "" : "会话连接后可用"}
              onClick={run(() => onOpenSftp(ctxTab.id))}
            >
              SFTP 文件管理
            </button>
          )}
          <div className="ctx-sep" />
          <button className="ctx-item" onClick={run(() => onDuplicate(ctxTab.id))}>
            复制会话
          </button>
          {!ctxTab.local && (
            <button
              className="ctx-item"
              title="新会话会进入当前目录，并以分组会话方式接入同一个 tmux"
              onClick={run(() => onDuplicateWithState(ctxTab.id))}
            >
              复制会话并携带状态
            </button>
          )}
          <div className="ctx-sep" />
          <button
            className="ctx-item"
            onClick={run(() => {
              renamingRef.current = ctxTab.id;
              setRenaming(ctxTab.id);
            })}
          >
            重命名
            <span className="ctx-shortcut">双击标签</span>
          </button>
          <div className="ctx-sep" />
          <button className="ctx-item danger" onClick={run(() => onClose(ctxTab.id))}>
            关闭标签
          </button>
        </div>
      )}
    </div>
  );
}
