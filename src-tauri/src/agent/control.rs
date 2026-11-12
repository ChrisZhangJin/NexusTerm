//! 终端控制权：人和 Agent 共用一个 pty，谁在「开车」要有一个说得清的状态。
//!
//! 设计取向：**用户永远赢**。键盘输入从不被拦、不被延迟 —— 用户一敲键，
//! 若此刻 Agent 正在驾驶，状态就自动落到 `Paused`；Agent 在每次写 pty 之前
//! 检查状态，`Paused` 就停下来等（可取消、有上限），绝不往用户正在打的字里插字节。
//!
//! 本模块只含**纯逻辑**（状态转移 + 按会话的登记表 + 等待循环），不碰 Tauri / pty，
//! 事件推送与 IPC 命令在 `act.rs` / `cmd.rs`，这样转移规则可以脱离运行时单测。
//!
//! ```text
//!   UserDriving --acquire--> AgentDriving --user_input/takeover--> Paused
//!        ^                        ^                                  |
//!        |                        +-----------handback---------------+
//!        +------------ release_run（一轮结束 / 被停止）--------------- 任意状态
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use parking_lot::Mutex;
use serde::Serialize;

/// 一个终端会话此刻的控制权归属。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ControlState {
    /// 没有 Agent 在驾驶这个会话（默认）。
    UserDriving,
    /// 有一轮 Agent 运行正在对它动手。
    AgentDriving,
    /// Agent 本想继续，但用户已动手（敲键盘 / 点了「接管」）：Agent 须让路。
    Paused,
}

/// 驱动状态转移的事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlEvent {
    /// 有一轮运行开始驾驶该会话。已 `Paused` 的保持 `Paused`（用户的接管不被新动作冲掉）。
    RunStarted,
    /// 所有驾驶它的运行都结束了。
    RunEnded,
    /// 用户敲了键盘。
    UserInput,
    /// 用户点了「接管」。
    Takeover,
    /// 用户点了「交还」。
    Handback,
}

/// 纯状态转移。不认识的组合一律原样返回 —— 例如 `UserDriving` 下的 `UserInput`
/// 就是普通打字，什么都不该发生。
pub fn next(state: ControlState, ev: ControlEvent) -> ControlState {
    use ControlEvent::*;
    use ControlState::*;
    match (state, ev) {
        (Paused, RunStarted) => Paused,
        (_, RunStarted) => AgentDriving,
        (_, RunEnded) => UserDriving,
        (AgentDriving, UserInput) | (AgentDriving, Takeover) => Paused,
        (Paused, Handback) => AgentDriving,
        (s, _) => s,
    }
}

struct Entry {
    state: ControlState,
    /// 正在驾驶它的运行（用运行的发起会话 id 标识）。一般只有一个，
    /// 两轮运行同时把手伸向同一个目标会话时有两个，须全部结束才算还给用户。
    holders: Vec<String>,
}

/// 按会话维护的控制权登记表。
#[derive(Default)]
pub struct ControlTable {
    inner: Mutex<HashMap<String, Entry>>,
}

