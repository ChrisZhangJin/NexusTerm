import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  ConnectRequest,
  DiscoveredKey,
  NewSession,
  RemoteEntry,
  SessionRecord,
} from "./types";

export const api = {
  health: () => invoke<string>("health"),

  sshConnect: (id: string, req: ConnectRequest) =>
    invoke<void>("ssh_connect", { id, req }),
  sshWrite: (id: string, data: number[]) =>
    invoke<void>("ssh_write", { id, data }),
  sshResize: (id: string, cols: number, rows: number) =>
    invoke<void>("ssh_resize", { id, cols, rows }),
  sshClose: (id: string) => invoke<void>("ssh_close", { id }),

  listSessions: () => invoke<SessionRecord[]>("list_sessions"),
  createSession: (session: NewSession) =>
    invoke<SessionRecord>("create_session", { session }),
  updateSession: (id: string, session: NewSession) =>
    invoke<SessionRecord>("update_session", { id, session }),
  deleteSession: (id: string) => invoke<void>("delete_session", { id }),

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
  exportAllXsh: (dir: string) => invoke<number>("export_all_xsh", { dir }),

  listSshKeys: () => invoke<DiscoveredKey[]>("list_ssh_keys"),
  verifyKey: (path: string, passphrase?: string) =>
    invoke<string>("verify_key", { path, passphrase }),

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
  localResize: (id: string, cols: number, rows: number) =>
    invoke<void>("local_resize", { id, cols, rows }),
  localClose: (id: string) => invoke<void>("local_close", { id }),

  /** ZMODEM 下载落盘 */
  writeFile: (path: string, data: number[]) =>
    invoke<void>("write_file", { path, data }),
};

/** base64 → Uint8Array。SSH 数据流必须走这条路径，不能 lossy 转字符串。 */
export function base64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

/** 字符串 → 后端的 Vec<u8> */
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
  handler: (code: number) => void
): Promise<UnlistenFn> {
  return listen<number>(`nx://term/closed/${id}`, (ev) => handler(ev.payload));
}
