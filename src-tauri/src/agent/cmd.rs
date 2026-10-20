//! 暴露给前端的 AI 助手命令。
//!
//! 命名必须是 snake_case：Tauri 2 直接拿函数名当 IPC 名字，
//! 而前端是按 `agent_get_config` 这种形状调用的。

use tauri::{AppHandle, State};

use super::{load_config, save_config, AgentConfig, AgentError};
use crate::agent::buffer::BufferStats;
use crate::agent::llm;
use crate::cmd::AppState;

fn err(e: AgentError) -> String {
    tracing::error!("{}", e);
    e.to_string()
}

/// 读取 AI 配置（缺失或坏掉时返回默认值）。
#[tauri::command]
pub fn agent_get_config(state: State<'_, AppState>) -> AgentConfig {
    load_config(&state.store)
}

/// 保存 AI 配置，并把全局开关同步进内存里的 AgentManager。
#[tauri::command]
pub fn agent_set_config(
    config: AgentConfig,
    state: State<'_, AppState>,
) -> Result<(), String> {
    save_config(&state.store, &config).map_err(err)?;
    // 写盘成功才算生效：db 里的值和内存里的原子开关必须保持一致
    state.agent.apply_config(&config);
    tracing::info!(
        "AI 配置已更新：enabled={} provider={} model={}",
        config.enabled,
        config.provider,
        config.model
    );
    Ok(())
}

/// 开关某个会话的旁路。
#[tauri::command]
pub fn agent_set_session(
    session_id: String,
    enabled: bool,
    state: State<'_, AppState>,
) -> Result<(), String> {
    if session_id.is_empty() {
        return Err(AgentError::NoSession(session_id).to_string());
    }
    state.agent.set_enabled(&session_id, enabled);
    Ok(())
}

/// 该会话是否在旁路。
#[tauri::command]
pub fn agent_get_session(session_id: String, state: State<'_, AppState>) -> bool {
    state.agent.is_enabled(&session_id)
}

/// 取该会话最近的上下文（给 UI 展示 / 手动构造 prompt 用）。
///
/// `max_bytes` 省略时用配置里的 `maxContextBytes`。
/// 返回的文本**未脱敏** —— 展示需要原文，
/// 真正的脱敏在调用 LLM 的那一层出网前做（见 `agent::redact`）。
#[tauri::command]
pub fn agent_context(
    session_id: String,
    max_bytes: Option<u32>,
    state: State<'_, AppState>,
) -> Result<String, String> {
    // 未指定 / 传 0 时回落到配置里的默认窗口
    let max = max_bytes
        .filter(|v| *v > 0)
        .unwrap_or_else(|| load_config(&state.store).max_context_bytes) as usize;
    Ok(state.agent.context(&session_id, max).unwrap_or_default())
}

/// 清空该会话已缓冲的上下文（相当于「让 agent 忘掉刚才的事」）。
#[tauri::command]
pub fn agent_clear(session_id: String, state: State<'_, AppState>) -> Result<(), String> {
    state.agent.clear(&session_id);
    Ok(())
}

/// 缓冲统计（字节数 / 累计丢弃量 / 是否已被截断）。
#[tauri::command]
pub fn agent_stats(session_id: String, state: State<'_, AppState>) -> Option<BufferStats> {
    state.agent.stats(&session_id)
}

// ——— 提问 / 取消 / 测试连接 ———

/// 向绑定在该会话上的 AI 助手提问。
///
/// **立即返回**，答案不通过返回值给，而是流式推事件：
///
/// - `nx://agent/token/{session_id}` → `String`，增量文本块
/// - `nx://agent/done/{session_id}`  → `{ reason: "stop" | "length" | "cancelled" }`
/// - `nx://agent/error/{session_id}` → `String`，用户可读的中文错误（终态，其后不再发 done）
///
/// 同一会话同时只允许一个流；已有流在跑时直接返回 `Err`，前端应提示用户先取消。
/// 上下文在返回之前就取完并脱敏完毕，锁不会跨越网络 I/O。
#[tauri::command]
pub async fn agent_ask(
    session_id: String,
    question: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<(), String> {
    let config = load_config(&state.store);
    state
        .llm
        .ask(&app, &state.agent, &config, &session_id, &question)
        .map_err(err)
}

/// 取消该会话正在进行的提问。
///
/// 置位后流会在下一个网络包处停下，并发 `done/cancelled`。
/// 没有进行中的提问时什么都不做（幂等）—— 用户连点两次不该弹错误。
#[tauri::command]
pub fn agent_cancel(session_id: String, state: State<'_, AppState>) -> Result<(), String> {
    if session_id.trim().is_empty() {
        return Err(AgentError::NoSession(session_id).to_string());
    }
    if state.llm.cancel(&session_id) {
        tracing::info!("AI 提问已请求取消：{}", session_id);
    } else {
        tracing::debug!("AI 取消请求被忽略，该会话没有进行中的提问：{}", session_id);
    }
    Ok(())
}

/// 用一条极短消息验证 Key / 地址 / 模型是否可用（设置界面「测试连接」按钮）。
///
/// 刻意不接 `State`：测的是用户**刚填进去、还没保存**的那份配置，
/// 与库里已保存的值、与运行时状态都无关。返回成功文案而非布尔值，
/// 是为了让用户看到模型真的回话了。
#[tauri::command]
pub async fn agent_test(config: AgentConfig) -> Result<String, String> {
    llm::test_connection(&config).await.map_err(err)
}
