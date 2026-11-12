//! Agent 与终端的「交互原语」：原始按键解析、等待某段文字出现。
//!
//! `nx_exec` 是「敲一行命令、等它跑完」，这对 `vim`、`passwd`、`apt` 的 `[Y/n]`、
//! `python` 的 REPL、卡住需要 Ctrl-C 的进程都不够用。这里补两样东西：
//!
//! - [`parse_keys`]：把模型写的「带转义的按键串」解码成要写进 pty 的字节。
//!   不追加换行、不等命令结束 —— 写进去就算完。
//! - [`wait_for_pattern`]：对旁路缓冲做「等某段文字出现 / 等输出静止」。
//!   纯逻辑、不碰 Tauri，所有时间与缓冲读取都由闭包注入，便于单测。
//!
//! ## 按键串语法（`nx_send_keys` 的 `keys` 参数）
//!
//! - 普通字符原样发送（UTF-8）。
//! - 反斜杠转义：`\r` 回车(CR)、`\n` 换行(LF)、`\t` 制表、`\e` ESC、`\\` 反斜杠、
//!   `\xNN` 任意字节（两位十六进制，如 `\x03` = Ctrl-C）。
//! - 尖括号具名键（不分大小写）：`<Enter>` `<Tab>` `<Esc>` `<Space>` `<BS>` `<Del>`
//!   `<Up>` `<Down>` `<Left>` `<Right>` `<Home>` `<End>` `<PgUp>` `<PgDn>` `<S-Tab>`
//!   `<F1>`…`<F12>`，`<C-a>`…`<C-z>`（Ctrl 组合，另有 `<C-[>` `<C-\>` `<C-]>` `<C-@>`），
//!   `<M-x>`（Alt/Meta + 单个字符，即 ESC 前缀），`<lt>` 字面的 `<`。
//! - 重复：`<Down*3>` = 连按三次（1–50 次），任何具名键都可以，方向键菜单靠它挪到目标项。
//! - 形如 `<名字>` 但名字不认识会**报错**而不是当字面量发送（避免 `<C-cc>` 这类笔误悄悄敲进终端）；
//!   不成对的 `<`（`a < b`、`<<EOF`）照常当字面字符。
//! - 终端处于应用光标键模式（DECCKM，vim / less / 不少 TUI 会打开）时，方向键与 Home / End 发 SS3
//!   （`\eOA`…）而不是 CSI（`\e[A`…），见 [`parse_keys_mode`]。模式来自前端屏幕帧；没有帧时按普通模式发。

use std::time::Duration;

use regex::Regex;
use tokio::time::{sleep, Instant};

/// 一次 `nx_send_keys` 解码后最多允许多少字节。
///
/// 4096 字节够给 Claude Code 之类的工具敲一整段需求（中文一个字 3 字节），也够一长串方向键；
/// 再长的内容（粘贴一整个文件）不该走这条路，也不该让一次工具调用无声地倾倒进终端。
pub const MAX_KEYS_BYTES: usize = 4096;

/// 具名键最多重复几次（`<Down*50>`）
pub const MAX_KEY_REPEAT: usize = 50;

/// 解析按键串（普通光标键模式）。成功返回要写入的字节，失败返回给模型看的原因。
pub fn parse_keys(src: &str) -> Result<Vec<u8>, String> {
    parse_keys_mode(src, false)
}

