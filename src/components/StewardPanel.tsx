/**
 * 托管控制台。**AI 托管**：写下目标 →「开始托管」→ 大模型（对话 Agent 的循环）当手和脑去执行，
 * Steward 的屏幕识别当眼睛盯屏；运行实况（步骤 / 审批 / 密码卡片）直接显示在本页。
 * 事件只订阅一份（在 AgentPanel 的 `useAgentRun` 里，本页经 `run` 桥读取与操作），
 * 所以审批 / 密码卡片不会在「对话」与「托管」两处各答一次。
 *
 * 下面是原有的零 LLM Steward（局面 / 决策 / 注入 / 回执），现在收进「诊断」。
 *
 * P2「托管控制台」：Steward 的眼睛（局面 / 事件）+ 手（决策 / 注入 / 回执）。
 *
 * P0 验屏幕帧持续同步、P1 验常驻只读观察，P2 在同一面板里补上：
 * 决策记录与依据引用、升级卡（建议 / 备选 / 倒计时 / 一键确认）、自动应答总闸、
 * 以及用户预先承载的偏好 / 意向 / 知识 / 剧本编辑区。
 *
 * 布局：顶部一张主卡（目标终端 / 目标 / 开始停止 / 档位 / 接管交还 / 自动应答 / 一行状态），
 * 下面是终端列表、局面、决策、事件；计数器 / 阈值 / 帧指纹等内部细节收进「诊断」。
 *
 * 决策编译零 LLM；自动应答每次冷启动默认关闭，打开后密码 / 永久扩权 / 断流 /
 * 低置信度 / approve 档依然 fail-closed。设计见 docs/nexus-agent-steward-design.md。
 */

