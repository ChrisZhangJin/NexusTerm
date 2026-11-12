//! 常驻 Steward 循环：事件驱动 + 自适应心跳 + 局面分类 + 空转检测 + 决策闭环。
//!
//! 设计见 `docs/nexus-agent-steward-design.md` §3.2 / §3.3 / §6 / §7。
//!
//! ## 这一层改变了什么
//! P0 建成「会话织物」后，屏幕帧只是被动躺在 `ScreenStore` 里，没人看。
//! P1 起一个**常驻** task：契约签下后，即使面板关掉、窗口最小化、用户离开两小时，
//! 后端仍在按自己的心跳读屏、分类、判断空转，并把局面变化写成可回放的事件流。
//! P2 在同一条循环里补上「局面 → 决策 → 注入 → 回执」闭环（`decision.rs`），
//! 让 Steward 真的能按用户预置的意向替 CLI 作答，而不只是描述屏幕。
//!
//! ## PR1 止血：只观察
//! 统一托管设计（`docs/nexus-host-unified-design.md` §1）之后，**唯一的写者是 LLM 托管运行**。
//! 本循环不再有任何写 PTY / SSH 的路径：决策只落成 DecisionRecord / 建议卡，批准建议卡也不写；
//! ControlTable 上有租约持有者（含暂停中的运行）时连建议都不出。下文 P2–P5 关于「注入」的描述
//! 仅作历史说明。
//!
//! ## 硬边界
//! - **默认零 LLM**：分类器、决策编译器、Playbook、意向模板全是纯函数；只有用户显式
//!   打开 `PlannerGate` 且规则编不出回复时才走 `planner.rs`，结果仍要过 PolicyEngine。
//! - **决策只在局面变化时发生**：signature 不含帧哈希，进度条刷屏不会重复出事件，
//!   同一个签名也不会重复注入（每个状态槽记 `last_decision_signature`）。
//! - **注入有总闸 + 策略复核**：`StewardBrain.executionEnabled` 默认 false；P3 起每次注入
//!   还要过 `policy::authorize` 与 `ownership.can_write`，并按 console 取写锁分片。
//! - **回执绝不重发**：1.5s 内没看到新帧就升级给人，不会把同一条命令再发一遍。
//!
//! ## P3–P5 接线
//! `StewardCore` 把所有权、授权契约、Checkpoint、六档 Timebox、预算、撤销窗口、
//! 写锁、LLM 并发闸聚合成一个状态。循环每个 tick 依次处理：恢复队列 → 时间盒 → 预算
//! → 用户裁决 → 回执 → 升级卡超时（按默认动作收敛）→ 分类 → 决策 → 策略复核 → 注入
//! → 写后回执 → Checkpoint。时间全部取 `core.elapsed_ms()` 的单调时钟，
//! 墙钟只用于展示、卡片倒计时与 Checkpoint TTL。
//!
//! ## 为什么是「单 task + 每 console 一个状态槽」
//! 设计里写的是每 console 一个 actor。这里用一个常驻 Supervisor 管理多个状态槽，
//! 语义等价且不会因契约反复签/停而泄漏 task；写锁分片保证两个 tab 不抢同一条 PTY。
//!
//! ## 前端心跳帧（P0 的一个缺口）
//! P0 只在 `onWriteParsed` / `onCursorMove` 时上报，终端静默就断流，
//! 后端分不清「屏幕没动」和「前端不在了」。P1 的 `termRegistry` 每 2s 补一帧同哈希帧，
//! 于是 `idleFrames` 按真实时间增长、断流也能被 `stale_ms` 判出来。

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Notify;

use super::budget::BudgetCheck;
use super::checkpoint::{should_save, ResumeVerdict, StewardCheckpoint, TimeboxHit};
use super::decision::{
    self, DecisionAction, DecisionContext, DecisionRecord, PendingReceipt, ReceiptState,
};
use super::learning;
use super::ownership::OwnershipState;
use super::policy::{self, Authority, PolicyVerdict};
use super::profile::StewardBrain;
use super::situation::{self, Situation, SituationKind};
use super::steward::{now_ms, ScreenSnapshot, ScreenStore, StewardContract, StewardRegistry};
use super::steward_core::{StewardCore, RESUME_TTL_MS};
use crate::cmd::AppState;

/// 事件流环形缓冲上限，够覆盖一次长任务的全部关键局面。
const MAX_EVENTS: usize = 200;
/// 没有任何契约时的巡检间隔：主要为了接住「刚签下契约」的唤醒（其实由 Bus 直接唤醒）。
const IDLE_POLL_MS: i64 = 2_000;
/// 墙钟跳变超过这个值（系统睡眠 / 手动改时间）视为时间漂移，重置各槽的分类时钟。
const CLOCK_JUMP_MS: i64 = 120_000;

/// 静默期心跳：1s → 5s → 15s → 30s → 60s（指数退避，对齐设计 §3.3）。
const SILENT_HEARTBEAT: [i64; 5] = [1_000, 5_000, 15_000, 30_000, 60_000];
/// 任务在跑时收紧：1s → 2s → 5s，别对编译中的进程降太久。
const PROGRESS_HEARTBEAT: [i64; 5] = [1_000, 2_000, 5_000, 5_000, 5_000];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StewardConfig {
    /// 连续多少次同一 signature 判空转（默认 3）
    pub no_progress_ticks: u32,
    /// 前端帧超过多久没到算断流（默认 6s，前端心跳 2s × 3）
    pub stale_ms: i64,
    /// 帧变化时的最小分类间隔，防抖（默认 250ms）
    pub min_classify_gap_ms: i64,
    /// 心跳上限（默认 60s）
    pub heartbeat_cap_ms: i64,
}

impl Default for StewardConfig {
    fn default() -> Self {
        Self {
            no_progress_ticks: 3,
            stale_ms: 6_000,
            min_classify_gap_ms: 250,
            heartbeat_cap_ms: 60_000,
        }
    }
}

#[derive(Clone, Default)]
pub struct StewardConfigStore(Arc<Mutex<StewardConfig>>);

impl StewardConfigStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self) -> StewardConfig {
        self.0.lock().clone()
    }

    /// 只放开两个真机验收会调的旋钮；档位表是行为约定，不暴露给 UI。
    pub fn update(&self, no_progress_ticks: Option<u32>, stale_ms: Option<i64>) -> StewardConfig {
        let mut c = self.0.lock();
        if let Some(n) = no_progress_ticks {
            c.no_progress_ticks = n.clamp(1, 20);
        }
        if let Some(ms) = stale_ms {
            c.stale_ms = ms.clamp(1_000, 600_000);
        }
        c.clone()
    }
}

/// 事件驱动总线：`steward_screen_delta` / 契约变更 / 前端手动请求都会敲一下。
/// 用 `notify_one` 而非 `notify_waiters`：daemon 正在处理时到达的唤醒会留一个 permit，不会丢。
#[derive(Clone, Default)]
pub struct StewardBus(Arc<Notify>);

impl StewardBus {
    pub fn wake(&self) {
        self.0.notify_one();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StewardEventKind {
    SituationChanged,
    NoProgress,
    StaleFeed,
    FeedRestored,
    ScreenLost,
    ContractStarted,
    ContractStopped,
    /// P2：编译出一条可审计的决策
    DecisionMade,
    /// P2：已把回复注入终端，等待回执
    ActionApplied,
    /// P2：需要人裁决（带建议 / 备选 / 倒计时）
    Escalation,
    /// P2：写后 1.5s 没等到新帧，不重发，升级给人
    ReceiptTimeout,
    /// P2：升级卡超时，按默认动作（暂停写入、保留现场）收敛
    EscalationTimeout,
    /// P2：决策被用户否掉 / 纠正
    DecisionRejected,
    /// P3：用户敲键夺权，Steward 转观察态
    #[allow(dead_code)] // PR1 后守护进程不再注入 / 夺权，保留以兼容前端事件种类
    Takeover,
    /// P3：用户交还，附「离开期间」摘要
    Handback,
    /// P3：Notify 档动作进入撤销窗口
    #[allow(dead_code)] // PR1 后守护进程不再注入 / 夺权，保留以兼容前端事件种类
    UndoWindow,
    /// P3：PolicyEngine 硬禁
    PolicyDenied,
    /// P3：升级卡超时后按默认动作收敛
    DefaultActionApplied,
    /// P4：Checkpoint 落盘
    CheckpointSaved,
    /// P4：从 Checkpoint 恢复（含现场对齐结论）
    Resumed,
    /// P4：Timebox 某档触发
    TimeboxHit,
    /// P4：预算耗尽，优雅交还
    BudgetExhausted,
    /// P4：PTY / SSH 断连，Suspended
    Suspended,
    /// P5：成功轨迹沉淀出剧本
    Distilled,
    /// P5：LLM Planner 兜底参与决策
    PlannerUsed,
}

impl StewardEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SituationChanged => "situation_changed",
            Self::NoProgress => "no_progress",
            Self::StaleFeed => "stale_feed",
            Self::FeedRestored => "feed_restored",
            Self::ScreenLost => "screen_lost",
            Self::ContractStarted => "contract_started",
            Self::ContractStopped => "contract_stopped",
            Self::DecisionMade => "decision_made",
            Self::ActionApplied => "action_applied",
            Self::Escalation => "escalation",
            Self::ReceiptTimeout => "receipt_timeout",
            Self::EscalationTimeout => "escalation_timeout",
            Self::DecisionRejected => "decision_rejected",
            Self::Takeover => "takeover",
            Self::Handback => "handback",
            Self::UndoWindow => "undo_window",
            Self::PolicyDenied => "policy_denied",
            Self::DefaultActionApplied => "default_action_applied",
            Self::CheckpointSaved => "checkpoint_saved",
            Self::Resumed => "resumed",
            Self::TimeboxHit => "timebox_hit",
            Self::BudgetExhausted => "budget_exhausted",
            Self::Suspended => "suspended",
            Self::Distilled => "distilled",
            Self::PlannerUsed => "planner_used",
        }
    }
}

/// 一条可回放的观察事件。P2 的 DecisionRecord 会挂在这些事件之后。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StewardEvent {
    pub seq: u64,
    pub ts_ms: i64,
    pub console_id: String,
    pub contract_id: String,
    pub kind: StewardEventKind,
    pub message: String,
    /// P1 的建议卡文本：只建议、不执行
    pub suggestion: Option<String>,
    pub situation: Option<Situation>,
    pub frame_id: u64,
    pub screen_hash: String,
    /// P2 仍恒为 0，写在事件里方便真机验收「全程没调模型」
    pub llm_calls: u32,
    /// P2：局面变化时编出的决策（含依据引用 / 备选 / 升级卡）
    pub decision: Option<DecisionRecord>,
}

#[derive(Default)]
struct EventInner {
    seq: u64,
    buf: VecDeque<StewardEvent>,
}

#[derive(Clone, Default)]
pub struct EventLog(Arc<Mutex<EventInner>>);

