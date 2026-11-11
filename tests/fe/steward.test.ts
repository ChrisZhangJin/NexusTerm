/**
 * P0 会话织物的跨语言契约：前端帧指纹必须与 Rust `agent::steward::frame_hash`
 * 逐字节一致，否则后端会把每一帧都判成漂移、无限要求重发全量。
 *
 * 校验向量来自 Rust 侧 `hash_matches_shared_vector`，两边改算法要一起改。
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import type { Terminal } from "@xterm/xterm";
import {
  frameHash,
  registerTerm,
  setScreenCaptureEnabled,
  setScreenDeltaSink,
  type ScreenDelta,
} from "../../src/lib/termRegistry";
import {
  ACTION_LABELS,
  OUTCOME_LABELS,
  REF_LABELS,
  decisionSettled,
  splitList,
  type DecisionRecord,
} from "../../src/lib/steward";

test("帧指纹与后端共用同一组校验向量", () => {
  assert.equal(frameHash(["hello"], false, 80, 24, 0, 0), "ee7ae512");
  assert.equal(frameHash(["hello", "world"], true, 120, 40, 5, 9), "16952930");
});

test("行内容、alt、尺寸、光标任一项变化都会换指纹", () => {
  const base = frameHash(["$ pwd", "/tmp"], false, 80, 24, 3, 1);
  assert.notEqual(frameHash(["$ pwd", "/var"], false, 80, 24, 3, 1), base);
  assert.notEqual(frameHash(["$ pwd", "/tmp"], true, 80, 24, 3, 1), base);
  assert.notEqual(frameHash(["$ pwd", "/tmp"], false, 120, 24, 3, 1), base);
  assert.notEqual(frameHash(["$ pwd", "/tmp"], false, 80, 40, 3, 1), base);
  assert.notEqual(frameHash(["$ pwd", "/tmp"], false, 80, 24, 4, 1), base);
  assert.notEqual(frameHash(["$ pwd", "/tmp"], false, 80, 24, 3, 2), base);
});

test("中文按 UTF-8 字节参与哈希（不是 UTF-16 码元）", () => {
  const cn = frameHash(["中文"], false, 80, 24, 0, 0);
  // 与 Rust 端 `fnv1a("中文\n#0:80:24:0:0")` 对齐的已知值
  assert.match(cn, /^[0-9a-f]{8}$/);
  assert.notEqual(cn, frameHash(["中"], false, 80, 24, 0, 0));
  assert.notEqual(cn, frameHash(["中文 "], false, 80, 24, 0, 0));
});

/**
 * P1 补的 P0 缺口：终端静默时仍要发同哈希心跳帧。
 * 否则后端分不清「屏幕没动」和「前端不在（浮窗 / 最小化 / 锁屏）」，
 * idleFrames 也不会按真实时间增长，空转检测就没有可信的时间基准。
 */
test("静默期心跳帧持续上报：哈希不变、只带 frameId、时间戳刷新", () => {
  const g = globalThis as unknown as { window?: unknown };
  const prevWindow = g.window;
  const intervals: Array<() => void> = [];
  let timeouts: Array<() => void> = [];
  g.window = {
    setInterval: (fn: () => void) => {
      intervals.push(fn);
      return intervals.length;
    },
    clearInterval: () => undefined,
    setTimeout: (fn: () => void) => {
      timeouts.push(fn);
      return timeouts.length;
    },
    clearTimeout: () => undefined,
  };

  const lines = ["$ pwd", "/tmp"];
  const fake = {
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
        cursorY: 1,
      },
    },
  } as unknown as Terminal;

  const frames: ScreenDelta[] = [];
  let unregister = () => undefined;
  try {
    setScreenDeltaSink((d) => frames.push(d));
    unregister = registerTerm("tab-1", fake);
    setScreenCaptureEnabled(true);
    assert.equal(intervals.length, 1, "开启采集后应起一个心跳定时器");

    // 初始全量帧
    timeouts.splice(0).forEach((fn) => fn());
    assert.equal(frames.length, 1);
    assert.equal(frames[0].full, true);
    assert.equal(frames[0].lines.length, 2);

    // 两次静默心跳：屏幕一个字节都没变，但帧号与时间戳要前进
    for (let i = 0; i < 2; i++) {
      intervals[0]();
      timeouts.splice(0).forEach((fn) => fn());
    }
    assert.equal(frames.length, 3);
    assert.deepEqual(
      frames.map((f) => f.frameId),
      [1, 2, 3]
    );
    assert.equal(new Set(frames.map((f) => f.hash)).size, 1, "哈希必须保持不变");
    assert.equal(frames[1].full, false);
    assert.equal(frames[1].lines.length, 0, "同哈希帧不带行内容");
    assert.ok(frames[2].tsMs >= frames[1].tsMs, "时间戳要持续刷新，后端才不会误判断流");
  } finally {
    setScreenCaptureEnabled(false);
    unregister();
    setScreenDeltaSink(null);
    g.window = prevWindow;
    void timeouts;
  }
});

