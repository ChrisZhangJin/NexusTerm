/**
 * Console 自动补全的数据层：**完全不碰 DOM / xterm**，只回答一个问题——
 * 根据这个会话的历史，人现在敲的这半句最可能是哪条命令？
 *
 * ## 历史从哪来
 *
 * 两个来源，都只在内存里，**不落盘**：
 * - shell 自己的历史文件（后端 `history_load_*` 读 `~/.bash_history` 等），连接时拉一次；
 * - 本次运行里人在这个会话敲过的命令（`recordCommand`）。
 * bash 默认退出时才写文件，所以第二项不是多余的：它补上「文件里还没有的最近几条」。
 *
 * 和渲染拆开是有意的：这件事能独立推理与验证，混进 xterm 的事件回调里就只能
 * 靠「跑起来看屏幕」来验了。
 *
 * ## key 是「会话身份」，不是标签页 id
 *
 * 本地 shell 固定是 `"local"`，已保存的 SSH 连接是它的 `SessionRecord.id`，
 * 同一身份的多个标签共用内存里的一份。临时 SSH 连接（手填参数、不保存就连）
 * 没有这个身份，调用方传标签页自己的 `tabId` 当 key —— 关掉标签就没了。
 */

/** 每个 key 记住多少条历史命令。与后端 `histfile::MAX_ITEMS` 保持一致。 */
const HISTORY_CAP = 1000;
/** 少于这么多字符就预测的话，噪声远大于帮助 —— 敲一个 `d` 不该猜出 `docker ps -a` */
const MIN_PREDICT_CHARS = 2;

const CR = 0x0d;
const DEL = 0x7f;
const BS = 0x08;

/** 当前正在敲的那一行（不含提示符）。 */
export interface LineState {
  /** 已知部分。`known` 为假时恒为空串。 */
  text: string;
  /**
   * 这一行从行首起的每个字符，我们是不是都亲眼见过。
   *
   * 补全只认**从行首开始**的前缀，所以一旦丢了前缀（Tab 交给 shell 补全、方向键翻历史、
   * Ctrl-A/E/W/R、粘贴……画面被 shell 改写了，我们无从得知现在行里是什么），
   * 就必须进入「未知」，直到下一次回车 / Ctrl-C 把行重置。
   *
   * 绝不能退化成「清空后接着攒」：那样后面敲的尾巴（`attach -t 0`）会被当成完整一行，
   * 既拿去匹配，回车后还会记进历史，之后敲 `att` 就提示出一条从没人完整敲过的命令。
   */
  known: boolean;
  /** 正处在括号粘贴（ESC[200~ … ESC[201~）里：整段都不是「敲」的，shell 也不会逐行执行。 */
  pasting: boolean;
}

export const newLineState = (): LineState => ({ text: "", known: true, pasting: false });

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
 * - 回车：提交。行已知才交出命令；未知的行什么都不交（也就不会进历史），随后重置为已知。
 * - 退格 / Ctrl-U：结果可推断（光标在行尾，Ctrl-U 删掉整行），行保持已知。
 * - Ctrl-C：新提示符，重置为已知的空行。Ctrl-L 只重绘屏幕，不动行内容。
 * - 其余控制字符与所有 ESC 序列（↑↓←→、Home/End、Tab…）：画面被改写，行变**未知**。
 *
 * ESC 序列必须整段吃掉：方向键是 `ESC [ A` 三个字节，只处理 ESC 的话后面的
 * `[A` 会被当普通字符接进行里。括号粘贴的起止标记也在这里认出来。
 *
 * @returns 本次按键里若有回车，返回被提交的那条命令；否则返回 null
 */