impl EventLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// 分配 seq、入环形缓冲，返回带 seq 的事件供 daemon emit。
    pub fn append(&self, events: Vec<StewardEvent>) -> Vec<StewardEvent> {
        let mut inner = self.0.lock();
        let mut out = Vec::with_capacity(events.len());
        for mut e in events {
            inner.seq += 1;
            e.seq = inner.seq;
            inner.buf.push_back(e.clone());
            out.push(e);
        }
        while inner.buf.len() > MAX_EVENTS {
            inner.buf.pop_front();
        }
        out
    }

    pub fn since(&self, after: u64, limit: usize) -> Vec<StewardEvent> {
        let inner = self.0.lock();
        let mut v: Vec<_> = inner
            .buf
            .iter()
            .filter(|e| e.seq > after)
            .cloned()
            .collect();
        let limit = limit.clamp(1, MAX_EVENTS);
        if v.len() > limit {
            v = v[v.len() - limit..].to_vec();
        }
        v
    }

    pub fn seq(&self) -> u64 {
        self.0.lock().seq
    }

    pub fn count(&self) -> usize {
        self.0.lock().buf.len()
    }

    pub fn clear(&self) {
        let mut inner = self.0.lock();
        inner.buf.clear();
        // seq 不重置：前端按 since 增量拉，重置会让前端游标失效
    }
}

/// 每个被托管 console 的循环状态。纯数据 + 纯函数，方便单测与 P4 搬到独立 actor。
#[derive(Debug, Clone)]
pub struct StewardState {
    pub console_id: String,
    pub contract_id: String,
    pub last_frame_hash: String,
    pub last_signature: String,
    /// 连续多少次分类得到同一 signature
    pub same_ticks: u32,
    pub last_classify_ms: i64,
    pub last_change_ms: i64,
    pub no_progress_raised: bool,
    pub stale: bool,
    pub situation: Option<Situation>,
    /// 已为哪个局面签名编过决策。同一签名不重复注入（进度条刷屏也不重复）。
    pub last_decision_signature: String,
    /// 上次决策时刻；同一签名短期内回来（回显造成的抖动）也不重复注入
    pub last_decision_ms: i64,
    /// 已注入但还没确认回执的动作；有值时循环会收紧心跳去等新帧
    pub pending: Option<PendingReceipt>,
    /// 待决升级卡 `(decisionSeq, createdAt)`；超时按默认动作收敛
    pub escalation: Option<(i64, i64)>,
    /// 这个 console 累计注入次数（真机验收看有没有真的写）
    pub injected_count: u32,
}

impl StewardState {
    pub fn new(console_id: String, contract_id: String) -> Self {
        Self {
            console_id,
            contract_id,
            last_frame_hash: String::new(),
            last_signature: String::new(),
            same_ticks: 0,
            last_classify_ms: 0,
            last_change_ms: 0,
            no_progress_raised: false,
            stale: false,
            situation: None,
            last_decision_signature: String::new(),
            last_decision_ms: 0,
            pending: None,
            escalation: None,
            injected_count: 0,
        }
    }

    pub fn heartbeat_ms(&self, cfg: &StewardConfig) -> i64 {
        let level = self.same_ticks.min(4) as usize;
        let in_progress = self
            .situation
            .as_ref()
            .map(|s| s.kind == SituationKind::Progress)
            .unwrap_or(false);
        let steps = if in_progress {
            PROGRESS_HEARTBEAT
        } else {
            SILENT_HEARTBEAT
        };
        steps[level].min(cfg.heartbeat_cap_ms)
    }

    /// 这次唤醒要不要真的分类一次。
    /// 帧变了 → 立刻（250ms 防抖）；帧没变（前端心跳帧）→ 只按自适应心跳节奏分类。
    pub fn should_classify(&self, snap: Option<&ScreenSnapshot>, now: i64, cfg: &StewardConfig) -> bool {
        let frame_changed = snap
            .map(|s| s.hash != self.last_frame_hash)
            .unwrap_or(false);
        let elapsed = now - self.last_classify_ms;
        if frame_changed {
            elapsed >= cfg.min_classify_gap_ms
        } else {
            elapsed >= self.heartbeat_ms(cfg)
        }
    }

    fn event(
        &self,
        kind: StewardEventKind,
        message: String,
        suggestion: Option<String>,
        situation: Option<Situation>,
        now: i64,
        frame_id: u64,
        screen_hash: String,
    ) -> StewardEvent {
        StewardEvent {
            seq: 0, // EventLog::append 统一分配
            ts_ms: now,
            console_id: self.console_id.clone(),
            contract_id: self.contract_id.clone(),
            kind,
            message,
            suggestion,
            situation,
            frame_id,
            screen_hash,
            llm_calls: 0,
            decision: None,
        }
    }

    /// 带决策记录 / 回执结论的事件。`step()` 保持纯观察，决策由常驻循环在 tick 之外附上去。
    fn decision_event(
        &self,
        kind: StewardEventKind,
        message: String,
        situation: Option<Situation>,
        record: Option<DecisionRecord>,
        now: i64,
        frame_id: u64,
        screen_hash: String,
    ) -> StewardEvent {
        let mut e = self.event(kind, message, None, situation, now, frame_id, screen_hash);
        e.decision = record;
        e
    }

    /// 一次分类 tick。**只读**：只产出事件，不写终端、不调模型。
    pub fn step(
        &mut self,
        snap: Option<&ScreenSnapshot>,
        now: i64,
        cfg: &StewardConfig,
    ) -> Vec<StewardEvent> {
        let mut out = Vec::new();
        let Some(snap) = snap else {
            if !self.stale {
                self.stale = true;
                out.push(self.event(
                    StewardEventKind::ScreenLost,
                    "后端还没有这个 console 的屏幕帧：先对齐现场，不判断局面".to_string(),
                    Some("让前端补一帧全量（面板上的「强制全量帧」），再恢复观察。".to_string()),
                    None,
                    now,
                    0,
                    String::new(),
                ));
            }
            self.last_classify_ms = now;
            return out;
        };

        let frame_changed = snap.hash != self.last_frame_hash;
        let sit = situation::classify(snap, now, cfg.stale_ms);
        let stale = sit.stale_ms > cfg.stale_ms;
        let frame_id = snap.frame_id;
        let hash = snap.hash.clone();

        if stale != self.stale {
            self.stale = stale;
            let (kind, message) = if stale {
                (
                    StewardEventKind::StaleFeed,
                    format!(
                        "前端帧断流 {}ms：局面置信度已降权，P1 只读、P3 将收敛到只读动作",
                        sit.stale_ms
                    ),
                )
            } else {
                (
                    StewardEventKind::FeedRestored,
                    "屏幕帧恢复：置信度回到正常，已重新对齐现场".to_string(),
                )
            };
            out.push(self.event(
                kind,
                message,
                None,
                Some(sit.clone()),
                now,
                frame_id,
                hash.clone(),
            ));
        }

        let signature = sit.signature.clone();
        let changed = signature != self.last_signature;
        if changed {
            let first = self.last_signature.is_empty();
            let prev = if first {
                "（首次）".to_string()
            } else {
                self.last_signature.clone()
            };
            self.last_signature = signature;
            self.same_ticks = 0;
            self.no_progress_raised = false;
            self.last_change_ms = now;
            let label = kind_label(sit.kind);
            // 需要人关注的局面立刻出建议卡（设计验收：1 分钟内识别「claude code 在等回答」）。
            let suggestion = if needs_attention(sit.kind) {
                Some(situation::observe_suggestion(
                    sit.kind,
                    0,
                    cfg.no_progress_ticks,
                ))
            } else {
                None
            };
            out.push(self.event(
                StewardEventKind::SituationChanged,
                format!(
                    "局面{}：{}（{} / 置信度 {:.2}）{}",
                    if first { "建立" } else { "变化" },
                    label,
                    sit.program.as_str(),
                    sit.confidence,
                    if first {
                        String::new()
                    } else {
                        format!(" · 上一个签名 {prev}")
                    }
                ),
                suggestion,
                Some(sit.clone()),
                now,
                frame_id,
                hash.clone(),
            ));
        } else {
            self.same_ticks = self.same_ticks.saturating_add(1);
            if self.same_ticks >= cfg.no_progress_ticks && !self.no_progress_raised {
                self.no_progress_raised = true;
                out.push(self.event(
                    StewardEventKind::NoProgress,
                    format!(
                        "连续 {} 个心跳局面未推进：{}（签名稳定 {}ms）",
                        self.same_ticks,
                        kind_label(sit.kind),
                        now - self.last_change_ms
                    ),
                    Some(situation::observe_suggestion(
                        sit.kind,
                        self.same_ticks,
                        cfg.no_progress_ticks,
                    )),
                    Some(sit.clone()),
                    now,
                    frame_id,
                    hash.clone(),
                ));
            }
        }

        self.situation = Some(sit);
        self.last_frame_hash = hash;
        self.last_classify_ms = now;
        if frame_changed {
            self.last_change_ms = now;
        }
        out
    }
}

fn needs_attention(kind: SituationKind) -> bool {
    matches!(
        kind,
        SituationKind::Question
            | SituationKind::Permission
            | SituationKind::Password
            | SituationKind::AwaitingInput
            | SituationKind::Error
            | SituationKind::Menu
            | SituationKind::Pager
    )
}

