//! 「这是误判」：门禁卡上用户说屏幕被认错了 → 把决策时的屏幕存成一份**回归样本**。
//!
//! 托管写入被拒 / 要批准时（`act.rs` 发门禁卡那一刻），把当时的屏幕帧（已脱敏）和分类结果暂存在内存里
//! （按卡片 id，只留最近 [`STASH_CAP`] 张）；用户点「这是误判」→ `host_report_misread` 取出来，
//! 加上用户的一句说明，写进应用数据目录下的 `misreads/misread-<时间戳>-<局面>.json`。
//! 暂存里找不到（重启过 / 太久以前的卡）就退回取这个终端此刻的屏幕。
//!
//! ## 把样本变成测试
//! 文件里的 `screen.lines` 就是一帧屏幕的逐行文本。把它抄进 `situation.rs` 的测试，用
//! `situation::frames::classify_lines(&[...])` 分类，断言**正确**的局面，例如：
//!
//! ```ignore
//! #[test]
//! fn python_heredoc_is_not_a_password_prompt() {
//!     let s = frames::classify_lines(&["$ cat > x.py <<'EOF'", "> password = input('pw: ')", "> "]);
//!     assert_ne!(s.kind, SituationKind::Password);
//! }
//! ```
//!
//! `classified` 记的是当时（错误的）判断和命中的规则（`evidence`），对着它去找是哪条规则认错了。
//! `screen.alt = true` 的帧要自己拼 `ScreenSnapshot { alt: true, .. }` 再调 `situation::classify`。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::super::buffer::strip_ansi;
use super::super::redact;
use super::super::situation::Situation;
use super::super::steward::ScreenSnapshot;

/// 暂存最多留几张卡的现场
pub const STASH_CAP: usize = 32;
/// 一帧最多存多少行（取末尾）、每行最多多少字符：样本要小
const MAX_LINES: usize = 80;
const MAX_LINE_CHARS: usize = 400;

/// 决策时的屏幕帧（已脱敏）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenFrame {
    pub lines: Vec<String>,
    pub alt: bool,
    /// 来自前端渲染帧（true）还是旁路字节流的末尾（false，没有新鲜帧时）
    pub rendered: bool,
}

/// 当时的分类结果
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Classified {
    /// `SituationKind::as_str`
    pub kind: String,
    /// `Program::as_str`
    pub program: String,
    /// 给人看的那句「屏幕被识别为：…」
    pub label: String,
    pub evidence: Vec<String>,
    pub confidence: f32,
}

/// 一次门禁决策的现场（暂存用，也是报告的主体）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateEvidence {
    pub contract_id: String,
    pub console_id: String,
    pub ts_ms: i64,
    /// 想做的那一步（已脱敏）
    pub action: String,
    /// 门禁的说明
    pub reason: String,
    pub screen: ScreenFrame,
    pub classified: Classified,
}

/// 写进 `misreads/` 的文件
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MisreadReport {
    pub version: u32,
    /// 用户点「这是误判」的时间
    pub reported_at_ms: i64,
    /// 用户的说明（可空）
    pub note: String,
    /// 现场来自门禁卡暂存（true）还是点按钮时现取的屏幕（false，可能已经不是当时那一屏）
    pub from_gate: bool,
    #[serde(flatten)]
    pub evidence: GateEvidence,
}

fn clip_line(l: &str) -> String {
    if l.chars().count() <= MAX_LINE_CHARS {
        l.to_string()
    } else {
        l.chars().take(MAX_LINE_CHARS).chain("…".chars()).collect()
    }
}

