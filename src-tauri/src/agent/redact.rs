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
//!
//! 两个入口：
//! - [`redact`]：通用，连续被删的 PEM 行折叠成一个占位符；
//! - [`redact_lines`]：**保持行数不变**（每个被删的行各换成一个占位符），
//!   给「先脱敏、再编行号」的文件读取 / grep 用 —— 行号必须和真实文件对得上。
//!
//! 所有正则规则都不跨行（不用 `\s` 吃换行），所以两个入口在行结构上的差别只来自 PEM 扫描。

use regex::{Captures, Regex};
use std::sync::OnceLock;

/// 所有敏感片段的统一占位符。
pub const PLACEHOLDER: &str = "[REDACTED]";

/// 一条基于正则的脱敏规则。
struct Rule {
    re: Regex,
    /// true：只替换名为 `v` 的捕获组（值），其余（参数名、分隔符）原样保留，
    /// 保证上下文仍然可读；同时按 [`keep_value`] 放过显然不是秘密的值（代码里的类型名、函数调用）
    keep_name: bool,
}

/// 会被当成「凭据名」的键。前面允许带任意 `XXX_` / `xxx.` / `xxx-` 前缀
/// （`DB_PASSWORD`、`GITHUB_TOKEN`、`spring.datasource.password`），后面不允许再接字母，
/// 所以 `password_file=`、`check_password(` 都不会命中。
const SECRET_NAMES: &str = r"(?:[A-Za-z0-9_.\-]*[_.\-])?(?:token|api[_-]?key|apikey|access[_-]?key|secret[_-]?key|private[_-]?key|secret|password|passwd|passphrase|pwd|Authorization|auth)";

