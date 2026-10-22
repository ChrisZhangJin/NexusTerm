//! Nexus AI Agent 的动作层：让模型不只是「读」终端，而是能真的「动手」。
//!
//! `llm.rs` 解决「怎么把答案一个字一个字推回来」，`executor.rs` 解决
//! 「一条命令怎么算跑完」，这一层解决的是中间那段编排：
//!
//! ```text
//! 提问 → 模型决定做点什么 → 真的去做 → 把终端回了什么续喂回去 → 再决定 → … → 给结论
//! ```
//!
//! ## 动作协议
//!
//! 模型要执行东西，就在回复里写这样一段围栏代码块：
//!
//! ````text
//! ```nx-exec
//! {"target":"<会话 id>","command":"df -h"}
//! ```
//! ````
//!
//! 之所以选「围栏代码块里放 JSON」而不是函数调用 tool-calls：后者各家方言
//! 差异极大（OpenAI / Anthropic / 各种兼容层各写各的），而围栏代码块只有一种写法，
//! 任何能说人话的模型都写得出来 —— 这才是对未知模型最稳的那一版。
//! 一个块里也可以给 `targets` + `commands` 两个数组，一次对多台机器下发同一批命令。
//! 不写 `target` 就默认落到「当前会话」。
//!
//! ## 三条不可妥协的约束
//!
//! 1. **命令必须是单行的**。换行意味着「一次注入一串互不相干的操作」，
//!    那样既没法逐条审批，也彻底失去了「哪一步炸了」的可追溯性。要分几步请拆成几轮。
//! 2. **人在回路是可选项，不是默认值**。默认 `ExecMode::Approval`：
//!    被 `danger::assess` 判为危险的命令会先在 UI 上弹出来等人点头。
//!    审批是有超时的 —— 人去接杯咖啡回来发现流程卡死，比「超时拒绝」糟糕得多。
//! 3. **任何一步失败都不能让整轮蒸发**。执行报错会当成一条反馈续喂给模型，
//!    让它自己决定重试、换命令还是收手；退出码非 0 也照常喂回去 ——
//!    失败的输出往往比成功的信息量更大。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use super::danger::assess;
use super::executor::{
    strip_sentinel_wrappers, wait_for_end, ExecEnd, ExecOutcome, Sentinel, WaitConfig,
};
use super::llm::{self, ChatMessage, CONTEXT_BEGIN, CONTEXT_END};
use super::{AgentConfig, AgentError};
use crate::agent::StreamKind;
use crate::cmd::AppState;

/// 一次 run 最多允许多少轮「思考 → 动作 → 再思考」。
///
/// 取 8：真正在排查问题时通常 3~5 步就收敛了；再多说明模型在打转，
/// 让它继续跑只会烧钱，不如停下来把现状交给人。
const MAX_STEPS: usize = 8;

/// 单一步骤里最多允许多少个动作（防止模型一口气吐出五十条命令）。
const MAX_ACTIONS: usize = 12;

/// 人工审批的最长等待。超时按「拒绝」处理。
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

/// 喂回给模型的单条执行反馈的字符上限。
///
/// 一条 `tail -f` 或 `dmesg` 轻松上千行，全都续喂回去第二轮就爆上下文了。
/// 保留头部（最有价值的通常在前面）而不是尾部。
const MAX_FEEDBACK_CHARS: usize = 6000;

// ——————————————————————————————— 协议类型 ———————————————————————————————

/// 一个可操作目标：**会话 id + 给人看的名字**。
///
/// 名字由前端给：后端只知道一串 uuid，把它显示在可执行文件ಕ್ಕ旁边毫无意义。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTarget {
    pub id: String,
    pub label: String,
}

/// 一次 Agentic 运行的请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRunRequest {
    /// 面板当前绑定的会话（同时是省略 target 时的默认目标）
    pub session_id: String,
    pub question: String,
    /// 允许操作的会话。为空时退回 `[session_id]`。
    #[serde(default)]
    pub targets: Vec<AgentTarget>,
}

/// 模型下发的一个动作：在哪个会话上跑什么命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentAction {
    /// `None` = 默认会话
    pub target: Option<String>,
    pub command: String,
}

/// 围栏代码块内部那一坨 JSON 的形状。字段全可选，是为了让模型怎么写都能接住。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExecBlock {
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    targets: Vec<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    commands: Vec<String>,
}

// ——————————————————————————————— 事件载荷 ———————————————————————————————

/// `nx://agent/step/{session}` 的载荷：一轮里发生的每一件事。
///
/// 刻意做成「一个扁平结构 + kind」而不是 tagged enum：TS 侧判别联合要写模板代码，
/// 而这里的多数字段本来就是可选的，扁平结构反而更好渲染。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepPayload {
    /// `plan` | `exec` | `result` | `skip` | `note` | `answer`
    pub kind: &'static str,
    pub step: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasons: Option<Vec<String>>,
}

