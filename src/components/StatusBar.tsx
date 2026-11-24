import { useCallback, useEffect, useRef, useState } from "react";
import { api, onTermPulse } from "../api";
import { t, useLocale } from "../i18n";
import { setPrefs, usePrefs } from "../lib/prefs";
import { getOutputCount } from "../lib/termRegistry";
import { getExtras, isSidecarDenied, markCompat, updateExtras } from "../lib/sessionExtras";

interface Props {
  /** 当前激活标签的状态 */
  status: "idle" | "connecting" | "connected" | "reconnecting" | "error";
  /** 形如 root@192.168.1.10:22，未连接时为空 */
  peer: string | null;
  /** 当前打开的标签数 */
  sessionCount: number;
  /** 其中处于已连接状态的数量 */
  connectedCount: number;
  /** 资源采集目标：已连接的 SSH 标签，或本地终端（采本机）；无则 null。
   *  hostKey 相同的标签共享一份数据（user@host:port，本机为 "local"） */
  statsTarget: {
    tabId: string;
    local: boolean;
    hostKey: string;
    /** 对应的会话配置 id：发现堡垒机拒绝并行通道时要靠它记住兼容模式 */
    sessionId?: string | null;
  } | null;
  /** 当前激活标签（统计输出字符数） */
  activeTabId: string | null;
  /** 复制成功 / 失败时弹一条提示（没传就静默） */
  onNotify?: (msg: string) => void;
  /** 打开「设置」下的某个窗口 */
  onOpenSettings: (
    which: "appearance" | "behavior" | "shortcuts" | "keys" | "agent" | "security"
  ) => void;
  /** 有设置窗口打开时高亮齿轮 */
  settingsOpen: boolean;
}

const LABEL: Record<Props["status"], string> = {
  get idle() { return t("未连接", "Not connected"); },
  get connecting() { return t("连接中", "Connecting"); },
  get connected() { return t("已连接", "Connected"); },
  get reconnecting() { return t("重连中", "Reconnecting"); },
  get error() { return t("连接失败", "Connection failed"); },
};

/** 资源刷新间隔 */
const STATS_INTERVAL = 5000;

/** 心电波形（两次心跳之间的平线 + 一次 QRS 脉冲） */
const ECG_PATH = "M0 8h9l3-4 3 8 3-4h7l3-5 3 10 3-5h13";

/** 这次脉搏是从哪来的：服务端 keepalive，还是「一次成功的资源采集」 */
type BeatSource = "keepalive" | "stats";

/**
 * 链路脉搏指示。服务端 keepalive，或一次成功的资源采集（exec 通道往返通了）；
 * 后者对不发 keepalive 的服务器（OpenSSH 默认 `ClientAliveInterval 0`）是唯一证据。
 * 两条都没有就只画暗基线：不假装链路有心跳。
 */
function Ecg({
  beat,
  lastBeat,
  source,
}: {
  beat: number;
  lastBeat: number | null;
  source: BeatSource | null;
}) {
  useLocale();
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(id);
  }, []);
  const age = lastBeat === null ? null : Math.max(0, Math.round((now - lastBeat) / 1000));
  // 90 秒没有任何脉搏：整条波形压低，别让「上次的余辉」看起来像还活着
  const stale = age !== null && age > 90;
  return (
    <span
      className={`sb-ecg${beat > 0 ? " live" : ""}${stale ? " stale" : ""}`}
      title={
        age === null
          ? t("等待链路脉搏：服务端心跳，或一次成功的资源采集", "Waiting for a link pulse: a server keepalive or a successful resource poll")
          : t("链路脉搏：{s} 秒前 · {src}", "Link pulse: {s}s ago · {src}", {
              s: age,
              src: source === "stats" ? t("资源采集回应", "resource poll reply") : t("服务端心跳", "server keepalive"),
            })
      }
    >
      <svg viewBox="0 0 56 16" width="56" height="16" aria-hidden="true">
        <path className="sb-ecg-base" d={ECG_PATH} />
        {/* key 变化 = 重新挂载 = 重放一次描线 + 余辉淡出 */}
        {beat > 0 && <path key={beat} className="sb-ecg-beat" d={ECG_PATH} />}
      </svg>
    </span>
  );
}

