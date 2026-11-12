//! 统一托管引擎的现场持久化（设计 §4）：`host_sessions` 一张表，一份契约一行。
//!
//! 整条 `HostCheckpoint` JSON 放 `json` 列，只把相位标签提出来做 `state` 列，启动时按它扫描非终态行。
//! **明文存储**（已定），与历史表 / 时间线表同级；密文本来就不进 `messages`。
//! `CREATE TABLE IF NOT EXISTS`，对旧库纯增量。
//!
//! 本层只搬运字符串，不解析 JSON：版本判断、迁移在 `agent::host::checkpoint` 里做，
//! 读到更新版本写的行时调用方可以原样保留、不覆盖。
// PR6 接入启动扫描 / 接驳前，这些入口只有测试在用
#![allow(dead_code)]

use rusqlite::{params, Connection, OptionalExtension, Row};

use super::Store;

/// 终态标签：这些行不再接驳。`expired` 由 [`Store::host_sessions_expire`] 打上。
pub const TERMINAL_STATES: &[&str] = &["completed", "failed", "stopped", "expired"];
pub const STATE_EXPIRED: &str = "expired";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSessionRow {
    pub contract_id: String,
    pub state: String,
    pub json: String,
    pub updated_at: i64,
}

pub(super) fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS host_sessions (
            contract_id TEXT PRIMARY KEY,
            state       TEXT NOT NULL,
            json        TEXT NOT NULL,
            updated_at  INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_host_sessions_state ON host_sessions(state);
        "#,
    )
}

fn row_to_session(r: &Row<'_>) -> rusqlite::Result<HostSessionRow> {
    Ok(HostSessionRow { contract_id: r.get(0)?, state: r.get(1)?, json: r.get(2)?, updated_at: r.get(3)? })
}

/// `state NOT IN (...)` 的占位串，与 [`TERMINAL_STATES`] 同步。
fn terminal_list() -> String {
    TERMINAL_STATES.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(", ")
}

impl Store {
    pub fn host_session_upsert(&self, contract_id: &str, state: &str, json: &str, updated_at: i64) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO host_sessions (contract_id, state, json, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(contract_id) DO UPDATE SET
               state = excluded.state, json = excluded.json, updated_at = excluded.updated_at",
            params![contract_id, state, json, updated_at],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn host_session_get(&self, contract_id: &str) -> Option<HostSessionRow> {
        let conn = self.conn.lock().ok()?;
        conn.query_row(
            "SELECT contract_id, state, json, updated_at FROM host_sessions WHERE contract_id = ?1",
            params![contract_id],
            row_to_session,
        )
        .optional()
        .ok()
        .flatten()
    }

    /// 全部行，按更新时间从旧到新。
    pub fn host_sessions(&self) -> Vec<HostSessionRow> {
        self.host_sessions_where("1 = 1")
    }

    /// 启动扫描用：所有非终态行（含 suspended），按更新时间从旧到新。
    pub fn host_sessions_non_terminal(&self) -> Vec<HostSessionRow> {
        self.host_sessions_where(&format!("state NOT IN ({})", terminal_list()))
    }

    fn host_sessions_where(&self, cond: &str) -> Vec<HostSessionRow> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let Ok(mut stmt) = conn.prepare(&format!(
            "SELECT contract_id, state, json, updated_at FROM host_sessions WHERE {cond}
             ORDER BY updated_at, contract_id"
        )) else {
            return Vec::new();
        };
        let rows = stmt
            .query_map([], row_to_session)
            .map(|it| it.filter_map(Result::ok).collect())
            .unwrap_or_default();
        rows
    }

