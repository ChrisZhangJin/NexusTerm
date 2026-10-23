//! AI agent 的调用层：把「人的问题 + 会话上下文」发给 LLM，流式推回前端。
//!
//! 旁路缓冲（`buffer.rs` / `mod.rs`）只解决了「agent 能看到什么」，这一层解决
//! 「人怎么问、答案怎么回来」。
//!
//! ## 四条不可妥协的边界
//!
//! 1. **脱敏必须在出网前完成**。`AgentManager::context` 刻意保留原文（UI 要展示），
//!    上下文里什么都有 —— 私钥、`Bearer` token、内网 IP。所以这里是唯一也是最后
//!    一道闸门：`config.redact` 为真时必须过 [`sanitize_context`]。
//! 2. **绝不在持 `AgentManager` 锁的时候做网络 I/O**。上下文在进入异步任务之前
//!    就取成 `String`，锁当场释放；HTTP 是秒级的，不能让终端的 `feed` 路径等它。
//! 3. **失败必须是中文句子**。401 / 429 / 404 / 超时 / 连不上都要翻译成
//!    「人照着就能做一件事」的提示，绝不能把 panic 或英文原文抛给用户。
//! 4. **同一会话同一时刻只允许一个流**。两条流交错会把前端的答案搅成一团，
//!    所以后到的提问直接拒绝，并留出 `agent_cancel` 让用户中途掐断。
//!
//! ## 为什么要流式
//!
//! 一个几百字的回答要生成几十秒。攒完再发意味着用户盯着空白界面，
//! 因此边收边推：`nx://agent/token/{session}`（增量文本）
//! → `nx://agent/done/{session}`（结束，带 reason）
//! → 出错则 `nx://agent/error/{session}`。事件命名沿用项目既有的
//! `nx://term/data/{id}`、`nx://local/data/{id}` 约定。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures_util::StreamExt;
use parking_lot::Mutex;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use super::redact::redact;
use super::{AgentAccount, AgentConfig, AgentError, AgentManager, DEFAULT_MAX_CONTEXT_BYTES};
use crate::errors;

/// 连接超时：DNS + TCP + TLS 三步都算在里面。15 秒足够跨洲握手，
/// 也足够在用户填错地址时给出一个明确失败而不是无限等待。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// 单次提问的整体超时（**含把流读完**）。
///
/// reqwest 的 `timeout()` 覆盖从发起到响应体读完的全过程，正是流式需要的语义。
/// 取 5 分钟是「不打断长答案」与「不让 UI 永远转圈」之间的折中。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Anthropic 的 `/messages` 强制要求 `max_tokens`，且没有默认值。
const ANTHROPIC_MAX_TOKENS: u32 = 4096;

/// Anthropic 要求显式声明 API 版本，否则 400。
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// 单个 SSE 行的长度上限。
///
/// 没有这个上限，一个故意不发换行符的服务端能把内存吃到爆。
/// 真实 SSE 行（一行 JSON）通常几百字节，1MB 已经极其宽松。
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// 错误信息里引用的服务端原文上限。
///
/// 网关的 502 页面动辄几十 KB HTML，整段贴给用户毫无意义，还会把日志刷爆。
const MAX_ERROR_DETAIL: usize = 300;

/// 读错误响应体时的字节上限（`read_error_body` 专用，比 MAX_ERROR_DETAIL 宽松，
/// 因为要先拿到原文才能从中挑出有意义的那句话）。
const MAX_ERROR_BODY: usize = 8 * 1024;

/// OpenAI 兼容流的结束标记。
const DONE_MARKER: &str = "[DONE]";

/// 「测试连接」用的极短提问。字数越少，越能排除「模型只是慢」这种误判。
const TEST_QUESTION: &str = "只回复 ok 两个字，不要任何解释。";

// ——————————————————————————————— provider 判定 ———————————————————————————————

/// 支持的协议方言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// OpenAI `/chat/completions`，以及所有兼容它的网关（vLLM / Ollama / 各类中转）
    OpenAI,
    /// Anthropic 原生 `/messages`
    Anthropic,
}

/// 由配置里的 provider 字段判定方言。
///
/// **刻意让未知值落到 OpenAI 兼容分支**：绝大多数自建网关（OneAPI、Ollama、
/// vLLM、各种中转）都是 OpenAI 形状，用户手打 `custom` 或拼错成 `OpenAI ` 时，
/// 宁可按兼容协议试一次，也不要直接报「不支持的 provider」。
pub fn resolve_provider(acct: &AgentAccount) -> Provider {
    if acct.provider.trim().eq_ignore_ascii_case("anthropic") {
        Provider::Anthropic
    } else {
        Provider::OpenAI
    }
}

/// 拼出请求地址。
///
/// 去掉 base_url 的尾斜杠：用户从网页上复制来的地址几乎都带 `/`，
/// 直接 `{base}/chat/completions` 会拼出 `//chat/completions`，
/// 部分网关会把它当成另一条路径（404）。
pub fn endpoint(acct: &AgentAccount, provider: Provider) -> String {
    // 先去掉尾斜杠，再去掉用户可能误填的完整终点后缀：界面提示承诺「结尾的
    // /chat/completions 会自动去掉」，这里把 /chat/completions 与 /messages
    // 都剥掉，保证无论用户填 base URL 还是完整终点都拼得对、不至于 404。
    let mut base = acct.base_url.trim().trim_end_matches('/');
    for suffix in ["/chat/completions", "/messages"] {
        if let Some(stripped) = base.strip_suffix(suffix) {
            base = stripped.trim_end_matches('/');
            break;
        }
    }
    let path = match provider {
        Provider::OpenAI => "/chat/completions",
        Provider::Anthropic => "/messages",
    };
    format!("{base}{path}")
}

// ——————————————————————————————— 系统提示词 ———————————————————————————————

/// 用户没填（或清空了）系统提示词时的兜底。
///
/// 刻意比 `AgentConfig::default()` 里那一版更啰嗦：那一版是用户在设置界面里看到、
/// 可以随手删掉的起点；这一版是「什么都没有」时的底线，必须把四条约束说全 ——
/// 少了其中任何一条，模型都倾向于凭空编造终端里发生过什么，而人根本无从分辨。
pub const FALLBACK_SYSTEM_PROMPT: &str = concat!(
    "你是 NexusTerm 里绑定在某个终端会话上的助手。",
    "用户会把该会话最近的输出记录发给你：其中 `$ ` 开头的是人敲进去的命令，",
    "其余是程序的输出（已剥离 ANSI 转义序列）。请遵守以下约定：",
    "1) 只依据记录里确实出现过的事实作答，记录里没有的事情不要臆测；",
    "2) 记录被截断或缺失时，明说你看不到全貌，并告诉用户还差哪些信息；",
    "3) 不确定就直接说不确定，并给出验证办法，不要编造输出内容或报错原文；",
    "4) 给出命令时必须说明它的作用与风险（尤其是删除、覆盖、修改权限、",
    "   作用范围超出当前目录的命令），并提醒用户在自己的机器上确认后再执行；",
    "5) 用中文简洁回答，不复述用户已经给出的输出。"
);

/// 取实际生效的系统提示词。
pub fn system_prompt(acct: &AgentAccount) -> &str {
    let custom = acct.system_prompt.trim();
    if custom.is_empty() {
        FALLBACK_SYSTEM_PROMPT
    } else {
        custom
    }
}

// ——————————————————————————————— 请求组装 ———————————————————————————————

/// 上下文段的引导语。放在分隔标记之前，是为了让模型先知道「下面这段是什么」。
pub const CONTEXT_INTRO: &str =
    "以下是该终端会话最近的输出记录（已剥离 ANSI 转义，人的输入行以 $ 开头）：";

pub const CONTEXT_BEGIN: &str = "----- 会话上下文开始 -----";
pub const CONTEXT_END: &str = "----- 会话上下文结束 -----";

/// 拿不到上下文时的说明。
///
/// **不能静默发空上下文**：模型不知道自己为什么「没看见」，就会开始编。
/// 明确告知它能让回答退化成「我猜你大概想问……」而不是伪造一条报错。
pub const CONTEXT_MISSING: &str = concat!(
    "（未能取得该终端会话的输出记录：会话旁路未开启，或上下文已被清空，",
    "或缓冲已被丢弃。请如实说明你看不到终端里发生过什么，",
    "并告诉用户需要开启会话旁路或重新执行关键命令。）"
);

/// **出网前唯一的闸门**。
///
/// `raw` 是 `AgentManager::context` 给出的原文（UI 展示用的那份）。
/// 配置要求脱敏时，这里是不把它发出去的最后机会。
pub fn sanitize_context(acct: &AgentAccount, raw: &str) -> String {
    if acct.redact {
        redact(raw)
    } else {
        raw.to_string()
    }
}

/// 组装发给模型的 user 消息。
///
/// `context` 为 `None` 或空白（会话未开启旁路 / 被 `agent_clear` 清空）时，
/// 改用 [`CONTEXT_MISSING`] 说明情况 —— 绝不静默发一个空上下文。
pub fn build_user_message(context: Option<&str>, question: &str) -> String {
    let body = match context.map(str::trim).filter(|s| !s.is_empty()) {
        Some(ctx) => format!("{CONTEXT_INTRO}\n{CONTEXT_BEGIN}\n{ctx}\n{CONTEXT_END}\n"),
        None => format!("{CONTEXT_MISSING}\n"),
    };
    format!("{body}\n我的问题：{question}")
}

/// 一条聊天消息。两种方言的形状一致（`role` + `content`），故共用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: "user".into(), content: content.into() }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: "assistant".into(), content: content.into() }
    }
}

/// OpenAI 兼容：`POST {base}/chat/completions`
#[derive(Debug, Serialize)]
struct OpenAiRequest {
    model: String,
    messages: Vec<ChatMessage>,
    stream: bool,
    /// 空则不出现：不支持 tools 的兼容层收到空数组也可能报错，干脆不声明。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<OpenAiTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<OpenAiToolChoice>,
}

/// Anthropic 原生：`POST {base}/messages`
#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    /// Anthropic 的 system 是**顶层字段**，塞进 messages 数组会被拒。
    /// 空则整个字段不出现（部分兼容层不接受空字符串）。
    #[serde(skip_serializing_if = "is_blank")]
    system: String,
    messages: Vec<ChatMessage>,
    /// Anthropic 没有默认值，必须显式给
    max_tokens: u32,
    stream: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<AnthropicToolChoice>,
}

