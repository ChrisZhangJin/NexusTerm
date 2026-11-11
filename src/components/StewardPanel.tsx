/**
 * P2「托管控制台」：Steward 的眼睛（局面 / 事件）+ 手（决策 / 注入 / 回执）。
 *
 * P0 验屏幕帧持续同步、P1 验常驻只读观察，P2 在同一面板里补上：
 * 决策记录与依据引用、升级卡（建议 / 备选 / 倒计时 / 一键确认）、自动应答总闸、
 * 以及用户预先承载的偏好 / 意向 / 知识 / 剧本编辑区。
 *
 * 决策编译零 LLM；自动应答每次冷启动默认关闭，打开后密码 / 永久扩权 / 断流 /
 * 低置信度 / approve 档依然 fail-closed。设计见 docs/nexus-agent-steward-design.md。
 */

import { useCallback, useEffect, useRef, useState } from "react";
import type { UnlistenFn } from "@tauri-apps/api/event";
import type { AgentTarget } from "../types";
import {
  ACTION_LABELS,
  EVENT_LABELS,
  OUTCOME_LABELS,
  REF_LABELS,
  decisionSettled,
  eventLabel,
  programLabel,
  situationLabel,
  stewardApi,
  type AbsenceSummary,
  type AuditEntry,
  type Authority,
  type ConsoleInfo,
  type CoreView,
  type DecisionRecord,
  type Explanation,
  type OwnershipState,
  type PlaybookCandidate,
  type ReviewReport,
  type ScreenSnapshot,
  type Situation,
  type StewardCheckpoint,
  type StewardContract,
  type StewardEvent,
  type StewardEventKind,
  type StewardRuntimeInfo,
  type StewardTier,
} from "../lib/steward";
import { captureNow } from "../lib/termRegistry";
import StewardContext from "./StewardContext";

export interface StewardPanelProps {
  /** 面板当前绑定的终端，作为默认选中项 */
  sessionId: string;
  /** 标签树里的可读名字，由 App 传入；后端只认 id */
  sessions?: AgentTarget[];
}

const TIERS: Array<{ value: StewardTier; label: string; hint: string }> = [
  { value: "auto", label: "自动", hint: "范围内自主执行，事后可查" },
  { value: "notify", label: "通知", hint: "自主执行并立即通知，短窗口可接管" },
  { value: "approve", label: "确认", hint: "先给建议 + 倒计时，人一键确认后才写" },
];

const OWNERSHIP_LABELS: Record<OwnershipState, string> = {
  agent_driving: "Steward 驾驶中",
  observing_only: "你已接管（Steward 只观察）",
  suspended: "暂停（保留现场）",
};

const hhmmss = (ms: number) => {
  try {
    return new Date(ms).toLocaleTimeString("zh-CN", { hour12: false });
  } catch {
    return String(ms);
  }
};

