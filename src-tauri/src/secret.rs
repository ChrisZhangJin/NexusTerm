//! 静态加密：把一小段机密（目前只有 TOTP 种子）加密后存进 db 的列里。
//!
//! ## 为什么只有种子加密，密码和私钥口令却是明文
//!
//! 这是有意的，不是漏做。`store::migrate_legacy_secrets` 记录了一个早先的产品
//! 决定：密码从系统凭据服务**搬进** db 明文列，换取「把 nexus.db 拷到另一台机器
//! 就能继续用」。TOTP 种子不能跟着走——它是第二因子，和第一因子躺在同一个文件里
//! 的那一刻，两道门就变成了一道。
//!
//! 所以这里是：**密文进 db，密钥进系统凭据服务**。威胁模型很具体——有人拿到了
//! `nexus.db`（备份盘、同步目录、误传的压缩包）。他能读到密码，但读不到种子，
//! 二次验证仍然拦得住他。代价是 db 不再整库可搬：换机器要重填种子。对第二因子
//! 来说，这个代价本来就该付。
//!
//! 密钥如果也放在 db 或 app 目录里，上面那一整段就不成立了 —— 那是混淆，不是加密。

use std::sync::Mutex;

use aes_gcm::aead::{Aead, KeyInit, OsRng as AeadRng};
use aes_gcm::{AeadCore, Aes256Gcm, Nonce};
use base64::Engine as _;
use rand_core::RngCore;

/// 系统凭据服务里的条目。和 `store` 里那个只用于一次性迁移的
/// `com.nexus-term.sessions` 是两回事，别混用。
const SERVICE: &str = "com.nexus-term.secrets";
const KEY_ENTRY: &str = "master-key";

/// 密文前缀。将来换算法/换密钥时靠它区分旧数据，不至于把一堆 base64 喂给
/// 新解密器再去猜为什么失败。
const V1: &str = "v1:";

/// AES-GCM 的 nonce 长度（96 bit，RFC 5116 推荐值）。
const NONCE_LEN: usize = 12;

/// 串行化「读密钥，没有就生成一把存进去」。两个连接同时解密时各自去
/// 凭据服务取密钥本身没问题，但首次生成必须只发生一次，否则后生成的那把会
/// 覆盖先生成的，而先那把已经加密过数据了。
static KEY_LOCK: Mutex<()> = Mutex::new(());

fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// 取主密钥；第一次调用时生成并写入系统凭据服务。
fn master_key() -> Result<[u8; 32], String> {
    let _guard = KEY_LOCK.lock().map_err(|_| "密钥锁已损坏".to_string())?;

    let entry = keyring::Entry::new(SERVICE, KEY_ENTRY).map_err(unavailable)?;
    match entry.get_password() {
        Ok(stored) => {
            let raw = b64()
                .decode(stored.trim())
                .map_err(|_| "系统凭据服务里的主密钥已损坏".to_string())?;
            let key: [u8; 32] = raw
                .try_into()
                .map_err(|_| "系统凭据服务里的主密钥长度不对".to_string())?;
            Ok(key)
        }
        Err(keyring::Error::NoEntry) => {
            let mut key = [0u8; 32];
            // rand_core 的 OsRng 直接读系统 CSPRNG，不经用户态种子。
            rand_core::OsRng.fill_bytes(&mut key);
            entry
                .set_password(&b64().encode(key))
                .map_err(|e| format!("无法把主密钥写入系统凭据服务：{}", e))?;
            tracing::info!("已生成本机主密钥并存入系统凭据服务");
            Ok(key)
        }
        Err(e) => Err(unavailable(e)),
    }
}

/// 凭据服务整体不可用时的提示。Linux 上最常见：无图形会话 / 没装
/// gnome-keyring 或 kwallet / keyring 被锁着。
fn unavailable(e: keyring::Error) -> String {
    format!(
        "无法访问系统凭据服务，TOTP 种子不能安全保存。\
         Linux 下请确认已登录图形会话且安装了 gnome-keyring 或 kwallet。\n\n原始错误：{}",
        e
    )
}

/// 加密成可以直接塞进 db 文本列的一串。
pub fn encrypt(plain: &str) -> Result<String, String> {
    let key = master_key()?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| e.to_string())?;
    let nonce = Aes256Gcm::generate_nonce(&mut AeadRng);
    let ct = cipher
        .encrypt(&nonce, plain.as_bytes())
        .map_err(|_| "加密失败".to_string())?;

    let mut blob = Vec::with_capacity(NONCE_LEN + ct.len());
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(&ct);
    Ok(format!("{}{}", V1, b64().encode(blob)))
}

/// 解密 [`encrypt`] 的产物。
///
/// 失败的常见原因不是「被篡改」，而是**换了台机器**：db 跟着走了，凭据服务里的
/// 密钥没有。文案要指向「重填种子」，而不是吓唬人说数据被改过。
pub fn decrypt(blob: &str) -> Result<String, String> {
    let body = blob
        .strip_prefix(V1)
        .ok_or_else(|| "密文格式无法识别（缺少版本前缀）".to_string())?;
    let raw = b64()
        .decode(body.trim())
        .map_err(|_| "密文不是合法的 base64".to_string())?;
    if raw.len() <= NONCE_LEN {
        return Err("密文过短，已损坏".into());
    }
    let (nonce, ct) = raw.split_at(NONCE_LEN);

    let key = master_key()?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| e.to_string())?;
    let plain = cipher.decrypt(Nonce::from_slice(nonce), ct).map_err(|_| {
        "无法解密已保存的 TOTP 种子。密钥存在本机的系统凭据服务里，\
         数据库换了机器就解不开 —— 请编辑该连接重新填一次种子。"
            .to_string()
    })?;
    String::from_utf8(plain).map_err(|_| "解密结果不是合法文本".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 需要真的系统凭据服务，CI / 容器里通常没有，所以默认不跑。
    /// 本机验证：`cargo test --lib secret:: -- --ignored --test-threads=1`
    #[test]
    #[ignore = "需要系统凭据服务（keychain / credential manager / secret-service）"]
    fn roundtrip() {
        let plain = "JBSWY3DPEHPK3PXP";
        let blob = encrypt(plain).unwrap();
        assert!(blob.starts_with(V1));
        assert!(!blob.contains(plain), "密文里不该出现明文");
        assert_eq!(decrypt(&blob).unwrap(), plain);
    }

    /// 每次加密用新 nonce：同一段明文两次加密的密文必须不同，
    /// 否则「这两个连接用的是同一个种子」从 db 里一眼可见。
    #[test]
    #[ignore = "需要系统凭据服务"]
    fn same_plaintext_encrypts_differently() {
        assert_ne!(encrypt("same").unwrap(), encrypt("same").unwrap());
    }

    #[test]
    #[ignore = "需要系统凭据服务"]
    fn rejects_tampered_ciphertext() {
        let blob = encrypt("JBSWY3DPEHPK3PXP").unwrap();
        let mut raw = b64().decode(blob.strip_prefix(V1).unwrap()).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0x01;
        let tampered = format!("{}{}", V1, b64().encode(&raw));
        assert!(decrypt(&tampered).is_err());
    }

    /// 这几条不碰凭据服务，任何环境都能跑。
    #[test]
    fn rejects_malformed_blobs_before_touching_the_keyring() {
        assert!(decrypt("没有前缀").is_err());
        assert!(decrypt("v1:这不是base64!!!").is_err());
        assert!(decrypt(&format!("{}{}", V1, b64().encode([0u8; 4]))).is_err());
    }
}
