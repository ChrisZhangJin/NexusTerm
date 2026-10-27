//! 私钥的读取、生成与「用户密钥库」。
//!
//! 密钥库是 Xshell「用户密钥」的对应物：私钥集中放在应用数据目录下的
//! `keys/` 里，元数据（名字、类型、长度、注释、指纹）记在 sqlite。
//! 这样做的理由不是好看 —— `~/.ssh` 里的密钥属于系统工具链，删改会牵连
//! git / ssh 本身；用户自己导入或生成的密钥需要一个**只属于本应用**的落脚地，
//! 也才能在应用内完成生成、导出、查看公钥这些操作。
//!
//! `~/.ssh` 下已有的密钥仍然会被列出来（标成外部密钥、只读），
//! 免得用户为了用本应用还要先把密钥复制一份。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use russh::keys::PrivateKey;
use serde::Serialize;
use ssh_key::{LineEnding, PublicKey};

use crate::store::{KeyRow, Store};

pub mod ppk;

/// 按内容识别格式再解析 —— PPK 与 OpenSSH 共用这一个入口。
///
/// `passphrase` 只对真正需要它的格式生效：OpenSSH 明文私钥、PPK 的
/// `Encryption: none` 都会忽略它。
fn parse_private_key(contents: &str, passphrase: Option<&str>) -> Result<PrivateKey, String> {
    if ppk::looks_like_ppk(contents) {
        return ppk::parse(contents, passphrase);
    }

    let key = PrivateKey::from_openssh(contents.as_bytes()).map_err(|e| {
        format!(
            "无法解析私钥：{e}。支持 OpenSSH（-----BEGIN OPENSSH PRIVATE KEY-----）\
             与 PuTTY 的 .ppk；老式 PEM（BEGIN RSA PRIVATE KEY）请先用 \
             ssh-keygen -m PEM 转换"
        )
    })?;

    if !key.is_encrypted() {
        return Ok(key);
    }
    let phrase = passphrase
        .filter(|p| !p.is_empty())
        .ok_or_else(|| "该私钥已加密，需要提供密码".to_string())?;
    key.decrypt(phrase.as_bytes())
        .map_err(|_| "私钥密码不正确".to_string())
}

/// 只读公钥与加密状态，用于列表展示 —— 不需要口令。
///
/// 两种格式的加密私钥都能给出公钥：OpenSSH 的公钥在 PEM 明文段里，
/// PPK 的公钥段本来就是明文。
fn read_public(contents: &str) -> Option<(PublicKey, bool)> {
    if ppk::looks_like_ppk(contents) {
        let key = ppk::public_key_only(contents)?;
        let encrypted = ppk::is_encrypted(contents)?;
        return Some((key, encrypted));
    }
    let key = PrivateKey::from_openssh(contents.as_bytes()).ok()?;
    Some((key.public_key().clone(), key.is_encrypted()))
}

/// 私钥文件里自带的注释。
///
/// OpenSSH 把注释附在公钥末尾，PPK 则在头部单独一行，所以不能混用。
fn embedded_comment(contents: &str, pubkey: &PublicKey) -> String {
    if ppk::looks_like_ppk(contents) {
        ppk::comment(contents).unwrap_or_default()
    } else {
        pubkey.comment().to_string()
    }
}

/// 密钥库目录名（位于应用数据目录下）。
pub const VAULT_DIR: &str = "keys";

/// 一条可供选择的密钥。外部密钥（`~/.ssh`）与库内密钥共用这个结构，
/// 靠 `source` 区分，前端据它决定「删除 / 导出」是否可用。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyInfo {
    /// 库内密钥用 uuid；外部密钥用 `file:<绝对路径>`（稳定且能反查）
    pub id: String,
    pub name: String,
    /// vault | sshDir
    pub source: String,
    /// 归一化类型名：RSA / ED25519 / ECDSA / 其它
    pub key_type: String,
    /// 原始算法名，如 ssh-rsa、ssh-ed25519
    pub algorithm: String,
    /// 位数；无法判定时为 0
    pub bits: u32,
    pub comment: String,
    pub fingerprint: String,
    pub encrypted: bool,
    pub path: String,
    /// 公钥单行文本（可直接贴进 authorized_keys）
    pub public_key: String,
    pub created_at: i64,
}

