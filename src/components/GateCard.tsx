/**
 * 托管门禁卡（kind:"gate"）：一次写入被拒绝 / 要你批准 / 执行后通知时，说清想做什么、为什么、
 * 当时屏幕被识别成什么。被拒的卡上可以一键「这是误判」（存回归样本并告诉 AI）或「放开这一类」（改契约）。
 * 「要你批准」的只渲染成一行：批准 / 拒绝在托管卡（HostCard）的审批卡上做。
 */

import { useState } from "react";
import { api } from "../api";
import { t, useLocale } from "../i18n";
import { localized } from "../lib/localized";
import { gateActions } from "../lib/host";
import { toFriendlyError } from "../lib/errors";
import type { AgentStep } from "../types";

/** 卡片要的托管上下文：没有（对话页）就不给按钮 */
export interface GateHost {
  contractId: string;
  deny: string[];
}

const VERDICT_TAGS: Record<string, string> = localized({
  refused: ["已拒绝", "Refused"],
  asked: ["等你批准", "Waiting for your approval"],
  notified: ["已执行 · 通知你", "Done · notifying you"],
});

export function GateCard({ step, host }: { step: AgentStep; host?: GateHost | null }) {
  useLocale();
  const verdict = step.gateVerdict ?? "refused";
  const acts = gateActions(step, host?.deny);
  const [busy, setBusy] = useState(false);
  const [done, setDone] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);

  const run = async (fn: () => Promise<string>) => {
    setBusy(true);
    setErr(null);
    try {
      setDone(await fn());
    } catch (e) {
      setErr(toFriendlyError(e).message);
    } finally {
      setBusy(false);
    }
  };

  const misread = () =>
    run(async () => {
      await api.hostReportMisread(host!.contractId, step.gateId);
      return t(
        "已把这一屏存成误判样本，并告诉 AI 屏幕被认错了、换个做法重试",
        "Saved this screen as a misread sample and told the AI the screen was misread so it tries another way"
      );
    });
  const release = () =>
    run(async () => {
      await api.hostAmend(host!.contractId, undefined, undefined, acts.release!);
      return t("已放开「{item}」，AI 会在下一步收到契约更新", "Allowed \"{item}\"; the AI gets the contract update on its next step", {
        item: acts.item?.label ?? step.denyItem ?? "",
      });
    });

  // 要你批准的那一次：能操作的是托管卡上的审批卡，实况里只留一行，免得同一件事出现两张卡
  if (verdict === "asked") {
    const what = step.command?.trim() || step.text || t("一次写入", "a write");
    return (
      <div className="ap-gate-line" title={[step.text, step.command, step.reasons?.join(t("；", "; "))].filter(Boolean).join("\n")}>
        <span className="ap-gate-line-text">
          {t("⏸ 等你批准：{what}（见上方托管卡）", "⏸ Waiting for your approval: {what} (see the autopilot card above)", { what })}
        </span>
        {host && acts.misread && !done ? (
          <button className="ap-gate-line-btn" disabled={busy} onClick={() => void misread()} title={t("屏幕其实不是这样：存下这一屏做回归样本，并告诉 AI", "The screen isn't actually like that: save it as a regression sample and tell the AI")}>
            {t("这是误判", "Misread")}
          </button>
        ) : null}
        {done ? <span className="ap-gate-line-done">{done}</span> : null}
        {err ? <span className="ap-secret-err">{err}</span> : null}
      </div>
    );
  }

  return (
    <div className={`ap-step gate ${verdict}`}>
      <div className="ap-step-head">
        <span className="ap-step-tag">{VERDICT_TAGS[verdict] ?? verdict}</span>
        {step.text ? <span className="ap-step-where">{step.text}</span> : null}
      </div>
      {step.command ? <pre className="ap-code">{step.command}</pre> : null}
      {step.reasons?.length ? <div className="ap-step-why">{t("原因：{why}", "Reason: {why}", { why: step.reasons.join(t("；", "; ")) })}</div> : null}
      {acts.item && (step.denyLocked || !acts.release) && verdict === "refused" ? (
        <div className="ap-step-why" title={acts.item.hint}>
          {t("禁止项「{label}」：{hint}", "Blocked action \"{label}\": {hint}", { label: acts.item.label, hint: acts.item.hint })}
        </div>
      ) : null}
      {host && !done && (acts.misread || acts.release) ? (
        <div className="ap-approve-row">
          {acts.misread ? (
            <button className="st-btn" disabled={busy} onClick={() => void misread()} title={t("屏幕其实不是这样：存下这一屏做回归样本，并告诉 AI", "The screen isn't actually like that: save it as a regression sample and tell the AI")}>
              {t("这是误判", "Misread")}
            </button>
          ) : null}
          {acts.release ? (
            <button className="st-btn" disabled={busy} onClick={() => void release()} title={acts.item?.hint}>
              {t("放开这一类", "Allow this kind")}
            </button>
          ) : null}
        </div>
      ) : null}
      {done ? <div className="ap-approve-done">{done}</div> : null}
      {err ? <div className="ap-secret-err ap-step-why">{err}</div> : null}
    </div>
  );
}
