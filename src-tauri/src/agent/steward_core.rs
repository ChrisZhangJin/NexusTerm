//! P3–P5 共享运行时状态：`StewardCore`。
//!
//! P2 给 `spawn_daemon` 传了 9 个 State，P3 之后还要接着堆参数会很脆。这里把新增的
//! 所有权、授权契约、Checkpoint、Timebox、预算、撤销窗口、Planner 开关、写锁分片、
//! LLM 并发闸、恢复队列收敛成一个 `app.manage()` 状态，循环与命令层各取一份 clone。
//!
//! 所有内部容器都是 `Arc<Mutex<_>>`，与既有 `StewardBrain` / `EventLog` 的惯例一致；
//! 时间的唯一来源是 `Instant`（单调），墙钟只用于展示与 TTL。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;

use super::checkpoint::{Spent, StewardCheckpoint, TimeboxConfig, TimeboxState};
use super::ownership::{OwnershipTable, OwnershipView};
use super::planner::PlannerGate;
use super::policy::{Authority, UndoEntry};

/// 恢复项等 console 重新出现的上限：checkpoint 里的 tab id 重启后可能永远不会再出现，
/// 过了这个时长就丢弃，而不是每个 tick 重新排队到天荒地老。
pub const RESUME_TTL_MS: i64 = 10 * 60_000;

/// 全局 LLM 并发上限（设计 §3.5 的 Supervisor 约束）。默认 2。
pub const MAX_LLM_CONCURRENCY: usize = 2;

#[derive(Clone)]
pub struct StewardCore {
    started: Instant,
    ownership: OwnershipTable,
    authorities: Arc<Mutex<HashMap<String, Authority>>>,
    checkpoints: Arc<Mutex<HashMap<String, StewardCheckpoint>>>,
    timeboxes: Arc<Mutex<HashMap<String, TimeboxState>>>,
    spent: Arc<Mutex<HashMap<String, Spent>>>,
    undo: Arc<Mutex<Vec<UndoEntry>>>,
    planner: PlannerGate,
    write_locks: Arc<Mutex<HashSet<String>>>,
    llm_inflight: Arc<AtomicUsize>,
    resume_queue: Arc<Mutex<Vec<String>>>,
    /// 恢复项第一次入队的单调时刻；重新排队不刷新，用来判 `RESUME_TTL_MS`
    resume_since: Arc<Mutex<HashMap<String, i64>>>,
    timebox_config: Arc<Mutex<TimeboxConfig>>,
    /// 每个契约自己的 Timebox 配置；没有单独设置的契约回落到全局 `timebox_config`
    timebox_configs: Arc<Mutex<HashMap<String, TimeboxConfig>>>,
    /// 每次冷启动都是 0；自动应答总闸保持不持久化
    injections: Arc<AtomicUsize>,
    /// 契约开始时的单调时刻。预算的 elapsed 必须从契约开始算，不能用 App 启动时刻：
    /// App 跑了 2 小时后新开的契约，否则第一步就会被判「预算耗尽」。
    contract_started: Arc<Mutex<HashMap<String, i64>>>,
    /// 已报过「预算耗尽」的契约。耗尽是边沿事件，只在第一次越线时暂停 + 通知一次，
    /// 否则每个 tick 都会重复暂停、刷屏事件。
    exhausted: Arc<Mutex<HashSet<String>>>,
    /// 已被消费的升级卡 seq。批准是一次性的：同一条决策无论点几次、从哪个入口批准，
    /// 只允许真正写入一次。
    consumed: Arc<Mutex<HashSet<i64>>>,
    /// 命令层（例如手动 Planner）产生的升级卡，交给守护循环挂到对应 console 的状态上。
    /// 命令层拿不到循环里的 `StewardState`，只能经由这里转交。
    pending_escalations: Arc<Mutex<Vec<(String, i64)>>>,
    /// 单调时钟的测试偏移：生产代码从不改它（恒为 0），测试用它模拟「App 已运行 2 小时」
    clock_offset_ms: Arc<AtomicI64>,
}

impl Default for StewardCore {
    fn default() -> Self {
        Self::new()
    }
}

