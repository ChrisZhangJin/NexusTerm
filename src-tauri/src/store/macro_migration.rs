//! 旧宏 → skill 迁移的幂等记录（Phase 3.5）。转换逻辑见 `agent::macro_migrate`。
//!
//! 一张新表 `macro_migrations`，`CREATE TABLE IF NOT EXISTS`：旧库打开时只多出一张空表，
//! 不改动任何既有表 / 列，纯增量、可回退（旧版本会忽略它）。
//!
//! 之所以用单独的表而不是在 skill 描述里打标签：描述是用户可编辑的文本，
//! 改掉标签就会让同一条宏被重复迁移；表里的记录与 skill 内容无关。
//! 记录**不**随 skill 删除而删除（见 `agent::macro_migrate` 对「技能已被删」的处理），
//! 也**不**随宏删除而删除（宏表里没有外键，迁移从不动原宏）。

use rusqlite::{params, Connection};

use super::{now_secs, Store};

/// 建表（幂等）。由 `Store::open` 在 `migrate` 之后调用。
pub(super) fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS macro_migrations (
            macro_id    TEXT PRIMARY KEY,
            skill_id    TEXT NOT NULL,
            migrated_at INTEGER NOT NULL
        );
        "#,
    )
}

impl Store {
    /// 这条宏迁移出的 skill id（没迁移过返回 `None`）。
    pub fn macro_migration_get(&self, macro_id: &str) -> Option<String> {
        let conn = self.conn.lock().ok()?;
        conn.query_row(
            "SELECT skill_id FROM macro_migrations WHERE macro_id = ?1",
            params![macro_id],
            |r| r.get::<_, String>(0),
        )
        .ok()
    }

    /// 记下「这条宏已迁移成这个 skill」。重复写入覆盖旧记录（重新迁移已删除的技能时用）。
    pub fn macro_migration_put(&self, macro_id: &str, skill_id: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO macro_migrations (macro_id, skill_id, migrated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(macro_id) DO UPDATE SET skill_id = excluded.skill_id, migrated_at = excluded.migrated_at",
            params![macro_id, skill_id, now_secs()],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::temp_store_at;

    #[test]
    fn put_get_roundtrip_and_overwrite() {
        let (store, _) = temp_store_at("macro-mig");
        assert_eq!(store.macro_migration_get("m1"), None);
        store.macro_migration_put("m1", "s1").unwrap();
        assert_eq!(store.macro_migration_get("m1").as_deref(), Some("s1"));
        store.macro_migration_put("m1", "s2").unwrap();
        assert_eq!(store.macro_migration_get("m1").as_deref(), Some("s2"));
    }

    #[test]
    fn reopening_existing_db_keeps_records() {
        let (store, path) = temp_store_at("macro-mig-reopen");
        store.macro_migration_put("m1", "s1").unwrap();
        drop(store);
        let again = super::super::Store::open(path.parent().unwrap().to_path_buf()).unwrap();
        assert_eq!(again.macro_migration_get("m1").as_deref(), Some("s1"));
    }
}
