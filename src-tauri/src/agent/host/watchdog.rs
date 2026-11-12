//! 托管看门狗（设计 §5）：纯函数，状态随 checkpoint 持久化。
//!
//! 三种警报：
//! - **Loop**：最近 8 步里同一个动作指纹出现 ≥3 次，且这几次局面签名都没变；
//! - **NoProgress**：连续 6 步局面签名与反馈 hash 都没变，或距上次进展超过 `no_progress_ms`；
//! - **Thrash**：连续 ≥3 轮没有任何动作（模型只说话不做事）。
//!
//! 升级梯度：某个警报第一次出现 → L1（往 feedback 里追加提示）；L1 之后同一警报再次出现
//! → L2（强制 WaitingHuman，卡片附最近 5 条 DecisionRecord）。L2 之后该警报从头计；
//! 局面签名一变（真有进展）就清掉所有 L1 记账。
//!
//! 时间一律是单调时钟（毫秒）。checkpoint 恢复后单调时钟换了基准，要先 `rebase`。

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use super::super::checkpoint::NO_PROGRESS_MS;

pub const LOOP_WINDOW: usize = 8;
pub const LOOP_REPEATS: usize = 3;
pub const NO_PROGRESS_STEPS: u32 = 6;
pub const THRASH_ROUNDS: u32 = 3;
/// L2 卡片附带的最近决策条数
pub const L2_RECENT_DECISIONS: usize = 5;

/// 一步的观察。`acted=false` = 这一轮模型没下发任何动作。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepObs {
    pub action_fp: u64,
    pub sig: String,
    pub fb_hash: u64,
    pub acted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Alarm {
    Loop { fp: u64, n: usize },
    NoProgress { steps: u32, ms: i64 },
    Thrash { rounds: u32 },
}

impl Alarm {
    /// 「同一警报」的判据：Loop 按指纹区分，其余按种类。
    pub fn key(&self) -> String {
        match self {
            Self::Loop { fp, .. } => format!("loop:{fp:x}"),
            Self::NoProgress { .. } => "no_progress".into(),
            Self::Thrash { .. } => "thrash".into(),
        }
    }

