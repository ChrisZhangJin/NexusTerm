/** 文件工具的差异预览：逐行着色（增绿删红），多区块逐个列出，行数多时折叠。
 *  预览不完整时在折叠区外放一条醒目横幅：批准写入的是完整内容，而这里看不到全部。 */

import { diffStat, incompleteNote, parseDiff } from "../lib/diff";

export function DiffView({ diff, open = false }: { diff: string; open?: boolean }) {
  const note = incompleteNote(diff);
  const lines = parseDiff(diff).filter((l, i) => !(note && i === 0 && l.kind === "note"));
  if (lines.length === 0 && !note) return null;
  const { added, removed, hunks } = diffStat(lines);
  return (
    <>
      {note && (
        <div className="ap-diff-incomplete" role="alert">
          {note}
        </div>
      )}
      {lines.length > 0 && (
        <details className="ap-diff" open={open || lines.length <= 30}>
          <summary>
            差异 <span className="ap-diff-add">+{added}</span> <span className="ap-diff-del">-{removed}</span>
            {hunks > 1 && <span className="ap-diff-meta"> · {hunks} 处</span>}
          </summary>
          <pre className="ap-code ap-diff-body">
            {lines.map((l, i) => (
              <div key={i} className={`ap-diff-${l.kind}`}>
                {l.text || " "}
              </div>
            ))}
          </pre>
        </details>
      )}
    </>
  );
}
