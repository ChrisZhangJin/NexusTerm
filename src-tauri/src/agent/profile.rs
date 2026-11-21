//! NexusAgent v2 / P2 的决策依据：Profile（偏好）、Intent（意向栈）、KB（知识）、Playbook（剧本）。
//!
//! 设计见 `docs/nexus-agent-steward-design.md` §4。这一层只管「用户预先放进来的东西」，
//! **不含任何终端输出**：屏幕文本是**不可信数据**，只能在 `decision.rs` 里作为匹配输入，
//! 绝不能反过来写进 Brain 或提升成约束。这是 §4.3 冲突消解的第 8 位永远压不过第 1~7 位的保证。
//!
//! 存储走 `store::steward` 的一张 `steward_ctx` 表；`StewardBrain` 是它的内存镜像，
//! 启动时一次性载入（几十条），常驻循环只读内存，不碰 SQLite 连接锁。
//!
//! 四类条目共用一个外壳 `CtxItem`，各自特有的字段放 `extra` JSON：
//! - `preference`：`{ "strength": "hard|soft" }`（strength 另存列，便于过滤）
//! - `intent`：`IntentSpec { successCriteria, constraints, answerTemplate, ... }`
//! - `knowledge`：`{ "tags": ["keychain", "vault"] }`
//! - `playbook`：`PlaybookSpec { matchProgram, matchKind, matchTopic, matchText, answer, optionIndex, tier, enabled }`

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::decision::DecisionRecord;
use super::situation::{Program, Situation};
use super::steward::now_ms;
use crate::store::{CtxRow, Store};

/// 单条文本上限（字符）。偏好 / 知识 / 回复模板都不该是长文档；
/// 真要放长文档走资料库，这里只放能给模型/规则用的短依据。
pub const MAX_TEXT_CHARS: usize = 2000;
/// 每类条目上限，防止误操作把内存与面板撑爆。
pub const MAX_ITEMS_PER_KIND: usize = 100;
/// 内存里保留的完整 DecisionRecord 条数（升级卡一键确认要按 seq 找回）。
const DECISION_CACHE: usize = 100;

pub const KIND_PREFERENCE: &str = "preference";
pub const KIND_INTENT: &str = "intent";
pub const KIND_KNOWLEDGE: &str = "knowledge";
pub const KIND_PLAYBOOK: &str = "playbook";

// ─────────────────────────── 数据结构 ───────────────────────────

/// 四类依据共用的外壳。序列化统一 camelCase，前端直接拿去渲染。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CtxItem {
    pub id: String,
    pub kind: String,
    pub text: String,
    /// hard | soft（偏好专用，其余为 soft）
    pub strength: String,
    /// global | claude_code | ssh | shell | <hostKey>
    pub scope: String,
    /// user | learned | seed
    pub source: String,
    pub evidence: String,
    pub extra: Value,
    /// 意向归属的契约；其余为空
    pub contract_id: String,
    /// 意向栈顺序，越大越靠栈顶
    pub position: i32,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 前端「新增 / 编辑一条依据」的输入。id 为空表示新建。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CtxInput {
    pub id: Option<String>,
    pub kind: String,
    pub text: String,
    pub strength: Option<String>,
    pub scope: Option<String>,
    pub source: Option<String>,
    pub evidence: Option<String>,
    pub extra: Option<Value>,
    pub contract_id: Option<String>,
    pub position: Option<i32>,
}

/// 意向的附加字段。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct IntentSpec {
    #[serde(default)]
    pub success_criteria: Vec<String>,
    #[serde(default)]
    pub constraints: Vec<String>,
    /// 需要替用户回答时使用的完整回复模板，支持 `{goal}` `{constraints}`
    /// `{options}` `{topic}` `{knowledge}` 占位符。**有意不调 LLM**：P2 的
    /// 回复由用户预置的意向/偏好确定性编译，可复核、可回放。
    pub answer_template: Option<String>,
    pub priority: Option<i32>,
    pub deadline_ms: Option<i64>,
    pub parent_id: Option<String>,
    /// active | done | archived
    pub status: Option<String>,
}

