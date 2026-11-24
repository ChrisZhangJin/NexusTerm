/**
 * 一个托管任务的状态卡，状态优先，从上到下：
 * 1. 卡头：胶囊（运行中 / 等你回答 / 你在驾驶 / 预算暂停 / 完成 / 失败 / 已停止）+ 此刻该点的按钮
 *    （交还 / 继续 / 暂停 / 接管 / 停止，见 `hostHeadActions`；全卡只有这一行控制）；
 * 2. 需要你的卡片紧跟卡头：确认判据 / 密码 / 问题 + 选项 + 自由答复 / 审批 / 看门狗；
 *    待重新接驳的候选；完成报告与「基于此报告继续」；
 * 3. 计划（紧凑）；
 * 4. 「契约 / 设置」折叠：档位、禁止项、改目标、停止时发 Ctrl-C、预算，收起时一行摘要。
 *
 * 卡片**永远等下去**：没有倒计时、没有默认动作（设计 §2 已定）。AgentPanel 与 StewardPanel 共用。
 */

import { useEffect, useRef, useState } from "react";
import { api, onHostNotify } from "../api";
import { t, useLocale } from "../i18n";
import { DiffView } from "./DiffView";
import { needsIncompleteBanner } from "../lib/diff";
import { TodoList } from "./TodoList";
import { toFriendlyError } from "../lib/errors";
import { checkLines, criteriaSummary, formatReport, checksToText, hostDetail, hostPill, isTerminal, needsReattach, pendingCard, textToChecks } from "../lib/host";
import { HOST_TIERS, hostHeadActions, hostSettingsSummary, type HostHeadAction } from "../lib/hostUi";
import { STEWARD_DURATION_OPTIONS } from "../lib/agentBudget";
import { fmtUsd, fromHostSpent, usageTooltip, usageWithTime } from "../lib/usage";
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
import HostChangesView from "./HostChangesView";
import { TIER_LABELS } from "../lib/steward";

export interface HostCardProps {
  status: HostStatus;
  /** 终端的可读名字 */
  label?: string;
  /** 「加预算继续」时给的新上限（托管页的预算偏好）；不给 = 同样大小的一份新预算 */
  grant?: HostBudgetGrant;
  /** 停止时同时发 Ctrl-C */
  stopCtrlC?: boolean;
  /** 给了就在「契约 / 设置」里显示「停止时同时发 Ctrl-C」开关 */
  onStopCtrlC?: (on: boolean) => void;
  /** 「停止」的实际动作（托管页还要一并停契约）；不给 = 直接 host_stop */
  onStop?: () => Promise<void>;
  /** 开着的 tab（待重新接驳时给候选排序用） */
  tabs?: AgentTarget[];
  /** 完成后「基于此报告继续」：用新目标在同一个终端上起下一个托管，上一个目标与报告作开场上下文 */
  onFollowUp?: (goal: string, prev: HostStartOpts["followUp"]) => Promise<void>;
}

const durationLabel = (m?: number) =>
  m === undefined ? "—" : (STEWARD_DURATION_OPTIONS.find((o) => o.minutes === m)?.label ?? t("{m} 分钟", "{m} min", { m }));

