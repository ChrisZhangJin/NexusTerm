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
  assert.deepEqual(b, { takeover: false, handback: false, resume: false, pause: false, stop: false, reattach: false });
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

import { checksToText, textToChecks } from "../../src/lib/host";

test("判据草稿：一行一条，往返不变；认不出的当人工确认", () => {
  const cs = [
    { kind: "commandOk" as const, cmd: "cargo test" },
    { kind: "fileExists" as const, path: "/srv/app/dist" },
    { kind: "screenMatches" as const, re: "All \\d+ passed" },
    { kind: "manual" as const, text: "由你确认完成" },
  ];
  assert.deepEqual(textToChecks(checksToText(cs)), cs);
  assert.deepEqual(textToChecks("  \n页面能打开\n命令：  \n文件：/a "), [
    { kind: "manual", text: "页面能打开" },
    { kind: "fileExists", path: "/a" },
  ]);
});

test("等密码 / 等确认判据有各自的胶囊", () => {
  const mk = (kind: "secret" | "criteriaDraft"): HostPhase => ({
    kind: "waitingHuman",
    card: {
      id: "x",
      kind: kind === "secret" ? { kind } : { kind, draft: [] },
      question: "q",
      options: [],
      reasons: [],
      createdWallMs: 0,
    },
  });
  assert.equal(hostPill(mk("secret")).label, "等你输入密码");
  assert.equal(hostPill(mk("criteriaDraft")).label, "等你确认判据");
});

import { consoleHintOf, needsReattach } from "../../src/lib/host";

test("ConsoleRef 线索：SSH 带记录 id、本地不带；cwd / tmux 取探测到的上下文", () => {
  const ssh = consoleHintOf({
    id: "tab-1",
    title: "root@web1",
    customTitle: "  ",
    sessionId: "rec-web1",
    status: "connected",
    ctx: { cwd: "/srv/app", tmux: "main", at: 1 },
  });
  assert.deepEqual(ssh, { consoleId: "tab-1", label: "root@web1", sessionRecordId: "rec-web1", cwd: "/srv/app", tmux: "main" });
  const local = consoleHintOf({ id: "tab-2", title: "", customTitle: "构建机", sessionId: "x", status: "connected", local: true });
  assert.deepEqual(local, { consoleId: "tab-2", label: "构建机", sessionRecordId: null, cwd: null, tmux: null });
});

test("挂起（重启 / 终端关了）只给接驳与放弃", () => {
  const p: HostPhase = { kind: "suspended", reason: "consoleGone", resume_to: { kind: "running" } };
  assert.equal(hostPill(p).label, "终端已关闭·待接驳");
  assert.equal(needsReattach(p), true);
  const b = hostButtons(p);
  assert.equal(b.reattach, true);
  assert.equal(b.takeover, false);
  assert.equal(b.stop, true);
  assert.equal(needsReattach({ kind: "running" }), false);
});
