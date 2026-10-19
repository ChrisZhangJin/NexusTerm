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
