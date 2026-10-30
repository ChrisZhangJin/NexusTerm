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

/// 新建一组 KDF 参数。
///
/// 单测里换成极轻的一组：默认那组每次派生要 64 MiB、约一秒，而本文件的用例验的
/// 是流程不是 KDF 强度，照默认跑下来整个测试套件会慢上几十秒。
#[cfg(not(test))]
fn fresh_kdf() -> KdfSpec {
    KdfSpec::generate()
}
#[cfg(test)]
fn fresh_kdf() -> KdfSpec {
    KdfSpec::generate_with(8, 1, 1)
}

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

    /// 启用了但没解锁 —— 此刻凭据读不出来也写不进去。
    ///
    /// 调用点用它来给出一句说得通的错误，而不是让用户看见「未保存密码」
    /// 这种与事实相反的提示。
    pub fn vault_locked(&self) -> bool {
        let s = self.vault.status();
        s.enabled && !s.unlocked
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
        let kdf = fresh_kdf();
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
        self.after_unlock();
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
        self.after_unlock();
        true
    }

    /// 刚解锁要补的两件事。
    fn after_unlock(&self) {
        self.finish_interrupted_sweep();
        // 旧版密码还躺在系统凭据服务里的话，现在才轮到把它们迁进来 ——
        // 启动时保险库是锁着的，那会儿迁移只会写失败然后把源头删掉。
        self.migrate_legacy_secrets();
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
        let kdf = fresh_kdf();
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

    // ——— 重置（忘了主密码的逃生口）———

    /// **销毁**所有解不开的凭据，把库退回未启用。
    ///
    /// 这不是恢复，是破坏性重置，界面上必须说清楚。主机 / 端口 / 用户名 / 分组 /
    /// 宏 / 密钥元数据全部保留，用户要补的只是密码本身。做这个口子是因为另一条路
    /// 更糟：忘了主密码又没有出口，用户只能删掉整个 nexus.db 重建全部连接。
    ///
    /// 不需要主密码 —— 需要的话这个功能就没有存在意义了。
    pub fn vault_reset(&self) -> Result<usize, String> {
        let meta = self.vault.meta().ok_or("保险库未启用")?;

        // 留一份再动手。用户现在解不开这些密文，但将来想起主密码就解得开 ——
        // 这份备份是那条退路唯一的载体，而它只值几十毫秒。
        self.backup_db("db.pre-reset.bak", true)?;

        // 先销毁数据再删元数据：反过来的话中途失败就剩下一堆没有密钥的密文，
        // 而那时连「哪些是密文」都还能看出来，只是再也没有入口去清它们了。
        let cleared = self.clear_encrypted_credentials()?;
        self.clear_settings(&[K_ID, K_KDF, K_DEK, K_VERIFY, K_MIGRATING])?;
        if !meta.id.is_empty() {
            remember::forget(&meta.id);
        }
        self.vault.disable();
        tracing::warn!(cleared, "保险库已重置，其中的加密凭据已销毁");
        Ok(cleared)
    }

    /// 把所有 `v2:` 的凭据列清成 NULL。`v1:` 的 TOTP 种子不动 —— 那些挂在
    /// 系统凭据服务上，和主密码无关，重置之后照样能用。
    fn clear_encrypted_credentials(&self) -> Result<usize, String> {
        let rows = self.read_credential_rows()?;
        let mut targets: Vec<(String, &'static str, String)> = Vec::new();
        for (id, password, passphrase, seed) in rows {
            for (column, current) in [
                ("password", password),
                ("passphrase", passphrase),
                ("totp_seed", seed),
            ] {
                let Some(current) = current.filter(|s| !s.is_empty()) else {
                    continue;
                };
                if vault::classify(&current) != Form::Dek {
                    continue;
                }
                targets.push((id.clone(), column, current));
            }
        }
        if targets.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn.lock().map_err(|e| e.to_string())?;
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        for (id, column, old) in &targets {
            // 清成 NULL 而不是空串：六个凭据方法把「空」一律当成「没配置」，
            // 两种写法读回来都是 None，但 NULL 才是它们自己写出来的形状。
            let sql = format!("UPDATE sessions SET {0} = NULL WHERE id = ?1 AND {0} = ?2", column);
            tx.execute(&sql, params![id, old]).map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(targets.len())
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
        self.backup_db("db.pre-vault.bak", false)
    }

    /// 在不可逆的操作之前复制一份 db。
    ///
    /// `overwrite = false` 时已有同名备份就直接跳过：那一份比新的更有价值，
    /// 它才是「动手之前」的样子。
    fn backup_db(&self, suffix: &str, overwrite: bool) -> Result<(), String> {
        let backup = self.path.with_extension(suffix);
        if backup.exists() {
            if !overwrite {
                return Ok(());
            }
            std::fs::remove_file(&backup).map_err(|e| format!("无法覆盖旧备份：{e}"))?;
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
        tracing::info!(path = %backup.display(), "已备份数据库");
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

#[cfg(test)]
mod tests {
    use super::super::tests::{a_session, temp_store_at};
    use super::*;
    use crate::store::Store;
    use std::path::PathBuf;

    const PW: &str = "correct horse battery staple";

    /// 建一个装了两条凭据的库。返回 (store, 数据目录, 会话 id)。
    ///
    /// 数据目录单独返回是为了「关掉再开一次」—— 保险库能不能跨进程存活，
    /// 只有重新 `Store::open` 才验得出来。
    fn store_with_credentials(tag: &str) -> (Store, PathBuf, String) {
        let (store, db) = temp_store_at(tag);
        let id = a_session(&store, "s");
        store.set_password(&id, "PLAINTEXT-CANARY").unwrap();
        store.set_passphrase(&id, "PASSPHRASE-CANARY").unwrap();
        (store, db, id)
    }

    /// 把 db 文件读成字节。
    ///
    /// 先 VACUUM 一遍：`UPDATE` 过的旧值会留在页面的空闲区里，不清理的话
    /// 「文件里搜不到明文」这条断言会被上一代的残留误伤。
    fn db_bytes(store: &Store, db: &std::path::Path) -> Vec<u8> {
        store.conn.lock().unwrap().execute_batch("VACUUM").unwrap();
        std::fs::read(db).expect("读 db 文件")
    }

    fn contains(hay: &[u8], needle: &str) -> bool {
        hay.windows(needle.len()).any(|w| w == needle.as_bytes())
    }

    /// 绕过 `reveal` 直接看列里存的那一串。用来断言「密文一个字节都没变」。
    fn raw_column(store: &Store, id: &str, column: &str) -> Option<String> {
        let conn = store.conn.lock().unwrap();
        conn.query_row(
            &format!("SELECT {column} FROM sessions WHERE id = ?1"),
            params![id],
            |r| r.get::<_, Option<String>>(0),
        )
        .unwrap()
    }

    // ——— 启用 ———

    /// 和 `without_a_vault_passwords_sit_in_the_file_as_plaintext` 互为镜像：
    /// 那条钉住「默认不开 = 零回归」，这条钉住「开了就真的加密了」。
    #[test]
    fn enabling_the_vault_scrubs_the_plaintext_out_of_the_file() {
        let (store, db, id) = store_with_credentials("vault-enable");
        assert!(contains(&db_bytes(&store, &db), "PLAINTEXT-CANARY"), "前置条件");

        let report = store.vault_enable(PW, false).unwrap();
        assert_eq!(report.credentials, 2, "密码 + 私钥口令");

        let bytes = db_bytes(&store, &db);
        assert!(!contains(&bytes, "PLAINTEXT-CANARY"), "密码不该还在文件里");
        assert!(!contains(&bytes, "PASSPHRASE-CANARY"), "私钥口令不该还在文件里");

        // 加密了，但照样读得回来。
        assert_eq!(store.get_password(&id).as_deref(), Some("PLAINTEXT-CANARY"));
        assert_eq!(store.get_passphrase(&id).as_deref(), Some("PASSPHRASE-CANARY"));
    }

    #[test]
    fn enabling_leaves_a_backup_of_the_original_database() {
        let (store, db, _) = store_with_credentials("vault-backup");
        store.vault_enable(PW, false).unwrap();

        let backup = db.with_extension("db.pre-vault.bak");
        let bytes = std::fs::read(&backup).expect("备份文件应当存在");
        assert!(
            contains(&bytes, "PLAINTEXT-CANARY"),
            "备份的意义就是留下启用之前的样子"
        );
    }

    #[test]
    fn the_vault_cannot_be_enabled_twice() {
        let (store, _, _) = store_with_credentials("vault-twice");
        store.vault_enable(PW, false).unwrap();
        assert!(store.vault_enable("another", false).is_err());
    }

    #[test]
    fn an_empty_master_password_is_refused() {
        let (store, _, _) = store_with_credentials("vault-empty-pw");
        assert!(store.vault_enable("", false).is_err());
        assert!(!store.vault().status().enabled, "拒了就不该留下半个保险库");
    }

    // ——— 可移植性：整个功能的验收标准 ———

    /// 把库关掉重开 —— 等价于「拷到另一台机器」，因为密钥材料全都在 db 里。
    #[test]
    fn a_vault_survives_reopening_the_database() {
        let (store, db, id) = store_with_credentials("vault-reopen");
        store.vault_enable(PW, false).unwrap();
        drop(store);

        let dir = db.parent().unwrap().to_path_buf();
        let store = Store::open(dir).expect("重新打开");
        assert_eq!(
            store.vault().status(),
            crate::vault::VaultStatus { enabled: true, unlocked: false },
            "开库应当认出保险库，并且是锁着的"
        );
        assert!(store.get_password(&id).is_none(), "没解锁就读不出来");

        assert!(store.vault_unlock("wrong", false).is_err(), "错密码必须失败");
        store.vault_unlock(PW, false).unwrap();
        assert_eq!(store.get_password(&id).as_deref(), Some("PLAINTEXT-CANARY"));
    }

    /// 锁着的时候写凭据必须**失败**，而不是悄悄写一份明文进去。
    ///
    /// 这条是「读出来是 None」之所以安全的前提：界面上看到空密码的用户点保存，
    /// 这里会报错，不会把真密码覆盖掉。
    #[test]
    fn a_locked_vault_refuses_to_write() {
        let (store, _, id) = store_with_credentials("vault-locked-write");
        store.vault_enable(PW, false).unwrap();
        store.vault_lock();

        assert!(store.vault_locked());
        assert!(store.set_password(&id, "new-one").is_err());
        assert!(store.set_passphrase(&id, "new-one").is_err());

        store.vault_unlock(PW, false).unwrap();
        assert_eq!(
            store.get_password(&id).as_deref(),
            Some("PLAINTEXT-CANARY"),
            "刚才那次失败的写入不该动过任何东西"
        );
    }

    // ——— 扫描 ———

    #[test]
    fn sweeping_again_changes_nothing() {
        let (store, _, id) = store_with_credentials("vault-sweep-idem");
        store.vault_enable(PW, false).unwrap();
        let before = raw_column(&store, &id, "password");

        let again = store.sweep().unwrap();
        assert_eq!(again.credentials, 0, "已经是密文的不该再被卷一次");
        assert_eq!(raw_column(&store, &id, "password"), before, "连密文都该一个字节没动");
    }

    /// 启用扫到一半掉电：库里一半明文一半密文，`vault.migrating` 还立着。
    /// 下一次解锁应当把剩下的扫完。
    #[test]
    fn an_interrupted_sweep_finishes_on_the_next_unlock() {
        let (store, db, id) = store_with_credentials("vault-resume");
        store.vault_enable(PW, false).unwrap();

        // 手工伪造那个中断现场。
        let other = a_session(&store, "half-done");
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE sessions SET password = 'LEFTOVER-CANARY' WHERE id = ?1",
                params![other],
            )
            .unwrap();
        store.write_settings(&[(K_MIGRATING, "1")]).unwrap();
        store.vault_lock();

        store.vault_unlock(PW, false).unwrap();
        assert_eq!(
            store.get_setting(K_MIGRATING),
            None,
            "补扫完了就该把标记清掉"
        );
        assert_eq!(store.get_password(&other).as_deref(), Some("LEFTOVER-CANARY"));
        assert!(!contains(&db_bytes(&store, &db), "LEFTOVER-CANARY"));
        assert_eq!(store.get_password(&id).as_deref(), Some("PLAINTEXT-CANARY"));
    }

    /// 来自另一台机器的 db：TOTP 种子是用那台机器的密钥加密的，这儿解不开。
    /// 能转的转，转不动的计个数，**绝不中止整个启用**。
    #[test]
    fn unreadable_totp_seeds_do_not_abort_the_enable() {
        let (store, db, id) = store_with_credentials("vault-bad-seed");
        // 直接写一个认不出形态的值，绕开 `set_totp_seed`（那条路要碰系统凭据服务）。
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE sessions SET totp_seed = 'JUNK-FROM-ELSEWHERE' WHERE id = ?1",
                params![id],
            )
            .unwrap();

        let report = store.vault_enable(PW, false).unwrap();
        assert_eq!(report.seeds_unreadable, 1);
        assert_eq!(report.credentials, 2, "密码照样要加密");
        assert!(!contains(&db_bytes(&store, &db), "PLAINTEXT-CANARY"));
    }

    // ——— 换主密码 ———

    #[test]
    fn changing_the_master_password_keeps_every_credential() {
        let (store, db, id) = store_with_credentials("vault-chpw");
        store.vault_enable(PW, false).unwrap();
        let ciphertext_before = raw_column(&store, &id, "password");

        assert!(store.vault_change_password("wrong", "new-pw").is_err());
        store.vault_change_password(PW, "new-pw").unwrap();

        assert_eq!(
            raw_column(&store, &id, "password"),
            ciphertext_before,
            "换的是包着 DEK 的那层，凭据密文一个字节都不该动"
        );

        drop(store);
        let store = Store::open(db.parent().unwrap().to_path_buf()).unwrap();
        assert!(store.vault_unlock(PW, false).is_err(), "旧密码该作废了");
        store.vault_unlock("new-pw", false).unwrap();
        assert_eq!(store.get_password(&id).as_deref(), Some("PLAINTEXT-CANARY"));
    }

    #[test]
    fn an_empty_new_master_password_is_refused() {
        let (store, _, _) = store_with_credentials("vault-chpw-empty");
        store.vault_enable(PW, false).unwrap();
        assert!(store.vault_change_password(PW, "").is_err());
    }

    // ——— 关闭 ———

    #[test]
    fn disabling_the_vault_puts_the_plaintext_back() {
        let (store, db, id) = store_with_credentials("vault-disable");
        store.vault_enable(PW, false).unwrap();

        assert!(store.vault_disable("wrong").is_err(), "关闭也要验主密码");

        let report = store.vault_disable(PW).unwrap();
        assert_eq!(report.credentials, 2);
        assert!(!store.vault().status().enabled);
        assert_eq!(store.get_password(&id).as_deref(), Some("PLAINTEXT-CANARY"));
        assert!(contains(&db_bytes(&store, &db), "PLAINTEXT-CANARY"));
        assert_eq!(store.get_setting(K_DEK), None, "元数据该清干净");
    }

    /// 关掉之后还能再开一次，而且是一把全新的 DEK / 盐。
    #[test]
    fn the_vault_can_be_enabled_again_after_being_disabled() {
        let (store, _, id) = store_with_credentials("vault-recycle");
        store.vault_enable(PW, false).unwrap();
        let first = store.get_setting(K_DEK).unwrap();
        store.vault_disable(PW).unwrap();

        store.vault_enable(PW, false).unwrap();
        assert_ne!(store.get_setting(K_DEK).unwrap(), first, "盐和 DEK 都该是新的");
        assert_eq!(store.get_password(&id).as_deref(), Some("PLAINTEXT-CANARY"));
    }

    // ——— 重置 ———

    /// 忘了主密码的逃生口：凭据没了，连接配置还在。
    #[test]
    fn resetting_destroys_the_credentials_but_keeps_the_sessions() {
        let (store, db, id) = store_with_credentials("vault-reset");
        store.vault_enable(PW, false).unwrap();
        store.vault_lock();

        let cleared = store.vault_reset().unwrap();
        assert_eq!(cleared, 2);
        assert!(!store.vault().status().enabled);
        assert_eq!(store.get_password(&id), None);
        assert_eq!(store.get_passphrase(&id), None);

        let s = store.get(&id).expect("会话本身必须还在");
        assert_eq!(s.host, "example.com");
        assert_eq!(s.username, "root");

        // 销毁之前留下的那一份：用户哪天想起主密码，这是唯一的退路。
        let backup = db.with_extension("db.pre-reset.bak");
        assert!(backup.exists(), "重置前必须留一份备份");

        // 清完就是一个干净的未启用库，照样能重新填密码。
        store.set_password(&id, "a-new-one").unwrap();
        assert_eq!(store.get_password(&id).as_deref(), Some("a-new-one"));
    }

    #[test]
    fn resetting_a_vault_that_was_never_enabled_is_an_error() {
        let (store, _, _) = store_with_credentials("vault-reset-none");
        assert!(store.vault_reset().is_err());
    }

    // ——— 损坏的元数据 ———

    /// 有 `vault.dek` 但 `vault.kdf` 读不出来 —— 必须判成**锁着**。
    ///
    /// 退回「未启用」看起来更友好，实际是最坏的选择：界面会以为凭据是明文，
    /// 然后让用户往一堆密文旁边写明文。
    #[test]
    fn damaged_metadata_pins_the_vault_shut() {
        let (store, db, _) = store_with_credentials("vault-damaged");
        store.vault_enable(PW, false).unwrap();
        store.write_settings(&[(K_KDF, "{ not json")]).unwrap();
        drop(store);

        let store = Store::open(db.parent().unwrap().to_path_buf()).unwrap();
        assert!(store.vault_locked(), "元数据坏了要 fail-closed");
        assert!(store.vault_unlock(PW, false).is_err());
    }
}
