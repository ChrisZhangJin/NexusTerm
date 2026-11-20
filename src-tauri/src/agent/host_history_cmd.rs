//! 「结果可追溯、可回看」：历史托管、实际改动、改文件前的原文备份与还原。
//!
//! - `host_history(limit)`：最近结束的托管（completed / failed / stopped），新的在前；
//! - `host_changes(contract_id)`：一次托管的「实际改动」（按决策记录提取，见 `host::changes`）+ 可还原的备份；
//! - `host_restore_backup(contract_id, backup_id)`：把备份的原文经同一个终端的文件通道（本地 / SFTP）写回去。
//!
//! 改动摘要在托管结束时定格进 `host_changes` 表（决策表只留最近 500 条，老托管靠它）。

use serde::Serialize;
use tauri::{AppHandle, State};

use crate::cmd::AppState;

use super::checkpoint::Spent;
use super::host::backup::{self, BackupEntry};
use super::host::changes::{self, ChangeSummary};
use super::host::checkpoint::HostCheckpoint;
use super::host::{CheckResult, HostPhase};
use super::steward::now_ms;

/// 历史托管一条。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostHistoryItem {
    pub contract_id: String,
    pub goal: String,
    /// 被托管终端的可读名字
    pub label: String,
    pub console_id: String,
    pub started_at: i64,
    pub ended_at: i64,
    pub duration_ms: i64,
    /// completed / failed / stopped
    pub state: String,
    /// 完成报告（completed）
    pub summary: Option<String>,
    /// 失败原因（failed）
    pub error: Option<String>,
    pub checks: Vec<CheckResult>,
    pub spent: Spent,
    pub changes: ChangeSummary,
    pub backups: Vec<BackupEntry>,
}

/// 一次托管的实际改动 + 可还原的备份。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostChanges {
    pub changes: ChangeSummary,
    pub backups: Vec<BackupEntry>,
}

fn backup_root(app: &AppHandle) -> std::path::PathBuf {
    backup::root(&crate::app_data_dir(app))
}

/// 定格过的优先；没有就按决策记录现算。
pub(crate) fn changes_for(store: &crate::store::Store, contract_id: &str) -> ChangeSummary {
    if let Some(s) = store.host_changes_get(contract_id).and_then(|j| serde_json::from_str(&j).ok()) {
        return s;
    }
    let rows = store.steward_decisions_for(contract_id);
    changes::from_json_rows(contract_id, rows.iter().map(String::as_str))
}

/// 托管结束时调用：把改动摘要定格下来。
pub(crate) fn snapshot_changes(store: &crate::store::Store, contract_id: &str) {
    let rows = store.steward_decisions_for(contract_id);
    let s = changes::from_json_rows(contract_id, rows.iter().map(String::as_str));
    match serde_json::to_string(&s) {
        Ok(j) => {
            if let Err(e) = store.host_changes_put(contract_id, &j, now_ms()) {
                tracing::warn!("托管改动摘要落盘失败：{e}");
            }
        }
        Err(e) => tracing::warn!("托管改动摘要序列化失败：{e}"),
    }
}

/// act.rs 写文件成功后调用：原文存一份。失败只记日志，不影响托管。
pub(crate) fn back_up_original(app: &AppHandle, contract_id: &str, console_id: &str, path: &str, original: &[u8]) {
    if let Err(e) = backup::save(&backup_root(app), contract_id, console_id, path, original, now_ms()) {
        // 不带内容、不带路径以外的东西
        tracing::warn!("托管改文件前的原文备份失败（{path}）：{e}");
    }
}

/// 启动时调用：托管行已被清理掉的，备份目录一并删掉。
pub(crate) fn prune_backups(app: &AppHandle, store: &crate::store::Store) {
    let n = backup::prune(&backup_root(app), |id| store.host_session_get(id).is_some());
    if n > 0 {
        tracing::info!("清理托管原文备份 {n} 份");
    }
}

/// 一行托管现场 → 历史条目（纯函数；不是结束态 / 读不回来的返回 None）。
pub(crate) fn history_item(row: &crate::store::host::HostSessionRow, changes: ChangeSummary, backups: Vec<BackupEntry>) -> Option<HostHistoryItem> {
    let cp = HostCheckpoint::from_json(&row.json).ok()?;
    let (summary, error, checks) = match &cp.phase {
        HostPhase::Completed { summary, checks } => (Some(summary.clone()), None, checks.clone()),
        HostPhase::Failed { error } => (None, Some(error.clone()), Vec::new()),
        HostPhase::Stopped => (None, None, Vec::new()),
        _ => return None,
    };
    let started_at = cp.created_wall_ms;
    let ended_at = row.updated_at.max(started_at);
    Some(HostHistoryItem {
        contract_id: row.contract_id.clone(),
        goal: cp.contract.goal.clone(),
        label: cp.console.label.clone(),
        console_id: cp.console.console_id.clone(),
        started_at,
        ended_at,
        duration_ms: ended_at - started_at,
        state: cp.phase.label().to_string(),
        summary,
        error,
        checks,
        spent: cp.spent,
        changes,
        backups,
    })
}

#[tauri::command]
pub fn host_history(limit: Option<usize>, app: AppHandle, state: State<'_, AppState>) -> Vec<HostHistoryItem> {
    let root = backup_root(&app);
    state
        .store
        .host_history_rows(limit.unwrap_or(crate::store::host::HISTORY_KEEP))
        .iter()
        .filter_map(|row| {
            history_item(row, changes_for(&state.store, &row.contract_id), backup::list(&root, &row.contract_id))
        })
        .collect()
}

