//! 保险库：用**用户主密码**派生的密钥加密 db 里的凭据。
//!
//! ## 和 [`crate::secret`] 的分工
//!
//! `secret` 的密钥在系统凭据服务里，所以密文**跟着机器走**——db 拷到别的机器就
//! 解不开了。TOTP 种子愿意付这个代价（它是第二因子，和第一因子同处一个文件就
//! 失去了意义），但密码不行：把 `nexus.db` 拷到另一台机器接着用，是这个工具最
//! 常见的用法，也是当初把密码从凭据服务搬回 db 明文列的全部理由
//! （见 `store::migrate_legacy_secrets`）。
//!
//! 所以这里的密钥来自**用户记在脑子里的主密码**，派生参数和盐跟着 db 一起走。
//! 换台机器，输同一个主密码，全部打开。代价是主密码忘了就没有任何恢复手段——
//! 这是设计，不是缺陷，UI 上必须说清楚。
//!
//! ## 两层密钥
//!
//! ```text
//! 主密码 --Argon2id(salt,params)--> KEK --AES-GCM--> 包装后的 DEK（存 settings）
//!                                                      |
//!                                              DEK --AES-GCM--> 每一条凭据
//! ```
//!
//! 记录全部用随机 DEK 加密，主密码只负责包装这一个 DEK。好处是**换主密码是
//! O(1)**：重新包装 DEK 即可，不需要重写每一行，也就不存在「改到一半、半个库
//! 用新密钥半个库用旧密钥」这种修不回来的中间态。
//!
//! ## 三种存储形态
//!
//! ```text
//! v1:<b64>   [`crate::secret`]，密钥在系统凭据服务（当前只有 TOTP 种子）
//! v2:<b64>   本模块，密钥是 DEK
//! <其它>      遗留明文
//! ```
//!
//! 每个值自带「我是什么」的标签，所以半迁移的库是**合法的混合体**，正确性不依赖
//! 迁移事务的原子性。
//!
//! ### 唯一的尖角：用户的真实密码恰好以 `v2:` 开头
//!
//! 启用保险库时的全量扫描里，「有已知前缀但解不开」判定为明文、照常加密，
//! 所以这种密码不会丢。但**启用之后不能再这样兜底**：那时库里不可能有裸明文，
//! 解密失败就是真的失败，必须报错。绝不能回退成「原样返回」——那会把一串
//! base64 当成密码发给服务器，一次失败的登录远比一个明确的报错难排查。

// 这一步是纯核心：密码学原语 + 状态机都齐了，但还没有任何调用方 —— 接进
// `Store` 的凭据读写是下一步的事。在那之前整个模块对 lib 来说都是死代码。
// **接线之后请删掉这一行**，它会掩盖真正没人用的东西。
#![allow(dead_code)]

use aes_gcm::aead::{Aead, KeyInit, OsRng as AeadRng, Payload};
use aes_gcm::{AeadCore, Aes256Gcm, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine as _;
use parking_lot::RwLock;
use rand_core::RngCore;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// `secret` 模块的前缀。这里只认它、不产生它（除了关闭保险库时把种子还回去）。
const V1: &str = "v1:";
/// 本模块的前缀。
const V2: &str = "v2:";

/// AES-GCM nonce 长度（96 bit，RFC 5116 推荐值）。
const NONCE_LEN: usize = 12;

/// 字段级 AAD 的前缀，后面接 [`Field::tag`]。
const AAD_FIELD: &[u8] = b"nx/v2/";
/// 包装 DEK 时的 AAD 前缀，后面接 KDF 参数的 JSON 原文。
const AAD_DEK: &[u8] = b"nx/dek/v1";
/// 口令校验哨兵的 AAD。
const AAD_VERIFY: &[u8] = b"nx/verify/v1";

/// 哨兵明文。内容无所谓，能解开就说明 DEK 是对的。
const VERIFY_PLAIN: &[u8] = b"nexus-term/vault/verify/v1";

/// 默认 Argon2id 参数 —— RFC 9106 §4 的第二推荐档。
///
/// OWASP 常被引用的那组（19 MiB / t=2 / p=1）是给每秒处理上千次登录的 Web 服务
/// 调的，瓶颈在服务端吞吐；这里是桌面应用，每次启动解锁一次，延迟预算 0.5–1.5 s，
/// 照搬等于白白放弃三倍以上的攻击成本。
const DEFAULT_M_COST: u32 = 65536; // KiB = 64 MiB
const DEFAULT_T_COST: u32 = 3;
/// `p=4` 在当前依赖配置下**不增加耗时**：argon2 0.5 的多线程要开 `parallel`
/// feature（默认没开），所以这是串行算 4 条通道，总工作量由 `m × t` 决定。
/// 零成本，且将来打开那个 feature 可以直接变快。
const DEFAULT_P_COST: u32 = 4;
const KEY_LEN: usize = 32;
const SALT_LEN: usize = 16;

fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

// ——— 字段种类 ———

/// 加密时绑进 AAD 的字段种类。
///
/// 取**字段种类**而不是会话 id：前者能挡住「把 passphrase 的密文粘到 totp_seed
/// 列」这类张冠李戴，后者会让将来的「复制会话」变成一堆解不开的数据。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    Password,
    Passphrase,
    Totp,
    ApiKey,
}

