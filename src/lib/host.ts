/**
 * 统一托管（host_*）的纯逻辑：相位 → 胶囊文案、当前要处理的卡片、按 console 归并（卡头按钮见 hostUi）。
 * 不碰 Tauri，便于 node:test 直接测。
 */

import type {
  AgentStep,
  HostCheck,
  HostCheckResult,
  HostConsoleHint,
  HostHumanCard,
  HostPhase,
  HostStatus,
  TerminalTab,
} from "../types";
import { t } from "../i18n";

export type HostPillTone = "active" | "warn" | "idle" | "done" | "bad";

export interface HostPill {
  label: string;
  tone: HostPillTone;
  /** 文案里已经说了「在等你」（标签徽标不再加前缀） */
  waiting?: boolean;
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
      return { label: t("运行中", "Running"), tone: "active" };
    case "waitingHuman":
      switch (p.card.kind.kind) {
        case "approve":
          return { label: t("等你批准", "Waiting for your approval"), tone: "warn", waiting: true };
        case "secret":
          return { label: t("等你输入密码", "Waiting for your password"), tone: "warn", waiting: true };
        case "criteriaDraft":
          return { label: t("等你确认判据", "Confirm the completion checks"), tone: "warn", waiting: true };
        case "manualCheck":
          return { label: t("等你确认完成", "Confirm it's done"), tone: "warn", waiting: true };
        case "watchdog":
          return { label: t("看门狗·等你看看", "Watchdog · please take a look"), tone: "warn", waiting: true };
        default:
          return { label: t("等你回答", "Waiting for your answer"), tone: "warn", waiting: true };
      }
    case "userDriving":
      return { label: t("你在驾驶", "You're driving"), tone: "warn" };
    case "paused":
      switch (p.cause.kind) {
        case "budget":
        case "deadline":
          return { label: t("预算暂停", "Paused · budget"), tone: "warn" };
        case "llmDown":
          return { label: t("暂停（模型不可用）", "Paused (model unavailable)"), tone: "warn" };
        default:
          return { label: t("已暂停", "Paused"), tone: "warn" };
      }
    case "suspended":
      return { label: p.reason === "consoleGone" ? t("终端断开·等重连", "Terminal lost · waiting to reconnect") : t("待重新接驳", "Needs reattaching"), tone: "warn" };
    case "completed":
      return { label: t("完成", "Done"), tone: "done" };
    case "failed":
      return { label: t("失败", "Failed"), tone: "bad" };
    case "stopped":
      return { label: t("已停止", "Stopped"), tone: "idle" };
  }
}

/** 标签徽标文字最多多少字（tooltip 里看一眼就够） */
const TAB_BADGE_MAX = 60;

/**
 * 标签上的「等你」徽标：托管在等人（卡片 / 预算·期限·模型暂停 / 挂起）时给一行字，否则 null。
 * 人在驾驶时不显示（人就在终端前）；用户自己点的暂停、已结束的托管也不显示。
 */
export function hostTabBadge(p: HostPhase): string | null {
  if (p.kind === "userDriving") return null;
  // 胶囊文案里已经带「等你」的（等你批准 / 看门狗·等你看看）不再重复前缀
  const head = (pill: HostPill) => (pill.waiting ? pill.label : t("等你：{label}", "Waiting for you: {label}", { label: pill.label }));
  switch (p.kind) {
    case "waitingHuman": {
      const q = clip(p.card.question.replace(/\s+/g, " ").trim());
      const k = p.card.kind.kind;
      if (k === "ask") return q ? t("等你：{q}", "Waiting for you: {q}", { q }) : t("等你回答", "Waiting for your answer");
      const label = head(hostPill(p));
      return q && k !== "criteriaDraft" ? t("{label}：{q}", "{label}: {q}", { label, q }) : label;
    }
    case "paused":
      return p.cause.kind === "user" ? null : head(hostPill(p));
    case "suspended":
      return head(hostPill(p));
    default:
      return null;
  }
}

function clip(text: string): string {
  const chars = Array.from(text);
  return chars.length > TAB_BADGE_MAX ? chars.slice(0, TAB_BADGE_MAX).join("") + "…" : text;
}

/** console id → 徽标文字（只含正在等人的托管） */
export function hostTabBadges(map: Record<string, HostStatus>): Record<string, string> {
  const out: Record<string, string> = {};
  for (const st of Object.values(map)) {
    const b = hostTabBadge(st.phase);
    if (!b) continue;
    // 多终端托管：它的每个终端都挂徽标（未结束的优先，不被同一终端上已结束的旧记录盖掉）
    for (const id of hostConsoleIds(st)) {
      if (!out[id] || !isTerminal(st.phase)) out[id] = b;
    }
  }
  return out;
}

