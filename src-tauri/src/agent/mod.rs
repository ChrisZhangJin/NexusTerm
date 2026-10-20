//! AI Agent 旁路：把每个终端会话的输出一边照常投到屏幕上，
//! 一边悄悄复制一份给 AI agent。
//!
//! 有了完整的实时 console 流，agent 才知道人在这个会话里做过什么，
//! 才可能带着上下文回答「刚才那条命令为什么失败了」。
//!
//! ## 三条不可妥协的约束
//!
//! 1. **旁路永远不许影响主链路**。feed 失败只能 `tracing::warn`，
//!    绝不能让 agent 缓冲的异常拖死终端 —— 这是旁路，不是必经之路。
//! 2. **缓冲必须有上限**。长会话轻松几 MB，全喂给 LLM 既爆 token 也爆预算，
//!    所以留最近 N 字节的环形窗口，超限丢弃并记账给 UI。
//! 3. **不许在持锁时做耗时操作**。锁保护的是毫秒级的数据结构，
//!    HTTP 请求、磁盘 IO 一律拿到释放之后再发生。

pub mod buffer;
pub mod cmd;
pub mod llm;
pub mod redact;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

// 对外（ssh/session.rs、local/mod.rs、cmd.rs 的接线点）只需要这个来源标记，
// 故单独 pub use 出去，缓冲内部的实现细节仍留在本模块里
pub use self::buffer::StreamKind;
use self::buffer::{BufferStats, SessionBuffer, DEFAULT_CAPACITY};
use crate::store::Store;

/// 单个会话缓冲的默认容量（字节），即 `SessionBuffer::DEFAULT_CAPACITY`。
pub const DEFAULT_MAX_CONTEXT_BYTES: u32 = 32 * 1024;

/// 配置在 settings 表里的键名。
const CONFIG_KEY: &str = "agent.config";

/// AI 助手相关错误。
///
/// 与 SSH 那边的约定一致：命令层统一转成 `String`（带中文提示）交给前端。
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("AI 会话不存在：{0}")]
    NoSession(String),
    #[error("AI 上下文序列化失败：{0}")]
    Serialize(String),
    #[error("AI 配置持久化失败：{0}")]
    Store(String),

    // ——— 以下供 LLM 调用层（llm.rs）使用 ———
    #[error("AI 助手未启用：请先在设置里打开总开关")]
    Disabled,
    #[error("尚未填写 API Key：请到设置里填写后再提问")]
    MissingKey,
    #[error("尚未填写模型名称：请到设置里填写后再提问")]
    MissingModel,
    #[error("请先输入你要问的问题")]
    EmptyQuestion,
    /// 同一会话已有流在跑：两条流交错会把前端的答案搅成一团，故直接拒绝。
    /// 不带 session_id：提示是弹在提问那个标签页里的，带 id 只是噪音。
    #[error("该会话已有提问在进行中：请等它结束，或先点“取消”")]
    Busy,
    #[error("请求构造失败：{0}")]
    BadRequest(String),
    /// 文案已由 llm.rs 翻译成中文，这里只负责往上抛
    #[error("{0}")]
    Transport(String),
}

/// AI 助手配置。字段用 camelCase，因为要原样进 JSON 发给前端。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentConfig {
    /// 全局总开关。关着的时候旁路彻底不动 —— 连一次哈希查找都没有。
    pub enabled: bool,
    /// `openai` | `anthropic` | `custom`
    pub provider: String,
    pub base_url: String,
    pub model: String,
    /// 明文存 db —— 与现有 SSH 密码策略保持一致（方便跨机器迁移），
    /// 已在 store 里把 db 文件权限锁到 0600。
    pub api_key: String,
    /// 单次喂给模型的最近字节数（环形窗口内的二次截取）
    pub max_context_bytes: u32,
    pub system_prompt: String,
    /// 新会话是否默认开启旁路
    pub auto_attach: bool,
    /// 出网前是否做脱敏。**默认 true**：关掉它意味着把原始输出发给第三方
    pub redact: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o-mini".into(),
            api_key: String::new(),
            max_context_bytes: DEFAULT_MAX_CONTEXT_BYTES,
            system_prompt: DEFAULT_SYSTEM_PROMPT.into(),
            auto_attach: false,
            // 默认脱敏：宁可让模型少看到一点，也不能把私钥发出去
            redact: true,
        }
    }
}

