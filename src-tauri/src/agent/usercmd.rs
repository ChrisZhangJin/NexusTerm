//! 用户提交过的命令行：side buffer 里的结构化「user command」记录。
//!
//! 旁路缓冲里的 `$ ` 转录只是一串带前缀的文本，既分不出一条命令在哪儿结束，
//! 也分不出是谁敲的。时间线（2.5）与录制器（3.2）要的是**分段后的结构化事件**：
//! 每条 = 一次回车提交，带时间戳、actor、在原始流里的位置。
//!
//! 数据来源只有一个：前端 `TerminalView` 在**回车**时推来的整行
//! （`cmd::agent_user_command`），不逐键推送。
//!
//! 三条硬规矩：
//!
//! 1. **无回显的输入绝不入账。** 前端判定这行没被回显过（密码提示），或后端按屏幕尾部
//!    分类出「正在等密码」（置信度 >= 0.8），一律只存 `SecretInput` 占位，不存内容。
//! 2. **actor 只会是 `User`。** Agent 自己发的命令走后端 `act.rs` 直接写 pty，
//!    根本不经过前端的键盘通路，所以不会被这里重复记一遍；`Actor::Agent` 预留给
//!    2.5 时间线合并两路事件时使用。
//! 3. **对外暴露给模型前必须过脱敏**：`line` 里照样可能带 `--password=xxx`、token，
//!    用 [`UserCommandEntry::redacted`] 取脱敏副本。

use std::collections::VecDeque;

use serde::Serialize;

use super::buffer::strip_ansi;
use super::redact;

/// 单会话保留的用户命令条数上限。环形：超了丢最老的。
pub const MAX_ENTRIES: usize = 256;

/// 单行内容上限（字节）。正常命令远小于此；超出的多半是粘贴了一大坨东西，截断并打标。
pub const MAX_LINE_BYTES: usize = 2048;

/// 谁干的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    User,
    /// 2.5 统一时间线里 Agent 自己发的动作。本模块不会产生它。
    Agent,
}

/// 条目类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UserEntryKind {
    /// 一条正常提交的命令，`line` 是清洗后的内容。
    Command,
    /// 一次无回显输入（密码 / 口令）。**只有占位，`line` 恒为空。**
    SecretInput,
}

/// 一条用户提交的命令事件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserCommandEntry {
    /// 会话内单调递增的序号（从 1 起，`clear` 也不归零），供读者做增量拉取。
    pub seq: u64,
    /// 提交时刻，Unix 毫秒。
    pub ts_ms: u64,
    pub actor: Actor,
    pub kind: UserEntryKind,
    /// 清洗后的命令行。`SecretInput` 时恒为空串。**原文，未脱敏**，见 [`Self::redacted`]。
    pub line: String,
    /// 原行超过 [`MAX_LINE_BYTES`] 被截断过。
    pub truncated: bool,
    /// 提交时旁路缓冲的 `pushed` 坐标：这条命令之后产生的输出从 `at` 开始，
    /// 配合 `SessionBuffer::since(at)` 就能切出「这条命令对应的输出段」。
    pub at: u64,
}

impl UserCommandEntry {
    /// 脱敏副本：`line` 过 [`redact::redact`]。凡是要给模型看的地方都该用它。
    pub fn redacted(&self) -> Self {
        Self {
            line: redact::redact(&self.line),
            ..self.clone()
        }
    }
}

/// 环形的条目日志。
#[derive(Debug, Default)]
pub struct UserCommandLog {
    entries: VecDeque<UserCommandEntry>,
    next_seq: u64,
}

impl UserCommandLog {
    /// 记一条命令。清洗后为空则不入账，返回 `None`。
    pub fn push_command(&mut self, raw: &str, ts_ms: u64, at: u64) -> Option<UserCommandEntry> {
        let (line, truncated) = sanitize_line(raw);
        if line.is_empty() {
            return None;
        }
        Some(self.push(UserEntryKind::Command, line, truncated, ts_ms, at))
    }

    /// 记一次无回显输入的占位。内容永远不进来。
    pub fn push_secret(&mut self, ts_ms: u64, at: u64) -> UserCommandEntry {
        self.push(UserEntryKind::SecretInput, String::new(), false, ts_ms, at)
    }

    fn push(
        &mut self,
        kind: UserEntryKind,
        line: String,
        truncated: bool,
        ts_ms: u64,
        at: u64,
    ) -> UserCommandEntry {
        self.next_seq += 1;
        let entry = UserCommandEntry {
            seq: self.next_seq,
            ts_ms,
            actor: Actor::User,
            kind,
            line,
            truncated,
            at,
        };
        self.entries.push_back(entry.clone());
        while self.entries.len() > MAX_ENTRIES {
            self.entries.pop_front();
        }
        entry
    }

