/**
 * Skill 编辑器的表单状态与转换（Phase 3.4）。纯逻辑，便于 `tests/fe/skillform.test.ts` 钉住。
 *
 * 为什么不直接编辑 `SkillInput`：
 * - 输入框要的是字符串（超时填一半、默认值清空），而 `SkillInput` 里是 `number | undefined`；
 * - 步骤需要稳定的 key（上移 / 删除时 React 不能串行），参数改名时也要保持行身份；
 * - 切换步骤种类时保留已填的文字（点错了再切回来不丢内容），所以步骤是「所有种类字段的并集」。
 *
 * 规则的权威实现（校验、风险评估）在 Rust `agent::skill`，这里只做表单必需的最小本地检查
 * （超时是不是数字），其余问题交给 `skill_validate` / 创建 / 更新返回，再用 [`groupProblems`]
 * 归到对应的步骤 / 参数上逐条显示。
 */

import type {
  Skill,
  SkillCheck,
  SkillHostScope,
  SkillInput,
  SkillOnUnexpected,
  SkillParam,
  SkillRisk,
  SkillSource,
  SkillStep,
} from "../types";
import { undeclaredParams } from "./skill";

// ——————————————————————————————— 类型 ———————————————————————————————

export type StepKind = SkillStep["kind"];

export interface ParamForm {
  /** 行身份（改名不变），仅用作 React key */
  uid: number;
  name: string;
  description: string;
  /** 空串 = 没有默认值 */
  defaultValue: string;
  required: boolean;
  secret: boolean;
}

/** 所有步骤种类字段的并集；只有 `kind` 对应的那几个会进入 [`toInput`] 的结果 */
export interface StepForm {
  uid: number;
  kind: StepKind;
  command: string;
  keys: string;
  expect: string;
  respond: string;
  /** expect_respond 的 secret 开关 */
  respondSecret: boolean;
  reason: string;
  /** secret 步骤引用的 secret 参数名，空串 = 不引用 */
  param: string;
  /** 空串 = 不设 */
  timeoutMs: string;
  expectAfter: string;
  onUnexpected: SkillOnUnexpected;
}

export interface SkillForm {
  name: string;
  description: string;
  params: ParamForm[];
  steps: StepForm[];
  verifyOn: boolean;
  verifyExpect: string;
  verifyTimeoutMs: string;
  /** 只读：编辑器不让改，保存时原样带上，后端只会把它抬高 */
  risk: SkillRisk;
  hostMode: "any" | "list";
  /** 每行 / 逗号 / 空白分隔一项，见 [`hostsFromText`] */
  hostsText: string;
  source: SkillSource;
}

// ——————————————————————————————— 常量 ———————————————————————————————

export const MIN_TIMEOUT_MS = 100;
export const MAX_TIMEOUT_MS = 600_000;

export const ON_UNEXPECTED_LABEL: Record<SkillOnUnexpected, string> = {
  ask: "问我",
  stop: "停止",
  llm: "交给 AI",
};

export const SOURCE_LABEL: Record<SkillSource, string> = {
  recorded: "录制",
  migrated_macro: "旧宏迁移",
  manual: "手写",
  imported: "导入",
};

export const STEP_KINDS: StepKind[] = ["exec", "keys", "expect_respond", "secret", "screen_check"];

// ——————————————————————————————— 构造 ———————————————————————————————

let uidSeq = 1;
const nextUid = () => uidSeq++;

export function newParam(name = ""): ParamForm {
  return { uid: nextUid(), name, description: "", defaultValue: "", required: false, secret: false };
}

export function newStep(kind: StepKind = "exec"): StepForm {
  return {
    uid: nextUid(),
    kind,
    command: "",
    keys: "",
    expect: "",
    respond: "",
    respondSecret: false,
    reason: "",
    param: "",
    timeoutMs: "",
    expectAfter: "",
    onUnexpected: "ask",
  };
}

/** 复制一步（新 uid） */
export function cloneStep(s: StepForm): StepForm {
  return { ...s, uid: nextUid() };
}

// ——————————————————————————————— SkillInput ⇄ SkillForm ———————————————————————————————

/** 库里的 skill → 可编辑的输入（去掉 id / 计数 / 时间戳；来源保留，更新时后端会忽略它） */
export function skillToInput(s: Skill): SkillInput {
  return {
    name: s.name,
    description: s.description,
    params: s.params,
    steps: s.steps,
    verify: s.verify,
    risk: s.risk,
    hostScope: s.hostScope,
    source: s.source,
  };
}

