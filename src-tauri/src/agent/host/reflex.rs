//! 反射快路径（设计 §1）：托管任务阻塞在 `nx_watch`、观察到 NeedsAttention 时，先看有没有
//! 确定的剧本答案，有就在同一个租约下照常受控写入，**不叫醒模型**（零 LLM 调用）。
//!
//! 只有这几条全满足才代答，否则一律叫醒模型：
//! 1. `profile::match_playbook` 命中，剧本自身不是 approve 档；
//! 2. 提交方式不是方向键菜单（ArrowSelect 输编号 / 文字都无效、回车会提交默认项）；
//! 3. 要发的字节合法（单行、无控制字符；`DecisionAction::send_bytes`），且能无歧义地写成按键串；
//! 4. `policy::evaluate` 是 Auto / Notify（密码 / 永久扩权 / deny 都在这里 fail-closed）；
//! 5. 托管强制点 `gate::authorize` 对这次按键给 Allow（Notify / Approve 都交给模型走正常路径）。
//!
//! 纯函数；写入、复核（keys_recheck）、落库在 `act.rs`。

use super::super::act::AgentAction;
use super::super::decision::{CtxRef, DecisionAction, DecisionRecord};
use super::super::policy::{self, PolicyVerdict, TIER_APPROVE};
use super::super::profile::{CtxItem, PlaybookSpec};
use super::super::situation::{submit_style, Situation, SubmitStyle};
use super::driver::HostCtx;
use super::gate::{authorize, Gate};

#[derive(Debug, Clone)]
pub enum Reflex {
    Answer {
        /// 真正写进 pty 的字节
        bytes: Vec<u8>,
        /// 同一内容的按键串写法（过闸、展示用）
        keys: String,
        /// 落库用（`llm_used:false`）
        record: DecisionRecord,
        playbook_id: String,
    },
    /// 不代答，叫醒模型；参数是原因（trace 用）
    Wake(String),
}

/// 字节 → `interact::parse_keys` 的按键串。含 `<` / `\` 的不转（避免被当成按键名 / 转义）。
fn to_keys(bytes: &[u8]) -> Option<String> {
    let (body, enter) = match bytes.strip_suffix(b"\r") {
        Some(b) => (b, true),
        None => (bytes, false),
    };
    let s = std::str::from_utf8(body).ok()?;
    if s.is_empty() || s.contains('<') || s.contains('\\') || s.chars().any(char::is_control) {
        return None;
    }
    Some(if enter { format!("{s}<Enter>") } else { s.to_string() })
}

