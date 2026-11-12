//! 文件工具（`nx_read` / `nx_write` / `nx_edit` / `nx_glob` / `nx_grep`）接进 Agent 循环的那一层。
//!
//! `files.rs` 只管「怎么读写文件」；这里管「Agent 什么时候可以写、怎么问人、给模型看什么」：
//! - 写 / 改走两段式：`prepare_*` 得到差异预览 → 需要时出审批卡（卡上带 diff）→ `commit`；
//! - 放行与否由调用方按模式算好传进来（[`WriteGate`]）：对话模式看 exec mode（审批模式下写入从不静默），
//!   托管模式就是 `gate::authorize` 的结论；
//! - 受保护路径（`~/.ssh`、`.env` 之类）只有在用户批准一张**写明该路径**的卡之后才置 `allow_sensitive`，
//!   它从来不是模型参数；
//! - 审批期间文件被改动（Conflict）如实告诉模型，**不自动重试**；
//! - 读到的内容在 `files.rs` 里就已整份脱敏（先脱敏、再编行号）；这里再按 `sensitive` / 账号开关过一遍；
//! - 禁读清单（私钥、`/etc/shadow`、云凭据……）里的路径：不批准只回元数据 +「需要用户批准才能读取」，
//!   批准一张写明该路径的卡之后才读，读到的内容**照样脱敏**；
//! - 差异预览不完整（超过上限 / 原文件过大 / 二进制）的写入：**任何档位都先出审批卡**，卡上写明预览不完整。
//!
//! 审批经 [`Approver`] 注入，整条流程可以用 `files::fake::FakeTransport` 单测。

use std::future::Future;

use serde_json::Value;

use super::files::{
    self, EditArgs, FileChange, FileError, FileOps, GlobArgs, GrepArgs, PendingChange, ReadArgs, WriteArgs,
    WriteOptions,
};
use super::redact;

/// 给模型看的 diff 最多多少字
const DIFF_TO_MODEL_CAP: usize = 4_000;
/// glob / grep 默认最多返回多少条
const DEFAULT_LIST_MAX: usize = 200;

/// 一次文件工具调用（参数沿用 `files.rs` 的结构）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileCall {
    Read(ReadArgs),
    Write(WriteArgs),
    Edit(EditArgs),
    Glob(GlobArgs),
    Grep(GrepArgs),
}

impl FileCall {
    /// 工具名 + 参数 → 调用。参数不合法（缺 path 等）返回 None，由调用方当成「没有产生动作」。
    pub fn parse(tool: &str, args: &Value) -> Option<Self> {
        let a = args.clone();
        Some(match tool {
            files::TOOL_READ => Self::Read(serde_json::from_value(a).ok()?),
            files::TOOL_WRITE => Self::Write(serde_json::from_value(a).ok()?),
            files::TOOL_EDIT => Self::Edit(serde_json::from_value(a).ok()?),
            files::TOOL_GLOB => Self::Glob(serde_json::from_value(a).ok()?),
            files::TOOL_GREP => Self::Grep(serde_json::from_value(a).ok()?),
            _ => return None,
        })
    }

