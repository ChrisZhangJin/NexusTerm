//! 托管写入的唯一强制点（设计 §3）：契约 → 代码，而不是只写进提示词。
//!
//! `authorize` 只在 `drive_gate!` 之后调用一次，覆盖 Exec / Keys / Secret / Skill-run / Open /
//! Tab-close；只读动作（读屏、等待、观察、检索）直接放行。检查顺序固定：
//! 1. `Authority.deny`：从动作推出的标签（force_push / delete / secret / permanent_grant）精确命中，
//!    或自定义关键字出现在真正要提交的文本里（与 `policy::evaluate` 同一口径）；
//! 2. Hard 偏好的 `extra.denyPattern` 正则，匹配命令或键入的每一行；
//! 3. 主机范围 `skill_run::check_scope`，fail-closed（认不出主机就拒）；
//! 4. Keys 走 `steward_mode::assess_keys`；
//! 5. Exec / Open 带的命令走 `danger::assess`；
//! 6. 档位：approve → 每次写都 Approve；notify → Notify（allow 标签提升为 Allow）；auto → Allow。
//!
//! 纯函数，不碰运行时。

use std::sync::LazyLock;

use regex::Regex;

use super::super::act::{AgentAction, OpenKind, TabOp};
use super::super::danger;
use super::super::interact;
use super::super::policy::{Authority, TIER_APPROVE, TIER_AUTO};
use super::super::profile::CtxItem;
use super::super::situation::{Situation, SituationKind};
use super::super::skill::HostScope;
use super::super::skill_run::{check_scope, HostIdentity};
use super::super::skilltool::SkillOp;
use super::super::steward_mode::{assess_keys, permanent_pick, strip_esc, typed_lines, KeysVerdict};

pub const TAG_FORCE_PUSH: &str = "force_push";
pub const TAG_DELETE: &str = "delete";
pub const TAG_SECRET: &str = "secret";
pub const TAG_PERMANENT_GRANT: &str = "permanent_grant";

/// Hard 偏好正则的编译上限，防止用户写出病态 pattern 拖慢每一次写入。
const HARD_RE_SIZE_LIMIT: usize = 1 << 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    Allow,
    /// 执行后发通知卡并带撤销窗口
    Notify,
    /// 挂起等人批准（WaitingHuman 审批卡），附原因
    Approve(Vec<String>),
    /// 不写入，把这段话反馈给模型
    Deny(String),
}

/// 一条被代码强制的 Hard 偏好：只取带 `extra.denyPattern` 的。
#[derive(Debug, Clone)]
pub struct HardRule {
    pub pref_id: String,
    pub deny: Regex,
}

impl HardRule {
    /// 从 Hard 偏好里挑出带合法 `denyPattern` 的。pattern 写错的不强制（UI 上标「未强制」）。
    pub fn from_prefs(hard: &[CtxItem]) -> Vec<HardRule> {
        hard.iter()
            .filter(|x| x.strength == "hard")
            .filter_map(|x| {
                let pat = x.extra.get("denyPattern")?.as_str()?.trim();
                if pat.is_empty() {
                    return None;
                }
                let deny = regex::RegexBuilder::new(pat).size_limit(HARD_RE_SIZE_LIMIT).build().ok()?;
                Some(HardRule { pref_id: x.id.clone(), deny })
            })
            .collect()
    }
}

pub struct HostPolicy {
    pub authority: Authority,
    pub hard: Vec<HardRule>,
    /// 被托管 console 所在主机的键（`hostmem::host_key` 的推导）；空 = 认不出
    pub host_key: String,
}

impl HostPolicy {
    pub fn enforced_ids(&self) -> Vec<String> {
        self.hard.iter().map(|r| r.pref_id.clone()).collect()
    }
}

/// 一次写入要评估的东西：类别标签、提交的文本行、要过 danger 的命令、解码后的按键。
struct Write {
    category: &'static str,
    lines: Vec<String>,
    command: Option<String>,
    keys: Option<Vec<u8>>,
}

