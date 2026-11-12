//! 托管模式（`agent_run` 的 `mode:"steward"`）下的按键闸门与提示词。
//!
//! 托管 = 对话 Agent 的 LLM 循环全自动跑，没人逐步点头，所以闸门要比「自动执行」更聪明：
//! - 在 shell 提示符上按回车 → 评估键入的那一行（与 Exec 同一套 `danger::assess`），危险就审批；
//! - 在程序的提示里（Claude Code / 脚本问答 / TUI）→ 放行，按键本来就是答复；
//! - 授权对话框里选了「永久授权」类选项 → 拒绝并告诉模型改选「仅本次」；
//! - 口令提示 → 拒绝，让模型走 `nx_secret`（密码永远不进模型）。
//!
//! 纯逻辑，不碰运行时，便于单测。

use super::danger::assess;
use super::situation::{shell_visible, Program, Situation, SituationKind};

/// 托管模式默认预算：步数。
pub const STEWARD_MAX_STEPS: u32 = 400;
/// 托管模式默认预算：分钟。
pub const STEWARD_MAX_MINUTES: u32 = 360;
/// 托管模式下人工审批的等待上限（秒）：人可能不在电脑前，比普通运行的 300s 长得多。
pub const STEWARD_APPROVAL_TIMEOUT_SECS: u64 = 30 * 60;

/// 按键闸门的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeysVerdict {
    /// 放行
    Allow,
    /// 需要人批准，附原因
    Approve(Vec<String>),
    /// 拒绝并把这段话喂回模型
    Refuse(String),
}

/// 方向键 / Shift-Tab 的 ESC 序列：在授权对话框里它们会移动或切换选项，我们看不到光标停在哪。
fn has_navigation(bytes: &[u8]) -> bool {
    const SEQS: [&[u8]; 6] = [b"\x1b[A", b"\x1b[B", b"\x1b[Z", b"\x1bOA", b"\x1bOB", b"\x1b[1;2Z"];
    SEQS.iter().any(|s| bytes.windows(s.len()).any(|w| w == *s))
}

fn has_history_nav(bytes: &[u8]) -> bool {
    const SEQS: [&[u8]; 4] = [b"\x1b[A", b"\x1b[B", b"\x1bOA", b"\x1bOB"];
    SEQS.iter().any(|s| bytes.windows(s.len()).any(|w| w == *s))
}

/// 按键里的可见文本（去掉控制字符与 ESC 序列），按回车分行。
pub(crate) fn typed_lines(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' | '\n' => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
            }
            '\x1b' => {
                // 跳过 CSI / SS3 序列：ESC [ ... 终止字节 或 ESC O x
                match chars.peek() {
                    Some('[') => {
                        chars.next();
                        while let Some(&n) = chars.peek() {
                            chars.next();
                            if ('\x40'..='\x7e').contains(&n) {
                                break;
                            }
                        }
                    }
                    Some('O') => {
                        chars.next();
                        chars.next();
                    }
                    _ => {}
                }
            }
            c if c.is_control() => {}
            c => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

pub(crate) fn strip_esc(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            match it.peek() {
                Some('[') => {
                    it.next();
                    while let Some(&n) = it.peek() {
                        it.next();
                        if ('\x40'..='\x7e').contains(&n) {
                            break;
                        }
                    }
                }
                Some('O') => {
                    it.next();
                    it.next();
                }
                _ => {}
            }
        } else if !c.is_control() {
            out.push(c);
        }
    }
    out
}

/// 按回车切开的片段（不含控制字符 / ESC 序列）。最后一段是最后一个回车之后、尚未提交的文字。
fn typed_segments(bytes: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    for &b in bytes {
        if b == b'\r' || b == b'\n' {
            out.push(strip_esc(&String::from_utf8_lossy(&cur)));
            cur.clear();
        } else {
            cur.push(b);
        }
    }
    out.push(strip_esc(&String::from_utf8_lossy(&cur)));
    out
}

/// 屏幕上 shell 提示符之后、光标之前已经敲着的那段（来自局面尾部的最后一个非空行）。
/// `None` = 读不出来（没尾部、认不出提示符）。`Some("")` = 提示符后是空的。
pub fn current_input_line(sit: &Situation) -> Option<String> {
    let line = sit.tail.lines().rev().find(|l| !l.trim().is_empty())?.trim_end();
    if line.starts_with("[redacted") {
        return None;
    }
    const MARKERS: [&str; 6] = ["$ ", "# ", "% ", "❯ ", "➜ ", "> "];
    let padded = format!("{line} ");
    let Some(at) = MARKERS.iter().filter_map(|m| padded.find(m).map(|i| (i, m.len()))).min() else {
        // 清洗时行尾的 `>` 被当框线抹掉了（`PS C:\\x>` / `u@h ~>`）：光标行是空提示符
        let on_prompt = sit.evidence.iter().any(|e| e == "shell:prompt-visible" || e == "prompt:shell_prompt");
        return on_prompt.then(String::new);
    };
    Some(padded[at.0 + at.1..].trim().to_string())
}

/// 屏幕末行是不是「shell 提示符 + 已敲的文字」。判成程序提示的局面里，这一行也得当 shell 行评估。
fn tail_has_shell_prompt(s: &Situation) -> bool {
    s.tail
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map_or(false, super::situation::shell_prompt_with_text)
}

/// 会让「最终被提交的那一行」变得不可知的编辑键：Tab 补全、Ctrl-R 反查、方向键翻历史。
fn has_opaque_edit(bytes: &[u8]) -> bool {
    bytes.contains(&0x09) || bytes.contains(&0x12) || has_history_nav(bytes)
}

/// 键入文字算不算「不止一个短答复」：长度 > 3 或含空格。程序提示里短答（y / 2 / q）直接放行，
/// 更长的文字还要过一遍危险评估，免得判错局面时 `rm -rf /<Enter>` 蒙混过去。
fn is_long_text(line: &str) -> bool {
    line.chars().count() > 3 || line.contains(' ')
}

