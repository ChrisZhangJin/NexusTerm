//! 长任务（托管）的上下文压缩。
//!
//! 托管一跑就是上百步，每步都把终端输出喂回模型，历史会一路涨到撑爆上下文窗口。
//! 这里在每次请求模型前，把「很久以前的步骤」折成一条简短的笔记，保留：
//! - 第 0 条（任务本身 + 上一轮摘要 + 初始屏幕）；
//! - 最近一段原样历史（模型接着干活需要的现场）。
//!
//! 历史是**纯文本的 user/assistant 交替**（工具调用在入历史时已渲染成文字，没有原生
//! tool_call / tool_result 配对），所以不存在拆散配对的问题；但必须保持严格交替 ——
//! 不少网关会拒绝连续两条同角色的消息。做法：笔记用 assistant 角色（插在第 0 条 user 之后），
//! 尾段从 user 消息开始，交替自然成立。

use super::llm::ChatMessage;

/// 超过这个总字符数就压缩（约等于几万 token）。
pub const COMPACT_THRESHOLD_CHARS: usize = 120_000;
/// 压缩后原样保留的最近消息条数（取奇数，保证尾段以 user 开头）。
const KEEP_TAIL: usize = 11;
/// 笔记里每条旧消息保留的字符数。
const NOTE_LINE_CHARS: usize = 160;
/// 笔记总长上限。
const NOTE_MAX_CHARS: usize = 8_000;
/// 尾段里单条消息的上限：一次巨大的屏幕输出不该独占窗口。
const TAIL_MSG_MAX_CHARS: usize = 12_000;
/// 笔记的开头标记，二次压缩时据此认出上一份笔记。
pub const NOTE_PREFIX: &str = "（已省略";

pub fn total_chars(messages: &[ChatMessage]) -> usize {
    messages.iter().map(|m| m.content.chars().count()).sum()
}

fn head_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

/// 保留首尾、截掉中间。
fn squeeze(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let h = max / 3;
    let t = max - h;
    format!(
        "{}\n…（中间省略 {} 字）…\n{}",
        s.chars().take(h).collect::<String>(),
        n - max,
        s.chars().skip(n - t).collect::<String>()
    )
}

/// 一条旧消息的一行摘要。
fn summarize(m: &ChatMessage) -> String {
    if m.role == "assistant" && m.content.starts_with(NOTE_PREFIX) {
        return squeeze(&m.content, 2_000);
    }
    let flat: String = m.content.split_whitespace().collect::<Vec<_>>().join(" ");
    let who = if m.role == "assistant" { "我" } else { "反馈" };
    format!("- {who}：{}", head_chars(&flat, NOTE_LINE_CHARS))
}

/// 总字符数超过 `budget_chars` 时压缩 `messages`。返回是否改动了历史。
///
/// 调用约定：在发请求前调用，此时最后一条是 user（上一步的反馈）。
pub fn compact(messages: &mut Vec<ChatMessage>, budget_chars: usize) -> bool {
    if total_chars(messages) <= budget_chars {
        return false;
    }
    let mut changed = false;
    let len = messages.len();
    // 中间段 = [1, tail_start)。尾段首条必须是 user，且至少折掉 2 条才值得
    if len > 1 + KEEP_TAIL + 1 {
        let mut tail_start = len - KEEP_TAIL;
        if messages[tail_start].role != "user" {
            tail_start -= 1; // 多折一条，让尾段从 user 开始
        }
        if tail_start > 1 && messages[tail_start].role == "user" {
            let middle: Vec<ChatMessage> = messages.drain(1..tail_start).collect();
            let n = middle.len();
            let mut lines: Vec<String> = middle.iter().map(summarize).collect();
            // 超长时丢最旧的行（上一份笔记在最前面，也一并丢弃的话至少留下计数）
            let mut body = lines.join("\n");
            while body.chars().count() > NOTE_MAX_CHARS && lines.len() > 1 {
                lines.remove(0);
                body = lines.join("\n");
            }
            let note = format!(
                "{NOTE_PREFIX} {n} 条较早的步骤，下面是它们的简要回顾；需要现场细节请重新 nx_screen。）\n{body}"
            );
            messages.insert(1, ChatMessage::assistant(note));
            changed = true;
        }
    }
    // 兜底：单条过大的（通常是整屏输出）压成首尾
    for m in messages.iter_mut().skip(1) {
        if m.content.chars().count() > TAIL_MSG_MAX_CHARS {
            m.content = squeeze(&m.content, TAIL_MSG_MAX_CHARS);
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convo(pairs: usize, fb_len: usize) -> Vec<ChatMessage> {
        let mut v = vec![ChatMessage::user("任务：把服务装好")];
        for i in 0..pairs {
            v.push(ChatMessage::assistant(format!("步骤 {i}：执行 `cmd{i}`")));
            v.push(ChatMessage::user(format!("输出{i} {}", "x".repeat(fb_len))));
        }
        v
    }

    fn alternates(v: &[ChatMessage]) -> bool {
        v.windows(2).all(|w| w[0].role != w[1].role)
    }

    #[test]
    fn under_budget_is_untouched() {
        let mut v = convo(5, 10);
        let before = v.clone();
        assert!(!compact(&mut v, 10_000));
        assert_eq!(v.len(), before.len());
    }

    #[test]
    fn compacts_middle_and_keeps_first_and_tail() {
        let mut v = convo(60, 3_000);
        let last = v.last().unwrap().content.clone();
        let first = v[0].content.clone();
        assert!(compact(&mut v, COMPACT_THRESHOLD_CHARS));
        assert_eq!(v[0].content, first);
        assert_eq!(v.last().unwrap().content, last);
        assert_eq!(v[1].role, "assistant");
        assert!(v[1].content.starts_with(NOTE_PREFIX));
        assert_eq!(v.len(), 1 + 1 + KEEP_TAIL);
        assert!(alternates(&v));
        assert_eq!(v[0].role, "user");
        assert!(total_chars(&v) < 120_000);
    }

    #[test]
    fn alternation_holds_for_every_length_parity() {
        for pairs in [13, 14, 30, 31] {
            let mut v = convo(pairs, 20_000);
            compact(&mut v, 1_000);
            assert!(alternates(&v), "pairs={pairs}");
            assert_eq!(v.last().unwrap().role, "user");
        }
    }

    #[test]
    fn second_compaction_folds_previous_note() {
        let mut v = convo(40, 5_000);
        compact(&mut v, 50_000);
        for i in 0..20 {
            v.push(ChatMessage::assistant(format!("后续 {i}")));
            v.push(ChatMessage::user(format!("更多输出 {i} {}", "y".repeat(5_000))));
        }
        assert!(compact(&mut v, 50_000));
        assert!(alternates(&v));
        assert_eq!(v.iter().filter(|m| m.content.starts_with(NOTE_PREFIX)).count(), 1);
    }

    #[test]
    fn short_history_only_squeezes_giant_messages() {
        let mut v = convo(3, 50_000);
        assert!(compact(&mut v, 10_000));
        assert_eq!(v.len(), 7);
        assert!(v.iter().skip(1).all(|m| m.content.chars().count() <= TAIL_MSG_MAX_CHARS + 60));
    }

    #[test]
    fn note_is_bounded() {
        let mut v = convo(500, 10);
        compact(&mut v, 100);
        assert!(v[1].content.chars().count() < NOTE_MAX_CHARS + 500);
    }
}
