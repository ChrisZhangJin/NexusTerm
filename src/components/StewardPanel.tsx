/**
 * 托管控制台。**AI 托管**：写下目标 →「开始托管」→ 大模型（对话 Agent 的循环）当手和脑去执行，
 * Steward 的屏幕识别当眼睛盯屏；运行实况（步骤 / 审批 / 密码卡片）直接显示在本页。
 * 事件只订阅一份（在 AgentPanel 的 `useAgentRun` 里，本页经 `run` 桥读取与操作），
 * 所以审批 / 密码卡片不会在「对话」与「托管」两处各答一次。
 *
 * 守护进程只观察（局面 / 空转 / 断流事件），动手的唯一入口是托管任务（`host_*` 命令，
 * 胶囊与卡片见 HostCard）。旧的零 LLM 决策卡 / 升级卡 / 撤销窗口 / 接管交还 / 恢复 /
 * Planner 界面随后端写路径在 PR8 删除。
 *
 * 布局（状态优先）：等重新接驳的卡 → 选中终端的托管卡（HostCard：一句话状态 + 唯一一行控制，
 * 等你的卡片紧跟卡头，档位 / 禁止项等收进「契约 / 设置」）→ 没有进行中的托管时才出现的开始表单
 * （三个预设 + 「更多设置」）→ 托管实况（运行中默认展开）→ 终端列表 → 当前局面 / 诊断（默认收起）。
 * 设计见 docs/nexus-host-unified-design.md。
 */

