//! 录制器（Phase 3.2）：把用户在终端里做的一段操作，从统一时间线（2.5）转成 Skill 步骤的**草稿**。
//!
//! 用户对某个终端会话按「开始录制」，照常操作，再按「结束录制」；这一段时间里时间线上的
//! 用户事件被 [`build_draft`] 折成一份 [`RecordingDraft`]。**它还不是成品**：没有名字 / 描述 /
//! 参数，也没经过 LLM 整理（3.3）与人工编辑（3.4），只是「忠实但已去噪」的步骤序列。
//!
//! 纯逻辑（[`build_draft`]）与状态（[`AgentManager`] 上的 `recorder_*`）分开：前者不碰锁、不碰 Tauri，
//! 可直接单测；后者只是把「录制开始时的坐标」存起来，结束时据此切出时间线事件。
//! Tauri 命令在 `agent::recorder_cmd`。
//!
//! # 草稿 JSON（camelCase）
//!
//! ```json
//! {
//!   "session": "tab-1", "host": { "sessionId": "tab-1", "label": "prod-web" },
//!   "startedAtMs": 1700000000000, "endedAtMs": 1700000060000,
//!   "source": "recorded",
//!   "steps": [ { "kind": "exec", "command": "cd /srv/app", "onUnexpected": "ask" },
//!              { "kind": "exec", "command": "sudo systemctl restart web", "onUnexpected": "ask" },
//!              { "kind": "secret", "reason": "提示「[sudo] password for alice:」要求输入密码 / 口令", "onUnexpected": "ask" },
//!              { "kind": "expect_respond", "expect": "Continue? [y/N]", "respond": "y<Enter>", "secret": false, "onUnexpected": "ask" },
//!              { "kind": "keys", "keys": "<C-c>", "onUnexpected": "ask" } ],
//!   "hints": [ { "stepIndex": 0, "outputTail": "…命令输出的最后几行（已脱敏、限长）…" } ],
//!   "risk": "high",
//!   "warnings": ["第 2 步（exec）风险较高：…", "录制期间可能有未捕获的操作：…"],
//!   "notes": ["合并了 1 处重复 / 续行片段"],
//!   "stats": { "events": 12, "droppedNoise": 2, "merged": 1 }
//! }
//! ```
//!
//! `steps` 就是 [`SkillStep`]（与 3.1 的模型一字不差），3.3 / 3.4 直接拿去当 `SkillInput.steps`
//! （`source` 取 `recorded`）。`hints` 是**不进步骤**的旁注：每步命令输出的末尾几行，供 3.3 的
//! LLM 推断 `expectAfter`，也供人工编辑时参考；`warnings` 是需要用户在保存前看一眼的问题，
//! `notes` 只是说明做过哪些整理。
//!
//! # 转换规则
//!
//! - **用户命令** → `exec`。回车前屏幕停在 确认 / 分页器 / 菜单 / REPL 提示上（时间线里紧挨在这条
//!   命令之前的 `prompt` 事件）→ 改成 `expect_respond`（`expect` = 提示行，`respond` = 输入 + `<Enter>`）。
//! - **按键**（Ctrl-C / Ctrl-D / Ctrl-Z / 单独回车）→ `keys`；紧跟在提示后则同样成 `expect_respond`；
//!   连续的 `keys` 合并成一步；无提示的单独回车是噪声，丢弃。
//! - **密码 / 口令**（`secret` 事件，或提示被判为密码时用户敲的任何东西）→ **只有** `secret` 占位，
//!   `reason` 来自提示行文字；内容从不进入录制，连同一提示下重输的密码合并成一步。
//! - 噪声：空行、`clear` / `cls` / `reset` / `history`、整行注释；`\` 续行的多行命令合并成一行；
//!   连续重复的同一条命令合并。
//! - **绝不录密钥**：疑似写死口令的命令 / 应答（`skill::literal_secret_hint`）整条丢弃并告警，
//!   原文不保留；所有 hint 与提示文字再过一遍 [`redact::redact`]。
//! - Agent 自己的动作不是「用户操作」，一律忽略（并告警）。
//! - **有界**：步数 ≤ `skill::MAX_STEPS`，命令 / 应答不超过 skill 的字数上限，hint 只留末 3 行且限长。
//!
//! # 已知缺口（只能告警，不能凭空补出）
//!
//! 用户命令来自前端回车时推来的整行（2.4）。历史回溯 / Tab 补全的行由 `TerminalView` 从终端屏幕读回；
//! 但片段 / 快速发送 / 宏回放 / 全屏程序里的回车仍不经过这条路。录制开始时记下旁路缓冲输入段的坐标，
//! 结束时数一遍终端实际收到的回车，比「捕获到的提交」多就告警「录制期间可能有未捕获的操作」。

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::interact::parse_keys;
use super::prompt::classify;
use super::redact;
use super::skill::{self, OnUnexpected, Risk, SkillSource, SkillStep};
use super::timeline::{self, Kind, TimelineEvent, MAX_EVENTS};
use super::usercmd::{self, Actor};
use super::AgentManager;

// ——————————————————————————————— 上限 ———————————————————————————————

/// 单步 `expect` 最多取提示行末尾多少字符（提示行偏长时，末尾最稳定）。
pub const MAX_EXPECT_CHARS: usize = 80;
/// `secret` 步骤 reason 里引用的提示文字上限。
const MAX_REASON_PROMPT_CHARS: usize = 80;
/// hint：最多几行 / 每行字符数 / 总字节数（取尾部）。
pub const HINT_MAX_LINES: usize = 3;
pub const HINT_LINE_CHARS: usize = 160;
pub const HINT_MAX_BYTES: usize = 480;
/// 告警 / 说明条数上限（防止异常输入刷屏）。
const MAX_WARNINGS: usize = 30;
const MAX_NOTES: usize = 10;
/// 单条告警文字上限。
const MAX_MESSAGE_CHARS: usize = 300;
/// 主机标签上限。
pub const MAX_HOST_LABEL_CHARS: usize = 80;
/// 判「用户是在回答什么提示」时往回看多少字节的程序输出。
const PROMPT_PROBE_BYTES: usize = 512;

/// 允许录进时间线的用户按键（`nx_send_keys` 语法）。别的一律不收：
/// 这条通道只为「没有文字的操作」（中断 / 退出 / 空回车）存在，不是逐键记录。
pub const ALLOWED_KEYS: &[&str] = &["<C-c>", "<C-d>", "<C-z>", "<Enter>"];

// ——————————————————————————————— 草稿模型 ———————————————————————————————

/// 录制发生在哪个会话 / 主机。`label` 由前端按标签名传入（可能为空）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingHost {
    pub session_id: String,
    pub label: String,
}

/// 某一步的输出尾部（候选的 `expectAfter` 依据）。已脱敏、限长。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepHint {
    pub step_index: usize,
    pub output_tail: String,
}

/// 整理过程的统计，给界面 / 测试看。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftStats {
    /// 参与转换的时间线事件数
    pub events: usize,
    /// 被当作噪声丢掉的用户操作数
    pub dropped_noise: usize,
    /// 被合并掉的片段 / 重复数
    pub merged: usize,
}

/// 录制草稿。见模块文档。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingDraft {
    pub session: String,
    pub host: RecordingHost,
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
    /// 恒为 `recorded`，3.3 / 3.4 建 `SkillInput` 时直接用
    pub source: SkillSource,
    pub steps: Vec<SkillStep>,
    pub hints: Vec<StepHint>,
    /// 对整个步骤序列 `skill::assess_risk` 的结果
    pub risk: Risk,
    /// 保存前需要用户看一眼的问题
    pub warnings: Vec<String>,
    /// 做过哪些整理（合并 / 去噪），仅说明
    pub notes: Vec<String>,
    pub stats: DraftStats,
}

