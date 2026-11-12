//! 统一托管的端到端（纯逻辑）场景 A–E。
//!
//! 不依赖 GUI / PTY / LLM：帧采集（`ScreenStore::apply`）→ 局面分类（`situation::classify`）
//! → 观察器（`steward_loop::StewardState::step`，只出建议、没有写路径）→ 托管任务内部的
//! 反射快路径（`profile::match_playbook` → `host::reflex::decide`，里面过 `policy::evaluate`
//! 与 `gate::authorize`）→ 模型路径的托管闸门（`gate::authorize`）→ 相位机（`HostShared`
//! + 假 IO）→ 现场落盘 / 重启扫描 / 接驳判定（`HostCheckpoint` + `host_cmd::scan_rows` /
//! `reattach_plan`）→ 纠正学习。
//!
//! `act::run` 本身要 AppHandle（真 PTY / Tauri 事件），这里把它在托管模式下真正调用的
//! 纯函数与 `HostIo` 接缝串起来；写入只检查要发的字节，不真的写终端。
//!
//! 旧版（守护进程零 LLM 决策 → 注入 → 回执 → 撤销窗口）的场景随那条写路径在 PR8 一起删除。

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use super::act::AgentAction;
use super::control::{ControlState, ControlTable};
use super::host::driver::tests::{shared_as, FakeIo};
use super::host::driver::{HostCtx, HostShared};
use super::host::gate::{authorize, Gate};
use super::host::phase::HostEvent;
use super::host::reflex::{decide, Reflex};
use super::host::{
    checkpoint::HostCheckpoint, ConsoleRef, HostHandle, HostPhase, PauseCause, SuspendReason,
};
use super::learning;
use super::policy::Authority;
use super::profile::{CtxInput, StewardBrain, KIND_PLAYBOOK};
use super::situation::{self, Program, Situation, SituationKind};
use super::steward::{ScreenDelta, ScreenLine, ScreenStore, StewardRegistry};
use super::steward_loop::{StewardConfig, StewardEventKind, StewardState};

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
            .map(|(y, text)| ScreenLine { y: y as u32, text })
            .collect(),
        ts_ms: ts,
    }
}

/// 帧落地 → 分类。
fn observe(console: &str, lines: &[&str], alt: bool) -> (ScreenStore, Situation) {
    let screen = ScreenStore::new();
    let ack = screen.apply(&frame(console, 1, lines, alt, NOW));
    assert!(ack.applied && !ack.need_full, "全量帧必须落地");
    let snap = screen.get(console).expect("快照在");
    let sit = situation::classify(&snap, NOW + 100, 6_000);
    (screen, sit)
}

/// 用户在剧本面板里写的一条剧本。
fn add_playbook(brain: &StewardBrain, id: &str, spec: serde_json::Value) {
    brain
        .upsert(CtxInput {
            id: Some(id.to_string()),
            kind: KIND_PLAYBOOK.to_string(),
            text: format!("剧本 {id}"),
            strength: None,
            scope: Some("global".to_string()),
            source: Some("user".to_string()),
            evidence: Some("e2e".to_string()),
            extra: Some(spec),
            contract_id: None,
            position: None,
        })
        .unwrap();
}

/// 托管任务眼里的契约（签约后由 host_start 拼出来的那份）。
fn hosted_ctx(cid: &str, console: &str, goal: &str, tier: &str) -> HostCtx {
    let mut c = HostCtx::bare(cid, console, goal);
    c.authority = Authority::default().with_tier(tier);
    c.contract.authority_tier = tier.to_string();
    c
}

fn keys(k: &str) -> AgentAction {
    AgentAction::Keys { target: None, keys: k.into() }
}

/// 让被 spawn 的任务跑到下一个挂起点。
async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

const CC_QUESTION: &[&str] = &[
    "╭────────────────────────────────────╮",
    "│ Do you want me to change the setting about whether to remember this? │",
    "│ 1. Yes                            │",
    "│ 2. No, keep as is                 │",
    "╰────────────────────────────────────╯",
    "  ? for shortcuts · esc to cancel",
];

const PERMISSION_CARD: &[&str] = &[
    "Do you want to proceed?",
    "❯ 1. Yes",
    "  2. Yes, and don't ask again",
    "  3. No",
    "  Esc to cancel",
];

// ───────────── 场景 A：claude code 提问 → 托管任务的反射快路径代答（零 LLM） ─────────────