const numStr = (n: number | undefined) => (n === undefined ? "" : String(n));

function stepToForm(s: SkillStep): StepForm {
  const f = newStep(s.kind);
  f.timeoutMs = numStr(s.timeoutMs);
  f.expectAfter = s.expectAfter ?? "";
  f.onUnexpected = s.onUnexpected ?? "ask";
  switch (s.kind) {
    case "exec":
      f.command = s.command;
      break;
    case "keys":
      f.keys = s.keys;
      break;
    case "expect_respond":
      f.expect = s.expect;
      f.respond = s.respond;
      f.respondSecret = s.secret;
      break;
    case "secret":
      f.reason = s.reason;
      f.param = s.param ?? "";
      break;
    case "screen_check":
      f.expect = s.expect;
      break;
  }
  return f;
}

/** 数据里带着「secret 参数有默认值」的参数名（草稿 / 手改过的导入文件才会有；UI 不允许产生）。 */
export function secretDefaultsIn(input: Pick<SkillInput, "params">): string[] {
  return input.params.filter((p) => p.secret && p.default !== undefined && p.default !== "").map((p) => p.name);
}

/**
 * 载入表单。secret 参数的默认值**一律丢弃**（secret 参数不得有默认值，也不该把可能是口令的字面值
 * 留在界面里）；调用方可用 [`secretDefaultsIn`] 提示用户有东西被丢了。
 */
export function toForm(input: SkillInput): SkillForm {
  return {
    name: input.name,
    description: input.description,
    params: input.params.map((p) => ({
      uid: nextUid(),
      name: p.name,
      description: p.description,
      defaultValue: p.secret ? "" : (p.default ?? ""),
      required: p.required,
      secret: p.secret,
    })),
    steps: input.steps.map(stepToForm),
    verifyOn: input.verify !== null,
    verifyExpect: input.verify?.expect ?? "",
    verifyTimeoutMs: numStr(input.verify?.timeoutMs),
    risk: input.risk,
    hostMode: input.hostScope.kind === "list" ? "list" : "any",
    hostsText: input.hostScope.kind === "list" ? hostsToText(input.hostScope.hosts) : "",
    source: input.source ?? "manual",
  };
}

export type TimeoutParse = { ok: true; value: number | undefined } | { ok: false };

/** 空串 = 不设；其余必须是 100..=600000 的整数 */
export function parseTimeout(s: string): TimeoutParse {
  const t = s.trim();
  if (t === "") return { ok: true, value: undefined };
  if (!/^\d+$/.test(t)) return { ok: false };
  const n = Number(t);
  if (n < MIN_TIMEOUT_MS || n > MAX_TIMEOUT_MS) return { ok: false };
  return { ok: true, value: n };
}

function stepToInput(f: StepForm): SkillStep {
  const timeout = parseTimeout(f.timeoutMs);
  const common = {
    ...(timeout.ok && timeout.value !== undefined ? { timeoutMs: timeout.value } : {}),
    ...(f.expectAfter.trim() ? { expectAfter: f.expectAfter } : {}),
    ...(f.onUnexpected !== "ask" ? { onUnexpected: f.onUnexpected } : {}),
  };
  switch (f.kind) {
    case "exec":
      return { kind: "exec", command: f.command, ...common };
    case "keys":
      return { kind: "keys", keys: f.keys, ...common };
    case "expect_respond":
      return { kind: "expect_respond", expect: f.expect, respond: f.respond, secret: f.respondSecret, ...common };
    case "secret":
      return { kind: "secret", reason: f.reason, ...(f.param ? { param: f.param } : {}), ...common };
    case "screen_check":
      return { kind: "screen_check", expect: f.expect, ...common };
  }
}

export function hostsFromText(text: string): string[] {
  const out: string[] = [];
  for (const h of text.split(/[\s,，;；]+/)) {
    if (h && !out.includes(h)) out.push(h);
  }
  return out;
}

export function hostsToText(hosts: string[]): string {
  return hosts.join("\n");
}

