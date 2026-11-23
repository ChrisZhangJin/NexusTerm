import { useEffect, useState } from "react";
import { api } from "../api";
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
        ? { title: "选择主机 CSV", filters: [{ name: "CSV", extensions: ["csv"] }] }
        : { title: "选择 OpenSSH 配置（例如 ~/.ssh/config）" });
      if (typeof path !== "string") return;
      setFinished(false);
      setPreview(null);
      setPreview(await api.previewConnectionImport(path, format));
    } catch (error) {
      setMessage(`无法预览：${String(error)}`);
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
      setMessage(`已导入 ${saved} 个连接。${failed.length ? `未保存 ${failed.length} 个：${failed.join("、")}` : ""}`);
    } catch {
      setMessage(`已保存 ${saved} 个连接，但列表刷新失败。请重新打开连接列表。${failed.length ? `另有 ${failed.length} 个未保存。` : ""}`);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="modal-backdrop" onMouseDown={() => { if (!busy) onClose(); }}>
      <section className="modal connection-import" role="dialog" aria-modal="true" aria-labelledby="connection-import-title" onMouseDown={(e) => e.stopPropagation()}>
        <h2 id="connection-import-title">从 Termius / OpenSSH 导入</h2>
        <p className="confirm-msg">选择已有的 OpenSSH 配置，或从 Termius 迁移时得到的主机 CSV。文件只在本机解析，不读取 Termius 数据库，也不导入密码或私钥内容。</p>
        <div className="connection-import-files">
          <button className="btn" autoFocus disabled={busy} onClick={() => void choose("ssh")}>选择 SSH config</button>
          <button className="btn" disabled={busy} onClick={() => void choose("csv")}>选择主机 CSV</button>
        </div>
        <details>
          <summary>如何准备迁移文件？</summary>
          <p>如果已有 OpenSSH 配置，直接选取该文件。若已获得主机 CSV，需包含 <code>Address</code>（或 <code>Hostname</code>）和 <code>Username</code>；可选 <code>Label</code>、<code>Port</code> 和 <code>Group</code>。</p>
          <p>如果只有 Termius 中的连接信息，可以手工填写上述 CSV 列。导入后可在连接设置中补填密码或选择本机密钥。依赖跳板或代理的条目不会作为直连导入。</p>
        </details>
        {preview && !finished && <>
          <p>{pending.length} 个可导入 · {rows.length - pending.length} 个重复 · {preview.warnings.length} 条提示</p>
          <div className="connection-import-results">
            <ul>
              {rows.map(({ session, duplicate }, i) => (
                <li key={i}><strong>{session.name}</strong> — {session.username}@{session.host} · {session.port ?? 22}
                  {session.folder && ` · ${session.folder}`} · {session.authType === "key" ? "私钥路径" : session.authType === "agent" ? "SSH agent" : "待填写密码"}{duplicate && " · 重复，跳过"}
                </li>
              ))}
              {preview.warnings.map((warning, i) => <li key={`warning-${i}`}>{warning}</li>)}
            </ul>
          </div>
        </>}
        {message && <p role="status">{message}</p>}
        <div className="modal-actions">
          <button className="btn" disabled={busy} onClick={onClose}>{finished ? "关闭" : "取消"}</button>
          {!finished && <button className="btn primary" disabled={busy || !pending.length} onClick={() => void save()}>{busy ? "处理中…" : `导入 ${pending.length} 个连接`}</button>}
        </div>
      </section>
    </div>
  );
}
