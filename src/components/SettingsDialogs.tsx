import { useEffect, useRef, useState } from "react";
import { api } from "../api";
import {
  accountLabel,
  defaultAccount,
  formatBytes,
  newAccountId,
  saveAgentConfig,
  useAgentConfig,
} from "../lib/agent";
import type {
  AgentAccount,
  AgentConfig,
  AgentExecMode,
  AgentProvider,
  SweepReport,
  VaultStatus,
} from "../types";
import MasterPasswordDialog, { type MasterPasswordMode } from "./MasterPasswordDialog";
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

/** Esc 关闭 —— 各设置窗口通用 */
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

// ——— 行为 ———

interface BehaviorProps {
  onClose: () => void;
  onOpenMacros: () => void;
}

export function BehaviorDialog(p: BehaviorProps) {
  const prefs = usePrefs();
  useEsc(p.onClose);
  return (
    <div className="modal-backdrop" onMouseDown={p.onClose}>
      <div className="modal settings-modal" onMouseDown={(e) => e.stopPropagation()}>
        <div className="settings-head">
          <h2>行为</h2>
          <button className="settings-x" onClick={p.onClose} title="关闭（Esc）">
            ×
          </button>
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
        <label className="settings-check">
          <input
            type="checkbox"
            checked={prefs.shareSftpConnection}
            onChange={(e) => setPrefs({ shareSftpConnection: e.target.checked })}
          />
          <span>
            文件面板复用已登录连接
            <em>
              同一主机开着终端标签时，文件面板直接在那条连接上开 SFTP，
              不必再输一次密码和验证码。关掉则每次单独连一条。
            </em>
          </span>
        </label>

        <div className="settings-section">宏</div>
        <div className="row">
          <label>录制与回放</label>
          <button
            className="btn"
            onClick={() => {
              p.onClose();
              p.onOpenMacros();
            }}
          >
            打开宏管理器…
          </button>
        </div>
        <p className="ag-hint">
          宏：录下一串操作（等什么、发什么），可一键重放，也能绑到某个会话上连接后自动执行。
        </p>

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

// ——— AI 助手 ———

const PROVIDER_ORDER: readonly AgentProvider[] = ["openai", "anthropic", "custom"];

/** 地址默认值要和后端 endpoint() 的拼接对得上：OpenAI 系拼 /chat/completions、Anthropic 拼 /messages，故此处都填「根地址」。 */
const PROVIDER_META: Record<
  AgentProvider,
  { label: string; url: string; urlHint: string; modelHint: string }
> = {
  openai: {
    label: "OpenAI",
    url: "https://api.openai.com/v1",
    urlHint: "",
    modelHint: "例如 gpt-4o-mini",
  },
  anthropic: {
    label: "Anthropic",
    url: "https://api.anthropic.com",
    urlHint: "",
    modelHint: "例如 claude-3-5-sonnet-latest",
  },
  custom: {
    label: "自定义",
    url: "",
    urlHint: "填 OpenAI 兼容接口地址（本地代理、网关、自建服务均可），结尾的 / 会自动去掉",
    modelHint: "按服务商文档填写",
  },
};

/** 只给四档而不是自由填数字：这个值直接决定单次请求的 token 量与花费，选档位比让用户猜 24576 和 32768 的差别安全。 */
const CONTEXT_CHOICES: readonly { bytes: number; label: string }[] = [
  { bytes: 16 * 1024, label: "16 KB · 约 5 千汉字 · 最省" },
  { bytes: 32 * 1024, label: "32 KB · 约 1.1 万汉字 · 默认" },
  { bytes: 64 * 1024, label: "64 KB · 约 2.2 万汉字" },
  { bytes: 128 * 1024, label: "128 KB · 约 4.4 万汉字 · 最完整" },
];

export function AgentDialog({ onClose }: { onClose: () => void }) {
  const saved = useAgentConfig();
  const [draft, setDraft] = useState<AgentConfig>(saved);
  const [showKey, setShowKey] = useState(false);
  const [testing, setTesting] = useState(false);
  const [testMsg, setTestMsg] = useState<{ ok: boolean; text: string } | null>(null);
  const [saveErr, setSaveErr] = useState<string | null>(null);
  /** 用户动过表单之后，后端的异步回包就不再覆盖本地草稿 */
  const editedRef = useRef(false);

  useEsc(onClose);

  // 配置是异步从后端读的，首帧是默认值：用户没编辑过就跟随真值，否则界面会停在「假」配置上，一保存就把真值冲掉。
  useEffect(() => {
    if (!editedRef.current) setDraft(saved);
  }, [saved]);

  // 当前正在编辑的账号（activeId 恒有效，由 lib/agent 的 normalize 保证；仍兜底取首个）
  const current: AgentAccount =
    draft.accounts.find((a) => a.id === draft.activeId) ?? draft.accounts[0];

  /** 改顶层配置（总开关 / 账号列表 / activeId / execMode） */
  const patch = (p: Partial<AgentConfig>) => {
    editedRef.current = true;
    setDraft((d) => ({ ...d, ...p }));
    setTestMsg(null);
  };

  /** 改当前账号的字段 */
  const patchAccount = (p: Partial<AgentAccount>) => {
    editedRef.current = true;
    setDraft((d) => ({
      ...d,
      accounts: d.accounts.map((a) => (a.id === d.activeId ? { ...a, ...p } : a)),
    }));
    setTestMsg(null);
  };

  const addAccount = () => {
    const id = newAccountId();
    const acct = defaultAccount(id, `账号 ${draft.accounts.length + 1}`);
    patch({ accounts: [...draft.accounts, acct], activeId: id });
  };

  /** 删除当前账号（至少保留一个） */
  const removeAccount = () => {
    if (draft.accounts.length <= 1) return;
    const rest = draft.accounts.filter((a) => a.id !== draft.activeId);
    patch({ accounts: rest, activeId: rest[0].id });
  };

  const dirty = JSON.stringify(draft) !== JSON.stringify(saved);

  // 与后端 validate 的后三项一致：地址 / 模型 / Key
  const missing = [
    !current.baseUrl.trim() && "接口地址",
    !current.model.trim() && "模型",
    !current.apiKey.trim() && "API Key",
  ].filter((x): x is string => typeof x === "string");
  const canTest = missing.length === 0;

  const runTest = async () => {
    if (!canTest || testing) return;
    setTesting(true);
    setTestMsg(null);
    try {
      // 后端 validate 先查 enabled、且只看 active_account：临时启用 + 只带当前账号即可单测它的连通性，不动库。
      const probe: AgentConfig = {
        ...draft,
        enabled: true,
        accounts: [current],
        activeId: current.id,
      };
      const text = await api.agentTest(probe);
      setTestMsg({ ok: true, text });
    } catch (e) {
      // 后端已把错误翻译成中文，直接展示即可
      setTestMsg({ ok: false, text: String(e ?? "测试失败") });
    } finally {
      setTesting(false);
    }
  };

  const save = async () => {
    setSaveErr(null);
    try {
      // 先写盘、后关窗：saveAgentConfig 写成功才更新内存缓存，不会出现「界面显示已保存、库里其实没变」。
      // autoAttach 恒为 true：界面已没有置 false 的入口，这里兜住旧配置里残留的 false。
      await saveAgentConfig({ ...draft, autoAttach: true });
      onClose();
    } catch (e) {
      setSaveErr(String(e ?? "保存失败"));
    }
  };

  // 库里可能存着四档之外的值（手改过 db 或旧版本遗留），补一个自定义选项免得 select 显示空白
  const contextOptions = CONTEXT_CHOICES.some((c) => c.bytes === current.maxContextBytes)
    ? CONTEXT_CHOICES
    : [
        { bytes: current.maxContextBytes, label: `${formatBytes(current.maxContextBytes)}（自定义）` },
        ...CONTEXT_CHOICES,
      ];

  return (
    <div className="modal-backdrop" onMouseDown={onClose}>
      <div className="modal settings-modal ag-modal" onMouseDown={(e) => e.stopPropagation()}>
        <div className="settings-head">
          <h2>Nexus AI Agent</h2>
          <button className="settings-x" onClick={onClose} title="关闭（Esc）">
            ×
          </button>
        </div>
        <p className="mark-hint">
          会话的输出旁路给模型，AI 因此能带着上下文回答「刚才那条命令为什么报错」。
          本页改动在点击「保存」后才生效。
        </p>

        {/* 安全提示刻意放在最上面：这是「把终端内容发给第三方」的开关，必须先看风险再填 Key */}
        <div className="ag-notice">
          <div className="ag-notice-title">
            <span className="ag-notice-icon">!</span>
            启用前请读一遍
          </div>
          <ul>
            <li>
              <strong>API Key 明文保存在本机 nexus.db</strong>
              —— 与已保存的主机密码同一套策略，不是系统钥匙串；备份或迁移数据库文件时它就包含在里面。
            </li>
            <li>
              <strong>开启后，该会话的终端输出会发往第三方模型服务</strong>
              —— 屏幕上出现过的内容都可能出网。脱敏能降低风险，但无法保证不泄露。
            </li>
            <li>
              <strong>AI 给出的命令不会自动执行</strong>
              —— 只进建议区，由你确认后才会写入终端。
            </li>
          </ul>
        </div>

        <div className="settings-section">账号</div>
        <div className="row">
          <label>当前账号</label>
          <select
            value={draft.activeId}
            onChange={(e) => patch({ activeId: e.target.value })}
          >
            {draft.accounts.map((a) => (
              <option key={a.id} value={a.id}>
                {accountLabel(a)}
              </option>
            ))}
          </select>
          <button className="ag-eye" onClick={addAccount} title="新增一个账号">
            新增
          </button>
          <button
            className="ag-eye"
            onClick={removeAccount}
            disabled={draft.accounts.length <= 1}
            title={draft.accounts.length <= 1 ? "至少保留一个账号" : "删除当前账号"}
          >
            删除
          </button>
        </div>
        <div className="row">
          <label>账号名称</label>
          <input
            value={current.name}
            onChange={(e) => patchAccount({ name: e.target.value })}
            placeholder="例如：OpenAI 正式、本地 vLLM"
            spellCheck={false}
          />
        </div>
        <p className="ag-hint">
          可配置多个账号（不同服务商 / 模型 / Key），在此下拉切换；Nexus AI Agent 使用当前选中的账号。
        </p>

        <div className="settings-section">连接</div>
        <div className="row">
          <label>服务商</label>
          <select
            value={current.provider}
            onChange={(e) => {
              const p = e.target.value as AgentProvider;
              // 换服务商时顺手把地址换成该家的默认值：留着旧地址几乎必然请求错地方
              patchAccount({ provider: p, baseUrl: PROVIDER_META[p].url });
            }}
          >
            {PROVIDER_ORDER.map((p) => (
              <option key={p} value={p}>
                {PROVIDER_META[p].label}
              </option>
            ))}
          </select>
        </div>
        <div className="row">
          <label>接口地址</label>
          <input
            value={current.baseUrl}
            onChange={(e) => patchAccount({ baseUrl: e.target.value })}
            placeholder={PROVIDER_META[current.provider].url || "https://your-host/v1"}
            spellCheck={false}
          />
        </div>
        {PROVIDER_META[current.provider].urlHint && (
          <p className="ag-hint">{PROVIDER_META[current.provider].urlHint}</p>
        )}
        <div className="row">
          <label>模型</label>
          <input
            value={current.model}
            onChange={(e) => patchAccount({ model: e.target.value })}
            placeholder={PROVIDER_META[current.provider].modelHint}
            spellCheck={false}
          />
        </div>
        <div className="row">
          <label>API Key</label>
          <input
            type={showKey ? "text" : "password"}
            value={current.apiKey}
            onChange={(e) => patchAccount({ apiKey: e.target.value })}
            placeholder="sk-…"
            autoComplete="off"
            spellCheck={false}
          />
          <button
            className="ag-eye"
            onClick={() => setShowKey((v) => !v)}
            title={showKey ? "隐藏 API Key" : "显示 API Key"}
          >
            {showKey ? "隐藏" : "显示"}
          </button>
        </div>
        <div className="ag-test-row">
          <button className="btn" disabled={!canTest || testing} onClick={runTest}>
            {testing ? "测试中…" : "测试连接"}
          </button>
          {testing && <span className="spinner sm" />}
          {testMsg ? (
            <span className={`ag-test-msg ${testMsg.ok ? "ok" : "err"}`}>{testMsg.text}</span>
          ) : (
            <span className="ag-hint ag-test-tip">
              {canTest ? "用当前账号发一条极短消息验证" : `还需填写：${missing.join("、")}`}
            </span>
          )}
        </div>

        <div className="settings-section">行为（当前账号）</div>
        <div className="ag-block">
          <span className="ag-block-label">系统提示词（留空则使用内置默认提示词）</span>
          <textarea
            className="ag-ta"
            value={current.systemPrompt}
            onChange={(e) => patchAccount({ systemPrompt: e.target.value })}
            placeholder="例如：你是资深 SRE，回答请给出可执行命令，并说明风险"
            spellCheck={false}
          />
        </div>
        <div className="row">
          <label>上下文上限</label>
          <select
            value={String(current.maxContextBytes)}
            onChange={(e) => patchAccount({ maxContextBytes: Number(e.target.value) })}
          >
            {contextOptions.map((c) => (
              <option key={c.bytes} value={String(c.bytes)}>
                {c.label}
              </option>
            ))}
          </select>
        </div>
        <p className="ag-hint">
          单次提问最多回看最近 {formatBytes(current.maxContextBytes)} 的终端输出，更早的部分会被丢弃。
        </p>
        <label className="settings-check">
          <input
            type="checkbox"
            checked={current.redact}
            onChange={(e) => patchAccount({ redact: e.target.checked })}
          />
          <span>
            出网前脱敏
            <em>
              会话输出发往模型前，抹掉密钥、token、私钥、邮箱、IP 等；关掉则发送原始输出
            </em>
          </span>
        </label>

        <div className="settings-section">总开关</div>
        <label className="settings-check">
          <input
            type="checkbox"
            checked={draft.enabled}
            onChange={(e) => patch({ enabled: e.target.checked })}
          />
          <span>
            启用 Nexus AI Agent
            <em>关闭时后端完全不采集旁路，零开销；不启用则本页其余设置都不生效</em>
          </span>
        </label>
        {/* 执行权限放在总开关区而不是某个账号下面：换模型不该悄悄换掉 Agent 在终端里的授权级别。 */}
        <div className="settings-section">执行权限</div>
        <div className="row">
          <label>执行模式</label>
          <select
            value={draft.execMode}
            disabled={!draft.enabled}
            onChange={(e) => patch({ execMode: e.target.value as AgentExecMode })}
          >
            <option value="approval">人在回路（推荐）</option>
            <option value="agentic">全自动</option>
          </select>
        </div>
        <p className="ag-hint">
          {draft.execMode === "agentic" ? (
            <>
              <strong>全自动</strong>
              ：Agent 会直接在终端会话里执行它认为必要的命令，不再逐条询问。
              只在你完全信任这台机器上的操作对象时选它。
            </>
          ) : (
            <>
              <strong>人在回路</strong>
              ：被判定为危险的命令（删除、覆写、改权限、把未知脚本管道给 shell 等）
              会先弹出卡片等你点头；只读命令照常执行。
            </>
          )}
        </p>

        {saveErr && <div className="ag-err">保存失败：{saveErr}</div>}

        <div className="modal-actions">
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

// ——— 安全（主密码保险库）———

/** 操作完成后给用户看的一句话。warn = 有东西需要用户动手补救，用警告色。 */
interface VaultReport {
  text: string;
  warn: boolean;
}

/** 「12 条凭据、2 个 API Key、3 个 TOTP 种子」——为 0 的项不列，免得报告里全是「0 个」。 */
function sweepParts(r: SweepReport): string {
  const xs = [
    `${r.credentials} 条凭据`,
    r.apiKeys > 0 ? `${r.apiKeys} 个 API Key` : null,
    r.seedsConverted > 0 ? `${r.seedsConverted} 个 TOTP 种子` : null,
  ].filter((x): x is string => x !== null);
  // 一条都没动过也要说话：静默成功会让用户怀疑按钮根本没生效
  return xs.length > 0 ? xs.join("、") : "0 条凭据";
}

/**
 * 把 `onDone` 的回值翻成人话。
 */
function describeDone(mode: MasterPasswordMode, r: SweepReport | null): VaultReport {
  if (r) {
    if (mode === "disable") {
      return {
        text: `已关闭主密码保护，${sweepParts(r)}还原成明文存回 nexus.db。`,
        warn: false,
      };
    }
    // seedsUnreadable 是唯一需要用户动手的结果：那几个种子是别的机器写的，本机解不开
    const tail =
      r.seedsUnreadable > 0
        ? ` ${r.seedsUnreadable} 个 TOTP 种子来自其它机器、解不开，需要重新填写。`
        : "";
    return { text: `已加密 ${sweepParts(r)}。${tail}`, warn: r.seedsUnreadable > 0 };
  }
  return mode === "change"
    ? { text: "主密码已修改。旧密码立即失效，请记牢新密码。", warn: false }
    : { text: "已解锁，本次运行期间凭据可正常读写。", warn: false };
}

export function SecurityDialog({ onClose }: { onClose: () => void }) {
  const [status, setStatus] = useState<VaultStatus | null>(null);
  /** null = 还在问系统凭据服务。不是 false —— 「没记住」和「还不知道」要分开显示 */
  const [remembered, setRemembered] = useState<boolean | null>(null);
  const [prompt, setPrompt] = useState<MasterPasswordMode | null>(null);
  /** 开出去的那个弹窗里「在本机记住」要不要预先勾上 */
  const [rememberDefault, setRememberDefault] = useState(false);
  const [report, setReport] = useState<VaultReport | null>(null);
  /** 每次操作完 +1，驱动下面两条 effect 重新拉状态 */
  const [tick, setTick] = useState(0);

  // 弹窗开着的时候 Escape 归它管，否则一下 Esc 会把两层一起关掉
  useEsc(onClose, prompt === null);

  // 状态是廉价的内存读，挂载即拉
  useEffect(() => {
    let alive = true;
    void (async () => {
      const s = await api.vaultStatus();
      if (alive) setStatus(s);
    })();
    return () => {
      alive = false;
    };
  }, [tick]);

  // 「本机记住」单独一条、且只在保险库启用后才问：vaultIsRemembered 会访问系统凭据服务，
  // Linux 上没有 D-Bus 时可能卡住，绝不能让它挡住首帧。
  useEffect(() => {
    if (!status?.enabled) return;
    let alive = true;
    setRemembered(null);
    void (async () => {
      const r = await api.vaultIsRemembered();
      if (alive) setRemembered(r);
    })();
    return () => {
      alive = false;
    };
  }, [status]);

  /** 打开主密码弹窗。`remember` 只影响弹窗里那个勾选框的初值。 */
  const open = (mode: MasterPasswordMode, remember = false) => {
    setReport(null);
    setRememberDefault(remember);
    setPrompt(mode);
  };

  const done = (mode: MasterPasswordMode) => (r: SweepReport | null) => {
    setPrompt(null);
    setReport(describeDone(mode, r));
    setTick((t) => t + 1);
  };

  const lockNow = async () => {
    await api.vaultLock();
    setReport({ text: "已锁定。再次读写已保存的凭据需要重新输入主密码。", warn: false });
    setTick((t) => t + 1);
  };

  /**
   * 「在本机记住」是单向免密的：关掉只是丢钥匙，随手就能做；
   * 打开等于往系统凭据服务里写一把能解开整个库的钥匙，必须重新验证主密码 ——
   * 这里没有免密写入的分支，是有意为之，不要补一个。
   */
  const toggleRemember = (next: boolean) => {
    if (next) {
      open("unlock", true);
      return;
    }
    setRemembered(false);
    setReport({
      text: "已忘掉本机记住的钥匙。当前不会锁定，下次启动时需要输入主密码。",
      warn: false,
    });
    void api.vaultForget();
  };

  const enabled = status?.enabled === true;
  const unlocked = status?.unlocked === true;

  return (
    <div className="modal-backdrop" onMouseDown={() => prompt === null && onClose()}>
      <div className="modal settings-modal" onMouseDown={(e) => e.stopPropagation()}>
        <div className="settings-head">
          <h2>安全</h2>
          <button className="settings-x" onClick={onClose} title="关闭（Esc）">
            ×
          </button>
        </div>
        <p className="mark-hint">
          主密码保护已保存的密码、私钥口令、API Key 和 TOTP 种子。密钥由主密码派生，
          nexus.db 可以原样拷到另一台机器，输同一个主密码就全部打开。
        </p>

        {/* 状态行 */}
        {status === null ? (
          <div className="sec-state">
            读取保险库状态…
          </div>
        ) : !enabled ? (
          <div className="sec-state off">
            未启用主密码保护
            <em>密码、私钥口令、API Key 以明文存在 nexus.db 里</em>
          </div>
        ) : unlocked ? (
          <div className="sec-state unlocked">
            已解锁
            <em>本次运行期间凭据可正常读写</em>
          </div>
        ) : (
          <div className="sec-state locked">
            已锁定
            <em>需要主密码才能读写已保存的凭据</em>
          </div>
        )}

        {/* 操作区：按状态给按钮，不给的那些是此刻根本做不了的 */}
        <div className="settings-section">主密码</div>
        {status === null ? (
          <p className="ag-hint">…</p>
        ) : !enabled ? (
          <>
            <div className="row">
              <label>保险库</label>
              <button className="btn primary" onClick={() => open("enable")}>
                启用主密码保护…
              </button>
            </div>
            <p className="ag-hint">
              启用后会把库里已有的凭据一次性改写成密文。
              <strong>忘记主密码将无法恢复任何凭据，也没有找回手段。</strong>
            </p>
          </>
        ) : unlocked ? (
          <div className="row">
            <label>保险库</label>
            <button className="btn" onClick={() => open("change")}>
              修改主密码…
            </button>
            <button className="btn" onClick={() => void lockNow()}>
              立即锁定
            </button>
            <button className="btn" onClick={() => open("disable")}>
              关闭主密码保护…
            </button>
          </div>
        ) : (
          <div className="row">
            <label>保险库</label>
            <button className="btn primary" onClick={() => open("unlock")}>
              输入主密码解锁…
            </button>
          </div>
        )}

        {/* 「在本机记住」：保险库没启用时这个开关没有意义，整块不渲染 */}
        {enabled &&
          (remembered === null ? (
            <p className="ag-hint">正在检查这把钥匙有没有记在本机…</p>
          ) : (
            <label className="settings-check">
              <input
                type="checkbox"
                checked={remembered}
                onChange={(e) => toggleRemember(e.target.checked)}
              />
              <span>
                在本机记住这把钥匙
                <em>
                  仅本机有效，换到其它机器仍需输入主密码。
                  打开需要重新验证主密码（把钥匙写进系统凭据服务没有免密的路径）；
                  关掉立即生效，当前不会锁定。
                </em>
              </span>
            </label>
          ))}

        {report && (
          <div className={`vault-report${report.warn ? " warn" : ""}`}>
            <strong>{report.text}</strong>
          </div>
        )}

        {/* 两栏对照：用户会默认「开了主密码 = 整个库都安全了」，而事实不是。 */}
        <div className="settings-section">加密范围</div>
        <div className="sec-cols">
          <div className={`sec-col${enabled ? " on" : " off"}`}>
            <h4>{enabled ? "已加密" : "已加密 —— 当前未加密"}</h4>
            <ul>
              <li>SSH 登录密码</li>
              <li>私钥口令</li>
              <li>LLM API Key</li>
              <li>TOTP 种子</li>
            </ul>
            {enabled ? (
              <em>
                密钥由主密码派生，整个 nexus.db 可以拷到别的机器，输同一个主密码就能打开。
                忘记主密码 = 以上四项全部永久无法恢复。
              </em>
            ) : (
              <em>
                保险库未启用，前三项以明文存在 nexus.db 里。
                TOTP 种子是例外：它现在挂在本机密钥上，是加密的，但<strong>不可移植</strong>
                —— 把 nexus.db 拷到另一台机器就解不开，必须重新填写。
              </em>
            )}
          </div>
          <div className="sec-col off">
            <h4>未加密（启用主密码也不会加密）</h4>
            <ul>
              <li>命令历史（command_history）</li>
              <li>连接备注</li>
              <li>登录脚本与会话附加设置</li>
              <li>宏的内容与步骤</li>
              <li>主机 / 端口 / 用户名 / 分组</li>
            </ul>
            <em>命令历史可能包含你手敲进命令行的密码，请留意。</em>
          </div>
        </div>

        <div className="modal-actions">
          <button className="btn primary" onClick={onClose}>
            完成
          </button>
        </div>
      </div>

      {prompt && (
        <MasterPasswordDialog
          mode={prompt}
          onClose={() => setPrompt(null)}
          rememberDefault={rememberDefault}
          onDone={done(prompt)}
        />
      )}
    </div>
  );
}
