//! Xshell 会话文件（.xsh）的导入与导出。
//!
//! `.xsh` 是 NetSarang 公开文档化的 INI 变体（不是私有二进制格式），要点：
//!
//! - **编码**：Xshell 写出的文件是 UTF-16LE（带 BOM）。读入必须检测 BOM，
//!   否则按 UTF-8 解会得到一堆 `\x00` 夹杂的乱键名；导出同样写 UTF-16LE+BOM，
//!   保证 Xshell 直接打开不乱码。
//! - **密码**：`[CONNECTION:AUTHENTICATION:PASSWORD]` 下的 `Password` 是加密串，
//!   与原机器绑定，跨机器导入无法解密 —— 一律丢弃，导入后由用户重填。
//!   导出时同理只写空值，绝不落明文。
//! - **关键节**（已用本机 ~/Tools/Sessions 下的 Xshell 8.1 真实产出核对）：
//!   ```ini
//!   [CONNECTION]
//!   Host=192.168.33.174
//!   Port=22
//!   Protocol=SSH
//!
//!   [CONNECTION:AUTHENTICATION]
//!   UserName=root
//!   Method=0                   ; 落盘是数字：0=密码 1=公钥（旧版可能是字符串）
//!   Password=<密文>            ; 加密且绑定原机器，跨机器不可迁移
//!   UserKey=                   ; 私钥路径 —— 真实键名是 UserKey，不是 IdentityFile
//!   ```
//! - 会话名：Xshell 8 不写 `[SESSION]` 节，官方行为是"会话名即文件名"，故一律用文件名。
//! - 兼容旧资料中的 `[CONNECTION:AUTHENTICATION:PUBLICKEY] IdentityFile`：作为回退一并识别。

use std::fs;
use std::path::Path;

use crate::store::NewSession;

/// 解析后的会话（尚未入库）。
#[derive(Debug)]
struct XshSession {
    name: String,
    host: String,
    port: u16,
    username: String,
    auth_type: String, // password | key | agent
    key_path: Option<String>,
}

// ——— 编码 ———

/// 读文件并按 BOM 解码：UTF-16LE / UTF-16BE / UTF-8（含 lossy 兜底）。
fn read_text(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| format!("读取失败：{}", e))?;
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        Ok(String::from_utf16_lossy(&units))
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        Ok(String::from_utf16_lossy(&units))
    } else {
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// UTF-16LE + BOM 编码，与 Xshell 原生格式一致。
fn to_utf16le_bom(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 2 + 2);
    out.extend_from_slice(&0xFEFF_u16.to_le_bytes());
    for unit in s.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

// ——— INI 解析 ———

/// 极简 INI 解析：`[Section]` + `Key=Value`，容忍 `;` `#` 行注释与行尾注释。
/// xsh 的 section 是大小写敏感的层级名（如 `CONNECTION:AUTHENTICATION`），
/// key 不区分大小写（读出时统一转大写比较）。
fn parse_ini(text: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let mut section = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_string();
            continue;
        }
        if let Some(eq) = line.find('=') {
            let key = line[..eq].trim().to_uppercase();
            let mut value = line[eq + 1..].trim().to_string();
            // 行尾注释：仅 ` ;` 起始才剥离，避免误伤值本身（如路径含分号的情况罕见）
            if let Some(pos) = value.find(" ;") {
                value.truncate(pos);
                value = value.trim_end().to_string();
            }
            out.push((section.clone(), key, value));
        }
    }
    out
}

fn lookup<'a>(ini: &'a [(String, String, String)], section: &str, key: &str) -> Option<&'a str> {
    ini.iter()
        .find(|(s, k, _)| s == section && *k == key.to_uppercase())
        .map(|(_, _, v)| v.as_str())
}

// ——— 解析单个 .xsh ———

