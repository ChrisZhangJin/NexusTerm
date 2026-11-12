//! `nx_watch` 的纯判定逻辑：盯着一个长任务，直到「值得模型回来看一眼」。
//!
//! 与 `nx_wait` 的区别：wait 等一段文字或静止，模型得自己反复轮询；watch 一次挂住，
//! 由屏幕局面（`situation::classify`）自己决定什么时候该叫醒模型 ——
//! 要人 / 要答复（提问、授权、密码、菜单、等输入、报错）、回到 shell、命中 `until`、
//! 屏幕长时间没变、超时。
//!
//! 本模块不碰 Tauri / pty / 时钟，只做判定与反馈文本，便于脱离运行时单测。

use super::act::head_tail_chars;
use super::situation::{self, Program, Situation, SituationKind, SubmitStyle};

/// 默认最长盯多久（毫秒）。
pub const DEFAULT_TIMEOUT_MS: u64 = 600_000;
/// 最长盯多久的上限（毫秒）。
pub const MAX_TIMEOUT_MS: u64 = 3_600_000;
/// 默认「屏幕多久没变就算卡住」（毫秒）。
pub const DEFAULT_STALL_MS: u64 = 120_000;
/// 轮询间隔。
pub const POLL_MS: u64 = 1_000;
/// 刚开始盯的前几秒不判「回到 shell / 无变化」：发命令与屏幕刷新之间有延迟，
/// 立刻看到旧提示符会把「命令刚发出」误判成「已结束」。
pub const SETTLE_MS: u64 = 3_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchCfg {
    pub timeout_ms: u64,
    pub stall_ms: u64,
}

impl WatchCfg {
    /// 把模型给的参数夹进合法域。
    pub fn from_args(timeout_ms: Option<u64>, stall_ms: Option<u64>) -> Self {
        Self {
            timeout_ms: timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).clamp(1_000, MAX_TIMEOUT_MS),
            stall_ms: stall_ms.unwrap_or(DEFAULT_STALL_MS).clamp(5_000, MAX_TIMEOUT_MS),
        }
    }
}

/// 为什么停下来叫醒模型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// 屏幕上出现了需要答复 / 处理的局面（提问、授权、密码、菜单、等输入、报错）
    NeedsAttention(SituationKind),
    /// 回到了 shell 提示符：任务多半跑完了
    ShellPrompt,
    /// `until` 命中
    Matched,
    /// 屏幕很久没变化
    Stalled,
    /// 到时间了
    Timeout,
    /// 被取消
    Cancelled,
    /// 用户点了「暂停」：先交还给主循环去停
    Paused,
}

impl Reason {
    pub fn label(self) -> String {
        match self {
            Self::NeedsAttention(k) => format!("需要处理：局面为 {}", k.as_str()),
            Self::ShellPrompt => "已回到 shell 提示符（任务多半已结束）".into(),
            Self::Matched => "until 已命中".into(),
            Self::Stalled => "屏幕长时间没有变化（可能卡住了）".into(),
            Self::Timeout => "到达最长等待时间，任务可能仍在运行".into(),
            Self::Cancelled => "已被用户取消".into(),
            Self::Paused => "用户请求暂停，先停在这里".into(),
        }
    }
}

/// 局面是否需要模型介入。`Progress` / `SubagentStatus` 之外的「在等人」类都算
/// （脚本里的「按回车继续」会被判成 Pager，同样得叫醒模型）。
fn needs_attention(kind: SituationKind) -> bool {
    matches!(
        kind,
        SituationKind::Question
            | SituationKind::Permission
            | SituationKind::Password
            | SituationKind::Menu
            | SituationKind::AwaitingInput
            | SituationKind::Pager
            | SituationKind::Error
    )
}

