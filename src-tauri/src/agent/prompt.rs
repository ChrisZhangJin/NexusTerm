//! 交互提示分类器：看一眼终端屏幕尾部，判断「现在终端在等什么」。
//!
//! agent 敲完命令后最尴尬的局面是：命令没结束，也没报错，只是停在某个提示上
//! —— 在等密码、等 `y/n`、卡在 `less` 里、停在 Python REPL。
//! 傻等到超时既浪费时间，又让模型对着一片沉默瞎猜。这个模块把「屏幕尾部」
//! 归类成 [`PromptKind`]，并给出一句中文的应答建议（[`PromptGuess::hint`]），
//! 让上层（`nx_exec` / `nx_wait` / `nx_screen`）能提前返回、并告诉模型下一步怎么办。
//!
//! ## 设计取舍
//!
//! - **纯函数，无状态，无 IO**。只依赖 `buffer::strip_ansi` 与 `regex`（项目已有）。
//! - **只看最后一行**（菜单另外回看前面的编号列表、全屏对话框看整屏）。
//!   长输出中间出现 `password` 这类字眼不算提示：真正在等输入的程序，
//!   光标一定停在最后一行。
//! - **优先级**：密码 > 是/否 > 分页器 > 菜单 > REPL > shell 提示符。
//!   shell 提示符只有在它就是最后一个非空行时才成立；
//!   所以「上一行是提示符、最后一行是 `[sudo] password for x:`」判密码。
//! - **拿不准就返回 `Unknown` + 低置信度**，绝不 panic，
//!   超长输入只看末尾 [`TAIL_BYTES`] 字节。

use std::sync::OnceLock;

use regex::Regex;
use serde::Serialize;

use super::buffer::strip_ansi;

/// 只分析输入末尾这么多字节：提示一定在尾部，没必要扫整屏历史。
const TAIL_BYTES: usize = 4096;
/// 最后一行最多保留末尾这么多字符参与匹配（提示都很短；
/// 被 `\r` 拼接起来的进度条可能是一行几千字符）。
const MAX_LINE_CHARS: usize = 240;
/// 菜单检测回看的行数（编号列表通常紧贴在选择提示之上）。
const MENU_LOOKBACK: usize = 25;
/// 全屏对话框 / 菜单检测回看的行数。
const DIALOG_LOOKBACK: usize = 24;

/// 终端当前在等什么。序列化为 snake_case，直接给前端 / 模型用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptKind {
    /// 密码 / 口令 / 验证码：必须由用户本人输入。
    Password,
    /// 是/否确认。
    YesNo,
    /// 分页器（less / more / man / git log …）或「按任意键继续」。
    Pager,
    /// 编号菜单或全屏选择对话框。
    Menu,
    /// 交互式解释器 / 数据库客户端 / 调试器。
    Repl,
    /// 普通 shell 提示符：上一条命令已结束。
    ShellPrompt,
    /// 看不出来。
    Unknown,
}

impl PromptKind {
    /// 与 serde 序列化一致的 snake_case 名字（喂给模型的 `prompt_kind` 字段用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::YesNo => "yes_no",
            Self::Pager => "pager",
            Self::Menu => "menu",
            Self::Repl => "repl",
            Self::ShellPrompt => "shell_prompt",
            Self::Unknown => "unknown",
        }
    }
}

/// 分类结果。
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PromptGuess {
    pub kind: PromptKind,
    /// 0.0 ~ 1.0。粗略档位：>= 0.85 基本确定，0.5 ~ 0.85 大概率，< 0.5 仅供参考。
    pub confidence: f32,
    /// 给模型看的简短中文建议（怎么应答）。
    pub hint: &'static str,
}

impl PromptGuess {
    /// 喂给模型的一行反馈：`prompt_kind=… confidence=0.90 提示：…`，以换行结尾。
    /// 工具结果（exec / wait / screen / send_keys / secret）共用这一行，格式只此一处。
    pub fn feedback(&self) -> String {
        format!(
            "prompt_kind={} confidence={:.2} 提示：{}\n",
            self.kind.as_str(),
            self.confidence,
            self.hint
        )
    }
}

const HINT_PASSWORD: &str =
    "这是密码/口令/验证码输入提示：不要猜测或编造，**不要用 nx_send_keys / nx_exec 输入**；请调用 nx_secret 让用户本人输入（输入内容不会经过你，也不会回显）。";
const HINT_YESNO: &str =
    "这是是/否确认提示：先核对操作风险；同意发送 y（或 yes）加回车，拒绝发送 n。ssh 首次连接的指纹确认必须回复完整的 yes。";
const HINT_PAGER: &str =
    "终端停在分页器/等待按键：q 退出，空格翻页，回车逐行下翻；按任意键提示发送回车或空格。以后可加 --no-pager 或 | cat 避免。";
const HINT_MENU: &str =
    "这是菜单/选择提示：发送选项编号（或字母）加回车；全屏对话框（whiptail/dialog）用方向键、空格、Tab、回车操作。";
const HINT_REPL: &str =
    "终端处于交互式解释器/数据库/调试器中，命令尚未回到 shell：输入该程序的语句；要退出请用 exit / quit() / \\q / Ctrl-D（视程序而定）。";
const HINT_SHELL: &str = "已回到 shell 提示符，上一条命令已经结束，可以输入下一条命令。";
const HINT_UNKNOWN: &str =
    "无法判断终端是否在等待输入：可再等一会儿，或读取完整屏幕再决定；不要盲目发送按键。";
const HINT_UNKNOWN_ALT: &str =
    "终端处于全屏程序（如 vim / htop / tmux）中，无法判断具体在等什么：读取完整屏幕再决定；通常用 q / Esc / :q 退出。";
const HINT_EMPTY: &str = "屏幕尾部为空：程序可能还没有输出，可再等一会儿。";

fn guess(kind: PromptKind, confidence: f32, hint: &'static str) -> PromptGuess {
    PromptGuess {
        kind,
        confidence,
        hint,
    }
}

/// 把一个正则编译一次并缓存。pattern 写错会在单测里第一时间 panic。
macro_rules! re {
    ($pat:expr) => {{
        static CELL: OnceLock<Regex> = OnceLock::new();
        CELL.get_or_init(|| Regex::new($pat).expect("prompt.rs: 非法正则"))
    }};
}

