use rusqlite::{params, Connection, Result as SqlResult};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRecord {
    pub id: String,
    pub name: String,
    pub folder: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    /// password | key | agent
    pub auth_type: String,
    /// 引用 ~/.ssh 下的私钥路径，仅当 auth_type = key 时有效
    pub key_path: Option<String>,
    pub startup_cmd: Option<String>,
    pub theme: Option<String>,
    /// TCP 连接超时（秒）。NULL 表示沿用 `errors::DEFAULT_CONNECT_TIMEOUT_SECS`。
    pub connect_timeout: Option<u32>,
    pub sort_order: i32,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSession {
    pub name: String,
    pub folder: Option<String>,
    pub host: String,
    pub port: Option<u16>,
    pub username: String,
    pub auth_type: String,
    pub key_path: Option<String>,
    pub startup_cmd: Option<String>,
    pub theme: Option<String>,
    pub connect_timeout: Option<u32>,
}

pub struct Store {
    conn: Mutex<Connection>,
}

/// 宏里的一步。
///
/// 三种原语刻意做得很少 —— 宏要的是「可预测」，不是编程语言。真正的灵活性
/// 来自 `Wait` 的三种用法：等某个模式出现、等输出静默（pattern 为空）、
/// 以及 `Sleep` 的兜底延时。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum MacroStep {
    /// 发送文本。`enter` 默认 true：绝大多数步骤就是「敲这一行然后回车」。
    Send {
        text: String,
        #[serde(default = "default_true")]
        enter: bool,
    },
    /// 等屏幕出现 `pattern`；`pattern` 为空表示「等输出静默」。
    Wait {
        #[serde(default)]
        pattern: String,
        /// 超时（毫秒）。0 = 用引擎默认值。
        #[serde(default, rename = "timeoutMs")]
        timeout_ms: u32,
    },
    /// 无条件等待。用在「服务端不输出任何东西，但必须等它准备好」的场合。
    Sleep { ms: u32 },
}

fn default_true() -> bool {
    true
}

/// 一条宏。`steps` 在库里是 JSON 文本，进出都走 serde（读取时坏数据会退化成空步骤，见 `row_to_macro`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MacroRecord {
    pub id: String,
    pub name: String,
    pub steps: Vec<MacroStep>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 密钥库里一条密钥的元数据（私钥文件本身在 `<app_data>/keys/<id>`）。
#[derive(Debug, Clone)]
pub struct KeyRow {
    pub id: String,
    pub name: String,
    pub path: String,
    /// 归一化后的类型名：RSA / ED25519 / ECDSA
    pub key_type: String,
    /// 密钥长度（位）。无法判定时为 0
    pub bits: u32,
    pub comment: String,
    pub fingerprint: String,
    pub encrypted: bool,
    pub created_at: i64,
}

/// 追加列的轻量迁移。
///
/// SQLite 没有 `ADD COLUMN IF NOT EXISTS`，且 `CREATE TABLE IF NOT EXISTS`
/// 对已经存在的旧库不会补列，必须显式检查 pragma。
/// 每次新增列都在这里加一条，旧库升级后即可平滑读取。
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    for (column, ddl) in [
        ("connect_timeout", "ALTER TABLE sessions ADD COLUMN connect_timeout INTEGER"),
        // 密码明文列：为了跨机器迁移连接配置，密码直接随 db 保存
        ("password", "ALTER TABLE sessions ADD COLUMN password TEXT"),
        // 私钥口令（passphrase）。与密码同理随 db 保存 —— 它只是「解开本地私钥文件」
        // 的口令，不是服务器凭据；不存的话每次连接都要重输一遍。
        ("passphrase", "ALTER TABLE sessions ADD COLUMN passphrase TEXT"),
    ] {
        let exists: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('sessions') WHERE name = ?1")?
            .exists(params![column])?;
        if !exists {
            conn.execute(ddl, [])?;
        }
    }
    Ok(())
}

