import { api } from "../api";

/**
 * 会话配置的「附加状态」：连接后自动执行的宏 + 兼容模式。
 *
 * 放模块级缓存而非 App state 的原因：① `dial` 需要**同步**读到 compat 才能决定
 * `sidecar` 开关；② 资源轮询（StatusBar）与上下文探测（App）都可能发现「这台机器
 * 不接受并行通道」，共用一份缓存才不会互相覆盖。
 * 丢了只是下次重学，所以 `update` 先改内存、立刻通知 UI，再静默落盘。
 */
export interface SessionExtras {
  /**
   * 旧版「登录后自动输入」的步骤（每行一步）。
   * 已被宏取代，只为**迁移**保留：启动时会转成一条宏并清空。
   */
  loginScript: string;
  /** 旧版是否自动回放。迁移时作为 `macroOn` 的取值 */
  loginScriptOn: boolean;
  /** 连接后自动执行的宏 id（空串 = 不执行） */
  macroId: string;
  /** 是否真的自动执行（关掉但保留选择，方便临时禁用） */
  macroOn: boolean;
  /** 兼容模式：不开旁路通道（堡垒机只放行一条通道） */
  compat: boolean;
}

const EMPTY: SessionExtras = {
  loginScript: "",
  loginScriptOn: true,
  macroId: "",
  macroOn: false,
  compat: false,
};

let cache = new Map<string, SessionExtras>();
/** 进行中的首次加载。并发调用必须共享同一个 Promise：只置 `loaded` 布尔位的话，
    第二个调用会在数据到达前就返回（启动时的旧登录脚本迁移会因此漏掉数据）。 */
let loading: Promise<void> | null = null;
const listeners = new Set<() => void>();

const clone = (e: SessionExtras): SessionExtras => ({ ...e });

function notify() {
  for (const fn of listeners) fn();
}

/** 启动时拉一次全量；之后所有读写都走内存 */
export function loadExtras(): Promise<void> {
  if (loading) return loading;
  loading = (async () => {
    try {
      const all = await api.sessionExtras();
      for (const [id, e] of Object.entries(all ?? {})) {
        if (e) cache.set(id, { ...EMPTY, ...e });
      }
    } catch {
      // 读不到就用空值：功能退化为「每次都重新学」，不影响连接
    }
    notify();
  })();
  return loading;
}

export function getExtras(id: string | null | undefined): SessionExtras {
  if (!id) return EMPTY;
  const e = cache.get(id);
  return e ? clone(e) : clone(EMPTY);
}

/** 改一项或几项，立刻生效并异步落库 */
export function updateExtras(id: string, patch: Partial<SessionExtras>): void {
  const next = { ...getExtras(id), ...patch };
  cache.set(id, next);
  notify();
  api.setSessionExtras(id, next).catch(() => {});
}

export function subscribeExtras(fn: () => void): () => void {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

/**
 * 这个错误是不是「服务端拒绝并行通道」。只认后端的固定文案（`SIDECAR_DENIED`，见
 * ssh/session.rs），不认网络超时之类 —— 临时故障误判成兼容模式会永久丢掉状态栏。
 */
export function isSidecarDenied(err: unknown): boolean {
  const text = err instanceof Error ? err.message : String(err ?? "");
  return text.includes("不支持并行通道");
}

/** 记录一次「这台机器只让开一条通道」，让后续连接直接跳过旁路通道 */
export function markCompat(
  configId: string | null | undefined,
  tabId?: string | null
): boolean {
  // 当前这条连接还在跑：先把它身上的开关关掉，否则下一次 probe 还会再试一次
  // （多数堡垒机在第二条通道被拒时就把整条连接掀了）
  if (tabId) api.sshSetSidecar(tabId, false).catch(() => {});
  if (!configId) return false;
  if (getExtras(configId).compat) return false;
  updateExtras(configId, { compat: true });
  return true;
}