    pub fn tool(&self) -> &'static str {
        match self {
            Self::Read(_) => files::TOOL_READ,
            Self::Write(_) => files::TOOL_WRITE,
            Self::Edit(_) => files::TOOL_EDIT,
            Self::Glob(_) => files::TOOL_GLOB,
            Self::Grep(_) => files::TOOL_GREP,
        }
    }

    pub fn target(&self) -> Option<&str> {
        match self {
            Self::Read(a) => a.target.as_deref(),
            Self::Write(a) => a.target.as_deref(),
            Self::Edit(a) => a.target.as_deref(),
            Self::Glob(a) => a.target.as_deref(),
            Self::Grep(a) => a.target.as_deref(),
        }
    }

    pub fn is_write(&self) -> bool {
        matches!(self, Self::Write(_) | Self::Edit(_))
    }

    /// 主路径（glob / grep 的搜索根可以省略）
    pub fn path(&self) -> Option<&str> {
        match self {
            Self::Read(a) => Some(&a.path),
            Self::Write(a) => Some(&a.path),
            Self::Edit(a) => Some(&a.path),
            Self::Glob(a) => a.path.as_deref(),
            Self::Grep(a) => a.path.as_deref(),
        }
    }

    fn path_mut(&mut self) -> Option<&mut String> {
        match self {
            Self::Read(a) => Some(&mut a.path),
            Self::Write(a) => Some(&mut a.path),
            Self::Edit(a) => Some(&mut a.path),
            Self::Glob(a) => a.path.as_mut(),
            Self::Grep(a) => a.path.as_mut(),
        }
    }

    /// 相对路径按已知的终端当前目录补成绝对路径；不知道当前目录就报错（附怎么办）。
    pub fn absolutize(&mut self, cwd: Option<&str>) -> Result<(), String> {
        if let Some(p) = self.path_mut() {
            *p = absolutize(p, cwd)?;
        }
        Ok(())
    }

    /// 写入要提交的文本行（托管闸门的 deny 关键字 / 硬偏好正则按它匹配）：路径 + 新内容。
    pub fn write_lines(&self) -> Vec<String> {
        match self {
            Self::Write(a) => std::iter::once(a.path.clone()).chain(a.content.lines().map(str::to_string)).collect(),
            Self::Edit(a) => std::iter::once(a.path.clone()).chain(a.new_string.lines().map(str::to_string)).collect(),
            _ => Vec::new(),
        }
    }

    /// 给人看的一句话（卡片标题 / 决策记录）
    pub fn summary(&self) -> String {
        match self {
            Self::Read(a) => format!("读取 {}", a.path),
            Self::Write(a) => format!("写入 {}（{} 字节）", a.path, a.content.len()),
            Self::Edit(a) => format!("修改 {}{}", a.path, if a.replace_all { "（全部替换）" } else { "" }),
            Self::Glob(a) => format!("查找文件 {}{}", a.pattern, a.path.as_deref().map(|p| format!("（在 {p}）")).unwrap_or_default()),
            Self::Grep(a) => format!("搜索 /{}/{}", a.pattern, a.path.as_deref().map(|p| format!("（在 {p}）")).unwrap_or_default()),
        }
    }
}

fn is_absolute_like(p: &str) -> bool {
    p.starts_with('/') || p.starts_with('~') || (p.len() > 2 && p.as_bytes()[1] == b':' && p.as_bytes()[0].is_ascii_alphabetic())
}

/// 绝对路径 / `~` 原样；相对路径拼到 `cwd` 上；`cwd` 未知时报错。
pub fn absolutize(path: &str, cwd: Option<&str>) -> Result<String, String> {
    let p = path.trim();
    if p.is_empty() || is_absolute_like(p) {
        return Ok(p.to_string());
    }
    match cwd.map(str::trim).filter(|c| is_absolute_like(c)) {
        Some(c) => {
            let rel = p.strip_prefix("./").unwrap_or(p);
            Ok(format!("{}/{}", c.trim_end_matches('/'), rel))
        }
        None => Err(format!(
            "「{p}」是相对路径，但不知道终端的当前目录。请改用绝对路径或以 ~ 开头；\
             不确定当前目录可以先 nx_exec 执行 pwd"
        )),
    }
}

/// 写入的放行决定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteGate {
    /// 直接写（对话的自动模式 / 托管闸门 Allow、Notify）
    Auto,
    /// 先出审批卡
    Approve(Vec<String>),
    /// 不写，原因反馈给模型
    Deny(String),
}

/// 审批卡的内容。
#[derive(Debug, Clone)]
pub struct WriteApproval {
    pub path: String,
    pub summary: String,
    /// 差异预览（敏感时已脱敏）
    pub diff: String,
    pub reasons: Vec<String>,
    /// 路径在受保护清单里：批准即允许这一次写入
    pub sensitive_path: bool,
    /// 这是一次**读取**受保护路径的申请（不是写入）：卡片标题应写「读取受保护路径」，`diff` 为空
    pub read: bool,
    /// `diff` 是否展示了全部修改。false 时卡片必须醒目提示「预览不完整」（diff 第一行也带
    /// [`files::DIFF_INCOMPLETE_MARK`]），这种写入在任何档位都会走到这里来问人
    pub preview_complete: bool,
}

impl WriteApproval {
    /// 卡片标题：读 / 写受保护路径都把路径写在标题上，预览不完整要在标题里就说出来。
    /// act.rs 的 `FileApprover` 应当用它（而不是只看 `sensitive_path` 拼「写入受保护路径」）。
    pub fn title(&self) -> String {
        let base = if self.read {
            self.summary.clone()
        } else if self.sensitive_path {
            format!("写入受保护路径 {}：{}", self.path, self.summary)
        } else {
            self.summary.clone()
        };
        if self.preview_complete {
            base
        } else {
            format!("{base}（⚠ 预览不完整：卡上看不到全部修改）")
        }
    }
}

