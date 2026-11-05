//! 旧宏 → skill 迁移（Phase 3.5）。
//!
//! 旧宏（`macros` 表，步骤 `send | wait | sleep`，见 `store::MacroStep`）一键转成 skill 草稿。
//! 本模块的**转换器是纯函数**（[`convert_macro`]：不碰数据库、不碰终端），
//! 编排（预览 / 落库 / 幂等）在下半部分，Tauri 命令在最底下。
//!
//! # 映射规则
//!
//! - **打字片段合并成行**：旧录制器把「一批连续可打印字符」与「每个控制键」各录成一步，
//!   手写宏里一行也可能拆成几步。转换时先还原转义（`\r \e \x03` …），再把片段拼成行：
//!   以回车结束的完整命令行 → `exec`；其余 → `keys`。
//! - **回车拆分**：一步文本里夹着几个回车（`a\rb\r`）就是几行；空行回车 → `keys <Enter>`。
//! - **控制键 → `keys`**：Ctrl-C / Ctrl-D / Ctrl-Z 单独成一个 `keys` 步骤（它们会打断前面的半行）；
//!   Tab / 方向键 / Home / End / Del / F1-F12 / 其它 Ctrl 组合与行内容混在一起时，
//!   **整行**改记成一个 `keys`（`ls<Tab>/etc<Enter>`），因为 exec 的语义是「敲一整行命令」。
//!   无法映射的转义序列丢弃并在报告里提醒。
//! - **交互式会话里的行一律 `keys`**：`exec` 的语义是「敲命令并等它结束」，
//!   但 `ssh` / `su` / `mysql` / `vim` / `tail -f` 之类不会结束，跳板机菜单里敲的 `6` 也不是 shell 命令。
//!   所以需要口令或会开启交互会话的命令行本身记成 `keys`，其后（直到 `exit` / `logout` / `quit` / Ctrl-D）
//!   的行也记成 `keys`。
//! - **延时基本丢弃**：新回放引擎按屏幕内容判断而不是定时。仅保留两种「有意义」的提示，
//!   变成**上一步**的 `timeoutMs`：显式的 `sleep ≥ 2s`，以及显式设置了 `timeoutMs ≥ 2s` 的 `wait`
//!   （都表示「上一步要很久」）。有 pattern 的 `wait` 变成上一步的 `expectAfter`
//!   （没有上一步时变成 `screen_check`），其 `timeoutMs` 作为同一步的超时提示；
//!   pattern 不合法（旧引擎对坏正则会退化成子串）则丢弃并提醒。
//!
//! # 口令（本模块的存在理由之一）
//!
//! 旧录制器录下了**一切**，包括输入的密码，且明文存在 `macros.steps` 里。
//! 转换出的 skill 里**绝不能**有这些文本。宏格式本身不记录「有没有回显」，
//! 所以只能靠启发式（宁可误报，误报的代价是用户在草稿里重新加一步）：
//!
//! 1. 紧跟在「像口令提示」的 `wait`（`Password:` / 口令 / 密码 / passphrase / OTP / 验证码…）之后的那一行
//!    → 转成 `expect_respond { secret: true }`（保留提示 pattern，应答留空，运行时现问）。
//! 2. 命令行本身含疑似写死的口令（复用 `skill::literal_secret_hint`：`password=…`、`sshpass -p`、
//!    `mysql -pXXX`、`sk-…`、Bearer …，外加 `FOO_PASS=…` / `echo … | sudo -S` / URL 内嵌凭据 / `curl -u a:b` /
//!    `redis-cli -a …` / `chpasswd`）→ 整行丢弃，换成 `secret` 占位。
//! 3. 紧跟在 `ssh` / `su` / `sudo` / `passwd` / `scp` / `sftp` / `mysql -p` / `docker login` 等
//!    「会要口令」的命令之后的第一行非空内容 → 视为口令，**除非**它明显是命令（已知命令名 + 参数 /
//!    `ls`、`pwd` 这类单词命令）、菜单序号（≤3 位纯数字）或 `yes/no`。
//! 4. 紧跟在口令之后的 4–8 位纯数字 → 视为一次性验证码（OTP）。
//! 5. 形似口令的单个词（8–64 字符、无空白与 `/`、含大写 / 小写 / 数字 / 特殊符号中至少三类）。
//!
//! 命中即**不复制文本**：换成 `secret` 步骤（只有通用理由，没有 value 字段），
//! 并在报告里加一条警告（只写步骤序号与命中类别，**从不引用原文**）。
//! 兜底：转换完还要过 `skill::validate`，若因凭据类问题失败，对应步骤同样被换成占位并报告，
//! 绝不静默保留。原宏**不会被修改或删除**，里面的明文口令仍在 —— 报告里会提醒用户处理。
//!
//! # 幂等与不动原宏
//!
//! `store::macro_migration` 的 `macro_migrations` 表记录「宏 → skill」。已迁移且技能还在 → 跳过；
//! 已迁移但技能被删了：「全部迁移」跳过（别把用户删掉的东西复活），对某条宏明确点「转为技能」则重新迁移。

use std::collections::{BTreeSet, HashSet};
use std::sync::OnceLock;

use regex::Regex;
use serde::Serialize;
use tauri::State;

use super::skill::{
    self, assess_risk, HostScope, OnUnexpected, SkillInput, SkillSource, SkillStep, MAX_DESC_CHARS,
    MAX_NAME_CHARS, MAX_PATTERN_CHARS, MAX_REASON_CHARS, MAX_STEPS, MAX_TIMEOUT_MS, MIN_TIMEOUT_MS,
};
use crate::cmd::AppState;
use crate::store::{MacroRecord, MacroStep, Store};

/// 低于这个值的 sleep / wait 超时不当作「上一步很慢」的提示。
const MIN_HINT_MS: u64 = 2000;
/// 口令占位的理由（给用户看的、运行时的输入提示）。刻意通用：不含任何原文。
const SECRET_REASON: &str = "需要输入密码（旧宏里录下的口令已丢弃，运行时由你现场输入）";
const SECRET_REASON_CMD: &str = "原命令行含疑似口令，已丢弃；请改用密码通道重写这一步";

// ——————————————————————————————— 转义与按键词法 ———————————————————————————————

/// `src/lib/macro.ts::unescapeText` 的镜像：还原编辑器 / 录制器的转义写法。
/// 支持 `\r \n \t \e \0 \\ \xNN`；其它反斜杠组合原样保留。
pub fn unescape_text(s: &str) -> String {
    let cs: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < cs.len() {
        let ch = cs[i];
        if ch != '\\' || i == cs.len() - 1 {
            out.push(ch);
            i += 1;
            continue;
        }
        match cs[i + 1] {
            '\\' => out.push('\\'),
            'r' => out.push('\r'),
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'e' => out.push('\x1b'),
            '0' => out.push('\0'),
            'x' => {
                let hex: String = cs.iter().skip(i + 2).take(2).collect();
                if hex.len() == 2 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
                    if let Some(c) = u8::from_str_radix(&hex, 16).ok().map(char::from) {
                        out.push(c);
                        i += 4;
                        continue;
                    }
                }
                out.push('\\');
                i += 1;
                continue;
            }
            _ => {
                out.push('\\');
                i += 1;
                continue;
            }
        }
        i += 2;
    }
    out
}

/// 词法单元。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Char(char),
    Enter,
    Key { name: String, kind: KeyKind },
    /// 无法映射的控制序列（丢弃，报告里提醒）
    Unmapped,
    /// 已知但无意义的序列（括号粘贴标记等），静默丢弃
    Ignored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyKind {
    /// Ctrl-C / D / Z / \：打断当前行，自成一步
    Interrupt,
    /// 退格
    Backspace,
    /// Ctrl-U 清行
    ClearLine,
    /// 其它：混进行里，整行改记成 keys
    Other,
}

fn key(name: &str, kind: KeyKind) -> Tok {
    Tok::Key { name: name.to_string(), kind }
}

/// 把还原后的文本切成词法单元。
fn lex(s: &str) -> Vec<Tok> {
    let cs: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        match c {
            '\r' => {
                out.push(Tok::Enter);
                if cs.get(i + 1) == Some(&'\n') {
                    i += 1;
                }
            }
            '\n' => out.push(Tok::Enter),
            '\t' => out.push(key("Tab", KeyKind::Other)),
            '\x08' | '\x7f' => out.push(key("BS", KeyKind::Backspace)),
            '\x15' => out.push(key("C-u", KeyKind::ClearLine)),
            '\x03' => out.push(key("C-c", KeyKind::Interrupt)),
            '\x04' => out.push(key("C-d", KeyKind::Interrupt)),
            '\x1a' => out.push(key("C-z", KeyKind::Interrupt)),
            '\x1c' => out.push(key("C-\\", KeyKind::Interrupt)),
            '\x1d' => out.push(key("C-]", KeyKind::Other)),
            '\x00' => out.push(key("C-@", KeyKind::Other)),
            '\x1b' => {
                let (tok, used) = lex_escape(&cs[i..]);
                out.push(tok);
                i += used - 1;
            }
            c if (1..=26).contains(&(c as u32)) => {
                let letter = char::from_u32(c as u32 + 96).unwrap_or('?');
                out.push(Tok::Key { name: format!("C-{letter}"), kind: KeyKind::Other });
            }
            c if c.is_control() => out.push(Tok::Unmapped),
            c => out.push(Tok::Char(c)),
        }
        i += 1;
    }
    out
}