/// 抓现场：有渲染帧就用帧（去掉末尾空行），没有就用分类器看到的末尾文本。逐行脱敏、限行限长。纯函数。
pub fn capture(
    contract_id: &str,
    console_id: &str,
    frame: Option<&ScreenSnapshot>,
    sit: &Situation,
    action: &str,
    reason: &str,
    ts_ms: i64,
) -> GateEvidence {
    let (raw, alt, rendered): (Vec<String>, bool, bool) = match frame {
        Some(f) => {
            let mut lines: Vec<String> = f.lines.iter().map(|l| strip_ansi(l).trim_end().to_string()).collect();
            while lines.last().is_some_and(|l| l.is_empty()) {
                lines.pop();
            }
            (lines, f.alt, true)
        }
        None => (sit.tail.lines().map(str::to_string).collect(), false, false),
    };
    let start = raw.len().saturating_sub(MAX_LINES);
    let text = redact::redact_lines(&raw[start..].join("\n"));
    let lines = text.lines().map(clip_line).collect();
    GateEvidence {
        contract_id: contract_id.to_string(),
        console_id: console_id.to_string(),
        ts_ms,
        action: redact::redact(action),
        reason: reason.to_string(),
        screen: ScreenFrame { lines, alt, rendered },
        classified: Classified {
            kind: sit.kind.as_str().to_string(),
            program: sit.program.as_str().to_string(),
            label: super::gate::screen_line(sit),
            evidence: sit.evidence.clone(),
            confidence: sit.confidence,
        },
    }
}

static STASH: LazyLock<Mutex<VecDeque<(String, GateEvidence)>>> = LazyLock::new(|| Mutex::new(VecDeque::new()));

/// 暂存一张门禁卡的现场（只留最近 [`STASH_CAP`] 张）。
pub fn stash(gate_id: &str, ev: GateEvidence) {
    let mut s = STASH.lock();
    s.retain(|(id, _)| id != gate_id);
    s.push_back((gate_id.to_string(), ev));
    while s.len() > STASH_CAP {
        s.pop_front();
    }
}

/// 取一张门禁卡的现场（不移除：同一张卡点两次也能存）。
pub fn find(gate_id: &str) -> Option<GateEvidence> {
    STASH.lock().iter().rev().find(|(id, _)| id == gate_id).map(|(_, e)| e.clone())
}

/// 报告文件名：`misread-<毫秒时间戳>-<局面>.json`
pub fn file_name(r: &MisreadReport) -> String {
    let kind: String = r.evidence.classified.kind.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
    format!("misread-{}-{kind}.json", r.reported_at_ms)
}

/// 写进 `dir`（不存在就建），返回文件路径。
pub fn write_report(dir: &Path, r: &MisreadReport) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(file_name(r));
    let json = serde_json::to_string_pretty(r).map_err(std::io::Error::other)?;
    std::fs::write(&path, json)?;
    Ok(path)
}

/// 告诉模型：用户说屏幕被认错了。同一屏幕上原样重试仍会被同一条规则拦下，所以要换路子。
pub fn model_note(ev: &GateEvidence, note: &str) -> String {
    let seen = ev.classified.label.trim_start_matches("屏幕被识别为：");
    let mut s = if ev.action.trim().is_empty() {
        format!("用户反馈：系统对屏幕的判断是**误判** —— 把屏幕认成了「{seen}」，用户说不是。")
    } else {
        format!("用户反馈：上一步「{}」被拦下是**误判** —— 系统把屏幕认成了「{seen}」，用户说不是。", ev.action)
    };
    if !note.trim().is_empty() {
        s.push_str(&format!("用户补充：{}。", note.trim()));
    }
    s.push_str(
        "请先 nx_screen 重新读屏、看清现场再继续。屏幕没变的话同样的按键还会被同一条规则拦下：\
         换一种做法（例如按 <C-c> 回到 shell 提示符，或改用 nx_write 直接写文件）。这不是契约问题，不要请用户改契约",
    );
    s
}

#[cfg(test)]
mod tests {
    use super::super::super::situation::{frames, Program, SituationKind};
    use super::*;