/// 问人。返回 true = 批准。
pub trait Approver {
    fn approve(&mut self, req: &WriteApproval) -> impl Future<Output = bool> + Send;
}

/// 一次调用的结果：给模型的反馈、给界面的卡片数据、真写了什么。
#[derive(Debug, Clone, Default)]
pub struct FileOutcome {
    pub ok: bool,
    /// 给模型（已按需脱敏）
    pub feedback: String,
    /// 卡片一行摘要
    pub summary: String,
    /// 写 / 改的差异（界面展示用，敏感时已脱敏）
    pub diff: Option<String>,
    /// 已落盘的修改
    pub change: Option<FileChange>,
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}\n…（已截断）", s.chars().take(max).collect::<String>())
    }
}

fn fail(summary: String, e: impl std::fmt::Display) -> FileOutcome {
    FileOutcome {
        ok: false,
        feedback: format!("{summary} 失败：{e}\n如果文件工具在这台机器上不可用，可以改用 nx_exec。\n"),
        summary,
        ..Default::default()
    }
}

/// 受保护路径没批准：只给元数据，不提示改用 nx_exec（那等于教模型绕过）
fn protected(summary: String, e: FileError) -> FileOutcome {
    FileOutcome {
        ok: false,
        feedback: format!("{summary} 没有执行：{e}。用户没有批准；不要尝试用其它方式读取它，确有需要请在结论里说明。\n"),
        summary,
        ..Default::default()
    }
}

/// 读文件。禁读清单命中时出一张写明路径的卡，批准后才读（内容在 `files.rs` 里照样已脱敏）。
async fn read<A: Approver>(
    ops: &FileOps<'_>,
    a: &ReadArgs,
    approver: &mut A,
) -> Result<files::ReadResult, FileError> {
    match ops.read(&a.path, a.offset, a.limit).await {
        Err(FileError::ReadDenied { path, size }) => {
            let req = WriteApproval {
                path: path.clone(),
                summary: format!("读取受保护路径 {path}"),
                diff: String::new(),
                reasons: vec![format!(
                    "「{path}」在受保护清单里（密钥 / 凭据，{size} 字节）：批准即允许读取这一次；内容仍会脱敏后才给模型"
                )],
                sensitive_path: false,
                read: true,
                preview_complete: true,
            };
            if !approver.approve(&req).await {
                return Err(FileError::ReadDenied { path, size });
            }
            ops.read_with(&path, a.offset, a.limit, true).await
        }
        other => other,
    }
}

fn shown(text: &str, sensitive: bool, redact_all: bool) -> String {
    if sensitive || redact_all {
        redact::redact(text)
    } else {
        text.to_string()
    }
}

/// 执行一次文件工具调用。写入的放行由 `gate` 决定，问人经 `approver`。
pub async fn run<A: Approver>(
    ops: &FileOps<'_>,
    call: &FileCall,
    gate: WriteGate,
    approver: &mut A,
    redact_all: bool,
) -> FileOutcome {
    let summary = call.summary();
    match call {
        FileCall::Read(a) => match read(ops, a, approver).await {
            Ok(r) => {
                let end = r.start_line + r.lines_returned.saturating_sub(1);
                let head = format!(
                    "文件 {}（第 {}–{} 行，共 {} 行{}{}）：\n",
                    r.path,
                    r.start_line,
                    end,
                    r.total_lines,
                    if r.truncated { "，后面还有，用 offset 接着读" } else { "" },
                    if r.lossy { "，含非法 UTF-8，不要拿它当 nx_edit 的原文" } else { "" },
                );
                FileOutcome {
                    ok: true,
                    feedback: format!("{head}{}", shown(&r.content, r.sensitive, redact_all)),
                    summary: format!("读取 {}（{} 行{}）", r.path, r.lines_returned, if r.sensitive { "，已脱敏" } else { "" }),
                    ..Default::default()
                }
            }
            Err(e @ FileError::ReadDenied { .. }) => protected(summary, e),
            Err(e) => fail(summary, e),
        },
        FileCall::Glob(a) => match ops.glob(a.path.as_deref(), &a.pattern, a.max_results.unwrap_or(DEFAULT_LIST_MAX)).await {
            Ok(r) => FileOutcome {
                ok: true,
                feedback: format!("{}\n", r.to_model_text()),
                summary: format!("{summary}：{} 个", r.files.len()),
                ..Default::default()
            },
            Err(e) => fail(summary, e),
        },
        FileCall::Grep(a) => match ops
            .grep(a.path.as_deref(), &a.pattern, a.glob.as_deref(), a.case_insensitive, a.max_results.unwrap_or(DEFAULT_LIST_MAX))
            .await
        {
            Ok(r) => FileOutcome {
                ok: true,
                feedback: shown(&r.to_model_text(), r.sensitive, redact_all),
                summary: format!("{summary}：{} 处", r.matches.len()),
                ..Default::default()
            },
            Err(e @ FileError::ReadDenied { .. }) => protected(summary, e),
            Err(e) => fail(summary, e),
        },
        FileCall::Write(_) | FileCall::Edit(_) => write(ops, call, gate, approver, redact_all, summary).await,
    }
}

