//! P4「长时间运行与恢复」：Checkpoint 数据、六档 Timebox、托管级预算、恢复判定。
//!
//! 设计见 `docs/nexus-agent-steward-design.md` §3.6 / §7。P2 的待回执状态只在内存，
//! 强杀 app 后既没现场也没计时器；本模块把它们变成可落盘、可恢复的事实。
//!
//! 三条安全口径：
//! - **恢复绝不重放没确认的注入**：checkpoint 只记「待回执过」，不记「把这条命令再发一遍」；
//! - **屏幕对不上就降档**：`resume_verdict` 在 hash 漂移时把档位降到 `approve`；
//! - **单调时钟**：计时全部用「相对启动的 elapsed」，墙钟只用于显示，
//!   系统睡眠 / 时间跳变不会误判成超时（`StewardCore` 用 `Instant` 喂这里）。

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

use super::budget::{self, BudgetCheck, BudgetSpent, ExhaustReason, RunBudget};
use super::policy::{Authority, TIER_APPROVE};

/// 六档超时（设计 §7）。每档都有默认动作，不存在「什么都不做」。
pub const SCREEN_ACK_MS: i64 = 3_000;
pub const RECEIPT_MS: i64 = 1_500;
pub const STEP_MS: i64 = 60_000;
pub const IDLE_MS: i64 = 5 * 60_000;
pub const NO_PROGRESS_MS: i64 = 15 * 60_000;
pub const DEADLINE_GRACE_MS: i64 = 60_000;
/// 每完成一个闭环或每 30s 写一次 Checkpoint
pub const CHECKPOINT_INTERVAL_MS: i64 = 30_000;
/// 超过这个时长的 checkpoint 不再自动恢复，交给人决定
pub const CHECKPOINT_TTL_MS: i64 = 24 * 60 * 60_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeboxConfig {
    pub screen_ack_ms: i64,
    pub receipt_ms: i64,
    pub step_ms: i64,
    pub idle_ms: i64,
    pub no_progress_ms: i64,
    /// 用户给的意向 deadline，0 = 不限
    pub deadline_ms: i64,
    pub grace_ms: i64,
}

impl Default for TimeboxConfig {
    fn default() -> Self {
        Self {
            screen_ack_ms: SCREEN_ACK_MS,
            receipt_ms: RECEIPT_MS,
            step_ms: STEP_MS,
            idle_ms: IDLE_MS,
            no_progress_ms: NO_PROGRESS_MS,
            deadline_ms: 0,
            grace_ms: DEADLINE_GRACE_MS,
        }
    }
}

impl TimeboxConfig {
    pub fn from_authority(a: &Authority) -> Self {
        Self {
            deadline_ms: a.timebox_ms as i64,
            ..Self::default()
        }
    }

    pub fn clamped(mut self) -> Self {
        self.screen_ack_ms = self.screen_ack_ms.clamp(500, 60_000);
        self.receipt_ms = self.receipt_ms.clamp(200, 30_000);
        self.step_ms = self.step_ms.clamp(5_000, 30 * 60_000);
        self.idle_ms = self.idle_ms.clamp(10_000, 2 * 60 * 60_000);
        self.no_progress_ms = self.no_progress_ms.clamp(60_000, 6 * 60 * 60_000);
        self.grace_ms = self.grace_ms.clamp(1_000, 30 * 60_000);
        self.deadline_ms = self.deadline_ms.clamp(0, 24 * 60 * 60_000);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeboxHit {
    /// 读屏回执超时 → 降级旁缓冲并标置信度
    ScreenAck,
    /// 写后回执超时 → 换通道重试一次（本实现选择不重发 + 升级，见 decision.rs）
    Receipt,
    /// 单步卡死 → 取消在途动作，回观察
    Step,
    /// 长时间静默 → 收紧心跳 + 状态通知
    Idle,
    /// 长时间无进展 → 主动干预
    NoProgress,
    /// 意向 deadline 到 → 收尾摘要 + 保留现场
    Deadline,
    /// deadline + grace → 硬停，写 Checkpoint 与升级卡
    Grace,
}

impl TimeboxHit {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ScreenAck => "screen_ack",
            Self::Receipt => "receipt",
            Self::Step => "step",
            Self::Idle => "idle",
            Self::NoProgress => "no_progress",
            Self::Deadline => "deadline",
            Self::Grace => "grace",
        }
    }

