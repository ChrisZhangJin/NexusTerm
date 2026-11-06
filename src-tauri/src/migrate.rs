//! 连接迁移：把全部已保存的连接（连同凭据）打成一个带密码保护的文件，拷到另一台电脑再导入。
//!
//! ## 文件格式（`.nexusconn`）
//!
//! 一个 JSON 文档：
//!
//! ```text
//! { "magic": "NEXUSCONN", "version": 1,
//!   "kdf": { Argon2id 参数 + 每个文件独立的随机盐 },
//!   "nonce": "<b64>", "ct": "<b64>" }
//! ```
//!
//! `ct` 是 AES-256-GCM(密钥, 载荷 JSON, AAD) 的密文，**AAD = 去掉 `ct` 的头部 JSON**，
//! 所以改 magic / version / kdf 参数 / nonce 任何一处都会让认证失败，而不是静默地
//! 用被改过的参数去解密。
//!
//! ## 密钥与保险库无关
//!
//! 导出密钥**只**由用户输入的「导出密码」经 Argon2id 派生，和主密码 / 保险库 / 本机
//! 凭据服务毫无关系 —— 这个文件的全部意义就是脱离本机能被另一台电脑打开。
//! 载荷里的凭据是通过 `Store` 的 getter 取出的明文，所以保险库开没开都一样；
//! 保险库启用但锁着时读不出东西，直接拒绝（比导出一份缺凭据的文件诚实）。
//!
//! ## 安全边界
//!
//! - 解密前先卡文件大小（64 MB）与 KDF 参数上限（防「小文件 + 巨大内存开销」的 DoS）。
//! - 载荷里**没有任何路径是被信任的**：私钥文件内容按文本嵌入，导入时一律经
//!   [`crate::keys::import_contents`] 写进 `<app_data>/keys/<新 uuid>`，文件名 / id
//!   不取自文件。
//! - 密钥材料用 `Zeroizing` / 手动 `zeroize` 尽量擦；错误串与返回的摘要里不含任何秘密。

use std::collections::{HashMap, HashSet};
use std::path::Path;

use aes_gcm::aead::{Aead, KeyInit, OsRng as AeadRng, Payload as AeadPayload};
use aes_gcm::{AeadCore, Aes256Gcm, Nonce};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::keys;
use crate::store::{NewSession, SessionExtras, SessionRecord, Store};
use crate::vault::KdfSpec;

const MAGIC: &str = "NEXUSCONN";
const VERSION: u32 = 1;
const NONCE_LEN: usize = 12;

/// 文件上限。几千条连接加私钥也就几 MB，64 MB 是明显异常的值。
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_SESSIONS: usize = 10_000;
/// 单个私钥文件上限。OpenSSH 私钥通常 < 4 KB，RSA 16384 位也不到 10 KB。
const MAX_KEY_BYTES: u64 = 1024 * 1024;
/// 单个文本字段上限，挡住载荷里塞巨型字符串。
const MAX_FIELD_CHARS: usize = 8192;
pub const MIN_PASSWORD_CHARS: usize = 8;

// 对导入文件里声明的 Argon2 参数设上限。参数是文件说了算的，不卡的话
// 一个几百字节的恶意文件就能让对方机器分配几十 GB 内存。
const KDF_MAX_M_KIB: u32 = 512 * 1024;
const KDF_MAX_T: u32 = 16;
const KDF_MAX_P: u32 = 16;

fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

// ———————————————————————— 载荷 ————————————————————————

/// 一条连接：`SessionRecord` 的可迁移字段 + 附加设置 + 三种凭据。
///
/// 故意不派生 `Debug`：任何一次 `{:?}` 都会把密码打进日志。
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportedSession {
    pub name: String,
    pub folder: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth_type: String,
    pub key_path: Option<String>,
    pub startup_cmd: Option<String>,
    pub theme: Option<String>,
    pub connect_timeout: Option<u32>,
    pub note: Option<String>,
    pub sort_order: i32,
    #[serde(default)]
    pub extras: Option<SessionExtras>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub passphrase: Option<String>,
    #[serde(default)]
    pub totp_seed: Option<String>,
    /// 指向 `Payload::keys` 里某条的引用；只在载荷内部有意义。
    #[serde(default)]
    pub key_ref: Option<String>,
}

impl Drop for ExportedSession {
    fn drop(&mut self) {
        self.password.zeroize();
        self.passphrase.zeroize();
        self.totp_seed.zeroize();
    }
}

/// 一份被嵌入的私钥文件。`id` 只是载荷内部的引用名，**不会**用作文件名。
#[derive(Serialize, Deserialize)]
pub struct ExportedKey {
    pub id: String,
    pub name: String,
    pub pem: String,
}

impl Drop for ExportedKey {
    fn drop(&mut self) {
        self.pem.zeroize();
    }
}

#[derive(Serialize, Deserialize, Default)]
pub struct ConnPayload {
    pub sessions: Vec<ExportedSession>,
    #[serde(default)]
    pub keys: Vec<ExportedKey>,
}

