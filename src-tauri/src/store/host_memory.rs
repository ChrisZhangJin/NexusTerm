//! 主机记忆的持久化（Phase 4.5）。键的推导、文本校验、上限与「谁能删什么」见 `agent::hostmem`。
//!
//! 一张新表 `host_memory`，`CREATE TABLE IF NOT EXISTS`：旧库打开时只多出一张空表，
//! 不改动任何既有表 / 列，纯增量、可回退（旧版本会忽略它）。
//!
//! **不加密**：与 `sessions.note` / `agent_conversations` 同级。因此写入口统一过
//! `hostmem::validate_note_text`（拒绝凭据、限长），上限也在这里**与写入同一把锁内**检查，
//! 不依赖调用方自觉。

use rusqlite::{params, Connection, OptionalExtension, Row};

use super::{now_secs, Store};
use crate::agent::hostmem::{
    self, HostNote, Kind, Source, MAX_HOSTS, MAX_NOTES_PER_HOST, MIN_ID_PREFIX,
};

/// 建表（幂等）。由 `Store::open` 在 `migrate` 之后调用。
pub(super) fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS host_memory (
            id         TEXT PRIMARY KEY,
            host_key   TEXT NOT NULL,
            text       TEXT NOT NULL,
            kind       TEXT NOT NULL DEFAULT 'other',
            source     TEXT NOT NULL DEFAULT 'agent',
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_host_memory_host ON host_memory(host_key, created_at);
        "#,
    )
}

const COLS: &str = "id, host_key, text, kind, source, created_at, updated_at";

fn row_to_note(r: &Row<'_>) -> rusqlite::Result<HostNote> {
    let kind: String = r.get(3)?;
    let source: String = r.get(4)?;
    Ok(HostNote {
        id: r.get(0)?,
        host_key: r.get(1)?,
        text: r.get(2)?,
        // 库里出现不认识的值（被别的版本写过）：退化成最保守的，不让它打不开
        kind: Kind::parse(&kind).unwrap_or(Kind::Other),
        source: Source::parse(&source).unwrap_or(Source::User),
        created_at: r.get(5)?,
        updated_at: r.get(6)?,
    })
}

