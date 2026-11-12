/**
 * 运行预算与暂停 / 恢复（4.6）的前端纯逻辑。后端见 `src-tauri/src/agent/budget.rs`。
 *
 * 面板不维护独立的「暂停状态机」：只从后端推来的步骤事件里读——
 * `paused` 卡 = 这一轮停在暂停上（带原因分类），`resumed` 卡 = 又跑起来了；
 * 其余步骤不改变它。一轮结束 / 出错 / 被停止时对话状态整体重建，暂停标记随之消失。
 */

/** 为什么暂停：预算三项之一，或用户手动暂停。与后端 `StepPayload.pauseKind` 一致。 */
export type PauseKind = "steps" | "time" | "output" | "user";

/** 与后端 `RunBudget`（camelCase）对应。0 的含义见各字段。 */
export interface RunBudget {
  /** 最多执行多少步（动作）。≥1 */
  maxSteps: number;
  /** 最长运行多少分钟。0 = 不限时 */
  maxMinutes: number;
  /** 喂回模型 + 模型回复的累计字符上限。0 = 不限 */
  maxOutputChars: number;
}

export const DEFAULT_RUN_BUDGET: RunBudget = { maxSteps: 25, maxMinutes: 10, maxOutputChars: 0 };

export const MAX_STEPS_CEIL = 500;
export const MAX_MINUTES_CEIL = 24 * 60;
export const MAX_OUTPUT_CHARS_CEIL = 100_000_000;

function intIn(v: unknown, lo: number, hi: number, fallback: number): number {
  const n = Math.round(Number(v));
  if (!Number.isFinite(n)) return fallback;
  return Math.min(hi, Math.max(lo, n));
}

/** 规整预算：缺失 / 非法值回落默认，越界夹紧（与后端 `RunBudget::clamped` 同向）。 */
export function normalizeBudget(raw: Partial<RunBudget> | null | undefined): RunBudget {
  const r = raw ?? {};
  const d = DEFAULT_RUN_BUDGET;
  const steps = Math.round(Number(r.maxSteps));
  return {
    // 0 / 缺失 / 非法 → 默认（0 步没有意义）
    maxSteps:
      Number.isFinite(steps) && steps > 0 ? Math.min(steps, MAX_STEPS_CEIL) : d.maxSteps,
    maxMinutes: r.maxMinutes === undefined ? d.maxMinutes : intIn(r.maxMinutes, 0, MAX_MINUTES_CEIL, d.maxMinutes),
    maxOutputChars:
      r.maxOutputChars === undefined
        ? d.maxOutputChars
        : intIn(r.maxOutputChars, 0, MAX_OUTPUT_CHARS_CEIL, d.maxOutputChars),
  };
}

/** 设置页输入框的字符串 → 数字（空 / 非数字 = NaN，交给 normalizeBudget 兜底）。 */
export function parseBudgetInput(s: string): number {
  const t = s.trim();
  return t === "" ? NaN : Number(t);
}

/** 带 `kind` / `pauseKind` 的步骤（`AgentStep` 的子集，避免循环依赖） */
export interface PauseStepLike {
  kind: string;
  pauseKind?: string;
}

const PAUSE_KINDS: readonly string[] = ["steps", "time", "output", "user"];

/**
 * 给定当前暂停状态与一个新到的步骤，返回新的暂停状态（`undefined` = 没在暂停）。
 * `paused` 卡进入暂停（未知 / 缺失的原因按 user 处理），`resumed` 卡退出，其余原样。
 */
export function nextPause(cur: PauseKind | undefined, step: PauseStepLike): PauseKind | undefined {
  if (step.kind === "paused") {
    return PAUSE_KINDS.includes(step.pauseKind ?? "") ? (step.pauseKind as PauseKind) : "user";
  }
  if (step.kind === "resumed") return undefined;
  return cur;
}

/** 暂停横幅的文案。 */
export function pauseBannerText(kind: PauseKind): string {
  switch (kind) {
    case "steps":
      return "已达到步数上限，本轮暂停。点「继续」换一份新预算接着做，或点「停止」结束。";
    case "time":
      return "已达到时间上限，本轮暂停。点「继续」换一份新预算接着做，或点「停止」结束。";
    case "output":
      return "已达到输出量上限，本轮暂停。点「继续」换一份新预算接着做，或点「停止」结束。";
    default:
      return "本轮已暂停。点「继续」接着做，或点「停止」结束。";
  }
}

/** 托管（steward）模式本轮的长度偏好：「最长时长」与「最多步数」。后端会再夹紧到 500 步 / 1440 分钟。 */
export interface StewardBudgetPref {
  maxMinutes: number;
  maxSteps: number;
}

/** 「最长时长」下拉的可选项（分钟）。 */
export const STEWARD_DURATION_OPTIONS: ReadonlyArray<{ minutes: number; label: string }> = [
  { minutes: 120, label: "2 小时" },
  { minutes: 360, label: "6 小时" },
  { minutes: 720, label: "12 小时" },
  { minutes: 1440, label: "24 小时" },
];

export const DEFAULT_STEWARD_BUDGET_PREF: StewardBudgetPref = { maxMinutes: 360, maxSteps: 400 };
export const LS_STEWARD_BUDGET = "nx.steward.budget";

/** 读本机存的偏好：缺失 / 损坏 / 越界 都回落默认；时长只认下拉里的选项，步数夹到 [1, 500]。 */
export function parseStewardBudgetPref(raw: string | null | undefined): StewardBudgetPref {
  const d = DEFAULT_STEWARD_BUDGET_PREF;
  if (!raw) return d;
  try {
    const o = JSON.parse(raw) as Partial<StewardBudgetPref> | null;
    const mins = Number(o?.maxMinutes);
    const steps = Math.round(Number(o?.maxSteps));
    return {
      maxMinutes: STEWARD_DURATION_OPTIONS.some((x) => x.minutes === mins) ? mins : d.maxMinutes,
      maxSteps: Number.isFinite(steps) && steps >= 1 ? Math.min(steps, MAX_STEPS_CEIL) : d.maxSteps,
    };
  } catch {
    return d;
  }
}

/** 预算类暂停（步数 / 时间 / 输出量用尽）—— 区别于用户手动暂停。 */
export const isBudgetPause = (k: PauseKind | undefined): boolean =>
  k === "steps" || k === "time" || k === "output";

/** 托管页预算用尽横幅。后端暂停后 1 小时没人点「继续」会自动结束这一轮。 */
export const BUDGET_EXHAUSTED_BANNER = "已达到预算上限，点「继续」接着跑（1 小时内不点会自动结束）";
