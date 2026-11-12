//! 计划清单（`nx_todo`）：模型自己维护的「要做的几步」，每次调用整表替换。
//!
//! 清单活在运行状态里（托管还随 checkpoint 落盘），**每一轮**重新渲染进系统提示的
//! 「当前计划」一节 —— 历史被压缩折掉也丢不了。对话与托管都有。

use serde::{Deserialize, Serialize};

pub const TOOL_TODO: &str = "nx_todo";
/// 清单最多几项（太长就不是计划了）
pub const MAX_ITEMS: usize = 30;
/// 单项最多多少字
const ITEM_CAP: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    #[default]
    Pending,
    InProgress,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub text: String,
    #[serde(default)]
    pub status: TodoStatus,
}

#[derive(Debug, Deserialize)]
struct TodoArgs {
    #[serde(default)]
    items: Vec<serde_json::Value>,
}

/// 工具参数 → 清单。容忍 `status` 写成 `in-progress` / `completed` / `doing` 之类的变体；
/// 空文本的项丢掉、超长截断、最多 [`MAX_ITEMS`] 项。参数根本不是对象返回 None。
pub fn parse(args: &serde_json::Value) -> Option<Vec<TodoItem>> {
    let a: TodoArgs = serde_json::from_value(args.clone()).ok()?;
    let items = a
        .items
        .iter()
        .filter_map(|v| {
            let text = v.get("text").or_else(|| v.get("content")).and_then(|t| t.as_str())?.trim();
            if text.is_empty() {
                return None;
            }
            let status = match v.get("status").and_then(|s| s.as_str()).unwrap_or("pending").trim().to_ascii_lowercase().as_str() {
                "in_progress" | "in-progress" | "inprogress" | "doing" | "active" | "running" => TodoStatus::InProgress,
                "done" | "completed" | "complete" | "finished" | "ok" => TodoStatus::Done,
                _ => TodoStatus::Pending,
            };
            Some(TodoItem { text: text.chars().take(ITEM_CAP).collect(), status })
        })
        .take(MAX_ITEMS)
        .collect();
    Some(items)
}

/// 系统提示里的「当前计划」一节；清单为空返回空串。
pub fn render(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let mut s = String::from("## 当前计划（nx_todo 维护；推进时更新状态）\n");
    for it in items {
        let mark = match it.status {
            TodoStatus::Done => "[x]",
            TodoStatus::InProgress => "[>]",
            TodoStatus::Pending => "[ ]",
        };
        s.push_str(&format!("- {mark} {}\n", it.text));
    }
    s
}

/// 给模型的回执。
pub fn ack(items: &[TodoItem]) -> String {
    let done = items.iter().filter(|i| i.status == TodoStatus::Done).count();
    format!("计划已更新：共 {} 项，完成 {done} 项。\n", items.len())
}

/// 工具声明。
pub fn tool_spec() -> super::llm::ToolSpec {
    super::llm::ToolSpec {
        name: TOOL_TODO,
        description: "维护本次任务的计划清单（每次调用整表替换）。三步以上的任务先列计划，\
                      推进时把正在做的标 in_progress、做完的标 done。清单会一直显示在你的系统提示里。"
            .into(),
        schema: serde_json::json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "text": { "type": "string", "description": "这一步要做什么" },
                            "status": { "type": "string", "enum": ["pending", "in_progress", "done"] }
                        },
                        "required": ["text"]
                    }
                }
            },
            "required": ["items"]
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_tolerates_variants_and_limits() {
        let v = parse(&json!({"items": [
            {"text": "装依赖", "status": "completed"},
            {"text": " 跑测试 ", "status": "in-progress"},
            {"text": "", "status": "done"},
            {"content": "发版"}
        ]}))
        .unwrap();
        assert_eq!(
            v,
            vec![
                TodoItem { text: "装依赖".into(), status: TodoStatus::Done },
                TodoItem { text: "跑测试".into(), status: TodoStatus::InProgress },
                TodoItem { text: "发版".into(), status: TodoStatus::Pending },
            ]
        );
        let many: Vec<_> = (0..50).map(|i| json!({"text": format!("s{i}")})).collect();
        assert_eq!(parse(&json!({ "items": many })).unwrap().len(), MAX_ITEMS);
        assert!(parse(&json!("x")).is_none());
    }

    #[test]
    fn render_marks_status() {
        let s = render(&[
            TodoItem { text: "a".into(), status: TodoStatus::Done },
            TodoItem { text: "b".into(), status: TodoStatus::InProgress },
            TodoItem { text: "c".into(), status: TodoStatus::Pending },
        ]);
        assert!(s.starts_with("## 当前计划"));
        assert!(s.contains("- [x] a\n- [>] b\n- [ ] c\n"));
        assert_eq!(render(&[]), "");
    }
}