impl Store {
    /// 某台主机的全部备忘，按创建时间从早到晚。读库失败按空处理（备忘缺席不该拖垮一次提问）。
    pub fn host_memory_list(&self, host_key: &str) -> Vec<HostNote> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let Ok(mut stmt) = conn.prepare(&format!(
            "SELECT {COLS} FROM host_memory WHERE host_key = ?1 ORDER BY created_at, rowid"
        )) else {
            return Vec::new();
        };
        stmt.query_map(params![host_key], row_to_note)
            .map(|it| it.flatten().collect())
            .unwrap_or_default()
    }

    /// 新增一条。返回 `(备忘, 是否新建)`：同主机上文本相同（忽略大小写）的已存在则只刷新
    /// `updated_at`（并保留原作者与类别）、返回 `false`。
    ///
    /// 超出上限直接报错而不是挤掉旧的：每主机 [`MAX_NOTES_PER_HOST`] 条、全库 [`MAX_HOSTS`] 台。
    pub fn host_memory_add(
        &self,
        host_key: &str,
        text: &str,
        kind: Kind,
        source: Source,
    ) -> Result<(HostNote, bool), String> {
        let text = hostmem::validate_note_text(text)?;
        let host_key = host_key.trim();
        if host_key.is_empty() {
            return Err("无法确定主机，不能保存备忘".into());
        }
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        // 去重：同主机、同文本（忽略大小写）
        let dup: Option<HostNote> = conn
            .query_row(
                &format!(
                    "SELECT {COLS} FROM host_memory WHERE host_key = ?1 AND lower(text) = lower(?2)"
                ),
                params![host_key, text],
                row_to_note,
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some(mut n) = dup {
            let now = now_secs();
            conn.execute("UPDATE host_memory SET updated_at = ?2 WHERE id = ?1", params![n.id, now])
                .map_err(|e| e.to_string())?;
            n.updated_at = now;
            return Ok((n, false));
        }
        let per_host: i64 = conn
            .query_row("SELECT COUNT(*) FROM host_memory WHERE host_key = ?1", params![host_key], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if per_host as usize >= MAX_NOTES_PER_HOST {
            return Err(format!(
                "这台主机的备忘已满（{MAX_NOTES_PER_HOST} 条）：请先用 nx_memory forget 删掉不再需要的，再记新的"
            ));
        }
        if per_host == 0 {
            let hosts: i64 = conn
                .query_row("SELECT COUNT(DISTINCT host_key) FROM host_memory", [], |r| r.get(0))
                .map_err(|e| e.to_string())?;
            if hosts as usize >= MAX_HOSTS {
                return Err(format!(
                    "已有 {MAX_HOSTS} 台主机存有备忘，达到上限：请用户在「主机备忘」里清理不用的主机后再记"
                ));
            }
        }
        let now = now_secs();
        let note = HostNote {
            id: uuid::Uuid::new_v4().simple().to_string(),
            host_key: host_key.to_string(),
            text,
            kind,
            source,
            created_at: now,
            updated_at: now,
        };
        conn.execute(
            &format!("INSERT INTO host_memory ({COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"),
            params![
                note.id,
                note.host_key,
                note.text,
                note.kind.as_str(),
                note.source.as_str(),
                note.created_at,
                note.updated_at
            ],
        )
        .map_err(|e| e.to_string())?;
        Ok((note, true))
    }

    /// 用户编辑：改文本与类别。**作者变为用户**（Agent 此后不能再删它）。
    /// 改成与同主机另一条相同的文本会报错（避免编辑出重复项）。
    pub fn host_memory_update(&self, id: &str, text: &str, kind: Kind) -> Result<HostNote, String> {
        let text = hostmem::validate_note_text(text)?;
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let cur: HostNote = conn
            .query_row(&format!("SELECT {COLS} FROM host_memory WHERE id = ?1"), params![id], row_to_note)
            .optional()
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "这条备忘不存在（可能已被删除）".to_string())?;
        let clash: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM host_memory WHERE host_key = ?1 AND lower(text) = lower(?2) AND id <> ?3",
                params![cur.host_key, text, id],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        if clash > 0 {
            return Err("同一台主机上已有内容相同的备忘".into());
        }
        let now = now_secs();
        conn.execute(
            "UPDATE host_memory SET text = ?2, kind = ?3, source = 'user', updated_at = ?4 WHERE id = ?1",
            params![id, text, kind.as_str(), now],
        )
        .map_err(|e| e.to_string())?;
        Ok(HostNote { text, kind, source: Source::User, updated_at: now, ..cur })
    }

    /// 删除一条，返回被删的那条。
    ///
    /// - `by = Agent`：必须给 `host_key`（只在那台主机里找）；`id_or_prefix` 可以是 id 前缀
    ///   （至少 [`MIN_ID_PREFIX`] 位，且在该主机内唯一）；只能删 `source = agent` 的备忘。
    /// - `by = User`：按完整 id 删任何一条，`host_key` 可省。
    pub fn host_memory_delete(
        &self,
        host_key: Option<&str>,
        id_or_prefix: &str,
        by: Source,
    ) -> Result<HostNote, String> {
        let id_or_prefix = id_or_prefix.trim();
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let found: Vec<HostNote> = match (by, host_key) {
            (Source::Agent, None) => return Err("缺少主机范围".into()),
            (Source::Agent, Some(hk)) => {
                if id_or_prefix.len() < MIN_ID_PREFIX {
                    return Err(format!("id 太短（至少 {MIN_ID_PREFIX} 位）：先用 op=read 查看备忘 id"));
                }
                let mut stmt = conn
                    .prepare(&format!(
                        "SELECT {COLS} FROM host_memory WHERE host_key = ?1 AND substr(id, 1, length(?2)) = ?2"
                    ))
                    .map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map(params![hk, id_or_prefix], row_to_note)
                    .map_err(|e| e.to_string())?
                    .flatten()
                    .collect();
                rows
            }
            (Source::User, hk) => {
                let mut stmt = conn
                    .prepare(&format!(
                        "SELECT {COLS} FROM host_memory WHERE id = ?1 AND (?2 IS NULL OR host_key = ?2)"
                    ))
                    .map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map(params![id_or_prefix, hk], row_to_note)
                    .map_err(|e| e.to_string())?
                    .flatten()
                    .collect();
                rows
            }
        };
        let note = match found.len() {
            0 => return Err("这台主机上没有这条备忘（id 不对？先用 op=read 查看）".into()),
            1 => found.into_iter().next().unwrap_or_else(|| unreachable!()),
            _ => return Err("id 前缀不唯一，请给更长的 id".into()),
        };
        if by == Source::Agent && note.source == Source::User {
            return Err("这条备忘是用户写的（或用户改过），只有用户本人能删除；不要重试".into());
        }
        conn.execute("DELETE FROM host_memory WHERE id = ?1", params![note.id])
            .map_err(|e| e.to_string())?;
        Ok(note)
    }

    /// 清空一台主机的全部备忘（用户在界面上点「清空」）。返回删了几条。
    pub fn host_memory_clear_host(&self, host_key: &str) -> Result<usize, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute("DELETE FROM host_memory WHERE host_key = ?1", params![host_key])
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{a_session, temp_store_at};
    use super::*;

    #[test]
    fn crud_roundtrip_and_order() {
        let (store, _) = temp_store_at("hm-crud");
        assert!(store.host_memory_list("h").is_empty());
        let (a, created) = store.host_memory_add("h", "第一条", Kind::Path, Source::Agent).unwrap();
        assert!(created);
        let (b, _) = store.host_memory_add("h", "第二条", Kind::Sudo, Source::User).unwrap();
        store.host_memory_add("other", "别的主机", Kind::Other, Source::Agent).unwrap();
        let list = store.host_memory_list("h");
        assert_eq!(list.iter().map(|n| n.text.as_str()).collect::<Vec<_>>(), ["第一条", "第二条"]);
        assert_eq!(list[0].kind, Kind::Path);
        assert_eq!(list[1].source, Source::User);

        store.host_memory_delete(None, &a.id, Source::User).unwrap();
        assert_eq!(store.host_memory_list("h").len(), 1);
        assert_eq!(store.host_memory_list("h")[0].id, b.id);
        assert_eq!(store.host_memory_clear_host("h").unwrap(), 1);
        assert!(store.host_memory_list("h").is_empty());
        assert_eq!(store.host_memory_list("other").len(), 1);
    }

    #[test]
    fn add_validates_text() {
        let (store, _) = temp_store_at("hm-validate");
        assert!(store.host_memory_add("h", "  ", Kind::Other, Source::User).is_err());
        let e = store
            .host_memory_add("h", "password=Hunter2xyz", Kind::Other, Source::User)
            .unwrap_err();
        assert!(e.contains("凭据"));
        assert!(store.host_memory_add("  ", "x", Kind::Other, Source::User).is_err());
        assert!(store.host_memory_list("h").is_empty());
    }

    #[test]
    fn per_host_cap() {
        let (store, _) = temp_store_at("hm-cap");
        for i in 0..MAX_NOTES_PER_HOST {
            store.host_memory_add("h", &format!("事实 {i}"), Kind::Other, Source::Agent).unwrap();
        }
        let e = store.host_memory_add("h", "多一条", Kind::Other, Source::Agent).unwrap_err();
        assert!(e.contains("已满"), "{e}");
        // 其它主机不受影响；重复文本仍可「刷新」而不是报满
        store.host_memory_add("h2", "事实", Kind::Other, Source::Agent).unwrap();
        let (_, created) = store.host_memory_add("h", "事实 3", Kind::Other, Source::Agent).unwrap();
        assert!(!created);
        // 删一条后又能加
        let first = store.host_memory_list("h")[0].id.clone();
        store.host_memory_delete(Some("h"), &first, Source::Agent).unwrap();
        store.host_memory_add("h", "多一条", Kind::Other, Source::Agent).unwrap();
    }

    #[test]
    fn host_count_cap() {
        let (store, _) = temp_store_at("hm-hosts");
        for i in 0..MAX_HOSTS {
            store.host_memory_add(&format!("host-{i}"), "x", Kind::Other, Source::Agent).unwrap();
        }
        let e = store.host_memory_add("brand-new", "x", Kind::Other, Source::Agent).unwrap_err();
        assert!(e.contains("上限"), "{e}");
        // 已有备忘的主机还能继续加
        store.host_memory_add("host-0", "y", Kind::Other, Source::Agent).unwrap();
        // 清掉一台后新主机可以进
        store.host_memory_clear_host("host-1").unwrap();
        store.host_memory_add("brand-new", "x", Kind::Other, Source::Agent).unwrap();
    }

    #[test]
    fn duplicate_text_merges_ignoring_case_and_keeps_author() {
        let (store, _) = temp_store_at("hm-dup");
        let (a, _) = store.host_memory_add("h", "Uses DOAS", Kind::Sudo, Source::User).unwrap();
        let (b, created) = store.host_memory_add("h", " uses doas ", Kind::Other, Source::Agent).unwrap();
        assert!(!created);
        assert_eq!(b.id, a.id);
        assert_eq!(b.source, Source::User, "不能借重复写入把用户的备忘变成 Agent 的");
        assert_eq!(store.host_memory_list("h").len(), 1);
    }

    #[test]
    fn agent_deletes_only_own_notes() {
        let (store, _) = temp_store_at("hm-owner");
        let (mine, _) = store.host_memory_add("h", "我记的", Kind::Other, Source::Agent).unwrap();
        let (theirs, _) = store.host_memory_add("h", "用户写的", Kind::Other, Source::User).unwrap();
        let e = store.host_memory_delete(Some("h"), &theirs.id, Source::Agent).unwrap_err();
        assert!(e.contains("用户"), "{e}");
        assert_eq!(store.host_memory_list("h").len(), 2);
        store.host_memory_delete(Some("h"), &mine.id, Source::Agent).unwrap();
        assert_eq!(store.host_memory_list("h").len(), 1);
        // 用户可以删任何一条
        store.host_memory_delete(None, &theirs.id, Source::User).unwrap();
        assert!(store.host_memory_list("h").is_empty());
    }

    #[test]
    fn agent_delete_needs_scope_and_unique_prefix() {
        let (store, _) = temp_store_at("hm-prefix");
        let (n, _) = store.host_memory_add("h", "x", Kind::Other, Source::Agent).unwrap();
        assert!(store.host_memory_delete(None, &n.id, Source::Agent).is_err());
        assert!(store.host_memory_delete(Some("h"), &n.id[..2], Source::Agent).is_err(), "太短");
        assert!(store.host_memory_delete(Some("other"), &n.id, Source::Agent).is_err(), "跨主机");
        store.host_memory_delete(Some("h"), &n.id[..8], Source::Agent).unwrap();
        assert!(store.host_memory_delete(Some("h"), &n.id, Source::Agent).is_err(), "已删");
    }

    #[test]
    fn user_edit_transfers_ownership_to_user() {
        let (store, _) = temp_store_at("hm-edit");
        let (n, _) = store.host_memory_add("h", "旧", Kind::Other, Source::Agent).unwrap();
        let up = store.host_memory_update(&n.id, "新", Kind::Quirk).unwrap();
        assert_eq!((up.text.as_str(), up.kind, up.source), ("新", Kind::Quirk, Source::User));
        assert_eq!(store.host_memory_list("h")[0].source, Source::User);
        // Agent 此后删不掉
        assert!(store.host_memory_delete(Some("h"), &n.id, Source::Agent).is_err());
        // 编辑同样过校验与去重
        assert!(store.host_memory_update(&n.id, "token=abcdef123456", Kind::Other).is_err());
        store.host_memory_add("h", "另一条", Kind::Other, Source::User).unwrap();
        assert!(store.host_memory_update(&n.id, "另一条", Kind::Other).is_err());
        assert!(store.host_memory_update("nope", "x", Kind::Other).is_err());
    }

    /// 旧库（没有这张表）打开后平滑多出一张空表，既有数据不受影响；再开一次也不报错。
    #[test]
    fn opening_an_old_db_adds_table_additively() {
        let (store, db) = temp_store_at("hm-migrate");
        let base = db.parent().unwrap().to_path_buf();
        let id = a_session(&store, "keep-me");
        {
            let conn = store.conn.lock().unwrap();
            conn.execute_batch("DROP INDEX IF EXISTS idx_host_memory_host; DROP TABLE host_memory;")
                .unwrap();
        }
        drop(store);
        let reopened = Store::open(base.clone()).unwrap();
        assert_eq!(reopened.get(&id).unwrap().name, "keep-me");
        assert!(reopened.host_memory_list("h").is_empty());
        reopened.host_memory_add("h", "记一条", Kind::Other, Source::User).unwrap();
        drop(reopened);
        let again = Store::open(base).unwrap();
        assert_eq!(again.host_memory_list("h").len(), 1);
    }
}
