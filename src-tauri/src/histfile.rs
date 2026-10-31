//! 读 shell 自己的历史文件，给终端灰字补全当候选来源。
//!
//! 只读、只放内存：内容绝不落盘、不进日志 —— 历史里常有手敲进命令行的密码或
//! token（`mysql -pXXX`），这也是 `command_history` 表被删掉的原因之一。
//!
//! 三种格式在这里自动识别，逐行判断，不需要调用方先说是哪种 shell：
//! - bash：一行一条；开了 `HISTTIMEFORMAT` 会夹着 `#1700000000` 的时间戳行
//! - zsh：普通格式同 bash，`EXTENDED_HISTORY` 下是 `: 1700000000:0;cmd`；
//!   本地文件里非 ASCII 字节被 metafy 过（见 [`unmetafy`]）
//! - fish：`- cmd: xxx` 后面跟缩进的 `when:` / `paths:` 行
//!
//! 多行命令一律丢弃：灰字只能补单行，补进去一段带换行的东西等于替用户按了回车。

use std::collections::HashSet;

/// 最多交出多少条（最近的）。前端每次按键线性扫一遍，千条量级无感。
pub const MAX_ITEMS: usize = 1000;
/// 单条命令的长度上限，与前端 `recordCommand` 的 4096 一致。
pub const MAX_CMD_LEN: usize = 4096;
/// 本地文件只读尾部这么多字节：历史文件可能有几十 MB，只要最近的。
const LOCAL_TAIL_BYTES: u64 = 1024 * 1024;

/// zsh 的 metafy 反操作：`0x83` 后面跟的那个字节要异或 `0x20` 还原。
///
/// zsh 把 `0x00` 和 `0x83..=0x9d` 这一段字节当作内部标记，写文件时用 `0x83`
/// 转义。UTF-8 的续字节正好落在这个区间，所以含中文的命令不还原就是乱码。
pub fn unmetafy(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut it = bytes.iter().copied();
    while let Some(b) = it.next() {
        if b == 0x83 {
            if let Some(n) = it.next() {
                out.push(n ^ 0x20);
                continue;
            }
        }
        out.push(b);
    }
    out
}

/// fish 历史里 cmd 的转义：`\\` → `\`，`\n` → 换行。返回 `None` 表示含换行，调用方丢弃。
fn fish_unescape(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('n') => return None,
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    Some(out)
}

/// `: 1700000000:0;cmd` → `cmd`
fn zsh_extended(line: &str) -> Option<&str> {
    let rest = line.strip_prefix(": ")?;
    let (head, cmd) = rest.split_once(';')?;
    let (ts, dur) = head.split_once(':')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    (digits(ts) && digits(dur)).then_some(cmd)
}

/// bash 的 `#1700000000` 时间戳行
fn is_bash_timestamp(line: &str) -> bool {
    line.strip_prefix('#')
        .is_some_and(|d| d.len() >= 9 && d.bytes().all(|b| b.is_ascii_digit()))
}

fn acceptable(cmd: &str) -> bool {
    cmd.chars().count() >= 2
        && cmd.len() <= MAX_CMD_LEN
        // 控制字符：补进终端会变成按键（Tab = 触发 shell 补全）。
        // U+FFFD：远端那条路里 zsh 的 metafy 字节被有损解码过，原文已无从还原。
        && !cmd.chars().any(|c| c.is_control() || c == '\u{fffd}')
        // 以 # 开头的是被注释掉的命令，不是用户想再来一遍的东西
        && !cmd.starts_with('#')
}

/// 解析历史文件文本。返回**从旧到新**、去重（同一条只留最近一次）、最多 [`MAX_ITEMS`] 条。
pub fn parse(text: &str) -> Vec<String> {
    let mut all: Vec<String> = Vec::new();
    // 上一行以 `\` 结尾 → 这是多行命令，一直丢到不以 `\` 结尾的那一行为止
    let mut in_continuation = false;

    for raw in text.lines() {
        let line = raw.trim_end_matches('\r');
        let ends_backslash = line.ends_with('\\');
        if in_continuation {
            in_continuation = ends_backslash;
            continue;
        }

        let cmd: String = if let Some(c) = zsh_extended(line) {
            c.to_string()
        } else if is_bash_timestamp(line) {
            continue;
        } else if let Some(c) = line.strip_prefix("- cmd: ") {
            // fish 的转义自成一套，`\` 结尾不代表续行
            match fish_unescape(c) {
                Some(s) => {
                    if acceptable(s.trim()) {
                        all.push(s.trim().to_string());
                    }
                    continue;
                }
                None => continue,
            }
        } else if line.starts_with("  ") || line.starts_with("- ") {
            // fish 的 when: / paths: 等字段行
            continue;
        } else {
            line.to_string()
        };

        if ends_backslash {
            in_continuation = true;
            continue;
        }
        let cmd = cmd.trim();
        if acceptable(cmd) {
            all.push(cmd.to_string());
        }
    }

    // 去重留最近：从后往前扫，第一次见到的就是最近的那条
    let mut seen: HashSet<&str> = HashSet::new();
    let mut keep: Vec<&str> = Vec::new();
    for c in all.iter().rev() {
        if keep.len() >= MAX_ITEMS {
            break;
        }
        if seen.insert(c.as_str()) {
            keep.push(c.as_str());
        }
    }
    keep.into_iter().rev().map(str::to_string).collect()
}

