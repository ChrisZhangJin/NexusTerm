//! Parse user-selected migration files as data. Never invoke ssh, Match exec, or read private keys.
use crate::store::NewSession;
use glob::Pattern;
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

const MAX_BYTES: u64 = 4 * 1024 * 1024;
const MAX_FILES: usize = 64;
const MAX_HOSTS: usize = 1000;

#[derive(Debug, Default, Serialize)]
pub struct Preview {
    sessions: Vec<NewSession>,
    warnings: Vec<String>,
}

fn read(path: &Path, remaining: &mut u64) -> Result<String, String> {
    if !path.is_file() {
        return Err("请选择普通文件".into());
    }
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|f| f.take(*remaining + 1).read_to_end(&mut bytes))
        .map_err(|_| "无法读取连接配置文件".to_string())?;
    *remaining = remaining
        .checked_sub(bytes.len() as u64)
        .ok_or("连接配置超过 4 MB 上限")?;
    let text = String::from_utf8(bytes).map_err(|_| "请选择 UTF-8 编码的连接配置")?;
    Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_string())
}

fn valid(s: &str) -> bool {
    !s.is_empty() && s.len() <= 1024 && !s.chars().any(char::is_control)
}

fn session(
    name: &str,
    host: &str,
    user: &str,
    port: &str,
    key: Option<String>,
    folder: Option<String>,
    auth: &str,
) -> Option<NewSession> {
    if !valid(name)
        || !valid(host)
        || host.chars().any(char::is_whitespace)
        || !valid(user)
        || user.chars().any(char::is_whitespace)
    {
        return None;
    }
    let port = if port.is_empty() {
        22
    } else {
        port.parse::<u16>().ok().filter(|p| *p > 0)?
    };
    Some(NewSession {
        name: name.into(),
        host: host.into(),
        port: Some(port),
        username: user.into(),
        auth_type: if key.is_some() { "key" } else { auth }.into(),
        key_path: key,
        folder,
        startup_cmd: None,
        theme: None,
        note: None,
        connect_timeout: None,
    })
}

fn csv_preview(text: &str) -> Result<Preview, String> {
    let mut reader = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .from_reader(text.as_bytes());
    let headers: Vec<String> = reader
        .headers()
        .map_err(|_| "无法读取 CSV 表头")?
        .iter()
        .map(|h| h.to_ascii_lowercase().replace([' ', '_', '-'], ""))
        .collect();
    let col = |aliases: &[&str]| headers.iter().position(|h| aliases.contains(&h.as_str()));
    let host = col(&["hostname", "host", "address", "ipaddress", "ip"])
        .ok_or("CSV 缺少 Hostname/Address 列")?;
    let user =
        col(&["username", "user", "sshusername", "sshuser"]).ok_or("CSV 缺少 Username 列")?;
    let name = col(&["label", "name", "alias"]);
    let port = col(&["port", "sshport"]);
    let group = col(&["group", "folder", "grouppath"]);
    let proxy = col(&[
        "proxyjump",
        "proxycommand",
        "jumphost",
        "hostchain",
        "proxy",
    ]);
    let protocol = col(&["protocol"]);
    let mut out = Preview::default();
    for (i, row) in reader.records().enumerate() {
        if i >= MAX_HOSTS {
            return Err("每次最多读取 1000 个连接".into());
        }
        let Ok(row) = row else {
            out.warnings.push(format!("第 {} 条：CSV 格式错误", i + 1));
            continue;
        };
        let get = |c: Option<usize>| c.and_then(|n| row.get(n)).unwrap_or("");
        if !matches!(get(proxy).to_ascii_lowercase().as_str(), "" | "none")
            || !matches!(
                get(protocol).to_ascii_lowercase().as_str(),
                "" | "ssh" | "sftp"
            )
        {
            out.warnings.push(format!(
                "第 {} 条：不支持代理/跳板或连接协议，已跳过",
                i + 1
            ));
            continue;
        }
        let address = get(Some(host));
        let label = get(name);
        let folder = get(group);
        match session(
            if label.is_empty() { address } else { label },
            address,
            get(Some(user)),
            get(port),
            None,
            valid(folder).then(|| {
                format!(
                    "/{}",
                    folder
                        .split('/')
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join("/")
                )
            }),
            "password",
        ) {
            Some(s) => out.sessions.push(s),
            None => out
                .warnings
                .push(format!("第 {} 条：主机、用户名或端口无效", i + 1)),
        }
    }
    Ok(out)
}

