import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { marked } from "marked";
import DOMPurify from "dompurify";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { api, base64ToBytes, type ViewerDocPayload } from "../api";
import { BUILTIN_THEMES, DEFAULT_THEME } from "../lib/theme";
import { applyChromeVars } from "../lib/chrome";
import {
  audioMime,
  codeLang,
  imageMime,
  isJson,
  isSvg,
  kindLabel,
  videoMime,
} from "../lib/viewable";
import ImageView from "./ImageView";
import AudioView from "./AudioView";
import VideoView from "./VideoView";
import CodeView from "./CodeView";
import { t, useLocale } from "../i18n";
import "./viewer.css";

/**
 * 浏览窗口：渲染 .md / .html；查看图片；播放音频 / 视频；高亮显示文本与配置文件。
 * - Markdown：marked → DOMPurify 净化后直接插入（不执行任何脚本，外链在系统浏览器打开）。
 * - HTML：sandbox iframe，src 走 nxdoc 协议（后端带 CSP 头）。
 *   默认禁脚本；勾选「允许脚本」后改 sandbox + 换 URL 重新加载。
 *   无论哪种都不给 allow-same-origin：页面是不透明源，碰不到应用的 IPC 与存储。
 */

type Doc = Omit<ViewerDocPayload, "data" | "refreshError"> & { bytes: Uint8Array };

function toDoc({ name, source, kind, revision, data }: ViewerDocPayload): Doc {
  return { name, source, kind, revision, bytes: base64ToBytes(data) };
}

/** UTF-8 优先；非法时回退 GBK（国内老文档常见） */
function decode(bytes: Uint8Array): { text: string; enc: string; lossy?: boolean } {
  try {
    return { text: new TextDecoder("utf-8", { fatal: true }).decode(bytes), enc: "UTF-8" };
  } catch {
    try {
      return { text: new TextDecoder("gbk").decode(bytes), enc: "GBK" };
    } catch {
      return { text: new TextDecoder("utf-8").decode(bytes), enc: "UTF-8", lossy: true };
    }
  }
}

/** iframe 的地址：Windows/Android 上自定义协议走 http://<scheme>.localhost，其它平台用 <scheme>:// */
function docUrl(id: string, js: boolean, revision: number) {
  const win = /Windows|Android/i.test(navigator.userAgent);
  const base = win ? "http://nxdoc.localhost/" : "nxdoc://localhost/";
  return `${base}${encodeURIComponent(id)}?v=${revision}${js ? "&js=1" : ""}`;
}

async function openExternal(href: string) {
  try {
    const { openUrl } = await import("@tauri-apps/plugin-opener");
    await openUrl(href);
  } catch {
    /* 打不开就算了 */
  }
}

