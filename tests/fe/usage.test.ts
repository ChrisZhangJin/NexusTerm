/**
 * `lib/usage` 的回归测试：token / 花费 / 用时的格式化、单价匹配、花费上限换算，
 * 以及对话状态里「上一轮用量」的落地。
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  fmtElapsed,
  fmtTokens,
  fmtUsd,
  fromHostSpent,
  fromRunUsage,
  hasUsage,
  normalizePrice,
  parseCostInput,
  priceFor,
  usageLine,
  usageTooltip,
  usageWithTime,
  usdToMicros,
} from "../../src/lib/usage";
import { EMPTY_CONV, withDone, withUsage } from "../../src/lib/agentRun";

test("fmtTokens：k / M 紧凑写法", () => {
  assert.equal(fmtTokens(0), "0");
  assert.equal(fmtTokens(999), "999");
  assert.equal(fmtTokens(1234), "1.2k");
  assert.equal(fmtTokens(12_345), "12.3k");
  assert.equal(fmtTokens(123_456), "123k");
  assert.equal(fmtTokens(1_234_567), "1.23M");
  assert.equal(fmtTokens(-5), "0");
});

test("fmtUsd：两位小数，不足一分给三位，再小写 <$0.001", () => {
  assert.equal(fmtUsd(0), "$0");
  assert.equal(fmtUsd(0.0004), "<$0.001");
  assert.equal(fmtUsd(0.004), "$0.004");
  assert.equal(fmtUsd(0.0412), "$0.04");
  assert.equal(fmtUsd(12.5), "$12.50");
});

test("fmtElapsed：秒 / 分 / 时分", () => {
  assert.equal(fmtElapsed(45), "45秒");
  assert.equal(fmtElapsed(12 * 60 + 5), "12分");
  assert.equal(fmtElapsed(3600 + 5 * 60), "1时05分");
});

test("usageLine：12.3k tok · ≈$0.04；没单价只给 token；估算的 token 带 ≈", () => {
  const u = { inputTokens: 10_000, outputTokens: 2_300, estimated: false, costUsd: 0.0412 };
  assert.equal(usageLine(u), "12.3k tok · ≈$0.04");
  assert.equal(usageLine({ ...u, costUsd: null }), "12.3k tok");
  assert.equal(usageLine({ ...u, estimated: true, costUsd: null }), "≈12.3k tok");
  assert.equal(usageWithTime({ ...u, elapsedSecs: 125 }), "12.3k tok · ≈$0.04 · 2分");
  // 还没调过模型：只显示用时
  assert.equal(usageWithTime({ inputTokens: 0, outputTokens: 0, estimated: false, costUsd: null, elapsedSecs: 30 }), "30秒");
});

test("usageTooltip：拆输入 / 输出，估算注明「估算」，没单价提示去设置", () => {
  const t = usageTooltip({ inputTokens: 10_000, outputTokens: 2_300, cacheReadTokens: 8_000, estimated: true, costUsd: null });
  assert.match(t, /输入 10\.0k tok · 输出 2\.3k tok/);
  assert.match(t, /缓存命中 8\.0k/);
  assert.match(t, /估算/);
  assert.match(t, /没有设置单价/);
  assert.match(usageTooltip({ inputTokens: 1, outputTokens: 1, estimated: false, costUsd: 0.5 }), /≈\$0\.50/);
});

test("fromHostSpent：旧记录没 token 字段；priced=false 不给钱", () => {
  const old = fromHostSpent({ steps: 3, elapsedSecs: 60, outputChars: 10 });
  assert.equal(hasUsage(old), false);
  assert.equal(old.costUsd, null);
  const s = fromHostSpent({ steps: 3, elapsedSecs: 60, outputChars: 10, inputTokens: 100, outputTokens: 5, costMicros: 40_000, priced: true });
  assert.equal(s.costUsd, 0.04);
  assert.equal(usageLine(s), "105 tok · ≈$0.04");
  const r = fromRunUsage({ inputTokens: 1, outputTokens: 2, estimated: true, elapsedSecs: 3, costUsd: null });
  assert.equal(r.costUsd, null);
  assert.equal(r.estimated, true);
});

test("花费上限：美元 ↔ 微美元，空 / 非法 / 负数 = 不限", () => {
  assert.equal(usdToMicros(0.5), 500_000);
  assert.equal(usdToMicros(0), 0);
  assert.equal(usdToMicros(-1), 0);
  assert.equal(usdToMicros(NaN), 0);
  assert.equal(parseCostInput(""), 0);
  assert.equal(parseCostInput("$1.239"), 1.24);
  assert.equal(parseCostInput("abc"), 0);
  assert.equal(parseCostInput("-2"), 0);
});

test("priceFor：不分大小写、取最长命中、单价全 0 的行不算", () => {
  const prices = [
    { pattern: "gpt-4o", inputPerM: 2.5, outputPerM: 10 },
    { pattern: "gpt-4o-mini", inputPerM: 0.15, outputPerM: 0.6 },
    { pattern: "deepseek", inputPerM: 0, outputPerM: 0 },
  ];
  assert.equal(priceFor(prices, "GPT-4o-mini-2024")?.pattern, "gpt-4o-mini");
  assert.equal(priceFor(prices, "gpt-4o")?.pattern, "gpt-4o");
  assert.equal(priceFor(prices, "deepseek-chat"), null);
  assert.equal(priceFor(prices, ""), null);
  assert.equal(priceFor(undefined, "gpt-4o"), null);
  assert.deepEqual(normalizePrice({ pattern: "x", inputPerM: -1, outputPerM: Number("abc") }), { pattern: "x", inputPerM: 0, outputPerM: 0 });
});

test("对话：done 带的用量与 usage 事件都落到 lastUsage", () => {
  const u = { inputTokens: 1, outputTokens: 2, estimated: false, elapsedSecs: 3, costUsd: 0.01 };
  const c = withUsage(withDone(EMPTY_CONV, "stop"), u);
  assert.deepEqual(c.lastUsage, u);
  // 旧后端的 done 没有 usage：不清掉上一轮的
  assert.deepEqual(withUsage(c, undefined).lastUsage, u);
});