#[tauri::command]
pub fn host_changes(contract_id: String, app: AppHandle, state: State<'_, AppState>) -> HostChanges {
    HostChanges {
        changes: changes_for(&state.store, &contract_id),
        backups: backup::list(&backup_root(&app), &contract_id),
    }
}

/// 把备份的原文写回去。走原来那个终端的文件通道；终端已经关了就报错。
#[tauri::command]
pub async fn host_restore_backup(
    contract_id: String,
    backup_id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<String, String> {
    let root = backup_root(&app);
    let (entry, bytes) = backup::load(&root, &contract_id, &backup_id)?;
    let st = &*state;
    if !st.local.contains(&entry.console_id) && !st.sessions.contains(&entry.console_id) {
        return Err(format!(
            "当初改「{}」的那个终端已经关闭，没法经它写回。请重新连上那台机器后手动恢复（原文保存在应用数据目录的 host-backups 里）",
            entry.path
        ));
    }
    let text = String::from_utf8(bytes).map_err(|_| "备份不是文本，不还原".to_string())?;
    let transport = super::files::transport_for(st, &entry.console_id).await.map_err(|e| format!("打不开文件通道：{e}"))?;
    let ops = super::files::FileOps::new(&*transport, super::files::FileOpsConfig::default());
    // 用户在确认框里点了「还原」：受保护路径（当初也是批准过才写的）照样允许写回
    let opts = super::files::WriteOptions { create_dirs: true, allow_sensitive: true };
    ops.write(&entry.path, &text, &opts).await.map_err(|e| format!("还原「{}」失败：{e}", entry.path))?;
    drop(ops);
    backup::mark_restored(&root, &contract_id, &backup_id, now_ms()).ok();
    Ok(format!("已把「{}」还原成托管改动之前的内容", entry.path))
}

#[cfg(test)]
mod tests {
    use super::super::host::{ConsoleRef, HostCtx};
    use super::*;
    use crate::store::host::HostSessionRow;

    fn cp(phase: HostPhase) -> HostCheckpoint {
        let ctx = HostCtx::bare("c1", "t1", "装好 nginx");
        let console = ConsoleRef { host_key: "local".into(), label: "本地终端".into(), console_id: "t1".into(), ..Default::default() };
        let mut c = HostCheckpoint::new(ctx.contract, ctx.authority, console, 1_000);
        c.phase = phase;
        c
    }

    fn row(c: &HostCheckpoint, updated: i64) -> HostSessionRow {
        HostSessionRow { contract_id: "c1".into(), state: c.state().to_string(), json: c.to_json(updated).unwrap(), updated_at: updated }
    }

    #[test]
    fn history_item_reads_finished_rows_only() {
        let done = cp(HostPhase::Completed { summary: "nginx 已在 80 端口运行".into(), checks: vec![] });
        let it = history_item(&row(&done, 61_000), ChangeSummary::default(), vec![]).unwrap();
        assert_eq!((it.state.as_str(), it.goal.as_str(), it.label.as_str()), ("completed", "装好 nginx", "本地终端"));
        assert_eq!((it.started_at, it.ended_at, it.duration_ms), (1_000, 61_000, 60_000));
        assert_eq!(it.summary.as_deref(), Some("nginx 已在 80 端口运行"));
        let failed = cp(HostPhase::Failed { error: "模型不可用".into() });
        let it = history_item(&row(&failed, 2_000), ChangeSummary::default(), vec![]).unwrap();
        assert_eq!((it.state.as_str(), it.error.as_deref()), ("failed", Some("模型不可用")));
        assert!(history_item(&row(&cp(HostPhase::Running), 2_000), ChangeSummary::default(), vec![]).is_none());
        let bad = HostSessionRow { contract_id: "x".into(), state: "completed".into(), json: "{".into(), updated_at: 1 };
        assert!(history_item(&bad, ChangeSummary::default(), vec![]).is_none());
        // 前端字段名
        let v = serde_json::to_value(&it).unwrap();
        assert!(v.get("contractId").is_some() && v.get("durationMs").is_some() && v.get("changes").is_some());
    }

    #[test]
    fn changes_prefer_snapshot_then_live_decisions() {
        let (store, _) = crate::store::tests::temp_store_at("host-changes-for");
        let mut rec = super::super::host::record::hosted_decision(
            &super::super::host::HostCtx::bare("c1", "t1", "g"),
            "t1",
            "exec",
            "systemctl restart nginx",
            "",
            &super::super::host::gate::Gate::Allow,
            None,
            1,
        );
        store.steward_decision_insert(1, "c1", "t1", "", &serde_json::to_string(&rec).unwrap(), "applied").unwrap();
        rec.chosen.text = "apt install x".into();
        rec.contract_id = "c2".into();
        store.steward_decision_insert(2, "c2", "t1", "", &serde_json::to_string(&rec).unwrap(), "applied").unwrap();
        let live = changes_for(&store, "c1");
        assert_eq!((live.commands, live.services.clone()), (1, vec!["重启 nginx".to_string()]));
        snapshot_changes(&store, "c1");
        store.steward_decisions_clear().unwrap();
        assert_eq!(changes_for(&store, "c1"), live, "决策被裁掉后仍能读到定格的摘要");
        assert!(changes_for(&store, "c2").is_empty());
    }
}
