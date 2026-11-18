/**
 * AI 写给人的一段话（托管卡上的问题 / 建议 / 完成报告）：段落、列表、行内代码与路径。
 * 纯展示；切块规则见 `lib/host.ts` 的 `textBlocks` / `inlineParts`。
 */
import { inlineParts, textBlocks } from "../lib/host";

function Inline({ text }: { text: string }) {
  return (
    <>
      {inlineParts(text).map((p, i) =>
        p.code ? (
          <code key={i} className="host-text-code">
            {p.text}
          </code>
        ) : (
          <span key={i}>{p.text}</span>
        )
      )}
    </>
  );
}

export default function HostText({ text, className }: { text: string; className?: string }) {
  return (
    <div className={`host-text${className ? ` ${className}` : ""}`}>
      {textBlocks(text).map((b, i) =>
        b.kind === "p" ? (
          <p key={i}>
            <Inline text={b.text} />
          </p>
        ) : b.kind === "ul" ? (
          <ul key={i}>
            {b.items.map((t, j) => (
              <li key={j}>
                <Inline text={t} />
              </li>
            ))}
          </ul>
        ) : (
          <ol key={i}>
            {b.items.map((t, j) => (
              <li key={j}>
                <Inline text={t} />
              </li>
            ))}
          </ol>
        )
      )}
    </div>
  );
}
