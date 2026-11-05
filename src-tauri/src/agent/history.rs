//! 对话与运行持久化（Phase 2.6）的纯逻辑：落盘前的清洗、一轮运行的摘要、摘要拼进提示词。
//!
//! 不碰数据库也不碰 `AppHandle`，便于测试；落盘在 `store::agent_history`，
//! 接线在 `act::run`（记步骤 + 取摘要）与 `agent::cmd`（面板的读 / 写 / 清空）。
//!
//! # 范围（scope）
//!
//! Agent 面板对话本来就是**全局唯一一份**（见 `AgentPanel` 的 `conv`：切标签不换聊天记录），
//! 所以持久化也是全局一份，而不是「每个会话 / 每个标签一份」：标签页 id 每次启动都会变，
//! 按它分桶重启后一份都对不上。运行摘要同理全局，摘要里写明了在哪台机器上做的。
//!
//! # 安全约束
//!
//! - 落盘的每一个字符串都过 [`redact::redact`]（**不看账号的 redact 开关**）：
//!   开关管的是「能不能发给模型」，而落盘是「静止数据」，库里本不该躺着 token / 口令形态的文本。
//!   这样「重载后摘要再发给模型」也天然是脱敏的，不依赖开关当时的值。
//! - 密码通道的值从不进步骤（`StepPayload` 里只有 `secret_status`），这里再加一道：
//!   名叫 `value` / `password` 的键一律丢弃。
//! - 数据里不含 API Key：消息与摘要都不是从账号配置里来的。

use std::collections::HashMap;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::act::StepPayload;
use super::redact::redact;

/// 面板对话最多保留多少条消息（含步骤卡片）。
pub const MAX_MESSAGES: usize = 200;
/// 单个字符串字段的字符上限（命令输出最容易超）。
pub const MAX_FIELD_CHARS: usize = 8000;
/// 整份对话序列化后的字节上限；超了就从最旧的开始丢。
pub const MAX_TOTAL_BYTES: usize = 1_000_000;
/// 库里最多留多少条运行摘要。
pub const MAX_SUMMARIES_KEPT: usize = 20;
/// 每次运行带进上下文的摘要条数上限。
pub const PRIOR_RUNS_IN_CONTEXT: usize = 3;
/// 拼进提示词的摘要块的字符上限。
pub const PRIOR_BLOCK_MAX_CHARS: usize = 3000;

/// 这些键下的值是 id / 枚举，不是自然语言：脱敏会误伤（uuid 可能被当成 token），也没有必要。
const ID_KEYS: &[&str] = &[
    "id", "role", "kind", "requestId", "target", "tool", "promptKind", "secretStatus",
    "secretResult", "sessionId",
];
/// 即使上游出了 bug 把值塞进来，这些键也不落盘。
const DROP_KEYS: &[&str] = &["value", "password", "passphrase", "secretValue", "apiKey"];

// ——————————————————————————————— 对话落盘前的清洗 ———————————————————————————————

/// 取前 `max` 个字符；被截断时补一个标记。
pub fn head_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// 取后 `max` 个字符（屏幕尾部才是现场）。
pub fn tail_chars(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let tail: String = s.chars().skip(n - max).collect();
    format!("…{tail}")
}

fn clean_value(key: Option<&str>, v: Value) -> Value {
    match v {
        Value::String(s) => {
            if key.is_some_and(|k| ID_KEYS.contains(&k)) {
                Value::String(head_chars(&s, 200))
            } else {
                Value::String(head_chars(&redact(&s), MAX_FIELD_CHARS))
            }
        }
        Value::Array(items) => {
            Value::Array(items.into_iter().map(|x| clean_value(key, x)).collect())
        }
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(k, _)| !DROP_KEYS.contains(&k.as_str()))
                .map(|(k, x)| {
                    let cleaned = clean_value(Some(&k), x);
                    (k, cleaned)
                })
                .collect(),
        ),
        other => other,
    }
}

/// 把面板交来的消息列表清洗成可以落盘的样子：只留最近 [`MAX_MESSAGES`] 条、
/// 非对象丢弃、去掉 `streaming`、所有文本脱敏并截断、总量超 [`MAX_TOTAL_BYTES`] 时丢最旧的。
pub fn sanitize_messages(msgs: Vec<Value>) -> Vec<Value> {
    let objs: Vec<Value> = msgs.into_iter().filter(|m| m.is_object()).collect();
    let skip = objs.len().saturating_sub(MAX_MESSAGES);
    let mut out: Vec<Value> = objs
        .into_iter()
        .skip(skip)
        .map(|m| {
            let mut m = clean_value(None, m);
            if let Some(o) = m.as_object_mut() {
                o.remove("streaming");
            }
            m
        })
        .collect();
    // 逐条算大小再从头丢，避免反复序列化整个数组
    let sizes: Vec<usize> = out.iter().map(|m| m.to_string().len() + 1).collect();
    let mut total: usize = sizes.iter().sum();
    let mut drop = 0;
    while total > MAX_TOTAL_BYTES && drop + 1 < out.len() {
        total -= sizes[drop];
        drop += 1;
    }
    out.drain(..drop);
    out
}

