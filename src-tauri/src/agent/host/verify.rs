//! 完成判据核验（设计 §5）：模型调 `nx_complete` 时逐条跑 `success_criteria`。
//!
//! - ScreenMatches：正则匹配当前帧；
//! - CommandOk：先过 `gate::authorize`，再要求「看起来只读」（`danger::assess` 不危险，且没有重定向 /
//!   写入类命令），否则这一条直接判不通过并写明原因 —— 判据不能成为绕过闸门的写入口；
//! - FileExists：`test -e <path>`，同一条执行通道；
//! - Manual：只能人确认。
//!
//! 结论：有自动项没过 → 「未满足：…」反馈给模型，仍 Running；只剩人工项 → ManualCheck 卡；
//! 全过 → Completed（附每条的证据）。执行经 `Prober` 注入，纯逻辑可单测。

use std::future::Future;

use super::super::act::AgentAction;
use super::super::danger;
use super::gate::{authorize, Gate, HostPolicy};
use super::{Check, CheckResult};

/// 包在判据命令后面，取回退出码。
pub const RC_MARK: &str = "__NX_CHECK_RC=";
/// 单条证据最多留多少字
const EVIDENCE_CAP: usize = 400;

/// 核验要用到的现场：读屏、跑一条只读命令（返回终端输出）。
pub trait Prober {
    fn screen(&self) -> String;
    fn run(&mut self, cmd: &str) -> impl Future<Output = Result<String, String>> + Send;
}

/// 写入类命令（首词）：判据只允许只读检查。
const WRITE_WORDS: &[&str] = &[
    "rm", "rmdir", "mv", "cp", "touch", "mkdir", "chmod", "chown", "tee", "dd", "truncate", "ln", "kill",
    "pkill", "killall", "reboot", "shutdown", "apt", "apt-get", "yum", "dnf", "brew", "npm", "pnpm", "yarn",
    "pip", "pip3", "cargo", "make", "docker", "kubectl", "systemctl", "service", "git", "sudo", "su",
    "install", "unzip", "tar", "curl", "wget",
];
/// 首词是上面这些、但子命令只读的例外
const READ_ONLY_FORMS: &[&str] = &[
    "git status", "git log", "git diff", "git rev-parse", "git show", "git branch --show-current",
    "docker ps", "docker inspect", "kubectl get", "kubectl describe", "systemctl is-active",
    "systemctl status", "cargo --version", "npm ls", "pip show", "curl -fsS", "curl -sf", "curl -fs",
];

/// 保守的「只读」判定：不危险、没有输出重定向、首词不是写入类（或是明确的只读形式）。
pub fn read_only_reason(cmd: &str) -> Result<(), String> {
    let c = cmd.trim();
    if c.is_empty() {
        return Err("命令为空".into());
    }
    let risk = danger::assess(c);
    if risk.dangerous {
        return Err(format!("危险命令不作为判据执行：{}", risk.reasons.join("；")));
    }
    let stripped = c.replace("2>&1", "").replace(">/dev/null", "").replace("> /dev/null", "");
    if stripped.contains('>') {
        return Err("判据命令里有输出重定向（会写文件），没有执行".into());
    }
    for seg in c.split(|ch| ch == ';' || ch == '|' || ch == '&').map(str::trim).filter(|s| !s.is_empty()) {
        let first = seg.split_whitespace().next().unwrap_or("");
        if WRITE_WORDS.contains(&first) && !READ_ONLY_FORMS.iter().any(|f| seg.starts_with(f)) {
            return Err(format!("「{first}」可能改动现场，判据只允许只读检查，没有执行"));
        }
    }
    Ok(())
}

