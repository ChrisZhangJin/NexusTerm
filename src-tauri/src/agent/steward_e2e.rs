//! NexusAgent P0–P5 端到端（纯逻辑）测试。
//!
//! 不依赖 GUI / PTY / LLM：帧采集（`ScreenStore::apply`）→ 局面分类（`situation::classify`）
//! → 意向装载（`StewardBrain`）→ 决策编译（`decision::compile`，零 LLM）→ 三档授权
//! （`policy::authorize`）→ 注入字节（`send_bytes`）→ 写后回执（`verify_receipt`，绝不重发）
//! → 撤销窗口 → Checkpoint 落盘 / 强杀恢复 → 时间盒 → 预算 → 纠正学习 → 复盘 / 解释链 / 蒸馏。
//!
//! 这条链就是常驻循环 `steward_loop::spawn_daemon` 每轮真正走的路径（`build_context`
//! 直接复用循环的 pub(crate) 实现），测试只是把 `inject_keys`（需要真实 PTY）替换成
//! 直接检查 `send_bytes` 的字节，以及把「等回执」换成立即喂下一帧。

use serde_json::json;

use super::budget::BudgetCheck;
use super::checkpoint::{
    resume_verdict, ResumeVerdict, Spent, StewardCheckpoint, TimeboxConfig, TimeboxHit,
    TimeboxState,
};
use super::decision::{self, DecisionAction, PendingReceipt, ReceiptState};
use super::learning;
use super::policy::{self, Authority, PolicyVerdict, UndoEntry};
use super::profile::{CtxInput, StewardBrain, KIND_INTENT};
use super::situation;
use super::steward::{ScreenDelta, ScreenLine, ScreenStore, StewardRegistry};
use super::steward_core::StewardCore;
use super::steward_loop::build_context;

const NOW: i64 = 1_730_000_000_000;

/// 构造一帧全量 `ScreenDelta`（hash 用与后端一致的 `frame_hash` 现算）。
fn frame(console: &str, frame_id: u64, lines: &[&str], alt: bool, ts: i64) -> ScreenDelta {
    let mut full: Vec<String> = vec![String::new(); 24];
    for (i, l) in lines.iter().enumerate() {
        full[i] = l.to_string();
    }
    let hash = super::steward::frame_hash(&full, alt, 80, 24, 0, 0);
    ScreenDelta {
        console_id: console.to_string(),
        frame_id,
        window_start: 0,
        total: 24,
        cols: 80,
        rows: 24,
        alt,
        cursor_x: 0,
        cursor_y: 0,
        hash,
        full: true,
        lines: full
            .into_iter()
            .enumerate()
            .map(|(y, text)| ScreenLine {
                y: y as u32,
                text,
            })
            .collect(),
        ts_ms: ts,
    }
}

/// 压入一条带 answerTemplate 的意向（用户在托管面板做的事）。
fn push_intent(brain: &StewardBrain, contract_id: &str, goal: &str) {
    brain
        .upsert(CtxInput {
            id: None,
            kind: KIND_INTENT.to_string(),
            text: goal.to_string(),
            strength: None,
            scope: None,
            source: Some("user".to_string()),
            evidence: Some("e2e".to_string()),
            extra: Some(json!({
                "successCriteria": [],
                "constraints": ["不要改设置 UI", "保持最小改动"],
                "answerTemplate": "Yes：按目标 {goal} 推进（约束：{constraints}）",
                "status": "active",
                "priority": 0
            })),
            contract_id: Some(contract_id.to_string()),
            position: Some(0),
        })
        .unwrap();
}

// ───────────────────── 场景 A：claude code 提问 → 自动应答闭环 ─────────────────────

