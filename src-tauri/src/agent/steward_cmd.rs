//! NexusAgent v2 / P2 的命令层：决策依据的读写、决策记录回放、自动应答总闸、升级卡裁决。
//!
//! 与 `steward.rs`（会话织物）/ `steward_loop.rs`（常驻循环）分开，是为了让
//! 「循环」这一侧保持纯粹：这里的所有命令都是**用户显式动作**，不会自己往终端写字节。
//! 唯一能触发注入的是升级卡裁决（投递 seq 给循环）和自动应答总闸（改一个 bool）。
//!
//! 安全约定：
//! - 升级卡只接受 Brain 里已存在的 DecisionRecord 的 `chosen`，不接受任意文本；
//! - 「自动应答」每次冷启动默认关闭，只在内存里，不持久化；
//! - 用户否掉决策时可写一条 `Learned` 偏好，构成 §4.4 的闭环学习。

use serde_json::json;
use tauri::{AppHandle, Emitter, State};

use crate::cmd::AppState;

use super::checkpoint::{StewardCheckpoint, TimeboxConfig};
use super::decision::{self, DecisionRecord};
use super::learning::{self, AuditEntry, Explanation, PlaybookCandidate, ReviewReport};
use super::ownership::AbsenceSummary;
use super::planner::{self, PlannerView};
use super::policy::{self, Authority, PolicyView};
use super::profile::{BrainView, CtxInput, CtxItem, StewardBrain, KIND_INTENT};
use super::situation;
use super::steward::{now_ms, ScreenStore, StewardContract, StewardRegistry};
use super::steward_core::{CoreView, StewardCore};
use super::steward_loop::{
    build_context, persist_decision, EventLog, StewardBus, StewardEvent, StewardEventKind,
    StewardResolutions,
};

/// 命令层直接往托管事件流补一条系统事件（循环下一 tick 也会看到同一份 log）。
fn emit_event(
    app: &AppHandle,
    log: &EventLog,
    console_id: &str,
    contract_id: &str,
    kind: StewardEventKind,
    message: impl Into<String>,
    llm_calls: u32,
) {
    let e = StewardEvent {
        seq: 0,
        ts_ms: now_ms(),
        console_id: console_id.to_string(),
        contract_id: contract_id.to_string(),
        kind,
        message: message.into(),
        suggestion: None,
        situation: None,
        frame_id: 0,
        screen_hash: String::new(),
        llm_calls,
        decision: None,
    };
    for stored in log.append(vec![e]) {
        let _ = app.emit("nx://steward/event", stored);
    }
}

/// 一次拿全四类依据 + 执行开关，面板不用拼四次。
#[tauri::command]
pub fn steward_brain(brain: State<'_, StewardBrain>) -> BrainView {
    brain.view()
}

/// 新增 / 编辑一条依据。校验在 `profile::normalize_input`，持久化与内存同一次调用完成。
#[tauri::command]
pub fn steward_ctx_upsert(
    input: CtxInput,
    brain: State<'_, StewardBrain>,
    state: State<'_, AppState>,
    bus: State<'_, StewardBus>,
) -> Result<CtxItem, String> {
    let item = brain.upsert(input)?;
    state.store.steward_ctx_upsert(&item.to_row())?;
    bus.wake();
    Ok(item)
}

#[tauri::command]
pub fn steward_ctx_delete(
    id: String,
    brain: State<'_, StewardBrain>,
    state: State<'_, AppState>,
    bus: State<'_, StewardBus>,
) -> Result<bool, String> {
    let removed = brain.delete(&id);
    if removed {
        state.store.steward_ctx_delete(&id)?;
        bus.wake();
    }
    Ok(removed)
}

