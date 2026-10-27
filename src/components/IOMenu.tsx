import { useEffect, useRef, useState } from "react";

interface Props {
  canExportSession: boolean;
  /** 「导出当前会话」置灰时的说明 */
  exportSessionHint?: string;
  onImport: () => void;
  onExportSession: () => void;
  onExportAll: () => void;
}

/**
 * 导入 / 导出菜单。用 position: fixed 按按钮位置定位 —— 连接列表弹窗是 overflow: hidden，
 * absolute 会被裁掉；菜单 DOM 仍在弹窗内部，所以点菜单不会触发弹窗的「点外部关闭」。
 */
export default function IOMenu({
  canExportSession,
  exportSessionHint,
  onImport,
  onExportSession,
  onExportAll,
}: Props) {
  const [pos, setPos] = useState<{ x: number; y: number } | null>(null);
  const rootRef = useRef<HTMLDivElement>(null);
  const btnRef = useRef<HTMLButtonElement>(null);
  const open = pos !== null;

  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (rootRef.current && !rootRef.current.contains(e.target as Node)) setPos(null);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation(); // 只收菜单，不连带关掉连接列表
        setPos(null);
      }
    };
    document.addEventListener("mousedown", onDown);
    window.addEventListener("keydown", onKey, true);
    return () => {
      document.removeEventListener("mousedown", onDown);
      window.removeEventListener("keydown", onKey, true);
    };
  }, [open]);

  const toggle = () => {
    if (open) return setPos(null);
    const r = btnRef.current?.getBoundingClientRect();
    if (!r) return;
    const W = 300;
    setPos({ x: Math.max(8, Math.min(r.left, window.innerWidth - W - 8)), y: r.bottom + 6 });
  };

  const pick = (fn: () => void) => () => {
    setPos(null);
    fn();
  };

  return (
    <div className="io-menu" ref={rootRef}>
      <button
        ref={btnRef}
        className={`btn-icon io-btn${open ? " active" : ""}`}
        onClick={toggle}
        title="导入 / 导出（Xshell .xsh）"
        aria-haspopup="menu"
        aria-expanded={open}
      >
        <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden>
          <path
            d="M5 2v9M2.5 8.5 5 11l2.5-2.5M11 14V5M8.5 7.5 11 5l2.5 2.5"
            fill="none"
            stroke="currentColor"
            strokeWidth="1.4"
            strokeLinecap="round"
            strokeLinejoin="round"
          />
        </svg>
      </button>

      {pos && (
        <div className="io-dropdown" role="menu" style={{ left: pos.x, top: pos.y }}>
          <button className="io-item" role="menuitem" onClick={pick(onImport)}>
            <span className="io-glyph">⤓</span>
            <span className="io-text">
              导入 Xshell 会话 (.xsh)…
              <small>支持多选；同机同用户的密码会自动解密导入</small>
            </span>
          </button>

          <div className="io-sep" />

          <button
            className="io-item"
            role="menuitem"
            disabled={!canExportSession}
            title={canExportSession ? "" : exportSessionHint ?? ""}
            onClick={pick(onExportSession)}
          >
            <span className="io-glyph">⤒</span>
            <span className="io-text">
              导出当前标签的会话为 .xsh…
              <small>{canExportSession ? "不包含密码" : exportSessionHint ?? "不包含密码"}</small>
            </span>
          </button>

          <button className="io-item" role="menuitem" onClick={pick(onExportAll)}>
            <span className="io-glyph">⤒</span>
            <span className="io-text">
              导出全部会话到文件夹…
              <small>每个会话一个 .xsh 文件；单个连接/分组可在列表里右键导出</small>
            </span>
          </button>
        </div>
      )}
    </div>
  );
}
