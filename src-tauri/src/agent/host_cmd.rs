//! 统一托管的命令层（设计 §1 / §2）：`host_start` 起任务，`host_*` 往任务的收件箱投事件。
//!
//! - `host_start`：**先**以 `host:{contract_id}` 拿 ControlTable 租约，**再**签契约（契约一出现在
//!   registry 里守护进程就看得见；此时租约已在，`control_owned` 为真，它只会观察），最后把
//!   `act::run(RunMode::Hosted)` 作为脱离的 tokio 任务交给 `HostSupervisor`，立即返回 contract_id；
//! - 相位变化与卡片走 `nx://host/{contract_id}`（另抄一份到 `nx://host/all` 给面板的总表），
//!   步骤仍走 `nx://agent/step/{console}`，RunFeed 照常渲染；
//! - 租约只在 Completed / Failed / Stopped 时释放（`Effect::ReleaseLease`，任务退出时再兜一次底）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::cmd::AppState;

use super::act::{self, AgentRunRequest, AgentTarget, RunBudgetOverride, RunMode};
use super::control::{ControlState, ControlTable};
use super::host::checkpoint::HostCheckpoint;
use super::checkpoint::{ResumeVerdict, Spent, CHECKPOINT_TTL_MS};
use super::host::checkpoint::LoadError;
use super::host::driver::Seed;
use super::policy::TIER_APPROVE;
use super::host::context::HostContext;
use super::host::criteria;
use super::host::gate::HardRule;
use super::host::{
    BudgetGrant, CardKind, CardReply, Check, ConsoleRef, HostCtx, HostEvent, HostHandle, HostIo, HostPhase,
    HostShared,
};
use super::policy::Authority;
use super::llm::ChatMessage;
use super::profile::StewardBrain;
use super::steward::{self, now_ms, ScreenStore, StewardContract, StewardRegistry};
use super::steward_core::StewardCore;
use super::steward_loop::StewardBus;
use super::AgentError;

/// 全局总表频道：面板据此维护「每个 console 的托管胶囊」。
pub const HOST_ALL_EVENT: &str = "nx://host/all";

/// `nx://host/{id}` 的载荷，也是 `host_status` / `host_list` 的返回。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostStatus {
    pub contract_id: String,
    pub run_id: String,
    pub console_id: String,
    pub goal: String,
    /// 短标签（running / waiting_human / user_driving / paused / suspended / completed / failed / stopped）
    pub state: &'static str,
    pub phase: HostPhase,
    /// 被托管终端的可读名字
    pub label: String,
    /// 给人看的提示（例如接驳后降为确认档的原因）
    pub notice: Option<String>,
    /// 计划清单（nx_todo）
    pub todo: Vec<super::todo::TodoItem>,
}

impl HostStatus {
    fn of(h: &HostShared, phase: &HostPhase) -> Self {
        Self {
            contract_id: h.contract_id().to_string(),
            run_id: h.run_id().to_string(),
            console_id: h.console_id.clone(),
            goal: h.goal.clone(),
            state: phase.label(),
            phase: phase.clone(),
            label: h.ctx().label,
            notice: h.notice(),
            todo: h.todo(),
        }
    }
}

fn emit_status(app: &AppHandle, st: &HostStatus) {
    let _ = app.emit(&format!("nx://host/{}", st.contract_id), st.clone());
    let _ = app.emit(HOST_ALL_EVENT, st.clone());
}

/// 真实的副作用落地：SQLite（host_sessions）+ Tauri 事件 + ControlTable。
struct AppHostIo {
    app: AppHandle,
    handle: HostHandle,
    console_id: String,
    goal: String,
    /// 下一次落盘的现场（相位在 `save` 时填）
    snap: Mutex<HostCheckpoint>,
    notice: Mutex<Option<String>>,
}

/// 托管状态快照（`nx://host/*` 事件的负载）。
fn status_of(
    handle: &HostHandle,
    console_id: &str,
    goal: &str,
    snap: &Mutex<HostCheckpoint>,
    notice: &Mutex<Option<String>>,
    phase: &HostPhase,
) -> HostStatus {
    // 只锁一次 `snap`：在结构体字面量里写两次 `snap.lock()`，第一个临时 guard 要活到整条语句结束，
    // 第二次加锁就自锁了（parking_lot 不可重入）—— 曾让 `host_start` 卡死主线程、整个窗口无响应。
    let (label, todo) = {
        let s = snap.lock();
        (s.console.label.clone(), s.todo.clone())
    };
    let notice = notice.lock().clone();
    HostStatus {
        contract_id: handle.contract_id.clone(),
        run_id: handle.run_id.clone(),
        console_id: console_id.to_string(),
        goal: goal.to_string(),
        state: phase.label(),
        phase: phase.clone(),
        label,
        notice,
        todo,
    }
}

impl HostIo for AppHostIo {
    fn save(&self, phase: &HostPhase) {
        let Some(st) = self.app.try_state::<AppState>() else { return };
        let mut c = self.snap.lock();
        c.phase = phase.clone();
        // 接驳时拿它和那时的屏幕比，判断现场有没有漂移
        if let Some(h) = self.app.try_state::<ScreenStore>().and_then(|s| s.get(&self.console_id)).map(|s| s.hash) {
            c.last_screen_hash = h;
        }
        let now = now_ms();
        match c.to_json(now) {
            Ok(json) => {
                if let Err(e) = st.store.host_session_upsert(&self.handle.contract_id, c.state(), &json, now) {
                    tracing::warn!("托管现场落盘失败：{e}");
                }
            }
            Err(e) => tracing::warn!("托管现场序列化失败：{e}"),
        }
    }

    fn emit(&self, phase: &HostPhase) {
        let st = status_of(&self.handle, &self.console_id, &self.goal, &self.snap, &self.notice, phase);
        emit_status(&self.app, &st);
    }

    fn release_lease(&self) {
        if let Some(st) = self.app.try_state::<AppState>() {
            st.act.release_control(&self.app, &self.handle.run_id);
        }
    }

    fn sync_control(&self, phase: &HostPhase) {
        let Some(st) = self.app.try_state::<AppState>() else { return };
        let ctl = &st.act.control;
        let changed = match control_target(phase) {
            Some(ControlState::Paused) => ctl.pause_run(&self.handle.run_id),
            Some(_) => ctl.resume_run(&self.handle.run_id),
            None => return,
        };
        for (session, s) in changed {
            act::emit_control(&self.app, &session, &ctl.holders_of(&session), s);
        }
    }

    fn note_progress(&self, messages: &[ChatMessage], step: usize, spent: &Spent) {
        record_progress(&mut self.snap.lock(), messages, step, spent);
    }

    fn note_todo(&self, todo: &[super::todo::TodoItem], phase: &HostPhase) {
        self.snap.lock().todo = todo.to_vec();
        self.emit(phase);
    }

    fn note_watchdog(&self, w: &super::host::watchdog::WatchdogState) {
        self.snap.lock().watchdog = w.clone();
    }

    fn note_deadline_ack(&self, acked: bool) {
        self.snap.lock().deadline_acked = acked;
    }

    fn console_alive(&self) -> bool {
        self.app.try_state::<AppState>().map_or(true, |st| act::session_alive(&st, &self.console_id))
    }

    fn note_notice(&self, notice: Option<&str>) {
        *self.notice.lock() = notice.map(str::to_string);
    }

    fn snapshot(&self) -> Option<HostCheckpoint> {
        Some(self.snap.lock().clone())
    }

    fn note_contract(&self, contract: &StewardContract, authority: &Authority) {
        let mut c = self.snap.lock();
        c.contract = contract.clone();
        c.authority = authority.clone();
    }
}