impl StewardCore {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            ownership: OwnershipTable::new(),
            authorities: Arc::new(Mutex::new(HashMap::new())),
            checkpoints: Arc::new(Mutex::new(HashMap::new())),
            timeboxes: Arc::new(Mutex::new(HashMap::new())),
            spent: Arc::new(Mutex::new(HashMap::new())),
            undo: Arc::new(Mutex::new(Vec::new())),
            planner: PlannerGate::new(),
            write_locks: Arc::new(Mutex::new(HashSet::new())),
            llm_inflight: Arc::new(AtomicUsize::new(0)),
            resume_queue: Arc::new(Mutex::new(Vec::new())),
            resume_since: Arc::new(Mutex::new(HashMap::new())),
            timebox_config: Arc::new(Mutex::new(TimeboxConfig::default())),
            timebox_configs: Arc::new(Mutex::new(HashMap::new())),
            injections: Arc::new(AtomicUsize::new(0)),
            contract_started: Arc::new(Mutex::new(HashMap::new())),
            exhausted: Arc::new(Mutex::new(HashSet::new())),
            consumed: Arc::new(Mutex::new(HashSet::new())),
            pending_escalations: Arc::new(Mutex::new(Vec::new())),
            clock_offset_ms: Arc::new(AtomicI64::new(0)),
        }
    }

    /// 单调 elapsed（毫秒）。系统睡眠 / 手动改时钟都不会让它跳变。
    pub fn elapsed_ms(&self) -> i64 {
        let base = self.started.elapsed().as_millis().min(i64::MAX as u128) as i64;
        base.saturating_add(self.clock_offset_ms.load(Ordering::Relaxed))
    }

    /// 测试专用：把单调时钟往前拨。
    #[cfg(test)]
    pub(crate) fn advance_clock(&self, ms: i64) {
        self.clock_offset_ms.fetch_add(ms, Ordering::Relaxed);
    }

    pub fn ownership(&self) -> &OwnershipTable {
        &self.ownership
    }

    pub fn planner(&self) -> &PlannerGate {
        &self.planner
    }

    // ── 授权契约 ──

    pub fn authority(&self, contract_id: &str) -> Authority {
        self.authorities
            .lock()
            .unwrap()
            .get(contract_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn set_authority(&self, contract_id: &str, auth: Authority) -> Authority {
        let a = auth.clamped();
        self.authorities
            .lock()
            .unwrap()
            .insert(contract_id.to_string(), a.clone());
        a
    }

    pub fn authorities(&self) -> Vec<(String, Authority)> {
        let mut v: Vec<_> = self
            .authorities
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    pub fn drop_contract(&self, contract_id: &str, console_id: &str) {
        self.authorities.lock().unwrap().remove(contract_id);
        self.contract_started.lock().unwrap().remove(contract_id);
        self.exhausted.lock().unwrap().remove(contract_id);
        self.timebox_configs.lock().unwrap().remove(contract_id);
        self.pending_escalations
            .lock()
            .unwrap()
            .retain(|(c, _)| c != console_id);
        self.checkpoints.lock().unwrap().remove(contract_id);
        self.spent.lock().unwrap().remove(contract_id);
        self.ownership().drop(console_id);
        self.timeboxes.lock().unwrap().remove(console_id);
        self.resume_queue
            .lock()
            .unwrap()
            .retain(|id| id != contract_id);
        self.resume_since.lock().unwrap().remove(contract_id);
    }

    // ── Timebox / 预算 ──

    pub fn timebox_config(&self) -> TimeboxConfig {
        self.timebox_config.lock().unwrap().clone()
    }

    pub fn set_timebox_config(&self, cfg: TimeboxConfig) -> TimeboxConfig {
        let c = cfg.clamped();
        *self.timebox_config.lock().unwrap() = c.clone();
        c
    }

    /// 只改某个契约的 Timebox；其它契约仍用全局默认。
    pub fn set_contract_timebox(&self, contract_id: &str, cfg: TimeboxConfig) -> TimeboxConfig {
        let c = cfg.clamped();
        self.timebox_configs
            .lock()
            .unwrap()
            .insert(contract_id.to_string(), c.clone());
        c
    }

    pub fn timebox_config_for(&self, contract_id: &str) -> TimeboxConfig {
        self.timebox_configs
            .lock()
            .unwrap()
            .get(contract_id)
            .cloned()
            .unwrap_or_else(|| self.timebox_config())
    }

    pub fn ensure_console(&self, console_id: &str) {
        let now = self.elapsed_ms();
        self.ownership().ensure(console_id, now);
        self.timeboxes
            .lock()
            .unwrap()
            .entry(console_id.to_string())
            .or_insert_with(|| TimeboxState::start(now));
    }

    /// 契约被替换 / 重新开始时把该 console 的计时清零，旧契约的空转时长不能算到新契约头上。
    pub fn reset_timebox(&self, console_id: &str) {
        let now = self.elapsed_ms();
        self.timeboxes
            .lock()
            .unwrap()
            .insert(console_id.to_string(), TimeboxState::start(now));
    }

    pub fn with_timebox<R>(&self, console_id: &str, f: impl FnOnce(&mut TimeboxState) -> R) -> R {
        let now = self.elapsed_ms();
        let mut m = self.timeboxes.lock().unwrap();
        let st = m
            .entry(console_id.to_string())
            .or_insert_with(|| TimeboxState::start(now));
        f(st)
    }

    // ── 契约计时 / 耗尽边沿 ──

    /// 记录契约开始时刻；重复调用不覆盖（恢复、循环补登记都可能再调一次）。
    pub fn start_contract(&self, contract_id: &str) {
        let now = self.elapsed_ms();
        self.contract_started
            .lock()
            .unwrap()
            .entry(contract_id.to_string())
            .or_insert(now);
    }

    /// 从 Checkpoint 恢复的契约：开始时刻往前推 `prior_elapsed_ms`，预算接着上次的用量算，
    /// 不会因为重启就白送一整份时间预算。
    pub fn resume_contract(&self, contract_id: &str, prior_elapsed_ms: i64) {
        let now = self.elapsed_ms();
        self.contract_started
            .lock()
            .unwrap()
            .insert(contract_id.to_string(), now - prior_elapsed_ms.max(0));
    }

    /// 契约自开始以来的单调毫秒数。没登记过的契约按「刚开始」算（返回 0），
    /// 宁可少算也不能拿 App 启动时刻误判耗尽。
    pub fn contract_elapsed_ms(&self, contract_id: &str) -> i64 {
        let now = self.elapsed_ms();
        self.contract_started
            .lock()
            .unwrap()
            .get(contract_id)
            .map(|t| (now - t).max(0))
            .unwrap_or(0)
    }

    /// 把契约的 elapsed 刷进预算，供每个 tick 的耗尽检查使用。
    pub fn touch_elapsed(&self, contract_id: &str) -> Spent {
        let e = self.contract_elapsed_ms(contract_id);
        let mut m = self.spent.lock().unwrap();
        let s = m.entry(contract_id.to_string()).or_default();
        s.set_elapsed(e);
        *s
    }

    /// 是否已报过耗尽（电平）。交还后循环据此重新核对，不让耗尽状态被绕过。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_exhausted(&self, contract_id: &str) -> bool {
        self.exhausted.lock().unwrap().contains(contract_id)
    }

    /// 第一次标记耗尽返回 true；之后同一契约再标返回 false。
    pub fn mark_exhausted(&self, contract_id: &str) -> bool {
        self.exhausted
            .lock()
            .unwrap()
            .insert(contract_id.to_string())
    }

    // ── 升级卡一次性消费 ──

    /// 原子地占用一条升级卡；已被占用返回 false。必须在写 PTY **之前**调用。
    pub fn try_consume(&self, seq: i64) -> bool {
        let mut m = self.consumed.lock().unwrap();
        let fresh = m.insert(seq);
        if m.len() > 4096 {
            // seq 单调递增，丢掉最旧的一半即可；被丢的 seq 在 brain / store 里也已是终态
            let mut v: Vec<i64> = m.iter().copied().collect();
            v.sort_unstable();
            for s in &v[..v.len() / 2] {
                m.remove(s);
            }
        }
        fresh
    }

    // PR1：守护进程不再注入，只剩测试 / 后续托管任务（reflex）会用
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_consumed(&self, seq: i64) -> bool {
        self.consumed.lock().unwrap().contains(&seq)
    }

    // ── 命令层 → 循环的升级卡转交 ──

    pub fn push_escalation(&self, console_id: &str, seq: i64) {
        self.pending_escalations
            .lock()
            .unwrap()
            .push((console_id.to_string(), seq));
    }

    pub fn drain_escalations(&self) -> Vec<(String, i64)> {
        std::mem::take(&mut *self.pending_escalations.lock().unwrap())
    }

    pub fn spent(&self, contract_id: &str) -> Spent {
        self.spent
            .lock()
            .unwrap()
            .get(contract_id)
            .copied()
            .unwrap_or_default()
    }

    pub fn bump_step(&self, contract_id: &str) -> Spent {
        let mut m = self.spent.lock().unwrap();
        let s = m.entry(contract_id.to_string()).or_default();
        s.bump_step();
        s.set_elapsed(self.contract_elapsed_ms(contract_id));
        *s
    }

    // PR1：守护进程不再注入，只剩测试 / 后续托管任务（reflex）会用
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn bump_injection(&self, contract_id: &str, bytes: usize) -> Spent {
        let mut m = self.spent.lock().unwrap();
        let s = m.entry(contract_id.to_string()).or_default();
        s.bump_injection(bytes);
        s.set_elapsed(self.contract_elapsed_ms(contract_id));
        *s
    }

    // PR1：守护进程不再注入，只剩测试 / 后续托管任务（reflex）会用
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn mark_injection(&self) -> usize {
        self.injections.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn injection_count(&self) -> usize {
        self.injections.load(Ordering::Relaxed)
    }

    // ── Checkpoint ──

    pub fn checkpoint(&self, contract_id: &str) -> Option<StewardCheckpoint> {
        self.checkpoints.lock().unwrap().get(contract_id).cloned()
    }

    pub fn set_checkpoint(&self, cp: StewardCheckpoint) {
        self.checkpoints
            .lock()
            .unwrap()
            .insert(cp.contract_id.clone(), cp);
    }

    pub fn checkpoints(&self) -> Vec<StewardCheckpoint> {
        let mut v: Vec<_> = self.checkpoints.lock().unwrap().values().cloned().collect();
        v.sort_by(|a, b| b.saved_wall_ms.cmp(&a.saved_wall_ms));
        v
    }

    // ── 撤销窗口 ──

    // PR1：守护进程不再注入，只剩测试 / 后续托管任务（reflex）会用
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn add_undo(&self, e: UndoEntry) {
        let mut m = self.undo.lock().unwrap();
        m.retain(|x| x.seq != e.seq);
        m.push(e);
        if m.len() > 64 {
            let n = m.len() - 64;
            m.drain(..n);
        }
    }

    pub fn live_undos(&self, now_wall_ms: i64) -> Vec<UndoEntry> {
        let mut m = self.undo.lock().unwrap();
        m.retain(|e| e.live(now_wall_ms));
        m.clone()
    }

    /// 消费一个撤销窗口；返回被撤销的动作。找不到 / 已过期都返回 None。
    pub fn consume_undo(&self, seq: i64, now_wall_ms: i64) -> Option<UndoEntry> {
        let mut m = self.undo.lock().unwrap();
        let i = m.iter().position(|e| e.seq == seq && e.live(now_wall_ms))?;
        m[i].used = true;
        Some(m[i].clone())
    }

    // ── 写锁分片 / LLM 并发闸（P4 Supervisor）──

    /// 每个 console 一把写许可：两个 tab 并行不会抢同一条 PTY，
    /// 全局 LLM 并发由 `llm_permit` 单独限制。
    pub fn write_permit(&self, console_id: &str) -> Option<WritePermit> {
        let mut locks = self.write_locks.lock().unwrap();
        if !locks.insert(console_id.to_string()) {
            return None;
        }
        Some(WritePermit {
            console_id: console_id.to_string(),
            locks: self.write_locks.clone(),
        })
    }

    pub fn llm_permit(&self) -> Option<LlmPermit> {
        let _ = self
            .llm_inflight
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < MAX_LLM_CONCURRENCY).then_some(n + 1)
            })
            .ok()?;
        Some(LlmPermit {
            counter: self.llm_inflight.clone(),
        })
    }

    pub fn llm_inflight(&self) -> usize {
        self.llm_inflight.load(Ordering::Relaxed)
    }

    pub fn write_locked(&self) -> Vec<String> {
        let mut v: Vec<_> = self.write_locks.lock().unwrap().iter().cloned().collect();
        v.sort();
        v
    }

    // ── 恢复队列 ──

    pub fn push_resume(&self, contract_id: &str) {
        let now = self.elapsed_ms();
        self.resume_since
            .lock()
            .unwrap()
            .entry(contract_id.to_string())
            .or_insert(now);
        let mut q = self.resume_queue.lock().unwrap();
        if !q.iter().any(|x| x == contract_id) {
            q.push(contract_id.to_string());
        }
    }

    /// 恢复项从第一次入队到现在等了多久（没登记过返回 0）。
    pub fn resume_waited_ms(&self, contract_id: &str) -> i64 {
        let now = self.elapsed_ms();
        self.resume_since
            .lock()
            .unwrap()
            .get(contract_id)
            .map(|t| (now - t).max(0))
            .unwrap_or(0)
    }

    /// 恢复项已处理（对齐完成 / 过期丢弃）：清掉计时，之后再投递重新计。
    pub fn forget_resume(&self, contract_id: &str) {
        self.resume_since.lock().unwrap().remove(contract_id);
    }

    pub fn drain_resume(&self) -> Vec<String> {
        std::mem::take(&mut *self.resume_queue.lock().unwrap())
    }

    // ── 视图 ──

    pub fn view(&self, now_wall_ms: i64) -> CoreView {
        CoreView {
            elapsed_ms: self.elapsed_ms(),
            ownership: self.ownership().view(),
            authorities: self.authorities(),
            live_undos: self.live_undos(now_wall_ms),
            planner_enabled: self.planner.enabled(),
            llm_inflight: self.llm_inflight(),
            write_locked: self.write_locked(),
            checkpoint_count: self.checkpoints.lock().unwrap().len(),
            resume_queue: self.resume_queue.lock().unwrap().clone(),
            injections: self.injection_count(),
            timebox: self.timebox_config(),
        }
    }
}

