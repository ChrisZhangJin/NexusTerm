/**
 * 统一托管（host_*）的纯逻辑：相位 → 胶囊文案、当前要处理的卡片、哪些按钮可点、按 console 归并。
 * 不碰 Tauri，便于 node:test 直接测。
 */

import type { PauseKind } from "./agentBudget";
import type { HostCheck, HostCheckResult, HostConsoleHint, HostHumanCard, HostPhase, HostStatus, TerminalTab } from "../types";

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
      switch (p.card.kind.kind) {
        case "approve":
          return { label: "等你批准", tone: "warn" };
        case "secret":
          return { label: "等你输入密码", tone: "warn" };
        case "criteriaDraft":
          return { label: "等你确认判据", tone: "warn" };
        case "manualCheck":
          return { label: "等你确认完成", tone: "warn" };
        case "watchdog":
          return { label: "看门狗·等你看看", tone: "warn" };
        default:
          return { label: "等你回答", tone: "warn" };
      }
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
      return { label: p.reason === "consoleGone" ? "终端已关闭·待接驳" : "待重新接驳", tone: "warn" };
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
  /** 待重新接驳：显示候选 / 打开新连接 */
  reattach: boolean;
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
  const suspended = b.kind === "suspended";
  return {
    takeover: live && !suspended && p.kind !== "userDriving",
    handback: p.kind === "userDriving",
    resume: b.kind === "paused",
    pause: p.kind === "running",
    stop: live,
    reattach: suspended,
  };
}

/** 等重新接驳（重启后 / 终端关了） */
export const needsReattach = (p: HostPhase): boolean => basePhase(p).kind === "suspended";

/** 一个 tab 的 console 线索：重启后后端按它（SSH 记录 → 标签 → cwd）给候选排序 */
export function consoleHintOf(tab: TerminalTab): HostConsoleHint {
  const nz = (s: string | null | undefined) => (s && s.trim() ? s.trim() : null);
  return {
    consoleId: tab.id,
    label: nz(tab.customTitle) ?? nz(tab.title) ?? tab.id,
    sessionRecordId: tab.local ? null : nz(tab.sessionId),
    cwd: nz(tab.ctx?.cwd),
    tmux: nz(tab.ctx?.tmux),
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

// ——— 完成判据：草稿卡里按「一行一条」编辑 ———

const CHECK_PREFIX: Array<[HostCheck["kind"], string]> = [
  ["commandOk", "命令"],
  ["fileExists", "文件"],
  ["screenMatches", "屏幕"],
  ["manual", "人工"],
];

/** 一条判据 → 一行「前缀: 内容」 */
export function checkToLine(c: HostCheck): string {
  switch (c.kind) {
    case "commandOk":
      return `命令: ${c.cmd}`;
    case "fileExists":
      return `文件: ${c.path}`;
    case "screenMatches":
      return `屏幕: ${c.re}`;
    case "manual":
      return `人工: ${c.text}`;
  }
}

export const checksToText = (cs: HostCheck[]): string => cs.map(checkToLine).join("\n");

/** 编辑后的文本 → 判据。空行忽略；认不出前缀的整行当人工确认项。 */
export function textToChecks(text: string): HostCheck[] {
  const out: HostCheck[] = [];
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (!line) continue;
    const m = /^(命令|文件|屏幕|人工)\s*[:：]\s*(.*)$/.exec(line);
    const kind = m ? CHECK_PREFIX.find(([, p]) => p === m[1])?.[0] : undefined;
    const body = (m ? m[2] : line).trim();
    if (!body) continue;
    switch (kind) {
      case "commandOk":
        out.push({ kind, cmd: body });
        break;
      case "fileExists":
        out.push({ kind, path: body });
        break;
      case "screenMatches":
        out.push({ kind, re: body });
        break;
      default:
        out.push({ kind: "manual", text: body });
    }
  }
  return out;
}

/** 一条判据的人话（与后端 `Check::describe` 同口径） */
export function describeCheck(c: HostCheck): string {
  switch (c.kind) {
    case "screenMatches":
      return `屏幕匹配 /${c.re}/`;
    case "commandOk":
      return `命令成功：\`${c.cmd}\``;
    case "fileExists":
      return `文件存在：${c.path}`;
    case "manual":
      return `人工确认：${c.text}`;
  }
}

/** 完成卡上的逐条证据（✓ / ✗ + 判据 + 证据） */
export function checkLines(results: HostCheckResult[]): string[] {
  return results.map((r) => `${r.passed ? "✓" : "✗"} ${describeCheck(r.check)}：${r.detail}`);
}
