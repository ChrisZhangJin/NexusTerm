//! P1 局面分类器：把一帧屏幕结构化成「现在是什么局面」。
//!
//! 设计见 `docs/nexus-agent-steward-design.md` §2 / §3.2。与 `prompt.rs` 的分工：
//! `prompt::classify` 判「终端在等什么输入」（7 类、只看最后一行），是这里的一个**信号**；
//! `situation::classify` 判「整个局面是什么」（程序、提问主题、选项、进度、空转语义），
//! 输出可直接喂给 Steward 循环的结构体。
//!
//! 三条硬约束：
//! 1. **纯函数、零 IO、零 LLM**。分类在每次心跳都跑，一旦调模型长任务就挂不住几小时。
//! 2. **拿不准就低置信度**。alt-screen、前端帧断流、屏幕为空都明确降权，P1 只读不写，
//!    P3 的 PolicyEngine 会据此把允许的动作收敛到 Observe。
//! 3. **signature 不含帧哈希**。进度条每帧都变，但它仍是「Progress」局面；
//!    空转判的是语义没推进，不是字节没变。
//!
//! 屏幕文本出事件流前统一走 `redact::redact`（沿用出网脱敏规则）：
//! 局面卡片、建议卡、以后给 LLM 的上下文都不该带着密钥原文。

use serde::Serialize;

use super::buffer::strip_ansi;
use super::prompt::{self, PromptGuess, PromptKind};
use super::redact;
use super::steward::ScreenSnapshot;

/// 只分析末尾这么多行：claude code 的提问卡、权限卡、菜单都在最后一屏内。
const TAIL_LINES: usize = 24;
/// 判定「最近」时回看的行数，避免把 24 行前的历史输出当成当前提示。
const RECENT_LINES: usize = 8;
/// 事件流里保留的脱敏屏幕文本上限（字符）。
const TAIL_CHARS: usize = 1200;
/// 连续这么多帧（含前端心跳帧）内容没变，才算「屏幕静止」。
const STATIC_FRAMES: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Program {
    Shell,
    ClaudeCode,
    Vim,
    Pager,
    Tui,
    Script,
    Ssh,
    Unknown,
}

impl Program {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::ClaudeCode => "claude_code",
            Self::Vim => "vim",
            Self::Pager => "pager",
            Self::Tui => "tui",
            Self::Script => "script",
            Self::Ssh => "ssh",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SituationKind {
    /// claude code / 交互脚本在问一个需要内容回答的问题
    Question,
    /// 授权确认（含「本次允许 / 永久允许」这类选项）
    Permission,
    /// 密码 / 口令 / 验证码：Steward 永不代填
    Password,
    /// 编号菜单或全屏选择框
    Menu,
    /// less / more / `按任意键继续`
    Pager,
    /// 任务在跑（进度条 / Thinking / 编译中）
    Progress,
    /// 子任务状态卡（截图里「两个 subagent 都没在跑」）
    SubagentStatus,
    /// 程序闲着等人输入（claude code 输入框、脚本 `Input:`）
    AwaitingInput,
    /// 回到 shell 提示符
    Idle,
    /// 报错停住
    Error,
    /// 全屏程序但认不出具体局面
    AltScreen,
    /// 看不出来
    Unknown,
}

impl SituationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Question => "question",
            Self::Permission => "permission",
            Self::Password => "password",
            Self::Menu => "menu",
            Self::Pager => "pager",
            Self::Progress => "progress",
            Self::SubagentStatus => "subagent_status",
            Self::AwaitingInput => "awaiting_input",
            Self::Idle => "idle",
            Self::Error => "error",
            Self::AltScreen => "alt_screen",
            Self::Unknown => "unknown",
        }
    }
}

/// 一个可选项。`grantsPermanent` 是给 P3 PolicyEngine 的硬标记：
/// 设计场景 B 里「Yes, and don't ask again」会永久扩大授权，绝不能被自动选中。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Choice {
    pub index: Option<String>,
    pub text: String,
    pub grants_permanent: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Situation {
    pub frame_id: u64,
    pub screen_hash: String,
    pub program: Program,
    pub kind: SituationKind,
    pub topic: Option<String>,
    pub options: Vec<Choice>,
    pub confidence: f32,
    /// 脱敏后的末尾屏幕文本，供审计 / 以后喂 LLM
    pub tail: String,
    /// 命中了哪条规则：局面卡片能解释「为什么这么判」
    pub evidence: Vec<String>,
    /// 距最后一帧的时间；超过阈值说明前端断流（浮窗 / 最小化 / 锁屏）
    pub stale_ms: i64,
    /// 语义签名（不含 hash），空转检测比的就是它
    pub signature: String,
}

impl Situation {
    pub fn signature_of(
        program: Program,
        kind: SituationKind,
        topic: Option<&str>,
        options: &[Choice],
    ) -> String {
        let opts = options
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("/");
        format!(
            "{}|{}|{}|{}",
            program.as_str(),
            kind.as_str(),
            topic.unwrap_or(""),
            opts
        )
    }
}

/// 选中一个选项之后要不要再按回车。
///
/// - `NeedsEnter`：行式提示（`Select [1-3]: ` / bash `select`）——编号 + 回车；
/// - `DigitSubmits`：带 `❯` 选中标记的编号菜单（Claude Code 权限卡、Ink select）——按数字即选中并提交，
///   多发的回车会落到下一屏（可能就是下一张权限卡）；
/// - `ArrowSelect`：inquirer 一类无编号的方向键菜单——只能方向键 + 回车，输入编号无效。
///   这里只负责识别并记录，驱动方向键是后续任务；在那之前注入层对它一律不写。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmitStyle {
    #[default]
    NeedsEnter,
    DigitSubmits,
    ArrowSelect,
}

/// `SubmitStyle` 记在 `evidence` 里（不进 signature，也不改 `Situation` 的结构）。
pub const SUBMIT_DIGIT: &str = "submit:digit-only";
pub const SUBMIT_ARROW: &str = "submit:arrow-select";
/// 方向键菜单的光标位置（证据形如 `menu:cursor=2`，0 起）
pub const MENU_CURSOR: &str = "menu:cursor=";

/// 方向键菜单此刻光标停在第几项（0 起）。
pub fn menu_cursor(sit: &Situation) -> Option<usize> {
    sit.evidence.iter().find_map(|e| e.strip_prefix(MENU_CURSOR)?.parse().ok())
}

/// 方向键菜单时给模型的操作提示：光标在哪、怎么移过去。不是方向键菜单返回 None。
pub fn arrow_hint(sit: &Situation) -> Option<String> {
    if submit_style(sit) != SubmitStyle::ArrowSelect || sit.options.is_empty() {
        return None;
    }
    let mut s = String::from("提交方式：方向键菜单，输入编号 / 文字都无效，回车提交光标所在项。\n");
    match menu_cursor(sit) {
        Some(c) => {
            s.push_str(&format!(
                "光标在第 {} 项「{}」。要选第 N 项：用 nx_send_keys 发 <Down*k><Enter>（k = N - {}，往上用 <Up*k>）；\
                 选当前项直接发 <Enter>。\n",
                c + 1,
                sit.options[c].text,
                c + 1
            ));
        }
        None => s.push_str("看不出光标在哪一项：先 nx_screen 确认，再用 <Down*k> / <Up*k> 移过去后 <Enter>。\n"),
    }
    Some(s)
}

/// 光标所在行在 `snap.lines` 里的下标。光标在 (0,0) 视为「帧没带光标」（真实终端里几乎不会停在左上角）。
pub fn cursor_index(snap: &ScreenSnapshot) -> Option<usize> {
    if snap.cursor_x == 0 && snap.cursor_y == 0 {
        return None;
    }
    let total = snap.lines.len();
    let top = total.saturating_sub(snap.rows as usize);
    let idx = top + snap.cursor_y as usize;
    (idx < total).then_some(idx)
}

/// 「当前行」为止的窗口终点：光标下面还画着东西时取到光标行（含它上面最后一个非空行），否则就是 `end`。
fn current_end(snap: &ScreenSnapshot, start: usize, end: usize) -> usize {
    cursor_index(snap)
        .filter(|ci| ci + 1 < end)
        .and_then(|ci| snap.lines[..=ci].iter().rposition(|l| !strip_ansi(l).trim().is_empty()).map(|i| i + 1))
        .filter(|e| *e > start)
        .unwrap_or(end)
}

/// 渲染帧里到「当前行」为止的文本（给提示分类器用）与真实的 alt 标志。
pub fn frame_text(snap: &ScreenSnapshot) -> (String, bool) {
    let end = snap.lines.iter().rposition(|l| !strip_ansi(l).trim().is_empty()).map_or(0, |i| i + 1);
    let start = end.saturating_sub(TAIL_LINES);
    let cur_end = current_end(snap, start, end);
    let text = snap.lines[start..cur_end].iter().map(|l| strip_ansi(l).trim_end().to_string()).collect::<Vec<_>>().join("\n");
    (text, snap.alt)
}

/// 发了答复按键之后，被答的那个提示是不是真的过去了（回执）。
/// - 之前不是在等人回答（不是提问 / 授权 / 菜单 / 密码 / 等输入）：无从核对，算过去了；
/// - 局面种类变了：过去了；
/// - 种类没变：要求局面签名变了、且**不是同一个**提问（主题也变了）。只有转圈 / 计时在跳不算——
///   签名里没有屏幕原文，转圈改不了它；
/// - 没有签名可比（退回旁路缓冲的分类）、种类又没变：按「提示仍在」处理。
pub fn prompt_answered(before: &Situation, after: &Situation) -> bool {
    use SituationKind as K;
    if !matches!(before.kind, K::Question | K::Permission | K::Menu | K::Password | K::AwaitingInput | K::Pager) {
        return true;
    }
    if after.kind != before.kind {
        return true;
    }
    if before.signature.is_empty() || after.signature.is_empty() {
        return false;
    }
    after.signature != before.signature && after.topic != before.topic
}

/// 这个局面里选中选项的提交方式。没有标记 = 行式提示，编号后要回车。
pub fn submit_style(sit: &Situation) -> SubmitStyle {
    if sit.evidence.iter().any(|e| e == SUBMIT_ARROW) {
        SubmitStyle::ArrowSelect
    } else if sit.evidence.iter().any(|e| e == SUBMIT_DIGIT) {
        SubmitStyle::DigitSubmits
    } else {
        SubmitStyle::NeedsEnter
    }
}

