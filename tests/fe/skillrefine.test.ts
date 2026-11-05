/**
 * 「整理为技能」前端纯逻辑自测：`lib/skillRefine`。跑法：`npm test`
 *
 * 钉住：主机条目规则与后端 `skill_refine::host_entry` 一致、「直接编辑」的机械草稿
 * 来源 / 范围 / 风险、结果标题、列表去重。
 */

import assert from "node:assert/strict";
import test from "node:test";

import type { RecordingDraft } from "../../src/lib/recorder";
import { cleanLines, hostEntry, hostScopeFor, mechanicalInput, refineHeadline, type RefinedSkillDraft } from "../../src/lib/skillRefine";
import type { SkillInput } from "../../src/types";

function draft(label: string, risk: RecordingDraft["risk"] = "low"): RecordingDraft {
  return {
    session: "s1",
    host: { sessionId: "s1", label },
    startedAtMs: 0,
    endedAtMs: 1000,
    source: "recorded",
    steps: [{ kind: "exec", command: "ls" }, { kind: "secret", reason: "sudo 密码" }],
    hints: [],
    risk,
    warnings: [],
    notes: [],
    stats: { events: 2, droppedNoise: 0, merged: 0 },
  };
}

test("hostEntry：取第一个词、去 user@，不可用则 null", () => {
  assert.equal(hostEntry("web-1"), "web-1");
  assert.equal(hostEntry("root@web-1 (ssh)"), "web-1");
  assert.equal(hostEntry("  "), null);
  assert.equal(hostEntry("root@"), null);
  assert.equal(hostEntry("[REDACTED]"), null);
  assert.equal(hostEntry("x".repeat(256)), null);
  assert.equal(hostEntry("x".repeat(255)), "x".repeat(255));
  assert.equal(hostEntry("a\u0001b"), null);
});

test("hostScopeFor：有主机名限定到它，没有才不限", () => {
  assert.deepEqual(hostScopeFor(draft("root@db1")), { kind: "list", hosts: ["db1"] });
  assert.deepEqual(hostScopeFor(draft("")), { kind: "any" });
});

test("mechanicalInput：来源 recorded、风险沿用、步骤原样、无参数与验证", () => {
  const d = draft("root@db1", "medium");
  const m = mechanicalInput(d);
  assert.equal(m.source, "recorded");
  assert.equal(m.risk, "medium");
  assert.equal(m.verify, null);
  assert.deepEqual(m.params, []);
  assert.deepEqual(m.steps, d.steps);
  assert.deepEqual(m.hostScope, { kind: "list", hosts: ["db1"] });
  assert.ok(m.name.includes("db1"));
  assert.ok(m.name.length <= 80);
  assert.equal(mechanicalInput(draft("")).name, "录制的操作");
});

test("mechanicalInput：超长主机名也不会让名称超限", () => {
  assert.ok([...mechanicalInput(draft("h".repeat(255))).name].length <= 80);
});

function refined(over: Partial<RefinedSkillDraft> = {}, input: Partial<SkillInput> = {}): RefinedSkillDraft {
  return {
    draft: { ...mechanicalInput(draft("db1")), ...input },
    notes: [],
    warnings: [],
    fallback: false,
    attempts: 1,
    ...over,
  };
}

test("refineHeadline：回退 / 成功 / 重试", () => {
  assert.ok(refineHeadline(refined({ fallback: true })).includes("退回"));
  const ok = refineHeadline(refined());
  assert.ok(ok.includes("2 步") && ok.includes("0 个参数"));
  assert.ok(!ok.includes("验证") && !ok.includes("重试"));
  const full = refineHeadline(refined({ attempts: 2 }, { verify: { expect: "ok" } }));
  assert.ok(full.includes("含验证") && full.includes("重试"));
});

test("cleanLines：去空白、去重、保持顺序", () => {
  assert.deepEqual(cleanLines([" a ", "", "b", "a", "  "]), ["a", "b"]);
});
