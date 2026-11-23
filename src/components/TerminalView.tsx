import { useCallback, useEffect, useRef, useState } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { CanvasAddon } from "@xterm/addon-canvas";
import { Unicode11Addon } from "@xterm/addon-unicode11";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { SearchAddon } from "@xterm/addon-search";
import "@xterm/xterm/css/xterm.css";
import { countOutput, emitInput, markOutput, registerTerm } from "../lib/termRegistry";
import {
  formatCombo,
  getPrefs,
  matchShortcut,
  useShortcuts,
  type ShortcutAction,
} from "../lib/prefs";
import { Highlighter, MARK_COLORS, bufferText, type MarkRule } from "../lib/highlight";
import { parseProbe } from "../lib/context";
import { getExtras, isSidecarDenied, markCompat } from "../lib/sessionExtras";
import { viewableKind } from "../lib/viewable";
import {
  dropCompletion,
  feedKeys,
  historyItems,
  loadHistory,
  newLineState,
  recordCommand,
} from "../lib/complete";
import { cursorFollowsTyped } from "../lib/echo";
import { ImeGuard, type KeyLike } from "../lib/ime";
import { buildUserCommandReport } from "../lib/userCommand";
import { captureLineStart, readLogicalLine, recorderTapAction, type LineStart } from "../lib/termLine";
import { isRecording, watchRecording } from "../lib/useRecorder";
import { createControlGate, lockedInput } from "../lib/control";

import {
  api,
  onAgentControl,
  onLocalClosed,
  onLocalData,
  onTermClosed,
  onTermData,
  bytesToBase64,
  stringToBytes,
} from "../api";
import { themeToXterm } from "../lib/theme";
import { isMacOS } from "../lib/platform";
import { installMacKeyboard, setInputTrace, traceSend } from "../lib/macInput";
import { SHIFT_ENTER_SEQ, lineHasShellPrompt, shouldSendMetaEnter, textBeforeCursor, wantsMetaEnter } from "../lib/shiftEnter";
import { replaceLineInput } from "../lib/historyPanel";
import HistoryPanel from "./HistoryPanel";
import { isSftpCommand } from "../lib/sftpCmd";
import { opensLink } from "../lib/links";
import { attachZmodem, type ZmodemGate } from "../lib/zmodem";
import type { ClosedInfo, TerminalTab, Theme } from "../types";

