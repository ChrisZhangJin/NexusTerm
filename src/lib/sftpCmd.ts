/**
 * 在 SSH 终端里敲了 `sftp ...` 时，应该顺手把侧边的 SFTP 面板打开并同步到当前目录。
 *
 * 只认「第一个词恰好是 sftp」：`sudo sftp` / `echo sftp` / `sftpx` 都不算 ——
 * 宁可漏掉也不要误触发（误触发会无故弹面板）。
 */
export function isSftpCommand(line: string): boolean {
  const first = line.trim().split(/\s+/, 1)[0];
  return first === "sftp";
}
