//! 剧本化的假模型：本机起一个 OpenAI 兼容的 `/chat/completions`，按剧本回工具调用。
//!
//! **生产代码零接缝**：`llm.rs` 照常拼请求、走 reqwest、解析回复 —— 只是 `base_url` 指向
//! `127.0.0.1:{port}`。所以重试 / 降级 / 工具配对修复这些出网层逻辑也一并在回放里。
//!
//! 剧本 = 有序的 [`Turn`]，外加一次性的 [`Rule`]：
//! - 每次「托管主循环」的请求（带 tools）先看规则：上一条反馈（末尾的 tool / user 消息）里含
//!   `when` 就回它的 reply（每条规则只触发一次），不消耗剧本游标；
//! - 否则取下一个 Turn；Turn 的 `expect` 不满足时记一条 mismatch（测试结束时断言为空），照常回复；
//! - 剧本用完 → 回 HTTP 400（Fatal），运行以 Failed 结束，测试立刻看得到，而不是卡 120s。
//!
//! 不带 tools 的一次性请求（判据起草、摘要……）：系统提示词是判据起草 → 回 `criteria`；
//! 其它回一句「好的」。所有请求原样记在 `requests` 里，失败时 [`FakeLlm::transcript`] 打出来排查。

use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// 模型的一次回复。
#[derive(Clone, Debug)]
pub enum Reply {
    /// 一个或多个工具调用（name, arguments）
    Tools(Vec<(String, Value)>),
    /// 纯文本（不调工具）
    Text(String),
}

impl Reply {
    pub fn tool(name: &str, args: Value) -> Self {
        Reply::Tools(vec![(name.to_string(), args)])
    }
}

#[derive(Clone, Debug)]
pub struct Turn {
    /// 上一条反馈必须包含它（剧本断言）；None = 不检查
    pub expect: Option<String>,
    pub reply: Reply,
}

#[derive(Clone, Debug)]
pub struct Rule {
    pub when: String,
    pub reply: Reply,
}

#[derive(Default)]
struct Inner {
    turns: Vec<Turn>,
    cursor: usize,
    rules: Vec<Option<Rule>>,
    criteria: Value,
    /// 每次托管主循环请求时「上一条反馈」的文本（按顺序）
    feedback: Vec<String>,
    mismatches: Vec<String>,
    requests: Vec<Value>,
    calls: u64,
}

#[derive(Clone)]
pub struct FakeLlm {
    pub port: u16,
    inner: Arc<Mutex<Inner>>,
}

/// 判据起草的系统提示词开头（见 `host::criteria::DRAFT_SYSTEM`）。
fn is_criteria_draft(system: &str) -> bool {
    system == super::super::host::criteria::DRAFT_SYSTEM
}

