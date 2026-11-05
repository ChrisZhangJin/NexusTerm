//! 录制器（Phase 3.2）暴露给前端的命令。转换逻辑与状态见 `agent::recorder`。
//!
//! 命名必须是 snake_case（Tauri 2 直接拿函数名当 IPC 名字）。
//!
//! 状态变化（开始 / 结束 / 取消）会推 `nx://agent/recorder/{session}`，载荷是 [`RecorderStatus`]，
//! 供面板与终端工具栏显示 🔴 指示。草稿只通过 `agent_record_stop` 的返回值交给前端，
//! 后端不留存：留着的话，用户关掉草稿窗口后它就成了一份没人管的、可能含命令原文的内存残留。

use tauri::{AppHandle, Emitter, State};

use super::recorder::{self, RecorderStatus, RecordingDraft};
use super::timeline;
use crate::cmd::AppState;

fn publish_status(app: &AppHandle, status: &RecorderStatus) {
    let _ = app.emit(&recorder::event_name(&status.session), status);
}

/// 开始录制这个终端。`host_label` 是前端的标签名 / 主机名，只用于草稿里的来源说明。
/// 总开关未启用 / 已在录制中返回 `Err`。
#[tauri::command]
pub fn agent_record_start(
    session_id: String,
    host_label: Option<String>,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<RecorderStatus, String> {
    let (status, evs) = state
        .agent
        .recorder_start(&session_id, host_label.as_deref().unwrap_or(""))?;
    timeline::publish(&app, &session_id, &evs);
    publish_status(&app, &status);
    Ok(status)
}

/// 结束录制并返回草稿（见 `recorder::RecordingDraft`）。没在录制返回 `Err`。
#[tauri::command]
pub fn agent_record_stop(
    session_id: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<RecordingDraft, String> {
    let (draft, evs) = state.agent.recorder_stop(&session_id)?;
    timeline::publish(&app, &session_id, &evs);
    publish_status(&app, &state.agent.recorder_status(&session_id));
    Ok(draft)
}

/// 当前录制状态（前端挂载 / 切会话时对一下，事件只管增量）。
#[tauri::command]
pub fn agent_record_status(session_id: String, state: State<'_, AppState>) -> RecorderStatus {
    state.agent.recorder_status(&session_id)
}

/// 放弃录制，不产出草稿。返回之前是否在录制。
#[tauri::command]
pub fn agent_record_cancel(session_id: String, state: State<'_, AppState>, app: AppHandle) -> bool {
    let was = state.agent.recorder_cancel(&session_id);
    if was {
        publish_status(&app, &state.agent.recorder_status(&session_id));
    }
    was
}

/// 前端上报一次「没有文字的用户操作」（Ctrl-C / Ctrl-D / Ctrl-Z / 空回车）。
/// 只在录制中、且 `keys` 在白名单内才会记，其余一律忽略。fire-and-forget。
#[tauri::command]
pub fn agent_user_keys(session_id: String, keys: String, state: State<'_, AppState>, app: AppHandle) {
    let evs = state.agent.recorder_user_keys(&session_id, &keys);
    timeline::publish(&app, &session_id, &evs);
}

/// 给 `agent_user_command` 用：录制期间在用户提交一行**之前**记下「这一行是在回答什么提示」。
/// 没在录制时只是一次 map 查询。
pub fn capture_context(app: &AppHandle, state: &AppState, session_id: &str) {
    let evs = state.agent.recorder_user_context(session_id);
    timeline::publish(app, session_id, &evs);
}