/// 确定性剧本。命中即产出固定动作，不调模型（设计 §4.2 / 场景 D）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PlaybookSpec {
    /// "*" 或 `Program::as_str()`
    pub match_program: Option<String>,
    /// "*" 或 `SituationKind::as_str()`
    pub match_kind: Option<String>,
    /// 与 situation.topic / tail 的子串匹配（不区分大小写）
    pub match_topic: Option<String>,
    /// 与脱敏屏幕 tail 的子串匹配；比 topic 更宽，用来认 ssh known_hosts 这类固定提问
    pub match_text: Option<String>,
    /// 要发出去的文本。若 `optionIndex` 有值则改发该选项编号。
    pub answer: String,
    pub option_index: Option<String>,
    /// auto | notify | approve；缺省 auto
    pub tier: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// KB 检索结果。带 `id` 是为了让 `DecisionRecord.ctxRefs` 能指回具体条目。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KnowledgeHit {
    pub id: String,
    pub text: String,
    pub score: i32,
    /// 固定标注：KB 内容是不可信数据，不能改写 Profile / Intent / Authority
    pub untrusted: bool,
}

/// 面板一次拿全的视图。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrainView {
    pub items: Vec<CtxItem>,
    pub decision_count: usize,
}

// ─────────────────────────── Brain ───────────────────────────

#[derive(Default)]
struct BrainInner {
    items: Vec<CtxItem>,
    decisions: HashMap<i64, DecisionRecord>,
}

#[derive(Clone, Default)]
pub struct StewardBrain(Arc<Mutex<BrainInner>>);

impl StewardBrain {
    pub fn new() -> Self {
        Self::default()
    }

    /// 从库里载入；表空时写入一组可编辑的默认种子。
    pub fn load(store: &Store) -> Self {
        let brain = Self::new();
        let mut rows = store.steward_ctx_load();
        if rows.is_empty() {
            for seed in seed_items() {
                store.steward_ctx_upsert(&seed.to_row()).ok();
                rows.push(seed.to_row());
            }
        }
        let mut inner = brain.0.lock();
        inner.items = rows
            .into_iter()
            .filter_map(|r| CtxItem::from_row(&r))
            .collect();
        inner.items.sort_by(|a, b| {
            a.kind
                .cmp(&b.kind)
                .then(a.position.cmp(&b.position))
                .then(a.created_at.cmp(&b.created_at))
        });
        drop(inner);
        brain
    }

    pub fn view(&self) -> BrainView {
        let inner = self.0.lock();
        BrainView {
            items: inner.items.clone(),
            decision_count: inner.decisions.len(),
        }
    }

    /// 新增 / 更新一条。校验在这里做，持久化由命令层接着调 `store.steward_ctx_upsert`。
    pub fn upsert(&self, input: CtxInput) -> Result<CtxItem, String> {
        let item = normalize_input(input)?;
        {
            let inner = self.0.lock();
            let n = inner
                .items
                .iter()
                .filter(|x| x.kind == item.kind && x.id != item.id)
                .count();
            if n >= MAX_ITEMS_PER_KIND {
                return Err(format!("{kind} 最多 {MAX_ITEMS_PER_KIND} 条", kind = item.kind));
            }
        }
        let mut inner = self.0.lock();
        if let Some(existing) = inner.items.iter_mut().find(|x| x.id == item.id) {
            *existing = item.clone();
        } else {
            inner.items.push(item.clone());
        }
        Ok(item)
    }

    pub fn delete(&self, id: &str) -> bool {
        let mut inner = self.0.lock();
        let before = inner.items.len();
        inner.items.retain(|x| x.id != id);
        before != inner.items.len()
    }

    /// 契约停止时清掉它的意向栈。
    pub fn drop_intents(&self, contract_id: &str) -> Vec<String> {
        let mut inner = self.0.lock();
        let mut ids = Vec::new();
        inner.items.retain(|x| {
            let hit = x.kind == KIND_INTENT && x.contract_id == contract_id;
            if hit {
                ids.push(x.id.clone());
            }
            !hit
        });
        ids
    }

