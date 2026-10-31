import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  AgentApproveRequest,
  AgentBufferStats,
  AgentConfig,
  AgentDonePayload,
  AgentExecResult,
  AgentRunRequest,
  AgentStep,
  AgentTabInfo,
  AgentTabRequest,
  AuthPrompt,
  ClosedInfo,
  ConnectOutcome,
  ConnectRequest,
  DiscoveredKey,
  GenerateKeyOptions,
  KeyInfo,
  MacroRecord,
  MacroStep,
  NewSession,
  RemoteEntry,
  SessionRecord,
  SweepReport,
  TotpPreview,
  VaultStatus,
} from "./types";

export const api = {
  health: () => invoke<string>("health"),

  sshConnect: (id: string, req: ConnectRequest) =>
    invoke<ConnectOutcome>("ssh_connect", { id, req }),
  /** 连接编辑弹窗的「测试连接」：只验证能不能连上，不开标签、不留痕迹 */
  sshTestConnect: (req: ConnectRequest) =>
    invoke<ConnectOutcome>("ssh_test_connect", { req }),
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
  clipboardRead: (tabId: string) => invoke<string>("clipboard_read", { tabId }),
  clipboardWrite: (text: string) => invoke<void>("clipboard_write", { text }),

  listSessions: () => invoke<SessionRecord[]>("list_sessions"),
  createSession: (session: NewSession) =>
    invoke<SessionRecord>("create_session", { session }),
  updateSession: (id: string, session: NewSession) =>
    invoke<SessionRecord>("update_session", { id, session }),
  deleteSession: (id: string) => invoke<void>("delete_session", { id }),

  /** 宏：持久化在后端，回放由前端执行（要读屏幕、按输出节奏发下一步） */
  listMacros: () => invoke<MacroRecord[]>("list_macros"),
  createMacro: (name: string, steps: MacroStep[]) =>
    invoke<MacroRecord>("create_macro", { name, steps }),
  updateMacro: (id: string, name: string, steps: MacroStep[]) =>
    invoke<MacroRecord>("update_macro", { id, name, steps }),
  deleteMacro: (id: string) => invoke<void>("delete_macro", { id }),

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

  /**
   * 保存 TOTP 种子。接受裸 base32（空格 / 连字符 / 小写都行）或 `otpauth://totp/...` 链接。
   * 传空串 = 清除。种子不合法时后端直接返回中文错误。
   */
  saveTotpSeed: (id: string, seed: string) => invoke<void>("save_totp_seed", { id, seed }),
  /**
   * 这条连接有没有存过种子。
   *
   * 注意这里没有对应的 `loadTotpSeed` —— 种子是第二因子，存进去就不再出后端，
   * 前端只问得到「有没有」。要改只能重填一次。
   */
  hasTotpSeed: (id: string) => invoke<boolean>("has_totp_seed", { id }),
  /** 按用户此刻填的**种子本身**（不是 id）算出当前的码与剩余秒数，供保存前跟手机对一眼 */
  totpPreview: (seed: string) => invoke<TotpPreview>("totp_preview", { seed }),

  listLocalDir: (path: string) =>
    invoke<RemoteEntry[]>("list_local_dir", { path }),

  sftpConnect: (id: string, req: ConnectRequest) =>
    invoke<void>("sftp_connect", { id, req }),
  sftpList: (id: string, path: string) =>
    invoke<RemoteEntry[]>("sftp_list", { id, path }),
  sftpHome: (id: string) => invoke<string>("sftp_home", { id }),
  sftpDownload: (id: string, remote: string, local: string, transferId: string) =>
    invoke<number>("sftp_download", { id, remote, local, transferId }),
  sftpUpload: (id: string, local: string, remote: string, transferId: string) =>
    invoke<number>("sftp_upload", { id, local, remote, transferId }),
  sftpCancelTransfer: (transferId: string) =>
    invoke<void>("sftp_cancel_transfer", { transferId }),
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
  /** 输入诊断日志（排查 mac 输入法）：后端直接写到 `<app_data>/logs/input.log` */
  appendInputLog: (lines: string) => invoke<void>("append_input_log", { lines }),

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

  // ——— Agentic：让 Agent 真的去终端里执行 ———

  /**
   * 发起一轮「会动手」的问答。**resolve 代表这一整轮已经跑完**（含所有执行步骤），
   * 过程信息全部走 `onAgentStep` / `onAgentApprove` / `onAgentError` 事件。
   * 与 `agentAsk` 的分工：这条会真在终端里跑命令。
   */
  agentRun: (request: AgentRunRequest) => invoke<void>("agent_run", { request }),
  /** 中止该会话正在进行的一轮。幂等。 */
  agentRunCancel: (sessionId: string) =>
    invoke<void>("agent_run_cancel", { sessionId }),
  /**
   * 答复一条审批请求。
   * 返回 false 表示这条请求已经没人接了（超时 / 答复晚到）—— 不是错误。
   */
  agentApprove: (requestId: string, approved: boolean) =>
    invoke<boolean>("agent_approve", { requestId, approved }),
  /**
   * 回执一次标签页动作。
   *
   * 后端在等这句话：不回，它就只能当成「没办成」—— 而那个判断会被喂给模型。
   * `tabs` 只在 list 时有意义。返回 false 表示请求已超时被丢弃，不是错误。
   */
  agentTabAck: (
    requestId: string,
    ok: boolean,
    detail?: string,
    tabs?: AgentTabInfo[]
  ) => invoke<boolean>("agent_tab_ack", { requestId, ok, detail, tabs }),
  /**
   * 不经模型、直接在会话里跑一条命令并拿回结果。
   *
   * 这条会给终端**真的下发输入** —— 只有用户点「执行」时才被调用。
   * 与 `agent_run` 共用同一条哨兵执行链路（同一套结束判定与脱敏策略）。
   */
  agentExec: (sessionId: string, command: string) =>
    invoke<AgentExecResult>("agent_exec", { sessionId, command }),
  /**
   * 导出一个会话迄今为止的全部输出（旁路缓冲的完整记录，不受「喂给模型」那个
   * 字节上限约束）。`deep=true` 额外探一遍该会话里的 tmux / docker 现状。
   * 没有可导出的内容时返回空字符串，不算错误。
   */
  agentExportSession: (sessionId: string, deep: boolean) =>
    invoke<string>("agent_export_session", { sessionId, deep }),
  /**
   * 开一个新会话：本地 shell，或连到一个已保存的会话配置（`kind:"ssh"` 时必填
   * `sessionRecordId`），可选顺手跑一条命令。成功后会收到
   * `nx://agent/session-opened` 事件，前端据此接出一个真正的标签页。
   */
  agentOpenSession: (
    originSessionId: string,
    kind: "local" | "ssh",
    sessionRecordId: string | null,
    label: string,
    command: string | null
  ) =>
    invoke<string>("agent_open_session", {
      originSessionId,
      kind,
      sessionRecordId,
      label,
      command,
    }),

  // ——— Console 自动补全：shell 自己的历史文件（只读、只在内存） ———

  /** 本机 shell 的历史，从旧到新。读不到是空数组 */
  historyLoadLocal: () => invoke<string[]>("history_load_local"),
  /** 远端 shell 的历史，从旧到新。兼容模式的连接会 reject（走的是旁路通道） */
  historyLoadRemote: (id: string) => invoke<string[]>("history_load_remote", { id }),

  /** 把 Agent 面板浮出为独立系统窗口（已开则切会话并聚焦） */
  agentOpenWindow: (sessionId: string) =>
    invoke<void>("agent_open_window", { sessionId }),
  /** 关闭浮动 Agent 窗口（幂等） */
  agentCloseWindow: () => invoke<void>("agent_close_window"),
  /** 浮动窗口是否已打开 */
  agentWindowOpen: () => invoke<boolean>("agent_window_open"),

  // ——— 浏览窗口（.md / .html 渲染查看、图片/音频/视频播放、文本高亮） ———
  viewerOpenSftp: (sftpId: string, path: string, source: string) =>
    invoke<void>("viewer_open_sftp", { sftpId, path, source }),
  viewerOpenSsh: (id: string, path: string, source: string) =>
    invoke<void>("viewer_open_ssh", { id, path, source }),
  viewerOpenLocal: (path: string) => invoke<void>("viewer_open_local", { path }),
  viewerDoc: (id: string) =>
    invoke<{
      name: string;
      source: string;
      kind: "markdown" | "html" | "image" | "audio" | "video" | "code";
      data: string;
    }>("viewer_doc", { id }),

  // ——— 主密码保险库 ———
  //
  // 返回 Result<T, E> 的几条（enable / unlock / changePassword / disable / reset）
  // 在 JS 侧表现为 invoke reject，一律 try/catch；错误已经是中文，直接 String(e) 展示。

  /** 廉价：只读内存里的枚举，不做 KDF、不碰系统凭据服务，随便调 */
  vaultStatus: () => invoke<VaultStatus>("vault_status"),
  /**
   * 这个库的钥匙有没有记在本机。
   *
   * **会访问系统凭据服务**，Linux 上没有 D-Bus 时可能卡住 —— 所以它和
   * `vaultStatus` 是分开的两条命令，别把它放在启动关键路径上。
   */
  vaultIsRemembered: () => invoke<boolean>("vault_is_remembered"),
  /** 试着用本机记住的钥匙直接解锁。在**准备弹解锁框之前**调一次：true 就不用弹了 */
  vaultTryRemembered: () => invoke<boolean>("vault_try_remembered"),
  /** 启用保险库：把现有凭据全部改写成密文，返回改了多少条 */
  vaultEnable: (password: string, rememberHere: boolean) =>
    invoke<SweepReport>("vault_enable", { password, rememberHere }),
  vaultUnlock: (password: string, rememberHere: boolean) =>
    invoke<void>("vault_unlock", { password, rememberHere }),
  /** 只丢掉内存里的密钥，不影响「本机记住」—— 那是 `vaultForget` */
  vaultLock: () => invoke<void>("vault_lock"),
  /** 只忘掉「本机记住」，当前这把锁不受影响，下次启动才要重新输主密码 */
  vaultForget: () => invoke<void>("vault_forget"),
  /** 换主密码。只重新包装那把 DEK，库里有多少条凭据都是一瞬间 */
  vaultChangePassword: (oldPassword: string, newPassword: string) =>
    invoke<void>("vault_change_password", { oldPassword, newPassword }),
  /** 关闭保险库，凭据还原成明文。要验主密码 */
  vaultDisable: (password: string) =>
    invoke<SweepReport>("vault_disable", { password }),
};

