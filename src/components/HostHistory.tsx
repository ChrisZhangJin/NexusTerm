/**
 * 「历史托管」：托管页底部的折叠区，列出最近结束的托管（新的在前：时间 · 终端 · 目标 · 状态）。
 * 点一条展开它的完成报告 / 失败原因、核验依据与实际改动。展开折叠区时才去拉。
 */

import { useCallback, useState } from "react";
import { api } from "../api";
import { t, useLocale } from "../i18n";
import { toFriendlyError } from "../lib/errors";
import { checkLines, formatReport } from "../lib/host";
import { fmtDuration, fmtWhen, historyStateLabel } from "../lib/hostHistory";
import type { HostHistoryItem } from "../types";
import HostChangesView from "./HostChangesView";
import HostText from "./HostText";
import { fromHostSpent, spentLine, usageTooltip } from "../lib/usage";

export default function HostHistory() {
  useLocale();
  const [items, setItems] = useState<HostHistoryItem[] | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [openId, setOpenId] = useState<string | null>(null);

  const load = useCallback(() => {
    setErr(null);
    api
      .hostHistory()
      .then(setItems)
      .catch((e) => setErr(toFriendlyError(e).message));
  }, []);

  return (
    <details
      className="st-diag host-history"
      onToggle={(e) => {
        if ((e.currentTarget as HTMLDetailsElement).open) load();
      }}
    >
      <summary>
        {t("历史托管", "Autopilot history")}
        {items ? <span className="st-meta">{t(" · {n} 条", " · {n}", { n: items.length })}</span> : null}
      </summary>
      <div className="host-history-body">
        <div className="st-form-row">
          <button className="st-btn" onClick={load}>
            {t("刷新", "Refresh")}
          </button>
        </div>
        {err && <div className="st-status warn">{err}</div>}
        {items && items.length === 0 && <div className="st-empty">{t("还没有结束的托管", "No finished autopilot runs yet")}</div>}
        {items?.map((it) => {
          const st = historyStateLabel(it.state);
          const open = openId === it.contractId;
          const usage = spentLine(it.spent);
          return (
            <div key={it.contractId} className={`host-history-item${open ? " on" : ""}`}>
              <button className="host-history-row" onClick={() => setOpenId(open ? null : it.contractId)} title={it.goal}>
                <span className="st-ev-time">{fmtWhen(it.endedAt)}</span>
                <span className="host-history-label">{it.label}</span>
                <span className="host-history-goal">{it.goal}</span>
                {usage && (
                  <span className="hc-usage" title={usageTooltip(fromHostSpent(it.spent))}>
                    {usage}
                  </span>
                )}
                <span className={`st-badge host-pill ${st.tone}`}>{st.label}</span>
              </button>
              {open && (
                <div className="host-report">
                  <div className="host-report-head">
                    <span>{it.state === "completed" ? t("完成报告", "Completion report") : it.state === "failed" ? t("失败", "Failed") : t("已停止", "Stopped")}</span>
                    <span className="st-meta">
                      {t("{start} 开始 · 用时 {dur} · {steps} 步", "Started {start} · took {dur} · {steps} steps", {
                        start: fmtWhen(it.startedAt),
                        dur: fmtDuration(it.durationMs),
                        steps: it.spent.steps,
                      })}
                      {usage ? t(" · 共 {usage}", " · total {usage}", { usage }) : ""}
                    </span>
                  </div>
                  {it.state === "completed" ? (
                    it.summary?.trim() ? (
                      <HostText className="host-report-body" text={formatReport(it.summary)} />
                    ) : (
                      <div className="host-report-body">{t("（AI 没有写完成报告）", "(The AI didn't write a completion report)")}</div>
                    )
                  ) : (
                    <div className="host-report-body">{it.error?.trim() || (it.state === "stopped" ? t("托管被停止，没有完成报告", "Autopilot was stopped; no completion report") : t("（没有记录原因）", "(No reason recorded)"))}</div>
                  )}
                  <HostChangesView contractId={it.contractId} data={{ changes: it.changes, backups: it.backups }} />
                  {it.checks.length > 0 && (
                    <details className="host-report-checks">
                      <summary>{t("核验依据（{n} 条）", "Verification ({n})", { n: it.checks.length })}</summary>
                      <ul className="host-checks">
                        {checkLines(it.checks).map((l, i) => (
                          <li key={i} className={l.startsWith("✓") ? "ok" : l.startsWith("·") ? "info" : "no"}>
                            {l}
                          </li>
                        ))}
                      </ul>
                    </details>
                  )}
                </div>
              )}
            </div>
          );
        })}
      </div>
    </details>
  );
}
