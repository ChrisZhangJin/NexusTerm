//! Agent 代为执行命令：在目标 session 的 pty 上跑一条命令，并可靠地判定它何时结束。
//!
//! ## 判定思路：等提示符回来
//!
//! 以前靠拼 `cmd; printf '\n__NX_END_<nonce>__%s\n' "$?"` 哨兵判定结束并拿退出码，
//! 但哨兵必须和命令打在同一行，会原样回显进用户正在看的真实终端，洗不掉。
//!
//! 现在**完全不往命令里拼任何东西**，只发 `cmd\n`，然后盯着终端看——和人的做法一样：
//!
//! 1. **记下提示符**：发命令之前，缓冲里最后一个非空行就是提示符本身
//!    （shell 此刻正停在那儿等输入）。
//! 2. **等它回来**：命令跑完，shell 会重新打印提示符。于是轮询缓冲，
//!    等到「连续 `silence_ms` 没有新输出」且「末行又变回了那个提示符」，就是跑完了。
//!
//! 要求静默是为了压掉误判：命令输出里偶尔真的会出现长得像提示符的行（`cat` 一段
//! README、`grep` 到一条示例），但它后面紧跟着还有输出，熬不过静默窗口。
//!
//! 提示符会变（`\w` 跟着 `cd` 走、`\t` 每秒都在变、git 分支、conda 环境名），
//! 所以比的是「同源」而不是「全等」，见 [`same_prompt`]。
//!
//! **拿不到提示符基线**（缓冲是空的，比如会话刚开、还没来得及打印提示符）时，
//! 退回「静默即算完」——不阻塞也不报错，但在结果里标明没能确认。
//!
//! 提示符一直没回来的情况（命令自己换掉了提示符：`su` / `python3` / 嵌套 ssh；
//! 或 `vim` / `top` 这类全屏程序；或命令真的还在跑）由总超时兜底，
//! 汇报给模型的也是「可能仍在运行」——这比瞎猜一个「已完成」诚实。
//!
//! 退出码因此拿不到，是刻意取舍：它从来只是展示用的参考信息，没有控制流依赖它，
//! 系统提示词本就教模型照真实输出判断成败。
//!
//! **停在交互提示上**（`[sudo] password:` / `[Y/n]` / `less` / `>>>`）是另一种「提示符没回来」：
//! 傻等满 120 秒毫无意义，模型只会对着一片沉默干着急。所以输出安静下来之后，
//! 还会拿缓冲尾部过一遍 [`prompt::classify`]：判成非 shell 提示符的交互提示且置信度够高
//! （[`AWAIT_INPUT_MIN_CONFIDENCE`]），就以 [`ExecEnd::AwaitingInput`] 提前返回，
//! 让模型改用 `nx_send_keys` / `nx_secret` 去应答。**shell 提示符回来（`Confirmed`）永远优先**——
//! 已经在 REPL 里跑命令、提示符就是 `>>> ` 的人，不该被当成「又在等输入」。
//!
//! 所有时限都按**真实流逝的时间**算，不按轮询次数折算。
//!
//! 本模块只负责「写入 + 判定结束 + 截取输出」，**不决定要不要执行**（审批在上层）。

use std::time::Duration;

use tokio::time::{sleep, Instant};

use super::prompt::{self, PromptGuess, PromptKind};

/// 提示符回来之后，还要再确认多久没有新输出，才认定命令结束。
const SILENCE_MS: u64 = 600;
/// 单条命令的总等待上限。超过就强制结束，避免 `top` / `tail -f` 这类永不返回的命令卡死循环。
const MAX_WAIT_MS: u64 = 120_000;
/// 完成判据单条命令的时间上限：执行通道自己的上限再留一点收尾余量。
pub const CHECK_CAP: std::time::Duration = std::time::Duration::from_millis(MAX_WAIT_MS + 10_000);
/// 轮询缓冲的间隔。
const POLL_MS: u64 = 120;
/// 输出安静多久之后，才去判断「是不是停在交互提示上」。
///
/// 比 `SILENCE_MS` 长一点：命令输出中途恰好停在一行像提示的文字、紧接着还有输出，
/// 属于常见的假阳性（进度刷新、分段打印）；多等这几百毫秒换来更稳的判断，
/// 对真正的密码 / 确认提示来说仍是「瞬间」。
const INPUT_QUIET_MS: u64 = 1_000;
/// 判成「在等输入」所需的最低置信度。`prompt::classify` 的档位：>= 0.85 基本确定、
/// 0.5 ~ 0.85 大概率；取 0.8 让「大概率」里偏高的那些也算，更低的只当参考。
pub const AWAIT_INPUT_MIN_CONFIDENCE: f32 = 0.8;
/// 取多少字节的缓冲尾部给分类器。提示一定在最尾巴，不用多读。
pub const CLASSIFY_TAIL_BYTES: usize = 2000;
/// 输出安静多久之后，才去判断「终端交给了另一个程序」（新 shell / 全屏程序），见 [`ExecEnd::Handover`]。
/// 比 [`INPUT_QUIET_MS`] 长：`docker exec` / `ssh` 登录时的 motd、tmux 重绘都是一阵一阵来的。
const HANDOVER_QUIET_MS: u64 = 2_000;
/// 判定「同源提示符」所需的公共前缀 / 后缀长度，见 [`same_prompt`]。
const PROMPT_AFFIX_MATCH: usize = 8;