/// 只打印条数的 `Debug`：`unwrap()` / `{:?}` 不该有机会把凭据带进日志或 panic 信息。
impl std::fmt::Debug for ConnPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ConnPayload({} sessions, {} keys)", self.sessions.len(), self.keys.len())
    }
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSummary {
    pub sessions: usize,
    pub passwords: usize,
    pub passphrases: usize,
    pub totp_seeds: usize,
    pub keys: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSummary {
    pub imported: usize,
    /// 与已有连接 (host, port, username, name) 相同而跳过的条数
    pub skipped: usize,
    pub passwords: usize,
    pub passphrases: usize,
    pub totp_seeds: usize,
    pub keys: usize,
    pub warnings: Vec<String>,
}

// ———————————————————————— 信封 ————————————————————————

#[derive(Serialize, Deserialize)]
struct Header {
    magic: String,
    version: u32,
    kdf: KdfSpec,
    nonce: String,
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    #[serde(flatten)]
    header: Header,
    ct: String,
}

/// 导出密码的本地校验（前端有同样一份，后端不信前端）。
pub fn check_password(password: &str) -> Result<(), String> {
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err(format!("导出密码至少 {MIN_PASSWORD_CHARS} 个字符"));
    }
    Ok(())
}

fn header_aad(h: &Header) -> Result<Vec<u8>, String> {
    serde_json::to_vec(h).map_err(|_| "文件头序列化失败".to_string())
}

fn check_kdf(k: &KdfSpec) -> Result<(), String> {
    if k.alg != "argon2id" || k.ver != 0x13 || k.len != 32 {
        return Err("文件使用了不支持的密钥派生算法".into());
    }
    if k.m > KDF_MAX_M_KIB || k.t > KDF_MAX_T || k.p > KDF_MAX_P || k.t == 0 || k.p == 0 {
        return Err("文件里的密钥派生参数超出允许范围，已拒绝".into());
    }
    Ok(())
}

/// 加密 `plain`，返回整个文件的字节。`kdf` 由调用方给，测试用小参数、生产用 `KdfSpec::generate()`。
pub fn seal(plain: &[u8], password: &str, kdf: KdfSpec) -> Result<Vec<u8>, String> {
    check_kdf(&kdf)?;
    let key = kdf
        .derive(password)
        .map_err(|_| "导出密钥派生失败".to_string())?;
    let nonce = Aes256Gcm::generate_nonce(&mut AeadRng);
    let header = Header {
        magic: MAGIC.into(),
        version: VERSION,
        kdf,
        nonce: b64().encode(nonce),
    };
    let aad = header_aad(&header)?;
    let cipher = Aes256Gcm::new_from_slice(key.as_ref()).map_err(|_| "加密初始化失败".to_string())?;
    let ct = cipher
        .encrypt(&nonce, AeadPayload { msg: plain, aad: &aad })
        .map_err(|_| "加密失败".to_string())?;
    let env = Envelope {
        header,
        ct: b64().encode(ct),
    };
    serde_json::to_vec(&env).map_err(|_| "序列化失败".to_string())
}

/// [`seal`] 的逆。密码错与被篡改在 AEAD 层不可区分，统一一句话。
pub fn open(bytes: &[u8], password: &str) -> Result<Zeroizing<Vec<u8>>, String> {
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("文件过大（超过 64 MB），不是有效的连接迁移文件".into());
    }
    let env: Envelope = serde_json::from_slice(bytes)
        .map_err(|_| "不是有效的连接迁移文件（.nexusconn）".to_string())?;
    if env.header.magic != MAGIC {
        return Err("不是有效的连接迁移文件（.nexusconn）".into());
    }
    if env.header.version != VERSION {
        return Err(format!(
            "不支持的文件版本 {}（本程序支持 {}），请升级 NexusTerm 后重试",
            env.header.version, VERSION
        ));
    }
    check_kdf(&env.header.kdf)?;
    let nonce = b64()
        .decode(env.header.nonce.trim())
        .ok()
        .filter(|n| n.len() == NONCE_LEN)
        .ok_or("文件已损坏（nonce 不合法）")?;
    let ct = b64().decode(env.ct.trim()).map_err(|_| "文件已损坏（密文不合法）")?;

    let aad = header_aad(&env.header)?;
    let key = env
        .header
        .kdf
        .derive(password)
        .map_err(|_| "文件里的密钥派生参数不合法".to_string())?;
    let cipher = Aes256Gcm::new_from_slice(key.as_ref()).map_err(|_| "解密初始化失败".to_string())?;
    let plain = cipher
        .decrypt(Nonce::from_slice(&nonce), AeadPayload { msg: &ct, aad: &aad })
        .map_err(|_| "导出密码不对，或文件已损坏 / 被篡改。未导入任何内容。".to_string())?;
    Ok(Zeroizing::new(plain))
}

// ———————————————————————— 载荷校验 ————————————————————————

fn too_long(s: &str) -> bool {
    s.chars().count() > MAX_FIELD_CHARS
}

