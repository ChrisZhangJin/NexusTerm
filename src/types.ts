/** 会话持久化记录 */
export interface SessionRecord {
  id: string;
  name: string;
  folder: string;
  host: string;
  port: number;
  username: string;
  authType: "password" | "key" | "agent";
  keyPath: string | null;
  startupCmd: string | null;
  theme: string | null;
  /** TCP 连接超时（秒）。null 表示沿用后端默认值。 */
  connectTimeout: number | null;
  sortOrder: number;
  createdAt: number;
  updatedAt: number;
}

export interface NewSession {
  name: string;
  folder?: string;
  host: string;
  port?: number;
  username: string;
  authType: string;
  keyPath?: string;
  startupCmd?: string;
  theme?: string;
  connectTimeout?: number | null;
}

export type AuthMethod =
  | { kind: "password"; password: string }
  | { kind: "privateKey"; path: string; passphrase?: string }
  | { kind: "agent" };

export interface ConnectRequest {
  host: string;
  port?: number;
  username: string;
  auth: AuthMethod;
  cols: number;
  rows: number;
  /** TCP 连接超时（秒）。缺省由后端取 10 秒。 */
  connectTimeoutSecs?: number;
}

export interface DiscoveredKey {
  name: string;
  path: string;
  keyType: string;
  fingerprint: string | null;
  encrypted: boolean;
}

/** 一个终端页签：持有 SSH 会话 id 与标题 */
export interface TerminalTab {
  id: string;
  title: string;
  sessionId: string | null;
  status: "connecting" | "connected" | "reconnecting" | "closed" | "error";
  error?: string;
  /** true = 本机 PTY（本地 shell），false = 远端 SSH 会话 */
  local?: boolean;
  /** 用户手动改的标签名；有值时优先于 title 显示 */
  customTitle?: string;
  /** 自动重连：当前第几次 / 总次数 */
  reconnectAttempt?: number;
  reconnectMax?: number;
  /** 下一次重连的时间戳（ms），用于倒计时 */
  reconnectAt?: number;
  /** 断开时刻（ms），用于横幅显示 */
  closedAt?: number;
  /** 最近一次探测到的远端上下文（cwd / tmux / docker），断线后仍保留用于恢复 */
  ctx?: import("./lib/context").TermContext;
  /** 最近一次上下文恢复的说明卡片（终端里不回显命令，改由卡片告知） */
  notice?: {
    mode: "reconnect" | "duplicate";
    rows: [string, string][];
    /** false = 没等到提示符或写入失败，未执行恢复 */
    ok: boolean;
    at: number;
  };
}

/** 会话结束原因（后端 ClosedInfo 的镜像） */
export interface ClosedInfo {
  code: number;
  /** exit = shell 正常退出；eof = 远端关闭；lost = 连接丢失；user = 本地主动关闭 */
  reason: "exit" | "eof" | "lost" | "user";
}

/** 远端/本地统一的目录项。后端 `RemoteEntry` 的镜像。 */
export interface RemoteEntry {
  name: string;
  path: string;
  isDir: boolean;
  size: number;
  mtime: number | null;
  permissions: number | null;
}

/** 主题：字段形状对齐 xterm ITheme，方便直接展开 */
export interface Theme {
  name: string;
  foreground: string;
  background: string;
  cursor: string;
  cursorAccent?: string;
  selectionBackground?: string;
  black: string;
  red: string;
  green: string;
  yellow: string;
  blue: string;
  magenta: string;
  cyan: string;
  white: string;
  brightBlack: string;
  brightRed: string;
  brightGreen: string;
  brightYellow: string;
  brightBlue: string;
  brightMagenta: string;
  brightCyan: string;
  brightWhite: string;
}

// ———————————————————————————— AI Agent（会话旁路） ————————————————————————————

/** 后端 `AgentConfig.provider` 的取值。注意 Rust 侧是 String，这里收窄成联合类型便于 UI 做下拉。 */
export type AgentProvider = "openai" | "anthropic" | "custom";

/**
 * AI 助手配置。后端持久化在 SQLite settings 表（key=`agent.config`），
 * serde `rename_all = "camelCase"`，故此处的字段名必须与 TS 侧一致。
 */
export interface AgentConfig {
  /** 全局总开关。关着的时候后端旁路完全不动，连一次哈希查找都没有。 */
  enabled: boolean;
  provider: AgentProvider;
  /** OpenAI 兼容根地址；后端会自行去掉尾斜杠再拼 /chat/completions */
  baseUrl: string;
  model: string;
  /** 明文存在 db 里，db 文件权限已锁 0600 —— 不要让 UI 把它打进日志 */
  apiKey: string;
  /** 单次喂给模型的最近字节数（在环形缓冲容量内的二次截取） */
  maxContextBytes: number;
  /** 留空 = 用后端内置提示词（后端会 trim 后判空） */
  systemPrompt: string;
  /** 新会话是否默认开启旁路 */
  autoAttach: boolean;
  /** 出网前是否脱敏。默认 true：关掉意味着把原始终端输出发给第三方 */
  redact: boolean;
}

/** 某个会话当前的旁路缓冲状态（后端 `BufferStats` 的镜像） */
export interface AgentBufferStats {
  bytes: number;
  /** 累计因超出容量被丢掉的字节数 */
  dropped: number;
  /** 曾经超限丢弃过 → 首尾都不完整，模型看不到 session 全貌 */
  truncated: boolean;
}

/** 流结束原因：stop=模型说完，length=撞到 max_tokens 被截断，cancelled=用户取消 */
export type AgentDoneReason = "stop" | "length" | "cancelled";

/** `nx://agent/done/{sessionId}` 的载荷 */
export interface AgentDonePayload {
  reason: AgentDoneReason;
}

/** 聊天面板里的一条消息。纯前端对象，不进 IPC。 */
export interface AgentMessage {
  id: string;
  role: "user" | "assistant";
  text: string;
  ts: number;
  /** 答案还在流式追加中（收到 done/error 后置回 false） */
  streaming?: boolean;
  /** 这条是一次失败的提问（文本是错误文案） */
  error?: boolean;
}