/// 一个可下发给模型的工具。
///
/// 刻意与 wire format 解耦：act.rs 决定「有哪些工具、各自什么语义」，
/// llm.rs 只负责翻译成 OpenAI / Anthropic 各自的形状。
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: String,
    pub schema: serde_json::Value,
}

/// OpenAI 侧的工具声明：function 外面还要包一层 `type:"function"`。
#[derive(Debug, Serialize)]
struct OpenAiTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: ToolFunctionSpec,
}

#[derive(Debug, Serialize)]
struct ToolFunctionSpec {
    name: &'static str,
    description: String,
    parameters: serde_json::Value,
}

/// Anthropic 侧的工具声明：`name` / `description` / `input_schema` 是平铺的，
/// 没有 function 那一层包装（写成 OpenAI 的形状会被整个拒掉）。
#[derive(Debug, Serialize)]
struct AnthropicTool {
    name: &'static str,
    description: String,
    input_schema: serde_json::Value,
}

/// `auto`：让模型自己决定是直接回答还是调工具。
///
/// 不用 `required` —— 那样模型连「我查清楚了，直接给结论」都说不出来，
/// 每一轮都被迫调一次工具，收不了尾。
#[derive(Debug, Serialize)]
struct OpenAiToolChoice {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Debug, Serialize)]
struct AnthropicToolChoice {
    #[serde(rename = "type")]
    kind: &'static str,
}

/// 模型要求调用的一个工具。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub name: String,
    /// 原始参数 JSON 文本。
    ///
    /// 刻意留成字符串而不是直接反序列化：act.rs 里已经有一套 ExecBlock 解析
    /// （缺字段给默认值、错名兼容），工具参数走同一条路就不用再写一遍。
    pub arguments: String,
}

/// 一次模型回复：可能同时含正文与若干工具调用。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LlmReply {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    /// 本轮请求是否真的带上了 tools 声明。
    ///
    /// 降级重试（服务方不认 tools）后置 false，调用方据此改回围栏协议。
    pub tools_supported: bool,
}

fn openai_tools(specs: &[ToolSpec]) -> Vec<OpenAiTool> {
    specs
        .iter()
        .map(|s| OpenAiTool {
            kind: "function",
            function: ToolFunctionSpec {
                name: s.name,
                description: s.description.clone(),
                parameters: s.schema.clone(),
            },
        })
        .collect()
}

fn anthropic_tools(specs: &[ToolSpec]) -> Vec<AnthropicTool> {
    specs
        .iter()
        .map(|s| AnthropicTool {
            name: s.name,
            description: s.description.clone(),
            input_schema: s.schema.clone(),
        })
        .collect()
}

fn is_blank(s: &str) -> bool {
    s.trim().is_empty()
}

/// 流式请求才声明 `Accept: text/event-stream`。
///
/// 不加这个头，部分网关（尤其是自建兼容层）会把流攒成一次性 JSON 返回，
/// 「流式」就名存实亡了。非流式的测试连接不需要它。
fn push_accept_sse(headers: &mut Vec<(String, String)>, stream: bool) {
    if stream {
        headers.push(("Accept".into(), "text/event-stream".into()));
    }
}

/// 组装好的一次请求。**刻意不 derive Debug** —— 里面带着 API Key，
/// 不能被任何 `tracing::debug!("{:?}", req)` 顺手打进日志。
pub struct PreparedRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// 组装请求。纯函数，便于单测锁住 wire format。
///
/// `stream = false` 用于「测试连接」：省掉 SSE 解析，只看一次往返通不通。
pub fn build_request(
    acct: &AgentAccount,
    provider: Provider,
    system: &str,
    user: &str,
    stream: bool,
) -> Result<PreparedRequest, AgentError> {
    let messages = vec![ChatMessage::user(user)];
    build_messages(acct, provider, system, &messages, stream)
}

/// 多轮版 [`build_request`]：整段会话历史一起发出去。
///
/// Agentic 循环每一轮都要把「已经执行过什么、终端回了什么」续在后面喂回去，
/// 单条 user 消息的形状不够用。
pub fn build_messages(
    acct: &AgentAccount,
    provider: Provider,
    system: &str,
    messages: &[ChatMessage],
    stream: bool,
) -> Result<PreparedRequest, AgentError> {
    build_messages_with_tools(acct, provider, system, messages, stream, &[])
}

/// 带工具声明版 [`build_messages`]。
///
/// 抽成独立函数而不是给 `build_messages` 加参数：那会让流式问答、测试连接、
/// 以及一众 wire format 单测都得多传一个 `&[]`，而这些路径根本不需要工具。
pub fn build_messages_with_tools(
    acct: &AgentAccount,
    provider: Provider,
    system: &str,
    messages: &[ChatMessage],
    stream: bool,
    tools: &[ToolSpec],
) -> Result<PreparedRequest, AgentError> {
    if acct.base_url.trim().is_empty() {
        return Err(AgentError::BadRequest("尚未填写接口地址（Base URL）".into()));
    }
    let model = acct.model.trim();
    if model.is_empty() {
        return Err(AgentError::MissingModel);
    }
    // trim 而不是直接用：从网页/终端复制 key 时经常带出首尾空白或换行，
    // 带进去的请求头会被 hyper 判为非法字符，报一句和「Key 错了」完全不同的错。
    let key = acct.api_key.trim().to_string();
    let url = endpoint(acct, provider);

    let (headers, body) = match provider {
        Provider::OpenAI => {
            let mut headers = vec![
                ("Authorization".into(), format!("Bearer {key}")),
                ("Content-Type".into(), "application/json".into()),
            ];
            push_accept_sse(&mut headers, stream);
            let req = OpenAiRequest {
                model: model.to_string(),
                messages: messages.to_vec(),
                stream,
                tools: openai_tools(tools),
                tool_choice: if tools.is_empty() {
                    None
                } else {
                    Some(OpenAiToolChoice { kind: "auto" })
                },
            };
            let body = serde_json::to_string(&req).map_err(|e| AgentError::Serialize(e.to_string()))?;
            (headers, body)
        }
        Provider::Anthropic => {
            let mut headers = vec![
                ("x-api-key".into(), key),
                ("anthropic-version".into(), ANTHROPIC_VERSION.into()),
                ("Content-Type".into(), "application/json".into()),
            ];
            push_accept_sse(&mut headers, stream);
            let req = AnthropicRequest {
                model: model.to_string(),
                system: system.to_string(),
                messages: messages.to_vec(),
                max_tokens: ANTHROPIC_MAX_TOKENS,
                stream,
                tools: anthropic_tools(tools),
                tool_choice: if tools.is_empty() {
                    None
                } else {
                    Some(AnthropicToolChoice { kind: "auto" })
                },
            };
            let body = serde_json::to_string(&req).map_err(|e| AgentError::Serialize(e.to_string()))?;
            (headers, body)
        }
    };

    // provider 不进 struct：调用方是把它传进来的，再回带一份只会多一个
    // 「到底该信哪个」的来源。url / headers / body 才是这一次请求的全部内容。
    Ok(PreparedRequest {
        url,
        headers,
        body,
    })
}

/// 把 `Vec<(String, String)>` 装成 reqwest 的 `HeaderMap`。
///
/// 抽出来是为了让三条出网路径（流式问答 / 测试连接 / Agentic 循环）共用同一套
/// 「非法字符 → 中文提示」的判定，不至于在其中一条上漏掉。
pub(crate) fn header_map(headers: &[(String, String)]) -> Result<HeaderMap, AgentError> {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| AgentError::BadRequest("请求头名非法，请检查配置".into()))?;
        let value = HeaderValue::from_str(value).map_err(|_| {
            // 九成是 API Key 被复制时带进了换行
            AgentError::BadRequest("API Key 含有非法字符（换行等），请重新复制".into())
        })?;
        map.append(name, value);
    }
    Ok(map)
}

/// 配置合法性检查。提问与测试连接共用，保证两处的行为一致。
pub fn validate(cfg: &AgentConfig) -> Result<(), AgentError> {
    if !cfg.enabled {
        return Err(AgentError::Disabled);
    }
    let acct = cfg.active_account();
    if acct.api_key.trim().is_empty() {
        return Err(AgentError::MissingKey);
    }
    if acct.model.trim().is_empty() {
        return Err(AgentError::MissingModel);
    }
    if acct.base_url.trim().is_empty() {
        return Err(AgentError::BadRequest("尚未填写接口地址（Base URL）".into()));
    }
    Ok(())
}

// ——————————————————————————————— SSE 解析 ———————————————————————————————

/// SSE 行缓冲状态机。
///
/// TCP 是**字节流**：一次 `bytes_stream().next()` 拿到的边界完全由网络包决定，
/// 半行、一行半、半个 UTF-8 字符都可能。所以「没凑齐一行的尾巴」必须跨 chunk
/// 留下来 —— 这也是这里不用 `lines()`、不按 `\r` 单独切分的原因：
/// 等到换行符再解码，多字节字符就一定是完整的。
pub struct SseDecoder {
    /// 上一次没凑齐一行的残部
    pending: Vec<u8>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self {
            pending: Vec::with_capacity(1024),
        }
    }

    /// 喂入一段字节，把其中**已经完整**的行追加进 `out`。返回本次新增的行数。
    ///
    /// `out` 由调用方复用：流式过程中每行都要走一遍解析，没必要每行都新分配一个 Vec。
    pub fn push(&mut self, data: &[u8], out: &mut Vec<String>) -> usize {
        let before = out.len();
        self.pending.extend_from_slice(data);

        // 只扫描，不改动 pending：切出若干完整行后一次性 drain
        let mut consumed = 0usize;
        for (i, &b) in self.pending.iter().enumerate() {
            if b != b'\n' {
                continue;
            }
            if let Some(line) = decode_line(&self.pending[consumed..i]) {
                out.push(line);
            }
            consumed = i + 1;
        }
        if consumed > 0 {
            self.pending.drain(..consumed);
        }

        // 服务端一直不发换行（或根本不是 SSE）时 pending 会无限增长，
        // 超限就整段作废：丢掉一个超长行，总好过把内存吃光。
        if self.pending.len() > MAX_LINE_BYTES {
            tracing::warn!(
                "SSE 单行超过 {} 字节，已丢弃缓冲（服务端可能没在发换行）",
                MAX_LINE_BYTES
            );
            self.pending.clear();
        }
        out.len() - before
    }

    /// 流结束后冲刷最后一行（没有换行结尾的残行）。
    ///
    /// 服务端在 `data: [DONE]` 之后直接关连接是常见做法，
    /// 少了这一步就会把最后一条 delta 丢掉。
    pub fn finish(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            return None;
        }
        let line = decode_line(&self.pending);
        self.pending.clear();
        line
    }
}

