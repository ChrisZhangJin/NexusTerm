//! 长任务（托管）的上下文压缩。
//!
//! 托管一跑就是上百步，每步都把终端输出喂回模型，历史会一路涨到撑爆上下文窗口。
//! 这里在每次请求模型前（估算 token 数超过按模型窗口算出的阈值时），把「很久以前的步骤」折成一条简短的笔记，保留：
//! - 第 0 条（任务本身 + 上一轮摘要 + 初始屏幕）；
//! - 最近一段原样历史（模型接着干活需要的现场）。
//!
//! 历史是**纯文本的 user/assistant 交替**（工具调用在入历史时已渲染成文字，没有原生
//! tool_call / tool_result 配对），所以不存在拆散配对的问题；但必须保持严格交替 ——
//! 不少网关会拒绝连续两条同角色的消息。做法：笔记用 assistant 角色（插在第 0 条 user 之后），
//! 尾段从 user 消息开始，交替自然成立。

use super::llm::ChatMessage;

/// 历史最多占上下文窗口的比例（百分比）。剩下的留给系统提示词、工具声明与模型的回复。
const WINDOW_SHARE_PCT: usize = 45;
/// token 阈值的上限：窗口再大也不让单次请求无限膨胀（又慢又贵，模型也更容易走神）。
const MAX_THRESHOLD_TOKENS: usize = 48_000;
/// token 阈值的下限：小窗口模型至少也得装下几步现场。
const MIN_THRESHOLD_TOKENS: usize = 3_000;
/// 认不出模型时的保守窗口。
const DEFAULT_CONTEXT_TOKENS: usize = 32_000;

/// 按模型名猜上下文窗口（token）。账号配置里没有「上下文窗口」这一项，只能按名字认；
/// 宁小勿大 —— 估小了只是早一点压缩，估大了是请求直接被服务方拒掉。
pub fn context_window_tokens(model: &str) -> usize {
    let m = model.to_ascii_lowercase();
    let has = |k: &str| m.contains(k);
    if has("claude") {
        200_000
    } else if has("gemini") {
        1_000_000
    } else if has("gpt-3.5") {
        16_000
    } else if has("gpt-4-32k") {
        32_000
    } else if has("gpt-4o") || has("gpt-4-turbo") || has("gpt-4.1") || has("gpt-4.5") || has("gpt-5")
        || m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4")
    {
        128_000
    } else if has("gpt-4") {
        8_000
    } else if has("deepseek") {
        64_000
    } else if has("qwen") || has("glm") || has("moonshot") || has("kimi") || has("mistral-large")
        || has("grok") || has("128k")
    {
        128_000
    } else if has("32k") {
        32_000
    } else if has("llama") || has("mistral") || has("mixtral") || has("gemma") || has("phi") {
        8_000
    } else {
        DEFAULT_CONTEXT_TOKENS
    }
}

/// 该模型下历史超过多少 token 就压缩。
pub fn threshold_tokens(model: &str) -> usize {
    (context_window_tokens(model) * WINDOW_SHARE_PCT / 100).clamp(MIN_THRESHOLD_TOKENS, MAX_THRESHOLD_TOKENS)
}

/// 是不是按「一字一 token」计的字符：中日韩文字、假名、谚文与全角标点。
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x2E80..=0x9FFF      // 部首、标点、假名、CJK 统一汉字等
        | 0xA960..=0xA97F    // 谚文扩展
        | 0xAC00..=0xD7AF    // 谚文音节
        | 0xF900..=0xFAFF    // 兼容汉字
        | 0xFE30..=0xFE4F    // 兼容形式
        | 0xFF00..=0xFFEF    // 全角 / 半角形式
        | 0x20000..=0x3FFFF) // 扩展 B 及以后
}

