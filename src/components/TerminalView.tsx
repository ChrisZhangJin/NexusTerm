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
import { readableKind } from "../lib/readable";
import {
  buildCompletions,
  dropCompletion,
  feedKeys,
  loadHistory,
  newLineState,
  recordCommand,
  type CompletionItem,
} from "../lib/complete";
import { GhostText, cursorAtLineEnd, cursorFollowsTyped } from "../lib/ghost";
import { ImeGuard, type KeyLike } from "../lib/ime";
import CompletionOverlay from "./CompletionOverlay";

import {
  api,
  onLocalClosed,
  onLocalData,
  onTermClosed,
  onTermData,
  bytesToBase64,
  stringToBytes,
} from "../api";
import { themeToXterm } from "../lib/theme";
import { isMacOS } from "../lib/platform";
import { installMacKeyboard } from "../lib/macInput";
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
 * 让 macOS 也支持按住 Shift 拖选文本（绕过 tmux 等程序的鼠标捕获）。
 *
 * xterm.js 的 `SelectionService.shouldForceSelection()` 是按平台分叉的：
 *
 * ```ts
 * if (Browser.isMac) {
 *   return event.altKey && options.macOptionClickForcesSelection;
 * }
 * return event.shiftKey;
 * ```
 *
 * 于是同一份 `set -g mouse on` 的 tmux 配置下，Windows / Linux 上 Shift+拖选
 * 能正常选文本，macOS 上 Shift 根本不算绕过键 —— 只有 ⌥ 算，而且还得显式把
 * macOptionClickForcesSelection 打开（默认 false）。
 *
 * 官方没有给 macOS 开 Shift 的选项（旧的 attachCustomMouseEventHandler 在
 * 5.x 已移除），所以这里对实例上的方法做特性检测式补丁：拿得到就补一条 Shift
 * 分支，拿不到就静默退回官方行为，绝不影响正常的选区与鼠标上报。
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
   * 命令历史按「会话身份」存取，不是 tabId：本地 shell 固定 `"local"`，
   * 已保存的 SSH 连接是它的 `SessionRecord.id`——这两种会持久化，重连/
   * 重启应用都能接着用。没有保存记录的临时连接，调用方应该传 `tabId`
   * 本身当 key：不会被持久化，行为与过去的纯内存历史一致。
   */
  historyKey: string;
  /** `historyKey` 是否要落盘持久化（本地 / 已保存会话为 true，临时连接为 false） */
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
}

/** 终端区域右键菜单状态 */
interface TermCtxMenu {
  x: number;
  y: number;
  /** 右键时的选区（菜单打开后选区可能被改，先存下来） */
  sel: string;
}

