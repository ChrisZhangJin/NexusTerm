/**
 * 宏引擎的纯逻辑自测。跑法：`npm test`
 *
 * 宏一旦出错，后果是**往远端敲了不该敲的东西**（把菜单选项打到目标机器
 * shell 里），所以转义、模式编译、录制切分这三块必须钉死。
 */

import assert from "node:assert/strict";
import test from "node:test";

import {
  compilePattern,
  describeStep,
  escapeText,
  unescapeText,
  startRecording,
  isTerminalReply,
} from "../../src/lib/macro";
import { emitInput } from "../../src/lib/termRegistry";

// ——— 转义 ———

test("unescapeText 还原常见转义", () => {
  assert.equal(unescapeText("ls -l\\r"), "ls -l\r");
  assert.equal(unescapeText("a\\nb"), "a\nb");
  assert.equal(unescapeText("a\\tb"), "a\tb");
  assert.equal(unescapeText("\\e[A"), "\x1b[A");
  assert.equal(unescapeText("\\x03"), "\x03");
});

test("unescapeText：反斜杠本身与不成对的转义", () => {
  // `\\n` 是两个字符：反斜杠 + n，不能变成换行
  assert.equal(unescapeText("\\\\n"), "\\n");
  // 末尾孤立的反斜杠原样保留
  assert.equal(unescapeText("abc\\"), "abc\\");
  // 不认识的转义原样保留，不吞字符
  assert.equal(unescapeText("\\q"), "\\q");
  // 坏的十六进制退回字面量
  assert.equal(unescapeText("\\xZZ"), "\\xZZ");
});

test("escapeText 与 unescapeText 互为逆运算", () => {
  for (const raw of ["ls -l\r", "a\tb", "\x1b[B", "\x03", "中文 \\ 路径", ""]) {
    assert.equal(unescapeText(escapeText(raw)), raw, `round-trip: ${JSON.stringify(raw)}`);
  }
});

test("escapeText 把控制字符写成可编辑的转义", () => {
  assert.equal(escapeText("\x1b[A"), "\\e[A");
  assert.equal(escapeText("\x03"), "\\x03");
  assert.equal(escapeText("\\"), "\\\\");
});

// ——— 模式编译 ———

test("compilePattern：空模式不匹配（调用方走静默分支）", () => {
  assert.equal(compilePattern(""), null);
  // 只由空白组成时不等于空串，但仍按子串处理 —— 是否 trim 由调用方决定
  assert.equal(typeof compilePattern("   "), "function");
});

test("compilePattern：子串与正则", () => {
  const sub = compilePattern("[Host]>")!;
  assert.equal(sub("请选择资产\n[Host]> "), true);
  assert.equal(sub("root@host:~#"), false);

  const re = compilePattern("/^(root|admin)@/m")!;
  assert.equal(re("xxx\nroot@web:~#"), true);
  assert.equal(re("user@web:~#"), false);

  const ci = compilePattern("/host\\]> /i")!;
  assert.equal(ci("[HOST]> "), true);
});

test("compilePattern：坏正则退化成子串，不抛异常", () => {
  const fn = compilePattern("/([a-z/")!;
  assert.equal(fn("/([a-z/ 原样"), true);
});

// ——— 步骤描述 ———

test("describeStep 给出人能读的摘要", () => {
  assert.equal(describeStep({ kind: "send", text: "2", enter: true }), "发送 2 ⏎");
  assert.equal(describeStep({ kind: "send", text: "", enter: true }), "发送 （空行回车） ⏎");
  assert.equal(describeStep({ kind: "wait", pattern: "[Host]>", timeoutMs: 0 }), "等待出现 [Host]>");
  assert.equal(describeStep({ kind: "sleep", ms: 1500 }), "等待 1.5s");
});

// ——— 录制 ———

const T = "rec-tab";

test("录制：一行一个「先等静默、再发送」", () => {
  const rec = startRecording(T);
  emitInput(T, "2\r");
  emitInput(T, "ls\r");
  const steps = rec.stop();
  assert.deepEqual(steps, [
    { kind: "wait", pattern: "", timeoutMs: 0 },
    { kind: "send", text: "2", enter: true },
    { kind: "wait", pattern: "", timeoutMs: 0 },
    { kind: "send", text: "ls", enter: true },
  ]);
});

test("录制：连续的回车不叠加多余的等待", () => {
  const rec = startRecording(T);
  emitInput(T, "\r\r");
  assert.deepEqual(rec.stop(), [
    { kind: "wait", pattern: "", timeoutMs: 0 },
    { kind: "send", text: "", enter: true },
    { kind: "wait", pattern: "", timeoutMs: 0 },
    { kind: "send", text: "", enter: true },
  ]);
});

test("录制：方向键单独成步（菜单导航不带回车）", () => {
  const rec = startRecording(T);
  emitInput(T, "\x1b[B\r");
  assert.deepEqual(rec.stop(), [
    { kind: "wait", pattern: "", timeoutMs: 0 },
    { kind: "send", text: "\\e[B", enter: false },
    { kind: "wait", pattern: "", timeoutMs: 0 },
    { kind: "send", text: "", enter: true },
  ]);
});

test("录制：退格与 Ctrl-U 只影响当前行，不进步骤", () => {
  const rec = startRecording(T);
  emitInput(T, "ab\x7f\x7fc");
  assert.deepEqual(rec.stop(), [
    { kind: "wait", pattern: "", timeoutMs: 0 },
    { kind: "send", text: "c", enter: false },
  ]);
});

test("录制：停止时未回车的半行也会保留", () => {
  const rec = startRecording(T);
  emitInput(T, "show ver");
  assert.deepEqual(rec.stop(), [
    { kind: "wait", pattern: "", timeoutMs: 0 },
    { kind: "send", text: "show ver", enter: false },
  ]);
});

test("录制：cancel 之后不再收键", () => {
  const rec = startRecording(T);
  emitInput(T, "a\r");
  rec.cancel();
  assert.deepEqual(rec.stop(), []);
});

// ——— 终端自动应答 ———

test("isTerminalReply：识别 xterm 的自动应答，不误伤用户按键", () => {
  // 自动应答
  assert.equal(isTerminalReply("\x1b[12;40R"), true, "DSR 光标位置");
  assert.equal(isTerminalReply("\x1b[?1;2c"), true, "DA 设备属性");
  assert.equal(isTerminalReply("\x1b]0;title\x07"), true, "OSC 应答");
  // 用户按键：方向键也是 ESC 序列，但绝不能当成应答
  assert.equal(isTerminalReply("\x1b[A"), false, "方向键");
  assert.equal(isTerminalReply("2"), false);
  assert.equal(isTerminalReply("\r"), false);
  assert.equal(isTerminalReply("\x03"), false);
});

test("录制：终端自动应答不会被录成步骤", () => {
  const rec = startRecording(T);
  emitInput(T, "\x1b[1;1R");
  emitInput(T, "2\r");
  assert.deepEqual(rec.stop(), [
    { kind: "wait", pattern: "", timeoutMs: 0 },
    { kind: "send", text: "2", enter: true },
  ]);
});