/// 对屏幕/输出尾部分类。
///
/// - `tail`：最近 ~20 行的屏幕或输出。带不带 ANSI 都行（内部会防御性地再剥一遍）。
/// - `alt_screen`：终端当前是否在备用屏幕（vim / less / tmux 等全屏程序）。
pub fn classify(tail: &str, alt_screen: bool) -> PromptGuess {
    let window = last_bytes(tail, TAIL_BYTES);
    let clean = strip_ansi(window);

    let lines: Vec<&str> = clean.lines().collect();
    // 末尾的空行（光标在新的一行但还没内容）不算
    let Some(last_idx) = lines.iter().rposition(|l| !l.trim().is_empty()) else {
        return guess(PromptKind::Unknown, 0.0, HINT_EMPTY);
    };
    let last_full = lines[last_idx].trim_end();
    let last = last_chars(last_full, MAX_LINE_CHARS);
    let lower = last.to_lowercase();
    let before = &lines[..last_idx];

    if let Some(g) = check_password(last, &lower) {
        return g;
    }
    if let Some(g) = check_yes_no(last, &lower) {
        return g;
    }
    if let Some(g) = check_pager(&lower, alt_screen) {
        return g;
    }
    if let Some(g) = check_menu(last, &lower, before, alt_screen) {
        return g;
    }
    if let Some(g) = check_repl(last) {
        return g;
    }
    if let Some(g) = check_shell(last, alt_screen) {
        return g;
    }

    if alt_screen {
        guess(PromptKind::Unknown, 0.2, HINT_UNKNOWN_ALT)
    } else {
        guess(PromptKind::Unknown, 0.1, HINT_UNKNOWN)
    }
}

// ───────────────────────────── 小工具 ─────────────────────────────