/// 生成密钥时的可选项。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateOptions {
    /// ed25519 | rsa
    pub algorithm: String,
    /// 仅 RSA 有意义：2048 / 3072 / 4096
    pub bits: Option<u32>,
    pub comment: Option<String>,
    /// 不为空则私钥落盘时用该口令加密
    pub passphrase: Option<String>,
    pub name: Option<String>,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn vault_path(base: &Path) -> PathBuf {
    base.join(VAULT_DIR)
}

fn vault_file(base: &Path, id: &str) -> PathBuf {
    vault_path(base).join(id)
}

fn id_for_path(path: &Path) -> String {
    format!("file:{}", path.to_string_lossy())
}

/// 加载并按需解密 OpenSSH 私钥。
pub async fn load_private_key(
    path: &str,
    passphrase: Option<String>,
) -> Result<PrivateKey, String> {
    let expanded = expand_tilde(path);
    let contents = tokio::fs::read_to_string(&expanded)
        .await
        .map_err(|e| format!("无法读取私钥 {}: {}", expanded, e))?;

    parse_private_key(&contents, passphrase.as_deref())
        .map_err(|e| format!("{e}（{}）", expanded))
}

// ——— 库操作 ———

/// 列出全部可用密钥：库内的在前，`~/.ssh` 的外部密钥在后。
///
/// 库内条目若私钥文件已被手工删除，这里顺手跳过并清掉元数据 ——
/// 否则界面上会留下一条永远连不上的僵尸记录。
pub async fn list_all(store: &Store, _base: &Path) -> Vec<KeyInfo> {
    let mut out = Vec::new();

    let mut stale = Vec::new();
    for row in store.list_keys() {
        if !Path::new(&row.path).exists() {
            stale.push(row.id.clone());
            continue;
        }
        if let Some(info) = describe(
            Path::new(&row.path),
            row.id.clone(),
            row.name.clone(),
            "vault",
            Some(row.comment.clone()),
            row.created_at,
        )
        .await
        {
            out.push(info);
        } else {
            stale.push(row.id.clone());
        }
    }
    for id in stale {
        tracing::warn!(key = %id, "密钥库记录的私钥文件已不存在，清理元数据");
        store.delete_key(&id);
    }

    for k in discover_keys().await {
        let path = PathBuf::from(&k.path);
        let display = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&k.name)
            .to_string();
        if let Some(info) = describe(&path, id_for_path(&path), display, "sshDir", None, 0).await {
            out.push(info);
        }
    }

    out
}

/// 读取一条密钥的展示信息。读不出（文件没了 / 格式坏）返回 None。
async fn describe(
    path: &Path,
    id: String,
    name: String,
    source: &str,
    comment_override: Option<String>,
    created_at: i64,
) -> Option<KeyInfo> {
    let contents = tokio::fs::read_to_string(path).await.ok()?;
    // 用「只读公钥」路径：加密私钥也能列出来，不需要用户先输口令
    let (pubkey, encrypted) = read_public(&contents)?;

    let algorithm = pubkey.algorithm().to_string();
    let key_type = normalize_type(&algorithm);
    let bits = key_bits(&pubkey);
    let fingerprint = pubkey.fingerprint(ssh_key::HashAlg::Sha256).to_string();
    let public_key = pubkey.to_openssh().unwrap_or_default();

    // 注释的优先级：库里的自定义注释 > .pub 文件 > 私钥内嵌注释
    let comment = match comment_override.filter(|c| !c.is_empty()) {
        Some(c) => c,
        None => read_pub_comment(path)
            .await
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| embedded_comment(&contents, &pubkey)),
    };

    Some(KeyInfo {
        id,
        name,
        source: source.to_string(),
        key_type,
        algorithm,
        bits,
        comment,
        fingerprint,
        encrypted,
        path: path.to_string_lossy().to_string(),
        public_key,
        created_at,
    })
}

async fn read_pub_comment(path: &Path) -> Option<String> {
    let mut pub_path = path.as_os_str().to_owned();
    pub_path.push(".pub");
    let contents = tokio::fs::read_to_string(PathBuf::from(pub_path))
        .await
        .ok()?;
    let key = PublicKey::from_openssh(contents.trim()).ok()?;
    Some(key.comment().to_string())
}

