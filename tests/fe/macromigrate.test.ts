/**
 * 旧宏迁移前端小工具的自测：`lib/macroMigrate`。跑法：`npm test`
 * 转换本身（口令启发式等）由后端 `agent::macro_migrate` 的 Rust 测试覆盖。
 */

import assert from "node:assert/strict";
import test from "node:test";

import type { MacroMigrationPreview, MacroMigrationResult } from "../../src/types";
import { countMigratable, countWarned, summarizeMigration } from "../../src/lib/macroMigrate";

const preview = (over: Partial<MacroMigrationPreview>): MacroMigrationPreview => ({
  macroId: "m",
  macroName: "n",
  draft: null,
  skipReason: null,
  errors: [],
  warnings: [],
  ...over,
});

test("countMigratable / countWarned：只数有草稿的、有警告的", () => {
  const draft = {
    name: "x",
    description: "",
    params: [],
    steps: [],
    verify: null,
    risk: "low" as const,
    hostScope: { kind: "any" as const },
  };
  const ps = [
    preview({ draft, warnings: ["w"] }),
    preview({ draft }),
    preview({ skipReason: "空" }),
    preview({ errors: ["e"], warnings: ["w"] }),
  ];
  assert.equal(countMigratable(ps), 2);
  assert.equal(countWarned(ps), 2);
  assert.equal(countMigratable([]), 0);
});

test("summarizeMigration：分三类计数", () => {
  const r = (outcome: MacroMigrationResult["outcome"]): MacroMigrationResult => ({
    macroId: "m",
    macroName: "n",
    outcome,
    warnings: [],
  });
  assert.equal(
    summarizeMigration([
      r({ kind: "migrated", skillId: "a" }),
      r({ kind: "migrated", skillId: "b" }),
      r({ kind: "skipped", reason: "x" }),
      r({ kind: "failed", errors: ["e"] }),
    ]),
    "已迁移 2 条，跳过 1 条，失败 1 条"
  );
  assert.equal(summarizeMigration([]), "已迁移 0 条，跳过 0 条，失败 0 条");
});