/// 粗估 token 数：CJK 一字算 1 个，其余按 3 个字符 1 个（终端输出里路径、哈希多，比英文散文更费 token）。
pub fn estimate_tokens(s: &str) -> usize {
    let (mut cjk, mut other) = (0usize, 0usize);
    for c in s.chars() {
        if is_cjk(c) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other.div_ceil(3)
}

pub fn total_tokens(messages: &[ChatMessage]) -> usize {
    messages.iter().map(|m| estimate_tokens(&m.content)).sum()
}

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

/// 估算 token 数超过 `budget_tokens` 时压缩 `messages`（阈值见 [`threshold_tokens`]）。返回是否改动了历史。
///
/// 调用约定：在发请求前调用，此时最后一条是 user（上一步的反馈）。
pub fn compact(messages: &mut Vec<ChatMessage>, budget_tokens: usize) -> bool {
    if total_tokens(messages) <= budget_tokens {
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
        assert!(compact(&mut v, threshold_tokens("gpt-4o")));
        assert_eq!(v[0].content, first);
        assert_eq!(v.last().unwrap().content, last);
        assert_eq!(v[1].role, "assistant");
        assert!(v[1].content.starts_with(NOTE_PREFIX));
        assert_eq!(v.len(), 1 + 1 + KEEP_TAIL);
        assert!(alternates(&v));
        assert_eq!(v[0].role, "user");
        assert!(total_tokens(&v) < threshold_tokens("gpt-4o"));
    }

    #[test]
    fn cjk_counts_one_token_per_char() {
        assert_eq!(estimate_tokens("中文输出"), 4);
        assert_eq!(estimate_tokens("abcdef"), 2);
        assert_eq!(estimate_tokens("ab中"), 2);
        assert_eq!(estimate_tokens("，。！"), 3);
        // 同样的字符数，中文比英文贵得多
        let zh = "错".repeat(30_000);
        let en = "x".repeat(30_000);
        assert!(estimate_tokens(&zh) > 2 * estimate_tokens(&en));
    }

    #[test]
    fn threshold_follows_model_window() {
        assert!(threshold_tokens("claude-sonnet-4-5") > threshold_tokens("deepseek-chat"));
        assert!(threshold_tokens("deepseek-chat") > threshold_tokens("some-unknown-model"));
        assert!(threshold_tokens("llama3-8b") < threshold_tokens("some-unknown-model"));
        for m in ["", "gemini-2.5-pro", "gpt-4", "x"] {
            let t = threshold_tokens(m);
            assert!((MIN_THRESHOLD_TOKENS..=MAX_THRESHOLD_TOKENS).contains(&t), "{m}: {t}");
        }
        // 阈值必须真的落在窗口之内
        for m in ["gpt-3.5-turbo", "gpt-4", "llama3", "qwen-max", "deepseek-chat"] {
            assert!(threshold_tokens(m) < context_window_tokens(m), "{m}");
        }
    }

    /// 回归：同样 6 万字符，纯中文的历史以前不会触发压缩（固定 12 万字符阈值），
    /// 实际已经约 6 万 token，64k 窗口的模型直接被撑爆。
    #[test]
    fn cjk_history_compacts_earlier_than_ascii() {
        let mk = |ch: &str| {
            let mut v = vec![ChatMessage::user("任务")];
            for i in 0..30 {
                v.push(ChatMessage::assistant(format!("步骤 {i}")));
                v.push(ChatMessage::user(ch.repeat(2_000)));
            }
            v
        };
        let budget = threshold_tokens("deepseek-chat");
        let mut en = mk("x");
        let mut zh = mk("错");
        assert!(!compact(&mut en, budget), "6 万 ASCII 字符约 2 万 token，不该压缩");
        assert!(compact(&mut zh, budget), "6 万汉字约 6 万 token，必须压缩");
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
        compact(&mut v, 20_000);
        for i in 0..20 {
            v.push(ChatMessage::assistant(format!("后续 {i}")));
            v.push(ChatMessage::user(format!("更多输出 {i} {}", "y".repeat(5_000))));
        }
        assert!(compact(&mut v, 20_000));
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
