/**
 * Shift+Tab 历史面板的纯逻辑：按当前这半行筛历史、给出要往终端里写的字节。
 *
 * 不碰 DOM / xterm，单测见 tests/fe/historyPanel.test.ts。
 */

/** 面板最多列多少条（再多翻不过来，也没必要一次画出来） */
export const PANEL_LIMIT = 200;

/**
 * 按关键字筛历史。`items` 是**最近用过的在前**的整条命令。
 *
 * - 空关键字：全部历史（最近的在前）；
 * - 不区分大小写；以关键字开头的排在前面，其余「包含」的在后，两组内部都保持最近的在前；
 * - 关键字可以有好几个词（`tmux att`）：每个词都得出现，按顺序出现的那些（「开头」那组看整串）才算开头匹配。
 */
export function searchHistory(items: readonly string[], query: string, limit = PANEL_LIMIT): string[] {
  const q = query.trim().toLowerCase();
  if (!q) return items.slice(0, limit);
  const words = q.split(/\s+/);
  const starts: string[] = [];
  const contains: string[] = [];
  for (const cmd of items) {
    const c = cmd.toLowerCase();
    if (c.startsWith(q)) starts.push(cmd);
    else if (words.every((w) => c.includes(w))) contains.push(cmd);
    if (starts.length >= limit) break;
  }
  return [...starts, ...contains].slice(0, limit);
}

/**
 * 把命令行换成 `cmd` 要发给终端的字节（不含回车：只放到命令行上，跑不跑由人决定）。
 *
 * - 我们确切知道行里敲了什么（`typed`）：退格删掉这么多字符再写 —— 任何 shell（含 PowerShell）都认；
 * - 不知道（之前按过 ↑ / Tab 等，行被 shell 改写过）：Ctrl-E 到行尾 + Ctrl-U 删整行（bash / zsh / fish 通用）。
 */
export function replaceLineInput(typed: string | null, cmd: string): string {
  if (typed === null) return `\x05\x15${cmd}`;
  return "\x7f".repeat([...typed].length) + cmd;
}
