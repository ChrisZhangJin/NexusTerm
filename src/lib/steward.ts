/**
 * NexusAgent v2 / 会话织物与常驻 Steward 的前端命令层。
 *
 * 设计见 `docs/nexus-agent-steward-design.md`。P0 做会话织物（枚举 console、
 * 屏幕帧持续同步、登记契约）；P1 起后端多了一个常驻 Steward：自己按心跳读屏、
 * 分类局面、判空转，把结果写成事件流推到前端。
 *
 * P2 起补上「局面 → 决策 → 注入 → 回执」闭环：`profile` / `intent` / `knowledge` /
 * `playbook` 是用户预先承载的三类依据，`DecisionRecord` 记录每条决策引用了哪几条依据、
 * 考虑过哪些备选，可回放、可纠正。**决策编译仍是零 LLM**，由确定性规则完成。
 *
 * 自动应答有一道显式总闸：每次冷启动默认关闭，打开后密码 / 永久扩权 / 断流 /
 * 低置信度 / approve 档仍然 fail-closed。升级卡带建议、备选与 30s 倒计时。
 *
 * 屏幕增量的采集在 `termRegistry.ts`（它持有 xterm 实例且保持纯函数、不依赖 Tauri），
 * 这里只负责把帧通过 IPC 送进后端 `agent::steward::ScreenStore`。
 */

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { ScreenDelta, ScreenDeltaAck } from "./termRegistry";

export type StewardTier = "auto" | "notify" | "approve";

export interface ConsoleInfo {
  id: string;
  /** local | ssh | stale（PTY 已关但屏幕现场还在） */
  kind: string;
  alive: boolean;
  hasScreen: boolean;
  frameId: number;
  screenHash: string;
  alt: boolean;
  idleFrames: number;
  updatedAt: number;
  contractId: string | null;
  /** observing（P0 只观察） | none */
  status: string;
}

export interface ScreenSnapshot {
  consoleId: string;
  frameId: number;
  windowStart: number;
  hash: string;
  alt: boolean;
  cols: number;
  rows: number;
  cursorX: number;
  cursorY: number;
  lines: string[];
  updatedAt: number;
  idleFrames: number;
}

export interface StewardContract {
  id: string;
  consoleId: string;
  hostKey: string;
  goal: string;
  authorityTier: StewardTier;
  /** P0 恒为 observing */
  status: string;
  intentIds: string[];
  timeboxMs: number;
  createdAt: number;
}

// ————————————————————————— P1 常驻循环 —————————————————————————

export type Program =
  | "shell"
  | "claude_code"
  | "vim"
  | "pager"
  | "tui"
  | "script"
  | "ssh"
  | "unknown";

export type SituationKind =
  | "question"
  | "permission"
  | "password"
  | "menu"
  | "pager"
  | "progress"
  | "subagent_status"
  | "awaiting_input"
  | "idle"
  | "error"
  | "alt_screen"
  | "unknown";

export interface Choice {
  index: string | null;
  text: string;
  /** true = 永久扩大授权，P3 的 PolicyEngine 绝不自动选 */
  grantsPermanent: boolean;
}

/** 后端 `agent::situation::Situation`：一帧屏幕的结构化局面 */
export interface Situation {
  frameId: number;
  screenHash: string;
  program: Program;
  kind: SituationKind;
  topic: string | null;
  options: Choice[];
  confidence: number;
  /** 已脱敏的末尾屏幕文本 */
  tail: string;
  /** 命中的分类规则，用来解释「为什么这么判」 */
  evidence: string[];
  staleMs: number;
  /** 语义签名，不含帧哈希 */
  signature: string;
}

export type StewardEventKind =
  | "situation_changed"
  | "no_progress"
  | "stale_feed"
  | "feed_restored"
  | "screen_lost"
  | "contract_started"
  | "contract_stopped"
  | "decision_made"
  | "action_applied"
  | "escalation"
  | "receipt_timeout"
  | "escalation_timeout"
  | "decision_rejected"
  | "takeover"
  | "handback"
  | "undo_window"
  | "policy_denied"
  | "default_action_applied"
  | "checkpoint_saved"
  | "resumed"
  | "timebox_hit"
  | "budget_exhausted"
  | "suspended"
  | "distilled"
  | "planner_used";

