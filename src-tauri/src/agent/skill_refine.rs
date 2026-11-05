//! Agent 整理 skill（Phase 3.3）：让 LLM 把 3.2 的机械录制草稿整理成一份干净、可复用的 skill 草稿。
//!
//! 本模块是**纯逻辑**：提示词构造、回复解析、后处理（夹紧 + 校验）、重试 / 回退的决策，
//! 都不碰网络、`AppHandle`、数据库。真正调 LLM 的只有 [`RefineLlm`] 这个小 trait，
//! 生产实现在 `skill_cmd.rs`（复用 `llm::complete_reply`，与 Agentic 循环同一个账号 / 错误翻译），
//! 测试里注入假的实现。Tauri 命令是 `skill_cmd::skill_refine_draft`。
//!
//! # 流程
//!
//! 1. [`build_user_message`]：把草稿按账号的 `redact` 标志**逐个字符串**脱敏后序列化成 JSON，
//!    连同用户给的提示（可选）发给模型。草稿里本来就没有密码值（3.2 的保证）；这里再过一遍
//!    `redact::redact` 是和其它出网路径一致的最后一道闸（`redact=false` 是用户明确选择不脱敏）。
//! 2. 模型返回**严格 JSON**（见下）；[`parse_reply`] 宽松解析：去掉代码围栏 / 前后废话，
//!    在文本里找第一个能解析、且像 skill 的 JSON 对象。
//! 3. [`finalize`]：夹紧（见「安全夹紧」）→ 语义检查 → `skill::validate`。任何一项不过都返回问题清单。
//! 4. [`refine`]：第 1 次不过 → 把问题清单反馈给模型**重试一次**；仍不过 → **回退**到机械草稿
//!    （[`mechanical_input`]，带警告）。机械草稿也过不了校验才返回 `Err`——绝不返回无效的东西。
//!    传输 / 鉴权类错误（LLM 根本没回答）直接返回 `Err`，不静默回退，用户该知道是 Key 错了。
//!
//! # 回复 JSON 契约（camelCase，枚举 snake_case）
//!
//! ```json
//! {
//!   "name": "重启 web 服务", "description": "……\n\n前置条件：……",
//!   "params": [ { "name": "service", "description": "服务名", "default": "web", "required": false, "secret": false } ],
//!   "steps": [ { "kind": "exec", "command": "systemctl restart {{service}}",
//!               "expectAfter": "/active/i", "onUnexpected": "stop" } ],
//!   "verify": { "expect": "active (running)" },
//!   "risk": "medium",
//!   "notes": ["把 web 提取成参数 service", "删掉了重复的 ls"]
//! }
//! ```
//!
//! `steps` / `params` / `verify` 就是 3.1 的模型（`deny_unknown_fields`：多出来的字段是解析错误，会被反馈重试）。
//! 接受外面再包一层 `{"skill": {…}, "notes": […]}`；`verify` 也可以是纯字符串（当作 `expect`）。
//! `notes` 不进 skill，只返回给界面解释「为什么这么整理」。
//!
//! # 安全夹紧（模型说了不算）
//!
//! - **来源**恒为 `recorded`。
//! - **主机范围**：忽略模型给的 `hostScope`（提示词里根本不要它）。有可用的录制主机名 →
//!   `list [主机名]`（见 [`clamp_host_scope`]：取标签里第一个词、去掉 `user@`，含脱敏占位符或空则不可用）；
//!   没有可用的 → `any` 并加警告。模型想设成 `any` 也会被改回 `list`；用户要放宽，在编辑器里自己改。
//! - **风险**只升不降：`max(模型给的, 录制草稿的, assess_risk(最终步骤))`，之后 `normalize` / `validate` 再兜一次。
//! - **secret**：secret 参数的 `default` 一律清掉；名字像口令（password / token / secret / api_key …）的参数
//!   强制 `secret=true` 并清掉默认值——于是它只能出现在 secret 通道里，用在命令里会被 `validate` 拒绝并反馈给模型。
//!   `validate` 对字面口令的拒绝（`password=…`、`sk-…` 等）**原样保留**，这里不绕过。
//! - **onUnexpected**：非只读的步骤（`assess_risk` 单步 > Low）不允许交给 `llm`，改成 `ask`。
//! - **不丢密码步骤、不凭空加操作**：整理后的 secret 步骤数不得少于录制；exec / keys / expect_respond / secret
//!   的总数不得多于录制（只允许新增 `screen_check`）——模型可以删噪声，不能发明命令。
//! - **脱敏占位符**：出网时被脱敏的值（IP / 邮箱等）模型看到的是 `[REDACTED]`；
//!   它若原样写进步骤 / 默认值 / verify，视为错误并要求改成参数（不带默认值）。
//! - 长度类限制（名称 / 描述 / 参数描述）先截断并记警告，再 `validate`。
//!
//! 回退草稿用的是**本地**未脱敏的步骤（它们从没离开过本机），不是模型的输出。

use serde::Serialize;
use serde_json::{json, Value};

use super::llm::{truncate_chars, ChatMessage};
use super::recorder::RecordingDraft;
use super::redact::{self, PLACEHOLDER};
use super::skill::{
    self, assess_risk, HostScope, OnUnexpected, Risk, SkillCheck, SkillInput, SkillParam, SkillSource, SkillStep,
    MAX_DESC_CHARS, MAX_HOST_ENTRY_CHARS, MAX_NAME_CHARS, MAX_PARAM_DESC_CHARS, MAX_STEPS,
};

// ——————————————————————————————— 上限 ———————————————————————————————

/// 最多尝试几次（首次 + 一次带校验反馈的重试）。
pub const MAX_ATTEMPTS: u32 = 2;
/// 用户提示的字数上限。
pub const MAX_HINT_CHARS: usize = 500;
/// 返回给界面的 notes / warnings：条数与单条长度上限。
const MAX_NOTES: usize = 20;
const MAX_NOTE_CHARS: usize = 300;
/// 反馈给模型的问题清单最多几条（防止刷屏）。
const MAX_FEEDBACK_ERRORS: usize = 20;
/// 回退警告里最多带几条原因。
const MAX_FALLBACK_REASONS: usize = 4;
/// 回退草稿的名称前缀。
const FALLBACK_NAME: &str = "录制的操作";

// ——————————————————————————————— 对外模型 ———————————————————————————————

/// `skill_refine_draft` 的返回值。`draft` 一定通过了 `skill::validate`（回退时也是）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefinedSkillDraft {
    /// 交给 `SkillManager` 的 `initialDraft`：来源 `recorded`，范围已夹紧，风险已抬高
    pub draft: SkillInput,
    /// 模型对整理决策的说明（只展示，不进 skill）
    pub notes: Vec<String>,
    /// 我们自己的提醒：被夹紧的项、截断、回退原因……用户保存前应看一眼
    pub warnings: Vec<String>,
    /// 是否退回了机械草稿（模型两次都没给出合格结果）
    pub fallback: bool,
    /// 实际调用模型的次数（1..=2）
    pub attempts: u32,
}

