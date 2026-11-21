//! 密码通道（`nx_secret`）：让 Agent 能「请用户输入一个密码」，而**永远碰不到密码本身**。
//!
//! ## 流程
//!
//! ```text
//! 模型 nx_secret{target?, reason}
//!   → 后端 emit `nx://agent/secret/{session}`（只带 reason / 目标，没有任何秘密）
//!   → 面板弹一张带「掩码输入框」的卡片，用户输入后 `agent_secret_ack(requestId, value)`
//!   → 后端在 [`await_secret`] 里收到，**直接写进目标 pty**（`value + "\r"`），
//!     不回显、不喂旁路缓冲、不进 trace、不进模型上下文
//!   → 模型只收到「用户已提供 / 用户拒绝 / 超时 / 已取消」+ 写入之后的输出尾部（脱敏后）
//! ```
//!
//! ## 秘密的活动范围（硬约束）
//!
//! - 值只存在于 [`SecretAnswer::Provided`] 的 `Zeroizing<String>` 里：一路从 tauri 命令参数 →
//!   oneshot 通道 → 写 pty 的那一刻，用完即擦。**不 derive `Debug` / `Clone` / `Serialize`**，
//!   `Debug` 手写成固定占位，万一被 `{:?}` 带进日志也只会看到 `<redacted>`。
//! - 反馈 / trace / 步骤卡全部用 [`SecretStatus`]（不含值）拼装；
//!   写入之后的输出尾部先过 [`mask_secret`]，防止「目标其实在回显」时把值带出去。
//! - 本模块不碰 Tauri、不碰 AppState：等待、校验、拼反馈都是纯逻辑，便于单测。

use std::fmt;
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::{sleep, Instant};
use zeroize::Zeroizing;

use super::prompt::PromptGuess;

/// 等用户输入的上限。超时按「没提供」处理（人去接杯咖啡回来，不能让一轮永远挂着）。
pub const SECRET_TIMEOUT: Duration = Duration::from_secs(120);

/// 轮询「本轮是否被取消」的间隔。
const CANCEL_POLL: Duration = Duration::from_millis(200);

/// 写进 pty 之后，等多久再取输出尾部。比按键长：`sudo` / `ssh` 校验密码要一两秒才有反应。
pub const SECRET_SETTLE: Duration = Duration::from_millis(1_000);

/// 密码的字节数上限。口令、令牌、短私钥口令都远小于它；超了多半是粘错了东西。
pub const MAX_SECRET_BYTES: usize = 1024;

/// `reason` 给人看的长度上限（字符）。它出现在卡片上，不该是一整段话。
const MAX_REASON_CHARS: usize = 200;

/// 掩掉回显用的占位。
const MASK: &str = "[已隐藏]";

/// 用户对一次密码请求的答复。**刻意不 derive 任何会把值带出去的 trait。**
pub enum SecretAnswer {
    /// 用户提供了密码（已校验过，不含换行 / 控制字符）。
    Provided(Zeroizing<String>),
    /// 用户点了「跳过」。
    Declined,
}

impl fmt::Debug for SecretAnswer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Provided(_) => f.write_str("Provided(<redacted>)"),
            Self::Declined => f.write_str("Declined"),
        }
    }
}

/// [`await_secret`] 的结果。同样不 derive `Clone` / `Serialize`；要往外说的只有 [`SecretStatus`]。
pub enum SecretOutcome {
    Provided(Zeroizing<String>),
    Declined,
    TimedOut,
    Cancelled,
}

impl fmt::Debug for SecretOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Provided(_) => "Provided(<redacted>)",
            Self::Declined => "Declined",
            Self::TimedOut => "TimedOut",
            Self::Cancelled => "Cancelled",
        })
    }
}

/// 一次密码请求最终的「可以对外说」的状态：不含任何秘密，trace / 卡片 / 反馈都用它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretStatus {
    /// 用户提供了，也已写进终端
    Provided,
    /// 用户点了跳过
    Declined,
    /// 超时没人输入
    TimedOut,
    /// 用户中途停止了本轮
    Cancelled,
    /// 用户提供了，但写进终端失败（原因不含秘密）
    WriteFailed(String),
}