/// [`build_draft`] 的非事件入参。
#[derive(Debug, Clone, Default)]
pub struct RecordMeta {
    pub session: String,
    pub host_label: String,
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
    /// 录制开始时时间线的 `last_seq`：窗口内第一条事件应是 `start_seq + 1`，否则说明环形缓冲丢了最早的。
    pub start_seq: u64,
    /// 录制期间终端输入段里数到的回车数（旁路缓冲 `in_enters_since`）；`None` = 没有数据，不做对账。
    pub in_submits: Option<u32>,
}

// ——————————————————————————————— 转换 ———————————————————————————————

/// 紧挨在用户操作之前的提示（时间线 `prompt` 事件）。
struct PromptCtx {
    kind: String,
    text: String,
}

#[derive(Default)]
struct Builder {
    steps: Vec<SkillStep>,
    /// 最后一步是不是「以反斜杠续行、还在等下一行」的 exec
    cont_open: bool,
    /// 事件 seq → 它并入的步骤下标（输出摘要按 `ref_seq` 找回步骤）
    seq_step: HashMap<u64, usize>,
    /// 步骤下标 → 它最新的输出摘要
    out_text: HashMap<usize, String>,
    /// 被丢弃的命令事件 seq：它们的输出不算「无主输出」
    ignored_seqs: HashSet<u64>,
    prompt: Option<PromptCtx>,
    warnings: Vec<String>,
    submitted: u32,
    agent_events: usize,
    orphan_outputs: usize,
    dropped_noise: usize,
    merged: usize,
    leaked_dropped: usize,
    too_long_dropped: usize,
    unknown_prompt_secret: usize,
}

/// 把录制窗口内的时间线事件（按 `seq` 升序）转成草稿。纯函数。
pub fn build_draft(events: &[TimelineEvent], meta: &RecordMeta) -> RecordingDraft {
    let mut b = Builder::default();

    // 时间线是环形的：窗口里第一条不是 start_seq + 1，说明最早的被挤掉了
    if let Some(first) = events.first() {
        if first.seq > meta.start_seq + 1 {
            b.warn("录制太长：时间线只保留最近一段，最早的操作已丢失，草稿不完整");
        }
    }

    for ev in events {
        b.feed(ev);
    }

    // 步数上限
    let mut steps = std::mem::take(&mut b.steps);
    if steps.len() > skill::MAX_STEPS {
        steps.truncate(skill::MAX_STEPS);
        b.warn(format!("步骤超过 {} 步上限，后面的已被截掉", skill::MAX_STEPS));
    }

    // 汇总型告警
    if b.agent_events > 0 {
        b.warn(format!(
            "录制期间 Agent 也执行了 {} 个动作，它们不是用户操作，已忽略",
            b.agent_events
        ));
    }
    if b.leaked_dropped > 0 {
        b.warn(format!(
            "有 {} 条命令 / 应答疑似含明文口令，已从草稿丢弃（原文未保留）。请改用密码步骤，不要把口令写在命令里",
            b.leaked_dropped
        ));
    }
    if b.too_long_dropped > 0 {
        b.warn(format!("有 {} 条命令过长，已丢弃", b.too_long_dropped));
    }
    if b.unknown_prompt_secret > 0 {
        b.warn(format!(
            "有 {} 处在密码提示后输入了文字，已按密码处理（内容未记录）",
            b.unknown_prompt_secret
        ));
    }
    if b.orphan_outputs > 0 {
        b.warn(
            "录制期间可能有未捕获的操作：出现了没有对应命令的输出（可能来自录制前启动的命令，\
             或经由历史 / 片段 / 宏发出的命令）",
        );
    }
    // 对账：终端实际收到的回车 vs 捕获到的提交。Agent 自己的动作也会写输入段，有它在就没法对账
    if let (Some(got), 0) = (meta.in_submits, b.agent_events) {
        if got > b.submitted {
            b.warn(format!(
                "录制期间可能有未捕获的操作：终端收到 {got} 次回车提交，只捕获到 {} 次\
                 （片段 / 快速发送 / 宏回放 / 全屏程序里的回车不在录制范围内）",
                b.submitted
            ));
        }
    }

    // 风险：整体 + 逐步标出危险的
    let risk = skill::assess_risk(&steps).risk;
    for (i, step) in steps.iter().enumerate() {
        let r = skill::assess_risk(std::slice::from_ref(step));
        if r.risk == Risk::High {
            let why = r
                .reasons
                .first()
                .and_then(|s| s.split_once('：'))
                .map(|(_, w)| w.to_string())
                .unwrap_or_else(|| "含危险操作".into());
            b.warn(format!("第 {} 步（{}）风险较高：{why}", i + 1, step.kind()));
        }
    }
    if steps.iter().any(|s| matches!(s, SkillStep::Exec { command, .. } if is_exit_command(command))) {
        b.warn("包含 exit / logout：回放到这一步会结束当前会话");
    }
    if steps.is_empty() {
        b.warn("录制期间没有捕获到可用的操作");
    }

    // hints：只给 exec / expect_respond（有命令输出可言）；不超出步数
    let mut hints: Vec<StepHint> = Vec::new();
    let mut idxs: Vec<usize> = b.out_text.keys().copied().collect();
    idxs.sort_unstable();
    for i in idxs {
        let Some(step) = steps.get(i) else { continue };
        if !matches!(step, SkillStep::Exec { .. } | SkillStep::ExpectRespond { .. }) {
            continue;
        }
        let tail = hint_tail(&b.out_text[&i]);
        if !tail.is_empty() {
            hints.push(StepHint { step_index: i, output_tail: tail });
        }
    }

    let mut notes = Vec::new();
    if b.merged > 0 {
        notes.push(format!("合并了 {} 处重复 / 续行 / 连续按键片段", b.merged));
    }
    if b.dropped_noise > 0 {
        notes.push(format!("忽略了 {} 条噪声操作（空行 / clear / 注释等）", b.dropped_noise));
    }
    notes.truncate(MAX_NOTES);

    let mut warnings = b.warnings;
    warnings.truncate(MAX_WARNINGS);

    RecordingDraft {
        session: meta.session.clone(),
        host: RecordingHost {
            session_id: meta.session.clone(),
            label: clip_head(redact::redact(meta.host_label.trim()).as_str(), MAX_HOST_LABEL_CHARS),
        },
        started_at_ms: meta.started_at_ms,
        ended_at_ms: meta.ended_at_ms,
        source: SkillSource::Recorded,
        steps,
        hints,
        risk,
        warnings,
        notes,
        stats: DraftStats { events: events.len(), dropped_noise: b.dropped_noise, merged: b.merged },
    }
}

impl Builder {
    fn warn(&mut self, msg: impl Into<String>) {
        let msg = clip_head(&msg.into(), MAX_MESSAGE_CHARS);
        if self.warnings.len() < MAX_WARNINGS && !self.warnings.contains(&msg) {
            self.warnings.push(msg);
        }
    }

    fn feed(&mut self, ev: &TimelineEvent) {
        if ev.actor == Actor::Agent {
            // Agent 的动作与它们的输出都不属于用户操作
            self.agent_events += 1;
            return;
        }
        match ev.kind {
            Kind::Prompt => self.on_prompt(ev),
            Kind::Command => self.on_command(ev),
            Kind::Keys => self.on_keys(ev),
            Kind::Secret => self.on_secret(),
            Kind::Output => self.on_output(ev),
            // 用户不会产生 wait / screen / 文件工具事件；见了也不当操作
            Kind::Wait | Kind::Screen | Kind::File => {}
        }
    }

    // ——— 提示 / 输出 ———

    fn on_prompt(&mut self, ev: &TimelineEvent) {
        let kind = ev.prompt_kind.as_deref().unwrap_or("");
        if !matches!(kind, "password" | "yes_no" | "pager" | "menu" | "repl") {
            return;
        }
        let text = redact::redact(ev.text.trim());
        self.prompt = Some(PromptCtx { kind: kind.to_string(), text });
    }

