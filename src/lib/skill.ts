/**
 * Skill 的前端纯逻辑（Phase 3.1）。类型见 `types.ts`，规则的权威实现在 Rust `agent::skill`
 * （校验、风险评估、渲染都以后端为准；这里只做编辑器要用的轻量辅助，不重复校验）。
 */

import { t } from "../i18n";
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

/** 步骤类型的显示名。各项是 getter：读的时候按当前语言取 */
export const STEP_KIND_LABEL: Readonly<Record<SkillStep["kind"], string>> = {
  get exec() {
    return t("执行命令", "Run command");
  },
  get keys() {
    return t("发送按键", "Send keys");
  },
  get expect_respond() {
    return t("等待并应答", "Wait & respond");
  },
  get secret() {
    return t("输入密码", "Enter password");
  },
  get screen_check() {
    return t("检查屏幕", "Check screen");
  },
};

/** 风险的显示名（getter，跟随当前语言） */
export const RISK_LABEL: Readonly<Record<SkillRisk, string>> = {
  get low() {
    return t("低风险", "Low risk");
  },
  get medium() {
    return t("中风险", "Medium risk");
  },
  get high() {
    return t("高风险", "High risk");
  },
};

/** 步骤的一行摘要（列表 / 预览用）。secret 步骤只显示理由，永远没有值可显示。 */
export function describeSkillStep(step: SkillStep): string {
  switch (step.kind) {
    case "exec":
      return step.command;
    case "keys":
      return step.keys;
    case "expect_respond":
      return step.secret
        ? t("等到「{e}」→ 输入密码", "Wait for “{e}” → enter password", { e: step.expect })
        : t("等到「{e}」→ {r}", "Wait for “{e}” → {r}", { e: step.expect, r: step.respond });
    case "secret":
      return t("输入密码：{r}", "Enter password: {r}", { r: step.reason });
    case "screen_check":
      return t("屏幕应出现「{e}」", "Screen should show “{e}”", { e: step.expect });
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
  const confirm = t("需逐步确认", "Confirm each step");
  if (!v) return { label: confirm, tone: "confirm", title: t("每个写步骤都会先询问你", "You'll be asked before every step that changes things") };
  switch (v.level) {
    case "trusted":
      return { label: t("已信任（只读）", "Trusted (read-only)"), tone: "trusted", title: v.reason };
    case "guarded":
      return { label: t("始终逐步确认", "Always confirm each step"), tone: "guarded", title: v.reason };
    default:
      return {
        label: v.streak > 0 ? `${confirm} ${v.streak}/${v.needed}` : confirm,
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
  return v.scopeReason?.trim() || t("超出技能的适用主机范围", "This host is outside the skill's host scope");
}
