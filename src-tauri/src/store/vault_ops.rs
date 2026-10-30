//! 保险库的启用 / 解锁 / 换密码 / 关闭，以及把库里的明文凭据改写成密文的全量扫描。
//!
//! ## 三条贯穿始终的规矩
//!
//! **一、元数据必须先于密文落盘。** `vault.kdf` 和 `vault.dek` 要在**任何一行被
//! 改写成密文之前**提交。顺序反了而中途崩溃，留下的就是一堆再也解不开的密文。
//! 启用因此分三段事务：写元数据 → 全量改写 → 清掉「进行中」标记。
//!
//! **二、加解密一律在 `conn` 锁之外做。** 那把锁不可重入，锁序永远是
//! 「先 vault 后 conn」。所以扫描是「读出来 → 放锁 → 在内存里加密 → 再拿锁写回」，
//! 事务里一句密码学调用都没有。
//!
//! **三、宁可干净地失败，也不要降级。** 加密结果在写回之前先解回来比对一遍；
//! 对不上就整个中止，一个字节都不碰 db。关闭保险库时如果系统凭据服务不可用、
//! TOTP 种子没法转回 v1，直接报错，**绝不降级存明文**。

// 命令层还没接上，所以这里大半还没有调用方。**接线之后请删掉这一行。**
#![allow(dead_code)]

use rusqlite::{params, Connection};
use serde::Serialize;

use super::Store;
use crate::vault::{self, remember, Field, Form, KdfSpec, Vault, VaultMeta};

/// `settings` 表里的键。全部跟着 db 走 —— 任何一项挪进系统凭据服务都会让
/// 「拷到另一台机器输同一个主密码就能打开」失效，而那是这个功能的全部意义。
const K_ID: &str = "vault.id";
const K_KDF: &str = "vault.kdf";
const K_DEK: &str = "vault.dek";
const K_VERIFY: &str = "vault.verify";
const K_MIGRATING: &str = "vault.migrating";

/// 预留给将来的恢复码：第二个 KEK 包装同一把 DEK。
///
/// 现在**不实现**。恢复码的现实归宿是和 nexus.db 一起躺在同一个同步目录里 ——
/// 锁旁边挂着钥匙。真正的逃生口是解锁状态下导出一份 .xsh。留着这个键名是为了
/// 将来真要做时它是个向后兼容的新增，不需要动版本号。
#[allow(dead_code)]
const K_RECOVERY: &str = "vault.dek.recovery";

const UPSERT: &str = "INSERT INTO settings (key,value) VALUES (?1,?2)
                      ON CONFLICT(key) DO UPDATE SET value = excluded.value";

/// 一次扫描动了些什么。启用之后要把它如实报给用户。
#[derive(Debug, Default, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SweepReport {
    /// 改写了的密码 / 私钥口令条数。
    pub credentials: usize,
    /// 从「本机密钥」换挂到主密钥的 TOTP 种子数。
    pub seeds_converted: usize,
    /// 解不开、原样留着的 TOTP 种子数 —— 这些要让用户重填。
    pub seeds_unreadable: usize,
}

/// 开库时判定保险库状态。**只查一次 settings，不做 KDF、不碰系统凭据服务。**
pub(super) fn probe(conn: &Connection) -> Vault {
    let get = |k: &str| -> Option<String> {
        conn.query_row("SELECT value FROM settings WHERE key = ?1", params![k], |r| {
            r.get::<_, String>(0)
        })
        .ok()
    };

    // 查询本身出错（库刚建、表还没有、文件有问题）时走这里：没有 vault.dek
    // 就是没启用。判错方向是故意的 —— 误判成「未启用」最多是多弹一个解锁框，
    // 反过来会让界面以为凭据是明文。
    let Some(_) = get(K_DEK) else {
        return Vault::disabled();
    };

    match (get(K_ID), get(K_KDF)) {
        (Some(id), Some(kdf_json)) => match KdfSpec::from_json(&kdf_json) {
            Ok(kdf) => Vault::locked(VaultMeta { id, kdf, kdf_json }),
            Err(e) => {
                tracing::error!(error = %e, "保险库的 KDF 参数读不出来");
                Vault::locked(damaged_meta())
            }
        },
        _ => {
            tracing::error!("保险库元数据不完整：有 vault.dek 但缺 vault.id 或 vault.kdf");
            Vault::locked(damaged_meta())
        }
    }
}