/// 默认系统提示词。刻意保持简短 —— 真正的上下文来自会话缓冲本身。
const DEFAULT_SYSTEM_PROMPT: &str = concat!(
    "你是 NexusTerm 的终端助手。用户会把终端会话的最近输出发给你，",
    "其中 `$ ` 开头的是人敲进去的命令，其余是程序的输出。",
    "请结合会话上下文用中文简洁回答，只给结论与必要命令，不要复述已给出的输出。"
);

/// 读取 AI 配置。不存在 / JSON 坏了都静默回落到 `Default` ——
/// 配置文件损坏不该让整个应用起不来，这是 settings 表的通用原则。
pub fn load_config(store: &Store) -> AgentConfig {
    let Some(raw) = store.get_setting(CONFIG_KEY) else {
        return AgentConfig::default();
    };
    match serde_json::from_str::<AgentConfig>(&raw) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!("AI 配置解析失败，回落到默认值：{}", e);
            AgentConfig::default()
        }
    }
}

/// 保存 AI 配置。
pub fn save_config(store: &Store, cfg: &AgentConfig) -> Result<(), AgentError> {
    let json = serde_json::to_string(cfg).map_err(|e| AgentError::Serialize(e.to_string()))?;
    store
        .set_setting(CONFIG_KEY, &json)
        .map_err(AgentError::Store)
}

/// 全局 agent 状态。
///
/// **锁的选择**：本地 PTY 的读循环跑在 `std::thread::spawn` 里（见 `local/mod.rs`），
/// 那里的 `emit` 是同步代码，没法 `await`。所以这里必须是能同步获取的锁，
/// `parking_lot::Mutex`（无异步 await 点、比 std 更快、且不会被 poisoning 卡住），
/// 而不是 `tokio::sync::Mutex`。
pub struct AgentManager {
    /// 全局总开关。放在原子变量里是为了让 `feed` 在「关着」这条最频繁的
    /// 路径上完全不碰锁，把旁路对主链路的影响压到一次 relaxed load。
    enabled: AtomicBool,
    /// 未显式登记过的会话，默认是否旁路。
    auto_attach: AtomicBool,
    sessions: Mutex<HashMap<String, SessionBuffer>>,
}

impl AgentManager {
    pub fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            auto_attach: AtomicBool::new(false),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// 把配置里的开关同步进来。改配置后必须调一次，
    /// 否则 Cargo.toml 里的默认值和 db 里的值会打架。
    pub fn apply_config(&self, cfg: &AgentConfig) {
        self.enabled.store(cfg.enabled, Ordering::Relaxed);
        self.auto_attach.store(cfg.auto_attach, Ordering::Relaxed);
    }

    /// 旁路入口。pty / SSH 的读循环（`ssh/session.rs` 的 `read_loop`、
    /// `local/mod.rs` 的读线程）与写入命令（`cmd.rs`）在 hot path 上调用它。
    ///
    /// 永不失败、永不 panic：任何一个异常都只允许 `tracing::warn`，
    /// 因为这里炸掉的代价是整条终端会话断掉 —— 这是旁路绝不能有的破坏力。
    pub fn feed(&self, session_id: &str, data: &[u8], kind: StreamKind) {
        // 第一道闸门无锁：全局关着的时候，旁路对主链路的开销只有这一次 load
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        if data.is_empty() {
            return;
        }

        let mut guard = self.sessions.lock();
        if let Some(buf) = guard.get_mut(session_id) {
            if buf.is_enabled() {
                buf.feed(data, kind);
            }
            return;
        }
        // 会话还没登记过：按全局 autoAttach 决定要不要为它建缓冲。
        // 用 get_mut + 兜底 insert 而不是 entry()，是因为「关闭」是最常见的分支，
        // 而 entry() 会在 pad 时先分配一个空条目 —— 那正是我们要避免的分配。
        if !self.auto_attach.load(Ordering::Relaxed) {
            return;
        }
        guard.insert(session_id.to_string(), SessionBuffer::with_attach(true));
        if let Some(buf) = guard.get_mut(session_id) {
            buf.feed(data, kind);
        }
    }

    /// 显式登记一个会话的旁路开关（会建缓冲）。
    pub fn set_enabled(&self, session_id: &str, enabled: bool) {
        let mut guard = self.sessions.lock();
        guard
            .entry(session_id.to_string())
            .or_insert_with(|| SessionBuffer::with_capacity(DEFAULT_CAPACITY))
            .set_enabled(enabled);
    }