/// 两次取到的局面是否「实质不同」：等审批 / 等控制权的这段时间里屏幕换了一个局面，
/// 之前对着旧局面批准的按键就不能再发。返回给模型看的一句原因。
///
/// 比的是语义（kind / program / 主题 / 选项）和光标所在的末行，不比帧哈希：
/// 进度条、时钟每帧都变，但局面没变。仍在跑的任务（Progress）末行本来就一直在变，不比末行。
pub fn materially_changed(before: &Situation, after: &Situation) -> Option<String> {
    if before.kind != after.kind {
        return Some(format!(
            "局面从 {} 变成了 {}",
            before.kind.as_str(),
            after.kind.as_str()
        ));
    }
    if before.program != after.program {
        return Some(format!(
            "前台程序从 {} 变成了 {}",
            before.program.as_str(),
            after.program.as_str()
        ));
    }
    if !before.signature.is_empty() && !after.signature.is_empty() && before.signature != after.signature {
        return Some("提问 / 选项已经不是批准时的那一屏".to_string());
    }
    if before.kind != SituationKind::Progress {
        let last = |t: &str| {
            t.lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.trim().to_string())
                .unwrap_or_default()
        };
        if last(&before.tail) != last(&after.tail) {
            return Some("光标所在的那一行已经变了".to_string());
        }
    }
    None
}

struct Draft {
    kind: SituationKind,
    confidence: f32,
    evidence: Vec<String>,
    topic: Option<String>,
    options: Vec<Choice>,
}

fn draft(
    kind: SituationKind,
    confidence: f32,
    rule: &str,
    topic: Option<String>,
    options: Vec<Choice>,
) -> Draft {
    Draft {
        kind,
        confidence,
        evidence: vec![rule.to_string()],
        topic,
        options,
    }
}

/// 去掉全屏程序的框线，让 `1. Yes` 这类选项能被解析，主题也不带一堆 `│`。
///
/// 只剥框线字符；选中标记（`❯` / `›` / `>`）只在**行首**、且后面跟着空格和一个像选项的东西时才剥。
/// `>` 不能当框线两头乱剥：`>>> ` / `mysql> ` / PS2 的 `> ` 是 REPL 提示符，`<Ok>` 是 whiptail 按钮。
fn clean_line(s: &str) -> String {
    const FRAME: &str = "│┃║╭╰╮╯─━═╔╗╚╝┌┐└┘┏┓┗┛";
    let t = s
        .trim()
        .trim_matches(|c: char| FRAME.contains(c) || c.is_whitespace());
    strip_cursor(t).trim().to_string()
}

/// 行首选中标记后面是不是一个选项：`1. Yes` / `2) No` / `[3] x`。
fn option_like(rest: &str) -> bool {
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 {
        return rest[digits..].starts_with(['.', ')', ':']);
    }
    rest.starts_with('[') && rest[1..].starts_with(|c: char| c.is_ascii_digit())
}

/// 剥掉行首的选择器光标：`❯ 1. Yes` → `1. Yes`、`› Refactor now` → `Refactor now`。
/// `❯` / `›` 后跟任意文字都算（它们不会是 REPL 提示符）；`>` 只认后面是编号选项的，
/// 否则 `> ` 续行 / node 提示符 / 引用块原样保留。
fn strip_cursor(t: &str) -> &str {
    for (cursor, any_text) in [('❯', true), ('›', true), ('>', false)] {
        let Some(rest) = t.strip_prefix(cursor) else {
            continue;
        };
        if !rest.starts_with(char::is_whitespace) {
            return t;
        }
        let body = rest.trim_start();
        if body.is_empty() {
            return t;
        }
        if any_text || option_like(body) {
            return body;
        }
        return t;
    }
    t
}

fn detect_program(lower: &str, alt: bool, guess: PromptKind) -> Program {
    if lower.contains("claude code")
        || lower.contains("claude-code")
        || lower.contains("esc to interrupt")
        || lower.contains("? for shortcuts")
        || lower.contains("don't ask again")
        || lower.contains("accept edits on")
        || lower.contains("shift+tab to cycle")
        || lower.contains("bypass permissions on")
        || lower.contains("tell claude what to do")
        || lower.contains("do you trust the files")
        || lower.contains("do you want to make this edit")
        || lower.contains("⏵⏵")
        || lower.contains("tokens") && lower.contains("⎿")
    {
        return Program::ClaudeCode;
    }
    if lower.contains("-- insert --") || lower.contains("normal mode") || lower.contains("[no name]") {
        return Program::Vim;
    }
    if lower.contains("--more--") || lower.contains("(end)") || guess == PromptKind::Pager {
        return Program::Pager;
    }
    if lower.contains("tmux") || lower.contains("htop") || lower.contains("ncurses") {
        return Program::Tui;
    }
    if lower.contains("password for") || lower.contains("authenticity of host") {
        return Program::Ssh;
    }
    if lower.contains("traceback") || lower.contains(">>>") || lower.contains("input:") {
        return Program::Script;
    }
    if alt {
        return Program::Tui;
    }
    // 只认「光标所在的最后一行」是 shell 提示符。屏幕里任何位置出现过 `$ `（脚本跑着时上面
    // 还留着 `user@h:~$ ./install.sh` 的回显）都不能算「现在停在 shell 上」。
    if guess == PromptKind::ShellPrompt {
        return Program::Shell;
    }
    Program::Unknown
}

/// 解析 `1. Yes` / `❯ 2. No` / `[3] ...` 这类编号选项，最多 6 条。
fn parse_options(lines: &[&str]) -> Vec<Choice> {
    let mut out = Vec::new();
    for raw in lines.iter().rev().take(20).rev() {
        let line = clean_line(raw);
        if line.is_empty() || line.len() > 160 {
            continue;
        }
        let (index, text) = if let Some(rest) = line.strip_prefix('[') {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() || !rest[digits.len()..].starts_with(']') {
                continue;
            }
            let cut = digits.len() + 1;
            (digits, rest[cut..].trim().to_string())
        } else {
            let digits: String = line.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                continue;
            }
            let rest = line[digits.len()..].trim_start();
            if !(rest.starts_with('.') || rest.starts_with(')') || rest.starts_with(':')) {
                continue;
            }
            (digits, rest[1..].trim().to_string())
        };
        if text.is_empty() {
            continue;
        }
        let grants_permanent = grants_permanent_text(&text.to_lowercase());
        let choice = Choice {
            index: Some(index),
            text,
            grants_permanent,
        };
        // 同一编号只留最后一条（带 ❯ 的高亮行往往在后面）
        if let Some(pos) = out
            .iter()
            .position(|c: &Choice| c.index == choice.index)
        {
            out[pos] = choice;
        } else if out.len() < 6 {
            out.push(choice);
        }
    }
    out
}

/// 选项原文里的永久扩权特征（「don't ask again」「allow all … this session」…）。
fn grants_permanent_text(l: &str) -> bool {
    l.contains("don't ask")
        || l.contains("don’t ask")
        || l.contains("allow all")
        || l.contains("this session")
        || l.contains("shift+tab")
        || l.contains("auto-accept")
        || l.contains("auto accept")
        || l.contains("bypass permissions")
        || l.contains("yolo")
        || l.contains("skip permissions")
        || l.contains("always")
        || l.contains("permanent")
        || l.contains("永久")
        || l.contains("不再询问")
        || l.contains("不再提示")
}

/// 终端程序的操作提示页脚（`? for shortcuts` / `esc to cancel` / `⏵⏵ accept edits on …`）：
/// 里面带 `?` 或 `esc to` 字样，但不是在向用户提问。
fn is_footer_line(lower: &str) -> bool {
    const FOOTERS: [&str; 14] = [
        "? for shortcuts",
        "esc to cancel",
        "esc to interrupt",
        "esc to exit",
        "accept edits on",
        "shift+tab to cycle",
        "bypass permissions on",
        "plan mode on",
        "enter to confirm",
        "enter to select",
        "to cycle",
        "ctrl+c to",
        "⏵⏵",
        "tab to cycle",
    ];
    FOOTERS.iter().any(|f| lower.contains(f))
}

fn question_line(lines: &[&str]) -> Option<String> {
    lines.iter().rev().take(RECENT_LINES).rev().find_map(|l| {
        let t = clean_line(l);
        if t.is_empty() || is_footer_line(&t.to_lowercase()) {
            return None;
        }
        let has_q = t.contains('?')
            || t.contains('？')
            || t.contains("是否")
            || t.contains("要不要")
            || t.contains("请选择")
            || t.contains("would you like")
            || t.contains("should i")
            || t.contains("do you want");
        if has_q && t.chars().count() >= 4 {
            Some(t.chars().take(120).collect())
        } else {
            None
        }
    })
}

fn topic_of(q: Option<String>, lower: &str) -> Option<String> {
    let q = q?;
    let l = q.to_lowercase();
    if l.contains("remember") || l.contains("记住") || q.contains("是否记住") {
        return Some("settings:remember-check".to_string());
    }
    if l.contains("settings") || q.contains("设置") {
        return Some("settings:ui-change".to_string());
    }
    if l.contains("permission") || q.contains("权限") || lower.contains("don't ask again") {
        return Some("permission:grant".to_string());
    }
    Some(q.chars().take(80).collect())
}

fn has_spinner(s: &str) -> bool {
    s.chars().any(|c| ('⠀'..='⣿').contains(&c))
        || s.contains("◐")
        || s.contains("◓")
        || s.contains("◑")
        || s.contains("◒")
}

fn percentage(s: &str) -> bool {
    let bytes = s.as_bytes();
    for i in 1..bytes.len() {
        if bytes[i] == b'%' && bytes[i - 1].is_ascii_digit() {
            return true;
        }
    }
    false
}

/// 只有原始屏幕行（带框线、带 `>`）才看得出来的线索。`clean_line` 会把 `│ > │` 这类输入框
/// 整行抹成空串，所以这些信号要在清洗之前取。
#[derive(Debug, Clone, Copy, Default)]
struct Hints {
    /// 屏幕底部有 Claude Code 风格的输入框（`│ > │` / 两道横线夹着的 `> `）
    input_box: bool,
    /// 最后一个非空行像是在等一句自由文本（`…目录 [/opt/app]: ` / `Press Enter to continue`）
    free_text: bool,
    /// `free_text` 里有明确的「在要输入」线索（`[default]:` / `name:` / `(y/n)` / `Press Enter`…），
    /// 而不只是一行冒号结尾的话
    free_text_strong: bool,
    /// 最近几行里有带 `❯` / `›` / `>` 选中标记的编号选项（选择器菜单）
    selector: bool,
}

