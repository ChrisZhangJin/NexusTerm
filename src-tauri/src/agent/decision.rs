//! 决策记录：托管写入（模型动手 / 反射快路径）留下的可审计 `DecisionRecord`。
//!
//! 早期这里是守护进程的零 LLM 决策编译器（`compile`）、写后回执校验与自动应答总闸；
//! PR8 删掉守护进程写路径后它们都没有调用方了，只留下数据结构与按键编码：
//! - `DecisionAction::send_bytes`：反射快路径（`host::reflex`）把剧本答案编成按键的唯一出口，
//!   单行、无控制字符，方向键菜单一律不写；
//! - `DecisionRecord` / `CtxRef`：`ctxRefs` 指回具体条目 id，`candidates` 记考虑过没选的，
//!   `rationale` 写清为什么。面板的决策回放、解释链、复盘都读它。
//!
//! 屏幕输出是不可信数据：只用来匹配与渲染，绝不写回 Profile / Intent / Authority。

use serde::{Deserialize, Serialize};

use super::situation::SubmitStyle;

// ─────────────────────────── 动作 ───────────────────────────

/// 一个动作。`injectable=true` 才会真的往 PTY 写字节；其余只是生命周期决定。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DecisionAction {
    /// answer | secret | wait | observe | advance | handoff
    pub kind: String,
    /// answer 是完整回复文本（不是把问题转述给人）；其余是原因 / 摘要
    pub text: String,
    /// 编号菜单里选中的选项编号；有值时注入的是编号而不是整段文本
    pub option_index: Option<String>,
    pub injectable: bool,
    /// 选中编号之后怎么提交（来自局面的 `situation::submit_style`）。旧记录没有这个字段 → 行式提示
    #[serde(default)]
    pub submit: SubmitStyle,
}

impl DecisionAction {
    pub fn answer(text: String, option_index: Option<String>) -> Self {
        Self {
            kind: "answer".to_string(),
            text,
            option_index,
            injectable: true,
            submit: SubmitStyle::NeedsEnter,
        }
    }

    /// 真正要送进 PTY 的字节。文本必须单行（校验在 `profile::validate_text`），
    /// 以 `\r` 收尾表示 Enter；含换行会让交互程序收到多条命令，这里直接拒绝。
    ///
    /// 选编号时看 `submit`：`DigitSubmits` 的菜单（Claude Code 权限卡）按数字即提交，只发数字——
    /// 多出的回车会落到下一屏；`ArrowSelect` 菜单输入编号 / 文字都无效、回车会提交默认项，一律不写。
    /// P3 起 `advance`（空转时催一句 / 翻页）与 `answer` 走同一条注入通道，
    /// 其余生命周期动作永远不写终端。
    pub fn send_bytes(&self) -> Option<Vec<u8>> {
        if !self.injectable || !matches!(self.kind.as_str(), "answer" | "advance") {
            return None;
        }
        if self.submit == SubmitStyle::ArrowSelect {
            return None;
        }
        let option = self.option_index.as_deref().filter(|s| !s.is_empty());
        let payload = option.unwrap_or(&self.text);
        if payload.is_empty() || payload.chars().any(|c| c.is_control()) {
            return None;
        }
        if option.is_some() && self.submit == SubmitStyle::DigitSubmits {
            return Some(payload.as_bytes().to_vec());
        }
        Some(format!("{payload}\r").into_bytes())
    }
}

/// 依据引用。`source` 决定优先级档位，`id` 可点回具体条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CtxRef {
    /// profile | intent | knowledge | playbook | authority | policy
    pub source: String,
    pub id: String,
    pub snippet: String,
}

impl CtxRef {
    pub(crate) fn of(source: &str, id: &str, snippet: impl Into<String>) -> Self {
        Self {
            source: source.to_string(),
            id: id.to_string(),
            snippet: snippet.into(),
        }
    }
}

/// 守护进程时代的升级卡。只为能反序列化旧的决策记录而保留，不再生成。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EscalationCard {
    pub recommendation: DecisionAction,
    pub why: Vec<CtxRef>,
    pub alternatives: Vec<DecisionAction>,
    pub default_action: DecisionAction,
    pub timeout_ms: u64,
    pub one_click: bool,
    /// 卡片生成时刻，前端据此算剩余时间
    pub created_at: i64,
}