fn kind_label(kind: SituationKind) -> &'static str {
    match kind {
        SituationKind::Question => "等待回答",
        SituationKind::Permission => "授权确认",
        SituationKind::Password => "密码输入",
        SituationKind::Menu => "编号菜单",
        SituationKind::Pager => "分页器",
        SituationKind::Progress => "任务进行中",
        SituationKind::SubagentStatus => "子任务状态",
        SituationKind::AwaitingInput => "等待输入",
        SituationKind::Idle => "shell 空闲",
        SituationKind::Error => "报错停住",
        SituationKind::AltScreen => "全屏程序（未细分）",
        SituationKind::Unknown => "未知",
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StewardStateInfo {
    pub console_id: String,
    pub contract_id: String,
    pub signature: String,
    pub kind: Option<String>,
    pub program: Option<String>,
    pub confidence: f32,
    pub same_ticks: u32,
    pub heartbeat_ms: i64,
    pub last_classify_ms: i64,
    pub last_change_ms: i64,
    pub stale: bool,
    pub no_progress_raised: bool,
    pub frame_hash: String,
    pub last_decision_signature: String,
    pub pending_receipt: bool,
    pub escalation_seq: i64,
    pub injected_count: u32,
}

impl StewardState {
    fn info(&self, cfg: &StewardConfig) -> StewardStateInfo {
        StewardStateInfo {
            console_id: self.console_id.clone(),
            contract_id: self.contract_id.clone(),
            signature: self.last_signature.clone(),
            kind: self.situation.as_ref().map(|s| s.kind.as_str().to_string()),
            program: self.situation.as_ref().map(|s| s.program.as_str().to_string()),
            confidence: self
                .situation
                .as_ref()
                .map(|s| s.confidence)
                .unwrap_or(0.0),
            same_ticks: self.same_ticks,
            heartbeat_ms: self.heartbeat_ms(cfg),
            last_classify_ms: self.last_classify_ms,
            last_change_ms: self.last_change_ms,
            stale: self.stale,
            no_progress_raised: self.no_progress_raised,
            frame_hash: self.last_frame_hash.clone(),
            last_decision_signature: self.last_decision_signature.clone(),
            pending_receipt: self.pending.is_some(),
            escalation_seq: self.escalation.map(|(seq, _)| seq).unwrap_or(0),
            injected_count: self.injected_count,
        }
    }
}

#[derive(Default)]
struct RuntimeInner {
    running: bool,
    ticks: u64,
    classifies: u64,
    last_tick_ms: i64,
    last_classify_ms: i64,
    consoles: Vec<StewardStateInfo>,
    decisions: u64,
    injections: u64,
    pending_receipts: usize,
    escalations: usize,
    /// 真实的 Planner 调用次数（以前写死 0，面板上的「LLM 调用」永远不变）
    llm_calls: u32,
}

#[derive(Clone, Default)]
pub struct StewardRuntime(Arc<Mutex<RuntimeInner>>);

impl StewardRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    fn set_running(&self, on: bool) {
        self.0.lock().running = on;
    }

    fn mark_classify(&self, now: i64) {
        let mut r = self.0.lock();
        r.classifies += 1;
        r.last_classify_ms = now;
    }

    fn last_tick(&self) -> i64 {
        self.0.lock().last_tick_ms
    }

    fn mark_tick(&self, now: i64, states: &HashMap<String, StewardState>, cfg: &StewardConfig) {
        let mut r = self.0.lock();
        r.ticks += 1;
        r.last_tick_ms = now;
        let mut v: Vec<_> = states.values().map(|s| s.info(cfg)).collect();
        v.sort_by(|a, b| a.console_id.cmp(&b.console_id));
        r.consoles = v;
        r.pending_receipts = states.values().filter(|s| s.pending.is_some()).count();
        r.escalations = states.values().filter(|s| s.escalation.is_some()).count();
    }

    /// 命令层（立即决策 / 手动 Planner）也会产生决策，计数要和循环共用一份。
    pub(crate) fn mark_decision(&self) {
        self.0.lock().decisions += 1;
    }

    pub(crate) fn mark_llm_call(&self) {
        let mut r = self.0.lock();
        r.llm_calls = r.llm_calls.saturating_add(1);
    }

    pub fn info(
        &self,
        event_seq: u64,
        event_count: usize,
        config: StewardConfig,
        execution_enabled: bool,
    ) -> StewardRuntimeInfo {
        let r = self.0.lock();
        StewardRuntimeInfo {
            running: r.running,
            tick_count: r.ticks,
            classify_count: r.classifies,
            last_tick_ms: r.last_tick_ms,
            last_classify_ms: r.last_classify_ms,
            llm_calls: r.llm_calls,
            event_seq,
            event_count,
            config,
            consoles: r.consoles.clone(),
            execution_enabled,
            decision_count: r.decisions,
            injected_count: r.injections,
            pending_receipts: r.pending_receipts,
            escalations: r.escalations,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StewardRuntimeInfo {
    pub running: bool,
    pub tick_count: u64,
    pub classify_count: u64,
    pub last_tick_ms: i64,
    pub last_classify_ms: i64,
    pub llm_calls: u32,
    pub event_seq: u64,
    pub event_count: usize,
    pub config: StewardConfig,
    pub consoles: Vec<StewardStateInfo>,
    /// P2：自动应答总闸；冷启动默认 false
    pub execution_enabled: bool,
    pub decision_count: u64,
    pub injected_count: u64,
    pub pending_receipts: usize,
    pub escalations: usize,
}

/// 算出这次循环之后该睡多久：取所有 console 里最近的一个 due。
fn next_delay(
    states: &HashMap<String, StewardState>,
    contracts: &[super::steward::StewardContract],
    screen: &ScreenStore,
    now: i64,
    cfg: &StewardConfig,
) -> u64 {
    if states.is_empty() {
        return IDLE_POLL_MS as u64;
    }
    let mut delay = cfg.heartbeat_cap_ms;
    // 有动作刚注入或有升级卡在倒计时：把巡检收紧到 250ms / 1s，
    // 别让 60s 的自适应心跳把 1.5s 的回执窗口睡过去。
    if states.values().any(|s| s.pending.is_some()) {
        delay = delay.min(250);
    } else if states.values().any(|s| s.escalation.is_some()) {
        delay = delay.min(1_000);
    }
    for c in contracts {
        let Some(st) = states.get(&c.console_id) else {
            continue;
        };
        let frame_changed = screen
            .get(&c.console_id)
            .map(|s| s.hash != st.last_frame_hash)
            .unwrap_or(false);
        let d = if frame_changed {
            (st.last_classify_ms + cfg.min_classify_gap_ms - now).max(0)
        } else {
            (st.last_classify_ms + st.heartbeat_ms(cfg) - now).max(0)
        };
        delay = delay.min(d);
    }
    (delay.max(50).min(cfg.heartbeat_cap_ms)) as u64
}

/// 用户在升级卡上点「按建议执行 / 我自己来」后的裁决队列。
/// 命令层只投递 seq，**真正注入永远只在常驻循环里发生**——这样待回执状态
/// 只有一个所有者（`StewardState.pending`），不会出现两处状态打架。
#[derive(Clone, Default)]
pub struct StewardResolutions(Arc<Mutex<Vec<(i64, bool)>>>);

impl StewardResolutions {
    pub fn new() -> Self {
        Self::default()
    }

    #[allow(dead_code)]
    pub fn approve(&self, seq: i64) {
        let mut q = self.0.lock();
        q.retain(|(s, _)| *s != seq);
        q.push((seq, true));
    }

    pub fn reject(&self, seq: i64) {
        let mut q = self.0.lock();
        q.retain(|(s, _)| *s != seq);
        q.push((seq, false));
    }

    fn drain(&self) -> Vec<(i64, bool)> {
        std::mem::take(&mut *self.0.lock())
    }
}

/// 局面 + 意向 + 偏好 + 知识 + 剧本 → 一次决策的全部输入。
pub(crate) fn build_context(
    c: &StewardContract,
    sit: &Situation,
    brain: &StewardBrain,
    now: i64,
) -> DecisionContext {
    let (hard, soft) = brain.applicable_preferences(sit.program, &c.host_key);
    DecisionContext {
        situation: sit.clone(),
        contract_id: c.id.clone(),
        console_id: c.console_id.clone(),
        host_key: c.host_key.clone(),
        authority_tier: c.authority_tier.clone(),
        intent: brain.top_intent(&c.id),
        hard,
        soft,
        knowledge: brain.retrieve_kb(sit, 6),
        playbook: brain.match_playbook(sit, &c.host_key),
        recent: brain.recent_decisions(),
        execution_enabled: brain.execution_enabled(),
        now_ms: now,
    }
}

/// 决策落 SQLite，返回自增 seq；失败时返回 0（内存里仍可展示，不因审计写失败卡住循环）。
pub(crate) fn persist_decision(app: &AppHandle, rec: &DecisionRecord) -> i64 {
    let st: State<'_, AppState> = app.state();
    let json = serde_json::to_string(rec).unwrap_or_default();
    st.store
        .steward_decision_insert(
            rec.ts_ms,
            &rec.contract_id,
            &rec.console_id,
            &rec.situation_signature,
            &json,
            &rec.outcome,
        )
        .unwrap_or(0)
}

fn set_outcome(app: &AppHandle, seq: i64, outcome: &str) {
    if seq <= 0 {
        return;
    }
    let st: State<'_, AppState> = app.state();
    st.store.steward_decision_outcome(seq, outcome).ok();
}

// PR1 止血：守护进程只观察。原先的 `inject_keys`（直写 PTY / SSH）已删除——
// 托管改由 LLM 运行（`act::run`）在 ControlTable 租约下动手，守护进程没有任何写路径，
// 决策只落成建议卡 / 记录（设计 §1「守护进程只观察」）。

fn publish(app: &AppHandle, log: &EventLog, events: Vec<StewardEvent>) {
    let stored = log.append(events);
    for e in &stored {
        let _ = app.emit("nx://steward/event", e.clone());
    }
}

// ───────────────── P3–P5 共用辅助 ─────────────────

/// 统一审计：把 DecisionRecord 摘要推进既有 Agent 时间线（「时间线」tab 也能看到托管决策）。
fn push_timeline(app: &AppHandle, rec: &DecisionRecord) {
    let st: State<'_, AppState> = app.state();
    let text = format!(
        "#{} [{}] {}",
        rec.seq,
        rec.outcome,
        rec.chosen.text.chars().take(120).collect::<String>()
    );
    let note = rec.rationale.chars().take(180).collect::<String>();
    let (_, events) = st.agent.timeline_agent(
        &rec.console_id,
        super::timeline::Kind::Decision,
        &text,
        Some(&note),
    );
    super::timeline::publish(app, &rec.console_id, &events);
}

/// 系统级事件（不挂某个 StewardState，例如恢复 / 时间盒 / 预算）。
fn sys_event(
    console_id: &str,
    contract_id: &str,
    kind: StewardEventKind,
    message: impl Into<String>,
    now: i64,
) -> StewardEvent {
    StewardEvent {
        seq: 0,
        ts_ms: now,
        console_id: console_id.to_string(),
        contract_id: contract_id.to_string(),
        kind,
        message: message.into(),
        suggestion: None,
        situation: None,
        frame_id: 0,
        screen_hash: String::new(),
        llm_calls: 0,
        decision: None,
    }
}

/// 决策落库后把 seq 记进 Checkpoint 的最近 20 条。
fn note_checkpoint_decision(core: &StewardCore, c: &StewardContract, seq: i64) {
    if seq <= 0 {
        return;
    }
    let mut cp = core
        .checkpoint(&c.id)
        .unwrap_or_else(|| StewardCheckpoint::new(&c.id, &c.console_id, &c.goal, &core.authority(&c.id)));
    cp.push_decision(seq);
    core.set_checkpoint(cp);
}

/// 把契约 / 意向栈 / 屏幕 hash / 最近决策 / 待决卡 / 预算 / 计时器写进 SQLite。
/// 返回是否写成功；失败只记事件，不阻塞循环。
#[allow(clippy::too_many_arguments)]
fn save_checkpoint(
    app: &AppHandle,
    core: &StewardCore,
    c: &StewardContract,
    st: Option<&StewardState>,
    screen: &ScreenStore,
    now: i64,
    state: &str,
    note: &str,
) -> bool {
    let auth = core.authority(&c.id);
    let mut cp = core
        .checkpoint(&c.id)
        .unwrap_or_else(|| StewardCheckpoint::new(&c.id, &c.console_id, &c.goal, &auth));
    let snap = screen.get(&c.console_id);
    cp.contract_id = c.id.clone();
    cp.console_id = c.console_id.clone();
    // 恢复时要用真实 host_key 重建契约，不能再拼一个 local:<id> 的假值（SSH 契约会因此出作用域）
    cp.host_key = c.host_key.clone();
    // TTL 从第一次落盘算起：周期快照每 30s 刷一次 saved_wall_ms，拿它算 TTL 永远不会过期
    if cp.created_wall_ms <= 0 {
        cp.created_wall_ms = now;
    }
    cp.goal = c.goal.clone();
    cp.authority = auth;
    cp.intent_ids = c.intent_ids.clone();
    cp.screen_hash = snap.as_ref().map(|s| s.hash.clone()).unwrap_or_default();
    cp.frame_id = snap.as_ref().map(|s| s.frame_id).unwrap_or(0);
    cp.pending_escalation = st.and_then(|s| s.escalation);
    cp.had_pending_receipt = st.map(|s| s.pending.is_some()).unwrap_or(false);
    cp.spent = core.spent(&c.id);
    let elapsed = core.elapsed_ms();
    let mut tb = super::checkpoint::TimeboxStateJson::default();
    core.with_timebox(&c.console_id, |t| {
        tb.elapsed_ms = elapsed - t.started_ms;
        tb.since_progress_ms = elapsed - t.last_progress_ms;
        tb.since_step_ms = elapsed - t.last_step_ms;
        tb.since_frame_ms = elapsed - t.last_frame_ms;
    });
    cp.timebox = tb;
    cp.saved_at = elapsed;
    cp.saved_wall_ms = now;
    cp.state = state.to_string();
    cp.last_signature = st
        .map(|s| s.last_decision_signature.clone())
        .unwrap_or_default();
    cp.note = note.to_string();

    let json = serde_json::to_string(&cp).unwrap_or_default();
    let app_state: State<'_, AppState> = app.state();
    let ok = app_state
        .store
        .steward_checkpoint_save(&c.id, &c.console_id, &json, state, now)
        .is_ok();
    core.set_checkpoint(cp);
    ok
}

/// 这个 console 此刻是否是一个活着的本地 PTY / SSH 会话。
fn console_known(app: &AppHandle, console_id: &str) -> bool {
    let st: State<'_, AppState> = app.state();
    st.local.contains(console_id) || st.sessions.contains(console_id)
}

/// 恢复项本 tick 的去向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeWait {
    /// 首帧已到：读屏对齐
    Ready,
    /// 还没首帧、未过 TTL：保持 Suspended，下一 tick 再看
    Wait,
    /// 等满 `RESUME_TTL_MS` 仍不见 console：丢弃
    Expire,
}

/// 纯函数：有没有首帧 + 已等多久 → 去向。
pub(crate) fn resume_wait(has_snap: bool, waited_ms: i64) -> ResumeWait {
    if has_snap {
        ResumeWait::Ready
    } else if waited_ms >= RESUME_TTL_MS {
        ResumeWait::Expire
    } else {
        ResumeWait::Wait
    }
}

/// host 作用域：契约的 host_scope + 会话是否真实存在（fail-closed）。
fn host_in_scope(app: &AppHandle, auth: &Authority, console_id: &str, host_key: &str) -> bool {
    policy::host_in_scope(auth, host_key, console_known(app, console_id))
}

/// P3 所有权：注入前的最后一道闸。返回 false 时循环只记录、不写终端。
fn may_write(core: &StewardCore, console_id: &str) -> bool {
    core.ownership().can_write(console_id)
}

/// 注入前拿写许可。拿不到（同一 console 还有一条写入在途）就**跳过**这次注入，
/// 而不是像以前那样 `let _permit = ...` 拿到 None 也照写——那等于没有锁。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn acquire_write(
    core: &StewardCore,
    console_id: &str,
) -> Option<super::steward_core::WritePermit> {
    if !may_write(core, console_id) {
        return None;
    }
    core.write_permit(console_id)
}

/// 升级卡「按建议执行」的完整闸门。命令层投递前、循环注入前各跑一遍同一个函数，
/// 任何一条不满足都拒绝并给出中文原因：
/// 1. 这张卡仍是待确认（未执行 / 未拒绝 / 未超时，也没被另一次批准消费）；
/// 2. 卡所属契约仍是该 console 的当前契约（停止 / 替换后旧卡作废）；
/// 3. 屏幕 hash 与做决策时一致——建议只对那一屏有效；
/// 4. 现场不是 shell 提示符（任何文本都会被当命令执行）；
/// 5. 动作可注入；PolicyEngine 重新复核不是硬禁；
/// 6. 自动应答总闸开着；所有权允许写。
#[allow(clippy::too_many_arguments)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn approval_gate(
    rec: &DecisionRecord,
    consumed: bool,
    active: Option<&StewardContract>,
    snap: Option<&ScreenSnapshot>,
    auth: &Authority,
    in_scope: bool,
    execution_enabled: bool,
    can_write: bool,
    now: i64,
) -> Result<Vec<u8>, String> {
    if consumed || rec.outcome != "escalated" || rec.escalation.is_none() {
        return Err(format!(
            "决策 #{} 已不是待确认状态（已执行 / 已拒绝 / 已超时），不能再批准",
            rec.seq
        ));
    }
    match active {
        Some(c) if c.id == rec.contract_id && c.console_id == rec.console_id => {}
        _ => return Err("这张卡所属的托管契约已停止或已被替换：不再执行".to_string()),
    }
    let Some(snap) = snap else {
        return Err("还没有该 console 的屏幕帧，无法核对现场：不执行".to_string());
    };
    if snap.hash != rec.situation_hash {
        return Err("屏幕已经变化，建议所依据的局面不复存在：请等待新的决策卡".to_string());
    }
    let sit = situation::classify(snap, now, 6_000);
    if situation::shell_visible(&sit) {
        return Err("屏幕停在 shell 提示符：Steward 不代答，请本人输入".to_string());
    }
    let Some(bytes) = rec.chosen.send_bytes() else {
        return Err("这条建议不能自动注入（密码 / 只读动作需本人处理）".to_string());
    };
    let mut probe = rec.clone();
    if let PolicyVerdict::Deny { reason } = policy::authorize(auth, &mut probe, &sit, in_scope) {
        return Err(format!("PolicyEngine 复核拒绝：{reason}"));
    }
    if !execution_enabled {
        return Err("自动应答总闸已关闭：打开后再确认执行".to_string());
    }
    if !can_write {
        return Err("当前是观察 / 暂停态：请先点「交还」再确认执行".to_string());
    }
    Ok(bytes)
}

