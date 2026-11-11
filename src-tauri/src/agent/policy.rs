//! P3 `PolicyEngine`：把「能不能自动写」从 `decision.rs` 里散落的硬编码收敛成一张授权契约。
//!
//! 设计见 `docs/nexus-agent-steward-design.md` §6.1 / §6.2。与 P2 的关系：
//! - `decision::compile` 负责**生成**有依据的回复（意向 / 剧本 / KB）；
//! - 本模块负责**授权**：按「风险 × 可逆性 × 作用域 × host」决定这条回复能不能自动注入，
//!   以及要不要降级成升级卡、要不要给撤销窗口。
//!
//! 三条硬底线（任何档位都不可越过）：
//! 1. `Authority.deny` 是硬禁，命中即 `Deny`，不写、不弹卡、只留审计；
//! 2. 密码 / 永久扩权永远 fail-closed（`allow_permanent_grant` 默认 false）；
//! 3. `host_in_scope == false` 时一律降到 `Approve`，复用 `skill_run::check_scope` 的判据。
//!
//! 整个模块是纯函数，不碰 Tauri / SQLite / PTY，先写单测再接循环。

use serde::{Deserialize, Serialize};

use super::decision::{CtxRef, DecisionAction, DecisionRecord, EscalationCard};
use super::situation::{Choice, Situation, SituationKind};

pub const TIER_AUTO: &str = "auto";
pub const TIER_NOTIFY: &str = "notify";
pub const TIER_APPROVE: &str = "approve";

pub const DEFAULT_UNDO_WINDOW_MS: u64 = 30_000;

/// 签托管契约时的授权参数。落在 `StewardContract.authority` 里，可热更新，不重启循环。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Authority {
    /// auto | notify | approve
    pub tier: String,
    /// 硬禁的动作类别 / 关键字；命中即拒绝，任何 allow 都不能覆盖
    pub deny: Vec<String>,
    /// 允许从 Notify 提到 Auto 的动作类别（例如「这台测试机上 cargo test 随便跑」）
    pub allow: Vec<String>,
    /// 是否允许自动选中「don't ask again」这类永久扩权选项。默认且强烈建议 false
    pub allow_permanent_grant: bool,
    /// Notify 档的撤销窗口；窗口内可一键回滚到「我自己来」
    pub undo_window_ms: u64,
    /// 允许写入的 host 列表；空 = 只由 `host_in_scope` 外部判据决定
    pub host_scope: Vec<String>,
    /// 本次托管的预算（从「本轮」提升到「整个契约」，P4 的 Timebox 会用）
    pub max_steps: u32,
    pub max_minutes: u32,
    pub max_output_chars: u64,
    /// 意向 deadline（毫秒，0 = 不限）；到期收尾摘要 + 保留现场
    pub timebox_ms: u64,
}

impl Default for Authority {
    fn default() -> Self {
        Self {
            tier: TIER_NOTIFY.to_string(),
            deny: vec![
                "secret".to_string(),
                "permanent_grant".to_string(),
                "force_push".to_string(),
                "delete".to_string(),
            ],
            allow: Vec::new(),
            allow_permanent_grant: false,
            undo_window_ms: DEFAULT_UNDO_WINDOW_MS,
            host_scope: Vec::new(),
            max_steps: 60,
            max_minutes: 120,
            max_output_chars: 2_000_000,
            timebox_ms: 0,
        }
    }
}

impl Authority {
    /// 前端传来的档位字符串一律归一，未知值回落到最保守的 notify。
    pub fn with_tier(mut self, tier: &str) -> Self {
        self.tier = match tier {
            TIER_AUTO | TIER_APPROVE => tier.to_string(),
            _ => TIER_NOTIFY.to_string(),
        };
        self
    }

    pub fn clamped(mut self) -> Self {
        self.undo_window_ms = self.undo_window_ms.min(300_000);
        self.max_steps = self.max_steps.clamp(1, 500);
        self.max_minutes = self.max_minutes.clamp(1, 24 * 60);
        self.max_output_chars = self.max_output_chars.clamp(1_000, 100_000_000);
        self.timebox_ms = self.timebox_ms.min(24 * 60 * 60 * 1000);
        self
    }

    pub fn denies(&self, token: &str) -> bool {
        self.deny.iter().any(|d| d == token)
    }

