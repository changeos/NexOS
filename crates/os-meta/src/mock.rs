//! Mock 实现（feature gate `mock`）——供下游 agent（discover/guest/provision/update/api）测试用。
//!
//! 约定（_conventions.md §5）：
//! - 实现完整 trait（不 panic 的默认返回）
//! - 提供构造器设置预期返回值（builder 风格）
//! - 纯内存、确定性
//!
//! 5 个 Mock：[`MockConsensus`] / [`MockDistributedKv`] / [`MockMetaStore`] /
//! [`MockFailoverOrchestrator`] / [`MockVipManager`]。
//! 复用 [`crate::impls`] 的内存后端逻辑（行为一致），并叠加"可注入预期返回值"的能力。

#![cfg(feature = "mock")]

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use os_core::{NodeId, NodeInfo, NodeRole, TaskId};
use os_network::IpCidr;

use crate::consensus::{ClusterState, ClusterStatus};
use crate::failover::FailoverStatus;
use crate::failover_sm::FailoverTask;
use crate::impls::{OpenraftConsensus, OpenraftKv, SqliteMetaStore};
use crate::kv::KvEntry;
use crate::meta_store::MetaSnapshot;
use crate::vip::VipConfig;
use crate::{Consensus, DistributedKv, FailoverOrchestrator, MetaError, MetaStore, VipManager};

// ----------------------------------------------------------------------------
// MockConsensus
// ----------------------------------------------------------------------------

/// Mock `Consensus`。
///
/// 默认行为：Standalone 状态、无 leader；可通过 builder 注入成员 / leader / 角色。
pub struct MockConsensus {
    inner: OpenraftConsensus,
    members: Mutex<Vec<NodeInfo>>,
    join_role: Mutex<NodeRole>,
    leave_fails: Mutex<bool>,
}

impl MockConsensus {
    /// 创建默认 mock（Standalone，空成员）。
    pub fn new() -> Self {
        Self {
            inner: OpenraftConsensus::new(),
            members: Mutex::new(Vec::new()),
            join_role: Mutex::new(NodeRole::Follower),
            leave_fails: Mutex::new(false),
        }
    }

    /// 注入成员列表（`get_members` 返回此快照）。
    pub fn with_members(self, members: Vec<NodeInfo>) -> Self {
        *self.members.lock().unwrap() = members;
        self
    }

    /// 注入角色 + leader（`status` 返回此快照）。
    pub fn with_state(self, self_id: NodeId, state: ClusterState, leader: Option<NodeId>) -> Self {
        Self {
            inner: OpenraftConsensus::with_state(self_id, state, leader),
            ..self
        }
    }

    /// 注入 `join_cluster` 返回的角色。
    pub fn with_join_role(self, role: NodeRole) -> Self {
        *self.join_role.lock().unwrap() = role;
        self
    }

    /// 让 `leave_cluster` 失败（用于测试错误路径）。
    pub fn with_leave_failure(self, fails: bool) -> Self {
        *self.leave_fails.lock().unwrap() = fails;
        self
    }
}

impl Default for MockConsensus {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Consensus for MockConsensus {
    async fn join_cluster(&self, _endpoint: String, _token: String) -> Result<NodeRole, MetaError> {
        self.inner.join_cluster(_endpoint, _token).await?;
        Ok(*self.join_role.lock().unwrap())
    }

    async fn leave_cluster(&self) -> Result<(), MetaError> {
        if *self.leave_fails.lock().unwrap() {
            return Err(MetaError::NotMember("mock: leave 已被配置为失败".into()));
        }
        self.inner.leave_cluster().await
    }

    async fn get_leader(&self) -> Option<NodeId> {
        self.inner.get_leader().await
    }

    async fn get_members(&self) -> Vec<NodeInfo> {
        self.members.lock().unwrap().clone()
    }

    async fn status(&self) -> ClusterStatus {
        self.inner.status().await
    }
}

// ----------------------------------------------------------------------------
// MockDistributedKv
// ----------------------------------------------------------------------------

/// Mock `DistributedKv`。
///
/// 默认行为：空 KV，所有操作正常工作；可预填条目。
pub struct MockDistributedKv {
    inner: OpenraftKv,
}

impl MockDistributedKv {
    /// 创建默认 mock（空 KV）。
    pub fn new() -> Self {
        Self {
            inner: OpenraftKv::new(),
        }
    }

