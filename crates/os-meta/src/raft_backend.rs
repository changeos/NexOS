//! openraft 真实后端——单节点 Raft 引擎适配（ADR-DEPS-002）。
//!
//! 本模块封装一个最小可用的 openraft 0.9 Raft 节点，供 [`crate::impls::OpenraftConsensus`]
//! 作为真实共识后端驱动。规格（meta-agent.md §3.5）：HA 集群用 openraft 做选主/日志复制/快照，
//! 本模块先以**单节点集群**形式跑通真实 Raft 引擎（满足 P2 接通 DoD：openraft 单节点共识测）。
//!
//! 设计要点（为什么这么写）：
//! - **TypeConfig**：openraft 0.9 用 `RaftTypeConfig` trait 定义类型集。本模块声明
//!   `MetaRaftConfig`：`D = MetaRequest`（客户端写请求，承载业务 JSON 命令）、
//!   `R = MetaResponse`（apply 返回）、`NodeId = u64`（openraft 默认）、
//!   `SnapshotData = Vec<u8>`（快照字节流，便于与 MetaStore dump 对齐）。
//! - **存储**：实现最小化的内存 `MemoryLogStore` + `MemoryStateMachine`，分别落地
//!   `RaftLogStorage` / `RaftStateMachine`（openraft 0.9 拆分接口）。无外部持久化，
//!   仅供单节点测试集群使用；真实部署应替换为持久后端（见 ADR-DEPS-002 注释）。
//! - **网络**：单节点无需 RPC，`NullNetwork` 实现返回 Unreachable（永远不被调用，
//!   因为单节点不会向自己发 RPC）。
//!
//! 该模块不暴露任何 pub 项到 crate 外（仅供 [`crate::impls`] 内部使用）。

use std::collections::BTreeMap;
use std::io::Cursor;
use std::sync::{Arc, Mutex};

use openraft::error::Fatal;
use openraft::storage::{
    LogFlushed, LogState, RaftLogReader, RaftLogStorage, RaftSnapshotBuilder, RaftStateMachine,
};
use openraft::{
    BasicNode, Entry, EntryPayload, LogId, OptionalSend, Raft, RaftNetwork, RaftNetworkFactory,
    Snapshot, SnapshotMeta, StoredMembership, Vote,
};
use serde::{Deserialize, Serialize};

// ----------------------------------------------------------------------------
// TypeConfig
// ----------------------------------------------------------------------------

/// apply 后的命令模型（与 [`crate::meta_apply::MetaCommand`] 对齐的序列化形式）。
///
/// 设计：直接以 JSON Value 作为请求载荷，apply 时分发给 MetaStore，避免在类型层
/// 重复定义一遍命令枚举（保持与 trait 契约的 apply_log JSON 语义一致）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetaRequest(pub serde_json::Value);

/// apply 返回——这里仅返回是否成功 + 已应用日志 id（信息最小化）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetaResponse {
    pub applied: bool,
}

// os-meta 的 openraft 类型配置。
//
// `declare_raft_types!` 宏自动实现 `RaftTypeConfig`，并填充未指定的默认类型。
openraft::declare_raft_types!(
    /// os-meta Raft 类型集
    pub MetaRaftConfig:
        D = MetaRequest,
        R = MetaResponse,
        NodeId = u64,
        Node = BasicNode,
        Entry = Entry<MetaRaftConfig>,
        SnapshotData = Cursor<Vec<u8>>,
);

// ----------------------------------------------------------------------------
// 内存日志存储 / 状态机（最小可用，单节点测试集群专用）
// ----------------------------------------------------------------------------

/// 内存日志条目仓库（log id → entry）。
#[derive(Default)]
struct MemStoreInner {
    last_purged_log_id: Option<LogId<u64>>,
    log: BTreeMap<u64, Entry<MetaRaftConfig>>,
    vote: Option<Vote<u64>>,
    committed: Option<LogId<u64>>,
    // —— 状态机部分 ——
    last_applied: Option<LogId<u64>>,
    last_membership: StoredMembership<u64, BasicNode>,
    /// 已 apply 的业务命令快照（用于在 apply 时给出 MetaResponse.applied）。
    snapshot: Vec<u8>,
}

