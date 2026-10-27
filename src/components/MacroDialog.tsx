import { useEffect, useRef, useState } from "react";
import type { MacroRecord, MacroStep } from "../types";

interface Props {
  macros: MacroRecord[];
  /** 打开时要选中的宏（录制完直接跳过来） */
  focusId: string | null;
  /** 当前激活标签是否可执行（已连接） */
  tabReady: boolean;
  /** 当前标签名字，用于「运行到 xxx」的说明 */
  tabLabel: string | null;
  /** 当前标签对应的会话，用于「连接后自动运行」绑定 */
  sessionId: string | null;
  sessionName: string | null;
  /** 当前会话已绑定的宏 id（未绑定为 null） */
  boundId: string | null;
  recording: boolean;
  onBind: (macroId: string | null) => void;
  onRecord: () => void;
  onRun: (m: MacroRecord) => void;
  onCreate: (name: string, steps: MacroStep[]) => Promise<MacroRecord>;
  onUpdate: (id: string, name: string, steps: MacroStep[]) => Promise<MacroRecord>;
  onDelete: (id: string) => Promise<void>;
  onClose: () => void;
}

/** 新建宏的初始步骤：等屏幕稳定 → 发一行。这是 90% 的宏长什么样 */
const TEMPLATE: MacroStep[] = [
  { kind: "wait", pattern: "", timeoutMs: 0 },
  { kind: "send", text: "", enter: true },
];

/** 自动保存的防抖：改名 / 粘一大段文本时不必每个字符都落一次盘 */
const SAVE_DEBOUNCE = 400;

const stepCount = (m: MacroRecord) => m.steps.length;

/**
 * 宏管理器。刻意「改完即存」而不是「保存 / 取消」：宏是边试边调出来的。
 * 唯一例外是名字为空时不落库（后端也会拒），否则会存出一堆无名条目。
 */