interface Stats {
  cpus?: number;
  cpuPct?: number;
  load?: string;
  memTotal?: number;
  memAvail?: number;
  diskTotal?: number;
  diskUsed?: number;
  diskMount?: string;
}

function parseStats(r: Record<string, string>): Stats {
  const num = (k: string) => {
    const v = Number(r[k]);
    return Number.isFinite(v) && r[k] !== undefined ? v : undefined;
  };
  return {
    cpus: num("CPUS"),
    cpuPct: num("CPU_PCT"),
    load: r.LOAD,
    memTotal: num("MEM_TOTAL"),
    memAvail: num("MEM_AVAIL"),
    diskTotal: num("DISK_TOTAL"),
    diskUsed: num("DISK_USED"),
    diskMount: r.DISK_MOUNT,
  };
}

function fmtBytes(n: number): string {
  const G = 1024 ** 3;
  if (n >= 1024 * G) return `${(n / 1024 / G).toFixed(1)}T`;
  if (n >= G) return `${(n / G).toFixed(1)}G`;
  return `${(n / 1024 ** 2).toFixed(0)}M`;
}

/** 占用率分级：<60 正常，<85 偏高，否则告警 —— 决定进度条颜色 */
const level = (pct: number) => (pct < 60 ? "ok" : pct < 85 ? "warn" : "crit");

/** 数字补间：每次刷新从旧值滚到新值（约 600ms，easeOutCubic） */
function useTween(target: number | undefined, ms = 600, animate = true): number | undefined {
  const [v, setV] = useState(target);
  const fromRef = useRef(target);
  useEffect(() => {
    if (target === undefined || !animate) {
      fromRef.current = target;
      setV(target);
      return;
    }
    const from = fromRef.current ?? target;
    const t0 = performance.now();
    let raf = 0;
    const step = (now: number) => {
      const k = Math.min(1, (now - t0) / ms);
      const e = 1 - Math.pow(1 - k, 3);
      const cur = from + (target - from) * e;
      fromRef.current = cur;
      setV(cur);
      if (k < 1) raf = requestAnimationFrame(step);
    };
    raf = requestAnimationFrame(step);
    return () => cancelAnimationFrame(raf);
  }, [target, ms, animate]);
  return v;
}

function Meter({
  label,
  pct,
  text,
  title,
  pulse,
  animate,
}: {
  label: string;
  pct: number | undefined;
  text: string;
  title: string;
  pulse: number;
  animate: boolean;
}) {
  const shown = useTween(pct, 600, animate);
  const p = Math.max(0, Math.min(100, shown ?? 0));
  return (
    <span className="sb-meter" title={title}>
      <span className="sb-meter-label">{label}</span>
      <span className="sb-bar">
        <span
          className={`sb-bar-fill lv-${level(pct ?? 0)}`}
          // 宽度直接给目标值，过渡交给 CSS；数字由 useTween 逐帧滚动
          style={{ width: `${pct === undefined ? 0 : Math.max(0, Math.min(100, pct))}%` }}
        />
        {/* key 变化即重放一次扫光动画 */}
        {animate && <span key={pulse} className="sb-bar-sheen" />}
      </span>
      <span className="sb-meter-text">
        {pct === undefined ? "—" : `${p.toFixed(pct < 10 ? 1 : 0)}%`}
      </span>
      {text && <span className="sb-meter-sub">{text}</span>}
    </span>
  );
}

/** 按主机记住查到的公网 IP（null = 查过但没拿到），切回同一主机不再重查 */
const publicIpCache = new Map<string, string | null>();

