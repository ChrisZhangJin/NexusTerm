/**
 * token 用量 / 花费 / 用时的显示（#8）。后端见 `src-tauri/src/agent/usage.rs`。
 *
 * 纯函数，便于 node:test 直接测：
 * - `usageLine`：卡头 / 对话框底部那一行 `12.3k tok · ≈$0.04`（没单价只显示 token）
 * - `usageTooltip`：悬停时的输入 / 输出拆分，估算的注明「估算」
 * - `fmtElapsed`：紧凑的用时
 * - 花费上限：输入框里的美元 ↔ 后端的微美元
 */

import type { HostSpent, ModelPrice, RunUsage } from "../types";

/** 1234 → "1.2k"，12345 → "12.3k"，1234567 → "1.23M"；1000 以下原样 */
export function fmtTokens(n: number): string {
  const v = Math.max(0, Math.round(Number(n) || 0));
  if (v < 1000) return String(v);
  if (v < 1_000_000) {
    const k = v / 1000;
    return `${k < 100 ? k.toFixed(1) : Math.round(k)}k`;
  }
  const m = v / 1_000_000;
  return `${m < 10 ? m.toFixed(2) : m < 100 ? m.toFixed(1) : Math.round(m)}M`;
}

/** 美元：≥ $0.01 两位小数；不足一分给三位（$0.004）；更小写 <$0.001 */
export function fmtUsd(usd: number): string {
  const v = Number(usd) || 0;
  if (v <= 0) return "$0";
  if (v < 0.001) return "<$0.001";
  if (v < 0.01) return `$${v.toFixed(3)}`;
  return `$${v.toFixed(2)}`;
}

/** 秒 → 「45秒」「12分」「1时05分」 */
export function fmtElapsed(secs: number): string {
  const s = Math.max(0, Math.floor(Number(secs) || 0));
  if (s < 60) return `${s}秒`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}分`;
  const h = Math.floor(m / 60);
  return `${h}时${String(m % 60).padStart(2, "0")}分`;
}

/** 显示用的用量（对话一轮的 `RunUsage` 与托管的 `HostSpent` 都收敛成它） */
export interface UsageView {
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens?: number;
  estimated: boolean;
  /** 没单价 = null，只显示 token */
  costUsd: number | null;
  elapsedSecs?: number;
}

export function fromRunUsage(u: RunUsage): UsageView {
  return {
    inputTokens: u.inputTokens,
    outputTokens: u.outputTokens,
    cacheReadTokens: u.cacheReadTokens,
    estimated: u.estimated,
    costUsd: typeof u.costUsd === "number" ? u.costUsd : null,
    elapsedSecs: u.elapsedSecs,
  };
}

export function fromHostSpent(s: HostSpent): UsageView {
  return {
    inputTokens: s.inputTokens ?? 0,
    outputTokens: s.outputTokens ?? 0,
    estimated: !!s.estimated,
    costUsd: s.priced ? (s.costMicros ?? 0) / 1_000_000 : null,
    elapsedSecs: s.elapsedSecs,
  };
}

/** 一个 token 都没有（还没调过模型 / 旧记录）时不显示 */
export function hasUsage(u: UsageView | null | undefined): u is UsageView {
  return !!u && u.inputTokens + u.outputTokens > 0;
}

/** `12.3k tok · ≈$0.04`；估算的 token 前加「≈」；没单价只给 token */
export function usageLine(u: UsageView): string {
  const tok = `${u.estimated ? "≈" : ""}${fmtTokens(u.inputTokens + u.outputTokens)} tok`;
  return u.costUsd === null ? tok : `${tok} · ≈${fmtUsd(u.costUsd)}`;
}

/** 带用时的一行：`12.3k tok · ≈$0.04 · 12分` */
export function usageWithTime(u: UsageView): string {
  const parts = hasUsage(u) ? [usageLine(u)] : [];
  if (u.elapsedSecs !== undefined && u.elapsedSecs > 0) parts.push(fmtElapsed(u.elapsedSecs));
  return parts.join(" · ");
}

/** 悬停说明：输入 / 输出拆分、缓存命中、估算与否、没单价时怎么补 */
export function usageTooltip(u: UsageView): string {
  const lines = [`输入 ${fmtTokens(u.inputTokens)} tok · 输出 ${fmtTokens(u.outputTokens)} tok`];
  if (u.cacheReadTokens) lines.push(`其中缓存命中 ${fmtTokens(u.cacheReadTokens)} tok`);
  if (u.estimated) lines.push("估算：服务方没有返回用量，按字数估的");
  if (u.costUsd === null) lines.push("当前模型没有设置单价，只显示 token（可在 AI 设置的「模型单价」里补上）");
  else lines.push(`花费 ≈${fmtUsd(u.costUsd)}（按设置里的单价折算，不含缓存折扣）`);
  if (u.elapsedSecs !== undefined && u.elapsedSecs > 0) lines.push(`用时 ${fmtElapsed(u.elapsedSecs)}`);
  return lines.join("\n");
}

// ———————————— 花费上限（美元 ↔ 微美元）————————————

/**
 * 「加预算继续」里的花费部分：表单留空（0）就不带这个字段 —— 后端沿用这一轮开局设的上限，
 * 「加预算继续」永远不会把花费上限撤掉；填了才按新值给。
 */
export function costGrant(maxCostUsd: number): { maxCostMicros?: number } {
  const m = usdToMicros(maxCostUsd);
  return m > 0 ? { maxCostMicros: m } : {};
}

/** 历史 / 卡头用的一行：没有 token 记录（旧记录）返回空串 */
export function spentLine(s: HostSpent | null | undefined): string {
  if (!s) return "";
  const u = fromHostSpent(s);
  return hasUsage(u) ? usageLine(u) : "";
}

/** 美元 → 微美元；非法 / 负数 → 0（= 不限） */
export function usdToMicros(usd: number): number {
  const v = Number(usd);
  if (!Number.isFinite(v) || v <= 0) return 0;
  return Math.round(v * 1_000_000);
}

/** 输入框文本 → 美元；空 / 非法 / 负数 → 0（= 不限），保留两位小数 */
export function parseCostInput(s: string): number {
  const v = Number(String(s).trim().replace(/^\$/, ""));
  if (!Number.isFinite(v) || v <= 0) return 0;
  return Math.round(v * 100) / 100;
}

// ———————————— 单价表（与后端 `usage::price_for` 同规则）————————————

/** 模型名里含 pattern（不分大小写）就命中；多条命中取 pattern 最长的；单价全 0 的行不算 */
export function priceFor(prices: readonly ModelPrice[] | null | undefined, model: string): ModelPrice | null {
  const m = model.trim().toLowerCase();
  if (!m) return null;
  let best: ModelPrice | null = null;
  for (const p of prices ?? []) {
    const pat = p.pattern.trim().toLowerCase();
    if (!pat || !m.includes(pat)) continue;
    if (!(p.inputPerM > 0 || p.outputPerM > 0)) continue;
    if (!best || pat.length > best.pattern.trim().length) best = p;
  }
  return best;
}

/** 规整一行单价（设置页 / IPC 边界）：非法数字 → 0 */
export function normalizePrice(raw: Partial<ModelPrice> | null | undefined): ModelPrice {
  const num = (v: unknown) => {
    const n = Number(v);
    return Number.isFinite(n) && n > 0 ? n : 0;
  };
  return {
    pattern: typeof raw?.pattern === "string" ? raw.pattern : "",
    inputPerM: num(raw?.inputPerM),
    outputPerM: num(raw?.outputPerM),
  };
}
