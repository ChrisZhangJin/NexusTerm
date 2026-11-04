/**
 * `lib/zmodem` 收尾阶段（ZFIN 之后）的回归测试。
 *
 * 现场症状：用 sz 下载偶尔报
 * `PROTOCOL: Only thing after ZFIN should be “OO” (79,79), not: 27,91,63,50,48,48,52,104…`
 * —— 后面那串是 shell 提示符。zmodem.js 在同一次 consume() 里解完 ZFIN 后还会继续
 * 跑 header 解析，把紧跟着的 `OO` 当垃圾吞掉；之后会话既没结束，下一块数据又撞上
 * 「ZFIN 之后只许是 OO」的断言。所以这里不测库，测我们给库打的补丁：不管 `OO` /
 * 提示符与 ZFIN 是同一块还是分几块到，会话都要干净收尾，提示符都要回到终端。
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import Zmodem from "zmodem.js";

import { attachZmodem } from "../../src/lib/zmodem";

const enc = new TextEncoder();
const dec = new TextDecoder();

const hex = (name: string): number[] =>
  Array.from((Zmodem as any).Header.build(name).to_hex());
const cat = (...parts: number[][]): Uint8Array => new Uint8Array(parts.flat());
const txt = (s: string): number[] => Array.from(enc.encode(s));

const PROMPT = "\x1b[?2004h(base) \x1b]0;chris@host: ~/work\x07\x1b[01;32mchris@host\x1b[00m$ ";

/** 建一个已经检测到远端 sz、处于「等对方发文件 / ZFIN」状态的会话 */
function setup() {
  const term: number[] = [];
  const msgs: string[] = [];
  const gate = attachZmodem({
    termWrite: (s) => msgs.push(s),
    termWriteBytes: (b) => term.push(...b),
    send: () => Promise.resolve(),
    notify: () => {},
  });
  gate.consume(cat(hex("ZRQINIT")));
  assert.equal(gate.isActive(), true, "握手后应进入会话");
  // Sentry 按设计会把握手头透传给终端（别的程序也可能恰好打出同样的字节），
  // 这里只关心握手**之后**终端收到了什么
  const base = term.length;
  return { gate, text: () => dec.decode(new Uint8Array(term.slice(base))), msgs };
}

const noError = (msgs: string[]) =>
  assert.ok(!msgs.some((m) => m.includes("协议错误")), `不该报协议错误：${msgs.join("|")}`);

test("ZFIN 与 OO、提示符在同一块：会话收尾，提示符回到终端", () => {
  const { gate, text, msgs } = setup();
  gate.consume(cat(hex("ZFIN"), txt("OO"), txt(PROMPT)));
  noError(msgs);
  assert.equal(gate.isActive(), false);
  assert.ok(text().endsWith(PROMPT), `终端应收到提示符，实际：${JSON.stringify(text())}`);
  assert.ok(!text().includes("OO"), "OO 不该漏到终端");
});

test("ZFIN 后 OO 与提示符分两块到：照常收尾", () => {
  const { gate, text, msgs } = setup();
  gate.consume(cat(hex("ZFIN")));
  assert.equal(gate.isActive(), true);
  gate.consume(cat(txt("OO")));
  assert.equal(gate.isActive(), false);
  gate.consume(cat(txt(PROMPT)));
  noError(msgs);
  assert.equal(text().endsWith(PROMPT), true);
});

test("OO 被拆成两块（O / O+提示符）", () => {
  const { gate, text, msgs } = setup();
  gate.consume(cat(hex("ZFIN")));
  gate.consume(cat(txt("O")));
  assert.equal(gate.isActive(), true, "只有半个 OO，还要等");
  gate.consume(cat(txt("O"), txt(PROMPT)));
  noError(msgs);
  assert.equal(gate.isActive(), false);
  assert.equal(text(), PROMPT);
});

test("对端没发 OO，直接就是提示符：当作会话结束，不报协议错误", () => {
  const { gate, text, msgs } = setup();
  gate.consume(cat(hex("ZFIN")));
  gate.consume(cat(txt(PROMPT)));
  noError(msgs);
  assert.equal(gate.isActive(), false);
  assert.equal(text(), PROMPT);
});

test("ZFIN 与提示符同块、没有 OO：同样收尾", () => {
  const { gate, text, msgs } = setup();
  gate.consume(cat(hex("ZFIN"), txt(PROMPT)));
  noError(msgs);
  assert.equal(gate.isActive(), false);
  assert.equal(text(), PROMPT);
});

test("收尾之后能开始下一次传输", () => {
  const { gate, msgs } = setup();
  gate.consume(cat(hex("ZFIN"), txt("OO"), txt(PROMPT)));
  gate.consume(cat(hex("ZRQINIT")));
  noError(msgs);
  assert.equal(gate.isActive(), true);
});
