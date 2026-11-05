/**
 * 录制器（3.2）前端纯逻辑的回归测试：`lib/recorder`（时长 / 按键映射 / 草稿文案）
 * 与 `lib/termLine`（从屏幕读回整行、录制时补报哪些操作）。
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  describeStep,
  draftDurationMs,
  formatElapsed,
  hintFor,
  idleStatus,
  specialKeyFor,
  statusEqual,
  summarizeDraft,
  type RecordingDraft,
} from "../../src/lib/recorder";
import {
  captureLineStart,
  readLogicalLine,
  recorderTapAction,
  type BufLike,
  type LineLike,
} from "../../src/lib/termLine";

// ——— lib/recorder ———

test("formatElapsed：m:ss / h:mm:ss，未开始与时钟回拨按 0", () => {
  assert.equal(formatElapsed(undefined, 5000), "0:00");
  assert.equal(formatElapsed(1000, 1000), "0:00");
  assert.equal(formatElapsed(1000, 66_000), "1:05");
  assert.equal(formatElapsed(0, 3_725_000), "1:02:05");
  assert.equal(formatElapsed(10_000, 1000), "0:00");
});

test("specialKeyFor：只认整段恰为 Ctrl-C / D / Z", () => {
  assert.equal(specialKeyFor("\x03"), "<C-c>");
  assert.equal(specialKeyFor("\x04"), "<C-d>");
  assert.equal(specialKeyFor("\x1a"), "<C-z>");
  assert.equal(specialKeyFor("\x03\x03"), null);
  assert.equal(specialKeyFor("c"), null);
  assert.equal(specialKeyFor("\r"), null);
  assert.equal(specialKeyFor("\x1b[A"), null);
  assert.equal(specialKeyFor(""), null);
});

const draft = (over: Partial<RecordingDraft> = {}): RecordingDraft => ({
  session: "s1",
  host: { sessionId: "s1", label: "prod" },
  startedAtMs: 1000,
  endedAtMs: 61_000,
  source: "recorded",
  steps: [
    { kind: "exec", command: "ls -la" },
    { kind: "secret", reason: "提示「Password:」要求输入密码 / 口令" },
    { kind: "expect_respond", expect: "Continue? [y/N]", respond: "y<Enter>", secret: false },
    { kind: "keys", keys: "<C-c>" },
  ],
  hints: [{ stepIndex: 0, outputTail: "total 8" }],
  risk: "low",
  warnings: [],
  notes: [],
  stats: { events: 5, droppedNoise: 0, merged: 0 },
  ...over,
});

test("describeStep：每种步骤一行描述，密码步骤没有任何内容", () => {
  const d = draft();
  assert.equal(describeStep(d.steps[0]), "ls -la");
  assert.match(describeStep(d.steps[1]), /输入密码/);
  assert.match(describeStep(d.steps[2]), /Continue\? \[y\/N\].*y<Enter>/);
  assert.equal(describeStep(d.steps[3]), "按键 <C-c>");
  assert.equal(
    describeStep({ kind: "expect_respond", expect: "pw:", respond: "", secret: true }),
    "等「pw:」→ 回答 （密码）"
  );
  assert.equal(describeStep({ kind: "screen_check", expect: "ok" }), "检查屏幕「ok」");
});

test("hintFor / summarizeDraft / draftDurationMs", () => {
  const d = draft({ risk: "high", warnings: ["a", "b"] });
  assert.equal(hintFor(d, 0)?.outputTail, "total 8");
  assert.equal(hintFor(d, 1), undefined);
  assert.equal(summarizeDraft(d), "4 步 · 风险高 · 2 条提醒");
  assert.equal(summarizeDraft(draft()), "4 步 · 风险低");
  assert.equal(draftDurationMs(d), 60_000);
  assert.equal(draftDurationMs(draft({ endedAtMs: 0 })), 0);
});

test("statusEqual / idleStatus", () => {
  const a = idleStatus("s");
  assert.equal(a.recording, false);
  assert.ok(statusEqual(a, idleStatus("s")));
  assert.ok(!statusEqual(a, { ...a, recording: true, startedAtMs: 1 }));
  assert.ok(!statusEqual({ ...a, recording: true, startedAtMs: 1 }, { ...a, recording: true, startedAtMs: 2 }));
});

// ——— lib/termLine ———

/** 造一个假的 xterm 缓冲：每行 `[文字, 是否折行续行]`，固定列宽 */
function fakeBuf(
  rows: [string, boolean][],
  cursor: { x: number; y: number },
  baseY = 0,
  cols = 20
): BufLike {
  const lines: LineLike[] = rows.map(([text, wrapped]) => ({
    isWrapped: wrapped,
    translateToString(_trim?: boolean, s = 0, e = cols) {
      return text.padEnd(cols).slice(s, e);
    },
  }));
  return {
    cursorX: cursor.x,
    cursorY: cursor.y,
    baseY,
    getLine: (y) => lines[y],
  };
}

