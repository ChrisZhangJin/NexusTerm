/**
 * `lib/diff`：文件工具差异预览的逐行分类。
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import { diffStat, parseDiff } from "../../src/lib/diff";

test("按行首分类：文件头 / 区块头 / 增 / 删 / 上下文", () => {
  const lines = parseDiff("--- a/conf.ini\n+++ b/conf.ini\n@@ -1,2 +1,2 @@\n a=1\n-b=2\n+b=3\n");
  assert.deepEqual(
    lines.map((l) => l.kind),
    ["meta", "meta", "hunk", "ctx", "del", "add"]
  );
  assert.deepEqual(diffStat(lines), { added: 1, removed: 1 });
});

test("空串与尾部空行", () => {
  assert.deepEqual(parseDiff(""), []);
  assert.equal(parseDiff("+x\n\n\n").length, 1);
});
