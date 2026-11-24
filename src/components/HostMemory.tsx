import { useCallback, useEffect, useState } from "react";
import { createPortal } from "react-dom";
import { api } from "../api";
import { t, useLocale } from "../i18n";
import type { AgentStep, HostMemoryView, HostNote, HostNoteKind } from "../types";
import {
  agentMayDelete,
  checkDraft,
  groupNotes,
  isGoneError,
  KIND_LABEL,
  MEMORY_OP_TAG,
  memoryStepInfo,
  NOTE_KINDS,
  SOURCE_LABEL,
  usageLabel,
} from "../lib/hostMemory";
import "./HostMemory.css";

/**
 * 「主机备忘」对话框：列出当前标签所在主机的备忘，可新增 / 编辑 / 删除 / 清空。
 * 规则（文本规整、凭据拒绝、条数上限）以后端为准，这里只做提交前的空 / 超长检查。
 * 主机键由后端按会话推导，前端只传会话 id + 标签名。
 */
export function HostMemoryDialog({
  sessionId,
  label,
  onClose,
}: {
  sessionId: string;
  label: string;
  onClose: () => void;
}) {
  useLocale();
  const [data, setData] = useState<HostMemoryView | null>(null);
  const [err, setErr] = useState("");
  const [busy, setBusy] = useState(false);
  /** 正在编辑的备忘 id；null = 在新增 */
  const [editId, setEditId] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  const [kind, setKind] = useState<HostNoteKind>("other");
  const [confirmClear, setConfirmClear] = useState(false);

  const reload = useCallback(async () => {
    try {
      setData(await api.hostMemoryList(sessionId, label));
    } catch (e) {
      setErr(String(e));
    }
  }, [sessionId, label]);

  useEffect(() => {
    void reload();
  }, [reload]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const resetForm = () => {
    setEditId(null);
    setDraft("");
    setKind("other");
  };

  const submit = async () => {
    if (!data) return;
    const chk = checkDraft(draft, data.maxChars);
    if (!chk.ok) {
      setErr(chk.error);
      return;
    }
    setBusy(true);
    setErr("");
    try {
      if (editId) await api.hostMemoryUpdate(editId, chk.text, kind);
      else await api.hostMemoryAdd(sessionId, label, chk.text, kind);
      resetForm();
      await reload();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  const remove = async (n: HostNote) => {
    setBusy(true);
    setErr("");
    try {
      await api.hostMemoryDelete(n.id);
      if (editId === n.id) resetForm();
      await reload();
    } catch (e) {
      const msg = String(e);
      // 已经不在了（比如另一处刚删掉）：当作成功，刷新即可
      if (isGoneError(msg)) await reload();
      else setErr(msg);
    } finally {
      setBusy(false);
    }
  };

  const clearAll = async () => {
    setBusy(true);
    setErr("");
    try {
      await api.hostMemoryClear(sessionId, label);
      setConfirmClear(false);
      resetForm();
      await reload();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  const startEdit = (n: HostNote) => {
    setEditId(n.id);
    setDraft(n.text);
    setKind(n.kind);
    setErr("");
  };

  const notes = data?.notes ?? [];
  const host = data?.host ?? null;

  return createPortal(
    <div
      className="modal-backdrop"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="modal hm-modal" role="dialog" aria-label={t("主机备忘", "Host notes")}>
        <h2>
          {t("主机备忘", "Host notes")}
          {host && <span className="hm-host" title={host.key}>{host.display}</span>}
        </h2>

        {data && !host && (
          <p className="hm-warn">{t("认不出这个标签属于哪台主机，无法记忆。", "Can't tell which host this tab belongs to, so nothing can be remembered.")}</p>
        )}
        {host?.origin === "label" && (
          <p className="hm-hint">
            {t(
              "这个标签没有登记连接信息，备忘按标签名「{name}」归档，换了标签名就看不到了。",
              "This tab has no saved connection, so notes are filed under the tab name \"{name}\" and won't show up if the tab is renamed.",
              { name: host.display }
            )}
          </p>
        )}
        <p className="hm-hint">
          {t(
            "助手会在每次运行开始时读到这些备忘，也会自己记一些（提示符样式、sudo 习惯、常用路径、这台机器的怪脾气）。助手只能删它自己记的；你编辑过的归你，它删不掉。不要在这里写密码或密钥。",
            "The assistant reads these notes at the start of every run and also writes some itself (prompt style, sudo habits, common paths, this machine's quirks). It can only delete the ones it wrote; anything you've edited is yours and it can't delete it. Don't put passwords or keys here."
          )}
        </p>

        {data && host && (
          <div className="hm-usage">{usageLabel(notes.length, data.maxNotes)}</div>
        )}

        <div className="hm-list">
          {data && host && notes.length === 0 && <div className="hm-empty">{t("还没有备忘。", "No notes yet.")}</div>}
          {groupNotes(notes).map((g) => (
            <div key={g.kind} className="hm-group">
              <div className="hm-group-title">{KIND_LABEL[g.kind]}</div>
              {g.notes.map((n) => (
                <div key={n.id} className={`hm-note${editId === n.id ? " editing" : ""}`}>
                  <span className={`hm-src ${n.source}`} title={agentMayDelete(n) ? t("助手可以删除", "The assistant can delete this") : t("助手不能删除", "The assistant can't delete this")}>
                    {SOURCE_LABEL[n.source]}
                  </span>
                  <span className="hm-text">{n.text}</span>
                  <button className="ap-mini" disabled={busy} onClick={() => startEdit(n)}>
                    {t("编辑", "Edit")}
                  </button>
                  <button className="ap-mini" disabled={busy} onClick={() => void remove(n)}>
                    {t("删除", "Delete")}
                  </button>
                </div>
              ))}
            </div>
          ))}
        </div>

        {host && (
          <div className="hm-form">
            <textarea
              value={draft}
              maxLength={(data?.maxChars ?? 500) * 2}
              placeholder={
                editId
                  ? t("修改这条备忘", "Edit this note")
                  : t("新增一条，例如：sudo 需要密码，不要用 sudo -n", "Add a note, e.g. sudo needs a password, don't use sudo -n")
              }
              onChange={(e) => setDraft(e.target.value)}
              rows={2}
            />
            <div className="hm-form-row">
              <select value={kind} onChange={(e) => setKind(e.target.value as HostNoteKind)}>
                {NOTE_KINDS.map((k) => (
                  <option key={k} value={k}>
                    {KIND_LABEL[k]}
                  </option>
                ))}
              </select>
              <span className="hm-count">
                {[...draft].length} / {data?.maxChars ?? 500}
              </span>
              {editId && (
                <button className="btn" disabled={busy} onClick={resetForm}>
                  {t("取消编辑", "Cancel edit")}
                </button>
              )}
              <button className="btn primary" disabled={busy || !draft.trim()} onClick={() => void submit()}>
                {editId ? t("保存", "Save") : t("添加", "Add")}
              </button>
            </div>
          </div>
        )}

        {err && <div className="error">{err}</div>}

        <div className="modal-actions">
          {host && notes.length > 0 &&
            (confirmClear ? (
              <>
                <span className="hm-hint">{t("清空这台主机的全部 {n} 条？", "Clear all {n} notes for this host?", { n: notes.length })}</span>
                <button className="btn" disabled={busy} onClick={() => setConfirmClear(false)}>
                  {t("不了", "Cancel")}
                </button>
                <button className="btn primary" disabled={busy} onClick={() => void clearAll()}>
                  {t("确认清空", "Clear all")}
                </button>
              </>
            ) : (
              <button className="btn" disabled={busy} onClick={() => setConfirmClear(true)}>
                {t("清空", "Clear")}
              </button>
            ))}
          <button className="btn" onClick={onClose}>
            {t("关闭", "Close")}
          </button>
        </div>
      </div>
    </div>,
    document.body,
  );
}

/**
 * 面板里 `kind: "memory"` 的步骤卡：「记住：…」/「忘记：…」/「回忆：…」。
 * 新增卡上带「删除」：用户觉得记错了可以当场撤销（走 `host_memory_delete`，不受作者限制）。
 */
export function MemoryStepCard({ step }: { step: AgentStep }) {
  useLocale();
  const info = memoryStepInfo(step);
  const [state, setState] = useState<"idle" | "busy" | "gone">("idle");
  const [err, setErr] = useState("");

  const del = async () => {
    if (!info.deletableId) return;
    setState("busy");
    setErr("");
    try {
      await api.hostMemoryDelete(info.deletableId);
      setState("gone");
    } catch (e) {
      const msg = String(e);
      if (isGoneError(msg)) setState("gone");
      else {
        setErr(msg);
        setState("idle");
      }
    }
  };

  return (
    <div className="ap-step">
      <div className="ap-step-head">
        <span className="ap-step-tag">{MEMORY_OP_TAG[info.op]}</span>
        {step.targetLabel ? (
          <span className="ap-step-where" title={step.target}>
            {step.targetLabel}
          </span>
        ) : null}
        {info.deletableId && (
          <button className="ap-mini" disabled={state !== "idle"} onClick={() => void del()}>
            {state === "gone" ? t("已删除", "Deleted") : t("删除", "Delete")}
          </button>
        )}
      </div>
      <div className="ap-step-why">{info.text}</div>
      {err && <div className="error">{err}</div>}
    </div>
  );
}
