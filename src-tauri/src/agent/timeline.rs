//! 统一时间线：一个终端会话里「谁在什么时候做了什么」的事件流。
//!
//! 数据来自四路，合并成同一条按 `seq` 排序的流：
//!
//! 1. 用户提交的命令（`usercmd.rs` / 2.4，回车时由前端推来）；
//! 2. Agent 在 `act.rs` 里做的动作（exec / keys / wait / screen / secret），只记「做了什么」；
//! 3. 检出的交互提示（`prompt.rs` 的分类结果，只在值得注意的时刻记：Agent 动作返回时
//!    屏幕停在密码 / 确认 / 分页器等提示上）；
//! 4. 每条命令对应的**输出摘要**：用旁路缓冲的 `at` / `pushed` 坐标切出该命令之后的程序输出，
//!    只留尾部若干行、限长、脱敏。输出作为**独立的 `output` 事件追加**（`ref_seq` 指回命令），
//!    于是整条流是只追加的，前端按 `seq` 增量拉取 / 订阅即可，不需要处理「改写已发事件」。
//!
//! ## 硬规矩
//!
//! - **绝不存密钥**：用户的无回显输入与 Agent 的 `nx_secret` 都只记 `secret` 占位，`text` 恒为空。
//!   Agent 一侧的文本与所有输出摘要入账前就过 `redact::redact`（纵深防御：哪怕模型
//!   把口令塞进 `nx_send_keys`，账上也没有明文）。
//! - **有界**：每会话环形 [`MAX_EVENTS`] 条；单条文本（沿用 `usercmd::MAX_LINE_BYTES`）、输出摘要都有字节上限。
//! - **给模型看必须脱敏**：用户命令行入账保持原文（与 2.4 一致，界面要看原貌），
//!   [`Timeline::digest`] 出口再按账户开关统一脱敏（模型只通过它看到时间线）。
//!
//! 本文件的 [`Timeline`] 是纯数据结构（不碰锁、不碰 Tauri），[`AgentManager`] 上的
//! `timeline_*` 方法把它与旁路缓冲粘起来，末尾的 [`begin`] / [`finish`] 再负责推送事件。

use std::collections::{HashMap, VecDeque};

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use super::buffer::strip_ansi;
use super::prompt::{PromptGuess, PromptKind};
use super::redact;
use super::usercmd::{self, Actor, UserCommandEntry, UserEntryKind};
use super::AgentManager;

/// 单会话保留的事件条数上限。环形：超了丢最老的。
pub const MAX_EVENTS: usize = 512;

/// 输出摘要最多保留的行数 / 每行字符数 / 总字节数（取尾部）。
pub const MAX_OUTPUT_LINES: usize = 20;
pub const MAX_OUTPUT_LINE_CHARS: usize = 240;
pub const MAX_OUTPUT_BYTES: usize = 2048;

/// 做摘要前先把原文裁到这么长（取尾部），防止对一大坨输出跑脱敏正则。
const SUMMARY_PRETRIM_BYTES: usize = 64 * 1024;

/// 用户命令的输出在「安静下来」之前最多等多久（防抖兜底），见 [`schedule_user_flush`]。
const FLUSH_MAX_WAIT_MS: u64 = 12_000;
const FLUSH_QUIET_MS: u64 = 800;

/// 给模型的「用户最近操作」摘要：最多几条、多大、多久以内。
pub const DIGEST_MAX_EVENTS: usize = 8;
pub const DIGEST_MAX_BYTES: usize = 1200;
pub const DIGEST_MAX_AGE_MS: u64 = 30 * 60 * 1000;
const DIGEST_LINE_CHARS: usize = 200;

/// 事件类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// 提交了一条命令。`text` 是命令行。
    Command,
    /// 发了原始按键（Agent 的 `nx_send_keys`）。`text` 是可读的按键描述。
    Keys,
    /// 检出一个交互提示。`text` 是提示行，`prompt_kind` / `confidence` 是分类。
    Prompt,
    /// 一条命令 / 动作之后的输出摘要。`ref_seq` 指回它。
    Output,
    /// 密码 / 口令输入的占位。**`text` 恒为空。**
    Secret,
    /// Agent 等一段文字出现 / 输出静止（`nx_wait`）。`text` 是等的条件。
    Wait,
    /// Agent 读了一次屏幕（`nx_screen`）。
    Screen,
    /// P5 统一审计：托管 Steward 产出的一条 DecisionRecord（`text` 是 rationale 摘要）。
    Decision,
    /// Agent 的文件工具（读 / 写 / 改 / 查找 / 搜索）。`text` 是一句摘要，写入时输出是差异预览。
    File,
}

