//! 统一托管引擎（设计见 `docs/nexus-host-unified-design.md`）。
//!
//! 纯逻辑：生命周期状态机（`phase`）、授权闸门（`gate`）、看门狗（`watchdog`）、
//! 契约上下文渲染（`context`）、可落盘现场（`checkpoint`）；控制流（`driver`：收件箱 + 挂起 + 副作用）
//! 也不碰 Tauri，副作用经 `HostIo` 落地。接线在 `act::run(RunMode::Hosted)` 与 `host_cmd.rs`。
#![allow(dead_code)]

pub mod checkpoint;
pub mod context;
pub mod criteria;
pub mod driver;
pub mod record;
pub mod reflex;
pub mod verify;
pub mod gate;
pub mod phase;
pub mod watchdog;

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

pub use driver::{HostCtx, HostIo, HostShared, ParkEnd};
pub use phase::{HostEvent, HostPhase};

/// 托管运行的 id 前缀：`host:{contract_id}`，避免和同一 tab 上的对话运行在 `begin()` 处冲突。
pub const RUN_ID_PREFIX: &str = "host:";

/// 托管任务的身份：交给 `act::run(RunMode::Hosted(HostHandle))`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostHandle {
    pub contract_id: String,
    pub run_id: String,
}

impl HostHandle {
    pub fn new(contract_id: &str) -> Self {
        Self { contract_id: contract_id.to_string(), run_id: run_id_for(contract_id) }
    }
}

pub fn run_id_for(contract_id: &str) -> String {
    format!("{RUN_ID_PREFIX}{contract_id}")
}

/// 被托管的那个 console 的稳定描述。`console_id` 是前端的易变 id，重启后**不作键**，
/// 接驳时按 host_key → session_record_id → label → cwd 给候选 tab 排序。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleRef {
    pub host_key: String,
    #[serde(default)]
    pub session_record_id: Option<String>,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub tmux: Option<String>,
    pub console_id: String,
}

impl ConsoleRef {
    /// 候选 tab 与保存的现场有多像：按位打分，高位优先（host_key 8 > 记录 id 4 > 标签 2 > cwd 1）。
    /// 0 = 毫无关系。排序时分数高的在前。
    pub fn match_score(&self, cand: &ConsoleRef) -> u8 {
        let same = |a: &str, b: &str| !a.is_empty() && a == b;
        let same_opt = |a: &Option<String>, b: &Option<String>| match (a, b) {
            (Some(x), Some(y)) => same(x, y),
            _ => false,
        };
        let mut s = 0;
        if same(&self.host_key, &cand.host_key) {
            s |= 8;
        }
        if same_opt(&self.session_record_id, &cand.session_record_id) {
            s |= 4;
        }
        if same(self.label.trim(), cand.label.trim()) {
            s |= 2;
        }
        if same_opt(&self.cwd, &cand.cwd) {
            s |= 1;
        }
        s
    }

    /// 把候选按相似度降序排好，丢掉毫无关系的。分数相同保持原顺序。
    pub fn rank<'a>(&self, cands: &'a [ConsoleRef]) -> Vec<&'a ConsoleRef> {
        let mut v: Vec<(u8, &ConsoleRef)> =
            cands.iter().map(|c| (self.match_score(c), c)).filter(|(s, _)| *s > 0).collect();
        v.sort_by(|a, b| b.0.cmp(&a.0));
        v.into_iter().map(|(_, c)| c).collect()
    }
}

/// 卡片上的一个选项：按钮文字 + 点了之后作为用户答复送回模型的话。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CardOption {
    pub label: String,
    pub reply: String,
}

/// 卡片的来源。决定答复回到循环时走哪条路。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum CardKind {
    /// `nx_ask_user`，或 Hosted 模式下没有动作的回复
    Ask,
    /// Approve 档 / 危险动作的写入审批
    Approve,
    /// 看门狗 L2：附最近几条决策记录
    Watchdog,
    /// 只剩人工确认项的完成判定：确认即 Completed
    ManualCheck { summary: String, checks: Vec<CheckResult> },
    /// `nx_secret`：密码走面板的密码卡片直达 pty，**不**经 `host_answer`；卡片只用来挂起相位
    Secret,
    /// 开始托管时模型起草的完成判据，等用户编辑 / 接受（`host_confirm_criteria`）
    CriteriaDraft { draft: Vec<Check> },
}

/// 一张等人处理的卡片。**永远等下去**：没有倒计时，也没有默认动作。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HumanCard {
    pub id: String,
    pub kind: CardKind,
    pub question: String,
    #[serde(default)]
    pub options: Vec<CardOption>,
    #[serde(default)]
    pub recommendation: Option<String>,
    /// 审批原因 / 看门狗附带的最近决策摘要
    #[serde(default)]
    pub reasons: Vec<String>,
    pub created_wall_ms: i64,
}