#[test]
fn e2e_a_question_answered_by_reflex_inside_host_task() {
    let (_screen, sit) = observe("tab-1", CC_QUESTION, true);
    assert_eq!(sit.program, Program::ClaudeCode);
    assert_eq!(sit.kind, SituationKind::Question);
    assert_eq!(sit.options.len(), 2);
    assert!(sit.confidence >= 0.8, "置信度 {sit:?}");

    // 观察器：识别到「在等回答」立刻出建议卡，但只建议、不写（没有决策、没有注入）
    let mut st = StewardState::new("tab-1".into(), "c-a".into());
    let snap = _screen.get("tab-1").unwrap();
    let evs = st.step(Some(&snap), NOW + 100, &StewardConfig::default());
    let card = evs
        .iter()
        .find(|e| e.kind == StewardEventKind::SituationChanged)
        .expect("局面建立");
    assert!(card.suggestion.is_some(), "需要人关注的局面要出建议卡");

    // 托管任务阻塞在 nx_watch、观察到 NeedsAttention：先查剧本
    let brain = StewardBrain::new();
    let ctx = hosted_ctx("c-a", "tab-1", "把 remember 检查关掉，最小改动", "auto");
    assert!(brain.match_playbook(&sit, &ctx.contract.host_key).is_none());
    assert!(
        matches!(decide(None, &sit, &ctx, NOW), Reflex::Wake(_)),
        "没有剧本 → 叫醒模型，绝不凭空编答案"
    );

    add_playbook(
        &brain,
        "pb-remember",
        json!({
            "matchProgram": "claude_code",
            "matchKind": "question",
            "matchText": "remember",
            "answer": "No, keep as is",
            "optionIndex": "2",
            "tier": "auto",
            "enabled": true
        }),
    );
    let pb = brain.match_playbook(&sit, &ctx.contract.host_key);
    assert!(pb.is_some(), "剧本命中");
    match decide(pb.clone(), &sit, &ctx, NOW + 200) {
        Reflex::Answer { bytes, keys, record, playbook_id } => {
            assert_eq!(playbook_id, "pb-remember");
            assert!(bytes.starts_with(b"2"), "选编号而不是整段文本：{bytes:?}");
            assert!(!bytes.contains(&b'\n'), "单行");
            assert!(keys.starts_with('2'));
            // 可审计：零 LLM、指回剧本、归属这份契约
            assert!(!record.llm_used);
            assert_eq!(record.contract_id, "c-a");
            assert_eq!(record.console_id, "tab-1");
            assert!(record.ctx_refs.iter().any(|r| r.source == "playbook" && r.id == "pb-remember"));
            assert!(record.rationale.starts_with("[reflex]"));
        }
        Reflex::Wake(why) => panic!("auto 档、剧本命中应直接代答：{why}"),
    }

    // 同一个剧本在「确认」档下：托管闸门不直接放行 → 叫醒模型走审批，不代答
    let strict = hosted_ctx("c-a", "tab-1", "目标", "approve");
    assert!(matches!(decide(pb, &sit, &strict, NOW + 300), Reflex::Wake(_)));
}

// ───────────── 场景 B：永久扩权选项无论哪条路径都不会被自动选中 ─────────────

#[test]
fn e2e_b_permanent_grant_option_is_never_auto_selected() {
    let (_screen, sit) = observe("tab-2", PERMISSION_CARD, true);
    assert_eq!(sit.kind, SituationKind::Permission);
    assert!(sit.options.iter().any(|o| o.grants_permanent));

    let ctx = hosted_ctx("c-b", "tab-2", "跑测试", "auto");

    // 反射路径：剧本（哪怕用户写错成选 2）也被策略层拦下，叫醒模型
    let brain = StewardBrain::new();
    add_playbook(
        &brain,
        "pb-perm",
        json!({ "matchKind": "permission", "answer": "Yes", "optionIndex": "2", "enabled": true }),
    );
    let pb = brain.match_playbook(&sit, &ctx.contract.host_key);
    assert!(pb.is_some());
    assert!(
        matches!(decide(pb, &sit, &ctx, NOW), Reflex::Wake(_)),
        "选永久扩权项不许代答"
    );

    // 模型路径：nx_send_keys 选 2 → 托管闸门硬拒（auto 档也一样）
    let g = authorize(&ctx.policy(), &keys("2"), Some(&sit));
    assert!(matches!(g, Gate::Deny(_)), "{g:?}");
    // 只选「本次允许」可以
    let g = authorize(&ctx.policy(), &keys("1"), Some(&sit));
    assert!(!matches!(g, Gate::Deny(_)), "本次允许不该被拒：{g:?}");

    // 就算契约打开了「允许永久扩权」、禁项里也拿掉了：托管模式下仍不替用户选永久项
    let mut open = ctx.clone();
    open.authority.allow_permanent_grant = true;
    open.authority.deny.retain(|d| d != "permanent_grant");
    let g = authorize(&open.policy(), &keys("2"), Some(&sit));
    assert!(matches!(g, Gate::Deny(ref why) if why.contains("永久")), "{g:?}");
}