/**
 * 当前主机的公网 IP：切到一个新主机时查一次（SSH 在远端经旁路通道问，本地在本机问），不轮询。
 * 点击复制，双击重新查询。兼容模式（堡垒机，不开旁路通道）不查。
 */
function PublicIp({
  hostKey,
  tabId,
  local,
  sessionId,
  onNotify,
}: {
  hostKey: string;
  tabId: string;
  local: boolean;
  sessionId?: string | null;
  onNotify?: (msg: string) => void;
}) {
  useLocale();
  const [ip, setIp] = useState<string | null | undefined>(() => publicIpCache.get(hostKey));
  const [busy, setBusy] = useState(false);
  const compat = !local && getExtras(sessionId).compat;
  const query = useCallback(async () => {
    if (compat) return;
    setBusy(true);
    try {
      const r = await api.hostPublicIp(tabId);
      publicIpCache.set(hostKey, r);
      setIp(r);
    } catch {
      publicIpCache.set(hostKey, null);
      setIp(null);
    } finally {
      setBusy(false);
    }
    // tabId 变了（同主机的另一个标签）不必重查：key 按主机
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [hostKey, compat]);
  useEffect(() => {
    if (!publicIpCache.has(hostKey)) void query();
  }, [hostKey, query]);
  if (compat) return null;
  const where = local ? t("本机", "This machine") : t("这台主机", "This host");
  return (
    <span
      className="sb-item sb-ip"
      title={
        ip
          ? t(
              "{where}的公网 IP（出网时对外显示的地址）\n点击复制 · 双击重新查询",
              "{where}: public IP (the address seen from outside)\nClick to copy · double-click to look up again",
              { where }
            )
          : busy
            ? t("正在查询公网 IP…", "Looking up public IP…")
            : t(
                "没查到{where}的公网 IP（可能不能出网，或没有 curl / wget）\n双击重新查询",
                "{where}: couldn't find the public IP (no internet access, or no curl / wget)\nDouble-click to try again",
                { where }
              )
      }
      onClick={() => {
        if (!ip) return;
        void api
          .clipboardWrite(ip)
          .then(() => onNotify?.(t("已复制公网 IP {ip}", "Copied public IP {ip}", { ip })))
          .catch(() => {});
      }}
      onDoubleClick={() => void query()}
    >
      {t("公网", "Public IP")} {busy && ip === undefined ? "…" : ip ?? "—"}
    </span>
  );
}

/**
 * 按主机共享的最近一次采集结果（key = user@host:port，本机为 "local"）：
 * 切回同主机的另一个标签时立刻显示上次数值，不再闪「采集资源…」。
 */
const statsCache = new Map<string, Stats>();

function ResourceMeters({
  hostKey,
  tabId,
  local,
  live,
  sessionId,
  onContextMenu,
  onBeat,
}: {
  hostKey: string;
  tabId: string;
  local: boolean;
  live: boolean;
  sessionId?: string | null;
  onContextMenu: (e: React.MouseEvent) => void;
  /** 一次采集成功 = 链路脉搏（对不发 keepalive 的服务器是唯一证据） */
  onBeat: () => void;
}) {
  useLocale();
  const [stats, setStats] = useState<Stats | null>(() => statsCache.get(hostKey) ?? null);
  const [pulse, setPulse] = useState(0);
  const [failed, setFailed] = useState(false);
  // 兼容模式（堡垒机）：不再轮询。用 ref 让 tick 立刻看到，不必等重渲染
  const compat = useRef(getExtras(sessionId).compat);
  compat.current = getExtras(sessionId).compat;
  // 只是为了让「兼容模式」这句说明能渲染出来（ref 变化不触发重渲染）
  const [compatNow, setCompatNow] = useState(compat.current);
  const [retrying, setRetrying] = useState(false);
  const [note, setNote] = useState<"denied" | null>(null);
  // 「已确认只支持单通道」这句提示只是临时反馈，几秒后回到可重试的样子
  useEffect(() => {
    if (!note) return;
    const id = window.setTimeout(() => setNote(null), 4000);
    return () => window.clearTimeout(id);
  }, [note]);
  // 采集用的标签可能变（同主机的另一个标签被激活），用 ref 取最新值，不重启轮询
  const tabRef = useRef(tabId);
  tabRef.current = tabId;
  // 回调也走 ref：它每次渲染都是新函数，不能进 effect 依赖（否则轮询会被反复重启）
  const beatRef = useRef(onBeat);
  beatRef.current = onBeat;

  useEffect(() => {
    let alive = true;
    let busy = false;
    setFailed(false);
    const tick = async (animate: boolean) => {
      if (busy || compat.current) return; // 上一次还没回来（网络慢）就跳过，避免堆积
      busy = true;
      try {
        const r = local ? await api.localStats() : await api.sshStats(tabRef.current);
        if (!alive) return;
        const st = parseStats(r);
        statsCache.set(hostKey, st);
        setStats(st);
        setFailed(false);
        // exec 通道往返成功 → 链路有回应，和 keepalive 一样算一次脉搏
        beatRef.current();
        if (animate) setPulse((n) => n + 1);
      } catch (e) {
        // 堡垒机拒绝第二条通道：这里已经是「第二次被拒」，再轮询只会把连接
        // 一次次顶掉。切兼容模式，状态栏改成明确的说明而不是反复闪「不可用」。
        if (!local && isSidecarDenied(e) && markCompat(sessionId, tabRef.current)) {
          compat.current = true;
          if (alive) {
            setFailed(true);
            setCompatNow(true);
          }
          return;
        }
        if (alive) setFailed(true);
      } finally {
        busy = false;
      }
    };
    // 关闭实时监控时若这台主机还从没采过，补采一次静态值，避免一直空着
    if (!live) {
      if (!statsCache.has(hostKey)) tick(false);
      return () => {
        alive = false;
      };
    }
    tick(true);
    const id = window.setInterval(() => tick(true), STATS_INTERVAL);
    return () => {
      alive = false;
      window.clearInterval(id);
    };
  }, [hostKey, local, live, sessionId]);

  const where = local ? t("本机", "This machine") : t("远端", "Remote");

  /**
   * 重新检测「是否真的只允许一条通道」。兼容模式一旦记住就永不再试探、会自我固化
   * （可能是通道号错位那个 bug 期间留下的误判），这里给一个显式出口。
   */
  const retry = async () => {
    if (retrying || !sessionId) return;
    setRetrying(true);
    try {
      await api.sshSetSidecar(tabRef.current, true);
      const r = await api.sshStats(tabRef.current);
      const st = parseStats(r);
      statsCache.set(hostKey, st);
      setStats(st);
      setFailed(false);
      compat.current = false;
      setCompatNow(false);
      updateExtras(sessionId, { compat: false });
    } catch {
      // 真的拒绝：恢复「不开旁路」，并明确告诉用户这次是现场确认过的
      api.sshSetSidecar(tabRef.current, false).catch(() => {});
      setNote("denied");
    } finally {
      setRetrying(false);
    }
  };

  if (!stats) {
    return (
      <span
        className="sb-res sb-res-loading"
        onContextMenu={onContextMenu}
        title={
          compatNow
            ? t(
                "该服务器曾被判定只允许一条 SSH 通道（堡垒机），此设置已记住，所以不采集远端资源。\n这是历史记录，不是算法降级 —— 点此重新检测：能取到数据就自动解除。\n（若它确实会掐断第二条通道，检测可能让当前连接断开）",
                "This server was found to allow only one SSH channel (bastion host). That's remembered, so remote resources aren't collected.\nIt's a saved record, not a fallback — click to re-check: if data comes back, it's cleared automatically.\n(If the server really kills a second channel, the check may drop the current connection)"
              )
            : t("右键可关闭实时监控", "Right-click to turn off live monitoring")
        }
      >
        {compatNow ? (
          <button type="button" className="sb-compat" disabled={retrying} onClick={retry}>
            {retrying
              ? t("检测通道…", "Checking channel…")
              : note === "denied"
                ? t("仅单通道 · 已确认", "Single channel · confirmed")
                : t("兼容模式 · 重检", "Compatibility mode · re-check")}
          </button>
        ) : failed ? (
          t("资源信息不可用", "Resource info unavailable")
        ) : (
          <>
            <span className="spinner sm" /> {t("采集资源…", "Collecting resources…")}
          </>
        )}
      </span>
    );
  }

  const memPct =
    stats.memTotal && stats.memAvail !== undefined
      ? ((stats.memTotal - stats.memAvail) / stats.memTotal) * 100
      : undefined;
  const diskPct =
    stats.diskTotal && stats.diskUsed !== undefined
      ? (stats.diskUsed / stats.diskTotal) * 100
      : undefined;

  return (
    <span
      className={`sb-res${failed ? " stale" : ""}${live ? "" : " paused"}`}
      onContextMenu={onContextMenu}
      title={
        live
          ? undefined
          : t("{where} · 实时监控已关闭，显示的是最后一次数值（右键开启）", "{where} · Live monitoring is off; showing the last values (right-click to turn on)", {
              where,
            })
      }
    >
      {live && (
        <span
          key={pulse}
          className="sb-res-dot"
          title={`${where} · ${failed ? t("最近一次刷新失败", "Last refresh failed") : t("每 5 秒刷新", "Refreshes every 5s")}${t(
            "（右键可关闭实时监控）",
            " (right-click to turn off live monitoring)"
          )}`}
        />
      )}
      {local && <span className="sb-res-where">{t("本机", "Local")}</span>}
      <Meter
        label="CPU"
        pct={stats.cpuPct}
        text={stats.cpus ? t("{n} 核", "{n} cores", { n: stats.cpus }) : ""}
        title={
          t("CPU：{n} 核，当前占用 {pct}%", "CPU: {n} cores, {pct}% in use", {
            n: stats.cpus ?? "?",
            pct: stats.cpuPct?.toFixed(1) ?? "?",
          }) + (stats.load ? t("\n平均负载（1/5/15 分钟）：{l}", "\nLoad average (1/5/15 min): {l}", { l: stats.load }) : "")
        }
        pulse={pulse}
        animate={live}
      />
      <Meter
        label={t("内存", "Mem")}
        pct={memPct}
        text={
          stats.memTotal !== undefined && stats.memAvail !== undefined
            ? t("可用 {a} / {b}", "{a} free / {b}", { a: fmtBytes(stats.memAvail), b: fmtBytes(stats.memTotal) })
            : ""
        }
        title={
          stats.memTotal !== undefined && stats.memAvail !== undefined
            ? t("内存：总量 {total}，可用 {free}，已用 {used}", "Memory: {total} total, {free} available, {used} used", {
                total: fmtBytes(stats.memTotal),
                free: fmtBytes(stats.memAvail),
                used: fmtBytes(stats.memTotal - stats.memAvail),
              })
            : t("内存信息不可用", "Memory info unavailable")
        }
        pulse={pulse}
        animate={live}
      />
      <Meter
        label={t("磁盘", "Disk")}
        pct={diskPct}
        text={
          stats.diskTotal !== undefined && stats.diskUsed !== undefined
            ? `${fmtBytes(stats.diskUsed)} / ${fmtBytes(stats.diskTotal)}`
            : ""
        }
        title={
          stats.diskTotal !== undefined && stats.diskUsed !== undefined
            ? t("磁盘（{m}）：总量 {total}，已用 {used}", "Disk ({m}): {total} total, {used} used", {
                m: stats.diskMount ?? "/",
                total: fmtBytes(stats.diskTotal),
                used: fmtBytes(stats.diskUsed),
              })
            : t("磁盘信息不可用", "Disk info unavailable")
        }
        pulse={pulse}
        animate={live}
      />
    </span>
  );
}

/** 1234 → 1,234；≥1 万用 K/M 缩写 */
function fmtCount(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(n >= 10_000_000 ? 0 : 1)}M`;
  if (n >= 10_000) return `${(n / 1000).toFixed(n >= 100_000 ? 0 : 1)}K`;
  return n.toLocaleString();
}

function fmtSize(n: number): string {
  if (n >= 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  if (n >= 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${n} B`;
}