/// 往某个契约的意向栈压一条。position 自动取当前最大值 +1（栈顶）。
#[tauri::command]
pub fn steward_intent_push(
    contract_id: String,
    text: String,
    constraints: Option<Vec<String>>,
    answer_template: Option<String>,
    brain: State<'_, StewardBrain>,
    state: State<'_, AppState>,
    bus: State<'_, StewardBus>,
) -> Result<CtxItem, String> {
    if contract_id.trim().is_empty() {
        return Err("契约 id 不能为空".to_string());
    }
    let position = brain
        .intent_stack(&contract_id)
        .first()
        .map(|i| i.position + 1)
        .unwrap_or(0);
    let item = brain.upsert(CtxInput {
        id: None,
        kind: KIND_INTENT.to_string(),
        text,
        strength: None,
        scope: None,
        source: Some("user".to_string()),
        evidence: Some("用户在托管面板压入".to_string()),
        extra: Some(json!({
            "successCriteria": [],
            "constraints": constraints.unwrap_or_default(),
            "answerTemplate": answer_template,
            "status": "active",
            "priority": 0
        })),
        contract_id: Some(contract_id),
        position: Some(position),
    })?;
    state.store.steward_ctx_upsert(&item.to_row())?;
    bus.wake();
    Ok(item)
}

/// 回放最近决策。库里存的是整条 JSON，`seq` / `outcome` 以列为准回填，
/// 这样回执校验原地更新 outcome 后，前端看到的就是最新状态。
#[tauri::command]
pub fn steward_decisions(
    limit: Option<usize>,
    state: State<'_, AppState>,
) -> Vec<DecisionRecord> {
    state
        .store
        .steward_decisions(limit.unwrap_or(40))
        .into_iter()
        .filter_map(|(seq, outcome, raw)| {
            let mut rec: DecisionRecord = serde_json::from_str(&raw).ok()?;
            rec.seq = seq;
            rec.outcome = outcome;
            Some(rec)
        })
        .collect()
}

/// 「立即决策」：只编译 + 落库 + 返回，**不注入、不写事件流**。
/// 用来人工核对「这个局面会怎么答、依据是哪几条」，不影响常驻循环的去重。
#[tauri::command]
pub fn steward_decide_now(
    console_id: String,
    app: AppHandle,
    screen: State<'_, ScreenStore>,
    registry: State<'_, StewardRegistry>,
    brain: State<'_, StewardBrain>,
    core: State<'_, StewardCore>,
    state: State<'_, AppState>,
) -> Result<DecisionRecord, String> {
    let Some(snap) = screen.get(&console_id) else {
        return Err("还没有这个 console 的屏幕帧".to_string());
    };
    let Some(contract) = registry.for_console(&console_id) else {
        return Err("这个 console 还没有托管契约".to_string());
    };
    let sit = situation::classify(&snap, now_ms(), 6_000);
    let mut rec = decision::compile(&build_context(&contract, &sit, &brain, now_ms()));
    // P3：面板上的「立即决策」和常驻循环走同一套策略复核，结果才不会两张皮。
    let auth = core.authority(&contract.id);
    let known = state.local.contains(&console_id) || state.sessions.contains(&console_id);
    let in_scope = policy::host_in_scope(&auth, &contract.host_key, known);
    policy::authorize(&auth, &mut rec, &sit, in_scope);
    rec.seq = persist_decision(&app, &rec);
    brain.remember_decision(rec.seq, &rec);
    Ok(rec)
}

/// 自动应答总闸。**不持久化**：每次冷启动都是 false，必须显式打开。
#[tauri::command]
pub fn steward_execution(brain: State<'_, StewardBrain>) -> bool {
    brain.execution_enabled()
}

#[tauri::command]
pub fn steward_set_execution(
    enabled: bool,
    brain: State<'_, StewardBrain>,
    bus: State<'_, StewardBus>,
) -> bool {
    let on = brain.set_execution(enabled);
    bus.wake();
    on
}