impl Default for SseDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// 把一段不含 `\n` 的字节转成一行文本：去掉 CRLF 的 `\r`，lossy 解码。
///
/// lossy 而不是严格 UTF-8：终端上下文里什么编码都有可能，
/// 一个非法字节不该让整条流中断。
fn decode_line(raw: &[u8]) -> Option<String> {
    let end = raw.strip_suffix(b"\r").unwrap_or(raw);
    Some(String::from_utf8_lossy(end).into_owned())
}

/// 从一条 SSE 行里取出 `data:` 的载荷。
///
/// 返回 `None` 的三种情况：注释行（心跳，如 `: ping`）、`event:` / `id:` 等
/// 其它字段行、以及空行 —— 调用方需要靠「是不是 None」之外的信息区分，
/// 故空行与 event 行由 [`dispatch_line`] 在更前面处理。
pub fn parse_data_line(line: &str) -> Option<&str> {
    if line.starts_with(':') {
        return None;
    }
    let rest = line.strip_prefix("data:")?;
    // SSE 规范允许冒号后最多一个空格
    let payload = rest.strip_prefix(' ').unwrap_or(rest);
    Some(payload.trim())
}

/// 流结束的原因，直接序列化进 `done` 事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FinishReason {
    /// 模型正常说完
    Stop,
    /// 撞到 max_tokens，答案被截断
    Length,
    /// 用户点了取消
    Cancelled,
}

/// 解析一行 SSE 后应当发生的动作。
#[derive(Debug, PartialEq, Eq)]
pub enum SseAction {
    /// 心跳、未知事件、解析不了的杂行：静默跳过
    Nothing,
    /// 增量文本
    Text(String),
    /// 流应当结束
    Done(FinishReason),
    /// 服务端在流里主动报错
    Failed(String),
}

/// 跨行保留的状态：当前事件块的事件名。
///
/// SSE 的一个事件块是「若干字段行 + 一个空行」，Anthropic 靠 `event:` 行区分
/// `content_block_delta` 与 `message_stop`，所以事件名必须跨行记住，
/// 并在空行处清空。
#[derive(Debug, Default)]
pub struct SseState {
    event: Option<String>,
}

/// 把一行喂进状态机，返回这一行要产生的动作。
pub fn dispatch_line(provider: Provider, state: &mut SseState, line: &str) -> SseAction {
    if line.is_empty() {
        // 空行 = 事件块结束，事件名随之失效
        state.event = None;
        return SseAction::Nothing;
    }
    if let Some(name) = line.strip_prefix("event:") {
        state.event = Some(name.trim().to_string());
        return SseAction::Nothing;
    }
    let Some(data) = parse_data_line(line) else {
        return SseAction::Nothing;
    };
    match provider {
        Provider::OpenAI => parse_openai_event(data),
        Provider::Anthropic => parse_anthropic_event(state.event.as_deref(), data),
    }
}

/// OpenAI 兼容流的一个 chunk。
///
/// 只声明要用的字段：各家兼容层塞的字段（usage、`system_fingerprint`、
/// `logprobs`…）一概不管，serde 默认忽略未知字段。
#[derive(Deserialize)]
struct OpenAiChunk {
    #[serde(default)]
    choices: Vec<OpenAiChoice>,
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    #[serde(default)]
    delta: OpenAiDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Default, Deserialize)]
struct OpenAiDelta {
    #[serde(default)]
    content: Option<String>,
}

/// 各家错误体的公共形状：OpenAI 用 `error.message`，Anthropic 也是。
#[derive(Deserialize)]
struct ApiError {
    #[serde(default)]
    message: String,
}

/// Anthropic 的 `content_block_delta`。
#[derive(Deserialize)]
struct AnthropicDelta {
    #[serde(default)]
    delta: AnthropicTextDelta,
}

#[derive(Default, Deserialize)]
struct AnthropicTextDelta {
    #[serde(default)]
    text: Option<String>,
}

/// Anthropic 的 `message_delta`：结束原因在这里，不在 text 事件里。
#[derive(Deserialize)]
struct AnthropicMessageDelta {
    #[serde(default)]
    delta: AnthropicStop,
}

#[derive(Default, Deserialize)]
struct AnthropicStop {
    #[serde(default)]
    stop_reason: Option<String>,
}

/// 只取 `type` 字段：用于「没有 `event:` 行时靠 JSON 自述判定事件类型」。
#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    error: Option<ApiError>,
}

/// 解析 OpenAI 兼容流里的一条 data。
pub fn parse_openai_event(data: &str) -> SseAction {
    if data == DONE_MARKER {
        return SseAction::Done(FinishReason::Stop);
    }
    let Ok(chunk) = serde_json::from_str::<OpenAiChunk>(data) else {
        // 解析不了的行一律忽略。SSE 里杂行很常见（某些网关会发 `data: ` 空载荷、
        // 或把心跳做成 `data: {}`），为它们中断整条流不值得。
        return SseAction::Nothing;
    };
    if let Some(err) = chunk.error {
        let msg = err.message.trim();
        if !msg.is_empty() {
            return SseAction::Failed(msg.to_string());
        }
    }
    if let Some(choice) = chunk.choices.first() {
        // 先取文本再判结束：部分兼容层会在同一个 chunk 里同时给 content 与
        // finish_reason，先判结束会白白丢掉最后一段文本。
        if let Some(text) = choice.delta.content.as_deref() {
            if !text.is_empty() {
                return SseAction::Text(text.to_string());
            }
        }
        if let Some(reason) = choice.finish_reason.as_deref() {
            return SseAction::Done(finish_reason_from(reason));
        }
    }
    SseAction::Nothing
}

/// 解析 Anthropic 原生流里的一条 data。
pub fn parse_anthropic_event(event: Option<&str>, data: &str) -> SseAction {
    let Ok(envelope) = serde_json::from_str::<Envelope>(data) else {
        return SseAction::Nothing;
    };
    // 事件名优先取 SSE 的 `event:` 行；没有（部分网关会剥掉它）就回落到
    // JSON 里的 `type` 字段 —— 少这个兜底，中转一层就整条流都不出字。
    let mut name = match event {
        Some(n) => n.to_string(),
        None => envelope.r#type.clone().unwrap_or_default(),
    };

    if envelope.error.is_some() && name.is_empty() {
        name = "error".to_string();
    }

    match name.as_str() {
        "content_block_delta" => match serde_json::from_str::<AnthropicDelta>(data) {
            Ok(d) => match d.delta.text {
                Some(text) if !text.is_empty() => SseAction::Text(text),
                _ => SseAction::Nothing,
            },
            Err(_) => SseAction::Nothing,
        },
        "message_stop" => SseAction::Done(FinishReason::Stop),
        "message_delta" => {
            match serde_json::from_str::<AnthropicMessageDelta>(data) {
                Ok(d) => {
                    let reason = d.delta.stop_reason.as_deref().unwrap_or_default();
                    SseAction::Done(finish_reason_from(reason))
                }
                // 解析不出来也已经知道「模型说完了」，按 stop 收尾好过让前端一直转圈
                Err(_) => SseAction::Done(FinishReason::Stop),
            }
        }
        "error" => {
            let msg = envelope
                .error
                .as_ref()
                .map(|e| e.message.trim().to_string())
                .unwrap_or_default();
            if msg.is_empty() {
                SseAction::Failed(truncate_chars(data, MAX_ERROR_DETAIL))
            } else {
                SseAction::Failed(msg)
            }
        }
        // ping / message_start / content_block_start / … 一概忽略
        _ => SseAction::Nothing,
    }
}

/// 各家对「为什么停」的用词并不统一，统一收敛成两个语义。
fn finish_reason_from(reason: &str) -> FinishReason {
    match reason.trim() {
        "length" | "model_length" | "max_tokens" => FinishReason::Length,
        _ => FinishReason::Stop,
    }
}

// ——————————————————————————————— 错误文案 ———————————————————————————————

/// 按字符截断并加省略号。按 `char` 而不是按字节切，否则中文会切出半个字。
pub fn truncate_chars(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(max.min(text.len()) + 1);
    for (i, c) in text.chars().enumerate() {
        if i >= max {
            out.push('…');
            return out;
        }
        out.push(c);
    }
    out
}

/// 从错误响应体里挑出「给人看的那句话」。
///
/// 字段名各家不统一（OpenAI / Anthropic 用 `error.message`，兼容层有的用
/// `msg` / `detail`）；JSON 解析不出来就退回纯文本首行（网关的 502 是 HTML）。
/// 拿不到有意义的文案时返回空串 —— 宁可少给一句，也不要把整坨 JSON 甩给用户。
pub fn extract_error_detail(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        let candidates = [
            value.pointer("/error/message").and_then(|v| v.as_str()),
            value.pointer("/message").and_then(|v| v.as_str()),
            value.pointer("/msg").and_then(|v| v.as_str()),
            value.pointer("/detail").and_then(|v| v.as_str()),
        ];
        for candidate in candidates.into_iter().flatten() {
            let text = candidate.trim();
            if !text.is_empty() {
                return truncate_chars(text, MAX_ERROR_DETAIL);
            }
        }
        return String::new();
    }
    // 非 JSON：把换行压成空格再截断，避免半屏 HTML 糊在提示框里
    let flat: String = trimmed
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    truncate_chars(&flat, MAX_ERROR_DETAIL)
}

/// HTTP 状态码 → 中文提示。
///
/// 沿用 `errors::compose` 的「主文案 + 原始错误」结构：主文案告诉人该做什么，
/// 原始错误保留在尾部小字里供排障（前端已经按 `RAW_SUFFIX` 拆分显示）。
pub fn http_status_message(status: StatusCode, body: &str) -> String {
    let main = match status.as_u16() {
        401 | 403 => "API Key 无效、已过期，或没有该模型的访问权限：请到设置里重新填写",
        429 => "请求被限流：提问太频繁或额度已耗尽，请稍后重试",
        404 => "模型或接口地址不存在：请检查模型名称与 Base URL 是否匹配",
        402 => "账户余额不足：请到服务商处充值后重试",
        400 => "请求被拒绝：通常是模型不支持流式输出，或参数不合法",
        408 | 409 => "请求超时或冲突，请稍后重试",
        500..=599 => "模型服务端故障，请稍后重试",
        _ => "模型服务返回了异常状态码",
    };
    errors::compose(
        &format!("{}（HTTP {}）", main, status.as_u16()),
        &extract_error_detail(body),
    )
}

