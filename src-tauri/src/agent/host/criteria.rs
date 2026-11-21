//! 完成判据起草（设计 §5 已定：模型起草、用户确认；YOLO 跳过确认）。
//!
//! 开始托管时一次 LLM 调用把目标转成 `Vec<Check>` 草稿。起草失败（模型不可用 / 回了看不懂的东西 /
//! 一条都没有）一律退回 `[Manual{"由你确认完成"}]`：判据永远不为空，完成永远要有人或代码点头。
//! 校验（`host::verify`）在 PR7。纯函数，网络调用在 `host_cmd.rs`。

use super::Check;

/// 起草失败时的唯一判据。
pub const FALLBACK_TEXT: &str = "由你确认完成";
/// 草稿最多几条
const MAX_CHECKS: usize = 8;
/// 单条字段的长度上限（字符）
const FIELD_CAP: usize = 300;

pub const DRAFT_SYSTEM: &str = "你是终端托管助手。把用户的托管目标转成可检验的完成判据。\n\
只输出一个 JSON 数组，不要任何解释。每个元素是下面四种之一：\n\
- {\"kind\":\"commandOk\",\"cmd\":\"只读命令，退出码为 0 即满足\"}（必须是只读命令，如 test / grep / curl -fsS）\n\
- {\"kind\":\"fileExists\",\"path\":\"/绝对/路径\"}\n\
- {\"kind\":\"screenMatches\",\"re\":\"终端当前屏幕应匹配的正则\"}\n\
- {\"kind\":\"manual\",\"text\":\"只能由人确认的一项\"}\n\
最多 5 条。能用命令或文件检验的就别用 manual；实在无法检验时给一条 manual。\n\
命令 / 文件判据在**另开的 shell** 里执行（工作目录 = 终端当前目录），不会敲进终端前台的程序：\
终端里跑着 Claude Code / vim 等也照样能核验；别写依赖前台程序状态的命令。";

pub fn fallback() -> Vec<Check> {
    vec![Check::Manual { text: FALLBACK_TEXT.to_string() }]
}

/// 起草请求里的用户消息。
pub fn draft_prompt(goal: &str) -> String {
    format!("托管目标：{}\n请输出完成判据 JSON 数组。", goal.trim())
}

fn clip(s: &str) -> String {
    s.trim().chars().take(FIELD_CAP).collect()
}

/// 规整一条判据：空的丢掉，过长的截断，正则写错的降为人工确认。
fn tidy(c: Check) -> Option<Check> {
    match c {
        Check::CommandOk { cmd } => Some(clip(&cmd)).filter(|s| !s.is_empty()).map(|cmd| Check::CommandOk { cmd }),
        Check::FileExists { path } => Some(clip(&path)).filter(|s| !s.is_empty()).map(|path| Check::FileExists { path }),
        Check::Manual { text } => Some(clip(&text)).filter(|s| !s.is_empty()).map(|text| Check::Manual { text }),
        Check::ScreenMatches { re } => {
            let re = clip(&re);
            if re.is_empty() {
                None
            } else if regex::Regex::new(&re).is_err() {
                Some(Check::Manual { text: format!("屏幕上出现：{re}") })
            } else {
                Some(Check::ScreenMatches { re })
            }
        }
    }
}

/// 从模型回复里抽出判据数组（容忍 ```json 围栏与前后废话）。一条都没有算失败。
pub fn parse_draft(text: &str) -> Result<Vec<Check>, String> {
    let start = text.find('[').ok_or("回复里没有 JSON 数组")?;
    let end = text.rfind(']').filter(|e| *e > start).ok_or("JSON 数组不完整")?;
    let raw: Vec<serde_json::Value> =
        serde_json::from_str(&text[start..=end]).map_err(|e| format!("判据不是合法 JSON：{e}"))?;
    let checks: Vec<Check> = raw
        .into_iter()
        .filter_map(|v| serde_json::from_value::<Check>(v).ok())
        .filter_map(tidy)
        .take(MAX_CHECKS)
        .collect();
    if checks.is_empty() {
        return Err("没有可用的判据".to_string());
    }
    Ok(checks)
}

/// 起草结果 → 判据；任何失败都退回人工确认。
pub fn draft_or_fallback(reply: Result<String, String>) -> Vec<Check> {
    match reply.and_then(|t| parse_draft(&t)) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("完成判据起草失败，退回人工确认：{e}");
            fallback()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fenced_array_and_tidies() {
        let t = "好的：\n```json\n[{\"kind\":\"commandOk\",\"cmd\":\" cargo test \"},\
                 {\"kind\":\"fileExists\",\"path\":\"\"},{\"kind\":\"screenMatches\",\"re\":\"([\"},\
                 {\"kind\":\"bogus\"}]\n```";
        assert_eq!(
            parse_draft(t).unwrap(),
            vec![
                Check::CommandOk { cmd: "cargo test".into() },
                Check::Manual { text: "屏幕上出现：([".into() },
            ]
        );
    }

    #[test]
    fn drafting_failure_falls_back_to_manual() {
        assert_eq!(draft_or_fallback(Err("429".into())), fallback());
        assert_eq!(draft_or_fallback(Ok("我觉得做完就行".into())), fallback());
        assert_eq!(draft_or_fallback(Ok("[]".into())), fallback());
        assert_eq!(draft_or_fallback(Ok("[{\"kind\":\"manual\",\"text\":\"  \"}]".into())), fallback());
        assert_eq!(fallback(), vec![Check::Manual { text: "由你确认完成".into() }]);
    }

    #[test]
    fn caps_number_of_checks() {
        let many: Vec<String> = (0..20).map(|i| format!("{{\"kind\":\"manual\",\"text\":\"t{i}\"}}")).collect();
        assert_eq!(parse_draft(&format!("[{}]", many.join(","))).unwrap().len(), MAX_CHECKS);
    }
}
