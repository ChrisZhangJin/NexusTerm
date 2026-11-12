//! 托管任务的控制流（设计 §2）：收件箱 + 相位 + 副作用执行，**不碰 Tauri**。
//!
//! `act::run(RunMode::Hosted)` 在三类地方进到这里：
//! - 写 pty 之前（`gate`）：收件箱里攒的事件先处理掉；用户在驾驶 / 暂停 / 等人时**无限期**挂起，
//!   只有取消能打断——没有 `PAUSE_MAX_WAIT`，也没有「接管次数满了就收场」；
//! - 要问人（`ask` / `approve`）：进 WaitingHuman，挂起期间零 LLM 调用，等 `host_answer`；
//! - 循环顶（`drain` + `park`）：预算用尽 / 模型连不上 → Paused，只有 `host_resume` 能离开。
//!
//! 相位迁移一律经 `phase::step`，副作用（落 checkpoint、推事件、放租约、控制权同步）交给 `HostIo`，
//! 这样整条控制流可以用假 IO + tokio 暂停时钟单测。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::mpsc;

use super::super::checkpoint::Spent;
use super::super::decision::CtxRef;
use super::super::policy::Authority;
use super::super::steward::StewardContract;
use super::gate::{HardRule, HostPolicy};
use super::phase::{step, Effect, HostEvent, HostPhase, Invalid};
use super::{BudgetGrant, CardKind, Check, HostHandle, HumanCard, SuspendReason};

/// 挂起等人期间多久看一眼 console 还在不在。
pub const LIVENESS_POLL: Duration = Duration::from_secs(3);

/// 挂起期间多久看一眼取消标记。不是超时：等多久都行，只是取消要及时生效。
pub const CANCEL_POLL: Duration = Duration::from_millis(200);

/// 副作用的落地方。真实实现在 `host_cmd.rs`（SQLite + Tauri 事件 + ControlTable）。
pub trait HostIo: Send + Sync {
    /// 落一份 checkpoint（`host_sessions` 一行）
    fn save(&self, phase: &HostPhase);
    /// 推 `nx://host/{contract_id}`
    fn emit(&self, phase: &HostPhase);
    /// 进入终态：释放 ControlTable 租约
    fn release_lease(&self);
    /// 按相位同步 ControlTable：驾驶 / 暂停 / 挂起标 Paused（租约不放），运行 / 等人标 AgentDriving
    fn sync_control(&self, phase: &HostPhase);
    /// 记下运行进度（对话历史、步数、契约级花费），下一次 `save` 一并落盘。默认不记。
    fn note_progress(&self, _messages: &[super::super::llm::ChatMessage], _step: usize, _spent: &Spent) {}
    /// 契约 / 授权变了（签约、`host_amend`、判据确认），下一次 `save` 一并落盘。默认不记。
    fn note_contract(&self, _contract: &StewardContract, _authority: &Authority) {}
    /// 计划清单变了：记进快照并推一次状态（面板的托管卡显示它）。默认不记。
    fn note_todo(&self, _todo: &[super::super::todo::TodoItem], _phase: &HostPhase) {}
    /// 看门狗状态变了，下一次 `save` 一并落盘。默认不记。
    fn note_watchdog(&self, _w: &super::watchdog::WatchdogState) {}
    /// 给人看的提示变了（随状态推给面板）。默认不记。
    fn note_notice(&self, _notice: Option<&str>) {}
    /// 期限暂停后用户「继续」过了（随 checkpoint 落盘，接驳后不再因同一期限暂停）。默认不记。
    fn note_deadline_ack(&self, _acked: bool) {}
    /// 被托管的 console 还在不在（tab 没关、连接没断）。挂起等人期间据此发现「tab 关了」。默认在。
    fn console_alive(&self) -> bool {
        true
    }
    /// 待落盘的现场（接驳时从它恢复）。默认没有。
    fn snapshot(&self) -> Option<super::checkpoint::HostCheckpoint> {
        None
    }
}

/// 托管任务眼里的契约：强制用的授权与 Hard 规则、系统提示里的 `## 托管契约` 一节及其依据。
#[derive(Debug, Clone)]
pub struct HostCtx {
    pub contract: StewardContract,
    pub authority: Authority,
    pub hard: Vec<HardRule>,
    /// `HostContext::build` 渲染的一节（接在系统提示后面）
    pub section: String,
    /// 这一节引用的依据，写进 DecisionRecord.ctx_refs
    pub refs: Vec<CtxRef>,
    /// 被托管终端的可读名字（主机备忘按它找主机）
    pub label: String,
}

impl HostCtx {
    /// 还没取到 brain / 授权时的最小契约（测试与签约前的占位）。
    pub fn bare(contract_id: &str, console_id: &str, goal: &str) -> Self {
        Self {
            contract: StewardContract {
                id: contract_id.to_string(),
                console_id: console_id.to_string(),
                host_key: format!("local:{console_id}"),
                goal: goal.to_string(),
                authority_tier: Authority::default().tier,
                status: "observing".to_string(),
                intent_ids: Vec::new(),
                timebox_ms: 0,
                created_at: 0,
                success_criteria: Vec::new(),
                yolo: false,
                criteria_confirmed: false,
            },
            authority: Authority::default(),
            hard: Vec::new(),
            section: String::new(),
            refs: Vec::new(),
            label: "当前会话".to_string(),
        }
    }

    /// `gate::authorize` 的输入。
    pub fn policy(&self) -> HostPolicy {
        HostPolicy {
            authority: self.authority.clone(),
            hard: self.hard.clone(),
            host_key: self.contract.host_key.clone(),
        }
    }
}

