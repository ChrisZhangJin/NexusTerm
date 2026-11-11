//! P3–P5 共享运行时状态：`StewardCore`。
//!
//! P2 给 `spawn_daemon` 传了 9 个 State，P3 之后还要接着堆参数会很脆。这里把新增的
//! 所有权、授权契约、Checkpoint、Timebox、预算、撤销窗口、Planner 开关、写锁分片、
//! LLM 并发闸、恢复队列收敛成一个 `app.manage()` 状态，循环与命令层各取一份 clone。
//!
//! 所有内部容器都是 `Arc<Mutex<_>>`，与既有 `StewardBrain` / `EventLog` 的惯例一致；
//! 时间的唯一来源是 `Instant`（单调），墙钟只用于展示与 TTL。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;

use super::checkpoint::{Spent, StewardCheckpoint, TimeboxConfig, TimeboxState};
use super::ownership::{OwnershipTable, OwnershipView};
use super::planner::PlannerGate;
use super::policy::{Authority, UndoEntry};

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
    timebox_config: Arc<Mutex<TimeboxConfig>>,
    /// 每次冷启动都是 0；自动应答总闸保持不持久化
    injections: Arc<AtomicUsize>,
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
            timebox_config: Arc::new(Mutex::new(TimeboxConfig::default())),
            injections: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// 单调 elapsed（毫秒）。系统睡眠 / 手动改时钟都不会让它跳变。
    pub fn elapsed_ms(&self) -> i64 {
        self.started.elapsed().as_millis().min(i64::MAX as u128) as i64
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
        self.checkpoints.lock().unwrap().remove(contract_id);
        self.spent.lock().unwrap().remove(contract_id);
        self.ownership().drop(console_id);
        self.timeboxes.lock().unwrap().remove(console_id);
        self.resume_queue
            .lock()
            .unwrap()
            .retain(|id| id != contract_id);
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

    pub fn ensure_console(&self, console_id: &str) {
        let now = self.elapsed_ms();
        self.ownership().ensure(console_id, now);
        self.timeboxes
            .lock()
            .unwrap()
            .entry(console_id.to_string())
            .or_insert_with(|| TimeboxState::start(now));
    }

    pub fn with_timebox<R>(&self, console_id: &str, f: impl FnOnce(&mut TimeboxState) -> R) -> R {
        let now = self.elapsed_ms();
        let mut m = self.timeboxes.lock().unwrap();
        let st = m
            .entry(console_id.to_string())
            .or_insert_with(|| TimeboxState::start(now));
        f(st)
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
        s.set_elapsed(self.elapsed_ms());
        *s
    }

    pub fn bump_injection(&self, contract_id: &str, bytes: usize) -> Spent {
        let mut m = self.spent.lock().unwrap();
        let s = m.entry(contract_id.to_string()).or_default();
        s.bump_injection(bytes);
        s.set_elapsed(self.elapsed_ms());
        *s
    }

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
        let mut q = self.resume_queue.lock().unwrap();
        if !q.iter().any(|x| x == contract_id) {
            q.push(contract_id.to_string());
        }
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
}
