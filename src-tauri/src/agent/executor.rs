//! Agent 代为执行命令：在目标 session 的 pty 上跑一条命令，并可靠地判定它何时结束。
//!
//! ## 判定思路：静默优先，活体确认兜底
//!
//! 以前靠拼 `cmd; printf '\n__NX_END_<nonce>__%s\n' "$?"` 哨兵判定结束并拿退出码，
//! 但哨兵必须和命令打在同一行，会原样回显进用户正在看的真实终端，洗不掉。
//!
//! 现在**完全不往命令里拼任何东西**，只发 `cmd\n`。判定分两步：
//!
//! 1. **静默**：输出连续 `silence_ms` 毫秒没有新增，视为「大概率跑完了」。
//! 2. **活体确认**：静默只是「大概率」，不是「确定」——网络慢的命令偶尔也会有输出间隙。
//!    于是在宣布结束前，去问一句「这个会话的 shell 现在还有没有前台子进程」：
//!    - 本地会话：直接查本机的 shell 进程有没有活着的子进程（`pgrep -P` / Windows 对应物）。
//!    - SSH 会话：复用已有的 [`crate::ssh::session::SessionManager::probe`]——它本来就是
//!      给「控制台自动补全」探测前台进程用的，`FG_COMM` 有值说明还有前台命令在跑。
//!    确认「没有」才真正收尾；确认「还有」就继续等，但把下一次确认的间隔拉长
//!    （`liveness_interval_ms`），避免对着 SSH 连接反复开旁路通道。
//!    **拿不到确定答案**（旁路通道被堡垒机拒绝、探测脚本在非 Linux 远端找不到 shell、
//!    本机没有 `pgrep`/`wmic`）时，不阻塞也不报错，直接退回「静默即算完」——
//!    等同于以前哨兵缺失时的兜底，只是现在是唯一的兜底，不是极端情况。
//!
//! 退出码因此拿不到，是刻意取舍：它从来只是展示用的参考信息，没有控制流依赖它，
//! 系统提示词本就教模型照真实输出判断成败。
//!
//! 本模块只负责「写入 + 判定结束 + 截取输出」，**不决定要不要执行**（审批在上层）。

use std::time::Duration;

/// 判定命令结束前，输出静默多久算「大概率跑完了」（触发活体确认的阈值）。
const SILENCE_MS: u64 = 1500;
/// 单条命令的总等待上限。超过就强制结束，避免 `top` / `tail -f` 这类永不返回的命令卡死循环。
const MAX_WAIT_MS: u64 = 120_000;
/// 轮询缓冲的间隔。
const POLL_MS: u64 = 80;
/// 两次「活体确认」之间的最短间隔——确认一次「还在跑」之后，不必每个 `POLL_MS`
/// 都去问一遍，SSH 场景下那意味着反复开旁路通道，堡垒机式的目标会被打烦。
const LIVENESS_INTERVAL_MS: u64 = 1_500;

/// 一条命令执行的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutcome {
    /// 命令产生的输出（已由缓冲剥离 ANSI）。
    pub output: String,
    /// 为何结束。
    pub reason: ExecEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecEnd {
    /// 静默触发后，活体确认明确给出「没有前台子进程了」——最可信的收尾
    Confirmed,
    /// 静默触发，但活体确认拿不到确定答案（通道不可用 / 探测失败），按完成处理
    Silence,
    /// 命中总超时上限
    Timeout,
    /// 等待期间被取消
    Cancelled,
}

/// 一次「活体确认」的结果：目标 shell 现在是否还有前台子进程在跑。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// 确认还有前台子进程——命令还没跑完
    Busy,
    /// 确认没有前台子进程——命令已经结束
    Idle,
    /// 问不出确定答案（通道/探测不可用），不能据此判断
    Unknown,
}

/// 等待命令结束的配置（便于测试注入更短的时限）。
#[derive(Clone, Copy)]
pub struct WaitConfig {
    pub silence_ms: u64,
    pub max_wait_ms: u64,
    pub poll_ms: u64,
    pub liveness_interval_ms: u64,
}

impl Default for WaitConfig {
    fn default() -> Self {
        Self {
            silence_ms: SILENCE_MS,
            max_wait_ms: MAX_WAIT_MS,
            poll_ms: POLL_MS,
            liveness_interval_ms: LIVENESS_INTERVAL_MS,
        }
    }
}

