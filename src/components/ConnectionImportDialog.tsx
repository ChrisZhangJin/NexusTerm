import { useEffect, useState } from "react";
import { api } from "../api";
import { t, useLocale } from "../i18n";
import { normFolder } from "../lib/folders";
import type { NewSession, SessionRecord } from "../types";

type Preview = Awaited<ReturnType<typeof api.previewConnectionImport>>;
const identity = (s: Pick<NewSession, "folder" | "name" | "host" | "port" | "username">) => JSON.stringify([
  normFolder(s.folder ?? ""), s.name, s.host.toLowerCase(), s.port ?? 22, s.username,
]);

export default function ConnectionImportDialog({ sessions, onClose, onImported }: {
  sessions: SessionRecord[];
  onClose: () => void;
  onImported: () => Promise<unknown>;
}) {
  const locale = useLocale();
  const [preview, setPreview] = useState<Preview | null>(null);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState("");
  const [finished, setFinished] = useState(false);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.preventDefault();
      e.stopPropagation();
      if (!busy) onClose();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [busy, onClose]);

  const seen = new Set(sessions.map(identity));
  const rows = (preview?.sessions ?? []).map((session) => {
    const id = identity(session);
    const duplicate = seen.has(id);
    seen.add(id);
    return { session, duplicate };
  });
  const pending = rows.filter((r) => !r.duplicate);

  const choose = async (format: "ssh" | "csv") => {
    setBusy(true);
    setMessage("");
    try {
      const { open } = await import("@tauri-apps/plugin-dialog");
      const path = await open(format === "csv"
        ? { title: t("选择主机 CSV", "Choose host CSV"), filters: [{ name: "CSV", extensions: ["csv"] }] }
        : { title: t("选择 OpenSSH 配置（例如 ~/.ssh/config）", "Choose an OpenSSH config (e.g. ~/.ssh/config)") });
      if (typeof path !== "string") return;
      setFinished(false);
      setPreview(null);
      setPreview(await api.previewConnectionImport(path, format));
    } catch (error) {
      setMessage(t("无法预览：{e}", "Can't preview: {e}", { e: String(error) }));
    } finally {
      setBusy(false);
    }
  };

  const save = async () => {
    setBusy(true);
    let saved = 0;
    const failed: string[] = [];
    for (const { session } of pending) {
      try {
        await api.createSession(session);
        saved++;
      } catch {
        failed.push(session.name);
      }
    }
    setFinished(true);
    try {
      await onImported();
      setMessage(
        t("已导入 {n} 个连接。", "Imported {n} connection(s).", { n: saved }) +
          (failed.length
            ? t("未保存 {n} 个：{list}", " {n} not saved: {list}", { n: failed.length, list: failed.join(t("、", ", ")) })
            : "")
      );
    } catch {
      setMessage(
        t(
          "已保存 {n} 个连接，但列表刷新失败。请重新打开连接列表。",
          "Saved {n} connection(s), but the list failed to refresh. Reopen the connection list.",
          { n: saved }
        ) + (failed.length ? t("另有 {n} 个未保存。", " {n} more not saved.", { n: failed.length }) : "")
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="modal-backdrop" onMouseDown={() => { if (!busy) onClose(); }}>
      <section className="modal connection-import" role="dialog" aria-modal="true" aria-labelledby="connection-import-title" onMouseDown={(e) => e.stopPropagation()}>
        <h2 id="connection-import-title">{t("从 Termius / OpenSSH 导入", "Import from Termius / OpenSSH")}</h2>
        <p className="confirm-msg">
          {t(
            "选择已有的 OpenSSH 配置，或从 Termius 迁移时得到的主机 CSV。文件只在本机解析，不读取 Termius 数据库，也不导入密码或私钥内容。",
            "Pick an existing OpenSSH config, or a host CSV from your Termius migration. The file is parsed locally; the Termius database isn't read, and no passwords or private key contents are imported."
          )}
        </p>
        <div className="connection-import-files">
          <button className="btn" autoFocus disabled={busy} onClick={() => void choose("ssh")}>{t("选择 SSH config", "Choose SSH config")}</button>
          <button className="btn" disabled={busy} onClick={() => void choose("csv")}>{t("选择主机 CSV", "Choose host CSV")}</button>
        </div>
        <details>
          <summary>{t("如何准备迁移文件？", "How do I prepare the file?")}</summary>
          {locale === "en" ? (
            <p>If you have an OpenSSH config, just pick it. A host CSV needs <code>Address</code> (or <code>Hostname</code>) and <code>Username</code>; <code>Label</code>, <code>Port</code> and <code>Group</code> are optional.</p>
          ) : (
            <p>如果已有 OpenSSH 配置，直接选取该文件。若已获得主机 CSV，需包含 <code>Address</code>（或 <code>Hostname</code>）和 <code>Username</code>；可选 <code>Label</code>、<code>Port</code> 和 <code>Group</code>。</p>
          )}
          <p>
            {t(
              "如果只有 Termius 中的连接信息，可以手工填写上述 CSV 列。导入后可在连接设置中补填密码或选择本机密钥。依赖跳板或代理的条目不会作为直连导入。",
              "If your connections only live in Termius, fill in the CSV columns above by hand. After importing, add passwords or pick local keys in each connection's settings. Entries that go through a jump host or proxy aren't imported as direct connections."
            )}
          </p>
        </details>
        {preview && !finished && <>
          <p>
            {t("{a} 个可导入 · {b} 个重复 · {c} 条提示", "{a} to import · {b} duplicate(s) · {c} note(s)", {
              a: pending.length,
              b: rows.length - pending.length,
              c: preview.warnings.length,
            })}
          </p>
          <div className="connection-import-results">
            <ul>
              {rows.map(({ session, duplicate }, i) => (
                <li key={i}><strong>{session.name}</strong> — {session.username}@{session.host} · {session.port ?? 22}
                  {session.folder && ` · ${session.folder}`} · {session.authType === "key"
                    ? t("私钥路径", "Private key path")
                    : session.authType === "agent"
                      ? "SSH agent"
                      : t("待填写密码", "Password needed")}
                  {duplicate && t(" · 重复，跳过", " · duplicate, skipped")}
                </li>
              ))}
              {preview.warnings.map((warning, i) => <li key={`warning-${i}`}>{warning}</li>)}
            </ul>
          </div>
        </>}
        {message && <p role="status">{message}</p>}
        <div className="modal-actions">
          <button className="btn" disabled={busy} onClick={onClose}>{finished ? t("关闭", "Close") : t("取消", "Cancel")}</button>
          {!finished && <button className="btn primary" disabled={busy || !pending.length} onClick={() => void save()}>{busy ? t("处理中…", "Working…") : t("导入 {n} 个连接", "Import {n} connection(s)", { n: pending.length })}</button>}
        </div>
      </section>
    </div>
  );
}
