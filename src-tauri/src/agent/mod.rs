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

pub mod act;
pub mod buffer;
pub mod capture;
pub mod cmd;
pub mod danger;
pub mod executor;
pub mod llm;
pub mod redact;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

// 接线点（ssh/session.rs、local/mod.rs、cmd.rs）只需要这个来源标记
pub use self::buffer::StreamKind;
use self::buffer::{BufferStats, SessionBuffer, DEFAULT_CAPACITY};
use crate::store::Store;

/// 单次喂给模型的默认上下文字节数（远小于缓冲容量 `buffer::DEFAULT_CAPACITY`）。
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

/// 单个 AI 账号的连接与行为配置。
///
/// 行为字段（system_prompt/max_context_bytes/redact）也随账号走，
/// 这样切账号时提示词与脱敏策略一并切换，不会「换了模型却还带着上一家的提示词」。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentAccount {
    /// 账号唯一 id（前端生成的 uuid 或时间戳串）。切换、删除都按它定位。
    pub id: String,
    /// 展示名，例如「OpenAI 正式」「本地 vLLM」。空时 UI 退回显示模型名。
    pub name: String,
    /// `openai` | `anthropic` | `custom`
    pub provider: String,
    pub base_url: String,
    pub model: String,
    /// LLM 的 API Key。
    ///
    /// 这个结构体里它永远是**明文** —— 进出 db 时由 [`load_config`] /
    /// [`save_config`] 逐字段加解密。没启用主密码时随 db 明文保存（与 SSH 密码
    /// 同一套策略，为了跨机器迁移），db 文件权限锁在 0600。
    pub api_key: String,
    /// 单次喂给模型的最近字节数（环形窗口内的二次截取）
    pub max_context_bytes: u32,
    pub system_prompt: String,
    /// 出网前是否做脱敏。**默认 true**：关掉它意味着把原始输出发给第三方
    pub redact: bool,
}

impl Default for AgentAccount {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            provider: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o-mini".into(),
            api_key: String::new(),
            max_context_bytes: DEFAULT_MAX_CONTEXT_BYTES,
            system_prompt: DEFAULT_SYSTEM_PROMPT.into(),
            // 默认脱敏：宁可让模型少看到一点，也不能把私钥发出去
            redact: true,
        }
    }
}

/// Agent 的执行模式。
///
/// - `Agentic`：**全自动**。判定出来的动作直接下发到终端执行。
/// - `Approval`：**人在回路**。危险命令（见 `danger::assess`）先弹出来让人点头才执行。
///
/// 默认是 `Approval`：Terminal agent 的手是真会删文件的手，
/// 「默认先看一眼」是这里唯一负责的默认选项。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecMode {
    Agentic,
    Approval,
}

impl Default for ExecMode {
    fn default() -> Self {
        ExecMode::Approval
    }
}

impl ExecMode {
    /// 宽松解析：认得的名字要大小写不敏感（用户手填 JSON 时很容易写成 `AGENTIC`），
    /// 认不得的一律落到 `Approval` —— 解析从严、降级从安全侧。
    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "agentic" | "auto" | "auto-run" => ExecMode::Agentic,
            _ => ExecMode::Approval,
        }
    }

    pub fn is_approval(self) -> bool {
        matches!(self, ExecMode::Approval)
    }
}

/// 让未知的字符串落在默认模式上，而不是让整份配置反序列化失败。
///
/// 没有它，一个手抖写成的 `"execMode": "agent"` 会让 `load_config` 整份回落到默认，
/// 连 API Key 都丢了 —— 那比「这次按审批模式跑」严重得多。
fn deserialize_exec_mode<'de, D>(d: D) -> Result<ExecMode, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(d)?;
    Ok(ExecMode::parse(&raw))
}