    /// 触发后的默认动作描述。每一档都有，绝不出现「什么都不做」。
    pub fn default_action(self) -> &'static str {
        match self {
            Self::ScreenAck => "降级到旁路缓冲，标低置信度，暂停一切写入",
            Self::Receipt => "不重发；标记 no_receipt 并升级给人确认",
            Self::Step => "取消在途动作，回到观察，等待下一帧",
            Self::Idle => "收紧心跳到 1s，发一条状态摘要给用户",
            Self::NoProgress => "按局面主动推进：催一句 / 翻页 / 汇报僵死",
            Self::Deadline => "写收尾摘要、保留现场、优雅交还",
            Self::Grace => "硬停托管，写 Checkpoint 与升级卡",
        }
    }
}

/// 时间盒状态。**所有时间参数都是相对启动的 elapsed 毫秒**，由调用方传单调时钟。
#[derive(Debug, Clone, Default)]
pub struct TimeboxState {
    pub started_ms: i64,
    pub last_progress_ms: i64,
    pub last_step_ms: i64,
    pub last_frame_ms: i64,
    raised: BTreeSet<TimeboxHit>,
}

impl TimeboxState {
    pub fn start(now: i64) -> Self {
        Self {
            started_ms: now,
            last_progress_ms: now,
            last_step_ms: now,
            last_frame_ms: now,
            raised: BTreeSet::new(),
        }
    }

    /// 局面有实质变化（新 hash / 新签名）→ 重置空转与静默计时。
    pub fn mark_progress(&mut self, now: i64) {
        self.last_progress_ms = now;
        self.last_frame_ms = now;
        self.raised.remove(&TimeboxHit::Idle);
        self.raised.remove(&TimeboxHit::NoProgress);
    }

    pub fn mark_frame(&mut self, now: i64) {
        self.last_frame_ms = now;
        self.raised.remove(&TimeboxHit::ScreenAck);
    }

    /// 一个决策—动作—回执闭环完成。
    pub fn mark_step(&mut self, now: i64) {
        self.last_step_ms = now;
        self.mark_progress(now);
        self.raised.remove(&TimeboxHit::Step);
        self.raised.remove(&TimeboxHit::Receipt);
    }

    /// 返回本次新触发的档位（同一档只报一次，直到对应 mark_* 重置）。
    pub fn check(&mut self, now: i64, cfg: &TimeboxConfig) -> Vec<TimeboxHit> {
        let mut hits = Vec::new();
        let mut push = |h: TimeboxHit, raised: &mut BTreeSet<TimeboxHit>| {
            if raised.insert(h) {
                hits.push(h);
            }
        };
        if now - self.last_frame_ms >= cfg.screen_ack_ms {
            push(TimeboxHit::ScreenAck, &mut self.raised);
        }
        if now - self.last_step_ms >= cfg.step_ms {
            push(TimeboxHit::Step, &mut self.raised);
        }
        if now - self.last_frame_ms >= cfg.idle_ms {
            push(TimeboxHit::Idle, &mut self.raised);
        }
        if now - self.last_progress_ms >= cfg.no_progress_ms {
            push(TimeboxHit::NoProgress, &mut self.raised);
        }
        if cfg.deadline_ms > 0 {
            let elapsed = now - self.started_ms;
            if elapsed >= cfg.deadline_ms + cfg.grace_ms {
                push(TimeboxHit::Grace, &mut self.raised);
            } else if elapsed >= cfg.deadline_ms {
                push(TimeboxHit::Deadline, &mut self.raised);
            }
        }
        hits
    }
}

/// 托管级累计预算。字段是标量，便于序列化进 Checkpoint。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Spent {
    pub steps: u32,
    pub elapsed_secs: u64,
    pub output_chars: u64,
    pub injections: u32,
}

impl Spent {
    pub fn bump_step(&mut self) {
        self.steps = self.steps.saturating_add(1);
    }
    pub fn bump_injection(&mut self, bytes: usize) {
        self.injections = self.injections.saturating_add(1);
        self.output_chars = self
            .output_chars
            .saturating_add(bytes.min(u32::MAX as usize) as u64);
    }
    pub fn set_elapsed(&mut self, elapsed_ms: i64) {
        self.elapsed_secs = (elapsed_ms.max(0) / 1000) as u64;
    }

    /// 复用 `budget.rs` 的相位状态机与同一套上限语义，只是从「本轮」提升到「本次托管」。
    pub fn check(&self, auth: &Authority) -> BudgetCheck {
        let limits = RunBudget {
            max_steps: auth.max_steps,
            max_minutes: auth.max_minutes,
            max_output_chars: auth.max_output_chars,
        };
        budget::check_budget(
            &BudgetSpent {
                steps: self.steps,
                elapsed_secs: self.elapsed_secs,
                output_chars: self.output_chars,
            },
            &limits,
        )
    }

