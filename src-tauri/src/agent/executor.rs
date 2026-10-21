//! Agent 代为执行命令：在目标 session 的 pty 上跑一条命令，并可靠地判定它何时结束。
//!
//! ## 为什么需要「哨兵」
//!
//! 远程 SSH / tmux 下终端只是一条字节流，**没有任何信号**告诉你「这条命令结束了」。
//! 于是我们不直接发 `cmd\n`，而是发：
//!
//! ```text
//! cmd; printf '\n__NX_END_<nonce>__%s\n' "$?"
//! ```
//!
//! `cmd` 跑完后 shell 会打印出一行 `__NX_END_<nonce>__<exit_code>`。后端在旁路缓冲里
//! 一旦扫到这行，就知道：① 命令结束了 ② 退出码是多少 ③ 这行之前的新增输出就是命令结果。
//! nonce 每次随机，避免和真实输出撞车。
//!
//! ## 兜底：输出静默
//!
//! 万一哨兵没能打印出来（命令把 shell 卡死、进入了交互式程序、连接断了），
//! 就靠「输出静默超过 `SILENCE_MS` 毫秒」兜底结束，退出码记为未知。再加一个总超时
//! `MAX_WAIT_MS`，绝不无限等待。
//!
//! ## 边界与安全
//!
//! - 本模块只负责「写入 + 判定结束 + 截取输出」，**不决定要不要执行**（审批在上层）。
//! - 命令里的换行会被拒绝（多行命令必须由模型拆成多步），避免一次注入一串危险操作。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// 判定命令结束前，输出静默多久算「跑完了」（哨兵缺失时的兜底）。
const SILENCE_MS: u64 = 1500;
/// 单条命令的总等待上限。超过就强制结束，避免 `top` / `tail -f` 这类永不返回的命令卡死循环。
const MAX_WAIT_MS: u64 = 120_000;
/// 轮询缓冲的间隔。
const POLL_MS: u64 = 80;

/// 生成哨兵用的单调递增计数，叠加随机数，进一步降低碰撞概率。
static SEQ: AtomicU64 = AtomicU64::new(0);

/// 一条命令执行的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutcome {
    /// 命令在哨兵之前产生的输出（已由缓冲剥离 ANSI）。
    pub output: String,
    /// 退出码。`None` = 没等到哨兵（靠静默/超时结束），退出码不可知。
    pub exit_code: Option<i32>,
    /// 为何结束。
    pub reason: ExecEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecEnd {
    /// 扫到了哨兵，正常结束
    Sentinel,
    /// 输出静默超时，按结束处理（退出码未知）
    Silence,
    /// 命中总超时上限
    Timeout,
    /// 等待期间被取消
    Cancelled,
}

/// 一个哨兵：随机串 + 用于拼命令、匹配输出。
pub struct Sentinel {
    token: String,
}

impl Sentinel {
    pub fn new() -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        // 用 uuid（已是依赖）取随机部分，避免再引入 rand
        let rand = uuid::Uuid::new_v4().simple().to_string();
        Self {
            token: format!("__NX_END_{seq:x}_{rand}__"),
        }
    }

    /// 把一条命令包成「命令 + 哨兵」。
    ///
    /// 用 `printf` 而不是 `echo`：`echo` 对 `\n` 的处理在不同 shell 下不一致，
    /// `printf` 到处都稳。前面补一个换行，保证哨兵一定独占一行（命令若无末尾换行也不粘连）。
    pub fn wrap(&self, cmd: &str) -> String {
        format!("{cmd}; printf '\\n{}%s\\n' \"$?\"\n", self.token)
    }

    /// 在一段文本里找哨兵行，返回（哨兵之前的输出, 退出码）。
    ///
    /// 退出码解析失败（兜底 shell 不支持 `$?`、或哨兵被截断）时退出码记 `None`。
    pub fn scan(&self, text: &str) -> Option<(String, Option<i32>)> {
        let idx = text.find(&self.token)?;
        let before = text[..idx].to_string();
        // 哨兵之后紧跟的是退出码数字，取到行尾
        let after = &text[idx + self.token.len()..];
        let code_str: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        let code = code_str.parse::<i32>().ok();
        Some((trim_sentinel_tail(&before), code))
    }
}

impl Default for Sentinel {
    fn default() -> Self {
        Self::new()
    }
}

/// 截掉命令输出尾部可能残留的哨兵命令回显。
///
/// 有些 shell 会把我们注入的整条命令（含 `printf '…'`）回显出来，落在输出里很难看。
/// 这里只做保守清理：去掉结尾的空白。更激进的回显抵消已经由 buffer 的 pending_echo 做了。
fn trim_sentinel_tail(s: &str) -> String {
    s.trim_end().to_string()
}

/// 等待命令结束的配置（便于测试注入更短的时限）。
#[derive(Clone, Copy)]
pub struct WaitConfig {
    pub silence_ms: u64,
    pub max_wait_ms: u64,
    pub poll_ms: u64,
}

impl Default for WaitConfig {
    fn default() -> Self {
        Self {
            silence_ms: SILENCE_MS,
            max_wait_ms: MAX_WAIT_MS,
            poll_ms: POLL_MS,
        }
    }
}