    pub fn allows(&self, token: &str) -> bool {
        self.allow.iter().any(|a| a == token)
    }
}

/// 动作风险分级。设计 §6.1 的「风险 × 可逆性」落成一个可比较的枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    /// 读屏 / 观察 / 等待
    Read,
    /// 回答一个提问、翻页、催进度：不直接改文件系统状态
    Reversible,
    /// 写文件 / 装依赖 / commit / 重启子任务等可回滚但影响外部状态
    Irreversible,
    /// 删除 / force push / 生产写操作
    Destructive,
    /// 「don't ask again」式永久扩权
    Permanent,
    /// 密码 / 凭据，永不进模型，永不代填
    Secret,
}

impl Risk {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Reversible => "reversible",
            Self::Irreversible => "irreversible",
            Self::Destructive => "destructive",
            Self::Permanent => "permanent",
            Self::Secret => "secret",
        }
    }
}

/// 破坏性动作的关键字。命中后即便档位是 auto 也降到 Approve。
const DESTRUCTIVE_TERMS: &[&str] = &[
    "rm -rf", "drop table", "drop database", "push -f", "push --force",
    "--force", "format ", "mkfs", "truncate", "git reset --hard", "删除", "格式化",
    "生产环境", "prod ", "shutdown", "reboot", "kill -9",
];

/// 永久扩权的文字特征；只在拿不到结构化选项时兜底使用。
const PERMANENT_TERMS: &[&str] = &["don't ask again", "always allow", "永久", "不再询问"];

/// 动作选中的那个屏幕选项（按编号精确匹配）。拿不到就说明是自由文本或伪造编号。
fn chosen_option<'a>(action: &DecisionAction, sit: &'a Situation) -> Option<&'a Choice> {
    let idx = action.option_index.as_deref().filter(|s| !s.is_empty())?;
    sit.options.iter().find(|o| o.index.as_deref() == Some(idx))
}

/// 真正会生效的语义文本：选了编号就是那个选项的原文，否则是动作文本本身。
/// 风险与 deny 关键字都只看它——「本次允许：Yes（不选择永久授权）」这种说明文字里
/// 出现「永久」二字，不能让本次允许被当成永久扩权一起拒掉。
fn effective_payload(action: &DecisionAction, sit: &Situation) -> String {
    match chosen_option(action, sit) {
        Some(o) => o.text.to_lowercase(),
        None => action.text.to_lowercase(),
    }
}

/// 从动作与局面推断风险。纯函数：同一条决策多次调用结果必须一致。
/// 有结构化选项时以选项元数据（`grants_permanent`）为准，选项原文的永久特征也算（fail-closed）；
/// 没有选项可依时才扫动作文本。
pub fn risk_of(action: &DecisionAction, sit: &Situation) -> Risk {
    if action.kind == "secret" || sit.kind == SituationKind::Password {
        return Risk::Secret;
    }
    if !action.injectable || matches!(action.kind.as_str(), "observe" | "wait") {
        return Risk::Read;
    }
    let payload = effective_payload(action, sit);
    let permanent = match chosen_option(action, sit) {
        Some(o) => o.grants_permanent || PERMANENT_TERMS.iter().any(|t| payload.contains(t)),
        None => PERMANENT_TERMS.iter().any(|t| payload.contains(t)),
    };
    if permanent {
        return Risk::Permanent;
    }
    if DESTRUCTIVE_TERMS.iter().any(|t| payload.contains(t)) {
        return Risk::Destructive;
    }
    // 权限提示的「本次允许」会改变会话状态，但作用域限于单会话，按可逆处理
    if sit.kind == SituationKind::Permission {
        return Risk::Reversible;
    }
    Risk::Irreversible
}

/// 策略裁决。前端据此渲染状态灯与撤销按钮。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum PolicyVerdict {
    /// 自主执行，事后可查
    Auto,
    /// 自主执行 + 立即通知 + 短窗口可撤销
    Notify { undo_ms: u64 },
    /// 先给建议、等人一键确认（带超时默认动作）
    Approve { reason: String },
    /// 硬禁：不写、不弹卡、只留审计
    Deny { reason: String },
}

