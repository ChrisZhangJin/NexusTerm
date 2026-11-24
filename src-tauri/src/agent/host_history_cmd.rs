//! 「结果可追溯、可回看」：历史托管、实际改动、改文件前的原文备份与还原。
//!
//! - `host_history(limit)`：最近结束的托管（completed / failed / stopped），新的在前；
//! - `host_changes(contract_id)`：一次托管的「实际改动」（按决策记录提取，见 `host::changes`）+ 可还原的备份；
//! - `host_restore_backup(contract_id, backup_id)`：把备份的原文经同一个终端的文件通道（本地 / SFTP）写回去。
//!
//! 改动摘要在托管每次落盘时增量并进 `host_changes` 表（决策表全局只留最近 500 条，早期改动 / 老托管靠它）。

use serde::Serialize;
use tauri::{AppHandle, Manager, State};

use crate::cmd::AppState;

use super::checkpoint::Spent;
use super::host::backup::{self, BackupEntry};
use super::host::changes::ChangeSummary;
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

/// 存下的摘要（没有就从空的开始）+ 它之后的新决策记录。不落盘。
fn merged_changes(store: &crate::store::Store, contract_id: &str) -> (ChangeSummary, bool) {
    let mut s = store
        .host_changes_get(contract_id)
        .and_then(|j| serde_json::from_str::<ChangeSummary>(&j).ok())
        .unwrap_or_else(|| ChangeSummary { merged_seq: Some(0), ..Default::default() });
    let Some(after) = s.merged_seq else { return (s, false) };
    let rows = store.steward_decisions_since(contract_id, after);
    let changed = s.absorb_rows(contract_id, rows.iter().map(|(q, j)| (*q, j.as_str())));
    (s, changed)
}

/// 存下的摘要并上之后的新记录（旧版本结束时定格的原样返回）。
pub(crate) fn changes_for(store: &crate::store::Store, contract_id: &str) -> ChangeSummary {
    merged_changes(store, contract_id).0
}

/// 托管每次落盘时调用：把新的决策记录增量并进存下的改动摘要（决策表全局只留最近 500 条，
/// 早期的改动靠这份摘要留住）。只读 seq 之后的新记录；没有新记录不写。
pub(crate) fn snapshot_changes(store: &crate::store::Store, contract_id: &str) {
    let (s, changed) = merged_changes(store, contract_id);
    if !changed {
        return;
    }
    match serde_json::to_string(&s) {
        Ok(j) => {
            if let Err(e) = store.host_changes_put(contract_id, &j, now_ms()) {
                tracing::warn!("托管改动摘要落盘失败：{e}");
            }
        }
        Err(e) => tracing::warn!("托管改动摘要序列化失败：{e}"),
    }
}

/// act.rs 写文件成功后调用：本次托管第一次碰这个文件时记一条（原文能备份就存一份）。失败只记日志，不影响托管。
pub(crate) fn back_up_original(app: &AppHandle, contract_id: &str, console_id: &str, path: &str, touch: backup::Touch<'_>) {
    if let Err(e) = backup::record(&backup_root(app), contract_id, console_id, path, touch, now_ms()) {
        // 不带内容、不带路径以外的东西
        tracing::warn!("托管改文件前的原文备份失败（{path}）：{e}");
    }
}

/// 启动时调用：托管行已被清理掉的，备份目录一并删掉；再按保留上限删掉最旧的（见 `backup` 模块文档）。
pub(crate) fn prune_backups(app: &AppHandle, store: &crate::store::Store) {
    let live = |id: &str| {
        store.host_session_get(id).is_some_and(|r| !matches!(r.state.as_str(), "completed" | "failed" | "stopped" | "expired"))
    };
    let n = backup::prune(&backup_root(app), |id| store.host_session_get(id).is_some(), live);
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
        // 多终端托管：「A + B」
        label: cp.consoles().map(|c| c.label.as_str()).collect::<Vec<_>>().join(" + "),
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

/// 历史托管。读几百行 JSON、逐个解析、扫备份目录，可能要几百毫秒：放到阻塞线程池里做，不占主线程
/// （同步命令跑在主线程上，会卡住界面）。
#[tauri::command]
pub async fn host_history(limit: Option<usize>, app: AppHandle) -> Result<Vec<HostHistoryItem>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let root = backup_root(&app);
        state
            .store
            .host_history_rows(limit.unwrap_or(crate::store::host::HISTORY_KEEP))
            .iter()
            .filter_map(|row| {
                history_item(row, changes_for(&state.store, &row.contract_id), backup::list(&root, &row.contract_id))
            })
            .collect()
    })
    .await
    .map_err(|e| crate::tr!("读取历史托管失败：{e}", "Failed to load autopilot history: {e}"))
}