impl StepPayload {
    fn new(kind: &'static str, step: usize) -> Self {
        Self {
            kind,
            step,
            target: None,
            target_label: None,
            command: None,
            output: None,
            exit_code: None,
            text: None,
            reasons: None,
        }
    }
}

/// `nx://agent/approve/{session}` 的载荷：请人点头。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovePayload {
    pub request_id: String,
    pub command: String,
    pub target: String,
    pub target_label: String,
    pub reasons: Vec<String>,
}

/// 给 UI 展示的单次执行结果（手动执行按钮也用它）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecResult {
    pub output: String,
    pub exit_code: Option<i32>,
    pub reason: ExecEnd,
}

fn emit_step(app: &AppHandle, session: &str, payload: StepPayload) {
    let _ = app.emit(&format!("nx://agent/step/{session}"), payload);
}

// ——————————————————————————————— 动作解析 ———————————————————————————————

/// 围栏块的开始标记。认它时也接受模型自作主张加的语言后缀（```nx-exec json）。
const FENCE_OPEN: &str = "```nx-exec";

/// 从一段模型回复里抽出所有动作。
///
/// 只认 ```nx-exec 围栏：普通代码块（常常是模型给用户抄的命令）一律不动，
/// 否则「请把这段命令贴进终端」这类善意的回复会被误当成「请帮我执行」。
pub fn extract_actions(text: &str) -> Vec<AgentAction> {
    let mut out = Vec::new();
    let mut inside = false;
    let mut buf = String::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if !inside {
            // 围栏开放行。模型常在语言位多写 ``json`` / 空格，starts_with 天然容得下。
            if trimmed.starts_with(FENCE_OPEN) {
                inside = true;
                buf.clear();
            }
            continue;
        }
        if trimmed.starts_with("```") {
            inside = false;
            out.extend(parse_block(&buf));
            continue;
        }
        buf.push_str(line);
        buf.push('\n');
    }
    // 围栏没收尾（模型被 max_tokens 截断）时，块里已有的东西也值得一试：
    // 一条残缺的 JSON 解不出来会被静默丢弃，不会因此误执行任何东西。
    if inside {
        out.extend(parse_block(&buf));
    }

    out.truncate(MAX_ACTIONS);
    out
}

/// 解析一个块。JSON 坏了就返回空 —— **宁可什么都不做，也不要猜**。
fn parse_block(raw: &str) -> Vec<AgentAction> {
    let Ok(block) = serde_json::from_str::<ExecBlock>(raw.trim()) else {
        return Vec::new();
    };

    let targets = if block.targets.is_empty() {
        block.target.into_iter().collect::<Vec<_>>()
    } else {
        block.targets
    };
    let commands = if block.commands.is_empty() {
        block.command.into_iter().collect::<Vec<_>>()
    } else {
        block.commands
    };
    if commands.is_empty() {
        return Vec::new();
    }

    // target 为空 → None（走默认会话）；否则逐个铺开。
    // 顺序是「目标在外、命令在内」：同一台机器上的几条命令连着跑，
    // 比来回切换目标更少打乱 shell 状态。
    let mut out = Vec::new();
    if targets.is_empty() {
        for c in &commands {
            out.push(AgentAction { target: None, command: c.clone() });
        }
    } else {
        for t in &targets {
            for c in &commands {
                out.push(AgentAction { target: Some(t.clone()), command: c.clone() });
            }
        }
    }
    out
}

/// 校验一条命令能不能下发。返回可读的拒绝理由。
///
/// 拒绝换行是这里最重要的一条：一次注入一串操作会让「逐步审批」彻底失效 ——
/// 人看到的永远只是一句话，而实际执行的可能有五条。
pub fn reject_command(cmd: &str) -> Option<&'static str> {
    // 只按空格与制表符判空：`\n` / `\r` 也要算「有东西」，否则会被当成空命令放过
    if cmd.trim_matches(|c: char| c == ' ' || c == '\t').is_empty() {
        return Some("命令为空");
    }
    // 换行意味着「一次注入一串互不相干的操作」：既没法逐条审批，
    // 也彻底失去「哪一步炸了」的可追溯性。要分几步请拆成几轮。
    // ESC / BEL / NUL 等其余控制字符同理 —— 它们能在屏幕上改写这条命令的显示，
    // 让人看到 `ls` 而实际跑的是别的。必须挡在协议这一层。
    if cmd.chars().any(|c| c.is_control() && c != '\t') {
        return Some("命令含换行或控制字符，必须拆成单条执行");
    }
    None
}

// ——————————————————————————————— 系统提示词 ———————————————————————————————

