//! 托管契约的授权表：`StewardCore`。
//!
//! 早期这里聚合了所有权、Checkpoint、六档 Timebox、预算、撤销窗口、Planner 开关、写锁分片、
//! LLM 并发闸、恢复队列等守护进程写路径的状态。统一托管之后这些都由托管任务自己持有
//! （`agent/host`：相位、`HostCheckpoint`、契约级 `Spent`、ControlTable 写租约），
//! PR8 删掉了守护进程的写路径，这里只剩「契约 id → Authority」一张表：
//! 签约时登记、`host_amend` 改档时更新、托管任务起步 / 接驳时读取。
//!
//! 内部容器是 `Arc<Mutex<_>>`，与既有 `StewardBrain` / `EventLog` 的惯例一致。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::policy::Authority;

#[derive(Clone, Default)]
pub struct StewardCore {
    authorities: Arc<Mutex<HashMap<String, Authority>>>,
}

impl StewardCore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 契约的授权；没登记过的契约返回默认授权（fail-closed 的默认档）。
    pub fn authority(&self, contract_id: &str) -> Authority {
        self.authorities
            .lock()
            .unwrap()
            .get(contract_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn set_authority(&self, contract_id: &str, auth: Authority) -> Authority {
        let a = auth.clamped();
        self.authorities
            .lock()
            .unwrap()
            .insert(contract_id.to_string(), a.clone());
        a
    }

    /// 契约结束：授权一并摘掉。
    pub fn drop_contract(&self, contract_id: &str) {
        self.authorities.lock().unwrap().remove(contract_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_roundtrip_and_drop() {
        let c = StewardCore::new();
        assert_eq!(c.authority("c1").tier, Authority::default().tier, "未登记 → 默认授权");
        c.set_authority("c1", Authority::default().with_tier("approve"));
        assert_eq!(c.authority("c1").tier, "approve");
        c.drop_contract("c1");
        assert_eq!(c.authority("c1").tier, Authority::default().tier);
    }
}
