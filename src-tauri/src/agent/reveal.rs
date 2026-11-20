//! 交付通道（`nx_reveal`）：把 Agent 生成的密码 / UUID / 密钥 / 节点链接这类**用户要、模型不该看**的结果
//! 直接交到用户手里。与 `nx_secret`（用户 → pty，值不经模型）方向相反：目标机 → 用户，值同样不经模型。
//!
//! ```text
//! 模型 nx_reveal{title, path | command, target?}
//!   └─ 后端取内容（文件：nx_read 同一套 transport，本地 / SFTP；命令：nx_run 同一条旁路通道，60s）
//!        ├─ 内容 → `kind:"reveal"` 步骤卡的 `reveal_content` → 前端「交付给你」卡（默认掩码，只在内存）
//!        └─ 模型只拿回执：「已把「标题」交给用户（N 行，M 字节），内容没有经过你」
//! ```
//!
//! 内容**不进**：`messages` / `feedback`（只有 [`receipt`] / [`failure_feedback`]，两者都不接收内容）、
//! 时间线、运行摘要（`history::RunLog::record` 不认 `reveal`）、决策记录（`record::describe_write` 只记标题 + 来源）、
//! trace、托管检查点，以及前端的对话历史（`agentHistory.toPersisted` 落盘前丢掉 `revealContent`）。
//!
//! 限制：最多 [`MAX_BYTES`] 字节，拒绝二进制（含 NUL / 非 UTF-8）和空内容；命令必须退出码 0
//! （失败时**不交付、也不把输出给模型** —— 失败输出里同样可能有秘密）。

use super::files::{FileError, FileOps};
use super::files_tool::{Approver, WriteApproval};
use super::llm;

pub const TOOL_REVEAL: &str = "nx_reveal";
/// 一次最多交付多少字节
pub const MAX_BYTES: usize = 64 * 1024;
/// `command` 来源的超时（秒）
pub const CMD_TIMEOUT_SECS: u64 = 60;
/// 标题最多多少字
const TITLE_MAX: usize = 80;
/// 标题缺省
const DEFAULT_TITLE: &str = "交付内容";

/// 内容从哪儿来。`Invalid` = 参数不合法（两个都给 / 都没给），执行层直接把原因反馈给模型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevealSource {
    Path(String),
    Command(String),
    Invalid(String),
}

impl RevealSource {
    /// 卡片 / 决策记录 / 回执里的「来源」一句话（只有路径或命令本身，没有内容）
    pub fn describe(&self) -> String {
        match self {
            Self::Path(p) => format!("文件 {p}"),
            Self::Command(c) => format!("命令 `{}`", one_line(c, 200)),
            Self::Invalid(_) => "参数无效".to_string(),
        }
    }
}

fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &flat[..i]),
        None => flat,
    }
}

/// 给模型的工具声明。
pub fn tool_spec() -> llm::ToolSpec {
    llm::ToolSpec {
        name: TOOL_REVEAL,
        description: "**交付通道**：把用户需要、但你不该看到的结果（生成的密码 / UUID / 密钥 / 令牌 / 节点链接 / 订阅地址 / 二维码文本等）\
                      直接交给用户本人。后端读取文件或旁路执行命令，把内容放进面板上的「交付给你」卡片（默认打码，用户点「显示」/「复制」）；\
                      **内容不会经过你**，你只会收到一句回执（标题、行数、字节数）。\
                      做法：先把凭据写进权限收紧的文件（如 `umask 077` 后写入、或 chmod 600），再用 path 指向它；\
                      或给一条只把它们打印出来的 command。path 与 command 必须二选一。\
                      上限 64 KB，只交付文本；command 必须非交互、60 秒内以退出码 0 结束（失败时不交付，也不会把输出给你）。\
                      **绝不要**用 nx_exec / nx_run / nx_read 把这类秘密打印给自己看，也不要把它们写进回复或 nx_complete 的总结；\
                      之后提到它们只说「已通过『交付给你』卡片交付」。".into(),
        schema: serde_json::json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "卡片标题：告诉用户这是什么，例如「sing-box 节点链接」「数据库 root 密码」。不要把秘密本身写进标题。"
                },
                "path": {
                    "type": "string",
                    "description": "目标机上的文件（绝对路径、~ 开头，或相对终端当前目录）。与 command 二选一。"
                },
                "command": {
                    "type": "string",
                    "description": "旁路执行的一条非交互命令，其输出（stdout+stderr）就是交付内容，如 `cat /etc/sing-box/links.txt`。与 path 二选一。"
                },
                "target": {
                    "type": "string",
                    "description": "目标会话 id。省略表示用户当前所在的会话。"
                }
            },
            "required": ["title"]
        }),
    }
}

