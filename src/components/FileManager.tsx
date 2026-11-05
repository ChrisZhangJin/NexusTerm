import { useCallback, useEffect, useMemo, useRef, useState, type MouseEvent as RMouseEvent, type ReactNode } from "react";
import { homeDir } from "@tauri-apps/api/path";
import { api, onSftpProgress } from "../api";
import { resolveAuth } from "../lib/auth";
import { normalizeTimeout, toFriendlyError } from "../lib/errors";
import { isWindows } from "../lib/platform";
import { getPrefs } from "../lib/prefs";
import type { AuthMethod, RemoteEntry, SessionRecord } from "../types";
import { kindIcon, viewableKind } from "../lib/viewable";

export function formatSize(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} K`;
  if (n < 1024 ** 3) return `${(n / 1024 ** 2).toFixed(1)} M`;
  return `${(n / 1024 ** 3).toFixed(1)} G`;
}

export function formatTime(sec: number | null): string {
  if (sec === null) return "—";
  const d = new Date(sec * 1000);
  const p = (x: number) => String(x).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

/** 「类型」列：简化版，不追求还原系统的完整文件类型数据库，够排序/辨认就行 */
export function typeLabel(e: RemoteEntry): string {
  if (e.isDir) return "文件夹";
  const i = e.name.lastIndexOf(".");
  if (i <= 0 || i === e.name.length - 1) return "文件";
  return `${e.name.slice(i + 1).toUpperCase()} 文件`;
}

/** 传输列表的一行。`id` 由前端生成，贯穿整次传输：既用来对上后端推来的进度事件，也用来发取消指令。 */
interface Transfer {
  id: string;
  kind: "upload" | "download";
  name: string;
  size: number;
  transferred: number;
  localPath: string;
  remotePath: string;
  status: "queued" | "running" | "done" | "error" | "canceled";
  /** 经「队列」加入：排队中的项一次只放行一个，前一个结束才轮到下一个 */
  viaQueue?: boolean;
  error?: string;
  startedAt: number;
  /** 上一次采样的时间戳/字节数，用来算瞬时速度（两次 progress 事件之间的差分） */
  lastSampleAt: number;
  lastSampleBytes: number;
  speed: number;
}

export function formatEta(t: Transfer): string {
  if (t.status !== "running" || t.speed <= 0 || t.size <= 0) return "—";
  const remain = Math.max(0, t.size - t.transferred);
  return formatDuration(remain / t.speed);
}

export function formatDuration(sec: number): string {
  const s = Math.round(sec);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const r = s % 60;
  const p = (x: number) => String(x).padStart(2, "0");
  return h > 0 ? `${p(h)}:${p(m)}:${p(r)}` : `${p(m)}:${p(r)}`;
}

type SortKey = "name" | "size" | "type" | "mtime";

/** 目录永远排文件前面；目录/文件内部再按选中的列比较 */
export function compareEntries(a: RemoteEntry, b: RemoteEntry, key: SortKey, dir: 1 | -1): number {
  if (a.isDir !== b.isDir) return a.isDir ? -1 : 1;
  let c: number;
  switch (key) {
    case "size":
      c = a.size - b.size;
      break;
    case "type":
      c = typeLabel(a).localeCompare(typeLabel(b));
      break;
    case "mtime":
      c = (a.mtime ?? 0) - (b.mtime ?? 0);
      break;
    default:
      c = a.name.localeCompare(b.name);
  }
  return c * dir;
}

/** 远端（SFTP）路径拼接：SFTP 协议一律用 `/`，与本机平台无关。 */
function joinPath(dir: string, name: string): string {
  return dir.endsWith("/") ? `${dir}${name}` : `${dir}/${name}`;
}

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

/** 本地路径拼接。沿用 `joinPath` 会在 Windows 上拼出 `C:\Users\dell/file` 这种混写路径。 */
function joinLocal(dir: string, name: string): string {
  const sep = isWindows() ? "\\" : "/";
  return /[\\/]$/.test(dir) ? `${dir}${name}` : `${dir}${sep}${name}`;
}

/**
 * 本地上级目录。Windows 要额外认盘符根，否则按 `/` 切会得到 -1，一路跳到 `/`，整个面板就废了。
 * 已到盘符根时返回空串 —— 后端 `list_local_dir` 据此列出所有可用盘符。
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
  /** 右键：`entry` 为 null 表示点在列表空白处 */
  onContext?: (e: RMouseEvent, entry: RemoteEntry | null) => void;
  /** 每递增一次就打开一次搜索框（右键菜单「搜索」用） */
  findToken: number;
  children?: ReactNode;
}

/** 可编辑路径栏。外部路径变化（双击进目录 / 上级）时要同步回显，故本地另存一份草稿。 */
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
  onContext,
  findToken,
  children,
}: PaneProps) {
  const [sort, setSort] = useState<{ key: SortKey; dir: 1 | -1 }>({ key: "name", dir: 1 });
  const sorted = useMemo(
    () => [...entries].sort((a, b) => compareEntries(a, b, sort.key, sort.dir)),
    [entries, sort]
  );
  const toggleSort = (key: SortKey) =>
    setSort((s) => (s.key === key ? { key, dir: s.dir === 1 ? -1 : 1 } : { key, dir: 1 }));
  const arrow = (key: SortKey) => (sort.key === key ? (sort.dir === 1 ? " ▲" : " ▼") : "");

  // 搜索 = 按文件名过滤当前目录。点「过滤」或回车才生效（同 Xftp），Esc / × 关闭并还原
  const rootRef = useRef<HTMLDivElement>(null);
  const findRef = useRef<HTMLInputElement>(null);
  const [findOpen, setFindOpen] = useState(false);
  const [draft, setDraft] = useState("");
  const [filter, setFilter] = useState("");
  const openFind = useCallback(() => {
    setFindOpen(true);
    requestAnimationFrame(() => {
      findRef.current?.focus();
      findRef.current?.select();
    });
  }, []);
  const closeFind = () => {
    setFindOpen(false);
    setDraft("");
    setFilter("");
    rootRef.current?.focus();
  };
  useEffect(() => {
    if (findToken > 0) openFind();
  }, [findToken, openFind]);
  // 换了目录，旧的过滤词就没意义了
  useEffect(() => {
    setFindOpen(false);
    setDraft("");
    setFilter("");
  }, [path]);
  const visible = useMemo(() => {
    const q = filter.trim().toLowerCase();
    return q ? sorted.filter((e) => e.name.toLowerCase().includes(q)) : sorted;
  }, [sorted, filter]);

  return (
    <div
      ref={rootRef}
      className="fm-pane"
      // 要先点一下这一栏拿到焦点，Ctrl/Cmd+F 才落在这一栏
      tabIndex={-1}
      onKeyDown={(e) => {
        if ((e.ctrlKey || e.metaKey) && !e.altKey && !e.shiftKey && e.key.toLowerCase() === "f") {
          e.preventDefault();
          openFind();
        }
      }}
    >
      <div className="fm-pane-head">
        <strong>{title}</strong>
        <PathInput path={path} onNavigate={onNavigate} />
        <button className="btn-row" onClick={onUp} title="上级目录">
          ↑
        </button>
      </div>

      <div className="fm-list-head">
        <span className="fm-icon" />
        <button className="fm-name fm-col-btn" onClick={() => toggleSort("name")}>
          名称{arrow("name")}
        </button>
        <button className="fm-size fm-col-btn" onClick={() => toggleSort("size")}>
          大小{arrow("size")}
        </button>
        <button className="fm-type fm-col-btn" onClick={() => toggleSort("type")}>
          类型{arrow("type")}
        </button>
        <button className="fm-mtime fm-col-btn" onClick={() => toggleSort("mtime")}>
          修改时间{arrow("mtime")}
        </button>
      </div>

      <div className="fm-list-wrap">
        <div
          className="fm-list"
          onContextMenu={(ev) => {
            if (!onContext) return;
            ev.preventDefault();
            onContext(ev, null);
          }}
        >
          {loading && <div className="empty">加载中…</div>}
          {error && <div className="error">{error}</div>}
          {!loading && !error && entries.length === 0 && (
            <div className="empty">空目录</div>
          )}
          {!loading && !error && entries.length > 0 && visible.length === 0 && (
            <div className="empty">无匹配项</div>
          )}
          {visible.map((e) => (
            <div
              key={e.path}
              className={`fm-row ${selected === e.path ? "selected" : ""}`}
              onClick={() => onSelect(e)}
              onDoubleClick={() => onActivate(e)}
              onContextMenu={(ev) => {
                if (!onContext) return;
                ev.preventDefault();
                ev.stopPropagation();
                onSelect(e);
                onContext(ev, e);
              }}
            >
              <span className="fm-icon">{e.isDir ? "📁" : kindIcon(viewableKind(e.name))}</span>
              <span className="fm-name">{e.name}</span>
              <span className="fm-size">
                {e.isDir ? "" : formatSize(e.size)}
              </span>
              <span className="fm-type">{typeLabel(e)}</span>
              <span className="fm-mtime">{formatTime(e.mtime)}</span>
            </div>
          ))}
        </div>
        {findOpen && (
          <div className="fm-find" onMouseDown={(e) => e.stopPropagation()}>
            <input
              ref={findRef}
              className="fm-find-input"
              value={draft}
              spellCheck={false}
              placeholder="按文件名过滤"
              onChange={(e) => setDraft(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") setFilter(draft);
                else if (e.key === "Escape") {
                  e.stopPropagation();
                  closeFind();
                }
              }}
            />
            <button className="btn" onClick={() => setFilter(draft)}>
              过滤
            </button>
            <button className="btn fm-find-x" onClick={closeFind} title="关闭 (Esc)">
              ✕
            </button>
          </div>
        )}
      </div>

      {children && <div className="fm-actions">{children}</div>}
    </div>
  );
}

interface Props {
  session: SessionRecord;
  /** 拥有这个面板的标签——SFTP 连接 id 按它来起，见下面 `sftpId` 的注释 */
  tabId: string;
  passphrase?: string;
  onClose: () => void;
  /** 不活跃时只用 CSS 隐藏、保持挂载（连接、路径、传输列表都留着），这样切标签页不会反复重连 SFTP。 */
  active: boolean;
  /** 取终端当前所在的远端目录。打开面板时远端栏据此直接落到那个目录，而不是家目录。 */
  getCwd?: () => Promise<string | undefined>;
  /** 每次从终端「SFTP 文件管理」进来递增；面板已开着时再点一次，远端栏重新同步到终端目录 */
  navToken?: number;
}

type PaneCtx = { x: number; y: number; side: "local" | "remote"; entry: RemoteEntry | null };

export default function FileManager({ session, tabId, passphrase, onClose, active, getCwd, navToken = 0 }: Props) {
  // 连接 id 按「拥有这个面板的标签」来起，不能按 session.id——两个标签完全可能
  // 连的是同一个已保存 session（同一台机器开两个标签），按 session.id 起的话
  // 第二个面板的 sftp_connect 会直接覆盖第一个在后端 map 里的同名条目，
  // 之后随便哪个面板一关（sftp_close）就把另一个也带断了（"会话不存在"）。
  const sftpId = `sftp-${tabId}`;
  const readyRef = useRef(false);
  const homeRef = useRef("/");
  const getCwdRef = useRef(getCwd);
  getCwdRef.current = getCwd;

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
  // 传输完成后的刷新要刷「当时」所在的目录，不是发起传输那一刻闭包里的
  const localPathRef = useRef(localPath);
  localPathRef.current = localPath;
  const remotePathRef = useRef(remotePath);
  remotePathRef.current = remotePath;
  const [localFind, setLocalFind] = useState(0);
  const [remoteFind, setRemoteFind] = useState(0);

  /** 不属于「传输」的一次性提示（新建目录失败、路径跳转失败……） */
  const [transferMsg, setTransferMsg] = useState<string | null>(null);
  const [transfers, setTransfers] = useState<Transfer[]>([]);
  /** 正在等取消生效的 id：取消是异步的（要等拷贝循环下一轮才检查标志），
   *  这段等待期里后端返回的还是「失败」，不是「已取消」——靠这个集合纠正状态。 */
  const cancelingRef = useRef<Set<string>>(new Set());

  useEffect(() => {
    const sub = onSftpProgress(({ transferId, transferred, total }) => {
      setTransfers((prev) =>
        prev.map((t) => {
          if (t.id !== transferId) return t;
          const now = Date.now();
          const dt = (now - t.lastSampleAt) / 1000;
          const speed = dt > 0.2 ? (transferred - t.lastSampleBytes) / dt : t.speed;
          return {
            ...t,
            transferred,
            size: total || t.size,
            speed: dt > 0.2 ? speed : t.speed,
            lastSampleAt: dt > 0.2 ? now : t.lastSampleAt,
            lastSampleBytes: dt > 0.2 ? transferred : t.lastSampleBytes,
          };
        })
      );
    });
    return () => void sub.then((u) => u());
  }, []);

  /** 本地 / 远端文件列表右键菜单（entry 为 null = 点在空白处） */
  const [ctx, setCtx] = useState<PaneCtx | null>(null);
  /** 传输行右键菜单 */
  const [transferCtx, setTransferCtx] = useState<{ x: number; y: number; transfer: Transfer } | null>(null);
  const transferCtxRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!transferCtx) return;
    const hide = (e: MouseEvent) => {
      if (transferCtxRef.current?.contains(e.target as Node)) return;
      setTransferCtx(null);
    };
    const esc = (e: KeyboardEvent) => e.key === "Escape" && setTransferCtx(null);
    const id = requestAnimationFrame(() => {
      document.addEventListener("mousedown", hide);
      document.addEventListener("keydown", esc);
    });
    return () => {
      cancelAnimationFrame(id);
      document.removeEventListener("mousedown", hide);
      document.removeEventListener("keydown", esc);
    };
  }, [transferCtx]);
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

  useEffect(() => {
    let cancelled = false;
    (async () => {
      // 与建连并行去问终端在哪个目录，不拖慢连接
      const cwdAsk = (getCwdRef.current?.() ?? Promise.resolve(undefined)).catch(() => undefined);
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
        // 终端当前目录列得出来就直接落在那里；列不出（已删 / 无权限）退回家目录
        const cwd = await cwdAsk;
        if (cancelled) return;
        let start = home;
        let first: RemoteEntry[] | undefined;
        if (cwd && cwd.startsWith("/") && cwd !== home) {
          try {
            first = await api.sftpList(sftpId, cwd);
            start = cwd;
          } catch {
            first = undefined;
          }
          if (cancelled) return;
        }
        readyRef.current = true;
        homeRef.current = home;
        setRemotePath(start);
        setStatus("ready");
        if (first) {
          setRemoteEntries(first);
          setRemoteError(null);
        } else {
          await loadRemote(home);
        }
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

  // 面板已开着时再从终端点一次「SFTP 文件管理」：远端栏跟到终端当前目录
  const lastNavRef = useRef(navToken);
  useEffect(() => {
    if (navToken === lastNavRef.current) return;
    lastNavRef.current = navToken;
    if (status !== "ready") return; // 还在连：建连流程自己会去取
    void (async () => {
      const cwd = await (getCwdRef.current?.() ?? Promise.resolve(undefined)).catch(() => undefined);
      if (cwd && cwd.startsWith("/") && cwd !== remotePathRef.current) await gotoRemote(cwd);
    })();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [navToken, status]);

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
      void doView(e);
    }
  };

  /** 上传/下载共用。传入的 `id` 会按 id 去重替换已有行，使「重试」在原行上重开而不是另起一条。 */
  const runTransfer = (
    kind: "upload" | "download",
    name: string,
    local: string,
    remote: string,
    size: number,
    id: string,
    viaQueue = false
  ) => {
    setTransfers((prev) => [
      {
        id,
        kind,
        name,
        size,
        transferred: 0,
        localPath: local,
        remotePath: remote,
        status: "running",
        viaQueue,
        startedAt: Date.now(),
        lastSampleAt: Date.now(),
        lastSampleBytes: 0,
        speed: 0,
      },
      ...prev.filter((t) => t.id !== id),
    ]);
    const run = kind === "upload" ? api.sftpUpload(sftpId, local, remote, id) : api.sftpDownload(sftpId, remote, local, id);
    run
      .then((n) => {
        setTransfers((prev) => prev.map((t) => (t.id === id ? { ...t, status: "done", transferred: n, size: n } : t)));
        if (kind === "upload") void loadRemote(remotePathRef.current);
        else void loadLocal(localPathRef.current);
      })
      .catch((e) => {
        const wasCanceling = cancelingRef.current.delete(id);
        setTransfers((prev) =>
          prev.map((t) =>
            t.id === id ? { ...t, status: wasCanceling ? "canceled" : "error", error: wasCanceling ? undefined : String(e) } : t
          )
        );
      });
  };

  const doUpload = async () => {
    const src = localEntries.find((e) => e.path === localSel);
    if (!src || src.isDir) {
      setTransferMsg("请先在左侧选择一个本地文件");
      return;
    }
    runTransfer("upload", src.name, src.path, joinPath(remotePath, src.name), src.size, crypto.randomUUID());
  };

  const doDownload = async () => {
    const src = remoteEntries.find((e) => e.path === remoteSel);
    if (!src || src.isDir) {
      setTransferMsg("请先在右侧选择一个远端文件");
      return;
    }
    runTransfer("download", src.name, joinLocal(localPath, src.name), src.path, src.size, crypto.randomUUID());
  };

  /** 右键「传输」/「队列」：`side` 是被点的那一栏，本地 → 上传，远端 → 下载 */
  const transferEntry = (side: "local" | "remote", e: RemoteEntry, queued: boolean) => {
    if (status !== "ready") return;
    if (e.isDir) {
      setTransferMsg("暂不支持传输目录，请选择文件");
      return;
    }
    const kind = side === "local" ? "upload" : "download";
    const local = side === "local" ? e.path : joinLocal(localPath, e.name);
    const remote = side === "local" ? joinPath(remotePath, e.name) : e.path;
    const id = crypto.randomUUID();
    if (!queued) {
      runTransfer(kind, e.name, local, remote, e.size, id);
      return;
    }
    const now = Date.now();
    setTransfers((prev) => [
      {
        id,
        kind,
        name: e.name,
        size: e.size,
        transferred: 0,
        localPath: local,
        remotePath: remote,
        status: "queued",
        viaQueue: true,
        startedAt: now,
        lastSampleAt: now,
        lastSampleBytes: 0,
        speed: 0,
      },
      ...prev,
    ]);
  };

  // 队列按加入顺序（列表是新的在前，所以从尾部找）一次放行一个
  useEffect(() => {
    if (status !== "ready") return;
    const q = transfers.filter((t) => t.viaQueue);
    if (q.some((t) => t.status === "running")) return;
    const next = [...q].reverse().find((t) => t.status === "queued");
    if (next) runTransfer(next.kind, next.name, next.localPath, next.remotePath, next.size, next.id, true);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [transfers, status]);

  const retryTransfer = (t: Transfer) => {
    setTransferCtx(null);
    // 复用同一个 id：重试是在这一行上重新开始，不是另起一条新记录
    runTransfer(t.kind, t.name, t.localPath, t.remotePath, t.size, t.id);
  };

  const cancelTransfer = (t: Transfer) => {
    if (t.status === "queued") {
      setTransfers((prev) => prev.map((x) => (x.id === t.id ? { ...x, status: "canceled" } : x)));
      return;
    }
    if (t.status !== "running") return;
    cancelingRef.current.add(t.id);
    void api.sftpCancelTransfer(t.id).catch(() => {});
  };

  const removeTransfer = (id: string) => setTransfers((prev) => prev.filter((t) => t.id !== id));
  const clearFinishedTransfers = () =>
    setTransfers((prev) => prev.filter((t) => t.status === "running" || t.status === "queued"));
  const cancelAllTransfers = () =>
    transfers.filter((t) => t.status === "running" || t.status === "queued").forEach(cancelTransfer);

  const revealLocal = async (path: string) => {
    try {
      const { revealItemInDir } = await import("@tauri-apps/plugin-opener");
      await revealItemInDir(path);
    } catch {
      /* 打不开就算了 */
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

  const doDelete = async (entry?: RemoteEntry) => {
    const src = entry ?? remoteEntries.find((e) => e.path === remoteSel);
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

  const doRename = async (entry?: RemoteEntry) => {
    const src = entry ?? remoteEntries.find((e) => e.path === remoteSel);
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

  const doRenameLocal = async (src: RemoteEntry) => {
    const name = prompt("新名称", src.name);
    if (!name || name === src.name) return;
    try {
      await api.renameLocal(src.path, joinLocal(localPath, name));
      setLocalSel(null);
      await loadLocal(localPath);
    } catch (e) {
      setTransferMsg(`重命名失败: ${e}`);
    }
  };

  const copyRemotePath = async (p: string) => {
    try {
      await api.clipboardWrite(p);
      setTransferMsg(`已复制路径：${p}`);
    } catch (e) {
      setTransferMsg(`复制失败: ${e}`);
    }
  };

  const openPaneCtx = (side: "local" | "remote", ev: RMouseEvent, entry: RemoteEntry | null) =>
    setCtx({
      side,
      entry,
      x: Math.min(ev.clientX, window.innerWidth - 200),
      y: Math.min(ev.clientY, window.innerHeight - (side === "remote" ? 320 : 220)),
    });

  return (
    <div className="fm" style={{ display: active ? "flex" : "none" }}>
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
          onContext={(ev, entry) => openPaneCtx("local", ev, entry)}
          findToken={localFind}
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
          onContext={(ev, entry) => openPaneCtx("remote", ev, entry)}
          findToken={remoteFind}
        >
          <button className="btn" onClick={doDownload} disabled={status !== "ready"}>
            ← 下载
          </button>
          <button className="btn" onClick={doMkdir} disabled={status !== "ready"}>
            新建目录
          </button>
          <button className="btn" onClick={() => void doRename()} disabled={status !== "ready"}>
            重命名
          </button>
          <button
            className="btn danger"
            onClick={() => void doDelete()}
            disabled={status !== "ready"}
          >
            删除
          </button>
        </Pane>
      </div>

      {transferMsg && <div className="fm-transfer">{transferMsg}</div>}

      {transfers.length > 0 && (
        <div className="fm-xfer">
          <div className="fm-xfer-head">
            <span>名称</span>
            <span>状态</span>
            <span>进度</span>
            <span>大小</span>
            <span>本地路径</span>
            <span>&lt;-&gt;</span>
            <span>远程路径</span>
            <span>速度</span>
            <span>剩余</span>
            <button className="btn-row fm-xfer-clear" onClick={clearFinishedTransfers} title="清除已完成">
              ✕
            </button>
          </div>
          <div className="fm-xfer-list">
            {transfers.map((t) => {
              const pct = t.size > 0 ? Math.min(100, Math.round((t.transferred / t.size) * 100)) : 0;
              return (
                <div
                  key={t.id}
                  className="fm-xfer-row"
                  onContextMenu={(ev) => {
                    ev.preventDefault();
                    setTransferCtx({
                      x: Math.min(ev.clientX, window.innerWidth - 180),
                      y: Math.min(ev.clientY, window.innerHeight - 200),
                      transfer: t,
                    });
                  }}
                >
                  <span className="fm-xfer-name" title={t.name}>
                    {t.name}
                  </span>
                  <span className={`fm-xfer-status fm-xfer-status-${t.status}`}>
                    {t.status === "queued"
                      ? "排队中"
                      : t.status === "running"
                      ? "进行中"
                      : t.status === "done"
                        ? "已完成"
                        : t.status === "canceled"
                          ? "已取消"
                          : "失败"}
                  </span>
                  <span className="fm-xfer-progress">
                    <span className="fm-xfer-bar">
                      <span className="fm-xfer-bar-fill" style={{ width: `${pct}%` }} />
                    </span>
                    {pct}%
                  </span>
                  <span className="fm-xfer-size">
                    {formatSize(t.transferred)}
                    {t.size > 0 ? ` / ${formatSize(t.size)}` : ""}
                  </span>
                  <span className="fm-xfer-path" title={t.localPath}>
                    {t.localPath}
                  </span>
                  <span className="fm-xfer-dir">{t.kind === "upload" ? "↑" : "↓"}</span>
                  <span className="fm-xfer-path" title={t.remotePath}>
                    {t.remotePath}
                  </span>
                  <span className="fm-xfer-speed">{t.status === "running" ? `${formatSize(t.speed)}/s` : "—"}</span>
                  <span className="fm-xfer-eta">{formatEta(t)}</span>
                </div>
              );
            })}
          </div>
        </div>
      )}

      {transferCtx && (
        <div ref={transferCtxRef} className="ctx-menu" style={{ left: transferCtx.x, top: transferCtx.y }}>
          <button
            className="ctx-item"
            disabled={transferCtx.transfer.status !== "running" && transferCtx.transfer.status !== "queued"}
            onClick={() => {
              cancelTransfer(transferCtx.transfer);
              setTransferCtx(null);
            }}
          >
            取消
          </button>
          <button
            className="ctx-item"
            disabled={
              transferCtx.transfer.status === "running" ||
              transferCtx.transfer.status === "queued" ||
              transferCtx.transfer.status === "done"
            }
            onClick={() => retryTransfer(transferCtx.transfer)}
          >
            重试
          </button>
          <button
            className="ctx-item"
            onClick={() => void revealLocal(transferCtx.transfer.localPath)}
          >
            在资源管理器中打开
          </button>
          <div className="ctx-sep" />
          <button
            className="ctx-item"
            disabled={transferCtx.transfer.status === "running"}
            onClick={() => {
              removeTransfer(transferCtx.transfer.id);
              setTransferCtx(null);
            }}
          >
            删除本条记录
          </button>
          <button
            className="ctx-item"
            onClick={() => {
              clearFinishedTransfers();
              setTransferCtx(null);
            }}
          >
            清除已完成
          </button>
          <button
            className="ctx-item danger"
            onClick={() => {
              cancelAllTransfers();
              setTransferCtx(null);
            }}
          >
            全部取消
          </button>
        </div>
      )}

      {ctx && (() => {
        const { side, entry } = ctx;
        const remote = side === "remote";
        const xferOk = status === "ready" && !!entry && !entry.isDir;
        const xferTip = entry?.isDir ? "暂不支持传输目录" : undefined;
        const done = () => setCtx(null);
        return (
          <div ref={ctxRef} className="ctx-menu" style={{ left: ctx.x, top: ctx.y }}>
            {remote && entry?.isDir && (
              <button
                className="ctx-item"
                onClick={() => {
                  done();
                  enterRemote(entry);
                }}
              >
                打开
              </button>
            )}
            {remote && entry && !entry.isDir && (
              <button
                className="ctx-item"
                disabled={!viewableKind(entry.name)}
                title={
                  viewableKind(entry.name)
                    ? "在新窗口中查看"
                    : "支持 Markdown、HTML、图片、音频、视频、文本与配置文件（json/yaml/toml/ini/conf/xml/sql…）"
                }
                onClick={() => void doView(entry)}
              >
                浏览
              </button>
            )}
            <button
              className="ctx-item"
              disabled={!xferOk}
              title={xferTip}
              onClick={() => {
                done();
                if (entry) transferEntry(side, entry, false);
              }}
            >
              传输
            </button>
            <button
              className="ctx-item"
              disabled={!xferOk}
              title={xferTip ?? "加入传输队列，按顺序逐个传输"}
              onClick={() => {
                done();
                if (entry) transferEntry(side, entry, true);
              }}
            >
              队列
            </button>
            <div className="ctx-sep" />
            <button
              className="ctx-item"
              onClick={() => {
                done();
                if (remote) setRemoteFind((n) => n + 1);
                else setLocalFind((n) => n + 1);
              }}
            >
              搜索
            </button>
            {remote && (
              <button
                className="ctx-item"
                onClick={() => {
                  done();
                  void copyRemotePath(entry ? entry.path : remotePath);
                }}
              >
                复制路径
              </button>
            )}
            <button
              className="ctx-item"
              disabled={!entry || (remote && status !== "ready")}
              onClick={() => {
                done();
                if (!entry) return;
                if (remote) void doRename(entry);
                else void doRenameLocal(entry);
              }}
            >
              重命名
            </button>
            {remote && (
              <button
                className="ctx-item danger"
                disabled={!entry || status !== "ready"}
                onClick={() => {
                  done();
                  if (entry) void doDelete(entry);
                }}
              >
                删除
              </button>
            )}
            <div className="ctx-sep" />
            <button
              className="ctx-item"
              disabled={remote && status !== "ready"}
              onClick={() => {
                done();
                if (remote) void loadRemote(remotePath);
                else void loadLocal(localPath);
              }}
            >
              刷新
            </button>
          </div>
        );
      })()}
    </div>
  );
}