/// Agentic 模式专用系统提示词。
///
/// 与「只读问答」的兜底提示词最大的不同：这里要求模型**动手**，
/// 所以必须把协议、边界、以及「什么时候该停」讲得毫不含糊 ——
/// 说不清楚的话，模型会在「再查一条」上无限打转。
pub const AGENTIC_SYSTEM_PROMPT: &str = concat!(
    "你是 NexusTerm 的终端 Agent，可以直接在用户的终端会话里执行命令。",
    "用户会给出若干个会话及其最近输出记录，其中 `$ ` 开头的是人敲进去的命令，",
    "其余是程序输出（已剥离 ANSI 转义）。\n\n",
    "【要执行东西时】在回复里写且只写这样一个围栏代码块：\n",
    "```nx-exec\n{\"target\":\"<会话id>\",\"command\":\"<命令>\"}\n```\n",
    "一次要对多台机器发同一批命令时，用 targets / commands 两个数组：\n",
    "```nx-exec\n{\"targets\":[\"id1\",\"id2\"],\"commands\":[\"uptime\",\"df -h\"]}\n```\n",
    "不写 target 表示落到用户当前所在的会话。\n\n",
    "【规则】\n",
    "1) 一个块里的 command 必须是**单条单行**命令，不许用换行或 && 串联互不相关的操作；",
    "需要分几步就分几轮，每轮都会收到上一条的真实输出；\n",
    "2) 一次最多给 12 条动作，够用为止，不要贪多；\n",
    "3) 优先用只读命令（ls / cat / grep / ss / df / ps / journalctl 等）把情况看清楚，",
    "确有必要才做写操作，并在下达前用一句话说明为什么要做、影响面多大；\n",
    "4) 命令的输出与退出码会原样回给你，**退出码非 0 不代表流程失败** ——",
    "照着真实输出继续判断，不要假装没看到；\n",
    "5) 不要执行不可逆操作（rm -rf、mkfs、DROP、chmod -R 777、把未知脚本管道给 shell 等），",
    "除非用户在这一轮对话里明确要求；\n",
    "6) 已经查清楚就不要再查了：直接用中文给出结论，说明依据与必要的后续建议；\n",
    "7) 记录缺失或被截断就直说看不到，不要臆测终端里发生过什么。"
);

// ——————————————————————————————— 上下文组装 ———————————————————————————————

/// 第一轮 user 消息：把所有目标会话的上下文与问题拼在一起。
fn plan_message(state: &AppState, req: &AgentRunRequest, targets: &[AgentTarget], max_bytes: usize) -> String {
    let mut msg = String::from(
        "以下是你可以操作的终端会话及其最近输出记录（`$ ` 开头是命令，其余是输出）：\n",
    );
    for t in targets {
        let raw = state.agent.context(&t.id, max_bytes).unwrap_or_default();
        // 擦掉过去几轮的哨兵包装痕迹：那是实现细节，不该占模型的注意力
        let ctx = strip_sentinel_wrappers(&raw);
        msg.push_str(&format!(
            "\n===== 会话 {} ({}) =====\n{}{}\n{}\n",
            t.label,
            t.id,
            CONTEXT_BEGIN,
            if ctx.trim().is_empty() {
                "（暂无可用记录：会话旁路未开启或已被清空）".to_string()
            } else {
                ctx
            },
            CONTEXT_END,
        ));
    }
    msg.push_str(&format!(
        "\n目标：{}\n\n需要执行操作就用 ```nx-exec 代码块，已经查清就直接给结论。",
        req.question.trim()
    ));
    msg
}

/// 把一次执行的原文结果变成喂回模型的反馈。
fn feedback_for(action: &AgentAction, label: &str, outcome: &ExecOutcome) -> String {
    let mut s = String::new();
    s.push_str(&format!("在会话「{label}」执行：`{}`\n", action.command.trim()));
    match outcome.reason {
        ExecEnd::Sentinel => s.push_str(&format!(
            "退出码：{}\n",
            outcome
                .exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "未知".into())
        )),
        ExecEnd::Silence => s.push_str("退出码：未知（长时间没有新输出，按执行完毕处理）\n"),
        ExecEnd::Timeout => s.push_str("退出码：未知（等待超时，命令可能仍在运行）\n"),
        ExecEnd::Cancelled => return format!("{s}结果：已被用户取消，不要重试这条命令。\n"),
    }
    // 保留头部：报错与关键信息通常在最前面，尾部往往是无关的重复
    s.push_str(&format!("输出：\n{}\n", head_chars(&outcome.output, MAX_FEEDBACK_CHARS)));
    s
}

/// 也有可能执行本身就没跑起来（会话关了、拒绝-ID 不存在）。
fn failure_feedback(action: &AgentAction, label: &str, why: &str) -> String {
    format!(
        "在会话「{label}」执行 `{}` 失败：{}\n请据此调整：换一条命令、换个会话，或直接给结论。\n",
        action.command.trim(),
        why
    )
}

/// 按**字符**截断并给出提示。按 `char` 而不是字节，否则中文会切出半个字。
fn head_chars(s: &str, max: usize) -> String {
    let total = s.chars().count();
    if total <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}\n……（输出过长，已截断 {} / {total} 字符）", total - max)
}

// ——————————————————————————————— 写入通道 ———————————————————————————————

/// 会话的写入通道：本地 PTY 还是 SSH。
enum Sink {
    Local,
    Ssh,
}