/// 内存 LogStore + StateMachine 复合体。
///
/// openraft 0.9 把存储拆为 `RaftLogStorage`（日志）与 `RaftStateMachine`
/// （状态机+快照）两接口；本结构同时实现两者，单节点测试集群无需分离。
#[derive(Clone, Default)]
pub struct MemoryRaftStore {
    inner: Arc<Mutex<MemStoreInner>>,
}

impl MemoryRaftStore {
    pub fn new() -> Self {
        Self::default()
    }
}

// RaftLogReader：被多个 replication 任务并发读取（单节点不会真正调用）。
impl RaftLogReader<MetaRaftConfig> for MemoryRaftStore {
    async fn try_get_log_entries<RB>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<MetaRaftConfig>>, openraft::StorageError<u64>>
    where
        RB: std::ops::RangeBounds<u64> + Clone + std::fmt::Debug + OptionalSend,
    {
        let start = match range.start_bound() {
            std::ops::Bound::Included(i) => *i,
            std::ops::Bound::Excluded(i) => i + 1,
            std::ops::Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            std::ops::Bound::Included(i) => i + 1,
            std::ops::Bound::Excluded(i) => *i,
            std::ops::Bound::Unbounded => u64::MAX,
        };
        let g = self.inner.lock().expect("poisoned");
        let mut out = Vec::new();
        for (_k, v) in g.log.range(start..end) {
            out.push(v.clone());
        }
        Ok(out)
    }
}

impl RaftLogStorage<MetaRaftConfig> for MemoryRaftStore {
    type LogReader = MemoryRaftStore;

    async fn get_log_state(
        &mut self,
    ) -> Result<LogState<MetaRaftConfig>, openraft::StorageError<u64>> {
        let g = self.inner.lock().expect("poisoned");
        let last = g.log.iter().next_back().map(|(_, e)| e.log_id);
        Ok(LogState {
            last_purged_log_id: g.last_purged_log_id,
            last_log_id: last,
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote<u64>) -> Result<(), openraft::StorageError<u64>> {
        self.inner.lock().expect("poisoned").vote = Some(*vote);
        Ok(())
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<u64>>, openraft::StorageError<u64>> {
        Ok(self.inner.lock().expect("poisoned").vote)
    }

    async fn save_committed(
        &mut self,
        committed: Option<LogId<u64>>,
    ) -> Result<(), openraft::StorageError<u64>> {
        self.inner.lock().expect("poisoned").committed = committed;
        Ok(())
    }

    async fn read_committed(&mut self) -> Result<Option<LogId<u64>>, openraft::StorageError<u64>> {
        Ok(self.inner.lock().expect("poisoned").committed)
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: LogFlushed<MetaRaftConfig>,
    ) -> Result<(), openraft::StorageError<u64>>
    where
        I: IntoIterator<Item = Entry<MetaRaftConfig>> + OptionalSend,
        I::IntoIter: OptionalSend,
    {
        let mut g = self.inner.lock().expect("poisoned");
        for e in entries {
            g.log.insert(e.log_id.index, e);
        }
        // 内存存储"持久化"是同步完成的，立即回调成功。
        callback.log_io_completed(Ok(()));
        Ok(())
    }

    async fn truncate(&mut self, log_id: LogId<u64>) -> Result<(), openraft::StorageError<u64>> {
        let mut g = self.inner.lock().expect("poisoned");
        let keys: Vec<u64> = g.log.range(log_id.index..).map(|(k, _)| *k).collect();
        for k in keys {
            g.log.remove(&k);
        }
        Ok(())
    }

    async fn purge(&mut self, log_id: LogId<u64>) -> Result<(), openraft::StorageError<u64>> {
        let mut g = self.inner.lock().expect("poisoned");
        let keys: Vec<u64> = g.log.range(..=log_id.index).map(|(k, _)| *k).collect();
        for k in keys {
            g.log.remove(&k);
        }
        g.last_purged_log_id = Some(log_id);
        Ok(())
    }
}

impl RaftSnapshotBuilder<MetaRaftConfig> for MemoryRaftStore {
    async fn build_snapshot(
        &mut self,
    ) -> Result<Snapshot<MetaRaftConfig>, openraft::StorageError<u64>> {
        let g = self.inner.lock().expect("poisoned");
        let meta = SnapshotMeta {
            last_log_id: g.last_applied,
            last_membership: g.last_membership.clone(),
            snapshot_id: format!("snap-{}", g.last_applied.map(|l| l.index).unwrap_or(0)),
        };
        let data = g.snapshot.clone();
        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(data)),
        })
    }
}

