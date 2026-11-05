/**
 * Skill 前端辅助的自测：`lib/skill`。跑法：`npm test`
 *
 * 权威校验在后端；这里只钉住「占位符识别规则与后端一致」——
 * 编辑器据此提示「未声明的参数」，识别错了会漏提示或误报。
 */

import assert from "node:assert/strict";
import test from "node:test";

import type { SkillInput } from "../../src/types";
import {
  describeSkillStep,
  emptySkillInput,
  placeholderNames,
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