impl SecretStatus {
    /// 前端卡片 / 步骤卡按它着色的稳定标识。
    pub fn code(&self) -> &'static str {
        match self {
            Self::Provided => "provided",
            Self::Declined => "declined",
            Self::TimedOut => "timeout",
            Self::Cancelled => "cancelled",
            Self::WriteFailed(_) => "failed",
        }
    }

    /// 给人看的一句话（步骤卡文案）。
    pub fn label(&self) -> String {
        match self {
            Self::Provided => "用户已输入，已写入终端".into(),
            Self::Declined => "用户选择跳过，没有输入".into(),
            Self::TimedOut => "等待超时，用户没有输入".into(),
            Self::Cancelled => "已取消".into(),
            Self::WriteFailed(why) => format!("写入终端失败：{why}"),
        }
    }
}

impl SecretOutcome {
    /// 没有写入过（或还没写入）时对应的状态；`Provided` 在这里先记成 `Provided`，
    /// 写入失败由调用方改成 `WriteFailed`。
    pub fn status(&self) -> SecretStatus {
        match self {
            Self::Provided(_) => SecretStatus::Provided,
            Self::Declined => SecretStatus::Declined,
            Self::TimedOut => SecretStatus::TimedOut,
            Self::Cancelled => SecretStatus::Cancelled,
        }
    }
}

/// 校验并整理前端交来的密码。
///
/// - 去掉末尾的 `\r` / `\n`（粘贴常带）；
/// - 中间不许有换行 / 其它控制字符：它们会在目标终端里被当成「提交」或转义序列，
///   密码被截成两半，后半段就成了一条命令；
/// - 长度有上限（[`MAX_SECRET_BYTES`]）。
///
/// 错误文案会原样回给前端展示，**绝不包含密码本身**。
pub fn sanitize_secret(raw: String) -> Result<Zeroizing<String>, &'static str> {
    // 先装进 Zeroizing：之后无论走哪条分支，这份内存都会在 drop 时被擦掉
    let mut v = Zeroizing::new(raw);
    let kept = v.trim_end_matches(['\r', '\n']).len();
    v.truncate(kept);
    if v.len() > MAX_SECRET_BYTES {
        return Err("输入过长");
    }
    if v.chars().any(|c| c.is_control()) {
        return Err("输入里含有换行或控制字符，无法安全地发送到终端");
    }
    Ok(v)
}

/// 要写进 pty 的字节：`value + "\r"`。返回值同样会在 drop 时擦掉。
pub fn secret_payload(value: &str) -> Zeroizing<Vec<u8>> {
    let mut bytes = Zeroizing::new(Vec::with_capacity(value.len() + 1));
    bytes.extend_from_slice(value.as_bytes());
    bytes.push(b'\r');
    bytes
}

/// 等用户的答复：答复到了 / 超时 / 本轮被取消，哪个先来算哪个。
///
/// 通道的发送端被丢掉（登记被清、窗口没了）按「跳过」处理——没人再能回答了，
/// 与审批「UI 那一头先没了 = 拒绝」同一思路。
pub async fn await_secret(
    rx: oneshot::Receiver<SecretAnswer>,
    timeout: Duration,
    cancelled: impl FnMut() -> bool,
) -> SecretOutcome {
    await_secret_for(rx, Some(timeout), cancelled).await
}

/// 同 [`await_secret`]，`timeout = None` 表示一直等（托管：卡片永远等下去，只有取消能打断）。
pub async fn await_secret_for(
    mut rx: oneshot::Receiver<SecretAnswer>,
    timeout: Option<Duration>,
    mut cancelled: impl FnMut() -> bool,
) -> SecretOutcome {
    let deadline = timeout.map(|t| Instant::now() + t);
    loop {
        if cancelled() {
            return SecretOutcome::Cancelled;
        }
        let now = Instant::now();
        let slice = match deadline {
            Some(d) if now >= d => return SecretOutcome::TimedOut,
            Some(d) => CANCEL_POLL.min(d - now),
            None => CANCEL_POLL,
        };
        tokio::select! {
            got = &mut rx => {
                return match got {
                    Ok(SecretAnswer::Provided(v)) => SecretOutcome::Provided(v),
                    Ok(SecretAnswer::Declined) | Err(_) => SecretOutcome::Declined,
                };
            }
            _ = sleep(slice) => {}
        }
    }
}