/** 表单 → 提交给后端的内容。secret 参数不带 default；无法解析的超时当作未设（由 [`localProblems`] 拦下保存）。 */
export function toInput(form: SkillForm): SkillInput {
  const params: SkillParam[] = form.params.map((p) => ({
    name: p.name,
    description: p.description,
    ...(!p.secret && p.defaultValue !== "" ? { default: p.defaultValue } : {}),
    required: p.required,
    secret: p.secret,
  }));
  let verify: SkillCheck | null = null;
  if (form.verifyOn) {
    const t = parseTimeout(form.verifyTimeoutMs);
    verify = {
      expect: form.verifyExpect,
      ...(t.ok && t.value !== undefined ? { timeoutMs: t.value } : {}),
    };
  }
  const hostScope: SkillHostScope =
    form.hostMode === "list" ? { kind: "list", hosts: hostsFromText(form.hostsText) } : { kind: "any" };
  return {
    name: form.name.trim(),
    description: form.description,
    params,
    steps: form.steps.map(stepToInput),
    verify,
    risk: form.risk,
    hostScope,
    source: form.source,
  };
}

// ——————————————————————————————— 本地检查 ———————————————————————————————

/** 本地能立刻判断的问题（只有超时填写）。其余规则一律以后端为准。 */
export interface LocalProblems {
  /** 步骤 uid → 超时无效 */
  stepTimeout: Set<number>;
  verifyTimeout: boolean;
}

export function localProblems(form: SkillForm): LocalProblems {
  const stepTimeout = new Set<number>();
  for (const s of form.steps) if (!parseTimeout(s.timeoutMs).ok) stepTimeout.add(s.uid);
  return { stepTimeout, verifyTimeout: form.verifyOn && !parseTimeout(form.verifyTimeoutMs).ok };
}

export function hasLocalProblems(p: LocalProblems): boolean {
  return p.stepTimeout.size > 0 || p.verifyTimeout;
}

// ——————————————————————————————— 步骤 / 参数操作 ———————————————————————————————

/** 把第 i 项移动 delta 位；越界返回原数组（同一引用，便于 setState 跳过重渲染） */
export function moveItem<T>(arr: readonly T[], i: number, delta: number): T[] {
  const j = i + delta;
  if (i < 0 || i >= arr.length || j < 0 || j >= arr.length || delta === 0) return arr as T[];
  const copy = arr.slice();
  [copy[i], copy[j]] = [copy[j], copy[i]];
  return copy;
}

export function removeAt<T>(arr: readonly T[], i: number): T[] {
  if (i < 0 || i >= arr.length) return arr as T[];
  return [...arr.slice(0, i), ...arr.slice(i + 1)];
}

export function insertAt<T>(arr: readonly T[], i: number, item: T): T[] {
  const k = Math.max(0, Math.min(i, arr.length));
  return [...arr.slice(0, k), item, ...arr.slice(k)];
}

const IDENT = /^[A-Za-z_][A-Za-z0-9_]{0,63}$/;
export const isIdent = (s: string) => IDENT.test(s);

/**
 * 把文本里的 `{{from}}` 换成 `{{to}}`。规则与后端 / `placeholderNames` 一致：
 * 两侧可有空白（保留原样）；`\{{from}}`（字面量）与 `{{.from}}` 之类不动。
 */
export function renamePlaceholder(text: string, from: string, to: string): string {
  return text.replace(/(\\)?\{\{(\s*)([A-Za-z_][A-Za-z0-9_]{0,63})(\s*)\}\}/g, (all, esc, l, name, r) => {
    if (esc || name !== from) return all;
    return `{{${l}${to}${r}}}`;
  });
}

/** 改步骤里所有会被代入的文本 + secret 步骤的 param 引用 */
function renameInStep(s: StepForm, from: string, to: string): StepForm {
  const rn = (t: string) => renamePlaceholder(t, from, to);
  const next: StepForm = {
    ...s,
    command: rn(s.command),
    keys: rn(s.keys),
    expect: rn(s.expect),
    respond: rn(s.respond),
    expectAfter: rn(s.expectAfter),
    param: s.param === from ? to : s.param,
  };
  // 没变就返回原对象：保住引用，让 memo 过的行不重渲染
  return (Object.keys(next) as (keyof StepForm)[]).every((k) => next[k] === s[k]) ? s : next;
}

/**
 * 给参数改名，并把所有引用同步改掉（命令 / 按键 / 应答 / 各 pattern / verify / secret 步骤的 param）。
 * 新名字不合法（非标识符）或与别的参数重名时只改名、不传播 —— 让后端校验把问题指出来，
 * 而不是把引用改到一个坏名字上。
 */