impl Field {
    fn tag(self) -> &'static str {
        match self {
            Field::Password => "password",
            Field::Passphrase => "passphrase",
            Field::Totp => "totp",
            Field::ApiKey => "apikey",
        }
    }

    fn aad(self) -> Vec<u8> {
        let mut v = Vec::with_capacity(AAD_FIELD.len() + 10);
        v.extend_from_slice(AAD_FIELD);
        v.extend_from_slice(self.tag().as_bytes());
        v
    }
}

// ——— 存储形态 ———

/// 一个已存值长什么样。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Form {
    /// 没有已知前缀 —— 遗留明文，或者尚未被扫描改写。
    Plain,
    /// `v1:` —— [`crate::secret`]，密钥在系统凭据服务。
    Keyring,
    /// `v2:` —— 本模块，密钥是 DEK。
    Dek,
}

/// 看前缀判断形态。**只看前缀，不做解密**，所以它很便宜、也绝不会误报成功。
pub fn classify(stored: &str) -> Form {
    if stored.starts_with(V2) {
        Form::Dek
    } else if stored.starts_with(V1) {
        Form::Keyring
    } else {
        Form::Plain
    }
}

// ——— KDF 参数 ———

/// 派生参数，连同盐一起存进 `settings` 表的 `vault.kdf`。
///
/// **参数必须入库，不能写死在代码里**：任何一次默认值调整都会让老库永久打不开。
/// `keys::ppk` 的 `derive_v3` 是现成范式 —— 它从 PPK 头部读 flavour/memory/passes，
/// 而不是假设它们是某个固定值。
///
/// 附带好处：参数变成显式输入之后，单测可以用极小的 m/t 建库，不然每个保险库
/// 单测都要付 64 MiB 的内存和对应的时间。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct KdfSpec {
    /// 目前只认 `"argon2id"`。
    pub alg: String,
    /// Argon2 版本号，`0x13`。
    pub ver: u32,
    /// 内存开销，单位 KiB。
    pub m: u32,
    /// 迭代次数。
    pub t: u32,
    /// 并行度。
    pub p: u32,
    /// 输出长度，固定 32。
    pub len: u32,
    /// base64 的盐。
    pub salt: String,
}

impl KdfSpec {
    /// 新建一套参数：默认开销 + 新随机盐。
    pub fn generate() -> Self {
        Self::generate_with(DEFAULT_M_COST, DEFAULT_T_COST, DEFAULT_P_COST)
    }

    /// 指定开销的版本。生产只用 [`KdfSpec::generate`]，这个给测试用。
    pub fn generate_with(m: u32, t: u32, p: u32) -> Self {
        let mut salt = [0u8; SALT_LEN];
        rand_core::OsRng.fill_bytes(&mut salt);
        Self {
            alg: "argon2id".into(),
            ver: 0x13,
            m,
            t,
            p,
            len: KEY_LEN as u32,
            salt: b64().encode(salt),
        }
    }