    /// L1 追加进 feedback 的提示。
    pub fn hint(&self) -> String {
        match self {
            Self::Loop { n, .. } => format!(
                "[看门狗] 同一个动作在局面没变的情况下已重复 {n} 次。换一种做法，或用 nx_ask_user 请用户决定"
            ),
            Self::NoProgress { steps, ms } => format!(
                "[看门狗] 已经 {steps} 步（{} 秒）屏幕和反馈都没有变化。先 nx_screen 核对现场，确认任务是否还在推进",
                ms / 1000
            ),
            Self::Thrash { rounds } => format!(
                "[看门狗] 连续 {rounds} 轮没有任何动作。要么下发动作，要么 nx_complete，要么 nx_ask_user"
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Level {
    /// 往 feedback 里追加提示
    L1,
    /// 强制 WaitingHuman
    L2,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchdogState {
    pub no_progress_ms: i64,
    window: VecDeque<StepObs>,
    /// 上一次观察的 (sig, fb_hash)。不从 window 取：Loop 触发后会清掉窗口里的同指纹步
    last: Option<(String, u64)>,
    /// 连续多少次观察的 (sig, fb_hash) 相同（含第一次）
    stale_steps: u32,
    /// 上次进展的单调时刻；None = 还没有任何观察
    progress_at_ms: Option<i64>,
    idle_rounds: u32,
    /// 已经发过 L1 的警报 key
    warned: Vec<String>,
}

impl Default for WatchdogState {
    fn default() -> Self {
        Self::new(NO_PROGRESS_MS)
    }
}

impl WatchdogState {
    pub fn new(no_progress_ms: i64) -> Self {
        Self {
            no_progress_ms,
            window: VecDeque::with_capacity(LOOP_WINDOW),
            last: None,
            stale_steps: 0,
            progress_at_ms: None,
            idle_rounds: 0,
            warned: Vec::new(),
        }
    }

    /// checkpoint 恢复后单调时钟换了基准：从现在重新计时（不把停机时间算成没进展）。
    pub fn rebase(&mut self, mono_ms: i64) {
        if self.progress_at_ms.is_some() {
            self.progress_at_ms = Some(mono_ms);
        }
    }

    /// 喂一步观察，返回这一步触发的警报（至多一个，优先级 Loop > Thrash > NoProgress）。
    /// 触发后相应计数清零，同一问题要重新攒够才会再次触发（那时就是 L2）。
    pub fn observe(&mut self, o: StepObs, mono_ms: i64) -> Option<Alarm> {
        let prev = self.last.replace((o.sig.clone(), o.fb_hash));
        let sig_changed = prev.as_ref().map_or(true, |p| p.0 != o.sig);
        let changed = sig_changed || prev.as_ref().map_or(true, |p| p.1 != o.fb_hash);
        if sig_changed && prev.is_some() {
            // 真有进展：之前的 L1 都作废
            self.warned.clear();
        }
        if changed {
            self.stale_steps = 1;
            self.progress_at_ms = Some(mono_ms);
        } else {
            self.stale_steps = self.stale_steps.saturating_add(1);
        }
        self.idle_rounds = if o.acted { 0 } else { self.idle_rounds.saturating_add(1) };

        self.window.push_back(o.clone());
        while self.window.len() > LOOP_WINDOW {
            self.window.pop_front();
        }

        if o.acted {
            let same: Vec<&StepObs> = self.window.iter().filter(|x| x.acted && x.action_fp == o.action_fp).collect();
            if same.len() >= LOOP_REPEATS && same.iter().all(|x| x.sig == o.sig) {
                let n = same.len();
                self.window.retain(|x| x.action_fp != o.action_fp);
                return Some(Alarm::Loop { fp: o.action_fp, n });
            }
        }
        if self.idle_rounds >= THRASH_ROUNDS {
            let rounds = self.idle_rounds;
            self.idle_rounds = 0;
            return Some(Alarm::Thrash { rounds });
        }
        let since = self.progress_at_ms.map_or(0, |t| mono_ms - t);
        if self.stale_steps >= NO_PROGRESS_STEPS || (self.no_progress_ms > 0 && since >= self.no_progress_ms) {
            let steps = self.stale_steps;
            self.stale_steps = 1;
            self.progress_at_ms = Some(mono_ms);
            return Some(Alarm::NoProgress { steps, ms: since });
        }
        None
    }

    /// 这个警报该升到哪一级。L2 后清掉记账（人已经介入，之后从 L1 重新开始）。
    pub fn escalate(&mut self, a: &Alarm) -> Level {
        let k = a.key();
        if let Some(i) = self.warned.iter().position(|w| *w == k) {
            self.warned.remove(i);
            Level::L2
        } else {
            self.warned.push(k);
            Level::L1
        }
    }

    /// `observe` + `escalate`。
    pub fn step(&mut self, o: StepObs, mono_ms: i64) -> Option<(Alarm, Level)> {
        let a = self.observe(o, mono_ms)?;
        let l = self.escalate(&a);
        Some((a, l))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(fp: u64, sig: &str, fb: u64, acted: bool) -> StepObs {
        StepObs { action_fp: fp, sig: sig.into(), fb_hash: fb, acted }
    }
    fn wd() -> WatchdogState {
        WatchdogState::new(0)
    }

    #[test]
    fn loop_needs_three_repeats_with_same_sig() {
        let mut w = wd();
        assert_eq!(w.observe(obs(7, "s", 1, true), 0), None);
        assert_eq!(w.observe(obs(8, "s", 2, true), 1), None);
        assert_eq!(w.observe(obs(7, "s", 3, true), 2), None);
        assert_eq!(w.observe(obs(7, "s", 4, true), 3), Some(Alarm::Loop { fp: 7, n: 3 }));
    }

    #[test]
    fn loop_not_raised_when_sig_changes() {
        let mut w = wd();
        for (i, s) in ["a", "b", "c", "d"].iter().enumerate() {
            assert_eq!(w.observe(obs(7, s, i as u64, true), i as i64), None);
        }
    }

    #[test]
    fn loop_window_is_eight_steps() {
        let mut w = wd();
        w.observe(obs(7, "s", 0, true), 0);
        w.observe(obs(7, "s", 1, true), 0);
        for i in 0..7 {
            // 中间夹 7 个别的动作，第一个 7 被挤出窗口
            assert_eq!(w.observe(obs(100 + i, "s", 10 + i, true), 0), None);
        }
        assert_eq!(w.observe(obs(7, "s", 99, true), 0), None);
    }

    #[test]
    fn thrash_after_three_idle_rounds() {
        let mut w = wd();
        assert_eq!(w.observe(obs(0, "a", 1, false), 0), None);
        assert_eq!(w.observe(obs(0, "b", 2, false), 0), None);
        assert_eq!(w.observe(obs(0, "c", 3, false), 0), Some(Alarm::Thrash { rounds: 3 }));
        // 计数清零
        assert_eq!(w.observe(obs(0, "d", 4, false), 0), None);
        // 有动作就打断
        let mut w = wd();
        w.observe(obs(0, "a", 1, false), 0);
        w.observe(obs(0, "b", 2, false), 0);
        assert_eq!(w.observe(obs(1, "c", 3, true), 0), None);
        assert_eq!(w.observe(obs(0, "d", 4, false), 0), None);
    }

    #[test]
    fn no_progress_after_six_identical_steps() {
        let mut w = wd();
        for i in 0..5 {
            assert_eq!(w.observe(obs(i, "s", 1, true), 0), None, "第 {i} 步");
        }
        assert_eq!(w.observe(obs(5, "s", 1, true), 0), Some(Alarm::NoProgress { steps: 6, ms: 0 }));
    }

    #[test]
    fn feedback_change_counts_as_progress() {
        let mut w = wd();
        for i in 0..20u64 {
            assert_eq!(w.observe(obs(i, "s", i, true), 0), None);
        }
    }

    #[test]
    fn no_progress_by_time() {
        let mut w = WatchdogState::new(60_000);
        assert_eq!(w.observe(obs(1, "s", 1, true), 0), None);
        assert_eq!(w.observe(obs(2, "s", 1, true), 30_000), None);
        assert_eq!(w.observe(obs(3, "s", 1, true), 60_000), Some(Alarm::NoProgress { steps: 3, ms: 60_000 }));
        // 计时从触发时刻重来
        assert_eq!(w.observe(obs(4, "s", 1, true), 61_000), None);
    }

    #[test]
    fn escalation_l1_then_l2_then_reset() {
        let mut w = wd();
        let mut levels = Vec::new();
        for i in 0..30 {
            if let Some((a, l)) = w.step(obs(0, "s", i, false), i as i64) {
                assert!(matches!(a, Alarm::Thrash { .. }));
                levels.push(l);
            }
        }
        assert_eq!(&levels[..4], &[Level::L1, Level::L2, Level::L1, Level::L2]);
    }

    #[test]
    fn progress_clears_l1() {
        let mut w = wd();
        w.observe(obs(0, "a", 1, false), 0);
        w.observe(obs(0, "a", 2, false), 0);
        let (_, l) = w.step(obs(0, "a", 3, false), 0).unwrap();
        assert_eq!(l, Level::L1);
        // 局面变了 = 真进展
        assert_eq!(w.step(obs(1, "b", 4, true), 0), None);
        w.observe(obs(0, "b", 5, false), 0);
        w.observe(obs(0, "b", 6, false), 0);
        let (_, l) = w.step(obs(0, "b", 7, false), 0).unwrap();
        assert_eq!(l, Level::L1);
    }

    #[test]
    fn different_loops_escalate_independently() {
        let mut w = wd();
        assert_eq!(w.escalate(&Alarm::Loop { fp: 1, n: 3 }), Level::L1);
        assert_eq!(w.escalate(&Alarm::Loop { fp: 2, n: 3 }), Level::L1);
        assert_eq!(w.escalate(&Alarm::Loop { fp: 1, n: 3 }), Level::L2);
    }

    #[test]
    fn repeated_loop_escalates_to_l2() {
        let mut w = wd();
        let mut levels = Vec::new();
        for i in 0..6 {
            if let Some((a, l)) = w.step(obs(9, "s", i, true), i as i64) {
                assert_eq!(a, Alarm::Loop { fp: 9, n: 3 });
                levels.push(l);
            }
        }
        assert_eq!(levels, vec![Level::L1, Level::L2]);
    }

    #[test]
    fn serde_round_trip_and_rebase() {
        let mut w = WatchdogState::new(60_000);
        w.observe(obs(1, "s", 1, true), 1_000_000);
        w.observe(obs(1, "s", 1, false), 1_000_100);
        let j = serde_json::to_string(&w).unwrap();
        let mut back: WatchdogState = serde_json::from_str(&j).unwrap();
        assert_eq!(back, w);
        // 新进程的单调时钟从 0 开始；不 rebase 会算出负的耗时，rebase 后从现在计
        back.rebase(5);
        assert_eq!(back.observe(obs(2, "s", 1, true), 59_000), None);
        assert!(matches!(back.observe(obs(3, "s", 1, true), 60_005), Some(Alarm::NoProgress { .. })));
    }
}