    pub fn host_session_delete(&self, contract_id: &str) -> Result<bool, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let n = conn
            .execute("DELETE FROM host_sessions WHERE contract_id = ?1", params![contract_id])
            .map_err(|e| e.to_string())?;
        Ok(n > 0)
    }

    /// TTL 清理：`updated_at` 早于 `now - ttl` 的
    /// - 非终态行标为 `expired`（保留 JSON，面板还能看到发生过什么），返回它们的 contract_id；
    /// - 终态行（含早先标过的 expired）直接删除。
    pub fn host_sessions_expire(&self, now_ms: i64, ttl_ms: i64) -> Result<Vec<String>, String> {
        let cutoff = now_ms.saturating_sub(ttl_ms);
        let mut conn = self.conn.lock().map_err(|e| e.to_string())?;
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        tx.execute(
            &format!("DELETE FROM host_sessions WHERE updated_at < ?1 AND state IN ({})", terminal_list()),
            params![cutoff],
        )
        .map_err(|e| e.to_string())?;
        let ids: Vec<String> = {
            let mut stmt = tx
                .prepare(&format!(
                    "SELECT contract_id FROM host_sessions WHERE updated_at < ?1 AND state NOT IN ({})
                     ORDER BY updated_at, contract_id",
                    terminal_list()
                ))
                .map_err(|e| e.to_string())?;
            let ids = stmt
                .query_map(params![cutoff], |r| r.get(0))
                .map_err(|e| e.to_string())?
                .filter_map(Result::ok)
                .collect();
            ids
        };
        for id in &ids {
            tx.execute(
                "UPDATE host_sessions SET state = ?2 WHERE contract_id = ?1",
                params![id, STATE_EXPIRED],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::temp_store_at;
    use super::*;

    fn ids(rows: &[HostSessionRow]) -> Vec<&str> {
        rows.iter().map(|r| r.contract_id.as_str()).collect()
    }

    #[test]
    fn upsert_get_overwrite() {
        let (store, _) = temp_store_at("host-upsert");
        assert!(store.host_session_get("c1").is_none());
        store.host_session_upsert("c1", "running", r#"{"v":1}"#, 10).unwrap();
        let r = store.host_session_get("c1").unwrap();
        assert_eq!(r, HostSessionRow { contract_id: "c1".into(), state: "running".into(), json: r#"{"v":1}"#.into(), updated_at: 10 });
        store.host_session_upsert("c1", "paused", r#"{"v":1,"x":2}"#, 20).unwrap();
        let r = store.host_session_get("c1").unwrap();
        assert_eq!((r.state.as_str(), r.json.as_str(), r.updated_at), ("paused", r#"{"v":1,"x":2}"#, 20));
        assert_eq!(store.host_sessions().len(), 1);
    }

    #[test]
    fn list_non_terminal_and_order() {
        let (store, _) = temp_store_at("host-scan");
        store.host_session_upsert("b", "waiting_human", "{}", 30).unwrap();
        store.host_session_upsert("a", "running", "{}", 20).unwrap();
        store.host_session_upsert("s", "suspended", "{}", 25).unwrap();
        store.host_session_upsert("d", "completed", "{}", 5).unwrap();
        store.host_session_upsert("f", "failed", "{}", 6).unwrap();
        store.host_session_upsert("x", "stopped", "{}", 7).unwrap();
        store.host_session_upsert("e", "expired", "{}", 8).unwrap();
        assert_eq!(ids(&store.host_sessions_non_terminal()), vec!["a", "s", "b"]);
        assert_eq!(ids(&store.host_sessions()), vec!["d", "f", "x", "e", "a", "s", "b"]);
    }

    #[test]
    fn delete_reports_whether_anything_went() {
        let (store, _) = temp_store_at("host-del");
        store.host_session_upsert("c1", "running", "{}", 1).unwrap();
        assert!(store.host_session_delete("c1").unwrap());
        assert!(!store.host_session_delete("c1").unwrap());
        assert!(store.host_session_get("c1").is_none());
    }

    #[test]
    fn expire_marks_stale_live_rows_and_drops_old_terminal_rows() {
        let (store, _) = temp_store_at("host-ttl");
        store.host_session_upsert("old-live", "suspended", r#"{"keep":1}"#, 1_000).unwrap();
        store.host_session_upsert("old-done", "completed", "{}", 1_000).unwrap();
        store.host_session_upsert("new-live", "running", "{}", 9_500).unwrap();
        store.host_session_upsert("new-done", "stopped", "{}", 9_500).unwrap();
        let expired = store.host_sessions_expire(10_000, 5_000).unwrap();
        assert_eq!(expired, vec!["old-live".to_string()]);
        let r = store.host_session_get("old-live").unwrap();
        assert_eq!((r.state.as_str(), r.json.as_str()), (STATE_EXPIRED, r#"{"keep":1}"#));
        assert!(store.host_session_get("old-done").is_none());
        assert_eq!(ids(&store.host_sessions_non_terminal()), vec!["new-live"]);
        // 再过一个 TTL：expired 行也被清掉，新行按自己的时间判
        let expired = store.host_sessions_expire(20_000, 5_000).unwrap();
        assert_eq!(expired, vec!["new-live".to_string()]);
        assert!(store.host_session_get("old-live").is_none());
        assert!(store.host_session_get("new-done").is_none());
    }

    #[test]
    fn schema_is_idempotent() {
        let (store, path) = temp_store_at("host-schema");
        store.host_session_upsert("c1", "running", "{}", 1).unwrap();
        drop(store);
        let reopened = Store::open(path.parent().unwrap().to_path_buf()).unwrap();
        assert_eq!(reopened.host_session_get("c1").unwrap().state, "running");
    }
}