/** 选区 → 可阅读的文件路径（md / html / 图片 / 配置文件）；否则返回 null */
function readablePath(sel: string): string | null {
  let s = sel.trim();
  if (!s || s.includes("\n")) return null;
  s = s.replace(/^['"`]+|['"`,;:]+$/g, ""); // 去掉 ls / 日志里常见的引号与尾随标点
  s = s.replace(/^file:\/\//i, "");
  return readableKind(s) ? s : null;
}

function debounce<A extends unknown[]>(fn: (...a: A) => void, ms: number) {
  let t: number | undefined;
  return (...a: A) => {
    if (t) window.clearTimeout(t);
    t = window.setTimeout(() => fn(...a), ms);
  };
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
  peerHost,
  status,
  reconnectAttempt,
  reconnectMax,
}: Props) {
  // 事件回调在 mount 时注册一次，用 ref 拿最新的 props
  const onClosedRef = useRef(onClosed);
  onClosedRef.current = onClosed;
  const peerHostRef = useRef(peerHost);
  peerHostRef.current = peerHost;
  const isLocal = kind === "local";
  const hostRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const activeRef = useRef(active);
  const [ctxMenu, setCtxMenu] = useState<TermCtxMenu | null>(null);
  const searchRef = useRef<SearchAddon | null>(null);
  const hlRef = useRef<Highlighter | null>(null);
  const [findOpen, setFindOpen] = useState(false);
  const [markOpen, setMarkOpen] = useState(false);
  /** 当前生效的标记规则数（右上角徽标用） */
  const [markCount, setMarkCount] = useState(0);
  const zmodemRef = useRef<ZmodemGate | null>(null);
  /** 外部文件拖入终端时的高亮遮罩 */
  const [dropHint, setDropHint] = useState(false);
  const dragDepth = useRef(0);
  const statusRef = useRef(status);
  statusRef.current = status;

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
   * 这一行有没有真的被回显过。
   *
   * 密码一类的输入（sudo、ssh、`passwd`……）远端会关掉回显，敲的字符
   * 一个都不会被写回屏幕，`cursorFollowsTyped` 也就永远匹配不上。
   * 拿它当「这行是不是密码」的判据：回了车却从没见过回显，就别记进历史——
   * history 本是用来猜下一条命令的，密码混进去既没用又危险。
   */
  const lineEchoedRef = useRef(false);
  const ghostRef = useRef(new GhostText());
  /** 当前候选列表（null = 不显示下拉） */
  const [hints, setHints] = useState<CompletionItem[] | null>(null);
  const [hintIndex, setHintIndex] = useState(0);
  // 键盘处理器只在 mount 时注册一次，闭包里拿不到逐次渲染的 hintIndex，
  // 用 ref 桥接 —— 否则按 ↑↓ 挑中第 3 条后按 Tab，补进去的还是第 1 条
  const hintIndexRef = useRef(0);
  hintIndexRef.current = hintIndex;
  /**
   * 唯一的「改高亮项」入口：ref 与 state 一起写。
   *
   * 只调 `setHintIndex` 是不够的 —— ref 要等下一次渲染才跟上，而鼠标划过下拉、
   * 或按完 ↑↓ 之后，人可能在同一帧内就按 Tab / Enter。那一刻读到的还是旧下标，
   * 补进去的自然不是屏幕上高亮的那条（表现为「按 Tab 总是补第一条」）。
   */
  const setActiveHint = useCallback((i: number) => {
    hintIndexRef.current = i;
    setHintIndex(i);
  }, []);
  /** 下拉锚点：光标在视口里的像素坐标 */
  const [hintPos, setHintPos] = useState({ x: 0, y: 0 });
  /** 上一次推给界面的下拉位置（用来跳过「位置没变」的重复渲染） */
  const hintPosRef = useRef({ x: 0, y: 0 });

  /** 上一次推给界面的候选签名（label 拼起来），用来跳过内容没变的重复渲染 */
  const hintSigRef = useRef("\u0000");
  const hintsRef = useRef<CompletionItem[]>([]);
  /**
   * 本帧有没有排队的补全重算（rAF 句柄，0 = 没排）。
   *
   * 敲键时**不**当场算候选：算候选要读历史、量 DOM（`getBoundingClientRect`）、
   * 改 React 状态、往终端里写灰字 —— 这些同步塞在 `onData` 里，等于让
   * 「把字节发给 shell」排在它们后头，连着敲就是一串能感觉到的顿挫。
   * 改成一帧最多算一次之后，打字这条路上只剩发字节，提示最多晚一帧出现。
   */
  const hintRaf = useRef(0);

  // 高亮项变了就把灰字换成它：下拉里选中哪条，光标后面就该提示哪条，
  // 否则「按 ↑↓ 挑一条再按 Tab」会得到跟选中项不一样的结果。
  useEffect(() => {
    const term = termRef.current;
    // 取 `hintsRef` 而不是 `hints`：状态里的那份可能因为「候选没变就不推送」
    // 而落后一步（`insert` 是随行内容变的），灰字必须跟真正会被 Tab 补进去的一致。
    const items = hintsRef.current;
    if (!term || items.length === 0 || term.buffer.active.type !== "normal") return;
    const item = items[Math.min(hintIndex, items.length - 1)];
    if (!item) return;
    ghostRef.current.show(term, item.insert, term.cols - term.buffer.active.cursorX - 1);
  }, [hintIndex, hints]);

  /**
   * 把候选列表推给界面，并把第一条画成行内灰字。
   *
   * 灰字只显示「放得下」的那一段（见 `GhostText.show`）——
   * 宁可提示得短一点也不让它换行把屏幕顶乱。
   */
  const publish = useCallback((items: CompletionItem[]) => {
    const term = termRef.current;
    const host = hostRef.current;
    if (!term || !host) return;

    // vim / less / top 用的是 alternate buffer：那里没有 shell 提示符，
    // 「当前行 = 人敲的命令」这个前提根本不成立。全屏程序里一律不提示，
    // 否则灰字会直接画在编辑器内容上。
    if (term.buffer.active.type !== "normal") {
      dismissHints();
      return;
    }
    // 人在行中间删改时一律不提示：补进去的那段会插到句子中间，
    // 而且灰字的擦除只敢从光标往后抹 —— 那里有真字就会连真字一起抹掉
    // （屏幕上看没了、shell 里还在，回车就又冒出来）。见 `cursorAtLineEnd`。
    if (!cursorAtLineEnd(term)) {
      dismissHints();
      return;
    }

    hintsRef.current = items;
    // 候选没变就不要再推一次状态：`insert` 是由 `label` 减去已输入前缀算出来的，
    // 所以 label 一样、补充部分必然一样。行里接着敲字时列表常常是同一批
    // （历史里就那几条），不比较的话每敲一个字都要重建一遍下拉的 40 个节点。
    const sig = items.map((i) => i.label).join("\n");
    if (sig !== hintSigRef.current) {
      hintSigRef.current = sig;
      setHints(items.length ? items : null);
      setActiveHint(0);
    }

    const room = term.cols - term.buffer.active.cursorX - 1;
    // 画的是「下拉里当前高亮的那条」而不是永远第一条：候选没变时高亮可能已经被
    // 方向键挪走了（见上面的签名判重），这时灰字必须跟着高亮走，否则按 Tab
    // 补进去的东西和屏幕上提示的不是同一条。
    const shownItem = items[Math.min(hintIndexRef.current, items.length - 1)];
    const ghost = ghostRef.current.show(term, shownItem ? shownItem.insert : "", room);
    if (!ghost) return;

    // 只在灰字真的画出来了才定位下拉：位置按光标格换算，
    // 与 xterm 的字符网格对齐（cellWidth 已处理 CJK / emoji 占两格的情况）
    const r = host.getBoundingClientRect();
    const cw = term.cols > 0 ? r.width / term.cols : 8;
    const ch = term.rows > 0 ? r.height / term.rows : 16;
    const x = Math.round(Math.min(r.left + term.buffer.active.cursorX * cw, window.innerWidth - 330));
    const y = Math.round(r.top + (term.buffer.active.cursorY + 1) * ch);
    // 同一个位置就不要再 set 一次：每次都是新对象，React 会当成变化重新渲染 ——
    // 而「敲一个字、光标挪一格、下拉跟着右移」正是每次按键都发生的事。
    // 只有真的挪了才推给界面，一行里连着打十个字就只重渲染一次。
    const last = hintPosRef.current;
    if (last.x !== x || last.y !== y) {
      hintPosRef.current = { x, y };
      setHintPos({ x, y });
    }
  }, []);

  /** 收起提示：擦掉灰字、清空候选。 */
  const dismissHints = useCallback(() => {
    const term = termRef.current;
    if (term) ghostRef.current.clear(term);
    hintsRef.current = [];
    hintSigRef.current = "\u0000";
    setHints(null);
    setActiveHint(0);
  }, [setActiveHint]);

  /** 按当前这半行重算候选：历史是本地同步缓存，不用等网络往返。 */
  const refreshHints = useCallback(
    (line: string) => {
      publish(buildCompletions(historyKeyRef.current, line));
    },
    [publish]
  );

  /**
   * 敲键后的补全刷新：**一帧最多一次**。
   *
   * 行状态（`lineRef`）本身就是即时更新的，所以延迟这一帧不会让候选算错 ——
   * 刷的时候读的是当时的行，早先几帧的中间态直接被跳过，反而更省。
   */
  const refreshHintsSoon = useCallback(() => {
    if (hintRaf.current) return;
    hintRaf.current = requestAnimationFrame(() => {
      hintRaf.current = 0;
      refreshHints(lineRef.current.text);
    });
  }, [refreshHints]);

  // 标签页关掉 / 组件卸载时把没跑完的那一帧撤掉，否则回调会碰已 dispose 的终端
  useEffect(() => {
    return () => {
      if (hintRaf.current) cancelAnimationFrame(hintRaf.current);
      hintRaf.current = 0;
    };
  }, []);

  /**
   * 接受当前高亮项：把缺的那一段补进终端。
   *
   * 两件事必须同时做：写 pty（让远端 shell 真的收到），
   * 以及更新本地行状态（否则下一次预测还按补完前的半句算）。
   */
  const acceptHintAt = useCallback(
    (index: number) => {
      const items = hintsRef.current;
      const term = termRef.current;
      if (!term || items.length === 0) return;
      // 先擦掉灰字再判「光标是否在行尾」：灰字本身就是画在光标后面的真实字符，
      // 不擦掉的话 `cursorAtLineEnd` 会把它当成「光标后面还有别的字」，
      // 于是按 Tab 接受补全反而被当成「在行中间编辑」给收起了——
      // 候选只有一条、灰字立刻画满光标后那几格时最容易踩到。
      ghostRef.current.clear(term);
      // 光标已经挪到行中间（人在改前面敲错的字）：这时补进去会插在句子中间，
      // 收起下拉比硬补安全 —— 想继续补全，按 End 回到行尾即可。
      if (!cursorAtLineEnd(term)) {
        dismissHints();
        return;
      }
      const item = items[Math.min(index, items.length - 1)];
      if (!item.insert) {
        dismissHints();
        return;
      }
      lineRef.current.text += item.insert;
      // 只把「缺的那一段」写进 pty：已经敲出来的字符已经在 shell 里了，
      // 再写一遍会重复。这里的写入和敲键盘走的是同一条路。
      void (isLocal
        ? api.localWrite(tabId, stringToBytes(item.insert))
        : api.sshWrite(tabId, stringToBytes(item.insert))
      ).catch(() => {});
      // 补完后重算一次：多半会自然消失（后面要打的是新 token），
      // 但遇到 `--f` → `--follow` 这种还能接着补的，下拉会立刻给出下一批。
      refreshHints(lineRef.current.text);
    },
    [dismissHints, isLocal, refreshHints]
  );

  /** 键盘 Tab 走当前高亮项；鼠标点击走被点的那一项。 */
  const acceptHint = useCallback(() => {
    acceptHintAt(hintIndexRef.current);
  }, [acceptHintAt]);

  /** 写 pty 的那条路定义在 mount 的 effect 里，用 ref 桥给下面的回调 */
  const sendRef = useRef<(d: string) => void>(() => {});
  /**
   * 输入法状态。接管回车之前必须问它一句：拼音组合里按回车是「上屏」，
   * 不是「执行」 —— 吞掉就打不出中文了。判定逻辑见 `lib/ime.ts`。
   */
  const imeRef = useRef<ImeGuard>(new ImeGuard());

  /**
   * Enter：先把高亮项补齐，**再**补一个真正的回车（也就是直接跑起来）。
   *
   * 下拉开着时，屏幕上的这行是「已经敲进去的半句 + 灰字」，灰字看着就是命令的
   * 一部分。这时候回车如果照旧只发半句，就成了「看到 A 执行 B」——
   * 敲的是 `vim P`，跑的是 `vim `。所以回车必须先把高亮项补上。
   *
   * 补完一定要收起下拉：`acceptHintAt` 末尾会重算候选，补出来的整句常常还能
   * 接着联想，下拉会马上又弹出来；不收起的话第二次回车又被吞掉，
   * 这条命令就永远发不出去了。
   */
  const acceptHintThenRun = useCallback(() => {
    acceptHintAt(hintIndexRef.current);
    dismissHints();
    // 走的是与「自己敲回车」同一条记账路径：行状态提交 + 进历史，再发 \r。
    const committed = feedKeys(lineRef.current, "\r");
    if (committed) {
      if (lineEchoedRef.current) recordCommand(historyKeyRef.current, committed, historyPersistRef.current);
      lineEchoedRef.current = false;
    }
    sendRef.current("\r");
  }, [acceptHintAt, dismissHints]);

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

    // 只有持久化的 key（本地 / 已保存会话）才值得拉全量历史；
    // 临时连接没有这个身份，维持现状的纯内存行为。
    if (historyPersist) void loadHistory(historyKey).catch(() => {});

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
    term.loadAddon(new WebLinksAddon());
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
    // 输入法：xterm 自己会处理组合输入，这里只是旁观 —— 记下「组合开始/结束」
    // 就够了，键盘处理器据此决定回车该不该被补全接管。capture 是为了抢在
    // xterm 内部处理之前拿到（顺序无所谓，但能保证不漏）。
    const onImeStart = () => imeRef.current.begin();
    const onImeEnd = (e: CompositionEvent) => imeRef.current.finish(e.timeStamp);
    // 组合中被切走时 compositionend 不一定来，留着 composing=true 会把回车全吞掉
    const onImeReset = () => imeRef.current.reset();
    host.addEventListener("compositionstart", onImeStart, true);
    host.addEventListener("compositionend", onImeEnd, true);
    window.addEventListener("blur", onImeReset);
    cleanups.push(() => {
      host.removeEventListener("mousedown", onSelDown, true);
      window.removeEventListener("mouseup", onSelUp, true);
      host.removeEventListener("compositionstart", onImeStart, true);
      host.removeEventListener("compositionend", onImeEnd, true);
      window.removeEventListener("blur", onImeReset);
    });
    // resize 会让列数、也可能让光标所在的物理行都变掉：灰字记的 atX/atY
    // 立刻过时，`clear` 的 `sameSpot` 检查从此永远对不上——擦不掉的灰字就会
    // 当真字一样赖在屏幕上。索性收起，下一次按键会按新尺寸重新算一遍。
    const resizeSub = term.onResize(() => {
      hl.refresh();
      dismissHints();
    });
    const bufSub = term.buffer.onBufferChange(() => hl.refresh());
    // 光标一挪就要复查：灰字是画在「当时的光标」之后的，
    // 按 ← / Home 回到行中间之后，它还赖在原处就变成了插在句子里的半截命令，
    // 而下一次擦除又会从新光标一路抹到行尾 —— 把后面真敲的字抹掉。
    // 只在灰字真的画着时才查 buffer，挪动光标本身是很热的路径。
    const cursorSub = term.onCursorMove(() => {
      if (!ghostRef.current.active) return;
      if (term.buffer.active.type !== "normal" || !cursorAtLineEnd(term)) {
        dismissHints();
      }
    });
    cleanups.push(
      () => parsedSub.dispose(),
      () => resizeSub.dispose(),
      () => bufSub.dispose(),
      () => cursorSub.dispose(),
      () => hl.dispose()
    );

    /**
     * 把灰字重新画回光标后面。
     *
     * 只在「光标还贴着我们记下的那半行」时才画 —— 见 `cursorFollowsTyped`。
     * 一旦不成立（真的输出来了、提示符被整行重画了），说明这一轮输入已经结束，
     * 直接收起提示，别把灰字甩到输出中间。
     */
    const redrawGhost = () => {
      const items = hintsRef.current;
      const typed = lineRef.current.text;
      if (!items.length || !typed) return;
      if (
        term.buffer.active.type !== "normal" ||
        !cursorAtLineEnd(term) ||
        !cursorFollowsTyped(term, typed)
      ) {
        dismissHints();
        return;
      }
      const item = items[Math.min(hintIndexRef.current, items.length - 1)];
      ghostRef.current.show(term, item.insert, term.cols - term.buffer.active.cursorX - 1);
    };

    // 灰字画在终端的**显示缓冲**里，不是 pty 里的真实字符。
    // 只要往屏幕上又写了东西，那段灰字就会变成"真"文本赖在那儿，
    // 还会把光标推到它后面 —— 于是人接着敲的字全落在灰字之后。
    // 所以所有「往屏幕上写字」的地方都必须走这一个出口：先擦，写完再补画。
    //
    // 「再补画」不是可有可无的润色：shell 会把人敲的每个字符回显回来，
    // 回显走的就是这里。不补画的话，灰字只在「按键 → 回显到达」这几十毫秒里
    // 闪一下就没了 —— 本地还好，SSH 上一个来回就是肉眼可见的闪烁。
    //
    // 补画按帧合并：回显是**一个字节一块**到达的，逐块补画等于每个字符都
    // 擦一次整行、再画一次整行（两次整行重绘）。一帧里到几块都只补画一次。
    let paintRaf = 0;
    const scheduleGhostPaint = () => {
      if (paintRaf) return;
      paintRaf = requestAnimationFrame(() => {
        paintRaf = 0;
        redrawGhost();
      });
    };
    cleanups.push(() => {
      if (paintRaf) cancelAnimationFrame(paintRaf);
    });

    const writeOut = (data: string | Uint8Array) => {
      ghostRef.current.clear(term);
      term.write(data);
      // 这一行是不是真被回显过——门槛判据与灰字重画共用一套
      // （见 `lib/ghost.ts` 的 `cursorFollowsTyped`），但这里当安全边界用，
      // 不能按它默认的「1 个字符就算」：密码的第一个字符恰好撞上提示符
      // 末尾或者上一行的残留字符，1 个字符的巧合匹配就会把没回显的密码
      // 误判成「回显过」而记进历史。至少连续匹配 3 个字符才采信。
      if (
        lineRef.current.text &&
        cursorFollowsTyped(term, lineRef.current.text, Math.min(3, lineRef.current.text.length))
      ) {
        lineEchoedRef.current = true;
      }
      scheduleGhostPaint();
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

    /**
     * 所有「用户敲进来的字节」的唯一出口。
     *
     * 两个调用方：`term.onData`（回车、Ctrl 组合、方向键、输入法上屏、粘贴）
     * 和 `lib/macInput` 的接管层（mac 上的普通字符 —— xterm 自己那条路会丢字）。
     * 两者共用这一份逻辑，宏录制与补全行状态才不会漏掉其中一半。
     */
    const handleInput = (d: string) => {
      if (zmodem.isActive()) return;
      // 旁路监听（宏录制、宏回放时「用户插了一脚就中止」）。发不发送由这里决定，监听方只读
      emitInput(tabId, d);
      send(d).catch(() => {});
      // 新的一行开始：清掉上一行留下的回显标记，不然刚敲完一条密码、
      // 紧接着正常敲的命令会被上一行"已回显过"的旧状态蹭到历史里。
      if (!lineRef.current.text) lineEchoedRef.current = false;
      // 补全：喂键攒行（同步、纯字符串运算），候选留到下一帧再算
      const committed = feedKeys(lineRef.current, d);
      if (committed) {
        if (lineEchoedRef.current) recordCommand(historyKeyRef.current, committed, historyPersistRef.current);
        lineEchoedRef.current = false;
      }
      refreshHintsSoon();
    };

    const dataSub = term.onData(handleInput);

    // macOS(WKWebView) 上普通字符改由我们自己在捕获阶段发 —— 详见 lib/macInput。
    // 必须挂在 host 上：xterm 的监听在 textarea 上，捕获阶段我们先到。
    cleanups.push(installMacKeyboard(host, term, handleInput, isMacOS()));

    // 快捷键全部来自「设置 → 快捷键」（lib/prefs），改了立即生效。
    // 默认 Windows / Linux 用 Ctrl+Shift+C/V/F：Ctrl+C 必须留给中断信号（^C）。
    term.attachCustomKeyEventHandler((e) => {
      if (e.type !== "keydown") return true;

      // 只在**有候选**时才接管 Tab / Enter / Esc / ↑↓。
      // 没有候选时一律放行 —— 那几个键本来属于 shell 自己的补全与历史，
      // 不能因为我们多了个提示层就把终端原本的能力挤掉。
      if (hintsRef.current && hintsRef.current.length > 0) {
        // 输入法组合中的按键一律不接管：那时候按回车是「选词上屏」、
        // 按 Esc 是「取消这组候选」，跟补全没关系，吞了就打不出中文了。
        // 判定走 `lib/ime.ts` 的那套（229 / isComposing / 组合刚结束的窗口），
        // 这里拿到的是原生事件，直接压成 KeyLike 即可。
        const info: KeyLike = {
          key: e.key,
          shiftKey: e.shiftKey,
          isComposing: e.isComposing,
          keyCode: e.keyCode ?? 0,
          timeStamp: e.timeStamp,
        };

        if (!imeRef.current.captures(info)) {
          if (e.key === "Tab") {
            e.preventDefault();
            acceptHint();
            return false;
          }
          if (e.key === "Enter") {
            // "send" 才是真回车；"ime" / "ime-after-commit" 是输入法在收尾，
            // "pass" 是 Shift+Enter 之类 —— 这几种都交回给终端自己处理。
            if (imeRef.current.verdict(info) !== "send") return true;
            e.preventDefault();
            acceptHintThenRun();
            return false;
          }
          if (e.key === "Escape") {
            e.preventDefault();
            dismissHints();
            return false;
          }
          if (e.key === "ArrowDown" || e.key === "ArrowUp") {
            e.preventDefault();
            const n = hintsRef.current?.length ?? 1;
            const step = e.key === "ArrowDown" ? 1 : -1;
            setActiveHint((hintIndexRef.current + step + n) % n);
            return false;
          }
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
      setCtxMenu({
        x: Math.min(e.clientX, window.innerWidth - MENU_W - 8),
        y: Math.min(e.clientY, window.innerHeight - MENU_H - 8),
        sel: term.getSelection(),
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
      // 临时连接的历史只在内存里，标签关掉就没用了；
      // 持久化的 key（本地 / 已保存会话）不动——可能还有别的标签在用，
      // 真正的数据也早就落盘了。
      dropCompletion(historyKeyRef.current, historyPersistRef.current);
    };
    // 字体等外观变化通过下面的 effect 单独应用，不重建实例
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tabId]);

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
    // 同样是往屏幕上写字：先把灰字擦掉，否则它会混进这行提示里
    const out = (s: string) => {
      ghostRef.current.clear(term);
      term.write(s);
    };
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
      .clipboardRead()
      .then((t) => {
        if (!t) return;
        // 走 xterm.paste：自动处理换行规范化与 bracketed paste，和键盘粘贴行为一致
        termRef.current?.paste(t);
      })
      .catch((e) => onNotify?.(`粘贴失败：${String(e)}`));
    termRef.current?.focus();
  }, [onNotify]);

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
   * 软换行：只换行、不执行。
   *
   * xterm.js 对 Enter 只判断 `altKey`（`case 13: altKey ? ESC+CR : CR`），
   * 也就是说 **Ctrl+Enter 与 Enter 发的是同一个 \r**，想改键必须在应用层拦。
   *
   * 这里刻意复用 Alt+Enter 的原生字节序列（ESC + CR），而不是发 "\n"：
   * 终端里 LF 同样是“提交”，只有 ESC 前缀能让远端 readline / tmux 认出
   * “这是一个带修饰键的 Enter”。字节一致 ⇒ 行为与改键前完全等价，
   * 不依赖远端 shell 是否刚好绑了 ^J。
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
  /** 阅读选中的文件：相对路径按 shell 当前目录解析 */
  const doRead = useCallback(
    async (raw: string) => {
      setCtxMenu(null);
      const p = readablePath(raw);
      if (!p) return;
      const name = p.split(/[\\/]/).pop() ?? p;
      try {
        if (isLocal) {
          const abs = /^([A-Za-z]:[\\/]|[\\/]|~)/.test(p);
          if (!abs) {
            onNotify?.("本地终端请选中完整路径（如 C:\\docs\\README.md）");
            return;
          }
          await api.readerOpenLocal(p);
          return;
        }
        let full = p;
        if (!p.startsWith("/") && p !== "~" && !p.startsWith("~/")) {
          const ctx = parseProbe(await api.sshProbe(tabId).catch(() => ({})));
          if (!ctx?.cwd) {
            onNotify?.("无法确定远端当前目录，请选中完整路径（/… 或 ~/…）");
            return;
          }
          full = (ctx.cwd.endsWith("/") ? ctx.cwd : ctx.cwd + "/") + p.replace(/^\.\//, "");
        }
        onNotify?.(`正在打开「${name}」…`);
        const where = peerHostRef.current ? `${peerHostRef.current}:` : "";
        await api.readerOpenSsh(tabId, full, `${where}${full}`);
      } catch (e) {
        onNotify?.(`阅读失败：${String(e)}`);
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
      {active && hints && hints.length > 0 && (
        <CompletionOverlay
          items={hints}
          active={Math.min(hintIndex, hints.length - 1)}
          x={hintPos.x}
          y={hintPos.y}
          viewportHeight={window.innerHeight}
          onPick={(i) => {
            setActiveHint(i);
            acceptHintAt(i);
          }}
          onHover={setActiveHint}
        />
      )}
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
              const p = readablePath(ctxMenu.sel);
              const name = p?.split(/[\\/]/).pop();
              return (
                <button
                  className="ctx-item"
                  disabled={!p || (!isLocal && status !== "connected")}
                  title={p ? "在新窗口中渲染查看" : "先用鼠标选中一个 .md / .html 文件名或路径"}
                  onClick={() => p && void doRead(ctxMenu.sel)}
                >
                  <span className="ctx-ellipsis">{name ? `阅读「${name}」` : "阅读"}</span>
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
      {active && findOpen && searchRef.current && (
        <FindBar
          search={searchRef.current}
          initial={termRef.current?.getSelection() ?? ""}
          onClose={() => {
            searchRef.current?.clearDecorations();
            setFindOpen(false);
            termRef.current?.focus();
          }}
        />
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
  initial,
  onClose,
}: {
  search: SearchAddon;
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

  useEffect(() => {
    inputRef.current?.focus();
    inputRef.current?.select();
  }, []);

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
