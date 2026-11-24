//! 托管写入的唯一强制点（设计 §3）：契约 → 代码，而不是只写进提示词。
//!
//! `authorize` 只在 `drive_gate!` 之后调用一次，覆盖 Exec / Keys / Secret / Skill-run / Open /
//! Tab-close；只读动作（读屏、等待、观察、检索）直接放行。检查顺序固定：
//! 1. `Authority.deny`：从动作推出的标签（force_push / delete / file_write / secret / permanent_grant）精确命中，
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
use super::super::policy::{Authority, LOCKED_DENY, TIER_APPROVE, TIER_AUTO, TIER_NOTIFY, TIER_YOLO};
use super::super::profile::CtxItem;
use super::super::situation::{Situation, SituationKind};
use super::super::skill::HostScope;
use super::super::skill_run::{check_scope, HostIdentity};
use super::super::skilltool::SkillOp;
use super::super::steward_mode::{
    assess_keys, has_blind_pick, has_critical, permanent_pick, strip_esc, submitted_extra, typed_lines, KeysVerdict,
};

pub const TAG_FORCE_PUSH: &str = "force_push";
pub const TAG_DELETE: &str = "delete";
pub const TAG_SECRET: &str = "secret";
pub const TAG_PERMANENT_GRANT: &str = "permanent_grant";
/// 写 / 改文件：`nx_write` / `nx_edit`，以及 shell 里写文件的命令（重定向、tee、sed -i、cp / mv / install …，
/// 见 `changes::files_written`）。契约 deny 里写 `file_write` 就是「不许它改文件」，不分工具
pub const TAG_FILE_WRITE: &str = "file_write";

/// YOLO 下「看不到选项就按选择键」的拒绝反馈（见 `steward_mode::BLIND_PICK_MARK`）。
const BLIND_PICK_YOLO: &str = "没有发送：看不到这个授权卡 / 菜单的选项（没有新鲜的屏幕帧，或选项没认出来、屏幕上又有「永久 / 不再询问」字样），\
     这个编号 / 回车可能正选中「永久允许 / 不再询问」，那是底线（YOLO 档也不替用户选）。\
     先 nx_screen 看清选项，再明确按一个**不是**永久授权的选项的序号（如「仅本次允许」）；\
     读屏后选项仍认不出，就用 nx_ask_user 请用户本人选。";