fn damaged_meta() -> VaultMeta {
    VaultMeta {
        id: String::new(),
        kdf: KdfSpec::damaged(),
        kdf_json: String::new(),
    }
}

impl Store {
    pub fn vault(&self) -> &Vault {
        &self.vault
    }

    /// 这个库的 DEK 有没有被记在本机凭据服务里。
    ///
    /// 会真的去读一次凭据服务，在 Linux 上可能慢，**别放在启动路径里**。
    pub fn vault_is_remembered(&self) -> bool {
        self.vault
            .meta()
            .is_some_and(|m| !m.id.is_empty() && remember::is_remembered(&m.id))
    }

    // ——— 启用 ———

    /// 启用保险库：建密钥、落元数据、把现有明文凭据全部改写成密文。
    pub fn vault_enable(&self, password: &str, remember_here: bool) -> Result<SweepReport, String> {
        if self.vault.status().enabled {
            return Err("保险库已经启用过了".into());
        }
        if password.is_empty() {
            return Err("主密码不能为空".into());
        }

        // SQLite 的事务防得住崩溃和断电，防不住这段代码里的逻辑错误。
        // 凭据是不可再生的数据，先留一份原样的。
        self.backup_before_vault()?;

        let id = uuid::Uuid::new_v4().to_string();
        let kdf = KdfSpec::generate();
        let kdf_json = kdf.to_json()?;
        let dek = vault::new_dek();
        let wrapped = vault::wrap_dek(password, &kdf, &kdf_json, &dek)?;
        let verify = vault::make_verify(&dek)?;

        // —— 第一段事务：元数据。必须先于任何密文落盘，见模块开头第一条。——
        self.write_settings(&[
            (K_ID, &id),
            (K_KDF, &kdf_json),
            (K_DEK, &wrapped),
            (K_VERIFY, &verify),
            (K_MIGRATING, "1"),
        ])?;

        self.vault.enable(
            VaultMeta {
                id: id.clone(),
                kdf,
                kdf_json,
            },
            dek,
        );

        // —— 第二段事务：全量改写。—— 崩在这里也不要紧：每个值自带前缀标签，
        // 半迁移的库是合法的，下次解锁会接着扫完。
        let report = match self.sweep() {
            Ok(r) => r,
            Err(e) => {
                // 元数据已经落盘了，所以保险库是真的启用了，不能假装没发生过。
                // 留着 vault.migrating，下次解锁继续。
                tracing::error!(error = %e, "启用保险库时的全量加密中断");
                return Err(format!(
                    "{e}\n\n保险库已经建好，但还有凭据没加密完。重新解锁会从中断处接着扫。"
                ));
            }
        };

        // —— 第三段事务：清掉「进行中」。——
        self.clear_settings(&[K_MIGRATING])?;

        if remember_here {
            self.remember_now(&id)?;
        }
        tracing::info!(?report, "保险库已启用");
        Ok(report)
    }

    // ——— 解锁 ———

    /// 用主密码解锁。
    pub fn vault_unlock(&self, password: &str, remember_here: bool) -> Result<(), String> {
        let meta = self.vault.meta().ok_or("保险库未启用")?;
        if self.vault.status().unlocked {
            return Ok(());
        }
        let wrapped = self
            .get_setting(K_DEK)
            .ok_or("保险库数据缺失（settings 里没有 vault.dek）")?;

        // 解包成功本身就是主密码的校验 —— 密码错会在 GCM tag 上失败。
        let dek = vault::unwrap_dek(password, &meta.kdf, &meta.kdf_json, &wrapped)?;

        let id = meta.id.clone();
        self.vault.unlock(meta, dek);

        if remember_here {
            self.remember_now(&id)?;
        }
        self.finish_interrupted_sweep();
        Ok(())
    }

