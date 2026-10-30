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

/**
 * 连接的几样秘密，和 `NewSession` 分开传。
 *
 * 合成一个对象而不是继续往 `onSave` 后面挂位置参数：三样里有两样是
 * `string | null` 且 null 另有含义，排成一串位置参数时传反了编译器也看不出来。
 */
export interface SessionSecrets {
  password: string;
  /** 私钥口令；null = 用户没动过，保持库里原值 */
  passphrase: string | null;
  /** TOTP 种子；null = 用户没动过，保持库里原值；空串 = 清除 */
  totpSeed: string | null;
}

/** `totp_preview` 的结果：此刻算出的码 + 它还剩几秒 */
export interface TotpPreview {
  code: string;
  remainingSecs: number;
}

/**
 * 宏的一步。字段名与 Rust 侧 serde 一致（`kind` 是判别式）。
 * 刻意只留三种原语：复杂流程应该写脚本，而不是把宏做成一个小语言。
 */
export type MacroStep =
  /** 发送文本；`enter` 为真时末尾补回车 */
  | { kind: "send"; text: string; enter: boolean }
  /**
   * 等屏幕出现 `pattern`（子串；`/.../` 或 `/.../i` 按正则）。
   * `pattern` 为空串表示「等输出静默」—— 不依赖具体提示符，适合菜单类界面。
   */
  | { kind: "wait"; pattern: string; timeoutMs: number }
  /** 无条件等着 */
  | { kind: "sleep"; ms: number };

export interface MacroRecord {
  id: string;
  name: string;
  steps: MacroStep[];
  createdAt: number;
  updatedAt: number;
}

export type AuthMethod =
  | { kind: "password"; password: string }
  | { kind: "privateKey"; path: string; passphrase?: string }
  | { kind: "agent" };

export interface ConnectRequest {
  host: string;
  port?: number;
  username: string;
  /** 登录凭据。SFTP 面板复用已认证的终端连接时可省略（本地没存密码也能开文件面板）。 */
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
   * 是否允许在该连接上开旁路通道（cwd/tmux 探测、状态栏资源采集）。默认 true。
   *
   * 堡垒机常把一条 SSH 连接当成一条终端，收到第二个 `CHANNEL_OPEN` 就断开
   * 客户端（表现为「连上几秒就掉线」）；这类会话传 false（「兼容模式」）。
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
  /** 这个标签不在前台时收到过新输出（tmux/docker 等后台任务常见）——
   *  标签栏据此显示提醒动效；切到它身上就清掉。 */
  hasActivity?: boolean;
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
 * 行为字段（系统提示词、脱敏）刻意也随账号走，换账号时一并切换。
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
 * Agent 的执行模式。后端 `ExecMode` 的镜像（lowercase）。
 * - `agentic`：全自动，判定出来的动作直接下发到终端；
 * - `approval`：人在回路，`danger` 一栏判为危险的命令先弹出来等人点头。
 */
export type AgentExecMode = "agentic" | "approval";

/**
 * AI 助手配置。后端持久化在 SQLite settings 表（key=`agent.config`），
 * serde `rename_all = "camelCase"`，故字段名必须两侧一致。
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
  /** 下令方式。后端对未知字符串降级到 approval（最安全的那一种）。 */
  execMode: AgentExecMode;
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
  /** Agentic 模式下这一步的过程信息（下令 / 结果 / 被跳过 / 提示） */
  step?: AgentStep;
  /** Agentic 审批模式下的待批请求；这张卡片自带「允许 / 拒绝」按钮 */
  approve?: AgentApproveRequest;
  /** 审批结果：true 已允许 / false 已拒绝 / undefined 还在等人 */
  approveResult?: boolean;
}

// ———————————————————————————— Agentic（会动手的那种） ————————————————————————————

/** 一个可操作目标。label 由前端给：后端只认 uuid，把 uuid 贴在进程边上毫无意义。 */
export interface AgentTarget {
  id: string;
  label: string;
}

