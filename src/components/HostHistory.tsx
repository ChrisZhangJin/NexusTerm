/**
 * 「历史托管」：托管页底部的折叠区，列出最近结束的托管（新的在前：时间 · 终端 · 目标 · 状态）。
 * 点一条展开它的完成报告 / 失败原因、核验依据与实际改动。展开折叠区时才去拉。
 */

import { useCallback, useState } from "react";
import { api } from "../api";
import { toFriendlyError } from "../lib/errors";
import { checkLines, formatReport } from "../lib/host";
import { fmtDuration, fmtWhen, historyStateLabel } from "../lib/hostHistory";
import type { HostHistoryItem } from "../types";
import HostChangesView from "./HostChangesView";
import HostText from "./HostText";

export default function HostHistory() {
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
        历史托管
        {items ? <span className="st-meta"> · {items.length} 条</span> : null}
      </summary>
      <div className="host-history-body">
        <div className="st-form-row">
          <button className="st-btn" onClick={load}>
            刷新
          </button>
        </div>
        {err && <div className="st-status warn">{err}</div>}
        {items && items.length === 0 && <div className="st-empty">还没有结束的托管</div>}
        {items?.map((it) => {
          const st = historyStateLabel(it.state);
          const open = openId === it.contractId;
          return (
            <div key={it.contractId} className={`host-history-item${open ? " on" : ""}`}>
              <button className="host-history-row" onClick={() => setOpenId(open ? null : it.contractId)} title={it.goal}>
                <span className="st-ev-time">{fmtWhen(it.endedAt)}</span>
                <span className="host-history-label">{it.label}</span>
                <span className="host-history-goal">{it.goal}</span>
                <span className={`st-badge host-pill ${st.tone}`}>{st.label}</span>
              </button>
              {open && (
                <div className="host-report">
                  <div className="host-report-head">
                    <span>{it.state === "completed" ? "完成报告" : it.state === "failed" ? "失败" : "已停止"}</span>
                    <span className="st-meta">
                      {fmtWhen(it.startedAt)} 开始 · 用时 {fmtDuration(it.durationMs)} · {it.spent.steps} 步
                    </span>
                  </div>
                  {it.state === "completed" ? (
                    it.summary?.trim() ? (
                      <HostText className="host-report-body" text={formatReport(it.summary)} />
                    ) : (
                      <div className="host-report-body">（AI 没有写完成报告）</div>
                    )
                  ) : (
                    <div className="host-report-body">{it.error?.trim() || (it.state === "stopped" ? "托管被停止，没有完成报告" : "（没有记录原因）")}</div>
                  )}
                  <HostChangesView contractId={it.contractId} data={{ changes: it.changes, backups: it.backups }} />
                  {it.checks.length > 0 && (
                    <details className="host-report-checks">
                      <summary>核验依据（{it.checks.length} 条）</summary>
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