/// 解析并校验载荷。文件已经通过了 AEAD 认证，所以这里防的不是「被篡改」，
/// 而是「导出方版本有 bug / 恶意构造的合法文件」。
pub fn parse_payload(plain: &[u8]) -> Result<ConnPayload, String> {
    let p: ConnPayload =
        serde_json::from_slice(plain).map_err(|_| "文件内容格式不对，无法解析".to_string())?;
    if p.sessions.len() > MAX_SESSIONS {
        return Err(format!("文件里的连接数超过上限 {MAX_SESSIONS}"));
    }
    if p.keys.len() > MAX_SESSIONS {
        return Err("文件里的私钥数量异常".into());
    }
    let key_ids: HashSet<&str> = p.keys.iter().map(|k| k.id.as_str()).collect();
    if key_ids.len() != p.keys.len() {
        return Err("文件里的私钥引用重复".into());
    }
    for k in &p.keys {
        if k.pem.len() as u64 > MAX_KEY_BYTES || too_long(&k.name) {
            return Err("文件里有超长的私钥条目".into());
        }
    }
    for (i, s) in p.sessions.iter().enumerate() {
        let n = i + 1;
        if s.name.trim().is_empty() || s.host.trim().is_empty() {
            return Err(format!("第 {n} 条连接缺少名称或主机"));
        }
        if s.port == 0 {
            return Err(format!("第 {n} 条连接的端口不合法"));
        }
        if !matches!(s.auth_type.as_str(), "password" | "key" | "agent") {
            return Err(format!("第 {n} 条连接的认证方式不认识"));
        }
        let texts = [
            Some(&s.name),
            Some(&s.folder),
            Some(&s.host),
            Some(&s.username),
            s.key_path.as_ref(),
            s.startup_cmd.as_ref(),
            s.theme.as_ref(),
            s.note.as_ref(),
            s.password.as_ref(),
            s.passphrase.as_ref(),
            s.totp_seed.as_ref(),
        ];
        if texts.iter().flatten().any(|t| too_long(t)) {
            return Err(format!("第 {n} 条连接里有超长字段"));
        }
        if let Some(r) = &s.key_ref {
            if !key_ids.contains(r.as_str()) {
                return Err(format!("第 {n} 条连接引用了不存在的私钥"));
            }
        }
    }
    Ok(p)
}

// ———————————————————————— 合并规划 ————————————————————————

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Import,
    /// 与已有连接 (host, port, username, name) 全部相同
    Duplicate,
}

/// 逐条判定。只和**导入前已有**的连接比，不和文件内部互比 ——
/// 源机器上两条一模一样的连接是用户自己的事，不该被悄悄吞掉一条。
pub fn plan_merge(existing: &[SessionRecord], incoming: &[ExportedSession]) -> Vec<Verdict> {
    let have: HashSet<(&str, u16, &str, &str)> = existing
        .iter()
        .map(|s| (s.host.as_str(), s.port, s.username.as_str(), s.name.as_str()))
        .collect();
    incoming
        .iter()
        .map(|s| {
            if have.contains(&(s.host.as_str(), s.port, s.username.as_str(), s.name.as_str())) {
                Verdict::Duplicate
            } else {
                Verdict::Import
            }
        })
        .collect()
}

/// 导入顺序：按源机器的 `sortOrder`（稳定），`Store::create` 会依次排在现有连接之后。
pub fn import_order(incoming: &[ExportedSession]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..incoming.len()).collect();
    idx.sort_by_key(|&i| incoming[i].sort_order);
    idx
}

// ———————————————————————— 导出 ————————————————————————

fn locked_msg(action: &str) -> String {
    format!("保险库已锁定，无法{action}已保存的凭据。请先在「安全」里输入主密码解锁。")
}

/// 从 Store 收集载荷。**不写任何文件、不加密。**
pub fn collect(store: &Store, include_keys: bool) -> Result<(ConnPayload, ExportSummary), String> {
    if store.vault_locked() {
        return Err(locked_msg("导出"));
    }
    let sessions = store.list()?;
    let extras = store.all_extras();
    let mut payload = ConnPayload::default();
    let mut sum = ExportSummary::default();
    // 私钥路径 → 载荷内引用名；同一把私钥被多条连接共用时只嵌一份
    let mut key_refs: HashMap<String, Option<String>> = HashMap::new();
    let mut skipped_keys = 0usize;

    for s in &sessions {
        let password = store.get_password(&s.id);
        let passphrase = store.get_passphrase(&s.id);
        let totp_seed = store.get_totp_seed(&s.id);
        if totp_seed.is_none() && store.has_totp_seed(&s.id) {
            sum.warnings
                .push(format!("「{}」的 TOTP 种子无法解密（可能来自其它机器），未导出", s.name));
        }
        sum.passwords += password.is_some() as usize;
        sum.passphrases += passphrase.is_some() as usize;
        sum.totp_seeds += totp_seed.is_some() as usize;

        let mut key_ref = None;
        if s.auth_type == "key" {
            match s.key_path.as_deref().filter(|p| !p.trim().is_empty()) {
                None => sum
                    .warnings
                    .push(format!("「{}」选了密钥认证但没有指定私钥文件", s.name)),
                Some(_) if !include_keys => skipped_keys += 1,
                Some(path) => {
                    let entry = key_refs.entry(path.to_string()).or_insert_with(|| {
                        match read_key_file(store, path) {
                            Ok((name, pem)) => {
                                let id = format!("k{}", payload.keys.len());
                                payload.keys.push(ExportedKey { id: id.clone(), name, pem });
                                Some(id)
                            }
                            Err(w) => {
                                sum.warnings.push(format!("私钥 {path}：{w}，未嵌入"));
                                None
                            }
                        }
                    });
                    key_ref = entry.clone();
                }
            }
        }

        payload.sessions.push(ExportedSession {
            name: s.name.clone(),
            folder: s.folder.clone(),
            host: s.host.clone(),
            port: s.port,
            username: s.username.clone(),
            auth_type: s.auth_type.clone(),
            key_path: s.key_path.clone(),
            startup_cmd: s.startup_cmd.clone(),
            theme: s.theme.clone(),
            connect_timeout: s.connect_timeout,
            note: s.note.clone(),
            sort_order: s.sort_order,
            extras: extras.get(&s.id).cloned(),
            password,
            passphrase,
            totp_seed,
            key_ref,
        });
    }
    if skipped_keys > 0 {
        sum.warnings.push(format!(
            "未包含私钥文件：{skipped_keys} 条密钥认证的连接需要在新电脑上自行放好私钥"
        ));
    }
    sum.sessions = payload.sessions.len();
    sum.keys = payload.keys.len();
    Ok((payload, sum))
}

