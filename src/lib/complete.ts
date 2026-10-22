/**
 * Console 自动补全的数据层：**完全不碰 DOM / xterm**，只回答两个问题。
 *
 * 1. 根据历史，人现在敲的这半句最可能是哪条命令？
 * 2. 某个可执行命令到底支持哪些参数？（答案由后端跑 `<binary> --help` 得来）
 *
 * 把它和渲染拆开是有意的：这两件事都能独立推理与验证，
 * 混进 xterm 的事件回调里就只能靠「跑起来看屏幕」来验了。
 */

import { api } from "../api";
import type { CmdOption } from "../types";

/** 每个会话记住多少条历史命令。够回放最近的工作节奏，又不至于吃内存。 */
const HISTORY_CAP = 300;
/** 选项缓存上限：一个会话里敲过的命令名可能很多 */
const OPT_CACHE_CAP = 64;
/** 少于这么多字符就预测的话，噪声远大于帮助 —— 敲一个 `d` 不该猜出 `docker ps -a` */
const MIN_PREDICT_CHARS = 2;

// 控制字符一律写成码点常量而不是正则区间：
// 那种写法在不同工具链里容易被二次转义，这里求稳。
const CR = 0x0d;
const DEL = 0x7f;
const BS = 0x08;

/** 当前正在敲的那一行（不含提示符）。 */
export interface LineState {
  text: string;
}

export const newLineState = (): LineState => ({ text: "" });

interface History {
  /** 最近用过的排在最后 */
  items: string[];
}

const histories = new Map<string, History>();
/** key = `tabId @@ program`。空数组也算「查过了」，免得对同一个不存在的东西反复问。 */
const optCache = new Map<string, CmdOption[]>();
/** 同一 key 只发一次请求：连按几个键不该打出好几条一模一样的 IPC */
const inflight = new Map<string, Promise<CmdOption[]>>();

const hasControl = (s: string): boolean => {
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i);
    if (c < 0x20 || c === DEL) return true;
  }
  return false;
};

/** 当前行的第一个 token —— 也就是那个「可执行名」（可能还带着路径） */
export function firstToken(line: string): string {
  const parts = line.trimStart().split(/[ \t]+/);
  return parts[0] ?? "";
}

/** 取当前行的最后一个 token（按空白切）。给参数补全定位「半截 flag」用。 */
export function lastToken(line: string): string {
  const parts = line.split(/[ \t]+/);
  return parts[parts.length - 1] ?? "";
}

/** 程序名的 basename：`/usr/bin/tail` → `tail`。给 UI 显示用。 */
export function programName(program: string): string {
  return program.split("/").filter(Boolean).pop() ?? program;
}

/** ESC 序列的解析状态：0 = 无，1 = 刚收到 ESC，2 = 已收到 ESC + `[` / `O` */
const ESC_NONE = 0;
const ESC_SEEN = 1;
const ESC_CSI = 2;

/**
 * 把一串按键喂进行状态。
 *
 * 回车表示「这行交出去了」；退格表示「行变短了」；
 * 其余控制字符（↑↓、Ctrl-U/Ctrl-W 整行编辑、Tab…）意味着
 * **画面被改写，我们已经无从得知光标停在哪、剩下什么**。
 * 与其拿一个错的半句去猜，不如把补全自动撤下：
 * 人回到提示符上重新敲，状态立刻又是准的。
 *
 * ESC 序列必须整段吃掉：方向键是 `ESC [ A` 三个字节，
 * 只按 ESC 一个字节处理的话，后面的 `[A` 会被当成普通字符接进行里，
 * 于是「按了一下 ↑」之后行状态变成 `[A`，预测与灰字全错。
 *
 * @returns 本次按键里若有回车，返回被提交的那条命令；否则返回 null
 */
