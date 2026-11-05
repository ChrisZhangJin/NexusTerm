//! Skill（可回放的操作流程）的数据模型、校验、渲染与导入导出（Phase 3.1）。
//!
//! 纯逻辑：不碰数据库、不碰 `AppHandle`、不碰终端。落盘在 `store::skills`，
//! Tauri 命令在 `agent::skill_cmd`。后续阶段都建在这个模型上：
//! 3.2 录制器产出 `SkillInput`，3.3 LLM 整理草稿，3.4 管理界面编辑 / 导入导出，
//! 3.5 旧宏迁移，4.x 回放引擎消费 [`render_step`] 的结果。
//!
//! # 模型（JSON 形状，字段一律 camelCase，枚举值 snake_case）
//!
//! ```json
//! {
//!   "id": "uuid", "name": "部署 web", "description": "拉代码并重启",
//!   "params": [ { "name": "branch", "description": "", "default": "main", "required": false, "secret": false } ],
//!   "steps": [
//!     { "kind": "exec", "command": "git checkout {{branch}}", "timeoutMs": 20000,
//!       "expectAfter": "Switched to", "onUnexpected": "ask" },
//!     { "kind": "keys", "keys": "<C-c>" },
//!     { "kind": "expect_respond", "expect": "[Y/n]", "respond": "y<Enter>", "secret": false },
//!     { "kind": "secret", "reason": "sudo 密码", "param": "sudo_pw" },
//!     { "kind": "screen_check", "expect": "/active \\(running\\)/" }
//!   ],
//!   "verify": { "expect": "ok", "timeoutMs": 5000 },
//!   "risk": "low|medium|high",
//!   "hostScope": { "kind": "any" }  // 或 { "kind": "list", "hosts": ["会话id", "*.prod.example.com"] }
//!   "source": "recorded|migrated_macro|manual|imported",
//!   "successCount": 0, "failCount": 0, "lastRunTs": null,
//!   "createdAt": 0, "updatedAt": 0, "schemaVersion": 1
//! }
//! ```
//!
//! 时间一律 **Unix 秒**（与 `sessions` / `macros` 表一致）。每个步骤都有三个公共字段：
//! `timeoutMs?`（本步最多等多久）、`expectAfter?`（做完这步后屏幕上应出现的 pattern）、
//! `onUnexpected`（`ask | stop | llm`，缺省 `ask`：偏离期望时问用户 / 停下 / 交给 LLM）。
//!
//! # pattern 语法
//!
//! 与 `nx_wait` 相同（[`interact::Pattern`]）：子串，或以 `/` 开头、含第二个 `/` 的 `/regex/i`。
//! `keys` 的语法同 `nx_send_keys`（[`interact::parse_keys`]）。
//!
//! # 占位符
//!
//! `{{name}}`（name 为 `[A-Za-z_][A-Za-z0-9_]*`，≤64 字符，两侧可有空白）引用已声明的参数，
//! 可出现在 `exec.command`、`keys.keys`、`expect_respond.respond` 与各种 pattern 里。
//! 其它长得像的写法（`docker inspect -f '{{.Name}}'`、`{{json .}}`）**不是**占位符，原样保留；
//! 真要写字面的 `{{name}}` 用 `\{{name}}`。引用了未声明的参数是校验错误。
//!
//! # 安全约束（本模块的存在理由之一）
//!
//! - **skill 里永远没有密码值。** `secret` 步骤只有 `reason` 与可选的 `param`（引用名），
//!   结构上没有 value 字段，且所有步骤 / 参数结构都 `deny_unknown_fields`：
//!   导入一份带 `value` / `password` 的 JSON 会在解析阶段就被拒。
//! - `secret: true` 的参数不能有 `default`，**只能**出现在 `secret.param` 与
//!   `secret: true` 的 `expect_respond.respond`（且必须整个 respond 就是这一个引用）里；
//!   出现在命令 / 按键 / pattern 里一律校验失败。运行时它们由密码通道（`nx_secret`）解析，
//!   [`render_step`] 不会把它们替换进任何文本，只留引用。
//! - 校验还会拒绝命令 / 按键 / 应答里疑似写死的口令（`password=…`、`sk-…`、`Bearer …`、`sshpass -p` 等），
//!   以及「expect 看着像口令提示、却不是 secret 的 respond」。这是兜底，不是保证：
//!   录制器必须把口令步骤录成 `secret`（见 3.2）。
//! - 导入的 skill 是**不受信输入**：限大小、严格解析、重新校验、重新评估风险，
//!   `success_count` 清零（4.4 的信任分级据此把它当成从没跑过的新 skill），来源强制为 `imported`。
//! - 风险等级 [`Risk`] 只会被抬高：[`Skill::normalize`] 把它抬到不低于 [`assess_risk`] 的结果，
//!   且 [`validate`] 拒绝低于评估值的 skill。
//! - 参数值（运行时给的）经 [`resolve_params`] 检查：不得含控制字符（含换行）、≤1000 字符；
//!   渲染到按键串里会转义（`\` `<`），渲染到 regex 里会 `regex::escape`，
//!   防止参数值夹带 `\x03` / `<C-c>` 或改写 pattern 的种类。
//!   **渲染后的命令仍必须在执行前重新过 `danger::assess`**（模板上的评估看不到参数值）。

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::danger;
use super::interact::{contains_enter, parse_keys, Pattern};

// ——————————————————————————————— 上限 ———————————————————————————————

/// 当前 schema 版本。只增不改字段语义；新增字段必须 `#[serde(default)]`，旧数据照常读。
pub const SCHEMA_VERSION: u32 = 1;
/// 导出文件头里的 `format`。
pub const EXPORT_FORMAT: &str = "nexus-skill";
pub const MAX_SKILLS: usize = 200;
pub const MAX_STEPS: usize = 100;
pub const MAX_PARAMS: usize = 32;
pub const MAX_NAME_CHARS: usize = 80;
pub const MAX_DESC_CHARS: usize = 500;
pub const MAX_PARAM_DESC_CHARS: usize = 200;
pub const MAX_COMMAND_CHARS: usize = 2000;
pub const MAX_KEYS_CHARS: usize = 512;
pub const MAX_PATTERN_CHARS: usize = 500;
pub const MAX_REASON_CHARS: usize = 200;
/// 运行时参数值（以及参数默认值）的长度上限。
pub const MAX_PARAM_VALUE_CHARS: usize = 1000;
pub const MAX_HOST_ENTRIES: usize = 100;
pub const MAX_HOST_ENTRY_CHARS: usize = 255;
/// 序列化后一份 skill 的 JSON 上限。
pub const MAX_SKILL_JSON_BYTES: usize = 256 * 1024;
/// `import_text` 接受的文本上限。
pub const MAX_IMPORT_BYTES: usize = 256 * 1024;
pub const MIN_TIMEOUT_MS: u64 = 100;
pub const MAX_TIMEOUT_MS: u64 = 600_000;

// ——————————————————————————————— 模型 ———————————————————————————————

/// 风险等级。可比较：`Low < Medium < High`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    #[default]
    Low,
    Medium,
    High,
}

/// 步骤偏离期望时怎么办。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnUnexpected {
    /// 暂停并问用户（默认：最保守的、仍可继续的选择）
    #[default]
    Ask,
    /// 直接停止这次回放
    Stop,
    /// 交给 LLM 判断（继续 / 改做法 / 停下问用户，见 4.3）
    Llm,
}

/// skill 的来历。`imported` 与 `migrated_macro` 都不带信任。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    Recorded,
    MigratedMacro,
    #[default]
    Manual,
    Imported,
}

/// 适用主机范围。条目可以是已保存会话的 id，或主机名 / `*` 通配的 glob（如 `*.prod.example.com`），
/// 不区分大小写。`Any` = 不限。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostScope {
    #[default]
    Any,
    List { hosts: Vec<String> },
}

impl HostScope {
    /// 这个会话（id + 主机名）是否在范围内。4.4 的回放前检查用它。
    pub fn allows(&self, session_id: &str, host: &str) -> bool {
        match self {
            HostScope::Any => true,
            HostScope::List { hosts } => hosts.iter().any(|h| {
                h.eq_ignore_ascii_case(session_id) || glob_match(h, host)
            }),
        }
    }
}

/// 只支持 `*`（匹配任意长度，含空），不区分大小写。
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// 参数声明。`secret` 参数的值**从不**存进 skill、也不随调用传入，运行时走密码通道。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillParam {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// 缺省值。`secret` 参数不得有。
    #[serde(default, rename = "default", skip_serializing_if = "Option::is_none")]
    pub default_value: Option<String>,
    /// 为真时：运行时必须给出值（没有 default 又没传就报错）。
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub secret: bool,
}

/// 一条期望：回放引擎据此检查屏幕。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillCheck {
    /// 子串或 `/regex/`。
    pub expect: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// 一个步骤。`kind` 为标签；每个变体都带三个公共字段
/// （`timeoutMs` / `expectAfter` / `onUnexpected`），用访问器 [`SkillStep::timeout_ms`] 等读取。
///
/// 刻意**没有**任何「值」字段：`secret` 步骤只有 `reason` 与 `param` 引用名。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum SkillStep {
    /// 敲一行命令并等它结束（`nx_exec` 语义）。`command` 可含 `{{param}}`，单行。
    Exec {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expect_after: Option<String>,
        #[serde(default)]
        on_unexpected: OnUnexpected,
    },
    /// 原始按键（`nx_send_keys` 语义），不等命令结束。
    Keys {
        keys: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expect_after: Option<String>,
        #[serde(default)]
        on_unexpected: OnUnexpected,
    },
    /// 等屏幕出现 `expect`，然后应答。`secret` 为真时应答由密码通道提供：
    /// `respond` 只能是空串（运行时现问）或唯一一个 `{{secret 参数}}` 引用。
    ExpectRespond {
        expect: String,
        respond: String,
        #[serde(default)]
        secret: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expect_after: Option<String>,
        #[serde(default)]
        on_unexpected: OnUnexpected,
    },
    /// 密码 / 口令占位：运行时由用户在审批卡里输入（`nx_secret`）。只有理由与引用名，永远没有值。
    Secret {
        reason: String,
        /// 引用的 secret 参数名（仅用于给提示卡起名 / 让保险库条目对上号）
        #[serde(default, skip_serializing_if = "Option::is_none")]
        param: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expect_after: Option<String>,
        #[serde(default)]
        on_unexpected: OnUnexpected,
    },
    /// 只检查屏幕，不做任何操作（`nx_screen` + 匹配）。
    ScreenCheck {
        expect: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expect_after: Option<String>,
        #[serde(default)]
        on_unexpected: OnUnexpected,
    },
}

