//! 托管生命周期（设计 §2）：纯函数 `step(phase, event) -> (下一相位, 副作用)`。
//!
//! 三条不变式由这里保证，运行时只管执行副作用：
//! - **交还回到 `resume_to`**：Paused(Budget) 时按键 → UserDriving{resume_to: Paused(Budget)}，
//!   交还后仍是 Paused，预算 / deadline 不会被「按一下键再交还」绕过；
//! - **Paused 只能由 Resume 离开**（以及 Stop / Fatal / 重启挂起）；
//! - **终态吸收**：Completed / Failed / Stopped 收到任何事件都原样返回、无副作用。
//!
//! 租约（ControlTable）只在进入终态时释放；暂停、接管、挂起都不释放。

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{BudgetGrant, CardKind, CardReply, CheckResult, ConsoleRef, HumanCard, PauseCause, SuspendReason};

/// 重新接驳后追加给模型的那句话。
pub const REATTACH_NOTE: &str = "应用重启后重新接驳，先读屏核对现场";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum HostPhase {
    Running,
    /// nx_ask_user / 审批 / 看门狗 L2 / 人工确认的 check
    WaitingHuman { card: HumanCard },
    /// 不设超时
    UserDriving { resume_to: Box<HostPhase> },
    Paused { cause: PauseCause },
    /// `resume_to` = 挂起前的状态（已剥掉 UserDriving），接驳确认后回到它
    Suspended { reason: SuspendReason, resume_to: Box<HostPhase> },
    Completed { summary: String, checks: Vec<CheckResult> },
    Failed { error: String },
    Stopped,
}

impl HostPhase {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed { .. } | Self::Failed { .. } | Self::Stopped)
    }

    /// 落 `host_sessions.state` 列、给 UI 胶囊用的短标签。
    pub fn label(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::WaitingHuman { .. } => "waiting_human",
            Self::UserDriving { .. } => "user_driving",
            Self::Paused { .. } => "paused",
            Self::Suspended { .. } => "suspended",
            Self::Completed { .. } => "completed",
            Self::Failed { .. } => "failed",
            Self::Stopped => "stopped",
        }
    }

    /// 剥掉（可能嵌套的）UserDriving，得到人交还后会回到的那个状态。
    pub fn base(&self) -> &HostPhase {
        match self {
            Self::UserDriving { resume_to } => resume_to.base(),
            p => p,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum HostEvent {
    /// 用户在被托管的终端里按了键
    UserKey,
    /// 用户点了「接管」
    Takeover,
    /// 用户点了「交还」
    Handback,
    Ask(HumanCard),
    Answer(CardReply),
    /// 契约级 `Spent` 判定用尽；参数是给人看的原因
    BudgetExhausted(String),
    /// 意向 deadline 到了
    DeadlineReached,
    /// 用户点了「暂停」
    Pause,
    /// 只能由用户触发：「加预算继续」/「继续」
    Resume { new_budget: Option<BudgetGrant> },
    /// 启动扫描：应用重启了
    Restart,
    /// 运行中失去现场（console 关了 / LLM 连不上）
    Suspend(SuspendReason),
    /// 用户确认重新接驳到这个 console
    Reattach(ConsoleRef),
    /// `nx_complete` 且 checks 全过
    CompleteVerified { summary: String, checks: Vec<CheckResult> },
    /// `nx_complete` 但有没满足的项；参数是未满足项的描述
    CompleteRejected(Vec<String>),
    Stop,
    Fatal(String),
}

impl HostEvent {
    pub fn name(&self) -> &'static str {
        match self {
            Self::UserKey => "user_key",
            Self::Takeover => "takeover",
            Self::Handback => "handback",
            Self::Ask(_) => "ask",
            Self::Answer(_) => "answer",
            Self::BudgetExhausted(_) => "budget_exhausted",
            Self::DeadlineReached => "deadline_reached",
            Self::Pause => "pause",
            Self::Resume { .. } => "resume",
            Self::Restart => "restart",
            Self::Suspend(_) => "suspend",
            Self::Reattach(_) => "reattach",
            Self::CompleteVerified { .. } => "complete_verified",
            Self::CompleteRejected(_) => "complete_rejected",
            Self::Stop => "stop",
            Self::Fatal(_) => "fatal",
        }
    }
}

/// 迁移要求运行时做的事。顺序即执行顺序。
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// 每次相位真正改变都写一次 checkpoint
    SaveCheckpoint,
    /// 把新相位发到 `nx://host/{id}`
    EmitPhase,
    /// 进入终态：释放 ControlTable 租约
    ReleaseLease,
    /// 以用户消息追加进对话（卡片答复 / 接驳提示）
    AppendUser(String),
    /// 追加到下一步的工具反馈里（未满足的完成判据）
    Feedback(String),
    /// 审批卡的结论，交给挂着的写入
    Approval { card_id: String, approved: bool },
    /// 用户给了新预算
    ApplyBudget(BudgetGrant),
    /// 接驳到新的 console，并强制先 `nx_screen`
    Reattach(ConsoleRef),
    ForceScreen,
}