/// 一条命令执行的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ExecOutcome {
    /// 命令产生的输出（已由缓冲剥离 ANSI）。
    pub output: String,
    /// 为何结束。
    pub reason: ExecEnd,
    /// 收尾时对终端尾部的分类（`Cancelled` 时不做，为 `None`）。
    /// 喂给模型的 `prompt_kind` 就出自这里；`AwaitingInput` 必然带它。
    pub prompt: Option<PromptGuess>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecEnd {
    /// 提示符回来了，且之后再没有新输出——最可信的收尾
    Confirmed,
    /// 没拿到提示符基线，只能按「长时间没有新输出」处理
    Silence,
    /// 命中总超时上限
    Timeout,
    /// 提示符没回来，但终端安静下来后停在了一个交互提示上（密码 / 是否确认 / 分页器 / 菜单 / REPL），
    /// 命令并没有结束——提前返回，让模型去应答而不是干等
    #[serde(rename = "awaiting_input")]
    AwaitingInput,
    /// 提示符没回来，但终端安静下来后已经交给了**另一个程序**：一个新的 shell 提示符（`docker exec -it`、
    /// `su`、`ssh`、`sudo -i`）或全屏程序（`tmux attach`、`vim`、`htop`）。命令本身没结束，
    /// 只是不会再「回来」了 —— 提前返回，让模型读屏接着在新的程序里干活，而不是干等到总超时
    Handover,
    /// 等待期间被取消
    Cancelled,
}

/// 等待命令结束的配置（便于测试注入更短的时限）。
#[derive(Clone, Copy)]
pub struct WaitConfig {
    pub silence_ms: u64,
    /// 判「停在交互提示上」要求的安静时长（见 [`INPUT_QUIET_MS`]）。
    pub input_quiet_ms: u64,
    /// 判「终端交给了另一个程序」要求的安静时长（见 [`HANDOVER_QUIET_MS`]）。
    pub handover_quiet_ms: u64,
    pub max_wait_ms: u64,
    pub poll_ms: u64,
}

impl Default for WaitConfig {
    fn default() -> Self {
        Self {
            silence_ms: SILENCE_MS,
            input_quiet_ms: INPUT_QUIET_MS,
            handover_quiet_ms: HANDOVER_QUIET_MS,
            max_wait_ms: MAX_WAIT_MS,
            poll_ms: POLL_MS,
        }
    }
}

/// 末行 `line` 是不是「发命令前那个提示符 `base`」又回来了。
///
/// 不能全等比：提示符里常带会变的东西。只要两头有一头没变够长，就认同源——
/// - `\w` 跟着 `cd` 变的，前缀稳定：`(base) chris@R720:~$` → `(base) chris@R720:/tmp$`
/// - `\t` 时间戳在开头的，后缀稳定：`[14:23:05] chris@R720:~$` → `[14:25:41] chris@R720:~$`
///
/// 短提示符（`$ `、`>>> `）两头都凑不满长度，走全等那条，照样能认出来。
pub fn same_prompt(base: &str, line: &str) -> bool {
    let (base, line) = (base.trim_end(), line.trim_end());
    if base.is_empty() {
        return false;
    }
    if base == line {
        return true;
    }
    common_affix(base.bytes(), line.bytes()) >= PROMPT_AFFIX_MATCH
        || common_affix(base.bytes().rev(), line.bytes().rev()) >= PROMPT_AFFIX_MATCH
}

/// 自定义提示符交接时给模型的分类：形状像、但没有分类器背书，置信度压在「大概率」一档。
const CUSTOM_PROMPT_CONFIDENCE: f32 = 0.6;
const HINT_CUSTOM_PROMPT: &str =
    "末行像一个新 shell 的自定义提示符（形状判断，不太确定）：先 nx_screen 读屏确认现场再继续。";