impl PolicyVerdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Notify { .. } => "notify",
            Self::Approve { .. } => "approve",
            Self::Deny { .. } => "deny",
        }
    }

    pub fn allows_write(&self) -> bool {
        matches!(self, Self::Auto | Self::Notify { .. })
    }
}

/// 授权核心。**顺序即设计 §6.1**：硬禁 → 密码/永久 → host 作用域 → 档位 → 风险。
pub fn evaluate(
    auth: &Authority,
    rec: &DecisionRecord,
    sit: &Situation,
    host_in_scope: bool,
) -> PolicyVerdict {
    let risk = risk_of(&rec.chosen, sit);
    let payload = effective_payload(&rec.chosen, sit);
    // 结构化标签：动作类别、风险档，以及 deny 列表里约定的 secret / permanent_grant
    let mut tags: Vec<&str> = vec![rec.chosen.kind.as_str(), risk.as_str()];
    match risk {
        Risk::Secret => tags.push("secret"),
        Risk::Permanent => tags.push("permanent_grant"),
        _ => {}
    }

    // 1. deny 是硬禁，allow 不能覆盖：标签精确命中，或自定义关键字出现在真正生效的文本里
    if tags.iter().any(|t| auth.denies(t))
        || auth
            .deny
            .iter()
            .any(|d| !d.is_empty() && payload.contains(&d.to_lowercase()))
    {
        return PolicyVerdict::Deny {
            reason: format!("命中 Authority.deny（{}）", risk.as_str()),
        };
    }
    // 2. 密码 / 永久扩权：fail-closed，任何档位都不放行
    if risk == Risk::Secret {
        return PolicyVerdict::Deny {
            reason: "密码 / 凭据只走 secret 通道，Steward 不代填".to_string(),
        };
    }
    if risk == Risk::Permanent && !auth.allow_permanent_grant {
        return PolicyVerdict::Deny {
            reason: "会永久扩大授权：默认只允许「本次允许」，绝不误选永久项".to_string(),
        };
    }
    // 3. host_scope fail-closed：作用范围不明一律降档
    if !host_in_scope {
        return PolicyVerdict::Approve {
            reason: "目标主机不在授权作用域内：fail-closed 降为确认档".to_string(),
        };
    }
    // 4. 现场不可信时策略层再复核一次（compile 已经判过，这里是第二道闸）
    if sit.confidence < 0.60
        || sit.stale_ms > 6_000
        || matches!(sit.kind, SituationKind::AltScreen | SituationKind::Unknown)
    {
        return PolicyVerdict::Approve {
            reason: format!(
                "现场置信度不足（{:.2} / 断流 {}ms）：只读观察或先确认",
                sit.confidence, sit.stale_ms
            ),
        };
    }
    // 5. approve 档一律先给卡
    if auth.tier == TIER_APPROVE {
        return PolicyVerdict::Approve {
            reason: "契约档位为「确认」：先给建议与倒计时，再决定是否执行".to_string(),
        };
    }
    // 6. allow 白名单可以把 Notify 提到 Auto
    if auth.allows(&rec.chosen.kind) || auth.allows(risk.as_str()) {
        return PolicyVerdict::Auto;
    }
    // 7. 破坏性动作在任何档位都要人确认（即便 auto）
    if risk >= Risk::Destructive {
        return PolicyVerdict::Approve {
            reason: format!("动作风险 {}：需要人一键确认", risk.as_str()),
        };
    }
    // 8. notify 档：执行但留撤销窗口
    if auth.tier == TIER_NOTIFY {
        return PolicyVerdict::Notify {
            undo_ms: auth.undo_window_ms,
        };
    }
    PolicyVerdict::Auto
}

