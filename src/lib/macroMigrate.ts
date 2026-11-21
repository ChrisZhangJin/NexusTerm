// 旧宏迁移（Phase 3.5）的前端小工具。后端转换见 Rust `agent::macro_migrate`。
import type { MacroMigrationPreview, MacroMigrationResult } from "../types";

/** 预览里「真的会保存」的条数 */
export function countMigratable(previews: MacroMigrationPreview[]): number {
  return previews.filter((p) => p.draft !== null).length;
}

/** 预览里带警告的条数（有警告的建议用户逐条核对） */
export function countWarned(previews: MacroMigrationPreview[]): number {
  return previews.filter((p) => p.warnings.length > 0).length;
}

/** 迁移结果的一行汇总，如「已迁移 3 条，跳过 1 条，失败 0 条」 */
export function summarizeMigration(results: MacroMigrationResult[]): string {
  let migrated = 0;
  let skipped = 0;
  let failed = 0;
  for (const r of results) {
    if (r.outcome.kind === "migrated") migrated++;
    else if (r.outcome.kind === "skipped") skipped++;
    else failed++;
  }
  return `已迁移 ${migrated} 条，跳过 ${skipped} 条，失败 ${failed} 条`;
}
