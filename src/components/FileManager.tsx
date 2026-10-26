import { useCallback, useEffect, useRef, useState, type MouseEvent as RMouseEvent, type ReactNode } from "react";
import { homeDir } from "@tauri-apps/api/path";
import { api } from "../api";
import { resolveAuth } from "../lib/auth";
import { normalizeTimeout, toFriendlyError } from "../lib/errors";
import { isWindows } from "../lib/platform";
import { getPrefs } from "../lib/prefs";
import type { AuthMethod, RemoteEntry, SessionRecord } from "../types";
import { kindIcon, viewableKind } from "../lib/viewable";

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

/** 远端（SFTP）路径拼接：SFTP 协议一律用 `/`，与本机平台无关。 */
function joinPath(dir: string, name: string): string {
  return dir.endsWith("/") ? `${dir}${name}` : `${dir}/${name}`;
}

/** 远端上级目录（POSIX）。 */
function parentPath(p: string): string {
  const trimmed = p.replace(/\/+$/, "");
  if (trimmed === "") return "/";
  const idx = trimmed.lastIndexOf("/");
  return idx <= 0 ? "/" : trimmed.slice(0, idx);
}

/** 规范化远端路径：折叠 //、处理 . 与 ..，去掉末尾 / */
function normalizeRemote(p: string): string {
  const out: string[] = [];
  for (const seg of p.split("/")) {
    if (!seg || seg === ".") continue;
    if (seg === "..") out.pop();
    else out.push(seg);
  }
  return "/" + out.join("/");
}

/**
 * 本地路径拼接。Windows 是 `C:\Users\…`，分隔符可能是 `\`；
 * 沿用上面的 `/` 版本会拼出 `C:\Users\dell/file` 这种混写路径。
 */
function joinLocal(dir: string, name: string): string {
  const sep = isWindows() ? "\\" : "/";
  return /[\\/]$/.test(dir) ? `${dir}${name}` : `${dir}${sep}${name}`;
}

/**
 * 本地上级目录。Windows 要额外认盘符根：
 * `C:\Users\dell` → `C:\Users`，`C:\` → `C:\`（不再往上），
 * 否则按 `/` 切会得到 -1，一路跳到 `/`，整个面板就废了。
 * 已到盘符根时返回空串 —— 后端会据此列出所有可用盘符。
 */
function parentLocal(p: string): string {
  const sep = isWindows() ? "\\" : "/";
  const t = p.replace(/[\\/]+$/, "");
  if (t === "") return "";
  if (/^[A-Za-z]:$/.test(t)) return "";
  const idx = Math.max(t.lastIndexOf("\\"), t.lastIndexOf("/"));
  if (idx < 0) return sep;
  const cut = t.slice(0, idx);
  if (cut === "") return sep;
  return /^[A-Za-z]:$/.test(cut) ? `${cut}${sep}` : cut;
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
  /** 路径栏回车跳转；返回 false 表示路径无效（输入框抖动并保留输入） */
  onNavigate: (p: string) => Promise<boolean>;
  /** 条目右键 */
  onEntryContext?: (e: RMouseEvent, entry: RemoteEntry) => void;
  children?: ReactNode;
}