impl HumanCard {
    pub fn ask(id: &str, question: &str, now_wall_ms: i64) -> Self {
        Self {
            id: id.to_string(),
            kind: CardKind::Ask,
            question: question.to_string(),
            options: Vec::new(),
            recommendation: None,
            reasons: Vec::new(),
            created_wall_ms: now_wall_ms,
        }
    }
}

/// 用户对卡片的答复。`approved` 只对审批 / 人工确认卡有意义。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CardReply {
    pub card_id: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub approved: Option<bool>,
}

/// 完成判据（`StewardContract.success_criteria`）。由模型起草、用户确认（YOLO 模式跳过确认）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Check {
    /// 当前帧匹配正则
    ScreenMatches { re: String },
    /// 只读命令退出码为 0（要过 `gate::authorize` 且 `danger::assess` 判为不危险）
    CommandOk { cmd: String },
    /// `test -e <path>`
    FileExists { path: String },
    /// 只能人确认
    Manual { text: String },
}

impl Check {
    pub fn describe(&self) -> String {
        match self {
            Self::ScreenMatches { re } => format!("屏幕匹配 /{re}/"),
            Self::CommandOk { cmd } => format!("命令成功：`{cmd}`"),
            Self::FileExists { path } => format!("文件存在：{path}"),
            Self::Manual { text } => format!("人工确认：{text}"),
        }
    }

    pub fn is_manual(&self) -> bool {
        matches!(self, Self::Manual { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckResult {
    pub check: Check,
    pub passed: bool,
    #[serde(default)]
    pub detail: String,
}

/// 为什么挂起：现场不在了，任务没法继续，但不算结束。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SuspendReason {
    AppRestart,
    ConsoleGone,
    LlmDown,
}

/// 为什么暂停。不复用 `budget::PauseCause`：那个没有 Deadline，也不能反序列化（要进 checkpoint）。
/// 预算原因存成给人看的一句话（`ExhaustReason::describe()`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PauseCause {
    Budget { reason: String },
    Deadline,
    User,
    /// 模型服务重试用尽（限流 / 5xx / 超时）。和预算暂停一样只能由用户「继续」离开
    LlmDown { error: String },
}

/// 用户「加预算继续」时给的新上限；`None` 字段 = 不变。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetGrant {
    #[serde(default)]
    pub max_steps: Option<u32>,
    #[serde(default)]
    pub max_minutes: Option<u32>,
    #[serde(default)]
    pub max_output_chars: Option<u64>,
}

/// 结束的托管记录留多久给面板看结局。
pub const ENDED_KEEP: std::time::Duration = std::time::Duration::from_secs(3600);
/// 结束的托管记录最多留几条（新的优先）。
pub const ENDED_MAX: usize = 20;

/// 清理结局：结束超过 `ENDED_KEEP` 的丢掉；剩下的只留最近 `ENDED_MAX` 条。未结束的一律不动。
fn prune_ended(map: &mut HashMap<String, Arc<HostShared>>) {
    map.retain(|_, h| h.ended_for().map_or(true, |d| d < ENDED_KEEP));
    let mut ended: Vec<(String, std::time::Duration)> =
        map.iter().filter_map(|(k, h)| h.ended_for().map(|d| (k.clone(), d))).collect();
    if ended.len() <= ENDED_MAX {
        return;
    }
    ended.sort_by_key(|(_, d)| *d); // 最近结束的在前
    for (k, _) in ended.into_iter().skip(ENDED_MAX) {
        map.remove(&k);
    }
}

/// 托管任务登记表（挂在 `ActRuntime.host` 上）：契约 id → 运行中的托管任务。
///
/// 命令层经它找到任务投事件；`gate_drive` / `request_approval` / `user_control` 经它判断
/// 某个运行 id（`host:{contract_id}`）或 console 是否在托管下。终态的任务留在表里给 UI 看结局，
/// 同一 console 再开托管时清掉。
#[derive(Default)]
pub struct HostSupervisor {
    runs: Mutex<HashMap<String, Arc<HostShared>>>,
}

impl HostSupervisor {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一个任务（新开、启动扫描恢复、接驳重建都走这里；同一契约 id 直接顶替）。
    /// 同一 console 上已结束的旧任务一并清掉，再按 `prune` 清理过旧的结局。
    pub fn register(&self, shared: Arc<HostShared>) {
        let mut map = self.runs.lock();
        map.retain(|_, h| !(h.console_id == shared.console_id && h.is_terminal()));
        map.insert(shared.contract_id().to_string(), shared);
        prune_ended(&mut map);
    }

    pub fn get(&self, contract_id: &str) -> Option<Arc<HostShared>> {
        self.runs.lock().get(contract_id).cloned()
    }

    pub fn phase(&self, contract_id: &str) -> Option<HostPhase> {
        self.get(contract_id).map(|h| h.phase())
    }

    pub fn remove(&self, contract_id: &str) -> Option<Arc<HostShared>> {
        self.runs.lock().remove(contract_id)
    }

    pub fn all(&self) -> Vec<Arc<HostShared>> {
        let mut map = self.runs.lock();
        prune_ended(&mut map);
        let mut v: Vec<_> = map.values().cloned().collect();
        drop(map);
        v.sort_by(|a, b| a.contract_id().cmp(b.contract_id()));
        v
    }

