//! NexusAgent v2 / P2 决策依据与决策记录的持久化。
//!
//! 设计稿 §4 写了 `profile` / `intents` / `steward_checkpoints` 等多张表。这里**有意收敛成两张**：
//! - `steward_ctx`：偏好、意向、知识、剧本四类依据共用一张表，按 `kind` 区分，各自特有的
//!   字段放 `extra` JSON。四类条目总数只有几十条，启动时一次性读进内存，不需要按列查询；
//!   字段还会继续演进（剧本后面要加正则、意向要加终态判定），JSON 免掉反复 ALTER TABLE。
//! - `steward_decisions`：每条 `DecisionRecord` 一行，整条 JSON 落盘，只把 `outcome` 提出来做列，
//!   便于回执校验后原地更新。`seq` 用 SQLite 自增，与内存里的环形缓冲序号一致。
//!
//! 两张表都是 `CREATE TABLE IF NOT EXISTS`，对旧库纯增量；**不加密**，与 `host_memory` 同级
//! （保险库只覆盖凭据列）。写入口统一过 `agent::profile::validate_*`，拒绝凭据类文本与控制字符。
//!
//! P4 再加第三张表 `steward_checkpoints`：整条 Checkpoint JSON 一行，启动时扫描未完成契约。
//! 与决策表一样是纯增量迁移，旧库打开只多一张空表。

use rusqlite::{params, Connection, Row};

use super::Store;

/// 决策审计表最多保留多少条。满了删最旧的，不让长任务把库撑大。
const MAX_DECISIONS: i64 = 500;

/// `steward_ctx` 的一行。`extra` 是 JSON 字符串，由 `agent::profile` 按 kind 解析。
#[derive(Debug, Clone)]
pub struct CtxRow {
    pub id: String,
    /// preference | intent | knowledge | playbook
    pub kind: String,
    pub text: String,
    /// hard | soft（只有 preference 用）
    pub strength: String,
    /// global | claude_code | ssh | shell | <host_key>
    pub scope: String,
    /// user | learned | seed
    pub source: String,
    /// 来源说明，例如「由 DecisionRecord #12 的纠正学到」
    pub evidence: String,
    pub extra: String,
    /// 意向条目归属的契约；其它 kind 为空
    pub contract_id: String,
    /// 意向栈顺序，越大越靠栈顶
    pub position: i32,
    pub created_at: i64,
    pub updated_at: i64,
}

pub(super) fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS steward_ctx (
            id          TEXT PRIMARY KEY,
            kind        TEXT NOT NULL,
            text        TEXT NOT NULL,
            strength    TEXT NOT NULL DEFAULT 'soft',
            scope       TEXT NOT NULL DEFAULT 'global',
            source      TEXT NOT NULL DEFAULT 'user',
            evidence    TEXT NOT NULL DEFAULT '',
            extra       TEXT NOT NULL DEFAULT '{}',
            contract_id TEXT NOT NULL DEFAULT '',
            position    INTEGER NOT NULL DEFAULT 0,
            created_at  INTEGER NOT NULL,
            updated_at  INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_steward_ctx_kind ON steward_ctx(kind, position);

        CREATE TABLE IF NOT EXISTS steward_decisions (
            seq           INTEGER PRIMARY KEY AUTOINCREMENT,
            ts_ms         INTEGER NOT NULL,
            contract_id   TEXT NOT NULL,
            console_id    TEXT NOT NULL,
            situation_sig TEXT NOT NULL,
            record_json   TEXT NOT NULL,
            outcome       TEXT NOT NULL DEFAULT 'proposed'
        );

        CREATE TABLE IF NOT EXISTS steward_checkpoints (
            contract_id   TEXT PRIMARY KEY,
            console_id    TEXT NOT NULL,
            record_json   TEXT NOT NULL,
            state         TEXT NOT NULL DEFAULT 'observing',
            saved_wall_ms INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_steward_cp_state ON steward_checkpoints(state);

        -- 用户删掉的蒸馏剧本留一条墓碑：蒸馏器每轮按确定性 id 重新生成候选，
        -- 没有墓碑的话删一条、下一轮又冒出来一条
        CREATE TABLE IF NOT EXISTS steward_tombstones (
            id TEXT PRIMARY KEY,
            ts INTEGER NOT NULL
        );
        "#,
    )
}