    fn on_output(&mut self, ev: &TimelineEvent) {
        match ev.ref_seq {
            Some(r) if self.seq_step.contains_key(&r) => {
                let i = self.seq_step[&r];
                self.out_text.insert(i, ev.text.clone());
            }
            Some(r) if self.ignored_seqs.contains(&r) => {}
            // 没有对应命令的输出：多半是录制前就在跑的命令，或未被捕获的操作
            _ => self.orphan_outputs += 1,
        }
    }

    // ——— 步骤落账 ———

    fn push_step(&mut self, step: SkillStep, seq: u64) -> usize {
        self.steps.push(step);
        let i = self.steps.len() - 1;
        self.seq_step.insert(seq, i);
        self.cont_open = false;
        i
    }

    fn secret_step(&mut self, seq: Option<u64>, ctx: Option<PromptCtx>) {
        let reason = secret_reason(ctx.as_ref());
        // 同一提示下输错重输：合并成一步
        if let Some(SkillStep::Secret { reason: last, .. }) = self.steps.last() {
            if *last == reason {
                self.merged += 1;
                self.cont_open = false;
                return;
            }
        }
        let step = SkillStep::Secret {
            reason,
            param: None,
            timeout_ms: None,
            expect_after: None,
            on_unexpected: OnUnexpected::Ask,
        };
        match seq {
            Some(s) => {
                self.push_step(step, s);
            }
            None => {
                self.steps.push(step);
                self.cont_open = false;
            }
        }
    }

    fn respond_step(&mut self, seq: u64, ctx: PromptCtx, respond: String) {
        let Some(expect) = expect_from_prompt(&ctx.text, &respond_answer_hint(&respond)) else {
            // 提示行取不出可用的 expect：退化成不带 expect 的步骤由调用方处理
            return;
        };
        self.push_step(
            SkillStep::ExpectRespond {
                expect,
                respond,
                secret: false,
                timeout_ms: None,
                expect_after: None,
                on_unexpected: OnUnexpected::Ask,
            },
            seq,
        );
    }

    // ——— 事件 ———

    fn on_secret(&mut self) {
        self.submitted += 1;
        let ctx = self.prompt.take();
        self.secret_step(None, ctx);
    }

    fn on_command(&mut self, ev: &TimelineEvent) {
        self.submitted += 1;
        let ctx = self.prompt.take();
        let raw = ev.text.trim();
        if raw.is_empty() {
            self.dropped_noise += 1;
            self.ignored_seqs.insert(ev.seq);
            return;
        }
        if ev.truncated || raw.chars().count() > skill::MAX_COMMAND_CHARS {
            self.too_long_dropped += 1;
            self.ignored_seqs.insert(ev.seq);
            return;
        }
        // 密码提示后敲的任何东西都按密码处理，绝不当命令存
        if let Some(c) = &ctx {
            if c.kind == "password" {
                self.unknown_prompt_secret += 1;
                self.ignored_seqs.insert(ev.seq);
                self.secret_step(None, ctx);
                return;
            }
        }
        // 写死口令的命令 / 应答：整条丢弃，原文不进草稿
        if skill::literal_secret_hint(raw).is_some() {
            self.leaked_dropped += 1;
            self.ignored_seqs.insert(ev.seq);
            return;
        }
        // 回答提示
        if let Some(c) = ctx {
            let respond = format!("{}<Enter>", escape_respond(raw));
            if respond.chars().count() > skill::MAX_KEYS_CHARS {
                self.too_long_dropped += 1;
                self.ignored_seqs.insert(ev.seq);
                return;
            }
            let before = self.steps.len();
            self.respond_step(ev.seq, c, respond);
            if self.steps.len() > before {
                return;
            }
            // 取不出 expect：当普通命令处理（落到下面）
        }
        self.exec(ev.seq, raw);
    }

    fn exec(&mut self, seq: u64, raw: &str) {
        if is_noise_command(raw) {
            self.dropped_noise += 1;
            self.ignored_seqs.insert(seq);
            return;
        }
        let command = escape_command(raw);

        // 续行：上一条以 `\` 结尾，这条是它的下文
        if self.cont_open {
            if let Some(SkillStep::Exec { command: prev, .. }) = self.steps.last_mut() {
                let head = prev.trim_end_matches('\\').trim_end().to_string();
                let joined = format!("{head} {command}");
                if joined.chars().count() > skill::MAX_COMMAND_CHARS {
                    self.too_long_dropped += 1;
                    self.ignored_seqs.insert(seq);
                    return;
                }
                *prev = joined;
                self.cont_open = ends_with_continuation(prev);
                let i = self.steps.len() - 1;
                self.seq_step.insert(seq, i);
                self.merged += 1;
                return;
            }
        }
        // 连续重复的同一条命令（重试 / 手滑回车两次）
        if let Some(SkillStep::Exec { command: prev, .. }) = self.steps.last() {
            if *prev == command {
                let i = self.steps.len() - 1;
                self.seq_step.insert(seq, i);
                self.merged += 1;
                return;
            }
        }
        let open = ends_with_continuation(&command);
        self.push_step(
            SkillStep::Exec {
                command,
                timeout_ms: None,
                expect_after: None,
                on_unexpected: OnUnexpected::Ask,
            },
            seq,
        );
        self.cont_open = open;
    }

    fn on_keys(&mut self, ev: &TimelineEvent) {
        let keys = ev.text.trim();
        let valid = !keys.is_empty()
            && keys.chars().count() <= skill::MAX_KEYS_CHARS
            && parse_keys(keys).is_ok()
            && skill::literal_secret_hint(keys).is_none();
        let is_enter = keys.eq_ignore_ascii_case("<Enter>");
        if is_enter {
            self.submitted += 1;
        }
        let ctx = self.prompt.take();
        if !valid {
            self.dropped_noise += 1;
            return;
        }
        if let Some(c) = ctx {
            // 空口令直接回车：仍然是密码步骤
            if c.kind == "password" && is_enter {
                self.unknown_prompt_secret += 1;
                self.secret_step(None, Some(c));
                return;
            }
            // 在密码提示上按 Ctrl-C 等：是普通按键，往下走
            if c.kind != "password" {
                let before = self.steps.len();
                self.respond_step(ev.seq, c, keys.to_string());
                if self.steps.len() > before {
                    return;
                }
            }
        }
        if is_enter {
            // 没有提示背景的单独回车：多半是敲了个空行
            self.dropped_noise += 1;
            return;
        }
        // 连续按键合并
        if let Some(SkillStep::Keys { keys: prev, .. }) = self.steps.last_mut() {
            if prev.chars().count() + keys.chars().count() <= skill::MAX_KEYS_CHARS {
                prev.push_str(keys);
                self.merged += 1;
                self.cont_open = false;
                return;
            }
        }
        if keys.eq_ignore_ascii_case("<C-c>") {
            if let Some(SkillStep::Exec { command, .. }) = self.steps.last() {
                let msg = format!(
                    "命令 `{}` 之后紧跟 Ctrl-C：它可能是长驻命令（回放时 exec 会一直等它结束），请考虑设置超时或改写",
                    clip_head(command, 60)
                );
                self.warn(msg);
            }
        }
        self.push_step(
            SkillStep::Keys {
                keys: keys.to_string(),
                timeout_ms: None,
                expect_after: None,
                on_unexpected: OnUnexpected::Ask,
            },
            ev.seq,
        );
    }
}

// ——————————————————————————————— 文本辅助 ———————————————————————————————

/// 只会被当噪声丢掉的命令：清屏 / 看历史 / 整行注释。
fn is_noise_command(cmd: &str) -> bool {
    let t = cmd.trim();
    if t.starts_with('#') {
        return true;
    }
    matches!(t, "clear" | "cls" | "reset" | "history" | "tput clear" | "tput reset")
}

fn is_exit_command(cmd: &str) -> bool {
    matches!(cmd.trim(), "exit" | "logout" | "quit")
}

/// 命令以奇数个反斜杠结尾 = 续行。
fn ends_with_continuation(cmd: &str) -> bool {
    let t = cmd.trim_end();
    let n = t.chars().rev().take_while(|c| *c == '\\').count();
    n % 2 == 1
}