/// 模型的一次调用。生产实现走 `llm::complete_reply`，测试注入假的。
/// 错误是已翻译成中文的句子。
#[allow(async_fn_in_trait)]
pub trait RefineLlm {
    async fn complete(&mut self, system: &str, messages: &[ChatMessage]) -> Result<String, String>;
}

// ——————————————————————————————— 提示词 ———————————————————————————————

/// 系统提示词：任务、输出契约、规则。固定文案，不含任何用户数据。
pub const SYSTEM_PROMPT: &str = r#"你是终端运维助手，任务是把用户录制的一段终端操作整理成一份干净、可复用的「技能（skill）」。
用户会给你一份 JSON 草稿：按顺序的步骤（steps）、每步命令输出的末尾几行（hints，stepIndex 是 steps 的 0 起下标）、录制时的提醒（warnings）。
你必须只回复**一个 JSON 对象**，不要任何解释、不要 Markdown 代码围栏。

【输出 JSON 形状】
{
  "name": "简短的技能名（≤60 字）",
  "description": "这个技能做什么（≤400 字）。前置条件单独成段，以「前置条件：」开头。",
  "params": [ { "name": "service", "description": "含义", "default": "web", "required": false, "secret": false } ],
  "steps": [ <步骤>, ... ],
  "verify": { "expect": "全部做完后屏幕上应出现的文字或 /正则/i" },
  "risk": "low|medium|high",
  "notes": ["用一句话解释你做的每个重要决定"]
}

【步骤种类（kind）与字段，字段名区分大小写，不得有别的字段】
- exec：{"kind":"exec","command":"单行命令，可含 {{参数}}"}
- keys：{"kind":"keys","keys":"<C-c>"}  按键串，如 <C-c> <Enter>
- expect_respond：{"kind":"expect_respond","expect":"屏幕上出现的提示","respond":"y<Enter>","secret":false}
- secret：{"kind":"secret","reason":"给用户看的输入理由","param":"可选的 secret 参数名"}
- screen_check：{"kind":"screen_check","expect":"屏幕上应有的文字"}
每个步骤还可带：timeoutMs（毫秒数）、expectAfter（做完这步后屏幕上应出现的文字或 /正则/i，写稳定的那一小段）、onUnexpected（"ask"|"stop"|"llm"）。

【整理规则】
1. 保持步骤的先后顺序与语义；删掉噪声和明显多余的步骤（重复的 ls / pwd / echo 之类）。不要新增录制里没有的命令（可以新增 screen_check 检查步骤）。
2. 提取参数：看起来只属于这一次的值（主机名 / 路径 / 版本号 / 容器名 / 服务名 / 端口……）改成 {{参数名}}，并在 params 里声明（参数名只能是字母数字下划线、不以数字开头；写 description；有把握时给 default）。通用的、每次都一样的东西不要参数化。
3. 密码 / 口令 / token / 私钥永远不是普通参数：不要写进任何命令、默认值或应答里；录制里的 secret 步骤原样保留（可改善 reason 文字）。需要引用时声明 secret 参数（secret:true，没有 default，required 可为 true），只能用于 secret 步骤的 param 或 secret:true 的 expect_respond。
4. 草稿里被脱敏的值显示为 [REDACTED]：它不是真实内容。遇到它就把对应位置改成 {{参数}}，且该参数**不要写 default**；不要把 [REDACTED] 原样写进输出。
5. 利用 hints 里的命令输出，为关键步骤填写 expectAfter（成功时一定会出现的稳定文字）；拿不准就不写。最后加一个 verify，检查整件事做成了。
6. onUnexpected：只读步骤（查看状态 / 列目录 / 看日志）用 "ask" 或 "llm"；会修改系统的步骤（重启、安装、删除、写文件、部署）用 "stop" 或 "ask"。
7. 前置条件（需要先 cd 到哪里、需要什么权限 / 软件 / 网络、在哪台机器上跑）写在 description 的「前置条件：」段里；模型没有单独的前置条件字段。
8. risk 如实评估（会改动系统 / 用 sudo / 删除类为 medium 或 high）。不要输出 hostScope 与 source，它们由程序决定。
9. 只输出 JSON。"#;

/// 逐个字符串脱敏（`redact_on` 为真时）。序列化**之前**对每个值单独做：
/// 整段 JSON 文本上跑正则会把引号 / 转义吃掉，弄坏结构。
fn scrub_value(v: &mut Value, redact_on: bool) {
    if !redact_on {
        return;
    }
    match v {
        Value::String(s) => *s = redact::redact(s),
        Value::Array(a) => a.iter_mut().for_each(|x| scrub_value(x, true)),
        Value::Object(m) => m.values_mut().for_each(|x| scrub_value(x, true)),
        _ => {}
    }
}

/// 发给模型的草稿 JSON：只含整理需要的东西（主机标签、步骤、hints、提醒、风险）。
/// 不含会话 id / 时间戳。`redact_on` 为真时每个字符串都过 `redact::redact`。
pub fn draft_payload(draft: &RecordingDraft, redact_on: bool) -> Value {
    let mut v = json!({
        "host": draft.host.label,
        "risk": draft.risk,
        "steps": draft.steps,
        "hints": draft.hints,
        "warnings": draft.warnings,
    });
    scrub_value(&mut v, redact_on);
    v
}

/// 清理用户给的提示：去控制字符、截断；`redact_on` 时同样脱敏（与草稿里的 `[REDACTED]` 一致）。
fn clean_hint(hint: Option<&str>, redact_on: bool) -> Option<String> {
    let t: String = hint?.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let t = t.trim();
    if t.is_empty() {
        return None;
    }
    let t = truncate_chars(t, MAX_HINT_CHARS);
    Some(if redact_on { redact::redact(&t) } else { t })
}

/// 首轮 user 消息。
pub fn build_user_message(draft: &RecordingDraft, hint: Option<&str>, redact_on: bool) -> String {
    let payload = serde_json::to_string_pretty(&draft_payload(draft, redact_on)).unwrap_or_else(|_| "{}".into());
    let mut msg = format!("下面是录制草稿：\n{payload}\n");
    if let Some(h) = clean_hint(hint, redact_on) {
        msg.push_str(&format!("\n用户的补充说明：{h}\n"));
    }
    msg.push_str("\n请按规则整理，只回复一个 JSON 对象。");
    msg
}

/// 重试时的 user 消息：把问题清单交还给模型。
pub fn retry_feedback(errors: &[String]) -> String {
    let list: String = errors
        .iter()
        .take(MAX_FEEDBACK_ERRORS)
        .map(|e| format!("- {}\n", truncate_chars(e, MAX_NOTE_CHARS)))
        .collect();
    format!(
        "你上一次的回复没有通过程序校验，问题如下：\n{list}\n请修正这些问题后重新回复完整的 JSON 对象（仍然只输出 JSON，不要解释）。"
    )
}