/// 读一个私钥文件（库内 `<app_data>/keys/<id>` 与外部 `~/.ssh/...` 一视同仁）。
/// 返回 (展示名, 内容)；错误是给用户看的原因，不含文件内容。
fn read_key_file(store: &Store, path: &str) -> Result<(String, String), &'static str> {
    let expanded = keys::expand_tilde(path);
    let p = Path::new(&expanded);
    let meta = std::fs::metadata(p).map_err(|_| "文件不存在或无法读取")?;
    if !meta.is_file() {
        return Err("不是文件");
    }
    if meta.len() > MAX_KEY_BYTES {
        return Err("文件过大");
    }
    let pem = std::fs::read_to_string(p).map_err(|_| "无法读取（不是文本文件？）")?;
    let managed = store
        .list_keys()
        .into_iter()
        .find(|k| k.path == expanded)
        .map(|k| k.name);
    let name = managed
        .or_else(|| p.file_name().and_then(|n| n.to_str()).map(str::to_string))
        .unwrap_or_else(|| "imported-key".into());
    Ok((name, pem))
}

/// 以仅所有者可读写的权限原子地写文件：先写同目录临时文件再改名，
/// 中途失败不会留下半截的 `.nexusconn`。
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".nexus-tmp");
    let tmp = std::path::PathBuf::from(tmp);

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let res = (|| {
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("写入失败：{e}"));
    }
    Ok(())
}

/// 完整导出：收集 → 加密 → 写文件。返回的摘要只含计数与警告。
pub fn export_to_file(
    store: &Store,
    dest: &Path,
    password: &str,
    include_keys: bool,
) -> Result<ExportSummary, String> {
    export_with_kdf(store, dest, password, include_keys, KdfSpec::generate())
}

fn export_with_kdf(
    store: &Store,
    dest: &Path,
    password: &str,
    include_keys: bool,
    kdf: KdfSpec,
) -> Result<ExportSummary, String> {
    check_password(password)?;
    let (payload, summary) = collect(store, include_keys)?;
    let json = Zeroizing::new(
        serde_json::to_vec(&payload).map_err(|_| "载荷序列化失败".to_string())?,
    );
    let file = seal(&json, password, kdf)?;
    write_private(dest, &file)?;
    Ok(summary)
}

// ———————————————————————— 导入 ————————————————————————

/// 读文件并解密、校验。同步（含 Argon2），调用方放到阻塞线程里。
pub fn read_and_open(path: &Path, password: &str) -> Result<ConnPayload, String> {
    let meta = std::fs::metadata(path).map_err(|_| "无法读取所选文件".to_string())?;
    if meta.len() > MAX_FILE_BYTES {
        return Err("文件过大（超过 64 MB），不是有效的连接迁移文件".into());
    }
    let bytes = std::fs::read(path).map_err(|_| "无法读取所选文件".to_string())?;
    let plain = open(&bytes, password)?;
    parse_payload(&plain)
}

/// 把载荷合并进本机。出错时尽力回滚本次已建的连接与私钥，不留半截。
pub async fn apply_import(
    store: &Store,
    keys_dir: &Path,
    mut payload: ConnPayload,
) -> Result<ImportSummary, String> {
    if store.vault_locked() {
        return Err(locked_msg("导入"));
    }
    let existing = store.list()?;
    let verdicts = plan_merge(&existing, &payload.sessions);

    let mut created: Vec<String> = Vec::new();
    let mut created_keys: Vec<String> = Vec::new();
    let mut sum = ImportSummary::default();
    let res = apply_inner(
        store,
        keys_dir,
        &mut payload,
        &verdicts,
        &mut created,
        &mut created_keys,
        &mut sum,
    )
    .await;
    if let Err(e) = res {
        for id in &created {
            let _ = store.delete(id);
        }
        for id in &created_keys {
            let _ = keys::delete(store, id).await;
        }
        return Err(format!("导入失败，已撤销本次导入：{e}"));
    }
    Ok(sum)
}

