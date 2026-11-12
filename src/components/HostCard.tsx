/**
 * 一个托管任务的状态卡：胶囊（运行中 / 等你回答 / 你在驾驶 / 预算暂停 / 完成 / 失败 / 已停止）、
 * 等人处理的卡片（问题 + 选项 + 自由答复 / 审批），以及 接管 / 交还 / 加预算继续 / 暂停 / 停止。
 *
 * 卡片**永远等下去**：没有倒计时、没有默认动作（设计 §2 已定）。AgentPanel 与 StewardPanel 共用。
 */

import { useEffect, useState } from "react";
import { api, onHostNotify } from "../api";
import { DiffView } from "./DiffView";
import { needsIncompleteBanner } from "../lib/diff";
import { TodoList } from "./TodoList";
import { toFriendlyError } from "../lib/errors";
import { checkLines, checksToText, hostButtons, hostDetail, hostPill, isTerminal, pendingCard, textToChecks } from "../lib/host";
import type {
  AgentTarget,
  HostBudgetGrant,
  HostCardReply,
  HostNotice,
  HostReattachCandidate,
  HostStatus,
} from "../types";

export interface HostCardProps {
  status: HostStatus;
  /** 终端的可读名字 */
  label?: string;
  /** 「加预算继续」时给的新上限（托管页的预算偏好）；不给 = 同样大小的一份新预算 */
  grant?: HostBudgetGrant;
  /** 停止时同时发 Ctrl-C */
  stopCtrlC?: boolean;
  /** 开着的 tab（待重新接驳时给候选排序用） */
  tabs?: AgentTarget[];
}