/** 暂停 / 结束的一行说明（胶囊旁边的小字） */
export function hostDetail(p: HostPhase): string | null {
  const b = basePhase(p);
  if (p.kind === "userDriving") {
    return b.kind === "running"
      ? t("你在操作终端，AI 已让路；点「交还」它就接着做", "You're using the terminal and the AI has stepped aside; click Hand back to let it continue")
      : t("你在操作终端；交还后回到「{label}」", "You're using the terminal; Hand back returns to \"{label}\"", { label: hostPill(b).label });
  }
  switch (b.kind) {
    case "paused":
      switch (b.cause.kind) {
        case "budget":
          return t("{reason}。点「加预算继续」恢复，或「停止」结束。", "{reason}. Click Add budget & continue to resume, or Stop to end.", { reason: b.cause.reason });
        case "deadline":
          return t("到了约定的期限。点「加预算继续」恢复，或「停止」结束。", "The agreed deadline has passed. Click Add budget & continue to resume, or Stop to end.");
        case "llmDown":
          return t("模型服务暂不可用（{error}）。恢复后点「继续」。", "The model service is unavailable ({error}). Click Resume once it's back.", { error: b.cause.error });
        default:
          return t("已暂停，点「继续」接着做。", "Paused. Click Resume to continue.");
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

/** 收一条状态进总表（按 contractId） */
export function upsertHost(map: Record<string, HostStatus>, st: HostStatus): Record<string, HostStatus> {
  return { ...map, [st.contractId]: st };
}

/** 一份托管操作的全部终端 id（主终端在前）；旧后端没有 `consoles` 时只有 consoleId */
export function hostConsoleIds(st: HostStatus): string[] {
  const ids = (st.consoles ?? []).map((c) => c.consoleId);
  return ids.includes(st.consoleId) ? ids : [st.consoleId, ...ids];
}

/** 托管是否涉及这个终端（主终端或其余终端之一） */
export const hostHasConsole = (st: HostStatus, consoleId: string): boolean =>
  hostConsoleIds(st).includes(consoleId);

/** 多终端托管里此刻不在的终端（等重新接驳的那几个） */
export const missingConsoles = (st: HostStatus) => (st.consoles ?? []).filter((c) => c.missing);

/**
 * 某个 console 的托管：优先未结束的；都结束了就取 contractId 最大（最新）的那份结局。
 * contractId 形如 `steward-{console}-{ms}`，同一 console 下按字符串比较即按时间。
 */
export function hostForConsole(
  map: Record<string, HostStatus>,
  consoleId: string
): HostStatus | null {
  // 多终端托管：它的任何一个终端上都能看到它（托管卡 / 实况 / 胶囊）
  const mine = Object.values(map).filter((s) => hostHasConsole(s, consoleId));
  if (mine.length === 0) return null;
  const live = mine.find((s) => !isTerminal(s.phase));
  if (live) return live;
  return mine.sort((a, b) => (a.contractId < b.contractId ? 1 : -1))[0];
}

// ——— 完成判据：草稿卡里按「一行一条」编辑 ———

const CHECK_KINDS: HostCheck["kind"][] = ["commandOk", "fileExists", "screenMatches", "manual"];

/** 每类判据的行前缀：中文 / 英文两套，编辑框里两种都认（切了语言之后旧文本照样能解析） */
const CHECK_PREFIXES: Record<HostCheck["kind"], [string, string]> = {
  commandOk: ["命令", "Command"],
  fileExists: ["文件", "File"],
  screenMatches: ["屏幕", "Screen"],
  manual: ["人工", "Manual"],
};

const checkPrefix = (kind: HostCheck["kind"]): string => t(...CHECK_PREFIXES[kind]);

/** 一条判据 → 一行「前缀: 内容」 */
export function checkToLine(c: HostCheck): string {
  const p = checkPrefix(c.kind);
  switch (c.kind) {
    case "commandOk":
      return `${p}: ${c.cmd}`;
    case "fileExists":
      return `${p}: ${c.path}`;
    case "screenMatches":
      return `${p}: ${c.re}`;
    case "manual":
      return `${p}: ${c.text}`;
  }
}

export const checksToText = (cs: HostCheck[]): string => cs.map(checkToLine).join("\n");

/** 编辑后的文本 → 判据。空行忽略；认不出前缀的整行当人工确认项。 */
export function textToChecks(text: string): HostCheck[] {
  const out: HostCheck[] = [];
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (!line) continue;
    const m = /^(命令|文件|屏幕|人工|command|file|screen|manual)\s*[:：]\s*(.*)$/i.exec(line);
    const head = m?.[1].toLowerCase();
    const kind = head ? CHECK_KINDS.find((k) => CHECK_PREFIXES[k].some((p) => p.toLowerCase() === head)) : undefined;
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
      return t("屏幕匹配 /{re}/", "Screen matches /{re}/", { re: c.re });
    case "commandOk":
      return t("命令成功：`{cmd}`", "Command succeeds: `{cmd}`", { cmd: c.cmd });
    case "fileExists":
      return t("文件存在：{path}", "File exists: {path}", { path: c.path });
    case "manual":
      return t("人工确认：{text}", "Manual check: {text}", { text: c.text });
  }
}

/**
 * 完成报告的显示文本。模型偶尔把整份报告挤成一段（「…。1. xxx 2. xxx 建议先…」）——
 * 原文里一个换行都没有时，在编号条目（`1.` `2、`）与「建议」前补换行；已经分行的原样返回。
 */
export function formatReport(summary: string): string {
  const s = summary.trim();
  if (!s || s.includes("\n")) return s;
  return s
    .replace(/\s+(?=\d{1,2}[.、](?!\d)\s?\S)/g, "\n")
    .replace(/[。；;]\s*(?=\d{1,2}[.、](?!\d)\s?\S)/g, (m) => m.trimEnd() + "\n")
    .replace(/\s*(?=建议[：:先])/g, "\n")
    .replace(/^\n+/, "");
}

/** 完成卡上的逐条证据（✓ 通过 / ✗ 没过 / · 人工项，不阻塞完成 + 判据 + 证据） */
export function checkLines(results: HostCheckResult[]): string[] {
  return results.map((r) => {
    const mark = r.check.kind === "manual" || r.skipped ? "·" : r.passed ? "✓" : "✗";
    return t("{mark} {check}：{detail}", "{mark} {check}: {detail}", { mark, check: describeCheck(r.check), detail: r.detail });
  });
}

// ———————————— 契约禁止项 ————————————

/** 后端总会补回、面板上只读的底线禁止项（与 policy.rs 的 LOCKED_DENY 一致） */
export const LOCKED_DENY = ["secret", "permanent_grant"] as const;

/** 开始托管时默认的禁止项（与后端 Authority::default 一致） */
export const DEFAULT_DENY = ["secret", "permanent_grant", "force_push", "delete"];

const denyLabels = (): Record<string, string> => ({
  delete: t("删除类操作（rm / prune / drop …）", "Deletions (rm / prune / drop …)"),
  force_push: t("强制推送（git push -f）", "Force push (git push -f)"),
  secret: t("替你输密码", "Typing your passwords"),
  permanent_grant: t("选「不再询问 / 永久允许」", "Choosing \"don't ask again / always allow\""),
});

export function isLockedDeny(tag: string): boolean {
  return (LOCKED_DENY as readonly string[]).includes(tag);
}

/** 面板上显示的名字；自定义关键字原样显示 */
export function denyLabel(tag: string): string {
  return denyLabels()[tag] ?? t("含「{tag}」", "Contains \"{tag}\"", { tag });
}

/** 禁止项的说明（chip 悬停提示）：内置项具体管什么，别让人以为「生成密钥 / 写配置」也被禁了 */
const denyHints = (): Record<string, string> => ({
  secret: t(
    "底线，不能放开（契约里叫 secret）：只指屏幕停在密码提示上时，AI 不替你输入密码（要你本人输入）。生成密钥、写带密码的配置文件都不受它限制",
    "Hard limit, can't be lifted (\"secret\" in the contract): when the screen stops at a password prompt, the AI won't type the password for you — you enter it yourself. Generating keys or writing config files that contain passwords is not affected"
  ),
  permanent_grant: t(
    "底线，不能放开（契约里叫 permanent_grant）：AI 不会在授权框里选「不再询问 / 永久允许」",
    "Hard limit, can't be lifted (\"permanent_grant\" in the contract): the AI never picks \"don't ask again / always allow\" in permission dialogs"
  ),
  force_push: t("git push --force 一类改写远端历史的推送。点 ✕ 放开", "Pushes that rewrite remote history, like git push --force. Click ✕ to allow"),
  delete: t("删除类操作：rm / prune / drop / kubectl delete 等。点 ✕ 放开", "Deletions: rm / prune / drop / kubectl delete, etc. Click ✕ to allow"),
  file_write: t("AI 用文件工具写 / 改文件。点 ✕ 放开", "The AI writing or editing files with its file tools. Click ✕ to allow"),
});

export function denyHint(tag: string): string {
  return denyHints()[tag] ?? t("命令或写入的内容里出现「{tag}」就拒绝。点 ✕ 放开", "Refuse any command or write containing \"{tag}\". Click ✕ to allow", { tag });
}

/** 门禁卡（kind:"gate"）上能做什么 */
export interface GateActions {
  /** 「这是误判」：只有看过屏幕、被拒 / 要批准的卡才有 */
  misread: boolean;
  /** 「放开这一类」后的禁止项列表；命中的不是可放开的契约禁止项（或已经放开）时为 null */
  release: string[] | null;
  /** 命中的禁止项给人看的名字与说明（没命中禁止项时为 null） */
  item: { label: string; hint: string } | null;
}

/** 门禁卡上的按钮：纯函数，`deny` 是契约当前的禁止项 */
export function gateActions(step: AgentStep, deny: readonly string[] | null | undefined): GateActions {
  const misread = step.gateVerdict !== "notified" && !!step.gateId;
  const tag = step.denyItem;
  if (!tag) return { misread, release: null, item: null };
  const item = { label: denyLabel(tag), hint: denyHint(tag) };
  const locked = step.denyLocked || isLockedDeny(tag);
  const list = deny ?? [];
  const release = !locked && step.gateVerdict === "refused" && list.includes(tag) ? removeDeny(list, tag) : null;
  return { misread, release, item };
}

/** 去掉一项（底线项去不掉） */
export function removeDeny(list: readonly string[], tag: string): string[] {
  if (isLockedDeny(tag)) return [...list];
  return list.filter((t) => t !== tag);
}

/** 加一项：去空白、去重（不分大小写）；底线项总在 */
export function addDeny(list: readonly string[], raw: string): string[] {
  const t = raw.trim();
  const base = [...new Set([...LOCKED_DENY, ...list])];
  if (!t || base.some((x) => x.toLowerCase() === t.toLowerCase())) return base;
  return [...base, t];
}

/** 判据文本的分类计数（确认卡上的「命令 2 · 屏幕 1 · 人工 1」），只列出现过的类，按固定顺序。 */
export function criteriaSummary(text: string): Array<{ label: string; n: number; blocking: boolean }> {
  const cs = textToChecks(text);
  return CHECK_KINDS.map((kind) => ({
    label: checkPrefix(kind),
    n: cs.filter((c) => c.kind === kind).length,
    blocking: kind !== "manual",
  })).filter((x) => x.n > 0);
}

// ———————————— 卡片正文：段落 / 列表 / 行内代码 ————————————

export type TextBlock = { kind: "p"; text: string } | { kind: "ul" | "ol"; items: string[] };

/** 模型把换行双重转义时的字面 `\n`（与后端 `unescape_newlines` 同一规则：有真换行就不动） */
export function unescapeNewlines(s: string): string {
  if (s.includes("\n") || !s.includes("\\n")) return s;
  return s.replace(/\\r\\n/g, "\n").replace(/\\n/g, "\n").replace(/\\t/g, "  ");
}

const BULLET = /^\s*[-•*·]\s+(.*)$/;
const ORDERED = /^\s*\d{1,2}[.、)]\s*(.*)$/;

/** 把 AI 写给人的一段话切成块：空行分段；`- ` / `1. ` 开头的连续行是列表。 */
export function textBlocks(raw: string): TextBlock[] {
  const out: TextBlock[] = [];
  let para: string[] = [];
  const flush = () => {
    if (para.length) out.push({ kind: "p", text: para.join("\n") });
    para = [];
  };
  for (const line of unescapeNewlines(raw.trim()).split("\n")) {
    const b = BULLET.exec(line);
    const o = b ? null : ORDERED.exec(line);
    const item = b?.[1] ?? o?.[1];
    if (item !== undefined) {
      flush();
      const kind = b ? "ul" : "ol";
      const last = out[out.length - 1];
      if (last && last.kind === kind) last.items.push(item.trim());
      else out.push({ kind, items: [item.trim()] });
    } else if (!line.trim()) {
      flush();
    } else {
      para.push(line.trim());
    }
  }
  flush();
  return out;
}

export type InlinePart = { code: boolean; text: string };

/** 行内：反引号里的、以及路径（`/etc/x`、`~/a/b`）显示成代码样式 */
export function inlineParts(s: string): InlinePart[] {
  const out: InlinePart[] = [];
  const re = /`([^`]+)`|((?:~|\/)[\w.@-]*(?:\/[\w.@-]+)+\/?)/g;
  let at = 0;
  for (let m = re.exec(s); m; m = re.exec(s)) {
    if (m.index > at) out.push({ code: false, text: s.slice(at, m.index) });
    out.push({ code: true, text: m[1] ?? m[2] });
    at = m.index + m[0].length;
  }
  if (at < s.length) out.push({ code: false, text: s.slice(at) });
  return out;
}