impl SkillStep {
    /// 步骤种类名（与 JSON 的 `kind` 一致），用于错误信息。
    pub fn kind(&self) -> &'static str {
        match self {
            SkillStep::Exec { .. } => "exec",
            SkillStep::Keys { .. } => "keys",
            SkillStep::ExpectRespond { .. } => "expect_respond",
            SkillStep::Secret { .. } => "secret",
            SkillStep::ScreenCheck { .. } => "screen_check",
        }
    }

    pub fn timeout_ms(&self) -> Option<u64> {
        match self {
            SkillStep::Exec { timeout_ms, .. }
            | SkillStep::Keys { timeout_ms, .. }
            | SkillStep::ExpectRespond { timeout_ms, .. }
            | SkillStep::Secret { timeout_ms, .. }
            | SkillStep::ScreenCheck { timeout_ms, .. } => *timeout_ms,
        }
    }

    pub fn expect_after(&self) -> Option<&str> {
        match self {
            SkillStep::Exec { expect_after, .. }
            | SkillStep::Keys { expect_after, .. }
            | SkillStep::ExpectRespond { expect_after, .. }
            | SkillStep::Secret { expect_after, .. }
            | SkillStep::ScreenCheck { expect_after, .. } => expect_after.as_deref(),
        }
    }

    pub fn on_unexpected(&self) -> OnUnexpected {
        match self {
            SkillStep::Exec { on_unexpected, .. }
            | SkillStep::Keys { on_unexpected, .. }
            | SkillStep::ExpectRespond { on_unexpected, .. }
            | SkillStep::Secret { on_unexpected, .. }
            | SkillStep::ScreenCheck { on_unexpected, .. } => *on_unexpected,
        }
    }
}

/// 完整的 skill（库里的形态）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub params: Vec<SkillParam>,
    pub steps: Vec<SkillStep>,
    /// 全部步骤做完后的整体验证（屏幕应出现什么）。
    #[serde(default)]
    pub verify: Option<SkillCheck>,
    #[serde(default)]
    pub risk: Risk,
    #[serde(default)]
    pub host_scope: HostScope,
    #[serde(default)]
    pub source: SkillSource,
    /// 成功 / 失败的回放次数。只有 `store::skills::skill_record_run_result` 能增加；
    /// 步骤 / 参数 / 范围 / 验证被编辑后清零（旧的信任不适用于新内容）。
    #[serde(default)]
    pub success_count: u32,
    #[serde(default)]
    pub fail_count: u32,
    #[serde(default)]
    pub last_run_ts: Option<i64>,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
}

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

/// 创建 / 更新 skill 时前端（或录制器 / 迁移器）提交的内容：没有 id、计数与时间戳，
/// 这些全由后端铸造，调用方无法伪造「已成功跑过 N 次」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillInput {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub params: Vec<SkillParam>,
    pub steps: Vec<SkillStep>,
    #[serde(default)]
    pub verify: Option<SkillCheck>,
    #[serde(default)]
    pub risk: Risk,
    #[serde(default)]
    pub host_scope: HostScope,
    /// 创建时有效（缺省 `manual`）；更新时忽略，保留原来历。
    #[serde(default)]
    pub source: Option<SkillSource>,
}

impl SkillInput {
    /// 铸成一份全新的 skill：计数清零，已规整（trim、风险抬高）。
    pub fn into_skill(self, id: String, now: i64) -> Skill {
        let mut s = Skill {
            id,
            name: self.name,
            description: self.description,
            params: self.params,
            steps: self.steps,
            verify: self.verify,
            risk: self.risk,
            host_scope: self.host_scope,
            source: self.source.unwrap_or_default(),
            success_count: 0,
            fail_count: 0,
            last_run_ts: None,
            created_at: now,
            updated_at: now,
            schema_version: SCHEMA_VERSION,
        };
        s.normalize();
        s
    }
}

impl From<&Skill> for SkillInput {
    fn from(s: &Skill) -> Self {
        SkillInput {
            name: s.name.clone(),
            description: s.description.clone(),
            params: s.params.clone(),
            steps: s.steps.clone(),
            verify: s.verify.clone(),
            risk: s.risk,
            host_scope: s.host_scope.clone(),
            source: Some(s.source),
        }
    }
}

impl Skill {
    /// 规整：去掉名称 / 描述首尾空白，把风险抬到不低于自动评估值。幂等。
    /// 存库与导入前必须调用（`store::skills` 与 [`import_text`] 都会调）。
    pub fn normalize(&mut self) {
        self.name = self.name.trim().to_string();
        self.description = self.description.trim().to_string();
        let assessed = assess_risk(&self.steps).risk;
        if self.risk < assessed {
            self.risk = assessed;
        }
    }

    /// 回放相关内容（步骤 / 参数 / 验证 / 范围）是否与另一份相同。
    /// 不同则历史成功次数不再可信，更新时据此清零。
    pub fn same_behavior(&self, other: &SkillInput) -> bool {
        self.steps == other.steps
            && self.params == other.params
            && self.verify == other.verify
            && self.host_scope == other.host_scope
    }
}

// ——————————————————————————————— 占位符 ———————————————————————————————

enum Seg<'a> {
    Lit(String),
    Param { name: &'a str, raw: &'a str },
}

fn is_ident(s: &str) -> bool {
    let mut it = s.chars();
    match it.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    s.len() <= 64 && it.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// 把模板切成「字面 / 占位符」。`\{{` 是字面 `{{`；`{{` 后不是合法参数名的原样当字面。
fn scan(text: &str) -> Vec<Seg<'_>> {
    let mut out = Vec::new();
    let mut lit = String::new();
    let mut i = 0usize;
    while i < text.len() {
        let rest = &text[i..];
        if rest.starts_with("\\{{") {
            lit.push_str("{{");
            i += 3;
            continue;
        }
        if rest.starts_with("{{") {
            if let Some(end) = rest[2..].find("}}") {
                let name = rest[2..2 + end].trim();
                if is_ident(name) {
                    if !lit.is_empty() {
                        out.push(Seg::Lit(std::mem::take(&mut lit)));
                    }
                    out.push(Seg::Param { name, raw: &rest[..end + 4] });
                    i += end + 4;
                    continue;
                }
            }
        }
        let ch = rest.chars().next().unwrap_or('\0');
        lit.push(ch);
        i += ch.len_utf8().max(1);
    }
    if !lit.is_empty() {
        out.push(Seg::Lit(lit));
    }
    out
}

/// 模板里按出现顺序引用的参数名（含重复）。
pub fn placeholder_names(text: &str) -> Vec<String> {
    scan(text)
        .into_iter()
        .filter_map(|s| match s {
            Seg::Param { name, .. } => Some(name.to_string()),
            Seg::Lit(_) => None,
        })
        .collect()
}

/// 代入占位符。`f` 返回 `None` 的占位符原样保留（`{{name}}`）。
fn render_template(text: &str, mut f: impl FnMut(&str) -> Option<String>) -> String {
    let mut out = String::new();
    for seg in scan(text) {
        match seg {
            Seg::Lit(s) => out.push_str(&s),
            Seg::Param { name, raw } => match f(name) {
                Some(v) => out.push_str(&v),
                None => out.push_str(raw),
            },
        }
    }
    out
}

/// 校验用：占位符全换成 `x`，得到一个「形状上等价」的文本去做语法检查。
fn dummy(text: &str) -> String {
    render_template(text, |_| Some("x".into()))
}

// ——————————————————————————————— 校验 ———————————————————————————————

fn has_control(s: &str, allow_tab: bool) -> bool {
    s.chars().any(|c| c.is_control() && !(allow_tab && c == '\t'))
}

