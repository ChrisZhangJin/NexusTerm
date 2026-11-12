/**
 * `lib/diff`：文件工具差异预览的逐行分类。
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import { diffStat, incompleteNote, INCOMPLETE_MARK, parseDiff } from "../../src/lib/diff";

test("按行首分类：文件头 / 区块头 / 增 / 删 / 上下文", () => {
  const lines = parseDiff("--- a/conf.ini\n+++ b/conf.ini\n@@ -1,2 +1,2 @@\n a=1\n-b=2\n+b=3\n");
  assert.deepEqual(
    lines.map((l) => l.kind),
    ["meta", "meta", "hunk", "ctx", "del", "add"]
  );
  assert.deepEqual(diffStat(lines), { added: 1, removed: 1, hunks: 1 });
});

test("空串与尾部空行", () => {
  assert.deepEqual(parseDiff(""), []);
  assert.equal(parseDiff("+x\n\n\n").length, 1);
});

test("多区块：每个 @@ 都是 hunk，区块之后的 `---x` 是删行而不是文件头", () => {
  const d = "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n a\n-b\n+B\n@@ -40,2 +40,2 @@\n x\n---y\n+++z\n";
  const lines = parseDiff(d);
  assert.deepEqual(
    lines.map((l) => l.kind),
    ["meta", "meta", "hunk", "ctx", "del", "add", "hunk", "ctx", "del", "add"]
  );
  assert.deepEqual(diffStat(lines), { added: 2, removed: 2, hunks: 2 });
});

test("预览不完整：识别首行标记，省略说明归为 note", () => {
  const d = `${INCOMPLETE_MARK}：差异共 3000 行，只显示前 2000 行\n--- a/f\n+++ b/f\n@@ -0,0 +1,3000 @@\n+1\n…（差异过长，省略 1000 行）`;
  assert.ok(incompleteNote(d)?.includes("只显示前 2000 行"));
  const kinds = parseDiff(d).map((l) => l.kind);
  assert.equal(kinds[0], "note");
  assert.equal(kinds[kinds.length - 1], "note");
  assert.equal(incompleteNote("--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b"), null);
});

import { needsIncompleteBanner } from "../../src/lib/diff";

test("审批卡的「预览不完整」横幅：后端说不完整且 diff 里没自带说明时才另放", () => {
  assert.equal(needsIncompleteBanner(false, ""), true);
  assert.equal(needsIncompleteBanner(false, undefined), true);
  assert.equal(needsIncompleteBanner(false, `${INCOMPLETE_MARK}：差异过长，只显示前 3 处\n@@ -1 +1 @@\n-a\n+b`), false);
  assert.equal(needsIncompleteBanner(true, ""), false);
  assert.equal(needsIncompleteBanner(undefined, ""), false);
});