/// 一次轮询的判定。`None` = 继续盯。
///
/// - `kind` / `program`：此刻屏幕的局面；
/// - `shell_visible`：屏幕是否停在 shell 提示符（见 `situation::shell_visible`）；
/// - `changed_ago_ms`：屏幕内容距上次变化过了多久；
/// - `elapsed_ms`：已经盯了多久（不含暂停的时间）。
///
/// 顺序即优先级：要人处理 > 回到 shell > 超时 > 卡住。
pub fn watch_verdict(
    kind: SituationKind,
    program: Program,
    shell_visible: bool,
    changed_ago_ms: u64,
    elapsed_ms: u64,
    cfg: &WatchCfg,
) -> Option<Reason> {
    let settled = elapsed_ms >= SETTLE_MS;
    // 屏幕上留着一行旧报错、末行却已是 shell 提示符：任务已经结束，报错在尾部里。
    // 不然「再盯一次」会因为那行旧报错立刻又被叫醒，永远盯不住。
    if settled && kind == SituationKind::Error && shell_visible && program == Program::Shell {
        return Some(Reason::ShellPrompt);
    }
    // 「等输入」（Claude Code 空输入框、`目录 [/opt/app]: ` 这类自由文本提示）形状上也像任务
    // 刚发出时的瞬间画面或输出里的一行冒号结尾的话：屏幕要静止一阵才算真的在等人。
    // 弱线索（构建输出里一句问号结尾的话、没有 `(y/n)` 的 `Continue?`）在 situation 里也归到
    // AwaitingInput 而不是 Permission，所以同样要静止才叫醒
    let static_ok = kind != SituationKind::AwaitingInput || changed_ago_ms >= SETTLE_MS;
    if needs_attention(kind) && static_ok && (settled || kind == SituationKind::Password) {
        return Some(Reason::NeedsAttention(kind));
    }
    if settled && (kind == SituationKind::Idle || (shell_visible && program == Program::Shell)) {
        return Some(Reason::ShellPrompt);
    }
    if elapsed_ms >= cfg.timeout_ms {
        return Some(Reason::Timeout);
    }
    if settled && changed_ago_ms >= cfg.stall_ms {
        return Some(Reason::Stalled);
    }
    None
}

