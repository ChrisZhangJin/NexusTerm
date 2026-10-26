import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { marked } from "marked";
import DOMPurify from "dompurify";
import { api, base64ToBytes } from "../api";
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
  type ViewableKind,
} from "../lib/viewable";
import ImageView from "./ImageView";
import AudioView from "./AudioView";
import VideoView from "./VideoView";
import CodeView from "./CodeView";
import "./viewer.css";

/**
 * 浏览窗口：渲染 .md / .html；查看图片；播放音频 / 视频；高亮显示文本与配置文件。
 * - Markdown：marked → DOMPurify 净化后直接插入（不执行任何脚本，外链在系统浏览器打开）。
 * - HTML：sandbox iframe，src 走 nxdoc 协议（后端带 CSP 头）。
 *   默认禁脚本；勾选「允许脚本」后改 sandbox + 换 URL 重新加载。
 *   无论哪种都不给 allow-same-origin：页面是不透明源，碰不到应用的 IPC 与存储。
 */

type Doc = { name: string; source: string; kind: ViewableKind; bytes: Uint8Array };

/** UTF-8 优先；非法时回退 GBK（国内老文档常见） */
function decode(bytes: Uint8Array): { text: string; enc: string } {
  try {
    return { text: new TextDecoder("utf-8", { fatal: true }).decode(bytes), enc: "UTF-8" };
  } catch {
    try {
      return { text: new TextDecoder("gbk").decode(bytes), enc: "GBK" };
    } catch {
      return { text: new TextDecoder("utf-8").decode(bytes), enc: "UTF-8(容错)" };
    }
  }
}

/** iframe 的地址：Windows/Android 上自定义协议走 http://<scheme>.localhost，其它平台用 <scheme>:// */
function docUrl(id: string, js: boolean) {
  const win = /Windows|Android/i.test(navigator.userAgent);
  const base = win ? "http://nxdoc.localhost/" : "nxdoc://localhost/";
  return `${base}${encodeURIComponent(id)}${js ? "?js=1" : ""}`;
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
  const [doc, setDoc] = useState<Doc | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [mode, setMode] = useState<"render" | "source">("render");
  const [allowJs, setAllowJs] = useState(false);
  const [zoom, setZoom] = useState(() => Number(localStorage.getItem("nx.viewer.zoom")) || 100);
  const mdRef = useRef<HTMLDivElement>(null);
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
    api
      .viewerDoc(id)
      .then((d) => {
        setDoc({ ...d, bytes: base64ToBytes(d.data) });
        document.title = `浏览 · ${d.name}`;
      })
      .catch((e) => setErr(String(e)));
  }, [id]);

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
        <div className="rd-error">无法打开：{err}</div>
      </div>
    );
  }
  if (!doc || !decoded) {
    return (
      <div className="rd-root">
        <div className="rd-loading">加载中…</div>
      </div>
    );
  }

  const isHtml = doc.kind === "html";
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
      <div className="rd-bar">
        <span className={`rd-badge rd-${doc.kind}`}>{kindLabel(doc.name, doc.kind)}</span>
        <span className="rd-source" title={doc.source}>
          {doc.source}
        </span>
        <span className="rd-meta">
          {size}
          {isImage ? (imgInfo ? ` · ${imgInfo}` : "") : isAudio || isVideo ? "" : ` · ${decoded.enc}`}
        </span>
        <span className="rd-spacer" />
        {isHtml && viewMode === "render" && (
          <label
            className="rd-js"
            title="页面在隔离沙箱中运行，无法访问本应用；只在信任该文件时开启"
          >
            <input type="checkbox" checked={allowJs} onChange={(e) => setAllowJs(e.target.checked)} />
            允许脚本
          </label>
        )}
        {hasSource && (
          <div className="rd-seg">
            <button className={mode === "render" ? "on" : ""} onClick={() => setMode("render")}>
              {isImage ? "图像" : "渲染"}
            </button>
            <button className={mode === "source" ? "on" : ""} onClick={() => setMode("source")}>
              源码
            </button>
          </div>
        )}
        {!((isImage || isAudio || isVideo) && viewMode === "render") && (
        <div className="rd-zoom">
          <button onClick={() => setZoom((z) => Math.max(50, z - 10))} title="缩小 (Ctrl -)">
            −
          </button>
          <button className="rd-zoom-val" onClick={() => setZoom(100)} title="还原 (Ctrl 0)">
            {zoom}%
          </button>
          <button onClick={() => setZoom((z) => Math.min(250, z + 10))} title="放大 (Ctrl +)">
            +
          </button>
        </div>
        )}
      </div>

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
              key={allowJs ? "js" : "nojs"}
              className="rd-frame"
              title={doc.name}
              src={docUrl(id, allowJs)}
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
              className="rd-md"
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