/// 写入内容里的脱敏占位符比原来多（写回了 nx_read 给的脱敏内容）：返回拒绝理由。
/// - nx_edit：new_string 里的占位符多于 old_string；
/// - nx_write：差异里新增行的占位符多于删除行；预览不完整、看不全差异时，内容里出现占位符就拒。
pub(crate) fn placeholder_wipe(call: &FileCall, pending: &PendingChange) -> Option<String> {
    let n = |t: &str| t.matches(redact::PLACEHOLDER).count();
    let more = match call {
        FileCall::Edit(a) => n(&a.new_string) > n(&a.old_string),
        FileCall::Write(a) => {
            if n(&a.content) == 0 {
                false
            } else if !pending.preview_complete {
                true
            } else {
                let (mut added, mut removed) = (0, 0);
                for l in pending.change.diff_preview.lines() {
                    if l.starts_with("+++") || l.starts_with("---") {
                        continue;
                    }
                    if let Some(r) = l.strip_prefix('+') {
                        added += n(r);
                    } else if let Some(r) = l.strip_prefix('-') {
                        removed += n(r);
                    }
                }
                added > removed
            }
        }
        _ => false,
    };
    more.then(|| {
        format!(
            "内容里有脱敏占位符 {}，写入会抹掉真实密钥；请用 nx_edit 只改需要改的行，不要整文件重写",
            redact::PLACEHOLDER
        )
    })
}

async fn prepare(ops: &FileOps<'_>, call: &FileCall, allow_sensitive: bool) -> Result<PendingChange, FileError> {
    match call {
        FileCall::Write(a) => {
            ops.prepare_write(&a.path, &a.content, &WriteOptions { create_dirs: a.create_dirs, allow_sensitive }).await
        }
        FileCall::Edit(a) => ops.prepare_edit(&a.path, &a.old_string, &a.new_string, a.replace_all, allow_sensitive).await,
        _ => Err(FileError::InvalidArg("不是写入".into())),
    }
}