fn danger_reasons(lines: &[String]) -> Vec<String> {
    let mut reasons = Vec::new();
    for line in lines {
        let r = assess(line);
        if r.dangerous {
            for why in r.reasons {
                reasons.push(format!("`{line}`：{why}"));
            }
        }
    }
    reasons
}

/// 评估一次按键。`sit` 是此刻屏幕的局面（没有屏幕帧就传 `None`，按「像 shell」保守处理）。
///
/// `sit.frame_id == 0` 表示局面来自旁路缓冲的启发式分类（没有新鲜的屏幕帧）：
/// 它没有选项列表，认不出「永久授权」，所以对菜单 / 授权类的选择一律改为人工批准。
/// 单独的编号（1–2 位数字，可带回车）。
fn is_bare_digit(visible: &str) -> bool {
    let v = visible.trim();
    !v.is_empty() && v.len() <= 2 && v.chars().all(|c| c.is_ascii_digit())
}

/// 确定停在空闲的 shell 提示符上（数字敲上去只是命令行里的字符）。
fn confident_shell(s: &Situation) -> bool {
    s.kind == SituationKind::Idle && matches!(s.program, Program::Shell) && s.confidence >= 0.8
}

/// 一个「光选不写」的按键：单个数字（可带回车），或者只有回车。
fn is_bare_pick(visible: &str, enter: bool) -> bool {
    let v = visible.trim();
    (v.is_empty() && enter) || (!v.is_empty() && v.chars().all(|c| c.is_ascii_digit()) && v.len() <= 2)
}

pub fn assess_keys(bytes: &[u8], sit: Option<&Situation>) -> KeysVerdict {
    let enter = super::interact::contains_enter(bytes);
    let visible = strip_esc(&String::from_utf8_lossy(bytes));
    let has_text = !visible.trim().is_empty();

    if let Some(s) = sit {
        // 口令：不许往里敲可见字符（控制键如 <C-c> / <Esc> 取消仍可）
        if s.kind == SituationKind::Password && has_text {
            return KeysVerdict::Refuse(
                "屏幕停在密码 / 口令提示上，不能用 nx_send_keys 输入（密码不经过模型，也不会回显）。\
                 请调用 nx_secret 让用户本人输入；不想输入就发 <C-c> 取消。"
                    .into(),
            );
        }
        // 授权对话框：选中「永久授权」类选项 → 拒绝
        if matches!(s.kind, SituationKind::Permission | SituationKind::Menu)
            && s.options.iter().any(|o| o.grants_permanent)
        {
            if let Some(why) = permanent_pick(bytes, &visible, s) {
                return KeysVerdict::Refuse(why);
            }
        }
        // 授权对话框批准的是屏幕上那条命令（`Bash command` 下的 `rm -rf build/`、`rm: remove … 'x'?`），
        // 不是「1」这一个键：命令本身危险就要人点头，与 PolicyEngine 的 `approved_payload` 同一判据
        let dialog = matches!(
            s.kind,
            SituationKind::Permission | SituationKind::Question | SituationKind::Menu | SituationKind::AwaitingInput
        ) && !super::situation::shell_visible(s);
        if let Some(cmd) = dialog.then(|| super::policy::approved_payload(&s.tail)).flatten() {
            if !declines(bytes, &visible, s) {
                let lines: Vec<String> = cmd.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
                let reasons = danger_reasons(&lines);
                if !reasons.is_empty() {
                    return KeysVerdict::Approve(
                        reasons.into_iter().map(|r| format!("批准对话框里的命令 {r}")).collect(),
                    );
                }
            }
        }
        let digit = !has_navigation(bytes) && is_bare_digit(&visible);
        // 没有新鲜屏幕帧时单按一个数字：除非确定停在 shell 提示符上，否则可能正在给一个看不见的
        // 授权卡 / 菜单选项（含「永久允许」）。回车不在此列：「按回车继续」很常见，要照常能用
        if s.frame_id == 0 && digit && !confident_shell(s) {
            return KeysVerdict::Refuse(
                "没有新鲜的屏幕画面，看不出这个数字会选中什么（可能是授权卡上的「永久允许」）。先 nx_screen 看清选项再选。".into(),
            );
        }
        // 看不到选项（没有新鲜屏幕帧 / 没认出选项）时对授权卡、菜单直接按数字或回车：不知道选中的是什么
        // （可能正是「永久允许」），让模型先读屏。例外：有新鲜帧的菜单只是没认出选项，按编号交给人批准
        // （否则只能逐个方向键挪）
        if matches!(s.kind, SituationKind::Permission | SituationKind::Menu)
            && (s.frame_id == 0 || s.options.is_empty())
            && !has_navigation(bytes)
            && is_bare_pick(&visible, enter)
        {
            if s.kind == SituationKind::Menu && s.frame_id != 0 && digit {
                return KeysVerdict::Approve(vec![
                    "屏幕上有菜单，但没认出它的选项，按编号选择需要你确认选中的是哪一项".into(),
                ]);
            }
            return KeysVerdict::Refuse(
                "看不到这个授权卡 / 菜单的选项，直接按数字或回车可能选中「永久允许」一类的项。先 nx_screen 看清选项再选。".into(),
            );
        }
        // 没有新鲜屏幕帧：看不到选项，选择动作（数字 / 方向键 / 回车）交给人
        if s.frame_id == 0
            && matches!(s.kind, SituationKind::Permission | SituationKind::Menu)
            && (enter || has_navigation(bytes) || visible.trim().chars().any(|c| c.is_ascii_digit()))
        {
            return KeysVerdict::Approve(vec![
                "没有新鲜的屏幕帧，看不到菜单 / 授权对话框的选项（可能含「永久授权」），需要人确认这次选择".into(),
            ]);
        }
        // 程序自己的提示：短答复放行；较长的文字仍过危险评估
        if is_program_prompt(s) {
            if !enter {
                return KeysVerdict::Allow;
            }
            let mut lines = typed_lines(bytes);
            if is_ambiguous(s) || tail_has_shell_prompt(s) {
                // 可能是没认出的 shell 提示符：屏幕上已敲着的内容 + 本次键入合起来才是被提交的那行
                if has_opaque_edit(bytes) {
                    return KeysVerdict::Approve(vec![
                        "局面认不出是什么，按键里又有 Tab 补全 / Ctrl-R / 方向键翻历史，最终提交的那一行看不到".into(),
                    ]);
                }
                if let Some(cur) = current_input_line(s) {
                    let first = typed_segments(bytes).into_iter().next().unwrap_or_default();
                    let joined = format!("{cur}{first}").trim().to_string();
                    if !joined.is_empty() && !lines.first().map_or(false, |l| *l == joined) {
                        lines.insert(0, joined);
                    }
                }
            }
            let long: Vec<String> = lines.into_iter().filter(|l| is_long_text(l)).collect();
            let reasons = danger_reasons(&long);
            return if reasons.is_empty() { KeysVerdict::Allow } else { KeysVerdict::Approve(reasons) };
        }
    }

    // shell（或不知道是什么）：回车会提交一行。被提交的那行 = 屏幕上已敲着的 + 本次键入的，
    // 不能只看本次（"rm -rf ~/x" 和 "<Enter>" 分两次发就绕过去了）
    if !enter {
        return KeysVerdict::Allow;
    }
    let segs = typed_segments(bytes);
    let mut submitted: Vec<String> = segs[..segs.len() - 1].iter().map(|x| x.trim().to_string()).collect();
    let mut reasons = Vec::new();
    // 本次键入里 Ctrl-U / Ctrl-C 之前的现有内容不算数（行被清掉了）
    let clears = bytes.iter().position(|&b| b == 0x15 || b == 0x03);
    let first_enter = bytes.iter().position(|&b| b == b'\r' || b == b'\n');
    let line_cleared = matches!((clears, first_enter), (Some(c), Some(e)) if c < e);
    if has_opaque_edit(bytes) {
        reasons.push("按键里有 Tab 补全 / Ctrl-R / 方向键翻历史，最终提交的那一行看不到".into());
    } else if !line_cleared {
        match sit.and_then(current_input_line) {
            Some(cur) => {
                if let Some(first) = submitted.first_mut() {
                    *first = format!("{cur}{first}").trim().to_string();
                }
            }
            None => {
                // 读不出屏幕上已敲的内容：只有「带着完整文字、且没有屏幕可参考」才放行
                // （没有局面时仍保留旧逻辑：只评估本次文字；但光回车就看不到那行了）
                let first_empty = segs.first().map(|x| x.trim().is_empty()).unwrap_or(true);
                if sit.is_some() || first_empty {
                    reasons.push("读不出屏幕上已经敲着的内容，提交的那一行看不到".into());
                }
            }
        }
    }
    submitted.retain(|l| !l.is_empty());
    reasons.extend(danger_reasons(&submitted));
    if reasons.is_empty() {
        KeysVerdict::Allow
    } else {
        KeysVerdict::Approve(reasons)
    }
}

