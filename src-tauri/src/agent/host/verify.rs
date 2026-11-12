//! 完成判据核验（设计 §5）：模型调 `nx_complete` 时逐条跑 `success_criteria`。
//!
//! - ScreenMatches：正则匹配当前帧；
//! - CommandOk：按判据是否经用户确认分两条规则（结果的 detail 前缀写明用的哪条）：
//!   - 用户确认过（草稿卡上接受 / 编辑）：和普通托管写入一样过 `gate::authorize` —— deny / 硬偏好 /
//!     危险 → Approve（挂起等批准）/ 档位全都生效，放行就执行（`cargo test` 这类通常 Allow / Notify）；
//!   - 没确认过（YOLO）：只允许「看起来只读」的命令（`danger::assess` 不危险、无重定向、首词非写入类），
//!     否则判不通过 —— 没人点过头的判据不能成为绕过闸门的写入口；
//! - FileExists：`test -e <path>`，同一条执行通道；
//! - Manual：只能人确认。
//!
//! 结论：有自动项没过 → 「未满足：…」反馈给模型，仍 Running；只剩人工项 → ManualCheck 卡；
//! 全过 → Completed（附每条的证据）。执行经 `Prober` 注入，纯逻辑可单测。

use std::future::Future;
use std::time::Duration;

use super::super::act::AgentAction;
use super::super::danger;
use super::gate::{authorize, Gate, HostPolicy};
use super::{Check, CheckResult};

/// 包在判据命令后面，取回退出码。
pub const RC_MARK: &str = "__NX_CHECK_RC=";
/// 单条证据最多留多少字
const EVIDENCE_CAP: usize = 400;

/// 核验要用到的现场：读屏、跑一条命令（返回终端输出；执行通道自己的超时报 Err("超时…")）、
/// 闸门要求批准时挂起等人（不设超时）。
pub trait Prober {
    fn screen(&self) -> String;
    fn run(&mut self, cmd: &str) -> impl Future<Output = Result<String, String>> + Send;
    fn approve(&mut self, cmd: &str, reasons: &[String]) -> impl Future<Output = bool> + Send;
}

/// YOLO 判据遇到非只读命令时的说法。
pub const YOLO_REFUSE: &str = "该检查不是只读命令，YOLO 下不自动执行；请用 nx_exec 自行运行或让用户确认";
const RULE_CONFIRMED: &str = "[已确认·经授权执行]";
const RULE_YOLO: &str = "[未确认·只读规则]";

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

async fn run_probe<P: Prober>(p: &mut P, cmd: &str, cap: Duration) -> Result<(bool, String), String> {
    let out = match tokio::time::timeout(cap, p.run(&format!("{cmd}; echo {RC_MARK}$?"))).await {
        Ok(r) => r?,
        Err(_) => return Ok((false, "超时".to_string())),
    };
    let (rc, rest) = parse_rc(&out);
    match rc {
        Some(code) => Ok((code == 0, format!("退出码 {code}；{}", clip(&rest)))),
        None => Ok((false, format!("没拿到退出码（命令可能还没结束）；{}", clip(&rest)))),
    }
}

/// 逐条核验。人工项一律 `passed=false, detail="待人工确认"`。`confirmed` = 判据经用户确认过；
/// `cap` = 每条命令的时间上限（沿用执行通道的上限），超时判不通过、证据写「超时」。
pub async fn verify<P: Prober>(
    checks: &[Check],
    policy: &HostPolicy,
    confirmed: bool,
    cap: Duration,
    p: &mut P,
) -> Vec<CheckResult> {
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
            Check::CommandOk { cmd } => command_check(c, cmd, policy, confirmed, cap, p).await,
            Check::FileExists { path } => match run_probe(p, &format!("test -e {}", shell_quote(path)), cap).await {
                Ok((ok, ev)) => result(c, ok, if ok { "存在".to_string() } else { format!("不存在（{ev}）") }),
                Err(e) => result(c, false, format!("执行失败：{e}")),
            },
        };
        out.push(r);
    }
    out
}

