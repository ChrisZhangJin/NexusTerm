/**
 * 一个托管任务的状态卡：胶囊（运行中 / 等你回答 / 你在驾驶 / 预算暂停 / 完成 / 失败 / 已停止）、
 * 等人处理的卡片（问题 + 选项 + 自由答复 / 审批），以及 接管 / 交还 / 加预算继续 / 暂停 / 停止。
 *
 * 卡片**永远等下去**：没有倒计时、没有默认动作（设计 §2 已定）。AgentPanel 与 StewardPanel 共用。
 */

import { useEffect, useRef, useState } from "react";
import { api, onHostNotify } from "../api";
import { DiffView } from "./DiffView";
import { needsIncompleteBanner } from "../lib/diff";
import { TodoList } from "./TodoList";
import { toFriendlyError } from "../lib/errors";
import { checkLines, criteriaSummary, formatReport, checksToText, hostButtons, hostDetail, hostPill, isTerminal, pendingCard, textToChecks } from "../lib/host";
import type {
  AgentTarget,
  HostBudgetGrant,
  HostCardReply,
  HostNotice,
  HostReattachCandidate,
  HostStartOpts,
  HostStatus,
} from "../types";
import DenyEditor from "./DenyEditor";
import HostText from "./HostText";
import { TIER_LABELS } from "../lib/steward";

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
  /** 完成后「基于此报告继续」：用新目标在同一个终端上起下一个托管，上一个目标与报告作开场上下文 */
  onFollowUp?: (goal: string, prev: HostStartOpts["followUp"]) => Promise<void>;
}

