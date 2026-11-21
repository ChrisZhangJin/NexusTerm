/**
 * 「实际改动」与「历史托管」的纯逻辑：分组、文件与备份对上号、时间 / 状态的文字。
 */

import type { HostBackupEntry, HostChangeSummary } from "../types";

export interface ChangeGroup {
  label: string;
  items: string[];
}

/** 除文件外的分组（文件单独渲染，带「还原」按钮）。空组不出现 */
export function changeGroups(c: HostChangeSummary): ChangeGroup[] {
  const groups: ChangeGroup[] = [
    { label: "软件包", items: c.packages },
    { label: "服务", items: c.services },
    { label: "端口 / 防火墙", items: c.ports },
    { label: "容器", items: c.containers },
    { label: "下载", items: c.downloads },
    { label: "删除", items: c.removals ?? [] },
  ];
  return groups.filter((g) => g.items.length > 0);
}

/** 什么改动都没有（连命令都没执行过） */
export function changesEmpty(c: HostChangeSummary): boolean {
  return c.commands === 0 && c.filesWritten.length === 0 && changeGroups(c).length === 0;
}

export interface FileRow {
  path: string;
  backup?: HostBackupEntry;
}

/** 去掉 `./`、`~/` 前缀，便于和备份里的绝对路径比对后缀 */
const tail = (p: string) => p.replace(/^\.\//, "").replace(/^~\//, "");

/**
 * 文件行：决策里的写法（可能是相对路径 / `~`）与备份里的绝对路径对上号。
 * 完全相同，或备份路径以 `/<相对写法>` 结尾，就算同一个文件；没对上的备份单独列在后面。
 */
export function fileRows(files: string[], backups: HostBackupEntry[]): FileRow[] {
  const used = new Set<string>();
  const rows: FileRow[] = files.map((path) => {
    const t = tail(path);
    const b = backups.find(
      (x) => !used.has(x.id) && (x.path === path || (!path.startsWith("/") && t !== "" && x.path.endsWith(`/${t}`)))
    );
    if (b) used.add(b.id);
    return { path, backup: b };
  });
  for (const b of backups) {
    if (!used.has(b.id)) rows.push({ path: b.path, backup: b });
  }
  return rows;
}

/** 这条记录能不能「还原」；不能的话给人看的说明 */
export function backupInfo(b: HostBackupEntry): { restorable: boolean; note: string } {
  switch (b.status ?? "backedUp") {
    case "created":
      return { restorable: false, note: "这个文件是托管新建的" };
    case "skipped":
      return { restorable: false, note: `没有备份原文：${b.note || "原因不明"}` };
    default:
      return { restorable: true, note: `原文已备份 ${fmtSize(b.size)}` };
  }
}

/** 72_000 → 「1 分 12 秒」 */
export function fmtDuration(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 60) return `${s} 秒`;
  const m = Math.floor(s / 60);
  if (m < 60) return s % 60 ? `${m} 分 ${s % 60} 秒` : `${m} 分`;
  const h = Math.floor(m / 60);
  return m % 60 ? `${h} 小时 ${m % 60} 分` : `${h} 小时`;
}

/** 「11-20 14:05」；跨年带年份 */
export function fmtWhen(ts: number, now: number = Date.now()): string {
  const d = new Date(ts);
  const p = (n: number) => String(n).padStart(2, "0");
  const md = `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
  return d.getFullYear() === new Date(now).getFullYear() ? md : `${d.getFullYear()}-${md}`;
}

export function historyStateLabel(state: string): { label: string; tone: string } {
  switch (state) {
    case "completed":
      return { label: "完成", tone: "done" };
    case "failed":
      return { label: "失败", tone: "bad" };
    case "stopped":
      return { label: "已停止", tone: "idle" };
    default:
      return { label: state, tone: "idle" };
  }
}

/** 1536 → 「1.5 KB」 */
export function fmtSize(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}