/// `❯ 1. Yes` / `│ › 2. No │`：选择器上带选中标记的编号选项行。
fn selector_line(raw: &str) -> bool {
    let t = raw
        .trim()
        .trim_start_matches(|c: char| c == '│' || c == '┃' || c.is_whitespace());
    let Some(rest) = t.strip_prefix('❯').or_else(|| t.strip_prefix('›')).or_else(|| t.strip_prefix('>')) else {
        return false;
    };
    let rest = rest.trim_start();
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    digits > 0 && rest[digits..].starts_with(|c| c == '.' || c == ')')
}

/// 方向键菜单的操作提示（inquirer / prompts / enquirer）。
fn arrow_cue(lower: &str) -> bool {
    lower.contains("use arrow keys")
        || lower.contains("arrow keys to")
        || lower.contains("↑/↓")
        || lower.contains("↑↓")
}

/// inquirer 一类方向键菜单：`? Pick one: (Use arrow keys)` 下面一列**不带编号**的选项，
/// 当前项前面是 `❯` / `›` / `>`。返回各选项（`index` 为空：输入编号无效，只能方向键 + 回车）。
///
/// 要求同时有选中光标行和方向键提示；带编号的选择器归 `parse_options` / `DigitSubmits`。
/// 方向键菜单：选项 + 光标（❯）当前停在第几项（0 起）。
fn parse_arrow_menu_at(raw: &[String]) -> Option<(Vec<Choice>, usize)> {
    let from = raw.len().saturating_sub(16);
    let window = &raw[from..];
    let lower = window.join("\n").to_lowercase();
    if !arrow_cue(&lower) {
        return None;
    }
    let cursor_at = window.iter().rposition(|l| {
        let t = l.trim_start_matches(|c: char| c == '│' || c == '┃' || c.is_whitespace());
        ['❯', '›', '>'].iter().any(|c| {
            t.strip_prefix(*c).map_or(false, |r| {
                r.starts_with(char::is_whitespace) && {
                    let b = r.trim();
                    !b.is_empty() && !option_like(b)
                }
            })
        })
    })?;
    // 光标行上下相邻、缩进的非空行是同一列选项；碰到提问行（`? …`）、空行或提示页脚就停
    let is_item = |l: &str| {
        let t = clean_line(l);
        let lt = t.to_lowercase();
        !t.is_empty()
            && !t.starts_with('?')
            && !arrow_cue(&lt)
            && !is_footer_line(&lt)
            && t.chars().count() <= 120
    };
    let mut lo = cursor_at;
    while lo > 0 && is_item(&window[lo - 1]) && window[lo - 1].starts_with(char::is_whitespace) {
        lo -= 1;
    }
    let mut hi = cursor_at;
    while hi + 1 < window.len() && is_item(&window[hi + 1]) && window[hi + 1].starts_with(char::is_whitespace) {
        hi += 1;
    }
    let opts: Vec<Choice> = window[lo..=hi]
        .iter()
        .take(12)
        .map(|l| {
            let text = clean_line(l);
            let grants_permanent = grants_permanent_text(&text.to_lowercase());
            Choice {
                index: None,
                text,
                grants_permanent,
            }
        })
        .collect();
    (!opts.is_empty()).then_some((opts, cursor_at - lo))
}

#[cfg(test)]
fn parse_arrow_menu(raw: &[String]) -> Option<Vec<Choice>> {
    parse_arrow_menu_at(raw).map(|x| x.0)
}

/// npm init 一类「冒号 / 问号后面括号里给默认值」的提示：`package name: (myapp) ` /
/// `Is this OK? (yes) `。返回括号里的默认值；直接回车即采用它。
pub fn offered_default(line: &str) -> Option<String> {
    let t = line.trim();
    if t.chars().count() > 120 || !t.ends_with(')') || arrow_cue(&t.to_lowercase()) {
        return None;
    }
    let open = t.rfind('(')?;
    let head = t[..open].trim_end();
    let default = t[open + 1..t.len() - 1].trim();
    if !head.ends_with([':', '?', '：', '？']) || default.contains(['(', ')']) || default.chars().count() > 60 {
        return None;
    }
    let words = head.split_whitespace().count();
    if !(1..=8).contains(&words) || head.chars().count() < 3 {
        return None;
    }
    Some(default.to_string())
}

/// 末行是「shell 提示符 + 已敲的文字」（`user@h:~/p$ rm -rf x` / `PS C:\> dir`）。
/// 光标就在这一行：不管上面残留着什么程序的画面，现在都是 shell 在收这行字。
pub fn shell_prompt_with_text(line: &str) -> bool {
    let t = line.trim_end();
    if t.chars().count() > 300 {
        return false;
    }
    let Some((at, _)) = ["$ ", "# ", "% ", "> "]
        .iter()
        .filter_map(|m| t.find(m).map(|i| (i, m.len())))
        .min()
    else {
        return false;
    };
    let prefix = &t[..at + 1];
    let typed = t[at + 2..].trim();
    if typed.is_empty() || prefix.trim() == "#" || prefix.trim() == ">" {
        return false;
    }
    prompt::classify(prefix, false).kind == PromptKind::ShellPrompt
}

fn is_rule_line(t: &str) -> bool {
    let t = t.trim();
    t.chars().filter(|c| matches!(c, '─' | '━' | '═')).count() >= 8
        && t.chars().all(|c| "─━═╭╮╰╯┌┐└┘ ".contains(c))
}

/// 这一行是不是输入框里的那一行（`> ` / `│ > text │`），返回是否带竖线框。
fn input_line(raw: &str) -> Option<bool> {
    let t = raw.trim();
    let boxed = t.starts_with('│') || t.starts_with('┃');
    let inner = t
        .trim_start_matches(|c: char| c == '│' || c == '┃' || c.is_whitespace())
        .trim_end_matches(|c: char| c == '│' || c == '┃' || c.is_whitespace());
    let rest = inner.strip_prefix('>').or_else(|| inner.strip_prefix('❯'))?;
    let rest = rest.trim_start();
    // `❯ 1. Yes` 是选择器上的选项，不是输入框
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 && rest[digits..].starts_with(|c| c == '.' || c == ')') {
        return None;
    }
    Some(boxed)
}

fn detect_input_box(raw: &[String]) -> bool {
    let n = raw.len();
    let from = n.saturating_sub(8);
    (from..n).any(|i| match input_line(&raw[i]) {
        None => false,
        Some(true) => true,
        Some(false) => {
            let prev = i.checked_sub(1).and_then(|j| raw.get(j));
            let next = raw.get(i + 1);
            prev.map_or(false, |l| is_rule_line(l)) || next.map_or(false, |l| is_rule_line(l))
        }
    })
}

/// 光标行像不像「程序在等一句自由文本」：以冒号 / 问号结尾，或「按回车继续」。
/// 只是形状判断——屏幕要静止一阵才算数，由调用方（nx_watch）把关。
/// 一个词的标签行（`Summary:` / `Fetching:`）不算：那是日志，不是在要输入。
pub fn free_text_prompt(line: &str) -> bool {
    free_text_strong(line) || free_text_weak(line)
}

fn free_text_candidate(line: &str) -> Option<String> {
    let t = line.trim_end();
    if t.is_empty() || t.chars().count() > 200 {
        return None;
    }
    if t.trim_start().starts_with(['│', '┃', '╭', '╰', '─', '═', '|']) {
        return None;
    }
    let l = t.to_lowercase();
    if is_footer_line(&l) {
        return None;
    }
    Some(l)
}

/// 明确的「在要输入」线索：按回车继续、`[默认值]:`、`name:` / `path:`、`(y/n)`、疑问词开头的问句、`Enter …`。
pub fn free_text_strong(line: &str) -> bool {
    let Some(l) = free_text_candidate(line) else {
        return false;
    };
    if l.contains("press enter") || l.contains("press return") || l.contains("hit enter") {
        return true;
    }
    // `package name: (myapp) ` / `Is this OK? (yes) `：括号里是回车即采用的默认值
    if offered_default(&l).is_some() {
        return true;
    }
    let t = l.trim_end();
    let colon = t.ends_with([':', '：']);
    let question = t.ends_with(['?', '？']);
    if !colon && !question {
        return false;
    }
    if t.contains("(y/n)") || t.contains("[y/n]") || t.contains("(yes/no)") || t.contains("password") {
        return true;
    }
    if t.starts_with("enter ") || t.contains(" enter ") || t.contains("请输入") || t.contains("请选择") {
        return true;
    }
    if colon {
        let body = t.trim_end_matches([':', '：']).trim_end();
        // `[/opt/app]:` / `(default: x):`
        if body.ends_with(']') || body.ends_with(')') {
            return true;
        }
        const SUFFIX: [&str; 14] = [
            "name", "path", "dir", "directory", "email", "user", "username", "host", "port", "url",
            "value", "input", "choice", "answer",
        ];
        if SUFFIX.iter().any(|w| body.ends_with(w)) {
            return true;
        }
    }
    if question {
        const WORDS: [&str; 22] = [
            "do", "does", "did", "would", "should", "shall", "can", "could", "will", "are", "is",
            "have", "continue", "proceed", "overwrite", "retry", "ok", "accept", "install", "remove",
            "delete", "replace",
        ];
        const WH: [&str; 4] = ["which", "what", "where", "how"];
        let first = t.split_whitespace().next().unwrap_or("");
        if WORDS.contains(&first) || WH.contains(&first) {
            return true;
        }
    }
    false
}

/// 形状像提示、但没有明确线索：至少两个词、以冒号 / 问号收尾（`Build summary:`）。
fn free_text_weak(line: &str) -> bool {
    let Some(l) = free_text_candidate(line) else {
        return false;
    };
    let t = l.trim_end();
    t.ends_with([':', '?', '：', '？']) && t.split_whitespace().count() >= 2
}

/// `free_text_prompt` 命中时写进证据里的标记。
pub const FREE_TEXT_PROMPT: &str = "prompt:free-text";