/// 疑似写死的口令。返回命中的说明。宁可少报：只抓形状很确定的。
fn literal_secret_hint(text: &str) -> Option<&'static str> {
    static RES: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    let res = RES.get_or_init(|| {
        [
            (r#"(?i)bearer\s+[A-Za-z0-9._~+/=\-]{8,}"#, "Bearer token"),
            (r#"sk-[A-Za-z0-9_\-]{8,}"#, "sk- 开头的 API key"),
            (r#"AKIA[0-9A-Z]{16}"#, "AWS access key"),
            (r#"-----BEGIN [A-Z ]*PRIVATE KEY-----"#, "私钥"),
            (r#"(?i)\bsshpass\s+-p"#, "sshpass -p 口令"),
            (r#"(?i)\bmysql(?:dump|admin)?\b[^\n]*\s-p\S"#, "mysql -p口令"),
        ]
        .iter()
        .map(|(p, why)| (Regex::new(p).unwrap_or_else(|e| panic!("skill 口令正则编译失败 {p}：{e}")), *why))
        .collect()
    });
    for (re, why) in res {
        if re.is_match(text) {
            return Some(why);
        }
    }
    // password=xxx / --password xxx：值是 `{{…}}` 或 `$VAR` 引用则放过
    static KV: OnceLock<Regex> = OnceLock::new();
    let kv = KV.get_or_init(|| {
        Regex::new(
            r#"(?i)(?:\b(?:token|api[_-]?key|access[_-]?key|secret[_-]?key|secret|password|passwd|pwd)=|--(?:password|passwd|token|secret)[= ]\s*)("[^"\n]*"|'[^'\n]*'|[^\s&"'<>`]+)"#,
        )
        .unwrap_or_else(|e| panic!("skill 口令 kv 正则编译失败：{e}"))
    });
    for cap in kv.captures_iter(text) {
        let v = cap[1].trim_matches(|c| c == '"' || c == '\'');
        if !(v.is_empty() || v.starts_with("{{") || v.starts_with('$') || v.starts_with('-')) {
            return Some("password= / --password 后跟字面值");
        }
    }
    None
}

fn looks_like_password_prompt(expect: &str) -> bool {
    let l = expect.to_lowercase();
    l.contains("password") || l.contains("passphrase") || l.contains("口令") || l.contains("密码")
}

/// pattern 是否会被当成 `/regex/`（按占位符换成 `x` 后的形状判断）。语法错返回原因。
fn pattern_is_regex(template: &str) -> Result<bool, String> {
    Pattern::parse(&dummy(template)).map(|p| matches!(p, Pattern::Regex(_)))
}

struct Ctx<'a> {
    errs: Vec<String>,
    declared: BTreeMap<&'a str, &'a SkillParam>,
}

impl<'a> Ctx<'a> {
    /// 检查模板里的占位符：必须已声明；secret 参数一律不许（`allow_secret` 的场合另行检查）。
    fn placeholders(&mut self, label: &str, text: &str) {
        for name in placeholder_names(text) {
            match self.declared.get(name.as_str()) {
                None => self.errs.push(format!("{label}：占位符 {{{{{name}}}}} 没有对应的参数声明")),
                Some(p) if p.secret => self.errs.push(format!(
                    "{label}：secret 参数 {{{{{name}}}}} 不能出现在这里（只能用在 secret 步骤 / secret 应答里，由密码通道在运行时提供）"
                )),
                Some(_) => {}
            }
        }
    }

    fn pattern(&mut self, label: &str, template: &str) {
        if template.trim().is_empty() {
            self.errs.push(format!("{label}：pattern 为空"));
            return;
        }
        if template.chars().count() > MAX_PATTERN_CHARS {
            self.errs.push(format!("{label}：pattern 超过 {MAX_PATTERN_CHARS} 字符"));
            return;
        }
        self.placeholders(label, template);
        if let Err(e) = pattern_is_regex(template) {
            self.errs.push(format!("{label}：{e}"));
        }
    }

    fn timeout(&mut self, label: &str, t: Option<u64>) {
        if let Some(t) = t {
            if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&t) {
                self.errs.push(format!(
                    "{label}：timeoutMs 必须在 {MIN_TIMEOUT_MS}..={MAX_TIMEOUT_MS} 之间"
                ));
            }
        }
    }

    fn literal_secret(&mut self, label: &str, text: &str) {
        if let Some(why) = literal_secret_hint(text) {
            self.errs.push(format!(
                "{label}：疑似写死了口令 / 密钥（{why}）。skill 里不能存密码值，请改用 secret 步骤或 secret 参数"
            ));
        }
    }

    /// 校验按键串（占位符换成 `x` 后能被 `parse_keys` 解码）。
    fn keys(&mut self, label: &str, text: &str) {
        if text.is_empty() {
            self.errs.push(format!("{label}：按键串为空"));
            return;
        }
        if text.chars().count() > MAX_KEYS_CHARS {
            self.errs.push(format!("{label}：按键串超过 {MAX_KEYS_CHARS} 字符"));
            return;
        }
        self.placeholders(label, text);
        if let Err(e) = parse_keys(&dummy(text)) {
            self.errs.push(format!("{label}：按键串无效：{e}"));
        }
        self.literal_secret(label, text);
    }
}

/// 校验一份 skill。通过返回 `Ok(())`，否则返回**全部**问题（中文，可直接展示）。
pub fn validate(skill: &Skill) -> Result<(), Vec<String>> {
    let mut cx = Ctx { errs: Vec::new(), declared: BTreeMap::new() };

    // —— 基本信息 ——
    let name = skill.name.trim();
    if name.is_empty() {
        cx.errs.push("名称不能为空".into());
    } else if name.chars().count() > MAX_NAME_CHARS {
        cx.errs.push(format!("名称超过 {MAX_NAME_CHARS} 字符"));
    } else if has_control(name, false) {
        cx.errs.push("名称不能含控制字符".into());
    }
    if skill.description.chars().count() > MAX_DESC_CHARS {
        cx.errs.push(format!("描述超过 {MAX_DESC_CHARS} 字符"));
    }
    if skill.schema_version == 0 || skill.schema_version > SCHEMA_VERSION {
        cx.errs.push(format!(
            "schemaVersion {} 不受支持（本版本最高 {SCHEMA_VERSION}）",
            skill.schema_version
        ));
    }

    // —— 参数 ——
    if skill.params.len() > MAX_PARAMS {
        cx.errs.push(format!("参数最多 {MAX_PARAMS} 个"));
    }
    let mut seen = HashSet::new();
    for p in &skill.params {
        if !is_ident(&p.name) {
            cx.errs.push(format!("参数名「{}」无效（只能是字母 / 数字 / 下划线，不以数字开头，≤64 字符）", p.name));
            continue;
        }
        if !seen.insert(p.name.as_str()) {
            cx.errs.push(format!("参数名「{}」重复", p.name));
            continue;
        }
        if p.description.chars().count() > MAX_PARAM_DESC_CHARS {
            cx.errs.push(format!("参数「{}」的描述超过 {MAX_PARAM_DESC_CHARS} 字符", p.name));
        }
        if let Some(d) = &p.default_value {
            if p.secret {
                cx.errs.push(format!("secret 参数「{}」不能有默认值", p.name));
            }
            if d.chars().count() > MAX_PARAM_VALUE_CHARS || has_control(d, false) {
                cx.errs.push(format!(
                    "参数「{}」的默认值无效（≤{MAX_PARAM_VALUE_CHARS} 字符，不含控制字符 / 换行）",
                    p.name
                ));
            }
        }
        cx.declared.insert(p.name.as_str(), p);
    }

    // —— 步骤 ——
    if skill.steps.is_empty() {
        cx.errs.push("至少需要一个步骤".into());
    }
    if skill.steps.len() > MAX_STEPS {
        cx.errs.push(format!("步骤最多 {MAX_STEPS} 个"));
    }
    for (i, step) in skill.steps.iter().enumerate().take(MAX_STEPS) {
        let label = format!("第 {} 步（{}）", i + 1, step.kind());
        validate_step(&mut cx, &label, step);
        cx.timeout(&label, step.timeout_ms());
        if let Some(p) = step.expect_after() {
            cx.pattern(&format!("{label} 的 expectAfter"), p);
        }
    }

    // —— 验证 / 范围 ——
    if let Some(v) = &skill.verify {
        cx.pattern("verify", &v.expect);
        cx.timeout("verify", v.timeout_ms);
    }
    if let HostScope::List { hosts } = &skill.host_scope {
        if hosts.is_empty() {
            cx.errs.push("hostScope 为 list 时至少要有一项（要不限主机请用 any）".into());
        }
        if hosts.len() > MAX_HOST_ENTRIES {
            cx.errs.push(format!("hostScope 最多 {MAX_HOST_ENTRIES} 项"));
        }
        for h in hosts.iter().take(MAX_HOST_ENTRIES) {
            if h.trim().is_empty()
                || h.chars().count() > MAX_HOST_ENTRY_CHARS
                || h.chars().any(|c| c.is_whitespace() || c.is_control())
            {
                cx.errs.push(format!("hostScope 的条目「{h}」无效（非空、无空白、≤{MAX_HOST_ENTRY_CHARS} 字符）"));
            }
        }
    }

    // —— 风险：不得低于自动评估 ——
    let report = assess_risk(&skill.steps);
    if skill.risk < report.risk {
        cx.errs.push(format!(
            "风险等级 {:?} 低于自动评估的 {:?}：{}",
            skill.risk,
            report.risk,
            report.reasons.join("；")
        ));
    }

    // —— 总大小 ——
    match serde_json::to_string(skill) {
        Ok(j) if j.len() > MAX_SKILL_JSON_BYTES => {
            cx.errs.push(format!("skill 序列化后超过 {} KB", MAX_SKILL_JSON_BYTES / 1024));
        }
        Err(e) => cx.errs.push(format!("无法序列化：{e}")),
        _ => {}
    }

    if cx.errs.is_empty() { Ok(()) } else { Err(cx.errs) }
}

fn validate_step(cx: &mut Ctx<'_>, label: &str, step: &SkillStep) {
    match step {
        SkillStep::Exec { command, .. } => {
            if command.trim().is_empty() {
                cx.errs.push(format!("{label}：命令为空"));
                return;
            }
            if command.chars().count() > MAX_COMMAND_CHARS {
                cx.errs.push(format!("{label}：命令超过 {MAX_COMMAND_CHARS} 字符"));
                return;
            }
            if has_control(command, true) {
                cx.errs.push(format!("{label}：命令必须是单行、不含控制字符（要发按键请用 keys 步骤）"));
            }
            cx.placeholders(label, command);
            cx.literal_secret(label, command);
        }
        SkillStep::Keys { keys, .. } => cx.keys(label, keys),
        SkillStep::ExpectRespond { expect, respond, secret, .. } => {
            cx.pattern(&format!("{label} 的 expect"), expect);
            if *secret {
                check_secret_respond(cx, label, respond);
            } else {
                if respond.is_empty() {
                    cx.errs.push(format!("{label}：respond 为空"));
                } else {
                    cx.keys(&format!("{label} 的 respond"), respond);
                }
                if looks_like_password_prompt(expect) {
                    cx.errs.push(format!(
                        "{label}：expect 像是口令提示，应答必须设 secret=true（skill 里不能存密码值）"
                    ));
                }
            }
        }
        SkillStep::Secret { reason, param, .. } => {
            if reason.trim().is_empty() {
                cx.errs.push(format!("{label}：reason 不能为空（给用户看的输入理由）"));
            } else if reason.chars().count() > MAX_REASON_CHARS {
                cx.errs.push(format!("{label}：reason 超过 {MAX_REASON_CHARS} 字符"));
            }
            if let Some(p) = param {
                require_secret_param(cx, label, p);
            }
        }
        SkillStep::ScreenCheck { expect, .. } => cx.pattern(&format!("{label} 的 expect"), expect),
    }
}

fn require_secret_param(cx: &mut Ctx<'_>, label: &str, name: &str) {
    match cx.declared.get(name) {
        Some(p) if p.secret => {}
        Some(_) => cx.errs.push(format!("{label}：参数「{name}」不是 secret 参数")),
        None => cx.errs.push(format!("{label}：参数「{name}」没有声明")),
    }
}

/// `secret: true` 的应答只允许「空」或「恰好一个 `{{secret 参数}}`」。
fn check_secret_respond(cx: &mut Ctx<'_>, label: &str, respond: &str) {
    if respond.trim().is_empty() {
        return;
    }
    let segs = scan(respond.trim());
    match segs.as_slice() {
        [Seg::Param { name, .. }] => require_secret_param(cx, label, name),
        _ => cx.errs.push(format!(
            "{label}：secret 应答只能是空（运行时现问）或唯一一个 {{{{secret 参数}}}} 引用，不能写字面值"
        )),
    }
}

// ——————————————————————————————— 风险评估 ———————————————————————————————

/// 风险评估结果：整体等级 + 每个抬高了等级的理由。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskReport {
    pub risk: Risk,
    pub reasons: Vec<String>,
}

/// 「只读」程序表。空切片 = 任意参数；否则第一个非 flag 参数必须在列表里。
const READONLY_PROGRAMS: &[(&str, &[&str])] = &[
    ("ls", &[]),
    ("cat", &[]),
    ("pwd", &[]),
    ("whoami", &[]),
    ("id", &[]),
    ("uname", &[]),
    ("uptime", &[]),
    ("df", &[]),
    ("du", &[]),
    ("free", &[]),
    ("ps", &[]),
    ("echo", &[]),
    ("head", &[]),
    ("tail", &[]),
    ("wc", &[]),
    ("grep", &[]),
    ("stat", &[]),
    ("which", &[]),
    ("ss", &[]),
    ("netstat", &[]),
    ("lsblk", &[]),
    ("ping", &[]),
    ("dig", &[]),
    ("nslookup", &[]),
    ("true", &[]),
    ("sleep", &[]),
    ("systemctl", &["status", "is-active", "is-enabled", "list-units", "show", "cat"]),
    ("git", &["status", "log", "diff", "show"]),
    ("docker", &["ps", "images", "logs", "inspect", "stats", "version", "info"]),
];

fn is_readonly_command(cmd: &str) -> bool {
    // 重定向到 /dev/null 与 2>&1 不写文件，先抹掉
    static NULLS: OnceLock<Regex> = OnceLock::new();
    let nulls = NULLS.get_or_init(|| {
        Regex::new(r"\d?>>?\s*/dev/null|\d?>&\d").unwrap_or_else(|e| panic!("正则编译失败：{e}"))
    });
    let cleaned = nulls.replace_all(cmd, " ");
    if cleaned.contains('>') || cleaned.contains('`') || cleaned.contains("$(") || cleaned.contains("<(") {
        return false;
    }
    cleaned
        .split(|c| matches!(c, ';' | '&' | '|' | '\n'))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .all(|seg| {
            let mut toks = seg.split_whitespace();
            let Some(first) = toks.next() else { return true };
            let prog = first.rsplit('/').next().unwrap_or(first);
            let Some((_, subs)) = READONLY_PROGRAMS.iter().find(|(n, _)| *n == prog) else {
                return false;
            };
            if subs.is_empty() {
                return true;
            }
            toks.find(|t| !t.starts_with('-')).is_some_and(|sub| subs.contains(&sub))
        })
}

/// 评估一组步骤的风险。规则（取最大）：
/// - `danger::assess` 判危险（含 sudo / rm / dd / 重定向覆盖等）的 exec / keys / 应答 → **High**；
///   exec 的首词是占位符（执行什么完全由参数决定）→ **High**。
/// - 含占位符的 exec、非「只读」命令（见 `READONLY_PROGRAMS`）→ **Medium**；
///   带回车的按键 / 非空应答、secret 步骤 → **Medium**。
/// - 纯只读命令（且无占位符）、不带回车的按键、screen_check → **Low**。
///
/// 评估只看模板；执行前仍须对渲染后的命令重新 `danger::assess`。
pub fn assess_risk(steps: &[SkillStep]) -> RiskReport {
    let mut risk = Risk::Low;
    let mut reasons = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        let (r, why) = assess_step(step);
        if r > Risk::Low {
            reasons.push(format!("第 {} 步（{}）：{why}", i + 1, step.kind()));
        }
        risk = risk.max(r);
    }
    RiskReport { risk, reasons }
}

fn assess_text(text: &str) -> Option<String> {
    let a = danger::assess(text);
    a.dangerous.then(|| a.reasons.join("、"))
}

fn assess_step(step: &SkillStep) -> (Risk, String) {
    match step {
        SkillStep::Exec { command, .. } => {
            if let Some(why) = assess_text(command) {
                return (Risk::High, why);
            }
            if matches!(scan(command.trim_start()).first(), Some(Seg::Param { .. })) {
                return (Risk::High, "命令的程序名来自参数，无法评估".into());
            }
            if !placeholder_names(command).is_empty() {
                return (Risk::Medium, "命令含参数占位符".into());
            }
            if is_readonly_command(command) {
                (Risk::Low, String::new())
            } else {
                (Risk::Medium, "命令可能修改系统状态".into())
            }
        }
        SkillStep::Keys { keys, .. } => assess_keys(keys, "按键含回车，会提交一行"),
        SkillStep::ExpectRespond { respond, secret, .. } => {
            if *secret {
                (Risk::Medium, "需要输入密码".into())
            } else {
                assess_keys(respond, "应答含回车，会提交一行")
            }
        }
        SkillStep::Secret { .. } => (Risk::Medium, "需要输入密码".into()),
        SkillStep::ScreenCheck { .. } => (Risk::Low, String::new()),
    }
}

fn assess_keys(keys: &str, enter_why: &str) -> (Risk, String) {
    let Ok(bytes) = parse_keys(&dummy(keys)) else {
        return (Risk::Medium, "按键串无法解析".into());
    };
    // 先解码再评估：`<Enter>` 之类具名键里的 `>` 不能被当成重定向。
    // 控制字节换成空格，剩下的就是会被敲进终端的可见文本。
    let text: String = String::from_utf8_lossy(&bytes)
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if let Some(why) = assess_text(&text) {
        return (Risk::High, why);
    }
    if contains_enter(&bytes) {
        (Risk::Medium, enter_why.into())
    } else {
        (Risk::Low, String::new())
    }
}

// ——————————————————————————————— 参数解析与渲染 ———————————————————————————————

/// 一次运行实际使用的参数：非 secret 参数的值（已合并默认值、已检查），以及 secret 参数名集合。
/// secret 参数**没有值**，只有名字。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResolvedParams {
    pub values: BTreeMap<String, String>,
    pub secret: BTreeSet<String>,
}