/// 只把与这台主机相关的技能交给 KB：`List` 作用域里没有这个 console 的技能不进来。
/// （历史 RunSummary 是全局一张表、不带主机归属，直接不聚合，见 `enrich_context`。）
pub(crate) fn host_scoped_skills(
    skills: Vec<super::skill::Skill>,
    console_id: &str,
) -> Vec<super::skill::Skill> {
    skills
        .into_iter()
        .filter(|s| match &s.host_scope {
            super::skill::HostScope::Any => true,
            // host 名在这里拿不到，只按会话 id 精确匹配；通配写法宁可漏掉也不跨主机泄漏
            super::skill::HostScope::List { hosts } => {
                hosts.iter().any(|h| h.eq_ignore_ascii_case(console_id))
            }
        })
        .collect()
}

/// P5：空转时按局面主动推进，而不是只描述现象（设计 §3.3 / 场景 C）。
/// 返回 None 表示这个局面不该由 Steward 主动插话。
fn no_progress_action(sit: &Situation) -> Option<DecisionAction> {
    // shell 提示符下「请继续…」会被当成命令执行
    if situation::shell_visible(sit) {
        return None;
    }
    match sit.kind {
        SituationKind::Pager => Some(DecisionAction::advance("q")),
        SituationKind::AwaitingInput | SituationKind::Question => Some(DecisionAction::advance(
            "请继续：按既定目标推进，并简要说明当前状态",
        )),
        SituationKind::SubagentStatus => Some(DecisionAction::advance(
            "请检查未运行的子任务并继续，若确实卡住就汇报原因",
        )),
        // 编译 / 下载中：只汇报，不打断
        SituationKind::Progress => None,
        _ => None,
    }
}

/// 空转干预也走一条完整、可审计的 DecisionRecord，而不是偷偷发几个键。
fn advance_record(
    c: &StewardContract,
    sit: &Situation,
    act: DecisionAction,
    now: i64,
) -> DecisionRecord {
    DecisionRecord {
        seq: 0,
        ts_ms: now,
        contract_id: c.id.clone(),
        console_id: c.console_id.clone(),
        situation_signature: sit.signature.clone(),
        situation_hash: sit.screen_hash.clone(),
        ctx_refs: vec![decision::CtxRef::of(
            "history",
            "watchdog",
            "连续同签名空转，按设计 §3.3 主动干预",
        )],
        candidates: vec![DecisionAction::observe("继续观察，不打断当前进程")],
        chosen: act,
        rationale: "空转检测：局面长时间无进展，按意向主动推进而不是只描述现象".to_string(),
        confidence: sit.confidence,
        tier: "notify".to_string(),
        llm_used: false,
        outcome: "proposed".to_string(),
        escalation: None,
        receipt: None,
    }
}

/// 守护进程只观察时给用户的统一说明（建议卡 / 批准兜底 / 事件里共用）。
pub(crate) const OBSERVER_ONLY_NOTE: &str =
    "守护进程只观察、不写终端：请在终端里自己输入，或交给 LLM 托管运行处理";

/// 这个 console 是否被 LLM 托管那一路占着：控制表里有任何租约持有者，或状态不是 `UserDriving`
/// （`AgentDriving` = LLM 正在驾驶；`Paused` = 用户暂停 / 接管了它，同样不该由 Steward 越俎代庖）。
/// 只看状态不够：运行暂停时租约仍在、状态可能被并发操作拨来拨去，持有者才是「谁占着」的真相。
fn llm_run_owns(app: &AppHandle, console_id: &str) -> bool {
    app.try_state::<AppState>()
        .map(|st| control_owned(&st.act.control, console_id))
        .unwrap_or(false)
}

/// `llm_run_owns` 的纯逻辑部分，便于单测。
pub(crate) fn control_owned(table: &super::control::ControlTable, console_id: &str) -> bool {
    !table.holders_of(console_id).is_empty()
        || table.state(console_id) != super::control::ControlState::UserDriving
}

/// 同一签名短期回来（回显抖动）不重复编译 / 注入。
const DECISION_COOLDOWN_MS: i64 = 30_000;

/// 这次 tick 要不要为这个局面编一次决策。纯函数，便于单测「同一签名不重复注入」。
/// 三个条件：局面需要应答、没有动作还在等回执、签名变了或冷却已过。
fn should_decide(st: &StewardState, sit: &Situation, now: i64) -> bool {
    let needs = matches!(
        sit.kind,
        SituationKind::Question
            | SituationKind::Permission
            | SituationKind::Password
            | SituationKind::Menu
            | SituationKind::AwaitingInput
            | SituationKind::Pager
            | SituationKind::Error
    );
    needs
        && st.pending.is_none()
        && (sit.signature != st.last_decision_signature
            || now - st.last_decision_ms >= DECISION_COOLDOWN_MS)
}