fn rule(
    lines: &[&str],
    alt: bool,
    guess: PromptGuess,
    program: Program,
    hints: Hints,
    arrow_opts: Option<Vec<Choice>>,
) -> Draft {
    let recent = lines
        .iter()
        .rev()
        .take(RECENT_LINES)
        .rev()
        .copied()
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase();
    let last = lines.last().copied().unwrap_or("").to_lowercase();
    // 光标附近（最后 3 个非空行）：「allow / deny」这类线索得出现在这里才算对话框，
    // 8 行之前的 `ufw status` 输出不算
    let near = lines
        .iter()
        .rev()
        .take(3)
        .rev()
        .copied()
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase();
    let q = question_line(lines);
    // 光标停在 shell 提示符上：上面不管残留着什么对话框文字，都已经是历史
    let at_shell = guess.kind == PromptKind::ShellPrompt;

    // 1) 密码：沿用 prompt.rs 的判据，绝不代填，也不把内容写进 tail
    if guess.kind == PromptKind::Password {
        return draft(
            SituationKind::Password,
            0.95,
            "prompt:password",
            Some("credential:input".to_string()),
            Vec::new(),
        );
    }

    // 2) 分页器：`(END)` / `--More--` / 任意键继续
    if guess.kind == PromptKind::Pager
        || last.contains("--more--")
        || last.contains("(end)")
        || last.contains("press any key")
    {
        return draft(
            SituationKind::Pager,
            0.9,
            "prompt:pager",
            Some("pager:continue".to_string()),
            Vec::new(),
        );
    }

    // 3) 授权确认：优先于普通提问，因为「Yes, and don't ask again」是硬边界
    // 编号的 Yes / No 选项（Claude Code 的编辑 / 新建 / 信任文件夹对话框都长这样）
    let opts_now = parse_options(lines);
    // Claude Code 正在干活（`esc to interrupt`）或停在空输入框：屏幕上的 `1. Yes / 2. No`、
    // `don't ask again` 是它回复里的文字（markdown 列表、解释），不是对话框——对话框会替掉输入框
    let cc_chatting = program == Program::ClaudeCode
        && (recent.contains("esc to interrupt") || hints.input_box);
    // 编号 Yes/No 要有对话框的形状才算：选中标记 `❯ 1.`，或 `esc to cancel` / `do you want…` 这类线索。
    // 光是两行 `1. Yes` `2. No`（脚本旧输出、回复里的列表）不够
    let dialog_cue = recent.contains("esc to cancel")
        || recent.contains("enter to confirm")
        || recent.contains("enter to select")
        || recent.contains("do you want")
        || recent.contains("would you like");
    let yes_no_options = !cc_chatting
        && (hints.selector || dialog_cue)
        && opts_now.len() >= 2
        && opts_now.iter().any(|c| c.text.to_lowercase().starts_with("yes"))
        && opts_now.iter().any(|c| c.text.to_lowercase().starts_with("no"))
        && !topic_of(q.clone(), &recent).map_or(false, |t| t.starts_with("settings:"));
    // 只有 `continue?` 这类弱确认句（没有 `(y/n)` 记号）不单独构成授权：构建输出里一行问号结尾的话
    // 也长这样。它在下面按「等输入」处理，nx_watch 要屏幕静止一阵才叫醒模型
    let strong_yes_no = guess.kind == PromptKind::YesNo && guess.confidence >= 0.9;
    let looks_permission = !at_shell
        && (yes_no_options
            || (!cc_chatting
                && (recent.contains("do you want to proceed")
                    || recent.contains("don't ask again")
                    || recent.contains("don’t ask again")
                    || recent.contains("yes, and ")
                    || (near.contains("allow") && near.contains("deny"))
                    || (near.contains("permission")
                        && (near.contains("allow") || near.contains("grant")))
                    || strong_yes_no)));
    if looks_permission {
        let mut options = parse_options(lines);
        if options.is_empty() {
            if let Some(a) = &arrow_opts {
                options = a.clone();
            }
        }
        let permanent = options.iter().any(|c| c.grants_permanent);
        let mut d = draft(
            SituationKind::Permission,
            if permanent { 0.95 } else { 0.85 },
            if permanent {
                "claude-code:permission-with-permanent-option"
            } else {
                "claude-code:permission-prompt"
            },
            topic_of(q.clone(), &recent),
            options,
        );
        if permanent {
            d.evidence
                .push("policy:never-auto-select-permanent-grant".to_string());
        }
        return d;
    }

    // 3b) Claude Code 在干活：`✻ Pondering… (12s · esc to interrupt)`。页脚的 `? for shortcuts`
    //     不是提问，所以必须排在提问规则前面
    if program == Program::ClaudeCode && recent.contains("esc to interrupt") {
        return draft(
            SituationKind::Progress,
            0.9,
            "claude-code:working",
            Some("task:in-progress".to_string()),
            Vec::new(),
        );
    }

    // 3c) Claude Code 空输入框、没有在干活：这一轮做完了，等下一条指令
    if program == Program::ClaudeCode && hints.input_box {
        return draft(
            SituationKind::AwaitingInput,
            0.9,
            "claude-code:input-box-idle",
            None,
            Vec::new(),
        );
    }

    // 3d) 选择器菜单（AskUserQuestion / 计划批准 / MCP 信任）：`❯ 1. …` + 其余编号项。
    //     不是 Yes/No 授权，但要人（模型）选一项，选项要给出来，方便识别「永久授权」类
    if hints.selector && opts_now.len() >= 2 {
        return draft(
            SituationKind::Menu,
            0.9,
            "prompt:selector-menu",
            topic_of(q.clone(), &recent).or_else(|| Some("menu:select".to_string())),
            opts_now,
        );
    }

    // 3e) 方向键菜单（inquirer `? Pick one: (Use arrow keys)` + `❯ React`）：无编号，只能方向键 + 回车
    if let Some(opts) = arrow_opts.filter(|o| o.len() >= 2) {
        return draft(
            SituationKind::Menu,
            0.85,
            "prompt:arrow-menu",
            topic_of(q.clone(), &recent).or_else(|| Some("menu:select".to_string())),
            opts,
        );
    }

    // 4) 需要内容回答的提问（截图里「是否记住」这类）
    let claude_q = program == Program::ClaudeCode && q.is_some();
    let script_q = recent.contains("input:")
        || recent.contains("please choose")
        || recent.contains("请选择")
        || (recent.contains("enter your") && recent.contains(":"));
    if claude_q || script_q {
        return draft(
            SituationKind::Question,
            if claude_q { 0.9 } else { 0.7 },
            if claude_q {
                "claude-code:question-card"
            } else {
                "script:question-prompt"
            },
            topic_of(q, &recent),
            parse_options(lines),
        );
    }

    // 5) 编号菜单
    if guess.kind == PromptKind::Menu {
        return draft(
            SituationKind::Menu,
            0.85,
            "prompt:menu",
            Some("menu:select".to_string()),
            parse_options(lines),
        );
    }

    // 5a) REPL 提示符（`>>> ` / `mysql> ` / `irb(main):001:0> ` / node 的 `> `）：程序在等一条语句
    if guess.kind == PromptKind::Repl && program != Program::ClaudeCode {
        return draft(
            SituationKind::AwaitingInput,
            if guess.confidence >= 0.8 { 0.85 } else { 0.5 },
            "prompt:repl",
            Some("repl:statement".to_string()),
            Vec::new(),
        );
    }

    // 5b) 光标行停在一句自由文本提示上（`Enter installation directory [/opt/app]: `）
    if hints.free_text_strong && guess.kind != PromptKind::ShellPrompt {
        // npm init 的 `package name: (myapp) `：默认值记进主题，回车即采用
        let default = lines.last().and_then(|l| offered_default(l));
        let mut d = draft(
            SituationKind::AwaitingInput,
            0.6,
            FREE_TEXT_PROMPT,
            default.as_ref().map(|v| format!("input:default={v}")),
            Vec::new(),
        );
        if default.is_some() {
            d.evidence.push("prompt:default-offered".to_string());
        }
        return d;
    }

    // 5c) 弱确认句（`Continue?` 而没有 `(y/n)`）：当「等输入」，不当授权
    if guess.kind == PromptKind::YesNo && !at_shell {
        return draft(
            SituationKind::AwaitingInput,
            0.5,
            "prompt:yes-no-weak",
            q.clone().or_else(|| Some(last.chars().take(80).collect())),
            Vec::new(),
        );
    }

    // 6) 任务在跑：spinner / 进度 / Thinking / esc to interrupt
    let looks_progress = has_spinner(&recent)
        || percentage(&recent)
        || recent.contains("thinking")
        || recent.contains("esc to interrupt")
        || recent.contains("working…")
        || recent.contains("working...")
        || recent.contains("compiling")
        || recent.contains("downloading")
        || recent.contains("building")
        || recent.contains("running ");
    if looks_progress {
        return draft(
            SituationKind::Progress,
            0.8,
            "cli:progress-indicator",
            Some("task:in-progress".to_string()),
            Vec::new(),
        );
    }

    // 6b) 只是一行冒号 / 问号结尾的话（没有明确线索）：不在跑任务时才当成「等输入」
    if hints.free_text && guess.kind != PromptKind::ShellPrompt {
        return draft(
            SituationKind::AwaitingInput,
            0.5,
            FREE_TEXT_PROMPT,
            None,
            Vec::new(),
        );
    }

    // 7) 子任务状态卡（截图场景 C）
    let looks_subagent = recent.contains("subagent")
        || recent.contains("子任务")
        || recent.contains("agent status")
        || recent.contains("no agents running")
        || (recent.contains("running") && recent.contains("idle"));
    if looks_subagent {
        return draft(
            SituationKind::SubagentStatus,
            0.75,
            "claude-code:subagent-status-card",
            Some("subagent:idle".to_string()),
            Vec::new(),
        );
    }

    // 8) 报错停住
    let looks_error = recent.contains("error:")
        || recent.contains("fatal:")
        || recent.contains("command not found")
        || recent.contains("permission denied")
        || recent.contains("panic")
        || recent.contains("traceback (most recent call last)");
    if looks_error {
        return draft(
            SituationKind::Error,
            0.8,
            "cli:error-marker",
            Some("task:failed".to_string()),
            Vec::new(),
        );
    }

    // 9) 回到 shell
    if guess.kind == PromptKind::ShellPrompt {
        return draft(
            SituationKind::Idle,
            0.85,
            "prompt:shell_prompt",
            None,
            Vec::new(),
        );
    }

    // 10) 程序闲着等人输入
    if program == Program::ClaudeCode
        || last.trim_end().ends_with('>')
        || last.trim_end().ends_with('❯')
        || recent.contains("enter to select")
        || recent.contains("esc to cancel")
    {
        return draft(
            SituationKind::AwaitingInput,
            0.6,
            "cli:input-ready",
            None,
            Vec::new(),
        );
    }

    if alt {
        return draft(
            SituationKind::AltScreen,
            0.3,
            "screen:alt-unclassified",
            None,
            Vec::new(),
        );
    }
    draft(
        SituationKind::Unknown,
        0.25,
        "screen:no-rule-matched",
        None,
        Vec::new(),
    )
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let skip = s.chars().count() - max;
    s.chars().skip(skip).collect()
}