impl ControlTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// 会话此刻的状态；没登记过就是 `UserDriving`。
    pub fn state(&self, session: &str) -> ControlState {
        self.inner
            .lock()
            .get(session)
            .map(|e| e.state)
            .unwrap_or(ControlState::UserDriving)
    }

    /// 运行 `run` 开始驾驶 `session`。幂等。返回**变化后**的新状态（没变返回 `None`）。
    pub fn acquire(&self, session: &str, run: &str) -> Option<ControlState> {
        let mut map = self.inner.lock();
        let e = map.entry(session.to_string()).or_insert(Entry {
            state: ControlState::UserDriving,
            holders: Vec::new(),
        });
        if !e.holders.iter().any(|h| h == run) {
            e.holders.push(run.to_string());
        }
        let new = next(e.state, ControlEvent::RunStarted);
        if new == e.state {
            return None;
        }
        e.state = new;
        Some(new)
    }

    /// 运行 `run` 结束（正常 / 出错 / 被停止）：从它驾驶过的所有会话上撤下。
    /// 返回 `(会话, 新状态)` 的变化清单；还有别的运行在驾驶的会话保持原状。
    pub fn release_run(&self, run: &str) -> Vec<(String, ControlState)> {
        let mut map = self.inner.lock();
        let mut changed = Vec::new();
        map.retain(|session, e| {
            let before = e.holders.len();
            e.holders.retain(|h| h != run);
            if e.holders.len() == before {
                return true; // 与这轮无关
            }
            if e.holders.is_empty() {
                // 最后一个驾驶者走了：还给用户，并清掉登记（表不随会话数涨）
                changed.push((session.clone(), next(e.state, ControlEvent::RunEnded)));
                return false;
            }
            true
        });
        changed
    }

    /// 运行 `run` 当前驾驶着哪些会话（停止时给它们补发 Ctrl-C 用）。
    pub fn sessions_of(&self, run: &str) -> Vec<String> {
        self.inner
            .lock()
            .iter()
            .filter(|(_, e)| e.holders.iter().any(|h| h == run))
            .map(|(s, _)| s.clone())
            .collect()
    }

    /// 某会话当前被哪些运行驾驶（面板据此把「该会话的状态」路由给对应的运行面板）。
    pub fn holders_of(&self, session: &str) -> Vec<String> {
        self.inner
            .lock()
            .get(session)
            .map(|e| e.holders.clone())
            .unwrap_or_default()
    }

    /// 用户在 `session` 上敲了键。仅当 Agent 正在驾驶时才有效果；返回新状态。
    ///
    /// 键盘热路径的前端只在「Agent 驾驶中」才会调到这里，所以这里不必追求零开销，
    /// 但仍然只是一次短锁 + 一次哈希查找。
    pub fn user_input(&self, session: &str) -> Option<ControlState> {
        self.apply(session, ControlEvent::UserInput)
    }

    /// 用户点「接管」。
    pub fn takeover(&self, session: &str) -> Option<ControlState> {
        self.apply(session, ControlEvent::Takeover)
    }

    /// 用户点「交还」。
    pub fn handback(&self, session: &str) -> Option<ControlState> {
        self.apply(session, ControlEvent::Handback)
    }

    /// 运行 `run` 自己进入暂停（预算 / 用户点「暂停」）：**不撤租约**，只把它驾驶的
    /// 会话一律标成 `Paused`。托管模式下暂停期间仍是这轮运行占着终端，
    /// 守护进程据 `holders_of` 非空就不会趁机写入。返回变化清单。
    pub fn pause_run(&self, run: &str) -> Vec<(String, ControlState)> {
        self.apply_run(run, ControlEvent::Takeover)
    }

    /// `pause_run` 的反向：用户点「继续」后把这轮运行驾驶的会话交还给它。
    pub fn resume_run(&self, run: &str) -> Vec<(String, ControlState)> {
        self.apply_run(run, ControlEvent::Handback)
    }

    fn apply_run(&self, run: &str, ev: ControlEvent) -> Vec<(String, ControlState)> {
        let mut map = self.inner.lock();
        let mut changed = Vec::new();
        for (session, e) in map.iter_mut() {
            if !e.holders.iter().any(|h| h == run) {
                continue;
            }
            let new = next(e.state, ev);
            if new != e.state {
                e.state = new;
                changed.push((session.clone(), new));
            }
        }
        changed.sort_by(|a, b| a.0.cmp(&b.0));
        changed
    }

    fn apply(&self, session: &str, ev: ControlEvent) -> Option<ControlState> {
        let mut map = self.inner.lock();
        let e = map.get_mut(session)?;
        let new = next(e.state, ev);
        if new == e.state {
            return None;
        }
        e.state = new;
        Some(new)
    }
}

/// 等待控制权还给 Agent 的结局。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseWait {
    /// 没被暂停（或等到了交还）：可以写了。
    Ready,
    /// 等的过程中本轮被用户停止。
    Cancelled,
    /// 等满上限用户仍没交还：按「用户接管了终端」处理，别再写。
    TookOver,
}

