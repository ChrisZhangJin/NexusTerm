//! 运行预算与暂停 / 恢复（4.6）。取代 `act.rs` 里写死的 `MAX_STEPS = 8`。
//!
//! 一轮 Agentic 运行有三个可配置的上限（见 [`RunBudget`]，存在 `AgentConfig.budget`）：
//!
//! - **步数**：Agent 实际执行的动作数（一次 `nx_exec` / `nx_send_keys` / `nx_wait` / `nx_screen` /
//!   `nx_tab` / `nx_memory` …各算 1 步；一次 `nx_skill run` 也只算 1 步 —— 技能引擎自己的
//!   `skill::MAX_STEPS` 上限不受影响）。
//! - **墙钟时间**：从本轮开始（或最近一次「继续」）算起。含等审批 / 等用户的时间，不做扣除。
//! - **输出字符**（可选，0 = 不限）：喂回模型的终端内容 + 模型回复文本的累计字符数，
//!   用来给「一路 `tail -f` / `cat` 大文件烧 token」兜底。
//!
//! 预算用尽**不是失败**：运行停在 [`RunPhase::Paused`]，向面板推一张 `paused` 步骤卡
//! （做了什么、还差什么、怎么继续），**同一轮继续占着会话**，对话历史 / 计划上下文原封不动
//! 留在 `act::run` 的局部变量里 —— 点「继续」就是给它换一份新预算接着转，不是重开一轮。
//! 点「停止」走原来的取消路径。用户也可以手动「暂停」：在下一个步骤边界（一轮模型调用开始前、
//! 或下一个动作开始前）停下。
//!
//! **安全**：暂停状态只有「相位 + 原因」，在内存里、不落库，里面没有任何终端内容 / 命令 / 密码
//! （原因文案只含数字）。暂停时 Agent 把控制权还给用户（`release_control`）；恢复时走与开局相同的
//! 重新获取路径（`ControlTable::acquire` + 每个动作前的 `gate_drive`），审批 / 作用范围 / 危险判断
//! 一律照旧，暂停期间用户动过终端的话模型还会被告知「先用 nx_screen 看一眼现状」。
//!
//! 本模块只含纯逻辑（预算判定、相位状态机、登记表），不碰 Tauri / pty，转移规则可脱离运行时单测。
//!
//! ```text
//!   Running --BudgetExhausted--> Paused(Budget) --Resume--> Running
//!   Running --PauseRequested--> Running{pending} --Boundary--> Paused(User) --Resume--> Running
//!   任意 --Stop / Finish--> Finished（终态）
//! ```

use std::collections::HashMap;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// 默认步数上限（原硬编码为 8）。
pub const DEFAULT_MAX_STEPS: u32 = 25;
/// 步数上限的合法域：至少 1 步，至多 500 步（再多就该拆任务）。
pub const MAX_STEPS_CEIL: u32 = 500;
/// 默认时间上限（分钟）。
pub const DEFAULT_MAX_MINUTES: u32 = 10;
/// 时间上限的最大值（分钟）。
pub const MAX_MINUTES_CEIL: u32 = 24 * 60;
/// 输出字符上限的最大值。
pub const MAX_OUTPUT_CHARS_CEIL: u64 = 100_000_000;

/// 暂停后最多挂多久还没人点「继续 / 停止」，就当用户走开了、收束这一轮（秒）。
pub const PAUSE_IDLE_LIMIT_SECS: u64 = 60 * 60;

// ——————————————————————————————— 配置 ———————————————————————————————

/// 一轮运行的预算上限。存进 `AgentConfig.budget`；旧配置没有这个字段，`serde(default)` 落到默认值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RunBudget {
    /// 最多执行多少步（动作）。0 不合法，规整时回落到默认。
    pub max_steps: u32,
    /// 最长运行多少分钟。0 = 不限时。
    pub max_minutes: u32,
    /// 喂回模型 + 模型回复的累计字符上限。0 = 不限。
    pub max_output_chars: u64,
}

impl Default for RunBudget {
    fn default() -> Self {
        Self { max_steps: DEFAULT_MAX_STEPS, max_minutes: DEFAULT_MAX_MINUTES, max_output_chars: 0 }
    }
}

