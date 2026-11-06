/**
 * 连接迁移（导出 / 导入全部连接）的前端纯逻辑：密码校验、文件名、结果文案。
 * 单独成文件是为了能在 node 里测 —— 组件里的同类逻辑没法跑。
 */

import type { ConnExportSummary, ConnImportSummary } from "../types";

/** 与 Rust `migrate::MIN_PASSWORD_CHARS` 一致；后端会再校验一次，这里只为省一次往返 */
export const MIN_EXPORT_PASSWORD = 8;

export const CONN_FILE_EXT = "nexusconn";

/** 导出密码的本地校验。返回错误文案，通过则返回 null。按字符（不是 UTF-16 单元）计数，与后端一致。 */
export function validateExportPassword(password: string, confirm: string): string | null {
  if ([...password].length < MIN_EXPORT_PASSWORD) {
    return `导出密码至少 ${MIN_EXPORT_PASSWORD} 个字符。`;
  }
  if (password !== confirm) return "两次输入的导出密码不一致。";
  return null;
}

/** 默认文件名：nexus-connections-YYYYMMDD.nexusconn */
export function exportFileName(now: Date = new Date()): string {
  const p = (n: number) => String(n).padStart(2, "0");
  return `nexus-connections-${now.getFullYear()}${p(now.getMonth() + 1)}${p(now.getDate())}.${CONN_FILE_EXT}`;
}

export interface MigrateReport {
  text: string;
  /** 有需要用户跟进的警告 */
  warn: boolean;
  /** 逐条警告（展示在摘要下面） */
  details: string[];
}

function credParts(r: {
  passwords: number;
  passphrases: number;
  totpSeeds: number;
  keys: number;
}): string {
  const xs = [
    r.passwords > 0 ? `${r.passwords} 个密码` : null,
    r.passphrases > 0 ? `${r.passphrases} 个私钥口令` : null,
    r.totpSeeds > 0 ? `${r.totpSeeds} 个 TOTP 种子` : null,
    r.keys > 0 ? `${r.keys} 个私钥文件` : null,
  ].filter((x): x is string => x !== null);
  return xs.length > 0 ? `，含 ${xs.join("、")}` : "";
}

export function describeExport(r: ConnExportSummary, path: string): MigrateReport {
  return {
    text: `已导出 ${r.sessions} 条连接${credParts(r)}到 ${path}。`,
    warn: r.warnings.length > 0,
    details: r.warnings,
  };
}

export function describeImport(r: ConnImportSummary): MigrateReport {
  const skip = r.skipped > 0 ? `；${r.skipped} 条与已有连接重复，已跳过（未覆盖）` : "";
  return {
    text: `已导入 ${r.imported} 条连接${credParts(r)}${skip}。`,
    warn: r.warnings.length > 0,
    details: r.warnings,
  };
}
