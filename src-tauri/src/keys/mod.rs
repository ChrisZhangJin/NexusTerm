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