/// 把运行进度写进待落盘的现场（契约级花费随它持久化，重启不清零）。
pub(crate) fn record_progress(c: &mut HostCheckpoint, messages: &[ChatMessage], step: usize, spent: &Spent) {
    c.messages = messages.to_vec();
    c.step = step;
    c.spent = *spent;
}

/// 渲染托管契约上下文（设计 §3）：授权 + Hard / Soft 偏好 + 意向栈 + 主机备忘 + KB（≤2KB）。
/// Hard 偏好里带 `denyPattern` 的同时编译成 `gate::authorize` 强制的规则。
pub(crate) fn build_ctx(
    app: &AppHandle,
    state: &AppState,
    contract: StewardContract,
    authority: Authority,
    label: &str,
) -> HostCtx {
    let sit = act::situation_now(app, state, &contract.console_id, false);
    let redact = super::load_config(&state.store).active_account().redact;
    let mem = super::hostmem::prompt_section(state, &contract.console_id, label, redact);
    let (section, refs, hard) = match app.try_state::<StewardBrain>() {
        Some(brain) => {
            let kb = brain.retrieve_kb(&sit, 3);
            let (section, refs) = HostContext::build(&contract, &authority, &brain, sit.program, &mem, &kb);
            let (hard, _) = brain.applicable_preferences(sit.program, &contract.host_key);
            (section, refs, HardRule::from_prefs(&hard))
        }
        None => (String::new(), Vec::new(), Vec::new()),
    };
    HostCtx { contract, authority, hard, section, refs, label: label.to_string() }
}

/// 开始托管前起草完成判据：一次 LLM 调用，60 秒拿不到就退回人工确认。
async fn draft_criteria(cfg: &super::AgentConfig, goal: &str, cancel: &AtomicBool) -> Vec<Check> {
    let acct = cfg.active_account();
    let msgs = vec![ChatMessage::user(criteria::draft_prompt(goal))];
    let call = super::llm::complete_reply(acct, criteria::DRAFT_SYSTEM, &msgs, &[], Some(cancel));
    let reply = match tokio::time::timeout(Duration::from_secs(60), call).await {
        Ok(Ok(r)) => Ok(r.text),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("起草超时".to_string()),
    };
    criteria::draft_or_fallback(reply)
}

/// 相位在 ControlTable 上的投影：人在驾驶 / 暂停 / 挂起 → Paused（租约不放，守护进程照样只看）；
/// 运行 / 等人 → AgentDriving（用户一敲键就落回 Paused 并投 UserKey）；终态不管（由放租约收尾）。
pub(crate) fn control_target(phase: &HostPhase) -> Option<ControlState> {
    match phase {
        HostPhase::UserDriving { .. } | HostPhase::Paused { .. } | HostPhase::Suspended { .. } => {
            Some(ControlState::Paused)
        }
        HostPhase::Running | HostPhase::WaitingHuman { .. } => Some(ControlState::AgentDriving),
        _ => None,
    }
}

/// 启动顺序的唯一实现：先拿租约，再签契约；签约失败就把租约放回去。
/// 抽成纯函数是为了能单测「契约对守护进程可见的那一刻，租约已经在了」。
pub(crate) fn claim_then_sign<T>(
    control: &ControlTable,
    console_id: &str,
    run_id: &str,
    sign: impl FnOnce() -> Result<T, String>,
) -> Result<(T, Option<ControlState>), String> {
    let changed = control.acquire(console_id, run_id);
    match sign() {
        Ok(v) => Ok((v, changed)),
        Err(e) => {
            control.release_run(run_id);
            Err(e)
        }
    }
}