/// AI 助手配置。字段用 camelCase，因为要原样进 JSON 发给前端。
///
/// 顶层只保留**全局**行为（总开关 enabled、新会话默认旁路 auto_attach），
/// 连接相关的一切下沉到 `accounts` 里。`active_id` 指向当前生效的账号。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentConfig {
    /// 全局总开关。关着的时候旁路彻底不动 —— 连一次哈希查找都没有。
    pub enabled: bool,
    /// 新会话是否默认开启旁路
    pub auto_attach: bool,
    /// 所有已配置的账号。至少有一个（反序列化后若为空会补一个默认账号）。
    pub accounts: Vec<AgentAccount>,
    /// 当前生效账号的 id。找不到时回落到第一个账号。
    pub active_id: String,
    /// Agent 的下令方式：`agentic`（全自动）/ `approval`（危险命令先问人）。
    #[serde(default, deserialize_with = "deserialize_exec_mode")]
    pub exec_mode: ExecMode,
}

impl Default for AgentConfig {
    fn default() -> Self {
        let acct = AgentAccount {
            id: "default".into(),
            name: "默认".into(),
            ..Default::default()
        };
        Self {
            enabled: false,
            auto_attach: false,
            active_id: acct.id.clone(),
            accounts: vec![acct],
            exec_mode: ExecMode::Approval,
        }
    }
}

impl AgentConfig {
    /// 取当前生效账号。accounts 保证非空（见 `load_config` 的规整），
    /// active_id 失效时回落到第一个。
    pub fn active_account(&self) -> &AgentAccount {
        self.accounts
            .iter()
            .find(|a| a.id == self.active_id)
            .or_else(|| self.accounts.first())
            .expect("accounts 恒非空：load_config 已保证")
    }

