use std::path::Path;

use russh::keys::PrivateKey;

/// 加载并按需解密 OpenSSH 私钥。
pub async fn load_private_key(
    path: &str,
    passphrase: Option<String>,
) -> Result<PrivateKey, String> {
    let expanded = expand_tilde(path);
    let contents = tokio::fs::read_to_string(&expanded)
        .await
        .map_err(|e| format!("无法读取私钥 {}: {}", expanded, e))?;

    // ssh-key 0.6 只提供 OpenSSH 格式入口，这已覆盖绝大多数场景
    //（ssh-keygen 自 7.8 起的默认输出就是该格式）。
    // 遗留的 PKCS#1 PEM（BEGIN RSA PRIVATE KEY）需用户先转换。
    let key = PrivateKey::from_openssh(contents.as_bytes()).map_err(|e| {
        format!(
            "无法解析私钥 {}: {}。若是 PEM 老格式，请先用 ssh-keygen -m PEM 转换为 OpenSSH 格式",
            expanded, e
        )
    })?;

    if key.is_encrypted() {
        let phrase = passphrase.ok_or_else(|| "该私钥已加密，需要提供密码".to_string())?;
        key.decrypt(phrase.as_bytes())
            .map_err(|_| "密码错误，无法解密私钥".to_string())
    } else {
        Ok(key)
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
    let key = PrivateKey::from_openssh(contents.as_bytes()).ok()?;

    let encrypted = key.is_encrypted();
    let key_type = key.algorithm().to_string();
    let fingerprint = key
        .public_key()
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

fn dirs_ssh() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/unknown".into());
    format!("{}/.ssh", home)
}

pub fn expand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{}/{}", home, rest)
    } else if p == "~" {
        std::env::var("HOME").unwrap_or_default()
    } else {
        p.to_string()
    }
}