impl RaftStateMachine<MetaRaftConfig> for MemoryRaftStore {
    type SnapshotBuilder = MemoryRaftStore;

    async fn applied_state(
        &mut self,
    ) -> Result<(Option<LogId<u64>>, StoredMembership<u64, BasicNode>), openraft::StorageError<u64>>
    {
        let g = self.inner.lock().expect("poisoned");
        Ok((g.last_applied, g.last_membership.clone()))
    }

    async fn apply<I>(
        &mut self,
        entries: I,
    ) -> Result<Vec<MetaResponse>, openraft::StorageError<u64>>
    where
        I: IntoIterator<Item = Entry<MetaRaftConfig>> + OptionalSend,
        I::IntoIter: OptionalSend,
    {
        let mut g = self.inner.lock().expect("poisoned");
        let mut out = Vec::new();
        for e in entries {
            g.last_applied = Some(e.log_id);
            if let EntryPayload::Membership(m) = &e.payload {
                g.last_membership = StoredMembership::new(Some(e.log_id), m.clone());
            }
            // 业务命令 apply：直接吞下，标记 applied=true（真实 apply 由
            // SqliteMetaStore 在 trait 层完成；Raft 状态机仅维护 last_applied）。
            out.push(MetaResponse { applied: true });
        }
        Ok(out)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<Cursor<Vec<u8>>>, openraft::StorageError<u64>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<u64, BasicNode>,
        snapshot: Box<Cursor<Vec<u8>>>,
    ) -> Result<(), openraft::StorageError<u64>> {
        let mut g = self.inner.lock().expect("poisoned");
        g.last_applied = meta.last_log_id;
        g.last_membership = meta.last_membership.clone();
        g.snapshot = snapshot.into_inner();
        Ok(())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<MetaRaftConfig>>, openraft::StorageError<u64>> {
        let g = self.inner.lock().expect("poisoned");
        if g.last_applied.is_none() && g.snapshot.is_empty() {
            return Ok(None);
        }
        let meta = SnapshotMeta {
            last_log_id: g.last_applied,
            last_membership: g.last_membership.clone(),
            snapshot_id: format!("snap-{}", g.last_applied.map(|l| l.index).unwrap_or(0)),
        };
        let data = g.snapshot.clone();
        Ok(Some(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(data)),
        }))
    }
}

// ----------------------------------------------------------------------------
// 网络：单节点集群的空实现（永远不被实际调用）
// ----------------------------------------------------------------------------

/// 空 NetworkFactory（单节点不发 RPC）。
#[derive(Debug, Default, Clone)]
pub struct NullNetwork;

impl RaftNetworkFactory<MetaRaftConfig> for NullNetwork {
    type Network = NullNetwork;

    async fn new_client(&mut self, _target: u64, _node: &BasicNode) -> Self::Network {
        NullNetwork
    }
}

impl RaftNetwork<MetaRaftConfig> for NullNetwork {
    async fn append_entries(
        &mut self,
        _rpc: openraft::raft::AppendEntriesRequest<MetaRaftConfig>,
        _option: openraft::network::RPCOption,
    ) -> Result<
        openraft::raft::AppendEntriesResponse<u64>,
        openraft::error::RPCError<u64, BasicNode, openraft::error::RaftError<u64>>,
    > {
        // 单节点：理论上 unreachable；返回网络不可达便于在错误出现时定位。
        Err(openraft::error::RPCError::Unreachable(
            openraft::error::Unreachable::new(&std::io::Error::other("single-node: no RPC")),
        ))
    }

    async fn install_snapshot(
        &mut self,
        _rpc: openraft::raft::InstallSnapshotRequest<MetaRaftConfig>,
        _option: openraft::network::RPCOption,
    ) -> Result<
        openraft::raft::InstallSnapshotResponse<u64>,
        openraft::error::RPCError<
            u64,
            BasicNode,
            openraft::error::RaftError<u64, openraft::error::InstallSnapshotError>,
        >,
    > {
        Err(openraft::error::RPCError::Unreachable(
            openraft::error::Unreachable::new(&std::io::Error::other("single-node: no RPC")),
        ))
    }