fn normalize_type(algorithm: &str) -> String {
    if algorithm.starts_with("ssh-rsa") || algorithm.starts_with("rsa-sha2") {
        "RSA".into()
    } else if algorithm.starts_with("ssh-ed25519") {
        "ED25519".into()
    } else if algorithm.starts_with("ecdsa-") {
        "ECDSA".into()
    } else if algorithm.starts_with("sk-") {
        "FIDO".into()
    } else {
        algorithm.to_string()
    }
}

fn key_bits(pubkey: &PublicKey) -> u32 {
    match pubkey.key_data() {
        ssh_key::public::KeyData::Rsa(k) => {
            // modulus 去掉前导零后的有效位长度
            let bytes = k.n.as_positive_bytes().unwrap_or(&[]);
            let Some(first) = bytes.iter().position(|b| *b != 0) else {
                return 0;
            };
            let lead = bytes[first];
            let rest = (bytes.len() - first - 1) as u32;
            rest * 8 + (8 - lead.leading_zeros())
        }
        ssh_key::public::KeyData::Ecdsa(k) => match k.curve() {
            ssh_key::EcdsaCurve::NistP256 => 256,
            ssh_key::EcdsaCurve::NistP384 => 384,
            ssh_key::EcdsaCurve::NistP521 => 521,
        },
        ssh_key::public::KeyData::Ed25519(_) => 256,
        _ => 0,
    }
}

/// 导入私钥到密钥库。
///
/// 加密私钥允许**不带口令**导入（用户可能暂时不想输入），
/// 但带了就必须验证 —— 否则连上时才发现口令错，等于把错误推迟到最糟的时机。
pub async fn import(
    store: &Store,
    base: &Path,
    src: &str,
    name: Option<String>,
    passphrase: Option<String>,
) -> Result<KeyInfo, String> {
    let src_path = PathBuf::from(expand_tilde(src));
    if !src_path.is_file() {
        return Err(format!("找不到私钥文件：{}", src_path.to_string_lossy()));
    }

    let contents = tokio::fs::read_to_string(&src_path)
        .await
        .map_err(|e| format!("无法读取私钥：{}", e))?;
    let (_, encrypted) = read_public(&contents)
        .ok_or("无法解析私钥：无法识别的格式（支持 OpenSSH 与 PuTTY 的 .ppk）")?;

    // 给了口令就顺带校验一次；没给也允许导入 —— 加密私钥可以等到连接时再问
    if encrypted {
        if let Some(p) = passphrase.as_deref().filter(|p| !p.is_empty()) {
            parse_private_key(&contents, Some(p))?;
        }
    } else {
        // 明文也要完整解析一遍：PPK 要验 MAC，损坏文件不该进库
        parse_private_key(&contents, None)?;
    }

    let id = uuid::Uuid::new_v4().to_string();
    let dest = vault_file(base, &id);
    tokio::fs::create_dir_all(vault_path(base))
        .await
        .map_err(|e| format!("无法创建密钥库目录：{}", e))?;

    // 原样复制字节，不重新编码：重新编码会丢掉原文件的换行风格与注释，
    // 也会把「带口令」这个状态意外改掉。
    tokio::fs::write(&dest, contents.as_bytes())
        .await
        .map_err(|e| format!("写入密钥库失败：{}", e))?;

    let fallback = src_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("imported-key")
        .to_string();
    let final_name = name
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or(fallback);

    finish_key(store, &dest, id, final_name, None).await
}

