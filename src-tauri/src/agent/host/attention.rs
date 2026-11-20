//! 「托管需要我」的提醒文案：纯函数，只看相位迁移，不碰任何 IO。
//!
//! - 只在**迁移**时提醒：同一张卡 / 同一个暂停原因反复推送（例如 nx_todo 触发的重推）不重复提醒；
//! - 人在驾驶（UserDriving）按它交还后会回到的状态算 —— 人就在终端前，接管 / 交还都不该再提醒一次；
//! - 用户自己点的「暂停」、用户点的「停止」不提醒（是他自己做的）；
//! - 文案只有一行：标签 + 原因，问题 / 摘要截短并脱敏，绝不带终端内容。

use super::{CardKind, HostPhase, PauseCause, SuspendReason};

/// 提醒正文最多多少个字符（系统通知 / 手机推送只看一眼）
const BODY_MAX_CHARS: usize = 80;

/// 一条要发出去的提醒。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attention {
    /// 「NexusTerm 托管 · {标签}」
    pub title: String,
    /// 一行原因，例如「等你回答：要进哪个容器？」
    pub body: String,
    /// waiting / paused / suspended / completed / failed（webhook 的 `state`）
    pub kind: &'static str,
}

/// 这个相位「要不要人」的去重键；`None` = 不需要人。
/// 键相同 = 同一件事，不重复提醒。
pub fn attention_key(p: &HostPhase) -> Option<String> {
    match p.base() {
        HostPhase::WaitingHuman { card } => Some(format!("waiting:{}", card.id)),
        HostPhase::Paused { cause } => match cause {
            PauseCause::User => None,
            PauseCause::Budget { .. } => Some("paused:budget".into()),
            PauseCause::Deadline => Some("paused:deadline".into()),
            PauseCause::LlmDown { .. } => Some("paused:llm_down".into()),
        },
        HostPhase::Suspended { reason, .. } => Some(format!("suspended:{reason:?}")),
        HostPhase::Completed { .. } => Some("completed".into()),
        HostPhase::Failed { .. } => Some("failed".into()),
        HostPhase::Running | HostPhase::Stopped | HostPhase::UserDriving { .. } => None,
    }
}

/// 从 `prev` 迁到 `next` 时要不要提醒、说什么。`label` 是被托管终端的名字。
pub fn attention_for(prev: &HostPhase, next: &HostPhase, label: &str) -> Option<Attention> {
    let key = attention_key(next)?;
    if attention_key(prev).as_deref() == Some(key.as_str()) {
        return None;
    }
    let (kind, head, detail): (&'static str, &str, String) = match next.base() {
        HostPhase::WaitingHuman { card } => {
            let q = card.question.clone();
            match &card.kind {
                CardKind::Ask => ("waiting", "等你回答", q),
                CardKind::Approve => ("waiting", "等你批准", q),
                CardKind::Secret => ("waiting", "等你输入密码", q),
                CardKind::Watchdog => ("waiting", "看门狗请你看一下", q),
                CardKind::ManualCheck { summary, .. } => {
                    ("waiting", "等你确认完成", if summary.trim().is_empty() { q } else { summary.clone() })
                }
                CardKind::CriteriaDraft { .. } => ("waiting", "等你确认完成判据", String::new()),
            }
        }
        HostPhase::Paused { cause } => match cause {
            PauseCause::Budget { reason } => ("paused", "预算用完，已暂停", reason.clone()),
            PauseCause::Deadline => ("paused", "到了约定期限，已暂停", String::new()),
            PauseCause::LlmDown { error } => ("paused", "模型服务不可用，已暂停", error.clone()),
            PauseCause::User => return None,
        },
        HostPhase::Suspended { reason, .. } => match reason {
            SuspendReason::ConsoleGone => ("suspended", "终端断开，托管已挂起", String::new()),
            SuspendReason::AppRestart => ("suspended", "应用重启，等你重新接驳", String::new()),
            SuspendReason::LlmDown => ("suspended", "模型服务连不上，托管已挂起", String::new()),
        },
        HostPhase::Completed { summary, .. } => ("completed", "托管完成", summary.clone()),
        HostPhase::Failed { error } => ("failed", "托管失败", error.clone()),
        _ => return None,
    };
    let detail = one_line(&detail);
    let body = if detail.is_empty() { head.to_string() } else { format!("{head}：{detail}") };
    let label = one_line(label);
    let title = if label.is_empty() { "NexusTerm 托管".to_string() } else { format!("NexusTerm 托管 · {label}") };
    Some(Attention { title, body, kind })
}