/// 自定义提示符最多多长（字符）；长行更可能是程序输出
const CUSTOM_PROMPT_MAX_CHARS: usize = 40;

/// 光标所在的末行像不像一个**分类器认不出的**新 shell 提示符（`inner$ ` `app> ` `dev% ` `box ❯ `）。
///
/// 只在「安静了足够久、原提示符没回来」之后才问（见 `wait_for_end` 的交接分支），所以要求的是形状：
/// 和基线提示符不是同一个、短、以提示符号收尾、符号前有字母数字主体、不超过三个词。
/// 刻意排除：数字收尾（`50%` `5 $`）、分隔线与箭头（`##` `=>` `->`）、HTML / 标签（含 `<`）、URL、
/// PS2 续行等裸符号（`> ` 是 heredoc / 引号没闭合，不是新 shell）。
pub fn looks_like_custom_prompt(base: &str, cursor_line: &str) -> bool {
    let t = cursor_line.trim();
    let n = t.chars().count();
    if n < 2 || n > CUSTOM_PROMPT_MAX_CHARS || same_prompt(base, cursor_line) || t.contains('<') || t.contains("://") {
        return false;
    }
    let Some(last) = t.chars().next_back() else { return false };
    if !matches!(last, '$' | '#' | '%' | '>' | '❯') {
        return false;
    }
    let body = t[..t.len() - last.len_utf8()].trim_end();
    let Some(prev) = body.chars().next_back() else { return false };
    // 数字收尾的多半是计数 / 百分比 / 金额（`50%` `5 $`）；`bash-5.1$` 这类分类器自己认得
    if prev.is_ascii_digit() {
        return false;
    }
    if matches!(prev, '$' | '#' | '%' | '>' | '=' | '-' | '.' | ',' | ':' | ';') {
        return false;
    }
    body.chars().any(|c| c.is_alphanumeric()) && body.split_whitespace().count() <= 3
}

/// 这个分类结果算不算「终端正停在一个交互提示上等人应答」。
///
/// 只认需要应答的几类（密码 / 是否确认 / 分页器 / 菜单 / REPL）；
/// `ShellPrompt`（已经结束）与 `Unknown`（拿不准）都不算，置信度也要够（见 [`AWAIT_INPUT_MIN_CONFIDENCE`]）。
pub fn is_awaiting_input(g: &PromptGuess) -> bool {
    g.confidence >= AWAIT_INPUT_MIN_CONFIDENCE
        && !matches!(g.kind, PromptKind::ShellPrompt | PromptKind::Unknown)
}

/// 两个字节序列的公共前缀长度（倒着传进来就是公共后缀长度）。
fn common_affix(a: impl Iterator<Item = u8>, b: impl Iterator<Item = u8>) -> usize {
    a.zip(b).take_while(|(x, y)| x == y).count()
}

/// 轮询式等待命令结束。
///
/// - `prompt`：发命令之前记下的提示符。`None` = 没拿到，退回「静默即算完」。
/// - `progress()`：一个只增不减的计数（目标 session 迄今写进缓冲的字节数），
///   数值变了就是「有新输出」。不拿输出文本本身来比：缓冲写满后文本长度几乎不变，
///   而且每次轮询都把整份缓冲解码一遍太贵。
/// - `last_line()`：缓冲此刻的末行，只在静默窗口满了之后才取。
/// - `tail()`：缓冲此刻的尾部文本（给 `prompt::classify` 用），只在静默窗口满了之后 / 收尾时才取。
/// - `output()`：「命令写下去之后新增的那段文本」，只在收尾时取一次。
/// - `cancelled()`：返回 true 时立即以 `Cancelled` 收场。
///
/// 纯逻辑、不碰 Tauri，便于单测。计时用 `tokio::time`，测试里暂停时钟即可瞬间跑完。
/// 给提示分类器的「终端尾部」：文本 + 是否 alt screen。
///
/// 有新鲜的前端渲染帧时由它给（全屏 / Ink 一类 TUI 只有渲染后的画面才看得懂，alt 也是真的）；
/// 没有就退回旁路缓冲的原始字节流，alt 只能当 false。`String` 自动转成后者，单测照旧传字符串。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tail {
    pub text: String,
    pub alt: bool,
}