export function feedKeys(state: LineState, data: string): string | null {
  let committed: string | null = null;
  let esc = ESC_NONE;

  for (const ch of data) {
    const code = ch.charCodeAt(0);

    // 正在吃 ESC 序列：一直吞到终止字节（0x40–0x7E）
    if (esc !== ESC_NONE) {
      if (esc === ESC_SEEN && (ch === "[" || ch === "O")) {
        esc = ESC_CSI;
        continue;
      }
      if (esc === ESC_CSI) {
        if (code >= 0x40 && code <= 0x7e) esc = ESC_NONE;
        continue;
      }
      // ESC 后面不是 `[` / `O`：不是光标序列，按普通字符继续走下面
      esc = ESC_NONE;
    }

    if (code === 0x1b) {
      esc = ESC_SEEN;
      state.text = "";
      continue;
    }
    if (code === CR || code === 0x0a) {
      const done = state.text.trim();
      state.text = "";
      if (done && committed === null) committed = done;
      continue;
    }
    if (code === DEL || code === BS) {
      state.text = state.text.slice(0, -1);
      continue;
    }
    if (code < 0x20) {
      state.text = "";
      continue;
    }
    state.text += ch;
  }
  return committed;
}

// ——— 历史 ———

/**
 * 记一条**已经提交**的命令。
 *
 * 只收非空且不含控制字符的输入：被打断的半句、一路 Tab 拼出来的残句，
 * 都不该混进历史 —— 它们一旦被记下，下次就会把垃圾推到最前面。
 */
export function recordCommand(tabId: string, raw: string): void {
  const cmd = raw.trim();
  if (!cmd || cmd.length > 4096 || hasControl(cmd)) return;

  let h = histories.get(tabId);
  if (!h) {
    h = { items: [] };
    histories.set(tabId, h);
  }
  // 已经在历史里的先摘出来，再压到队尾 —— 「最近用过的优先」是预测的主要依据
  const dup = h.items.indexOf(cmd);
  if (dup >= 0) h.items.splice(dup, 1);
  h.items.push(cmd);
  if (h.items.length > HISTORY_CAP) {
    h.items.splice(0, h.items.length - HISTORY_CAP);
  }
}

/**
 * 历史预测：这半句之后最可能是什么。
 *
 * 返回**完整的一条命令**，调用方拿它减去已输入的部分就是该显示的灰字；
 * 没有合适候选时返回 null。
 *
 * 排序只按「最近用过」：终端里的工作是成串的 —— 刚跑过 `tail -f`，
 * 下一次多半还是它。比词频模型简单得多，也更贴合真实手感。
 */
export function predictHistory(tabId: string, line: string): string | null {
  const prefix = line.trimStart();
  if (prefix.length < MIN_PREDICT_CHARS || hasControl(prefix)) return null;

  const items = histories.get(tabId)?.items;
  if (!items) return null;
  for (let i = items.length - 1; i >= 0; i--) {
    const cmd = items[i];
    // 完全相同的那条跳过：人都已经打完了，再「预测」一遍只是打扰
    if (cmd.length > prefix.length && cmd.startsWith(prefix)) return cmd;
  }
  return null;
}

/** 历史里所有以 prefix 开头的命令（最近用过的在前）。给下拉列表用。 */
export function historyMatches(tabId: string, prefix: string): string[] {
  const items = histories.get(tabId)?.items;
  if (!items || prefix.length < 1) return [];
  const out: string[] = [];
  for (let i = items.length - 1; i >= 0; i--) {
    const cmd = items[i];
    if (cmd !== prefix && cmd.startsWith(prefix)) out.push(cmd);
    if (out.length >= 30) break;
  }
  return out;
}

/** 标签关掉了就把它攒的历史与缓存放掉 —— 这些 Map 的生命周期是整个进程 */
export function dropCompletion(tabId: string): void {
  histories.delete(tabId);
  for (const k of [...optCache.keys()]) {
    if (k.startsWith(`${tabId}@@`)) optCache.delete(k);
  }
}

// ——— 二进制参数 ———

/**
 * 取某个程序支持的参数。**带缓存，且同一时刻只发一个请求。**
 *
 * 失败一律退化成空数组：参数提示是锦上添花，
 * 程序不存在、远端不允许开旁路通道，都不该影响人继续打字。
 */
export function optionsFor(tabId: string, program: string): Promise<CmdOption[]> {
  if (!worthAsking(program)) return Promise.resolve([]);
  const key = `${tabId}@@${program}`;
  const cached = optCache.get(key);
  if (cached) return Promise.resolve(cached);
  const pending = inflight.get(key);
  if (pending) return pending;

  const p = api
    .consoleCommandOptions(tabId, program)
    .catch(() => [] as CmdOption[])
    .then((opts) => {
      setCache(key, opts);
      inflight.delete(key);
      return opts;
    });
  inflight.set(key, p);
  return p;
}

