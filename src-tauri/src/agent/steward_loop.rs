//! 常驻 Steward 观察器：事件驱动 + 自适应心跳 + 局面分类 + 空转 / 断流检测。
//!
//! 设计见 `docs/nexus-host-unified-design.md` §1「守护进程只观察」。
//!
//! ## 这一层做什么
//! 屏幕帧由前端持续上报进 `ScreenStore`。这里起一个**常驻** task：契约签下后，即使面板关掉、
//! 窗口最小化，后端仍按自己的心跳读屏、分类、判断空转与断流，并把局面变化写成可回放的事件流
//! （`EventLog` + `nx://steward/event`）。
//!
//! ## 只观察
//! 统一托管之后**唯一的写者是托管任务**（`act::run` 的 Hosted 模式，见 `agent/host`）。
//! 本循环没有任何写 PTY / SSH 的路径，也不编决策、不出升级卡、不做撤销 / 回执 / 恢复：
//! 那一整套零 LLM 的「决策 → 注入 → 回执」闭环已在 PR8 删除，反射快路径搬进了托管任务内部
//! （`host::reflex`），与模型共用同一份写租约。
//!
//! ## 为什么是「单 task + 每 console 一个状态槽」
//! 一个常驻 Supervisor 管理多个状态槽，语义等价于每 console 一个 actor，
//! 且不会因契约反复签 / 停而泄漏 task。
//!
//! ## 前端心跳帧
//! 终端静默时 `termRegistry` 每 2s 补一帧同哈希帧，于是 `idleFrames` 按真实时间增长、
//! 断流也能被 `stale_ms` 判出来。

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use tokio::sync::Notify;

use super::situation::{self, Situation, SituationKind};
use super::steward::{now_ms, ScreenSnapshot, ScreenStore, StewardRegistry};

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
        }
    }
}

/// 一条可回放的观察事件。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StewardEvent {
    pub seq: u64,
    pub ts_ms: i64,
    pub console_id: String,
    pub contract_id: String,
    pub kind: StewardEventKind,
    pub message: String,
    /// 建议卡文本：只建议、不执行
    pub suggestion: Option<String>,
    pub situation: Option<Situation>,
    pub frame_id: u64,
    pub screen_hash: String,
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

/// 每个被托管 console 的循环状态。纯数据 + 纯函数，方便单测。
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
        }
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
    }

    pub fn info(&self, event_seq: u64, event_count: usize, config: StewardConfig) -> StewardRuntimeInfo {
        let r = self.0.lock();
        StewardRuntimeInfo {
            running: r.running,
            tick_count: r.ticks,
            classify_count: r.classifies,
            last_tick_ms: r.last_tick_ms,
            last_classify_ms: r.last_classify_ms,
            event_seq,
            event_count,
            config,
            consoles: r.consoles.clone(),
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
    pub event_seq: u64,
    pub event_count: usize,
    pub config: StewardConfig,
    pub consoles: Vec<StewardStateInfo>,
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

fn publish(app: &AppHandle, log: &EventLog, events: Vec<StewardEvent>) {
    let stored = log.append(events);
    for e in &stored {
        let _ = app.emit("nx://steward/event", e.clone());
    }
}

/// 应用启动时调一次。返回后循环常驻到 app 退出。
///
/// 每个 tick：对齐契约表（签下 / 停止各记一条）→ 按自适应心跳分类 → 发事件 → 睡到最近的 due。
/// 全程只读、零 LLM。
pub fn spawn_daemon(
    app: AppHandle,
    screen: ScreenStore,
    registry: StewardRegistry,
    log: EventLog,
    bus: StewardBus,
    runtime: StewardRuntime,
    config: StewardConfigStore,
) {
    tauri::async_runtime::spawn(async move {
        runtime.set_running(true);
        let mut states: HashMap<String, StewardState> = HashMap::new();
        loop {
            let now = now_ms();
            let cfg = config.get();
            let contracts = registry.all();

            // 墙钟跳变保护：唤醒瞬间不要把每个 console 都判成「连续 3 次空转」
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
                    let st = StewardState::new(c.console_id.clone(), c.id.clone());
                    events.push(st.event(
                        StewardEventKind::ContractStarted,
                        format!("开始观察：目标「{}」", c.goal),
                        None,
                        None,
                        now,
                        0,
                        String::new(),
                    ));
                    states.insert(c.console_id.clone(), st);
                }
            }

            // ── 观察：分类 / 空转 / 断流 ──
            for c in &contracts {
                let snap = screen.get(&c.console_id);
                let Some(st) = states.get_mut(&c.console_id) else {
                    continue;
                };
                if st.should_classify(snap.as_ref(), now, &cfg) {
                    let produced = st.step(snap.as_ref(), now, &cfg);
                    if !produced.is_empty() {
                        runtime.mark_classify(now);
                    }
                    events.extend(produced);
                }
            }

            publish(&app, &log, events);
            runtime.mark_tick(now, &states, &cfg);

            let delay = next_delay(&states, &contracts, &screen, now, &cfg);
            let _ = tokio::time::timeout(Duration::from_millis(delay), bus.0.notified()).await;
        }
    });
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
) -> StewardRuntimeInfo {
    runtime.info(log.seq(), log.count(), config.get())
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
            cursor_y: 0, app_cursor: false,
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

    #[test]
    fn daemon_has_no_write_path_at_all() {
        // 守护进程只观察：源码里不允许再出现任何写 PTY / SSH 的调用。
        // 关键字拆开拼，免得这条测试自己命中自己。
        let src = include_str!("steward_loop.rs");
        for needle in [
            concat!("sessions", ".write("),
            concat!("local", ".write("),
            concat!("inject", "_keys("),
            concat!("persist", "_decision("),
        ] {
            assert!(!src.contains(needle), "steward_loop 不应再有写路径：{needle}");
        }
    }
}
