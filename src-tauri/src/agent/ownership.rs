//! P3 所有权：让「用户永远赢」对 Steward 的注入真正生效。
//!
//! P2 的 `inject_keys` 直写 PTY，绕过了 `control.rs` 的 `gate_drive`，所以用户敲键时
//! Steward 并不会停。本模块补上分片所有权表：按 console 持有写许可，
//! 循环在注入前**必须**查 `can_write`；`ObservingOnly` 期间继续观察与记录，
//! 只是不写、不产出可注入动作（设计 §3.5）。
//!
//! 与 `control.rs` 的分工：那边服务旧一轮 Agentic 路径（run 级租约），
//! 这边服务常驻托管（console 级、跨轮、跨面板生命周期）。两者互不覆盖，
//! 但都遵守同一条语义：用户敲键 → 100ms 内夺权。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;

/// 接管 SLA：设计 §8 的验收线。命令层把 `key_at_ms` 一起送进来，
/// 这里记录真实处理延迟，前端与端到端测试都能核对。
pub const TAKEOVER_SLA_MS: i64 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnershipState {
    /// Steward 持有写许可
    AgentDriving,
    /// 用户敲键 / 手动接管：只观察记录，绝不写终端
    ObservingOnly,
    /// 断连、预算耗尽、意向超时：保留现场，等用户决定
    Suspended,
}

impl OwnershipState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AgentDriving => "agent_driving",
            Self::ObservingOnly => "observing_only",
            Self::Suspended => "suspended",
        }
    }
}

/// 「离开期间发生了什么」的一条记录。屏幕变化、决策、回执、超时都会追加。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AbsenceNote {
    pub ts_ms: i64,
    pub kind: String,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct OwnershipSlot {
    pub state: OwnershipState,
    pub since_ms: i64,
    pub last_key_ms: i64,
    pub takeover_latency_ms: i64,
    pub takeover_count: u32,
    pub suspension_reason: Option<String>,
    pub absence: Vec<AbsenceNote>,
}

impl OwnershipSlot {
    fn new(now: i64) -> Self {
        Self {
            state: OwnershipState::AgentDriving,
            since_ms: now,
            last_key_ms: 0,
            takeover_latency_ms: 0,
            takeover_count: 0,
            suspension_reason: None,
            absence: Vec::new(),
        }
    }
}

/// 交还时给用户的摘要卡：已做 / 在做 / 待决 / 依据 / 下一步建议。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AbsenceSummary {
    pub console_id: String,
    pub since_ms: i64,
    pub until_ms: i64,
    pub duration_ms: i64,
    pub takeover_count: u32,
    pub done: Vec<String>,
    pub pending: Vec<String>,
    pub next_step: String,
    pub screen_aligned: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnershipView {
    pub console_id: String,
    pub state: OwnershipState,
    pub since_ms: i64,
    pub takeover_latency_ms: i64,
    pub takeover_count: u32,
    pub suspension_reason: Option<String>,
    pub absence_count: usize,
}

#[derive(Clone, Default)]
pub struct OwnershipTable(Arc<Mutex<HashMap<String, OwnershipSlot>>>);

