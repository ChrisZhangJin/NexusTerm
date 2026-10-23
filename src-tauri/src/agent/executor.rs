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
//! 上面的写法是 POSIX 的。Windows 本地会话跑的是 `%COMSPEC%`（cmd.exe），既没有
//! `printf` 也没有 `$?`，硬塞过去只会得到「'printf' 不是内部或外部命令」，哨兵永不出现。
//! 所以方言由 [`Shell`] 显式指定，cmd 下改发
//! `cmd & echo. & call echo __NX_END_…__%^ERRORLEVEL%`。
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

/// 哨兵的固定前缀。既要拼进命令、又要在事后从会话记录里认出来擦掉。
pub const SENTINEL_MARK: &str = "__NX_END_";

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

/// 目标 shell 的方言。
///
/// 哨兵是拼进用户那条命令尾部的，语法必须跟 shell 对上 —— 把 POSIX 的
/// `printf '…' "$?"` 塞进 Windows 的 cmd.exe，只会得到一句
/// 「'printf' 不是内部或外部命令」：哨兵永远不出现，命令只能靠「输出静默」兜底结束，
/// 退出码也拿不到。所以这里必须显式区分，不能默认全世界都是 sh。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    /// POSIX：sh / bash / zsh / dash，以及 SSH 到 Linux 的登录 shell
    Posix,
    /// Windows 交互式 cmd.exe（即 `%COMSPEC%`，也是本地会话的默认 shell）
    Cmd,
}

impl Shell {
    /// 本机默认交互 shell 的方言。
    ///
    /// 只对**本地**会话有意义：SSH 目标一律按 POSIX 处理（OpenSSH 服务端跑在
    /// Windows 上是极少数，真遇到也会走静默兜底而不是把命令写坏）。
    pub fn native() -> Self {
        if cfg!(windows) {
            Shell::Cmd
        } else {
            Shell::Posix
        }
    }
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
            token: format!("{SENTINEL_MARK}{seq:x}_{rand}__"),
        }
    }

    /// 按目标 shell 的方言把命令包成「命令 + 哨兵」。
    pub fn wrap_for(&self, cmd: &str, shell: Shell) -> String {
        match shell {
            // 用 `printf` 而不是 `echo`：`echo` 对 `\n` 的处理在不同 shell 下不一致，
            // `printf` 到处都稳。前面补一个换行，保证哨兵一定独占一行（命令若无末尾
            // 换行也不粘连）。
            Shell::Posix => format!("{cmd}; printf '\\n{}%s\\n' \"$?\"\n", self.token),
            // cmd.exe 里没有 printf，也没有 `$?`。
            //
            // `%ERRORLEVEL%` 的坑：它在**整行解析时**就展开，所以直接写
            // `dir & echo %ERRORLEVEL%` 拿到的是上一条命令遗留的旧值，永远是错的。
            // 这里用 `^` 把百分号转义一层，留给 `call` 的二次解析去取当前值。
            //
            // `echo.` 先打一个空行，作用同 POSIX 分支里那个 `\n`：保证哨兵独占一行。
            Shell::Cmd => format!("{cmd} & echo. & call echo {}%^ERRORLEVEL%\n", self.token),
        }
    }

    /// 在一段文本里找哨兵行，返回（哨兵之前的输出, 退出码）。
    ///
    /// 退出码解析失败（兜底 shell 不支持 `$?`、或哨兵被截断）时退出码记 `None`。
    pub fn scan(&self, text: &str) -> Option<(String, Option<i32>)> {
        let idx = self.find_at_line_start(text)?;
        let before = text[..idx].to_string();
        // 哨兵之后紧跟的是退出码数字，取到行尾
        let after = &text[idx + self.token.len()..];
        let code_str: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        let code = code_str.parse::<i32>().ok();
        Some((trim_sentinel_tail(&before), code))
    }

    /// 找出**位于行首**的那次 token 出现，返回它的字节下标。
    ///
    /// 为什么不直接 `text.find()`：命令回显那一行里也含 token
    /// （`dir & echo. & call echo __NX_END_…__%^ERRORLEVEL%`），
    /// 取到它就会把退出码读成 `%^ERRORLEVEL%` 这种非数字，
    /// 明明成功的命令也会被记成「退出码未知」。
    ///
    /// 真正由哨兵打印出来的那一行，token 一定在行首（POSIX 的 `printf '\n…'`
    /// 与 cmd 的 `echo.` 都保证了前面有一个换行）。
    fn find_at_line_start(&self, text: &str) -> Option<usize> {
        let mut from = 0usize;
        while let Some(rel) = text[from..].find(&self.token) {
            let abs = from + rel;
            if abs == 0 || text.as_bytes().get(abs - 1) == Some(&b'\n') {
                return Some(abs);
            }
            from = abs + self.token.len();
        }
        None
    }
}