    /// 预填条目。
    pub fn with_entries<I>(entries: I) -> Self
    where
        I: IntoIterator<Item = KvEntry>,
    {
        Self {
            inner: OpenraftKv::from_entries(entries),
        }
    }
}

impl Default for MockDistributedKv {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DistributedKv for MockDistributedKv {
    async fn put(&self, key: &str, value: serde_json::Value) -> Result<KvEntry, MetaError> {
        self.inner.put(key, value).await
    }
    async fn get(&self, key: &str) -> Option<KvEntry> {
        self.inner.get(key).await
    }
    async fn delete(&self, key: &str) -> Result<(), MetaError> {
        self.inner.delete(key).await
    }
    async fn list(&self, prefix: &str) -> Vec<KvEntry> {
        self.inner.list(prefix).await
    }
    async fn cas(
        &self,
        key: &str,
        expected_version: Option<u64>,
        new_value: serde_json::Value,
    ) -> Result<KvEntry, MetaError> {
        self.inner.cas(key, expected_version, new_value).await
    }
}

// ----------------------------------------------------------------------------
// MockMetaStore
// ----------------------------------------------------------------------------

/// Mock `MetaStore`。
///
/// 默认行为：空内存状态；apply/snapshot/restore/query 全部基于内存。
pub struct MockMetaStore {
    inner: SqliteMetaStore,
}

impl MockMetaStore {
    /// 创建默认 mock。
    pub fn new() -> Self {
        Self {
            inner: SqliteMetaStore::new(),
        }
    }
}

impl Default for MockMetaStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MetaStore for MockMetaStore {
    async fn apply_log(&self, entry: serde_json::Value) -> Result<(), MetaError> {
        self.inner.apply_log(entry).await
    }
    async fn snapshot(&self) -> Result<MetaSnapshot, MetaError> {
        self.inner.snapshot().await
    }
    async fn restore(&self, snap: MetaSnapshot) -> Result<(), MetaError> {
        self.inner.restore(snap).await
    }
    async fn query(
        &self,
        sql: &str,
        params: Vec<serde_json::Value>,
    ) -> Result<Vec<serde_json::Value>, MetaError> {
        self.inner.query(sql, params).await
    }
}

// ----------------------------------------------------------------------------
// MockFailoverOrchestrator
// ----------------------------------------------------------------------------

/// Mock `FailoverOrchestrator`。
///
/// 默认行为：`detect_failure` 返回 `None`（存活）；
/// `trigger_failover` 入队 Triggered 任务；`failover_status` 查询任务态。
/// 可注入 `detect_failure` 的预期返回值与已知失败原因。
pub struct MockFailoverOrchestrator {
    tasks: Mutex<HashMap<TaskId, FailoverTask>>,
    failure_reason: Mutex<Option<String>>,
}

impl MockFailoverOrchestrator {
    /// 创建默认 mock（节点均存活）。
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            failure_reason: Mutex::new(None),
        }
    }

    /// 注入 `detect_failure` 的预期返回（`Some(reason)` 表示判定失效）。
    pub fn with_failure_detected(self, reason: impl Into<String>) -> Self {
        *self.failure_reason.lock().unwrap() = Some(reason.into());
        self
    }
}

impl Default for MockFailoverOrchestrator {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl FailoverOrchestrator for MockFailoverOrchestrator {
    async fn detect_failure(&self, _node: &NodeId) -> Result<Option<String>, MetaError> {
        Ok(self.failure_reason.lock().unwrap().clone())
    }

    async fn trigger_failover(&self, failed: &NodeId) -> Result<TaskId, MetaError> {
        let task = FailoverTask::new(failed.clone());
        let tid = task.task_id;
        self.tasks.lock().unwrap().insert(tid, task);
        Ok(tid)
    }

    async fn failover_status(&self, task: &TaskId) -> FailoverStatus {
        self.tasks
            .lock()
            .unwrap()
            .get(task)
            .map(|t| t.to_status())
            .unwrap_or(FailoverStatus::Aborted)
    }
}

// ----------------------------------------------------------------------------
// MockVipManager
// ----------------------------------------------------------------------------

/// Mock `VipManager`。
///
/// 默认行为：无 owner；`assign`/`release`/`current_owner` 基于内存态，含冲突检测。
pub struct MockVipManager {
    config: VipConfig,
    owner: Mutex<Option<NodeId>>,
}

impl MockVipManager {
    /// 用 VIP 配置创建。
    pub fn new(config: VipConfig) -> Self {
        Self {
            config,
            owner: Mutex::new(None),
        }
    }

    /// 便利构造（CIDR + 接口）。
    pub fn with_cidr(cidr: IpCidr, interface: impl Into<String>) -> Self {
        Self::new(VipConfig {
            ip: cidr,
            interface: interface.into(),
            current_owner: None,
        })
    }

    /// 预置 owner。
    pub fn with_owner(self, owner: NodeId) -> Self {
        *self.owner.lock().unwrap() = Some(owner);
        self
    }

