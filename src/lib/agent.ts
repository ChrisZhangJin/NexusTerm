/**
 * AI Agent 配置的前端 store。
 *
 * 配置的真身在后端 SQLite（settings 表 key=`agent.config`），这里做的是：
 * 进程内缓存一份 + useSyncExternalStore 广播。
 * 之所以必须走订阅而不是各面板自己 useState 再传 props ——
 * 设置窗口里改完（比如关掉总开关、换模型），终端侧的提问按钮要立刻跟着变，
 * 二者不在一个组件树分支上，层层传 props 会把 App.tsx 撑爆。玩法与 prefs.ts 一致。
 */

import { useSyncExternalStore } from "react";
import { api } from "../api";
import type { AgentConfig, AgentProvider } from "../types";

/** 与后端 `AgentConfig::default()` 对齐。systemPrompt 留空 = 用后端内置提示词。 */
export const DEFAULT_AGENT_CONFIG: AgentConfig = {
  enabled: false,
  provider: "openai",
  baseUrl: "https://api.openai.com/v1",
  model: "gpt-4o-mini",
  apiKey: "",
  maxContextBytes: 32 * 1024,
  systemPrompt: "",
  autoAttach: false,
  redact: true,
};

const PROVIDERS: readonly AgentProvider[] = ["openai", "anthropic", "custom"];

/**
 * IPC 边界上的兜底：Rust 侧 provider 是 String、u32 是定长整数，
 * JSON 能表达的东西比 Rust 出的值宽得多。这里把未知/越界值收敛回合法域，
 * 保证 UI 拿到的对象一定符合 AgentConfig 的类型不变量。
 */
function normalize(raw: Partial<AgentConfig> | null | undefined): AgentConfig {
  const c = { ...DEFAULT_AGENT_CONFIG, ...(raw ?? {}) };
  return {
    enabled: Boolean(c.enabled),
    provider: PROVIDERS.includes(c.provider) ? c.provider : "openai",
    baseUrl: typeof c.baseUrl === "string" ? c.baseUrl : "",
    model: typeof c.model === "string" ? c.model : "",
    apiKey: typeof c.apiKey === "string" ? c.apiKey : "",
    // 0 与非法值回落到默认窗口 —— 后端对 max_context_bytes == 0 是同样的处理
    maxContextBytes: clampContextBytes(c.maxContextBytes),
    systemPrompt: typeof c.systemPrompt === "string" ? c.systemPrompt : "",
    autoAttach: Boolean(c.autoAttach),
    redact: Boolean(c.redact),
  };
}

/** u32 上限：后端形参是 u32，超过会在反序列化时炸掉整个 IPC 调用 */
function clampContextBytes(n: unknown): number {
  const v = Math.round(Number(n));
  if (!Number.isFinite(v) || v <= 0) return DEFAULT_AGENT_CONFIG.maxContextBytes;
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
 * 加载配置。已加载过就直接返回缓存 —— 反复开关聊天面板不该反复打 IPC。
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
 * 配置是否足以发起一次提问。
 * 检查顺序与文案刻意对齐后端 `llm::validate`，免得前后端弹出两句不同的话。
 * 注意这里**不校验** apiKey 的格式：各家 key 形状差别太大，
 * 唯一可信的校验就是打一次模型 —— 那件事交给 `agent_test`。
 */
export function isAgentReady(cfg: AgentConfig): { ok: boolean; reason?: string } {
  if (!cfg.enabled) return { ok: false, reason: "AI 助手未启用：请先在设置里打开总开关" };
  if (!cfg.apiKey.trim()) return { ok: false, reason: "尚未填写 API Key：请到设置里填写后再提问" };
  if (!cfg.model.trim()) return { ok: false, reason: "尚未填写模型名称：请到设置里填写后再提问" };
  if (!cfg.baseUrl.trim()) return { ok: false, reason: "尚未填写接口地址（Base URL）" };
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