#[test]
fn e2e_a_question_screen_full_loop_with_receipt() {
    // 1. 帧采集：前端全量帧落地（哈希校验过）
    let screen = ScreenStore::new();
    let ack = screen.apply(&frame(
        "tab-1",
        1,
        &[
            "╭────────────────────────────────────╮",
            "│ Do you want me to change the setting about whether to remember this? │",
            "│ 1. Yes                            │",
            "│ 2. No, keep as is                 │",
            "╰────────────────────────────────────╯",
            "  ? for shortcuts · esc to cancel",
        ],
        true,
        NOW,
    ));
    assert!(ack.applied, "全量帧必须落地");
    assert!(!ack.need_full);

    // 2. 局面分类
    let snap = screen.get("tab-1").expect("快照在");
    let sit = situation::classify(&snap, NOW + 100, 6_000);
    assert_eq!(sit.program, situation::Program::ClaudeCode);
    assert_eq!(sit.kind, situation::SituationKind::Question);
    assert_eq!(sit.options.len(), 2);
    assert!(sit.confidence >= 0.8, "置信度 {sit:?}");

    // 3. 签契约 + 装载意向
    let registry = StewardRegistry::new();
    let contract = registry.start("tab-1", "local", "把 remember 检查关掉，最小改动", "notify");
    let brain = StewardBrain::new();
    push_intent(&brain, &contract.id, "把 remember 检查关掉");

    // 4. 决策编译（零 LLM）
    let ctx = build_context(&contract, &sit, &brain, NOW + 200);
    assert_eq!(ctx.goal(), "把 remember 检查关掉");
    let mut rec = decision::compile(&ctx);
    assert_eq!(rec.chosen.kind, "answer");
    assert!(!rec.llm_used, "P2 硬保证：编译不走模型");
    assert!(rec.ctx_refs.iter().any(|r| r.source == "intent"), "必须引用意向");

    // 5. Notify 档授权：放行 + 撤销窗口
    let auth = Authority::default().with_tier("notify");
    let verdict = policy::authorize(&auth, &mut rec, &sit, true);
    assert!(
        matches!(verdict, PolicyVerdict::Notify { .. }),
        "notify 档应放行并留撤销窗口，实际 {verdict:?}"
    );

    // 6. 注入字节：单行 + \r
    let bytes = rec.chosen.send_bytes().expect("answer 必须可注入");
    assert_eq!(*bytes.last().unwrap(), b'\r');
    assert!(!bytes[..bytes.len() - 1].iter().any(|b| *b == b'\n'));

    // 7. 撤销窗口（Notify 档 30s）
    let core = StewardCore::new();
    if let PolicyVerdict::Notify { undo_ms } = verdict {
        core.add_undo(UndoEntry::new(&rec, undo_ms));
    }
    assert_eq!(core.live_undos(NOW + 1_000).len(), 1);
    assert!(core.live_undos(NOW + 60_000).is_empty(), "窗口过期自动消失");

    // 8. 写后回执：下一帧带着回显 → Applied；绝不重发
    let echo: String = rec.chosen.text.chars().take(24).collect();
    let ack2 = screen.apply(&frame(
        "tab-1",
        2,
        &[
            "╭────────────────────────────────────╮",
            "│ accepted: settings updated         │",
            &format!("│ {echo}").leak() as &str,
        ],
        true,
        NOW + 400,
    ));
    assert!(ack2.applied && ack2.changed, "回执帧必须落地且内容变化");
    let pending = PendingReceipt {
        decision_seq: 1,
        console_id: "tab-1".to_string(),
        contract_id: contract.id.clone(),
        before_frame: 1,
        before_hash: snap.hash.clone(),
        echo,
        injected_at: NOW + 300,
        deadline_ms: NOW + 300 + decision::RECEIPT_WINDOW_MS,
    };
    let snap2 = screen.get("tab-1").unwrap();
    let (state, msg) = decision::verify_receipt(&pending, Some(&snap2), NOW + 400);
    assert_eq!(state, ReceiptState::Applied);
    assert!(msg.contains("回显命中"), "回执文本要能解释命中：{msg}");

    // 9. 决策闭环：outcome=applied，可复盘、可解释
    rec.outcome = "applied".to_string();
    rec.receipt = Some(msg);
    let report = learning::review("*", std::slice::from_ref(&rec));
    assert_eq!(report.total, 1);
    assert_eq!(report.applied, 1);
    let exp = learning::explain(&rec, &brain.view().items);
    assert_eq!(exp.seq, rec.seq);
    assert!(!exp.why_auto.is_empty(), "解释链要给出「为什么自动放行」");

    // 10. 蒸馏：同签名两次 applied 且回复一致 → 剧本候选
    let mut second = rec.clone();
    second.seq = 2;
    let cands = learning::distill(&[rec.clone(), second]);
    assert_eq!(cands.len(), 1, "成功轨迹应沉淀出剧本候选");
    assert!(cands[0].occurrences >= 2);
}

// ───────────────────── 场景 B：永久扩权选项 fail-closed ─────────────────────

