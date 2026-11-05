//! Skill（Phase 3.1）暴露给前端的命令。模型与校验见 `agent::skill`，存储见 `store::skills`。
//!
//! 命名必须是 snake_case（Tauri 2 直接拿函数名当 IPC 名字）。
//!
//! **刻意不暴露**：`skill_record_run_result`（成功 / 失败计数）。计数是 4.4 信任分级的依据，
//! 只能由后端回放引擎在真跑完后记，不能让前端自己改。创建 / 更新也无法带计数
//! （`SkillInput` 里没有这些字段）。

use tauri::State;

use super::skill::{self, Skill, SkillInput};
use crate::cmd::AppState;

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 列出 skill（最近更新在前）。`query` 非空时按名称 / 描述子串搜索。
#[tauri::command]
pub fn skill_list(query: Option<String>, state: State<'_, AppState>) -> Vec<Skill> {
    state.store.skill_list(query.as_deref())
}

#[tauri::command]
pub fn skill_get(id: String, state: State<'_, AppState>) -> Option<Skill> {
    state.store.skill_get(&id)
}

/// 新建。后端铸造 id / 时间戳 / 计数；风险会被自动抬到不低于评估值；校验失败返回全部问题（换行分隔）。
#[tauri::command]
pub fn skill_create(input: SkillInput, state: State<'_, AppState>) -> Result<Skill, String> {
    state.store.skill_create(input)
}

/// 更新内容（保留 id / 创建时间 / 来源；回放相关内容变了则计数清零）。
#[tauri::command]
pub fn skill_update(id: String, input: SkillInput, state: State<'_, AppState>) -> Result<Skill, String> {
    state.store.skill_update(&id, input)
}

/// 删除。返回是否真删了。
#[tauri::command]
pub fn skill_delete(id: String, state: State<'_, AppState>) -> Result<bool, String> {
    state.store.skill_delete(&id)
}

/// 只校验、不保存：给编辑器做实时提示。返回问题列表（空 = 通过）。
/// 校验的是规整（trim、风险抬高）之后的内容，与真正保存时一致。
#[tauri::command]
pub fn skill_validate(input: SkillInput) -> Vec<String> {
    let s = input.into_skill(String::new(), now());
    skill::validate(&s).err().unwrap_or_default()
}

/// 导出为文本（带版本头的美化 JSON）。运行统计不导出。前端负责写文件。
#[tauri::command]
pub fn skill_export(id: String, state: State<'_, AppState>) -> Result<String, String> {
    let s = state.store.skill_get(&id).ok_or_else(|| "skill 不存在".to_string())?;
    Ok(skill::export_text(&s))
}

/// 解析并校验导入文本，**不保存**：给「导入前预览 / 改名」用。
/// 返回的是一份新 id、来源 `imported`、计数为 0 的 skill。
#[tauri::command]
pub fn skill_import_preview(text: String) -> Result<Skill, String> {
    skill::import_text(&text, uuid::Uuid::new_v4().to_string(), now()).map_err(|e| e.join("\n"))
}

/// 导入并保存。不受信输入：严格解析、重新校验、风险重评、计数为 0，永远不会自带信任。
#[tauri::command]
pub fn skill_import(text: String, state: State<'_, AppState>) -> Result<Skill, String> {
    let s = skill::import_text(&text, uuid::Uuid::new_v4().to_string(), now()).map_err(|e| e.join("\n"))?;
    state.store.skill_insert(s)
}