/// 生成新密钥（ed25519 或 RSA）。RSA 默认 4096 —— 现代机器几秒钟能出，
/// 而 2048 在部分合规要求下已经不达标。
pub async fn generate(
    store: &Store,
    base: &Path,
    opts: GenerateOptions,
) -> Result<KeyInfo, String> {
    let algo = opts.algorithm.to_lowercase();
    let bits = match algo.as_str() {
        "ed25519" => 256,
        "rsa" => {
            let b = opts.bits.unwrap_or(4096);
            if !(2048..=8192).contains(&b) {
                return Err("RSA 长度需在 2048–8192 位之间".into());
            }
            b
        }
        other => return Err(format!("不支持的密钥算法：{}", other)),
    };

    let comment = opts.comment.unwrap_or_default();
    let passphrase = opts.passphrase.filter(|p| !p.is_empty());

    // RSA 4096 的生成是纯 CPU 活（可能几秒），放到阻塞线程池，
    // 否则会把整个 tokio 调度器卡住 —— 界面上连进度都不会动。
    let comment_for_task = comment.clone();
    let algo_for_task = algo.clone();
    let generated = tokio::task::spawn_blocking(move || -> Result<PrivateKey, String> {
        let mut rng = rand_core::OsRng;
        let mut key = if algo_for_task == "ed25519" {
            PrivateKey::random(&mut rng, ssh_key::Algorithm::Ed25519)
                .map_err(|e| e.to_string())?
        } else {
            // PrivateKey::random 的 RSA 分支固定 4096 位，要自选长度只能直接构造
            let kp = ssh_key::private::RsaKeypair::random(&mut rng, bits as usize)
                .map_err(|e| e.to_string())?;
            PrivateKey::new(ssh_key::private::KeypairData::from(kp), &comment_for_task)
                .map_err(|e| e.to_string())?
        };
        if !comment_for_task.is_empty() {
            key.set_comment(comment_for_task);
        }
        if let Some(p) = passphrase {
            key = key
                .encrypt(&mut rng, p.as_bytes())
                .map_err(|e| format!("加密私钥失败：{}", e))?;
        }
        Ok(key)
    })
    .await
    .map_err(|e| format!("生成密钥任务失败：{}", e))??;

    let id = uuid::Uuid::new_v4().to_string();
    let dest = vault_file(base, &id);
    tokio::fs::create_dir_all(vault_path(base))
        .await
        .map_err(|e| format!("无法创建密钥库目录：{}", e))?;

    let pem = generated
        .to_openssh(LineEnding::LF)
        .map_err(|e| format!("导出私钥失败：{}", e))?;
    tokio::fs::write(&dest, pem.as_bytes())
        .await
        .map_err(|e| format!("写入密钥库失败：{}", e))?;

    let default_name = format!(
        "{}-{}",
        if algo == "ed25519" {
            "id_ed25519"
        } else {
            "id_rsa"
        },
        chrono::Local::now().format("%Y%m%d")
    );
    let name = opts
        .name
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or(default_name);

    finish_key(store, &dest, id, name, None).await
}

/// 写入 .pub、收紧权限、登记元数据 —— 导入与生成共用的收尾。
async fn finish_key(
    store: &Store,
    path: &Path,
    id: String,
    name: String,
    comment: Option<String>,
) -> Result<KeyInfo, String> {
    let contents = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| format!("读取刚写入的私钥失败：{}", e))?;
    let (pubkey, encrypted) = read_public(&contents)
        .ok_or_else(|| format!("解析刚写入的私钥失败：{}", path.display()))?;

    // 私钥文件只能本人可读（Windows 无此语义，由 ACL 兜底）
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }

    let public_line = pubkey.to_openssh().unwrap_or_default();
    let mut pub_os = path.as_os_str().to_owned();
    pub_os.push(".pub");
    let _ = tokio::fs::write(PathBuf::from(pub_os), format!("{}\n", public_line.trim())).await;

    let algorithm = pubkey.algorithm().to_string();
    let row = KeyRow {
        id: id.clone(),
        name: name.clone(),
        path: path.to_string_lossy().to_string(),
        key_type: normalize_type(&algorithm),
        bits: key_bits(&pubkey),
        comment: comment
            .clone()
            .unwrap_or_else(|| embedded_comment(&contents, &pubkey)),
        fingerprint: pubkey.fingerprint(ssh_key::HashAlg::Sha256).to_string(),
        encrypted,
        created_at: now_secs(),
    };
    store.insert_key(&row)?;

    describe(path, id, name, "vault", comment, row.created_at)
        .await
        .ok_or_else(|| "密钥已保存但无法读取元信息".to_string())
}