impl OwnershipTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn ensure(&self, console_id: &str, now: i64) {
        let mut m = self.0.lock().unwrap();
        m.entry(console_id.to_string())
            .or_insert_with(|| OwnershipSlot::new(now));
    }

    /// 前端在 `termRegistry.onInput` 上节流调用。`key_at_ms` 是键盘事件的墙钟时间戳，
    /// 只能与墙钟 `now_wall` 相减得到处理延迟（用于 SLA 验收）；`since_ms` 与其它
    /// 所有权时间一样走单调时钟 `now_mono`，否则交还时的「离开时长」会把两种时钟混算。
    /// 夺权是同步的，不排队、不等 tick。未登记的 console 返回 -1 且不建槽：
    /// 所有权槽只由契约生命周期创建，敲键不能凭空造出一个可写槽。
    pub fn user_key(&self, console_id: &str, key_at_ms: i64, now_wall: i64, now_mono: i64) -> i64 {
        let mut m = self.0.lock().unwrap();
        let Some(slot) = m.get_mut(console_id) else {
            return -1;
        };
        let latency = (now_wall - key_at_ms).max(0);
        slot.last_key_ms = key_at_ms;
        if slot.state != OwnershipState::ObservingOnly {
            slot.state = OwnershipState::ObservingOnly;
            slot.since_ms = now_mono;
            slot.takeover_count += 1;
            slot.takeover_latency_ms = latency;
            slot.suspension_reason = None;
        }
        let text = if latency > TAKEOVER_SLA_MS {
            format!("用户敲键，{latency}ms 夺权（超出 {TAKEOVER_SLA_MS}ms SLA），Steward 转观察态")
        } else {
            format!("用户敲键，{latency}ms 内夺权，Steward 转观察态")
        };
        slot.absence.push(AbsenceNote {
            ts_ms: now_mono,
            kind: "user_key".into(),
            text,
        });
        trim(&mut slot.absence);
        latency
    }

    pub fn takeover(&self, console_id: &str, now: i64) {
        let mut m = self.0.lock().unwrap();
        let Some(slot) = m.get_mut(console_id) else {
            return;
        };
        slot.state = OwnershipState::ObservingOnly;
        slot.since_ms = now;
        slot.takeover_count += 1;
        slot.takeover_latency_ms = 0;
        slot.suspension_reason = None;
    }

    /// 用户点「交还」。先由命令层读屏对齐，再把 `screen_aligned` 结果传进来；
    /// 返回的摘要就是设计 §3.5 的「离开期间发生了什么 + 当前待决 + 下一步建议」。
    /// 未登记的 console 只返回一张空摘要，不会凭空建一个可写槽。
    pub fn handback(
        &self,
        console_id: &str,
        now: i64,
        screen_aligned: bool,
    ) -> AbsenceSummary {
        let mut m = self.0.lock().unwrap();
        let Some(slot) = m.get_mut(console_id) else {
            return AbsenceSummary {
                console_id: console_id.to_string(),
                since_ms: now,
                until_ms: now,
                duration_ms: 0,
                takeover_count: 0,
                done: Vec::new(),
                pending: Vec::new(),
                next_step: "该 console 当前没有托管契约".to_string(),
                screen_aligned,
            };
        };
        let notes = std::mem::take(&mut slot.absence);
        let mut done = Vec::new();
        let mut pending = Vec::new();
        for n in &notes {
            match n.kind.as_str() {
                "decision" | "applied" | "receipt" | "advance" | "checkpoint" => {
                    done.push(n.text.clone())
                }
                "escalation" | "receipt_timeout" | "no_progress" | "stale_feed"
                | "suspend" => pending.push(n.text.clone()),
                _ => {}
            }
        }
        done.truncate(20);
        pending.truncate(10);
        let next_step = if !screen_aligned {
            "屏幕与 Checkpoint 不一致：已降为「确认」档，请先核对现场".to_string()
        } else if pending.is_empty() {
            "现场已对齐，从最后 Checkpoint 继续值守".to_string()
        } else {
            "现场已对齐，但有待决事项：建议先处理升级卡再继续".to_string()
        };
        let summary = AbsenceSummary {
            console_id: console_id.to_string(),
            since_ms: slot.since_ms,
            until_ms: now,
            duration_ms: (now - slot.since_ms).max(0),
            takeover_count: slot.takeover_count,
            done,
            pending,
            next_step,
            screen_aligned,
        };
        slot.state = OwnershipState::AgentDriving;
        slot.since_ms = now;
        slot.absence.clear();
        summary
    }

    pub fn suspend(&self, console_id: &str, reason: &str, now: i64) {
        let mut m = self.0.lock().unwrap();
        let Some(slot) = m.get_mut(console_id) else {
            return;
        };
        slot.state = OwnershipState::Suspended;
        slot.since_ms = now;
        slot.suspension_reason = Some(reason.to_string());
        slot.absence.push(AbsenceNote {
            ts_ms: now,
            kind: "suspend".into(),
            text: format!("托管暂停：{reason}（现场已保留）"),
        });
        trim(&mut slot.absence);
    }

    pub fn resume(&self, console_id: &str, now: i64) {
        let mut m = self.0.lock().unwrap();
        let Some(slot) = m.get_mut(console_id) else {
            return;
        };
        slot.state = OwnershipState::AgentDriving;
        slot.since_ms = now;
        slot.suspension_reason = None;
    }

    /// 循环在每次注入前查这个。`Suspended` 与 `ObservingOnly` 都不写。
    pub fn can_write(&self, console_id: &str) -> bool {
        self.0
            .lock()
            .unwrap()
            .get(console_id)
            .map(|s| s.state == OwnershipState::AgentDriving)
            .unwrap_or(false)
    }

    pub fn state(&self, console_id: &str) -> OwnershipState {
        self.0
            .lock()
            .unwrap()
            .get(console_id)
            .map(|s| s.state)
            .unwrap_or(OwnershipState::AgentDriving)
    }

    /// 只有契约生命周期登记过的 console 才有槽（测试用来断言敲键不会凭空建槽）。
    #[cfg(test)]
    pub fn has(&self, console_id: &str) -> bool {
        self.0.lock().unwrap().contains_key(console_id)
    }

    /// 观察期照样记事件，交还时才有东西可汇报。
    pub fn note(&self, console_id: &str, kind: &str, text: impl Into<String>, now: i64) {
        let mut m = self.0.lock().unwrap();
        let Some(slot) = m.get_mut(console_id) else {
            return;
        };
        slot.absence.push(AbsenceNote {
            ts_ms: now,
            kind: kind.to_string(),
            text: text.into(),
        });
        trim(&mut slot.absence);
    }

    pub fn view(&self) -> Vec<OwnershipView> {
        let m = self.0.lock().unwrap();
        let mut v: Vec<_> = m
            .iter()
            .map(|(id, s)| OwnershipView {
                console_id: id.clone(),
                state: s.state,
                since_ms: s.since_ms,
                takeover_latency_ms: s.takeover_latency_ms,
                takeover_count: s.takeover_count,
                suspension_reason: s.suspension_reason.clone(),
                absence_count: s.absence.len(),
            })
            .collect();
        v.sort_by(|a, b| a.console_id.cmp(&b.console_id));
        v
    }

    pub fn drop(&self, console_id: &str) {
        self.0.lock().unwrap().remove(console_id);
    }
}

