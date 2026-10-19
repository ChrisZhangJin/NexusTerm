import { useCallback, useEffect, useRef, useState } from "react";
import SessionList from "./components/SessionList";
import SessionDialog from "./components/SessionDialog";
import TerminalView, { clock } from "./components/TerminalView";
import FileManager from "./components/FileManager";
import TitleBar from "./components/TitleBar";
import Toolbar from "./components/Toolbar";
import TabBar from "./components/TabBar";
import StatusBar from "./components/StatusBar";
import { api, stringToBytes } from "./api";
import {
  parseProbe,
  restoreCommand,
  restoreNotice,
  stealthInput,
  type TermContext,
} from "./lib/context";
import { cellWidth, getTerm, waitForPrompt } from "./lib/termRegistry";
import ConfirmDialog, { type ConfirmOptions } from "./components/ConfirmDialog";
import { resolveAuth } from "./lib/auth";
import { normalizeTimeout, toFriendlyError } from "./lib/errors";
import { BUILTIN_THEMES, DEFAULT_THEME, FONT_FAMILIES } from "./lib/theme";
import { applyChromeVars } from "./lib/chrome";
import { isMacOS } from "./lib/platform";
import type {
  AuthMethod,
  ClosedInfo,
  NewSession,
  SessionRecord,
  TerminalTab,
} from "./types";

let tabSeq = 0;
const nextId = () => `tab-${Date.now()}-${++tabSeq}`;

/** 断线自动重连的等待间隔（ms），数组长度即最大尝试次数 */
const RECONNECT_DELAYS = [2000, 4000, 8000, 15000, 30000];

const THEME_NAMES = Object.keys(BUILTIN_THEMES);