/// 解析以 ESC 开头的序列，返回 (词法单元, 消耗的字符数，含 ESC)。
fn lex_escape(cs: &[char]) -> (Tok, usize) {
    match cs.get(1) {
        Some('[') => {
            // CSI：参数字节 0x30-0x3f，中间字节 0x20-0x2f，终止字节 0x40-0x7e
            let mut j = 2;
            while j < cs.len() && matches!(cs[j] as u32, 0x20..=0x3f) {
                j += 1;
            }
            let Some(&fin) = cs.get(j) else {
                return (key("Esc", KeyKind::Other), 1);
            };
            if !matches!(fin as u32, 0x40..=0x7e) {
                return (key("Esc", KeyKind::Other), 1);
            }
            let params: String = cs[2..j].iter().collect();
            (csi_key(&params, fin), j + 1)
        }
        Some('O') => match cs.get(2) {
            Some(&f) => (ss3_key(f), 3),
            None => (key("M-O", KeyKind::Other), 2),
        },
        Some(&c) if c.is_ascii_alphanumeric() => (Tok::Key { name: format!("M-{c}"), kind: KeyKind::Other }, 2),
        _ => (key("Esc", KeyKind::Other), 1),
    }
}

fn csi_key(params: &str, fin: char) -> Tok {
    let named = |n: &str| key(n, KeyKind::Other);
    match (params, fin) {
        ("", 'A') | ("1", 'A') => named("Up"),
        ("", 'B') | ("1", 'B') => named("Down"),
        ("", 'C') | ("1", 'C') => named("Right"),
        ("", 'D') | ("1", 'D') => named("Left"),
        ("", 'H') | ("1", 'H') => named("Home"),
        ("", 'F') | ("1", 'F') => named("End"),
        ("", 'Z') => named("S-Tab"),
        ("1" | "7", '~') => named("Home"),
        ("4" | "8", '~') => named("End"),
        ("3", '~') => named("Del"),
        ("5", '~') => named("PgUp"),
        ("6", '~') => named("PgDn"),
        ("11", '~') => named("F1"),
        ("12", '~') => named("F2"),
        ("13", '~') => named("F3"),
        ("14", '~') => named("F4"),
        ("15", '~') => named("F5"),
        ("17", '~') => named("F6"),
        ("18", '~') => named("F7"),
        ("19", '~') => named("F8"),
        ("20", '~') => named("F9"),
        ("21", '~') => named("F10"),
        ("23", '~') => named("F11"),
        ("24", '~') => named("F12"),
        // 括号粘贴的起止标记：没有对应按键，也不是丢信息
        ("200" | "201", '~') => Tok::Ignored,
        _ => Tok::Unmapped,
    }
}

fn ss3_key(f: char) -> Tok {
    let named = |n: &str| key(n, KeyKind::Other);
    match f {
        'A' => named("Up"),
        'B' => named("Down"),
        'C' => named("Right"),
        'D' => named("Left"),
        'H' => named("Home"),
        'F' => named("End"),
        'P' => named("F1"),
        'Q' => named("F2"),
        'R' => named("F3"),
        'S' => named("F4"),
        _ => Tok::Unmapped,
    }
}

/// 把文本写进按键串：`\` 与 `<` 要转义（`parse_keys` 的语法）。
fn esc_keys(text: &str) -> String {
    text.replace('\\', "\\\\").replace('<', "<lt>")
}

/// exec 命令里的 `{{` 会被当成参数占位符；迁移的宏没有参数，一律按字面保留。
fn esc_braces(cmd: &str) -> String {
    cmd.replace("{{", "\\{{")
}

// ——————————————————————————————— 口令启发式 ———————————————————————————————

