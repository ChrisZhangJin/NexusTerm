//! Agent 对话与运行摘要的持久化（Phase 2.6）。
//!
//! 两张新表，全是 `CREATE TABLE IF NOT EXISTS`：旧库打开时只多出两张空表，
//! 不改动任何既有表 / 列，因此对已有 `nexus.db` 是纯增量、可回退（旧版本会忽略它们）。
//!
//! - `agent_conversations`：每个 scope 一行，`messages` 是 JSON 数组。目前只有 `global` 一个 scope
//!   （面板对话本来就全局唯一，见 `agent::history` 模块注释）。
//! - `agent_run_summaries`：每轮运行一行，`summary` 是 `RunSummary` 的 JSON，只留最近
//!   [`MAX_SUMMARIES_KEPT`] 条。
//!
//! **不加密**：与 `sessions.note` 同级（见 `migrate` 里的说明）。内容在落盘前已由
//! `agent::history` 脱敏；保险库（`vault`）只覆盖凭据列，不含这里。`clear` 会真删。

use rusqlite::{params, Connection};
use serde_json::Value;

use super::{now_secs, Store};
use crate::agent::history::{RunSummary, MAX_SUMMARIES_KEPT};

/// 对话的唯一 scope。
pub const GLOBAL_SCOPE: &str = "global";

/// 建表（幂等）。由 `Store::open` 在 `migrate` 之后调用。
pub(super) fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS agent_conversations (
            scope      TEXT PRIMARY KEY,
            messages   TEXT NOT NULL DEFAULT '[]',
            updated_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS agent_run_summaries (
            id      INTEGER PRIMARY KEY AUTOINCREMENT,
            scope   TEXT NOT NULL DEFAULT 'global',
            ts      INTEGER NOT NULL,
            summary TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_agent_run_summaries_scope
            ON agent_run_summaries(scope, id);
        "#,
    )
}

impl Store {
    /// 覆盖保存全局对话。`messages` 必须已经过 `agent::history::sanitize_messages`。
    pub fn agent_conv_save(&self, messages: &[Value]) -> Result<(), String> {
        let json = serde_json::to_string(messages).map_err(|e| e.to_string())?;
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO agent_conversations (scope, messages, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(scope) DO UPDATE SET messages = excluded.messages, updated_at = excluded.updated_at",
            params![GLOBAL_SCOPE, json, now_secs()],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// 读全局对话。没有 / 数据损坏都当空：坏数据不该让面板打不开。
    pub fn agent_conv_load(&self) -> Vec<Value> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let raw: Option<String> = conn
            .query_row(
                "SELECT messages FROM agent_conversations WHERE scope = ?1",
                params![GLOBAL_SCOPE],
                |r| r.get(0),
            )
            .ok();
        raw.and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok())
            .unwrap_or_default()
    }