impl RunBudget {
    /// 夹紧到合法域（配置可能被手改 / 前端传来越界值）。
    pub fn clamped(self) -> Self {
        Self {
            max_steps: if self.max_steps == 0 {
                DEFAULT_MAX_STEPS
            } else {
                self.max_steps.min(MAX_STEPS_CEIL)
            },
            max_minutes: self.max_minutes.min(MAX_MINUTES_CEIL),
            max_output_chars: self.max_output_chars.min(MAX_OUTPUT_CHARS_CEIL),
        }
    }

    fn max_secs(&self) -> u64 {
        u64::from(self.max_minutes) * 60
    }
}

// ——————————————————————————————— 判定 ———————————————————————————————

/// 自本轮开始（或最近一次「继续」）以来已经花掉的预算。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BudgetSpent {
    pub steps: u32,
    pub elapsed_secs: u64,
    pub output_chars: u64,
}

/// 哪一项预算用尽了。携带的数字只用于文案。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ExhaustReason {
    Steps { used: u32, max: u32 },
    Time { used_secs: u64, max_secs: u64 },
    Output { used: u64, max: u64 },
}

impl ExhaustReason {
    /// 给人看的一句话。
    pub fn describe(&self) -> String {
        match *self {
            Self::Steps { used, max } => format!("已执行 {used} 步，达到步数上限（{max} 步）"),
            Self::Time { used_secs, max_secs } => format!(
                "已运行 {} 分钟，达到时间上限（{} 分钟）",
                used_secs.div_ceil(60),
                max_secs / 60
            ),
            Self::Output { used, max } => format!("累计输出 {used} 字符，达到输出上限（{max} 字符）"),
        }
    }

    /// 稳定的机器可读码（前端 / 日志用）。
    pub fn code(&self) -> &'static str {
        match self {
            Self::Steps { .. } => "steps",
            Self::Time { .. } => "time",
            Self::Output { .. } => "output",
        }
    }
}

/// [`check_budget`] 的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetCheck {
    Continue,
    Exhausted(ExhaustReason),
}

/// 纯函数：已花的预算 vs. 上限。**`>=` 即用尽**（「最多 N 步」= 做满 N 步就停）。
/// 多项同时用尽时按 步数 → 时间 → 输出 的顺序报第一项。
pub fn check_budget(spent: &BudgetSpent, limits: &RunBudget) -> BudgetCheck {
    let l = limits.clamped();
    if spent.steps >= l.max_steps {
        return BudgetCheck::Exhausted(ExhaustReason::Steps { used: spent.steps, max: l.max_steps });
    }
    if l.max_minutes > 0 && spent.elapsed_secs >= l.max_secs() {
        return BudgetCheck::Exhausted(ExhaustReason::Time {
            used_secs: spent.elapsed_secs,
            max_secs: l.max_secs(),
        });
    }
    if l.max_output_chars > 0 && spent.output_chars >= l.max_output_chars {
        return BudgetCheck::Exhausted(ExhaustReason::Output {
            used: spent.output_chars,
            max: l.max_output_chars,
        });
    }
    BudgetCheck::Continue
}

// ——————————————————————————————— 相位状态机 ———————————————————————————————

/// 为什么暂停。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PauseCause {
    /// 预算用尽
    Budget { reason: ExhaustReason },
    /// 用户手动点了「暂停」
    User,
}

/// 一轮运行的相位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunPhase {
    /// 在跑。`pause_pending` = 用户已点「暂停」，等下一个步骤边界生效。
    Running { pause_pending: bool },
    Paused(PauseCause),
    /// 终态：正常结束 / 被停止 / 出错。
    Finished,
}

impl RunPhase {
    pub const RUNNING: Self = Self::Running { pause_pending: false };
    pub fn is_paused(&self) -> bool {
        matches!(self, Self::Paused(_))
    }
}

/// 驱动相位转移的事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseEvent {
    /// 预算判定为用尽（运行循环在步骤边界上报）。
    BudgetExhausted(ExhaustReason),
    /// 用户点了「暂停」。
    PauseRequested,
    /// 运行循环走到一个步骤边界：若有待生效的手动暂停，此刻生效。
    Boundary,
    /// 用户点了「继续」。
    Resume,
    /// 用户点了「停止」，或运行结束 / 出错。
    Stop,
}