    /// 试试本机记住的那把钥匙。没记住、或者记的不是这个库的，都返回 false。
    ///
    /// **这会访问系统凭据服务**，Linux 上没有 D-Bus 时可能卡住，所以它是惰性的：
    /// 由前端在要显示解锁框之前调一次，而不是放在应用启动路径上。
    pub fn vault_try_remembered(&self) -> bool {
        let Some(meta) = self.vault.meta() else {
            return false;
        };
        if self.vault.status().unlocked {
            return true;
        }
        if meta.id.is_empty() {
            return false;
        }
        let Some(dek) = remember::load(&meta.id) else {
            return false;
        };

        // 记住的是 DEK，手上没有主密码也就没有 KEK，没法靠解包来验证这把钥匙。
        // 哨兵补的就是这一步 —— 少了它，一把属于**别的库**的 DEK 会被当成本库的，
        // 然后每一条凭据都解不开，错误信息还指向「数据损坏」。
        let ok = self
            .get_setting(K_VERIFY)
            .is_some_and(|s| vault::check_verify(&dek, &s));
        if !ok {
            tracing::warn!("本机记住的保险库密钥与当前库不匹配，已清除");
            remember::forget(&meta.id);
            return false;
        }

        self.vault.unlock(meta, dek);
        self.finish_interrupted_sweep();
        true
    }

    /// 丢掉内存里的密钥。不影响「已记住」。
    pub fn vault_lock(&self) {
        self.vault.lock();
    }

    /// 忘掉本机记住的密钥（下次启动要重新输主密码）。
    pub fn vault_forget(&self) {
        if let Some(meta) = self.vault.meta() {
            if !meta.id.is_empty() {
                remember::forget(&meta.id);
            }
        }
    }

    fn remember_now(&self, id: &str) -> Result<(), String> {
        let dek = self.vault.dek().ok_or("保险库未解锁")?;
        remember::save(id, &dek)
    }

    // ——— 换主密码 ———

    /// 换主密码。只重新包装 DEK，不动任何一条凭据，所以是 O(1)。
    ///
    /// 必须验旧密码：否则谁摸到这台开着的机器都能把用户锁在自己的库外面。
    pub fn vault_change_password(&self, old: &str, new: &str) -> Result<(), String> {
        if new.is_empty() {
            return Err("新主密码不能为空".into());
        }
        let meta = self.vault.meta().ok_or("保险库未启用")?;
        let wrapped = self
            .get_setting(K_DEK)
            .ok_or("保险库数据缺失（settings 里没有 vault.dek）")?;
        let dek = vault::unwrap_dek(old, &meta.kdf, &meta.kdf_json, &wrapped)
            .map_err(|_| "原主密码不正确".to_string())?;

        // 换密码顺手换盐，免得两次派生共用同一份预计算。
        let kdf = KdfSpec::generate();
        let kdf_json = kdf.to_json()?;
        let rewrapped = vault::wrap_dek(new, &kdf, &kdf_json, &dek)?;

        // kdf 和 dek 必须在同一个事务里换掉。只落了一半，库就打不开了。
        self.write_settings(&[(K_KDF, &kdf_json), (K_DEK, &rewrapped)])?;

        self.vault.unlock(
            VaultMeta {
                id: meta.id,
                kdf,
                kdf_json,
            },
            dek,
        );
        // 本机记住的是 DEK，DEK 没变，所以那个条目依然有效，不用动。
        Ok(())
    }

    // ——— 关闭 ———

    /// 关闭保险库，把凭据还原成明文。
    ///
    /// 要验主密码 —— 这一步会把全库凭据明文化，比读一条密码危险得多。
    pub fn vault_disable(&self, password: &str) -> Result<SweepReport, String> {
        let meta = self.vault.meta().ok_or("保险库未启用")?;
        let wrapped = self
            .get_setting(K_DEK)
            .ok_or("保险库数据缺失（settings 里没有 vault.dek）")?;
        let dek = vault::unwrap_dek(password, &meta.kdf, &meta.kdf_json, &wrapped)?;
        let id = meta.id.clone();
        self.vault.unlock(meta, dek);

        // 先还原数据再删元数据。反过来的话，中途失败就剩下一堆没有密钥的密文。
        let report = self.unsweep()?;

        self.clear_settings(&[K_ID, K_KDF, K_DEK, K_VERIFY, K_MIGRATING])?;
        if !id.is_empty() {
            remember::forget(&id);
        }
        self.vault.disable();
        tracing::info!(?report, "保险库已关闭，凭据恢复明文");
        Ok(report)
    }

