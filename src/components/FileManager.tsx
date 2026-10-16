import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { homeDir } from "@tauri-apps/api/path";
import { api } from "../api";
import { resolveAuth } from "../lib/auth";
import { normalizeTimeout, toFriendlyError } from "../lib/errors";
import type { RemoteEntry, SessionRecord } from "../types";

function formatSize(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} K`;
  if (n < 1024 ** 3) return `${(n / 1024 ** 2).toFixed(1)} M`;
  return `${(n / 1024 ** 3).toFixed(1)} G`;
}

function formatTime(sec: number | null): string {
  if (sec === null) return "—";
  const d = new Date(sec * 1000);
  const p = (x: number) => String(x).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

/** POSIX 路径拼接。SFTP 一律用 `/`，不能用标准库的 join。 */
function joinPath(dir: string, name: string): string {
  return dir.endsWith("/") ? `${dir}${name}` : `${dir}/${name}`;
}

function parentPath(p: string): string {
  const trimmed = p.replace(/\/+$/, "");
  if (trimmed === "") return "/";
  const idx = trimmed.lastIndexOf("/");
  return idx <= 0 ? "/" : trimmed.slice(0, idx);
}

interface PaneProps {
  title: string;
  path: string;
  entries: RemoteEntry[];
  loading: boolean;
  error: string | null;
  selected: string | null;
  onSelect: (entry: RemoteEntry) => void;
  onActivate: (entry: RemoteEntry) => void;
  onUp: () => void;
  children?: ReactNode;
}

function Pane({
  title,
  path,
  entries,
  loading,
  error,
  selected,
  onSelect,
  onActivate,
  onUp,
  children,
}: PaneProps) {
  return (
    <div className="fm-pane">
      <div className="fm-pane-head">
        <strong>{title}</strong>
        <code className="fm-path">{path}</code>
        <button className="btn-row" onClick={onUp} title="上级目录">
          ↑
        </button>
      </div>

      <div className="fm-list">
        {loading && <div className="empty">加载中…</div>}
        {error && <div className="error">{error}</div>}
        {!loading && !error && entries.length === 0 && (
          <div className="empty">空目录</div>
        )}
        {entries.map((e) => (
          <div
            key={e.path}
            className={`fm-row ${selected === e.path ? "selected" : ""}`}
            onClick={() => onSelect(e)}
            onDoubleClick={() => onActivate(e)}
          >
            <span className="fm-icon">{e.isDir ? "📁" : "📄"}</span>
            <span className="fm-name">{e.name}</span>
            <span className="fm-size">
              {e.isDir ? "" : formatSize(e.size)}
            </span>
            <span className="fm-mtime">{formatTime(e.mtime)}</span>
          </div>
        ))}
      </div>

      {children && <div className="fm-actions">{children}</div>}
    </div>
  );
}

interface Props {
  session: SessionRecord;
  passphrase?: string;
  onClose: () => void;
}

export default function FileManager({ session, passphrase, onClose }: Props) {
  // SFTP 用独立连接，连接 id 与终端 tab 分开，避免长传输阻塞交互 shell
  const sftpId = `sftp-${session.id}`;
  const readyRef = useRef(false);

  const [status, setStatus] = useState<"connecting" | "ready" | "error">(
    "connecting"
  );
  const [statusMsg, setStatusMsg] = useState("");

  const [localPath, setLocalPath] = useState("");
  const [localEntries, setLocalEntries] = useState<RemoteEntry[]>([]);
  const [localBusy, setLocalBusy] = useState(false);
  const [localError, setLocalError] = useState<string | null>(null);
  const [localSel, setLocalSel] = useState<string | null>(null);

  const [remotePath, setRemotePath] = useState(".");
  const [remoteEntries, setRemoteEntries] = useState<RemoteEntry[]>([]);
  const [remoteBusy, setRemoteBusy] = useState(false);
  const [remoteError, setRemoteError] = useState<string | null>(null);
  const [remoteSel, setRemoteSel] = useState<string | null>(null);

  const [transferMsg, setTransferMsg] = useState<string | null>(null);

  const loadLocal = useCallback(async (p: string) => {
    setLocalBusy(true);
    setLocalError(null);
    try {
      setLocalEntries(await api.listLocalDir(p));
    } catch (e) {
      setLocalError(String(e));
      setLocalEntries([]);
    } finally {
      setLocalBusy(false);
    }
  }, []);

  const loadRemote = useCallback(async (p: string) => {
    setRemoteBusy(true);
    setRemoteError(null);
    try {
      setRemoteEntries(await api.sftpList(sftpId, p));
    } catch (e) {
      setRemoteError(String(e));
      setRemoteEntries([]);
    } finally {
      setRemoteBusy(false);
    }
  }, [sftpId]);

  // 建立 SFTP 连接；成功后解析家目录作为起始路径
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const auth = await resolveAuth(session, passphrase);
        await api.sftpConnect(sftpId, {
          host: session.host,
          port: session.port,
          username: session.username,
          auth,
          cols: 80,
          rows: 24,
          connectTimeoutSecs: normalizeTimeout(session.connectTimeout),
        });
        if (cancelled) return;
        const home = await api.sftpHome(sftpId);
        if (cancelled) return;
        readyRef.current = true;
        setRemotePath(home);
        setStatus("ready");
        await loadRemote(home);
      } catch (e) {
        if (cancelled) return;
        setStatus("error");
        setStatusMsg(toFriendlyError(e).message);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [sftpId, session, loadRemote]);

  // 本地面板从家目录起步
  useEffect(() => {
    homeDir()
      .then(setLocalPath)
      .catch(() => setLocalPath("/"));
  }, []);

  useEffect(() => {
    if (!localPath) return;
    loadLocal(localPath);
  }, [localPath, loadLocal]);

  // 卸载时释放远端连接
  useEffect(
    () => () => {
      if (readyRef.current) api.sftpClose(sftpId).catch(() => {});
    },
    [sftpId]
  );

  const enterLocal = (e: RemoteEntry) => {
    if (e.isDir) {
      setLocalSel(null);
      setLocalPath(e.path);
    }
  };

  const enterRemote = (e: RemoteEntry) => {
    if (e.isDir) {
      setRemoteSel(null);
      setRemotePath(e.path);
      loadRemote(e.path);
    }
  };

  const doUpload = async () => {
    const src = localEntries.find((e) => e.path === localSel);
    if (!src || src.isDir) {
      setTransferMsg("请先在左侧选择一个本地文件");
      return;
    }
    setTransferMsg("上传中…");
    try {
      const n = await api.sftpUpload(sftpId, src.path, joinPath(remotePath, src.name));
      setTransferMsg(`上传完成 · ${formatSize(n)}`);
      await loadRemote(remotePath);
    } catch (e) {
      setTransferMsg(`上传失败: ${e}`);
    }
  };

  const doDownload = async () => {
    const src = remoteEntries.find((e) => e.path === remoteSel);
    if (!src || src.isDir) {
      setTransferMsg("请先在右侧选择一个远端文件");
      return;
    }
    setTransferMsg("下载中…");
    try {
      const n = await api.sftpDownload(sftpId, src.path, joinPath(localPath, src.name));
      setTransferMsg(`下载完成 · ${formatSize(n)}`);
      await loadLocal(localPath);
    } catch (e) {
      setTransferMsg(`下载失败: ${e}`);
    }
  };

  const doMkdir = async () => {
    const name = prompt("新目录名称");
    if (!name) return;
    try {
      await api.sftpMkdir(sftpId, joinPath(remotePath, name));
      await loadRemote(remotePath);
    } catch (e) {
      setTransferMsg(`创建失败: ${e}`);
    }
  };

  const doDelete = async () => {
    const src = remoteEntries.find((e) => e.path === remoteSel);
    if (!src) {
      setTransferMsg("请先在右侧选择要删除的条目");
      return;
    }
    if (!confirm(`删除远端「${src.name}」？`)) return;
    try {
      await api.sftpRemove(sftpId, src.path, src.isDir);
      setRemoteSel(null);
      await loadRemote(remotePath);
    } catch (e) {
      setTransferMsg(`删除失败: ${e}`);
    }
  };

  const doRename = async () => {
    const src = remoteEntries.find((e) => e.path === remoteSel);
    if (!src) {
      setTransferMsg("请先在右侧选择要重命名的条目");
      return;
    }
    const name = prompt("新名称", src.name);
    if (!name) return;
    try {
      await api.sftpRename(sftpId, src.path, joinPath(remotePath, name));
      await loadRemote(remotePath);
    } catch (e) {
      setTransferMsg(`重命名失败: ${e}`);
    }
  };

  return (
    <div className="fm">
      <div className="fm-head">
        <strong>SFTP · {session.name}</strong>
        <span className={`fm-status fm-status-${status}`}>
          {status === "ready" ? "已连接" : status === "connecting" ? "连接中…" : statusMsg || "失败"}
        </span>
        <button className="btn" onClick={onClose}>
          关闭
        </button>
      </div>

      <div className="fm-body">
        <Pane
          title="本地"
          path={localPath}
          entries={localEntries}
          loading={localBusy}
          error={localError}
          selected={localSel}
          onSelect={(e) => setLocalSel(e.path)}
          onActivate={enterLocal}
          onUp={() => setLocalPath(parentPath(localPath))}
        >
          <button className="btn" onClick={doUpload} disabled={status !== "ready"}>
            上传 →
          </button>
        </Pane>

        <Pane
          title="远端"
          path={remotePath}
          entries={remoteEntries}
          loading={remoteBusy}
          error={remoteError}
          selected={remoteSel}
          onSelect={(e) => setRemoteSel(e.path)}
          onActivate={enterRemote}
          onUp={() => {
            const p = parentPath(remotePath);
            setRemotePath(p);
            loadRemote(p);
          }}
        >
          <button className="btn" onClick={doDownload} disabled={status !== "ready"}>
            ← 下载
          </button>
          <button className="btn" onClick={doMkdir} disabled={status !== "ready"}>
            新建目录
          </button>
          <button className="btn" onClick={doRename} disabled={status !== "ready"}>
            重命名
          </button>
          <button
            className="btn danger"
            onClick={doDelete}
            disabled={status !== "ready"}
          >
            删除
          </button>
        </Pane>
      </div>

      {transferMsg && <div className="fm-transfer">{transferMsg}</div>}
    </div>
  );
}