async fn write<A: Approver>(
    ops: &FileOps<'_>,
    call: &FileCall,
    gate: WriteGate,
    approver: &mut A,
    redact_all: bool,
    summary: String,
) -> FileOutcome {
    if let WriteGate::Deny(why) = &gate {
        return FileOutcome {
            ok: false,
            feedback: format!("{summary} 没有执行：{why}\n"),
            summary,
            ..Default::default()
        };
    }
    // 受保护路径：先按「允许」算出预览给人看（不落盘），批准一张写明该路径的卡之后才提交
    let (pending, sensitive_path) = match prepare(ops, call, false).await {
        Ok(p) => (p, false),
        Err(FileError::Denied(_)) => match prepare(ops, call, true).await {
            Ok(p) => (p, true),
            Err(e) => return fail(summary, e),
        },
        Err(e) => return fail(summary, e),
    };
    let ch = &pending.change;
    // 脱敏占位符写回去 = 抹掉真实密钥（模型读到的是脱敏后的内容，整文件重写会把 [REDACTED] 原样写进去）
    if let Some(why) = placeholder_wipe(call, &pending) {
        return FileOutcome {
            ok: false,
            feedback: format!("{summary} 没有执行：{why}\n"),
            summary,
            diff: Some(redact::redact(&ch.diff_preview)),
            ..Default::default()
        };
    }
    // 审批卡上给人看原文（人看得到自己的密钥，脱敏后的 diff 会把「改成占位符」藏起来）；
    // 进步骤卡 / 时间线 / 运行记录 / 落盘历史的那份一律脱敏，给模型的按开关与敏感标记脱敏
    let card_diff = ch.diff_preview.clone();
    let diff = redact::redact(&ch.diff_preview);
    let mut reasons = match &gate {
        WriteGate::Approve(r) => r.clone(),
        _ => Vec::new(),
    };
    if sensitive_path {
        reasons.push(format!("「{}」在受保护清单里（密钥 / 凭据 / 系统配置）：批准即允许这一次写入", ch.path));
    }
    // 预览不完整：人看不到全部将要写入的内容，自动档位也不能静默落盘
    let complete = pending.preview_complete;
    if !complete {
        reasons.push("预览不完整：差异过长、原文件过大或是二进制，卡上看不到全部修改；批准即按完整内容写入".into());
    }
    if sensitive_path || !complete || matches!(gate, WriteGate::Approve(_)) {
        let req = WriteApproval {
            path: ch.path.clone(),
            summary: summary.clone(),
            diff: card_diff,
            reasons,
            sensitive_path,
            read: false,
            preview_complete: complete,
        };
        if !approver.approve(&req).await {
            return FileOutcome {
                ok: false,
                feedback: format!("{summary} 没有执行：用户没有批准。换个做法，或在结论里说明需要他本人修改。\n"),
                summary,
                diff: Some(diff),
                ..Default::default()
            };
        }
    }
    match ops.commit(&pending).await {
        Ok(done) => {
            let mut fb = format!(
                "已{}{}（{} 字节{}{}）。差异：\n{}\n",
                if done.created { "新建" } else { "写入" },
                done.path,
                done.bytes,
                if done.replacements > 0 { format!("，替换 {} 处", done.replacements) } else { String::new() },
                if done.atomic { "" } else { "，远端不支持原子改名，走了备份 → 改名" },
                clip(&shown(&done.diff_preview, done.sensitive || sensitive_path || redact_all, false), DIFF_TO_MODEL_CAP),
            );
            if !done.atomic {
                fb.push_str("（落盘不是原子的：中间有极短窗口目标文件不存在。）\n");
            }
            FileOutcome { ok: true, feedback: fb, summary, diff: Some(diff), change: Some(done) }
        }
        Err(FileError::Conflict(p)) => FileOutcome {
            ok: false,
            feedback: format!(
                "「{p}」在审批期间被改动过，这次修改没有写入（也不会自动重试）。请重新 nx_read 看清现状后再决定怎么改。\n"
            ),
            summary,
            diff: Some(diff),
            ..Default::default()
        },
        Err(e) => fail(summary, e),
    }
}

#[cfg(test)]
mod tests {
    use super::super::files::fake::FakeTransport;
    use super::super::files::FileOpsConfig;
    use super::*;

    /// 记下问过什么，按预设回答。
    struct Ask {
        answer: bool,
        asked: Vec<WriteApproval>,
        /// 问的时候顺手改一下文件（模拟审批期间别人动了它）
        tamper: Option<Box<dyn Fn() + Send>>,
    }

    impl Approver for Ask {
        fn approve(&mut self, req: &WriteApproval) -> impl Future<Output = bool> + Send {
            self.asked.push(req.clone());
            if let Some(t) = &self.tamper {
                t();
            }
            let a = self.answer;
            async move { a }
        }
    }

    fn ask(answer: bool) -> Ask {
        Ask { answer, asked: Vec::new(), tamper: None }
    }

    fn write_call(path: &str, content: &str) -> FileCall {
        FileCall::Write(WriteArgs { target: None, path: path.into(), content: content.into(), create_dirs: false })
    }

    #[tokio::test]
    async fn approval_mode_write_parks_and_commits_on_approve() {
        let t = FakeTransport::new("/home/me");
        let ops = FileOps::new(&t, FileOpsConfig::default());
        let mut a = ask(true);
        let out = run(&ops, &write_call("/home/me/run.sh", "#!/bin/sh\necho hi\n"), WriteGate::Approve(vec!["审批模式".into()]), &mut a, false).await;
        assert!(out.ok, "{}", out.feedback);
        assert_eq!(a.asked.len(), 1, "审批模式下写入从不静默");
        assert!(a.asked[0].diff.contains("+echo hi"), "卡上带 diff：{}", a.asked[0].diff);
        assert_eq!(t.get("/home/me/run.sh").unwrap(), b"#!/bin/sh\necho hi\n");
        assert!(out.feedback.contains("已新建"));
    }

