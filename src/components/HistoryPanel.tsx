/**
 * Shift+Tab 历史面板：在光标附近列出这个会话的命令历史，按当前这半行筛选。
 *
 * - 输入框预填命令行上已经敲的字，继续打字可以再筛；
 * - ↑↓ 选，Enter / Tab / 单击 = 把这条放到命令行上（不执行，回车由人按）；Esc / 点外面 = 关闭；
 * - 筛选规则见 `lib/historyPanel.searchHistory`。
 */
import { useEffect, useMemo, useRef, useState } from "react";
import { searchHistory } from "../lib/historyPanel";
import { t, useLocale } from "../i18n";

interface Props {
  /** 全部历史，最近用过的在前 */
  items: string[];
  /** 打开时命令行上已经敲的字 */
  initialQuery: string;
  /** 面板左上角相对终端容器的位置；`above` = 光标下方放不下，改放在光标行上方（此时 y 是面板底边） */
  pos: { x: number; y: number; above: boolean };
  onPick: (cmd: string) => void;
  onClose: () => void;
}

/** 把命中的关键字加粗 */
function Marked({ text, query }: { text: string; query: string }) {
  const words = query.trim().toLowerCase().split(/\s+/).filter(Boolean);
  if (!words.length) return <>{text}</>;
  const lower = text.toLowerCase();
  const hit = new Array<boolean>(text.length).fill(false);
  for (const w of words) {
    let i = lower.indexOf(w);
    while (i >= 0) {
      for (let k = i; k < i + w.length; k++) hit[k] = true;
      i = lower.indexOf(w, i + w.length);
    }
  }
  const parts: { t: string; on: boolean }[] = [];
  for (let i = 0; i < text.length; i++) {
    const last = parts[parts.length - 1];
    if (last && last.on === hit[i]) last.t += text[i];
    else parts.push({ t: text[i], on: hit[i] });
  }
  return (
    <>
      {parts.map((p, i) => (p.on ? <b key={i}>{p.t}</b> : <span key={i}>{p.t}</span>))}
    </>
  );
}

export default function HistoryPanel({ items, initialQuery, pos, onPick, onClose }: Props) {
  useLocale();
  const [query, setQuery] = useState(initialQuery);
  const [sel, setSel] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const rootRef = useRef<HTMLDivElement>(null);
  const shown = useMemo(() => searchHistory(items, query), [items, query]);

  useEffect(() => {
    const el = inputRef.current;
    if (!el) return;
    el.focus();
    // 光标放到末尾：人接着打字是在已有的关键字后面追加
    el.setSelectionRange(el.value.length, el.value.length);
  }, []);
  // 关键字变了回到第一条
  useEffect(() => setSel(0), [query]);
  // 选中项滚进视野
  useEffect(() => {
    listRef.current?.querySelector<HTMLElement>(`[data-i="${sel}"]`)?.scrollIntoView({ block: "nearest" });
  }, [sel]);
  // 点到面板外面就关
  useEffect(() => {
    const onDown = (e: MouseEvent) => {
      if (rootRef.current && !rootRef.current.contains(e.target as Node)) onClose();
    };
    window.addEventListener("mousedown", onDown, true);
    return () => window.removeEventListener("mousedown", onDown, true);
  }, [onClose]);

  const pick = (i: number) => {
    const cmd = shown[i];
    if (cmd !== undefined) onPick(cmd);
  };

  return (
    <div
      ref={rootRef}
      className="hist-panel"
      style={pos.above ? { left: pos.x, bottom: pos.y } : { left: pos.x, top: pos.y }}
      role="dialog"
      aria-label={t("命令历史", "Command history")}
    >
      <input
        ref={inputRef}
        className="hist-search"
        value={query}
        placeholder={t("筛选历史命令…", "Filter command history…")}
        spellCheck={false}
        onChange={(e) => setQuery(e.target.value)}
        onKeyDown={(e) => {
          if (e.nativeEvent.isComposing) return;
          if (e.key === "ArrowDown") {
            e.preventDefault();
            setSel((i) => Math.min(i + 1, Math.max(shown.length - 1, 0)));
          } else if (e.key === "ArrowUp") {
            e.preventDefault();
            setSel((i) => Math.max(i - 1, 0));
          } else if (e.key === "PageDown") {
            e.preventDefault();
            setSel((i) => Math.min(i + 10, Math.max(shown.length - 1, 0)));
          } else if (e.key === "PageUp") {
            e.preventDefault();
            setSel((i) => Math.max(i - 10, 0));
          } else if (e.key === "Enter" || (e.key === "Tab" && !e.shiftKey)) {
            e.preventDefault();
            pick(sel);
          } else if (e.key === "Escape" || (e.key === "Tab" && e.shiftKey)) {
            // 再按一次 Shift+Tab 也当关闭
            e.preventDefault();
            onClose();
          }
        }}
      />
      <div ref={listRef} className="hist-list" role="listbox">
        {shown.length === 0 ? (
          <div className="hist-empty">{items.length === 0
              ? t("这个会话还没有历史命令", "No command history for this session yet")
              : t("没有匹配的历史命令", "No matching commands")}</div>
        ) : (
          shown.map((cmd, i) => (
            <div
              key={`${i}:${cmd}`}
              data-i={i}
              role="option"
              aria-selected={i === sel}
              className={`hist-item${i === sel ? " on" : ""}`}
              title={cmd}
              onMouseEnter={() => setSel(i)}
              onMouseDown={(e) => {
                // 别让输入框失焦再触发外部点击关闭
                e.preventDefault();
                pick(i);
              }}
            >
              <Marked text={cmd} query={query} />
            </div>
          ))
        )}
      </div>
      <div className="hist-foot">{t("↑↓ 选择 · Enter 放到命令行 · Esc 关闭 · {n} 条", "↑↓ select · Enter puts it on the command line · Esc close · {n}", { n: shown.length })}</div>
    </div>
  );
}