/** NumLock / CapsLock：浏览器拿不到初始状态，只能从下一次键盘/鼠标事件里读 */
function useLockKeys() {
  const [st, setSt] = useState<{ num: boolean; cap: boolean } | null>(null);
  useEffect(() => {
    const read = (e: KeyboardEvent | MouseEvent) => {
      if (typeof e.getModifierState !== "function") return;
      const num = e.getModifierState("NumLock");
      // keydown CapsLock 本身时，状态已经是切换后的值（Chromium 行为）
      const cap = e.getModifierState("CapsLock");
      setSt((p) => (p && p.num === num && p.cap === cap ? p : { num, cap }));
    };
    // capture：xterm 的 textarea 会吞掉部分键盘事件，必须在捕获阶段读
    window.addEventListener("keydown", read, true);
    window.addEventListener("keyup", read, true);
    window.addEventListener("mousedown", read, true);
    window.addEventListener("mousemove", read, { capture: true, passive: true });
    return () => {
      window.removeEventListener("keydown", read, true);
      window.removeEventListener("keyup", read, true);
      window.removeEventListener("mousedown", read, true);
      window.removeEventListener("mousemove", read, true);
    };
  }, []);
  return st;
}

/** 当前标签累计输出量；每秒读一次登记表（计数在数据回调里累加，这里只取值） */
function useOutputCount(tabId: string | null) {
  const [c, setC] = useState({ chars: 0, bytes: 0 });
  useEffect(() => {
    if (!tabId) {
      setC({ chars: 0, bytes: 0 });
      return;
    }
    const read = () => {
      const n = getOutputCount(tabId);
      setC((p) => (p.chars === n.chars && p.bytes === n.bytes ? p : n));
    };
    read();
    const id = window.setInterval(read, 1000);
    return () => window.clearInterval(id);
  }, [tabId]);
  return c;
}

