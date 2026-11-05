/**
 * Skill 前端辅助的自测：`lib/skill`。跑法：`npm test`
 *
 * 权威校验在后端；这里只钉住「占位符识别规则与后端一致」——
 * 编辑器据此提示「未声明的参数」，识别错了会漏提示或误报。
 */

import assert from "node:assert/strict";
import test from "node:test";

import type { SkillInput, SkillTrustView } from "../../src/types";
import {
  describeSkillStep,
  emptySkillInput,
  placeholderNames,
  scopeBlock,
  trustBadge,
  undeclaredParams,
} from "../../src/lib/skill";

test("placeholderNames：基本、空白、重复", () => {
  assert.deepEqual(placeholderNames("a {{x}} b {{ y_1 }} {{x}}"), ["x", "y_1", "x"]);
  assert.deepEqual(placeholderNames("plain"), []);
});

test("placeholderNames：模板语法与转义不是占位符", () => {
  assert.deepEqual(placeholderNames("docker inspect -f '{{.Name}}' c"), []);
  assert.deepEqual(placeholderNames("{{json .}} {{ a b }} {{}} {{1x}}"), []);
  assert.deepEqual(placeholderNames("echo \\{{x}} {{y}}"), ["y"]);
});

test("undeclaredParams：命令 / 按键 / pattern / verify 里的未声明引用", () => {
  const input: SkillInput = {
    ...emptySkillInput(),
    params: [{ name: "a", description: "", required: false, secret: false }],
    steps: [
      { kind: "exec", command: "echo {{a}} {{b}}" },
      { kind: "keys", keys: "{{c}}" },
      { kind: "expect_respond", expect: "{{d}}", respond: "y", secret: false, expectAfter: "{{e}}" },
      { kind: "secret", reason: "sudo", param: "pw" },
    ],
    verify: { expect: "{{f}} {{b}}" },
  };
  assert.deepEqual(undeclaredParams(input), ["b", "c", "d", "e", "f"]);
});

test("describeSkillStep：secret 步骤只显示理由", () => {
  assert.equal(describeSkillStep({ kind: "secret", reason: "sudo 密码", param: "pw" }), "输入密码：sudo 密码");
  assert.equal(
    describeSkillStep({ kind: "expect_respond", expect: "Password:", respond: "{{pw}}", secret: true }),
    "等到「Password:」→ 输入密码",
  );
  assert.equal(describeSkillStep({ kind: "exec", command: "ls" }), "ls");
});

// ——— 4.4 信任徽标 / 范围预判 ———

const view = (p: Partial<SkillTrustView>): SkillTrustView => ({
  id: "a",
  level: "confirm",
  streak: 0,
  needed: 3,
  reason: "原因",
  scopeOk: null,
  scopeReason: null,
  ...p,
});

test("trustBadge：没拿到结论时按最严的「需逐步确认」显示，绝不显示已信任", () => {
  const b = trustBadge(undefined);
  assert.equal(b.label, "需逐步确认");
  assert.equal(b.tone, "confirm");
});

test("trustBadge：三档与进度", () => {
  assert.deepEqual(trustBadge(view({})), { label: "需逐步确认", tone: "confirm", title: "原因" });
  assert.equal(trustBadge(view({ streak: 2 })).label, "需逐步确认 2/3");
  const t = trustBadge(view({ level: "trusted", streak: 3 }));
  assert.deepEqual([t.label, t.tone], ["已信任（只读）", "trusted"]);
  const g = trustBadge(view({ level: "guarded" }));
  assert.deepEqual([g.label, g.tone], ["始终逐步确认", "guarded"]);
});

test("trustBadge：悬停说明就是后端给的原因", () => {
  assert.equal(trustBadge(view({ level: "trusted", reason: "只读技能，已连续成功运行 3 次" })).title, "只读技能，已连续成功运行 3 次");
});

test("scopeBlock：只在后端明确说超出范围时才拦", () => {
  assert.equal(scopeBlock(undefined), null);
  assert.equal(scopeBlock(view({ scopeOk: null })), null);
  assert.equal(scopeBlock(view({ scopeOk: true })), null);
  assert.equal(scopeBlock(view({ scopeOk: false, scopeReason: "超出技能的适用主机范围：xx" })), "超出技能的适用主机范围：xx");
  assert.equal(scopeBlock(view({ scopeOk: false, scopeReason: null })), "超出技能的适用主机范围");
  assert.equal(scopeBlock(view({ scopeOk: false, scopeReason: "  " })), "超出技能的适用主机范围");
});
