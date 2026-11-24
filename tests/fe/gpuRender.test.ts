import { test } from "node:test";
import assert from "node:assert/strict";

import { GpuSlots, MAX_CONTEXT_LOSSES, detectGpu, isSoftwareRenderer, wantsGpu } from "../../src/lib/gpuRender";

test("isSoftwareRenderer：软件模拟的 WebGL 不用；真显卡可用", () => {
  for (const n of ["Google SwiftShader", "llvmpipe (LLVM 15.0.7, 256 bits)", "Microsoft Basic Render Driver", "softpipe"]) {
    assert.ok(isSoftwareRenderer(n), n);
  }
  for (const n of ["Apple M2 Pro", "ANGLE (Apple, ANGLE Metal Renderer: Apple M1, Unspecified Version)", "NVIDIA GeForce RTX 3060", "Intel(R) Iris(R) Xe Graphics"]) {
    assert.ok(!isSoftwareRenderer(n), n);
  }
});

test("GpuSlots：满了挤掉最久没用的那个，并通知它退回 Canvas", () => {
  const s = new GpuSlots(2);
  const evicted: string[] = [];
  s.claim("a", () => evicted.push("a"));
  s.claim("b", () => evicted.push("b"));
  s.claim("a", () => evicted.push("a")); // a 又被看了一眼 → 变成最近
  s.claim("c", () => evicted.push("c")); // 满了：挤掉最久没用的 b
  assert.deepEqual(evicted, ["b"]);
  assert.deepEqual(s.holders(), ["a", "c"]);
  assert.ok(!s.has("b"));
});

test("GpuSlots：重复 claim 不占两个名额；release 让出名额且不触发 evict", () => {
  const s = new GpuSlots(2);
  let evicts = 0;
  s.claim("a", () => evicts++);
  s.claim("a", () => evicts++);
  s.claim("b", () => evicts++);
  assert.equal(evicts, 0);
  s.release("a");
  s.claim("c", () => evicts++);
  assert.equal(evicts, 0);
  assert.deepEqual(s.holders(), ["b", "c"]);
});

test("wantsGpu：设置关了 / 不可见 / 没有 GPU / 丢失太多次都不用", () => {
  const gpu = { ok: true, renderer: "Apple M2" };
  assert.ok(wantsGpu({ pref: true, visible: true, gpu, losses: 0 }));
  assert.ok(!wantsGpu({ pref: false, visible: true, gpu, losses: 0 }));
  assert.ok(!wantsGpu({ pref: true, visible: false, gpu, losses: 0 }));
  assert.ok(!wantsGpu({ pref: true, visible: true, gpu: { ok: false, renderer: "" }, losses: 0 }));
  assert.ok(!wantsGpu({ pref: true, visible: true, gpu, losses: MAX_CONTEXT_LOSSES }));
});

test("detectGpu：非浏览器环境里报告不可用（不抛）", () => {
  const g = detectGpu();
  assert.equal(g.ok, false);
});