export interface StewardEvent {
  seq: number;
  tsMs: number;
  consoleId: string;
  contractId: string;
  kind: StewardEventKind;
  message: string;
  /** P1 的建议卡：只建议、不执行 */
  suggestion: string | null;
  situation: Situation | null;
  frameId: number;
  screenHash: string;
  /** P2 仍恒为 0 */
  llmCalls: number;
  /** 局面变化时编出的决策（含依据引用 / 备选 / 升级卡） */
  decision: DecisionRecord | null;
}

// ————————————————————————— P2 决策依据 —————————————————————————

export type CtxKind = "preference" | "intent" | "knowledge" | "playbook";

export interface CtxItem {
  id: string;
  kind: CtxKind;
  text: string;
  /** hard | soft */
  strength: string;
  /** global | claude_code | ssh | shell | <hostKey> */
  scope: string;
  /** user | learned | seed */
  source: string;
  evidence: string;
  /** 各 kind 特有字段：IntentSpec / PlaybookSpec / { tags: string[] } */
  extra: Record<string, unknown>;
  contractId: string;
  position: number;
  createdAt: number;
  updatedAt: number;
}

export interface CtxInput {
  id?: string | null;
  kind: CtxKind;
  text: string;
  strength?: string | null;
  scope?: string | null;
  source?: string | null;
  evidence?: string | null;
  extra?: Record<string, unknown> | null;
  contractId?: string | null;
  position?: number | null;
}

export interface BrainView {
  items: CtxItem[];
  executionEnabled: boolean;
  decisionCount: number;
}

export interface DecisionAction {
  /** answer | secret | wait | observe | advance | handoff */
  kind: string;
  text: string;
  optionIndex: string | null;
  injectable: boolean;
}

export interface CtxRef {
  /** profile | intent | knowledge | playbook | authority | policy | history */
  source: string;
  id: string;
  snippet: string;
}

export interface EscalationCard {
  recommendation: DecisionAction;
  why: CtxRef[];
  alternatives: DecisionAction[];
  defaultAction: DecisionAction;
  timeoutMs: number;
  oneClick: boolean;
  createdAt: number;
}

export interface DecisionRecord {
  seq: number;
  tsMs: number;
  contractId: string;
  consoleId: string;
  situationSignature: string;
  situationHash: string;
  ctxRefs: CtxRef[];
  candidates: DecisionAction[];
  chosen: DecisionAction;
  rationale: string;
  confidence: number;
  /** auto | notify | approve */
  tier: string;
  llmUsed: boolean;
  /** proposed | applied | escalated | observed | rejected | no_receipt | rolled_back */
  outcome: string;
  escalation: EscalationCard | null;
  receipt: string | null;
}

export interface StewardPolicy {
  llmUsed: boolean;
  executionDefault: boolean;
  receiptWindowMs: number;
  escalationTimeoutMs: number;
  minAutoConfidence: number;
  maxAutoStaleMs: number;
  deny: string[];
}

// ————————————————————————— P3 所有权 / 授权 / 撤销 —————————————————————————

/** agent_driving | observing_only | suspended */
export type OwnershipState = "agent_driving" | "observing_only" | "suspended";

export interface OwnershipView {
  consoleId: string;
  state: OwnershipState;
  sinceMs: number;
  takeoverLatencyMs: number;
  takeoverCount: number;
  suspensionReason: string | null;
  absenceCount: number;
}

export interface AbsenceNote {
  tsMs: number;
  kind: string;
  text: string;
}

/** 交还时的摘要卡：离开期间做了什么、还差什么、下一步建议 */
export interface AbsenceSummary {
  consoleId: string;
  sinceMs: number;
  untilMs: number;
  durationMs: number;
  takeoverCount: number;
  done: string[];
  pending: string[];
  nextStep: string;
  screenAligned: boolean;
}

/** 真授权契约（P3 起不再是静态数组），可热更新、不重启循环 */
export interface Authority {
  tier: StewardTier;
  deny: string[];
  allow: string[];
  allowPermanentGrant: boolean;
  undoWindowMs: number;
  hostScope: string[];
  maxSteps: number;
  maxMinutes: number;
  maxOutputChars: number;
  timeboxMs: number;
}

export interface UndoEntry {
  seq: number;
  consoleId: string;
  action: DecisionAction;
  expiresAt: number;
  used: boolean;
}