async fn apply_inner(
    store: &Store,
    keys_dir: &Path,
    payload: &mut ConnPayload,
    verdicts: &[Verdict],
    created: &mut Vec<String>,
    created_keys: &mut Vec<String>,
    sum: &mut ImportSummary,
) -> Result<(), String> {
    // 载荷内引用名 → 本机密钥库里的新路径（None = 恢复失败，只警告一次）
    let mut restored: HashMap<String, Option<String>> = HashMap::new();

    for i in import_order(&payload.sessions) {
        if verdicts[i] == Verdict::Duplicate {
            sum.skipped += 1;
            continue;
        }

        // 先处理私钥：只恢复被「真正要导入」的连接引用到的那几把
        let mut key_path = payload.sessions[i].key_path.clone();
        if let Some(r) = payload.sessions[i].key_ref.clone() {
            if !restored.contains_key(&r) {
                let k = payload.keys.iter().find(|k| k.id == r);
                let new_path = match k {
                    Some(k) => {
                        match keys::import_contents(store, keys_dir, &k.pem, k.name.clone(), None).await {
                            Ok(info) => {
                                created_keys.push(info.id);
                                sum.keys += 1;
                                Some(info.path)
                            }
                            Err(e) => {
                                sum.warnings.push(format!("私钥「{}」无法恢复：{e}", k.name));
                                None
                            }
                        }
                    }
                    None => None,
                };
                restored.insert(r.clone(), new_path);
            }
            match restored.get(&r).cloned().flatten() {
                Some(p) => key_path = Some(p),
                None => sum.warnings.push(format!(
                    "「{}」的私钥未能恢复，保留原路径，需要手动处理",
                    payload.sessions[i].name
                )),
            }
        } else if payload.sessions[i].auth_type == "key" {
            sum.warnings.push(format!(
                "「{}」的导出文件里没有私钥，路径保持原样，请确认新电脑上该路径存在",
                payload.sessions[i].name
            ));
        }

        let s = &mut payload.sessions[i];
        let folder = if s.folder.trim().is_empty() { "/".to_string() } else { s.folder.clone() };
        let rec = store.create(NewSession {
            name: s.name.clone(),
            folder: Some(folder),
            host: s.host.clone(),
            port: Some(s.port),
            username: s.username.clone(),
            auth_type: s.auth_type.clone(),
            key_path,
            startup_cmd: s.startup_cmd.clone(),
            theme: s.theme.clone(),
            connect_timeout: s.connect_timeout,
            note: s.note.clone(),
        })?;
        created.push(rec.id.clone());

        if let Some(e) = &s.extras {
            store.set_extras(&rec.id, e)?;
        }
        if let Some(mut v) = s.password.take() {
            let r = store.set_password(&rec.id, &v);
            v.zeroize();
            r?;
            sum.passwords += 1;
        }
        if let Some(mut v) = s.passphrase.take() {
            let r = store.set_passphrase(&rec.id, &v);
            v.zeroize();
            r?;
            sum.passphrases += 1;
        }
        if let Some(mut v) = s.totp_seed.take() {
            let r = store.set_totp_seed(&rec.id, &v);
            v.zeroize();
            r?;
            sum.totp_seeds += 1;
        }
        sum.imported += 1;
    }
    Ok(())
}

// ———————————————————————— 测试 ————————————————————————

#[cfg(test)]
mod tests {
    use super::*;

    const PW: &str = "correct horse battery";

    fn tiny() -> KdfSpec {
        KdfSpec::generate_with(64, 1, 1)
    }

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("nexus-migrate-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn store(tag: &str) -> (Store, std::path::PathBuf) {
        let dir = tmp_dir(tag);
        (Store::open(dir.clone()).expect("开库"), dir)
    }

    fn new_session(name: &str, host: &str, auth: &str, key_path: Option<String>) -> NewSession {
        NewSession {
            name: name.into(),
            folder: Some("/prod/web".into()),
            host: host.into(),
            port: Some(2222),
            username: "deploy".into(),
            auth_type: auth.into(),
            key_path,
            startup_cmd: Some("tmux a".into()),
            theme: Some("dracula".into()),
            connect_timeout: Some(7),
            note: Some("备注".into()),
        }
    }

    /// 生成一把真实的 ed25519 私钥文本
    fn real_key_pem() -> String {
        use ssh_key::{Algorithm, LineEnding, PrivateKey};
        let k = PrivateKey::random(&mut rand_core::OsRng, Algorithm::Ed25519).unwrap();
        k.to_openssh(LineEnding::LF).unwrap().to_string()
    }

    // ——— 信封 ———

    #[test]
    fn seal_then_open_roundtrips() {
        let f = seal(b"hello", PW, tiny()).unwrap();
        assert_eq!(&**open(&f, PW).unwrap(), b"hello");
    }

    #[test]
    fn wrong_password_fails() {
        let f = seal(b"hello", PW, tiny()).unwrap();
        let e = open(&f, "another password").unwrap_err();
        assert!(e.contains("导出密码不对"));
    }

    #[test]
    fn each_file_has_fresh_salt_and_nonce() {
        let a = seal(b"x", PW, tiny()).unwrap();
        let b = seal(b"x", PW, tiny()).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let f = seal(b"hello world", PW, tiny()).unwrap();
        let mut env: Envelope = serde_json::from_slice(&f).unwrap();
        let mut ct = b64().decode(&env.ct).unwrap();
        ct[0] ^= 1;
        env.ct = b64().encode(ct);
        assert!(open(&serde_json::to_vec(&env).unwrap(), PW).is_err());
    }

    #[test]
    fn tampered_header_fails() {
        let f = seal(b"hello world", PW, tiny()).unwrap();
        // 改 nonce
        let mut env: Envelope = serde_json::from_slice(&f).unwrap();
        let mut n = b64().decode(&env.header.nonce).unwrap();
        n[0] ^= 1;
        env.header.nonce = b64().encode(n);
        assert!(open(&serde_json::to_vec(&env).unwrap(), PW).is_err());

        // 改 KDF 参数：派生出的密钥就不对了
        let mut env: Envelope = serde_json::from_slice(&f).unwrap();
        env.header.kdf.t += 1;
        assert!(open(&serde_json::to_vec(&env).unwrap(), PW).is_err());
    }

