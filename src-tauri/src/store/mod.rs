use rusqlite::{params, Connection, Result as SqlResult};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
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
                sort_order   INTEGER NOT NULL DEFAULT 0,
                created_at   INTEGER NOT NULL,
                updated_at   INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_sessions_folder ON sessions(folder);

            CREATE TABLE IF NOT EXISTS settings (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
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

    // 预留给下一阶段：持久化主题、字体、布局比例等外观设置
    #[allow(dead_code)]
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

    #[allow(dead_code)]
    pub fn get_setting(&self, k: &str) -> Option<String> {
        let conn = self.conn.lock().ok()?;
        conn.query_row("SELECT value FROM settings WHERE key = ?1", params![k], |r| {
            r.get(0)
        })
        .ok()
    }
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

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Store {
    /// 一次性迁移：把旧版本存在系统凭据服务里的密码搬进 db 的明文列。
    /// 只对 db 里 password 为空、但凭据服务里有值的会话执行；搬完即从凭据服务删除。
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