fn parse_xsh(text: &str, fallback_name: &str) -> Result<XshSession, String> {
    let ini = parse_ini(text);

    let host = lookup(&ini, "CONNECTION", "Host")
        .filter(|v| !v.is_empty())
        .ok_or("缺少 [CONNECTION] Host")?
        .to_string();

    let port = lookup(&ini, "CONNECTION", "Port")
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(22);

    // 只支持 SSH 协议；TELNET/RLOGIN/SERIAL 明确告知跳过
    let protocol = lookup(&ini, "CONNECTION", "Protocol")
        .unwrap_or("SSH")
        .to_uppercase();
    if protocol != "SSH" {
        return Err(format!("不支持的协议 {}（仅支持 SSH）", protocol));
    }

    let username = lookup(&ini, "CONNECTION:AUTHENTICATION", "UserName")
        .unwrap_or("root")
        .to_string();

    // Xshell 8 落盘为数字（0=密码、1=公钥）；旧版/资料里也可能见到字符串枚举。
    // 再以 UserKey 是否有值兜底 —— 真实样本里 Method 有缺失的情况。
    let method = lookup(&ini, "CONNECTION:AUTHENTICATION", "Method")
        .unwrap_or("")
        .trim()
        .to_uppercase();
    let user_key = lookup(&ini, "CONNECTION:AUTHENTICATION", "UserKey")
        .filter(|v| !v.trim().is_empty())
        .map(str::to_string);

    let by_method_key = matches!(method.as_str(), "1" | "PUBLICKEY" | "PASSWORD_AND_PUBLICKEY");
    let auth_type = if by_method_key || user_key.is_some() {
        "key"
    } else {
        "password"
    };

    let key_path = if auth_type == "key" {
        user_key.or_else(|| {
            lookup(&ini, "CONNECTION:AUTHENTICATION:PUBLICKEY", "IdentityFile")
                .filter(|v| !v.trim().is_empty())
                .map(str::to_string)
        })
    } else {
        None
    };

    let name = lookup(&ini, "SESSION", "SessionName")
        .filter(|v| !v.is_empty())
        .unwrap_or(fallback_name)
        .to_string();

    Ok(XshSession {
        name,
        host,
        port,
        username,
        auth_type: auth_type.to_string(),
        key_path,
    })
}

// ——— 导出 ———

/// 生成 xsh 文本。密码位永远留空 —— 明文密码绝不写盘。
pub fn render_xsh(
    name: &str,
    host: &str,
    port: u16,
    username: &str,
    auth_type: &str,
    key_path: Option<&str>,
) -> String {
    // Xshell 8 落盘用数字：0=密码 1=公钥
    let method = if auth_type == "key" { "1" } else { "0" };
    let user_key = key_path.unwrap_or("");
    format!(
        "[CONNECTION]\r\n\
         Protocol=SSH\r\n\
         Host={host}\r\n\
         Port={port}\r\n\
         \r\n\
         [CONNECTION:AUTHENTICATION]\r\n\
         UserName={user}\r\n\
         Method={method}\r\n\
         Password=\r\n\
         UserKey={user_key}\r\n\
         \r\n\
         [CONNECTION:AUTHENTICATION:PUBLICKEY]\r\n\
         IdentityFile={user_key}\r\n\
         \r\n\
         [SESSION]\r\n\
         SessionName={name}\r\n",
        host = host,
        port = port,
        user = username,
        method = method,
        user_key = user_key,
        name = name,
    )
}

// ——— 面向命令的入口 ———

pub struct ParsedFile {
    pub label: String,
    pub result: Result<NewSession, String>,
}

/// 批量解析：每个路径 → 会话记录（密码一律不导入），入库由 cmd 层做。
pub fn parse_files(paths: &[String]) -> Vec<ParsedFile> {
    paths
        .iter()
        .map(|p| {
            let path = Path::new(p);
            let label = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.clone());
            let result = (|| -> Result<NewSession, String> {
                let text = read_text(path)?;
                let fallback = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "imported".to_string());
                let s = parse_xsh(&text, &fallback)?;
                Ok(NewSession {
                    name: s.name,
                    folder: Some("/导入".to_string()),
                    host: s.host,
                    port: Some(s.port),
                    username: s.username,
                    auth_type: s.auth_type,
                    key_path: s.key_path,
                    startup_cmd: None,
                    theme: None,
                    connect_timeout: None,
                })
            })();
            ParsedFile {
                label,
                result,
            }
        })
        .collect()
}