/// 每次决策的唯一可审计记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DecisionRecord {
    /// 落库后由 SQLite 回填；内存态先填 0
    pub seq: i64,
    pub ts_ms: i64,
    pub contract_id: String,
    pub console_id: String,
    pub situation_signature: String,
    pub situation_hash: String,
    pub ctx_refs: Vec<CtxRef>,
    pub candidates: Vec<DecisionAction>,
    pub chosen: DecisionAction,
    pub rationale: String,
    pub confidence: f32,
    /// auto | notify | approve
    pub tier: String,
    /// P2 硬保证：false
    pub llm_used: bool,
    /// proposed | applied | escalated | observed | rejected | no_receipt | rolled_back
    pub outcome: String,
    pub escalation: Option<EscalationCard>,
    /// 注入后的回执结果，未注入为空
    pub receipt: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn send_bytes_is_single_line_and_ends_with_cr() {
        let a = DecisionAction::answer("yes".into(), None);
        assert_eq!(a.send_bytes().unwrap(), b"yes\r".to_vec());
        let b = DecisionAction::answer("long text".into(), Some("2".into()));
        assert_eq!(b.send_bytes().unwrap(), b"2\r".to_vec());
        let mut bad = a.clone();
        bad.text = "a\nb".into();
        assert!(bad.send_bytes().is_none());
        let mut observe = a.clone();
        observe.kind = "observe".into();
        observe.injectable = false;
        assert!(observe.send_bytes().is_none(), "生命周期动作永远不写终端");
    }

    #[test]
    fn digit_submit_menus_send_digit_only_line_prompts_add_enter() {
        // 带 `❯` 的 Claude Code 选择卡：按数字即选中并提交，多一个回车会落到下一屏
        let mut digit = DecisionAction::answer("不改设置，直接改代码".into(), Some("2".into()));
        digit.submit = SubmitStyle::DigitSubmits;
        assert_eq!(digit.send_bytes().unwrap(), b"2".to_vec());
        // 行式菜单（`Enter choice [1-3]: `）：编号 + 回车
        let line = DecisionAction::answer("不改设置，直接改代码".into(), Some("2".into()));
        assert_eq!(line.send_bytes().unwrap(), b"2\r".to_vec());

        // 真实屏幕帧走一遍分类：Claude Code 权限卡 → 数字即提交；脚本菜单 → 要回车
        use super::super::situation;
        let cc = situation::frames::classify_lines(&situation::frames::CC_TRUST);
        assert_eq!(situation::submit_style(&cc), SubmitStyle::DigitSubmits);
        let sh = situation::frames::classify_lines(&situation::frames::SCRIPT_MENU);
        assert_eq!(situation::submit_style(&sh), SubmitStyle::NeedsEnter);

        // 自由文本回答不受影响：仍以回车提交
        let mut free = DecisionAction::answer("hello".into(), None);
        free.submit = SubmitStyle::DigitSubmits;
        assert_eq!(free.send_bytes().unwrap(), b"hello\r".to_vec());
        // 方向键菜单：编号 / 文字都无效，回车会提交默认项 —— 不写
        let mut arrow = DecisionAction::answer("React".into(), Some("1".into()));
        arrow.submit = SubmitStyle::ArrowSelect;
        assert!(arrow.send_bytes().is_none());
        // 旧记录没有 submit 字段也能反序列化
        let old: DecisionAction = serde_json::from_value(json!({
            "kind": "answer", "text": "x", "optionIndex": "1", "injectable": true
        }))
        .unwrap();
        assert_eq!(old.submit, SubmitStyle::NeedsEnter);
    }

    #[test]
    fn old_record_with_escalation_card_still_parses() {
        let a = DecisionAction::answer("yes".into(), None);
        let raw = json!({
            "seq": 1, "tsMs": 1, "contractId": "c", "consoleId": "t",
            "situationSignature": "s", "situationHash": "h", "ctxRefs": [],
            "candidates": [], "chosen": a, "rationale": "r", "confidence": 0.9,
            "tier": "approve", "llmUsed": false, "outcome": "escalated",
            "escalation": {
                "recommendation": a, "why": [], "alternatives": [], "defaultAction": a,
                "timeoutMs": 30000, "oneClick": true, "createdAt": 1
            },
            "receipt": null
        });
        let r: DecisionRecord = serde_json::from_value(raw).unwrap();
        assert!(r.escalation.is_some());
    }
}