    #[tokio::test]
    async fn denied_or_declined_writes_nothing() {
        let t = FakeTransport::new("/home/me");
        let ops = FileOps::new(&t, FileOpsConfig::default());
        let out = run(&ops, &write_call("/home/me/a.txt", "x"), WriteGate::Deny("契约禁止".into()), &mut ask(true), false).await;
        assert!(!out.ok && out.feedback.contains("契约禁止"));
        assert!(t.get("/home/me/a.txt").is_none());
        let mut a = ask(false);
        let out = run(&ops, &write_call("/home/me/a.txt", "x"), WriteGate::Approve(vec![]), &mut a, false).await;
        assert!(!out.ok && out.feedback.contains("没有批准"));
        assert!(t.get("/home/me/a.txt").is_none());
    }

    #[tokio::test]
    async fn conflict_on_commit_is_reported_not_retried() {
        let t = std::sync::Arc::new(FakeTransport::new("/home/me"));
        t.add_file("/home/me/conf.ini", b"a=1\n");
        let ops = FileOps::new(&*t, FileOpsConfig::default());
        let t2 = t.clone();
        let mut a = Ask { answer: true, asked: vec![], tamper: Some(Box::new(move || t2.add_file("/home/me/conf.ini", b"a=9\n"))) };
        let call = FileCall::Edit(EditArgs {
            target: None,
            path: "/home/me/conf.ini".into(),
            old_string: "a=1".into(),
            new_string: "a=2".into(),
            replace_all: false,
        });
        let out = run(&ops, &call, WriteGate::Approve(vec![]), &mut a, false).await;
        assert!(!out.ok);
        assert!(out.feedback.contains("审批期间被改动") && out.feedback.contains("不会自动重试"), "{}", out.feedback);
        assert_eq!(t.get("/home/me/conf.ini").unwrap(), b"a=9\n", "别人的修改原样保留");
        assert_eq!(a.asked.len(), 1);
    }

    #[tokio::test]
    async fn sensitive_path_needs_a_card_naming_it_even_in_auto() {
        let t = FakeTransport::new("/home/me");
        t.add_dir("/home/me/.ssh");
        let ops = FileOps::new(&t, FileOpsConfig::default());
        let mut a = ask(false);
        let out = run(&ops, &write_call("/home/me/.ssh/config", "Host x\n"), WriteGate::Auto, &mut a, false).await;
        assert!(!out.ok && t.get("/home/me/.ssh/config").is_none());
        assert_eq!(a.asked.len(), 1);
        assert!(a.asked[0].sensitive_path && a.asked[0].reasons[0].contains("/home/me/.ssh/config"));
        let mut a = ask(true);
        let out = run(&ops, &write_call("/home/me/.ssh/config", "Host x\n"), WriteGate::Auto, &mut a, false).await;
        assert!(out.ok && t.get("/home/me/.ssh/config").is_some(), "批准写明路径的卡之后才写");
    }

    #[tokio::test]
    async fn rewriting_redacted_content_is_refused_and_file_unchanged() {
        let t = FakeTransport::new("/home/me");
        let original = b"DB_HOST=db\nDB_PASSWORD=hunter2hunter2\nOPENAI_API_KEY=sk-proj-abcdefghijklmnopqrstuvwxyz1234\n";
        t.add_file("/home/me/app/.env", original);
        let ops = FileOps::new(&t, FileOpsConfig::default());
        // 模型读到的是脱敏后的内容
        let read = FileCall::Read(ReadArgs { target: None, path: "/home/me/app/.env".into(), offset: None, limit: None });
        let r = run(&ops, &read, WriteGate::Auto, &mut ask(true), false).await;
        assert!(r.feedback.contains(redact::PLACEHOLDER));
        // 改一行端口、整文件写回：占位符会把真实密钥抹掉 —— 拒绝，文件不动，连卡都不出
        let rewritten = "DB_HOST=db2\nDB_PASSWORD=[REDACTED]\nOPENAI_API_KEY=[REDACTED]\n";
        let mut a = ask(true);
        let out = run(&ops, &write_call("/home/me/app/.env", rewritten), WriteGate::Auto, &mut a, false).await;
        assert!(!out.ok, "{}", out.feedback);
        assert!(out.feedback.contains("脱敏占位符") && out.feedback.contains("nx_edit"));
        assert!(a.asked.is_empty());
        assert_eq!(t.get("/home/me/app/.env").unwrap(), original.to_vec());
        // nx_edit 把占位符写进去同样拒；只改不相关的行照常
        let bad_edit = FileCall::Edit(EditArgs {
            target: None,
            path: "/home/me/app/.env".into(),
            old_string: "DB_PASSWORD=hunter2hunter2".into(),
            new_string: "DB_PASSWORD=[REDACTED]".into(),
            replace_all: false,
        });
        assert!(!run(&ops, &bad_edit, WriteGate::Auto, &mut ask(true), false).await.ok);
        let good_edit = FileCall::Edit(EditArgs {
            target: None,
            path: "/home/me/app/.env".into(),
            old_string: "DB_HOST=db".into(),
            new_string: "DB_HOST=db2".into(),
            replace_all: false,
        });
        assert!(run(&ops, &good_edit, WriteGate::Auto, &mut ask(true), false).await.ok);
        assert!(String::from_utf8(t.get("/home/me/app/.env").unwrap()).unwrap().contains("DB_PASSWORD=hunter2hunter2"));
    }