/// 取最后至多 `n` 字节，并对齐到 UTF-8 字符边界。
fn last_bytes(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let mut start = s.len() - n;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// 取最后至多 `n` 个字符。
fn last_chars(s: &str, n: usize) -> &str {
    let count = s.chars().count();
    if count <= n {
        return s;
    }
    let skip = count - n;
    let idx = s.char_indices().nth(skip).map(|(i, _)| i).unwrap_or(0);
    &s[idx..]
}

fn char_count(s: &str) -> usize {
    s.chars().count()
}

/// 以「标点 / 空白」收尾，而不是字母数字：用来判断关键词是不是一个独立的词。
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// 找 `pat` 的第一个「前后都不是词字符」的出现，返回 (起, 止) 字节下标。
fn find_bounded(hay: &str, pat: &str) -> Option<(usize, usize)> {
    for (start, _) in hay.match_indices(pat) {
        let end = start + pat.len();
        let before_ok = hay[..start].chars().next_back().map_or(true, |c| !is_word_char(c));
        let after_ok = hay[end..].chars().next().map_or(true, |c| !is_word_char(c));
        if before_ok && after_ok {
            return Some((start, end));
        }
    }
    None
}

fn has_word(hay: &str, pat: &str) -> bool {
    find_bounded(hay, pat).is_some()
}

/// 去掉末尾的空白与「？ ?」之类，用于判断一句话是不是以某个词收尾。
fn ends_question(l: &str) -> bool {
    let t = l.trim_end();
    t.ends_with('?') || t.ends_with('？')
}

fn ends_colon(l: &str) -> bool {
    let t = l.trim_end();
    t.ends_with(':') || t.ends_with('：')
}

// ───────────────────────────── 密码 ─────────────────────────────

fn check_password(line: &str, lower: &str) -> Option<PromptGuess> {
    // 密码提示一定以冒号结尾（`Password:` / `密码：`），后面是用户要输的内容。
    // 冒号后面还有内容（`Password: ****`、`Last password change: ...`）说明不是在等输入。
    if !ends_colon(lower) || char_count(lower) > 160 {
        return None;
    }

    const EN_WORDS: [&str; 7] = [
        "password",
        "passphrase",
        "passcode",
        "verification code",
        "security code",
        "authentication code",
        "auth code",
    ];
    const EN_BOUNDED: [&str; 6] = ["pin", "otp", "mfa", "2fa", "token", "one-time code"];
    const ZH_WORDS: [&str; 9] = [
        "密码", "口令", "验证码", "验证代码", "动态码", "动态口令", "安全码", "令牌", "密钥短语",
    ];

    let hit = EN_WORDS.iter().any(|w| lower.contains(w))
        || EN_BOUNDED.iter().any(|w| has_word(lower, w))
        || ZH_WORDS.iter().any(|w| lower.contains(w));
    if !hit {
        return None;
    }

    // `token` / `pin` 这类词太常见（`Your token:` 没问题，但 `Username for token:` …）：
    // 只有行很短时才信；其余强关键词不受此限。
    let strong = EN_WORDS.iter().any(|w| lower.contains(w)) || ZH_WORDS.iter().any(|w| lower.contains(w));
    if !strong && char_count(lower) > 60 {
        return None;
    }

    // 行首缩进：更像 `cat config.yml` 吐出来的 `  password:` 键，降低置信度
    let indented = line.starts_with("  ") || line.starts_with('\t');
    let conf = if indented { 0.5 } else { 0.95 };
    Some(guess(PromptKind::Password, conf, HINT_PASSWORD))
}

// ───────────────────────────── 是 / 否 ─────────────────────────────

fn check_yes_no(line: &str, lower: &str) -> Option<PromptGuess> {
    if char_count(lower) > 200 {
        return None;
    }
    if yes_no_token(lower) {
        return Some(guess(PromptKind::YesNo, 0.92, HINT_YESNO));
    }
    if yes_no_weak(line, lower) {
        return Some(guess(PromptKind::YesNo, 0.75, HINT_YESNO));
    }
    None
}

/// 明确的 `y/n` 记号：`(y/n)` `[Y/n]` `(yes/no/[fingerprint])` `[是/否]` …
fn yes_no_token(l: &str) -> bool {
    if l.contains("是/否") || l.contains("是或否") || l.contains("是|否") {
        return true;
    }
    if l.contains("[y]es") && l.contains("[n]o") {
        return true;
    }
    for pat in ["y/n", "yes/no", "y|n", "yes|no", "y or n", "yes or no"] {
        if let Some((_, end)) = find_bounded(l, pat) {
            // 记号后面不应再跟一大段话（否则更像是正文里提到了 y/n）
            if char_count(&l[end..]) <= 50 {
                return true;
            }
        }
    }
    false
}

/// 没有 y/n 记号的确认句：`Continue?` `Proceed?` `是否继续` …
fn yes_no_weak(line: &str, lower: &str) -> bool {
    if char_count(lower) > 120 {
        return false;
    }
    let _ = line;
    if ends_question(lower) {
        const EN: [&str; 9] = [
            "continue",
            "proceed",
            "are you sure",
            "do you want",
            "would you like",
            "overwrite",
            "confirm",
            "do you wish",
            "really",
        ];
        const ZH: [&str; 5] = ["继续", "确认", "确定", "是否", "覆盖"];
        if EN.iter().any(|w| has_word(lower, w)) || ZH.iter().any(|w| lower.contains(w)) {
            return true;
        }
    }
    if lower.contains("是否") {
        let t = lower.trim_end();
        const ENDS: [&str; 10] = ["?", "？", ":", "：", "吗", ")", "）", "]", "】", "。"];
        const WORDS: [&str; 6] = ["继续", "确认", "覆盖", "删除", "执行", "退出"];
        if ENDS.iter().any(|e| t.ends_with(e)) || WORDS.iter().any(|w| t.ends_with(w)) {
            return true;
        }
    }
    false
}

// ───────────────────────────── 分页器 ─────────────────────────────

fn check_pager(lower: &str, alt_screen: bool) -> Option<PromptGuess> {
    let t = lower.trim();
    if char_count(t) > 160 {
        return None;
    }
    if t.contains("--more--") || t.contains("-- more --") {
        return Some(guess(PromptKind::Pager, 0.95, HINT_PAGER));
    }
    if t.ends_with("(end)") {
        return Some(guess(PromptKind::Pager, 0.95, HINT_PAGER));
    }
    // less 的状态行：`lines 1-23/100`、`lines 1-23/100 50%`
    if re!(r"^(?:.{0,60}\s)?lines \d+-\d+(?:/\d+)?(?:\s.*)?$").is_match(t) && char_count(t) <= 80 {
        return Some(guess(PromptKind::Pager, 0.9, HINT_PAGER));
    }
    // man 页底部：`Manual page ls(1) line 1/200 (press h for help or q to quit)`
    if t.contains("press h for help") || t.contains("press q to quit") || t.contains("q to quit") {
        return Some(guess(PromptKind::Pager, 0.85, HINT_PAGER));
    }
    const KEYS: [&str; 11] = [
        "press any key",
        "hit any key",
        "any key to continue",
        "press enter to continue",
        "press return to continue",
        "press [enter]",
        "press space to continue",
        "press <enter>",
        "按任意键",
        "按回车键继续",
        "按回车继续",
    ];
    if KEYS.iter().any(|k| t.contains(k)) {
        return Some(guess(PromptKind::Pager, 0.85, HINT_PAGER));
    }
    // less 在备用屏幕里的裸 `:` 提示
    if alt_screen && t == ":" {
        return Some(guess(PromptKind::Pager, 0.75, HINT_PAGER));
    }
    None
}

/// 主屏幕上的分页器：git 默认 `LESS=FRX`，less 不进备用屏幕，最后一行只剩一个 `:`。
///
/// 裸 `:` 在主屏幕上太常见（脚本输出、YAML 片段），所以要同时满足：
/// - 屏幕已经静止一阵（`static_screen`，由调用方按帧计数判断）；
/// - 上面至少 5 行内容，且紧挨着的那行不像一句等输入的提示（不以 `:` / `?` 结尾、不是 shell 提示符）。
pub fn normal_screen_pager(before: &[&str], last: &str, static_screen: bool) -> Option<PromptGuess> {
    if !static_screen || last.trim() != ":" {
        return None;
    }
    let content: Vec<&str> = before.iter().copied().filter(|l| !l.trim().is_empty()).collect();
    if content.len() < 5 {
        return None;
    }
    let prev = content.last()?.trim_end();
    if ends_colon(prev) || ends_question(prev) || check_shell(prev, false).is_some() {
        return None;
    }
    Some(guess(PromptKind::Pager, 0.7, HINT_PAGER))
}

// ───────────────────────────── 菜单 ─────────────────────────────

/// 形如 `1) foo` `2. bar` `[3] baz` `(4) qux` `5: x` `> 1 - y` 的编号项。
fn is_numbered_item(line: &str) -> bool {
    re!(r"^\s*[>*]?\s*[\[(]?\d{1,3}[\]).:\-]\s+\S").is_match(line)
}

fn check_menu(
    last: &str,
    lower: &str,
    before: &[&str],
    alt_screen: bool,
) -> Option<PromptGuess> {
    let t = lower.trim();
    let items = before
        .iter()
        .rev()
        .take(MENU_LOOKBACK)
        .filter(|l| is_numbered_item(l))
        .count();

    const KEYWORDS: [&str; 17] = [
        "select",
        "choose",
        "choice",
        "enter your",
        "your selection",
        "which one",
        "pick",
        "option",
        "number",
        "enter selection",
        "请选择",
        "请输入",
        "选择",
        "选项",
        "序号",
        "编号",
        "请选",
    ];
    let has_kw = KEYWORDS.iter().any(|k| t.contains(k));
    let ends_prompt = ["：", ":", "?", "？", ">", "]", ")", "）"]
        .iter()
        .any(|e| t.ends_with(e));
    let short = char_count(t) <= 100;

    // bash `select` 的 `#? ` 提示
    if t == "#?" && items >= 2 {
        return Some(guess(PromptKind::Menu, 0.9, HINT_MENU));
    }
    if t == "#?" {
        return Some(guess(PromptKind::Menu, 0.55, HINT_MENU));
    }
    if short && items >= 2 && ends_prompt && has_kw {
        return Some(guess(PromptKind::Menu, 0.9, HINT_MENU));
    }
    // `Choose [1-3]:`、`Enter choice (1-5):` —— 编号列表可能已经滚出尾部
    let has_range = re!(r"[\[(]\s*\d+\s*-\s*\d+\s*[\])]").is_match(t)
        || re!(r"[\[(]\s*\d+\s*(?:/\s*\d+\s*)+[\])]").is_match(t);
    if short && has_range && ends_prompt {
        let conf = if has_kw { 0.8 } else { 0.55 };
        return Some(guess(PromptKind::Menu, conf, HINT_MENU));
    }
    if short && ends_colon(t) {
        const EXPLICIT: [&str; 7] = [
            "enter choice",
            "enter your choice",
            "your choice",
            "enter selection",
            "请选择",
            "请输入选项",
            "请输入序号",
        ];
        if EXPLICIT.iter().any(|k| t.contains(k)) {
            return Some(guess(PromptKind::Menu, 0.7, HINT_MENU));
        }
    }
    if short && items >= 3 && ends_prompt {
        return Some(guess(PromptKind::Menu, 0.5, HINT_MENU));
    }

    if alt_screen {
        // whiptail / dialog：底部按钮
        let dialog_hit = before
            .iter()
            .rev()
            .take(DIALOG_LOOKBACK)
            .copied()
            .chain(std::iter::once(last))
            .any(|l| {
                let l = l.to_lowercase();
                [
                    "<ok>", "<cancel>", "<yes>", "<no>", "<select>", "<back>", "<exit>", "< ok >",
                    "< cancel >", "< yes >", "< no >", "<help>",
                ]
                .iter()
                .any(|b| l.contains(b))
            });
        if dialog_hit {
            return Some(guess(PromptKind::Menu, 0.85, HINT_MENU));
        }
        // ncurses 单选 / 复选列表：`( ) foo` `(*) bar` `[ ] x` `[*] y`
        let radios = before
            .iter()
            .rev()
            .take(DIALOG_LOOKBACK)
            .copied()
            .chain(std::iter::once(last))
            .filter(|l| re!(r"^[\s│|]*(?:\(.\)|\[.\])\s+\S").is_match(l))
            .count();
        if radios >= 2 {
            return Some(guess(PromptKind::Menu, 0.5, HINT_MENU));
        }
    }
    None
}

// ───────────────────────────── REPL ─────────────────────────────

fn check_repl(line: &str) -> Option<PromptGuess> {
    let t = line.trim();
    if t.is_empty() || char_count(t) > 80 {
        return None;
    }

    // 极其明确的提示符
    let strong = re!(
        r"(?i)^(?:>>>|>>|\(?(?:gdb|lldb|pdb|ipdb|cmd|rr|dlv)\)|(?:ipdb|pdb|pwndbg)>|gef➤|in \[\d+\]:|octave:\d+>|(?:mysql|sqlite|sql|ftp|sftp|lftp|telnet|nslookup|sqlplus|scala|jshell|ghci|prelude|julia|lua|clj|utop|guile|racket|php|mongo|mongosh|cqlsh|gnuplot|octave|matlab|node|python|ruby|redis-cli|psql|irb|iex)\s?>|mariadb(?:\s*\[[^\]]*\])?>|sbt(?::[^>\s]+)?>|irb(?:\([^)]*\))?(?::\d+)*[>*\x22'`]|iex(?:\(\d+\))?>|(?:\[\d+\]\s*)?pry\(.*\)>|\d+\.\d+\.\d+\s+:\d+\s+>)$"
    )
    .is_match(t);
    if strong {
        return Some(guess(PromptKind::Repl, 0.9, HINT_REPL));
    }

    // postgres：`postgres=# ` `mydb=> ` 以及续行 `postgres-# ` `postgres(# `
    if re!(r"^[\w.\-]+(?:=|-|\()[#>]$").is_match(t) {
        return Some(guess(PromptKind::Repl, 0.88, HINT_REPL));
    }
    // redis：`127.0.0.1:6379>` `127.0.0.1:6379[1]>` `[::1]:6379>`
    if re!(r"^[\[\]\w.:\-]+:\d+(?:\[\d+\])?>$").is_match(t) {
        return Some(guess(PromptKind::Repl, 0.88, HINT_REPL));
    }
    // ipython 续行 `   ...: `
    if re!(r"^\.{3}:$").is_match(t) {
        return Some(guess(PromptKind::Repl, 0.85, HINT_REPL));
    }
    // python 续行 / sqlite 续行 / mysql 续行
    if t == "..." || t == "...>" || t == "->" {
        return Some(guess(PromptKind::Repl, 0.6, HINT_REPL));
    }
    // 裸 `>`：node / R 的提示符，也可能是 shell 的 PS2 续行
    if t == ">" {
        return Some(guess(PromptKind::Repl, 0.4, HINT_REPL));
    }
    None
}

// ───────────────────────────── shell 提示符 ─────────────────────────────

fn check_shell(line: &str, alt_screen: bool) -> Option<PromptGuess> {
    let t = line.trim();
    if t.is_empty() || char_count(t) > 200 {
        return None;
    }
    let conf = shell_confidence(t)?;
    // 备用屏幕里看到「像提示符」的文字，多半是 vim 缓冲区或 tmux 内部：降级
    let conf = if alt_screen { conf.min(0.5) } else { conf };
    Some(guess(PromptKind::ShellPrompt, conf, HINT_SHELL))
}

fn shell_confidence(t: &str) -> Option<f32> {
    // PowerShell：`PS C:\> ` `PS C:\Users\x> ` `PS /home/x> `
    if re!(r"^PS(?:\s.*)?>$").is_match(t) && !t.contains("PS>>") {
        return Some(0.95);
    }
    // cmd.exe：`C:\> ` `C:\Users\x>`
    if re!(r#"^[A-Za-z]:\\[^<>"|?*]*>$"#).is_match(t) {
        return Some(0.95);
    }
    // 箭头类提示符：starship / pure / oh-my-zsh
    if t.ends_with('❯') || t.ends_with('λ') || t.ends_with('»') {
        return Some(0.88);
    }
    if t.starts_with('➜') {
        return Some(0.85);
    }
    // fish：`user@host ~> `
    if t.ends_with('>') && t.contains('@') && !t.contains('<') && !t.contains(" >") {
        return Some(0.8);
    }

    let last = t.chars().next_back()?;
    if !matches!(last, '$' | '#' | '%') {
        return None;
    }
    let body = &t[..t.len() - 1];
    // 裸 `$` / `#` / `%`
    if body.trim().is_empty() {
        return Some(0.85);
    }
    // 进度条与百分比：`100%` `[####]#` `####`
    let prev = body.chars().next_back()?;
    if last == '%' && prev.is_ascii_digit() {
        return None;
    }
    if last == '#' && (prev == '#' || prev == '=' || prev == '-') {
        return None;
    }
    if last == '$' && prev == '$' {
        return None;
    }

    let has_at = body.contains('@');
    // user@host:~$  user@host ~ %  root@x:/#  (venv) me@h:~/p$
    if has_at && (body.contains(':') || body.contains('~') || body.contains('/') || body.contains(' ') || body.contains(']')) {
        return Some(0.95);
    }
    // [user@host ~]$
    if re!(r"^\[.*\]\s?$").is_match(body) {
        return Some(0.95);
    }
    // bash-5.1$  sh-4.4#  -bash-4.2$  zsh%
    if re!(r"^-?(?:bash|sh|zsh|ksh|dash|ash|fish|csh|tcsh)[-\d.]*$").is_match(body) {
        return Some(0.9);
    }
    // busybox / 极简提示符：`/ # ` `~ $ ` `~/proj %`
    if re!(r"^[~/]\S*\s$").is_match(body) {
        return Some(0.85);
    }
    // 其它：路径或括号收尾后接符号 —— 稍弱
    if matches!(prev, '~' | '/' | ']' | ')' | '─') {
        return Some(0.7);
    }
    if has_at && last == '$' {
        return Some(0.6);
    }
    None
}

// ───────────────────────────── 测试 ─────────────────────────────

#[cfg(test)]
mod tests {
    #[test]
    fn as_str_matches_serde_names_and_feedback_line() {
        for k in [
            PromptKind::Password,
            PromptKind::YesNo,
            PromptKind::Pager,
            PromptKind::Menu,
            PromptKind::Repl,
            PromptKind::ShellPrompt,
            PromptKind::Unknown,
        ] {
            let json = serde_json::to_string(&k).unwrap();
            assert_eq!(json, format!("\"{}\"", k.as_str()));
        }
        let g = classify("[sudo] password for chris:", false);
        let fb = g.feedback();
        assert!(fb.starts_with("prompt_kind=password confidence="));
        assert!(fb.contains("nx_secret"));
        assert!(fb.ends_with('\n'));
    }

    use super::*;

    fn k(s: &str) -> PromptKind {
        classify(s, false).kind
    }
    fn ka(s: &str) -> PromptKind {
        classify(s, true).kind
    }

    // ——— 密码 ———

    #[test]
    fn password_plain() {
        assert_eq!(k("Password:"), PromptKind::Password);
        assert_eq!(k("Password: "), PromptKind::Password);
        assert_eq!(k("password:"), PromptKind::Password);
        assert_eq!(k("PASSWORD:"), PromptKind::Password);
    }

    #[test]
    fn password_sudo() {
        assert_eq!(k("[sudo] password for alice: "), PromptKind::Password);
        assert_eq!(k("[sudo] alice 的密码："), PromptKind::Password);
    }

    #[test]
    fn password_ssh_host() {
        assert_eq!(k("alice@10.0.0.5's password: "), PromptKind::Password);
        assert_eq!(k("(alice@host) Password: "), PromptKind::Password);
    }

    #[test]
    fn password_passphrase() {
        assert_eq!(
            k("Enter passphrase for key '/home/a/.ssh/id_ed25519': "),
            PromptKind::Password
        );
        assert_eq!(
            k("Enter passphrase (empty for no passphrase): "),
            PromptKind::Password
        );
        assert_eq!(k("Enter same passphrase again: "), PromptKind::Password);
    }

    #[test]
    fn password_chinese() {
        assert_eq!(k("密码："), PromptKind::Password);
        assert_eq!(k("请输入密码: "), PromptKind::Password);
        assert_eq!(k("口令："), PromptKind::Password);
    }

    #[test]
    fn password_otp_and_codes() {
        assert_eq!(k("Verification code: "), PromptKind::Password);
        assert_eq!(k("Enter OTP: "), PromptKind::Password);
        assert_eq!(k("One-time password (OTP): "), PromptKind::Password);
        assert_eq!(k("验证码："), PromptKind::Password);
        assert_eq!(k("请输入动态口令："), PromptKind::Password);
        assert_eq!(k("Enter MFA code: "), PromptKind::Password);
        assert_eq!(k("Duo passcode or option (1-3): "), PromptKind::Password);
        assert_eq!(k("Enter PIN: "), PromptKind::Password);
    }

    #[test]
    fn password_confidence_high() {
        let g = classify("Password: ", false);
        assert!(g.confidence >= 0.9);
        assert!(!g.hint.is_empty());
    }

    #[test]
    fn password_beats_earlier_shell_prompt() {
        assert_eq!(
            k("alice@host:~$ sudo ls\n[sudo] password for alice: "),
            PromptKind::Password
        );
    }

    #[test]
    fn password_mysql() {
        assert_eq!(k("Enter password: "), PromptKind::Password);
    }

    #[test]
    fn password_retry() {
        assert_eq!(
            k("Sorry, try again.\n[sudo] password for alice: "),
            PromptKind::Password
        );
    }

    #[test]
    fn password_mid_output_is_not_prompt() {
        let out = "To change the password: edit the file\nand restart the service\nDone.\nbuild ok";
        assert_ne!(k(out), PromptKind::Password);
    }

    #[test]
    fn password_with_value_after_colon_is_not_prompt() {
        assert_ne!(k("Password: ********"), PromptKind::Password);
        assert_ne!(k("Last password change : Nov 01, 2026"), PromptKind::Password);
        assert_ne!(k("password updated successfully"), PromptKind::Password);
    }

    #[test]
    fn password_word_without_colon_is_not_prompt() {
        assert_ne!(k("Changing password for alice."), PromptKind::Password);
        assert_ne!(k("wrong password"), PromptKind::Password);
    }

    #[test]
    fn password_then_prompt_returns_shell() {
        // 密码提示之后又回到了提示符：最后一行说了算
        assert_eq!(
            k("[sudo] password for alice: \nalice@host:~$ "),
            PromptKind::ShellPrompt
        );
    }

    #[test]
    fn password_yaml_key_is_weak() {
        let g = classify("db:\n  password:", false);
        // 缩进的 `password:` 可能是 yaml 键：置信度必须明显降低
        assert!(g.confidence < 0.6);
    }

    // ——— 是 / 否 ———

    #[test]
    fn yesno_tokens() {
        assert_eq!(k("Do you want to continue? [Y/n] "), PromptKind::YesNo);
        assert_eq!(k("Overwrite file? [y/N] "), PromptKind::YesNo);
        assert_eq!(k("Proceed (y/n)? "), PromptKind::YesNo);
        assert_eq!(k("Remove all files (y/n) "), PromptKind::YesNo);
        assert_eq!(k("Are you sure? (yes/no) "), PromptKind::YesNo);
        assert_eq!(k("Install? [yes/no]: "), PromptKind::YesNo);
        assert_eq!(k("Is this ok [y/N]:"), PromptKind::YesNo);
        assert_eq!(k("Save changes? (y or n)"), PromptKind::YesNo);
    }

    #[test]
    fn yesno_ssh_fingerprint() {
        let tail = "The authenticity of host '10.0.0.5 (10.0.0.5)' can't be established.\n\
                    ED25519 key fingerprint is SHA256:abcdef.\n\
                    This key is not known by any other names.\n\
                    Are you sure you want to continue connecting (yes/no/[fingerprint])? ";
        let g = classify(tail, false);
        assert_eq!(g.kind, PromptKind::YesNo);
        assert!(g.confidence >= 0.9);
    }

    #[test]
    fn yesno_weak_forms() {
        assert_eq!(k("Continue? "), PromptKind::YesNo);
        assert_eq!(k("Continue?"), PromptKind::YesNo);
        assert_eq!(k("Proceed?"), PromptKind::YesNo);
        assert_eq!(k("Are you sure?"), PromptKind::YesNo);
        assert_eq!(k("Do you want to install these packages?"), PromptKind::YesNo);
    }

    #[test]
    fn yesno_chinese() {
        assert_eq!(k("是否继续？"), PromptKind::YesNo);
        assert_eq!(k("是否继续"), PromptKind::YesNo);
        assert_eq!(k("是否继续 (y/n)"), PromptKind::YesNo);
        assert_eq!(k("确认删除吗？"), PromptKind::YesNo);
        assert_eq!(k("是否覆盖已有文件 [是/否]"), PromptKind::YesNo);
        assert_eq!(k("是否确认："), PromptKind::YesNo);
    }

    #[test]
    fn yesno_rpm_style() {
        assert_eq!(
            k("Replace foo.txt? [y]es, [n]o, [A]ll, [N]one, [r]ename: "),
            PromptKind::YesNo
        );
    }

    #[test]
    fn yesno_apt_after_long_list() {
        let mut s = String::new();
        for i in 0..30 {
            s.push_str(&format!("  package-{i}\n"));
        }
        s.push_str("After this operation, 100 MB of additional disk space will be used.\n");
        s.push_str("Do you want to continue? [Y/n] ");
        assert_eq!(k(&s), PromptKind::YesNo);
    }

    #[test]
    fn yesno_beats_earlier_shell_prompt() {
        assert_eq!(
            k("alice@host:~$ rm -i foo\nrm: remove regular file 'foo'? y/n"),
            PromptKind::YesNo
        );
    }

    #[test]
    fn yesno_false_positives() {
        // 词中的 y/n 不算
        assert_ne!(k("any/none of the above"), PromptKind::YesNo);
        assert_ne!(k("key/name pairs loaded"), PromptKind::YesNo);
        // 正文里提到 continue 但不是疑问句
        assert_ne!(k("The build will continue in the background"), PromptKind::YesNo);
        // 中段出现 y/n，最后一行是普通输出
        assert_ne!(k("answer with (y/n) to proceed\nrunning step 3 of 10\ndone"), PromptKind::YesNo);
    }

    #[test]
    fn yesno_then_shell_prompt_is_shell() {
        assert_eq!(
            k("Continue? [Y/n] y\nDone.\nalice@host:~$ "),
            PromptKind::ShellPrompt
        );
    }

    // ——— 分页器 ———

    #[test]
    fn pager_more() {
        assert_eq!(k("--More--"), PromptKind::Pager);
        assert_eq!(k("--More--(45%)"), PromptKind::Pager);
        assert_eq!(k("line a\nline b\n -- More -- "), PromptKind::Pager);
    }

    #[test]
    fn pager_end() {
        assert_eq!(k("(END)"), PromptKind::Pager);
        assert_eq!(ka("some text\n(END)"), PromptKind::Pager);
        assert_eq!(k("foo.txt (END)"), PromptKind::Pager);
    }

    #[test]
    fn pager_lines_status() {
        assert_eq!(k("lines 1-23/100 "), PromptKind::Pager);
        assert_eq!(k("lines 1-23"), PromptKind::Pager);
        assert_eq!(k("lines 1-20/20 (END)"), PromptKind::Pager);
        assert_eq!(k("lines 24-46/100 24%"), PromptKind::Pager);
    }

    #[test]
    fn pager_press_any_key() {
        assert_eq!(k("Press any key to continue . . ."), PromptKind::Pager);
        assert_eq!(k("Press ENTER to continue"), PromptKind::Pager);
        assert_eq!(k("按任意键继续..."), PromptKind::Pager);
        assert_eq!(k("请按回车键继续"), PromptKind::Pager);
    }

    #[test]
    fn pager_man_status() {
        assert_eq!(
            ka("Manual page ls(1) line 1/230 (press h for help or q to quit)"),
            PromptKind::Pager
        );
    }

    #[test]
    fn pager_colon_requires_alt_screen() {
        let screen = "LS(1)   User Commands   LS(1)\n\nNAME\n   ls - list\n:";
        assert_eq!(ka(screen), PromptKind::Pager);
        assert_ne!(k(screen), PromptKind::Pager);
    }

    #[test]
    fn pager_colon_in_text_is_not_pager() {
        assert_ne!(ka("vim buffer\nnotes:"), PromptKind::Pager);
    }

    #[test]
    fn pager_word_end_in_middle_is_not_pager() {
        assert_ne!(k("(END) marker seen\nprocessing\nok"), PromptKind::Pager);
    }

    // ——— 菜单 ———

    #[test]
    fn menu_numbered_select() {
        let s = "Please choose an option:\n1) Install\n2) Update\n3) Quit\nEnter choice: ";
        assert_eq!(k(s), PromptKind::Menu);
    }

    #[test]
    fn menu_range_prompt() {
        assert_eq!(
            k("1) a\n2) b\n3) c\nChoose [1-3]: "),
            PromptKind::Menu
        );
        assert_eq!(k("Choose [1-3]: "), PromptKind::Menu);
        assert_eq!(k("Enter choice (1-5):"), PromptKind::Menu);
    }

    #[test]
    fn menu_chinese() {
        assert_eq!(
            k("1. 安装\n2. 卸载\n3. 退出\n请选择："),
            PromptKind::Menu
        );
        assert_eq!(k("请选择: "), PromptKind::Menu);
    }

    #[test]
    fn menu_bash_select() {
        assert_eq!(k("1) red\n2) green\n3) blue\n#? "), PromptKind::Menu);
    }

    #[test]
    fn menu_bracket_numbers() {
        assert_eq!(
            k("[1] first\n[2] second\n[3] third\nSelect:"),
            PromptKind::Menu
        );
    }

    #[test]
    fn menu_whiptail_alt_screen() {
        let s = "┌──────────┐\n│ Pick one │\n│  ( ) a   │\n│  (*) b   │\n└──────────┘\n   <Ok>   <Cancel>";
        assert_eq!(ka(s), PromptKind::Menu);
    }

    #[test]
    fn menu_dialog_buttons_need_alt_screen() {
        // 非备用屏幕下只有 <Ok> 字样不足以判定
        assert_ne!(k("click <Ok> to continue"), PromptKind::Menu);
    }

    #[test]
    fn menu_list_without_prompt_is_not_menu() {
        // 编号列表是普通输出，最后一行是命令输出
        let s = "Steps:\n1. clone\n2. build\n3. test\nAll done.";
        assert_ne!(k(s), PromptKind::Menu);
    }

    #[test]
    fn menu_numbered_output_then_shell() {
        let s = "1) a\n2) b\nalice@host:~$ ";
        assert_eq!(k(s), PromptKind::ShellPrompt);
    }

    // ——— REPL ———

    #[test]
    fn repl_python() {
        assert_eq!(k(">>> "), PromptKind::Repl);
        assert_eq!(k("Python 3.12\n>>>"), PromptKind::Repl);
        assert_eq!(k("... "), PromptKind::Repl);
        assert_eq!(k(">>> print(1)\n1\n>>> "), PromptKind::Repl);
    }

    #[test]
    fn repl_ipython() {
        assert_eq!(k("In [3]: "), PromptKind::Repl);
        assert_eq!(k("In [12]:"), PromptKind::Repl);
        assert_eq!(k("   ...: "), PromptKind::Repl);
    }

    #[test]
    fn repl_databases() {
        assert_eq!(k("mysql> "), PromptKind::Repl);
        assert_eq!(k("MariaDB [(none)]> "), PromptKind::Repl);
        assert_eq!(k("postgres=# "), PromptKind::Repl);
        assert_eq!(k("mydb=> "), PromptKind::Repl);
        assert_eq!(k("postgres-# "), PromptKind::Repl);
        assert_eq!(k("sqlite> "), PromptKind::Repl);
        assert_eq!(k("SQL> "), PromptKind::Repl);
        assert_eq!(k("    -> "), PromptKind::Repl);
    }

    #[test]
    fn repl_redis() {
        assert_eq!(k("127.0.0.1:6379> "), PromptKind::Repl);
        assert_eq!(k("127.0.0.1:6379[2]> "), PromptKind::Repl);
        assert_eq!(k("redis-cli> "), PromptKind::Repl);
    }

    #[test]
    fn repl_debuggers() {
        assert_eq!(k("(gdb) "), PromptKind::Repl);
        assert_eq!(k("(Pdb) "), PromptKind::Repl);
        assert_eq!(k("(lldb) "), PromptKind::Repl);
        assert_eq!(k("ipdb> "), PromptKind::Repl);
    }

    #[test]
    fn repl_ruby_node() {
        assert_eq!(k("irb(main):001:0> "), PromptKind::Repl);
        assert_eq!(k("irb(main):002> "), PromptKind::Repl);
        assert_eq!(k("node > "), PromptKind::Repl);
        assert_eq!(k("[1] pry(main)> "), PromptKind::Repl);
        assert_eq!(k("iex(1)> "), PromptKind::Repl);
    }

    #[test]
    fn repl_bare_gt_is_low_confidence() {
        let g = classify("> ", false);
        assert_eq!(g.kind, PromptKind::Repl);
        assert!(g.confidence < 0.5);
    }

    #[test]
    fn repl_ftp() {
        assert_eq!(k("ftp> "), PromptKind::Repl);
        assert_eq!(k("sftp> "), PromptKind::Repl);
    }

    #[test]
    fn repl_ellipsis_in_text_is_not_repl() {
        assert_ne!(k("Loading..."), PromptKind::Repl);
        assert_ne!(k("Waiting for the server... "), PromptKind::Repl);
    }

    #[test]
    fn repl_echoed_statement_is_not_prompt() {
        // 提示符后面已经有内容：命令在执行中，不是等输入
        assert_ne!(k(">>> import time; time.sleep(100)"), PromptKind::Repl);
    }

    #[test]
    fn repl_beats_hash_suffix() {
        // `postgres=#` 以 # 结尾，但应判 REPL 而不是 root shell
        assert_eq!(k("postgres=# "), PromptKind::Repl);
    }

    // ——— shell 提示符 ———

    #[test]
    fn shell_bash_user() {
        assert_eq!(k("alice@host:~$ "), PromptKind::ShellPrompt);
        assert_eq!(k("alice@host:~/proj/src$"), PromptKind::ShellPrompt);
        assert_eq!(k("[alice@host ~]$ "), PromptKind::ShellPrompt);
        assert_eq!(k("(venv) alice@host:~$ "), PromptKind::ShellPrompt);
        assert_eq!(k("bash-5.1$ "), PromptKind::ShellPrompt);
        assert_eq!(k("-bash-4.2$ "), PromptKind::ShellPrompt);
    }

    #[test]
    fn shell_root() {
        assert_eq!(k("root@host:/# "), PromptKind::ShellPrompt);
        assert_eq!(k("[root@host ~]# "), PromptKind::ShellPrompt);
        assert_eq!(k("sh-4.2# "), PromptKind::ShellPrompt);
        assert_eq!(k("/ # "), PromptKind::ShellPrompt);
        assert_eq!(k("# "), PromptKind::ShellPrompt);
    }

    #[test]
    fn shell_bare_sigils() {
        assert_eq!(k("$ "), PromptKind::ShellPrompt);
        assert_eq!(k("% "), PromptKind::ShellPrompt);
        assert_eq!(k("$"), PromptKind::ShellPrompt);
    }

    #[test]
    fn shell_zsh() {
        assert_eq!(k("alice@host ~ % "), PromptKind::ShellPrompt);
    }

    #[test]
    fn shell_windows() {
        assert_eq!(k("PS C:\\> "), PromptKind::ShellPrompt);
        assert_eq!(k("PS C:\\Users\\alice> "), PromptKind::ShellPrompt);
        assert_eq!(k("C:\\Users\\alice>"), PromptKind::ShellPrompt);
        assert_eq!(k("C:\\> "), PromptKind::ShellPrompt);
        assert_eq!(k("Microsoft Windows\nC:\\Windows\\System32>"), PromptKind::ShellPrompt);
    }

    #[test]
    fn shell_fish_and_arrows() {
        assert_eq!(k("alice@host ~> "), PromptKind::ShellPrompt);
        assert_eq!(k("❯ "), PromptKind::ShellPrompt);
        assert_eq!(k("~/proj ❯ "), PromptKind::ShellPrompt);
        assert_eq!(k("➜  proj git:(main) ✗ "), PromptKind::ShellPrompt);
    }

    #[test]
    fn shell_after_output() {
        assert_eq!(
            k("total 0\n-rw-r--r-- 1 a a 0 x\nalice@host:~$ "),
            PromptKind::ShellPrompt
        );
    }

    #[test]
    fn shell_only_when_last_nonempty_line() {
        // 提示符在中间，最后是普通输出：不算
        assert_ne!(k("alice@host:~$ \nrunning...\nstill running"), PromptKind::ShellPrompt);
        // 提示符 + 正在执行的命令：不算
        assert_ne!(k("alice@host:~$ sleep 100"), PromptKind::ShellPrompt);
    }

    #[test]
    fn shell_trailing_blank_lines_ignored() {
        assert_eq!(k("alice@host:~$ \n\n\n"), PromptKind::ShellPrompt);
        assert_eq!(k("alice@host:~$ \r\n\r\n"), PromptKind::ShellPrompt);
    }

    #[test]
    fn shell_false_positive_percent_and_progress() {
        assert_ne!(k("Downloading... 100%"), PromptKind::ShellPrompt);
        assert_ne!(k("progress: 45%"), PromptKind::ShellPrompt);
        assert_ne!(k("[##########] ########"), PromptKind::ShellPrompt);
        assert_ne!(k("=========="), PromptKind::ShellPrompt);
        assert_ne!(k("Total cost: 5$"), PromptKind::ShellPrompt);
    }

    #[test]
    fn shell_alt_screen_is_downgraded() {
        let g = classify("alice@host:~$ ", true);
        assert_eq!(g.kind, PromptKind::ShellPrompt);
        assert!(g.confidence <= 0.5);
        let g2 = classify("alice@host:~$ ", false);
        assert!(g2.confidence >= 0.9);
    }

    // ——— ANSI / CRLF / 空白 ———

    #[test]
    fn ansi_colored_shell_prompt() {
        let s = "\u{1b}[01;32malice@host\u{1b}[00m:\u{1b}[01;34m~\u{1b}[00m$ ";
        assert_eq!(k(s), PromptKind::ShellPrompt);
    }

    #[test]
    fn ansi_colored_password() {
        assert_eq!(k("\u{1b}[1m[sudo] password for alice:\u{1b}[0m "), PromptKind::Password);
    }

    #[test]
    fn ansi_with_osc_title() {
        let s = "\u{1b}]0;alice@host: ~\u{07}\u{1b}[?2004halice@host:~$ ";
        assert_eq!(k(s), PromptKind::ShellPrompt);
    }

    #[test]
    fn crlf_password() {
        assert_eq!(k("ssh x@y\r\nx@y's password: "), PromptKind::Password);
    }

    #[test]
    fn crlf_yesno() {
        assert_eq!(k("foo\r\nContinue? [Y/n] \r\n"), PromptKind::YesNo);
    }

    #[test]
    fn crlf_repl() {
        assert_eq!(k("Python 3.12\r\n>>> "), PromptKind::Repl);
    }

    #[test]
    fn trailing_spaces_and_tabs() {
        assert_eq!(k("Password:   \t "), PromptKind::Password);
        assert_eq!(k("[Y/n]    "), PromptKind::YesNo);
        assert_eq!(k("alice@host:~$      "), PromptKind::ShellPrompt);
    }

    #[test]
    fn ansi_colored_yesno() {
        assert_eq!(
            k("\u{1b}[33mOverwrite?\u{1b}[0m \u{1b}[1m[y/N]\u{1b}[0m "),
            PromptKind::YesNo
        );
    }

    #[test]
    fn fullwidth_colon_and_nbsp() {
        assert_eq!(k("密码：\u{a0}"), PromptKind::Password);
        assert_eq!(k("密码：\u{3000}"), PromptKind::Password);
    }

    // ——— 健壮性 ———

    #[test]
    fn empty_and_whitespace_input() {
        for s in ["", " ", "\n", "\r\n\r\n", "   \n  \n", "\u{1b}[0m"] {
            let g = classify(s, false);
            assert_eq!(g.kind, PromptKind::Unknown, "input {s:?}");
            assert!(g.confidence < 0.2);
        }
        assert_eq!(classify("", true).kind, PromptKind::Unknown);
    }

    #[test]
    fn huge_input_does_not_panic_and_uses_tail() {
        let mut s = "x".repeat(2_000_000);
        s.push_str("\nContinue? [Y/n] ");
        assert_eq!(k(&s), PromptKind::YesNo);
    }

    #[test]
    fn huge_single_line_without_newline() {
        let s = "ab".repeat(500_000);
        let g = classify(&s, false);
        assert_eq!(g.kind, PromptKind::Unknown);
    }

    #[test]
    fn multibyte_boundary_cut_does_not_panic() {
        // 总长度不对齐 4096：确保按字符边界截断
        let mut s = "密".repeat(5000);
        s.push_str("\n密码：");
        assert_eq!(k(&s), PromptKind::Password);
        let s2 = format!("{}😀", "密".repeat(2000));
        let _ = classify(&s2, false);
    }

    #[test]
    fn binary_garbage_does_not_panic() {
        let s: String = (0u8..=255).map(|b| b as char).collect();
        let _ = classify(&s, false);
        let _ = classify(&s, true);
    }

    #[test]
    fn lone_escape_does_not_panic() {
        let _ = classify("\u{1b}", false);
        let _ = classify("\u{1b}[", false);
        let _ = classify("abc\u{1b}]0;unterminated", true);
    }

    // ——— 长输出与误判 ———

    #[test]
    fn long_output_mentioning_keywords_in_middle() {
        let s = "enter your password: in the config\n\
                 (y/n) prompts are disabled with -y\n\
                 press any key is not required\n\
                 select an option from the list\n\
                 >>> is the python prompt\n\
                 compiled 120 files in 3.4s";
        assert_eq!(k(s), PromptKind::Unknown);
    }

    #[test]
    fn plain_output_is_unknown_with_low_confidence() {
        let g = classify("compiling foo v0.1.0\ncompiling bar v0.2.0", false);
        assert_eq!(g.kind, PromptKind::Unknown);
        assert!(g.confidence <= 0.2);
        assert!(!g.hint.is_empty());
    }

    #[test]
    fn unknown_alt_screen_has_dedicated_hint() {
        let g = classify("some vim buffer text\n-- INSERT --", true);
        assert_eq!(g.kind, PromptKind::Unknown);
        assert_ne!(g.hint, classify("some text", false).hint);
    }

    #[test]
    fn error_message_with_colon_is_not_password() {
        assert_ne!(k("error: could not read from remote repository:"), PromptKind::Password);
    }

    #[test]
    fn question_unrelated_is_not_yesno() {
        assert_ne!(k("what is this?"), PromptKind::YesNo);
    }

    #[test]
    fn serde_snake_case() {
        let j = serde_json::to_string(&PromptKind::ShellPrompt).unwrap();
        assert_eq!(j, "\"shell_prompt\"");
        let j = serde_json::to_string(&PromptKind::YesNo).unwrap();
        assert_eq!(j, "\"yes_no\"");
    }

    #[test]
    fn confidence_always_in_range_and_hint_nonempty() {
        let samples = [
            "", "Password:", "Continue? [Y/n]", "(END)", "1) a\n2) b\nChoose [1-2]:", ">>> ",
            "alice@host:~$ ", "random output", "---", "😀", "> ",
        ];
        for s in samples {
            for alt in [false, true] {
                let g = classify(s, alt);
                assert!((0.0..=1.0).contains(&g.confidence), "{s:?}");
                assert!(!g.hint.is_empty(), "{s:?}");
            }
        }
    }

    #[test]
    fn every_regex_compiles() {
        // 触发所有 re! 的惰性编译：pattern 写错在这里第一时间暴露
        for s in [
            ">>> ", "x\n1) a\n2) b\nChoose [1-2]: ", "PS C:\\> ", "C:\\>", "lines 1-2/3", "a\n  (*) x\n  ( ) y",
            "postgres=# ", "127.0.0.1:6379>", "   ...: ", "[root@h ~]# ", "/ # ", "bash-5.1$ ",
        ] {
            let _ = classify(s, true);
            let _ = classify(s, false);
        }
    }

    #[test]
    fn normal_screen_pager_needs_static_screen_and_paged_content() {
        let log = [
            "commit 3f2a1b0c9d8e7f6a5b4c3d2e1f0a9b8c7d6e5f4a",
            "Author: Dev <dev@example.com>",
            "Date:   Mon Nov 9 10:00:00 2026 +0800",
            "",
            "    fix(agent): tighten prompt rules",
            "",
            "commit 1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b",
        ];
        let g = normal_screen_pager(&log, ":", true).expect("静止的一屏 git log + `:` 是分页器");
        assert_eq!(g.kind, PromptKind::Pager);
        // 屏幕还在动：不认
        assert!(normal_screen_pager(&log, ":", false).is_none());
        // 内容太少 / 上一行是提示 / 末行不是裸冒号：不认
        assert!(normal_screen_pager(&log[..2], ":", true).is_none());
        assert!(normal_screen_pager(&["a", "b", "c", "d", "Enter value:"], ":", true).is_none());
        assert!(normal_screen_pager(&log, "key: value", true).is_none());
        // 主屏幕上的裸 `:` 不经这条路径时仍是 Unknown（classify 本身不知道屏幕静没静止）
        assert_eq!(classify(&format!("{}\n:", log.join("\n")), false).kind, PromptKind::Unknown);
    }
}