fn clip(s: &str) -> String {
    let t = s.trim();
    let n = t.chars().count();
    if n <= EVIDENCE_CAP {
        t.to_string()
    } else {
        format!("…{}", t.chars().skip(n - EVIDENCE_CAP).collect::<String>())
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 从输出里取退出码（最后一个标记），并去掉标记行作为证据。
pub fn parse_rc(out: &str) -> (Option<i32>, String) {
    let mut rc = None;
    let mut kept = Vec::new();
    for line in out.lines() {
        match line.trim().strip_prefix(RC_MARK) {
            Some(n) => rc = n.trim().parse().ok(),
            None => kept.push(line),
        }
    }
    (rc, kept.join("\n"))
}

fn result(check: &Check, passed: bool, detail: impl Into<String>) -> CheckResult {
    CheckResult { check: check.clone(), passed, detail: detail.into() }
}

async fn run_probe<P: Prober>(p: &mut P, cmd: &str) -> Result<(bool, String), String> {
    let out = p.run(&format!("{cmd}; echo {RC_MARK}$?")).await?;
    let (rc, rest) = parse_rc(&out);
    match rc {
        Some(code) => Ok((code == 0, format!("退出码 {code}；{}", clip(&rest)))),
        None => Ok((false, format!("没拿到退出码（命令可能还没结束）；{}", clip(&rest)))),
    }
}

/// 逐条核验。人工项一律 `passed=false, detail="待人工确认"`。
pub async fn verify<P: Prober>(checks: &[Check], policy: &HostPolicy, p: &mut P) -> Vec<CheckResult> {
    let mut out = Vec::with_capacity(checks.len());
    for c in checks {
        let r = match c {
            Check::Manual { .. } => result(c, false, "待人工确认"),
            Check::ScreenMatches { re } => match regex::Regex::new(re) {
                Err(e) => result(c, false, format!("正则写错了：{e}")),
                Ok(rx) => {
                    let screen = p.screen();
                    match rx.find(&screen) {
                        Some(m) => result(c, true, format!("屏幕上有「{}」", clip(m.as_str()))),
                        None => result(c, false, format!("当前屏幕没有匹配；屏幕尾部：{}", clip(&screen))),
                    }
                }
            },
            Check::CommandOk { cmd } => {
                let gate = authorize(policy, &AgentAction::Exec { target: None, command: cmd.clone() }, None);
                match (gate, read_only_reason(cmd)) {
                    (Gate::Deny(why), _) => result(c, false, format!("契约不允许执行：{why}")),
                    (Gate::Approve(_), _) => result(c, false, "这条命令需要批准才能执行，判据不自动跑；改成只读命令或改为人工确认"),
                    (_, Err(why)) => result(c, false, why),
                    _ => match run_probe(p, cmd).await {
                        Ok((ok, ev)) => result(c, ok, ev),
                        Err(e) => result(c, false, format!("执行失败：{e}")),
                    },
                }
            }
            Check::FileExists { path } => match run_probe(p, &format!("test -e {}", shell_quote(path))).await {
                Ok((ok, ev)) => result(c, ok, if ok { "存在".to_string() } else { format!("不存在（{ev}）") }),
                Err(e) => result(c, false, format!("执行失败：{e}")),
            },
        };
        out.push(r);
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    AllPassed,
    /// 自动项都过了，只剩人工确认项
    ManualOnly,
    /// 有自动项没过：每条「判据 —— 证据」
    Unmet(Vec<String>),
}

pub fn verdict(results: &[CheckResult]) -> Verdict {
    let unmet: Vec<String> = results
        .iter()
        .filter(|r| !r.check.is_manual() && !r.passed)
        .map(|r| format!("{} —— {}", r.check.describe(), r.detail))
        .collect();
    if !unmet.is_empty() {
        return Verdict::Unmet(unmet);
    }
    if results.iter().any(|r| r.check.is_manual()) {
        return Verdict::ManualOnly;
    }
    Verdict::AllPassed
}

/// 给人 / 模型看的逐条结果。
pub fn evidence_text(results: &[CheckResult]) -> String {
    results
        .iter()
        .map(|r| format!("{} {}：{}", if r.passed { "✓" } else { "✗" }, r.check.describe(), r.detail))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::super::policy::Authority;
    use super::*;

    /// 假现场：屏幕固定，命令按前缀给输出，记下跑过什么。
    pub(crate) struct FakeProber {
        pub screen: String,
        pub replies: Vec<(&'static str, &'static str)>,
        pub ran: Vec<String>,
    }

    impl Prober for FakeProber {
        fn screen(&self) -> String {
            self.screen.clone()
        }
        fn run(&mut self, cmd: &str) -> impl Future<Output = Result<String, String>> + Send {
            self.ran.push(cmd.to_string());
            let out = self
                .replies
                .iter()
                .find(|(p, _)| cmd.starts_with(p))
                .map(|(_, o)| o.to_string())
                .unwrap_or_else(|| format!("{RC_MARK}1"));
            async move { Ok(out) }
        }
    }

    pub(crate) fn policy() -> HostPolicy {
        HostPolicy { authority: Authority::default().with_tier("auto"), hard: vec![], host_key: "local".into() }
    }

    #[tokio::test]
    async fn failing_check_is_unmet_with_evidence() {
        let mut p = FakeProber {
            screen: "test result: FAILED. 3 passed; 1 failed".into(),
            replies: vec![("cargo test", "running 4 tests\nFAILED\n__NX_CHECK_RC=101")],
            ran: vec![],
        };
        let checks = vec![
            Check::CommandOk { cmd: "cargo test".into() },
            Check::ScreenMatches { re: r"test result: ok".into() },
        ];
        // cargo test 不在只读清单里：先确认它会被拒（不是跑了失败）
        let r = verify(&checks, &policy(), &mut p).await;
        assert!(!r[0].passed && r[0].detail.contains("cargo"));
        assert!(p.ran.is_empty(), "写入类命令一条都没跑");
        match verdict(&r) {
            Verdict::Unmet(v) => assert!(v.len() == 2 && v[1].contains("屏幕尾部")),
            v => panic!("{v:?}"),
        }
    }

    #[tokio::test]
    async fn read_only_command_runs_and_rc_decides() {
        let mut p = FakeProber {
            screen: String::new(),
            replies: vec![("grep -q ok /tmp/r", "__NX_CHECK_RC=0"), ("test -e '/srv/app/dist'", "__NX_CHECK_RC=1")],
            ran: vec![],
        };
        let checks = vec![
            Check::CommandOk { cmd: "grep -q ok /tmp/r".into() },
            Check::FileExists { path: "/srv/app/dist".into() },
        ];
        let r = verify(&checks, &policy(), &mut p).await;
        assert!(r[0].passed, "{:?}", r[0]);
        assert!(!r[1].passed && r[1].detail.starts_with("不存在"));
        assert_eq!(p.ran[0], format!("grep -q ok /tmp/r; echo {RC_MARK}$?"));
        assert!(matches!(verdict(&r), Verdict::Unmet(_)));
    }

    #[tokio::test]
    async fn dangerous_or_writing_command_ok_is_refused() {
        let mut p = FakeProber { screen: String::new(), replies: vec![], ran: vec![] };
        for cmd in ["rm -rf /tmp/x && true", "echo ok > /etc/flag", "git push", "sudo test -e /root"] {
            let r = verify(&[Check::CommandOk { cmd: cmd.into() }], &policy(), &mut p).await;
            assert!(!r[0].passed, "{cmd} 应被拒");
        }
        assert!(p.ran.is_empty());
        // 契约禁项同样拦
        let mut pol = policy();
        pol.authority.deny = vec!["grep".into()];
        let r = verify(&[Check::CommandOk { cmd: "grep -q x f".into() }], &pol, &mut p).await;
        assert!(r[0].detail.contains("契约不允许"));
    }

    #[tokio::test]
    async fn manual_only_and_all_passed() {
        let mut p = FakeProber { screen: "nginx/1.25 ready".into(), replies: vec![], ran: vec![] };
        let checks = vec![Check::ScreenMatches { re: r"nginx/1\.2\d".into() }, Check::Manual { text: "页面能打开".into() }];
        let r = verify(&checks, &policy(), &mut p).await;
        assert_eq!(verdict(&r), Verdict::ManualOnly);
        let r = verify(&checks[..1], &policy(), &mut p).await;
        assert_eq!(verdict(&r), Verdict::AllPassed);
        assert!(evidence_text(&r).starts_with("✓ 屏幕匹配"));
    }

    #[test]
    fn rc_parsing() {
        assert_eq!(parse_rc("a\n__NX_CHECK_RC=0\n"), (Some(0), "a".to_string()));
        assert_eq!(parse_rc("nothing").0, None);
    }
}