/// 当前相位不接受这个事件。状态不变。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid {
    pub from: &'static str,
    pub event: &'static str,
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "托管状态 {} 不接受事件 {}", self.from, self.event)
    }
}

impl std::error::Error for Invalid {}

fn moved(next: HostPhase, extra: Vec<Effect>) -> Result<(HostPhase, Vec<Effect>), Invalid> {
    let mut fx = Vec::with_capacity(extra.len() + 3);
    if next.is_terminal() {
        fx.push(Effect::ReleaseLease);
    }
    fx.extend(extra);
    fx.push(Effect::SaveCheckpoint);
    fx.push(Effect::EmitPhase);
    Ok((next, fx))
}

/// 纯迁移函数。见模块注释与设计 §2 的迁移表。
pub fn step(phase: &HostPhase, ev: HostEvent) -> Result<(HostPhase, Vec<Effect>), Invalid> {
    use HostEvent as E;
    use HostPhase as P;

    if phase.is_terminal() {
        return Ok((phase.clone(), Vec::new()));
    }
    let invalid = || Invalid { from: phase.label(), event: ev.name() };

    match (phase, &ev) {
        (_, E::Stop) => moved(P::Stopped, vec![]),
        (_, E::Fatal(e)) => moved(P::Failed { error: e.clone() }, vec![]),

        // 人来了：记住当前态，交还时回去。已经在驾驶就保持（不嵌套）
        (P::UserDriving { .. }, E::UserKey | E::Takeover) => Ok((phase.clone(), Vec::new())),
        (_, E::UserKey | E::Takeover) => {
            moved(P::UserDriving { resume_to: Box::new(phase.clone()) }, vec![])
        }
        (P::UserDriving { resume_to }, E::Handback) => moved((**resume_to).clone(), vec![]),

        // 重启 / 失去现场：挂起，记住人交还后本该回到的状态
        (P::Suspended { resume_to, .. }, E::Restart) => Ok((
            P::Suspended { reason: SuspendReason::AppRestart, resume_to: resume_to.clone() },
            vec![Effect::SaveCheckpoint, Effect::EmitPhase],
        )),
        (P::Suspended { .. }, E::Suspend(_)) => Err(invalid()),
        (_, E::Restart) => moved(
            P::Suspended { reason: SuspendReason::AppRestart, resume_to: Box::new(phase.base().clone()) },
            vec![],
        ),
        (_, E::Suspend(r)) => moved(
            P::Suspended { reason: *r, resume_to: Box::new(phase.base().clone()) },
            vec![],
        ),
        (P::Suspended { resume_to, .. }, E::Reattach(c)) => moved(
            (**resume_to).clone(),
            vec![
                Effect::Reattach(c.clone()),
                Effect::ForceScreen,
                Effect::AppendUser(REATTACH_NOTE.to_string()),
            ],
        ),

        (P::Running, E::Ask(card)) => moved(P::WaitingHuman { card: card.clone() }, vec![]),
        (P::WaitingHuman { card }, E::Answer(reply)) => {
            if reply.card_id != card.id {
                return Err(invalid());
            }
            answer(card, reply)
        }

        (P::Running, E::BudgetExhausted(r)) => {
            moved(P::Paused { cause: PauseCause::Budget { reason: r.clone() } }, vec![])
        }
        (P::Running, E::DeadlineReached) => moved(P::Paused { cause: PauseCause::Deadline }, vec![]),
        (P::Running, E::Pause) => moved(P::Paused { cause: PauseCause::User }, vec![]),
        (P::Paused { .. }, E::Resume { new_budget }) => {
            let fx = new_budget.map(Effect::ApplyBudget).into_iter().collect();
            moved(P::Running, fx)
        }

        (P::Running, E::CompleteVerified { summary, checks }) => {
            moved(P::Completed { summary: summary.clone(), checks: checks.clone() }, vec![])
        }
        (P::Running, E::CompleteRejected(unmet)) => Ok((
            P::Running,
            vec![Effect::Feedback(format!("未满足：{}", unmet.join("；")))],
        )),

        _ => Err(invalid()),
    }
}

