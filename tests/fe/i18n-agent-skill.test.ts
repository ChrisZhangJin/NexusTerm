/**
 * 设置 / Agent 对话 / 技能 / 宏 / 录制这些纯函数切到英文时的文案。默认（中文）由各自的用例覆盖。
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import { __setLocaleForTest } from "../../src/i18n";
import { pauseBannerText, STEWARD_DURATION_OPTIONS } from "../../src/lib/agentBudget";
import { emptyAnswer, noConclusion } from "../../src/lib/agentRun";
import { interruptedNote } from "../../src/lib/agentHistory";
import { describeStep as describeMacroStep } from "../../src/lib/macro";
import { summarizeMigration } from "../../src/lib/macroMigrate";
import { summarizeDraft, type RecordingDraft } from "../../src/lib/recorder";
import { RISK_LABEL, STEP_KIND_LABEL, describeSkillStep, trustBadge } from "../../src/lib/skill";
import { describeHostScope, formatLastRun, groupProblems, SOURCE_LABEL } from "../../src/lib/skillForm";
import { fmtElapsed, usageTooltip } from "../../src/lib/usage";

function inEnglish(f: () => void) {
  __setLocaleForTest("en");
  try {
    f();
  } finally {
    __setLocaleForTest("zh");
  }
}

test("预算 / 对话收尾文案：英文", () => {
  inEnglish(() => {
    assert.match(pauseBannerText("steps"), /^Step limit reached/);
    assert.equal(STEWARD_DURATION_OPTIONS[0].label, "2 hours");
    assert.equal(emptyAnswer(), "(No content returned)");
    assert.match(noConclusion(), /^\(No conclusion/);
    assert.match(interruptedNote(), /Interrupted/);
  });
  // 表项是 getter：切回中文立刻跟着变
  assert.equal(STEWARD_DURATION_OPTIONS[0].label, "2 小时");
});

test("用量：英文的时长与悬停说明", () => {
  inEnglish(() => {
    assert.equal(fmtElapsed(45), "45s");
    assert.equal(fmtElapsed(3900), "1h05m");
    const tip = usageTooltip({ inputTokens: 1000, outputTokens: 20, costUsd: null, elapsedSecs: 90 });
    assert.match(tip, /^Input 1(\.0)?k tok · Output 20 tok/);
    assert.match(tip, /No price set/);
    assert.match(tip, /Time 1m$/);
  });
});

test("宏 / 迁移 / 录制摘要：英文", () => {
  inEnglish(() => {
    assert.equal(describeMacroStep({ kind: "sleep", ms: 1500 }), "Wait 1.5s");
    assert.equal(summarizeMigration([]), "Migrated 0, skipped 0, failed 0");
    const d = {
      steps: [{ kind: "exec", command: "ls" }],
      warnings: ["a", "b"],
      risk: "high",
    } as unknown as RecordingDraft;
    assert.equal(summarizeDraft(d), "1 step · High risk · 2 warnings");
  });
});

test("技能：标签表、步骤描述、信任徽标、主机范围、上次运行", () => {
  inEnglish(() => {
    assert.equal(RISK_LABEL.medium, "Medium risk");
    assert.equal(STEP_KIND_LABEL.exec, "Run command");
    assert.equal(SOURCE_LABEL.recorded, "Recorded");
    assert.equal(describeSkillStep({ kind: "screen_check", expect: "OK" } as never), "Screen should show “OK”");
    assert.equal(trustBadge(undefined).label, "Confirm each step");
    assert.equal(describeHostScope({ kind: "list", hosts: ["a", "b", "c"] }), "a, b and 1 more");
    assert.equal(formatLastRun(null), "Never run");
    assert.equal(formatLastRun(1000, 1000 * 1000 + 120_000), "2 min ago");
  });
  assert.equal(RISK_LABEL.medium, "中风险");
});

test("后端校验问题的归类：中英文前缀都认", () => {
  const g = groupProblems([
    "第 2 步（exec）：命令为空",
    "Step 3 (keys): empty",
    "参数「host」重复",
    "Parameter name “port” is invalid",
    "secret parameter \"pw\" cannot have a default",
    "风险等级 Low 低于自动评估的 High",
    "Risk level Low is below the assessed High",
  ]);
  assert.deepEqual([...g.steps.keys()], [1, 2]);
  assert.deepEqual([...g.params.keys()], ["host", "port", "pw"]);
  assert.equal(g.risk.length, 2);
  assert.equal(g.general.length, 0);
});
