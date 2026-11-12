//! 托管任务的可落盘现场（设计 §4）：整条 JSON 存 `host_sessions.json`。
//!
//! - 带版本号 `v`。读到比本程序新的版本**不猜**：返回 `FutureVersion`，调用方保留原行、不覆盖；
//! - `messages` 落盘前先 `compact::compact`（≤60k 字符）再 `history::sanitize_messages`（脱敏 + 截断），
//!   密文永远不进 `messages`；
//! - 恢复判定复用 `checkpoint::resume_verdict`：屏幕漂移就把档位降到 approve。

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::super::checkpoint::{resume_verdict, ResumeVerdict, Spent, StewardCheckpoint, CHECKPOINT_TTL_MS};
use super::super::compact::compact;
use super::super::history::sanitize_messages;
use super::super::llm::ChatMessage;
use super::super::policy::Authority;
use super::super::steward::StewardContract;
use super::phase::{step, Effect, HostEvent, HostPhase};
use super::watchdog::WatchdogState;
use super::ConsoleRef;

/// 当前写出的版本。
pub const HOST_CHECKPOINT_V: u32 = 1;
/// 落盘前对话历史的字符上限
pub const MESSAGES_BUDGET_CHARS: usize = 60_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostCheckpoint {
    pub v: u32,
    pub contract: StewardContract,
    pub authority: Authority,
    pub phase: HostPhase,
    pub console: ConsoleRef,
    pub messages: Vec<ChatMessage>,
    pub spent: Spent,
    pub step: usize,
    #[serde(default)]
    pub woke_hash: HashMap<String, String>,
    #[serde(default)]
    pub last_screen_hash: String,
    #[serde(default)]
    pub watchdog: WatchdogState,
    /// 计划清单（nx_todo）；旧现场没有该字段 → 空
    #[serde(default)]
    pub todo: Vec<super::super::todo::TodoItem>,
    pub created_wall_ms: i64,
    pub saved_wall_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// 比本程序新的版本写的：不解析、不覆盖
    FutureVersion(u32),
    /// 不是 JSON / 缺字段 / 没有版本号
    Corrupt(String),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FutureVersion(v) => write!(f, "托管现场由更新版本（v{v}）写入，当前版本只认到 v{HOST_CHECKPOINT_V}"),
            Self::Corrupt(e) => write!(f, "托管现场损坏：{e}"),
        }
    }
}

impl std::error::Error for LoadError {}

impl HostCheckpoint {
    pub fn new(
        contract: StewardContract,
        authority: Authority,
        console: ConsoleRef,
        now_wall_ms: i64,
    ) -> Self {
        Self {
            v: HOST_CHECKPOINT_V,
            contract,
            authority,
            phase: HostPhase::Running,
            console,
            messages: Vec::new(),
            spent: Spent::default(),
            step: 0,
            woke_hash: HashMap::new(),
            last_screen_hash: String::new(),
            watchdog: WatchdogState::default(),
            todo: Vec::new(),
            created_wall_ms: now_wall_ms,
            saved_wall_ms: now_wall_ms,
        }
    }

