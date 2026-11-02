//! Agent 的过程日志：`AgentConfig.trace` 打开时，把推理过程与每轮请求/回复落到
//! `<app_data>/logs/agent.log`。
//!
//! ## 为什么是独立文件，而不走 `tracing`
//!
//! - 内容是整段模型回复、整段命令输出，体量与性质都不是 `nexus.log` 那种一行一事的运行日志，
//!   混进去会把断链、握手这类真正要翻的线索淹掉。
//! - 默认关闭、关闭时**不创建文件**：`tracing` 的 layer 一旦装上就是常驻的，
//!   而这里只在用户勾选后才第一次写盘。
//!
//! ## 为什么默认关
//!
//! 命令输出里可能有密码、token、内网地址。写入前一律过 [`redact`]，
//! 但脱敏是尽力而为（不认得的格式就漏了），所以仍然要用户自己点头才记。
//! 与 mac 输入托管的 `nx.inputTrace` 是同一个理由。

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use super::redact::redact;

/// 日志目录，启动时由 `lib.rs` 在 `init_logging` 之后登记一次。
///
/// 没登记（例如单测）就静默不写：排查开关不该成为让主流程出错的理由。
static LOG_DIR: OnceLock<PathBuf> = OnceLock::new();

/// 串行化写入：一轮里多个会话可能同时在记，交错写会把条目撕开。
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// 超过这个大小就滚一份旧的，与 `nexus.log` 同一思路，避免长期开着写满磁盘。
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

/// 单条记录的字符上限。命令输出、整段回复偶尔很大，截断比让日志一夜长到 8MB 更合理。
const MAX_ENTRY_CHARS: usize = 20_000;

const FILE_NAME: &str = "agent.log";

/// 登记日志目录（`<app_data>/logs`）。重复调用无害，以第一次为准。
pub fn init(data_dir: &Path) {
    let _ = LOG_DIR.set(data_dir.join("logs"));
}

/// 记一条。**调用方必须先判断 `cfg.trace`**：这里不查开关，
/// 这样关闭时连拼装日志文本的开销都不会发生。
///
/// `step` 为 0 表示不属于某一步（开场、流式问答）。
pub fn record(session: &str, step: usize, kind: &str, body: &str) {
    let Some(dir) = LOG_DIR.get() else {
        return;
    };
    let entry = format_entry(
        &chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        session,
        step,
        kind,
        body,
    );
    append_rotating(dir, &entry, MAX_LOG_BYTES);
}

/// 拼出一条记录：标题行 + 脱敏、截断后的正文。纯函数，便于单测。
///
/// 先脱敏再截断：反过来的话，一段被截在中间的 PEM 私钥会因为缺了结尾标记而逃过脱敏。
fn format_entry(ts: &str, session: &str, step: usize, kind: &str, body: &str) -> String {
    let safe = redact(body);
    let total = safe.chars().count();
    let shown: String = if total > MAX_ENTRY_CHARS {
        let head: String = safe.chars().take(MAX_ENTRY_CHARS).collect();
        format!("{head}\n…（已截断，共 {total} 字符）")
    } else {
        safe
    };
    format!("[{ts}] step={step} session={session} {kind}\n{shown}\n\n")
}

/// 追加写入，写之前检查是否该滚动。任何 I/O 失败都吞掉：排查开关不能拖垮 agent。
fn append_rotating(dir: &Path, entry: &str, limit: u64) {
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join(FILE_NAME);
    if std::fs::metadata(&path).map(|m| m.len() > limit).unwrap_or(false) {
        let _ = std::fs::rename(&path, dir.join(format!("{FILE_NAME}.1")));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(entry.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nx-trace-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn entry_is_redacted_before_it_is_written() {
        let e = format_entry("t", "s1", 2, "reply", "key=AKIAIOSFODNN7EXAMPLE 结束");
        assert!(!e.contains("AKIAIOSFODNN7EXAMPLE"), "密钥不能原样落盘：{e}");
        assert!(e.starts_with("[t] step=2 session=s1 reply\n"));
    }

    #[test]
    fn long_entry_is_truncated_with_a_marker() {
        let body = "啊".repeat(MAX_ENTRY_CHARS + 50);
        let e = format_entry("t", "s", 1, "reply", &body);
        assert!(e.contains("已截断"));
        assert!(e.chars().count() < MAX_ENTRY_CHARS + 200);
    }

    #[test]
    fn file_is_created_lazily_and_rotates_past_limit() {
        let dir = scratch("rotate");
        // 没写过就不该有文件：关着开关的用户磁盘上不应出现 agent.log
        assert!(!dir.join(FILE_NAME).exists());

        append_rotating(&dir, "first\n", 5);
        assert_eq!(std::fs::read_to_string(dir.join(FILE_NAME)).unwrap(), "first\n");
        append_rotating(&dir, "second\n", 5);
        // 第二次写前文件已超 5 字节 → 旧的滚走，新文件只剩 second
        assert_eq!(std::fs::read_to_string(dir.join(FILE_NAME)).unwrap(), "second\n");
        assert_eq!(
            std::fs::read_to_string(dir.join(format!("{FILE_NAME}.1"))).unwrap(),
            "first\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
