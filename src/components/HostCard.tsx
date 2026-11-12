/**
 * 一个托管任务的状态卡：胶囊（运行中 / 等你回答 / 你在驾驶 / 预算暂停 / 完成 / 失败 / 已停止）、
 * 等人处理的卡片（问题 + 选项 + 自由答复 / 审批），以及 接管 / 交还 / 加预算继续 / 暂停 / 停止。
 *
 * 卡片**永远等下去**：没有倒计时、没有默认动作（设计 §2 已定）。AgentPanel 与 StewardPanel 共用。
 */

import { useState } from "react";
import { api } from "../api";
import { toFriendlyError } from "../lib/errors";
import { hostButtons, hostDetail, hostPill, pendingCard } from "../lib/host";
import type { HostBudgetGrant, HostCardReply, HostStatus } from "../types";

export interface HostCardProps {
  status: HostStatus;
  /** 终端的可读名字 */
  label?: string;
  /** 「加预算继续」时给的新上限（托管页的预算偏好）；不给 = 同样大小的一份新预算 */
  grant?: HostBudgetGrant;
  /** 停止时同时发 Ctrl-C */
  stopCtrlC?: boolean;
}

export default function HostCard({ status, label, grant, stopCtrlC }: HostCardProps) {
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const pill = hostPill(status.phase);
  const detail = hostDetail(status.phase);
  const card = pendingCard(status.phase);
  const btn = hostButtons(status.phase);
  const cid = status.contractId;

  const act = async (f: () => Promise<unknown>) => {
    if (busy) return;
    setBusy(true);
    setErr(null);
    try {
      await f();
    } catch (e) {
      setErr(toFriendlyError(e).message);
    } finally {
      setBusy(false);
    }
  };

  const answer = (reply: Omit<HostCardReply, "cardId">) =>
    card &&
    act(async () => {
      await api.hostAnswer(cid, { cardId: card.id, ...reply });
      setText("");
    });

  const approve = card?.kind.kind === "approve";

  return (
    <div className="st-card host-card">
      <div className="st-main-head">
        <span className={`st-badge host-pill ${pill.tone}`}>{pill.label}</span>
        <span className="st-name">{label ? `托管 · ${label}` : "托管"}</span>
        <span className="st-meta host-goal" title={status.goal}>
          {status.goal}
        </span>
      </div>
      {detail && <div className="st-esc-why">{detail}</div>}

      {card && (
        <div className="st-esc host-ask">
          <div className="st-esc-head">{approve ? "等你批准" : "等你回答"}</div>
          <div className="host-question">{card.question}</div>
          {card.reasons.length > 0 && <div className="st-esc-why">原因：{card.reasons.join("；")}</div>}
          {card.recommendation && <div className="st-esc-why">建议：{card.recommendation}</div>}
          {card.options.length > 0 && (
            <div className="st-form-row st-center">
              {card.options.map((o, i) => (
                <button
                  key={`${o.label}-${i}`}
                  className="st-btn"
                  disabled={busy}
                  title={o.reply}
                  onClick={() => void answer({ text: o.reply })}
                >
                  {o.label}
                </button>
              ))}
            </div>
          )}
          <div className="st-form-row st-center">
            <input
              className="st-input"
              value={text}
              disabled={busy}
              onChange={(e) => setText(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.nativeEvent.isComposing && !approve && text.trim()) {
                  void answer({ text: text.trim() });
                }
              }}
              placeholder={approve ? "可选：附一句话给 AI" : "写下你的答复"}
            />
            {approve ? (
              <>
                <button className="st-btn primary" disabled={busy} onClick={() => void answer({ text: text.trim(), approved: true })}>
                  批准
                </button>
                <button className="st-btn danger" disabled={busy} onClick={() => void answer({ text: text.trim(), approved: false })}>
                  拒绝
                </button>
              </>
            ) : (
              <button className="st-btn primary" disabled={busy || !text.trim()} onClick={() => void answer({ text: text.trim() })}>
                回答
              </button>
            )}
          </div>
        </div>
      )}

      {(btn.takeover || btn.handback || btn.resume || btn.pause || btn.stop) && (
        <div className="st-form-row st-center">
          {btn.resume && (
            <button className="st-btn primary" disabled={busy} onClick={() => void act(() => api.hostResume(cid, grant))}>
              加预算继续
            </button>
          )}
          {btn.handback && (
            <button className="st-btn primary" disabled={busy} onClick={() => void act(() => api.hostHandback(cid))}>
              交还
            </button>
          )}
          {btn.takeover && (
            <button
              className="st-btn"
              disabled={busy}
              title="让 AI 立刻让路（你在终端里敲键也会自动触发），直到你点「交还」"
              onClick={() => void act(() => api.hostTakeover(cid))}
            >
              接管
            </button>
          )}
          {btn.pause && (
            <button className="st-btn" disabled={busy} onClick={() => void act(() => api.hostPause(cid))}>
              暂停
            </button>
          )}
          {btn.stop && (
            <button className="st-btn danger" disabled={busy} onClick={() => void act(() => api.hostStop(cid, stopCtrlC))}>
              停止
            </button>
          )}
        </div>
      )}
      {err && <div className="st-err">{err}</div>}
    </div>
  );
}
