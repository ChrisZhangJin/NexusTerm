import { useEffect, useRef, useState } from "react";
import {
  DEFAULT_SHORTCUTS,
  SHORTCUT_LABELS,
  SHORTCUT_ORDER,
  comboFromEvent,
  formatCombo,
  setPrefs,
  setShortcuts,
  usePrefs,
  useShortcuts,
  validateCombo,
  type ShortcutAction,
  type Shortcuts,
} from "../lib/prefs";
import type { FONT_FAMILIES as FontList } from "../lib/theme";

/** Esc 关闭 —— 两个窗口通用 */
function useEsc(onClose: () => void, enabled = true) {
  useEffect(() => {
    if (!enabled) return;
    const h = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        onClose();
      }
    };
    window.addEventListener("keydown", h);
    return () => window.removeEventListener("keydown", h);
  }, [onClose, enabled]);
}

// ——— 外观 ———

interface AppearanceProps {
  themeName: string;
  themeNames: string[];
  onTheme: (n: string) => void;
  fontFamily: string;
  fonts: typeof FontList;
  onFontFamily: (f: string) => void;
  fontSize: number;
  onFontSize: (n: number) => void;
  lineHeight: number;
  onLineHeight: (n: number) => void;
  onClose: () => void;
}

export function AppearanceDialog(p: AppearanceProps) {
  const prefs = usePrefs();
  useEsc(p.onClose);
  return (
    <div className="modal-backdrop" onMouseDown={p.onClose}>
      <div className="modal settings-modal" onMouseDown={(e) => e.stopPropagation()}>
        <div className="settings-head">
          <h2>外观</h2>
          <button className="settings-x" onClick={p.onClose} title="关闭（Esc）">
            ×
          </button>
        </div>

        <div className="settings-section">终端</div>
        <div className="row">
          <label>配色</label>
          <select value={p.themeName} onChange={(e) => p.onTheme(e.target.value)}>
            {p.themeNames.map((n) => (
              <option key={n} value={n}>
                {n}
              </option>
            ))}
          </select>
        </div>
        <div className="row">
          <label>字体</label>
          <select value={p.fontFamily} onChange={(e) => p.onFontFamily(e.target.value)}>
            {p.fonts.map((f) => (
              <option key={f.value} value={f.value}>
                {f.label}
              </option>
            ))}
          </select>
        </div>
        <div className="row">
          <label>字号</label>
          <input
            type="range"
            min={10}
            max={22}
            value={p.fontSize}
            onChange={(e) => p.onFontSize(Number(e.target.value))}
          />
          <span className="settings-val">{p.fontSize}px</span>
        </div>
        <div className="row">
          <label>行高</label>
          <input
            type="range"
            min={1}
            max={2}
            step={0.05}
            value={p.lineHeight}
            onChange={(e) => p.onLineHeight(Number(e.target.value))}
          />
          <span className="settings-val">{p.lineHeight.toFixed(2)}</span>
        </div>
        <div
          className="welcome-preview settings-preview"
          style={{
            fontFamily: p.fontFamily,
            fontSize: p.fontSize,
            lineHeight: Math.max(1, p.lineHeight),
          }}
        >
          <span className="wp-dim">chris@server:~$ </span>
          <span className="wp-cmd">ssh prod-server</span>
          {"\n"}
          <span className="wp-dim">
            {p.fontSize}px / 行高 {p.lineHeight.toFixed(2)} · 中文与 emoji 宽度已启用
          </span>
        </div>

        <div className="settings-section">终端行为</div>
        <label className="settings-check">
          <input
            type="checkbox"
            checked={prefs.copyOnSelect}
            onChange={(e) => setPrefs({ copyOnSelect: e.target.checked })}
          />
          <span>
            选中即复制
            <em>鼠标选中文本后自动写入剪贴板</em>
          </span>
        </label>
        <label className="settings-check">
          <input
            type="checkbox"
            checked={prefs.confirmCloseTab}
            onChange={(e) => setPrefs({ confirmCloseTab: e.target.checked })}
          />
          <span>
            关闭标签前确认
            <em>会话仍在连接时，关闭标签先弹窗询问</em>
          </span>
        </label>

        <div className="settings-section">底部栏</div>
        <label className="settings-check">
          <input
            type="checkbox"
            checked={prefs.showResources}
            onChange={(e) => setPrefs({ showResources: e.target.checked })}
          />
          <span>
            显示 CPU / 内存 / 磁盘
            <em>SSH 会话显示远端主机，本地终端显示本机</em>
          </span>
        </label>
        <label className={`settings-check${prefs.showResources ? "" : " disabled"}`}>
          <input
            type="checkbox"
            checked={prefs.liveMonitor}
            disabled={!prefs.showResources}
            onChange={(e) => setPrefs({ liveMonitor: e.target.checked })}
          />
          <span>
            实时监控
            <em>每 5 秒刷新并播放过渡动画；关闭后停止采集，所有动画不再展示（也可在资源区右键切换）</em>
          </span>
        </label>

        <div className="modal-actions">
          <button className="btn primary" onClick={p.onClose}>
            完成
          </button>
        </div>
      </div>
    </div>
  );
}

