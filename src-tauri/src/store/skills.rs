//! Skill 的持久化（Phase 3.1）。模型与校验见 `agent::skill`，本文件只管 SQLite。
//!
//! 一张新表 `skills`，`CREATE TABLE IF NOT EXISTS`：旧库打开时只多出一张空表，
//! 不改动任何既有表 / 列，纯增量、可回退（旧版本会忽略它）。
//!
//! - `params` / `steps` / `verify` / `host_scope` 存 JSON 文本（与 `macros.steps` 同理：
//!   步骤结构还会演进，不建子表；单条 skill 从不需要按步骤查询）。
//! - `risk` / `source` 存 snake_case 文本。时间是 Unix 秒。
//! - **不加密**：与 `macros` / `sessions.note` 同级。skill 里按设计没有任何密码值
//!   （见 `agent::skill` 的安全约束），所以也没有需要加密的列。
//! - 上限：[`MAX_SKILLS`] 条；每条的步骤数 / 大小由 `skill::validate` 卡。
//!
//! 写入口（`skill_create` / `skill_update` / `skill_insert`）一律先 `normalize`（抬风险）再
//! `validate`，库里不会有未校验的 skill。成功 / 失败计数**只能**经 [`Store::skill_record_run_result`]
//! 增加，创建 / 更新都清不掉也伪造不了；更新时若回放相关内容变了，计数清零。

use rusqlite::{params, Connection, Row};
use serde_json::Value;

use super::{now_secs, Store};
use crate::agent::skill::{self, Skill, SkillInput, MAX_SKILLS};

/// 建表（幂等）。由 `Store::open` 在 `migrate` 之后调用。
pub(super) fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS skills (
            id             TEXT PRIMARY KEY,
            name           TEXT NOT NULL,
            description    TEXT NOT NULL DEFAULT '',
            params         TEXT NOT NULL DEFAULT '[]',
            steps          TEXT NOT NULL DEFAULT '[]',
            verify         TEXT,
            risk           TEXT NOT NULL DEFAULT 'medium',
            host_scope     TEXT NOT NULL DEFAULT '{"kind":"any"}',
            source         TEXT NOT NULL DEFAULT 'manual',
            success_count  INTEGER NOT NULL DEFAULT 0,
            fail_count     INTEGER NOT NULL DEFAULT 0,
            last_run_ts    INTEGER,
            created_at     INTEGER NOT NULL,
            updated_at     INTEGER NOT NULL,
            schema_version INTEGER NOT NULL DEFAULT 1
        );
        CREATE INDEX IF NOT EXISTS idx_skills_updated ON skills(updated_at);
        "#,
    )?;
    // 4.4 信任分级的两列：旧库补列（`ALTER … ADD COLUMN` 带默认值，纯增量；旧版本会忽略它们）
    let has = |name: &str| -> rusqlite::Result<bool> {
        let mut stmt = conn.prepare("SELECT name FROM pragma_table_info('skills')")?;
        let names = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(names.iter().any(|n| n == name))
    };
    if !has("trust_streak")? {
        conn.execute("ALTER TABLE skills ADD COLUMN trust_streak INTEGER NOT NULL DEFAULT 0", [])?;
    }
    if !has("trust_hash")? {
        conn.execute("ALTER TABLE skills ADD COLUMN trust_hash TEXT NOT NULL DEFAULT ''", [])?;
    }
    Ok(())
}

const COLS: &str = "id, name, description, params, steps, verify, risk, host_scope, source, \
                    success_count, fail_count, last_run_ts, created_at, updated_at, schema_version, \
                    trust_streak, trust_hash";

fn json<T: serde::Serialize>(v: &T) -> Result<String, String> {
    serde_json::to_string(v).map_err(|e| e.to_string())
}

/// 枚举列（存的是 snake_case 文本）读回。
fn enum_col<T: serde::de::DeserializeOwned>(s: String) -> rusqlite::Result<T> {
    serde_json::from_value(Value::String(s))
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))
}

fn json_col<T: serde::de::DeserializeOwned>(s: &str) -> rusqlite::Result<T> {
    serde_json::from_str(s)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))
}

fn row_to_skill(r: &Row) -> rusqlite::Result<Skill> {
    let verify: Option<String> = r.get(5)?;
    Ok(Skill {
        id: r.get(0)?,
        name: r.get(1)?,
        description: r.get(2)?,
        params: json_col(&r.get::<_, String>(3)?)?,
        steps: json_col(&r.get::<_, String>(4)?)?,
        verify: verify.as_deref().map(json_col).transpose()?,
        risk: enum_col(r.get(6)?)?,
        host_scope: json_col(&r.get::<_, String>(7)?)?,
        source: enum_col(r.get(8)?)?,
        success_count: r.get::<_, i64>(9)?.max(0) as u32,
        fail_count: r.get::<_, i64>(10)?.max(0) as u32,
        last_run_ts: r.get(11)?,
        created_at: r.get(12)?,
        updated_at: r.get(13)?,
        schema_version: r.get::<_, i64>(14)?.max(0) as u32,
        trust_streak: r.get::<_, i64>(15)?.max(0) as u32,
        trust_hash: r.get(16)?,
    })
}

