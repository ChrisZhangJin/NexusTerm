/**
 * 「提交命令事件」推送前的判定自测（2.4）。跑法：`npm test`
 *
 * 钉死的是安全边界：没被回显的行（密码）内容绝不出前端。
 */

import assert from "node:assert/strict";
import test from "node:test";

import { buildUserCommandReport } from "../../src/lib/userCommand";
import { feedKeys, newLineState } from "../../src/lib/complete";

test("回显过的普通命令：带内容推送", () => {
  assert.deepEqual(buildUserCommandReport("  ls -la ", true, false), { line: "ls -la", noEcho: false });
});

test("没被回显（密码提示）：只发无回显标记，行内容清空", () => {
  assert.deepEqual(buildUserCommandReport("hunter2", false, false), { line: "", noEcho: true });
});

test("全屏程序（备用屏）里敲的不推", () => {
  assert.equal(buildUserCommandReport("dd", true, true), null);
  assert.equal(buildUserCommandReport("secret", false, true), null);
});

test("回显过但全是空白：不推", () => {
  assert.equal(buildUserCommandReport("   ", true, false), null);
});

test("超长行在前端先截断，避免把整段粘贴塞进 IPC", () => {
  const r = buildUserCommandReport("a".repeat(10000), true, false);
  assert.ok(r && r.line.length <= 4096 && !r.noEcho);
});

test("与行状态机衔接：只有回车才产出整行，逐键不产出", () => {
  const st = newLineState();
  assert.equal(feedKeys(st, "l"), null);
  assert.equal(feedKeys(st, "s"), null);
  const committed = feedKeys(st, "\r");
  assert.equal(committed, "ls");
  assert.deepEqual(buildUserCommandReport(committed!, true, false), { line: "ls", noEcho: false });
});
