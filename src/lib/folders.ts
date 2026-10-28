/**
 * 连接分组的路径计算。
 *
 * 分组没有自己的表：一个分组之所以存在，只是因为有连接的 `folder` 字段指向它
 * （见 `SessionList.buildTree`）。所以「改名」「拖进另一个分组」这两件事落到
 * 存储层是同一个动作 —— 把子树下所有连接的 `folder` 换个前缀，一次 `reorder_sessions`
 * 写回。这里只负责算出新路径，不碰 IO，方便自测。
 *
 * 根目录在存储里写作 `"/"`，在这里一律用 `""` 表示；只有出库那一刻才换回去。
 */

import type { SessionRecord } from "../types";

/** 规整路径：去掉首尾斜杠、压掉中间的空段和空白段，空 / `/` 都是根（`""`）。 */
export function normFolder(raw: string): string {
  return (raw || "")
    .split("/")
    .map((seg) => seg.trim())
    .filter(Boolean)
    .join("/");
}

/** 父路径：`"a/b/c"` → `"a/b"`，顶层 → `""`。 */
export function parentFolder(path: string): string {
  const p = normFolder(path);
  const i = p.lastIndexOf("/");
  return i < 0 ? "" : p.slice(0, i);
}

/** 末段名：`"a/b/c"` → `"c"`。 */
export function leafFolder(path: string): string {
  const p = normFolder(path);
  return p.slice(p.lastIndexOf("/") + 1);
}

/**
 * 行内改名输入框里那行字 → 目标完整路径。返回 `null` 表示没变化 / 不用动。
 *
 * 两种写法，都支持 `/`（旧版把斜杠直接 `replace` 掉了，所以「改名成 a/b」
 * 和「移到 /a/b」都静默失效 —— 这正是要修的 bug）：
 *
 * - **以 `/` 开头** = 从根算起的绝对路径。`/office/train` 就是「挪到 office 下，叫 train」。
 * - **不以 `/` 开头** = 相对自己的父级。顶层的 `train` 填 `office/train` 同样是挪进 office；
 *   `office/train` 填 `prod/train` 得到 `office/prod/train`。
 *
 * 不在这里拦「移进自己的子孙」—— 那是语义错误，要给用户提示，交给
 * [`isSelfOrDescendant`] 的调用方报。
 */
export function resolveFolderInput(oldPath: string, input: string | null): string | null {
  const raw = (input ?? "").trim();
  if (!raw) return null;
  const parent = parentFolder(oldPath);
  const abs = raw.startsWith("/");
  const target = normFolder(abs || !parent ? raw : `${parent}/${raw}`);
  if (!target) return null; // 只填了「/」之类：没给出名字，当取消
  return target === normFolder(oldPath) ? null : target;
}

/**
 * `newPath` 是不是 `oldPath` 自己或它的子孙。
 *
 * 把分组拖进自己的子分组是个无解的动作（子树要成为自己的后代），必须挡住，
 * 否则前缀替换会算出一条无限嵌套的路径。
 */
export function isSelfOrDescendant(oldPath: string, newPath: string): boolean {
  const a = normFolder(oldPath);
  const b = normFolder(newPath);
  return b === a || b.startsWith(`${a}/`);
}

/**
 * 把分组 `oldPath` 整棵子树搬到 `newPath`，算出 `reorder_sessions` 要的
 * `[id, sortOrder, folder][]`。
 *
 * `sortOrder` 原样带过去：同层内的次序是连接自己的事，搬家不该打乱它。
 * 返回空数组表示这棵子树下一个连接都没有（分组本身也就不存在了），不用写库。
 */
export function folderMoveItems(
  sessions: SessionRecord[],
  oldPath: string,
  newPath: string
): [string, number, string][] {
  const from = normFolder(oldPath);
  if (!from) return []; // 根不是一个可搬运的分组
  const to = normFolder(newPath);
  const items: [string, number, string][] = [];
  for (const s of sessions) {
    const f = normFolder(s.folder);
    if (f !== from && !f.startsWith(`${from}/`)) continue;
    const rest = f.slice(from.length).replace(/^\//, ""); // 子树里的相对路径
    const next = to ? (rest ? `${to}/${rest}` : to) : rest;
    items.push([s.id, s.sortOrder, next || "/"]);
  }
  return items;
}

/**
 * 把分组 `oldPath` 拖到分组 `parentPath` 底下时的落点路径。
 * 末段名不变，只换父级；拖到根就是 `parentPath = ""`。
 */
export function folderDropTarget(oldPath: string, parentPath: string): string {
  const leaf = leafFolder(oldPath);
  const parent = normFolder(parentPath);
  return parent ? `${parent}/${leaf}` : leaf;
}