/// reqwest 的传输层错误 → 中文提示。
pub fn transport_error(err: &reqwest::Error) -> String {
    let raw = err.to_string();
    if err.is_timeout() {
        return errors::compose(
            &format!(
                "请求超时：{} 秒内没有拿到模型服务的响应。请稍后重试，或换一个更快的模型",
                REQUEST_TIMEOUT.as_secs()
            ),
            &raw,
        );
    }
    if err.is_connect() {
        return errors::compose(
            "连不上模型服务：请检查网络与代理设置，以及 Base URL 是否正确",
            &raw,
        );
    }
    if err.is_decode() {
        return errors::compose(
            "模型服务返回了无法解析的响应：请确认 Base URL 指向的是兼容 OpenAI 的接口",
            &raw,
        );
    }
    if err.is_builder() {
        return errors::compose(
            "请求构造失败：请检查 API Key 与 Base URL 是否含非法字符（换行、空格等）",
            &raw,
        );
    }
    errors::compose("网络请求失败：请检查网络与代理设置", &raw)
}

/// 读响应体，**限长**。
///
/// 成功响应也走限长：模型可能一口气返回几 MB JSON，而我们只需要其中的正文，
/// 超了就截断 —— 少几个字好过把内存吃穿。
async fn read_body_limited(resp: reqwest::Response, max: usize) -> String {
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(Ok(chunk)) = stream.next().await {
        buf.extend_from_slice(&chunk);
        if buf.len() >= max {
            break;
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// 读错误响应体（比成功体宽松一些：原文里可能藏着有用的那句话）。
async fn read_error_body(resp: reqwest::Response) -> String {
    read_body_limited(resp, MAX_ERROR_BODY).await
}

// ——————————————————————————————— HTTP 客户端 ———————————————————————————————

/// 装 rustls 的 crypto provider。
///
/// `rustls-no-provider` 把「用哪个 provider」交给调用方，**不装就在建 Client 时
/// panic**（reqwest 内部是硬 panic）。所以这一步必须在任何 `Client::builder()`
/// 之前发生，且绝不能失败后继续往下走。
fn ensure_crypto_provider() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        // install_default 返回 Err 表示「已经有人装过了」——那正是我们要的状态，
        // 故按成功处理，只留一条 debug 记录。
        if rustls::crypto::ring::default_provider()
            .install_default()
            .is_err()
        {
            tracing::debug!("rustls 的默认 crypto provider 已存在，沿用之");
        }
    });
}

/// 全局复用的 HTTP 客户端。
///
/// 每个 `Client` 自带连接池：逐次新建意味着每次提问都要重做 TCP + TLS 握手
/// （几百毫秒），流式首字延迟会明显变差。
///
/// `pub(crate)`：Agentic 循环（`agent/act.rs`）也走它。
pub(crate) fn http_client() -> Result<Client, AgentError> {
    static CLIENT: OnceLock<Result<Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            ensure_crypto_provider();
            Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                // 整体超时，含把流读完 —— 没有它，一个挂死的连接能让 UI 永远转圈
                .timeout(REQUEST_TIMEOUT)
                .build()
                .map_err(|e| e.to_string())
        })
        .clone()
        .map_err(AgentError::Transport)
}

// ——————————————————————————————— 运行时 ———————————————————————————————

/// per-session 的取消标记。
///
/// 用 `Arc<AtomicBool>` 而不是 `tokio_util::sync::CancellationToken`：
/// 为了一个布尔量多引一个依赖不值得，而且这里只需要「置位 / 查询」两个动作。
type CancelFlag = Arc<AtomicBool>;

struct Inner {
    /// 正在跑的提问：session_id → 取消标记。
    ///
    /// 两个用途：① 同一会话的并发提问直接拒绝（两条流交错会把答案搅成一团，
    /// 前端无法分辨哪句属于哪个问题）；② 用户点取消时置位，让流尽快停下。
    inflight: Mutex<HashMap<String, CancelFlag>>,
}

impl Inner {
    /// 摘掉一条登记。放在 `Inner` 上是为了让持有 `Arc<Inner>` 的任务
    /// （见 `InflightGuard`）在 drop 时也能调用它。
    fn finish(&self, session_id: &str) {
        self.inflight.lock().remove(session_id);
    }
}

/// agent 的 LLM 调用运行时。挂在 `AppState` 上，与 `AgentManager` 并列 ——
/// 缓冲归缓冲，出网归出网，两者各自的锁互不干扰。
pub struct LlmRuntime {
    inner: Arc<Inner>,
}

impl LlmRuntime {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                inflight: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// 登记一次提问。返回 `None` 表示该会话已经有提问在跑 —— 调用方应当直接拒绝。
    fn begin(&self, session_id: &str) -> Option<CancelFlag> {
        let mut guard = self.inner.inflight.lock();
        if guard.contains_key(session_id) {
            return None;
        }
        let flag: CancelFlag = Arc::new(AtomicBool::new(false));
        guard.insert(session_id.to_string(), Arc::clone(&flag));
        Some(flag)
    }