export default function HostCard({ status, label, grant, stopCtrlC, tabs = [], onFollowUp }: HostCardProps) {
  const [text, setText] = useState("");
  const [nextGoal, setNextGoal] = useState("");
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
  const [criteriaBig, setCriteriaBig] = useState(false);
  const criteriaRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!draft) return;
    setCriteria(checksToText(draft));
    // 这张卡挡着托管开始：一出现就滚到眼前
    criteriaRef.current?.scrollIntoView({ block: "nearest", behavior: "smooth" });
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
      {detail && status.phase.kind !== "completed" && <div className="st-esc-why">{detail}</div>}
      {status.phase.kind === "completed" && (
        <div className="host-report">
          <div className="host-report-head">
            <span>完成报告</span>
            <button
              className="st-btn"
              disabled={!status.phase.summary}
              onClick={() => void api.clipboardWrite(status.phase.kind === "completed" ? status.phase.summary : "").catch(() => {})}
            >
              复制
            </button>
          </div>
          {status.phase.summary.trim() ? (
            <HostText className="host-report-body" text={formatReport(status.phase.summary)} />
          ) : (
            <div className="host-report-body">（AI 没有写完成报告）</div>
          )}
          {status.phase.checks.length > 0 && (
            <details className="host-report-checks">
              <summary>核验依据（{status.phase.checks.length} 条）</summary>
              <ul className="host-checks">
                {checkLines(status.phase.checks).map((l, i) => (
                  <li key={i} className={l.startsWith("✓") ? "ok" : l.startsWith("·") ? "info" : "no"}>
                    {l}
                  </li>
                ))}
              </ul>
            </details>
          )}
          {onFollowUp && (
            <div className="host-followup">
              <textarea
                className="st-goal"
                rows={2}
                value={nextGoal}
                disabled={busy}
                placeholder="基于这份报告接着做什么？例如：按方案执行 A 到 D 项，每项执行前后报告释放的空间"
                onChange={(e) => setNextGoal(e.target.value)}
              />
              <button
                className="st-btn primary"
                disabled={busy || !nextGoal.trim()}
                title="在同一个终端上开始下一个托管；上一个目标与这份完成报告会作为它的开场上下文"
                onClick={() =>
                  void act(async () => {
                    if (status.phase.kind !== "completed") return;
                    await onFollowUp(nextGoal.trim(), { goal: status.goal, summary: status.phase.summary });
                    setNextGoal("");
                  })
                }
              >
                基于此报告继续
              </button>
            </div>
          )}
        </div>
      )}
      {status.notice && <div className="st-status warn">{status.notice}</div>}
      {live && !btn.reattach && status.tier && (
        <div className="host-tier-row">
          <span className="deny-title">授权档位</span>
          <select
            className={`st-select${status.tier === "yolo" ? " st-select-yolo" : ""}`}
            value={status.tier}
            disabled={busy}
            title="运行中也能改，下一步起生效"
            onChange={(e) => void act(() => api.hostAmend(cid, undefined, e.target.value))}
          >
            {(["auto", "notify", "approve", "yolo"] as const).map((t) => (
              <option key={t} value={t}>
                {TIER_LABELS[t]}
              </option>
            ))}
          </select>
          {status.tier === "yolo" && <span className="st-meta host-yolo on">只有越过禁止项才会停下来问你</span>}
        </div>
      )}
      {live && !btn.reattach && (
        <DenyEditor
          value={status.deny ?? []}
          disabled={busy}
          onChange={(next) => void act(() => api.hostAmend(cid, undefined, undefined, next))}
        />
      )}
      {status.todo && status.todo.length > 0 && <TodoList items={status.todo} />}

      {btn.reattach && (
        <div className="st-esc host-ask">
          <div className="st-esc-head">
            {status.phase.kind === "suspended" && status.phase.reason === "consoleGone"
              ? "被托管的终端断开了"
              : "应用重启了，托管停在这里"}
          </div>
          {status.phase.kind === "suspended" && status.phase.reason === "consoleGone" && (
            <div className="st-esc-why">
              如果是网络断了：这个标签页重连成功后，托管会在几秒内<b>自动接着跑</b>（AI 先读屏核对现场），不用做任何事。
              标签页已经关掉了，才需要在下面手动接回。
            </div>
          )}
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
        <div className="host-criteria" ref={criteriaRef}>
          <div className="host-criteria-head">
            <span className="host-criteria-step">第 1 步</span>
            <span className="host-criteria-title">确认完成判据</span>
            <span className="host-criteria-wait">托管还没开始，等你接受</span>
          </div>
          <div className="host-criteria-why">
            AI 按目标起草了下面的判据，用来判断「做完了没有」。一行一条，可直接改；接受后托管才开始。
          </div>
          <div className="host-criteria-legend">
            {criteriaSummary(criteria).map((x) => (
              <span key={x.label} className={`host-criteria-kind${x.blocking ? "" : " soft"}`}>
                {x.label} {x.n}
              </span>
            ))}
            <span className="host-criteria-hint" title="命令：在终端当前目录另开 shell 执行，退出码 0 算通过（按授权档位，危险的先问你）&#10;文件：路径存在算通过&#10;屏幕：正则命中终端屏幕算通过&#10;人工：只列进报告，不挡完成">
              前缀：命令 / 文件 / 屏幕 / 人工（人工项不挡完成）ⓘ
            </span>
          </div>
          <textarea
            className={`host-criteria-text${criteriaBig ? " big" : ""}`}
            rows={Math.min(16, Math.max(6, criteria.split("\n").length + 1))}
            spellCheck={false}
            value={criteria}
            disabled={busy}
            onChange={(e) => setCriteria(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && (e.ctrlKey || e.metaKey) && !busy) {
                e.preventDefault();
                void act(() => api.hostConfirmCriteria(cid, card.id, textToChecks(criteria)));
              }
            }}
          />
          <div className="host-criteria-actions">
            <button className="st-btn" disabled={busy} onClick={() => setCriteriaBig((b) => !b)}>
              {criteriaBig ? "收起" : "放大编辑"}
            </button>
            <button className="st-btn" disabled={busy || criteria === checksToText(draft)} onClick={() => setCriteria(checksToText(draft))}>
              恢复草稿
            </button>
            <span className="host-criteria-spacer" />
            <span className="host-criteria-kbd">Ctrl+Enter</span>
            <button
              className="st-btn primary host-criteria-go"
              disabled={busy}
              onClick={() => void act(() => api.hostConfirmCriteria(cid, card.id, textToChecks(criteria)))}
            >
              接受并开始托管
            </button>
          </div>
        </div>
      )}

      {card && secret && (
        <div className="st-esc host-ask">
          <div className="st-esc-head">等你输入密码</div>
          <HostText className="host-question-body" text={card.question} />
          <div className="st-esc-why">密码只在下方的密码卡片里输入，直接写进终端，AI 看不到它。不限时。</div>
        </div>
      )}

      {card && !draft && !secret && (
        <div className="st-esc host-ask">
          <div className="st-esc-head">
            {manualCheck ? "等你确认完成" : approve ? "等你批准" : card.kind.kind === "watchdog" ? "看门狗：它好像卡住了" : "等你回答"}
          </div>
          <HostText className="host-question-body" text={card.question} />
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
          {card.recommendation && (
            <div className="host-reco">
              <span className="host-reco-tag">AI 建议</span>
              <HostText text={card.recommendation} />
            </div>
          )}
          {needsIncompleteBanner(card.previewComplete, card.diff) && (
            <div className="ap-diff-incomplete" role="alert">
              ⚠ 预览不完整：卡上看不到全部修改，批准写入的是完整内容
            </div>
          )}
          {card.diff && <DiffView diff={card.diff} open />}
          {card.options.length > 0 && (
            <div className="host-options">
              {card.options.map((o, i) => (
                <button
                  key={`${o.label}-${i}`}
                  className="st-btn host-option"
                  disabled={busy}
                  title={o.reply}
                  onClick={() => void answer({ text: o.reply })}
                >
                  {o.label}
                </button>
              ))}
            </div>
          )}
          <div className="host-answer-row">
            <textarea
              className="st-input host-answer"
              rows={Math.min(5, Math.max(1, text.split("\n").length))}
              value={text}
              disabled={busy}
              onChange={(e) => setText(e.target.value)}
              onKeyDown={(e) => {
                // Enter 发送，Shift+Enter 换行
                if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing && !approve && text.trim()) {
                  e.preventDefault();
                  void answer({ text: text.trim() });
                }
              }}
              placeholder={approve ? "可选：附一句话给 AI" : card.options.length > 0 ? "或者自己写答复（Enter 发送，Shift+Enter 换行）" : "写下你的答复（Enter 发送，Shift+Enter 换行）"}
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