/// 开始托管：立即返回 contract_id，运行实况走事件。
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn host_start(
    console_id: String,
    goal: String,
    authority_tier: Option<String>,
    label: Option<String>,
    budget: Option<RunBudgetOverride>,
    console_text: Option<String>,
    console_alt: Option<bool>,
    yolo: Option<bool>,
    console_ref: Option<ConsoleHint>,
    app: AppHandle,
    state: State<'_, AppState>,
    registry: State<'_, StewardRegistry>,
    brain: State<'_, StewardBrain>,
    bus: State<'_, StewardBus>,
    core: State<'_, StewardCore>,
) -> Result<String, String> {
    let goal = goal.trim().to_string();
    if goal.is_empty() {
        return Err("托管目标不能为空".to_string());
    }
    if !state.local.contains(&console_id) && !state.sessions.contains(&console_id) {
        return Err(format!("没有 id 为「{console_id}」的终端"));
    }
    let rt = &state.act;
    if rt.host.live_on_console(&console_id).is_some() {
        return Err("这个终端已经在托管中".to_string());
    }
    if rt.is_running(&console_id) {
        return Err("这个终端上有一轮对话 / 技能正在运行，请先停止".to_string());
    }
    // 上一份 Agent 配置不可用就别签约：签了也起不来
    let cfg = super::load_config(&state.store);
    super::llm::validate(&cfg).map_err(|e| e.to_string())?;

    let contract_id = steward::new_contract_id(&console_id);
    let handle = HostHandle::new(&contract_id);
    let tier = authority_tier.unwrap_or_else(|| "auto".to_string());
    let (contract, changed) = claim_then_sign(&rt.control, &console_id, &handle.run_id, || {
        steward::sign_contract(
            Some(&contract_id),
            &console_id,
            &goal,
            &tier,
            &state,
            &registry,
            &brain,
            &bus,
            &core,
        )
    })?;
    if let Some(st) = changed {
        act::emit_control(&app, &console_id, &[handle.run_id.clone()], st);
    }

    let mut hint = console_ref.unwrap_or_default();
    if hint.label.as_deref().map_or(true, |l| l.trim().is_empty()) {
        hint.label = label;
    }
    let console = console_ref_of(&state, &console_id, &hint);
    let label = console.label.clone();
    // 文件工具的相对路径按它补全
    let hint_cwd = console.cwd.clone();
    let snap = HostCheckpoint::new(contract.clone(), core.authority(&contract_id), console, now_ms());
    let io = AppHostIo {
        app: app.clone(),
        handle: handle.clone(),
        console_id: console_id.clone(),
        goal: goal.clone(),
        snap: Mutex::new(snap),
        notice: Mutex::new(None),
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let shared = HostShared::new(handle, &console_id, &goal, cancel, Box::new(io));
    let authority = core.authority(&contract_id);
    shared.set_ctx(build_ctx(&app, &state, contract, authority, &label), None);
    rt.host.register(Arc::clone(&shared));
    shared.announce();
    let yolo = yolo.unwrap_or(false);

    let req = AgentRunRequest {
        session_id: console_id.clone(),
        question: goal,
        targets: vec![AgentTarget { id: console_id, label, cwd: hint_cwd }],
        console_text,
        console_alt: console_alt.unwrap_or(false),
        mode: Some("steward".to_string()),
        budget,
    };
    let task_app = app.clone();
    let task_host = Arc::clone(&shared);
    tauri::async_runtime::spawn(async move {
        let st = task_app.state::<AppState>();
        // 先定完成判据：YOLO 直接采用草稿；否则挂起等用户编辑 / 接受（这期间零 LLM 调用、不写终端）
        let draft = draft_criteria(&cfg, &req.question, &task_host.cancel).await;
        let card_id = uuid::Uuid::new_v4().simple().to_string();
        let res = match task_host.settle_criteria(draft, yolo, &card_id, now_ms()).await {
            Some(checks) => {
                // 判据进了契约：重渲染一次上下文（目标 / 判据 / 授权都在这一节里）
                let c = task_host.ctx();
                task_host.set_ctx(build_ctx(&task_app, &st, c.contract, c.authority, &c.label), None);
                tracing::info!("托管 {} 判据 {} 条（YOLO：{yolo}）", task_host.contract_id(), checks.len());
                act::run(&task_app, &st, &st.act, &cfg, &req, RunMode::Hosted(Arc::clone(&task_host))).await
            }
            None => Ok(()),
        };
        finish_hosted(&task_app, &task_host, res);
    });
    Ok(contract_id)
}

/// 托管任务退出：还没进终态就按出口补一个（取消 → Stopped；出错 → Failed），
/// 再兜底放租约、收掉守护进程那边的契约。
fn finish_hosted(app: &AppHandle, h: &HostShared, res: Result<(), AgentError>) {
    h.mark_exited();
    // 被接驳的新任务顶替：租约、契约、这一行都归新任务了，旧任务什么都不收
    // （不迁终态 → 没有 ReleaseLease；不放 `host:{id}` 租约；不停契约）
    if !should_finalize(h) {
        return;
    }
    if !h.is_terminal() {
        let ev = match res {
            _ if h.cancel.load(Ordering::Relaxed) => HostEvent::Stop,
            Err(e) => HostEvent::Fatal(e.to_string()),
            Ok(()) => HostEvent::Fatal("托管运行意外结束".to_string()),
        };
        let _ = h.apply(ev);
    }
    let Some(st) = app.try_state::<AppState>() else { return };
    st.act.release_control(app, h.run_id());
    if let (Some(registry), Some(brain), Some(core)) = (
        app.try_state::<StewardRegistry>(),
        app.try_state::<StewardBrain>(),
        app.try_state::<StewardCore>(),
    ) {
        steward::stop_contract(&registry, &brain, &st.store, &core, h.contract_id());
    }
    if let Some(bus) = app.try_state::<StewardBus>() {
        bus.wake();
    }
}

fn hosted(state: &AppState, contract_id: &str) -> Result<Arc<HostShared>, String> {
    state.act.host.get(contract_id).ok_or_else(|| format!("没有托管任务 {contract_id}"))
}

/// 人在驾驶时点了卡片上的按钮 / 「加预算继续」：先替他交还，事件才合法。
fn handback_if_driving(app: &AppHandle, state: &AppState, h: &HostShared) {
    if matches!(h.phase(), HostPhase::UserDriving { .. }) {
        release_user_control(app, state, h);
        h.send(HostEvent::Handback);
    }
}

/// 控制表那一半的「交还」：把终端从用户手里拿回来，并推控制权事件。
fn release_user_control(app: &AppHandle, state: &AppState, h: &HostShared) {
    if let Some(st) = state.act.control.handback(&h.console_id) {
        act::emit_control(app, &h.console_id, &state.act.control.holders_of(&h.console_id), st);
    }
}

/// 停止托管。`send_ctrl_c` 时顺手给它驾驶的终端发 Ctrl-C。
#[tauri::command]
pub async fn host_stop(
    contract_id: String,
    send_ctrl_c: Option<bool>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<bool, String> {
    let h = hosted(&state, &contract_id)?;
    if h.is_terminal() {
        return Ok(false);
    }
    let had_task = state.act.is_running(h.run_id());
    h.cancel.store(true, Ordering::Relaxed);
    act::stop_run(&app, &state, h.run_id(), send_ctrl_c.unwrap_or(false)).await;
    // 马上进终态（UI 立刻看到「已停止」），再投一个 Stop 把挂起中的任务叫醒
    let _ = h.apply(HostEvent::Stop);
    h.send(HostEvent::Stop);
    // 启动扫描恢复的挂起行没有运行任务：没人走 finish_hosted，契约在这里收
    if !had_task {
        finish_hosted(&app, &h, Ok(()));
    }
    Ok(true)
}

/// 答复卡片（问题 / 审批）。卡片 id 不对 / 已经答过 → Err。
#[tauri::command]
pub async fn host_answer(
    contract_id: String,
    reply: CardReply,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let h = hosted(&state, &contract_id)?;
    match h.phase().base() {
        // 密码卡的值只走密码通道；判据草稿走 host_confirm_criteria
        HostPhase::WaitingHuman { card } if matches!(card.kind, CardKind::Secret | CardKind::CriteriaDraft { .. }) => {
            return Err("这张卡片不能在这里答复".to_string())
        }
        HostPhase::WaitingHuman { card } if card.id == reply.card_id => {}
        _ => return Err("这张卡片已经处理过了，或不是当前等你答复的那张".to_string()),
    }
    h.answer_card(reply, || release_user_control(&app, &state, &h));
    Ok(())
}

/// 接管：Agent 立刻让路（等同于在终端里敲了键），直到「交还」。
#[tauri::command]
pub async fn host_takeover(contract_id: String, app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let h = hosted(&state, &contract_id)?;
    if let Some(st) = state.act.control.takeover(&h.console_id) {
        act::emit_control(&app, &h.console_id, &state.act.control.holders_of(&h.console_id), st);
    }
    h.send(HostEvent::Takeover);
    Ok(())
}

/// 交还：回到接管前的状态（不一定是运行中：预算暂停时接管，交还后仍是暂停）。
#[tauri::command]
pub async fn host_handback(contract_id: String, app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let h = hosted(&state, &contract_id)?;
    if let Some(st) = state.act.control.handback(&h.console_id) {
        act::emit_control(&app, &h.console_id, &state.act.control.holders_of(&h.console_id), st);
    }
    h.send(HostEvent::Handback);
    Ok(())
}

/// 暂停：下一个闸口（下一次模型调用前 / 下一个动作前）停下，等「继续」。
#[tauri::command]
pub async fn host_pause(contract_id: String, state: State<'_, AppState>) -> Result<(), String> {
    let h = hosted(&state, &contract_id)?;
    h.send(HostEvent::Pause);
    Ok(())
}

/// 加预算继续：只有暂停（预算 / 模型不可用 / 手动）才接受。
#[tauri::command]
pub async fn host_resume(
    contract_id: String,
    budget: Option<BudgetGrant>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let h = hosted(&state, &contract_id)?;
    if !matches!(h.phase().base(), HostPhase::Paused { .. }) {
        return Err("托管没有暂停，不需要继续".to_string());
    }
    handback_if_driving(&app, &state, &h);
    h.send(HostEvent::Resume { new_budget: budget });
    Ok(())
}

/// 确认完成判据（草稿卡上点「接受」）：编辑后的判据写进契约，再答复草稿卡让任务开跑。
#[tauri::command]
pub async fn host_confirm_criteria(
    contract_id: String,
    card_id: String,
    checks: Vec<Check>,
    app: AppHandle,
    registry: State<'_, StewardRegistry>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let h = hosted(&state, &contract_id)?;
    match h.phase().base() {
        HostPhase::WaitingHuman { card } if card.id == card_id && matches!(card.kind, CardKind::CriteriaDraft { .. }) => {}
        _ => return Err("没有等你确认的判据草稿".to_string()),
    }
    let checks = if checks.is_empty() { criteria::fallback() } else { checks };
    registry.set_criteria(&contract_id, checks.clone(), false);
    h.set_criteria(checks, false);
    // 等确认时用户可能动过终端（你在驾驶）：先交还再答复，否则 Answer 被静默丢掉、「接受」没反应
    h.answer_card(CardReply { card_id, text: String::new(), approved: Some(true) }, || {
        release_user_control(&app, &state, &h)
    });
    Ok(())
}

/// 改契约（目标 / 档位）：不重启运行。重建 `## 托管契约` 一节，并在下一步前告诉模型「契约已更新：…」。
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn host_amend(
    contract_id: String,
    goal: Option<String>,
    authority_tier: Option<String>,
    app: AppHandle,
    registry: State<'_, StewardRegistry>,
    brain: State<'_, StewardBrain>,
    state: State<'_, AppState>,
    bus: State<'_, StewardBus>,
    core: State<'_, StewardCore>,
) -> Result<(), String> {
    let h = hosted(&state, &contract_id)?;
    if h.is_terminal() {
        return Err("托管已经结束，不能再改".to_string());
    }
    let goal = goal.map(|g| g.trim().to_string()).filter(|g| !g.is_empty());
    let tier = authority_tier.filter(|t| !t.trim().is_empty());
    if goal.is_none() && tier.is_none() {
        return Ok(());
    }
    let amended = steward::steward_amend(
        contract_id.clone(),
        goal.clone(),
        tier.clone(),
        registry,
        brain,
        state.clone(),
        bus,
        core.clone(),
    )
    .ok_or_else(|| "契约不存在".to_string())?;
    let prev = h.ctx();
    let mut contract = amended;
    // 判据 / YOLO 以托管任务这边为准（registry 那份可能是签约时的空表）
    contract.success_criteria = prev.contract.success_criteria.clone();
    contract.yolo = prev.contract.yolo;
    let authority = core.authority(&contract_id);
    let note = amend_note(goal.as_deref(), tier.as_deref());
    h.set_ctx(build_ctx(&app, &state, contract, authority, &prev.label), Some(note));
    Ok(())
}

/// 「契约已更新：…」那句话。
pub(crate) fn amend_note(goal: Option<&str>, tier: Option<&str>) -> String {
    let mut parts = Vec::new();
    if let Some(g) = goal {
        parts.push(format!("目标改为「{g}」"));
    }
    if let Some(t) = tier {
        parts.push(format!("授权档位改为 {t}"));
    }
    format!("契约已更新：{}。系统提示里的「托管契约」一节已按新契约重建，后续请以它为准。", parts.join("；"))
}

/// Notify 档的「撤销」：没有真回滚 —— 前端先接管，这里把用户的纠正记成 Learned 偏好，
/// 并把那条决策标成 rejected，下次同主题检索会命中它。
#[tauri::command]
pub async fn host_correct(
    contract_id: String,
    seq: i64,
    topic: String,
    correction: String,
    brain: State<'_, StewardBrain>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let _ = hosted(&state, &contract_id)?;
    state.store.steward_decision_outcome(seq, "rejected").ok();
    brain.set_decision_outcome(seq, "rejected");
    let correction = correction.trim();
    // 同类纠正不无限堆积：等价的 Learned 偏好已存在就跳过（原升级卡「我自己来」的去重口径）
    let skip = matches!(
        super::learning::should_learn(&brain.view().items, topic.trim(), correction),
        super::learning::LearnDecision::Skip { .. }
    );
    if !correction.is_empty() && !skip {
        let item = brain.learn_correction(seq, topic.trim(), correction)?;
        state.store.steward_ctx_upsert(&item.to_row())?;
    }
    Ok(())
}

#[tauri::command]
pub fn host_status(contract_id: String, state: State<'_, AppState>) -> Option<HostStatus> {
    state.act.host.get(&contract_id).map(|h| HostStatus::of(&h, &h.phase()))
}

/// 全部托管任务（含已结束、还没被同一 console 的新托管顶掉的）。
#[tauri::command]
pub fn host_list(state: State<'_, AppState>) -> Vec<HostStatus> {
    state.act.host.all().iter().map(|h| HostStatus::of(h, &h.phase())).collect()
}

// ——————————————————————— 重启恢复与重新接驳（设计 §4） ———————————————————————

/// 前端给的 console 线索（TerminalTab 上有的：保存的 SSH 记录、标签名、探测到的 cwd / tmux）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleHint {
    #[serde(default)]
    pub console_id: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub session_record_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub tmux: Option<String>,
}

fn nonempty(s: &Option<String>) -> Option<String> {
    s.as_deref().map(str::trim).filter(|x| !x.is_empty()).map(str::to_string)
}

/// 线索 + 主机键 → 可落盘的 ConsoleRef（纯函数）。
pub(crate) fn console_ref_from(host_key: String, console_id: &str, hint: &ConsoleHint) -> ConsoleRef {
    ConsoleRef {
        host_key,
        session_record_id: nonempty(&hint.session_record_id),
        label: nonempty(&hint.label).unwrap_or_else(|| "当前会话".to_string()),
        cwd: nonempty(&hint.cwd),
        tmux: nonempty(&hint.tmux),
        console_id: console_id.to_string(),
    }
}

/// 主机键用 `hostmem::resolve_host`（ssh:user@host:port / local / tab:标签）：重启后 console id 会变，它不变。
pub(crate) fn console_ref_of(state: &AppState, console_id: &str, hint: &ConsoleHint) -> ConsoleRef {
    let label = nonempty(&hint.label).unwrap_or_else(|| "当前会话".to_string());
    let key = super::hostmem::resolve_host(state, console_id, &label).map(|h| h.key).unwrap_or_default();
    console_ref_from(key, console_id, hint)
}

/// 启动扫描的纯逻辑：先按 TTL 把过期行标成 expired，再把所有非终态行读回来、迁到
/// Suspended(AppRestart) 并写回（`updated_at` 不动，重启不给 TTL 续命）。比本程序新的版本不碰。
/// 返回 (挂起待接驳的现场, 过期的契约 id)。**此时不写任何终端。**
pub(crate) fn scan_rows(store: &crate::store::Store, now: i64) -> (Vec<HostCheckpoint>, Vec<String>) {
    let expired = store.host_sessions_expire(now, CHECKPOINT_TTL_MS).unwrap_or_else(|e| {
        tracing::warn!("托管现场过期清理失败：{e}");
        Vec::new()
    });
    let mut out = Vec::new();
    for row in store.host_sessions_non_terminal() {
        match HostCheckpoint::from_json(&row.json) {
            Ok(mut cp) => {
                cp.suspend_for_restart();
                match cp.to_json(now) {
                    Ok(j) => {
                        store.host_session_upsert(&row.contract_id, cp.state(), &j, row.updated_at).ok();
                    }
                    Err(e) => tracing::warn!("托管现场 {} 回写失败：{e}", row.contract_id),
                }
                out.push(cp);
            }
            Err(LoadError::FutureVersion(v)) => {
                tracing::warn!("托管现场 {} 由更新版本（v{v}）写入，保留原行不恢复", row.contract_id)
            }
            Err(e) => tracing::warn!("托管现场 {} 读不回来：{e}", row.contract_id),
        }
    }
    (out, expired)
}

/// 用一份现场搭一个托管任务的共享状态（还没起运行任务）。启动扫描与接驳共用。
fn shared_from_checkpoint(app: &AppHandle, cp: &HostCheckpoint) -> Arc<HostShared> {
    let handle = HostHandle::new(&cp.contract.id);
    let io = AppHostIo {
        app: app.clone(),
        handle: handle.clone(),
        console_id: cp.console.console_id.clone(),
        goal: cp.contract.goal.clone(),
        snap: Mutex::new(cp.clone()),
        notice: Mutex::new(None),
    };
    let h = HostShared::new(
        handle,
        &cp.console.console_id,
        &cp.contract.goal,
        Arc::new(AtomicBool::new(false)),
        Box::new(io),
    );
    let mut ctx = HostCtx::bare(&cp.contract.id, &cp.console.console_id, &cp.contract.goal);
    ctx.contract = cp.contract.clone();
    ctx.authority = cp.authority.clone();
    ctx.label = cp.console.label.clone();
    h.set_ctx(ctx, None);
    h.set_spent(cp.spent);
    h.restore_watchdog(cp.watchdog.clone());
    h.set_todo(cp.todo.clone());
    h.set_deadline_acked(cp.deadline_acked);
    h
}

/// 启动时调用（AppState 已 manage 之后）：未结束的托管一律挂起待接驳，登记进 HostSupervisor
/// 让 host_list 看得见；过期的收掉契约。不写任何终端。
pub fn boot_scan(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else { return };
    // 过期的行由 scan_rows 在 host_sessions 里收掉，这里只记一笔
    let (restored, expired) = scan_rows(&state.store, now_ms());
    for cp in restored {
        let h = shared_from_checkpoint(app, &cp);
        h.restore(cp.phase.clone(), Vec::new());
        state.act.host.register(h);
    }
    if !expired.is_empty() {
        tracing::info!("托管现场过期 {} 份：{:?}", expired.len(), expired);
    }
}

/// 一个接驳候选：打分越高越像重启前那个 console。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReattachCandidate {
    pub console_id: String,
    pub label: String,
    pub score: u8,
}