/// 脱敏、压成一行、截短。
fn one_line(s: &str) -> String {
    let flat = super::super::redact::redact(s).split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= BODY_MAX_CHARS {
        return flat;
    }
    let mut out: String = flat.chars().take(BODY_MAX_CHARS).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::host::{Check, HumanCard};

    fn card(id: &str, kind: CardKind, q: &str) -> HostPhase {
        let mut c = HumanCard::ask(id, q, 0);
        c.kind = kind;
        HostPhase::WaitingHuman { card: c }
    }

    fn body(prev: &HostPhase, next: &HostPhase) -> Option<String> {
        attention_for(prev, next, "web-01").map(|a| a.body)
    }

    #[test]
    fn running_to_waiting_each_card_kind() {
        let r = HostPhase::Running;
        let a = attention_for(&r, &card("1", CardKind::Ask, "要进哪个容器？"), "web-01").unwrap();
        assert_eq!(a.title, "NexusTerm 托管 · web-01");
        assert_eq!(a.body, "等你回答：要进哪个容器？");
        assert_eq!(a.kind, "waiting");
        assert_eq!(body(&r, &card("2", CardKind::Approve, "rm -rf build")).unwrap(), "等你批准：rm -rf build");
        assert_eq!(body(&r, &card("3", CardKind::Secret, "sudo 密码")).unwrap(), "等你输入密码：sudo 密码");
        assert_eq!(body(&r, &card("4", CardKind::Watchdog, "好像在原地打转")).unwrap(), "看门狗请你看一下：好像在原地打转");
        let manual = CardKind::ManualCheck { summary: "页面能打开了".into(), checks: vec![] };
        assert_eq!(body(&r, &card("5", manual, "确认？")).unwrap(), "等你确认完成：页面能打开了");
        let draft = CardKind::CriteriaDraft { draft: vec![Check::Manual { text: "x".into() }] };
        assert_eq!(body(&r, &card("6", draft, "")).unwrap(), "等你确认完成判据");
    }

    #[test]
    fn leaving_waiting_or_repeating_is_silent() {
        let w = card("1", CardKind::Ask, "q");
        assert_eq!(attention_for(&w, &HostPhase::Running, "t"), None);
        // 同一张卡反复推送（例如 nx_todo 重推）
        assert_eq!(attention_for(&w, &w, "t"), None);
        // 人接管 / 交还同一张卡
        let driving = HostPhase::UserDriving { resume_to: Box::new(w.clone()) };
        assert_eq!(attention_for(&w, &driving, "t"), None);
        assert_eq!(attention_for(&driving, &w, "t"), None);
        // 换了一张新卡：要提醒
        assert!(attention_for(&w, &card("2", CardKind::Ask, "q2"), "t").is_some());
        // 一直在跑
        assert_eq!(attention_for(&HostPhase::Running, &HostPhase::Running, "t"), None);
    }

    #[test]
    fn paused_causes() {
        let r = HostPhase::Running;
        let p = |cause| HostPhase::Paused { cause };
        let b = p(PauseCause::Budget { reason: "已执行 400 步".into() });
        let a = attention_for(&r, &b, "t").unwrap();
        assert_eq!((a.kind, a.body.as_str()), ("paused", "预算用完，已暂停：已执行 400 步"));
        assert_eq!(attention_for(&b, &b, "t"), None);
        assert_eq!(body(&r, &p(PauseCause::Deadline)).unwrap(), "到了约定期限，已暂停");
        assert_eq!(body(&r, &p(PauseCause::LlmDown { error: "429".into() })).unwrap(), "模型服务不可用，已暂停：429");
        // 用户自己点的暂停
        assert_eq!(body(&r, &p(PauseCause::User)), None);
    }

    #[test]
    fn suspended_reasons() {
        let s = |reason| HostPhase::Suspended { reason, resume_to: Box::new(HostPhase::Running) };
        let r = HostPhase::Running;
        let a = attention_for(&r, &s(SuspendReason::ConsoleGone), "t").unwrap();
        assert_eq!((a.kind, a.body.as_str()), ("suspended", "终端断开，托管已挂起"));
        assert!(body(&r, &s(SuspendReason::AppRestart)).is_some());
        assert!(body(&r, &s(SuspendReason::LlmDown)).is_some());
        assert_eq!(attention_for(&s(SuspendReason::ConsoleGone), &s(SuspendReason::ConsoleGone), "t"), None);
        // 断线挂起 → 重启挂起：原因变了，再提醒一次
        assert!(attention_for(&s(SuspendReason::ConsoleGone), &s(SuspendReason::AppRestart), "t").is_some());
    }

    #[test]
    fn terminal_phases() {
        let r = HostPhase::Running;
        let done = HostPhase::Completed { summary: "部署好了\n第二行".into(), checks: vec![] };
        let a = attention_for(&r, &done, "t").unwrap();
        assert_eq!((a.kind, a.body.as_str()), ("completed", "托管完成：部署好了 第二行"));
        let failed = HostPhase::Failed { error: "连不上".into() };
        let a = attention_for(&card("1", CardKind::Ask, "q"), &failed, "t").unwrap();
        assert_eq!((a.kind, a.body.as_str()), ("failed", "托管失败：连不上"));
        // 用户点的停止
        assert_eq!(attention_for(&r, &HostPhase::Stopped, "t"), None);
        assert_eq!(attention_for(&done, &done, "t"), None);
    }

    #[test]
    fn body_is_one_short_redacted_line() {
        let q = format!("密码是 password=hunter2 吗？{}", "很长".repeat(100));
        let a = attention_for(&HostPhase::Running, &card("1", CardKind::Ask, &q), "").unwrap();
        assert_eq!(a.title, "NexusTerm 托管");
        assert!(!a.body.contains("hunter2"), "{}", a.body);
        assert!(!a.body.contains('\n'));
        assert!(a.body.ends_with('…'));
        assert!(a.body.chars().count() <= "等你回答：".chars().count() + BODY_MAX_CHARS + 1);
    }
}
