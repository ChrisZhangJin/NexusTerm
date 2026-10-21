import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  AgentBufferStats,
  AgentConfig,
  AgentDonePayload,
  ClosedInfo,
  ConnectOutcome,
  ConnectRequest,
  DiscoveredKey,
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
  /** 探测远端交互 shell 的 cwd / 前台进程 / tmux（KEY→VALUE） */
  sshProbe: (id: string) => invoke<Record<string, string>>("ssh_probe", { id }),
  sshStats: (id: string) => invoke<Record<string, string>>("ssh_stats", { id }),
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

  // ——— AI Agent 旁路（终端输出旁路喂给 LLM） ———

  agentGetConfig: () => invoke<AgentConfig>("agent_get_config"),
  agentSetConfig: (config: AgentConfig) =>
    invoke<void>("agent_set_config", { config }),
  /** 开关某个会话的旁路 */
  agentSetSession: (sessionId: string, enabled: boolean) =>
    invoke<void>("agent_set_session", { sessionId, enabled }),
  agentGetSession: (sessionId: string) =>
    invoke<boolean>("agent_get_session", { sessionId }),
  /**
   * 取该会话最近的上下文。
   * `maxBytes` 省略（或 0）时后端回落到配置里的 maxContextBytes。
   * 返回值是**未脱敏**原文 —— 脱敏只在真正出网前做，便于界面如实展示。
   */
  agentContext: (sessionId: string, maxBytes?: number) =>
    invoke<string>("agent_context", { sessionId, maxBytes: maxBytes ?? null }),
  /** 清空缓冲（让 agent 忘掉刚才的事） */
  agentClear: (sessionId: string) => invoke<void>("agent_clear", { sessionId }),
  /** 会话没登记过旁路时返回 null，而不是抛错 */
  agentStats: (sessionId: string) =>
    invoke<AgentBufferStats | null>("agent_stats", { sessionId }),
  /**
   * 提问。**resolve 只代表流已启动，不代表回答完成** ——
   * 答案要走 `onAgentToken` / `onAgentDone` / `onAgentError` 事件收。
   */
  agentAsk: (sessionId: string, question: string) =>
    invoke<void>("agent_ask", { sessionId, question }),
  agentCancel: (sessionId: string) => invoke<void>("agent_cancel", { sessionId }),
  /** 用「还没保存」的那份配置测连通性（刻意不读库里的值） */
  agentTest: (config: AgentConfig) => invoke<string>("agent_test", { config }),

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

// ——— AI Agent 流式事件 ———
//
// 后端契约：error 是终态（其后不再发 done），done 一定会发
// （流自然结束却没收到结束标记时兜底为 stop）。
// 所以收尾只认事件，**不要靠超时**。

/** agent 答案的增量文本块 */
export async function onAgentToken(
  id: string,
  handler: (text: string) => void
): Promise<UnlistenFn> {
  return listen<string>(`nx://agent/token/${id}`, (ev) => handler(ev.payload));
}

/** 本次流结束（含 reason：stop / length / cancelled） */
export async function onAgentDone(
  id: string,
  handler: (p: AgentDonePayload) => void
): Promise<UnlistenFn> {
  return listen<AgentDonePayload>(`nx://agent/done/${id}`, (ev) => handler(ev.payload));
}

/** 终态错误，payload 是后端翻译好的中文文案 */
export async function onAgentError(
  id: string,
  handler: (msg: string) => void
): Promise<UnlistenFn> {
  return listen<string>(`nx://agent/error/${id}`, (ev) => handler(ev.payload));
}