/// 一次托管的实际改动（没定格就按决策记录现算）+ 备份清单：同上，放到阻塞线程池里。
#[tauri::command]
pub async fn host_changes(contract_id: String, app: AppHandle) -> Result<HostChanges, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        HostChanges {
            changes: changes_for(&state.store, &contract_id),
            backups: backup::list(&backup_root(&app), &contract_id),
        }
    })
    .await
    .map_err(|e| crate::tr!("读取托管改动失败：{e}", "Failed to load autopilot changes: {e}"))
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
        return Err(crate::tr!(
            "当初改「{}」的那个终端已经关闭，没法经它写回。请重新连上那台机器后手动恢复（原文保存在应用数据目录的 host-backups 里）",
            "The terminal that changed \"{}\" has been closed, so it can't be written back through it. Reconnect to that machine and restore it manually (the original is saved under host-backups in the app data directory)",
            entry.path
        ));
    }
    let text = String::from_utf8(bytes).map_err(|_| crate::tr!("备份不是文本，不还原", "The backup isn't text, not restoring"))?;
    let transport = super::files::transport_for(st, &entry.console_id)
        .await
        .map_err(|e| crate::tr!("打不开文件通道：{e}", "Couldn't open the file channel: {e}"))?;
    let ops = super::files::FileOps::new(&*transport, super::files::FileOpsConfig::default());
    // 用户在确认框里点了「还原」：受保护路径（当初也是批准过才写的）照样允许写回
    let opts = super::files::WriteOptions { create_dirs: true, allow_sensitive: true };
    ops.write(&entry.path, &text, &opts)
        .await
        .map_err(|e| crate::tr!("还原「{}」失败：{e}", "Failed to restore \"{}\": {e}", entry.path))?;
    drop(ops);
    backup::mark_restored(&root, &contract_id, &backup_id, now_ms()).ok();
    Ok(crate::tr!("已把「{}」还原成托管改动之前的内容", "Restored \"{}\" to its content before autopilot changed it", entry.path))
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

    #[test]
    fn periodic_snapshots_merge_incrementally_and_survive_trimming() {
        let (store, _) = crate::store::tests::temp_store_at("host-changes-merge");
        let mut rec = super::super::host::record::hosted_decision(
            &super::super::host::HostCtx::bare("c1", "t1", "g"),
            "t1",
            "exec",
            "apt-get install -y nginx",
            "",
            &super::super::host::gate::Gate::Allow,
            None,
            1,
        );
        let put = |rec: &super::super::decision::DecisionRecord| {
            store.steward_decision_insert(1, &rec.contract_id, "t1", "", &serde_json::to_string(rec).unwrap(), "applied").unwrap()
        };
        put(&rec);
        snapshot_changes(&store, "c1");
        snapshot_changes(&store, "c1"); // 没有新记录：不重复计数
        // 早期记录被裁掉，之后又有新动作
        store.steward_decisions_clear().unwrap();
        rec.chosen.text = "systemctl restart nginx && apt-get install -y nginx".into();
        put(&rec);
        // 进行中读：存下的 + 之后的新记录
        let live = changes_for(&store, "c1");
        assert_eq!(live.commands, 2);
        assert_eq!(live.packages, vec!["安装 nginx（apt-get）"]);
        assert_eq!(live.services, vec!["重启 nginx"]);
        snapshot_changes(&store, "c1");
        snapshot_changes(&store, "c1");
        let s = changes_for(&store, "c1");
        assert_eq!((s.commands, s.packages.len(), s.services.len()), (2, 1, 1));
        // 旧版本结束时定格的（没有 merged_seq）：原样返回，不再并
        let legacy = ChangeSummary { commands: 7, ..Default::default() };
        store.host_changes_put("old", &serde_json::to_string(&legacy).unwrap(), 1).unwrap();
        rec.contract_id = "old".into();
        put(&rec);
        snapshot_changes(&store, "old");
        assert_eq!(changes_for(&store, "old"), legacy);
    }
}
