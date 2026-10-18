import { useCallback, useEffect, useRef, useState } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { CanvasAddon } from "@xterm/addon-canvas";
import { Unicode11Addon } from "@xterm/addon-unicode11";
import { WebLinksAddon } from "@xterm/addon-web-links";
import "@xterm/xterm/css/xterm.css";

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
import type { Theme } from "../types";

interface Props {
  tabId: string;
  active: boolean;
  /** ssh = 远端会话；local = 本机 PTY。两者共用同一套渲染与事件协议 */
  kind?: "ssh" | "local";
  theme: Theme;
  fontFamily: string;
  fontSize: number;
  lineHeight: number;
  onClosed?: (code: number) => void;
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
}: Props) {
  const isLocal = kind === "local";
  const hostRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const activeRef = useRef(active);
  const [ctxMenu, setCtxMenu] = useState<TermCtxMenu | null>(null);

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

    term.open(host);
    termRef.current = term;
    fitRef.current = fit;

    let disposed = false;
    const cleanups: Array<() => void> = [];

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
    const closedFeed = isLocal ? onLocalClosed : onTermClosed;
    dataFeed(tabId, (bytes) => {
      if (!disposed) zmodem.consume(bytes);
    }).then((u) => cleanups.push(u));

    closedFeed(tabId, (code) => {
      if (disposed) return;
      term.writeln("");
      term.writeln(
        isLocal
          ? `\x1b[90m[Shell 已退出，退出码 ${code}]\x1b[0m`
          : `\x1b[90m[会话已关闭，退出码 ${code}]\x1b[0m`
      );
      onClosed?.(code);
    }).then((u) => cleanups.push(u));

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
        if (k === "c") {
          const sel = term.getSelection();
          if (sel) navigator.clipboard?.writeText(sel).catch(() => {});
          return false;
        }
        if (k === "v") {
          navigator.clipboard
            ?.readText()
            .then((t) => {
              if (t && !zmodem.isActive()) send(t).catch(() => {});
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
      const MENU_W = 160;
      const MENU_H = 200;
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

  // 菜单动作
  const doCopy = useCallback(() => {
    const sel = termRef.current?.getSelection();
    if (sel) navigator.clipboard?.writeText(sel).catch(() => {});
    setCtxMenu(null);
  }, []);

  const doPaste = useCallback(() => {
    setCtxMenu(null);
    navigator.clipboard
      ?.readText()
      .then((t) => {
        if (t) {
          (isLocal
            ? api.localWrite(tabId, stringToBytes(t))
            : api.sshWrite(tabId, stringToBytes(t))
          ).catch(() => {});
        }
      })
      .catch(() => {});
  }, [isLocal, tabId]);

  const doSelectAll = useCallback(() => {
    termRef.current?.selectAll();
    setCtxMenu(null);
  }, []);

  const doClearScreen = useCallback(() => {
    termRef.current?.clear();
    setCtxMenu(null);
  }, []);

  const doDisconnect = useCallback(() => {
    setCtxMenu(null);
    onDisconnect?.();
  }, [onDisconnect]);

  // 点击任意位置收菜单
  useEffect(() => {
    if (!ctxMenu) return;
    const hide = () => setCtxMenu(null);
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
          <div className="ctx-menu" style={{ left: ctxMenu.x, top: ctxMenu.y }}>
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
          </div>
        </>
      )}
    </>
  );
}