export function renameParam(form: SkillForm, uid: number, newName: string): SkillForm {
  const idx = form.params.findIndex((p) => p.uid === uid);
  if (idx < 0) return form;
  const old = form.params[idx].name;
  if (old === newName) return form;
  const params = form.params.map((p, i) => (i === idx ? { ...p, name: newName } : p));
  const clash = form.params.some((p, i) => i !== idx && p.name === newName);
  if (!isIdent(old) || !isIdent(newName) || clash) return { ...form, params };
  return {
    ...form,
    params,
    steps: form.steps.map((s) => renameInStep(s, old, newName)),
    verifyExpect: renamePlaceholder(form.verifyExpect, old, newName),
  };
}

/** 切换参数的 secret：开启时清掉默认值（secret 参数不得有默认值） */
export function setParamSecret(p: ParamForm, secret: boolean): ParamForm {
  return secret ? { ...p, secret, defaultValue: "" } : { ...p, secret };
}

/** respond 是不是「恰好一个 `{{name}}`」，是则返回 name */
export function respondRef(respond: string): string | null {
  const m = /^\s*\{\{\s*([A-Za-z_][A-Za-z0-9_]{0,63})\s*\}\}\s*$/.exec(respond);
  return m ? m[1] : null;
}

/**
 * 切换 expect_respond 的 secret。**开启**时除非 respond 已经是单一引用，否则清空：
 * 防止把原来的字面应答（可能就是口令）带进 secret 步骤。
 */
export function setRespondSecret(s: StepForm, on: boolean): StepForm {
  if (!on) return { ...s, respondSecret: false };
  return { ...s, respondSecret: true, respond: respondRef(s.respond) ? s.respond : "" };
}

/** 删掉一个参数时，secret 步骤里对它的引用一并清掉（占位符留着，由「未声明」提示） */
export function removeParam(form: SkillForm, uid: number): SkillForm {
  const p = form.params.find((x) => x.uid === uid);
  if (!p) return form;
  return {
    ...form,
    params: form.params.filter((x) => x.uid !== uid),
    steps: form.steps.map((s) => (s.param === p.name ? { ...s, param: "" } : s)),
  };
}

/** 把引用了却没声明的参数补成声明（非 secret、非必填）。返回新表单与补上的名字。 */
export function declareMissing(form: SkillForm): { form: SkillForm; added: string[] } {
  const added = undeclaredParams(toInput(form));
  if (added.length === 0) return { form, added };
  return { form: { ...form, params: [...form.params, ...added.map((n) => newParam(n))] }, added };
}

// ——————————————————————————————— 占位符高亮 ———————————————————————————————

export interface PlaceholderSegment {
  text: string;
  /** `param`：已声明；`undeclared`：引用了没声明（后端会拒）；`text`：普通文本（含 `\{{x}}` 字面量） */
  kind: "text" | "param" | "undeclared";
  /** kind 为 param / undeclared 时的参数名 */
  name?: string;
  /** 已声明且是 secret 参数（在命令 / 按键里用会被后端拒绝，只能进 secret 通道） */
  secret?: boolean;
}

/** 把文本按占位符切段，用于高亮显示。`secretNames` 里的参数会带 `secret` 标记。 */
export function placeholderSegments(
  text: string,
  declared: ReadonlySet<string>,
  secretNames: ReadonlySet<string> = new Set()
): PlaceholderSegment[] {
  const out: PlaceholderSegment[] = [];
  const re = /(\\)?\{\{\s*([A-Za-z_][A-Za-z0-9_]{0,63})\s*\}\}/g;
  let last = 0;
  for (let m = re.exec(text); m; m = re.exec(text)) {
    if (m[1]) continue; // 转义的字面量留在普通文本里
    if (m.index > last) out.push({ text: text.slice(last, m.index), kind: "text" });
    const name = m[2];
    out.push(
      declared.has(name)
        ? { text: m[0], kind: "param", name, secret: secretNames.has(name) }
        : { text: m[0], kind: "undeclared", name }
    );
    last = m.index + m[0].length;
  }
  if (last < text.length) out.push({ text: text.slice(last), kind: "text" });
  return out;
}

/** 一段文本里出现的占位符（去重、保序），带声明状态——字段下方的小标签用 */
export function placeholderRefs(
  text: string,
  declared: ReadonlySet<string>,
  secretNames: ReadonlySet<string> = new Set()
): { name: string; declared: boolean; secret: boolean }[] {
  const seen = new Set<string>();
  const out: { name: string; declared: boolean; secret: boolean }[] = [];
  for (const seg of placeholderSegments(text, declared, secretNames)) {
    if (seg.kind === "text" || !seg.name || seen.has(seg.name)) continue;
    seen.add(seg.name);
    out.push({ name: seg.name, declared: seg.kind === "param", secret: !!seg.secret });
  }
  return out;
}