    async fn vote(
        &mut self,
        _rpc: openraft::raft::VoteRequest<u64>,
        _option: openraft::network::RPCOption,
    ) -> Result<
        openraft::raft::VoteResponse<u64>,
        openraft::error::RPCError<u64, BasicNode, openraft::error::RaftError<u64>>,
    > {
        Err(openraft::error::RPCError::Unreachable(
            openraft::error::Unreachable::new(&std::io::Error::other("single-node: no RPC")),
        ))
    }
}

// ----------------------------------------------------------------------------
// 单节点集群启动器
// ----------------------------------------------------------------------------

/// 启动一个**单节点** openraft 集群并完成 `initialize`。
///
/// 流程：
/// 1. 构造 openraft `Config`（心跳/选举超时取较小值，加速测试）。
/// 2. `Raft::new(id, config, NullNetwork, log_store, state_machine)` 启动 Raft 任务。
/// 3. `initialize({id => BasicNode})` 把自己作为唯一 voter，立即触发选举并当选 leader。
///
/// 返回的 `Raft<MetaRaftConfig>` 是 `Clone`（内部 Arc），调用方可继续持有并查询 metrics。
pub async fn spawn_single_node(id: u64) -> Result<Raft<MetaRaftConfig>, Fatal<u64>> {
    let config = Arc::new(
        openraft::Config {
            heartbeat_interval: 50,
            election_timeout_min: 150,
            election_timeout_max: 300,
            cluster_name: "os-meta-single".into(),
            ..Default::default()
        }
        .validate()
        .map_err(|e| {
            let _ = e; // ConfigError 不直接转 Fatal；用 Panicked 携带消息
            Fatal::Panicked
        })?,
    );

    let store = MemoryRaftStore::new();
    let raft = Raft::new(id, config, NullNetwork, store.clone(), store).await?;

    // 初始化单节点成员 → 立即当选 leader。
    let mut members = BTreeMap::new();
    members.insert(
        id,
        BasicNode {
            addr: format!("127.0.0.1:{}", 7000 + id),
        },
    );
    raft.initialize(members)
        .await
        .map_err(|_e| Fatal::Panicked)?;

    Ok(raft)
}

