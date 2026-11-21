/**
 * Skill 的前端纯逻辑（Phase 3.1）。类型见 `types.ts`，规则的权威实现在 Rust `agent::skill`
 * （校验、风险评估、渲染都以后端为准；这里只做编辑器要用的轻量辅助，不重复校验）。
 */

import type { SkillInput, SkillRisk, SkillStep, SkillTrustView } from "../types";

/**
 * 模板里引用的参数名，规则与后端一致：`{{name}}`（两侧可有空白），name 为
 * `[A-Za-z_][A-Za-z0-9_]*` 且 ≤64 字符；`{{.Name}}` 这类不是占位符；`\{{x}}` 是字面量。
 * 给编辑器做「自动补全参数声明 / 标出未声明的占位符」用。
 */
export function placeholderNames(text: string): string[] {
  const out: string[] = [];
  const re = /(\\)?\{\{\s*([A-Za-z_][A-Za-z0-9_]{0,63})\s*\}\}/g;
  for (let m = re.exec(text); m; m = re.exec(text)) {
    if (!m[1]) out.push(m[2]);
  }
  return out;
}

/** 一个步骤里所有会被代入的模板文本（命令 / 按键 / 应答 / 各 pattern） */
export function stepTemplates(step: SkillStep): string[] {
  const out: string[] = [];
  switch (step.kind) {
    case "exec":
      out.push(step.command);
      break;
    case "keys":
      out.push(step.keys);
      break;
    case "expect_respond":
      out.push(step.expect, step.respond);
      break;
    case "screen_check":
      out.push(step.expect);
      break;
    case "secret":
      break;
  }
  if (step.expectAfter) out.push(step.expectAfter);
  return out;
}

/** 一条 skill 引用了、但没有声明的参数名（去重，按出现顺序） */
export function undeclaredParams(input: Pick<SkillInput, "params" | "steps" | "verify">): string[] {
  const declared = new Set(input.params.map((p) => p.name));
  const seen = new Set<string>();
  const texts = input.steps.flatMap(stepTemplates);
  if (input.verify) texts.push(input.verify.expect);
  for (const t of texts) {
    for (const n of placeholderNames(t)) {
      if (!declared.has(n)) seen.add(n);
    }
  }
  return [...seen];
}

export const STEP_KIND_LABEL: Record<SkillStep["kind"], string> = {
  exec: "执行命令",
  keys: "发送按键",
  expect_respond: "等待并应答",
  secret: "输入密码",
  screen_check: "检查屏幕",
};

export const RISK_LABEL: Record<SkillRisk, string> = {
  low: "低风险",
  medium: "中风险",
  high: "高风险",
};

/** 步骤的一行摘要（列表 / 预览用）。secret 步骤只显示理由，永远没有值可显示。 */
export function describeSkillStep(step: SkillStep): string {
  switch (step.kind) {
    case "exec":
      return step.command;
    case "keys":
      return step.keys;
    case "expect_respond":
      return step.secret ? `等到「${step.expect}」→ 输入密码` : `等到「${step.expect}」→ ${step.respond}`;
    case "secret":
      return `输入密码：${step.reason}`;
    case "screen_check":
      return `屏幕应出现「${step.expect}」`;
  }
}

/** 一份空白草稿（新建 skill 的起点）。 */
export function emptySkillInput(): SkillInput {
  return {
    name: "",
    description: "",
    params: [],
    steps: [],
    verify: null,
    risk: "low",
    hostScope: { kind: "any" },
    source: "manual",
  };
}

// ——————————————————————————————— 信任分级（4.4） ———————————————————————————————

/** 列表里信任徽标的外观：`tone` 对应 CSS 类（`sk-trust.<tone>`）。 */
export interface TrustBadge {
  label: string;
  tone: "confirm" | "trusted" | "guarded";
  /** 鼠标悬停的完整说明（后端给的原因） */
  title: string;
}

/**
 * 把后端的信任结论变成徽标。后端是权威：这里只翻译，不重算。
 * 没拿到结论（还在加载 / 失败）时按最严的「需逐步确认」展示，不会误导成已信任。
 */
export function trustBadge(v: SkillTrustView | undefined): TrustBadge {
  if (!v) return { label: "需逐步确认", tone: "confirm", title: "每个写步骤都会先询问你" };
  switch (v.level) {
    case "trusted":
      return { label: "已信任（只读）", tone: "trusted", title: v.reason };
    case "guarded":
      return { label: "始终逐步确认", tone: "guarded", title: v.reason };
    default:
      return {
        label: v.streak > 0 ? `需逐步确认 ${v.streak}/${v.needed}` : "需逐步确认",
        tone: "confirm",
        title: v.reason,
      };
  }
}

/**
 * 当前会话下这份技能能不能跑：超出适用主机范围时返回拒绝原因（用来置灰「运行」并解释），否则 null。
 * 只是体验层的预判，真正的拒绝在后端 `run_skill`，不依赖这里。
 */
export function scopeBlock(v: SkillTrustView | undefined): string | null {
  if (!v || v.scopeOk !== false) return null;
  return v.scopeReason?.trim() || "超出技能的适用主机范围";
}