// ——————————————————————————————— 运行摘要 ———————————————————————————————

/// 一轮运行的紧凑摘要。**文本在生成时就已脱敏**（见模块注释）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunSummary {
    /// 结束时间（unix 秒）
    pub ts: i64,
    /// 用户这一轮的目标（原话，截断）
    pub goal: String,
    /// 做过的动作，一步一行，如 `exec[web1] systemctl restart nginx`
    pub actions: Vec<String>,
    /// 结局：模型的结论，或没有结论的原因（被取消 / 出错 / 步数上限）
    pub outcome: String,
    /// 收尾时终端的状态：在哪台机器、提示符类型、屏幕尾部
    pub final_state: String,
}

const GOAL_CHARS: usize = 300;
const ACTION_CHARS: usize = 100;
const MAX_ACTIONS_KEPT: usize = 12;
const OUTCOME_CHARS: usize = 400;
const FINAL_STATE_CHARS: usize = 240;

/// 一轮运行进行中的流水账。`record` 从 `emit_step` 里喂，运行结束由 `finish` 收成摘要。
#[derive(Debug, Clone, Default)]
pub struct RunLog {
    goal: String,
    actions: Vec<String>,
    /// 超过保留条数被挤掉的步数
    dropped: usize,
    answer: Option<String>,
    last_note: Option<String>,
    errored: bool,
    /// 最近一次拿到的终端状态：(展示名, 提示符分类, 屏幕 / 输出尾部)
    last_state: Option<(String, Option<String>, String)>,
}

impl RunLog {
    pub fn new(goal: &str) -> Self {
        Self { goal: goal.trim().to_string(), ..Self::default() }
    }

    fn push_action(&mut self, line: String) {
        // 同一个动作连续出现两次（如 tab 的「正在…」与结果）只记一次
        if self.actions.last() == Some(&line) {
            return;
        }
        if self.actions.len() >= MAX_ACTIONS_KEPT {
            // 保留开头（起因）与结尾（现状），挤掉中间最旧的一条非首条
            self.actions.remove(1);
            self.dropped += 1;
        }
        self.actions.push(line);
    }

    pub fn mark_error(&mut self) {
        self.errored = true;
    }

    /// 记一个步骤事件。只取摘要要用的字段；`tool` / `waiting` 这类纯展示事件忽略。
    pub fn record(&mut self, p: &StepPayload) {
        let label = p.target_label.as_deref().unwrap_or("当前会话");
        let cmd = p.command.as_deref().unwrap_or("").trim();
        match p.kind {
            "exec" => self.push_action(format!("exec[{label}] {cmd}")),
            // 这三类「手和眼」先发进行中（output 为空）再发收尾；动作只在开头记一次
            "keys" | "wait" | "screen" => {
                if p.output.is_none() {
                    self.push_action(format!("{}[{label}] {cmd}", p.kind).trim_end().to_string());
                }
            }
            "secret" => {
                // 只记结局状态，永远没有值；进行中的那条（无状态）不记
                if let Some(st) = p.secret_status.as_deref() {
                    self.push_action(format!("secret[{label}] 请求用户输入密码 → {st}"));
                }
            }
            "open" => {
                if let Some(t) = p.text.as_deref() {
                    self.push_action(format!("open {}", t.trim()));
                }
            }
            "tab" => {
                if let Some(t) = p.text.as_deref() {
                    self.push_action(format!("tab[{label}] {}", t.trim()));
                }
            }
            "export" => self.push_action(format!("export[{label}]")),
            "skip" => self.push_action(format!("未执行: {cmd}")),
            "answer" => self.answer = p.text.clone(),
            "note" => self.last_note = p.text.clone(),
            _ => {}
        }
        // 带输出的收尾（result / keys / wait / screen / secret）刷新「最后状态」
        if matches!(p.kind, "result" | "keys" | "wait" | "screen" | "secret") {
            if let Some(out) = p.output.as_deref() {
                self.last_state = Some((label.to_string(), p.prompt_kind.clone(), out.to_string()));
            }
        }
    }