/// 文本里若有会被 skill 当成占位符的 `{{name}}`，给每个 `{{` 加 `\` 转义成字面量；
/// 没有占位符（含 `docker -f '{{.Name}}'` 这类 Go 模板）就原样保留。
fn escape_command(s: &str) -> String {
    if skill::placeholder_names(s).is_empty() {
        s.to_string()
    } else {
        s.replace("{{", "\\{{")
    }
}

/// `respond` 里的文字：按键语法转义 `\` 与 `<`，再处理占位符。
fn escape_respond(s: &str) -> String {
    escape_command(&s.replace('\\', "\\\\").replace('<', "<lt>"))
}

/// 从 `respond`（如 `y<Enter>`）里取出用户敲的那部分文字，用来从提示行末尾剥掉回显。
fn respond_answer_hint(respond: &str) -> String {
    respond
        .strip_suffix("<Enter>")
        .unwrap_or(respond)
        .replace("<lt>", "<")
        .replace("\\\\", "\\")
}

/// 提示行 → `expect`：剥掉末尾被回显的应答、取末尾 [`MAX_EXPECT_CHARS`] 字符、
/// 避开 `/…/` 正则语法、转义占位符。取不出来（空）返回 `None`。
fn expect_from_prompt(prompt: &str, answer: &str) -> Option<String> {
    let mut text = redact::redact(prompt.trim());
    if !answer.is_empty() {
        if let Some(rest) = text.strip_suffix(answer) {
            if !rest.trim().is_empty() {
                text = rest.trim_end().to_string();
            }
        }
    }
    let text = clip_tail(text.trim(), MAX_EXPECT_CHARS);
    // 以 `/` 开头且含第二个 `/` 会被当正则；提示行本来就是字面文字，去掉开头的斜杠即可
    let text = text.trim_start_matches('/').trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some(escape_command(&text))
}

/// secret 步骤的 `reason`：由提示行文字派生，没有提示就用通用说法。**永远不含任何输入内容。**
fn secret_reason(ctx: Option<&PromptCtx>) -> String {
    match ctx.map(|c| c.text.trim()).filter(|t| !t.is_empty()) {
        Some(t) => format!("提示「{}」要求输入密码 / 口令", clip_tail(&redact::redact(t), MAX_REASON_PROMPT_CHARS)),
        None => "录制时输入了一次密码 / 口令".to_string(),
    }
}

/// 输出摘要 → hint：脱敏、去空行、末 [`HINT_MAX_LINES`] 行、每行限长、总量限长。
pub fn hint_tail(text: &str) -> String {
    let redacted = redact::redact(text);
    let lines: Vec<String> = redacted
        .lines()
        .map(|l| l.trim().chars().filter(|c| !c.is_control()).collect::<String>())
        .filter(|l| !l.is_empty())
        .collect();
    let skip = lines.len().saturating_sub(HINT_MAX_LINES);
    let lines: Vec<String> = lines
        .into_iter()
        .skip(skip)
        .map(|l| {
            if l.chars().count() > HINT_LINE_CHARS {
                clip_head(&l, HINT_LINE_CHARS) + "…"
            } else {
                l
            }
        })
        .collect();
    let mut out = lines.join("\n");
    if out.len() > HINT_MAX_BYTES {
        let mut start = out.len() - HINT_MAX_BYTES;
        while !out.is_char_boundary(start) {
            start += 1;
        }
        out = out[start..].to_string();
    }
    out
}

fn clip_head(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

fn clip_tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        s.to_string()
    } else {
        s.chars().skip(n - max).collect()
    }
}

// ——————————————————————————————— 录制状态 ———————————————————————————————

/// 一个会话正在进行的录制。
#[derive(Debug, Clone)]
pub struct RecState {
    pub started_at_ms: u64,
    /// 开始时时间线的 `last_seq`
    pub start_seq: u64,
    /// 开始时旁路缓冲的 `pushed` 坐标（输入段对账用）
    pub start_pushed: u64,
    pub host_label: String,
}

/// 会话 → 录制状态（`AgentManager` 的字段类型）。
pub type Recordings = HashMap<String, RecState>;

/// 推给前端的录制状态（`agent_record_status` 返回、`nx://agent/recorder/{session}` 事件载荷）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecorderStatus {
    pub session: String,
    pub recording: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    pub host_label: String,
}

impl RecorderStatus {
    fn idle(session: &str) -> Self {
        Self { session: session.to_string(), recording: false, started_at_ms: None, host_label: String::new() }
    }
}

impl AgentManager {
    pub fn recorder_status(&self, session_id: &str) -> RecorderStatus {
        match self.recordings.lock().get(session_id) {
            Some(r) => RecorderStatus {
                session: session_id.to_string(),
                recording: true,
                started_at_ms: Some(r.started_at_ms),
                host_label: r.host_label.clone(),
            },
            None => RecorderStatus::idle(session_id),
        }
    }

    /// 这个会话是否正在录制。写时间线的热路径先问它，没录制就什么都不多做。
    pub fn recorder_active(&self, session_id: &str) -> bool {
        self.recordings.lock().contains_key(session_id)
    }

    /// 开始录制。返回新状态与顺带落账的事件（把上一条命令尚未落账的输出先冲掉，
    /// 免得它被算进这一段录制里变成「无主输出」）。
    pub fn recorder_start(
        &self,
        session_id: &str,
        host_label: &str,
    ) -> Result<(RecorderStatus, Vec<TimelineEvent>), String> {
        if !self.enabled.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("AI 助手总开关未启用：时间线不会记录，无法录制".into());
        }
        if self.recorder_active(session_id) {
            return Err("这个终端已经在录制中".into());
        }
        let evs = self.timeline_flush(session_id, None);
        let start_seq = self.timelines.lock().get(session_id).map(|t| t.last_seq()).unwrap_or(0);
        let start_pushed = self.pushed(session_id).unwrap_or(0);
        let label = clip_head(host_label.trim(), MAX_HOST_LABEL_CHARS);
        self.recordings.lock().insert(
            session_id.to_string(),
            RecState {
                started_at_ms: usercmd::now_ms(),
                start_seq,
                start_pushed,
                host_label: label,
            },
        );
        Ok((self.recorder_status(session_id), evs))
    }

    /// 结束录制，切出这段时间线事件转成草稿。没在录制返回错误。
    /// 返回草稿与顺带落账的事件（最后一条命令的输出摘要）。
    pub fn recorder_stop(&self, session_id: &str) -> Result<(RecordingDraft, Vec<TimelineEvent>), String> {
        let state = self
            .recordings
            .lock()
            .remove(session_id)
            .ok_or_else(|| "这个终端没有在录制".to_string())?;
        let flushed = self.timeline_flush(session_id, None);
        let events = self.timeline_since(session_id, state.start_seq, MAX_EVENTS);
        let in_submits = self.sessions.lock().get(session_id).map(|b| b.in_enters_since(state.start_pushed));
        let meta = RecordMeta {
            session: session_id.to_string(),
            host_label: state.host_label,
            started_at_ms: state.started_at_ms,
            ended_at_ms: usercmd::now_ms(),
            start_seq: state.start_seq,
            in_submits,
        };
        Ok((build_draft(&events, &meta), flushed))
    }

    /// 取消录制，丢弃一切。返回之前是否在录制。
    pub fn recorder_cancel(&self, session_id: &str) -> bool {
        self.recordings.lock().remove(session_id).is_some()
    }

    /// 录制期间、用户提交一行 / 按键**之前**调用：把待落账的上一条输出冲掉，
    /// 再看屏幕尾部是不是停在 确认 / 分页器 / 菜单 / REPL / 密码 提示上，是就记一条用户侧的 `prompt` 事件，
    /// 让 [`build_draft`] 知道这次输入是在「回答什么」。没在录制时什么都不做、不分类。
    pub fn recorder_user_context(&self, session_id: &str) -> Vec<TimelineEvent> {
        if !self.recorder_active(session_id) {
            return Vec::new();
        }
        let mut out = self.timeline_flush(session_id, None);
        // 与时间线锁不嵌套：先取尾部再放锁
        let tail = self.sessions.lock().get(session_id).map(|b| b.out_tail(PROMPT_PROBE_BYTES));
        let Some(tail) = tail else { return out };
        let guess = classify(&tail, false);
        if !timeline::is_notable(&guess, false) {
            return out;
        }
        let line = tail
            .lines()
            .rev()
            .map(str::trim_end)
            .find(|l| !l.trim().is_empty())
            .unwrap_or_default()
            .to_string();
        let ev = self
            .timelines
            .lock()
            .entry(session_id.to_string())
            .or_default()
            .push_prompt(Actor::User, &guess, &line, usercmd::now_ms());
        out.extend(ev);
        out
    }

    /// 录制期间记一次用户按键（只收 [`ALLOWED_KEYS`]）。没在录制 / 不在白名单返回空。
    pub fn recorder_user_keys(&self, session_id: &str, keys: &str) -> Vec<TimelineEvent> {
        let keys = keys.trim();
        if !ALLOWED_KEYS.iter().any(|k| k.eq_ignore_ascii_case(keys)) {
            return Vec::new();
        }
        let mut out = self.recorder_user_context(session_id);
        if !self.recorder_active(session_id) {
            return out;
        }
        let ev = self
            .timelines
            .lock()
            .entry(session_id.to_string())
            .or_default()
            .push_user_keys(keys, usercmd::now_ms());
        out.push(ev);
        out
    }
}

