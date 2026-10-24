import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";

/**
 * 图片查看：Blob URL + <img>（SVG 同样走 <img>，其中的脚本 / 外部资源都不会执行或加载）。
 * - 打开时「适应窗口」（小图不放大），窗口尺寸变化时若仍处于适应模式则跟着重算；
 * - 滚轮以光标为中心缩放，拖拽平移，双击在「适应」与「100%」之间切换；
 * - 背景可切棋盘格 / 暗 / 亮，便于看透明图。
 */

type Bg = "checker" | "dark" | "light";
const MIN = 0.02;
const MAX = 32;

export default function ImageView({
  bytes,
  mime,
  onInfo,
}: {
  bytes: Uint8Array;
  mime: string;
  onInfo: (info: string) => void;
}) {
  const [url, setUrl] = useState<string | null>(null);
  const [err, setErr] = useState(false);
  const [nat, setNat] = useState<{ w: number; h: number } | null>(null);
  const [view, setView] = useState({ s: 1, x: 0, y: 0 });
  const [fit, setFit] = useState(true);
  const [bg, setBg] = useState<Bg>(() => (localStorage.getItem("nx.reader.imgbg") as Bg) || "checker");
  const boxRef = useRef<HTMLDivElement>(null);
  const drag = useRef<{ x: number; y: number; vx: number; vy: number } | null>(null);
  const viewRef = useRef(view);
  viewRef.current = view;

  useEffect(() => {
    const u = URL.createObjectURL(new Blob([bytes as BlobPart], { type: mime }));
    setUrl(u);
    return () => URL.revokeObjectURL(u);
  }, [bytes, mime]);

  useEffect(() => localStorage.setItem("nx.reader.imgbg", bg), [bg]);

  /** 居中放置于给定缩放 */
  const place = useCallback(
    (s: number) => {
      const box = boxRef.current;
      if (!box || !nat) return;
      setView({ s, x: (box.clientWidth - nat.w * s) / 2, y: (box.clientHeight - nat.h * s) / 2 });
    },
    [nat],
  );

  const fitScale = useCallback(() => {
    const box = boxRef.current;
    if (!box || !nat || !nat.w || !nat.h) return 1;
    return Math.min(1, (box.clientWidth - 24) / nat.w, (box.clientHeight - 24) / nat.h);
  }, [nat]);

  // 首次 / 适应模式下随窗口尺寸重算
  useLayoutEffect(() => {
    if (!nat) return;
    if (fit) place(fitScale());
    const box = boxRef.current;
    if (!box) return;
    const ro = new ResizeObserver(() => {
      if (fit) place(fitScale());
    });
    ro.observe(box);
    return () => ro.disconnect();
  }, [nat, fit, place, fitScale]);

  useEffect(() => {
    if (nat) onInfo(`${nat.w} × ${nat.h} px`);
  }, [nat, onInfo]);

  /** 以 (cx, cy)（相对容器）为中心缩放到 s */
  const zoomAt = useCallback((s: number, cx?: number, cy?: number) => {
    const box = boxRef.current;
    if (!box) return;
    const ns = Math.min(MAX, Math.max(MIN, s));
    const px = cx ?? box.clientWidth / 2;
    const py = cy ?? box.clientHeight / 2;
    setFit(false);
    setView((v) => ({
      s: ns,
      x: px - ((px - v.x) * ns) / v.s,
      y: py - ((py - v.y) * ns) / v.s,
    }));
  }, []);

  // 滚轮缩放（不需要按 Ctrl —— 图片查看器的习惯）
  useEffect(() => {
    const box = boxRef.current;
    if (!box) return;
    const wheel = (e: WheelEvent) => {
      e.preventDefault();
      const r = box.getBoundingClientRect();
      const k = e.deltaY < 0 ? 1.15 : 1 / 1.15;
      zoomAt(viewRef.current.s * k, e.clientX - r.left, e.clientY - r.top);
    };
    box.addEventListener("wheel", wheel, { passive: false });
    return () => box.removeEventListener("wheel", wheel);
  }, [zoomAt]);

  // 键盘：Ctrl +/-/0，以及 F 适应 / 1 原始大小
  useEffect(() => {
    const key = (e: KeyboardEvent) => {
      const mod = e.ctrlKey || e.metaKey;
      if ((mod && (e.key === "=" || e.key === "+")) || (!mod && e.key === "+")) zoomAt(view.s * 1.25);
      else if (e.key === "-") zoomAt(view.s / 1.25);
      else if ((mod && e.key === "0") || e.key.toLowerCase() === "f") setFit(true);
      else if (!mod && e.key === "1") {
        setFit(false);
        place(1);
      } else return;
      e.preventDefault();
    };
    window.addEventListener("keydown", key);
    return () => window.removeEventListener("keydown", key);
  }, [view.s, zoomAt, place]);

  const onDown = (e: React.PointerEvent) => {
    if (e.button !== 0) return;
    (e.target as HTMLElement).setPointerCapture?.(e.pointerId);
    drag.current = { x: e.clientX, y: e.clientY, vx: view.x, vy: view.y };
  };
  const onMove = (e: React.PointerEvent) => {
    const d = drag.current;
    if (!d) return;
    setFit(false);
    setView((v) => ({ ...v, x: d.vx + e.clientX - d.x, y: d.vy + e.clientY - d.y }));
  };
  const onUp = () => {
    drag.current = null;
  };
  const onDouble = () => {
    if (fit || Math.abs(view.s - fitScale()) < 0.001) {
      setFit(false);
      place(1);
    } else setFit(true);
  };

  const pct = Math.round(view.s * 100);

  return (
    <div className="rd-img-root">
      <div className="rd-img-tools">
        <div className="rd-seg">
          <button className={fit ? "on" : ""} onClick={() => setFit(true)} title="适应窗口 (F)">
            适应
          </button>
          <button
            className={!fit && pct === 100 ? "on" : ""}
            onClick={() => {
              setFit(false);
              place(1);
            }}
            title="原始大小 (1)"
          >
            1:1
          </button>
        </div>
        <div className="rd-zoom">
          <button onClick={() => zoomAt(view.s / 1.25)} title="缩小 (-)">
            −
          </button>
          <span className="rd-zoom-val rd-zoom-ro">{pct}%</span>
          <button onClick={() => zoomAt(view.s * 1.25)} title="放大 (+)">
            +
          </button>
        </div>
        <div className="rd-seg" title="背景">
          {(["checker", "dark", "light"] as Bg[]).map((b) => (
            <button key={b} className={bg === b ? "on" : ""} onClick={() => setBg(b)}>
              {b === "checker" ? "▦" : b === "dark" ? "■" : "□"}
            </button>
          ))}
        </div>
        <span className="rd-img-hint">滚轮缩放 · 拖拽平移 · 双击切换适应/1:1</span>
      </div>
      <div
        ref={boxRef}
        className={`rd-img-box rd-bg-${bg}`}
        onPointerDown={onDown}
        onPointerMove={onMove}
        onPointerUp={onUp}
        onPointerCancel={onUp}
        onDoubleClick={onDouble}
      >
        {err ? (
          <div className="rd-error">图片无法解码（文件损坏，或格式不受支持）</div>
        ) : (
          url && (
            <img
              src={url}
              alt=""
              draggable={false}
              className={view.s >= 3 ? "pixelated" : ""}
              onLoad={(e) => {
                const im = e.currentTarget;
                // 无固有尺寸的 SVG（只有 viewBox）给个默认画布
                setNat({ w: im.naturalWidth || 512, h: im.naturalHeight || 512 });
              }}
              onError={() => setErr(true)}
              style={{
                width: nat ? nat.w * view.s : undefined,
                height: nat ? nat.h * view.s : undefined,
                transform: `translate(${view.x}px, ${view.y}px)`,
                visibility: nat ? "visible" : "hidden",
              }}
            />
          )
        )}
      </div>
    </div>
  );
}