export interface PolicyView {
  authority: Authority;
  verdicts: string[];
  risks: string[];
  hardRules: string[];
}

// ————————————————————————— P4 时间盒 / Checkpoint —————————————————————————

export interface TimeboxConfig {
  screenAckMs: number;
  receiptMs: number;
  stepMs: number;
  idleMs: number;
  noProgressMs: number;
  deadlineMs: number;
  graceMs: number;
}

export interface Spent {
  steps: number;
  elapsedSecs: number;
  outputChars: number;
  injections: number;
}

export interface TimeboxStateJson {
  elapsedMs: number;
  sinceProgressMs: number;
  sinceStepMs: number;
  sinceFrameMs: number;
}

/** 一次托管的可恢复现场（steward_checkpoints 表整条 JSON） */
export interface StewardCheckpoint {
  contractId: string;
  consoleId: string;
  goal: string;
  authority: Authority;
  intentIds: string[];
  screenHash: string;
  frameId: number;
  lastDecisions: number[];
  pendingEscalation: [number, number] | null;
  hadPendingReceipt: boolean;
  spent: Spent;
  timebox: TimeboxStateJson;
  savedAt: number;
  savedWallMs: number;
  state: string;
  lastSignature: string;
  note: string;
}

// ————————————————————————— P5 解释链 / 复盘 / Planner —————————————————————————

export interface ExplainedRef {
  source: string;
  id: string;
  snippet: string;
  label: string;
  resolvedText: string | null;
}

export interface Explanation {
  seq: number;
  tier: string;
  outcome: string;
  confidence: number;
  rationale: string;
  refs: ExplainedRef[];
  whyAuto: string;
  llmUsed: boolean;
}

export interface ReviewReport {
  contractId: string;
  total: number;
  applied: number;
  escalated: number;
  rejected: number;
  noReceipt: number;
  injected: number;
  topRefs: string[];
  done: string[];
  pending: string[];
  risks: string[];
}

export interface AuditEntry {
  tsMs: number;
  kind: string;
  text: string;
  seq: string;
  decisionSeq: number | null;
}

export interface PlaybookCandidate {
  id: string;
  signature: string;
  occurrences: number;
  answer: string;
  optionIndex: string | null;
  matchProgram: string | null;
  matchKind: string | null;
  matchTopic: string | null;
  input: CtxInput;
}

export interface PlannerView {
  enabled: boolean;
  system: string;
  note: string;
}

/** P3–P5 运行时一次性总览：所有权 / 授权 / 撤销 / 时间盒 / 预算 / 恢复队列 */
export interface CoreView {
  elapsedMs: number;
  ownership: OwnershipView[];
  authorities: Array<[string, Authority]>;
  liveUndos: UndoEntry[];
  plannerEnabled: boolean;
  llmInflight: number;
  writeLocked: string[];
  checkpointCount: number;
  resumeQueue: string[];
  injections: number;
  timebox: TimeboxConfig;
}

export interface StewardConfig {
  noProgressTicks: number;
  staleMs: number;
  minClassifyGapMs: number;
  heartbeatCapMs: number;
}

export interface StewardStateInfo {
  consoleId: string;
  contractId: string;
  signature: string;
  kind: SituationKind | null;
  program: Program | null;
  confidence: number;
  sameTicks: number;
  heartbeatMs: number;
  lastClassifyMs: number;
  lastChangeMs: number;
  stale: boolean;
  noProgressRaised: boolean;
  frameHash: string;
  lastDecisionSignature: string;
  pendingReceipt: boolean;
  escalationSeq: number;
  injectedCount: number;
}

export interface StewardRuntimeInfo {
  running: boolean;
  tickCount: number;
  classifyCount: number;
  lastTickMs: number;
  lastClassifyMs: number;
  /** P2 仍恒为 0 */
  llmCalls: number;
  eventSeq: number;
  eventCount: number;
  config: StewardConfig;
  consoles: StewardStateInfo[];
  executionEnabled: boolean;
  decisionCount: number;
  injectedCount: number;
  pendingReceipts: number;
  escalations: number;
}

const SITUATION_LABELS: Record<SituationKind, string> = {
  question: "等待回答",
  permission: "授权确认",
  password: "密码输入",
  menu: "编号菜单",
  pager: "分页器",
  progress: "任务进行中",
  subagent_status: "子任务状态",
  awaiting_input: "等待输入",
  idle: "shell 空闲",
  error: "报错停住",
  alt_screen: "全屏程序（未细分）",
  unknown: "未知",
};

