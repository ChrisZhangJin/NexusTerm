//! 托管写入的决策记录（设计 §3）：每个真正执行了的托管动作落一条
//! `DecisionRecord{llm_used:true, ctx_refs, chosen, rationale}`，进与守护进程同一张 `steward_decisions` 表，
//! 面板的决策回放、「为什么」解释链、Notify 档的纠正（`learn_correction`）都按 seq 找回它。
//!
//! 拼记录是纯函数；落库走 `persist`。

use super::super::act::{AgentAction, OpenKind, TabOp};
use super::super::decision::{DecisionAction, DecisionRecord};
use super::super::situation::Situation;
use super::driver::HostCtx;
use super::gate::Gate;

/// 决策落 SQLite，返回自增 seq；失败时返回 0（内存里仍可展示，不因审计写失败卡住托管）。
pub fn persist(app: &tauri::AppHandle, rec: &DecisionRecord) -> i64 {
    use tauri::Manager;
    let Some(st) = app.try_state::<crate::cmd::AppState>() else {
        return 0;
    };
    let json = serde_json::to_string(rec).unwrap_or_default();
    st.store
        .steward_decision_insert(
            rec.ts_ms,
            &rec.contract_id,
            &rec.console_id,
            &rec.situation_signature,
            &json,
            &rec.outcome,
        )
        .unwrap_or(0)
}

/// 理由里最多留多少字（模型这一轮的说明）
const RATIONALE_CAP: usize = 600;

/// 动作类别 + 给人看的一句话（不含密码：nx_secret 只记原因）。只读动作返回 `None`。
pub fn describe_write(a: &AgentAction) -> Option<(&'static str, String)> {
    Some(match a {
        AgentAction::Exec { command, .. } => ("exec", command.trim().to_string()),
        AgentAction::Keys { keys, .. } => ("keys", format!("发送按键：{keys}")),
        AgentAction::Secret { reason, .. } => {
            ("secret", format!("请用户输入密码（{}）", reason.as_deref().unwrap_or("未说明")))
        }
        AgentAction::Skill { call, .. } if call.op == super::super::skilltool::SkillOp::Run => {
            ("skill", format!("运行技能：{}", call.key.as_deref().unwrap_or("?")))
        }
        AgentAction::Open { kind, label, command } => {
            let what = match kind {
                OpenKind::Local => "本地".to_string(),
                OpenKind::Ssh { session_record_id } => format!("SSH {session_record_id}"),
            };
            let mut t = format!("打开{what}会话「{}」", label.as_deref().unwrap_or("新会话"));
            if let Some(c) = command {
                t.push_str(&format!("并执行：{}", c.trim()));
            }
            ("open", t)
        }
        AgentAction::Tab { op: TabOp::Close, target, .. } => {
            ("tab_close", format!("关闭标签页 {}", target.as_deref().unwrap_or("当前")))
        }
        AgentAction::Tab { op: TabOp::Reconnect, target, .. } => {
            ("tab_reconnect", format!("重连标签页 {}", target.as_deref().unwrap_or("当前")))
        }
        _ => return None,
    })
}

fn gate_word(g: &Gate) -> &'static str {
    match g {
        Gate::Allow => "放行",
        Gate::Notify => "执行后通知",
        Gate::Approve(_) => "经用户批准",
        Gate::Deny(_) => "拒绝",
    }
}

/// 一条托管决策记录。`seq` 落库后回填。
pub fn hosted_decision(
    ctx: &HostCtx,
    console_id: &str,
    kind: &str,
    text: &str,
    rationale: &str,
    gate: &Gate,
    sit: Option<&Situation>,
    now_ms: i64,
) -> DecisionRecord {
    let why: String = rationale.trim().chars().take(RATIONALE_CAP).collect();
    let why = if why.is_empty() { "（模型这一轮没有写说明）".to_string() } else { why };
    DecisionRecord {
        seq: 0,
        ts_ms: now_ms,
        contract_id: ctx.contract.id.clone(),
        console_id: console_id.to_string(),
        situation_signature: sit.map(|s| s.signature.clone()).unwrap_or_default(),
        situation_hash: sit.map(|s| s.screen_hash.clone()).unwrap_or_default(),
        ctx_refs: ctx.refs.clone(),
        candidates: Vec::new(),
        chosen: DecisionAction {
            kind: kind.to_string(),
            text: text.to_string(),
            option_index: None,
            injectable: true,
            submit: Default::default(),
        },
        rationale: format!("[{}] {why}", gate_word(gate)),
        confidence: 1.0,
        tier: ctx.authority.tier.clone(),
        llm_used: true,
        outcome: "applied".to_string(),
        escalation: None,
        receipt: None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::decision::CtxRef;
    use super::*;

    #[test]
    fn record_is_llm_used_with_refs_and_gate() {
        let mut ctx = HostCtx::bare("c1", "t1", "升级依赖");
        ctx.refs = vec![CtxRef::of("profile", "p1", "不要动 main 分支")];
        let a = AgentAction::Exec { target: None, command: " cargo update ".into() };
        let (kind, text) = describe_write(&a).unwrap();
        let r = hosted_decision(&ctx, "t1", kind, &text, "先更新锁文件 [P:p1]", &Gate::Notify, None, 42);
        assert!(r.llm_used);
        assert_eq!(r.chosen.kind, "exec");
        assert_eq!(r.chosen.text, "cargo update");
        assert_eq!(r.ctx_refs.len(), 1);
        assert!(r.rationale.starts_with("[执行后通知]") && r.rationale.contains("[P:p1]"));
        assert_eq!(r.outcome, "applied");
        assert_eq!(r.contract_id, "c1");
    }

    #[test]
    fn read_only_actions_are_not_writes_and_secret_has_no_value() {
        assert!(describe_write(&AgentAction::Screen { target: None }).is_none());
        let (_, t) = describe_write(&AgentAction::Secret { target: None, reason: Some("sudo".into()) }).unwrap();
        assert_eq!(t, "请用户输入密码（sudo）");
    }
}