/// 额外的「写死凭据」形状（`skill::literal_secret_hint` 之外的补充）。返回命中类别。
fn extra_literal_secret(t: &str) -> Option<&'static str> {
    static RES: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    let res = RES.get_or_init(|| {
        [
            (r#"(?i)\becho\b[^|\n]*\|\s*(?:sudo\s+-S|su\b|ssh\b|kinit\b|passwd\b)"#, "把口令管道给 sudo / su"),
            (r#"[A-Za-z][A-Za-z0-9+.\-]*://[^/\s:@]+:[^/\s@]+@"#, "URL 里内嵌了账号口令"),
            (r#"(?i)\bcurl\b[^\n]*\s(?:-u|--user)[=\s]+[^\s:]+:\S+"#, "curl -u 账号:口令"),
            (r#"(?i)\bredis-cli\b[^\n]*\s-a\s+\S+"#, "redis-cli -a 口令"),
            (r#"(?i)\bchpasswd\b"#, "chpasswd"),
        ]
        .iter()
        .map(|(p, why)| (Regex::new(p).unwrap_or_else(|e| panic!("宏迁移口令正则编译失败 {p}：{e}")), *why))
        .collect()
    });
    for (re, why) in res {
        if re.is_match(t) {
            return Some(why);
        }
    }
    // FOO_PASS=xxx / API_TOKEN=xxx：值是 `$VAR` / `{{…}}` 引用则放过
    static ENV: OnceLock<Regex> = OnceLock::new();
    let env = ENV.get_or_init(|| {
        Regex::new(r#"(?i)\b[A-Za-z0-9_]*(?:pass(?:word|wd|phrase)?|pwd|secret|token|api_?key|access_?key)[A-Za-z0-9_]*=(\S)"#)
            .unwrap_or_else(|e| panic!("宏迁移口令 env 正则编译失败：{e}"))
    });
    for cap in env.captures_iter(t) {
        if !matches!(&cap[1], "$" | "{") {
            return Some("口令 / 令牌类变量赋了字面值");
        }
    }
    None
}

/// 命令行是否含疑似写死的凭据（skill 校验用的那套 + 补充）。
fn literal_cred(t: &str) -> Option<&'static str> {
    skill::literal_secret_hint(t).or_else(|| extra_literal_secret(t))
}

/// 等待的 pattern 是否像在等一个口令 / 验证码输入。
fn looks_like_secret_prompt(pattern: &str) -> bool {
    static CLASS: OnceLock<Regex> = OnceLock::new();
    static WORDS: OnceLock<Regex> = OnceLock::new();
    let class = CLASS.get_or_init(|| Regex::new(r"\[(\w)\w\]").unwrap_or_else(|e| panic!("正则编译失败：{e}")));
    let words = WORDS.get_or_init(|| {
        Regex::new(r"(?i)pass(?:word|wd|phrase|code)|\bpin\b|\botp\b|\b2fa\b|\bmfa\b|verification code|verify code|token|口令|密码|验证码|动态码|密钥短语")
            .unwrap_or_else(|e| panic!("正则编译失败：{e}"))
    });
    if skill::looks_like_password_prompt(pattern) {
        return true;
    }
    // `[Pp]assword` 这类字符类写法：把 `[Pp]` 折成 `P` 再比
    let folded = class.replace_all(pattern, "$1");
    words.is_match(pattern) || words.is_match(&folded)
}

/// 去掉前导的 `VAR=val` 赋值与 `sudo` 及其选项，得到真正要运行的词序列。
/// 同时返回原始首词（用来判断是不是 sudo 本身）。
fn command_words(line: &str) -> (String, Vec<String>) {
    let toks: Vec<&str> = line.split_whitespace().collect();
    let mut i = 0;
    while i < toks.len() && toks[i].contains('=') && !toks[i].starts_with('-') {
        i += 1;
    }
    let first = toks.get(i).map(|s| s.rsplit('/').next().unwrap_or(s).to_string()).unwrap_or_default();
    let mut words: Vec<String> = toks[i.min(toks.len())..].iter().map(|s| s.to_string()).collect();
    if first == "sudo" {
        // 跳过 sudo 的选项（`-u user` / `-g group` 带一个参数）
        let mut j = 1;
        while j < words.len() && words[j].starts_with('-') {
            let takes_arg = matches!(words[j].as_str(), "-u" | "-g" | "-h" | "-p" | "-C" | "-r" | "-t");
            j += if takes_arg { 2 } else { 1 };
        }
        let sudo_flags: Vec<String> = words[1..j.min(words.len())].to_vec();
        let mut rest: Vec<String> = words.split_off(j.min(words.len()));
        // `sudo -i` / `sudo -s`：开一个交互 shell
        if rest.is_empty() && sudo_flags.iter().any(|f| f == "-i" || f == "-s") {
            rest.push("bash".into());
        }
        words = rest;
    }
    if let Some(w) = words.first_mut() {
        *w = w.rsplit('/').next().unwrap_or(w).to_string();
    }
    (first, words)
}

/// 这条命令会向人要口令（所以不能用 exec：它会卡在口令提示上）。
fn needs_credentials(line: &str) -> bool {
    let (first, words) = command_words(line);
    if first == "sudo" {
        return true;
    }
    let prog = words.first().map(String::as_str).unwrap_or("");
    const ALWAYS: &[&str] = &[
        "ssh", "su", "passwd", "scp", "sftp", "ftp", "telnet", "kinit", "ssh-add", "ssh-keygen", "login", "smbclient",
        "htpasswd",
    ];
    if ALWAYS.contains(&prog) {
        return true;
    }
    if prog == "docker" || prog == "podman" {
        return words.get(1).is_some_and(|w| w == "login");
    }
    const DB: &[&str] = &["mysql", "mysqldump", "mysqladmin", "mongo", "mongosh", "psql", "pg_dump", "mongodump"];
    if DB.contains(&prog) {
        return words.iter().skip(1).any(|w| w == "-p" || w == "--password" || w == "-W");
    }
    false
}

/// 这条命令会开一个不会自己结束的交互会话（其后的行是敲给它的，不是 shell 命令）。
fn launches_interactive(line: &str) -> bool {
    let (_, words) = command_words(line);
    let prog = words.first().map(String::as_str).unwrap_or("");
    let args: Vec<&str> = words.iter().skip(1).map(String::as_str).collect();
    let has_flag = |fs: &[&str]| args.iter().any(|a| fs.contains(a));
    const ALWAYS: &[&str] = &[
        "ssh", "telnet", "su", "ftp", "sftp", "screen", "tmux", "top", "htop", "vi", "vim", "nano", "less", "more",
        "man", "nc", "ncat", "watch", "redis-cli", "sqlite3",
    ];
    if ALWAYS.contains(&prog) {
        return true;
    }
    const DB: &[&str] = &["mysql", "mongo", "mongosh", "psql"];
    if DB.contains(&prog) {
        return !has_flag(&["-e", "--execute", "-c", "--command", "-f", "--file"]);
    }
    const REPL: &[&str] = &["python", "python3", "node", "irb", "ipython", "bash", "sh", "zsh", "fish", "lua"];
    if REPL.contains(&prog) {
        return args.iter().all(|a| a.starts_with('-') && *a != "-c");
    }
    if matches!(prog, "docker" | "podman" | "kubectl") {
        let runs = args.iter().any(|a| matches!(*a, "exec" | "run" | "attach"));
        let tty = args.iter().any(|a| a.starts_with('-') && !a.starts_with("--") && (a.contains('i') || a.contains('t')));
        return runs && tty;
    }
    if prog == "tail" || prog == "journalctl" {
        return has_flag(&["-f", "-F", "--follow"]);
    }
    false
}

fn is_exit_line(t: &str) -> bool {
    matches!(t.trim().to_lowercase().as_str(), "exit" | "logout" | "quit" | "\\q" | "exit;" | "quit;" | "bye")
}

/// 紧跟登录命令之后、但明显不是口令的行：已知命令 + 参数、`ls` 这类单词命令、菜单序号、`yes/no`。
fn obviously_not_password(t: &str) -> bool {
    const KNOWN: &[&str] = &[
        "ls", "ll", "la", "cd", "pwd", "cat", "tail", "head", "less", "grep", "find", "df", "du", "free", "top", "htop",
        "ps", "kill", "systemctl", "service", "journalctl", "docker", "kubectl", "git", "vi", "vim", "nano", "echo",
        "whoami", "id", "uname", "uptime", "date", "hostname", "clear", "exit", "logout", "sudo", "mkdir", "rm", "cp",
        "mv", "chmod", "chown", "tar", "curl", "wget", "ping", "ip", "ifconfig", "netstat", "ss", "make", "npm", "yarn",
        "pip", "python", "python3", "node", "bash", "sh", "w", "who", "mysql", "psql",
    ];
    const BARE_OK: &[&str] = &[
        "ls", "ll", "la", "pwd", "whoami", "id", "uptime", "date", "hostname", "clear", "exit", "logout", "top", "htop",
        "df", "free", "w", "who",
    ];
    let lower = t.to_lowercase();
    if matches!(lower.as_str(), "yes" | "no" | "y" | "n") {
        return true;
    }
    if t.len() <= 3 && t.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    let mut it = t.split_whitespace();
    let Some(first) = it.next() else { return false };
    let first = first.to_lowercase();
    if !KNOWN.contains(&first.as_str()) {
        return false;
    }
    it.next().is_some() || BARE_OK.contains(&first.as_str())
}

/// 形似口令的单个词：8–64 字符、无空白与 `/`、不以路径 / 选项 / 变量符号开头，
/// 含大写 / 小写 / 数字 / 特殊符号（不含 `.-_:~`）中至少三类。
fn password_shaped(t: &str) -> bool {
    let n = t.chars().count();
    if !(8..=64).contains(&n) || t.chars().any(|c| c.is_whitespace() || c == '/') {
        return false;
    }
    if t.starts_with(['.', '~', '-', '$']) {
        return false;
    }
    let lower = t.chars().any(|c| c.is_ascii_lowercase());
    let upper = t.chars().any(|c| c.is_ascii_uppercase());
    let digit = t.chars().any(|c| c.is_ascii_digit());
    let special = t.chars().any(|c| c.is_ascii_punctuation() && !".-_:~".contains(c));
    [lower, upper, digit, special].iter().filter(|b| **b).count() >= 3
}

fn otp_like(t: &str) -> bool {
    (4..=8).contains(&t.len()) && t.chars().all(|c| c.is_ascii_digit())
}

// ——————————————————————————————— 转换器 ———————————————————————————————

/// 刚等过的口令提示：随后输入的那一行就是口令。
struct Prompt {
    /// 提示的 pattern（已转义 / 校验）；坏 pattern 为 `None`，此时只用来标记「下一行是口令」。
    pattern: Option<String>,
    timeout: Option<u64>,
}

struct Builder {
    steps: Vec<SkillStep>,
    warnings: Vec<String>,
    /// 本行已敲的可见字符（组合模式下也一直累计，只用于启发式判断）
    text: String,
    /// 组合模式：行里混进了特殊键，整行按 keys 记；这里是已累计的按键串
    keys: Option<String>,
    prompt: Option<Prompt>,
    /// 上一条命令会要口令，下一行非空内容很可能就是口令
    login: bool,
    /// 刚吐出过口令占位，下一行若是短数字很可能是验证码
    after_secret: bool,
    /// 交互会话嵌套深度（>0 时各行一律 keys）
    depth: u32,
    secrets: u32,
    unmapped: u32,
    bad_patterns: u32,
}

fn timeout_hint(ms: u64) -> Option<u64> {
    (ms > 0).then(|| ms.clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS))
}

/// 宏里的 pattern 转成 skill 的 pattern：`{{` 按字面保留，且必须是合法 pattern。
fn sanitize_pattern(p: &str) -> Option<String> {
    let esc = esc_braces(p.trim());
    if esc.is_empty() || esc.chars().count() > MAX_PATTERN_CHARS || esc.chars().any(char::is_control) {
        return None;
    }
    skill::pattern_is_regex(&esc).ok().map(|_| esc)
}

fn step_timeout_mut(step: &mut SkillStep) -> &mut Option<u64> {
    match step {
        SkillStep::Exec { timeout_ms, .. }
        | SkillStep::Keys { timeout_ms, .. }
        | SkillStep::ExpectRespond { timeout_ms, .. }
        | SkillStep::Secret { timeout_ms, .. }
        | SkillStep::ScreenCheck { timeout_ms, .. } => timeout_ms,
    }
}

fn step_expect_after_mut(step: &mut SkillStep) -> &mut Option<String> {
    match step {
        SkillStep::Exec { expect_after, .. }
        | SkillStep::Keys { expect_after, .. }
        | SkillStep::ExpectRespond { expect_after, .. }
        | SkillStep::Secret { expect_after, .. }
        | SkillStep::ScreenCheck { expect_after, .. } => expect_after,
    }
}

fn exec_step(command: String) -> SkillStep {
    SkillStep::Exec { command, timeout_ms: None, expect_after: None, on_unexpected: OnUnexpected::Ask }
}

fn keys_step(keys: String) -> SkillStep {
    SkillStep::Keys { keys, timeout_ms: None, expect_after: None, on_unexpected: OnUnexpected::Ask }
}

fn secret_step(reason: &str) -> SkillStep {
    SkillStep::Secret {
        reason: reason.chars().take(MAX_REASON_CHARS).collect(),
        param: None,
        timeout_ms: None,
        expect_after: None,
        on_unexpected: OnUnexpected::Ask,
    }
}

impl Builder {
    fn new() -> Self {
        Builder {
            steps: Vec::new(),
            warnings: Vec::new(),
            text: String::new(),
            keys: None,
            prompt: None,
            login: false,
            after_secret: false,
            depth: 0,
            secrets: 0,
            unmapped: 0,
            bad_patterns: 0,
        }
    }

    /// 喂入一步 `send`。
    fn feed(&mut self, raw: &str, enter: bool) {
        for tok in lex(&unescape_text(raw)) {
            match tok {
                Tok::Char(c) => {
                    self.text.push(c);
                    if let Some(k) = &mut self.keys {
                        k.push_str(&esc_keys(&c.to_string()));
                    }
                }
                Tok::Enter => self.emit_line(true),
                Tok::Key { name, kind } => self.press(&name, kind),
                Tok::Unmapped => self.unmapped += 1,
                Tok::Ignored => {}
            }
        }
        if enter {
            self.emit_line(true);
        }
    }

    fn press(&mut self, name: &str, kind: KeyKind) {
        match kind {
            KeyKind::Interrupt => {
                self.emit_line(false);
                self.prompt = None;
                self.login = false;
                self.after_secret = false;
                if name == "C-d" {
                    self.depth = self.depth.saturating_sub(1);
                }
                self.steps.push(keys_step(format!("<{name}>")));
            }
            KeyKind::Backspace => {
                if self.keys.is_none() && !self.text.is_empty() {
                    self.text.pop();
                } else {
                    self.text.pop();
                    self.compose(name);
                }
            }
            KeyKind::ClearLine => {
                if self.keys.is_none() && !self.text.is_empty() {
                    self.text.clear();
                } else {
                    self.text.clear();
                    self.compose(name);
                }
            }
            KeyKind::Other => self.compose(name),
        }
    }

    /// 进入（或继续）组合模式：把已敲的文字与这个键一起记成按键串。
    fn compose(&mut self, name: &str) {
        let k = match self.keys.take() {
            Some(k) => k,
            None => esc_keys(&self.text),
        };
        self.keys = Some(format!("{k}<{name}>"));
    }

    /// 本行（或本个片段）是否像口令。命中返回类别（不含原文）。
    fn suspect(&mut self, text: &str, has_prompt: bool) -> Option<String> {
        if has_prompt {
            return Some("紧跟在口令提示之后".into());
        }
        let t = text.trim();
        if t.is_empty() {
            return None;
        }
        if let Some(why) = literal_cred(t) {
            return Some(format!("命令行含疑似写死的口令（{why}）"));
        }
        let login = std::mem::take(&mut self.login);
        let after_secret = std::mem::take(&mut self.after_secret);
        if login && !obviously_not_password(t) {
            return Some("紧跟在 ssh / sudo / su 等会要口令的命令之后".into());
        }
        if after_secret && otp_like(t) {
            return Some("紧跟在口令之后的纯数字，疑似验证码".into());
        }
        if password_shaped(t) {
            return Some("形似口令的单个词".into());
        }
        None
    }

    /// 把当前行 / 片段写成步骤。`committed` = 以回车结束。
    fn emit_line(&mut self, committed: bool) {
        let nothing = self.keys.is_none() && self.text.trim().is_empty();
        if nothing && !committed {
            self.text.clear();
            return;
        }
        let text = std::mem::take(&mut self.text);
        let keys = self.keys.take();
        let prompt = self.prompt.take();
        let n = self.steps.len() + 1;

        if let Some(why) = self.suspect(&text, prompt.is_some()) {
            self.secrets += 1;
            self.after_secret = true;
            self.login = false;
            self.warnings.push(format!(
                "技能第 {n} 步：疑似口令（{why}），内容已丢弃，改为密码占位（secret）。请核对是否确为口令"
            ));
            let step = match prompt.as_ref().and_then(|p| p.pattern.clone().map(|e| (e, p.timeout))) {
                Some((expect, timeout_ms)) => SkillStep::ExpectRespond {
                    expect,
                    respond: String::new(),
                    secret: true,
                    timeout_ms,
                    expect_after: None,
                    on_unexpected: OnUnexpected::Ask,
                },
                None if why.starts_with("命令行含") => secret_step(SECRET_REASON_CMD),
                None => secret_step(SECRET_REASON),
            };
            self.steps.push(step);
            return;
        }

        let cmd = text.trim().to_string();
        if let Some(k) = keys {
            self.steps.push(keys_step(if committed { format!("{k}<Enter>") } else { k }));
            return;
        }
        if !committed {
            self.steps.push(keys_step(esc_keys(&text)));
            return;
        }
        if cmd.is_empty() {
            self.steps.push(keys_step("<Enter>".into()));
            return;
        }
        // 完整命令行
        let cred = needs_credentials(&cmd);
        let launch = launches_interactive(&cmd);
        if self.depth > 0 || cred || launch {
            if launch && self.depth == 0 {
                self.warnings.push(format!(
                    "技能第 {n} 步起含交互式会话（ssh / 菜单 / 数据库客户端等）：这一行及其后各行（到 exit / Ctrl-D 为止）按 keys 回放，不等命令结束"
                ));
            }
            self.steps.push(keys_step(format!("{}<Enter>", esc_keys(&cmd))));
        } else {
            self.steps.push(exec_step(esc_braces(&cmd)));
        }
        if launch {
            self.depth += 1;
        }
        if is_exit_line(&cmd) {
            self.depth = self.depth.saturating_sub(1);
        }
        self.login = cred;
    }

    /// 给上一步一个「它要很久」的超时提示（只增不减）。
    fn hint_prev(&mut self, ms: u64) {
        if ms < MIN_HINT_MS {
            return;
        }
        if let Some(last) = self.steps.last_mut() {
            let t = step_timeout_mut(last);
            *t = Some(t.map_or(ms, |old| old.max(ms)).clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS));
        }
    }

    /// 把一个已校验的 pattern 等待接到上一步（`expectAfter`），没有上一步则成为 `screen_check`。
    fn apply_wait(&mut self, pattern: Option<String>, timeout: Option<u64>) {
        let Some(pat) = pattern else {
            self.bad_patterns += 1;
            return;
        };
        if let Some(last) = self.steps.last_mut() {
            let ea = step_expect_after_mut(last);
            if ea.is_none() {
                *ea = Some(pat);
                if let Some(ms) = timeout {
                    let t = step_timeout_mut(last);
                    *t = Some(t.map_or(ms, |old| old.max(ms)));
                }
                return;
            }
        }
        self.steps.push(SkillStep::ScreenCheck {
            expect: pat,
            timeout_ms: timeout,
            expect_after: None,
            on_unexpected: OnUnexpected::Ask,
        });
    }

    /// 之前挂起的口令提示没等到随后的输入：按普通等待处理。
    fn resolve_prompt(&mut self) {
        if let Some(p) = self.prompt.take() {
            if p.pattern.is_some() {
                self.apply_wait(p.pattern, p.timeout);
            }
        }
    }

    fn wait(&mut self, pattern: &str, timeout_ms: u32) {
        let timeout = timeout_hint(timeout_ms as u64);
        let p = pattern.trim();
        if p.is_empty() {
            // 「等输出静默」：丢弃；显式的大超时说明上一步很慢
            if let Some(ms) = timeout {
                self.hint_prev(ms);
            }
            return;
        }
        self.emit_line(false);
        self.resolve_prompt();
        let pat = sanitize_pattern(p);
        if looks_like_secret_prompt(p) {
            self.prompt = Some(Prompt { pattern: pat, timeout });
        } else {
            self.apply_wait(pat, timeout);
        }
    }

    fn sleep(&mut self, ms: u32) {
        self.hint_prev(ms as u64);
    }

    fn finish(&mut self) {
        self.emit_line(false);
        self.resolve_prompt();
        if self.unmapped > 0 {
            self.warnings.push(format!("有 {} 处无法映射的按键序列已丢弃", self.unmapped));
        }
        if self.bad_patterns > 0 {
            self.warnings.push(format!("有 {} 处等待的 pattern 不合法，已忽略", self.bad_patterns));
        }
        if self.secrets > 0 {
            self.warnings.push(
                "原宏里仍保存着当时录下的明文内容（迁移不会修改或删除原宏），建议确认后编辑或删除原宏".into(),
            );
        }
    }
}

/// 转换的判定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum Verdict {
    /// 得到一份已通过校验的草稿
    Draft { draft: Box<SkillInput> },
    /// 没有可迁移的内容
    Skipped { reason: String },
    /// 转换出的 skill 过不了校验（返回全部问题）
    Failed { errors: Vec<String> },
}

/// 转换结果：判定 + 警告（警告从不引用被怀疑为口令的原文）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Conversion {
    pub verdict: Verdict,
    pub warnings: Vec<String>,
}

