/**
 * `lib/agentRun` 的回归测试：从 AgentPanel 抽出的运行状态 reducer，以及托管视图用的切片 / 待处理计数。
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  EMPTY_CONV,
  isHandEyeKind,
  pendingPrompts,
  sliceFromMessage,
  withApprove,
  withConclusion,
  withDecision,
  withStep,
  type Conversation,
} from "../../src/lib/agentRun";
import type { AgentMessage, AgentStep } from "../../src/types";

const step = (o: Partial<AgentStep>): AgentStep => ({ kind: "note", step: 1, ...o });
const busy: Conversation = { ...EMPTY_CONV, busy: true };

test("watch 属于「手和眼」：收尾步骤并回同目标同内容的进行中卡片", () => {
  assert.equal(isHandEyeKind("watch"), true);
  assert.equal(isHandEyeKind("exec"), false);
  let c = withStep(busy, step({ kind: "watch", target: "t1", command: "盯屏" }));
  assert.equal(c.messages.length, 1);
  c = withStep(c, step({ kind: "watch", target: "t1", command: "盯屏", output: "done" }));
  assert.equal(c.messages.length, 1);
  assert.equal(c.messages[0].step?.output, "done");
});

test("未知步骤种类原样追加成一张卡，不丢", () => {
  const c = withStep(busy, step({ kind: "something-new", text: "x" }));
  assert.equal(c.messages.length, 1);
  assert.equal(c.messages[0].step?.kind, "something-new");
});

test("exec → result 合成一张卡片", () => {
  let c = withStep(busy, step({ kind: "exec", target: "t", command: "ls" }));
  c = withStep(c, step({ kind: "result", target: "t", command: "ls", output: "a" }));
  assert.equal(c.messages.length, 1);
  assert.equal(c.messages[0].step?.kind, "result");
});

test("结论落地即收掉 busy", () => {
  const c = withConclusion(busy, "完成");
  assert.equal(c.busy, false);
  assert.equal(c.messages[c.messages.length - 1].text, "完成");
});

test("sliceFromMessage：只取起手消息之后；找不到 / 没有起点就是空", () => {
  const msgs: AgentMessage[] = [
    { id: "a", role: "user", text: "聊天", ts: 1 },
    { id: "g", role: "user", text: "托管目标：x", ts: 2 },
    { id: "s", role: "assistant", text: "", ts: 3, step: step({ kind: "exec" }) },
  ];
  assert.deepEqual(sliceFromMessage(msgs, "g").map((m) => m.id), ["s"]);
  assert.deepEqual(sliceFromMessage(msgs, "zzz"), []);
  assert.deepEqual(sliceFromMessage(msgs, null), []);
});

test("pendingPrompts：审批 / 密码卡片答复后不再计数", () => {
  let c = withApprove(busy, { requestId: "r1", target: "t", targetLabel: "T", command: "rm -rf x", reasons: [] } as never);
  assert.equal(pendingPrompts(c.messages), 1);
  c = withDecision(c, "r1", true);
  assert.equal(pendingPrompts(c.messages), 0);
});

test("watch 心跳（output 为空、同 target+command）原地更新，不另起卡", () => {
  let c = withStep(busy, step({ kind: "watch", target: "t1", command: "盯屏", text: "已盯 60s" }));
  for (let i = 0; i < 5; i++) {
    c = withStep(c, step({ kind: "watch", target: "t1", command: "盯屏", text: `已盯 ${(i + 2) * 60}s` }));
  }
  assert.equal(c.messages.length, 1);
  assert.equal(c.messages[0].step?.text, "已盯 360s");
  assert.equal(c.messages[0].step?.output, undefined);
  // 收尾（output 为空串也算）合并进同一张卡
  c = withStep(c, step({ kind: "watch", target: "t1", command: "盯屏", output: "" }));
  assert.equal(c.messages.length, 1);
  assert.equal(c.messages[0].step?.output, "");
});

test("watch 心跳：不同 target / command 是另一次盯屏，已收尾的不再被心跳复活", () => {
  let c = withStep(busy, step({ kind: "watch", target: "t1", command: "A" }));
  c = withStep(c, step({ kind: "watch", target: "t2", command: "A" }));
  c = withStep(c, step({ kind: "watch", target: "t1", command: "B" }));
  assert.equal(c.messages.length, 3);
  // t1/A 收尾后，再来的同款心跳是新一次盯屏 → 新卡，且不改已收尾那张
  c = withStep(c, step({ kind: "watch", target: "t1", command: "A", output: "ok" }));
  c = withStep(c, step({ kind: "watch", target: "t1", command: "A" }));
  assert.equal(c.messages.length, 4);
  assert.equal(c.messages[0].step?.output, "ok");
  assert.equal(c.messages[3].step?.output, undefined);
});

import {
  DEFAULT_STEWARD_BUDGET_PREF,
  isBudgetPause,
  parseStewardBudgetPref,
} from "../../src/lib/agentBudget";

test("托管长度偏好：缺失 / 损坏回落默认，时长只认选项，步数夹到 [1,500]", () => {
  assert.deepEqual(parseStewardBudgetPref(null), DEFAULT_STEWARD_BUDGET_PREF);
  assert.deepEqual(parseStewardBudgetPref("not json"), DEFAULT_STEWARD_BUDGET_PREF);
  assert.deepEqual(DEFAULT_STEWARD_BUDGET_PREF, { maxMinutes: 360, maxSteps: 400 });
  assert.deepEqual(parseStewardBudgetPref('{"maxMinutes":720,"maxSteps":50}'), { maxMinutes: 720, maxSteps: 50 });
  assert.deepEqual(parseStewardBudgetPref('{"maxMinutes":999,"maxSteps":9999}'), { maxMinutes: 360, maxSteps: 500 });
  assert.deepEqual(parseStewardBudgetPref('{"maxMinutes":120,"maxSteps":0}'), { maxMinutes: 120, maxSteps: 400 });
});

test("isBudgetPause：预算三项是，手动暂停 / 未暂停不是", () => {
  assert.equal(isBudgetPause("steps"), true);
  assert.equal(isBudgetPause("time"), true);
  assert.equal(isBudgetPause("output"), true);
  assert.equal(isBudgetPause("user"), false);
  assert.equal(isBudgetPause(undefined), false);
});