const MAX_NOTES: usize = 200;

fn trim(v: &mut Vec<AbsenceNote>) {
    if v.len() > MAX_NOTES {
        let excess = v.len() - MAX_NOTES;
        v.drain(..excess);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_key_takes_over_within_sla() {
        let t = OwnershipTable::new();
        assert!(t.can_write("tab-1") == false, "未登记的 console 不能写");
        t.ensure("tab-1", 0);
        assert!(t.can_write("tab-1"));
        let latency = t.user_key("tab-1", 10_000, 10_042, 50);
        assert_eq!(latency, 42);
        assert!(latency < TAKEOVER_SLA_MS);
        assert_eq!(t.state("tab-1"), OwnershipState::ObservingOnly);
        assert!(!t.can_write("tab-1"));
    }

    #[test]
    fn repeated_keys_do_not_inflate_takeover_count() {
        let t = OwnershipTable::new();
        t.ensure("tab-1", 0);
        for i in 0..5 {
            t.user_key("tab-1", 1_000 + i * 20, 1_005 + i * 20, 5 + i * 20);
        }
        let v = t.view();
        assert_eq!(v[0].takeover_count, 1);
        assert_eq!(v[0].absence_count, 5);
    }

    #[test]
    fn handback_summarizes_absence_and_restores_write() {
        let t = OwnershipTable::new();
        t.ensure("tab-1", 0);
        // 墙钟与单调钟故意错开：since_ms 必须取单调钟 110，而不是墙钟 1_000_110
        t.user_key("tab-1", 1_000_100, 1_000_110, 110);
        t.note("tab-1", "applied", "按意向回答了 settings 提问", 500);
        t.note("tab-1", "receipt", "回执确认", 900);
        t.note("tab-1", "escalation", "有一次权限确认待处理", 1_200);
        let s = t.handback("tab-1", 2_000, true);
        assert_eq!(s.duration_ms, 1_890);
        assert_eq!(s.done.len(), 2);
        assert_eq!(s.pending.len(), 1);
        assert!(s.next_step.contains("待决"));
        assert!(t.can_write("tab-1"));
        assert_eq!(t.view()[0].absence_count, 0);
    }

    #[test]
    fn misaligned_screen_downgrades_next_step() {
        let t = OwnershipTable::new();
        t.ensure("tab-1", 0);
        let s = t.handback("tab-1", 500, false);
        assert!(!s.screen_aligned);
        assert!(s.next_step.contains("确认"));
    }

    #[test]
    fn suspended_cannot_write_until_resumed() {
        let t = OwnershipTable::new();
        t.ensure("tab-1", 0);
        t.suspend("tab-1", "SSH 断连", 300);
        assert_eq!(t.state("tab-1"), OwnershipState::Suspended);
        assert!(!t.can_write("tab-1"));
        t.resume("tab-1", 900);
        assert!(t.can_write("tab-1"));
    }

    #[test]
    fn notes_are_bounded() {
        let t = OwnershipTable::new();
        t.ensure("tab-1", 0);
        for i in 0..300 {
            t.note("tab-1", "decision", format!("d{i}"), i);
        }
        assert_eq!(t.view()[0].absence_count, MAX_NOTES);
    }

    #[test]
    fn consoles_are_independent() {
        let t = OwnershipTable::new();
        t.ensure("a", 0);
        t.ensure("b", 0);
        t.user_key("a", 10, 15, 15);
        assert!(!t.can_write("a"));
        assert!(t.can_write("b"), "一个 tab 被接管不应影响另一个");
    }

    #[test]
    fn mutators_never_create_slots_for_unknown_consoles() {
        let t = OwnershipTable::new();
        assert_eq!(t.user_key("ghost", 10, 15, 15), -1);
        t.takeover("ghost", 1);
        t.suspend("ghost", "x", 1);
        t.resume("ghost", 1);
        t.note("ghost", "decision", "x", 1);
        let s = t.handback("ghost", 5, true);
        assert_eq!(s.duration_ms, 0);
        assert!(!t.has("ghost"));
        assert!(t.view().is_empty());
        // 关键：resume / handback 不能让一个没登记的 console 变成可写
        assert!(!t.can_write("ghost"));
    }

    #[test]
    fn slow_takeover_is_flagged_against_sla() {
        let t = OwnershipTable::new();
        t.ensure("tab-1", 0);
        let latency = t.user_key("tab-1", 0, TAKEOVER_SLA_MS + 50, 1);
        assert!(latency > TAKEOVER_SLA_MS);
        let s = t.handback("tab-1", 10, true);
        assert_eq!(s.duration_ms, 9, "离开时长按单调钟计");
    }
}