/// 纯：按 host_key → 记录 id → 标签 → cwd 给候选排序，丢掉毫无关系的。
pub(crate) fn rank_candidates(saved: &ConsoleRef, cands: &[ConsoleRef]) -> Vec<ReattachCandidate> {
    saved
        .rank(cands)
        .into_iter()
        .map(|c| ReattachCandidate { console_id: c.console_id.clone(), label: c.label.clone(), score: saved.match_score(c) })
        .collect()
}

/// 「待重新接驳」卡上的候选 tab。前端把开着的 tab 的线索传进来。
#[tauri::command]
pub fn host_reattach_candidates(
    contract_id: String,
    candidates: Vec<ConsoleHint>,
    state: State<'_, AppState>,
) -> Result<Vec<ReattachCandidate>, String> {
    let h = hosted(&state, &contract_id)?;
    let saved = h.saved_console().ok_or_else(|| "没有可接驳的现场".to_string())?;
    let cands: Vec<ConsoleRef> = candidates
        .iter()
        .filter(|c| !c.console_id.is_empty())
        .map(|c| console_ref_of(&state, &c.console_id, c))
        .collect();
    Ok(rank_candidates(&saved, &cands))
}

/// 接驳判定：没帧 / 新开的连接都无从核对 → 按漂移处理（降到确认档），不拒绝；过期才拒绝。
pub(crate) fn reattach_plan(
    cp: &HostCheckpoint,
    console: ConsoleRef,
    current_hash: Option<&str>,
    now: i64,
) -> Result<(HostCheckpoint, Vec<super::host::phase::Effect>, bool), String> {
    let mut verdict = cp.reattach_verdict(current_hash, now);
    if matches!(verdict, ResumeVerdict::Suspended { .. }) {
        verdict = ResumeVerdict::Drift { downgrade_to: TIER_APPROVE.to_string() };
    }
    let drifted = matches!(verdict, ResumeVerdict::Drift { .. });
    let mut next = cp.clone();
    let mut fx = next.reattach(console, &verdict)?;
    // 密码卡的请求通道随旧进程没了，没法再答：回到运行，让模型重新发起 nx_secret
    if matches!(&next.phase, HostPhase::WaitingHuman { card } if card.kind == CardKind::Secret) {
        next.phase = HostPhase::Running;
        fx.push(super::host::phase::Effect::Feedback(
            "（重启前等你输入的密码请求已经失效，没有写入任何东西；需要的话请重新调用 nx_secret。）".to_string(),
        ));
    }
    next.contract.console_id = next.console.console_id.clone();
    next.contract.authority_tier = next.authority.tier.clone();
    Ok((next, fx, drifted))
}