/// 纯相位转移。不认识的组合原样返回；`Finished` 是吸收态。
pub fn next_phase(phase: RunPhase, ev: PhaseEvent) -> RunPhase {
    use PhaseEvent::*;
    use RunPhase::*;
    match (phase, ev) {
        (Finished, _) => Finished,
        (_, Stop) => Finished,
        (Running { .. }, BudgetExhausted(reason)) => Paused(PauseCause::Budget { reason }),
        (Running { .. }, PauseRequested) => Running { pause_pending: true },
        (Running { pause_pending: true }, Boundary) => Paused(PauseCause::User),
        (Running { pause_pending: false }, Boundary) => phase,
        (Paused(_), Resume) => RunPhase::RUNNING,
        // 已暂停时再点暂停 / 再上报用尽 / 边界：保持原样（不改变原因）
        (p, _) => p,
    }
}

// ——————————————————————————————— 登记表 ———————————————————————————————

/// 按运行（发起会话 id，与 `ActRuntime::inflight` 同键）登记相位。
#[derive(Default)]
pub struct PhaseTable {
    inner: Mutex<HashMap<String, RunPhase>>,
}

impl PhaseTable {
    /// 一轮运行开始：登记为 Running。
    pub fn start(&self, run: &str) {
        self.inner.lock().insert(run.to_string(), RunPhase::RUNNING);
    }

    /// 当前相位；没登记 = `None`。
    pub fn phase(&self, run: &str) -> Option<RunPhase> {
        self.inner.lock().get(run).copied()
    }

    /// 应用一个事件，返回新相位。没登记返回 `None`。
    pub fn apply(&self, run: &str, ev: PhaseEvent) -> Option<RunPhase> {
        let mut map = self.inner.lock();
        let cur = map.get_mut(run)?;
        *cur = next_phase(*cur, ev);
        Some(*cur)
    }

    /// 用户点「暂停」。返回是否有一轮在跑且尚未结束（幂等）。
    pub fn request_pause(&self, run: &str) -> bool {
        matches!(
            self.apply(run, PhaseEvent::PauseRequested),
            Some(RunPhase::Running { .. } | RunPhase::Paused(_))
        )
    }

    /// 用户点「继续」。**只有确实处于暂停时才返回 true**（否则前端的陈旧点击无害）。
    pub fn resume(&self, run: &str) -> bool {
        let mut map = self.inner.lock();
        match map.get_mut(run) {
            Some(p) if p.is_paused() => {
                *p = next_phase(*p, PhaseEvent::Resume);
                true
            }
            _ => false,
        }
    }

    /// 用户点「停止」或运行结束：摘掉登记（表不随运行数涨）。
    pub fn finish(&self, run: &str) {
        self.inner.lock().remove(run);
    }

    /// 该运行此刻是否处于暂停。
    pub fn is_paused(&self, run: &str) -> bool {
        self.phase(run).is_some_and(|p| p.is_paused())
    }
}

// ——————————————————————————————— 文案 ———————————————————————————————

/// 暂停卡片的正文：**做了什么**（`recent_actions`，来自脱敏后的流水账摘要行）、
/// **还差什么**（尚无结论，由模型接着做）、**怎么继续**。
pub fn paused_text(cause: &PauseCause, steps_done: u32, recent_actions: &[String]) -> String {
    let mut s = match cause {
        PauseCause::Budget { reason } => format!("本轮预算已用尽：{}。先暂停在这里。", reason.describe()),
        PauseCause::User => "已按你的要求在步骤边界暂停。".to_string(),
    };
    if recent_actions.is_empty() {
        s.push_str(&format!("\n已做：本段共 {steps_done} 步，尚无可列的动作。"));
    } else {
        s.push_str(&format!("\n已做（本段共 {steps_done} 步，最近 {} 条）：", recent_actions.len()));
        for a in recent_actions {
            s.push_str("\n · ");
            s.push_str(a);
        }
    }
    s.push_str("\n还差：任务还没有给出结论，剩下的部分由助手接着做。");
    s.push_str("\n点「继续」会在同一段对话上下文里换一份新预算接着跑；点「停止」则结束本轮。");
    s
}