#[test]
fn e2e_b_permanent_grant_option_is_never_auto_selected() {
    let screen = ScreenStore::new();
    screen.apply(&frame(
        "tab-2",
        1,
        &[
            "Do you want to proceed?",
            "❯ 1. Yes",
            "  2. Yes, and don't ask again",
            "  3. No",
            "  Esc to cancel",
        ],
        true,
        NOW,
    ));
    let snap = screen.get("tab-2").unwrap();
    let sit = situation::classify(&snap, NOW + 100, 6_000);
    assert_eq!(sit.kind, situation::SituationKind::Permission);
    assert!(sit.options.iter().any(|o| o.grants_permanent));

    let registry = StewardRegistry::new();
    let contract = registry.start("tab-2", "local", "跑测试", "approve");
    let brain = StewardBrain::new();
    push_intent(&brain, &contract.id, "跑测试");

    let ctx = build_context(&contract, &sit, &brain, NOW + 200);
    let mut rec = decision::compile(&ctx);
    // 编译层：只挑「本次允许」，绝不选永久项
    assert!(
        rec.chosen.text.contains("本次允许"),
        "默认动作应是本次允许：{}",
        rec.chosen.text
    );
    assert_eq!(rec.outcome, "escalated");
    assert!(rec.escalation.is_some(), "必须给升级卡等人裁决");

    // 授权层（即便有人把档位改回 notify）：永久扩权仍不放行
    let auth = Authority::default().with_tier("notify");
    let permanent = DecisionAction::answer("2. Yes, and don't ask again".to_string(), Some("2".to_string()));
    let mut forged = rec.clone();
    forged.chosen = permanent;
    let verdict = policy::authorize(&auth, &mut forged, &sit, true);
    assert!(
        matches!(verdict, PolicyVerdict::Deny { .. }),
        "自动选永久项必须被硬禁，实际 {verdict:?}"
    );
}

// ───────────────────── 场景 C：密码屏 → secret 通道，绝不代填 ─────────────────────

#[test]
fn e2e_c_password_prompt_is_secret_channel_only() {
    let screen = ScreenStore::new();
    screen.apply(&frame(
        "tab-3",
        1,
        &["[sudo] password for chris:"],
        false,
        NOW,
    ));
    let snap = screen.get("tab-3").unwrap();
    let sit = situation::classify(&snap, NOW + 100, 6_000);
    assert_eq!(sit.kind, situation::SituationKind::Password);
    assert_eq!(sit.tail, "[redacted: credential prompt]");

    let registry = StewardRegistry::new();
    let contract = registry.start("tab-3", "local", "安装依赖", "auto");
    let brain = StewardBrain::new();
    push_intent(&brain, &contract.id, "安装依赖");

    let ctx = build_context(&contract, &sit, &brain, NOW + 200);
    let mut rec = decision::compile(&ctx);
    assert_eq!(rec.chosen.kind, "secret", "密码局面只走 secret 通道");
    assert!(rec.chosen.send_bytes().is_none(), "secret 不可注入");

    // 即便 auto 档：密码被策略层硬禁
    let auth = Authority::default().with_tier("auto");
    let verdict = policy::authorize(&auth, &mut rec, &sit, true);
    assert!(matches!(verdict, PolicyVerdict::Deny { .. }), "{verdict:?}");
}

// ───────────────────── 断流 / 低置信度：fail-closed 只观察 ─────────────────────

#[test]
fn e2e_stale_screen_downgrades_to_observe() {
    let screen = ScreenStore::new();
    screen.apply(&frame("tab-4", 1, &["$ cargo test", "running..."], false, NOW));
    // 60s 后才看这帧：远超 6s 断流阈值
    let snap = screen.get("tab-4").unwrap();
    let sit = situation::classify(&snap, NOW + 60_000, 6_000);
    assert!(sit.stale_ms > 6_000);

    let registry = StewardRegistry::new();
    let contract = registry.start("tab-4", "local", "跑测试", "auto");
    let brain = StewardBrain::new();
    push_intent(&brain, &contract.id, "跑测试");

    let ctx = build_context(&contract, &sit, &brain, NOW + 60_100);
    let mut rec = decision::compile(&ctx);
    assert_eq!(rec.chosen.kind, "observe", "断流现场只能观察");
    assert_eq!(rec.outcome, "observed");

    let auth = Authority::default().with_tier("auto");
    let verdict = policy::authorize(&auth, &mut rec, &sit, true);
    assert!(
        matches!(verdict, PolicyVerdict::Approve { .. }),
        "策略层再拦一道：{verdict:?}"
    );
}

// ───────────────────── 场景 E：夺权 / 交还 / 撤销学习 ─────────────────────