    /// 取消某会话正在进行的提问。返回是否确实有流被置位。
    pub fn cancel(&self, session_id: &str) -> bool {
        let flag = self.inner.inflight.lock().get(session_id).cloned();
        match flag {
            Some(flag) => {
                flag.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    /// 该会话是否有提问在跑。
    ///
    /// 生产代码暂无调用方：留给后续任务里「这个标签正在生成答案」的 UI 状态查询
    /// （与 `AgentManager::snapshot` / `tracked_sessions` 同理，先备着）。
    #[allow(dead_code)]
    pub fn is_running(&self, session_id: &str) -> bool {
        self.inner.inflight.lock().contains_key(session_id)
    }

    /// 发起一次提问。**立即返回**，文本通过事件推送。
    ///
    /// 上下文在这里取完就释放 `AgentManager` 的锁，之后的所有耗时动作
    /// （脱敏、组装、HTTP）都发生在 spawn 出去的任务里 —— 终端的 `feed` 路径
    /// 不会因为它们被堵住哪怕一毫秒。
    pub fn ask(
        &self,
        app: &AppHandle,
        mgr: &AgentManager,
        cfg: &AgentConfig,
        session_id: &str,
        question: &str,
    ) -> Result<(), AgentError> {
        validate(cfg)?;
        if session_id.trim().is_empty() {
            return Err(AgentError::NoSession(session_id.to_string()));
        }
        let question = question.trim();
        if question.is_empty() {
            return Err(AgentError::EmptyQuestion);
        }

        // 并发闸门：先登记再开流，登记失败说明已经有一条在跑
        let Some(flag) = self.begin(session_id) else {
            return Err(AgentError::Busy);
        };

        // —— 唯一一次碰 AgentManager 的锁，读完立刻释放 ——
        let acct_max = cfg.active_account().max_context_bytes;
        let max_bytes = if acct_max == 0 {
            DEFAULT_MAX_CONTEXT_BYTES
        } else {
            acct_max
        } as usize;
        let raw_context = mgr.context(session_id, max_bytes);

        let app = app.clone();
        let cfg = cfg.clone();
        let session = session_id.to_string();
        let question = question.to_string();
        let inner = Arc::clone(&self.inner);
        tauri::async_runtime::spawn(async move {
            // RAII：任务无论正常结束、被取消还是 panic 之后的 unwind，
            // 都会把这条登记摘掉，会话不会被永久卡在「提问中」。
            let _guard = InflightGuard {
                inner,
                session: session.clone(),
            };
            stream_answer(app, cfg, session, raw_context, question, flag).await;
        });
        Ok(())
    }
}

impl Default for LlmRuntime {
    fn default() -> Self {
        Self::new()
    }
}

/// 在 drop 时摘掉 in-flight 登记。
struct InflightGuard {
    inner: Arc<Inner>,
    session: String,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.inner.finish(&self.session);
        tracing::info!("AI 提问结束：{}", self.session);
    }
}

// ——————————————————————————————— 事件推送 ———————————————————————————————

/// `nx://agent/done/{session}` 的载荷。
///
/// `Clone` 不是装饰：tauri 的 `Emitter::emit` 要求 `S: Serialize + Clone`
/// （内部要把同一份载荷发给每个窗口），缺了它编译不过。
#[derive(Clone, Serialize)]
struct DonePayload {
    reason: FinishReason,
}

/// `nx://agent/token/{session}` → 增量文本块。
fn emit_token(app: &AppHandle, session: &str, text: &str) {
    if text.is_empty() {
        return;
    }
    let _ = app.emit(&format!("nx://agent/token/{session}"), text.to_string());
}

/// `nx://agent/done/{session}` → `{ reason }`。
fn emit_done(app: &AppHandle, session: &str, reason: FinishReason) {
    let _ = app.emit(
        &format!("nx://agent/done/{session}"),
        DonePayload { reason },
    );
}

/// `nx://agent/error/{session}` → 用户可读的错误文案。
///
/// error 是**终态**：发了它就不会再发 done，前端据此把状态机收掉。
fn emit_error(app: &AppHandle, session: &str, message: &str) {
    tracing::error!("AI 提问失败（{}）：{}", session, message);
    let _ = app.emit(
        &format!("nx://agent/error/{session}"),
        message.to_string(),
    );
}

// ——————————————————————————————— 流式主流程 ———————————————————————————————

/// 处理一行后流应当如何继续。
enum Step {
    Continue,
    Finished(FinishReason),
    /// 服务端在流里报错：发给前端后立刻收摊
    Aborted(String),
}

/// [`dispatch_line`] 的薄封装：把「动作」变成「副作用 + 如何继续」。
///
/// 单独拆出来是为了让主循环和「末尾残行」的处理共用同一套逻辑，
/// 而 [`dispatch_line`] 本身保持纯函数、可单测。
fn step(
    provider: Provider,
    state: &mut SseState,
    line: &str,
    app: &AppHandle,
    session: &str,
) -> Step {
    match dispatch_line(provider, state, line) {
        SseAction::Nothing => Step::Continue,
        SseAction::Text(text) => {
            emit_token(app, session, &text);
            Step::Continue
        }
        SseAction::Done(reason) => Step::Finished(reason),
        SseAction::Failed(message) => Step::Aborted(message),
    }
}

/// 一次完整的流式问答。任何失败都以 `error` 事件收场，不 panic、不向上冒泡。
async fn stream_answer(
    app: AppHandle,
    cfg: AgentConfig,
    session: String,
    raw_context: Option<String>,
    question: String,
    cancel: CancelFlag,
) {
    let acct = cfg.active_account();
    let provider = resolve_provider(acct);
    // —— 出网前最后一个闸门 ——
    let context = raw_context.as_deref().map(|c| sanitize_context(acct, c));
    let user = build_user_message(context.as_deref(), &question);
    let prepared = match build_request(acct, provider, system_prompt(acct), &user, true) {
        Ok(p) => p,
        Err(e) => {
            emit_error(&app, &session, &e.to_string());
            return;
        }
    };

    let client = match http_client() {
        Ok(c) => c,
        Err(e) => {
            emit_error(&app, &session, &e.to_string());
            return;
        }
    };

    let mut headers = HeaderMap::new();
    for (name, value) in &prepared.headers {
        let name = match HeaderName::from_bytes(name.as_bytes()) {
            Ok(n) => n,
            Err(_) => {
                emit_error(&app, &session, "请求头非法，请检查配置");
                return;
            }
        };
        let value = match HeaderValue::from_str(value) {
            Ok(v) => v,
            // 九成是 API Key 被复制时带进了换行
            Err(_) => {
                emit_error(&app, &session, "API Key 含有非法字符（换行等），请重新复制");
                return;
            }
        };
        headers.append(name, value);
    }

    // 用户在握手之前就点了取消：不必再发请求
    if cancel.load(Ordering::Relaxed) {
        tracing::info!("AI 提问在发起前被取消：{}", session);
        emit_done(&app, &session, FinishReason::Cancelled);
        return;
    }

    let resp = match client
        .post(&prepared.url)
        .headers(headers)
        .body(prepared.body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            emit_error(&app, &session, &transport_error(&e));
            return;
        }
    };

    let status = resp.status();
    if !status.is_success() {
        let body = read_error_body(resp).await;
        emit_error(&app, &session, &http_status_message(status, &body));
        return;
    }

    let mut stream = resp.bytes_stream();
    let mut decoder = SseDecoder::new();
    let mut state = SseState::default();
    let mut lines: Vec<String> = Vec::with_capacity(8);
    let mut finished: Option<FinishReason> = None;

    while let Some(item) = stream.next().await {
        // 取消检查放在每段的开头：置位后最多再多处理一个网络包就停。
        // 离开这个循环会 drop 掉 response，连接随之关闭，服务端就不再生成了。
        if cancel.load(Ordering::Relaxed) {
            finished = Some(FinishReason::Cancelled);
            break;
        }
        let chunk = match item {
            Ok(c) => c,
            Err(e) => {
                emit_error(&app, &session, &transport_error(&e));
                return;
            }
        };
        decoder.push(&chunk, &mut lines);
        for line in lines.drain(..) {
            match step(provider, &mut state, &line, &app, &session) {
                Step::Continue => {}
                Step::Finished(reason) => {
                    finished = Some(reason);
                    break;
                }
                Step::Aborted(message) => {
                    emit_error(&app, &session, &message);
                    return;
                }
            }
        }
        if finished.is_some() {
            break;
        }
    }

    // 流结束但最后一行没有换行符（服务端写完 [DONE] 直接关连接是常态）：
    // 冲刷残行再判一次，否则最后一段答案会丢。
    if finished.is_none() {
        if let Some(line) = decoder.finish() {
            match step(provider, &mut state, &line, &app, &session) {
                Step::Continue => {}
                Step::Finished(reason) => finished = Some(reason),
                Step::Aborted(message) => {
                    emit_error(&app, &session, &message);
                    return;
                }
            }
        }
    }

    match finished {
        Some(reason) => emit_done(&app, &session, reason),
        // 既没有 [DONE] 也没有 message_stop 就断了：按正常结束处理。
        // 少发一个 done，前端就会永远停在「生成中」。
        None => emit_done(&app, &session, FinishReason::Stop),
    }
}

// ——————————————————————————————— 测试连接 ———————————————————————————————

/// 从一次非流式响应里取出回答正文。
pub fn extract_answer_text(provider: Provider, body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let text = match provider {
        Provider::OpenAI => value.pointer("/choices/0/message/content"),
        Provider::Anthropic => value.pointer("/content/0/text"),
    };
    text.and_then(|v| v.as_str()).map(|s| s.to_string())
}

/// 从一次非流式响应里取出正文 + 工具调用。
///
/// 两种方言的形状差别不小，但都刻意不反序列化成具体类型：
/// 兼容层各家往 `tool_calls` / `content` 里塞的字段五花八门，
/// 用 `pointer` 逐个取，缺哪个就当那个没有，比整体反序列化抗造。
pub fn extract_reply(provider: Provider, body: &str) -> Option<LlmReply> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let mut reply = LlmReply::default();

