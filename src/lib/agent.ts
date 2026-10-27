/**
 * AI Agent 配置的前端 store。
 *
 * 配置的真身在后端 SQLite（settings 表 key=`agent.config`），这里是进程内缓存 +
 * useSyncExternalStore 广播。走订阅而不是传 props：设置窗口与终端侧提问按钮不在
 * 同一组件树分支上，层层传 props 会把 App.tsx 撑爆。玩法与 prefs.ts 一致。
 */

import { useSyncExternalStore } from "react";
import { api } from "../api";
import type { AgentAccount, AgentConfig, AgentProvider } from "../types";

/** 生成一个账号 id：时间戳 + 随机尾巴，够用且不依赖 crypto.randomUUID 的可用性。 */
export function newAccountId(): string {
  return `acct-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;
}

/** 与后端 `AgentAccount::default()` 对齐。systemPrompt 留空 = 用后端内置提示词。 */
export function defaultAccount(id = "default", name = "默认"): AgentAccount {
  return {
    id,
    name,
    provider: "openai",
    baseUrl: "https://api.openai.com/v1",
    model: "gpt-4o-mini",
    apiKey: "",
    maxContextBytes: 32 * 1024,
    systemPrompt: "",
    redact: true,
  };
}

/** 与后端 `AgentConfig::default()` 对齐：单个默认账号 + 人在回路。 */
export const DEFAULT_AGENT_CONFIG: AgentConfig = {
  enabled: false,
  autoAttach: false,
  accounts: [defaultAccount()],
  activeId: "default",
  execMode: "approval",
};

const PROVIDERS: readonly AgentProvider[] = ["openai", "anthropic", "custom"];

/** 规整单个账号，把未知/越界值收敛回合法域。 */
function normalizeAccount(raw: Partial<AgentAccount> | null | undefined, idx: number): AgentAccount {
  const d = defaultAccount();
  const c = { ...d, ...(raw ?? {}) };
  const id = typeof c.id === "string" && c.id.trim() ? c.id : `acct-${idx}`;
  return {
    id,
    name: typeof c.name === "string" ? c.name : "",
    provider: PROVIDERS.includes(c.provider) ? c.provider : "openai",
    baseUrl: typeof c.baseUrl === "string" ? c.baseUrl : "",
    model: typeof c.model === "string" ? c.model : "",
    apiKey: typeof c.apiKey === "string" ? c.apiKey : "",
    // 0 与非法值回落到默认窗口 —— 后端对 max_context_bytes == 0 是同样的处理
    maxContextBytes: clampContextBytes(c.maxContextBytes),
    systemPrompt: typeof c.systemPrompt === "string" ? c.systemPrompt : "",
    redact: Boolean(c.redact),
  };
}

/**
 * IPC 边界上的兜底：把后端来的配置收敛成合法的多账号结构。
 * 账号列表为空时补一个默认账号；activeId 失效时回落到第一个。
 * 兼容旧的扁平配置（后端已做迁移，但前端也容错一次，避免读到残缺对象就崩）。
 */
function normalize(raw: Partial<AgentConfig> | null | undefined): AgentConfig {
  const c = { ...(raw ?? {}) } as Partial<AgentConfig> & Partial<AgentAccount>;
  let accounts: AgentAccount[] = Array.isArray(c.accounts)
    ? c.accounts.map((a, i) => normalizeAccount(a, i))
    : [];
  // 兼容后端万一回了旧扁平结构：把顶层连接字段包成一个账号
  if (accounts.length === 0) {
    if (c.baseUrl !== undefined || c.model !== undefined || c.apiKey !== undefined) {
      accounts = [normalizeAccount({ id: "default", name: "默认", ...c }, 0)];
    } else {
      accounts = [defaultAccount()];
    }
  }
  const activeId =
    typeof c.activeId === "string" && accounts.some((a) => a.id === c.activeId)
      ? c.activeId
      : accounts[0].id;
  return {
    enabled: Boolean(c.enabled),
    autoAttach: Boolean(c.autoAttach),
    accounts,
    activeId,
    // 认不得的值一律收敛到 approval —— 与后端 `ExecMode::parse` 的降级方向一致。
    // 宁可多问一句，也不要默认全自动去动别人的机器。
    execMode: c.execMode === "agentic" ? "agentic" : "approval",
  };
}

/** 取当前生效账号（accounts 恒非空、activeId 恒有效，由 normalize 保证）。 */
export function activeAccount(cfg: AgentConfig): AgentAccount {
  return cfg.accounts.find((a) => a.id === cfg.activeId) ?? cfg.accounts[0];
}

/** 账号的展示名：优先用 name，空则退回模型名，再空则用 id 前缀。 */
export function accountLabel(a: AgentAccount): string {
  if (a.name.trim()) return a.name.trim();
  if (a.model.trim()) return a.model.trim();
  return a.id;
}

/** u32 上限：后端形参是 u32，超过会在反序列化时炸掉整个 IPC 调用 */
function clampContextBytes(n: unknown): number {
  const v = Math.round(Number(n));
  if (!Number.isFinite(v) || v <= 0) return defaultAccount().maxContextBytes;
  return Math.min(v, 0xffffffff);
}

// ——— store ———

let config: AgentConfig = { ...DEFAULT_AGENT_CONFIG };
/** 是否已成功从后端读过一次。区分「后端默认值」与「还没加载」很重要，否则提问按钮会因 undefined 误判 */
let loaded = false;
let inflight: Promise<AgentConfig> | null = null;

const listeners = new Set<() => void>();
const emit = () => listeners.forEach((f) => f());

function setCache(next: AgentConfig) {
  config = next;
  loaded = true;
  emit();
}

/** 非 React 代码读当前配置 */
export const getAgentConfig = (): AgentConfig => config;

/**
 * 加载配置。已加载过就走缓存 —— 反复开关聊天面板不该反复打 IPC。
 * @param force true 时忽略缓存强制回源（后端配置可能被其它窗口改过）
 */
export async function loadAgentConfig(force = false): Promise<AgentConfig> {
  if (loaded && !force) return config;
  // 并发去重：多个面板同时挂载只发一次请求
  if (inflight) return inflight;
  inflight = (async () => {
    try {
      setCache(normalize(await api.agentGetConfig()));
    } catch {
      // 读盘失败时保留内存里的旧值：设置界面正在编辑的内容不能被一次失败冲掉
    } finally {
      inflight = null;
    }
    return config;
  })();
  return inflight;
}

/**
 * 保存配置。
 * 顺序很重要：写盘成功后才更新内存 —— 否则 sqlite 挂了界面却显示「已保存」。
 */
export async function saveAgentConfig(cfg: AgentConfig): Promise<void> {
  const next = normalize(cfg);
  await api.agentSetConfig(next);
  setCache(next);
}

const subscribe = (f: () => void) => {
  listeners.add(f);
  // 订阅即拉取：面板挂载就能拿到后端真值，调用方不必各自写一个 mount effect
  void loadAgentConfig();
  return () => {
    listeners.delete(f);
  };
};

/** 订阅配置变化；首次订阅会自动加载（未加载完时给出 DEFAULT_AGENT_CONFIG） */
export function useAgentConfig(): AgentConfig {
  return useSyncExternalStore(subscribe, getAgentConfig);
}

// ——— 校验 / 格式化 ———

/**
 * 配置是否足以发起一次提问。检查顺序与文案刻意对齐后端 `llm::validate`，免得前后端
 * 弹出两句不同的话。不校验 apiKey 格式：各家形状差别太大，真校验交给 `agent_test`。
 */
export function isAgentReady(cfg: AgentConfig): { ok: boolean; reason?: string } {
  if (!cfg.enabled) return { ok: false, reason: "Nexus AI Agent 未启用：请先在设置里打开总开关" };
  const a = activeAccount(cfg);
  if (!a.apiKey.trim()) return { ok: false, reason: "当前账号尚未填写 API Key：请到设置里填写后再提问" };
  if (!a.model.trim()) return { ok: false, reason: "当前账号尚未填写模型名称：请到设置里填写后再提问" };
  if (!a.baseUrl.trim()) return { ok: false, reason: "当前账号尚未填写接口地址（Base URL）" };
  return { ok: true };
}

/** 缓冲体积的展示文案：不到 1 KB 按字节，往上逐级进位 */
export function formatBytes(n: number): string {
  const v = Number(n);
  if (!Number.isFinite(v) || v <= 0) return "0 B";
  if (v < 1024) return `${Math.round(v)} B`;
  if (v < 1024 * 1024) return `${(v / 1024).toFixed(1)} KB`;
  return `${(v / 1024 / 1024).toFixed(1)} MB`;
}
