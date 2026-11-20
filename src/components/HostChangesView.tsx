/**
 * 「实际改动」：一次托管真正做了什么（按执行过的命令 / 文件写入提取，不是模型的说法）。
 * 托管覆盖 / 修改过的已有文件留了原文备份的，行尾有「还原」：确认后经原来那个终端的文件通道写回。
 *
 * 完成报告（HostCard）里不给 `data`，自己按契约 id 拉；历史托管里直接给。
 */

import { useCallback, useEffect, useState } from "react";
import { api } from "../api";
import { toFriendlyError } from "../lib/errors";
import { changeGroups, changesEmpty, fileRows, fmtSize, fmtWhen } from "../lib/hostHistory";
import type { HostBackupEntry, HostChanges } from "../types";
import ConfirmDialog, { type ConfirmOptions } from "./ConfirmDialog";

export default function HostChangesView({ contractId, data }: { contractId: string; data?: HostChanges }) {
  const [fetched, setFetched] = useState<HostChanges | null>(null);
  const [restored, setRestored] = useState<Record<string, number>>({});
  const [msg, setMsg] = useState<{ ok: boolean; text: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const [confirm, setConfirm] = useState<{ opts: ConfirmOptions; backup: HostBackupEntry } | null>(null);

  useEffect(() => {
    if (data) return;
    let gone = false;
    api
      .hostChanges(contractId)
      .then((c) => !gone && setFetched(c))
      .catch(() => !gone && setFetched(null));
    return () => {
      gone = true;
    };
  }, [contractId, data]);

  const doRestore = useCallback(
    async (b: HostBackupEntry) => {
      setBusy(true);
      setMsg(null);
      try {
        const text = await api.hostRestoreBackup(contractId, b.id);
        setRestored((r) => ({ ...r, [b.id]: Date.now() }));
        setMsg({ ok: true, text });
      } catch (e) {
        setMsg({ ok: false, text: toFriendlyError(e).message });
      } finally {
        setBusy(false);
      }
    },
    [contractId]
  );

  const c = data ?? fetched;
  if (!c) return null;
  const { changes, backups } = c;
  if (changesEmpty(changes) && backups.length === 0) {
    return (
      <div className="host-changes">
        <div className="host-changes-head">实际改动</div>
        <div className="host-changes-none">没有执行过写入动作</div>
      </div>
    );
  }
  const files = fileRows(changes.filesWritten, backups);
  return (
    <div className="host-changes">
      <div className="host-changes-head">
        实际改动
        <span className="st-meta"> · 执行命令 {changes.commands} 条 · 按实际执行的命令与文件写入整理</span>
      </div>
      {files.length > 0 && (
        <div className="host-changes-group">
          <span className="host-changes-label">文件</span>
          <ul>
            {files.map((f) => {
              const b = f.backup;
              const at = b ? (restored[b.id] ?? b.restoredAt ?? null) : null;
              return (
                <li key={`${f.path}-${b?.id ?? ""}`}>
                  <code>{f.path}</code>
                  {b && (
                    <>
                      <span className="st-meta"> · 原文已备份 {fmtSize(b.size)}</span>
                      {at ? (
                        <span className="st-meta"> · 已于 {fmtWhen(at)} 还原</span>
                      ) : null}
                      <button
                        className="st-btn host-changes-restore"
                        disabled={busy}
                        title="把这个文件写回托管改动之前的内容"
                        onClick={() =>
                          setConfirm({
                            backup: b,
                            opts: {
                              title: "还原文件",
                              message: `把「${b.path}」还原成托管改动之前的内容？`,
                              detail: "会覆盖这个文件现在的内容（包括托管之后你自己做的修改）。经原来那个终端写回；终端已关闭时无法还原。",
                              okText: "还原",
                              danger: true,
                            },
                          })
                        }
                      >
                        还原
                      </button>
                    </>
                  )}
                </li>
              );
            })}
          </ul>
        </div>
      )}
      {changeGroups(changes).map((g) => (
        <div key={g.label} className="host-changes-group">
          <span className="host-changes-label">{g.label}</span>
          <ul>
            {g.items.map((it) => (
              <li key={it}>{it}</li>
            ))}
          </ul>
        </div>
      ))}
      {msg && <div className={`st-status${msg.ok ? "" : " warn"}`}>{msg.text}</div>}
      {confirm && (
        <ConfirmDialog
          opts={confirm.opts}
          onResult={(ok) => {
            const b = confirm.backup;
            setConfirm(null);
            if (ok) void doRestore(b);
          }}
        />
      )}
    </div>
  );
}