/// 用户确认接驳：接到 `console_id` 这个 tab，或 `open_new` 按保存的线索新开一个连接。
/// 新任务带着重启前的对话、花费（作为新的一段预算起点）、判据、契约 / 授权与挂着的卡片接着跑；
/// 第一步前强制读屏，并告诉模型「应用重启后重新接驳，先读屏核对现场」。屏幕漂移就降到确认档。
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn host_reattach(
    contract_id: String,
    console_id: Option<String>,
    open_new: Option<bool>,
    app: AppHandle,
    state: State<'_, AppState>,
    registry: State<'_, StewardRegistry>,
    core: State<'_, StewardCore>,
    screen: State<'_, ScreenStore>,
) -> Result<(), String> {
    // 同一份契约同时只允许一次接驳（双击 / 两个面板一起点）：否则会起两个任务，其中一个没有租约
    let _reattaching = state
        .act
        .host
        .begin_reattach(&contract_id)
        .ok_or_else(|| "这份托管正在接驳中，请稍候".to_string())?;
    let old = hosted(&state, &contract_id)?;
    if !matches!(old.phase(), HostPhase::Suspended { .. }) {
        return Err("这份托管没有在等重新接驳".to_string());
    }
    let mut cp = old.checkpoint().ok_or_else(|| "没有可接驳的现场".to_string())?;
    let saved = cp.console.clone();
    let mut restore_note = None;
    let console_id = if open_new.unwrap_or(false) {
        let kind = match &saved.session_record_id {
            Some(rec) => act::OpenKind::Ssh { session_record_id: rec.clone() },
            None => act::OpenKind::Local,
        };
        let spec = act::OpenSpec { kind, label: saved.label.clone(), command: None };
        let out = act::open_session(&app, &state, &saved.console_id, spec).await.map_err(|e| e.to_string())?;
        restore_note = restore_commands_note(&saved);
        out.session_id
    } else {
        console_id.filter(|c| !c.trim().is_empty()).ok_or_else(|| "请选择要接驳的终端".to_string())?
    };
    if !act::session_alive(&state, &console_id) {
        return Err(format!("没有 id 为「{console_id}」的终端"));
    }
    // 打开新连接要等一阵：回来后再确认一遍还是那份挂起的任务（没被停掉 / 没被别处接走）。
    // 新开的标签页不替用户关（里面可能已经有他要的现场 / 正在登录），只在报错里说明
    if let Err(e) = still_reattachable(&state.act.host, &contract_id, &old) {
        return Err(if open_new.unwrap_or(false) {
            format!("{e}。刚为接驳新开的标签页「{}」保留着，不需要的话可以手动关掉。", saved.label)
        } else {
            e
        });
    }
    if state.act.host.live_on_console(&console_id).is_some_and(|h| h.contract_id() != contract_id) {
        return Err("这个终端正被另一份托管占着".to_string());
    }
    if state.act.is_running(&console_id) {
        return Err("这个终端上有一轮对话 / 技能正在运行，请先停止".to_string());
    }
    let hint = ConsoleHint {
        console_id: console_id.clone(),
        label: Some(saved.label.clone()),
        session_record_id: saved.session_record_id.clone(),
        cwd: saved.cwd.clone(),
        tmux: saved.tmux.clone(),
    };
    let console = console_ref_of(&state, &console_id, &hint);
    let current = screen.get(&console_id).map(|s| s.hash);
    // 新开的连接屏幕一定不一样：不拿它比，直接按漂移降档
    let current = if open_new.unwrap_or(false) { None } else { current };
    // 托管任务这边的判据 / 授权以内存为准（可能被 host_amend / 判据确认改过）
    let live = old.ctx();
    cp.contract = live.contract.clone();
    cp.authority = live.authority.clone();
    let (next, effects, drifted) = reattach_plan(&cp, console, current.as_deref(), now_ms())?;

    // 租约换到新 console：拿不到（别的运行占着）就不接驳、不写；拿到后撤掉旧 console 上的。
    // 这一步在契约对守护进程可见之前完成（与 host_start 同一顺序），确认的那一刻 control_owned(新) 即为真
    let old_console = old.console_id.clone();
    let changes = move_lease(&state.act.control, old.run_id(), &old_console, &console_id)?;
    for (session, st) in changes {
        act::emit_control(&app, &session, &state.act.control.holders_of(&session), st);
    }
    // 同一会话里 tab 关掉（ConsoleGone）时旧任务还挂在 park 上：让位给新任务
    let had_task = state.act.is_running(old.run_id());
    if had_task {
        old.supersede();
    }
    registry.restore(next.contract.clone());
    core.set_authority(&contract_id, next.authority.clone());

    let h = shared_from_checkpoint(&app, &next);
    h.restore(next.phase.clone(), effects);
    h.set_seed(Seed { messages: next.messages.clone(), step: next.step });
    if let Some(n) = restore_note {
        h.send_user_note(n);
    }
    if drifted {
        h.set_notice(Some("接驳时的屏幕和重启前不一样（或是新开的连接），已降为「确认」档：每次写入都会先问你。".to_string()));
    }
    let c = h.ctx();
    h.set_ctx(build_ctx(&app, &state, c.contract, c.authority, &c.label), None);
    state.act.host.register(Arc::clone(&h));
    h.announce();
    spawn_resumed(app.clone(), h, had_task.then(|| Arc::clone(&old)));
    Ok(())
}