impl From<String> for Tail {
    fn from(text: String) -> Self {
        Self { text, alt: false }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn wait_for_end<P, T, L, O, C, R>(
    cfg: WaitConfig,
    prompt: Option<&str>,
    mut progress: P,
    mut last_line: T,
    mut tail: L,
    mut output: O,
    mut cancelled: C,
) -> ExecOutcome
where
    P: FnMut() -> u64,
    T: FnMut() -> String,
    L: FnMut() -> R,
    R: Into<Tail>,
    O: FnMut() -> String,
    C: FnMut() -> bool,
{
    let classify_tail = |t: Tail| prompt::classify(&t.text, t.alt);
    let silence = Duration::from_millis(cfg.silence_ms);
    // 不短于静默窗口：否则「提示符回来」这条更可信的收尾还没轮到判，就先被「在等输入」抢走了
    let input_quiet = Duration::from_millis(cfg.input_quiet_ms.max(cfg.silence_ms));
    let poll = Duration::from_millis(cfg.poll_ms);
    let deadline = Instant::now() + Duration::from_millis(cfg.max_wait_ms);

    let finish = |output: &mut O, tail: &mut L, reason, guess: Option<PromptGuess>| ExecOutcome {
        output: strip_tail_noise(&output()),
        reason,
        // 取消时不分类（没人要看）；其余收尾都带上，模型据此知道终端此刻停在什么上
        prompt: match (reason, guess) {
            (ExecEnd::Cancelled, _) => None,
            (_, Some(g)) => Some(g),
            (_, None) => Some(classify_tail(tail().into())),
        },
    };

    let mut last = progress();
    let mut last_change = Instant::now();
    // 已经对「进度 = N」那一刻的尾部分类过了：输出没动，结果不会变，别每个轮询周期都再解码一遍缓冲
    let mut classified_at: Option<u64> = None;
    let mut handover_at: Option<u64> = None;
    let handover_quiet = Duration::from_millis(cfg.handover_quiet_ms.max(cfg.input_quiet_ms).max(cfg.silence_ms));

    loop {
        if cancelled() {
            return finish(&mut output, &mut tail, ExecEnd::Cancelled, None);
        }

        let now_progress = progress();
        if now_progress != last {
            last = now_progress;
            last_change = Instant::now();
        }

        let quiet_for = Instant::now().duration_since(last_change);
        if quiet_for >= silence {
            match prompt {
                // 提示符回来了：命令结束
                Some(base) if same_prompt(base, &last_line()) => {
                    return finish(&mut output, &mut tail, ExecEnd::Confirmed, None)
                }
                // 没有基线可比：先看是不是停在交互提示上，否则静默就是唯一线索
                None => {
                    let g = classify_tail(tail().into());
                    if is_awaiting_input(&g) {
                        return finish(&mut output, &mut tail, ExecEnd::AwaitingInput, Some(g));
                    }
                    return finish(&mut output, &mut tail, ExecEnd::Silence, Some(g));
                }
                // 有基线但还没回来：继续等，交给总超时兜底——
                // 不过若是停在交互提示上，没必要等满，早点让模型去应答
                Some(_) => {
                    if quiet_for >= input_quiet && classified_at != Some(last) {
                        classified_at = Some(last);
                        let g = classify_tail(tail().into());
                        if is_awaiting_input(&g) {
                            return finish(&mut output, &mut tail, ExecEnd::AwaitingInput, Some(g));
                        }
                    }
                    // 再安静一会儿还是没回来：读屏看终端是不是交给了别的程序（新 shell / 全屏程序）
                    if quiet_for >= handover_quiet && handover_at != Some(last) {
                        handover_at = Some(last);
                        let t: Tail = tail().into();
                        let alt = t.alt;
                        let cursor_line = t.text.rsplit('\n').next().unwrap_or_default().to_string();
                        let g = classify_tail(t);
                        let new_shell = g.kind == PromptKind::ShellPrompt && g.confidence >= AWAIT_INPUT_MIN_CONFIDENCE;
                        if alt || new_shell {
                            return finish(&mut output, &mut tail, ExecEnd::Handover, Some(g));
                        }
                        // 分类器认不出的自定义短提示符（`inner$ ` `app> `）：形状对就也算交接，免得等满总超时
                        if let Some(base) = prompt.filter(|b| looks_like_custom_prompt(b, &cursor_line)) {
                            tracing::debug!(base, line = cursor_line.as_str(), "exec: 自定义提示符，按交接收尾");
                            let g = PromptGuess { kind: PromptKind::ShellPrompt, confidence: CUSTOM_PROMPT_CONFIDENCE, hint: HINT_CUSTOM_PROMPT };
                            return finish(&mut output, &mut tail, ExecEnd::Handover, Some(g));
                        }
                    }
                }
            }
        }

        if Instant::now() >= deadline {
            return finish(&mut output, &mut tail, ExecEnd::Timeout, None);
        }

        sleep(poll).await;
    }
}

/// 去掉输出尾部可能残留的多余空白。
fn strip_tail_noise(s: &str) -> String {
    s.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROMPT: &str = "(base) chris@CHS-PowerEdge-R720:~$ ";

    fn cfg(silence_ms: u64, max_wait_ms: u64, poll_ms: u64) -> WaitConfig {
        WaitConfig { silence_ms, input_quiet_ms: 1_000, handover_quiet_ms: 2_000, max_wait_ms, poll_ms }
    }

    /// 回归（截图：`docker exec -it … bash`）：命令换了一个新 shell，原提示符永远不会回来 ——
    /// 读屏认出新的 shell 提示符就收尾，而不是干等到 120 秒总超时。
    #[tokio::test(start_paused = true)]
    async fn new_shell_prompt_hands_over_instead_of_timing_out() {
        let started = Instant::now();
        let inner = "root@ubuntu-22at159:~/workspace/Sonari# ";
        let out = wait_for_end(
            cfg(300, 120_000, 50),
            Some("(base) chris@ubuntu-22at159:~/workspace$ "),
            || 0,
            || inner.to_string(),
            || inner.to_string(),
            || String::new(),
            || false,
        )
        .await;
        assert_eq!(out.reason, ExecEnd::Handover);
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    }

    /// 回归（e2e `nested_shell_with_custom_prompt_hands_over`）：`PS1='inner$ ' bash` —— 分类器认不出这么短的
    /// 自定义提示符，原提示符又不会回来，曾经等满 120s 才 Timeout。
    #[tokio::test(start_paused = true)]
    async fn custom_short_prompt_hands_over_instead_of_timing_out() {
        let started = Instant::now();
        let out = wait_for_end(
            cfg(300, 120_000, 50),
            Some("root@e2e:/tmp/work# "),
            || 0,
            || "inner$".to_string(),
            || "root@e2e:/tmp/work# PS1='inner$ ' bash --norc\ninner$".to_string(),
            || String::new(),
            || false,
        )
        .await;
        assert_eq!(out.reason, ExecEnd::Handover);
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        let g = out.prompt.unwrap();
        assert_eq!(g.kind, PromptKind::ShellPrompt);
        assert!(g.confidence < AWAIT_INPUT_MIN_CONFIDENCE, "形状判断，不冒充分类器的把握");
    }

    #[test]
    fn custom_prompt_shape() {
        let base = "chris@host:~$ ";
        for l in ["inner$ ", "app> ", "dev% ", "box ❯ ", "my shell #", "mysh$"] {
            assert!(looks_like_custom_prompt(base, l), "{l}");
        }
        for l in [
            "   Compiling serde v1.0.200",
            "Downloading 50%",
            "> ",
            ">",
            "$",
            "==>",
            "build ->",
            "<div>",
            "####",
            "see http://x.y/a#",
            "a very long line of program output that happens to end with $",
            "chris@host:~$ ",
            "Total cost: 5 $",
            "...>",
        ] {
            assert!(!looks_like_custom_prompt(base, l), "{l}");
        }
    }

    /// 回归（截图：`tmux attach -t cnn` 等了 50 秒还在等）：全屏程序接管了终端，同样提前收尾。
    #[tokio::test(start_paused = true)]
    async fn full_screen_program_hands_over_instead_of_timing_out() {
        let started = Instant::now();
        let out = wait_for_end(
            cfg(300, 120_000, 50),
            Some("(base) chris@ubuntu-22at159:~/workspace$ "),
            || 0,
            || "[cnn] 0:bash- 1:sonari*      \"ubuntu-22at159\" 23:12 16-Nov-26".to_string(),
            || Tail { text: "(base) chris@ubuntu-22at159:~/wo\n[cnn] 0:bash- 1:sonari*".into(), alt: true },
            || String::new(),
            || false,
        )
        .await;
        assert_eq!(out.reason, ExecEnd::Handover);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// 还在跑的命令（安静了一会儿，但末行不是提示符、也不是全屏）：照旧等，不误判成交接。
    #[tokio::test(start_paused = true)]
    async fn quiet_running_command_is_not_a_handover() {
        let out = wait_for_end(
            cfg(300, 10_000, 50),
            Some("(base) chris@ubuntu-22at159:~/workspace$ "),
            || 0,
            || "   Compiling serde v1.0.200".to_string(),
            || "   Compiling serde v1.0.200".to_string(),
            || String::new(),
            || false,
        )
        .await;
        assert_eq!(out.reason, ExecEnd::Timeout);
    }

    #[tokio::test(start_paused = true)]
    async fn prompt_back_after_silence_ends_the_run() {
        let out = wait_for_end(
            cfg(300, 10_000, 50),
            Some(PROMPT),
            || 0,
            || PROMPT.to_string(),
            || PROMPT.to_string(),
            || "a.mp3\nb.mp3".to_string(),
            || false,
        )
        .await;
        assert_eq!(out.reason, ExecEnd::Confirmed);
        assert_eq!(out.output, "a.mp3\nb.mp3");
    }

    /// 回归（截图里那条 `find`）：命令刷了一大堆输出、早就跑完回到提示符了，
    /// 判定必须在静默窗口内收尾，而不是干等到 120 秒总超时。
    #[tokio::test(start_paused = true)]
    async fn noisy_command_that_finished_does_not_wait_for_the_timeout() {
        let started = Instant::now();
        let n = std::cell::Cell::new(0u64);
        let out = wait_for_end(
            cfg(600, 120_000, 120),
            Some(PROMPT),
            || {
                // 前 10 轮一直在刷输出，之后回到提示符不动了
                if n.get() < 10 {
                    n.set(n.get() + 1);
                }
                n.get()
            },
            || PROMPT.to_string(),
            || PROMPT.to_string(),
            || "…一大堆 mp3…".to_string(),
            || false,
        )
        .await;
        assert_eq!(out.reason, ExecEnd::Confirmed);
        assert!(started.elapsed() < Duration::from_secs(5), "本该秒回，实际 {:?}", started.elapsed());
    }

    /// 输出里混进一行长得像提示符的文本（`cat` 一段 README），但后面还有输出：
    /// 静默窗口会把它熬掉，不该在那一刻误判成结束。
    #[tokio::test(start_paused = true)]
    async fn prompt_like_line_mid_stream_is_not_a_false_finish() {
        let n = std::cell::Cell::new(0u64);
        let out = wait_for_end(
            cfg(600, 10_000, 120),
            Some(PROMPT),
            || {
                n.set(n.get() + 1);
                n.get()
            },
            || PROMPT.to_string(), // 末行一直"像"提示符
            || PROMPT.to_string(),
            || "streaming".to_string(),
            || false,
        )
        .await;
        // 输出一直在增长 → 静默窗口永远满不了 → 只能撞总超时，而不是提前宣布完成
        assert_eq!(out.reason, ExecEnd::Timeout);
    }

    /// 提示符始终没回来（`su` / `python3` 换掉了提示符、或命令真的还在跑）：
    /// 老老实实等到总超时，并如实汇报，不猜一个「已完成」。
    #[tokio::test(start_paused = true)]
    async fn prompt_never_returns_falls_through_to_timeout() {
        let out = wait_for_end(
            cfg(300, 3_000, 100),
            Some(PROMPT),
            || 0,
            || ">>> ".to_string(),
            // 尾部是 REPL 提示：但这条测试要的是「没有判成交互提示时撞总超时」，所以尾部给无信息的普通输出
            || "Python 3.12".to_string(),
            || "Python 3.12".to_string(),
            || false,
        )
        .await;
        assert_eq!(out.reason, ExecEnd::Timeout);
    }

    #[tokio::test(start_paused = true)]
    async fn without_a_baseline_silence_is_the_only_clue() {
        let out = wait_for_end(
            cfg(300, 10_000, 100),
            None,
            || 0,
            || "whatever".to_string(),
            || "whatever".to_string(),
            || "output".to_string(),
            || false,
        )
        .await;
        assert_eq!(out.reason, ExecEnd::Silence);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_honors_cancellation() {
        let out = wait_for_end(
            WaitConfig::default(),
            Some(PROMPT),
            || 0,
            String::new,
            String::new,
            || "running".to_string(),
            || true, // 立即取消
        )
        .await;
        assert_eq!(out.reason, ExecEnd::Cancelled);
        assert_eq!(out.output, "running");
    }

    /// 提示符没回来、但终端停在 `[sudo] password` 上：不该干等 120 秒，要提前交还给模型。
    #[tokio::test(start_paused = true)]
    async fn password_prompt_returns_early_as_awaiting_input() {
        let started = Instant::now();
        let out = wait_for_end(
            WaitConfig::default(),
            Some(PROMPT),
            || 0,
            || "[sudo] password for chris:".to_string(),
            || "$ sudo ls\n[sudo] password for chris:".to_string(),
            || "[sudo] password for chris:".to_string(),
            || false,
        )
        .await;
        assert_eq!(out.reason, ExecEnd::AwaitingInput);
        assert_eq!(out.prompt.map(|g| g.kind), Some(PromptKind::Password));
        assert!(started.elapsed() < Duration::from_secs(5), "本该很快返回，实际 {:?}", started.elapsed());
    }

    #[tokio::test(start_paused = true)]
    async fn yes_no_prompt_returns_early_as_awaiting_input() {
        let out = wait_for_end(
            WaitConfig::default(),
            Some(PROMPT),
            || 0,
            || "Do you want to continue? [Y/n]".to_string(),
            || "$ apt install foo\nDo you want to continue? [Y/n]".to_string(),
            || "Do you want to continue? [Y/n]".to_string(),
            || false,
        )
        .await;
        assert_eq!(out.reason, ExecEnd::AwaitingInput);
        assert_eq!(out.prompt.map(|g| g.kind), Some(PromptKind::YesNo));
    }

    /// 没拿到基线时，原本是「静默即算完」；现在若停在交互提示上，要如实说「在等输入」而不是「已完成」。
    #[tokio::test(start_paused = true)]
    async fn without_a_baseline_an_interactive_prompt_is_awaiting_input_not_silence() {
        let out = wait_for_end(
            cfg(300, 10_000, 100),
            None,
            || 0,
            || "Password:".to_string(),
            || "Password:".to_string(),
            || "Password:".to_string(),
            || false,
        )
        .await;
        assert_eq!(out.reason, ExecEnd::AwaitingInput);
    }

    /// shell 提示符回来永远优先：已经身处 REPL 的人，`>>> ` 就是他的提示符，
    /// 命令跑完回到 `>>> ` 必须判 Confirmed，而不是「又在等输入」。
    #[tokio::test(start_paused = true)]
    async fn returned_baseline_prompt_wins_over_awaiting_input() {
        let out = wait_for_end(
            WaitConfig::default(),
            Some(">>> "),
            || 0,
            || ">>> ".to_string(),
            || ">>> ".to_string(),
            || "3".to_string(),
            || false,
        )
        .await;
        assert_eq!(out.reason, ExecEnd::Confirmed);
    }

    /// 输出还在刷的时候，哪怕当下末行像一个确认提示，也不能提前返回——
    /// 要先安静下来（见 `INPUT_QUIET_MS`）才去判。
    #[tokio::test(start_paused = true)]
    async fn prompt_like_tail_while_output_is_still_flowing_is_not_awaiting_input() {
        let n = std::cell::Cell::new(0u64);
        let out = wait_for_end(
            cfg(600, 5_000, 120),
            Some(PROMPT),
            || {
                n.set(n.get() + 1);
                n.get()
            },
            || "…".to_string(),
            || "Continue? [y/N]".to_string(),
            || "streaming".to_string(),
            || false,
        )
        .await;
        assert_eq!(out.reason, ExecEnd::Timeout);
    }

    /// 回归：没有交互提示的普通长命令（尾部是无信息的输出）仍旧等到总超时，行为不变。
    #[tokio::test(start_paused = true)]
    async fn uninformative_tail_still_falls_through_to_timeout() {
        let out = wait_for_end(
            cfg(300, 3_000, 100),
            Some(PROMPT),
            || 0,
            || "copying 41%".to_string(),
            || "copying 41%".to_string(),
            || "copying 41%".to_string(),
            || false,
        )
        .await;
        assert_eq!(out.reason, ExecEnd::Timeout);
        // 超时收尾也带着分类，供模型参考
        assert!(out.prompt.is_some());
    }

    /// 另一个 shell 提示符（嵌套 ssh / su 之后）不是「在等输入」——它不需要应答，也不是 REPL；
    /// 但原提示符也不会回来了：按「交给了另一个程序」收尾（以前是干等到总超时）。
    #[tokio::test(start_paused = true)]
    async fn a_foreign_shell_prompt_is_not_awaiting_input() {
        let out = wait_for_end(
            cfg(300, 3_000, 100),
            Some(PROMPT),
            || 0,
            || "root@other-box:/opt# ".to_string(),
            || "root@other-box:/opt# ".to_string(),
            || "".to_string(),
            || false,
        )
        .await;
        assert_eq!(out.reason, ExecEnd::Handover);
        assert_eq!(out.prompt.map(|g| g.kind), Some(PromptKind::ShellPrompt));
        assert!(!is_awaiting_input(&classify_tail_for_test("root@other-box:/opt# ")));
    }

    fn classify_tail_for_test(t: &str) -> PromptGuess {
        crate::agent::prompt::classify(t, false)
    }

    #[tokio::test(start_paused = true)]
    async fn confirmed_outcome_carries_a_shell_prompt_classification() {
        let out = wait_for_end(
            cfg(300, 10_000, 50),
            Some(PROMPT),
            || 0,
            || PROMPT.to_string(),
            || PROMPT.to_string(),
            || "a.mp3".to_string(),
            || false,
        )
        .await;
        assert_eq!(out.reason, ExecEnd::Confirmed);
        assert_eq!(out.prompt.map(|g| g.kind), Some(PromptKind::ShellPrompt));
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_outcome_has_no_classification() {
        let out = wait_for_end(
            WaitConfig::default(),
            Some(PROMPT),
            || 0,
            String::new,
            || "Password:".to_string(),
            || "running".to_string(),
            || true,
        )
        .await;
        assert_eq!(out.reason, ExecEnd::Cancelled);
        assert!(out.prompt.is_none());
    }

    #[test]
    fn is_awaiting_input_respects_kind_and_threshold() {
        use crate::agent::prompt::classify;
        assert!(is_awaiting_input(&classify("[sudo] password for x:", false)));
        assert!(is_awaiting_input(&classify("Proceed? [Y/n]", false)));
        assert!(is_awaiting_input(&classify(">>> ", false)));
        // 已回到 shell / 看不出来：都不算
        assert!(!is_awaiting_input(&classify(PROMPT, false)));
        assert!(!is_awaiting_input(&classify("some random output line", false)));
        assert!(!is_awaiting_input(&classify("", false)));
        // 置信度不够就不算，即使种类对
        let weak = PromptGuess { kind: PromptKind::Menu, confidence: AWAIT_INPUT_MIN_CONFIDENCE - 0.01, hint: "" };
        assert!(!is_awaiting_input(&weak));
        let strong = PromptGuess { confidence: AWAIT_INPUT_MIN_CONFIDENCE, ..weak };
        assert!(is_awaiting_input(&strong));
    }

    /// `AwaitingInput` 序列化名要稳定：前端 / 模型都按这个字符串认。
    #[test]
    fn awaiting_input_serializes_as_snake_case() {
        assert_eq!(serde_json::to_string(&ExecEnd::AwaitingInput).unwrap(), "\"awaiting_input\"");
        assert_eq!(serde_json::to_string(&ExecEnd::Confirmed).unwrap(), "\"confirmed\"");
    }

    /// 末行要到静默窗口满了才取：命令刷屏期间每 120ms 解码一次缓冲太贵。
    #[tokio::test(start_paused = true)]
    async fn last_line_is_only_read_once_output_goes_quiet() {
        let n = std::cell::Cell::new(0u64);
        let reads = std::cell::Cell::new(0u32);
        let _ = wait_for_end(
            cfg(600, 2_000, 100),
            Some(PROMPT),
            || {
                n.set(n.get() + 1);
                n.get()
            },
            || {
                reads.set(reads.get() + 1);
                "still going".to_string()
            },
            String::new,
            String::new,
            || false,
        )
        .await;
        assert_eq!(reads.get(), 0, "输出一直在增长，不该去取末行");
    }

    #[test]
    fn identical_prompt_matches() {
        assert!(same_prompt(PROMPT, PROMPT));
        // 行尾空白无所谓：缓冲取末行时已经 trim 过
        assert!(same_prompt(PROMPT, PROMPT.trim_end()));
    }

    /// `cd` 之后 `\w` 变了，前缀还在。
    #[test]
    fn prompt_matches_after_cwd_changed() {
        assert!(same_prompt(PROMPT, "(base) chris@CHS-PowerEdge-R720:/var/log$ "));
    }

    /// 时间戳在开头的提示符，前缀每次都不同，后缀还在。
    #[test]
    fn prompt_matches_when_only_the_leading_timestamp_changed() {
        assert!(same_prompt("[14:23:05] chris@R720:~$ ", "[14:25:41] chris@R720:~$ "));
    }

    /// 短提示符两头都凑不满 8 个字符，只能全等——但本来也不会变。
    #[test]
    fn short_prompts_match_exactly() {
        assert!(same_prompt("$ ", "$ "));
        assert!(same_prompt(">>> ", ">>> "));
        assert!(!same_prompt("$ ", ">>> "));
    }

    #[test]
    fn unrelated_output_is_not_a_prompt() {
        assert!(!same_prompt(PROMPT, "/home/chris/a.mp3"));
        assert!(!same_prompt(PROMPT, ""));
        // 没有基线时任何东西都不算匹配
        assert!(!same_prompt("", "(base) chris@CHS-PowerEdge-R720:~$ "));
    }

    /// 换了台机器 / 换了用户，提示符整体不同，不能认成同一个。
    #[test]
    fn a_different_host_prompt_does_not_match() {
        assert!(!same_prompt(PROMPT, "root@other-box:/opt# "));
    }
}