/// 解析工具参数：(target, 标题, 来源)。标题清洗成单行、限长，缺省给一个通用名。
pub fn parse(args: &serde_json::Value) -> (Option<String>, String, RevealSource) {
    let s = |k: &str| args.get(k).and_then(|v| v.as_str()).map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    let target = s("target");
    let title = s("title").map(|t| one_line(&t, TITLE_MAX)).unwrap_or_else(|| DEFAULT_TITLE.to_string());
    let source = match (s("path"), s("command")) {
        (Some(p), None) => RevealSource::Path(p),
        (None, Some(c)) => RevealSource::Command(c),
        (Some(_), Some(_)) => RevealSource::Invalid("path 与 command 只能给一个".into()),
        (None, None) => RevealSource::Invalid("缺少 path 或 command（二选一）".into()),
    };
    (target, title, source)
}

/// 为什么没交付。**任何一个变体都不带内容**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevealErr {
    /// 什么都没有（或全是空白）
    Empty,
    /// 超过上限（字节数）
    TooLarge(u64),
    /// 二进制 / 非 UTF-8
    Binary,
    /// 受保护文件，用户没批准
    Denied(String),
    /// 文件层报错（不存在 / 是目录 / 路径无效 / 传输出错），文案来自 `FileError`（只有路径与原因）
    File(String),
    /// 命令退出码非 0 / 拿不到退出码
    Exit(Option<i32>),
    /// 没有旁路通道
    Unavailable,
    Timeout,
    Cancelled,
    /// 旁路通道出错。原文只给用户看（可能夹带输出），模型只拿到「出错了」
    Failed(String),
}

impl RevealErr {
    /// 给模型的一句话原因（不含任何内容；`Failed` 的原文也不给）
    fn for_model(&self) -> String {
        match self {
            Self::Empty => "内容为空".into(),
            Self::TooLarge(n) => format!("内容过大（{n} 字节，上限 {MAX_BYTES} 字节）：请只交付用户真正需要的那几行"),
            Self::Binary => "内容看起来是二进制（含 NUL 或不是 UTF-8 文本），只能交付文本".into(),
            Self::Denied(p) => format!("「{p}」是受保护文件，用户没有批准读取"),
            Self::File(e) => e.clone(),
            Self::Exit(Some(rc)) => format!("命令退出码 {rc}（失败）。为免泄露，失败时的输出既不交付也不给你看：请先用不打印秘密的命令排查"),
            Self::Exit(None) => "没拿到命令的退出码（可能被截断或让 shell 提前退出），为稳妥起见没有交付".into(),
            Self::Unavailable => "这个会话没有可用的旁路执行通道（SSH 兼容模式 / 堡垒机 / 本平台不支持）：请改用 path 交付一个文件".into(),
            Self::Timeout => format!("命令 {CMD_TIMEOUT_SECS} 秒内没有结束"),
            Self::Cancelled => "用户取消了这一轮".into(),
            Self::Failed(_) => "旁路通道出错（连接问题），没有交付".into(),
        }
    }

    /// 给用户看的原因（卡片上）
    pub fn for_user(&self) -> String {
        match self {
            Self::Failed(e) => format!("旁路通道出错：{e}"),
            other => other.for_model(),
        }
    }
}

/// 内容是否能交付：非空、不超限、是文本。返回 UTF-8 文本。
pub fn check_content(bytes: Vec<u8>) -> Result<String, RevealErr> {
    if bytes.len() > MAX_BYTES {
        return Err(RevealErr::TooLarge(bytes.len() as u64));
    }
    if bytes.contains(&0) {
        return Err(RevealErr::Binary);
    }
    let text = String::from_utf8(bytes).map_err(|_| RevealErr::Binary)?;
    if text.trim().is_empty() {
        return Err(RevealErr::Empty);
    }
    Ok(text)
}

/// 命令来源：退出码必须是 0；去掉旁路脚本在开头打的 `[nexus] …` 说明行（那是我们加的，不是内容）。
pub fn command_content(out: &str, rc: Option<i32>) -> Result<String, RevealErr> {
    if rc != Some(0) {
        return Err(RevealErr::Exit(rc));
    }
    let mut rest = out;
    while rest.starts_with("[nexus] ") {
        rest = rest.split_once('\n').map_or("", |(_, r)| r);
    }
    check_content(rest.as_bytes().to_vec())
}