/// 迁移副作用里要交回运行循环的那部分。
#[derive(Debug, Default)]
struct Pending {
    /// `Effect::AppendUser`：以用户消息追加进对话
    user: Vec<String>,
    /// `Effect::Feedback`：并进下一步的工具反馈
    feedback: Vec<String>,
    /// 审批卡 id → 结论
    approvals: HashMap<String, bool>,
    /// 「加预算继续」给的新上限
    grant: Option<BudgetGrant>,
    /// 收到过 Resume：运行循环要换一份新预算
    resumed: bool,
    /// 接驳后第一步前强制读屏（`Effect::ForceScreen`）
    force_screen: bool,
}

/// 接驳 / 重启后接着跑的现场：对话历史与步数（运行循环开头取走）。
#[derive(Debug, Clone, Default)]
pub struct Seed {
    pub messages: Vec<super::super::llm::ChatMessage>,
    pub step: usize,
}

/// 运行进度多久落一次盘（Running 期间的周期快照；相位迁移另外即时落）。
pub const PROGRESS_TICK: Duration = Duration::from_secs(30);

/// 挂起结束的方式。
#[derive(Debug, Clone, PartialEq)]
pub enum ParkEnd {
    /// 回到 Running，可以继续
    Running,
    /// 取消标记置位（用户点了停止）
    Cancelled,
    /// 进了终态（Stopped / Failed / Completed）
    Ended(HostPhase),
}

/// 一个托管任务的共享状态：命令层（`host_*`）与运行循环各持一个 `Arc`。
pub struct HostShared {
    pub handle: HostHandle,
    pub console_id: String,
    pub goal: String,
    pub cancel: Arc<AtomicBool>,
    tx: mpsc::UnboundedSender<HostEvent>,
    rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<HostEvent>>,
    phase: Mutex<HostPhase>,
    pending: Mutex<Pending>,
    io: Box<dyn HostIo>,
    /// 契约 + 渲染好的上下文；`version` 每次 `set_ctx` 加一，运行循环据此重建系统提示
    ctx: Mutex<(HostCtx, u64)>,
    /// `host_amend` 留给模型的一句「契约已更新：…」
    amend_notes: Mutex<Vec<String>>,
    /// 契约级花费（随 checkpoint 持久化，暂停 / 继续不清零）
    spent: Mutex<Spent>,
    /// 接驳时被新任务顶替：旧任务退出时不收尾（不停契约、不放租约）
    superseded: AtomicBool,
    /// 运行任务已经彻底退出（`act::run` 的各个守卫都已 drop）
    exited: AtomicBool,
    /// 接驳 / 重启后接着跑的现场
    seed: Mutex<Option<Seed>>,
    /// 给人看的一句提示（例如「接驳时屏幕和重启前不一样，已降为确认档」），随状态推给面板
    notice: Mutex<Option<String>>,
    /// 进入终态的时刻（登记表按它清理旧记录）
    ended_at: Mutex<Option<std::time::Instant>>,
    /// 上一次周期快照的时刻与对话指纹（没变就不再克隆整段历史）
    progress: Mutex<(Option<std::time::Instant>, (usize, usize, usize))>,
    /// 看门狗（随 checkpoint 持久化）
    watchdog: Mutex<super::watchdog::WatchdogState>,
    /// 意向期限到了、用户已经点过「继续」：不再因为同一个期限反复暂停
    deadline_acked: AtomicBool,
    /// 计划清单（nx_todo，随 checkpoint 持久化）
    todo: Mutex<Vec<super::super::todo::TodoItem>>,
}

/// 进程内单调时钟（毫秒）。看门狗只认它；重启后基准变了，恢复时要 `rebase`。
pub fn mono_ms() -> i64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_millis() as i64
}