export default function App() {
  const [sessions, setSessions] = useState<SessionRecord[]>([]);
  const [tabs, setTabs] = useState<TerminalTab[]>([]);
  const [activeTab, setActiveTab] = useState<string | null>(null);
  const [dialogOpen, setDialogOpen] = useState(false);
  const [editing, setEditing] = useState<SessionRecord | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  // 非空时展示该会话的 SFTP 文件管理器
  const [fmSession, setFmSession] = useState<SessionRecord | null>(null);

  const [themeName, setThemeName] = useState(DEFAULT_THEME);
  const [fontFamily, setFontFamily] = useState(FONT_FAMILIES[0].value);
  const [fontSize, setFontSize] = useState(13);
  const [lineHeight, setLineHeight] = useState(1.2);

  // 弹出式会话列表（Xshell「打开」按钮风格）
  const [sessionPopupOpen, setSessionPopupOpen] = useState(false);

  const passphraseRef = useRef<Map<string, string>>(new Map());

  // —— 轻量提示条（导入/导出/复制会话等结果） ——
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

  // 异步流程（重连循环、连接回调）里要读最新值，用 ref 桥接
  const tabsRef = useRef(tabs);
  tabsRef.current = tabs;
  const sessionsRef = useRef(sessions);
  sessionsRef.current = sessions;
  /** 每个标签当前重连循环的取消开关 */
  const reconnectCtl = useRef(new Map<string, { cancelled: boolean }>());

  const patchTab = useCallback((id: string, patch: Partial<TerminalTab>) => {
    setTabs((prev) => prev.map((t) => (t.id === id ? { ...t, ...patch } : t)));
  }, []);

  /** 新标签插到 afterId 右侧；没给就追加到末尾 */
  const insertTab = useCallback((tab: TerminalTab, afterId?: string | null) => {
    setTabs((prev) => {
      const i = afterId ? prev.findIndex((t) => t.id === afterId) : -1;
      if (i < 0) return [...prev, tab];
      return [...prev.slice(0, i + 1), tab, ...prev.slice(i + 1)];
    });
    setActiveTab(tab.id);
  }, []);

  /** 对指定标签发起一次 SSH 拨号（新建与重连共用） */
  const dial = useCallback(async (tabId: string, s: SessionRecord) => {
    const auth: AuthMethod = await resolveAuth(s, passphraseRef.current.get(s.id));
    // 用终端当前的真实尺寸申请 PTY。原来写死 120×32：TerminalView 挂载时的 resize
    // 早于会话建立（后端还没这个 id，静默失败），之后尺寸不变也就不再推送，
    // 远端就一直停在 120×32 —— 复制会话 / tmux 只占左上角一块的根因。
    const term = getTerm(tabId);
    await api.sshConnect(tabId, {
      host: s.host,
      port: s.port,
      username: s.username,
      auth,
      cols: term?.cols || 120,
      rows: term?.rows || 32,
      connectTimeoutSecs: normalizeTimeout(s.connectTimeout),
    });
  }, []);

  /** 探测标签的远端上下文并写回 tab.ctx；失败或无结果返回已缓存的值 */
  const probeTab = useCallback(
    async (tabId: string) => {
      try {
        const c = parseProbe(await api.sshProbe(tabId));
        if (c) patchTab(tabId, { ctx: c });
        return c ?? tabsRef.current.find((t) => t.id === tabId)?.ctx;
      } catch {
        return tabsRef.current.find((t) => t.id === tabId)?.ctx;
      }
    },
    [patchTab]
  );

  /**
   * 新连接就绪后把上下文还原进去（cd / tmux）。
   * - 等 shell 真正停在提示符上再注入（输出静默 + 行尾是提示符），不再靠固定延时
   * - 注入命令自带「抹掉自身回显」的前缀，终端里看不到命令；改由右下角说明卡片告知做了什么
   */
  const applyContext = useCallback(
    async (
      tabId: string,
      c: TermContext | undefined,
      mode: "reconnect" | "duplicate",
      since: number
    ) => {
      if (!c) return;
      const rows = restoreNotice(c, mode);
      if (rows.length === 0) return;
      const body = restoreCommand(c, mode);
      let ok = true;
      if (body) {
        const ready = await waitForPrompt(tabId, since);
        const term = getTerm(tabId);
        const tab = tabsRef.current.find((t) => t.id === tabId);
        if (!ready || !term || tab?.status !== "connected") {
          ok = false;
        } else {
          const buf = term.buffer.active;
          const input = stealthInput(body, buf.cursorX, term.cols, cellWidth);
          await api.sshWrite(tabId, stringToBytes(input)).catch(() => {
            ok = false;
          });
        }
      }
      patchTab(tabId, { notice: { mode, rows, ok, at: Date.now() } });
    },
    [patchTab]
  );

  // 已连接的 SSH 标签定期探测上下文：断线后就靠这份缓存恢复
  useEffect(() => {
    const tick = () => {
      for (const t of tabsRef.current) {
        if (!t.local && t.status === "connected") probeTab(t.id);
      }
    };
    const first = window.setTimeout(tick, 3000);
    const id = window.setInterval(tick, 15000);
    return () => {
      window.clearTimeout(first);
      window.clearInterval(id);
    };
  }, [probeTab]);

  /** 建立连接并开启标签页；restore 给了则连上后还原该上下文 */
  const connect = useCallback(
    async (
      s: SessionRecord,
      afterId?: string | null,
      restore?: TermContext,
      title?: string
    ) => {
      const tabId = nextId();
      insertTab(
        { id: tabId, title: s.name, customTitle: title, sessionId: s.id, status: "connecting" },
        afterId
      );
      try {
        const since = Date.now();
        await dial(tabId, s);
        // 连接过程中标签被关掉了：把刚建好的会话收掉，避免孤儿连接
        if (!tabsRef.current.some((t) => t.id === tabId)) {
          api.sshClose(tabId).catch(() => {});
          return;
        }
        patchTab(tabId, { status: "connected", ctx: restore });
        applyContext(tabId, restore, "duplicate", since);
      } catch (e) {
        patchTab(tabId, { status: "error", error: String(e) });
      }
    },
    [dial, insertTab, patchTab, applyContext]
  );

  /**
   * 重连。auto=true 为断线自动重连（按退避间隔最多 RECONNECT_DELAYS.length 次），
   * auto=false 为用户手动「重新连接」（立即尝试 1 次）。
   */
  const reconnect = useCallback(
    async (tabId: string, auto: boolean) => {
      const tab = tabsRef.current.find((t) => t.id === tabId);
      if (!tab || tab.local || !tab.sessionId) return;
      const s = sessionsRef.current.find((x) => x.id === tab.sessionId);
      if (!s) {
        patchTab(tabId, { status: "closed", error: "该连接配置已被删除，无法重连" });
        return;
      }
      // 同一标签只允许一个重连循环
      const old = reconnectCtl.current.get(tabId);
      if (old) old.cancelled = true;
      const ctl = { cancelled: false };
      reconnectCtl.current.set(tabId, ctl);
      // 清掉后端残留的旧会话条目
      await api.sshClose(tabId).catch(() => {});

      const max = auto ? RECONNECT_DELAYS.length : 1;
      let lastErr = "";
      for (let i = 1; i <= max; i++) {
        const delay = auto ? RECONNECT_DELAYS[i - 1] : 0;
        patchTab(tabId, {
          status: "reconnecting",
          reconnectAttempt: i,
          reconnectMax: max,
          reconnectAt: Date.now() + delay,
          error: undefined,
        });
        if (delay) await new Promise((r) => window.setTimeout(r, delay));
        if (ctl.cancelled) return;
        try {
          const since = Date.now();
          await dial(tabId, s);
          if (ctl.cancelled || !tabsRef.current.some((t) => t.id === tabId)) {
            api.sshClose(tabId).catch(() => {});
            return;
          }
          reconnectCtl.current.delete(tabId);
          patchTab(tabId, {
            status: "connected",
            reconnectAttempt: undefined,
            reconnectMax: undefined,
            reconnectAt: undefined,
            closedAt: undefined,
          });
          // 用断线前最后一次探测到的上下文恢复（tmux 直接 attach 回原会话）
          applyContext(tabId, tab.ctx, "reconnect", since);
          return;
        } catch (e) {
          lastErr = toFriendlyError(String(e)).message;
          if (ctl.cancelled) return;
        }
      }
      reconnectCtl.current.delete(tabId);
      patchTab(tabId, {
        status: "closed",
        error: lastErr,
        reconnectAttempt: undefined,
        reconnectAt: undefined,
      });
    },
    [dial, patchTab, applyContext]
  );

  const cancelReconnect = useCallback(
    (tabId: string) => {
      const ctl = reconnectCtl.current.get(tabId);
      if (ctl) ctl.cancelled = true;
      reconnectCtl.current.delete(tabId);
      patchTab(tabId, {
        status: "closed",
        reconnectAttempt: undefined,
        reconnectAt: undefined,
      });
    },
    [patchTab]
  );

  /** 后端报告会话结束 */
  const handleClosed = useCallback(
    (tabId: string, info: ClosedInfo) => {
      if (info.reason === "user") return; // 本地主动关闭，标签已移除
      patchTab(tabId, { status: "closed", closedAt: Date.now() });
      // 只有「连接丢失」才自动重连；远端 exit / 主动关闭属于正常结束
      if (info.reason === "lost") reconnect(tabId, true);
    },
    [patchTab, reconnect]
  );

  // —— 应用内确认框（window.confirm 在 WebView2 下不可靠） ——
  const [confirmState, setConfirmState] = useState<{
    opts: ConfirmOptions;
    resolve: (r: { ok: boolean; dontAsk: boolean }) => void;
  } | null>(null);
  const askConfirm = useCallback(
    (opts: ConfirmOptions) =>
      new Promise<{ ok: boolean; dontAsk: boolean }>((resolve) =>
        setConfirmState({ opts, resolve })
      ),
    []
  );

  const closeTabNow = useCallback((tabId: string) => {
    const ctl = reconnectCtl.current.get(tabId);
    if (ctl) ctl.cancelled = true;
    reconnectCtl.current.delete(tabId);
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

  /** 关闭标签：会话还活着时先确认（可勾选「不再询问」，存 localStorage） */
  const closeTab = useCallback(
    async (tabId: string) => {
      const t = tabsRef.current.find((x) => x.id === tabId);
      if (!t) return;
      const live =
        t.status === "connected" || t.status === "connecting" || t.status === "reconnecting";
      if (live && localStorage.getItem("nx.confirmCloseTab") !== "off") {
        const name = t.customTitle || t.title;
        const { ok, dontAsk } = await askConfirm({
          title: "关闭标签页",
          message: t.local
            ? `关闭「${name}」？本地 shell 及其中运行的程序会被结束。`
            : `关闭「${name}」？SSH 连接会断开，未在 tmux 中运行的前台程序会被终止。`,
          okText: "关闭",
          danger: true,
          dontAskLabel: "不再询问",
        });
        if (!ok) return;
        if (dontAsk) localStorage.setItem("nx.confirmCloseTab", "off");
      }
      closeTabNow(tabId);
    },
    [askConfirm, closeTabNow]
  );

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
      const { ok } = await askConfirm({
        title: "删除连接",
        message: `删除连接「${s.name}」？密码也会一并删除。`,
        okText: "删除",
        danger: true,
      });
      if (!ok) return;
      await api.deleteSession(s.id);
      await refresh();
    },
    [refresh, askConfirm]
  );

  // 拖拽后持久化排序与分组归属
  const reorderSessions = useCallback(
    async (items: [string, number, string][]) => {
      try {
        await api.reorderSessions(items);
        await refresh();
      } catch (e) {
        showToast(`调整失败：${String(e)}`);
      }
    },
    [refresh, showToast]
  );

  // 导出单个连接为 .xsh（右键菜单用）
  const exportOneXsh = useCallback(
    async (s: SessionRecord) => {
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
    },
    [showToast]
  );

  // 导出整个分组下的连接到目录
  const exportFolderXsh = useCallback(
    async (list: SessionRecord[], folderName: string) => {
      if (list.length === 0) return;
      const { open } = await import("@tauri-apps/plugin-dialog");
      const dir = await open({ directory: true, title: `选择「${folderName}」导出目录` });
      if (!dir || Array.isArray(dir)) return;
      try {
        const n = await api.exportSessionsXsh(list.map((s) => s.id), dir);
        showToast(`已导出分组「${folderName}」的 ${n} 个连接到 ${dir}（不含密码）`);
      } catch (e) {
        showToast(`导出失败：${String(e)}`);
      }
    },
    [showToast]
  );

  // 分组改名：把 oldPath 这一层的末段名改成 newName，连同其子分组一起平移
  const renameFolder = useCallback(
    async (oldPath: string, newName: string) => {
      const parent = oldPath.includes("/")
        ? oldPath.slice(0, oldPath.lastIndexOf("/"))
        : "";
      const newPath = parent ? `${parent}/${newName}` : newName;
      const norm = (f: string) => f.replace(/^\/+|\/+$/g, "");
      // 命中 oldPath 本身或其子路径的连接，整体换前缀
      const items: [string, number, string][] = [];
      for (const s of sessions) {
        const f = norm(s.folder);
        if (f === oldPath || f.startsWith(`${oldPath}/`)) {
          const rest = f.slice(oldPath.length); // "" 或 "/子路径"
          const nf = `${newPath}${rest}`;
          items.push([s.id, s.sortOrder, nf]);
        }
      }
      if (items.length === 0) return;
      try {
        await api.reorderSessions(items);
        await refresh();
        showToast(`分组已改名为「${newName}」`);
      } catch (e) {
        showToast(`改名失败：${String(e)}`);
      }
    },
    [sessions, refresh, showToast]
  );

  const cycleTheme = useCallback(() => {
    setThemeName((cur) => {
      const i = THEME_NAMES.indexOf(cur);
      return THEME_NAMES[(i + 1) % THEME_NAMES.length] ?? DEFAULT_THEME;
    });
  }, []);

  /** 新开一个本地 shell 标签 */
  const newLocalTab = useCallback(
    (afterId?: string | null) => {
      const id = nextId();
      // 先开标签再 spawn：TerminalView 挂载后会 fit 并纠正这里的初始尺寸
      insertTab(
        { id, title: "本地终端", sessionId: null, status: "connected", local: true },
        afterId
      );
      api.localSpawn(id, 120, 32).catch((e) => {
        patchTab(id, { status: "error", error: String(e) });
        showToast(`本地终端启动失败：${String(e)}`);
      });
    },
    [insertTab, patchTab, showToast]
  );

  // —— 启动时开一个本地 shell：没有 SSH 连接时也能当终端用 ——
  const localSpawned = useRef(false);
  useEffect(() => {
    if (localSpawned.current) return;
    localSpawned.current = true;
    newLocalTab();
  }, [newLocalTab]);

  /** 复制会话：在右侧再开一个同目标的标签 */
  const duplicateTab = useCallback(
    async (tabId: string) => {
      const t = tabsRef.current.find((x) => x.id === tabId);
      if (!t) return;
      if (t.local) return newLocalTab(tabId);
      const s = sessionsRef.current.find((x) => x.id === t.sessionId);
      if (!s) return showToast("该连接配置已被删除，无法复制");
      // 在线就现探一次拿最新目录；已断开则用缓存
      const c = t.status === "connected" ? await probeTab(tabId) : t.ctx;
      connect(s, tabId, c, t.customTitle);
    },
    [connect, newLocalTab, showToast, probeTab]
  );

  const renameTab = useCallback(
    (tabId: string, name: string) => {
      patchTab(tabId, { customTitle: name.trim() || undefined });
    },
    [patchTab]
  );

  /** 拖拽换位：把 fromId 移到 toId 的前/后 */
  const moveTab = useCallback((fromId: string, toId: string, after: boolean) => {
    if (fromId === toId) return;
    setTabs((prev) => {
      const from = prev.find((t) => t.id === fromId);
      if (!from) return prev;
      const rest = prev.filter((t) => t.id !== fromId);
      const j = rest.findIndex((t) => t.id === toId);
      if (j < 0) return prev;
      const at = after ? j + 1 : j;
      return [...rest.slice(0, at), from, ...rest.slice(at)];
    });
  }, []);

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
      const pwOk = results.filter((r) => r.passwordImported);
      await refresh();
      const parts: string[] = [];
      if (pwOk.length > 0) {
        parts.push(`成功导入 ${ok.length} 个会话（${pwOk.length} 个已自动导入密码）`);
      } else {
        parts.push(`成功导入 ${ok.length} 个会话`);
      }
      if (bad.length > 0) {
        parts.push(`失败 ${bad.length} 个：${bad.map((b) => `${b.label}（${b.error}）`).join("；")}`);
      }
      showToast(parts.join("。"));
    } catch (e) {
      showToast(`导入失败：${String(e)}`);
    }
  }, [refresh, showToast]);

  const exportSessionXsh = useCallback(async () => {
    // 导出当前激活标签对应的会话
    const sid = tabs.find((t) => t.id === activeTab)?.sessionId;
    const s = sessions.find((x) => x.id === sid);
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
  }, [sessions, activeTab, tabs, showToast]);

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

  const appTitle = activeSession
    ? `${activeSession.name} — ${activeSession.username}@${activeSession.host}`
    : "NexusTerm";

  const statusBar: "idle" | "connecting" | "connected" | "reconnecting" | "error" =
    activeTabState?.status === "error"
      ? "error"
      : activeTabState?.status === "connecting"
        ? "connecting"
        : activeTabState?.status === "reconnecting"
          ? "reconnecting"
          : activeTabState?.status === "connected"
            ? "connected"
            : "idle";

  const peer = activeSession
    ? `${activeSession.username}@${activeSession.host}:${activeSession.port}`
    : null;

  // 当前激活标签是否有对应会话（用于导出按钮状态）
  const canExportActive = !!activeSession;

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
        canConnect={false}
        canDisconnect={!!activeTabState}
        canSftp={activeTabState?.status === "connected"}
        sftpOpen={!!fmSession}
        settingsOpen={settingsOpen}
        sessionPopupOpen={sessionPopupOpen}
        onCreate={() => {
          setEditing(null);
          setDialogOpen(true);
        }}
        onConnect={() => {}}
        onDisconnect={() => activeTabState && closeTab(activeTabState.id)}
        onToggleSftp={toggleSftp}
        onToggleSettings={() => setSettingsOpen((v) => !v)}
        onToggleSessionPopup={() => setSessionPopupOpen((v) => !v)}
        canExportSession={canExportActive}
        onImport={importXsh}
        onExportSession={exportSessionXsh}
        onExportAll={exportAllXsh}
      />

      <TabBar
        tabs={tabs}
        activeTab={activeTab}
        onSelect={setActiveTab}
        onClose={closeTab}
        onNewTab={newLocalTab}
        onReconnect={(id) => reconnect(id, false)}
        onDuplicate={duplicateTab}
        onRename={renameTab}
        onMove={moveTab}
      />

      <div className="workspace">
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
                onDisconnect={() => closeTab(t.id)}
                peerHost={sessions.find((x) => x.id === t.sessionId)?.host}
                status={t.status}
                reconnectAttempt={t.reconnectAttempt}
                reconnectMax={t.reconnectMax}
                onClosed={(info) =>
                  t.local ? patchTab(t.id, { status: "closed" }) : handleClosed(t.id, info)
                }
              />
            ))}

            {/* 重连进度：右上角浮层，不遮挡终端内容 */}
            {activeTabState?.status === "reconnecting" && (
              <ReconnectBadge
                key={`rc-${activeTabState.id}-${activeTabState.reconnectAttempt}`}
                host={activeSession?.host ?? ""}
                attempt={activeTabState.reconnectAttempt ?? 1}
                max={activeTabState.reconnectMax ?? 1}
                at={activeTabState.reconnectAt ?? Date.now()}
                onCancel={() => cancelReconnect(activeTabState.id)}
              />
            )}
            {activeTabState?.status === "closed" && !activeTabState.local && (
              <div className="reconnect-badge idle">
                <span>
                  已断开{activeTabState.closedAt ? ` · ${clock(activeTabState.closedAt)}` : ""}
                  {activeTabState.error ? ` · ${activeTabState.error}` : ""}
                </span>
                <button className="btn-link" onClick={() => reconnect(activeTabState.id, false)}>
                  重新连接
                </button>
              </div>
            )}

            {/* 上下文恢复说明：取代在终端里回显 cd/tmux 命令 */}
            {activeTabState?.notice && activeTabState.status === "connected" && (
              <RestoreCard
                key={`rn-${activeTabState.id}-${activeTabState.notice.at}`}
                notice={activeTabState.notice}
                onClose={() => patchTab(activeTabState.id, { notice: undefined })}
              />
            )}

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
                  <li>点击工具栏「连接」打开连接列表，双击即可连接</li>
                  <li>工具栏「SFTP」在终端与文件管理器之间切换</li>
                  <li>点右上角 ⚙ 或工具栏「外观」调整配色与字体</li>
                  <li>
                    {isMacOS()
                      ? "⌘C 复制 · Ctrl+C 发送中断信号"
                      : `Ctrl+Shift+C 复制 · Ctrl+Shift+V 粘贴 · Ctrl+C 发送中断信号`}
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

      {/* 右下角计数 = 当前打开的标签数（与 Xshell「N 会话」一致），不是已保存的连接数 */}
      <StatusBar
        status={statusBar}
        peer={peer}
        sessionCount={tabs.length}
        connectedCount={tabs.filter((t) => t.status === "connected").length}
      />

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

      {confirmState && (
        <ConfirmDialog
          opts={confirmState.opts}
          onResult={(ok, dontAsk) => {
            confirmState.resolve({ ok, dontAsk });
            setConfirmState(null);
          }}
        />
      )}

      {/* 弹出式会话列表 —— 从工具栏「☰ 会话」按钮下方弹出 */}
      {sessionPopupOpen && (
        <SessionList
          sessions={sessions}
          onConnect={connect}
          onCreate={() => {
            setSessionPopupOpen(false);
            setEditing(null);
            setDialogOpen(true);
          }}
          onEdit={(s) => {
            setSessionPopupOpen(false);
            setEditing(s);
            setDialogOpen(true);
          }}
          onDelete={removeSession}
          onClose={() => setSessionPopupOpen(false)}
          onReorder={reorderSessions}
          onExport={exportOneXsh}
          onExportFolder={exportFolderXsh}
          onRenameFolder={renameFolder}
        />
      )}
    </div>
  );
}

