//! 导出一个会话「迄今为止的全部输出」，供 agent 工具与前端「导出」按钮共用。
//!
//! 两层：
//!
//! 1. **基础层**：`AgentManager::snapshot`，旁路环形缓冲里还留着的字节。
//! 2. **深度层**（`deep=true`）：环形缓冲装不下「开旁路之前」或「另一个 tmux pane」的内容，
//!    所以主动去要 tmux / docker 自己的 scrollback。远端走旁路 exec 通道
//!    （`SessionManager::run_sidecar`）；本地不该为导出去打扰用户正在用的那个 pty，
//!    故另起独立子进程跑探测脚本。
//!
//! 深度层是**尽力而为**：探测失败（没装 tmux/docker、超时、命令不存在）一律安静跳过，
//! 不让「导出」这个本该保底的功能因为环境缺东西而报错。

use std::time::Duration;

use crate::cmd::AppState;

/// 远端走旁路通道时，单次探测的超时（秒）。
const SIDECAR_TIMEOUT_SECS: u64 = 4;
/// 本地走子进程时，单次探测的超时。
const LOCAL_TIMEOUT: Duration = Duration::from_secs(4);
/// 深度层单项内容的字符上限：tmux/docker 历史可以很长，这里只取尾部最近的部分。
const DEEP_SECTION_MAX_CHARS: usize = 8 * 1024;

/// 定位「用户此刻在看哪个 tmux pane」，抓最近 200 行。
/// 与 `act::capture_fullscreen` 用的是同一条探测脚本。
const TMUX_PROBE: &str = concat!(
    "p=$(tmux display-message -p '#{session_name}:#{window_index}.#{pane_index}' 2>/dev/null); ",
    "[ -n \"$p\" ] || p=$(tmux list-panes -a -F ",
    "'#{session_name}:#{window_index}.#{pane_index} #{?window_active,1,0} #{?pane_active,1,0}' ",
    "2>/dev/null | awk '$2==1 && $3==1 {print $1; exit}'); ",
    "[ -n \"$p\" ] && tmux capture-pane -p -t \"$p\" -S -200"
);

/// 列出正在跑的容器，取每个容器最近 200 行日志。
const DOCKER_PROBE: &str = concat!(
    "docker ps --format '{{.Names}}' 2>/dev/null | while read -r c; do ",
    "echo \"== $c ==\"; docker logs --tail 200 \"$c\" 2>&1; done"
);

/// 导出一个会话迄今为止的全部输出。
///
/// 返回 `None` 表示两层都没拿到任何东西（会话不存在 / 从未旁路过 / 探测全部落空）。
pub async fn export_session(state: &AppState, session_id: &str, deep: bool) -> Option<String> {
    let base = state.agent.snapshot(session_id);

    let extra = if deep {
        deep_scrollback(state, session_id).await
    } else {
        None
    };

    match (base, extra) {
        (Some(b), Some(e)) if !b.is_empty() || !e.is_empty() => {
            let mut out = String::new();
            if !b.is_empty() {
                out.push_str(&b);
            }
            if !e.is_empty() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str("# 补充抓取（tmux / docker）\n");
                out.push_str(&e);
            }
            Some(out)
        }
        (Some(b), None) => Some(b),
        (None, Some(e)) if !e.is_empty() => Some(format!("# 补充抓取（tmux / docker）\n{e}")),
        _ => None,
    }
}

/// 深度层：按会话类型分流到 SSH 旁路通道或本地子进程。
async fn deep_scrollback(state: &AppState, session_id: &str) -> Option<String> {
    if state.sessions.contains(session_id) {
        deep_scrollback_ssh(state, session_id).await
    } else if state.local.contains(session_id) {
        deep_scrollback_local().await
    } else {
        None
    }
}

async fn deep_scrollback_ssh(state: &AppState, session_id: &str) -> Option<String> {
    let mut sections = Vec::new();

    if let Ok(raw) = state
        .sessions
        .run_sidecar(session_id, TMUX_PROBE, None, SIDECAR_TIMEOUT_SECS, "抓取 tmux 内容超时")
        .await
    {
        push_section(&mut sections, "tmux", &raw);
    }
    if let Ok(raw) = state
        .sessions
        .run_sidecar(session_id, DOCKER_PROBE, None, SIDECAR_TIMEOUT_SECS, "抓取 docker 日志超时")
        .await
    {
        push_section(&mut sections, "docker", &raw);
    }

    join_sections(sections)
}

/// 本地会话：不碰用户正在用的 pty，另起一个独立子进程去跑探测脚本。
async fn deep_scrollback_local() -> Option<String> {
    let mut sections = Vec::new();

    if let Some(raw) = spawn_probe(TMUX_PROBE).await {
        push_section(&mut sections, "tmux", &raw);
    }
    if let Some(raw) = spawn_probe(DOCKER_PROBE).await {
        push_section(&mut sections, "docker", &raw);
    }

    join_sections(sections)
}

/// 起一个 `sh -c <script>` 子进程，拿标准输出。失败 / 超时一律 `None`。
///
/// 掐断 stdin（避免卡死等输入），超时后把孩子一并带走（`kill_on_drop`），
/// 不让探测脚本挂死整个导出。
async fn spawn_probe(script: &str) -> Option<String> {
    use std::process::Stdio;
    use tokio::process::Command;

    let child = async {
        Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await
    };
    let out = tokio::time::timeout(LOCAL_TIMEOUT, child).await.ok()?.ok()?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    let text = text.trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn push_section(sections: &mut Vec<(&'static str, String)>, label: &'static str, raw: &str) {
    let text = raw.trim().to_string();
    if !text.is_empty() {
        sections.push((label, tail_chars(&text, DEEP_SECTION_MAX_CHARS)));
    }
}

fn join_sections(sections: Vec<(&'static str, String)>) -> Option<String> {
    if sections.is_empty() {
        return None;
    }
    let mut out = String::new();
    for (label, text) in sections {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("## {label}\n{text}\n"));
    }
    Some(out)
}

/// 取字符串末尾最多 `max` 个字符（按字符而非字节切，避免切断多字节字符）。
fn tail_chars(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        chars[chars.len() - max..].iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawn_probe_collects_stdout() {
        let out = spawn_probe("echo hello-from-probe").await;
        assert_eq!(out.as_deref(), Some("hello-from-probe"));
    }

    #[tokio::test]
    async fn spawn_probe_returns_none_on_empty_output() {
        let out = spawn_probe("true").await;
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn spawn_probe_collects_stderr_too() {
        let out = spawn_probe("echo err-text >&2").await;
        assert_eq!(out.as_deref(), Some("err-text"));
    }

    #[test]
    fn join_sections_formats_each_with_heading() {
        let sections = vec![("tmux", "line1".to_string()), ("docker", "line2".to_string())];
        let joined = join_sections(sections).unwrap();
        assert!(joined.contains("## tmux\nline1"));
        assert!(joined.contains("## docker\nline2"));
    }

    #[test]
    fn join_sections_empty_is_none() {
        assert!(join_sections(Vec::new()).is_none());
    }

    #[test]
    fn tail_chars_keeps_only_the_end() {
        let long: String = "x".repeat(10).chars().chain("END".chars()).collect();
        assert_eq!(tail_chars(&long, 3), "END");
        assert_eq!(tail_chars("short", 100), "short");
    }
}