/// 单个会话 → xsh 字节（UTF-16LE+BOM），供 cmd 层写盘。
pub fn encode_xsh(
    name: &str,
    host: &str,
    port: u16,
    username: &str,
    auth_type: &str,
    key_path: Option<&str>,
) -> Vec<u8> {
    to_utf16le_bom(&render_xsh(name, host, port, username, auth_type, key_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "[CONNECTION]\r\nHost=10.0.0.5\r\nPort=2222\r\nProtocol=SSH\r\n\
        [CONNECTION:AUTHENTICATION]\r\nUserName=admin\r\nMethod=PUBLICKEY\r\n\
        [CONNECTION:AUTHENTICATION:PUBLICKEY]\r\nIdentityFile=C:\\keys\\id_rsa\r\n\
        [SESSION]\r\nSessionName=prod-server\r\n";

    #[test]
    fn parses_sample() {
        let s = parse_xsh(SAMPLE, "fallback").unwrap();
        assert_eq!(s.name, "prod-server");
        assert_eq!(s.host, "10.0.0.5");
        assert_eq!(s.port, 2222);
        assert_eq!(s.username, "admin");
        assert_eq!(s.auth_type, "key");
        assert_eq!(s.key_path.as_deref(), Some("C:\\keys\\id_rsa"));
    }

    #[test]
    fn utf16le_bom_roundtrip() {
        let bytes = to_utf16le_bom(SAMPLE);
        assert_eq!(&bytes[0..2], &[0xFF, 0xFE]);
        let decoded = read_text_from_bytes(&bytes);
        assert!(decoded.contains("SessionName=prod-server"));
    }

    /// 真实样本节选（Xshell 8.1 产出，UTF-16LE+CRLF，密码已脱敏为占位）
    const REAL: &str = "[CONNECTION]\r\nPort=22\r\nHost=192.168.33.174\r\nProtocol=SSH\r\n\
         [CONNECTION:AUTHENTICATION]\r\nPassword=ENCRYPTED-BLOB\r\nMethod=0\r\n\
         UserKey=\r\nUserName=root\r\n";

    #[test]
    fn parses_real_xshell8_file() {
        let s = parse_xsh(REAL, "192.168.33.174_jenkins").unwrap();
        assert_eq!(s.host, "192.168.33.174");
        assert_eq!(s.port, 22);
        assert_eq!(s.username, "root");
        // Method=0 且 UserKey 为空 → 密码认证
        assert_eq!(s.auth_type, "password");
        assert_eq!(s.key_path, None);
        // 无 [SESSION] 节 → 回退文件名
        assert_eq!(s.name, "192.168.33.174_jenkins");
    }

    #[test]
    fn user_key_overrides_method() {
        // 真实键名是 UserKey；即便 Method 缺失也应识别为公钥认证
        let s = REAL.replace("UserKey=\r", "UserKey=C:\\keys\\id_rsa\r").replace("Method=0\r\n", "");
        let parsed = parse_xsh(&s, "f").unwrap();
        assert_eq!(parsed.auth_type, "key");
        assert_eq!(parsed.key_path.as_deref(), Some("C:\\keys\\id_rsa"));
    }

    #[test]
    fn method_one_means_publickey() {
        let s = REAL.replace("Method=0", "Method=1");
        assert_eq!(parse_xsh(&s, "f").unwrap().auth_type, "key");
    }

    #[test]
    fn rejects_non_ssh() {
        let s = SAMPLE.replace("Protocol=SSH", "Protocol=TELNET");
        assert!(parse_xsh(&s, "x").unwrap_err().contains("不支持的协议"));
    }

    fn read_text_from_bytes(bytes: &[u8]) -> String {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    }
}