pub fn decide(playbook: Option<(CtxItem, PlaybookSpec)>, sit: &Situation, ctx: &HostCtx, now_ms: i64) -> Reflex {
    let Some((item, spec)) = playbook else {
        return Reflex::Wake("没有匹配的剧本".into());
    };
    if spec.tier.as_deref() == Some(TIER_APPROVE) {
        return Reflex::Wake(format!("剧本 {} 要求先确认", item.id));
    }
    let style = submit_style(sit);
    if style == SubmitStyle::ArrowSelect {
        return Reflex::Wake("方向键菜单不代答".into());
    }
    let mut chosen = DecisionAction::answer(spec.answer.clone(), spec.option_index.clone());
    chosen.submit = style;
    let Some(bytes) = chosen.send_bytes() else {
        return Reflex::Wake("剧本答案不能安全地写成按键".into());
    };
    let Some(keys) = to_keys(&bytes) else {
        return Reflex::Wake("剧本答案含特殊字符，交给模型".into());
    };
    let record = DecisionRecord {
        seq: 0,
        ts_ms: now_ms,
        contract_id: ctx.contract.id.clone(),
        console_id: ctx.contract.console_id.clone(),
        situation_signature: sit.signature.clone(),
        situation_hash: sit.screen_hash.clone(),
        ctx_refs: vec![CtxRef::of("playbook", &item.id, item.text.clone())],
        candidates: Vec::new(),
        chosen,
        rationale: format!("[reflex] 剧本 [P:{}] 命中，未调用模型", item.id),
        confidence: 1.0,
        tier: ctx.authority.tier.clone(),
        llm_used: false,
        outcome: "applied".to_string(),
        escalation: None,
        receipt: None,
    };
    match policy::evaluate(&ctx.authority, &record, sit, true) {
        PolicyVerdict::Auto | PolicyVerdict::Notify { .. } => {}
        v => return Reflex::Wake(format!("策略不放行代答：{}", v.as_str())),
    }
    match authorize(&ctx.policy(), &AgentAction::Keys { target: None, keys: keys.clone() }, Some(sit)) {
        Gate::Allow => Reflex::Answer { bytes, keys, record, playbook_id: item.id },
        g => Reflex::Wake(format!("托管闸门没有直接放行：{g:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::policy::Authority;
    use super::super::super::situation::{Choice, Program, SituationKind};
    use super::*;
    use serde_json::json;

    pub(crate) fn sit(kind: SituationKind, tail: &str, options: Vec<Choice>) -> Situation {
        Situation {
            frame_id: 3,
            screen_hash: "h".into(),
            program: Program::Unknown,
            kind,
            topic: None,
            options,
            confidence: 0.9,
            tail: tail.into(),
            evidence: vec![],
            stale_ms: 0,
            signature: "sig".into(),
        }
    }

    fn playbook(answer: &str, option: Option<&str>, tier: Option<&str>) -> (CtxItem, PlaybookSpec) {
        let spec = PlaybookSpec {
            match_program: None,
            match_kind: None,
            match_topic: None,
            match_text: Some("continue connecting".into()),
            answer: answer.into(),
            option_index: option.map(Into::into),
            tier: tier.map(Into::into),
            enabled: true,
        };
        let item = CtxItem {
            id: "pb-known-hosts".into(),
            kind: "playbook".into(),
            text: "首次连 ssh 的 known_hosts 提问答 yes".into(),
            strength: "soft".into(),
            scope: "global".into(),
            source: "user".into(),
            evidence: String::new(),
            extra: json!({}),
            contract_id: String::new(),
            position: 0,
            created_at: 0,
            updated_at: 0,
        };
        (item, spec)
    }

    fn ctx(tier: &str) -> HostCtx {
        let mut c = HostCtx::bare("c1", "t1", "部署");
        c.authority = Authority::default().with_tier(tier);
        c
    }

    const KNOWN_HOSTS: &str = "Are you sure you want to continue connecting (yes/no/[fingerprint])? ";

    #[test]
    fn answers_known_prompt_without_llm() {
        let s = sit(SituationKind::Question, KNOWN_HOSTS, vec![]);
        match decide(Some(playbook("yes", None, None)), &s, &ctx("auto"), 9) {
            Reflex::Answer { bytes, keys, record, playbook_id } => {
                assert_eq!(bytes, b"yes\r");
                assert_eq!(keys, "yes<Enter>");
                assert!(!record.llm_used);
                assert_eq!(playbook_id, "pb-known-hosts");
                assert_eq!(record.ctx_refs[0].source, "playbook");
            }
            r => panic!("应代答：{r:?}"),
        }
    }

    #[test]
    fn no_write_when_gate_says_approve() {
        let s = sit(SituationKind::Question, KNOWN_HOSTS, vec![]);
        assert!(matches!(decide(Some(playbook("yes", None, None)), &s, &ctx("approve"), 9), Reflex::Wake(_)));
        assert!(matches!(decide(Some(playbook("yes", None, Some("approve"))), &s, &ctx("auto"), 9), Reflex::Wake(_)));
        assert!(matches!(decide(None, &s, &ctx("auto"), 9), Reflex::Wake(_)));
    }

    #[test]
    fn never_answers_arrow_menus_or_passwords() {
        let opt = |i: &str, t: &str| Choice { index: Some(i.into()), text: t.into(), grants_permanent: false };
        // 方向键菜单：不代答（哪怕剧本给了编号）
        let mut menu = sit(SituationKind::Menu, "Select a model", vec![opt("1", "Sonnet"), opt("2", "Opus")]);
        menu.evidence.push(super::super::super::situation::SUBMIT_ARROW.to_string());
        assert_eq!(submit_style(&menu), SubmitStyle::ArrowSelect);
        assert!(matches!(
            decide(Some(playbook("x", Some("1"), None)), &menu, &ctx("auto"), 9),
            Reflex::Wake(ref w) if w.contains("方向键")
        ));
        // 同样的菜单换成按编号提交：只发数字、不加回车
        let mut digit = menu.clone();
        digit.evidence = vec![super::super::super::situation::SUBMIT_DIGIT.to_string()];
        if let Reflex::Answer { bytes, .. } = decide(Some(playbook("x", Some("1"), None)), &digit, &ctx("auto"), 9) {
            assert_eq!(bytes, b"1");
        }
        let pw = sit(SituationKind::Password, "[sudo] password for me: ", vec![]);
        assert!(matches!(decide(Some(playbook("hunter2", None, None)), &pw, &ctx("auto"), 9), Reflex::Wake(_)));
    }

    #[test]
    fn keys_conversion_refuses_ambiguous_payloads() {
        assert_eq!(to_keys(b"1"), Some("1".into()));
        assert_eq!(to_keys(b"a<b\r"), None);
        assert_eq!(to_keys(b"\r"), None);
    }
}