/// 删除库内密钥（连同 .pub）。外部密钥（`~/.ssh`）拒绝删除 ——
/// 那是 ssh / git 的资产，不该由本应用悄悄动。
pub async fn delete(store: &Store, id: &str) -> Result<(), String> {
    let Some(row) = store.delete_key(id) else {
        return Err("密钥不存在".into());
    };
    let path = PathBuf::from(&row.path);
    let _ = tokio::fs::remove_file(&path).await;
    let mut pub_os = path.as_os_str().to_owned();
    pub_os.push(".pub");
    let _ = tokio::fs::remove_file(PathBuf::from(pub_os)).await;
    Ok(())
}

/// 导出私钥到指定路径，并在旁边写一份 `.pub`。
pub async fn export(store: &Store, id: &str, dest: &str) -> Result<(), String> {
    let row = store.get_key(id).ok_or_else(|| "密钥不存在".to_string())?;
    let src = PathBuf::from(&row.path);
    let contents = tokio::fs::read(&src)
        .await
        .map_err(|e| format!("读取私钥失败：{}", e))?;

    let dest_path = PathBuf::from(expand_tilde(dest));
    if let Some(parent) = dest_path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("无法创建导出目录：{}", e))?;
        }
    }
    tokio::fs::write(&dest_path, &contents)
        .await
        .map_err(|e| format!("写入导出文件失败：{}", e))?;

    let mut pub_src_os = src.as_os_str().to_owned();
    pub_src_os.push(".pub");
    if let Ok(pub_contents) = tokio::fs::read(PathBuf::from(pub_src_os)).await {
        let mut dest_os = dest_path.as_os_str().to_owned();
        dest_os.push(".pub");
        let _ = tokio::fs::write(PathBuf::from(dest_os), pub_contents).await;
    }
    Ok(())
}

/// 重命名 / 修改注释。
pub fn update_meta(
    store: &Store,
    id: &str,
    name: Option<String>,
    comment: Option<String>,
) -> Result<(), String> {
    store.update_key_meta(
        id,
        name.as_deref().map(str::trim).filter(|n| !n.is_empty()),
        comment.as_deref(),
    )
}