/// 同 [`parse_keys`]；`app_cursor` = 终端处于应用光标键模式（DECCKM），方向键 / Home / End 发 SS3。
pub fn parse_keys_mode(src: &str, app_cursor: bool) -> Result<Vec<u8>, String> {
    let chars: Vec<char> = src.chars().collect();
    let mut out: Vec<u8> = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => {
                let Some(&n) = chars.get(i + 1) else {
                    return Err("按键串以孤立的反斜杠结尾（要发字面反斜杠请写 \\\\）".into());
                };
                match n {
                    'r' => out.push(0x0d),
                    'n' => out.push(0x0a),
                    't' => out.push(0x09),
                    'e' | 'E' => out.push(0x1b),
                    '\\' => out.push(b'\\'),
                    'x' | 'X' => {
                        let hex: String = chars.iter().skip(i + 2).take(2).collect();
                        if hex.chars().count() != 2 || !hex.chars().all(|h| h.is_ascii_hexdigit()) {
                            return Err("`\\x` 后面必须是两位十六进制，如 \\x03".into());
                        }
                        // 上面已确认两位都是十六进制，from_str_radix 不会失败
                        out.push(u8::from_str_radix(&hex, 16).map_err(|e| e.to_string())?);
                        i += 2;
                    }
                    other => {
                        return Err(format!(
                            "不认识的转义 `\\{other}`（支持 \\r \\n \\t \\e \\\\ \\xNN）"
                        ));
                    }
                }
                i += 2;
            }
            '<' => match named_key_at(&chars, i, app_cursor)? {
                Some((bytes, consumed)) => {
                    out.extend_from_slice(&bytes);
                    i += consumed;
                }
                None => {
                    out.push(b'<');
                    i += 1;
                }
            },
            _ => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                i += 1;
            }
        }
        if out.len() > MAX_KEYS_BYTES {
            return Err(format!("按键串解码后超过 {MAX_KEYS_BYTES} 字节上限，请拆成几次发送"));
        }
    }
    if out.is_empty() {
        return Err("按键串为空".into());
    }
    Ok(out)
}

/// 在 `chars[at] == '<'` 处尝试读一个具名键。
///
/// - `Ok(Some((字节, 吃掉的字符数)))`：读到了；
/// - `Ok(None)`：这个 `<` 不是具名键的开头，当字面字符；
/// - `Err`：形如 `<名字>` 但名字不认识。
fn named_key_at(chars: &[char], at: usize, app_cursor: bool) -> Result<Option<(Vec<u8>, usize)>, String> {
    // 向后找 `>`，名字最长 15 个字符（`<Backspace>`、`<PgDn*50>` 之类也够用）。
    let mut end = None;
    for (off, &c) in chars.iter().enumerate().skip(at + 1).take(15) {
        if c == '>' {
            end = Some(off);
            break;
        }
        // 名字里只能有字母数字与 `-`、`[`、`\`、`]`、`@`
        if !(c.is_ascii_alphanumeric() || matches!(c, '-' | '[' | '\\' | ']' | '@' | '*')) {
            return Ok(None);
        }
    }
    let Some(end) = end else { return Ok(None) };
    let name: String = chars[at + 1..end].iter().collect();
    if name.is_empty() {
        return Ok(None);
    }
    // `<名字*N>`：重复 N 次
    let (base, times) = match name.rsplit_once('*') {
        Some((b, n)) if !b.is_empty() => match n.parse::<usize>() {
            Ok(t) if (1..=MAX_KEY_REPEAT).contains(&t) => (b.to_string(), t),
            _ => return Err(format!("`<{name}>` 的重复次数要在 1–{MAX_KEY_REPEAT} 之间")),
        },
        _ => (name.clone(), 1),
    };
    match key_bytes(&base, app_cursor) {
        Some(b) => Ok(Some((b.repeat(times), end - at + 1))),
        None => Err(format!(
            "不认识的按键 `<{name}>`（要发字面 `<` 请写 <lt>；支持的键见 nx_send_keys 的说明）"
        )),
    }
}