/// 托管任务退出时要不要收尾（迁终态 / 放租约 / 停契约）。被接驳顶替的旧任务一律不收。
pub(crate) fn should_finalize(h: &HostShared) -> bool {
    !h.is_superseded()
}

/// 接驳时把 `host:{id}` 的租约从旧 console 挪到新 console（ControlTable 按 console 记）。
/// - 新 console 已被别的运行占着 → Err，不接驳；
/// - 先拿新的，再撤旧的（旧 tab 还在就还给用户，不在了就清掉登记）。
/// 返回需要推事件的变化。
pub(crate) fn move_lease(
    control: &ControlTable,
    run_id: &str,
    old_console: &str,
    new_console: &str,
) -> Result<Vec<(String, ControlState)>, String> {
    if control.holders_of(new_console).iter().any(|h| h != run_id) {
        return Err("这个终端正被另一轮运行驾驶着（对话 / 技能 / 另一份托管），先停掉它再接驳".to_string());
    }
    let mut changes = Vec::new();
    if let Some(st) = control.acquire(new_console, run_id) {
        changes.push((new_console.to_string(), st));
    }
    if old_console != new_console {
        if let Some(st) = control.release_session(old_console, run_id) {
            changes.push((old_console.to_string(), st));
        }
    }
    Ok(changes)
}

/// 接驳的 await 之后复核：登记表里还是同一个任务对象，且仍挂起。
pub(crate) fn still_reattachable(
    sup: &super::host::HostSupervisor,
    contract_id: &str,
    old: &Arc<HostShared>,
) -> Result<(), String> {
    match sup.get(contract_id) {
        Some(cur) if Arc::ptr_eq(&cur, old) && matches!(cur.phase(), HostPhase::Suspended { .. }) => Ok(()),
        _ => Err("这份托管在接驳过程中已被停止或接到了别处".to_string()),
    }
}