export const EVENT_LABELS: Record<StewardEventKind, string> = {  situation_changed: "局面变化",
  no_progress: "空转",
  stale_feed: "帧断流",
  feed_restored: "帧恢复",
  screen_lost: "无屏幕帧",
  contract_started: "开始托管",
  contract_stopped: "停止托管",
  decision_made: "决策",
  action_applied: "已执行",
  escalation: "待确认",
  receipt_timeout: "回执超时",
  escalation_timeout: "确认超时",
  decision_rejected: "用户接管",
  takeover: "用户接管",
  handback: "交还",
  undo_window: "撤销窗口",
  policy_denied: "策略硬禁",
  default_action_applied: "超时默认收敛",
  checkpoint_saved: "现场落盘",
  resumed: "恢复托管",
  timebox_hit: "时间盒触发",
  budget_exhausted: "预算耗尽",
  suspended: "暂停保留现场",
  distilled: "沉淀剧本",
  planner_used: "Planner 兜底",
};

const PROGRAM_LABELS: Record<Program, string> = {
  shell: "Shell",
  claude_code: "Claude Code",
  vim: "Vim",
  pager: "分页器",
  tui: "全屏 TUI",
  script: "交互脚本",
  ssh: "SSH",
  unknown: "未知程序",
};

export const situationLabel = (k: SituationKind): string => SITUATION_LABELS[k];
export const eventLabel = (k: StewardEventKind): string => EVENT_LABELS[k];
export const programLabel = (p: Program): string => PROGRAM_LABELS[p];

/** P2 决策卡的人话标签。放这里而不是组件里，方便单测与以后复用。 */
export const ACTION_LABELS: Record<string, string> = {
  answer: "替你回答",
  secret: "密码（本人输入）",
  wait: "继续等待",
  observe: "只观察",
  advance: "推进步骤",
  handoff: "交还",
};

export const REF_LABELS: Record<string, string> = {
  profile: "偏好",
  intent: "意向",
  knowledge: "知识",
  playbook: "剧本",
  authority: "授权",
  policy: "安全策略",
  history: "历史决策",
};

export const OUTCOME_LABELS: Record<string, string> = {
  proposed: "已提议",
  applied: "已生效",
  escalated: "待确认",
  observed: "只观察",
  rejected: "用户接管",
  no_receipt: "回执超时",
  rolled_back: "已纠正",
};

/** 档位的人话标签（值不变，只换显示）。面板、依据编辑、决策卡共用。 */
export const TIER_LABELS: Record<StewardTier, string> = {
  auto: "自动执行",
  notify: "通知后执行",
  approve: "需确认后执行",
};

export const tierLabel = (t: string): string => TIER_LABELS[t as StewardTier] ?? t;

/** 契约 / 现场状态的人话标签；未知值原样显示 */
const CONTRACT_STATUS_LABELS: Record<string, string> = {
  observing: "观察中",
  none: "未托管",
  agent_driving: "托管中",
  observing_only: "你接管中",
  suspended: "已暂停",
};

export const contractStatusLabel = (s: string): string => CONTRACT_STATUS_LABELS[s] ?? s;

/** 已收敛的决策不再显示升级卡 / 倒计时。 */
export const decisionSettled = (d: DecisionRecord): boolean =>
  ["applied", "rejected", "rolled_back", "no_receipt"].includes(d.outcome);

/**
 * 这条决策还在等你拍板吗？后端把每条待确认的决策都记成 outcome=escalated 并挂一张
 * 升级卡；超时后循环会按默认动作收敛并改写 outcome，前端倒计时到点也先收起。
 */
export const decisionAwaiting = (d: DecisionRecord, now: number): boolean =>
  !!d.escalation &&
  d.outcome === "escalated" &&
  now < d.escalation.createdAt + d.escalation.timeoutMs;

/** 当前有托管契约的 console —— 只有它们需要屏幕帧 / 心跳 / 敲键夺权上报 */
export const stewardConsoleIds = (contracts: StewardContract[]): string[] => [
  ...new Set(contracts.map((c) => c.consoleId)),
];

// ————————————————————————— 授权草稿（按契约 id 绑定） —————————————————————————