/** `agent_run` 的入参 */
export interface AgentRunRequest {
  /** 面板当前绑定的会话，同时是模型省略 target 时的默认目标 */
  sessionId: string;
  question: string;
  /** 允许操作的会话。留空时后端退回 [sessionId]。 */
  targets: AgentTarget[];
  /**
   * 由前端从 xterm 缓冲区抓的当前屏幕快照（含 scrollback）。
   *
   * 旁路缓冲只记「开了之后」的字节，可能缺开头或被容量截断，所以后端
   * 优先用这份快照当上下文，旁路缓冲只作兜底。
   */
  consoleText?: string;
  /**
   * 该会话当前是否停在 alternate buffer（vim / less / top / tmux / docker attach 等）。
   * 后端据此提示模型：这类程序的输出抓不到，要用 capture-pane / docker logs 之类的 skill。
   */
  consoleAlt?: boolean;
}

/**
 * `nx://agent/step/{sessionId}` 的载荷：一轮里发生的每一件事。
 *
 * 刻意是「扁平结构 + kind」而不是判别联合：多数字段本就可选，渲染时不用做类型收窄。
 */
export interface AgentStep {
  /** plan | tool | exec | result | skip | note | answer | open | export | tab | waiting */
  kind: string;
  step: number;
  /** kind:"tool" 专用：模型调用的工具名（原始，如 nx_tab） */
  tool?: string;
  /** kind:"tool" 专用：给人看的中文工具名 */
  toolLabel?: string;
  /** kind:"tool" 专用：原始参数 JSON 文本 */
  toolArgs?: string;
  target?: string;
  targetLabel?: string;
  command?: string;
  output?: string;
  text?: string;
  reasons?: string[];
  /** kind:"waiting" 专用：命令发出去之后已经等了多少秒，还没收到结果 */
  elapsedSecs?: number;
}

/** `nx://agent/approve/{sessionId}` 的载荷：请人点头 */
export interface AgentApproveRequest {
  requestId: string;
  command: string;
  target: string;
  targetLabel: string;
  reasons: string[];
}

/**
 * `nx://agent/tab/{sessionId}` 的载荷：请前端动一下标签页。
 *
 * 后端没有 tab 概念（只知道 pty / ssh 连接），所以这是「转达」不是「执行」：
 * 办完必须回一句 `agentTabAck`，否则后端只能当成没办成。
 */
export interface AgentTabRequest {
  requestId: string;
  /** 发起这次操作的会话，只为定位来源 */
  sessionId: string;
  /** list | focus | close | rename | reconnect */
  op: string;
  /** 中文说法，直接可以拿去展示 */
  opLabel: string;
  target?: string;
  name?: string;
}

/** 一个标签页的对外视图（回执里的清单） */
export interface AgentTabInfo {
  id: string;
  title: string;
  /** local | ssh */
  kind: string;
  status: string;
  active: boolean;
}

/** `agent_run` 在 UI 上的两种叫法 */
export type AgentRunMode = "ask" | "act";

/** `agent_exec` 的返回值。reason 是命令结束的方式——没有退出码，
 *  "confirmed" 是活体确认过「真的没有前台进程了」，"silence" 是静默兜底（问不出确定答案）。 */
export interface AgentExecResult {
  output: string;
  reason: "confirmed" | "silence" | "timeout" | "cancelled";
}

/**
 * 保险库状态。「已记住」不是第三种状态 —— 它只是「锁定 → 已解锁」的一条自动边，
 * 要单独问 `api.vaultIsRemembered()`。
 */
export interface VaultStatus {
  enabled: boolean;
  unlocked: boolean;
}

/**
 * 一次启用 / 关闭扫描动了些什么。要如实报给用户：`seedsUnreadable > 0` 意味着
 * 那几个 TOTP 种子是别的机器写的、本机解不开，用户必须重新填一遍。
 */
export interface SweepReport {
  credentials: number;
  seedsConverted: number;
  seedsUnreadable: number;
  apiKeys: number;
}