/// 新开连接后要恢复的工作目录 / tmux：交给模型经 nx_exec 去做（走 gate::authorize，cd / tmux attach 这类
/// 例行命令照常放行或按档位批准），不在这里绕过闸门直接写。
pub(crate) fn restore_commands_note(c: &ConsoleRef) -> Option<String> {
    let mut steps = Vec::new();
    if let Some(cwd) = &c.cwd {
        steps.push(format!("`cd {}`", shell_quote(cwd)));
    }
    if let Some(t) = &c.tmux {
        steps.push(format!("`tmux attach -t {}`", shell_quote(t)));
    }
    if steps.is_empty() {
        return None;
    }
    Some(format!(
        "这是为接驳新开的连接，现场还没恢复。请先读屏，再依次执行 {}，确认回到重启前的现场后再继续。",
        steps.join("，然后 ")
    ))
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-~:@".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// 起接驳后的运行任务：等旧任务（若有）退出、腾出运行登记，再接着跑。
fn spawn_resumed(app: AppHandle, h: Arc<HostShared>, old: Option<Arc<HostShared>>) {
    tauri::async_runtime::spawn(async move {
        let st = app.state::<AppState>();
        // 等被顶替的旧任务彻底退出（守卫都 drop 了）再起，免得它的收尾摘掉新任务的登记
        if let Some(old) = old {
            for _ in 0..300 {
                if old.has_exited() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if !old.has_exited() {
                tracing::warn!("托管 {} 的旧任务 30 秒没退出，仍按新现场接着跑", h.contract_id());
            }
        }
        // 重启前停在判据草稿卡上：先等它确认，再开跑
        if matches!(h.phase().base(), HostPhase::WaitingHuman { card } if matches!(card.kind, CardKind::CriteriaDraft { .. })) {
            if !matches!(h.park().await, super::host::ParkEnd::Running) {
                finish_hosted(&app, &h, Ok(()));
                return;
            }
        }
        let cfg = super::load_config(&st.store);
        let c = h.ctx();
        let req = AgentRunRequest {
            session_id: h.console_id.clone(),
            question: c.contract.goal.clone(),
            targets: vec![AgentTarget { id: h.console_id.clone(), label: c.label.clone(), cwd: h.saved_console().and_then(|c| c.cwd) }],
            console_text: None,
            console_alt: false,
            mode: Some("steward".to_string()),
            budget: None,
        };
        let res = act::run(&app, &st, &st.act, &cfg, &req, RunMode::Hosted(Arc::clone(&h))).await;
        finish_hosted(&app, &h, res);
    });
}

#[cfg(test)]
mod tests {
    use super::super::host::PauseCause;
    use super::*;

    /// 这个 console 是否归托管：有租约持有者，或状态不是 UserDriving（暂停中的运行同样占着）。
    fn control_owned(table: &ControlTable, console_id: &str) -> bool {
        !table.holders_of(console_id).is_empty()
            || table.state(console_id) != ControlState::UserDriving
    }

    #[test]
    fn lease_is_held_before_contract_becomes_visible() {
        let table = ControlTable::new();
        let registry = StewardRegistry::new();
        let (c, changed) = claim_then_sign(&table, "t1", "host:c1", || {
            // 签约这一刻：租约已在托管手里
            assert!(control_owned(&table, "t1"));
            assert_eq!(table.holders_of("t1"), vec!["host:c1".to_string()]);
            assert!(registry.for_console("t1").is_none(), "租约在前、契约在后");
            Ok::<_, String>(registry.start_with_id("c1", "t1", "local", "目标", "auto"))
        })
        .unwrap();
        assert_eq!(c.id, "c1");
        assert_eq!(changed, Some(ControlState::AgentDriving));
        assert!(registry.for_console("t1").is_some());
    }

    /// 回归：组装托管状态时对同一把 `snap` 锁了两次（同一条语句里临时 guard 活到语句末尾），
    /// parking_lot 不可重入 → 自锁。`host_start` 是同步命令跑在主线程上，整个窗口卡死。
    #[test]
    fn status_of_does_not_self_deadlock() {
        let ctx = HostCtx::bare("c1", "t1", "目标");
        let console = ConsoleRef {
            host_key: "local:t1".into(),
            console_id: "t1".into(),
            label: "本地".into(),
            ..Default::default()
        };
        let mut cp = HostCheckpoint::new(ctx.contract, ctx.authority, console, 1);
        cp.todo = vec![super::super::todo::TodoItem { text: "第一步".into(), status: Default::default() }];
        let snap = std::sync::Arc::new(Mutex::new(cp));
        let notice = std::sync::Arc::new(Mutex::new(Some("提示".to_string())));
        let (tx, rx) = std::sync::mpsc::channel();
        let (s, n) = (snap.clone(), notice.clone());
        std::thread::spawn(move || {
            let st = status_of(&HostHandle::new("c1"), "t1", "目标", &s, &n, &HostPhase::Running);
            let _ = tx.send(st);
        });
        let st = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("status_of 2 秒内没返回：同一把锁被重复获取（自锁）");
        assert_eq!(st.label, "本地");
        assert_eq!(st.todo.len(), 1);
        assert_eq!(st.notice.as_deref(), Some("提示"));
    }

    #[test]
    fn contract_spent_survives_checkpoint_round_trip() {
        let ctx = HostCtx::bare("c1", "t1", "目标");
        let console = ConsoleRef { host_key: "local:t1".into(), console_id: "t1".into(), ..Default::default() };
        let mut cp = HostCheckpoint::new(ctx.contract, ctx.authority, console, 1);
        let spent = Spent { steps: 412, elapsed_secs: 3_600, output_chars: 90_000, injections: 0 };
        record_progress(&mut cp, &[ChatMessage::user("目标".to_string())], 412, &spent);
        let back = HostCheckpoint::from_json(&cp.to_json(2).unwrap()).unwrap();
        assert_eq!(back.spent, spent);
        assert_eq!(back.step, 412);
    }

    #[test]
    fn amend_note_names_what_changed() {
        let n = amend_note(Some("改成只升级 patch 版本"), Some("approve"));
        assert!(n.starts_with("契约已更新：") && n.contains("patch") && n.contains("approve"));
    }

    // ——— PR6：重启恢复 / 接驳 ———

    use super::super::host::phase::REATTACH_NOTE;
    use super::super::host::{HumanCard, SuspendReason};
    use crate::store::tests::temp_store_at;

    fn saved_console(id: &str) -> ConsoleRef {
        ConsoleRef {
            host_key: "ssh:root@web1:22".into(),
            session_record_id: Some("rec-web1".into()),
            label: "web1".into(),
            cwd: Some("/srv/app".into()),
            tmux: Some("main".into()),
            console_id: id.into(),
        }
    }

    fn saved_cp(phase: HostPhase, created: i64) -> HostCheckpoint {
        let mut ctx = HostCtx::bare("c1", "tab-old", "升级依赖");
        ctx.authority = Authority::default().with_tier("auto");
        ctx.contract.success_criteria = vec![Check::CommandOk { cmd: "cargo test".into() }];
        let mut cp = HostCheckpoint::new(ctx.contract, ctx.authority, saved_console("tab-old"), created);
        cp.phase = phase;
        cp.messages = vec![
            ChatMessage::user("升级依赖".to_string()),
            ChatMessage::assistant("先看锁文件".to_string()),
            ChatMessage::user("（cargo update 的输出）".to_string()),
        ];
        cp.step = 7;
        cp.spent = Spent { steps: 7, elapsed_secs: 120, output_chars: 5_000, injections: 0 };
        cp.last_screen_hash = "h-before".into();
        cp
    }

    fn ask_card() -> HumanCard {
        HumanCard::ask("k1", "要不要连 major 版本一起升？", 1)
    }

    #[tokio::test(start_paused = true)]
    async fn restart_suspends_then_reattach_resumes_with_messages_and_pending_card() {
        let (store, _) = temp_store_at("host-boot");
        let now = 10_000_000;
        let cp = saved_cp(HostPhase::WaitingHuman { card: ask_card() }, now - 1_000);
        store.host_session_upsert("c1", cp.state(), &cp.to_json(now - 500).unwrap(), now - 500).unwrap();
        // 任务没了（应用被杀）→ 启动扫描
        let (restored, expired) = scan_rows(&store, now);
        assert!(expired.is_empty());
        assert_eq!(restored.len(), 1);
        let r = &restored[0];
        assert_eq!(
            r.phase,
            HostPhase::Suspended {
                reason: SuspendReason::AppRestart,
                resume_to: Box::new(HostPhase::WaitingHuman { card: ask_card() })
            }
        );
        let row = store.host_session_get("c1").unwrap();
        assert_eq!(row.state, "suspended");
        assert_eq!(row.updated_at, now - 500, "扫描不给 TTL 续命");
        // 接到一个新 tab（屏幕与重启前一致）
        let mut console = saved_console("tab-new");
        console.console_id = "tab-new".into();
        let (next, fx, drifted) = reattach_plan(r, console, Some("h-before"), now).unwrap();
        assert!(!drifted);
        assert_eq!(next.authority.tier, "auto", "现场一致不降档");
        assert_eq!(next.contract.console_id, "tab-new");
        assert_eq!(next.contract.success_criteria, vec![Check::CommandOk { cmd: "cargo test".into() }]);
        // 搭任务：挂着的卡片原样恢复，对话 / 步数接着用，第一步前强制读屏并告诉模型
        let (h, _io) = super::super::host::driver::tests::shared_as("c1", "tab-new");
        h.restore(next.phase.clone(), fx);
        h.set_seed(Seed { messages: next.messages.clone(), step: next.step });
        h.set_spent(next.spent);
        assert_eq!(h.phase(), HostPhase::WaitingHuman { card: ask_card() });
        let seed = h.take_seed().unwrap();
        assert_eq!(seed.messages.len(), 3);
        assert_eq!(seed.messages[2].content, "（cargo update 的输出）");
        assert_eq!(seed.step, 7);
        assert!(h.take_force_screen());
        assert_eq!(h.take_user(), vec![REATTACH_NOTE.to_string()]);
        assert_eq!(h.spent().steps, 7, "契约级花费带过来");
        // 卡片照常能答
        h.send(HostEvent::Answer(CardReply { card_id: "k1".into(), text: "只升 minor".into(), approved: None }));
        assert_eq!(h.park().await, super::super::host::ParkEnd::Running);
        assert_eq!(h.take_user(), vec!["用户答复：只升 minor".to_string()]);
    }

    #[test]
    fn drift_or_no_frame_downgrades_to_approve() {
        let cp = HostCheckpoint {
            phase: HostPhase::Suspended { reason: SuspendReason::AppRestart, resume_to: Box::new(HostPhase::Running) },
            ..saved_cp(HostPhase::Running, 1_000)
        };
        let (next, _, drifted) = reattach_plan(&cp, saved_console("t2"), Some("h-other"), 2_000).unwrap();
        assert!(drifted);
        assert_eq!(next.authority.tier, TIER_APPROVE);
        assert_eq!(next.contract.authority_tier, TIER_APPROVE);
        assert_eq!(next.phase, HostPhase::Running);
        let (next, _, drifted) = reattach_plan(&cp, saved_console("t2"), None, 2_000).unwrap();
        assert!(drifted && next.authority.tier == TIER_APPROVE, "没帧无从核对：按漂移降档，不拒绝");
    }

    #[test]
    fn ttl_expires_rows_and_refuses_reattach() {
        let (store, _) = temp_store_at("host-ttl");
        let now = 100 * 3_600_000;
        let old = saved_cp(HostPhase::Running, 1);
        store.host_session_upsert("old", "running", &old.to_json(1).unwrap(), 1).unwrap();
        let (restored, expired) = scan_rows(&store, now);
        assert!(restored.is_empty());
        assert_eq!(expired, vec!["old".to_string()]);
        assert_eq!(store.host_session_get("old").unwrap().state, "expired");
        // 创建超过 24h 的现场不接驳
        let cp = HostCheckpoint {
            phase: HostPhase::Suspended { reason: SuspendReason::AppRestart, resume_to: Box::new(HostPhase::Running) },
            ..saved_cp(HostPhase::Running, 1)
        };
        assert!(reattach_plan(&cp, saved_console("t2"), Some("h-before"), now).is_err());
    }

    #[test]
    fn console_gone_reattaches_to_another_tab() {
        let cp = HostCheckpoint {
            phase: HostPhase::Suspended {
                reason: SuspendReason::ConsoleGone,
                resume_to: Box::new(HostPhase::Paused { cause: super::super::host::PauseCause::Budget { reason: "步数".into() } }),
            },
            ..saved_cp(HostPhase::Running, 1_000)
        };
        let (next, fx, _) = reattach_plan(&cp, saved_console("tab-3"), Some("h-before"), 2_000).unwrap();
        assert!(matches!(next.phase, HostPhase::Paused { .. }), "回到挂起前的状态，预算暂停不被绕过");
        assert_eq!(next.console.console_id, "tab-3");
        assert!(fx.contains(&super::super::host::phase::Effect::ForceScreen));
    }

    #[test]
    fn pending_secret_card_does_not_survive_restart() {
        let mut card = HumanCard::ask("s1", "输入密码", 1);
        card.kind = CardKind::Secret;
        let cp = HostCheckpoint {
            phase: HostPhase::Suspended {
                reason: SuspendReason::AppRestart,
                resume_to: Box::new(HostPhase::WaitingHuman { card }),
            },
            ..saved_cp(HostPhase::Running, 1_000)
        };
        let (next, fx, _) = reattach_plan(&cp, saved_console("t2"), Some("h-before"), 2_000).unwrap();
        assert_eq!(next.phase, HostPhase::Running);
        assert!(fx.iter().any(|e| matches!(e, super::super::host::phase::Effect::Feedback(t) if t.contains("nx_secret"))));
    }

    #[test]
    fn reattach_moves_lease_from_a_to_b_and_keeps_contract_live() {
        let table = ControlTable::new();
        let registry = StewardRegistry::new();
        let run = "host:c1";
        // 原来托管在 A 上：租约 + 契约 + 一个挂在 ConsoleGone 上的旧任务
        table.acquire("tab-A", run);
        registry.start_with_id("c1", "tab-A", "local", "升级依赖", "auto");
        let (old, old_io) = super::super::host::driver::tests::shared_as("c1", "tab-A");
        old.apply(HostEvent::Suspend(SuspendReason::ConsoleGone)).unwrap();

        // 确认接驳到 B：先挪租约，再顶替旧任务、恢复契约
        let changes = move_lease(&table, run, "tab-A", "tab-B").unwrap();
        assert!(control_owned(&table, "tab-B"), "确认的那一刻 B 就归托管");
        assert!(!control_owned(&table, "tab-A"), "A 上的租约撤掉、登记清掉");
        assert_eq!(table.holders_of("tab-B"), vec![run.to_string()]);
        assert!(changes.iter().any(|(s, st)| s == "tab-B" && *st == ControlState::AgentDriving));
        assert!(changes.iter().any(|(s, st)| s == "tab-A" && *st == ControlState::UserDriving));
        old.supersede();
        let mut c = registry.get("c1").unwrap();
        c.console_id = "tab-B".into();
        registry.restore(c);

        // 旧任务退出：不收尾 —— 不迁终态（没有 ReleaseLease）、不停契约
        assert!(!should_finalize(&old));
        assert!(!old.is_terminal());
        assert_eq!(*old_io.released.lock(), 0);
        assert!(control_owned(&table, "tab-B"));
        assert_eq!(registry.get("c1").map(|c| c.console_id), Some("tab-B".to_string()), "契约仍在");

        // B 已被别的运行占着 → 不接驳、不写，A 的租约也不动
        let t2 = ControlTable::new();
        t2.acquire("tab-A", run);
        t2.acquire("tab-B", "tab-B"); // 对话运行（运行 id = 发起会话）
        assert!(move_lease(&t2, run, "tab-A", "tab-B").is_err());
        assert_eq!(t2.holders_of("tab-B"), vec!["tab-B".to_string()]);
        assert_eq!(t2.holders_of("tab-A"), vec![run.to_string()]);
    }

    #[test]
    fn reattach_rechecks_after_await() {
        let sup = super::super::host::HostSupervisor::new();
        let (old, _io) = super::super::host::driver::tests::shared();
        old.apply(HostEvent::Suspend(SuspendReason::ConsoleGone)).unwrap();
        sup.register(old.clone());
        assert!(still_reattachable(&sup, "c1", &old).is_ok());
        // await 期间别处已经接走（登记表里换成了新任务）：拒绝
        let (newer, _io) = super::super::host::driver::tests::shared();
        sup.register(newer.clone());
        assert!(still_reattachable(&sup, "c1", &old).is_err());
        // 或者被停掉了
        let sup2 = super::super::host::HostSupervisor::new();
        sup2.register(old.clone());
        old.apply(HostEvent::Stop).unwrap();
        assert!(still_reattachable(&sup2, "c1", &old).is_err());
    }

    #[test]
    fn candidates_ranked_by_host_record_label_cwd() {
        let saved = saved_console("old");
        let cand = |id: &str, host: &str, rec: Option<&str>, label: &str, cwd: Option<&str>| ConsoleRef {
            host_key: host.into(),
            session_record_id: rec.map(Into::into),
            label: label.into(),
            cwd: cwd.map(Into::into),
            tmux: None,
            console_id: id.into(),
        };
        let got = rank_candidates(
            &saved,
            &[
                cand("t-cwd", "local", None, "x", Some("/srv/app")),
                cand("t-host", "ssh:root@web1:22", None, "y", None),
                cand("t-none", "local", None, "z", None),
                cand("t-rec", "ssh:other:22", Some("rec-web1"), "web1", None),
            ],
        );
        let ids: Vec<_> = got.iter().map(|c| c.console_id.as_str()).collect();
        assert_eq!(ids, vec!["t-host", "t-rec", "t-cwd"]);
        assert!(got[0].score > got[1].score);
    }

    #[test]
    fn console_ref_builder_and_restore_commands() {
        let hint = ConsoleHint {
            console_id: "t9".into(),
            label: Some("  ".into()),
            session_record_id: Some("".into()),
            cwd: Some("/srv/my app".into()),
            tmux: Some("main".into()),
        };
        let c = console_ref_from("local".into(), "t9", &hint);
        assert_eq!(c.label, "当前会话");
        assert_eq!(c.session_record_id, None);
        assert_eq!(c.cwd.as_deref(), Some("/srv/my app"));
        let note = restore_commands_note(&c).unwrap();
        assert!(note.contains("`cd '/srv/my app'`") && note.contains("`tmux attach -t main`"), "{note}");
        assert!(restore_commands_note(&console_ref_from("local".into(), "t", &ConsoleHint::default())).is_none());
    }

    #[test]
    fn failed_sign_gives_the_lease_back() {
        let table = ControlTable::new();
        let r: Result<((), _), _> = claim_then_sign(&table, "t1", "host:c1", || Err("坏了".into()));
        assert!(r.is_err());
        assert!(!control_owned(&table, "t1"));
    }

    #[test]
    fn control_projection_keeps_lease_paused_while_human_or_budget() {
        let running = HostPhase::Running;
        assert_eq!(control_target(&running), Some(ControlState::AgentDriving));
        let paused = HostPhase::Paused { cause: PauseCause::Budget { reason: "x".into() } };
        assert_eq!(control_target(&paused), Some(ControlState::Paused));
        let driving = HostPhase::UserDriving { resume_to: Box::new(running) };
        assert_eq!(control_target(&driving), Some(ControlState::Paused));
        assert_eq!(control_target(&HostPhase::Stopped), None);
    }
}