type Directive = (String, Vec<String>);

fn config_lines(
    path: &Path,
    base: &Path,
    remaining: &mut u64,
    count: &mut usize,
    stack: &mut HashSet<PathBuf>,
    lines: &mut Vec<Directive>,
) -> Result<(), String> {
    *count += 1;
    if *count > MAX_FILES || stack.len() >= 16 {
        return Err("SSH Include 层级或文件数量超过上限".into());
    }
    let path = path.canonicalize().map_err(|_| "无法打开 SSH 配置文件")?;
    if !stack.insert(path.clone()) {
        return Err("SSH 配置中存在循环 Include".into());
    }
    for (i, line) in read(&path, remaining)?.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let end = line
            .find(|c: char| c.is_whitespace() || c == '=')
            .unwrap_or(line.len());
        let key = line[..end].to_ascii_lowercase();
        let rest = line[end..]
            .trim_start()
            .trim_start_matches('=')
            .trim_start();
        // Parse only directives that affect migration. Preserve literal Windows path separators.
        if !matches!(
            key.as_str(),
            "host"
                | "hostname"
                | "user"
                | "port"
                | "identityfile"
                | "include"
                | "match"
                | "proxyjump"
                | "proxycommand"
                | "remotecommand"
                | "canonicalizehostname"
        ) {
            continue;
        }
        let escaped;
        let rest = if cfg!(windows) && matches!(key.as_str(), "identityfile" | "include") {
            escaped = rest.replace('\\', "\\\\");
            escaped.as_str()
        } else {
            rest
        };
        let mut args =
            shell_words::split(rest).map_err(|_| format!("SSH 配置第 {} 行引号不完整", i + 1))?;
        if let Some(pos) = args.iter().position(|s| s.starts_with('#')) {
            args.truncate(pos);
        }
        if args.is_empty() {
            return Err(format!("SSH 配置第 {} 行缺少参数", i + 1));
        }
        if key == "include" {
            for a in args {
                let p = base.join(crate::keys::expand_tilde(&a));
                let matches = glob::glob(p.to_str().ok_or("Include 路径编码无效")?)
                    .map_err(|_| "Include 模式无效")?;
                for m in matches {
                    config_lines(
                        &m.map_err(|_| "无法读取 Include 文件")?,
                        base,
                        remaining,
                        count,
                        stack,
                        lines,
                    )?;
                }
            }
        } else if key == "match" {
            return Err(
                "此配置包含 Match。请提供只含要迁移的 Host 条目的配置副本；Nexus 不执行 Match 条件"
                    .into(),
            );
        } else {
            lines.push((key, args));
        }
    }
    stack.remove(&path);
    Ok(())
}

fn applies(patterns: &[String], alias: &str) -> bool {
    let mut found = false;
    for p in patterns {
        let (neg, p) = p
            .strip_prefix('!')
            .map_or((false, p.as_str()), |p| (true, p));
        if Pattern::new(&p.to_ascii_lowercase())
            .is_ok_and(|p| p.matches(&alias.to_ascii_lowercase()))
        {
            if neg {
                return false;
            }
            found = true;
        }
    }
    found
}