type BoxFut<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>;

/// 轮询式等待命令结束。
///
/// `snapshot()` 每次返回「命令写下去之后、目标 session 新增的那段文本」。
/// 调用方负责在写命令前记录基线，并在这里传入一个能取到「基线之后新增输出」的闭包。
/// `cancelled()` 返回 true 时立即以 `Cancelled` 收场。
/// `liveness()` 在静默触发、且距上次确认已经过了 `liveness_interval_ms` 时才会被调用，
/// 返回「还在跑 / 确认结束 / 问不出来」。
///
/// `liveness()` 返回的 future 允许借用调用方的局部变量（不要求 `'static`）——
/// 真正的实现（`ssh_liveness`/`local_liveness`，见 `act.rs`）都是借着 `&AppState`
/// 和 `&str` 去跑的，没有也不该有一份能活到 `'static` 的拷贝。
///
/// 纯逻辑、不碰 Tauri，便于单测；真正的 sleep 由 `sleep` 闭包提供（测试里可换成即时返回）。
pub async fn wait_for_end<'a, S, C, F, L>(
    cfg: WaitConfig,
    mut snapshot: S,
    mut cancelled: C,
    mut sleep: F,
    mut liveness: L,
) -> ExecOutcome
where
    S: FnMut() -> String,
    C: FnMut() -> bool,
    F: FnMut(Duration) -> BoxFut<()>,
    L: FnMut() -> std::pin::Pin<Box<dyn std::future::Future<Output = Liveness> + Send + 'a>>,
{
    let mut waited: u64 = 0;
    let mut last_len: usize = snapshot().len();
    let mut silent: u64 = 0;
    // 活体确认的冷却剩余；0 表示现在就可以确认。初值为 0，首次静默触发即可确认一次。
    let mut liveness_cooldown: u64 = 0;

    loop {
        if cancelled() {
            let text = snapshot();
            return ExecOutcome {
                output: strip_tail_noise(&text),
                reason: ExecEnd::Cancelled,
            };
        }

        let text = snapshot();
        if text.len() == last_len {
            silent += cfg.poll_ms;
        } else {
            // 有新输出：之前「还在跑」的确认已经过时，下次静默触发要立刻重新确认，
            // 不能沿用旧的冷却计时。
            silent = 0;
            last_len = text.len();
            liveness_cooldown = 0;
        }

        if liveness_cooldown > 0 {
            liveness_cooldown = liveness_cooldown.saturating_sub(cfg.poll_ms);
        }

        if silent >= cfg.silence_ms && liveness_cooldown == 0 {
            match liveness().await {
                Liveness::Idle => {
                    return ExecOutcome {
                        output: strip_tail_noise(&text),
                        reason: ExecEnd::Confirmed,
                    };
                }
                Liveness::Unknown => {
                    return ExecOutcome {
                        output: strip_tail_noise(&text),
                        reason: ExecEnd::Silence,
                    };
                }
                Liveness::Busy => {
                    liveness_cooldown = cfg.liveness_interval_ms;
                }
            }
        }

        waited += cfg.poll_ms;
        if waited >= cfg.max_wait_ms {
            return ExecOutcome {
                output: strip_tail_noise(&text),
                reason: ExecEnd::Timeout,
            };
        }

        sleep(Duration::from_millis(cfg.poll_ms)).await;
    }
}

