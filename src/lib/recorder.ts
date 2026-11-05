/**
 * 录制器（Phase 3.2）的前端纯逻辑。后端转换与草稿模型见 `src-tauri/src/agent/recorder.rs`。
 *
 * 这里只放不碰 React / Tauri 的纯函数（可直接单测）：草稿类型、已用时间格式化、
 * 终端按键到「可上报的无文字操作」的映射、草稿的展示文案。
 * 状态与订阅在 `lib/useRecorder.ts`。
 */

import type { SkillStep } from "../types";

/** 后端 `RecorderStatus`（camelCase） */
export interface RecorderStatus {
  session: string;
  recording: boolean;
  startedAtMs?: number;
  hostLabel: string;
}

export type RecorderRisk = "low" | "medium" | "high";

/** 某一步的输出尾部：候选的 `expectAfter` 依据（已脱敏、限长）。不进步骤本身。 */
export interface StepHint {
  stepIndex: number;
  outputTail: string;
}

/** 后端 `RecordingDraft`（camelCase）。`steps` 就是 3.1 的 `SkillStep[]`。 */
export interface RecordingDraft {
  session: string;
  host: { sessionId: string; label: string };
  startedAtMs: number;
  endedAtMs: number;
  source: "recorded";
  steps: SkillStep[];
  hints: StepHint[];
  risk: RecorderRisk;
  /** 保存前需要用户看一眼的问题 */
  warnings: string[];
  /** 做过哪些整理，仅说明 */
  notes: string[];
  stats: { events: number; droppedNoise: number; merged: number };
}

export const idleStatus = (session: string): RecorderStatus => ({
  session,
  recording: false,
  hostLabel: "",
});

/** 已用时间 `m:ss`（≥1 小时 `h:mm:ss`）。时钟回拨 / 未开始按 0。 */
export function formatElapsed(startedAtMs: number | undefined, nowMs: number): string {
  if (startedAtMs === undefined) return "0:00";
  const s = Math.max(0, Math.floor((nowMs - startedAtMs) / 1000));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  const pad = (n: number) => String(n).padStart(2, "0");
  return h > 0 ? `${h}:${pad(m)}:${pad(sec)}` : `${m}:${pad(sec)}`;
}

/**
 * 终端 `onData` 的一段输入 → 要上报给录制器的无文字按键（`nx_send_keys` 语法）。
 * 只认**整段恰好是**这个按键的输入（粘贴 / 输入法的长文本不会误中）；其余返回 null。
 * 与后端白名单 `recorder::ALLOWED_KEYS` 对应，后端还会再校验一次。
 */
export function specialKeyFor(data: string): string | null {
  switch (data) {
    case "\x03":
      return "<C-c>";
    case "\x04":
      return "<C-d>";
    case "\x1a":
      return "<C-z>";
    default:
      return null;
  }
}

/** 一步的单行描述（草稿预览用）。`secret` 只有说明，没有内容。 */
export function describeStep(step: SkillStep): string {
  switch (step.kind) {
    case "exec":
      return step.command;
    case "keys":
      return `按键 ${step.keys}`;
    case "expect_respond":
      return `等「${step.expect}」→ 回答 ${step.secret ? "（密码）" : step.respond}`;
    case "secret":
      return `输入密码：${step.reason}`;
    case "screen_check":
      return `检查屏幕「${step.expect}」`;
  }
}

/** 某一步对应的 hint（没有返回 undefined） */
export function hintFor(draft: RecordingDraft, stepIndex: number): StepHint | undefined {
  return draft.hints.find((h) => h.stepIndex === stepIndex);
}

/** 草稿一行摘要：「5 步 · 风险高 · 2 条提醒」 */
export function summarizeDraft(d: RecordingDraft): string {
  const risk = d.risk === "high" ? "风险高" : d.risk === "medium" ? "风险中" : "风险低";
  const parts = [`${d.steps.length} 步`, risk];
  if (d.warnings.length > 0) parts.push(`${d.warnings.length} 条提醒`);
  return parts.join(" · ");
}

/** 录制时长（毫秒，不会为负） */
export function draftDurationMs(d: RecordingDraft): number {
  return Math.max(0, d.endedAtMs - d.startedAtMs);
}

/** 两份状态是否一致（避免重复 setState） */
export function statusEqual(a: RecorderStatus, b: RecorderStatus): boolean {
  return (
    a.session === b.session &&
    a.recording === b.recording &&
    a.startedAtMs === b.startedAtMs &&
    a.hostLabel === b.hostLabel
  );
}