fn enum_text<T: serde::Serialize>(v: &T) -> Result<String, String> {
    match serde_json::to_value(v).map_err(|e| e.to_string())? {
        Value::String(s) => Ok(s),
        other => Err(format!("枚举序列化异常：{other}")),
    }
}

fn validated(mut s: Skill) -> Result<Skill, String> {
    s.normalize();
    skill::validate(&s).map_err(|e| e.join("\n"))?;
    Ok(s)
}

fn count(conn: &Connection) -> Result<usize, String> {
    conn.query_row("SELECT COUNT(*) FROM skills", [], |r| r.get::<_, i64>(0))
        .map(|n| n as usize)
        .map_err(|e| e.to_string())
}

fn fetch(conn: &Connection, id: &str) -> Result<Option<Skill>, String> {
    let sql = format!("SELECT {COLS} FROM skills WHERE id = ?1");
    match conn.query_row(&sql, params![id], row_to_skill) {
        Ok(s) => Ok(Some(s)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// 写入一整行（`INSERT`）。调用方已 `validated`。
fn insert_row(conn: &Connection, s: &Skill) -> Result<(), String> {
    let verify = s.verify.as_ref().map(json).transpose()?;
    conn.execute(
        &format!("INSERT INTO skills ({COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)"),
        params![
            s.id,
            s.name,
            s.description,
            json(&s.params)?,
            json(&s.steps)?,
            verify,
            enum_text(&s.risk)?,
            json(&s.host_scope)?,
            enum_text(&s.source)?,
            s.success_count as i64,
            s.fail_count as i64,
            s.last_run_ts,
            s.created_at,
            s.updated_at,
            s.schema_version as i64,
            s.trust_streak as i64,
            s.trust_hash,
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

impl Store {
    /// 新建。id / 时间戳 / 计数由这里铸造，调用方无法伪造「已成功跑过」。
    pub fn skill_create(&self, input: SkillInput) -> Result<Skill, String> {
        let s = validated(input.into_skill(uuid::Uuid::new_v4().to_string(), now_secs()))?;
        self.skill_insert(s)
    }

    /// 写入一份已经铸好的 skill（`import_text` 的产物、录制 / 迁移产物）。
    /// 会再次 `normalize` + `validate`，并**强制清零**计数——信任只能靠真跑出来。
    pub fn skill_insert(&self, mut s: Skill) -> Result<Skill, String> {
        s.success_count = 0;
        s.fail_count = 0;
        s.last_run_ts = None;
        // 信任同样只能靠真跑出来：入库的一律从「首次运行」起步（导入 / 录制 / 迁移都走这里）
        s.trust_streak = 0;
        s.trust_hash.clear();
        let s = validated(s)?;
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        if count(&conn)? >= MAX_SKILLS {
            return Err(format!("skill 数量已达上限（{MAX_SKILLS}），请先删除不用的"));
        }
        if fetch(&conn, &s.id)?.is_some() {
            return Err("skill id 已存在".into());
        }
        insert_row(&conn, &s)?;
        Ok(s)
    }

    /// 更新内容。保留 id / 创建时间 / 来源；回放相关内容（步骤 / 参数 / 验证 / 范围）
    /// 有变化则成功 / 失败计数清零，否则保留。`input.source` 被忽略。
    pub fn skill_update(&self, id: &str, input: SkillInput) -> Result<Skill, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let old = fetch(&conn, id)?.ok_or_else(|| "skill 不存在".to_string())?;
        let keep_stats = old.same_behavior(&input);
        let mut s = input.clone().into_skill(old.id.clone(), now_secs());
        s.source = old.source;
        s.created_at = old.created_at;
        if keep_stats {
            s.success_count = old.success_count;
            s.fail_count = old.fail_count;
            s.last_run_ts = old.last_run_ts;
        }
        // 信任（4.4）：用户编辑过「会改变技能做什么」的内容（步骤 / 参数 / 验证 / 范围 / 实际风险）就清零。
        // 比 `keep_stats` 更严：用户把手填的风险调低也算（指纹含实际风险）。名称 / 描述不影响信任。
        s.normalize();
        if skill::trust_fingerprint(&old) == skill::trust_fingerprint(&s) {
            s.trust_streak = old.trust_streak;
            s.trust_hash = old.trust_hash.clone();
        }
        let s = validated(s)?;
        let verify = s.verify.as_ref().map(json).transpose()?;
        conn.execute(
            "UPDATE skills SET name=?2, description=?3, params=?4, steps=?5, verify=?6, risk=?7,
                host_scope=?8, success_count=?9, fail_count=?10, last_run_ts=?11, updated_at=?12,
                schema_version=?13, trust_streak=?14, trust_hash=?15 WHERE id=?1",
            params![
                s.id,
                s.name,
                s.description,
                json(&s.params)?,
                json(&s.steps)?,
                verify,
                enum_text(&s.risk)?,
                json(&s.host_scope)?,
                s.success_count as i64,
                s.fail_count as i64,
                s.last_run_ts,
                s.updated_at,
                s.schema_version as i64,
                s.trust_streak as i64,
                s.trust_hash,
            ],
        )
        .map_err(|e| e.to_string())?;
        Ok(s)
    }

    /// 删除。返回是否真删了一条。
    pub fn skill_delete(&self, id: &str) -> Result<bool, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let n = conn
            .execute("DELETE FROM skills WHERE id = ?1", params![id])
            .map_err(|e| e.to_string())?;
        Ok(n > 0)
    }

    /// 按 id 取。库里数据损坏（JSON 解不开）按「不存在」处理并记日志，不让界面打不开。
    pub fn skill_get(&self, id: &str) -> Option<Skill> {
        let conn = self.conn.lock().ok()?;
        match fetch(&conn, id) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("读取 skill {id} 失败：{e}");
                None
            }
        }
    }

    /// 列表，最近更新的在前。`query` 非空时按名称 / 描述做子串搜索（不区分大小写，
    /// ASCII 范围内；中文本就无大小写）。单条损坏的行跳过并记日志。
    pub fn skill_list(&self, query: Option<&str>) -> Vec<Skill> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let q = query.map(str::trim).filter(|q| !q.is_empty());
        let like = q.map(|q| {
            let esc = q.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
            format!("%{esc}%")
        });
        let sql = format!(
            "SELECT {COLS} FROM skills {} ORDER BY updated_at DESC, created_at DESC, id",
            if like.is_some() {
                "WHERE name LIKE ?1 ESCAPE '\\' OR description LIKE ?1 ESCAPE '\\'"
            } else {
                ""
            }
        );
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return Vec::new();
        };
        let rows = match &like {
            Some(l) => stmt.query_map(params![l], row_to_skill),
            None => stmt.query_map([], row_to_skill),
        };
        let Ok(rows) = rows else {
            return Vec::new();
        };
        rows.filter_map(|r| match r {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("跳过损坏的 skill 行：{e}");
                None
            }
        })
        .collect()
    }

    /// 记一次回放结果（只动计数与信任，不改 `updated_at`——运行不是编辑）。
    /// 仅供回放引擎（后端）调用，**不**暴露为 Tauri 命令——否则前端可以自抬信任。
    ///
    /// - 成功 / 失败计数 +1，`last_run_ts` 取当前；
    /// - **信任阶梯**（4.4）：只有 `ok && clean`（完整成功、验证通过、没有任何偏离）**且**
    ///   `ran_fingerprint` 与库里**当前**内容的指纹一致（跑的就是现在存着的这份；运行途中被编辑过则不算）
    ///   才让 `trust_streak` +1 并记下指纹；其余一律清零——失败 / 偏离 / 内容变了都会掉信任。
    pub fn skill_record_run(&self, id: &str, ok: bool, clean: bool, ran_fingerprint: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let cur = fetch(&conn, id)?.ok_or_else(|| "skill 不存在".to_string())?;
        let fp = skill::trust_fingerprint(&cur);
        let earn = ok && clean && !ran_fingerprint.is_empty() && ran_fingerprint == fp;
        let (streak, hash) = if earn {
            // 指纹变过（理论上编辑时已清零）就从 1 重新数
            let base = if cur.trust_hash == fp { cur.trust_streak } else { 0 };
            (base.saturating_add(1), fp)
        } else {
            (0, String::new())
        };
        let col = if ok { "success_count" } else { "fail_count" };
        conn.execute(
            &format!(
                "UPDATE skills SET {col} = {col} + 1, last_run_ts = ?2, trust_streak = ?3, trust_hash = ?4 WHERE id = ?1"
            ),
            params![id, now_secs(), streak as i64, hash],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// 清零信任（不动计数）：运行里出现过偏离、但没有「完整成功 / 明确失败」的结局时用。
    pub fn skill_drop_trust(&self, id: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute("UPDATE skills SET trust_streak = 0, trust_hash = '' WHERE id = ?1", params![id])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// 只记计数、不给信任加分的简化入口（成功也不算「干净」，失败照样清零信任）。
    pub fn skill_record_run_result(&self, id: &str, ok: bool) -> Result<(), String> {
        self.skill_record_run(id, ok, false, "")
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::temp_store_at;
    use super::*;
    use crate::agent::skill::{
        HostScope, OnUnexpected, Risk, SkillCheck, SkillParam, SkillSource, SkillStep,
    };

    fn exec(cmd: &str) -> SkillStep {
        SkillStep::Exec {
            command: cmd.into(),
            timeout_ms: None,
            expect_after: None,
            on_unexpected: OnUnexpected::Ask,
        }
    }

    fn input(name: &str) -> SkillInput {
        SkillInput {
            name: name.into(),
            description: format!("{name} 的描述"),
            params: vec![SkillParam {
                name: "dir".into(),
                description: String::new(),
                default_value: Some("/tmp".into()),
                required: false,
                secret: false,
            }],
            steps: vec![exec("ls {{dir}}")],
            verify: Some(SkillCheck { expect: "total".into(), timeout_ms: Some(3000) }),
            risk: Risk::Low,
            host_scope: HostScope::List { hosts: vec!["*.example.com".into()] },
            source: None,
        }
    }

    #[test]
    fn create_get_roundtrip() {
        let (store, _) = temp_store_at("skill-crud");
        let s = store.skill_create(input("列目录")).unwrap();
        assert!(!s.id.is_empty());
        assert_eq!(s.source, SkillSource::Manual);
        assert_eq!((s.success_count, s.fail_count, s.last_run_ts), (0, 0, None));
        assert_eq!(s.created_at, s.updated_at);
        // 带占位符的 exec 至少 Medium（自动抬高）
        assert_eq!(s.risk, Risk::Medium);
        let got = store.skill_get(&s.id).unwrap();
        assert_eq!(got, s);
        assert!(store.skill_get("nope").is_none());
    }

    #[test]
    fn create_rejects_invalid_and_never_stores_secret_values() {
        let (store, _) = temp_store_at("skill-invalid");
        let mut bad = input("x");
        bad.steps = vec![];
        let e = store.skill_create(bad).unwrap_err();
        assert!(e.contains("至少需要一个步骤"));
        let mut bad = input("x");
        bad.steps = vec![exec("export PASSWORD=hunter2")];
        assert!(store.skill_create(bad).unwrap_err().contains("口令"));
        assert!(store.skill_list(None).is_empty());
    }

    #[test]
    fn create_cannot_forge_stats() {
        let (store, _) = temp_store_at("skill-forge");
        let mut s = input("a").into_skill("forged".into(), 1);
        s.success_count = 99;
        s.last_run_ts = Some(5);
        let got = store.skill_insert(s).unwrap();
        assert_eq!(got.success_count, 0);
        assert_eq!(got.last_run_ts, None);
        assert!(store.skill_insert(got.clone()).unwrap_err().contains("已存在"));
    }

    #[test]
    fn risk_is_raised_on_write() {
        let (store, _) = temp_store_at("skill-risk");
        let mut i = input("danger");
        i.steps = vec![exec("sudo rm -rf /var/x")];
        i.risk = Risk::Low;
        let s = store.skill_create(i).unwrap();
        assert_eq!(s.risk, Risk::High);
        assert_eq!(store.skill_get(&s.id).unwrap().risk, Risk::High);
    }

    #[test]
    fn update_keeps_identity_and_stats_when_behavior_unchanged() {
        let (store, _) = temp_store_at("skill-update");
        let s = store.skill_create(input("a")).unwrap();
        store.skill_record_run_result(&s.id, true).unwrap();
        store.skill_record_run_result(&s.id, true).unwrap();
        store.skill_record_run_result(&s.id, false).unwrap();

        // 只改名称 / 描述：计数保留
        let mut i = input("a2");
        i.description = "新描述".into();
        i.source = Some(SkillSource::Imported); // 更新时忽略
        let u = store.skill_update(&s.id, i).unwrap();
        assert_eq!(u.id, s.id);
        assert_eq!(u.name, "a2");
        assert_eq!(u.source, SkillSource::Manual);
        assert_eq!(u.created_at, s.created_at);
        assert_eq!((u.success_count, u.fail_count), (2, 1));
        assert!(u.last_run_ts.is_some());
        assert_eq!(store.skill_get(&s.id).unwrap(), u);

        // 改了步骤：计数清零
        let mut i = input("a2");
        i.steps = vec![exec("ls -l {{dir}}")];
        let u = store.skill_update(&s.id, i).unwrap();
        assert_eq!((u.success_count, u.fail_count, u.last_run_ts), (0, 0, None));
        assert_eq!(u.steps, vec![exec("ls -l {{dir}}")]);

        // 改了适用范围同样清零
        store.skill_record_run_result(&s.id, true).unwrap();
        let mut i = input("a2");
        i.steps = vec![exec("ls -l {{dir}}")];
        i.host_scope = HostScope::Any;
        let u = store.skill_update(&s.id, i).unwrap();
        assert_eq!(u.success_count, 0);
    }

    #[test]
    fn update_errors() {
        let (store, _) = temp_store_at("skill-update-err");
        assert!(store.skill_update("nope", input("a")).unwrap_err().contains("不存在"));
        let s = store.skill_create(input("a")).unwrap();
        let mut bad = input("a");
        bad.name = "  ".into();
        assert!(store.skill_update(&s.id, bad).unwrap_err().contains("名称"));
        // 失败的更新不改动原数据
        assert_eq!(store.skill_get(&s.id).unwrap(), s);
    }

    #[test]
    fn delete_works() {
        let (store, _) = temp_store_at("skill-delete");
        let s = store.skill_create(input("a")).unwrap();
        assert!(store.skill_delete(&s.id).unwrap());
        assert!(!store.skill_delete(&s.id).unwrap());
        assert!(store.skill_get(&s.id).is_none());
    }

    #[test]
    fn list_orders_and_searches() {
        let (store, path) = temp_store_at("skill-list");
        let a = store.skill_create(input("Deploy Web")).unwrap();
        let b = store.skill_create(input("清理日志")).unwrap();
        let mut c_in = input("备份数据库");
        c_in.description = "每天 100% 备份_db".into();
        let c = store.skill_create(c_in).unwrap();
        // 把更新时间拉开，验证按 updated_at 倒序
        {
            let conn = Connection::open(&path).unwrap();
            for (id, ts) in [(&a.id, 100), (&b.id, 300), (&c.id, 200)] {
                conn.execute("UPDATE skills SET updated_at=?2 WHERE id=?1", params![id, ts]).unwrap();
            }
        }
        let names = |v: Vec<Skill>| v.into_iter().map(|s| s.name).collect::<Vec<_>>();
        assert_eq!(names(store.skill_list(None)), vec!["清理日志", "备份数据库", "Deploy Web"]);
        assert_eq!(names(store.skill_list(Some("  "))).len(), 3);
        // 子串、大小写不敏感（ASCII）
        assert_eq!(names(store.skill_list(Some("deploy"))), vec!["Deploy Web"]);
        assert_eq!(names(store.skill_list(Some("日志"))), vec!["清理日志"]);
        // 描述里搜
        assert_eq!(names(store.skill_list(Some("的描述"))).len(), 2);
        // LIKE 通配符按字面
        assert_eq!(names(store.skill_list(Some("100%"))), vec!["备份数据库"]);
        assert_eq!(names(store.skill_list(Some("_db"))), vec!["备份数据库"]);
        assert!(store.skill_list(Some("%")).len() == 1);
        assert!(store.skill_list(Some("不存在")).is_empty());
    }

    #[test]
    fn record_run_result_counts() {
        let (store, _) = temp_store_at("skill-run");
        let s = store.skill_create(input("a")).unwrap();
        store.skill_record_run_result(&s.id, true).unwrap();
        store.skill_record_run_result(&s.id, true).unwrap();
        store.skill_record_run_result(&s.id, false).unwrap();
        let g = store.skill_get(&s.id).unwrap();
        assert_eq!((g.success_count, g.fail_count), (2, 1));
        assert!(g.last_run_ts.is_some());
        assert_eq!(g.updated_at, s.updated_at, "运行不算编辑");
        assert!(store.skill_record_run_result("nope", true).unwrap_err().contains("不存在"));
    }

    #[test]
    fn caps_skill_count() {
        let (store, _) = temp_store_at("skill-cap");
        for i in 0..MAX_SKILLS {
            store.skill_create(input(&format!("s{i}"))).unwrap();
        }
        let e = store.skill_create(input("one-too-many")).unwrap_err();
        assert!(e.contains("上限"), "{e}");
        assert_eq!(store.skill_list(None).len(), MAX_SKILLS);
        // 删一个后又能建
        let first = store.skill_list(None).pop().unwrap();
        store.skill_delete(&first.id).unwrap();
        store.skill_create(input("fits")).unwrap();
    }

    #[test]
    fn corrupt_row_is_skipped_not_fatal() {
        let (store, path) = temp_store_at("skill-corrupt");
        let good = store.skill_create(input("good")).unwrap();
        let bad = store.skill_create(input("bad")).unwrap();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute("UPDATE skills SET steps='not json' WHERE id=?1", params![bad.id]).unwrap();
        }
        let list = store.skill_list(None);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, good.id);
        assert!(store.skill_get(&bad.id).is_none());
    }

    #[test]
    fn reopen_persists() {
        let (store, path) = temp_store_at("skill-reopen");
        let s = store.skill_create(input("a")).unwrap();
        store.skill_record_run_result(&s.id, true).unwrap();
        let want = store.skill_get(&s.id).unwrap();
        drop(store);
        let again = Store::open(path.parent().unwrap().to_path_buf()).unwrap();
        assert_eq!(again.skill_get(&s.id).unwrap(), want);
    }

    /// 旧库（没有 skills 表）打开后多出这张表，其它表原封不动；重复打开幂等。
    #[test]
    fn opening_old_db_adds_table_without_touching_others() {
        let dir = std::env::temp_dir().join(format!("nexus-store-skill-old-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nexus.db");
        {
            // 模拟 3.1 之前的库：只有 sessions / macros，各放一行
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (id TEXT PRIMARY KEY, name TEXT NOT NULL, folder TEXT NOT NULL DEFAULT '/',
                    host TEXT NOT NULL, port INTEGER NOT NULL DEFAULT 22, username TEXT NOT NULL DEFAULT '',
                    auth_type TEXT NOT NULL DEFAULT 'agent', key_path TEXT, startup_cmd TEXT, theme TEXT,
                    connect_timeout INTEGER, note TEXT, password TEXT, passphrase TEXT, totp_seed TEXT,
                    sort_order INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
                 CREATE TABLE macros (id TEXT PRIMARY KEY, name TEXT NOT NULL, steps TEXT NOT NULL DEFAULT '[]',
                    created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
                 INSERT INTO macros VALUES ('m1','旧宏','[]',1,2);
                 INSERT INTO sessions (id,name,host,created_at,updated_at) VALUES ('s1','旧会话','h',1,2);",
            )
            .unwrap();
            let has: i64 = conn
                .query_row("SELECT COUNT(*) FROM sqlite_master WHERE name='skills'", [], |r| r.get(0))
                .unwrap();
            assert_eq!(has, 0);
        }
        let store = Store::open(dir.clone()).unwrap();
        assert!(store.skill_list(None).is_empty());
        let s = store.skill_create(input("新")).unwrap();
        assert_eq!(store.skill_get(&s.id).unwrap().name, "新");
        drop(store);
        // 再开一次：幂等，数据仍在
        let store = Store::open(dir.clone()).unwrap();
        assert_eq!(store.skill_list(None).len(), 1);
        drop(store);
        let conn = Connection::open(&path).unwrap();
        let macro_name: String =
            conn.query_row("SELECT name FROM macros WHERE id='m1'", [], |r| r.get(0)).unwrap();
        let sess_name: String =
            conn.query_row("SELECT name FROM sessions WHERE id='s1'", [], |r| r.get(0)).unwrap();
        assert_eq!((macro_name.as_str(), sess_name.as_str()), ("旧宏", "旧会话"));
    }

    #[test]
    fn imported_skill_inserts_untrusted() {
        let (store, _) = temp_store_at("skill-import");
        let s = store.skill_create(input("a")).unwrap();
        store.skill_record_run_result(&s.id, true).unwrap();
        let text = skill::export_text(&store.skill_get(&s.id).unwrap());
        let imported = skill::import_text(&text, uuid::Uuid::new_v4().to_string(), now_secs()).unwrap();
        let saved = store.skill_insert(imported).unwrap();
        assert_ne!(saved.id, s.id);
        assert_eq!(saved.source, SkillSource::Imported);
        assert_eq!(saved.success_count, 0);
        assert_eq!(store.skill_list(None).len(), 2);
    }

    // ——— 4.4 信任阶梯 ———

    /// 只读（Low）技能输入：`ls -la`，无参数。
    fn ro_input(name: &str) -> SkillInput {
        SkillInput {
            name: name.into(),
            description: String::new(),
            params: vec![],
            steps: vec![exec("ls -la")],
            verify: None,
            risk: Risk::Low,
            host_scope: HostScope::Any,
            source: None,
        }
    }

    fn fp(store: &Store, id: &str) -> String {
        skill::trust_fingerprint(&store.skill_get(id).unwrap())
    }

    fn level(store: &Store, id: &str) -> skill::TrustLevel {
        skill::trust_info(&store.skill_get(id).unwrap()).level
    }

    /// 干净成功一次（带当前指纹）。
    fn clean_run(store: &Store, id: &str) {
        let f = fp(store, id);
        store.skill_record_run(id, true, true, &f).unwrap();
    }

    fn clean_runs(store: &Store, id: &str, n: u32) {
        for _ in 0..n {
            clean_run(store, id);
        }
    }

    #[test]
    fn ladder_three_clean_runs_earn_trust() {
        let (store, _) = temp_store_at("trust-ladder");
        let s = store.skill_create(ro_input("只读")).unwrap();
        assert_eq!(s.risk, Risk::Low);
        assert_eq!(level(&store, &s.id), skill::TrustLevel::Confirm);
        for n in 1..skill::TRUST_MIN_SUCCESSES {
            clean_run(&store, &s.id);
            assert_eq!(store.skill_get(&s.id).unwrap().trust_streak, n);
            assert_eq!(level(&store, &s.id), skill::TrustLevel::Confirm);
        }
        clean_run(&store, &s.id);
        let got = store.skill_get(&s.id).unwrap();
        assert_eq!(got.success_count, skill::TRUST_MIN_SUCCESSES);
        assert_eq!(skill::trust_info(&got).level, skill::TrustLevel::Trusted);
    }

    #[test]
    fn ladder_non_read_only_never_trusted() {
        let (store, _) = temp_store_at("trust-guarded");
        let s = store.skill_create(input("带参数")).unwrap();
        assert_eq!(s.risk, Risk::Medium);
        clean_runs(&store, &s.id, 10);
        assert_eq!(level(&store, &s.id), skill::TrustLevel::Guarded);
    }

    #[test]
    fn ladder_failure_drops_trust_and_counts() {
        let (store, _) = temp_store_at("trust-fail");
        let s = store.skill_create(ro_input("a")).unwrap();
        clean_runs(&store, &s.id, 3);
        assert_eq!(level(&store, &s.id), skill::TrustLevel::Trusted);
        store.skill_record_run(&s.id, false, false, &fp(&store, &s.id)).unwrap();
        let got = store.skill_get(&s.id).unwrap();
        assert_eq!((got.success_count, got.fail_count, got.trust_streak), (3, 1, 0));
        assert!(got.trust_hash.is_empty());
        assert_eq!(skill::trust_info(&got).level, skill::TrustLevel::Confirm);
        // 重新从头数
        clean_runs(&store, &s.id, 3);
        assert_eq!(level(&store, &s.id), skill::TrustLevel::Trusted);
    }

    #[test]
    fn ladder_success_with_deviation_does_not_count_and_resets() {
        let (store, _) = temp_store_at("trust-deviation");
        let s = store.skill_create(ro_input("a")).unwrap();
        clean_runs(&store, &s.id, 2);
        // 成功了，但有偏离（clean=false）：成功计数 +1，信任清零
        store.skill_record_run(&s.id, true, false, &fp(&store, &s.id)).unwrap();
        let got = store.skill_get(&s.id).unwrap();
        assert_eq!((got.success_count, got.trust_streak), (3, 0));
        assert_eq!(skill::trust_info(&got).level, skill::TrustLevel::Confirm);
    }

    #[test]
    fn ladder_record_run_result_wrapper_never_earns() {
        let (store, _) = temp_store_at("trust-wrapper");
        let s = store.skill_create(ro_input("a")).unwrap();
        for _ in 0..5 {
            store.skill_record_run_result(&s.id, true).unwrap();
        }
        let got = store.skill_get(&s.id).unwrap();
        assert_eq!((got.success_count, got.trust_streak), (5, 0));
        assert_eq!(skill::trust_info(&got).level, skill::TrustLevel::Confirm);
    }

    #[test]
    fn ladder_run_of_a_stale_fingerprint_does_not_earn() {
        let (store, _) = temp_store_at("trust-stale");
        let s = store.skill_create(ro_input("a")).unwrap();
        let before = fp(&store, &s.id);
        // 运行途中技能被编辑了
        let mut edited = ro_input("a");
        edited.steps = vec![exec("ls -l")];
        store.skill_update(&s.id, edited).unwrap();
        store.skill_record_run(&s.id, true, true, &before).unwrap();
        let got = store.skill_get(&s.id).unwrap();
        assert_eq!((got.success_count, got.trust_streak), (1, 0));
        // 空指纹同样不算
        store.skill_record_run(&s.id, true, true, "").unwrap();
        assert_eq!(store.skill_get(&s.id).unwrap().trust_streak, 0);
    }

    #[test]
    fn ladder_edit_of_behaviour_resets_trust() {
        let (store, _) = temp_store_at("trust-edit");
        let s = store.skill_create(ro_input("a")).unwrap();
        clean_runs(&store, &s.id, 3);
        assert_eq!(level(&store, &s.id), skill::TrustLevel::Trusted);

        // 改步骤
        let mut e = ro_input("a");
        e.steps = vec![exec("cat /etc/hostname")];
        let got = store.skill_update(&s.id, e).unwrap();
        assert_eq!((got.trust_streak, got.trust_hash.as_str()), (0, ""));
        assert_eq!(skill::trust_info(&got).level, skill::TrustLevel::Confirm);
        clean_runs(&store, &s.id, 3);
        assert_eq!(level(&store, &s.id), skill::TrustLevel::Trusted);

        // 放宽适用主机
        let mut e = ro_input("a");
        e.steps = vec![exec("cat /etc/hostname")];
        e.host_scope = HostScope::List { hosts: vec!["*.prod.example.com".into()] };
        let got = store.skill_update(&s.id, e.clone()).unwrap();
        assert_eq!(got.trust_streak, 0);
        clean_runs(&store, &s.id, 3);
        assert_eq!(level(&store, &s.id), skill::TrustLevel::Trusted);

        // 加验证条件
        e.verify = Some(SkillCheck { expect: "x".into(), timeout_ms: None });
        assert_eq!(store.skill_update(&s.id, e).unwrap().trust_streak, 0);
    }

    #[test]
    fn ladder_edit_of_name_or_description_keeps_trust() {
        let (store, _) = temp_store_at("trust-rename");
        let s = store.skill_create(ro_input("a")).unwrap();
        clean_runs(&store, &s.id, 3);
        let mut e = ro_input("改名了");
        e.description = "新的描述".into();
        let got = store.skill_update(&s.id, e).unwrap();
        assert_eq!(got.trust_streak, 3);
        assert_eq!(skill::trust_info(&got).level, skill::TrustLevel::Trusted);
    }

    #[test]
    fn ladder_risk_edit_resets_trust() {
        let (store, _) = temp_store_at("trust-risk");
        let s = store.skill_create(ro_input("a")).unwrap();
        clean_runs(&store, &s.id, 3);
        let mut e = ro_input("a");
        e.risk = Risk::High;
        let got = store.skill_update(&s.id, e).unwrap();
        assert_eq!(got.trust_streak, 0);
        assert_eq!(skill::trust_info(&got).level, skill::TrustLevel::Guarded);
    }

    #[test]
    fn ladder_import_and_insert_start_untrusted_even_with_forged_fields() {
        let (store, _) = temp_store_at("trust-insert");
        let mut forged = ro_input("forged").into_skill("forged-id".into(), 1);
        forged.trust_streak = 99;
        forged.trust_hash = skill::trust_fingerprint(&forged);
        let got = store.skill_insert(forged).unwrap();
        assert_eq!((got.trust_streak, got.trust_hash.as_str()), (0, ""));
        assert_eq!(skill::trust_info(&got).level, skill::TrustLevel::Confirm);

        // 已受信的技能导出 -> 导入：不带信任
        clean_runs(&store, &got.id, 3);
        assert_eq!(level(&store, &got.id), skill::TrustLevel::Trusted);
        let text = skill::export_text(&store.skill_get(&got.id).unwrap());
        let imported = skill::import_text(&text, "imp".into(), 5).unwrap();
        let saved = store.skill_insert(imported).unwrap();
        assert_eq!(skill::trust_info(&saved).level, skill::TrustLevel::Confirm);
    }

    #[test]
    fn ladder_drop_trust_keeps_counts() {
        let (store, _) = temp_store_at("trust-drop");
        let s = store.skill_create(ro_input("a")).unwrap();
        clean_runs(&store, &s.id, 3);
        store.skill_drop_trust(&s.id).unwrap();
        let got = store.skill_get(&s.id).unwrap();
        assert_eq!((got.success_count, got.fail_count, got.trust_streak), (3, 0, 0));
        assert_eq!(skill::trust_info(&got).level, skill::TrustLevel::Confirm);
    }

    #[test]
    fn ladder_trust_survives_reopen() {
        let (store, path) = temp_store_at("trust-reopen");
        let s = store.skill_create(ro_input("a")).unwrap();
        clean_runs(&store, &s.id, 3);
        drop(store);
        let store = Store::open(path.parent().unwrap().to_path_buf()).unwrap();
        assert_eq!(level(&store, &s.id), skill::TrustLevel::Trusted);
    }

    #[test]
    fn old_skills_table_gets_trust_columns_and_starts_untrusted() {
        let (store, path) = temp_store_at("trust-migrate");
        let s = store.skill_create(ro_input("旧")).unwrap();
        drop(store);
        // 模拟 4.4 之前的库：去掉两列
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute("ALTER TABLE skills DROP COLUMN trust_streak", []).unwrap();
            conn.execute("ALTER TABLE skills DROP COLUMN trust_hash", []).unwrap();
        }
        let store = Store::open(path.parent().unwrap().to_path_buf()).unwrap();
        let got = store.skill_get(&s.id).unwrap();
        assert_eq!((got.trust_streak, got.trust_hash.as_str()), (0, ""));
        assert_eq!(skill::trust_info(&got).level, skill::TrustLevel::Confirm);
        // 之后可以正常挣信任
        clean_runs(&store, &s.id, 3);
        assert_eq!(level(&store, &s.id), skill::TrustLevel::Trusted);
    }
}