    /// 收成摘要。什么都没发生（没动作也没结论）返回 `None`：没有信息量的摘要只会占上下文。
    pub fn finish(self, now_secs: i64) -> Option<RunSummary> {
        if self.goal.is_empty() || (self.actions.is_empty() && self.answer.is_none()) {
            return None;
        }
        let outcome = match (&self.answer, self.errored) {
            (Some(a), _) if !a.trim().is_empty() => head_chars(a.trim(), OUTCOME_CHARS),
            (_, true) => "模型调用出错，本轮中断，没有给出结论".to_string(),
            _ => match &self.last_note {
                Some(n) if !n.trim().is_empty() => {
                    format!("没有给出结论（{}）", head_chars(n.trim(), 120))
                }
                _ => "没有给出结论".to_string(),
            },
        };
        let final_state = match &self.last_state {
            Some((label, kind, out)) => {
                let tail = tail_chars(out.trim(), FINAL_STATE_CHARS).replace('\n', " ⏎ ");
                match kind {
                    Some(k) => format!("在「{label}」，终端状态 {k}，屏幕尾部：{tail}"),
                    None => format!("在「{label}」，屏幕尾部：{tail}"),
                }
            }
            None => String::new(),
        };
        let mut actions: Vec<String> =
            self.actions.iter().map(|a| head_chars(a, ACTION_CHARS)).collect();
        if self.dropped > 0 {
            actions.insert(1.min(actions.len()), format!("…（中间省略 {} 步）", self.dropped));
        }
        // 全部文本落盘前脱敏：摘要不看账号开关，永远是脱敏态
        Some(RunSummary {
            ts: now_secs,
            goal: redact(&head_chars(&self.goal, GOAL_CHARS)),
            actions: actions.iter().map(|a| redact(a)).collect(),
            outcome: redact(&outcome),
            final_state: redact(&final_state),
        })
    }
}

/// 各会话进行中的 [`RunLog`]。键是发起会话 id（与 `ActRuntime::inflight` 同键）。
#[derive(Default)]
pub struct RunLogs {
    inner: Mutex<HashMap<String, RunLog>>,
}

impl RunLogs {
    pub fn start(&self, session: &str, goal: &str) {
        self.inner.lock().insert(session.to_string(), RunLog::new(goal));
    }

    /// 没有在记的会话（不是 `act::run` 发起的步骤，如手动 `agent_exec`）直接忽略。
    pub fn record(&self, session: &str, p: &StepPayload) {
        if let Some(log) = self.inner.lock().get_mut(session) {
            log.record(p);
        }
    }

    pub fn mark_error(&self, session: &str) {
        if let Some(log) = self.inner.lock().get_mut(session) {
            log.mark_error();
        }
    }

    pub fn take(&self, session: &str) -> Option<RunLog> {
        self.inner.lock().remove(session)
    }
}

// ——————————————————————————————— 摘要 → 提示词 ———————————————————————————————

fn age_label(now: i64, ts: i64) -> String {
    let d = (now - ts).max(0);
    match d {
        0..=59 => "刚刚".into(),
        60..=3599 => format!("{} 分钟前", d / 60),
        3600..=86399 => format!("{} 小时前", d / 3600),
        _ => format!("{} 天前", d / 86400),
    }
}