/// 读取单条密钥的公钥（属性面板「复制公钥」用）。
pub async fn public_key_of(store: &Store, id: &str) -> Result<String, String> {
    let row = store.get_key(id).ok_or_else(|| "密钥不存在".to_string())?;
    let info = describe(
        Path::new(&row.path),
        row.id,
        row.name,
        "vault",
        Some(row.comment),
        row.created_at,
    )
    .await
    .ok_or_else(|| "无法读取密钥".to_string())?;
    Ok(info.public_key)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 家目录解析错会让「列出 ~/.ssh 密钥」静默返回空列表 —— 用户看到的是
    /// 「没有可用私钥」，很难联想到是 HOME / USERPROFILE 取错了，故锁死。
    #[test]
    fn home_dir_points_to_existing_directory() {
        let home = home_dir();
        assert!(!home.is_empty(), "家目录解析为空");
        assert!(
            std::path::Path::new(&home).exists(),
            "家目录不存在：{}",
            home
        );
    }

    /// `~/.ssh/id_ed25519` 展开后必须用本平台的分隔符，
    /// Windows 上混用 `/` 虽能打开但路径显示是错的，且拼接逻辑不再可信。
    #[test]
    fn expand_tilde_uses_platform_separator() {
        let p = expand_tilde("~/.ssh/id_ed25519");
        assert!(p.ends_with("id_ed25519"), "展开结果不正确：{}", p);
        if cfg!(windows) {
            assert!(p.contains(".ssh\\id_ed25519"), "Windows 上应为反斜杠：{}", p);
        } else {
            assert!(p.contains(".ssh/id_ed25519"), "Unix 上应为正斜杠：{}", p);
        }
    }

    #[test]
    fn type_names_are_normalized() {
        assert_eq!(normalize_type("ssh-rsa"), "RSA");
        assert_eq!(normalize_type("rsa-sha2-512"), "RSA");
        assert_eq!(normalize_type("ssh-ed25519"), "ED25519");
        assert_eq!(normalize_type("ecdsa-sha2-nistp256"), "ECDSA");
    }

    async fn temp_store() -> (Store, PathBuf) {
        let dir = std::env::temp_dir().join(format!("nexus-keys-test-{}", uuid::Uuid::new_v4()));
        let store = Store::open(dir.clone()).expect("临时库打开失败");
        (store, dir)
    }

    /// 生成 ed25519：类型 / 长度 / 公钥都要对得上，私钥要真的能读出来。
    #[tokio::test]
    async fn generate_ed25519_round_trips() {
        let (store, dir) = temp_store().await;
        let info = generate(
            &store,
            &dir,
            GenerateOptions {
                algorithm: "ed25519".into(),
                bits: None,
                comment: Some("nexus@test".into()),
                passphrase: None,
                name: Some("我的测试密钥".into()),
            },
        )
        .await
        .expect("生成失败");

        assert_eq!(info.key_type, "ED25519");
        assert_eq!(info.bits, 256);
        assert!(!info.encrypted);
        assert_eq!(info.comment, "nexus@test");
        assert!(info.public_key.starts_with("ssh-ed25519 "));
        assert!(info.fingerprint.starts_with("SHA256:"));
        assert!(Path::new(&info.path).exists());
        // 公钥文件必须一起写出来，否则「复制公钥」之外的 scp 流程会缺文件
        assert!(Path::new(&format!("{}.pub", info.path)).exists());

        let key = load_private_key(&info.path, None).await.expect("读不回生成的私钥");
        assert_eq!(key.algorithm().to_string(), "ssh-ed25519");

        // 本机 ~/.ssh 里可能本来就有密钥，这里只关心库内的那一条
        let listed = list_all(&store, &dir).await;
        let vault: Vec<_> = listed.iter().filter(|k| k.source == "vault").collect();
        assert_eq!(vault.len(), 1);
        assert_eq!(vault[0].name, "我的测试密钥");
    }

    /// 加密私钥：不给口令读不出来，给错口令报「不正确」，给对了才能用。
    /// 这是「连接时弹窗问口令」能成立的前提。
    #[tokio::test]
    async fn encrypted_key_requires_correct_passphrase() {
        let (store, dir) = temp_store().await;
        let info = generate(
            &store,
            &dir,
            GenerateOptions {
                algorithm: "rsa".into(),
                bits: Some(2048),
                comment: None,
                passphrase: Some("s3cret".into()),
                name: None,
            },
        )
        .await
        .expect("生成 RSA 失败");

        assert!(info.encrypted, "填了口令就必须落盘为加密状态");
        assert_eq!(info.key_type, "RSA");
        assert_eq!(info.bits, 2048);

        let err = load_private_key(&info.path, None).await.unwrap_err();
        assert!(err.contains("需要提供密码"), "实际：{err}");

        let err = load_private_key(&info.path, Some("wrong".into()))
            .await
            .unwrap_err();
        assert!(err.contains("不正确"), "实际：{err}");

        assert!(load_private_key(&info.path, Some("s3cret".into()))
            .await
            .is_ok());
    }

    /// 导入 = 原样复制。加密状态、注释、公钥都必须保持不变。
    #[tokio::test]
    async fn import_keeps_encryption_and_comment() {
        let (store, dir) = temp_store().await;

        // 先造一把带口令的密钥当作「外部文件」
        let src_dir = std::env::temp_dir().join(format!("nexus-src-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&src_dir).await.unwrap();
        let src = generate(
            &store,
            &dir,
            GenerateOptions {
                algorithm: "ed25519".into(),
                bits: None,
                comment: Some("import-me".into()),
                passphrase: Some("pw".into()),
                name: None,
            },
        )
        .await
        .unwrap();
        let external = src_dir.join("id_test");
        tokio::fs::copy(&src.path, &external).await.unwrap();

        let imported = import(&store, &dir, external.to_str().unwrap(), Some("外部密钥".into()), None)
            .await
            .expect("导入失败");

        assert!(imported.encrypted, "加密状态必须保留");
        assert_eq!(imported.name, "外部密钥");
        assert_ne!(imported.path, external.to_string_lossy());
        assert!(load_private_key(&imported.path, Some("pw".into())).await.is_ok());
    }

    /// 导入时给了错口令要立刻拒绝 —— 否则错误会拖到连接那一刻才暴露。
    #[tokio::test]
    async fn import_rejects_wrong_passphrase() {
        let (store, dir) = temp_store().await;
        let src_dir = std::env::temp_dir().join(format!("nexus-src-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&src_dir).await.unwrap();
        let src = generate(
            &store,
            &dir,
            GenerateOptions {
                algorithm: "ed25519".into(),
                bits: None,
                comment: None,
                passphrase: Some("pw".into()),
                name: None,
            },
        )
        .await
        .unwrap();
        let external = src_dir.join("id_test");
        tokio::fs::copy(&src.path, &external).await.unwrap();

        let err = import(&store, &dir, external.to_str().unwrap(), None, Some("bad".into()))
            .await
            .unwrap_err();
        assert!(err.contains("不正确"), "实际：{err}");
    }

    /// 删除要同时带走私钥与 .pub，且元数据不能留僵尸。
    #[tokio::test]
    async fn delete_removes_files_and_metadata() {
        let (store, dir) = temp_store().await;
        let info = generate(
            &store,
            &dir,
            GenerateOptions {
                algorithm: "ed25519".into(),
                bits: None,
                comment: None,
                passphrase: None,
                name: None,
            },
        )
        .await
        .unwrap();

        delete(&store, &info.id).await.expect("删除失败");
        assert!(!Path::new(&info.path).exists());
        assert!(!Path::new(&format!("{}.pub", info.path)).exists());
        assert!(store.list_keys().is_empty());
    }
}

/// 列出 ~/.ssh/ 下可用的私钥
pub async fn discover_keys() -> Vec<DiscoveredKey> {
    let dir = dirs_ssh();
    let mut out = Vec::new();
    let Ok(mut rd) = tokio::fs::read_dir(&dir).await else {
        return out;
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // 私钥文件没有扩展名；跳过 .pub / known_hosts / config 等
        if name.ends_with(".pub")
            || name == "known_hosts"
            || name == "known_hosts.old"
            || name == "config"
            || name == "authorized_keys"
            || name.starts_with('.')
        {
            continue;
        }
        if let Ok(meta) = entry.metadata().await {
            if !meta.is_file() {
                continue;
            }
        }
        if let Some(info) = inspect_key(&path).await {
            out.push(info);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredKey {
    pub name: String,
    pub path: String,
    pub key_type: String,
    pub fingerprint: Option<String>,
    pub encrypted: bool,
}

async fn inspect_key(path: &Path) -> Option<DiscoveredKey> {
    let contents = tokio::fs::read_to_string(path).await.ok()?;
    let (pubkey, encrypted) = read_public(&contents)?;

    let key_type = pubkey.algorithm().to_string();
    let fingerprint = pubkey
        .fingerprint(russh::keys::HashAlg::Sha256)
        .to_string()
        .into();

    Some(DiscoveredKey {
        name: path.file_name()?.to_str()?.to_string(),
        path: path.to_str()?.to_string(),
        key_type,
        fingerprint,
        encrypted,
    })
}

/// 用户家目录。
///
/// Windows 上 `HOME` 通常不存在（只有 Git Bash / Cygwin 才会设，且值是 POSIX 风格
/// 的 `/c/Users/xxx`），直接拿去拼路径会得到 Rust 打不开的路径。必须以
/// `USERPROFILE` 为准；反过来 Unix 上没有 `USERPROFILE`，故按平台分支取。
#[cfg(windows)]
pub fn home_dir() -> String {
    std::env::var("USERPROFILE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("HOME").ok())
        .unwrap_or_else(|| "C:\\Users\\Public".to_string())
}

#[cfg(not(windows))]
pub fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".to_string())
}

fn dirs_ssh() -> String {
    // 用 PathBuf 拼接，分隔符在 Windows 上自动是 `\`
    std::path::Path::new(&home_dir())
        .join(".ssh")
        .to_string_lossy()
        .to_string()
}

pub fn expand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        // 用户填的路径分隔符可能是 `/`，逐段拼才能在两端都得到合法路径
        let mut out = std::path::PathBuf::from(home_dir());
        for seg in rest.split('/') {
            out.push(seg);
        }
        out.to_string_lossy().to_string()
    } else if p == "~" {
        home_dir()
    } else {
        p.to_string()
    }
}