    /// 该会话是否在旁路。没登记过的按全局 `autoAttach` 处理 ——
    /// 这样接线方什么都不用做，只要开了 autoAttach，新会话自动就有上下文。
    pub fn is_enabled(&self, session_id: &str) -> bool {
        self.sessions
            .lock()
            .get(session_id)
            .map(|b| b.is_enabled())
            .unwrap_or_else(|| self.auto_attach.load(Ordering::Relaxed))
    }

    /// 取最近 `max_bytes` 的会话上下文。**注意：这里不做脱敏** ——
    /// UI 展示要看到原文，脱敏由调用 LLM 的那一层在出网前统一做。
    ///
    /// 返回 `None` 表示「没有上下文」（未启用 / 会话不存在）。
    pub fn context(&self, session_id: &str, max_bytes: usize) -> Option<String> {
        let guard = self.sessions.lock();
        let buf = guard.get(session_id)?;
        if !buf.is_enabled() {
            return None;
        }
        Some(buf.take_tail(max_bytes))
    }

    /// 完整会话记录。当前没有命令在调用它 —— 保留是因为「查看完整上下文」
    /// 这个 UI 入口（`AgentManager::context` 只给最近 N 字节）属于后续任务；
    /// 它已被本模块的 `missing_session_reports_none` 单测覆盖。
    #[allow(dead_code)]
    pub fn snapshot(&self, session_id: &str) -> Option<String> {
        let guard = self.sessions.lock();
        let buf = guard.get(session_id)?;
        Some(buf.snapshot())
    }

    /// 丢弃已缓冲的内容，但保留开关状态（用户「让 agent 忘掉刚才的事」）。
    pub fn clear(&self, session_id: &str) {
        if let Some(buf) = self.sessions.lock().get_mut(session_id) {
            buf.clear();
        }
    }

    /// 会话关闭时调用。**必须调用**，否则长生命周期的 AppState 会
    /// 一直攥着每个已关闭会话最多 `capacity` 字节的内存。
    /// 调用点：`cmd::ssh_close` 与 `cmd::local_close`。
    pub fn remove(&self, session_id: &str) {
        self.sessions.lock().remove(session_id);
    }

    pub fn stats(&self, session_id: &str) -> Option<BufferStats> {
        self.sessions.lock().get(session_id).map(|b| b.stats())
    }

    /// 当前登记了多少会话（诊断 / 排查内存泄漏用）。
    /// 生产代码暂无调用方：留给后续任务的诊断命令（对照打开的标签数查泄漏）。
    #[allow(dead_code)]
    pub fn tracked_sessions(&self) -> usize {
        self.sessions.lock().len()
    }
}