/// RAII 写许可，离开作用域自动释放。
pub struct WritePermit {
    console_id: String,
    locks: Arc<Mutex<HashSet<String>>>,
}

impl Drop for WritePermit {
    fn drop(&mut self) {
        self.locks.lock().unwrap().remove(&self.console_id);
    }
}

pub struct LlmPermit {
    counter: Arc<AtomicUsize>,
}

impl Drop for LlmPermit {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CoreView {
    pub elapsed_ms: i64,
    pub ownership: Vec<OwnershipView>,
    pub authorities: Vec<(String, Authority)>,
    pub live_undos: Vec<UndoEntry>,
    pub planner_enabled: bool,
    pub llm_inflight: usize,
    pub write_locked: Vec<String>,
    pub checkpoint_count: usize,
    pub resume_queue: Vec<String>,
    pub injections: usize,
    pub timebox: TimeboxConfig,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::decision::DecisionRecord;
    use crate::agent::policy::UndoEntry;

    fn dummy_entry(seq: i64, now: i64) -> UndoEntry {
        let r = DecisionRecord {
            seq, ts_ms: now, contract_id: "c".into(), console_id: "t".into(),
            situation_signature: "s".into(), situation_hash: "h".into(),
            ctx_refs: vec![], candidates: vec![],
            chosen: crate::agent::decision::DecisionAction::answer("x".into(), None),
            rationale: String::new(), confidence: 0.9, tier: "notify".into(),
            llm_used: false, outcome: "proposed".into(), escalation: None, receipt: None,
        };
        UndoEntry::new(&r, 30_000)
    }