/// 升级卡「按建议执行」：只投递 seq，注入由常驻循环执行。
/// 这里同时挡掉密码 / 不可注入动作，避免升级卡变成一个通用写终端后门。
#[tauri::command]
pub fn steward_approve(
    seq: i64,
    brain: State<'_, StewardBrain>,
    resolutions: State<'_, StewardResolutions>,
    bus: State<'_, StewardBus>,
    core: State<'_, StewardCore>,
) -> Result<bool, String> {
    let rec = brain
        .decision(seq)
        .ok_or_else(|| format!("找不到决策 #{seq}（可能已过期）"))?;
    if rec.chosen.send_bytes().is_none() {
        return Err("这条建议不能自动注入（密码 / 只读动作需本人处理）".to_string());
    }
    if !core.ownership().can_write(&rec.console_id) {
        return Err("当前是观察 / 暂停态：请先点「交还」再确认执行".to_string());
    }
    resolutions.approve(seq);
    bus.wake();
    Ok(true)
}

/// 升级卡「我自己来」：撤销决策；给了纠正文本就写一条 Learned 偏好。
#[tauri::command]
pub fn steward_reject(
    seq: i64,
    correction: Option<String>,
    brain: State<'_, StewardBrain>,
    state: State<'_, AppState>,
    resolutions: State<'_, StewardResolutions>,
    bus: State<'_, StewardBus>,
) -> Result<bool, String> {
    let rec = brain
        .decision(seq)
        .ok_or_else(|| format!("找不到决策 #{seq}（可能已过期）"))?;
    resolutions.reject(seq);
    state
        .store
        .steward_decision_outcome(seq, "rolled_back")
        .ok();
    if let Some(text) = correction.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        let topic = rec.situation_signature.clone();
        // P5：同类纠正不无限堆积；等价的 Learned 偏好已存在就跳过。
        if !matches!(
            learning::should_learn(&brain.view().items, &topic, &text),
            learning::LearnDecision::Skip { .. }
        ) {
            let learned = brain.learn_correction(seq, &topic, &text)?;
            state.store.steward_ctx_upsert(&learned.to_row())?;
        }
    }
    bus.wake();
    Ok(true)
}

#[tauri::command]
pub fn steward_clear_decisions(state: State<'_, AppState>) -> Result<(), String> {
    state.store.steward_decisions_clear()
}

/// 面板上的配置里带一份「静态门控说明」，避免前端硬编码策略文字。
#[tauri::command]
pub fn steward_policy() -> serde_json::Value {
    json!({
        "llmUsed": false,
        "executionDefault": false,
        "receiptWindowMs": decision::RECEIPT_WINDOW_MS,
        "escalationTimeoutMs": decision::ESCALATION_TIMEOUT_MS,
        "minAutoConfidence": decision::MIN_AUTO_CONFIDENCE,
        "maxAutoStaleMs": decision::MAX_AUTO_STALE_MS,
        "deny": ["password", "permanent_grant", "stale_feed", "low_confidence", "approve_tier"]
    })
}

// ───────────────────── P3：授权、所有权、撤销窗口 ─────────────────────

/// 一次拿全 P3–P5 的运行时状态：所有权、授权契约、撤销窗口、时间盒、预算、恢复队列。
#[tauri::command]
pub fn steward_core_view(core: State<'_, StewardCore>) -> CoreView {
    core.view(now_ms())
}

/// 用户敲键 → 100ms 内夺权。前端在 `termRegistry.onInput` 上节流调用，
/// `keyAtMs` 是键盘事件的时间戳，返回处理延迟便于核对 SLA。
#[tauri::command]
pub fn steward_user_key(
    console_id: String,
    key_at_ms: i64,
    registry: State<'_, StewardRegistry>,
    core: State<'_, StewardCore>,
    bus: State<'_, StewardBus>,
) -> i64 {
    if registry.for_console(&console_id).is_none() {
        return -1; // 没托管的 tab 不建槽，避免每个键都产生副作用
    }
    let latency = core.ownership().user_key(&console_id, key_at_ms, now_ms());
    bus.wake();
    latency
}

#[tauri::command]
pub fn steward_takeover(
    console_id: String,
    core: State<'_, StewardCore>,
    bus: State<'_, StewardBus>,
) -> bool {
    core.ownership().takeover(&console_id, core.elapsed_ms());
    bus.wake();
    true
}