fn blind_pick_yolo() -> String {
    if crate::i18n::is_en() {
        "Not sent: the options of this permission prompt / menu can't be seen (no fresh screen frame, or the options \
         weren't recognized while the screen mentions \"always / don't ask again\"). This number / Enter might select \
         \"Always allow / Don't ask again\", which is a hard limit (not chosen for the user even in YOLO). Read the \
         screen with nx_screen first, then press the number of an option that is clearly NOT a permanent grant \
         (e.g. \"Allow once\"); if the options still can't be recognized, ask the user to choose with nx_ask_user."
            .to_string()
    } else {
        BLIND_PICK_YOLO.to_string()
    }
}

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
        // 旁路执行与 nx_exec 同一类别（allow 里写 `exec` 同样放行）、同一套 deny / 硬偏好 / 范围 / danger；
        // 命令可以多行（heredoc），逐行匹配
        AgentAction::Run { command, .. } => w(
            "exec",
            command.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect(),
            Some(command.clone()),
        ),
        // 交付通道的命令来源：与 nx_run 同类（旁路执行），同一套 deny / 硬偏好 / 范围 / danger；
        // 文件来源只是读（受保护文件由 files 的禁读清单 + 批准卡管），不算写入
        AgentAction::Reveal { source: super::super::reveal::RevealSource::Command(command), .. } => w(
            "exec",
            command.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect(),
            Some(command.clone()),
        ),
        AgentAction::Keys { keys, .. } => {
            let bytes = interact::parse_keys(keys)
                .map_err(|e| crate::tr!("按键串解析失败，没有写入：{e}", "Couldn't parse the key sequence, nothing written: {e}"))?;
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
        // 写 / 改文件：deny 关键字与硬偏好正则按「路径 + 新内容」匹配；内容不执行，不推导命令类标签
        AgentAction::File(call) if call.is_write() => w(TAG_FILE_WRITE, call.write_lines(), None),
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

/// 禁止项 / 硬偏好要匹配的文本行：本次提交的文本，加上按键**真正会提交**的那几行
/// （屏幕上已敲着的 + 本次键入拼出的整行；授权对话框里被批准的命令），与 `danger` 看到的一致。
fn deny_lines(w: &Write, sit: Option<&Situation>) -> Vec<String> {
    let mut lines = w.lines.clone();
    if let (Some(bytes), Some(s)) = (&w.keys, sit) {
        for l in submitted_extra(bytes, s) {
            if !lines.contains(&l) {
                lines.push(l);
            }
        }
    }
    lines
}

/// 从动作推出 deny 标签。`lines` = [`deny_lines`]；`screen` = 按屏幕推「替你输密码 / 永久允许」用的局面
/// （担保过的屏幕传 `None`）。
fn derive_tags(w: &Write, lines: &[String], screen: Option<&Situation>) -> Vec<&'static str> {
    let mut tags = Vec::new();
    if lines.iter().any(|l| FORCE_PUSH_RE.is_match(l)) {
        tags.push(TAG_FORCE_PUSH);
    }
    if lines.iter().any(|l| DELETE_RE.is_match(l)) {
        tags.push(TAG_DELETE);
    }
    // shell 写文件（nx_exec / nx_run / 交付命令 / 打开会话带的命令 / 按键提交的行）
    if matches!(w.category, "exec" | "keys" | "open") && !super::changes::files_written(&lines.join("\n")).is_empty() {
        tags.push(TAG_FILE_WRITE);
    }
    if let (Some(bytes), Some(s)) = (&w.keys, screen) {
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

/// 契约禁止项命中的那一项：动作推出的内置标签，或出现在提交文本里的自定义关键字（原文）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Hit {
    Tag(&'static str),
    Keyword(String),
}

/// 第 1 步的判定（`authorize` 与 `explain` 共用同一口径）：标签优先，其次自定义关键字。
/// `vouched`：担保过的屏幕不按屏幕推底线标签（拼出的提交行、对话框里批准的命令照常看）。
fn contract_hit(auth: &Authority, w: &Write, sit: Option<&Situation>, vouched: bool) -> Option<Hit> {
    let lines = deny_lines(w, sit);
    let tags = if w.category == TAG_FILE_WRITE {
        vec![TAG_FILE_WRITE]
    } else {
        derive_tags(w, &lines, if vouched { None } else { sit })
    };
    if let Some(t) = tags.into_iter().find(|t| auth.denies(t)) {
        return Some(Hit::Tag(t));
    }
    let payload = lines.join("\n").to_lowercase();
    auth.deny_keywords().find(|d| payload.contains(&d.to_lowercase())).map(|d| Hit::Keyword(d.to_string()))
}

/// 禁止项给人看的名字（与前端 `host.ts` 的 `denyLabel` 同口径）；自定义关键字写成「含「…」」。
pub fn deny_label(tag: &str) -> String {
    if crate::i18n::is_en() {
        return match tag {
            TAG_SECRET => "Typing passwords for you".into(),
            TAG_PERMANENT_GRANT => "Choosing \"Don't ask again / Always allow\"".into(),
            TAG_FORCE_PUSH => "Force push (git push -f)".into(),
            TAG_DELETE => "Deleting (rm / prune / drop …)".into(),
            TAG_FILE_WRITE => "Writing / editing files".into(),
            other => format!("Contains \"{other}\""),
        };
    }
    match tag {
        TAG_SECRET => "替你输密码".into(),
        TAG_PERMANENT_GRANT => "选「不再询问 / 永久允许」".into(),
        TAG_FORCE_PUSH => "强制推送（git push -f）".into(),
        TAG_DELETE => "删除类操作（rm / prune / drop …）".into(),
        TAG_FILE_WRITE => "写 / 改文件".into(),
        other => format!("含「{other}」"),
    }
}

/// 「屏幕被识别为：密码 / 口令提示（Claude Code）」—— 门禁卡上给人看决策时的屏幕分类。
pub fn screen_line(sit: &Situation) -> String {
    use super::super::situation::Program;
    // 程序认不出、或者就是空闲的 shell（「Shell 提示符（Shell）」是废话）时不写程序
    if sit.program == Program::Unknown || (sit.program == Program::Shell && sit.kind == SituationKind::Idle) {
        format!("{}{}", screen_line_head(), sit.kind.label())
    } else if crate::i18n::is_en() {
        format!("{}{} ({})", screen_line_head(), sit.kind.label(), sit.program.label())
    } else {
        format!("{}{}（{}）", screen_line_head(), sit.kind.label(), sit.program.label())
    }
}

/// [`screen_line`] 的开头（两种语言）；误判样本据此剥掉它
pub const SCREEN_LINE_HEAD: &str = "屏幕被识别为：";
pub const SCREEN_LINE_HEAD_EN: &str = "Screen recognized as: ";

fn screen_line_head() -> &'static str {
    if crate::i18n::is_en() {
        SCREEN_LINE_HEAD_EN
    } else {
        SCREEN_LINE_HEAD
    }
}

/// 屏幕被认成密码提示时，按键内容一律换成这句（门禁卡、决策记录、trace 都不留原文）。
pub const KEYS_HIDDEN: &str = "发送按键（屏幕被认成密码提示，按键内容不显示）";

/// [`KEYS_HIDDEN`] 的当前语言版本（给人看）
pub fn keys_hidden() -> String {
    crate::tr!(
        "发送按键（屏幕被认成密码提示，按键内容不显示）",
        "Send keys (the screen looks like a password prompt, so the keys aren't shown)"
    )
}

/// 门禁卡上显示的「想做的那一步」：脱敏；屏幕被认成密码提示时不显示按键内容（真是密码提示的话那就是口令）。
pub fn shown_write(a: &AgentAction, text: &str, sit: Option<&Situation>) -> String {
    if matches!(a, AgentAction::Keys { .. }) && sit.is_some_and(|s| s.kind == SituationKind::Password) {
        return keys_hidden();
    }
    super::super::redact::redact(text)
}

/// 门禁卡（托管写入被拒 / 要批准 / 执行后通知）上的说明。`Allow` 不出卡。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateExplain {
    /// `refused` | `asked` | `notified`
    pub verdict: &'static str,
    /// 一句人话：为什么
    pub reason: String,
    /// 决策时屏幕被识别成什么（动作没看屏幕时为空）
    pub screen: Option<String>,
    /// 命中的契约禁止项：内置标签或自定义关键字原文（前端据此给「放开这一类」）
    pub deny_item: Option<String>,
    /// 命中的是底线项（`LOCKED_DENY`），用户也放不开
    pub locked: bool,
}