/// 名称去重：已被占用就加后缀 ` (2)`、` (3)`…（保证总长不超过上限）。
pub fn unique_name(base: &str, taken: &HashSet<String>) -> String {
    let cleaned: String = base.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let cleaned = cleaned.trim();
    let base: String = if cleaned.is_empty() { "未命名宏".into() } else { cleaned.chars().take(MAX_NAME_CHARS).collect() };
    if !taken.contains(&base) {
        return base;
    }
    for n in 2u32.. {
        let suffix = format!(" ({n})");
        let room = MAX_NAME_CHARS - suffix.chars().count();
        let cand: String = base.chars().take(room).collect::<String>().trim_end().to_string() + &suffix;
        if !taken.contains(&cand) {
            return cand;
        }
    }
    base
}

/// `validate` 报的「第 N 步」里的 N（从 1 起）。
fn error_step_index(e: &str) -> Option<usize> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^第 (\d+) 步").unwrap_or_else(|e| panic!("正则编译失败：{e}")));
    re.captures(e).and_then(|c| c[1].parse().ok())
}

/// 校验草稿；若因凭据类问题失败，把对应步骤换成 `secret` 占位并报告，直到通过或剩下无法自动处理的错误。
/// 绝不静默保留看起来像凭据的内容。
fn finalize(mut input: SkillInput, warnings: &mut Vec<String>) -> Result<SkillInput, Vec<String>> {
    for _ in 0..=MAX_STEPS {
        input.risk = assess_risk(&input.steps).risk;
        let skill = input.clone().into_skill(String::new(), 0);
        let errs = match skill::validate(&skill) {
            Ok(()) => {
                input.risk = skill.risk;
                return Ok(input);
            }
            Err(e) => e,
        };
        let bad: BTreeSet<usize> = errs
            .iter()
            .filter(|e| e.contains("口令") || e.contains("密钥"))
            .filter_map(|e| error_step_index(e))
            .filter(|i| {
                matches!(
                    input.steps.get(i - 1),
                    Some(SkillStep::Exec { .. } | SkillStep::Keys { .. })
                        | Some(SkillStep::ExpectRespond { secret: false, .. })
                )
            })
            .collect();
        if bad.is_empty() {
            return Err(errs);
        }
        for i in bad {
            input.steps[i - 1] = secret_step(SECRET_REASON_CMD);
            warnings.push(format!(
                "技能第 {i} 步：校验判定含凭据类内容，内容已丢弃，改为密码占位（secret）。请核对"
            ));
        }
    }
    Err(vec!["无法在合理次数内清除凭据类内容".into()])
}

