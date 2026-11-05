import { memo, useMemo } from "react";
import type { SkillRisk, SkillStep } from "../types";
import { describeSkillStep, RISK_LABEL, STEP_KIND_LABEL } from "../lib/skill";
import { placeholderRefs, placeholderSegments } from "../lib/skillForm";
import "./SkillManager.css";

/** 风险徽标。风险只读：由后端评估、只会被抬高，界面不提供修改入口。 */
export function RiskBadge({ risk, title }: { risk: SkillRisk; title?: string }) {
  return (
    <span className={`sk-risk ${risk}`} title={title ?? "风险等级由后端评估，只会被抬高、不能手动降低"}>
      {RISK_LABEL[risk]}
    </span>
  );
}

/** 把文本里的 `{{param}}` 高亮：已声明为绿，未声明为红，secret 参数带锁。 */
export const Highlighted = memo(function Highlighted({
  text,
  declared,
  secretNames,
}: {
  text: string;
  declared: ReadonlySet<string>;
  secretNames?: ReadonlySet<string>;
}) {
  const segs = useMemo(() => placeholderSegments(text, declared, secretNames), [text, declared, secretNames]);
  return (
    <>
      {segs.map((s, i) =>
        s.kind === "text" ? (
          <span key={i}>{s.text}</span>
        ) : (
          <span
            key={i}
            className={`sk-ph ${s.kind === "param" ? "ok" : "bad"}${s.secret ? " secret" : ""}`}
            title={s.kind === "param" ? (s.secret ? "secret 参数：运行时走密码通道" : "已声明的参数") : "未声明的参数"}
          >
            {s.text}
          </span>
        )
      )}
    </>
  );
});

/**
 * 输入框下方的占位符提示：这段文本引用了哪些参数、有没有声明。
 * `allowSecret` 为真时（secret 应答 / secret 步骤）secret 参数是合法用法，否则标出警告
 * （后端会拒绝把 secret 参数代入命令 / 按键 / pattern）。
 */
export const PlaceholderChips = memo(function PlaceholderChips({
  text,
  declared,
  secretNames,
  allowSecret,
}: {
  text: string;
  declared: ReadonlySet<string>;
  secretNames: ReadonlySet<string>;
  allowSecret?: boolean;
}) {
  const refs = useMemo(() => placeholderRefs(text, declared, secretNames), [text, declared, secretNames]);
  if (refs.length === 0) return null;
  return (
    <div className="sk-chips">
      {refs.map((r) => {
        const bad = !r.declared;
        const misuse = r.declared && r.secret && !allowSecret;
        return (
          <span
            key={r.name}
            className={`sk-ph ${bad || misuse ? "bad" : "ok"}${r.secret ? " secret" : ""}`}
            title={
              bad
                ? "未声明的参数：请在「参数」里添加"
                : misuse
                  ? "secret 参数只能用于密码通道（secret 步骤 / secret 应答），不能代入命令、按键或 pattern"
                  : r.secret
                    ? "secret 参数：运行时走密码通道"
                    : "已声明的参数"
            }
          >
            {`{{${r.name}}}`}
            {bad && " 未声明"}
            {misuse && " 不可用于此处"}
          </span>
        );
      })}
    </div>
  );
});

/** 只读的步骤列表（导入预览用） */
export function StepPreview({
  steps,
  declared,
  secretNames,
}: {
  steps: SkillStep[];
  declared: ReadonlySet<string>;
  secretNames: ReadonlySet<string>;
}) {
  return (
    <ol className="sk-steplist">
      {steps.map((s, i) => (
        <li key={i}>
          <span className="sk-kind">{STEP_KIND_LABEL[s.kind]}</span>
          <code className="sk-code">
            <Highlighted text={describeSkillStep(s)} declared={declared} secretNames={secretNames} />
          </code>
        </li>
      ))}
    </ol>
  );
}
