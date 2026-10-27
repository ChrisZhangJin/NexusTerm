//! 上下文出网前的脱敏管道。
//!
//! 终端输出里什么都有：`cat ~/.aws/credentials`、`env | grep TOKEN`、
//! 邮箱、内网 IP、CI 里直接 `echo $OPENAI_API_KEY`。这些东西一旦被拼进
//! prompt 发给第三方模型，就是一次没有日志、无法回收的凭据泄漏。
//!
//! 所以脱敏必须是**出网前的最后一个闸门**，而不是可选项。
//!
//! 占位符统一用 [`PLACEHOLDER`]：区分类型的占位符（`[REDACTED:EMAIL]`）
//! 会把「原文这里是一个邮箱」这件事也告诉对方 —— 粒度越细泄漏越多，
//! 这里的目标是尽可能零泄漏，可读性交给周围的上下文去补。

use regex::Regex;
use std::sync::OnceLock;

/// 所有敏感片段的统一占位符。
pub const PLACEHOLDER: &str = "[REDACTED]";

/// 一条基于正则的脱敏规则。
struct Rule {
    re: Regex,
    /// true：只替换值、保留左边的参数名（`token=` 这类），保证上下文仍然可读
    keep_name: bool,
}

/// 规则集的匹配顺序是有意义的：前缀型 token（`sk-` / `Bearer` / `AKIA`）在前，
/// 宽泛的邮箱与 IP 在后 —— 否则 IP 规则会先把形如地址的东西吃掉，
/// 让更具体的规则再也匹配不到。
///
/// PEM 私钥不走这里：它是多行结构，且经常被环形缓冲截成半截，
/// 用行扫描器处理更可靠（见 [`redact_pem_blocks`]）。
fn rules() -> &'static [Rule] {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(|| {
        let patterns: &[(&str, bool)] = &[
            // Authorization 头里的 Bearer token
            (r#"(?i)bearer\s+[A-Za-z0-9._~+/=\-]{8,}"#, false),
            // OpenAI / Anthropic 风格的前缀 key
            (r#"sk-[A-Za-z0-9_\-]{8,}"#, false),
            // AWS access key id
            (r#"AKIA[0-9A-Z]{16}"#, false),
            // URL / 命令行里的 `token=xxx` `api_key=xxx` `password='xxx'`。
            // 值写成三选一（双引号 / 单引号 / 裸值）是为了把引号一起吃掉；
            // regex crate 基于有限自动机、**不支持反向引用**，所以不能用
            // `=(['"]?)X\1` 这种写法来保证引号配对。
            // 引号内的值允许带空格（`password="my secret pw"`），
            // 但限制在单行内（`[^"\n]`），否则一处没闭合的引号会吞掉半屏正文。
            (r#"(?i)\b(token|api[_-]?key|access[_-]?key|secret[_-]?key|secret|password|passwd|pwd|Authorization|auth)=(?:"[^"\n]{0,256}"|'[^'\n]{0,256}'|[^\s&"'<>`]+)"#, true),
            // 邮箱
            (r#"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}"#, false),
            // IPv4（限定每段 0-255）
            (r#"\b(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])(?:\.(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])){3}\b"#, false),
        ];

        patterns
            .iter()
            .map(|(pattern, keep_name)| Rule {
                // 这里是程序 bug（正则写错）而不是运行时的外部输入，
                // 且 [all_patterns_compile] 单测会锁住它能通过，故 panic 是合理的
                re: Regex::new(pattern).unwrap_or_else(|e| {
                    panic!("脱敏正则编译失败，请检查 pattern `{pattern}`：{e}")
                }),
                keep_name: *keep_name,
            })
            .collect()
    })
}

/// PEM 私钥开始标记行的判定。
///
/// 刻意用 `contains` 而不是 `^` 锚定：环形缓冲是从中间切进去的，
/// 而且 Bash 提示符常常和正文挤在同一行（`user@host:~$ cat ---...`）。
/// 宁可整行替换掉（含提示符），也不能放过一个真私钥。
fn is_begin_line(line: &str) -> bool {
    line.contains("-----BEGIN") && line.contains("PRIVATE KEY-----")
}

fn is_end_line(line: &str) -> bool {
    line.contains("-----END") && line.contains("PRIVATE KEY-----")
}

/// 私钥正文的最小行长。真实的 PEM base64 正文一行 64 字符，
/// 取 16 作为「这确实是一段密钥而不是普通文本」的下限。
const PEM_BODY_MIN_LEN: usize = 16;

/// 是否像一行 PEM 正文：纯 base64 字母，不含空格。
///
/// 有这个约束在，普通的英文句子（带空格）不会让扫描器误以为还在正文里。
fn is_body_line(line: &str) -> bool {
    !line.is_empty() && line.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
}

/// 剥离 PEM 私钥块（含被环形缓冲截断的残块）。
///
/// 不用正则（`-----BEGIN [\s\S]*?-----END`）：环形缓冲会把几屏长的私钥切断，
/// 只剩半截是常态，而 base64 正文才是真正的 key，标记行一文不值 ——
/// 正则只认完整块，缺一头就整段失守。
///
/// 故改为按行扫描：`BEGIN` 没等到 `END` 就删到串尾；`END` 前面连着的 base64 正文回溯删掉。
fn redact_pem_blocks(text: &str) -> String {
    // split_inclusive 保留换行符，重建时能原样拼回去（含 \r\n）
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut dropped = vec![false; lines.len()];

    // 当前处于「已看见密钥证据」的状态：block_start 是本块的第一行
    let mut block_start: Option<usize> = None;
    let mut saw_begin = false;
    let mut saw_long_body = false;

    for i in 0..lines.len() {
        let line = lines[i].trim();
        if line.is_empty() {
            // 空行不打断块判定：某些格式在 BEGIN 后会插一个空行
            continue;
        }

        if is_begin_line(line) {
            block_start = Some(i);
            saw_begin = true;
            saw_long_body = false;
            continue;
        }

        if is_end_line(line) {
            // 有 BEGIN → 删整块；没有 BEGIN（头被截掉了）但前面连着真实的
            // 长 base64 正文 → 那是被截断的下半截，同样整段删
            let start = if saw_begin || saw_long_body {
                block_start.unwrap_or(i)
            } else {
                i
            };
            for j in start..=i {
                dropped[j] = true;
            }
            block_start = None;
            saw_begin = false;
            saw_long_body = false;
            continue;
        }

        if is_body_line(line) {
            if block_start.is_none() {
                block_start = Some(i);
            }
            if line.len() >= PEM_BODY_MIN_LEN {
                saw_long_body = true;
            }
            continue;
        }

        // 既非标记也非正文 → 本块到此为止。
        // 已经见过 BEGIN 却没等到 END：是被截断的上半截，删掉已确认的部分。
        if saw_begin {
            if let Some(s) = block_start {
                for j in s..i {
                    dropped[j] = true;
                }
            }
        }
        block_start = None;
        saw_begin = false;
        saw_long_body = false;
    }

    // 走到串尾 BEGIN 依然悬着（流被缓冲截掉了尾巴）
    if saw_begin {
        if let Some(s) = block_start {
            for j in s..lines.len() {
                dropped[j] = true;
            }
        }
    }

    if !dropped.iter().any(|d| *d) {
        return text.to_string();
    }

    // 连续被删的行折叠成一个占位符，避免输出几十行 [REDACTED]
    let mut out = String::with_capacity(text.len());
    let mut in_run = false;
    for (i, line) in lines.iter().enumerate() {
        if dropped[i] {
            if !in_run {
                in_run = true;
                out.push_str(PLACEHOLDER);
                if line.ends_with('\n') {
                    out.push('\n');
                }
            }
        } else {
            in_run = false;
            out.push_str(line);
        }
    }
    out
}

/// 脱敏一段文本，返回替换后的新串。
///
/// `AgentManager::context` 刻意保持原文（UI 要展示），脱敏只在 LLM 请求出网这条边界上做
/// （见 `llm::sanitize_context`）。
#[allow(dead_code)]
pub fn redact(text: &str) -> String {
    // PEM 先走：它的 base64 正文不该被后面的 IP / 邮箱规则再点一遍名
    let mut out = redact_pem_blocks(text);
    for rule in rules() {
        out = if rule.keep_name {
            rule.re
                .replace_all(&out, format!("${{1}}={PLACEHOLDER}").as_str())
                .into_owned()
        } else {
            rule.re.replace_all(&out, PLACEHOLDER).into_owned()
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_patterns_compile() {
        // 走一遍空串即可触发 OnceLock 初始化；任何 pattern 写错这里就会 panic
        assert_eq!(redact(""), "");
    }

    #[test]
    fn redacts_openai_key() {
        assert_eq!(
            redact("OPENAI_API_KEY=sk-proj-abcdefghijklmnopqrstuvwxyz1234"),
            format!("OPENAI_API_KEY={PLACEHOLDER}")
        );
        assert_eq!(
            redact("Authorization: Bearer sk-abcdefgh12345678"),
            format!("Authorization: {PLACEHOLDER}")
        );
    }

    #[test]
    fn redacts_bearer_token() {
        assert_eq!(
            redact("Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.payload-x"),
            format!("Authorization: {PLACEHOLDER}")
        );
        assert_eq!(
            redact("curl -H 'bearer abc.def_ghi-jkl' https://x"),
            format!("curl -H '{PLACEHOLDER}' https://x")
        );
    }

    #[test]
    fn redacts_aws_access_key() {
        let out = redact("aws_access_key_id = AKIAIOSFODNN7EXAMPLE");
        assert_eq!(out, format!("aws_access_key_id = {PLACEHOLDER}"));
        // 长度不足 16 位不算 AKIA key，避免误伤普通单词
        assert_eq!(redact("AKIASHORT"), "AKIASHORT");
    }

    #[test]
    fn redacts_pem_private_key() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA1234567890\n-----END RSA PRIVATE KEY-----";
        assert_eq!(redact(pem), format!("{PLACEHOLDER}\n"));
    }

    #[test]
    fn redacts_openssh_private_key() {
        let pem = concat!(
            "-----BEGIN OPENSSH PRIVATE KEY-----\n",
            "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n",
            "-----END OPENSSH PRIVATE KEY-----\n"
        );
        let out = redact(pem);
        assert!(!out.contains("b3BlbnNzaC1rZXktdjE"));
        assert_eq!(out, format!("{PLACEHOLDER}\n"));
    }

    /// 环形缓冲从头部截断：只剩 END 和它前面的 base64 正文，BEGIN 已被丢弃
    #[test]
    fn redacts_head_truncated_key() {
        let tail = concat!(
            "MIIEPjCCBGSgAwIBAgIRAJ9OZ4bSbJTrkKgBnRviF1owDQYJKoZIhvcNAQELBQAw\n",
            "uTgvZXhhbXBsZS5jb20vcGF0aC90by9jZXJ0aWZpY2F0ZS1hdXRob3JpdHktMQ\n",
            "-----END RSA PRIVATE KEY-----\n",
            "$ neofetch\n"
        );
        let out = redact(tail);
        assert!(!out.contains("MIIEPjCCBGSgAwIBAg"), "正文漏了：{out}");
        assert_eq!(out, format!("{PLACEHOLDER}\n$ neofetch\n"));
    }

    /// 反过来：只剩下半截，BEGIN 之后一直没有 END
    #[test]
    fn redacts_tail_truncated_key() {
        let tail = concat!(
            "$ cat ~/.ssh/id_rsa\n",
            "-----BEGIN OPENSSH PRIVATE KEY-----\n",
            "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n"
        );
        let out = redact(tail);
        assert!(!out.contains("b3BlbnNzaC1rZXktdjE"), "正文漏了：{out}");
        assert_eq!(out, format!("$ cat ~/.ssh/id_rsa\n{PLACEHOLDER}\n"));
    }

    /// 提示符和 BEGIN 挤在同一行时也要认出来
    #[test]
    fn recognizes_begin_marker_with_prompt_prefix() {
        let out = redact("host:~$ -----BEGIN PRIVATE KEY-----\nQUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVo\n");
        assert!(!out.contains("QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVo"));
        assert_eq!(out, format!("{PLACEHOLDER}\n"));
    }

    /// 没有真正的密钥时不能把普通输出误吃掉
    #[test]
    fn does_not_eat_ordinary_output() {
        let text = "$ ls -la\ntotal 8\n-rw-------  1 user  staff  3243 Jan  1 00:00 id_rsa\n$ uptime\n";
        assert_eq!(redact_pem_blocks(text), text);
    }

    #[test]
    fn redacts_email() {
        assert_eq!(
            redact("联系 alice.wu@example.com 获取详情"),
            format!("联系 {PLACEHOLDER} 获取详情")
        );
    }

    #[test]
    fn redacts_ipv4() {
        assert_eq!(
            redact("ssh root@host via 192.168.31.7 failed"),
            format!("ssh root@host via {PLACEHOLDER} failed")
        );
        // 非法八位组不算 IPv4
        assert_eq!(redact("192.300.1.1"), "192.300.1.1");
    }

    #[test]
    fn redacts_query_parameters_but_keeps_names() {
        assert_eq!(
            redact("https://api.x.com/v1/me?token=abc123&page=2"),
            format!("https://api.x.com/v1/me?token={PLACEHOLDER}&page=2")
        );
        assert_eq!(
            redact("--api_key=secret value --other=keepme"),
            format!("--api_key={PLACEHOLDER} value --other=keepme")
        );
        // 引号要随值一起吃掉，不留悬空的半截引号
        assert_eq!(redact("password='hunter2'"), format!("password={PLACEHOLDER}"));
        assert_eq!(
            redact("password=\"my secret pw\""),
            format!("password={PLACEHOLDER}")
        );
    }

    #[test]
    fn keeps_ordinary_text_intact() {
        let text = "$ ls -la\ntotal 8\ndrwxr-xr-x  2 user staff   64 Jan  1 00:00 src\n";
        assert_eq!(redact(text), text);
    }

    #[test]
    fn keeps_versions_and_decimals_readable_enough() {
        // 已知副作用：点分版本号与 IPv4 同形，会被一起脱掉。
        // 宁误杀不漏放 —— 见模块注释。
        assert!(redact("nginx 1.2.3.4").contains(PLACEHOLDER));
    }

    #[test]
    fn is_idempotent() {
        let once = redact("key sk-abcdefgh12345678 to a@b.com at 10.0.0.1");
        let twice = redact(&once);
        assert_eq!(once, twice);
        assert!(!once.contains("a@b.com"));
    }

    #[test]
    fn never_panics_on_weird_input() {
        let weird = "\u{1b}[31m---\u{0}-----BEGIN \u{7f}PRIVATE KEY----- %%%%% \r\n\t";
        let out = redact(weird);
        assert!(!out.is_empty());
    }

    #[test]
    fn preserves_crlf_line_endings() {
        let text = "line-one\r\nline-two\r\n";
        assert_eq!(redact_pem_blocks(text), text);
    }
}
