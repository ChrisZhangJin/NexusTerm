import { useEffect, useRef, useState } from "react";

/**
 * 应用内确认框。WebView2 下 window.confirm 不可靠（可能被直接吞掉返回 false），
 * 所以所有确认都走这个组件。
 */
export interface ConfirmOptions {
  title: string;
  message: string;
  /** 次要说明（灰字） */
  detail?: string;
  okText?: string;
  danger?: boolean;
  /** 给了就显示「不再询问」复选框，勾选后回调收到 true */
  dontAskLabel?: string;
}

export default function ConfirmDialog({
  opts,
  onResult,
}: {
  opts: ConfirmOptions;
  onResult: (ok: boolean, dontAsk: boolean) => void;
}) {
  const [dontAsk, setDontAsk] = useState(false);
  const okRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    okRef.current?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onResult(false, false);
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onResult]);

  return (
    <div className="modal-backdrop" onMouseDown={() => onResult(false, false)}>
      <div className="modal confirm-modal" onMouseDown={(e) => e.stopPropagation()}>
        <h2>{opts.title}</h2>
        <p className="confirm-msg">{opts.message}</p>
        {opts.detail && <p className="confirm-detail">{opts.detail}</p>}
        <div className="modal-actions">
          {opts.dontAskLabel && (
            <label className="confirm-dontask">
              <input
                type="checkbox"
                checked={dontAsk}
                onChange={(e) => setDontAsk(e.target.checked)}
              />
              {opts.dontAskLabel}
            </label>
          )}
          <button className="btn" onClick={() => onResult(false, false)}>
            取消
          </button>
          <button
            ref={okRef}
            className={`btn primary${opts.danger ? " danger" : ""}`}
            onClick={() => onResult(true, dontAsk)}
          >
            {opts.okText ?? "确定"}
          </button>
        </div>
      </div>
    </div>
  );
}