/// OpenAI 方言里「上一条反馈」：最后一条 assistant 之后的全部 tool / user 消息正文。
pub fn last_feedback(messages: &[Value]) -> String {
    let start = messages.iter().rposition(|m| m["role"] == "assistant").map_or(0, |i| i + 1);
    messages[start..]
        .iter()
        .filter(|m| m["role"] != "system")
        .filter_map(|m| m["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

impl FakeLlm {
    /// 在 tauri 的异步运行时上起服务，返回时已经在监听。
    pub fn start(turns: Vec<Turn>, rules: Vec<Rule>, criteria: Value) -> Self {
        let inner = Arc::new(Mutex::new(Inner {
            turns,
            rules: rules.into_iter().map(Some).collect(),
            criteria,
            ..Default::default()
        }));
        let listener = tauri::async_runtime::block_on(TcpListener::bind("127.0.0.1:0")).expect("假模型监听失败");
        let port = listener.local_addr().unwrap().port();
        let srv = Arc::clone(&inner);
        tauri::async_runtime::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else { break };
                let srv = Arc::clone(&srv);
                tauri::async_runtime::spawn(async move {
                    let _ = serve(sock, srv).await;
                });
            }
        });
        Self { port, inner }
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// 剧本走到第几步了（已回复的 Turn 数）。
    pub fn cursor(&self) -> usize {
        self.inner.lock().cursor
    }

    pub fn mismatches(&self) -> Vec<String> {
        self.inner.lock().mismatches.clone()
    }

    /// 托管主循环每次请求时模型看到的「上一条反馈」。
    pub fn feedback(&self) -> Vec<String> {
        self.inner.lock().feedback.clone()
    }

    /// 全部请求的可读摘要（失败时打印）。
    pub fn transcript(&self) -> String {
        let g = self.inner.lock();
        let mut out = String::new();
        for (i, f) in g.feedback.iter().enumerate() {
            out.push_str(&format!("—— 模型第 {i} 次看到的反馈 ——\n{}\n", truncate(f, 1500)));
        }
        out
    }

    fn respond(inner: &Mutex<Inner>, req: &Value) -> Result<Value, String> {
        let mut g = inner.lock();
        g.requests.push(req.clone());
        g.calls += 1;
        let messages = req["messages"].as_array().cloned().unwrap_or_default();
        let system = messages.first().filter(|m| m["role"] == "system").and_then(|m| m["content"].as_str()).unwrap_or("");
        let has_tools = req["tools"].as_array().is_some_and(|t| !t.is_empty());
        if !has_tools {
            let text = if is_criteria_draft(system) { g.criteria.to_string() } else { "好的".to_string() };
            return Ok(completion(&Reply::Text(text), g.calls));
        }
        let fb = last_feedback(&messages);
        g.feedback.push(fb.clone());
        if let Some(slot) = g.rules.iter_mut().find(|r| r.as_ref().is_some_and(|r| fb.contains(&r.when))) {
            let rule = slot.take().unwrap();
            return Ok(completion(&rule.reply, g.calls));
        }
        let Some(turn) = g.turns.get(g.cursor).cloned() else {
            return Err(format!("剧本已用完（共 {} 步），模型又被请求了一次", g.turns.len()));
        };
        g.cursor += 1;
        if let Some(want) = &turn.expect {
            if !fb.contains(want.as_str()) {
                let n = g.cursor;
                g.mismatches.push(format!("第 {n} 步期望反馈含「{want}」，实际：\n{}", truncate(&fb, 1500)));
            }
        }
        Ok(completion(&turn.reply, g.calls))
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

fn completion(reply: &Reply, n: u64) -> Value {
    let msg = match reply {
        Reply::Text(t) => json!({ "role": "assistant", "content": t }),
        Reply::Tools(calls) => {
            let calls: Vec<Value> = calls
                .iter()
                .enumerate()
                .map(|(i, (name, args))| {
                    json!({ "id": format!("call_{n}_{i}"), "type": "function",
                            "function": { "name": name, "arguments": args.to_string() } })
                })
                .collect();
            json!({ "role": "assistant", "content": null, "tool_calls": calls })
        }
    };
    let finish = if matches!(reply, Reply::Tools(_)) { "tool_calls" } else { "stop" };
    json!({ "id": format!("e2e-{n}"), "object": "chat.completion", "model": "e2e-fake",
            "choices": [{ "index": 0, "message": msg, "finish_reason": finish }] })
}

/// 极简 HTTP/1.1：读头、按 Content-Length 读体、回一个 JSON、关连接。
async fn serve(mut sock: tokio::net::TcpStream, inner: Arc<Mutex<Inner>>) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let header_end = loop {
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let len = head
        .lines()
        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
        .unwrap_or(0);
    while buf.len() < header_end + len {
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let req: Value = serde_json::from_slice(&buf[header_end..]).unwrap_or(Value::Null);
    let (status, body) = match FakeLlm::respond(&inner, &req) {
        Ok(v) => ("200 OK", v.to_string()),
        Err(e) => ("400 Bad Request", json!({ "error": { "message": e } }).to_string()),
    };
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    sock.write_all(resp.as_bytes()).await?;
    sock.shutdown().await
}
