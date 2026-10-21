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
  /**
   * 登录凭据。SFTP 面板复用已认证的终端连接时不需要，可以省略 ——
   * 那种情况下本地没存密码也能打开文件面板。
   */
  auth?: AuthMethod;
  cols: number;
  rows: number;
  /** TCP 连接超时（秒）。缺省由后端取 10 秒。 */
  connectTimeoutSecs?: number;
  /**
   * 旧服务器兼容开关（如 CentOS 6 的 OpenSSH 5.3）。
   * 不传 = 自动：先按现代算法握手，协商失败再用 SHA-1 的旧 KEX 重试一次；
   * true = 一开始就带上旧算法；false = 只用现代算法、绝不降级。
   */
  legacyCrypto?: boolean;
  /**
   * 这条连接属于哪个会话配置（侧栏那一项，不是标签 id）。
   * 后端拿它做已认证连接登记，SFTP 面板据此在同一连接上开子系统，
   * 避免对 OTP 服务器重复要验证码。
   */
  configId?: string;
  /**
   * 是否允许在该连接上开旁路通道（cwd/tmux 探测、状态栏资源采集）。
   *
   * 默认 true。企业堡垒机常把一条 SSH 连接当成一条终端，收到第二个
   * `CHANNEL_OPEN` 就直接断开客户端 —— 表现是「连上几秒就掉线」。
   * 这类会话传 false（会话的「兼容模式」），终端功能不受影响。
   */
  sidecar?: boolean;
}

/** 连接成功后的协商结果。 */
export interface ConnectOutcome {
  /** 本次是否用了 SHA-1 系列的旧密钥交换算法 */
  legacyCrypto: boolean;
}

/** 二次验证（keyboard-interactive）的一条待填字段 */
export interface AuthPromptField {
  /** 服务端给的提示语，如 "Verification code:" */
  prompt: string;
  /** false = 秘密输入（密码 / 验证码），true = 明文回显 */
  echo: boolean;
  /** 后端已带入的默认值（目前只有"密码"类问题会自动填已保存的密码），可改 */
  prefill: string | null;
}

/** 后端在认证中途下发的一组问题 —— 二次验证 / OTP / 私钥口令 */
export interface AuthPrompt {
  sessionId: string;
  /**
   * 提问类型：
   * - `keyboard-interactive`：服务端下发的验证码 / 密码挑战
   * - `passphrase`：本地私钥已加密，需要口令
   */
  kind: "keyboard-interactive" | "passphrase";
  host: string;
  /** 登录用户名（私钥口令这类握手前的问题为空） */
  username?: string;
  /** 服务端给的标题，通常为空或 "OTP Code" */
  name: string;
  instructions: string;
  fields: AuthPromptField[];
  /** 第几轮（1 起），多因素时服务端会连问几轮 */
  round: number;
}

export interface DiscoveredKey {
  name: string;
  path: string;
  keyType: string;
  fingerprint: string | null;
  encrypted: boolean;
}

/** 密钥库里的一条密钥（后端 KeyInfo 的镜像） */
export interface KeyInfo {
  /** 库内密钥是 uuid；`~/.ssh` 里的外部密钥是 `file:<路径>` */
  id: string;
  name: string;
  /** vault = 应用密钥库；sshDir = 从 ~/.ssh 扫描到的外部密钥（只读） */
  source: "vault" | "sshDir";
  keyType: string;
  /** 原始算法名，如 ssh-rsa */
  algorithm: string;
  /** 位数，0 = 未知 */
  bits: number;
  comment: string;
  fingerprint: string;
  encrypted: boolean;
  path: string;
  /** 公钥单行文本，可直接贴进 authorized_keys */
  publicKey: string;
  createdAt: number;
}

export interface GenerateKeyOptions {
  /** ed25519 | rsa */
  algorithm: string;
  /** 仅 RSA：2048 / 3072 / 4096 */
  bits?: number;
  comment?: string;
  passphrase?: string;
  name?: string;
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
 * 单个 AI 账号的连接与行为配置。后端 `AgentAccount` 的镜像（camelCase）。
 *
 * 多账号设计：用户可配多组（OpenAI / Anthropic / 自建网关…），整组切换。
 * 连接字段与行为字段都随账号走，这样换账号时系统提示词、脱敏策略一并切换。
 */
export interface AgentAccount {
  /** 账号唯一 id（前端生成）。切换、删除按它定位。 */
  id: string;
  /** 展示名，例如「OpenAI 正式」。空时 UI 退回显示模型名。 */
  name: string;
  provider: AgentProvider;
  /** OpenAI 兼容根地址；后端会去掉尾斜杠与重复的 /chat/completions 再拼。 */
  baseUrl: string;
  model: string;
  /** 明文存在 db 里，db 文件权限已锁 0600 —— 不要让 UI 把它打进日志 */
  apiKey: string;
  /** 单次喂给模型的最近字节数（在环形缓冲容量内的二次截取） */
  maxContextBytes: number;
  /** 留空 = 用后端内置提示词（后端会 trim 后判空） */
  systemPrompt: string;
  /** 出网前是否脱敏。默认 true：关掉意味着把原始终端输出发给第三方 */
  redact: boolean;
}

/**
 * AI 助手配置。后端持久化在 SQLite settings 表（key=`agent.config`），
 * serde `rename_all = "camelCase"`，故此处的字段名必须与 TS 侧一致。
 *
 * 顶层只保留全局行为（总开关、新会话默认旁路），连接相关下沉到 accounts。
 */
export interface AgentConfig {
  /** 全局总开关。关着的时候后端旁路完全不动，连一次哈希查找都没有。 */
  enabled: boolean;
  /** 新会话是否默认开启旁路 */
  autoAttach: boolean;
  /** 所有已配置的账号。至少有一个。 */
  accounts: AgentAccount[];
  /** 当前生效账号的 id。找不到时后端回落到第一个。 */
  activeId: string;
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
