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
fn clean_line(s: &str) -> String {
    const FRAME: &str = "│┃╭╰╮╯─═╔╗╚╝┌┐└┘❯›>";
    s.trim()
        .trim_start_matches(|c: char| FRAME.contains(c) || c.is_whitespace())
        .trim_end_matches(|c: char| FRAME.contains(c) || c.is_whitespace())
        .trim()
        .to_string()
}

fn detect_program(lower: &str, alt: bool, guess: PromptKind) -> Program {
    if lower.contains("claude code")
        || lower.contains("claude-code")
        || lower.contains("esc to interrupt")
        || lower.contains("? for shortcuts")
        || lower.contains("don't ask again")
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
    if guess == PromptKind::ShellPrompt || lower.contains("$ ") || lower.contains("➜ ") {
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
        let l = text.to_lowercase();
        let grants_permanent = l.contains("don't ask again")
            || l.contains("don’t ask again")
            || l.contains("always")
            || l.contains("permanent")
            || l.contains("永久")
            || l.contains("不再询问")
            || l.contains("不再提示");
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

fn question_line(lines: &[&str]) -> Option<String> {
    lines.iter().rev().take(RECENT_LINES).rev().find_map(|l| {
        let t = clean_line(l);
        if t.is_empty() {
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

fn rule(lines: &[&str], alt: bool, guess: PromptGuess, program: Program) -> Draft {
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
    let q = question_line(lines);

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
    let looks_permission = recent.contains("do you want to proceed")
        || recent.contains("don't ask again")
        || recent.contains("don’t ask again")
        || recent.contains("yes, and ")
        || (recent.contains("allow") && recent.contains("deny"))
        || (recent.contains("permission") && (recent.contains("allow") || recent.contains("grant")))
        || guess.kind == PromptKind::YesNo;
    if looks_permission {
        let options = parse_options(lines);
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
    let start = snap.lines.len().saturating_sub(TAIL_LINES);
    let cleaned: Vec<String> = snap.lines[start..]
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
    let guess = prompt::classify(&text, snap.alt);
    let program = detect_program(&lower, snap.alt, guess.kind);
    let mut d = rule(&non_empty, snap.alt, guess, program);
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
            cursor_y: 0,
            lines: v,
            updated_at: 1_000,
            idle_frames: 0,
        }
    }

    fn c(s: &ScreenSnapshot) -> Situation {
        classify(s, 1_100, 6_000)
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
}