    /// 序列化成要存进 `settings` 的那一串。
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string(self).map_err(|e| format!("无法序列化保险库 KDF 参数：{e}"))
    }

    /// 从 `settings` 里读回来。
    pub fn from_json(s: &str) -> Result<Self, String> {
        serde_json::from_str(s).map_err(|e| format!("保险库 KDF 参数已损坏：{e}"))
    }

    /// 由主密码派生 KEK。
    fn derive(&self, password: &str) -> Result<Zeroizing<[u8; KEY_LEN]>, String> {
        if self.alg != "argon2id" {
            return Err(format!("不支持的保险库密钥派生算法：{}", self.alg));
        }
        if self.ver != 0x13 {
            return Err(format!("不支持的 Argon2 版本：0x{:x}", self.ver));
        }
        if self.len as usize != KEY_LEN {
            return Err(format!("保险库密钥长度不对：{}", self.len));
        }
        let salt = b64()
            .decode(self.salt.trim())
            .map_err(|_| "保险库的盐不是合法 base64".to_string())?;
        if salt.len() < 8 {
            return Err("保险库的盐过短".into());
        }

        let params = Params::new(self.m, self.t, self.p, Some(KEY_LEN))
            .map_err(|e| format!("保险库 Argon2 参数不合法：{e}"))?;
        let alg = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

        let mut out = Zeroizing::new([0u8; KEY_LEN]);
        alg.hash_password_into(password.as_bytes(), &salt, out.as_mut())
            .map_err(|e| format!("保险库密钥派生失败：{e}"))?;
        Ok(out)
    }
}

// ——— 原始封装 ———

/// `b64(nonce ‖ AES-256-GCM(key, plain, aad))`，**不带前缀**。
fn seal_raw(key: &[u8; KEY_LEN], aad: &[u8], plain: &[u8]) -> Result<String, String> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| e.to_string())?;
    let nonce = Aes256Gcm::generate_nonce(&mut AeadRng);
    let ct = cipher
        .encrypt(&nonce, Payload { msg: plain, aad })
        .map_err(|_| "保险库加密失败".to_string())?;

    let mut blob = Vec::with_capacity(NONCE_LEN + ct.len());
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(&ct);
    Ok(b64().encode(blob))
}

/// [`seal_raw`] 的逆。AAD 对不上会在 GCM tag 校验这一步失败。
fn open_raw(key: &[u8; KEY_LEN], aad: &[u8], body: &str) -> Result<Zeroizing<Vec<u8>>, String> {
    let raw = b64()
        .decode(body.trim())
        .map_err(|_| "保险库密文不是合法 base64".to_string())?;
    if raw.len() <= NONCE_LEN {
        return Err("保险库密文过短，已损坏".into());
    }
    let (nonce, ct) = raw.split_at(NONCE_LEN);

    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| e.to_string())?;
    let plain = cipher
        .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad })
        .map_err(|_| "保险库解密失败：密钥不对，或数据已损坏".to_string())?;
    Ok(Zeroizing::new(plain))
}

fn to_text(bytes: Zeroizing<Vec<u8>>) -> Result<String, String> {
    String::from_utf8(bytes.to_vec()).map_err(|_| "保险库解密结果不是合法文本".into())
}

// ——— DEK 的生成、包装与校验 ———

/// 生成一把新的数据密钥。
pub fn new_dek() -> Zeroizing<[u8; KEY_LEN]> {
    let mut dek = Zeroizing::new([0u8; KEY_LEN]);
    // rand_core 的 OsRng 直接读系统 CSPRNG，不经用户态种子。
    rand_core::OsRng.fill_bytes(dek.as_mut());
    dek
}

fn dek_aad(kdf_json: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(AAD_DEK.len() + kdf_json.len());
    v.extend_from_slice(AAD_DEK);
    v.extend_from_slice(kdf_json.as_bytes());
    v
}

/// 用主密码包装 DEK，产物存进 `settings` 的 `vault.dek`。
///
/// AAD 绑定 KDF 参数的 **JSON 原文**，所以有人改了库里的 `vault.kdf`（比如把
/// m 降到 8 想加速爆破）时，包装直接解不开，而不是悄悄用弱参数派生。
pub fn wrap_dek(
    password: &str,
    kdf: &KdfSpec,
    kdf_json: &str,
    dek: &[u8; KEY_LEN],
) -> Result<String, String> {
    let kek = kdf.derive(password)?;
    seal_raw(&kek, &dek_aad(kdf_json), dek.as_slice())
}