/// 应用启动时调一次。返回后循环常驻到 app 退出。
///
/// P3–P5 之后每个 tick 的顺序固定：恢复队列 → 时间盒 → 预算 → 用户裁决 → 回执
/// → 升级卡超时（按默认动作收敛）→ 分类 → 决策 → 策略复核 → 注入 → 回执
/// → Checkpoint / 蒸馏。**注入前必查所有权与写锁**，观察期只记录不写。
#[allow(clippy::too_many_arguments)]
pub fn spawn_daemon(
    app: AppHandle,
    screen: ScreenStore,
    registry: StewardRegistry,
    log: EventLog,
    bus: StewardBus,
    runtime: StewardRuntime,
    config: StewardConfigStore,
    brain: StewardBrain,
    resolutions: StewardResolutions,
    core: StewardCore,
) {
    tauri::async_runtime::spawn(async move {
        runtime.set_running(true);
        let mut states: HashMap<String, StewardState> = HashMap::new();
        let mut last_cp: HashMap<String, i64> = HashMap::new();
        let mut last_distill_ms: i64 = 0;
        // 当前被 LLM 运行占着的 console：只在「进入 / 离开」时记一笔，不按 tick 刷事件
        let mut llm_owned: std::collections::HashSet<String> = std::collections::HashSet::new();
        loop {
            let now = now_ms();
            let mono = core.elapsed_ms();
            let cfg = config.get();
            let contracts = registry.all();

            // 墙钟跳变保护：唤醒瞬间不要把每个 console 都判成「连续 3 次空转」。
            // 时间盒 / 预算一律走 mono，本来就不受墙钟影响。
            let last_tick = runtime.last_tick();
            if last_tick > 0 && now - last_tick > CLOCK_JUMP_MS {
                for st in states.values_mut() {
                    st.last_classify_ms = now;
                    st.last_change_ms = now;
                }
            }

            let mut events = Vec::new();
            states.retain(|id, st| {
                let active = contracts
                    .iter()
                    .any(|c| c.console_id == *id && c.id == st.contract_id);
                if !active {
                    events.push(st.event(
                        StewardEventKind::ContractStopped,
                        "托管已停止：Steward 退出该 console（现场帧保留）".to_string(),
                        None,
                        None,
                        now,
                        0,
                        String::new(),
                    ));
                }
                active
            });
            for c in &contracts {
                if !states.contains_key(&c.console_id) {
                    core.ensure_console(&c.console_id);
                    // 契约计时从这里兜底登记（命令层 / 恢复路径已登记过的不会被覆盖）
                    core.start_contract(&c.id);
                    let st = StewardState::new(c.console_id.clone(), c.id.clone());
                    events.push(st.event(
                        StewardEventKind::ContractStarted,
                        format!("开始常驻托管：目标「{}」", c.goal),
                        None,
                        None,
                        now,
                        0,
                        String::new(),
                    ));
                    states.insert(c.console_id.clone(), st);
                }
            }
            // 命令层（手动 Planner）产生的升级卡：挂到状态槽上，才会有超时收敛与批准入口
            for (console_id, seq) in core.drain_escalations() {
                if let Some(st) = states.get_mut(&console_id) {
                    st.escalation = Some((seq, now));
                }
            }

            // ── P4 恢复：启动扫描出的未完成契约，先读屏对齐 ──────────────
            let mut resume_later = Vec::new();
            for contract_id in core.drain_resume() {
                let Some(c) = registry.get(&contract_id) else {
                    continue;
                };
                let Some(cp) = core.checkpoint(&contract_id) else {
                    continue;
                };
                let snap = screen.get(&c.console_id);
                match resume_wait(snap.is_some(), core.resume_waited_ms(&contract_id)) {
                    ResumeWait::Ready => core.forget_resume(&contract_id),
                    ResumeWait::Wait => {
                        // 前端还没把这个 tab 的首帧送上来：保持启动时的 Suspended，
                        // 下一 tick 再核对；看不到现场时绝不恢复写入
                        resume_later.push(contract_id);
                        continue;
                    }
                    ResumeWait::Expire => {
                        // checkpoint 里的 tab id 重启后多半不会再出现：过了 TTL 就丢弃，
                        // 不再无限重新排队，契约也一并摘掉，免得幽灵契约继续累计时间盒 / 预算事件
                        let msg = format!(
                            "恢复项等了 {} 分钟仍不见 console {}：放弃自动恢复（契约已摘除）",
                            RESUME_TTL_MS / 60_000,
                            c.console_id
                        );
                        registry.stop(&contract_id);
                        core.drop_contract(&contract_id, &c.console_id);
                        app.state::<AppState>()
                            .store
                            .steward_checkpoint_state(&contract_id, "completed")
                            .ok();
                        events.push(sys_event(
                            &c.console_id,
                            &contract_id,
                            StewardEventKind::ContractStopped,
                            msg,
                            now,
                        ));
                        continue;
                    }
                }
                let verdict = super::checkpoint::resume_verdict(
                    &cp,
                    snap.as_ref().map(|s| s.hash.as_str()),
                    now,
                );
                let (msg, suspend) = match &verdict {
                    ResumeVerdict::Clean => {
                        core.ownership().resume(&c.console_id, mono);
                        ("Checkpoint 现场对齐：恢复常驻值守".to_string(), false)
                    }
                    ResumeVerdict::Drift { downgrade_to } => {
                        let mut a = core.authority(&contract_id);
                        a.tier = downgrade_to.clone();
                        core.set_authority(&contract_id, a);
                        registry.amend(&contract_id, None, Some(downgrade_to.clone()));
                        // 降到「确认」档后每一步都要人点头，可以解除启动时的等待首帧暂停
                        core.ownership().resume(&c.console_id, mono);
                        (
                            format!("屏幕与 Checkpoint 不一致：先读屏对齐，档位降为 {downgrade_to}"),
                            false,
                        )
                    }
                    ResumeVerdict::Expired { reason } => {
                        (format!("Checkpoint 不自动恢复：{reason}"), true)
                    }
                    ResumeVerdict::Suspended { reason } => {
                        (format!("上次托管异常中断，等用户确认后续跑：{reason}"), true)
                    }
                };
                if suspend {
                    core.ownership().suspend(&c.console_id, &msg, mono);
                }
                events.push(sys_event(
                    &c.console_id,
                    &contract_id,
                    StewardEventKind::Resumed,
                    msg.clone(),
                    now,
                ));
                save_checkpoint(
                    &app,
                    &core,
                    &c,
                    states.get(&c.console_id),
                    &screen,
                    now,
                    if suspend { "suspended" } else { "observing" },
                    &msg,
                );
            }
            for id in resume_later {
                core.push_resume(&id);
            }

            // ── P4 六档 Timebox + 预算耗尽优雅交还 ─────────────────────
            for c in &contracts {
                // 幽灵契约（console 既没有屏幕帧、也不是活着的 PTY / SSH）不计时、不报事件：
                // 它要么还在等恢复对齐，要么会被恢复 TTL 摘掉
                if screen.get(&c.console_id).is_none() && !console_known(&app, &c.console_id) {
                    continue;
                }
                let tb_cfg = core.timebox_config_for(&c.id);
                let hits = core.with_timebox(&c.console_id, |t| t.check(mono, &tb_cfg));
                for h in hits {
                    let msg = format!("时间盒 [{}]：{}", h.as_str(), h.default_action());
                    events.push(sys_event(
                        &c.console_id,
                        &c.id,
                        StewardEventKind::TimeboxHit,
                        msg.clone(),
                        now,
                    ));
                    core.ownership()
                        .note(&c.console_id, "no_progress", &msg, mono);
                    if matches!(h, TimeboxHit::Deadline | TimeboxHit::Grace) {
                        core.ownership().suspend(&c.console_id, &msg, mono);
                        save_checkpoint(
                            &app,
                            &core,
                            c,
                            states.get(&c.console_id),
                            &screen,
                            now,
                            "suspended",
                            &msg,
                        );
                    }
                }
                // 时间盒的 Deadline / Grace 只按边沿报一次；用户按键→交还把所有权拨回
                // AgentDriving 后，这里按电平再核一遍：过了时限就重新挂起，不被绕过
                if let Some(h) = core.with_timebox(&c.console_id, |t| t.deadline_passed(mono, &tb_cfg)) {
                    if core.ownership().state(&c.console_id) == OwnershipState::AgentDriving {
                        let msg = format!("时间盒 [{}] 仍然有效：交还后保持挂起", h.as_str());
                        core.ownership().suspend(&c.console_id, &msg, mono);
                        events.push(sys_event(
                            &c.console_id,
                            &c.id,
                            StewardEventKind::Suspended,
                            msg,
                            now,
                        ));
                    }
                }
                let auth = core.authority(&c.id);
                // elapsed 每 tick 按契约自己的起点刷新；耗尽通知只在第一次越线时发一次
                let spent = core.touch_elapsed(&c.id);
                if let BudgetCheck::Exhausted(reason) = spent.check(&auth) {
                    if !core.mark_exhausted(&c.id) {
                        // 已经报过：仍按电平复核。交还 / 误恢复把所有权拨回 AgentDriving 时重新挂起，
                        // 预算没补之前不能继续托管（每次状态翻转只记一条，不按 tick 刷）
                        if core.ownership().state(&c.console_id) == OwnershipState::AgentDriving {
                            let msg = format!("{}（交还后仍未补预算）", spent.exhaust_text(reason));
                            core.ownership().suspend(&c.console_id, &msg, mono);
                            events.push(sys_event(
                                &c.console_id,
                                &c.id,
                                StewardEventKind::BudgetExhausted,
                                msg,
                                now,
                            ));
                        }
                        continue;
                    }
                    let msg = spent.exhaust_text(reason);
                    core.ownership().suspend(&c.console_id, &msg, mono);
                    save_checkpoint(
                        &app,
                        &core,
                        c,
                        states.get(&c.console_id),
                        &screen,
                        now,
                        "suspended",
                        &msg,
                    );
                    events.push(sys_event(
                        &c.console_id,
                        &c.id,
                        StewardEventKind::BudgetExhausted,
                        msg,
                        now,
                    ));
                }
            }

            // ── 用户的升级卡裁决：只投递 seq，注入统一由循环执行 ────────
            for (seq, approved) in resolutions.drain() {
                let Some(rec) = brain.decision(seq) else {
                    continue;
                };
                if !approved {
                    let console_id = rec.console_id.clone();
                    // 拒绝也是一次消费：之后迟到的批准一律作废
                    core.try_consume(seq);
                    set_outcome(&app, seq, "rejected");
                    brain.set_decision_outcome(seq, "rejected");
                    if let Some(st) = states.get_mut(&console_id) {
                        st.escalation = None;
                        events.push(st.decision_event(
                            StewardEventKind::DecisionRejected,
                            format!("决策 #{} 已由用户接管：Steward 继续观察，不写终端", seq),
                            None,
                            Some(rec),
                            now,
                            0,
                            String::new(),
                        ));
                    }
                    core.ownership()
                        .note(&console_id, "decision", format!("用户接管决策 #{seq}"), mono);
                    continue;
                }
                // PR1：守护进程只观察，批准一张卡**不会**写终端。命令层已直接拒绝批准，
                // 这里只兜底处理队列里残留的旧批准：消费掉、标成「只观察」并明说。
                core.try_consume(seq);
                set_outcome(&app, seq, "observed");
                brain.set_decision_outcome(seq, "observed");
                if let Some(st) = states.get_mut(&rec.console_id) {
                    st.escalation = None;
                }
                events.push(sys_event(
                    &rec.console_id,
                    &rec.contract_id,
                    StewardEventKind::DecisionRejected,
                    format!("决策 #{seq} 未执行：{OBSERVER_ONLY_NOTE}"),
                    now,
                ));
            }

            for c in &contracts {
                let snap = screen.get(&c.console_id);
                let Some(st) = states.get_mut(&c.console_id) else {
                    continue;
                };
                // P4：帧真的变了才重置 screen_ack / idle 计时（同哈希心跳帧不算进展）。
                if let Some(s) = &snap {
                    if st.last_frame_hash != s.hash {
                        core.with_timebox(&c.console_id, |t| t.mark_frame(mono));
                        core.with_timebox(&c.console_id, |t| t.mark_progress(mono));
                    }
                }

                // ── 写后回执：新帧 = 已生效；超时 = 升级，绝不重发 ──────
                if let Some(p) = st.pending.clone() {
                    let (state, msg) = decision::verify_receipt(&p, snap.as_ref(), now);
                    match state {
                        ReceiptState::Waiting => {}
                        ReceiptState::Applied => {
                            set_outcome(&app, p.decision_seq, "applied");
                            brain.set_decision_outcome(p.decision_seq, "applied");
                            st.pending = None;
                            core.with_timebox(&c.console_id, |t| t.mark_step(mono));
                            core.ownership()
                                .note(&c.console_id, "receipt", &msg, mono);
                            save_checkpoint(
                                &app,
                                &core,
                                c,
                                Some(st),
                                &screen,
                                now,
                                "observing",
                                "决策—动作—回执闭环完成",
                            );
                            events.push(st.decision_event(
                                StewardEventKind::ActionApplied,
                                format!("回执确认：{msg}"),
                                None,
                                brain.decision(p.decision_seq),
                                now,
                                snap.as_ref().map(|s| s.frame_id).unwrap_or(0),
                                snap.as_ref().map(|s| s.hash.clone()).unwrap_or_default(),
                            ));
                        }
                        ReceiptState::Timeout => {
                            set_outcome(&app, p.decision_seq, "no_receipt");
                            brain.set_decision_outcome(p.decision_seq, "no_receipt");
                            st.pending = None;
                            core.ownership()
                                .note(&c.console_id, "receipt_timeout", &msg, mono);
                            save_checkpoint(
                                &app,
                                &core,
                                c,
                                Some(st),
                                &screen,
                                now,
                                "observing",
                                "写后回执超时（未重发）",
                            );
                            events.push(st.decision_event(
                                StewardEventKind::ReceiptTimeout,
                                format!("回执超时：{msg}"),
                                None,
                                brain.decision(p.decision_seq),
                                now,
                                0,
                                String::new(),
                            ));
                        }
                    }
                }

                // ── 升级卡超时：真正执行默认动作（暂停写入 + 保留现场） ──
                if let Some((seq, created)) = st.escalation {
                    if now - created > decision::ESCALATION_TIMEOUT_MS as i64 {
                        st.escalation = None;
                        // 已按默认动作收敛：这张卡不能再被批准
                        core.try_consume(seq);
                        set_outcome(&app, seq, "escalated");
                        core.ownership().takeover(&c.console_id, mono);
                        let msg = format!(
                            "升级卡 #{} 超时：按默认动作暂停写入、保留现场并通知（未僵死）",
                            seq
                        );
                        core.ownership()
                            .note(&c.console_id, "escalation", &msg, mono);
                        save_checkpoint(
                            &app,
                            &core,
                            c,
                            Some(st),
                            &screen,
                            now,
                            "observing",
                            &msg,
                        );
                        events.push(st.decision_event(
                            StewardEventKind::EscalationTimeout,
                            msg.clone(),
                            None,
                            brain.decision(seq),
                            now,
                            0,
                            String::new(),
                        ));
                        events.push(st.decision_event(
                            StewardEventKind::DefaultActionApplied,
                            msg,
                            None,
                            brain.decision(seq),
                            now,
                            0,
                            String::new(),
                        ));
                    }
                }

                // ── 观察：分类 / 空转 / 断流 ────────────────────────────
                let mut saw_no_progress = false;
                if st.should_classify(snap.as_ref(), now, &cfg) {
                    let produced = st.step(snap.as_ref(), now, &cfg);
                    saw_no_progress = produced
                        .iter()
                        .any(|p| p.kind == StewardEventKind::NoProgress);
                    if !produced.is_empty() {
                        runtime.mark_classify(now);
                    }
                    // P3：观察期照样把局面变化记进「离开期间」摘要
                    for p in &produced {
                        core.ownership().note(
                            &c.console_id,
                            if p.kind == StewardEventKind::NoProgress {
                                "no_progress"
                            } else {
                                "decision"
                            },
                            &p.message,
                            mono,
                        );
                    }
                    events.extend(produced);
                }

                // ── 互斥：LLM 托管（act::run）正在驾驶 / 用户暂停了它 → 只看不写 ────
                // 眼睛（上面的采集与分类）照常工作；决策与注入让给 LLM 那一路，
                // 否则两个「手」会同时往同一个终端写。
                if llm_run_owns(&app, &c.console_id) {
                    if llm_owned.insert(c.console_id.clone()) {
                        core.ownership().note(
                            &c.console_id,
                            "decision",
                            "LLM 托管运行接手该终端：守护进程只观察、不出决策",
                            mono,
                        );
                    }
                    continue;
                }
                llm_owned.remove(&c.console_id);

                // ── P5 场景 C：空转时按局面主动推进，而不是只描述现象 ────
                // 与「被动等提问」用同一条 DecisionRecord / PolicyEngine / 回执链路，
                // 进度中（Progress）不插话，Pager / 等待输入 / 子任务僵死才催一句。
                if saw_no_progress {
                    if let Some(sit) = st.situation.clone() {
                        if let Some(act) = no_progress_action(&sit) {
                            let auth = core.authority(&c.id);
                            let in_scope =
                                host_in_scope(&app, &auth, &c.console_id, &c.host_key);
                            // Pager 的 `q` 是确定性按键；其余催促都是自由文本，
                            // 不对应屏幕上的结构化选项，只能出卡等人确认
                            let free_text = act.text != "q";
                            let mut rec = advance_record(c, &sit, act, now);
                            let mut verdict = policy::authorize(&auth, &mut rec, &sit, in_scope);
                            if free_text && !matches!(verdict, PolicyVerdict::Deny { .. }) {
                                rec.tier = policy::TIER_APPROVE.to_string();
                                rec.outcome = "escalated".to_string();
                                rec.escalation = Some(decision::EscalationCard::new(
                                    rec.chosen.clone(),
                                    rec.ctx_refs.clone(),
                                    rec.ts_ms,
                                ));
                                verdict = PolicyVerdict::Approve {
                                    reason: "空转催促是自由文本：需人确认后才注入".to_string(),
                                };
                            }
                            let seq = persist_decision(&app, &rec);
                            rec.seq = seq;
                            brain.remember_decision(seq, &rec);
                            runtime.mark_decision();
                            note_checkpoint_decision(&core, c, seq);
                            push_timeline(&app, &rec);
                            // PR1：守护进程只观察。策略即便放行（例如 Pager 的 `q`）也不代按，
                            // 只出建议；步数只算交给人（出卡）的决策，纯观察不耗预算
                            let mut acted = false;
                            let denied = matches!(verdict, PolicyVerdict::Deny { .. });
                            if !denied && rec.escalation.is_none() {
                                events.push(sys_event(
                                    &c.console_id,
                                    &c.id,
                                    StewardEventKind::NoProgress,
                                    format!(
                                        "空转已识别，建议「{}」（{}）",
                                        rec.chosen.text, OBSERVER_ONLY_NOTE
                                    ),
                                    now,
                                ));
                            } else if !denied
                                && rec.escalation.is_some()
                                && may_write(&core, &c.console_id)
                            {
                                acted = true;
                                st.escalation = Some((seq, now));
                                core.ownership().note(
                                    &c.console_id,
                                    "escalation",
                                    format!("空转催促 #{seq} 待确认"),
                                    mono,
                                );
                                events.push(st.decision_event(
                                    StewardEventKind::Escalation,
                                    format!(
                                        "空转已识别，建议催一句「{}」（{}）",
                                        rec.chosen.text, OBSERVER_ONLY_NOTE
                                    ),
                                    Some(sit.clone()),
                                    Some(rec.clone()),
                                    now,
                                    0,
                                    String::new(),
                                ));
                            } else {
                                events.push(sys_event(
                                    &c.console_id,
                                    &c.id,
                                    StewardEventKind::NoProgress,
                                    format!(
                                        "空转已识别，但策略未放行推进（{}）；保留现场并通知",
                                        verdict.as_str()
                                    ),
                                    now,
                                ));
                            }
                            if acted {
                                core.bump_step(&c.id);
                            }
                        }
                    }
                }

                // ── 决策：只在需要应答、签名变化（或过冷却）时编一次 ────
                let Some(sit) = st.situation.clone() else {
                    continue;
                };
                if !should_decide(st, &sit, now) {
                    continue;
                }
                st.last_decision_signature = sit.signature.clone();
                st.last_decision_ms = now;

                let ctx = enrich_context(&app, build_context(c, &sit, &brain, now), &c.host_key);
                let mut rec = decision::compile(&ctx);
                let auth = core.authority(&c.id);
                let in_scope = host_in_scope(&app, &auth, &c.console_id, &c.host_key);
                let verdict = policy::authorize(&auth, &mut rec, &sit, in_scope);

                let seq = persist_decision(&app, &rec);
                rec.seq = seq;
                brain.remember_decision(seq, &rec);
                runtime.mark_decision();
                note_checkpoint_decision(&core, c, seq);
                push_timeline(&app, &rec);

                let (frame_id, frame_hash) = snap
                    .as_ref()
                    .map(|s| (s.frame_id, s.hash.clone()))
                    .unwrap_or((0, String::new()));
                events.push(st.decision_event(
                    StewardEventKind::DecisionMade,
                    format!("[{}] {}", verdict.as_str(), rec.rationale),
                    Some(sit.clone()),
                    Some(rec.clone()),
                    now,
                    frame_id,
                    frame_hash.clone(),
                ));

                if !may_write(&core, &c.console_id) {
                    // 用户敲键夺权后：继续观察与记录，但绝不写终端
                    core.ownership().note(
                        &c.console_id,
                        "decision",
                        format!("#{seq} 观察态决策：{}", rec.chosen.text),
                        mono,
                    );
                    continue;
                }
                // 步数只算出卡；纯观察 / 硬禁不消耗预算。
                // PR1：Auto / Notify 档的决策也只是建议（outcome 停在 proposed），守护进程不注入
                let denied = matches!(verdict, PolicyVerdict::Deny { .. });
                if denied {
                    events.push(sys_event(
                        &c.console_id,
                        &c.id,
                        StewardEventKind::PolicyDenied,
                        format!("决策 #{} 被 PolicyEngine 硬禁：{}", seq, rec.rationale),
                        now,
                    ));
                }
                // 升级卡与 verdict 解耦：Auto / Notify 档下编译器也可能因为低置信、自由文本
                // 或报错局面留卡；卡只是建议，挂上后照常走超时收敛
                if !denied && rec.escalation.is_some() {
                    core.bump_step(&c.id);
                    st.escalation = Some((seq, now));
                    core.ownership().note(
                        &c.console_id,
                        "escalation",
                        format!("决策 #{seq} 待确认"),
                        mono,
                    );
                    events.push(st.decision_event(
                        StewardEventKind::Escalation,
                        format!("建议卡：已给出建议与备选（{OBSERVER_ONLY_NOTE}）"),
                        Some(sit.clone()),
                        Some(rec.clone()),
                        now,
                        frame_id,
                        frame_hash,
                    ));
                }
            }

            // ── P4 周期 Checkpoint（每 30s 或每闭环）+ P5 成功轨迹蒸馏 ──
            for c in &contracts {
                let last = *last_cp.get(&c.id).unwrap_or(&0);
                if should_save(last, mono, false) {
                    let state = if core.ownership().state(&c.console_id)
                        == OwnershipState::Suspended
                    {
                        "suspended"
                    } else {
                        "observing"
                    };
                    if save_checkpoint(
                        &app,
                        &core,
                        c,
                        states.get(&c.console_id),
                        &screen,
                        now,
                        state,
                        "周期快照",
                    ) {
                        last_cp.insert(c.id.clone(), mono);
                        events.push(sys_event(
                            &c.console_id,
                            &c.id,
                            StewardEventKind::CheckpointSaved,
                            "现场已落盘：契约 / 意向栈 / 屏幕 hash / 最近决策 / 预算 / 计时器",
                            now,
                        ));
                    }
                }
            }
            if mono - last_distill_ms > 60_000 {
                last_distill_ms = mono;
                let app_state: State<'_, AppState> = app.state();
                let recs: Vec<DecisionRecord> = app_state
                    .store
                    .steward_decisions(80)
                    .into_iter()
                    .filter_map(|(_, _, raw)| serde_json::from_str(&raw).ok())
                    .collect();
                let existing = brain.view().items;
                for cand in learning::distill(&recs) {
                    // 已存在的不覆盖；用户删过的（墓碑）不复活
                    if existing.iter().any(|i| i.id == cand.id)
                        || app_state.store.steward_tombstoned(&cand.id)
                    {
                        continue;
                    }
                    if let Ok(item) = brain.upsert(cand.input.clone()) {
                        app_state.store.steward_ctx_upsert(&item.to_row()).ok();
                        events.push(sys_event(
                            "",
                            "",
                            StewardEventKind::Distilled,
                            format!(
                                "成功轨迹沉淀剧本「{}」（{} 次一致回执，默认停用，仅作用于 {}）",
                                cand.id,
                                cand.occurrences,
                                cand.input.scope.as_deref().unwrap_or("-")
                            ),
                            now,
                        ));
                    }
                }
            }

            publish(&app, &log, events);
            runtime.mark_tick(now, &states, &cfg);

            let delay = next_delay(&states, &contracts, &screen, now, &cfg);
            let _ = tokio::time::timeout(Duration::from_millis(delay), bus.0.notified()).await;
        }
    });
}