fn answer(card: &HumanCard, reply: &CardReply) -> Result<(HostPhase, Vec<Effect>), Invalid> {
    match &card.kind {
        CardKind::Approve => {
            let approved = reply.approved.unwrap_or(false);
            let mut fx = vec![Effect::Approval { card_id: card.id.clone(), approved }];
            if !reply.text.trim().is_empty() {
                fx.push(Effect::AppendUser(format!("用户答复：{}", reply.text.trim())));
            }
            moved(HostPhase::Running, fx)
        }
        CardKind::ManualCheck { summary, checks } if reply.approved == Some(true) => {
            let checks = checks
                .iter()
                .map(|c| {
                    if c.check.is_manual() {
                        CheckResult { check: c.check.clone(), passed: true, detail: "用户确认".into() }
                    } else {
                        c.clone()
                    }
                })
                .collect();
            moved(HostPhase::Completed { summary: summary.clone(), checks }, vec![])
        }
        CardKind::ManualCheck { .. } => {
            let why = if reply.text.trim().is_empty() { "（未说明）" } else { reply.text.trim() };
            moved(
                HostPhase::Running,
                vec![Effect::Feedback(format!("未满足：用户没有确认完成，{why}"))],
            )
        }
        CardKind::Ask | CardKind::Watchdog => moved(
            HostPhase::Running,
            vec![Effect::AppendUser(format!("用户答复：{}", reply.text.trim()))],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::super::Check;
    use super::*;

    fn card(id: &str, kind: CardKind) -> HumanCard {
        let mut c = HumanCard::ask(id, "继续吗？", 1);
        c.kind = kind;
        c
    }
    fn waiting() -> HostPhase {
        HostPhase::WaitingHuman { card: card("k1", CardKind::Ask) }
    }
    fn paused_budget() -> HostPhase {
        HostPhase::Paused { cause: PauseCause::Budget { reason: "步数".into() } }
    }
    fn driving(p: HostPhase) -> HostPhase {
        HostPhase::UserDriving { resume_to: Box::new(p) }
    }
    fn suspended(p: HostPhase) -> HostPhase {
        HostPhase::Suspended { reason: SuspendReason::AppRestart, resume_to: Box::new(p) }
    }
    fn reply(id: &str, text: &str, approved: Option<bool>) -> CardReply {
        CardReply { card_id: id.into(), text: text.into(), approved }
    }
    fn console() -> ConsoleRef {
        ConsoleRef { host_key: "local".into(), console_id: "t2".into(), ..Default::default() }
    }
    fn next(p: &HostPhase, e: HostEvent) -> HostPhase {
        step(p, e).expect("迁移应合法").0
    }
    fn non_terminal() -> Vec<HostPhase> {
        vec![
            HostPhase::Running,
            waiting(),
            driving(HostPhase::Running),
            paused_budget(),
            HostPhase::Paused { cause: PauseCause::Deadline },
            HostPhase::Paused { cause: PauseCause::User },
            suspended(HostPhase::Running),
        ]
    }
    fn terminal() -> Vec<HostPhase> {
        vec![
            HostPhase::Completed { summary: "ok".into(), checks: vec![] },
            HostPhase::Failed { error: "x".into() },
            HostPhase::Stopped,
        ]
    }
    fn all_events() -> Vec<HostEvent> {
        vec![
            HostEvent::UserKey,
            HostEvent::Takeover,
            HostEvent::Handback,
            HostEvent::Ask(card("k9", CardKind::Ask)),
            HostEvent::Answer(reply("k1", "好", None)),
            HostEvent::BudgetExhausted("步数".into()),
            HostEvent::DeadlineReached,
            HostEvent::Pause,
            HostEvent::Resume { new_budget: None },
            HostEvent::Restart,
            HostEvent::Suspend(SuspendReason::ConsoleGone),
            HostEvent::Reattach(console()),
            HostEvent::CompleteVerified { summary: "s".into(), checks: vec![] },
            HostEvent::CompleteRejected(vec!["a".into()]),
            HostEvent::Stop,
            HostEvent::Fatal("boom".into()),
        ]
    }

    #[test]
    fn terminal_states_absorb_everything() {
        for t in terminal() {
            for e in all_events() {
                let (p, fx) = step(&t, e).unwrap();
                assert_eq!(p, t);
                assert!(fx.is_empty());
            }
        }
    }

    #[test]
    fn user_key_and_takeover_from_any_non_terminal_remember_current() {
        for p in non_terminal() {
            for e in [HostEvent::UserKey, HostEvent::Takeover] {
                let n = next(&p, e);
                match &p {
                    HostPhase::UserDriving { .. } => assert_eq!(n, p, "已在驾驶不嵌套"),
                    _ => assert_eq!(n, driving(p.clone())),
                }
            }
        }
    }

    #[test]
    fn handback_returns_to_resume_to_not_always_running() {
        for p in [HostPhase::Running, waiting(), paused_budget(), suspended(HostPhase::Running)] {
            let d = next(&p, HostEvent::UserKey);
            assert_eq!(next(&d, HostEvent::Handback), p);
        }
    }

    #[test]
    fn budget_bypass_is_closed() {
        // 预算暂停 → 按键 → 交还 → 仍是预算暂停
        let p = paused_budget();
        let d = next(&p, HostEvent::UserKey);
        assert_eq!(d, driving(paused_budget()));
        let back = next(&d, HostEvent::Handback);
        assert_eq!(back, paused_budget());
        // 驾驶期间也不能 Resume
        assert!(step(&d, HostEvent::Resume { new_budget: None }).is_err());
    }

    #[test]
    fn paused_exits_only_via_resume_stop_fatal_takeover_or_suspend() {
        for p in [paused_budget(), HostPhase::Paused { cause: PauseCause::Deadline }] {
            for e in all_events() {
                let name = e.name();
                match step(&p, e) {
                    Ok((n, _)) => match name {
                        "resume" => assert_eq!(n, HostPhase::Running),
                        "stop" => assert_eq!(n, HostPhase::Stopped),
                        "fatal" => assert!(matches!(n, HostPhase::Failed { .. })),
                        "user_key" | "takeover" => assert_eq!(n, driving(p.clone())),
                        "restart" | "suspend" => {
                            assert!(matches!(n, HostPhase::Suspended { ref resume_to, .. } if **resume_to == p))
                        }
                        other => panic!("Paused 不应接受 {other}"),
                    },
                    Err(_) => assert!(
                        !matches!(name, "resume" | "stop" | "fatal" | "user_key" | "takeover" | "restart" | "suspend"),
                        "{name} 应合法"
                    ),
                }
            }
        }
    }

    #[test]
    fn resume_applies_new_budget() {
        let g = BudgetGrant { max_steps: Some(100), ..Default::default() };
        let (n, fx) = step(&paused_budget(), HostEvent::Resume { new_budget: Some(g) }).unwrap();
        assert_eq!(n, HostPhase::Running);
        assert_eq!(fx[0], Effect::ApplyBudget(g));
        assert!(fx.contains(&Effect::SaveCheckpoint));
        assert!(!fx.contains(&Effect::ReleaseLease));
    }

    #[test]
    fn running_transitions() {
        let r = HostPhase::Running;
        assert_eq!(next(&r, HostEvent::Ask(card("a", CardKind::Ask))), HostPhase::WaitingHuman {
            card: card("a", CardKind::Ask)
        });
        assert_eq!(next(&r, HostEvent::BudgetExhausted("x".into())), HostPhase::Paused {
            cause: PauseCause::Budget { reason: "x".into() }
        });
        assert_eq!(next(&r, HostEvent::DeadlineReached), HostPhase::Paused { cause: PauseCause::Deadline });
        assert_eq!(next(&r, HostEvent::Pause), HostPhase::Paused { cause: PauseCause::User });
        assert!(step(&r, HostEvent::Handback).is_err());
        assert!(step(&r, HostEvent::Resume { new_budget: None }).is_err());
        assert!(step(&r, HostEvent::Reattach(console())).is_err());
        assert!(step(&r, HostEvent::Answer(reply("a", "", None))).is_err());
    }

    #[test]
    fn complete_verified_releases_lease_rejected_stays_running() {
        let checks = vec![CheckResult { check: Check::FileExists { path: "/x".into() }, passed: true, detail: String::new() }];
        let (n, fx) = step(&HostPhase::Running, HostEvent::CompleteVerified { summary: "done".into(), checks: checks.clone() }).unwrap();
        assert_eq!(n, HostPhase::Completed { summary: "done".into(), checks });
        assert_eq!(fx[0], Effect::ReleaseLease);

        let (n, fx) = step(&HostPhase::Running, HostEvent::CompleteRejected(vec!["测试没过".into(), "文件不在".into()])).unwrap();
        assert_eq!(n, HostPhase::Running);
        assert_eq!(fx, vec![Effect::Feedback("未满足：测试没过；文件不在".into())]);
        // 只有 Running 能完成
        assert!(step(&waiting(), HostEvent::CompleteVerified { summary: "s".into(), checks: vec![] }).is_err());
    }

    #[test]
    fn stop_and_fatal_from_every_non_terminal_release_lease() {
        for p in non_terminal() {
            let (n, fx) = step(&p, HostEvent::Stop).unwrap();
            assert_eq!(n, HostPhase::Stopped);
            assert!(fx.contains(&Effect::ReleaseLease));
            let (n, fx) = step(&p, HostEvent::Fatal("e".into())).unwrap();
            assert_eq!(n, HostPhase::Failed { error: "e".into() });
            assert!(fx.contains(&Effect::ReleaseLease));
        }
    }

    #[test]
    fn lease_never_released_on_non_terminal_moves() {
        for p in non_terminal() {
            for e in all_events() {
                if matches!(e, HostEvent::Stop | HostEvent::Fatal(_)) {
                    continue;
                }
                if let Ok((n, fx)) = step(&p, e) {
                    if !n.is_terminal() {
                        assert!(!fx.contains(&Effect::ReleaseLease));
                    }
                }
            }
        }
    }

    #[test]
    fn answer_ask_card_appends_user_message() {
        let (n, fx) = step(&waiting(), HostEvent::Answer(reply("k1", " 用 B 方案 ", None))).unwrap();
        assert_eq!(n, HostPhase::Running);
        assert_eq!(fx[0], Effect::AppendUser("用户答复：用 B 方案".into()));
    }

    #[test]
    fn answer_with_wrong_card_id_is_rejected() {
        assert!(step(&waiting(), HostEvent::Answer(reply("other", "x", None))).is_err());
    }

    #[test]
    fn approval_card_reports_decision_default_deny() {
        let w = HostPhase::WaitingHuman { card: card("ap", CardKind::Approve) };
        let (_, fx) = step(&w, HostEvent::Answer(reply("ap", "", Some(true)))).unwrap();
        assert_eq!(fx[0], Effect::Approval { card_id: "ap".into(), approved: true });
        let (n, fx) = step(&w, HostEvent::Answer(reply("ap", "", None))).unwrap();
        assert_eq!(n, HostPhase::Running);
        assert_eq!(fx[0], Effect::Approval { card_id: "ap".into(), approved: false });
    }

    #[test]
    fn manual_check_card_completes_only_on_approval() {
        let checks = vec![
            CheckResult { check: Check::FileExists { path: "/x".into() }, passed: true, detail: String::new() },
            CheckResult { check: Check::Manual { text: "页面能打开".into() }, passed: false, detail: String::new() },
        ];
        let w = HostPhase::WaitingHuman {
            card: card("m", CardKind::ManualCheck { summary: "部署完成".into(), checks }),
        };
        let (n, fx) = step(&w, HostEvent::Answer(reply("m", "", Some(true)))).unwrap();
        match n {
            HostPhase::Completed { summary, checks } => {
                assert_eq!(summary, "部署完成");
                assert!(checks.iter().all(|c| c.passed));
            }
            other => panic!("应完成，得到 {other:?}"),
        }
        assert_eq!(fx[0], Effect::ReleaseLease);
        let (n, fx) = step(&w, HostEvent::Answer(reply("m", "页面 502", Some(false)))).unwrap();
        assert_eq!(n, HostPhase::Running);
        assert_eq!(fx[0], Effect::Feedback("未满足：用户没有确认完成，页面 502".into()));
    }

    #[test]
    fn restart_suspends_and_reattach_restores_saved_state() {
        for p in [HostPhase::Running, waiting(), paused_budget()] {
            let s = next(&p, HostEvent::Restart);
            assert_eq!(s, suspended(p.clone()));
            let (back, fx) = step(&s, HostEvent::Reattach(console())).unwrap();
            assert_eq!(back, p, "挂起中的卡片 / 暂停原样恢复");
            assert_eq!(&fx[..3], &[
                Effect::Reattach(console()),
                Effect::ForceScreen,
                Effect::AppendUser(REATTACH_NOTE.into()),
            ]);
        }
    }

    #[test]
    fn restart_while_user_driving_drops_the_driver() {
        let d = driving(paused_budget());
        assert_eq!(next(&d, HostEvent::Restart), suspended(paused_budget()));
        // 嵌套也剥干净
        let dd = driving(driving(HostPhase::Running));
        assert_eq!(dd.base(), &HostPhase::Running);
    }

    #[test]
    fn restart_while_suspended_keeps_original_resume_to() {
        let s = HostPhase::Suspended { reason: SuspendReason::ConsoleGone, resume_to: Box::new(paused_budget()) };
        assert_eq!(next(&s, HostEvent::Restart), suspended(paused_budget()));
        assert!(step(&s, HostEvent::Suspend(SuspendReason::LlmDown)).is_err());
    }

    #[test]
    fn suspend_for_console_gone_or_llm_down() {
        let s = next(&HostPhase::Running, HostEvent::Suspend(SuspendReason::LlmDown));
        assert_eq!(s, HostPhase::Suspended { reason: SuspendReason::LlmDown, resume_to: Box::new(HostPhase::Running) });
    }

    #[test]
    fn waiting_human_has_no_way_out_but_answer_stop_fatal_driver_suspend() {
        for e in all_events() {
            let name = e.name();
            let ok = step(&waiting(), e).is_ok();
            let expect = matches!(name, "answer" | "stop" | "fatal" | "user_key" | "takeover" | "restart" | "suspend");
            assert_eq!(ok, expect, "WaitingHuman + {name}");
        }
    }

    #[test]
    fn every_real_move_saves_and_emits() {
        for p in non_terminal() {
            for e in all_events() {
                if let Ok((n, fx)) = step(&p, e) {
                    if n != p {
                        assert!(fx.contains(&Effect::SaveCheckpoint) && fx.contains(&Effect::EmitPhase));
                    }
                }
            }
        }
    }

    #[test]
    fn phase_serde_round_trip() {
        for p in non_terminal().into_iter().chain(terminal()) {
            let j = serde_json::to_string(&p).unwrap();
            assert_eq!(serde_json::from_str::<HostPhase>(&j).unwrap(), p, "{j}");
        }
    }
}