test("readLogicalLine：读回提示符之后、光标之前的整行（历史回溯场景）", () => {
  // `$ ` 之后是 ↑ 回溯出来的 `git pull`，光标在行尾
  const buf = fakeBuf([["$ git pull", false]], { x: 10, y: 0 });
  const start = { x: 2, y: 0 };
  assert.equal(readLogicalLine(buf, start), "git pull");
});

test("readLogicalLine：跨自动折行的长命令", () => {
  const buf = fakeBuf(
    [
      ["$ echo aaaaaaaaaaaaa", false], // 恰好 20 列
      ["bbbbb", true],
    ],
    { x: 5, y: 1 }
  );
  assert.equal(readLogicalLine(buf, { x: 2, y: 0 }), "echo aaaaaaaaaaaaabbbbb");
});

test("readLogicalLine：光标后还有内容 / 行不连续 / 起点在光标后 / 无起点 → null", () => {
  // 光标在行中（用户在中间编辑，或有右侧提示）
  assert.equal(readLogicalLine(fakeBuf([["$ git pull", false]], { x: 5, y: 0 }), { x: 2, y: 0 }), null);
  // 下一行不是折行续行：不是同一条输入
  assert.equal(
    readLogicalLine(fakeBuf([["$ ls", false], ["other", false]], { x: 5, y: 1 }), { x: 2, y: 0 }),
    null
  );
  // 起点在光标之后（清屏 / 重绘后起点过期）
  assert.equal(readLogicalLine(fakeBuf([["$ ls", false]], { x: 2, y: 0 }), { x: 4, y: 0 }), null);
  assert.equal(readLogicalLine(fakeBuf([["$ ls", false]], { x: 4, y: 0 }), null), null);
  // 起点所在行已滚出缓冲
  assert.equal(readLogicalLine(fakeBuf([["x", false]], { x: 1, y: 0 }), { x: 0, y: 7 }), null);
});

test("readLogicalLine：空输入（回溯没出结果）读回空串，行尾空白被修剪", () => {
  assert.equal(readLogicalLine(fakeBuf([["$ ", false]], { x: 2, y: 0 }), { x: 2, y: 0 }), "");
  assert.equal(readLogicalLine(fakeBuf([["$ ls  ", false]], { x: 6, y: 0 }), { x: 2, y: 0 }), "ls");
});

test("readLogicalLine / captureLineStart：滚动后用绝对行号（baseY + cursorY）", () => {
  const buf = fakeBuf(
    [
      ["old", false],
      ["old2", false],
      ["$ ls -l", false],
    ],
    { x: 7, y: 0 },
    2
  );
  const start = captureLineStart({ ...buf, cursorX: 2 });
  assert.deepEqual(start, { x: 2, y: 2 });
  assert.equal(readLogicalLine(buf, start), "ls -l");
});

const tap = (o: Partial<Parameters<typeof recorderTapAction>[0]> = {}) =>
  recorderTapAction({
    data: "\r",
    committed: null,
    wasKnown: true,
    altScreen: false,
    recover: () => null,
    ...o,
  });

test("recorderTapAction：Ctrl-C / D / Z 上报按键", () => {
  assert.deepEqual(tap({ data: "\x03" }), { kind: "keys", keys: "<C-c>" });
  assert.deepEqual(tap({ data: "\x04", wasKnown: false }), { kind: "keys", keys: "<C-d>" });
  assert.deepEqual(tap({ data: "\x1a" }), { kind: "keys", keys: "<C-z>" });
});

test("recorderTapAction：行状态机已交出命令时不重复上报", () => {
  let recovered = false;
  const a = tap({
    committed: "ls",
    recover: () => {
      recovered = true;
      return "ls";
    },
  });
  assert.equal(a, null);
  assert.equal(recovered, false);
});

test("recorderTapAction：已知的空行回车 = 空回车按键", () => {
  assert.deepEqual(tap(), { kind: "keys", keys: "<Enter>" });
});

test("recorderTapAction：未知行（↑ / Tab）回车时从屏幕读回命令", () => {
  assert.deepEqual(tap({ wasKnown: false, recover: () => "  git pull " }), { kind: "command", line: "git pull" });
  // 读不到 / 读到空：不上报（后端靠回车计数告警）
  assert.equal(tap({ wasKnown: false, recover: () => null }), null);
  assert.equal(tap({ wasKnown: false, recover: () => "   " }), null);
});

test("recorderTapAction：全屏程序、粘贴 / 带转义的输入、普通字符都不补报", () => {
  assert.equal(tap({ altScreen: true }), null);
  assert.equal(tap({ altScreen: true, data: "\x03" }), null);
  assert.equal(tap({ data: "\x1b[200~ls\r\x1b[201~" }), null);
  assert.equal(tap({ data: "l" }), null);
  assert.equal(tap({ data: "ls\r", wasKnown: false, recover: () => "ls" }), null);
});