/// 末行是 shell 提示符时追加的证据标记（局面不一定判成 Idle）。
pub const SHELL_PROMPT_VISIBLE: &str = "shell:prompt-visible";

/// 屏幕当前是否停在 shell 提示符上。此时写入的任何文本都会被当成命令执行，
/// 决策层与空转推进都以此为硬闸。
pub fn shell_visible(sit: &Situation) -> bool {
    sit.program == Program::Shell
        || sit
            .evidence
            .iter()
            .any(|e| e == SHELL_PROMPT_VISIBLE || e == "prompt:shell_prompt")
}

/// 对一帧屏幕分类。`stale_limit_ms` 由 StewardConfig 给（默认 6s）；
/// 前端帧断流时置信度减半，P3 会据此只允许只读动作。
pub fn classify(snap: &ScreenSnapshot, now_ms: i64, stale_limit_ms: i64) -> Situation {
    // 前端抓的是整块缓冲区，光标下方常有几十行空白（clear 之后、刚开的 tab）。
    // 先砍掉尾部空行再取窗口，否则提示符被挤出窗口，整屏只剩空行 → Unknown。
    let end = snap
        .lines
        .iter()
        .rposition(|l| !strip_ansi(l).trim().is_empty())
        .map_or(0, |i| i + 1);
    let start = end.saturating_sub(TAIL_LINES);
    let cleaned: Vec<String> = snap.lines[start..end]
        .iter()
        .map(|l| clean_line(&strip_ansi(l)))
        .collect();
    let non_empty: Vec<&str> = cleaned
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|s| s.as_str())
        .collect();
    let text = cleaned.join("\n");
    let lower = text.to_lowercase();
    // 「当前行」= 光标所在行（帧带了光标、且光标下面还画着东西时）：TUI 常在输入行下面画状态栏 /
    // 快捷键提示，拿最后一个非空行当提示行会认错。程序识别、输入框、选项仍看整个窗口
    let cur_end = current_end(snap, start, end);
    let cur_text = snap.lines[start..cur_end].iter().map(|l| clean_line(&strip_ansi(l))).collect::<Vec<_>>().join("\n");
    let mut guess = prompt::classify(&cur_text, snap.alt);
    let raw: Vec<String> = snap.lines[start..end]
        .iter()
        .map(|l| strip_ansi(l).trim_end().to_string())
        .filter(|l| !l.trim().is_empty())
        .collect();
    let raw_cur: Vec<String> = snap.lines[start..cur_end]
        .iter()
        .map(|l| strip_ansi(l).trim_end().to_string())
        .filter(|l| !l.trim().is_empty())
        .collect();
    let input_box = detect_input_box(&raw);
    // `clean_line` 会把行尾的 `>` 当框线抹掉，PowerShell `PS C:\> ` / cmd `C:\>` / fish `u@h ~> `
    // 就认不出来了：清洗没认出提示符时，拿原始末行再认一次
    if guess.kind == PromptKind::Unknown && !input_box {
        if let Some(l) = raw_cur.last() {
            let g = prompt::classify(l, snap.alt);
            if g.kind == PromptKind::ShellPrompt {
                guess = g;
            }
        }
    }
    // git 用 `LESS=FRX`：less 跑在主屏幕上，最后一行只剩一个 `:`。屏幕静止了一阵、上面是一屏内容时才认
    if guess.kind == PromptKind::Unknown && !snap.alt {
        if let Some((last, before)) = raw_cur.split_last() {
            let before: Vec<&str> = before.iter().map(|s| s.as_str()).collect();
            if let Some(g) = prompt::normal_screen_pager(&before, last, snap.idle_frames >= STATIC_FRAMES) {
                guess = g;
            }
        }
    }
    let typed_prompt = !snap.alt && raw_cur.last().map_or(false, |l| shell_prompt_with_text(l));
    let mut program = detect_program(&lower, snap.alt, guess.kind);
    let stale_cc = program == Program::ClaudeCode;
    // 光标行就是 shell 提示符（空的，或带着已敲的字）：上面残留的 Claude Code 画面已经是历史，
    // 不能再按 Claude Code 的规则判（否则 `rm -rf` 敲在提示符上、一个回车就被当成「程序里的短答」放行）
    if program == Program::ClaudeCode && !snap.alt {
        if guess.kind == PromptKind::ShellPrompt {
            program = Program::Shell;
        } else if typed_prompt {
            program = Program::Unknown;
        }
    }
    let hints = Hints {
        input_box,
        free_text: raw_cur.last().map_or(false, |l| free_text_prompt(l)),
        free_text_strong: raw_cur.last().map_or(false, |l| free_text_strong(l)),
        selector: raw.iter().rev().take(RECENT_LINES.max(12)).any(|l| selector_line(l)),
    };
    let (arrow_opts, arrow_cursor) = match parse_arrow_menu_at(&raw) {
        Some((o, c)) => (Some(o), Some(c)),
        None => (None, None),
    };
    let arrow = arrow_opts.is_some();
    let mut d = rule(&non_empty, snap.alt, guess, program, hints, arrow_opts);
    // 选中之后怎么提交（见 `SubmitStyle`）：带编号且有 `❯` 选中标记 / Claude Code 的选择卡按数字即提交；
    // 无编号的方向键菜单只能方向键；其余行式提示（`Select [1-3]: `）编号后要回车
    if matches!(
        d.kind,
        SituationKind::Permission | SituationKind::Menu | SituationKind::Question
    ) && !d.options.is_empty()
    {
        if d.options.iter().all(|o| o.index.is_none()) {
            if arrow {
                d.evidence.push(SUBMIT_ARROW.to_string());
                if let Some(c) = arrow_cursor.filter(|c| *c < d.options.len()) {
                    d.evidence.push(format!("{MENU_CURSOR}{c}"));
                }
            }
        } else if hints.selector || program == Program::ClaudeCode {
            d.evidence.push(SUBMIT_DIGIT.to_string());
        }
    }
    if typed_prompt && stale_cc && !d.evidence.iter().any(|e| e == SHELL_PROMPT_VISIBLE) {
        d.evidence.push(SHELL_PROMPT_VISIBLE.to_string());
    }
    // 规则按优先级 return，「报错」排在「回到 shell」前面：屏幕上一行 error 加一个空闲
    // 提示符会被判成 Error，提示符这一事实就丢了。单独补一条证据，决策层据此拒绝代答。
    if guess.kind == PromptKind::ShellPrompt
        && !d.evidence.iter().any(|e| e == "prompt:shell_prompt")
    {
        d.evidence.push(SHELL_PROMPT_VISIBLE.to_string());
    }

    let stale_ms = if snap.updated_at > 0 {
        (now_ms - snap.updated_at).max(0)
    } else {
        stale_limit_ms + 1
    };
    if stale_ms > stale_limit_ms {
        d.confidence = (d.confidence * 0.5).max(0.1);
        d.evidence.push(format!("stale-feed:>{stale_limit_ms}ms"));
    }
    if snap.alt && d.confidence > 0.8 && d.kind == SituationKind::Unknown {
        d.confidence = 0.5;
        d.evidence.push("screen:alt-penalty".to_string());
    }

    let signature = Situation::signature_of(program, d.kind, d.topic.as_deref(), &d.options);
    // 密码提示那一行连「脱敏后的原文」都不留：脱敏规则认的是 key=value / token，
    // 认不出用户随手敲进口令提示的明文。密码只走 secret.rs，不进事件流 / trace / LLM。
    let tail = if d.kind == SituationKind::Password {
        "[redacted: credential prompt]".to_string()
    } else {
        truncate_chars(&redact::redact(&text), TAIL_CHARS)
    };
    Situation {
        frame_id: snap.frame_id,
        screen_hash: snap.hash.clone(),
        program,
        kind: d.kind,
        topic: d.topic,
        options: d.options,
        confidence: d.confidence,
        tail,
        evidence: d.evidence,
        stale_ms,
        signature,
    }
}

/// P1 的「建议卡」文本：按 §3.3 四类干预给出**建议**，不执行、不调 LLM。
/// P2 接 DecisionCompiler 后，这里会被真实 DecisionRecord 取代。
pub fn observe_suggestion(kind: SituationKind, same_ticks: u32, no_progress_ticks: u32) -> String {
    let loop_risk = same_ticks >= no_progress_ticks.saturating_mul(2);
    if loop_risk && matches!(kind, SituationKind::Question | SituationKind::Permission) {
        return "同一提问反复出现，疑似回环：建议升级给用户并附上前几次局面记录，不要重复作答。".to_string();
    }
    match kind {
        SituationKind::Question | SituationKind::AwaitingInput => {
            "检测到程序在等回答：P2 会按意向生成完整回复并注入；P1 只读，请手动回答。".to_string()
        }
        SituationKind::Permission => {
            "检测到授权确认：建议选「本次允许」；凡含「永久 / 不再询问」的选项一律不自动选。".to_string()
        }
        SituationKind::Password => {
            "密码 / 口令必须由用户本人输入：Steward 永不代填，也不把内容写进事件流。".to_string()
        }
        SituationKind::Progress => {
            "任务仍有输出但局面未推进：建议延长等待并给一条状态摘要，不打断当前进程。".to_string()
        }
        SituationKind::SubagentStatus => {
            "子任务空转：建议催一句 / 换 prompt 重启子任务 / 判定僵死并汇报，附历史记录。".to_string()
        }
        SituationKind::Pager | SituationKind::Menu => {
            "程序停在分页器或菜单：P2 可发送 q / 编号推进；P1 只读，请手动操作。".to_string()
        }
        SituationKind::Idle => {
            "已回到 shell 且局面未变：建议核对 success_criteria，若已满足则交还并出摘要。".to_string()
        }
        SituationKind::Error => "命令报错停住：建议升级并附脱敏屏幕快照，不要盲目重试。".to_string(),
        _ => "局面长时间未变化：建议先对齐屏幕现场（强制全量帧），再决定是否需要人工介入。".to_string(),
    }
}

