/**
 * `lib/host` 的回归测试：相位 → 胶囊、卡片与按钮。
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  basePhase,
  hostButtons,
  hostDetail,
  hostForConsole,
  hostPauseKind,
  hostPill,
  pendingCard,
  upsertHost,
} from "../../src/lib/host";
import type { HostHumanCard, HostPhase, HostStatus } from "../../src/types";

const card = (kind: "ask" | "approve" = "ask"): HostHumanCard => ({
  id: "k1",
  kind: { kind },
  question: "部署到哪？",
  options: [],
  reasons: [],
  createdWallMs: 0,
});
const running: HostPhase = { kind: "running" };
const waiting: HostPhase = { kind: "waitingHuman", card: card() };
const budget: HostPhase = { kind: "paused", cause: { kind: "budget", reason: "已执行 400 步" } };
const drive = (p: HostPhase): HostPhase => ({ kind: "userDriving", resume_to: p });

test("胶囊文案覆盖全部相位", () => {
  const cases: Array<[HostPhase, string]> = [
    [running, "运行中"],
    [waiting, "等你回答"],
    [{ kind: "waitingHuman", card: card("approve") }, "等你批准"],
    [drive(running), "你在驾驶"],
    [budget, "预算暂停"],
    [{ kind: "paused", cause: { kind: "deadline" } }, "预算暂停"],
    [{ kind: "paused", cause: { kind: "llmDown", error: "429" } }, "暂停（模型不可用）"],
    [{ kind: "suspended", reason: "appRestart", resume_to: running }, "待重新接驳"],
    [{ kind: "completed", summary: "ok", checks: [] }, "完成"],
    [{ kind: "failed", error: "x" }, "失败"],
    [{ kind: "stopped" }, "已停止"],
  ];
  for (const [p, label] of cases) assert.equal(hostPill(p).label, label, p.kind);
});

test("人在驾驶时卡片仍可见，按钮只给交还", () => {
  const p = drive(waiting);
  assert.equal(basePhase(p).kind, "waitingHuman");
  assert.equal(pendingCard(p)?.id, "k1");
  const b = hostButtons(p);
  assert.equal(b.handback, true);
  assert.equal(b.takeover, false);
  assert.equal(b.stop, true);
  assert.match(hostDetail(p) ?? "", /交还后回到「等你回答」/);
});

test("预算暂停时按键再交还：仍可加预算继续", () => {
  const b = hostButtons(drive(budget));
  assert.equal(b.resume, true);
  assert.equal(hostPauseKind(drive(budget)), "steps");
  assert.equal(hostButtons(running).resume, false);
  assert.equal(hostButtons(running).pause, true);
});

test("终态没有可点的按钮", () => {
  const b = hostButtons({ kind: "stopped" });
  assert.deepEqual(b, { takeover: false, handback: false, resume: false, pause: false, stop: false });
});

test("按 console 取托管：未结束优先，否则最新结局", () => {
  const st = (id: string, consoleId: string, phase: HostPhase): HostStatus => ({
    contractId: id,
    runId: `host:${id}`,
    consoleId,
    goal: "g",
    state: phase.kind,
    phase,
  });
  let m: Record<string, HostStatus> = {};
  m = upsertHost(m, st("steward-t1-100", "t1", { kind: "stopped" }));
  m = upsertHost(m, st("steward-t1-200", "t1", { kind: "failed", error: "x" }));
  assert.equal(hostForConsole(m, "t1")?.contractId, "steward-t1-200");
  m = upsertHost(m, st("steward-t1-050", "t1", running));
  assert.equal(hostForConsole(m, "t1")?.contractId, "steward-t1-050");
  assert.equal(hostForConsole(m, "t2"), null);
});