export default function StewardPanel({ sessionId, sessions }: StewardPanelProps) {
  const [consoles, setConsoles] = useState<ConsoleInfo[]>([]);
  const [contracts, setContracts] = useState<StewardContract[]>([]);
  const [snapshot, setSnapshot] = useState<ScreenSnapshot | null>(null);
  const [events, setEvents] = useState<StewardEvent[]>([]);
  const [decisions, setDecisions] = useState<DecisionRecord[]>([]);
  const [runtime, setRuntime] = useState<StewardRuntimeInfo | null>(null);
  const [manual, setManual] = useState<Situation | null>(null);
  const [showTail, setShowTail] = useState(false);
  const [showCtx, setShowCtx] = useState(false);
  // —— P3-P5：托管核心 / 交还摘要 / 授权 / 现场恢复 / 复盘 ——
  const [core, setCore] = useState<CoreView | null>(null);
  const [absence, setAbsence] = useState<AbsenceSummary | null>(null);
  const [checkpoints, setCheckpoints] = useState<StewardCheckpoint[]>([]);
  const [review, setReview] = useState<ReviewReport | null>(null);
  const [audit, setAudit] = useState<AuditEntry[]>([]);
  const [candidates, setCandidates] = useState<PlaybookCandidate[]>([]);
  const [explanation, setExplanation] = useState<Explanation | null>(null);
  const [showOps, setShowOps] = useState(false);
  const [showReview, setShowReview] = useState(false);
  const [selected, setSelected] = useState(sessionId);
  const [goal, setGoal] = useState("");
  const [tier, setTier] = useState<StewardTier>("notify");
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [clock, setClock] = useState(Date.now());
  const [correction, setCorrection] = useState("");
  const selectedRef = useRef(selected);
  selectedRef.current = selected;

  const refresh = useCallback(async () => {
    try {
      const [list, signed, rt, evs, recs, cv, cps] = await Promise.all([
        stewardApi.listConsoles(),
        stewardApi.status(),
        stewardApi.runtime(),
        // 事件流最多 200 条，取最近 60 条足够；全量拉取避免前端游标在
        // 「清空事件」后倒退，seq 由后端单调分配。
        stewardApi.events(0, 60),
        stewardApi.decisions(20),
        stewardApi.coreView(),
        stewardApi.checkpoints(),
      ]);
      setConsoles(list);
      setContracts(signed);
      setRuntime(rt);
      setEvents(evs);
      setDecisions(recs);
      setCore(cv);
      setCheckpoints(cps);
      setSnapshot(await stewardApi.screenGet(selectedRef.current));
      setErr(null);
    } catch (e) {
      setErr(`读取常驻 Steward 失败：${String(e)}`);
    }
  }, []);

  useEffect(() => {
    void refresh();
    const timer = window.setInterval(() => void refresh(), 1500);
    return () => window.clearInterval(timer);
  }, [refresh]);

  useEffect(() => {
    const timer = window.setInterval(() => setClock(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, []);

  // 后端推事件时立刻刷一次，不必等 1.5s 轮询
  useEffect(() => {
    let cancelled = false;
    let un: UnlistenFn | null = null;
    void stewardApi.onEvent(() => void refresh()).then((f) => {
      if (cancelled) f();
      else un = f;
    });
    return () => {
      cancelled = true;
      un?.();
    };
  }, [refresh]);

  const contract = contracts.find((c) => c.consoleId === selected) ?? null;
  const label = (id: string) => sessions?.find((s) => s.id === id)?.label ?? id;
  const state = runtime?.consoles.find((c) => c.consoleId === selected) ?? null;
  const own = core?.ownership.find((o) => o.consoleId === selected) ?? null;
  const undoList = core?.liveUndos ?? [];
  const contractAuth = contract
    ? (core?.authorities.find(([id]) => id === contract.id)?.[1] ?? null)
    : null;
  const [authDraft, setAuthDraft] = useState<Authority | null>(null);
  const authRef = useRef<Authority | null>(null);
  authRef.current = authDraft;

  // 契约或授权变化时同步编辑草稿（避免每 1.5s 轮询把用户正在编辑的值冲掉）
  useEffect(() => {
    setAuthDraft((prev) => {
      if (prev && prev.tier === contractAuth?.tier) return prev;
      return contractAuth;
    });
  }, [contractAuth]);

  const takeover = async () => {
    try {
      await stewardApi.takeover(selected);
      await refresh();
    } catch (e) {
      setErr(`接管失败：${String(e)}`);
    }
  };

  const handback = async () => {
    try {
      const sum = await stewardApi.handback(selected);
      setAbsence(sum);
      await refresh();
    } catch (e) {
      setErr(`交还失败：${String(e)}`);
    }
  };

  const undoAction = async (seq: number) => {
    try {
      await stewardApi.undo(seq);
      await refresh();
    } catch (e) {
      setErr(`撤销失败：${String(e)}`);
    }
  };

  const doResume = async (contractId: string) => {
    try {
      await stewardApi.resume(contractId);
      await refresh();
    } catch (e) {
      setErr(`恢复失败：${String(e)}`);
    }
  };

  const togglePlanner = async () => {
    try {
      await stewardApi.setPlanner(!core?.plannerEnabled);
      await refresh();
    } catch (e) {
      setErr(`切换 Planner 失败：${String(e)}`);
    }
  };

  const doPlanNow = async () => {
    try {
      await stewardApi.planNow(selected);
      await refresh();
    } catch (e) {
      setErr(`Planner 调用失败：${String(e)}`);
    }
  };

  const doExplain = async (seq: number) => {
    try {
      setExplanation(await stewardApi.explain(seq));
    } catch (e) {
      setErr(`解释链读取失败：${String(e)}`);
    }
  };

  const loadReview = async () => {
    try {
      setReview(await stewardApi.review(contract?.id));
      setAudit(await stewardApi.audit(contract?.id));
    } catch (e) {
      setErr(`复盘读取失败：${String(e)}`);
    }
  };

  const loadDistill = async () => {
    try {
      setCandidates(await stewardApi.distill());
    } catch (e) {
      setErr(`蒸馏预览失败：${String(e)}`);
    }
  };

  const saveAuthority = async () => {
    const a = authRef.current;
    if (!contract || !a) return;
    setBusy(true);
    try {
      await stewardApi.setAuthority(contract.id, a);
      await refresh();
    } catch (e) {
      setErr(`更新授权失败：${String(e)}`);
    } finally {
      setBusy(false);
    }
  };

  // 完整局面优先用「立即分类」的结果；否则取该 console 最近一条带局面的事件
  const latestEventSituation =
    [...events]
      .reverse()
      .find((e) => e.consoleId === selected && e.situation)?.situation ?? null;
  const situation = manual ?? latestEventSituation;

  const start = async () => {
    if (!selected || !goal.trim() || busy) return;
    setBusy(true);
    try {
      await stewardApi.start(selected, goal.trim(), tier);
      await refresh();
    } catch (e) {
      setErr(`签契约失败：${String(e)}`);
    } finally {
      setBusy(false);
    }
  };

  const stop = async () => {
    if (!contract || busy) return;
    setBusy(true);
    try {
      await stewardApi.stop(contract.id);
      setManual(null);
      await refresh();
    } finally {
      setBusy(false);
    }
  };

  const amend = async () => {
    if (!contract || !goal.trim() || busy) return;
    setBusy(true);
    try {
      await stewardApi.amend(contract.id, goal.trim(), tier);
      await refresh();
    } finally {
      setBusy(false);
    }
  };

  const classifyNow = async () => {
    if (!selected) return;
    try {
      const s = await stewardApi.classifyNow(selected);
      setManual(s);
      await stewardApi.wake();
    } catch (e) {
      setErr(`立即分类失败：${String(e)}`);
    }
  };

  const decideNow = async () => {
    try {
      await stewardApi.decideNow(selected);
      await refresh();
    } catch (e) {
      setErr(`立即决策失败：${String(e)}`);
    }
  };

  const toggleExecution = async () => {
    try {
      await stewardApi.setExecution(!runtime?.executionEnabled);
      await stewardApi.wake();
      await refresh();
    } catch (e) {
      setErr(`切换自动应答失败：${String(e)}`);
    }
  };

  const approve = async (seq: number) => {
    try {
      await stewardApi.approve(seq);
      await refresh();
    } catch (e) {
      setErr(`执行建议失败：${String(e)}`);
    }
  };

  const reject = async (seq: number) => {
    try {
      await stewardApi.reject(seq, correction.trim() || undefined);
      setCorrection("");
      await refresh();
    } catch (e) {
      setErr(`接管失败：${String(e)}`);
    }
  };

  const setTicks = async (n: number) => {
    try {
      await stewardApi.setConfig(n);
      await stewardApi.wake();
      await refresh();
    } catch (e) {
      setErr(`调整阈值失败：${String(e)}`);
    }
  };

  const clear = async () => {
    await stewardApi.clearEvents();
    setManual(null);
    await refresh();
  };

  const clearDecisions = async () => {
    await stewardApi.clearDecisions();
    await refresh();
  };

  return (
    <div className="ap-steward">
      <div className="st-note">
        P3–P5 托管核心：签契约后 Steward 常驻读屏、按意向编译决策并替你执行。你敲键即夺权
       （100ms SLA），交还时给「离开期间」摘要；Notify 档留 30s 撤销窗口；断连 / 强杀后凭
        Checkpoint 恢复现场（屏幕漂移自动降「确认」档）。决策编译零 LLM，Planner 只是兜底且默认关。
      </div>

      {err && <div className="st-err">{err}</div>}

      <div className="st-runtime">
        <span className={`st-dot${runtime?.running ? " live" : ""}`} />
        <span>{runtime?.running ? "Steward 运行中" : "Steward 未运行"}</span>
        <span className="st-meta">
          tick {runtime?.tickCount ?? 0} · 分类 {runtime?.classifyCount ?? 0} · 决策 {runtime?.decisionCount ?? 0} ·
          注入 <strong className={runtime?.injectedCount ? "st-hot" : ""}>{runtime?.injectedCount ?? 0}</strong> ·
          待回执 {runtime?.pendingReceipts ?? 0} · 待确认 {runtime?.escalations ?? 0} ·{" "}
          <strong className="st-zero">LLM {runtime?.llmCalls ?? 0}</strong>
        </span>
        <button
          className={`st-btn${runtime?.executionEnabled ? " danger" : " primary"}`}
          onClick={() => void toggleExecution()}
          title="总闸：每次冷启动默认关闭；打开后仍受安全策略 fail-closed 约束"
        >
          自动应答：{runtime?.executionEnabled ? "开" : "关"}
        </button>
        <button
          className={`st-btn${core?.plannerEnabled ? " danger" : ""}`}
          onClick={() => void togglePlanner()}
          title="LLM Planner 兜底总闸：默认关闭、不持久化；只有规则编不出回复时才调模型，结果仍过 PolicyEngine"
        >
          Planner：{core?.plannerEnabled ? "开" : "关"}
          {core ? `（在途 ${core.llmInflight}/2）` : ""}
        </button>
        <button className="st-btn" onClick={() => void setTicks(1)} title="真机验收：一次心跳就触发空转事件">
          阈值 1
        </button>
        <button className="st-btn" onClick={() => void setTicks(3)} title="恢复默认 3 次">
          阈值 3
        </button>
      </div>

      <div className="st-runtime">
        <span className="st-meta">
          所有权 {core?.ownership.length ?? 0} 槽 · 撤销窗口{" "}
          <strong className={undoList.length ? "st-hot" : ""}>{undoList.length}</strong> · 现场{" "}
          {core?.checkpointCount ?? 0} 份 · 待恢复 {core?.resumeQueue.length ?? 0} · 写锁{" "}
          {core?.writeLocked.length ?? 0} · 单调时钟 {(core?.elapsedMs ?? 0) / 1000 | 0}s
        </span>
        <button className="st-btn" onClick={() => setShowOps((v) => !v)}>
          {showOps ? "收起托管核心" : "托管核心（接管 / 撤销 / 授权 / 恢复）"}
        </button>
        <button className="st-btn" onClick={() => setShowReview((v) => !v)}>
          {showReview ? "收起复盘" : "复盘（审计 / 蒸馏 / 解释链）"}
        </button>
      </div>

      {showOps && (
        <div className="st-sit">
          <div className="st-sit-head">
            <span>所有权 · {label(selected)}</span>
            <span className="st-meta">
              {own
                ? `${OWNERSHIP_LABELS[own.state]} · 接管 ${own.takeoverCount} 次 · 夺权延迟 ${
                    own.takeoverLatencyMs >= 0 ? `${own.takeoverLatencyMs}ms` : "—"
                  }`
                : "这个 console 还没有所有权槽（签契约后创建）"}
            </span>
          </div>
          <div className="st-form-row">
            <button
              className="st-btn danger"
              onClick={() => void takeover()}
              title="手动接管：Steward 立刻转观察态，绝不再写这个 console（用户敲键也会自动触发，100ms SLA）"
            >
              我来接管
            </button>
            <button
              className="st-btn primary"
              onClick={() => void handback()}
              title="交还给 Steward：先读屏对齐（不一致自动降确认档），再给「离开期间」摘要"
            >
              交还托管
            </button>
            {core?.plannerEnabled && (
              <button
                className="st-btn"
                onClick={() => void doPlanNow()}
                title="真·LLM 兜底：规则编不出回复时调模型，结果仍要过 PolicyEngine"
              >
                立即规划
              </button>
            )}
          </div>
          {absence && (
            <div className="st-card">
              <div className="st-esc-head">
                离开 {Math.round(absence.durationMs / 1000)}s · 你接管 {absence.takeoverCount} 次 · 屏幕
                {absence.screenAligned ? "对齐" : "漂移（已降确认档）"}
              </div>
              <div className="st-esc-why">
                已做 {absence.done.length} 条：{absence.done.join("；") || "—"}
              </div>
              <div className="st-esc-why">
                待决 {absence.pending.length} 条：{absence.pending.join("；") || "—"}
              </div>
              <div className="st-evidence">下一步：{absence.nextStep}</div>
            </div>
          )}
          {undoList.length > 0 && (
            <div className="st-esc">
              <div className="st-esc-head">
                撤销窗口 · {undoList.length} 条（Notify 档执行后 30s 内可一键回滚）
              </div>
              {undoList.map((u) => (
                <div key={u.seq} className="st-form-row">
                  <span className="st-dec-seq">#{u.seq}</span>
                  <span className="st-dec-text">
                    {ACTION_LABELS[u.action.kind] ?? u.action.kind}「{u.action.text}」
                  </span>
                  <span className="st-meta">
                    剩 {Math.max(0, Math.ceil((u.expiresAt - clock) / 1000))}s
                  </span>
                  <button className="st-btn danger" onClick={() => void undoAction(u.seq)}>
                    撤销并接管
                  </button>
                </div>
              ))}
            </div>
          )}
          {contract && authDraft && (
            <div className="st-form">
              <div className="st-form-title">授权契约 · {contract.id.slice(0, 12)}</div>
              <div className="st-form-row">
                <select
                  className="st-select"
                  value={authDraft.tier}
                  onChange={(e) => setAuthDraft({ ...authDraft, tier: e.target.value as StewardTier })}
                >
                  {TIERS.map((t) => (
                    <option key={t.value} value={t.value}>
                      {t.label}
                    </option>
                  ))}
                </select>
                <label className="st-meta" title="允许自动选中 don't ask again 类永久扩权选项；强烈建议关闭">
                  <input
                    type="checkbox"
                    checked={authDraft.allowPermanentGrant}
                    onChange={(e) => setAuthDraft({ ...authDraft, allowPermanentGrant: e.target.checked })}
                  />
                  永久扩权
                </label>
                <label className="st-meta">
                  预算步数
                  <input
                    className="st-input"
                    type="number"
                    min={0}
                    value={authDraft.maxSteps}
                    onChange={(e) => setAuthDraft({ ...authDraft, maxSteps: Number(e.target.value) || 0 })}
                  />
                </label>
                <label className="st-meta">
                  预算分钟
                  <input
                    className="st-input"
                    type="number"
                    min={0}
                    value={authDraft.maxMinutes}
                    onChange={(e) => setAuthDraft({ ...authDraft, maxMinutes: Number(e.target.value) || 0 })}
                  />
                </label>
                <label className="st-meta" title="意向 deadline（分钟），0 = 不限；到期收尾摘要 + 保留现场">
                  deadline 分钟
                  <input
                    className="st-input"
                    type="number"
                    min={0}
                    value={Math.round(authDraft.timeboxMs / 60000)}
                    onChange={(e) =>
                      setAuthDraft({ ...authDraft, timeboxMs: (Number(e.target.value) || 0) * 60000 })
                    }
                  />
                </label>
                <button className="st-btn primary" disabled={busy} onClick={() => void saveAuthority()}>
                  保存授权
                </button>
              </div>
              <div className="st-evidence">
                硬禁：{contractAuth?.deny.join(" / ") || "—"} · 提权白名单：
                {contractAuth?.allow.join(" / ") || "—"} · host 作用域：
                {contractAuth?.hostScope.join(" / ") || "全部已注册 console"}
              </div>
            </div>
          )}
          {core && (
            <div className="st-form">
              <div className="st-form-title">时间盒 · 六档</div>
              <div className="st-evidence">
                读屏回执 {core.timebox.screenAckMs / 1000}s · 写后回执 {core.timebox.receiptMs / 1000}s · 单步{" "}
                {core.timebox.stepMs / 60000}min · 空闲 {core.timebox.idleMs / 60000}min · 无进展{" "}
                {core.timebox.noProgressMs / 60000}min · 意向 deadline{" "}
                {core.timebox.deadlineMs ? `${Math.round(core.timebox.deadlineMs / 60000)}min` : "不限"}
              </div>
            </div>
          )}
          <div className="st-form">
            <div className="st-form-title">现场恢复 · {checkpoints.length} 份 Checkpoint</div>
            {checkpoints.length === 0 && (
              <div className="st-empty">还没有落盘的现场。每个决策闭环或每 30s 自动保存一次。</div>
            )}
            {checkpoints.slice(0, 5).map((cp) => (
              <div key={cp.contractId} className="st-form-row">
                <span className="st-dec-seq">{cp.contractId.slice(0, 12)}</span>
                <span className="st-dec-text">{cp.goal.slice(0, 40) || "（无目标）"}</span>
                <span className="st-meta">
                  {cp.state} · 档 {cp.authority.tier} · 步 {cp.spent.steps} · 决策 {cp.lastDecisions.length} 条
                  {cp.pendingEscalation ? ` · 待决 #${cp.pendingEscalation[0]}` : ""}
                </span>
                <button className="st-btn primary" onClick={() => void doResume(cp.contractId)}>
                  恢复
                </button>
              </div>
            ))}
          </div>
        </div>
      )}

      <div className="st-head">
        <span>会话织物 · {consoles.length} 个 console</span>
        <div>
          <button className="st-btn" onClick={() => void classifyNow()} title="当场分类一帧，核对判据">
            立即分类
          </button>
          <button className="st-btn" onClick={() => void decideNow()} title="当场编译一条决策并落库，不注入">
            立即决策
          </button>
          <button
            className="st-btn"
            onClick={() => {
              captureNow(selected);
              void refresh();
            }}
            title="让前端立刻补一帧全量，验证前后端是否对齐"
          >
            强制全量帧
          </button>
          <button className="st-btn" onClick={() => setShowCtx((v) => !v)}>
            {showCtx ? "收起依据" : "编辑依据"}
          </button>
          <button className="st-btn danger" onClick={() => void clearDecisions()}>
            清空决策
          </button>
          <button className="st-btn danger" onClick={() => void clear()}>
            清空事件
          </button>
          <button className="st-btn" onClick={() => void refresh()}>
            刷新
          </button>
        </div>
      </div>

      <div className="st-list">
        {consoles.length === 0 && <div className="st-empty">还没有检测到终端</div>}
        {consoles.map((c) => {
          const cs = runtime?.consoles.find((x) => x.consoleId === c.id);
          return (
            <button
              key={c.id}
              className={`st-row${selected === c.id ? " on" : ""}`}
              onClick={() => {
                setSelected(c.id);
                setManual(null);
                const signed = contracts.find((x) => x.consoleId === c.id);
                setGoal(signed?.goal ?? "");
                setTier(signed?.authorityTier ?? "notify");
              }}
            >
              <span className={`st-dot${c.alive ? " live" : ""}`} />
              <span className="st-name">{label(c.id)}</span>
              <span className="st-badge">{c.kind}</span>
              {c.alt && <span className="st-badge alt">全屏</span>}
              {c.status !== "none" && <span className="st-badge on">托管中</span>}
              {cs?.kind && <span className="st-badge situation">{situationLabel(cs.kind)}</span>}
              {cs?.injectedCount ? <span className="st-badge hot">注入 {cs.injectedCount}</span> : null}
              <span className="st-meta">
                {c.hasScreen
                  ? `帧 ${c.frameId} · ${c.screenHash.slice(0, 8)} · 空转 ${c.idleFrames}`
                  : "无屏幕帧"}
              </span>
            </button>
          );
        })}
      </div>

      <div className="st-sit">
        <div className="st-sit-head">
          <span>当前局面 · {label(selected)}</span>
          <span className="st-meta">
            {state
              ? `心跳 ${state.heartbeatMs}ms · 同局面 ${state.sameTicks} · ${state.stale ? "断流降权" : "帧新鲜"}${state.pendingReceipt ? " · 等回执" : ""}`
              : "该 console 尚未托管"}
          </span>
        </div>
        {situation ? (
          <div className="st-sit-body">
            <div className="st-sit-line">
              <span className="st-kind">{situationLabel(situation.kind)}</span>
              <span className="st-badge">{programLabel(situation.program)}</span>
              <span className="st-meta">置信度 {situation.confidence.toFixed(2)}</span>
              <span className="st-meta">帧 {situation.frameId}</span>
            </div>
            {situation.topic && <div className="st-topic">主题：{situation.topic}</div>}
            {situation.options.length > 0 && (
              <div className="st-opts">
                {situation.options.map((o, i) => (
                  <span key={`${o.index ?? i}-${i}`} className={`st-opt${o.grantsPermanent ? " perm" : ""}`}>
                    {o.index ? `${o.index}. ` : ""}
                    {o.text}
                    {o.grantsPermanent ? " ⚠ 永久授权，不自动选" : ""}
                  </span>
                ))}
              </div>
            )}
            <div className="st-evidence">判据：{situation.evidence.join(" · ")}</div>
            <button className="st-btn" onClick={() => setShowTail((v) => !v)}>
              {showTail ? "收起脱敏屏幕" : "查看脱敏屏幕 / 签名"}
            </button>
            {showTail && (
              <>
                <div className="st-evidence">签名：{situation.signature}</div>
                <pre className="st-pre small">{situation.tail || "（空屏）"}</pre>
              </>
            )}
          </div>
        ) : (
          <div className="st-empty">
            还没有局面记录。签一份托管契约，等前端心跳帧上屏后这里会出现第一张局面卡片。
          </div>
        )}
      </div>

      <div className="st-dec-wrap">
        <div className="st-sit-head">
          <span>决策记录 · 最近 {decisions.length} 条</span>
          <span className="st-meta">可展开看依据引用与备选动作</span>
        </div>
        <div className="st-dec-list">
          {decisions.length === 0 && (
            <div className="st-empty">还没有决策。制造一个提问 / 权限确认，或点「立即决策」。</div>
          )}
          {[...decisions].reverse().map((d) => {
            const card = d.escalation;
            const left = card
              ? Math.max(0, Math.ceil((card.createdAt + card.timeoutMs - clock) / 1000))
              : 0;
            const showCard = !!card && !decisionSettled(d);
            return (
              <div key={d.seq} className={`st-dec ${d.outcome}`}>
                <div className="st-dec-head">
                  <span className="st-dec-seq">#{d.seq}</span>
                  <span className="st-kind">{ACTION_LABELS[d.chosen.kind] ?? d.chosen.kind}</span>
                  <span className="st-badge">{d.tier}</span>
                  <span className="st-badge outcome">{OUTCOME_LABELS[d.outcome] ?? d.outcome}</span>
                  <span className="st-meta">
                    {hhmmss(d.tsMs)} · 置信度 {d.confidence.toFixed(2)} · LLM {d.llmUsed ? 1 : 0}
                  </span>
                  <button className="st-btn" onClick={() => void doExplain(d.seq)} title="展开解释链：为什么这次这么做">
                    为什么
                  </button>
                </div>
                <div className="st-dec-text">{d.chosen.text}</div>
                <div className="st-evidence">{d.rationale}</div>
                {d.receipt && <div className="st-evidence">回执：{d.receipt}</div>}
                <details className="st-dec-details">
                  <summary>依据 {d.ctxRefs.length} 条 · 备选 {d.candidates.length} 个</summary>
                  <div className="st-refs">
                    {d.ctxRefs.map((r, i) => (
                      <div key={`${r.id}-${i}`} className="st-ref">
                        <span className="st-badge">{REF_LABELS[r.source] ?? r.source}</span>
                        <span className="st-ref-id">{r.id}</span>
                        <span>{r.snippet}</span>
                      </div>
                    ))}
                  </div>
                  <div className="st-evidence">
                    备选：{d.candidates.map((c) => `${ACTION_LABELS[c.kind] ?? c.kind}「${c.text}」`).join(" / ") || "—"}
                  </div>
                </details>
                {showCard && card && (
                  <div className="st-esc">
                    <div className="st-esc-head">
                      升级卡 · 剩余 {left}s · 超时默认：{card.defaultAction.text}
                    </div>
                    <div className="st-esc-why">
                      建议：{card.recommendation.text}（{card.why.length} 条依据）
                    </div>
                    <div className="st-form-row">
                      <button className="st-btn primary" onClick={() => void approve(d.seq)}>
                        按建议执行
                      </button>
                      <input
                        className="st-input"
                        value={correction}
                        onChange={(e) => setCorrection(e.target.value)}
                        placeholder="可选：写下纠正，会存成 Learned 偏好"
                      />
                      <button className="st-btn danger" onClick={() => void reject(d.seq)}>
                        我自己来
                      </button>
                    </div>
                  </div>
                )}
              </div>
            );
          })}
        </div>
      </div>

      <div className="st-ev">
        <div className="st-sit-head">
          <span>观察事件 · 最近 {events.length} 条</span>
          <span className="st-meta">seq {runtime?.eventSeq ?? 0}</span>
        </div>
        <div className="st-ev-list">
          {events.length === 0 && <div className="st-empty">还没有事件</div>}
          {[...events].reverse().map((e) => (
            <div key={e.seq} className={`st-ev-row ${e.kind}`}>
              <span className="st-ev-time">{hhmmss(e.tsMs)}</span>
              <span className="st-ev-tag">{eventLabel(e.kind)}</span>
              <div className="st-ev-body">
                <div className="st-ev-msg">{e.message}</div>
                {e.suggestion && <div className="st-card">建议：{e.suggestion}</div>}
                {e.decision && (
                  <div className="st-card">
                    决策 #{e.decision.seq} · {ACTION_LABELS[e.decision.chosen.kind] ?? e.decision.chosen.kind} ·{" "}
                    {OUTCOME_LABELS[e.decision.outcome] ?? e.decision.outcome}
                  </div>
                )}
              </div>
            </div>
          ))}
        </div>
      </div>

      {explanation && (
        <div className="st-esc">
          <div className="st-esc-head">
            解释链 · 决策 #{explanation.seq} · {OUTCOME_LABELS[explanation.outcome] ?? explanation.outcome} ·{" "}
            {explanation.llmUsed ? "用过 LLM" : "零 LLM"}
          </div>
          <div className="st-esc-why">{explanation.whyAuto}</div>
          <div className="st-evidence">{explanation.rationale}</div>
          <div className="st-refs">
            {explanation.refs.map((r, i) => (
              <div key={`${r.id}-${i}`} className="st-ref">
                <span className="st-badge">{r.label}</span>
                <span className="st-ref-id">{r.id}</span>
                <span>{r.resolvedText ?? r.snippet}</span>
              </div>
            ))}
          </div>
        </div>
      )}

      {showReview && (
        <div className="st-dec-wrap">
          <div className="st-sit-head">
            <span>复盘 · {review ? review.contractId : contract?.id ?? "全部契约"}</span>
            <div>
              <button className="st-btn" onClick={() => void loadReview()} title="跨会话复盘 + 统一审计时间线">
                生成复盘
              </button>
              <button className="st-btn" onClick={() => void loadDistill()} title="预览可沉淀的剧本候选">
                蒸馏预览
              </button>
            </div>
          </div>
          {review && (
            <div className="st-card">
              <div className="st-esc-head">
                决策 {review.total} 条 · 生效 {review.applied} · 待确认 {review.escalated} · 你否掉{" "}
                {review.rejected} · 无回执 {review.noReceipt} · 注入 {review.injected}
              </div>
              <div className="st-evidence">高频依据：{review.topRefs.join(" / ") || "—"}</div>
              <div className="st-esc-why">已完成：{review.done.join("；") || "—"}</div>
              <div className="st-esc-why">未闭环：{review.pending.join("；") || "—"}</div>
              <div className="st-evidence">风险动作：{review.risks.join(" / ") || "无"}</div>
            </div>
          )}
          {audit.length > 0 && (
            <div className="st-ev-list">
              {[...audit]
                .sort((a, b) => b.tsMs - a.tsMs)
                .slice(0, 40)
                .map((a) => (
                  <div key={a.seq} className={`st-ev-row ${a.kind}`}>
                    <span className="st-ev-time">{hhmmss(a.tsMs)}</span>
                    <span className="st-ev-tag">{EVENT_LABELS[a.kind as StewardEventKind] ?? a.kind}</span>
                    <div className="st-ev-body">
                      <div className="st-ev-msg">{a.text}</div>
                    </div>
                  </div>
                ))}
            </div>
          )}
          {candidates.length > 0 && (
            <div className="st-form">
              <div className="st-form-title">剧本候选 · {candidates.length} 条（循环每 60s 自动写入 Brain）</div>
              {candidates.slice(0, 8).map((c) => (
                <div key={c.id} className="st-form-row">
                  <span className="st-badge">{c.occurrences} 次</span>
                  <span className="st-dec-text">{c.input.text.slice(0, 60)}</span>
                  <span className="st-meta">
                    {c.matchProgram ?? "任意程序"} · {c.matchKind ?? "任意局面"}
                  </span>
                </div>
              ))}
            </div>
          )}
        </div>
      )}

      <div className="st-form">
        <div className="st-form-title">
          托管目标 · {label(selected)}
          {contract && <span className="st-badge on">已签 {contract.status}</span>}
        </div>
        <textarea
          className="st-goal"
          value={goal}
          onChange={(e) => setGoal(e.target.value)}
          placeholder="例如：把 claude code 的钥匙串二次弹窗分析做完并落地修复，别改设置 UI，最小改动"
          rows={2}
        />
        <div className="st-form-row">
          <select
            className="st-select"
            value={tier}
            onChange={(e) => setTier(e.target.value as StewardTier)}
            title={TIERS.find((t) => t.value === tier)?.hint}
          >
            {TIERS.map((t) => (
              <option key={t.value} value={t.value}>
                {t.label}
              </option>
            ))}
          </select>
          <button className="st-btn primary" disabled={busy || !goal.trim()} onClick={() => void start()}>
            签契约
          </button>
          {contract && (
            <>
              <button className="st-btn" disabled={busy || !goal.trim()} onClick={() => void amend()}>
                改目标
              </button>
              <button className="st-btn danger" disabled={busy} onClick={() => void stop()}>
                停止托管
              </button>
            </>
          )}
        </div>
        {contract && (
          <div className="st-contract">
            {contract.id} · {contract.hostKey} · 档位 {contract.authorityTier} · 意向 {contract.intentIds.length} 条
          </div>
        )}
      </div>

      {showCtx && (
        <StewardContext contractId={contract?.id ?? null} onChanged={() => void refresh()} />
      )}

      <div className="st-screen">
        <div className="st-screen-head">
          后端屏幕快照
          {snapshot && (
            <span className="st-meta">
              {snapshot.cols}×{snapshot.rows} · {snapshot.alt ? "alt" : "normal"} · 帧 {snapshot.frameId} ·{" "}
              {snapshot.hash.slice(0, 8)}
            </span>
          )}
        </div>
        <pre className="st-pre">
          {snapshot
            ? snapshot.lines.slice(-12).join("\n") || "（空屏）"
            : "该 console 还没有上报过屏幕帧"}
        </pre>
      </div>
    </div>
  );
}
