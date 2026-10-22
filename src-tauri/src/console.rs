//! Console 自动补全的后端：问出一个可执行命令支持哪些参数。
//!
//! ## 为什么要后端来做
//!
//! `<binary> --help` 必须在这台机器（或那个 SSH 会话所在的远端）上跑才准 ——
//! 前端并不知道当前标签连的是本地 shell 还是某台远端主机，也不知道它能不能开旁路通道。
//!
//! ## 为什么不在用户那个终端里跑
//!
//! 往正在用的 pty 里塞一串 `--help` 会把屏幕弄脏、把 shell 历史也要污染掉，
//! 而且一个交互式程序（`top` / `man`）会直接把终端抢走。所以一律走：
//! 本地 → 子进程；远端 → 该连接的旁路 exec 通道（`SessionManager::run_sidecar`）。
//!
//! ## 安全边界
//!
//! 这条命令由一个模型预测以外的东西触发 —— 是**人敲的命令名**，
//! 但仍然当作不可信输入处理：必须是单个 token、字符集收得过审，
//! 否则一旦混进 `;` / `$()` / 反引号，这里就成了一个不用登录的远程命令执行入口。

use std::time::Duration;

use serde::Serialize;

use crate::agent::buffer::strip_ansi;

/// 一条 --help 里解析出来的选项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CmdOption {
    /// 所有写法：`["-a", "--all"]`。第一个是最短的、最常敲的那个。
    pub flags: Vec<String>,
    /// 一行说明
    pub summary: String,
    /// 该选项是否还能接一个取值（`--color[=WHEN]` / `--lines NUM` 这种）
    pub takes_value: bool,
}

/// 一个会话能记住的选项条目上限。
///
/// `ls --help` 三十来条，但 `git --help` 和 `ffmpeg -h` 能上千；
/// 超过这个数的话，多出来的既显示不下也对补全没帮助。
const MAX_OPTIONS: usize = 200;

/// 本地会话：直接起子进程问帮助，不走 shell、不经过用户的终端。
///
/// `--help` 试不到东西就退到 `-h` —— BSD / Go / 各类自研工具往往只认后者，
/// 只认一种的话会丢掉一大半可用提示。
pub async fn local_options(program: &str) -> Vec<CmdOption> {
    for flag in LOCAL_HELP_FLAGS {
        let Some(text) = spawn_help(program, flag).await else {
            continue;
        };
        let opts = parse_help(&text);
        if !opts.is_empty() {
            return opts;
        }
    }
    Vec::new()
}

/// 本地尝试的 help 开关顺序。
const LOCAL_HELP_FLAGS: [&str; 4] = ["--help", "-h", "-help", "--usage"];

/// 起一次子进程拿帮助正文。失败（程序不存在 / 非可执行 / 超时）返回 None。
async fn spawn_help(program: &str, flag: &str) -> Option<String> {
    use std::process::Stdio;
    use tokio::process::Command;

    let child = async {
        Command::new(program)
            .arg(flag)
            // stdin 必须掐断：不少程序在没有 stdin 关闭时会安静地等着人输入，
            // 于是这一次「问帮助」会一直挂住，把补全整个拖死。
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // 同上：超时后必须把这个孩子带走，不能留成孤儿进程
            .kill_on_drop(true)
            .output()
            .await
    };
    // 4 秒足够任何一个 `--help` 打印完；等更久只会让用户觉得补全失灵了
    let out = tokio::time::timeout(Duration::from_secs(4), child)
        .await
        .ok()?
        .ok()?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    // 帮助常常写在 stderr 里（尤其是那些自己手写 help 的工具）
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Some(text)
}

/// 允许出现在「可执行文件名」里的字符。
///
/// 刻意不含空格、引号、`$` / `` ` ``、`;` / `|` / `&`、括号、通配符 ——
/// 少支持一个奇怪的二进制名，好过给一条命令注入开个口子。
fn valid_program(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/'))
}

