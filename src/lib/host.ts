/**
 * 统一托管（host_*）的纯逻辑：相位 → 胶囊文案、当前要处理的卡片、哪些按钮可点、按 console 归并。
 * 不碰 Tauri，便于 node:test 直接测。
 */

import type { PauseKind } from "./agentBudget";
import type { HostHumanCard, HostPhase, HostStatus } from "../types";

export type HostPillTone = "active" | "warn" | "idle" | "done" | "bad";

export interface HostPill {
  label: string;
  tone: HostPillTone;
}

/** 剥掉（可能嵌套的）userDriving，得到人交还后会回到的那个状态 */
export function basePhase(p: HostPhase): HostPhase {
  return p.kind === "userDriving" ? basePhase(p.resume_to) : p;
}

export function isTerminal(p: HostPhase): boolean {
  return p.kind === "completed" || p.kind === "failed" || p.kind === "stopped";
}

/** 每个被托管 console 的状态胶囊 */
export function hostPill(p: HostPhase): HostPill {
  switch (p.kind) {
    case "running":
      return { label: "运行中", tone: "active" };
    case "waitingHuman":
      return { label: p.card.kind.kind === "approve" ? "等你批准" : "等你回答", tone: "warn" };
    case "userDriving":
      return { label: "你在驾驶", tone: "warn" };
    case "paused":
      switch (p.cause.kind) {
        case "budget":
        case "deadline":
          return { label: "预算暂停", tone: "warn" };
        case "llmDown":
          return { label: "暂停（模型不可用）", tone: "warn" };
        default:
          return { label: "已暂停", tone: "warn" };
      }
    case "suspended":
      return { label: "待重新接驳", tone: "warn" };
    case "completed":
      return { label: "完成", tone: "done" };
    case "failed":
      return { label: "失败", tone: "bad" };
    case "stopped":
      return { label: "已停止", tone: "idle" };
  }
}

/** 暂停 / 结束的一行说明（胶囊旁边的小字） */
export function hostDetail(p: HostPhase): string | null {
  const b = basePhase(p);
  if (p.kind === "userDriving") {
    return b.kind === "running"
      ? "你在操作终端，AI 已让路；点「交还」它就接着做"
      : `你在操作终端；交还后回到「${hostPill(b).label}」`;
  }
  switch (b.kind) {
    case "paused":
      switch (b.cause.kind) {
        case "budget":
          return `${b.cause.reason}。点「加预算继续」恢复，或「停止」结束。`;
        case "deadline":
          return "到了约定的期限。点「加预算继续」恢复，或「停止」结束。";
        case "llmDown":
          return `模型服务暂不可用（${b.cause.error}）。恢复后点「加预算继续」。`;
        default:
          return "已暂停，点「加预算继续」接着做。";
      }
    case "completed":
      return b.summary;
    case "failed":
      return b.error;
    default:
      return null;
  }
}

/** 当前等人处理的卡片（人在驾驶时也照样显示：答复会先替他交还） */
export function pendingCard(p: HostPhase): HostHumanCard | null {
  const b = basePhase(p);
  return b.kind === "waitingHuman" ? b.card : null;
}

export interface HostButtons {
  takeover: boolean;
  handback: boolean;
  resume: boolean;
  pause: boolean;
  stop: boolean;
}

/** 哪些按钮此刻有意义 */
export function hostButtons(p: HostPhase): HostButtons {
  const b = basePhase(p);
  const live = !isTerminal(p);
  return {
    takeover: live && p.kind !== "userDriving",
    handback: p.kind === "userDriving",
    resume: b.kind === "paused",
    pause: p.kind === "running",
    stop: live,
  };
}

/** 暂停原因映射到旧的 PauseKind（托管页那条预算横幅沿用它） */
export function hostPauseKind(p: HostPhase): PauseKind | undefined {
  const b = basePhase(p);
  if (b.kind !== "paused") return undefined;
  switch (b.cause.kind) {
    case "budget":
      return "steps";
    case "deadline":
      return "time";
    case "llmDown":
      return "llm";
    default:
      return "user";
  }
}

/** 收一条状态进总表（按 contractId） */
export function upsertHost(map: Record<string, HostStatus>, st: HostStatus): Record<string, HostStatus> {
  return { ...map, [st.contractId]: st };
}

/**
 * 某个 console 的托管：优先未结束的；都结束了就取 contractId 最大（最新）的那份结局。
 * contractId 形如 `steward-{console}-{ms}`，同一 console 下按字符串比较即按时间。
 */
export function hostForConsole(
  map: Record<string, HostStatus>,
  consoleId: string
): HostStatus | null {
  const mine = Object.values(map).filter((s) => s.consoleId === consoleId);
  if (mine.length === 0) return null;
  const live = mine.find((s) => !isTerminal(s.phase));
  if (live) return live;
  return mine.sort((a, b) => (a.contractId < b.contractId ? 1 : -1))[0];
}