    /// 反序列化后的规整：补齐空账号列表、修正失效的 active_id、给没有 id 的账号补 id。
    /// 兼容旧版扁平配置（见 `load_config` 的迁移分支，那里已把旧字段塞进一个账号）。
    fn normalize(&mut self) {
        if self.accounts.is_empty() {
            self.accounts.push(AgentAccount {
                id: "default".into(),
                name: "默认".into(),
                ..Default::default()
            });
        }
        // 给没 id 的账号补一个稳定 id（极少见：手改过 db）
        for (i, a) in self.accounts.iter_mut().enumerate() {
            if a.id.trim().is_empty() {
                a.id = format!("acct-{i}");
            }
        }
        if !self.accounts.iter().any(|a| a.id == self.active_id) {
            self.active_id = self.accounts[0].id.clone();
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
    let mut cfg = match serde_json::from_str::<AgentConfig>(&raw) {
        Ok(mut cfg) if !cfg.accounts.is_empty() => {
            cfg.normalize();
            cfg
        }
        // 解析成功但 accounts 为空 → 多半是旧扁平配置被 serde(default) 填成了空列表，
        // 走迁移分支把旧字段捞出来。
        _ => migrate_legacy_config(&raw),
    };
    reveal_keys(store, &mut cfg);
    cfg
}

/// 把配置里每个账号的 api_key 解开。解不开的按「未配置」处理（清空）。
///
/// **逐字段加密，而不是把整个 value 加密**，这是个会直接造成数据丢失的区别：
/// 本函数所在的 `load_config` 的既定契约是「不存在 / JSON 坏了都静默回落
/// `Default`」。整体加密的话，锁定态下 AI 面板会显示成「全新未配置」，用户点一下
/// 保存就把模型、baseUrl、系统提示词全覆盖掉。逐字段还让 `lib.rs` 启动时的
/// `apply_config` 在锁定态依然读得到 `enabled` / `auto_attach` 这些非敏感开关，
/// 启动行为与改造前一致。
fn reveal_keys(store: &Store, cfg: &mut AgentConfig) {
    for a in &mut cfg.accounts {
        if a.api_key.is_empty() {
            continue;
        }
        match store.vault().reveal(crate::vault::Field::ApiKey, &a.api_key) {
            Ok(plain) => a.api_key = plain,
            Err(e) => {
                tracing::warn!(account = %a.id, error = %e, "LLM API Key 解不开，本次按未配置处理");
                a.api_key.clear();
            }
        }
    }
}

/// 把旧版扁平 AgentConfig（顶层直接是 provider/baseUrl/model/apiKey… 没有 accounts）
/// 迁移成单账号的新结构。解析不出来就回默认值。
fn migrate_legacy_config(raw: &str) -> AgentConfig {
    #[derive(Deserialize)]
    #[serde(default, rename_all = "camelCase")]
    struct Legacy {
        enabled: bool,
        auto_attach: bool,
        provider: String,
        base_url: String,
        model: String,
        api_key: String,
        max_context_bytes: u32,
        system_prompt: String,
        redact: bool,
    }
    impl Default for Legacy {
        fn default() -> Self {
            let a = AgentAccount::default();
            Self {
                enabled: false,
                auto_attach: false,
                provider: a.provider,
                base_url: a.base_url,
                model: a.model,
                api_key: a.api_key,
                max_context_bytes: a.max_context_bytes,
                system_prompt: a.system_prompt,
                redact: a.redact,
            }
        }
    }

    let Ok(old) = serde_json::from_str::<Legacy>(raw) else {
        tracing::warn!("AI 配置解析失败（新旧格式均不匹配），回落到默认值");
        return AgentConfig::default();
    };
    tracing::info!("检测到旧版 AI 配置，迁移为多账号结构");
    let acct = AgentAccount {
        id: "default".into(),
        name: "默认".into(),
        provider: old.provider,
        base_url: old.base_url,
        model: old.model,
        api_key: old.api_key,
        max_context_bytes: old.max_context_bytes,
        system_prompt: old.system_prompt,
        redact: old.redact,
    };
    AgentConfig {
        enabled: old.enabled,
        auto_attach: old.auto_attach,
        active_id: acct.id.clone(),
        accounts: vec![acct],
        // 旧配置没有这一项，按最保守的「人在回路」给
        exec_mode: ExecMode::Approval,
    }
}

/// 保存 AI 配置。保存前规整一遍（补账号、修 active_id），
/// 避免把一份非法状态写进库，下次读出来又得兜底。
pub fn save_config(store: &Store, cfg: &AgentConfig) -> Result<(), AgentError> {
    // 锁定态必须拒绝。此刻 `load_config` 给出的 api_key 是空的，界面上那一栏
    // 看起来就是「没配」—— 放行保存，用户按一下就把真实的 key 清掉了，
    // 而他完全不知道刚才发生了什么。
    if store.vault_locked() {
        return Err(AgentError::Store("保险库已锁定，请先输入主密码".into()));
    }

    let mut cfg = cfg.clone();
    cfg.normalize();
    for a in &mut cfg.accounts {
        if a.api_key.is_empty() {
            continue;
        }
        a.api_key = store
            .vault()
            .protect(crate::vault::Field::ApiKey, &a.api_key)
            .map_err(AgentError::Store)?;
    }
    let json = serde_json::to_string(&cfg).map_err(|e| AgentError::Serialize(e.to_string()))?;
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

    /// 把配置里的开关同步进内存。改配置后必须调一次，否则 db 里的值与内存里的原子开关会打架。
    pub fn apply_config(&self, cfg: &AgentConfig) {
        self.enabled.store(cfg.enabled, Ordering::Relaxed);
        // autoAttach 恒为 true：面板与设置页都已经没有「旁路开关」，
        // 助手默认就要能看到当前会话。db 里可能还躺着旧版本存下的 false，
        // 在这里兜住它，避免升级后新建会话「明明启用了却没有上下文」。
        self.auto_attach.store(true, Ordering::Relaxed);
    }

    /// 旁路入口。pty / SSH 的读循环（`ssh/session.rs` 的 `read_loop`、
    /// `local/mod.rs` 的读线程）与写入命令（`cmd.rs`）在 hot path 上调用它。
    ///
    /// 永不失败、永不 panic：任何一个异常都只允许 `tracing::warn`，
    /// 因为这里炸掉的代价是整条终端会话断掉 —— 这是旁路绝不能有的破坏力。
    pub fn feed(&self, session_id: &str, data: &[u8], kind: StreamKind) {
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
        // 用 get_mut + 兜底 insert 而不是 entry()：entry() 需要先把 session_id 拷成
        // 一个 owned String，而「已登记」才是热路径，不该为它付这份分配。
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

    /// 完整会话记录（`AgentManager::context` 只给最近 N 字节用于喂模型）。
    /// 供 `agent::capture::export_session`（导出）与 `act::run_one`（对比执行前后差量）调用。
    pub fn snapshot(&self, session_id: &str) -> Option<String> {
        let guard = self.sessions.lock();
        let buf = guard.get(session_id)?;
        Some(buf.snapshot())
    }

    /// 这个会话有史以来写进缓冲的原始字节总数（只增不减）。
    ///
    /// 很便宜（不解码、不剥 ANSI），供 Agent 每隔几十毫秒轮询「有没有新输出」。
    pub fn pushed(&self, session_id: &str) -> Option<u64> {
        self.sessions.lock().get(session_id).map(|b| b.pushed())
    }

    /// 从 `mark`（之前某次 [`Self::pushed`] 的返回值）往后的内容，格式同 [`Self::snapshot`]。
    pub fn since(&self, session_id: &str, mark: u64) -> Option<String> {
        self.sessions.lock().get(session_id).map(|b| b.since(mark))
    }

    /// 缓冲尾部最后一个非空行——shell 停在提示符时就是提示符本身。
    pub fn last_line(&self, session_id: &str) -> Option<String> {
        self.sessions.lock().get(session_id).map(|b| b.last_line())
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

    /// 当前登记了多少会话（诊断用：对照打开的标签数查泄漏）。生产代码暂无调用方。
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
    fn auto_attach_ignores_stale_config() {
        // 面板与设置页都已经没有「旁路开关」，助手面向全局：会话在哪，哪就是上下文。
        // db 里可能还躺着旧版本存下的 autoAttach:false —— 那种配置不能再让
        // 新会话「明明启用了，却一条上下文都没有」。
        let m = mgr(true, false);
        m.feed("s1", b"hello", StreamKind::Out);
        assert_eq!(m.tracked_sessions(), 1);
        assert!(m.is_enabled("s1"));
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
        assert!(!c.auto_attach);
        assert!(c.exec_mode.is_approval(), "默认必须是「人在回路」");

        assert_eq!(c.accounts.len(), 1, "默认必须恰有一个账号");
        let a = c.active_account();
        assert_eq!(a.provider, "openai");
        assert_eq!(a.base_url, "https://api.openai.com/v1");
        assert_eq!(a.model, "gpt-4o-mini");
        assert_eq!(a.max_context_bytes, 32768);
        assert!(a.redact, "脱敏默认必须开着");
    }

    #[test]
    fn exec_mode_round_trips_as_lowercase_string() {
        let cfg = AgentConfig {
            exec_mode: ExecMode::Agentic,
            ..Default::default()
        };
        let json = serde_json::to_value(&cfg).expect("应能序列化");
        assert_eq!(json["execMode"], "agentic");
        let back: AgentConfig = serde_json::from_value(json).expect("应能反序列化");
        assert_eq!(back.exec_mode, ExecMode::Agentic);
    }

    /// 认不得的值必须落在「审批」上，而且**不能连累整份配置**
    #[test]
    fn unknown_exec_mode_degrades_to_approval() {
        let raw = r#"{"enabled":true,"execMode":"agent","accounts":[{"id":"x","model":"m","apiKey":"k"}]}"#;
        let cfg: AgentConfig = serde_json::from_str(raw).expect("不应因一个字段坏了整份配置");
        assert!(cfg.exec_mode.is_approval(), "未知值必须降级到最安全的模式");
        assert!(cfg.enabled, "其它字段必须照常生效");
        assert_eq!(cfg.active_account().api_key, "k");
        // 大小写与别名都认
        assert_eq!(ExecMode::parse(" AGENTIC "), ExecMode::Agentic);
        assert_eq!(ExecMode::parse("auto"), ExecMode::Agentic);
        assert_eq!(ExecMode::parse("nonsense"), ExecMode::Approval);
    }

    /// 旧版扁平配置迁移后同样必须是最保守的模式
    #[test]
    fn migrated_config_starts_in_approval_mode() {
        let cfg = migrate_legacy_config(r#"{"enabled":true,"model":"m","apiKey":"k"}"#);
        assert!(cfg.exec_mode.is_approval());
    }

    /// 序列化字段必须是 camelCase —— 前端按这个形状解析
    #[test]
    fn config_serializes_to_camel_case() {
        let json = serde_json::to_value(AgentConfig::default()).expect("序列化应成功");
        assert!(json.get("accounts").is_some());
        assert!(json.get("activeId").is_some());
        assert!(json.get("autoAttach").is_some());
        let acct = &json["accounts"][0];
        assert!(acct.get("apiKey").is_some());
        assert!(acct.get("baseUrl").is_some());
        assert!(acct.get("maxContextBytes").is_some());
        assert!(acct.get("systemPrompt").is_some());
        assert!(acct.get("api_key").is_none());
    }

    /// 新格式部分字段缺失应走 default 而不是崩
    #[test]
    fn config_partial_json_falls_back_to_defaults() {
        let cfg: AgentConfig =
            serde_json::from_str(r#"{"accounts":[{"id":"x","model":"o3-mini"}],"activeId":"x"}"#)
                .expect("字段缺失应走 default 而不是报错");
        let a = cfg.active_account();
        assert_eq!(a.model, "o3-mini");
        assert_eq!(a.base_url, AgentAccount::default().base_url);
        assert!(a.redact, "未指定的布尔字段应取默认值 true");
    }

    /// 旧版扁平配置必须能迁移成单账号而不是丢数据
    #[test]
    fn legacy_flat_config_migrates_to_single_account() {
        let raw = r#"{"enabled":true,"provider":"anthropic","baseUrl":"https://x/v1","model":"claude-3","apiKey":"sk-legacy","maxContextBytes":65536,"systemPrompt":"hi","autoAttach":true,"redact":false}"#;
        let cfg = migrate_legacy_config(raw);
        assert!(cfg.enabled);
        assert!(cfg.auto_attach);
        assert_eq!(cfg.accounts.len(), 1);
        let a = cfg.active_account();
        assert_eq!(a.provider, "anthropic");
        assert_eq!(a.base_url, "https://x/v1");
        assert_eq!(a.model, "claude-3");
        assert_eq!(a.api_key, "sk-legacy");
        assert_eq!(a.max_context_bytes, 65536);
        assert_eq!(a.system_prompt, "hi");
        assert!(!a.redact);
    }

    /// active_id 指向不存在的账号时，active_account 回落到第一个
    #[test]
    fn active_id_falls_back_to_first() {
        let mut cfg = AgentConfig {
            accounts: vec![
                AgentAccount { id: "a".into(), model: "m-a".into(), ..Default::default() },
                AgentAccount { id: "b".into(), model: "m-b".into(), ..Default::default() },
            ],
            active_id: "ghost".into(),
            ..Default::default()
        };
        cfg.normalize();
        assert_eq!(cfg.active_account().model, "m-a");
    }

    #[test]
    fn agent_helpers_are_reachable() {
        assert_eq!(redact::redact("AKIAIOSFODNN7EXAMPLE"), "[REDACTED]");
        assert_eq!(buffer::strip_ansi("\u{1b}[31mx\u{1b}[0m"), "x");
    }
}