export default function StatusBar({
  status,
  peer,
  sessionCount,
  connectedCount,
  statsTarget,
  activeTabId,
  onNotify,
  onOpenSettings,
  settingsOpen,
}: Props) {
  useLocale();
  const prefs = usePrefs();
  const locks = useLockKeys();
  const out = useOutputCount(activeTabId);
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  const [settingsMenuOpen, setSettingsMenuOpen] = useState(false);
  const settingsMenuRef = useRef<HTMLDivElement>(null);

  // 设置菜单：点外 / Esc / 窗口失焦收起
  useEffect(() => {
    if (!settingsMenuOpen) return;
    const down = (e: MouseEvent) => {
      if (!settingsMenuRef.current?.contains(e.target as Node)) setSettingsMenuOpen(false);
    };
    const key = (e: KeyboardEvent) => e.key === "Escape" && setSettingsMenuOpen(false);
    const blur = () => setSettingsMenuOpen(false);
    document.addEventListener("mousedown", down);
    window.addEventListener("keydown", key);
    window.addEventListener("blur", blur);
    return () => {
      document.removeEventListener("mousedown", down);
      window.removeEventListener("keydown", key);
      window.removeEventListener("blur", blur);
    };
  }, [settingsMenuOpen]);

  const pickSettings = (
    w: "appearance" | "behavior" | "shortcuts" | "keys" | "agent" | "security"
  ) => {
    setSettingsMenuOpen(false);
    onOpenSettings(w);
  };

  // 只订阅当前标签：切走时清空状态，不给后台标签白跑动效
  const [beat, setBeat] = useState(0);
  const [lastBeat, setLastBeat] = useState<number | null>(null);
  const [beatSource, setBeatSource] = useState<BeatSource | null>(null);
  const bump = useCallback((source: BeatSource) => {
    setBeatSource(source);
    setLastBeat(Date.now());
    setBeat((n) => n + 1);
  }, []);
  const bumpStats = useCallback(() => bump("stats"), [bump]);

  useEffect(() => {
    setBeat(0);
    setLastBeat(null);
    setBeatSource(null);
    if (!activeTabId) return;
    let dead = false;
    let un: (() => void) | null = null;
    void onTermPulse(activeTabId, () => {
      if (!dead) bump("keepalive");
    }).then((f) => (dead ? f() : (un = f)));
    return () => {
      dead = true;
      un?.();
    };
  }, [activeTabId, bump]);

  const openMenu = (e: React.MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    const W = 180;
    const H = 72;
    setMenu({
      x: Math.min(e.clientX, window.innerWidth - W - 6),
      y: Math.max(6, Math.min(e.clientY - H, window.innerHeight - H - 6)),
    });
  };

  useEffect(() => {
    if (!menu) return;
    const close = () => setMenu(null);
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && close();
    window.addEventListener("keydown", onKey);
    window.addEventListener("blur", close);
    return () => {
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("blur", close);
    };
  }, [menu]);

  const live = prefs.liveMonitor;

  return (
    <footer className={`statusbar${live ? "" : " no-anim"}`}>
      <span className={`sb-status sb-${status}`}>
        {status === "reconnecting" && live ? <span className="spinner sm" /> : <span className="sb-dot" />}
        {LABEL[status]}
      </span>
      {status === "connected" && <Ecg beat={beat} lastBeat={lastBeat} source={beatSource} />}
      <span className="sb-item">{peer ?? "—"}</span>
      {statsTarget && (
        <PublicIp
          // 同一主机（含本机）的标签共享：切换标签不重查
          key={`ip:${statsTarget.hostKey}`}
          hostKey={statsTarget.hostKey}
          tabId={statsTarget.tabId}
          local={statsTarget.local}
          sessionId={statsTarget.sessionId}
          onNotify={onNotify}
        />
      )}
      {prefs.showResources && statsTarget && (
        <ResourceMeters
          // 同一主机（含本机）的标签共享组件与数据：切换时不重采、不闪烁
          key={statsTarget.hostKey}
          hostKey={statsTarget.hostKey}
          tabId={statsTarget.tabId}
          local={statsTarget.local}
          sessionId={statsTarget.sessionId}
          live={live}
          onContextMenu={openMenu}
          onBeat={bumpStats}
        />
      )}
      <span className="sb-spacer" />
      {activeTabId && (
        <span
          className="sb-item sb-count"
          title={
            t(
              "当前终端累计输出 {n} 个字符（{size}）\n导出时仅包含缓冲区内的内容（最多 10000 行滚动历史），实际文件可能更小",
              "This terminal has output {n} characters in total ({size})\nExports only include the buffer (up to 10,000 lines of scrollback), so the file may be smaller",
              { n: out.chars.toLocaleString(), size: fmtSize(out.bytes) }
            )
          }
        >
          {t("字符", "Chars")} {fmtCount(out.chars)}
        </span>
      )}
      {activeTabId && (
        <button
          type="button"
          className="sb-item sb-id"
          title={t(
            "当前会话 id（与 Agent 对话里引用的 target 是同一个值）点击复制",
            "Current session id (the same value the Agent chat uses as target). Click to copy"
          )}
          onClick={() => {
            api
              .clipboardWrite(activeTabId)
              .then(() => onNotify?.(t("已复制会话 id", "Copied session id")))
              .catch((e) => onNotify?.(t("复制失败：{e}", "Copy failed: {e}", { e: String(e) })));
          }}
        >
          ID {activeTabId}
        </button>
      )}
      <span
        className={`sb-lock${locks?.num ? " on" : ""}`}
        title={
          locks
            ? `NumLock ${locks.num ? t("已开启", "on") : t("已关闭", "off")}`
            : t("按任意键后显示 NumLock 状态", "Press any key to show NumLock state")
        }
      >
        NUM
      </span>
      <span
        className={`sb-lock${locks?.cap ? " on" : ""}`}
        title={
          locks
            ? `CapsLock ${locks.cap ? t("已开启（大写）", "on (uppercase)") : t("已关闭（小写）", "off (lowercase)")}`
            : t("按任意键后显示 CapsLock 状态", "Press any key to show CapsLock state")
        }
      >
        CAP
      </span>
      <span
        className="sb-item"
        title={t("已打开 {n} 个标签，其中 {c} 个在线", "{n} tab(s) open, {c} online", { n: sessionCount, c: connectedCount })}
      >
        {t("{n} 会话", "{n} sessions", { n: sessionCount })}
        {connectedCount !== sessionCount ? t("（{c} 在线）", " ({c} online)", { c: connectedCount }) : ""}
      </span>

      <div className="sb-settings" ref={settingsMenuRef}>
        <button
          className={`sb-settings-btn${settingsOpen || settingsMenuOpen ? " active" : ""}`}
          onClick={() => setSettingsMenuOpen((v) => !v)}
          title={t("设置", "Settings")}
          aria-haspopup="menu"
          aria-expanded={settingsMenuOpen}
        >
          ⚙
        </button>
        {settingsMenuOpen && (
          <div
            className="ctx-menu sb-settings-menu"
            role="menu"
            data-tauri-drag-region="false"
          >
            <button className="ctx-item" role="menuitem" onClick={() => pickSettings("appearance")}>
              {t("外观…", "Appearance…")}
              <span className="ctx-shortcut">{t("配色 · 字体 · 底部栏", "Colors · font · status bar")}</span>
            </button>
            <button className="ctx-item" role="menuitem" onClick={() => pickSettings("behavior")}>
              {t("行为…", "Behavior…")}
              <span className="ctx-shortcut">{t("终端行为 · 宏", "Terminal behavior · macros")}</span>
            </button>
            <button className="ctx-item" role="menuitem" onClick={() => pickSettings("shortcuts")}>
              {t("快捷键…", "Shortcuts…")}
            </button>
            <button className="ctx-item" role="menuitem" onClick={() => pickSettings("keys")}>
              {t("密钥管理…", "Key manager…")}
              <span className="ctx-shortcut">{t("生成 · 导入 · 导出", "Generate · import · export")}</span>
            </button>
            <button className="ctx-item" role="menuitem" onClick={() => pickSettings("agent")}>
              Nexus AI Agent…
              <span className="ctx-shortcut">{t("把会话输出旁路给模型", "Share session output with the model")}</span>
            </button>
            <button className="ctx-item" role="menuitem" onClick={() => pickSettings("security")}>
              {t("安全…", "Security…")}
              <span className="ctx-shortcut">{t("主密码 · 凭据加密", "Master password · credential encryption")}</span>
            </button>
          </div>
        )}
      </div>

      {menu && (
        <>
          <div
            className="ctx-backdrop"
            onMouseDown={() => setMenu(null)}
            onContextMenu={(e) => {
              e.preventDefault();
              setMenu(null);
            }}
          />
          <div className="ctx-menu sb-menu" style={{ left: menu.x, top: menu.y }}>
            <button
              className="ctx-item"
              onClick={() => {
                setPrefs({ liveMonitor: !live });
                setMenu(null);
              }}
            >
              {live ? t("关闭实时监控", "Turn off live monitoring") : t("开启实时监控", "Turn on live monitoring")}
            </button>
            <button
              className="ctx-item"
              onClick={() => {
                setPrefs({ showResources: false });
                setMenu(null);
              }}
            >
              {t("在底部栏隐藏资源信息", "Hide resource info in the status bar")}
            </button>
          </div>
        </>
      )}
    </footer>
  );
}