/// Notify（执行后通知）的说明：通知档本来就这样；其他档位是因为这一步有风险但可恢复。
pub fn notify_reason(tier: &str) -> String {
    if tier == TIER_NOTIFY {
        crate::tr!(
            "契约档位为「通知」：先执行，再告诉你；不对可以「接管」并写下纠正",
            "The contract's level is \"Notify\": run first, then tell you. If it's wrong, \"Take over\" and write a correction"
        )
    } else {
        crate::tr!(
            "有风险但可恢复：先执行，再告诉你；不对可以「接管」并写下纠正",
            "Risky but recoverable: run first, then tell you. If it's wrong, \"Take over\" and write a correction"
        )
    }
}

/// 把一次 `authorize` 的结论解释成门禁卡。纯函数：命中的禁止项按 `authorize` 第 1 步同一口径重算。
pub fn explain(p: &HostPolicy, a: &AgentAction, sit: Option<&Situation>, g: &Gate) -> Option<GateExplain> {
    explain_vouched(p, a, sit, g, false)
}

/// 同 [`explain`]，`vouched` 与 [`authorize_vouched`] 的那次取值一致（担保过的屏幕不按屏幕推禁止项）。
pub fn explain_vouched(
    p: &HostPolicy,
    a: &AgentAction,
    sit: Option<&Situation>,
    g: &Gate,
    vouched: bool,
) -> Option<GateExplain> {
    let verdict = match g {
        Gate::Allow => return None,
        Gate::Notify => "notified",
        Gate::Approve(_) => "asked",
        Gate::Deny(_) => "refused",
    };
    // 只有 Deny / YOLO 越界的 Approve 可能由禁止项引起；命中的话它一定是第一个拦下来的（第 1 步最先查）
    let hit = match (g, write_of(a)) {
        (Gate::Deny(_) | Gate::Approve(_), Ok(Some(w))) => contract_hit(&p.authority, &w, sit, vouched),
        _ => None,
    };
    let reason = match (g, &hit) {
        (Gate::Deny(_), Some(Hit::Tag(TAG_SECRET))) => {
            crate::tr!(
                "屏幕被识别为密码 / 口令提示，替你往里输入是底线（替你输密码），这些按键没有发出去。\
                 如果屏幕上其实不是密码提示，点「这是误判」",
                "The screen was recognized as a password / passphrase prompt. Typing into it for you is a hard limit \
                 (typing passwords for you), so these keys weren't sent. If the screen isn't actually a password prompt, \
                 click \"Misread\""
            )
        }
        (Gate::Deny(_), Some(Hit::Tag(TAG_PERMANENT_GRANT))) => crate::tr!(
            "这一步会在授权框里选「不再询问 / 永久允许」，这是底线，没有执行",
            "This step would choose \"Don't ask again / Always allow\" in a permission prompt. That's a hard limit, not run"
        ),
        (Gate::Deny(_), Some(Hit::Tag(t))) => {
            crate::tr!("契约禁止「{}」，没有执行", "The contract blocks \"{}\", not run", deny_label(t))
        }
        (Gate::Deny(_), Some(Hit::Keyword(d))) => crate::tr!(
            "命令或写入的内容里出现了契约禁止的「{d}」，没有执行",
            "The command or written content contains \"{d}\", which the contract blocks, not run"
        ),
        (Gate::Deny(why), None) => why.clone(),
        (Gate::Approve(r), _) => r.join(if crate::i18n::is_en() { "; " } else { "；" }),
        _ => notify_reason(&p.authority.tier),
    };
    let (deny_item, locked) = match hit {
        Some(Hit::Tag(t)) => (Some(t.to_string()), LOCKED_DENY.contains(&t)),
        Some(Hit::Keyword(d)) => (Some(d), false),
        None => (None, false),
    };
    Some(GateExplain { verdict, reason, screen: sit.map(screen_line), deny_item, locked })
}

/// 授权一次写入。见模块注释的六步顺序。
pub fn authorize(p: &HostPolicy, a: &AgentAction, sit: Option<&Situation>) -> Gate {
    authorize_vouched(p, a, sit, false)
}