/// 判断一个 id 指向哪种会话。**先本地后 SSH**：两者的 id 都由前端生成且互不重叠，
/// 本地是更常见的那一种（启动就是一个本地标签页）。
fn sink_of(state: &AppState, id: &str) -> Option<Sink> {
    if state.local.contains(id) {
        return Some(Sink::Local);
    }
    if state.sessions.contains(id) {
        return Some(Sink::Ssh);
    }
    None
}

/// 往任意一种会话写字节。会话不存在时才报错。
async fn write_to(state: &AppState, id: &str, data: &[u8]) -> Result<(), String> {
    match sink_of(state, id) {
        Some(Sink::Local) => state.local.write(id, data),
        Some(Sink::Ssh) => state
            .sessions
            .write(id, data.to_vec())
            .await
            .map_err(|e| e.to_string()),
        None => Err(format!("会话不存在或已关闭：{id}")),
    }
}

// ——————————————————————————————— 单条命令执行 ———————————————————————————————

/// 在目标会话上跑一条命令，返回输出与退出码。
///
/// **会把这条命令原样旁路进 agent 缓冲**：一方面让 `pending_echo` 抵消掉 pty 的回显，
/// 另一方面留下「这一轮到底干了什么」的完整审计。
pub async fn run_one(
    state: &AppState,
    app: &AppHandle,
    session_id: &str,
    command: &str,
    step: usize,
    label: &str,
    target_id: &str,
    cancel: Option<&AtomicBool>,
    announce: bool,
) -> Result<ExecOutcome, String> {
    if let Some(why) = reject_command(command) {
        return Err(why.to_string());
    }
    let cmd = command.trim();

    // 要让这条命令的输出进得了旁路，缓冲必须是开着的。
    // 这里是「Agent 主动要用某个会话」，与全局 autoAttach 无关，故显式打开。
    state.agent.set_enabled(session_id, true);

    let sentinel = Sentinel::new();
    let payload = sentinel.wrap(cmd);
    write_to(state, session_id, payload.as_bytes()).await?;
    // 喂的是**包裹后**的原始字节：这样 pty 的回显才能被 pending_echo 逐字节抵消干净
    state.agent.feed(session_id, payload.as_bytes(), StreamKind::In);

    if announce {
        emit_step(
            app,
            session_id,
            StepPayload {
                target: Some(target_id.into()),
                target_label: Some(label.into()),
                command: Some(cmd.into()),
                ..StepPayload::new("exec", step)
            },
        );
    }

    let baseline = state.agent.snapshot(session_id).unwrap_or_default();
    let delta = || {
        let now = state.agent.snapshot(session_id).unwrap_or_default();
        match now.strip_prefix(baseline.as_str()) {
            Some(rest) => strip_sentinel_wrappers(rest),
            // 环形缓冲把开头丢掉了（超长输出）：基线不再是前缀，
            // 退回整份快照，兜里至少还能扫到哨兵。
            None => strip_sentinel_wrappers(&now),
        }
    };
    let outcome = wait_for_end(
        &sentinel,
        WaitConfig::default(),
        delta,
        || cancel.map(|c| c.load(Ordering::Relaxed)).unwrap_or(false),
        |d| Box::pin(tokio::time::sleep(d)) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
    )
    .await;

    if announce {
        emit_step(
            app,
            session_id,
            StepPayload {
                target: Some(target_id.into()),
                target_label: Some(label.into()),
                command: Some(cmd.into()),
                output: Some(outcome.output.clone()),
                exit_code: outcome.exit_code,
                ..StepPayload::new("result", step)
            },
        );
    }
    Ok(outcome)
}

// ——————————————————————————————— 审批 ———————————————————————————————

/// Agentic 运行的运行时：一个会话一轮访问的取消标记，以及等待人工批准的登记表。
pub struct ActRuntime {
    inflight: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// request_id → 谁来收「批准 / 拒绝」这个答复
    approvals: Mutex<HashMap<String, tokio::sync::oneshot::Sender<bool>>>,
}

impl ActRuntime {
    pub fn new() -> Self {
        Self {
            inflight: Mutex::new(HashMap::new()),
            approvals: Mutex::new(HashMap::new()),
        }
    }

    /// 登记一轮运行。返回 `None` 表示该会话已经有一轮在跑。
    fn begin(&self, session_id: &str) -> Option<Arc<AtomicBool>> {
        let mut guard = self.inflight.lock();
        if guard.contains_key(session_id) {
            return None;
        }
        let flag = Arc::new(AtomicBool::new(false));
        guard.insert(session_id.to_string(), Arc::clone(&flag));
        Some(flag)
    }

    fn finish(&self, session_id: &str) {
        self.inflight.lock().remove(session_id);
    }