    /// 头部被 AAD 保护：即使攻击者拿到密钥也不能只改头不重加密 ——
    /// 这里用「同一把密钥、AAD 不同」直接验证 AAD 确实参与了认证。
    #[test]
    fn header_is_authenticated_as_aad() {
        let kdf = tiny();
        let key = kdf.derive(PW).unwrap();
        let f = seal(b"data", PW, kdf).unwrap();
        let env: Envelope = serde_json::from_slice(&f).unwrap();
        let nonce = b64().decode(&env.header.nonce).unwrap();
        let ct = b64().decode(&env.ct).unwrap();
        let cipher = Aes256Gcm::new_from_slice(key.as_ref()).unwrap();
        let ok = cipher.decrypt(
            Nonce::from_slice(&nonce),
            AeadPayload { msg: &ct, aad: &header_aad(&env.header).unwrap() },
        );
        assert!(ok.is_ok());
        let bad = cipher.decrypt(
            Nonce::from_slice(&nonce),
            AeadPayload { msg: &ct, aad: b"other" },
        );
        assert!(bad.is_err());
    }

    #[test]
    fn malformed_files_are_rejected_with_clear_errors() {
        assert!(open(b"not json", PW).unwrap_err().contains("不是有效的连接迁移文件"));
        assert!(open(b"{}", PW).is_err());
        let f = seal(b"x", PW, tiny()).unwrap();

        let mut env: Envelope = serde_json::from_slice(&f).unwrap();
        env.header.magic = "OTHER".into();
        assert!(open(&serde_json::to_vec(&env).unwrap(), PW)
            .unwrap_err()
            .contains("不是有效的连接迁移文件"));

        let mut env: Envelope = serde_json::from_slice(&f).unwrap();
        env.header.version = 99;
        assert!(open(&serde_json::to_vec(&env).unwrap(), PW)
            .unwrap_err()
            .contains("不支持的文件版本"));

        let mut env: Envelope = serde_json::from_slice(&f).unwrap();
        env.header.nonce = "AAAA".into();
        assert!(open(&serde_json::to_vec(&env).unwrap(), PW).is_err());
    }

    #[test]
    fn oversized_file_and_kdf_params_are_rejected() {
        let big = vec![b' '; MAX_FILE_BYTES as usize + 1];
        assert!(open(&big, PW).unwrap_err().contains("过大"));

        // 恶意文件声明一个巨大的内存开销：必须在派生前就拒绝
        let f = seal(b"x", PW, tiny()).unwrap();
        let mut env: Envelope = serde_json::from_slice(&f).unwrap();
        env.header.kdf.m = u32::MAX;
        assert!(open(&serde_json::to_vec(&env).unwrap(), PW)
            .unwrap_err()
            .contains("超出允许范围"));
    }

    #[test]
    fn short_export_password_is_rejected() {
        assert!(check_password("short").is_err());
        assert!(check_password("12345678").is_ok());
    }

    // ——— 载荷校验 ———

    fn payload_json(extra: &str) -> Vec<u8> {
        format!(
            r#"{{"sessions":[{{"name":"a","folder":"/","host":"h","port":22,"username":"u","authType":"password","keyPath":null,"startupCmd":null,"theme":null,"connectTimeout":null,"note":null,"sortOrder":1{extra}}}]}}"#
        )
        .into_bytes()
    }