    #[tokio::test]
    async fn approval_card_shows_raw_diff_but_outcome_is_redacted() {
        let t = FakeTransport::new("/home/me");
        t.add_file("/home/me/conf.ini", b"key=sk-proj-abcdefghijklmnopqrstuvwxyz1234\nport=1\n");
        let ops = FileOps::new(&t, FileOpsConfig::default());
        let call = FileCall::Edit(EditArgs {
            target: None,
            path: "/home/me/conf.ini".into(),
            old_string: "key=sk-proj-abcdefghijklmnopqrstuvwxyz1234".into(),
            new_string: "key=sk-proj-zyxwvutsrqponmlkjihgfedcba9999".into(),
            replace_all: false,
        });
        let mut a = ask(true);
        let out = run(&ops, &call, WriteGate::Approve(vec![]), &mut a, false).await;
        assert!(out.ok);
        // 人在卡上看得到前后两个真实值，才分得清改了什么
        assert!(a.asked[0].diff.contains("abcdefghijklmnopqrstuvwxyz1234") && a.asked[0].diff.contains("zyxwvutsrqponmlkjihgfedcba9999"));
        // 步骤卡 / 时间线 / 运行记录用的那份是脱敏的
        let d = out.diff.unwrap();
        assert!(!d.contains("abcdefghijklmnopqrstuvwxyz1234") && !d.contains("zyxwvutsrqponmlkjihgfedcba9999"), "{d}");
    }

    #[tokio::test]
    async fn sensitive_read_is_redacted_for_the_model() {
        let t = FakeTransport::new("/home/me");
        t.add_file("/home/me/app/.env", b"OPENAI_API_KEY=sk-proj-abcdefghijklmnopqrstuvwxyz1234\nPORT=80\n");
        let ops = FileOps::new(&t, FileOpsConfig::default());
        let call = FileCall::Read(ReadArgs { target: None, path: "/home/me/app/.env".into(), offset: None, limit: None });
        let out = run(&ops, &call, WriteGate::Auto, &mut ask(true), false).await;
        assert!(out.ok);
        assert!(!out.feedback.contains("abcdefghijklmnopqrstuvwxyz1234"), "{}", out.feedback);
        assert!(out.feedback.contains("PORT=80"));
        assert!(out.summary.contains("已脱敏"));
        // 普通文件、没开脱敏：原样
        t.add_file("/home/me/a.txt", b"hello\n");
        let call = FileCall::Read(ReadArgs { target: None, path: "/home/me/a.txt".into(), offset: None, limit: None });
        assert!(run(&ops, &call, WriteGate::Auto, &mut ask(true), false).await.feedback.contains("hello"));
    }