function setCache(key: string, opts: CmdOption[]) {
  optCache.set(key, opts);
  // FIFO 淘汰：几十条缓存而已，不必为它引入 LRU 的复杂度
  if (optCache.size > OPT_CACHE_CAP) {
    const oldest = optCache.keys().next().value;
    if (oldest !== undefined) optCache.delete(oldest);
  }
}

/**
 * 从参数表里挑出匹配当前半个 token 的那些。
 *
 * 排序讲究：**短的在前**。人敲 `--v` 时，想要的是 `--verbose` 这种能直接补完的，
 * 所以按候选长度升序 —— 第一个就是补全收益最大的那条。
 */
export function matchOptions(opts: CmdOption[], token: string): CmdOption[] {
  if (!token) return [];
  const out: CmdOption[] = [];
  for (const o of opts) {
    const hit = o.flags.find((f) => f !== token && f.startsWith(token));
    if (!hit) continue;
    out.push({ ...o, flags: [hit, ...o.flags.filter((f) => f !== hit)] });
  }
  out.sort((a, b) => a.flags[0].length - b.flags[0].length);
  return out.slice(0, 40);
}

/** 同步的缓存查询：有就用，没有返回 null（调用方再决定要不要发起请求） */
export function peekOptions(tabId: string, program: string): CmdOption[] | null {
  return optCache.get(`${tabId}@@${program}`) ?? null;
}

// ——— 合成候选 ———

export interface CompletionItem {
  kind: "history" | "flag";
  /** 主文本：历史 = 整条命令；参数 = 那个 flag（如 `--follow`） */
  label: string;
  /** 右侧一句话说明 */
  detail: string;
  /** 按 Tab 真正补进去的那一段 —— **不含已经敲出来的部分** */
  insert: string;
}

/**
 * shell 内建命令：它们没有独立的可执行文件，跑 `<name> --help`
 * 只会把 `--help` 当参数打印一遍（甚至直接挂住等 stdin）。
 * 明知道问不出东西，就别去打扰后端。
 */
const BUILTINS = new Set([
  "cd", "echo", "export", "source", ".", "alias", "unalias", "unset", "set",
  "shift", "history", "exit", "logout", "fg", "bg", "jobs", "pwd", "read",
  "eval", "exec", "trap", "wait", "times", "hash", "bind", "dirs", "pushd",
  "popd", "disown", "ulimit", "umask", "type", "let", "local", "declare",
  "return", "break", "continue", "true", "false", "test", "[",
]);

/** 这个程序名值得去问一次 `--help` 吗？ */
export function worthAsking(program: string): boolean {
  if (!program || program.length > 64) return false;
  const base = programName(program).toLowerCase();
  if (BUILTINS.has(base)) return false;
  // 明显不是程序名：路径里带空格、或长得像一串参数
  if (/\s/.test(program)) return false;
  return true;
}

/**
 * 把「历史预测」与「参数表」合成一份候选列表。
 *
 * 顺序是刻意的：**参数在前，历史在后**。
 * 人正敲到一半的 `--fo` 说明他此刻脑子里想的是这个程序的参数，
 * 把它排在前面才不用按方向键去找；历史只是「也许你想重跑整条」的兜底。
 *
 * @param opts 该程序的参数表（尚未查到时传空数组，只出历史）
 */
export function buildCompletions(
  tabId: string,
  line: string,
  opts: CmdOption[]
): CompletionItem[] {
  const prefix = line.trimStart();
  if (!prefix) return [];

  const out: CompletionItem[] = [];
  const seen = new Set<string>();

  const token = lastToken(line);
  if (token.startsWith("-") && opts.length > 0) {
    for (const o of matchOptions(opts, token)) {
      const flag = o.flags[0];
      const key = `f:${flag}`;
      if (seen.has(key)) continue;
      seen.add(key);
      out.push({
        kind: "flag",
        label: flag,
        detail: o.summary || (o.takesValue ? "需要取值" : "开关"),
        // 补完 flag 后留一个空格：多半还要接着打值或下一个参数
        insert: flag.slice(token.length) + " ",
      });
    }
  }

  for (const cmd of historyMatches(tabId, prefix)) {
    const key = `h:${cmd}`;
    if (seen.has(key)) continue;
    seen.add(key);
    out.push({
      kind: "history",
      label: cmd,
      detail: "历史命令",
      insert: cmd.slice(prefix.length),
    });
  }

  return out.slice(0, 40);
}
