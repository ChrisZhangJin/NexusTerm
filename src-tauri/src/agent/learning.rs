//! P5 学习闭环：KB 聚合、成功轨迹沉淀、纠正去重、解释链、跨会话复盘、统一审计。
//!
//! 设计见 `docs/nexus-agent-steward-design.md` §4.4 / §8。P2 已经把「器官」做齐
//! （`learn_correction` / `CtxRef` / `DecisionRecord`），P5 补的是把它们串成闭环：
//!
//! 1. **KB 不再只有手填条目**：`aggregate_kb` 把主机记忆、技能、历史 RunSummary
//!    一起纳入检索，全部标为不可信数据，只能当参考，不能改写 Profile / Authority。
//! 2. **成功轨迹沉淀**：同一局面被同一条回复成功处理两次 → `distill` 产出 Playbook 候选，
//!    下次同类提问直接命中确定性剧本，不再询问、也不调模型。
//! 3. **纠正不无限堆积**：`should_learn` 做去重与冲突检测。
//! 4. **可解释**：`explain` 把 `ctxRefs` 展开成「引用了哪条、为什么这次自动放行」。
//! 5. **统一审计**：`merge_audit` 把观察事件与决策记录合成一条时间线。
//!
//! 全部是纯函数；落库 / 写时间线 / LLM 调用放在 `steward_cmd.rs` 与 `planner.rs`。

use std::collections::HashMap;

use serde::Serialize;

use super::decision::{CtxRef, DecisionRecord};
use super::hostmem::HostNote;
use super::history::RunSummary;
use super::profile::{CtxInput, CtxItem, KnowledgeHit, PlaybookSpec, KIND_PLAYBOOK};
use super::situation::Situation;
use super::skill::Skill;
use super::steward_loop::StewardEvent;

/// 蒸馏一条成功轨迹至少需要重复几次。2 次是刻意保守：一次可能是碰巧。
pub const DISTILL_MIN_OCCURRENCES: usize = 2;

// ─────────────────────────── KB 聚合 ───────────────────────────

fn keywords(sit: &Situation) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(t) = &sit.topic {
        v.extend(t.split([':', '-', '_', '/']).map(|s| s.to_lowercase()).filter(|s| s.len() >= 3));
    }
    for w in sit.tail.to_lowercase().split_whitespace() {
        let w = w.trim_matches(|c: char| !c.is_alphanumeric());
        if w.len() >= 5 && v.len() < 24 {
            v.push(w.to_string());
        }
    }
    v
}

fn score_text(text: &str, kws: &[String]) -> i32 {
    let t = text.to_lowercase();
    kws.iter().filter(|k| t.contains(k.as_str())).count() as i32
}

/// 把三类既有资产聚进 KB。`base` 是 `StewardBrain::retrieve_kb` 的结果（优先级最高）。
/// 所有新增条目 `untrusted=true`，id 带来源前缀，点开可回溯。
pub fn aggregate_kb(
    base: Vec<KnowledgeHit>,
    host_notes: &[HostNote],
    skills: &[Skill],
    summaries: &[RunSummary],
    sit: &Situation,
    limit: usize,
) -> Vec<KnowledgeHit> {
    let kws = keywords(sit);
    let mut out = base;
    for n in host_notes {
        let s = score_text(&n.text, &kws);
        if s > 0 {
            out.push(KnowledgeHit {
                id: format!("hostmem:{}", n.id),
                text: format!("[主机记忆] {}", n.text),
                score: s + 1,
                untrusted: true,
            });
        }
    }
    for sk in skills {
        let s = score_text(&format!("{} {}", sk.name, sk.description), &kws);
        if s > 0 {
            out.push(KnowledgeHit {
                id: format!("skill:{}", sk.id),
                text: format!("[技能] {} — {}", sk.name, sk.description),
                score: s,
                untrusted: true,
            });
        }
    }
    for (i, r) in summaries.iter().enumerate() {
        let s = score_text(&format!("{} {} {}", r.goal, r.outcome, r.final_state), &kws);
        if s > 0 {
            out.push(KnowledgeHit {
                id: format!("run:{i}:{}", r.ts),
                text: format!("[历史运行] 目标「{}」结局「{}」", r.goal, r.outcome),
                score: s,
                untrusted: true,
            });
        }
    }
    out.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
    out.truncate(limit.max(1).min(32));
    out
}

// ─────────────────────── 成功轨迹 → 剧本 ───────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybookCandidate {
    pub id: String,
    pub signature: String,
    pub occurrences: usize,
    pub answer: String,
    pub option_index: Option<String>,
    pub match_program: Option<String>,
    pub match_kind: Option<String>,
    pub match_topic: Option<String>,
    pub input: CtxInput,
}