/// P5：把主机记忆 / 技能 / 历史 RunSummary 聚进 KB，再喂给决策编译器。
/// 所有新增条目都标「不可信数据」，只能当参考，不能改写 Profile / Authority。
///
/// 只聚合属于这台主机的数据：主机记忆按 host_key 取；技能过滤掉作用域不含本会话的；
/// 历史 RunSummary 是全局一张表、不记录在哪台主机跑的，聚进来就会把 A 主机的经验
/// 当成 B 主机的依据，所以不聚合。
fn enrich_context(app: &AppHandle, mut ctx: DecisionContext, host_key: &str) -> DecisionContext {
    let st: State<'_, AppState> = app.state();
    let notes = st.store.host_memory_list(host_key);
    let skills = host_scoped_skills(st.store.skill_list(None), &ctx.console_id);
    let sit = ctx.situation.clone();
    ctx.knowledge = learning::aggregate_kb(ctx.knowledge, &notes, &skills, &[], &sit, 8);
    ctx
}

// ───────────────────────────── Tauri 命令 ─────────────────────────────

#[tauri::command]
pub fn steward_events(
    since_seq: Option<u64>,
    limit: Option<usize>,
    log: State<'_, EventLog>,
) -> Vec<StewardEvent> {
    log.since(since_seq.unwrap_or(0), limit.unwrap_or(60))
}