    #[test]
    fn monotonic_clock_never_goes_backwards() {
        let c = StewardCore::new();
        let a = c.elapsed_ms();
        let b = c.elapsed_ms();
        assert!(b >= a);
    }

    #[test]
    fn write_permit_is_per_console_and_releases_on_drop() {
        let c = StewardCore::new();
        let p = c.write_permit("tab-1");
        assert!(p.is_some());
        assert!(c.write_permit("tab-1").is_none());
        assert!(c.write_permit("tab-2").is_some());
        drop(p);
        assert!(c.write_permit("tab-1").is_some());
    }

    #[test]
    fn contract_elapsed_counts_from_contract_start_not_app_start() {
        let c = StewardCore::new();
        c.advance_clock(3 * 3_600_000);
        c.start_contract("late");
        assert!(c.contract_elapsed_ms("late") < 1_000);
        assert_eq!(c.contract_elapsed_ms("unknown"), 0);
        c.advance_clock(5_000);
        let e = c.touch_elapsed("late").elapsed_secs;
        assert!((5..=6).contains(&e), "{e}");
        // 恢复的契约接着上次的用量
        c.resume_contract("old", 60_000);
        assert!(c.contract_elapsed_ms("old") >= 60_000);
    }

    #[test]
    fn exhaustion_is_edge_triggered_and_reset_by_drop() {
        let c = StewardCore::new();
        assert!(c.mark_exhausted("k"));
        assert!(!c.mark_exhausted("k"));
        c.drop_contract("k", "tab-k");
        assert!(c.mark_exhausted("k"));
    }

