/**
 * 各标签 SFTP 面板的传输活动登记。
 *
 * 面板的传输列表是它自己的 state，而「关标签」发生在 App 里——App 要在关之前
 * 问一句「还有没有在传的」，只能让面板把读数登记到这里。
 */
export interface SftpActivity {
  /** 正在传输 */
  running: number;
  /** 队列里等着的 */
  queued: number;
}

const readers = new Map<string, () => SftpActivity>();

/** 登记某个标签面板的读数函数；返回注销函数 */
export function registerSftpActivity(tabId: string, read: () => SftpActivity): () => void {
  readers.set(tabId, read);
  return () => {
    if (readers.get(tabId) === read) readers.delete(tabId);
  };
}

export function sftpActivity(tabId: string): SftpActivity {
  return readers.get(tabId)?.() ?? { running: 0, queued: 0 };
}

/** 关闭确认框里的说明；没有活动返回 null */
export function describeSftpActivity(a: SftpActivity): string | null {
  if (a.running + a.queued === 0) return null;
  const parts: string[] = [];
  if (a.running) parts.push(`${a.running} 个正在进行的传输`);
  if (a.queued) parts.push(`队列中等待的 ${a.queued} 个任务`);
  return `SFTP 面板里还有${parts.join("、")}，关闭会中断传输（队列不会保留）。`;
}