fn key_bytes(name: &str, app_cursor: bool) -> Option<Vec<u8>> {
    let lower = name.to_ascii_lowercase();
    // 应用光标键模式（DECCKM）：方向键 / Home / End 走 SS3
    if app_cursor {
        let ss3: Option<&[u8]> = match lower.as_str() {
            "up" => Some(b"\x1bOA"),
            "down" => Some(b"\x1bOB"),
            "right" => Some(b"\x1bOC"),
            "left" => Some(b"\x1bOD"),
            "home" => Some(b"\x1bOH"),
            "end" => Some(b"\x1bOF"),
            _ => None,
        };
        if let Some(b) = ss3 {
            return Some(b.to_vec());
        }
    }
    // Ctrl 组合
    if let Some(rest) = lower.strip_prefix("c-") {
        let mut it = rest.chars();
        let (ch, none_more) = (it.next()?, it.next().is_none());
        if !none_more {
            return None;
        }
        return match ch {
            'a'..='z' => Some(vec![ch as u8 - b'a' + 1]),
            '@' => Some(vec![0x00]),
            '[' => Some(vec![0x1b]),
            '\\' => Some(vec![0x1c]),
            ']' => Some(vec![0x1d]),
            _ => None,
        };
    }
    // Alt / Meta + 单个字符：ESC 前缀。用原始名字取字符，保留大小写。
    if lower.starts_with("m-") {
        let rest: Vec<char> = name.chars().skip(2).collect();
        if rest.len() != 1 {
            return None;
        }
        let mut v = vec![0x1b];
        let mut buf = [0u8; 4];
        v.extend_from_slice(rest[0].encode_utf8(&mut buf).as_bytes());
        return Some(v);
    }
    let seq: &[u8] = match lower.as_str() {
        "enter" | "return" | "cr" => b"\r",
        "tab" => b"\t",
        "s-tab" | "backtab" => b"\x1b[Z",
        "esc" | "escape" => b"\x1b",
        "space" => b" ",
        "bs" | "backspace" => b"\x7f",
        "del" | "delete" => b"\x1b[3~",
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        "home" => b"\x1b[H",
        "end" => b"\x1b[F",
        "pgup" | "pageup" => b"\x1b[5~",
        "pgdn" | "pagedown" => b"\x1b[6~",
        "ins" | "insert" => b"\x1b[2~",
        "lt" => b"<",
        "f1" => b"\x1bOP",
        "f2" => b"\x1bOQ",
        "f3" => b"\x1bOR",
        "f4" => b"\x1bOS",
        "f5" => b"\x1b[15~",
        "f6" => b"\x1b[17~",
        "f7" => b"\x1b[18~",
        "f8" => b"\x1b[19~",
        "f9" => b"\x1b[20~",
        "f10" => b"\x1b[21~",
        "f11" => b"\x1b[23~",
        "f12" => b"\x1b[24~",
        _ => return None,
    };
    Some(seq.to_vec())
}

/// 这串字节里有没有「提交一行」的动作（CR / LF）。
///
/// Approval 模式据此决定要不要人工批准：没有回车，按键只是往行里加字，什么都还没执行。
pub fn contains_enter(bytes: &[u8]) -> bool {
    bytes.iter().any(|b| *b == 0x0d || *b == 0x0a)
}

/// 把要发送的字节做成可读的一句话（审批卡 / 步骤卡 / trace 用）：
/// 控制字符写成 `<C-c>` `<Enter>` `<Esc>` 这类人话，可见字符原样。
pub fn display_keys(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::new();
    for c in text.chars() {
        match c {
            '\r' => out.push_str("<Enter>"),
            '\n' => out.push_str("<LF>"),
            '\t' => out.push_str("<Tab>"),
            '\x1b' => out.push_str("<Esc>"),
            '\x7f' => out.push_str("<BS>"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("<C-{}>", (c as u8 + b'a' - 1) as char));
            }
            c => out.push(c),
        }
    }
    out
}

// ——————————————————————————————— 等待某段文字 ———————————————————————————————

/// `nx_wait` 的匹配目标：子串，或 `/regex/`（可带 `i` 标志）。
#[derive(Debug, Clone)]
pub enum Pattern {
    Substring(String),
    Regex(Regex),
}

impl Pattern {
    /// 解析模型给的 pattern。以 `/` 开头、并含第二个 `/` 的按正则（`/err(or)?/i`）；
    /// 其余一律当子串。正则写错返回原因，让模型改。
    pub fn parse(raw: &str) -> Result<Self, String> {
        if raw.is_empty() {
            return Err("pattern 为空".into());
        }
        if let Some(rest) = raw.strip_prefix('/') {
            if let Some(close) = rest.rfind('/') {
                let body = &rest[..close];
                let flags = &rest[close + 1..];
                if !body.is_empty() && flags.chars().all(|f| f == 'i') {
                    let src = if flags.contains('i') { format!("(?i){body}") } else { body.to_string() };
                    return Regex::new(&src)
                        .map(Pattern::Regex)
                        .map_err(|e| format!("正则无法编译：{e}"));
                }
            }
        }
        Ok(Pattern::Substring(raw.to_string()))
    }

    pub fn is_match(&self, text: &str) -> bool {
        match self {
            Pattern::Substring(s) => text.contains(s.as_str()),
            Pattern::Regex(r) => r.is_match(text),
        }
    }
}