    #[test]
    fn escalation_is_consumed_once() {
        let c = StewardCore::new();
        assert!(!c.is_consumed(7));
        assert!(c.try_consume(7));
        assert!(!c.try_consume(7));
        assert!(c.is_consumed(7));
    }

    #[test]
    fn per_contract_timebox_falls_back_to_global() {
        let c = StewardCore::new();
        let mut cfg = c.timebox_config();
        cfg.deadline_ms = cfg.deadline_ms.saturating_add(1_000);
        let set = c.set_contract_timebox("a", cfg.clone());
        assert_eq!(c.timebox_config_for("a").deadline_ms, set.deadline_ms);
        assert_eq!(c.timebox_config_for("b").deadline_ms, c.timebox_config().deadline_ms);
        c.drop_contract("a", "tab-a");
        assert_eq!(c.timebox_config_for("a").deadline_ms, c.timebox_config().deadline_ms);
    }

    #[test]
    fn llm_permits_capped_at_two() {
        let c = StewardCore::new();
        let a = c.llm_permit().unwrap();
        let b = c.llm_permit().unwrap();
        assert!(c.llm_permit().is_none());
        assert_eq!(c.llm_inflight(), 2);
        drop(a);
        assert!(c.llm_permit().is_some());
        drop(b);
    }