    pub fn exhaust_text(&self, r: ExhaustReason) -> String {
        format!("预算耗尽（{}）：优雅交还、写摘要，不硬杀进程", r.describe())
    }
}

/// 一次托管的可恢复现场。整条 JSON 落 `steward_checkpoints`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StewardCheckpoint {
    pub contract_id: String,
    pub console_id: String,
    pub goal: String,
    pub authority: Authority,
    pub intent_ids: Vec<String>,
    pub screen_hash: String,
    pub frame_id: u64,
    /// 最近 20 条决策 seq
    pub last_decisions: Vec<i64>,
    /// 待决升级卡 (seq, created_at)
    pub pending_escalation: Option<(i64, i64)>,
    /// 只记「曾经有个待回执」，恢复时不重发
    pub had_pending_receipt: bool,
    pub spent: Spent,
    pub timebox: TimeboxStateJson,
    pub saved_at: i64,
    /// wall-clock，仅用于展示 / TTL
    pub saved_wall_ms: i64,
    /// observing | suspended | completed
    pub state: String,
    pub last_signature: String,
    pub note: String,
}

/// `TimeboxState` 带 BTreeSet，单独做个可序列化镜像（只需时间戳，raised 恢复后清空）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeboxStateJson {
    pub elapsed_ms: i64,
    pub since_progress_ms: i64,
    pub since_step_ms: i64,
    pub since_frame_ms: i64,
}

impl StewardCheckpoint {
    pub fn new(contract_id: &str, console_id: &str, goal: &str, auth: &Authority) -> Self {
        Self {
            contract_id: contract_id.to_string(),
            console_id: console_id.to_string(),
            goal: goal.to_string(),
            authority: auth.clone(),
            intent_ids: Vec::new(),
            screen_hash: String::new(),
            frame_id: 0,
            last_decisions: Vec::new(),
            pending_escalation: None,
            had_pending_receipt: false,
            spent: Spent::default(),
            timebox: TimeboxStateJson::default(),
            saved_at: 0,
            saved_wall_ms: 0,
            state: "observing".to_string(),
            last_signature: String::new(),
            note: String::new(),
        }
    }

    pub fn push_decision(&mut self, seq: i64) {
        if seq > 0 {
            self.last_decisions.push(seq);
            if self.last_decisions.len() > 20 {
                let n = self.last_decisions.len() - 20;
                self.last_decisions.drain(..n);
            }
        }
    }
}

