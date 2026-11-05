/**
 * `lib/agentHistory` 的回归测试：哪些消息落盘、读回来怎样收尾、防抖写盘。
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  createDebouncedSaver,
  fromPersisted,
  INTERRUPTED_NOTE,
  MAX_PERSISTED_MESSAGES,
  mergeRestored,
  toPersisted,
} from "../../src/lib/agentHistory";
import type { AgentMessage } from "../../src/types";

const msg = (id: string, extra: Partial<AgentMessage> = {}): AgentMessage => ({
  id,
  role: "assistant",
  text: "",
  ts: 1,
  ...extra,
});

test("toPersisted：丢弃流式中的半截回答，只留最近 N 条", () => {
  const list = [msg("a", { text: "ok" }), msg("b", { text: "半截", streaming: true })];
  assert.deepEqual(toPersisted(list).map((m) => m.id), ["a"]);

  const many = Array.from({ length: MAX_PERSISTED_MESSAGES + 25 }, (_, i) => msg(`m${i}`));
  const kept = toPersisted(many);
  assert.equal(kept.length, MAX_PERSISTED_MESSAGES);
  assert.equal(kept[0].id, "m25");
  assert.equal(kept[kept.length - 1].id, `m${MAX_PERSISTED_MESSAGES + 24}`);
});

test("fromPersisted：非数组 / 坏条目被丢弃，不会抛", () => {
  assert.deepEqual(fromPersisted(null), []);
  assert.deepEqual(fromPersisted({ a: 1 }), []);
  const out = fromPersisted([
    1,
    "x",
    null,
    { id: 1 },
    { id: "a", role: "user", text: "hi", ts: 1 },
    { id: "b", role: "bot", text: "", ts: 1 },
  ]);
  assert.deepEqual(out.map((m) => m.id), ["a"]);
});

test("fromPersisted：读回的消息永远不带 streaming", () => {
  const [m] = fromPersisted([{ id: "a", role: "assistant", text: "x", ts: 1, streaming: true }]);
  assert.equal(m.streaming, undefined);
});

test("fromPersisted：没答复的审批卡按未批准，没结局的密码卡按 expired", () => {
  const out = fromPersisted([
    msg("ap", { approve: { requestId: "r", command: "rm -rf x", target: "t", targetLabel: "t", reasons: [] } }),
    msg("ap2", {
      approve: { requestId: "r2", command: "ls", target: "t", targetLabel: "t", reasons: [] },
      approveResult: true,
    }),
    msg("sc", { secret: { requestId: "s", target: "t", targetLabel: "t", reason: "sudo", timeoutSecs: 120 } }),
  ]);
  assert.equal(out[0].approveResult, false);
  assert.equal(out[1].approveResult, true, "已有答复的不动");
  assert.equal(out[2].secretResult, "expired");
});

test("fromPersisted：进行中的步骤卡收尾，已完成的原样保留", () => {
  const out = fromPersisted([
    msg("e", { step: { kind: "exec", step: 1, command: "sleep 99" } }),
    msg("k", { step: { kind: "keys", step: 2, command: "\\x03" } }),
    msg("w", { step: { kind: "wait", step: 3, command: "ready" } }),
    msg("s", { step: { kind: "screen", step: 4 } }),
    msg("p", { step: { kind: "secret", step: 5, command: "sudo" } }),
    msg("done", { step: { kind: "result", step: 6, command: "ls", output: "a\nb" } }),
    msg("keys-done", { step: { kind: "keys", step: 7, output: "" } }),
  ]);
  assert.equal(out[0].step?.kind, "result");
  assert.equal(out[0].step?.output, INTERRUPTED_NOTE);
  for (const i of [1, 2, 3]) {
    assert.equal(out[i].step?.output, "", `步骤 ${i} 应被补成已收尾`);
    assert.equal(out[i].step?.text, INTERRUPTED_NOTE);
  }
  assert.equal(out[4].step?.secretStatus, "cancelled");
  assert.equal(out[5].step?.output, "a\nb");
  assert.equal(out[6].step?.text, undefined, "已收尾的 keys 不加说明");
});

test("步骤种类（tool/result/skip/note/open/tab/export）都能原样往返", () => {
  const kinds = ["tool", "result", "skip", "note", "open", "tab", "export"];
  const list = kinds.map((k, i) => msg(`k${i}`, { step: { kind: k, step: i, text: k } }));
  const out = fromPersisted(JSON.parse(JSON.stringify(toPersisted(list))));
  assert.deepEqual(out.map((m) => m.step?.kind), kinds);
});

test("mergeRestored：面板为空时原样返回读回的那份（同一引用）", () => {
  const restored = [msg("a")];
  assert.equal(mergeRestored([], restored), restored);
});

test("mergeRestored：读盘期间已有新消息 → 旧历史在前、新的在后，同 id 不重复", () => {
  const cur = [msg("n1"), msg("dup")];
  const out = mergeRestored(cur, [msg("o1"), msg("dup")]);
  assert.deepEqual(out.map((m) => m.id), ["o1", "n1", "dup"]);
  assert.equal(mergeRestored(cur, []), cur);
});

function fakeTimers() {
  let next = 1;
  const pending = new Map<number, () => void>();
  return {
    timers: {
      set: (fn: () => void) => {
        const h = next++;
        pending.set(h, fn);
        return h;
      },
      clear: (h: unknown) => void pending.delete(h as number),
    },
    runAll: () => {
      const fns = [...pending.values()];
      pending.clear();
      fns.forEach((f) => f());
    },
    count: () => pending.size,
  };
}

test("createDebouncedSaver：连续 schedule 只写最后一次", () => {
  const t = fakeTimers();
  const saved: number[] = [];
  const s = createDebouncedSaver<number>((v) => saved.push(v), 100, t.timers);
  s.schedule(1);
  s.schedule(2);
  s.schedule(3);
  assert.equal(t.count(), 1);
  t.runAll();
  assert.deepEqual(saved, [3]);
  assert.equal(s.pending(), false);
});

test("createDebouncedSaver：flush 立刻写、cancel 丢弃", () => {
  const t = fakeTimers();
  const saved: number[] = [];
  const s = createDebouncedSaver<number>((v) => saved.push(v), 100, t.timers);
  s.schedule(1);
  s.flush();
  assert.deepEqual(saved, [1]);
  s.flush(); // 没有挂起的：不重复写
  assert.deepEqual(saved, [1]);

  s.schedule(2);
  s.cancel();
  t.runAll();
  s.flush();
  assert.deepEqual(saved, [1], "cancel 之后什么都不会被写出去");
});