#[tauri::command]
pub fn steward_runtime_info(
    runtime: State<'_, StewardRuntime>,
    log: State<'_, EventLog>,
    config: State<'_, StewardConfigStore>,
    brain: State<'_, StewardBrain>,
) -> StewardRuntimeInfo {
    runtime.info(
        log.seq(),
        log.count(),
        config.get(),
        brain.execution_enabled(),
    )
}

/// 面板「立即分类」：不写事件流，只当场返回一帧的局面，便于人工核对判据。
#[tauri::command]
pub fn steward_classify_now(
    console_id: String,
    screen: State<'_, ScreenStore>,
    config: State<'_, StewardConfigStore>,
) -> Option<Situation> {
    screen
        .get(&console_id)
        .map(|s| situation::classify(&s, now_ms(), config.get().stale_ms))
}

#[tauri::command]
pub fn steward_clear_events(log: State<'_, EventLog>) {
    log.clear();
}

#[tauri::command]
pub fn steward_config(config: State<'_, StewardConfigStore>) -> StewardConfig {
    config.get()
}

#[tauri::command]
pub fn steward_set_config(
    no_progress_ticks: Option<u32>,
    stale_ms: Option<i64>,
    config: State<'_, StewardConfigStore>,
    bus: State<'_, StewardBus>,
) -> StewardConfig {
    let c = config.update(no_progress_ticks, stale_ms);
    bus.wake();
    c
}