impl Default for Sentinel {
    fn default() -> Self {
        Self::new()
    }
}

/// 打印出来就是那个一次性 token —— 便于拼接期望文本、也便于日志里对账。
impl std::fmt::Display for Sentinel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.token)
    }
}

/// 截掉命令输出尾部可能残留的哨兵命令回显。
///
/// 有些 shell 会把我们注入的整条命令（含 `printf '…'`）回显出来，落在输出里很难看。
/// 这里只做保守清理：去掉结尾的空白。更激进的回显抵消已经由 buffer 的 pending_echo 做了。
fn trim_sentinel_tail(s: &str) -> String {
    s.trim_end().to_string()
}

/// 把会话记录里**过去的**哨兵包装痕迹擦掉，让它读起来仍像一条人敲的命令。
///
/// 为什么要擦：终端会把我们注入的整串 `cmd; printf '\n__NX_END_…__%s\n' "$?"`
/// 原样回显出来，这些字节和其它输出一起进了旁路缓冲。下次把历史喂给模型时，
/// 它看到的就是一串看不懂的 printf —— 既浪费 token，也容易被模型当成
/// 「这条歪命令是我自己提的」而误学。
///
/// 只做保守的两步清理：整行丢弃哨兵输出行，命令行截到 `; printf` 之前。
/// 不做任何可能对正常命令造成损伤的模糊替换。
pub fn strip_sentinel_wrappers(text: &str) -> String {
    if !text.contains(SENTINEL_MARK) && !SENTINEL_TAILS.iter().any(|t| text.contains(t)) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        // ① 命令回显：它同时含 TAIL 与 MARK，必须先按 TAIL 切开再判别的，
        //    否则整行会被 ② 误丢，原本那条命令就跟着没了。
        //    一行只可能含一种方言的尾巴，取最早出现的那个切点即可。
        if let Some(i) = SENTINEL_TAILS.iter().filter_map(|t| line.find(t)).min() {
            let head = line[..i].trim_end();
            if !head.is_empty() {
                out.push_str(head);
                if line.ends_with('\n') {
                    out.push('\n');
                }
            }
            continue;
        }
        // ② 哨兵自己打印出来的那一行（`__NX_END_…__<退出码>`）
        if line.contains(SENTINEL_MARK) {
            continue;
        }
        out.push_str(line);
    }
    out
}