// ───────────────── P2：决策卡的人话标签与升级卡收敛 ─────────────────

const fakeDecision = (outcome: string): DecisionRecord => ({
  seq: 1,
  tsMs: 1_000,
  contractId: "c-1",
  consoleId: "tab-1",
  situationSignature: "claude_code|question|A",
  situationHash: "aabbccdd",
  ctxRefs: [],
  candidates: [],
  chosen: { kind: "answer", text: "yes", optionIndex: null, injectable: true },
  rationale: "依据意向",
  confidence: 0.9,
  tier: "auto",
  llmUsed: false,
  outcome,
  escalation: null,
  receipt: null,
});

test("升级卡只在未收敛的决策上出现", () => {
  assert.equal(decisionSettled(fakeDecision("proposed")), false);
  for (const o of ["applied", "escalated", "rejected", "no_receipt", "rolled_back"]) {
    assert.equal(decisionSettled(fakeDecision(o)), true, `${o} 不该继续显示倒计时`);
  }
});

test("决策卡的动作 / 依据 / 结果标签覆盖后端枚举", () => {
  for (const k of ["answer", "secret", "wait", "observe", "advance", "handoff"]) {
    assert.ok(ACTION_LABELS[k], `动作 ${k} 缺中文标签`);
  }
  for (const s of ["profile", "intent", "knowledge", "playbook", "authority", "policy", "history"]) {
    assert.ok(REF_LABELS[s], `依据来源 ${s} 缺中文标签`);
  }
  for (const o of ["proposed", "applied", "escalated", "observed", "rejected", "no_receipt", "rolled_back"]) {
    assert.ok(OUTCOME_LABELS[o], `结果 ${o} 缺中文标签`);
  }
});

test("依据编辑里的中英文分隔列表能去掉空项", () => {
  assert.deepEqual(splitList("别改设置 UI, 最小改动；不要动命令\n,  ,保持签名"), [
    "别改设置 UI",
    "最小改动",
    "不要动命令",
    "保持签名",
  ]);
  assert.deepEqual(splitList("  , ，;  "), []);
});

// ————————————————————————— P3–P5 前端接入 —————————————————————————

import { emitInput as emitInputP3, setUserKeySink } from "../../src/lib/termRegistry";
import { EVENT_LABELS } from "../../src/lib/steward";

test("P3 事件枚举扩容后每个 kind 都有中文标签", () => {
  const kinds = [
    "takeover",
    "handback",
    "undo_window",
    "policy_denied",
    "default_action_applied",
    "checkpoint_saved",
    "resumed",
    "timebox_hit",
    "budget_exhausted",
    "suspended",
    "distilled",
    "planner_used",
  ] as const;
  for (const k of kinds) {
    assert.ok(EVENT_LABELS[k], `事件 ${k} 缺中文标签`);
  }
});

test("用户敲键触发夺权上报，且同 console 250ms 内节流", () => {
  const calls: Array<[string, number]> = [];
  setUserKeySink((id, at) => calls.push([id, at]));
  emitInputP3("tab-1", "a");
  emitInputP3("tab-1", "b");
  emitInputP3("tab-1", "c");
  // 连发三个字节只算一次：夺权是一次性状态切换，SLA 看首个键
  assert.equal(calls.length, 1);
  assert.equal(calls[0][0], "tab-1");
  // 另一个 console 不受节流影响
  emitInputP3("tab-2", "x");
  assert.equal(calls.length, 2);
  assert.equal(calls[1][0], "tab-2");
  setUserKeySink(null);
});

test("空字节不触发夺权上报（防 paste 空事件噪音）", () => {
  const calls: Array<[string, number]> = [];
  setUserKeySink((id, at) => calls.push([id, at]));
  emitInputP3("tab-3", "");
  assert.equal(calls.length, 0);
  setUserKeySink(null);
});