export function feedKeys(state: LineState, data: string): string | null {
  let committed: string | null = null;
  let esc = ESC_NONE;
  let params = "";

  const lose = () => {
    state.text = "";
    state.known = false;
  };

  for (const ch of data) {
    const code = ch.charCodeAt(0);

    // 正在吃 ESC 序列：一直吞到终止字节（0x40–0x7E）
    if (esc !== ESC_NONE) {
      if (esc === ESC_SEEN && (ch === "[" || ch === "O")) {
        esc = ESC_CSI;
        params = "";
        continue;
      }
      if (esc === ESC_CSI) {
        if (code >= 0x40 && code <= 0x7e) {
          esc = ESC_NONE;
          if (ch === "~" && params === "200") state.pasting = true;
          else if (ch === "~" && params === "201") state.pasting = false;
        } else {
          params += ch;
        }
        continue;
      }
      // ESC 后面不是 `[` / `O`：不是光标序列，按普通字符继续走下面
      esc = ESC_NONE;
    }

    if (code === 0x1b) {
      esc = ESC_SEEN;
      lose();
      continue;
    }
    // 粘贴期间一切都不算「敲」：不攒字、不提交（shell 也不会在粘贴的换行上执行）
    if (state.pasting) continue;

    if (code === CR || code === 0x0a) {
      const done = state.known ? state.text.trim() : "";
      state.text = "";
      state.known = true;
      if (done && committed === null) committed = done;
      continue;
    }
    if (code === 0x03) {
      // Ctrl-C
      state.text = "";
      state.known = true;
      continue;
    }
    if (code === 0x0c) continue; // Ctrl-L
    if (code === 0x15) {
      // Ctrl-U：光标在行尾（已知的行必然如此）时删掉整行
      if (state.known) state.text = "";
      continue;
    }
    if (code === DEL || code === BS) {
      if (state.known) state.text = state.text.slice(0, -1);
      continue;
    }
    if (code < 0x20) {
      lose();
      continue;
    }
    if (state.known) state.text += ch;
  }
  // 粘贴过的行：括号粘贴结束后行里多了我们没逐字看过的内容
  if (state.pasting) lose();
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
 * 只进内存，不落盘。
 */
export function recordCommand(key: string, raw: string): void {
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
}

/**
 * 把 shell 历史文件里的命令并进这个 key 的历史。
 *
 * `fetched` 是**从旧到新**的。它们排在已有条目**之前**：加载是异步的，等它回来时人
 * 可能已经敲过几条，那几条一定比文件里的新；同一条命令两边都有就只留内存里那份。
 * 失败（兼容模式、没有历史文件）就当没有，补全退化成只用本次运行里敲过的命令。
 */
export async function loadHistory(key: string, fetch: () => Promise<string[]>): Promise<void> {
  const fetched = await fetch().catch(() => [] as string[]);
  const h = historyOf(key);
  const mine = new Set(h.items);
  const older = fetched.filter((c) => !mine.has(c) && c.length <= 4096 && !hasControl(c));
  h.items = [...older, ...h.items].slice(-HISTORY_CAP);
}

/**
 * 历史预测：这半句之后最可能是什么。
 *
 * 返回**完整的一条命令**，调用方减去已输入的部分就是该显示的灰字。
 *
 * 只按「最近用过」排序：终端里的工作是成串的，刚跑过 `tail -f` 下一次多半
 * 还是它 —— 比词频模型简单，也更贴合真实手感。
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
  // 门槛与 `predictHistory` 保持一致：敲一个字符就弹一条完整历史，
  // 噪声远大于帮助——真正想要的是「接着敲了几个字，像是在找这条」。
  if (!items || prefix.length < MIN_PREDICT_CHARS) return [];
  const out: string[] = [];
  for (let i = items.length - 1; i >= 0; i--) {
    const cmd = items[i];
    if (cmd !== prefix && cmd.startsWith(prefix)) out.push(cmd);
    if (out.length >= 30) break;
  }
  return out;
}

/**
 * 标签关掉了：只清临时 key 的内存历史（没保存的 SSH 连接，退出即忘）。
 * 共享 key（本地 / 已保存会话）不动——可能还有别的标签共用同一个 key，
 * 重开标签时也不必再读一遍文件之前先白手起家。
 */
export function dropCompletion(key: string, shared: boolean): void {
  if (!shared) histories.delete(key);
}

// ——— 合成候选 ———

export interface CompletionItem {
  /** 主文本：整条历史命令 */
  label: string;
  /** 按 Tab 真正补进去的那一段 —— **不含已经敲出来的部分** */
  insert: string;
}

/** 候选列表：按当前整行做前缀匹配，不区分命令名/参数阶段。 */
export function buildCompletions(key: string, line: string): CompletionItem[] {
  const prefix = line.trimStart();
  if (!prefix) return [];

  const out: CompletionItem[] = [];
  for (const cmd of historyMatches(key, prefix)) {
    out.push({ label: cmd, insert: cmd.slice(prefix.length) });
  }
  return out;
}