    /// 最近 `limit` 条（按时间正序，最新的在最后）。
    pub fn recent(&self, limit: usize) -> Vec<UserCommandEntry> {
        let skip = self.entries.len().saturating_sub(limit);
        self.entries.iter().skip(skip).cloned().collect()
    }

    /// `seq` 严格大于 `after` 的全部条目（增量拉取）。
    pub fn since_seq(&self, after: u64) -> Vec<UserCommandEntry> {
        self.entries
            .iter()
            .filter(|e| e.seq > after)
            .cloned()
            .collect()
    }

    /// 清空条目，`next_seq` 保留（读者手里的 seq 游标不能倒退）。
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// 清洗一行命令：剥 ANSI、控制字符一律丢（制表 / 换行折成空格）、去首尾空白、
/// 按字节上限在字符边界截断。返回 `(内容, 是否被截断)`。
pub fn sanitize_line(raw: &str) -> (String, bool) {
    let stripped = strip_ansi(raw);
    let cleaned: String = stripped
        .chars()
        .filter_map(|c| match c {
            '\t' | '\n' | '\r' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.len() <= MAX_LINE_BYTES {
        return (trimmed.to_string(), false);
    }
    let mut end = MAX_LINE_BYTES;
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    (trimmed[..end].to_string(), true)
}

/// 当前 Unix 毫秒。系统时钟早于 1970 时回 0，不 panic。
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_ansi_and_control() {
        let (s, t) = sanitize_line("  \u{1b}[31mls\u{1b}[0m -la\u{7}\t\n ");
        assert_eq!(s, "ls -la");
        assert!(!t);
    }

    #[test]
    fn sanitize_truncates_on_char_boundary() {
        let raw = "命".repeat(MAX_LINE_BYTES); // 每个 3 字节
        let (s, t) = sanitize_line(&raw);
        assert!(t);
        assert!(s.len() <= MAX_LINE_BYTES);
        assert!(s.chars().all(|c| c == '命'));
    }

    #[test]
    fn empty_after_sanitize_is_dropped() {
        let mut log = UserCommandLog::default();
        assert!(log.push_command("  \u{1b}[0m \t", 1, 0).is_none());
        assert!(log.recent(10).is_empty());
    }

    #[test]
    fn seq_is_monotonic_and_ring_caps() {
        let mut log = UserCommandLog::default();
        for i in 0..(MAX_ENTRIES + 10) {
            log.push_command(&format!("cmd {i}"), i as u64, 0);
        }
        let all = log.recent(usize::MAX);
        assert_eq!(all.len(), MAX_ENTRIES);
        assert_eq!(all.last().unwrap().seq, (MAX_ENTRIES + 10) as u64);
        assert_eq!(all.first().unwrap().seq, 11);
        // 最近 3 条按时间正序
        let last3 = log.recent(3);
        assert_eq!(last3.len(), 3);
        assert!(last3[0].seq < last3[2].seq);
    }

    #[test]
    fn since_seq_is_incremental_and_clear_keeps_cursor() {
        let mut log = UserCommandLog::default();
        log.push_command("a", 1, 0);
        log.push_command("b", 2, 0);
        assert_eq!(log.since_seq(1).len(), 1);
        log.clear();
        assert!(log.recent(5).is_empty());
        let e = log.push_command("c", 3, 0).unwrap();
        assert_eq!(e.seq, 3);
    }

    #[test]
    fn secret_entry_has_no_content_and_actor_user() {
        let mut log = UserCommandLog::default();
        let e = log.push_secret(5, 42);
        assert_eq!(e.kind, UserEntryKind::SecretInput);
        assert_eq!(e.line, "");
        assert_eq!(e.actor, Actor::User);
        assert_eq!(e.at, 42);
    }

    #[test]
    fn redacted_masks_secrets_in_line_but_not_original() {
        let mut log = UserCommandLog::default();
        let e = log
            .push_command("curl -H 'Authorization: Bearer sk-abcdefghijklmnopqrstuvwx' http://x", 1, 0)
            .unwrap();
        let r = e.redacted();
        assert!(!r.line.contains("sk-abcdefghijklmnopqrstuvwx"), "{}", r.line);
        assert_eq!(r.seq, e.seq);
        assert!(e.line.contains("sk-abcdefghijklmnopqrstuvwx"));
    }
}