import { useCallback, useEffect, useRef, useState } from "react";
import type { UnlistenFn } from "@tauri-apps/api/event";
import type {
  AgentApproveRequest,
  AgentMessage,
  AgentSecretRequest,
  AgentTarget,
  HostStartOpts,
  HostStatus,
} from "../types";
import { t, useLocale } from "../i18n";
import HostCard from "./HostCard";
import HostHistory from "./HostHistory";
import DenyEditor from "./DenyEditor";
import { DEFAULT_DENY, addDeny, hostPill, isTerminal, needsReattach } from "../lib/host";
import { HOST_PRESETS, HOST_TIERS, LS_HOST_TIER, TIER_HINTS, hostSettingsSummary, parseHostTier, presetOfTier } from "../lib/hostUi";
import { pendingPrompts } from "../lib/agentRun";
import { activeAccount, useAgentConfig } from "../lib/agent";
import { costGrant, fmtUsd, parseCostInput, priceFor } from "../lib/usage";
import {
  LS_STEWARD_BUDGET,
  STEWARD_DURATION_OPTIONS,
  MAX_STEPS_CEIL,
  parseStewardBudgetPref,
  type StewardBudgetPref,
} from "../lib/agentBudget";
import { RunFeed } from "./RunFeed";
import {
  ACTION_LABELS,
  EVENT_LABELS,
  OUTCOME_LABELS,
  REF_LABELS,
  TIER_LABELS,
  contractStatusLabel,
  eventLabel,
  programLabel,
  situationLabel,
  stewardApi,
  stewardConsoleIds,
  tierLabel,
  type AuditEntry,
  type ConsoleInfo,
  type DecisionRecord,
  type Explanation,
  type ReviewReport,
  type ScreenSnapshot,
  type Situation,
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

/** 开始托管表单上的禁止项偏好 */
const LS_HOST_DENY = "nx.host.deny";
/** 开始托管前要不要先确认完成判据（默认不要：托管的主旨是少打扰人） */
const LS_HOST_CONFIRM_CRITERIA = "nx.host.confirmCriteria";

export interface StewardRunBridge {
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
  /** 这一轮停在暂停上（预算用尽 / 手动暂停）：实况里的一行暂停说明用；控制按钮都在托管卡上 */
  paused: boolean;
  pauseText?: string;
  stopCtrlC: boolean;
  toggleStopCtrlC: (on: boolean) => void;
  /** 开始托管（`host_start`）：后端起好任务就 resolve，失败会作为错误消息出现在实况里 */
  start: (
    sessionId: string,
    goal: string,
    budget?: StewardBudgetPref,
    tier?: string,
    yolo?: boolean,
    opts?: HostStartOpts
  ) => Promise<void>;
  /** 停止这个终端上的托管（按 stopCtrlC 决定要不要发 Ctrl-C） */
  cancel: () => Promise<void>;
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
  /** 某个终端的托管运行桥（由 AgentPanel 提供）：每个终端各自一份，同一时刻可托管多个终端 */
  runFor: (consoleId: string) => StewardRunBridge;
}

/** 以前常驻页面顶部的说明段，现在只在表单头的「?」上悬停可见 */
const intro = () =>
  t(
    "AI 托管：由大模型替你在这个终端里执行命令、操作交互程序（含 Claude Code 之类的控制台工具），并用屏幕识别盯着长任务跑完。" +
      "危险命令仍会先问你；密码只会请你本人输入；不会替你选「不再询问 / 永久允许」这类选项。你一敲键它就让路。" +
      "下方「诊断」里是屏幕识别（不调大模型、不写终端）与决策记录的细节。",
    "Autopilot: an AI model runs commands and drives interactive programs (including console tools like Claude Code) in this terminal for you, and watches long tasks to completion with screen recognition. " +
      "Risky commands still ask you first; passwords are only ever typed by you; it never picks options like \"don't ask again / always allow\". It steps aside as soon as you type. " +
      "The Diagnostics section below shows screen recognition details (no model calls, no terminal writes) and the decision log."
  );

const readTierPref = (): StewardTier => {
  try {
    return parseHostTier(localStorage.getItem(LS_HOST_TIER));
  } catch {
    return parseHostTier(null);
  }
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

export default function StewardPanel({ sessionId, sessions, runFor }: StewardPanelProps) {
  useLocale();
  const [consoles, setConsoles] = useState<ConsoleInfo[]>([]);
  const [contracts, setContracts] = useState<StewardContract[]>([]);
  const [snapshot, setSnapshot] = useState<ScreenSnapshot | null>(null);
  const [events, setEvents] = useState<StewardEvent[]>([]);
  const [decisions, setDecisions] = useState<DecisionRecord[]>([]);
  const [runtime, setRuntime] = useState<StewardRuntimeInfo | null>(null);
  const [manual, setManual] = useState<Situation | null>(null);
  const [showTail, setShowTail] = useState(false);
  const [showCtx, setShowCtx] = useState(false);
  // —— 复盘 / 审计 / 解释链（只读决策记录）——
  const [review, setReview] = useState<ReviewReport | null>(null);
  const [audit, setAudit] = useState<AuditEntry[]>([]);
  const [explanation, setExplanation] = useState<Explanation | null>(null);
  const [showReview, setShowReview] = useState(false);
  const [selected, setSelected] = useState(sessionId);
  /** 选中的那个终端的托管（别的终端上的托管不影响这里能不能开始） */
  const run = runFor(selected);
  const [goal, setGoal] = useState("");
  /** 多终端托管：主终端（选中的这个）之外一并交给 AI 的终端 */
  const [extra, setExtra] = useState<string[]>([]);
  /** 开始表单上的档位（预设）；用户选的记在本地，下次沿用 */
  const [tier, setTier] = useState<StewardTier>(readTierPref);
  const chooseTier = (t: StewardTier) => {
    setTier(t);
    try {
      localStorage.setItem(LS_HOST_TIER, t);
    } catch {
      /* 存不了就只在这次生效 */
    }
  };
  /** 开始前先确认 AI 起草的完成判据（默认关 = 直接采用草稿开始，未确认的判据只跑只读检查、不阻塞完成）。记在本地 */
  const [confirmCriteria, setConfirmCriteriaState] = useState(() => localStorage.getItem(LS_HOST_CONFIRM_CRITERIA) === "1");
  const setConfirmCriteria = (on: boolean) => {
    setConfirmCriteriaState(on);
    try {
      localStorage.setItem(LS_HOST_CONFIRM_CRITERIA, on ? "1" : "0");
    } catch {
      /* 存不了就只在这次生效 */
    }
  };
  const yolo = !confirmCriteria;
  /** 开始托管时的禁止项（记在本地，下次开始沿用；底线项总在） */
  const [deny, setDenyState] = useState<string[]>(() => {
    try {
      const v = JSON.parse(localStorage.getItem(LS_HOST_DENY) ?? "null");
      if (Array.isArray(v)) return v.reduce<string[]>((acc, t) => (typeof t === "string" ? addDeny(acc, t) : acc), addDeny([], ""));
    } catch {
      /* 坏数据按默认 */
    }
    return [...DEFAULT_DENY];
  });
  const setDeny = (next: string[]) => {
    setDenyState(next);
    localStorage.setItem(LS_HOST_DENY, JSON.stringify(next));
  };
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
  /** 花费上限输入框的草稿（美元，空 = 不限） */
  const [costDraft, setCostDraft] = useState<string | null>(null);
  const commitCost = () => {
    if (costDraft === null) return;
    updateBudgetPref({ maxCostUsd: parseCostInput(costDraft) });
    setCostDraft(null);
  };
  // 花费上限要靠模型单价折算：当前模型没单价时这一项不生效，表单上说一声
  const agentCfg = useAgentConfig();
  const modelPriced = priceFor(agentCfg.prices, activeAccount(agentCfg).model) !== null;
  const selectedRef = useRef(selected);
  selectedRef.current = selected;
  /** 用户在列表里手动点过别的终端（非当前标签）时记下；为 null 时跟随当前标签 */
  const pickedRef = useRef<string | null>(null);

  const loadOnce = useCallback(async () => {
    try {
      const [list, signed, rt, evs, recs, snap] = await Promise.all([
        stewardApi.listConsoles(),
        stewardApi.status(),
        stewardApi.runtime(),
        // 全量拉取避免前端游标在「清空事件」后倒退，seq 由后端单调分配。
        stewardApi.events(0, EVENTS_FETCH),
        stewardApi.decisions(20),
        stewardApi.screenGet(selectedRef.current),
      ]);
      setConsoles(list);
      setContracts(signed);
      // 契约列表顺手喂给采集门控：新签的契约立刻补一帧全量，停掉的不再采
      setStewardConsoles(stewardConsoleIds(signed));
      setRuntime(rt);
      setEvents(evs);
      setDecisions(recs);
      setSnapshot(snap);
      setErr(null);
    } catch (e) {
      setErr(t("读取常驻 Steward 失败：{e}", "Failed to read Steward state: {e}", { e: String(e) }));
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
  /** 可以一并交给这次托管的其余终端：开着的、不是主终端、没有在别的托管里 */
  const extraOptions = (sessions ?? []).filter((s) => {
    if (s.id === selected) return false;
    const h = run.hostFor(s.id);
    // 那个终端上正跑着一轮对话 / 技能：后端会拒，干脆不列
    if (runFor(s.id).otherBusy) return false;
    return !h || isTerminal(h.phase);
  });
  /** 勾选里仍然可选的那些（标签关了 / 被别的托管占了 / 换成了主终端的自动剔掉） */
  const extraChosen = extra.filter((id) => extraOptions.some((o) => o.id === id));
  const toggleExtra = (id: string, on: boolean) =>
    setExtra((l) => (on ? (l.includes(id) ? l : [...l, id]) : l.filter((x) => x !== id)));
  /** 契约 id 人看不懂；换成它所属终端的名字 */
  const contractName = (cid: string) => {
    const consoleId = contracts.find((c) => c.id === cid)?.consoleId;
    return consoleId ? label(consoleId) : cid;
  };
  const state = runtime?.consoles.find((c) => c.consoleId === selected) ?? null;
  const consoleInfo = consoles.find((c) => c.id === selected) ?? null;

  // 目标框 / 档位：按「终端 + 契约 id」加载。换终端或契约换了才重填，用户正在改的不被轮询冲掉；
  // 同一份契约在后端换了档（接驳时屏幕漂移会自动降「需确认」档）也跟上。
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
      setTier(readTierPref());
    } else if (contract && serverTier !== prev.tier) {
      setTier(contract.authorityTier);
    }
    // contract 本身每轮轮询都是新对象，只看 id / 档位
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selected, contractId, contract?.authorityTier]);

  const doExplain = async (seq: number) => {
    try {
      setExplanation(await stewardApi.explain(seq));
    } catch (e) {
      setErr(t("解释链读取失败：{e}", "Failed to load the explanation: {e}", { e: String(e) }));
    }
  };

  const loadReview = async () => {
    try {
      setReview(await stewardApi.review(contract?.id));
      setAudit(await stewardApi.audit(contract?.id));
    } catch (e) {
      setErr(t("复盘读取失败：{e}", "Failed to load the review: {e}", { e: String(e) }));
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

  /**
   * 开始托管 = 后端 `host_start`：先拿这个终端的租约，再签契约（前端据此开始上报屏幕帧 —— 眼睛），
   * 最后起常驻的托管任务（手和脑）。顺序在后端保证：契约出现时守护进程已经看得到租约，只会观察。
   * 托管任务只有完成 / 失败 / 停止才结束；问你问题、你接管、预算用尽都只是挂起。
   */
  const startHosting = async () => {
    if (!selected || !goal.trim() || busy || run.active || run.otherBusy || !run.ready) return;
    setBusy(true);
    try {
      await run.start(selected, goal.trim(), budgetPref, tier, yolo, { deny, extra: extraChosen });
      await refresh();
    } finally {
      setBusy(false);
    }
  };

  /** 停止托管：先停这一轮 Agent（幂等），再把契约一并停掉 —— 否则这个终端会一直被当成托管中采帧 */
  const stopHosting = async () => {
    if (busy) return;
    setBusy(true);
    try {
      if (run.active) await run.cancel();
      if (contract) await stewardApi.stop(contract.id);
      setManual(null);
      await refresh();
    } catch (e) {
      setErr(t("停止托管失败：{e}", "Failed to stop autopilot: {e}", { e: String(e) }));
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
      setErr(t("停止托管失败：{e}", "Failed to stop autopilot: {e}", { e: String(e) }));
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
      setErr(t("立即分类失败：{e}", "Classify now failed: {e}", { e: String(e) }));
    }
  };

  const setTicks = async (n: number) => {
    try {
      await stewardApi.setConfig(n);
      await stewardApi.wake();
      await refresh();
    } catch (e) {
      setErr(t("调整空转阈值失败：{e}", "Failed to change the idle threshold: {e}", { e: String(e) }));
    }
  };

  const clear = async () => {
    try {
      await stewardApi.clearEvents();
      setManual(null);
      await refresh();
    } catch (e) {
      setErr(t("清空事件失败：{e}", "Failed to clear events: {e}", { e: String(e) }));
    }
  };

  const clearDecisions = async () => {
    try {
      await stewardApi.clearDecisions();
      await refresh();
    } catch (e) {
      setErr(t("清空决策失败：{e}", "Failed to clear decisions: {e}", { e: String(e) }));
    }
  };

  /** 托管那一轮发起在当前选中的终端上：实况与控制键才在这里显示 */
  // 多终端托管：实况在主终端的频道上，选中其余终端时 session 是主终端 id，同样算「这里的托管」
  const runHere = run.session !== null;
  const promptsPending = runHere ? pendingPrompts(run.messages) : 0;
  /** 选中终端上的托管任务（含刚结束的结局） */
  const selectedHost = selected ? run.hostFor(selected) : null;
  /** 选中终端上还没结束的托管：有它就不出开始表单（控制全在托管卡上） */
  const liveHost = selectedHost && !isTerminal(selectedHost.phase) ? selectedHost : null;
  /** 开始表单下的一行提醒：只说挡着你开始的事 */
  const formWarn: string | null = run.otherBusy
    ? t("对话 / 技能正在运行，结束后才能开始托管", "A chat or skill is running; autopilot can start once it finishes")
    : !run.ready
      ? (run.notReadyReason ?? t("Nexus AI Agent 尚未配置完成，请先去设置", "Nexus AI Agent isn't set up yet; finish it in Settings first"))
      : contract && state?.stale
        ? t("屏幕帧断流 — 终端可能已关闭", "Screen feed stalled — the terminal may be closed")
        : null;
  const formSummary = hostSettingsSummary(tier, deny);
  const durationLabel = STEWARD_DURATION_OPTIONS.find((o) => o.minutes === budgetPref.maxMinutes)?.label ?? t("{m} 分钟", "{m} min", { m: budgetPref.maxMinutes });

  /** 一条决策记录（托管写入 / 反射代答各一条），可展开依据与解释链 */
  const renderDecision = (d: DecisionRecord) => {
            return (
              <div key={d.seq} className={`st-dec ${d.outcome}`}>
                <div className="st-dec-head">
                  <span className="st-dec-seq">#{d.seq}</span>
                  <span className="st-kind">{ACTION_LABELS[d.chosen.kind] ?? d.chosen.kind}</span>
                  <span className="st-badge">{tierLabel(d.tier)}</span>
                  <span className="st-badge outcome">{OUTCOME_LABELS[d.outcome] ?? d.outcome}</span>
                  <span className="st-meta">
                    {t("{who} · {time} · 置信度 {conf} · LLM {llm}", "{who} · {time} · confidence {conf} · LLM {llm}", {
                      who: label(d.consoleId),
                      time: hhmmss(d.tsMs),
                      conf: d.confidence.toFixed(2),
                      llm: d.llmUsed ? 1 : 0,
                    })}
                  </span>
                  <button className="st-btn" onClick={() => void doExplain(d.seq)} title={t("展开解释链：为什么这次这么做", "Show the explanation: why it did this")}>
                    {t("为什么", "Why")}
                  </button>
                </div>
                <div className="st-dec-text">{d.chosen.text}</div>
                <div className="st-evidence">{d.rationale}</div>
                {d.receipt && <div className="st-evidence">{t("回执：{r}", "Receipt: {r}", { r: d.receipt })}</div>}
                <details className="st-dec-details">
                  <summary>
                    {t("依据 {refs} 条 · 备选 {cands} 个", "{refs} refs · {cands} alternatives", { refs: d.ctxRefs.length, cands: d.candidates.length })}
                  </summary>
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
                    {t("备选：", "Alternatives: ")}
                    {d.candidates.map((c) => t("{kind}「{text}」", "{kind} \"{text}\"", { kind: ACTION_LABELS[c.kind] ?? c.kind, text: c.text })).join(" / ") || "—"}
                  </div>
                </details>
              </div>
            );
  };

  return (
    <div className="ap-steward">
      {(err || run.error) && <div className="st-err">{err ?? run.error}</div>}

      {/* 待重新接驳：重启后 console id 都变了，不跟选中的终端走，全部列出；接驳要你确认 */}
      {run.awaitingReattach.map((h) => (
        <HostCard key={h.contractId} status={h} label={h.label} tabs={sessions} stopCtrlC={run.stopCtrlC} />
      ))}

      {/* 选中终端上的托管：一句话状态 + 控制 + 等你回答 / 批准的卡片（永远等下去，没有倒计时） */}
      {selectedHost && !needsReattach(selectedHost.phase) && (
        <HostCard
          // 按托管换实例：卡里的草稿 / 通知卡 / 「改目标」等局部状态不能跟着面板串到另一份托管上
          key={selectedHost.contractId}
          status={selectedHost}
          label={label(selectedHost.consoleId)}
          grant={{
            maxSteps: budgetPref.maxSteps,
            maxMinutes: budgetPref.maxMinutes,
            ...costGrant(budgetPref.maxCostUsd),
          }}
          stopCtrlC={run.stopCtrlC}
          onStopCtrlC={run.toggleStopCtrlC}
          onStop={stopHosting}
          onFollowUp={
            run.otherBusy || !run.ready
              ? undefined
              : (nextGoal: string, prev: HostStartOpts["followUp"]) =>
                  run.start(selectedHost.consoleId, nextGoal, budgetPref, tier, yolo, {
                    deny,
                    followUp: prev,
                    // 多终端托管的后续任务沿用同一组终端（只留还开着的）
                    extra: (selectedHost.consoles ?? [])
                      .filter((c) => !c.primary && (sessions ?? []).some((s) => s.id === c.consoleId))
                      .map((c) => c.consoleId),
                  })
          }
        />
      )}

      {/* 开始表单：只在这个终端没有进行中的托管时出现 */}
      {!liveHost && (
        <div className="st-main">
          <div className="st-main-head">
            <span className={`st-dot${runtime?.running ? " live" : ""}`} title={runtime?.running ? t("Steward 运行中", "Steward running") : t("Steward 未运行", "Steward not running")} />
            <span className="st-name">
              {selectedHost ? t("新的托管", "New autopilot") : t("托管", "Autopilot")} · {label(selected)}
            </span>
            {contract && <span className="st-badge on">{contractStatusLabel(contract.status)}</span>}
            <span className="hc-help" title={intro()}>
              ?
            </span>
          </div>
          {!selectedHost && (
            <div className="hc-hint">
              {t(
                "写下目标，AI 替你在这个终端里一步步做完；危险操作先问你，密码只由你本人输入。",
                "Describe the goal and the AI works through it step by step in this terminal. Risky actions ask you first; passwords are typed only by you."
              )}
            </div>
          )}
          <textarea
            className="st-goal"
            value={goal}
            disabled={busy}
            onChange={(e) => setGoal(e.target.value)}
            placeholder={t(
              "写下要它完成的事，例如：把 /srv/app 的依赖升级后跑完测试并汇报；或：盯着这个安装脚本跑完，遇到确认就替我答",
              "What should it get done? E.g. upgrade the dependencies in /srv/app, run the tests and report back; or: watch this install script to the end and answer its prompts for me"
            )}
            rows={3}
          />
          {extraOptions.length > 0 && (
            <details className="hc-more hc-extra">
              <summary
                title={t(
                  "目标要跨几台机器时（例如在 A 上搭代理、再从 B 上测试），把其余终端也勾上：它们一起交给 AI、一起只读锁定，你在其中任何一个上动手，整个托管都会让路",
                  "When the goal spans several machines (e.g. set up a proxy on A, then test from B), tick the other terminals too: they're handed to the AI and locked read-only together, and acting in any of them makes the whole autopilot step aside"
                )}
              >
                {t("同时操作的终端（可选）", "Other terminals to drive (optional)")}
                <span className="st-meta">
                  {extraChosen.length > 0
                    ? t(" · 主终端「{main}」+ {rest}", " · main \"{main}\" + {rest}", {
                        main: label(selected),
                        rest: extraChosen.map((id) => t("「{l}」", "\"{l}\"", { l: label(id) })).join(t("", ", ")),
                      })
                    : t(" · 只操作这一个", " · this one only")}
                </span>
              </summary>
              <div className="hc-settings-body">
                {extraOptions.map((o) => (
                  <label key={o.id} className="st-meta hc-row">
                    <input
                      type="checkbox"
                      checked={extraChosen.includes(o.id)}
                      disabled={busy}
                      onChange={(e) => toggleExtra(o.id, e.target.checked)}
                    />
                    {o.label}
                  </label>
                ))}
                <div className="hc-hint">
                  {t(
                    "AI 默认在主终端上动手，要操作其余终端时会指明是哪一个；完成判据在主终端上核验。",
                    "The AI works in the main terminal by default and names the terminal whenever it acts in another one; completion checks run on the main terminal."
                  )}
                </div>
              </div>
            </details>
          )}
          <div className="hc-presets" role="radiogroup" aria-label={t("放手程度", "Autonomy")}>
            {HOST_PRESETS.map((p) => (
              <button
                key={p.tier}
                role="radio"
                aria-checked={tier === p.tier}
                className={`hc-preset${tier === p.tier ? " on" : ""}${p.tier === "yolo" ? " yolo" : ""}`}
                disabled={busy}
                title={p.hint}
                onClick={() => chooseTier(p.tier)}
              >
                {p.label}
              </button>
            ))}
          </div>
          {tier === "yolo" ? (
            <div className="st-yolo-warn">
              {t(
                "YOLO·完全放手：AI 会自己执行一切，包括格式化、重启、删库这类不可逆操作，不再问你。只有越过「更多设置」里的禁止项时才会停下来问。生产机器慎用。",
                "YOLO: the AI does everything on its own, including irreversible actions like formatting, rebooting or dropping databases, without asking. It only stops to ask when a blocked action from More settings comes up. Be careful on production machines."
              )}
            </div>
          ) : (
            <div className="hc-hint">
              {presetOfTier(tier) ? "" : t("{tier}：", "{tier}: ", { tier: TIER_LABELS[tier] })}
              {TIER_HINTS[tier]}
            </div>
          )}
          <div className="st-form-row st-center">
            <button
              className="st-btn primary"
              disabled={busy || !goal.trim() || run.otherBusy || !run.ready || run.active}
              onClick={() => void startHosting()}
              title={
                !run.ready
                  ? (run.notReadyReason ?? t("请先完成 Nexus AI Agent 配置", "Set up Nexus AI Agent first"))
                  : run.otherBusy
                    ? t("对话 / 技能正在运行，结束后才能开始托管", "A chat or skill is running; autopilot can start once it finishes")
                    : t("签下托管契约（开始盯屏）并让 AI 去执行这个目标", "Sign the autopilot contract (start watching the screen) and let the AI work on this goal")
              }
            >
              {t("开始托管", "Start autopilot")}
            </button>
            {contract && (
              <button className="st-btn danger" disabled={busy} onClick={() => void stop()} title={t("结束这个终端上残留的托管契约（停止盯屏）", "End the leftover autopilot contract on this terminal (stop watching the screen)")}>
                {t("停止托管", "Stop autopilot")}
              </button>
            )}
          </div>
          <details className="hc-more">
            <summary>
              {t("更多设置", "More settings")}
              <span className="st-meta">
                {t(" · 最长 {dur} · 最多 {steps} 步", " · up to {dur} · up to {steps} steps", { dur: durationLabel, steps: budgetPref.maxSteps })}
                {budgetPref.maxCostUsd > 0 ? t(" · 最多花 {cost}", " · max spend {cost}", { cost: fmtUsd(budgetPref.maxCostUsd) }) : ""}
                {confirmCriteria ? t(" · 先确认判据", " · confirm checks first") : ""} · {formSummary.deny}
              </span>
            </summary>
            <div className="hc-settings-body">
              <label className="st-meta hc-row" title={t("授权档位：{hint}", "Permission level: {hint}", { hint: TIER_HINTS[tier] })}>
                {t("档位", "Level")}
                <select
                  className={`st-select${tier === "yolo" ? " st-select-yolo" : ""}`}
                  value={tier}
                  disabled={busy}
                  onChange={(e) => chooseTier(e.target.value as StewardTier)}
                >
                  {HOST_TIERS.map((tr) => (
                    <option key={tr} value={tr} title={TIER_HINTS[tr]}>
                      {TIER_LABELS[tr]}
                    </option>
                  ))}
                </select>
              </label>
              <div className="st-form-row st-center">
                <label className="st-meta" title={t(
                    "这一轮最长跑多久，到点会暂停等你点「加预算继续」（后端上限 24 小时）",
                    "How long this round may run; when time's up it pauses until you click Add budget & continue (backend limit 24 hours)"
                  )}
                >
                  {t("最长", "Up to")}
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
                <label className="st-meta" title={t("最多执行多少步动作（后端上限 {max}）", "Maximum number of action steps (backend limit {max})", { max: MAX_STEPS_CEIL })}>
                  {t("最多", "Up to")}
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
                  {t("步", "steps")}
                </label>
                <label
                  className="st-meta"
                  title={t(
                    "按模型单价折算的花费，到了就暂停等你点「加预算继续」。留空 = 不限。单价在 AI 设置的「模型单价」里改",
                    "Spend computed from model prices; when reached it pauses until you click Add budget & continue. Leave empty for no limit. Set prices under Model prices in AI settings"
                  )}
                >
                  {t("最多花费 $", "Max spend $")}
                  <input
                    className="st-num"
                    type="number"
                    min={0}
                    step={0.1}
                    placeholder={t("不限", "No limit")}
                    value={costDraft ?? (budgetPref.maxCostUsd > 0 ? String(budgetPref.maxCostUsd) : "")}
                    disabled={busy}
                    onChange={(e) => setCostDraft(e.target.value)}
                    onBlur={commitCost}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") commitCost();
                    }}
                  />
                </label>
              </div>
              {budgetPref.maxCostUsd > 0 && !modelPriced && (
                <div className="st-status warn">
                  {t(
                    "当前模型没有设置单价，花费上限不会生效（只计 token）。可在 AI 设置的「模型单价」里补上。",
                    "The current model has no price set, so the spend limit won't apply (only tokens are counted). Add one under Model prices in AI settings."
                  )}
                </div>
              )}
              <label
                className="st-meta hc-row"
                title={t(
                  "勾上：开始前先出一张「确认完成判据」卡，你改好点接受才开始。不勾（默认）：直接采用 AI 起草的判据开始；未确认的判据只跑只读检查，跑不了的记「未核验」，不挡完成。",
                  "On: before starting, show a Confirm completion checks card; it starts after you edit and accept. Off (default): start right away with the AI-drafted checks; unconfirmed checks run read-only only, and any that can't run are marked unverified without blocking completion."
                )}
              >
                <input type="checkbox" checked={confirmCriteria} disabled={busy} onChange={(e) => setConfirmCriteria(e.target.checked)} />
                {t("开始前先确认完成判据", "Confirm completion checks before starting")}
              </label>
              <DenyEditor value={deny} onChange={setDeny} disabled={busy} />
            </div>
          </details>
          {formWarn && <div className="st-status warn">{formWarn}</div>}
        </div>
      )}

      {/* 托管实况：运行中默认展开（密码卡也在这里面）；换终端时按新终端重置。
          有密码 / 批准卡等你时换个 key 重新挂载：用户先前手动收起过的话，`open` 表达式没变（运行中恒为 true），
          React 不会重新应用，密码输入框就藏在收起的实况里了 */}
      {runHere && (run.active || run.messages.length > 0) && (
        <details key={`${selected}:${promptsPending}`} className="hc-feed" open={run.active || promptsPending > 0}>
          <summary>
            {t("托管实况 · {l}", "Autopilot live · {l}", { l: label(selected) })}
            <span className={promptsPending > 0 ? "hc-warn" : "st-meta"}>
              {" "}
              ·{" "}
              {promptsPending > 0
                ? t("有 {n} 项等你处理（批准 / 输入密码）", "{n} waiting for you (approval / password)", { n: promptsPending })
                : run.active
                  ? run.paused
                    ? t("已暂停", "Paused")
                    : t("执行中", "Running")
                  : t("已结束", "Finished")}
            </span>
          </summary>
          <RunFeed
            messages={run.messages}
            busy={run.active}
            paused={run.paused}
            pauseText={run.pauseText}
            onDecide={(req, ok) => void run.decide(req, ok)}
            onAnswerSecret={run.answerSecret}
            host={selectedHost ? { contractId: selectedHost.contractId, deny: selectedHost.deny ?? [] } : null}
          />
        </details>
      )}

      <div className="st-head">
        <span>{t("终端 · {n} 个", "Terminals · {n}", { n: consoles.length })}</span>
        <div>
          <button className="st-btn" onClick={() => void refresh()}>
            {t("刷新", "Refresh")}
          </button>
        </div>
      </div>

      <div className="st-list">
        {consoles.length === 0 && <div className="st-empty">{t("还没有检测到终端", "No terminals detected yet")}</div>}
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
              {c.alt && <span className="st-badge alt">{t("全屏", "Full screen")}</span>}
              {c.status !== "none" && !pill && <span className="st-badge on">{contractStatusLabel(c.status)}</span>}
              {cs?.kind && <span className="st-badge situation">{situationLabel(cs.kind)}</span>}
            </button>
          );
        })}
      </div>

      {/* 历史托管：结束了的托管，点开回看报告与实际改动 */}
      <HostHistory />

      {/* 当前局面：屏幕识别的结论，调试用，默认收起 */}
      <details className="st-diag">
        <summary>
          {t("当前局面 · {l}", "Current situation · {l}", { l: label(selected) })}
          {situation ? ` · ${situationLabel(situation.kind)}` : ""} ·{" "}
          {state
            ? state.stale
              ? t("帧断流", "Feed stalled")
              : t("帧新鲜", "Feed fresh")
            : t("该终端尚未托管", "This terminal isn't on autopilot")}
        </summary>
        <div className="st-sit hc-sit">
        {situation ? (
          <div className="st-sit-body">
            <div className="st-sit-line">
              <span className="st-kind">{situationLabel(situation.kind)}</span>
              <span className="st-badge">{programLabel(situation.program)}</span>
              <span className="st-meta">{t("置信度 {c}", "Confidence {c}", { c: situation.confidence.toFixed(2) })}</span>
              <span
                className="st-meta"
                title={t("屏幕没变时心跳帧只推进帧号，不会产生新的局面记录", "When the screen doesn't change, heartbeat frames only bump the frame number and don't create a new situation record")}
              >
                {t("识别于帧 {f}", "Recognised at frame {f}", { f: situation.frameId })}
                {consoleInfo?.hasScreen && consoleInfo.frameId !== situation.frameId
                  ? t("（之后未变，当前帧 {f}）", " (unchanged since; current frame {f})", { f: consoleInfo.frameId })
                  : ""}
              </span>
            </div>
            {situation.topic && <div className="st-topic">{t("主题：{x}", "Topic: {x}", { x: situation.topic })}</div>}
            {situation.options.length > 0 && (
              <div className="st-opts">
                {situation.options.map((o, i) => (
                  <span key={`${o.index ?? i}-${i}`} className={`st-opt${o.grantsPermanent ? " perm" : ""}`}>
                    {o.index ? `${o.index}. ` : ""}
                    {o.text}
                    {o.grantsPermanent ? t(" ⚠ 永久授权，不自动选", " ⚠ Permanent grant, never auto-picked") : ""}
                  </span>
                ))}
              </div>
            )}
            <div className="st-evidence">{t("判据：{x}", "Evidence: {x}", { x: situation.evidence.join(" · ") })}</div>
            <button className="st-btn" onClick={() => setShowTail((v) => !v)}>
              {showTail ? t("收起脱敏屏幕", "Hide redacted screen") : t("查看脱敏屏幕 / 签名", "Show redacted screen / signature")}
            </button>
            {showTail && (
              <>
                <div className="st-evidence">{t("签名：{x}", "Signature: {x}", { x: situation.signature })}</div>
                <pre className="st-pre small">{situation.tail || t("（空屏）", "(empty screen)")}</pre>
              </>
            )}
          </div>
        ) : (
          <div className="st-empty">
            {t(
              "还没有局面记录。开始托管后，终端屏幕一上报这里就会出现第一张局面卡片。",
              "No situation records yet. Once autopilot starts, the first situation card appears here as soon as the terminal reports its screen."
            )}
          </div>
        )}
        </div>
      </details>

      <details className="st-diag">
        <summary>{t("诊断（屏幕识别 / 决策记录 / 事件 / 屏幕快照 / 复盘）", "Diagnostics (screen recognition / decisions / events / screen snapshot / review)")}</summary>
        <div className="st-diag-body">
          <div className="st-form-row st-center">
          <button className="st-btn" onClick={() => void classifyNow()} title={t("当场分类一帧，核对判据", "Classify a frame right now to check the evidence")}>
            {t("立即分类", "Classify now")}
          </button>
          <button className="st-btn" onClick={() => setShowCtx((v) => !v)}>
            {showCtx ? t("收起依据", "Hide decision basis") : t("编辑依据", "Edit decision basis")}
          </button>
          </div>

      <div className="st-dec-wrap">
        <div className="st-sit-head">
          <span>{t("决策记录 · 最近 {n} 条", "Decisions · last {n}", { n: decisions.length })}</span>
          <span className="st-meta">{t("可展开看依据引用与备选动作", "Expand to see referenced basis and alternative actions")}</span>
        </div>
        <div className="st-dec-list">
          {decisions.length === 0 && (
            <div className="st-empty">
              {t(
                "还没有决策。托管任务每次写入（含反射代答）都会在这里留一条。",
                "No decisions yet. Every write by autopilot (including reflex answers) leaves an entry here."
              )}
            </div>
          )}
          {[...decisions].reverse().map((d) => renderDecision(d))}
        </div>
      </div>

      <div className="st-runtime">
        <button className="st-btn" onClick={() => setShowReview((v) => !v)}>
          {showReview ? t("收起复盘", "Hide review") : t("复盘（审计 / 解释链）", "Review (audit / explanations)")}
        </button>
      </div>

      {explanation && (
        <div className="st-esc">
          <div className="st-esc-head">
            {t("解释链 · 决策 #{seq}", "Explanation · decision #{seq}", { seq: explanation.seq })} ·{" "}
            {OUTCOME_LABELS[explanation.outcome] ?? explanation.outcome} ·{" "}
            {explanation.llmUsed ? t("用过 LLM", "used LLM") : t("零 LLM", "no LLM")}
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
              {t("复盘 · ", "Review · ")}
              {review ? contractName(review.contractId) : contract ? label(contract.consoleId) : t("全部契约", "All contracts")}
            </span>
            <div>
              <button className="st-btn" onClick={() => void loadReview()} title={t("跨会话复盘 + 统一审计时间线", "Cross-session review + unified audit timeline")}>
                {t("生成复盘", "Generate review")}
              </button>
            </div>
          </div>
          {review && (
            <div className="st-card">
              <div className="st-esc-head">
                {t(
                  "决策 {total} 条 · 生效 {applied} · 待确认 {escalated} · 你否掉 {rejected} · 无回执 {noReceipt} · 注入 {injected}",
                  "{total} decisions · {applied} applied · {escalated} awaiting confirmation · {rejected} rejected by you · {noReceipt} without receipt · {injected} injected",
                  {
                    total: review.total,
                    applied: review.applied,
                    escalated: review.escalated,
                    rejected: review.rejected,
                    noReceipt: review.noReceipt,
                    injected: review.injected,
                  }
                )}
              </div>
              <div className="st-evidence">{t("高频依据：{x}", "Most used basis: {x}", { x: review.topRefs.join(" / ") || "—" })}</div>
              <div className="st-esc-why">{t("已完成：{x}", "Done: {x}", { x: review.done.join(t("；", "; ")) || "—" })}</div>
              <div className="st-esc-why">{t("未闭环：{x}", "Open: {x}", { x: review.pending.join(t("；", "; ")) || "—" })}</div>
              <div className="st-evidence">{t("风险动作：{x}", "Risky actions: {x}", { x: review.risks.join(" / ") || t("无", "none") })}</div>
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
        </div>
      )}

      {showCtx && (
        <StewardContext contractId={contract?.id ?? null} onChanged={() => void refresh()} />
      )}

      <div className="st-ev">
        <div className="st-sit-head">
          <span>{t("观察事件 · 最近 {n} 条", "Observed events · last {n}", { n: Math.min(events.length, EVENTS_SHOWN) })}</span>
        </div>
        <div className="st-ev-list">
          {events.length === 0 && <div className="st-empty">{t("还没有事件", "No events yet")}</div>}
          {events
            .slice(-EVENTS_SHOWN)
            .reverse()
            .map((e) => (
              <div key={e.seq} className={`st-ev-row ${e.kind}`}>
                <span className="st-ev-time">{hhmmss(e.tsMs)}</span>
                <span className="st-ev-tag">{eventLabel(e.kind)}</span>
                <div className="st-ev-body">
                  <div className="st-ev-msg">{e.message}</div>
                  {e.suggestion && <div className="st-card">{t("建议：{x}", "Suggestion: {x}", { x: e.suggestion })}</div>}
                </div>
              </div>
            ))}
        </div>
      </div>

      <div className="st-screen">
        <div className="st-screen-head">
          {t("后端屏幕快照", "Backend screen snapshot")}
          {snapshot && (
            <span className="st-meta">
              {snapshot.cols}×{snapshot.rows} · {snapshot.alt ? t("全屏", "full screen") : t("普通", "normal")} ·{" "}
              {t("帧 {f}", "frame {f}", { f: snapshot.frameId })}
            </span>
          )}
        </div>
        <pre className="st-pre">
          {snapshot
            ? snapshot.lines.slice(-12).join("\n") || t("（空屏）", "(empty screen)")
            : t(
                "该终端还没有上报过屏幕帧（未托管的终端不上报，开始托管后会立刻补一帧）",
                "This terminal hasn't reported any screen frames yet (terminals not on autopilot don't report; one is sent right after autopilot starts)"
              )}
        </pre>
      </div>


          <div className="st-evidence">
            {t(
              "内部说明：签约后屏幕识别常驻读屏、识别局面、判空转与断流，只出建议、不写终端。动手只有托管任务一个入口；你一敲键它就让路，交还后回到原来的状态。",
              "Internals: once a contract is signed, screen recognition keeps reading the screen, recognising the situation and detecting no-progress and stalled feeds. It only makes suggestions and never writes to the terminal. Only the autopilot task acts; it steps aside as soon as you type and returns to its previous state when you hand back."
            )}
          </div>
          <div className="st-evidence">
            {runtime?.running ? t("屏幕识别运行中", "Screen recognition running") : t("屏幕识别未运行", "Screen recognition not running")} ·{" "}
            {t("tick {tick} · 分类 {cls} · 事件 seq {seq}", "tick {tick} · classified {cls} · event seq {seq}", {
              tick: runtime?.tickCount ?? 0,
              cls: runtime?.classifyCount ?? 0,
              seq: runtime?.eventSeq ?? 0,
            })}
          </div>
          {state && (
            <div className="st-evidence">
              {t("{l}：心跳 {hb}ms · 同局面 {n} 次", "{l}: heartbeat {hb}ms · same situation ×{n}", { l: label(selected), hb: state.heartbeatMs, n: state.sameTicks })}
            </div>
          )}
          {consoleInfo && (
            <div className="st-evidence">
              {consoleInfo.hasScreen
                ? t("帧 {f} · 指纹 {h} · 空转帧 {idle}", "frame {f} · hash {h} · idle frames {idle}", {
                    f: consoleInfo.frameId,
                    h: consoleInfo.screenHash.slice(0, 8),
                    idle: consoleInfo.idleFrames,
                  })
                : t("无屏幕帧", "No screen frames")}
              {snapshot ? t(" · 快照指纹 {h}", " · snapshot hash {h}", { h: snapshot.hash.slice(0, 8) }) : ""}
            </div>
          )}
          {contract && (
            <div className="st-contract">
              {t("契约 {id} · {host} · {tier} · 意向 {n} 条", "Contract {id} · {host} · {tier} · {n} intents", {
                id: contract.id,
                host: contract.hostKey,
                tier: tierLabel(contract.authorityTier),
                n: contract.intentIds.length,
              })}
            </div>
          )}
          <div className="st-form-row">
            <button className="st-btn" onClick={() => void setTicks(1)} title={t("真机验收：一次心跳没变化就判空转", "For on-device testing: treat one unchanged heartbeat as no progress")}>
              {t("空转判定：1 次心跳", "No-progress after: 1 heartbeat")}
            </button>
            <button className="st-btn" onClick={() => void setTicks(3)} title={t("恢复默认：连续 3 次心跳没变化才判空转", "Restore default: no progress after 3 unchanged heartbeats in a row")}>
              {t("空转判定：默认 3 次", "No-progress after: 3 (default)")}
            </button>
            <button
              className="st-btn"
              onClick={() => {
                captureNow(selected);
                void refresh();
              }}
              title={t("让前端立刻补一帧全量，验证前后端是否对齐", "Have the frontend send a full frame now to verify frontend and backend are in sync")}
            >
              {t("强制全量帧", "Force full frame")}
            </button>
            <button className="st-btn danger" onClick={() => void clearDecisions()}>
              {t("清空决策", "Clear decisions")}
            </button>
            <button className="st-btn danger" onClick={() => void clear()}>
              {t("清空事件", "Clear events")}
            </button>
          </div>
        </div>
      </details>
    </div>
  );
}
