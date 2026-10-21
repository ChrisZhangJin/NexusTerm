//! keyboard-interactive 认证（RFC 4256）的人机交互桥。
//!
//! 二次验证全部走这条协议：服务端在认证**中途**下发一组问题（"OTP Code:"、
//! "Password:"、"Verification code:"…），客户端把答案回填进去才能继续握手。
//! 后端不可能自己编答案，只能把问题抛给界面、挂起等待。
//!
//! 实现方式是一张「会话 id → 一次性应答通道」的登记表：
//!
//! ```text
//!   connect() ──ask()──▶ emit nx://auth/prompt ──▶ 前端弹窗
//!        ▲                                              │
//!        └────────── oneshot 唤醒 ◀── respond() ◀───────┘
//! ```
//!
//! 事件走**单一全局通道**而不是按会话分段（`nx://auth/prompt/<id>`）：
//! 前端只需注册一次监听，也不用为每个标签维护订阅生命周期。
//! 同一时刻只可能有一个连接在等认证，全局通道不会串台。
//!
//! 注意：这里等的是**用户**，不是网络，所以超时给得比 TCP 宽松得多
//! （默认 3 分钟），否则用户低头找手机的工夫就会被迫重连。

use std::collections::HashMap;
use std::time::Duration;

use parking_lot::Mutex;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::oneshot;

/// 等待用户输入验证码的上限。超时视同用户取消。
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(180);

/// 一条待填的字段。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthPromptField {
    /// 服务端给的提示语，如 "OTP Code:"
    pub prompt: String,
    /// false = 输入需要隐藏（密码 / 验证码），true = 明文回显
    pub echo: bool,
    /// 已替用户填好的默认值（目前只有"密码"类问题会自动带入已保存的密码）。
    /// 前端应当把它放进输入框，但**允许用户改写** —— 密码输错时用户能就地修正。
    pub prefill: Option<String>,
}

/// 一次性下发给前端的一组问题。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthPrompt {
    /// 哪个会话在等这次输入（前端回填答案时原样带回）
    pub session_id: String,
    /// 提问类型：
    /// * `keyboard-interactive` —— 服务端下发的二次验证 / 密码挑战
    /// * `passphrase` —— 本地私钥已加密，需要口令才能用
    ///
    /// 前端据此切换标题与提示，两种问题混在同一个弹窗队列里。
    pub kind: String,
    /// 连的是哪台机器，弹窗标题用
    pub host: String,
    /// 登录用户名。弹窗里和主机一起展示，避免同时开着几台机器时认错窗口。
    /// 私钥口令那类问题在握手之前发生，可以留空。
    #[serde(default)]
    pub username: String,
    /// 这组问题的标题，服务端通常填 "OTP Code" 之类
    pub name: String,
    /// 补充说明，可能为空
    pub instructions: String,
    pub fields: Vec<AuthPromptField>,
    /// 第几轮（1 起）。服务端可能连问多轮：先密码、再验证码
    pub round: u32,
}

/// 没拿到答案的三种原因。分开是因为提示文案完全不同：
/// 用户取消不该说"超时"，超时也不该说"已取消"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskAbort {
    /// 用户点了取消，或会话被关闭
    Cancelled,
    /// 等太久（[`PROMPT_TIMEOUT`]）
    Timeout,
    /// 连事件都发不出去（窗口已销毁等）
    Unavailable,
}

/// 「会话 id → 应答通道」登记表。
#[derive(Default)]
pub struct AuthPrompts {
    pending: Mutex<HashMap<String, oneshot::Sender<Result<Vec<String>, AskAbort>>>>,
}

impl AuthPrompts {
    pub fn new() -> Self {
        Self::default()
    }