/// 局面是不是「某个程序在问我 / 在跑」（而不是停在 shell 提示符上）。
///
/// 屏幕最后一行不是 shell 提示符，就不是在 shell 上：脚本跑着时，上面残留的
/// `user@h:~$ ./install.sh` 回显不算数；认不出局面（Unknown）也不能默认当 shell 处理，
/// 否则 `y<Enter>` 这种对脚本的答复会被当成「在 shell 里提交一行」而白白进审批。
fn is_program_prompt(s: &Situation) -> bool {
    !shell_visible(s)
}

/// 「认不出是什么」的局面：既不是已知程序、也没有明确的提问 / 授权 / 菜单。
/// 可能是没认出的自定义 shell 提示符，所以回车时仍要把屏幕上已敲的那一行一起评估。
fn is_ambiguous(s: &Situation) -> bool {
    s.program == Program::Unknown && matches!(s.kind, SituationKind::Unknown | SituationKind::Error)
}

/// 这次按键是否在授权对话框里选了「永久授权」类选项。选中则给出拒绝理由。
pub(crate) fn permanent_pick(bytes: &[u8], visible: &str, s: &Situation) -> Option<String> {
    let first = visible.trim().chars().next();
    // 光按回车 = 选光标停着的那项，默认是第一项：第一项若是永久授权（计划批准的「Yes, and auto-accept edits」）
    if first.is_none()
        && super::interact::contains_enter(bytes)
        && s.options.first().map_or(false, |o| o.grants_permanent)
    {
        return Some(refuse_permanent(&s.options[0].text));
    }
    // 直接按序号 / 字母选
    if let Some(c) = first {
        let hit = s.options.iter().find(|o| {
            o.index.as_deref().map(|i| i.eq_ignore_ascii_case(&c.to_string())).unwrap_or(false)
        });
        if let Some(o) = hit {
            if o.grants_permanent {
                return Some(refuse_permanent(&o.text));
            }
            return None;
        }
    }
    // 方向键 / Shift-Tab：不知道光标会落到哪个选项（Claude Code 的 Shift-Tab 本身就是「本会话一律允许」）
    if has_navigation(bytes) {
        return Some(
            "授权对话框里有「永久授权」类选项，而方向键 / Shift-Tab 会让光标落点不可知。\
             请直接按选项的序号（如 1）选「仅本次允许」，不要用方向键或 Shift-Tab。"
                .into(),
        );
    }
    None
}