impl Store {
    pub fn open(base: PathBuf) -> Result<Self, String> {
        fs::create_dir_all(&base).map_err(|e| e.to_string())?;
        let path = base.join("nexus.db");
        let conn = Connection::open(&path).map_err(|e| e.to_string())?;

        // 会话库含主机名与连接配置，必须限制为仅所有者可读写
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).ok();
        }

        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS sessions (
                id           TEXT PRIMARY KEY,
                name         TEXT NOT NULL,
                folder       TEXT NOT NULL DEFAULT '/',
                host         TEXT NOT NULL,
                port         INTEGER NOT NULL DEFAULT 22,
                username     TEXT NOT NULL DEFAULT '',
                auth_type    TEXT NOT NULL DEFAULT 'agent',
                key_path     TEXT,
                startup_cmd  TEXT,
                theme        TEXT,
                connect_timeout INTEGER,
                password     TEXT,
                passphrase   TEXT,
                sort_order   INTEGER NOT NULL DEFAULT 0,
                created_at   INTEGER NOT NULL,
                updated_at   INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_sessions_folder ON sessions(folder);

            CREATE TABLE IF NOT EXISTS settings (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            -- 用户密钥库的元数据。私钥文件本身放在 <app_data>/keys/<id>，
            -- 这里只记展示信息：名字、类型、长度、注释、指纹。
            -- path 冗余存一份，会话直接引用它即可，不必再回表。
            CREATE TABLE IF NOT EXISTS ssh_keys (
                id          TEXT PRIMARY KEY,
                name        TEXT NOT NULL,
                path        TEXT NOT NULL,
                key_type    TEXT NOT NULL,
                bits        INTEGER NOT NULL DEFAULT 0,
                comment     TEXT NOT NULL DEFAULT '',
                fingerprint TEXT NOT NULL DEFAULT '',
                encrypted   INTEGER NOT NULL DEFAULT 0,
                created_at  INTEGER NOT NULL
            );

            -- 宏：一串「等什么、发什么」的操作，可以一键或连接后自动重放。
            -- steps 存 JSON 数组。之所以不建 steps 子表：单条宏的步骤数很少、
            -- 从不需要按步骤查询，而步骤结构还会继续演进（以后可能有条件分支），
            -- 子表会逼着做频繁的列迁移。
            CREATE TABLE IF NOT EXISTS macros (
                id         TEXT PRIMARY KEY,
                name       TEXT NOT NULL,
                steps      TEXT NOT NULL DEFAULT '[]',
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            -- 终端自动补全用的「session 级」命令历史。key 是会话身份
            -- （已保存连接配置的 id，本地固定 'local'），不是临时的标签页 id ——
            -- 这样同一台服务器下次重连、甚至重启应用后新开的标签，都能接着用
            -- 之前在这台机器上敲过的命令做补全。
            CREATE TABLE IF NOT EXISTS command_history (
                key          TEXT NOT NULL,
                command      TEXT NOT NULL,
                use_count    INTEGER NOT NULL DEFAULT 1,
                last_used_at INTEGER NOT NULL,
                PRIMARY KEY (key, command)
            );
            CREATE INDEX IF NOT EXISTS idx_history_key_time ON command_history(key, last_used_at);
            "#,
        )
        .map_err(|e| e.to_string())?;

        migrate(&conn).map_err(|e| format!("数据库迁移失败: {}", e))?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn list(&self) -> Result<Vec<SessionRecord>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT id,name,folder,host,port,username,auth_type,key_path,
                        startup_cmd,theme,connect_timeout,sort_order,created_at,updated_at
                 FROM sessions ORDER BY folder, sort_order, name",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], row_to_session)
            .map_err(|e| e.to_string())?
            .collect::<SqlResult<Vec<_>>>()
            .map_err(|e| e.to_string())?;
        Ok(rows)
    }

    pub fn create(&self, s: NewSession) -> Result<SessionRecord, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_secs();
        let max_order: i32 = conn
            .query_row("SELECT COALESCE(MAX(sort_order),0) FROM sessions", [], |r| r.get(0))
            .unwrap_or(0);

        conn.execute(
            "INSERT INTO sessions (id,name,folder,host,port,username,auth_type,key_path,
                                   startup_cmd,theme,connect_timeout,sort_order,created_at,updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                id,
                s.name,
                s.folder.unwrap_or_else(|| "/".into()),
                s.host,
                s.port.unwrap_or(22),
                s.username,
                s.auth_type,
                s.key_path,
                s.startup_cmd,
                s.theme,
                s.connect_timeout,
                max_order + 1,
                now,
                now,
            ],
        )
        .map_err(|e| e.to_string())?;
        drop(conn);
        self.get(&id)
    }

    pub fn get(&self, id: &str) -> Result<SessionRecord, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.query_row(
            "SELECT id,name,folder,host,port,username,auth_type,key_path,
                    startup_cmd,theme,connect_timeout,sort_order,created_at,updated_at
             FROM sessions WHERE id = ?1",
            params![id],
            row_to_session,
        )
        .map_err(|_| format!("会话不存在: {}", id))
    }

    pub fn update(&self, id: &str, s: NewSession) -> Result<SessionRecord, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "UPDATE sessions SET name=?2, folder=?3, host=?4, port=?5, username=?6,
                    auth_type=?7, key_path=?8, startup_cmd=?9, theme=?10,
                    connect_timeout=?11, updated_at=?12
             WHERE id=?1",
            params![
                id,
                s.name,
                s.folder.unwrap_or_else(|| "/".into()),
                s.host,
                s.port.unwrap_or(22),
                s.username,
                s.auth_type,
                s.key_path,
                s.startup_cmd,
                s.theme,
                s.connect_timeout,
                now_secs(),
            ],
        )
        .map_err(|e| e.to_string())?;
        drop(conn);
        self.get(id)
    }

    pub fn delete(&self, id: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    // ───────────────────────── 宏 ─────────────────────────

    pub fn list_macros(&self) -> Result<Vec<MacroRecord>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("SELECT id,name,steps,created_at,updated_at FROM macros ORDER BY created_at")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], row_to_macro)
            .map_err(|e| e.to_string())?
            .collect::<SqlResult<Vec<_>>>()
            .map_err(|e| e.to_string())?;
        Ok(rows)
    }

    pub fn create_macro(&self, name: &str, steps: &[MacroStep]) -> Result<MacroRecord, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("宏名称不能为空".into());
        }
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_secs();
        let json = serde_json::to_string(steps).map_err(|e| format!("步骤序列化失败: {e}"))?;
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO macros (id,name,steps,created_at,updated_at) VALUES (?1,?2,?3,?4,?5)",
            params![id, name, json, now, now],
        )
        .map_err(|e| e.to_string())?;
        drop(conn);
        self.get_macro(&id)
    }

    pub fn get_macro(&self, id: &str) -> Result<MacroRecord, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.query_row(
            "SELECT id,name,steps,created_at,updated_at FROM macros WHERE id = ?1",
            params![id],
            row_to_macro,
        )
        .map_err(|_| format!("宏不存在: {}", id))
    }

    pub fn update_macro(
        &self,
        id: &str,
        name: &str,
        steps: &[MacroStep],
    ) -> Result<MacroRecord, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("宏名称不能为空".into());
        }
        let json = serde_json::to_string(steps).map_err(|e| format!("步骤序列化失败: {e}"))?;
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let n = conn
            .execute(
                "UPDATE macros SET name=?2, steps=?3, updated_at=?4 WHERE id=?1",
                params![id, name, json, now_secs()],
            )
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err(format!("宏不存在: {}", id));
        }
        drop(conn);
        self.get_macro(id)
    }

    pub fn delete_macro(&self, id: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute("DELETE FROM macros WHERE id = ?1", params![id])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// 单个会话身份最多保留多少条历史。超出的按最近使用时间裁掉。
    const HISTORY_CAP: i64 = 300;

    /// 记一条命令：已存在就顶到最近并计数，否则新插一条；随后裁掉超出
    /// `HISTORY_CAP` 的那部分 —— 裁剪放在同一次调用里，调用方不必关心配额。
    pub fn record_history(&self, key: &str, command: &str) -> Result<(), String> {
        let command = command.trim();
        if command.is_empty() {
            return Ok(());
        }
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO command_history (key, command, use_count, last_used_at)
             VALUES (?1, ?2, 1, ?3)
             ON CONFLICT(key, command) DO UPDATE SET
                use_count = use_count + 1,
                last_used_at = excluded.last_used_at",
            params![key, command, now_ms()],
        )
        .map_err(|e| e.to_string())?;
        conn.execute(
            "DELETE FROM command_history WHERE key = ?1 AND command NOT IN (
                SELECT command FROM command_history WHERE key = ?1
                ORDER BY last_used_at DESC LIMIT ?2
             )",
            params![key, Self::HISTORY_CAP],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// 取某个会话身份的历史，按最近使用时间倒序。
    pub fn list_history(&self, key: &str) -> Result<Vec<String>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT command FROM command_history WHERE key = ?1
                 ORDER BY last_used_at DESC LIMIT ?2",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![key, Self::HISTORY_CAP], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .collect::<SqlResult<Vec<_>>>()
            .map_err(|e| e.to_string())?;
        Ok(rows)
    }

    /// 保存明文密码（随 db 持久化，便于跨机器迁移）。
    pub fn set_password(&self, id: &str, password: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "UPDATE sessions SET password = ?2 WHERE id = ?1",
            params![id, password],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// 读取明文密码。
    pub fn get_password(&self, id: &str) -> Option<String> {
        let conn = self.conn.lock().ok()?;
        conn.query_row(
            "SELECT password FROM sessions WHERE id = ?1",
            params![id],
            |r| r.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
        .filter(|p| !p.is_empty())
    }

    /// 保存私钥口令。空串视为「清除」——避免把空口令当成有效口令传给解密流程。
    pub fn set_passphrase(&self, id: &str, passphrase: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "UPDATE sessions SET passphrase = ?2 WHERE id = ?1",
            params![id, passphrase],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn get_passphrase(&self, id: &str) -> Option<String> {
        let conn = self.conn.lock().ok()?;
        conn.query_row(
            "SELECT passphrase FROM sessions WHERE id = ?1",
            params![id],
            |r| r.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
        .filter(|p| !p.is_empty())
    }

    // ——— 密钥库 ———

    pub fn list_keys(&self) -> Vec<KeyRow> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let Ok(mut stmt) = conn.prepare(
            "SELECT id, name, path, key_type, bits, comment, fingerprint, encrypted, created_at
             FROM ssh_keys ORDER BY name COLLATE NOCASE",
        ) else {
            return Vec::new();
        };
        stmt.query_map([], |r| {
            Ok(KeyRow {
                id: r.get(0)?,
                name: r.get(1)?,
                path: r.get(2)?,
                key_type: r.get(3)?,
                bits: r.get::<_, i64>(4)? as u32,
                comment: r.get(5)?,
                fingerprint: r.get(6)?,
                encrypted: r.get::<_, i64>(7)? != 0,
                created_at: r.get(8)?,
            })
        })
        .map(|it| it.flatten().collect())
        .unwrap_or_default()
    }

    pub fn get_key(&self, id: &str) -> Option<KeyRow> {
        let conn = self.conn.lock().ok()?;
        conn.query_row(
            "SELECT id, name, path, key_type, bits, comment, fingerprint, encrypted, created_at
             FROM ssh_keys WHERE id = ?1",
            params![id],
            |r| {
                Ok(KeyRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    path: r.get(2)?,
                    key_type: r.get(3)?,
                    bits: r.get::<_, i64>(4)? as u32,
                    comment: r.get(5)?,
                    fingerprint: r.get(6)?,
                    encrypted: r.get::<_, i64>(7)? != 0,
                    created_at: r.get(8)?,
                })
            },
        )
        .ok()
    }

    pub fn insert_key(&self, k: &KeyRow) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO ssh_keys (id, name, path, key_type, bits, comment, fingerprint, encrypted, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                k.id,
                k.name,
                k.path,
                k.key_type,
                k.bits as i64,
                k.comment,
                k.fingerprint,
                if k.encrypted { 1 } else { 0 },
                k.created_at
            ],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn delete_key(&self, id: &str) -> Option<KeyRow> {
        let row = self.get_key(id)?;
        let conn = self.conn.lock().ok()?;
        conn.execute("DELETE FROM ssh_keys WHERE id = ?1", params![id])
            .ok()?;
        Some(row)
    }

    pub fn update_key_meta(&self, id: &str, name: Option<&str>, comment: Option<&str>) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        if let Some(n) = name {
            conn.execute("UPDATE ssh_keys SET name = ?2 WHERE id = ?1", params![id, n])
                .map_err(|e| e.to_string())?;
        }
        if let Some(c) = comment {
            conn.execute("UPDATE ssh_keys SET comment = ?2 WHERE id = ?1", params![id, c])
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// 批量更新排序与分组归属（拖拽后持久化）。
    pub fn reorder(&self, items: &[(String, i32, String)]) -> Result<(), String> {
        let mut conn = self.conn.lock().map_err(|e| e.to_string())?;
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        let now = now_secs();
        for (id, sort_order, folder) in items {
            tx.execute(
                "UPDATE sessions SET sort_order = ?2, folder = ?3, updated_at = ?4 WHERE id = ?1",
                params![id, sort_order, folder, now],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(())
    }

    // AI 助手配置、主题、字体、布局比例等外观设置都走这张表
    pub fn set_setting(&self, k: &str, v: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO settings (key,value) VALUES (?1,?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![k, v],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn get_setting(&self, k: &str) -> Option<String> {
        let conn = self.conn.lock().ok()?;
        conn.query_row("SELECT value FROM settings WHERE key = ?1", params![k], |r| {
            r.get(0)
        })
        .ok()
    }

    /// 所有会话的附加状态（一次取全，前端启动时缓存，省掉每次连接一次 IPC）。
    pub fn all_extras(&self) -> HashMap<String, SessionExtras> {
        let mut out = HashMap::new();
        let Ok(conn) = self.conn.lock() else {
            return out;
        };
        let Ok(mut stmt) = conn.prepare("SELECT key, value FROM settings WHERE key LIKE 'extras:%'")
        else {
            return out;
        };
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        });
        let Ok(rows) = rows else { return out };
        for row in rows.flatten() {
            let (key, value) = row;
            let id = key[EXTRAS_PREFIX.len()..].to_string();
            if let Ok(e) = serde_json::from_str::<SessionExtras>(&value) {
                out.insert(id, e);
            }
        }
        out
    }

    pub fn set_extras(&self, id: &str, e: &SessionExtras) -> Result<(), String> {
        let json = serde_json::to_string(e).map_err(|e| e.to_string())?;
        self.set_setting(&format!("{EXTRAS_PREFIX}{id}"), &json)
    }
}

const EXTRAS_PREFIX: &str = "extras:";

/// 会话配置的附加状态：连接后自动执行的宏 + 兼容模式。
///
/// 存在 `settings` 表而不是 `sessions` 的列，原因有二：
/// ① 这类数据是「本机学到的行为」，不该跟着导入导出跑到别人机器上；
/// ② 省掉一次 session 表列迁移（那条路径要动 4 处 SELECT 语句）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionExtras {
    /// **旧版**「登录后自动输入」，只为迁移保留：前端启动时会把它转成一条宏
    /// 并清空，之后不再读写。
    #[serde(default)]
    pub login_script: String,
    #[serde(default)]
    pub login_script_on: bool,
    /// 连接后自动执行的宏 id（空串 = 不执行）。宏本体在 `macros` 表
    #[serde(default)]
    pub macro_id: String,
    /// 是否真的自动执行（关掉但保留选择，方便临时禁用）
    #[serde(default)]
    pub macro_on: bool,
    /// 兼容模式：不在该连接上开旁路通道（堡垒机只放行一条通道）
    #[serde(default)]
    pub compat: bool,
}

fn row_to_session(r: &rusqlite::Row) -> SqlResult<SessionRecord> {
    Ok(SessionRecord {
        id: r.get(0)?,
        name: r.get(1)?,
        folder: r.get(2)?,
        host: r.get(3)?,
        port: r.get(4)?,
        username: r.get(5)?,
        auth_type: r.get(6)?,
        key_path: r.get(7)?,
        startup_cmd: r.get(8)?,
        theme: r.get(9)?,
        connect_timeout: r.get(10)?,
        sort_order: r.get(11)?,
        created_at: r.get(12)?,
        updated_at: r.get(13)?,
    })
}

fn row_to_macro(r: &rusqlite::Row) -> SqlResult<MacroRecord> {
    // 步骤是 JSON 文本。解析失败说明库被外部改坏了：退化成空步骤而不是
    // 让整个宏列表加载失败 —— 用户至少还能看到这条宏并改写它。
    let raw: String = r.get(2)?;
    let steps: Vec<MacroStep> = serde_json::from_str(&raw).unwrap_or_default();
    Ok(MacroRecord {
        id: r.get(0)?,
        name: r.get(1)?,
        steps,
        created_at: r.get(3)?,
        updated_at: r.get(4)?,
    })
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 毫秒精度的时间戳，专给 `command_history.last_used_at` 用——秒级精度在短时间内
/// 连续敲好几条命令时会打平，`ORDER BY last_used_at DESC` 就排不出正确顺序。
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Store {
    /// 一次性迁移：把旧版本存在系统凭据服务里的密码搬进 db 的明文列（仅当 db 里为空），搬完即从凭据服务删除。
    pub fn migrate_legacy_secrets(&self) {
        let ids: Vec<String> = {
            let conn = match self.conn.lock() {
                Ok(c) => c,
                Err(_) => return,
            };
            let mut stmt = match conn.prepare(
                "SELECT id FROM sessions WHERE password IS NULL OR password = ''",
            ) {
                Ok(s) => s,
                Err(_) => return,
            };
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map(|it| it.flatten().collect::<Vec<_>>())
                .unwrap_or_default();
            rows
        };
        for id in ids {
            if let Some(pw) = legacy_keyring::load(&id) {
                if !pw.is_empty() {
                    let _ = self.set_password(&id, &pw);
                    legacy_keyring::delete(&id);
                }
            }
        }
    }
}

/// 旧版密码存储（系统凭据服务）—— 仅用于一次性迁移到明文 db。
mod legacy_keyring {
    const SERVICE: &str = "com.nexus-term.sessions";

    pub fn load(session_id: &str) -> Option<String> {
        let entry = keyring::Entry::new(SERVICE, session_id).ok()?;
        entry.get_password().ok()
    }

    pub fn delete(session_id: &str) {
        if let Ok(entry) = keyring::Entry::new(SERVICE, session_id) {
            entry.delete_credential().ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str) -> Store {
        let dir = std::env::temp_dir().join(format!(
            "nexus-store-{tag}-{}",
            uuid::Uuid::new_v4()
        ));
        Store::open(dir).expect("打开临时库")
    }

    /// 步骤的 JSON 形状是和前端共用的契约，改字段名必须两边一起改 ——
    /// 这里把它钉死，避免「Rust 编译通过、前端读不到 timeoutMs」这种静默错位。
    #[test]
    fn macro_step_json_shape_is_stable() {
        let steps = vec![
            MacroStep::Send { text: "6".into(), enter: true },
            MacroStep::Wait { pattern: "[Host]>".into(), timeout_ms: 20_000 },
            MacroStep::Sleep { ms: 300 },
        ];
        let json = serde_json::to_value(&steps).unwrap();
        assert_eq!(
            json,
            serde_json::json!([
                { "kind": "send", "text": "6", "enter": true },
                { "kind": "wait", "pattern": "[Host]>", "timeoutMs": 20000 },
                { "kind": "sleep", "ms": 300 }
            ])
        );
        let back: Vec<MacroStep> = serde_json::from_value(json).unwrap();
        assert_eq!(back, steps);
    }

    /// `enter` 缺省为 true：手写的旧数据 / 前端省略该字段时仍然是「发完回车」。
    #[test]
    fn send_defaults_to_enter() {
        let s: Vec<MacroStep> =
            serde_json::from_str(r#"[{"kind":"send","text":"ls"}]"#).unwrap();
        assert_eq!(s[0], MacroStep::Send { text: "ls".into(), enter: true });
    }

    #[test]
    fn macro_crud_roundtrip() {
        let store = temp_store("macro");
        let steps = vec![
            MacroStep::Wait { pattern: String::new(), timeout_ms: 0 },
            MacroStep::Send { text: "2".into(), enter: true },
        ];

        let m = store.create_macro("选第二台", &steps).unwrap();
        assert_eq!(m.name, "选第二台");
        assert_eq!(m.steps, steps);
        assert_eq!(store.list_macros().unwrap().len(), 1);

        let updated = store.update_macro(&m.id, "选第三台", &steps[1..]).unwrap();
        assert_eq!(updated.name, "选第三台");
        assert_eq!(updated.steps.len(), 1);

        store.delete_macro(&m.id).unwrap();
        assert!(store.list_macros().unwrap().is_empty());
        assert!(store.get_macro(&m.id).is_err());
    }

    #[test]
    fn macro_rejects_empty_name() {
        let store = temp_store("macro-name");
        assert!(store.create_macro("   ", &[]).is_err());
        let m = store.create_macro("ok", &[]).unwrap();
        assert!(store.update_macro(&m.id, "", &[]).is_err());
        // 名字非法时不该顺手把记录改坏
        assert_eq!(store.get_macro(&m.id).unwrap().name, "ok");
    }

    #[test]
    fn history_record_and_list_roundtrip() {
        let store = temp_store("history");
        store.record_history("host-1", "ls -la").unwrap();
        store.record_history("host-1", "uptime").unwrap();
        // 不同 key 之间互不可见
        store.record_history("host-2", "whoami").unwrap();
        assert_eq!(
            store.list_history("host-1").unwrap(),
            vec!["uptime".to_string(), "ls -la".to_string()]
        );
        assert_eq!(store.list_history("host-2").unwrap(), vec!["whoami".to_string()]);
    }

    #[test]
    fn history_same_command_bumps_to_recent_instead_of_duplicating() {
        let store = temp_store("history-bump");
        store.record_history("k", "ls").unwrap();
        store.record_history("k", "pwd").unwrap();
        store.record_history("k", "ls").unwrap(); // 再敲一次 ls：应该顶到最近，不是新增一条
        let items = store.list_history("k").unwrap();
        assert_eq!(items, vec!["ls".to_string(), "pwd".to_string()]);
    }

    /// 超过单 key 的 300 条上限要被裁掉 —— 用构造好的时间戳直接写库，
    /// 避免同一秒内调用 300 次 `record_history` 时 `last_used_at` 打平、排序不确定。
    #[test]
    fn history_prunes_beyond_cap() {
        let store = temp_store("history-cap");
        {
            let conn = store.conn.lock().unwrap();
            for i in 0..320i64 {
                conn.execute(
                    "INSERT INTO command_history (key, command, use_count, last_used_at)
                     VALUES ('k', ?1, 1, ?2)",
                    params![format!("cmd-{i}"), i],
                )
                .unwrap();
            }
        }
        // 这次记录会触发裁剪：命中一条已存在的命令，顺带把超额的老记录删掉
        store.record_history("k", "cmd-319").unwrap();
        let items = store.list_history("k").unwrap();
        assert_eq!(items.len(), 300);
        // 保留的必须是最近使用的那 300 条（cmd-20..=cmd-319），最老的 cmd-0..=cmd-19 被裁掉
        assert!(items.contains(&"cmd-319".to_string()));
        assert!(!items.contains(&"cmd-0".to_string()));
    }
}