    /// 「清空对话」：对话与运行摘要一并删除（摘要也是对话内容的衍生物，留着等于没清）。
    pub fn agent_history_clear(&self) -> Result<(), String> {
        let mut conn = self.conn.lock().map_err(|e| e.to_string())?;
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM agent_conversations", [])
            .map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM agent_run_summaries", [])
            .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())
    }

    /// 追加一条运行摘要，并把超出 [`MAX_SUMMARIES_KEPT`] 的最旧几条删掉。
    pub fn agent_summary_push(&self, s: &RunSummary) -> Result<(), String> {
        let json = serde_json::to_string(s).map_err(|e| e.to_string())?;
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO agent_run_summaries (scope, ts, summary) VALUES (?1, ?2, ?3)",
            params![GLOBAL_SCOPE, s.ts, json],
        )
        .map_err(|e| e.to_string())?;
        conn.execute(
            "DELETE FROM agent_run_summaries WHERE scope = ?1 AND id NOT IN
               (SELECT id FROM agent_run_summaries WHERE scope = ?1 ORDER BY id DESC LIMIT ?2)",
            params![GLOBAL_SCOPE, MAX_SUMMARIES_KEPT as i64],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// 最近 `n` 条摘要，**从旧到新**。单条损坏就跳过。
    pub fn agent_summaries_recent(&self, n: usize) -> Vec<RunSummary> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let Ok(mut stmt) = conn.prepare(
            "SELECT summary FROM agent_run_summaries WHERE scope = ?1 ORDER BY id DESC LIMIT ?2",
        ) else {
            return Vec::new();
        };
        let rows = stmt.query_map(params![GLOBAL_SCOPE, n as i64], |r| r.get::<_, String>(0));
        let mut out: Vec<RunSummary> = match rows {
            Ok(it) => it
                .flatten()
                .filter_map(|s| serde_json::from_str(&s).ok())
                .collect(),
            Err(_) => Vec::new(),
        };
        out.reverse();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{a_session, temp_store_at};
    use super::*;
    use serde_json::json;

    fn summ(i: i64) -> RunSummary {
        RunSummary {
            ts: i,
            goal: format!("g{i}"),
            actions: vec![format!("exec[a] cmd{i}")],
            outcome: "ok".into(),
            final_state: String::new(),
        }
    }

    #[test]
    fn conversation_roundtrip_and_overwrite() {
        let (store, _) = temp_store_at("agent-conv");
        assert!(store.agent_conv_load().is_empty());
        store
            .agent_conv_save(&[json!({"id":"a","role":"user","text":"hi"})])
            .unwrap();
        store
            .agent_conv_save(&[
                json!({"id":"a","role":"user","text":"hi"}),
                json!({"id":"b","role":"assistant","text":"yo","step":{"kind":"keys","step":1}}),
            ])
            .unwrap();
        let got = store.agent_conv_load();
        assert_eq!(got.len(), 2);
        assert_eq!(got[1]["step"]["kind"], "keys");
    }

    #[test]
    fn corrupt_conversation_reads_as_empty() {
        let (store, _) = temp_store_at("agent-corrupt");
        store.agent_conv_save(&[]).unwrap();
        store
            .conn
            .lock()
            .unwrap()
            .execute("UPDATE agent_conversations SET messages = '{not json'", [])
            .unwrap();
        assert!(store.agent_conv_load().is_empty());
    }

    #[test]
    fn summaries_are_ordered_oldest_first_and_pruned() {
        let (store, _) = temp_store_at("agent-sum");
        for i in 0..(MAX_SUMMARIES_KEPT as i64 + 5) {
            store.agent_summary_push(&summ(i)).unwrap();
        }
        let all = store.agent_summaries_recent(1000);
        assert_eq!(all.len(), MAX_SUMMARIES_KEPT, "超出保留条数的最旧几条被删");
        assert_eq!(all.last().unwrap().goal, format!("g{}", MAX_SUMMARIES_KEPT + 4));
        assert_eq!(all[0].goal, "g5");
        let last3 = store.agent_summaries_recent(3);
        assert_eq!(last3.len(), 3);
        assert!(last3[0].ts < last3[2].ts, "从旧到新");
    }

    #[test]
    fn clear_deletes_both_tables() {
        let (store, _) = temp_store_at("agent-clear");
        store
            .agent_conv_save(&[json!({"id":"a","role":"user","text":"hi"})])
            .unwrap();
        store.agent_summary_push(&summ(1)).unwrap();
        store.agent_history_clear().unwrap();
        assert!(store.agent_conv_load().is_empty());
        assert!(store.agent_summaries_recent(10).is_empty());
        // 清空是真删：表里一行都不剩（不是写了个空数组）
        let conn = store.conn.lock().unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM agent_conversations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    /// 旧库（没有这两张表）打开后要平滑多出两张空表，既有数据不受影响。
    #[test]
    fn opening_an_old_db_adds_tables_additively() {
        let (store, db) = temp_store_at("agent-migrate");
        let base = db.parent().unwrap().to_path_buf();
        let id = a_session(&store, "keep-me");
        {
            let conn = store.conn.lock().unwrap();
            conn.execute_batch(
                "DROP INDEX IF EXISTS idx_agent_run_summaries_scope;
                 DROP TABLE agent_conversations;
                 DROP TABLE agent_run_summaries;",
            )
            .unwrap();
        }
        drop(store);
        let reopened = Store::open(base.clone()).unwrap();
        assert_eq!(reopened.get(&id).unwrap().name, "keep-me");
        assert!(reopened.agent_conv_load().is_empty());
        reopened
            .agent_conv_save(&[json!({"id":"a","role":"user","text":"x"})])
            .unwrap();
        reopened.agent_summary_push(&summ(1)).unwrap();
        drop(reopened);
        // 再开一次（表已存在）也不报错，数据还在
        let again = Store::open(base).unwrap();
        assert_eq!(again.agent_conv_load().len(), 1);
        assert_eq!(again.agent_summaries_recent(5).len(), 1);
    }
}
