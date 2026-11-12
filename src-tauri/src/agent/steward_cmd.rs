//! 托管面板的命令层：决策依据（偏好 / 意向 / 知识 / 剧本）的读写、决策记录回放与复盘。
//!
//! 这里的命令都只读写依据与审计记录，**不会往终端写字节**。动手的唯一入口是托管任务
//! （`host_cmd.rs`）；守护进程那一套升级卡裁决 / 撤销 / 接管交还 / 恢复 / Planner 命令
//! 已随零 LLM 写路径在 PR8 删除，对应能力由 `host_*` 命令承担。

use serde_json::json;
use tauri::State;

use crate::cmd::AppState;

use super::decision::DecisionRecord;
use super::learning::{self, AuditEntry, Explanation, ReviewReport};
use super::profile::{BrainView, CtxInput, CtxItem, StewardBrain, KIND_INTENT};
use super::steward_loop::{EventLog, StewardBus};

/// 一次拿全四类依据，面板不用拼四次。
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


#[tauri::command]
pub fn steward_clear_decisions(state: State<'_, AppState>) -> Result<(), String> {
    state.store.steward_decisions_clear()
}

// ───────────────────── 解释链 / 复盘 / 审计 ─────────────────────

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

