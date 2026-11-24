/**
 * 托管相关纯函数的英文界面：切到 en 后文案跟着变、解析两种语言都认。
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import { __setLocaleForTest } from "../../src/i18n";
import { checkToLine, describeCheck, hostPill, hostTabBadge, textToChecks, denyLabel } from "../../src/lib/host";
import { hostHeadActions, hostSettingsSummary, HOST_PRESETS } from "../../src/lib/hostUi";
import { checkDraft, isGoneError, KIND_LABEL, usageLabel } from "../../src/lib/hostMemory";
import { fmtDuration, historyStateLabel } from "../../src/lib/hostHistory";
import { incompleteNote, parseDiff } from "../../src/lib/diff";
import { TIER_LABELS, tierLabel } from "../../src/lib/steward";
import type { HostPhase } from "../../src/types";

function inEnglish(fn: () => void) {
  __setLocaleForTest("en");
  try {
    fn();
  } finally {
    __setLocaleForTest("zh");
  }
}

const waiting = (kind: "approve" | "ask"): HostPhase =>
  ({
    kind: "waitingHuman",
    card: { id: "x", kind: { kind }, question: "Proceed?", options: [], reasons: [], createdWallMs: 0 },
  }) as unknown as HostPhase;

test("英文：胶囊与标签徽标（「等你」前缀按 waiting 标记判断，不看文字）", () => {
  inEnglish(() => {
    assert.equal(hostPill({ kind: "running" } as HostPhase).label, "Running");
    assert.equal(hostPill(waiting("approve")).label, "Waiting for your approval");
    assert.equal(hostTabBadge(waiting("approve")), "Waiting for your approval: Proceed?");
    assert.equal(hostTabBadge(waiting("ask")), "Waiting for you: Proceed?");
    const paused = { kind: "paused", cause: { kind: "llmDown", error: "x" } } as unknown as HostPhase;
    assert.equal(hostTabBadge(paused), "Waiting for you: Paused (model unavailable)");
  });
  assert.equal(hostTabBadge(waiting("approve")), "等你批准：Proceed?");
});

test("英文：判据行前缀；两种语言的前缀都能解析", () => {
  inEnglish(() => {
    assert.equal(checkToLine({ kind: "commandOk", cmd: "make test" }), "Command: make test");
    assert.equal(describeCheck({ kind: "fileExists", path: "/a" }), "File exists: /a");
  });
  assert.deepEqual(textToChecks("Command: ls\nscreen: ok$\n文件: /etc/x\n人工：看一眼"), [
    { kind: "commandOk", cmd: "ls" },
    { kind: "screenMatches", re: "ok$" },
    { kind: "fileExists", path: "/etc/x" },
    { kind: "manual", text: "看一眼" },
  ]);
});

test("英文：卡头按钮、设置摘要、预设、档位与禁止项", () => {
  inEnglish(() => {
    assert.deepEqual(
      hostHeadActions({ kind: "running" } as HostPhase).map((a) => a.label),
      ["Pause", "Take over", "Stop"]
    );
    assert.deepEqual(hostSettingsSummary("auto", ["secret", "delete", "force_push"]), {
      tier: "Auto",
      deny: "Blocked: delete, force push",
      yolo: false,
    });
    assert.deepEqual(
      HOST_PRESETS.map((p) => p.label),
      ["Careful", "Auto", "YOLO"]
    );
    assert.equal(TIER_LABELS.approve, "Ask first");
    assert.equal(tierLabel("notify"), "Notify");
    assert.equal(denyLabel("foo"), 'Contains "foo"');
  });
  assert.equal(HOST_PRESETS[0].label, "稳妥");
  assert.equal(TIER_LABELS.approve, "需确认后执行");
});

test("英文：主机备忘与历史托管的文案；isGoneError 认两种语言", () => {
  inEnglish(() => {
    assert.equal(KIND_LABEL.path, "Paths");
    assert.equal(usageLabel(3, 50), "3 / 50 notes");
    assert.deepEqual(checkDraft("  ", 10), { ok: false, error: "Note can't be empty" });
    assert.equal(fmtDuration(72_000), "1m 12s");
    assert.equal(historyStateLabel("failed").label, "Failed");
  });
  assert.equal(isGoneError("备忘 abc 不存在"), true);
  assert.equal(isGoneError("Note abc not found"), true);
  assert.equal(isGoneError("permission denied"), false);
});

test("diff：英文的「预览不完整」标记也认", () => {
  const d = "⚠ Preview incomplete: 3000 lines, showing first 2000\n@@ -1 +1 @@\n-a\n+b\n…(diff too long, 1000 lines omitted)";
  assert.equal(incompleteNote(d), "⚠ Preview incomplete: 3000 lines, showing first 2000");
  const kinds = parseDiff(d).map((l) => l.kind);
  assert.deepEqual(kinds, ["note", "hunk", "del", "add", "note"]);
});
