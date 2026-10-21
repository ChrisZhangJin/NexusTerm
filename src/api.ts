import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  AuthPrompt,
  ClosedInfo,
  ConnectOutcome,
  ConnectRequest,
  DiscoveredKey,
  GenerateKeyOptions,
  KeyInfo,
  NewSession,
  RemoteEntry,
  SessionRecord,
} from "./types";

export const api = {
  health: () => invoke<string>("health"),

  sshConnect: (id: string, req: ConnectRequest) =>
    invoke<ConnectOutcome>("ssh_connect", { id, req }),
  sshWrite: (id: string, data: number[]) =>
    invoke<void>("ssh_write", { id, data }),
  /** 二进制大块写入（base64），ZMODEM 上传用 */
  sshWriteB64: (id: string, data: string) => invoke<void>("ssh_write_b64", { id, data }),
  sshResize: (id: string, cols: number, rows: number) =>
    invoke<void>("ssh_resize", { id, cols, rows }),
  sshClose: (id: string) => invoke<void>("ssh_close", { id }),
  /** 回填二次验证答案；answers = null 表示用户取消（后端立刻中止，不再等超时） */
  sshAuthRespond: (id: string, answers: string[] | null) =>
    invoke<boolean>("ssh_auth_respond", { id, answers }),
  /** 探测远端交互 shell 的 cwd / 前台进程 / tmux（KEY→VALUE） */
  sshProbe: (id: string) => invoke<Record<string, string>>("ssh_probe", { id }),
  sshStats: (id: string) => invoke<Record<string, string>>("ssh_stats", { id }),
  /** 运行中切换旁路通道（发现堡垒机拒绝并行通道时关掉） */
  sshSetSidecar: (id: string, on: boolean) =>
    invoke<void>("ssh_set_sidecar", { id, on }),
  /** 所有会话配置的附加状态（登录脚本 / 兼容模式），启动时拉一次 */
  sessionExtras: () =>
    invoke<Record<string, import("./lib/sessionExtras").SessionExtras>>("session_extras"),
  setSessionExtras: (
    id: string,
    extras: import("./lib/sessionExtras").SessionExtras
  ) => invoke<void>("set_session_extras", { id, extras }),
  localStats: () => invoke<Record<string, string>>("local_stats"),
  clipboardRead: () => invoke<string>("clipboard_read"),
  clipboardWrite: (text: string) => invoke<void>("clipboard_write", { text }),

  listSessions: () => invoke<SessionRecord[]>("list_sessions"),
  createSession: (session: NewSession) =>
    invoke<SessionRecord>("create_session", { session }),
  updateSession: (id: string, session: NewSession) =>
    invoke<SessionRecord>("update_session", { id, session }),
  deleteSession: (id: string) => invoke<void>("delete_session", { id }),
  /** 拖拽后批量持久化排序与分组归属：items = [id, sortOrder, folder][] */
  reorderSessions: (items: [string, number, string][]) =>
    invoke<void>("reorder_sessions", { items }),

  savePassword: (id: string, password: string) =>
    invoke<void>("save_password", { id, password }),
  loadPassword: (id: string) => invoke<string | null>("load_password", { id }),

  importXsh: (paths: string[]) =>
    invoke<{ label: string; ok: boolean; error: string | null; passwordImported: boolean }[]>(
      "import_xsh",
      { paths }
    ),
  exportXsh: (id: string, path: string) =>
    invoke<void>("export_xsh", { id, path }),
  exportSessionsXsh: (ids: string[], dir: string) =>
    invoke<number>("export_sessions_xsh", { ids, dir }),
  exportAllXsh: (dir: string) => invoke<number>("export_all_xsh", { dir }),

  listSshKeys: () => invoke<DiscoveredKey[]>("list_ssh_keys"),
  verifyKey: (path: string, passphrase?: string) =>
    invoke<string>("verify_key", { path, passphrase }),

  /** 密钥库 + ~/.ssh 里发现的外部密钥（source 区分） */
  listKeys: () => invoke<KeyInfo[]>("list_keys"),
  importKey: (src: string, name?: string, passphrase?: string) =>
    invoke<KeyInfo>("import_key", { src, name, passphrase }),
  generateKey: (opts: GenerateKeyOptions) => invoke<KeyInfo>("generate_key", { opts }),
  deleteKey: (id: string) => invoke<void>("delete_key", { id }),
  exportKey: (id: string, dest: string) => invoke<void>("export_key", { id, dest }),
  updateKeyMeta: (id: string, name?: string, comment?: string) =>
    invoke<void>("update_key_meta", { id, name, comment }),
  keyPublic: (id: string) => invoke<string>("key_public", { id }),

  savePassphrase: (id: string, passphrase: string) =>
    invoke<void>("save_passphrase", { id, passphrase }),
  loadPassphrase: (id: string) => invoke<string | null>("load_passphrase", { id }),

  listLocalDir: (path: string) =>
    invoke<RemoteEntry[]>("list_local_dir", { path }),

  sftpConnect: (id: string, req: ConnectRequest) =>
    invoke<void>("sftp_connect", { id, req }),
  sftpList: (id: string, path: string) =>
    invoke<RemoteEntry[]>("sftp_list", { id, path }),
  sftpHome: (id: string) => invoke<string>("sftp_home", { id }),
  sftpDownload: (id: string, remote: string, local: string) =>
    invoke<number>("sftp_download", { id, remote, local }),
  sftpUpload: (id: string, local: string, remote: string) =>
    invoke<number>("sftp_upload", { id, local, remote }),
  sftpMkdir: (id: string, path: string) =>
    invoke<void>("sftp_mkdir", { id, path }),
  sftpRemove: (id: string, path: string, isDir: boolean) =>
    invoke<void>("sftp_remove", { id, path, isDir }),
  sftpRename: (id: string, from: string, to: string) =>
    invoke<void>("sftp_rename", { id, from, to }),
  sftpClose: (id: string) => invoke<void>("sftp_close", { id }),

  localSpawn: (id: string, cols: number, rows: number) =>
    invoke<void>("local_spawn", { id, cols, rows }),
  localWrite: (id: string, data: number[]) =>
    invoke<void>("local_write", { id, data }),
  localWriteB64: (id: string, data: string) =>
    invoke<void>("local_write_b64", { id, data }),
  localResize: (id: string, cols: number, rows: number) =>
    invoke<void>("local_resize", { id, cols, rows }),
  localClose: (id: string) => invoke<void>("local_close", { id }),

  /** ZMODEM 下载落盘 */
  writeFile: (path: string, data: number[]) =>
    invoke<void>("write_file", { path, data }),
  /** 大文件落盘：base64 传输，避免 number[] 变成几亿个 JSON 数字 */
  writeFileB64: (path: string, data: string) =>
    invoke<void>("write_file_b64", { path, data }),
  /** 下载流式落盘：每 MB 追加一次，内存不随文件大小增长（truncate 只在第一次） */
  appendFileB64: (path: string, data: string, truncate: boolean) =>
    invoke<void>("append_file_b64", { path, data, truncate }),

  // ——— 阅读窗口（.md / .html 渲染查看） ———
  readerOpenSftp: (sftpId: string, path: string, source: string) =>
    invoke<void>("reader_open_sftp", { sftpId, path, source }),
  readerOpenSsh: (id: string, path: string, source: string) =>
    invoke<void>("reader_open_ssh", { id, path, source }),
  readerOpenLocal: (path: string) => invoke<void>("reader_open_local", { path }),
  readerDoc: (id: string) =>
    invoke<{ name: string; source: string; kind: "markdown" | "html" | "image" | "code"; data: string }>(
      "reader_doc",
      { id }
    ),
};