/// 把等到的答复落实：`Provided` 才会写终端，其余结局什么都不写。
///
/// 返回 (对外状态, 写入后的输出尾部)。尾部已经过 [`mask_secret`]，没写入时为空串。
///
/// - `write`：收下 `value + "\r"`（用完即擦的 `Zeroizing`）把它写进 pty。注入进来是为了测试，
///   也让本函数不依赖 `AppState`。
/// - `tail`：写入**之后**取新输出，由调用方在写入**之前**记好起点（`pushed` 的 mark）。
///
/// 注意：这里刻意**不**把密码喂进旁路缓冲（`StreamKind::In` 会给它记一条 `$ ` 转录）。
pub async fn deliver<W, WF, T>(
    outcome: SecretOutcome,
    write: W,
    settle: Duration,
    tail: T,
) -> (SecretStatus, String)
where
    W: FnOnce(Zeroizing<Vec<u8>>) -> WF,
    WF: std::future::Future<Output = Result<(), String>>,
    T: FnOnce() -> String,
{
    match outcome {
        SecretOutcome::Provided(value) => {
            match write(secret_payload(&value)).await {
                Ok(()) => {
                    sleep(settle).await;
                    (SecretStatus::Provided, mask_secret(&tail(), &value))
                }
                // 错误文案来自会话层（「会话不存在」「写入超时」），不含写入内容；
                // 仍过一遍掩码，宁可多涂不可漏。
                Err(e) => (SecretStatus::WriteFailed(mask_secret(&e, &value)), String::new()),
            }
        }
        other => (other.status(), String::new()),
    }
}

/// 写入前复核时，终端已经不在口令提示上的说明（不含任何秘密）。
pub const PROMPT_GONE: &str =
    "终端已经不在密码 / 口令提示上（可能用户自己输入了、提示超时退出了，或屏幕切到了别的程序），为安全起见没有写入";

/// 写入前的最后一道复核：用户填卡片可能花了两分钟，这期间口令提示可能已经没了——
/// 此时写进去的密码会被回显，甚至被 shell 当成一条命令执行（进历史记录）。
///
/// `still_prompting` 只在用户真的提供了密码时才调用。不在提示上时返回 `Err(WriteFailed)`，
/// `Zeroizing` 的值随 `outcome` 一起在这里 drop 擦掉；其余结局原样放行（它们本来就不写）。
pub fn guard_prompt(
    outcome: SecretOutcome,
    still_prompting: impl FnOnce() -> bool,
) -> Result<SecretOutcome, SecretStatus> {
    match outcome {
        SecretOutcome::Provided(value) => {
            if still_prompting() {
                Ok(SecretOutcome::Provided(value))
            } else {
                drop(value);
                Err(SecretStatus::WriteFailed(PROMPT_GONE.to_string()))
            }
        }
        other => Ok(other),
    }
}

/// 把输出里出现的密码原文替换掉。
///
/// 正常的密码提示不回显，这是兜底：目标若其实在回显（提示没被识别成密码、`stty echo` 开着、
/// 输错了地方），写入之后读到的输出尾部里就有密码，而这段尾部是要回给模型 / 展示在卡片上的。
/// 宁可把短密码引起的无关字符一并涂掉，也不能漏。
pub fn mask_secret(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        return text.to_string();
    }
    text.replace(secret, MASK)
}

/// 清洗 `reason`：模型写的，要给人看。去首尾空白、压成一行、限长。
pub fn clean_reason(raw: Option<&str>) -> String {
    let one_line: String = raw
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if one_line.is_empty() {
        return "终端正在请求输入密码".to_string();
    }
    if one_line.chars().count() > MAX_REASON_CHARS {
        let head: String = one_line.chars().take(MAX_REASON_CHARS).collect();
        return format!("{head}…");
    }
    one_line
}