/** HH:MM:SS，本地时区 */
export function clock(ts: number): string {
  const d = new Date(ts);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

/** xterm.js 原生 Alt+Enter 的字节序列（ESC + CR），软换行沿用它 */
const SOFT_NEWLINE = "\x1b\r";

/**
 * 排查 mac 中文输入法用的两个开关（存 localStorage，重启保持）。
 *
 * 输入法那几类症状（丢字 / 多空格 / 选候选多一个数字 / 标点重复）在别人机器上
 * 只闪一下，靠口头描述连着猜错了三轮，所以：① 记真值（见 `lib/macInput` 的
 * trace）；② 接管层能当场关掉 —— 同一次操作在开 / 关两态的差别，直接说明问题
 * 出在我们这层还是 xterm 那层。
 *
 * - `nx.inputTrace` = "1"：开始记录（默认关 —— 终端里敲的东西含密码，不该默认落盘）
 * - `nx.macInput`   = "0"：关掉 mac 输入接管层
 *
 * 快捷键：⌘⌃⌥L 切日志，⌘⌃⌥K 切接管层。
 */
const LS_INPUT_TRACE = "nx.inputTrace";
const LS_MAC_INPUT = "nx.macInput";
const macLayerOn = (): boolean => localStorage.getItem(LS_MAC_INPUT) !== "0";

/**
 * 路径由后端决定（`<app_data>/logs/input.log`）；前端拿不到 home
 * （`@tauri-apps/api/path` 依赖没装的 path 插件），这里只是给人看的说明。
 */
const INPUT_LOG_HINT = "~/Library/Application Support/com.nexus-term.app/logs/input.log";

/**
 * 光标是否被程序关掉了（DECTCEM `CSI ?25l`）。公开 API 没有这个状态，只能读内部；
 * 结构变了就当「没关」—— 对 Shift+Enter 判定来说那是退回只看提示符。
 */
function isCursorHidden(term: Terminal): boolean {
  try {
    const core = (term as unknown as { _core?: { coreService?: { isCursorHidden?: boolean } } })._core;
    return core?.coreService?.isCursorHidden === true;
  } catch {
    return false;
  }
}

/**
 * 让 macOS 也支持按住 Shift 拖选文本（绕过 tmux 等程序的鼠标捕获）。
 *
 * xterm.js 的 `SelectionService.shouldForceSelection()` 按平台分叉：mac 上只认
 * `altKey && macOptionClickForcesSelection`，Shift 根本不算绕过键，于是同一份
 * `set -g mouse on` 的 tmux 配置在 Windows / Linux 上 Shift+拖选可用、mac 上不行。
 *
 * 官方没给 mac 开 Shift 的选项（attachCustomMouseEventHandler 在 5.x 已移除），
 * 只能对实例方法做特性检测式补丁：拿不到就静默退回官方行为。
 */
function patchShiftSelection(term: Terminal): void {
  if (!isMacOS()) return;
  try {
    type SelSvc = {
      shouldForceSelection?: (ev: MouseEvent) => boolean;
    };
    const core = (term as unknown as { _core?: { _selectionService?: SelSvc } })
      ._core;
    const svc = core?._selectionService;
    if (typeof svc?.shouldForceSelection !== "function") return;
    const original = svc.shouldForceSelection!.bind(svc);
    svc.shouldForceSelection = (ev: MouseEvent) => ev.shiftKey || original(ev);
  } catch {
    // 内部结构变了就放弃补丁，退回官方行为
  }
}

interface Props {
  tabId: string;
  active: boolean;
  /** ssh = 远端会话；local = 本机 PTY。两者共用同一套渲染与事件协议 */
  kind?: "ssh" | "local";
  /**
   * 补全历史按「会话身份」存取，不是 tabId：本地 shell 固定 `"local"`，
   * 已保存的 SSH 连接是它的 `SessionRecord.id`，同一身份的标签共用一份内存历史。
   * 没有保存记录的临时连接，调用方应该传 `tabId` 本身当 key：关掉标签就忘。
   */
  historyKey: string;
  /** `historyKey` 是否是多个标签共用的身份（本地 / 已保存会话为 true，临时连接为 false） */
  historyPersist: boolean;
  theme: Theme;
  fontFamily: string;
  fontSize: number;
  lineHeight: number;
  onClosed?: (info: ClosedInfo) => void;
  /** 远端主机地址（断开提示用），本地终端为空 */
  peerHost?: string;
  /** 标签状态 —— 用于在终端里打印重连进度 */
  status?: TerminalTab["status"];
  reconnectAttempt?: number;
  reconnectMax?: number;
  /** ZMODEM 传输结果等轻提示 */
  onNotify?: (msg: string) => void;
  /** 断开当前标签页 */
  onDisconnect?: () => void;
  /** 打开 SFTP 文件管理器（仅 SSH） */
  onOpenSftp?: () => void;
  /** 一行命令被当成真的 shell 命令提交了（行有回显、不在全屏程序里）。本地与 SSH 都会报 */
  onShellCommand?: (line: string) => void;
  /** 不在前台时收到新输出——标签栏据此点亮提醒动效。只在非活跃时调用。 */
  onBackgroundOutput?: () => void;
}

/** 终端区域右键菜单状态 */
interface TermCtxMenu {
  x: number;
  y: number;
  /** 右键时的选区（菜单打开后选区可能被改，先存下来） */
  sel: string;
  /** 「浏览」菜单项真正要用的目标：有能解析成文件的选区就用选区，
   *  否则退化成鼠标落点所在的那个词（见 `wordAtPoint`） */
  target: string;
}

/**
 * 选区 / 词 → 可浏览的文件路径（md / html / 图片 / 音视频 / 文本配置）；否则返回 null。
 *
 * @param requireExt 鼠标落点自动取词（没有用户明确选中）时传 true：提示符那种整行
 *   没有空格的文本会被 `wordAtPoint` 当成一整个词，若再放行「没扩展名当纯文本」
 *   的兜底，就会把 `user@host:~/path$` 这种东西当成文件去 `cat`，拼出一个根本
 *   不存在的路径。真正选中文字是用户的明确意图，不受此限制。
 */
function viewablePath(sel: string, requireExt?: boolean): string | null {
  let s = sel.trim();
  if (!s || s.includes("\n")) return null;
  s = s.replace(/^['"`]+|['"`,;:]+$/g, ""); // 去掉 ls / 日志里常见的引号与尾随标点
  s = s.replace(/^file:\/\//i, "");
  return viewableKind(s, { requireExt }) ? s : null;
}

/**
 * 鼠标落点所在的「词」——右键浏览不再要求先完整选中文件名，停在上面点右键就行。
 *
 * 已知局限：按「1 字符 = 1 列」换算，行里有宽字符（中文等）会让列位置错位，
 * 命中可能不准；只按空白切词，文件名中间带空格仍需手动选中。
 */
function wordAtPoint(term: Terminal, host: HTMLElement, clientX: number, clientY: number): string | null {
  const r = host.getBoundingClientRect();
  if (term.cols <= 0 || term.rows <= 0 || r.width <= 0 || r.height <= 0) return null;
  const col = Math.floor(((clientX - r.left) / r.width) * term.cols);
  const row = Math.floor(((clientY - r.top) / r.height) * term.rows);
  if (col < 0 || row < 0 || col >= term.cols || row >= term.rows) return null;
  const buf = term.buffer.active;
  const line = buf.getLine(buf.viewportY + row);
  if (!line) return null;
  const text = line.translateToString(true);
  if (col >= text.length || /\s/.test(text[col])) return null;
  const isBoundary = (c: string) => /\s/.test(c) || "()[]{}<>".includes(c);
  let start = col;
  let end = col;
  while (start > 0 && !isBoundary(text[start - 1])) start--;
  while (end < text.length - 1 && !isBoundary(text[end + 1])) end++;
  const word = text.slice(start, end + 1);
  return word || null;
}

function debounce<A extends unknown[]>(fn: (...a: A) => void, ms: number) {
  let t: number | undefined;
  return (...a: A) => {
    if (t) window.clearTimeout(t);
    t = window.setTimeout(() => fn(...a), ms);
  };
}

/**
 * 回车提交的整行推给后端 side buffer（agent 时间线 / 录制器的数据源）。
 * 只在回车时调一次，fire-and-forget；无回显的行只发「无回显」标记，不发内容。
 */
function pushUserCommand(sessionId: string, committed: string, echoed: boolean, altScreen: boolean): void {
  const r = buildUserCommandReport(committed, echoed, altScreen);
  if (!r) return;
  void api.agentUserCommand(sessionId, r.line, r.noEcho).catch(() => {});
}

/**
 * 录制中（3.2）补报行状态机没能交出的操作：Ctrl-C 等按键、空回车、↑ 历史 / Tab 补全出来的整行。
 * 调用方先问 `isRecording`，没在录制时这里一次都不会走到。
 */
function tapRecorder(
  sessionId: string,
  term: Terminal,
  data: string,
  committed: string | null,
  wasKnown: boolean,
  start: LineStart | null,
): void {
  const buf = term.buffer.active;
  const act = recorderTapAction({
    data,
    committed,
    wasKnown,
    altScreen: buf.type !== "normal",
    recover: () => readLogicalLine(buf, start),
  });
  if (!act) return;
  if (act.kind === "keys") void api.agentUserKeys(sessionId, act.keys).catch(() => {});
  else pushUserCommand(sessionId, act.line, true, false);
}

export default function TerminalView({
  tabId,
  active,
  kind = "ssh",
  historyKey,
  historyPersist,
  theme,
  fontFamily,
  fontSize,
  lineHeight,
  onClosed,
  onNotify,
  onDisconnect,
  onOpenSftp,
  onShellCommand,
  onBackgroundOutput,
  peerHost,
  status,
  reconnectAttempt,
  reconnectMax,
}: Props) {
  // 事件回调在 mount 时注册一次，用 ref 拿最新的 props
  const onClosedRef = useRef(onClosed);
  onClosedRef.current = onClosed;
  const onNotifyRef = useRef(onNotify);
  onNotifyRef.current = onNotify;
  /** 由终端 mount 时填好：⌘⌃⌥K / ⌘⌃⌥L 两个调试开关的实际动作 */
  const toggleMacInputRef = useRef<(() => void) | null>(null);
  const toggleInputTraceRef = useRef<(() => void) | null>(null);
  const peerHostRef = useRef(peerHost);
  peerHostRef.current = peerHost;
  const onBackgroundOutputRef = useRef(onBackgroundOutput);
  onBackgroundOutputRef.current = onBackgroundOutput;
  const isLocal = kind === "local";
  const hostRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const activeRef = useRef(active);
  const [ctxMenu, setCtxMenu] = useState<TermCtxMenu | null>(null);
  const searchRef = useRef<SearchAddon | null>(null);
  const hlRef = useRef<Highlighter | null>(null);
  const [findOpen, setFindOpen] = useState(false);
  /** AI 正在驾驶这个会话：只读（见 `lib/control` 的 `lockedInput`） */
  const [aiLocked, setAiLocked] = useState(false);
  /** 锁住时被丢掉的按键计数：变化时横幅闪一下，告诉用户「按键没进去」 */
  const [lockNudge, setLockNudge] = useState(0);
  const [markOpen, setMarkOpen] = useState(false);
  /** Shift+Tab 历史面板：打开时的历史快照、预填的关键字、相对终端容器的位置 */
  const [hist, setHist] = useState<{
    items: string[];
    query: string;
    pos: { x: number; y: number; above: boolean };
  } | null>(null);
  /** mount effect 里的输入处理（记账 + 发字节 + 只读闸）桥给面板的回调 */
  const handleInputRef = useRef<(d: string) => void>(() => {});
  /** Shift+Tab：打开历史面板（键盘处理在 mount effect 里，经 ref 调到这里） */
  const openHistoryRef = useRef<() => void>(() => {});
  /** 当前生效的标记规则数（右上角徽标用） */
  const [markCount, setMarkCount] = useState(0);
  const zmodemRef = useRef<ZmodemGate | null>(null);
  /** 外部文件拖入终端时的高亮遮罩 */
  const [dropHint, setDropHint] = useState(false);
  const dragDepth = useRef(0);
  const statusRef = useRef(status);
  statusRef.current = status;

  // ——— 输入法排查开关（见上面的常量说明）———
  useEffect(() => {
    let alive = true;
    // 默认关：终端里敲的东西包含密码，没有明确开记录就不该往磁盘上落
    let on = localStorage.getItem(LS_INPUT_TRACE) === "1";
    const apply = (next: boolean) => {
      on = next;
      localStorage.setItem(LS_INPUT_TRACE, next ? "1" : "0");
      setInputTrace(next, true);
      if (alive) {
        onNotifyRef.current?.(
          next
            ? `正在记录输入日志 → ${INPUT_LOG_HINT}；再按 ⌘⌃⌥L 停止`
            : `输入日志已停止（文件：${INPUT_LOG_HINT}）`
        );
      }
    };
    toggleInputTraceRef.current = () => apply(!on);
    if (on) apply(true);
    return () => {
      alive = false;
      setInputTrace(false);
    };
  }, []);

  // 两个开关都用 ⌘⌃⌥ 打头（⌃ 是 Control，不是 Shift —— 文案写错过一次，
  // 照着按不动）：⌥ 会把 e.key 变成 ¬ ˚ 之类，只能按物理键 e.code 判
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!e.metaKey || !e.ctrlKey || !e.altKey) return;
      if (e.code === "KeyL") {
        e.preventDefault();
        e.stopPropagation();
        toggleInputTraceRef.current?.();
      } else if (e.code === "KeyK") {
        e.preventDefault();
        e.stopPropagation();
        toggleMacInputRef.current?.();
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, []);

  activeRef.current = active;

  // ——— Console 自动补全 ———
  //
  // 补全**只负责给建议**，从不自己写 pty。
  // 真正落到终端的只有两处：用户按 Tab 接受的那一段，以及用户自己敲的每一个键。
  const historyKeyRef = useRef(historyKey);
  historyKeyRef.current = historyKey;
  const historyPersistRef = useRef(historyPersist);
  historyPersistRef.current = historyPersist;
  const lineRef = useRef(newLineState());
  /**
   * 这一行有没有真的被回显过 —— 当「是不是密码」的判据：密码输入时远端关掉回显，
   * `cursorFollowsTyped` 永远匹配不上；回了车却从没见过回显就不记进历史。
   */
  const lineEchoedRef = useRef(false);
  /** 录制用：当前这行输入的起点（提示符之后）。↑ 历史 / Tab 补全后靠它把整行从屏幕读回来 */
  const lineStartRef = useRef<LineStart | null>(null);
  // 录制状态由后端事件驱动（面板可能浮出在另一个窗口里按的「开始录制」），终端只需同步问 `isRecording`
  useEffect(() => watchRecording(tabId), [tabId]);
  const onOpenSftpRef = useRef(onOpenSftp);
  onOpenSftpRef.current = onOpenSftp;
  const onShellCommandRef = useRef(onShellCommand);
  onShellCommandRef.current = onShellCommand;
  const sftpTimerRef = useRef<number | undefined>(undefined);
  useEffect(() => () => window.clearTimeout(sftpTimerRef.current), []);
  /**
   * 一行命令被当成「真的 shell 命令」提交之后的统一出口（与进历史同一个判定：
   * 行有回显、不在全屏程序里）。目前只做一件事：SSH 标签里敲了 `sftp ...` 就
   * 稍后打开 / 同步 SFTP 面板。命令本身照常发给远端，这里不拦。
   */
  const onCommandCommitted = useCallback((line: string) => {
    onShellCommandRef.current?.(line);
    if (kind !== "ssh" || statusRef.current !== "connected") return;
    if (!isSftpCommand(line)) return;
    // 延后一点：别和回车的回显抢；面板内部会再探测 cwd
    window.clearTimeout(sftpTimerRef.current);
    sftpTimerRef.current = window.setTimeout(() => {
      if (statusRef.current === "connected") onOpenSftpRef.current?.();
    }, 300);
  }, [kind]);
  /** 面板大小（与样式 `.hist-panel` 一致），用来决定放在光标下方还是上方、别超出右边 */
  const HIST_W = 560;
  const HIST_H = 330;

  /** Shift+Tab：在光标附近打开历史面板，用命令行上已敲的字预筛 */
  openHistoryRef.current = () => {
    const term = termRef.current;
    const host = hostRef.current;
    const screen = term?.element?.querySelector<HTMLElement>(".xterm-screen");
    if (!term || !host || !screen) return;
    const box = (host.offsetParent as HTMLElement | null) ?? host;
    const r = screen.getBoundingClientRect();
    const pr = box.getBoundingClientRect();
    const cellW = r.width / Math.max(term.cols, 1);
    const cellH = r.height / Math.max(term.rows, 1);
    const buf = term.buffer.active;
    const rowTop = r.top - pr.top + buf.cursorY * cellH;
    const below = rowTop + cellH;
    const above = below + HIST_H > pr.height && rowTop > HIST_H;
    const x = Math.max(4, Math.min(r.left - pr.left + buf.cursorX * cellW, pr.width - HIST_W - 4));
    setHist({
      items: historyItems(historyKeyRef.current),
      query: lineRef.current.known ? lineRef.current.text.trimStart() : "",
      pos: above ? { x, y: pr.height - rowTop, above: true } : { x, y: below, above: false },
    });
  };

  // 切走标签就收起历史面板（它属于这个终端的光标位置）
  useEffect(() => {
    if (!active) setHist(null);
  }, [active]);

  /** 关面板，焦点还给终端 */
  const closeHistory = useCallback(() => {
    setHist(null);
    termRef.current?.focus();
  }, []);

  /** 选中一条：换掉命令行上的内容（不回车，跑不跑由人决定），行状态记成这条 */
  const pickHistory = useCallback((cmd: string) => {
    setHist(null);
    const line = lineRef.current;
    handleInputRef.current(replaceLineInput(line.known ? line.text : null, cmd));
    // 不管之前知不知道行里是什么，换完之后就是这一条
    lineRef.current = { text: cmd, known: true, pasting: false };
    termRef.current?.focus();
  }, []);

  const onCommandCommittedRef = useRef(onCommandCommitted);
  onCommandCommittedRef.current = onCommandCommitted;
  /** 写 pty 的那条路定义在 mount 的 effect 里，用 ref 桥给下面的回调 */
  const sendRef = useRef<(d: string) => void>(() => {});
  /**
   * 输入法状态。接管回车之前必须问它一句：拼音组合里按回车是「上屏」，
   * 不是「执行」 —— 吞掉就打不出中文了。判定逻辑见 `lib/ime.ts`。
   */
  const imeRef = useRef<ImeGuard>(new ImeGuard());

  /** 把当前行列数同步给后端（SSH 是 window_change，本地是 PTY resize） */
  const syncSize = useCallback(() => {
    const t = termRef.current;
    if (!t) return;
    const push = kind === "local"
      ? api.localResize(tabId, t.cols, t.rows)
      : api.sshResize(tabId, t.cols, t.rows);
    push.catch(() => {});
  }, [kind, tabId]);

  // mount 时创建的闭包要拿到最新的 syncSize，用 ref 桥接
  const syncSizeRef = useRef(syncSize);
  syncSizeRef.current = syncSize;

  // —— 生命周期：整个 tab 存续期间只创建一次 Terminal ——
  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;

    // 本地 shell 的历史文件现在就能读；远端要等连接建立（见下面按 status 触发的 effect）。
    if (isLocal) void loadHistory(historyKey, api.historyLoadLocal).catch(() => {});

    const term = new Terminal({
      fontFamily,
      fontSize,
      lineHeight,
      cursorBlink: true,
      cursorStyle: "bar",
      allowProposedApi: true,
      scrollback: 10_000,
      theme: themeToXterm(theme),
      macOptionIsMeta: true,
      // macOS 官方约定的绕过键是 ⌥（配合 macOptionIsMeta 使用）。
      // 开启后 ⌥+拖选可用；Shift 由下面的补丁补上，与 Windows 手感对齐。
      macOptionClickForcesSelection: true,
    });

    const fit = new FitAddon();
    term.loadAddon(fit);
    term.loadAddon(new CanvasAddon());
    const unicode = new Unicode11Addon();
    term.loadAddon(unicode);
    term.unicode.activeVersion = "11"; // 中文 / emoji 宽度必须启用
    // 终端里的链接：Ctrl+点击（macOS 为 ⌘+点击）用系统浏览器打开；单纯点击不打开，免得点一下选文字就跳网页。
    // 悬停时在终端区域显示提示（xterm 自己会给链接加下划线）
    term.loadAddon(
      new WebLinksAddon(
        (event, uri) => {
          if (!opensLink(event, isMacOS())) return;
          event.preventDefault();
          void import("@tauri-apps/plugin-opener")
            .then(({ openUrl }) => openUrl(uri))
            .catch(() => {});
        },
        {
          hover: () => {
            if (host) host.title = isMacOS() ? "⌘+点击 在浏览器中打开" : "Ctrl+点击 在浏览器中打开";
          },
          leave: () => {
            if (host) host.title = "";
          },
        }
      )
    );
    const search = new SearchAddon({ highlightLimit: 2000 });
    term.loadAddon(search);
    searchRef.current = search;

    term.open(host);
    // 必须在 open() 之后：SelectionService 是 open() 里才创建的
    patchShiftSelection(term);
    termRef.current = term;
    fitRef.current = fit;
    const hl = new Highlighter(term);
    hlRef.current = hl;

    let disposed = false;
    const cleanups: Array<() => void> = [registerTerm(tabId, term)];
    // 新输出写完后：记录活跃时间（还原命令等提示符用），并重扫标记高亮
    const parsedSub = term.onWriteParsed(() => {
      markOutput(tabId);
      // 不在前台时才需要点亮标签——自己正盯着看的标签没必要提醒自己
      if (!activeRef.current) onBackgroundOutputRef.current?.();
      hl.refresh();
    });
    // 选中即复制：只在「鼠标在本终端按下 → 松开」时触发。
    // 不用 onSelectionChange：查找跳转、全选快捷键也会改选区，会意外覆盖剪贴板。
    // mouseup 挂 window：拖选到终端外再松手也要算。
    let selPressed = false;
    const onSelDown = (e: MouseEvent) => {
      selPressed = e.button === 0;
    };
    const onSelUp = (e: MouseEvent) => {
      if (!selPressed || e.button !== 0) return;
      selPressed = false;
      if (!getPrefs().copyOnSelect) return;
      // 等 xterm 自己的 mouseup 处理完、选区定稿
      setTimeout(() => {
        if (disposed || !term.hasSelection()) return;
        const sel = term.getSelection();
        if (sel) api.clipboardWrite(sel).catch(() => {});
      }, 0);
    };
    host.addEventListener("mousedown", onSelDown, true);
    window.addEventListener("mouseup", onSelUp, true);
    // 输入法：只旁观，记下组合起止供键盘处理器判断回车该不该被补全接管
    const onImeStart = (e: CompositionEvent) => imeRef.current.begin(e.timeStamp);
    // 组合是否还活着的心跳，见 `lib/ime.ts` 的 `STALE_COMPOSE_MS`——没有它，
    // 卡死的 compositionend 就永远等不到人清理
    const onImeUpdate = (e: CompositionEvent) => imeRef.current.touch(e.timeStamp);
    const onImeEnd = (e: CompositionEvent) => imeRef.current.finish(e.timeStamp);
    // 组合中被切走时 compositionend 不一定来，留着 composing=true 会把回车全吞掉
    const onImeReset = () => imeRef.current.reset();
    host.addEventListener("compositionstart", onImeStart, true);
    host.addEventListener("compositionupdate", onImeUpdate, true);
    host.addEventListener("compositionend", onImeEnd, true);
    window.addEventListener("blur", onImeReset);
    cleanups.push(() => {
      host.removeEventListener("mousedown", onSelDown, true);
      window.removeEventListener("mouseup", onSelUp, true);
      host.removeEventListener("compositionstart", onImeStart, true);
      host.removeEventListener("compositionupdate", onImeUpdate, true);
      host.removeEventListener("compositionend", onImeEnd, true);
      window.removeEventListener("blur", onImeReset);
    });
    const resizeSub = term.onResize(() => hl.refresh());
    const bufSub = term.buffer.onBufferChange(() => hl.refresh());
    cleanups.push(
      () => parsedSub.dispose(),
      () => resizeSub.dispose(),
      () => bufSub.dispose(),
      () => hl.dispose()
    );

    const writeOut = (data: string | Uint8Array) => {
      term.write(data);
      // 这行是不是真被回显过。这里当安全边界用，不能按 `cursorFollowsTyped`
      // 默认的「1 个字符就算」：密码首字符撞上提示符末尾的巧合匹配，就会把没
      // 回显的密码记进历史。至少连续匹配 3 个字符才采信。
      if (
        lineRef.current.text &&
        cursorFollowsTyped(term, lineRef.current.text, Math.min(3, lineRef.current.text.length))
      ) {
        lineEchoedRef.current = true;
      }
    };

    // —— ZMODEM（lrzsz）：远端 rz/sz 的字节流先过 Sentry 嗅探 ——
    const zmodem = attachZmodem({
      termWrite: writeOut,
      termWriteBytes: (b) => {
        countOutput(tabId, b);
        writeOut(b);
      },
      send: (bytes) => {
        const b64 = bytesToBase64(bytes);
        return isLocal ? api.localWriteB64(tabId, b64) : api.sshWriteB64(tabId, b64);
      },
      notify: onNotify ?? (() => {}),
    });
    zmodemRef.current = zmodem;

    // 输出 → 终端（SSH 与本地 PTY 协议一致，只是事件名前缀不同）
    const dataFeed = isLocal ? onLocalData : onTermData;
    dataFeed(tabId, (bytes) => {
      if (!disposed) zmodem.consume(bytes);
    }).then((u) => cleanups.push(u));

    if (isLocal) {
      onLocalClosed(tabId, (code) => {
        if (disposed) return;
        writeOut(`\r\n\x1b[90m[Shell 已退出，退出码 ${code}]\x1b[0m\r\n`);
        onClosedRef.current?.({ code, reason: "exit" });
      }).then((u) => cleanups.push(u));
    } else {
      onTermClosed(tabId, (info) => {
        if (disposed || info.reason === "user") return;
        // Xshell 风格：先说明原因，再给出带时间的断开行，方便事后判断何时掉线
        const host = peerHostRef.current ?? "remote host";
        const why =
          info.reason === "lost"
            ? "Connection lost (network unreachable or keepalive timeout)."
            : info.reason === "exit"
              ? `Connection closed. (exit status ${info.code})`
              : "Connection closed by foreign host.";
        writeOut(
          `\r\n\r\n\x1b[33m${why}\x1b[0m\r\n\r\n` +
            `\x1b[1;33mDisconnected from remote host(${host}) at ${clock(Date.now())}.\x1b[0m\r\n`
        );
        onClosedRef.current?.(info);
      }).then((u) => cleanups.push(u));
    }

    // 键盘输入 → 远端 / 本地 PTY（ZMODEM 会话进行中时忽略，出站被协议帧占用）
    const send = (d: string) =>
      isLocal
        ? api.localWrite(tabId, stringToBytes(d))
        : api.sshWrite(tabId, stringToBytes(d));
    sendRef.current = send;

    // ——— Agent 控制权闸 ———
    // 平时（没有 Agent 在驾驶这个终端）闸是关的：每个键只多一次布尔判断，不发 IPC。
    // Agent 驾驶时闸开着，用户第一次真的打字就通知后端一次（→ Paused），随即闸关。
    // 字节本身永远先发给 pty，闸只旁观，不拦、不延迟。
    const controlGate = createControlGate(() => {
      api.agentControlUserInput(tabId).catch(() => {});
    });
    let sawControlEvent = false;
    onAgentControl(tabId, (p) => {
      // 同一频道也会收到「以本会话为发起方的运行」驾驶别的会话的状态，只认自己的
      if (disposed || p.session !== tabId) return;
      sawControlEvent = true;
      controlGate.set(p.state);
      setAiLocked(p.state === "agentDriving");
    }).then((u) => cleanups.push(u));
    // 挂载时对一次账（比如终端是在 Agent 已经在跑时才重建的）；期间若已有事件到达，以事件为准
    api
      .agentControlGet(tabId)
      .then((st) => {
        if (!disposed && !sawControlEvent) {
          controlGate.set(st);
          setAiLocked(st === "agentDriving");
        }
      })
      .catch(() => {});

    /**
     * 所有「用户敲进来的字节」的唯一出口。
     *
     * 两个调用方：`term.onData`（回车、Ctrl 组合、方向键、输入法上屏、粘贴）
     * 和 `lib/macInput` 的接管层（mac 上的普通字符 —— xterm 自己那条路会丢字）。
     * 两者共用这一份逻辑，宏录制与补全行状态才不会漏掉其中一半。
     */
    const handleInput = (d: string) => {
      if (zmodem.isActive()) return;
      // AI 在驾驶这个会话：只读。打字 / 粘贴一律丢掉（误触会把 AI 挤成「你在驾驶」）；
      // Ctrl+C 是唯一的键盘出口：立即接管，再把 ^C 发出去中断正在跑的命令
      const verdict = lockedInput(controlGate.armed, d);
      if (verdict === "drop") {
        setLockNudge((n) => n + 1);
        return;
      }
      if (verdict === "interrupt") {
        controlGate.set("paused");
        setAiLocked(false);
        api.agentControlTakeover(tabId).catch(() => {});
      }
      // 旁路监听（宏录制、宏回放时「用户插了一脚就中止」）。发不发送由这里决定，监听方只读
      emitInput(tabId, d);
      send(d).catch(() => {});
      // 用户键入 → 若 Agent 正在驾驶则让它暂停（字节已发出，这里只通知，不拦）
      controlGate.onInput(d);
      // 新行开始要清掉上一行的回显标记，否则密码行的「已回显过」旧状态会把
      // 紧接着敲的命令蹭进历史
      if (!lineRef.current.text) lineEchoedRef.current = false;
      // 录制中：新一行的第一个键到来时屏幕光标还停在提示符末尾，记下它作为「这行输入的起点」
      const recording = isRecording(tabId);
      const wasKnown = lineRef.current.known;
      if (recording && wasKnown && !lineRef.current.text) lineStartRef.current = captureLineStart(term.buffer.active);
      // 喂键攒行（同步、纯字符串运算），候选留到下一帧再算
      const committed = feedKeys(lineRef.current, d);
      if (committed) {
        // 全屏程序（vim / less）里敲的不是 shell 命令，不进历史
        if (lineEchoedRef.current && term.buffer.active.type === "normal") {
          recordCommand(historyKeyRef.current, committed);
          onCommandCommittedRef.current(committed);
        }
        pushUserCommand(tabId, committed, lineEchoedRef.current, term.buffer.active.type !== "normal");
        lineEchoedRef.current = false;
      }
      if (recording) tapRecorder(tabId, term, d, committed, wasKnown, lineStartRef.current);
    };
    // 历史面板选中一条时也走这里（只读闸、宏录制、Agent 控制权都照常）
    handleInputRef.current = handleInput;

    // macOS(WKWebView) 上普通字符改由我们在捕获阶段发 —— 详见 lib/macInput。
    // 必须挂在 host 上：xterm 的监听在 textarea 上，捕获阶段我们先到。
    // 包在可重装的壳里是为了 ⌘⌃⌥K 能当场开关对比，不必重建终端。
    let macUninstall: () => void = () => {};
    /** 接管层开着时，xterm 自己要发的字节先过一道闸，见 macInput 的 OnceGuard */
    let macArbitrate: (d: string) => boolean = () => true;

    const dataSub = term.onData((d) => {
      // 同一颗键 xterm 和接管层可能各发一份（WKWebView 的事件序列按输入法而变），
      // 这里是两条路唯一的汇合点，重复只能在这儿掐。
      if (!macArbitrate(d)) return;
      // 记 via 是为了在日志里把「我们发的」和「xterm 发的」分开 —— 标点重复
      // 这类问题只有看到 via 才知道多出来的那一下是谁干的
      traceSend(d, "xterm");
      handleInput(d);
    });

    const applyMacInput = () => {
      macUninstall();
      if (macLayerOn()) {
        const mac = installMacKeyboard(host, term, handleInput, isMacOS());
        macUninstall = mac.uninstall;
        macArbitrate = mac.arbitrate;
      } else {
        macUninstall = () => {};
        macArbitrate = () => true;
      }
    };
    applyMacInput();
    cleanups.push(() => macUninstall());
    toggleMacInputRef.current = () => {
      const next = !macLayerOn();
      localStorage.setItem(LS_MAC_INPUT, next ? "1" : "0");
      applyMacInput();
      onNotifyRef.current?.(
        next ? "已开启 mac 输入接管层（⌘⌃⌥K 可再关）" : "已关闭 mac 输入接管层，走 xterm 原生路径（⌘⌃⌥K 恢复）"
      );
    };

    // 快捷键全部来自「设置 → 快捷键」（lib/prefs），改了立即生效。
    // 默认 Windows / Linux 用 Ctrl+Shift+C/V/F：Ctrl+C 必须留给中断信号（^C）。
    term.attachCustomKeyEventHandler((e) => {
      if (e.type !== "keydown") return true;

      // Shift+Enter → `ESC \r`（Meta+Enter），让 Claude Code / nanopi 这类 TUI 换行而不是提交；
      // shell 提示符、vim / less 等备用屏程序里保持原样（当回车）。判定见 `lib/shiftEnter`。
      if (e.key === "Enter" && e.shiftKey) {
        const imeBusy = imeRef.current.captures({
          key: e.key,
          shiftKey: e.shiftKey,
          isComposing: e.isComposing,
          keyCode: e.keyCode ?? 0,
          timeStamp: e.timeStamp,
        });
        const metaEnter = shouldSendMetaEnter(
          { key: e.key, shiftKey: e.shiftKey, ctrlKey: e.ctrlKey, altKey: e.altKey, metaKey: e.metaKey, imeBusy },
          () =>
            wantsMetaEnter({
              before: textBeforeCursor(term.buffer.active),
              cursorHidden: isCursorHidden(term),
              altScreen: term.buffer.active.type !== "normal",
            })
        );
        if (metaEnter) {
          e.preventDefault();
          handleInput(SHIFT_ENTER_SEQ);
          return false;
        }
      }

      // Shift+Tab：在 shell 提示符上打开历史面板（按命令行上已敲的字筛）。
      // 只在确实停在 shell 提示符上时接管 —— Claude Code 用 Shift+Tab 切模式、zsh 用它反向补全、
      // vim 等全屏程序也有自己的用法，这些场合原样交给程序。AI 驾驶中、输入法组合中也不接管。
      if (e.key === "Tab" && e.shiftKey && !e.ctrlKey && !e.altKey && !e.metaKey) {
        const info: KeyLike = {
          key: e.key,
          shiftKey: e.shiftKey,
          isComposing: e.isComposing,
          keyCode: e.keyCode ?? 0,
          timeStamp: e.timeStamp,
        };
        if (
          !controlGate.armed &&
          !imeRef.current.captures(info) &&
          !isCursorHidden(term) &&
          lineHasShellPrompt(textBeforeCursor(term.buffer.active))
        ) {
          e.preventDefault();
          openHistoryRef.current();
          return false;
        }
      }

      const action = matchShortcut(e);
      if (!action) return true;
      e.preventDefault();
      shortcutRef.current(action);
      return false;
    });

    // 二进制粘贴（避免中转会破坏字节）
    const binSub = term.onBinary((d) => {
      if (zmodem.isActive()) return;
      const bytes = Array.from(new Uint8Array(d.length)).map((_, i) => d.charCodeAt(i));
      (isLocal ? api.localWrite(tabId, bytes) : api.sshWrite(tabId, bytes)).catch(
        () => {}
      );
    });

    // 右键弹出终端上下文菜单（断开/复制/粘贴/全选/清屏）
    const onCtxMenu = (e: MouseEvent) => {
      e.preventDefault();
      if (zmodem.isActive()) return;
      const MENU_W = 190;
      const MENU_H = 372;
      const sel = term.getSelection();
      // 落点优先：xterm 的选区只在左键按下时清/重建，右键不碰它，所以 `sel` 很
      // 可能是和鼠标位置毫无关系的旧选区。只有落点解析不出文件时（如带空格的
      // 路径）才退回去看选区。
      const atPoint = wordAtPoint(term, host, e.clientX, e.clientY);
      const target = atPoint && viewablePath(atPoint, true) ? atPoint : viewablePath(sel) ? sel : "";
      setCtxMenu({
        x: Math.min(e.clientX, window.innerWidth - MENU_W - 8),
        y: Math.min(e.clientY, window.innerHeight - MENU_H - 8),
        sel,
        target,
      });
    };
    host.addEventListener("contextmenu", onCtxMenu);

    const doFit = () => {
      // 容器隐藏时 fit 会得到 0 列，必须跳过，否则会把错误的尺寸推给远端
      if (!host.isConnected || host.clientWidth === 0) return;
      try {
        fit.fit();
        syncSizeRef.current();
      } catch {
        /* 容器尚未布局 */
      }
    };

    // 拖拽分隔条会高频触发；必须防抖，否则会打满 IPC
    const ro = new ResizeObserver(debounce(doFit, 120));
    ro.observe(host);

    // 布局稳定后立即对齐一次 —— ResizeObserver 首次回调加 120ms 防抖太慢，
    // 首屏会先以默认 24×80 渲染，连接成功后必须马上纠正
    requestAnimationFrame(() => doFit());

    return () => {
      disposed = true;
      host.removeEventListener("contextmenu", onCtxMenu);
      zmodem.dispose();
      zmodemRef.current = null;
      ro.disconnect();
      dataSub.dispose();
      binSub.dispose();
      cleanups.forEach((f) => f());
      term.dispose();
      termRef.current = null;
      // 持久化的 key 不动：可能还有别的标签在用，数据也早落盘了
      dropCompletion(historyKeyRef.current, historyPersistRef.current);
    };
    // 字体等外观变化通过下面的 effect 单独应用，不重建实例
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tabId]);

  // —— 连上之后读一次远端 shell 的历史文件，给 Shift+Tab 历史面板当数据 ——
  // 走旁路 exec 通道：兼容模式（堡垒机）一条额外通道都不开，直接跳过；
  // 第一次被服务端拒绝时和状态栏、上下文探测一样，记下「这台只让开一条通道」。
  const remoteHistoryLoaded = useRef(false);
  useEffect(() => {
    if (isLocal || status !== "connected" || remoteHistoryLoaded.current) return;
    remoteHistoryLoaded.current = true;
    const configId = historyPersistRef.current ? historyKeyRef.current : null;
    if (configId && getExtras(configId).compat) return;
    void loadHistory(historyKeyRef.current, async () => {
      try {
        return await api.historyLoadRemote(tabId);
      } catch (e) {
        if (isSidecarDenied(e) && markCompat(configId, tabId)) {
          onNotify?.(
            "该服务器（堡垒机）不支持并行通道，已切换为兼容模式：不再采集远端资源，下次连接也不会再开额外通道"
          );
        }
        throw e;
      }
    });
  }, [status, isLocal, tabId, onNotify]);

  // —— 会话一建立就重新对齐尺寸 ——
  // 挂载时的 fit/resize 发生在 sshConnect 完成之前，后端尚无此会话会静默丢弃；
  // 若之后容器尺寸没变，ResizeObserver 也不会再触发，远端 PTY 会停留在初始尺寸。
  useEffect(() => {
    if (status !== "connected") return;
    const run = () => {
      const host = hostRef.current;
      if (!host || host.clientWidth === 0) return;
      try {
        fitRef.current?.fit();
      } catch {
        /* noop */
      }
      syncSizeRef.current();
    };
    run();
    // 再补一次：标签刚切到前台时布局可能还没稳定
    const id = window.setTimeout(run, 300);
    return () => window.clearTimeout(id);
  }, [status]);

  // —— 重连进度写进终端（留痕，方便回看什么时候断/什么时候恢复） ——
  const prevStatus = useRef(status);
  useEffect(() => {
    const term = termRef.current;
    const prev = prevStatus.current;
    prevStatus.current = status;
    if (!term || isLocal) return;
    const now = clock(Date.now());
    const out = (s: string) => term.write(s);
    if (status === "reconnecting" && reconnectAttempt) {
      out(
        `\x1b[90mReconnecting to ${peerHost ?? "remote host"} ` +
          `(attempt ${reconnectAttempt}/${reconnectMax ?? reconnectAttempt}) at ${now}...\x1b[0m\r\n`
      );
    } else if (status === "connected" && prev === "reconnecting") {
      out(`\x1b[32mReconnected to ${peerHost ?? "remote host"} at ${now}.\x1b[0m\r\n\r\n`);
    } else if (status === "closed" && prev === "reconnecting") {
      out(`\x1b[31mReconnect failed at ${now}. 右键标签页可「重新连接」。\x1b[0m\r\n`);
    }
  }, [status, reconnectAttempt, reconnectMax, peerHost, isLocal]);

  // —— 主题变更 ——
  useEffect(() => {
    const term = termRef.current;
    if (term) term.options.theme = themeToXterm(theme);
  }, [theme]);

  // —— 字体变更 ——
  useEffect(() => {
    const term = termRef.current;
    if (!term) return;
    term.options.fontFamily = fontFamily;
    term.options.fontSize = fontSize;
    // xterm 的 lineHeight 是无量纲倍数（1.2 = 1.2 倍行距），不是像素！
    // 传像素值会让每行高被放大 fontSize 倍，直接撑爆渲染。
    term.options.lineHeight = lineHeight;
    try {
      fitRef.current?.fit();
      syncSize();
    } catch {
      /* noop */
    }
  }, [fontFamily, fontSize, lineHeight, tabId]);

  // —— 切回可见时重新 fit ——
  useEffect(() => {
    if (!active) return;
    const id = window.setTimeout(() => {
      try {
        fitRef.current?.fit();
        syncSize();
        termRef.current?.focus();
      } catch {
        /* noop */
      }
    }, 30);
    return () => window.clearTimeout(id);
  }, [active, tabId]);

  // 菜单动作。剪贴板走后端（arboard）：WebView2 下 navigator.clipboard 会被静默拒绝
  const doCopy = useCallback(() => {
    const sel = termRef.current?.getSelection();
    setCtxMenu(null);
    if (sel) api.clipboardWrite(sel).catch((e) => onNotify?.(`复制失败：${String(e)}`));
    termRef.current?.focus();
  }, [onNotify]);

  const doPaste = useCallback(() => {
    setCtxMenu(null);
    api
      .clipboardRead(tabId)
      .then((t) => {
        if (!t) return;
        // 走 xterm.paste：自动处理换行规范化与 bracketed paste，和键盘粘贴行为一致
        termRef.current?.paste(t);
      })
      .catch((e) => onNotify?.(`粘贴失败：${String(e)}`));
    termRef.current?.focus();
  }, [onNotify, tabId]);

  const doSelectAll = useCallback(() => {
    termRef.current?.selectAll();
    setCtxMenu(null);
  }, []);

  const doClearScreen = useCallback(() => {
    setCtxMenu(null);
    const t = termRef.current;
    if (!t) return;
    t.clear();
    // 再让远端 shell 重绘提示符（Ctrl+L），否则清完是一片空白
    (isLocal ? api.localWrite(tabId, [0x0c]) : api.sshWrite(tabId, [0x0c])).catch(() => {});
    t.focus();
  }, [isLocal, tabId]);

  /**
   * 软换行：只换行、不执行。xterm.js 对 Enter 只判断 `altKey`，Ctrl+Enter 和
   * Enter 发的是同一个 `\r`，想改键只能在应用层拦。
   *
   * 复用 Alt+Enter 的原生字节（ESC + CR）而不是 "\n"：LF 在终端里同样是「提交」，
   * 只有 ESC 前缀能让远端 readline / tmux 认出「带修饰键的 Enter」。
   */
  const doSoftNewline = useCallback(() => {
    if (zmodemRef.current?.isActive()) return;
    const bytes = stringToBytes(SOFT_NEWLINE);
    (isLocal ? api.localWrite(tabId, bytes) : api.sshWrite(tabId, bytes)).catch(() => {});
  }, [isLocal, tabId]);

  const doDisconnect = useCallback(() => {
    setCtxMenu(null);
    onDisconnect?.();
  }, [onDisconnect]);
  /** 浏览选中（或鼠标落点）的文件：相对路径按 shell 当前目录解析 */
  const doView = useCallback(
    async (raw: string) => {
      setCtxMenu(null);
      const p = viewablePath(raw);
      if (!p) return;
      const name = p.split(/[\\/]/).pop() ?? p;
      try {
        if (isLocal) {
          const abs = /^([A-Za-z]:[\\/]|[\\/]|~)/.test(p);
          if (!abs) {
            onNotify?.("本地终端请选中完整路径（如 C:\\docs\\README.md）");
            return;
          }
          await api.viewerOpenLocal(p);
          return;
        }
        let full = p;
        if (!p.startsWith("/") && p !== "~" && !p.startsWith("~/")) {
          let ctx: ReturnType<typeof parseProbe> = null;
          let probeErr: string | undefined;
          try {
            ctx = parseProbe(await api.sshProbe(tabId));
          } catch (e) {
            probeErr = String(e);
          }
          if (!ctx?.cwd) {
            // 兼容模式（堡垒机只放行一条通道）下探测目录本来就会被拒绝，
            // 说清原因，别让人以为是 bug
            onNotify?.(
              probeErr?.includes("兼容模式")
                ? "该连接处于兼容模式（服务器不支持并行通道），无法自动获取远端目录，请选中完整路径（/… 或 ~/…）"
                : "无法确定远端当前目录，请选中完整路径（/… 或 ~/…）"
            );
            return;
          }
          full = (ctx.cwd.endsWith("/") ? ctx.cwd : ctx.cwd + "/") + p.replace(/^\.\//, "");
        }
        onNotify?.(`正在打开「${name}」…`);
        const where = peerHostRef.current ? `${peerHostRef.current}:` : "";
        await api.viewerOpenSsh(tabId, full, `${where}${full}`);
      } catch (e) {
        onNotify?.(`浏览失败：${String(e)}`);
      }
    },
    [isLocal, tabId, onNotify]
  );

  const doFind = useCallback(() => {
    setCtxMenu(null);
    setFindOpen(true);
  }, []);

  /** 快捷键分发：按键处理在 mount 时注册一次，经 ref 拿最新的动作实现 */
  const shortcutRef = useRef<(a: ShortcutAction) => void>(() => {});

  const doMark = useCallback(() => {
    setCtxMenu(null);
    setMarkOpen(true);
  }, []);

  /** 导出整个缓冲区（含滚动历史）为 UTF-8 文本 */
  const doExport = useCallback(async () => {
    setCtxMenu(null);
    const t = termRef.current;
    if (!t) return;
    const text = bufferText(t);
    const p = (n: number) => String(n).padStart(2, "0");
    const d = new Date();
    const stamp = `${d.getFullYear()}${p(d.getMonth() + 1)}${p(d.getDate())}-${p(d.getHours())}${p(d.getMinutes())}${p(d.getSeconds())}`;
    const base = (peerHost ?? (isLocal ? "local" : "session")).replace(/[\\/:*?"<>|]/g, "_");
    try {
      const { save } = await import("@tauri-apps/plugin-dialog");
      const path = await save({
        title: "导出终端输出",
        defaultPath: `${base}-${stamp}.txt`,
        filters: [
          { name: "文本文件", extensions: ["txt", "log"] },
        ],
      });
      if (!path) return;
      await api.writeFile(path, Array.from(new TextEncoder().encode(text)));
      const lines = text.split("\n").length - 1;
      onNotify?.(`已导出 ${lines} 行到 ${path}`);
    } catch (e) {
      onNotify?.(`导出失败：${String(e)}`);
    } finally {
      termRef.current?.focus();
    }
  }, [peerHost, isLocal, onNotify]);

  shortcutRef.current = (a) => {
    if (zmodemRef.current?.isActive() && a !== "copy" && a !== "find") return;
    switch (a) {
      case "disconnect":
        // 本地终端的「断开」= 关闭标签，误触代价大，快捷键只作用于 SSH
        if (!isLocal) doDisconnect();
        break;
      case "copy": {
        const sel = termRef.current?.getSelection();
        if (sel) api.clipboardWrite(sel).catch(() => {});
        break;
      }
      case "paste":
        doPaste();
        break;
      case "softNewline":
        doSoftNewline();
        break;
      case "find":
        doFind();
        break;
      case "selectAll":
        doSelectAll();
        break;
      case "clearScreen":
        doClearScreen();
        break;
      case "exportBuffer":
        doExport();
        break;
    }
  };

  // —— 外部文件拖入 = 上传（远端执行 rz，ZMODEM 握手后直接发送拖入的文件） ——
  const hasFiles = (e: React.DragEvent) => Array.from(e.dataTransfer?.types ?? []).includes("Files");
  const onDragEnter = useCallback((e: React.DragEvent) => {
    if (!hasFiles(e)) return;
    e.preventDefault();
    dragDepth.current++;
    setDropHint(true);
  }, []);
  const onDragOver = useCallback((e: React.DragEvent) => {
    if (!hasFiles(e)) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = "copy";
  }, []);
  const onDragLeave = useCallback((e: React.DragEvent) => {
    if (!hasFiles(e)) return;
    dragDepth.current = Math.max(0, dragDepth.current - 1);
    if (dragDepth.current === 0) setDropHint(false);
  }, []);
  const onDrop = useCallback(
    (e: React.DragEvent) => {
      if (!hasFiles(e)) return;
      e.preventDefault();
      dragDepth.current = 0;
      setDropHint(false);
      const files = Array.from(e.dataTransfer.files ?? []).filter((f) => f.size > 0 || f.type !== "");
      if (files.length === 0) {
        onNotify?.("没有可上传的文件（不支持直接拖入文件夹）");
        return;
      }
      if (isLocal) {
        onNotify?.("本地终端不支持拖拽上传，请在 SSH 会话中使用");
        return;
      }
      if (statusRef.current !== "connected") {
        onNotify?.("会话未连接，无法上传");
        return;
      }
      const z = zmodemRef.current;
      if (!z) return;
      if (z.isActive()) {
        onNotify?.("已有 ZMODEM 传输在进行中，请稍后再试");
        return;
      }
      z.queueUpload(files);
      // -E：远端已有同名文件时自动改名，避免覆盖也避免被拒收
      const cmd = "rz -E\r";
      (isLocal ? api.localWrite(tabId, stringToBytes(cmd)) : api.sshWrite(tabId, stringToBytes(cmd))).catch(
        (err) => onNotify?.(`发送 rz 失败：${String(err)}`)
      );
      const total = files.reduce((n, f) => n + f.size, 0);
      onNotify?.(
        `正在通过 rz 上传 ${files.length} 个文件（${(total / 1024 / 1024).toFixed(1)} MB）到远端当前目录；远端需已安装 lrzsz`
      );
      termRef.current?.focus();
    },
    [isLocal, tabId, onNotify]
  );

  const applyMarks = useCallback(
    (rules: MarkRule[], cs: boolean) => {
      const n = hlRef.current?.apply(rules, cs) ?? 0;
      setMarkCount(hlRef.current?.active.length ?? 0);
      if (rules.some((r) => r.text)) onNotify?.(`标记完成：共高亮 ${n} 处`);
      return n;
    },
    [onNotify]
  );

  const clearMarks = useCallback(() => {
    hlRef.current?.clear();
    setMarkCount(0);
  }, []);

  // 点菜单外收菜单。必须排除菜单自身：否则 mousedown 先把菜单卸载，
  // click 永远到不了按钮 —— 这正是「右键菜单一个都没作用」的根因
  const ctxRef = useRef<HTMLDivElement>(null);
  const sc = useShortcuts();
  useEffect(() => {
    if (!ctxMenu) return;
    const hide = (e: MouseEvent) => {
      if (ctxRef.current?.contains(e.target as Node)) return;
      setCtxMenu(null);
    };
    // 延一帧避免当次 contextmenu 事件又立即关掉
    const id = requestAnimationFrame(() => {
      document.addEventListener("mousedown", hide);
    });
    return () => {
      cancelAnimationFrame(id);
      document.removeEventListener("mousedown", hide);
    };
  }, [ctxMenu]);

  return (
    <>
      <div
        ref={hostRef}
        className="term-host"
        style={{ display: active ? "block" : "none" }}
        onDragEnter={onDragEnter}
        onDragOver={onDragOver}
        onDragLeave={onDragLeave}
        onDrop={onDrop}
      />
      {active && dropHint && (
        <div className="term-drop">
          <div className="term-drop-box">
            <div className="term-drop-icon">⇪</div>
            <div className="term-drop-title">松开以上传到远端当前目录</div>
            <div className="term-drop-sub">通过 rz（lrzsz / ZMODEM）传输 · 同名文件自动改名</div>
          </div>
        </div>
      )}
      {ctxMenu && active && (
        <>
          <div
            className="ctx-backdrop"
            onClick={() => setCtxMenu(null)}
            onContextMenu={(e) => { e.preventDefault(); setCtxMenu(null); }}
          />
          <div ref={ctxRef} className="ctx-menu" style={{ left: ctxMenu.x, top: ctxMenu.y }}>
            <button className="ctx-item" onClick={doDisconnect}>
              断开
              <Kbd combo={sc.disconnect} />
            </button>
            {!isLocal && (
              <button
                className="ctx-item"
                disabled={status !== "connected"}
                onClick={() => {
                  setCtxMenu(null);
                  onOpenSftp?.();
                }}
              >
                SFTP 文件管理
              </button>
            )}
            <div className="ctx-sep" />
            {(() => {
              const p = viewablePath(ctxMenu.target);
              const name = p?.split(/[\\/]/).pop();
              return (
                <button
                  className="ctx-item"
                  disabled={!p || (!isLocal && status !== "connected")}
                  title={p ? "在新窗口中渲染查看" : "鼠标移到一个可预览的文件名上再右键"}
                  onClick={() => p && void doView(ctxMenu.target)}
                >
                  <span className="ctx-ellipsis">{name ? `浏览「${name}」` : "浏览"}</span>
                </button>
              );
            })()}
            <div className="ctx-sep" />
            <button className="ctx-item" onClick={doCopy}>
              复制
              <Kbd combo={sc.copy} />
            </button>
            <button className="ctx-item" onClick={doPaste}>
              粘贴
              <Kbd combo={sc.paste} />
            </button>
            <div className="ctx-sep" />
            <button className="ctx-item" onClick={doSelectAll}>
              全选
              <Kbd combo={sc.selectAll} />
            </button>
            <button className="ctx-item" onClick={doClearScreen}>
              清屏
              <Kbd combo={sc.clearScreen} />
            </button>
            <div className="ctx-sep" />
            <button className="ctx-item" onClick={doFind}>
              查找…
              <Kbd combo={sc.find} />
            </button>
            <button className="ctx-item" onClick={doMark}>
              标记…
              {markCount > 0 && <span className="ctx-shortcut">{markCount} 条生效</span>}
            </button>
            <button className="ctx-item" onClick={doExport}>
              导出…
              <Kbd combo={sc.exportBuffer} />
            </button>
          </div>
        </>
      )}
      {active && aiLocked && (
        <div key={lockNudge} className={`ai-lock-banner${lockNudge > 0 ? " nudge" : ""}`} onMouseDown={(e) => e.stopPropagation()}>
          <span className="ai-lock-dot" />
          <span>AI 托管中 · 只读</span>
          <span className="ai-lock-hint">要动手请点「接管」· Ctrl+C 立即接管并中断</span>
          <button
            className="st-btn primary"
            onClick={() => {
              setAiLocked(false);
              api.agentControlTakeover(tabId).catch(() => {});
            }}
          >
            接管
          </button>
        </div>
      )}
      {active && findOpen && searchRef.current && (
        <FindBar
          search={searchRef.current}
          term={termRef.current}
          initial={termRef.current?.getSelection() ?? ""}
          onClose={() => {
            searchRef.current?.clearDecorations();
            setFindOpen(false);
            termRef.current?.focus();
          }}
        />
      )}
      {active && hist && (
        <HistoryPanel items={hist.items} initialQuery={hist.query} pos={hist.pos} onPick={pickHistory} onClose={closeHistory} />
      )}

      {active && markOpen && hlRef.current && (
        <MarkDialog
          initial={hlRef.current.active}
          initialCase={hlRef.current.isCaseSensitive}
          seed={termRef.current?.getSelection() ?? ""}
          onApply={applyMarks}
          onClear={clearMarks}
          onClose={() => {
            setMarkOpen(false);
            termRef.current?.focus();
          }}
        />
      )}
    </>
  );
}

/** 菜单项右侧的快捷键提示；未绑定时不显示 */
function Kbd({ combo }: { combo: string }) {
  return combo ? <span className="ctx-shortcut">{formatCombo(combo)}</span> : null;
}

// ——— 查找条 ———

/** 装饰色必须是 #RRGGBB */
const FIND_DECO = {
  matchBackground: "#5a4a10",
  matchBorder: "#c9a227",
  matchOverviewRuler: "#c9a227",
  activeMatchBackground: "#ff9632",
  activeMatchBorder: "#ffffff",
  activeMatchColorOverviewRuler: "#ff9632",
};

function FindBar({
  search,
  term,
  initial,
  onClose,
}: {
  search: SearchAddon;
  term: Terminal | null;
  initial: string;
  onClose: () => void;
}) {
  const [q, setQ] = useState(initial.split("\n")[0] ?? "");
  const [caseSensitive, setCaseSensitive] = useState(false);
  const [wholeWord, setWholeWord] = useState(false);
  const [regex, setRegex] = useState(false);
  const [result, setResult] = useState<{ index: number; count: number } | null>(null);
  const [bad, setBad] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);
  // tmux / screen / less / vim 等全屏程序跑在备用屏：xterm 没有它的滚动历史，只能搜到当前这一屏
  const [altScreen, setAltScreen] = useState(term?.buffer.active.type === "alternate");

  useEffect(() => {
    inputRef.current?.focus();
    inputRef.current?.select();
  }, []);

  useEffect(() => {
    if (!term) return;
    setAltScreen(term.buffer.active.type === "alternate");
    const sub = term.buffer.onBufferChange((b) => setAltScreen(b.type === "alternate"));
    return () => sub.dispose();
  }, [term]);

  useEffect(() => {
    const sub = search.onDidChangeResults((r) =>
      setResult({ index: r.resultIndex, count: r.resultCount })
    );
    return () => sub.dispose();
  }, [search]);

  const opts = useCallback(
    (incremental: boolean) => ({
      caseSensitive,
      wholeWord,
      regex,
      incremental,
      decorations: FIND_DECO,
    }),
    [caseSensitive, wholeWord, regex]
  );

  // 关键字或选项变化：增量查找第一个
  useEffect(() => {
    if (!q) {
      search.clearDecorations();
      setResult(null);
      setBad(false);
      return;
    }
    if (regex) {
      try {
        new RegExp(q);
      } catch {
        setBad(true);
        return;
      }
    }
    setBad(false);
    const found = search.findNext(q, opts(true));
    if (!found) setResult({ index: -1, count: 0 });
  }, [q, opts, regex, search]);

  const next = () => q && !bad && search.findNext(q, opts(false));
  const prev = () => q && !bad && search.findPrevious(q, opts(false));

  const status = !q
    ? ""
    : bad
      ? "正则无效"
      : !result || result.count === 0
        ? "无结果"
        : result.index < 0
          ? `${result.count}+ 处`
          : `${result.index + 1} / ${result.count}`;

  return (
    <div className="find-bar" onMouseDown={(e) => e.stopPropagation()}>
      <div className="find-row">
        <input
          ref={inputRef}
          className={`find-input${bad || (result && result.count === 0 && q) ? " miss" : ""}`}
          value={q}
          placeholder="查找关键字"
          onChange={(e) => setQ(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              if (e.shiftKey) prev();
              else next();
            } else if (e.key === "Escape") {
              e.preventDefault();
              onClose();
            }
          }}
        />
        <span className="find-status">{status}</span>
        <button className="find-btn" onClick={prev} title="上一个 (Shift+Enter)">↑</button>
        <button className="find-btn" onClick={next} title="下一个 (Enter)">↓</button>
        <button className="find-btn" onClick={onClose} title="关闭 (Esc)">×</button>
      </div>
      {altScreen && (
        <div className="find-hint">
          当前在全屏程序（tmux / screen / less 等）里，只能搜索当前这一屏；它自己的滚动历史请用它自带的搜索（tmux：Ctrl+B 后按 [，再按 Ctrl+S）。
        </div>
      )}
      <div className="find-opts">
        <label>
          <input
            type="checkbox"
            checked={caseSensitive}
            onChange={(e) => setCaseSensitive(e.target.checked)}
          />
          大小写敏感
        </label>
        <label>
          <input
            type="checkbox"
            checked={wholeWord}
            onChange={(e) => setWholeWord(e.target.checked)}
          />
          全词匹配
        </label>
        <label>
          <input type="checkbox" checked={regex} onChange={(e) => setRegex(e.target.checked)} />
          正则
        </label>
      </div>
    </div>
  );
}

// ——— 标记窗口 ———

function MarkDialog({
  initial,
  initialCase,
  seed,
  onApply,
  onClear,
  onClose,
}: {
  initial: MarkRule[];
  initialCase: boolean;
  /** 打开时终端里的选中文本：自动填进第一个空位 */
  seed: string;
  onApply: (rules: MarkRule[], caseSensitive: boolean) => number;
  onClear: () => void;
  onClose: () => void;
}) {
  const [texts, setTexts] = useState<string[]>(() => {
    const t = MARK_COLORS.map((c) => initial.find((r) => r.color === c.color)?.text ?? "");
    const s = seed.split("\n")[0]?.trim();
    if (s && !t.includes(s)) {
      const i = t.findIndex((x) => !x);
      if (i >= 0) t[i] = s;
    }
    return t;
  });
  const [cs, setCs] = useState(initialCase);
  const firstRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    firstRef.current?.focus();
  }, []);

  const submit = () => {
    onApply(
      MARK_COLORS.map((c, i) => ({ color: c.color, text: texts[i] ?? "" })),
      cs
    );
    onClose();
  };

  return (
    <div className="modal-backdrop" onMouseDown={onClose}>
      <div
        className="modal mark-modal"
        onMouseDown={(e) => e.stopPropagation()}
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.preventDefault();
            onClose();
          } else if (e.key === "Enter") {
            e.preventDefault();
            submit();
          }
        }}
      >
        <h2>标记关键字</h2>
        <p className="mark-hint">为每种颜色输入要高亮的文本，留空则不使用。新输出会自动按规则继续高亮。</p>
        {MARK_COLORS.map((c, i) => (
          <div key={c.key} className="mark-row">
            <span className="mark-swatch" style={{ background: c.color }} />
            <span className="mark-label">{c.label}</span>
            <input
              ref={i === 0 ? firstRef : undefined}
              value={texts[i]}
              placeholder="要高亮的文本"
              onChange={(e) =>
                setTexts((prev) => prev.map((x, j) => (j === i ? e.target.value : x)))
              }
            />
            {texts[i] && (
              <button
                className="mark-x"
                title="清空"
                onClick={() => setTexts((prev) => prev.map((x, j) => (j === i ? "" : x)))}
              >
                ×
              </button>
            )}
          </div>
        ))}
        <label className="mark-case">
          <input type="checkbox" checked={cs} onChange={(e) => setCs(e.target.checked)} />
          大小写敏感
        </label>
        <div className="modal-actions">
          <button
            className="btn"
            style={{ marginRight: "auto" }}
            onClick={() => {
              onClear();
              onClose();
            }}
          >
            清除全部标记
          </button>
          <button className="btn" onClick={onClose}>
            取消
          </button>
          <button className="btn primary" onClick={submit}>
            确定
          </button>
        </div>
      </div>
    </div>
  );
}