/// 我们追加到命令尾部的字面片段。**每支持一种 shell 方言都要在这里登记一行**，
/// 否则命令回显就擦不干净（回显里带着 token，只按 token 删会把整条命令一起吞掉）。
const SENTINEL_TAILS: &[&str] = &[
    "; printf '\\n",         // POSIX
    "& echo. & call echo ", // Windows cmd
];

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
        let wrapped = s.wrap_for("ls -la", Shell::Posix);
        assert!(wrapped.starts_with("ls -la; printf "));
        assert!(wrapped.contains(&s.token));
        assert!(wrapped.ends_with('\n'));
    }

    #[test]
    fn wrap_for_cmd_uses_call_and_escaped_errorlevel() {
        let s = Sentinel::new();
        let wrapped = s.wrap_for("dir", Shell::Cmd);
        assert!(wrapped.starts_with("dir & echo. & call echo "), "{wrapped}");
        assert!(wrapped.contains(&s.token));
        // `%^ERRORLEVEL%` 而不是 `%ERRORLEVEL%`：后者会在整行解析时拿到旧值
        assert!(wrapped.contains("%^ERRORLEVEL%"), "{wrapped}");
        // POSIX 的东西一个都不能漏进 cmd 命令行
        assert!(!wrapped.contains("printf"), "{wrapped}");
        assert!(!wrapped.contains("$?"), "{wrapped}");
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

    // —————————————— 记录的痕迹擦除 ——————————————

    #[test]
    fn echo_line_is_trimmed_back_to_plain_command() {
        let s = Sentinel::new();
        let wrapped = s.wrap_for("df -h", Shell::Posix);
        let transcript = format!("$ {wrapped}Filesystem\n");
        let clean = strip_sentinel_wrappers(&transcript);
        assert_eq!(clean, "$ df -h\nFilesystem\n");
        assert!(!clean.contains(SENTINEL_MARK), "哨兵不该留在记录里");
    }

    #[test]
    fn sentinel_output_line_is_dropped() {
        let s = Sentinel::new();
        let transcript = format!("/dev/disk1\n{s}0\n");
        let clean = strip_sentinel_wrappers(&transcript);
        assert_eq!(clean, "/dev/disk1\n");
    }

    // —————————————— Windows cmd 方言 ——————————————

    #[test]
    fn cmd_echo_line_is_trimmed_back_to_plain_command() {
        let s = Sentinel::new();
        let wrapped = s.wrap_for("dir", Shell::Cmd);
        // cmd 会把整行连同 `& echo. & call echo …%^ERRORLEVEL%` 一起回显
        let transcript = format!("C:\\Users\\me>{wrapped} 驱动器 C 中的卷是 OS\n{s}0\n");
        let clean = strip_sentinel_wrappers(&transcript);
        assert_eq!(clean, "C:\\Users\\me>dir\n 驱动器 C 中的卷是 OS\n");
        assert!(!clean.contains(SENTINEL_MARK));
        assert!(!clean.contains("call echo"));
    }

    #[tokio::test]
    async fn cmd_sentinel_line_yields_exit_code() {
        let s = Sentinel::new();
        let wrapped = s.wrap_for("dir", Shell::Cmd);
        // 真实回显 + `<DIR>` 输出 + `echo.` 打出的空行 + 哨兵行
        // 注意 wrapped 自己已经以换行结尾，夹具别再补一个，否则会多出空行
        let stream = format!("C:\\>{wrapped}<DIR>  docs\n\n{}1\n", s.token);
        let out = wait_for_end(
            &s,
            WaitConfig { silence_ms: 500, max_wait_ms: 2_000, poll_ms: 10 },
            || stream.clone(),
            || false,
            immediate_sleep,
        )
        .await;
        assert_eq!(out.reason, ExecEnd::Sentinel);
        assert_eq!(out.exit_code, Some(1));
        // 输出里还留着命令回显（剥哨兵是调用方最后一步的事）
        let shown = strip_sentinel_wrappers(&out.output);
        assert_eq!(shown, "C:\\>dir\n<DIR>  docs");
    }

    #[test]
    fn plain_text_passes_through_untouched() {
        let text = "$ ls\nfile.txt\n";
        assert_eq!(strip_sentinel_wrappers(text), text);
    }

    /// 一次完整回合：命令回显 + 输出 + 哨兵，全都理顺
    #[test]
    fn whole_round_trip_leaves_a_readable_transcript() {
        let s = Sentinel::new();
        let wrapped = s.wrap_for("uptime", Shell::Posix);
        // printf 前面那个 \n 保证哨兵一定独占一行 —— 真实回显也是如此
        let transcript = format!("$ {wrapped} 10:00 up 3 days\n{s}0\n");
        let clean = strip_sentinel_wrappers(&transcript);
        assert_eq!(clean, "$ uptime\n 10:00 up 3 days\n");
        assert!(!clean.contains(SENTINEL_MARK));
    }

    #[test]
    fn strip_is_idempotent() {
        let s = Sentinel::new();
        let once = strip_sentinel_wrappers(&s.wrap_for("pwd", Shell::Posix));
        assert_eq!(strip_sentinel_wrappers(&once), once);
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