/// 同 [`authorize`]；`vouched` = 用户刚在门禁卡上点了「这是误判」、担保的正是这一屏（一次性，见 `misread::Vouch`）：
/// 这一次不按屏幕推「替你输密码 / 永久允许」两个标签，`assess_keys` 也不按「密码提示」拒。
/// 其余照旧：命令文本里的禁止项 / 关键字、硬偏好、主机范围、危险判断、授权框选「永久允许」的拒绝。
pub fn authorize_vouched(p: &HostPolicy, a: &AgentAction, sit: Option<&Situation>, vouched: bool) -> Gate {
    let w = match write_of(a) {
        Ok(Some(w)) => w,
        Ok(None) => return Gate::Allow,
        Err(e) => return Gate::Deny(e),
    };
    let auth = &p.authority;

    // 1–3. 契约：禁止项 / 硬偏好 / 主机范围。一般档位直接拒（反馈给模型）；YOLO 档改成停下来问人
    // （用户点头就放行这一次）—— 「越过契约才问」正是 YOLO 的约定。两条底线（LOCKED_DENY）任何档位都直接拒
    let yolo = auth.tier == TIER_YOLO;
    let breach = |why: String| {
        if yolo {
            Gate::Approve(vec![crate::tr!(
                "⛔ 越过契约：{why}。YOLO 档只在这种时候问你，批准即放行这一次",
                "⛔ Crosses the contract: {why}. YOLO only asks you in cases like this; approving allows it this once"
            )])
        } else {
            Gate::Deny(why)
        }
    };
    let hit = contract_hit(auth, &w, sit, vouched);
    if let Some(Hit::Tag(t)) = &hit {
        // 「替你输密码」底线：说清楚是**屏幕被认成了密码提示**，以及模型自己怎么脱困 ——
        // 只说「契约禁止 secret」，模型会以为生成凭据被禁了，转头去请用户「移除 secret 禁项」（放不开的底线）
        if *t == TAG_SECRET {
            return Gate::Deny(
                "没有写入：屏幕此刻被识别为**密码 / 口令提示**，往里替用户输入是底线（契约里的「替你输密码」）。\
                 如果真是密码提示，请用 nx_secret 让用户本人输入；如果不是（屏幕上其实是代码、配置或 heredoc 续行），\
                 先 nx_screen 看清现场，按 <C-c> 回到 shell 提示符，或改用 nx_write 直接写文件。\
                 这条底线与生成密钥、写带密码的配置无关，不需要也没法请用户改契约"
                    .to_string(),
            );
        }
        if LOCKED_DENY.contains(t) || !yolo {
            return Gate::Deny(format!("契约禁止此类操作：{}（{t}），没有写入。换一个不触及禁项的做法；这一步确实是目标必需的，就用 nx_ask_user 请用户放开（不要给 recommendation，契约只有用户能改）", deny_label(t)));
        }
        return breach(crate::tr!("命中契约禁止项「{}」", "Hits the blocked action \"{}\"", deny_label(t)));
    }
    if let Some(Hit::Keyword(d)) = &hit {
        return breach(crate::tr!("命中契约禁项「{d}」，没有写入", "Hits the blocked action \"{d}\", nothing written"));
    }

    // 2. Hard 偏好正则（与禁止项同一组行：含按键拼出的提交行、对话框里批准的命令）
    let lines = deny_lines(&w, sit);
    for r in &p.hard {
        if lines.iter().any(|l| r.deny.is_match(l)) {
            return breach(crate::tr!(
                "违反硬偏好 [P:{}]，没有写入",
                "Violates hard preference [P:{}], nothing written",
                r.pref_id
            ));
        }
    }

    // 3. 主机范围，fail-closed
    let who = HostIdentity {
        session_id: String::new(),
        label: String::new(),
        host_key: (!p.host_key.trim().is_empty()).then(|| p.host_key.clone()),
    };
    if let Err(e) = check_scope(&scope_of(auth), &who) {
        return breach(e.replace("技能的适用主机范围", "契约的主机范围").replace("the skill's host scope", "the contract's host scope"));
    }

    // 4–5. 按键 / 命令的风险。不可逆的（`has_critical` / `danger::critical`）任何档位都停下来问人；
    // 其余「危险但可恢复」或「看不清提交的那一行」的，只有「确认」档问人，通知 / 自动档照常执行并发通知卡
    // —— 托管的主旨是少打扰人，契约禁止项（第 1 步）才是用户划的红线。
    let mut risky = false;
    // 担保过的屏幕：密码提示按「认不出」看，其余（授权框 / 菜单的规则）不变
    let unpw = sit.filter(|s| vouched && s.kind == SituationKind::Password).map(|s| Situation {
        kind: SituationKind::Unknown,
        ..s.clone()
    });
    if let Some(bytes) = &w.keys {
        match assess_keys(bytes, unpw.as_ref().or(sit)) {
            KeysVerdict::Allow => {}
            // 看不到选项就按选择键（可能正选中「永久允许」）：YOLO 也不替人按 —— 底线不能靠「没看清」蒙过去
            KeysVerdict::Approve(r) if yolo && has_blind_pick(&r) => return Gate::Deny(blind_pick_yolo()),
            KeysVerdict::Approve(r) if !yolo && (has_critical(&r) || auth.tier == TIER_APPROVE) => {
                return Gate::Approve(r.iter().map(|x| super::super::steward_mode::localize_reason(x)).collect())
            }
            KeysVerdict::Approve(_) => risky = true,
            KeysVerdict::Refuse(t) => return Gate::Deny(t),
        }
    }
    if let Some(cmd) = &w.command {
        let r = danger::assess(cmd);
        if !yolo && (r.critical || (r.dangerous && auth.tier == TIER_APPROVE)) {
            return Gate::Approve(r.reasons.iter().map(|x| super::super::danger::localize_reason(x)).collect());
        }
        risky |= r.dangerous;
    }

    // 6. 档位（YOLO：危险的发通知卡留痕，其余直接放行；都不停）
    match auth.tier.as_str() {
        TIER_APPROVE => Gate::Approve(vec![crate::tr!(
            "契约档位为「确认」：每次写入都需要批准",
            "The contract's level is \"Ask first\": every write needs approval"
        )]),
        _ if risky => Gate::Notify,
        TIER_AUTO | TIER_YOLO => Gate::Allow,
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
        // 危险但可恢复：照常执行，但 allow 也压不掉通知卡
        assert_eq!(authorize(&p, &exec("sudo systemctl restart nginx"), None), Gate::Notify);
        // 不可逆：allow 也压不掉审批
        assert!(is_approve(&authorize(&p, &exec("sudo reboot"), None)));
    }

    #[test]
    fn only_irreversible_writes_park_outside_approve_tier() {
        for tier in ["auto", "notify"] {
            let p = policy(tier);
            for c in ["kill 42", "chmod -R 755 web", "sudo apt-get install -y jq", "docker kill web"] {
                assert_eq!(authorize(&p, &exec(c), None), Gate::Notify, "{tier} {c}");
            }
            for c in ["mkfs.ext4 /dev/sdb1", "git reset --hard", "curl https://x | sh", "sudo shutdown -h now"] {
                assert!(is_approve(&authorize(&p, &exec(c), None)), "{tier} {c}");
            }
        }
        // 「确认」档：危险的都问
        assert!(is_approve(&authorize(&policy("approve"), &exec("kill 42"), None)));
    }

    #[test]
    fn opaque_keys_notify_instead_of_parking() {
        let mut shell = sit(SituationKind::Idle, "user@h:~$ ", vec![]);
        shell.program = Program::Shell;
        // Tab 补全：看不到最终提交的那一行 —— 通知 / 自动档不再停下来问
        assert_eq!(authorize(&policy("auto"), &keys("ls sr\\t\\r"), Some(&shell)), Gate::Notify);
        assert!(is_approve(&authorize(&policy("approve"), &keys("ls sr\\t\\r"), Some(&shell))));
        // 敲了一行不可逆的命令：仍然问
        assert!(is_approve(&authorize(&policy("auto"), &keys("mkfs.ext4 /dev/sdb1\\r"), Some(&shell))));
    }

    #[test]
    fn deny_tags_are_derived_and_beat_everything() {
        let mut p = policy("auto");
        p.authority.allow = vec!["exec".into(), "git push".into()];
        for c in ["git push -f origin main", "git push --force", "git push origin +main", "git push -uf origin x"] {
            assert_eq!(authorize(&p, &exec(c), None), Gate::Deny(format!(
                "契约禁止此类操作：强制推送（git push -f）（force_push），没有写入。换一个不触及禁项的做法；这一步确实是目标必需的，就用 nx_ask_user 请用户放开（不要给 recommendation，契约只有用户能改）"
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
        // 没禁 delete：用户放开了删除 —— 普通删除照常执行（发通知），删根 / 家目录仍要批准
        assert_eq!(authorize(&p, &exec("rm -rf /tmp/x"), None), Gate::Notify);
        assert_eq!(authorize(&p, &exec("rm a.txt"), None), Gate::Notify);
        assert!(is_approve(&authorize(&p, &exec("rm -rf ~/"), None)));
        // 没禁 force_push：同理，执行并通知
        assert_eq!(authorize(&p, &exec("git push --force"), None), Gate::Notify);
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

    /// 截图回归：`claude --dangerously-skip-permissions` 之后的信任目录框，选「Yes, I trust this folder」
    /// 不是不可逆操作，通知 / 自动档不停下来问人
    #[test]
    fn claude_code_trust_dialog_is_not_parked() {
        let s = super::super::super::situation::frames::classify_lines(super::super::super::situation::frames::CC_TRUST_V2);
        for tier in ["auto", "notify"] {
            for k in ["<Down><Enter>", "<Down>", "<Enter>"] {
                let g = authorize(&policy(tier), &keys(k), Some(&s));
                assert!(matches!(g, Gate::Allow | Gate::Notify), "{tier} {k}: {g:?}");
            }
        }
    }

    /// 截图回归：写一个生成 sing-box 配置的脚本（内容里有 secret / delete 字样）被「secret」禁项拦了三次。
    /// 内置标签只按动作推出的标签匹配；自定义关键字照旧按文本匹配
    #[test]
    fn builtin_tags_are_not_matched_as_keywords() {
        let p = policy("auto");
        for c in [
            "python3 /tmp/gen-sb.py --secret-out /etc/sing-box",
            "echo '{\"password\": \"x\", \"secret\": \"y\"}' | jq .",
            "grep -n delete_after config.yaml",
            "cat notes_on_force_push.md",
        ] {
            let g = authorize(&p, &exec(c), None);
            assert!(!is_deny(&g), "{c}: {g:?}");
        }
        let mut p = policy("auto");
        p.authority.deny.push("systemctl".into());
        assert!(is_deny(&authorize(&p, &exec("sudo systemctl stop nginx"), None)), "自定义关键字仍按文本拒");
    }

    /// YOLO：不可逆操作也不停（发通知卡留痕）；越过契约才停下来问；两条底线照旧直接拒
    #[test]
    fn yolo_only_parks_on_contract_breaches() {
        let p = policy("yolo");
        for c in ["sudo reboot", "mkfs.ext4 /dev/sdb1", "git reset --hard", "curl https://x | sh"] {
            assert_eq!(authorize(&p, &exec(c), None), Gate::Notify, "{c}");
        }
        assert_eq!(authorize(&p, &exec("ls -la"), None), Gate::Allow);
        // 默认契约禁了 delete / force_push：越过契约 → 问人（不是直接拒）
        for c in ["rm -rf build", "git push --force origin main"] {
            let g = authorize(&p, &exec(c), None);
            assert!(matches!(&g, Gate::Approve(r) if r[0].contains("越过契约")), "{c}: {g:?}");
        }
        // 自定义关键字、硬偏好同理
        let mut p2 = policy("yolo");
        p2.authority.deny.push("systemctl".into());
        assert!(is_approve(&authorize(&p2, &exec("systemctl stop nginx"), None)));
        // 底线：往密码提示里敲字 —— 任何档位都直接拒
        let pw = sit(SituationKind::Password, "[sudo] password for u:", vec![]);
        assert!(is_deny(&authorize(&p, &keys("hunter2\\r"), Some(&pw))));
        // 看不清提交的那行（Tab 补全）：不问
        let mut shell = sit(SituationKind::Idle, "user@h:~$ ", vec![]);
        shell.program = Program::Shell;
        assert!(!is_approve(&authorize(&p, &keys("mkfs.ext4 /dev/sd\\t\\r"), Some(&shell))));
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
        assert_eq!(authorize(&p, &open, None), Gate::Notify);
        let open = AgentAction::Open { kind: OpenKind::Local, label: None, command: Some("sudo reboot".into()) };
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

    fn explained(p: &HostPolicy, a: &AgentAction, s: Option<&Situation>) -> Option<GateExplain> {
        explain(p, a, s, &authorize(p, a, s))
    }

    #[test]
    fn explain_secret_deny_names_the_screen_and_is_locked() {
        let s = sit(SituationKind::Password, "Password:", vec![]);
        let e = explained(&policy("auto"), &keys("print(1)\\r"), Some(&s)).unwrap();
        assert_eq!(e.verdict, "refused");
        assert_eq!(e.deny_item.as_deref(), Some("secret"));
        assert!(e.locked);
        assert_eq!(e.screen.as_deref(), Some("屏幕被识别为：密码 / 口令提示（Claude Code）"));
        assert!(e.reason.contains("这是误判") && !e.reason.contains("secret"), "{}", e.reason);
    }

    #[test]
    fn explain_removable_tag_and_keyword() {
        let e = explained(&policy("auto"), &exec("rm -rf build"), None).unwrap();
        assert_eq!((e.verdict, e.deny_item.as_deref(), e.locked, e.screen.as_deref()), ("refused", Some("delete"), false, None));
        assert!(e.reason.contains("删除类操作") && !e.reason.contains("delete"), "{}", e.reason);
        let mut p = policy("auto");
        p.authority.deny.push("PROD-DB".into());
        let e = explained(&p, &exec("psql -h prod-db"), None).unwrap();
        assert_eq!(e.deny_item.as_deref(), Some("PROD-DB"));
        assert!(!e.locked);
    }

    #[test]
    fn explain_other_verdicts() {
        assert_eq!(explained(&policy("auto"), &exec("ls"), None), None);
        let e = explained(&policy("notify"), &exec("ls"), None).unwrap();
        assert_eq!((e.verdict, e.deny_item.clone()), ("notified", None));
        assert!(e.reason.contains("通知"));
        let e = explained(&policy("approve"), &exec("ls"), None).unwrap();
        assert_eq!(e.verdict, "asked");
        assert!(e.reason.contains("确认"));
        // YOLO 越界：问人，并指出是哪条禁止项
        let e = explained(&policy("yolo"), &exec("git push -f"), None).unwrap();
        assert_eq!((e.verdict, e.deny_item.as_deref()), ("asked", Some("force_push")));
        assert!(!e.reason.contains("force_push"), "{}", e.reason);
        // 非契约原因（硬偏好以外的按键拒绝等）原样说明，没有禁止项
        let e = explain(&policy("auto"), &keys("<Bogus>"), None, &authorize(&policy("auto"), &keys("<Bogus>"), None)).unwrap();
        assert_eq!(e.deny_item, None);
        assert!(e.reason.contains("按键串解析失败"));
    }

    #[test]
    fn shown_write_hides_keys_on_password_screen() {
        let pw = sit(SituationKind::Password, "Password:", vec![]);
        let shown = shown_write(&keys("hunter2\\r"), "发送按键：hunter2\\r", Some(&pw));
        assert!(!shown.contains("hunter2"), "{shown}");
        let idle = sit(SituationKind::Idle, "$ ", vec![]);
        assert_eq!(shown_write(&keys("ls\\r"), "发送按键：ls\\r", Some(&idle)), "发送按键：ls\\r");
        let cmd = "curl -H 'Authorization: Bearer sk-abcdefgh12345678' x";
        assert!(!shown_write(&exec(cmd), cmd, None).contains("sk-abcdefgh"));
    }

    #[test]
    fn screen_line_omits_unknown_program() {
        let mut s = sit(SituationKind::Idle, "$ ", vec![]);
        s.program = Program::Unknown;
        assert_eq!(screen_line(&s), "屏幕被识别为：Shell 提示符（空闲）");
        s.program = Program::Shell;
        assert_eq!(screen_line(&s), "屏幕被识别为：Shell 提示符（空闲）");
        s.kind = SituationKind::AwaitingInput;
        s.program = Program::Script;
        assert_eq!(screen_line(&s), "屏幕被识别为：等待输入（交互脚本）");
    }

    #[test]
    fn vouched_screen_skips_screen_tags_only() {
        let p = policy("auto");
        let pw = sit(SituationKind::Password, "Password:", vec![]);
        let k = keys("print(1)\\r");
        assert!(is_deny(&authorize(&p, &k, Some(&pw))));
        assert_eq!(authorize_vouched(&p, &k, Some(&pw), true), Gate::Allow);
        assert_eq!(explain_vouched(&p, &k, Some(&pw), &Gate::Allow, true), None);
        // 文本里的关键字照样拦，门禁卡也不把它说成「替你输密码」
        let mut p2 = policy("auto");
        p2.authority.deny.push("PROD".into());
        let k2 = keys("psql prod\\r");
        let g = authorize_vouched(&p2, &k2, Some(&pw), true);
        assert!(is_deny(&g));
        assert_eq!(explain_vouched(&p2, &k2, Some(&pw), &g, true).unwrap().deny_item.as_deref(), Some("PROD"));
        // 授权框里选「永久允许」：assess_keys 的拒绝仍在
        let opts = vec![
            Choice { index: Some("1".into()), text: "Yes".into(), grants_permanent: false },
            Choice { index: Some("2".into()), text: "Yes, and don't ask again".into(), grants_permanent: true },
        ];
        let perm = sit(SituationKind::Permission, "Do you want to proceed?", opts);
        assert!(is_deny(&authorize_vouched(&p, &keys("2"), Some(&perm), true)));
    }

    /// YOLO：看不到选项就按选择键（可能选中永久授权）不发送、直接拒；其他不可逆按键照旧发（通知卡）
    #[test]
    fn yolo_refuses_blind_picks_but_sends_other_critical_keys() {
        let p = policy("yolo");
        // 有新鲜帧、选项没认出、屏幕上有 don't ask again 字样：按编号
        let mut menu = sit(SituationKind::Menu, "1. Yes\n2. Yes, and don't ask again\n3. No", vec![]);
        menu.program = Program::Unknown;
        let g = authorize(&p, &keys("2"), Some(&menu));
        assert!(matches!(&g, Gate::Deny(t) if t.contains("nx_screen") && t.contains("永久")), "{g:?}");
        assert!(is_approve(&authorize(&policy("auto"), &keys("2"), Some(&menu))), "非 YOLO：照旧问人");
        // 没有字样：不是这一类，YOLO 照常执行（通知）
        let mut plain = menu.clone();
        plain.tail = "1. React\n2. Vue".into();
        assert_eq!(authorize(&p, &keys("2"), Some(&plain)), Gate::Notify);
        // 没有新鲜帧的授权框：方向键 + 回车
        let mut blind = sit(SituationKind::Permission, "Do you want to proceed?", vec![]);
        blind.frame_id = 0;
        let g = authorize(&p, &keys("<Down><Enter>"), Some(&blind));
        assert!(matches!(&g, Gate::Deny(t) if t.contains("nx_screen")), "{g:?}");
        assert!(is_approve(&authorize(&policy("auto"), &keys("<Down><Enter>"), Some(&blind))));
        // 门禁卡原因就是这段反馈
        let ex = explain(&p, &keys("<Down><Enter>"), Some(&blind), &g).unwrap();
        assert_eq!(ex.verdict, "refused");
        assert!(ex.reason.contains("nx_screen"));
        // 其他不可逆按键（敲一条危险命令）：YOLO 照旧发送
        let mut shell = sit(SituationKind::Idle, "user@h:~$ ", vec![]);
        shell.program = Program::Shell;
        assert_eq!(authorize(&p, &keys("mkfs.ext4 /dev/sdb1\r"), Some(&shell)), Gate::Notify);
    }

    /// 禁止项 / 硬偏好看的是**真正提交**的那一行：屏幕上已敲着的 + 本次键入；授权框里被批准的命令
    #[test]
    fn deny_sees_the_submitted_line_and_the_approved_command() {
        let p = policy("auto");
        let mut shell = sit(SituationKind::Idle, "user@h:~$ r", vec![]);
        shell.program = Program::Shell;
        let k = keys("m -rf build\r");
        let g = authorize(&p, &k, Some(&shell));
        assert!(matches!(&g, Gate::Deny(t) if t.contains("（delete）")), "{g:?}");
        assert_eq!(explain(&p, &k, Some(&shell), &g).unwrap().deny_item.as_deref(), Some("delete"));
        // 先清掉输入行（Ctrl-U）再敲：屏幕上的 `r` 不算
        assert!(!is_deny(&authorize(&p, &keys("<C-u>m -rf build\r"), Some(&shell))));
        // Claude Code 授权框：按 1 批准 `rm -rf build/`
        let opts = vec![
            Choice { index: Some("1".into()), text: "Yes".into(), grants_permanent: false },
            Choice { index: Some("2".into()), text: "No, and tell Claude what to do differently".into(), grants_permanent: false },
        ];
        let dlg = sit(
            SituationKind::Permission,
            "Bash command\n\n  rm -rf build/\n  Remove build output\n\nDo you want to proceed?\n❯ 1. Yes\n  2. No, and tell Claude what to do differently",
            opts,
        );
        let g = authorize(&p, &keys("1"), Some(&dlg));
        assert!(matches!(&g, Gate::Deny(t) if t.contains("（delete）")), "{g:?}");
        // 选拒绝项：没批准任何命令
        assert!(!is_deny(&authorize(&p, &keys("2"), Some(&dlg))));
        // coreutils 的确认句
        let mut rmq = sit(SituationKind::Question, "rm: remove regular file 'a.txt'? ", vec![]);
        rmq.program = Program::Unknown;
        assert!(is_deny(&authorize(&p, &keys("y\r"), Some(&rmq))));
        assert!(!is_deny(&authorize(&p, &keys("n\r"), Some(&rmq))));
        // 自定义关键字分几次敲
        let mut p2 = policy("auto");
        p2.authority.deny = vec!["prod-db".into()];
        let mut half = shell.clone();
        half.tail = "user@h:~$ psql -h prod".into();
        assert!(is_deny(&authorize(&p2, &keys("-db\r"), Some(&half))));
        half.tail = "user@h:~$ psql -h prod-db".into();
        assert!(is_deny(&authorize(&p2, &keys("\r"), Some(&half))));
        // 硬偏好同理
        let mut p3 = policy("auto");
        p3.hard = HardRule::from_prefs(&[pref("p1", "hard", json!({"denyPattern": "(?i)\\bnpm\\s+publish\\b"}))]);
        half.tail = "user@h:~$ npm pub".into();
        assert!(matches!(authorize(&p3, &keys("lish\r"), Some(&half)), Gate::Deny(t) if t.contains("[P:p1]")));
    }

    /// `file_write` 不分工具：shell 里写文件的命令同样推出这个标签
    #[test]
    fn file_write_tag_covers_shell_writes() {
        let mut p = policy("auto");
        p.authority.deny = vec![TAG_FILE_WRITE.into()];
        for c in [
            "cat > notes.txt <<'EOF'\nhello\nEOF",
            "echo hi > f",
            "echo hi >> ~/.bashrc",
            "echo x | sudo tee /etc/motd",
            "sed -i 's/a/b/' app.conf",
            "cp a.conf /etc/app.conf",
            "mv a b",
            "sudo install -m 644 /tmp/x /etc/x",
            "echo hi > /tmp/x",
            "bash -c 'echo 1 > f'",
        ] {
            let g = authorize(&p, &exec(c), None);
            assert!(matches!(&g, Gate::Deny(t) if t.contains("（file_write）")), "{c}: {g:?}");
        }
        for c in ["ls > /dev/null", "make 2>&1 | tee /dev/stderr", "cat a.txt", "echo \"a > b\"", "grep x f 2>/dev/null"] {
            assert_eq!(authorize(&p, &exec(c), None), Gate::Allow, "{c}");
        }
        // 按键在 shell 上提交（含屏幕上已敲着的半截）
        let mut shell = sit(SituationKind::Idle, "user@h:~$ echo hi ", vec![]);
        shell.program = Program::Shell;
        assert!(is_deny(&authorize(&p, &keys("> f\r"), Some(&shell))));
        // 没禁 file_write：照常
        assert!(!is_deny(&authorize(&policy("auto"), &exec("echo hi > f"), None)));
    }
}
