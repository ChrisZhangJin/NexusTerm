//! 学习闭环的只读一侧：纠正去重、解释链、跨会话复盘、统一审计。
//!
//! 设计见 `docs/nexus-agent-steward-design.md` §4.4 / §8。
//!
//! 1. **纠正不无限堆积**：`should_learn` 做去重与冲突检测（`host_correct` 用）。
//! 2. **可解释**：`explain` 把 `ctxRefs` 展开成「引用了哪条、为什么这次自动放行」。
//! 3. **跨会话复盘**：`review` 按契约汇总决策结局。
//! 4. **统一审计**：`merge_audit` 把观察事件与决策记录合成一条时间线。
//!
//! 守护进程时代的 KB 聚合（`aggregate_kb`）与成功轨迹蒸馏（`distill`）只服务于已删除的
//! 零 LLM 决策编译器，PR8 一并删除。全部是纯函数；落库放在 `steward_cmd.rs` / `host_cmd.rs`。

use std::collections::HashMap;

use serde::Serialize;

use super::decision::{CtxRef, DecisionRecord};
use super::profile::CtxItem;
use super::steward_loop::StewardEvent;

fn truncate(s: &str, n: usize) -> String {
    let mut v: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        v.push('…');
    }
    v
}

// ─────────────────────────── 纠正学习 ───────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum LearnDecision {
    Create,
    Merge { id: String },
    Skip { reason: String },
}

/// 同类纠正不要无限堆积：同一 topic 已有实质相同的 Learned 偏好就跳过；
/// 有相似但更弱的条目则合并（由调用方更新 existing 的 evidence）。
pub fn should_learn(existing: &[CtxItem], topic: &str, correction: &str) -> LearnDecision {
    let c = correction.trim();
    if c.is_empty() || c.chars().count() < 3 {
        return LearnDecision::Skip {
            reason: "纠正内容太短，不值得沉淀".to_string(),
        };
    }
    let t = topic.to_lowercase();
    for it in existing {
        if it.source != "learned" {
            continue;
        }
        let same_text = it.text.to_lowercase() == c.to_lowercase();
        let same_topic = it.text.to_lowercase().contains(&t) || it.evidence.to_lowercase().contains(&t);
        if same_text {
            return LearnDecision::Skip {
                reason: format!("已有等价的 Learned 偏好 {}", it.id),
            };
        }
        if same_topic {
            // 同主题但措辞更完整：合并到已有条目，不新增一条重复偏好
            return LearnDecision::Merge { id: it.id.clone() };
        }
    }
    LearnDecision::Create
}

// ─────────────────────────── 解释链 ───────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplainedRef {
    pub source: String,
    pub id: String,
    pub snippet: String,
    pub label: String,
    /// 点回具体条目后的原文（若条目还在 Brain 里）
    pub resolved_text: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Explanation {
    pub seq: i64,
    pub tier: String,
    pub outcome: String,
    pub confidence: f32,
    pub rationale: String,
    pub refs: Vec<ExplainedRef>,
    /// 「为什么这次自动放行」的一句话答案
    pub why_auto: String,
    pub llm_used: bool,
}

const REF_LABELS: &[(&str, &str)] = &[
    ("profile", "偏好"),
    ("intent", "意向"),
    ("knowledge", "知识（不可信）"),
    ("playbook", "剧本"),
    ("authority", "授权契约"),
    ("policy", "安全策略"),
    ("history", "历史决策"),
];