/// 取出要查询的程序名。
///
/// 只取 `command` 的第一个 token 并校验字符集；校验不过返回 `None`。
///
/// **第一个 token 之后跟着什么我们不管** —— 重要的是最终喂给操作系统的
/// 只有这一串审过字符集的干净名字，绝无可能被 shell 解释出别的意图。
pub fn program_of(command: &str) -> Option<&str> {
    let first = command.split_whitespace().next()?;
    if !valid_program(first) {
        return None;
    }
    Some(first)
}

/// 解析 `--help` 的正文，抽出选项列表。
///
/// 现实里的 help 排版千奇百怪，这里只认最常见、也最无害的一种：
/// 一行里第一个以 `-` 开头的 token 是选项，跟着的若干个同类 token 是它的别名，
/// 剩下的行内文本是说明。其余形状（Usage、Examples、版权声明）全部忽略 ——
/// 宁可少给几条，也不要把一段英文正文当成一堆选项糊到屏幕上。
pub fn parse_help(text: &str) -> Vec<CmdOption> {
    let clean = strip_ansi(text);
    let mut out: Vec<CmdOption> = Vec::new();

    for raw in clean.lines() {
        if out.len() >= MAX_OPTIONS {
            break;
        }
        let trimmed_start = raw.trim_start();
        // 顶格写 `-x` 的通常是 Usage / Examples / 版权声明，不当选项
        let indent = raw.len() - trimmed_start.len();
        if indent == 0 || trimmed_start.is_empty() {
            continue;
        }

        // 按空白切词，同时记住每个 token 在原行里的起点，以便原样取回说明部分
        let mut toks: Vec<(usize, &str)> = Vec::new();
        let mut i = 0usize;
        for piece in trimmed_start.split_whitespace() {
            if let Some(at) = trimmed_start[i..].find(piece) {
                toks.push((i + at, piece));
                i += at + piece.len();
            }
        }
        let head = toks.first().map(|(_, s)| *s).unwrap_or("");

        if !is_flag(head) {
            // 缩进更深且不是选项 → 上一条说明的续行
            if indent >= 4 && !trimmed_start.starts_with('-') {
                if let Some(last) = out.last_mut() {
                    push_summary(&mut last.summary, trimmed_start);
                }
            }
            continue;
        }

        // 连续的 flag token 都是同一个选项的别名：`-a, --all`
        let mut flags: Vec<String> = Vec::new();
        let mut takes_value = false;
        let mut consumed = 0usize;
        for (pos, tok) in &toks {
            if !is_flag(tok) {
                break;
            }
            consumed = *pos + tok.len();
            let word = tok.trim_end_matches(',');
            if word.contains('[') || word.contains('=') {
                takes_value = true;
            }
            let name = word
                .split(|c| c == '[' || c == '=')
                .next()
                .unwrap_or(word)
                .to_string();
            if !name.is_empty() && !flags.contains(&name) {
                flags.push(name);
            }
        }
        if flags.is_empty() {
            continue;
        }

        // flag 之后：可能是大写占位符（`--lines NUM`），然后才是说明
        let mut after = &trimmed_start[consumed.min(trimmed_start.len())..];
        let next = after.split_whitespace().next().unwrap_or("");
        if !next.is_empty() && is_placeholder(next) {
            takes_value = true;
            let skip = after.find(next).unwrap_or(0) + next.len();
            after = &after[skip..];
        }
        let summary = after.trim().to_string();

        out.push(CmdOption { flags, summary, takes_value });
    }

    out.retain(|o| !o.flags.is_empty());
    out
}

/// 一个 token 是不是选项：`-a` / `--all` / `--color[=WHEN]`。
///
/// 第 2 个字符可以是字母数字，也可以是 `-`（长选项 `--all`）；
/// 但光秃秃的一个 `--` 不算，它是 GNU 的「选项到此为止」分隔符。
fn is_flag(tok: &str) -> bool {
    let mut chars = tok.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let Some(second) = chars.next() else {
        return false;
    };
    if first != '-' {
        return false;
    }
    if second == '-' {
        tok.len() > 2
    } else {
        second.is_ascii_alphanumeric()
    }
}