/// CommandOk 一条：按确认与否走两条规则，detail 前缀写明用的哪条。
async fn command_check<P: Prober>(
    c: &Check,
    cmd: &str,
    policy: &HostPolicy,
    confirmed: bool,
    cap: Duration,
    p: &mut P,
) -> CheckResult {
    let rule = if confirmed { RULE_CONFIRMED } else { RULE_YOLO };
    let r = |ok: bool, d: String| result(c, ok, format!("{rule} {d}"));
    let gate = authorize(policy, &AgentAction::Exec { target: None, command: cmd.to_string() }, None);
    if let Gate::Deny(why) = &gate {
        return r(false, format!("契约不允许执行：{why}"));
    }
    if confirmed {
        // 用户点过头的判据：与普通托管写入同一套闸门；危险 / approve 档就挂起等批准
        if let Gate::Approve(reasons) = &gate {
            if !p.approve(cmd, reasons).await {
                return r(false, "这条检查需要批准，用户没有批准，没有执行".to_string());
            }
        }
    } else {
        if let Err(why) = read_only_reason(cmd) {
            return r(false, format!("{YOLO_REFUSE}（{why}）"));
        }
        if matches!(gate, Gate::Approve(_)) {
            return r(false, format!("{YOLO_REFUSE}（需要批准）"));
        }
    }
    match run_probe(p, cmd, cap).await {
        Ok((ok, ev)) => r(ok, ev),
        Err(e) => r(false, format!("执行失败：{e}")),
    }
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
        pub asked: Vec<String>,
        pub approve_answer: bool,
        pub slow: bool,
    }

    pub(crate) fn prober(screen: &str, replies: Vec<(&'static str, &'static str)>) -> FakeProber {
        FakeProber { screen: screen.into(), replies, ran: vec![], asked: vec![], approve_answer: false, slow: false }
    }

    const CAP: Duration = Duration::from_secs(120);

    impl Prober for FakeProber {
        fn screen(&self) -> String {
            self.screen.clone()
        }
        fn approve(&mut self, cmd: &str, _reasons: &[String]) -> impl Future<Output = bool> + Send {
            self.asked.push(cmd.to_string());
            let ok = self.approve_answer;
            async move { ok }
        }
        fn run(&mut self, cmd: &str) -> impl Future<Output = Result<String, String>> + Send {
            self.ran.push(cmd.to_string());
            let slow = self.slow;
            let out = self
                .replies
                .iter()
                .find(|(p, _)| cmd.starts_with(p))
                .map(|(_, o)| o.to_string())
                .unwrap_or_else(|| format!("{RC_MARK}1"));
            async move {
                if slow {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                }
                Ok(out)
            }
        }
    }

    pub(crate) fn policy() -> HostPolicy {
        HostPolicy { authority: Authority::default().with_tier("auto"), hard: vec![], host_key: "local".into() }
    }

    #[tokio::test]
    async fn failing_check_is_unmet_with_evidence() {
        let mut p = prober("test result: FAILED. 3 passed; 1 failed", vec![("cargo test", "running 4 tests\nFAILED\n__NX_CHECK_RC=101")]);
        let checks = vec![
            Check::CommandOk { cmd: "cargo test".into() },
            Check::ScreenMatches { re: r"test result: ok".into() },
        ];
        // cargo test 不在只读清单里：先确认它会被拒（不是跑了失败）
        let r = verify(&checks, &policy(), false, CAP, &mut p).await;
        assert!(!r[0].passed && r[0].detail.contains("cargo"));
        assert!(p.ran.is_empty(), "写入类命令一条都没跑");
        match verdict(&r) {
            Verdict::Unmet(v) => assert!(v.len() == 2 && v[1].contains("屏幕尾部")),
            v => panic!("{v:?}"),
        }
    }

    #[tokio::test]
    async fn read_only_command_runs_and_rc_decides() {
        let mut p = prober("", vec![("grep -q ok /tmp/r", "__NX_CHECK_RC=0"), ("test -e '/srv/app/dist'", "__NX_CHECK_RC=1")]);
        let checks = vec![
            Check::CommandOk { cmd: "grep -q ok /tmp/r".into() },
            Check::FileExists { path: "/srv/app/dist".into() },
        ];
        let r = verify(&checks, &policy(), false, CAP, &mut p).await;
        assert!(r[0].passed, "{:?}", r[0]);
        assert!(!r[1].passed && r[1].detail.starts_with("不存在"));
        assert_eq!(p.ran[0], format!("grep -q ok /tmp/r; echo {RC_MARK}$?"));
        assert!(matches!(verdict(&r), Verdict::Unmet(_)));
    }

    #[tokio::test]
    async fn dangerous_or_writing_command_ok_is_refused() {
        let mut p = prober("", vec![]);
        for cmd in ["rm -rf /tmp/x && true", "echo ok > /etc/flag", "git push", "sudo test -e /root"] {
            let r = verify(&[Check::CommandOk { cmd: cmd.into() }], &policy(), false, CAP, &mut p).await;
            assert!(!r[0].passed, "{cmd} 应被拒");
        }
        assert!(p.ran.is_empty());
        // 契约禁项同样拦
        let mut pol = policy();
        pol.authority.deny = vec!["grep".into()];
        let r = verify(&[Check::CommandOk { cmd: "grep -q x f".into() }], &pol, false, CAP, &mut p).await;
        assert!(r[0].detail.contains("契约不允许"));
    }

    #[tokio::test]
    async fn manual_only_and_all_passed() {
        let mut p = prober("nginx/1.25 ready", vec![]);
        let checks = vec![Check::ScreenMatches { re: r"nginx/1\.2\d".into() }, Check::Manual { text: "页面能打开".into() }];
        let r = verify(&checks, &policy(), false, CAP, &mut p).await;
        assert_eq!(verdict(&r), Verdict::ManualOnly);
        let r = verify(&checks[..1], &policy(), false, CAP, &mut p).await;
        assert_eq!(verdict(&r), Verdict::AllPassed);
        assert!(evidence_text(&r).starts_with("✓ 屏幕匹配"));
    }

    #[tokio::test]
    async fn confirmed_test_runner_goes_through_authorize_and_runs() {
        let mut p = prober("", vec![("cargo test", "test result: ok. 12 passed\n__NX_CHECK_RC=0")]);
        let checks = vec![Check::CommandOk { cmd: "cargo test".into() }];
        let r = verify(&checks, &policy(), true, CAP, &mut p).await;
        assert!(r[0].passed, "{:?}", r[0]);
        assert!(r[0].detail.starts_with(RULE_CONFIRMED));
        assert_eq!(p.ran.len(), 1);
        assert!(p.asked.is_empty(), "auto 档 + 不危险：Allow，直接跑");
        // notify 档同样直接跑
        let mut pol = policy();
        pol.authority = pol.authority.with_tier("notify");
        let r = verify(&checks, &pol, true, CAP, &mut p).await;
        assert!(r[0].passed && p.asked.is_empty());
    }

    #[tokio::test]
    async fn yolo_refuses_the_same_command() {
        let mut p = prober("", vec![("cargo test", "__NX_CHECK_RC=0")]);
        let r = verify(&[Check::CommandOk { cmd: "cargo test".into() }], &policy(), false, CAP, &mut p).await;
        assert!(!r[0].passed);
        assert!(r[0].detail.starts_with(RULE_YOLO) && r[0].detail.contains(YOLO_REFUSE), "{}", r[0].detail);
        assert!(p.ran.is_empty() && p.asked.is_empty());
    }

    #[tokio::test]
    async fn confirmed_dangerous_check_parks_for_approval() {
        let cmd = "sudo systemctl is-active nginx";
        assert!(danger::assess(cmd).dangerous, "前提：sudo 被判危险");
        let mut p = prober("", vec![("sudo systemctl", "active\n__NX_CHECK_RC=0")]);
        let checks = vec![Check::CommandOk { cmd: cmd.into() }];
        let r = verify(&checks, &policy(), true, CAP, &mut p).await;
        assert_eq!(p.asked.len(), 1, "危险 → Approve → 挂起问人");
        assert!(!r[0].passed && r[0].detail.contains("没有批准") && p.ran.is_empty());
        p.approve_answer = true;
        let r = verify(&checks, &policy(), true, CAP, &mut p).await;
        assert_eq!(p.ran.len(), 1, "批准了才执行");
        assert!(r[0].passed);
        // 契约 deny 照样拦，问都不问
        let mut pol = policy();
        pol.authority.deny = vec!["sudo".into()];
        let mut p2 = prober("", vec![]);
        let r = verify(&checks, &pol, true, CAP, &mut p2).await;
        assert!(r[0].detail.contains("契约不允许") && p2.asked.is_empty() && p2.ran.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn each_check_is_time_capped() {
        let mut p = prober("", vec![("grep", "__NX_CHECK_RC=0")]);
        p.slow = true;
        let r = verify(&[Check::CommandOk { cmd: "grep -q x f".into() }], &policy(), true, Duration::from_secs(5), &mut p).await;
        assert!(!r[0].passed && r[0].detail.ends_with("超时"), "{}", r[0].detail);
    }

    #[test]
    fn rc_parsing() {
        assert_eq!(parse_rc("a\n__NX_CHECK_RC=0\n"), (Some(0), "a".to_string()));
        assert_eq!(parse_rc("nothing").0, None);
    }
}
