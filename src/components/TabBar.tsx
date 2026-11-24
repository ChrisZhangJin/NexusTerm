import { useEffect, useRef, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import type { TerminalTab } from "../types";
import { isMacOS } from "../lib/platform";
import { t, useLocale } from "../i18n";

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
  /** 最左侧「☰ 连接」按钮 */
  sessionPopupOpen: boolean;
  onToggleSessionPopup: () => void;
  /** 右侧 AI 助手面板开关 */
  agentOpen: boolean;
  onToggleAgent: () => void;
  /** 托管在等人的终端：console id（= tab.sessionId）→ 一行说明（见 lib/host 的 hostTabBadges） */
  hostBadges?: Record<string, string>;
}

const appWindow = getCurrentWindow();

const tabLabel = (t: TerminalTab) => t.customTitle || t.autoTitle || t.title;

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
  hostBadges,
}: Props) {
  useLocale();
  const mac = isMacOS();
  const [ctx, setCtx] = useState<{ x: number; y: number; id: string } | null>(null);
  const ctxRef = useRef<HTMLDivElement>(null);
  const [renaming, setRenaming] = useState<string | null>(null);
  const renamingRef = useRef<string | null>(null);
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

  /**
   * macOS 全屏时系统会隐藏红绿灯，80px 的预留就变成标题栏左边空一块；
   * 写 data-fullscreen 让 CSS 收回 --lights-w 并补回栏左内边距。
   * 只在 mac 上跑：别的平台 --lights-w 本来就是 0。
   */
  useEffect(() => {
    if (!mac) return;
    const root = document.documentElement;
    const sync = () => {
      appWindow
        .isFullscreen()
        .then((fs) => {
          root.dataset.fullscreen = fs ? "1" : "0";
        })
        .catch(() => {
          root.dataset.fullscreen = "0";
        });
    };
    sync();
    let unlisten: (() => void) | undefined;
    appWindow
      .onResized(() => sync())
      .then((f) => {
        unlisten = f;
      })
      .catch(() => {});
    return () => {
      unlisten?.();
      delete root.dataset.fullscreen;
    };
  }, [mac]);

  const [drag, setDrag] = useState<{ id: string; overId: string | null; after: boolean } | null>(
    null
  );
  const dragStart = useRef<{ id: string; x: number; started: boolean } | null>(null);
  const tabEls = useRef(new Map<string, HTMLDivElement>());

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
    <div className="tabbar" data-tauri-drag-region="deep">
      {/* 交通灯预留区：只占位，不响应交互 */}
      {mac && <div className="titlebar-lights" aria-hidden />}

      {/* sp-toggle：连接列表的「点外部关闭」会放行它，避免先关后开 */}
      <button
        className={`tb-action tabbar-conn sp-toggle ${sessionPopupOpen ? "active" : ""}`}
        onClick={onToggleSessionPopup}
        title={t("打开连接列表（新建 / 导入 / 导出也在这里）", "Open the connection list (new / import / export are here too)")}
      >
        <span className="tb-glyph">☰</span>
        {t("连接", "Connections")}
      </button>
      <div className="tabs">
        {tabs.map((tab) => {
          const isDragging = drag?.id === tab.id; // drag 只在越过阈值后才被设置
          const dropBefore = drag?.overId === tab.id && drag.id !== tab.id && !drag.after;
          const dropAfter = drag?.overId === tab.id && drag.id !== tab.id && drag.after;
          const hostBadge = tab.sessionId ? hostBadges?.[tab.sessionId] : undefined;
          return (
            <div
              key={tab.id}
              ref={(el) => {
                if (el) tabEls.current.set(tab.id, el);
                else tabEls.current.delete(tab.id);
              }}
              className={[
                "tab",
                tab.id === activeTab ? "active" : "",
                renaming === tab.id ? "renaming" : "",
                tab.status === "error" ? "tab-error" : "",
                // 不在前台时收到过新输出：点亮提醒动效。已经是当前标签就不用再提醒自己。
                tab.hasActivity && tab.id !== activeTab ? "tab-activity" : "",
                hostBadge ? "tab-host-wait" : "",
                isDragging ? "tab-dragging" : "",
                dropBefore ? "drop-before" : "",
                dropAfter ? "drop-after" : "",
              ].join(" ")}
              // .tabbar 是 deep 拖动区，而标签是普通 div，不像 BUTTON/INPUT 那样被
              // Tauri 自动排除：单击会被吞、双击触发系统最大化/还原。显式关掉。
              data-tauri-drag-region="false"
              onPointerDown={(e) => {
                if (e.button !== 0 || renaming === tab.id) return;
                if ((e.target as HTMLElement).closest(".tab-close")) return;
                dragStart.current = { id: tab.id, x: e.clientX, started: false };
              }}
              onClick={() => onSelect(tab.id)}
              onDoubleClick={() => {
                renamingRef.current = tab.id;
                setRenaming(tab.id);
              }}
              onContextMenu={(e) => {
                e.preventDefault();
                onSelect(tab.id);
                setCtx({
                  x: Math.min(e.clientX, window.innerWidth - 200),
                  y: e.clientY,
                  id: tab.id,
                });
              }}
              title={hostBadge ? `${tabLabel(tab)}\n${hostBadge}` : tabLabel(tab)}
            >
              {tab.status === "reconnecting" && <span className="spinner sm" title={t("正在重连", "Reconnecting")} />}
              {hostBadge && <span className="tab-host-dot" aria-label={hostBadge} />}
              {renaming === tab.id ? (
                <input
                  className="tab-rename"
                  autoFocus
                  defaultValue={tabLabel(tab)}
                  onFocus={(e) => e.currentTarget.select()}
                  onClick={(e) => e.stopPropagation()}
                  onKeyDown={(e) => {
                    e.stopPropagation();
                    if (e.key === "Enter") finishRename(tab.id, e.currentTarget.value);
                    else if (e.key === "Escape") finishRename(tab.id, null);
                  }}
                  onBlur={(e) => finishRename(tab.id, e.currentTarget.value)}
                />
              ) : (
                <span className="tab-title">{tabLabel(tab)}</span>
              )}
              <button
                className="tab-close"
                onClick={(e) => {
                  e.stopPropagation();
                  onClose(tab.id);
                }}
                title={t("关闭", "Close")}
              >
                ×
              </button>
            </div>
          );
        })}
        <button className="tab-new" title={t("新标签页（本地终端）", "New tab (local terminal)")} onClick={() => onNewTab()}>
          +
        </button>
      </div>

      {/* 靠右：.tabs 是 flex:1，把这个按钮顶到最右 */}
      <button
        className={`tb-action tabbar-agent ${agentOpen ? "active" : ""}`}
        onClick={onToggleAgent}
        title={
          agentOpen
            ? t("收起 Nexus AI Agent 面板", "Hide the Nexus AI Agent panel")
            : t(
                "展开 Nexus AI Agent 面板（它能看到当前会话的终端输出，用来带上下文回答问题）",
                "Show the Nexus AI Agent panel (it sees this session's terminal output and answers with that context)"
              )
        }
        aria-pressed={agentOpen}
      >
        <span className="tb-glyph">✦</span>
        AI
      </button>

      {/* 非 macOS 平台没有系统标题栏按钮，自绘一套放在最右（Windows 习惯） */}
      {!mac && (
        <div className="titlebar-controls">
          <button
            className="win-btn"
            onClick={() => appWindow.minimize().catch(() => {})}
            title={t("最小化", "Minimize")}
            aria-label={t("最小化", "Minimize")}
          >
            <svg viewBox="0 0 10 10" aria-hidden>
              <rect x="0" y="4.5" width="10" height="1" />
            </svg>
          </button>
          <button
            className="win-btn"
            onClick={() => appWindow.toggleMaximize().catch(() => {})}
            title={maximized ? t("向下还原", "Restore") : t("最大化", "Maximize")}
            aria-label={maximized ? t("向下还原", "Restore") : t("最大化", "Maximize")}
          >
            {maximized ? (
              <svg viewBox="0 0 10 10" aria-hidden>
                <rect x="0.5" y="2.5" width="7" height="7" fill="none" strokeWidth="1" />
                <path d="M2.5 2.5 V0.5 H9.5 V7.5 H7.5" fill="none" strokeWidth="1" />
              </svg>
            ) : (
              <svg viewBox="0 0 10 10" aria-hidden>
                <rect x="0.5" y="0.5" width="9" height="9" fill="none" strokeWidth="1" />
              </svg>
            )}
          </button>
          <button
            className="win-btn win-btn-close"
            onClick={() => appWindow.close().catch(() => {})}
            title={t("关闭", "Close")}
            aria-label={t("关闭", "Close")}
          >
            <svg viewBox="0 0 10 10" aria-hidden>
              <path d="M0.5 0.5 L9.5 9.5 M9.5 0.5 L0.5 9.5" strokeWidth="1" />
            </svg>
          </button>
        </div>
      )}

      {ctx && ctxTab && (
        <div ref={ctxRef} className="ctx-menu" style={{ left: ctx.x, top: ctx.y }}>
          {(ctxTab.status === "closed" || ctxTab.status === "error") && !ctxTab.local ? (
            <button className="ctx-item" onClick={run(() => onReconnect(ctxTab.id))}>
              {t("重新连接", "Reconnect")}
            </button>
          ) : !ctxTab.local ? (
            <button className="ctx-item" onClick={run(() => onDisconnect(ctxTab.id))}>
              {t("断开", "Disconnect")}
            </button>
          ) : null}
          {!ctxTab.local && (
            <button
              className="ctx-item"
              disabled={ctxTab.status !== "connected"}
              title={ctxTab.status === "connected" ? "" : t("会话连接后可用", "Available once the session is connected")}
              onClick={run(() => onOpenSftp(ctxTab.id))}
            >
              {t("SFTP 文件管理", "SFTP file manager")}
            </button>
          )}
          <div className="ctx-sep" />
          <button className="ctx-item" onClick={run(() => onDuplicate(ctxTab.id))}>
            {t("复制会话", "Duplicate session")}
          </button>
          {!ctxTab.local && (
            <button
              className="ctx-item"
              title={t(
                "新会话会进入当前目录，并以分组会话方式接入同一个 tmux",
                "The new session opens in the current directory and joins the same tmux as a grouped session"
              )}
              onClick={run(() => onDuplicateWithState(ctxTab.id))}
            >
              {t("复制会话并携带状态", "Duplicate with state")}
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
            {t("重命名", "Rename")}
            <span className="ctx-shortcut">{t("双击标签", "Double-click tab")}</span>
          </button>
          <div className="ctx-sep" />
          <button className="ctx-item danger" onClick={run(() => onClose(ctxTab.id))}>
            {t("关闭标签", "Close tab")}
          </button>
        </div>
      )}
    </div>
  );
}