impl HostShared {
    pub fn new(
        handle: HostHandle,
        console_id: &str,
        goal: &str,
        cancel: Arc<AtomicBool>,
        io: Box<dyn HostIo>,
    ) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let handle_cid = handle.contract_id.clone();
        Arc::new(Self {
            handle,
            console_id: console_id.to_string(),
            goal: goal.to_string(),
            cancel,
            tx,
            rx: tokio::sync::Mutex::new(rx),
            phase: Mutex::new(HostPhase::Running),
            pending: Mutex::new(Pending::default()),
            io,
            ctx: Mutex::new((HostCtx::bare(&handle_cid, console_id, goal), 0)),
            amend_notes: Mutex::new(Vec::new()),
            spent: Mutex::new(Spent::default()),
            superseded: AtomicBool::new(false),
            exited: AtomicBool::new(false),
            seed: Mutex::new(None),
            notice: Mutex::new(None),
            ended_at: Mutex::new(None),
            progress: Mutex::new((None, (0, 0, 0))),
            watchdog: Mutex::new(super::watchdog::WatchdogState::default()),
            deadline_acked: AtomicBool::new(false),
            todo: Mutex::new(Vec::new()),
        })
    }

    // ——— 计划清单 ———

    pub fn todo(&self) -> Vec<super::super::todo::TodoItem> {
        self.todo.lock().clone()
    }

    /// 换上新的计划清单（整表替换）：记进快照、推一次状态。
    pub fn set_todo(&self, items: Vec<super::super::todo::TodoItem>) {
        if !self.is_superseded() {
            self.io.note_todo(&items, &self.phase());
        }
        *self.todo.lock() = items;
    }

    // ——— 期限 ———

    /// 契约的意向期限（墙钟毫秒）；没设为 None。`timebox_ms` 是从签约起算的时长。
    pub fn deadline_ms(&self) -> Option<i64> {
        let c = self.ctx();
        let span = if c.contract.timebox_ms > 0 { c.contract.timebox_ms } else { c.authority.timebox_ms };
        (span > 0 && c.contract.created_at > 0).then(|| c.contract.created_at + span as i64)
    }

    /// 从 checkpoint 恢复「期限已确认」。
    pub fn set_deadline_acked(&self, acked: bool) {
        self.deadline_acked.store(acked, Ordering::Relaxed);
        self.io.note_deadline_ack(acked);
    }

    pub fn deadline_acked(&self) -> bool {
        self.deadline_acked.load(Ordering::Relaxed)
    }

    /// 期限到了且还没被用户「继续」确认过：进 Paused{Deadline}。返回是否刚暂停。
    pub fn check_deadline(&self, now_wall_ms: i64) -> bool {
        if self.deadline_acked.load(Ordering::Relaxed) || self.phase() != HostPhase::Running {
            return false;
        }
        match self.deadline_ms() {
            Some(d) if now_wall_ms >= d => self.apply(HostEvent::DeadlineReached).is_ok(),
            _ => false,
        }
    }

    // ——— 看门狗 ———

    /// 从 checkpoint 恢复看门狗：单调时钟换了基准，先 `rebase`（停机时间不算没进展）。
    pub fn restore_watchdog(&self, mut w: super::watchdog::WatchdogState) {
        w.rebase(mono_ms());
        self.io.note_watchdog(&w);
        *self.watchdog.lock() = w;
    }

    pub fn watchdog(&self) -> super::watchdog::WatchdogState {
        self.watchdog.lock().clone()
    }

    /// 喂一步观察，返回触发的警报与级别（L1 提示 / L2 出卡）。
    pub fn watch_step(&self, o: super::watchdog::StepObs) -> Option<(super::watchdog::Alarm, super::watchdog::Level)> {
        let mut w = self.watchdog.lock();
        let r = w.step(o, mono_ms());
        self.io.note_watchdog(&w);
        r
    }

    // ——— 重启 / 接驳 ———

    /// 直接设定相位（启动扫描恢复的挂起任务、接驳后回到的状态），并执行接驳带来的副作用
    /// （追加「先读屏」的话、强制读屏）。不落盘、不推事件：调用方随后 `announce`。
    pub fn restore(&self, phase: HostPhase, effects: Vec<Effect>) {
        if phase.is_terminal() {
            *self.ended_at.lock() = Some(std::time::Instant::now());
        }
        *self.phase.lock() = phase.clone();
        for e in effects {
            match e {
                Effect::SaveCheckpoint | Effect::EmitPhase | Effect::ReleaseLease => {}
                other => self.run_effect(other, &phase),
            }
        }
    }

    /// 当前现场：快照 + 内存里最新的相位 / 契约 / 授权 / 花费。
    pub fn checkpoint(&self) -> Option<super::checkpoint::HostCheckpoint> {
        let mut c = self.io.snapshot()?;
        let ctx = self.ctx();
        c.phase = self.phase();
        c.contract = ctx.contract;
        c.authority = ctx.authority;
        c.spent = self.spent();
        Some(c)
    }

    /// 重启前被托管的那个 console 的描述。
    pub fn saved_console(&self) -> Option<super::ConsoleRef> {
        self.io.snapshot().map(|c| c.console)
    }

    /// 追加一条给模型的用户消息（下一步前并进对话）。
    pub fn send_user_note(&self, text: String) {
        self.pending.lock().user.push(text);
    }

    pub fn set_seed(&self, seed: Seed) {
        *self.seed.lock() = Some(seed);
    }

    pub fn take_seed(&self) -> Option<Seed> {
        self.seed.lock().take()
    }

    pub fn take_force_screen(&self) -> bool {
        std::mem::take(&mut self.pending.lock().force_screen)
    }

    /// 重启前挂着的审批卡在这边被答了：等它的那次写入已经不在了，结论只能告诉模型让它重发。
    pub fn take_stale_approvals(&self) -> Vec<(String, bool)> {
        self.pending.lock().approvals.drain().collect()
    }

    pub fn set_notice(&self, n: Option<String>) {
        self.io.note_notice(n.as_deref());
        *self.notice.lock() = n;
    }

    pub fn notice(&self) -> Option<String> {
        self.notice.lock().clone()
    }

    /// 被接驳的新任务顶替：旧任务随后被取消，退出时什么都不收。
    pub fn supersede(&self) {
        self.superseded.store(true, Ordering::Relaxed);
        // 只置取消旗：挂起中的 park 最多 CANCEL_POLL 后醒来退出。**不能**投 Stop —— 那会把旧任务
        // 迁到终态，副作用里放掉的是新旧任务共用的 `host:{id}` 租约，还会把这一行落成 stopped
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_superseded(&self) -> bool {
        self.superseded.load(Ordering::Relaxed)
    }

    /// 运行任务退出时调用（`finish_hosted` 第一行）。接驳的新任务要等到它，
    /// 否则旧任务稍后 drop 的相位 / 托管登记守卫会把新任务的登记摘掉。
    pub fn mark_exited(&self) {
        self.exited.store(true, Ordering::Release);
    }

    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }

    /// 测试用：假装已经结束了 `ago` 这么久。
    #[cfg(test)]
    pub(crate) fn mark_ended_ago(&self, ago: Duration) {
        *self.ended_at.lock() = std::time::Instant::now().checked_sub(ago);
    }

    /// 进入终态多久了；没结束为 `None`。
    pub fn ended_for(&self) -> Option<Duration> {
        self.ended_at.lock().map(|t| t.elapsed())
    }

    /// Running 期间的周期快照：距上次超过 `PROGRESS_TICK` 且对话有变化时才克隆历史、落盘。
    /// 相位迁移（问人 / 审批 / 暂停 / 完成）另外即时落盘，用的是最近一次快照。
    pub fn tick_progress(&self, messages: &[super::super::llm::ChatMessage], step: usize) -> bool {
        if self.is_superseded() {
            return false;
        }
        let fp = (messages.len(), messages.last().map_or(0, |m| m.content.len()), step);
        {
            let mut g = self.progress.lock();
            let due = g.0.map_or(true, |t| t.elapsed() >= PROGRESS_TICK);
            if !due || g.1 == fp {
                return false;
            }
            *g = (Some(std::time::Instant::now()), fp);
        }
        self.note_progress(messages, step);
        self.checkpoint_now();
        true
    }

    /// 马上把进度记进快照（要挂起等人之前用，保证落盘的是最新对话）。不落盘。
    pub fn flush_progress(&self, messages: &[super::super::llm::ChatMessage], step: usize) {
        let fp = (messages.len(), messages.last().map_or(0, |m| m.content.len()), step);
        {
            let mut g = self.progress.lock();
            if g.1 == fp {
                return;
            }
            g.1 = fp;
        }
        self.note_progress(messages, step);
    }

    // ——— 契约 / 上下文 ———

    pub fn ctx(&self) -> HostCtx {
        self.ctx.lock().0.clone()
    }

    pub fn ctx_version(&self) -> u64 {
        self.ctx.lock().1
    }

    /// 换上新的契约上下文；`note` 有值时作为「契约已更新：…」留给运行循环追加给模型。
    pub fn set_ctx(&self, ctx: HostCtx, note: Option<String>) {
        self.io.note_contract(&ctx.contract, &ctx.authority);
        {
            let mut g = self.ctx.lock();
            g.0 = ctx;
            g.1 += 1;
        }
        if let Some(n) = note {
            self.amend_notes.lock().push(n);
        }
    }

    pub fn take_amend_notes(&self) -> Vec<String> {
        std::mem::take(&mut self.amend_notes.lock())
    }

    /// 用户确认（或 YOLO 直接采用）的完成判据写进契约。
    pub fn set_criteria(&self, checks: Vec<Check>, yolo: bool) {
        let mut c = self.ctx();
        c.contract.success_criteria = checks;
        c.contract.yolo = yolo;
        // 不是 YOLO 就是用户在草稿卡上确认（或编辑）过
        c.contract.criteria_confirmed = !yolo;
        self.set_ctx(c, None);
    }

    /// 开始托管前定判据：YOLO 直接采用草稿；否则出一张草稿卡挂起，等 `host_confirm_criteria`
    /// 把编辑后的判据写进契约再答复。返回最终判据；取消 / 结束返回 `None`。
    pub async fn settle_criteria(&self, draft: Vec<Check>, yolo: bool, card_id: &str, now_ms: i64) -> Option<Vec<Check>> {
        if yolo {
            self.set_criteria(draft.clone(), true);
            return Some(draft);
        }
        let mut card = HumanCard::ask(card_id, "确认完成判据：编辑后点「接受」开始托管", now_ms);
        card.kind = CardKind::CriteriaDraft { draft };
        match self.ask(card).await {
            ParkEnd::Running => Some(self.ctx().contract.success_criteria),
            _ => None,
        }
    }

    // ——— 契约级花费 ———

    pub fn spent(&self) -> Spent {
        self.spent.lock().clone()
    }

    pub fn set_spent(&self, s: Spent) {
        *self.spent.lock() = s;
    }

    // ——— 现场 ———

    /// 托管的 console 不在了（tab 关了 / 连接没了）：挂起（ConsoleGone），不再往一个不存在的终端写。
    /// 接驳是 PR6 的事；挂起中只有停止能结束它。返回是否刚挂起。
    pub fn console_gone(&self) -> bool {
        let p = self.phase();
        if p.is_terminal() || matches!(p, HostPhase::Suspended { .. }) {
            return false;
        }
        self.apply(HostEvent::Suspend(SuspendReason::ConsoleGone)).is_ok()
    }

    pub fn contract_id(&self) -> &str {
        &self.handle.contract_id
    }

    pub fn run_id(&self) -> &str {
        &self.handle.run_id
    }

    pub fn phase(&self) -> HostPhase {
        self.phase.lock().clone()
    }

    pub fn is_terminal(&self) -> bool {
        self.phase.lock().is_terminal()
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// 投一个事件进收件箱（命令层用）。运行循环在下一个闸口 / 挂起处处理。
    pub fn send(&self, ev: HostEvent) -> bool {
        self.tx.send(ev).is_ok()
    }

    /// 开局：落第一份 checkpoint、推一次相位、同步控制权。
    pub fn announce(&self) {
        if self.is_superseded() {
            return;
        }
        let p = self.phase();
        self.io.save(&p);
        self.io.emit(&p);
        self.io.sync_control(&p);
    }

    /// 立刻迁移一次并执行副作用。非法事件不改状态（控制权仍按当前相位对一次账：
    /// 旧的「交还」按钮可能已经把 ControlTable 翻成 AgentDriving，而相位并不接受它）。
    pub fn apply(&self, ev: HostEvent) -> Result<HostPhase, Invalid> {
        let name = ev.name();
        // 被接驳顶替的旧任务：相位冻结。它和新任务共用 `host:{id}`，任何迁移的副作用
        // （落盘覆盖新行、ReleaseLease 放掉新 console 的租约、同步控制权）都会打到新任务身上
        if self.is_superseded() {
            return Err(Invalid { from: "superseded", event: name });
        }
        let res = {
            let mut cur = self.phase.lock();
            match step(&cur, ev) {
                Ok((next, fx)) => {
                    *cur = next.clone();
                    Ok((next, fx))
                }
                Err(e) => Err((e, cur.clone())),
            }
        };
        match res {
            Ok((next, fx)) => {
                if next.is_terminal() {
                    let mut e = self.ended_at.lock();
                    e.get_or_insert_with(std::time::Instant::now);
                }
                for e in fx {
                    self.run_effect(e, &next);
                }
                self.io.sync_control(&next);
                Ok(next)
            }
            Err((e, cur)) => {
                tracing::debug!("托管 {} 忽略事件 {name}：{e}", self.contract_id());
                self.io.sync_control(&cur);
                Err(e)
            }
        }
    }

    fn run_effect(&self, e: Effect, next: &HostPhase) {
        match e {
            Effect::SaveCheckpoint => self.io.save(next),
            Effect::EmitPhase => self.io.emit(next),
            Effect::ReleaseLease => self.io.release_lease(),
            Effect::AppendUser(s) => self.pending.lock().user.push(s),
            Effect::Feedback(s) => self.pending.lock().feedback.push(s),
            Effect::Approval { card_id, approved } => {
                self.pending.lock().approvals.insert(card_id, approved);
            }
            Effect::ApplyBudget(g) => {
                let mut p = self.pending.lock();
                p.grant = Some(g);
            }
            // 换 console 由接驳命令重建任务时完成，这里只记「先读屏」
            Effect::Reattach(_) => {}
            Effect::ForceScreen => self.pending.lock().force_screen = true,
        }
    }

    /// 把收件箱里已到的事件全部处理掉，不等待。
    pub fn drain(&self) {
        let Ok(mut rx) = self.rx.try_lock() else { return };
        while let Ok(ev) = rx.try_recv() {
            self.apply_inbox(ev);
        }
    }

    /// 收件箱来的事件：记下「从暂停恢复」，运行循环据此换预算。
    fn apply_inbox(&self, ev: HostEvent) {
        // 期限暂停后用户点了「继续」：这一次期限算确认过了，不再一恢复就又停
        if matches!(ev, HostEvent::Resume { .. })
            && matches!(self.phase().base(), HostPhase::Paused { cause: super::PauseCause::Deadline })
        {
            self.deadline_acked.store(true, Ordering::Relaxed);
            self.io.note_deadline_ack(true);
        }
        let was_paused = matches!(self.phase(), HostPhase::Paused { .. });
        let resume = matches!(ev, HostEvent::Resume { .. });
        if self.apply(ev).is_ok() && resume && was_paused {
            self.pending.lock().resumed = true;
        }
    }

    /// 不在 Running 就挂起：只等收件箱和取消，**没有超时**、零 LLM 调用。
    pub async fn park(&self) -> ParkEnd {
        let mut rx = self.rx.lock().await;
        loop {
            let p = self.phase();
            if p.is_terminal() {
                return ParkEnd::Ended(p);
            }
            if self.cancelled() {
                return ParkEnd::Cancelled;
            }
            if p == HostPhase::Running {
                return ParkEnd::Running;
            }
            let ev = tokio::select! {
                ev = rx.recv() => ev,
                _ = wait_cancel(&self.cancel) => None,
                // 等人期间（问题卡 / 人在驾驶 / 暂停）tab 被关掉：转成挂起（ConsoleGone），面板出「待重新接驳」，
                // 而不是让一个没有终端的任务无声地占着租约
                _ = tokio::time::sleep(LIVENESS_POLL) => {
                    if !self.io.console_alive() {
                        self.console_gone();
                    }
                    None
                }
            };
            if let Some(ev) = ev {
                self.apply_inbox(ev);
            }
        }
    }

    /// 写 pty 之前的闸：先处理收件箱；`user_paused` 为真（用户在终端里动了键）而相位还是 Running 时
    /// 补一个 UserKey；然后不在 Running 就无限期挂起。返回 true = 可以写；false = 取消 / 已结束。
    ///
    /// 已知窄缝（复审 L2）：闸口放行到真正写 pty 之间还有几毫秒（写本身是异步的），用户恰好在这几毫秒里
    /// 按键不会被这次写入看到 —— 他的按键照常进终端，下一次写入前的闸口会接住（转 UserDriving）。
    /// 要彻底堵上得把「查控制权 + 写 pty」放进同一把锁，改动面太大，先记在这里。
    pub async fn gate(&self, user_paused: impl Fn() -> bool, console_alive: impl Fn() -> bool) -> bool {
        if self.cancelled() {
            return false;
        }
        self.drain();
        if !console_alive() {
            self.console_gone();
        }
        if self.phase() == HostPhase::Running && user_paused() {
            let _ = self.apply(HostEvent::UserKey);
        }
        self.park().await == ParkEnd::Running
    }

    /// 出一张卡（问题 / 人工确认）并挂起到有人答复。答复以用户消息留在 `take_user` 里。
    pub async fn ask(&self, card: HumanCard) -> ParkEnd {
        // 不在 Running（比如用户刚按了键）就先等它回来：卡片只能从 Running 发出
        match self.park().await {
            ParkEnd::Running => {}
            end => return end,
        }
        if let Err(e) = self.apply(HostEvent::Ask(card)) {
            tracing::warn!("托管 {} 出卡失败：{e}", self.contract_id());
        }
        self.park().await
    }

    /// 审批卡：**不设超时**。返回用户的结论；取消 / 结束 = 拒绝。
    pub async fn approve(&self, mut card: HumanCard) -> bool {
        card.kind = CardKind::Approve;
        let id = card.id.clone();
        match self.ask(card).await {
            ParkEnd::Running => self.pending.lock().approvals.remove(&id).unwrap_or(false),
            _ => false,
        }
    }

    /// 取走待追加的用户消息（卡片答复）。
    pub fn take_user(&self) -> Vec<String> {
        std::mem::take(&mut self.pending.lock().user)
    }

    /// 取走待并进反馈的文字（未满足的判据等）。
    pub fn take_feedback(&self) -> Vec<String> {
        std::mem::take(&mut self.pending.lock().feedback)
    }

    /// 取走「从暂停恢复」的信号与新预算：`None` = 没有恢复过；`Some(grant)` = 恢复了（grant 可能为空）。
    pub fn take_resume(&self) -> Option<Option<BudgetGrant>> {
        let mut p = self.pending.lock();
        if !std::mem::take(&mut p.resumed) {
            return None;
        }
        Some(p.grant.take())
    }

    /// 让运行循环下一次落盘时带上最新进度。
    pub fn note_progress(&self, messages: &[super::super::llm::ChatMessage], step: usize) {
        let spent = self.spent();
        self.io.note_progress(messages, step, &spent);
    }

    /// 周期快照：「动作 + 反馈」一轮之后落一次（相位不变）。
    pub fn checkpoint_now(&self) {
        if self.is_superseded() {
            return;
        }
        self.io.save(&self.phase());
    }
}