    /// 当前配置快照。
    pub fn config(&self) -> VipConfig {
        let owner = self.owner.lock().unwrap().clone();
        VipConfig {
            ip: self.config.ip,
            interface: self.config.interface.clone(),
            current_owner: owner,
        }
    }
}

#[async_trait]
impl VipManager for MockVipManager {
    async fn assign(&self, node: &NodeId) -> Result<(), MetaError> {
        let mut g = self.owner.lock().unwrap();
        if let Some(current) = g.as_ref() {
            if current != node {
                return Err(MetaError::VipConflict(format!(
                    "VIP 已被节点 {} 持有",
                    current
                )));
            }
            return Ok(());
        }
        *g = Some(node.clone());
        Ok(())
    }

    async fn release(&self) -> Result<(), MetaError> {
        *self.owner.lock().unwrap() = None;
        Ok(())
    }

    async fn current_owner(&self) -> Option<NodeId> {
        self.owner.lock().unwrap().clone()
    }
}

// ----------------------------------------------------------------------------
// 编译期断言：所有 mock 都满足 `Box<dyn Trait>`（dyn 兼容性）
// ----------------------------------------------------------------------------

#[allow(dead_code)]
fn _assert_dyn_compatible(
    _c: Box<dyn Consensus>,
    _k: Box<dyn DistributedKv>,
    _m: Box<dyn MetaStore>,
    _f: Box<dyn FailoverOrchestrator>,
    _v: Box<dyn VipManager>,
) {
    // 占位：证明 5 个 trait 都是对象安全的（原生 async fn 不进 vtable 时的 E0038 防线）
}

// ----------------------------------------------------------------------------
// mock 自测（确保 builder 行为正确）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consensus::ClusterState;
    use os_core::Utc;
    use serde_json::json;

    #[tokio::test]
    async fn mock_kv_works() {
        let kv = MockDistributedKv::new();
        let e = kv.put("a", json!(1)).await.unwrap();
        assert_eq!(e.version, 1);
        let e2 = kv.cas("a", Some(1), json!(2)).await.unwrap();
        assert_eq!(e2.version, 2);
    }

    #[tokio::test]
    async fn mock_consensus_members_injected() {
        let m = MockConsensus::new()
            .with_state(
                NodeId::new("self"),
                ClusterState::Leader,
                Some(NodeId::new("self")),
            )
            .with_members(vec![fake_node("n1"), fake_node("n2")]);
        let st = m.status().await;
        assert_eq!(st.state, ClusterState::Leader);
        assert_eq!(m.get_members().await.len(), 2);
        let role = m.join_cluster("x".into(), "t".into()).await.unwrap();
        assert_eq!(role, NodeRole::Follower);
    }

    #[tokio::test]
    async fn mock_consensus_leave_failure() {
        let m = MockConsensus::new().with_leave_failure(true);
        let err = m.leave_cluster().await.unwrap_err();
        assert!(matches!(err, MetaError::NotMember(_)));
    }

    #[tokio::test]
    async fn mock_failover_detect_injected() {
        let fo = MockFailoverOrchestrator::new().with_failure_detected("探活超时");
        let n = NodeId::new("n1");
        assert_eq!(
            fo.detect_failure(&n).await.unwrap().as_deref(),
            Some("探活超时")
        );
        let tid = fo.trigger_failover(&n).await.unwrap();
        assert!(matches!(
            fo.failover_status(&tid).await,
            FailoverStatus::Running { .. }
        ));
    }

    #[tokio::test]
    async fn mock_meta_store_roundtrip() {
        let s = MockMetaStore::new();
        s.apply_log(json!({"op":"put","table":"t","key":"k","value":1}))
            .await
            .unwrap();
        let snap = s.snapshot().await.unwrap();
        let s2 = MockMetaStore::new();
        s2.restore(snap).await.unwrap();
        let rows = s2.query("SELECT * FROM t", vec![]).await.unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn mock_vip_conflict() {
        let cidr = IpCidr::new("10.0.0.9".parse().unwrap(), 24);
        let m = MockVipManager::with_cidr(cidr, "br0").with_owner(NodeId::new("n1"));
        let err = m.assign(&NodeId::new("n2")).await.unwrap_err();
        assert!(matches!(err, MetaError::VipConflict(_)));
        assert_eq!(m.current_owner().await, Some(NodeId::new("n1")));
    }

    fn fake_node(id: &str) -> NodeInfo {
        NodeInfo {
            node_id: NodeId::new(id),
            role: NodeRole::Follower,
            version: "0.1.0".into(),
            arch: "x86_64".into(),
            endpoints: vec![],
            health: os_core::Health::Healthy,
        }
    }

    // 时间锚点断言（mock 不直接用 Utc，此处确保 chrono 时钟可用）
    #[test]
    fn _utc_anchor() {
        let _ = Utc::now();
    }
}
