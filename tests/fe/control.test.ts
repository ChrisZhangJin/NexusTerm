/**
 * `lib/control` 的回归测试：键盘闸只在 Agent 驾驶时通知一次、不把终端自动应答当成用户打字。
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  applyControl,
  createControlGate,
  drivingSessions,
  isUserTyping,
  lockedInput,
  pausedSessions,
  type ControlState,
} from "../../src/lib/control";

test("普通字符 / 控制字符 / 粘贴 / 方向键都算用户打字", () => {
  for (const d of ["a", "中", "\r", "\x03", "\x7f", "\t", "ls -la\r", "\x1b", "\x1b[A", "\x1b[B", "\x1bOP", "\x1b[3~", "\x1bb"]) {
    assert.equal(isUserTyping(d), true, JSON.stringify(d));
  }
});

test("空串不算", () => {
  assert.equal(isUserTyping(""), false);
});

test("xterm 对终端查询的自动应答不算用户打字", () => {
  for (const d of [
    "\x1b[12;40R", // CPR
    "\x1b[?12;40R",
    "\x1b[?1;2c", // DA1
    "\x1b[>0;276;0c", // DA2
    "\x1b[0n", // DSR
    "\x1b[?997;1n",
    "\x1b[?2004;2$y", // DECRPM
    "\x1b[I", // 焦点进入
    "\x1b[O", // 焦点离开
    "\x1b]11;rgb:0000/0000/0000\x07", // OSC 颜色应答
    "\x1b]52;c;aGVsbG8=\x1b\\",
    "\x1bP1+r5463\x1b\\", // DCS
    "\x1b[8;32;120t", // 窗口大小
  ]) {
    assert.equal(isUserTyping(d), false, JSON.stringify(d));
  }
});

test("闸关着时不通知（热路径只读一个布尔）", () => {
  let n = 0;
  const g = createControlGate(() => n++);
  for (let i = 0; i < 100; i++) g.onInput("a");
  assert.equal(n, 0);
  assert.equal(g.armed, false);
});

test("agentDriving 开闸，第一次用户打字通知一次，之后不再通知", () => {
  let n = 0;
  const g = createControlGate(() => n++);
  g.set("agentDriving");
  assert.equal(g.armed, true);
  g.onInput("l");
  g.onInput("s");
  g.onInput("\r");
  assert.equal(n, 1);
  assert.equal(g.armed, false);
});

test("开闸后终端自动应答不触发通知，真键盘才触发", () => {
  let n = 0;
  const g = createControlGate(() => n++);
  g.set("agentDriving");
  g.onInput("\x1b[12;40R");
  g.onInput("\x1b[I");
  assert.equal(n, 0);
  assert.equal(g.armed, true);
  g.onInput("x");
  assert.equal(n, 1);
});

test("交还（再次 agentDriving）后闸重新打开", () => {
  let n = 0;
  const g = createControlGate(() => n++);
  g.set("agentDriving");
  g.onInput("a");
  g.set("paused");
  g.onInput("b");
  assert.equal(n, 1);
  g.set("agentDriving");
  g.onInput("c");
  assert.equal(n, 2);
});

test("paused / userDriving 都不开闸", () => {
  const g = createControlGate(() => {});
  g.set("paused");
  assert.equal(g.armed, false);
  g.set("userDriving");
  assert.equal(g.armed, false);
});

test("applyControl：userDriving 删除登记，没变化返回原对象", () => {
  const s0: Record<string, ControlState> = {};
  const s1 = applyControl(s0, { session: "a", state: "agentDriving" });
  assert.deepEqual(s1, { a: "agentDriving" });
  assert.equal(applyControl(s1, { session: "a", state: "agentDriving" }), s1);
  const s2 = applyControl(s1, { session: "a", state: "paused" });
  assert.deepEqual(s2, { a: "paused" });
  const s3 = applyControl(s2, { session: "a", state: "userDriving" });
  assert.deepEqual(s3, {});
  assert.equal(applyControl(s3, { session: "zz", state: "userDriving" }), s3);
});

test("pausedSessions / drivingSessions", () => {
  const s: Record<string, ControlState> = { a: "paused", b: "agentDriving", c: "paused" };
  assert.deepEqual(pausedSessions(s).sort(), ["a", "c"]);
  assert.deepEqual(drivingSessions(s), ["b"]);
});

test("AI 驾驶的会话只读：打字 / 粘贴 / 方向键丢掉，Ctrl+C 是唯一出口", () => {
  assert.equal(lockedInput(true, "a"), "drop");
  assert.equal(lockedInput(true, "ls -la\r"), "drop"); // 粘贴
  assert.equal(lockedInput(true, "\r"), "drop");
  assert.equal(lockedInput(true, "\x1b[A"), "drop"); // 方向键
  assert.equal(lockedInput(true, "2"), "drop"); // 不能替 AI 选菜单
  assert.equal(lockedInput(true, "\x03"), "interrupt");
});

test("锁住时 xterm 的自动应答照常放行（拦了终端里的程序会卡住）", () => {
  assert.equal(lockedInput(true, "\x1b[12;40R"), "pass"); // CPR
  assert.equal(lockedInput(true, "\x1b[?1;2c"), "pass"); // DA
  assert.equal(lockedInput(true, "\x1b[I"), "pass"); // 焦点上报
});

test("没在 AI 驾驶时一切照常", () => {
  for (const d of ["a", "\r", "\x03", "\x1b[A"]) assert.equal(lockedInput(false, d), "pass");
});