fn row_to_ctx(r: &Row<'_>) -> rusqlite::Result<CtxRow> {
    Ok(CtxRow {
        id: r.get(0)?,
        kind: r.get(1)?,
        text: r.get(2)?,
        strength: r.get(3)?,
        scope: r.get(4)?,
        source: r.get(5)?,
        evidence: r.get(6)?,
        extra: r.get(7)?,
        contract_id: r.get(8)?,
        position: r.get(9)?,
        created_at: r.get(10)?,
        updated_at: r.get(11)?,
    })
}

const CTX_COLS: &str = "id, kind, text, strength, scope, source, evidence, extra, contract_id, position, created_at, updated_at";

impl Store {
    /// 启动时全量载入。坏行（extra 不是 JSON、kind 不认识）不该让整个面板打不开：
    /// 解析与归一在 `agent::profile::Brain::from_rows` 里逐行做，这里只负责搬运。
    pub fn steward_ctx_load(&self) -> Vec<CtxRow> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let Ok(mut stmt) = conn.prepare(&format!(
            "SELECT {CTX_COLS} FROM steward_ctx ORDER BY kind, position, created_at"
        )) else {
            return Vec::new();
        };
        let rows = stmt
            .query_map([], row_to_ctx)
            .map(|it| it.filter_map(Result::ok).collect())
            .unwrap_or_default();
        rows
    }

    pub fn steward_ctx_upsert(&self, row: &CtxRow) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO steward_ctx
             (id, kind, text, strength, scope, source, evidence, extra, contract_id, position, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(id) DO UPDATE SET
               kind = excluded.kind, text = excluded.text, strength = excluded.strength,
               scope = excluded.scope, source = excluded.source, evidence = excluded.evidence,
               extra = excluded.extra, contract_id = excluded.contract_id,
               position = excluded.position, updated_at = excluded.updated_at",
            params![
                row.id,
                row.kind,
                row.text,
                row.strength,
                row.scope,
                row.source,
                row.evidence,
                row.extra,
                row.contract_id,
                row.position,
                row.created_at,
                row.updated_at
            ],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn steward_ctx_delete(&self, id: &str) -> Result<bool, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let n = conn
            .execute("DELETE FROM steward_ctx WHERE id = ?1", params![id])
            .map_err(|e| e.to_string())?;
        Ok(n > 0)
    }

    /// 契约停止时把它名下的意向栈一并归档（不删决策记录，审计要留着）。
    pub fn steward_ctx_delete_intents(&self, contract_id: &str) -> Result<usize, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let n = conn
            .execute(
                "DELETE FROM steward_ctx WHERE kind = 'intent' AND contract_id = ?1",
                params![contract_id],
            )
            .map_err(|e| e.to_string())?;
        Ok(n)
    }

    /// 落一条决策，返回 SQLite 分配的 seq。调用方把它回填进 `DecisionRecord.seq`
    /// 再写回 `record_json` 是不必要的：JSON 里也带一份 seq（0），前端展示用列里的 seq 为准。
    pub fn steward_decision_insert(
        &self,
        ts_ms: i64,
        contract_id: &str,
        console_id: &str,
        situation_sig: &str,
        record_json: &str,
        outcome: &str,
    ) -> Result<i64, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO steward_decisions (ts_ms, contract_id, console_id, situation_sig, record_json, outcome)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![ts_ms, contract_id, console_id, situation_sig, record_json, outcome],
        )
        .map_err(|e| e.to_string())?;
        let seq = conn.last_insert_rowid();
        // 与写入同一把锁内裁剪，避免并发下超上限
        conn.execute(
            "DELETE FROM steward_decisions WHERE seq <=
               (SELECT MAX(seq) FROM steward_decisions) - ?1",
            params![MAX_DECISIONS],
        )
        .map_err(|e| e.to_string())?;
        Ok(seq)
    }

    pub fn steward_decision_outcome(&self, seq: i64, outcome: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "UPDATE steward_decisions SET outcome = ?1 WHERE seq = ?2",
            params![outcome, seq],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// 最近若干条决策的 `(seq, outcome, record_json)`，按 seq 升序。
    pub fn steward_decisions(&self, limit: usize) -> Vec<(i64, String, String)> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let Ok(mut stmt) = conn.prepare(
            "SELECT seq, outcome, record_json FROM steward_decisions ORDER BY seq DESC LIMIT ?1",
        ) else {
            return Vec::new();
        };
        let mut v: Vec<(i64, String, String)> = stmt
            .query_map([limit.clamp(1, 500) as i64], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .map(|it| it.filter_map(Result::ok).collect())
            .unwrap_or_default();
        v.reverse();
        v
    }

    pub fn steward_decisions_clear(&self) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute("DELETE FROM steward_decisions", [])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    // ───────────────── P4：Checkpoint ─────────────────

    /// 一契约一行，重复保存走 UPSERT（长任务每 30s 写一次也不会堆积）。
    pub fn steward_checkpoint_save(
        &self,
        contract_id: &str,
        console_id: &str,
        record_json: &str,
        state: &str,
        saved_wall_ms: i64,
    ) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO steward_checkpoints
                (contract_id, console_id, record_json, state, saved_wall_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(contract_id) DO UPDATE SET
                console_id = excluded.console_id,
                record_json = excluded.record_json,
                state = excluded.state,
                saved_wall_ms = excluded.saved_wall_ms",
            params![contract_id, console_id, record_json, state, saved_wall_ms],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// 启动时扫描未完成契约：`(contract_id, console_id, state, record_json)`。
    pub fn steward_checkpoints(&self) -> Vec<(String, String, String, String)> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let Ok(mut stmt) = conn.prepare(
            "SELECT contract_id, console_id, state, record_json
             FROM steward_checkpoints WHERE state != 'completed'
             ORDER BY saved_wall_ms DESC",
        ) else {
            return Vec::new();
        };
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map(|it| it.filter_map(Result::ok).collect())
            .unwrap_or_default()
    }

    /// 清理不再可能恢复的 Checkpoint：已完成的，以及超过 TTL 的（恢复判定反正会拒）。
    /// 一契约一行，但每次启动 / 重开 tab 都会产生新契约 id，不裁剪的话表会无限增长。
    pub fn steward_checkpoints_prune(&self, now_wall_ms: i64, ttl_ms: i64) -> usize {
        let Ok(conn) = self.conn.lock() else {
            return 0;
        };
        conn.execute(
            "DELETE FROM steward_checkpoints WHERE state = 'completed' OR saved_wall_ms < ?1",
            params![now_wall_ms.saturating_sub(ttl_ms)],
        )
        .unwrap_or(0)
    }

    pub fn steward_tombstone_add(&self, id: &str, ts: i64) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT OR REPLACE INTO steward_tombstones (id, ts) VALUES (?1, ?2)",
            params![id, ts],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn steward_tombstoned(&self, id: &str) -> bool {
        let Ok(conn) = self.conn.lock() else {
            // 读不到就当作已删除：宁可少蒸馏一条，也不复活用户删掉的剧本
            return true;
        };
        conn.query_row(
            "SELECT 1 FROM steward_tombstones WHERE id = ?1",
            params![id],
            |_| Ok(()),
        )
        .is_ok()
    }

    pub fn steward_checkpoint(&self, contract_id: &str) -> Option<String> {
        let conn = self.conn.lock().ok()?;
        conn.query_row(
            "SELECT record_json FROM steward_checkpoints WHERE contract_id = ?1",
            params![contract_id],
            |r| r.get(0),
        )
        .ok()
    }

    /// 契约完成 / 用户停止后标记，避免下次启动误恢复。
    pub fn steward_checkpoint_state(&self, contract_id: &str, state: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "UPDATE steward_checkpoints SET state = ?1 WHERE contract_id = ?2",
            params![state, contract_id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::temp_store_at;

    #[test]
    fn prune_drops_completed_and_expired_checkpoints() {
        let (store, _) = temp_store_at("steward-cp-prune");
        store.steward_checkpoint_save("c-old", "t1", "{}", "observing", 1_000).unwrap();
        store.steward_checkpoint_save("c-done", "t2", "{}", "observing", 9_000).unwrap();
        store.steward_checkpoint_state("c-done", "completed").unwrap();
        store.steward_checkpoint_save("c-live", "t3", "{}", "observing", 9_500).unwrap();
        assert_eq!(store.steward_checkpoints_prune(10_000, 5_000), 2);
        let left: Vec<String> = store.steward_checkpoints().into_iter().map(|r| r.0).collect();
        assert_eq!(left, vec!["c-live".to_string()]);
        assert!(store.steward_checkpoint("c-old").is_none());
    }

    #[test]
    fn tombstones_persist() {
        let (store, _) = temp_store_at("steward-tomb");
        assert!(!store.steward_tombstoned("distilled-1"));
        store.steward_tombstone_add("distilled-1", 5).unwrap();
        assert!(store.steward_tombstoned("distilled-1"));
    }
}