#[tauri::command]
pub fn steward_wake(bus: State<'_, StewardBus>) -> bool {
    bus.wake();
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::steward::{frame_hash, ScreenLine};

    fn delta(lines: &[&str], frame_id: u64) -> crate::agent::steward::ScreenDelta {
        let v: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
        crate::agent::steward::ScreenDelta {
            console_id: "tab-1".to_string(),
            frame_id,
            window_start: 0,
            total: v.len() as u32,
            cols: 80,
            rows: 24,
            alt: false,
            cursor_x: 0,
            cursor_y: 0,
            hash: frame_hash(&v, false, 80, 24, 0, 0),
            full: true,
            lines: v
                .iter()
                .enumerate()
                .map(|(i, s)| ScreenLine {
                    y: i as u32,
                    text: s.clone(),
                })
                .collect(),
            ts_ms: 1_000,
        }
    }

    fn store(lines: &[&str], frame_id: u64, ts: i64) -> (ScreenStore, ScreenSnapshot) {
        let s = ScreenStore::new();
        let mut d = delta(lines, frame_id);
        d.ts_ms = ts;
        s.apply(&d);
        let snap = s.get("tab-1").unwrap();
        (s, snap)
    }

    fn cfg() -> StewardConfig {
        StewardConfig::default()
    }

    #[test]
    fn first_classify_emits_situation_and_suggestion_card() {
        let (_, snap) = store(&["Do you want to proceed?", "1. Yes", "2. No"], 1, 1_000);
        let mut st = StewardState::new("tab-1".to_string(), "c-1".to_string());
        let evs = st.step(Some(&snap), 1_100, &cfg());
        assert!(evs
            .iter()
            .any(|e| e.kind == StewardEventKind::SituationChanged));
        let card = evs
            .iter()
            .find(|e| e.kind == StewardEventKind::SituationChanged)
            .unwrap();
        assert!(card.suggestion.is_some(), "授权确认应立即出建议卡");
        assert_eq!(card.llm_calls, 0);
        assert!(card.situation.is_some());
    }

    #[test]
    fn identical_situation_counts_ticks_then_raises_no_progress_once() {
        let (_, snap) = store(&["⠋ Compiling 42%", "esc to interrupt"], 1, 1_000);
        let mut st = StewardState::new("tab-1".to_string(), "c-1".to_string());
        st.step(Some(&snap), 1_100, &cfg());
        let mut raised = 0;
        for i in 0..6 {
            let evs = st.step(Some(&snap), 2_000 + i * 1_000, &cfg());
            raised += evs
                .iter()
                .filter(|e| e.kind == StewardEventKind::NoProgress)
                .count();
        }
        assert_eq!(raised, 1, "同一局面只升级一次，不每个心跳刷屏");
        assert_eq!(st.same_ticks, 6);
        assert!(st.no_progress_raised);
    }

    #[test]
    fn signature_change_resets_no_progress_and_heartbeat() {
        let (_, a) = store(&["⠋ Compiling 42%", "esc to interrupt"], 1, 1_000);
        let (_, b) = store(&["$ done", "chris@mac ~ %"], 2, 3_000);
        let mut st = StewardState::new("tab-1".to_string(), "c-1".to_string());
        st.step(Some(&a), 1_100, &cfg());
        st.step(Some(&a), 6_100, &cfg());
        st.step(Some(&a), 21_100, &cfg());
        assert_eq!(st.same_ticks, 2);
        // Progress 走收紧档：1s → 2s → 5s；静默档此时才是 15s（下一个用例覆盖）
        assert_eq!(st.heartbeat_ms(&cfg()), 5_000);
        let evs = st.step(Some(&b), 22_000, &cfg());
        assert!(evs
            .iter()
            .any(|e| e.kind == StewardEventKind::SituationChanged));
        assert_eq!(st.same_ticks, 0);
        assert!(!st.no_progress_raised);
        assert_eq!(st.heartbeat_ms(&cfg()), 1_000, "局面变化后心跳重置");
    }

    #[test]
    fn progress_uses_tighter_heartbeat_than_silent() {
        let (_, a) = store(&["⠋ Compiling 42%", "esc to interrupt"], 1, 1_000);
        let (_, b) = store(&["$ done", "chris@mac ~ %"], 2, 1_000);
        let mut ps = StewardState::new("tab-1".to_string(), "c-1".to_string());
        ps.step(Some(&a), 1_100, &cfg());
        ps.same_ticks = 2;
        let mut is = StewardState::new("tab-1".to_string(), "c-1".to_string());
        is.step(Some(&b), 1_100, &cfg());
        is.same_ticks = 2;
        assert!(ps.heartbeat_ms(&cfg()) < is.heartbeat_ms(&cfg()));
        assert_eq!(ps.heartbeat_ms(&cfg()), 5_000);
        assert_eq!(is.heartbeat_ms(&cfg()), 15_000);
    }

    #[test]
    fn heartbeat_frames_do_not_force_extra_classification() {
        let (_, snap) = store(&["$ done", "chris@mac ~ %"], 1, 1_000);
        let mut st = StewardState::new("tab-1".to_string(), "c-1".to_string());
        st.step(Some(&snap), 1_100, &cfg());
        // 前端每 2s 补一帧同哈希心跳，不该让后端每个心跳都分类
        assert!(!st.should_classify(Some(&snap), 2_000, &cfg()));
        assert!(st.should_classify(Some(&snap), 7_000, &cfg()));
    }

    #[test]
    fn changed_frame_classifies_after_debounce() {
        let (_, a) = store(&["$ pwd"], 1, 1_000);
        let (_, b) = store(&["$ pwd", "/tmp"], 2, 1_000);
        let mut st = StewardState::new("tab-1".to_string(), "c-1".to_string());
        st.step(Some(&a), 1_100, &cfg());
        assert!(!st.should_classify(Some(&b), 1_200, &cfg()));
        assert!(st.should_classify(Some(&b), 1_400, &cfg()));
    }

    #[test]
    fn stale_feed_and_restore_are_events_not_silent_degradation() {
        let (_, snap) = store(&["Do you want to proceed?", "1. Yes"], 1, 1_000);
        let mut st = StewardState::new("tab-1".to_string(), "c-1".to_string());
        let evs = st.step(Some(&snap), 20_100, &cfg());
        assert!(evs.iter().any(|e| e.kind == StewardEventKind::StaleFeed));
        let (_, fresh) = store(&["Do you want to proceed?", "1. Yes"], 2, 20_200);
        let evs = st.step(Some(&fresh), 20_300, &cfg());
        assert!(evs
            .iter()
            .any(|e| e.kind == StewardEventKind::FeedRestored));
    }

    #[test]
    fn missing_screen_reports_screen_lost_once() {
        let mut st = StewardState::new("tab-1".to_string(), "c-1".to_string());
        let a = st.step(None, 1_000, &cfg());
        let b = st.step(None, 3_000, &cfg());
        assert_eq!(
            a.iter()
                .filter(|e| e.kind == StewardEventKind::ScreenLost)
                .count(),
            1
        );
        assert!(b.is_empty());
    }

    #[test]
    fn event_log_assigns_monotonic_seq_and_caps_buffer() {
        let log = EventLog::new();
        let mut bulk = Vec::new();
        for _ in 0..210 {
            bulk.push(StewardEvent {
                seq: 0,
                ts_ms: 1,
                console_id: "t".to_string(),
                contract_id: "c".to_string(),
                kind: StewardEventKind::NoProgress,
                message: "x".to_string(),
                suggestion: None,
                situation: None,
                frame_id: 1,
                screen_hash: "h".to_string(),
                llm_calls: 0,
                decision: None,
            });
        }
        let out = log.append(bulk);
        assert_eq!(out.first().unwrap().seq, 1);
        assert_eq!(out.last().unwrap().seq, 210);
        assert_eq!(log.count(), MAX_EVENTS);
        assert_eq!(log.since(205, 60).len(), 5);
        log.clear();
        assert_eq!(log.count(), 0);
        assert_eq!(log.seq(), 210, "清空不清游标，前端增量拉取不会倒退");
    }

    #[test]
    fn config_update_clamps_and_wakes() {
        let c = StewardConfigStore::new();
        let bus = StewardBus::default();
        let x = c.update(Some(99), Some(10));
        assert_eq!(x.no_progress_ticks, 20);
        assert_eq!(x.stale_ms, 1_000);
        bus.wake();
        assert_eq!(c.get().no_progress_ticks, 20);
    }

    // ─────────────────── P2 自动应答的安全门控 ───────────────────

    fn test_situation(kind: SituationKind, signature: &str) -> Situation {
        Situation {
            frame_id: 1,
            screen_hash: "h1".into(),
            program: super::super::situation::Program::ClaudeCode,
            kind,
            topic: Some("settings:remember-check".into()),
            options: vec![],
            confidence: 0.9,
            tail: "问一句".into(),
            evidence: vec![],
            stale_ms: 0,
            signature: signature.into(),
        }
    }

    #[test]
    fn should_decide_dedupes_same_signature_and_waits_for_receipt() {
        let mut st = StewardState::new("tab-1".into(), "c-1".into());
        let q = test_situation(SituationKind::Question, "claude_code|question|A");
        assert!(should_decide(&st, &q, 1_000));
        st.last_decision_signature = q.signature.clone();
        st.last_decision_ms = 1_000;
        assert!(
            !should_decide(&st, &q, 2_000),
            "同一签名的回显抖动不能重复注入"
        );
        assert!(
            should_decide(&st, &q, 1_000 + DECISION_COOLDOWN_MS + 1),
            "冷却过后同一个提问重新出现，可以再答一次"
        );
        let idle = test_situation(SituationKind::Idle, "claude_code|idle|");
        assert!(!should_decide(&st, &idle, 99_999), "空闲局面不编决策");
        st.pending = Some(PendingReceipt {
            decision_seq: 1,
            console_id: "tab-1".into(),
            contract_id: "c-1".into(),
            before_frame: 1,
            before_hash: "h1".into(),
            echo: "yes".into(),
            injected_at: 1_000,
            deadline_ms: 2_500,
        });
        let q2 = test_situation(SituationKind::Question, "claude_code|question|B");
        assert!(
            !should_decide(&st, &q2, 99_999),
            "上一条还没回执，不能连着发第二条"
        );
    }

    #[test]
    fn execution_switch_off_means_zero_injection_even_for_a_good_decision() {
        // 一条完全可注入的决策（Auto 档、Question、有依据），在总闸关闭时也不许写
        let ctx = super::super::decision::DecisionContext {
            situation: test_situation(SituationKind::Question, "claude_code|question|A"),
            contract_id: "c-1".into(),
            console_id: "tab-1".into(),
            host_key: "local:tab-1".into(),
            authority_tier: "auto".into(),
            intent: None,
            hard: vec![],
            soft: vec![],
            knowledge: vec![],
            playbook: None,
            recent: vec![],
            execution_enabled: false,
            now_ms: 1_000,
        };
        let rec = decision::compile(&ctx);
        assert!(!decision::can_execute(&rec, false, "auto"));
        assert!(!decision::can_execute(&rec, true, "approve"));
        // 没有意向时决策本身也 fail-closed（不会凭空编回复）
        assert_eq!(rec.chosen.kind, "observe");

        // 换成有 Playbook 的确定性回复：总闸关 = 0 次注入，总闸开 = 可以注入
        let mut ctx2 = ctx;
        ctx2.playbook = Some((
            super::super::profile::CtxItem {
                id: "pb".into(),
                kind: "playbook".into(),
                text: "测试剧本".into(),
                strength: "soft".into(),
                scope: "global".into(),
                source: "user".into(),
                evidence: String::new(),
                extra: serde_json::json!({}),
                contract_id: String::new(),
                position: 0,
                created_at: 0,
                updated_at: 0,
            },
            super::super::profile::PlaybookSpec {
                answer: "yes".into(),
                tier: Some("auto".into()),
                enabled: true,
                ..Default::default()
            },
        ));
        let rec2 = decision::compile(&ctx2);
        assert_eq!(rec2.chosen.text, "yes");
        assert!(!decision::can_execute(&rec2, false, "auto"));
        assert!(decision::can_execute(&rec2, true, "auto"));
        assert!(!decision::can_execute(&rec2, true, "approve"));
    }

    #[test]
    fn receipt_timeout_never_resends() {
        let p = PendingReceipt {
            decision_seq: 9,
            console_id: "tab-1".into(),
            contract_id: "c-1".into(),
            before_frame: 4,
            before_hash: "old".into(),
            echo: "yes".into(),
            injected_at: 1_000,
            deadline_ms: 2_500,
        };
        let (state, msg) = decision::verify_receipt(&p, None, 2_600);
        assert_eq!(state, ReceiptState::Timeout);
        assert!(msg.contains("不重发"));
        // 超时状态下即使后来来了一帧，也不该变成「再发一次」：循环里 pending 已被清掉，
        // 这里只断言校验结果本身不会给出 retry 语义。
        assert_eq!(ReceiptState::Timeout, state);
    }

    #[test]
    fn no_progress_never_nudges_at_a_shell_prompt() {
        let mut q = test_situation(SituationKind::AwaitingInput, "shell|awaiting|A");
        assert!(no_progress_action(&q).is_some());
        q.evidence.push(situation::SHELL_PROMPT_VISIBLE.to_string());
        assert!(no_progress_action(&q).is_none(), "shell 下催促会被当命令执行");
    }

    #[test]
    fn host_scoped_skills_drop_other_hosts() {
        let mk = |name: &str, scope: serde_json::Value| -> super::super::skill::Skill {
            serde_json::from_value(serde_json::json!({
                "name": name, "steps": [], "hostScope": scope
            }))
            .unwrap()
        };
        let skills = vec![
            mk("any", serde_json::json!({"kind": "any"})),
            mk("mine", serde_json::json!({"kind": "list", "hosts": ["TAB-1"]})),
            mk("other", serde_json::json!({"kind": "list", "hosts": ["tab-2", "*"]})),
        ];
        let kept: Vec<String> = host_scoped_skills(skills, "tab-1")
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(kept, vec!["any".to_string(), "mine".to_string()]);
    }

    // ── PR1 止血 ──

    #[test]
    fn daemon_has_no_write_path_at_all() {
        // 守护进程只观察：源码里不允许再出现任何写 PTY / SSH 的调用。
        // 关键字拆开拼，免得这条测试自己命中自己。
        let src = include_str!("steward_loop.rs");
        for needle in [
            concat!("sessions", ".write("),
            concat!("local", ".write("),
            concat!("inject", "_keys("),
        ] {
            assert!(!src.contains(needle), "steward_loop 不应再有写路径：{needle}");
        }
    }

    #[test]
    fn llm_run_holding_or_pausing_console_blocks_daemon() {
        use super::super::control::ControlTable;
        let t = ControlTable::new();
        assert!(!control_owned(&t, "tab-1"), "没人驾驶时守护进程可出建议");
        t.acquire("tab-1", "run-1");
        assert!(control_owned(&t, "tab-1"));
        // 托管运行暂停（hold_paused 的 steward 分支）：租约仍在 → 守护进程仍让路
        t.pause_run("run-1");
        assert!(control_owned(&t, "tab-1"));
        // 用户点「交还」：状态回到 AgentDriving，持有者仍在
        t.handback("tab-1");
        assert!(control_owned(&t, "tab-1"));
        t.release_run("run-1");
        assert!(!control_owned(&t, "tab-1"), "运行结束才还给守护进程观察");
    }

    #[test]
    fn resume_entry_for_missing_console_expires() {
        assert_eq!(resume_wait(true, RESUME_TTL_MS * 10), ResumeWait::Ready);
        assert_eq!(resume_wait(false, 0), ResumeWait::Wait);
        assert_eq!(resume_wait(false, RESUME_TTL_MS - 1), ResumeWait::Wait);
        assert_eq!(resume_wait(false, RESUME_TTL_MS), ResumeWait::Expire);
        // 与 StewardCore 的计时联动：重新排队不刷新起点，过了 TTL 就判过期
        let core = StewardCore::new();
        core.push_resume("c-ghost");
        for id in core.drain_resume() {
            core.push_resume(&id); // 循环里「没首帧 → 下一 tick 再看」
        }
        core.advance_clock(RESUME_TTL_MS + 1);
        core.push_resume("c-ghost");
        assert_eq!(
            resume_wait(false, core.resume_waited_ms("c-ghost")),
            ResumeWait::Expire
        );
        core.drop_contract("c-ghost", "tab-ghost");
        assert!(core.drain_resume().is_empty());
        assert_eq!(core.resume_waited_ms("c-ghost"), 0);
    }
}