// ───────────── 场景 C：密码屏 → 只走 nx_secret，绝不代填 ─────────────

#[test]
fn e2e_c_password_prompt_is_secret_channel_only() {
    let (_screen, sit) = observe("tab-3", &["[sudo] password for chris:"], false);
    assert_eq!(sit.kind, SituationKind::Password);
    assert_eq!(sit.tail, "[redacted: credential prompt]", "密码屏不进模型上下文");

    let ctx = hosted_ctx("c-c", "tab-3", "安装依赖", "auto");

    // 反射路径：就算有剧本恰好答这一屏，也不代填
    let brain = StewardBrain::new();
    add_playbook(
        &brain,
        "pb-pass",
        json!({ "matchKind": "password", "answer": "hunter2", "enabled": true }),
    );
    let pb = brain.match_playbook(&sit, &ctx.contract.host_key);
    assert!(matches!(decide(pb, &sit, &ctx, NOW), Reflex::Wake(_)));

    // 模型路径：在密码屏上发可见字符 = 拒绝；只能走 nx_secret（用户本人输入）
    let g = authorize(&ctx.policy(), &keys("hunter2<Enter>"), Some(&sit));
    assert!(matches!(g, Gate::Deny(_)), "{g:?}");
    let secret = AgentAction::Secret { target: None, reason: Some("sudo".into()) };
    assert!(!matches!(authorize(&ctx.policy(), &secret, Some(&sit)), Gate::Deny(_)));
}

// ───────────── 场景 D：强杀重启 → 挂起待接驳；屏幕漂移降「确认」档；不自动写 ─────────────

#[test]
fn e2e_d_restart_suspends_and_reattach_downgrades_on_drift() {
    let (store, _dir) = crate::store::tests::temp_store_at("e2e-host-restart");
    let ctx = hosted_ctx("c-d", "tab-d", "升级依赖", "auto");
    let console = ConsoleRef {
        console_id: "tab-d".into(),
        host_key: "local:tab-d".into(),
        label: "本地".into(),
        ..Default::default()
    };
    let mut cp = HostCheckpoint::new(ctx.contract.clone(), ctx.authority.clone(), console.clone(), NOW);
    cp.phase = HostPhase::Running;
    cp.last_screen_hash = "h-before".into();
    store.host_session_upsert("c-d", cp.state(), &cp.to_json(NOW).unwrap(), NOW).unwrap();

    // 启动扫描：运行中的托管一律挂起（AppRestart），记住重启前的相位
    let (restored, expired) = super::host_cmd::scan_rows(&store, NOW + 1_000);
    assert!(expired.is_empty());
    assert_eq!(restored.len(), 1);
    let saved = &restored[0];
    match &saved.phase {
        HostPhase::Suspended { reason, resume_to } => {
            assert_eq!(*reason, SuspendReason::AppRestart);
            assert_eq!(**resume_to, HostPhase::Running);
        }
        p => panic!("重启后应挂起待接驳：{p:?}"),
    }

    // 接驳到新 tab：屏幕一致 → 档位不变、回到运行
    let new_console = ConsoleRef { console_id: "tab-d2".into(), ..console.clone() };
    let (next, _fx, drifted) =
        super::host_cmd::reattach_plan(saved, new_console.clone(), Some("h-before"), NOW + 2_000).unwrap();
    assert!(!drifted);
    assert_eq!(next.phase, HostPhase::Running);
    assert_eq!(next.authority.tier, "auto");
    assert_eq!(next.contract.console_id, "tab-d2");

    // 屏幕变了 → 降到「确认」档；看不到帧也按漂移处理（绝不当成一致直接恢复写权限）
    for hash in [Some("h-other"), None] {
        let (next, _fx, drifted) =
            super::host_cmd::reattach_plan(saved, new_console.clone(), hash, NOW + 2_000).unwrap();
        assert!(drifted, "hash={hash:?}");
        assert_eq!(next.authority.tier, "approve");
        assert_eq!(next.contract.authority_tier, "approve");
    }

    // 过了 TTL 的现场不再接驳
    let late = NOW + super::checkpoint::CHECKPOINT_TTL_MS + 10_000;
    assert!(super::host_cmd::reattach_plan(saved, new_console, Some("h-before"), late).is_err());
}