    /// 取消某会话正在进行的一轮。返回是否确实有流被置位。
    pub fn cancel(&self, session_id: &str) -> bool {
        match self.inflight.lock().get(session_id) {
            Some(flag) => {
                flag.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    #[allow(dead_code)]
    pub fn is_running(&self, session_id: &str) -> bool {
        self.inflight.lock().contains_key(session_id)
    }

    /// 收下前端的审批答复。**找不到请求说明已经超时**，返回 false 让调用方如实告知。
    pub fn answer(&self, request_id: &str, approved: bool) -> bool {
        let Some(tx) = self.approvals.lock().remove(request_id) else {
            return false;
        };
        // 接收端已经因为超时被 drop 掉了 → send 会失败，同样是「没人接了」
        tx.send(approved).is_ok()
    }

    /// 挂一个待批请求，返回用来等答复的接收端。
    fn register(&self, request_id: &str) -> tokio::sync::oneshot::Receiver<bool> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.approvals.lock().insert(request_id.to_string(), tx);
        rx
    }

    /// 请求被应答或超时后，把登记清掉（对着这一次 request_id，不影响别的请求）。
    fn unregister(&self, request_id: &str) {
        self.approvals.lock().remove(request_id);
    }
}

impl Default for ActRuntime {
    fn default() -> Self {
        Self::new()
    }
}

/// 请人点头。超时（或 UI 崩了导致发送端被 drop）一律按**拒绝**处理。
async fn request_approval(
    app: &AppHandle,
    runtime: &ActRuntime,
    session_id: &str,
    payload: &ApprovePayload,
) -> bool {
    let rx = runtime.register(&payload.request_id);
    let _ = app.emit(&format!("nx://agent/approve/{session_id}"), payload.clone());

    let decision = match tokio::time::timeout(APPROVAL_TIMEOUT, rx).await {
        Ok(Ok(approved)) => approved,
        // Err = 超时；Ok(Err) = UI 那一头先没了（窗口关了 / 页面重载）
        _ => {
            tracing::warn!(
                "审批请求无人应答，按拒绝处理：{} {}",
                payload.request_id,
                payload.command
            );
            false
        }
    };
    runtime.unregister(&payload.request_id);
    decision
}

// ——————————————————————————————— Agentic 主循环 ———————————————————————————————

/// 目标列表的兜底：前端一个都没传时，用当前会话。
fn resolve_targets(req: &AgentRunRequest) -> Vec<AgentTarget> {
    if req.targets.is_empty() {
        vec![AgentTarget {
            id: req.session_id.clone(),
            label: "当前会话".into(),
        }]
    } else {
        req.targets.clone()
    }
}

/// 把 `target` 落到真实会话 id 与展示名上。
fn locate<'a>(id: Option<&str>, targets: &'a [AgentTarget], fallback: &str) -> (String, &'a str) {
    match id {
        Some(want) => targets
            .iter()
            .find(|t| t.id == want)
            .map(|t| (t.id.clone(), t.label.as_str()))
            .unwrap_or_else(|| (want.to_string(), "未知会话")),
        None => targets
            .iter()
            .find(|t| t.id == fallback)
            .map(|t| (t.id.clone(), t.label.as_str()))
            .unwrap_or_else(|| (fallback.to_string(), "当前会话")),
    }
}

/// 跑完一整轮 Agentic 问答。过程中只发事件，不通过返回值交付内容。
pub async fn run(
    app: &AppHandle,
    state: &AppState,
    runtime: &ActRuntime,
    cfg: &AgentConfig,
    req: &AgentRunRequest,
) -> Result<(), AgentError> {
    if req.session_id.trim().is_empty() {
        return Err(AgentError::NoSession(req.session_id.clone()));
    }
    if req.question.trim().is_empty() {
        return Err(AgentError::EmptyQuestion);
    }
    llm::validate(cfg)?;

    let Some(cancel) = runtime.begin(&req.session_id) else {
        return Err(AgentError::Busy);
    };
    let acct = cfg.active_account();
    let max_bytes = if acct.max_context_bytes == 0 {
        super::DEFAULT_MAX_CONTEXT_BYTES
    } else {
        acct.max_context_bytes
    } as usize;

    let targets = resolve_targets(req);
    let system = IfAgentic::pick(&acct.system_prompt);    let mut messages = vec![ChatMessage::user(plan_message(state, req, &targets, max_bytes))];

    let session = req.session_id.clone();
    let mut step = 0usize;
    let mut feedback = String::new();

    loop {
        if cancel.load(Ordering::Relaxed) {
            emit_step(app, &session, StepPayload { text: Some("已取消。".into()), ..StepPayload::new("note", step) });
            break;
        }
        if step >= MAX_STEPS {
            emit_step(
                app,
                &session,
                StepPayload {
                    text: Some(format!("已达到本轮步数上限（{MAX_STEPS} 步），先停下来：由你确认是否继续。")),
                    ..StepPayload::new("note", step)
                },
            );
            break;
        }
        step += 1;

        if !feedback.is_empty() {
            messages.push(ChatMessage::user(feedback.clone()));
            feedback.clear();
        }

        let reply = match llm::complete(acct, &system, &messages, Some(&cancel)).await {
            Ok(text) => text,
            Err(e) => {
                // 先把自己这一轮从登记表里摘掉，再报错 —— 否则会话会被永久卡住「运行中」
                runtime.finish(&session);
                let _ = app.emit(&format!("nx://agent/error/{session}"), e.to_string());
                return Err(e);
            }
        };
        messages.push(ChatMessage::assistant(reply.clone()));

        let actions = extract_actions(&reply);
        if actions.is_empty() {
            // 没有动作块 = 模型准备收尾了，这段纯文本就是给用户的结论
            let text = strip_action_blocks(&reply);
            if !text.trim().is_empty() {
                emit_step(app, &session, StepPayload { text: Some(text), ..StepPayload::new("answer", step) });
            }
            break;
        }

        for action in actions {
            if cancel.load(Ordering::Relaxed) {
                feedback.push_str("（用户取消了后续动作，请直接给出结论。）\n");
                break;
            }
            let (target_id, target_label) = locate(action.target.as_deref(), &targets, &session);
            // 一轮内的多个动作共用同一个 step 号：UI 按 step 折叠，同一步归在一张卡里

            if let Some(why) = reject_command(&action.command) {
                feedback.push_str(&failure_feedback(&action, target_label, why));
                continue;
            }

            let risk = assess(&action.command);
            if cfg.exec_mode.is_approval() && risk.dangerous {
                let request_id = uuid::Uuid::new_v4().simple().to_string();
                let req_payload = ApprovePayload {
                    request_id: request_id.clone(),
                    command: action.command.trim().into(),
                    target: target_id.clone(),
                    target_label: target_label.into(),
                    reasons: risk.reasons.clone(),
                };
                if !request_approval(app, runtime, &session, &req_payload).await {
                    emit_step(
                        app,
                        &session,
                        StepPayload {
                            target: Some(target_id.clone()),
                            target_label: Some(target_label.into()),
                            command: Some(action.command.trim().into()),
                            reasons: Some(risk.reasons.clone()),
                            ..StepPayload::new("skip", step)
                        },
                    );
                    feedback.push_str(&format!(
                        "命令 `{}` 未获执行（人没批准 / 等待超时）。这可能正是必要的一步：\
                         请要么换个更安全的等价做法，要么在结论里明确告诉用户需要他本人执行这一步。\n",
                        action.command.trim()
                    ));
                    continue;
                }
            }

            match run_one(
                state,
                app,
                &target_id,
                &action.command,
                step,
                target_label,
                &target_id,
                Some(&cancel),
                true,
            )
            .await
            {
                Ok(outcome) => {
                    let text = feedback_for(&action, target_label, &outcome);
                    feedback.push_str(&text);
                    feedback.push('\n');
                }
                Err(e) => {
                    emit_step(
                        app,
                        &session,
                        StepPayload {
                            target: Some(target_id.clone()),
                            target_label: Some(target_label.into()),
                            command: Some(action.command.trim().into()),
                            text: Some(e.clone()),
                            ..StepPayload::new("note", step)
                        },
                    );
                    feedback.push_str(&failure_feedback(&action, target_label, &e));
                }
            }
        }
    }

    runtime.finish(&session);
    Ok(())
}

/// 挑这一轮该用哪份系统提示词。
///
/// 返回 owned String 而不是 `&str`：用户写了自定义提示词时，成品是
/// 「他的提示词 + 必需的动作协议」，拼接只能是我这边的活。
struct IfAgentic;

impl IfAgentic {
    fn pick(custom: &str) -> String {
        if custom.trim().is_empty() {
            AGENTIC_SYSTEM_PROMPT.to_string()
        } else {
            // 尊重用户自己的设定 —— 也许他就是要一个不许动手的顾问。
            // 但协议必须挂上去：否则他在自定义提示词下永远不知道该怎么下令。
            format!("{}\n{CUSTOM_APPENDIX}", custom.trim_end())
        }
    }
}

/// 挂在用户自定义提示词后面的协议说明。
///
/// 不能简单返回 custom：那样用户的提示词里没有动作协议，模型就永远只会聊天。
/// 这里做的是「**用户的提示词 + 必需的协议**」而不是二选一。
const CUSTOM_APPENDIX: &str = concat!(
    "\n\n【可执行 mode】你可以直接在终端会话里执行命令：",
    "在回复里写 ```nx-exec 围栏代码块，内容为 {\"target\":\"<会话id>\",\"command\":\"<单行命令>\"}；",
    "多目标时用 targets / commands 数组。command 必须是单条单行命令。",
    "不写 target 表示落到用户当前会话。执行结果会回给你，据此继续。",
    "不可逆操作（rm -rf / mkfs / DROP 等）除非用户明确要求，否则不要执行。",
    "查清了直接给中文结论，不要再追加无谓的探查。"
);

/// 把回复里的 ```nx-exec 块摘掉，剩下就是给人看的结论。
///
/// 用于「模型一面下令一面顺手写了段说明」的情况：步数耗完之后这些块执行不了，
/// 留在答案里只是一堆噪音。
fn strip_action_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut inside = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if !inside {
            if trimmed.starts_with(FENCE_OPEN) {
                inside = true;
                continue;
            }
            out.push_str(line);
            out.push('\n');
        } else if trimmed.starts_with("```") {
            inside = false;
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(json: &str) -> String {
        format!("```nx-exec\n{json}\n```\n")
    }

    #[test]
    fn single_action_is_parsed() {
        let acts = extract_actions(&block(r#"{"target":"s1","command":"df -h"}"#));
        assert_eq!(
            acts,
            vec![AgentAction { target: Some("s1".into()), command: "df -h".into() }]
        );
    }

    #[test]
    fn missing_target_falls_back_to_current() {
        let acts = extract_actions(&block(r#"{"command":"uptime"}"#));
        assert_eq!(acts, vec![AgentAction { target: None, command: "uptime".into() }]);
    }

    /// 多主机 × 多命令：目标在外层，同一台机器上的命令连着跑
    #[test]
    fn multi_target_expands_target_outer() {
        let acts = extract_actions(&block(
            r#"{"targets":["a","b"],"commands":["uptime","whoami"]}"#,
        ));
        assert_eq!(acts.len(), 4);
        assert_eq!(acts[0], AgentAction { target: Some("a".into()), command: "uptime".into() });
        assert_eq!(acts[1], AgentAction { target: Some("a".into()), command: "whoami".into() });
        assert_eq!(acts[2], AgentAction { target: Some("b".into()), command: "uptime".into() });
        assert_eq!(acts[3], AgentAction { target: Some("b".into()), command: "whoami".into() });
    }

    /// 模型爱在围栏语言位写 json —— 不许因此整块失效
    #[test]
    fn fence_language_suffix_is_tolerated() {
        let acts = extract_actions("```nx-exec json\n{\"command\":\"ls\"}\n```\n");
        assert_eq!(acts, vec![AgentAction { target: None, command: "ls".into() }]);
    }

    #[test]
    fn plain_code_blocks_are_never_actions() {
        let text = "请先自己试一下：\n```bash\nrm -rf /\n```\n我再去帮你看日志。\n";
        assert!(extract_actions(text).is_empty(), "普通代码块不该被当成要执行的动作");
    }

    #[test]
    fn broken_json_yields_nothing() {
        assert!(extract_actions(&block("{not json")).is_empty());
        assert!(extract_actions(&block(r#"{"target":"s1"}"#)).is_empty(), "没有 command 就不算动作");
    }

    #[test]
    fn unterminated_fence_is_still_attempted() {
        let acts = extract_actions("```nx-exec\n{\"command\":\"pwd\"}");
        assert_eq!(acts, vec![AgentAction { target: None, command: "pwd".into() }]);
    }

    #[test]
    fn action_count_is_capped() {
        let mut commands = Vec::new();
        for i in 0..50 {
            commands.push(format!("echo {i}"));
        }
        let json = serde_json::json!({ "commands": commands }).to_string();
        assert_eq!(extract_actions(&block(&json)).len(), MAX_ACTIONS);
    }

    #[test]
    fn multiline_and_blank_commands_are_rejected() {
        assert_eq!(reject_command(""), Some("命令为空"));
        assert_eq!(reject_command("   "), Some("命令为空"));
        assert!(reject_command("ls\nrm -rf /").is_some(), "换行必须拒绝");
        assert!(reject_command("ls\r").is_some(), "回车必须拒绝");
        assert!(reject_command("bad\0cmd").is_some());
        // ESC 能在屏幕上把这条命令画成别的东西 —— 必须拒
        assert!(reject_command("ls\u{1b}]0;ok\u{7}").is_some());
        assert_eq!(reject_command("df -h"), None);
    }

    /// rd 带头的一组合法命令不能被误杀（含管道、引号、中文场景）
    #[test]
    fn normal_commands_pass_validation() {
        for cmd in [
            "ls -la /var/log",
            "grep -R 'error' /var/log/nginx --include='*.log' | head -50",
            "ss -tlnp | grep :8080",
            "journalctl -u nginx --since '10 min ago' --no-pager",
        ] {
            assert_eq!(reject_command(cmd), None, "{cmd} 不该被拒绝");
        }
    }

    #[test]
    fn action_blocks_are_stripped_from_the_final_answer() {
        let reply = "先看一下磁盘：\n```nx-exec\n{\"command\":\"df -h\"}\n```\n\n已经很清楚了。\n";
        assert_eq!(strip_action_blocks(reply), "先看一下磁盘：\n\n已经很清楚了。");
    }

    #[test]
    fn head_chars_caps_and_marks() {
        let long = "x".repeat(10);
        assert_eq!(head_chars(&long, 20), long);
        let capped = head_chars(&long, 4);
        assert!(capped.starts_with("xxxx"));
        assert!(capped.contains("6 / 10"), "{capped}");
    }

    /// 中文截断不能切出半个字
    #[test]
    fn head_chars_respects_char_boundaries() {
        let s = "中文输出内容";
        assert_eq!(head_chars(s, 2), "中文\n……（输出过长，已截断 4 / 6 字符）");
    }

    #[test]
    fn feedback_reports_exit_code_and_output() {
        let act = AgentAction { target: None, command: "false".into() };
        let out = ExecOutcome {
            output: "err\n".into(),
            exit_code: Some(1),
            reason: ExecEnd::Sentinel,
        };
        let text = feedback_for(&act, "web-01", &out);
        assert!(text.contains("false"));
        assert!(text.contains("退出码：1"));
        assert!(text.contains("err"));
        assert!(text.contains("web-01"));
    }

    #[test]
    fn cancellation_feedback_tells_model_not_to_retry() {
        let act = AgentAction { target: None, command: "sleep 100".into() };
        let out = ExecOutcome { output: String::new(), exit_code: None, reason: ExecEnd::Cancelled };
        assert!(feedback_for(&act, "x", &out).contains("不要重试"));
    }

    #[test]
    fn unknown_exit_code_is_worded_as_unknown() {
        let act = AgentAction { target: None, command: "top".into() };
        let out = ExecOutcome { output: "alive".into(), exit_code: None, reason: ExecEnd::Silence };
        let text = feedback_for(&act, "x", &out);
        assert!(text.contains("未知"));
        assert!(text.contains("alive"));
    }

    #[test]
    fn locate_prefers_named_target_and_falls_back() {
        let targets = vec![
            AgentTarget { id: "a".into(), label: "web-01".into() },
            AgentTarget { id: "b".into(), label: "db-01".into() },
        ];
        assert_eq!(locate(Some("b"), &targets, "a"), ("b".to_string(), "db-01"));
        assert_eq!(locate(Some("ghost"), &targets, "a"), ("ghost".to_string(), "未知会话"));
        assert_eq!(locate(None, &targets, "a"), ("a".to_string(), "web-01"));
    }

    #[test]
    fn empty_targets_fall_back_to_current_session() {
        let req = AgentRunRequest { session_id: "cur".into(), question: "q".into(), targets: vec![] };
        let resolved = resolve_targets(&req);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].id, "cur");
    }

    #[test]
    fn concurrent_runs_on_one_session_are_rejected() {
        let rt = ActRuntime::new();
        assert!(rt.begin("s1").is_some());
        assert!(rt.begin("s1").is_none());
        assert!(rt.begin("s2").is_some());
        rt.finish("s1");
        assert!(rt.begin("s1").is_some(), "结束后应可再次开始");
    }

    #[test]
    fn answering_an_unknown_request_is_reported() {
        let rt = ActRuntime::new();
        assert!(!rt.answer("nope", true), "没人挂着的请求应答应返回 false");
    }

    #[tokio::test]
    async fn answering_a_pending_request_delivers_the_decision() {
        let rt = ActRuntime::new();
        let rx = rt.register("r1");
        assert!(rt.answer("r1", true));
        assert!(rx.await.unwrap());
    }

    /// 只允许一轮：UI 崩了导致 rx 被 drop 时，unregister 不能误伤别的请求
    #[test]
    fn unregister_only_touches_its_own_request() {
        let rt = ActRuntime::new();
        let _rx1 = rt.register("r1");
        let _rx2 = rt.register("r2");
        rt.unregister("r1");
        assert!(rt.answer("r2", true));
    }

    #[test]
    fn cancel_sets_the_flag() {
        let rt = ActRuntime::new();
        let flag = rt.begin("s1").expect("应登记成功");
        assert!(rt.is_running("s1"));
        assert!(rt.cancel("s1"));
        assert!(flag.load(Ordering::Relaxed));
        assert!(!rt.cancel("s2"), "没有在跑的会话取消应返回 false");
    }

    /// agentic 提示词必须把协议与两条硬边界写清楚，否则模型不会动手 / 停不下来
    #[test]
    fn agentic_prompt_states_the_protocol_and_limits() {
        assert!(AGENTIC_SYSTEM_PROMPT.contains("nx-exec"));
        for key in ["targets", "commands", "单行", "不可逆", "不要再"] {
            assert!(AGENTIC_SYSTEM_PROMPT.contains(key), "提示词缺少要点：{key}");
        }
        assert!(CUSTOM_APPENDIX.contains("nx-exec"));
    }

    #[test]
    fn prompt_pick_never_drops_the_protocol() {
        // 用户清空提示词 → 用 agentic 兜底
        assert_eq!(IfAgentic::pick("   "), AGENTIC_SYSTEM_PROMPT);
        // 用户写了自己的 → 他的话要留着，协议也要挂上
        let picked = IfAgentic::pick("你是我的运维顾问");
        assert!(picked.starts_with("你是我的运维顾问"), "用户的提示词被丢了");
        assert!(picked.contains("nx-exec"), "自定义模式也必须带上协议");
    }
}