// ——————————————————————————————— 解析 ———————————————————————————————

/// 解析后的模型回复（尚未夹紧 / 校验）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Parsed {
    pub name: String,
    pub description: String,
    pub params: Vec<SkillParam>,
    pub steps: Vec<SkillStep>,
    pub verify: Option<SkillCheck>,
    pub risk: Option<Risk>,
    pub notes: Vec<String>,
}

/// 从 `start`（必须指向 `{`）起，找与之配平的 `}`（识别字符串与转义）。返回切片。
fn balanced_object(text: &str, start: usize) -> Option<&str> {
    let (mut depth, mut in_str, mut esc) = (0usize, false, false);
    for (i, &b) in text.as_bytes().iter().enumerate().skip(start) {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// 在文本里找第一个「能解析成对象、且像 skill（有 `steps` 或 `skill`）」的 JSON。
/// 代码围栏、前后废话（甚至废话里的 `{{param}}`）都不碍事。
pub fn extract_json_object(text: &str) -> Option<Value> {
    for (i, c) in text.char_indices() {
        if c != '{' {
            continue;
        }
        let Some(slice) = balanced_object(text, i) else { continue };
        if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(slice) {
            if m.contains_key("steps") || m.contains_key("skill") {
                return Some(Value::Object(m));
            }
        }
    }
    None
}

fn parse_notes(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str()).map(str::to_string).collect(),
        Some(Value::String(s)) if !s.trim().is_empty() => vec![s.clone()],
        _ => Vec::new(),
    }
}

/// 宽松解析模型回复。错误文案会原样反馈给模型。
pub fn parse_reply(text: &str) -> Result<Parsed, String> {
    let root = extract_json_object(text)
        .ok_or_else(|| "回复里找不到 JSON 对象（需要一个含 name / steps 的 JSON，不要用自然语言回答）".to_string())?;
    let Value::Object(root) = root else { return Err("回复不是 JSON 对象".into()) };
    // 兼容 {"skill": {...}, "notes": [...]}
    let (obj, outer_notes) = match root.get("skill") {
        Some(Value::Object(inner)) => (inner.clone(), root.get("notes").cloned()),
        _ => (root.clone(), None),
    };

    let mut out = Parsed {
        name: obj.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
        description: obj.get("description").and_then(Value::as_str).unwrap_or_default().to_string(),
        ..Parsed::default()
    };
    out.notes = parse_notes(obj.get("notes").or(outer_notes.as_ref()));

    match obj.get("steps") {
        Some(Value::Array(a)) => {
            let mut errs = Vec::new();
            for (i, s) in a.iter().enumerate() {
                match serde_json::from_value::<SkillStep>(s.clone()) {
                    Ok(step) => out.steps.push(step),
                    Err(e) => errs.push(format!("steps[{i}] 无法解析：{e}")),
                }
            }
            if !errs.is_empty() {
                return Err(errs.join("\n"));
            }
        }
        _ => return Err("缺少 steps 数组".into()),
    }
    match obj.get("params") {
        None | Some(Value::Null) => {}
        Some(Value::Array(a)) => {
            let mut errs = Vec::new();
            for (i, p) in a.iter().enumerate() {
                match serde_json::from_value::<SkillParam>(p.clone()) {
                    Ok(p) => out.params.push(p),
                    Err(e) => errs.push(format!("params[{i}] 无法解析：{e}")),
                }
            }
            if !errs.is_empty() {
                return Err(errs.join("\n"));
            }
        }
        Some(_) => return Err("params 必须是数组".into()),
    }
    out.verify = match obj.get("verify") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.trim().is_empty() => None,
        Some(Value::String(s)) => Some(SkillCheck { expect: s.clone(), timeout_ms: None }),
        Some(v) => Some(serde_json::from_value::<SkillCheck>(v.clone()).map_err(|e| format!("verify 无法解析：{e}"))?),
    };
    // 风险：认不出的值当没给（反正只会被抬高）
    out.risk = obj.get("risk").and_then(|v| serde_json::from_value::<Risk>(v.clone()).ok());
    Ok(out)
}

// ——————————————————————————————— 夹紧 ———————————————————————————————

/// 参数名像口令 / 令牌 / 密钥。宁可多判：判中只会让它变成 secret 参数（不能写进命令，会被校验拦下反馈给模型）。
fn credential_like_name(name: &str) -> bool {
    let n = name.to_lowercase();
    ["password", "passwd", "passphrase", "pwd", "secret", "token", "api_key", "apikey", "credential", "private_key"]
        .iter()
        .any(|k| n.contains(k))
}

/// 录制主机名 → 可写进 `hostScope` 的条目。
///
/// 标签是前端的标签名（3.2 已脱敏、限长），形态不定：`prod-web`、`root@host`、`prod web (2)`。
/// 取第一个空白分隔的词，去掉 `user@` 前缀；空、含脱敏占位符、过长、含控制字符的视为不可用。
pub fn host_entry(label: &str) -> Option<String> {
    let first = label.split_whitespace().next()?;
    let host = first.rsplit('@').next().unwrap_or(first).trim();
    if host.is_empty()
        || host.contains(PLACEHOLDER)
        || host.chars().count() > MAX_HOST_ENTRY_CHARS
        || host.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return None;
    }
    Some(host.to_string())
}

/// 主机范围夹紧：**无视模型**，由录制的主机决定。返回（范围，可选警告）。
/// 有可用主机名 → `list [主机名]`；否则 `any` 并提醒用户在编辑器里确认。
pub fn clamp_host_scope(draft: &RecordingDraft) -> (HostScope, Option<String>) {
    match host_entry(&draft.host.label) {
        Some(h) => (HostScope::List { hosts: vec![h] }, None),
        None => (
            HostScope::Any,
            Some("无法从录制的标签确定主机名，适用主机已设为「不限」；请在编辑器里确认是否要限定主机。".into()),
        ),
    }
}

fn set_on_unexpected(step: &mut SkillStep, v: OnUnexpected) {
    match step {
        SkillStep::Exec { on_unexpected, .. }
        | SkillStep::Keys { on_unexpected, .. }
        | SkillStep::ExpectRespond { on_unexpected, .. }
        | SkillStep::Secret { on_unexpected, .. }
        | SkillStep::ScreenCheck { on_unexpected, .. } => *on_unexpected = v,
    }
}

/// 是否「口令输入」步骤（secret 步骤，或 secret 应答）。
fn is_secret_step(s: &SkillStep) -> bool {
    matches!(s, SkillStep::Secret { .. } | SkillStep::ExpectRespond { secret: true, .. })
}