// ——— 快捷键 ———

export function ShortcutsDialog({ onClose }: { onClose: () => void }) {
  const saved = useShortcuts();
  const [draft, setDraft] = useState<Shortcuts>(saved);
  const [recording, setRecording] = useState<ShortcutAction | null>(null);
  const [err, setErr] = useState<{ action: ShortcutAction; msg: string } | null>(null);
  const boxRef = useRef<HTMLDivElement>(null);

  useEsc(onClose, recording === null);

  // 录制：捕获阶段拦下按键，避免被终端 / 其他全局快捷键处理
  useEffect(() => {
    if (!recording) return;
    const h = (e: KeyboardEvent) => {
      e.preventDefault();
      e.stopPropagation();
      if (e.key === "Escape" && !e.ctrlKey && !e.altKey && !e.shiftKey && !e.metaKey) {
        setRecording(null);
        setErr(null);
        return;
      }
      if ((e.key === "Backspace" || e.key === "Delete") && !e.ctrlKey && !e.altKey && !e.metaKey && !e.shiftKey) {
        setDraft((d) => ({ ...d, [recording]: "" }));
        setRecording(null);
        setErr(null);
        return;
      }
      const combo = comboFromEvent(e);
      if (!combo) return; // 只按了修饰键，继续等
      const bad = validateCombo(combo);
      if (bad) {
        setErr({ action: recording, msg: `${formatCombo(combo)}：${bad}` });
        return;
      }
      setDraft((d) => {
        const next = { ...d };
        // 冲突：同一组合键只能属于一个动作，旧的那个被清空
        for (const a of SHORTCUT_ORDER) if (a !== recording && next[a] === combo) next[a] = "";
        next[recording] = combo;
        return next;
      });
      const clash = SHORTCUT_ORDER.find((a) => a !== recording && draft[a] === combo);
      setErr(
        clash
          ? { action: recording, msg: `已从「${SHORTCUT_LABELS[clash]}」移除 ${formatCombo(combo)}` }
          : null
      );
      setRecording(null);
    };
    window.addEventListener("keydown", h, true);
    return () => window.removeEventListener("keydown", h, true);
  }, [recording, draft]);

  const dirty = SHORTCUT_ORDER.some((a) => draft[a] !== saved[a]);
  const isDefault = SHORTCUT_ORDER.every((a) => draft[a] === DEFAULT_SHORTCUTS[a]);

  const save = () => {
    setShortcuts(draft);
    onClose();
  };

  return (
    <div className="modal-backdrop" onMouseDown={() => !recording && onClose()}>
      <div
        ref={boxRef}
        className="modal settings-modal sc-modal"
        onMouseDown={(e) => e.stopPropagation()}
      >
        <div className="settings-head">
          <h2>快捷键</h2>
          <button className="settings-x" onClick={onClose} title="关闭（Esc）">
            ×
          </button>
        </div>
        <p className="mark-hint">
          点击右侧按键框后直接按下新的组合键；Backspace 清除绑定，Esc 取消录制。
          终端右键菜单里显示的快捷键会同步更新。
        </p>

        <div className="sc-list">
          {SHORTCUT_ORDER.map((a) => {
            const changed = draft[a] !== DEFAULT_SHORTCUTS[a];
            return (
              <div key={a} className={`sc-row${recording === a ? " recording" : ""}`}>
                <span className="sc-label">
                  {SHORTCUT_LABELS[a]}
                  {changed && <span className="sc-changed" title="已修改">●</span>}
                </span>
                <button
                  className={`sc-key${draft[a] ? "" : " empty"}`}
                  onClick={() => {
                    setErr(null);
                    setRecording(recording === a ? null : a);
                  }}
                  title="点击后按下新的组合键"
                >
                  {recording === a ? "请按下组合键…" : draft[a] ? formatCombo(draft[a]) : "未设置"}
                </button>
                <button
                  className="sc-reset"
                  disabled={!changed}
                  onClick={() => setDraft((d) => ({ ...d, [a]: DEFAULT_SHORTCUTS[a] }))}
                  title={`恢复默认：${DEFAULT_SHORTCUTS[a] ? formatCombo(DEFAULT_SHORTCUTS[a]) : "未设置"}`}
                >
                  ↺
                </button>
                {err?.action === a && <div className="sc-err">{err.msg}</div>}
              </div>
            );
          })}
        </div>

        <div className="modal-actions">
          <button
            className="btn"
            style={{ marginRight: "auto" }}
            disabled={isDefault}
            onClick={() => {
              setDraft({ ...DEFAULT_SHORTCUTS });
              setErr(null);
            }}
          >
            全部恢复默认
          </button>
          <button className="btn" onClick={onClose}>
            取消
          </button>
          <button className="btn primary" disabled={!dirty} onClick={save}>
            保存
          </button>
        </div>
      </div>
    </div>
  );
}
