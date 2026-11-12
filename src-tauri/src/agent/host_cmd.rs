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

use parking_lot::Mutex;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::cmd::AppState;

use super::act::{self, AgentRunRequest, AgentTarget, RunBudgetOverride, RunMode};
use super::control::{ControlState, ControlTable};
use super::host::checkpoint::HostCheckpoint;
use super::host::{
    BudgetGrant, CardReply, ConsoleRef, HostEvent, HostHandle, HostIo, HostPhase, HostShared,
};
use super::llm::ChatMessage;
use super::profile::StewardBrain;
use super::steward::{self, now_ms, ScreenStore, StewardRegistry};
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
}

impl HostIo for AppHostIo {
    fn save(&self, phase: &HostPhase) {
        let Some(st) = self.app.try_state::<AppState>() else { return };
        let mut c = self.snap.lock();
        c.phase = phase.clone();
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
        let st = HostStatus {
            contract_id: self.handle.contract_id.clone(),
            run_id: self.handle.run_id.clone(),
            console_id: self.console_id.clone(),
            goal: self.goal.clone(),
            state: phase.label(),
            phase: phase.clone(),
        };
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

    fn note_progress(&self, messages: &[ChatMessage], step: usize) {
        let mut c = self.snap.lock();
        c.messages = messages.to_vec();
        c.step = step;
    }
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
pub fn host_start(
    console_id: String,
    goal: String,
    authority_tier: Option<String>,
    label: Option<String>,
    budget: Option<RunBudgetOverride>,
    console_text: Option<String>,
    console_alt: Option<bool>,
    app: AppHandle,
    state: State<'_, AppState>,
    registry: State<'_, StewardRegistry>,
    brain: State<'_, StewardBrain>,
    bus: State<'_, StewardBus>,
    core: State<'_, StewardCore>,
    screen: State<'_, ScreenStore>,
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
            &screen,
        )
    })?;
    if let Some(st) = changed {
        act::emit_control(&app, &console_id, &[handle.run_id.clone()], st);
    }

    let label = label.filter(|l| !l.trim().is_empty()).unwrap_or_else(|| "当前会话".to_string());
    let console = ConsoleRef {
        host_key: contract.host_key.clone(),
        session_record_id: None,
        label: label.clone(),
        cwd: None,
        tmux: None,
        console_id: console_id.clone(),
    };
    let snap = HostCheckpoint::new(contract.clone(), core.authority(&contract_id), console, now_ms());
    let io = AppHostIo {
        app: app.clone(),
        handle: handle.clone(),
        console_id: console_id.clone(),
        goal: goal.clone(),
        snap: Mutex::new(snap),
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let shared = HostShared::new(handle, &console_id, &goal, cancel, Box::new(io));
    rt.host.register(Arc::clone(&shared));
    shared.announce();

    let req = AgentRunRequest {
        session_id: console_id.clone(),
        question: goal,
        targets: vec![AgentTarget { id: console_id, label }],
        console_text,
        console_alt: console_alt.unwrap_or(false),
        mode: Some("steward".to_string()),
        budget,
    };
    let task_app = app.clone();
    let task_host = Arc::clone(&shared);
    tauri::async_runtime::spawn(async move {
        let st = task_app.state::<AppState>();
        let res = act::run(&task_app, &st, &st.act, &cfg, &req, RunMode::Hosted(Arc::clone(&task_host))).await;
        finish_hosted(&task_app, &task_host, res);
    });
    Ok(contract_id)
}

/// 托管任务退出：还没进终态就按出口补一个（取消 → Stopped；出错 → Failed），
/// 再兜底放租约、收掉守护进程那边的契约。
fn finish_hosted(app: &AppHandle, h: &HostShared, res: Result<(), AgentError>) {
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
        if let Some(st) = state.act.control.handback(&h.console_id) {
            act::emit_control(app, &h.console_id, &state.act.control.holders_of(&h.console_id), st);
        }
        h.send(HostEvent::Handback);
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
    h.cancel.store(true, Ordering::Relaxed);
    act::stop_run(&app, &state, h.run_id(), send_ctrl_c.unwrap_or(false)).await;
    // 马上进终态（UI 立刻看到「已停止」），再投一个 Stop 把挂起中的任务叫醒
    let _ = h.apply(HostEvent::Stop);
    h.send(HostEvent::Stop);
    Ok(true)
}

/// 答复卡片（问题 / 审批）。卡片 id 不对 / 已经答过 → Err。
#[tauri::command]
pub fn host_answer(
    contract_id: String,
    reply: CardReply,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let h = hosted(&state, &contract_id)?;
    match h.phase().base() {
        HostPhase::WaitingHuman { card } if card.id == reply.card_id => {}
        _ => return Err("这张卡片已经处理过了，或不是当前等你答复的那张".to_string()),
    }
    handback_if_driving(&app, &state, &h);
    h.send(HostEvent::Answer(reply));
    Ok(())
}

/// 接管：Agent 立刻让路（等同于在终端里敲了键），直到「交还」。
#[tauri::command]
pub fn host_takeover(contract_id: String, app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let h = hosted(&state, &contract_id)?;
    if let Some(st) = state.act.control.takeover(&h.console_id) {
        act::emit_control(&app, &h.console_id, &state.act.control.holders_of(&h.console_id), st);
    }
    h.send(HostEvent::Takeover);
    Ok(())
}

/// 交还：回到接管前的状态（不一定是运行中：预算暂停时接管，交还后仍是暂停）。
#[tauri::command]
pub fn host_handback(contract_id: String, app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let h = hosted(&state, &contract_id)?;
    if let Some(st) = state.act.control.handback(&h.console_id) {
        act::emit_control(&app, &h.console_id, &state.act.control.holders_of(&h.console_id), st);
    }
    h.send(HostEvent::Handback);
    Ok(())
}

/// 暂停：下一个闸口（下一次模型调用前 / 下一个动作前）停下，等「继续」。
#[tauri::command]
pub fn host_pause(contract_id: String, state: State<'_, AppState>) -> Result<(), String> {
    let h = hosted(&state, &contract_id)?;
    h.send(HostEvent::Pause);
    Ok(())
}

/// 加预算继续：只有暂停（预算 / 模型不可用 / 手动）才接受。
#[tauri::command]
pub fn host_resume(
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

#[tauri::command]
pub fn host_status(contract_id: String, state: State<'_, AppState>) -> Option<HostStatus> {
    state.act.host.get(&contract_id).map(|h| HostStatus::of(&h, &h.phase()))
}

/// 全部托管任务（含已结束、还没被同一 console 的新托管顶掉的）。
#[tauri::command]
pub fn host_list(state: State<'_, AppState>) -> Vec<HostStatus> {
    state.act.host.all().iter().map(|h| HostStatus::of(h, &h.phase())).collect()
}

#[cfg(test)]
mod tests {
    use super::super::host::PauseCause;
    use super::super::steward_loop::control_owned;
    use super::*;

    #[test]
    fn lease_is_held_before_contract_becomes_visible() {
        let table = ControlTable::new();
        let registry = StewardRegistry::new();
        let (c, changed) = claim_then_sign(&table, "t1", "host:c1", || {
            // 签约这一刻：守护进程的判定（PR1 的 `control_owned`）已经为真，它只会观察
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