/// 解开 [`wrap_dek`] 的产物。
///
/// **这就是主密码的校验**：密码错 → GCM tag 校验失败。之所以选它而不是找条真实
/// 数据试解，是因为启用保险库时库里**可能一条凭据都没有**，那时没有样本可试。
pub fn unwrap_dek(
    password: &str,
    kdf: &KdfSpec,
    kdf_json: &str,
    wrapped: &str,
) -> Result<Zeroizing<[u8; KEY_LEN]>, String> {
    let kek = kdf.derive(password)?;
    let raw = open_raw(&kek, &dek_aad(kdf_json), wrapped)
        .map_err(|_| "主密码不正确".to_string())?;
    let dek: [u8; KEY_LEN] = raw
        .as_slice()
        .try_into()
        .map_err(|_| "保险库数据密钥长度不对".to_string())?;
    Ok(Zeroizing::new(dek))
}

/// 造一个用 DEK 加密的哨兵，存进 `settings` 的 `vault.verify`。
///
/// 为什么光有 [`unwrap_dek`] 还不够：开了「在本机记住」之后，系统凭据服务里存的
/// 是 **DEK 本身**，自动解锁时手上没有主密码、因而没有 KEK，**没法验证这把 DEK
/// 对不对**。哨兵补上的就是这一步。
pub fn make_verify(dek: &[u8; KEY_LEN]) -> Result<String, String> {
    seal_raw(dek, AAD_VERIFY, VERIFY_PLAIN)
}

/// 校验一把 DEK 是不是这个库的。
pub fn check_verify(dek: &[u8; KEY_LEN], stored: &str) -> bool {
    match open_raw(dek, AAD_VERIFY, stored) {
        Ok(plain) => plain.as_slice() == VERIFY_PLAIN,
        Err(_) => false,
    }
}

// ——— 状态 ———

/// 保险库的元数据，全部来自 `settings` 表，因而跟着 db 走。
#[derive(Clone, Debug)]
pub struct VaultMeta {
    /// 建库时生成的 uuid。系统凭据服务的条目名用它，所以多个库天然互不干扰，
    /// db 拷到新机器后「已记住」会自动变成否。
    pub id: String,
    pub kdf: KdfSpec,
    /// `vault.kdf` 的**原文**。AAD 绑的是它，所以必须逐字节保留，
    /// 不能拿 `kdf` 重新序列化一遍 —— 字段顺序或空白变了就解不开了。
    pub kdf_json: String,
}

enum VaultState {
    /// `settings` 里没有 `vault.dek`：用户没启用。一切照旧。
    Disabled,
    /// 已启用，但这次会话还没拿到 DEK。
    Locked { meta: VaultMeta },
    /// 已启用且手上有 DEK。
    Unlocked {
        meta: VaultMeta,
        dek: Zeroizing<[u8; KEY_LEN]>,
    },
}

/// 对前端暴露的状态。「已记住」不是第四个状态，它只是 `Locked → Unlocked`
/// 的一条自动边。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VaultStatus {
    pub enabled: bool,
    pub unlocked: bool,
}

/// 保险库。住在 `Store` 内部，不住 `AppState` —— 凭据的读写方法全是 `Store` 的
/// 方法，调用点一律 `state.store.xxx()`，密钥跟着数据走，调用方签名一个都不用动。
///
/// **不变量：[`Vault::protect`] / [`Vault::reveal`] 永不触碰 `Store` 的 `conn`。**
/// 那把锁是不可重入的 `std::sync::Mutex`，在事务里回头调加解密会直接死锁。
/// 锁序永远是「先 vault 后 conn」。
pub struct Vault {
    state: RwLock<VaultState>,
}

impl Default for Vault {
    fn default() -> Self {
        Self::disabled()
    }
}

impl Vault {
    pub fn disabled() -> Self {
        Self {
            state: RwLock::new(VaultState::Disabled),
        }
    }

    pub fn locked(meta: VaultMeta) -> Self {
        Self {
            state: RwLock::new(VaultState::Locked { meta }),
        }
    }

