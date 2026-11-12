//! 托管现场的恢复判定与契约级预算计量。
//!
//! 早期这里还有守护进程的六档 Timebox、周期 Checkpoint 落盘与恢复队列；PR8 删除守护进程写路径后，
//! 托管任务自己持久化现场（`host::checkpoint::HostCheckpoint` → `host_sessions` 表），
//! 这里只剩它复用的两样东西：
//! - `resume_verdict`：接驳前核对现场，屏幕对不上就把档位降到 `approve`，没帧绝不当成一致；
//! - `Spent`：契约级累计预算（托管任务按它算整份契约用了多少步 / 时长 / 输出）。

use serde::{Deserialize, Serialize};

use super::policy::{Authority, TIER_APPROVE};

/// 看门狗判「长时间无进展」的阈值
pub const NO_PROGRESS_MS: i64 = 15 * 60_000;
/// 超过这个时长的现场不再自动恢复，交给人决定
pub const CHECKPOINT_TTL_MS: i64 = 24 * 60 * 60_000;

/// 托管级累计预算。字段是标量，便于序列化进 Checkpoint。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Spent {
    pub steps: u32,
    pub elapsed_secs: u64,
    pub output_chars: u64,
    pub injections: u32,
}

/// `resume_verdict` 的输入：一次托管的现场摘要（由 `HostCheckpoint` 现拼，不落盘）。
/// 只留判定要看的几项；契约 id / console / 目标 / 授权以 `HostCheckpoint` 为准。
#[derive(Debug, Clone)]
pub struct StewardCheckpoint {
    pub screen_hash: String,
    /// wall-clock，仅用于 TTL
    pub saved_wall_ms: i64,
    /// observing | suspended | completed
    pub state: String,
    pub note: String,
    pub host_key: String,
    /// 首次落盘的墙钟。TTL 从这里算；为 0 时回落到 `saved_wall_ms`。
    pub created_wall_ms: i64,
}

impl StewardCheckpoint {
    /// 签名保留契约 / console / 目标 / 授权四项，与 `HostCheckpoint::reattach_verdict` 的调用对齐。
    pub fn new(_contract_id: &str, _console_id: &str, _goal: &str, _auth: &Authority) -> Self {
        Self {
            screen_hash: String::new(),
            saved_wall_ms: 0,
            state: "observing".to_string(),
            note: String::new(),
            host_key: String::new(),
            created_wall_ms: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum ResumeVerdict {
    /// 现场一致，直接恢复
    Clean,
    /// 屏幕与 checkpoint 不一致：先给差异摘要，档位降为 approve
    Drift { downgrade_to: String },
    /// checkpoint 过期 / 已完成，不自动恢复
    Expired { reason: String },
    /// 上次是断连 / 预算耗尽，等用户确认再续跑
    Suspended { reason: String },
}

/// 恢复判定。启动时扫描未完成契约后调一次。
pub fn resume_verdict(
    cp: &StewardCheckpoint,
    current_hash: Option<&str>,
    now_wall_ms: i64,
) -> ResumeVerdict {
    if cp.state == "completed" {
        return ResumeVerdict::Expired {
            reason: "该契约已完成，无需恢复".to_string(),
        };
    }
    let born = if cp.created_wall_ms > 0 {
        cp.created_wall_ms
    } else {
        cp.saved_wall_ms
    };
    if now_wall_ms - born > CHECKPOINT_TTL_MS {
        return ResumeVerdict::Expired {
            reason: format!(
                "Checkpoint 已超过 {} 小时，现场可能早已变化",
                CHECKPOINT_TTL_MS / 3_600_000
            ),
        };
    }
    if cp.state == "suspended" {
        return ResumeVerdict::Suspended {
            reason: cp.note.clone(),
        };
    }
    // 没有屏幕帧 = 没法核对现场，绝不能当成 Clean 直接恢复写权限；
    // 调用方应当等首帧到了再判一次。
    let Some(h) = current_hash else {
        return ResumeVerdict::Suspended {
            reason: "还没有屏幕帧，无法核对现场：等首帧到达后再判定是否恢复".to_string(),
        };
    };
    // checkpoint 里没存屏幕 hash（契约刚开就退出）同样无从比对，按漂移处理、降到 approve。
    if cp.screen_hash.is_empty() || h != cp.screen_hash {
        return ResumeVerdict::Drift {
            downgrade_to: TIER_APPROVE.to_string(),
        };
    }
    ResumeVerdict::Clean
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_clean_drift_expired_suspended() {
        let mut cp = StewardCheckpoint::new("c1", "tab-1", "目标", &Authority::default());
        cp.screen_hash = "aaaa1111".into();
        cp.saved_wall_ms = 1_000;
        assert_eq!(resume_verdict(&cp, Some("aaaa1111"), 2_000), ResumeVerdict::Clean);
        assert_eq!(
            resume_verdict(&cp, Some("bbbb2222"), 2_000),
            ResumeVerdict::Drift { downgrade_to: "approve".into() }
        );
        assert!(matches!(resume_verdict(&cp, None, CHECKPOINT_TTL_MS + 2_000), ResumeVerdict::Expired { .. }));
        cp.state = "suspended".into();
        cp.note = "SSH 断连".into();
        assert!(matches!(resume_verdict(&cp, Some("aaaa1111"), 2_000), ResumeVerdict::Suspended { .. }));
        cp.state = "completed".into();
        assert!(matches!(resume_verdict(&cp, Some("aaaa1111"), 2_000), ResumeVerdict::Expired { .. }));
    }

    #[test]
    fn resume_without_frame_is_never_clean() {
        let mut cp = StewardCheckpoint::new("c1", "tab-1", "目标", &Authority::default());
        cp.screen_hash = "aaaa1111".into();
        cp.saved_wall_ms = 1_000;
        assert!(matches!(resume_verdict(&cp, None, 2_000), ResumeVerdict::Suspended { .. }));
        // 没存过 hash 的 checkpoint 也无从比对
        cp.screen_hash.clear();
        assert!(matches!(resume_verdict(&cp, Some("x"), 2_000), ResumeVerdict::Drift { .. }));
    }

    #[test]
    fn ttl_counts_from_creation_not_last_periodic_save() {
        let mut cp = StewardCheckpoint::new("c1", "tab-1", "目标", &Authority::default());
        cp.screen_hash = "aaaa1111".into();
        cp.created_wall_ms = 1_000;
        // 周期快照刚刷新过 saved_wall_ms，但契约本身已经开了 24h 以上
        cp.saved_wall_ms = CHECKPOINT_TTL_MS + 1_500;
        assert!(matches!(
            resume_verdict(&cp, Some("aaaa1111"), CHECKPOINT_TTL_MS + 2_000),
            ResumeVerdict::Expired { .. }
        ));
    }

}
