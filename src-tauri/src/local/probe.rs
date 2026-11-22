//! 本地终端的上下文探测：与 SSH 的 `PROBE_SCRIPT` 输出同一组 KEY=VALUE，前端的 `parseProbe` 直接复用。
//!
//! - `SHELL_CWD`：shell 进程的工作目录；
//! - `FG_COMM` / `FG_ARGS`：PTY 前台进程组组长的命令名与参数（参数之间用 `\x1f` 分隔，与 SSH 侧一致）；
//! - `TTY` / `TMUX_SESSION` / `TMUX_PATH`：按 tty 匹配 tmux 客户端。
//!
//! 只读，不往终端里写任何东西。Linux 读 /proc；macOS 用 `ps` / `lsof`（参数只能拿到空格拼起来的一整串，
//! 按空白切开是近似）；Windows 没有对应机制，返回空。

use std::collections::HashMap;

/// 参数分隔符：与 SSH 探测脚本把 argv 里的 `\0` 换成 `\x1f` 一致
const US: char = '\x1f';

/// 探测一个本地 PTY。`shell_pid` = 启动的 shell；`fg_pgid` = PTY 前台进程组（拿不到为 None）。
pub fn probe(shell_pid: Option<u32>, fg_pgid: Option<i32>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if !cfg!(unix) {
        return out;
    }
    let tty = shell_pid.and_then(proc_tty);
    if let Some(t) = &tty {
        out.insert("TTY".into(), t.clone());
    }
    if let Some(cwd) = shell_pid.and_then(proc_cwd) {
        out.insert("SHELL_CWD".into(), cwd);
    }
    // 前台是 shell 自己时不报（和 SSH 侧一样：只有真的有前台程序才有 FG_*）
    if let Some(pg) = fg_pgid.filter(|p| *p > 0 && Some(*p as u32) != shell_pid) {
        let pid = pg as u32;
        if let Some(args) = proc_args(pid) {
            let comm = args.first().map(|a| a.rsplit('/').next().unwrap_or(a).to_string()).unwrap_or_default();
            out.insert("FG_COMM".into(), comm);
            out.insert("FG_ARGS".into(), args.join(&US.to_string()));
        }
        if let Some(cwd) = proc_cwd(pid) {
            out.insert("FG_CWD".into(), cwd);
        }
    }
    if let Some(t) = &tty {
        if let Some((session, path)) = tmux_client(t) {
            out.insert("TMUX_SESSION".into(), session);
            if !path.is_empty() {
                out.insert("TMUX_PATH".into(), path);
            }
        }
    }
    out
}

#[cfg(target_os = "linux")]
fn proc_cwd(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok().map(|p| p.to_string_lossy().into_owned())
}

#[cfg(target_os = "linux")]
fn proc_tty(pid: u32) -> Option<String> {
    let t = std::fs::read_link(format!("/proc/{pid}/fd/0")).ok()?.to_string_lossy().into_owned();
    t.starts_with("/dev/").then_some(t)
}

#[cfg(target_os = "linux")]
fn proc_args(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args: Vec<String> =
        raw.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect();
    (!args.is_empty()).then_some(args)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn proc_cwd(pid: u32) -> Option<String> {
    // macOS：lsof -Fn 输出 `p<pid>` / `fcwd` / `n<path>`
    let out = std::process::Command::new("lsof").args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"]).output().ok()?;
    String::from_utf8_lossy(&out.stdout).lines().find_map(|l| l.strip_prefix('n').map(str::to_string))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn proc_tty(pid: u32) -> Option<String> {
    // macOS：ps 给的是 `ttys003`，tmux 的 client_tty 是 `/dev/ttys003`
    let out = std::process::Command::new("ps").args(["-o", "tty=", "-p", &pid.to_string()]).output().ok()?;
    let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!t.is_empty() && t != "??").then(|| format!("/dev/{t}"))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn proc_args(pid: u32) -> Option<Vec<String>> {
    let out = std::process::Command::new("ps").args(["-o", "args=", "-p", &pid.to_string()]).output().ok()?;
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let args: Vec<String> = line.split_whitespace().map(str::to_string).collect();
    (!args.is_empty()).then_some(args)
}

#[cfg(not(unix))]
fn proc_cwd(_pid: u32) -> Option<String> {
    None
}

#[cfg(not(unix))]
fn proc_args(_pid: u32) -> Option<Vec<String>> {
    None
}

#[cfg(not(unix))]
fn proc_tty(_pid: u32) -> Option<String> {
    None
}

/// 这个 tty 是不是某个 tmux 客户端：返回（会话名，当前 pane 目录）。没装 tmux / 不是客户端 → None。
fn tmux_client(tty: &str) -> Option<(String, String)> {
    let out = std::process::Command::new("tmux")
        .args(["list-clients", "-F", "#{client_tty}\t#{session_name}\t#{pane_current_path}"])
        .output()
        .ok()?;
    parse_tmux_clients(&String::from_utf8_lossy(&out.stdout), tty)
}

fn parse_tmux_clients(text: &str, tty: &str) -> Option<(String, String)> {
    text.lines().find_map(|l| {
        let mut it = l.split('\t');
        let (t, s, p) = (it.next()?, it.next()?, it.next().unwrap_or(""));
        (t == tty && !s.is_empty()).then(|| (s.to_string(), p.to_string()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmux_client_matches_by_tty() {
        let text = "/dev/pts/3\twork\t/home/a\n/dev/pts/7\tops\t/srv\n";
        assert_eq!(parse_tmux_clients(text, "/dev/pts/7"), Some(("ops".into(), "/srv".into())));
        assert_eq!(parse_tmux_clients(text, "/dev/pts/9"), None);
        assert_eq!(parse_tmux_clients("", "/dev/pts/7"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn probes_own_process_cwd_and_args() {
        let me = std::process::id();
        let m = probe(Some(me), None);
        assert_eq!(m.get("SHELL_CWD").map(String::as_str), std::env::current_dir().ok().as_deref().and_then(|p| p.to_str()));
        // 前台进程组等于 shell 自己：不报 FG_*
        let m = probe(Some(me), Some(me as i32));
        assert!(!m.contains_key("FG_COMM"));
        // 前台是另一个进程（这里拿自己当「前台」，shell 换成一个不存在的 pid）
        let m = probe(Some(u32::MAX), Some(me as i32));
        assert!(m.get("FG_ARGS").is_some_and(|a| !a.is_empty()), "{m:?}");
    }
}