/// 评审探针里的真实屏幕帧：situation / watch / steward_mode 的回归测试共用。
#[cfg(test)]
pub(crate) mod frames {
    use super::*;

    /// 把一组屏幕行当作一帧（非 alt）分类。
    pub fn classify_lines(lines: &[&str]) -> Situation {
        let snap = ScreenSnapshot {
            console_id: "t".into(),
            frame_id: 7,
            window_start: 0,
            hash: "h".into(),
            alt: false,
            cols: 100,
            rows: 40,
            cursor_x: 0,
            cursor_y: 0, app_cursor: false,
            lines: lines.iter().map(|x| x.to_string()).collect(),
            updated_at: 1_000,
            idle_frames: 0,
        };
        classify(&snap, 1_100, 6_000)
    }

    pub const CC_WORKING: [&str; 13] = ["> fix the failing test in foo.py", "", "● I'll read the file first.", "", "● Read(foo.py)", "  ⎿  Read 120 lines", "", "✻ Pondering… (12s · ↓ 1.2k tokens · esc to interrupt)", "", "╭──────────────────────────────────────────╮", "│ >                                        │", "╰──────────────────────────────────────────╯", "  ? for shortcuts"];
    pub const CC_WORKING_RULES: [&str; 9] = ["● Update(foo.py)", "  ⎿  Updated foo.py with 3 additions", "", "✢ Cogitating… (8s · esc to interrupt)", "", "────────────────────────────────────────", "> ", "────────────────────────────────────────", "  ? for shortcuts"];
    pub const CC_IDLE_BOX: [&str; 6] = ["● Done. I fixed the bug in foo.py and all 12 tests pass.", "", "╭──────────────────────────────────────────╮", "│ >                                        │", "╰──────────────────────────────────────────╯", "  ? for shortcuts"];
    pub const CC_IDLE_ASKS: [&str; 6] = ["● Done. I fixed foo.py. Want me to also update the README?", "", "────────────────────────────────────────", "> ", "────────────────────────────────────────", "  ? for shortcuts"];
    pub const CC_IDLE_ACCEPT_EDITS: [&str; 6] = ["● Done. All tests pass.", "", "────────────────────────────────────────", "> ", "────────────────────────────────────────", "  ⏵⏵ accept edits on (shift+tab to cycle)"];
    pub const CC_BASH_PERM: [&str; 12] = ["╭──────────────────────────────────────────────────────╮", "│ Bash command                                          │", "│                                                       │", "│   rm -rf build                                        │", "│   Remove build directory                              │", "│                                                       │", "│ Do you want to proceed?                               │", "│ ❯ 1. Yes                                              │", "│   2. Yes, and don't ask again for rm commands in /x   │", "│   3. No, and tell Claude what to do differently (esc) │", "│                                                       │", "╰──────────────────────────────────────────────────────╯"];
    pub const CC_EDIT_PERM: [&str; 10] = ["Edit file", "╭─────╮", "│ foo.py │", "│ - a │", "│ + b │", "╰─────╯", "Do you want to make this edit to foo.py?", "❯ 1. Yes", "  2. Yes, allow all edits during this session (shift+tab)", "  3. No, and tell Claude what to do differently (esc)"];
    pub const CC_CREATE_PERM: [&str; 8] = ["Create file", "╭─────╮", "│ new.py │", "╰─────╯", "Do you want to create new.py?", "❯ 1. Yes", "  2. Yes, allow all edits during this session (shift+tab)", "  3. No, and tell Claude what to do differently (esc)"];
    pub const CC_TRUST: [&str; 8] = ["Do you trust the files in this folder?", "", "/root/proj", "", "❯ 1. Yes, proceed", "  2. No, exit", "", "Enter to confirm · Esc to exit"];
    pub const CC_REPLY_YN_WORKING: &[&str] = &["● Options:", "  1. Yes - do it", "  2. No - skip it", "", "✻ Pondering… (12s · esc to interrupt)", "────────────────────────────────────────", "> ", "────────────────────────────────────────", "  ? for shortcuts"];
    pub const CC_REPLY_YN_IDLE: &[&str] = &["● Options:", "  1. Yes - do it", "  2. No - skip it", "", "────────────────────────────────────────", "> ", "────────────────────────────────────────", "  ? for shortcuts"];
    pub const STALE_YN_THEN_RETRY: &[&str] = &["user@h:~/p$ ./run.sh", "  1. Yes - use cache", "  2. No - rebuild", "cache miss", "retrying:"];
    pub const CC_LEFTOVER_THEN_SHELL: &[&str] = &["● Done.", "────────────────────────────────────────", "> ", "────────────────────────────────────────", "  ? for shortcuts", "user@h:~/p$ "];
    pub const CC_LEFTOVER_THEN_TYPED_RM: &[&str] = &["● Done.", "────────────────────────────────────────", "> ", "────────────────────────────────────────", "  ? for shortcuts", "user@h:~/p$ rm -rf /tmp/zz"];
    pub const BUILD_COMPILING: &[&str] = &["user@h:~/p$ ./build.sh", "Compiling modules:"];
    pub const BUILD_SUMMARY: &[&str] = &["user@h:~/p$ ./build.sh", "Summary:"];
    pub const BUILD_FETCHING: &[&str] = &["user@h:~/p$ ./build.sh", "Fetching: "];
    pub const PS_PROMPT: &[&str] = &["Get-ChildItem done", "PS C:\\Users\\x> "];
    pub const PS_TYPED_REMOVE: &[&str] = &["PS C:\\Users\\x> Remove-Item -Recurse -Force C:\\data"];
    pub const CMD_PROMPT: &[&str] = &["Volume in drive C", "C:\\Users\\x>"];
    pub const FISH_PROMPT: &[&str] = &["done", "user@host ~> "];
    pub const CC_ASK_MENU: &[&str] = &["Which approach should I take?", "", "❯ 1. Refactor now", "  2. Leave as is", "  3. Other", "", "Enter to select · ↑/↓ to navigate · Esc to cancel"];
    pub const CC_PLAN_APPROVAL: &[&str] = &["Would you like to proceed?", "", "❯ 1. Yes, and auto-accept edits", "  2. Yes, and manually approve edits", "  3. No, keep planning", "", "ctrl-g to edit in editor · esc to cancel"];
    pub const SCRIPT_RUNNING: [&str; 4] = ["user@h:~/p$ ./long.sh", "step 3 of 10: compiling module x", "  CC foo.o", "  CC bar.o"];
    pub const SCRIPT_YN: [&str; 4] = ["user@h:~/p$ ./install.sh", "Installing packages... 45%", "Unpacking foo", "Do you want to continue? [Y/n] "];
    pub const SCRIPT_MENU: [&str; 6] = ["user@h:~/p$ ./setup.sh", "Select an option:", "1) Install", "2) Upgrade", "3) Quit", "Enter choice [1-3]: "];
    pub const SUDO_PW: [&str; 2] = ["user@h:~/p$ sudo apt update", "[sudo] password for root: "];
    pub const SCRIPT_DIR_PROMPT: [&str; 3] = ["user@h:~/p$ ./install.sh", "Welcome to the installer", "Enter installation directory [/opt/app]: "];
    pub const SCRIPT_PRESS_ENTER: [&str; 2] = ["user@h:~/p$ ./install.sh", "Press Enter to continue or Ctrl-C to abort"];
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(lines: &[&str], alt: bool) -> ScreenSnapshot {
        let v: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
        ScreenSnapshot {
            console_id: "tab-1".to_string(),
            frame_id: 7,
            window_start: 0,
            hash: "abcd1234".to_string(),
            alt,
            cols: 80,
            rows: 24,
            cursor_x: 0,
            cursor_y: 0, app_cursor: false,
            lines: v,
            updated_at: 1_000,
            idle_frames: 0,
        }
    }

    fn c(s: &ScreenSnapshot) -> Situation {
        classify(s, 1_100, 6_000)
    }

    #[test]
    fn trailing_blank_rows_do_not_push_prompt_out_of_window() {
        let mut lines = vec!["$ ls", "a  b  c", "alice@host:~/proj$ "];
        lines.extend(std::iter::repeat("").take(35));
        let x = c(&snap(&lines, false));
        assert_eq!(x.program, Program::Shell);
        assert_eq!(x.kind, SituationKind::Idle);
        assert!(shell_visible(&x));
    }

    #[test]
    fn error_line_above_shell_prompt_keeps_shell_evidence() {
        let s = snap(
            &["$ cargo build", "error: could not compile `nexus`", "alice@host:~/proj$ "],
            false,
        );
        let x = c(&s);
        assert_eq!(x.kind, SituationKind::Error);
        assert!(x.evidence.iter().any(|e| e == SHELL_PROMPT_VISIBLE));
        assert!(shell_visible(&x));
    }

    #[test]
    fn claude_code_question_is_recognized_with_topic() {
        let s = snap(
            &[
                "╭────────────────────────────────────╮",
                "│ Do you want me to change the setting about whether to remember this? │",
                "│ 1. Yes                            │",
                "│ 2. No, keep as is                 │",
                "╰────────────────────────────────────╯",
                "  ? for shortcuts · esc to cancel",
            ],
            true,
        );
        let x = c(&s);
        assert_eq!(x.program, Program::ClaudeCode);
        assert_eq!(x.kind, SituationKind::Question);
        assert_eq!(x.topic.as_deref(), Some("settings:remember-check"));
        assert_eq!(x.options.len(), 2);
        assert!(x.confidence >= 0.8);
    }

    #[test]
    fn permission_with_permanent_option_is_flagged() {
        let s = snap(
            &[
                "Do you want to proceed?",
                "❯ 1. Yes",
                "  2. Yes, and don't ask again",
                "  3. No",
                "  Esc to cancel",
            ],
            true,
        );
        let x = c(&s);
        assert_eq!(x.kind, SituationKind::Permission);
        assert!(x.options.iter().any(|o| o.grants_permanent));
        assert!(x
            .evidence
            .iter()
            .any(|e| e.contains("never-auto-select-permanent-grant")));
    }

    #[test]
    fn plain_question_without_options_is_question() {
        let s = snap(&["Claude Code", "Should I also update the README?", "> "], true);
        let x = c(&s);
        assert_eq!(x.kind, SituationKind::Question);
        assert_eq!(x.program, Program::ClaudeCode);
    }