/// 恢复后塞给模型的一句话：暂停期间终端可能被用户动过，别凭旧印象继续写。
pub const RESUME_FEEDBACK: &str = "（本轮曾被暂停，用户已点「继续」。暂停期间终端可能被用户操作过，\
    屏幕与之前不一定一样：继续写入之前先用 nx_screen 看一眼现状，再接着完成原来的目标；\
    已经完成的步骤不要重做。）\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn lim(steps: u32, minutes: u32, chars: u64) -> RunBudget {
        RunBudget { max_steps: steps, max_minutes: minutes, max_output_chars: chars }
    }
    fn spent(steps: u32, secs: u64, chars: u64) -> BudgetSpent {
        BudgetSpent { steps, elapsed_secs: secs, output_chars: chars }
    }

    // ——— check_budget ———

    #[test]
    fn continues_below_all_limits() {
        assert_eq!(check_budget(&spent(0, 0, 0), &lim(25, 10, 1000)), BudgetCheck::Continue);
        assert_eq!(check_budget(&spent(24, 599, 999), &lim(25, 10, 1000)), BudgetCheck::Continue);
    }

    #[test]
    fn steps_exhausted_exactly_at_limit() {
        match check_budget(&spent(25, 0, 0), &lim(25, 10, 0)) {
            BudgetCheck::Exhausted(ExhaustReason::Steps { used: 25, max: 25 }) => {}
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn time_exhausted_and_zero_means_unlimited() {
        assert!(matches!(
            check_budget(&spent(1, 600, 0), &lim(25, 10, 0)),
            BudgetCheck::Exhausted(ExhaustReason::Time { used_secs: 600, max_secs: 600 })
        ));
        // 0 分钟 = 不限时：跑再久也不触发
        assert_eq!(check_budget(&spent(1, 10_000_000, 0), &lim(25, 0, 0)), BudgetCheck::Continue);
    }

    #[test]
    fn output_limit_is_optional() {
        assert_eq!(check_budget(&spent(1, 1, u64::MAX), &lim(25, 10, 0)), BudgetCheck::Continue);
        assert!(matches!(
            check_budget(&spent(1, 1, 5000), &lim(25, 10, 5000)),
            BudgetCheck::Exhausted(ExhaustReason::Output { used: 5000, max: 5000 })
        ));
    }

    #[test]
    fn first_exhausted_dimension_wins() {
        let r = check_budget(&spent(30, 9999, 9999), &lim(25, 10, 100));
        assert!(matches!(r, BudgetCheck::Exhausted(ExhaustReason::Steps { .. })));
        let r = check_budget(&spent(1, 9999, 9999), &lim(25, 10, 100));
        assert!(matches!(r, BudgetCheck::Exhausted(ExhaustReason::Time { .. })));
    }

    #[test]
    fn zero_steps_limit_is_clamped_to_default_not_instant_exhaustion() {
        // 配置里 maxSteps=0 不该让每一轮一上来就用尽
        assert_eq!(check_budget(&spent(0, 0, 0), &lim(0, 10, 0)), BudgetCheck::Continue);
        assert_eq!(lim(0, 10, 0).clamped().max_steps, DEFAULT_MAX_STEPS);
        assert_eq!(
            lim(u32::MAX, u32::MAX, u64::MAX).clamped(),
            lim(MAX_STEPS_CEIL, MAX_MINUTES_CEIL, MAX_OUTPUT_CHARS_CEIL)
        );
    }

    #[test]
    fn default_is_larger_than_the_old_hardcoded_eight() {
        let d = RunBudget::default();
        assert!(d.max_steps > 8);
        assert_eq!((d.max_steps, d.max_minutes, d.max_output_chars), (25, 10, 0));
    }

    #[test]
    fn describe_has_only_numbers() {
        assert!(ExhaustReason::Steps { used: 25, max: 25 }.describe().contains("25"));
        assert!(ExhaustReason::Time { used_secs: 601, max_secs: 600 }.describe().contains("11"));
        assert_eq!(ExhaustReason::Output { used: 1, max: 1 }.code(), "output");
    }

    // ——— 配置兼容 ———

    #[test]
    fn budget_deserializes_from_empty_and_partial_objects() {
        let b: RunBudget = serde_json::from_str("{}").unwrap();
        assert_eq!(b, RunBudget::default());
        let b: RunBudget = serde_json::from_str(r#"{"maxSteps":40}"#).unwrap();
        assert_eq!((b.max_steps, b.max_minutes, b.max_output_chars), (40, 10, 0));
        let b: RunBudget = serde_json::from_str(r#"{"maxMinutes":0,"maxOutputChars":123456}"#).unwrap();
        assert_eq!((b.max_steps, b.max_minutes, b.max_output_chars), (25, 0, 123456));
    }

    #[test]
    fn budget_serializes_camel_case() {
        let v = serde_json::to_value(RunBudget::default()).unwrap();
        assert_eq!(v["maxSteps"], 25);
        assert_eq!(v["maxMinutes"], 10);
        assert_eq!(v["maxOutputChars"], 0);
    }

    // ——— 相位状态机 ———

    #[test]
    fn budget_exhaustion_pauses_and_resume_continues() {
        let r = ExhaustReason::Steps { used: 25, max: 25 };
        let p = next_phase(RunPhase::RUNNING, PhaseEvent::BudgetExhausted(r));
        assert_eq!(p, RunPhase::Paused(PauseCause::Budget { reason: r }));
        assert_eq!(next_phase(p, PhaseEvent::Resume), RunPhase::RUNNING);
    }

    #[test]
    fn manual_pause_takes_effect_only_at_boundary() {
        let p = next_phase(RunPhase::RUNNING, PhaseEvent::PauseRequested);
        assert_eq!(p, RunPhase::Running { pause_pending: true });
        assert!(!p.is_paused(), "没到边界前仍在跑");
        let p = next_phase(p, PhaseEvent::Boundary);
        assert_eq!(p, RunPhase::Paused(PauseCause::User));
        assert_eq!(next_phase(p, PhaseEvent::Resume), RunPhase::RUNNING);
    }

    #[test]
    fn boundary_without_request_is_noop() {
        assert_eq!(next_phase(RunPhase::RUNNING, PhaseEvent::Boundary), RunPhase::RUNNING);
    }

    #[test]
    fn stop_wins_from_any_phase_and_finished_is_absorbing() {
        let r = ExhaustReason::Time { used_secs: 1, max_secs: 1 };
        for p in [
            RunPhase::RUNNING,
            RunPhase::Running { pause_pending: true },
            RunPhase::Paused(PauseCause::User),
            RunPhase::Paused(PauseCause::Budget { reason: r }),
        ] {
            assert_eq!(next_phase(p, PhaseEvent::Stop), RunPhase::Finished);
        }
        for ev in [
            PhaseEvent::Resume,
            PhaseEvent::PauseRequested,
            PhaseEvent::Boundary,
            PhaseEvent::BudgetExhausted(r),
        ] {
            assert_eq!(next_phase(RunPhase::Finished, ev), RunPhase::Finished);
        }
    }

    #[test]
    fn paused_keeps_original_cause_on_repeated_events() {
        let r = ExhaustReason::Steps { used: 3, max: 3 };
        let p = RunPhase::Paused(PauseCause::Budget { reason: r });
        assert_eq!(next_phase(p, PhaseEvent::PauseRequested), p);
        assert_eq!(next_phase(p, PhaseEvent::Boundary), p);
        let other = ExhaustReason::Output { used: 1, max: 1 };
        assert_eq!(next_phase(p, PhaseEvent::BudgetExhausted(other)), p);
    }

    #[test]
    fn resume_while_running_is_noop() {
        assert_eq!(next_phase(RunPhase::RUNNING, PhaseEvent::Resume), RunPhase::RUNNING);
        let pending = RunPhase::Running { pause_pending: true };
        assert_eq!(next_phase(pending, PhaseEvent::Resume), pending);
    }

    #[test]
    fn exhaustion_clears_pending_pause_request() {
        let r = ExhaustReason::Steps { used: 1, max: 1 };
        let p = next_phase(RunPhase::Running { pause_pending: true }, PhaseEvent::BudgetExhausted(r));
        assert_eq!(p, RunPhase::Paused(PauseCause::Budget { reason: r }));
        // 恢复后不会立刻又因为旧的「待暂停」停下
        assert_eq!(next_phase(p, PhaseEvent::Resume), RunPhase::RUNNING);
    }

    // ——— 登记表 ———

    #[test]
    fn table_lifecycle() {
        let t = PhaseTable::default();
        assert!(!t.request_pause("s"), "没登记的运行没法暂停");
        assert!(!t.resume("s"));
        t.start("s");
        assert_eq!(t.phase("s"), Some(RunPhase::RUNNING));
        assert!(!t.resume("s"), "没在暂停时点继续是空操作");
        assert!(t.request_pause("s"));
        assert_eq!(t.apply("s", PhaseEvent::Boundary), Some(RunPhase::Paused(PauseCause::User)));
        assert!(t.is_paused("s"));
        assert!(t.resume("s"));
        assert!(!t.is_paused("s"));
        assert!(!t.resume("s"), "重复继续只生效一次");
        let r = ExhaustReason::Steps { used: 2, max: 2 };
        t.apply("s", PhaseEvent::BudgetExhausted(r));
        assert!(t.is_paused("s"));
        t.finish("s");
        assert_eq!(t.phase("s"), None);
        assert!(!t.is_paused("s"));
    }

    #[test]
    fn runs_are_independent() {
        let t = PhaseTable::default();
        t.start("a");
        t.start("b");
        t.request_pause("a");
        t.apply("a", PhaseEvent::Boundary);
        assert!(t.is_paused("a"));
        assert!(!t.is_paused("b"));
        assert!(!t.resume("b"));
        assert!(t.resume("a"));
    }

    #[test]
    fn stop_beats_a_stale_resume() {
        let t = PhaseTable::default();
        t.start("s");
        t.apply("s", PhaseEvent::BudgetExhausted(ExhaustReason::Steps { used: 1, max: 1 }));
        t.apply("s", PhaseEvent::Stop);
        assert!(!t.is_paused("s"));
        assert!(!t.resume("s"), "停止之后陈旧的「继续」不能复活这一轮");
        assert_eq!(t.phase("s"), Some(RunPhase::Finished));
    }

    // ——— 与运行循环同一条等待原语 ———

    async fn no_sleep(_: std::time::Duration) {}

    #[tokio::test]
    async fn hold_wakes_on_resume_and_on_cancel() {
        use crate::agent::control::{wait_unpaused, PauseWait};
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::time::Duration;
        let t = PhaseTable::default();
        t.start("s");
        t.apply("s", PhaseEvent::BudgetExhausted(ExhaustReason::Steps { used: 1, max: 1 }));
        let cancel = AtomicBool::new(false);
        let polls = AtomicUsize::new(0);
        // 轮询到第 3 次时用户点了「继续」
        let r = wait_unpaused(
            || {
                if polls.fetch_add(1, Ordering::Relaxed) == 2 {
                    t.resume("s");
                }
                t.is_paused("s")
            },
            || cancel.load(Ordering::Relaxed),
            Duration::from_secs(PAUSE_IDLE_LIMIT_SECS),
            Duration::from_millis(200),
            no_sleep,
        )
        .await;
        assert_eq!(r, PauseWait::Ready);
        // 再暂停一次，这次用户点「停止」
        t.apply("s", PhaseEvent::BudgetExhausted(ExhaustReason::Steps { used: 1, max: 1 }));
        cancel.store(true, Ordering::Relaxed);
        let r = wait_unpaused(
            || t.is_paused("s"),
            || cancel.load(Ordering::Relaxed),
            Duration::from_secs(PAUSE_IDLE_LIMIT_SECS),
            Duration::from_millis(200),
            no_sleep,
        )
        .await;
        assert_eq!(r, PauseWait::Cancelled);
    }

    #[tokio::test]
    async fn hold_gives_up_after_idle_limit() {
        use crate::agent::control::{wait_unpaused, PauseWait};
        use std::time::Duration;
        let t = PhaseTable::default();
        t.start("s");
        t.apply("s", PhaseEvent::PauseRequested);
        t.apply("s", PhaseEvent::Boundary);
        let r = wait_unpaused(
            || t.is_paused("s"),
            || false,
            Duration::from_secs(PAUSE_IDLE_LIMIT_SECS),
            Duration::from_secs(600),
            no_sleep,
        )
        .await;
        assert_eq!(r, PauseWait::TookOver);
    }

    // ——— 文案 ———

    #[test]
    fn paused_text_says_done_remaining_and_how_to_continue() {
        let c = PauseCause::Budget { reason: ExhaustReason::Steps { used: 25, max: 25 } };
        let s = paused_text(&c, 25, &["exec[web1] ls".to_string(), "exec[web1] df -h".to_string()]);
        assert!(s.contains("步数上限"));
        assert!(s.contains("exec[web1] df -h"));
        assert!(s.contains("还差"));
        assert!(s.contains("继续") && s.contains("停止"));
        assert!(paused_text(&PauseCause::User, 0, &[]).contains("暂停"));
    }
}
