/**
 * 统一时间线（2.5）的前端纯逻辑。后端数据模型与合并见 `src-tauri/src/agent/timeline.rs`。
 *
 * 后端的事件流是**只追加**的：一条命令的输出摘要作为独立的 `output` 事件稍后追加（`refSeq` 指回命令），
 * 已发出的事件不会被改写。所以前端只需要：按 `seq` 去重合并、截到上限、把 output 挂回它的命令成行。
 * 这里全是不碰 React / Tauri 的纯函数，可以直接单测。
 */

export type TimelineActor = "user" | "agent";

export type TimelineKind =
  | "command"
  | "keys"
  | "prompt"
  | "output"
  | "secret"
  | "wait"
  | "screen"
  | "decision"
  | "file";

/** 与后端 `TimelineEvent`（camelCase）一一对应 */
export interface TimelineEvent {
  seq: number;
  tsMs: number;
  actor: TimelineActor;
  kind: TimelineKind;
  /** 命令行 / 按键描述 / 提示行 / 输出摘要。secret 恒为空。 */
  text: string;
  note?: string;
  /** output 事件指向它所属命令 / 动作的 seq */
  refSeq?: number;
  promptKind?: string;
  confidence?: number;
  truncated: boolean;
}

/** 前端最多留多少条（与后端环形上限同量级，防止长开面板时无限增长） */
export const MAX_TIMELINE_EVENTS = 512;

/**
 * 把新到的事件并进现有列表：按 `seq` 去重、保持 seq 升序、截到上限（丢最老的）。
 * 没有任何新事件时**原样返回 `prev`**（引用不变，React 可以据此跳过重渲染）。
 * 已有的事件对象引用保持不变，memo 行才不会被无谓重渲染。
 */
export function mergeEvents(prev: TimelineEvent[], incoming: TimelineEvent[]): TimelineEvent[] {
  if (incoming.length === 0) return prev;
  const have = new Set<number>();
  for (const e of prev) have.add(e.seq);
  const fresh = incoming.filter((e) => !have.has(e.seq));
  if (fresh.length === 0) return prev;
  const lastSeq = prev.length > 0 ? prev[prev.length - 1].seq : 0;
  let out: TimelineEvent[];
  if (fresh.every((e, i) => e.seq > lastSeq && (i === 0 || e.seq > fresh[i - 1].seq))) {
    out = prev.concat(fresh); // 常见路径：纯追加
  } else {
    out = prev.concat(fresh).sort((a, b) => a.seq - b.seq);
  }
  return out.length > MAX_TIMELINE_EVENTS ? out.slice(out.length - MAX_TIMELINE_EVENTS) : out;
}

/** 一行 = 一个主事件 + 挂在它名下的输出摘要（可能没有） */
export interface TimelineRow {
  ev: TimelineEvent;
  outputs: TimelineEvent[];
}

/**
 * 把平铺的事件流排成展示用的行：`output` 事件并回它的命令 / 动作；
 * 找不到所属事件的（所属的已被环形挤掉了，或没有 refSeq）单独成行。
 */
export function buildRows(events: TimelineEvent[]): TimelineRow[] {
  const rows: TimelineRow[] = [];
  const bySeq = new Map<number, TimelineRow>();
  for (const ev of events) {
    if (ev.kind === "output" && ev.refSeq !== undefined) {
      const owner = bySeq.get(ev.refSeq);
      if (owner) {
        owner.outputs.push(ev);
        continue;
      }
    }
    const row: TimelineRow = { ev, outputs: [] };
    rows.push(row);
    bySeq.set(ev.seq, row);
  }
  return rows;
}

/** 两行是否展示上等价（memo 比较用）：同一事件对象、同样的输出事件。 */
export function rowEqual(a: TimelineRow, b: TimelineRow): boolean {
  if (a.ev !== b.ev || a.outputs.length !== b.outputs.length) return false;
  for (let i = 0; i < a.outputs.length; i++) if (a.outputs[i] !== b.outputs[i]) return false;
  return true;
}

/** 是否贴着底部（容差内）。用户往上翻了就不该再被自动滚回去。 */
export function isNearBottom(scrollHeight: number, scrollTop: number, clientHeight: number, slack = 32): boolean {
  return scrollHeight - scrollTop - clientHeight <= slack;
}

/** `HH:MM:SS`（本地时区） */
export function formatClock(tsMs: number): string {
  const d = new Date(tsMs);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

export const ACTOR_LABEL: Record<TimelineActor, string> = { user: "用户", agent: "Agent" };

export const KIND_META: Record<TimelineKind, { icon: string; label: string }> = {
  command: { icon: "$", label: "命令" },
  keys: { icon: "⌨", label: "按键" },
  prompt: { icon: "?", label: "提示" },
  output: { icon: "↳", label: "输出" },
  secret: { icon: "•••", label: "密码" },
  wait: { icon: "…", label: "等待" },
  screen: { icon: "▣", label: "读屏" },
  decision: { icon: "◆", label: "托管决策" },
  file: { icon: "🗎", label: "文件" },
};

const PROMPT_LABEL: Record<string, string> = {
  password: "密码提示",
  yes_no: "是/否确认",
  pager: "分页器",
  menu: "菜单",
  repl: "交互解释器",
  shell_prompt: "Shell 提示符",
  unknown: "未知提示",
};

export function promptLabel(kind?: string): string {
  return (kind && PROMPT_LABEL[kind]) || "交互提示";
}

/** 一行主文案。secret 没有内容，要说清「这里发生过一次密码输入，但没记」。 */
export function describeEvent(ev: TimelineEvent): string {
  switch (ev.kind) {
    case "secret":
      return ev.actor === "agent" ? "请用户输入密码（内容不经过助手）" : "输入了密码 / 口令（内容未记录）";
    case "prompt":
      return ev.text ? `${promptLabel(ev.promptKind)}：${ev.text}` : promptLabel(ev.promptKind);
    case "wait":
      return ev.text ? `等待 ${ev.text}` : "等待";
    case "screen":
      return "读取当前屏幕";
    default:
      return ev.text;
  }
}

/** 输出摘要折叠态的一行说明：「5 行」+ 是否被截断 */
export function outputSummary(outputs: TimelineEvent[]): string {
  const lines = outputs.reduce((n, o) => n + (o.text ? o.text.split("\n").length : 0), 0);
  const cut = outputs.some((o) => o.truncated);
  return `输出 ${lines} 行${cut ? "（仅尾部）" : ""}`;
}