/// 是否会向终端做操作（相对只检查屏幕的 screen_check）。
fn is_action_step(s: &SkillStep) -> bool {
    !matches!(s, SkillStep::ScreenCheck { .. })
}

/// 步骤里所有文本字段（用来查 `[REDACTED]`）。
fn step_texts(s: &SkillStep) -> Vec<&str> {
    let mut v: Vec<&str> = match s {
        SkillStep::Exec { command, .. } => vec![command],
        SkillStep::Keys { keys, .. } => vec![keys],
        SkillStep::ExpectRespond { expect, respond, .. } => vec![expect, respond],
        SkillStep::Secret { reason, param, .. } => {
            let mut v = vec![reason.as_str()];
            v.extend(param.as_deref());
            v
        }
        SkillStep::ScreenCheck { expect, .. } => vec![expect],
    };
    v.extend(s.expect_after());
    v
}

fn clip(s: &str, max: usize, what: &str, warnings: &mut Vec<String>) -> String {
    if s.chars().count() > max {
        warnings.push(format!("{what}超过 {max} 字符，已截断"));
        // truncate_chars 会在末尾追加「…」，所以先留一个字符的位置，保证总长 ≤ max
        truncate_chars(s, max.saturating_sub(1))
    } else {
        s.to_string()
    }
}

/// 夹紧 + 语义检查 + 校验的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finalized {
    pub input: SkillInput,
    pub notes: Vec<String>,
    pub warnings: Vec<String>,
}

/// 把模型的回复变成一份合格的 `SkillInput`，或返回**全部**问题（反馈给模型 / 用于回退警告）。
/// 夹紧规则见模块文档。
pub fn finalize(parsed: Parsed, draft: &RecordingDraft) -> Result<Finalized, Vec<String>> {
    let mut warnings: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let Parsed { name, description, mut params, mut steps, verify, risk, notes } = parsed;

    // —— 长度类：截断并提醒，不当成硬错误 ——
    let name = clip(name.trim(), MAX_NAME_CHARS, "名称", &mut warnings);
    let description = clip(description.trim(), MAX_DESC_CHARS, "描述", &mut warnings);
    for p in &mut params {
        p.description = clip(&p.description, MAX_PARAM_DESC_CHARS, &format!("参数「{}」的描述", p.name), &mut warnings);
    }

    // —— secret：不许带默认值；像口令的参数强制 secret ——
    for p in &mut params {
        if !p.secret && credential_like_name(&p.name) {
            p.secret = true;
            warnings.push(format!("参数「{}」名字像口令 / 令牌，已强制设为 secret（运行时由密码通道提供）。", p.name));
        }
        if p.secret && p.default_value.take().is_some() {
            warnings.push(format!("secret 参数「{}」不允许有默认值，已清除。", p.name));
        }
    }

    // —— 脱敏占位符不得进入功能性字段 ——
    for (i, s) in steps.iter().enumerate() {
        if step_texts(s).iter().any(|t| t.contains(PLACEHOLDER)) {
            errors.push(format!(
                "第 {} 步含脱敏占位符 {PLACEHOLDER}：那是被隐藏的真实值，请把对应位置改成 {{{{参数名}}}} 并声明参数（不要带默认值）",
                i + 1
            ));
        }
    }
    for p in &params {
        if p.default_value.as_deref().is_some_and(|d| d.contains(PLACEHOLDER)) {
            errors.push(format!("参数「{}」的默认值含脱敏占位符 {PLACEHOLDER}，请去掉 default", p.name));
        }
    }
    if verify.as_ref().is_some_and(|v| v.expect.contains(PLACEHOLDER)) {
        errors.push(format!("verify 含脱敏占位符 {PLACEHOLDER}，请改写或去掉"));
    }

    // —— 不丢密码步骤、不凭空加操作 ——
    let orig_secret = draft.steps.iter().filter(|s| is_secret_step(s)).count();
    let new_secret = steps.iter().filter(|s| is_secret_step(s)).count();
    if new_secret < orig_secret {
        errors.push(format!(
            "录制里有 {orig_secret} 个密码输入步骤，整理后只剩 {new_secret} 个：密码步骤不能删除或合并"
        ));
    }
    let orig_actions = draft.steps.iter().filter(|s| is_action_step(s)).count();
    let new_actions = steps.iter().filter(|s| is_action_step(s)).count();
    if new_actions > orig_actions {
        errors.push(format!(
            "整理后有 {new_actions} 个操作步骤，多于录制的 {orig_actions} 个：不能新增录制里没有的命令 / 按键（只允许新增 screen_check 检查步骤）"
        ));
    }
    if steps.len() > MAX_STEPS {
        errors.push(format!("步骤最多 {MAX_STEPS} 个"));
    }

    // —— onUnexpected：非只读步骤不许交给 LLM ——
    for (i, s) in steps.iter_mut().enumerate() {
        if s.on_unexpected() == OnUnexpected::Llm && assess_risk(std::slice::from_ref(s)).risk > Risk::Low {
            set_on_unexpected(s, OnUnexpected::Ask);
            warnings.push(format!(
                "第 {} 步会修改系统，onUnexpected 由 llm 改为 ask（改动系统的步骤不交给 LLM 自行处理）。",
                i + 1
            ));
        }
    }

    // —— 范围 / 来源 / 风险 ——
    let (host_scope, scope_warn) = clamp_host_scope(draft);
    warnings.extend(scope_warn);
    let risk = risk
        .unwrap_or_default()
        .max(draft.risk)
        .max(assess_risk(&steps).risk);

    let input = SkillInput {
        name,
        description,
        params,
        steps,
        verify,
        risk,
        host_scope,
        source: Some(SkillSource::Recorded),
    };

    // —— 校验（不放宽）：规整后的内容必须通过 ——
    let skill = input.into_skill(String::new(), 0);
    if let Err(mut e) = skill::validate(&skill) {
        errors.append(&mut e);
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(Finalized {
        input: SkillInput::from(&skill),
        notes: cap_messages(notes),
        warnings: cap_messages(warnings),
    })
}

fn cap_messages(v: Vec<String>) -> Vec<String> {
    v.into_iter()
        .map(|s| truncate_chars(s.trim(), MAX_NOTE_CHARS))
        .filter(|s| !s.is_empty())
        .take(MAX_NOTES)
        .collect()
}

// ——————————————————————————————— 回退 ———————————————————————————————

/// 机械草稿 → `SkillInput`（不经模型）：步骤原样、没有参数 / verify，范围同样夹紧，风险取录制值再抬高。
/// 本地数据，不脱敏。过不了校验返回问题清单。
pub fn mechanical_input(draft: &RecordingDraft) -> Result<(SkillInput, Vec<String>), Vec<String>> {
    let mut warnings = Vec::new();
    let (host_scope, w) = clamp_host_scope(draft);
    warnings.extend(w);
    let label = host_entry(&draft.host.label).unwrap_or_default();
    let name = if label.is_empty() { FALLBACK_NAME.to_string() } else { format!("{FALLBACK_NAME}（{label}）") };
    let input = SkillInput {
        name: truncate_chars(&name, MAX_NAME_CHARS),
        description: "由终端录制直接生成的草稿，未经整理：没有提取参数、没有期望与验证，请编辑后再保存。".into(),
        params: Vec::new(),
        steps: draft.steps.clone(),
        verify: None,
        risk: draft.risk,
        host_scope,
        source: Some(SkillSource::Recorded),
    };
    let skill = input.into_skill(String::new(), 0);
    skill::validate(&skill)?;
    Ok((SkillInput::from(&skill), warnings))
}

// ——————————————————————————————— 决策与驱动 ———————————————————————————————

/// 一次尝试没过之后怎么办。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// 带着问题清单再问一次
    Retry(String),
    /// 用尽次数，退回机械草稿
    Fallback,
}