    fn snap(lines: &[&str]) -> ScreenSnapshot {
        ScreenSnapshot {
            console_id: "c1".into(),
            frame_id: 1,
            window_start: 0,
            hash: "h".into(),
            alt: true,
            cols: 80,
            rows: 24,
            cursor_x: 0,
            cursor_y: 0,
            app_cursor: false,
            lines: lines.iter().map(|x| x.to_string()).collect(),
            updated_at: 1,
            idle_frames: 0,
        }
    }

    #[test]
    fn capture_redacts_trims_and_records_classification() {
        let lines = ["export OPENAI_API_KEY=sk-proj-abcdefghijklmnopqrstuvwxyz1234", ">>> pw = input('Password: ')", "", ""];
        let mut sit = frames::classify_lines(&lines);
        sit.kind = SituationKind::Password;
        sit.program = Program::Script;
        let ev = capture("k1", "c1", Some(&snap(&lines)), &sit, "发送按键：sk-proj-abcdefghijklmnopqrstuvwxyz1234", "底线", 5);
        assert_eq!(ev.screen.lines.len(), 2, "末尾空行去掉");
        assert!(ev.screen.alt && ev.screen.rendered);
        assert!(!ev.screen.lines[0].contains("sk-proj"), "{:?}", ev.screen.lines);
        assert!(!ev.action.contains("sk-proj"));
        assert_eq!(ev.classified.kind, "password");
        assert_eq!(ev.classified.program, "script");
        assert_eq!(ev.classified.label, "屏幕被识别为：密码 / 口令提示（交互脚本）");
    }

    #[test]
    fn capture_without_frame_uses_tail_and_limits_size() {
        let mut sit = frames::classify_lines(&["$ "]);
        let long = "x".repeat(MAX_LINE_CHARS + 50);
        sit.tail = (0..MAX_LINES + 10).map(|_| long.as_str()).collect::<Vec<_>>().join("\n");
        let ev = capture("k", "c", None, &sit, "ls", "", 0);
        assert!(!ev.screen.rendered);
        assert_eq!(ev.screen.lines.len(), MAX_LINES);
        assert_eq!(ev.screen.lines[0].chars().count(), MAX_LINE_CHARS + 1);
    }

    #[test]
    fn stash_keeps_latest_and_caps() {
        let sit = frames::classify_lines(&["$ "]);
        let ev = |a: &str| capture("k", "c", None, &sit, a, "", 0);
        stash("misread-test-a", ev("one"));
        stash("misread-test-a", ev("two"));
        assert_eq!(find("misread-test-a").unwrap().action, "two");
        for i in 0..STASH_CAP {
            stash(&format!("misread-test-fill-{i}"), ev("x"));
        }
        assert!(find("misread-test-a").is_none());
        assert!(STASH.lock().len() <= STASH_CAP);
    }

    #[test]
    fn report_round_trips_to_disk() {
        let sit = frames::classify_lines(&["$ "]);
        let r = MisreadReport {
            version: 1,
            reported_at_ms: 42,
            note: "是 Python 代码".into(),
            from_gate: true,
            evidence: capture("k", "c", None, &sit, "ls", "why", 1),
        };
        let dir = std::env::temp_dir().join(format!("nx-misread-{}", std::process::id()));
        let path = write_report(&dir, &r).unwrap();
        assert!(path.file_name().unwrap().to_str().unwrap().starts_with("misread-42-"));
        let back: MisreadReport = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back, r);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_note_says_misread_and_how_to_proceed() {
        let mut sit = frames::classify_lines(&["$ "]);
        sit.kind = SituationKind::Password;
        sit.program = Program::Unknown;
        let ev = capture("k", "c", None, &sit, "发送按键：…", "", 0);
        let n = model_note(&ev, " 这是 heredoc ");
        assert!(n.contains("误判") && n.contains("「密码 / 口令提示」") && n.contains("用户补充：这是 heredoc。"));
        assert!(n.contains("nx_screen") && !n.contains("secret"));
        let ev = capture("k", "c", None, &sit, "", "", 0);
        assert!(model_note(&ev, "").starts_with("用户反馈：系统对屏幕的判断是**误判**"));
    }
}
