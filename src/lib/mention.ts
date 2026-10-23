/**
 * 输入框里的 `@会话名` 联想。
 *
 * 抽出来是为了能被单独验证：这里全是字符串下标的活儿，
 * 混在 React 组件的 onChange 里就只能靠「打几个字看看」来验了。
 */

/** 光标停在的 `@xxx` 片段：`start` 是 `@` 的下标，`query` 是 `@` 之后到光标的部分 */
export interface MentionQuery {
  start: number;
  query: string;
}

/**
 * 光标是不是正停在一个 `@xxx` 片段里？是就返回它的起点与已输入的部分。
 *
 * 判定规则刻意简单：
 * - `@` 必须在行首或空白之后（邮箱、`a@b` 不该触发）
 * - `@` 到光标之间不能有空白（打完了就是普通文本）
 */
export function scanMention(text: string, caret: number): MentionQuery | null {
  if (caret < 0 || caret > text.length) return null;

  let i = caret - 1;
  while (i >= 0 && !/\s/.test(text[i])) i--;
  const start = i + 1;
  if (start >= caret || text[start] !== "@") return null;

  const query = text.slice(start + 1, caret);
  // 片段里又出现 `@` → 按离光标更近的那个算（`@a@b` 时人是在选 b）
  const last = query.lastIndexOf("@");
  if (last >= 0) return { start: start + 1 + last, query: query.slice(last + 1) };
  return { start, query };
}

/** `@` 之前那一段文本（选中一项之后要把它原样留下） */
export function beforeMention(text: string, m: MentionQuery): string {
  return text.slice(0, m.start);
}

/**
 * 匹配 `@` 片段的会话。
 *
 * 排序讲究：**没在作用范围里的排前面**。人敲 `@` 多半是想加一台新机器，
 * 把已经选过的顶在最前面，等于让他每次都要多按一下方向键。
 */
export function matchSessions<T extends { id: string; label: string }>(
  all: T[],
  picked: string[],
  query: string
): T[] {
  const q = query.trim().toLowerCase();
  const inScope = new Set(picked);
  const hit = (s: T) => (q === "" ? true : s.label.toLowerCase().includes(q));
  const fresh = all.filter((s) => !inScope.has(s.id) && hit(s));
  const already = all.filter((s) => inScope.has(s.id) && hit(s));
  return [...fresh, ...already].slice(0, 8);
}