/// 宏 → skill 草稿。**纯函数**。`taken` 是已被占用的 skill 名称（用于去重）。
///
/// 草稿：来源 `migrated_macro`、主机范围不限、无参数 / 验证、风险经 `assess_risk`；
/// 计数由存储层铸造（永远为 0）。
pub fn convert_macro(m: &MacroRecord, taken: &HashSet<String>) -> Conversion {
    let mut b = Builder::new();
    for s in &m.steps {
        match s {
            MacroStep::Send { text, enter } => b.feed(text, *enter),
            MacroStep::Wait { pattern, timeout_ms } => b.wait(pattern, *timeout_ms),
            MacroStep::Sleep { ms } => b.sleep(*ms),
        }
    }
    b.finish();
    let mut warnings = std::mem::take(&mut b.warnings);

    if b.steps.iter().all(|s| matches!(s, SkillStep::ScreenCheck { .. })) {
        return Conversion {
            verdict: Verdict::Skipped { reason: "宏里没有可迁移的操作（没有步骤，或只有等待 / 延时）".into() },
            warnings,
        };
    }

    let name = unique_name(&m.name, taken);
    let short: String = name.chars().take(60).collect();
    let description: String =
        format!("由旧宏「{short}」迁移而来（原宏保留）。回放前请核对每一步。").chars().take(MAX_DESC_CHARS).collect();
    let input = SkillInput {
        name,
        description,
        params: Vec::new(),
        steps: b.steps,
        verify: None,
        risk: skill::Risk::Low,
        host_scope: HostScope::Any,
        source: Some(SkillSource::MigratedMacro),
    };
    match finalize(input, &mut warnings) {
        Ok(draft) => Conversion { verdict: Verdict::Draft { draft: Box::new(draft) }, warnings },
        Err(errors) => Conversion { verdict: Verdict::Failed { errors }, warnings },
    }
}

// ——————————————————————————————— 编排（预览 / 落库 / 幂等） ———————————————————————————————