/// 交还：先读屏对齐（与 Checkpoint 的 screen_hash 比对），再给「离开期间」摘要。
/// hash 不一致时自动把档位降为「确认」，与设计 §3.5 / §3.6 一致。
#[tauri::command]
pub fn steward_handback(
    console_id: String,
    screen_aligned: Option<bool>,
    app: AppHandle,
    screen: State<'_, ScreenStore>,
    registry: State<'_, StewardRegistry>,
    core: State<'_, StewardCore>,
    log: State<'_, EventLog>,
    bus: State<'_, StewardBus>,
) -> AbsenceSummary {
    let aligned = screen_aligned.unwrap_or_else(|| {
        let cp = registry
            .for_console(&console_id)
            .and_then(|c| core.checkpoint(&c.id));
        match (cp, screen.get(&console_id)) {
            (Some(cp), Some(s)) => cp.screen_hash.is_empty() || cp.screen_hash == s.hash,
            (None, _) => true,
            _ => false,
        }
    });
    if !aligned {
        if let Some(c) = registry.for_console(&console_id) {
            let a = core.authority(&c.id).with_tier("approve");
            core.set_authority(&c.id, a);
            registry.amend(&c.id, None, Some("approve".to_string()));
        }
    }
    let summary = core
        .ownership()
        .handback(&console_id, core.elapsed_ms(), aligned);
    let contract_id = registry
        .for_console(&console_id)
        .map(|c| c.id)
        .unwrap_or_default();
    emit_event(
        &app,
        &log,
        &console_id,
        &contract_id,
        StewardEventKind::Handback,
        format!(
            "已交还（离开 {}ms，{} 条已做 / {} 条待决）：{}",
            summary.duration_ms,
            summary.done.len(),
            summary.pending.len(),
            summary.next_step
        ),
        0,
    );
    bus.wake();
    summary
}

/// Notify 档 30s 撤销窗口：立刻转观察态（阻止后续连发）、标记 rolled_back、
/// 并写一条 Learned 偏好，让同类动作下次不再自动放行。
#[tauri::command]
pub fn steward_undo(
    seq: i64,
    core: State<'_, StewardCore>,
    brain: State<'_, StewardBrain>,
    state: State<'_, AppState>,
    bus: State<'_, StewardBus>,
) -> Result<bool, String> {
    let entry = core
        .consume_undo(seq, now_ms())
        .ok_or_else(|| format!("决策 #{seq} 的撤销窗口已过期或不存在"))?;
    core.ownership().takeover(&entry.console_id, core.elapsed_ms());
    state
        .store
        .steward_decision_outcome(seq, "rolled_back")
        .ok();
    let text = format!("不要自动应答：{}", entry.action.text);
    if !matches!(
        learning::should_learn(&brain.view().items, "undo", &text),
        learning::LearnDecision::Skip { .. }
    ) {
        if let Ok(item) = brain.learn_correction(seq, "undo", &text) {
            state.store.steward_ctx_upsert(&item.to_row()).ok();
        }
    }
    bus.wake();
    Ok(true)
}

/// 真正的授权契约（不再是 P2 的静态数组）。
#[tauri::command]
pub fn steward_policy_v2(
    contract_id: Option<String>,
    core: State<'_, StewardCore>,
) -> PolicyView {
    let a = contract_id
        .as_deref()
        .map(|id| core.authority(id))
        .unwrap_or_default();
    policy::policy_view(&a)
}

#[tauri::command]
pub fn steward_set_authority(
    contract_id: String,
    authority: Authority,
    registry: State<'_, StewardRegistry>,
    core: State<'_, StewardCore>,
    bus: State<'_, StewardBus>,
) -> Authority {
    let a = core.set_authority(&contract_id, authority);
    registry.amend(&contract_id, None, Some(a.tier.clone()));
    core.set_timebox_config(TimeboxConfig::from_authority(&a));
    bus.wake();
    a
}

// ───────────────────── P4：Timebox / Checkpoint / 恢复 ─────────────────────

#[tauri::command]
pub fn steward_timebox(core: State<'_, StewardCore>) -> TimeboxConfig {
    core.timebox_config()
}

