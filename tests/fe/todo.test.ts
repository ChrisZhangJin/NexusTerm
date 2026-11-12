/**
 * `lib/todo`：计划清单的勾选符号、进度与当前项。
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import { todoView } from "../../src/lib/todo";

test("勾选符号、进度、当前项", () => {
  const v = todoView([
    { text: "装依赖", status: "done" },
    { text: "跑测试", status: "in_progress" },
    { text: "发版", status: "pending" },
  ]);
  assert.deepEqual(
    v.lines.map((l) => l.mark + l.text),
    ["☑装依赖", "▶跑测试", "☐发版"]
  );
  assert.equal(v.done, 1);
  assert.equal(v.total, 3);
  assert.equal(v.current, "跑测试");
});

test("没有在做的就取第一项未做；空清单", () => {
  assert.equal(todoView([{ text: "a", status: "done" }, { text: "b", status: "pending" }]).current, "b");
  assert.deepEqual(todoView(undefined), { lines: [], done: 0, total: 0, current: null });
});