/// 在远端（`sh -s` 的 stdin）跑的脚本：按登录 shell 选历史文件，输出最后 2000 行。
///
/// - 只读**一个**文件：zsh 用户的 `~/.bash_history` 是更早以前的残留，混进来只会
///   给出过时的建议。
/// - `cut -c1-4097`：病态的超长行（粘贴进去的整段 base64）不能原样搬过网络；
///   截成 4097 字符后 [`acceptable`] 会因为超长把它整条丢掉。
/// - 找不到文件不算错，输出为空即可。
pub const REMOTE_SCRIPT: &str = r#"
case "${SHELL##*/}" in
  zsh)  f="${HISTFILE:-$HOME/.zsh_history}" ;;
  fish) f="${XDG_DATA_HOME:-$HOME/.local/share}/fish/fish_history" ;;
  *)    f="${HISTFILE:-$HOME/.bash_history}" ;;
esac
[ -r "$f" ] && tail -n 2000 "$f" | cut -c1-4097
exit 0
"#;

/// 本机 shell 的历史文件路径。Windows 默认的 `cmd.exe` 没有持久历史，返回 `None`。
#[cfg(unix)]
fn local_path() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let home = env("HOME").map(PathBuf::from)?;
    let shell = env("SHELL").unwrap_or_default();
    let name = shell.rsplit('/').next().unwrap_or("");
    Some(match name {
        "zsh" => env("HISTFILE").map(PathBuf::from).unwrap_or_else(|| {
            env("ZDOTDIR").map(PathBuf::from).unwrap_or_else(|| home.clone()).join(".zsh_history")
        }),
        "fish" => env("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"))
            .join("fish/fish_history"),
        _ => env("HISTFILE").map(PathBuf::from).unwrap_or_else(|| home.join(".bash_history")),
    })
}

#[cfg(not(unix))]
fn local_path() -> Option<std::path::PathBuf> {
    None
}

/// 读本机历史。文件不存在 / 读不了都当作「没有历史」，补全不该因此报错。
pub fn load_local() -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    let Some(path) = local_path() else {
        return Vec::new();
    };
    let Ok(mut f) = std::fs::File::open(&path) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(LOCAL_TAIL_BYTES);
    if start > 0 && f.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if f.take(LOCAL_TAIL_BYTES).read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&unmetafy(&buf)).into_owned();
    // 从中间切进来时第一行是半截，丢掉
    let text = if start > 0 {
        text.split_once('\n').map(|(_, rest)| rest).unwrap_or("")
    } else {
        &text
    };
    parse(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_plain_and_timestamps() {
        let t = "ls -la\n#1700000000\ncd /tmp\n#1700000001\nls -la\n";
        // 重复的 ls -la 只留最近一次，顺序从旧到新
        assert_eq!(parse(t), vec!["cd /tmp", "ls -la"]);
    }

    #[test]
    fn zsh_extended_format() {
        let t = ": 1700000000:0;git status\n: 1700000005:2;docker ps -a\n";
        assert_eq!(parse(t), vec!["git status", "docker ps -a"]);
    }

    #[test]
    fn zsh_multiline_command_is_dropped_entirely() {
        let t = ": 1:0;echo a \\\nb \\\nc\n: 2:0;pwd -P\n";
        assert_eq!(parse(t), vec!["pwd -P"]);
    }

    #[test]
    fn fish_format() {
        let t = "- cmd: tmux attach -t 0\n  when: 1700000000\n- cmd: echo a\\\\b\n  when: 1\n  paths:\n    - /tmp\n";
        assert_eq!(parse(t), vec!["tmux attach -t 0", "echo a\\b"]);
    }

    #[test]
    fn fish_multiline_is_dropped() {
        let t = "- cmd: echo a\\nb\n  when: 1\n- cmd: ls /tmp\n  when: 2\n";
        assert_eq!(parse(t), vec!["ls /tmp"]);
    }

    #[test]
    fn rejects_comments_controls_short_and_lossy() {
        let t = "#comment out\nx\nls\tfoo\nbad \u{fffd} bytes\nok cmd\n";
        assert_eq!(parse(t), vec!["ok cmd"]);
    }

    #[test]
    fn rejects_overlong() {
        let long = "a".repeat(MAX_CMD_LEN + 1);
        let fits = "b".repeat(MAX_CMD_LEN);
        let t = format!("{long}\n{fits}\n");
        assert_eq!(parse(&t), vec![fits]);
    }

    #[test]
    fn crlf_is_tolerated() {
        assert_eq!(parse("ls -l\r\npwd -P\r\n"), vec!["ls -l", "pwd -P"]);
    }

    #[test]
    fn caps_to_most_recent() {
        let t: String = (0..MAX_ITEMS + 50).map(|i| format!("cmd-{i}\n")).collect();
        let out = parse(&t);
        assert_eq!(out.len(), MAX_ITEMS);
        assert_eq!(out.first().unwrap(), "cmd-50");
        assert_eq!(out.last().unwrap(), &format!("cmd-{}", MAX_ITEMS + 49));
    }

    #[test]
    fn unmetafy_restores_marker_bytes() {
        // 0x83 后面跟 (b ^ 0x20)：0x83 0xA3 还原成 0xA3 ^ 0x20 = 0x83
        assert_eq!(unmetafy(&[b'a', 0x83, 0xA3, b'b']), vec![b'a', 0x83, b'b']);
        // 末尾孤立的 0x83 原样保留，不能吞掉或越界
        assert_eq!(unmetafy(&[b'a', 0x83]), vec![b'a', 0x83]);
    }

    #[test]
    fn unmetafy_then_decode() {
        // 'Ã' = C3 83；写进 zsh 历史时那个 0x83 被转义成 83 A3
        let metafied = [b'e', b'c', b'h', b'o', b' ', 0xC3, 0x83, 0xA3];
        let bytes = unmetafy(&metafied);
        assert_eq!(String::from_utf8(bytes).unwrap(), "echo Ã");
    }
}
