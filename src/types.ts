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