/// 预览：转换后的草稿 + 警告，**不写任何东西**。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationPreview {
    pub macro_id: String,
    pub macro_name: String,
    /// 会被保存的草稿（名称已去重）。为 `None` 时看 `skip_reason` / `errors`。
    pub draft: Option<SkillInput>,
    pub skip_reason: Option<String>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum MigrationOutcome {
    Migrated { skill_id: String },
    Skipped { reason: String },
    Failed { errors: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationResult {
    pub macro_id: String,
    pub macro_name: String,
    pub outcome: MigrationOutcome,
    pub warnings: Vec<String>,
}

/// 选出要迁移的宏：`ids` 为 `None` = 全部（库里的顺序），否则按给定顺序；不存在的 id 记为 `Err(id)`。
fn select(store: &Store, ids: Option<&[String]>) -> Result<Vec<Result<MacroRecord, String>>, String> {
    let all = store.list_macros()?;
    Ok(match ids {
        None => all.into_iter().map(Ok).collect(),
        Some(ids) => ids
            .iter()
            .map(|id| all.iter().find(|m| &m.id == id).cloned().ok_or_else(|| id.clone()))
            .collect(),
    })
}

/// 单条宏的计划。
enum Plan {
    Draft(Box<SkillInput>, Vec<String>),
    Skipped(String, Vec<String>),
    Failed(Vec<String>, Vec<String>),
}

fn plan_one(store: &Store, m: &MacroRecord, explicit: bool, taken: &mut HashSet<String>) -> Plan {
    // 幂等：已迁移过就不再迁移
    if let Some(sid) = store.macro_migration_get(&m.id) {
        match store.skill_get(&sid) {
            Some(s) => return Plan::Skipped(format!("已迁移为技能「{}」", s.name), Vec::new()),
            None if !explicit => {
                return Plan::Skipped(
                    "此前已迁移过，对应的技能已被删除；如需重新迁移，请在这条宏上单独点「转为技能」".into(),
                    Vec::new(),
                )
            }
            None => {}
        }
    }
    let c = convert_macro(m, taken);
    match c.verdict {
        Verdict::Draft { draft } => {
            taken.insert(draft.name.clone());
            Plan::Draft(draft, c.warnings)
        }
        Verdict::Skipped { reason } => Plan::Skipped(reason, c.warnings),
        Verdict::Failed { errors } => Plan::Failed(errors, c.warnings),
    }
}

fn taken_names(store: &Store) -> HashSet<String> {
    store.skill_list(None).into_iter().map(|s| s.name).collect()
}

/// 预览迁移（不写库）。
pub fn preview(store: &Store, ids: Option<&[String]>) -> Result<Vec<MigrationPreview>, String> {
    let explicit = ids.is_some();
    let mut taken = taken_names(store);
    Ok(select(store, ids)?
        .into_iter()
        .map(|t| match t {
            Err(id) => MigrationPreview {
                macro_id: id,
                macro_name: String::new(),
                draft: None,
                skip_reason: None,
                errors: vec!["宏不存在".into()],
                warnings: Vec::new(),
            },
            Ok(m) => {
                let mut p = MigrationPreview {
                    macro_id: m.id.clone(),
                    macro_name: m.name.clone(),
                    draft: None,
                    skip_reason: None,
                    errors: Vec::new(),
                    warnings: Vec::new(),
                };
                match plan_one(store, &m, explicit, &mut taken) {
                    Plan::Draft(d, w) => {
                        p.draft = Some(*d);
                        p.warnings = w;
                    }
                    Plan::Skipped(r, w) => {
                        p.skip_reason = Some(r);
                        p.warnings = w;
                    }
                    Plan::Failed(e, w) => {
                        p.errors = e;
                        p.warnings = w;
                    }
                }
                p
            }
        })
        .collect())
}

/// 执行迁移：逐条转换、落库、记幂等标记。**不删除、不修改原宏。**
pub fn migrate(store: &Store, ids: Option<&[String]>) -> Result<Vec<MigrationResult>, String> {
    let explicit = ids.is_some();
    let mut taken = taken_names(store);
    Ok(select(store, ids)?
        .into_iter()
        .map(|t| {
            let m = match t {
                Err(id) => {
                    return MigrationResult {
                        macro_id: id,
                        macro_name: String::new(),
                        outcome: MigrationOutcome::Failed { errors: vec!["宏不存在".into()] },
                        warnings: Vec::new(),
                    }
                }
                Ok(m) => m,
            };
            let (outcome, warnings) = match plan_one(store, &m, explicit, &mut taken) {
                Plan::Skipped(reason, w) => (MigrationOutcome::Skipped { reason }, w),
                Plan::Failed(errors, w) => (MigrationOutcome::Failed { errors }, w),
                Plan::Draft(draft, w) => (save(store, &m, *draft), w),
            };
            MigrationResult { macro_id: m.id, macro_name: m.name, outcome, warnings }
        })
        .collect())
}

/// 落库 + 记标记。标记写失败时回滚刚建的 skill，避免「建了却没记」导致下次重复迁移。
fn save(store: &Store, m: &MacroRecord, draft: SkillInput) -> MigrationOutcome {
    let skill = match store.skill_create(draft) {
        Ok(s) => s,
        Err(e) => return MigrationOutcome::Failed { errors: e.lines().map(str::to_string).collect() },
    };
    if let Err(e) = store.macro_migration_put(&m.id, &skill.id) {
        let _ = store.skill_delete(&skill.id);
        return MigrationOutcome::Failed { errors: vec![format!("记录迁移标记失败：{e}")] };
    }
    MigrationOutcome::Migrated { skill_id: skill.id }
}

// ——————————————————————————————— Tauri 命令 ———————————————————————————————

/// 预览把宏转成技能的结果（草稿 + 警告），**不写入任何东西**。`ids` 为空 = 全部宏。
#[tauri::command]
pub fn macros_migrate_preview(
    ids: Option<Vec<String>>,
    state: State<'_, AppState>,
) -> Result<Vec<MigrationPreview>, String> {
    preview(&state.store, ids.as_deref())
}

/// 把宏转成技能并保存。`ids` 为空 = 全部宏。已迁移过的宏会跳过；**不删除原宏**。
#[tauri::command]
pub fn macros_migrate(
    ids: Option<Vec<String>>,
    state: State<'_, AppState>,
) -> Result<Vec<MigrationResult>, String> {
    migrate(&state.store, ids.as_deref())
}

// ——————————————————————————————— 测试 ———————————————————————————————

#[cfg(test)]
mod tests {
    use super::*;

    fn send(t: &str, enter: bool) -> MacroStep {
        MacroStep::Send { text: t.into(), enter }
    }
    fn wait(p: &str, ms: u32) -> MacroStep {
        MacroStep::Wait { pattern: p.into(), timeout_ms: ms }
    }
    fn rec(name: &str, steps: Vec<MacroStep>) -> MacroRecord {
        MacroRecord { id: "m1".into(), name: name.into(), steps, created_at: 0, updated_at: 0 }
    }
    fn conv(steps: Vec<MacroStep>) -> Conversion {
        convert_macro(&rec("宏", steps), &HashSet::new())
    }
    fn draft(c: &Conversion) -> &SkillInput {
        match &c.verdict {
            Verdict::Draft { draft } => draft,
            other => panic!("期望草稿，实际 {other:?}（警告 {:?}）", c.warnings),
        }
    }
    fn exec_cmds(d: &SkillInput) -> Vec<String> {
        d.steps
            .iter()
            .filter_map(|s| match s {
                SkillStep::Exec { command, .. } => Some(command.clone()),
                _ => None,
            })
            .collect()
    }
    fn keys_of(s: &SkillStep) -> &str {
        match s {
            SkillStep::Keys { keys, .. } => keys,
            other => panic!("期望 keys，实际 {other:?}"),
        }
    }
    /// 口令的字面值不得出现在序列化后的任何地方（草稿 / 警告 / 判定）。
    fn assert_absent(c: &Conversion, literal: &str) {
        let json = serde_json::to_string(c).unwrap();
        assert!(!json.contains(literal), "口令字面值泄漏到了结果里：{json}");
    }
    fn is_secretish(s: &SkillStep) -> bool {
        matches!(s, SkillStep::Secret { .. } | SkillStep::ExpectRespond { secret: true, .. })
    }

    // —— 转义 ——

    #[test]
    fn unescape_matches_the_typescript_version() {
        assert_eq!(unescape_text(r"a\rb\n\t\e\0\\"), "a\rb\n\t\x1b\0\\");
        assert_eq!(unescape_text(r"\x03\x7f"), "\x03\x7f");
        // 非法 \x、未知转义、结尾孤立反斜杠：原样保留
        assert_eq!(unescape_text(r"\xZZ\q\"), r"\xZZ\q\");
    }

    // —— 打字合并 / 回车 ——

    #[test]
    fn typed_fragments_merge_into_one_exec_line() {
        let c = conv(vec![send("l", false), send("s -l", false), wait("", 0), send("a", true)]);
        assert_eq!(exec_cmds(draft(&c)), vec!["ls -la"]);
        assert_eq!(draft(&c).steps.len(), 1);
    }

    #[test]
    fn enter_flag_and_embedded_enters_split_lines() {
        let c = conv(vec![send("pwd", true), send(r"whoami\rdf -h\r", false), send("", true)]);
        let d = draft(&c);
        assert_eq!(exec_cmds(d), vec!["pwd", "whoami", "df -h"]);
        // 末尾的空回车 = 单独的 <Enter>
        assert_eq!(keys_of(d.steps.last().unwrap()), "<Enter>");
    }

    #[test]
    fn crlf_counts_once() {
        let c = conv(vec![send(r"ls\r\npwd", true)]);
        assert_eq!(exec_cmds(draft(&c)), vec!["ls", "pwd"]);
    }

    #[test]
    fn backspace_edits_the_pending_line() {
        let c = conv(vec![send(r"lss\x7f", false), send("", true)]);
        assert_eq!(exec_cmds(draft(&c)), vec!["ls"]);
    }

    #[test]
    fn exec_keeps_braces_literal() {
        let c = conv(vec![send("docker inspect -f '{{.Name}}' web", true)]);
        let d = draft(&c);
        // 迁移的宏没有参数：{{ 必须按字面保留，校验才不会报「占位符没有对应参数」
        assert_eq!(exec_cmds(d), vec![r"docker inspect -f '\{{.Name}}' web"]);
        assert!(d.params.is_empty());
    }

    // —— 控制键 ——

    #[test]
    fn ctrl_c_is_its_own_keys_step_and_interrupts_the_half_line() {
        let c = conv(vec![send("tail -f x", false), send(r"\x03", false), send("ls", true)]);
        let d = draft(&c);
        assert_eq!(keys_of(&d.steps[0]), "tail -f x");
        assert_eq!(keys_of(&d.steps[1]), "<C-c>");
        assert!(matches!(&d.steps[2], SkillStep::Exec { command, .. } if command == "ls"));
    }

    #[test]
    fn special_keys_turn_the_whole_line_into_keys() {
        // 录制器的形状：ls / Tab / 其余文本回车
        let c = conv(vec![send("ls", false), send(r"\t", false), send("/etc", true)]);
        let d = draft(&c);
        assert_eq!(d.steps.len(), 1);
        assert_eq!(keys_of(&d.steps[0]), "ls<Tab>/etc<Enter>");
    }

    #[test]
    fn arrow_keys_map_to_named_keys() {
        let c = conv(vec![send(r"\e[A", false), send("", true), send(r"\e[B\eOD\e[3~\e[1;5C", false)]);
        let d = draft(&c);
        assert_eq!(keys_of(&d.steps[0]), "<Up><Enter>");
        assert_eq!(keys_of(&d.steps[1]), "<Down><Left><Del>");
        // 带修饰键的方向键无法映射：丢弃并提醒
        assert!(c.warnings.iter().any(|w| w.contains("无法映射")), "{:?}", c.warnings);
    }

    #[test]
    fn literal_angle_and_backslash_are_escaped_inside_keys() {
        let c = conv(vec![send(r"a<b\\c", false), send(r"\t", false)]);
        assert_eq!(keys_of(&draft(&c).steps[0]), r"a<lt>b\\c<Tab>");
    }

    #[test]
    fn ctrl_letters_map_to_c_notation() {
        let c = conv(vec![send(r"\x01", false), send(r"\x05", false), send(r"\x04", false)]);
        let d = draft(&c);
        assert_eq!(keys_of(&d.steps[0]), "<C-a><C-e>");
        assert_eq!(keys_of(&d.steps[1]), "<C-d>");
    }

    // —— 延时 / 等待 ——

    #[test]
    fn delays_are_dropped_except_as_timeout_hints() {
        let c = conv(vec![
            wait("", 0),
            send("make build", true),
            wait("", 0),
            send("pwd", true),
            MacroStep::Sleep { ms: 300 },
            send("ls", true),
            MacroStep::Sleep { ms: 90_000 },
            wait("", 120_000),
            send("whoami", true),
        ]);
        let d = draft(&c);
        assert_eq!(exec_cmds(d), vec!["make build", "pwd", "ls", "whoami"]);
        assert_eq!(d.steps.len(), 4);
        assert_eq!(d.steps[0].timeout_ms(), None);
        assert_eq!(d.steps[1].timeout_ms(), None); // 300ms 的 sleep 不是提示
        // ls 之后的 90s sleep 与 120s 超时：取较大者
        assert_eq!(d.steps[2].timeout_ms(), Some(120_000));
        assert_eq!(d.steps[3].timeout_ms(), None);
    }

    #[test]
    fn pattern_wait_becomes_expect_after_of_previous_step() {
        let c = conv(vec![send("systemctl restart nginx", true), wait("active (running)", 15_000), send("ls", true)]);
        let d = draft(&c);
        assert_eq!(d.steps[0].expect_after(), Some("active (running)"));
        assert_eq!(d.steps[0].timeout_ms(), Some(15_000));
        assert_eq!(d.steps[1].expect_after(), None);
    }

    #[test]
    fn leading_pattern_wait_becomes_screen_check_and_bad_pattern_is_dropped() {
        let c = conv(vec![wait("login:", 0), send("ls", true), wait("/(unclosed/", 0)]);
        let d = draft(&c);
        assert!(matches!(&d.steps[0], SkillStep::ScreenCheck { expect, .. } if expect == "login:"));
        assert_eq!(d.steps.len(), 2);
        assert!(c.warnings.iter().any(|w| w.contains("不合法")), "{:?}", c.warnings);
    }

    #[test]
    fn empty_and_wait_only_macros_are_skipped() {
        for steps in [vec![], vec![wait("", 0), MacroStep::Sleep { ms: 5000 }], vec![send("", false)]] {
            let c = conv(steps);
            assert!(matches!(c.verdict, Verdict::Skipped { .. }), "{:?}", c.verdict);
        }
    }

    // —— 交互会话 ——

    #[test]
    fn lines_inside_interactive_sessions_are_keys_not_exec() {
        // 跳板机：ssh 之后敲菜单序号
        let c = conv(vec![send("ssh ops@jump", true), wait("", 0), send("6", true), send("exit", true), send("ls", true)]);
        let d = draft(&c);
        assert_eq!(keys_of(&d.steps[0]), "ssh ops@jump<Enter>");
        assert_eq!(keys_of(&d.steps[1]), "6<Enter>");
        assert_eq!(keys_of(&d.steps[2]), "exit<Enter>");
        // exit 之后回到本地 shell：恢复 exec
        assert!(matches!(&d.steps[3], SkillStep::Exec { command, .. } if command == "ls"));
        assert!(c.warnings.iter().any(|w| w.contains("交互式会话")), "{:?}", c.warnings);
    }

    #[test]
    fn non_interactive_database_one_shot_stays_exec() {
        let c = conv(vec![send("mysql -e 'select 1'", true), send("ls", true)]);
        assert_eq!(exec_cmds(draft(&c)), vec!["mysql -e 'select 1'", "ls"]);
    }

    // —— 口令 ——

    #[test]
    fn password_after_prompt_wait_becomes_secret_expect_respond() {
        let pw = "Tr0ub4dor&3";
        let c = conv(vec![send("ssh root@10.0.0.1", true), wait("Password:", 30_000), send(pw, true), send("uptime", true)]);
        assert_absent(&c, pw);
        let d = draft(&c);
        assert!(matches!(
            &d.steps[1],
            SkillStep::ExpectRespond { expect, respond, secret: true, timeout_ms: Some(30_000), .. }
                if expect == "Password:" && respond.is_empty()
        ));
        assert!(c.warnings.iter().any(|w| w.contains("疑似口令")));
        assert!(c.warnings.iter().any(|w| w.contains("原宏里仍保存着")));
    }

    #[test]
    fn password_after_quiet_wait_following_ssh_becomes_secret_step() {
        // 录制器的形状：wait(静默) 在每行之前，没有提示 pattern
        let pw = "hunter2";
        let c = conv(vec![wait("", 0), send("ssh deploy@host", true), wait("", 0), send(pw, true), wait("", 0), send("df -h", true)]);
        assert_absent(&c, pw);
        let d = draft(&c);
        assert!(matches!(&d.steps[1], SkillStep::Secret { .. }), "{:?}", d.steps);
        // 口令后的下一行仍是 ssh 会话里的命令
        assert_eq!(keys_of(&d.steps[2]), "df -h<Enter>");
    }

    #[test]
    fn password_for_sudo_and_su_and_passwd() {
        for first in ["sudo systemctl restart nginx", "su -", "passwd", "scp a b:/tmp", "mysql -u root -p", "docker login reg.io"] {
            let c = conv(vec![send(first, true), send("correct horse", true)]);
            assert_absent(&c, "correct horse");
            assert!(draft(&c).steps.iter().any(is_secretish), "{first}");
        }
    }

    #[test]
    fn password_prompt_variants_are_recognized() {
        for p in ["Password:", "[sudo] password for bob:", "/[Pp]assword/", "Enter passphrase", "请输入密码", "口令：", "Verification code:", "OTP"] {
            let c = conv(vec![send("ssh h", true), wait(p, 0), send("SeCr3t!Value", true)]);
            assert_absent(&c, "SeCr3t!Value");
            assert!(draft(&c).steps.iter().any(is_secretish), "{p}");
        }
    }

    #[test]
    fn empty_enter_after_password_prompt_is_still_a_secret() {
        let c = conv(vec![send("ssh h", true), wait("Password:", 0), send("", true)]);
        assert!(draft(&c).steps.iter().any(is_secretish));
    }

    #[test]
    fn unconsumed_password_prompt_wait_degrades_to_expect_after() {
        let c = conv(vec![send("ssh h", true), wait("Password:", 0)]);
        assert_eq!(draft(&c).steps[0].expect_after(), Some("Password:"));
    }

    #[test]
    fn literal_credentials_in_command_lines_are_dropped() {
        let cases = [
            ("mysql -uroot -pS3cretPw db", "S3cretPw"),
            ("sshpass -p 'my pass' ssh a@b", "my pass"),
            ("curl -H 'Authorization: Bearer abcdef1234567890' x", "abcdef1234567890"),
            ("export DB_PASS=Zx9Qv7Lm2", "Zx9Qv7Lm2"),
            ("echo Zx9Qv7Lm2 | sudo -S ls", "Zx9Qv7Lm2"),
            ("git clone https://bob:Zx9Qv7Lm2@host/r.git", "Zx9Qv7Lm2"),
            ("curl -u admin:Zx9Qv7Lm2 http://x", "Zx9Qv7Lm2"),
            ("redis-cli -a Zx9Qv7Lm2 ping", "Zx9Qv7Lm2"),
            ("login.sh --password=Zx9Qv7Lm2", "Zx9Qv7Lm2"),
        ];
        for (line, lit) in cases {
            let c = conv(vec![send("pwd", true), send(line, true)]);
            assert_absent(&c, lit);
            let d = draft(&c);
            assert!(is_secretish(&d.steps[1]), "{line}：{:?}", d.steps);
            assert!(matches!(&d.steps[0], SkillStep::Exec { command, .. } if command == "pwd"));
        }
    }

    #[test]
    fn env_references_are_not_mistaken_for_literals() {
        let c = conv(vec![send("export DB_PASS=$VAULT_PASS", true)]);
        assert_eq!(exec_cmds(draft(&c)), vec!["export DB_PASS=$VAULT_PASS"]);
    }

    #[test]
    fn literal_credential_inside_a_composite_keys_line_is_dropped() {
        let c = conv(vec![send("sshpass -p Zx9Qv7Lm2", false), send(r"\t", false), send(" ssh h", true)]);
        assert_absent(&c, "Zx9Qv7Lm2");
        assert!(draft(&c).steps.iter().any(is_secretish));
    }

    #[test]
    fn password_shaped_single_word_is_dropped_but_commands_and_paths_are_not() {
        let c = conv(vec![send("Xk9#mP2$vLq", true)]);
        assert_absent(&c, "Xk9#mP2$vLq");
        assert!(is_secretish(&draft(&c).steps[0]));

        let c = conv(vec![send("./deploy.sh", true), send("/usr/local/bin/Run2Go", true), send("deploy-2024.sh", true), send("ls -la", true)]);
        let d = draft(&c);
        assert_eq!(d.steps.len(), 4);
        assert!(d.steps.iter().all(|s| !is_secretish(s)), "{:?}", d.steps);
    }

    #[test]
    fn commands_and_menu_choices_after_login_are_not_treated_as_passwords() {
        let c = conv(vec![send("ssh h", true), send("yes", true), send("ls -l /var", true), send("3", true)]);
        let d = draft(&c);
        assert!(d.steps.iter().all(|s| !is_secretish(s)), "{:?}", d.steps);
        // 且「紧跟」只管第一行非空内容：随后的普通词不再被怀疑
        let c = conv(vec![send("ssh h", true), send("hunter2", true), send("hello", true)]);
        let d = draft(&c);
        assert!(is_secretish(&d.steps[1]));
        assert!(!is_secretish(&d.steps[2]));
    }

    #[test]
    fn otp_after_password_is_secret() {
        let c = conv(vec![send("ssh h", true), send("hunter2", true), send("482913", true)]);
        assert_absent(&c, "482913");
        assert_absent(&c, "hunter2");
        let d = draft(&c);
        assert!(is_secretish(&d.steps[1]) && is_secretish(&d.steps[2]));
    }

    #[test]
    fn warnings_never_quote_the_suspected_text() {
        let pw = "Zq8!wErTy_unique";
        let c = conv(vec![send("ssh h", true), wait("Password:", 0), send(pw, true), send(&format!("mysql -pZq8Leak{pw}"), true)]);
        assert_absent(&c, pw);
        assert_absent(&c, "Zq8Leak");
    }

    #[test]
    fn validation_fallback_replaces_credential_steps_instead_of_keeping_them() {
        // 绕过启发式直接构造：校验报凭据类错误时必须被换成占位，而不是整份失败或静默保留
        let input = SkillInput {
            name: "x".into(),
            description: String::new(),
            params: vec![],
            steps: vec![
                exec_step("pwd".into()),
                keys_step("sshpass -p Zx9Qv7Lm2 ssh h<Enter>".into()),
                SkillStep::ExpectRespond {
                    expect: "Password:".into(),
                    respond: "Zx9Qv7Lm2".into(),
                    secret: false,
                    timeout_ms: None,
                    expect_after: None,
                    on_unexpected: OnUnexpected::Ask,
                },
            ],
            verify: None,
            risk: skill::Risk::Low,
            host_scope: HostScope::Any,
            source: Some(SkillSource::MigratedMacro),
        };
        let mut warnings = Vec::new();
        let out = finalize(input, &mut warnings).unwrap();
        assert!(matches!(&out.steps[0], SkillStep::Exec { .. }));
        assert!(is_secretish(&out.steps[1]) && is_secretish(&out.steps[2]));
        assert!(!serde_json::to_string(&out).unwrap().contains("Zx9Qv7Lm2"));
        assert_eq!(warnings.len(), 2);
    }

    // —— 结果形状 ——

    #[test]
    fn draft_has_expected_metadata_and_passes_validation() {
        let c = conv(vec![send("systemctl status nginx", true), send("rm -rf /tmp/x", true)]);
        let d = draft(&c);
        assert_eq!(d.source, Some(SkillSource::MigratedMacro));
        assert_eq!(d.host_scope, HostScope::Any);
        assert!(d.params.is_empty() && d.verify.is_none());
        // rm -rf 是危险命令：风险被评估为 High
        assert_eq!(d.risk, skill::Risk::High);
        assert_eq!(d.risk, assess_risk(&d.steps).risk);
        skill::validate(&d.clone().into_skill("id".into(), 1)).unwrap();
        assert!(d.description.contains("迁移"));
    }

    #[test]
    fn too_many_steps_is_reported_as_failed_not_truncated() {
        let steps: Vec<MacroStep> = (0..=MAX_STEPS).map(|i| send(&format!("echo {i}"), true)).collect();
        let c = conv(steps);
        assert!(matches!(&c.verdict, Verdict::Failed { errors } if errors.iter().any(|e| e.contains("步骤最多"))), "{:?}", c.verdict);
    }

    // —— 名称去重 ——

    #[test]
    fn names_are_deduplicated_with_numeric_suffix() {
        let mut taken: HashSet<String> = HashSet::new();
        assert_eq!(unique_name("部署", &taken), "部署");
        taken.insert("部署".into());
        assert_eq!(unique_name("部署", &taken), "部署 (2)");
        taken.insert("部署 (2)".into());
        assert_eq!(unique_name("部署", &taken), "部署 (3)");
        assert_eq!(unique_name("  ", &HashSet::new()), "未命名宏");
        // 超长名称：截断后加后缀仍不超上限
        let long: String = "长".repeat(200);
        let first = unique_name(&long, &HashSet::new());
        assert_eq!(first.chars().count(), MAX_NAME_CHARS);
        let second = unique_name(&long, &HashSet::from([first]));
        assert!(second.chars().count() <= MAX_NAME_CHARS && second.ends_with(" (2)"));
    }

    #[test]
    fn convert_uses_taken_names() {
        let taken: HashSet<String> = ["宏".to_string()].into();
        let c = convert_macro(&rec("宏", vec![send("ls", true)]), &taken);
        assert_eq!(draft(&c).name, "宏 (2)");
    }

    // —— 编排：预览 / 迁移 / 幂等（真实的临时库） ——

    fn temp_store() -> Store {
        let dir = std::env::temp_dir().join(format!("nexus-macro-mig-{}", uuid::Uuid::new_v4()));
        Store::open(dir).expect("打开临时库")
    }

    #[test]
    fn preview_writes_nothing_and_migrate_is_idempotent_and_keeps_macros() {
        let store = temp_store();
        let pw = "Zq8!wErTy_unique";
        let a = store.create_macro("巡检", &[send("uptime", true), send("df -h", true)]).unwrap();
        let b = store.create_macro("登录", &[send("ssh ops@h", true), wait("Password:", 0), send(pw, true)]).unwrap();
        let empty = store.create_macro("空宏", &[]).unwrap();
        // 已有同名 skill：迁移出的名称要加后缀
        store
            .skill_create(SkillInput {
                name: "巡检".into(),
                description: String::new(),
                params: vec![],
                steps: vec![exec_step("ls".into())],
                verify: None,
                risk: skill::Risk::Low,
                host_scope: HostScope::Any,
                source: None,
            })
            .unwrap();
        let before = store.skill_list(None).len();

        // 预览：不写库，也不留标记
        let pv = preview(&store, None).unwrap();
        assert_eq!(pv.len(), 3);
        assert_eq!(pv[0].draft.as_ref().unwrap().name, "巡检 (2)");
        assert!(pv[1].warnings.iter().any(|w| w.contains("疑似口令")));
        assert!(pv[2].skip_reason.is_some() && pv[2].draft.is_none());
        assert_eq!(store.skill_list(None).len(), before);
        assert!(store.macro_migration_get(&a.id).is_none());
        assert!(!serde_json::to_string(&pv).unwrap().contains(pw));

        // 迁移
        let res = migrate(&store, None).unwrap();
        assert_eq!(res.len(), 3);
        let sid_a = match &res[0].outcome {
            MigrationOutcome::Migrated { skill_id } => skill_id.clone(),
            o => panic!("{o:?}"),
        };
        assert!(matches!(&res[1].outcome, MigrationOutcome::Migrated { .. }));
        assert!(matches!(&res[2].outcome, MigrationOutcome::Skipped { .. }));
        assert_eq!(res[0].macro_id, a.id);
        assert!(!serde_json::to_string(&res).unwrap().contains(pw));

        // 入库的 skill：来源 / 计数 / 名称；口令不在库里
        let sa = store.skill_get(&sid_a).unwrap();
        assert_eq!(sa.name, "巡检 (2)");
        assert_eq!(sa.source, SkillSource::MigratedMacro);
        assert_eq!((sa.success_count, sa.fail_count, sa.last_run_ts), (0, 0, None));
        let all = serde_json::to_string(&store.skill_list(None)).unwrap();
        assert!(!all.contains(pw));
        assert_eq!(store.skill_list(None).len(), before + 2);
        // 原宏原样保留
        assert_eq!(store.list_macros().unwrap().len(), 3);
        assert_eq!(store.get_macro(&b.id).unwrap().steps.len(), 3);
        assert_eq!(store.get_macro(&empty.id).unwrap().steps.len(), 0);

        // 幂等：再迁一次全部跳过、不新增
        let again = migrate(&store, None).unwrap();
        assert!(matches!(&again[0].outcome, MigrationOutcome::Skipped { reason } if reason.contains("已迁移")));
        assert!(matches!(&again[1].outcome, MigrationOutcome::Skipped { .. }));
        assert_eq!(store.skill_list(None).len(), before + 2);
        let again_ids = migrate(&store, Some(&[a.id.clone()])).unwrap();
        assert!(matches!(&again_ids[0].outcome, MigrationOutcome::Skipped { .. }));
        assert_eq!(store.skill_list(None).len(), before + 2);
    }

    #[test]
    fn deleted_migrated_skill_is_not_resurrected_by_migrate_all_but_can_be_redone_explicitly() {
        let store = temp_store();
        let a = store.create_macro("巡检", &[send("uptime", true)]).unwrap();
        let sid = match &migrate(&store, None).unwrap()[0].outcome {
            MigrationOutcome::Migrated { skill_id } => skill_id.clone(),
            o => panic!("{o:?}"),
        };
        assert!(store.skill_delete(&sid).unwrap());

        let all = migrate(&store, None).unwrap();
        assert!(matches!(&all[0].outcome, MigrationOutcome::Skipped { reason } if reason.contains("已被删除")));
        assert!(store.skill_list(None).is_empty());

        let one = migrate(&store, Some(&[a.id.clone()])).unwrap();
        let new_id = match &one[0].outcome {
            MigrationOutcome::Migrated { skill_id } => skill_id.clone(),
            o => panic!("{o:?}"),
        };
        assert_ne!(new_id, sid);
        assert_eq!(store.skill_list(None).len(), 1);
        assert_eq!(store.macro_migration_get(&a.id), Some(new_id));
    }

    #[test]
    fn unknown_ids_and_selection_order() {
        let store = temp_store();
        let a = store.create_macro("甲", &[send("ls", true)]).unwrap();
        let b = store.create_macro("乙", &[send("pwd", true)]).unwrap();
        let res = migrate(&store, Some(&[b.id.clone(), "nope".into()])).unwrap();
        assert_eq!(res[0].macro_id, b.id);
        assert!(matches!(&res[1].outcome, MigrationOutcome::Failed { errors } if errors[0].contains("不存在")));
        assert!(store.macro_migration_get(&a.id).is_none());
        assert_eq!(store.skill_list(None).len(), 1);
    }

    #[test]
    fn same_name_macros_in_one_batch_get_distinct_names() {
        let store = temp_store();
        store.create_macro("同名", &[send("ls", true)]).unwrap();
        store.create_macro("同名", &[send("pwd", true)]).unwrap();
        let res = migrate(&store, None).unwrap();
        assert!(res.iter().all(|r| matches!(r.outcome, MigrationOutcome::Migrated { .. })));
        let mut names: Vec<String> = store.skill_list(None).into_iter().map(|s| s.name).collect();
        names.sort();
        assert_eq!(names, vec!["同名", "同名 (2)"]);
    }

    #[test]
    fn result_json_shape_is_stable() {
        let r = MigrationResult {
            macro_id: "m".into(),
            macro_name: "n".into(),
            outcome: MigrationOutcome::Migrated { skill_id: "s".into() },
            warnings: vec!["w".into()],
        };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            serde_json::json!({"macroId":"m","macroName":"n","outcome":{"kind":"migrated","skillId":"s"},"warnings":["w"]})
        );
        let f = MigrationOutcome::Failed { errors: vec!["e".into()] };
        assert_eq!(serde_json::to_value(&f).unwrap(), serde_json::json!({"kind":"failed","errors":["e"]}));
    }
}
