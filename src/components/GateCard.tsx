/**
 * 托管门禁卡（kind:"gate"）：一次写入被拒绝 / 要你批准 / 执行后通知时，说清想做什么、为什么、
 * 当时屏幕被识别成什么。被拒的卡上可以一键「这是误判」（存回归样本并告诉 AI）或「放开这一类」（改契约）。
 */

import { useState } from "react";
import { api } from "../api";
import { gateActions } from "../lib/host";
import { toFriendlyError } from "../lib/errors";
import type { AgentStep } from "../types";

/** 卡片要的托管上下文：没有（对话页）就不给按钮 */
export interface GateHost {
  contractId: string;
  deny: string[];
}

const VERDICT_TAGS: Record<string, string> = {
  refused: "已拒绝",
  asked: "等你批准",
  notified: "已执行 · 通知你",
};

export function GateCard({ step, host }: { step: AgentStep; host?: GateHost | null }) {
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
      return "已把这一屏存成误判样本，并告诉 AI 屏幕被认错了、换个做法重试";
    });
  const release = () =>
    run(async () => {
      await api.hostAmend(host!.contractId, undefined, undefined, acts.release!);
      return `已放开「${acts.item?.label ?? step.denyItem}」，AI 会在下一步收到契约更新`;
    });

  return (
    <div className={`ap-step gate ${verdict}`}>
      <div className="ap-step-head">
        <span className="ap-step-tag">{VERDICT_TAGS[verdict] ?? verdict}</span>
        {step.text ? <span className="ap-step-where">{step.text}</span> : null}
      </div>
      {step.command ? <pre className="ap-code">{step.command}</pre> : null}
      {step.reasons?.length ? <div className="ap-step-why">原因：{step.reasons.join("；")}</div> : null}
      {acts.item && (step.denyLocked || !acts.release) && verdict === "refused" ? (
        <div className="ap-step-why" title={acts.item.hint}>
          禁止项「{acts.item.label}」：{acts.item.hint}
        </div>
      ) : null}
      {host && !done && (acts.misread || acts.release) ? (
        <div className="ap-approve-row">
          {acts.misread ? (
            <button className="st-btn" disabled={busy} onClick={() => void misread()} title="屏幕其实不是这样：存下这一屏做回归样本，并告诉 AI">
              这是误判
            </button>
          ) : null}
          {acts.release ? (
            <button className="st-btn" disabled={busy} onClick={() => void release()} title={acts.item?.hint}>
              放开这一类
            </button>
          ) : null}
        </div>
      ) : null}
      {done ? <div className="ap-approve-done">{done}</div> : null}
      {err ? <div className="ap-secret-err ap-step-why">{err}</div> : null}
    </div>
  );
}