    #[test]
    fn undo_window_expires_and_can_be_consumed_once() {
        let c = StewardCore::new();
        c.add_undo(dummy_entry(1, 1_000));
        assert_eq!(c.live_undos(2_000).len(), 1);
        assert!(c.consume_undo(1, 2_000).is_some());
        assert!(c.consume_undo(1, 2_000).is_none());
        assert!(c.live_undos(40_000).is_empty());
    }

    #[test]
    fn authority_and_checkpoint_roundtrip() {
        let c = StewardCore::new();
        let a = c.set_authority("c-1", Authority::default().with_tier("auto"));
        assert_eq!(c.authority("c-1").tier, "auto");
        assert_eq!(a.tier, "auto");
        let mut cp = StewardCheckpoint::new("c-1", "tab-1", "目标", &a);
        cp.screen_hash = "abcd".into();
        c.set_checkpoint(cp);
        assert_eq!(c.checkpoint("c-1").unwrap().screen_hash, "abcd");
        assert_eq!(c.checkpoints().len(), 1);
        c.drop_contract("c-1", "tab-1");
        assert!(c.checkpoint("c-1").is_none());
        assert_eq!(c.authority("c-1").tier, "notify", "删掉后回落默认档");
    }

    #[test]
    fn budget_and_timebox_are_per_contract() {
        let c = StewardCore::new();
        c.ensure_console("tab-1");
        c.bump_step("c-1");
        c.bump_injection("c-2", 10);
        assert_eq!(c.spent("c-1").steps, 1);
        assert_eq!(c.spent("c-1").injections, 0);
        assert_eq!(c.spent("c-2").injections, 1);
        c.with_timebox("tab-1", |t| t.mark_step(1_000));
        assert!(c.elapsed_ms() >= 0);
    }

    #[test]
    fn view_exposes_every_runtime_facet() {
        let c = StewardCore::new();
        c.ensure_console("tab-1");
        c.push_resume("c-9");
        let v = c.view(1_000);
        assert_eq!(v.ownership.len(), 1);
        assert_eq!(v.resume_queue, vec!["c-9".to_string()]);
        assert!(!v.planner_enabled);
        assert_eq!(v.checkpoint_count, 0);
    }

    #[test]
    fn exhausted_budget_survives_user_key_then_handback() {
        let c = StewardCore::new();
        c.ensure_console("tab-1");
        assert!(c.mark_exhausted("c-1"));
        c.ownership().suspend("tab-1", "预算耗尽（步数）", c.elapsed_ms());
        c.ownership().user_key("tab-1", 0, 5, c.elapsed_ms());
        c.ownership().handback("tab-1", c.elapsed_ms(), true);
        assert!(!c.ownership().can_write("tab-1"), "按一下键再交还不能绕过预算耗尽");
        assert!(c.is_exhausted("c-1"), "耗尽是电平，循环据此重新核对");
        assert!(!c.mark_exhausted("c-1"), "通知仍只发一次");
    }
}