/// 一条时间线事件。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineEvent {
    /// 会话内单调递增（从 1 起，`clear` 也不归零）。
    pub seq: u64,
    /// 发生时刻，Unix 毫秒。
    pub ts_ms: u64,
    pub actor: Actor,
    pub kind: Kind,
    pub text: String,
    /// 简短状态：`已确认执行完毕` / `超时` / `已取消` / secret 的结局码等。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// `Output` 事件指向它所属的命令 / 动作事件的 `seq`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_seq: Option<u64>,
    /// `Prompt` 事件的分类（`prompt::PromptKind::as_str`）与置信度。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    /// `text` 被截断过。
    pub truncated: bool,
}

impl TimelineEvent {
    fn new(actor: Actor, kind: Kind, text: String, ts_ms: u64) -> Self {
        Self {
            seq: 0,
            ts_ms,
            actor,
            kind,
            text,
            note: None,
            ref_seq: None,
            prompt_kind: None,
            confidence: None,
            truncated: false,
        }
    }
}

/// 还没落账输出的那条用户命令：`at` 是它提交时旁路缓冲的 `pushed` 坐标。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pending {
    cmd_seq: u64,
    at: u64,
}

/// 单会话的时间线（纯数据，不含锁）。
#[derive(Debug, Default)]
pub struct Timeline {
    events: VecDeque<TimelineEvent>,
    next_seq: u64,
    pending: Option<Pending>,
}

impl Timeline {
    fn push(&mut self, mut ev: TimelineEvent) -> TimelineEvent {
        self.next_seq += 1;
        ev.seq = self.next_seq;
        self.events.push_back(ev.clone());
        while self.events.len() > MAX_EVENTS {
            self.events.pop_front();
        }
        ev
    }

    /// 并入一条用户命令事件（来自 2.4 的 `UserCommandEntry`）。
    ///
    /// `SecretInput` 只记占位；`Command` 会成为「待落账输出」的那条（上一条没落账的会被顶掉，
    /// 调用方应先 [`Self::take_pending`] 把它处理掉）。
    pub fn push_user(&mut self, e: &UserCommandEntry) -> TimelineEvent {
        match e.kind {
            UserEntryKind::SecretInput => self.push(TimelineEvent::new(
                Actor::User,
                Kind::Secret,
                String::new(),
                e.ts_ms,
            )),
            UserEntryKind::Command => {
                let mut ev =
                    TimelineEvent::new(Actor::User, Kind::Command, e.line.clone(), e.ts_ms);
                ev.truncated = e.truncated;
                let ev = self.push(ev);
                self.pending = Some(Pending { cmd_seq: ev.seq, at: e.at });
                ev
            }
        }
    }

    /// 并入一条 Agent 动作。`text` 入账前脱敏并限长；`Kind::Secret` 的 `text` 一律清空。
    pub fn push_agent(
        &mut self,
        kind: Kind,
        text: &str,
        note: Option<&str>,
        ts_ms: u64,
    ) -> TimelineEvent {
        let (text, truncated) = if kind == Kind::Secret {
            (String::new(), false)
        } else {
            clip_text(&redact::redact(text))
        };
        let mut ev = TimelineEvent::new(Actor::Agent, kind, text, ts_ms);
        ev.note = note.map(|n| clip_text(&redact::redact(n)).0);
        ev.truncated = truncated;
        self.push(ev)
    }

    /// 并入一条用户的按键事件（只在录制期间由录制器写入，`text` 是 `nx_send_keys` 语法的
    /// 白名单按键，如 `<C-c>`）。调用方须保证 `text` 来自白名单，这里照单限长。
    pub fn push_user_keys(&mut self, text: &str, ts_ms: u64) -> TimelineEvent {
        let (text, truncated) = clip_text(text);
        let mut ev = TimelineEvent::new(Actor::User, Kind::Keys, text, ts_ms);
        ev.truncated = truncated;
        self.push(ev)
    }

    /// 最近一条事件的 seq（没有事件时为 0）。`next_seq` 与环形裁剪无关、只增不减，
    /// 录制器用它标记「从哪之后算我这段录制」。
    pub fn last_seq(&self) -> u64 {
        self.next_seq
    }