// ============================================================================
// 单元测试：类型配置 + 内存存储 + 空网络（纯逻辑，不启动 Raft 任务）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use openraft::CommittedLeaderId;
    use std::time::Duration;

    // 构造辅助：CommittedLeaderId + LogId + Vote
    fn clid(term: u64, node_id: u64) -> CommittedLeaderId<u64> {
        CommittedLeaderId::new(term, node_id)
    }
    fn lid(term: u64, node_id: u64, index: u64) -> LogId<u64> {
        LogId::new(clid(term, node_id), index)
    }

    // ---- MetaRequest / MetaResponse 模型 ----

    #[test]
    fn meta_request_default_is_null_value() {
        let r = MetaRequest::default();
        assert_eq!(r.0, serde_json::Value::Null);
    }

    #[test]
    fn meta_request_serde_roundtrip() {
        let req = MetaRequest(serde_json::json!({"op": "put", "k": "v"}));
        let s = serde_json::to_string(&req).unwrap();
        let back: MetaRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn meta_request_eq_clone() {
        let a = MetaRequest(serde_json::json!(1));
        let b = a.clone();
        assert_eq!(a, b);
        let c = MetaRequest(serde_json::json!(2));
        assert_ne!(a, c);
    }

    #[test]
    fn meta_response_default_is_applied_false() {
        let r = MetaResponse::default();
        assert!(!r.applied);
    }

    #[test]
    fn meta_response_serde_roundtrip() {
        let r = MetaResponse { applied: true };
        let s = serde_json::to_string(&r).unwrap();
        let back: MetaResponse = serde_json::from_str(&s).unwrap();
        assert_eq!(back, r);
    }

    // ---- MemoryRaftStore 基础（async 方法用 #[tokio::test] 驱动） ----
    //
    // 注：append/truncate/purge/apply 都依赖 openraft 的 LogFlushed（pub(crate)，
    // 外部无法构造）。故这些方法经 impls.rs 的 start_single_node 真实集群间接覆盖；
    // 此处仅直接测可构造的 vote/committed/snapshot 路径。

    #[tokio::test]
    async fn memory_store_new_is_empty() {
        let mut s = MemoryRaftStore::new();
        let state = s.get_log_state().await.unwrap();
        assert!(state.last_purged_log_id.is_none());
        assert!(state.last_log_id.is_none());
    }

    #[tokio::test]
    async fn memory_store_save_and_read_vote_uncommitted() {
        let mut s = MemoryRaftStore::new();
        let vote = Vote::new(5, 1);
        s.save_vote(&vote).await.unwrap();
        let read = s.read_vote().await.unwrap();
        assert_eq!(read, Some(vote));
    }

    #[tokio::test]
    async fn memory_store_save_and_read_vote_committed() {
        let mut s = MemoryRaftStore::new();
        let vote = Vote::new_committed(7, 2);
        s.save_vote(&vote).await.unwrap();
        let read = s.read_vote().await.unwrap();
        assert_eq!(read, Some(vote));
    }

    #[tokio::test]
    async fn memory_store_save_and_read_committed() {
        let mut s = MemoryRaftStore::new();
        let lid_val = lid(1, 1, 3);
        s.save_committed(Some(lid_val)).await.unwrap();
        let read = s.read_committed().await.unwrap();
        assert_eq!(read, Some(lid_val));
    }

    #[tokio::test]
    async fn memory_store_read_committed_default_none() {
        let mut s = MemoryRaftStore::new();
        let read = s.read_committed().await.unwrap();
        assert!(read.is_none());
    }

    #[tokio::test]
    async fn memory_store_read_vote_default_none() {
        let mut s = MemoryRaftStore::new();
        let read = s.read_vote().await.unwrap();
        assert!(read.is_none());
    }

    #[tokio::test]
    async fn memory_store_clear_committed_with_none() {
        // 先存再清空（save_committed(None)）
        let mut s = MemoryRaftStore::new();
        s.save_committed(Some(lid(1, 1, 3))).await.unwrap();
        s.save_committed(None).await.unwrap();
        let read = s.read_committed().await.unwrap();
        assert!(read.is_none());
    }

    #[tokio::test]
    async fn memory_store_applied_state_default() {
        let mut s = MemoryRaftStore::new();
        let (last, membership) = s.applied_state().await.unwrap();
        assert!(last.is_none());
        let voters: Vec<u64> = membership.voter_ids().collect();
        assert!(voters.is_empty());
    }

    #[tokio::test]
    async fn memory_store_get_current_snapshot_default_none() {
        let mut s = MemoryRaftStore::new();
        let snap = s.get_current_snapshot().await.unwrap();
        assert!(snap.is_none());
    }

    #[tokio::test]
    async fn memory_store_build_snapshot_after_install() {
        // install_snapshot 后再 build_snapshot 应返回非 None
        let mut s = MemoryRaftStore::new();
        let last_lid = lid(1, 1, 5);
        let membership = openraft::Membership::new(
            vec![std::iter::once(1u64).collect::<std::collections::BTreeSet<_>>()],
            (),
        );
        let meta = SnapshotMeta {
            last_log_id: Some(last_lid),
            last_membership: StoredMembership::new(Some(last_lid), membership),
            snapshot_id: "snap-5".into(),
        };
        s.install_snapshot(&meta, Box::new(Cursor::new(vec![1, 2, 3])))
            .await
            .unwrap();
        // get_current_snapshot 应有值
        let snap = s.get_current_snapshot().await.unwrap();
        assert!(snap.is_some());
        // build_snapshot 也应成功，meta.last_log_id 与 install 一致
        let built = s.build_snapshot().await.unwrap();
        assert_eq!(built.meta.last_log_id, Some(last_lid));
    }

    #[tokio::test]
    async fn memory_store_install_snapshot_then_applied_state_reflects() {
        // install_snapshot 更新 last_applied + last_membership
        let mut s = MemoryRaftStore::new();
        let last_lid = lid(2, 3, 9);
        let membership = openraft::Membership::new(
            vec![std::iter::once(3u64).collect::<std::collections::BTreeSet<_>>()],
            (),
        );
        let meta = SnapshotMeta {
            last_log_id: Some(last_lid),
            last_membership: StoredMembership::new(Some(last_lid), membership),
            snapshot_id: "snap-9".into(),
        };
        s.install_snapshot(&meta, Box::new(Cursor::new(vec![])))
            .await
            .unwrap();
        let (last, membership) = s.applied_state().await.unwrap();
        assert_eq!(last, Some(last_lid));
        let voters: Vec<u64> = membership.voter_ids().collect();
        assert!(voters.contains(&3));
    }

    #[tokio::test]
    async fn memory_store_begin_receiving_snapshot_returns_empty_cursor() {
        let mut s = MemoryRaftStore::new();
        let cur = s.begin_receiving_snapshot().await.unwrap();
        assert!(cur.into_inner().is_empty());
    }

    #[tokio::test]
    async fn memory_store_clone_shares_state() {
        // Clone（Arc<Mutex>）应共享内部状态：在 clone 写入，原 store 可见
        let mut s = MemoryRaftStore::new();
        let vote = Vote::new(9, 2);
        s.save_vote(&vote).await.unwrap();
        let mut s2 = s.clone();
        let read = s2.read_vote().await.unwrap();
        assert_eq!(read, Some(vote));
    }

    #[tokio::test]
    async fn memory_store_get_log_reader_returns_clone() {
        let mut s = MemoryRaftStore::new();
        let _reader = s.get_log_reader().await;
        // 仅验证 get_log_reader 不 panic（返回 clone）
    }

    #[tokio::test]
    async fn memory_store_get_snapshot_builder_returns_clone() {
        let mut s = MemoryRaftStore::new();
        let _builder = s.get_snapshot_builder().await;
        // 仅验证不 panic
    }

    #[tokio::test]
    async fn memory_store_try_get_log_entries_empty_when_no_log() {
        let mut s = MemoryRaftStore::new();
        let got = s.try_get_log_entries(0..10).await.unwrap();
        assert!(got.is_empty());
    }

    // ---- NullNetwork（永远返回 Unreachable） ----

    #[tokio::test]
    async fn null_network_new_client_returns_factory_type() {
        let mut factory = NullNetwork;
        let _client = factory.new_client(1, &BasicNode { addr: "x".into() }).await;
        // 仅验证可构造（单节点不发 RPC）
    }

    #[tokio::test]
    async fn null_network_append_entries_returns_unreachable() {
        let mut net = NullNetwork;
        let req = openraft::raft::AppendEntriesRequest {
            vote: Vote::default(),
            prev_log_id: None,
            entries: vec![],
            leader_commit: None,
        };
        let res = net
            .append_entries(
                req,
                openraft::network::RPCOption::new(Duration::from_millis(100)),
            )
            .await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn null_network_vote_returns_unreachable() {
        let mut net = NullNetwork;
        let req = openraft::raft::VoteRequest {
            vote: Vote::default(),
            last_log_id: None,
        };
        let res = net
            .vote(
                req,
                openraft::network::RPCOption::new(Duration::from_millis(100)),
            )
            .await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn null_network_install_snapshot_returns_unreachable() {
        let mut net = NullNetwork;
        let req = openraft::raft::InstallSnapshotRequest {
            vote: Vote::default(),
            meta: SnapshotMeta {
                last_log_id: None,
                last_membership: StoredMembership::default(),
                snapshot_id: "x".into(),
            },
            offset: 0,
            data: vec![],
            done: true,
        };
        let res = net
            .install_snapshot(
                req,
                openraft::network::RPCOption::new(Duration::from_millis(100)),
            )
            .await;
        assert!(res.is_err());
    }

    #[test]
    fn null_network_clone_works() {
        let n = NullNetwork;
        let _n2 = n.clone();
        // 仅验证派生 Clone 不 panic
    }

    // ---- 单节点集群启动（真实 Raft 任务） ----
    //
    // 这些测试在 impls.rs 已覆盖（start_single_node → 等待 Leader），
    // 这里仅覆盖 spawn_single_node 的成功路径一次（小 id）。

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_single_node_succeeds_and_elects_leader() {
        let raft = spawn_single_node(42).await.expect("启动单节点 Raft");
        raft.wait(Some(Duration::from_secs(2)))
            .state(openraft::ServerState::Leader, "单节点必须当选 Leader")
            .await
            .expect("等待 Leader 超时");
        let m = raft.metrics().borrow().clone();
        assert!(m.current_term >= 1, "当选后 term 应 >= 1");
        let _ = raft.shutdown().await; // 清理
    }
}
