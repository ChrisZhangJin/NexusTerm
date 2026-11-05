import { useEffect, useState } from "react";
import { api } from "../api";
import { useRecorder } from "../lib/useRecorder";
import type { SkillInput } from "../types";
import { cleanLines, mechanicalInput, refineHeadline, type RefinedSkillDraft } from "../lib/skillRefine";
import {
  describeStep,
  draftDurationMs,
  formatElapsed,
  hintFor,
  summarizeDraft,
} from "../lib/recorder";

/** 每秒刷新的「当前时刻」，只在 `on` 时跑 */
function useNow(on: boolean): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!on) return;
    setNow(Date.now());
    const t = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(t);
  }, [on]);
  return now;
}

/**
 * 🔴 录制中指示 + 已用时间（紧凑版，给面板头部用）。没在录制时不渲染。
 * 点它不做事，只是让用户在任何视图下都看得到「正在录」。
 */
export function RecordingBadge({ sessionId }: { sessionId: string }) {
  const { status } = useRecorder(sessionId);
  const now = useNow(status.recording);
  if (!status.recording) return null;
  return (
    <span className="rec-badge" title="正在录制这个终端里的操作">
      <span className="rec-dot" />
      录制中 {formatElapsed(status.startedAtMs, now)}
    </span>
  );
}

/** 开始 / 结束 / 取消录制的按钮组，放在时间线头部。 */
export function RecorderControls({
  sessionId,
  hostLabel,
}: {
  sessionId: string;
  hostLabel?: string;
}) {
  const { status, busy, start, stop, cancel } = useRecorder(sessionId);
  const now = useNow(status.recording);
  if (!status.recording) {
    return (
      <button
        className="ap-mini rec-btn"
        disabled={busy}
        onClick={() => void start(hostLabel)}
        title="开始录制：之后你在这个终端里的操作会被整理成一份可复用的步骤草稿"
      >
        ● 开始录制
      </button>
    );
  }
  return (
    <>
      <button
        className="ap-mini rec-btn on"
        disabled={busy}
        onClick={() => void stop()}
        title="结束录制并生成草稿"
      >
        ■ 结束录制 {formatElapsed(status.startedAtMs, now)}
      </button>
      <button className="ap-mini" disabled={busy} onClick={() => void cancel()} title="放弃这次录制，不生成草稿">
        取消
      </button>
    </>
  );
}

/**
 * 录制错误 + 最近一份草稿的预览。草稿留在 `useRecorder` 的 store 里。
 * 密码步骤只显示说明，永远没有内容；hint 是命令输出的末尾几行（已脱敏）。
 *
 * 两条路都只是「打开 SkillManager 预填草稿」，保存永远由用户在编辑器里点：
 * - 整理为技能（3.3）：让模型整理，后端夹紧并校验后返回；先在卡片里展示说明 / 警告，用户确认再打开；
 * - 直接编辑：不经模型，机械草稿原样进编辑器。
 */
export function RecorderDraftCard({
  sessionId,
  onOpenSkillDraft,
}: {
  sessionId: string;
  onOpenSkillDraft?: (draft: SkillInput) => void;
}) {
  const { draft, error, dismissDraft } = useRecorder(sessionId);
  const [showHints, setShowHints] = useState(false);
  const [refineHint, setRefineHint] = useState("");
  const [refining, setRefining] = useState(false);
  const [refineErr, setRefineErr] = useState<string | null>(null);
  const [refined, setRefined] = useState<RefinedSkillDraft | null>(null);

  // 草稿换了 / 没了，旧的整理结果就不再对应它
  useEffect(() => {
    setRefined(null);
    setRefineErr(null);
  }, [draft]);

  const refine = async () => {
    if (!draft || refining) return;
    setRefining(true);
    setRefineErr(null);
    setRefined(null);
    try {
      setRefined(await api.skillRefineDraft(draft, refineHint));
    } catch (e) {
      setRefineErr(String(e));
    } finally {
      setRefining(false);
    }
  };

  if (error) {
    return <div className="rec-card rec-err">录制失败：{error}</div>;
  }
  if (!draft) return null;
  const secs = Math.round(draftDurationMs(draft) / 1000);
  return (
    <div className="rec-card">
      <div className="rec-card-head">
        <span className="rec-card-title">录制草稿</span>
        <span className="tl-dim">
          {summarizeDraft(draft)} · {secs} 秒
          {draft.host.label ? ` · ${draft.host.label}` : ""}
        </span>
        <button className="ap-mini" onClick={() => setShowHints((v) => !v)} disabled={draft.hints.length === 0}>
          {showHints ? "隐藏输出" : "输出提示"}
        </button>
        <button className="ap-mini" onClick={dismissDraft} disabled={refining} title="丢弃这份草稿">
          丢弃
        </button>
      </div>
      {draft.steps.length > 0 && (
        <div className="rec-actions">
          <input
            className="rec-hint-input"
            value={refineHint}
            maxLength={500}
            disabled={refining}
            placeholder="给模型的提示（可选），例如：把主机名做成参数"
            onChange={(e) => setRefineHint(e.target.value)}
          />
          <button
            className="ap-mini rec-refine"
            disabled={refining}
            onClick={() => void refine()}
            title="让模型把这份录制整理成可复用的技能：提取参数、加期望与验证。整理结果会再经过后端校验，保存前你可以在编辑器里修改"
          >
            {refining ? "整理中…" : "✨ 整理为技能"}
          </button>
          <button
            className="ap-mini"
            disabled={refining || !onOpenSkillDraft}
            onClick={() => onOpenSkillDraft?.(mechanicalInput(draft))}
            title="不经模型，把录制的步骤原样放进编辑器自己改"
          >
            直接编辑
          </button>
        </div>
      )}
      {refineErr && <div className="rec-err-line">整理失败：{refineErr}</div>}
      {refined && (
        <div className={`rec-refined${refined.fallback ? " fb" : ""}`}>
          <div className="rec-refined-head">
            <span>{refineHeadline(refined)}</span>
            <button
              className="ap-mini"
              disabled={!onOpenSkillDraft}
              onClick={() => onOpenSkillDraft?.(refined.draft)}
              title="在技能编辑器里打开整理结果；保存由你在编辑器里确认"
            >
              打开编辑器
            </button>
          </div>
          {cleanLines(refined.warnings).length > 0 && (
            <ul className="rec-warn">
              {cleanLines(refined.warnings).map((w, i) => (
                <li key={i}>{w}</li>
              ))}
            </ul>
          )}
          {cleanLines(refined.notes).length > 0 && (
            <ul className="rec-refine-notes tl-dim">
              {cleanLines(refined.notes).map((n, i) => (
                <li key={i}>{n}</li>
              ))}
            </ul>
          )}
        </div>
      )}
      {draft.warnings.length > 0 && (
        <ul className="rec-warn">
          {draft.warnings.map((w, i) => (
            <li key={i}>{w}</li>
          ))}
        </ul>
      )}
      {draft.steps.length === 0 ? (
        <div className="tl-dim">没有可用的步骤。</div>
      ) : (
        <ol className="rec-steps">
          {draft.steps.map((s, i) => {
            const hint = showHints ? hintFor(draft, i) : undefined;
            return (
              <li key={i}>
                <span className={`rec-kind k-${s.kind}`}>{s.kind}</span>
                <span className="rec-step-text mono">{describeStep(s)}</span>
                {hint && <pre className="tl-out">{hint.outputTail}</pre>}
              </li>
            );
          })}
        </ol>
      )}
      {draft.notes.length > 0 && <div className="tl-dim rec-notes">{draft.notes.join("；")}</div>}
    </div>
  );
}