/// 把裁决写回决策记录：改档位 / 结果 / 升级卡，并补一条可审计的 policy 依据。
///
/// 返回裁决本身，调用方据此决定注入、开撤销窗口还是只记事件。
pub fn authorize(
    auth: &Authority,
    rec: &mut DecisionRecord,
    sit: &Situation,
    host_in_scope: bool,
) -> PolicyVerdict {
    let verdict = evaluate(auth, rec, sit, host_in_scope);
    let marker = format!("policy:{}", verdict.as_str());
    rec.ctx_refs.push(CtxRef::of(
        "policy",
        &marker,
        match &verdict {
            PolicyVerdict::Auto => "PolicyEngine：授权档允许自主执行".to_string(),
            PolicyVerdict::Notify { undo_ms } => {
                format!("PolicyEngine：自主执行，{undo_ms}ms 内可撤销")
            }
            PolicyVerdict::Approve { reason } => {
                format!("PolicyEngine 降为确认：{reason}")
            }
            PolicyVerdict::Deny { reason } => {
                format!("PolicyEngine 硬禁：{reason}")
            }
        },
    ));

    match &verdict {
        PolicyVerdict::Auto => {
            rec.tier = TIER_AUTO.to_string();
        }
        PolicyVerdict::Notify { .. } => {
            rec.tier = TIER_NOTIFY.to_string();
        }
        PolicyVerdict::Approve { reason } => {
            rec.tier = TIER_APPROVE.to_string();
            rec.outcome = "escalated".to_string();
            rec.escalation = Some(EscalationCard::new(rec.chosen.clone(), rec.ctx_refs.clone(), rec.ts_ms));
            rec.rationale = format!("{}；{}", rec.rationale, reason);
        }
        PolicyVerdict::Deny { reason } => {
            // 硬禁不是「转述问题给用户」，而是明确不写、把原因留在审计里。
            rec.tier = TIER_APPROVE.to_string();
            rec.outcome = "rejected".to_string();
            rec.chosen = DecisionAction::observe(reason.clone());
            rec.escalation = None;
            rec.rationale = format!("{}；PolicyEngine 硬禁：{}", rec.rationale, reason);
        }
    }
    verdict
}

/// Notify 档的撤销窗口记录。真正「撤销」不是把已敲的键收回（做不到），
/// 而是：立刻转观察态避免后续连发 + 把这条决策标为 rolled_back + 写一条 Learned 偏好。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UndoEntry {
    pub seq: i64,
    pub console_id: String,
    pub action: DecisionAction,
    pub expires_at: i64,
    pub used: bool,
}

impl UndoEntry {
    pub fn new(rec: &DecisionRecord, undo_ms: u64) -> Self {
        Self {
            seq: rec.seq,
            console_id: rec.console_id.clone(),
            action: rec.chosen.clone(),
            expires_at: rec.ts_ms + undo_ms as i64,
            used: false,
        }
    }

    pub fn live(&self, now: i64) -> bool {
        !self.used && now <= self.expires_at
    }
}

/// host 作用域判据（fail-closed）。
///
/// `host_scope` 非空时必须命中列表；未显式限定时，仍要求这个 console 是后端
/// 真实存在的本地 / SSH 会话——屏幕帧可以被伪造，会话注册表不能。
/// 复用 `skill_run::check_scope` 的语义（无可比对名字一律拒绝），但不强依赖
/// `HostIdentity`，让 Steward 循环在纯逻辑层就能调用。
pub fn host_in_scope(auth: &Authority, host_key: &str, known_console: bool) -> bool {
    if auth.host_scope.is_empty() {
        return known_console;
    }
    auth.host_scope
        .iter()
        .any(|h| h == "*" || h == host_key || (!h.is_empty() && host_key.contains(h.as_str())))
}

/// 给前端看的策略说明（真值，不再是 P2 的静态数组）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyView {
    pub authority: Authority,
    pub verdicts: Vec<&'static str>,
    pub risks: Vec<&'static str>,
    pub hard_rules: Vec<String>,
}