/** base64 → Uint8Array。SSH 数据流必须走这条路径，不能 lossy 转字符串。 */
export function base64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

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

/** 字符串 → 后端的 Vec<u8> */
export function stringToBytes(s: string): number[] {
  return Array.from(new TextEncoder().encode(s));
}

/** SFTP 上传/下载进度：全局事件，按 payload 里的 transferId 分发给对应的传输行 */
export async function onSftpProgress(
  handler: (p: { transferId: string; transferred: number; total: number }) => void
): Promise<UnlistenFn> {
  return listen<{ transferId: string; transferred: number; total: number }>(
    "nx://sftp/progress",
    (ev) => handler(ev.payload)
  );
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
 * 服务端心跳（`keepalive@openssh.com`）到达。
 *
 * 后端在 russh 处理 keepalive 的分支里发这个事件（vendor 补丁 7）——
 * 是「链路还活着」的正向证据，比任何本地计时都可信：
 * 服务端愿意探活，说明它那边的会话与 NAT 映射都还在。
 */
export async function onTermPulse(
  id: string,
  handler: () => void
): Promise<UnlistenFn> {
  return listen(`nx://term/pulse/${id}`, () => handler());
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

/**
 * Agentic 一轮里发生的每一步：下令 / 执行结果 / 被跳过 / 提示 / 最终结论。
 * 与 token/done 是两条并行的线：这里送过程，那边送说了什么话。
 */
export async function onAgentStep(
  id: string,
  handler: (step: AgentStep) => void
): Promise<UnlistenFn> {
  return listen<AgentStep>(`nx://agent/step/${id}`, (ev) => handler(ev.payload));
}

/** 请用户批准一条危险命令（只有 approval 模式下才会出现） */
export async function onAgentApprove(
  id: string,
  handler: (req: AgentApproveRequest) => void
): Promise<UnlistenFn> {
  return listen<AgentApproveRequest>(`nx://agent/approve/${id}`, (ev) =>
    handler(ev.payload)
  );
}

/**
 * Agent 要对标签页动手（列出 / 切换 / 关闭 / 改名 / 重连）。
 *
 * 这是唯一一类**必须由前端执行**的动作：tab 是 React state，后端不知道它存在。
 * 处理完务必 `api.agentTabAck(...)`，后端在等回执。
 *
 * 全局事件（不分会话）：标签页是整个窗口的东西，不是某个会话的私产。
 */
export async function onAgentTab(
  handler: (req: AgentTabRequest) => void
): Promise<UnlistenFn> {
  return listen<AgentTabRequest>("nx://agent/tab", (ev) => handler(ev.payload));
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