    #[test]
    fn password_never_leaks_into_tail() {
        // 屏幕里可能残留历史明文；密码局面下整段 tail 一律换成占位
        let s = snap(&["Bearer hunter2secret", "[sudo] password for chris:"], false);
        let x = c(&s);
        assert_eq!(x.kind, SituationKind::Password);
        assert_eq!(x.tail, "[redacted: credential prompt]");
        assert!(!x.tail.contains("hunter2secret"));
    }

    #[test]
    fn progress_spinner_is_progress_not_idle() {
        let s = snap(&["⠋ Compiling 42%", "  esc to interrupt"], true);
        let x = c(&s);
        assert_eq!(x.kind, SituationKind::Progress);
    }

    #[test]
    fn subagent_idle_card_is_detected() {
        let s = snap(&["Subagent status", "running: 0  idle: 11d4m"], true);
        let x = c(&s);
        assert_eq!(x.kind, SituationKind::SubagentStatus);
    }

    #[test]
    fn shell_prompt_is_idle() {
        let s = snap(&["$ pwd", "/tmp", "chris@mac ~ %"], false);
        let x = c(&s);
        assert_eq!(x.kind, SituationKind::Idle);
    }

    #[test]
    fn stale_feed_halves_confidence() {
        let mut s = snap(&["Do you want to proceed?", "1. Yes", "2. No"], false);
        s.updated_at = 100;
        let x = classify(&s, 20_100, 6_000);
        assert!(x.stale_ms > 6_000);
        assert!(x.confidence < 0.5);
        assert!(x.evidence.iter().any(|e| e.starts_with("stale-feed")));
    }

    #[test]
    fn signature_ignores_frame_hash_and_noise() {
        let a = snap(&["⠋ Thinking 12%", "esc to interrupt"], true);
        let mut b = snap(&["⠙ Thinking 34%", "esc to interrupt"], true);
        b.frame_id = 8;
        b.hash = "ffff0000".to_string();
        assert_eq!(c(&a).signature, c(&b).signature);
    }

    #[test]
    fn signature_changes_when_kind_changes() {
        let a = c(&snap(&["$ done", "chris@mac ~ %"], false));
        let b = c(&snap(&["Do you want to proceed?", "1. Yes"], false));
        assert_ne!(a.signature, b.signature);
    }

    #[test]
    fn empty_and_alt_screens_are_low_confidence() {
        let x = c(&snap(&[], true));
        assert!(x.confidence <= 0.3);
        assert_eq!(x.kind, SituationKind::AltScreen);
    }

    #[test]
    fn suggestion_escalates_repeated_question_as_loop() {
        let s = observe_suggestion(SituationKind::Question, 6, 3);
        assert!(s.contains("回环"));
        let p = observe_suggestion(SituationKind::Permission, 3, 3);
        assert!(p.contains("本次允许"));
    }

    // ───────────── 真实屏幕回归（评审探针里的帧）─────────────

    fn cs(lines: &[&str], alt: bool) -> Situation {
        c(&snap(lines, alt))
    }

    #[test]
    fn claude_code_working_is_progress_not_question() {
        for (n, f) in [("box", &frames::CC_WORKING[..]), ("rules", &frames::CC_WORKING_RULES[..])] {
            for alt in [false, true] {
                let x = cs(f, alt);
                assert_eq!(x.program, Program::ClaudeCode, "{n}");
                assert_eq!(x.kind, SituationKind::Progress, "{n} alt={alt}");
            }
        }
    }

    #[test]
    fn claude_code_idle_input_box_is_awaiting_input() {
        for (n, f) in [
            ("box", &frames::CC_IDLE_BOX[..]),
            ("asks in prose", &frames::CC_IDLE_ASKS[..]),
            ("accept edits", &frames::CC_IDLE_ACCEPT_EDITS[..]),
        ] {
            let x = cs(f, false);
            assert_eq!(x.program, Program::ClaudeCode, "{n}");
            assert_eq!(x.kind, SituationKind::AwaitingInput, "{n}: {:?}", x.evidence);
        }
    }

    #[test]
    fn claude_code_dialogs_are_permission_with_options() {
        let x = cs(&frames::CC_BASH_PERM, false);
        assert_eq!(x.kind, SituationKind::Permission);
        assert_eq!(x.options.len(), 3);
        assert!(x.options[1].grants_permanent && !x.options[0].grants_permanent);

        let x = cs(&frames::CC_EDIT_PERM, false);
        assert_eq!(x.program, Program::ClaudeCode);
        assert_eq!(x.kind, SituationKind::Permission);
        assert_eq!(x.options.len(), 3);
        assert!(!x.options[0].grants_permanent);
        assert!(x.options[1].grants_permanent, "allow all edits during this session 是永久授权类");
        assert!(!x.options[2].grants_permanent);

        let x = cs(&frames::CC_CREATE_PERM, false);
        assert_eq!(x.kind, SituationKind::Permission);
        assert_eq!(x.options.len(), 3);

        let x = cs(&frames::CC_TRUST, false);
        assert_eq!(x.kind, SituationKind::Permission);
        assert_eq!(x.options.len(), 2);
        assert!(x.options.iter().all(|o| !o.grants_permanent));
    }

    #[test]
    fn footer_lines_are_not_questions() {
        assert!(question_line(&["● Done", "  ? for shortcuts"]).is_none());
        assert!(question_line(&["Esc to cancel · ? for help"]).is_none());
        assert!(question_line(&["⏵⏵ accept edits on (shift+tab to cycle) ?"]).is_none());
    }

    #[test]
    fn echoed_prompt_above_running_script_is_not_a_shell() {
        let x = cs(&frames::SCRIPT_RUNNING, false);
        assert_ne!(x.program, Program::Shell);
        assert!(!shell_visible(&x), "{:?}", x.evidence);
        assert_ne!(x.kind, SituationKind::Idle);
        // 同一个回显行在最后一行（命令刚敲出去、还没输出）也不是「停在提示符上」
        let y = cs(&["user@h:~/p$ ./long.sh"], false);
        assert!(!shell_visible(&y));
        // 真正回到提示符才算
        let z = cs(&["step 10 of 10 done", "user@h:~/p$ "], false);
        assert!(shell_visible(&z));
        assert_eq!(z.kind, SituationKind::Idle);
    }

    #[test]
    fn script_prompts_are_recognised_not_shell() {
        let yn = cs(&frames::SCRIPT_YN, false);
        assert!(!shell_visible(&yn));
        assert!(matches!(yn.kind, SituationKind::Permission | SituationKind::Question), "{:?}", yn.kind);

        let menu = cs(&frames::SCRIPT_MENU, false);
        assert!(!shell_visible(&menu));
        assert_eq!(menu.kind, SituationKind::Menu);

        let pw = cs(&frames::SUDO_PW, false);
        assert_eq!(pw.kind, SituationKind::Password);
        assert!(!shell_visible(&pw));
    }

    #[test]
    fn free_text_prompt_is_awaiting_input() {
        let x = cs(&frames::SCRIPT_DIR_PROMPT, false);
        assert_eq!(x.kind, SituationKind::AwaitingInput, "{:?}", x.evidence);
        assert!(x.evidence.iter().any(|e| e == FREE_TEXT_PROMPT));
        assert!(!shell_visible(&x));
        let y = cs(&frames::SCRIPT_PRESS_ENTER, false);
        // 「按回车继续」prompt.rs 判成 Pager；无论哪种，nx_watch 都会叫醒模型
        assert!(matches!(y.kind, SituationKind::Pager | SituationKind::AwaitingInput), "{:?}", y.kind);
        assert!(!shell_visible(&y));
        // 普通输出行不是提示
        assert!(!free_text_prompt("  CC foo.o"));
        assert!(!free_text_prompt("│ Continue? │"));
        assert!(free_text_prompt("Name: "));
        assert!(free_text_strong("Enter installation directory [/opt/app]: "));
        // 一个词的标签行是日志，不是提示；`Name>` 也不再算
        assert!(!free_text_prompt("Summary:"));
        assert!(!free_text_prompt("Fetching: "));
        assert!(!free_text_prompt("Name> "));
        assert!(!free_text_prompt(">"));
    }

    #[test]
    fn shell_prompt_with_typed_text_shapes() {
        assert!(shell_prompt_with_text("user@h:~/p$ rm -rf /tmp/zz"));
        assert!(shell_prompt_with_text("(venv) [root@h ~]# ls"));
        assert!(shell_prompt_with_text("PS C:\\Users\\x> Remove-Item -Recurse foo"));
        assert!(shell_prompt_with_text("$ ./build.sh"));
        assert!(!shell_prompt_with_text("user@h:~/p$ "));
        assert!(!shell_prompt_with_text("echo a > f"));
        assert!(!shell_prompt_with_text("# a markdown heading"));
        assert!(!shell_prompt_with_text("Installing packages... 45%"));
    }

    // ───────────── clean_line 不再把 `>` 当框线（REPL / whiptail）─────────────

    #[test]
    fn clean_line_keeps_repl_prompts_and_buttons() {
        assert_eq!(clean_line(">>> "), ">>>");
        assert_eq!(clean_line("mysql> "), "mysql>");
        assert_eq!(clean_line("> "), ">");
        assert_eq!(clean_line("│        <Ok>        │"), "<Ok>");
        assert_eq!(clean_line("irb(main):001:0> "), "irb(main):001:0>");
        // 选中标记只在行首、后面跟选项时才剥
        assert_eq!(clean_line("│ ❯ 1. Yes      │"), "1. Yes");
        assert_eq!(clean_line("> 2. No"), "2. No");
        assert_eq!(clean_line("› Refactor now"), "Refactor now");
        assert_eq!(clean_line("> quoted reply text"), "> quoted reply text");
        assert_eq!(clean_line("PS C:\\Users\\x> "), "PS C:\\Users\\x>");
    }

    #[test]
    fn repl_prompts_are_awaiting_input() {
        for (name, frame) in [
            ("python", &["$ python3", "Python 3.12.1 (main) [GCC 13.2.0] on linux", "Type \"help\" for more information.", ">>> "][..]),
            ("mysql", &["Welcome to the MySQL monitor.  Commands end with ; or \\g.", "", "mysql> "][..]),
            ("irb", &["$ irb", "irb(main):001:0> "][..]),
            ("node", &["Welcome to Node.js v22.1.0.", "Type \".help\" for more information.", "> "][..]),
        ] {
            let x = cs(frame, false);
            assert_eq!(x.kind, SituationKind::AwaitingInput, "{name}: {:?}", x.evidence);
            assert!(x.evidence.iter().any(|e| e == "prompt:repl"), "{name}: {:?}", x.evidence);
            assert!(!shell_visible(&x), "{name}");
        }
    }

