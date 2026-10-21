import { useCallback, useEffect, useRef, useState } from "react";
import SessionList from "./components/SessionList";
import SessionDialog from "./components/SessionDialog";
import TerminalView, { clock } from "./components/TerminalView";
import FileManager from "./components/FileManager";
import TitleBar from "./components/TitleBar";
import TabBar from "./components/TabBar";
import StatusBar from "./components/StatusBar";
import { AgentPanel } from "./components/AgentPanel";
import {
  AppearanceDialog,
  AgentDialog,
  ShortcutsDialog,
} from "./components/SettingsDialogs";
import { api, stringToBytes } from "./api";
import { loadAgentConfig } from "./lib/agent";
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
import { formatCombo, getPrefs, setPrefs, useShortcuts } from "./lib/prefs";
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
/** 断线自动重连：最多 10 次，间隔从 2 秒起每次翻倍（2s、4s、8s … 1024s，累计约 34 分钟） */
const RECONNECT_MAX = 10;
const RECONNECT_BASE = 2000;
const reconnectDelay = (attempt: number) => RECONNECT_BASE * 2 ** (attempt - 1);

const THEME_NAMES = Object.keys(BUILTIN_THEMES);

export default function App() {
  const [sessions, setSessions] = useState<SessionRecord[]>([]);
  const [tabs, setTabs] = useState<TerminalTab[]>([]);
  const [activeTab, setActiveTab] = useState<string | null>(null);
  const [dialogOpen, setDialogOpen] = useState(false);
  const [editing, setEditing] = useState<SessionRecord | null>(null);
  /** 设置窗口：外观 / 快捷键 / AI 助手（模态，同一时刻只开一个） */
  const [settingsWin, setSettingsWin] = useState<
    "appearance" | "shortcuts" | "agent" | null
  >(null);
  const settingsOpen = settingsWin !== null;
  // 非空时展示该会话的 SFTP 文件管理器
  const [fmSession, setFmSession] = useState<SessionRecord | null>(null);
  /**
   * AI 助手侧栏是否展开。
   * 与「面板是否挂载」是两件事：面板始终挂载、只切 visible，
   * 否则一轮对话的历史会随收起一起丢掉。
   */
  const [agentOpen, setAgentOpen] = useState(false);

  // 外观持久化到 localStorage（之前每次启动都回到默认）
  const [themeName, setThemeName] = useState(() => {
    const v = localStorage.getItem("nx.theme");
    return v && BUILTIN_THEMES[v] ? v : DEFAULT_THEME;
  });
  const [fontFamily, setFontFamily] = useState(
    () => localStorage.getItem("nx.fontFamily") || FONT_FAMILIES[0].value
  );
  const [fontSize, setFontSize] = useState(() => {
    const n = Number(localStorage.getItem("nx.fontSize"));
    return n >= 10 && n <= 22 ? n : 13;
  });
  const [lineHeight, setLineHeight] = useState(() => {
    const n = Number(localStorage.getItem("nx.lineHeight"));
    return n >= 1 && n <= 2 ? n : 1.2;
  });
  useEffect(() => {
    localStorage.setItem("nx.theme", themeName);
    localStorage.setItem("nx.fontFamily", fontFamily);
    localStorage.setItem("nx.fontSize", String(fontSize));
    localStorage.setItem("nx.lineHeight", String(lineHeight));
  }, [themeName, fontFamily, fontSize, lineHeight]);

  // 弹出式会话列表（Xshell「打开」按钮风格）
  const [sessionPopupOpen, setSessionPopupOpen] = useState(false);

  const passphraseRef = useRef<Map<string, string>>(new Map());
  const shortcuts = useShortcuts();

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

  // 文件拖到终端以外的地方松开时，WebView 默认会「打开」该文件把整个界面替换掉 —— 全局拦下。
  // 终端区自己处理 drop（rz 上传），这里只兜底默认行为。
  useEffect(() => {
    const stop = (e: DragEvent) => {
      if (Array.from(e.dataTransfer?.types ?? []).includes("Files")) e.preventDefault();
    };
    window.addEventListener("dragover", stop);
    window.addEventListener("drop", stop);
    return () => {
      window.removeEventListener("dragover", stop);
      window.removeEventListener("drop", stop);
    };
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

  /**
   * 新会话建立时，把「是否旁路」的默认值下发到后端。
   *
   * 后端对「没登记过的会话」本来就回落到全局 autoAttach，所以这条 IPC 严格说
   * 是冗余的；仍然发一次，是为了让这个会话的开关状态**在还没有任何输出时就
   * 是确定的** —— 面板打开即可看到它是开是关、也能在采集开始前就关掉它，
   * 而不是等第一字节输出时被 feed() 顺手建出来、想关也已经采了一截。
   *
   * 两条硬约束：
   * - 总开关关着时什么都不发。未启用 = 零开销，一条 IPC 也不该有。
   * - 只在**建会话时**发一次。之后用户在面板里的每次开关都由面板直接写后端
   *   （它是这条状态的唯一写入者），App 不另起一份镜像，避免两处状态打架。
   */
  const seedAgentAttach = useCallback((tabId: string) => {
    void loadAgentConfig().then((cfg) => {
      if (!cfg.enabled || !cfg.autoAttach) return;
      api.agentSetSession(tabId, true).catch(() => {});
    });
  }, []);

  /** 对指定标签发起一次 SSH 拨号（新建与重连共用） */
  const dial = useCallback(async (tabId: string, s: SessionRecord) => {
    const auth: AuthMethod = await resolveAuth(s, passphraseRef.current.get(s.id));
    // 用终端当前的真实尺寸申请 PTY。原来写死 120×32：TerminalView 挂载时的 resize
    // 早于会话建立（后端还没这个 id，静默失败），之后尺寸不变也就不再推送，
    // 远端就一直停在 120×32 —— 复制会话 / tmux 只占左上角一块的根因。
    const term = getTerm(tabId);
    const outcome = await api.sshConnect(tabId, {
      host: s.host,
      port: s.port,
      username: s.username,
      auth,
      cols: term?.cols || 120,
      rows: term?.rows || 32,
      connectTimeoutSecs: normalizeTimeout(s.connectTimeout),
    });
    // 老服务器（CentOS 6.x 的 OpenSSH 5.3 等）会退到 SHA-1 的 DH 组，安全强度偏低，
    // 不阻断使用但要明确告知，避免用户以为是正常连接。
    if (outcome?.legacyCrypto) {
      showToast(`${s.host} 使用了旧版密钥交换算法（兼容模式），建议升级服务器 OpenSSH`);
    }
  }, [showToast]);

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
      // 旁路开关的默认值只跟会话一起定一次，之后交给面板
      seedAgentAttach(tabId);
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
    [dial, insertTab, seedAgentAttach, patchTab, applyContext]
  );

  /**
   * 重连。auto=true 为断线自动重连（间隔逐次翻倍，最多 RECONNECT_MAX 次），
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

      const max = auto ? RECONNECT_MAX : 1;
      let lastErr = "";
      for (let i = 1; i <= max; i++) {
        const delay = auto ? reconnectDelay(i) : 0;
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

  /**
   * 断开会话但保留标签：终端内容留着可回看 / 导出，
   * 需要时右键「重新连接」，或手动关闭标签。
   */
  const disconnectTab = useCallback(
    (tabId: string) => {
      const t = tabsRef.current.find((x) => x.id === tabId);
      if (!t || t.local) return;
      const ctl = reconnectCtl.current.get(tabId);
      if (ctl) ctl.cancelled = true;
      reconnectCtl.current.delete(tabId);
      const wasLive = t.status === "connected" || t.status === "reconnecting" || t.status === "connecting";
      api.sshClose(tabId).catch(() => {});
      patchTab(tabId, {
        status: "closed",
        closedAt: Date.now(),
        error: undefined,
        notice: undefined,
        reconnectAttempt: undefined,
        reconnectAt: undefined,
      });
      if (wasLive) {
        // 后端对主动关闭（reason=user）不再回推提示，这里自己写一行留痕
        const host = sessionsRef.current.find((x) => x.id === t.sessionId)?.host ?? "remote host";
        getTerm(tabId)?.write(
          `\r\n\r\n\x1b[1;33mDisconnected from remote host(${host}) at ${clock(Date.now())}.\x1b[0m\r\n` +
            `\x1b[90m会话已断开，标签页保留。右键标签可「重新连接」，不需要时手动关闭。\x1b[0m\r\n`
        );
      }
    },
    [patchTab]
  );

  /** 关闭标签：会话还活着时先确认（可勾选「不再询问」，存 localStorage） */
  const closeTab = useCallback(
    async (tabId: string) => {
      const t = tabsRef.current.find((x) => x.id === tabId);
      if (!t) return;
      const live =
        t.status === "connected" || t.status === "connecting" || t.status === "reconnecting";
      if (live && getPrefs().confirmCloseTab) {
        const name = t.customTitle || t.title;
        const { ok, dontAsk } = await askConfirm({
          title: "关闭标签页",
          message: t.local
            ? `关闭「${name}」？本地 shell 及其中运行的程序会被结束。`
            : `关闭「${name}」？SSH 连接会断开，未在 tmux 中运行的前台程序会被终止。`,
          okText: "关闭",
          danger: true,
          dontAskLabel: "不再询问（可在 设置 → 外观 中恢复）",
        });
        if (!ok) return;
        // 可在「⚙ 设置 → 外观 → 终端行为」重新勾选恢复
        if (dontAsk) setPrefs({ confirmCloseTab: false });
      }
      closeTabNow(tabId);
    },
    [askConfirm, closeTabNow]
  );

  /**
   * 把文本写进某个会话的 pty —— AI 面板「插入到终端」的唯一落地路径。
   *
   * 调用链上必然已经过了两道人为确认：点建议卡片上的「插入到终端」→ 在确认框里
   * 点「插入」。除这条链路外，本函数没有任何调用方，也不接受任何自动触发。
   *
   * **刻意不追加 \n**：只把命令插进当前输入行，回车执行留给人在终端里自己按。
   * 替用户按下这个回车，等于替他执行一条可能没读完的命令 —— 那不是便利，是事故。
   * 后端会把这次写入当成 StreamKind::In 记进上下文，与人手敲进去的命令同等对待。
   */
  const sendToTerminal = useCallback(async (tabId: string, text: string) => {
    if (!text) return;
    const t = tabsRef.current.find((x) => x.id === tabId);
    if (!t) return;
    const bytes = stringToBytes(text);
    if (t.local) await api.localWrite(tabId, bytes);
    else await api.sshWrite(tabId, bytes);
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
      // 本地终端也走同一条旁路（后端 local 读线程同样会 feed）
      seedAgentAttach(id);
      api.localSpawn(id, 120, 32).catch((e) => {
        patchTab(id, { status: "error", error: String(e) });
        showToast(`本地终端启动失败：${String(e)}`);
      });
    },
    [insertTab, seedAgentAttach, patchTab, showToast]
  );

  // —— 启动时开一个本地 shell：没有 SSH 连接时也能当终端用 ——
  const localSpawned = useRef(false);
  useEffect(() => {
    if (localSpawned.current) return;
    localSpawned.current = true;
    newLocalTab();
  }, [newLocalTab]);

  /**
   * 复制会话：在右侧再开一个同目标的标签。
   * withState=false 只是对同一主机新开一个干净会话；
   * withState=true 额外恢复当前目录与 tmux（分组会话，窗口共享、视图独立）。
   */
  const duplicateTab = useCallback(
    async (tabId: string, withState: boolean) => {
      const t = tabsRef.current.find((x) => x.id === tabId);
      if (!t) return;
      if (t.local) return newLocalTab(tabId);
      const s = sessionsRef.current.find((x) => x.id === t.sessionId);
      if (!s) return showToast("该连接配置已被删除，无法复制");
      if (!withState) return connect(s, tabId);
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

  /** 打开某个标签所属连接的 SFTP（标签 / 终端右键菜单） */
  const openSftp = useCallback(
    (tabId: string) => {
      const sid = tabs.find((t) => t.id === tabId)?.sessionId;
      const s = sessions.find((x) => x.id === sid);
      if (!s) return;
      setActiveTab(tabId);
      setFmSession(s);
    },
    [sessions, tabs]
  );

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

  /**
   * 侧栏真正占位的时机。
   * 面板本身帮不上忙的两种情况：用户收起、或一个会话都没有（旁路无从谈起）。
   * 这两种情况下容器宽度直接归零，不留一条空白。
   */
  const agentShown = agentOpen && !!activeTab;

  return (
    <div className="app" data-theme={themeName}>
      <TitleBar
        title={appTitle}
        themeName={themeName}
        onCycleTheme={cycleTheme}
        onOpenSettings={setSettingsWin}
        settingsOpen={settingsOpen}
      />

      <TabBar
        sessionPopupOpen={sessionPopupOpen}
        onToggleSessionPopup={() => setSessionPopupOpen((v) => !v)}
        tabs={tabs}
        activeTab={activeTab}
        onSelect={setActiveTab}
        onClose={closeTab}
        onNewTab={newLocalTab}
        onReconnect={(id) => reconnect(id, false)}
        onDisconnect={disconnectTab}
        onOpenSftp={openSftp}
        onDuplicate={(id) => duplicateTab(id, false)}
        onDuplicateWithState={(id) => duplicateTab(id, true)}
        onRename={renameTab}
        onMove={moveTab}
        agentOpen={agentOpen}
        onToggleAgent={() => setAgentOpen((v) => !v)}
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
                onDisconnect={() => (t.local ? closeTab(t.id) : disconnectTab(t.id))}
                onOpenSftp={() => openSftp(t.id)}
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
                <button className="btn-link" onClick={() => closeTab(activeTabState.id)}>
                  关闭标签
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
                  <li>点击标签栏最左侧「☰ 连接」打开连接列表，双击即可连接</li>
                  <li>右键标签或终端选择「SFTP 文件管理」打开文件管理器</li>
                  <li>右上角「⚙ 设置」调整外观、自定义快捷键</li>
                  <li>
                    {formatCombo(shortcuts.copy) || "（未设置）"} 复制 ·{" "}
                    {formatCombo(shortcuts.paste) || "（未设置）"} 粘贴 · Ctrl+C 发送中断信号
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

        {/*
          AI 助手侧栏。

          放在 .workspace 里（与 .main 并列）而不是 .term-area 里：.term-area 是
          position:relative，终端宿主是 inset:0 的绝对定位元素，塞在这里只会盖住
          终端右边一块；而 .workspace 本来就是 flex 行容器、.main 是 flex:1，
          于是侧栏占多少、.main 就少多少 —— 终端宽度自然跟着变，
          TerminalView 现有的 ResizeObserver 会重新 fit 并把新尺寸推给后端。
          这样既不用改 term-area 的内部结构，也不用新造一套尺寸通知。

          面板**始终挂载**：聊天记录按会话存在组件内部（切会话互不可见），
          卸载即丢失。收起只是把 visible 置 false（组件自己返回 null、停掉
          事件订阅与状态轮询），并把容器宽度归零。
        */}
        <div className={`agent-side${agentShown ? "" : " collapsed"}`}>
          <AgentPanel
            sessionId={activeTab ?? ""}
            visible={agentShown}
            onClose={() => setAgentOpen(false)}
            onSendToTerminal={sendToTerminal}
            onOpenSettings={() => setSettingsWin("agent")}
          />
        </div>
      </div>

      {/* 右下角计数 = 当前打开的标签数（与 Xshell「N 会话」一致），不是已保存的连接数 */}
      <StatusBar
        status={statusBar}
        peer={peer}
        sessionCount={tabs.length}
        connectedCount={tabs.filter((t) => t.status === "connected").length}
        statsTarget={
          !activeTabState
            ? null
            : activeTabState.local
              ? { tabId: activeTabState.id, local: true, hostKey: "local" }
              : activeTabState.status === "connected"
                ? {
                    tabId: activeTabState.id,
                    local: false,
                    hostKey: peer ?? activeTabState.id,
                  }
                : null
        }
        activeTabId={activeTabState?.id ?? null}
      />

      {settingsWin === "appearance" && (
        <AppearanceDialog
          themeName={themeName}
          themeNames={THEME_NAMES}
          onTheme={setThemeName}
          fontFamily={fontFamily}
          fonts={FONT_FAMILIES}
          onFontFamily={setFontFamily}
          fontSize={fontSize}
          onFontSize={setFontSize}
          lineHeight={lineHeight}
          onLineHeight={setLineHeight}
          onClose={() => setSettingsWin(null)}
        />
      )}
      {settingsWin === "shortcuts" && <ShortcutsDialog onClose={() => setSettingsWin(null)} />}
      {settingsWin === "agent" && <AgentDialog onClose={() => setSettingsWin(null)} />}

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

      {/* 弹出式会话列表 —— 从标签栏最左侧「☰ 连接」按钮下方弹出 */}
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
          io={{
            canExportSession: canExportActive,
            exportSessionHint: "当前标签不是 SSH 会话",
            onImport: () => {
              setSessionPopupOpen(false);
              importXsh();
            },
            onExportSession: () => {
              setSessionPopupOpen(false);
              exportSessionXsh();
            },
            onExportAll: () => {
              setSessionPopupOpen(false);
              exportAllXsh();
            },
          }}
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
        {left > 0
          ? left >= 60
            ? `${Math.floor(left / 60)} 分 ${String(left % 60).padStart(2, "0")} 秒后`
            : `${left} 秒后`
          : "正在"}
        重连 {host}（第 {attempt}/{max} 次）
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