/// 把调用方给的参数值与声明合并：补默认值、检查必填、拒绝未声明的名字与非法值。
///
/// secret 参数**不接受**随调用传入的值（防止口令经由 LLM 工具参数 / 日志流动），
/// 它们只能在运行时由密码通道提供。可选且无默认值又没传的参数取空串。
pub fn resolve_params(
    decls: &[SkillParam],
    supplied: &BTreeMap<String, String>,
) -> Result<ResolvedParams, Vec<String>> {
    let mut errs = Vec::new();
    let mut out = ResolvedParams::default();
    for name in supplied.keys() {
        match decls.iter().find(|p| &p.name == name) {
            None => errs.push(format!("未声明的参数「{name}」")),
            Some(p) if p.secret => {
                errs.push(format!("secret 参数「{name}」不能随调用传入，运行时由密码通道提供"))
            }
            Some(_) => {}
        }
    }
    for p in decls {
        if p.secret {
            out.secret.insert(p.name.clone());
            continue;
        }
        let given = supplied.get(&p.name).filter(|v| !v.is_empty());
        let value = match (given, &p.default_value) {
            (Some(v), _) => v.clone(),
            (None, Some(d)) => d.clone(),
            (None, None) if p.required => {
                errs.push(format!("缺少必填参数「{}」", p.name));
                continue;
            }
            (None, None) => String::new(),
        };
        if value.chars().count() > MAX_PARAM_VALUE_CHARS || has_control(&value, false) {
            errs.push(format!(
                "参数「{}」的值无效（≤{MAX_PARAM_VALUE_CHARS} 字符，不含控制字符 / 换行）",
                p.name
            ));
            continue;
        }
        out.values.insert(p.name.clone(), value);
    }
    if errs.is_empty() { Ok(out) } else { Err(errs) }
}

