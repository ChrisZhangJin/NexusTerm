/**
 * Console 自动补全的数据层：**完全不碰 DOM / xterm**，只回答一个问题——
 * 根据这个会话的历史，人现在敲的这半句最可能是哪条命令？
 *
 * 把它和渲染拆开是有意的：这件事能独立推理与验证，
 * 混进 xterm 的事件回调里就只能靠「跑起来看屏幕」来验了。
 *
 * ## key 是「会话身份」，不是标签页 id
 *
 * 历史按 `historyKey` 存取——本地 shell 固定是 `"local"`，已保存的 SSH
 * 连接是它的 `SessionRecord.id`，两者都会落盘（`api.historyRecord`），
 * 同一台机器下次重连、甚至重启应用后新开的标签都能接着用。
 *
 * 没有引用已保存连接记录的临时 SSH 连接（比如手填参数、不保存就连）
 * 没有这个身份，调用方应该传标签页自己的 `tabId` 当 key——这类 key
 * **不会**被持久化（没人会调 `loadHistory`/`persistCommand` 之外的
 * 持久化路径去碰它），行为和过去的纯内存历史一致，关掉标签就没了。
 */

import { api } from "../api";

/** 每个 key 记住多少条历史命令。够回放最近的工作节奏，又不至于吃内存；
 *  与后端 `Store::HISTORY_CAP` 保持一致。 */
const HISTORY_CAP = 300;
/** 少于这么多字符就预测的话，噪声远大于帮助 —— 敲一个 `d` 不该猜出 `docker ps -a` */
const MIN_PREDICT_CHARS = 2;

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

const hasControl = (s: string): boolean => {
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i);
    if (c < 0x20 || c === DEL) return true;
  }
  return false;
};

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

function historyOf(key: string): History {
  let h = histories.get(key);
  if (!h) {
    h = { items: [] };
    histories.set(key, h);
  }
  return h;
}

/**
 * 记一条**已经提交**的命令。
 *
 * 只收非空且不含控制字符的输入：被打断的半句、一路 Tab 拼出来的残句，
 * 都不该混进历史 —— 它们一旦被记下，下次就会把垃圾推到最前面。
 *
 * 本地缓存立即更新（同步，接下来几次按键的预测不用等网络往返）；
 * `persist` 为真时再额外 fire-and-forget 落盘 —— 失败不影响本地体验，
 * 和过去 `--help` 查询失败退化成空数组是同一套哲学。
 */
export function recordCommand(key: string, raw: string, persist: boolean): void {
  const cmd = raw.trim();
  if (!cmd || cmd.length > 4096 || hasControl(cmd)) return;

  const h = historyOf(key);
  // 已经在历史里的先摘出来，再压到队尾 —— 「最近用过的优先」是预测的主要依据
  const dup = h.items.indexOf(cmd);
  if (dup >= 0) h.items.splice(dup, 1);
  h.items.push(cmd);
  if (h.items.length > HISTORY_CAP) {
    h.items.splice(0, h.items.length - HISTORY_CAP);
  }

  if (persist) {
    void api.historyRecord(key, cmd).catch(() => {});
  }
}

/**
 * 从后端拉一个会话身份的全量历史，灌进本地缓存。
 *
 * 只给**持久化**的 key 调用（本地 shell / 已保存的 SSH 连接）——
 * 标签打开或重连时调一次即可，之后的预测全部走本地缓存，
 * 和 `predictHistory`/`historyMatches` 的同步特性保持一致。
 * 失败就留着本地缓存原样（通常是空的）：这台机器以前没留下历史。
 */
export async function loadHistory(key: string): Promise<void> {
  const items = await api.historyList(key).catch(() => [] as string[]);
  // 后端按「最近用过在前」返回，本地约定是「最近用过在后」，这里 reverse 对齐
  historyOf(key).items = items.slice().reverse();
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
export function predictHistory(key: string, line: string): string | null {
  const prefix = line.trimStart();
  if (prefix.length < MIN_PREDICT_CHARS || hasControl(prefix)) return null;

  const items = histories.get(key)?.items;
  if (!items) return null;
  for (let i = items.length - 1; i >= 0; i--) {
    const cmd = items[i];
    // 完全相同的那条跳过：人都已经打完了，再「预测」一遍只是打扰
    if (cmd.length > prefix.length && cmd.startsWith(prefix)) return cmd;
  }
  return null;
}

/** 历史里所有以 prefix 开头的命令（最近用过的在前）。给下拉列表用。 */
export function historyMatches(key: string, prefix: string): string[] {
  const items = histories.get(key)?.items;
  if (!items || prefix.length < 1) return [];
  const out: string[] = [];
  for (let i = items.length - 1; i >= 0; i--) {
    const cmd = items[i];
    if (cmd !== prefix && cmd.startsWith(prefix)) out.push(cmd);
    if (out.length >= 30) break;
  }
  return out;
}

/**
 * 标签关掉了：只清非持久化 key 的内存历史（临时 SSH 连接，退出即忘）。
 * 持久化 key（本地 / 已保存会话）不动——可能还有别的标签共用同一个 key，
 * 真正的数据也早就落盘了，内存里留着只是热缓存，没必要清。
 */
export function dropCompletion(key: string, persist: boolean): void {
  if (!persist) histories.delete(key);
}

// ——— 合成候选 ———

export interface CompletionItem {
  /** 主文本：整条历史命令 */
  label: string;
  /** 按 Tab 真正补进去的那一段 —— **不含已经敲出来的部分** */
  insert: string;
}

/**
 * 把历史预测合成候选列表（按当前整行做前缀匹配，不区分命令名/参数阶段）。
 */
export function buildCompletions(key: string, line: string): CompletionItem[] {
  const prefix = line.trimStart();
  if (!prefix) return [];

  const out: CompletionItem[] = [];
  for (const cmd of historyMatches(key, prefix)) {
    out.push({ label: cmd, insert: cmd.slice(prefix.length) });
  }
  return out;
}