    /// `host_sessions.state` 列。
    pub fn state(&self) -> &'static str {
        self.phase.label()
    }

    /// 落盘前清洗对话历史：先压缩，再脱敏 / 截断。
    pub fn prepare_messages(mut msgs: Vec<ChatMessage>) -> Vec<ChatMessage> {
        compact(&mut msgs, MESSAGES_BUDGET_CHARS);
        let vals: Vec<Value> = msgs.iter().filter_map(|m| serde_json::to_value(m).ok()).collect();
        sanitize_messages(vals).into_iter().filter_map(|v| serde_json::from_value(v).ok()).collect()
    }

    /// 序列化。`messages` 一律先过 `prepare_messages`，调用方不必记得。
    pub fn to_json(&self, now_wall_ms: i64) -> Result<String, String> {
        let mut c = self.clone();
        c.v = HOST_CHECKPOINT_V;
        c.saved_wall_ms = now_wall_ms;
        c.messages = Self::prepare_messages(c.messages);
        serde_json::to_string(&c).map_err(|e| e.to_string())
    }

    pub fn from_json(s: &str) -> Result<Self, LoadError> {
        let raw: Value = serde_json::from_str(s).map_err(|e| LoadError::Corrupt(e.to_string()))?;
        let v = raw
            .get("v")
            .and_then(Value::as_u64)
            .ok_or_else(|| LoadError::Corrupt("缺少版本号 v".into()))?;
        let v = u32::try_from(v).map_err(|_| LoadError::FutureVersion(u32::MAX))?;
        if v > HOST_CHECKPOINT_V {
            return Err(LoadError::FutureVersion(v));
        }
        if v == 0 {
            return Err(LoadError::Corrupt("版本号 v=0".into()));
        }
        // v1 是第一版，没有更旧的需要迁移
        serde_json::from_value(raw).map_err(|e| LoadError::Corrupt(e.to_string()))
    }

    /// 超过 TTL（从创建算，周期快照不续命）。
    pub fn expired(&self, now_wall_ms: i64) -> bool {
        now_wall_ms - self.created_wall_ms > CHECKPOINT_TTL_MS
    }

    /// 启动扫描：非终态一律挂起（AppRestart），此时不写终端。返回是否改动。
    pub fn suspend_for_restart(&mut self) -> bool {
        match step(&self.phase, HostEvent::Restart) {
            Ok((p, _)) if p != self.phase => {
                self.phase = p;
                true
            }
            _ => false,
        }
    }

    /// 接驳前核对现场。复用 `checkpoint::resume_verdict` 的口径（TTL / 没帧 / hash 漂移）。
    pub fn reattach_verdict(&self, current_hash: Option<&str>, now_wall_ms: i64) -> ResumeVerdict {
        let mut cp = StewardCheckpoint::new(&self.contract.id, &self.console.console_id, &self.contract.goal, &self.authority);
        cp.screen_hash = self.last_screen_hash.clone();
        cp.created_wall_ms = self.created_wall_ms;
        cp.saved_wall_ms = self.saved_wall_ms;
        cp.host_key = self.console.host_key.clone();
        cp.state = if self.phase.is_terminal() { "completed" } else { "observing" }.to_string();
        resume_verdict(&cp, current_hash, now_wall_ms)
    }

    /// 用户确认接驳：按判定降档，再把相位迁回挂起前的状态。返回迁移副作用。
    /// Expired / Suspended（没帧）不接驳，返回 Err 说明。
    pub fn reattach(
        &mut self,
        console: ConsoleRef,
        verdict: &ResumeVerdict,
    ) -> Result<Vec<Effect>, String> {
        match verdict {
            ResumeVerdict::Expired { reason } | ResumeVerdict::Suspended { reason } => return Err(reason.clone()),
            ResumeVerdict::Drift { downgrade_to } => self.authority.tier = downgrade_to.clone(),
            ResumeVerdict::Clean => {}
        }
        let (p, fx) = step(&self.phase, HostEvent::Reattach(console.clone())).map_err(|e| e.to_string())?;
        self.phase = p;
        self.console = console;
        Ok(fx)
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::checkpoint::CHECKPOINT_TTL_MS;
    use super::super::super::policy::TIER_APPROVE;
    use super::super::{Check, PauseCause, SuspendReason};
    use super::*;

    fn contract() -> StewardContract {
        StewardContract {
            id: "c1".into(),
            console_id: "t1".into(),
            host_key: "ssh:root@a:22".into(),
            goal: "升级 nginx".into(),
            authority_tier: "auto".into(),
            status: "observing".into(),
            intent_ids: vec!["i1".into()],
            timebox_ms: 0,
            created_at: 1,
            success_criteria: vec![Check::ScreenMatches { re: "nginx/1\\.2".into() }],
            yolo: true,
            criteria_confirmed: false,
        }
    }
    fn console(id: &str) -> ConsoleRef {
        ConsoleRef {
            host_key: "ssh:root@a:22".into(),
            session_record_id: Some("rec".into()),
            label: "a".into(),
            cwd: Some("/etc/nginx".into()),
            tmux: Some("main".into()),
            console_id: id.into(),
        }
    }
    fn cp() -> HostCheckpoint {
        let mut c = HostCheckpoint::new(contract(), Authority::default().with_tier("auto"), console("t1"), 1_000);
        c.phase = HostPhase::Paused { cause: PauseCause::Budget { reason: "步数".into() } };
        c.messages = vec![ChatMessage::user("开始"), ChatMessage::assistant("好的")];
        c.spent.steps = 12;
        c.step = 12;
        c.woke_hash.insert("t1".into(), "abc".into());
        c.last_screen_hash = "h1".into();
        c
    }

    #[test]
    fn round_trip() {
        let c = cp();
        let j = c.to_json(2_000).unwrap();
        let back = HostCheckpoint::from_json(&j).unwrap();
        let mut want = c.clone();
        want.saved_wall_ms = 2_000;
        assert_eq!(serde_json::to_value(&back).unwrap(), serde_json::to_value(&want).unwrap());
        assert_eq!(back.contract.success_criteria, contract().success_criteria);
        assert!(back.contract.yolo);
        assert_eq!(back.state(), "paused");
    }

    #[test]
    fn future_version_is_refused_not_guessed() {
        let mut v: Value = serde_json::from_str(&cp().to_json(0).unwrap()).unwrap();
        v["v"] = Value::from(HOST_CHECKPOINT_V + 1);
        v["somethingNew"] = Value::from(true);
        assert_eq!(HostCheckpoint::from_json(&v.to_string()).unwrap_err(), LoadError::FutureVersion(HOST_CHECKPOINT_V + 1));
        v["v"] = Value::from(u64::MAX);
        assert!(matches!(HostCheckpoint::from_json(&v.to_string()), Err(LoadError::FutureVersion(_))));
    }

    #[test]
    fn missing_or_bad_version_and_garbage_are_corrupt() {
        let mut v: Value = serde_json::from_str(&cp().to_json(0).unwrap()).unwrap();
        v.as_object_mut().unwrap().remove("v");
        assert!(matches!(HostCheckpoint::from_json(&v.to_string()), Err(LoadError::Corrupt(_))));
        v["v"] = Value::from(0);
        assert!(matches!(HostCheckpoint::from_json(&v.to_string()), Err(LoadError::Corrupt(_))));
        assert!(matches!(HostCheckpoint::from_json("not json"), Err(LoadError::Corrupt(_))));
        assert!(matches!(HostCheckpoint::from_json(r#"{"v":1}"#), Err(LoadError::Corrupt(_))));
    }

    #[test]
    fn old_contract_json_without_new_fields_still_loads() {
        let mut v: Value = serde_json::from_str(&cp().to_json(0).unwrap()).unwrap();
        let c = v["contract"].as_object_mut().unwrap();
        c.remove("successCriteria");
        c.remove("yolo");
        v.as_object_mut().unwrap().remove("watchdog");
        v.as_object_mut().unwrap().remove("wokeHash");
        let back = HostCheckpoint::from_json(&v.to_string()).unwrap();
        assert!(back.contract.success_criteria.is_empty());
        assert!(!back.contract.yolo);
        assert_eq!(back.watchdog, WatchdogState::default());
    }

    #[test]
    fn messages_are_sanitized_and_compacted_on_save() {
        let mut c = cp();
        c.messages = vec![ChatMessage::user("目标")];
        for n in 0..200 {
            c.messages.push(ChatMessage::assistant(format!("第 {n} 步 {}", "x".repeat(800))));
            c.messages.push(ChatMessage::user(format!("反馈 {n} {}", "y".repeat(800))));
        }
        c.messages.push(ChatMessage::user("password=hunter2secret"));
        let back = HostCheckpoint::from_json(&c.to_json(0).unwrap()).unwrap();
        let total: usize = back.messages.iter().map(|m| m.content.chars().count()).sum();
        assert!(total <= MESSAGES_BUDGET_CHARS + 2_000, "{total}");
        assert!(back.messages.len() < c.messages.len());
        assert!(!back.messages.iter().any(|m| m.content.contains("hunter2secret")));
    }

    #[test]
    fn restart_suspends_non_terminal_only() {
        let mut c = cp();
        assert!(c.suspend_for_restart());
        assert_eq!(c.phase, HostPhase::Suspended {
            reason: SuspendReason::AppRestart,
            resume_to: Box::new(HostPhase::Paused { cause: PauseCause::Budget { reason: "步数".into() } }),
        });
        assert!(!c.suspend_for_restart(), "已挂起再扫一次不变");
        c.phase = HostPhase::Stopped;
        assert!(!c.suspend_for_restart());
    }

    #[test]
    fn reattach_clean_restores_saved_phase_and_tier() {
        let mut c = cp();
        c.suspend_for_restart();
        let v = c.reattach_verdict(Some("h1"), 5_000);
        assert_eq!(v, ResumeVerdict::Clean);
        let fx = c.reattach(console("t9"), &v).unwrap();
        assert!(matches!(c.phase, HostPhase::Paused { .. }), "预算暂停经重启仍是暂停");
        assert_eq!(c.authority.tier, "auto");
        assert_eq!(c.console.console_id, "t9");
        assert!(fx.contains(&Effect::ForceScreen));
    }

    #[test]
    fn reattach_drift_downgrades_to_approve() {
        let mut c = cp();
        c.phase = HostPhase::Running;
        c.suspend_for_restart();
        let v = c.reattach_verdict(Some("other"), 5_000);
        assert!(matches!(v, ResumeVerdict::Drift { .. }));
        c.reattach(console("t9"), &v).unwrap();
        assert_eq!(c.phase, HostPhase::Running);
        assert_eq!(c.authority.tier, TIER_APPROVE);
    }

    #[test]
    fn reattach_refused_when_expired_or_no_frame() {
        let mut c = cp();
        c.suspend_for_restart();
        let late = c.created_wall_ms + CHECKPOINT_TTL_MS + 1;
        assert!(c.expired(late));
        assert!(!c.expired(c.created_wall_ms + 10));
        let v = c.reattach_verdict(Some("h1"), late);
        assert!(matches!(v, ResumeVerdict::Expired { .. }));
        assert!(c.reattach(console("t9"), &v).is_err());
        let v = c.reattach_verdict(None, 5_000);
        assert!(matches!(v, ResumeVerdict::Suspended { .. }));
        assert!(c.reattach(console("t9"), &v).is_err());
        assert!(matches!(c.phase, HostPhase::Suspended { .. }), "拒绝接驳时状态不变");
    }
}