/// 保留尾部的截断（长任务的结局在屏幕最下面）。
fn head(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

/// 组装喂回模型的反馈。`sit` 的 `tail` 已脱敏（密码提示行连原文都不留）；
/// 没有局面时用 `fallback_tail`（调用方保证已脱敏或会过出网闸门）。`extra` 是命中 `until` 时的新输出。
/// **整段**（表头 + 选项 + 提示 + 屏幕尾部 + extra）不超过 `max_chars`。
pub fn render_feedback(
    label: &str,
    reason: Reason,
    elapsed_ms: u64,
    sit: Option<&Situation>,
    fallback_tail: &str,
    extra: &str,
    max_chars: usize,
) -> String {
    let mut s = format!(
        "已盯会话「{}」{} 秒，停下原因：{}\n",
        head(label, 60),
        elapsed_ms / 1000,
        reason.label()
    );
    let mut tail = fallback_tail.trim().to_string();
    if let Some(x) = sit {
        s.push_str(&format!(
            "局面：kind={} program={} confidence={:.2}\n",
            x.kind.as_str(),
            x.program.as_str(),
            x.confidence
        ));
        if let Some(t) = &x.topic {
            s.push_str(&format!("主题：{}\n", head(t, 200)));
        }
        if !x.options.is_empty() {
            s.push_str("选项：\n");
            for o in x.options.iter().take(12) {
                s.push_str(&format!(
                    "  {}. {}{}\n",
                    o.index.as_deref().unwrap_or("-"),
                    head(&o.text, 120),
                    if o.grants_permanent { "（会永久扩大授权，不要选）" } else { "" }
                ));
            }
            // 选中之后怎么提交：多发一个回车会落到下一屏（可能是下一张权限卡）
            match situation::submit_style(x) {
                SubmitStyle::DigitSubmits => s.push_str("提交方式：按编号即选中并提交，只发数字，不要再加 <Enter>。\n"),
                SubmitStyle::ArrowSelect => s.push_str("提交方式：方向键菜单，输入编号无效；用 <Up>/<Down> 移到目标项再 <Enter>。\n"),
                SubmitStyle::NeedsEnter => {}
            }
        }
        tail = x.tail.trim().to_string();
    }
    s.push_str(hint_for(reason));
    s.push_str("屏幕尾部：\n");
    let extra = extra.trim();
    // 表头已占的长度之外，剩下的给尾部与 extra（至少留一点，免得头太长时什么都不剩）
    let room = max_chars.saturating_sub(s.chars().count() + 40).max(200);
    let (tail_room, extra_room) = if extra.is_empty() { (room, 0) } else { (room / 2, room - room / 2) };
    // 首尾都留（报错常在开头、结论在结尾），中间省略并标明字数
    s.push_str(&head_tail_chars(&tail, tail_room));
    s.push('\n');
    if !extra.is_empty() {
        s.push_str("命中期间的新输出（尾部）：\n");
        s.push_str(&head_tail_chars(extra, extra_room.saturating_sub(60).max(100)));
        s.push('\n');
    }
    s
}

fn hint_for(reason: Reason) -> &'static str {
    match reason {
        Reason::NeedsAttention(SituationKind::Password) => {
            "提示：这是密码 / 口令提示：不要用 nx_send_keys 输入，请调用 nx_secret 让用户本人输入。\n"
        }
        Reason::NeedsAttention(SituationKind::Permission) => {
            "提示：授权确认：只选「本次允许」类选项，绝不选永久授权（don't ask again / always）。\n"
        }
        Reason::NeedsAttention(_) => "提示：按屏幕上的局面用 nx_send_keys 作答；拿不准先 nx_screen。\n",
        Reason::ShellPrompt => "提示：确认结果后继续下一步或给出结论。\n",
        Reason::Stalled => "提示：先 nx_screen 看一眼；确实卡死可 <C-c> 打断后换办法，或询问用户。\n",
        Reason::Timeout => "提示：任务可能仍在运行；可以再次 nx_watch，或先 nx_screen 看进度。\n",
        Reason::Matched | Reason::Cancelled | Reason::Paused => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> WatchCfg {
        WatchCfg::from_args(Some(60_000), Some(30_000))
    }

    #[test]
    fn args_are_clamped() {
        let c = WatchCfg::from_args(Some(999_999_999), Some(1));
        assert_eq!(c.timeout_ms, MAX_TIMEOUT_MS);
        assert_eq!(c.stall_ms, 5_000);
        let d = WatchCfg::from_args(None, None);
        assert_eq!((d.timeout_ms, d.stall_ms), (DEFAULT_TIMEOUT_MS, DEFAULT_STALL_MS));
    }

    #[test]
    fn progress_keeps_watching() {
        let v = watch_verdict(SituationKind::Progress, Program::Script, false, 1_000, 10_000, &cfg());
        assert_eq!(v, None);
    }

    #[test]
    fn attention_kinds_wake_the_model() {
        for k in [
            SituationKind::Question,
            SituationKind::Permission,
            SituationKind::Menu,
            SituationKind::Pager,
            SituationKind::AwaitingInput,
            SituationKind::Error,
        ] {
            assert_eq!(
                watch_verdict(k, Program::ClaudeCode, false, 5_000, 10_000, &cfg()),
                Some(Reason::NeedsAttention(k))
            );
        }
    }

    #[test]
    fn awaiting_input_needs_a_static_screen() {
        // 屏幕刚变过：可能只是输出里一行冒号结尾的话 / 刚发出任务时的旧输入框
        assert_eq!(watch_verdict(SituationKind::AwaitingInput, Program::Script, false, 500, 10_000, &cfg()), None);
        assert_eq!(
            watch_verdict(SituationKind::AwaitingInput, Program::Script, false, SETTLE_MS, 10_000, &cfg()),
            Some(Reason::NeedsAttention(SituationKind::AwaitingInput))
        );
    }

    #[test]
    fn password_wakes_immediately_even_in_settle_window() {
        assert_eq!(
            watch_verdict(SituationKind::Password, Program::Ssh, false, 0, 100, &cfg()),
            Some(Reason::NeedsAttention(SituationKind::Password))
        );
    }

    #[test]
    fn settle_window_ignores_stale_idle_prompt() {
        // 刚发出命令，屏幕还停在旧提示符
        assert_eq!(watch_verdict(SituationKind::Idle, Program::Shell, true, 0, 500, &cfg()), None);
        assert_eq!(
            watch_verdict(SituationKind::Idle, Program::Shell, true, 0, 5_000, &cfg()),
            Some(Reason::ShellPrompt)
        );
    }

    #[test]
    fn old_error_above_a_shell_prompt_means_finished_not_attention() {
        assert_eq!(
            watch_verdict(SituationKind::Error, Program::Shell, true, 0, 5_000, &cfg()),
            Some(Reason::ShellPrompt)
        );
        // 程序里的报错（没回到 shell）仍然要叫醒
        assert_eq!(
            watch_verdict(SituationKind::Error, Program::Script, false, 0, 5_000, &cfg()),
            Some(Reason::NeedsAttention(SituationKind::Error))
        );
    }

    #[test]
    fn timeout_and_stall() {
        assert_eq!(
            watch_verdict(SituationKind::Progress, Program::Script, false, 0, 60_000, &cfg()),
            Some(Reason::Timeout)
        );
        assert_eq!(
            watch_verdict(SituationKind::Progress, Program::Script, false, 30_000, 40_000, &cfg()),
            Some(Reason::Stalled)
        );
        // 没到阈值
        assert_eq!(
            watch_verdict(SituationKind::Unknown, Program::Unknown, false, 29_000, 40_000, &cfg()),
            None
        );
    }

    #[test]
    fn feedback_is_bounded_and_keeps_the_tail() {
        let tail: String = (0..5000).map(|i| format!("line{i}\n")).collect();
        let fb = render_feedback("s1", Reason::Timeout, 61_000, None, &tail, "", 500);
        assert!(fb.contains("line4999"));
        assert!(fb.contains("line0\n"), "开头也保留");
        assert!(!fb.contains("line2500\n"));
        assert!(fb.chars().count() < 1_000);
        assert!(fb.contains("中间省略"));
    }

    #[test]
    fn whole_feedback_is_bounded_with_extra_and_many_options() {
        use crate::agent::situation::Choice;
        let sit = Situation {
            frame_id: 1,
            screen_hash: String::new(),
            program: Program::ClaudeCode,
            kind: SituationKind::Menu,
            topic: Some("t".repeat(5000)),
            options: (0..50)
                .map(|i| Choice { index: Some(i.to_string()), text: "o".repeat(400), grants_permanent: false })
                .collect(),
            confidence: 0.9,
            tail: "z".repeat(9000),
            evidence: Vec::new(),
            stale_ms: 0,
            signature: String::new(),
        };
        let fb = render_feedback("s", Reason::Matched, 1000, Some(&sit), "", &"e".repeat(9000), 3000);
        assert!(fb.chars().count() <= 3000 + 600, "len={}", fb.chars().count());
        assert!(fb.contains("命中期间"));
    }

    #[test]
    fn weak_question_mid_build_needs_a_static_screen() {
        // 构建输出里一行问号结尾的话：situation 归为 AwaitingInput（不是 Permission）……
        let sit = situation::frames::classify_lines(&["Compiling foo v0.1.0", "warning: unused variable", "note: should the linker proceed?"]);
        assert_eq!(sit.kind, SituationKind::AwaitingInput, "{:?}", sit.evidence);
        // ……屏幕还在刷新时不叫醒
        assert_eq!(watch_verdict(sit.kind, sit.program, false, 500, 10_000, &cfg()), None);
        // 静止下来才算在等人
        assert_eq!(
            watch_verdict(sit.kind, sit.program, false, SETTLE_MS, 10_000, &cfg()),
            Some(Reason::NeedsAttention(SituationKind::AwaitingInput))
        );
        // ufw 的 allow / deny 输出上面是 shell 提示符：回到 shell，不是授权
        let ufw = situation::frames::classify_lines(&["22/tcp ALLOW Anywhere", "80/tcp DENY Anywhere", "alice@host:~$ "]);
        assert_eq!(
            watch_verdict(ufw.kind, ufw.program, situation::shell_visible(&ufw), 0, 10_000, &cfg()),
            Some(Reason::ShellPrompt)
        );
    }

    #[test]
    fn feedback_tells_how_to_submit_the_choice() {
        let cc = situation::frames::classify_lines(&situation::frames::CC_BASH_PERM);
        let fb = render_feedback("s", Reason::NeedsAttention(cc.kind), 1000, Some(&cc), "", "", 3000);
        assert!(fb.contains("只发数字"), "{fb}");
        let sh = situation::frames::classify_lines(&situation::frames::SCRIPT_MENU);
        let fb = render_feedback("s", Reason::NeedsAttention(sh.kind), 1000, Some(&sh), "", "", 3000);
        assert!(!fb.contains("提交方式"), "{fb}");
    }
}
