import { useCallback, useEffect, useRef, useState } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { CanvasAddon } from "@xterm/addon-canvas";
import { Unicode11Addon } from "@xterm/addon-unicode11";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { SearchAddon } from "@xterm/addon-search";
import "@xterm/xterm/css/xterm.css";
import { markOutput, registerTerm } from "../lib/termRegistry";
import { Highlighter, MARK_COLORS, bufferText, type MarkRule } from "../lib/highlight";

import {
  api,
  onLocalClosed,
  onLocalData,
  onTermClosed,
  onTermData,
  stringToBytes,
} from "../api";
import { isMacOS } from "../lib/platform";
import { themeToXterm } from "../lib/theme";
import { attachZmodem } from "../lib/zmodem";
import type { ClosedInfo, TerminalTab, Theme } from "../types";

/** HH:MM:SS，本地时区 */
export function clock(ts: number): string {
  const d = new Date(ts);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

interface Props {
  tabId: string;
  active: boolean;
  /** ssh = 远端会话；local = 本机 PTY。两者共用同一套渲染与事件协议 */
  kind?: "ssh" | "local";
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
}

/** 终端区域右键菜单状态 */
interface TermCtxMenu {
  x: number;
  y: number;
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
  theme,
  fontFamily,
  fontSize,
  lineHeight,
  onClosed,
  onNotify,
  onDisconnect,
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

  activeRef.current = active;

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
    const resizeSub = term.onResize(() => hl.refresh());
    const bufSub = term.buffer.onBufferChange(() => hl.refresh());
    cleanups.push(
      () => parsedSub.dispose(),
      () => resizeSub.dispose(),
      () => bufSub.dispose(),
      () => hl.dispose()
    );

    // —— ZMODEM（lrzsz）：远端 rz/sz 的字节流先过 Sentry 嗅探 ——
    const zmodem = attachZmodem({
      termWrite: (s) => term.write(s),
      termWriteBytes: (b) => term.write(b),
      send: (bytes) => {
        (isLocal ? api.localWrite(tabId, bytes) : api.sshWrite(tabId, bytes)).catch(
          () => {}
        );
      },
      notify: onNotify ?? (() => {}),
    });

    // 输出 → 终端（SSH 与本地 PTY 协议一致，只是事件名前缀不同）
    const dataFeed = isLocal ? onLocalData : onTermData;
    dataFeed(tabId, (bytes) => {
      if (!disposed) zmodem.consume(bytes);
    }).then((u) => cleanups.push(u));

    if (isLocal) {
      onLocalClosed(tabId, (code) => {
        if (disposed) return;
        term.write(`\r\n\x1b[90m[Shell 已退出，退出码 ${code}]\x1b[0m\r\n`);
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
        term.write(
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

    const dataSub = term.onData((d) => {
      if (zmodem.isActive()) return;
      send(d).catch(() => {});
    });

    // Windows / Linux 上 Ctrl+C 必须留给中断信号（^C），不能像 macOS 那样
    // 兼作复制（macOS 有 ⌘C）。补上终端惯例的 Ctrl+Shift+C / Ctrl+Shift+V，
    // 否则非 macOS 用户根本没有复制入口。
    if (!isMacOS()) {
      term.attachCustomKeyEventHandler((e) => {
        if (e.type !== "keydown" || !(e.ctrlKey || e.metaKey) || !e.shiftKey) {
          return true;
        }
        const k = e.key.toLowerCase();
        if (k === "f") {
          setFindOpen(true);
          return false;
        }
        if (k === "c") {
          const sel = term.getSelection();
          if (sel) api.clipboardWrite(sel).catch(() => {});
          return false;
        }
        if (k === "v") {
          api
            .clipboardRead()
            .then((t) => {
              if (t && !zmodem.isActive()) term.paste(t);
            })
            .catch(() => {});
          return false;
        }
        return true;
      });
    }

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
      const MENU_H = 300;
      setCtxMenu({
        x: Math.min(e.clientX, window.innerWidth - MENU_W - 8),
        y: Math.min(e.clientY, window.innerHeight - MENU_H - 8),
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
      ro.disconnect();
      dataSub.dispose();
      binSub.dispose();
      cleanups.forEach((f) => f());
      term.dispose();
      termRef.current = null;
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
    if (status === "reconnecting" && reconnectAttempt) {
      term.write(
        `\x1b[90mReconnecting to ${peerHost ?? "remote host"} ` +
          `(attempt ${reconnectAttempt}/${reconnectMax ?? reconnectAttempt}) at ${now}...\x1b[0m\r\n`
      );
    } else if (status === "connected" && prev === "reconnecting") {
      term.write(`\x1b[32mReconnected to ${peerHost ?? "remote host"} at ${now}.\x1b[0m\r\n\r\n`);
    } else if (status === "closed" && prev === "reconnecting") {
      term.write(`\x1b[31mReconnect failed at ${now}. 右键标签页可「重新连接」。\x1b[0m\r\n`);
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

  const doDisconnect = useCallback(() => {
    setCtxMenu(null);
    onDisconnect?.();
  }, [onDisconnect]);

  const doFind = useCallback(() => {
    setCtxMenu(null);
    setFindOpen(true);
  }, []);

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
      />
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
              <span className="ctx-shortcut">Alt+C</span>
            </button>
            <div className="ctx-sep" />
            <button className="ctx-item" onClick={doCopy}>
              复制
              <span className="ctx-shortcut">Ctrl+Shift+C</span>
            </button>
            <button className="ctx-item" onClick={doPaste}>
              粘贴
              <span className="ctx-shortcut">Ctrl+Shift+V</span>
            </button>
            <div className="ctx-sep" />
            <button className="ctx-item" onClick={doSelectAll}>
              全选
            </button>
            <button className="ctx-item" onClick={doClearScreen}>
              清屏
            </button>
            <div className="ctx-sep" />
            <button className="ctx-item" onClick={doFind}>
              查找…
              <span className="ctx-shortcut">Ctrl+Shift+F</span>
            </button>
            <button className="ctx-item" onClick={doMark}>
              标记…
              {markCount > 0 && <span className="ctx-shortcut">{markCount} 条生效</span>}
            </button>
            <button className="ctx-item" onClick={doExport}>
              导出…
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