    // ——— 全量扫描 ———

    /// 上次启用/扫描被中断过的话，在这里接着扫完。解锁成功后调用。
    fn finish_interrupted_sweep(&self) {
        if self.get_setting(K_MIGRATING).as_deref() != Some("1") {
            return;
        }
        tracing::warn!("上次的保险库加密没做完，正在接着扫");
        match self.sweep() {
            Ok(report) => {
                tracing::info!(?report, "保险库加密已补完");
                let _ = self.clear_settings(&[K_MIGRATING]);
            }
            // 补不完就留着标记，下次解锁再试。数据本身是安全的 ——
            // 半迁移的库完全可用，只是还有几条是明文。
            Err(e) => tracing::error!(error = %e, "补完保险库加密失败，下次解锁再试"),
        }
    }

    /// 把还是明文的凭据改写成密文。**幂等**，可以反复跑。
    pub fn sweep(&self) -> Result<SweepReport, String> {
        if self.vault.dek().is_none() {
            return Err("保险库未解锁，无法执行加密扫描".into());
        }

        let rows = self.read_credential_rows()?;
        let mut plan: Vec<Rewrite> = Vec::new();
        let mut report = SweepReport::default();

        for (id, password, passphrase, seed) in rows {
            for (field, column, current) in [
                (Field::Password, "password", password),
                (Field::Passphrase, "passphrase", passphrase),
            ] {
                let Some(current) = current.filter(|s| !s.is_empty()) else {
                    continue;
                };
                // 已经是密文了。启用被中断过时这一条很常见。
                if vault::classify(&current) == Form::Dek {
                    continue;
                }
                plan.push(self.rewrite(&id, column, field, &current)?);
                report.credentials += 1;
            }

            // TOTP 种子从「本机密钥」换挂到主密钥，顺带变得可移植。
            let Some(current) = seed.filter(|s| !s.is_empty()) else {
                continue;
            };
            match vault::classify(&current) {
                Form::Dek => {}
                Form::Keyring => match crate::secret::decrypt(&current) {
                    Ok(plain) => {
                        plan.push(self.rewrite(&id, "totp_seed", Field::Totp, &plain)?);
                        report.seeds_converted += 1;
                    }
                    // db 来自另一台机器，那把本机密钥根本不在这儿。原样留着，
                    // **绝不能因此中止整个启用流程** —— 为了几个要重填的种子，
                    // 把所有密码留在明文里是个糟糕的交换。
                    Err(e) => {
                        tracing::warn!(session = %id, error = %e, "TOTP 种子解不开，保持原样");
                        report.seeds_unreadable += 1;
                    }
                },
                // 这一列从引入起就只存密文，裸值只可能是损坏数据。
                Form::Plain => {
                    tracing::warn!(session = %id, "TOTP 种子格式无法识别，保持原样");
                    report.seeds_unreadable += 1;
                }
            }
        }

        self.apply(&plan)?;
        Ok(report)
    }

    /// [`Store::sweep`] 的逆：密文还原成明文（TOTP 则换回本机密钥）。
    fn unsweep(&self) -> Result<SweepReport, String> {
        let rows = self.read_credential_rows()?;
        let mut plan: Vec<Rewrite> = Vec::new();
        let mut report = SweepReport::default();

        for (id, password, passphrase, seed) in rows {
            for (field, column, current) in [
                (Field::Password, "password", password),
                (Field::Passphrase, "passphrase", passphrase),
            ] {
                let Some(current) = current.filter(|s| !s.is_empty()) else {
                    continue;
                };
                if vault::classify(&current) != Form::Dek {
                    continue;
                }
                let plain = self.vault.reveal(field, &current)?;
                plan.push(Rewrite {
                    id: id.clone(),
                    column,
                    old: current,
                    new: plain,
                });
                report.credentials += 1;
            }

            let Some(current) = seed.filter(|s| !s.is_empty()) else {
                continue;
            };
            if vault::classify(&current) != Form::Dek {
                continue;
            }
            let plain = self.vault.reveal(Field::Totp, &current)?;
            // 种子回到「密钥在系统凭据服务」。取不到服务时**直接报错**：
            // 把第二因子降级成明文存在第一因子旁边，等于把二次验证作废。
            let v1 = crate::secret::encrypt(&plain).map_err(|e| {
                format!("{e}\n\n关闭保险库需要把 TOTP 种子交还给系统凭据服务。\
                         如果这台机器上没有可用的凭据服务，请先清除这些种子再关闭。")
            })?;
            plan.push(Rewrite {
                id: id.clone(),
                column: "totp_seed",
                old: current,
                new: v1,
            });
            report.seeds_converted += 1;
        }

        self.apply(&plan)?;
        Ok(report)
    }