/// 推给前端的录制状态事件名。载荷是 [`RecorderStatus`]。
pub fn event_name(session_id: &str) -> String {
    format!("nx://agent/recorder/{session_id}")
}

// ——————————————————————————————— 测试 ———————————————————————————————

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::buffer::StreamKind;
    use crate::agent::usercmd::UserCommandLog;
    use crate::agent::AgentConfig;

    fn ev(seq: u64, actor: Actor, kind: Kind, text: &str) -> TimelineEvent {
        TimelineEvent {
            seq,
            ts_ms: 1_000 + seq,
            actor,
            kind,
            text: text.to_string(),
            note: None,
            ref_seq: None,
            prompt_kind: None,
            confidence: None,
            truncated: false,
        }
    }
    fn cmd(seq: u64, t: &str) -> TimelineEvent {
        ev(seq, Actor::User, Kind::Command, t)
    }
    fn keys(seq: u64, t: &str) -> TimelineEvent {
        ev(seq, Actor::User, Kind::Keys, t)
    }
    fn secret(seq: u64) -> TimelineEvent {
        ev(seq, Actor::User, Kind::Secret, "")
    }
    fn prompt(seq: u64, kind: &str, t: &str) -> TimelineEvent {
        let mut e = ev(seq, Actor::User, Kind::Prompt, t);
        e.prompt_kind = Some(kind.to_string());
        e.confidence = Some(0.9);
        e
    }
    fn out(seq: u64, r: u64, t: &str) -> TimelineEvent {
        let mut e = ev(seq, Actor::User, Kind::Output, t);
        e.ref_seq = Some(r);
        e
    }
    fn meta() -> RecordMeta {
        RecordMeta { session: "s1".into(), host_label: "prod".into(), start_seq: 0, ..Default::default() }
    }
    fn draft(events: &[TimelineEvent]) -> RecordingDraft {
        build_draft(events, &meta())
    }
    fn exec(c: &str) -> SkillStep {
        SkillStep::Exec { command: c.into(), timeout_ms: None, expect_after: None, on_unexpected: OnUnexpected::Ask }
    }

    #[test]
    fn commands_become_exec_steps_in_order() {
        let d = draft(&[cmd(1, "cd /srv/app"), cmd(2, "git pull"), cmd(3, "ls -la")]);
        assert_eq!(d.steps, vec![exec("cd /srv/app"), exec("git pull"), exec("ls -la")]);
        assert_eq!(d.source, SkillSource::Recorded);
        assert_eq!(d.host.label, "prod");
        assert_eq!(d.session, "s1");
    }

    #[test]
    fn noise_is_dropped() {
        let d = draft(&[cmd(1, "   "), cmd(2, "clear"), cmd(3, "# note"), cmd(4, "history"), cmd(5, "ls")]);
        assert_eq!(d.steps, vec![exec("ls")]);
        assert_eq!(d.stats.dropped_noise, 4);
    }

    #[test]
    fn backslash_continuation_is_joined() {
        let d = draft(&[cmd(1, "docker run \\"), cmd(2, "  -d \\"), cmd(3, "nginx")]);
        assert_eq!(d.steps, vec![exec("docker run -d nginx")]);
        assert_eq!(d.stats.merged, 2);
    }

    #[test]
    fn double_backslash_is_not_continuation() {
        let d = draft(&[cmd(1, "echo a\\\\"), cmd(2, "ls")]);
        assert_eq!(d.steps.len(), 2);
    }

    #[test]
    fn consecutive_duplicates_merge_but_separated_ones_stay() {
        let d = draft(&[cmd(1, "make"), cmd(2, "make"), cmd(3, "ls"), cmd(4, "make")]);
        assert_eq!(d.steps, vec![exec("make"), exec("ls"), exec("make")]);
        assert_eq!(d.stats.merged, 1);
    }

    #[test]
    fn answer_after_prompt_becomes_expect_respond() {
        let d = draft(&[
            cmd(1, "apt upgrade"),
            prompt(2, "yes_no", "Do you want to continue? [Y/n]"),
            cmd(3, "y"),
        ]);
        assert_eq!(d.steps.len(), 2);
        assert_eq!(
            d.steps[1],
            SkillStep::ExpectRespond {
                expect: "Do you want to continue? [Y/n]".into(),
                respond: "y<Enter>".into(),
                secret: false,
                timeout_ms: None,
                expect_after: None,
                on_unexpected: OnUnexpected::Ask,
            }
        );
    }

    #[test]
    fn echoed_answer_is_stripped_from_expect() {
        let d = draft(&[prompt(1, "yes_no", "Continue? [y/N] y"), cmd(2, "y")]);
        match &d.steps[0] {
            SkillStep::ExpectRespond { expect, .. } => assert_eq!(expect, "Continue? [y/N]"),
            s => panic!("{s:?}"),
        }
    }

    #[test]
    fn menu_choice_and_repl_input() {
        let d = draft(&[
            prompt(1, "menu", "Select an option [1-3]:"),
            cmd(2, "2"),
            prompt(3, "repl", "mysql>"),
            cmd(4, "show databases;"),
        ]);
        assert_eq!(d.steps.len(), 2);
        assert!(matches!(&d.steps[0], SkillStep::ExpectRespond { respond, .. } if respond == "2<Enter>"));
        assert!(matches!(&d.steps[1], SkillStep::ExpectRespond { expect, respond, .. }
            if expect == "mysql>" && respond == "show databases;<Enter>"));
    }

    #[test]
    fn prompt_context_is_consumed_by_one_action_only() {
        let d = draft(&[prompt(1, "yes_no", "ok? [y/n]"), cmd(2, "y"), cmd(3, "ls")]);
        assert!(matches!(d.steps[0], SkillStep::ExpectRespond { .. }));
        assert_eq!(d.steps[1], exec("ls"));
    }

    #[test]
    fn respond_escapes_key_syntax() {
        let d = draft(&[prompt(1, "repl", ">>>"), cmd(2, "print('<C-c>')")]);
        match &d.steps[0] {
            SkillStep::ExpectRespond { respond, .. } => {
                assert_eq!(respond, "print('<lt>C-c>')<Enter>");
                // 解码后必须正好是用户敲的那行 + 回车，不能夹带 Ctrl-C
                assert_eq!(parse_keys(respond).unwrap(), b"print('<C-c>')\r");
            }
            s => panic!("{s:?}"),
        }
    }

    #[test]
    fn expect_never_parses_as_regex_and_placeholders_are_escaped() {
        let d = draft(&[prompt(1, "menu", "/dev/sda1 [y/n]"), cmd(2, "y")]);
        match &d.steps[0] {
            SkillStep::ExpectRespond { expect, .. } => assert!(!expect.starts_with('/')),
            s => panic!("{s:?}"),
        }
        let d = draft(&[prompt(1, "yes_no", "use {{name}}? [y/n]"), cmd(2, "y")]);
        match &d.steps[0] {
            SkillStep::ExpectRespond { expect, .. } => assert!(skill::placeholder_names(expect).is_empty()),
            s => panic!("{s:?}"),
        }
    }

    #[test]
    fn long_prompt_keeps_the_tail() {
        let long = format!("{} Proceed? [y/N]", "x".repeat(300));
        let d = draft(&[prompt(1, "yes_no", &long), cmd(2, "y")]);
        match &d.steps[0] {
            SkillStep::ExpectRespond { expect, .. } => {
                assert_eq!(expect.chars().count(), MAX_EXPECT_CHARS);
                assert!(expect.ends_with("Proceed? [y/N]"));
            }
            s => panic!("{s:?}"),
        }
    }

    #[test]
    fn keys_become_keys_steps_and_merge() {
        let d = draft(&[cmd(1, "ping 1.1.1.1"), keys(2, "<C-c>"), keys(3, "<C-c>"), cmd(4, "ls")]);
        assert_eq!(d.steps.len(), 3);
        assert!(matches!(&d.steps[1], SkillStep::Keys { keys, .. } if keys == "<C-c><C-c>"));
        // 紧跟 exec 的 Ctrl-C 会提示长驻命令
        assert!(d.warnings.iter().any(|w| w.contains("Ctrl-C")), "{:?}", d.warnings);
    }

    #[test]
    fn keys_after_prompt_become_expect_respond() {
        let d = draft(&[prompt(1, "pager", "--More--"), keys(2, "<Enter>")]);
        assert!(matches!(&d.steps[0], SkillStep::ExpectRespond { expect, respond, .. }
            if expect == "--More--" && respond == "<Enter>"));
    }

    #[test]
    fn bare_enter_without_prompt_is_noise() {
        let d = draft(&[keys(1, "<Enter>"), keys(2, "<Enter>"), cmd(3, "ls")]);
        assert_eq!(d.steps, vec![exec("ls")]);
    }

    #[test]
    fn invalid_or_secretish_keys_are_dropped() {
        let d = draft(&[keys(1, "<Bogus>"), keys(2, "sshpass -p hunter2"), keys(3, "")]);
        assert!(d.steps.is_empty());
    }

    #[test]
    fn secret_event_gives_placeholder_with_reason_from_prompt() {
        let d = draft(&[
            cmd(1, "sudo ls"),
            prompt(2, "password", "[sudo] password for alice:"),
            secret(3),
            cmd(4, "id"),
        ]);
        assert_eq!(d.steps.len(), 3);
        match &d.steps[1] {
            SkillStep::Secret { reason, param, .. } => {
                assert!(reason.contains("[sudo] password for alice:"), "{reason}");
                assert!(param.is_none());
            }
            s => panic!("{s:?}"),
        }
    }

    #[test]
    fn secret_without_prompt_has_generic_reason() {
        let d = draft(&[secret(1)]);
        assert!(matches!(&d.steps[0], SkillStep::Secret { reason, .. } if reason.contains("密码")));
    }

    #[test]
    fn repeated_secret_with_same_prompt_merges_as_retry() {
        let d = draft(&[
            prompt(1, "password", "Password:"),
            secret(2),
            prompt(3, "password", "Password:"),
            secret(4),
        ]);
        assert_eq!(d.steps.len(), 1);
        assert_eq!(d.stats.merged, 1);
    }

    #[test]
    fn text_typed_at_a_password_prompt_is_never_recorded() {
        // 前端本该标成 secret 事件；万一漏标成了命令，内容也绝不能进草稿
        let d = draft(&[
            prompt(1, "password", "Password:"),
            cmd(2, "hunter2-super-secret"),
            out(3, 2, "Welcome"),
        ]);
        let json = serde_json::to_string(&d).unwrap();
        assert!(!json.contains("hunter2"), "{json}");
        assert!(matches!(&d.steps[0], SkillStep::Secret { .. }));
        assert!(d.warnings.iter().any(|w| w.contains("密码")));
        // 该条的输出也不算无主输出
        assert!(!d.warnings.iter().any(|w| w.contains("没有对应命令")));
    }

    #[test]
    fn empty_password_enter_is_a_secret_step() {
        let d = draft(&[prompt(1, "password", "Enter passphrase:"), keys(2, "<Enter>")]);
        assert!(matches!(&d.steps[0], SkillStep::Secret { .. }));
    }

    #[test]
    fn ctrl_c_at_password_prompt_stays_a_plain_key() {
        let d = draft(&[prompt(1, "password", "Password:"), keys(2, "<C-c>")]);
        assert!(matches!(&d.steps[0], SkillStep::Keys { keys, .. } if keys == "<C-c>"));
    }

    #[test]
    fn hardcoded_secrets_are_dropped_not_kept() {
        let d = draft(&[
            cmd(1, "mysql -u root -pS3cretPw"),
            cmd(2, "curl -H 'Authorization: Bearer abcdefghijklmnop' http://x"),
            cmd(3, "export API_KEY=1 password=hunter2"),
            cmd(4, "ls"),
        ]);
        assert_eq!(d.steps, vec![exec("ls")]);
        let json = serde_json::to_string(&d).unwrap();
        assert!(!json.contains("S3cretPw") && !json.contains("abcdefghijklmnop") && !json.contains("hunter2"));
        assert!(d.warnings.iter().any(|w| w.contains("明文口令")));
    }

    #[test]
    fn secret_in_repl_answer_is_dropped() {
        let d = draft(&[prompt(1, "repl", "mysql>"), cmd(2, "SET password=hunter2;")]);
        assert!(d.steps.is_empty());
    }

    #[test]
    fn dangerous_commands_are_flagged_by_step() {
        let d = draft(&[cmd(1, "ls"), cmd(2, "rm -rf /var/data"), cmd(3, "pwd")]);
        assert_eq!(d.risk, Risk::High);
        assert!(d.warnings.iter().any(|w| w.contains("第 2 步") && w.contains("风险较高")), "{:?}", d.warnings);
        assert!(!d.warnings.iter().any(|w| w.contains("第 1 步") || w.contains("第 3 步")));
    }

    #[test]
    fn readonly_recording_is_low_risk() {
        let d = draft(&[cmd(1, "ls"), cmd(2, "pwd")]);
        assert_eq!(d.risk, Risk::Low);
    }

    #[test]
    fn exit_is_warned() {
        let d = draft(&[cmd(1, "ls"), cmd(2, "exit")]);
        assert!(d.warnings.iter().any(|w| w.contains("exit")));
    }

    #[test]
    fn placeholder_lookalikes_in_commands_are_escaped() {
        let d = draft(&[cmd(1, "echo {{name}}"), cmd(2, "docker inspect -f '{{.Name}}' web")]);
        assert_eq!(d.steps[0], exec("echo \\{{name}}"));
        // Go 模板不是占位符，原样保留
        assert_eq!(d.steps[1], exec("docker inspect -f '{{.Name}}' web"));
    }

    #[test]
    fn outputs_become_trimmed_redacted_hints() {
        let text = "l1\nl2\nl3\nl4 token=abcdef123456\nl5";
        let d = draft(&[cmd(1, "build"), out(2, 1, text), cmd(3, "ls")]);
        assert_eq!(d.hints.len(), 1);
        let h = &d.hints[0];
        assert_eq!(h.step_index, 0);
        assert_eq!(h.output_tail.lines().count(), HINT_MAX_LINES);
        assert!(h.output_tail.ends_with("l5"));
        assert!(!h.output_tail.contains("abcdef123456"), "{}", h.output_tail);
        assert!(h.output_tail.len() <= HINT_MAX_BYTES);
    }

    #[test]
    fn hint_lines_and_bytes_are_capped() {
        let long = "x".repeat(1000);
        let t = hint_tail(&format!("{long}\n{long}\n{long}"));
        assert!(t.len() <= HINT_MAX_BYTES);
        for l in t.lines() {
            assert!(l.chars().count() <= HINT_LINE_CHARS + 1 || t.len() <= HINT_MAX_BYTES);
        }
        // 中文不会在字节边界被切坏
        let zh = "字".repeat(400);
        let t = hint_tail(&format!("{zh}\n{zh}\n{zh}"));
        assert!(t.len() <= HINT_MAX_BYTES);
    }

    #[test]
    fn hints_skip_secret_and_keys_steps() {
        let d = draft(&[secret(1), out(2, 1, "Last login: x"), keys(3, "<C-c>")]);
        assert!(d.hints.is_empty());
    }

    #[test]
    fn duplicate_command_keeps_latest_output_as_hint() {
        let d = draft(&[cmd(1, "make"), out(2, 1, "fail"), cmd(3, "make"), out(4, 3, "done")]);
        assert_eq!(d.steps.len(), 1);
        assert_eq!(d.hints[0].output_tail, "done");
    }

    #[test]
    fn agent_events_are_ignored_with_warning() {
        let agent_cmd = ev(2, Actor::Agent, Kind::Command, "whoami");
        let mut agent_out = ev(3, Actor::Agent, Kind::Output, "root");
        agent_out.ref_seq = Some(2);
        let d = draft(&[cmd(1, "ls"), agent_cmd, agent_out]);
        assert_eq!(d.steps, vec![exec("ls")]);
        assert!(d.warnings.iter().any(|w| w.contains("Agent")));
    }

    #[test]
    fn orphan_output_warns_about_uncaptured_operations() {
        let d = draft(&[cmd(1, "ls"), out(2, 99, "something")]);
        assert!(d.warnings.iter().any(|w| w.contains("可能有未捕获的操作")), "{:?}", d.warnings);
        // 有对应命令的输出不告警
        let d = draft(&[cmd(1, "ls"), out(2, 1, "a")]);
        assert!(!d.warnings.iter().any(|w| w.contains("未捕获")));
    }

    #[test]
    fn enter_count_reconciliation() {
        let mut m = meta();
        m.in_submits = Some(3);
        let d = build_draft(&[cmd(1, "ls"), cmd(2, "pwd")], &m);
        assert!(d.warnings.iter().any(|w| w.contains("3 次回车") && w.contains("2 次")), "{:?}", d.warnings);
        // 数目对得上：不告警（空回车也算捕获）
        m.in_submits = Some(3);
        let d = build_draft(&[cmd(1, "ls"), keys(2, "<Enter>"), cmd(3, "pwd")], &m);
        assert!(!d.warnings.iter().any(|w| w.contains("未捕获")), "{:?}", d.warnings);
        // 没有数据：不对账
        m.in_submits = None;
        let d = build_draft(&[cmd(1, "ls")], &m);
        assert!(!d.warnings.iter().any(|w| w.contains("未捕获")));
        // 密码提交也算一次
        m.in_submits = Some(1);
        let d = build_draft(&[secret(1)], &m);
        assert!(!d.warnings.iter().any(|w| w.contains("未捕获")));
    }

    #[test]
    fn reconciliation_is_skipped_when_agent_acted() {
        let mut m = meta();
        m.in_submits = Some(5);
        let d = build_draft(&[cmd(1, "ls"), ev(2, Actor::Agent, Kind::Command, "x")], &m);
        assert!(!d.warnings.iter().any(|w| w.contains("次回车")));
    }

    #[test]
    fn ring_overflow_is_reported() {
        let mut m = meta();
        m.start_seq = 10;
        let d = build_draft(&[cmd(20, "ls")], &m);
        assert!(d.warnings.iter().any(|w| w.contains("录制太长")));
        let d = build_draft(&[cmd(11, "ls")], &m);
        assert!(!d.warnings.iter().any(|w| w.contains("录制太长")));
    }

    #[test]
    fn empty_recording_warns() {
        let d = draft(&[]);
        assert!(d.steps.is_empty());
        assert!(d.warnings.iter().any(|w| w.contains("没有捕获")));
        assert_eq!(d.risk, Risk::Low);
    }

    #[test]
    fn step_count_is_capped() {
        let evs: Vec<TimelineEvent> = (1..=150).map(|i| cmd(i, &format!("echo {i}"))).collect();
        let d = draft(&evs);
        assert_eq!(d.steps.len(), skill::MAX_STEPS);
        assert!(d.warnings.iter().any(|w| w.contains("上限")));
    }

    #[test]
    fn overlong_or_truncated_commands_are_dropped() {
        let mut t = cmd(1, "ls");
        t.truncated = true;
        let long = format!("echo {}", "a".repeat(skill::MAX_COMMAND_CHARS + 10));
        let d = draft(&[t, cmd(2, &long), cmd(3, "pwd")]);
        assert_eq!(d.steps, vec![exec("pwd")]);
        assert!(d.warnings.iter().any(|w| w.contains("过长")));
    }

    #[test]
    fn host_label_is_clipped_and_redacted() {
        let mut m = meta();
        m.host_label = format!("root@10.1.2.3 {}", "h".repeat(200));
        let d = build_draft(&[], &m);
        assert!(d.host.label.chars().count() <= MAX_HOST_LABEL_CHARS);
        assert!(!d.host.label.contains("10.1.2.3"));
    }

    #[test]
    fn draft_steps_pass_skill_validation_and_roundtrip() {
        // 草稿能直接铸成合法 skill：密码步骤 / 应答 / 按键 / 命令都符合 3.1 的约束
        let d = draft(&[
            cmd(1, "cd /srv/app"),
            cmd(2, "sudo systemctl restart web"),
            prompt(3, "password", "[sudo] password for alice:"),
            secret(4),
            cmd(5, "apt upgrade"),
            prompt(6, "yes_no", "Do you want to continue? [Y/n]"),
            cmd(7, "y"),
            keys(8, "<C-c>"),
        ]);
        let input = skill::SkillInput {
            name: "rec".into(),
            description: String::new(),
            params: vec![],
            steps: d.steps.clone(),
            verify: None,
            risk: d.risk,
            host_scope: skill::HostScope::Any,
            source: Some(SkillSource::Recorded),
        };
        let s = input.into_skill("id".into(), 1);
        skill::validate(&s).unwrap_or_else(|e| panic!("{e:?}"));
        // JSON 往返（deny_unknown_fields）
        let json = serde_json::to_string(&d.steps).unwrap();
        let back: Vec<SkillStep> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, d.steps);
    }

    #[test]
    fn draft_json_shape_is_camel_case() {
        let d = draft(&[cmd(1, "ls"), out(2, 1, "a")]);
        let v = serde_json::to_value(&d).unwrap();
        for k in ["session", "host", "startedAtMs", "endedAtMs", "source", "steps", "hints", "risk", "warnings", "notes", "stats"] {
            assert!(v.get(k).is_some(), "缺字段 {k}: {v}");
        }
        assert_eq!(v["source"], "recorded");
        assert_eq!(v["hints"][0]["stepIndex"], 0);
        assert_eq!(v["hints"][0]["outputTail"], "a");
        assert_eq!(v["steps"][0]["kind"], "exec");
        assert_eq!(v["host"]["sessionId"], "s1");
    }

    // ——— 状态 / 与时间线的粘合 ———

    fn mgr() -> AgentManager {
        let m = AgentManager::new();
        m.apply_config(&AgentConfig { enabled: true, ..AgentConfig::default() });
        m
    }

    fn user_cmd(m: &AgentManager, s: &str, line: &str) {
        let e = m.record_user_command(s, line, false).unwrap();
        m.timeline_user(s, &e);
    }

    #[test]
    fn start_requires_global_switch_and_rejects_double_start() {
        let off = AgentManager::new();
        assert!(off.recorder_start("s", "").is_err());
        let m = mgr();
        let (st, _) = m.recorder_start("s", " prod ").unwrap();
        assert!(st.recording && st.host_label == "prod");
        assert!(m.recorder_start("s", "").is_err());
        assert!(m.recorder_status("s").recording);
        assert!(!m.recorder_status("other").recording);
        assert!(m.recorder_cancel("s"));
        assert!(!m.recorder_cancel("s"));
        assert!(!m.recorder_status("s").recording);
    }

    #[test]
    fn stop_without_start_errors() {
        assert!(mgr().recorder_stop("s").is_err());
    }

    #[test]
    fn full_flow_records_only_the_window() {
        let m = mgr();
        // 录制前的操作不该进草稿
        m.feed("s", b"$ ", StreamKind::Out);
        m.feed("s", b"before\n", StreamKind::In);
        user_cmd(&m, "s", "before");
        m.recorder_start("s", "prod").unwrap();
        m.feed("s", b"cd /tmp\r", StreamKind::In);
        user_cmd(&m, "s", "cd /tmp");
        m.feed("s", b"ls\r", StreamKind::In);
        user_cmd(&m, "s", "ls");
        let (draft, _) = m.recorder_stop("s").unwrap();
        assert_eq!(draft.steps, vec![exec("cd /tmp"), exec("ls")]);
        assert!(!draft.warnings.iter().any(|w| w.contains("未捕获")), "{:?}", draft.warnings);
        assert!(!m.recorder_active("s"));
    }

    #[test]
    fn previous_commands_pending_output_is_not_attributed_to_the_recording() {
        let m = mgr();
        m.feed("s", b"x\r", StreamKind::In);
        user_cmd(&m, "s", "slow");
        m.feed("s", b"late output\n", StreamKind::Out);
        // 开始时把 slow 的输出先冲掉，它不会在窗口里变成无主输出
        m.recorder_start("s", "").unwrap();
        user_cmd(&m, "s", "ls");
        m.feed("s", b"ls\r", StreamKind::In);
        let (draft, _) = m.recorder_stop("s").unwrap();
        assert_eq!(draft.steps, vec![exec("ls")]);
        assert!(!draft.warnings.iter().any(|w| w.contains("没有对应命令")), "{:?}", draft.warnings);
    }

    #[test]
    fn uncaptured_input_is_warned_via_the_in_stream() {
        let m = mgr();
        m.recorder_start("s", "").unwrap();
        // 一条走了前端上报，一条（片段 / 宏回放）直接写进了 pty
        m.feed("s", b"ls\r", StreamKind::In);
        user_cmd(&m, "s", "ls");
        m.feed("s", b"uptime\r", StreamKind::In);
        let (draft, _) = m.recorder_stop("s").unwrap();
        assert_eq!(draft.steps, vec![exec("ls")]);
        assert!(draft.warnings.iter().any(|w| w.contains("可能有未捕获的操作")), "{:?}", draft.warnings);
    }

    #[test]
    fn user_context_records_prompt_only_while_recording_and_only_when_notable() {
        let m = mgr();
        m.feed("s", b"Do you want to continue? [Y/n] ", StreamKind::Out);
        // 没在录制：什么都不记
        assert!(m.recorder_user_context("s").is_empty());
        assert!(m.timeline_since("s", 0, 10).is_empty());

        m.recorder_start("s", "").unwrap();
        let evs = m.recorder_user_context("s");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].kind, Kind::Prompt);
        assert_eq!(evs[0].actor, Actor::User);
        assert_eq!(evs[0].prompt_kind.as_deref(), Some("yes_no"));

        // 普通 shell 提示符不值得记
        let m2 = mgr();
        m2.feed("s", b"alice@host:~$ ", StreamKind::Out);
        m2.recorder_start("s", "").unwrap();
        assert!(m2.recorder_user_context("s").is_empty());
    }

    #[test]
    fn answer_flow_end_to_end_through_the_manager() {
        let m = mgr();
        m.feed("s", b"Do you want to continue? [Y/n] ", StreamKind::Out);
        m.recorder_start("s", "").unwrap();
        // 与 agent_user_command 同序：先记提示上下文，再记命令
        let entry = m.record_user_command("s", "y", false).unwrap();
        m.recorder_user_context("s");
        m.timeline_user("s", &entry);
        let (draft, _) = m.recorder_stop("s").unwrap();
        assert!(matches!(&draft.steps[0], SkillStep::ExpectRespond { expect, respond, .. }
            if expect.contains("continue? [Y/n]") && respond == "y<Enter>"), "{:?}", draft.steps);
    }

    #[test]
    fn answer_flow_works_even_after_the_answer_is_echoed() {
        // 回车时终端已把用户敲的 `y` 回显出来：提示行变成 `... [Y/n] y`
        let m = mgr();
        m.feed("s", b"Do you want to continue? [Y/n] ", StreamKind::Out);
        m.recorder_start("s", "").unwrap();
        m.feed("s", b"y", StreamKind::Out);
        let entry = m.record_user_command("s", "y", false).unwrap();
        m.recorder_user_context("s");
        m.timeline_user("s", &entry);
        let (draft, _) = m.recorder_stop("s").unwrap();
        assert!(matches!(&draft.steps[0], SkillStep::ExpectRespond { expect, respond, .. }
            if expect == "Do you want to continue? [Y/n]" && respond == "y<Enter>"), "{:?}", draft.steps);
    }

    #[test]
    fn password_flow_never_stores_the_secret_through_the_manager() {
        let m = mgr();
        m.feed("s", b"[sudo] password for alice: ", StreamKind::Out);
        m.recorder_start("s", "").unwrap();
        // 前端判定无回显：line 为空，只有占位
        let entry = m.record_user_command("s", "", true).unwrap();
        m.recorder_user_context("s");
        m.timeline_user("s", &entry);
        let (draft, _) = m.recorder_stop("s").unwrap();
        assert!(matches!(&draft.steps[0], SkillStep::Secret { reason, .. } if reason.contains("sudo")));
        assert_eq!(draft.steps.len(), 1);
    }

    #[test]
    fn user_keys_whitelist_and_recording_gate() {
        let m = mgr();
        assert!(m.recorder_user_keys("s", "<C-c>").is_empty(), "没在录制不记");
        m.recorder_start("s", "").unwrap();
        assert!(m.recorder_user_keys("s", "hunter2").is_empty(), "非白名单不记");
        assert!(m.recorder_user_keys("s", "<Esc>").is_empty());
        let evs = m.recorder_user_keys("s", "<c-C>");
        assert_eq!(evs.last().unwrap().kind, Kind::Keys);
        let (draft, _) = m.recorder_stop("s").unwrap();
        assert!(matches!(&draft.steps[0], SkillStep::Keys { keys, .. } if keys.eq_ignore_ascii_case("<C-c>")));
    }

    #[test]
    fn timeline_digest_never_includes_recorder_events() {
        // 录制专用的 prompt / keys 事件不应泄进给模型的「用户最近操作」摘要
        let m = mgr();
        m.feed("s", b"Proceed? [y/N] ", StreamKind::Out);
        m.recorder_start("s", "").unwrap();
        m.recorder_user_keys("s", "<C-c>");
        assert_eq!(m.timeline_digest("s", true), "");
    }

    #[test]
    fn in_enters_since_counts_submits() {
        let mut b = crate::agent::buffer::SessionBuffer::new();
        b.feed(b"a\r", StreamKind::In);
        let mark = b.pushed();
        b.feed(b"ls\r\n", StreamKind::In);
        b.feed(b"out\n", StreamKind::Out);
        b.feed(b"x\ny\r", StreamKind::In);
        b.feed(b"\n", StreamKind::In);
        assert_eq!(b.in_enters_since(0), 4);
        // 从 mark 起：`ls\r\n`(1) + `x\n`(1) + `y\r`(1) + 随后的 `\n`(跟在 \r 后 → 同一次提交，不再计)
        assert_eq!(b.in_enters_since(mark), 3);
        assert_eq!(b.in_enters_since(b.pushed()), 0);
        let _ = UserCommandLog::default();
    }
}