fn write_of(a: &AgentAction) -> Result<Option<Write>, String> {
    let w = |category, lines: Vec<String>, command| Write { category, lines, command, keys: None };
    Ok(Some(match a {
        AgentAction::Exec { command, .. } => w("exec", vec![command.trim().to_string()], Some(command.clone())),
        AgentAction::Keys { keys, .. } => {
            let bytes = interact::parse_keys(keys).map_err(|e| format!("按键串解析失败，没有写入：{e}"))?;
            Write { category: "keys", lines: typed_lines(&bytes), command: None, keys: Some(bytes) }
        }
        AgentAction::Secret { .. } => w("secret_prompt", vec![], None),
        AgentAction::Skill { call, .. } if call.op == SkillOp::Run => {
            let mut lines: Vec<String> = call.key.iter().cloned().collect();
            lines.extend(call.params.values().cloned());
            w("skill", lines, None)
        }
        AgentAction::Open { command, kind, .. } => {
            let mut lines: Vec<String> = command.iter().map(|c| c.trim().to_string()).collect();
            if let OpenKind::Ssh { session_record_id } = kind {
                lines.push(session_record_id.clone());
            }
            w("open", lines, command.clone())
        }
        AgentAction::Tab { op: TabOp::Close, target, .. } => w("tab_close", target.iter().cloned().collect(), None),
        AgentAction::Tab { op: TabOp::Reconnect, target, .. } => {
            w("tab_reconnect", target.iter().cloned().collect(), None)
        }
        _ => return Ok(None),
    }))
}