/**
 * 授权表单的编辑草稿。必须记住「从哪份契约加载的」：轮询每 1.5s 都会给一个新的
 * Authority 对象，切到别的 console 时契约也会换，草稿只能写回它来源的那份契约。
 */
export interface AuthDraft {
  contractId: string;
  authority: Authority;
  /** 用户改过还没保存；改过的草稿不被轮询覆盖 */
  dirty: boolean;
}

const sameAuthority = (a: Authority, b: Authority): boolean =>
  JSON.stringify(a) === JSON.stringify(b);

/**
 * 轮询结果到来时怎么处理草稿：
 * - 契约 id 变了（换 console / 重签）→ 丢掉旧草稿，按新契约重新加载；
 * - 同一份契约、用户改过 → 原样保留，绝不被轮询冲掉；
 * - 同一份契约、没改过 → 跟随后端（比如交还时降档），值没变就返回原对象省一次渲染。
 */
export function syncAuthDraft(
  prev: AuthDraft | null,
  contractId: string | null,
  server: Authority | null
): AuthDraft | null {
  if (!contractId) return null;
  if (prev && prev.contractId === contractId) {
    if (prev.dirty || !server) return prev;
    return sameAuthority(prev.authority, server) ? prev : { contractId, authority: server, dirty: false };
  }
  return server ? { contractId, authority: server, dirty: false } : null;
}

/**
 * 「保存授权」要发出去的内容。草稿不属于目标契约时返回 null（宁可不存，也不能把
 * A 的预算 / host 作用域写到 B 上）。只取表单里能改的字段，其余（deny / allow /
 * hostScope / 档位）一律以目标契约当前的为准 —— 档位只由顶部那一个选择框改。
 */
export function authorityToSave(
  draft: AuthDraft | null,
  contractId: string | null,
  server: Authority | null
): Authority | null {
  if (!draft || !contractId || !server || draft.contractId !== contractId) return null;
  const a = draft.authority;
  return {
    ...server,
    allowPermanentGrant: a.allowPermanentGrant,
    maxSteps: a.maxSteps,
    maxMinutes: a.maxMinutes,
    timeboxMs: a.timeboxMs,
  };
}

// ————————————————————————— 顶部状态行 —————————————————————————

export interface StatusLineInput {
  hasContract: boolean;
  ownership: OwnershipState | null;
  suspensionReason?: string | null;
  executionEnabled: boolean;
  /** 这个 console 上还在倒计时的升级卡数 */
  awaiting: number;
  situation: SituationKind | null;
  /** 帧断流（终端关了 / 前端不在） */
  stale?: boolean;
}

export type StatusTone = "idle" | "warn" | "active";

/** 屏幕上「有程序在问你」的局面：只有这些才可能被应答 */
const ANSWERABLE: SituationKind[] = ["question", "permission", "menu", "pager", "error", "awaiting_input"];

/**
 * 托管页顶部那一行「它现在为什么没动 / 在干什么」。只从已有状态推导，按优先级取第一条。
 */
export function stewardStatusLine(s: StatusLineInput): { text: string; tone: StatusTone } {
  if (!s.hasContract) {
    return { text: "未开始：选择一个终端，写下目标，点「开始托管」", tone: "idle" };
  }
  if (s.ownership === "observing_only") {
    return { text: "你接管中 — 点「交还托管」让它继续", tone: "warn" };
  }
  if (s.ownership === "suspended") {
    return { text: `已暂停（${s.suspensionReason || "预算用完 / 恢复后需确认"}）`, tone: "warn" };
  }
  if (s.awaiting > 0) {
    return { text: `有 ${s.awaiting} 条待你确认的建议 ↓`, tone: "warn" };
  }
  if (s.stale) {
    return { text: "屏幕帧断流 — 终端可能已关闭，暂不应答", tone: "warn" };
  }
  if (s.situation === "password") {
    return { text: "屏幕在要密码 — 只能由你本人输入", tone: "warn" };
  }
  if (!s.executionEnabled) {
    return { text: "自动应答已关 — 只会给建议，不会替你输入", tone: "idle" };
  }
  if (s.situation && ANSWERABLE.includes(s.situation)) {
    return { text: `正在应答：${SITUATION_LABELS[s.situation]}`, tone: "active" };
  }
  const now = s.situation ? SITUATION_LABELS[s.situation] : "未识别";
  return {
    text: `等待屏幕出现提问 / 权限确认（当前：${now} → 不会主动执行命令）`,
    tone: "idle",
  };
}