/// 从决策历史里蒸馏确定性剧本。只有 `applied`（有写后回执）且两次以上、
/// 且回复文本完全一致才生成候选——避免把一次侥幸或互相矛盾的回答固化下来。
pub fn distill(decisions: &[DecisionRecord]) -> Vec<PlaybookCandidate> {
    #[derive(Default)]
    struct Group {
        n: usize,
        text: String,
        option_index: Option<String>,
        sig_parts: Vec<String>,
        evidence: String,
    }
    let mut groups: HashMap<String, Group> = HashMap::new();
    for d in decisions {
        if d.outcome != "applied" || d.llm_used || !d.chosen.injectable {
            continue;
        }
        if matches!(d.chosen.kind.as_str(), "observe" | "wait" | "secret" | "handoff") {
            continue;
        }
        let g = groups.entry(d.situation_signature.clone()).or_default();
        if g.n == 0 {
            g.text = d.chosen.text.clone();
            g.option_index = d.chosen.option_index.clone();
            g.sig_parts = d
                .situation_signature
                .split('|')
                .map(|s| s.to_string())
                .collect();
            g.evidence = format!("DecisionRecord #{}", d.seq);
        }
        // 同签名但回复不一致：这一组作废（用空 text 标记）
        if g.text != d.chosen.text {
            g.text.clear();
        }
        g.n += 1;
    }
    let mut out: Vec<PlaybookCandidate> = groups
        .into_iter()
        .filter(|(_, g)| g.n >= DISTILL_MIN_OCCURRENCES && !g.text.is_empty())
        .map(|(sig, g)| {
            let spec = PlaybookSpec {
                match_program: g.sig_parts.first().cloned(),
                match_kind: g.sig_parts.get(1).cloned(),
                match_topic: g.sig_parts.get(2).cloned(),
                match_text: None,
                answer: g.text.clone(),
                option_index: g.option_index.clone(),
                tier: Some("auto".to_string()),
                enabled: true,
            };
            let id = format!("distilled-{}", short_hash(&sig));
            PlaybookCandidate {
                id: id.clone(),
                signature: sig.clone(),
                occurrences: g.n,
                answer: g.text.clone(),
                option_index: g.option_index.clone(),
                match_program: spec.match_program.clone(),
                match_kind: spec.match_kind.clone(),
                match_topic: spec.match_topic.clone(),
                input: CtxInput {
                    id: Some(id),
                    kind: KIND_PLAYBOOK.to_string(),
                    text: format!("自动沉淀：{} → {}", sig.split('|').take(2).collect::<Vec<_>>().join("/"), truncate(&g.text, 60)),
                    strength: None,
                    scope: Some("global".to_string()),
                    source: Some("distilled".to_string()),
                    evidence: Some(g.evidence.clone()),
                    extra: Some(serde_json::to_value(spec).unwrap_or_default()),
                    contract_id: None,
                    position: None,
                },
            }
        })
        .collect();
    out.sort_by(|a, b| b.occurrences.cmp(&a.occurrences).then_with(|| a.id.cmp(&b.id)));
    out
}

fn short_hash(s: &str) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{h:08x}")
}

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
            decision_seq: e.decision.as_ref().map(|d| d.seq),
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
    use super::super::situation::{Program, SituationKind};

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

    fn sit() -> Situation {
        Situation {
            frame_id: 1,
            screen_hash: "aabb".into(),
            program: Program::ClaudeCode,
            kind: SituationKind::Question,
            topic: Some("settings:remember-check".into()),
            options: vec![],
            confidence: 0.93,
            tail: "Do you want to change the setting remember check?".into(),
            evidence: vec![],
            stale_ms: 0,
            signature: "claude_code|question|settings:remember-check".into(),
        }
    }

    #[test]
    fn kb_aggregates_three_sources_and_marks_untrusted() {
        let notes = vec![HostNote {
            id: "h1".into(), host_key: "local:tab-1".into(),
            text: "remember-check 与钥匙串读取有关".into(),
            kind: super::super::hostmem::Kind::Quirk,
            source: super::super::hostmem::Source::User,
            created_at: 0, updated_at: 0,
        }];
        let runs = vec![RunSummary {
            ts: 1, goal: "修 remember-check 设置".into(),
            actions: vec![], outcome: "已修好".into(), final_state: "shell".into(),
        }];
        let hits = aggregate_kb(vec![], &notes, &[], &runs, &sit(), 8);
        assert!(hits.len() >= 2);
        assert!(hits.iter().all(|h| h.untrusted));
        assert!(hits.iter().any(|h| h.id.starts_with("hostmem:")));
        assert!(hits.iter().any(|h| h.id.starts_with("run:")));
    }

    #[test]
    fn distill_requires_two_identical_successes() {
        let one = vec![rec(1, "claude_code|question|settings", "applied", "No — 不改设置")];
        assert!(distill(&one).is_empty());
        let two = vec![
            rec(1, "claude_code|question|settings", "applied", "No — 不改设置"),
            rec(2, "claude_code|question|settings", "applied", "No — 不改设置"),
        ];
        let c = distill(&two);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].occurrences, 2);
        assert_eq!(c[0].match_kind.as_deref(), Some("question"));
        assert_eq!(c[0].input.kind, "playbook");
        assert_eq!(c[0].input.source.as_deref(), Some("distilled"));
    }

    #[test]
    fn distill_rejects_conflicting_or_unconfirmed() {
        let v = vec![
            rec(1, "sig", "applied", "A"),
            rec(2, "sig", "applied", "B"),
            rec(3, "other", "escalated", "C"),
            rec(4, "other2", "no_receipt", "D"),
        ];
        assert!(distill(&v).is_empty());
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
        assert_eq!(e.refs.len(), 3);
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
            frame_id: 1, screen_hash: "h".into(), llm_calls: 0, decision: None,
        }];
        let v = merge_audit(&e, &[rec(5, "sig", "applied", "x")]);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].ts_ms, 1_005);
        assert_eq!(v[0].kind, "decision");
        assert_eq!(v[1].kind, "situation_changed");
    }
}