import { useCallback, useEffect, useRef, useState } from "react";
import type { UnlistenFn } from "@tauri-apps/api/event";
import type {
  AgentApproveRequest,
  AgentMessage,
  AgentSecretRequest,
  AgentTarget,
  HostStatus,
} from "../types";
import HostCard from "./HostCard";
import { hostPill, isTerminal, needsReattach } from "../lib/host";
import { pendingPrompts } from "../lib/agentRun";
import {
  BUDGET_EXHAUSTED_BANNER,
  LS_STEWARD_BUDGET,
  STEWARD_DURATION_OPTIONS,
  MAX_STEPS_CEIL,
  isBudgetPause,
  parseStewardBudgetPref,
  type PauseKind,
  type StewardBudgetPref,
} from "../lib/agentBudget";
import { RunFeed } from "./RunFeed";
import {
  ACTION_LABELS,
  EVENT_LABELS,
  OUTCOME_LABELS,
  REF_LABELS,
  TIER_LABELS,
  authorityToSave,
  contractStatusLabel,
  decisionAwaiting,
  eventLabel,
  programLabel,
  situationLabel,
  stewardApi,
  stewardConsoleIds,
  stewardStatusLine,
  syncAuthDraft,
  tierLabel,
  type AbsenceSummary,
  type AuditEntry,
  type AuthDraft,
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
import { captureNow, setStewardConsoles } from "../lib/termRegistry";
import StewardContext from "./StewardContext";

/**
 * AgentPanel 传进来的「运行桥」：同一份 `useAgentRun` 的只读视图 + 动作。
 * 本页不自己订阅任何 agent 事件，也不自己调 agent_approve / agent_secret_ack。
 */
export interface StewardRunBridge {
  /** 托管那一轮对应的托管任务（相位 / 卡片），没有为 null */
  host: HostStatus | null;
  /** 任意终端上的托管任务（终端列表里的胶囊用） */
  hostFor: (consoleId: string) => HostStatus | null;
  /** 等重新接驳的托管（重启后 / 终端关了） */
  awaitingReattach: HostStatus[];
  /** 托管那一轮发起在哪个终端（最近一次托管；非托管起手的一轮为 null） */
  session: string | null;
  /** 托管那一轮此刻正在跑 */
  active: boolean;
  /** 另有一轮「对话 / 技能」在跑（同一时刻只允许一轮，此时不能开始托管） */
  otherBusy: boolean;
  /** 托管那一轮的消息（步骤 / 审批 / 密码卡片 / 结论） */
  messages: AgentMessage[];
  /** 这一轮停在暂停上（预算用尽 / 手动暂停）→ 可「继续」 */
  paused: boolean;
  pauseText?: string;
  /** 暂停原因：steps / time / output = 预算用尽，user = 手动暂停 */
  pauseKind?: PauseKind;
  /** 用户敲键夺权，Agent 已让路 → 可「交还」 */
  userPaused: boolean;
  /** Agent 正在驾驶终端 → 可「接管」 */
  driving: boolean;
  stopCtrlC: boolean;
  toggleStopCtrlC: (on: boolean) => void;
  /** 开始托管（`host_start`）：后端起好任务就 resolve，失败会作为错误消息出现在实况里 */
  start: (
    sessionId: string,
    goal: string,
    budget?: StewardBudgetPref,
    tier?: string,
    yolo?: boolean
  ) => Promise<void>;
  cancel: () => Promise<void>;
  pause: () => Promise<void>;
  /** 加预算继续：给了预算偏好就按它换一份新上限 */
  resume: (budget?: StewardBudgetPref) => Promise<void>;
  takeover: () => void;
  handback: () => void;
  decide: (req: AgentApproveRequest, approved: boolean) => Promise<void>;
  answerSecret: (req: AgentSecretRequest, value?: string) => Promise<string | null>;
  /** Nexus AI Agent 是否已配置好；没配好就不能开始托管 */
  ready: boolean;
  notReadyReason?: string;
  /** agent 动作（取消 / 暂停 / 继续 / 审批）失败的一行文案 */
  error: string | null;
}

export interface StewardPanelProps {
  /** 面板当前绑定的终端，作为默认选中项；切标签时跟着走 */
  sessionId: string;
  /** 标签树里的可读名字，由 App 传入；后端只认 id */
  sessions?: AgentTarget[];
  /** 托管那一轮的运行桥（由 AgentPanel 提供） */
  run: StewardRunBridge;
}

const TIERS: Array<{ value: StewardTier; label: string; hint: string }> = [
  { value: "auto", label: TIER_LABELS.auto, hint: "范围内自主执行，事后可查" },
  { value: "notify", label: TIER_LABELS.notify, hint: "自主执行并立即通知，30s 内可撤销" },
  { value: "approve", label: TIER_LABELS.approve, hint: "先给建议 + 倒计时，你一键确认后才写" },
];

const OWNERSHIP_LABELS: Record<OwnershipState, string> = {
  agent_driving: "托管中（可替你应答）",
  observing_only: "你接管中（Steward 只观察）",
  suspended: "已暂停（保留现场）",
};

/** console 列表上的短徽标：后端 status 是该 console 的真实所有权态 */
const CONSOLE_OWNER_BADGE: Record<string, string> = {
  agent_driving: "托管中",
  observing_only: "你接管中",
  suspended: "已暂停",
};

/** 事件流后端最多留 200 条：全拉下来，局面按 console 过滤，列表只显示最近 60 条 */
const EVENTS_FETCH = 200;
const EVENTS_SHOWN = 60;

const hhmmss = (ms: number) => {
  try {
    return new Date(ms).toLocaleTimeString("zh-CN", { hour12: false });
  } catch {
    return String(ms);
  }
};

export default function StewardPanel({ sessionId, sessions, run }: StewardPanelProps) {
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
  /** YOLO：直接采用 AI 起草的完成判据、不经确认就开始。每次打开面板默认关 */
  const [yolo, setYolo] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  /** 本轮托管的长度偏好（最长时长 / 最多步数），记在本机 */
  const [budgetPref, setBudgetPref] = useState<StewardBudgetPref>(() =>
    parseStewardBudgetPref(localStorage.getItem(LS_STEWARD_BUDGET))
  );
  const updateBudgetPref = (patch: Partial<StewardBudgetPref>) => {
    const next = parseStewardBudgetPref(JSON.stringify({ ...budgetPref, ...patch }));
    setBudgetPref(next);
    localStorage.setItem(LS_STEWARD_BUDGET, JSON.stringify(next));
    return next;
  };
  /** 步数输入框的草稿文本：输入过程中允许为空 / 不合法，失焦或回车才解析、夹紧、落盘 */
  const [stepsDraft, setStepsDraft] = useState<string | null>(null);
  const commitSteps = () => {
    if (stepsDraft === null) return;
    updateBudgetPref({ maxSteps: Number(stepsDraft) });
    setStepsDraft(null);
  };
  const [clock, setClock] = useState(Date.now());
  const [correction, setCorrection] = useState("");
  const selectedRef = useRef(selected);
  selectedRef.current = selected;
  /** 用户在列表里手动点过别的终端（非当前标签）时记下；为 null 时跟随当前标签 */
  const pickedRef = useRef<string | null>(null);

  const loadOnce = useCallback(async () => {
    try {
      const [list, signed, rt, evs, recs, cv, cps, snap] = await Promise.all([
        stewardApi.listConsoles(),
        stewardApi.status(),
        stewardApi.runtime(),
        // 全量拉取避免前端游标在「清空事件」后倒退，seq 由后端单调分配。
        stewardApi.events(0, EVENTS_FETCH),
        stewardApi.decisions(20),
        stewardApi.coreView(),
        stewardApi.checkpoints(),
        stewardApi.screenGet(selectedRef.current),
      ]);
      setConsoles(list);
      setContracts(signed);
      // 契约列表顺手喂给采集门控：新签的契约立刻补一帧全量，停掉的不再采
      setStewardConsoles(stewardConsoleIds(signed));
      setRuntime(rt);
      setEvents(evs);
      setDecisions(recs);
      setCore(cv);
      setCheckpoints(cps);
      setSnapshot(snap);
      setErr(null);
    } catch (e) {
      setErr(`读取常驻 Steward 失败：${String(e)}`);
    }
  }, []);

  // 刷新同一时刻只跑一轮：轮询 / 事件推送 / 按钮回调撞在一起时，后到的请求只标记
  // 「再来一轮」，等当前这轮落地后接着跑 —— 结果按顺序落地，也不会丢掉最后一次刷新。
  const inflightRef = useRef<Promise<void> | null>(null);
  const againRef = useRef(false);
  const refresh = useCallback((): Promise<void> => {
    if (inflightRef.current) {
      againRef.current = true;
      return inflightRef.current;
    }
    const run = async () => {
      do {
        againRef.current = false;
        await loadOnce();
      } while (againRef.current);
    };
    const p = run().finally(() => {
      inflightRef.current = null;
    });
    inflightRef.current = p;
    return p;
  }, [loadOnce]);

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

  // 跟随当前标签：除非用户在列表里明确点了别的终端
  useEffect(() => {
    if (pickedRef.current !== null) return;
    setSelected(sessionId);
  }, [sessionId]);

  // 换了终端立刻拉一次快照，不等下一轮轮询
  useEffect(() => {
    setManual(null);
    void refresh();
  }, [selected, refresh]);

  const contract = contracts.find((c) => c.consoleId === selected) ?? null;
  const contractId = contract?.id ?? null;

  const label = (id: string) => sessions?.find((s) => s.id === id)?.label ?? id;
  /** 契约 id 形如 steward-<tabId>-<ts>，人看不懂；换成它所属终端的名字 */
  const contractName = (cid: string) => {
    const consoleId =
      contracts.find((c) => c.id === cid)?.consoleId ??
      checkpoints.find((c) => c.contractId === cid)?.consoleId;
    return consoleId ? label(consoleId) : cid;
  };
  const state = runtime?.consoles.find((c) => c.consoleId === selected) ?? null;
  const consoleInfo = consoles.find((c) => c.id === selected) ?? null;
  const own = core?.ownership.find((o) => o.consoleId === selected) ?? null;
  const undoList = core?.liveUndos ?? [];
  const contractAuth: Authority | null = contract
    ? (core?.authorities.find(([id]) => id === contract.id)?.[1] ?? null)
    : null;

  // 目标框 / 档位：按「终端 + 契约 id」加载。换终端或契约换了才重填，用户正在改的不被轮询冲掉；
  // 同一份契约在后端换了档（交还时屏幕漂移会自动降「需确认」档）也跟上。
  const formRef = useRef<{ console: string; contractId: string | null; tier: StewardTier | null }>({
    console: "",
    contractId: null,
    tier: null,
  });
  useEffect(() => {
    const prev = formRef.current;
    const serverTier = contract?.authorityTier ?? null;
    formRef.current = { console: selected, contractId, tier: serverTier };
    if (contract && (prev.console !== selected || prev.contractId !== contractId)) {
      setGoal(contract.goal);
      setTier(contract.authorityTier);
    } else if (!contract && prev.console !== selected) {
      setGoal("");
      setTier("notify");
    } else if (contract && serverTier !== prev.tier) {
      setTier(contract.authorityTier);
    }
    // contract 本身每轮轮询都是新对象，只看 id / 档位
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selected, contractId, contract?.authorityTier]);

  // 授权草稿按契约 id 绑定（见 syncAuthDraft）；依赖用内容而不是对象，避免每轮轮询都重同步
  const [authDraft, setAuthDraft] = useState<AuthDraft | null>(null);
  const authKey = contractAuth ? JSON.stringify(contractAuth) : "";
  useEffect(() => {
    setAuthDraft((prev) => syncAuthDraft(prev, contractId, contractAuth));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [contractId, authKey]);
  const draft = authDraft && authDraft.contractId === contractId ? authDraft : null;
  const editDraft = (patch: Partial<Authority>) =>
    setAuthDraft((d) => (d ? { ...d, authority: { ...d.authority, ...patch }, dirty: true } : d));

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

  const doResume = async (cid: string) => {
    try {
      await stewardApi.resume(cid);
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
    // 草稿只能写回它来源的那份契约；档位以契约当前值为准（只由顶部选择框改）
    const toSave = authorityToSave(draft, contractId, contractAuth);
    if (!contractId || !toSave) return;
    setBusy(true);
    try {
      const saved = await stewardApi.setAuthority(contractId, toSave);
      setAuthDraft((d) =>
        d && d.contractId === contractId ? { contractId, authority: saved, dirty: false } : d
      );
      await refresh();
    } catch (e) {
      setErr(`更新授权失败：${String(e)}`);
    } finally {
      setBusy(false);
    }
  };

  // 局面卡：「立即分类」与事件流里该 console 最近一条局面，取帧更新的那个
  const latestEventSituation =
    [...events]
      .reverse()
      .find((e) => e.consoleId === selected && e.situation)?.situation ?? null;
  const situation =
    manual && (!latestEventSituation || manual.frameId >= latestEventSituation.frameId)
      ? manual
      : latestEventSituation;

  const awaiting = decisions.filter((d) => d.consoleId === selected && decisionAwaiting(d, clock)).length;
  const status = stewardStatusLine({
    hasContract: !!contract,
    ownership: own?.state ?? null,
    suspensionReason: own?.suspensionReason ?? null,
    executionEnabled: !!runtime?.executionEnabled,
    awaiting,
    situation: state?.kind ?? situation?.kind ?? null,
    stale: state?.stale ?? false,
  });

  /**
   * 开始托管 = 后端 `host_start`：先拿这个终端的租约，再签契约（前端据此开始上报屏幕帧 —— 眼睛），
   * 最后起常驻的托管任务（手和脑）。顺序在后端保证：契约出现时守护进程已经看得到租约，只会观察。
   * 托管任务只有完成 / 失败 / 停止才结束；问你问题、你接管、预算用尽都只是挂起。
   */
  const startHosting = async () => {
    if (!selected || !goal.trim() || busy || run.active || run.otherBusy || !run.ready) return;
    setBusy(true);
    try {
      await run.start(selected, goal.trim(), budgetPref, tier, yolo);
      await refresh();
    } finally {
      setBusy(false);
    }
  };

  /** 停止托管：先停这一轮 Agent（幂等），再把契约一并停掉 —— 否则零 LLM 的 Steward 会接着在该终端上动手 */
  const stopHosting = async () => {
    if (busy) return;
    setBusy(true);
    try {
      if (run.active) await run.cancel();
      if (contract) await stewardApi.stop(contract.id);
      setManual(null);
      await refresh();
    } catch (e) {
      setErr(`停止托管失败：${String(e)}`);
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
    } catch (e) {
      setErr(`停止托管失败：${String(e)}`);
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
    } catch (e) {
      setErr(`更新目标 / 档位失败：${String(e)}`);
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
      setErr(`调整空转阈值失败：${String(e)}`);
    }
  };

  const clear = async () => {
    try {
      await stewardApi.clearEvents();
      setManual(null);
      await refresh();
    } catch (e) {
      setErr(`清空事件失败：${String(e)}`);
    }
  };

  const clearDecisions = async () => {
    try {
      await stewardApi.clearDecisions();
      await refresh();
    } catch (e) {
      setErr(`清空决策失败：${String(e)}`);
    }
  };

  const execOn = !!runtime?.executionEnabled;
  const driving = !own || own.state === "agent_driving";
  // 目标框 / 档位改过还没保存到契约上
  const formDirty = !!contract && (tier !== contract.authorityTier || goal.trim() !== contract.goal);
  /** 托管那一轮发起在当前选中的终端上：实况与控制键才在这里显示 */
  const runHere = run.session !== null && run.session === selected;
  const promptsPending = runHere ? pendingPrompts(run.messages) : 0;
  /** 选中终端上的托管任务（含刚结束的结局） */
  const selectedHost = selected ? run.hostFor(selected) : null;
  /** 状态行：AI 托管在跑时说「它现在在干什么」，否则回落到规则式 Steward 的状态 */
  const runStatus: { text: string; tone: "idle" | "active" | "warn" } =
    run.active && !runHere
      ? { text: `AI 托管正在「${label(run.session ?? "")}」上运行，切到该终端查看实况`, tone: "active" }
      : run.active && run.paused && isBudgetPause(run.pauseKind)
        ? { text: BUDGET_EXHAUSTED_BANNER, tone: "warn" }
      : run.active && run.paused
        ? { text: run.pauseText?.split("\n")[0] || "已暂停：点「继续」接着做，或「停止」", tone: "warn" }
        : run.active && run.userPaused
          ? { text: "你在操作终端，AI 已让路 — 点「交还」让它继续", tone: "warn" }
          : run.active && promptsPending > 0
            ? { text: "AI 托管中：有一项等你批准 / 输入", tone: "warn" }
            : run.active
              ? { text: "AI 托管中：大模型在执行，屏幕识别在盯屏。你一敲键它就让路。", tone: "active" }
              : run.otherBusy
                ? { text: "对话 / 技能正在运行，结束后才能开始托管", tone: "warn" }
                : !run.ready
                  ? { text: run.notReadyReason ?? "Nexus AI Agent 尚未配置完成，请先去设置", tone: "warn" }
                  : !contract
                    ? { text: "未开始：选择一个终端，写下目标，点「开始托管」", tone: "idle" }
                    : { text: status.text, tone: status.tone };

  /** 一条决策记录；`withCard` 为真时（且仍在等你确认）带上「按建议执行 / 我自己来」升级卡 */
  const renderDecision = (d: DecisionRecord, withCard: boolean) => {
            const card = d.escalation;
            const left = card
              ? Math.max(0, Math.ceil((card.createdAt + card.timeoutMs - clock) / 1000))
              : 0;
            const showCard = withCard && decisionAwaiting(d, clock);
            return (
              <div key={d.seq} className={`st-dec ${d.outcome}`}>
                <div className="st-dec-head">
                  <span className="st-dec-seq">#{d.seq}</span>
                  <span className="st-kind">{ACTION_LABELS[d.chosen.kind] ?? d.chosen.kind}</span>
                  <span className="st-badge">{tierLabel(d.tier)}</span>
                  <span className="st-badge outcome">{OUTCOME_LABELS[d.outcome] ?? d.outcome}</span>
                  <span className="st-meta">
                    {label(d.consoleId)} · {hhmmss(d.tsMs)} · 置信度 {d.confidence.toFixed(2)} · LLM{" "}
                    {d.llmUsed ? 1 : 0}
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
                      待你确认 · 剩余 {left}s · 超时默认：{card.defaultAction.text}
                    </div>
                    <div className="st-esc-why">
                      建议：{card.recommendation.text}（{card.why.length} 条依据）
                    </div>
                    <div className="st-form-row st-center">
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
  };

  return (
    <div className="ap-steward">
      <div className="st-main">
        <div className="st-main-head">
          <span className={`st-dot${runtime?.running ? " live" : ""}`} title={runtime?.running ? "Steward 运行中" : "Steward 未运行"} />
          <span className="st-name">目标终端：{label(selected)}</span>
          {runHere && run.active && <span className="st-badge on">AI 托管中</span>}
          {contract && <span className="st-badge on">{contractStatusLabel(contract.status)}</span>}
          {contract && own && <span className="st-badge">{OWNERSHIP_LABELS[own.state]}</span>}
        </div>
        <textarea
          className="st-goal"
          value={goal}
          disabled={run.active}
          onChange={(e) => setGoal(e.target.value)}
          placeholder="写下要它完成的事，例如：把 /srv/app 的依赖升级后跑完测试并汇报；或：盯着这个安装脚本跑完，遇到确认就替我答"
          rows={3}
        />
        <div className="st-form-row st-center">
          {!run.active ? (
            <>
              <button
                className="st-btn primary"
                disabled={busy || !goal.trim() || run.otherBusy || !run.ready}
                onClick={() => void startHosting()}
                title={
                  !run.ready
                    ? (run.notReadyReason ?? "请先完成 Nexus AI Agent 配置")
                    : run.otherBusy
                      ? "对话 / 技能正在运行，结束后才能开始托管"
                      : "签下托管契约（开始盯屏）并让 AI 去执行这个目标"
                }
              >
                开始托管
              </button>
              <label className="st-meta" title="这一轮最长跑多久，到点会暂停等你点「继续」（后端上限 24 小时）">
                最长
                <select
                  className="st-select"
                  value={budgetPref.maxMinutes}
                  disabled={busy}
                  onChange={(e) => updateBudgetPref({ maxMinutes: Number(e.target.value) })}
                >
                  {STEWARD_DURATION_OPTIONS.map((o) => (
                    <option key={o.minutes} value={o.minutes}>
                      {o.label}
                    </option>
                  ))}
                </select>
              </label>
              <label className="st-meta" title={`最多执行多少步动作（后端上限 ${MAX_STEPS_CEIL}）`}>
                最多
                <input
                  className="st-num"
                  type="number"
                  min={1}
                  max={MAX_STEPS_CEIL}
                  value={stepsDraft ?? String(budgetPref.maxSteps)}
                  disabled={busy}
                  onChange={(e) => setStepsDraft(e.target.value)}
                  onBlur={commitSteps}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") commitSteps();
                  }}
                />
                步
              </label>
              <label
                className={`st-meta host-yolo${yolo ? " on" : ""}`}
                title="开启后直接采用 AI 起草的完成判据，不再请你确认就开始托管。只跳过这一步：授权档位和禁止项照常生效。"
              >
                <input type="checkbox" checked={yolo} disabled={busy} onChange={(e) => setYolo(e.target.checked)} />
                YOLO（跳过判据确认）
              </label>
              {contract && (
                <button className="st-btn danger" disabled={busy} onClick={() => void stop()}>
                  停止托管
                </button>
              )}
            </>
          ) : (
            <>
              {run.paused ? (
                <button className="st-btn primary" onClick={() => void run.resume(budgetPref)} title="同一段上下文、换一份新预算接着做">
                  加预算继续
                </button>
              ) : run.userPaused ? (
                <button className="st-btn primary" onClick={run.handback} title="把终端交还给 AI，它从暂停处继续">
                  交还
                </button>
              ) : (
                <>
                  <button className="st-btn" onClick={() => void run.pause()} title="当前这一步做完后暂停，终端还给你；之后可以「继续」">
                    暂停
                  </button>
                  {run.driving && (
                    <button className="st-btn" onClick={run.takeover} title="让 AI 立刻让路（你在终端里敲键也会自动触发）">
                      接管
                    </button>
                  )}
                </>
              )}
              <button className="st-btn danger" disabled={busy} onClick={() => void stopHosting()} title="停止这一轮并结束托管契约">
                停止
              </button>
              <label className="st-meta" title="停止时同时往被驾驶的终端发送 Ctrl-C，打断它正在跑的命令">
                <input type="checkbox" checked={run.stopCtrlC} onChange={(e) => run.toggleStopCtrlC(e.target.checked)} />
                同时发 Ctrl-C
              </label>
            </>
          )}
        </div>
        {runHere && run.active && run.paused && isBudgetPause(run.pauseKind) && (
          <div className="st-budget-banner" role="alert">
            <span>{BUDGET_EXHAUSTED_BANNER}</span>
            <button className="st-btn primary" onClick={() => void run.resume(budgetPref)} title="同一段上下文、换一份新预算接着做">
              加预算继续
            </button>
            <button className="st-btn danger" disabled={busy} onClick={() => void stopHosting()}>
              停止
            </button>
          </div>
        )}
        {!(runHere && run.active && run.paused && isBudgetPause(run.pauseKind)) && (
          <div className={`st-status ${runStatus.tone}`}>{runStatus.text}</div>
        )}
        {absence && (
          <div className="st-card">
            <div className="st-esc-head">
              离开 {Math.round(absence.durationMs / 1000)}s · 你接管 {absence.takeoverCount} 次 · 屏幕
              {absence.screenAligned ? "对齐" : "漂移（已降「需确认」档）"}
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
      </div>

      <div className="st-note">
        <strong>AI 托管</strong>：由大模型替你在这个终端里执行命令、操作交互程序（含 Claude Code 之类的控制台工具），
        并用屏幕识别盯着长任务跑完。危险命令仍会先问你；密码只会请你本人输入；不会替你选「不再询问 / 永久允许」这类选项。
        下方「诊断」里是原有的规则式 Steward（不调大模型）的细节。
      </div>

      {(err || run.error) && <div className="st-err">{err ?? run.error}</div>}

      {/* 待重新接驳：重启后 console id 都变了，不跟选中的终端走，全部列出；接驳要你确认 */}
      {run.awaitingReattach.map((h) => (
        <HostCard key={h.contractId} status={h} label={h.label} tabs={sessions} stopCtrlC={run.stopCtrlC} />
      ))}

      {/* 选中终端上的托管：胶囊 + 等你回答 / 批准的卡片（永远等下去，没有倒计时） */}
      {selectedHost && !needsReattach(selectedHost.phase) && (
        <HostCard
          status={selectedHost}
          label={label(selectedHost.consoleId)}
          grant={{ maxSteps: budgetPref.maxSteps, maxMinutes: budgetPref.maxMinutes }}
          stopCtrlC={run.stopCtrlC}
        />
      )}

      {runHere && (run.active || run.messages.length > 0) && (
        <div className="st-run">
          <div className="st-sit-head">
            <span>托管实况 · {label(selected)}</span>
            <span className="st-meta">{run.active ? (run.paused ? "已暂停" : "执行中") : "已结束"}</span>
          </div>
          {promptsPending > 0 && (
            <div className="st-status warn">有 {promptsPending} 项等你处理（批准 / 输入密码），见下方卡片</div>
          )}
          <RunFeed
            messages={run.messages}
            busy={run.active}
            paused={run.paused}
            pauseText={run.pauseText}
            onDecide={(req, ok) => void run.decide(req, ok)}
            onAnswerSecret={run.answerSecret}
          />
        </div>
      )}

      {/* 规则式 Steward 的待确认建议：要你点头才执行，不能藏进折叠的诊断里 */}
      {decisions
        .filter((d) => d.consoleId === selected && decisionAwaiting(d, clock))
        .map((d) => renderDecision(d, true))}

      <div className="st-head">
        <span>终端 · {consoles.length} 个</span>
        <div>
          <button className="st-btn" onClick={() => void refresh()}>
            刷新
          </button>
        </div>
      </div>

      <div className="st-list">
        {consoles.length === 0 && <div className="st-empty">还没有检测到终端</div>}
        {consoles.map((c) => {
          const cs = runtime?.consoles.find((x) => x.consoleId === c.id);
          const h = run.hostFor(c.id);
          const pill = h && !isTerminal(h.phase) ? hostPill(h.phase) : null;
          return (
            <button
              key={c.id}
              className={`st-row${selected === c.id ? " on" : ""}`}
              onClick={() => {
                // 点回当前标签 = 恢复跟随；点别的 = 固定在它上面
                pickedRef.current = c.id === sessionId ? null : c.id;
                setSelected(c.id);
              }}
            >
              <span className={`st-dot${c.alive ? " live" : ""}`} />
              <span className="st-name">{label(c.id)}</span>
              {pill && <span className={`st-badge host-pill ${pill.tone}`}>{pill.label}</span>}
              <span className="st-badge">{c.kind}</span>
              {c.alt && <span className="st-badge alt">全屏</span>}
              {c.status !== "none" && (
                <span className={`st-badge${c.status === "agent_driving" ? " on" : ""}`}>
                  {CONSOLE_OWNER_BADGE[c.status] ?? "托管中"}
                </span>
              )}
              {cs?.kind && <span className="st-badge situation">{situationLabel(cs.kind)}</span>}
              {cs?.injectedCount ? <span className="st-badge hot">注入 {cs.injectedCount}</span> : null}
            </button>
          );
        })}
      </div>

      <div className="st-sit">
        <div className="st-sit-head">
          <span>当前局面 · {label(selected)}</span>
          <span className="st-meta">
            {state
              ? `${state.stale ? "帧断流" : "帧新鲜"}${state.pendingReceipt ? " · 等回执" : ""}`
              : "该终端尚未托管"}
          </span>
        </div>
        {situation ? (
          <div className="st-sit-body">
            <div className="st-sit-line">
              <span className="st-kind">{situationLabel(situation.kind)}</span>
              <span className="st-badge">{programLabel(situation.program)}</span>
              <span className="st-meta">置信度 {situation.confidence.toFixed(2)}</span>
              <span className="st-meta" title="屏幕没变时心跳帧只推进帧号，不会产生新的局面记录">
                识别于帧 {situation.frameId}
                {consoleInfo?.hasScreen && consoleInfo.frameId !== situation.frameId
                  ? `（之后未变，当前帧 ${consoleInfo.frameId}）`
                  : ""}
              </span>
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
            还没有局面记录。开始托管后，终端屏幕一上报这里就会出现第一张局面卡片。
          </div>
        )}
      </div>

      <details className="st-diag">
        <summary>诊断（规则式 Steward：决策 / 事件 / 屏幕快照 / 授权 / 复盘）</summary>
        <div className="st-diag-body">
          <div className="st-form-row st-center">
            <select
              className="st-select"
              value={tier}
              onChange={(e) => setTier(e.target.value as StewardTier)}
              title={"规则式 Steward 的档位：" + (TIERS.find((t) => t.value === tier)?.hint ?? "")}
            >
              {TIERS.map((t) => (
                <option key={t.value} value={t.value}>
                  {t.label}
                </option>
              ))}
            </select>
            {contract && (
              <button
                className={`st-btn${formDirty ? " primary" : ""}`}
                disabled={busy || !goal.trim() || !formDirty}
                onClick={() => void amend()}
                title="热更新目标与档位，不重启托管"
              >
                保存修改
              </button>
            )}
            {contract &&
              (driving ? (
                <button className="st-btn" onClick={() => void takeover()} title="规则式 Steward：立刻转观察态，绝不再写这个终端">
                  我来接管
                </button>
              ) : (
                <button className="st-btn primary" onClick={() => void handback()} title="交还给规则式 Steward：先读屏对齐再给「离开期间」摘要">
                  交还托管
                </button>
              ))}
            <button
              className={`st-btn${execOn ? " warn" : ""}`}
              onClick={() => void toggleExecution()}
              title="规则式 Steward 的总闸（与 AI 托管无关）：每次冷启动默认关闭；打开后仍受安全策略 fail-closed 约束"
            >
              {execOn ? "规则自动应答：开" : "规则自动应答：关（只建议）"}
            </button>
          <button className="st-btn" onClick={() => void classifyNow()} title="当场分类一帧，核对判据">
            立即分类
          </button>
          <button className="st-btn" onClick={() => void decideNow()} title="当场编译一条决策并落库，不注入">
            立即决策
          </button>
          <button className="st-btn" onClick={() => setShowCtx((v) => !v)}>
            {showCtx ? "收起依据" : "编辑依据"}
          </button>
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
          {[...decisions].reverse().map((d) => renderDecision(d, false))}
        </div>
      </div>

      <div className="st-runtime">
        <button className="st-btn" onClick={() => setShowOps((v) => !v)}>
          {showOps ? "收起托管核心" : "托管核心（撤销 / 授权 / 恢复）"}
        </button>
        <button className="st-btn" onClick={() => setShowReview((v) => !v)}>
          {showReview ? "收起复盘" : "复盘（审计 / 蒸馏 / 解释链）"}
        </button>
        <button
          className={`st-btn${core?.plannerEnabled ? " warn" : ""}`}
          onClick={() => void togglePlanner()}
          title="LLM Planner 兜底总闸：默认关闭、不持久化；只有规则编不出回复时才调模型，结果仍过 PolicyEngine"
        >
          Planner：{core?.plannerEnabled ? "开" : "关"}
          {core ? `（在途 ${core.llmInflight}/2）` : ""}
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
                : "这个终端还没有所有权槽（开始托管后创建）"}
            </span>
          </div>
          {core?.plannerEnabled && (
            <div className="st-form-row">
              <button
                className="st-btn"
                onClick={() => void doPlanNow()}
                title="真·LLM 兜底：规则编不出回复时调模型，结果仍要过 PolicyEngine"
              >
                立即规划
              </button>
            </div>
          )}
          {undoList.length > 0 && (
            <div className="st-esc">
              <div className="st-esc-head">
                撤销窗口 · {undoList.length} 条（「通知后执行」档执行后 30s 内可一键回滚）
              </div>
              {undoList.map((u) => (
                <div key={u.seq} className="st-form-row st-center">
                  <span className="st-dec-seq">#{u.seq}</span>
                  <span className="st-ellipsis" title={u.action.text}>
                    {label(u.consoleId)} · {ACTION_LABELS[u.action.kind] ?? u.action.kind}「{u.action.text}」
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
          {contract && draft && (
            <div className="st-form">
              <div className="st-form-title">
                授权 · {label(contract.consoleId)}
                <span className="st-meta">档位用顶部的选择框改</span>
              </div>
              <div className="st-form-row st-center">
                <label className="st-meta" title="允许自动选中 don't ask again 类永久扩权选项；强烈建议关闭">
                  <input
                    type="checkbox"
                    checked={draft.authority.allowPermanentGrant}
                    onChange={(e) => editDraft({ allowPermanentGrant: e.target.checked })}
                  />
                  永久扩权
                </label>
                <label className="st-meta">
                  预算步数
                  <input
                    className="st-input"
                    type="number"
                    min={0}
                    value={draft.authority.maxSteps}
                    onChange={(e) => editDraft({ maxSteps: Number(e.target.value) || 0 })}
                  />
                </label>
                <label className="st-meta">
                  预算分钟
                  <input
                    className="st-input"
                    type="number"
                    min={0}
                    value={draft.authority.maxMinutes}
                    onChange={(e) => editDraft({ maxMinutes: Number(e.target.value) || 0 })}
                  />
                </label>
                <label className="st-meta" title="意向 deadline（分钟），0 = 不限；到期收尾摘要 + 保留现场">
                  deadline 分钟
                  <input
                    className="st-input"
                    type="number"
                    min={0}
                    value={Math.round(draft.authority.timeboxMs / 60000)}
                    onChange={(e) => editDraft({ timeboxMs: (Number(e.target.value) || 0) * 60000 })}
                  />
                </label>
                <button className="st-btn primary" disabled={busy || !draft.dirty} onClick={() => void saveAuthority()}>
                  保存授权
                </button>
                {draft.dirty && contractAuth && (
                  <button
                    className="st-btn"
                    onClick={() => setAuthDraft({ contractId: contract.id, authority: contractAuth, dirty: false })}
                  >
                    放弃修改
                  </button>
                )}
              </div>
              <div className="st-evidence">
                硬禁：{contractAuth?.deny.join(" / ") || "—"} · 提权白名单：
                {contractAuth?.allow.join(" / ") || "—"} · host 作用域：
                {contractAuth?.hostScope.join(" / ") || "全部已注册终端"}
              </div>
            </div>
          )}
          <div className="st-form">
            <div className="st-form-title">现场恢复 · {checkpoints.length} 份</div>
            {checkpoints.length === 0 && (
              <div className="st-empty">还没有落盘的现场。每个决策闭环或每 30s 自动保存一次。</div>
            )}
            {checkpoints.slice(0, 5).map((cp) => (
              <div key={cp.contractId} className="st-form-row st-center st-nowrap">
                <span className="st-badge">{label(cp.consoleId)}</span>
                <span className="st-ellipsis" title={cp.goal || "（无目标）"}>
                  {cp.goal || "（无目标）"}
                </span>
                <span className="st-meta">
                  {contractStatusLabel(cp.state)} · {tierLabel(cp.authority.tier)} · 步 {cp.spent.steps} · 决策{" "}
                  {cp.lastDecisions.length} 条
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
            <span>
              复盘 · {review ? contractName(review.contractId) : contract ? label(contract.consoleId) : "全部契约"}
            </span>
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
                <div key={c.id} className="st-form-row st-center st-nowrap">
                  <span className="st-badge">{c.occurrences} 次</span>
                  <span className="st-ellipsis" title={c.input.text}>
                    {c.input.text}
                  </span>
                  <span className="st-meta">
                    {c.matchProgram ?? "任意程序"} · {c.matchKind ?? "任意局面"}
                  </span>
                </div>
              ))}
            </div>
          )}
        </div>
      )}

      {showCtx && (
        <StewardContext contractId={contract?.id ?? null} onChanged={() => void refresh()} />
      )}

      <div className="st-ev">
        <div className="st-sit-head">
          <span>观察事件 · 最近 {Math.min(events.length, EVENTS_SHOWN)} 条</span>
        </div>
        <div className="st-ev-list">
          {events.length === 0 && <div className="st-empty">还没有事件</div>}
          {events
            .slice(-EVENTS_SHOWN)
            .reverse()
            .map((e) => (
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

      <div className="st-screen">
        <div className="st-screen-head">
          后端屏幕快照
          {snapshot && (
            <span className="st-meta">
              {snapshot.cols}×{snapshot.rows} · {snapshot.alt ? "全屏" : "普通"} · 帧 {snapshot.frameId}
            </span>
          )}
        </div>
        <pre className="st-pre">
          {snapshot
            ? snapshot.lines.slice(-12).join("\n") || "（空屏）"
            : "该终端还没有上报过屏幕帧（未托管的终端不上报，开始托管后会立刻补一帧）"}
        </pre>
      </div>


          <div className="st-evidence">
            内部说明：开始托管后 Steward 常驻读屏、识别局面；你敲键即夺权（100ms SLA），交还时给「离开期间」
            摘要；「通知后执行」档留 30s 撤销窗口；断连 / 强杀后凭 Checkpoint 恢复现场（屏幕漂移自动降「需确认」档）。
            决策编译零 LLM，Planner 只是兜底且默认关。
          </div>
          <div className="st-evidence">
            {runtime?.running ? "Steward 运行中" : "Steward 未运行"} · tick {runtime?.tickCount ?? 0} · 分类{" "}
            {runtime?.classifyCount ?? 0} · 决策 {runtime?.decisionCount ?? 0} · 注入{" "}
            <strong className={runtime?.injectedCount ? "st-hot" : ""}>{runtime?.injectedCount ?? 0}</strong> · 待回执{" "}
            {runtime?.pendingReceipts ?? 0} · 待确认 {runtime?.escalations ?? 0} · 事件 seq {runtime?.eventSeq ?? 0} ·{" "}
            <strong className="st-zero">LLM {runtime?.llmCalls ?? 0}</strong>
          </div>
          <div className="st-evidence">
            所有权 {core?.ownership.length ?? 0} 槽 · 撤销窗口 {undoList.length} · 现场 {core?.checkpointCount ?? 0} 份 ·
            待恢复 {core?.resumeQueue.length ?? 0} · 写锁 {core?.writeLocked.length ?? 0} · 单调时钟{" "}
            {((core?.elapsedMs ?? 0) / 1000) | 0}s
          </div>
          {state && (
            <div className="st-evidence">
              {label(selected)}：心跳 {state.heartbeatMs}ms · 同局面 {state.sameTicks} 次 · 已注入 {state.injectedCount}
            </div>
          )}
          {consoleInfo && (
            <div className="st-evidence">
              {consoleInfo.hasScreen
                ? `帧 ${consoleInfo.frameId} · 指纹 ${consoleInfo.screenHash.slice(0, 8)} · 空转帧 ${consoleInfo.idleFrames}`
                : "无屏幕帧"}
              {snapshot ? ` · 快照指纹 ${snapshot.hash.slice(0, 8)}` : ""}
            </div>
          )}
          {contract && (
            <div className="st-contract">
              契约 {contract.id} · {contract.hostKey} · {tierLabel(contract.authorityTier)} · 意向{" "}
              {contract.intentIds.length} 条
            </div>
          )}
          {core && (
            <div className="st-evidence">
              时间盒：读屏回执 {core.timebox.screenAckMs / 1000}s · 写后回执 {core.timebox.receiptMs / 1000}s · 单步{" "}
              {core.timebox.stepMs / 60000}min · 空闲 {core.timebox.idleMs / 60000}min · 无进展{" "}
              {core.timebox.noProgressMs / 60000}min · 意向 deadline{" "}
              {core.timebox.deadlineMs ? `${Math.round(core.timebox.deadlineMs / 60000)}min` : "不限"}
            </div>
          )}
          <div className="st-form-row">
            <button className="st-btn" onClick={() => void setTicks(1)} title="真机验收：一次心跳没变化就判空转">
              空转判定：1 次心跳
            </button>
            <button className="st-btn" onClick={() => void setTicks(3)} title="恢复默认：连续 3 次心跳没变化才判空转">
              空转判定：默认 3 次
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
            <button className="st-btn danger" onClick={() => void clearDecisions()}>
              清空决策
            </button>
            <button className="st-btn danger" onClick={() => void clear()}>
              清空事件
            </button>
          </div>
        </div>
      </details>
    </div>
  );
}