/// 第 `attempt`（从 1 数）次没通过后的决策：还有次数就重试（反馈问题清单），否则回退。
pub fn decide_after_failure(attempt: u32, errors: &[String]) -> Decision {
    if attempt < MAX_ATTEMPTS {
        Decision::Retry(retry_feedback(errors))
    } else {
        Decision::Fallback
    }
}

fn fallback_warning(errors: &[String]) -> String {
    let why: Vec<String> = errors.iter().take(MAX_FALLBACK_REASONS).map(|e| truncate_chars(e, 120)).collect();
    format!("Agent 整理的结果两次都没有通过校验，已退回为未整理的录制草稿。原因：{}", why.join("；"))
}

/// 整理一份草稿。
///
/// `redact_on` 来自账号的 `redact` 标志。LLM 自身的失败（鉴权 / 网络 / 空回复）原样返回 `Err`；
/// 模型回复不合格则按 [`decide_after_failure`] 重试 / 回退。
pub async fn refine<L: RefineLlm>(
    llm: &mut L,
    draft: &RecordingDraft,
    hint: Option<&str>,
    redact_on: bool,
) -> Result<RefinedSkillDraft, String> {
    if draft.steps.is_empty() {
        return Err("草稿里没有可整理的步骤".into());
    }
    if draft.steps.len() > MAX_STEPS {
        return Err(format!("草稿步骤过多（最多 {MAX_STEPS} 步）"));
    }
    let mut messages = vec![ChatMessage::user(build_user_message(draft, hint, redact_on))];
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let reply = llm.complete(SYSTEM_PROMPT, &messages).await?;
        let outcome = parse_reply(&reply).map_err(|e| vec![e]).and_then(|p| finalize(p, draft));
        let errors = match outcome {
            Ok(f) => {
                return Ok(RefinedSkillDraft {
                    draft: f.input,
                    notes: f.notes,
                    warnings: f.warnings,
                    fallback: false,
                    attempts: attempt,
                })
            }
            Err(e) => e,
        };
        match decide_after_failure(attempt, &errors) {
            Decision::Retry(feedback) => {
                messages.push(ChatMessage::assistant(reply));
                messages.push(ChatMessage::user(feedback));
            }
            Decision::Fallback => {
                let (input, mut warnings) = mechanical_input(draft)
                    .map_err(|e| format!("Agent 整理失败，且录制草稿本身也未通过校验：{}", e.join("；")))?;
                warnings.insert(0, fallback_warning(&errors));
                return Ok(RefinedSkillDraft {
                    draft: input,
                    notes: Vec::new(),
                    warnings: cap_messages(warnings),
                    fallback: true,
                    attempts: attempt,
                });
            }
        }
    }
}

