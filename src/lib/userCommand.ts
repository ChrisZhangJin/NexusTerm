/**
 * 「用户提交了一行命令」推给后端 side buffer 前的判定（纯函数，便于测试）。
 *
 * 只有回车那一刻才会走到这里，绝不逐键推送。
 */
export interface UserCommandReport {
  /** 无回显时恒为空串：密码内容连 IPC 都不出前端 */
  line: string;
  /** 这行没被终端回显过（密码提示）→ 后端只存占位 */
  noEcho: boolean;
}

/** 与后端 `usercmd::MAX_LINE_BYTES` 同量级的前端上限，避免把整段粘贴塞进 IPC */
const MAX_LINE_CHARS = 4096;

/**
 * @param committed 行状态机（`feedKeys`）交出的整行
 * @param echoed 这行是否真被回显过（`lineEchoedRef`，至少连续 3 字符匹配才采信）
 * @param altScreen 是否在备用屏（vim / less 里敲的不是 shell 命令）
 * @returns 要推的负载；不该推（全屏程序、空行）时为 null
 */
export function buildUserCommandReport(
  committed: string,
  echoed: boolean,
  altScreen: boolean,
): UserCommandReport | null {
  if (altScreen) return null;
  if (!echoed) return { line: "", noEcho: true };
  const line = committed.trim();
  if (!line) return null;
  return { line: line.length > MAX_LINE_CHARS ? line.slice(0, MAX_LINE_CHARS) : line, noEcho: false };
}