/// Agent 写 pty 之前的闸：`Paused` 就轮询等待，直到交还 / 取消 / 超过 `max`。
///
/// 取消优先于一切 —— 用户点了停止，就不该再多等一个轮询周期。
/// 闭包而非直接拿 `ControlTable`：测试里可以用假时钟 / 假状态驱动。
pub async fn wait_unpaused<P, C, S, Fut>(
    is_paused: P,
    is_cancelled: C,
    max: Duration,
    poll: Duration,
    mut sleep: S,
) -> PauseWait
where
    P: Fn() -> bool,
    C: Fn() -> bool,
    S: FnMut(Duration) -> Fut,
    Fut: Future<Output = ()>,
{
    let mut waited = Duration::ZERO;
    loop {
        if is_cancelled() {
            return PauseWait::Cancelled;
        }
        if !is_paused() {
            return PauseWait::Ready;
        }
        if waited >= max {
            return PauseWait::TookOver;
        }
        sleep(poll).await;
        waited += poll;
    }
}

#[cfg(test)]
mod tests {
    use super::ControlEvent::*;
    use super::ControlState::*;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn transitions_normal_path() {
        assert_eq!(next(UserDriving, RunStarted), AgentDriving);
        assert_eq!(next(AgentDriving, UserInput), Paused);
        assert_eq!(next(AgentDriving, Takeover), Paused);
        assert_eq!(next(Paused, Handback), AgentDriving);
        assert_eq!(next(AgentDriving, RunEnded), UserDriving);
        assert_eq!(next(Paused, RunEnded), UserDriving);
    }

    #[test]
    fn typing_while_nobody_drives_is_noop() {
        assert_eq!(next(UserDriving, UserInput), UserDriving);
        assert_eq!(next(UserDriving, Takeover), UserDriving);
        assert_eq!(next(UserDriving, Handback), UserDriving);
        // Paused 下继续打字 / 再点接管都还是 Paused
        assert_eq!(next(Paused, UserInput), Paused);
        assert_eq!(next(Paused, Takeover), Paused);
        // 交还只对 Paused 有意义
        assert_eq!(next(AgentDriving, Handback), AgentDriving);
    }

    #[test]
    fn pause_run_keeps_lease_and_marks_paused() {
        let t = ControlTable::new();
        t.acquire("tab-a", "run-1");
        t.acquire("tab-b", "run-1");
        t.acquire("tab-c", "run-2");
        assert_eq!(
            t.pause_run("run-1"),
            vec![("tab-a".to_string(), Paused), ("tab-b".to_string(), Paused)]
        );
        // 租约还在：守护进程据 holders 判定「LLM 运行占着」
        assert_eq!(t.holders_of("tab-a"), vec!["run-1".to_string()]);
        assert_eq!(t.state("tab-c"), AgentDriving, "别的运行不受影响");
        assert!(t.pause_run("run-1").is_empty(), "幂等");
        assert_eq!(
            t.resume_run("run-1"),
            vec![("tab-a".to_string(), AgentDriving), ("tab-b".to_string(), AgentDriving)]
        );
        assert!(t.pause_run("ghost").is_empty());
    }

    #[test]
    fn new_action_does_not_override_takeover() {
        assert_eq!(next(Paused, RunStarted), Paused);
    }

    #[test]
    fn table_full_lifecycle() {
        let t = ControlTable::new();
        assert_eq!(t.state("s"), UserDriving);
        assert_eq!(t.acquire("s", "s"), Some(AgentDriving));
        assert_eq!(t.acquire("s", "s"), None, "幂等");
        assert_eq!(t.state("s"), AgentDriving);
        assert_eq!(t.user_input("s"), Some(Paused));
        assert_eq!(t.user_input("s"), None, "已经 Paused，再敲键不再变化");
        assert_eq!(t.handback("s"), Some(AgentDriving));
        assert_eq!(t.takeover("s"), Some(Paused));
        assert_eq!(t.release_run("s"), vec![("s".to_string(), UserDriving)]);
        assert_eq!(t.state("s"), UserDriving);
        // 登记已清空：之后用户打字不会凭空变出状态
        assert_eq!(t.user_input("s"), None);
    }

