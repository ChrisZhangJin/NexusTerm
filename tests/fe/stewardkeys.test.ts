/**
 * 用户敲键判定与采集门控：
 * - xterm 自己生成的字节（焦点 / 鼠标 / 查询应答）不算用户敲键，不能让托管任务让路；
 * - 只有托管中的 console 才采屏幕帧、跑心跳；新签契约立刻补一帧全量。
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import type { Terminal } from "@xterm/xterm";
import {
  isXtermGenerated,
  registerTerm,
  setScreenCaptureEnabled,
  setScreenDeltaSink,
  setStewardConsoles,
  type ScreenDelta,
} from "../../src/lib/termRegistry";
import { isUserTyping } from "../../src/lib/control";

const GENERATED = [
  "\x1b[I", // 焦点进入
  "\x1b[O", // 焦点离开
  "\x1b[M !!", // X10 鼠标按下（1,1）
  "\x1b[M#\x7f\x7f", // X10 鼠标释放、坐标较大
  "\x1b[<0;10;5M", // SGR 按下
  "\x1b[<0;10;5m", // SGR 释放
  "\x1b[<35;80;24M", // SGR 移动（1003 追踪）
  "\x1b[<64;3;3M", // SGR 滚轮
  "\x1b[32;10;5M", // urxvt
  "\x1b[12;40R", // CPR
  "\x1b[?12;40R",
  "\x1b[?1;2c", // DA1
  "\x1b[>0;276;0c", // DA2
  "\x1b[0n", // DSR
  "\x1b[?2004;2$y", // DECRPM
  "\x1b[8;24;80t", // 窗口尺寸
  "\x1b]11;rgb:0000/0000/0000\x07", // OSC 颜色应答
  "\x1bP1$r0m\x1b\\", // DCS 应答
  // 一次 onData 连着好几段
  "\x1b[<35;1;1M\x1b[<35;2;1M\x1b[<35;3;1M",
  "\x1b[O\x1b[I",
  "\x1b[I\x1b[12;40R",
];

const USER = [
  "a",
  "中文",
  "\r",
  "\x03",
  "\x7f",
  "\t",
  "\x1b",
  "\x1b[A",
  "\x1bOP",
  "\x1b[3~",
  "\x1b[1;5C",
  "\x1bb",
  "\x1b[200~ls -la\x1b[201~", // 括号粘贴
  "ls\r",
  "\x1b[Ia", // 焦点事件后面跟着真字符：整串不全是生成字节，算用户
];

test("xterm 生成的焦点 / 鼠标 / 应答字节被识别出来", () => {
  for (const d of GENERATED) assert.equal(isXtermGenerated(d), true, JSON.stringify(d));
});

test("真实按键 / 粘贴 / 方向键都不算 xterm 生成", () => {
  for (const d of USER) assert.equal(isXtermGenerated(d), false, JSON.stringify(d));
  assert.equal(isXtermGenerated(""), false);
});

test("Agent 控制闸复用同一判定：鼠标 / 焦点不暂停 Agent", () => {
  for (const d of GENERATED) assert.equal(isUserTyping(d), false, JSON.stringify(d));
  for (const d of USER) assert.equal(isUserTyping(d), true, JSON.stringify(d));
});

// ————————————————————————— 采集门控 —————————————————————————

interface FakeWindow {
  intervals: Map<number, () => void>;
  timeouts: Map<number, () => void>;
  flush(): void;
  restore(): void;
}

function fakeWindow(): FakeWindow {
  const g = globalThis as unknown as { window?: unknown };
  const prev = g.window;
  const intervals = new Map<number, () => void>();
  const timeouts = new Map<number, () => void>();
  let n = 0;
  g.window = {
    setInterval: (fn: () => void) => {
      intervals.set(++n, fn);
      return n;
    },
    clearInterval: (id: number) => intervals.delete(id),
    setTimeout: (fn: () => void) => {
      timeouts.set(++n, fn);
      return n;
    },
    clearTimeout: (id: number) => timeouts.delete(id),
  };
  return {
    intervals,
    timeouts,
    flush() {
      const fns = [...timeouts.values()];
      timeouts.clear();
      fns.forEach((fn) => fn());
    },
    restore() {
      g.window = prev;
    },
  };
}

const fakeTerm = (lines: string[]): Terminal =>
  ({
    cols: 80,
    rows: 24,
    buffer: {
      active: {
        type: "normal",
        get length() {
          return lines.length;
        },
        getLine: (y: number) => ({ translateToString: () => lines[y] ?? "" }),
        cursorX: 0,
        cursorY: 0,
      },
    },
  }) as unknown as Terminal;

test("没有契约时不采帧、不起心跳；签约立刻补全量帧，停约停心跳", () => {
  const w = fakeWindow();
  const frames: ScreenDelta[] = [];
  const unA = registerTerm("cap-a", fakeTerm(["$ a"]));
  const unB = registerTerm("cap-b", fakeTerm(["$ b"]));
  try {
    setScreenDeltaSink((d) => frames.push(d));
    setScreenCaptureEnabled(true);
    w.flush();
    assert.equal(w.intervals.size, 0, "没有托管中的 console 时不该有心跳定时器");
    assert.equal(frames.length, 0, "没签契约的终端不采帧");

    // 签约：立刻补一帧全量，心跳起来
    setStewardConsoles(["cap-a"]);
    assert.equal(w.intervals.size, 1);
    w.flush();
    assert.equal(frames.length, 1);
    assert.equal(frames[0].consoleId, "cap-a");
    assert.equal(frames[0].full, true);

    // 心跳只照顾托管中的 cap-a
    const [beat] = [...w.intervals.values()];
    beat();
    w.flush();
    assert.deepEqual(
      frames.map((f) => f.consoleId),
      ["cap-a", "cap-a"]
    );

    // 契约全停：心跳定时器被清掉
    setStewardConsoles([]);
    assert.equal(w.intervals.size, 0);
    beat();
    w.flush();
    assert.equal(frames.length, 2, "停约后不再上报");

    // 再签：又是一帧全量（旧基线已丢，后端不会拿到半截增量）
    setStewardConsoles(["cap-a"]);
    w.flush();
    assert.equal(frames.length, 3);
    assert.equal(frames[2].full, true);
  } finally {
    setScreenCaptureEnabled(false);
    setStewardConsoles([]);
    setScreenDeltaSink(null);
    unA();
    unB();
    w.restore();
  }
});