export default function HostCard({ status, label, grant, stopCtrlC, tabs = [] }: HostCardProps) {
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const pill = hostPill(status.phase);
  const detail = hostDetail(status.phase);
  const card = pendingCard(status.phase);
  const btn = hostButtons(status.phase);
  const cid = status.contractId;
  const live = !isTerminal(status.phase);
  // 待重新接驳：开着的 tab 交给后端按相似度排序（host_key → SSH 记录 → 标签 → cwd）
  const [cands, setCands] = useState<HostReattachCandidate[]>([]);
  const tabKey = tabs.map((t) => t.id).join("|");
  useEffect(() => {
    if (!btn.reattach) return;
    let gone = false;
    api
      .hostReattachCandidates(
        cid,
        tabs.map((t) => t.hint ?? { consoleId: t.id, label: t.label })
      )
      .then((c) => !gone && setCands(c))
      .catch(() => !gone && setCands([]));
    return () => {
      gone = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [cid, btn.reattach, tabKey]);
  // 判据草稿：一行一条，可改；换了一张卡就重置
  const draft = card?.kind.kind === "criteriaDraft" ? card.kind.draft : null;
  const [criteria, setCriteria] = useState("");
  useEffect(() => {
    if (draft) setCriteria(checksToText(draft));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [card?.id]);
  // 改目标（host_amend）
  const [goalDraft, setGoalDraft] = useState<string | null>(null);
  // 通知档写入后的通知卡（只留最近几条）
  const [notices, setNotices] = useState<HostNotice[]>([]);
  const [correction, setCorrection] = useState("");
  useEffect(() => {
    let off: (() => void) | null = null;
    let disposed = false;
    void onHostNotify(cid, (n) => setNotices((l) => [n, ...l].slice(0, 3))).then((f) => {
      if (disposed) f();
      else off = f;
    });
    return () => {
      disposed = true;
      off?.();
    };
  }, [cid]);

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

  // 审批卡与「只剩人工项」的完成确认卡都是「批准 / 驳回」
  const approve = card?.kind.kind === "approve" || card?.kind.kind === "manualCheck";
  const manualCheck = card?.kind.kind === "manualCheck";
  const secret = card?.kind.kind === "secret";

  /** 通知卡的「撤销」：没有真回滚 —— 先接管（AI 立刻让路），再把纠正记成偏好 */
  const undo = (n: HostNotice) =>
    act(async () => {
      await api.hostTakeover(cid);
      await api.hostCorrect(cid, n.seq, n.text, correction.trim());
      setCorrection("");
      setNotices((l) => l.filter((x) => x.seq !== n.seq));
    });

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
      {status.phase.kind === "completed" && status.phase.checks.length > 0 && (
        <ul className="host-checks">
          {checkLines(status.phase.checks).map((l, i) => (
            <li key={i} className={l.startsWith("✓") ? "ok" : "no"}>
              {l}
            </li>
          ))}
        </ul>
      )}
      {status.notice && <div className="st-status warn">{status.notice}</div>}
      {status.todo && status.todo.length > 0 && <TodoList items={status.todo} />}

      {btn.reattach && (
        <div className="st-esc host-ask">
          <div className="st-esc-head">
            {status.phase.kind === "suspended" && status.phase.reason === "consoleGone"
              ? "被托管的终端已经关闭"
              : "应用重启了，托管停在这里"}
          </div>
          <div className="st-esc-why">
            选一个终端接回去（按和原来的相似度排序），或按保存的连接新开一个。接回后 AI 会先读屏核对现场；
            屏幕和原来不一样时会降为「确认」档，每次写入都先问你。
          </div>
          {cands.length === 0 && <div className="st-esc-why">没有找到相似的终端。</div>}
          <div className="st-form-row st-center">
            {cands.map((c) => (
              <button
                key={c.consoleId}
                className="st-btn"
                disabled={busy}
                onClick={() => void act(() => api.hostReattach(cid, c.consoleId))}
                title={`相似度 ${c.score}`}
              >
                接到「{c.label}」
              </button>
            ))}
            <button className="st-btn primary" disabled={busy} onClick={() => void act(() => api.hostReattach(cid, null, true))}>
              打开新连接
            </button>
            <button className="st-btn danger" disabled={busy} onClick={() => void act(() => api.hostStop(cid, false))}>
              放弃
            </button>
          </div>
        </div>
      )}

      {live && (
        <div className="st-form-row st-center">
          {goalDraft === null ? (
            <button className="st-btn" disabled={busy} onClick={() => setGoalDraft(status.goal)} title="改目标：不重启，AI 下一步就按新目标做">
              改目标
            </button>
          ) : (
            <>
              <input className="st-input" value={goalDraft} disabled={busy} onChange={(e) => setGoalDraft(e.target.value)} />
              <button
                className="st-btn primary"
                disabled={busy || !goalDraft.trim()}
                onClick={() =>
                  void act(async () => {
                    await api.hostAmend(cid, goalDraft.trim());
                    setGoalDraft(null);
                  })
                }
              >
                保存
              </button>
              <button className="st-btn" disabled={busy} onClick={() => setGoalDraft(null)}>
                取消
              </button>
            </>
          )}
        </div>
      )}

      {notices.map((n) => (
        <div key={n.seq} className="st-esc host-notice">
          <div className="st-esc-head">已执行（通知档）</div>
          <div className="host-question">{n.text}</div>
          {live && (
            <div className="st-form-row st-center">
              <input
                className="st-input"
                value={correction}
                disabled={busy}
                onChange={(e) => setCorrection(e.target.value)}
                placeholder="可选：哪里不对、下次该怎么做（会记成偏好）"
              />
              <button className="st-btn warn" disabled={busy} onClick={() => void undo(n)} title="AI 立刻让路，由你处理现场；纠正记成偏好">
                撤销（接管）
              </button>
              <button className="st-btn" disabled={busy} onClick={() => setNotices((l) => l.filter((x) => x.seq !== n.seq))}>
                知道了
              </button>
            </div>
          )}
        </div>
      ))}

      {card && draft && (
        <div className="st-esc host-ask">
          <div className="st-esc-head">确认完成判据</div>
          <div className="st-esc-why">
            AI 按目标起草了下面的判据，一行一条（前缀：命令 / 文件 / 屏幕 / 人工）。改好后点「接受」，托管才开始。接受后，「命令」类判据会在完成时按授权档位执行（危险的先问你）。
          </div>
          <textarea className="st-goal" rows={Math.max(3, criteria.split("\n").length)} value={criteria} disabled={busy} onChange={(e) => setCriteria(e.target.value)} />
          <div className="st-form-row st-center">
            <button
              className="st-btn primary"
              disabled={busy}
              onClick={() => void act(() => api.hostConfirmCriteria(cid, card.id, textToChecks(criteria)))}
            >
              接受
            </button>
          </div>
        </div>
      )}

      {card && secret && (
        <div className="st-esc host-ask">
          <div className="st-esc-head">等你输入密码</div>
          <div className="host-question">{card.question}</div>
          <div className="st-esc-why">密码只在下方的密码卡片里输入，直接写进终端，AI 看不到它。不限时。</div>
        </div>
      )}

      {card && !draft && !secret && (
        <div className="st-esc host-ask">
          <div className="st-esc-head">
            {manualCheck ? "等你确认完成" : approve ? "等你批准" : card.kind.kind === "watchdog" ? "看门狗：它好像卡住了" : "等你回答"}
          </div>
          <div className="host-question">{card.question}</div>
          {card.reasons.length > 0 &&
            (manualCheck || card.kind.kind === "watchdog" ? (
              <ul className="host-checks">
                {card.reasons.map((r, i) => (
                  <li key={i}>{r}</li>
                ))}
              </ul>
            ) : (
              <div className="st-esc-why">原因：{card.reasons.join("；")}</div>
            ))}
          {card.recommendation && <div className="st-esc-why">建议：{card.recommendation}</div>}
          {needsIncompleteBanner(card.previewComplete, card.diff) && (
            <div className="ap-diff-incomplete" role="alert">
              ⚠ 预览不完整：卡上看不到全部修改，批准写入的是完整内容
            </div>
          )}
          {card.diff && <DiffView diff={card.diff} open />}
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
                  {manualCheck ? "确认完成" : "批准"}
                </button>
                <button className="st-btn danger" disabled={busy} onClick={() => void answer({ text: text.trim(), approved: false })}>
                  {manualCheck ? "还没完成" : "拒绝"}
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
          {btn.stop && !btn.reattach && (
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
