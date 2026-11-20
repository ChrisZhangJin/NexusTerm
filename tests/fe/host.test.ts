/**
 * `lib/host` 的回归测试：相位 → 胶囊、卡片与按钮。
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  basePhase,
  hostDetail,
  hostForConsole,
  hostPill,
  hostTabBadge,
  hostTabBadges,
  pendingCard,
  upsertHost,
} from "../../src/lib/host";
import { hostHeadActions } from "../../src/lib/hostUi";
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
  assert.deepEqual(hostHeadActions(p).map((a) => a.kind), ["handback", "stop"]);
  assert.match(hostDetail(p) ?? "", /交还后回到「等你回答」/);
});

test("终态没有可点的按钮", () => {
  assert.deepEqual(hostHeadActions({ kind: "stopped" }), []);
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
  assert.equal(hostPill(p).label, "终端断开·等重连");
  assert.equal(needsReattach(p), true);
  // 卡头不给按钮：接驳卡里有候选 / 打开新连接 / 放弃
  assert.deepEqual(hostHeadActions(p), []);
  assert.equal(needsReattach({ kind: "running" }), false);
});

import { addDeny, checkLines, denyLabel, formatReport, isLockedDeny, removeDeny } from "../../src/lib/host";

test("完成卡逐条证据与新卡片胶囊", () => {
  const lines = checkLines([
    { check: { kind: "commandOk", cmd: "grep -q ok /tmp/r" }, passed: true, detail: "退出码 0" },
    { check: { kind: "manual", text: "页面能打开" }, passed: false, detail: "未自动核验（仅供参考，不阻塞完成）" },
  ]);
  // 没法核验的命令判据同样标「·」（终端前台是 Claude Code、又没有旁路通道）
  assert.equal(
    checkLines([{ check: { kind: "commandOk", cmd: "git status" }, passed: false, skipped: true, detail: "未核验：终端前台是 Claude Code" }])[0],
    "· 命令成功：`git status`：未核验：终端前台是 Claude Code"
  );
  // 人工项不阻塞完成：标「·」而不是「✗」，免得完成了却像失败
  assert.deepEqual(lines, [
    "✓ 命令成功：`grep -q ok /tmp/r`：退出码 0",
    "· 人工确认：页面能打开：未自动核验（仅供参考，不阻塞完成）",
  ]);
  const mk = (kind: "manualCheck" | "watchdog"): HostPhase => ({
    kind: "waitingHuman",
    card: {
      id: "x",
      kind: kind === "watchdog" ? { kind } : { kind, summary: "s", checks: [] },
      question: "q",
      options: [],
      reasons: [],
      createdWallMs: 0,
    },
  });
  assert.equal(hostPill(mk("manualCheck")).label, "等你确认完成");
  assert.equal(hostPill(mk("watchdog")).label, "看门狗·等你看看");
});

test("完成报告：模型挤成一段时按编号与「建议」分行；已分行的原样保留", () => {
  const squashed = "磁盘检查完成，没有删除任何文件。可清理清单： 1. Docker 镜像约 44G。 2. target 目录 19G； 3. /tmp 14G。建议先做 1 到 4 项。";
  assert.equal(
    formatReport(squashed),
    "磁盘检查完成，没有删除任何文件。可清理清单：\n1. Docker 镜像约 44G。\n2. target 目录 19G；\n3. /tmp 14G。\n建议先做 1 到 4 项。"
  );
  const lined = "已完成\n- 第一点 1. 不拆\n建议：无";
  assert.equal(formatReport(lined), lined);
  assert.equal(formatReport("  "), "");
  // 版本号、小数不能被当成编号拆开
  assert.equal(formatReport("升级到 v1.2 后占用 3.5G"), "升级到 v1.2 后占用 3.5G");
});

test("禁止项：底线项去不掉；放开 delete；加关键字去重", () => {
  const base = ["secret", "permanent_grant", "force_push", "delete"];
  assert.deepEqual(removeDeny(base, "delete"), ["secret", "permanent_grant", "force_push"]);
  assert.deepEqual(removeDeny(base, "secret"), base, "底线项去不掉");
  assert.deepEqual(addDeny(["delete"], " systemctl "), ["secret", "permanent_grant", "delete", "systemctl"]);
  assert.deepEqual(addDeny(base, "DELETE"), base, "不分大小写去重");
  assert.ok(isLockedDeny("permanent_grant") && !isLockedDeny("delete"));
  assert.match(denyLabel("delete"), /删除/);
  assert.equal(denyLabel("systemctl"), "含「systemctl」");
});

import { criteriaSummary } from "../../src/lib/host";

test("criteriaSummary：按固定顺序计数，只列出现过的类；认不出前缀的算人工", () => {
  const s = criteriaSummary("屏幕: done\n命令: ls\n\n命令: true\n随便一句话");
  assert.deepEqual(s, [
    { label: "命令", n: 2, blocking: true },
    { label: "屏幕", n: 1, blocking: true },
    { label: "人工", n: 1, blocking: false },
  ]);
  assert.deepEqual(criteriaSummary("  \n"), []);
});

import { inlineParts, textBlocks, unescapeNewlines } from "../../src/lib/host";

test("textBlocks：字面 \\n 还原，空行分段，连续 - / 1. 行成列表", () => {
  const q = "服务端已就绪，请完成 S:5。\\n\\n本机 ufw 是 inactive。放行：\\n- TCP：443、2053\\n- UDP：8443\\n\\n结果如何？";
  assert.deepEqual(textBlocks(q), [
    { kind: "p", text: "服务端已就绪，请完成 S:5。" },
    { kind: "p", text: "本机 ufw 是 inactive。放行：" },
    { kind: "ul", items: ["TCP：443、2053", "UDP：8443"] },
    { kind: "p", text: "结果如何？" },
  ]);
  assert.deepEqual(textBlocks("1. 甲\n2. 乙"), [{ kind: "ol", items: ["甲", "乙"] }]);
  assert.equal(unescapeNewlines("a\nb \\n"), "a\nb \\n", "有真换行就不动");
});

test("inlineParts：反引号与路径显示成代码", () => {
  assert.deepEqual(inlineParts("见 /root/proxy-nodes.txt（权限 `600`）"), [
    { code: false, text: "见 " },
    { code: true, text: "/root/proxy-nodes.txt" },
    { code: false, text: "（权限 " },
    { code: true, text: "600" },
    { code: false, text: "）" },
  ]);
  assert.deepEqual(inlineParts("TCP/UDP 都行"), [{ code: false, text: "TCP/UDP 都行" }]);
});

test("hostTabBadge：等人时给一行「等你：…」，不等人时 null", () => {
  assert.equal(hostTabBadge(waiting), "等你：部署到哪？");
  assert.equal(hostTabBadge({ kind: "waitingHuman", card: card("approve") }), "等你批准：部署到哪？");
  assert.equal(
    hostTabBadge({ kind: "waitingHuman", card: { ...card(), kind: { kind: "criteriaDraft", draft: [] } } }),
    "等你确认判据"
  );
  assert.equal(
    hostTabBadge({ kind: "waitingHuman", card: { ...card(), kind: { kind: "watchdog" } } }),
    "看门狗·等你看看：部署到哪？"
  );
  assert.equal(hostTabBadge(budget), "等你：预算暂停");
  assert.equal(hostTabBadge({ kind: "paused", cause: { kind: "user" } }), null);
  assert.equal(
    hostTabBadge({ kind: "suspended", reason: "consoleGone", resume_to: running }),
    "等你：终端断开·等重连"
  );
  // 人在驾驶：人就在终端前，不亮
  assert.equal(hostTabBadge({ kind: "userDriving", resume_to: waiting }), null);
  assert.equal(hostTabBadge(running), null);
  assert.equal(hostTabBadge({ kind: "completed", summary: "好了", checks: [] }), null);
  assert.equal(hostTabBadge({ kind: "stopped" }), null);
});

test("hostTabBadge：问题压成一行并截短", () => {
  const long: HostPhase = { kind: "waitingHuman", card: { ...card(), question: "第一行\n" + "长".repeat(100) } };
  const b = hostTabBadge(long)!;
  assert.ok(!b.includes("\n"));
  assert.ok(b.startsWith("等你：第一行 长"));
  assert.ok(b.endsWith("…"));
  assert.equal(Array.from(b).length, "等你：".length + 60 + 1);
});

test("hostTabBadges：只收正在等人的，按 console 归", () => {
  const st = (id: string, consoleId: string, phase: HostPhase) => ({ contractId: id, consoleId, phase }) as HostStatus;
  const m = upsertHost(upsertHost(upsertHost({}, st("a", "c1", waiting)), st("b", "c2", running)), st("c", "c3", budget));
  assert.deepEqual(hostTabBadges(m), { c1: "等你：部署到哪？", c3: "等你：预算暂停" });
});

// ———————————— 门禁卡按钮 ————————————

import { gateActions } from "../../src/lib/host";
import { withStep, EMPTY_CONV } from "../../src/lib/agentRun";
import type { AgentStep } from "../../src/types";

const gate = (over: Partial<AgentStep>): AgentStep => ({ kind: "gate", step: 1, gateVerdict: "refused", ...over });

test("门禁卡：底线项（替你输密码）只给「这是误判」，不给放开，并带说明", () => {
  const a = gateActions(gate({ denyItem: "secret", denyLocked: true, gateId: "g1" }), ["secret", "permanent_grant", "delete"]);
  assert.equal(a.misread, true);
  assert.equal(a.release, null);
  assert.equal(a.item?.label, "替你输密码");
  assert.match(a.item?.hint ?? "", /不能放开/);
});

test("门禁卡：可放开的禁止项给出去掉它之后的列表；不在当前契约里（已放开）就不给", () => {
  const deny = ["secret", "permanent_grant", "delete", "PROD"];
  assert.deepEqual(gateActions(gate({ denyItem: "delete" }), deny).release, ["secret", "permanent_grant", "PROD"]);
  assert.deepEqual(gateActions(gate({ denyItem: "PROD" }), deny).release, ["secret", "permanent_grant", "delete"]);
  assert.equal(gateActions(gate({ denyItem: "force_push" }), deny).release, null);
  // 没看屏幕（命令 / 写文件）就没有「这是误判」
  assert.equal(gateActions(gate({ denyItem: "delete" }), deny).misread, false);
});

test("门禁卡：要批准 / 通知的卡不给放开；通知卡没有误判按钮", () => {
  const deny = ["secret", "permanent_grant", "force_push"];
  const asked = gateActions(gate({ gateVerdict: "asked", denyItem: "force_push", gateId: "g" }), deny);
  assert.equal(asked.release, null);
  assert.equal(asked.misread, true);
  const n = gateActions(gate({ gateVerdict: "notified", gateId: "g" }), deny);
  assert.deepEqual(n, { misread: false, release: null, item: null });
});

test("门禁步骤在实况里单独成卡", () => {
  const c = withStep(EMPTY_CONV, gate({ command: "rm -rf build", denyItem: "delete" }));
  assert.equal(c.messages.length, 1);
  assert.equal(c.messages[0].step?.kind, "gate");
});
