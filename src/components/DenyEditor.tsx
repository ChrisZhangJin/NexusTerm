/**
 * 托管契约的「禁止项」编辑器：开始托管的表单与托管卡共用。
 *
 * 底线项（替你输密码、选「永久允许」）只读、带锁；其余每项一个 ✕（= 放开这类操作），
 * 还能加自定义关键字（命中即拒绝）。改动立即回调，由调用方决定落到哪（本地偏好 / host_amend）。
 */
import { useState } from "react";
import { t, useLocale } from "../i18n";
import { addDeny, denyHint, denyLabel, isLockedDeny, removeDeny } from "../lib/host";

interface DenyEditorProps {
  value: readonly string[];
  onChange: (next: string[]) => void;
  disabled?: boolean;
  /** 被拒绝过的那一类（托管卡上高亮，提示「点 ✕ 放开」） */
  highlight?: string | null;
}

export default function DenyEditor({ value, onChange, disabled, highlight }: DenyEditorProps) {
  useLocale();
  const [draft, setDraft] = useState("");
  const add = () => {
    const next = addDeny(value, draft);
    setDraft("");
    if (next.length !== value.length) onChange(next);
  };
  return (
    <div className="deny-editor">
      <span
        className="deny-title"
        title={t(
          "命中禁止项的操作一律不执行（任何授权档位都一样）。点 ✕ 放开这一类。",
          "Actions that hit a blocked item are never run, at any permission level. Click ✕ to allow that kind."
        )}
      >
        {t("禁止项", "Blocked actions")}
      </span>
      {value.map((tag) => {
        const locked = isLockedDeny(tag);
        return (
          <span
            key={tag}
            className={`deny-chip${locked ? " locked" : ""}${highlight === tag ? " hit" : ""}`}
            title={denyHint(tag)}
          >
            {locked ? "🔒 " : ""}
            {denyLabel(tag)}
            {!locked && (
              <button
                className="deny-x"
                disabled={disabled}
                aria-label={t("放开：{label}", "Allow: {label}", { label: denyLabel(tag) })}
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
        placeholder={t("+ 关键字（如 systemctl）", "+ keyword (e.g. systemctl)")}
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