/// 文件来源：走 nx_read 同一套 transport 与禁读清单。受保护文件出一张写明路径的「读取受保护路径」卡，
/// 批准后才读（内容照样只给用户）。返回 (规整后的路径, 文本)。
pub async fn fetch_file<A: Approver>(ops: &FileOps<'_>, path: &str, approver: &mut A) -> Result<(String, String), RevealErr> {
    let got = match ops.read_raw(path, MAX_BYTES, false).await {
        Err(FileError::ReadDenied { path, size }) => {
            let req = WriteApproval {
                path: path.clone(),
                summary: format!("读取受保护路径 {path}，交付给你"),
                diff: String::new(),
                reasons: vec![format!(
                    "「{path}」在受保护清单里（密钥 / 凭据，{size} 字节）：批准即允许读取这一次，内容只放进「交付给你」卡片，不会给模型"
                )],
                sensitive_path: false,
                read: true,
                preview_complete: true,
            };
            if !approver.approve(&req).await {
                return Err(RevealErr::Denied(path));
            }
            ops.read_raw(&path, MAX_BYTES, true).await
        }
        other => other,
    };
    let (resolved, bytes) = got.map_err(|e| match e {
        FileError::TooLarge { size, .. } => RevealErr::TooLarge(size),
        FileError::Binary(_) => RevealErr::Binary,
        FileError::ReadDenied { path, .. } => RevealErr::Denied(path),
        other => RevealErr::File(other.to_string()),
    })?;
    Ok((resolved, check_content(bytes)?))
}

/// 交给用户的内容（步骤卡上的 `revealContent`）。序列化透明（前端拿到原文），
/// 但 `Debug` 只打印字节数 —— 任何 `{:?}` 日志 / trace 都不会把内容带出去。
#[derive(Clone, serde::Serialize)]
#[serde(transparent)]
pub struct RevealContent(pub String);

impl std::fmt::Debug for RevealContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RevealContent(<已隐藏，{} 字节>)", self.0.len())
    }
}

/// 「N 行，M 字节」
pub fn size_note(content: &str) -> String {
    format!("{} 行，{} 字节", content.lines().count(), content.len())
}

/// 交付成功后喂回模型的回执。**签名里就没有内容**：只拿行数与字节数。
pub fn receipt(title: &str, lines: usize, bytes: usize) -> String {
    format!(
        "已把「{title}」交给用户（{lines} 行，{bytes} 字节），内容没有经过你。\
         之后提到它只说「{title}」已通过『交付给你』卡片交付，不要复述、猜测或重新打印其中的值。\n"
    )
}

/// 没交付时喂回模型的话（不含内容）
pub fn failure_feedback(title: &str, source: &RevealSource, err: &RevealErr) -> String {
    format!("「{title}」（{}）没有交付给用户：{}。\n", source.describe(), err.for_model())
}

/// 决策记录 / 门禁卡上的一句话：只有标题与来源
pub fn decision_text(title: &str, source: &RevealSource) -> String {
    format!("交付给用户「{title}」（{}）", source.describe())
}

#[cfg(test)]
mod tests {
    use super::super::act::AgentAction;
    use super::super::files::fake::FakeTransport;
    use super::super::files::FileOpsConfig;
    use super::*;
    use std::future::Future;

    struct Ask {
        answer: bool,
        asked: Vec<WriteApproval>,
    }

    impl Approver for Ask {
        fn approve(&mut self, req: &WriteApproval) -> impl Future<Output = bool> + Send {
            self.asked.push(req.clone());
            let a = self.answer;
            async move { a }
        }
    }

    fn ask(answer: bool) -> Ask {
        Ask { answer, asked: Vec::new() }
    }

    const SECRET: &str = "vless://1b6f3c2e-9d1a-4c8e-b0f2-7a3d5e9c1f42@203.0.113.7:443?security=reality&pbk=Zx9Kq3vT\n";

    #[test]
    fn parse_requires_exactly_one_source() {
        let (t, title, src) = parse(&serde_json::json!({"title": " 节点\n链接 ", "path": "/etc/x", "target": "s1"}));
        assert_eq!((t.as_deref(), title.as_str()), (Some("s1"), "节点 链接"));
        assert_eq!(src, RevealSource::Path("/etc/x".into()));
        let (_, _, src) = parse(&serde_json::json!({"title": "a", "command": "cat x"}));
        assert_eq!(src, RevealSource::Command("cat x".into()));
        let (_, _, src) = parse(&serde_json::json!({"title": "a", "command": "cat x", "path": "/x"}));
        assert!(matches!(src, RevealSource::Invalid(_)));
        let (_, title, src) = parse(&serde_json::json!({"path": " ", "command": ""}));
        assert!(matches!(src, RevealSource::Invalid(_)));
        assert_eq!(title, DEFAULT_TITLE);
    }