/// 轮询式等待命令结束。
///
/// `snapshot()` 每次返回「命令写下去之后、目标 session 新增的那段文本」。
/// 调用方负责在写命令前记录基线，并在这里传入一个能取到「基线之后新增输出」的闭包。
/// `cancelled()` 返回 true 时立即以 `Cancelled` 收场。
///
/// 纯逻辑、不碰 Tauri，便于单测；真正的 sleep 由 `sleep` 闭包提供（测试里可换成即时返回）。
pub async fn wait_for_end<S, C, F>(
    sentinel: &Sentinel,
    cfg: WaitConfig,
    mut snapshot: S,
    mut cancelled: C,
    mut sleep: F,
) -> ExecOutcome
where
    S: FnMut() -> String,
    C: FnMut() -> bool,
    F: FnMut(Duration) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
{
    let mut waited: u64 = 0;
    let mut last_len: usize = snapshot().len();
    let mut silent: u64 = 0;

    loop {
        if cancelled() {
            let text = snapshot();
            return ExecOutcome {
                output: strip_tail_noise(&text),
                exit_code: None,
                reason: ExecEnd::Cancelled,
            };
        }

        let text = snapshot();
        if let Some((output, code)) = sentinel.scan(&text) {
            return ExecOutcome {
                output,
                exit_code: code,
                reason: ExecEnd::Sentinel,
            };
        }

        // 静默检测：输出长度不再增长就累加静默时间
        if text.len() == last_len {
            silent += cfg.poll_ms;
            if silent >= cfg.silence_ms {
                return ExecOutcome {
                    output: strip_tail_noise(&text),
                    exit_code: None,
                    reason: ExecEnd::Silence,
                };
            }
        } else {
            silent = 0;
            last_len = text.len();
        }

        waited += cfg.poll_ms;
        if waited >= cfg.max_wait_ms {
            return ExecOutcome {
                output: strip_tail_noise(&text),
                exit_code: None,
                reason: ExecEnd::Timeout,
            };
        }

        sleep(Duration::from_millis(cfg.poll_ms)).await;
    }
}

/// 去掉输出尾部可能残留的半截哨兵命令回显 / 多余空白。
fn strip_tail_noise(s: &str) -> String {
    s.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentinel_tokens_are_unique() {
        let a = Sentinel::new();
        let b = Sentinel::new();
        assert_ne!(a.token, b.token, "两次哨兵不该相同");
    }

    #[test]
    fn wrap_appends_printf_sentinel() {
        let s = Sentinel::new();
        let wrapped = s.wrap("ls -la");
        assert!(wrapped.starts_with("ls -la; printf "));
        assert!(wrapped.contains(&s.token));
        assert!(wrapped.ends_with('\n'));
    }

    #[test]
    fn scan_extracts_output_and_exit_code() {
        let s = Sentinel::new();
        let stream = format!("file1\nfile2\n{}0\n", s.token);
        let (out, code) = s.scan(&stream).expect("应扫到哨兵");
        assert_eq!(out, "file1\nfile2");
        assert_eq!(code, Some(0));
    }

    #[test]
    fn scan_handles_nonzero_exit_code() {
        let s = Sentinel::new();
        let stream = format!("command not found\n{}127\n", s.token);
        let (out, code) = s.scan(&stream).expect("应扫到哨兵");
        assert_eq!(out, "command not found");
        assert_eq!(code, Some(127));
    }

    #[test]
    fn scan_returns_none_without_sentinel() {
        let s = Sentinel::new();
        assert!(s.scan("just some output\n").is_none());
    }

    #[test]
    fn scan_exit_code_none_when_not_numeric() {
        let s = Sentinel::new();
        // 哨兵后面不是数字（某些兜底 shell 不展开 $?）
        let stream = format!("out\n{}$?\n", s.token);
        let (out, code) = s.scan(&stream).expect("应扫到哨兵");
        assert_eq!(out, "out");
        assert_eq!(code, None);
    }

    fn immediate_sleep(_: Duration) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(async {})
    }

    #[tokio::test]
    async fn wait_returns_on_sentinel() {
        let s = Sentinel::new();
        let full = format!("done\n{}0\n", s.token);
        let out = wait_for_end(
            &s,
            WaitConfig { silence_ms: 1000, max_wait_ms: 10_000, poll_ms: 10 },
            || full.clone(),
            || false,
            immediate_sleep,
        )
        .await;
        assert_eq!(out.reason, ExecEnd::Sentinel);
        assert_eq!(out.exit_code, Some(0));
        assert_eq!(out.output, "done");
    }

    #[tokio::test]
    async fn wait_falls_back_to_silence() {
        let s = Sentinel::new();
        // 永远没有哨兵，输出长度恒定 → 应当静默结束
        let out = wait_for_end(
            &s,
            WaitConfig { silence_ms: 30, max_wait_ms: 10_000, poll_ms: 10 },
            || "partial output".to_string(),
            || false,
            immediate_sleep,
        )
        .await;
        assert_eq!(out.reason, ExecEnd::Silence);
        assert_eq!(out.exit_code, None);
        assert_eq!(out.output, "partial output");
    }

    #[tokio::test]
    async fn wait_honors_cancellation() {
        let s = Sentinel::new();
        let out = wait_for_end(
            &s,
            WaitConfig::default(),
            || "running".to_string(),
            || true, // 立即取消
            immediate_sleep,
        )
        .await;
        assert_eq!(out.reason, ExecEnd::Cancelled);
    }

    #[tokio::test]
    async fn wait_hits_timeout_when_output_keeps_growing() {
        let s = Sentinel::new();
        let mut n = 0usize;
        let out = wait_for_end(
            &s,
            WaitConfig { silence_ms: 10_000, max_wait_ms: 50, poll_ms: 10 },
            || {
                // 每次都更长 → 永不静默，只能撞总超时
                n += 1;
                "x".repeat(n)
            },
            || false,
            immediate_sleep,
        )
        .await;
        assert_eq!(out.reason, ExecEnd::Timeout);
    }
}
