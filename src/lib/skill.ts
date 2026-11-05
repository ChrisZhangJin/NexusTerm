/**
 * Skill 的前端纯逻辑（Phase 3.1）。类型见 `types.ts`，规则的权威实现在 Rust `agent::skill`
 * （校验、风险评估、渲染都以后端为准；这里只做编辑器要用的轻量辅助，不重复校验）。
 */

import type { SkillInput, SkillRisk, SkillStep } from "../types";

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