    /// 取走「待落账输出」的那条用户命令，返回 `(命令 seq, 提交时坐标)`。
    pub fn take_pending(&mut self) -> Option<(u64, u64)> {
        self.pending.take().map(|p| (p.cmd_seq, p.at))
    }

    /// 当前待落账的命令 seq（防抖任务用来确认「还是不是我等的那条」）。
    pub fn pending_seq(&self) -> Option<u64> {
        self.pending.map(|p| p.cmd_seq)
    }

    /// 并入一条输出摘要。摘要为空（没输出）时不入账，返回 `None`。
    pub fn push_output(
        &mut self,
        ref_seq: Option<u64>,
        actor: Actor,
        raw: &str,
        ts_ms: u64,
    ) -> Option<TimelineEvent> {
        let (text, truncated) = summarize_output(raw);
        if text.is_empty() {
            return None;
        }
        let mut ev = TimelineEvent::new(actor, Kind::Output, text, ts_ms);
        ev.ref_seq = ref_seq;
        ev.truncated = truncated;
        Some(self.push(ev))
    }

    /// 并入一条「检出提示」事件。与最近一条提示事件**完全相同**（同类别、同提示行）
    /// 且其后没有别的事件时视为重复，不入账。
    pub fn push_prompt(
        &mut self,
        actor: Actor,
        guess: &PromptGuess,
        line: &str,
        ts_ms: u64,
    ) -> Option<TimelineEvent> {
        let (text, truncated) = clip_text(&redact::redact(line.trim()));
        let kind = guess.kind.as_str();
        if let Some(last) = self.events.back() {
            if last.kind == Kind::Prompt
                && last.prompt_kind.as_deref() == Some(kind)
                && last.text == text
            {
                return None;
            }
        }
        let mut ev = TimelineEvent::new(actor, Kind::Prompt, text, ts_ms);
        ev.prompt_kind = Some(kind.to_string());
        ev.confidence = Some(guess.confidence);
        ev.truncated = truncated;
        Some(self.push(ev))
    }

    /// `seq` 大于 `after` 的事件，最多 `limit` 条。超出时**保留最新的**
    /// （首屏 `after=0` 要的是最近一段，而不是最古老的一段）。
    pub fn since(&self, after: u64, limit: usize) -> Vec<TimelineEvent> {
        let fresh: Vec<&TimelineEvent> = self.events.iter().filter(|e| e.seq > after).collect();
        let skip = fresh.len().saturating_sub(limit);
        fresh.into_iter().skip(skip).cloned().collect()
    }

    /// 清空事件与待落账状态，`next_seq` 保留（读者手里的游标不能倒退）。
    pub fn clear(&mut self) {
        self.events.clear();
        self.pending = None;
    }

    /// 给模型的「用户最近在终端里做了什么」摘要（只含用户的命令 / 密码输入占位，
    /// 不含 Agent 自己的动作——那些模型本来就知道）。没有可说的就返回空串。
    ///
    /// `redact_on` 对应账户的脱敏开关；`now_ms` 作参数以便测试。
    pub fn digest(&self, now_ms: u64, redact_on: bool) -> String {
        let mut picked: Vec<&TimelineEvent> = self
            .events
            .iter()
            .filter(|e| {
                e.actor == Actor::User
                    && matches!(e.kind, Kind::Command | Kind::Secret)
                    && now_ms.saturating_sub(e.ts_ms) <= DIGEST_MAX_AGE_MS
            })
            .collect();
        let skip = picked.len().saturating_sub(DIGEST_MAX_EVENTS);
        picked.drain(..skip);

        let mut lines: Vec<String> = picked
            .iter()
            .map(|e| {
                let ago = ago_label(now_ms.saturating_sub(e.ts_ms));
                match e.kind {
                    Kind::Secret => format!("· {ago}：输入了一次密码 / 口令（内容未记录）"),
                    _ => {
                        let text = if redact_on { redact::redact(&e.text) } else { e.text.clone() };
                        let (text, cut) = head_chars(&text, DIGEST_LINE_CHARS);
                        format!("· {ago}：执行 `{text}`{}", if cut || e.truncated { " …" } else { "" })
                    }
                }
            })
            .collect();
        // 总量超限就丢最老的：越新的越有用
        while !lines.is_empty() && lines.iter().map(|l| l.len() + 1).sum::<usize>() > DIGEST_MAX_BYTES {
            lines.remove(0);
        }
        lines.join("\n")
    }
}

