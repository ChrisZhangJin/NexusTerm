import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { emit, listen } from "@tauri-apps/api/event";
import SessionList from "./components/SessionList";
import SessionDialog from "./components/SessionDialog";
import TerminalView, { clock } from "./components/TerminalView";
import FileManager from "./components/FileManager";
import TabBar from "./components/TabBar";
import StatusBar from "./components/StatusBar";
import { AgentPanel } from "./components/AgentPanel";
import {
  AppearanceDialog,
  BehaviorDialog,
  AgentDialog,
  ShortcutsDialog,
} from "./components/SettingsDialogs";
import KeyManagerDialog from "./components/KeyManagerDialog";
import MacroDialog from "./components/MacroDialog";
import AuthPromptDialog from "./components/AuthPromptDialog";
import { api, onAuthPrompt, onAgentTab, stringToBytes } from "./api";
import { loadAgentConfig } from "./lib/agent";
import {
  parseProbe,
  restoreCommand,
  restoreNotice,
  stealthInput,
  type TermContext,
} from "./lib/context";
import { cellWidth, getTerm, onInput, snapshotConsole, waitForPrompt } from "./lib/termRegistry";
import {
  isTerminalReply,
  runMacro,
  startRecording,
  type MacroRecorder,
} from "./lib/macro";
import ConfirmDialog, { type ConfirmOptions } from "./components/ConfirmDialog";
import { resolveAuth } from "./lib/auth";
import {
  getExtras,
  isSidecarDenied,
  loadExtras,
  markCompat,
  updateExtras,
} from "./lib/sessionExtras";
import { normalizeTimeout, toFriendlyError } from "./lib/errors";
import { BUILTIN_THEMES, DEFAULT_THEME, FONT_FAMILIES } from "./lib/theme";
import { applyChromeVars } from "./lib/chrome";
import { formatCombo, getPrefs, setPrefs, useShortcuts } from "./lib/prefs";
import type {
  AgentTabInfo,
  AgentTabRequest,
  AgentTarget,
  AuthMethod,
  AuthPrompt,
  ClosedInfo,
  MacroRecord,
  MacroStep,
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

/** AI 面板宽度边界（px）：下限保证内容不挤压，上限防止把终端拖没 */
const AGENT_MIN_W = 280;
const AGENT_MAX_W = 760;
const clampAgentWidth = (w: number) =>
  Math.min(AGENT_MAX_W, Math.max(AGENT_MIN_W, Math.round(w)));

export default function App() {
  const [sessions, setSessions] = useState<SessionRecord[]>([]);
  const [tabs, setTabs] = useState<TerminalTab[]>([]);
  const [activeTab, setActiveTab] = useState<string | null>(null);
  const [dialogOpen, setDialogOpen] = useState(false);
  const [editing, setEditing] = useState<SessionRecord | null>(null);
  /** 设置窗口：外观 / 快捷键 / 密钥管理 / AI 助手（模态，同一时刻只开一个） */
  const [settingsWin, setSettingsWin] = useState<
    "appearance" | "behavior" | "shortcuts" | "keys" | "agent" | null
  >(null);
  const settingsOpen = settingsWin !== null;
  // 按 tabId 索引：哪些标签开着 SFTP 文件管理器、连的是哪个 session。
  // 用 map 而不是单个值，是因为面板必须「只在自己所属的标签前台时展示」——
  // 单个全局值做不到这件事：切标签页时它既不隐藏也不跟着换，表现就是不管
  // 切到哪个标签，看到的都是最初那个 session 的面板。
  const [sftpTabs, setSftpTabs] = useState<Record<string, SessionRecord>>({});
  /**
   * AI 助手侧栏是否展开。
   * 与「面板是否挂载」是两件事：面板始终挂载、只切 visible，
   * 否则一轮对话的历史会随收起一起丢掉。
   */
  const [agentOpen, setAgentOpen] = useState(false);
  /**
   * AI 面板宽度（px），持久化到 localStorage。
   * 拖动左缘手柄改宽；clamp 在 [AGENT_MIN_W, AGENT_MAX_W]，避免拖没或把终端挤没。
   */
  const [agentWidth, setAgentWidth] = useState(() => {
    const v = Number(localStorage.getItem("nx.agentWidth"));
    return Number.isFinite(v) && v >= AGENT_MIN_W && v <= AGENT_MAX_W ? v : 340;
  });
  useEffect(() => {
    localStorage.setItem("nx.agentWidth", String(agentWidth));
  }, [agentWidth]);
  /** 面板左缘拖拽：按下时记录起点，move 时按反向位移（往左拖=变宽）更新宽度 */
  const startAgentResize = useCallback(
    (e: React.MouseEvent) => {
      e.preventDefault();
      const startX = e.clientX;
      const startW = agentWidth;
      const onMove = (ev: MouseEvent) => {
        const next = clampAgentWidth(startW + (startX - ev.clientX));
        setAgentWidth(next);
      };
      const onUp = () => {
        window.removeEventListener("mousemove", onMove);
        window.removeEventListener("mouseup", onUp);
        document.body.style.cursor = "";
        document.body.style.userSelect = "";
      };
      window.addEventListener("mousemove", onMove);
      window.addEventListener("mouseup", onUp);
      // 拖动期间全局锁定光标与选区，否则会连带选中终端文本
      document.body.style.cursor = "col-resize";
      document.body.style.userSelect = "none";
    },
    [agentWidth],
  );
  /** AI 面板是否已浮出为独立窗口。浮出时主窗口内嵌面板收起。 */
  const [agentFloated, setAgentFloated] = useState(false);

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
  /** 标签 id → 会话配置 id。私钥口令弹窗只知道标签，回写口令要用配置 id */
  const configByTab = useRef<Map<string, string>>(new Map());
  /** 标签最后一次连上的时刻。用来判断「是不是刚连上就掉线」（堡垒机特征） */
  const connectedAt = useRef<Map<string, number>>(new Map());
  const shortcuts = useShortcuts();

  // —— 轻量提示条（导入/导出/复制会话等结果） ——
  const [toast, setToast] = useState<string | null>(null);
  const toastTimer = useRef<number | undefined>(undefined);
  const showToast = useCallback((msg: string) => {
    setToast(msg);
    if (toastTimer.current) window.clearTimeout(toastTimer.current);
    toastTimer.current = window.setTimeout(() => setToast(null), 5000);
  }, []);

  // —— 二次验证（OTP / 堡垒机挑战）——
  // 后端在认证中途挂起等答案，这里弹框收集。
  //
  // 必须用**队列**而不是单个 state：终端标签和 SFTP 面板是两条独立连接，
  // 打到同一台要 OTP 的机器时会同时各弹一个问题。单槽的话后到的会把先到的
  // 顶掉，被顶掉的那个连接只能干等到 180 秒超时后失败 —— 表现就是
  // 「明明输了验证码却连不上」。
  const [authPrompts, setAuthPrompts] = useState<AuthPrompt[]>([]);
  /** 本次重连过程中出现过交互式认证（OTP）的标签：自动重试没有意义，交给用户手动点 */
  const interactiveAuth = useRef(new Set<string>());
  /** 用户主动取消了验证码的标签：这不是「连接失败」，不该打红色错误 */
  const authCancelled = useRef(new Set<string>());

  useEffect(() => {
    let alive = true;
    let unlisten: (() => void) | undefined;
    onAuthPrompt((p) => {
      // 到了这一步说明这条连接必须人工介入，自动重试只会反复弹窗
      interactiveAuth.current.add(p.sessionId);
      setAuthPrompts((prev) => {
        // 同一连接同一轮重复下发（输错重试）时替换，不要把一个连接排两次。
        // kind 必须参与比较：私钥口令和随后服务端下发的 OTP 都是第 1 轮，
        // 不区分就会互相顶掉。
        const rest = prev.filter(
          (x) =>
            !(x.sessionId === p.sessionId && x.round === p.round && x.kind === p.kind)
        );
        return [...rest, p];
      });
    }).then((fn) => {
      // 订阅是异步的，组件可能已经卸载（开发时 StrictMode 会双跑）
      if (alive) unlisten = fn;
      else fn();
    });
    return () => {
      alive = false;
      unlisten?.();
    };
  }, []);

  /** 答完/取消队首那条，后面的自动顶上来 */
  const resolveAuthPrompt = useCallback(
    (sessionId: string, answers: string[] | null, remember = false) => {
      if (answers === null) authCancelled.current.add(sessionId);
      // 私钥口令要留下来：本次运行内重连直接复用，勾了「记住」再落库。
      // 不这么做的话后端下次重连仍拿不到口令，又会弹一次窗。
      if (answers && answers.length > 0) {
        const prompt = authPrompts.find((p) => p.sessionId === sessionId);
        if (prompt?.kind === "passphrase") {
          const configId = configByTab.current.get(sessionId);
          if (configId) {
            passphraseRef.current.set(configId, answers[0]);
            if (remember) api.savePassphrase(configId, answers[0]).catch(() => {});
          }
        }
      }
      setAuthPrompts((prev) => prev.filter((p) => p.sessionId !== sessionId));
      api.sshAuthRespond(sessionId, answers).catch(() => {});
    },
    [authPrompts]
  );

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
    // 附加状态（登录脚本 / 兼容模式）必须先于任何连接就绪：
    // dial 里要同步判断 compat，晚到就只能靠默认值，等于这次白连
    void loadExtras();
    api.health().then((h) => console.log("[nexus-term]", h)).catch(() => {});
  }, [refresh]);

  // 异步流程（重连循环、连接回调）里要读最新值，用 ref 桥接
  const tabsRef = useRef(tabs);
  tabsRef.current = tabs;
  /** 同上：Agent 的 tab 动作在下发时才读「哪个标签是当前的」，不能闭包捕获旧值 */
  const activeTabRef = useRef(activeTab);
  activeTabRef.current = activeTab;
  const sessionsRef = useRef(sessions);
  sessionsRef.current = sessions;
  /** 每个标签当前重连循环的取消开关 */
  const reconnectCtl = useRef(new Map<string, { cancelled: boolean }>());

  const patchTab = useCallback((id: string, patch: Partial<TerminalTab>) => {
    setTabs((prev) => prev.map((t) => (t.id === id ? { ...t, ...patch } : t)));
  }, []);

  /**
   * 后台标签收到新输出：点亮标签栏的提醒动效。
   *
   * `TerminalView` 已经在「自己不是前台标签」时才调它，这里只再做一次幂等检查——
   * 真正高频的是日志刷屏（tmux/docker 这类场景一秒钟能触发几十次 onWriteParsed），
   * 已经点亮了就不用再 `setTabs` 一次，否则整个标签栏跟着空刷新。
   */
  const markTabActivity = useCallback((tabId: string) => {
    if (tabsRef.current.find((t) => t.id === tabId)?.hasActivity) return;
    patchTab(tabId, { hasActivity: true });
  }, [patchTab]);

  // 标签一旦被看到就清掉提醒——不管是点出来的还是快捷键切过去的，统一在这收口
  useEffect(() => {
    if (activeTab) patchTab(activeTab, { hasActivity: false });
  }, [activeTab, patchTab]);

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
    // 记住「标签 → 会话配置」，私钥口令弹窗答完要回写到对应配置上
    configByTab.current.set(tabId, s.id);

    // 私钥口令的来源有三处，优先级从快到慢：
    //   内存缓存（本次运行里输过） → 会话配置（上次勾了「记住口令」） → 弹窗问用户
    let passphrase = passphraseRef.current.get(s.id);
    if (passphrase === undefined && s.authType === "key") {
      const saved = await api.loadPassphrase(s.id).catch(() => null);
      if (saved) {
        passphraseRef.current.set(s.id, saved);
        passphrase = saved;
      }
    }
    const auth: AuthMethod = await resolveAuth(s, passphrase);
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
      // 后端按这个 id 登记「已认证连接」，SFTP 面板复用它开子系统，
      // 免去再认证一次（OTP 服务器就是免再输一次验证码）
      configId: s.id,
      // 兼容模式（堡垒机只放行一条通道）：连上后一个额外通道都不开，
      // 否则每 3 秒一次的 cwd 探测会把整条连接反复顶掉
      sidecar: !getExtras(s.id).compat,
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
      const tab = tabsRef.current.find((t) => t.id === tabId);
      // 兼容模式的连接一条额外通道都不开：probe 本身会立刻返回，留着只是空转
      if (tab?.sessionId && getExtras(tab.sessionId).compat) return tab.ctx;
      try {
        const c = parseProbe(await api.sshProbe(tabId));
        if (c) patchTab(tabId, { ctx: c });
        return c ?? tabsRef.current.find((t) => t.id === tabId)?.ctx;
      } catch (e) {
        // 服务端明确拒绝并行通道 → 这台机器是堡垒机，永久切兼容模式。
        // 否则这条连接上每 15 秒还会再试一次，每次都可能把连接掀掉。
        if (isSidecarDenied(e) && markCompat(tab?.sessionId, tabId)) {
          showToast(
            "该服务器（堡垒机）不支持并行通道，已切换为兼容模式：不再采集远端资源，下次连接也不会再开额外通道"
          );
        }
        return tab?.ctx;
      }
    },
    [patchTab, showToast]
  );

  // ───────────────────────── 宏 ─────────────────────────
  //
  // 宏替换掉了原来的「跳板机登录路径」专用功能：那是一套只会录「菜单里选序号」
  // 的特例，换一台堡垒机、或者想顺手跑几条命令就不适用了。现在同一个引擎既能
  // 重放登录路径，也能做任何「等一会儿、敲一行」的事，绑到会话上就是自动执行。

  const [macros, setMacros] = useState<MacroRecord[]>([]);
  const [macroOpen, setMacroOpen] = useState(false);
  const [macroFocus, setMacroFocus] = useState<string | null>(null);
  /** 正在回放的宏：悬浮条据此显示进度，点一下中止 */
  const [macroRun, setMacroRun] = useState<{
    tabId: string;
    name: string;
    step: number;
    total: number;
    ctl: AbortController;
  } | null>(null);
  const [recording, setRecording] = useState<{ tabId: string; rec: MacroRecorder } | null>(null);
  const [recSteps, setRecSteps] = useState(0);

  const macrosRef = useRef<MacroRecord[]>([]);
  macrosRef.current = macros;
  /** 标签 → 正在回放的宏的取消开关（关标签 / 断开时要收掉） */
  const macroCtl = useRef(new Map<string, AbortController>());

  const refreshMacros = useCallback(async () => {
    try {
      setMacros(await api.listMacros());
    } catch (e) {
      console.error("加载宏失败", e);
    }
  }, []);

  useEffect(() => {
    void refreshMacros();
  }, [refreshMacros]);

  /**
   * 旧版「跳板机登录路径」→ 宏 的一次性迁移。
   *
   * 原来的实现只认「屏幕像菜单」这一种场景，换台堡垒机或想顺手跑几条命令就
   * 不适用了；现在统一成宏，但用户已经录过的路径不能丢。转换后清空旧字段，
   * 下次启动就不会重复生成。
   */
  const migratedLogin = useRef(false);
  useEffect(() => {
    if (migratedLogin.current || sessions.length === 0) return;
    migratedLogin.current = true;
    void (async () => {
      // 附加状态是异步加载的，迁移必须等它到位，否则读到的都是空值
      await loadExtras();
      const added: MacroRecord[] = [];
      for (const s of sessionsRef.current) {
        const e = getExtras(s.id);
        if (!e.loginScript.trim() || e.macroId) continue;
        const steps: MacroStep[] = [];
        for (const line of e.loginScript
          .split(/\r?\n/)
          .map((x) => x.trim())
          .filter(Boolean)) {
          // 旧逻辑是「菜单出来 → 发一步」，等价于「等屏幕稳定 → 发一行」
          steps.push({ kind: "wait", pattern: "", timeoutMs: 0 });
          steps.push({ kind: "send", text: line, enter: true });
        }
        if (!steps.length) continue;
        try {
          const m = await api.createMacro(`登录路径 · ${s.name}`, steps);
          updateExtras(s.id, { loginScript: "", macroId: m.id, macroOn: e.loginScriptOn });
          added.push(m);
        } catch (err) {
          console.error("迁移登录路径失败", err);
        }
      }
      if (added.length) {
        // 重新拉一次而不是本地合并：启动时的 listMacros 可能与迁移并发，
        // 它的快照里没有这些新宏，直接合并会被晚到的结果覆盖掉
        await refreshMacros();
        showToast(`已将 ${added.length} 条跳板机登录路径转成宏，可在「宏」里修改`);
      }
    })();
  }, [sessions.length, showToast, refreshMacros]);

  const createMacro = useCallback(async (name: string, steps: MacroStep[]) => {
    const m = await api.createMacro(name, steps);
    setMacros((prev) => [...prev, m]);
    return m;
  }, []);

  const updateMacro = useCallback(async (id: string, name: string, steps: MacroStep[]) => {
    const m = await api.updateMacro(id, name, steps);
    setMacros((prev) => prev.map((x) => (x.id === id ? m : x)));
    return m;
  }, []);

  const deleteMacro = useCallback(async (id: string) => {
    await api.deleteMacro(id);
    setMacros((prev) => prev.filter((m) => m.id !== id));
    // 绑定关系也要一起断掉，否则会话每次连接都会去找一条不存在的宏
    for (const s of sessionsRef.current) {
      if (getExtras(s.id).macroId === id) updateExtras(s.id, { macroId: "", macroOn: false });
    }
  }, []);

  /**
   * 在一个标签上跑宏。
   *
   * 三重中止条件：连接断开、用户按键、用户点悬浮条。用户按键中止看着有点凶，
   * 但宏正在往远端打字时人再插一脚，两边输入交错后基本没法收拾 —— 停下重来
   * 永远比「看起来在跑、其实发错了」便宜。
   */
  const executeMacro = useCallback(
    async (tabId: string, macro: MacroRecord, opts: { auto?: boolean } = {}) => {
      // 提示里区分「手动点运行」和「连上就自动跑」：后者失败时用户可能正在干别的，
      // 得让他一眼看出这条消息来自哪个机制
      const tag = opts.auto ? "自动宏" : "宏";
      const tab = tabsRef.current.find((t) => t.id === tabId);
      if (!tab || tab.status !== "connected") {
        showToast(`${tag}「${macro.name}」没有执行：当前标签未连接`);
        return;
      }
      if (!macro.steps.length) {
        showToast(`${tag}「${macro.name}」没有任何步骤`);
        return;
      }

      macroCtl.current.get(tabId)?.abort();
      const ctl = new AbortController();
      macroCtl.current.set(tabId, ctl);
      setMacroRun({ tabId, name: macro.name, step: 1, total: macro.steps.length, ctl });

      // 终端对远端查询的自动应答不算「用户插手」，否则 tmux / vim 一发查询
      // 就会把自动宏掐掉
      const off = onInput(tabId, (d) => {
        if (!isTerminalReply(d)) ctl.abort();
      });
      try {
        const r = await runMacro(tabId, macro.steps, {
          signal: ctl.signal,
          write: tab.local
            ? (id, bytes) => api.localWrite(id, bytes)
            : undefined,
          onStep: (i) =>
            setMacroRun((prev) =>
              prev && prev.ctl === ctl ? { ...prev, step: i + 1 } : prev
            ),
        });
        if (r.ok) {
          showToast(`${tag}「${macro.name}」执行完成（${r.sent} 步）`);
        } else if (r.reason === "cancelled") {
          showToast(`${tag}「${macro.name}」已中止`);
        } else if (r.reason === "closed") {
          showToast(`${tag}「${macro.name}」中断：连接已断开`);
        } else {
          const at = (r.step ?? 0) + 1;
          const step = macro.steps[r.step ?? 0];
          const why =
            step?.kind === "wait" && step.pattern
              ? `等不到「${step.pattern}」`
              : "等待超时";
          showToast(
            `${tag}「${macro.name}」第 ${at} 步${why}，已停止（后面的步骤没有发出去）`
          );
        }
      } finally {
        off();
        if (macroCtl.current.get(tabId) === ctl) macroCtl.current.delete(tabId);
        setMacroRun((prev) => (prev && prev.ctl === ctl ? null : prev));
      }
    },
    [showToast]
  );

  /** 连接就绪后执行会话绑定的宏（登录路径、自动初始化命令…） */
  const runAutoMacro = useCallback(
    async (tabId: string, configId: string | undefined) => {
      if (!configId) return;
      const e = getExtras(configId);
      if (!e.macroOn || !e.macroId) return;
      const m = macrosRef.current.find((x) => x.id === e.macroId);
      if (!m) return;
      await executeMacro(tabId, m, { auto: true });
    },
    [executeMacro]
  );

  /** 录制：把用户接下来的操作变成一条新宏，停下来就进编辑器 */
  const toggleRecording = useCallback(async () => {
    if (recording) {
      const steps = recording.rec.stop();
      setRecording(null);
      setRecSteps(0);
      if (!steps.length) {
        showToast("没有录到任何操作");
        return;
      }
      try {
        const stamp = new Date();
        const hhmm = `${String(stamp.getHours()).padStart(2, "0")}${String(
          stamp.getMinutes()
        ).padStart(2, "0")}`;
        const m = await createMacro(`录制 ${hhmm}`, steps);
        setMacroFocus(m.id);
        setMacroOpen(true);
        showToast(`已录成宏「${m.name}」，可以改名或调整等待条件`);
      } catch (e) {
        showToast(`保存宏失败：${String(e)}`);
      }
      return;
    }
    const tab = tabsRef.current.find((t) => t.id === activeTab);
    if (!tab || tab.status !== "connected") {
      showToast("先连接一个会话再录制");
      return;
    }
    const rec = startRecording(tab.id);
    setRecSteps(0);
    setRecording({ tabId: tab.id, rec });
    setMacroOpen(false);
  }, [recording, activeTab, createMacro, showToast]);

  // 录制中的步数：录制器是同步累积的，只有靠轮询才能反映到界面上
  useEffect(() => {
    if (!recording) return;
    const t = window.setInterval(() => setRecSteps(recording.rec.count()), 400);
    return () => window.clearInterval(t);
  }, [recording]);

  // 标签断开 / 关掉：收掉宏与录制，别把定时器和输入订阅留着
  useEffect(() => {
    for (const [id, ctl] of [...macroCtl.current]) {
      const tab = tabs.find((t) => t.id === id);
      if (!tab || tab.status !== "connected") {
        ctl.abort();
        macroCtl.current.delete(id);
      }
    }
    if (recording) {
      const tab = tabs.find((t) => t.id === recording.tabId);
      if (!tab || tab.status !== "connected") {
        recording.rec.cancel();
        setRecording(null);
        setRecSteps(0);
      }
    }
  }, [tabs, recording]);

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
        connectedAt.current.set(tabId, Date.now());
        // 顺序很重要：先跑会话绑定的宏（登录路径会在这里把菜单选完、进到目标机器），
        // 再注入 cd / tmux 还原命令。反过来的话，还原命令会被喂给菜单本身，
        // 直接变成一个非法选项。
        void runAutoMacro(tabId, s.id).then(() =>
          applyContext(tabId, restore, "duplicate", since)
        );
      } catch (e) {
        // 用户自己关掉了验证码弹窗，不是连接出错，别标成红色错误
        if (authCancelled.current.delete(tabId)) {
          patchTab(tabId, { status: "closed", error: "已取消二次验证" });
          return;
        }
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
      // 新一轮重连重新观察：上一轮留下的标记不该影响这一轮
      interactiveAuth.current.delete(tabId);
      authCancelled.current.delete(tabId);
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
          connectedAt.current.set(tabId, Date.now());
          patchTab(tabId, {
            status: "connected",
            reconnectAttempt: undefined,
            reconnectMax: undefined,
            reconnectAt: undefined,
            closedAt: undefined,
          });
          // 用断线前最后一次探测到的上下文恢复（tmux 直接 attach 回原会话）。
          // 仍然排在自动宏之后：宏没跑完时目标机器还没进去。
          void runAutoMacro(tabId, s.id).then(() =>
            applyContext(tabId, tab.ctx, "reconnect", since)
          );
          return;
        } catch (e) {
          lastErr = toFriendlyError(String(e)).message;
          if (ctl.cancelled) return;
          // 这条连接要求验证码（或用户取消了验证码）：重试只会再弹一次窗，
          // 码还可能是同一个已经失效的 —— 停下来让用户手动决定。
          if (interactiveAuth.current.has(tabId)) {
            interactiveAuth.current.delete(tabId);
            authCancelled.current.delete(tabId);
            lastErr = `${lastErr}。需要重新输入二次验证码，请点「重新连接」`;
            break;
          }
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
      interactiveAuth.current.delete(tabId);
      // 后端可能正挂在「等用户填验证码」上。光置取消标志只是前端不再重试，
      // 那条 connect 任务还会继续等到 180 秒超时，弹窗也留在屏幕上。
      // 关会话会顺带唤醒它（SessionManager::close 里 cancel 了 pending 应答）。
      api.sshClose(tabId).catch(() => {});
      setAuthPrompts((prev) => prev.filter((p) => p.sessionId !== tabId));
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

      // 「连上没几秒就掉线」+ 这条连接还开着旁路通道 → 高度怀疑是堡垒机
      // 因为第二条 CHANNEL_OPEN 把整条连接掐了（有的实现连拒绝包都不发，
      // 我们那边只能看到 lost，认不出是「被拒」）。这里做兜底学习：
      // 自动切兼容模式，重连时才不会无限循环「连上→几秒→断开」。
      const tab = tabsRef.current.find((t) => t.id === tabId);
      const at = connectedAt.current.get(tabId);
      if (
        tab?.sessionId &&
        info.reason === "lost" &&
        at &&
        Date.now() - at < 20_000 &&
        markCompat(tab.sessionId, tabId)
      ) {
        showToast(
          "连接建立后几秒即断开，已按堡垒机处理：为该会话打开「兼容模式」（不再开旁路通道）。若这是网络问题，可在编辑连接里关掉兼容模式"
        );
      }

      // 只有「连接丢失」才自动重连；远端 exit / 主动关闭属于正常结束
      if (info.reason === "lost") reconnect(tabId, true);
    },
    [patchTab, reconnect, showToast]
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
    interactiveAuth.current.delete(tabId);
    configByTab.current.delete(tabId);
    // 标签在等验证码时被关掉：后端 close() 会解除挂起，弹窗也得跟着消失，
    // 否则它会一直杵在那儿等一个已经不存在的连接。
    setAuthPrompts((prev) => prev.filter((p) => p.sessionId !== tabId));
    // 标签带走它自己的 SFTP 面板，不然连接/状态会悄悄留着
    setSftpTabs((prev) => {
      if (!(tabId in prev)) return prev;
      const next = { ...prev };
      delete next[tabId];
      return next;
    });
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
      interactiveAuth.current.delete(tabId);
      const wasLive = t.status === "connected" || t.status === "reconnecting" || t.status === "connecting";
      api.sshClose(tabId).catch(() => {});
      setAuthPrompts((prev) => prev.filter((p) => p.sessionId !== tabId));
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

  /** 把 AI 面板浮出为独立窗口：开窗 + 收起内嵌面板 */
  const floatAgentOut = useCallback(async () => {
    if (!activeTab) return;
    await api.agentOpenWindow(activeTab);
    setAgentFloated(true);
    setAgentOpen(false);
  }, [activeTab]);

  /**
   * 交给 Agent 的「可操作会话」清单。
   *
   * 只列**还活着**的标签：给模型一个已经断开的会话 id，它会在那边吃到
   * 一句看不懂的「会话不存在」，而用户完全不知道该去修什么。
   */
  const agentSessions: AgentTarget[] = useMemo(
    () =>
      tabs
        .filter((t) => t.status === "connected" || t.status === "reconnecting")
        .map((t) => ({ id: t.id, label: t.customTitle || t.title || t.id })),
    [tabs],
  );

  // 浮窗开着时，主窗口切换活动会话要通知浮窗跟随。
  // 顺带把「可操作会话」清单一起发过去 —— 浮窗里没有标签树，
  // 而「执行」模式的作用范围选择器全靠这份清单。
  const agentSessionKey = agentSessions.map((s) => s.id).join("|");
  useEffect(() => {
    if (!agentFloated || !activeTab) return;
    void emit("nx://agent/active-session", { sessionId: activeTab, sessions: agentSessions });
    // agentSessionKey 代替 agentSessions 本身进依赖数组：数组每次渲染都是新引用，
    // 直接放进去会让这个 effect 每帧都跑一遍。
  }, [agentFloated, activeTab, agentSessionKey]); // eslint-disable-line react-hooks/exhaustive-deps

  // 监听浮窗回传的事件：写终端、开设置、窗口关闭
  useEffect(() => {
    const unSend = listen<{ sessionId: string; text: string }>(
      "nx://agent/send-to-terminal",
      (e) => {
        if (e.payload?.sessionId) void sendToTerminal(e.payload.sessionId, e.payload.text);
      },
    );
    const unSettings = listen("nx://agent/open-settings", () => setSettingsWin("agent"));
    // 浮窗被关掉 → 恢复「未浮出」状态，让用户能重新内嵌或再次浮出
    const unClosed = listen("nx://agent/window-closed", () => setAgentFloated(false));
    return () => {
      void unSend.then((f) => f());
      void unSettings.then((f) => f());
      void unClosed.then((f) => f());
    };
  }, [sendToTerminal]);

  // Agent 自己开的新会话（`nx-exec` 的 open 动作）：后端已经真的 spawn/connect
  // 好了，这里只需要把它接成一个标签页，不再重复调用 localSpawn / sshConnect。
  useEffect(() => {
    const un = listen<{
      id: string;
      originSessionId: string;
      kind: "local" | "ssh";
      label: string;
    }>("nx://agent/session-opened", (e) => {
      const { id, originSessionId, kind, label } = e.payload;
      insertTab(
        {
          id,
          title: label || (kind === "local" ? "本地终端" : "远端会话"),
          sessionId: null,
          status: "connected",
          local: kind === "local",
        },
        originSessionId
      );
      seedAgentAttach(id);
    });
    return () => void un.then((f) => f());
  }, [insertTab, seedAgentAttach]);

  // 启动时同步一次浮窗状态（热重载 / 重开主窗口时浮窗可能还在）
  useEffect(() => {
    void api.agentWindowOpen().then(setAgentFloated).catch(() => {});
  }, []);

  /**
   * Agent 下发的标签页动作（`nx-exec` 的 `tab` 动作）。
   *
   * 只能在这里落地：tab 是 React state，后端连「开着几个标签页」都不知道，
   * 它只负责把动作转过来。后端在等回执（3 秒超时，超时会被记成「未确认」），
   * 所以**每条分支都必须 ack**，失败也要说清楚为什么 —— 静默就是上次那种
   * 「它说了要做，然后没有然后」。
   *
   * 关闭走的是 `closeTabNow`：用户选了「直接执行，面板留痕」，
   * 不再弹确认框，但步骤卡里会写明关了哪一个。
   */
  const handleAgentTab = useCallback(
    async (req: AgentTabRequest) => {
      const ack = (ok: boolean, detail: string, list?: AgentTabInfo[]) =>
        api.agentTabAck(req.requestId, ok, detail, list).catch(() => false);
      const now = tabsRef.current;
      const nameOf = (t: TerminalTab) => t.customTitle || t.title || t.id;
      const toInfo = (t: TerminalTab): AgentTabInfo => ({
        id: t.id,
        title: nameOf(t),
        kind: t.local ? "local" : "ssh",
        status: t.status,
        active: t.id === activeTabRef.current,
      });

      if (req.op === "list") {
        const list = now.map(toInfo);
        await ack(true, `当前共 ${list.length} 个标签页`, list);
        return;
      }

      const target = now.find((t) => t.id === req.target);
      if (!target) {
        await ack(false, `没有 id 为「${req.target ?? "（空）"}」的标签页`);
        return;
      }

      switch (req.op) {
        case "focus":
          setActiveTab(target.id);
          await ack(true, `已切到「${nameOf(target)}」`);
          return;
        case "rename": {
          const name = (req.name ?? "").trim();
          if (!name) {
            await ack(false, "新名字为空");
            return;
          }
          const before = nameOf(target);
          patchTab(target.id, { customTitle: name });
          await ack(true, `已把「${before}」改名为「${name}」`);
          return;
        }
        case "close": {
          // 与后端同一条护栏：关掉它等于把 Agent 自己的会话也收了
          if (target.id === req.sessionId) {
            await ack(false, "不能关闭 Agent 自己所在的标签页");
            return;
          }
          const name = nameOf(target);
          closeTabNow(target.id);
          await ack(true, `已关闭「${name}」及其中的进程`);
          return;
        }
        case "reconnect": {
          if (target.local) {
            await ack(false, "本地终端不需要重连，只能关掉重开");
            return;
          }
          if (target.status === "connected" || target.status === "connecting") {
            await ack(false, `「${nameOf(target)}」当前是 ${target.status}，不需要重连`);
            return;
          }
          void reconnect(target.id, false);
          await ack(true, `已开始重连「${nameOf(target)}」`);
          return;
        }
        default:
          await ack(false, `不认识的操作：${req.op}`);
      }
    },
    [closeTabNow, patchTab, reconnect]
  );

  useEffect(() => {
    const un = onAgentTab((req) => {
      void handleAgentTab(req);
    });
    return () => void un.then((f) => f());
  }, [handleAgentTab]);

  const saveSession = useCallback(
    async (payload: NewSession, password: string, passphrase: string | null) => {
      let id = editing?.id;
      if (editing) {
        await api.updateSession(editing.id, payload);
      } else {
        const created = await api.createSession(payload);
        id = created.id;
      }
      if (!id) return;
      if (password) await api.savePassword(id, password);
      // passphrase === null：用户没动过这个字段，保持库里的原值
      if (payload.authType === "key" && passphrase !== null) {
        const value = passphrase.trim();
        await api.savePassphrase(id, value);
        if (value) passphraseRef.current.set(id, value);
        else passphraseRef.current.delete(id);
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
      setSftpTabs((prev) => ({ ...prev, [tabId]: s }));
    },
    [sessions, tabs]
  );

  /** 关掉某个标签的 SFTP 面板（面板自己的卸载 effect 会去断开连接） */
  const closeSftpTab = useCallback((tabId: string) => {
    setSftpTabs((prev) => {
      if (!(tabId in prev)) return prev;
      const next = { ...prev };
      delete next[tabId];
      return next;
    });
  }, []);

  const activeTabState = tabs.find((t) => t.id === activeTab) ?? null;
  const activeSession = activeTabState?.sessionId
    ? sessions.find((s) => s.id === activeTabState.sessionId) ?? null
    : null;

  // 队首那条才是当前要答的：同一连接同一时刻只会有一个问题在等，
  // 多个连接并发时按到达顺序逐个问。
  const authPrompt = authPrompts[0] ?? null;
  const authPromptReconnecting = authPrompt
    ? tabs.find((t) => t.id === authPrompt.sessionId)?.status === "reconnecting"
    : false;

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
                // 命令历史按会话身份存取：本地固定 'local'，已保存的 SSH
                // 连接是它的 sessionId；没有保存记录的临时连接退回 tabId
                // 本身（不持久化，行为与过去的纯内存历史一致）。
                historyKey={t.local ? "local" : t.sessionId ?? t.id}
                historyPersist={t.local || !!t.sessionId}
                onNotify={showToast}
                theme={theme}
                fontFamily={fontFamily}
                fontSize={fontSize}
                lineHeight={lineHeight}
                onDisconnect={() => (t.local ? closeTab(t.id) : disconnectTab(t.id))}
                onOpenSftp={() => openSftp(t.id)}
                onBackgroundOutput={() => markTabActivity(t.id)}
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

            {Object.entries(sftpTabs).map(([tabId, s]) => (
              <FileManager
                key={tabId}
                tabId={tabId}
                active={tabId === activeTab}
                session={s}
                passphrase={passphraseRef.current.get(s.id)}
                onClose={() => closeSftpTab(tabId)}
              />
            ))}

            {/* 宏回放 / 录制：贴在终端右上角，点一下即中止 */}
            {macroRun && macroRun.tabId === activeTab && (
              <div
                className="macro-running"
                title="点击中止这条宏"
                onClick={() => macroRun.ctl.abort()}
              >
                <span className="mr-dot" />
                宏「{macroRun.name}」{macroRun.step}/{macroRun.total}
                <span className="mr-hint">点击中止</span>
              </div>
            )}
            {recording && recording.tabId === activeTab && (
              <div
                className="macro-running rec"
                title="点击停止录制"
                onClick={() => void toggleRecording()}
              >
                <span className="mr-dot" />
                正在录制宏 · {recSteps} 步
                <span className="mr-hint">点击停止</span>
              </div>
            )}

            {tabs.length === 0 && Object.keys(sftpTabs).length === 0 && (
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

          面板**始终挂载**：整份对话是全局唯一的（助手面向所有会话，
          切标签只换「当前上下文」，不换聊天记录），卸载即丢失。
          收起只是把 visible 置 false（组件自己返回 null、停掉事件订阅），
          并把容器宽度归零。
        */}
        <div
          className={`agent-side${agentShown ? "" : " collapsed"}`}
          style={agentShown ? { width: agentWidth } : undefined}
        >
          {agentShown && (
            <div
              className="agent-resize"
              onMouseDown={startAgentResize}
              title="拖动调整面板宽度"
            />
          )}
          <AgentPanel
            sessionId={activeTab ?? ""}
            visible={agentShown}
            onClose={() => setAgentOpen(false)}
            onSendToTerminal={sendToTerminal}
            onOpenSettings={() => setSettingsWin("agent")}
            onFloatOut={floatAgentOut}
            sessions={agentSessions}
            onGetConsole={snapshotConsole}
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
                    sessionId: activeTabState.sessionId,
                  }
                : null
        }
        activeTabId={activeTabState?.id ?? null}
        onNotify={showToast}
        onOpenSettings={setSettingsWin}
        settingsOpen={settingsOpen}
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
      {settingsWin === "behavior" && (
        <BehaviorDialog
          onClose={() => setSettingsWin(null)}
          onOpenMacros={() => setMacroOpen(true)}
        />
      )}
      {settingsWin === "shortcuts" && <ShortcutsDialog onClose={() => setSettingsWin(null)} />}
      {settingsWin === "agent" && <AgentDialog onClose={() => setSettingsWin(null)} />}
      {settingsWin === "keys" && <KeyManagerDialog onClose={() => setSettingsWin(null)} />}

      {macroOpen && (
        <MacroDialog
          macros={macros}
          focusId={macroFocus}
          tabReady={activeTabState?.status === "connected"}
          tabLabel={
            activeTabState ? activeTabState.customTitle || activeTabState.title : null
          }
          sessionId={activeSession?.id ?? null}
          sessionName={activeSession?.name ?? null}
          boundId={
            activeSession && getExtras(activeSession.id).macroOn
              ? getExtras(activeSession.id).macroId || null
              : null
          }
          recording={!!recording}
          onBind={(id) => {
            if (!activeSession) return;
            updateExtras(activeSession.id, { macroId: id ?? "", macroOn: !!id });
          }}
          onRecord={() => void toggleRecording()}
          onRun={(m) => {
            if (!activeTab) return;
            setMacroOpen(false);
            void executeMacro(activeTab, m);
          }}
          onCreate={createMacro}
          onUpdate={updateMacro}
          onDelete={deleteMacro}
          onClose={() => setMacroOpen(false)}
        />
      )}

      {dialogOpen && (
        <SessionDialog
          editing={editing}
          macros={macros}
          onSave={saveSession}
          onClose={() => {
            setDialogOpen(false);
            setEditing(null);
          }}
        />
      )}

      {toast && <div className="toast">{toast}</div>}

      {authPrompt && (
        <AuthPromptDialog
          // 服务端可能连问多轮，换轮时必须重建组件（清空上一轮的输入）。
          // kind 也要进 key：私钥口令与紧随其后的 OTP 轮次都是 1，
          // 少这一项会让 OTP 框里残留刚才的口令。
          key={`${authPrompt.sessionId}-${authPrompt.kind}-${authPrompt.round}`}
          prompt={authPrompt}
          reconnecting={authPromptReconnecting}
          queued={authPrompts.length - 1}
          onSubmit={(answers, remember) =>
            resolveAuthPrompt(authPrompt.sessionId, answers, remember)
          }
          onCancel={() => resolveAuthPrompt(authPrompt.sessionId, null)}
        />
      )}

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