static FORCE_PUSH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bgit\b.*\bpush\b.*(\s(-f|--force|--force-with-lease)(\s|=|$)|\s\+\S|\s-[a-z]*f[a-z]*(\s|$))")
        .unwrap()
});
static DELETE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?ix)(^|[\s;&|(`])(rm|rmdir|unlink|shred|srm|del|erase|rd|remove-item|ri)(\s|$)
          |\bgit\s+(clean|branch\s+-d|push\s+\S+\s+:|push\s+.*--delete)
          |\bfind\b.*\s-delete\b
          |\bdrop\s+(table|database|schema)\b
          |\btruncate\b
          |\bdelete\s+from\b
          |\b(kubectl|docker|helm|gh)\b.*\b(delete|rm|rmi|prune|uninstall)\b",
    )
    .unwrap()
});

/// 从动作推出 deny 标签。
fn derive_tags(w: &Write, sit: Option<&Situation>) -> Vec<&'static str> {
    let mut tags = Vec::new();
    if w.lines.iter().any(|l| FORCE_PUSH_RE.is_match(l)) {
        tags.push(TAG_FORCE_PUSH);
    }
    if w.lines.iter().any(|l| DELETE_RE.is_match(l)) {
        tags.push(TAG_DELETE);
    }
    if let (Some(bytes), Some(s)) = (&w.keys, sit) {
        let visible = strip_esc(&String::from_utf8_lossy(bytes));
        if s.kind == SituationKind::Password && !visible.trim().is_empty() {
            tags.push(TAG_SECRET);
        }
        if s.options.iter().any(|o| o.grants_permanent) && permanent_pick(bytes, &visible, s).is_some() {
            tags.push(TAG_PERMANENT_GRANT);
        }
    }
    tags
}

/// allow 标签：动作类别精确命中，或命令等于 / 以「条目 + 空格」开头（如 `cargo test`）。
fn allowed(auth: &Authority, w: &Write) -> bool {
    if auth.allows(w.category) {
        return true;
    }
    let Some(cmd) = w.command.as_deref().map(str::trim) else { return false };
    auth.allow.iter().map(|a| a.trim()).filter(|a| !a.is_empty()).any(|a| {
        cmd == a || cmd.strip_prefix(a).map_or(false, |rest| rest.starts_with(char::is_whitespace))
    })
}

fn scope_of(auth: &Authority) -> HostScope {
    if auth.host_scope.is_empty() {
        HostScope::Any
    } else {
        HostScope::List { hosts: auth.host_scope.clone() }
    }
}

/// 授权一次写入。见模块注释的六步顺序。
pub fn authorize(p: &HostPolicy, a: &AgentAction, sit: Option<&Situation>) -> Gate {
    let w = match write_of(a) {
        Ok(Some(w)) => w,
        Ok(None) => return Gate::Allow,
        Err(e) => return Gate::Deny(e),
    };
    let auth = &p.authority;

    // 1. deny：标签精确命中，或自定义关键字出现在要提交的文本里
    let tags = derive_tags(&w, sit);
    if let Some(t) = tags.iter().find(|t| auth.denies(t)) {
        return Gate::Deny(format!("契约禁止此类操作（{t}），没有写入。换一个不触及禁项的做法，或用 nx_ask_user 请用户处理"));
    }
    let payload = w.lines.join("\n").to_lowercase();
    if let Some(d) = auth.deny.iter().map(|d| d.trim()).find(|d| !d.is_empty() && payload.contains(&d.to_lowercase())) {
        return Gate::Deny(format!("命中契约禁项「{d}」，没有写入"));
    }

    // 2. Hard 偏好正则
    for r in &p.hard {
        if w.lines.iter().any(|l| r.deny.is_match(l)) {
            return Gate::Deny(format!("违反硬偏好 [P:{}]，没有写入", r.pref_id));
        }
    }

    // 3. 主机范围，fail-closed
    let who = HostIdentity {
        session_id: String::new(),
        label: String::new(),
        host_key: (!p.host_key.trim().is_empty()).then(|| p.host_key.clone()),
    };
    if let Err(e) = check_scope(&scope_of(auth), &who) {
        return Gate::Deny(e.replace("技能的适用主机范围", "契约的主机范围"));
    }

    // 4. Keys
    if let Some(bytes) = &w.keys {
        match assess_keys(bytes, sit) {
            KeysVerdict::Allow => {}
            KeysVerdict::Approve(r) => return Gate::Approve(r),
            KeysVerdict::Refuse(t) => return Gate::Deny(t),
        }
    }

    // 5. Exec / Open 的命令
    if let Some(cmd) = &w.command {
        let r = danger::assess(cmd);
        if r.dangerous {
            return Gate::Approve(r.reasons);
        }
    }

    // 6. 档位
    match auth.tier.as_str() {
        TIER_APPROVE => Gate::Approve(vec!["契约档位为「确认」：每次写入都需要批准".to_string()]),
        TIER_AUTO => Gate::Allow,
        _ if allowed(auth, &w) => Gate::Allow,
        _ => Gate::Notify,
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::situation::{Choice, Program};
    use super::super::super::skilltool::SkillCall;
    use super::*;
    use serde_json::json;

    fn policy(tier: &str) -> HostPolicy {
        HostPolicy {
            authority: Authority::default().with_tier(tier),
            hard: Vec::new(),
            host_key: "local".into(),
        }
    }
    fn exec(c: &str) -> AgentAction {
        AgentAction::Exec { target: None, command: c.into() }
    }
    fn keys(k: &str) -> AgentAction {
        AgentAction::Keys { target: None, keys: k.into() }
    }
    fn sit(kind: SituationKind, tail: &str, options: Vec<Choice>) -> Situation {
        Situation {
            frame_id: 7,
            screen_hash: "h".into(),
            program: Program::ClaudeCode,
            kind,
            topic: None,
            options,
            confidence: 0.9,
            tail: tail.into(),
            evidence: vec![],
            stale_ms: 0,
            signature: String::new(),
        }
    }
    fn pref(id: &str, strength: &str, extra: serde_json::Value) -> CtxItem {
        CtxItem {
            id: id.into(),
            kind: "preference".into(),
            text: "不许碰生产库".into(),
            strength: strength.into(),
            scope: "global".into(),
            source: "user".into(),
            evidence: String::new(),
            extra,
            contract_id: String::new(),
            position: 0,
            created_at: 0,
            updated_at: 0,
        }
    }
    fn is_deny(g: &Gate) -> bool {
        matches!(g, Gate::Deny(_))
    }
    fn is_approve(g: &Gate) -> bool {
        matches!(g, Gate::Approve(_))
    }

    #[test]
    fn read_only_actions_always_allowed_even_under_approve() {
        let p = policy("approve");
        for a in [
            AgentAction::Screen { target: None },
            AgentAction::Wait { target: None, pattern: None, timeout_ms: None, quiet_ms: None },
            AgentAction::Watch { target: None, until: None, timeout_ms: None, stall_ms: None },
            AgentAction::Tab { op: TabOp::List, target: None, name: None },
        ] {
            assert_eq!(authorize(&p, &a, None), Gate::Allow);
        }
    }

    #[test]
    fn tier_truth_table_for_safe_exec() {
        assert_eq!(authorize(&policy("auto"), &exec("ls -la"), None), Gate::Allow);
        assert_eq!(authorize(&policy("notify"), &exec("ls -la"), None), Gate::Notify);
        assert!(is_approve(&authorize(&policy("approve"), &exec("ls -la"), None)));
        // 不认识的档位按 notify
        let mut p = policy("auto");
        p.authority.tier = "weird".into();
        assert_eq!(authorize(&p, &exec("ls"), None), Gate::Notify);
    }

    #[test]
    fn allow_tags_lift_notify_to_auto_but_not_approve() {
        let mut p = policy("notify");
        p.authority.allow = vec!["cargo test".into()];
        assert_eq!(authorize(&p, &exec("cargo test --lib"), None), Gate::Allow);
        assert_eq!(authorize(&p, &exec("cargo test"), None), Gate::Allow);
        assert_eq!(authorize(&p, &exec("cargo testx"), None), Gate::Notify);
        assert_eq!(authorize(&p, &exec("cargo build"), None), Gate::Notify);
        p.authority.allow = vec!["exec".into()];
        assert_eq!(authorize(&p, &exec("make"), None), Gate::Allow);
        p.authority.tier = TIER_APPROVE.into();
        assert!(is_approve(&authorize(&p, &exec("make"), None)));
    }

    #[test]
    fn allow_never_lifts_danger() {
        let mut p = policy("auto");
        p.authority.allow = vec!["exec".into(), "sudo".into()];
        assert!(is_approve(&authorize(&p, &exec("sudo systemctl restart nginx"), None)));
    }

    #[test]
    fn deny_tags_are_derived_and_beat_everything() {
        let mut p = policy("auto");
        p.authority.allow = vec!["exec".into(), "git push".into()];
        for c in ["git push -f origin main", "git push --force", "git push origin +main", "git push -uf origin x"] {
            assert_eq!(authorize(&p, &exec(c), None), Gate::Deny(format!(
                "契约禁止此类操作（force_push），没有写入。换一个不触及禁项的做法，或用 nx_ask_user 请用户处理"
            )), "{c}");
        }
        for c in ["rm -rf build", "cd x && rm a.txt", "kubectl delete pod web", "find . -name '*.o' -delete", "git clean -fdx"] {
            assert!(is_deny(&authorize(&p, &exec(c), None)), "{c}");
        }
        assert_eq!(authorize(&p, &exec("git push origin main"), None), Gate::Allow);
        assert_eq!(authorize(&p, &exec("echo firmware"), None), Gate::Allow);
    }

    #[test]
    fn deny_tag_not_in_list_falls_through() {
        let mut p = policy("auto");
        p.authority.deny.clear();
        // 没禁 delete：rm 不再被拒，但仍要过 danger（rm 一律需要批准）
        assert!(is_approve(&authorize(&p, &exec("rm -rf /tmp/x"), None)));
        assert!(is_approve(&authorize(&p, &exec("rm a.txt"), None)));
        // 没禁 force_push：不再被拒，但 danger 把强推判为需要批准
        assert!(is_approve(&authorize(&p, &exec("git push --force"), None)));
        assert_eq!(authorize(&p, &exec("git push origin main"), None), Gate::Allow);
    }

    #[test]
    fn deny_custom_keyword_matches_payload() {
        let mut p = policy("auto");
        p.authority.deny = vec!["PROD-DB".into()];
        assert!(is_deny(&authorize(&p, &exec("psql -h prod-db.internal"), None)));
        assert!(is_deny(&authorize(&p, &keys("psql -h prod-db\\r"), None)));
        assert_eq!(authorize(&p, &exec("psql -h staging"), None), Gate::Allow);
    }

    #[test]
    fn secret_tag_from_typing_into_password_prompt() {
        let p = policy("auto");
        let s = sit(SituationKind::Password, "Password:", vec![]);
        assert!(is_deny(&authorize(&p, &keys("hunter2\\r"), Some(&s))));
        // 控制键（取消）不是输入口令
        assert_eq!(authorize(&p, &keys("<C-c>"), Some(&s)), Gate::Allow);
        // nx_secret 本身就是人输入，不打 secret 标签
        assert_eq!(authorize(&p, &AgentAction::Secret { target: None, reason: None }, Some(&s)), Gate::Allow);
    }

    #[test]
    fn permanent_grant_tag_from_option_pick() {
        let p = policy("auto");
        let opts = vec![
            Choice { index: Some("1".into()), text: "Yes".into(), grants_permanent: false },
            Choice { index: Some("2".into()), text: "Yes, and don't ask again".into(), grants_permanent: true },
        ];
        let s = sit(SituationKind::Permission, "Do you want to proceed?", opts);
        let g = authorize(&p, &keys("2"), Some(&s));
        assert!(matches!(&g, Gate::Deny(t) if t.contains("permanent_grant")), "{g:?}");
        assert_eq!(authorize(&p, &keys("1"), Some(&s)), Gate::Allow);
        // 没禁 permanent_grant 时 assess_keys 仍 fail-closed
        let mut p2 = policy("auto");
        p2.authority.deny.clear();
        assert!(is_deny(&authorize(&p2, &keys("2"), Some(&s))));
    }

    #[test]
    fn hard_rule_regex_on_command_and_typed_lines() {
        let mut p = policy("auto");
        p.hard = HardRule::from_prefs(&[
            pref("p1", "hard", json!({"denyPattern": "(?i)\\bnpm\\s+publish\\b"})),
            pref("p2", "hard", json!({})),
            pref("p3", "soft", json!({"denyPattern": "ls"})),
            pref("p4", "hard", json!({"denyPattern": "(unclosed"})),
        ]);
        assert_eq!(p.enforced_ids(), vec!["p1".to_string()]);
        assert_eq!(authorize(&p, &exec("npm publish --tag next"), None), Gate::Deny("违反硬偏好 [P:p1]，没有写入".into()));
        assert!(is_deny(&authorize(&p, &keys("echo hi\\rNPM publish\\r"), None)));
        assert_eq!(authorize(&p, &exec("ls"), None), Gate::Allow);
    }

    #[test]
    fn deny_precedes_hard_rule() {
        let mut p = policy("auto");
        p.hard = HardRule::from_prefs(&[pref("p1", "hard", json!({"denyPattern": "rm"}))]);
        assert!(matches!(authorize(&p, &exec("rm x"), None), Gate::Deny(t) if t.contains("delete")));
    }

    #[test]
    fn host_scope_fail_closed() {
        let mut p = policy("auto");
        p.authority.host_scope = vec!["web1".into()];
        p.host_key = "ssh:root@web1:22".into();
        assert_eq!(authorize(&p, &exec("uptime"), None), Gate::Allow);
        p.host_key = "ssh:root@db1:22".into();
        assert!(matches!(authorize(&p, &exec("uptime"), None), Gate::Deny(t) if t.contains("契约的主机范围")));
        // 认不出主机：拒
        p.host_key = String::new();
        assert!(is_deny(&authorize(&p, &exec("uptime"), None)));
        // 空范围 = 不限
        p.authority.host_scope.clear();
        assert_eq!(authorize(&p, &exec("uptime"), None), Gate::Allow);
    }

    #[test]
    fn hard_rule_precedes_scope() {
        let mut p = policy("auto");
        p.authority.host_scope = vec!["web1".into()];
        p.host_key = "ssh:root@db1:22".into();
        p.hard = HardRule::from_prefs(&[pref("p1", "hard", json!({"denyPattern": "uptime"}))]);
        assert!(matches!(authorize(&p, &exec("uptime"), None), Gate::Deny(t) if t.contains("[P:p1]")));
    }

    #[test]
    fn keys_go_through_assess_keys() {
        let p = policy("auto");
        // shell 上提交危险命令 → Approve
        let shell = sit(SituationKind::Idle, "user@h:~$ ", vec![]);
        let mut shell = shell;
        shell.program = Program::Shell;
        assert!(is_approve(&authorize(&p, &keys("sudo reboot\\r"), Some(&shell))));
        assert_eq!(authorize(&p, &keys("ls\\r"), Some(&shell)), Gate::Allow);
        // 没回车：只是敲字
        assert_eq!(authorize(&p, &keys("sudo reb"), Some(&shell)), Gate::Allow);
        // 坏按键串 fail-closed
        assert!(matches!(authorize(&p, &keys("<Bogus>"), None), Gate::Deny(t) if t.contains("按键串解析失败")));
    }

    #[test]
    fn keys_under_notify_tier_still_notify() {
        let mut shell = sit(SituationKind::Idle, "user@h:~$ ", vec![]);
        shell.program = Program::Shell;
        assert_eq!(authorize(&policy("notify"), &keys("ls\\r"), Some(&shell)), Gate::Notify);
    }

    #[test]
    fn open_command_assessed_and_tab_close_is_a_write() {
        let p = policy("auto");
        let open = AgentAction::Open { kind: OpenKind::Local, label: None, command: Some("sudo -i".into()) };
        assert!(is_approve(&authorize(&p, &open, None)));
        let close = AgentAction::Tab { op: TabOp::Close, target: Some("t1".into()), name: None };
        assert_eq!(authorize(&policy("notify"), &close, None), Gate::Notify);
        assert!(is_approve(&authorize(&policy("approve"), &close, None)));
    }

    #[test]
    fn skill_run_is_a_write_but_list_is_not() {
        let call = |op| SkillCall { op, query: None, key: Some("deploy".into()), params: Default::default(), params_error: None };
        let run = AgentAction::Skill { target: None, call: call(SkillOp::Run) };
        let list = AgentAction::Skill { target: None, call: call(SkillOp::List) };
        assert!(is_approve(&authorize(&policy("approve"), &run, None)));
        assert_eq!(authorize(&policy("approve"), &list, None), Gate::Allow);
        let mut p = policy("notify");
        p.authority.allow = vec!["skill".into()];
        assert_eq!(authorize(&p, &run, None), Gate::Allow);
    }
}