    match provider {
        Provider::OpenAI => {
            if let Some(text) = value.pointer("/choices/0/message/content").and_then(|v| v.as_str())
            {
                reply.text = text.to_string();
            }
            if let Some(calls) =
                value.pointer("/choices/0/message/tool_calls").and_then(|v| v.as_array())
            {
                for c in calls {
                    let name = c.pointer("/function/name").and_then(|v| v.as_str()).unwrap_or_default();
                    if name.is_empty() {
                        continue;
                    }
                    // arguments 规范上是一段 JSON **字符串**；少数兼容层直接给对象。
                    // 两种都接住：对象就用它的文本形式，等于帮它补上一层引号。
                    let arguments = match c.pointer("/function/arguments").and_then(|v| v.as_str()) {
                        Some(s) => s.to_string(),
                        None => c
                            .pointer("/function/arguments")
                            .filter(|v| !v.is_null())
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "{}".to_string()),
                    };
                    reply.tool_calls.push(ToolCall { name: name.to_string(), arguments });
                }
            }
        }
        Provider::Anthropic => {
            // Anthropic 把正文和工具调用混在同一个 content 数组里，按 type 分拣。
            // 只取 /content/0/text 会漏掉「模型边说边调」时排在后面的 text 块。
            if let Some(blocks) = value.pointer("/content").and_then(|v| v.as_array()) {
                for b in blocks {
                    match b.get("type").and_then(|v| v.as_str()).unwrap_or_default() {
                        "text" => {
                            if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                                reply.text.push_str(t);
                            }
                        }
                        "tool_use" => {
                            let name = b.get("name").and_then(|v| v.as_str()).unwrap_or_default();
                            if name.is_empty() {
                                continue;
                            }
                            let arguments = b
                                .get("input")
                                .filter(|v| !v.is_null())
                                .map(|v| v.to_string())
                                .unwrap_or_else(|| "{}".to_string());
                            reply.tool_calls.push(ToolCall { name: name.to_string(), arguments });
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    Some(reply)
}

/// 服务方是否在抱怨我们传了 tools。
///
/// 只按关键字猜，不做结构化解析：各家报错文案完全不同
/// （`Unknown parameter: tools` / `tools is not supported` / `Invalid value for 'tools'` …），
/// 而这里的代价只是一次重试，宁可多判一点。
fn is_tool_unsupported(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("tools")
        && (lower.contains("unknown parameter")
            || lower.contains("unsupported")
            || lower.contains("not supported")
            || lower.contains("invalid value")
            || lower.contains("does not support")
            || lower.contains("unrecognized"))
}

/// 单次 actor 回应体上限。
///
/// 一次完整的 HTTP 响应够用了；超限时会被截断。
const MAX_COMPLETE_BODY: usize = 512 * 1024;

/// 等到取消标记被置位。
///
/// 只轮询布尔量，不做任何 I/O。配合 `tokio::select!` 就能给任意请求安上一个
/// 「用户点了取消就立刻放弃」的出口 —— 否则一条三十秒的生成在这期间完全无法打断。
async fn until_cancelled(cancel: &AtomicBool) {
    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 带工具调用的 [`complete`]（**Agentic 循环专用**）。
///
/// 返回正文 + 模型要求调用的工具。`tools` 为空时等价 [`complete`]。
///
/// **服务方不认 tools 时自动降级**：拿掉 tools 重发一次，并把
/// `tools_supported` 置回 false。这样接第三方兼容层（不少自建网关只转发
/// messages）不会直接把整个 Agentic 循环打死 —— 调用方据此改回围栏协议，
/// 而围栏协议本来就是它认的老路。
pub async fn complete_reply(
    acct: &AgentAccount,
    system: &str,
    messages: &[ChatMessage],
    tools: &[ToolSpec],
    cancel: Option<&AtomicBool>,
) -> Result<LlmReply, AgentError> {
    // 首轮先按「支持 tools」发
    let first = post_chat(acct, system, messages, tools, cancel).await?;
    let provider = first.provider;

    let mut degraded = false;
    let (status, body) = if !first.status.is_success()
        && !tools.is_empty()
        && is_tool_unsupported(&first.body)
    {
        tracing::warn!("服务方不接受 tools 声明（HTTP {}），本轮起改为纯文本协议", first.status.as_u16());
        degraded = true;
        post_chat(acct, system, messages, &[], cancel).await?.into_parts()
    } else {
        first.into_parts()
    };

    if !status.is_success() {
        return Err(AgentError::Transport(http_status_message(status, &body)));
    }

    let mut reply = extract_reply(provider, &body).unwrap_or_default();
    reply.tools_supported = !tools.is_empty() && !degraded;
    if reply.text.trim().is_empty() && reply.tool_calls.is_empty() {
        return Err(AgentError::Transport(
            "模型返回了空回复：请确认该模型可用于对话接口".into(),
        ));
    }
    Ok(reply)
}

/// 发一次非流式请求。返回「状态码 + 响应体 + 解析出的 provider」。
///
/// 三条出网路径（complete / complete_reply / 降级重试）共用，
/// 免得取消判定与限长读取被复制三份。
async fn post_chat(
    acct: &AgentAccount,
    system: &str,
    messages: &[ChatMessage],
    tools: &[ToolSpec],
    cancel: Option<&AtomicBool>,
) -> Result<PostResult, AgentError> {
    if let Some(flag) = cancel {
        if flag.load(Ordering::Relaxed) {
            return Err(AgentError::Transport("已取消".into()));
        }
    }
    let provider = resolve_provider(acct);
    let prepared = build_messages_with_tools(acct, provider, system, messages, false, tools)?;
    let client = http_client()?;
    let headers = header_map(&prepared.headers)?;

    let request = client
        .post(&prepared.url)
        .headers(headers)
        .body(prepared.body)
        .send();

    let resp = match cancel {
        Some(flag) => match tokio::select! {
            r = request => r,
            _ = until_cancelled(flag) => return Err(AgentError::Transport("已取消".into())),
        } {
            Ok(r) => r,
            Err(e) => return Err(AgentError::Transport(transport_error(&e))),
        },
        None => request
            .await
            .map_err(|e| AgentError::Transport(transport_error(&e)))?,
    };

    let status = resp.status();
    // 成功响应也可以很大（模型可能一口气吐出几十 KB），统一限长
    let body = if status.is_success() {
        read_body_limited(resp, MAX_COMPLETE_BODY).await
    } else {
        read_error_body(resp).await
    };
    Ok(PostResult { status, body, provider })
}

/// [`post_chat`] 的结果。小结构是为了让重试那行能直接 `.into_parts()`。
struct PostResult {
    status: StatusCode,
    body: String,
    provider: Provider,
}

impl PostResult {
    fn into_parts(self) -> (reqwest::StatusCode, String) {
        (self.status, self.body)
    }
}

/// 用一条极短消息验证 Key / 地址 / 模型是否可用，供设置界面「测试连接」用。
///
/// 走**非流式**：少一层 SSE 解析，失败原因更干净（Key 错 / 地址错 / 模型不存在
/// 都能在状态码上区分），也省掉一次几十秒的生成。
///
/// 刻意不接收 `AppState`：测的是用户刚填进去、还没保存的那份配置，
/// 与库里已保存的、以及运行时状态都无关。
pub async fn test_connection(cfg: &AgentConfig) -> Result<String, AgentError> {
    validate(cfg)?;
    let acct = cfg.active_account();
    let provider = resolve_provider(acct);
    let prepared = build_request(acct, provider, system_prompt(acct), TEST_QUESTION, false)?;
    let client = http_client()?;
    let headers = header_map(&prepared.headers)?;

    let resp = client
        .post(&prepared.url)
        .headers(headers)
        .body(prepared.body)
        .send()
        .await
        .map_err(|e| AgentError::Transport(transport_error(&e)))?;

    let status = resp.status();
    // 成功响应很小（一条短回复）；错误体才需要限长
    let body = if status.is_success() {
        resp.text().await.unwrap_or_default()
    } else {
        read_error_body(resp).await
    };
    if !status.is_success() {
        return Err(AgentError::Transport(http_status_message(status, &body)));
    }

    match extract_answer_text(provider, &body) {
        Some(text) if !text.trim().is_empty() => Ok(format!(
            "连接成功：模型 {} 已响应，回复「{}」",
            acct.model.trim(),
            truncate_chars(text.trim(), 60)
        )),
        _ => Ok(format!(
            "连接成功：模型 {} 已响应，但正文为空（请确认该模型可用于对话接口）",
            acct.model.trim()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::redact::PLACEHOLDER;

    /// 测试账号：带一个可用的 key，其余取默认。
    fn acct() -> AgentAccount {
        AgentAccount {
            id: "default".into(),
            name: "默认".into(),
            api_key: "sk-test-key".into(),
            ..Default::default()
        }
    }

    /// 测试配置：启用 + 单账号（key 已填）。validate 这类按 AgentConfig 的测试用它。
    fn cfg() -> AgentConfig {
        AgentConfig {
            enabled: true,
            auto_attach: false,
            active_id: "default".into(),
            accounts: vec![acct()],
            ..Default::default()
        }
    }

    // —————————————— SSE 行缓冲 ——————————————

    #[test]
    fn half_line_is_held_until_newline_arrives() {
        let mut d = SseDecoder::new();
        let mut out = Vec::new();
        assert_eq!(d.push(b"data: {\"a\":1", &mut out), 0, "半行不该产出");
        assert_eq!(d.push(b"}\n", &mut out), 1);
        assert_eq!(out, vec![r#"data: {"a":1}"#.to_string()]);
    }

    #[test]
    fn line_split_across_three_chunks_is_reassembled() {
        let mut d = SseDecoder::new();
        let mut out = Vec::new();
        d.push(b"data: hel", &mut out);
        d.push(b"lo wor", &mut out);
        d.push(b"ld\n", &mut out);
        assert_eq!(out, vec!["data: hello world".to_string()]);
    }

    #[test]
    fn multiple_lines_in_one_chunk() {
        let mut d = SseDecoder::new();
        let mut out = Vec::new();
        assert_eq!(d.push(b"a\nb\nc\n", &mut out), 3);
        assert_eq!(out, vec!["a", "b", "c"]);
    }

    #[test]
    fn crlf_line_ending_is_stripped() {
        let mut d = SseDecoder::new();
        let mut out = Vec::new();
        d.push(b"data: x\r\ndata: y\r\n", &mut out);
        assert_eq!(out, vec!["data: x", "data: y"]);
    }

    /// 空行是事件块的边界，必须原样交给上层 —— Anthropic 靠它重置 event 名
    #[test]
    fn blank_lines_are_preserved() {
        let mut d = SseDecoder::new();
        let mut out = Vec::new();
        d.push(b"data: one\n\ndata: two\n", &mut out);
        assert_eq!(out, vec!["data: one", "", "data: two"]);
    }

    #[test]
    fn finish_flushes_trailing_line_without_newline() {
        let mut d = SseDecoder::new();
        let mut out = Vec::new();
        d.push(b"data: [DONE]", &mut out);
        assert!(out.is_empty());
        assert_eq!(d.finish().as_deref(), Some("data: [DONE]"));
        assert_eq!(d.finish(), None, "冲刷过一次就不该再有残留");
    }

    /// 多字节字符正好被网络包切开：等换行再解码才能保证字符完整
    #[test]
    fn utf8_split_across_chunks_stays_intact() {
        let mut d = SseDecoder::new();
        let mut out = Vec::new();
        let text = "data: 中文回答";
        let bytes = text.as_bytes();
        // 在第 8 个字节处切一刀，正好切在某个汉字中间
        d.push(&bytes[..8], &mut out);
        d.push(&bytes[8..], &mut out);
        d.push(b"\n", &mut out);
        assert_eq!(out, vec![text.to_string()]);
    }

    #[test]
    fn empty_chunk_produces_nothing() {
        let mut d = SseDecoder::new();
        let mut out = Vec::new();
        d.push(b"", &mut out);
        assert!(out.is_empty());
        assert_eq!(d.finish(), None);
    }

    /// 不发换行的服务端不能把内存吃光
    #[test]
    fn overlong_line_is_dropped() {
        let mut d = SseDecoder::new();
        let mut out = Vec::new();
        let flood = vec![b'x'; MAX_LINE_BYTES + 10];
        d.push(&flood, &mut out);
        assert!(out.is_empty(), "超长行不该产出");
        // 丢弃之后仍然可用：后续正常行照常产出。
        // 前导 \n 会先补出一个空行（那是被截掉的行留下的边界），
        // 上层对空行本就当作事件分隔符处理，故无害。
        d.push(b"\ndata: ok\n", &mut out);
        assert!(out.iter().all(|l| !l.contains('x')), "废弃内容不该再出现");
        assert_eq!(out.last().map(String::as_str), Some("data: ok"));
    }

    // —————————————— data 行识别 ——————————————

    #[test]
    fn data_prefix_with_and_without_space() {
        assert_eq!(parse_data_line("data: {}\n".trim_end()), Some("{}"));
        assert_eq!(parse_data_line("data:{}"), Some("{}"));
        assert_eq!(parse_data_line("data: [DONE]"), Some("[DONE]"));
    }

    #[test]
    fn comment_lines_are_not_data() {
        assert_eq!(parse_data_line(": ping - keepalive"), None);
        assert_eq!(parse_data_line(""), None, "空行由 dispatch_line 处理");
    }

    #[test]
    fn event_lines_are_not_data() {
        assert_eq!(parse_data_line("event: content_block_delta"), None);
    }

    // —————————————— OpenAI 事件 ——————————————

    #[test]
    fn openai_text_delta() {
        let data = r#"{"choices":[{"delta":{"content":"你好"}}]}"#;
        assert_eq!(
            parse_openai_event(data),
            SseAction::Text("你好".to_string())
        );
    }

    #[test]
    fn openai_done_marker() {
        assert_eq!(parse_openai_event("[DONE]"), SseAction::Done(FinishReason::Stop));
    }

    #[test]
    fn openai_finish_reason_length() {
        let data = r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#;
        assert_eq!(
            parse_openai_event(data),
            SseAction::Done(FinishReason::Length)
        );
    }

    /// 最后一个 chunk 常常 content 与 finish_reason 同时出现，不能丢文本
    #[test]
    fn openai_prefers_text_over_finish_reason() {
        let data = r#"{"choices":[{"delta":{"content":"。"},"finish_reason":"stop"}]}"#;
        assert_eq!(
            parse_openai_event(data),
            SseAction::Text("。".to_string())
        );
    }

    #[test]
    fn openai_in_stream_error() {
        let data = r#"{"error":{"message":"Rate limit reached","type":"rate_limit"}}"#;
        assert_eq!(
            parse_openai_event(data),
            SseAction::Failed("Rate limit reached".to_string())
        );
    }

    #[test]
    fn openai_garbage_is_ignored() {
        assert_eq!(parse_openai_event("not json"), SseAction::Nothing);
        assert_eq!(parse_openai_event(""), SseAction::Nothing);
        assert_eq!(parse_openai_event("{}"), SseAction::Nothing);
    }

    // —————————————— Anthropic 事件 ——————————————

    #[test]
    fn anthropic_content_block_delta() {
        let data = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#;
        assert_eq!(
            parse_anthropic_event(Some("content_block_delta"), data),
            SseAction::Text("hi".to_string())
        );
    }

    /// 网关剥掉 event 行时必须靠 JSON 里的 type 兜住
    #[test]
    fn anthropic_falls_back_to_json_type() {
        let data = r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}"#;
        assert_eq!(
            parse_anthropic_event(None, data),
            SseAction::Text("hi".to_string())
        );
    }

    #[test]
    fn anthropic_message_stop() {
        assert_eq!(
            parse_anthropic_event(Some("message_stop"), r#"{"type":"message_stop"}"#),
            SseAction::Done(FinishReason::Stop)
        );
    }

    #[test]
    fn anthropic_message_delta_max_tokens() {
        let data = r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":4096}}"#;
        assert_eq!(
            parse_anthropic_event(Some("message_delta"), data),
            SseAction::Done(FinishReason::Length)
        );
    }

    #[test]
    fn anthropic_message_delta_end_turn() {
        let data = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#;
        assert_eq!(
            parse_anthropic_event(Some("message_delta"), data),
            SseAction::Done(FinishReason::Stop)
        );
    }

    #[test]
    fn anthropic_error_event() {
        let data = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        assert_eq!(
            parse_anthropic_event(Some("error"), data),
            SseAction::Failed("Overloaded".to_string())
        );
    }

    #[test]
    fn anthropic_ping_is_ignored() {
        assert_eq!(
            parse_anthropic_event(Some("ping"), r#"{"type":"ping"}"#),
            SseAction::Nothing
        );
        assert_eq!(
            parse_anthropic_event(Some("message_start"), r#"{"type":"message_start","message":{}}"#),
            SseAction::Nothing
        );
    }

    // —————————————— dispatch_line 状态机 ——————————————

    #[test]
    fn dispatch_tracks_event_name_across_lines() {
        let mut st = SseState::default();
        // OpenAI 不认识 event 行，但有也不该出错
        assert_eq!(
            dispatch_line(Provider::OpenAI, &mut st, "event: whatever"),
            SseAction::Nothing
        );
        assert_eq!(
            dispatch_line(
                Provider::OpenAI,
                &mut st,
                r#"data: {"choices":[{"delta":{"content":"a"}}]}"#
            ),
            SseAction::Text("a".to_string())
        );
    }

    /// 空行必须清掉 event 名：否则下一个事件的 data 会被上一个事件名误判
    #[test]
    fn blank_line_resets_event_name() {
        let mut st = SseState::default();
        dispatch_line(Provider::Anthropic, &mut st, "event: message_delta");
        assert_eq!(st.event.as_deref(), Some("message_delta"));
        dispatch_line(Provider::Anthropic, &mut st, "");
        assert_eq!(st.event, None);
        // 重置后同一条 data 不再被当成 message_delta
        let data = r#"{"type":"content_block_delta","delta":{"text":"z"}}"#;
        assert_eq!(
            dispatch_line(Provider::Anthropic, &mut st, &format!("data: {data}")),
            SseAction::Text("z".to_string())
        );
    }

    // —————————————— 请求组装 ——————————————

    fn body_of(req: &PreparedRequest) -> serde_json::Value {
        serde_json::from_str(&req.body).expect("body 应是合法 JSON")
    }

    #[test]
    fn endpoint_trims_trailing_slash() {
        let a = AgentAccount {
            base_url: "https://api.openai.com/v1/".into(),
            ..acct()
        };
        assert_eq!(
            endpoint(&a, Provider::OpenAI),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(endpoint(&a, Provider::Anthropic), "https://api.openai.com/v1/messages");
    }

    /// 用户误填完整终点 URL 时，endpoint 要剥掉重复后缀而不是拼两次（曾导致 404）
    #[test]
    fn endpoint_strips_duplicate_endpoint_suffix() {
        let a = AgentAccount {
            base_url: "http://10.0.3.112/v1/chat/completions".into(),
            ..acct()
        };
        assert_eq!(
            endpoint(&a, Provider::OpenAI),
            "http://10.0.3.112/v1/chat/completions",
            "填了完整终点也应只有一个 /chat/completions"
        );
        let a = AgentAccount {
            provider: "anthropic".into(),
            base_url: "https://api.anthropic.com/messages".into(),
            ..acct()
        };
        assert_eq!(endpoint(&a, Provider::Anthropic), "https://api.anthropic.com/messages");
    }

    #[test]
    fn openai_request_shape() {
        let req = build_request(&acct(), Provider::OpenAI, "SYS", "USER", true).expect("应组装成功");
        assert_eq!(req.url, "https://api.openai.com/v1/chat/completions");
        assert!(req
            .headers
            .contains(&("Authorization".into(), "Bearer sk-test-key".into())));
        assert!(req
            .headers
            .contains(&("Content-Type".into(), "application/json".into())));

        let body = body_of(&req);
        assert_eq!(body["model"], "gpt-4o-mini");
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "USER");
        // OpenAI 方言的 system 走 messages，body 里不该有顶层 system
        assert!(body.get("system").is_none());
    }

    /// 流式要 Accept: text/event-stream，非流式（测试连接）不该带上它
    #[test]
    fn accept_header_only_for_streaming() {
        let streaming = build_request(&acct(), Provider::OpenAI, "S", "U", true).expect("应组装成功");
        assert!(streaming
            .headers
            .contains(&("Accept".into(), "text/event-stream".into())));

        let one_shot = build_request(&acct(), Provider::OpenAI, "S", "U", false).expect("应组装成功");
        assert!(!one_shot.headers.iter().any(|(k, _)| k == "Accept"));
        assert_eq!(body_of(&one_shot)["stream"], false);
    }

    #[test]
    fn anthropic_request_shape() {
        let a = AgentAccount {
            provider: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            model: "claude-3-5-sonnet".into(),
            ..acct()
        };
        let req = build_request(&a, Provider::Anthropic, "SYS", "USER", true).expect("应组装成功");
        assert_eq!(req.url, "https://api.anthropic.com/messages");
        assert!(req.headers.contains(&("x-api-key".into(), "sk-test-key".into())));
        assert!(req
            .headers
            .contains(&("anthropic-version".into(), ANTHROPIC_VERSION.into())));

        let body = body_of(&req);
        assert_eq!(body["model"], "claude-3-5-sonnet");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], ANTHROPIC_MAX_TOKENS);
        assert_eq!(body["system"], "SYS", "system 必须是顶层字段");
        assert_eq!(body["messages"][0]["content"], "USER");
        // Anthropic 的 messages 里不能出现 system 角色
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn api_key_is_trimmed() {
        let a = AgentAccount {
            api_key: "  sk-abc\n".into(),
            ..acct()
        };
        let req = build_request(&a, Provider::OpenAI, "S", "U", false).expect("应组装成功");
        assert!(req
            .headers
            .contains(&("Authorization".into(), "Bearer sk-abc".into())));
    }

    #[test]
    fn provider_resolution_is_case_insensitive() {
        let a = AgentAccount {
            provider: " Anthropic ".into(),
            ..acct()
        };
        assert_eq!(resolve_provider(&a), Provider::Anthropic);
        // 未知值落到兼容分支，而不是报错
        let a = AgentAccount {
            provider: "custom".into(),
            ..acct()
        };
        assert_eq!(resolve_provider(&a), Provider::OpenAI);
    }

    #[test]
    fn missing_model_or_url_is_rejected() {
        let a = AgentAccount {
            model: "  ".into(),
            ..acct()
        };
        assert!(matches!(
            build_request(&a, Provider::OpenAI, "S", "U", true),
            Err(AgentError::MissingModel)
        ));
        let a = AgentAccount {
            base_url: String::new(),
            ..acct()
        };
        assert!(matches!(
            build_request(&a, Provider::OpenAI, "S", "U", true),
            Err(AgentError::BadRequest(_))
        ));
    }

    // —————————————— 系统提示词 ——————————————

    #[test]
    fn blank_system_prompt_falls_back() {
        let a = AgentAccount {
            system_prompt: "   ".into(),
            ..acct()
        };
        let s = system_prompt(&a);
        assert_eq!(s, FALLBACK_SYSTEM_PROMPT);
        // 四条要点必须在兜底提示词里
        for key in ["终端会话", "不要臆测", "不确定", "风险"] {
            assert!(s.contains(key), "兜底提示词缺少要点：{key}");
        }
    }

    #[test]
    fn custom_system_prompt_is_used_verbatim() {
        let a = AgentAccount {
            system_prompt: "你是我的助手".into(),
            ..acct()
        };
        assert_eq!(system_prompt(&a), "你是我的助手");
    }

    // —————————————— 上下文与脱敏 ——————————————

    const RAW_CONTEXT: &str = "$ export OPENAI_API_KEY=sk-abcdefgh12345678\n$ curl https://h/1.2.3.4\n";

    #[test]
    fn redact_on_masks_secrets_before_leaving() {
        let a = AgentAccount {
            redact: true,
            ..acct()
        };
        let safe = sanitize_context(&a, RAW_CONTEXT);
        assert!(!safe.contains("sk-abcdefgh12345678"), "原文密钥漏出去了");
        assert!(safe.contains(PLACEHOLDER));
        assert!(safe.starts_with("$ export OPENAI_API_KEY="), "脱敏应保留可读性");
    }

    #[test]
    fn redact_off_keeps_original_text() {
        let a = AgentAccount {
            redact: false,
            ..acct()
        };
        assert_eq!(sanitize_context(&a, RAW_CONTEXT), RAW_CONTEXT);
    }

    #[test]
    fn user_message_wraps_context_with_markers() {
        let msg = build_user_message(Some("$ ls\nfile.txt\n"), "为什么失败了？");
        assert!(msg.starts_with(CONTEXT_INTRO));
        assert!(msg.contains(CONTEXT_BEGIN));
        assert!(msg.contains("$ ls\nfile.txt"));
        assert!(msg.contains(CONTEXT_END));
        assert!(msg.ends_with("我的问题：为什么失败了？"));
        // 上下文与问题之间要有空行
        assert!(msg.contains(&format!("{CONTEXT_END}\n\n我的问题：")));
    }

    /// 上下文为 None（会话没开旁路）或空白（被清空）时，必须让模型知道
    #[test]
    fn missing_context_is_announced_not_silent() {
        for ctx in [None, Some(""), Some("   \n ")] {
            let msg = build_user_message(ctx, "怎么了？");
            assert!(msg.contains(CONTEXT_MISSING), "空上下文不该静默：{msg}");
            assert!(!msg.contains(CONTEXT_BEGIN));
            assert!(msg.ends_with("我的问题：怎么了？"));
        }
    }

    #[test]
    fn question_is_never_dropped() {
        let msg = build_user_message(None, "这条命令什么意思");
        assert!(msg.contains("这条命令什么意思"));
    }

    // —————————————— 错误文案 ——————————————

    #[test]
    fn status_codes_map_to_chinese_hints() {
        let cases = [
            (401, "API Key"),
            (403, "API Key"),
            (429, "限流"),
            (404, "接口地址"),
            (402, "余额不足"),
            (400, "请求被拒绝"),
            (503, "服务端故障"),
        ];
        for (code, hint) in cases {
            let msg = http_status_message(StatusCode::from_u16(code).unwrap(), "");
            assert!(msg.contains(hint), "HTTP {code} 的文案缺少「{hint}」：{msg}");
            assert!(msg.contains(&code.to_string()), "应带上状态码：{msg}");
        }
    }

    #[test]
    fn server_error_message_is_kept_as_detail() {
        let body = r#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error"}}"#;
        let msg = http_status_message(StatusCode::UNAUTHORIZED, body);
        assert!(msg.contains("Incorrect API key provided"));
        assert!(msg.contains(crate::errors::RAW_SUFFIX), "应沿用统一的小字格式");
    }

    #[test]
    fn html_error_page_is_truncated() {
        let page = "<html>".to_string() + &"x".repeat(5000) + "</html>";
        let detail = extract_error_detail(&page);
        assert!(detail.chars().count() <= MAX_ERROR_DETAIL + 1);
        assert!(detail.ends_with('…'));
    }

    #[test]
    fn json_without_known_fields_yields_no_detail() {
        assert_eq!(extract_error_detail(r#"{"weird":{"a":1}}"#), "");
        assert_eq!(extract_error_detail(""), "");
    }

    #[test]
    fn truncate_never_splits_a_char() {
        assert_eq!(truncate_chars("中文测试", 2), "中文…");
        assert_eq!(truncate_chars("abc", 10), "abc");
    }

    // —————————————— 并发与取消 ——————————————

    #[test]
    fn second_ask_on_same_session_is_rejected() {
        let rt = LlmRuntime::new();
        assert!(rt.begin("s1").is_some());
        assert!(rt.begin("s1").is_none(), "并发提问必须被拒绝");
        assert!(rt.begin("s2").is_some(), "不同会话互不影响");
    }

    #[test]
    fn cancel_sets_the_flag_and_clears_on_finish() {
        let rt = LlmRuntime::new();
        let flag = rt.begin("s1").expect("首次登记应成功");
        assert!(rt.is_running("s1"));
        assert!(rt.cancel("s1"));
        assert!(flag.load(Ordering::Relaxed), "取消标记应已置位");

        // 走 Inner 而不是 LlmRuntime：生产路径里摘登记的是 InflightGuard::drop
        rt.inner.finish("s1");
        assert!(!rt.is_running("s1"));
        assert!(!rt.cancel("s1"), "没有进行中的提问时取消应返回 false");
        // 结束后可以再次提问
        assert!(rt.begin("s1").is_some());
    }

    #[test]
    fn validation_rejects_incomplete_config() {
        assert!(matches!(validate(&AgentConfig::default()), Err(AgentError::Disabled)));
        let c = AgentConfig {
            accounts: vec![AgentAccount { api_key: String::new(), ..acct() }],
            ..cfg()
        };
        assert!(matches!(validate(&c), Err(AgentError::MissingKey)));
        let c = AgentConfig {
            accounts: vec![AgentAccount { base_url: String::new(), ..acct() }],
            ..cfg()
        };
        assert!(matches!(validate(&c), Err(AgentError::BadRequest(_))));
    }

    /// done 事件的载荷形状是给前端的契约，改动会静默破坏前端
    #[test]
    fn done_payload_serializes_as_expected() {
        let json = serde_json::to_value(DonePayload {
            reason: FinishReason::Cancelled,
        })
        .expect("应能序列化");
        assert_eq!(json, serde_json::json!({"reason": "cancelled"}));
        let json = serde_json::to_value(DonePayload {
            reason: FinishReason::Length,
        })
        .expect("应能序列化");
        assert_eq!(json, serde_json::json!({"reason": "length"}));
    }

    #[test]
    fn answer_text_is_extracted_per_provider() {
        let openai = r#"{"choices":[{"message":{"role":"assistant","content":"ok"}}]}"#;
        assert_eq!(
            extract_answer_text(Provider::OpenAI, openai).as_deref(),
            Some("ok")
        );
        let anthropic = r#"{"content":[{"type":"text","text":"ok"}]}"#;
        assert_eq!(
            extract_answer_text(Provider::Anthropic, anthropic).as_deref(),
            Some("ok")
        );
        assert_eq!(extract_answer_text(Provider::OpenAI, "not json"), None);
    }

    // ——————————————————— 工具调用：wire format ———————————————————

    fn spec() -> ToolSpec {
        ToolSpec {
            name: "nx_tab",
            description: "操作标签页".into(),
            schema: serde_json::json!({"type": "object", "properties": {"op": {"type": "string"}}}),
        }
    }

    /// OpenAI 侧：function 外面要包 `type:"function"`，且必须有 tool_choice。
    #[test]
    fn openai_wire_declares_tools() {
        let req = build_messages_with_tools(
            &acct(),
            Provider::OpenAI,
            "SYS",
            &[ChatMessage::user("U")],
            false,
            &[spec()],
        )
        .expect("应组装成功");
        let json: serde_json::Value = serde_json::from_str(&req.body).expect("body 应是 JSON");
        assert_eq!(json["tools"][0]["type"], "function");
        assert_eq!(json["tools"][0]["function"]["name"], "nx_tab");
        assert_eq!(json["tools"][0]["function"]["parameters"]["type"], "object");
        assert_eq!(json["tool_choice"]["type"], "auto");
    }

    /// Anthropic 侧：name/description/input_schema 是平铺的，没有 function 包装层。
    /// 写成 OpenAI 的形状会被整个拒掉。
    #[test]
    fn anthropic_wire_declares_tools_flat() {
        let req = build_messages_with_tools(
            &acct(),
            Provider::Anthropic,
            "SYS",
            &[ChatMessage::user("U")],
            false,
            &[spec()],
        )
        .expect("应组装成功");
        let json: serde_json::Value = serde_json::from_str(&req.body).expect("body 应是 JSON");
        assert_eq!(json["tools"][0]["name"], "nx_tab");
        assert!(json["tools"][0]["function"].is_null(), "Anthropic 不该有 function 包装层");
        assert_eq!(json["tools"][0]["input_schema"]["type"], "object");
    }

    /// 不带 tools 时，请求里不该出现 tools / tool_choice 字段 ——
    /// 部分兼容层收到不认识的参数名会直接 400，而不是忽略。
    #[test]
    fn no_tools_leaves_no_trace_in_body() {
        for provider in [Provider::OpenAI, Provider::Anthropic] {
            let req = build_messages(&acct(), provider, "SYS", &[ChatMessage::user("U")], false)
                .expect("应组装成功");
            let json: serde_json::Value = serde_json::from_str(&req.body).expect("body 应是 JSON");
            assert!(json.get("tools").is_none(), "{provider:?} 不该带 tools");
            assert!(json.get("tool_choice").is_none(), "{provider:?} 不该带 tool_choice");
        }
    }

    /// OpenAI 的 arguments 按规范是**字符串**。
    #[test]
    fn openai_tool_calls_are_parsed() {
        let body = r#"{"choices":[{"message":{"content":"先看看",
            "tool_calls":[{"id":"c1","type":"function",
            "function":{"name":"nx_tab","arguments":"{\"op\":\"list\"}"}}]}}]}"#;
        let reply = extract_reply(Provider::OpenAI, body).expect("应解析成功");
        assert_eq!(reply.text, "先看看");
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].name, "nx_tab");
        assert_eq!(reply.tool_calls[0].arguments, r#"{"op":"list"}"#);
    }

    /// 有些兼容层把 arguments 直接给成对象而不是字符串，两种都要接住。
    #[test]
    fn openai_tool_calls_accept_object_arguments() {
        let body = r#"{"choices":[{"message":{"content":null,
            "tool_calls":[{"function":{"name":"nx_exec","arguments":{"command":"uptime"}}}]}}]}"#;
        let reply = extract_reply(Provider::OpenAI, body).expect("应解析成功");
        assert_eq!(reply.tool_calls[0].arguments, r#"{"command":"uptime"}"#);
    }

    /// Anthropic 把正文与工具调用混在同一个 content 数组里。
    /// 只取 content[0] 会漏掉「边说边调」时排在后面的 text 块。
    #[test]
    fn anthropic_tool_use_is_parsed_alongside_text() {
        let body = r#"{"content":[
            {"type":"text","text":"先看看有哪些标签页"},
            {"type":"tool_use","id":"tu1","name":"nx_tab","input":{"op":"list"}},
            {"type":"text","text":"，再说。"}]}"#;
        let reply = extract_reply(Provider::Anthropic, body).expect("应解析成功");
        assert_eq!(reply.text, "先看看有哪些标签页，再说。");
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].name, "nx_tab");
        assert_eq!(reply.tool_calls[0].arguments, r#"{"op":"list"}"#);
    }

    /// 没调工具时 tool_calls 必须是空，且正文照常取出 ——
    /// 上层靠「有没有工具调用」来决定走工具路径还是围栏路径。
    #[test]
    fn plain_answer_has_no_tool_calls() {
        let oai = r#"{"choices":[{"message":{"content":"已经查清楚了"}}]}"#;
        let reply = extract_reply(Provider::OpenAI, oai).expect("应解析成功");
        assert!(reply.tool_calls.is_empty());
        assert_eq!(reply.text, "已经查清楚了");

        let anth = r#"{"content":[{"type":"text","text":"已经查清楚了"}]}"#;
        let reply = extract_reply(Provider::Anthropic, anth).expect("应解析成功");
        assert!(reply.tool_calls.is_empty());
    }

    /// 降级判据：必须是「在抱怨 tools」才算，普通的 400 不能触发重试。
    #[test]
    fn tool_unsupported_is_detected_only_for_tools() {
        assert!(is_tool_unsupported(r#"{"error":{"message":"Unknown parameter: tools"}}"#));
        assert!(is_tool_unsupported("tools is not supported by this model"));
        assert!(is_tool_unsupported("Invalid value for 'tools'"));
        // 普通的 400（比如 context length）不该被当成「不支持 tools」
        assert!(!is_tool_unsupported("maximum context length exceeded"));
        assert!(!is_tool_unsupported("invalid api key"));
    }
}