// ——————————————————————————————— 后端问题归类 ———————————————————————————————

export interface GroupedProblems {
  /** 名称 / 描述 / 步骤数 / 大小等整体问题 */
  general: string[];
  /** 步骤下标（0 起）→ 该步的问题 */
  steps: Map<number, string[]>;
  /** 参数名 → 该参数的问题 */
  params: Map<string, string[]>;
  verify: string[];
  hosts: string[];
  /** 风险（后端只会抬高；这里出现通常说明草稿里的 risk 低于评估值） */
  risk: string[];
}

/** 创建 / 更新的 reject 文案与 `skill_validate` 的结果是同一种：每行一个问题 */
export function splitProblems(message: string): string[] {
  return message
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean);
}

const push = <K>(m: Map<K, string[]>, k: K, v: string) => {
  const a = m.get(k);
  if (a) a.push(v);
  else m.set(k, [v]);
};

/**
 * 按后端问题的文案前缀归类：「第 N 步（kind）…」→ 步骤；「参数「x」…」「secret 参数「x」…」「参数名「x」…」→ 参数；
 * 「verify…」「hostScope…」「风险等级…」各自一组；其余归整体。文案是后端写的，认不出就落到 general，
 * 不会丢问题。
 */
export function groupProblems(problems: readonly string[]): GroupedProblems {
  const g: GroupedProblems = {
    general: [],
    steps: new Map(),
    params: new Map(),
    verify: [],
    hosts: [],
    risk: [],
  };
  for (const p of problems) {
    let m: RegExpExecArray | null;
    if ((m = /^第 (\d+) 步/.exec(p))) push(g.steps, Number(m[1]) - 1, p);
    else if ((m = /^(?:secret )?参数(?:名)?「(.*?)」/.exec(p))) push(g.params, m[1], p);
    else if (p.startsWith("verify")) g.verify.push(p);
    else if (p.startsWith("hostScope")) g.hosts.push(p);
    else if (p.startsWith("风险等级")) g.risk.push(p);
    else g.general.push(p);
  }
  return g;
}

export function problemCount(g: GroupedProblems): number {
  let n = g.general.length + g.verify.length + g.hosts.length + g.risk.length;
  for (const v of g.steps.values()) n += v.length;
  for (const v of g.params.values()) n += v.length;
  return n;
}

// ——————————————————————————————— 其它展示辅助 ———————————————————————————————

const RISK_ORDER: Record<SkillRisk, number> = { low: 0, medium: 1, high: 2 };

/** 后端把风险从提交值抬高了吗（保存后提示用） */
export function riskRaised(submitted: SkillRisk, saved: SkillRisk): boolean {
  return RISK_ORDER[saved] > RISK_ORDER[submitted];
}

/** 适用主机的一行描述 */
export function describeHostScope(scope: SkillHostScope): string {
  if (scope.kind === "any") return "不限";
  if (scope.hosts.length === 0) return "（空）";
  return scope.hosts.length <= 2 ? scope.hosts.join("、") : `${scope.hosts.slice(0, 2).join("、")} 等 ${scope.hosts.length} 项`;
}

/** 上次运行时间（Unix 秒）。`now` 以毫秒传入以便测试。 */
export function formatLastRun(ts: number | null, now: number = Date.now()): string {
  if (ts === null || ts <= 0) return "从未运行";
  const diff = Math.max(0, Math.floor(now / 1000) - ts);
  if (diff < 60) return "刚刚";
  if (diff < 3600) return `${Math.floor(diff / 60)} 分钟前`;
  if (diff < 86400) return `${Math.floor(diff / 3600)} 小时前`;
  if (diff < 86400 * 30) return `${Math.floor(diff / 86400)} 天前`;
  const d = new Date(ts * 1000);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
}

/**
 * 编辑会让后端清零成功 / 失败计数吗（步骤 / 参数 / 验证 / 主机范围任一变了）。
 * 只用于保存前提醒；清零与否以后端为准。
 */
export function replayChanged(a: SkillInput, b: SkillInput): boolean {
  return (
    JSON.stringify([a.params, a.steps, a.verify, a.hostScope]) !==
    JSON.stringify([b.params, b.steps, b.verify, b.hostScope])
  );
}

/** 导出文件的默认名：去掉文件名里的非法字符 */
export function exportFileName(name: string): string {
  const base = name.trim().replace(/[\\/:*?"<>|\x00-\x1f]/g, "_") || "skill";
  return `${base}.skill.json`;
}
