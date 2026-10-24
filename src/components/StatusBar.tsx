import { useCallback, useEffect, useRef, useState } from "react";
import { api, onTermPulse } from "../api";
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
  onOpenSettings: (which: "appearance" | "behavior" | "shortcuts" | "keys" | "agent") => void;
  /** 有设置窗口打开时高亮齿轮 */
  settingsOpen: boolean;
}

const LABEL: Record<Props["status"], string> = {
  idle: "未连接",
  connecting: "连接中",
  connected: "已连接",
  reconnecting: "重连中",
  error: "连接失败",
};

/** 资源刷新间隔 */
const STATS_INTERVAL = 5000;

/** 心电波形（两次心跳之间的平线 + 一次 QRS 脉冲） */
const ECG_PATH = "M0 8h9l3-4 3 8 3-4h7l3-5 3 10 3-5h13";

/** 这次脉搏是从哪来的：服务端 keepalive，还是「一次成功的资源采集」 */
type BeatSource = "keepalive" | "stats";

/**
 * 链路脉搏指示。
 *
 * 两种来源都算「链路活着」：
 * ① 服务端 **真正发来** 的 `keepalive@openssh.com`（vendor 补丁 7）；
 * ② 一次成功的资源采集 —— 能取回 CPU/内存/磁盘，说明 exec 通道往返通了，
 *    对不发 keepalive 的服务器（OpenSSH 默认 `ClientAliveInterval 0`）这是唯一的证据。
 *
 * 两条都没有就只画一条暗基线：不假装链路有心跳。
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
          ? "等待链路脉搏：服务端心跳，或一次成功的资源采集"
          : `链路脉搏：${age} 秒前 · ${source === "stats" ? "资源采集回应" : "服务端心跳"}`
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

/**
 * 按主机共享的最近一次采集结果（key = user@host:port，本机为 "local"）。
 * 同一主机的多个标签共用：切回时立刻显示上次数值，不再闪「采集资源…」；
 * 轮询只由当前激活标签发起，远端负载不随标签数增加。
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
    // 关闭实时监控：停止轮询、数值定格在最后一次。
    // 若这台主机还从没采过（暂停状态下新开的连接），补采一次静态值，避免一直空着
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

  const where = local ? "本机" : "远端";

  /**
   * 重新检测「是否真的只允许一条通道」。
   *
   * 兼容模式一旦被记住，连接时就不开旁路通道，于是**永远不会再试探**，
   * 标记会自我固化 —— 用户看到的「兼容模式」可能是很久以前（甚至通道号
   * 错位那个 bug 期间）留下的误判。这里给一个显式出口：
   * 只对当前连接打开旁路，再真取一次数据，拿到就说明放行第二条通道。
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
            ? "该服务器曾被判定只允许一条 SSH 通道（堡垒机），此设置已记住，所以不采集远端资源。\n" +
              "这是历史记录，不是算法降级 —— 点此重新检测：能取到数据就自动解除。\n" +
              "（若它确实会掐断第二条通道，检测可能让当前连接断开）"
            : "右键可关闭实时监控"
        }
      >
        {compatNow ? (
          <button type="button" className="sb-compat" disabled={retrying} onClick={retry}>
            {retrying ? "检测通道…" : note === "denied" ? "仅单通道 · 已确认" : "兼容模式 · 重检"}
          </button>
        ) : failed ? (
          "资源信息不可用"
        ) : (
          <><span className="spinner sm" /> 采集资源…</>
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
      title={live ? undefined : `${where} · 实时监控已关闭，显示的是最后一次数值（右键开启）`}
    >
      {live && (
        <span
          key={pulse}
          className="sb-res-dot"
          title={`${where} · ${failed ? "最近一次刷新失败" : "每 5 秒刷新"}（右键可关闭实时监控）`}
        />
      )}
      {local && <span className="sb-res-where">本机</span>}
      <Meter
        label="CPU"
        pct={stats.cpuPct}
        text={stats.cpus ? `${stats.cpus} 核` : ""}
        title={
          `CPU：${stats.cpus ?? "?"} 核，当前占用 ${stats.cpuPct?.toFixed(1) ?? "?"}%` +
          (stats.load ? `\n平均负载（1/5/15 分钟）：${stats.load}` : "")
        }
        pulse={pulse}
        animate={live}
      />
      <Meter
        label="内存"
        pct={memPct}
        text={
          stats.memTotal !== undefined && stats.memAvail !== undefined
            ? `可用 ${fmtBytes(stats.memAvail)} / ${fmtBytes(stats.memTotal)}`
            : ""
        }
        title={
          stats.memTotal !== undefined && stats.memAvail !== undefined
            ? `内存：总量 ${fmtBytes(stats.memTotal)}，可用 ${fmtBytes(stats.memAvail)}，已用 ${fmtBytes(stats.memTotal - stats.memAvail)}`
            : "内存信息不可用"
        }
        pulse={pulse}
        animate={live}
      />
      <Meter
        label="磁盘"
        pct={diskPct}
        text={
          stats.diskTotal !== undefined && stats.diskUsed !== undefined
            ? `${fmtBytes(stats.diskUsed)} / ${fmtBytes(stats.diskTotal)}`
            : ""
        }
        title={
          stats.diskTotal !== undefined && stats.diskUsed !== undefined
            ? `磁盘（${stats.diskMount ?? "/"}）：总量 ${fmtBytes(stats.diskTotal)}，已用 ${fmtBytes(stats.diskUsed)}`
            : "磁盘信息不可用"
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

  const pickSettings = (w: "appearance" | "behavior" | "shortcuts" | "keys" | "agent") => {
    setSettingsMenuOpen(false);
    onOpenSettings(w);
  };

  // 链路脉搏：服务端 keepalive（后端事件）或一次成功的资源采集，两者都算。
  // 只订阅当前标签：切走时清空状态，不给后台标签白跑动效。
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
            `当前终端累计输出 ${out.chars.toLocaleString()} 个字符（${fmtSize(out.bytes)}）\n` +
            `导出时仅包含缓冲区内的内容（最多 10000 行滚动历史），实际文件可能更小`
          }
        >
          字符 {fmtCount(out.chars)}
        </span>
      )}
      {activeTabId && (
        <button
          type="button"
          className="sb-item sb-id"
          title="当前会话 id（与 Agent 对话里引用的 target 是同一个值）点击复制"
          onClick={() => {
            api
              .clipboardWrite(activeTabId)
              .then(() => onNotify?.("已复制会话 id"))
              .catch((e) => onNotify?.(`复制失败：${String(e)}`));
          }}
        >
          ID {activeTabId}
        </button>
      )}
      <span
        className={`sb-lock${locks?.num ? " on" : ""}`}
        title={locks ? `NumLock ${locks.num ? "已开启" : "已关闭"}` : "按任意键后显示 NumLock 状态"}
      >
        NUM
      </span>
      <span
        className={`sb-lock${locks?.cap ? " on" : ""}`}
        title={locks ? `CapsLock ${locks.cap ? "已开启（大写）" : "已关闭（小写）"}` : "按任意键后显示 CapsLock 状态"}
      >
        CAP
      </span>
      <span className="sb-item" title={`已打开 ${sessionCount} 个标签，其中 ${connectedCount} 个在线`}>
        {sessionCount} 会话
        {connectedCount !== sessionCount ? `（${connectedCount} 在线）` : ""}
      </span>

      <div className="sb-settings" ref={settingsMenuRef}>
        <button
          className={`sb-settings-btn${settingsOpen || settingsMenuOpen ? " active" : ""}`}
          onClick={() => setSettingsMenuOpen((v) => !v)}
          title="设置"
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
              外观…
              <span className="ctx-shortcut">配色 · 字体 · 底部栏</span>
            </button>
            <button className="ctx-item" role="menuitem" onClick={() => pickSettings("behavior")}>
              行为…
              <span className="ctx-shortcut">终端行为 · 宏</span>
            </button>
            <button className="ctx-item" role="menuitem" onClick={() => pickSettings("shortcuts")}>
              快捷键…
            </button>
            <button className="ctx-item" role="menuitem" onClick={() => pickSettings("keys")}>
              密钥管理…
              <span className="ctx-shortcut">生成 · 导入 · 导出</span>
            </button>
            <button className="ctx-item" role="menuitem" onClick={() => pickSettings("agent")}>
              Nexus AI Agent…
              <span className="ctx-shortcut">把会话输出旁路给模型</span>
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
              {live ? "关闭实时监控" : "开启实时监控"}
            </button>
            <button
              className="ctx-item"
              onClick={() => {
                setPrefs({ showResources: false });
                setMenu(null);
              }}
            >
              在底部栏隐藏资源信息
            </button>
          </div>
        </>
      )}
    </footer>
  );
}