pub fn policy_view(auth: &Authority) -> PolicyView {
    PolicyView {
        authority: auth.clone(),
        verdicts: vec!["auto", "notify", "approve", "deny"],
        risks: vec![
            "read", "reversible", "irreversible", "destructive", "permanent", "secret",
        ],
        hard_rules: vec![
            "Authority.deny 命中即拒绝，allow 不能覆盖".to_string(),
            "密码 / 凭据永不代填，也不进模型上下文".to_string(),
            if auth.allow_permanent_grant {
                "⚠️ 已允许自动永久扩权（不建议）".to_string()
            } else {
                "「don't ask again」式永久扩权默认硬禁".to_string()
            },
            "host 不在作用域内一律降为确认档".to_string(),
            "断流 / 低置信度 / 未知全屏只允许只读".to_string(),
            "Notify 档执行后留 30s 撤销窗口".to_string(),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::decision::{DecisionAction, DecisionRecord};
    use super::super::situation::{Choice, Program};

    fn sit(kind: SituationKind, permanent: bool) -> Situation {
        Situation {
            frame_id: 1,
            screen_hash: "abcd1234".into(),
            program: Program::ClaudeCode,
            kind,
            topic: Some("settings:remember-check".into()),
            options: vec![
                Choice { index: Some("1".into()), text: "Yes".into(), grants_permanent: false },
                Choice { index: Some("2".into()), text: "Yes, and don't ask again".into(), grants_permanent: permanent },
            ],
            confidence: 0.93,
            tail: "Do you want to change the setting?".into(),
            evidence: vec!["question:claude_code".into()],
            stale_ms: 0,
            signature: "claude_code|question|settings:remember-check".into(),
        }
    }

    fn rec(kind: &str, text: &str, idx: Option<&str>, injectable: bool) -> DecisionRecord {
        DecisionRecord {
            seq: 7,
            ts_ms: 1_000,
            contract_id: "c-1".into(),
            console_id: "tab-1".into(),
            situation_signature: "sig".into(),
            situation_hash: "abcd1234".into(),
            ctx_refs: Vec::new(),
            candidates: Vec::new(),
            chosen: DecisionAction { kind: kind.into(), text: text.into(), option_index: idx.map(str::to_string), injectable },
            rationale: "依据意向".into(),
            confidence: 0.9,
            tier: "auto".into(),
            llm_used: false,
            outcome: "proposed".into(),
            escalation: None,
            receipt: None,
        }
    }

    #[test]
    fn auto_tier_allows_plain_answer() {
        let a = Authority::default().with_tier("auto");
        let r = rec("answer", "No — 不要改设置，直接在代码里改", None, true);
        assert_eq!(evaluate(&a, &r, &sit(SituationKind::Question, false), true), PolicyVerdict::Auto);
    }

    #[test]
    fn notify_tier_has_undo_window() {
        let a = Authority::default().with_tier("notify");
        let r = rec("answer", "ok", None, true);
        let v = evaluate(&a, &r, &sit(SituationKind::Question, false), true);
        assert_eq!(v, PolicyVerdict::Notify { undo_ms: DEFAULT_UNDO_WINDOW_MS });
        let e = UndoEntry::new(&r, DEFAULT_UNDO_WINDOW_MS);
        assert!(e.live(1_000 + 20_000));
        assert!(!e.live(1_000 + 40_000));
    }

    #[test]
    fn approve_tier_always_escalates() {
        let a = Authority::default().with_tier("approve");
        let r = rec("answer", "ok", None, true);
        assert!(matches!(evaluate(&a, &r, &sit(SituationKind::Question, false), true), PolicyVerdict::Approve { .. }));
    }

    #[test]
    fn permanent_grant_is_hard_denied() {
        let a = Authority::default().with_tier("auto");
        let r = rec("answer", "Yes", Some("2"), true);
        let v = evaluate(&a, &r, &sit(SituationKind::Permission, true), true);
        assert!(matches!(v, PolicyVerdict::Deny { .. }), "got {v:?}");
        let mut r2 = r.clone();
        authorize(&a, &mut r2, &sit(SituationKind::Permission, true), true);
        assert_eq!(r2.outcome, "rejected");
        assert!(!r2.chosen.injectable);
    }

    #[test]
    fn password_is_hard_denied_even_with_allow_all() {
        let mut a = Authority::default().with_tier("auto");
        a.deny.clear();
        a.allow.push("secret".into());
        a.allow_permanent_grant = true;
        let r = rec("secret", "本人输入", None, false);
        assert!(matches!(evaluate(&a, &r, &sit(SituationKind::Password, false), true), PolicyVerdict::Deny { .. }));
    }

    #[test]
    fn host_out_of_scope_downgrades_to_approve() {
        let a = Authority::default().with_tier("auto");
        let r = rec("answer", "ok", None, true);
        assert!(matches!(evaluate(&a, &r, &sit(SituationKind::Question, false), false), PolicyVerdict::Approve { .. }));
    }

    #[test]
    fn destructive_action_requires_confirmation_even_in_auto() {
        let a = Authority::default().with_tier("auto");
        let r = rec("answer", "git push -f origin main", None, true);
        assert_eq!(risk_of(&r.chosen, &sit(SituationKind::Question, false)), Risk::Destructive);
        assert!(matches!(evaluate(&a, &r, &sit(SituationKind::Question, false), true), PolicyVerdict::Approve { .. }));
    }

    #[test]
    fn allow_list_promotes_notify_to_auto() {
        let mut a = Authority::default().with_tier("notify");
        a.allow.push("answer".into());
        let r = rec("answer", "cargo test", None, true);
        assert_eq!(evaluate(&a, &r, &sit(SituationKind::Question, false), true), PolicyVerdict::Auto);
    }

    #[test]
    fn allow_once_passes_while_permanent_option_is_denied() {
        let a = Authority::default().with_tier("auto");
        // compile() 产出的「本次允许」说明文字里带「永久」二字，但选中的是非永久的 1 号
        let once = rec("answer", "本次允许：Yes（不选择永久授权）", Some("1"), true);
        let s = sit(SituationKind::Permission, true);
        assert_eq!(risk_of(&once.chosen, &s), Risk::Reversible);
        assert!(!matches!(evaluate(&a, &once, &s, true), PolicyVerdict::Deny { .. }));
        // 2 号带 grants_permanent：无论动作文本怎么写都拒
        let perm = rec("answer", "just yes", Some("2"), true);
        assert!(matches!(evaluate(&a, &perm, &s, true), PolicyVerdict::Deny { .. }));
        // 分类器漏标 grants_permanent，但选项原文是永久特征：仍然 fail-closed
        let s2 = sit(SituationKind::Permission, false);
        assert_eq!(risk_of(&perm.chosen, &s2), Risk::Permanent);
        // 编号对不上任何选项（伪造）时退回扫动作文本
        let forged = rec("answer", "2. Yes, and don't ask again", Some("9"), true);
        assert_eq!(risk_of(&forged.chosen, &s), Risk::Permanent);
        // 密码依旧硬禁
        let pw = rec("secret", "本人输入", None, false);
        assert!(matches!(evaluate(&a, &pw, &sit(SituationKind::Password, false), true), PolicyVerdict::Deny { .. }));
    }

    #[test]
    fn deny_cannot_be_overridden_by_allow() {
        let mut a = Authority::default().with_tier("auto");
        a.deny.push("answer".into());
        a.allow.push("answer".into());
        let r = rec("answer", "ok", None, true);
        assert!(matches!(evaluate(&a, &r, &sit(SituationKind::Question, false), true), PolicyVerdict::Deny { .. }));
    }

    #[test]
    fn authorize_writes_audit_ref_and_card() {
        let a = Authority::default().with_tier("approve");
        let mut r = rec("answer", "ok", None, true);
        authorize(&a, &mut r, &sit(SituationKind::Question, false), true);
        assert_eq!(r.outcome, "escalated");
        assert!(r.escalation.is_some());
        assert!(r.ctx_refs.iter().any(|x| x.source == "policy" && x.id == "policy:approve"));
        assert!(r.rationale.contains("确认"));
    }

    #[test]
    fn host_scope_is_fail_closed() {
        let a = Authority::default();
        assert!(host_in_scope(&a, "local:tab-1", true));
        assert!(!host_in_scope(&a, "local:tab-1", false), "会话不存在就不该自动写");
        let mut b = Authority::default();
        b.host_scope = vec!["web1".into()];
        assert!(host_in_scope(&b, "ssh:web1:22", true));
        assert!(!host_in_scope(&b, "ssh:prod-db:22", true));
        b.host_scope = vec!["*".into()];
        assert!(host_in_scope(&b, "anything", true));
    }

    #[test]
    fn clamped_authority_stays_in_bounds() {
        let a = Authority { undo_window_ms: u64::MAX, max_steps: 0, max_minutes: 99999, max_output_chars: 0, timebox_ms: u64::MAX, ..Default::default() }.clamped();
        assert_eq!(a.undo_window_ms, 300_000);
        assert_eq!(a.max_steps, 1);
        assert_eq!(a.max_minutes, 24 * 60);
        assert_eq!(a.max_output_chars, 1_000);
        assert!(a.timebox_ms < u64::MAX);
        assert_eq!(Authority::default().with_tier("???").tier, "notify");
    }
}