/// 把一条决策的依据引用展开成人能核对的解释链。
pub fn explain(rec: &DecisionRecord, items: &[CtxItem]) -> Explanation {
    let refs: Vec<ExplainedRef> = rec
        .ctx_refs
        .iter()
        .map(|r| ExplainedRef {
            source: r.source.clone(),
            id: r.id.clone(),
            snippet: r.snippet.clone(),
            label: REF_LABELS
                .iter()
                .find(|(k, _)| *k == r.source)
                .map(|(_, v)| v.to_string())
                .unwrap_or_else(|| r.source.clone()),
            resolved_text: items
                .iter()
                .find(|i| i.id == r.id)
                .map(|i| i.text.clone()),
        })
        .collect();

    let policy_refs: Vec<&CtxRef> = rec
        .ctx_refs
        .iter()
        .filter(|r| r.source == "policy")
        .collect();
    let has_playbook = refs.iter().any(|r| r.source == "playbook");
    let has_intent = refs.iter().any(|r| r.source == "intent");
    let hard_count = refs.iter().filter(|r| r.source == "profile" && r.snippet.contains("硬约束")).count();

    let why = if rec.outcome == "rejected" {
        "没有自动放行：PolicyEngine 命中硬禁，只留下审计记录".to_string()
    } else if rec.outcome == "escalated" {
        "没有自动放行：档位或风险要求人确认，已给出建议、备选与倒计时默认动作".to_string()
    } else if rec.llm_used {
        "由 LLM Planner 兜底生成，但仍经 PolicyEngine 复核后才放行".to_string()
    } else {
        format!(
            "档位 {}；{}；{} 条硬约束、置信度 {:.2}；{}，因此按预置依据直接执行且全程零 LLM 调用",
            rec.tier,
            if has_playbook { "命中确定性剧本" } else if has_intent { "按意向栈编译完整回复" } else { "依据偏好与知识" },
            hard_count,
            rec.confidence,
            if policy_refs.is_empty() { "无策略降级" } else { "策略层已复核" }
        )
    };

    Explanation {
        seq: rec.seq,
        tier: rec.tier.clone(),
        outcome: rec.outcome.clone(),
        confidence: rec.confidence,
        rationale: rec.rationale.clone(),
        refs,
        why_auto: why,
        llm_used: rec.llm_used,
    }
}

// ─────────────────────── 复盘 / 统一审计 ───────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewReport {
    pub contract_id: String,
    pub total: usize,
    pub applied: usize,
    pub escalated: usize,
    pub rejected: usize,
    pub no_receipt: usize,
    pub injected: usize,
    pub top_refs: Vec<String>,
    pub done: Vec<String>,
    pub pending: Vec<String>,
    pub risks: Vec<String>,
}

/// 跨会话复盘：一份契约（或全部）做过什么、卡在哪、依据来自哪里。
pub fn review(contract_id: &str, decisions: &[DecisionRecord]) -> ReviewReport {
    let mut top: HashMap<String, usize> = HashMap::new();
    let mut done = Vec::new();
    let mut pending = Vec::new();
    let mut risks = Vec::new();
    let mut applied = 0;
    let mut escalated = 0;
    let mut rejected = 0;
    let mut no_receipt = 0;
    let mut injected = 0;
    for d in decisions {
        for r in &d.ctx_refs {
            *top.entry(r.source.clone()).or_default() += 1;
        }
        match d.outcome.as_str() {
            "applied" => {
                applied += 1;
                injected += 1;
                done.push(format!("#{} {} → {}", d.seq, d.situation_signature, truncate(&d.chosen.text, 60)));
            }
            "escalated" => {
                escalated += 1;
                pending.push(format!("#{} 待确认：{}", d.seq, d.rationale));
            }
            "rejected" => {
                rejected += 1;
                risks.push(format!("#{} 被用户接管或策略硬禁：{}", d.seq, d.rationale));
            }
            "no_receipt" => {
                no_receipt += 1;
                risks.push(format!("#{} 写后回执超时，未重发", d.seq));
            }
            _ => {}
        }
    }
    let mut refs: Vec<_> = top.into_iter().collect();
    refs.sort_by(|a, b| b.1.cmp(&a.1));
    ReviewReport {
        contract_id: contract_id.to_string(),
        total: decisions.len(),
        applied,
        escalated,
        rejected,
        no_receipt,
        injected,
        top_refs: refs.into_iter().map(|(k, n)| format!("{k} ×{n}")).collect(),
        done: done.into_iter().take(12).collect(),
        pending: pending.into_iter().take(8).collect(),
        risks: risks.into_iter().take(8).collect(),
    }
}

/// 统一审计视图：把 P1 观察事件与 P2 决策记录按时间合成一条流。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditEntry {
    pub ts_ms: i64,
    pub kind: String,
    pub text: String,
    pub seq: String,
    pub decision_seq: Option<i64>,
}

