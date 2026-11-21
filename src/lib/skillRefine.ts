/**
 * 「整理为技能」（Phase 3.3）的前端纯逻辑。后端见 `src-tauri/src/agent/skill_refine.rs`：
 * 模型的输出在那里被夹紧并校验，这里只负责类型、「直接编辑」用的机械转换、结果文案。
 */

import type { SkillHostScope, SkillInput } from "../types";
import type { RecordingDraft } from "./recorder";

/** 后端 `RefinedSkillDraft`（camelCase）。`draft` 已通过后端校验，可直接当 `SkillManager.initialDraft` */
export interface RefinedSkillDraft {
  draft: SkillInput;
  /** 模型对整理决策的说明（只展示） */
  notes: string[];
  /** 程序侧的提醒：被夹紧的项、截断、回退原因…… */
  warnings: string[];
  /** 模型两次都没给出合格结果，退回了未整理的录制草稿 */
  fallback: boolean;
  /** 调用模型的次数（1..=2） */
  attempts: number;
}

/** 与后端 `skill::MAX_HOST_ENTRY_CHARS` / `MAX_NAME_CHARS` 一致 */
const MAX_HOST_ENTRY_CHARS = 255;
const MAX_NAME_CHARS = 80;
/** 与后端 `redact::PLACEHOLDER` 一致 */
const REDACTED = "[REDACTED]";

/**
 * 录制标签 → 可写进 `hostScope` 的主机条目（与后端 `skill_refine::host_entry` 同规则）：
 * 取第一个空白分隔的词、去掉 `user@` 前缀；空 / 含脱敏占位符 / 过长 / 含控制字符则不可用。
 */
export function hostEntry(label: string): string | null {
  const first = label.trim().split(/\s+/)[0] ?? "";
  if (!first) return null;
  const host = (first.split("@").pop() ?? "").trim();
  if (!host || host.includes(REDACTED) || [...host].length > MAX_HOST_ENTRY_CHARS) return null;
  // eslint-disable-next-line no-control-regex
  if (/[\u0000-\u0020\u007f-\u009f]/.test(host)) return null;
  return host;
}

/** 主机范围：有可用主机名 → 限定到它；否则不限（与后端同样的决定） */
export function hostScopeFor(draft: RecordingDraft): SkillHostScope {
  const h = hostEntry(draft.host.label);
  return h ? { kind: "list", hosts: [h] } : { kind: "any" };
}

/**
 * 「直接编辑」：不经模型，把机械草稿原样转成 `SkillInput`（没有参数 / 验证；来源 recorded；
 * 风险取录制时的评估，后端保存时还会再抬高）。保存前仍由后端 `skill_create` 校验。
 */
export function mechanicalInput(draft: RecordingDraft): SkillInput {
  const h = hostEntry(draft.host.label);
  return {
    name: [...(h ? `录制的操作（${h}）` : "录制的操作")].slice(0, MAX_NAME_CHARS).join(""),
    description: "由终端录制直接生成的草稿，未经整理：没有提取参数、没有期望与验证，请编辑后再保存。",
    params: [],
    steps: draft.steps,
    verify: null,
    risk: draft.risk,
    hostScope: hostScopeFor(draft),
    source: "recorded",
  };
}

/** 结果的一行标题（卡片里显示） */
export function refineHeadline(r: RefinedSkillDraft): string {
  if (r.fallback) return "整理没有成功，已退回为未整理的录制草稿";
  const bits = [`整理完成：${r.draft.steps.length} 步`, `${r.draft.params.length} 个参数`];
  if (r.draft.verify) bits.push("含验证");
  if (r.attempts > 1) bits.push("经过一次修正重试");
  return bits.join(" · ");
}

/** 去掉空白条目、去重（保持顺序），给界面列表用 */
export function cleanLines(lines: readonly string[]): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const l of lines) {
    const t = l.trim();
    if (t && !seen.has(t)) {
      seen.add(t);
      out.push(t);
    }
  }
  return out;
}
