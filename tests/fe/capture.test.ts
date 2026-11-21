/**
 * 屏幕帧采集的门控（纯逻辑）：Agent 运行驾驶终端期间采、结束就停；托管契约照旧。
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import { applyCapture, captureTargets } from "../../src/lib/termRegistry";

test("运行开始驾驶 → 采；还给用户 → 停", () => {
  let agent: Set<string> = new Set();
  agent = applyCapture(agent, { session: "t1", on: true });
  assert.deepEqual([...agent], ["t1"]);
  // 暂停（用户临时接管）仍算在运行里：后端发的是 on=true
  agent = applyCapture(agent, { session: "t1", on: true });
  assert.equal(agent.size, 1);
  agent = applyCapture(agent, { session: "t1", on: false });
  assert.equal(agent.size, 0, "运行结束零采集");
});

test("采集目标 = 托管契约 ∪ Agent 驾驶中", () => {
  const t = captureTargets(new Set(["host-1"]), new Set(["chat-2", "host-1"]));
  assert.deepEqual([...t].sort(), ["chat-2", "host-1"]);
  assert.equal(captureTargets(new Set(), new Set()).size, 0);
});
