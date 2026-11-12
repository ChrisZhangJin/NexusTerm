/**
 * NexusAgent v2 / 会话织物与常驻 Steward 的前端命令层。
 *
 * 设计见 `docs/nexus-agent-steward-design.md`。P0 做会话织物（枚举 console、
 * 屏幕帧持续同步、登记契约）；P1 起后端多了一个常驻 Steward：自己按心跳读屏、
 * 分类局面、判空转，把结果写成事件流推到前端。
 *
 * `profile` / `intent` / `knowledge` / `playbook` 是用户预先承载的几类依据；托管任务的每次写入
 * 落一条 `DecisionRecord`（引用了哪几条依据、为什么），可回放、可纠正、可复盘。
 *
 * 守护进程只观察：动手的唯一入口是托管任务（`lib/host.ts` 的 `host_*` 命令）。旧的零 LLM
 * 决策 / 升级卡 / 撤销 / 接管交还 / 恢复 / Planner 绑定随后端写路径在 PR8 删除。
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
  /** hosted（有托管契约） | none。托管相位看 `host_list` */
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
  | "contract_stopped";

export interface StewardEvent {
  seq: number;
  tsMs: number;
  consoleId: string;
  contractId: string;
  kind: StewardEventKind;
  message: string;
  /** 建议卡：只建议、不执行 */
  suggestion: string | null;
  situation: Situation | null;
  frameId: number;
  screenHash: string;
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
  decisionCount: number;
}

export interface DecisionAction {
  /** answer | secret | wait | observe | advance | handoff */
  kind: string;
  text: string;
  optionIndex: string | null;
  injectable: boolean;
  /** 选中编号后的提交方式：digit_submits 只发数字；arrow_select 不注入；旧记录缺省 = needs_enter */
  submit?: "needs_enter" | "digit_submits" | "arrow_select";
}

export interface CtxRef {
  /** profile | intent | knowledge | playbook | authority | policy | history */
  source: string;
  id: string;
  snippet: string;
}

/** 守护进程时代的升级卡：只会出现在旧的决策记录里，不再生成 */
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

// ————————————————————————— 解释链 / 复盘 / 审计 —————————————————————————

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
}

export interface StewardRuntimeInfo {
  running: boolean;
  tickCount: number;
  classifyCount: number;
  lastTickMs: number;
  lastClassifyMs: number;
  eventSeq: number;
  eventCount: number;
  config: StewardConfig;
  consoles: StewardStateInfo[];
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

export const EVENT_LABELS: Record<StewardEventKind, string> = {
  situation_changed: "局面变化",
  no_progress: "空转",
  stale_feed: "帧断流",
  feed_restored: "帧恢复",
  screen_lost: "无屏幕帧",
  contract_started: "开始托管",
  contract_stopped: "停止托管",
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

/** 决策记录的人话标签。放这里而不是组件里，方便单测与以后复用。 */
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
  hosted: "托管中",
  none: "未托管",
};

export const contractStatusLabel = (s: string): string => CONTRACT_STATUS_LABELS[s] ?? s;

/** 当前有托管契约的 console —— 只有它们需要屏幕帧 / 心跳上报 */
export const stewardConsoleIds = (contracts: StewardContract[]): string[] => [
  ...new Set(contracts.map((c) => c.consoleId)),
];

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

  stop: (contractId: string) => invoke<boolean>("steward_stop", { contractId }),

  status: () => invoke<StewardContract[]>("steward_status"),

  /** 增量拉观察事件；sinceSeq 是前端已见到的最大 seq */
  events: (sinceSeq = 0, limit = 60) =>
    invoke<StewardEvent[]>("steward_events", { sinceSeq, limit }),

  /** 常驻观察循环的运行统计：tick / 分类次数 / 心跳 */
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

  // ——— 决策依据 / 决策记录 ———

  /** 一次拿全偏好 / 意向 / 知识 / 剧本 */
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

  /** 回放最近决策（托管写入 / 反射代答各一条） */
  decisions: (limit = 40) =>
    invoke<DecisionRecord[]>("steward_decisions", { limit }),

  clearDecisions: () => invoke<void>("steward_clear_decisions"),

  /** 「为什么这次自动放行」的人话解释链 */
  explain: (seq: number) => invoke<Explanation | null>("steward_explain", { seq }),

  /** 跨会话复盘：做过什么、卡在哪、依据来自哪里 */
  review: (contractId?: string) =>
    invoke<ReviewReport>("steward_review", { contractId: contractId ?? null }),

  /** 统一审计：观察事件 + 决策记录合成一条时间线 */
  audit: (contractId?: string) =>
    invoke<AuditEntry[]>("steward_audit", { contractId: contractId ?? null }),

  /** 订阅后端推来的观察事件；返回取消订阅函数 */
  onEvent: (handler: (e: StewardEvent) => void): Promise<UnlistenFn> =>
    listen<StewardEvent>("nx://steward/event", (ev) => handler(ev.payload)),
};