#[test]
fn e2e_e_takeover_handback_and_undo_learns() {
    let core = StewardCore::new();
    core.ensure_console("tab-5");
    assert!(core.ownership().can_write("tab-5"), "初始 Steward 驾驶");

    // 用户敲键 → 100ms SLA 夺权
    let latency = core.ownership().user_key("tab-5", NOW, NOW + 40);
    assert!(latency >= 0 && latency <= 100, "夺权延迟 {latency}ms");
    assert!(!core.ownership().can_write("tab-5"), "观察态绝不写终端");

    // 离开期间：观察与决策都进 AbsenceNote
    core.ownership().note("tab-5", "decision", "#3 观察态决策：等待用户", NOW + 50);
    core.ownership().note("tab-5", "escalation", "#4 待确认", NOW + 100);

    // 交还：摘要 + 恢复驾驶
    let summary = core.ownership().handback("tab-5", NOW + 500, true);
    assert_eq!(summary.takeover_count, 1);
    // duration 从接管时刻起算（NOW+40 → NOW+500），40ms 夺权处理也在「离开」里
    assert!(
        (400..=500).contains(&summary.duration_ms),
        "离开时长 {}",
        summary.duration_ms
    );
    assert!(summary.done.iter().any(|d| d.contains("决策")));
    assert!(summary.pending.iter().any(|p| p.contains("待确认")));
    assert!(!summary.next_step.is_empty());
    assert!(summary.screen_aligned);
    assert!(core.ownership().can_write("tab-5"), "交还后恢复驾驶");

    // 撤销 → 写 Learned 偏好，同类动作下次不再自动放行
    let brain = StewardBrain::new();
    let item = brain
        .learn_correction(9, "undo", "不要自动应答：Yes：按目标推进")
        .expect("写回 Learned 偏好");
    assert_eq!(item.source, "learned");
    let decision = learning::should_learn(&brain.view().items, "undo", "不要自动应答：Yes：按目标推进");
    assert!(
        !matches!(decision, learning::LearnDecision::Create { .. }),
        "已学过的纠正不重复新增（Skip 或 Merge 到已有条目）：{decision:?}"
    );
}

// ───────────────────── 场景 D：Checkpoint 落盘 → 强杀恢复 ─────────────────────

#[test]
fn e2e_d_checkpoint_resume_verdicts() {
    let auth = Authority::default().with_tier("notify");
    let mut cp = StewardCheckpoint::new("c-1", "tab-6", "目标", &auth);
    cp.screen_hash = "h1".to_string();
    cp.state = "observing".to_string();
    cp.saved_wall_ms = NOW;
    cp.push_decision(7);
    cp.push_decision(8);

    // 现场一致 → 直接恢复
    assert!(matches!(
        resume_verdict(&cp, Some("h1"), NOW + 1_000),
        ResumeVerdict::Clean
    ));
    // 屏幕漂移 → 降「确认」档
    assert!(matches!(
        resume_verdict(&cp, Some("zz"), NOW + 1_000),
        ResumeVerdict::Drift { .. }
    ));
    // 已完成 → 不自动恢复
    let mut done = cp.clone();
    done.state = "completed".to_string();
    assert!(matches!(
        resume_verdict(&done, Some("h1"), NOW + 1_000),
        ResumeVerdict::Expired { .. }
    ));
    // 断连挂起 → 等用户确认
    let mut sus = cp.clone();
    sus.state = "suspended".to_string();
    assert!(matches!(
        resume_verdict(&sus, Some("h1"), NOW + 1_000),
        ResumeVerdict::Suspended { .. }
    ));
    // 超过 24h TTL → 过期
    assert!(matches!(
        resume_verdict(&cp, Some("h1"), NOW + 25 * 3_600_000),
        ResumeVerdict::Expired { .. }
    ));

    // 落库-重建往返：StewardCore 保存 + Checkpoint 状态取回
    let core = StewardCore::new();
    core.set_checkpoint(cp.clone());
    assert_eq!(core.checkpoint("c-1").unwrap().screen_hash, "h1");
    assert_eq!(core.checkpoint("c-1").unwrap().last_decisions, vec![7, 8]);
    core.drop_contract("c-1", "tab-6");
    assert!(core.checkpoint("c-1").is_none());
}

// ───────────────────── 时间盒六档：触发与复位 ─────────────────────