    #[test]
    fn receipt_and_failures_carry_no_content() {
        let content = SECRET.to_string();
        let r = receipt("节点链接", content.lines().count(), content.len());
        assert!(r.starts_with(&format!("已把「节点链接」交给用户（1 行，{} 字节），内容没有经过你", content.len())), "{r}");
        for leak in ["vless", "1b6f3c2e", "203.0.113.7", "Zx9Kq3vT"] {
            assert!(!r.contains(leak), "{r}");
        }
        // 旁路通道的出错原文可能夹带输出：只给用户，不给模型
        let e = RevealErr::Failed(format!("超时（{SECRET}）"));
        let fb = failure_feedback("节点链接", &RevealSource::Command("cat /etc/l".into()), &e);
        assert!(!fb.contains("1b6f3c2e") && fb.contains("没有交付"), "{fb}");
        assert!(e.for_user().contains("1b6f3c2e"));
    }

    #[test]
    fn size_and_binary_limits() {
        assert_eq!(check_content(b"  \n".to_vec()), Err(RevealErr::Empty));
        assert_eq!(check_content(vec![b'a'; MAX_BYTES + 1]), Err(RevealErr::TooLarge(MAX_BYTES as u64 + 1)));
        assert!(check_content(vec![b'a'; MAX_BYTES]).is_ok());
        assert_eq!(check_content(b"ab\0cd".to_vec()), Err(RevealErr::Binary));
        assert_eq!(check_content(vec![0xff, 0xfe, b'a']), Err(RevealErr::Binary));
        assert_eq!(check_content("密码：x".as_bytes().to_vec()).unwrap(), "密码：x");
    }

    #[test]
    fn command_needs_rc_zero_and_drops_our_banner() {
        assert_eq!(command_content(SECRET, Some(1)), Err(RevealErr::Exit(Some(1))));
        assert_eq!(command_content(SECRET, None), Err(RevealErr::Exit(None)));
        let out = format!("[nexus] 不知道终端当前目录，在 ~ 下执行\n{SECRET}");
        assert_eq!(command_content(&out, Some(0)).unwrap(), SECRET);
        // 失败的原因里没有输出
        let fb = failure_feedback("x", &RevealSource::Command("c".into()), &RevealErr::Exit(Some(2)));
        assert!(fb.contains("退出码 2") && !fb.contains("vless"));
    }

    /// 文件来源：普通文件原样交付（不脱敏，内容只给用户）；超限 / 二进制拒绝；受保护文件要批准
    #[tokio::test]
    async fn fetch_file_raw_with_limits_and_protected_approval() {
        let t = FakeTransport::new("/home/me");
        t.add_file("/home/me/links.txt", SECRET.as_bytes());
        t.add_file("/home/me/big.txt", &vec![b'x'; MAX_BYTES + 10]);
        t.add_file("/home/me/bin", b"\x7fELF\0\0\x01");
        let key = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQ\n-----END OPENSSH PRIVATE KEY-----\n";
        t.add_file("/home/me/.ssh/id_ed25519", key.as_bytes());
        let ops = FileOps::new(&t, FileOpsConfig::default());

        let mut a = ask(true);
        let (p, c) = fetch_file(&ops, "~/links.txt", &mut a).await.unwrap();
        assert_eq!((p.as_str(), c.as_str()), ("/home/me/links.txt", SECRET));
        assert!(a.asked.is_empty(), "普通文件不问人");
        assert_eq!(fetch_file(&ops, "/home/me/big.txt", &mut a).await, Err(RevealErr::TooLarge(MAX_BYTES as u64 + 10)));
        assert_eq!(fetch_file(&ops, "/home/me/bin", &mut a).await, Err(RevealErr::Binary));
        assert!(matches!(fetch_file(&ops, "/home/me/nope", &mut a).await, Err(RevealErr::File(_))));

        let mut no = ask(false);
        let r = fetch_file(&ops, "~/.ssh/id_ed25519", &mut no).await;
        assert_eq!(r, Err(RevealErr::Denied("/home/me/.ssh/id_ed25519".into())));
        assert_eq!(no.asked.len(), 1);
        assert!(no.asked[0].read && no.asked[0].title().contains("/home/me/.ssh/id_ed25519"));
        let mut yes = ask(true);
        let (_, c) = fetch_file(&ops, "~/.ssh/id_ed25519", &mut yes).await.unwrap();
        assert_eq!(c, key, "批准后原样交给用户（不经模型，所以不脱敏）");
    }