impl Default for AgentManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mgr(enabled: bool, auto_attach: bool) -> AgentManager {
        let m = AgentManager::new();
        m.apply_config(&AgentConfig {
            enabled,
            auto_attach,
            ..Default::default()
        });
        m
    }

    #[test]
    fn global_switch_off_is_a_no_op() {
        let m = mgr(false, true);
        m.feed("s1", b"hello", StreamKind::Out);
        assert_eq!(m.tracked_sessions(), 0, "关着时不该建任何缓冲");
        assert!(m.context("s1", 1024).is_none());
    }

    #[test]
    fn auto_attach_off_does_not_allocate() {
        let m = mgr(true, false);
        m.feed("s1", b"hello", StreamKind::Out);
        assert_eq!(m.tracked_sessions(), 0);
        assert!(!m.is_enabled("s1"));
    }

    #[test]
    fn auto_attach_on_creates_buffer_automatically() {
        let m = mgr(true, true);
        m.feed("s1", b"ls\n", StreamKind::In);
        assert_eq!(m.tracked_sessions(), 1);
        assert!(m.is_enabled("s1"));
        assert_eq!(m.context("s1", 1024).as_deref(), Some("$ ls\n"));
    }

    #[test]
    fn explicit_disable_stops_feeding_but_keeps_entry() {
        let m = mgr(true, false);
        m.set_enabled("s1", true);
        m.feed("s1", b"aa", StreamKind::Out);
        m.set_enabled("s1", false);
        m.feed("s1", b"bb", StreamKind::Out);
        assert!(!m.is_enabled("s1"));
        assert_eq!(m.context("s1", 1024), None);
        // 重新打开后，关着的这段时间里的数据确实没进来
        m.set_enabled("s1", true);
        assert_eq!(m.context("s1", 1024).as_deref(), Some("aa"));
    }

    #[test]
    fn sessions_are_isolated() {
        let m = mgr(true, true);
        m.feed("a", b"from-a", StreamKind::Out);
        m.feed("b", b"from-b", StreamKind::Out);
        assert_eq!(m.context("a", 1024).as_deref(), Some("from-a"));
        assert_eq!(m.context("b", 1024).as_deref(), Some("from-b"));
    }

    #[test]
    fn context_is_capped_by_max_bytes() {
        let m = mgr(true, true);
        let long: String = "x".repeat(5000);
        m.feed("a", long.as_bytes(), StreamKind::Out);
        let ctx = m.context("a", 100).expect("应有上下文");
        assert!(ctx.len() <= 100, "实际 {} 字节", ctx.len());
    }

    #[test]
    fn stats_report_dropped_and_truncated() {
        let m = mgr(true, true);
        m.set_enabled("a", true);
        // 默认容量 256KB，喂超过它
        let chunk = vec![b'z'; 64 * 1024];
        for _ in 0..8 {
            m.feed("a", &chunk, StreamKind::Out);
        }
        let s = m.stats("a").expect("应有统计");
        assert!(s.dropped > 0, "应当发生过丢弃");
        assert!(s.truncated);
        assert!(s.bytes <= 256 * 1024);
    }

    #[test]
    fn clear_empties_but_keeps_enabled() {
        let m = mgr(true, true);
        m.feed("a", b"secret-free text", StreamKind::Out);
        m.clear("a");
        assert_eq!(m.context("a", 1024).as_deref(), Some(""));
        assert!(m.is_enabled("a"));
    }

    #[test]
    fn remove_frees_the_entry() {
        let m = mgr(true, true);
        m.feed("a", b"data", StreamKind::Out);
        assert_eq!(m.tracked_sessions(), 1);
        m.remove("a");
        assert_eq!(m.tracked_sessions(), 0);
        assert!(m.stats("a").is_none());
    }

    /// 回显抑制走的是同一条 feed 路径：In 之后紧跟的 Out 开头应被抵消掉
    #[test]
    fn echo_is_suppressed_through_manager() {
        let m = mgr(true, true);
        m.feed("s1", b"pwd\n", StreamKind::In);
        m.feed("s1", b"pwd\r\n/home/chris\r\n", StreamKind::Out);
        assert_eq!(m.context("s1", 1024).as_deref(), Some("$ pwd\n/home/chris\n"));
    }

    #[test]
    fn missing_session_reports_none() {
        let m = mgr(true, true);
        assert!(m.context("nope", 1024).is_none());
        assert!(m.stats("nope").is_none());
        assert!(m.snapshot("nope").is_none());
    }

    #[test]
    fn empty_feed_does_not_create_entry() {
        let m = mgr(true, true);
        m.feed("a", b"", StreamKind::Out);
        assert_eq!(m.tracked_sessions(), 0);
    }

    #[test]
    fn config_defaults_match_spec() {
        let c = AgentConfig::default();
        assert!(!c.enabled);
        assert_eq!(c.provider, "openai");
        assert_eq!(c.base_url, "https://api.openai.com/v1");
        assert_eq!(c.model, "gpt-4o-mini");
        assert_eq!(c.max_context_bytes, 32768);
        assert!(c.redact, "脱敏默认必须开着");
        assert!(!c.auto_attach);
    }

    /// 序列化字段必须是 camelCase —— 前端按这个形状解析
    #[test]
    fn config_serializes_to_camel_case() {
        let json = serde_json::to_value(AgentConfig::default()).expect("序列化应成功");
        assert!(json.get("apiKey").is_some());
        assert!(json.get("baseUrl").is_some());
        assert!(json.get("maxContextBytes").is_some());
        assert!(json.get("systemPrompt").is_some());
        assert!(json.get("autoAttach").is_some());
        assert!(json.get("api_key").is_none());
    }

    /// 旧版本 / 残缺的配置必须能读回来而不是崩掉
    #[test]
    fn config_partial_json_falls_back_to_defaults() {
        let cfg: AgentConfig = serde_json::from_str(r#"{"model":"o3-mini"}"#)
            .expect("字段缺失应走 default 而不是报错");
        assert_eq!(cfg.model, "o3-mini");
        assert_eq!(cfg.base_url, AgentConfig::default().base_url);
        assert!(cfg.redact, "未指定的布尔字段应取默认值 true");
    }

    #[test]
    fn agent_helpers_are_reachable() {
        assert_eq!(redact::redact("AKIAIOSFODNN7EXAMPLE"), "[REDACTED]");
        assert_eq!(buffer::strip_ansi("\u{1b}[31mx\u{1b}[0m"), "x");
    }
}