/// 把最近几轮摘要渲染成一段拼在第一条 user 消息前面的文本。`summaries` 按时间从旧到新。
/// 空输入返回空串。总长受 [`PRIOR_BLOCK_MAX_CHARS`] 约束（超了丢最旧的）。
///
/// 摘要落盘时已脱敏；调用方出网前仍会再过一次 `scrub`（账号开关为真时），双保险。
pub fn render_prior_runs(summaries: &[RunSummary], now: i64) -> String {
    let take = summaries.len().min(PRIOR_RUNS_IN_CONTEXT);
    let recent = &summaries[summaries.len() - take..];
    let render_one = |i: usize, s: &RunSummary| -> String {
        let mut b = format!("{}. （{}）目标：{}\n", i + 1, age_label(now, s.ts), s.goal);
        if !s.actions.is_empty() {
            b.push_str(&format!("   做了：{}\n", s.actions.join("；")));
        }
        b.push_str(&format!("   结果：{}\n", s.outcome));
        if !s.final_state.is_empty() {
            b.push_str(&format!("   当时终端：{}\n", s.final_state));
        }
        b
    };
    let header = "【此前几轮任务摘要】（从旧到新）供你理解「再来一次 / 继续刚才的」这类指代。\
                  摘要是历史记录、可能已过期：动手前以当前屏幕为准，不要把它当作现状。\n";
    let mut start = 0;
    loop {
        let body: String = recent[start..]
            .iter()
            .enumerate()
            .map(|(i, s)| render_one(i, s))
            .collect();
        if body.is_empty() {
            return String::new();
        }
        if body.chars().count() + header.chars().count() <= PRIOR_BLOCK_MAX_CHARS
            || start + 1 >= recent.len()
        {
            // 只剩一条还超长：硬截断，宁可截断也不能撑爆上下文
            let full = format!("{header}{body}\n");
            return head_chars(&full, PRIOR_BLOCK_MAX_CHARS);
        }
        start += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn step(kind: &'static str) -> StepPayload {
        StepPayload::new(kind, 1)
    }

    #[test]
    fn sanitize_keeps_last_n_and_strips_streaming() {
        let msgs: Vec<Value> = (0..MAX_MESSAGES + 30)
            .map(|i| json!({"id": format!("m{i}"), "role": "user", "text": format!("q{i}"), "streaming": true}))
            .collect();
        let out = sanitize_messages(msgs);
        assert_eq!(out.len(), MAX_MESSAGES);
        assert_eq!(out[0]["id"], "m30", "留的是最近的");
        assert!(out.iter().all(|m| m.get("streaming").is_none()));
    }

    #[test]
    fn sanitize_redacts_terminal_text_but_not_ids() {
        let uid = "3f2b8c1e-aaaa-bbbb-cccc-0123456789ab";
        let m = json!({
            "id": uid, "role": "assistant", "text": "",
            "step": {"kind": "result", "target": uid, "command": "env",
                     "output": "OPENAI sk-ABCDEFGH12345678 mail bob@example.com"}
        });
        let out = sanitize_messages(vec![m]);
        let s = out[0].to_string();
        assert!(!s.contains("sk-ABCDEFGH12345678"));
        assert!(!s.contains("bob@example.com"));
        assert_eq!(out[0]["id"], uid);
        assert_eq!(out[0]["step"]["target"], uid);
    }

    #[test]
    fn sanitize_drops_value_like_keys_and_non_objects() {
        let m = json!({"id":"a","role":"assistant","secret":{"requestId":"r","reason":"sudo","value":"hunter2","password":"x"}});
        let out = sanitize_messages(vec![json!("junk"), json!(3), m]);
        assert_eq!(out.len(), 1);
        let s = out[0].to_string();
        assert!(!s.contains("hunter2") && !s.contains("\"password\""));
        assert!(s.contains("sudo"));
    }

    #[test]
    fn sanitize_truncates_long_fields_and_caps_total() {
        let big = "x".repeat(MAX_FIELD_CHARS * 3);
        let out = sanitize_messages(vec![json!({"id":"a","role":"user","text":big})]);
        assert!(out[0]["text"].as_str().unwrap().chars().count() <= MAX_FIELD_CHARS + 1);

        // 200 条每条 ~8KB（约 1.6MB）> 1MB：应从最旧的开始丢
        let many: Vec<Value> = (0..MAX_MESSAGES)
            .map(|i| json!({"id":format!("m{i}"),"role":"user","text":"y".repeat(MAX_FIELD_CHARS)}))
            .collect();
        let out = sanitize_messages(many);
        assert!(out.len() < MAX_MESSAGES);
        assert!(serde_json::to_string(&out).unwrap().len() <= MAX_TOTAL_BYTES + 1000);
        assert_eq!(out.last().unwrap()["id"], format!("m{}", MAX_MESSAGES - 1));
    }

    #[test]
    fn run_log_builds_summary_of_a_typical_run() {
        let mut log = RunLog::new("重启 nginx");
        log.record(&StepPayload {
            target_label: Some("web1".into()),
            command: Some("systemctl restart nginx".into()),
            ..step("exec")
        });
        log.record(&StepPayload {
            target_label: Some("web1".into()),
            command: Some("systemctl restart nginx".into()),
            output: Some("ok\nuser@web1:~$".into()),
            prompt_kind: Some("shell_prompt".into()),
            ..step("result")
        });
        log.record(&StepPayload { text: Some("nginx 已重启并在监听 80。".into()), ..step("answer") });
        let s = log.finish(1000).unwrap();
        assert_eq!(s.goal, "重启 nginx");
        assert_eq!(s.actions, vec!["exec[web1] systemctl restart nginx"]);
        assert!(s.outcome.contains("已重启"));
        assert!(s.final_state.contains("web1") && s.final_state.contains("shell_prompt"));
    }

    #[test]
    fn run_log_records_hands_and_eyes_once_and_secret_status_only() {
        let mut log = RunLog::new("装包");
        for out in [None, Some("done".to_string())] {
            log.record(&StepPayload {
                target_label: Some("h".into()),
                command: Some("\\x03".into()),
                output: out,
                ..step("keys")
            });
        }
        log.record(&StepPayload { target_label: Some("h".into()), command: Some("sudo 密码".into()), ..step("secret") });
        log.record(&StepPayload {
            target_label: Some("h".into()),
            command: Some("sudo 密码".into()),
            secret_status: Some("provided".into()),
            output: Some(String::new()),
            ..step("secret")
        });
        let s = log.finish(1).unwrap();
        assert_eq!(s.actions.len(), 2, "keys 开头只记一次、进行中的 secret 不记: {:?}", s.actions);
        assert!(s.actions[1].contains("provided"));
        assert!(s.outcome.starts_with("没有给出结论"));
    }

    #[test]
    fn run_log_error_cancel_and_empty() {
        let mut log = RunLog::new("g");
        log.record(&StepPayload { command: Some("ls".into()), ..step("exec") });
        log.mark_error();
        assert!(log.finish(1).unwrap().outcome.contains("出错"));

        let mut log = RunLog::new("g");
        log.record(&StepPayload { command: Some("ls".into()), ..step("exec") });
        log.record(&StepPayload { text: Some("已取消。".into()), ..step("note") });
        assert!(log.finish(1).unwrap().outcome.contains("已取消"));

        assert!(RunLog::new("g").finish(1).is_none(), "什么都没发生不出摘要");
    }

    #[test]
    fn run_log_caps_actions_keeping_first_and_last() {
        let mut log = RunLog::new("g");
        for i in 0..40 {
            log.record(&StepPayload { command: Some(format!("cmd{i}")), ..step("exec") });
        }
        let s = log.finish(1).unwrap();
        assert!(s.actions.len() <= MAX_ACTIONS_KEPT + 1);
        assert!(s.actions[0].contains("cmd0"));
        assert!(s.actions.last().unwrap().contains("cmd39"));
        assert!(s.actions.iter().any(|a| a.contains("省略")));
    }

    #[test]
    fn summary_is_redacted_and_never_holds_secrets() {
        let mut log = RunLog::new("用 sk-ABCDEFGH12345678 登录");
        log.record(&StepPayload { command: Some("curl -H 'Authorization: Bearer sk-ABCDEFGH12345678'".into()), ..step("exec") });
        log.record(&StepPayload {
            output: Some("mail bob@example.com".into()),
            ..step("result")
        });
        let s = log.finish(1).unwrap();
        let all = serde_json::to_string(&s).unwrap();
        assert!(!all.contains("sk-ABCDEFGH12345678"));
        assert!(!all.contains("bob@example.com"));
    }

    #[test]
    fn run_logs_ignore_unknown_sessions_and_take_removes() {
        let logs = RunLogs::default();
        logs.record("nope", &step("exec")); // 不 panic
        logs.start("s", "goal");
        logs.record("s", &StepPayload { command: Some("ls".into()), ..step("exec") });
        assert!(logs.take("s").is_some());
        assert!(logs.take("s").is_none());
    }

    fn summ(ts: i64, goal: &str) -> RunSummary {
        RunSummary {
            ts,
            goal: goal.into(),
            actions: vec!["exec[a] ls".into()],
            outcome: "ok".into(),
            final_state: "在「a」".into(),
        }
    }

    #[test]
    fn render_prior_runs_basics() {
        assert_eq!(render_prior_runs(&[], 100), "");
        let s = render_prior_runs(&[summ(40, "g1")], 100);
        assert!(s.contains("目标：g1") && s.contains("1 分钟前") && s.contains("做了：exec[a] ls"));
        assert!(s.contains("可能已过期"));
    }

    #[test]
    fn render_prior_runs_keeps_only_last_three_and_caps_size() {
        let many: Vec<_> = (0..6).map(|i| summ(i, &format!("goal{i}"))).collect();
        let s = render_prior_runs(&many, 10);
        assert!(!s.contains("goal2") && s.contains("goal3") && s.contains("goal5"));

        let fat = RunSummary { outcome: "o".repeat(5000), ..summ(0, "fat") };
        let s = render_prior_runs(&[fat.clone(), fat.clone(), fat], 10);
        assert!(s.chars().count() <= PRIOR_BLOCK_MAX_CHARS + 1);
    }
}
