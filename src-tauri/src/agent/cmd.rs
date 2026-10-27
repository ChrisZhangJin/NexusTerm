//! 暴露给前端的 AI 助手命令。
//!
//! 命名必须是 snake_case：Tauri 2 直接拿函数名当 IPC 名字，
//! 而前端是按 `agent_get_config` 这种形状调用的。

use tauri::{AppHandle, Manager, State, WebviewUrl, WebviewWindowBuilder};

use super::act::{self, AgentRunRequest, ExecResult, OpenKind, OpenSpec};
use super::{capture, load_config, save_config, AgentConfig, AgentError};
use crate::agent::buffer::BufferStats;
use crate::agent::llm;
use crate::cmd::AppState;

/// 浮动 Agent 窗口的固定 label。只开一个：再次调用就聚焦并切会话，不叠窗。
const AGENT_WINDOW_LABEL: &str = "agent-float";

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
    let acct = config.active_account();
    tracing::info!(
        "AI 配置已更新：enabled={} accounts={} active={} provider={} model={}",
        config.enabled,
        config.accounts.len(),
        config.active_id,
        acct.provider,
        acct.model
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
    let max = max_bytes
        .filter(|v| *v > 0)
        .unwrap_or_else(|| load_config(&state.store).active_account().max_context_bytes)
        as usize;
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

// ——— Agentic：让 Agent 真的去终端里执行 ———

/// 发起一轮「会动手」的 Agentic 问答。**会一直等到这一轮跑完才返回**，
/// 过程中什么都不通过返回值给，全部走事件：
///
/// - `nx://agent/step/{session_id}`    → 每一步：下令 / 结果 / 跳过 / 提示 / 结论
/// - `nx://agent/approve/{session_id}` → 请用户批准一条危险命令（审批模式下才会出现）
/// - `nx://agent/error/{session_id}`   → 终态错误（其后不再发任何事件）
///
/// 不像 `agent_ask` 那样立即返回：「模型思考 → 写入终端 → 读回输出」是一条
/// 不可打乱的时间线，提前返回反而要另起一堆并发状态去同步它。
///
/// 同一会话同时只允许一轮。已有轮在跑时返回 `Err`，前端应提示用户先取消。
#[tauri::command]
pub async fn agent_run(
    request: AgentRunRequest,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<(), String> {
    let config = load_config(&state.store);
    act::run(&app, &state, &state.act, &config, &request)
        .await
        .map_err(err)
}

/// 取消该会话正在进行的这一轮 Agentic 运行（中止整条连锁操作）。
///
/// 幂等：没有在跑时什么都不做 —— 用户连点两次不该弹错误。
#[tauri::command]
pub fn agent_run_cancel(session_id: String, state: State<'_, AppState>) -> Result<(), String> {
    if session_id.trim().is_empty() {
        return Err(AgentError::NoSession(session_id).to_string());
    }
    if state.act.cancel(&session_id) {
        tracing::info!("Agentic 运行已请求取消：{}", session_id);
    } else {
        tracing::debug!("取消请求被忽略，该会话没有进行中的 Agentic 运行：{}", session_id);
    }
    Ok(())
}

/// 答复一次审批请求。`approved = false` 或请求已超时，都按「不执行」处理。
///
/// 返回 false 表示**这个请求已经没人接了**（多半是超时或答复晚到了）：
/// 那不是错误，前端只要把那张卡片置灰即可。
#[tauri::command]
pub fn agent_approve(
    request_id: String,
    approved: bool,
    state: State<'_, AppState>,
) -> Result<bool, String> {
    Ok(state.act.answer(&request_id, approved))
}

/// 前端对一次标签页动作的回执。
///
/// 必须回：后端在等它才能把「办成了 / 办砸了 / 没人接」区分开。
/// 返回 false 表示这个请求已经超时被丢掉了 —— 前端把卡片置灰即可，不是错误。
#[tauri::command]
pub fn agent_tab_ack(
    request_id: String,
    ok: bool,
    detail: Option<String>,
    tabs: Option<Vec<crate::agent::act::TabInfo>>,
    state: State<'_, AppState>,
) -> Result<bool, String> {
    Ok(state.act.answer_tab(
        &request_id,
        crate::agent::act::TabReply {
            ok,
            detail: detail.unwrap_or_default(),
            tabs: tabs.unwrap_or_default(),
        },
    ))
}

/// 在指定会话里手动跑一条命令（UI 上的「帮我执行」按钮、结果卡重试）。
///
/// 与 `agent_run` 共用同一条执行链路，但不调用模型：用户点哪条就跑哪条。
#[tauri::command]
pub async fn agent_exec(
    session_id: String,
    command: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<ExecResult, String> {
    let config = load_config(&state.store);
    if !config.enabled {
        return Err(AgentError::Disabled.to_string());
    }
    let outcome = act::run_one(
        &state,
        &app,
        &session_id,
        &command,
        0,
        "当前会话",
        &session_id,
        None,
        false,
    )
    .await
    .map_err(|e| {
        tracing::error!("Agent 执行命令失败：{}", e);
        e
    })?;
    Ok(ExecResult {
        output: outcome.output,
        reason: outcome.reason,
    })
}

// ——— 导出 / 开新会话（供前端按钮直接调用，与 agent_run 内部走的是同一套实现） ———

/// 导出一个会话迄今为止的全部输出。
///
/// `deep=false`：只给旁路缓冲里的内容（不受 `agent_context` 的字节上限约束）。
/// `deep=true`：额外追加 tmux pane / docker 日志，尽力而为，探测失败不算错误。
///
/// 一无所获时返回空字符串而不报错：「没有可导出的内容」是合法答案。
#[tauri::command]
pub async fn agent_export_session(
    session_id: String,
    deep: bool,
    state: State<'_, AppState>,
) -> Result<String, String> {
    Ok(capture::export_session(&state, &session_id, deep)
        .await
        .unwrap_or_default())
}

/// 开一个新会话：本地 shell，或连到一个已保存的 SSH 主机，可选顺手跑一条命令。
///
/// 新会话 id 由后端铸造，成功后会发出 `nx://agent/session-opened/{origin_session_id}`，
/// 前端据此为它接一个真正的标签页。`kind="ssh"` 时 `session_record_id` 必须指向
/// 一个已保存的会话配置 —— 这里不接受调用方直接给主机/用户名/密码。
#[tauri::command]
pub async fn agent_open_session(
    origin_session_id: String,
    kind: String,
    session_record_id: Option<String>,
    label: String,
    command: Option<String>,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<String, String> {
    let kind = match kind.as_str() {
        "ssh" => {
            let id = session_record_id
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| "kind=ssh 必须提供 session_record_id".to_string())?;
            OpenKind::Ssh { session_record_id: id }
        }
        _ => OpenKind::Local,
    };
    let spec = OpenSpec { kind, label, command };
    let outcome = act::open_session(&app, &state, &origin_session_id, spec)
        .await
        .map_err(err)?;
    Ok(outcome.session_id)
}

// ——— 浮动独立窗口 ———

/// 把 Nexus AI Agent 面板浮出为一个独立系统窗口。
///
/// 复用同一个 label，不叠窗。加载的仍是本应用 index.html，靠注入脚本
/// `window.__NX_AGENT__ = sessionId` 让前端切到「只渲染 Agent 面板」的界面。
#[tauri::command]
pub fn agent_open_window(session_id: String, app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window(AGENT_WINDOW_LABEL) {
        use tauri::Emitter;
        let _ = win.emit("nx://agent/active-session", session_id);
        let _ = win.set_focus();
        let _ = win.show();
        return Ok(());
    }

    let init = format!(
        "window.__NX_AGENT__ = {};",
        serde_json::to_string(&session_id).unwrap_or_else(|_| "\"\"".into())
    );
    WebviewWindowBuilder::new(&app, AGENT_WINDOW_LABEL, WebviewUrl::App("index.html".into()))
        .title("Nexus AI Agent")
        .inner_size(420.0, 640.0)
        .min_inner_size(320.0, 420.0)
        .initialization_script(&init)
        .build()
        .map(|_| ())
        .map_err(|e| format!("无法打开 Nexus AI Agent 窗口：{e}"))
}

/// 关闭浮动 Agent 窗口（若存在）。幂等。
#[tauri::command]
pub fn agent_close_window(app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window(AGENT_WINDOW_LABEL) {
        let _ = win.close();
    }
    Ok(())
}

/// 浮动窗口是否已打开。主窗口据此决定是收起内嵌面板还是正常显示。
#[tauri::command]
pub fn agent_window_open(app: AppHandle) -> bool {
    app.get_webview_window(AGENT_WINDOW_LABEL).is_some()
}
