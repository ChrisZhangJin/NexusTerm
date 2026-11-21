/**
 * SFTP 面板的文件名搜索。
 *
 * 没有 `*` 时是**精确匹配**（整名相等，不分大小写）：`07.txt` 只命中 `07.txt`。
 * 要模糊就自己写 `*`，它匹配任意长度（含空）的任意字符：`*07.txt` 命中所有以 07.txt
 * 结尾的名字，`07*` 命中以 07 开头的，`*07*` 命中包含 07 的。
 * 空输入 = 不过滤。
 */
export function nameMatcher(pattern: string): (name: string) => boolean {
  const q = pattern.trim().toLowerCase();
  if (!q) return () => true;
  if (!q.includes("*")) return (name) => name.toLowerCase() === q;
  const re = new RegExp(
    "^" +
      q
        .split("*")
        .map((part) => part.replace(/[.+?^${}()|[\]\\]/g, "\\$&"))
        .join(".*") +
      "$"
  );
  return (name) => re.test(name.toLowerCase());
}
