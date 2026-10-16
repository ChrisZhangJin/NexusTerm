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
  status: "connecting" | "connected" | "closed" | "error";
  error?: string;
  /** true = 本机 PTY（本地 shell），false = 远端 SSH 会话 */
  local?: boolean;
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