    #[test]
    fn payload_validation() {
        assert!(parse_payload(&payload_json("")).is_ok());
        assert!(parse_payload(b"[]").is_err());
        assert!(parse_payload(&payload_json(r#","keyRef":"nope""#)).is_err());
        let bad_auth = String::from_utf8(payload_json(""))
            .unwrap()
            .replace("\"password\"", "\"weird\"");
        assert!(parse_payload(bad_auth.as_bytes()).is_err());
        let bad_port = String::from_utf8(payload_json(""))
            .unwrap()
            .replace("\"port\":22", "\"port\":0");
        assert!(parse_payload(bad_port.as_bytes()).is_err());
    }

    #[test]
    fn too_many_sessions_rejected() {
        let one = r#"{"name":"a","folder":"/","host":"h","port":22,"username":"u","authType":"agent","sortOrder":0}"#;
        let many = vec![one; MAX_SESSIONS + 1].join(",");
        let json = format!(r#"{{"sessions":[{many}]}}"#);
        assert!(parse_payload(json.as_bytes()).unwrap_err().contains("上限"));
    }

    // ——— 合并规划 ———

    #[test]
    fn plan_merge_matches_host_port_user_name() {
        let (st, _d) = store("plan");
        st.create(new_session("web", "10.0.0.1", "agent", None)).unwrap();
        let existing = st.list().unwrap();
        let mk = |name: &str, host: &str, port: u16, user: &str| ExportedSession {
            name: name.into(),
            folder: "/".into(),
            host: host.into(),
            port,
            username: user.into(),
            auth_type: "agent".into(),
            key_path: None,
            startup_cmd: None,
            theme: None,
            connect_timeout: None,
            note: None,
            sort_order: 0,
            extras: None,
            password: None,
            passphrase: None,
            totp_seed: None,
            key_ref: None,
        };
        let incoming = vec![
            mk("web", "10.0.0.1", 2222, "deploy"),  // 全同 -> 重复
            mk("web2", "10.0.0.1", 2222, "deploy"), // 名字不同
            mk("web", "10.0.0.2", 2222, "deploy"),  // 主机不同
            mk("web", "10.0.0.1", 22, "deploy"),    // 端口不同
            mk("web", "10.0.0.1", 2222, "root"),    // 用户不同
        ];
        let v = plan_merge(&existing, &incoming);
        assert_eq!(
            v,
            vec![
                Verdict::Duplicate,
                Verdict::Import,
                Verdict::Import,
                Verdict::Import,
                Verdict::Import
            ]
        );
    }

    // ——— 端到端 ———

    /// 开保险库的库：TOTP 种子必须有地方加密（未启用时它要走系统凭据服务）。
    fn vault_store(tag: &str) -> (Store, std::path::PathBuf) {
        let (s, d) = store(tag);
        s.vault_enable("master-pw", false).unwrap();
        (s, d)
    }

    #[tokio::test]
    async fn roundtrip_reproduces_sessions_extras_and_credentials() {
        let (src, src_dir) = vault_store("rt-src");
        let key_file = src_dir.join("id_ext");
        std::fs::write(&key_file, real_key_pem()).unwrap();
        let key_path = key_file.to_string_lossy().to_string();

        let a = src.create(new_session("A", "host-a.example", "password", None)).unwrap();
        src.set_password(&a.id, "PW-CANARY-1").unwrap();
        src.set_totp_seed(&a.id, "JBSWY3DPEHPK3PXP").unwrap();
        src.set_extras(
            &a.id,
            &SessionExtras { login_script_on: true, login_script: "ls".into(), compat: true, ..Default::default() },
        )
        .unwrap();
        let b = src.create(new_session("B", "host-b.example", "key", Some(key_path.clone()))).unwrap();
        src.set_passphrase(&b.id, "PASSPHRASE-CANARY").unwrap();
        // 密钥认证但文件缺失
        src.create(new_session("C", "host-c.example", "key", Some("/nonexistent/id_x".into()))).unwrap();

        let out = src_dir.join("all.nexusconn");
        let sum = export_with_kdf(&src, &out, PW, true, tiny()).unwrap();
        assert_eq!((sum.sessions, sum.passwords, sum.passphrases, sum.totp_seeds, sum.keys), (3, 1, 1, 1, 1));
        assert!(sum.warnings.iter().any(|w| w.contains("/nonexistent/id_x")));

        // 摘要里不能出现任何秘密
        let s = serde_json::to_string(&sum).unwrap();
        for secret in ["PW-CANARY-1", "PASSPHRASE-CANARY", "JBSWY3DPEHPK3PXP"] {
            assert!(!s.contains(secret));
        }

        // 文件里没有明文密码 / 主机
        let bytes = std::fs::read(&out).unwrap();
        for needle in ["PW-CANARY-1", "PASSPHRASE-CANARY", "JBSWY3DPEHPK3PXP", "host-a.example", "deploy", "OPENSSH"] {
            assert!(
                !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
                "文件里不该出现明文 {needle}"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&out).unwrap().permissions().mode() & 0o777, 0o600);
        }

        // 导入到另一个（同样开了保险库的）库
        let (dst, dst_dir) = vault_store("rt-dst");
        dst.create(new_session("pre-existing", "other", "agent", None)).unwrap();
        let keys_dir = dst_dir.join(keys::VAULT_DIR);
        let payload = read_and_open(&out, PW).unwrap();
        let r = apply_import(&dst, &keys_dir, payload).await.unwrap();
        assert_eq!((r.imported, r.skipped, r.passwords, r.passphrases, r.totp_seeds, r.keys), (3, 0, 1, 1, 1, 1));

        let list = dst.list().unwrap();
        assert_eq!(list.len(), 4);
        let pre = list.iter().find(|x| x.name == "pre-existing").unwrap();
        let by = |n: &str| list.iter().find(|x| x.name == n).unwrap().clone();
        let (na, nb, nc) = (by("A"), by("B"), by("C"));
        // 排在已有连接之后，且保持源顺序
        assert!(pre.sort_order < na.sort_order && na.sort_order < nb.sort_order && nb.sort_order < nc.sort_order);
        assert_ne!(na.id, a.id, "必须生成新 id");
        assert_eq!(
            (na.folder.as_str(), na.host.as_str(), na.port, na.username.as_str(), na.auth_type.as_str()),
            ("/prod/web", "host-a.example", 2222, "deploy", "password")
        );
        assert_eq!(na.startup_cmd.as_deref(), Some("tmux a"));
        assert_eq!(na.theme.as_deref(), Some("dracula"));
        assert_eq!(na.connect_timeout, Some(7));
        assert_eq!(na.note.as_deref(), Some("备注"));
        assert_eq!(dst.get_password(&na.id).as_deref(), Some("PW-CANARY-1"));
        assert_eq!(dst.get_totp_seed(&na.id).as_deref(), Some("JBSWY3DPEHPK3PXP"));
        let ex = dst.all_extras();
        let e = ex.get(&na.id).expect("extras");
        assert!(e.login_script_on && e.compat && e.login_script == "ls");

        // 私钥被写进了新库的密钥库，路径已改写，口令跟过来了
        assert_eq!(dst.get_passphrase(&nb.id).as_deref(), Some("PASSPHRASE-CANARY"));
        let new_path = nb.key_path.clone().unwrap();
        assert!(Path::new(&new_path).starts_with(&keys_dir), "{new_path}");
        assert_eq!(
            std::fs::read_to_string(&new_path).unwrap(),
            std::fs::read_to_string(&key_file).unwrap()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&new_path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // 缺失私钥的连接保留原路径
        assert_eq!(nc.key_path.as_deref(), Some("/nonexistent/id_x"));
        assert!(r.warnings.iter().any(|w| w.contains("「C」")));

        // 再导入一次：全部是重复，什么都不新增
        let payload = read_and_open(&out, PW).unwrap();
        let r2 = apply_import(&dst, &keys_dir, payload).await.unwrap();
        assert_eq!((r2.imported, r2.skipped, r2.keys), (0, 3, 0));
        assert_eq!(dst.list().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn two_sessions_sharing_a_key_embed_it_once() {
        let (src, dir) = store("share");
        let kf = dir.join("shared");
        std::fs::write(&kf, real_key_pem()).unwrap();
        let p = kf.to_string_lossy().to_string();
        src.create(new_session("A", "a", "key", Some(p.clone()))).unwrap();
        src.create(new_session("B", "b", "key", Some(p))).unwrap();
        let (payload, sum) = collect(&src, true).unwrap();
        assert_eq!((sum.keys, payload.keys.len()), (1, 1));

        let (dst, dd) = store("share-dst");
        let kd = dd.join(keys::VAULT_DIR);
        let r = apply_import(&dst, &kd, payload).await.unwrap();
        assert_eq!((r.imported, r.keys), (2, 1));
        let l = dst.list().unwrap();
        assert_eq!(l[0].key_path, l[1].key_path);
    }

    #[test]
    fn exclude_keys_option_embeds_nothing() {
        let (src, dir) = store("nokeys");
        let kf = dir.join("k");
        std::fs::write(&kf, real_key_pem()).unwrap();
        src.create(new_session("A", "a", "key", Some(kf.to_string_lossy().into()))).unwrap();
        let (p, sum) = collect(&src, false).unwrap();
        assert!(p.keys.is_empty() && p.sessions[0].key_ref.is_none());
        assert_eq!(sum.keys, 0);
        assert!(sum.warnings.iter().any(|w| w.contains("未包含私钥文件")));
    }

    /// 载荷里的 key id / 名字 / keyPath 都是不可信的 ——
    /// 写盘位置只能是 `<keys_dir>/<新 uuid>`。
    #[tokio::test]
    async fn hostile_key_ids_and_names_cannot_escape_the_key_store() {
        let evil = tmp_dir("evil-target");
        let pem = real_key_pem();
        let plain = serde_json::json!({
            "sessions": [{
                "name": "x", "folder": "/", "host": "h", "port": 22, "username": "u",
                "authType": "key", "keyPath": "../../etc/passwd", "sortOrder": 0,
                "keyRef": "../../../escape"
            }],
            "keys": [{ "id": "../../../escape", "name": format!("{}/../pwn", evil.display()), "pem": pem }]
        });
        let file = seal(&serde_json::to_vec(&plain).unwrap(), PW, tiny()).unwrap();
        let f = tmp_dir("evil-file").join("e.nexusconn");
        std::fs::write(&f, file).unwrap();

        let (dst, dd) = store("evil-dst");
        let kd = dd.join(keys::VAULT_DIR);
        let payload = read_and_open(&f, PW).unwrap();
        let r = apply_import(&dst, &kd, payload).await.unwrap();
        assert_eq!(r.keys, 1);
        let s = &dst.list().unwrap()[0];
        let np = std::path::PathBuf::from(s.key_path.clone().unwrap());
        // `import_contents` 的 base 参数是「密钥库目录的上一层」口径，沿用现有 import_key 的传法
        assert_eq!(np.parent().unwrap(), kd.join(keys::VAULT_DIR).as_path());
        assert!(uuid::Uuid::parse_str(np.file_name().unwrap().to_str().unwrap()).is_ok());
        assert_eq!(std::fs::read_dir(&evil).unwrap().count(), 0);
        assert!(!dd.join("escape").exists());
    }

    #[tokio::test]
    async fn garbage_key_in_file_warns_but_still_imports_the_session() {
        let plain = serde_json::json!({
            "sessions": [{
                "name": "x", "folder": "/", "host": "h", "port": 22, "username": "u",
                "authType": "key", "keyPath": "/orig/key", "sortOrder": 0, "keyRef": "k0"
            }],
            "keys": [{ "id": "k0", "name": "bad", "pem": "not a key" }]
        });
        let payload = parse_payload(&serde_json::to_vec(&plain).unwrap()).unwrap();
        let (dst, dd) = store("garbage-key");
        let r = apply_import(&dst, &dd.join(keys::VAULT_DIR), payload).await.unwrap();
        assert_eq!((r.imported, r.keys), (1, 0));
        assert_eq!(dst.list().unwrap()[0].key_path.as_deref(), Some("/orig/key"));
        assert!(r.warnings.iter().any(|w| w.contains("无法恢复")));
    }

    #[tokio::test]
    async fn locked_vault_refuses_both_directions() {
        let (st, dir) = vault_store("locked");
        st.create(new_session("A", "a", "agent", None)).unwrap();
        st.vault_lock();
        let e = export_with_kdf(&st, &dir.join("o.nexusconn"), PW, true, tiny()).unwrap_err();
        assert!(e.contains("解锁"));
        assert!(!dir.join("o.nexusconn").exists());

        let e = apply_import(&st, &dir.join("k"), ConnPayload::default()).await.unwrap_err();
        assert!(e.contains("解锁"));
    }

    #[test]
    fn wrong_password_on_a_real_file_imports_nothing() {
        let (src, dir) = store("wrongpw");
        src.create(new_session("A", "a", "agent", None)).unwrap();
        let out = dir.join("o.nexusconn");
        export_with_kdf(&src, &out, PW, true, tiny()).unwrap();
        let e = read_and_open(&out, "wrong password!").err().unwrap();
        assert!(e.contains("导出密码不对"));
        assert!(!e.contains(PW));
    }
}