/// 这次按键是不是「不批准」：`n` / `no`、选了文字以 No 开头的编号、只按 Esc / Ctrl-C 之类不提交的键。
fn declines(bytes: &[u8], visible: &str, s: &Situation) -> bool {
    let t = visible.trim().to_lowercase();
    let Some(first) = t.chars().next() else {
        // 没有可见字符：有回车 = 提交光标所在项（当作批准，fail-closed）；没有回车 = Esc / Ctrl-C 取消
        return !super::interact::contains_enter(bytes);
    };
    if t == "n" || t == "no" {
        return true;
    }
    s.options.iter().any(|o| {
        o.index.as_deref().map_or(false, |i| i.eq_ignore_ascii_case(&first.to_string()))
            && o.text.trim().to_lowercase().starts_with("no")
    })
}

fn refuse_permanent(text: &str) -> String {
    format!(
        "选项「{text}」会永久扩大授权（don't ask again / always allow 一类），托管模式下不能替用户选。\
         请改选「仅本次允许」的选项；没有这样的选项就选拒绝，并在结论里告诉用户需要他本人决定。"
    )
}

/// 托管模式追加在系统提示词末尾的守则。
pub const STEWARD_ADDENDUM: &str = "\n\n## 托管模式\n\
你现在处于「托管」：用户把这个终端交给你，你要自主跑到目标完成，中途不会有人逐步指挥。\n\
- 持续推进，直到目标完成或确实走不下去；完成后用中文给出结论：做了什么、结果如何、有什么遗留。\n\
- 长时间运行的脚本 / 构建 / 安装，用 nx_watch 一次挂住直到它要人处理、结束或卡住，不要反复 nx_wait 轮询。\n\
- 操作 Claude Code 等控制台工具：它的提问和授权提示用 nx_send_keys 作答；授权只选「仅本次」，\
绝不选 don't ask again / always allow / allow all edits during this session / 永久授权一类选项，也不要用 Shift-Tab；\n\
- 给 Claude Code 这类 TUI 输入一段需求：先 nx_send_keys 发文字（不带回车），再单独一次 nx_send_keys 发 `<Enter>` 提交；\
提交后用 nx_watch 等它干完。屏幕局面变成 awaiting_input（输入框是空的、没在转圈）= 它这一轮做完了，读屏确认结果再决定下一步；\
需要打断它按 `<Esc>`。\n\
- 脚本 / 安装程序停在自由文本提示（如「Enter installation directory [/opt/app]:」）：按屏幕提示发答案加 `<Enter>`，不确定的值（路径、账号等）问用户，别瞎填。\n\
- 遇到密码 / 口令 / 令牌提示，一律调用 nx_secret 让用户本人输入，绝不自己键入、也不要在回复里复述任何密钥。\n\
- 破坏性操作（删除、格式化、覆盖、重启、提权等）会弹审批让用户点头；被拒绝就换更安全的办法，不要绕过。\n\
- 拿不准、有歧义、或同一办法连续失败两次：停下来问用户，不要猜着干。\n";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::situation::Choice;

    fn sit(program: Program, kind: SituationKind, options: Vec<Choice>) -> Situation {
        Situation {
            frame_id: 1,
            screen_hash: String::new(),
            program,
            kind,
            topic: None,
            options,
            confidence: 0.9,
            tail: "user@h:~$ ".to_string(),
            evidence: Vec::new(),
            stale_ms: 0,
            signature: String::new(),
        }
    }

    fn ch(idx: &str, text: &str, perm: bool) -> Choice {
        Choice { index: Some(idx.into()), text: text.into(), grants_permanent: perm }
    }

    fn perm_sit() -> Situation {
        sit(
            Program::ClaudeCode,
            SituationKind::Permission,
            vec![ch("1", "Yes", false), ch("2", "Yes, and don't ask again for this command", true), ch("3", "No", false)],
        )
    }

    #[test]
    fn password_refuses_text_but_allows_ctrl_c() {
        let s = sit(Program::Ssh, SituationKind::Password, vec![]);
        assert!(matches!(assess_keys(b"hunter2\r", Some(&s)), KeysVerdict::Refuse(m) if m.contains("nx_secret")));
        assert_eq!(assess_keys(b"\x03", Some(&s)), KeysVerdict::Allow);
        assert_eq!(assess_keys(b"\x1b", Some(&s)), KeysVerdict::Allow);
    }

    #[test]
    fn permission_permanent_option_refused() {
        let s = perm_sit();
        assert!(matches!(assess_keys(b"2\r", Some(&s)), KeysVerdict::Refuse(m) if m.contains("永久")));
        assert!(matches!(assess_keys(b"2", Some(&s)), KeysVerdict::Refuse(_)));
    }

    #[test]
    fn permission_once_option_allowed() {
        let s = perm_sit();
        assert_eq!(assess_keys(b"1\r", Some(&s)), KeysVerdict::Allow);
        assert_eq!(assess_keys(b"3", Some(&s)), KeysVerdict::Allow);
        // 单独回车 = 默认项
        assert_eq!(assess_keys(b"\r", Some(&s)), KeysVerdict::Allow);
    }

    #[test]
    fn permission_arrow_and_shift_tab_refused_when_permanent_exists() {
        let s = perm_sit();
        assert!(matches!(assess_keys(b"\x1b[B\r", Some(&s)), KeysVerdict::Refuse(_)));
        assert!(matches!(assess_keys(b"\x1b[Z", Some(&s)), KeysVerdict::Refuse(_)));
    }

    #[test]
    fn permission_without_permanent_option_allows_arrows() {
        let s = sit(Program::ClaudeCode, SituationKind::Permission, vec![ch("1", "Yes", false), ch("2", "No", false)]);
        assert_eq!(assess_keys(b"\x1b[B\r", Some(&s)), KeysVerdict::Allow);
    }

    #[test]
    fn shell_enter_assesses_typed_line() {
        let s = sit(Program::Shell, SituationKind::Idle, vec![]);
        assert_eq!(assess_keys(b"ls -la\r", Some(&s)), KeysVerdict::Allow);
        assert!(matches!(assess_keys(b"rm -rf /tmp/x\r", Some(&s)), KeysVerdict::Approve(r) if r.iter().any(|x| x.contains("rm"))));
        // 没回车只是往行里加字
        assert_eq!(assess_keys(b"rm -rf /tmp/x", Some(&s)), KeysVerdict::Allow);
    }

    #[test]
    fn shell_history_recall_needs_approval() {
        let s = sit(Program::Shell, SituationKind::Idle, vec![]);
        assert!(matches!(assess_keys(b"\x1b[A\r", Some(&s)), KeysVerdict::Approve(_)));
    }

    #[test]
    fn program_prompt_allows_anything() {
        let s = sit(Program::ClaudeCode, SituationKind::AwaitingInput, vec![]);
        assert_eq!(assess_keys(b"please tidy the old logs\r", Some(&s)), KeysVerdict::Allow);
        let q = sit(Program::Script, SituationKind::Question, vec![]);
        assert_eq!(assess_keys(b"y\r", Some(&q)), KeysVerdict::Allow);
    }

    #[test]
    fn no_screen_behaves_like_shell() {
        assert_eq!(assess_keys(b"echo hi\r", None), KeysVerdict::Allow);
        assert!(matches!(assess_keys(b"sudo reboot\r", None), KeysVerdict::Approve(_)));
    }

    #[test]
    fn shell_prompt_visible_beats_program_label() {
        // 局面判成 Error，但屏幕末行是 shell 提示符：仍按 shell 把关
        let mut s = sit(Program::Tui, SituationKind::Error, vec![]);
        s.evidence.push(crate::agent::situation::SHELL_PROMPT_VISIBLE.to_string());
        assert!(matches!(assess_keys(b"rm -rf x\r", Some(&s)), KeysVerdict::Approve(_)));
    }

    #[test]
    fn typed_lines_skip_escape_sequences() {
        assert_eq!(typed_lines(b"\x1b[Bls\r"), vec!["ls".to_string()]);
        assert_eq!(typed_lines(b"a\rb\n"), vec!["a".to_string(), "b".to_string()]);
    }

    fn at(tail: &str) -> Situation {
        let mut s = sit(Program::Shell, SituationKind::Idle, vec![]);
        s.tail = tail.to_string();
        s
    }

    #[test]
    fn split_send_cannot_bypass_the_gate() {
        // 先发 "rm -rf ~/x"（无回车），再单独发回车
        let s = at("user@h:~$ rm -rf ~/x");
        assert!(matches!(assess_keys(b"\r", Some(&s)), KeysVerdict::Approve(r) if r.iter().any(|x| x.contains("rm"))));
        // 补全一半再接上
        let s = at("root@h:/# rm -rf /tm");
        assert!(matches!(assess_keys(b"p/x\r", Some(&s)), KeysVerdict::Approve(_)));
        // 提示符后是空的：光回车无害
        assert_eq!(assess_keys(b"\r", Some(&at("user@h:~$"))), KeysVerdict::Allow);
        assert_eq!(assess_keys(b"\r", Some(&at("user@h:~$ "))), KeysVerdict::Allow);
        // 屏幕上已敲着安全命令
        assert_eq!(assess_keys(b"\r", Some(&at("user@h:~$ ls -la"))), KeysVerdict::Allow);
    }

    #[test]
    fn unreadable_current_line_needs_approval() {
        assert!(matches!(assess_keys(b"\r", Some(&at(""))), KeysVerdict::Approve(_)));
        assert!(matches!(assess_keys(b"x\r", Some(&at("no prompt marker here"))), KeysVerdict::Approve(_)));
        // 没有局面：光回车看不到那行；带文字则只评估文字
        assert!(matches!(assess_keys(b"\r", None), KeysVerdict::Approve(_)));
        assert_eq!(assess_keys(b"ls\r", None), KeysVerdict::Allow);
    }

    #[test]
    fn clearing_the_line_first_discards_what_was_on_screen() {
        let s = at("user@h:~$ rm -rf ~/x");
        assert_eq!(assess_keys(b"\x15ls\r", Some(&s)), KeysVerdict::Allow);
        assert!(matches!(assess_keys(b"\tls\r", Some(&s)), KeysVerdict::Approve(_)));
    }

    #[test]
    fn current_input_line_strips_the_prompt() {
        assert_eq!(current_input_line(&at("a\nuser@h:~$ echo > f")).as_deref(), Some("echo > f"));
        assert_eq!(current_input_line(&at("(venv) [root@h ~]# ls")).as_deref(), Some("ls"));
        assert_eq!(current_input_line(&at("")), None);
    }

    #[test]
    fn no_frame_bare_digit_refused_except_at_shell_and_enter_still_works() {
        // 没有帧、局面认不出 / 在等输入：单按数字 —— 拒
        for kind in [SituationKind::Unknown, SituationKind::AwaitingInput, SituationKind::Question, SituationKind::Progress] {
            let mut x = sit(Program::Unknown, kind, vec![]);
            x.frame_id = 0;
            assert!(matches!(assess_keys(b"2", Some(&x)), KeysVerdict::Refuse(_)), "{kind:?}");
            assert!(matches!(assess_keys(b"1\r", Some(&x)), KeysVerdict::Refuse(_)), "{kind:?}");
        }
        // 「按回车继续」：等输入时单按回车照常
        let mut wait = sit(Program::Unknown, SituationKind::AwaitingInput, vec![]);
        wait.frame_id = 0;
        assert!(!matches!(assess_keys(b"\r", Some(&wait)), KeysVerdict::Refuse(_)));
        // 确定停在 shell 提示符上：数字只是命令行字符
        let mut sh = sit(Program::Shell, SituationKind::Idle, vec![]);
        sh.frame_id = 0;
        sh.confidence = 0.9;
        assert!(!matches!(assess_keys(b"2", Some(&sh)), KeysVerdict::Refuse(_)));
    }

    #[test]
    fn fresh_menu_with_unparsed_options_digit_needs_approval() {
        let mut m = sit(Program::Unknown, SituationKind::Menu, vec![]);
        m.frame_id = 9;
        assert!(matches!(assess_keys(b"2", Some(&m)), KeysVerdict::Approve(_)));
        assert!(matches!(assess_keys(b"2\r", Some(&m)), KeysVerdict::Approve(_)));
        // 回车不知道选的是哪一项：仍拒
        assert!(matches!(assess_keys(b"\r", Some(&m)), KeysVerdict::Refuse(_)));
        // 授权卡没认出选项：仍拒
        let mut p = m.clone();
        p.kind = SituationKind::Permission;
        assert!(matches!(assess_keys(b"2", Some(&p)), KeysVerdict::Refuse(_)));
    }

    #[test]
    fn no_fresh_frame_menu_selection_is_refused_or_needs_approval() {
        let mut blind = sit(Program::Unknown, SituationKind::Menu, vec![]);
        blind.frame_id = 0;
        // 看不到选项：直接按数字 / 回车 —— 拒（先读屏）
        for k in [&b"2\r"[..], b"2", b"\r"] {
            assert!(matches!(assess_keys(k, Some(&blind)), KeysVerdict::Refuse(ref w) if w.contains("nx_screen")), "{k:?}");
        }
        let mut perm = blind.clone();
        perm.kind = SituationKind::Permission;
        assert!(matches!(assess_keys(b"1", Some(&perm)), KeysVerdict::Refuse(_)));
        // 方向键挪动：仍要人确认
        assert!(matches!(assess_keys(b"\x1b[B\r", Some(&blind)), KeysVerdict::Approve(_)));
        // 有帧但没认出选项：按编号交给人批准（R1）
        let mut seen = blind.clone();
        seen.frame_id = 7;
        assert!(matches!(assess_keys(b"2\r", Some(&seen)), KeysVerdict::Approve(_)));
        // 有帧、选项已知（都不是永久授权）：放行
        seen.options = vec![super::super::situation::Choice { index: Some("2".into()), text: "Vue".into(), grants_permanent: false }];
        assert_eq!(assess_keys(b"2\r", Some(&seen)), KeysVerdict::Allow);
    }

    #[test]
    fn program_prompt_still_assesses_long_text() {
        let s = sit(Program::ClaudeCode, SituationKind::AwaitingInput, vec![]);
        assert!(matches!(assess_keys(b"rm -rf /\r", Some(&s)), KeysVerdict::Approve(_)));
        assert_eq!(assess_keys(b"y\r", Some(&s)), KeysVerdict::Allow);
        assert_eq!(assess_keys(b"2\r", Some(&s)), KeysVerdict::Allow);
    }

    #[test]
    fn addendum_mentions_the_key_rules() {
        for k in ["nx_watch", "nx_secret", "don't ask again", "仅本次"] {
            assert!(STEWARD_ADDENDUM.contains(k), "{k}");
        }
    }

    // ───────────── 真实屏幕帧：分类 + nx_watch 判定 + 按键闸门，三者一起验 ─────────────

    use crate::agent::situation::frames::{self, classify_lines};
    use crate::agent::watch::{watch_verdict, Reason, WatchCfg};

    fn wake(sit: &Situation, changed_ago_ms: u64) -> Option<Reason> {
        let cfg = WatchCfg::from_args(Some(600_000), Some(120_000));
        watch_verdict(sit.kind, sit.program, shell_visible(sit), changed_ago_ms, 10_000, &cfg)
    }

    fn is_approve(v: &KeysVerdict) -> bool {
        matches!(v, KeysVerdict::Approve(_))
    }

    #[test]
    fn claude_code_working_keeps_watching() {
        for f in [&frames::CC_WORKING[..], &frames::CC_WORKING_RULES[..]] {
            let x = classify_lines(f);
            assert_eq!(x.kind, SituationKind::Progress);
            assert_eq!(wake(&x, 500), None);
            // 干活时用户 / 模型敲字不被拦（只有 Esc 之类）
            assert_eq!(assess_keys(b"\x1b", Some(&x)), KeysVerdict::Allow);
        }
    }

    #[test]
    fn claude_code_idle_wakes_the_model_and_text_then_enter_is_allowed() {
        for f in [&frames::CC_IDLE_BOX[..], &frames::CC_IDLE_ASKS[..], &frames::CC_IDLE_ACCEPT_EDITS[..]] {
            let x = classify_lines(f);
            assert_eq!(x.kind, SituationKind::AwaitingInput);
            assert_eq!(wake(&x, 5_000), Some(Reason::NeedsAttention(SituationKind::AwaitingInput)));
            // 屏幕刚变过（任务刚提交）先不叫醒
            assert_eq!(wake(&x, 500), None);
            assert_eq!(assess_keys("帮我把 foo.py 里的 bug 修掉".as_bytes(), Some(&x)), KeysVerdict::Allow);
            assert_eq!(assess_keys(b"\r", Some(&x)), KeysVerdict::Allow);
        }
    }

    #[test]
    fn claude_code_bash_permission_blocks_the_permanent_option() {
        let x = classify_lines(&frames::CC_BASH_PERM);
        assert_eq!(wake(&x, 0), Some(Reason::NeedsAttention(SituationKind::Permission)));
        // 1 号是「本次允许」，但批准的是 `rm -rf build`：命令危险，要人点头（不是直接放行）
        assert!(matches!(assess_keys(b"1\r", Some(&x)), KeysVerdict::Approve(_)));
        assert!(matches!(assess_keys(b"2\r", Some(&x)), KeysVerdict::Refuse(_)));
        assert!(matches!(assess_keys(b"\x1b[Z", Some(&x)), KeysVerdict::Refuse(_)));
        assert_eq!(assess_keys(b"3", Some(&x)), KeysVerdict::Allow);
    }

    #[test]
    fn claude_code_edit_create_and_trust_dialogs() {
        for f in [&frames::CC_EDIT_PERM[..], &frames::CC_CREATE_PERM[..]] {
            let x = classify_lines(f);
            assert_eq!(wake(&x, 0), Some(Reason::NeedsAttention(SituationKind::Permission)));
            assert_eq!(assess_keys(b"1\r", Some(&x)), KeysVerdict::Allow);
            assert!(matches!(assess_keys(b"2\r", Some(&x)), KeysVerdict::Refuse(_)), "allow all edits during this session");
            assert!(matches!(assess_keys(b"\x1b[Z", Some(&x)), KeysVerdict::Refuse(_)));
        }
        let t = classify_lines(&frames::CC_TRUST);
        assert_eq!(t.kind, SituationKind::Permission);
        assert_eq!(assess_keys(b"1\r", Some(&t)), KeysVerdict::Allow);
    }

    #[test]
    fn running_script_with_echoed_prompt_is_not_finished_and_yn_is_allowed() {
        let run = classify_lines(&frames::SCRIPT_RUNNING);
        assert_eq!(wake(&run, 500), None, "脚本还在跑：不能因为上面有 `$ ./long.sh` 回显就当成回到了 shell");
        assert_ne!(wake(&run, 500), Some(Reason::ShellPrompt));
        // 脚本在跑时回车 / 答复不进 shell 分支
        assert_eq!(assess_keys(b"\r", Some(&run)), KeysVerdict::Allow);
        assert_eq!(assess_keys(b"y\r", Some(&run)), KeysVerdict::Allow);

        let yn = classify_lines(&frames::SCRIPT_YN);
        assert_eq!(wake(&yn, 0), Some(Reason::NeedsAttention(yn.kind)));
        assert_eq!(assess_keys(b"y\r", Some(&yn)), KeysVerdict::Allow);
        assert!(is_approve(&assess_keys(b"rm -rf /\r", Some(&yn))), "长文本仍要过危险评估");

        let menu = classify_lines(&frames::SCRIPT_MENU);
        assert_eq!(wake(&menu, 0), Some(Reason::NeedsAttention(SituationKind::Menu)));
        assert_eq!(assess_keys(b"2\r", Some(&menu)), KeysVerdict::Allow);

        // 真回到提示符才算结束
        let done = classify_lines(&["step 10 of 10 done", "user@h:~/p$ "]);
        assert_eq!(wake(&done, 0), Some(Reason::ShellPrompt));
    }

    #[test]
    fn sudo_password_is_refused_and_wakes_immediately() {
        let x = classify_lines(&frames::SUDO_PW);
        let cfg = WatchCfg::from_args(None, None);
        assert_eq!(
            watch_verdict(x.kind, x.program, shell_visible(&x), 0, 100, &cfg),
            Some(Reason::NeedsAttention(SituationKind::Password))
        );
        assert!(matches!(assess_keys(b"hunter2\r", Some(&x)), KeysVerdict::Refuse(_)));
        assert_eq!(assess_keys(b"\x03", Some(&x)), KeysVerdict::Allow);
    }

    #[test]
    fn free_text_prompt_wakes_after_the_screen_settles_and_answers_are_allowed() {
        let x = classify_lines(&frames::SCRIPT_DIR_PROMPT);
        assert_eq!(x.kind, SituationKind::AwaitingInput);
        assert_eq!(wake(&x, 500), None, "屏幕刚变过：可能只是输出里的一行");
        assert_eq!(wake(&x, 5_000), Some(Reason::NeedsAttention(SituationKind::AwaitingInput)));
        assert_eq!(assess_keys(b"/opt/myapp\r", Some(&x)), KeysVerdict::Allow);
        assert_eq!(assess_keys(b"\r", Some(&x)), KeysVerdict::Allow);
        assert!(is_approve(&assess_keys(b"rm -rf /\r", Some(&x))));
    }

    #[test]
    fn unrecognised_screen_is_not_treated_as_a_bare_shell() {
        // 认不出局面（没有任何提示符）：短答复放行，长命令仍评估；半行危险命令 + 单独回车也拦得住
        let x = classify_lines(&["some output", "more output"]);
        assert_eq!(x.kind, SituationKind::Unknown);
        assert_eq!(assess_keys(b"y\r", Some(&x)), KeysVerdict::Allow);
        assert!(is_approve(&assess_keys(b"rm -rf ~/x\r", Some(&x))));
        let half = classify_lines(&["total 4", "myhost> rm -rf ~/x"]);
        assert!(is_approve(&assess_keys(b"\r", Some(&half))), "{:?}", half.kind);
        assert!(is_approve(&assess_keys(b"\t\r", Some(&x))));
    }

    #[test]
    fn steward_budget_defaults_are_generous() {
        assert_eq!((STEWARD_MAX_STEPS, STEWARD_MAX_MINUTES), (400, 360));
    }

    // ───────────── 第三轮复查（N1–N5）的回归 ─────────────

    #[test]
    fn n1_markdown_yes_no_in_a_claude_code_reply_is_not_a_permission_dialog() {
        let w = classify_lines(frames::CC_REPLY_YN_WORKING);
        assert_eq!(w.kind, SituationKind::Progress, "{:?}", w.evidence);
        assert_eq!(wake(&w, 5_000), None);
        let i = classify_lines(frames::CC_REPLY_YN_IDLE);
        assert_eq!(i.kind, SituationKind::AwaitingInput, "{:?}", i.evidence);
        assert_eq!(wake(&i, 5_000), Some(Reason::NeedsAttention(SituationKind::AwaitingInput)));
        // 对输入框敲字 + 回车照常放行
        assert_eq!(assess_keys(b"2", Some(&i)), KeysVerdict::Allow);
        assert_eq!(assess_keys(b"\r", Some(&i)), KeysVerdict::Allow);
        // 脚本旧输出里残留的 1. Yes / 2. No，加一行 `retrying:`
        let r = classify_lines(frames::STALE_YN_THEN_RETRY);
        assert_ne!(r.kind, SituationKind::Permission, "{:?}", r.evidence);
        assert_eq!(wake(&r, 500), None);
    }

    #[test]
    fn n2_leftover_claude_code_box_above_a_shell_prompt() {
        // 空提示符：Claude Code 已退出，回到 shell
        let b = classify_lines(frames::CC_LEFTOVER_THEN_SHELL);
        assert_eq!(b.program, Program::Shell, "{:?}", b.evidence);
        assert_eq!(b.kind, SituationKind::Idle);
        assert!(shell_visible(&b));
        assert_eq!(wake(&b, 5_000), Some(Reason::ShellPrompt));
        // 提示符上敲着 rm -rf：回车必须进审批
        let x = classify_lines(frames::CC_LEFTOVER_THEN_TYPED_RM);
        assert!(shell_visible(&x), "{:?} {:?}", x.program, x.evidence);
        assert!(is_approve(&assess_keys(b"\r", Some(&x))), "{:?}", assess_keys(b"\r", Some(&x)));
        // 就算局面被判成程序提示，末行是带文字的 shell 提示符也照样评估
        let mut y = sit(Program::ClaudeCode, SituationKind::AwaitingInput, vec![]);
        y.tail = "────\nuser@h:~/p$ rm -rf /tmp/zz".into();
        assert!(is_approve(&assess_keys(b"\r", Some(&y))));
    }

    #[test]
    fn n3_label_lines_in_build_output_do_not_wake_the_model() {
        for f in [frames::BUILD_COMPILING, frames::BUILD_SUMMARY, frames::BUILD_FETCHING] {
            let x = classify_lines(f);
            assert_ne!(x.kind, SituationKind::AwaitingInput, "{f:?} {:?}", x.evidence);
            assert_eq!(wake(&x, 10_000), None, "{f:?}");
        }
        assert_eq!(classify_lines(frames::BUILD_COMPILING).kind, SituationKind::Progress);
        // 真的提示仍然叫醒（见 free_text_prompt_wakes_after_the_screen_settles…）
        let d = classify_lines(&frames::SCRIPT_DIR_PROMPT);
        assert_eq!(wake(&d, 5_000), Some(Reason::NeedsAttention(SituationKind::AwaitingInput)));
    }

    #[test]
    fn n4_powershell_cmd_and_fish_prompts_are_shells() {
        for f in [frames::PS_PROMPT, frames::CMD_PROMPT, frames::FISH_PROMPT] {
            let x = classify_lines(f);
            assert_eq!(x.kind, SituationKind::Idle, "{f:?} {:?}", x.evidence);
            assert_eq!(x.program, Program::Shell, "{f:?}");
            assert!(shell_visible(&x));
            assert_eq!(wake(&x, 5_000), Some(Reason::ShellPrompt), "{f:?}");
        }
        // PowerShell 提示符上敲着 Remove-Item -Recurse：回车进审批
        let t = classify_lines(frames::PS_TYPED_REMOVE);
        assert!(is_approve(&assess_keys(b"\r", Some(&t))), "{:?}", assess_keys(b"\r", Some(&t)));
        assert!(is_approve(&assess_keys(b"Remove-Item -Recurse -Force C:\\x\r", Some(&classify_lines(frames::PS_PROMPT)))));
        assert_eq!(assess_keys(b"dir\r", Some(&classify_lines(frames::PS_PROMPT))), KeysVerdict::Allow);
    }

    #[test]
    fn n5_selector_menus_are_menus_with_options() {
        let m = classify_lines(frames::CC_ASK_MENU);
        assert_eq!(m.kind, SituationKind::Menu, "{:?}", m.evidence);
        assert_eq!(m.options.len(), 3);
        assert_eq!(wake(&m, 0), Some(Reason::NeedsAttention(SituationKind::Menu)));
        assert_eq!(assess_keys(b"1\r", Some(&m)), KeysVerdict::Allow);
        assert_eq!(assess_keys(b"\r", Some(&m)), KeysVerdict::Allow);
    }

    #[test]
    fn n5_plan_approval_auto_accept_is_a_permanent_option() {
        let p = classify_lines(frames::CC_PLAN_APPROVAL);
        assert_eq!(p.kind, SituationKind::Permission, "{:?}", p.evidence);
        assert_eq!(wake(&p, 0), Some(Reason::NeedsAttention(SituationKind::Permission)));
        assert!(p.options[0].grants_permanent, "{:?}", p.options);
        assert!(matches!(assess_keys(b"1\r", Some(&p)), KeysVerdict::Refuse(_)));
        assert!(matches!(assess_keys(b"1", Some(&p)), KeysVerdict::Refuse(_)));
        assert!(matches!(assess_keys(b"\r", Some(&p)), KeysVerdict::Refuse(_)), "默认高亮第一项 = 永久授权");
        assert_eq!(assess_keys(b"2\r", Some(&p)), KeysVerdict::Allow);
        assert_eq!(assess_keys(b"3", Some(&p)), KeysVerdict::Allow);
    }

    #[test]
    fn approving_a_dangerous_command_in_a_permission_dialog_needs_a_human() {
        use crate::agent::situation::frames;
        // CC_BASH_PERM 批准的是 `rm -rf build`
        let p = frames::classify_lines(&frames::CC_BASH_PERM);
        assert!(matches!(assess_keys(b"1", Some(&p)), KeysVerdict::Approve(_)));
        assert!(matches!(assess_keys(b"\r", Some(&p)), KeysVerdict::Approve(_)), "回车提交光标项（Yes）");
        // 拒绝不需要人
        assert_eq!(assess_keys(b"3", Some(&p)), KeysVerdict::Allow);
        assert_eq!(assess_keys(b"\x1b", Some(&p)), KeysVerdict::Allow);
        // 安全命令照常放行
        let safe: Vec<String> = frames::CC_BASH_PERM.iter().map(|l| l.replace("rm -rf build ", "ls -la build ")).collect();
        let refs: Vec<&str> = safe.iter().map(|s| s.as_str()).collect();
        let q = frames::classify_lines(&refs);
        assert_eq!(assess_keys(b"1", Some(&q)), KeysVerdict::Allow);
    }
}
