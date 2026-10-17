import { useCallback, useEffect, useRef, useState } from "react";
import SessionList from "./components/SessionList";
import SessionDialog from "./components/SessionDialog";
import TerminalView from "./components/TerminalView";
import FileManager from "./components/FileManager";
import TitleBar from "./components/TitleBar";
import Toolbar from "./components/Toolbar";
import TabBar from "./components/TabBar";
import StatusBar from "./components/StatusBar";
import { api } from "./api";
import { resolveAuth } from "./lib/auth";
import { normalizeTimeout, toFriendlyError } from "./lib/errors";
import { BUILTIN_THEMES, DEFAULT_THEME, FONT_FAMILIES } from "./lib/theme";
import { applyChromeVars } from "./lib/chrome";
import { isMacOS, modKeyLabel } from "./lib/platform";
import type {
  AuthMethod,
  NewSession,
  SessionRecord,
  TerminalTab,
} from "./types";

let tabSeq = 0;
const nextId = () => `tab-${Date.now()}-${++tabSeq}`;

const THEME_NAMES = Object.keys(BUILTIN_THEMES);

export default function App() {
  const [sessions, setSessions] = useState<SessionRecord[]>([]);
  const [tabs, setTabs] = useState<TerminalTab[]>([]);
  const [activeTab, setActiveTab] = useState<string | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [dialogOpen, setDialogOpen] = useState(false);
  const [editing, setEditing] = useState<SessionRecord | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  // 非空时展示该会话的 SFTP 文件管理器
  const [fmSession, setFmSession] = useState<SessionRecord | null>(null);

  const [themeName, setThemeName] = useState(DEFAULT_THEME);
  const [fontFamily, setFontFamily] = useState(FONT_FAMILIES[0].value);
  const [fontSize, setFontSize] = useState(13);
  const [lineHeight, setLineHeight] = useState(1.2);
  // 侧边栏可收起，把宽度全部让给终端
  const [sidebarVisible, setSidebarVisible] = useState(true);

  const passphraseRef = useRef<Map<string, string>>(new Map());
  // 右键菜单始终作用于「打开那一刻」的会话与标签页集合，用 ref 拿到最新快照
  const tabsRef = useRef(tabs);
  tabsRef.current = tabs;

  // 会话列表右键菜单：屏幕坐标 + 目标会话
  const [ctxMenu, setCtxMenu] = useState<{
    x: number;
    y: number;
    session: SessionRecord;
  } | null>(null);

  const openCtxMenu = useCallback((e: React.MouseEvent, s: SessionRecord) => {
    e.preventDefault();
    e.stopPropagation();
    // 简单防溢出：贴着窗口右/下边缘时往回收
    const MENU_W = 160;
    const MENU_H = 110;
    setCtxMenu({
      x: Math.min(e.clientX, window.innerWidth - MENU_W - 8),
      y: Math.min(e.clientY, window.innerHeight - MENU_H - 8),
      session: s,
    });
  }, []);

  const closeCtxMenu = useCallback(() => setCtxMenu(null), []);

  // —— 轻量提示条（导入/导出/复制会话等结果） ——
  // 放在组件上部：duplicateSession 等回调要引用 showToast，晚定义会 TDZ
  const [toast, setToast] = useState<string | null>(null);
  const toastTimer = useRef<number | undefined>(undefined);
  const showToast = useCallback((msg: string) => {
    setToast(msg);
    if (toastTimer.current) window.clearTimeout(toastTimer.current);
    toastTimer.current = window.setTimeout(() => setToast(null), 5000);
  }, []);

  const theme = BUILTIN_THEMES[themeName] ?? BUILTIN_THEMES[DEFAULT_THEME];

  // 外壳（标题栏/工具栏/侧栏/状态栏）跟随主题一起换色
  useEffect(() => {
    applyChromeVars(theme);
  }, [theme]);

  // ⌘B 收起 / 展开侧边栏
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "b") {
        e.preventDefault();
        setSidebarVisible((v) => !v);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  // 屏蔽 WebView2 的默认右键菜单（就是那个「返回 / 刷新 / 打印」系统菜单）。
  // 输入类控件保留原生菜单 —— 那里的复制粘贴是有用的；终端区域的右键由
  // TerminalView 自己接管成「粘贴」。
  useEffect(() => {
    const onCtx = (e: MouseEvent) => {
      const t = e.target as HTMLElement | null;
      if (t?.closest("input, textarea, select, [contenteditable='true']")) return;
      e.preventDefault();
    };
    document.addEventListener("contextmenu", onCtx);
    return () => document.removeEventListener("contextmenu", onCtx);
  }, []);

  const refresh = useCallback(async () => {
    try {
      setSessions(await api.listSessions());
    } catch (e) {
      console.error("加载会话失败", e);
    }
  }, []);

  useEffect(() => {
    refresh();
    api.health().then((h) => console.log("[nexus-term]", h)).catch(() => {});
  }, [refresh]);

  /** 建立连接并开启标签页 */
  const connect = useCallback(async (s: SessionRecord) => {
    const tabId = nextId();
    setTabs((prev) => [
      ...prev,
      { id: tabId, title: s.name, sessionId: s.id, status: "connecting" },
    ]);
    setActiveTab(tabId);

    const fail = (msg: string) =>
      setTabs((prev) =>
        prev.map((t) => (t.id === tabId ? { ...t, status: "error", error: msg } : t))
      );

    try {
      const auth: AuthMethod = await resolveAuth(
        s,
        passphraseRef.current.get(s.id)
      );

      await api.sshConnect(tabId, {
        host: s.host,
        port: s.port,
        username: s.username,
        auth,
        cols: 120,
        rows: 32,
        connectTimeoutSecs: normalizeTimeout(s.connectTimeout),
      });

      setTabs((prev) =>
        prev.map((t) => (t.id === tabId ? { ...t, status: "connected" } : t))
      );
    } catch (e) {
      fail(String(e));
    }
  }, []);

  const closeTab = useCallback((tabId: string) => {
    setTabs((prev) => {
      const t = prev.find((x) => x.id === tabId);
      if (t?.local) api.localClose(tabId).catch(() => {});
      else api.sshClose(tabId).catch(() => {});
      return prev;
    });
    setTabs((prev) => {
      const next = prev.filter((t) => t.id !== tabId);
      setActiveTab((cur) =>
        cur === tabId ? (next[next.length - 1]?.id ?? null) : cur
      );
      return next;
    });
  }, []);

  const saveSession = useCallback(
    async (payload: NewSession, password: string) => {
      if (editing) {
        await api.updateSession(editing.id, payload);
        if (password) await api.savePassword(editing.id, password);
      } else {
        const created = await api.createSession(payload);
        if (password) await api.savePassword(created.id, password);
      }
      await refresh();
    },
    [editing, refresh]
  );

  const removeSession = useCallback(
    async (s: SessionRecord) => {
      // 密码存在各平台的系统凭据服务里（Keychain / 凭据管理器 / Secret Service）
      if (!confirm(`删除会话「${s.name}」？此操作同时清除系统凭据服务中的密码。`)) return;
      await api.deleteSession(s.id);
      if (selectedId === s.id) setSelectedId(null);
      await refresh();
    },
    [refresh, selectedId]
  );

  /** 复制会话：以该会话为模板立即创建一条新记录。密码存于系统凭据服务、
   *  与会话 id 绑定，因此无法随复制迁移，需用户编辑补填。 */
  const duplicateSession = useCallback(
    async (s: SessionRecord) => {
      try {
        await api.createSession({
          name: `${s.name} - 副本`,
          folder: s.folder || undefined,
          host: s.host,
          port: s.port,
          username: s.username,
          authType: s.authType,
          keyPath: s.keyPath ?? undefined,
          startupCmd: s.startupCmd ?? undefined,
          theme: s.theme ?? undefined,
          connectTimeout: s.connectTimeout,
        });
        await refresh();
        showToast(`已复制「${s.name}」。密码不会随会话复制，需要时请编辑补填。`);
      } catch (e) {
        showToast(`复制失败：${String(e)}`);
      }
    },
    [refresh, showToast]
  );

  /** 关闭该会话打开的所有标签页（右键菜单的「关闭会话」）。 */
  const closeSessionTabs = useCallback((s: SessionRecord) => {
    const closing = tabsRef.current.filter((t) => t.sessionId === s.id);
    if (closing.length === 0) return;
    closing.forEach((t) => api.sshClose(t.id).catch(() => {}));
    const next = tabsRef.current.filter((t) => t.sessionId !== s.id);
    setTabs(next);
    setActiveTab((cur) =>
      closing.some((t) => t.id === cur) ? (next[next.length - 1]?.id ?? null) : cur
    );
  }, []);

  const cycleTheme = useCallback(() => {
    setThemeName((cur) => {
      const i = THEME_NAMES.indexOf(cur);
      return THEME_NAMES[(i + 1) % THEME_NAMES.length] ?? DEFAULT_THEME;
    });
  }, []);

  // —— 启动时开一个本地 shell：没有 SSH 连接时也能当终端用 ——
  const localSpawned = useRef(false);
  useEffect(() => {
    if (localSpawned.current) return;
    localSpawned.current = true;
    const id = nextId();
    // 先开标签再 spawn：TerminalView 挂载后会 fit 并纠正这里的初始尺寸
    setTabs((prev) => [
      ...prev,
      {
        id,
        title: "本地终端",
        sessionId: null,
        status: "connected",
        local: true,
      },
    ]);
    setActiveTab(id);
    api.localSpawn(id, 120, 32).catch((e) => showToast(`本地终端启动失败：${String(e)}`));
  }, [showToast]);

  // —— Xshell .xsh 导入 / 导出 ——
  const importXsh = useCallback(async () => {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const picked = await open({
      multiple: true,
      title: "选择 Xshell 会话文件",
      filters: [{ name: "Xshell 会话", extensions: ["xsh"] }],
    });
    if (!picked) return;
    const paths = Array.isArray(picked) ? picked : [picked];
    try {
      const results = await api.importXsh(paths);
      const ok = results.filter((r) => r.ok);
      const bad = results.filter((r) => !r.ok);
      await refresh();
      const parts: string[] = [];
      parts.push(`成功导入 ${ok.length} 个会话（导入后请补填密码）`);
      if (bad.length > 0) {
        parts.push(`失败 ${bad.length} 个：${bad.map((b) => `${b.label}（${b.error}）`).join("；")}`);
      }
      showToast(parts.join("。"));
    } catch (e) {
      showToast(`导入失败：${String(e)}`);
    }
  }, [refresh, showToast]);

  const exportSessionXsh = useCallback(async () => {
    const s = sessions.find((x) => x.id === selectedId);
    if (!s) return;
    const { save } = await import("@tauri-apps/plugin-dialog");
    const target = await save({
      title: "导出为 Xshell 会话",
      defaultPath: `${s.name}.xsh`,
      filters: [{ name: "Xshell 会话", extensions: ["xsh"] }],
    });
    if (!target) return;
    try {
      await api.exportXsh(s.id, target);
      showToast(`已导出「${s.name}」到 ${target}（不含密码）`);
    } catch (e) {
      showToast(`导出失败：${String(e)}`);
    }
  }, [sessions, selectedId, showToast]);

  const exportAllXsh = useCallback(async () => {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const dir = await open({ directory: true, title: "选择导出目录" });
    if (!dir || Array.isArray(dir)) return;
    try {
      const n = await api.exportAllXsh(dir);
      showToast(`已导出 ${n} 个会话到 ${dir}（不含密码）`);
    } catch (e) {
      showToast(`导出失败：${String(e)}`);
    }
  }, [showToast]);

  const toggleSftp = useCallback(() => {
    setFmSession((cur) => {
      if (cur) return null;
      const sid = tabs.find((t) => t.id === activeTab)?.sessionId;
      return sessions.find((x) => x.id === sid) ?? null;
    });
  }, [activeTab, sessions, tabs]);

  const activeTabState = tabs.find((t) => t.id === activeTab) ?? null;
  const activeSession = activeTabState?.sessionId
    ? sessions.find((s) => s.id === activeTabState.sessionId) ?? null
    : null;
  const selectedSession = sessions.find((s) => s.id === selectedId) ?? null;

  const appTitle = activeSession
    ? `${activeSession.name} — ${activeSession.username}@${activeSession.host}`
    : "NexusTerm";

  const statusBar: "idle" | "connecting" | "connected" | "error" =
    activeTabState?.status === "error"
      ? "error"
      : activeTabState?.status === "connecting"
        ? "connecting"
        : activeTabState?.status === "connected"
          ? "connected"
          : "idle";

  const peer = activeSession
    ? `${activeSession.username}@${activeSession.host}:${activeSession.port}`
    : null;

  return (
    <div className="app" data-theme={themeName}>
      <TitleBar
        title={appTitle}
        themeName={themeName}
        onCycleTheme={cycleTheme}
        onToggleSettings={() => setSettingsOpen((v) => !v)}
        settingsOpen={settingsOpen}
      />

      <Toolbar
        canConnect={!!selectedSession}
        canDisconnect={!!activeTabState}
        canSftp={activeTabState?.status === "connected"}
        sftpOpen={!!fmSession}
        settingsOpen={settingsOpen}
        onCreate={() => {
          setEditing(null);
          setDialogOpen(true);
        }}
        onConnect={() => selectedSession && connect(selectedSession)}
        onDisconnect={() => activeTabState && closeTab(activeTabState.id)}
        onToggleSftp={toggleSftp}
        onToggleSettings={() => setSettingsOpen((v) => !v)}
        sidebarVisible={sidebarVisible}
        onToggleSidebar={() => setSidebarVisible((v) => !v)}
        canExportSession={!!selectedId}
        onImport={importXsh}
        onExportSession={exportSessionXsh}
        onExportAll={exportAllXsh}
      />

      <TabBar
        tabs={tabs}
        activeTab={activeTab}
        onSelect={setActiveTab}
        onClose={closeTab}
      />

      <div className="workspace">
        {sidebarVisible && (
          <SessionList
            sessions={sessions}
            selectedId={selectedId}
            onSelect={(s) => setSelectedId(s.id)}
            onConnect={connect}
            onCreate={() => {
              setEditing(null);
              setDialogOpen(true);
            }}
            onEdit={(s) => {
              setEditing(s);
              setDialogOpen(true);
            }}
            onDelete={removeSession}
            onSessionContextMenu={openCtxMenu}
          />
        )}

        <main className="main">
          <div className="term-area">
            {tabs.map((t) => (
              <TerminalView
                key={t.id}
                tabId={t.id}
                active={t.id === activeTab}
                kind={t.local ? "local" : "ssh"}
                onNotify={showToast}
                theme={theme}
                fontFamily={fontFamily}
                fontSize={fontSize}
                lineHeight={lineHeight}
                onClosed={() => {
                  setTabs((prev) =>
                    prev.map((x) =>
                      x.id === t.id ? { ...x, status: "closed" } : x
                    )
                  );
                }}
              />
            ))}

            {fmSession && (
              <FileManager
                key={fmSession.id}
                session={fmSession}
                passphrase={passphraseRef.current.get(fmSession.id)}
                onClose={() => setFmSession(null)}
              />
            )}

            {tabs.length === 0 && !fmSession && (
              <div className="welcome">
                <h1>NexusTerm</h1>
                <p>跨平台 SSH / SFTP 客户端 · Rust + TypeScript</p>
                <ul>
                  <li>双击左侧会话即可连接</li>
                  <li>工具栏「SFTP」在终端与文件管理器之间切换</li>
                  <li>点右上角 ⚙ 或工具栏「外观」调整配色与字体</li>
                  <li>
                    {isMacOS()
                      ? "⌘C 复制 · Ctrl+C 发送中断信号 · ⌘B 收起侧栏"
                      : `Ctrl+Shift+C 复制 · Ctrl+Shift+V 粘贴 · Ctrl+C 发送中断信号 · ${modKeyLabel()}+B 收起侧栏`}
                  </li>
                </ul>
              </div>
            )}

            {tabs
              .filter((t) => t.status === "error" && t.id === activeTab)
              .map((t) => (
                <div key={`err-${t.id}`} className="term-overlay">
                  <TerminalError
                    raw={t.error}
                    onClose={() => closeTab(t.id)}
                  />
                </div>
              ))}
          </div>
        </main>
      </div>

      <StatusBar status={statusBar} peer={peer} sessionCount={sessions.length} />

      {settingsOpen && (
        <div className="settings-panel">
          <h3>外观</h3>
          <label>
            配色
            <select value={themeName} onChange={(e) => setThemeName(e.target.value)}>
              {THEME_NAMES.map((n) => (
                <option key={n} value={n}>
                  {n}
                </option>
              ))}
            </select>
          </label>
          <label>
            字体
            <select
              value={fontFamily}
              onChange={(e) => setFontFamily(e.target.value)}
            >
              {FONT_FAMILIES.map((f) => (
                <option key={f.value} value={f.value}>
                  {f.label}
                </option>
              ))}
            </select>
          </label>
          <label>
            字号 {fontSize}px
            <input
              type="range"
              min={10}
              max={22}
              value={fontSize}
              onChange={(e) => setFontSize(Number(e.target.value))}
            />
          </label>
          <label>
            行高 {lineHeight.toFixed(2)}
            <input
              type="range"
              min={1}
              max={2}
              step={0.05}
              value={lineHeight}
              onChange={(e) => setLineHeight(Number(e.target.value))}
            />
          </label>
          {/* 字体实时预览：调滑块时立刻看到连接后的实际效果 */}
          <div
            className="welcome-preview"
            style={{
              fontFamily,
              fontSize,
              lineHeight: Math.max(1, lineHeight),
            }}
          >
            <span className="wp-dim">chris@server:~$ </span>
            <span className="wp-cmd">ssh prod-server</span>
            {"\n"}
            <span className="wp-dim">
              {fontSize}px / 行高 {lineHeight.toFixed(2)} · 中文与 emoji 宽度已启用
            </span>
          </div>
        </div>
      )}

      {dialogOpen && (
        <SessionDialog
          editing={editing}
          onSave={saveSession}
          onClose={() => {
            setDialogOpen(false);
            setEditing(null);
          }}
        />
      )}

      {toast && <div className="toast">{toast}</div>}

      {ctxMenu && (
        <>
          {/* 全屏透明层：点 anywhere 收菜单，再点一次右键也收 */}
          <div
            className="ctx-backdrop"
            onClick={closeCtxMenu}
            onContextMenu={(e) => {
              e.preventDefault();
              closeCtxMenu();
            }}
          />
          <div className="ctx-menu" style={{ left: ctxMenu.x, top: ctxMenu.y }}>
            <button
              className="ctx-item"
              onClick={() => {
                closeCtxMenu();
                setEditing(null);
                setDialogOpen(true);
              }}
            >
              新建会话
            </button>
            <button
              className="ctx-item"
              onClick={() => {
                closeCtxMenu();
                duplicateSession(ctxMenu.session);
              }}
            >
              复制会话
            </button>
            <button
              className="ctx-item"
              disabled={
                !tabsRef.current.some((t) => t.sessionId === ctxMenu.session.id)
              }
              title={
                tabsRef.current.some((t) => t.sessionId === ctxMenu.session.id)
                  ? undefined
                  : "该会话没有已打开的标签页"
              }
              onClick={() => {
                closeCtxMenu();
                closeSessionTabs(ctxMenu.session);
              }}
            >
              关闭会话
            </button>
          </div>
        </>
      )}
    </div>
  );
}

/** 连接失败弹层：大字给人类看，原始错误留作小字便于排障。 */
function TerminalError({ raw, onClose }: { raw?: string; onClose: () => void }) {
  const { message, detail } = toFriendlyError(raw);
  return (
    <div className="term-error">
      <strong>连接失败</strong>
      <p className="term-error-msg">{message}</p>
      {detail && <pre className="term-error-detail">{detail}</pre>}
      <button className="btn" onClick={onClose}>
        关闭标签
      </button>
    </div>
  );
}