/** base64 → Uint8Array。SSH 数据流必须走这条路径，不能 lossy 转字符串。 */
export function base64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

/** 字符串 → 后端的 Vec<u8> */
/** 字节 → base64（分块 fromCharCode，避免大数组展开爆栈） */
export function bytesToBase64(bytes: ArrayLike<number>): string {
  let bin = "";
  const CH = 0x8000;
  for (let i = 0; i < bytes.length; i += CH) {
    const end = Math.min(i + CH, bytes.length);
    const part: number[] = new Array(end - i);
    for (let j = i; j < end; j++) part[j - i] = bytes[j];
    bin += String.fromCharCode.apply(null, part);
  }
  return btoa(bin);
}

export function stringToBytes(s: string): number[] {
  return Array.from(new TextEncoder().encode(s));
}

/** 订阅某个会话的远端输出 */
export async function onTermData(
  id: string,
  handler: (bytes: Uint8Array) => void
): Promise<UnlistenFn> {
  return listen<string>(`nx://term/data/${id}`, (ev) => {
    handler(base64ToBytes(ev.payload));
  });
}

/** 本地 shell 的输出（与 SSH 通道同协议） */
export async function onLocalData(
  id: string,
  handler: (bytes: Uint8Array) => void
): Promise<UnlistenFn> {
  return listen<string>(`nx://local/data/${id}`, (ev) => {
    handler(base64ToBytes(ev.payload));
  });
}

/** 本地 shell 退出 */
export async function onLocalClosed(
  id: string,
  handler: (code: number) => void
): Promise<UnlistenFn> {
  return listen<number>(`nx://local/closed/${id}`, (ev) => handler(ev.payload));
}

/** 订阅会话关闭 */
export async function onTermClosed(
  id: string,
  handler: (info: ClosedInfo) => void
): Promise<UnlistenFn> {
  return listen<ClosedInfo | number>(`nx://term/closed/${id}`, (ev) => {
    const p = ev.payload;
    handler(typeof p === "number" ? { code: p, reason: "eof" } : p);
  });
}

/**
 * 订阅二次验证问询。
 *
 * 用全局单通道而不是按会话分段：认证在「连接中」这一刻发生，此时前端还不知道
 * 会不会被问，没法提前为每个标签挂监听。同一时刻只可能有一个连接在等认证，
 * 不会串台。答案由 `sshAuthRespond` 按 `sessionId` 送回。
 */
export async function onAuthPrompt(
  handler: (prompt: AuthPrompt) => void
): Promise<UnlistenFn> {
  return listen<AuthPrompt>("nx://auth/prompt", (ev) => handler(ev.payload));
}