export default function HostCard({ status, label, grant, stopCtrlC, onStopCtrlC, onStop, tabs = [], onFollowUp }: HostCardProps) {
  useLocale();
  const [text, setText] = useState("");
  const [nextGoal, setNextGoal] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const pill = hostPill(status.phase);
  const detail = hostDetail(status.phase);
  const card = pendingCard(status.phase);
  const reattach = needsReattach(status.phase);
  const cid = status.contractId;
  const live = !isTerminal(status.phase);
  // 多终端托管：全部终端（主终端在前）；接驳一次接一个，先接主终端，再按顺序接其余不在的
  const terms = status.consoles ?? [];
  const multi = terms.length > 1;
  const gone = terms.filter((c) => c.missing);
  const slot = multi ? (gone.find((c) => c.primary) ?? gone[0] ?? null) : null;
  // 待重新接驳：开着的 tab 交给后端按相似度排序（host_key → SSH 记录 → 标签 → cwd）
  const [cands, setCands] = useState<HostReattachCandidate[]>([]);
  const tabKey = tabs.map((tab) => tab.id).join("|");
  useEffect(() => {
    if (!reattach) return;
    let gone = false;
    api
      .hostReattachCandidates(
        cid,
        tabs.map((tab) => tab.hint ?? { consoleId: tab.id, label: tab.label }),
        slot?.consoleId
      )
      .then((c) => !gone && setCands(c))
      .catch(() => !gone && setCands([]));
    return () => {
      gone = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [cid, reattach, tabKey, slot?.consoleId]);
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

  const acts = hostHeadActions(status.phase);
  const runAct = (a: HostHeadAction) => {
    switch (a.kind) {
      case "resume":
        return act(() => api.hostResume(cid, grant));
      case "handback":
        return act(() => api.hostHandback(cid));
      case "takeover":
        return act(() => api.hostTakeover(cid));
      case "pause":
        return act(() => api.hostPause(cid));
      case "stop":
        return act(() => (onStop ? onStop() : api.hostStop(cid, stopCtrlC)));
    }
  };
  const summary = hostSettingsSummary(status.tier, status.deny);
  // 卡头的「花了多少」：token · ≈钱 · 用时（结束后就是全程合计）
  const usage = status.spent ? fromHostSpent(status.spent) : null;
  const usageText = usage ? usageWithTime(usage) : "";
  const settings = live && !reattach;

  return (
    <div className="st-card host-card">
      {/* 卡头：一句话状态 + 此刻该点的按钮（全卡只有这一行控制） */}
      <div className="hc-head">
        <span className={`st-badge host-pill ${pill.tone}`}>{pill.label}</span>
        <span className="hc-title">{label ? t("托管 · {label}", "Autopilot · {label}", { label }) : t("托管", "Autopilot")}</span>
        {usage && usageText && (
          <span className="hc-usage" title={usageTooltip(usage)}>
            {usageText}
          </span>
        )}
        {acts.length > 0 && (
          <span className="hc-acts">
            {acts.map((a) => (
              <button
                key={a.kind}
                className={`st-btn${a.tone === "plain" ? "" : ` ${a.tone}`}`}
                disabled={busy}
                title={a.title}
                onClick={() => void runAct(a)}
              >
                {a.label}
              </button>
            ))}
          </span>
        )}
      </div>
      {multi && (
        <div className="hc-terms st-meta" title={t(
            "这次托管一起操作的终端：你在其中任何一个上动手，整个托管都会让路",
            "Terminals this autopilot drives together: if you act in any of them, the whole autopilot steps aside"
          )}
        >
          {t("终端：", "Terminals: ")}
          {terms.map((c, i) => (
            <span key={c.consoleId} className={c.missing ? "hc-warn" : undefined}>
              {i > 0 ? t("、", ", ") : ""}
              {t("「{label}」", "\"{label}\"", { label: c.label })}
              {c.primary ? t("（主）", " (main)") : ""}
              {c.missing ? t("（已断开）", " (disconnected)") : ""}
            </span>
          ))}
        </div>
      )}
      <div className="hc-goal" title={status.goal}>
        {status.goal}
      </div>
      {detail && status.phase.kind !== "completed" && (
        <div className={`st-status ${status.phase.kind === "failed" ? "bad" : "warn"}`}>{detail}</div>
      )}
      {status.notice && <div className="st-status warn">{status.notice}</div>}
      {err && <div className="st-err">{err}</div>}

      {/* 需要你的时候：卡片紧跟卡头 */}
      {card && draft && (
        <div className="host-criteria" ref={criteriaRef}>
          <div className="host-criteria-head">
            <span className="host-criteria-step">{t("第 1 步", "Step 1")}</span>
            <span className="host-criteria-title">{t("确认完成判据", "Confirm completion checks")}</span>
            <span className="host-criteria-wait">{t("托管还没开始，等你接受", "Autopilot hasn't started; waiting for you to accept")}</span>
          </div>
          <div className="host-criteria-why">
            {t(
              "AI 按目标起草了下面的判据，用来判断「做完了没有」。一行一条，可直接改；接受后托管才开始。",
              "The AI drafted these checks from the goal to decide whether the job is done. One per line, edit freely; autopilot starts once you accept."
            )}
          </div>
          <div className="host-criteria-legend">
            {criteriaSummary(criteria).map((x) => (
              <span key={x.label} className={`host-criteria-kind${x.blocking ? "" : " soft"}`}>
                {x.label} {x.n}
              </span>
            ))}
            <span
              className="host-criteria-hint"
              title={t(
                "命令：在终端当前目录另开 shell 执行，退出码 0 算通过（按授权档位，危险的先问你）\n文件：路径存在算通过\n屏幕：正则命中终端屏幕算通过\n人工：只列进报告，不挡完成",
                "Command: runs in a separate shell in the terminal's current directory; exit code 0 passes (follows the permission level, risky ones ask you first)\nFile: passes if the path exists\nScreen: passes if the regex matches the terminal screen\nManual: only listed in the report, doesn't block completion"
              )}
            >
              {t("前缀：命令 / 文件 / 屏幕 / 人工（人工项不挡完成）ⓘ", "Prefixes: Command / File / Screen / Manual (manual items don't block completion) ⓘ")}
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
              {criteriaBig ? t("收起", "Shrink") : t("放大编辑", "Expand editor")}
            </button>
            <button className="st-btn" disabled={busy || criteria === checksToText(draft)} onClick={() => setCriteria(checksToText(draft))}>
              {t("恢复草稿", "Revert to draft")}
            </button>
            <span className="host-criteria-spacer" />
            <span className="host-criteria-kbd">Ctrl+Enter</span>
            <button
              className="st-btn primary host-criteria-go"
              disabled={busy}
              onClick={() => void act(() => api.hostConfirmCriteria(cid, card.id, textToChecks(criteria)))}
            >
              {t("接受并开始托管", "Accept & start autopilot")}
            </button>
          </div>
        </div>
      )}

      {card && secret && (
        <div className="st-esc host-ask">
          <div className="st-esc-head">{t("等你输入密码", "Waiting for your password")}</div>
          <HostText className="host-question-body" text={card.question} />
          <div className="st-esc-why">
            {t(
              "密码只在下方的密码卡片里输入，直接写进终端，AI 看不到它。不限时。",
              "Enter the password only in the password card below. It goes straight into the terminal and the AI never sees it. No time limit."
            )}
          </div>
        </div>
      )}

      {card && !draft && !secret && (
        <div className="st-esc host-ask">
          <div className="st-esc-head">
            {manualCheck
              ? t("等你确认完成", "Confirm it's done")
              : approve
                ? t("等你批准", "Waiting for your approval")
                : card.kind.kind === "watchdog"
                  ? t("看门狗：它好像卡住了", "Watchdog: it seems stuck")
                  : t("等你回答", "Waiting for your answer")}
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
              <div className="st-esc-why">{t("原因：{why}", "Reason: {why}", { why: card.reasons.join(t("；", "; ")) })}</div>
            ))}
          {card.recommendation && (
            <div className="host-reco">
              <span className="host-reco-tag">{t("AI 建议", "AI suggests")}</span>
              <HostText text={card.recommendation} />
            </div>
          )}
          {needsIncompleteBanner(card.previewComplete, card.diff) && (
            <div className="ap-diff-incomplete" role="alert">
              {t("⚠ 预览不完整：卡上看不到全部修改，批准写入的是完整内容", "⚠ Preview incomplete: the card doesn't show every change, but approving writes the full content")}
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
              placeholder={
                approve
                  ? t("可选：附一句话给 AI", "Optional: add a note for the AI")
                  : card.options.length > 0
                    ? t("或者自己写答复（Enter 发送，Shift+Enter 换行）", "Or write your own reply (Enter to send, Shift+Enter for a new line)")
                    : t("写下你的答复（Enter 发送，Shift+Enter 换行）", "Write your reply (Enter to send, Shift+Enter for a new line)")
              }
            />
            {approve ? (
              <>
                <button className="st-btn primary" disabled={busy} onClick={() => void answer({ text: text.trim(), approved: true })}>
                  {manualCheck ? t("确认完成", "Confirm done") : t("批准", "Approve")}
                </button>
                <button className="st-btn danger" disabled={busy} onClick={() => void answer({ text: text.trim(), approved: false })}>
                  {manualCheck ? t("还没完成", "Not done yet") : t("拒绝", "Reject")}
                </button>
              </>
            ) : (
              <button className="st-btn primary" disabled={busy || !text.trim()} onClick={() => void answer({ text: text.trim() })}>
                {t("回答", "Answer")}
              </button>
            )}
          </div>
        </div>
      )}

      {reattach && (
        <div className="st-esc host-ask">
          <div className="st-esc-head">
            {status.phase.kind === "suspended" && status.phase.reason === "consoleGone"
              ? t("被托管的终端断开了", "The autopilot terminal disconnected")
              : t("应用重启了，托管停在这里", "The app restarted; autopilot stopped here")}
          </div>
          {status.phase.kind === "suspended" && status.phase.reason === "consoleGone" && (
            <div className="st-esc-why">
              {t("如果是网络断了：这个标签页重连成功后，托管会在几秒内", "If the network dropped: once this tab reconnects, autopilot ")}
              <b>{t("自动接着跑", "resumes automatically")}</b>
              {t(
                "（AI 先读屏核对现场），不用做任何事。标签页已经关掉了，才需要在下面手动接回。",
                " within a few seconds (the AI re-reads the screen first); nothing to do. Only if the tab was closed do you need to reattach below."
              )}
            </div>
          )}
          {slot && (
            <div className="st-esc-why">
              {t("这次托管一共 {n} 个终端，现在先接回「", "This autopilot uses {n} terminals. Reattach \"", { n: terms.length })}
              <b>{slot.label}</b>
              {t("」", "\" first")}
              {slot.primary ? t("（主终端）", " (main terminal)") : ""}
              {gone.length > 1
                ? t("，之后还要接 {n} 个；全部接上后才会接着跑。", ", then {n} more; it resumes once all are attached.", { n: gone.length - 1 })
                : t("；接上后就接着跑。", "; it resumes once attached.")}
            </div>
          )}
          <div className="st-esc-why">
            {t(
              "选一个终端接回去（按和原来的相似度排序），或按保存的连接新开一个。接回后 AI 会先读屏核对现场；屏幕和原来不一样时会降为「确认」档，每次写入都先问你。",
              "Pick a terminal to reattach (sorted by similarity to the original), or open a new one from the saved connection. After reattaching, the AI re-reads the screen first; if it doesn't match, it drops to Ask first and asks before every write."
            )}
          </div>
          {cands.length === 0 && <div className="st-esc-why">{t("没有找到相似的终端。", "No similar terminals found.")}</div>}
          <div className="st-form-row st-center">
            {cands.map((c) => (
              <button
                key={c.consoleId}
                className="st-btn"
                disabled={busy}
                onClick={() => void act(() => api.hostReattach(cid, c.consoleId, false, slot?.consoleId))}
                title={t("相似度 {score}", "Similarity {score}", { score: c.score })}
              >
                {t("接到「{label}」", "Attach to \"{label}\"", { label: c.label })}
              </button>
            ))}
            <button className="st-btn primary" disabled={busy} onClick={() => void act(() => api.hostReattach(cid, null, true, slot?.consoleId))}>
              {t("打开新连接", "Open new connection")}
            </button>
            <button className="st-btn danger" disabled={busy} onClick={() => void act(() => api.hostStop(cid, false))}>
              {t("放弃", "Give up")}
            </button>
          </div>
        </div>
      )}

      {status.phase.kind === "completed" && (
        <div className="host-report">
          <div className="host-report-head">
            <span>{t("完成报告", "Completion report")}</span>
            <button
              className="st-btn"
              disabled={!status.phase.summary}
              onClick={() => void api.clipboardWrite(status.phase.kind === "completed" ? status.phase.summary : "").catch(() => {})}
            >
              {t("复制", "Copy")}
            </button>
          </div>
          {status.phase.summary.trim() ? (
            <HostText className="host-report-body" text={formatReport(status.phase.summary)} />
          ) : (
            <div className="host-report-body">{t("（AI 没有写完成报告）", "(The AI didn't write a completion report)")}</div>
          )}
          <HostChangesView contractId={cid} />
          {status.phase.checks.length > 0 && (
            <details className="host-report-checks">
              <summary>{t("核验依据（{n} 条）", "Verification ({n})", { n: status.phase.checks.length })}</summary>
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
                placeholder={t(
                  "基于这份报告接着做什么？例如：按方案执行 A 到 D 项，每项执行前后报告释放的空间",
                  "What next, based on this report? E.g. carry out items A to D of the plan and report the space freed before and after each"
                )}
                onChange={(e) => setNextGoal(e.target.value)}
              />
              <button
                className="st-btn primary"
                disabled={busy || !nextGoal.trim()}
                title={t(
                  "在同一个终端上开始下一个托管；上一个目标与这份完成报告会作为它的开场上下文",
                  "Start the next autopilot on the same terminal, with the previous goal and this completion report as its starting context"
                )}
                onClick={() =>
                  void act(async () => {
                    if (status.phase.kind !== "completed") return;
                    await onFollowUp(nextGoal.trim(), { goal: status.goal, summary: status.phase.summary });
                    setNextGoal("");
                  })
                }
              >
                {t("基于此报告继续", "Continue from this report")}
              </button>
            </div>
          )}
        </div>
      )}
      {notices.map((n) => (
        <div key={n.seq} className="st-esc host-notice">
          <div className="st-esc-head">{t("已执行（通知档）", "Done (Notify)")}</div>
          <div className="host-question">{n.text}</div>
          {live && (
            <div className="st-form-row st-center">
              <input
                className="st-input"
                value={correction}
                disabled={busy}
                onChange={(e) => setCorrection(e.target.value)}
                placeholder={t("可选：哪里不对、下次该怎么做（会记成偏好）", "Optional: what was wrong and what to do next time (saved as a preference)")}
              />
              <button className="st-btn warn" disabled={busy} onClick={() => void undo(n)} title={t("AI 立刻让路，由你处理现场；纠正记成偏好", "The AI steps aside right away so you can handle it; your correction is saved as a preference")}>
                {t("撤销（接管）", "Undo (take over)")}
              </button>
              <button className="st-btn" disabled={busy} onClick={() => setNotices((l) => l.filter((x) => x.seq !== n.seq))}>
                {t("知道了", "Got it")}
              </button>
            </div>
          )}
        </div>
      ))}

      {status.todo && status.todo.length > 0 && (
        <div className="hc-plan">
          <TodoList items={status.todo} />
        </div>
      )}

      {/* 契约 / 设置：档位、禁止项、改目标、停止时发 Ctrl-C、预算 —— 平时收起，只露一行摘要 */}
      {settings && (
        <details className="hc-settings">
          <summary>
            <span className="hc-settings-label">{t("契约 / 设置", "Contract / settings")}</span>
            <span className={summary.yolo ? "hc-yolo" : undefined}>{summary.tier}</span>
            <span className="hc-settings-deny"> · {summary.deny}</span>
          </summary>
          <div className="hc-settings-body">
            {status.tier && (
              <div className="hc-row">
                <span className="deny-title">{t("授权档位", "Permission level")}</span>
                <select
                  className={`st-select${status.tier === "yolo" ? " st-select-yolo" : ""}`}
                  value={status.tier}
                  disabled={busy}
                  title={t("运行中也能改，下一步起生效", "Can be changed while running; applies from the next step")}
                  onChange={(e) => void act(() => api.hostAmend(cid, undefined, e.target.value))}
                >
                  {HOST_TIERS.map((tier) => (
                    <option key={tier} value={tier}>
                      {TIER_LABELS[tier]}
                    </option>
                  ))}
                </select>
                {status.tier === "yolo" && <span className="st-meta hc-yolo">{t("只有越过禁止项才会停下来问你", "Stops to ask only when a blocked action comes up")}</span>}
              </div>
            )}
            <DenyEditor
              value={status.deny ?? []}
              disabled={busy}
              onChange={(next) => void act(() => api.hostAmend(cid, undefined, undefined, next))}
            />
            <div className="st-form-row st-center">
              {goalDraft === null ? (
                <button className="st-btn" disabled={busy} onClick={() => setGoalDraft(status.goal)} title={t("改目标：不重启，AI 下一步就按新目标做", "Change the goal without restarting; the AI follows the new goal from its next step")}>
                  {t("改目标", "Change goal")}
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
                    {t("保存", "Save")}
                  </button>
                  <button className="st-btn" disabled={busy} onClick={() => setGoalDraft(null)}>
                    {t("取消", "Cancel")}
                  </button>
                </>
              )}
            </div>
            {onStopCtrlC && (
              <label className="st-meta hc-row" title={t("停止时同时往被驾驶的终端发送 Ctrl-C，打断它正在跑的命令", "On Stop, also send Ctrl-C to the driven terminal to interrupt the running command")}>
                <input type="checkbox" checked={!!stopCtrlC} onChange={(e) => onStopCtrlC(e.target.checked)} />
                {t("停止时同时发 Ctrl-C", "Send Ctrl-C on Stop")}
              </label>
            )}
            {grant && (
              <div className="st-meta">
                {t("预算：最长 {dur} · 最多 {steps} 步", "Budget: up to {dur} · up to {steps} steps", {
                  dur: durationLabel(grant.maxMinutes),
                  steps: grant.maxSteps ?? "—",
                })}
                {grant.maxCostMicros ? t(" · 最多花 {cost}", " · max spend {cost}", { cost: fmtUsd(grant.maxCostMicros / 1_000_000) }) : ""}
                {t("（「加预算继续」时按这份给）", " (used for Add budget & continue)")}
              </div>
            )}
          </div>
        </details>
      )}
    </div>
  );
}