pub fn merge_audit(events: &[StewardEvent], decisions: &[DecisionRecord]) -> Vec<AuditEntry> {
    let mut v: Vec<AuditEntry> = Vec::new();
    for e in events {
        v.push(AuditEntry {
            ts_ms: e.ts_ms,
            kind: e.kind.as_str().to_string(),
            text: e.message.clone(),
            seq: format!("ev{}", e.seq),
            decision_seq: None,
        });
    }
    for d in decisions {
        v.push(AuditEntry {
            ts_ms: d.ts_ms,
            kind: "decision".to_string(),
            text: format!("[{}] {}", d.outcome, d.rationale),
            seq: format!("dec{}", d.seq),
            decision_seq: Some(d.seq),
        });
    }
    v.sort_by(|a, b| a.ts_ms.cmp(&b.ts_ms).then_with(|| a.seq.cmp(&b.seq)));
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::decision::DecisionAction;

    fn rec(seq: i64, sig: &str, outcome: &str, text: &str) -> DecisionRecord {
        DecisionRecord {
            seq,
            ts_ms: 1_000 + seq,
            contract_id: "c-1".into(),
            console_id: "tab-1".into(),
            situation_signature: sig.into(),
            situation_hash: "aabb".into(),
            ctx_refs: vec![
                CtxRef::of("intent", "i-1", "目标：修好构建"),
                CtxRef::of("profile", "p-1", "硬约束：最小改动"),
                CtxRef::of("policy", "policy:auto", "策略层已复核"),
                CtxRef::of("authority", "c-1", "授权档位 auto · host local:tab-1"),
            ],
            candidates: vec![],
            chosen: DecisionAction::answer(text.into(), None),
            rationale: "依据意向编译".into(),
            confidence: 0.91,
            tier: "auto".into(),
            llm_used: false,
            outcome: outcome.into(),
            escalation: None,
            receipt: Some("applied".into()),
        }
    }

    #[test]
    fn learned_preference_dedupes_and_merges() {
        let existing = vec![CtxItem {
            id: "l-1".into(), kind: "preference".into(), text: "别改设置".into(),
            strength: "soft".into(), scope: "global".into(), source: "learned".into(),
            evidence: "settings:remember-check / DecisionRecord #3".into(),
            extra: serde_json::json!({}), contract_id: "".into(), position: 0,
            created_at: 0, updated_at: 0,
        }];
        assert!(matches!(should_learn(&existing, "settings", "别改设置"), LearnDecision::Skip { .. }));
        assert!(matches!(should_learn(&existing, "settings", "别改设置，而且不要动签名"), LearnDecision::Merge { .. }));
        assert_eq!(should_learn(&existing, "deploy", "部署前先跑测试"), LearnDecision::Create);
        assert!(matches!(should_learn(&existing, "deploy", "ok"), LearnDecision::Skip { .. }));
    }

    #[test]
    fn explanation_answers_why_auto() {
        let e = explain(&rec(9, "sig", "applied", "No"), &[]);
        assert!(e.why_auto.contains("零 LLM"));
        assert!(e.why_auto.contains("意向"));
        // rec() 与真实 compile 一样带一条 authority 引用（蒸馏靠它取主机）
        assert_eq!(e.refs.len(), 4);
        assert_eq!(e.refs[0].label, "意向");
        assert!(!e.llm_used);
        let denied = explain(&rec(10, "sig", "rejected", "x"), &[]);
        assert!(denied.why_auto.contains("没有自动放行"));
    }

    #[test]
    fn review_counts_outcomes_and_lists_pending() {
        let d = vec![
            rec(1, "a", "applied", "x"), rec(2, "b", "escalated", "y"),
            rec(3, "c", "no_receipt", "z"), rec(4, "d", "rejected", "w"),
        ];
        let r = review("c-1", &d);
        assert_eq!((r.total, r.applied, r.escalated, r.rejected, r.no_receipt), (4, 1, 1, 1, 1));
        assert_eq!(r.injected, 1);
        assert_eq!(r.pending.len(), 1);
        assert_eq!(r.risks.len(), 2);
        assert!(r.top_refs.iter().any(|s| s.starts_with("intent")));
    }

    #[test]
    fn merge_audit_sorts_by_time() {
        let e = vec![StewardEvent {
            seq: 1, ts_ms: 2_000, console_id: "tab-1".into(), contract_id: "c-1".into(),
            kind: super::super::steward_loop::StewardEventKind::SituationChanged,
            message: "局面变化".into(), suggestion: None, situation: None,
            frame_id: 1, screen_hash: "h".into(),
        }];
        let v = merge_audit(&e, &[rec(5, "sig", "applied", "x")]);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].ts_ms, 1_005);
        assert_eq!(v[0].kind, "decision");
        assert_eq!(v[1].kind, "situation_changed");
    }
}