    /// 决策记录 / 门禁：动作里根本没有内容字段；记下的是 `reveal` + 标题 + 来源
    #[test]
    fn decision_record_has_title_and_source_only() {
        let a = AgentAction::Reveal {
            target: None,
            title: "节点链接".into(),
            source: RevealSource::Command("cat /etc/sing-box/links.txt".into()),
        };
        let (kind, text) = super::super::host::record::describe_write(&a).expect("交付要落决策记录");
        assert_eq!(kind, "reveal");
        assert!(text.contains("节点链接") && text.contains("cat /etc/sing-box/links.txt"), "{text}");
        let a = AgentAction::Reveal { target: None, title: "t".into(), source: RevealSource::Path("/x".into()) };
        assert_eq!(super::super::host::record::describe_write(&a).unwrap().1, "交付给用户「t」（文件 /x）");
        let bad = AgentAction::Reveal { target: None, title: "t".into(), source: RevealSource::Invalid("x".into()) };
        assert!(super::super::host::record::describe_write(&bad).is_none());
    }

    /// 交付卡经 `emit_step` 时会顺路喂给运行摘要：摘要里不能有内容
    #[test]
    fn run_log_never_keeps_revealed_content() {
        let mut log = super::super::history::RunLog::new("装 sing-box");
        log.record(&super::super::act::StepPayload {
            command: Some("命令 `cat /etc/sing-box/links.txt`".into()),
            text: Some("节点链接".into()),
            output: Some("1 行，120 字节".into()),
            reveal_content: Some(RevealContent(SECRET.into())),
            ..super::super::act::StepPayload::new("reveal", 1)
        });
        log.record(&super::super::act::StepPayload { text: Some("好了".into()), ..super::super::act::StepPayload::new("answer", 2) });
        let s = format!("{:?}", log.finish(0));
        assert!(!s.contains("1b6f3c2e") && !s.contains("vless"), "{s}");
    }

    /// 内容只经序列化（事件）到前端；`{:?}`（tracing / trace / panic 信息）里看不到
    #[test]
    fn step_payload_serializes_content_but_debug_hides_it() {
        let p = super::super::act::StepPayload {
            text: Some("节点链接".into()),
            reveal_content: Some(RevealContent(SECRET.into())),
            ..super::super::act::StepPayload::new("reveal", 1)
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["revealContent"], SECRET, "前端要拿到原文");
        let dbg = format!("{p:?}");
        assert!(!dbg.contains("1b6f3c2e") && dbg.contains("已隐藏"), "{dbg}");
    }

    /// 后端落盘对话历史（`agent_history_save` → `sanitize_messages`）再兜一道：带着内容的交付卡也存不进去
    #[test]
    fn saved_panel_history_drops_reveal_content() {
        let msgs = vec![serde_json::json!({
            "id": "m1", "role": "assistant", "text": "", "ts": 1,
            "step": {"kind": "reveal", "step": 1, "text": "节点链接", "revealContent": SECRET}
        })];
        let out = super::super::history::sanitize_messages(msgs);
        let s = serde_json::to_string(&out).unwrap();
        assert!(!s.contains("1b6f3c2e") && s.contains("节点链接"), "{s}");
    }

    /// trace 的「actions」段按 `{:?}` 记动作：动作里只有标题与来源
    #[test]
    fn traced_action_has_no_content_field() {
        let a = AgentAction::Reveal { target: None, title: "节点链接".into(), source: RevealSource::Path("/root/links.txt".into()) };
        assert_eq!(
            format!("{a:?}"),
            r#"Reveal { target: None, title: "节点链接", source: Path("/root/links.txt") }"#
        );
    }

    #[test]
    fn tool_call_parses_into_reveal_action() {
        let call = llm::ToolCall { id: String::new(), name: TOOL_REVEAL.into(), arguments: r#"{"title":"pw","path":"/x"}"#.into() };
        assert_eq!(
            super::super::act::actions_from_tool_call(&call),
            vec![AgentAction::Reveal { target: None, title: "pw".into(), source: RevealSource::Path("/x".into()) }]
        );
        assert!(super::super::act::tool_specs().iter().any(|s| s.name == TOOL_REVEAL));
    }
}
