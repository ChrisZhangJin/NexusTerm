import { t } from "../i18n";

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
  if (a.running)
    parts.push(
      t("{n} 个正在进行的传输", a.running === 1 ? "{n} transfer in progress" : "{n} transfers in progress", {
        n: a.running,
      })
    );
  if (a.queued)
    parts.push(
      t("队列中等待的 {n} 个任务", a.queued === 1 ? "{n} queued task" : "{n} queued tasks", { n: a.queued })
    );
  return t(
    "SFTP 面板里还有{parts}，关闭会中断传输（队列不会保留）。",
    "The SFTP panel still has {parts}. Closing will interrupt them (the queue won't be kept).",
    { parts: parts.join(t("、", " and ")) }
  );
}