/// 应答的去向。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "via", rename_all = "snake_case")]
pub enum Respond {
    /// 普通按键串（已代入参数、已转义），交给 `parse_keys`。
    Keys { keys: String },
    /// 由密码通道提供；`param` 是引用的 secret 参数名（可无）。
    Secret { param: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum RenderedAction {
    Exec { command: String },
    Keys { keys: String },
    ExpectRespond { expect: String, respond: Respond },
    Secret { reason: String, param: Option<String> },
    ScreenCheck { expect: String },
}

/// 渲染后的步骤：回放引擎直接照着执行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderedStep {
    #[serde(flatten)]
    pub action: RenderedAction,
    pub timeout_ms: Option<u64>,
    pub expect_after: Option<String>,
    pub on_unexpected: OnUnexpected,
}

/// 按键串里的转义：参数值只能是「字面字符」，不能夹带 `\x03` / `<C-c>`。
fn escape_keys(v: &str) -> String {
    v.replace('\\', "\\\\").replace('<', "<lt>")
}

/// 代入文本模板。secret 参数不代入，原样保留 `{{name}}` 引用。
fn subst(text: &str, p: &ResolvedParams, esc: impl Fn(&str) -> String) -> String {
    render_template(text, |name| {
        if p.secret.contains(name) {
            None
        } else {
            p.values.get(name).map(|v| esc(v))
        }
    })
}

/// 渲染 pattern：正则里的参数值 `regex::escape`；子串形态若代入后会被误读成 `/regex/`，
/// 就改写成等价的 `/转义后的字面/`，保证参数值改变不了 pattern 的种类。
fn render_pattern(template: &str, p: &ResolvedParams) -> String {
    match pattern_is_regex(template) {
        Ok(true) => subst(template, p, regex::escape),
        _ => {
            let s = subst(template, p, |v| v.to_string());
            if s.starts_with('/') {
                format!("/{}/", regex::escape(&s))
            } else {
                s
            }
        }
    }
}

/// 把一个步骤按参数渲染成可执行形态。
///
/// - `exec.command`、`keys.keys`、`expect_respond.respond` 与各 pattern 里的 `{{name}}` 被代入。
/// - **secret 参数不代入**：`expect_respond{secret:true}` 渲染成 [`Respond::Secret`]，
///   `secret` 步骤只带 `reason` 与参数名；文本里残留的 secret 引用原样保留。
/// - 渲染不做任何安全判断；执行前仍要 `danger::assess` / scope 检查渲染后的命令。
pub fn render_step(step: &SkillStep, params: &ResolvedParams) -> RenderedStep {
    let action = match step {
        SkillStep::Exec { command, .. } => {
            RenderedAction::Exec { command: subst(command, params, |v| v.to_string()) }
        }
        SkillStep::Keys { keys, .. } => {
            RenderedAction::Keys { keys: subst(keys, params, escape_keys) }
        }
        SkillStep::ExpectRespond { expect, respond, secret, .. } => RenderedAction::ExpectRespond {
            expect: render_pattern(expect, params),
            respond: if *secret {
                Respond::Secret { param: placeholder_names(respond).into_iter().next() }
            } else {
                Respond::Keys { keys: subst(respond, params, escape_keys) }
            },
        },
        SkillStep::Secret { reason, param, .. } => {
            RenderedAction::Secret { reason: reason.clone(), param: param.clone() }
        }
        SkillStep::ScreenCheck { expect, .. } => {
            RenderedAction::ScreenCheck { expect: render_pattern(expect, params) }
        }
    };
    RenderedStep {
        action,
        timeout_ms: step.timeout_ms(),
        expect_after: step.expect_after().map(|p| render_pattern(p, params)),
        on_unexpected: step.on_unexpected(),
    }
}

/// 渲染 `verify` 的 pattern。
pub fn render_check(check: &SkillCheck, params: &ResolvedParams) -> SkillCheck {
    SkillCheck { expect: render_pattern(&check.expect, params), timeout_ms: check.timeout_ms }
}

// ——————————————————————————————— 导入 / 导出 ———————————————————————————————

/// 导出为文本：带版本头的、美化过的 JSON。
///
/// ```json
/// { "format": "nexus-skill", "version": 1, "skill": { ...Skill... } }
/// ```
///
/// 运行统计（成功 / 失败次数、上次运行）不导出：它们是本机的信任记录，不该随文件走。
pub fn export_text(skill: &Skill) -> String {
    let mut s = skill.clone();
    s.success_count = 0;
    s.fail_count = 0;
    s.last_run_ts = None;
    let v = serde_json::json!({
        "format": EXPORT_FORMAT,
        "version": SCHEMA_VERSION,
        "skill": s,
    });
    serde_json::to_string_pretty(&v).unwrap_or_default()
}

/// 导入文本。**不受信输入**：限大小、严格解析（未知字段一律拒绝，含任何 `value` / `password`）、
/// 版本不得高于本版本；成功后得到一份全新的 skill——新 id、来源 `imported`、计数清零、
/// 时间戳取当前、风险重新评估（只抬不降）、重新校验。**不会**入库，也**不会**被信任。
pub fn import_text(text: &str, new_id: String, now: i64) -> Result<Skill, Vec<String>> {
    if text.len() > MAX_IMPORT_BYTES {
        return Err(vec![format!("文件过大（超过 {} KB）", MAX_IMPORT_BYTES / 1024)]);
    }
    let root: Value =
        serde_json::from_str(text).map_err(|e| vec![format!("不是有效的 JSON：{e}")])?;
    if root.get("format").and_then(Value::as_str) != Some(EXPORT_FORMAT) {
        return Err(vec![format!("不是 Nexus skill 文件（缺少 format = \"{EXPORT_FORMAT}\"）")]);
    }
    match root.get("version").and_then(Value::as_u64) {
        Some(v) if v >= 1 && v <= SCHEMA_VERSION as u64 => {}
        Some(v) => {
            return Err(vec![format!(
                "文件版本 {v} 不受支持（本版本最高 {SCHEMA_VERSION}），请升级 Nexus"
            )])
        }
        None => return Err(vec!["缺少 version".into()]),
    }
    let body = root.get("skill").cloned().ok_or_else(|| vec!["缺少 skill 字段".to_string()])?;
    let mut skill: Skill =
        serde_json::from_value(body).map_err(|e| vec![format!("skill 内容无效：{e}")])?;

    skill.id = new_id;
    skill.source = SkillSource::Imported;
    skill.success_count = 0;
    skill.fail_count = 0;
    skill.last_run_ts = None;
    skill.created_at = now;
    skill.updated_at = now;
    skill.schema_version = SCHEMA_VERSION;
    skill.normalize();
    validate(&skill)?;
    Ok(skill)
}

// ——————————————————————————————— 测试 ———————————————————————————————

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn p(name: &str) -> SkillParam {
        SkillParam {
            name: name.into(),
            description: String::new(),
            default_value: None,
            required: false,
            secret: false,
        }
    }
    fn secret_p(name: &str) -> SkillParam {
        SkillParam { secret: true, ..p(name) }
    }
    fn exec(cmd: &str) -> SkillStep {
        SkillStep::Exec {
            command: cmd.into(),
            timeout_ms: None,
            expect_after: None,
            on_unexpected: OnUnexpected::Ask,
        }
    }
    fn keys(k: &str) -> SkillStep {
        SkillStep::Keys {
            keys: k.into(),
            timeout_ms: None,
            expect_after: None,
            on_unexpected: OnUnexpected::Ask,
        }
    }
    fn er(expect: &str, respond: &str, secret: bool) -> SkillStep {
        SkillStep::ExpectRespond {
            expect: expect.into(),
            respond: respond.into(),
            secret,
            timeout_ms: None,
            expect_after: None,
            on_unexpected: OnUnexpected::Ask,
        }
    }
    fn secret_step(reason: &str, param: Option<&str>) -> SkillStep {
        SkillStep::Secret {
            reason: reason.into(),
            param: param.map(Into::into),
            timeout_ms: None,
            expect_after: None,
            on_unexpected: OnUnexpected::Ask,
        }
    }
    fn check(expect: &str) -> SkillStep {
        SkillStep::ScreenCheck {
            expect: expect.into(),
            timeout_ms: None,
            expect_after: None,
            on_unexpected: OnUnexpected::Ask,
        }
    }
    fn skill(params: Vec<SkillParam>, steps: Vec<SkillStep>) -> Skill {
        let input = SkillInput {
            name: "测试".into(),
            description: "d".into(),
            params,
            steps,
            verify: None,
            risk: Risk::Low,
            host_scope: HostScope::Any,
            source: None,
        };
        input.into_skill("id-1".into(), 100)
    }
    fn errs(s: &Skill) -> String {
        validate(s).expect_err("应校验失败").join("\n")
    }
    fn vals(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    // ——— 占位符 ———

    #[test]
    fn placeholder_scan_basics() {
        assert_eq!(placeholder_names("a {{x}} b {{ y_1 }} {{x}}"), vec!["x", "y_1", "x"]);
        assert!(placeholder_names("plain").is_empty());
        // 不是合法参数名的 {{…}} 不是占位符（docker / go 模板）
        assert!(placeholder_names("docker inspect -f '{{.Name}}' c").is_empty());
        assert!(placeholder_names("{{json .}} {{ a b }} {{}} {{1x}}").is_empty());
        // 转义
        assert!(placeholder_names(r"echo \{{x}}").is_empty());
        assert_eq!(render_template(r"echo \{{x}} {{x}}", |_| Some("V".into())), "echo {{x}} V");
        // 未闭合
        assert!(placeholder_names("{{x").is_empty());
        // 多字节不 panic，且三重花括号里的占位符能认出来
        assert_eq!(placeholder_names("中文{{名}}{{{a}}}"), vec!["a"]);
        assert_eq!(render_template("{{{a}}}", |_| Some("V".into())), "{V}");
    }

    #[test]
    fn render_template_keeps_unresolved() {
        assert_eq!(render_template("a {{ x }} b", |_| None), "a {{ x }} b");
    }

    // ——— 校验：通过 ———

    #[test]
    fn valid_full_skill_passes() {
        let mut s = skill(
            vec![
                SkillParam { default_value: Some("main".into()), ..p("branch") },
                secret_p("sudo_pw"),
            ],
            vec![
                exec("git checkout {{branch}}"),
                keys("<C-c>"),
                er("[Y/n]", "y<Enter>", false),
                er("[sudo] password", "{{sudo_pw}}", true),
                secret_step("sudo 密码", Some("sudo_pw")),
                check("/active \\(running\\)/i"),
            ],
        );
        s.verify = Some(SkillCheck { expect: "ok {{branch}}".into(), timeout_ms: Some(5000) });
        s.host_scope = HostScope::List { hosts: vec!["*.prod.example.com".into(), "abc".into()] };
        validate(&s).unwrap_or_else(|e| panic!("{e:?}"));
    }

    // ——— 校验：名称 / 参数 / 步骤 ———

    #[test]
    fn name_rules() {
        let mut s = skill(vec![], vec![exec("ls")]);
        s.name = "   ".into();
        assert!(errs(&s).contains("名称不能为空"));
        s.name = "x".repeat(MAX_NAME_CHARS + 1);
        assert!(errs(&s).contains("名称超过"));
        s.name = "a\nb".into();
        assert!(errs(&s).contains("控制字符"));
        s.name = "字".repeat(MAX_NAME_CHARS);
        assert!(validate(&s).is_ok());
        s.description = "x".repeat(MAX_DESC_CHARS + 1);
        assert!(errs(&s).contains("描述超过"));
    }

    #[test]
    fn param_rules() {
        let s = skill(vec![p("a"), p("a")], vec![exec("ls")]);
        assert!(errs(&s).contains("重复"));
        let s = skill(vec![p("1a")], vec![exec("ls")]);
        assert!(errs(&s).contains("无效"));
        let s = skill(vec![p("a b")], vec![exec("ls")]);
        assert!(errs(&s).contains("无效"));
        let s = skill((0..MAX_PARAMS + 1).map(|i| p(&format!("p{i}"))).collect(), vec![exec("ls")]);
        assert!(errs(&s).contains("参数最多"));
        let s = skill(
            vec![SkillParam { default_value: Some("x\ny".into()), ..p("a") }],
            vec![exec("ls")],
        );
        assert!(errs(&s).contains("默认值无效"));
    }

    #[test]
    fn secret_param_never_has_default() {
        let s = skill(
            vec![SkillParam { default_value: Some("hunter2".into()), ..secret_p("pw") }],
            vec![secret_step("pw", Some("pw"))],
        );
        assert!(errs(&s).contains("不能有默认值"));
    }

    #[test]
    fn steps_nonempty_and_capped() {
        let s = skill(vec![], vec![]);
        assert!(errs(&s).contains("至少需要一个步骤"));
        let s = skill(vec![], (0..MAX_STEPS).map(|_| exec("ls")).collect());
        assert!(validate(&s).is_ok());
        let s = skill(vec![], (0..MAX_STEPS + 1).map(|_| exec("ls")).collect());
        assert!(errs(&s).contains("步骤最多"));
    }

    #[test]
    fn undeclared_placeholder_rejected_everywhere() {
        let s = skill(vec![], vec![exec("echo {{nope}}")]);
        assert!(errs(&s).contains("没有对应的参数声明"));
        let s = skill(vec![], vec![keys("{{nope}}")]);
        assert!(errs(&s).contains("nope"));
        let s = skill(vec![], vec![check("{{nope}}")]);
        assert!(errs(&s).contains("nope"));
        let mut st = exec("ls");
        if let SkillStep::Exec { expect_after, .. } = &mut st {
            *expect_after = Some("{{nope}}".into());
        }
        assert!(errs(&skill(vec![], vec![st])).contains("expectAfter"));
        let mut s = skill(vec![], vec![exec("ls")]);
        s.verify = Some(SkillCheck { expect: "{{nope}}".into(), timeout_ms: None });
        assert!(errs(&s).contains("verify"));
    }

    #[test]
    fn go_template_is_not_a_placeholder() {
        let s = skill(vec![], vec![exec("docker inspect -f '{{.State.Status}}' web")]);
        assert!(validate(&s).is_ok());
    }

    #[test]
    fn secret_param_confined_to_secret_contexts() {
        let ps = || vec![secret_p("pw")];
        for st in [
            exec("login {{pw}}"),
            keys("{{pw}}<Enter>"),
            check("{{pw}}"),
            er("ok?", "{{pw}}", false),
        ] {
            let s = skill(ps(), vec![st]);
            assert!(errs(&s).contains("secret 参数"), "{}", errs(&s));
        }
        // 合法位置
        let s = skill(ps(), vec![secret_step("x", Some("pw")), er("Password:", "{{pw}}", true)]);
        assert!(validate(&s).is_ok());
    }

    #[test]
    fn secret_step_param_must_be_declared_secret() {
        let s = skill(vec![], vec![secret_step("x", Some("pw"))]);
        assert!(errs(&s).contains("没有声明"));
        let s = skill(vec![p("pw")], vec![secret_step("x", Some("pw"))]);
        assert!(errs(&s).contains("不是 secret 参数"));
        let s = skill(vec![], vec![secret_step("  ", None)]);
        assert!(errs(&s).contains("reason"));
        let s = skill(vec![], vec![secret_step("x", None)]);
        assert!(validate(&s).is_ok());
    }

    #[test]
    fn secret_respond_must_be_reference_or_empty() {
        let s = skill(vec![secret_p("pw")], vec![er("Password:", "hunter2", true)]);
        assert!(errs(&s).contains("不能写字面值"));
        let s = skill(vec![secret_p("pw")], vec![er("Password:", "{{pw}}<Enter>", true)]);
        assert!(errs(&s).contains("不能写字面值"));
        let s = skill(vec![secret_p("pw")], vec![er("Password:", "", true)]);
        assert!(validate(&s).is_ok());
        let s = skill(vec![secret_p("pw")], vec![er("Password:", " {{ pw }} ", true)]);
        assert!(validate(&s).is_ok());
        let s = skill(vec![p("pw")], vec![er("Password:", "{{pw}}", true)]);
        assert!(errs(&s).contains("不是 secret 参数"));
    }

    #[test]
    fn password_prompt_requires_secret_respond() {
        let s = skill(vec![], vec![er("Password:", "hunter2<Enter>", false)]);
        assert!(errs(&s).contains("口令提示"));
        let s = skill(vec![], vec![er("/passphrase/i", "x", false)]);
        assert!(errs(&s).contains("口令提示"));
        let s = skill(vec![], vec![er("请输入密码", "x", false)]);
        assert!(errs(&s).contains("口令提示"));
    }

    #[test]
    fn literal_secrets_rejected() {
        for cmd in [
            "export PASSWORD=hunter2",
            "curl -u x --password hunter2 http://a",
            "curl -H 'Authorization: Bearer abcdefgh12345678' http://a",
            "echo sk-abcdefghijklmn",
            "sshpass -p hunter2 ssh a@b",
            "mysql -u root -phunter2 db",
            "echo AKIAABCDEFGHIJKLMNOP",
            "echo token=\"abc def\"",
        ] {
            let s = skill(vec![], vec![exec(cmd)]);
            assert!(errs(&s).contains("口令"), "应拒绝：{cmd}");
        }
        // 引用不算字面值
        for cmd in ["echo password=$PW", "echo token={{t}}", "mysql -u root -p db", "ls --password"] {
            let s = skill(vec![p("t")], vec![exec(cmd)]);
            assert!(validate(&s).is_ok(), "不应拒绝：{cmd} {:?}", validate(&s));
        }
        // 按键 / 应答里同样查
        let s = skill(vec![], vec![keys("export PASSWORD=hunter2<Enter>")]);
        assert!(errs(&s).contains("口令"));
        let s = skill(vec![], vec![er("ok?", "sk-abcdefghijklmn", false)]);
        assert!(errs(&s).contains("口令"));
    }

    #[test]
    fn exec_must_be_single_line() {
        let s = skill(vec![], vec![exec("ls\nrm x")]);
        assert!(errs(&s).contains("单行"));
        let s = skill(vec![], vec![exec("ls\t-l")]);
        assert!(validate(&s).is_ok());
        let s = skill(vec![], vec![exec("  ")]);
        assert!(errs(&s).contains("命令为空"));
        let s = skill(vec![], vec![exec(&"a".repeat(MAX_COMMAND_CHARS + 1))]);
        assert!(errs(&s).contains("超过"));
    }

    #[test]
    fn keys_and_patterns_validated() {
        let s = skill(vec![], vec![keys("<C-cc>")]);
        assert!(errs(&s).contains("按键串无效"));
        let s = skill(vec![], vec![keys("")]);
        assert!(errs(&s).contains("按键串为空"));
        let s = skill(vec![], vec![check("/(unclosed/")]);
        assert!(errs(&s).contains("正则无法编译"));
        let s = skill(vec![], vec![check("")]);
        assert!(errs(&s).contains("pattern 为空"));
        let s = skill(vec![], vec![check(&"a".repeat(MAX_PATTERN_CHARS + 1))]);
        assert!(errs(&s).contains("pattern 超过"));
        // 正则里的占位符换 x 后能编译
        let s = skill(vec![p("v")], vec![check("/ver {{v}}$/")]);
        assert!(validate(&s).is_ok());
        let s = skill(vec![], vec![er("ok", "", false)]);
        assert!(errs(&s).contains("respond 为空"));
    }

    #[test]
    fn timeout_range() {
        let mut st = exec("ls");
        if let SkillStep::Exec { timeout_ms, .. } = &mut st {
            *timeout_ms = Some(5);
        }
        assert!(errs(&skill(vec![], vec![st.clone()])).contains("timeoutMs"));
        if let SkillStep::Exec { timeout_ms, .. } = &mut st {
            *timeout_ms = Some(MAX_TIMEOUT_MS + 1);
        }
        assert!(errs(&skill(vec![], vec![st.clone()])).contains("timeoutMs"));
        if let SkillStep::Exec { timeout_ms, .. } = &mut st {
            *timeout_ms = Some(30_000);
        }
        assert!(validate(&skill(vec![], vec![st])).is_ok());
    }

    #[test]
    fn host_scope_rules() {
        let mut s = skill(vec![], vec![exec("ls")]);
        s.host_scope = HostScope::List { hosts: vec![] };
        assert!(errs(&s).contains("至少要有一项"));
        s.host_scope = HostScope::List { hosts: vec!["a b".into()] };
        assert!(errs(&s).contains("无效"));
        s.host_scope = HostScope::List { hosts: vec!["".into()] };
        assert!(errs(&s).contains("无效"));
        s.host_scope = HostScope::List { hosts: (0..MAX_HOST_ENTRIES + 1).map(|i| format!("h{i}")).collect() };
        assert!(errs(&s).contains("最多"));
    }

    #[test]
    fn host_scope_matching() {
        assert!(HostScope::Any.allows("s", "h"));
        let sc = HostScope::List { hosts: vec!["*.prod.example.com".into(), "SESSION-1".into(), "db?".into()] };
        assert!(sc.allows("x", "web1.PROD.example.com"));
        assert!(sc.allows("session-1", "whatever"));
        assert!(!sc.allows("x", "web1.stage.example.com"));
        assert!(!sc.allows("x", "prod.example.com")); // `*.` 要求至少有一个点前缀
        assert!(!sc.allows("x", "db1")); // 只有 * 通配，? 是字面
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(glob_match("*", ""));
        assert!(!glob_match("a*b", "a"));
    }

    #[test]
    fn schema_version_checked() {
        let mut s = skill(vec![], vec![exec("ls")]);
        s.schema_version = SCHEMA_VERSION + 1;
        assert!(errs(&s).contains("schemaVersion"));
        s.schema_version = 0;
        assert!(errs(&s).contains("schemaVersion"));
    }

    #[test]
    fn validate_reports_all_problems_at_once() {
        let mut s = skill(vec![], vec![exec("{{a}}"), keys("<C-cc>")]);
        s.name = String::new();
        let e = validate(&s).unwrap_err();
        assert!(e.len() >= 3, "{e:?}");
    }

    #[test]
    fn oversize_skill_rejected() {
        let mut s = skill(vec![], vec![exec("ls")]);
        // 上限按字符数卡，而 JSON 大小按字节：多字节字符能让 100 步撑过总上限。
        s.steps = (0..MAX_STEPS)
            .map(|_| {
                SkillStep::Exec {
                    command: "echo ".to_string() + &"字".repeat(MAX_COMMAND_CHARS - 5),
                    timeout_ms: None,
                    expect_after: Some("b".repeat(MAX_PATTERN_CHARS)),
                    on_unexpected: OnUnexpected::Ask,
                }
            })
            .collect();
        let r = validate(&s);
        assert!(r.is_err() && errs(&s).contains("KB"), "{r:?}");
    }

    // ——— 风险 ———

    #[test]
    fn risk_levels() {
        let r = |steps: Vec<SkillStep>| assess_risk(&steps).risk;
        assert_eq!(r(vec![exec("ls -l /var/log"), exec("cat /etc/os-release | grep NAME")]), Risk::Low);
        assert_eq!(r(vec![exec("df -h 2>/dev/null")]), Risk::Low);
        assert_eq!(r(vec![exec("systemctl status nginx")]), Risk::Low);
        assert_eq!(r(vec![exec("systemctl restart nginx")]), Risk::Medium);
        assert_eq!(r(vec![exec("git pull")]), Risk::Medium);
        assert_eq!(r(vec![exec("ls > out.txt")]), Risk::High); // danger::assess 判覆盖式重定向
        assert_eq!(r(vec![exec("echo hi >> log")]), Risk::Medium);
        assert_eq!(r(vec![exec("ls $(whoami)")]), Risk::Medium);
        assert_eq!(r(vec![exec("ls; make install")]), Risk::Medium);
        assert_eq!(r(vec![exec("cat {{f}}")]), Risk::Medium);
        assert_eq!(r(vec![exec("{{cmd}} --flag")]), Risk::High);
        assert_eq!(r(vec![exec("sudo systemctl restart nginx")]), Risk::High);
        assert_eq!(r(vec![exec("rm -rf /tmp/x")]), Risk::High);
        assert_eq!(r(vec![exec("psql -c 'drop table users'")]), Risk::High);
        assert_eq!(r(vec![keys("<C-c>")]), Risk::Low);
        assert_eq!(r(vec![keys("q")]), Risk::Low);
        assert_eq!(r(vec![keys("ls<Enter>")]), Risk::Medium);
        assert_eq!(r(vec![keys("rm -rf /<Enter>")]), Risk::High);
        assert_eq!(r(vec![keys("echo a > f<Enter>")]), Risk::High);
        assert_eq!(r(vec![er("ok?", "y<Enter>", false)]), Risk::Medium);
        assert_eq!(r(vec![er("ok?", "<Down>", false)]), Risk::Low);
        assert_eq!(r(vec![er("Password:", "", true)]), Risk::Medium);
        assert_eq!(r(vec![secret_step("x", None)]), Risk::Medium);
        assert_eq!(r(vec![check("ok")]), Risk::Low);
        assert_eq!(r(vec![]), Risk::Low);
    }

    #[test]
    fn risk_report_explains() {
        let rep = assess_risk(&[exec("ls"), exec("sudo rm -rf /var/x")]);
        assert_eq!(rep.risk, Risk::High);
        assert_eq!(rep.reasons.len(), 1);
        assert!(rep.reasons[0].contains("第 2 步"));
        assert!(rep.reasons[0].contains("sudo"));
    }

    #[test]
    fn normalize_only_raises_risk() {
        let mut s = skill(vec![], vec![exec("rm -rf /tmp/x")]);
        assert_eq!(s.risk, Risk::High);
        s.risk = Risk::Low;
        assert!(errs(&s).contains("低于自动评估"));
        s.normalize();
        assert_eq!(s.risk, Risk::High);
        // 用户手动标高不会被降
        let mut s = skill(vec![], vec![exec("ls")]);
        s.risk = Risk::High;
        s.normalize();
        assert_eq!(s.risk, Risk::High);
        assert!(validate(&s).is_ok());
    }

    // ——— 参数解析 ———

    #[test]
    fn resolve_params_merges_defaults_and_checks() {
        let decls = vec![
            SkillParam { default_value: Some("main".into()), ..p("branch") },
            SkillParam { required: true, ..p("host") },
            p("opt"),
            secret_p("pw"),
        ];
        let r = resolve_params(&decls, &vals(&[("host", "web1")])).unwrap();
        assert_eq!(r.values["branch"], "main");
        assert_eq!(r.values["host"], "web1");
        assert_eq!(r.values["opt"], "");
        assert!(r.secret.contains("pw") && !r.values.contains_key("pw"));
        let r = resolve_params(&decls, &vals(&[("host", "h"), ("branch", "dev")])).unwrap();
        assert_eq!(r.values["branch"], "dev");
        // 空串视为没给：回落默认
        let r = resolve_params(&decls, &vals(&[("host", "h"), ("branch", "")])).unwrap();
        assert_eq!(r.values["branch"], "main");

        let e = resolve_params(&decls, &vals(&[])).unwrap_err().join("|");
        assert!(e.contains("缺少必填参数「host」"));
        let e = resolve_params(&decls, &vals(&[("host", "h"), ("zzz", "1")])).unwrap_err().join("|");
        assert!(e.contains("未声明"));
        let e = resolve_params(&decls, &vals(&[("host", "h"), ("pw", "hunter2")])).unwrap_err().join("|");
        assert!(e.contains("secret 参数「pw」不能随调用传入"));
        assert!(!e.contains("hunter2"));
        let e = resolve_params(&decls, &vals(&[("host", "a\nrm -rf /")])).unwrap_err().join("|");
        assert!(e.contains("值无效"));
        let e = resolve_params(&decls, &vals(&[("host", &"x".repeat(MAX_PARAM_VALUE_CHARS + 1))])).unwrap_err().join("|");
        assert!(e.contains("值无效"));
    }

    // ——— 渲染 ———

    fn rp(decls: &[SkillParam], pairs: &[(&str, &str)]) -> ResolvedParams {
        resolve_params(decls, &vals(pairs)).unwrap()
    }

    #[test]
    fn render_exec_and_metadata() {
        let decls = [p("b")];
        let step = SkillStep::Exec {
            command: "git checkout {{b}} && echo '{{.X}}'".into(),
            timeout_ms: Some(900),
            expect_after: Some("on {{b}}".into()),
            on_unexpected: OnUnexpected::Llm,
        };
        let r = render_step(&step, &rp(&decls, &[("b", "dev")]));
        assert_eq!(
            r.action,
            RenderedAction::Exec { command: "git checkout dev && echo '{{.X}}'".into() }
        );
        assert_eq!(r.timeout_ms, Some(900));
        assert_eq!(r.expect_after.as_deref(), Some("on dev"));
        assert_eq!(r.on_unexpected, OnUnexpected::Llm);
    }

    #[test]
    fn render_never_substitutes_secret_params() {
        let decls = [p("u"), secret_p("pw")];
        let params = rp(&decls, &[("u", "root")]);
        // 即便（绕过校验地）把 secret 引用写进命令，渲染也只留引用
        let r = render_step(&exec("login {{u}} {{pw}}"), &params);
        assert_eq!(r.action, RenderedAction::Exec { command: "login root {{pw}}".into() });
        // secret 应答渲染成引用
        let r = render_step(&er("Password:", "{{pw}}", true), &params);
        assert_eq!(
            r.action,
            RenderedAction::ExpectRespond {
                expect: "Password:".into(),
                respond: Respond::Secret { param: Some("pw".into()) }
            }
        );
        let r = render_step(&er("Password:", "", true), &params);
        assert!(matches!(
            r.action,
            RenderedAction::ExpectRespond { respond: Respond::Secret { param: None }, .. }
        ));
        let r = render_step(&secret_step("sudo", Some("pw")), &params);
        assert_eq!(
            r.action,
            RenderedAction::Secret { reason: "sudo".into(), param: Some("pw".into()) }
        );
    }

    #[test]
    fn render_keys_escapes_param_values() {
        let decls = [p("v")];
        // 参数值里的 `\x03` 和 `<C-c>` 只能是字面字符
        let params = rp(&decls, &[("v", r"a\x03<C-c>b")]);
        let r = render_step(&keys("{{v}}<Enter>"), &params);
        let RenderedAction::Keys { keys: k } = r.action else { panic!() };
        let bytes = parse_keys(&k).unwrap();
        assert_eq!(bytes, b"a\\x03<C-c>b\r".to_vec());
        let r = render_step(&er("ok?", "{{v}}\\r", false), &params);
        let RenderedAction::ExpectRespond { respond: Respond::Keys { keys: k }, .. } = r.action else {
            panic!()
        };
        assert_eq!(parse_keys(&k).unwrap(), b"a\\x03<C-c>b\r".to_vec());
    }

    #[test]
    fn render_patterns_cannot_change_kind() {
        let decls = [p("v")];
        // 正则：值被转义
        let r = render_step(&check("/^ver {{v}}$/"), &rp(&decls, &[("v", "1.2+")]));
        let RenderedAction::ScreenCheck { expect } = r.action else { panic!() };
        let pat = Pattern::parse(&expect).unwrap();
        assert!(pat.is_match("ver 1.2+") && !pat.is_match("ver 1.22") && !pat.is_match("ver 1x2+"));
        // 子串：值以 / 开头会被误读成正则 → 改写成等价字面 regex
        let r = render_step(&check("{{v}}"), &rp(&decls, &[("v", "/a.c/")]));
        let RenderedAction::ScreenCheck { expect } = r.action else { panic!() };
        let pat = Pattern::parse(&expect).unwrap();
        assert!(pat.is_match("x /a.c/ y") && !pat.is_match("/abc/"));
        // 普通子串原样
        let r = render_step(&check("v={{v}}"), &rp(&decls, &[("v", "1")]));
        assert_eq!(r.action, RenderedAction::ScreenCheck { expect: "v=1".into() });
    }

    #[test]
    fn render_check_and_json_shape() {
        let c = render_check(
            &SkillCheck { expect: "up {{v}}".into(), timeout_ms: Some(1000) },
            &rp(&[p("v")], &[("v", "3")]),
        );
        assert_eq!(c.expect, "up 3");
        let r = render_step(&keys("<C-c>"), &ResolvedParams::default());
        let j = serde_json::to_value(&r).unwrap();
        assert_eq!(j, json!({"kind":"keys","keys":"<C-c>","timeoutMs":null,"expectAfter":null,"onUnexpected":"ask"}));
    }

    // ——— 序列化形状 ———

    #[test]
    fn step_json_shape() {
        let j = serde_json::to_value(vec![
            exec("ls"),
            er("a", "b", false),
            secret_step("r", Some("pw")),
            secret_step("r", None),
        ])
        .unwrap();
        assert_eq!(
            j,
            json!([
                {"kind":"exec","command":"ls","onUnexpected":"ask"},
                {"kind":"expect_respond","expect":"a","respond":"b","secret":false,"onUnexpected":"ask"},
                {"kind":"secret","reason":"r","param":"pw","onUnexpected":"ask"},
                {"kind":"secret","reason":"r","onUnexpected":"ask"},
            ])
        );
        // 缺省字段可省略
        let st: SkillStep = serde_json::from_value(json!({"kind":"screen_check","expect":"x"})).unwrap();
        assert_eq!(st, check("x"));
        assert_eq!(st.on_unexpected(), OnUnexpected::Ask);
        let st: SkillStep = serde_json::from_value(
            json!({"kind":"exec","command":"ls","timeoutMs":5000,"expectAfter":"a","onUnexpected":"llm"}),
        )
        .unwrap();
        assert_eq!(st.timeout_ms(), Some(5000));
        assert_eq!(st.expect_after(), Some("a"));
        assert_eq!(st.on_unexpected(), OnUnexpected::Llm);
        let sc: HostScope = serde_json::from_value(json!({"kind":"list","hosts":["a"]})).unwrap();
        assert_eq!(sc, HostScope::List { hosts: vec!["a".into()] });
        assert_eq!(serde_json::to_value(HostScope::Any).unwrap(), json!({"kind":"any"}));
        let pa: SkillParam = serde_json::from_value(json!({"name":"a","default":"d"})).unwrap();
        assert_eq!(pa.default_value.as_deref(), Some("d"));
        assert!(!pa.required && !pa.secret);
    }

    #[test]
    fn secret_step_has_no_value_field_by_construction() {
        for bad in [
            json!({"kind":"secret","reason":"r","value":"hunter2"}),
            json!({"kind":"secret","reason":"r","password":"hunter2"}),
            json!({"kind":"exec","command":"ls","value":"x"}),
            json!({"kind":"expect_respond","expect":"a","respond":"","secret":true,"value":"x"}),
            json!({"kind":"nope"}),
        ] {
            assert!(serde_json::from_value::<SkillStep>(bad.clone()).is_err(), "{bad}");
        }
        assert!(serde_json::from_value::<SkillParam>(json!({"name":"a","secret":true,"value":"x"})).is_err());
    }

    // ——— 导入 / 导出 ———

    #[test]
    fn export_import_roundtrip() {
        let mut s = skill(
            vec![SkillParam { default_value: Some("main".into()), description: "分支".into(), ..p("branch") }, secret_p("pw")],
            vec![
                exec("git checkout {{branch}}"),
                keys("<C-c>"),
                er("Password:", "{{pw}}", true),
                secret_step("sudo", Some("pw")),
                check("/ok/i"),
            ],
        );
        s.verify = Some(SkillCheck { expect: "done".into(), timeout_ms: Some(2000) });
        s.host_scope = HostScope::List { hosts: vec!["*.example.com".into()] };
        s.success_count = 7;
        s.fail_count = 2;
        s.last_run_ts = Some(555);
        s.source = SkillSource::Recorded;
        s.risk = Risk::High;

        let text = export_text(&s);
        assert!(text.contains("\"format\": \"nexus-skill\""));
        assert!(text.contains("\"version\": 1"));
        assert!(text.contains('\n'), "应是美化过的");
        let back = import_text(&text, "new-id".into(), 999).unwrap();

        // 内容一致
        assert_eq!(back.name, s.name);
        assert_eq!(back.description, s.description);
        assert_eq!(back.params, s.params);
        assert_eq!(back.steps, s.steps);
        assert_eq!(back.verify, s.verify);
        assert_eq!(back.host_scope, s.host_scope);
        assert_eq!(back.risk, Risk::High);
        // 不受信：新 id、来源、计数、时间
        assert_eq!(back.id, "new-id");
        assert_eq!(back.source, SkillSource::Imported);
        assert_eq!((back.success_count, back.fail_count, back.last_run_ts), (0, 0, None));
        assert_eq!((back.created_at, back.updated_at), (999, 999));
        assert_eq!(back.schema_version, SCHEMA_VERSION);
        // 导出文本不含运行统计
        assert!(!text.contains("\"successCount\": 7"));
    }

    #[test]
    fn import_rejects_bad_input() {
        let ok = |skill: Value| json!({"format":"nexus-skill","version":1,"skill":skill}).to_string();
        let good = json!({"name":"n","steps":[{"kind":"exec","command":"ls"}]});
        assert!(import_text(&ok(good.clone()), "i".into(), 1).is_ok());

        let e = |t: &str| import_text(t, "i".into(), 1).unwrap_err().join("|");
        assert!(e("not json").contains("不是有效的 JSON"));
        assert!(e("{}").contains("不是 Nexus skill 文件"));
        assert!(e(r#"{"format":"other","version":1,"skill":{}}"#).contains("不是 Nexus skill 文件"));
        assert!(e(r#"{"format":"nexus-skill","skill":{}}"#).contains("缺少 version"));
        assert!(e(r#"{"format":"nexus-skill","version":99,"skill":{}}"#).contains("不受支持"));
        assert!(e(r#"{"format":"nexus-skill","version":1}"#).contains("缺少 skill"));
        assert!(e(&ok(json!({"steps":[]}))).contains("skill 内容无效"));
        // 校验失败：空步骤、未声明占位符
        assert!(e(&ok(json!({"name":"n","steps":[]}))).contains("至少需要一个步骤"));
        assert!(e(&ok(json!({"name":"n","steps":[{"kind":"exec","command":"{{x}}"}]}))).contains("没有对应的参数声明"));
        // 超大
        let big = "x".repeat(MAX_IMPORT_BYTES + 1);
        assert!(e(&big).contains("文件过大"));
    }

    #[test]
    fn import_rejects_secret_values() {
        let t = |step: Value| {
            json!({"format":"nexus-skill","version":1,"skill":{"name":"n","steps":[step]}}).to_string()
        };
        let e = import_text(&t(json!({"kind":"secret","reason":"r","value":"hunter2"})), "i".into(), 1)
            .unwrap_err()
            .join("|");
        assert!(e.contains("unknown field"), "{e}");
        // 带口令的命令被校验拦下
        let e = import_text(&t(json!({"kind":"exec","command":"export PASSWORD=hunter2"})), "i".into(), 1)
            .unwrap_err()
            .join("|");
        assert!(e.contains("口令"));
        // secret 参数带 default
        let txt = json!({"format":"nexus-skill","version":1,"skill":{
            "name":"n","params":[{"name":"pw","secret":true,"default":"hunter2"}],
            "steps":[{"kind":"secret","reason":"r","param":"pw"}]}})
        .to_string();
        assert!(import_text(&txt, "i".into(), 1).unwrap_err().join("|").contains("不能有默认值"));
    }

    #[test]
    fn import_cannot_smuggle_trust_or_low_risk() {
        let txt = json!({"format":"nexus-skill","version":1,"skill":{
            "id":"evil","name":"n","risk":"low","source":"recorded",
            "successCount":1000,"failCount":0,"lastRunTs":5,"createdAt":1,"updatedAt":1,
            "steps":[{"kind":"exec","command":"sudo rm -rf /var/x"}]}})
        .to_string();
        let s = import_text(&txt, "fresh".into(), 50).unwrap();
        assert_eq!(s.id, "fresh");
        assert_eq!(s.risk, Risk::High, "风险被重新评估并抬高");
        assert_eq!(s.source, SkillSource::Imported);
        assert_eq!(s.success_count, 0);
        assert_eq!(s.last_run_ts, None);
        assert_eq!((s.created_at, s.updated_at), (50, 50));
    }

    #[test]
    fn into_skill_zeroes_counters_and_normalizes() {
        let input = SkillInput {
            name: "  n  ".into(),
            description: " d ".into(),
            params: vec![],
            steps: vec![exec("rm x")],
            verify: None,
            risk: Risk::Low,
            host_scope: HostScope::Any,
            source: Some(SkillSource::Recorded),
        };
        let s = input.clone().into_skill("i".into(), 7);
        assert_eq!((s.name.as_str(), s.description.as_str()), ("n", "d"));
        assert_eq!(s.risk, Risk::High);
        assert_eq!((s.success_count, s.fail_count, s.last_run_ts), (0, 0, None));
        assert_eq!(s.source, SkillSource::Recorded);
        assert!(s.same_behavior(&SkillInput::from(&s)));
        let mut changed = input;
        changed.steps = vec![exec("ls")];
        assert!(!s.same_behavior(&changed));
    }

    #[test]
    fn skill_json_roundtrip_with_defaults() {
        // 最小 JSON 也能读（旧数据 / 手写）
        let s: Skill = serde_json::from_value(json!({"name":"n","steps":[{"kind":"keys","keys":"q"}]})).unwrap();
        assert_eq!(s.risk, Risk::Low);
        assert_eq!(s.host_scope, HostScope::Any);
        assert_eq!(s.source, SkillSource::Manual);
        assert_eq!(s.schema_version, SCHEMA_VERSION);
        let back: Skill = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }
}
