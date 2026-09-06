//! 共识核心——openraft 封装
//!
//! 决策依据：规划文档 §3.5 —— HA 集群用 openraft 做 Raft 共识（选主/日志复制/快照）。
//! 本模块仅定义 `Consensus` trait（async）与集群元数据结构。
//!
//! 设计要点：
//! - `join_cluster`：新节点携带凭证加入既有集群，返回被分配的角色（通常 Follower）
//! - `leave_cluster`：节点主动退出（先做日志追赶，再从成员列表移除）
//! - 写操作（KV/Failover）经 leader 转发；非 leader 节点返回 `NotLeader`

use async_trait::async_trait;
use os_core::{DateTime, Deserialize, NodeId, NodeInfo, NodeRole, Serialize};

// ----------------------------------------------------------------------------
// 集群元数据
// ----------------------------------------------------------------------------

/// 集群配置（openraft 集群拓扑）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterConfig {
    /// 集群 ID（全局唯一，创建集群时生成）
    pub cluster_id: String,
    /// 当前成员节点列表
    pub nodes: Vec<NodeInfo>,
    /// 法定人数（quorum = floor(members/2) + 1）
    pub quorum_size: u32,
    /// 当前任期 leader（None = 选举中/无 leader）
    pub leader: Option<NodeId>,
}

/// 集群运行状态机角色
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusterState {
    /// leader（openraft 当前任期当选）
    Leader,
    /// follower（法定成员，跟随 leader）
    Follower,
    /// candidate（选举中）
    Candidate,
    /// 离线（与多数成员失联）
    Offline,
    /// 独立单节点（未加入任何集群）
    Standalone,
}

/// 集群运行状态（Raft 关键索引 + 角色）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterStatus {
    /// 本节点当前角色
    pub state: ClusterState,
    /// 当前任期 leader
    pub leader: Option<NodeId>,
    /// Raft term（任期号）
    pub term: u64,
    /// 已提交日志索引（committed index）
    pub commit_index: u64,
    /// 已应用日志索引（applied 到状态机）
    pub applied_index: u64,
    /// 状态采集时间
    pub checked_at: DateTime,
}

// ----------------------------------------------------------------------------
// Consensus trait（async，openraft 封装）
// ----------------------------------------------------------------------------

/// 共识核心——封装 openraft，提供集群成员管理与状态查询。
///
/// 实现者：`OpenraftConsensus`（默认，内嵌 SQLite 状态机）；其他实现可替换。
/// 写路径经 leader 转发；非 leader 节点的写操作返回 `NotLeader`。
///
/// 注：按 ADR-COMPAT-001，本 trait 经 `Box<dyn Consensus>` 运行期多态（见 mock.rs
/// `_assert_dyn_compatible`），故用 `#[async_trait]`；方法签名未变。
#[async_trait]
pub trait Consensus: Send + Sync {
    /// 加入既有集群。
    ///
    /// - `endpoint`：已知成员（通常为 leader）的接入地址
    /// - `token`：加入凭证（由 discovery/mTLS 配对签发）
    ///
    /// 成功返回被分配的角色（通常 Follower）。
    async fn join_cluster(
        &self,
        endpoint: String,
        token: String,
    ) -> Result<NodeRole, crate::MetaError>;

    /// 主动退出集群（先追赶日志，再从成员列表移除）。
    async fn leave_cluster(&self) -> Result<(), crate::MetaError>;

    /// 查询当前任期 leader；选举中/无 leader 返回 None。
    async fn get_leader(&self) -> Option<NodeId>;

    /// 查询当前成员列表。
    async fn get_members(&self) -> Vec<NodeInfo>;

    /// 查询集群运行状态。
    async fn status(&self) -> ClusterStatus;
}

// ----------------------------------------------------------------------------
// 单元测试：集群元数据模型 serde 往返 + Display
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use os_core::{NodeInfo, Utc};

    fn sample_node_info() -> NodeInfo {
        NodeInfo {
            node_id: os_core::NodeId::new("n1"),
            role: NodeRole::Leader,
            version: "0.1.0".into(),
            arch: "x86_64".into(),
            endpoints: vec!["10.0.0.1:7946".into()],
            health: os_core::Health::Healthy,
        }
    }

    // ---- ClusterConfig ----

    #[test]
    fn cluster_config_serde_roundtrip() {
        let cfg = ClusterConfig {
            cluster_id: "cluster-1".into(),
            nodes: vec![sample_node_info()],
            quorum_size: 2,
            leader: Some(os_core::NodeId::new("n1")),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: ClusterConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cluster_id, cfg.cluster_id);
        assert_eq!(back.quorum_size, cfg.quorum_size);
        assert_eq!(back.nodes.len(), 1);
        assert_eq!(back.leader.as_ref().map(|n| n.as_str()), Some("n1"));
    }

    #[test]
    fn cluster_config_leader_none_serializes() {
        let cfg = ClusterConfig {
            cluster_id: "c".into(),
            nodes: vec![],
            quorum_size: 1,
            leader: None,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("\"leader\":null"));
    }

    // ---- ClusterState ----

    #[test]
    fn cluster_state_all_variants_serde_snake_case_roundtrip() {
        for s in [
            ClusterState::Leader,
            ClusterState::Follower,
            ClusterState::Candidate,
            ClusterState::Offline,
            ClusterState::Standalone,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            let back: ClusterState = serde_json::from_str(&json).unwrap();
            assert_eq!(back, s, "状态 {s:?} 往返失败（json={json}）");
        }
    }

    #[test]
    fn cluster_state_serde_uses_snake_case() {
        assert_eq!(
            serde_json::to_string(&ClusterState::Leader).unwrap(),
            "\"leader\""
        );
        assert_eq!(
            serde_json::to_string(&ClusterState::Standalone).unwrap(),
            "\"standalone\""
        );
    }

    #[test]
    fn cluster_state_copy_clone_eq() {
        let a = ClusterState::Leader;
        let b = a;
        assert_eq!(a, b);
    }

    // ---- ClusterStatus ----

    #[test]
    fn cluster_status_serde_roundtrip() {
        let st = ClusterStatus {
            state: ClusterState::Leader,
            leader: Some(os_core::NodeId::new("n1")),
            term: 5,
            commit_index: 10,
            applied_index: 9,
            checked_at: Utc::now(),
        };
        let json = serde_json::to_string(&st).unwrap();
        let back: ClusterStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back.state, st.state);
        assert_eq!(back.term, st.term);
        assert_eq!(back.commit_index, st.commit_index);
        assert_eq!(back.applied_index, st.applied_index);
        assert_eq!(back.leader.as_ref().map(|n| n.as_str()), Some("n1"));
    }

    #[test]
    fn cluster_status_leader_none_roundtrip() {
        let st = ClusterStatus {
            state: ClusterState::Candidate,
            leader: None,
            term: 0,
            commit_index: 0,
            applied_index: 0,
            checked_at: Utc::now(),
        };
        let json = serde_json::to_string(&st).unwrap();
        let back: ClusterStatus = serde_json::from_str(&json).unwrap();
        assert!(back.leader.is_none());
        assert_eq!(back.state, ClusterState::Candidate);
    }
}