/** 自动重连浮层：转圈 + 第几次 + 倒计时 */
function ReconnectBadge({
  host,
  attempt,
  max,
  at,
  onCancel,
}: {
  host: string;
  attempt: number;
  max: number;
  at: number;
  onCancel: () => void;
}) {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const id = window.setInterval(() => setNow(Date.now()), 250);
    return () => window.clearInterval(id);
  }, []);
  const left = Math.max(0, Math.ceil((at - now) / 1000));
  return (
    <div className="reconnect-badge">
      <span className="spinner" />
      <span>
        {left > 0 ? `${left} 秒后` : "正在"}重连 {host}（第 {attempt}/{max} 次）
      </span>
      <button className="btn-link" onClick={onCancel}>
        取消
      </button>
    </div>
  );
}

/** 上下文恢复说明卡片：右上角浮出，8 秒后自动收起（悬停时暂停） */
function RestoreCard({
  notice,
  onClose,
}: {
  notice: NonNullable<TerminalTab["notice"]>;
  onClose: () => void;
}) {
  const [hover, setHover] = useState(false);
  // 父组件每次渲染都会给新的 onClose；用 ref 免得计时器被反复重置
  const closeRef = useRef(onClose);
  closeRef.current = onClose;
  useEffect(() => {
    if (hover) return;
    const id = window.setTimeout(() => closeRef.current(), 8000);
    return () => window.clearTimeout(id);
  }, [hover]);
  const title = !notice.ok
    ? "未能自动恢复上下文"
    : notice.mode === "reconnect"
      ? "已恢复断线前的工作环境"
      : "已同步原会话的工作环境";
  return (
    <div
      className={`restore-card${notice.ok ? "" : " warn"}`}
      onMouseEnter={() => setHover(true)}
      onMouseLeave={() => setHover(false)}
    >
      <div className="restore-head">
        <span className="restore-icon">{notice.ok ? "↻" : "!"}</span>
        <span className="restore-title">{title}</span>
        <button className="restore-close" onClick={onClose} title="关闭">
          ×
        </button>
      </div>
      <dl className="restore-rows">
        {notice.rows.map(([k, v]) => (
          <div key={k} className="restore-row">
            <dt>{k}</dt>
            <dd>{v}</dd>
          </div>
        ))}
      </dl>
      {!notice.ok && (
        <p className="restore-foot">远端提示符迟迟未出现，为避免误输入已跳过，可手动执行。</p>
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