    pub fn status(&self) -> VaultStatus {
        match &*self.state.read() {
            VaultState::Disabled => VaultStatus {
                enabled: false,
                unlocked: false,
            },
            VaultState::Locked { .. } => VaultStatus {
                enabled: true,
                unlocked: false,
            },
            VaultState::Unlocked { .. } => VaultStatus {
                enabled: true,
                unlocked: true,
            },
        }
    }

    pub fn meta(&self) -> Option<VaultMeta> {
        match &*self.state.read() {
            VaultState::Disabled => None,
            VaultState::Locked { meta } | VaultState::Unlocked { meta, .. } => Some(meta.clone()),
        }
    }

    /// 取一份 DEK 的拷贝。
    ///
    /// 全量扫描**必须**先用它把密钥取出来，再去开事务 —— 见本类型的锁序不变量。
    pub fn dek(&self) -> Option<Zeroizing<[u8; KEY_LEN]>> {
        match &*self.state.read() {
            VaultState::Unlocked { dek, .. } => Some(dek.clone()),
            _ => None,
        }
    }

    /// 解锁成功后登记密钥。
    pub fn unlock(&self, meta: VaultMeta, dek: Zeroizing<[u8; KEY_LEN]>) {
        *self.state.write() = VaultState::Unlocked { meta, dek };
    }

    /// 丢掉内存里的 DEK，退回 `Locked`。已经是 `Disabled` 的话什么都不做。
    pub fn lock(&self) {
        let mut st = self.state.write();
        if let VaultState::Unlocked { meta, .. } = &*st {
            let meta = meta.clone();
            *st = VaultState::Locked { meta };
        }
    }

    /// 启用保险库（建库时调用）。
    pub fn enable(&self, meta: VaultMeta, dek: Zeroizing<[u8; KEY_LEN]>) {
        *self.state.write() = VaultState::Unlocked { meta, dek };
    }

    /// 关闭保险库，回到「什么都不加密」。
    pub fn disable(&self) {
        *self.state.write() = VaultState::Disabled;
    }

    /// 把明文变成可以直接写进 db 文本列的一串。
    ///
    /// | 状态 | password / passphrase / apikey | totp |
    /// |---|---|---|
    /// | `Disabled` | 原样返回 | `v1:`（系统凭据服务） |
    /// | `Unlocked` | `v2:` | `v2:` |
    /// | `Locked` | `Err` | `Err` |
    ///
    /// `Disabled` 下对非 TOTP 字段**原样返回**，所以 SQL 和参数与改造前逐字节
    /// 相同 —— 「默认不开」是真的零回归，唯一新增成本是一次无竞争的读锁。
    pub fn protect(&self, kind: Field, plain: &str) -> Result<String, String> {
        match &*self.state.read() {
            VaultState::Unlocked { dek, .. } => {
                let body = seal_raw(dek, &kind.aad(), plain.as_bytes())?;
                Ok(format!("{V2}{body}"))
            }
            VaultState::Disabled => match kind {
                Field::Totp => crate::secret::encrypt(plain),
                _ => Ok(plain.to_string()),
            },
            VaultState::Locked { .. } => Err(LOCKED_WRITE.into()),
        }
    }

    /// [`Vault::protect`] 的逆。
    ///
    /// 派发看的是**值自己的前缀**而不是当前状态，因为半迁移的库是合法的：
    /// 同一张表里可以既有 `v2:` 又有还没扫到的明文。
    pub fn reveal(&self, kind: Field, stored: &str) -> Result<String, String> {
        match classify(stored) {
            Form::Dek => match &*self.state.read() {
                VaultState::Unlocked { dek, .. } => to_text(open_raw(dek, &kind.aad(), &stored[V2.len()..])?),
                _ => Err(LOCKED_READ.into()),
            },
            // v1 的密钥在系统凭据服务里，和保险库是否解锁无关。
            Form::Keyring => crate::secret::decrypt(stored),
            // 还没被扫描改写的遗留明文。
            //
            // 即使此刻是 `Locked` 也照常返回：这个值本来就以明文躺在同一个文件里，
            // 拒绝返回换不来任何安全，只会把「扫描被中断过」变成一次莫名其妙的
            // 连接失败。写回不受影响 —— `protect` 在 `Locked` 下仍然报错。
            Form::Plain => Ok(stored.to_string()),
        }
    }
}

