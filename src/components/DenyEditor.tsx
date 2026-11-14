/**
 * 托管契约的「禁止项」编辑器：开始托管的表单与托管卡共用。
 *
 * 底线项（替你输密码、选「永久允许」）只读、带锁；其余每项一个 ✕（= 放开这类操作），
 * 还能加自定义关键字（命中即拒绝）。改动立即回调，由调用方决定落到哪（本地偏好 / host_amend）。
 */
import { useState } from "react";
import { addDeny, denyLabel, isLockedDeny, removeDeny } from "../lib/host";

interface DenyEditorProps {
  value: readonly string[];
  onChange: (next: string[]) => void;
  disabled?: boolean;
  /** 被拒绝过的那一类（托管卡上高亮，提示「点 ✕ 放开」） */
  highlight?: string | null;
}

export default function DenyEditor({ value, onChange, disabled, highlight }: DenyEditorProps) {
  const [draft, setDraft] = useState("");
  const add = () => {
    const next = addDeny(value, draft);
    setDraft("");
    if (next.length !== value.length) onChange(next);
  };
  return (
    <div className="deny-editor">
      <span className="deny-title" title="命中禁止项的操作一律不执行（任何授权档位都一样）。点 ✕ 放开这一类。">
        禁止项
      </span>
      {value.map((tag) => {
        const locked = isLockedDeny(tag);
        return (
          <span
            key={tag}
            className={`deny-chip${locked ? " locked" : ""}${highlight === tag ? " hit" : ""}`}
            title={locked ? "底线：不能放开" : "点 ✕ 放开这一类操作"}
          >
            {locked ? "🔒 " : ""}
            {denyLabel(tag)}
            {!locked && (
              <button
                className="deny-x"
                disabled={disabled}
                aria-label={`放开：${denyLabel(tag)}`}
                onClick={() => onChange(removeDeny(value, tag))}
              >
                ✕
              </button>
            )}
          </span>
        );
      })}
      <input
        className="deny-add"
        value={draft}
        disabled={disabled}
        placeholder="+ 关键字（如 systemctl）"
        onChange={(e) => setDraft(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            add();
          }
        }}
        onBlur={() => draft.trim() && add()}
      />
    </div>
  );
}