/// 「3 分钟前」式的相对时间。
fn ago_label(ms: u64) -> String {
    let s = ms / 1000;
    if s < 5 {
        "刚刚".to_string()
    } else if s < 60 {
        format!("{s} 秒前")
    } else if s < 3600 {
        format!("{} 分钟前", s / 60)
    } else {
        format!("{} 小时前", s / 3600)
    }
}

/// 取前 `max` 个字符，返回 `(内容, 是否截断)`。
fn head_chars(s: &str, max: usize) -> (String, bool) {
    if s.chars().count() <= max {
        return (s.to_string(), false);
    }
    (s.chars().take(max).collect(), true)
}

/// 单条文本限长：清洗成单行安全文本、按字节上限在字符边界截断。
fn clip_text(raw: &str) -> (String, bool) {
    let (s, cut) = usercmd::sanitize_line(raw);
    (s, cut)
}

/// 输出摘要：剥 ANSI → 脱敏 → 去空行 → 只留尾部 [`MAX_OUTPUT_LINES`] 行 →
/// 每行限 [`MAX_OUTPUT_LINE_CHARS`] 字符 → 总量限 [`MAX_OUTPUT_BYTES`] 字节。
/// 返回 `(摘要, 是否有内容被裁掉)`。
pub fn summarize_output(raw: &str) -> (String, bool) {
    let mut truncated = false;
    let mut s = strip_ansi(raw);
    if s.len() > SUMMARY_PRETRIM_BYTES {
        let mut start = s.len() - SUMMARY_PRETRIM_BYTES;
        while !s.is_char_boundary(start) {
            start += 1;
        }
        s = s[start..].to_string();
        truncated = true;
    }
    // 先脱敏再裁：PEM 之类跨行的结构得在完整的情况下认出来
    let s = redact::redact(&s);

    let mut lines: Vec<String> = s
        .lines()
        .map(|l| l.trim_end().replace('\t', "  "))
        .map(|l| l.chars().filter(|c| !c.is_control()).collect::<String>())
        .filter(|l| !l.trim().is_empty())
        .collect();
    if lines.len() > MAX_OUTPUT_LINES {
        lines.drain(..lines.len() - MAX_OUTPUT_LINES);
        truncated = true;
    }
    for l in &mut lines {
        if l.chars().count() > MAX_OUTPUT_LINE_CHARS {
            *l = l.chars().take(MAX_OUTPUT_LINE_CHARS).collect::<String>() + "…";
            truncated = true;
        }
    }
    let mut out = lines.join("\n");
    if out.len() > MAX_OUTPUT_BYTES {
        let mut start = out.len() - MAX_OUTPUT_BYTES;
        while !out.is_char_boundary(start) {
            start += 1;
        }
        // 尽量对齐到下一行开头，别留半行
        if let Some(rel) = out[start..].find('\n') {
            if start + rel + 1 < out.len() {
                start += rel + 1;
            }
        }
        out = out[start..].to_string();
        truncated = true;
    }
    (out, truncated)
}

/// 一次分类结果值不值得在时间线上记一笔：非 ShellPrompt / Unknown 且置信度够。
/// `force`（如 `wait_for_end` 已判 awaiting_input）时只要不是 ShellPrompt / Unknown 就记。
pub fn is_notable(guess: &PromptGuess, force: bool) -> bool {
    if matches!(guess.kind, PromptKind::ShellPrompt | PromptKind::Unknown) {
        return false;
    }
    force || guess.confidence >= 0.6
}

// ——————————————————————————————— 与旁路缓冲粘合 ———————————————————————————————

impl AgentManager {
    /// 并入一条用户命令事件，返回新增的事件（可能先带上一条命令的输出摘要，再是命令本身）。
    /// 与 `record_user_command` 同一开关语义：总开关关着 / 无该会话缓冲时由调用方保证不会走到这里。
    pub fn timeline_user(&self, session_id: &str, entry: &UserCommandEntry) -> Vec<TimelineEvent> {
        let mut out = self.timeline_flush(session_id, None);
        let mut guard = self.timelines.lock();
        out.push(guard.entry(session_id.to_string()).or_default().push_user(entry));
        out
    }