    pub fn intent_stack(&self, contract_id: &str) -> Vec<CtxItem> {
        let inner = self.0.lock();
        let mut v: Vec<_> = inner
            .items
            .iter()
            .filter(|x| x.kind == KIND_INTENT && x.contract_id == contract_id)
            .cloned()
            .collect();
        v.sort_by(|a, b| b.position.cmp(&a.position));
        v
    }

    /// 某个程序 / 主机上生效的偏好。`scope` 支持 global / program / hostKey / "*"。
    pub fn applicable_preferences(
        &self,
        program: Program,
        host_key: &str,
    ) -> (Vec<CtxItem>, Vec<CtxItem>) {
        let inner = self.0.lock();
        let mut hard = Vec::new();
        let mut soft = Vec::new();
        for x in inner.items.iter().filter(|x| x.kind == KIND_PREFERENCE) {
            let applies = matches!(x.scope.as_str(), "global" | "*")
                || x.scope == program.as_str()
                || x.scope == host_key;
            if !applies {
                continue;
            }
            if x.strength == "hard" {
                hard.push(x.clone());
            } else {
                soft.push(x.clone());
            }
        }
        (hard, soft)
    }

    /// KB 检索：tag 命中 +2，topic 命中 +1，正文含关键词 +1；取前 k 条。
    /// 结果固定标 `untrusted=true`，`decision.rs` 只把它当数据拼进回复模板。
    pub fn retrieve_kb(&self, situation: &Situation, k: usize) -> Vec<KnowledgeHit> {
        let inner = self.0.lock();
        let haystack = format!(
            "{} {} {} {}",
            situation.program.as_str(),
            situation.kind.as_str(),
            situation.topic.as_deref().unwrap_or(""),
            situation.tail.to_lowercase()
        )
        .to_lowercase();
        let topic = situation.topic.as_deref().unwrap_or("").to_lowercase();
        let mut hits: Vec<KnowledgeHit> = inner
            .items
            .iter()
            .filter(|x| x.kind == KIND_KNOWLEDGE)
            .filter_map(|x| {
                let tags: Vec<String> = x
                    .extra
                    .get("tags")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_lowercase)
                            .collect()
                    })
                    .unwrap_or_default();
                let mut score = 0;
                for t in &tags {
                    if !t.is_empty() && haystack.contains(t) {
                        score += 2;
                    }
                }
                if !topic.is_empty() && haystack.contains(&topic) {
                    score += 1;
                }
                if score == 0 {
                    return None;
                }
                Some(KnowledgeHit {
                    id: x.id.clone(),
                    text: x.text.clone(),
                    score,
                    untrusted: true,
                })
            })
            .collect();
        hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.id.cmp(&b.id)));
        hits.truncate(k.clamp(1, 6));
        hits
    }

    /// 剧本匹配：program / kind / topic / tail 四项，给了就都要命中（AND）。
    /// 剧本与偏好用同一套作用域：global / "*" / 程序名 / host_key。
    /// 在 A 主机上蒸馏出的剧本（scope = A 的 host_key）不会在 B 主机上自动作答。
    pub fn match_playbook(
        &self,
        situation: &Situation,
        host_key: &str,
    ) -> Option<(CtxItem, PlaybookSpec)> {
        let inner = self.0.lock();
        let tail = situation.tail.to_lowercase();
        let topic = situation.topic.as_deref().unwrap_or("").to_lowercase();
        inner
            .items
            .iter()
            .filter(|x| x.kind == KIND_PLAYBOOK)
            .filter(|x| {
                matches!(x.scope.as_str(), "global" | "*")
                    || x.scope == situation.program.as_str()
                    || x.scope == host_key
            })
            .filter_map(|x| {
                let spec: PlaybookSpec = serde_json::from_value(x.extra.clone()).ok()?;
                if !spec.enabled || spec.answer.trim().is_empty() {
                    return None;
                }
                if let Some(p) = spec.match_program.as_deref() {
                    if p != "*" && p != situation.program.as_str() {
                        return None;
                    }
                }
                if let Some(k) = spec.match_kind.as_deref() {
                    if k != "*" && k != situation.kind.as_str() {
                        return None;
                    }
                }
                if let Some(t) = spec.match_topic.as_deref() {
                    if !t.is_empty() && !topic.contains(&t.to_lowercase()) {
                        return None;
                    }
                }
                if let Some(t) = spec.match_text.as_deref() {
                    if !t.is_empty() && !tail.contains(&t.to_lowercase()) {
                        return None;
                    }
                }
                Some((x.clone(), spec))
            })
            .next()
    }

    /// 记住一条决策：解释链 / 纠正按 seq 找回它。
    pub fn remember_decision(&self, seq: i64, record: &DecisionRecord) {
        let mut inner = self.0.lock();
        inner.decisions.insert(seq, record.clone());
        if inner.decisions.len() > DECISION_CACHE {
            let oldest = *inner.decisions.keys().min().unwrap_or(&0);
            inner.decisions.remove(&oldest);
        }
    }

    pub fn decision(&self, seq: i64) -> Option<DecisionRecord> {
        self.0.lock().decisions.get(&seq).cloned()
    }

    /// 内存里那份决策的 outcome 也要跟着 SQLite 改：升级卡批准时读的是这里，
    /// 只改库不改内存，「已执行 / 已拒绝」的卡在内存里永远还是 escalated，可以被反复批准。
    pub fn set_decision_outcome(&self, seq: i64, outcome: &str) {
        if let Some(r) = self.0.lock().decisions.get_mut(&seq) {
            r.outcome = outcome.to_string();
        }
    }

    /// 闭环学习（§4.4）：用户否掉一条自动决策时，把纠正写成 `Learned` 软偏好，
    /// 下次同主题检索会命中它。`evidence` 指向原决策 seq，可回放。
    pub fn learn_correction(&self, seq: i64, topic: &str, correction: &str) -> Result<CtxItem, String> {
        let text = format!("关于「{topic}」：{correction}");
        self.upsert(CtxInput {
            id: None,
            kind: KIND_PREFERENCE.to_string(),
            text,
            strength: Some("soft".to_string()),
            scope: Some("global".to_string()),
            source: Some("learned".to_string()),
            evidence: Some(format!("DecisionRecord #{seq} 被用户纠正")),
            extra: Some(json!({ "topic": topic })),
            contract_id: None,
            position: None,
        })
    }
}