/** 可编辑路径栏：回车跳转，Esc 还原；外部路径变化（双击进目录 / 上级）时同步显示 */
function PathInput({
  path,
  onNavigate,
}: {
  path: string;
  onNavigate: (p: string) => Promise<boolean>;
}) {
  const [value, setValue] = useState(path);
  const [invalid, setInvalid] = useState(false);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    setValue(path);
    setInvalid(false);
  }, [path]);
  const submit = async () => {
    const v = value.trim();
    if (!v || v === path || busy) return;
    setBusy(true);
    const ok = await onNavigate(v);
    setBusy(false);
    if (!ok) {
      // 先清再置，保证连续输错也能重放抖动动画
      setInvalid(false);
      requestAnimationFrame(() => setInvalid(true));
    }
  };
  return (
    <input
      className={`fm-path-input${invalid ? " invalid" : ""}`}
      value={value}
      spellCheck={false}
      title="输入路径后回车跳转，Esc 还原"
      disabled={busy}
      onChange={(e) => {
        setValue(e.target.value);
        setInvalid(false);
      }}
      onFocus={(e) => e.currentTarget.select()}
      onKeyDown={(e) => {
        if (e.key === "Enter") void submit();
        else if (e.key === "Escape") {
          setValue(path);
          setInvalid(false);
          e.currentTarget.blur();
        }
      }}
    />
  );
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
  onNavigate,
  onEntryContext,
  children,
}: PaneProps) {
  return (
    <div className="fm-pane">
      <div className="fm-pane-head">
        <strong>{title}</strong>
        <PathInput path={path} onNavigate={onNavigate} />
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
            onContextMenu={(ev) => {
              if (!onEntryContext) return;
              ev.preventDefault();
              onSelect(e);
              onEntryContext(ev, e);
            }}
          >
            <span className="fm-icon">{e.isDir ? "📁" : kindIcon(viewableKind(e.name))}</span>
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
  const homeRef = useRef("/");

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
  /** 远端条目右键菜单 */
  const [ctx, setCtx] = useState<{ x: number; y: number; entry: RemoteEntry } | null>(null);
  const ctxRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!ctx) return;
    const hide = (e: MouseEvent) => {
      if (ctxRef.current?.contains(e.target as Node)) return;
      setCtx(null);
    };
    const esc = (e: KeyboardEvent) => e.key === "Escape" && setCtx(null);
    const id = requestAnimationFrame(() => {
      document.addEventListener("mousedown", hide);
      document.addEventListener("keydown", esc);
    });
    return () => {
      cancelAnimationFrame(id);
      document.removeEventListener("mousedown", hide);
      document.removeEventListener("keydown", esc);
    };
  }, [ctx]);

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
        // 凭据只是「独立连一条」时才需要。拿不到也先往下走：
        // 只要这台机器的终端标签还连着的，后端会复用它，不必再认证。
        let auth: AuthMethod | undefined;
        try {
          auth = await resolveAuth(session, passphrase);
        } catch {
          auth = undefined;
        }
        await api.sftpConnect(sftpId, {
          host: session.host,
          port: session.port,
          username: session.username,
          auth,
          cols: 80,
          rows: 24,
          connectTimeoutSecs: normalizeTimeout(session.connectTimeout),
          // 有已连着的终端标签时，后端直接在那条连接上开 sftp 子系统，
          // 不再走一次认证（OTP 服务器就不会再弹验证码）。
          // 设置里关掉「复用已登录连接」时传 undefined，强制独立拨号。
          configId: getPrefs().shareSftpConnection ? session.id : undefined,
        });
        if (cancelled) return;
        const home = await api.sftpHome(sftpId);
        if (cancelled) return;
        readyRef.current = true;
        homeRef.current = home;
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

  // 本地面板从家目录起步。取不到时 Windows 退回盘符根，而不是 POSIX 的 `/`
  useEffect(() => {
    homeDir()
      .then(setLocalPath)
      .catch(() => setLocalPath(isWindows() ? "C:\\" : "/"));
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

  /** 本地路径栏跳转：先试列目录，成功才切换 */
  const gotoLocal = async (p: string): Promise<boolean> => {
    try {
      const list = await api.listLocalDir(p);
      setLocalSel(null);
      setLocalEntries(list);
      setLocalError(null);
      setLocalPath(p);
      return true;
    } catch (e) {
      setTransferMsg(`无法打开本地路径「${p}」：${e}`);
      return false;
    }
  };

  /** 远端路径栏跳转：支持 ~ / ~/xxx 与相对路径（相对当前目录） */
  const gotoRemote = async (raw: string): Promise<boolean> => {
    if (status !== "ready") return false;
    let p = raw;
    if (p === "~" || p.startsWith("~/")) p = homeRef.current + p.slice(1);
    else if (!p.startsWith("/")) p = joinPath(remotePath, p);
    p = normalizeRemote(p);
    try {
      const list = await api.sftpList(sftpId, p);
      setRemoteSel(null);
      setRemoteEntries(list);
      setRemoteError(null);
      setRemotePath(p);
      return true;
    } catch (e) {
      setTransferMsg(`无法打开远端路径「${p}」：${e}`);
      return false;
    }
  };

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
    } else if (viewableKind(e.name)) {
      // 双击可浏览文件（md / html / 图片 / 音视频 / 配置文件）直接打开
      void doView(e);
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
      const n = await api.sftpDownload(
        sftpId,
        src.path,
        joinLocal(localPath, src.name)
      );
      setTransferMsg(`下载完成 · ${formatSize(n)}`);
      await loadLocal(localPath);
    } catch (e) {
      setTransferMsg(`下载失败: ${e}`);
    }
  };

  /** 在新窗口中浏览：md / html 渲染，图片/音视频播放，配置文件高亮 */
  const doView = async (entry: RemoteEntry) => {
    setCtx(null);
    setTransferMsg(`正在打开「${entry.name}」…`);
    try {
      await api.viewerOpenSftp(
        sftpId,
        entry.path,
        `${session.username}@${session.host}:${entry.path}`
      );
      setTransferMsg(null);
    } catch (e) {
      setTransferMsg(`浏览失败：${e}`);
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
          onUp={() => setLocalPath(parentLocal(localPath))}
          onNavigate={gotoLocal}
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
          onNavigate={gotoRemote}
          onEntryContext={(ev, entry) =>
            setCtx({
              x: Math.min(ev.clientX, window.innerWidth - 200),
              y: Math.min(ev.clientY, window.innerHeight - 190),
              entry,
            })
          }
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

      {ctx && (
        <div ref={ctxRef} className="ctx-menu" style={{ left: ctx.x, top: ctx.y }}>
          {ctx.entry.isDir ? (
            <button
              className="ctx-item"
              onClick={() => {
                const e = ctx.entry;
                setCtx(null);
                enterRemote(e);
              }}
            >
              打开
            </button>
          ) : (
            <>
              <button
                className="ctx-item"
                disabled={!viewableKind(ctx.entry.name)}
                title={
                  viewableKind(ctx.entry.name)
                    ? "在新窗口中查看"
                    : "支持 Markdown、HTML、图片、音频、视频、文本与配置文件（json/yaml/toml/ini/conf/xml/sql…）"
                }
                onClick={() => doView(ctx.entry)}
              >
                浏览
              </button>
              <button
                className="ctx-item"
                onClick={() => {
                  setCtx(null);
                  void doDownload();
                }}
              >
                下载到本地
              </button>
            </>
          )}
          <div className="ctx-sep" />
          <button
            className="ctx-item"
            onClick={() => {
              setCtx(null);
              void doRename();
            }}
          >
            重命名
          </button>
          <button
            className="ctx-item danger"
            onClick={() => {
              setCtx(null);
              void doDelete();
            }}
          >
            删除
          </button>
        </div>
      )}
    </div>
  );
}
