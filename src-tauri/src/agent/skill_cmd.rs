//! Skill（Phase 3.1）暴露给前端的命令。模型与校验见 `agent::skill`，存储见 `store::skills`。
//!
//! 命名必须是 snake_case（Tauri 2 直接拿函数名当 IPC 名字）。
//!
//! **刻意不暴露**：`skill_record_run` / `skill_record_run_result`（成功 / 失败计数与信任阶梯）。
//! 计数是 4.4 信任分级的依据，只能由后端回放引擎在真跑完后记，不能让前端自己改。
//! 前端只读得到 [`skill_trust`] 算出的结论。创建 / 更新也无法带计数
//! （`SkillInput` 里没有这些字段）。

use tauri::State;

use super::llm::{self, ChatMessage};
use super::load_config;
use super::recorder::RecordingDraft;
use super::skill::{self, Skill, SkillInput};
use super::skill_refine::{self, RefineLlm, RefinedSkillDraft};
use super::skill_run::{self, check_scope, HostIdentity};
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

/// 4.2/4.3：在指定会话上回放一份 skill（UI「运行」按钮触发）。
///
/// 与 `agent_run` 走同一套运行时登记（同一会话同时只有一轮、面板「停止」可取消、控制权仲裁、审批 / 密码卡片），
/// 步骤进度以 `skill` 类步骤卡推给面板，结束时发一条 `answer` 收尾。secret 参数的值永远不经过这里：
/// 前端不会传，传了也不会被代入（只能经密码通道由用户输入）。
/// 返回完整的运行报告（含偏离记录与对 skill 的修改建议；不会自动改 skill）。
#[tauri::command]
pub async fn skill_run(
    skill_id: String,
    params: std::collections::BTreeMap<String, String>,
    session_id: String,
    target_label: Option<String>,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<skill_run::SkillRunReport, String> {
    let skill = state.store.skill_get(&skill_id).ok_or_else(|| "技能不存在".to_string())?;
    let cfg = load_config(&state.store);
    let opts = skill_run::RunOpts::from_config(&cfg);
    let ctx = skill_run::SkillRunCtx { app: &app, state: &state, runtime: &state.act, cfg: &cfg };
    let label = target_label.unwrap_or_default();
    skill_run::run_skill(&ctx, &skill, &params, &session_id, &label, opts).await
}

/// 一份 skill 给界面看的信任结论（4.4）：档位 + 原因 + 当前会话是否在适用主机范围内。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillTrustView {
    pub id: String,
    #[serde(flatten)]
    pub trust: skill::TrustInfo,
    /// 传了会话才有：`true` 在范围内、`false` 超出（运行会被拒绝）。
    pub scope_ok: Option<bool>,
    /// 超出范围时的原因（中文）。
    pub scope_reason: Option<String>,
}

/// 纯逻辑：把 skill 列表映射成信任视图。`who` 为空则不做范围判断。
pub fn build_trust_views(skills: &[Skill], who: Option<&HostIdentity>) -> Vec<SkillTrustView> {
    skills
        .iter()
        .map(|s| {
            let scope = who.map(|w| check_scope(&s.host_scope, w));
            SkillTrustView {
                id: s.id.clone(),
                trust: skill::trust_info(s),
                scope_ok: scope.as_ref().map(|r| r.is_ok()),
                scope_reason: scope.and_then(|r| r.err()),
            }
        })
        .collect()
}

/// 4.4：所有 skill 的信任档位（只读）。传 `session_id` 时同时给出「这个会话在不在各 skill 的适用主机范围内」。
/// 档位由后端按成功次数 / 风险 / 内容指纹算出，前端无法改写。
#[tauri::command]
pub fn skill_trust(
    session_id: Option<String>,
    target_label: Option<String>,
    state: State<'_, AppState>,
) -> Vec<SkillTrustView> {
    let skills = state.store.skill_list(None);
    let who = session_id
        .filter(|s| !s.trim().is_empty())
        .map(|sid| HostIdentity::resolve(&state, &sid, target_label.as_deref().unwrap_or("")));
    build_trust_views(&skills, who.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::skill::{
        HostScope, OnUnexpected, Risk, SkillStep, TrustLevel, TRUST_MIN_SUCCESSES,
    };

    fn ro_skill(id: &str, scope: HostScope) -> Skill {
        SkillInput {
            name: id.into(),
            description: String::new(),
            params: vec![],
            steps: vec![SkillStep::Exec {
                command: "ls -la".into(),
                timeout_ms: None,
                expect_after: None,
                on_unexpected: OnUnexpected::Ask,
            }],
            verify: None,
            risk: Risk::Low,
            host_scope: scope,
            source: None,
        }
        .into_skill(id.into(), 1)
    }

    fn who(key: Option<&str>) -> HostIdentity {
        HostIdentity { session_id: "s1".into(), label: "web-1 (root)".into(), host_key: key.map(Into::into) }
    }

    #[test]
    fn trust_views_without_a_session_make_no_scope_claim() {
        let skills = vec![ro_skill("a", HostScope::List { hosts: vec!["x".into()] })];
        let v = build_trust_views(&skills, None);
        assert_eq!(v.len(), 1);
        assert_eq!((v[0].id.as_str(), v[0].trust.level), ("a", TrustLevel::Confirm));
        assert_eq!((v[0].scope_ok, v[0].scope_reason.clone()), (None, None));
    }

    #[test]
    fn trust_views_flag_out_of_scope_skills_with_a_reason() {
        let skills = vec![
            ro_skill("any", HostScope::Any),
            ro_skill("prod", HostScope::List { hosts: vec!["*.prod.example.com".into()] }),
            ro_skill("web", HostScope::List { hosts: vec!["root@web.example.com".into()] }),
        ];
        let w = who(Some("ssh:root@web.example.com:22"));
        let v = build_trust_views(&skills, Some(&w));
        assert_eq!(v[0].scope_ok, Some(true));
        assert_eq!(v[1].scope_ok, Some(false));
        let why = v[1].scope_reason.clone().unwrap();
        assert!(why.contains("*.prod.example.com") && why.contains("没有执行任何操作"), "{why}");
        assert_eq!(v[2].scope_ok, Some(true));
        assert!(v[2].scope_reason.is_none());
    }

    #[test]
    fn trust_view_serializes_flat_camel_case_for_the_ui() {
        let skills = vec![ro_skill("a", HostScope::Any)];
        let v = build_trust_views(&skills, Some(&who(None)));
        let j = serde_json::to_value(&v[0]).unwrap();
        assert_eq!(j["id"], "a");
        assert_eq!(j["level"], "confirm");
        assert_eq!(j["streak"], 0);
        assert_eq!(j["needed"], TRUST_MIN_SUCCESSES);
        assert!(j["reason"].is_string());
        assert_eq!(j["scopeOk"], true);
        assert!(j["scopeReason"].is_null());
    }
}
