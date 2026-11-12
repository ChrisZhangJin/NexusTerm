/**
 * 文件工具的差异预览（unified diff）→ 逐行着色用的结构。纯函数，node:test 直接测。
 * 后端的 diff 只供展示（`files::diff_preview`），这里也只按行首字符分类，不当补丁解析。
 *
 * 后端产出的是**多区块** diff（每处修改一个 `@@` hunk）。预览不完整时（差异超过上限、
 * 原文件过大或是二进制）第一行以 {@link INCOMPLETE_MARK} 开头：界面必须醒目提示，
 * 因为批准后写入的是完整内容，而卡上看不到全部。
 */

export type DiffLineKind = "meta" | "hunk" | "add" | "del" | "ctx" | "note";

export interface DiffLine {
  kind: DiffLineKind;
  text: string;
}

/** 与后端 `files::DIFF_INCOMPLETE_MARK` 一致 */
export const INCOMPLETE_MARK = "⚠ 预览不完整";

export function parseDiff(diff: string): DiffLine[] {
  const out: DiffLine[] = [];
  // 文件头（`---` / `+++`）只出现在第一个 hunk 之前；之后的 `---x` 是删掉了一行 `--x`
  let inHunk = false;
  for (const line of diff.replace(/\r\n/g, "\n").split("\n")) {
    if (line === "" && out.length > 0 && out[out.length - 1].text === "") continue;
    let kind: DiffLineKind;
    if (line.startsWith("@@")) {
      kind = "hunk";
      inHunk = true;
    } else if (!inHunk && (line.startsWith("+++") || line.startsWith("---"))) kind = "meta";
    else if (line.startsWith(INCOMPLETE_MARK) || line.startsWith("…（")) kind = "note";
    else if (line.startsWith("+")) kind = "add";
    else if (line.startsWith("-")) kind = "del";
    else kind = "ctx";
    out.push({ kind, text: line });
  }
  while (out.length > 0 && out[out.length - 1].text === "") out.pop();
  return out;
}

/** 预览是否不完整；是则返回那条说明（去掉标记前缀前的整行），否则 null */
export function incompleteNote(diff: string): string | null {
  const first = diff.replace(/^\s+/, "").split("\n", 1)[0] ?? "";
  return first.startsWith(INCOMPLETE_MARK) ? first : null;
}

/** 增 / 删行数与区块数（卡片标题用） */
export function diffStat(lines: DiffLine[]): { added: number; removed: number; hunks: number } {
  return {
    added: lines.filter((l) => l.kind === "add").length,
    removed: lines.filter((l) => l.kind === "del").length,
    hunks: lines.filter((l) => l.kind === "hunk").length,
  };
}
