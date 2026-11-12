/**
 * `lib/agentBudget`（4.6 运行预算与暂停 / 恢复）的回归测试：配置规整、旧配置兼容、暂停标记随步骤事件进出。
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  DEFAULT_RUN_BUDGET,
  MAX_MINUTES_CEIL,
  MAX_STEPS_CEIL,
  isBudgetPause,
  nextPause,
  normalizeBudget,
  parseBudgetInput,
  pauseBannerText,
} from "../../src/lib/agentBudget";
import { DEFAULT_AGENT_CONFIG } from "../../src/lib/agent";

test("默认预算大于旧的硬编码 8 步，输出字符默认不限", () => {
  assert.deepEqual(DEFAULT_RUN_BUDGET, { maxSteps: 25, maxMinutes: 10, maxOutputChars: 0 });
  assert.ok(DEFAULT_RUN_BUDGET.maxSteps > 8);
  assert.deepEqual(DEFAULT_AGENT_CONFIG.budget, DEFAULT_RUN_BUDGET);
});

test("旧配置（没有 budget）规整为默认预算", () => {
  assert.deepEqual(normalizeBudget(undefined), DEFAULT_RUN_BUDGET);
  assert.deepEqual(normalizeBudget(null), DEFAULT_RUN_BUDGET);
  assert.deepEqual(normalizeBudget({}), DEFAULT_RUN_BUDGET);
});

test("部分字段缺失时只补缺的那项；0 对时间 / 输出是合法的「不限」", () => {
  assert.deepEqual(normalizeBudget({ maxSteps: 40 }), { maxSteps: 40, maxMinutes: 10, maxOutputChars: 0 });
  assert.deepEqual(normalizeBudget({ maxMinutes: 0, maxOutputChars: 5000 }), {
    maxSteps: 25,
    maxMinutes: 0,
    maxOutputChars: 5000,
  });
});

test("非法 / 越界值被收敛：0 步回落默认，其余夹紧到合法域", () => {
  assert.equal(normalizeBudget({ maxSteps: 0 }).maxSteps, 25);
  assert.equal(normalizeBudget({ maxSteps: NaN }).maxSteps, 25);
  assert.equal(normalizeBudget({ maxSteps: 1e9 }).maxSteps, MAX_STEPS_CEIL);
  assert.equal(normalizeBudget({ maxMinutes: -5 }).maxMinutes, 0);
  assert.equal(normalizeBudget({ maxMinutes: 1e9 }).maxMinutes, MAX_MINUTES_CEIL);
  assert.equal(normalizeBudget({ maxMinutes: NaN }).maxMinutes, 10);
  assert.equal(normalizeBudget({ maxSteps: 12.6 }).maxSteps, 13);
});

test("设置页输入：空串 / 非数字是 NaN，交给 normalize 兜底", () => {
  assert.ok(Number.isNaN(parseBudgetInput("")));
  assert.ok(Number.isNaN(parseBudgetInput("  ")));
  assert.equal(parseBudgetInput(" 30 "), 30);
  assert.equal(normalizeBudget({ maxSteps: parseBudgetInput("") }).maxSteps, 25);
});

test("paused 步骤进入暂停并记住原因，resumed 退出，其余步骤不改变", () => {
  assert.equal(nextPause(undefined, { kind: "paused", pauseKind: "steps" }), "steps");
  assert.equal(nextPause(undefined, { kind: "paused", pauseKind: "time" }), "time");
  assert.equal(nextPause(undefined, { kind: "paused", pauseKind: "output" }), "output");
  assert.equal(nextPause(undefined, { kind: "paused", pauseKind: "user" }), "user");
  assert.equal(nextPause(undefined, { kind: "paused", pauseKind: "llm" }), "llm");
  // 未知 / 缺失的原因按手动暂停处理，仍然是「暂停」
  assert.equal(nextPause(undefined, { kind: "paused", pauseKind: "???" }), "user");
  assert.equal(nextPause(undefined, { kind: "paused" }), "user");
  assert.equal(nextPause("steps", { kind: "resumed" }), undefined);
  // 暂停期间 / 运行期间到来的普通步骤不改状态
  assert.equal(nextPause("steps", { kind: "note" }), "steps");
  assert.equal(nextPause(undefined, { kind: "exec" }), undefined);
});

test("暂停横幅按原因给文案，且都提示「继续」与「停止」", () => {
  for (const k of ["steps", "time", "output", "user", "llm"] as const) {
    const t = pauseBannerText(k);
    assert.ok(t.includes("继续") && t.includes("停止"), t);
  }
  assert.ok(pauseBannerText("steps").includes("步数"));
  assert.ok(pauseBannerText("time").includes("时间"));
  assert.ok(pauseBannerText("llm").includes("模型服务"));
  // 模型出错不是预算用尽：托管页不该把它显示成「已达到预算上限」
  assert.equal(isBudgetPause("llm"), false);
});