impl CtxItem {
    pub fn to_row(&self) -> CtxRow {
        CtxRow {
            id: self.id.clone(),
            kind: self.kind.clone(),
            text: self.text.clone(),
            strength: self.strength.clone(),
            scope: self.scope.clone(),
            source: self.source.clone(),
            evidence: self.evidence.clone(),
            extra: serde_json::to_string(&self.extra).unwrap_or_else(|_| "{}".to_string()),
            contract_id: self.contract_id.clone(),
            position: self.position,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }

    /// 坏行不致命：extra 不是 JSON 就退化成空对象，kind 不认识就跳过。
    pub fn from_row(r: &CtxRow) -> Option<Self> {
        if !matches!(
            r.kind.as_str(),
            KIND_PREFERENCE | KIND_INTENT | KIND_KNOWLEDGE | KIND_PLAYBOOK
        ) {
            return None;
        }
        Some(Self {
            id: r.id.clone(),
            kind: r.kind.clone(),
            text: r.text.clone(),
            strength: r.strength.clone(),
            scope: r.scope.clone(),
            source: r.source.clone(),
            evidence: r.evidence.clone(),
            extra: serde_json::from_str(&r.extra).unwrap_or_else(|_| json!({})),
            contract_id: r.contract_id.clone(),
            position: r.position,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }

}

/// 统一校验：kind 白名单、文本限长、无控制字符、无凭据形状。
/// 与 `hostmem::validate_note_text` 同思路——依据文本会被拼进回复发到终端，
/// 不能夹带换行/ESC 之类的控制序列，更不能把口令写进偏好里。
pub fn validate_text(text: &str) -> Result<String, String> {
    let t = text.trim().to_string();
    if t.is_empty() {
        return Err("内容不能为空".to_string());
    }
    if t.chars().count() > MAX_TEXT_CHARS {
        return Err(format!("内容不能超过 {MAX_TEXT_CHARS} 字"));
    }
    if t.chars().any(|c| c.is_control()) {
        return Err("内容不能包含换行或控制字符".to_string());
    }
    let lower = t.to_lowercase();
    if lower.contains("password:") || lower.contains("passphrase") || lower.contains("private key") {
        return Err("依据里不能写口令 / 私钥；密码类输入走独立通道".to_string());
    }
    Ok(t)
}

fn normalize_input(input: CtxInput) -> Result<CtxItem, String> {
    let kind = input.kind.trim().to_lowercase();
    if !matches!(
        kind.as_str(),
        KIND_PREFERENCE | KIND_INTENT | KIND_KNOWLEDGE | KIND_PLAYBOOK
    ) {
        return Err(format!("不认识的条目类型「{kind}」"));
    }
    let text = validate_text(&input.text)?;
    let ts = now_ms();
    let strength = match input.strength.as_deref() {
        Some("hard") => "hard",
        _ => "soft",
    };
    let source = match input.source.as_deref() {
        Some("learned") => "learned",
        Some("seed") => "seed",
        _ => "user",
    };
    let mut extra = input.extra.unwrap_or_else(|| json!({}));
    if !extra.is_object() {
        extra = json!({});
    }
    // 剧本的回复模板单独校验：它会原样发进终端，必须单行、有限长
    if kind == KIND_PLAYBOOK {
        let spec: PlaybookSpec = serde_json::from_value(extra.clone())
            .map_err(|e| format!("剧本 extra 格式不对：{e}"))?;
        validate_text(&spec.answer).map_err(|e| format!("剧本回复：{e}"))?;
    }
    if kind == KIND_INTENT {
        let spec: IntentSpec = serde_json::from_value(extra.clone())
            .map_err(|e| format!("意向 extra 格式不对：{e}"))?;
        if let Some(tpl) = spec.answer_template.as_deref() {
            validate_text(tpl).map_err(|e| format!("回复模板：{e}"))?;
        }
        for c in &spec.constraints {
            validate_text(c)?;
        }
    }
    Ok(CtxItem {
        id: input
            .id
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("ctx-{kind}-{ts}-{:04x}", next_salt())),
        kind,
        text,
        strength: strength.to_string(),
        scope: input
            .scope
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "global".to_string()),
        source: source.to_string(),
        evidence: input.evidence.unwrap_or_default(),
        extra,
        contract_id: input.contract_id.unwrap_or_default(),
        position: input.position.unwrap_or(0),
        created_at: ts,
        updated_at: ts,
    })
}

fn next_salt() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(1);
    N.fetch_add(1, Ordering::Relaxed) & 0xffff
}