    /// 按运行 id（`host:…`）或 console id 找**未结束**的托管任务。
    pub fn live(&self, id: &str) -> Option<Arc<HostShared>> {
        self.runs
            .lock()
            .values()
            .find(|h| !h.is_terminal() && (h.run_id() == id || h.console_id == id))
            .cloned()
    }

    /// 这个 console 上有没有未结束的托管。
    pub fn live_on_console(&self, console_id: &str) -> Option<Arc<HostShared>> {
        self.runs
            .lock()
            .values()
            .find(|h| !h.is_terminal() && h.console_id == console_id)
            .cloned()
    }

    /// 立即迁移一个契约的相位（命令层用，比如「停止」要马上生效）；非法事件不改状态。
    pub fn apply(&self, contract_id: &str, ev: HostEvent) -> Result<HostPhase, String> {
        let h = self.get(contract_id).ok_or_else(|| format!("没有托管契约 {contract_id}"))?;
        h.apply(ev).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cref(host: &str, rec: Option<&str>, label: &str, cwd: Option<&str>, id: &str) -> ConsoleRef {
        ConsoleRef {
            host_key: host.into(),
            session_record_id: rec.map(Into::into),
            label: label.into(),
            cwd: cwd.map(Into::into),
            tmux: None,
            console_id: id.into(),
        }
    }

    #[test]
    fn run_id_is_namespaced() {
        let h = HostHandle::new("c1");
        assert_eq!(h.run_id, "host:c1");
    }

    #[test]
    fn reattach_candidates_ranked_by_host_then_record_then_label_then_cwd() {
        let saved = cref("ssh:root@a:22", Some("rec1"), "web1", Some("/srv"), "old");
        let cands = vec![
            cref("local", None, "", Some("/srv"), "t-cwd"),
            cref("local", None, "web1", None, "t-label"),
            cref("ssh:root@a:22", None, "", None, "t-host"),
            cref("other", Some("rec1"), "", None, "t-rec"),
            cref("local", None, "zzz", None, "t-none"),
        ];
        let ids: Vec<_> = saved.rank(&cands).into_iter().map(|c| c.console_id.as_str()).collect();
        assert_eq!(ids, vec!["t-host", "t-rec", "t-label", "t-cwd"]);
    }

    #[test]
    fn empty_fields_never_match() {
        let a = cref("", None, "", None, "x");
        assert_eq!(a.match_score(&cref("", None, "", None, "y")), 0);
    }

    #[test]
    fn supervisor_applies_and_rejects() {
        let s = HostSupervisor::new();
        let (h, _io) = driver::tests::shared();
        s.register(h);
        assert!(s.apply("c1", HostEvent::Handback).is_err());
        assert_eq!(s.phase("c1"), Some(HostPhase::Running));
        assert!(s.live("host:c1").is_some() && s.live("t1").is_some());
        s.apply("c1", HostEvent::Stop).unwrap();
        assert_eq!(s.phase("c1"), Some(HostPhase::Stopped));
        assert!(s.live("host:c1").is_none(), "终态不算在托管");
        assert!(s.apply("nope", HostEvent::Stop).is_err());
        // 同一 console 再开：旧的终态记录被清掉
        let (h2, _io) = driver::tests::shared();
        s.register(h2);
        assert_eq!(s.all().len(), 1);
    }

    #[test]
    fn supervisor_prunes_old_and_excess_endings_but_keeps_live() {
        let s = HostSupervisor::new();
        let (live, _io) = driver::tests::shared_as("live", "tl");
        s.register(live);
        let (old, _io) = driver::tests::shared_as("old", "t-old");
        old.apply(HostEvent::Stop).unwrap();
        old.mark_ended_ago(ENDED_KEEP + std::time::Duration::from_secs(1));
        s.register(old);
        assert!(s.get("old").is_none(), "结束超过一小时的清掉");
        for i in 0..(ENDED_MAX + 5) {
            let (h, _io) = driver::tests::shared_as(&format!("e{i:02}"), &format!("t{i}"));
            h.apply(HostEvent::Stop).unwrap();
            h.mark_ended_ago(std::time::Duration::from_secs(1000 - i as u64));
            s.register(h);
        }
        let all = s.all();
        assert_eq!(all.iter().filter(|h| h.is_terminal()).count(), ENDED_MAX);
        assert!(s.get("live").is_some(), "未结束的一律保留");
        // 留下的是最近结束的
        assert!(s.get(&format!("e{:02}", ENDED_MAX + 4)).is_some());
        assert!(s.get("e00").is_none());
    }

    #[test]
    fn check_serde_tagged() {
        let c = Check::CommandOk { cmd: "cargo test".into() };
        let j = serde_json::to_string(&c).unwrap();
        assert_eq!(j, r#"{"kind":"commandOk","cmd":"cargo test"}"#);
        assert_eq!(serde_json::from_str::<Check>(&j).unwrap(), c);
    }
}