#[test]
fn e2e_timebox_six_levels_fire_and_reset() {
    let cfg = TimeboxConfig {
        screen_ack_ms: 3_000,
        receipt_ms: 1_500,
        step_ms: 60_000,
        idle_ms: 300_000,
        no_progress_ms: 900_000,
        deadline_ms: 60_000,
        grace_ms: 5_000,
    };
    let mut tb = TimeboxState::start(0);

    // 3s 没有新帧 → 读屏回执档
    assert!(tb.check(3_001, &cfg).contains(&TimeboxHit::ScreenAck));
    // 同档只报一次；mark_frame 复位
    assert!(!tb.check(3_002, &cfg).contains(&TimeboxHit::ScreenAck));
    tb.mark_frame(3_500);
    assert!(!tb.check(3_600, &cfg).contains(&TimeboxHit::ScreenAck));

    // 60s 无闭环 → 单步档 + 意向 deadline
    let hits = tb.check(60_000, &cfg);
    assert!(hits.contains(&TimeboxHit::Step));
    assert!(hits.contains(&TimeboxHit::Deadline));
    assert!(!hits.contains(&TimeboxHit::Grace), "宽限还没到");
    // 65s 后 → 宽限档（优雅收尾，不硬杀）
    assert!(tb.check(65_001, &cfg).contains(&TimeboxHit::Grace));

    // 闭环 mark_step 复位单步与回执档
    let mut tb2 = TimeboxState::start(0);
    assert!(tb2.check(60_001, &cfg).contains(&TimeboxHit::Step));
    tb2.mark_step(60_100);
    assert!(!tb2.check(60_200, &cfg).contains(&TimeboxHit::Step));

    // 局面有实质变化 → 空转 / 无进展复位
    let mut tb3 = TimeboxState::start(0);
    assert!(tb3.check(900_001, &cfg).contains(&TimeboxHit::NoProgress));
    tb3.mark_progress(900_100);
    assert!(!tb3.check(900_200, &cfg).contains(&TimeboxHit::NoProgress));
}

// ───────────────────── 预算耗尽：优雅交还而非硬杀 ─────────────────────

#[test]
fn e2e_budget_exhaustion_is_graceful() {
    let auth = Authority {
        tier: "auto".to_string(),
        max_steps: 3,
        max_minutes: 30,
        max_output_chars: 4_096,
        ..Authority::default()
    };
    let mut spent = Spent::default();
    for _ in 0..3 {
        spent.bump_step();
    }
    assert_eq!(spent.steps, 3);
    assert!(matches!(spent.check(&auth), BudgetCheck::Exhausted(_)));
    spent.bump_injection(8_192);
    assert!(spent.output_chars >= 8_192);
}

// ───────────────────── Planner 兜底：默认关，只兜 observe ─────────────────────

#[test]
fn e2e_planner_gate_only_for_observe_fallback() {
    // 用会编译出 answer 的提问屏：规则已给出确定性回复 → 不需要模型
    let screen = ScreenStore::new();
    screen.apply(&frame(
        "tab-7",
        1,
        &[
            "╭────────────────────────────────────╮",
            "│ Do you want me to change the setting about whether to remember this? │",
            "│ 1. Yes                            │",
            "│ 2. No, keep as is                 │",
            "╰────────────────────────────────────╯",
            "  ? for shortcuts · esc to cancel",
        ],
        true,
        NOW,
    ));
    let snap = screen.get("tab-7").unwrap();
    let sit = situation::classify(&snap, NOW + 100, 6_000);
    assert_eq!(sit.kind, situation::SituationKind::Question);

    let registry = StewardRegistry::new();
    let contract = registry.start("tab-7", "local", "跑通 e2e", "notify");
    let brain = StewardBrain::new();
    push_intent(&brain, &contract.id, "跑通 e2e");
    let ctx = build_context(&contract, &sit, &brain, NOW + 200);
    let answer_rec = decision::compile(&ctx);
    assert_eq!(answer_rec.chosen.kind, "answer");

    // 规则已给出可注入回复 → 不需要模型
    let gate = super::planner::PlannerGate::new();
    assert!(!gate.enabled(), "Planner 默认关且不持久化");
    assert!(!super::planner::should_plan(&gate, &answer_rec));
    gate.set(true);
    assert!(!super::planner::should_plan(&gate, &answer_rec), "有确定性回复就不调模型");

    // 只有规则编不出回复（observe / wait）才兜底
    let mut obs = answer_rec.clone();
    obs.chosen = DecisionAction::observe("现场不足以托付写入");
    obs.outcome = "observed".to_string();
    assert!(super::planner::should_plan(&gate, &obs));
}