    /// 加密一个值，**并且立刻解回来比对**。
    ///
    /// 这一步看着多余，但凭据是不可再生的数据：对不上就整个中止，一个字节都不写。
    /// 它把「某个边界情况下的加密 bug」从「全库密码永久损坏」降级成「一次干净的
    /// 失败，数据原封不动」。
    fn rewrite(
        &self,
        id: &str,
        column: &'static str,
        field: Field,
        plain: &str,
    ) -> Result<Rewrite, String> {
        let new = self.vault.protect(field, plain)?;
        let back = self
            .vault
            .reveal(field, &new)
            .map_err(|e| format!("加密结果无法解回（{column}）：{e}"))?;
        if back != plain {
            return Err(format!("加密结果与原值不一致（{column}），已中止，未改动任何数据"));
        }
        Ok(Rewrite {
            id: id.to_string(),
            column,
            old: plain.to_string(),
            new,
        })
    }

    fn read_credential_rows(&self) -> Result<Vec<CredentialRow>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("SELECT id, password, passphrase, totp_seed FROM sessions")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map_err(|e| e.to_string())?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| e.to_string())?;
        Ok(rows)
    }

    /// 一次事务写回。**里面一句密码学调用都没有** —— 见模块开头第二条。
    fn apply(&self, plan: &[Rewrite]) -> Result<(), String> {
        if plan.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().map_err(|e| e.to_string())?;
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        for r in plan {
            // 带上旧值做条件：读出来到写回去之间，要是别的地方改过这一条
            // （那时保险库已经解锁，写进去的本来就是密文），就跳过，
            // 别拿手上这份陈旧的明文把它盖掉。
            //
            // 列名来自上面那几个字面量，不是外部输入。
            let sql = format!(
                "UPDATE sessions SET {0} = ?2 WHERE id = ?1 AND {0} = ?3",
                r.column
            );
            tx.execute(&sql, params![r.id, r.new, r.old])
                .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(())
    }

    // ——— 零碎 ———

    fn write_settings(&self, kv: &[(&str, &str)]) -> Result<(), String> {
        let mut conn = self.conn.lock().map_err(|e| e.to_string())?;
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        for (k, v) in kv {
            tx.execute(UPSERT, params![k, v]).map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(())
    }

    fn clear_settings(&self, keys: &[&str]) -> Result<(), String> {
        let mut conn = self.conn.lock().map_err(|e| e.to_string())?;
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        for k in keys {
            tx.execute("DELETE FROM settings WHERE key = ?1", params![k])
                .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(())
    }

    /// 启用前留一份原样的库。已经有备份就不覆盖 —— 那一份才是真正「启用之前」的。
    fn backup_before_vault(&self) -> Result<(), String> {
        let backup = self.path.with_extension("db.pre-vault.bak");
        if backup.exists() {
            return Ok(());
        }
        // 走一遍 SQLite 自己的备份 API 而不是 fs::copy：WAL 模式下直接拷主文件
        // 可能拿到一份缺了最近写入的副本。
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute("VACUUM INTO ?1", params![backup.to_string_lossy()])
            .map_err(|e| format!("无法备份数据库：{e}"))?;
        drop(conn);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o600));
        }
        tracing::info!(path = %backup.display(), "启用保险库前已备份数据库");
        Ok(())
    }
}

type CredentialRow = (String, Option<String>, Option<String>, Option<String>);

/// 一条待写回：带旧值是为了做比较并交换，见 [`Store::apply`]。
struct Rewrite {
    id: String,
    column: &'static str,
    old: String,
    new: String,
}