export default function Viewer({ id }: { id: string }) {
  const lang = useLocale();
  const [doc, setDoc] = useState<Doc | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [refreshError, setRefreshError] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const refreshPending = useRef(false);
  const initialLoad = useRef<{ id: string; doc: Promise<ViewerDocPayload> } | null>(null);
  const [mode, setMode] = useState<"render" | "source">("render");
  const [allowJs, setAllowJs] = useState(false);
  const [zoom, setZoom] = useState(() => Number(localStorage.getItem("nx.viewer.zoom")) || 100);
  const mdRef = useRef<HTMLDivElement>(null);
  // 全屏：窗口铺满屏幕，Markdown 纸面同时取消 920px 宽度上限，宽表格才能完整显示
  const [fullscreen, setFullscreen] = useState(false);
  const [imgInfo, setImgInfo] = useState("");
  const [toast, setToast] = useState<string | null>(null);
  const toastTimer = useRef<number | undefined>(undefined);
  const notify = useCallback((m: string) => {
    setToast(m);
    window.clearTimeout(toastTimer.current);
    toastTimer.current = window.setTimeout(() => setToast(null), 1800);
  }, []);

  // 外壳配色跟随主窗口主题
  useEffect(() => {
    const name = localStorage.getItem("nx.theme") ?? DEFAULT_THEME;
    applyChromeVars(BUILTIN_THEMES[name] ?? BUILTIN_THEMES[DEFAULT_THEME]);
  }, []);

  useEffect(() => {
    // StrictMode 会重跑 effect；同一个页面只发一次加载请求（真正重载页面会重新取文档）。
    if (initialLoad.current?.id !== id) initialLoad.current = { id, doc: api.viewerDoc(id) };
    let active = true;
    initialLoad.current.doc
      .then((d) => {
        if (!active) return;
        initialLoad.current = null; // 不保留已解码文档的 base64 副本（尤其是音视频）
        setDoc(toDoc(d));
        setRefreshError(d.refreshError ?? null);
      })
      .catch((e) => {
        if (!active) return;
        initialLoad.current = null;
        setErr(String(e));
      });
    return () => { active = false; };
  }, [id]);

  const isHtml = doc?.kind === "html";
  const refresh = useCallback(async () => {
    if (!isHtml || refreshPending.current) return;
    refreshPending.current = true;
    setRefreshing(true);
    setRefreshError(null);
    try {
      setDoc(toDoc(await api.viewerRefresh(id)));
    } catch (e) {
      setRefreshError(String(e));
    } finally {
      refreshPending.current = false;
      setRefreshing(false);
    }
  }, [id, isHtml]);

  useEffect(() => {
    if (!isHtml) return;
    // 仅接收外壳快捷键；不尝试访问隔离 iframe 的键盘事件。
    const key = (e: KeyboardEvent) => {
      if (e.defaultPrevented || e.isComposing || e.altKey || e.shiftKey) return;
      const f5 = e.key === "F5" && !e.ctrlKey && !e.metaKey;
      const modR = e.key.toLowerCase() === "r" && (e.ctrlKey || e.metaKey);
      if (!f5 && !modR) return;
      e.preventDefault();
      if (!e.repeat) void refresh();
    };
    window.addEventListener("keydown", key);
    return () => window.removeEventListener("keydown", key);
  }, [isHtml, refresh]);

  // 窗口标题跟随语言切换
  const docName = doc?.name;
  useEffect(() => {
    if (docName !== undefined) document.title = t("浏览 · {name}", "View · {name}", { name: docName });
  }, [docName, lang]);

  useEffect(() => {
    localStorage.setItem("nx.viewer.zoom", String(zoom));
  }, [zoom]);

  // 图片（除 SVG 源码视图外）/ 音频 / 视频都是二进制内容，不需要解码成文本
  const decoded = useMemo(() => {
    if (!doc) return null;
    if (doc.kind === "audio" || doc.kind === "video") return { text: "", enc: "" };
    if (doc.kind === "image" && !isSvg(doc.name)) return { text: "", enc: "" };
    return decode(doc.bytes);
  }, [doc]);

  const mdHtml = useMemo(() => {
    if (!doc || doc.kind !== "markdown" || !decoded) return "";
    const raw = marked.parse(decoded.text, { gfm: true, breaks: false, async: false }) as string;
    return DOMPurify.sanitize(raw, {
      USE_PROFILES: { html: true },
      // input 保留：GFM 任务列表的 [x] 复选框（marked 生成的是 disabled）
      FORBID_TAGS: ["style", "form", "button", "iframe", "object", "embed"],
    });
  }, [doc, decoded]);

  // Markdown 里的链接：锚点在页内滚动，外链交给系统浏览器
  useEffect(() => {
    const el = mdRef.current;
    if (!el) return;
    const onClick = (e: MouseEvent) => {
      const a = (e.target as HTMLElement).closest("a");
      if (!a) return;
      const href = a.getAttribute("href") ?? "";
      e.preventDefault();
      if (href.startsWith("#")) {
        const target = el.querySelector(`[id="${CSS.escape(decodeURIComponent(href.slice(1)))}"]`);
        target?.scrollIntoView({ behavior: "smooth", block: "start" });
      } else if (/^(https?:|mailto:)/i.test(href)) {
        void openExternal(href);
      }
    };
    el.addEventListener("click", onClick);
    return () => el.removeEventListener("click", onClick);
  }, [mdHtml, mode]);

  const toggleFullscreen = useCallback((to?: boolean) => {
    const win = getCurrentWindow();
    void win
      .isFullscreen()
      .then((cur) => win.setFullscreen(to ?? !cur))
      .then(() => win.isFullscreen())
      .then(setFullscreen)
      .catch(() => {});
  }, []);

  // F11 切换，Esc 退出；窗口被系统手势（如 macOS 绿色按钮）改了全屏状态时同步回来
  useEffect(() => {
    const key = (e: KeyboardEvent) => {
      if (e.key === "F11") {
        e.preventDefault();
        toggleFullscreen();
      } else if (e.key === "Escape" && fullscreen) {
        toggleFullscreen(false);
      }
    };
    window.addEventListener("keydown", key);
    const win = getCurrentWindow();
    let unlisten: (() => void) | undefined;
    void win
      .onResized(() => void win.isFullscreen().then(setFullscreen).catch(() => {}))
      .then((f) => {
        unlisten = f;
      })
      .catch(() => {});
    return () => {
      window.removeEventListener("keydown", key);
      unlisten?.();
    };
  }, [fullscreen, toggleFullscreen]);

  // Ctrl + 滚轮 / Ctrl +/- / Ctrl 0 缩放
  const isImage = doc?.kind === "image";
  const isAudio = doc?.kind === "audio";
  const isVideo = doc?.kind === "video";
  useEffect(() => {
    if (isImage || isAudio || isVideo) return; // 图片查看器自己处理缩放；音视频不需要字体缩放
    const clamp = (z: number) => Math.min(250, Math.max(50, z));
    const key = (e: KeyboardEvent) => {
      if (!(e.ctrlKey || e.metaKey)) return;
      if (e.key === "=" || e.key === "+") setZoom((z) => clamp(z + 10));
      else if (e.key === "-") setZoom((z) => clamp(z - 10));
      else if (e.key === "0") setZoom(100);
      else return;
      e.preventDefault();
    };
    const wheel = (e: WheelEvent) => {
      if (!e.ctrlKey) return;
      e.preventDefault();
      setZoom((z) => clamp(z + (e.deltaY < 0 ? 10 : -10)));
    };
    window.addEventListener("keydown", key);
    window.addEventListener("wheel", wheel, { passive: false });
    return () => {
      window.removeEventListener("keydown", key);
      window.removeEventListener("wheel", wheel);
    };
  }, [isImage, isAudio, isVideo]);

  if (err) {
    return (
      <div className="rd-root">
        <div className="rd-error">{t("无法打开：", "Can't open: ")}{err}</div>
      </div>
    );
  }
  if (!doc || !decoded) {
    return (
      <div className="rd-root">
        <div className="rd-loading">{t("加载中…", "Loading…")}</div>
      </div>
    );
  }

  const isCode = doc.kind === "code";
  // 源码切换：md / html / svg 有意义；配置文件本身就是源码，位图没有源码
  const hasSource = doc.kind === "markdown" || isHtml || (isImage && isSvg(doc.name));
  const viewMode = hasSource ? mode : "render";
  const size =
    doc.bytes.length < 1024
      ? `${doc.bytes.length} B`
      : doc.bytes.length < 1024 * 1024
        ? `${(doc.bytes.length / 1024).toFixed(1)} KB`
        : `${(doc.bytes.length / 1024 / 1024).toFixed(1)} MB`;

  return (
    <div className="rd-root">
      <div className={`rd-bar${isHtml ? " rd-bar-html" : ""}`}>
        <span className={`rd-badge rd-${doc.kind}`}>{kindLabel(doc.name, doc.kind)}</span>
        <span className="rd-source" title={doc.source}>
          {doc.source}
        </span>
        <span className="rd-meta">
          {size}
          {isImage ? (imgInfo ? ` · ${imgInfo}` : "") : isAudio || isVideo ? "" : ` · ${decoded.lossy ? t("UTF-8(容错)", "UTF-8 (lossy)") : decoded.enc}`}
        </span>
        <span className="rd-spacer" />
        {isHtml && (
          <button
            className="rd-refresh"
            disabled={refreshing}
            onClick={() => void refresh()}
            title={t("重读原文件 (F5 / Ctrl+R / ⌘R)", "Read the original file again (F5 / Ctrl+R / ⌘R)")}
          >
            {refreshing ? t("刷新中…", "Refreshing…") : t("刷新", "Refresh")}
          </button>
        )}
        {isHtml && viewMode === "render" && (
          <label
            className="rd-js"
            title={t(
              "页面在隔离沙箱中运行，无法访问本应用；只在信任该文件时开启",
              "The page runs in an isolated sandbox and can't access this app. Only enable if you trust this file."
            )}
          >
            <input type="checkbox" checked={allowJs} onChange={(e) => setAllowJs(e.target.checked)} />
            {t("允许脚本", "Allow scripts")}
          </label>
        )}
        {hasSource && (
          <div className="rd-seg">
            <button className={mode === "render" ? "on" : ""} onClick={() => setMode("render")}>
              {isImage ? t("图像", "Image") : t("渲染", "Rendered")}
            </button>
            <button className={mode === "source" ? "on" : ""} onClick={() => setMode("source")}>
              {t("源码", "Source")}
            </button>
          </div>
        )}
        {!((isImage || isAudio || isVideo) && viewMode === "render") && (
        <div className="rd-zoom">
          <button onClick={() => setZoom((z) => Math.max(50, z - 10))} title={t("缩小 (Ctrl -)", "Zoom out (Ctrl -)")}>
            −
          </button>
          <button className="rd-zoom-val" onClick={() => setZoom(100)} title={t("还原 (Ctrl 0)", "Reset zoom (Ctrl 0)")}>
            {zoom}%
          </button>
          <button onClick={() => setZoom((z) => Math.min(250, z + 10))} title={t("放大 (Ctrl +)", "Zoom in (Ctrl +)")}>
            +
          </button>
        </div>
        )}
        <button
          className={`rd-full${fullscreen ? " on" : ""}`}
          onClick={() => toggleFullscreen()}
          title={
            fullscreen
              ? t("退出全屏 (Esc / F11)", "Exit full screen (Esc / F11)")
              : t("全屏 (F11)：宽表格可完整显示", "Full screen (F11): shows wide tables in full")
          }
        >
          {fullscreen ? t("退出全屏", "Exit full screen") : t("全屏", "Full screen")}
        </button>
      </div>

      {refreshError !== null && (
        <div className="rd-refresh-error" role="alert">
          {t("无法刷新：", "Can't refresh: ")}{refreshError}
        </div>
      )}
      <div className="rd-body">
        {viewMode === "source" ? (
          <pre className="rd-src" style={{ fontSize: `${(13 * zoom) / 100}px` }}>
            {decoded.text}
          </pre>
        ) : isImage ? (
          <ImageView bytes={doc.bytes} mime={imageMime(doc.name)} onInfo={setImgInfo} />
        ) : isAudio ? (
          <AudioView bytes={doc.bytes} mime={audioMime(doc.name)} />
        ) : isVideo ? (
          <VideoView bytes={doc.bytes} mime={videoMime(doc.name)} />
        ) : isCode ? (
          <CodeView
            text={decoded.text}
            lang={codeLang(doc.name)}
            isJson={isJson(doc.name)}
            zoom={zoom}
            onNotify={notify}
          />
        ) : isHtml ? (
          <div className="rd-frame-wrap">
            <iframe
              key={`${doc.revision}-${allowJs}`}
              className="rd-frame"
              title={doc.name}
              src={docUrl(id, allowJs, doc.revision)}
              sandbox={allowJs ? "allow-scripts allow-popups" : "allow-popups"}
              referrerPolicy="no-referrer"
              style={{
                width: `${10000 / zoom}%`,
                height: `${10000 / zoom}%`,
                transform: `scale(${zoom / 100})`,
              }}
            />
          </div>
        ) : (
          <div className="rd-scroll">
            <article
              ref={mdRef}
              className={`rd-md${fullscreen ? " rd-wide" : ""}`}
              style={{ fontSize: `${(15 * zoom) / 100}px` }}
              dangerouslySetInnerHTML={{ __html: mdHtml }}
            />
          </div>
        )}
        {toast && <div className="rd-toast">{toast}</div>}
      </div>
    </div>
  );
}
