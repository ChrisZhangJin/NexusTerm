//! Skill（Phase 3.1）暴露给前端的命令。模型与校验见 `agent::skill`，存储见 `store::skills`。
//!
//! 命名必须是 snake_case（Tauri 2 直接拿函数名当 IPC 名字）。
//!
//! **刻意不暴露**：`skill_record_run_result`（成功 / 失败计数）。计数是 4.4 信任分级的依据，
//! 只能由后端回放引擎在真跑完后记，不能让前端自己改。创建 / 更新也无法带计数
//! （`SkillInput` 里没有这些字段）。

use tauri::State;

use super::llm::{self, ChatMessage};
use super::load_config;
use super::recorder::RecordingDraft;
use super::skill::{self, Skill, SkillInput};
use super::skill_refine::{self, RefineLlm, RefinedSkillDraft};
use super::AgentAccount;
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

/// 生产环境的 [`RefineLlm`]：走当前激活账号，复用 Agentic 循环用的 `llm::complete_reply`
/// （同一套 provider 判定 / 非流式补全 / 中文错误翻译；不带 tools）。
struct AccountLlm(AgentAccount);

impl RefineLlm for AccountLlm {
    async fn complete(&mut self, system: &str, messages: &[ChatMessage]) -> Result<String, String> {
        llm::complete_reply(&self.0, system, messages, &[], None)
            .await
            .map(|r| r.text)
            .map_err(|e| e.to_string())
    }
}

/// 3.3：让 Agent 把录制草稿整理成 skill 草稿（参数 / 期望 / 验证 / 前置条件）。**不保存**：
/// 返回的 `draft` 交给前端 `SkillManager` 的 `initialDraft`，由用户检查、编辑、明确点保存。
///
/// 草稿按当前账号的 `redact` 标志脱敏后才出网；模型输出一律过 `skill::validate` / `normalize` /
/// `assess_risk` 并被夹紧（来源 recorded、主机范围、风险只升不降、无 secret 默认值……），
/// 校验失败会带着问题重试一次，仍不行就退回机械草稿。细节见 `agent::skill_refine`。
/// `hint` 是用户的补充说明（可选）。总开关未启用 / 没配 Key 时返回中文错误。
#[tauri::command]
pub async fn skill_refine_draft(
    draft: RecordingDraft,
    hint: Option<String>,
    state: State<'_, AppState>,
) -> Result<RefinedSkillDraft, String> {
    let cfg = load_config(&state.store);
    llm::validate(&cfg).map_err(|e| e.to_string())?;
    let acct = cfg.active_account().clone();
    let redact_on = acct.redact;
    let mut llm = AccountLlm(acct);
    skill_refine::refine(&mut llm, &draft, hint.as_deref(), redact_on).await
}