    /// 把「待落账」那条用户命令的输出摘要落账。`expect` 非空时，只有待落账的恰好是它才动
    /// （防抖任务晚到、而下一条命令已经把它冲掉的情形）。返回 0 或 1 条事件。
    pub fn timeline_flush(&self, session_id: &str, expect: Option<u64>) -> Vec<TimelineEvent> {
        let (cmd_seq, at) = {
            let mut guard = self.timelines.lock();
            let Some(tl) = guard.get_mut(session_id) else { return Vec::new() };
            if let Some(want) = expect {
                if tl.pending_seq() != Some(want) {
                    return Vec::new();
                }
            }
            match tl.take_pending() {
                Some(p) => p,
                None => return Vec::new(),
            }
        };
        // 两把锁不嵌套：先放掉 timelines 再去取缓冲
        let raw = self
            .sessions
            .lock()
            .get(session_id)
            .map(|b| b.out_since(at))
            .unwrap_or_default();
        let mut guard = self.timelines.lock();
        guard
            .entry(session_id.to_string())
            .or_default()
            .push_output(Some(cmd_seq), Actor::User, &raw, usercmd::now_ms())
            .into_iter()
            .collect()
    }

    /// 记一条 Agent 动作。返回 `(该动作事件的 seq, 新增事件)`；总开关关着时 `(0, [])`。
    /// 先冲掉待落账的用户输出，保证事件顺序就是真实顺序。
    pub fn timeline_agent(
        &self,
        session_id: &str,
        kind: Kind,
        text: &str,
        note: Option<&str>,
    ) -> (u64, Vec<TimelineEvent>) {
        if !self.is_global_enabled() {
            return (0, Vec::new());
        }
        let mut out = self.timeline_flush(session_id, None);
        let mut guard = self.timelines.lock();
        let ev = guard
            .entry(session_id.to_string())
            .or_default()
            .push_agent(kind, text, note, usercmd::now_ms());
        let seq = ev.seq;
        out.push(ev);
        (seq, out)
    }

    /// 记 Agent 动作的结果：输出摘要 +（值得时）一条检出提示。
    /// `ref_seq` 是 [`Self::timeline_agent`] 返回的 seq；0 视为没有。
    pub fn timeline_result(
        &self,
        session_id: &str,
        ref_seq: u64,
        output: &str,
        prompt: Option<(&PromptGuess, &str, bool)>,
    ) -> Vec<TimelineEvent> {
        if !self.is_global_enabled() {
            return Vec::new();
        }
        let ts = usercmd::now_ms();
        let mut guard = self.timelines.lock();
        let tl = guard.entry(session_id.to_string()).or_default();
        let mut out = Vec::new();
        let rseq = (ref_seq != 0).then_some(ref_seq);
        out.extend(tl.push_output(rseq, Actor::Agent, output, ts));
        if let Some((guess, line, force)) = prompt {
            if is_notable(guess, force) {
                out.extend(tl.push_prompt(Actor::Agent, guess, line, ts));
            }
        }
        out
    }

    /// 增量读取（给前端）：`seq` 大于 `since` 的事件，最多 `limit` 条（超出保留最新的）。**原文。**
    pub fn timeline_since(&self, session_id: &str, since: u64, limit: usize) -> Vec<TimelineEvent> {
        self.timelines
            .lock()
            .get(session_id)
            .map(|t| t.since(since, limit))
            .unwrap_or_default()
    }

    /// 给模型的「用户最近操作」摘要，见 [`Timeline::digest`]。没有就空串。
    pub fn timeline_digest(&self, session_id: &str, redact_on: bool) -> String {
        self.timelines
            .lock()
            .get(session_id)
            .map(|t| t.digest(usercmd::now_ms(), redact_on))
            .unwrap_or_default()
    }

    /// 清空该会话的时间线（与 `clear` / 关会话联动）。
    pub fn timeline_clear(&self, session_id: &str) {
        if let Some(t) = self.timelines.lock().get_mut(session_id) {
            t.clear();
        }
    }

