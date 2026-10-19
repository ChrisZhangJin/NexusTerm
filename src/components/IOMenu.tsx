import { useEffect, useRef, useState } from "react";

interface Props {
  canExportSession: boolean;
  onImport: () => void;
  onExportSession: () => void;
  onExportAll: () => void;
}

/**
 * 导入 / 导出下拉菜单。
 *
 * 菜单项按数据流向分组：上半是「进」（导入），下半是「出」（导出）。
 * 导出单会话依赖侧栏选中项，无选中时置灰并提示。
 */
export default function IOMenu({
  canExportSession,
  onImport,
  onExportSession,
  onExportAll,
}: Props) {
  const [open, setOpen] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);

  // 点击菜单外自动收起
  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (rootRef.current && !rootRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", onDown);
    return () => document.removeEventListener("mousedown", onDown);
  }, [open]);

  const pick = (fn: () => void) => () => {
    setOpen(false);
    fn();
  };

  return (
    <div className="io-menu" ref={rootRef}>
      <button
        className={`tb-action ${open ? "active" : ""}`}
        onClick={() => setOpen((v) => !v)}
        title="导入 Xshell 会话，或把现有会话导出为 .xsh"
      >
        <span className="tb-glyph">⇄</span>
        导入/导出
      </button>

      {open && (
        <div className="io-dropdown" role="menu">
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
            title={canExportSession ? "" : "先在左侧选中一个会话"}
            onClick={pick(onExportSession)}
          >
            <span className="io-glyph">⤒</span>
            <span className="io-text">
              导出选中会话为 .xsh…
              <small>不包含密码</small>
            </span>
          </button>

          <button className="io-item" role="menuitem" onClick={pick(onExportAll)}>
            <span className="io-glyph">⤒</span>
            <span className="io-text">
              导出全部会话到文件夹…
              <small>每个会话一个 .xsh 文件</small>
            </span>
          </button>
        </div>
      )}
    </div>
  );
}