/// 首次启动的种子：**全部可编辑 / 可删除**。给用户一个能直接跑通场景 A/B 的起点，
/// 而不是一个空壳面板。seed 条目在 UI 上会标来源，避免误以为是系统自带的硬规则。
pub fn seed_items() -> Vec<CtxItem> {
    let ts = now_ms();
    let make = |id: &str, kind: &str, text: &str, strength: &str, extra: Value| CtxItem {
        id: id.to_string(),
        kind: kind.to_string(),
        text: text.to_string(),
        strength: strength.to_string(),
        scope: "global".to_string(),
        source: "seed".to_string(),
        evidence: "首次启动预置，可编辑或删除".to_string(),
        extra,
        contract_id: String::new(),
        position: 0,
        created_at: ts,
        updated_at: ts,
    };
    vec![
        make(
            "seed-hp-1",
            KIND_PREFERENCE,
            "不要改设置界面的开关（含「是否记住」），优先直接改代码",
            "hard",
            json!({}),
        ),
        make(
            "seed-hp-2",
            KIND_PREFERENCE,
            "绝不自动选择会永久扩大授权的选项（don't ask again / always allow）",
            "hard",
            json!({}),
        ),
        make(
            "seed-hp-3",
            KIND_PREFERENCE,
            "密码、密钥、验证码一律由用户本人输入，Agent 不代填",
            "hard",
            json!({}),
        ),
        make(
            "seed-sp-1",
            KIND_PREFERENCE,
            "最小改动，优先复用现有代码与测试；回复用中文、命令用英文",
            "soft",
            json!({}),
        ),
        make(
            "seed-kb-1",
            KIND_KNOWLEDGE,
            "钥匙串二次弹窗根因：vault_try_remembered 被调用两次；修复应只判条目是否存在，并做稳定签名的 release build 冷启动验证",
            "soft",
            json!({ "tags": ["keychain", "vault", "钥匙串", "remember"] }),
        ),
        make(
            "seed-kb-2",
            KIND_KNOWLEDGE,
            "claude code 权限提示里编号 1 通常是「本次允许」，含 always / don't ask again 的编号会永久扩权",
            "soft",
            json!({ "tags": ["permission", "claude_code", "always"] }),
        ),
        make(
            "seed-pb-ssh",
            KIND_PLAYBOOK,
            "SSH 首次连接确认：目标主机在授权范围内时回答 yes",
            "soft",
            json!({
                "matchProgram": "ssh",
                "matchText": "are you sure you want to continue connecting",
                "answer": "yes",
                "tier": "auto",
                "enabled": true
            }),
        ),
        make(
            "seed-pb-pager",
            KIND_PLAYBOOK,
            "less / more 分页器停在 (END) 时退出",
            "soft",
            json!({
                "matchKind": "pager",
                "answer": "q",
                "tier": "auto",
                "enabled": true
            }),
        ),
        make(
            "seed-pb-npm",
            KIND_PLAYBOOK,
            "npm init / 交互式脚本的默认提问：沿用括号里的默认值",
            "soft",
            json!({
                "matchProgram": "script",
                "matchText": "package name:",
                "answer": "",
                "tier": "notify",
                "enabled": false
            }),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::situation::{Program, SituationKind};

    fn brain() -> StewardBrain {
        let b = StewardBrain::new();
        *b.0.lock() = BrainInner {
            items: seed_items(),
            ..BrainInner::default()
        };
        b
    }

    #[test]
    fn seeds_are_editable_and_valid() {
        let items = seed_items();
        assert!(items.len() >= 8);
        for x in &items {
            assert!(validate_text(&x.text).is_ok(), "{}", x.text);
        }
        assert_eq!(
            items
                .iter()
                .filter(|x| x.kind == KIND_PREFERENCE && x.strength == "hard")
                .count(),
            3
        );
    }

    #[test]
    fn validate_rejects_control_chars_and_credentials() {
        assert!(validate_text("正常偏好").is_ok());
        assert!(validate_text("带\n换行").is_err());
        assert!(validate_text("").is_err());
        assert!(validate_text("password: hunter2").is_err());
        assert!(validate_text(&"字".repeat(MAX_TEXT_CHARS + 1)).is_err());
    }

    #[test]
    fn normalize_creates_playbook_and_rejects_bad_extra() {
        let b = brain();
        let item = b
            .upsert(CtxInput {
                id: None,
                kind: "playbook".into(),
                text: "测速脚本确认".into(),
                strength: None,
                scope: Some("shell".into()),
                source: None,
                evidence: None,
                extra: Some(json!({ "matchKind": "question", "answer": "y" })),
                contract_id: None,
                position: None,
            })
            .unwrap();
        assert_eq!(item.kind, KIND_PLAYBOOK);
        assert_eq!(item.scope, "shell");
        assert!(b.upsert(CtxInput {
            id: None,
            kind: "playbook".into(),
            text: "坏剧本".into(),
            strength: None,
            scope: None,
            source: None,
            evidence: None,
            extra: Some(json!({ "answer": "带\n换行" })),
            contract_id: None,
            position: None,
        })
        .is_err());
    }

    #[test]
    fn preferences_filter_by_scope() {
        let b = brain();
        let (hard, soft) = b.applicable_preferences(Program::ClaudeCode, "local:tab-1");
        assert_eq!(hard.len(), 3);
        assert_eq!(soft.len(), 1);
        let (hard2, _) = b.applicable_preferences(Program::Shell, "other");
        assert_eq!(hard2.len(), 3, "global 硬约束对所有程序生效");
    }

    #[test]
    fn kb_retrieval_is_scored_and_marked_untrusted() {
        let b = brain();
        let sit = Situation {
            frame_id: 1,
            screen_hash: "a".into(),
            program: Program::ClaudeCode,
            kind: SituationKind::Question,
            topic: Some("keychain:remember-check".into()),
            options: vec![],
            confidence: 0.9,
            tail: "keychain vault_try_remembered called twice".into(),
            evidence: vec![],
            stale_ms: 0,
            signature: "claude_code|question|keychain".into(),
        };
        let hits = b.retrieve_kb(&sit, 6);
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|h| h.untrusted));
        assert_eq!(hits[0].id, "seed-kb-1");
    }

    #[test]
    fn playbook_matches_are_conjunctive() {
        let b = brain();
        let mut sit = Situation {
            frame_id: 1,
            screen_hash: "a".into(),
            program: Program::Ssh,
            kind: SituationKind::Question,
            topic: None,
            options: vec![],
            confidence: 0.9,
            tail: "Are you sure you want to continue connecting (yes/no/[fingerprint])?".into(),
            evidence: vec![],
            stale_ms: 0,
            signature: "ssh|question".into(),
        };
        let hit = b.match_playbook(&sit, "local:tab-1").unwrap();
        assert_eq!(hit.0.id, "seed-pb-ssh");
        assert_eq!(hit.1.answer, "yes");
        sit.tail = "别的提示".into();
        assert!(b.match_playbook(&sit, "local:tab-1").is_none());
    }

    #[test]
    fn host_scoped_playbook_only_matches_its_host() {
        let b = brain();
        b.upsert(CtxInput {
            id: Some("pb-host-a".into()),
            kind: KIND_PLAYBOOK.to_string(),
            text: "A 主机专用".into(),
            strength: None,
            scope: Some("ssh:root@a:22".into()),
            source: Some("user".into()),
            evidence: None,
            extra: Some(serde_json::json!({
                "matchProgram": "*", "matchText": "only-on-a", "answer": "ok", "enabled": true
            })),
            contract_id: None,
            position: None,
        })
        .unwrap();
        let sit = Situation {
            frame_id: 1,
            screen_hash: "a".into(),
            program: Program::Shell,
            kind: SituationKind::Question,
            topic: None,
            options: vec![],
            confidence: 0.9,
            tail: "only-on-a ?".into(),
            evidence: vec![],
            stale_ms: 0,
            signature: "x".into(),
        };
        assert!(b.match_playbook(&sit, "ssh:root@a:22").is_some());
        assert!(b.match_playbook(&sit, "ssh:root@b:22").is_none());
    }

    #[test]
    fn decision_outcome_updates_in_memory_copy() {
        let b = brain();
        let r = DecisionRecord {
            seq: 5, ts_ms: 0, contract_id: "c".into(), console_id: "t".into(),
            situation_signature: "s".into(), situation_hash: "h".into(),
            ctx_refs: vec![], candidates: vec![],
            chosen: crate::agent::decision::DecisionAction::answer("x".into(), None),
            rationale: String::new(), confidence: 0.9, tier: "approve".into(),
            llm_used: false, outcome: "escalated".into(), escalation: None, receipt: None,
        };
        b.remember_decision(5, &r);
        b.set_decision_outcome(5, "applied");
        assert_eq!(b.decision(5).unwrap().outcome, "applied");
    }

    #[test]
    fn intent_stack_is_per_contract_and_dropped_with_it() {
        let b = brain();
        for (i, status) in [("active", 1), ("done", 9)].iter().enumerate() {
            b.upsert(CtxInput {
                id: Some(format!("i{i}")),
                kind: "intent".into(),
                text: format!("意向 {i}"),
                strength: None,
                scope: None,
                source: None,
                evidence: None,
                extra: Some(json!({ "status": status.0 })),
                contract_id: Some("c1".into()),
                position: Some(status.1),
            })
            .unwrap();
        }
        assert_eq!(b.intent_stack("c1").len(), 2);
        assert_eq!(b.drop_intents("c1").len(), 2);
    }

    #[test]
    fn learned_correction_points_at_the_decision() {
        let b = brain();
        let item = b.learn_correction(12, "settings:remember-check", "别改设置，直接改代码").unwrap();
        assert_eq!(item.source, "learned");
        assert!(item.evidence.contains("#12"));
    }
}