// ——————————————————————————————— 测试 ———————————————————————————————

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::recorder::{DraftStats, RecordingHost, StepHint};

    fn exec(cmd: &str) -> SkillStep {
        SkillStep::Exec { command: cmd.into(), timeout_ms: None, expect_after: None, on_unexpected: OnUnexpected::Ask }
    }

    fn secret_step() -> SkillStep {
        SkillStep::Secret {
            reason: "sudo 密码".into(),
            param: None,
            timeout_ms: None,
            expect_after: None,
            on_unexpected: OnUnexpected::Ask,
        }
    }

    fn draft_with(label: &str, steps: Vec<SkillStep>) -> RecordingDraft {
        let risk = assess_risk(&steps).risk;
        RecordingDraft {
            session: "tab-1".into(),
            host: RecordingHost { session_id: "tab-1".into(), label: label.into() },
            started_at_ms: 0,
            ended_at_ms: 1000,
            source: SkillSource::Recorded,
            steps,
            hints: vec![],
            risk,
            warnings: vec![],
            notes: vec![],
            stats: DraftStats::default(),
        }
    }

    fn base_draft() -> RecordingDraft {
        draft_with("prod-web", vec![exec("cd /srv/app"), exec("git pull"), exec("systemctl status web")])
    }

    /// 合格回复（对应 base_draft）
    fn good_reply() -> String {
        r#"{"name":"更新并查看 web","description":"拉代码。\n\n前置条件：在 prod-web 上。",
            "params":[{"name":"dir","description":"项目目录","default":"/srv/app","required":false,"secret":false}],
            "steps":[
              {"kind":"exec","command":"cd {{dir}}","onUnexpected":"stop"},
              {"kind":"exec","command":"git pull","expectAfter":"Already up to date","onUnexpected":"stop"},
              {"kind":"exec","command":"systemctl status web","expectAfter":"/active/i","onUnexpected":"llm"}],
            "verify":{"expect":"active (running)"},"risk":"low","notes":["提取了 dir 参数"]}"#
            .to_string()
    }

    /// 脚本化的假 LLM：按序返回；记录收到的消息。
    struct Fake {
        replies: Vec<Result<String, String>>,
        seen: Vec<Vec<ChatMessage>>,
        systems: Vec<String>,
    }
    impl Fake {
        fn new(replies: Vec<Result<String, String>>) -> Self {
            Self { replies, seen: vec![], systems: vec![] }
        }
    }
    impl RefineLlm for Fake {
        async fn complete(&mut self, system: &str, messages: &[ChatMessage]) -> Result<String, String> {
            self.systems.push(system.to_string());
            self.seen.push(messages.to_vec());
            if self.replies.is_empty() {
                return Err("没有更多脚本回复".into());
            }
            self.replies.remove(0)
        }
    }

    fn run<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    // ——— 解析宽松度 ———

    #[test]
    fn parse_plain_json() {
        let p = parse_reply(&good_reply()).unwrap();
        assert_eq!(p.name, "更新并查看 web");
        assert_eq!(p.steps.len(), 3);
        assert_eq!(p.params[0].name, "dir");
        assert_eq!(p.verify.unwrap().expect, "active (running)");
        assert_eq!(p.notes, vec!["提取了 dir 参数"]);
    }

    #[test]
    fn parse_strips_code_fences_and_chatter() {
        let wrapped = format!("好的，整理如下：\n```json\n{}\n```\n希望有帮助 {{{{x}}}}", good_reply());
        let p = parse_reply(&wrapped).unwrap();
        assert_eq!(p.steps.len(), 3);
    }

    #[test]
    fn parse_skips_leading_braces_that_are_not_json() {
        let text = format!("参数写成 {{{{host}}}} 的形式。\n}} 多余的括号\n{}", good_reply());
        assert!(parse_reply(&text).is_ok());
    }

    #[test]
    fn parse_accepts_skill_wrapper_and_string_verify() {
        let text = r#"{"skill":{"name":"a","steps":[{"kind":"exec","command":"ls"}],"verify":"ok"},"notes":["n1"]}"#;
        let p = parse_reply(text).unwrap();
        assert_eq!(p.verify.unwrap().expect, "ok");
        assert_eq!(p.notes, vec!["n1"]);
    }

    #[test]
    fn parse_braces_inside_strings_do_not_confuse_scanner() {
        let text = r#"{"name":"a","steps":[{"kind":"exec","command":"docker inspect -f '{{.Name}}' x }"}]}"#;
        let p = parse_reply(text).unwrap();
        assert!(matches!(&p.steps[0], SkillStep::Exec { command, .. } if command.contains("}}' x }")));
    }

    #[test]
    fn parse_errors_are_actionable() {
        assert!(parse_reply("我无法完成").unwrap_err().contains("JSON"));
        let bad = r#"{"name":"a","steps":[{"kind":"exec","command":"ls","password":"x"}]}"#;
        let e = parse_reply(bad).unwrap_err();
        assert!(e.contains("steps[0]"), "{e}");
        let bad2 = r#"{"name":"a","steps":[{"kind":"teleport"}]}"#;
        assert!(parse_reply(bad2).unwrap_err().contains("steps[0]"));
        let bad3 = r#"{"name":"a","steps":"ls"}"#;
        assert!(parse_reply(bad3).is_err());
    }

    #[test]
    fn parse_unknown_risk_is_ignored() {
        let p = parse_reply(r#"{"name":"a","risk":"extreme","steps":[{"kind":"exec","command":"ls"}]}"#).unwrap();
        assert_eq!(p.risk, None);
    }

    // ——— 夹紧 ———

    #[test]
    fn finalize_good_reply_ok_and_recorded_source() {
        let d = base_draft();
        let f = finalize(parse_reply(&good_reply()).unwrap(), &d).unwrap();
        assert_eq!(f.input.source, Some(SkillSource::Recorded));
        assert_eq!(f.input.steps.len(), 3);
        // cd {{dir}} 含占位符 → 至少 medium；git pull 非只读 → medium
        assert!(f.input.risk >= Risk::Medium);
        assert!(skill::validate(&f.input.clone().into_skill(String::new(), 0)).is_ok());
    }

    #[test]
    fn host_scope_is_clamped_to_recorded_host() {
        let d = base_draft();
        let f = finalize(parse_reply(&good_reply()).unwrap(), &d).unwrap();
        assert_eq!(f.input.host_scope, HostScope::List { hosts: vec!["prod-web".into()] });
        // 模型就算写了 any / 别的主机，也被无视（parse 不读 hostScope）
        let text = good_reply().replacen('{', r#"{"hostScope":{"kind":"any"},"#, 1);
        let f = finalize(parse_reply(&text).unwrap(), &d).unwrap();
        assert_eq!(f.input.host_scope, HostScope::List { hosts: vec!["prod-web".into()] });
    }

    #[test]
    fn host_entry_heuristics() {
        assert_eq!(host_entry("prod-web").as_deref(), Some("prod-web"));
        assert_eq!(host_entry("root@db1 (2)").as_deref(), Some("db1"));
        assert_eq!(host_entry("prod web").as_deref(), Some("prod"));
        assert_eq!(host_entry("root@[REDACTED]"), None);
        assert_eq!(host_entry("   "), None);
        assert_eq!(host_entry(&"h".repeat(300)), None);
    }

    #[test]
    fn host_scope_any_only_when_no_usable_host() {
        let d = draft_with("", vec![exec("ls")]);
        let (scope, warn) = clamp_host_scope(&d);
        assert_eq!(scope, HostScope::Any);
        assert!(warn.is_some());
        let d = draft_with("root@[REDACTED]", vec![exec("ls")]);
        assert_eq!(clamp_host_scope(&d).0, HostScope::Any);
    }

    #[test]
    fn risk_never_goes_down() {
        // 录制里有 sudo（High）；模型把它删成只读命令并声称 low：风险仍不低于录制值
        let d = draft_with("h", vec![exec("sudo systemctl restart web"), secret_step()]);
        assert_eq!(d.risk, Risk::High);
        let text = r#"{"name":"x","steps":[{"kind":"exec","command":"ls"},{"kind":"secret","reason":"sudo 密码"}],"risk":"low"}"#;
        let f = finalize(parse_reply(text).unwrap(), &d).unwrap();
        assert_eq!(f.input.risk, Risk::High);
    }

    #[test]
    fn risk_is_raised_to_assessed_and_model_may_raise_further() {
        let d = draft_with("h", vec![exec("ls")]);
        let text = r#"{"name":"x","steps":[{"kind":"exec","command":"ls"}],"risk":"low"}"#;
        assert_eq!(finalize(parse_reply(text).unwrap(), &d).unwrap().input.risk, Risk::Low);
        let text = r#"{"name":"x","steps":[{"kind":"exec","command":"ls"}],"risk":"high"}"#;
        assert_eq!(finalize(parse_reply(text).unwrap(), &d).unwrap().input.risk, Risk::High);
        // 模型低报一个会改系统的步骤：被 assess_risk 抬到 Medium
        let d = draft_with("h", vec![exec("touch /tmp/x")]);
        let text = r#"{"name":"x","steps":[{"kind":"exec","command":"touch /tmp/x"}],"risk":"low"}"#;
        assert_eq!(finalize(parse_reply(text).unwrap(), &d).unwrap().input.risk, Risk::Medium);
    }

    #[test]
    fn secret_param_default_is_stripped_and_credential_names_forced_secret() {
        let d = draft_with("h", vec![exec("ls"), secret_step()]);
        let text = r#"{"name":"x","params":[
            {"name":"db_password","default":"hunter2","required":false,"secret":false},
            {"name":"tok","default":"abc","required":false,"secret":true}],
          "steps":[{"kind":"exec","command":"ls"},{"kind":"secret","reason":"口令","param":"db_password"}]}"#;
        let f = finalize(parse_reply(text).unwrap(), &d).unwrap();
        let pw = f.input.params.iter().find(|p| p.name == "db_password").unwrap();
        assert!(pw.secret && pw.default_value.is_none());
        let tok = f.input.params.iter().find(|p| p.name == "tok").unwrap();
        assert!(tok.secret && tok.default_value.is_none());
        assert!(f.warnings.iter().any(|w| w.contains("db_password")));
    }

    #[test]
    fn password_param_used_in_command_is_rejected_not_weakened() {
        let d = draft_with("h", vec![exec("mysql -p123456")]);
        let text = r#"{"name":"x","params":[{"name":"db_password","default":"123456"}],
          "steps":[{"kind":"exec","command":"mysql -p{{db_password}}"}]}"#;
        let errs = finalize(parse_reply(text).unwrap(), &d).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("secret")), "{errs:?}");
    }

    #[test]
    fn literal_credentials_still_rejected_by_validate() {
        let d = draft_with("h", vec![exec("ls")]);
        let text = r#"{"name":"x","steps":[{"kind":"exec","command":"curl -H 'Authorization: Bearer abcdefghijklmnop' x"}]}"#;
        assert!(finalize(parse_reply(text).unwrap(), &d).is_err());
        let text = r#"{"name":"x","steps":[{"kind":"exec","command":"mysql --password=hunter22"}]}"#;
        assert!(finalize(parse_reply(text).unwrap(), &d).is_err());
    }

    #[test]
    fn on_unexpected_llm_downgraded_for_mutating_steps_only() {
        let d = draft_with("h", vec![exec("systemctl restart web"), exec("systemctl status web")]);
        let text = r#"{"name":"x","steps":[
            {"kind":"exec","command":"systemctl restart web","onUnexpected":"llm"},
            {"kind":"exec","command":"systemctl status web","onUnexpected":"llm"}]}"#;
        let f = finalize(parse_reply(text).unwrap(), &d).unwrap();
        assert_eq!(f.input.steps[0].on_unexpected(), OnUnexpected::Ask);
        assert_eq!(f.input.steps[1].on_unexpected(), OnUnexpected::Llm);
        assert!(f.warnings.iter().any(|w| w.contains("第 1 步")));
    }

    #[test]
    fn dropping_secret_step_is_rejected() {
        let d = draft_with("h", vec![exec("sudo ls"), secret_step()]);
        let text = r#"{"name":"x","steps":[{"kind":"exec","command":"sudo ls"}]}"#;
        let errs = finalize(parse_reply(text).unwrap(), &d).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("密码输入步骤")), "{errs:?}");
    }

    #[test]
    fn inventing_actions_is_rejected_but_screen_check_allowed() {
        let d = draft_with("h", vec![exec("ls")]);
        let text = r#"{"name":"x","steps":[{"kind":"exec","command":"ls"},{"kind":"exec","command":"whoami"}]}"#;
        let errs = finalize(parse_reply(text).unwrap(), &d).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("多于录制")), "{errs:?}");
        let text = r#"{"name":"x","steps":[{"kind":"exec","command":"ls"},{"kind":"screen_check","expect":"ok"}]}"#;
        assert!(finalize(parse_reply(text).unwrap(), &d).is_ok());
    }

    #[test]
    fn redaction_placeholder_in_steps_or_defaults_is_rejected() {
        let d = draft_with("h", vec![exec("ping 10.0.0.1")]);
        let text = r#"{"name":"x","steps":[{"kind":"exec","command":"ping [REDACTED]"}]}"#;
        let errs = finalize(parse_reply(text).unwrap(), &d).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("脱敏占位符")), "{errs:?}");
        let text = r#"{"name":"x","params":[{"name":"host","default":"[REDACTED]"}],"steps":[{"kind":"exec","command":"ping {{host}}"}]}"#;
        assert!(finalize(parse_reply(text).unwrap(), &d).is_err());
        let text = r#"{"name":"x","params":[{"name":"host"}],"steps":[{"kind":"exec","command":"ping {{host}}"}]}"#;
        assert!(finalize(parse_reply(text).unwrap(), &d).is_ok());
    }

    #[test]
    fn overlong_text_is_truncated_with_warning() {
        let d = draft_with("h", vec![exec("ls")]);
        let long = "述".repeat(600);
        let text = format!(r#"{{"name":"x","description":"{long}","steps":[{{"kind":"exec","command":"ls"}}]}}"#);
        let f = finalize(parse_reply(&text).unwrap(), &d).unwrap();
        assert!(f.input.description.chars().count() <= MAX_DESC_CHARS);
        assert!(f.warnings.iter().any(|w| w.contains("描述")));
    }

    #[test]
    fn invalid_skill_reports_all_validation_errors() {
        let d = draft_with("h", vec![exec("ls")]);
        let text = r#"{"name":"","steps":[{"kind":"exec","command":"echo {{undeclared}}"}]}"#;
        let errs = finalize(parse_reply(text).unwrap(), &d).unwrap_err();
        assert!(errs.len() >= 2, "{errs:?}");
    }

    // ——— 提示词与脱敏 ———

    fn leaky_draft() -> RecordingDraft {
        let mut d = draft_with(
            "root@10.9.8.7",
            vec![exec("scp a.tar admin@build.example.com:/tmp"), exec("ping 192.168.5.5")],
        );
        d.hints = vec![StepHint { step_index: 1, output_tail: "token=abcd1234efgh from 172.16.0.9".into() }];
        d.warnings = vec!["第 1 步：目标 admin@build.example.com".into()];
        d
    }

    #[test]
    fn prompt_is_redacted_when_flag_on() {
        let msg = build_user_message(&leaky_draft(), Some("把 10.1.1.1 参数化，邮箱 a@b.com"), true);
        for secret in [
            "10.9.8.7",
            "192.168.5.5",
            "172.16.0.9",
            "abcd1234efgh",
            "admin@build.example.com",
            "10.1.1.1",
            "a@b.com",
        ] {
            assert!(!msg.contains(secret), "泄漏了 {secret}：\n{msg}");
        }
        assert!(msg.contains(PLACEHOLDER));
        assert!(!msg.contains("tab-1"), "不应发送会话 id");
    }

    #[test]
    fn prompt_keeps_originals_when_flag_off() {
        let msg = build_user_message(&leaky_draft(), None, false);
        assert!(msg.contains("192.168.5.5") && msg.contains("build.example.com"));
    }

    #[test]
    fn prompt_json_stays_parseable_after_redaction() {
        let d = draft_with("h", vec![exec(r#"echo "password=\"x y\" ok""#)]);
        let payload = draft_payload(&d, true);
        let s = serde_json::to_string(&payload).unwrap();
        assert!(serde_json::from_str::<Value>(&s).is_ok());
        assert_eq!(payload["steps"][0]["kind"], "exec");
    }

    #[test]
    fn hint_is_clamped_and_control_chars_removed() {
        let long = format!("a\nb\x07{}", "长".repeat(900));
        let msg = build_user_message(&base_draft(), Some(&long), false);
        let tail = msg.split("用户的补充说明：").nth(1).unwrap();
        let line = tail.lines().next().unwrap();
        assert!(line.chars().count() <= MAX_HINT_CHARS + 2);
        assert!(!line.contains('\x07'));
        assert!(build_user_message(&base_draft(), Some("   "), false).find("补充说明").is_none());
    }

    #[test]
    fn system_prompt_documents_contract_and_never_asks_for_scope() {
        for kw in [
            "exec",
            "expect_respond",
            "secret",
            "screen_check",
            "前置条件",
            "[REDACTED]",
            "onUnexpected",
            "verify",
            "notes",
        ] {
            assert!(SYSTEM_PROMPT.contains(kw), "缺少 {kw}");
        }
        assert!(SYSTEM_PROMPT.contains("不要输出 hostScope"));
    }

    // ——— 决策 ———

    #[test]
    fn decision_logic() {
        let errs = vec!["x".to_string()];
        match decide_after_failure(1, &errs) {
            Decision::Retry(fb) => assert!(fb.contains("- x")),
            other => panic!("{other:?}"),
        }
        assert_eq!(decide_after_failure(MAX_ATTEMPTS, &errs), Decision::Fallback);
    }

    // ——— 端到端（假 LLM） ———

    #[test]
    fn refine_accepts_first_good_reply() {
        let mut llm = Fake::new(vec![Ok(good_reply())]);
        let r = run(refine(&mut llm, &base_draft(), Some("部署"), true)).unwrap();
        assert!(!r.fallback);
        assert_eq!(r.attempts, 1);
        assert_eq!(r.notes, vec!["提取了 dir 参数"]);
        assert_eq!(r.draft.source, Some(SkillSource::Recorded));
        assert_eq!(llm.systems[0], SYSTEM_PROMPT);
        assert_eq!(llm.seen[0].len(), 1);
    }

    #[test]
    fn refine_retries_once_with_validation_feedback() {
        let bad = r#"{"name":"x","steps":[{"kind":"exec","command":"echo {{nope}}"}]}"#.to_string();
        let mut llm = Fake::new(vec![Ok(bad.clone()), Ok(good_reply())]);
        let r = run(refine(&mut llm, &base_draft(), None, true)).unwrap();
        assert!(!r.fallback);
        assert_eq!(r.attempts, 2);
        // 第二次请求带上了上一次的回复和问题清单
        let second = &llm.seen[1];
        assert_eq!(second.len(), 3);
        assert_eq!(second[1].role, "assistant");
        assert_eq!(second[1].content, bad);
        assert!(second[2].content.contains("没有通过程序校验") && second[2].content.contains("nope"));
    }

    #[test]
    fn refine_retries_after_unparseable_reply() {
        let mut llm = Fake::new(vec![Ok("抱歉，我不确定".into()), Ok(good_reply())]);
        let r = run(refine(&mut llm, &base_draft(), None, false)).unwrap();
        assert_eq!(r.attempts, 2);
        assert!(!r.fallback);
    }

    #[test]
    fn refine_falls_back_to_mechanical_after_two_failures() {
        let bad = r#"{"name":"x","steps":[{"kind":"exec","command":"echo {{nope}}"}]}"#.to_string();
        let mut llm = Fake::new(vec![Ok(bad.clone()), Ok(bad)]);
        let d = base_draft();
        let r = run(refine(&mut llm, &d, None, true)).unwrap();
        assert!(r.fallback);
        assert_eq!(r.attempts, 2);
        assert_eq!(r.draft.steps, d.steps);
        assert!(r.draft.params.is_empty() && r.draft.verify.is_none());
        assert_eq!(r.draft.source, Some(SkillSource::Recorded));
        assert_eq!(r.draft.host_scope, HostScope::List { hosts: vec!["prod-web".into()] });
        assert!(r.warnings[0].contains("退回"));
        // 回退结果必须是合格的
        assert!(skill::validate(&r.draft.clone().into_skill(String::new(), 0)).is_ok());
    }

    #[test]
    fn fallback_keeps_recorded_risk() {
        let d = draft_with("h", vec![exec("sudo rm -rf /tmp/x"), secret_step()]);
        let junk = "not json".to_string();
        let mut llm = Fake::new(vec![Ok(junk.clone()), Ok(junk)]);
        let r = run(refine(&mut llm, &d, None, true)).unwrap();
        assert!(r.fallback && r.draft.risk == Risk::High);
    }

    #[test]
    fn llm_transport_error_is_returned_not_swallowed() {
        let mut llm = Fake::new(vec![Err("API Key 无效（401）".into())]);
        let e = run(refine(&mut llm, &base_draft(), None, true)).unwrap_err();
        assert!(e.contains("401"));
        assert_eq!(llm.seen.len(), 1);
    }

    #[test]
    fn llm_error_on_retry_is_also_returned() {
        let bad = r#"{"name":"x","steps":[]}"#.to_string();
        let mut llm = Fake::new(vec![Ok(bad), Err("超时".into())]);
        assert_eq!(run(refine(&mut llm, &base_draft(), None, true)).unwrap_err(), "超时");
    }

    #[test]
    fn empty_or_oversized_draft_is_rejected_without_calling_llm() {
        let mut llm = Fake::new(vec![Ok(good_reply())]);
        assert!(run(refine(&mut llm, &draft_with("h", vec![]), None, true)).is_err());
        let many = draft_with("h", (0..=MAX_STEPS).map(|_| exec("ls")).collect());
        assert!(run(refine(&mut llm, &many, None, true)).is_err());
        assert!(llm.seen.is_empty());
    }

    #[test]
    fn mechanical_input_fails_closed_on_invalid_draft() {
        let d = draft_with("h", vec![exec("mysql --password=hunter22")]);
        assert!(mechanical_input(&d).is_err());
        // 回退也过不了 → refine 返回 Err，而不是无效草稿
        let junk = "x".to_string();
        let mut llm = Fake::new(vec![Ok(junk.clone()), Ok(junk)]);
        assert!(run(refine(&mut llm, &d, None, true)).is_err());
    }

    #[test]
    fn model_output_with_redacted_placeholder_triggers_retry() {
        let d = draft_with("h", vec![exec("ping 10.0.0.1")]);
        let first = r#"{"name":"x","steps":[{"kind":"exec","command":"ping [REDACTED]"}]}"#.to_string();
        let second = r#"{"name":"x","params":[{"name":"host","description":"目标"}],"steps":[{"kind":"exec","command":"ping {{host}}"}]}"#.to_string();
        let mut llm = Fake::new(vec![Ok(first), Ok(second)]);
        let r = run(refine(&mut llm, &d, None, true)).unwrap();
        assert_eq!(r.attempts, 2);
        assert!(!r.fallback);
        assert!(r.draft.params[0].default_value.is_none());
        // 第一轮发给模型的消息里没有真实 IP
        assert!(!llm.seen[0][0].content.contains("10.0.0.1"));
    }
}
