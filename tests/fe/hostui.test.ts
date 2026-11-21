/**
 * `lib/hostUi` 的回归测试：卡头按钮、「契约 / 设置」摘要、开始表单预设。
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  DEFAULT_HOST_TIER,
  HOST_PRESETS,
  denyShort,
  hostHeadActions,
  hostSettingsSummary,
  parseHostTier,
  presetOfTier,
} from "../../src/lib/hostUi";
import type { HostHumanCard, HostPhase } from "../../src/types";

const card: HostHumanCard = { id: "k1", kind: { kind: "ask" }, question: "?", options: [], reasons: [], createdWallMs: 0 };
const running: HostPhase = { kind: "running" };
const waiting: HostPhase = { kind: "waitingHuman", card };
const kinds = (p: HostPhase) => hostHeadActions(p).map((a) => a.kind);
const primary = (p: HostPhase) => hostHeadActions(p).find((a) => a.tone === "primary")?.label ?? null;

test("运行中：暂停 + 接管 + 停止，停止永远在最后", () => {
  assert.deepEqual(kinds(running), ["pause", "takeover", "stop"]);
  assert.equal(hostHeadActions(running).at(-1)?.tone, "danger");
  assert.equal(primary(running), null);
});

test("等你回答：卡头只有接管 / 停止（主动作是下面的卡片）", () => {
  assert.deepEqual(kinds(waiting), ["takeover", "stop"]);
});

test("暂停：预算 / 期限 → 加预算继续；手动 / 模型不可用 → 继续", () => {
  const budget: HostPhase = { kind: "paused", cause: { kind: "budget", reason: "x" } };
  assert.deepEqual(kinds(budget), ["resume", "stop"]);
  assert.equal(primary(budget), "加预算继续");
  assert.equal(primary({ kind: "paused", cause: { kind: "deadline" } }), "加预算继续");
  assert.equal(primary({ kind: "paused", cause: { kind: "user" } }), "继续");
});

test("你在驾驶：交还为主；预算暂停时接手了，仍可直接加预算继续", () => {
  assert.deepEqual(kinds({ kind: "userDriving", resume_to: running }), ["handback", "stop"]);
  assert.equal(primary({ kind: "userDriving", resume_to: running }), "交还");
  const budget: HostPhase = { kind: "paused", cause: { kind: "budget", reason: "x" } };
  assert.deepEqual(kinds({ kind: "userDriving", resume_to: budget }), ["resume", "handback", "stop"]);
  assert.equal(primary({ kind: "userDriving", resume_to: budget }), "加预算继续");
});

test("挂起 / 终态：卡头没有按钮", () => {
  assert.deepEqual(kinds({ kind: "suspended", reason: "consoleGone", resume_to: running }), []);
  assert.deepEqual(kinds({ kind: "completed", summary: "", checks: [] }), []);
  assert.deepEqual(kinds({ kind: "failed", error: "x" }), []);
});

test("设置摘要：档位 + 非底线禁止项短名；yolo 标出来", () => {
  assert.deepEqual(hostSettingsSummary("auto", ["secret", "permanent_grant", "delete", "force_push"]), {
    tier: "自动执行",
    deny: "禁止：删除、强推",
    yolo: false,
  });
  const y = hostSettingsSummary("yolo", ["secret", "permanent_grant", "systemctl"]);
  assert.equal(y.yolo, true);
  assert.equal(y.tier, "YOLO·完全放手");
  assert.equal(y.deny, "禁止：「systemctl」");
  assert.equal(hostSettingsSummary(undefined, null).deny, "禁止：仅底线项");
  assert.equal(denyShort("file_write"), "写文件");
});

test("预设 ↔ 档位；本地偏好坏了回落「自动」", () => {
  assert.deepEqual(
    HOST_PRESETS.map((p) => p.tier),
    ["approve", "auto", "yolo"]
  );
  assert.equal(presetOfTier("approve")?.label, "稳妥");
  assert.equal(presetOfTier("notify"), null);
  assert.equal(DEFAULT_HOST_TIER, "auto");
  assert.equal(parseHostTier(null), "auto");
  assert.equal(parseHostTier("garbage"), "auto");
  assert.equal(parseHostTier("notify"), "notify");
  assert.equal(parseHostTier("yolo"), "yolo");
});