#[tauri::command]
pub fn steward_set_timebox(
    config: TimeboxConfig,
    core: State<'_, StewardCore>,
    bus: State<'_, StewardBus>,
) -> TimeboxConfig {
    let c = core.set_timebox_config(config);
    bus.wake();
    c
}

#[tauri::command]
pub fn steward_checkpoints(core: State<'_, StewardCore>) -> Vec<StewardCheckpoint> {
    core.checkpoints()
}

/// 手动触发恢复：契约不在内存里时先从 Checkpoint 重建，再投递给常驻循环读屏对齐。
#[tauri::command]
pub fn steward_resume(
    contract_id: String,
    registry: State<'_, StewardRegistry>,
    core: State<'_, StewardCore>,
    bus: State<'_, StewardBus>,
) -> Result<String, String> {
    if registry.get(&contract_id).is_none() {
        let cp = core
            .checkpoint(&contract_id)
            .ok_or_else(|| format!("找不到契约 {contract_id} 的 Checkpoint"))?;
        let c = StewardContract {
            id: cp.contract_id.clone(),
            console_id: cp.console_id.clone(),
            host_key: format!("local:{}", cp.console_id),
            goal: cp.goal.clone(),
            authority_tier: cp.authority.tier.clone(),
            status: "observing".to_string(),
            intent_ids: cp.intent_ids.clone(),
            timebox_ms: cp.authority.timebox_ms,
            created_at: cp.saved_wall_ms,
        };
        core.set_authority(&contract_id, cp.authority.clone());
        registry.restore(c);
    }
    core.ensure_console(
        &registry
            .get(&contract_id)
            .map(|c| c.console_id)
            .unwrap_or_default(),
    );
    core.push_resume(&contract_id);
    bus.wake();
    Ok("已投递恢复：常驻循环会先读屏对齐，再决定降档 / 暂停 / 直接续跑".to_string())
}

// ───────────────────── P5：解释链 / 复盘 / 蒸馏 / Planner ─────────────────────

#[tauri::command]
pub fn steward_explain(
    seq: i64,
    brain: State<'_, StewardBrain>,
    state: State<'_, AppState>,
) -> Option<Explanation> {
    let rec = brain.decision(seq).or_else(|| {
        state
            .store
            .steward_decisions(200)
            .into_iter()
            .find(|(s, _, _)| *s == seq)
            .and_then(|(s, outcome, raw)| {
                let mut r: DecisionRecord = serde_json::from_str(&raw).ok()?;
                r.seq = s;
                r.outcome = outcome;
                Some(r)
            })
    })?;
    Some(learning::explain(&rec, &brain.view().items))
}

#[tauri::command]
pub fn steward_review(
    contract_id: Option<String>,
    state: State<'_, AppState>,
) -> ReviewReport {
    let recs: Vec<DecisionRecord> = state
        .store
        .steward_decisions(300)
        .into_iter()
        .filter_map(|(seq, outcome, raw)| {
            let mut r: DecisionRecord = serde_json::from_str(&raw).ok()?;
            r.seq = seq;
            r.outcome = outcome;
            Some(r)
        })
        .filter(|r| {
            contract_id
                .as_deref()
                .map(|id| r.contract_id == id)
                .unwrap_or(true)
        })
        .collect();
    learning::review(contract_id.as_deref().unwrap_or("*"), &recs)
}

/// 统一审计视图：观察事件 + 决策记录合成一条时间线（既有「时间线」tab 也已有 Decision 事件）。
#[tauri::command]
pub fn steward_audit(
    contract_id: Option<String>,
    log: State<'_, EventLog>,
    state: State<'_, AppState>,
) -> Vec<AuditEntry> {
    let mut events = log.since(0, 200);
    if let Some(id) = &contract_id {
        events.retain(|e| e.contract_id == *id);
    }
    let mut recs: Vec<DecisionRecord> = state
        .store
        .steward_decisions(200)
        .into_iter()
        .filter_map(|(seq, outcome, raw)| {
            let mut r: DecisionRecord = serde_json::from_str(&raw).ok()?;
            r.seq = seq;
            r.outcome = outcome;
            Some(r)
        })
        .collect();
    if let Some(id) = &contract_id {
        recs.retain(|r| r.contract_id == *id);
    }
    learning::merge_audit(&events, &recs)
}