/// 说明里出现的全大写占位符：`NUM` / `FILE` / `WHEN|ALWAYS`
fn is_placeholder(tok: &str) -> bool {
    tok.len() >= 2
        && tok
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == '_' || c == '|' || c.is_ascii_digit())
        && tok.chars().any(|c| c.is_ascii_uppercase())
}

/// 把续行的文字接到上一条说明后面。中间用一个空格，不做任何 trim ——
/// help 里的换行有时是有意的（列清单），保留换行比糊成一坨更好读；
/// 但 UI 上一行放得下才有意义，所以这里折中成空格。
fn push_summary(summary: &mut String, more: &str) {
    if summary.is_empty() {
        summary.push_str(more.trim());
    } else {
        summary.push(' ');
        summary.push_str(more.trim());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LS_HELP: &str = "\
Usage: ls [OPTION]... [FILE]...
List information about the FILEs.

Mandatory arguments to long options are mandatory for short options too.
  -a, --all                  do not ignore entries starting with .
  -A, --almost-all           do not list implied . and ..
      --color[=WHEN]         colorize the output; WHEN can be 'always',
                             'auto', or 'never'
  -h, --human-readable       with -l and -s, print sizes like 1K 234M
  -l                         use a long listing format
      --version              output version information and exit

Examples:
  ls -la
";

    fn flags_of<'a>(opts: &'a [CmdOption], name: &str) -> Option<&'a CmdOption> {
        opts.iter().find(|o| o.flags.iter().any(|f| f == name))
    }

    #[test]
    fn short_and_long_aliases_land_in_one_option() {
        let opts = parse_help(LS_HELP);
        let a = flags_of(&opts, "-a").expect("应解析出 -a");
        assert_eq!(a.flags, vec!["-a", "--all"]);
        assert_eq!(a.summary, "do not ignore entries starting with .");
        assert!(!a.takes_value);
    }

    #[test]
    fn long_only_option_is_parsed() {
        let opts = parse_help(LS_HELP);
        let c = flags_of(&opts, "--color").expect("应解析出 --color");
        assert_eq!(c.flags, vec!["--color"]);
        // `[=WHEN]` 表示可带值
        assert!(c.takes_value, "带 [] 的选项应标记为可接值");
    }

    #[test]
    fn option_without_long_form_is_kept() {
        let opts = parse_help(LS_HELP);
        let l = flags_of(&opts, "-l").expect("应解析出 -l");
        assert_eq!(l.flags, vec!["-l"]);
        assert!(l.summary.starts_with("use a long listing format"));
    }

    /// Usage / Examples 顶格写的 `-la` 不该被当成选项
    #[test]
    fn usage_and_examples_are_not_options() {
        let opts = parse_help(LS_HELP);
        assert!(flags_of(&opts, "-la").is_none());
        assert!(flags_of(&opts, "[OPTION]").is_none());
    }

    #[test]
    fn wrapped_description_is_joined_to_the_previous_option() {
        let opts = parse_help(LS_HELP);
        let c = flags_of(&opts, "--color").expect("应解析出 --color");
        assert!(
            c.summary.contains("'auto'") && c.summary.contains("'never'"),
            "续行应被接上：{}",
            c.summary
        );
    }

    #[test]
    fn ansi_escapes_are_stripped_before_parsing() {
        let opts = parse_help("  \u{1b}[1m-v\u{1b}[0m  verbose output\n");
        let v = flags_of(&opts, "-v").expect("ANSI 不该挡住解析");
        assert_eq!(v.summary, "verbose output");
    }

    #[test]
    fn garbage_and_empty_input_yield_nothing() {
        assert!(parse_help("").is_empty());
        assert!(parse_help("not a help page at all\njust prose\n").is_empty());
    }

    #[test]
    fn huge_pages_are_capped() {
        let flood: String = (0..MAX_OPTIONS + 500)
            .map(|i| format!("  -x{i}, --xx{i}   desc\n"))
            .collect();
        assert_eq!(parse_help(&flood).len(), MAX_OPTIONS);
    }

    // —————————————— 程序名校验 ——————————————

    #[test]
    fn ordinary_programs_pass() {
        for p in ["ls", "git", "journalctl", "foo_bar", "node-v20", "/usr/bin/tail", "docker-compose"] {
            assert_eq!(program_of(p), Some(p), "{p} 应合法");
        }
        // 带参数时只取第一个 token
        assert_eq!(program_of("systemctl status nginx"), Some("systemctl"));
    }

    /// 真实防线是「只取第一个 token 且字符集审过」：危险后缀会在席前就被丢掉，
    /// 喂给 OS 的永远只是 `Some("ls")` 这种干净名字。这两种情形合起来才算安全。
    #[test]
    fn metacharacters_never_reach_the_os() {
        // ① 元字符只要出现在第一个 token 里（哪怕没有空格），整个名字作废
        for evil in [
            "$(whoami)", "`id`", "a$b", "test'x", "foo(bar)", "a\\b",
            "ls;rm", "ls&&id", "ls|cat",
        ] {
            assert_eq!(program_of(evil), None, "{evil} 不该被接受");
        }
        // ② 元字符在第一个 token 之后 → 只留下头一个干净的 token，
        //    危险后缀根本没进到 OS 那一层（它也不会被拼回命令行里）
        for (input, kept) in [
            ("ls | cat", Some("ls")),
            ("systemctl status nginx", Some("systemctl")),
            ("ls && whoami", Some("ls")),
            ("ls; rm -rf /", None), // 这个 `;` 是紧贴第一个 token 的，故整串作废
        ] {
            assert_eq!(program_of(input), kept, "{input} 的取词结果不对");
        }
    }

    #[test]
    fn empty_or_overlong_names_are_rejected() {
        assert_eq!(program_of(""), None);
        assert_eq!(program_of("   "), None);
        assert_eq!(program_of(&"x".repeat(200)), None);
    }

    // —————————————— 真实 help 排版 ——————————————

    /// Go / Rust 那类自己手写 help 的工具：用制表符缩进，且只有 `-h`。
    const GO_STYLE: &str = "\
Usage:\n  tool [flags]\n\nFlags:\n\t-v, --verbose\tverbose output\n\t-n, --number int\t how many\n";

    #[test]
    fn tab_indented_help_parses() {
        let opts = parse_help(GO_STYLE);
        let v = flags_of(&opts, "--verbose").expect("制表符缩进也该解析出来");
        assert_eq!(v.flags, vec!["-v", "--verbose"]);
        assert_eq!(v.summary, "verbose output");
    }

    #[test]
    fn takes_value_is_detected_from_placeholder() {
        let opts = parse_help(GO_STYLE);
        let n = flags_of(&opts, "--number").expect("应解析出 --number");
        // `--number int` 里的 `int` 不是大写，所以靠 `[]`/`=` 之外的规则认不出来；
        // 但 GNU 风格 `--lines=NUM` 必须认得出来
        assert_eq!(n.flags, vec!["-n", "--number"]);

        let gnu = parse_help("  -n, --lines=NUM   output the last NUM lines\n");
        let g = flags_of(&gnu, "--lines").expect("应解析出 --lines");
        assert!(g.takes_value, "`=NUM` 必须标记为可接值");
        assert_eq!(g.summary, "output the last NUM lines");
    }

    #[test]
    fn uppercase_placeholder_marks_takes_value() {
        let opts = parse_help("  -c, --count NUM   how many times\n");
        let c = flags_of(&opts, "--count").expect("应解析出 --count");
        assert!(c.takes_value, "大写占位符 NUM 应让它标记为可接值");
        assert_eq!(c.summary, "how many times", "占位符不该混进说明里");
    }

    #[test]
    fn bare_double_dash_is_not_an_option() {
        // GNU 的「选项到此为止」分隔符
        assert!(!is_flag("--"));
        assert!(!is_flag("-"));
        assert!(is_flag("-a"));
        assert!(is_flag("--all"));
    }

    #[test]
    fn repeated_flag_names_are_deduplicated() {
        let opts = parse_help("  -a, -a, --all, --all   show everything\n");
        let a = flags_of(&opts, "--all").expect("应解析出 --all");
        assert_eq!(a.flags, vec!["-a", "--all"], "重复写法只保留一次");
    }

    // —————————————— 真的起子进程 ——————————————
    //
    // `parse_help` 那几个用例再全，也验不到「OS 那一层」：程序存不存在、
    // 会不会因为没关 stdin 而挂住、输出在 stdout 还是 stderr。
    // 这里用一个临时脚本当替身，把整条 spawn 路径真跑一遍。

    /// 造一个会打印指定帮助正文的可执行脚本，返回它的路径
    ///
    /// 只在 unix 上可用：这里造的替身是 `#!/bin/sh` 脚本，Windows 直接 spawn
    /// 一个 `.sh` 会失败 —— 那不是被测逻辑的问题，是平台没有这个执行入口。
    #[cfg(unix)]
    fn write_fake_program(name: &str, body: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("nx_console_probe");
        std::fs::create_dir_all(&dir).expect("临时目录应可创建");
        let path = dir.join(name);
        std::fs::write(&path, body).expect("脚本应可写入");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("脚本应可执行");
        }
        path
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn options_come_from_a_real_child_process() {
        let path = write_fake_program(
            "fake_with_help.sh",
            "#!/bin/sh\ncat <<'EOF'\n  -f, --follow   track the tail\n  -n, --lines N  how many\nEOF\n",
        );
        let opts = local_options(&path.to_string_lossy()).await;
        let f = flags_of(&opts, "--follow").expect("真跑子进程应拿到 --follow");
        assert_eq!(f.flags, vec!["-f", "--follow"]);
        assert_eq!(f.summary, "track the tail");
    }

    /// 帮助写在 stderr 里（很多自研工具就这么干）也要收得到
    #[cfg(unix)]
    #[tokio::test]
    async fn help_written_to_stderr_is_collected() {
        let path = write_fake_program(
            "fake_stderr.sh",
            "#!/bin/sh\necho '  -q, --quiet   be quiet' >&2\n",
        );
        let opts = local_options(&path.to_string_lossy()).await;
        assert!(
            flags_of(&opts, "--quiet").is_some(),
            "stderr 里的帮助也该被收到"
        );
    }

    /// `--help` 没东西时退到 `-h`
    #[cfg(unix)]
    #[tokio::test]
    async fn falls_back_to_short_h() {
        let path = write_fake_program(
            "fake_only_h.sh",
            "#!/bin/sh\nif [ \"$1\" = \"-h\" ]; then echo '  -v   verbose'; fi\n",
        );
        let opts = local_options(&path.to_string_lossy()).await;
        assert!(flags_of(&opts, "-v").is_some(), "`--help` 空时应退回 `-h`");
    }

    /// 程序不存在 / 不可执行：退化成空数组，不能让补全炸掉
    #[tokio::test]
    async fn missing_program_yields_nothing() {
        let opts = local_options("nx_definitely_not_a_real_binary_9f3a").await;
        assert!(opts.is_empty());
    }

    /// 中文说明也不能被截断 —— 说明部分按原始字符取，不能按字节切
    #[test]
    fn non_ascii_summary_survives() {
        let opts = parse_help("  -q, --quiet   静默模式，不输出任何东西\n");
        let q = flags_of(&opts, "--quiet").expect("应解析出 --quiet");
        assert_eq!(q.summary, "静默模式，不输出任何东西");
    }
}
