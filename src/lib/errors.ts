/** 与 Rust `errors::RAW_SUFFIX` 保持一致：主文案与原始错误之间的分隔符。 */
const RAW_SUFFIX = "\n\n原始错误：";

export interface FriendlyError {
  /** 大字：给用户看的中文提示 */
  message: string;
  /** 小字：原始错误，便于排障 */
  detail?: string;
}

/**
 * 后端已经做过一次翻译，这里只负责拆成两段。
 * 同时对未经翻译的裸 errno 做一次兜底映射 —— 早期版本产生的错误串、
 * 或 SFTP 侧未覆盖的路径都可能直接透传 `os error 60`。
 */
export function toFriendlyError(raw: unknown): FriendlyError {
  const text = String(raw ?? "未知错误").trim();
  if (!text) return { message: "未知错误" };

  const idx = text.indexOf(RAW_SUFFIX);
  if (idx >= 0) {
    return {
      message: text.slice(0, idx).trim() || "连接失败",
      detail: text.slice(idx + RAW_SUFFIX.length).trim() || undefined,
    };
  }

  const fallback = fallbackFromRaw(text);
  if (fallback) return { message: fallback, detail: text };
  return { message: text };
}

function fallbackFromRaw(text: string): string | null {
  if (/os error 60|timed? ?out/i.test(text)) {
    return "连接超时：主机无响应。请检查 IP 是否正确、主机是否开机、是否在同一网络、防火墙是否放行 22 端口";
  }
  if (/os error 61|connection refused/i.test(text)) {
    return "连接被拒绝：端口未开放或服务未启动";
  }
  if (/os error 51|os error 65|network is unreachable|no route to host/i.test(text)) {
    return "网络不可达：目标主机不在可达网段内，请检查本机网络与路由";
  }
  if (/authenticat|permission denied \(publickey|access denied/i.test(text)) {
    return "认证失败：用户名、密码或密钥不正确";
  }
  return null;
}

/** 会话里存的超时可能是 null / 0 / 非数字，统一收敛成合法秒数。 */
export function normalizeTimeout(seconds: number | null | undefined): number {
  const n = Number(seconds);
  if (!Number.isFinite(n) || n <= 0) return 10;
  return Math.min(Math.max(Math.round(n), 1), 600);
}