    #[test]
    fn whiptail_ok_only_dialog_is_menu() {
        let x = cs(
            &[
                "┌──────────────┤ Installer ├──────────────┐",
                "│                                         │",
                "│ Installation finished successfully.     │",
                "│                                         │",
                "│                 <Ok>                    │",
                "│                                         │",
                "└─────────────────────────────────────────┘",
            ],
            true,
        );
        assert_eq!(x.kind, SituationKind::Menu, "{:?}", x.evidence);
        assert!(x.tail.contains("<Ok>"));
    }

    // ───────────── 提交方式：数字即提交 vs 编号 + 回车 ─────────────

    #[test]
    fn selector_menus_submit_on_digit_line_prompts_need_enter() {
        for f in [&frames::CC_BASH_PERM[..], &frames::CC_EDIT_PERM[..], frames::CC_ASK_MENU, frames::CC_PLAN_APPROVAL] {
            let x = cs(f, false);
            assert_eq!(submit_style(&x), SubmitStyle::DigitSubmits, "{:?}", x.evidence);
        }
        let menu = cs(&frames::SCRIPT_MENU, false);
        assert_eq!(menu.kind, SituationKind::Menu);
        assert_eq!(submit_style(&menu), SubmitStyle::NeedsEnter);
        let yn = cs(&frames::SCRIPT_YN, false);
        assert_eq!(submit_style(&yn), SubmitStyle::NeedsEnter);
    }

    #[test]
    fn inquirer_arrow_menu_is_menu_with_arrow_select() {
        let x = cs(
            &[
                "$ npm create vite@latest",
                "? Pick a framework: (Use arrow keys)",
                "❯ React",
                "  Vue",
                "  Svelte",
            ],
            false,
        );
        assert_eq!(x.kind, SituationKind::Menu, "{:?}", x.evidence);
        assert_eq!(submit_style(&x), SubmitStyle::ArrowSelect);
        let texts: Vec<&str> = x.options.iter().map(|o| o.text.as_str()).collect();
        assert_eq!(texts, ["React", "Vue", "Svelte"]);
        assert!(x.options.iter().all(|o| o.index.is_none()));
        // 没有方向键提示的 `> 文字`（引用、REPL 回显）不是方向键菜单
        let y = cs(&["> some quoted text", "  more text", "  even more"], false);
        assert_ne!(submit_style(&y), SubmitStyle::ArrowSelect);
    }

    #[test]
    fn inquirer_cursor_index_and_arrow_hint() {
        let x = cs(
            &[
                "? Pick a framework: (Use arrow keys)",
                "  React",
                "  Vue",
                "❯ Svelte",
                "  Solid",
            ],
            false,
        );
        assert_eq!(submit_style(&x), SubmitStyle::ArrowSelect);
        assert_eq!(menu_cursor(&x), Some(2), "{:?}", x.evidence);
        let h = arrow_hint(&x).unwrap();
        assert!(h.contains("光标在第 3 项「Svelte」") && h.contains("<Down*k><Enter>"), "{h}");
        // 不是方向键菜单就没有提示
        let y = cs(&["$ ls", "a b", "$ "], false);
        assert!(arrow_hint(&y).is_none());
    }

    #[test]
    fn cursor_row_is_the_current_line() {
        let lines = ["$ ./setup", "Project name: ", "", "  esc cancel · enter submit · ctrl+c quit"];
        let mut snap = ScreenSnapshot {
            console_id: "t".into(),
            frame_id: 1,
            window_start: 0,
            hash: "h".into(),
            alt: false,
            cols: 80,
            rows: 4,
            cursor_x: 14,
            cursor_y: 1,
            app_cursor: false,
            lines: lines.iter().map(|x| x.to_string()).collect(),
            updated_at: 1_000,
            idle_frames: 0,
        };
        assert_eq!(cursor_index(&snap), Some(1));
        let (text, alt) = frame_text(&snap);
        assert!(text.ends_with("Project name: ") || text.ends_with("Project name:"), "{text:?}");
        assert!(!alt);
        let with_cursor = classify(&snap, 1_100, 6_000);
        assert_eq!(with_cursor.kind, SituationKind::AwaitingInput, "{:?}", with_cursor.evidence);
        // 帧没带光标（0,0）：按最后一个非空行（状态栏）认，退回原来的行为
        snap.cursor_x = 0;
        snap.cursor_y = 0;
        assert_eq!(cursor_index(&snap), None);
        assert!(frame_text(&snap).0.ends_with("ctrl+c quit"));
    }

    fn sig_sit(kind: SituationKind, topic: &str) -> Situation {
        Situation {
            frame_id: 1,
            screen_hash: "h".into(),
            program: Program::Unknown,
            kind,
            topic: Some(topic.into()),
            options: vec![],
            confidence: 0.9,
            tail: String::new(),
            evidence: vec![],
            stale_ms: 0,
            signature: Situation::signature_of(Program::Unknown, kind, Some(topic), &[]),
        }
    }

    #[test]
    fn prompt_answered_needs_a_different_prompt_not_a_spinner() {
        let q = sig_sit(SituationKind::Question, "Continue? [y/N]");
        // 同一提问还在（只是屏幕别处的转圈在跳：签名不变）
        let mut spinner = q.clone();
        spinner.screen_hash = "h2".into();
        spinner.tail = "⠙ working".into();
        assert!(!prompt_answered(&q, &spinner));
        // 进到了下一个提问
        assert!(prompt_answered(&q, &sig_sit(SituationKind::Question, "Overwrite config? [y/N]")));
        // 回到 shell / 开始跑了
        assert!(prompt_answered(&q, &sig_sit(SituationKind::Progress, "")));
        // 没有签名可比（旁路分类）且种类没变：当作提示仍在
        let mut raw = q.clone();
        raw.signature.clear();
        assert!(!prompt_answered(&raw, &raw.clone()));
        // 本来就不是提示：无从核对，算过去了
        assert!(prompt_answered(&sig_sit(SituationKind::Idle, ""), &sig_sit(SituationKind::Idle, "")));
    }

    // ───────────── 误判修正 ─────────────

    #[test]
    fn ufw_status_above_shell_prompt_is_idle_not_permission() {
        let x = cs(
            &[
                "$ sudo ufw status",
                "Status: active",
                "",
                "To                         Action      From",
                "--                         ------      ----",
                "22/tcp                     ALLOW       Anywhere",
                "80/tcp                     DENY        Anywhere",
                "alice@host:~$ ",
            ],
            false,
        );
        assert_eq!(x.kind, SituationKind::Idle, "{:?}", x.evidence);
        // allow / deny 在 8 行内但不在光标附近，也不算授权
        let y = cs(
            &[
                "22/tcp ALLOW Anywhere",
                "80/tcp DENY Anywhere",
                "rule a",
                "rule b",
                "rule c",
                "still running step 4",
            ],
            false,
        );
        assert_ne!(y.kind, SituationKind::Permission);
        // 真正的 allow / deny 提示还在
        let z = cs(&["Grant camera access?", "   < Allow >     < Deny >"], false);
        assert_eq!(z.kind, SituationKind::Permission, "{:?}", z.evidence);
    }

    #[test]
    fn weak_question_mid_build_is_not_permission() {
        let x = cs(&["Compiling foo v0.1.0", "warning: unused variable", "note: should the linker proceed?"], false);
        assert_ne!(x.kind, SituationKind::Permission, "{:?}", x.evidence);
        let y = cs(&["step 3 done", "Continue?"], false);
        assert_eq!(y.kind, SituationKind::AwaitingInput, "{:?}", y.evidence);
        // 带 (y/n) 记号的仍是授权
        let z = cs(&["Overwrite config? (y/n) "], false);
        assert_eq!(z.kind, SituationKind::Permission);
    }

    #[test]
    fn npm_init_default_prompts_are_awaiting_input_with_default() {
        let x = cs(&["This utility will walk you through creating a package.json file.", "package name: (myapp) "], false);
        assert_eq!(x.kind, SituationKind::AwaitingInput, "{:?}", x.evidence);
        assert_eq!(x.topic.as_deref(), Some("input:default=myapp"));
        assert!(x.evidence.iter().any(|e| e == "prompt:default-offered"));
        let y = cs(&["  \"license\": \"ISC\"", "}", "", "", "Is this OK? (yes) "], false);
        assert_eq!(y.kind, SituationKind::AwaitingInput, "{:?}", y.evidence);
        assert_eq!(y.topic.as_deref(), Some("input:default=yes"));
        assert_eq!(offered_default("version: (1.0.0) ").as_deref(), Some("1.0.0"));
        assert!(offered_default("(venv) user@h:~$").is_none());
        assert!(offered_default("done (3 files)").is_none());
    }

    #[test]
    fn git_pager_on_normal_screen_needs_static_frames() {
        let lines = [
            "commit 3f2a1b0c9d8e7f6a5b4c3d2e1f0a9b8c7d6e5f4a",
            "Author: Dev <dev@example.com>",
            "Date:   Mon Nov 9 10:00:00 2026 +0800",
            "",
            "    fix(agent): tighten prompt rules",
            "",
            "commit 1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b",
            ":",
        ];
        let mut s = snap(&lines, false);
        assert_ne!(c(&s).kind, SituationKind::Pager, "屏幕刚变化时不认");
        s.idle_frames = STATIC_FRAMES;
        let x = c(&s);
        assert_eq!(x.kind, SituationKind::Pager, "{:?}", x.evidence);
        assert_eq!(x.program, Program::Pager);
    }

    #[test]
    fn material_change_detection() {
        let a = cs(&frames::CC_BASH_PERM, false);
        assert!(materially_changed(&a, &a.clone()).is_none());
        let b = cs(&frames::CC_IDLE_BOX, false);
        assert!(materially_changed(&a, &b).is_some());
        // 同为 shell，但提示符上敲的字变了
        let p1 = cs(&["user@h:~/p$ ls"], false);
        let p2 = cs(&["user@h:~/p$ rm -rf /"], false);
        assert!(materially_changed(&p1, &p2).is_some());
        // 进度条每帧都变，但局面没变
        let g1 = cs(&["⠋ Thinking 12%", "esc to interrupt"], true);
        let g2 = cs(&["⠙ Thinking 34%", "esc to interrupt"], true);
        assert!(materially_changed(&g1, &g2).is_none());
    }
}
