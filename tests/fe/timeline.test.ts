/**
 * `lib/timeline` 的回归测试：事件按 seq 去重合并、output 并回命令成行、贴底判定、secret 不显示内容。
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  MAX_TIMELINE_EVENTS,
  buildRows,
  describeEvent,
  formatClock,
  isNearBottom,
  mergeEvents,
  outputSummary,
  rowEqual,
  type TimelineEvent,
} from "../../src/lib/timeline";

const ev = (seq: number, over: Partial<TimelineEvent> = {}): TimelineEvent => ({
  seq,
  tsMs: seq * 1000,
  actor: "user",
  kind: "command",
  text: `cmd ${seq}`,
  truncated: false,
  ...over,
});

test("mergeEvents：纯追加保持引用，重复与空批次原样返回 prev", () => {
  const a = [ev(1), ev(2)];
  assert.equal(mergeEvents(a, []), a);
  assert.equal(mergeEvents(a, [ev(1), ev(2)]), a);
  const b = mergeEvents(a, [ev(3)]);
  assert.deepEqual(b.map((e) => e.seq), [1, 2, 3]);
  assert.equal(b[0], a[0]); // 已有事件对象引用不变（memo 行才不会重渲染）
});

test("mergeEvents：乱序 / 与首屏拉取重叠时按 seq 去重并排序", () => {
  const a = [ev(3), ev(4)];
  const b = mergeEvents(a, [ev(1), ev(2), ev(4), ev(5)]);
  assert.deepEqual(b.map((e) => e.seq), [1, 2, 3, 4, 5]);
});

test("mergeEvents：超过上限丢最老的", () => {
  const all = Array.from({ length: MAX_TIMELINE_EVENTS + 10 }, (_, i) => ev(i + 1));
  const list = mergeEvents([], all);
  assert.equal(list.length, MAX_TIMELINE_EVENTS);
  assert.equal(list[0].seq, 11);
  assert.equal(list[list.length - 1].seq, MAX_TIMELINE_EVENTS + 10);
});

test("buildRows：output 并回所属命令；找不到所属的单独成行", () => {
  const rows = buildRows([
    ev(1),
    ev(2, { actor: "agent", kind: "command", text: "uptime" }),
    ev(3, { actor: "agent", kind: "output", refSeq: 2, text: "up 3 days" }),
    ev(4, { kind: "output", refSeq: 99, text: "orphan" }),
    ev(5, { kind: "output", text: "no ref" }),
  ]);
  assert.equal(rows.length, 4);
  assert.equal(rows[1].outputs.length, 1);
  assert.equal(rows[1].outputs[0].text, "up 3 days");
  assert.equal(rows[2].ev.seq, 4);
  assert.equal(rows[3].ev.seq, 5);
});

test("rowEqual：同事件同输出才等价", () => {
  const e1 = ev(1);
  const o = ev(2, { kind: "output", refSeq: 1 });
  assert.equal(rowEqual({ ev: e1, outputs: [o] }, { ev: e1, outputs: [o] }), true);
  assert.equal(rowEqual({ ev: e1, outputs: [] }, { ev: e1, outputs: [o] }), false);
  assert.equal(rowEqual({ ev: e1, outputs: [] }, { ev: ev(1), outputs: [] }), false);
});

test("isNearBottom：容差内算贴底，往上翻就不算", () => {
  assert.equal(isNearBottom(1000, 600, 400), true);
  assert.equal(isNearBottom(1000, 580, 400), true); // 差 20
  assert.equal(isNearBottom(1000, 300, 400), false);
});

test("describeEvent：secret 只说明发生过、不含任何内容；prompt 带分类", () => {
  assert.match(describeEvent(ev(1, { kind: "secret", text: "" })), /未记录/);
  assert.match(describeEvent(ev(1, { actor: "agent", kind: "secret", text: "" })), /不经过助手/);
  const p = describeEvent(ev(2, { kind: "prompt", promptKind: "password", text: "Password:" }));
  assert.match(p, /密码提示/);
  assert.match(p, /Password:/);
  assert.equal(describeEvent(ev(3, { kind: "wait", text: "ready" })), "等待 ready");
});

test("outputSummary：统计行数并标注截断", () => {
  const a = ev(1, { kind: "output", text: "a\nb\nc" });
  const b = ev(2, { kind: "output", text: "d", truncated: true });
  assert.equal(outputSummary([a]), "输出 3 行");
  assert.equal(outputSummary([a, b]), "输出 4 行（仅尾部）");
});

test("formatClock：HH:MM:SS（测试 runner 固定 UTC）", () => {
  assert.equal(formatClock(0), "00:00:00");
  assert.equal(formatClock(3_725_000), "01:02:05");
});