async fn wait_cancel(flag: &AtomicBool) {
    while !flag.load(Ordering::Relaxed) {
        tokio::time::sleep(CANCEL_POLL).await;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::{CardReply, PauseCause};
    use super::*;

    /// 记下每次副作用的假 IO。
    #[derive(Default)]
    pub(crate) struct FakeIo {
        pub saves: Mutex<Vec<String>>,
        pub emits: Mutex<Vec<String>>,
        pub released: Mutex<u32>,
        pub control: Mutex<Vec<String>>,
        /// console 没了（tab 关了）
        pub dead: Mutex<bool>,
        pub acked: Mutex<bool>,
    }

    impl HostIo for Arc<FakeIo> {
        fn save(&self, p: &HostPhase) {
            self.saves.lock().push(p.label().into());
        }
        fn emit(&self, p: &HostPhase) {
            self.emits.lock().push(p.label().into());
        }
        fn release_lease(&self) {
            *self.released.lock() += 1;
        }
        fn sync_control(&self, p: &HostPhase) {
            self.control.lock().push(p.label().into());
        }
        fn console_alive(&self) -> bool {
            !*self.dead.lock()
        }
        fn note_deadline_ack(&self, acked: bool) {
            *self.acked.lock() = acked;
        }
    }

    pub(crate) fn shared() -> (Arc<HostShared>, Arc<FakeIo>) {
        shared_as("c1", "t1")
    }

    pub(crate) fn shared_as(cid: &str, console: &str) -> (Arc<HostShared>, Arc<FakeIo>) {
        let io = Arc::new(FakeIo::default());
        let h = HostShared::new(
            HostHandle::new(cid),
            console,
            "目标",
            Arc::new(AtomicBool::new(false)),
            Box::new(io.clone()),
        );
        (h, io)
    }

    fn reply(id: &str, text: &str, approved: Option<bool>) -> CardReply {
        CardReply { card_id: id.into(), text: text.into(), approved }
    }

    /// 让被 spawn 的任务跑到下一个挂起点。
    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ask_parks_with_zero_progress_until_answer() {
        let (h, io) = shared();
        let h2 = h.clone();
        let task = tokio::spawn(async move { h2.ask(HumanCard::ask("k1", "部署到哪？", 0)).await });
        settle().await;
        assert!(matches!(h.phase(), HostPhase::WaitingHuman { .. }));
        // 一整天过去也还在等：卡片永远等下去
        tokio::time::advance(Duration::from_secs(24 * 3600)).await;
        settle().await;
        assert!(!task.is_finished());
        assert!(matches!(h.phase(), HostPhase::WaitingHuman { .. }));
        assert!(io.saves.lock().contains(&"waiting_human".to_string()), "进入等人时落 checkpoint");
        h.send(HostEvent::Answer(reply("k1", "staging", None)));
        assert_eq!(task.await.unwrap(), ParkEnd::Running);
        assert_eq!(h.take_user(), vec!["用户答复：staging".to_string()]);
        assert_eq!(*io.released.lock(), 0, "非终态不放租约");
    }

    #[tokio::test(start_paused = true)]
    async fn user_driving_has_no_timeout_and_handback_restores_resume_to() {
        let (h, _io) = shared();
        let h2 = h.clone();
        // 用户在终端里按了键：闸口补 UserKey，进 UserDriving{Running}
        let task = tokio::spawn(async move { h2.gate(|| true, || true).await });
        settle().await;
        assert_eq!(h.phase(), HostPhase::UserDriving { resume_to: Box::new(HostPhase::Running) });
        // 原来 300s（PAUSE_MAX_WAIT）就让模型收尾；现在多久都不动
        tokio::time::advance(Duration::from_secs(3 * 3600)).await;
        settle().await;
        assert!(!task.is_finished());
        h.send(HostEvent::Handback);
        assert!(task.await.unwrap(), "交还后可以写");
        assert_eq!(h.phase(), HostPhase::Running);
    }

    #[tokio::test(start_paused = true)]
    async fn handback_returns_to_waiting_card_not_running() {
        let (h, _io) = shared();
        h.apply(HostEvent::Ask(HumanCard::ask("k1", "q", 0))).unwrap();
        h.send(HostEvent::UserKey);
        h.drain();
        assert!(matches!(h.phase(), HostPhase::UserDriving { .. }));
        h.send(HostEvent::Handback);
        h.drain();
        assert!(matches!(h.phase(), HostPhase::WaitingHuman { ref card } if card.id == "k1"));
    }

    #[tokio::test(start_paused = true)]
    async fn approval_parks_without_timeout() {
        let (h, _io) = shared();
        let h2 = h.clone();
        let task = tokio::spawn(async move { h2.approve(HumanCard::ask("a1", "rm -rf build？", 0)).await });
        settle().await;
        match h.phase() {
            HostPhase::WaitingHuman { card } => assert_eq!(card.kind, CardKind::Approve),
            p => panic!("应在等审批：{p:?}"),
        }
        // 原来托管审批 30 分钟（STEWARD_APPROVAL_TIMEOUT_SECS）就按拒绝；现在不超时
        tokio::time::advance(Duration::from_secs(2 * 3600)).await;
        settle().await;
        assert!(!task.is_finished());
        h.send(HostEvent::Answer(reply("a1", "", Some(true))));
        assert!(task.await.unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn approval_denied_or_cancelled_is_false() {
        let (h, _io) = shared();
        let h2 = h.clone();
        let task = tokio::spawn(async move { h2.approve(HumanCard::ask("a1", "q", 0)).await });
        settle().await;
        h.send(HostEvent::Answer(reply("a1", "别删", Some(false))));
        assert!(!task.await.unwrap());
        assert_eq!(h.take_user(), vec!["用户答复：别删".to_string()]);

        let h3 = h.clone();
        let task = tokio::spawn(async move { h3.approve(HumanCard::ask("a2", "q", 0)).await });
        settle().await;
        h.cancel.store(true, Ordering::Relaxed);
        tokio::time::advance(CANCEL_POLL * 2).await;
        assert!(!task.await.unwrap(), "取消 = 拒绝，且及时返回");
    }

    #[tokio::test(start_paused = true)]
    async fn budget_pause_leaves_only_via_resume_and_carries_grant() {
        let (h, io) = shared();
        h.apply(HostEvent::BudgetExhausted("步数".into())).unwrap();
        let h2 = h.clone();
        let task = tokio::spawn(async move { h2.park().await });
        settle().await;
        // 按键 → 交还：仍是 Paused（预算绕过已堵住）
        h.send(HostEvent::UserKey);
        h.send(HostEvent::Handback);
        settle().await;
        assert!(matches!(h.phase(), HostPhase::Paused { cause: PauseCause::Budget { .. } }));
        assert!(!task.is_finished());
        let g = BudgetGrant { max_steps: Some(50), ..Default::default() };
        h.send(HostEvent::Resume { new_budget: Some(g) });
        assert_eq!(task.await.unwrap(), ParkEnd::Running);
        assert_eq!(h.take_resume(), Some(Some(g)));
        assert_eq!(h.take_resume(), None);
        assert!(io.control.lock().contains(&"paused".to_string()));
    }

    #[tokio::test(start_paused = true)]
    async fn stop_ends_park_and_releases_lease_once() {
        let (h, io) = shared();
        h.apply(HostEvent::LlmDown("503".into())).unwrap();
        let h2 = h.clone();
        let task = tokio::spawn(async move { h2.park().await });
        settle().await;
        h.send(HostEvent::Stop);
        assert_eq!(task.await.unwrap(), ParkEnd::Ended(HostPhase::Stopped));
        assert_eq!(*io.released.lock(), 1);
        // 终态吸收：再停一次不再放租约
        let _ = h.apply(HostEvent::Stop);
        assert_eq!(*io.released.lock(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn console_gone_suspends_and_waits_without_writing() {
        let (h, io) = shared();
        let h2 = h.clone();
        let task = tokio::spawn(async move { h2.gate(|| false, || false).await });
        settle().await;
        assert_eq!(
            h.phase(),
            HostPhase::Suspended { reason: SuspendReason::ConsoleGone, resume_to: Box::new(HostPhase::Running) }
        );
        tokio::time::advance(Duration::from_secs(3600)).await;
        settle().await;
        assert!(!task.is_finished(), "挂起中不写、也不自己结束");
        assert_eq!(*io.released.lock(), 0, "挂起不放租约");
        assert!(io.control.lock().last().map(String::as_str) == Some("suspended"));
        h.send(HostEvent::Stop);
        assert!(!task.await.unwrap());
        // 已挂起时再检测不重复迁移
        assert!(!h.console_gone());
    }

    #[tokio::test(start_paused = true)]
    async fn yolo_takes_the_draft_without_asking() {
        let (h, io) = shared();
        let draft = vec![Check::FileExists { path: "/srv/app/dist".into() }];
        let got = h.settle_criteria(draft.clone(), true, "cd1", 0).await;
        assert_eq!(got, Some(draft.clone()));
        assert_eq!(h.phase(), HostPhase::Running);
        assert!(io.saves.lock().is_empty(), "没有出卡、没有相位迁移");
        let c = h.ctx().contract;
        assert!(c.yolo);
        assert_eq!(c.success_criteria, draft);
        assert!(!c.criteria_confirmed, "YOLO 的判据没人确认过");
    }

    #[tokio::test(start_paused = true)]
    async fn non_yolo_waits_for_confirmed_criteria() {
        let (h, _io) = shared();
        let draft = vec![Check::Manual { text: "由你确认完成".into() }];
        let h2 = h.clone();
        let d2 = draft.clone();
        let task = tokio::spawn(async move { h2.settle_criteria(d2, false, "cd1", 0).await });
        settle().await;
        match h.phase() {
            HostPhase::WaitingHuman { card } => {
                assert_eq!(card.kind, CardKind::CriteriaDraft { draft: draft.clone() })
            }
            p => panic!("应等用户确认判据：{p:?}"),
        }
        tokio::time::advance(Duration::from_secs(24 * 3600)).await;
        settle().await;
        assert!(!task.is_finished());
        // host_confirm_criteria：先把编辑后的判据写进契约，再答复
        let edited = vec![Check::CommandOk { cmd: "cargo test".into() }];
        h.set_criteria(edited.clone(), false);
        h.send(HostEvent::Answer(CardReply { card_id: "cd1".into(), text: String::new(), approved: Some(true) }));
        assert_eq!(task.await.unwrap(), Some(edited));
        assert!(!h.ctx().contract.yolo);
        assert!(h.ctx().contract.criteria_confirmed, "草稿卡上接受 = 用户确认过");
        // 随契约进 checkpoint，重启后仍在
        let ctx = h.ctx();
        let cp = super::super::checkpoint::HostCheckpoint::new(ctx.contract, ctx.authority, Default::default(), 1);
        let back = super::super::checkpoint::HostCheckpoint::from_json(&cp.to_json(2).unwrap()).unwrap();
        assert!(back.contract.criteria_confirmed);
    }

    #[tokio::test(start_paused = true)]
    async fn superseded_task_is_inert_on_every_exit_path() {
        let (h, io) = shared();
        h.apply(HostEvent::Suspend(SuspendReason::ConsoleGone)).unwrap();
        let saves = io.saves.lock().len();
        let controls = io.control.lock().len();
        let h2 = h.clone();
        let parked = tokio::spawn(async move { h2.park().await });
        settle().await;
        h.supersede();
        tokio::time::advance(CANCEL_POLL * 2).await;
        assert_eq!(parked.await.unwrap(), ParkEnd::Cancelled, "挂起中的旧任务被取消旗叫醒");
        // 旧任务退出路径上可能碰到的一切：终态迁移、周期快照、即时落盘、开局广播
        for ev in [HostEvent::Stop, HostEvent::Fatal("x".into()), HostEvent::CompleteVerified { summary: "s".into(), checks: vec![] }] {
            assert!(h.apply(ev).is_err());
        }
        assert!(!h.tick_progress(&[super::super::super::llm::ChatMessage::user("x".to_string())], 99));
        h.checkpoint_now();
        h.announce();
        assert!(!h.is_terminal());
        assert_eq!(*io.released.lock(), 0, "不放共用的 host:{{id}} 租约");
        assert_eq!(io.saves.lock().len(), saves, "不覆盖新任务那一行");
        assert_eq!(io.control.lock().len(), controls, "不动控制权");
        // 写到一半被取消：闸口直接返回不可写
        assert!(!h.gate(|| false, || true).await);
    }

    #[test]
    fn progress_snapshot_only_when_due_and_changed() {
        let (h, io) = shared();
        let msgs = vec![super::super::super::llm::ChatMessage::user("a".to_string())];
        assert!(h.tick_progress(&msgs, 1), "第一次就落");
        assert!(!h.tick_progress(&msgs, 2), "不到 30 秒不落（也不克隆历史）");
        assert_eq!(io.saves.lock().len(), 1);
        // 要挂起前的 flush：对话变了才记，不落盘
        let more = vec![msgs[0].clone(), super::super::super::llm::ChatMessage::assistant("b".to_string())];
        h.flush_progress(&more, 2);
        assert_eq!(io.saves.lock().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn tab_closed_while_parked_becomes_console_gone() {
        let io = Arc::new(FakeIo::default());
        *io.dead.lock() = false;
        let h = HostShared::new(HostHandle::new("c1"), "t1", "目标", Arc::new(AtomicBool::new(false)), Box::new(io.clone()));
        h.apply(HostEvent::Ask(HumanCard::ask("k1", "q", 0))).unwrap();
        let h2 = h.clone();
        let task = tokio::spawn(async move { h2.park().await });
        settle().await;
        // tab 关了
        *io.dead.lock() = true;
        tokio::time::advance(LIVENESS_POLL * 2).await;
        settle().await;
        match h.phase() {
            HostPhase::Suspended { reason: SuspendReason::ConsoleGone, resume_to } => {
                assert!(matches!(*resume_to, HostPhase::WaitingHuman { .. }), "接驳后卡片还在");
            }
            p => panic!("应挂起待接驳：{p:?}"),
        }
        assert!(!task.is_finished(), "挂起后接着等（接驳 / 停止）");
        assert_eq!(*io.released.lock(), 0, "租约不放");
        h.send(HostEvent::Stop);
        task.await.unwrap();
    }

    #[test]
    fn deadline_pauses_once_then_resume_is_honored() {
        let (h, io) = shared();
        let mut c = h.ctx();
        c.contract.created_at = 1_000;
        c.contract.timebox_ms = 60_000;
        h.set_ctx(c, None);
        assert_eq!(h.deadline_ms(), Some(61_000));
        assert!(!h.check_deadline(30_000));
        assert!(h.check_deadline(61_000));
        assert_eq!(h.phase(), HostPhase::Paused { cause: PauseCause::Deadline });
        h.send(HostEvent::Resume { new_budget: None });
        h.drain();
        assert_eq!(h.phase(), HostPhase::Running);
        assert!(!h.check_deadline(99_000), "继续过了就不再因同一期限暂停");
        assert!(*io.acked.lock(), "确认记进快照，随 checkpoint 落盘");
        // 接驳后的新任务从 checkpoint 恢复确认：不再暂停
        let (h2, _io2) = shared();
        let mut c2 = h2.ctx();
        c2.contract.created_at = 1_000;
        c2.contract.timebox_ms = 60_000;
        h2.set_ctx(c2, None);
        h2.set_deadline_acked(true);
        assert!(!h2.check_deadline(99_000));
    }

    #[tokio::test(start_paused = true)]
    async fn every_transition_writes_checkpoint() {
        let (h, io) = shared();
        h.apply(HostEvent::Takeover).unwrap();
        h.apply(HostEvent::Handback).unwrap();
        h.apply(HostEvent::CompleteVerified { summary: "ok".into(), checks: vec![] }).unwrap();
        assert_eq!(*io.saves.lock(), vec!["user_driving", "running", "completed"]);
        assert_eq!(*io.emits.lock(), vec!["user_driving", "running", "completed"]);
        assert_eq!(*io.released.lock(), 1);
    }
}