    /// 把问题推给前端并挂起，直到用户提交或中止。
    pub async fn ask(
        &self,
        app: &AppHandle,
        prompt: AuthPrompt,
    ) -> Result<Vec<String>, AskAbort> {
        let session_id = prompt.session_id.clone();
        let (tx, rx) = oneshot::channel();
        // 同一会话只可能有一个问题在等，直接覆盖旧槽位
        self.pending.lock().insert(session_id.clone(), tx);

        if let Err(e) = app.emit("nx://auth/prompt", &prompt) {
            // 连事件都发不出去，没必要干等
            tracing::error!(session = %session_id, "下发二次验证问题失败：{e}");
            self.pending.lock().remove(&session_id);
            return Err(AskAbort::Unavailable);
        }

        let got = tokio::time::timeout(PROMPT_TIMEOUT, rx).await;
        self.pending.lock().remove(&session_id);

        match got {
            Ok(Ok(Ok(answers))) => Ok(answers),
            Ok(Ok(Err(abort))) => Err(abort),
            // 发送端被丢弃：等价于会话没了
            Ok(Err(_)) => Err(AskAbort::Cancelled),
            Err(_) => {
                tracing::warn!(session = %session_id, "等待二次验证输入超时");
                Err(AskAbort::Timeout)
            }
        }
    }

    /// 前端回填答案。`answers = None` 表示用户主动取消。
    ///
    /// 返回 false 说明没有对应的等待者（多半是已经超时或会话已关），
    /// 属于正常竞态，前端不必报错。
    pub fn respond(&self, session_id: &str, answers: Option<Vec<String>>) -> bool {
        let Some(tx) = self.pending.lock().remove(session_id) else {
            return false;
        };
        tx.send(match answers {
            Some(a) => Ok(a),
            None => Err(AskAbort::Cancelled),
        })
        .is_ok()
    }

    /// 会话被关闭 / 用户点断开时解除挂起，避免后台 connect 任务一直挂着。
    pub fn cancel(&self, session_id: &str) {
        if let Some(tx) = self.pending.lock().remove(session_id) {
            let _ = tx.send(Err(AskAbort::Cancelled));
        }
    }
}

#[cfg(test)]
mod prompt_tests {
    use super::*;

    /// 直接塞一个发送端，绕过需要 AppHandle 的 ask()
    fn waiter(prompts: &AuthPrompts, id: &str) -> oneshot::Receiver<Result<Vec<String>, AskAbort>> {
        let (tx, rx) = oneshot::channel();
        prompts.pending.lock().insert(id.to_string(), tx);
        rx
    }

    #[tokio::test]
    async fn respond_delivers_answers() {
        let prompts = AuthPrompts::new();
        let rx = waiter(&prompts, "s1");

        assert!(prompts.respond("s1", Some(vec!["123456".into()])));
        assert_eq!(rx.await.unwrap(), Ok(vec!["123456".to_string()]));
    }

    #[tokio::test]
    async fn respond_none_is_cancellation() {
        let prompts = AuthPrompts::new();
        let rx = waiter(&prompts, "s1");

        assert!(prompts.respond("s1", None));
        assert_eq!(rx.await.unwrap(), Err(AskAbort::Cancelled));
    }

    /// 关闭会话时要把挂起的等待者叫醒，否则后台 connect 会挂到超时
    #[tokio::test]
    async fn cancel_wakes_waiter() {
        let prompts = AuthPrompts::new();
        let rx = waiter(&prompts, "s1");

        prompts.cancel("s1");
        assert_eq!(rx.await.unwrap(), Err(AskAbort::Cancelled));
        // 槽位已被取走，重复取消是安全的空操作
        prompts.cancel("s1");
    }

    /// 超时后再回填属于正常竞态：应返回 false 而不是 panic
    #[tokio::test]
    async fn respond_after_timeout_is_false() {
        let prompts = AuthPrompts::new();
        let _rx = waiter(&prompts, "s1");
        prompts.pending.lock().remove("s1");

        assert!(!prompts.respond("s1", Some(vec!["x".into()])));
    }

    /// 同一个会话重复登机时，旧槽位必须被顶掉，不能留着两个等待者
    #[tokio::test]
    async fn ask_slot_is_single() {
        let prompts = AuthPrompts::new();
        let _old = waiter(&prompts, "s1");
        let new = waiter(&prompts, "s1");

        assert!(prompts.respond("s1", Some(vec!["new".into()])));
        assert_eq!(new.await.unwrap(), Ok(vec!["new".to_string()]));
        assert!(prompts.pending.lock().get("s1").is_none());
    }
}