/// 喂回模型的反馈。**只含状态与（已脱敏的）输出尾部**，不含也不可能含密码。
///
/// `tail`：写入之后的输出尾部（调用方已经 `mask_secret` 过）；`guess`：对终端此刻的分类。
pub fn secret_feedback(
    label: &str,
    status: &SecretStatus,
    tail: &str,
    guess: Option<&PromptGuess>,
) -> String {
    match status {
        SecretStatus::Provided => {
            let tail = tail.trim();
            let mut s = format!(
                "已向会话「{label}」提交用户提供的密码（你看不到它，也不需要看到）。\n\
                 约 1 秒后的新输出：\n{}\n",
                if tail.is_empty() { "（暂无新输出）" } else { tail }
            );
            if let Some(g) = guess {
                s.push_str(&g.feedback());
            }
            s.push_str(
                "若输出显示认证失败 / 又一次要求密码，不要自己重试输入：请用 nx_secret 再请用户输入一次，\
                 或在结论里告诉用户。\n",
            );
            s
        }
        SecretStatus::Declined => format!(
            "用户拒绝提供会话「{label}」所需的密码，什么都没有写入终端。\
             不要再次索要同一个密码，也不要尝试别的办法绕过：请在结论里说明这一步需要用户自己完成。\n"
        ),
        SecretStatus::TimedOut => format!(
            "等待用户为会话「{label}」输入密码超时（{} 秒内没有输入），什么都没有写入终端。\
             不要重复索要；请在结论里说明卡在需要密码这一步，由用户决定是否继续。\n",
            SECRET_TIMEOUT.as_secs()
        ),
        SecretStatus::Cancelled => {
            format!("等待用户为会话「{label}」输入密码时本轮已被取消，什么都没有写入终端。\n")
        }
        SecretStatus::WriteFailed(why) => format!(
            "用户已提供密码，但写入会话「{label}」失败：{why}。密码没有送达，也没有保留。\
             请据此决定：重新请求一次，或在结论里告诉用户。\n"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::prompt::{classify, PromptKind};

    const SECRET: &str = "Hunter2-sUp3r!pw";

    fn provided(v: &str) -> SecretAnswer {
        SecretAnswer::Provided(Zeroizing::new(v.to_string()))
    }

    // ——— 等待：提供 / 拒绝 / 超时 / 取消 / 发送端丢失 ———

    #[tokio::test(start_paused = true)]
    async fn provided_answer_comes_through() {
        let (tx, rx) = oneshot::channel();
        tx.send(provided(SECRET)).unwrap();
        match await_secret(rx, SECRET_TIMEOUT, || false).await {
            SecretOutcome::Provided(v) => assert_eq!(v.as_str(), SECRET),
            other => panic!("应当是 Provided，实际 {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn declined_answer_is_declined() {
        let (tx, rx) = oneshot::channel();
        tx.send(SecretAnswer::Declined).unwrap();
        let out = await_secret(rx, SECRET_TIMEOUT, || false).await;
        assert!(matches!(out, SecretOutcome::Declined));
    }

    #[tokio::test(start_paused = true)]
    async fn no_answer_times_out() {
        let (_tx, rx) = oneshot::channel::<SecretAnswer>();
        let started = Instant::now();
        let out = await_secret(rx, Duration::from_secs(5), || false).await;
        assert!(matches!(out, SecretOutcome::TimedOut));
        assert!(started.elapsed() >= Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_secs(6));
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_flag_ends_the_wait_promptly() {
        let (_tx, rx) = oneshot::channel::<SecretAnswer>();
        let started = Instant::now();
        let polls = std::cell::Cell::new(0u32);
        let out = await_secret(rx, SECRET_TIMEOUT, || {
            polls.set(polls.get() + 1);
            polls.get() > 3
        })
        .await;
        assert!(matches!(out, SecretOutcome::Cancelled));
        assert!(started.elapsed() < Duration::from_secs(5), "取消应当很快生效");
    }

    #[tokio::test(start_paused = true)]
    async fn dropped_sender_counts_as_declined() {
        let (tx, rx) = oneshot::channel::<SecretAnswer>();
        drop(tx);
        let out = await_secret(rx, SECRET_TIMEOUT, || false).await;
        assert!(matches!(out, SecretOutcome::Declined));
    }

    // ——— 校验 ———

    #[test]
    fn sanitize_strips_trailing_newline_only() {
        assert_eq!(sanitize_secret("abc123\r\n".into()).unwrap().as_str(), "abc123");
        assert_eq!(sanitize_secret("abc123\n".into()).unwrap().as_str(), "abc123");
        // 空格是密码的一部分，不能 trim
        assert_eq!(sanitize_secret(" pass word ".into()).unwrap().as_str(), " pass word ");
    }

    #[test]
    fn sanitize_rejects_control_chars_and_overlong() {
        assert!(sanitize_secret("ab\ncd".into()).is_err());
        assert!(sanitize_secret("ab\rcd".into()).is_err());
        assert!(sanitize_secret("ab\x1bcd".into()).is_err());
        assert!(sanitize_secret("ab\x03cd".into()).is_err());
        assert!(sanitize_secret("x".repeat(MAX_SECRET_BYTES + 1)).is_err());
        assert!(sanitize_secret("x".repeat(MAX_SECRET_BYTES)).is_ok());
        // 非 ASCII 的合法口令
        assert!(sanitize_secret("密码🔑pass".into()).is_ok());
    }

    #[test]
    fn sanitize_errors_never_contain_the_value() {
        for bad in [format!("{SECRET}\nrm -rf /"), format!("{SECRET}\x1b[31m")] {
            let e = sanitize_secret(bad).unwrap_err();
            assert!(!e.contains(SECRET));
        }
    }

    #[test]
    fn payload_is_value_plus_carriage_return() {
        assert_eq!(secret_payload("pw").as_slice(), b"pw\r");
        assert_eq!(secret_payload("").as_slice(), b"\r");
    }

    // ——— 不外泄：Debug / 反馈 / 掩码 ———

    #[test]
    fn debug_output_never_contains_the_value() {
        let a = provided(SECRET);
        assert!(!format!("{a:?}").contains(SECRET));
        let o = SecretOutcome::Provided(Zeroizing::new(SECRET.to_string()));
        assert!(!format!("{o:?}").contains(SECRET));
        // 套进容器里也一样
        assert!(!format!("{:?}", Some(&o)).contains(SECRET));
        assert!(!format!("{:?}", vec![provided(SECRET)]).contains(SECRET));
    }

    #[test]
    fn status_carries_no_secret_and_has_stable_codes() {
        let o = SecretOutcome::Provided(Zeroizing::new(SECRET.to_string()));
        let st = o.status();
        assert_eq!(st, SecretStatus::Provided);
        assert_eq!(st.code(), "provided");
        assert!(!format!("{st:?}").contains(SECRET));
        assert!(!st.label().contains(SECRET));
        assert_eq!(SecretStatus::Declined.code(), "declined");
        assert_eq!(SecretStatus::TimedOut.code(), "timeout");
        assert_eq!(SecretStatus::Cancelled.code(), "cancelled");
        assert_eq!(SecretStatus::WriteFailed("x".into()).code(), "failed");
    }

    #[test]
    fn feedback_for_every_status_has_no_secret_and_says_what_happened() {
        let g = classify("[sudo] password for chris:", false);
        for st in [
            SecretStatus::Provided,
            SecretStatus::Declined,
            SecretStatus::TimedOut,
            SecretStatus::Cancelled,
            SecretStatus::WriteFailed("通道已关闭".into()),
        ] {
            let fb = secret_feedback("web-01", &st, "Sorry, try again.", Some(&g));
            assert!(!fb.contains(SECRET), "{st:?}");
            assert!(fb.contains("web-01"));
        }
        assert!(secret_feedback("a", &SecretStatus::Declined, "", None).contains("拒绝"));
        assert!(secret_feedback("a", &SecretStatus::TimedOut, "", None).contains("超时"));
        assert!(secret_feedback("a", &SecretStatus::Cancelled, "", None).contains("取消"));
        // 提供成功：带上输出尾部与分类
        let ok = secret_feedback("a", &SecretStatus::Provided, "Sorry, try again.", Some(&g));
        assert!(ok.contains("Sorry, try again."));
        assert!(ok.contains("prompt_kind=password"));
    }

    /// 目标其实在回显时，写入之后读到的尾部里就有密码；掩码之后再进反馈，反馈里不能有它。
    #[test]
    fn echoed_secret_is_masked_before_it_reaches_the_feedback() {
        let raw_tail = format!("Password: {SECRET}\nSorry, try again.\nPassword:");
        let masked = mask_secret(&raw_tail, SECRET);
        assert!(!masked.contains(SECRET));
        assert!(masked.contains(MASK));
        let fb = secret_feedback("a", &SecretStatus::Provided, &masked, None);
        assert!(!fb.contains(SECRET));
    }

    #[test]
    fn mask_handles_repeats_and_empty_secret() {
        assert_eq!(mask_secret("aXbXc", "X"), format!("a{MASK}b{MASK}c"));
        assert_eq!(mask_secret("unchanged", ""), "unchanged");
        assert_eq!(mask_secret("", "x"), "");
    }

    #[test]
    fn reason_is_cleaned_for_display() {
        assert_eq!(clean_reason(None), "终端正在请求输入密码");
        assert_eq!(clean_reason(Some("   \n ")), "终端正在请求输入密码");
        assert_eq!(clean_reason(Some("  sudo 需要\n密码  ")), "sudo 需要 密码");
        let long = "字".repeat(MAX_REASON_CHARS + 50);
        let c = clean_reason(Some(&long));
        assert_eq!(c.chars().count(), MAX_REASON_CHARS + 1);
        assert!(c.ends_with('…'));
    }

    // ——— deliver：只有 Provided 才写；写的是 value + \r；回显被掩 ———

    use std::sync::{Arc, Mutex as StdMutex};

    fn recorder() -> (Arc<StdMutex<Vec<Vec<u8>>>>, impl Fn(Zeroizing<Vec<u8>>) -> std::future::Ready<Result<(), String>>) {
        let log: Arc<StdMutex<Vec<Vec<u8>>>> = Arc::new(StdMutex::new(Vec::new()));
        let l = Arc::clone(&log);
        (log, move |p: Zeroizing<Vec<u8>>| {
            l.lock().unwrap().push(p.to_vec());
            std::future::ready(Ok(()))
        })
    }

    #[tokio::test(start_paused = true)]
    async fn deliver_writes_value_plus_cr_and_returns_masked_tail() {
        let (log, w) = recorder();
        let out = SecretOutcome::Provided(Zeroizing::new(SECRET.to_string()));
        let (st, tail) = deliver(
            out,
            w,
            SECRET_SETTLE,
            || format!("{SECRET}\nSorry, try again.\nPassword:"),
        )
        .await;
        assert_eq!(st, SecretStatus::Provided);
        assert_eq!(log.lock().unwrap().as_slice(), &[format!("{SECRET}\r").into_bytes()]);
        assert!(!tail.contains(SECRET));
        assert!(tail.contains("Sorry, try again."));
    }

    #[tokio::test(start_paused = true)]
    async fn deliver_never_writes_unless_provided() {
        for out in [
            SecretOutcome::Declined,
            SecretOutcome::TimedOut,
            SecretOutcome::Cancelled,
        ] {
            let want = out.status();
            let (log, w) = recorder();
            let (st, tail) = deliver(out, w, SECRET_SETTLE, || "x".to_string()).await;
            assert_eq!(st, want);
            assert!(tail.is_empty());
            assert!(log.lock().unwrap().is_empty(), "{st:?} 时不许写终端");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn deliver_reports_write_failure_without_leaking() {
        let out = SecretOutcome::Provided(Zeroizing::new(SECRET.to_string()));
        let (st, tail) = deliver(
            out,
            |_p| std::future::ready(Err(format!("会话已关闭（{SECRET}）"))),
            SECRET_SETTLE,
            || panic!("写入失败时不应再取输出"),
        )
        .await;
        assert!(matches!(st, SecretStatus::WriteFailed(_)));
        assert!(!st.label().contains(SECRET));
        assert!(!format!("{st:?}").contains(SECRET));
        assert!(tail.is_empty());
    }

    #[test]
    fn prompt_kind_is_part_of_the_feedback_line() {
        let g = classify("[sudo] password for chris:", false);
        assert_eq!(g.kind, PromptKind::Password);
        let fb = secret_feedback("a", &SecretStatus::Provided, "", Some(&g));
        assert!(fb.contains("prompt_kind=password"));
    }

    #[test]
    fn guard_prompt_drops_value_when_prompt_is_gone() {
        // 提示还在：原样放行
        match guard_prompt(SecretOutcome::Provided(Zeroizing::new(SECRET.into())), || true) {
            Ok(SecretOutcome::Provided(v)) => assert_eq!(v.as_str(), SECRET),
            other => panic!("应当放行，实际 {other:?}"),
        }
        // 提示没了：不写，状态里没有密码
        match guard_prompt(SecretOutcome::Provided(Zeroizing::new(SECRET.into())), || false) {
            Err(st) => {
                assert_eq!(st.code(), "failed");
                assert!(!st.label().contains(SECRET));
                assert!(secret_feedback("s", &st, "", None).contains("没有写入"));
            }
            Ok(other) => panic!("应当拒绝写入，实际 {other:?}"),
        }
        // 用户没提供：不必复核屏幕
        let mut asked = false;
        let r = guard_prompt(SecretOutcome::Declined, || {
            asked = true;
            false
        });
        assert!(matches!(r, Ok(SecretOutcome::Declined)));
        assert!(!asked);
    }
}