/// 去掉输出尾部可能残留的多余空白。
fn strip_tail_noise(s: &str) -> String {
    s.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn immediate_sleep(_: Duration) -> BoxFut<()> {
        Box::pin(async {})
    }

    fn always(v: Liveness) -> impl FnMut() -> BoxFut<Liveness> {
        move || Box::pin(async move { v })
    }

    #[tokio::test]
    async fn silence_then_confirmed_idle_ends_run() {
        let out = wait_for_end(
            WaitConfig { silence_ms: 30, max_wait_ms: 10_000, poll_ms: 10, liveness_interval_ms: 1_000 },
            || "done".to_string(),
            || false,
            immediate_sleep,
            always(Liveness::Idle),
        )
        .await;
        assert_eq!(out.reason, ExecEnd::Confirmed);
        assert_eq!(out.output, "done");
    }

    #[tokio::test]
    async fn silence_then_unknown_liveness_falls_back_to_silence() {
        let out = wait_for_end(
            WaitConfig { silence_ms: 30, max_wait_ms: 10_000, poll_ms: 10, liveness_interval_ms: 1_000 },
            || "partial output".to_string(),
            || false,
            immediate_sleep,
            always(Liveness::Unknown),
        )
        .await;
        assert_eq!(out.reason, ExecEnd::Silence);
        assert_eq!(out.output, "partial output");
    }

    /// 活体确认说「还在跑」就不能提前收尾，哪怕已经静默很久——
    /// 要一路等到它说「确实没了」，或者撞上总超时。
    #[tokio::test]
    async fn busy_liveness_keeps_waiting_until_idle() {
        let mut calls = 0u32;
        let out = wait_for_end(
            WaitConfig { silence_ms: 10, max_wait_ms: 10_000, poll_ms: 10, liveness_interval_ms: 20 },
            || "still here".to_string(),
            || false,
            immediate_sleep,
            move || {
                calls += 1;
                // 前两次「还在跑」，第三次才「确认结束」
                let v = if calls < 3 { Liveness::Busy } else { Liveness::Idle };
                Box::pin(async move { v })
            },
        )
        .await;
        assert_eq!(out.reason, ExecEnd::Confirmed);
    }

    #[tokio::test]
    async fn busy_liveness_is_throttled_not_polled_every_tick() {
        // poll 间隔 5ms，确认间隔 50ms：静默长期保持，活体确认不该被打到几十次。
        let mut calls = 0u32;
        let _ = wait_for_end(
            WaitConfig { silence_ms: 5, max_wait_ms: 200, poll_ms: 5, liveness_interval_ms: 50 },
            || "x".to_string(),
            || false,
            immediate_sleep,
            move || {
                calls += 1;
                Box::pin(async move { Liveness::Busy })
            },
        )
        .await;
        // 总时长 200ms、确认间隔 50ms：最多确认 4~5 次，远少于 poll 次数（200/5=40）
        assert!(calls <= 6, "活体确认被调用了 {calls} 次，没有被节流");
    }

    #[tokio::test]
    async fn new_output_resets_silence_and_cooldown() {
        // 第一次 snapshot 后输出还在变化，不该触发 liveness；liveness 被调用即失败。
        let mut n = 0usize;
        let out = wait_for_end(
            WaitConfig { silence_ms: 20, max_wait_ms: 100, poll_ms: 10, liveness_interval_ms: 1_000 },
            move || {
                n += 1;
                // 前 3 次持续增长，之后才静默
                "x".repeat(n.min(3))
            },
            || false,
            immediate_sleep,
            always(Liveness::Idle),
        )
        .await;
        assert_eq!(out.reason, ExecEnd::Confirmed);
    }

    #[tokio::test]
    async fn wait_honors_cancellation() {
        let out = wait_for_end(
            WaitConfig::default(),
            || "running".to_string(),
            || true, // 立即取消
            immediate_sleep,
            always(Liveness::Busy),
        )
        .await;
        assert_eq!(out.reason, ExecEnd::Cancelled);
    }

    #[tokio::test]
    async fn wait_hits_timeout_when_output_keeps_growing() {
        let mut n = 0usize;
        let out = wait_for_end(
            WaitConfig { silence_ms: 10_000, max_wait_ms: 50, poll_ms: 10, liveness_interval_ms: 1_000 },
            move || {
                // 每次都更长 → 永不静默，只能撞总超时
                n += 1;
                "x".repeat(n)
            },
            || false,
            immediate_sleep,
            always(Liveness::Idle),
        )
        .await;
        assert_eq!(out.reason, ExecEnd::Timeout);
    }

    /// 活体确认一直说「还在跑」，但命令事实上失控（比如 `tail -f`）：
    /// 总超时必须是绝对兜底，不能被 Busy 判定无限拖住。
    #[tokio::test]
    async fn timeout_backstop_wins_even_if_always_busy() {
        let out = wait_for_end(
            WaitConfig { silence_ms: 5, max_wait_ms: 50, poll_ms: 10, liveness_interval_ms: 5 },
            || "still streaming".to_string(),
            || false,
            immediate_sleep,
            always(Liveness::Busy),
        )
        .await;
        assert_eq!(out.reason, ExecEnd::Timeout);
    }
}