// ───────────── 场景 E：接管 / 交还 / 预算不被绕过 / Notify 纠正学习 ─────────────

#[tokio::test(start_paused = true)]
async fn e2e_e_takeover_handback_budget_and_correction() {
    // 签约前先拿租约：契约出现时这个 console 已经归托管任务
    let table = ControlTable::new();
    let registry = StewardRegistry::new();
    let (contract, changed) = super::host_cmd::claim_then_sign(&table, "tab-e", "host:c-e", || {
        Ok::<_, String>(registry.start_with_id("c-e", "tab-e", "local", "跑测试", "notify"))
    })
    .unwrap();
    assert_eq!(contract.id, "c-e");
    assert_eq!(changed, Some(ControlState::AgentDriving));
    assert_eq!(table.holders_of("tab-e"), vec!["host:c-e".to_string()]);

    let (h, io): (Arc<HostShared>, Arc<FakeIo>) = shared_as("c-e", "tab-e");
    assert_eq!(h.phase(), HostPhase::Running);

    // 用户敲键：写入闸口挂起，进 UserDriving；多久都不超时、不写
    let h2 = h.clone();
    let task = tokio::spawn(async move { h2.gate(|| true, || true).await });
    settle().await;
    assert_eq!(h.phase(), HostPhase::UserDriving { resume_to: Box::new(HostPhase::Running) });
    tokio::time::advance(Duration::from_secs(4 * 3600)).await;
    settle().await;
    assert!(!task.is_finished(), "人在驾驶期间零写入、零 LLM");
    h.send(HostEvent::Handback);
    assert!(task.await.unwrap(), "交还后才可以写");
    assert_eq!(h.phase(), HostPhase::Running);
    assert_eq!(*io.released.lock(), 0, "接管 / 交还都不放租约");

    // 预算用尽 → 暂停；按一下键再交还，仍回到预算暂停（堵住旧守护进程的绕过）
    h.apply(HostEvent::BudgetExhausted("步数".into())).unwrap();
    let paused = h.phase();
    assert!(matches!(paused, HostPhase::Paused { cause: PauseCause::Budget { .. } }), "{paused:?}");
    h.apply(HostEvent::UserKey).unwrap();
    h.apply(HostEvent::Handback).unwrap();
    assert_eq!(h.phase(), paused, "交还不能绕过预算暂停");
    h.apply(HostEvent::Resume { new_budget: None }).unwrap();
    assert_eq!(h.phase(), HostPhase::Running, "只有用户「加预算继续」才回到运行");

    // Notify 档：模型的写入执行后发通知卡；用户纠正 → Learned 偏好，同类纠正不重复堆积
    let ctx = hosted_ctx("c-e", "tab-e", "跑测试", "notify");
    assert_eq!(
        authorize(&ctx.policy(), &AgentAction::Exec { target: None, command: "cargo test".into() }, None),
        Gate::Notify
    );
    let brain = StewardBrain::new();
    let topic = "claude_code|question|settings:remember-check";
    let fix = "不要改设置 UI，直接改代码";
    assert_eq!(learning::should_learn(&brain.view().items, topic, fix), learning::LearnDecision::Create);
    let learned = brain.learn_correction(41, topic, fix).unwrap();
    assert_eq!(learned.source, "learned");
    assert!(learned.evidence.contains("#41"), "指回被纠正的那条决策");
    assert!(matches!(
        learning::should_learn(&brain.view().items, topic, fix),
        learning::LearnDecision::Skip { .. } | learning::LearnDecision::Merge { .. }
    ));

    // 停止：终态才放租约
    h.apply(HostEvent::Stop).unwrap();
    assert!(h.is_terminal());
    assert_eq!(*io.released.lock(), 1);
}

// ───────────── 用户暂停 / 挂起的卡片同样不超时：交还回到原来的卡 ─────────────

#[tokio::test(start_paused = true)]
async fn e2e_handback_returns_to_pending_card_not_running() {
    let io = Arc::new(FakeIo::default());
    let h = HostShared::new(
        HostHandle::new("c-f"),
        "tab-f",
        "目标",
        Arc::new(AtomicBool::new(false)),
        Box::new(io.clone()),
    );
    h.apply(HostEvent::Ask(super::host::HumanCard::ask("k1", "部署到哪？", NOW))).unwrap();
    h.apply(HostEvent::Takeover).unwrap();
    assert!(matches!(h.phase(), HostPhase::UserDriving { .. }));
    h.apply(HostEvent::Handback).unwrap();
    assert!(matches!(h.phase(), HostPhase::WaitingHuman { ref card } if card.id == "k1"));
}
