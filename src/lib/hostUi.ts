/**
 * 托管页（StewardPanel / HostCard）的界面决策：纯函数，便于 node:test 直接测。
 * - 卡头上此刻该给哪几个按钮（每个相位只有一行，不再上下重复）
 * - 「契约 / 设置」折叠时的一行摘要
 * - 开始托管表单的预设（稳妥 / 自动 / YOLO）↔ 授权档位
 */

import type { HostPhase } from "../types";
import { basePhase, isLockedDeny, isTerminal } from "./host";
import { TIER_LABELS, type StewardTier } from "./steward";

// ———————————— 卡头动作 ————————————

export type HostHeadActionKind = "resume" | "handback" | "pause" | "takeover" | "stop";

export interface HostHeadAction {
  kind: HostHeadActionKind;
  label: string;
  /** primary = 这个相位下最该点的那一个；danger = 停止 */
  tone: "primary" | "plain" | "danger";
  title: string;
}

/**
 * 卡头上的按钮，按显示顺序：最该点的在前，「停止」永远在最后。
 * 等你回答 / 批准时卡头只给「接管 / 停止」——主动作是紧跟在卡头下面的那张卡；
 * 待重新接驳时不给（接驳卡里有「放弃」）；结束了也不给（报告里有「基于此报告继续」）。
 */
export function hostHeadActions(p: HostPhase): HostHeadAction[] {
  if (isTerminal(p)) return [];
  const b = basePhase(p);
  if (b.kind === "suspended") return [];
  const out: HostHeadAction[] = [];
  if (b.kind === "paused") {
    // 暂停时你又接手了终端：「继续」照样可点（交还只会回到暂停）
    const budget = b.cause.kind === "budget" || b.cause.kind === "deadline";
    out.push({
      kind: "resume",
      label: budget ? "加预算继续" : "继续",
      tone: "primary",
      title: "同一段上下文、换一份新预算接着做",
    });
    if (p.kind === "userDriving") {
      out.push({ kind: "handback", label: "交还", tone: "plain", title: "把终端交还给 AI（回到暂停状态）" });
    }
  } else if (p.kind === "userDriving") {
    out.push({ kind: "handback", label: "交还", tone: "primary", title: "把终端交还给 AI，它从你接手前的状态继续" });
  } else {
    if (p.kind === "running") {
      out.push({ kind: "pause", label: "暂停", tone: "plain", title: "当前这一步做完后暂停，终端还给你；之后可以「继续」" });
    }
    out.push({
      kind: "takeover",
      label: "接管",
      tone: "plain",
      title: "让 AI 立刻让路（你在终端里敲键也会自动触发），直到你点「交还」",
    });
  }
  out.push({ kind: "stop", label: "停止", tone: "danger", title: "结束这个托管（契约一并结束）" });
  return out;
}

// ———————————— 契约 / 设置摘要 ————————————

/** 摘要里用的短名（chip 上是长名）；底线项总在，不进摘要 */
const DENY_SHORT: Record<string, string> = {
  delete: "删除",
  force_push: "强推",
  file_write: "写文件",
};

export function denyShort(tag: string): string {
  return DENY_SHORT[tag] ?? `「${tag}」`;
}

/** 「契约 / 设置」收起时的一行：`自动执行 · 禁止：删除、强推`；yolo 档调用方标红 */
export function hostSettingsSummary(
  tier: string | null | undefined,
  deny: readonly string[] | null | undefined
): { tier: string; deny: string; yolo: boolean } {
  const t = tier && tier in TIER_LABELS ? TIER_LABELS[tier as StewardTier] : (tier ?? "");
  const extra = (deny ?? []).filter((d) => !isLockedDeny(d)).map(denyShort);
  return {
    tier: t,
    deny: extra.length > 0 ? `禁止：${extra.join("、")}` : "禁止：仅底线项",
    yolo: tier === "yolo",
  };
}

// ———————————— 开始表单：预设 ————————————

export interface HostPreset {
  tier: StewardTier;
  label: string;
  hint: string;
}

/** 每个授权档位一句话（预设按钮 / 档位下拉的提示） */
export const TIER_HINTS: Record<StewardTier, string> = {
  approve: "每次写入都先等你批准，AI 的问题也都交给你答",
  notify: "同「自动」，另外每次写入都发通知卡，可接管并纠正",
  auto: "范围内自主执行、自主作答，只有不可逆的操作才停下来问你；事后可查",
  yolo: "一切自主执行，包括不可逆的操作（格式化、重启、删库……）；只有越过禁止项才停下来问你。替你输密码、选「永久允许」仍然不会做",
};

/** 开始表单上的主选择（「通知后执行」档留在「更多设置」里的完整档位下拉） */
export const HOST_PRESETS: readonly HostPreset[] = [
  { tier: "approve", label: "稳妥", hint: TIER_HINTS.approve },
  { tier: "auto", label: "自动", hint: TIER_HINTS.auto },
  { tier: "yolo", label: "YOLO·完全放手", hint: TIER_HINTS.yolo },
];

/** 完整档位（「更多设置」里的下拉、托管卡上的改档） */
export const HOST_TIERS: readonly StewardTier[] = ["approve", "notify", "auto", "yolo"];

export const DEFAULT_HOST_TIER: StewardTier = "auto";
/** 开始表单上选的档位（预设），记在本机 */
export const LS_HOST_TIER = "nx.host.tier";

/** 读本机存的档位：不认识的回落默认「自动」 */
export function parseHostTier(raw: string | null | undefined): StewardTier {
  return (HOST_TIERS as readonly string[]).includes(raw ?? "") ? (raw as StewardTier) : DEFAULT_HOST_TIER;
}

/** 这个档位对应哪个预设；「通知后执行」不是预设 → null */
export function presetOfTier(tier: string): HostPreset | null {
  return HOST_PRESETS.find((p) => p.tier === tier) ?? null;
}