/// 是否该写一次 Checkpoint：闭环刚完成，或距上次已过 30s。
pub fn should_save(last_saved_ms: i64, now: i64, closed_loop: bool) -> bool {
    closed_loop || now - last_saved_ms >= CHECKPOINT_INTERVAL_MS
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum ResumeVerdict {
    /// 现场一致，直接恢复
    Clean,
    /// 屏幕与 checkpoint 不一致：先给差异摘要，档位降为 approve
    Drift { downgrade_to: String },
    /// checkpoint 过期 / 已完成，不自动恢复
    Expired { reason: String },
    /// 上次是断连 / 预算耗尽，等用户确认再续跑
    Suspended { reason: String },
}

/// 恢复判定。启动时扫描未完成契约后调一次。
pub fn resume_verdict(
    cp: &StewardCheckpoint,
    current_hash: Option<&str>,
    now_wall_ms: i64,
) -> ResumeVerdict {
    if cp.state == "completed" {
        return ResumeVerdict::Expired {
            reason: "该契约已完成，无需恢复".to_string(),
        };
    }
    if now_wall_ms - cp.saved_wall_ms > CHECKPOINT_TTL_MS {
        return ResumeVerdict::Expired {
            reason: format!(
                "Checkpoint 已超过 {} 小时，现场可能早已变化",
                CHECKPOINT_TTL_MS / 3_600_000
            ),
        };
    }
    if cp.state == "suspended" {
        return ResumeVerdict::Suspended {
            reason: cp.note.clone(),
        };
    }
    match current_hash {
        Some(h) if !cp.screen_hash.is_empty() && h != cp.screen_hash => ResumeVerdict::Drift {
            downgrade_to: TIER_APPROVE.to_string(),
        },
        _ => ResumeVerdict::Clean,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timebox_fires_each_rung_once() {
        let cfg = TimeboxConfig::default();
        let mut st = TimeboxState::start(0);
        let hits = st.check(3_001, &cfg);
        assert!(hits.contains(&TimeboxHit::ScreenAck));
        assert!(!st.check(3_050, &cfg).contains(&TimeboxHit::ScreenAck));
        st.mark_frame(3_100);
        assert!(st.check(6_200, &cfg).contains(&TimeboxHit::ScreenAck));
    }

    #[test]
    fn all_six_rungs_have_default_actions() {
        for h in [
            TimeboxHit::ScreenAck, TimeboxHit::Receipt, TimeboxHit::Step,
            TimeboxHit::Idle, TimeboxHit::NoProgress, TimeboxHit::Deadline, TimeboxHit::Grace,
        ] {
            assert!(!h.default_action().is_empty());
        }
    }

    #[test]
    fn deadline_and_grace() {
        let mut cfg = TimeboxConfig::default();
        cfg.deadline_ms = 10_000;
        cfg.grace_ms = 1_000;
        let mut st = TimeboxState::start(0);
        assert!(!st.check(9_999, &cfg).contains(&TimeboxHit::Deadline));
        assert!(st.check(10_000, &cfg).contains(&TimeboxHit::Deadline));
        assert!(st.check(11_000, &cfg).contains(&TimeboxHit::Grace));
    }

    #[test]
    fn progress_resets_idle_and_no_progress() {
        let mut cfg = TimeboxConfig::default();
        cfg.no_progress_ms = 1_000;
        cfg.idle_ms = 500;
        let mut st = TimeboxState::start(0);
        assert!(st.check(1_100, &cfg).contains(&TimeboxHit::NoProgress));
        st.mark_progress(1_200);
        let hits = st.check(1_300, &cfg);
        assert!(!hits.contains(&TimeboxHit::NoProgress));
        assert!(!hits.contains(&TimeboxHit::Idle));
    }

    #[test]
    fn budget_exhausts_gracefully() {
        let auth = Authority::default();
        let mut spent = Spent::default();
        for _ in 0..auth.max_steps {
            spent.bump_step();
        }
        assert!(matches!(spent.check(&auth), BudgetCheck::Exhausted(_)));
        let mut cheap = Spent::default();
        assert!(matches!(cheap.check(&auth), BudgetCheck::Continue));
        cheap.bump_injection(12);
        assert_eq!(cheap.injections, 1);
        assert_eq!(cheap.output_chars, 12);
    }

    #[test]
    fn checkpoint_interval_and_decision_ring() {
        assert!(should_save(0, 30_000, false));
        assert!(should_save(0, 10, true));
        assert!(!should_save(20_000, 25_000, false));
        let mut cp = StewardCheckpoint::new("c1", "tab-1", "目标", &Authority::default());
        for i in 1..=25 {
            cp.push_decision(i);
        }
        assert_eq!(cp.last_decisions.len(), 20);
        assert_eq!(cp.last_decisions[0], 6);
    }

    #[test]
    fn resume_clean_drift_expired_suspended() {
        let mut cp = StewardCheckpoint::new("c1", "tab-1", "目标", &Authority::default());
        cp.screen_hash = "aaaa1111".into();
        cp.saved_wall_ms = 1_000;
        assert_eq!(resume_verdict(&cp, Some("aaaa1111"), 2_000), ResumeVerdict::Clean);
        assert_eq!(
            resume_verdict(&cp, Some("bbbb2222"), 2_000),
            ResumeVerdict::Drift { downgrade_to: "approve".into() }
        );
        assert!(matches!(resume_verdict(&cp, None, CHECKPOINT_TTL_MS + 2_000), ResumeVerdict::Expired { .. }));
        cp.state = "suspended".into();
        cp.note = "SSH 断连".into();
        assert!(matches!(resume_verdict(&cp, Some("aaaa1111"), 2_000), ResumeVerdict::Suspended { .. }));
        cp.state = "completed".into();
        assert!(matches!(resume_verdict(&cp, Some("aaaa1111"), 2_000), ResumeVerdict::Expired { .. }));
    }

    #[test]
    fn config_clamps_absurd_values() {
        let c = TimeboxConfig { screen_ack_ms: 1, receipt_ms: 1, step_ms: 1, idle_ms: 1, no_progress_ms: 1, deadline_ms: u64::MAX as i64, grace_ms: 1 }.clamped();
        assert_eq!(c.screen_ack_ms, 500);
        assert!(c.deadline_ms <= 24 * 60 * 60_000);
    }
}
