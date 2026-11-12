/**
 * 文件工具的差异预览（unified diff）→ 逐行着色用的结构。纯函数，node:test 直接测。
 * 后端的 diff 只供展示（`files::unified_diff`），这里也只按行首字符分类，不当补丁解析。
 */

export type DiffLineKind = "meta" | "hunk" | "add" | "del" | "ctx";

export interface DiffLine {
  kind: DiffLineKind;
  text: string;
}

export function parseDiff(diff: string): DiffLine[] {
  const out: DiffLine[] = [];
  for (const line of diff.replace(/\r\n/g, "\n").split("\n")) {
    if (line === "" && out.length > 0 && out[out.length - 1].text === "") continue;
    let kind: DiffLineKind;
    if (line.startsWith("+++") || line.startsWith("---")) kind = "meta";
    else if (line.startsWith("@@")) kind = "hunk";
    else if (line.startsWith("+")) kind = "add";
    else if (line.startsWith("-")) kind = "del";
    else kind = "ctx";
    out.push({ kind, text: line });
  }
  while (out.length > 0 && out[out.length - 1].text === "") out.pop();
  return out;
}

/** 增 / 删行数（卡片标题用） */
export function diffStat(lines: DiffLine[]): { added: number; removed: number } {
  return {
    added: lines.filter((l) => l.kind === "add").length,
    removed: lines.filter((l) => l.kind === "del").length,
  };
}