/** 表单里逗号 / 分号分隔的列表，供 intent.constraints 与 knowledge.tags 使用。 */
export const splitList = (s: string): string[] =>
  s
    .split(/[,，;；\n]/)
    .map((x) => x.trim())
    .filter(Boolean);

export const stewardApi = {
  /** 当前所有终端 + 屏幕帧状态 + 契约状态，一次拿全 */
  listConsoles: () => invoke<ConsoleInfo[]>("steward_list_consoles"),

  /**
   * 上报一帧。`needFull=true` 表示后端与前端不同步（窗口滑动 / 哈希漂移 / 缺基线），
   * 调用方必须让该 console 的下一帧发全量，不能继续发增量。
   */
  screenDelta: (delta: ScreenDelta) =>
    invoke<ScreenDeltaAck>("steward_screen_delta", { delta }),

  /** 读后端保存的结构化屏幕快照（面板验证 / P1 局面分类用） */
  screenGet: (consoleId: string) =>
    invoke<ScreenSnapshot | null>("steward_screen_get", { consoleId }),

  /** 签一份托管契约。同一 console 只允许一份，新的会顶掉旧的 */
  start: (consoleId: string, goal: string, authorityTier: StewardTier) =>
    invoke<StewardContract>("steward_start", { consoleId, goal, authorityTier }),

  stop: (contractId: string) => invoke<boolean>("steward_stop", { contractId }),

  status: () => invoke<StewardContract[]>("steward_status"),

  amend: (
    contractId: string,
    goal?: string,
    authorityTier?: StewardTier
  ) =>
    invoke<StewardContract | null>("steward_amend", {
      contractId,
      goal: goal ?? null,
      authorityTier: authorityTier ?? null,
    }),

  /** 增量拉观察事件；sinceSeq 是前端已见到的最大 seq */
  events: (sinceSeq = 0, limit = 60) =>
    invoke<StewardEvent[]>("steward_events", { sinceSeq, limit }),

  /** 常驻循环的运行统计：tick / 分类次数 / 心跳 / LLM 次数（P1 恒 0） */
  runtime: () => invoke<StewardRuntimeInfo>("steward_runtime_info"),

  /** 当场分类一帧，不写事件流（人工核对判据用） */
  classifyNow: (consoleId: string) =>
    invoke<Situation | null>("steward_classify_now", { consoleId }),

  clearEvents: () => invoke<void>("steward_clear_events"),

  config: () => invoke<StewardConfig>("steward_config"),

  /** 真机验收可调短空转阈值；后端会 clamp 到安全范围 */
  setConfig: (noProgressTicks?: number, staleMs?: number) =>
    invoke<StewardConfig>("steward_set_config", {
      noProgressTicks: noProgressTicks ?? null,
      staleMs: staleMs ?? null,
    }),

  /** 唤醒常驻循环立刻跑一轮（不跳过局面去重规则） */
  wake: () => invoke<boolean>("steward_wake"),

  // ——— P2 决策依据 / 决策记录 / 自动应答 ———

  /** 一次拿全偏好 / 意向 / 知识 / 剧本 + 执行开关 */
  brain: () => invoke<BrainView>("steward_brain"),

  ctxUpsert: (input: CtxInput) => invoke<CtxItem>("steward_ctx_upsert", { input }),

  ctxDelete: (id: string) => invoke<boolean>("steward_ctx_delete", { id }),

  intentPush: (
    contractId: string,
    text: string,
    constraints?: string[],
    answerTemplate?: string
  ) =>
    invoke<CtxItem>("steward_intent_push", {
      contractId,
      text,
      constraints: constraints ?? null,
      answerTemplate: answerTemplate ?? null,
    }),

  /** 回放最近决策；outcome 是回执校验后的最新状态 */
  decisions: (limit = 40) =>
    invoke<DecisionRecord[]>("steward_decisions", { limit }),

  /** 当场编译一条决策并落库，不注入、不写事件流（人工核对依据用） */
  decideNow: (consoleId: string) =>
    invoke<DecisionRecord>("steward_decide_now", { consoleId }),

  execution: () => invoke<boolean>("steward_execution"),

  /** 自动应答总闸，冷启动默认关；不持久化 */
  setExecution: (enabled: boolean) =>
    invoke<boolean>("steward_set_execution", { enabled }),

  /** 升级卡「按建议执行」：只投递 seq，由常驻循环注入 */
  approve: (seq: number) => invoke<boolean>("steward_approve", { seq }),

  /** 升级卡「我自己来」；传纠正文本会写一条 Learned 偏好 */
  reject: (seq: number, correction?: string) =>
    invoke<boolean>("steward_reject", {
      seq,
      correction: correction ?? null,
    }),

  clearDecisions: () => invoke<void>("steward_clear_decisions"),

  policy: () => invoke<StewardPolicy>("steward_policy"),

  // ——— P3 所有权 / 授权 / 撤销 ———

  /** P3–P5 运行时一次性总览（所有权 / 授权 / 撤销 / 时间盒 / 恢复队列） */
  coreView: () => invoke<CoreView>("steward_core_view"),

  /** 用户敲键 → 后端 100ms 内转 ObservingOnly；返回处理延迟（ms），-1 = 该 tab 未托管 */
  userKey: (consoleId: string, keyAtMs: number) =>
    invoke<number>("steward_user_key", { consoleId, keyAtMs }),

  /** 手动接管：立刻转观察态，Steward 绝不再写这个 console */
  takeover: (consoleId: string) => invoke<boolean>("steward_takeover", { consoleId }),

  /** 交还：先读屏对齐（不一致自动降「确认」档），再给离开期间摘要 */
  handback: (consoleId: string, screenAligned?: boolean) =>
    invoke<AbsenceSummary>("steward_handback", {
      consoleId,
      screenAligned: screenAligned ?? null,
    }),

  /** Notify 档撤销：转观察态 + rolled_back + 写 Learned 偏好 */
  undo: (seq: number) => invoke<boolean>("steward_undo", { seq }),

  /** 真授权契约视图（含硬禁规则说明） */
  policyV2: (contractId?: string) =>
    invoke<PolicyView>("steward_policy_v2", { contractId: contractId ?? null }),

  /** 热更新授权契约：deny / allow / 预算 / 意向 deadline / host 作用域 */
  setAuthority: (contractId: string, authority: Authority) =>
    invoke<Authority>("steward_set_authority", { contractId, authority }),

  // ——— P4 时间盒 / Checkpoint / 恢复 ———

  timebox: () => invoke<TimeboxConfig>("steward_timebox"),

  setTimebox: (config: TimeboxConfig) =>
    invoke<TimeboxConfig>("steward_set_timebox", { config }),

  checkpoints: () => invoke<StewardCheckpoint[]>("steward_checkpoints"),

  /** 手动恢复：从 Checkpoint 重建契约，常驻循环先读屏对齐再决定降档 / 续跑 */
  resume: (contractId: string) =>
    invoke<string>("steward_resume", { contractId }),

  // ——— P5 解释链 / 复盘 / 蒸馏 / Planner ———

  /** 「为什么这次自动放行」的人话解释链 */
  explain: (seq: number) => invoke<Explanation | null>("steward_explain", { seq }),

  /** 跨会话复盘：做过什么、卡在哪、依据来自哪里 */
  review: (contractId?: string) =>
    invoke<ReviewReport>("steward_review", { contractId: contractId ?? null }),

  /** 统一审计：观察事件 + 决策记录合成一条时间线 */
  audit: (contractId?: string) =>
    invoke<AuditEntry[]>("steward_audit", { contractId: contractId ?? null }),

  /** 预览可沉淀的剧本候选（循环每 60s 自动写入 Brain） */
  distill: () => invoke<PlaybookCandidate[]>("steward_distill"),

  planner: () => invoke<PlannerView>("steward_planner"),

  /** LLM Planner 总闸：默认关、不持久化 */
  setPlanner: (enabled: boolean) => invoke<boolean>("steward_set_planner", { enabled }),

  /** 真·LLM 兜底：规则编不出回复时调模型，结果仍要过 PolicyEngine */
  planNow: (consoleId: string) => invoke<DecisionRecord>("steward_plan_now", { consoleId }),

  /** 订阅后端推来的观察 / 决策事件；返回取消订阅函数 */
  onEvent: (handler: (e: StewardEvent) => void): Promise<UnlistenFn> =>
    listen<StewardEvent>("nx://steward/event", (ev) => handler(ev.payload)),
};