/// 一次等待的参数。
#[derive(Debug, Clone)]
pub struct WaitSpec {
    pub pattern: Option<Pattern>,
    /// 连续这么久没有新输出就返回。`None` = 不按静止返回，只等 pattern / 超时。
    pub quiet_ms: Option<u64>,
    pub timeout_ms: u64,
    pub poll_ms: u64,
}

/// 超时上限与默认值。上限卡死是为了不让一次工具调用把整轮挂住。
pub const WAIT_DEFAULT_TIMEOUT_MS: u64 = 15_000;
pub const WAIT_MAX_TIMEOUT_MS: u64 = 60_000;
/// 没给 pattern 也没给 quietMs 时的静止窗口。
pub const WAIT_DEFAULT_QUIET_MS: u64 = 1_500;
const WAIT_MIN_QUIET_MS: u64 = 200;
const WAIT_MAX_QUIET_MS: u64 = 30_000;
const WAIT_POLL_MS: u64 = 120;

impl WaitSpec {
    /// 按模型给的原始参数规整：超时封顶、静止窗口夹到合理范围，
    /// pattern 与 quietMs 都缺省时退回「等静止」。
    pub fn from_args(
        pattern: Option<Pattern>,
        timeout_ms: Option<u64>,
        quiet_ms: Option<u64>,
    ) -> Self {
        let timeout_ms = timeout_ms.unwrap_or(WAIT_DEFAULT_TIMEOUT_MS).clamp(100, WAIT_MAX_TIMEOUT_MS);
        let quiet_ms = match (&pattern, quiet_ms) {
            (_, Some(q)) => Some(q.clamp(WAIT_MIN_QUIET_MS, WAIT_MAX_QUIET_MS)),
            (None, None) => Some(WAIT_DEFAULT_QUIET_MS),
            (Some(_), None) => None,
        };
        Self { pattern, quiet_ms, timeout_ms, poll_ms: WAIT_POLL_MS }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitEnd {
    /// pattern 出现了
    Matched,
    /// 输出静止了 quiet_ms
    Quiet,
    Timeout,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct WaitResult {
    pub end: WaitEnd,
    /// 等待期间（自 mark 起）的输出
    pub output: String,
}

/// 轮询缓冲，直到命中 pattern / 输出静止 / 超时 / 被取消。
///
/// - `pushed`：缓冲有史以来的字节总数（便宜，用来判断「有没有新输出」）；
/// - `since`：从调用方记下的 mark 起的输出文本（只在有新字节时才取，避免每轮都扫缓冲）；
/// - `cancelled`：用户是否已取消本轮。
pub async fn wait_for_pattern(
    spec: &WaitSpec,
    pushed: impl Fn() -> u64,
    since: impl Fn() -> String,
    cancelled: impl Fn() -> bool,
) -> WaitResult {
    let started = Instant::now();
    let mut last_pushed = pushed();
    let mut last_change = started;
    let mut text = since();
    // 进来时就已经有内容：pattern 可能早就出现了（等待开始前，模型思考期间到的输出）
    if let Some(p) = &spec.pattern {
        if p.is_match(&text) {
            return WaitResult { end: WaitEnd::Matched, output: text };
        }
    }
    loop {
        if cancelled() {
            return WaitResult { end: WaitEnd::Cancelled, output: text };
        }
        let now = Instant::now();
        if now.duration_since(started) >= Duration::from_millis(spec.timeout_ms) {
            return WaitResult { end: WaitEnd::Timeout, output: text };
        }
        if let Some(q) = spec.quiet_ms {
            if now.duration_since(last_change) >= Duration::from_millis(q) {
                return WaitResult { end: WaitEnd::Quiet, output: text };
            }
        }
        sleep(Duration::from_millis(spec.poll_ms)).await;

        let p = pushed();
        if p != last_pushed {
            last_pushed = p;
            last_change = Instant::now();
            text = since();
            if let Some(pat) = &spec.pattern {
                if pat.is_match(&text) {
                    return WaitResult { end: WaitEnd::Matched, output: text };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn repeat_syntax() {
        assert_eq!(parse_keys("<Down*3><Enter>").unwrap(), b"\x1b[B\x1b[B\x1b[B\r".to_vec());
        assert_eq!(parse_keys("<up*1>").unwrap(), b"\x1b[A".to_vec());
        assert_eq!(parse_keys("<C-c*2>").unwrap(), vec![3, 3]);
        assert!(parse_keys("<Down*0>").is_err());
        assert!(parse_keys("<Down*51>").is_err());
        assert!(parse_keys("<Down*x>").is_err());
        // 不成对的照旧当字面量
        assert_eq!(parse_keys("a*b").unwrap(), b"a*b".to_vec());
    }

    #[test]
    fn ss3_arrows_in_application_cursor_mode() {
        assert_eq!(parse_keys_mode("<Up><Down><Right><Left>", true).unwrap(), b"\x1bOA\x1bOB\x1bOC\x1bOD".to_vec());
        assert_eq!(parse_keys_mode("<Home><End>", true).unwrap(), b"\x1bOH\x1bOF".to_vec());
        assert_eq!(parse_keys_mode("<Down*2><Enter>", true).unwrap(), b"\x1bOB\x1bOB\r".to_vec());
        // 其余键不受影响；普通模式仍是 CSI
        assert_eq!(parse_keys_mode("<PgDn><F5>", true).unwrap(), parse_keys("<PgDn><F5>").unwrap());
        assert_eq!(parse_keys_mode("<Up>", false).unwrap(), b"\x1b[A".to_vec());
    }

    use super::*;

    #[test]
    fn plain_text_is_sent_verbatim_without_newline() {
        assert_eq!(parse_keys("ls -la").unwrap(), b"ls -la");
        assert_eq!(parse_keys("中文").unwrap(), "中文".as_bytes());
    }

    #[test]
    fn backslash_escapes() {
        assert_eq!(parse_keys(r"a\rb\nc\td\ee\\f").unwrap(), b"a\rb\nc\td\x1be\\f");
        assert_eq!(parse_keys(r"\x03\x1B\xff").unwrap(), vec![0x03, 0x1b, 0xff]);
    }

    #[test]
    fn bad_escapes_are_rejected() {
        assert!(parse_keys(r"\q").is_err());
        assert!(parse_keys(r"abc\").is_err());
        assert!(parse_keys(r"\x0").is_err());
        assert!(parse_keys(r"\xzz").is_err());
    }

    #[test]
    fn named_keys() {
        assert_eq!(parse_keys("<C-c>").unwrap(), vec![0x03]);
        assert_eq!(parse_keys("<c-D>").unwrap(), vec![0x04]);
        assert_eq!(parse_keys("<Esc>:wq<Enter>").unwrap(), b"\x1b:wq\r");
        assert_eq!(parse_keys("<Up><Down><Tab>").unwrap(), b"\x1b[A\x1b[B\t");
        assert_eq!(parse_keys("<M-b>").unwrap(), b"\x1bb");
        assert_eq!(parse_keys("<F5>").unwrap(), b"\x1b[15~");
        assert_eq!(parse_keys("<C-@>").unwrap(), vec![0]);
        assert_eq!(parse_keys("1<lt>2").unwrap(), b"1<2");
    }

    #[test]
    fn unknown_named_key_is_an_error_but_bare_angle_is_literal() {
        assert!(parse_keys("<C-cc>").is_err());
        assert!(parse_keys("<Foo>").is_err());
        assert_eq!(parse_keys("a < b").unwrap(), b"a < b");
        assert_eq!(parse_keys("cat <<EOF").unwrap(), b"cat <<EOF");
        assert_eq!(parse_keys("<>").unwrap(), b"<>");
        assert_eq!(parse_keys("x<").unwrap(), b"x<");
    }

    #[test]
    fn size_limit_and_empty() {
        assert!(parse_keys("").is_err());
        assert!(parse_keys(&"a".repeat(MAX_KEYS_BYTES)).is_ok());
        assert!(parse_keys(&"a".repeat(MAX_KEYS_BYTES + 1)).is_err());
        // 转义展开后才超限也要拦
        assert!(parse_keys(&"<Up>".repeat(1400)).is_err());
    }

    #[test]
    fn enter_detection() {
        assert!(contains_enter(b"ls\r"));
        assert!(contains_enter(b"a\nb"));
        assert!(!contains_enter(b"\x03"));
        assert!(!contains_enter(b"\x1b[A"));
    }

    #[test]
    fn display_is_human_readable() {
        assert_eq!(display_keys(b"\x03"), "<C-c>");
        assert_eq!(display_keys(b"ls\r"), "ls<Enter>");
        assert_eq!(display_keys(b"\x1b:q"), "<Esc>:q");
    }

    #[test]
    fn pattern_parsing() {
        assert!(matches!(Pattern::parse("login:").unwrap(), Pattern::Substring(_)));
        assert!(matches!(Pattern::parse("/err(or)?/").unwrap(), Pattern::Regex(_)));
        assert!(Pattern::parse("/ERR/i").unwrap().is_match("an error"));
        assert!(!Pattern::parse("/ERR/").unwrap().is_match("an error"));
        assert!(Pattern::parse("/(/").is_err());
        assert!(Pattern::parse("").is_err());
        // 路径不是正则：没有第二个斜杠闭合成「正则 + 合法标志」
        assert!(matches!(Pattern::parse("/var/log").unwrap(), Pattern::Substring(_)));
    }

    #[test]
    fn spec_clamps_and_defaults() {
        let s = WaitSpec::from_args(None, Some(999_999), None);
        assert_eq!(s.timeout_ms, WAIT_MAX_TIMEOUT_MS);
        assert_eq!(s.quiet_ms, Some(WAIT_DEFAULT_QUIET_MS));
        let p = Pattern::parse("x").unwrap();
        let s = WaitSpec::from_args(Some(p.clone()), None, None);
        assert_eq!(s.quiet_ms, None);
        assert_eq!(s.timeout_ms, WAIT_DEFAULT_TIMEOUT_MS);
        let s = WaitSpec::from_args(Some(p), Some(1), Some(1));
        assert_eq!(s.quiet_ms, Some(WAIT_MIN_QUIET_MS));
        assert_eq!(s.timeout_ms, 100);
    }

    fn spec(p: Option<&str>, quiet: Option<u64>, timeout: u64) -> WaitSpec {
        WaitSpec {
            pattern: p.map(|p| Pattern::parse(p).unwrap()),
            quiet_ms: quiet,
            timeout_ms: timeout,
            poll_ms: 50,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn matches_pattern_in_new_output() {
        let started = Instant::now();
        let r = wait_for_pattern(
            &spec(Some("Password:"), None, 10_000),
            || (started.elapsed().as_millis() / 100) as u64,
            || if started.elapsed().as_millis() >= 500 { "Password: ".into() } else { "...".to_string() },
            || false,
        )
        .await;
        assert_eq!(r.end, WaitEnd::Matched);
        assert!(r.output.contains("Password:"));
    }

    #[tokio::test(start_paused = true)]
    async fn already_present_matches_immediately() {
        let started = Instant::now();
        let r = wait_for_pattern(&spec(Some("ok"), None, 10_000), || 1, || "all ok".into(), || false).await;
        assert_eq!(r.end, WaitEnd::Matched);
        assert!(started.elapsed() < Duration::from_millis(10));
    }

    #[tokio::test(start_paused = true)]
    async fn returns_on_quiet() {
        let r = wait_for_pattern(&spec(None, Some(300), 10_000), || 5, || "done".into(), || false).await;
        assert_eq!(r.end, WaitEnd::Quiet);
        assert_eq!(r.output, "done");
    }

    #[tokio::test(start_paused = true)]
    async fn steady_output_defers_quiet_until_timeout() {
        let started = Instant::now();
        let r = wait_for_pattern(
            &spec(Some("never"), Some(300), 2_000),
            || started.elapsed().as_millis() as u64,
            || "spam".into(),
            || false,
        )
        .await;
        assert_eq!(r.end, WaitEnd::Timeout);
    }

    #[tokio::test(start_paused = true)]
    async fn pattern_only_waits_through_silence_until_timeout() {
        let r = wait_for_pattern(&spec(Some("never"), None, 1_000), || 0, String::new, || false).await;
        assert_eq!(r.end, WaitEnd::Timeout);
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_wins() {
        let r = wait_for_pattern(&spec(Some("never"), None, 10_000), || 0, String::new, || true).await;
        assert_eq!(r.end, WaitEnd::Cancelled);
    }
}
