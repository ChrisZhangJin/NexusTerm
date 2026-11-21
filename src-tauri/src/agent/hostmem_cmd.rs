//! 主机记忆（Phase 4.5）暴露给前端的命令。模型与规则见 `agent::hostmem`，存储见 `store::host_memory`。
//!
//! 前端只认**会话（标签）id + 标签名**，主机键一律由后端按登记表推导（`hostmem::resolve_host`），
//! 前端不自己拼键——两边各算一份迟早对不上。用户可以编辑 / 删除任何一条（包括 Agent 写的）。

use serde::Serialize;
use tauri::State;

use super::hostmem::{self, HostNote, HostRef, Kind, Source};
use crate::cmd::AppState;

/// 面板「主机备忘」对话框一次要的全部数据。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostMemoryView {
    /// 认不出主机（标签名也是空）时为 `None`，界面据此提示「无法记忆」
    pub host: Option<HostRef>,
    pub notes: Vec<HostNote>,
    pub max_notes: usize,
    pub max_chars: usize,
}

fn view(state: &AppState, session_id: &str, label: &str) -> HostMemoryView {
    let host = hostmem::resolve_host(state, session_id, label);
    let notes = host
        .as_ref()
        .map(|h| state.store.host_memory_list(&h.key))
        .unwrap_or_default();
    HostMemoryView {
        host,
        notes,
        max_notes: hostmem::MAX_NOTES_PER_HOST,
        max_chars: hostmem::MAX_NOTE_CHARS,
    }
}

fn parse_kind(kind: &str) -> Result<Kind, String> {
    Kind::parse(kind.trim()).ok_or_else(|| format!("未知类别：{kind}"))
}

/// 当前标签所在主机的备忘。
#[tauri::command]
pub fn host_memory_list(session_id: String, label: String, state: State<'_, AppState>) -> HostMemoryView {
    view(&state, &session_id, &label)
}

/// 用户新增一条（`source = user`）。
#[tauri::command]
pub fn host_memory_add(
    session_id: String,
    label: String,
    text: String,
    kind: String,
    state: State<'_, AppState>,
) -> Result<HostNote, String> {
    let host = hostmem::resolve_host(&state, &session_id, &label)
        .ok_or_else(|| "无法确定这个标签属于哪台主机".to_string())?;
    state
        .store
        .host_memory_add(&host.key, &text, parse_kind(&kind)?, Source::User)
        .map(|(n, _)| n)
}

/// 用户编辑一条：改文本 / 类别，作者变为用户。
#[tauri::command]
pub fn host_memory_update(
    id: String,
    text: String,
    kind: String,
    state: State<'_, AppState>,
) -> Result<HostNote, String> {
    state.store.host_memory_update(&id, &text, parse_kind(&kind)?)
}

/// 用户删除一条（任何作者的都行）。
#[tauri::command]
pub fn host_memory_delete(id: String, state: State<'_, AppState>) -> Result<(), String> {
    state.store.host_memory_delete(None, &id, Source::User).map(|_| ())
}

/// 清空当前标签所在主机的全部备忘，返回删了几条。
#[tauri::command]
pub fn host_memory_clear(session_id: String, label: String, state: State<'_, AppState>) -> Result<usize, String> {
    let host = hostmem::resolve_host(&state, &session_id, &label)
        .ok_or_else(|| "无法确定这个标签属于哪台主机".to_string())?;
    state.store.host_memory_clear_host(&host.key)
}