/// 规则集的匹配顺序是有意义的：前缀型 token（`sk-` / `Bearer` / `AKIA`）在前，
/// 宽泛的邮箱与 IP 在后 —— 否则 IP 规则会先把形如地址的东西吃掉，
/// 让更具体的规则再也匹配不到。
///
/// PEM 私钥不走这里：它是多行结构，且经常被环形缓冲截成半截，
/// 用行扫描器处理更可靠（见 [`redact_pem_blocks`]）。
fn rules() -> &'static [Rule] {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(|| {
        // 值的三种写法（双引号 / 单引号 / 裸值），长度都有上限。
        // regex crate 基于有限自动机、**不支持反向引用**，所以不能用
        // `=(['"]?)X\1` 这种写法来保证引号配对。引号内允许空格但限制在单行内，
        // 否则一处没闭合的引号会吞掉半屏正文。裸值不以 `=` 开头（`password == x` 是比较，不是赋值）。
        let value_eq = r#"(?P<v>"[^"\n]{0,256}"|'[^'\n]{0,256}'|[^\s&"'<>`=][^\s&"'<>`]{0,511})"#;
        // `key: value`（YAML / INI / HTTP 头）：值可以带空格，到行尾为止
        let value_colon = r#"(?P<v>"[^"\n]{0,256}"|'[^'\n]{0,256}'|[^\s"'#][^\r\n]{0,255})"#;
        let kv_eq = format!(r#"(?i)\b{SECRET_NAMES}["']?[ \t]*=[ \t]*{value_eq}"#);
        // 冒号形式只认行首（可带缩进、YAML 列表的 `- `、引号），避免误伤行内的 `fn f(password: &str)`
        let kv_colon = format!(r#"(?im)^[ \t]*(?:-[ \t]+)?["']?{SECRET_NAMES}["']?[ \t]*:[ \t]*{value_colon}"#);
        // 同上，但「行首」是 JSON 字符串里转义的 `\n`（模型的工具参数：`"content":"db:\n  password: x\n"`）。
        // 值到下一个反斜杠 / 引号为止，不吞掉后面的转义与 JSON 结构
        let value_esc = r#"(?P<v>\\"[^"\\\n]{0,256}\\"|[^\s"'#\\][^\r\n\\"]{0,255})"#;
        let kv_colon_esc = format!(r#"(?i)\\n[ \t]*(?:-[ \t]+)?{SECRET_NAMES}[ \t]*:[ \t]*{value_esc}"#);
        // 单行 JSON：`{"password":"x","user":"y"}`
        let kv_json = format!(r#"(?i)"{SECRET_NAMES}"[ \t]*:[ \t]*(?P<v>"[^"\n]{{0,256}}")"#);

        // 环境变量式的名字：大写、前面没有分隔符（`PGPASSWORD`、`NPMTOKEN`），或带 `_KEY_BASE` 一类后缀
        // （`SECRET_KEY_BASE`）。只认大写，免得 `mysecret` 之类的普通词被当成键名
        let kv_env = format!(
            r#"\b(?:[A-Z][A-Z0-9_]*)?(?:PASSWORD|PASSWD|TOKEN|SECRET)(?:_KEY_BASE|_KEY|_BASE)?["']?[ \t]*=[ \t]*{value_eq}"#
        );
        let patterns: Vec<(String, bool)> = vec![
            // URL 里的用户信息：`scheme://user:pw@host`（用户名可空：`redis://:pw@cache`）
            (r#"\b[A-Za-z][A-Za-z0-9+.\-]*://[^\s/:@'"]*:(?P<v>[^\s/@'"]+)@"#.into(), true),
            // `mysql -pSecret` / `-p'Secret'`（-p 后紧跟口令；单独的 `-p` 是交互输入，不动）
            (r#"(?:^|[\s;|&(])(?:mysql|mysqldump|mysqladmin|mariadb|mariadb-dump)\b[^\n]*?[ \t]-p(?P<v>'[^'\n]*'|"[^"\n]*"|[^\s'"\-][^\s]*)"#.into(), true),
            // `sshpass -p 口令`
            (r#"\bsshpass\b[^\n]*?[ \t]-p[ \t]*(?P<v>'[^'\n]*'|"[^"\n]*"|[^\s'"\-][^\s]*)"#.into(), true),
            // `docker login -p 口令` / `--password 口令` / `--password=口令`
            (r#"\bdocker\b[^\n]*?\blogin\b[^\n]*?[ \t](?:-p|--password)(?:[ \t]+|=)(?P<v>'[^'\n]*'|"[^"\n]*"|[^\s'"\-][^\s]*)"#.into(), true),
            // openssl `-passin pass:口令` / `-passout pass:口令`
            (r#"-pass(?:in|out)[ \t]+pass:(?P<v>'[^'\n]*'|"[^"\n]*"|[^\s'"]+)"#.into(), true),
            // XML / HTML：`<password>口令</password>`、`<secret>`、`<token>`、`<apiKey>`
            (r#"(?i)<(?:password|passwd|secret|token|api[_-]?key|access[_-]?key|client[_-]?secret)>(?P<v>[^<\n]{1,256})</"#.into(), true),
            // `curl -u user:pw` / `--user user:pw` / `--user=user:pw`
            (r#"\bcurl\b[^\n]*?[ \t](?:-u|--user)(?:[ \t]+|=)["']?[^\s:'"]*:(?P<v>[^\s'"]+)"#.into(), true),
            // Authorization 头里的 Bearer token（只吃空格 / Tab，不跨行）
            (r#"(?i)bearer[ \t]+[A-Za-z0-9._~+/=\-]{8,}"#.into(), false),
            // OpenAI / Anthropic 风格的前缀 key；前面要是词边界，`task-xxxx` 不算
            (r#"\bsk-[A-Za-z0-9_\-]{16,}"#.into(), false),
            // Stripe 密钥 / 受限密钥（pk_ 是公开的可发布密钥，不算）
            (r#"\b(?:sk|rk)_(?:live|test)_[A-Za-z0-9]{10,}"#.into(), false),
            // GitHub token：ghp_ / gho_ / ghu_ / ghs_ / ghr_ 与细粒度 PAT
            (r#"\bgh[pousr]_[A-Za-z0-9]{30,}"#.into(), false),
            (r#"\bgithub_pat_[A-Za-z0-9_]{22,}"#.into(), false),
            // GitLab PAT
            (r#"\bglpat-[A-Za-z0-9_\-]{20,}"#.into(), false),
            // Slack token
            (r#"\bxox[abposr]-[A-Za-z0-9\-]{10,}"#.into(), false),
            // Google API key
            (r#"\bAIza[0-9A-Za-z_\-]{35}"#.into(), false),
            // JWT：三段 base64url，头部总是 `{"` 编码出来的 eyJ
            (r#"\beyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}"#.into(), false),
            // AWS access key id
            (r#"AKIA[0-9A-Z]{16}"#.into(), false),
            // /etc/shadow 里的 crypt 哈希：$1$ $2b$ $5$ $6$ $y$ $gy$ $7$ $sha1$ …，到 `:` 为止
            (r#"\$(?:1|2[abxy]?|5|6|7|y|gy|sha1|md5)\$[A-Za-z0-9./$=,+\-]{8,}"#.into(), false),
            (kv_json, true),
            (kv_colon, true),
            (kv_colon_esc, true),
            (kv_eq, true),
            (kv_env, true),
            // 邮箱
            (r#"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}"#.into(), false),
            // IPv4（限定每段 0-255）
            (r#"\b(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])(?:\.(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])){3}\b"#.into(), false),
        ];

        patterns
            .into_iter()
            .map(|(pattern, keep_name)| Rule {
                // 这里是程序 bug（正则写错）而不是运行时的外部输入，
                // 且 [all_patterns_compile] 单测会锁住它能通过，故 panic 是合理的
                re: Regex::new(&pattern).unwrap_or_else(|e| {
                    panic!("脱敏正则编译失败，请检查 pattern `{pattern}`：{e}")
                }),
                keep_name,
            })
            .collect()
    })
}

/// 值显然不是秘密：代码里的类型标注 / 字面量 / 已脱敏的占位符 / 变量引用 / 函数调用 / 取值表达式。
/// 只对「键名 + 值」规则生效；宁可漏放这几个固定形状，也别把 `password: &str` 改成乱码。
///
/// 放过（代码，不是口令）：`$VAR` `$(cmd)` `${VAR}`、`name(…`（调用）、`cfg[`（取键）、
/// `a.b.c`（属性路径）、与键同名（`self.secret = secret`）。
/// 仍然脱敏：引号里的字面量（`"hunter2"`）、裸的口令 / 高熵串（`hunter2`、`S3cret`）。
fn keep_value(raw: &str, key: &str) -> bool {
    static SHAPES: OnceLock<[Regex; 3]> = OnceLock::new();
    let [call_open, lookup_open, dotted] = SHAPES.get_or_init(|| {
        [
            Regex::new(r"^[A-Za-z_][A-Za-z0-9_:.]*\(").expect("调用"),
            Regex::new(r"^[A-Za-z_][A-Za-z0-9_.]*\[").expect("取键"),
            Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)+$").expect("属性路径"),
        ]
    });
    let t = raw.trim().trim_end_matches([',', ';']);
    if t.starts_with('$') && t[1..].starts_with(|c: char| c == '(' || c == '{' || c == '_' || c.is_ascii_alphabetic()) {
        return true;
    }
    if call_open.is_match(t) || lookup_open.is_match(t) {
        return true;
    }
    // 属性路径：每段至少两个字符、没有「字母数字混合的长段」（`s.abc123…`、`hvs.CAES…` 是令牌，不是路径）
    if dotted.is_match(t)
        && t.split('.').all(|seg| {
            let mixed = seg.chars().any(|c| c.is_ascii_digit()) && seg.chars().any(|c| c.is_ascii_alphabetic());
            seg.len() >= 2 && !(mixed && seg.len() >= 6)
        })
    {
        return true;
    }
    let name = key.rsplit(|c: char| matches!(c, '.' | '>' | ' ' | '\t')).next().unwrap_or(key);
    // `PWD` / `OLDPWD` 是 shell 的当前 / 上一个目录，值是路径，不是口令
    if (name.eq_ignore_ascii_case("PWD") || name.eq_ignore_ascii_case("OLDPWD"))
        && (t.starts_with('/') || t.starts_with('~') || t.starts_with("\"/") || t.starts_with("'/"))
    {
        return true;
    }
    if !name.is_empty() && t.eq_ignore_ascii_case(name) {
        return true;
    }
    // 函数调用（`password = read_pw();`）：值是代码，不是字面量
    static CALL: OnceLock<Regex> = OnceLock::new();
    let call = CALL.get_or_init(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_:.]*\(.*\)[;,]?$").expect("调用形正则"));
    if call.is_match(raw.trim()) {
        return true;
    }
    let v = raw.trim().trim_end_matches([',', ';', ')', '{']).trim();
    let v = v.trim_start_matches('&').trim_start_matches("mut ").trim();
    if v.is_empty() || v == PLACEHOLDER || v.contains(PLACEHOLDER) {
        return true;
    }
    const WORDS: &[&str] = &[
        "str", "String", "string", "number", "bool", "boolean", "any", "unknown", "None", "null",
        "nil", "undefined", "true", "false", "True", "False", "\"\"", "''", "Option<String>",
        "Option<&str>", "SecretString", "Secret<String>", "Zeroizing<String>",
    ];
    WORDS.contains(&v)
}

/// 对一条「键名 + 值」规则做替换：只换 `v` 组，其余原样。
fn replace_kv(re: &Regex, text: &str) -> String {
    re.replace_all(text, |caps: &Captures| {
        let all = caps.get(0).expect("整体匹配必然存在");
        let Some(v) = caps.name("v") else { return all.as_str().to_string() };
        let s = all.as_str();
        let (a, b) = (v.start() - all.start(), v.end() - all.start());
        // 键名：值前面那段去掉分隔符（`self.secret = ` → `self.secret`）
        let key = s[..a].trim_end_matches(|c: char| matches!(c, '=' | ':' | ' ' | '\t' | '"' | '\'')).trim();
        if keep_value(v.as_str(), key) {
            return all.as_str().to_string();
        }
        format!("{}{PLACEHOLDER}{}", &s[..a], &s[b..])
    })
    .into_owned()
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

/// 一行的「正文部分」：剥掉常见前缀后剩下的 base64 串，不像正文返回 None。
///
/// 能剥掉的前缀（都是工具自己加上去的，不能因为它们让正文漏网）：
/// - grep 结果的 `path:12:`；
/// - `cat -n` / nx_read 的行号 `     5\t`；
/// - diff 的 `+` / `-` 与缩进（YAML 块标量里的密钥）；
/// - JSON 字符串里的引号、尾随的 `\n` 转义与逗号。
///
/// 剥完之后要求剩下的整串都是 base64 字母（不含空格）—— 普通英文句子不会让扫描器误以为还在正文里。
fn body_of(line: &str) -> Option<&str> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"^(?:[^\s:]+:\d+[:\-])?\s*(?:\d+\t)?\s*[+\-]?\s*["']?([A-Za-z0-9+/=]+)(?:\\n)?["']?,?\s*$"#)
            .expect("PEM 正文正则")
    });
    re.captures(line).and_then(|c| c.get(1)).map(|m| m.as_str())
}

/// 找出 PEM 私钥块（含被环形缓冲截断的残块）所在的行，返回 (按行切开的原文, 每行是否删掉)。
///
/// 不用正则（`-----BEGIN [\s\S]*?-----END`）：环形缓冲会把几屏长的私钥切断，
/// 只剩半截是常态，而 base64 正文才是真正的 key，标记行一文不值 ——
/// 正则只认完整块，缺一头就整段失守。
///
/// 故改为按行扫描：`BEGIN` 没等到 `END` 就删到串尾；`END` 前面连着的 base64 正文回溯删掉。
fn pem_lines(text: &str) -> (Vec<&str>, Vec<bool>) {
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
            for d in dropped.iter_mut().take(i + 1).skip(start) {
                *d = true;
            }
            block_start = None;
            saw_begin = false;
            saw_long_body = false;
            continue;
        }

        if let Some(body) = body_of(line) {
            if block_start.is_none() {
                block_start = Some(i);
            }
            if body.len() >= PEM_BODY_MIN_LEN {
                saw_long_body = true;
            }
            continue;
        }

        // 既非标记也非正文 → 本块到此为止。
        // 已经见过 BEGIN 却没等到 END：是被截断的上半截，删掉已确认的部分。
        if saw_begin {
            if let Some(s) = block_start {
                for d in dropped.iter_mut().take(i).skip(s) {
                    *d = true;
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
            for d in dropped.iter_mut().skip(s) {
                *d = true;
            }
        }
    }
    (lines, dropped)
}

/// 剥离 PEM 私钥块。`collapse` 为真时连续被删的行折叠成一个占位符（避免输出几十行 [REDACTED]），
/// 否则每行各换一个占位符、行数不变。
fn redact_pem(text: &str, collapse: bool) -> String {
    let (lines, dropped) = pem_lines(text);
    if !dropped.iter().any(|d| *d) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut in_run = false;
    for (i, line) in lines.iter().enumerate() {
        if dropped[i] {
            if !(collapse && in_run) {
                out.push_str(PLACEHOLDER);
                // 原样保留行尾（含 \r\n）
                out.push_str(&line[line.trim_end_matches(['\n', '\r']).len()..]);
            }
            in_run = true;
        } else {
            in_run = false;
            out.push_str(line);
        }
    }
    out
}

fn redact_pem_blocks(text: &str) -> String {
    redact_pem(text, true)
}

fn apply_rules(mut out: String) -> String {
    for rule in rules() {
        out = if rule.keep_name {
            replace_kv(&rule.re, &out)
        } else {
            rule.re.replace_all(&out, PLACEHOLDER).into_owned()
        };
    }
    out
}

/// 脱敏一段文本，返回替换后的新串。
///
/// `AgentManager::context` 刻意保持原文（UI 要展示），脱敏只在 LLM 请求出网这条边界上做
/// （见 `llm::sanitize_context`）；落盘的对话历史 / 托管 checkpoint / trace / 时间线也都过它。
pub fn redact(text: &str) -> String {
    // PEM 先走：它的 base64 正文不该被后面的 IP / 邮箱规则再点一遍名
    apply_rules(redact_pem_blocks(text))
}

/// 同 [`redact`]，但**行数不变**：被删的 PEM 行各自换成一个占位符。
/// 给文件读取 / grep 用 —— 先对原文脱敏、再编行号，行号仍与真实文件一致。
pub fn redact_lines(text: &str) -> String {
    apply_rules(redact_pem(text, false))
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

    // —— 加强后的规则 ——

    const ED25519: &str = concat!(
        "-----BEGIN OPENSSH PRIVATE KEY-----\n",
        "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n",
        "QyNTUxOQAAACDqVZ1vYw5cX0xr8b7mJ3lK9pQh2Zr4uT1sY8eWnB0dEwAAAJgAAAAAAAAA\n",
        "AAAAAAtzc2gtZWQyNTUxOQAAACDqVZ1vYw5cX0xr8b7mJ3lK9pQh2Zr4uT1sY8eWnB0dEw\n",
        "AAAEB0dGVzdC1rZXktb25seS1ub3QtcmVhbA==\n",
        "-----END OPENSSH PRIVATE KEY-----\n"
    );

    fn assert_no_body(out: &str) {
        for body in ["b3BlbnNzaC1rZXktdjE", "QyNTUxOQAAACDqVZ1v", "AAAAAAtzc2gtZWQy", "AAAEB0dGVzdC1rZXkt"] {
            assert!(!out.contains(body), "正文漏了 {body}：\n{out}");
        }
    }

    /// 文件读取的行号、grep 前缀、diff 的 `+` 都不能让正文漏网
    #[test]
    fn pem_body_with_tool_prefixes_is_caught() {
        let numbered: String =
            ED25519.lines().enumerate().map(|(i, l)| format!("{:>6}\t{l}\n", i + 1)).collect();
        assert_no_body(&redact(&numbered));
        assert_no_body(&redact_lines(&numbered));
        let grep: String = ED25519.lines().enumerate().map(|(i, l)| format!("/home/me/.ssh/id_ed25519:{}:{l}\n", i + 1)).collect();
        assert_no_body(&redact(&grep));
        let diff: String = ED25519.lines().map(|l| format!("+{l}\n")).collect();
        assert_no_body(&redact(&diff));
        let yaml: String = ED25519.lines().map(|l| format!("    {l}\n")).collect();
        assert_no_body(&redact(&format!("tls:\n  key: |\n{yaml}done: 1\n")));
    }

    #[test]
    fn redact_lines_keeps_line_count() {
        let text = format!("before\n{ED25519}after\n");
        let out = redact_lines(&text);
        assert_eq!(out.lines().count(), text.lines().count(), "{out}");
        assert_no_body(&out);
        assert!(out.starts_with("before\n") && out.ends_with("after\n"));
        assert_eq!(out.matches(PLACEHOLDER).count(), 6);
        // CRLF 行尾也原样保留
        let crlf = "x\r\n-----BEGIN RSA PRIVATE KEY-----\r\nMIIEowIBAAKCAQEA1234567890\r\n-----END RSA PRIVATE KEY-----\r\ny\r\n";
        assert_eq!(redact_lines(crlf), format!("x\r\n{0}\r\n{0}\r\n{0}\r\ny\r\n", PLACEHOLDER));
    }

    #[test]
    fn prefixed_and_spaced_secret_names() {
        assert_eq!(redact("DB_PASSWORD=hunter2"), format!("DB_PASSWORD={PLACEHOLDER}"));
        assert_eq!(redact("export GITHUB_TOKEN=abc123"), format!("export GITHUB_TOKEN={PLACEHOLDER}"));
        assert_eq!(redact("db.password = s3cret"), format!("db.password = {PLACEHOLDER}"));
        assert_eq!(redact("[db]\npassword = s3cret\n"), format!("[db]\npassword = {PLACEHOLDER}\n"));
        assert_eq!(redact("db:\n  password: my secret pw\n"), format!("db:\n  password: {PLACEHOLDER}\n"));
        assert_eq!(redact("  - api_key: \"abc\""), format!("  - api_key: {PLACEHOLDER}"));
        assert_eq!(redact(r#"{"user":"a","password":"x y"}"#), format!(r#"{{"user":"a","password":{PLACEHOLDER}}}"#));
        assert_eq!(redact("Secret_Key=AbC"), format!("Secret_Key={PLACEHOLDER}"));
        // 工具参数 JSON 里转义换行后的 YAML
        let args = r#"{"path":"/a.yml","content":"db:\n  password: S3cret pw\n  host: x\n"}"#;
        assert_eq!(
            redact(args),
            format!(r#"{{"path":"/a.yml","content":"db:\n  password: {PLACEHOLDER}\n  host: x\n"}}"#)
        );
    }

    #[test]
    fn code_using_password_as_identifier_survives() {
        let code = "fn check_password(password: &str) -> bool {\n    let ok = verify_password(password);\n    if password == stored { return true; }\n    let password = read_pw();\n    ok\n}\n";
        assert_eq!(redact(code), code);
        let ts = "interface Login {\n  password: string;\n  token?: string;\n}\n";
        assert_eq!(redact(ts), ts);
        assert_eq!(redact("password_file=/run/secrets/db"), "password_file=/run/secrets/db");
        assert_eq!(redact("debug: true\nauth: false\n"), "debug: true\nauth: false\n");
    }

    /// 复审 H1：表驱动。正例的「秘密」子串必须消失；反例必须原样不动。
    #[test]
    fn redaction_table() {
        let positives: &[(&str, &str)] = &[
            ("PGPASSWORD=xyz123 psql -h db", "xyz123"),
            ("export PGPASSWORD=abc987", "abc987"),
            ("MYSQL_PWD=foo42 mysql", "foo42"),
            ("SECRET_KEY_BASE=deadbeef1234cafe", "deadbeef1234cafe"),
            ("DJANGO_SECRET_KEY=abcd1234", "abcd1234"),
            ("NPMTOKEN=npm_xyz777", "npm_xyz777"),
            ("APITOKEN=tok999", "tok999"),
            ("VAULT_TOKEN=s.abcdef123", "s.abcdef123"),
            ("SLACK_WEBHOOK_SECRET=whs3cr3t", "whs3cr3t"),
            ("GH_TOKEN=gho_short1", "gho_short1"),
            ("HEROKU_API_KEY=her0ku", "her0ku"),
            ("SECRET=plainsecret", "plainsecret"),
            ("AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG", "wJalrXUtnFEMI"),
            ("client_secret=cl1ent", "cl1ent"),
            ("mysql -u root -pS3cret db", "S3cret"),
            ("mysql -uroot -p'S3 cret' db", "S3 cret"),
            ("mysqldump --password=S3cret db", "S3cret"),
            ("mariadb -h x -pHunter22", "Hunter22"),
            ("curl -u admin:S3cret https://x", "S3cret"),
            ("curl --user admin:Pw0rd https://x", "Pw0rd"),
            ("curl -s -u 'admin:Qw3rty' https://x", "Qw3rty"),
            ("curl --user=admin:Zx9 https://x", "Zx9"),
            ("postgres://app:S3cretPg@db:5432/app", "S3cretPg"),
            ("mysql://root:MyPw@localhost/db", "MyPw"),
            ("https://user:HttpPw@example.com/path", "HttpPw"),
            ("redis://:RedisPw@cache:6379", "RedisPw"),
            ("git clone https://bob:ghTok3n@git.example.com/r.git", "ghTok3n"),
            ("password=hunter2", "hunter2"),
            ("password: hunter3", "hunter3"),
            (r#"{"password": "jsonpw"}"#, "jsonpw"),
            (r#"PASSWORD="quoted literal""#, "quoted literal"),
            ("api_key = 'abcdef'", "abcdef"),
            (r#"self.password = "litpw""#, "litpw"),
            ("config.password = 'cfgpw'", "cfgpw"),
            (r#"secret = "abc123""#, "abc123"),
            ("Authorization: Basic dXNlcjpwYXNz", "dXNlcjpwYXNz"),
            ("pwd: yamlpw", "yamlpw"),
            ("token=tkn_123", "tkn_123"),
            ("--api_key=secretvalue", "secretvalue"),
            ("db.password = s3cret", "s3cret"),
            (concat!("sk_", "live_51Habcdefghijklmnop"), "51Habcdefghijklmnop"),
            (concat!("STRIPE=sk_", "test_FAKEfakeFAKEfake00000000"), "FAKEfakeFAKEfake00000000"),
            (concat!("rk_", "live_abcdefghijklmn"), "abcdefghijklmn"),
            ("sshpass -p 'Ssh Pw1' ssh root@h", "Ssh Pw1"),
            ("sshpass -pSshPw2 ssh h", "SshPw2"),
            ("docker login -u bob -p DockPw1 registry.io", "DockPw1"),
            ("docker login --username bob --password DockPw2 r.io", "DockPw2"),
            ("docker login --password=DockPw3 r.io", "DockPw3"),
            ("openssl pkcs12 -export -passout pass:SslOut1 -out a.p12", "SslOut1"),
            ("openssl rsa -in k.pem -passin pass:SslIn2", "SslIn2"),
            ("<password>XmlPw1</password>", "XmlPw1"),
            ("<Secret>XmlSec2</Secret>", "XmlSec2"),
            ("<token>XmlTok3</token>", "XmlTok3"),
            ("export PWD=hunterpwd", "hunterpwd"),
        ];
        for (raw, leak) in positives {
            let out = redact(raw);
            assert!(!out.contains(leak), "应脱敏：{raw} → {out}");
            assert!(out.contains(PLACEHOLDER), "{raw} → {out}");
        }
        let negatives: &[&str] = &[
            r#"config.password = cfg.get("db.password")"#,
            "auth = Auth(user, pw)",
            "self.secret = secret",
            "pwd=$(pwd)",
            "PASSWORD=$DB_PASSWORD",
            r#"password = os.environ["DB_PASSWORD"]"#,
            r#"token = request.headers.get("X-Token")"#,
            "secret = settings.SECRET_KEY",
            "password = getpass.getpass()",
            "api_key=${API_KEY}",
            "let password = read_pw();",
            "fn check_password(password: &str) -> bool {",
            "password_file=/run/secrets/db",
            "if password == stored { return true; }",
            "auth: false",
            "git clone https://github.com/x/y.git",
            "TOKEN_TTL=3600",
            "mysql -h db -P 3306 -u root -p",
            r#"curl -u "$USER:$PASS" https://x"#,
            "password = password",
            "export PWD=/home/me",
            "OLDPWD=/var/www",
            "PWD=~/src",
            "pk_live_abcdefghijklmnop",
            "docker login registry.io",
            "sshpass -f /run/secret.txt ssh h",
            "openssl rsa -in k.pem -passin env:KEY_PASS",
            "<password></password>",
        ];
        for raw in negatives {
            assert_eq!(redact(raw), *raw, "不该脱敏");
        }
    }

    #[test]
    fn shadow_hashes_are_redacted() {
        let line = "root:$6$Zr8qBx1L$Qk4pU2c9dYJ0oF1vXxjv5w8m3eTq7Lr2sN6aH4bG9yK1zC0iD5fE8hJ3kM7nP2qR6tV9wX4yZ1:19700:0:99999:7:::";
        let out = redact(line);
        assert!(!out.contains("Qk4pU2c9dYJ0") && !out.contains("Zr8qBx1L"), "{out}");
        assert!(out.starts_with("root:") && out.ends_with(":19700:0:99999:7:::"), "{out}");
        let y = redact("me:$y$j9T$3Lx9Qn1hV0pS$Wb8sT2kq/E4uM9fR1dY7cN0zA6gH3jL5pX2vB8nK1mQ:19800:0:99999:7:::");
        assert!(!y.contains("Wb8sT2kq"), "{y}");
        // 锁定账户 / 无口令不受影响
        assert_eq!(redact("daemon:*:19000:0:99999:7:::"), "daemon:*:19000:0:99999:7:::");
    }

    #[test]
    fn common_token_formats() {
        for (raw, leak) in [
            ("ghp_0123456789abcdefghijABCDEFGHIJ012345", "0123456789abcdefghij"),
            ("github_pat_11ABCDEFG0123456789_abcdefghijklmnopqrstuvwxyz", "abcdefghijklmnop"),
            ("xoxb-1234567890-abcdefghijkl", "abcdefghijkl"),
            ("glpat-abcdefghij0123456789", "abcdefghij0123"),
            ("eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U", "dozjgNryP4J3"),
        ] {
            let out = redact(&format!("value {raw} end"));
            assert!(!out.contains(leak), "{raw} → {out}");
            assert_eq!(out, format!("value {PLACEHOLDER} end"));
        }
        // 太短 / 嵌在词里的 sk- 不算
        assert_eq!(redact("task-runner sk-short"), "task-runner sk-short");
    }
}
