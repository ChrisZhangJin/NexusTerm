/** 文件工具的差异预览：逐行着色（增绿删红），行数多时折叠。 */

import { diffStat, parseDiff } from "../lib/diff";

export function DiffView({ diff, open = false }: { diff: string; open?: boolean }) {
  const lines = parseDiff(diff);
  if (lines.length === 0) return null;
  const { added, removed } = diffStat(lines);
  return (
    <details className="ap-diff" open={open || lines.length <= 30}>
      <summary>
        差异 <span className="ap-diff-add">+{added}</span> <span className="ap-diff-del">-{removed}</span>
      </summary>
      <pre className="ap-code ap-diff-body">
        {lines.map((l, i) => (
          <div key={i} className={`ap-diff-${l.kind}`}>
            {l.text || " "}
          </div>
        ))}
      </pre>
    </details>
  );
}