    /// 禁读清单：不批准只有元数据 +「需要用户批准才能读取」；批准一张写明路径的卡之后才读，内容仍脱敏
    #[tokio::test]
    async fn protected_read_needs_approval_and_is_still_redacted() {
        let t = FakeTransport::new("/home/me");
        let key = concat!(
            "-----BEGIN OPENSSH PRIVATE KEY-----\n",
            "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n",
            "QyNTUxOQAAACDqVZ1vYw5cX0xr8b7mJ3lK9pQh2Zr4uT1sY8eWnB0dEwAAAJgAAAAAAAAA\n",
            "-----END OPENSSH PRIVATE KEY-----\n"
        );
        t.add_file("/home/me/.ssh/id_ed25519", key.as_bytes());
        let ops = FileOps::new(&t, FileOpsConfig::default());
        let call = FileCall::Read(ReadArgs { target: None, path: "~/.ssh/id_ed25519".into(), offset: None, limit: None });

        let mut a = ask(false);
        let out = run(&ops, &call, WriteGate::Auto, &mut a, false).await;
        assert!(!out.ok);
        assert!(out.feedback.contains("需要用户批准才能读取") && out.feedback.contains("/home/me/.ssh/id_ed25519"), "{}", out.feedback);
        assert!(out.feedback.contains(&format!("{} 字节", key.len())), "带元数据：{}", out.feedback);
        assert!(!out.feedback.contains("b3BlbnNzaC1"), "{}", out.feedback);
        assert!(!out.feedback.contains("nx_exec"), "不能教模型绕过");
        assert_eq!(a.asked.len(), 1);
        assert!(a.asked[0].read && a.asked[0].summary.contains("/home/me/.ssh/id_ed25519"));
        assert!(a.asked[0].title().starts_with("读取受保护路径 /home/me/.ssh/id_ed25519"));

        let mut a = ask(true);
        let out = run(&ops, &call, WriteGate::Auto, &mut a, false).await;
        assert!(out.ok, "{}", out.feedback);
        assert!(!out.feedback.contains("b3BlbnNzaC1") && !out.feedback.contains("QyNTUxOQAAACDq"), "{}", out.feedback);
        assert!(out.feedback.contains("[REDACTED]") && out.summary.contains("已脱敏"));
    }

    /// 预览不完整（差异超过上限）的写入：自动档位也必须出卡，卡上写明
    #[tokio::test]
    async fn incomplete_preview_forces_a_card_even_in_auto() {
        let t = FakeTransport::new("/home/me");
        let before: String = (0..3000).map(|i| format!("{i}\n")).collect();
        t.add_file("/home/me/big.txt", before.as_bytes());
        let ops = FileOps::new(&t, FileOpsConfig::default());
        let after: String = (0..3000).map(|i| format!("x{i}\n")).collect();
        let call = write_call("/home/me/big.txt", &after);

        let mut a = ask(false);
        let out = run(&ops, &call, WriteGate::Auto, &mut a, false).await;
        assert!(!out.ok && out.feedback.contains("没有批准"));
        assert_eq!(t.get("/home/me/big.txt").unwrap(), before.as_bytes(), "没批准不写");
        assert_eq!(a.asked.len(), 1);
        assert!(!a.asked[0].preview_complete);
        assert!(a.asked[0].diff.starts_with(files::DIFF_INCOMPLETE_MARK));
        assert!(a.asked[0].reasons.iter().any(|r| r.contains("预览不完整")));
        assert!(a.asked[0].title().contains("预览不完整"));

        // 完整的小改动在自动档位照旧直接写
        let mut a = ask(false);
        let out = run(&ops, &write_call("/home/me/s.txt", "hi\n"), WriteGate::Auto, &mut a, false).await;
        assert!(out.ok && a.asked.is_empty());
    }

    #[test]
    fn relative_paths_need_a_known_cwd() {
        assert_eq!(absolutize("/etc/hosts", None).unwrap(), "/etc/hosts");
        assert_eq!(absolutize("~/x", None).unwrap(), "~/x");
        assert_eq!(absolutize("./src/main.rs", Some("/srv/app/")).unwrap(), "/srv/app/src/main.rs");
        let e = absolutize("src/main.rs", None).unwrap_err();
        assert!(e.contains("相对路径") && e.contains("pwd"), "{e}");
        let mut c = write_call("a.txt", "x");
        assert!(c.absolutize(None).is_err());
        c.absolutize(Some("/tmp")).unwrap();
        assert_eq!(c.path(), Some("/tmp/a.txt"));
    }

    #[test]
    fn parse_and_write_lines() {
        let c = FileCall::parse(files::TOOL_WRITE, &serde_json::json!({"path": "/a", "content": "l1\nl2"})).unwrap();
        assert!(c.is_write());
        assert_eq!(c.write_lines(), vec!["/a", "l1", "l2"]);
        assert!(FileCall::parse(files::TOOL_READ, &serde_json::json!({})).is_none(), "缺 path 不产出动作");
        assert!(FileCall::parse("nx_nope", &serde_json::json!({})).is_none());
    }
}