const LOCKED_WRITE: &str = "保险库已锁定，无法保存凭据。请先输入主密码解锁。";
const LOCKED_READ: &str = "保险库已锁定，无法读取凭据。请先输入主密码解锁。";

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用的轻量参数。生产是 64 MiB / t=3，每个单测都付那个开销不现实。
    fn tiny() -> KdfSpec {
        KdfSpec::generate_with(64, 1, 1)
    }

    fn unlocked_vault() -> Vault {
        let kdf = tiny();
        let kdf_json = kdf.to_json().unwrap();
        let v = Vault::disabled();
        v.enable(
            VaultMeta {
                id: "test".into(),
                kdf,
                kdf_json,
            },
            new_dek(),
        );
        v
    }

    // ——— KDF ———

    #[test]
    fn kdf_spec_survives_a_json_roundtrip() {
        let spec = tiny();
        assert_eq!(KdfSpec::from_json(&spec.to_json().unwrap()).unwrap(), spec);
    }

    #[test]
    fn same_password_and_salt_derive_the_same_key() {
        let spec = tiny();
        assert_eq!(
            spec.derive("hunter2").unwrap().as_slice(),
            spec.derive("hunter2").unwrap().as_slice()
        );
    }

    #[test]
    fn a_fresh_spec_has_a_fresh_salt() {
        assert_ne!(tiny().salt, tiny().salt);
    }

    #[test]
    fn rejects_unknown_kdf_algorithms() {
        let mut spec = tiny();
        spec.alg = "scrypt".into();
        assert!(spec.derive("hunter2").is_err());
    }

    #[test]
    fn default_params_are_the_rfc9106_second_tier() {
        let spec = KdfSpec::generate();
        assert_eq!((spec.m, spec.t, spec.p), (65536, 3, 4));
        assert_eq!(spec.len, 32);
    }

    // ——— DEK 包装 ———

    #[test]
    fn dek_survives_wrap_and_unwrap() {
        let kdf = tiny();
        let json = kdf.to_json().unwrap();
        let dek = new_dek();
        let wrapped = wrap_dek("hunter2", &kdf, &json, &dek).unwrap();
        let back = unwrap_dek("hunter2", &kdf, &json, &wrapped).unwrap();
        assert_eq!(back.as_slice(), dek.as_slice());
    }

    #[test]
    fn the_wrong_master_password_fails() {
        let kdf = tiny();
        let json = kdf.to_json().unwrap();
        let wrapped = wrap_dek("hunter2", &kdf, &json, &new_dek()).unwrap();
        assert!(unwrap_dek("hunter3", &kdf, &json, &wrapped).is_err());
    }

    /// 有人改了库里的 `vault.kdf` 想把派生开销调低 —— 解不开。
    ///
    /// 严格说这一条靠的是「参数变了 KEK 就变了」，AAD 只是第二道。真正只有
    /// AAD 能挡的是下面那条。
    #[test]
    fn weakening_the_kdf_params_breaks_the_wrapping() {
        let kdf = tiny();
        let json = kdf.to_json().unwrap();
        let wrapped = wrap_dek("hunter2", &kdf, &json, &new_dek()).unwrap();

        let mut weakened = kdf.clone();
        weakened.t = kdf.t + 1;
        let tampered_json = weakened.to_json().unwrap();
        assert!(unwrap_dek("hunter2", &weakened, &tampered_json, &wrapped).is_err());
    }

    /// AAD 绑的是 `vault.kdf` 的**原文**，所以语义相同但排版不同的 JSON 也解不开。
    ///
    /// 这不是在防攻击者，是在防未来的自己：`VaultMeta` 必须原样保留从 db 读到的
    /// 那一串，任何「拿 `kdf` 重新 `to_json()` 一遍」的省事写法都会让整库打不开，
    /// 而且要等到 serde 字段顺序或格式变化的那一天才会炸。
    #[test]
    fn the_kdf_json_must_be_preserved_verbatim() {
        let kdf = tiny();
        let json = kdf.to_json().unwrap();
        let wrapped = wrap_dek("hunter2", &kdf, &json, &new_dek()).unwrap();

        let reformatted = serde_json::to_string_pretty(&kdf).unwrap();
        assert_ne!(reformatted, json, "前提：排版确实不同");
        assert!(unwrap_dek("hunter2", &kdf, &reformatted, &wrapped).is_err());
    }

    // ——— 哨兵 ———

    #[test]
    fn verify_sentinel_accepts_only_its_own_dek() {
        let dek = new_dek();
        let sentinel = make_verify(&dek).unwrap();
        assert!(check_verify(&dek, &sentinel));
        assert!(!check_verify(&new_dek(), &sentinel));
    }

    #[test]
    fn verify_sentinel_rejects_garbage_without_panicking() {
        let dek = new_dek();
        assert!(!check_verify(&dek, ""));
        assert!(!check_verify(&dek, "这不是 base64!!!"));
        assert!(!check_verify(&dek, &b64().encode([0u8; 4])));
    }

    // ——— 前缀嗅探 ———

    #[test]
    fn classify_sees_all_three_forms() {
        assert_eq!(classify("v2:abc"), Form::Dek);
        assert_eq!(classify("v1:abc"), Form::Keyring);
        assert_eq!(classify("hunter2"), Form::Plain);
        assert_eq!(classify(""), Form::Plain);
    }

    // ——— Disabled：零回归的核心断言 ———

    #[test]
    fn disabled_passes_plaintext_straight_through() {
        let v = Vault::disabled();
        for kind in [Field::Password, Field::Passphrase, Field::ApiKey] {
            assert_eq!(v.protect(kind, "hunter2").unwrap(), "hunter2");
            assert_eq!(v.reveal(kind, "hunter2").unwrap(), "hunter2");
        }
        assert_eq!(v.status().enabled, false);
    }

    /// 用户的真实密码恰好长得像密文时，`Disabled` 下也必须原样穿过去。
    #[test]
    fn disabled_does_not_mangle_a_password_that_looks_like_ciphertext() {
        let v = Vault::disabled();
        assert_eq!(v.protect(Field::Password, "v2:oops").unwrap(), "v2:oops");
    }

    // ——— Unlocked ———

    #[test]
    fn unlocked_roundtrips_every_field() {
        let v = unlocked_vault();
        for kind in [Field::Password, Field::Passphrase, Field::Totp, Field::ApiKey] {
            let blob = v.protect(kind, "hunter2").unwrap();
            assert!(blob.starts_with(V2), "{kind:?} 应该产生 v2 密文");
            assert!(!blob.contains("hunter2"), "密文里不该出现明文");
            assert_eq!(v.reveal(kind, &blob).unwrap(), "hunter2");
        }
    }

    #[test]
    fn each_encryption_uses_a_fresh_nonce() {
        let v = unlocked_vault();
        assert_ne!(
            v.protect(Field::Password, "same").unwrap(),
            v.protect(Field::Password, "same").unwrap()
        );
    }

    /// AAD 绑字段种类：passphrase 的密文粘到 totp_seed 列上必须解不开。
    #[test]
    fn ciphertext_cannot_be_moved_between_fields() {
        let v = unlocked_vault();
        let blob = v.protect(Field::Passphrase, "hunter2").unwrap();
        assert!(v.reveal(Field::Totp, &blob).is_err());
        assert!(v.reveal(Field::Password, &blob).is_err());
        assert_eq!(v.reveal(Field::Passphrase, &blob).unwrap(), "hunter2");
    }

    #[test]
    fn unlocked_rejects_tampered_ciphertext() {
        let v = unlocked_vault();
        let blob = v.protect(Field::Password, "hunter2").unwrap();
        let mut raw = b64().decode(&blob[V2.len()..]).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0x01;
        let tampered = format!("{V2}{}", b64().encode(&raw));
        assert!(v.reveal(Field::Password, &tampered).is_err());
    }

    /// 半迁移的库：同一张表里既有 v2 又有还没扫到的明文，两种都要能读。
    #[test]
    fn unlocked_still_reads_not_yet_swept_plaintext() {
        let v = unlocked_vault();
        assert_eq!(v.reveal(Field::Password, "遗留明文").unwrap(), "遗留明文");
    }

    /// 另一把 DEK 的密文解不开 —— 这是「db 拷来了但主密码不对」的场景。
    #[test]
    fn one_vaults_ciphertext_is_opaque_to_another() {
        let a = unlocked_vault();
        let b = unlocked_vault();
        let blob = a.protect(Field::Password, "hunter2").unwrap();
        assert!(b.reveal(Field::Password, &blob).is_err());
    }

    // ——— Locked ———

    #[test]
    fn locked_refuses_to_read_or_write_ciphertext() {
        let v = unlocked_vault();
        let blob = v.protect(Field::Password, "hunter2").unwrap();
        v.lock();

        assert_eq!(
            v.status(),
            VaultStatus {
                enabled: true,
                unlocked: false
            }
        );
        assert!(v.protect(Field::Password, "hunter2").is_err());
        assert!(v.reveal(Field::Password, &blob).is_err());
        assert!(v.dek().is_none());
    }

    /// 锁定态下**明文**仍然读得出来。它本来就以明文躺在同一个文件里，拒绝返回
    /// 换不来安全，只会让「扫描被中断过」变成一次看不懂的连接失败。
    #[test]
    fn locked_still_reads_leftover_plaintext() {
        let v = unlocked_vault();
        v.lock();
        assert_eq!(v.reveal(Field::Password, "遗留明文").unwrap(), "遗留明文");
    }

    #[test]
    fn relocking_an_unlocked_vault_then_unlocking_restores_access() {
        let v = unlocked_vault();
        let meta = v.meta().unwrap();
        let dek = v.dek().unwrap();
        let blob = v.protect(Field::Password, "hunter2").unwrap();

        v.lock();
        assert!(v.reveal(Field::Password, &blob).is_err());

        v.unlock(meta, dek);
        assert_eq!(v.reveal(Field::Password, &blob).unwrap(), "hunter2");
    }

    /// `lock()` 对没启用的库是空操作，不能把它误推成 `Locked` —— 那会让一个
    /// 从没启用过保险库的用户突然被弹解锁框。
    #[test]
    fn locking_a_disabled_vault_is_a_no_op() {
        let v = Vault::disabled();
        v.lock();
        assert_eq!(v.status().enabled, false);
        assert_eq!(v.protect(Field::Password, "hunter2").unwrap(), "hunter2");
    }

    #[test]
    fn disable_drops_the_key_and_goes_back_to_passthrough() {
        let v = unlocked_vault();
        v.disable();
        assert_eq!(
            v.status(),
            VaultStatus {
                enabled: false,
                unlocked: false
            }
        );
        assert!(v.dek().is_none());
        assert_eq!(v.protect(Field::Password, "hunter2").unwrap(), "hunter2");
    }

    // ——— 端到端：这是「换台机器」的全过程 ———

    #[test]
    fn a_vault_can_be_reopened_from_nothing_but_the_stored_metadata() {
        // 第一台机器：建库、加密一条密码。
        let kdf = tiny();
        let kdf_json = kdf.to_json().unwrap();
        let dek = new_dek();
        let wrapped = wrap_dek("hunter2", &kdf, &kdf_json, &dek).unwrap();
        let sentinel = make_verify(&dek).unwrap();

        let v1 = Vault::disabled();
        v1.enable(
            VaultMeta {
                id: "vault-a".into(),
                kdf,
                kdf_json: kdf_json.clone(),
            },
            dek,
        );
        let blob = v1.protect(Field::Password, "s3cret").unwrap();

        // 第二台机器：只有 db 里的这几串 + 用户脑子里的主密码。
        let kdf2 = KdfSpec::from_json(&kdf_json).unwrap();
        let dek2 = unwrap_dek("hunter2", &kdf2, &kdf_json, &wrapped).unwrap();
        assert!(check_verify(&dek2, &sentinel), "哨兵应当认得这把 DEK");

        let v2 = Vault::locked(VaultMeta {
            id: "vault-a".into(),
            kdf: kdf2,
            kdf_json,
        });
        v2.unlock(v2.meta().unwrap(), dek2);
        assert_eq!(v2.reveal(Field::Password, &blob).unwrap(), "s3cret");
    }
}