export default function MacroDialog({
  macros,
  focusId,
  tabReady,
  tabLabel,
  sessionId,
  sessionName,
  boundId,
  recording,
  onBind,
  onRecord,
  onRun,
  onCreate,
  onUpdate,
  onDelete,
  onClose,
}: Props) {
  const [sel, setSel] = useState<string | null>(focusId ?? macros[0]?.id ?? null);
  const [name, setName] = useState("");
  const [steps, setSteps] = useState<MacroStep[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const [confirmDel, setConfirmDel] = useState(false);
  /** 会话当前绑定的宏。本地存一份：切换选中项时复选框要跟着变，而绑定关系
      不在宏列表里，拿不到重渲染信号 */
  const [bound, setBound] = useState<string | null>(boundId);
  const timer = useRef<number | undefined>(undefined);
  /** 刚新建、但 props 里的列表还没刷新过来的宏 id */
  const justCreated = useRef<string | null>(null);
  /** 有改动还没落盘。用它而不是定时器句柄：清理顺序不可依赖 */
  const pending = useRef(false);
  const latest = useRef({ sel, name, steps });
  latest.current = { sel, name, steps };
  /** 刚切到另一条宏时会跑一次自动保存，那一次不该把新宏的内容写回去 */
  const skipSave = useRef(true);

  const current = macros.find((m) => m.id === sel) ?? null;
  /** 保存回调里要读最新的列表，但不能把 macros 放进效果依赖（会让回包覆盖草稿） */
  const macrosRef = useRef(macros);
  macrosRef.current = macros;

  // 选中项变化时载入草稿。只依赖 sel：macros 每次保存都会换新数组，
  // 跟着它重载会把用户正在敲的内容弹回上一次保存的版本。
  useEffect(() => {
    const m = macrosRef.current.find((x) => x.id === sel);
    if (!m) return;
    justCreated.current = null;
    skipSave.current = true;
    setName(m.name);
    setSteps(m.steps);
    setConfirmDel(false);
    setError(null);
    setSaved(false);
  }, [sel]);

  // 选中项失效（首次打开 / 被删掉）：落到第一条
  useEffect(() => {
    if (sel && macros.some((m) => m.id === sel)) return;
    // 新建后 setState 与列表更新在同一批里到达；万一没到（React 分批渲染），
    // 也不能把刚建好的选中项弹回第一条 —— 那会让「新建」看起来毫无反应
    if (sel && sel === justCreated.current) return;
    const first = macros[0] ?? null;
    skipSave.current = true;
    setSel(first?.id ?? null);
    setName(first?.name ?? "");
    setSteps(first?.steps ?? []);
    setConfirmDel(false);
    setSaved(false);
  }, [sel, macros]);

  // 录制结束会带着新 id 打开/跳到这条宏
  useEffect(() => {
    if (focusId) setSel(focusId);
  }, [focusId]);

  // 防抖自动保存
  useEffect(() => {
    if (!sel) return;
    if (skipSave.current) {
      skipSave.current = false;
      return;
    }
    if (!name.trim()) return;
    const m = macrosRef.current.find((x) => x.id === sel);
    // 用序列化比较而不是引用比较：回包会换掉 props 里的数组，
    // 引用永远不等，会变成每 400ms 写一次库的死循环
    if (m && m.name === name.trim() && JSON.stringify(m.steps) === JSON.stringify(steps)) {
      return;
    }
    window.clearTimeout(timer.current);
    pending.current = true;
    timer.current = window.setTimeout(() => {
      void (async () => {
        try {
          await onUpdate(sel, name.trim(), steps);
          pending.current = false;
          setSaved(true);
          setError(null);
        } catch (e) {
          setError(String(e));
        }
      })();
    }, SAVE_DEBOUNCE);
    return () => window.clearTimeout(timer.current);
  }, [name, steps, sel, onUpdate]);

  /**
   * 关闭窗口时补存一次：自动保存有 400ms 防抖，「改完就关」会把那次修改静默丢掉。
   * 用 pending 标记而不是定时器句柄 —— 卸载时句柄多半已被上一个效果的清理函数清掉。
   */
  useEffect(
    () => () => {
      if (!pending.current) return;
      const d = latest.current;
      if (!d.sel || !d.name.trim()) return;
      pending.current = false;
      void onUpdate(d.sel, d.name.trim(), d.steps).catch(() => {});
    },
    [onUpdate]
  );

  useEffect(() => {
    if (!saved) return;
    const t = window.setTimeout(() => setSaved(false), 1600);
    return () => window.clearTimeout(t);
  }, [saved]);

  // Esc 关闭：录制中先不响应，避免用户想停止录制却把窗口关了
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || recording) return;
      e.stopPropagation();
      onClose();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose, recording]);

  const createNew = async () => {
    setBusy(true);
    setError(null);
    try {
      // 名字带序号，避免「新宏」重名到分不清
      const m = await onCreate(`新宏 ${macros.length + 1}`, TEMPLATE);
      justCreated.current = m.id;
      skipSave.current = true;
      setSel(m.id);
      setName(m.name);
      setSteps(m.steps);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const remove = async () => {
    if (!current) return;
    if (!confirmDel) {
      setConfirmDel(true);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await onDelete(current.id);
      setConfirmDel(false);
      const rest = macros.filter((m) => m.id !== current.id);
      const next = rest[0] ?? null;
      skipSave.current = true;
      setSel(next?.id ?? null);
      setName(next?.name ?? "");
      setSteps(next?.steps ?? []);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const patchStep = (i: number, next: MacroStep) => {
    setSteps((prev) => prev.map((s, k) => (k === i ? next : s)));
  };

  const move = (i: number, delta: number) => {
    setSteps((prev) => {
      const j = i + delta;
      if (j < 0 || j >= prev.length) return prev;
      const copy = prev.slice();
      [copy[i], copy[j]] = [copy[j], copy[i]];
      return copy;
    });
  };

  const add = (kind: MacroStep["kind"]) => {
    const step: MacroStep =
      kind === "send"
        ? { kind: "send", text: "", enter: true }
        : kind === "wait"
          ? { kind: "wait", pattern: "", timeoutMs: 0 }
          : { kind: "sleep", ms: 1000 };
    setSteps((prev) => [...prev, step]);
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal macro-modal" onClick={(e) => e.stopPropagation()}>
        <h2>宏</h2>

        <div className="macro-body">
          <aside className="macro-list">
            {macros.length === 0 && <p className="macro-empty">还没有宏</p>}
            {macros.map((m) => (
              <button
                key={m.id}
                className={`macro-item${m.id === sel ? " active" : ""}`}
                onClick={() => setSel(m.id)}
                title={m.name}
              >
                <span className="macro-item-name">{m.name}</span>
                <span className="macro-item-count">{stepCount(m)} 步</span>
              </button>
            ))}
            <button className="btn macro-new" disabled={busy} onClick={() => void createNew()}>
              ＋ 新建宏
            </button>
          </aside>

          <section className="macro-editor">
            {!current ? (
              <div className="macro-placeholder">
                <p>宏是一串「等什么、发什么」的操作。</p>
                <p className="hint" style={{ margin: 0 }}>
                  点「新建宏」，或者直接点下面的「录制」，把你现在做的事录成一条宏。
                </p>
              </div>
            ) : (
              <>
                <div className="macro-name-row">
                  <input
                    className="macro-name"
                    value={name}
                    spellCheck={false}
                    placeholder="宏名称"
                    onChange={(e) => setName(e.target.value)}
                  />
                  <span className={`macro-save${saved ? " on" : ""}`}>
                    {name.trim() ? (saved ? "已保存" : "自动保存") : "名称不能为空"}
                  </span>
                </div>

                <div className="macro-steps">
                  {steps.length === 0 && (
                    <p className="macro-empty">还没有步骤，从下面添加一步。</p>
                  )}
                  {steps.map((s, i) => (
                    <div className="macro-step" key={i}>
                      <span className="macro-step-no">{i + 1}</span>

                      {s.kind === "send" && (
                        <>
                          <input
                            className="macro-param grow"
                            value={s.text}
                            spellCheck={false}
                            placeholder="要发送的内容（支持 \r \n \t \e \xNN 转义）"
                            onChange={(e) =>
                              patchStep(i, { ...s, text: e.target.value })
                            }
                          />
                          <label className="macro-check">
                            <input
                              type="checkbox"
                              checked={s.enter}
                              onChange={(e) =>
                                patchStep(i, { ...s, enter: e.target.checked })
                              }
                            />
                            回车
                          </label>
                        </>
                      )}

                      {s.kind === "wait" && (
                        <>
                          <input
                            className="macro-param grow"
                            value={s.pattern}
                            spellCheck={false}
                            placeholder="等待出现的文本；留空 = 等输出停止"
                            onChange={(e) =>
                              patchStep(i, { ...s, pattern: e.target.value })
                            }
                          />
                          <input
                            className="macro-param num"
                            type="number"
                            min={0}
                            value={s.timeoutMs ? Math.round(s.timeoutMs / 1000) : ""}
                            placeholder="20"
                            title="超时（秒）；留空 = 默认 20 秒"
                            onChange={(e) =>
                              patchStep(i, {
                                ...s,
                                timeoutMs: Math.max(0, Number(e.target.value) || 0) * 1000,
                              })
                            }
                          />
                          <span className="macro-unit">秒</span>
                        </>
                      )}

                      {s.kind === "sleep" && (
                        <>
                          <span className="macro-param-label">等待</span>
                          <input
                            className="macro-param num"
                            type="number"
                            min={0}
                            step={0.5}
                            value={s.ms ? Math.round(s.ms / 100) / 10 : ""}
                            placeholder="1"
                            onChange={(e) =>
                              patchStep(i, {
                                ...s,
                                ms: Math.max(0, Number(e.target.value) || 0) * 1000,
                              })
                            }
                          />
                          <span className="macro-unit">秒</span>
                        </>
                      )}

                      <span className="macro-step-ops">
                        <button
                          className="icon-btn"
                          title="上移"
                          disabled={i === 0}
                          onClick={() => move(i, -1)}
                        >
                          ↑
                        </button>
                        <button
                          className="icon-btn"
                          title="下移"
                          disabled={i === steps.length - 1}
                          onClick={() => move(i, 1)}
                        >
                          ↓
                        </button>
                        <button
                          className="icon-btn danger"
                          title="删除这一步"
                          onClick={() => setSteps((prev) => prev.filter((_, k) => k !== i))}
                        >
                          ✕
                        </button>
                      </span>
                    </div>
                  ))}
                </div>

                <div className="macro-add">
                  <button className="btn" onClick={() => add("send")}>
                    ＋ 发送
                  </button>
                  <button className="btn" onClick={() => add("wait")}>
                    ＋ 等待
                  </button>
                  <button className="btn" onClick={() => add("sleep")}>
                    ＋ 延时
                  </button>
                  <button
                    className="btn danger-text macro-del"
                    disabled={busy}
                    onClick={() => void remove()}
                  >
                    {confirmDel ? "再点一次，永久删除" : "删除这条宏"}
                  </button>
                </div>
              </>
            )}
          </section>
        </div>

        {error && <p className="error">{error}</p>}

        <div className="macro-bind">
          <label className="settings-check login-path-check">
            <input
              type="checkbox"
              checked={!!sel && bound === sel}
              disabled={!sessionId || !sel}
              onChange={(e) => {
                const next = e.target.checked ? sel : null;
                setBound(next);
                onBind(next);
              }}
            />
            <span>
              连接后自动运行
              <em>
                {!sessionId
                  ? "当前标签没有对应的会话配置，无法绑定"
                  : bound === sel
                    ? `「${sessionName}」每次连上都会先执行这条宏（重连也算）`
                    : `勾选后，「${sessionName}」每次连上会先执行这条宏`}
              </em>
            </span>
          </label>
        </div>

        <div className="modal-actions macro-actions">
          <button
            className="btn"
            disabled={!tabReady || recording || !current}
            title={tabReady ? "" : "先连接一个会话，宏才有地方执行"}
            onClick={() => current && onRun(current)}
          >
            运行{tabLabel ? `（${tabLabel}）` : ""}
          </button>
          <button className={`btn${recording ? " primary" : ""}`} onClick={onRecord}>
            {recording ? "停止录制" : "录制"}
          </button>
          <span className="macro-spacer" />
          <button className="btn primary" onClick={onClose}>
            关闭
          </button>
        </div>
      </div>
    </div>
  );
}