fn ssh_preview(lines: &[Directive], fallback_user: &str) -> Result<Preview, String> {
    let mut seen = HashSet::new();
    let mut out = Preview::default();
    for alias in lines
        .iter()
        .filter(|(k, _)| k == "host")
        .flat_map(|(_, p)| p)
    {
        if alias.starts_with('!')
            || alias.contains(['*', '?', '['])
            || !seen.insert(alias.to_ascii_lowercase())
        {
            continue;
        }
        if seen.len() > MAX_HOSTS {
            return Err("每次最多读取 1000 个连接".into());
        }
        let mut active = true;
        let mut opts = HashMap::new();
        for (key, values) in lines {
            if key == "host" {
                active = applies(values, alias);
            } else if active {
                opts.entry(key.as_str()).or_insert(values[0].as_str());
            }
        }
        let get = |key| opts.get(key).copied().unwrap_or("");
        if [
            "proxyjump",
            "proxycommand",
            "remotecommand",
            "canonicalizehostname",
        ]
        .iter()
        .any(|&k| !matches!(get(k).to_ascii_lowercase().as_str(), "" | "none" | "no"))
        {
            out.warnings.push(format!(
                "{alias}：含跳板、代理、远端启动命令或主机名重写，已跳过"
            ));
            continue;
        }
        let host = opts
            .get("hostname")
            .copied()
            .unwrap_or(alias)
            .replace("%h", alias);
        if host.contains(['%', '$']) {
            out.warnings
                .push(format!("{alias}：含不支持的 HostName 变量，已跳过"));
            continue;
        }
        let key = get("identityfile");
        let key = (!matches!(key, "" | "none")).then(|| crate::keys::expand_tilde(key));
        let key = key.and_then(|k| {
            if Path::new(&k).is_absolute() && valid(&k) && !k.contains(['%', '$']) {
                Some(k)
            } else {
                out.warnings
                    .push(format!("{alias}：IdentityFile 需要手动设置"));
                None
            }
        });
        match session(
            alias,
            &host,
            opts.get("user").copied().unwrap_or(fallback_user),
            get("port"),
            key,
            None,
            "agent",
        ) {
            Some(s) => out.sessions.push(s),
            None => out
                .warnings
                .push(format!("{alias}：主机、用户名或端口无效")),
        }
    }
    Ok(out)
}

pub fn preview(path: &Path, format: &str) -> Result<Preview, String> {
    let mut remaining = MAX_BYTES;
    match format {
        "csv" => csv_preview(&read(path, &mut remaining)?),
        "ssh" => {
            let mut lines = Vec::new();
            let base = path.parent().unwrap_or(Path::new("."));
            config_lines(
                path,
                base,
                &mut remaining,
                &mut 0,
                &mut HashSet::new(),
                &mut lines,
            )?;
            let user = std::env::var("USER")
                .or_else(|_| std::env::var("USERNAME"))
                .unwrap_or_default();
            ssh_preview(&lines, &user)
        }
        _ => Err("不支持的连接配置格式".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn csv_maps_groups_without_importing_passwords() {
        let out = csv_preview("Label,Address,Username,SSH Port,Group,Password\r\n\"Web, prod\",web.example,ops,2222,Prod,do-not-import\r\nbad,bad.example,root,65536,,\r\n").unwrap();
        assert_eq!(out.sessions.len(), 1);
        assert_eq!(out.sessions[0].name, "Web, prod");
        assert_eq!(out.sessions[0].port, Some(2222));
        assert_eq!(out.sessions[0].folder.as_deref(), Some("/Prod"));
        assert!(!serde_json::to_string(&out)
            .unwrap()
            .contains("do-not-import"));
        assert_eq!(out.warnings.len(), 1);
    }
    #[test]
    fn ssh_applies_ordered_defaults_and_skips_jump_host() {
        let lines = [
            ("host", "web web2"),
            ("hostname", "%h.example"),
            ("port", "2222"),
            ("host", "jumped"),
            ("proxyjump", "jump"),
            ("host", "*"),
            ("user", "ops"),
            ("port", "22"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.split_whitespace().map(str::to_string).collect()))
        .collect::<Vec<_>>();
        let out = ssh_preview(&lines, "local").unwrap();
        assert_eq!(out.sessions.len(), 2);
        assert_eq!(out.sessions[1].host, "web2.example");
        assert_eq!(out.sessions[0].port, Some(2222));
        assert_eq!(out.sessions[0].username, "ops");
        assert_eq!(out.warnings.len(), 1);
    }

    #[test]
    fn ssh_config_follows_include_and_rejects_match() {
        let dir = std::env::temp_dir().join(format!("nx-import-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("conf.d")).unwrap();
        std::fs::write(dir.join("config"), "Include conf.d/*.conf\nHost *\n  User ops\n").unwrap();
        std::fs::write(dir.join("conf.d/web.conf"), "Host web\n  HostName 10.0.0.5\n  Port 2200\n").unwrap();
        let out = preview(&dir.join("config"), "ssh").unwrap();
        assert_eq!(out.sessions.len(), 1);
        assert_eq!(out.sessions[0].host, "10.0.0.5");
        assert_eq!(out.sessions[0].port, Some(2200));
        assert_eq!(out.sessions[0].username, "ops");
        std::fs::write(dir.join("config"), "Match host *\n  User root\n").unwrap();
        assert!(preview(&dir.join("config"), "ssh").unwrap_err().contains("Match"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