    #[test]
    fn table_user_actions_noop_when_nobody_drives() {
        let t = ControlTable::new();
        assert_eq!(t.user_input("x"), None);
        assert_eq!(t.takeover("x"), None);
        assert_eq!(t.handback("x"), None);
        assert_eq!(t.state("x"), UserDriving);
    }

    #[test]
    fn table_run_drives_multiple_sessions_and_releases_all() {
        let t = ControlTable::new();
        t.acquire("a", "a");
        t.acquire("b", "a");
        let mut mine = t.sessions_of("a");
        mine.sort();
        assert_eq!(mine, vec!["a", "b"]);
        t.user_input("b");
        let mut ch = t.release_run("a");
        ch.sort_by(|x, y| x.0.cmp(&y.0));
        assert_eq!(ch, vec![("a".to_string(), UserDriving), ("b".to_string(), UserDriving)]);
        assert!(t.sessions_of("a").is_empty());
    }

    #[test]
    fn table_shared_session_returns_to_user_only_when_all_runs_end() {
        let t = ControlTable::new();
        t.acquire("shared", "r1");
        t.acquire("shared", "r2");
        assert_eq!(t.holders_of("shared"), vec!["r1", "r2"]);
        assert!(t.release_run("r1").is_empty(), "r2 还在驾驶，不能还给用户");
        assert_eq!(t.state("shared"), AgentDriving);
        assert_eq!(t.release_run("r2"), vec![("shared".to_string(), UserDriving)]);
    }

    #[test]
    fn table_release_unknown_run_is_noop() {
        let t = ControlTable::new();
        assert!(t.release_run("nope").is_empty());
    }

    #[test]
    fn table_acquire_while_paused_stays_paused() {
        let t = ControlTable::new();
        t.acquire("s", "s");
        t.user_input("s");
        assert_eq!(t.acquire("s", "s"), None);
        assert_eq!(t.state("s"), Paused);
    }

    #[test]
    fn state_serializes_camel_case() {
        assert_eq!(serde_json::to_string(&UserDriving).unwrap(), "\"userDriving\"");
        assert_eq!(serde_json::to_string(&AgentDriving).unwrap(), "\"agentDriving\"");
        assert_eq!(serde_json::to_string(&Paused).unwrap(), "\"paused\"");
    }

    async fn no_sleep(_: Duration) {}

    #[tokio::test]
    async fn wait_not_paused_passes_immediately() {
        let r = wait_unpaused(|| false, || false, Duration::from_secs(5), Duration::from_millis(100), no_sleep)
            .await;
        assert_eq!(r, PauseWait::Ready);
    }

    #[tokio::test]
    async fn wait_passes_after_handback() {
        let polls = AtomicUsize::new(0);
        let r = wait_unpaused(
            || polls.fetch_add(1, Ordering::Relaxed) < 3,
            || false,
            Duration::from_secs(5),
            Duration::from_millis(100),
            no_sleep,
        )
        .await;
        assert_eq!(r, PauseWait::Ready);
    }

    #[tokio::test]
    async fn wait_cancel_takes_priority() {
        let r = wait_unpaused(|| true, || true, Duration::from_secs(5), Duration::from_millis(100), no_sleep)
            .await;
        assert_eq!(r, PauseWait::Cancelled);
    }

    #[tokio::test]
    async fn wait_cancel_mid_wait() {
        let polls = AtomicUsize::new(0);
        let r = wait_unpaused(
            || true,
            || polls.fetch_add(1, Ordering::Relaxed) >= 2,
            Duration::from_secs(5),
            Duration::from_millis(100),
            no_sleep,
        )
        .await;
        assert_eq!(r, PauseWait::Cancelled);
    }

    #[tokio::test]
    async fn wait_timeout_means_user_took_over() {
        let slept = AtomicUsize::new(0);
        let r = wait_unpaused(
            || true,
            || false,
            Duration::from_millis(500),
            Duration::from_millis(100),
            |_| {
                slept.fetch_add(1, Ordering::Relaxed);
                async {}
            },
        )
        .await;
        assert_eq!(r, PauseWait::TookOver);
        assert_eq!(slept.load(Ordering::Relaxed), 5);
    }
}