    fn is_global_enabled(&self) -> bool {
        self.enabled.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// 会话时间线表（`AgentManager` 的字段类型）。
pub type Timelines = HashMap<String, Timeline>;

// ——————————————————————————————— 事件推送 ———————————————————————————————

/// 推给前端的事件名。载荷是 `Vec<TimelineEvent>`（一次可能带好几条）。
pub fn event_name(session_id: &str) -> String {
    format!("nx://agent/timeline/{session_id}")
}

/// 推送新增事件；空的不推。
pub fn publish(app: &AppHandle, session_id: &str, events: &[TimelineEvent]) {
    if events.is_empty() {
        return;
    }
    let _ = app.emit(&event_name(session_id), events);
}

/// Agent 动作开始：记一条并推送，返回它的 seq（0 = 没记，后续 [`finish`] 当作无引用）。
pub fn begin(
    app: &AppHandle,
    mgr: &AgentManager,
    session_id: &str,
    kind: Kind,
    text: &str,
    note: Option<&str>,
) -> u64 {
    let (seq, evs) = mgr.timeline_agent(session_id, kind, text, note);
    publish(app, session_id, &evs);
    seq
}

/// Agent 动作结束：记输出摘要与（值得时的）提示并推送。
/// `force_prompt`：调用方已确认「命令没结束、停在交互提示上」（`ExecEnd::AwaitingInput`）。
pub fn finish(
    app: &AppHandle,
    mgr: &AgentManager,
    session_id: &str,
    ref_seq: u64,
    output: &str,
    guess: Option<&PromptGuess>,
    force_prompt: bool,
) {
    let line = mgr.last_line(session_id).unwrap_or_default();
    let evs = mgr.timeline_result(
        session_id,
        ref_seq,
        output,
        guess.map(|g| (g, line.as_str(), force_prompt)),
    );
    publish(app, session_id, &evs);
}

/// 用户命令提交后：等输出安静下来（防抖）再把摘要落账并推送。
/// 下一条命令 / Agent 动作先到时会把它冲掉，这里 `expect` 校验会让晚到的任务空转。
pub fn schedule_user_flush(app: AppHandle, session_id: String, cmd_seq: u64) {
    use tauri::Manager;
    tauri::async_runtime::spawn(async move {
        let started = std::time::Instant::now();
        let mut last = None;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(FLUSH_QUIET_MS)).await;
            let state = app.state::<crate::cmd::AppState>();
            let now = state.agent.pushed(&session_id);
            let quiet = now == last;
            last = now;
            if quiet || started.elapsed().as_millis() as u64 >= FLUSH_MAX_WAIT_MS {
                let evs = state.agent.timeline_flush(&session_id, Some(cmd_seq));
                publish(&app, &session_id, &evs);
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::buffer::StreamKind;
    use crate::agent::prompt::classify;
    use crate::agent::usercmd::UserCommandLog;
    use crate::agent::AgentConfig;

    fn user_cmd(line: &str, ts: u64, at: u64) -> UserCommandEntry {
        UserCommandLog::default().push_command(line, ts, at).unwrap()
    }

    fn mgr() -> AgentManager {
        let m = AgentManager::new();
        m.apply_config(&AgentConfig { enabled: true, ..AgentConfig::default() });
        m
    }

    #[test]
    fn user_command_and_secret_are_merged_with_monotonic_seq() {
        let mut tl = Timeline::default();
        let a = tl.push_user(&user_cmd("ls", 1, 0));
        let mut log = UserCommandLog::default();
        let s = tl.push_user(&log.push_secret(2, 5));
        assert_eq!((a.seq, a.actor, a.kind), (1, Actor::User, Kind::Command));
        assert_eq!((s.seq, s.kind), (2, Kind::Secret));
        assert_eq!(s.text, "");
    }

    #[test]
    fn agent_secret_never_stores_text_and_other_text_is_redacted() {
        let mut tl = Timeline::default();
        let s = tl.push_agent(Kind::Secret, "hunter2-the-password", Some("delivered"), 1);
        assert_eq!(s.text, "");
        assert_eq!(s.actor, Actor::Agent);
        let k = tl.push_agent(
            Kind::Keys,
            "curl -H 'Authorization: Bearer sk-abcdefghijklmnopqrstuvwx'",
            None,
            2,
        );
        assert!(!k.text.contains("sk-abcdefghijklmnopqrstuvwx"), "{}", k.text);
        assert!(!format!("{:?}", tl.since(0, 10)).contains("hunter2"));
    }

    #[test]
    fn ring_is_bounded_and_since_is_incremental() {
        let mut tl = Timeline::default();
        for i in 0..(MAX_EVENTS + 20) {
            tl.push_agent(Kind::Command, &format!("cmd {i}"), None, i as u64);
        }
        let all = tl.since(0, usize::MAX);
        assert_eq!(all.len(), MAX_EVENTS);
        assert_eq!(all.last().unwrap().seq, (MAX_EVENTS + 20) as u64);
        assert_eq!(all.first().unwrap().seq, 21);
        // limit 保留最新的
        let last3 = tl.since(0, 3);
        assert_eq!(last3.len(), 3);
        assert_eq!(last3[2].seq, (MAX_EVENTS + 20) as u64);
        // 增量
        let top = all.last().unwrap().seq;
        assert!(tl.since(top, 10).is_empty());
        tl.push_agent(Kind::Wait, "x", None, 0);
        assert_eq!(tl.since(top, 10).len(), 1);
    }

    #[test]
    fn clear_keeps_cursor_and_drops_pending() {
        let mut tl = Timeline::default();
        tl.push_user(&user_cmd("a", 1, 0));
        tl.clear();
        assert!(tl.since(0, 10).is_empty());
        assert!(tl.take_pending().is_none());
        let e = tl.push_agent(Kind::Screen, "", None, 2);
        assert_eq!(e.seq, 2);
    }

    #[test]
    fn output_summary_is_tail_bounded_and_redacted() {
        let raw: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let (s, t) = summarize_output(&raw);
        assert!(t);
        assert_eq!(s.lines().count(), MAX_OUTPUT_LINES);
        assert!(s.ends_with("line 99"));

        let long = "x".repeat(MAX_OUTPUT_LINE_CHARS * 3);
        let (s, t) = summarize_output(&long);
        assert!(t && s.chars().count() <= MAX_OUTPUT_LINE_CHARS + 1);

        let wide: String = (0..MAX_OUTPUT_LINES)
            .map(|i| format!("{i:02} {}\n", "y".repeat(MAX_OUTPUT_LINE_CHARS - 4)))
            .collect();
        let (s, _) = summarize_output(&wide);
        assert!(s.len() <= MAX_OUTPUT_BYTES, "{}", s.len());

        let (s, _) = summarize_output("token=1\nAuthorization: Bearer sk-abcdefghijklmnopqrstuvwx\n");
        assert!(!s.contains("sk-abcdefghijklmnopqrstuvwx"), "{s}");

        assert_eq!(summarize_output("\n  \n\u{1b}[0m"), (String::new(), false));
    }

    #[test]
    fn output_with_multibyte_does_not_panic() {
        let raw = "命令输出".repeat(2000);
        let (s, t) = summarize_output(&raw);
        assert!(t);
        assert!(s.len() <= MAX_OUTPUT_BYTES + 4);
    }

    #[test]
    fn empty_output_is_not_recorded() {
        let mut tl = Timeline::default();
        assert!(tl.push_output(Some(1), Actor::User, "  \n", 1).is_none());
    }

    #[test]
    fn prompt_events_dedupe_only_when_consecutive_and_identical() {
        let mut tl = Timeline::default();
        let g = classify("[sudo] password for chris:", false);
        assert_eq!(g.kind, PromptKind::Password);
        assert!(tl.push_prompt(Actor::Agent, &g, "[sudo] password for chris:", 1).is_some());
        assert!(tl.push_prompt(Actor::Agent, &g, "[sudo] password for chris:", 2).is_none());
        tl.push_agent(Kind::Secret, "", None, 3);
        assert!(tl.push_prompt(Actor::Agent, &g, "[sudo] password for chris:", 4).is_some());
    }

    #[test]
    fn notable_prompts() {
        let pw = classify("Password:", false);
        assert!(is_notable(&pw, false));
        let sh = classify("user@host:~$ ", false);
        assert!(!is_notable(&sh, true));
    }

    #[test]
    fn digest_lists_user_commands_only_redacted_and_capped() {
        let mut tl = Timeline::default();
        let now = 10_000_000u64;
        tl.push_user(&user_cmd("ls -la", now - 120_000, 0));
        tl.push_agent(Kind::Command, "uptime", None, now - 100_000);
        tl.push_user(&user_cmd("curl -H 'Authorization: Bearer sk-abcdefghijklmnopqrstuvwx' x", now - 10_000, 0));
        let mut log = UserCommandLog::default();
        tl.push_user(&log.push_secret(now - 2_000, 0));
        let d = tl.digest(now, true);
        assert!(d.contains("ls -la") && d.contains("2 分钟前"), "{d}");
        assert!(!d.contains("uptime"), "{d}");
        assert!(!d.contains("sk-abcdefghijklmnopqrstuvwx"), "{d}");
        assert!(d.contains("刚刚") && d.contains("内容未记录"), "{d}");
        // 关脱敏时保持原文
        assert!(tl.digest(now, false).contains("sk-abcdefghijklmnopqrstuvwx"));
    }

    #[test]
    fn digest_ignores_stale_and_respects_caps() {
        let mut tl = Timeline::default();
        let now = 100_000_000u64;
        tl.push_user(&user_cmd("old", now - DIGEST_MAX_AGE_MS - 1, 0));
        assert_eq!(tl.digest(now, true), "");
        for i in 0..40 {
            tl.push_user(&user_cmd(&format!("echo {i} {}", "z".repeat(150)), now - 1000, 0));
        }
        let d = tl.digest(now, true);
        assert!(d.len() <= DIGEST_MAX_BYTES, "{}", d.len());
        assert!(d.lines().count() <= DIGEST_MAX_EVENTS);
        assert!(d.contains("echo 39"));
    }

    // ——— 与缓冲粘合 ———

    #[test]
    fn manager_attaches_output_between_commands_only() {
        let m = mgr();
        m.feed("s", b"user@h:~$ ", StreamKind::Out);
        let e1 = m.record_user_command("s", "ls", false).unwrap();
        // 回车后的程序输出
        m.feed("s", b"a.txt\r\nb.txt\r\n", StreamKind::Out);
        // 用户已经在敲下一条：这些输入不能算进 ls 的输出
        m.feed("s", b"pwd", StreamKind::In);
        let e2 = m.record_user_command("s", "pwd", false).unwrap();
        let evs = m.timeline_user("s", &e1);
        assert_eq!(evs.len(), 1);
        let evs = m.timeline_user("s", &e2);
        // 第一次调用没有待落账；第二次先落 ls 的输出，再是 pwd 命令
        assert_eq!(evs.len(), 2, "{evs:?}");
        assert_eq!(evs[0].kind, Kind::Output);
        assert_eq!(evs[0].ref_seq, Some(1));
        assert_eq!(evs[0].actor, Actor::User);
        assert!(evs[0].text.contains("a.txt") && evs[0].text.contains("b.txt"), "{:?}", evs[0]);
        assert!(!evs[0].text.contains("pwd"), "{:?}", evs[0]);
        assert_eq!(evs[1].kind, Kind::Command);
        assert_eq!(evs[1].text, "pwd");
    }

    #[test]
    fn manager_flush_respects_expect_and_is_idempotent() {
        let m = mgr();
        let e = m.record_user_command("s", "echo hi", false).unwrap();
        m.timeline_user("s", &e);
        m.feed("s", b"hi\r\n", StreamKind::Out);
        assert!(m.timeline_flush("s", Some(99)).is_empty());
        assert_eq!(m.timeline_flush("s", Some(1)).len(), 1);
        assert!(m.timeline_flush("s", Some(1)).is_empty());
        assert_eq!(m.timeline_since("s", 0, 10).len(), 2);
    }

    #[test]
    fn manager_agent_flow_records_action_output_and_prompt() {
        let m = mgr();
        let (seq, evs) = m.timeline_agent("s", Kind::Command, "sudo ls", Some("exec"));
        assert_eq!(seq, 1);
        assert_eq!(evs.len(), 1);
        let g = classify("[sudo] password for chris:", false);
        let out = m.timeline_result("s", seq, "[sudo] password for chris:", Some((&g, "[sudo] password for chris:", true)));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].kind, Kind::Output);
        assert_eq!(out[0].ref_seq, Some(1));
        assert_eq!(out[1].kind, Kind::Prompt);
        assert_eq!(out[1].prompt_kind.as_deref(), Some("password"));
        // 密码占位
        let (s, _) = m.timeline_agent("s", Kind::Secret, "输入 sudo 密码", Some("delivered"));
        let all = m.timeline_since("s", 0, 50);
        assert_eq!(all.iter().find(|e| e.seq == s).unwrap().text, "");
    }

    #[test]
    fn manager_off_records_nothing_and_remove_clear_work() {
        let off = AgentManager::new();
        assert_eq!(off.timeline_agent("s", Kind::Keys, "x", None).0, 0);
        assert!(off.timeline_since("s", 0, 10).is_empty());

        let m = mgr();
        m.timeline_agent("s", Kind::Keys, "x", None);
        m.timeline_clear("s");
        assert!(m.timeline_since("s", 0, 10).is_empty());
        m.timeline_agent("s", Kind::Keys, "y", None);
        m.remove("s");
        assert!(m.timeline_since("s", 0, 10).is_empty());
    }

    #[test]
    fn event_serializes_camel_case_without_empty_options() {
        let mut tl = Timeline::default();
        let e = tl.push_agent(Kind::Keys, "<C-c>", None, 7);
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["tsMs"], 7);
        assert_eq!(v["actor"], "agent");
        assert_eq!(v["kind"], "keys");
        assert!(v.get("refSeq").is_none() && v.get("note").is_none());
    }
}