/// 预览可沉淀的剧本候选（循环每 60s 会自动把新候选写入 Brain）。
#[tauri::command]
pub fn steward_distill(state: State<'_, AppState>) -> Vec<PlaybookCandidate> {
    let recs: Vec<DecisionRecord> = state
        .store
        .steward_decisions(200)
        .into_iter()
        .filter_map(|(_, _, raw)| serde_json::from_str(&raw).ok())
        .collect();
    learning::distill(&recs)
}

#[tauri::command]
pub fn steward_planner(core: State<'_, StewardCore>) -> PlannerView {
    planner::view(core.planner())
}

/// LLM Planner 总闸：默认关闭、不持久化，每次冷启动回到 false。
#[tauri::command]
pub fn steward_set_planner(
    enabled: bool,
    core: State<'_, StewardCore>,
    bus: State<'_, StewardBus>,
) -> bool {
    let on = core.planner().set(enabled);
    bus.wake();
    on
}

/// 真·LLM 兜底：只在总闸打开、规则编不出可注入回复、且拿到全局并发许可时调用。
/// 结果仍要过 `policy::authorize`；失败一律回退 Observe，不把错误抛给托管循环。
#[tauri::command]
pub async fn steward_plan_now(
    console_id: String,
    app: AppHandle,
    screen: State<'_, ScreenStore>,
    registry: State<'_, StewardRegistry>,
    brain: State<'_, StewardBrain>,
    core: State<'_, StewardCore>,
    state: State<'_, AppState>,
    log: State<'_, EventLog>,
    bus: State<'_, StewardBus>,
) -> Result<DecisionRecord, String> {
    if !core.planner().enabled() {
        return Err("LLM Planner 总闸未打开".to_string());
    }
    let Some(_permit) = core.llm_permit() else {
        return Err("LLM 并发已达上限（默认 2），请稍后再试".to_string());
    };
    let snap = screen
        .get(&console_id)
        .ok_or_else(|| "还没有这个 console 的屏幕帧".to_string())?;
    let contract = registry
        .for_console(&console_id)
        .ok_or_else(|| "这个 console 还没有托管契约".to_string())?;
    let sit = situation::classify(&snap, now_ms(), 6_000);
    let ctx = build_context(&contract, &sit, &brain, now_ms());
    let mut rec = decision::compile(&ctx);
    if !planner::should_plan(core.planner(), &rec) {
        return Ok(rec); // 规则已给出回复，不需要模型
    }
    let cfg = super::load_config(&state.store);
    let acct = cfg.active_account().clone();
    if acct.base_url.trim().is_empty() || acct.api_key.trim().is_empty() {
        return Err("AI 账号未配置完整（baseUrl / apiKey）".to_string());
    }
    let (system, messages) = planner::build_request(&ctx);
    let reply = super::llm::complete_reply(&acct, &system, &messages, &[], None)
        .await
        .map_err(|e| format!("Planner 调用失败：{e:?}"))?;
    let (action, why) = planner::parse_reply(&reply.text);
    let auth = core.authority(&contract.id);
    let known = state.local.contains(&console_id) || state.sessions.contains(&console_id);
    let in_scope = policy::host_in_scope(&auth, &contract.host_key, known);
    planner::apply_plan(&mut rec, action, &why, &auth, &sit, in_scope);
    rec.seq = persist_decision(&app, &rec);
    brain.remember_decision(rec.seq, &rec);
    emit_event(
        &app,
        &log,
        &console_id,
        &contract.id,
        StewardEventKind::PlannerUsed,
        format!(
            "LLM Planner 兜底产出决策 #{}（{}），已由 PolicyEngine 复核",
            rec.seq,
            rec.chosen.label()
        ),
        1,
    );
    bus.wake();
    Ok(rec)
}
