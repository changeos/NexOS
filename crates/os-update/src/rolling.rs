//! HA 集群滚动升级（规划文档 §3.12）
//!
//! 逐节点升级：follower 先升 → 单节点验证通过 → 再升 leader，保证 HA 不中断。

use os_core::{NodeId, TaskId};
use serde::{Deserialize, Serialize};

use crate::update::UpdateManifest;

// ----------------------------------------------------------------------------
// 策略与计划
// ----------------------------------------------------------------------------

/// 滚动升级策略
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollingStrategy {
    /// Follower 先升（默认，leader 最后，最大化可用性）
    FollowersFirst,
    /// 一次一个节点（逐节点串行）
    OneAtATime,
    /// 全部同时（仅维护窗口用）
    AllAtOnce,
}

/// 滚动升级计划
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollingPlan {
    /// 节点升级顺序（含全部节点；按策略排定）
    pub order: Vec<NodeId>,
    /// 采纳的策略
    pub strategy: RollingStrategy,
    /// 是否在每节点升级后做一次健康验证
    pub per_node_verify: bool,
}

// ----------------------------------------------------------------------------
// 滚动状态
// ----------------------------------------------------------------------------

/// 滚动升级状态机
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum RollingStatus {
    /// 排队中
    Pending,
    /// 升级中（附当前节点与已完成列表）
    Upgrading {
        /// 当前正在升级的节点
        current_node: NodeId,
        /// 已完成的节点
        completed: Vec<NodeId>,
    },
    /// 全部完成
    Completed,
    /// 失败（附失败节点与原因）
    Failed {
        /// 失败的节点
        failed_node: NodeId,
        /// 失败原因
        reason: String,
    },
}

// ----------------------------------------------------------------------------
// RollingUpgrade trait（async）
// ----------------------------------------------------------------------------

/// HA 滚动升级编排——逐节点升级，保证集群可用性。
///
/// 实现者：`HaRollingUpgrade`（默认），配合 os-meta 的 leader 选举。
#[allow(async_fn_in_trait)]
pub trait RollingUpgrade: Send + Sync {
    /// 按策略为给定清单排定升级顺序。
    async fn plan(
        &self,
        manifest: &UpdateManifest,
        strategy: RollingStrategy,
    ) -> Result<RollingPlan, crate::UpdateError>;

    /// 执行滚动升级（follower 先，验证后 leader）。
    async fn execute(&self, plan: RollingPlan) -> Result<TaskId, crate::UpdateError>;

    /// 查询滚动升级任务状态。
    async fn status(&self, task: &TaskId) -> RollingStatus;
}

// ============================================================================
// 纯逻辑：节点顺序决策 + 状态机推进（无 meta/bootloader 依赖）
//
// 以下为纯函数/纯状态机，供 HaRollingUpgrade 实现复用，亦可独立测试。
// ============================================================================

use os_core::{NodeInfo, NodeRole};

// ----------------------------------------------------------------------------
// 节点顺序决策
// ----------------------------------------------------------------------------

/// 给定集群成员 + 角色，按策略排定升级顺序（纯函数）。
///
/// 策略语义：
/// - [`RollingStrategy::FollowersFirst`]（默认）：所有 follower 先（按 node_id 字典序
///   稳定排序），leader 最后。最大化可用性——即使 follower 升级失败，leader 仍在线。
///   Standalone 节点直接排末尾（无 leader 概念）。
/// - [`RollingStrategy::OneAtATime`]：等同 FollowersFirst 的顺序，但调用方在 execute
///   时逐节点串行（顺序本身不变，区别在执行节流，由 status 机表达）。
/// - [`RollingStrategy::AllAtOnce`]：顺序无意义（全部并行），返回原顺序。
///
/// 决策不变量：
/// - leader 永远排在所有 follower 之后（除非 AllAtOnce 透传）。
/// - 同角色内按 node_id 字典序稳定排序，保证可复现。
/// - 输入含重复 node_id 不去重（调用方负责），但顺序稳定。
///
/// 错误：
/// - 多于一个 leader（集群异常）→ `SlotConflict`（升级前须先恢复单 leader）。
/// - 成员为空 → `Internal`。
pub fn decide_upgrade_order(
    members: &[NodeInfo],
    strategy: RollingStrategy,
) -> Result<Vec<NodeId>, crate::UpdateError> {
    if members.is_empty() {
        return Err(crate::UpdateError::Internal(
            "集群成员为空，无法排定升级顺序".to_string(),
        ));
    }

    // AllAtOnce：透传原顺序（按 node_id 稳定排序便于可复现）
    if strategy == RollingStrategy::AllAtOnce {
        let mut all: Vec<&NodeInfo> = members.iter().collect();
        all.sort_by(|a, b| a.node_id.as_str().cmp(b.node_id.as_str()));
        return Ok(all.into_iter().map(|n| n.node_id.clone()).collect());
    }

    // FollowersFirst / OneAtATime：follower 先（稳定排序），leader 最后
    let leaders: Vec<&NodeInfo> = members
        .iter()
        .filter(|n| n.role == NodeRole::Leader)
        .collect();
    if leaders.len() > 1 {
        return Err(crate::UpdateError::SlotConflict(format!(
            "集群有 {} 个 leader（应至多 1），升级前须先恢复单 leader",
            leaders.len()
        )));
    }

    // follower / peer 先（按 node_id 字典序稳定排序）
    let mut followers: Vec<&NodeInfo> = members
        .iter()
        .filter(|n| matches!(n.role, NodeRole::Follower | NodeRole::Peer))
        .collect();
    followers.sort_by(|a, b| a.node_id.as_str().cmp(b.node_id.as_str()));

    // standalone 节点（单节点集群成员）排在 follower 之后、leader 之前
    let mut standalone: Vec<&NodeInfo> = members
        .iter()
        .filter(|n| n.role == NodeRole::Standalone)
        .collect();
    standalone.sort_by(|a, b| a.node_id.as_str().cmp(b.node_id.as_str()));

    let mut order: Vec<NodeId> = Vec::with_capacity(members.len());
    order.extend(followers.into_iter().map(|n| n.node_id.clone()));
    order.extend(standalone.into_iter().map(|n| n.node_id.clone()));
    // leader 最后
    order.extend(leaders.into_iter().map(|n| n.node_id.clone()));
    Ok(order)
}

// ----------------------------------------------------------------------------
// 滚动状态机推进（纯函数）
// ----------------------------------------------------------------------------

/// 滚动升级状态机推进器——给定当前状态与升级计划，推进到下一节点。
///
/// 这是纯状态转换逻辑，不含任何 I/O；HaRollingUpgrade::execute 据此驱动实际升级。
#[derive(Debug, Clone)]
pub struct RollingStateMachine {
    /// 升级计划（节点顺序）
    pub plan: RollingPlan,
    /// 已完成的节点
    pub completed: Vec<NodeId>,
    /// 当前状态
    pub state: RollingStatus,
}

impl RollingStateMachine {
    /// 用计划初始化状态机（进入 Pending）。
    #[must_use]
    pub fn new(plan: RollingPlan) -> Self {
        Self {
            plan,
            completed: Vec::new(),
            state: RollingStatus::Pending,
        }
    }

    /// 启动升级（Pending → Upgrading 第一个节点）。
    /// 错误：计划为空 / 当前非 Pending。
    pub fn start(&mut self) -> Result<&RollingStatus, crate::UpdateError> {
        if !matches!(self.state, RollingStatus::Pending) {
            return Err(crate::UpdateError::Internal(
                "当前状态非 Pending，无法启动".to_string(),
            ));
        }
        let first =
            self.plan.order.first().cloned().ok_or_else(|| {
                crate::UpdateError::Internal("升级计划为空，无节点可升".to_string())
            })?;
        self.state = RollingStatus::Upgrading {
            current_node: first,
            completed: Vec::new(),
        };
        Ok(&self.state)
    }

    /// 标记当前节点升级成功，推进到下一节点（或 Completed）。
    /// 错误：当前非 Upgrading。
    pub fn on_node_succeeded(&mut self) -> Result<&RollingStatus, crate::UpdateError> {
        let current = match &self.state {
            RollingStatus::Upgrading { current_node, .. } => current_node.clone(),
            _ => {
                return Err(crate::UpdateError::Internal(
                    "当前状态非 Upgrading，无法推进".to_string(),
                ));
            }
        };
        self.completed.push(current.clone());
        // 找下一个未完成节点
        let next = self
            .plan
            .order
            .iter()
            .find(|n| !self.completed.contains(*n))
            .cloned();
        match next {
            Some(n) => {
                self.state = RollingStatus::Upgrading {
                    current_node: n,
                    completed: self.completed.clone(),
                };
            }
            None => {
                self.state = RollingStatus::Completed;
            }
        }
        Ok(&self.state)
    }

    /// 标记当前节点升级失败 → Failed（终止滚动）。
    pub fn on_node_failed(
        &mut self,
        reason: impl Into<String>,
    ) -> Result<&RollingStatus, crate::UpdateError> {
        let current = match &self.state {
            RollingStatus::Upgrading { current_node, .. } => current_node.clone(),
            _ => {
                return Err(crate::UpdateError::Internal(
                    "当前状态非 Upgrading，无法标记失败".to_string(),
                ));
            }
        };
        self.state = RollingStatus::Failed {
            failed_node: current,
            reason: reason.into(),
        };
        Ok(&self.state)
    }

    /// 是否所有节点都已升级（Completed）。
    #[must_use]
    pub fn is_done(&self) -> bool {
        matches!(self.state, RollingStatus::Completed)
    }
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use os_core::{Health, NodeId, NodeInfo, NodeRole};

    fn node(id: &str, role: NodeRole) -> NodeInfo {
        NodeInfo {
            node_id: NodeId::new(id),
            role,
            version: "1.0.0".to_string(),
            arch: "x86_64".to_string(),
            endpoints: Vec::new(),
            health: Health::Healthy,
        }
    }

    // —— decide_upgrade_order ——

    #[test]
    fn order_followers_first_leader_last() {
        let members = vec![
            node("leader1", NodeRole::Leader),
            node("f2", NodeRole::Follower),
            node("f1", NodeRole::Follower),
            node("f3", NodeRole::Follower),
        ];
        let order = decide_upgrade_order(&members, RollingStrategy::FollowersFirst).unwrap();
        // follower 字典序在前，leader 最后
        assert_eq!(
            order,
            vec![
                NodeId::new("f1"),
                NodeId::new("f2"),
                NodeId::new("f3"),
                NodeId::new("leader1"),
            ]
        );
    }

    #[test]
    fn order_one_at_a_time_same_as_followers_first() {
        let members = vec![
            node("L", NodeRole::Leader),
            node("b", NodeRole::Follower),
            node("a", NodeRole::Follower),
        ];
        let o1 = decide_upgrade_order(&members, RollingStrategy::FollowersFirst).unwrap();
        let o2 = decide_upgrade_order(&members, RollingStrategy::OneAtATime).unwrap();
        assert_eq!(o1, o2);
    }

    #[test]
    fn order_all_at_once_preserves_sorted_input() {
        let members = vec![
            node("z", NodeRole::Follower),
            node("a", NodeRole::Leader),
            node("m", NodeRole::Follower),
        ];
        let order = decide_upgrade_order(&members, RollingStrategy::AllAtOnce).unwrap();
        // AllAtOnce：按 node_id 字典序，leader 不强制最后
        assert_eq!(
            order,
            vec![NodeId::new("a"), NodeId::new("m"), NodeId::new("z")]
        );
    }

    #[test]
    fn order_error_on_multiple_leaders() {
        let members = vec![
            node("L1", NodeRole::Leader),
            node("L2", NodeRole::Leader),
            node("f", NodeRole::Follower),
        ];
        let err = decide_upgrade_order(&members, RollingStrategy::FollowersFirst).unwrap_err();
        assert!(matches!(err, crate::UpdateError::SlotConflict(_)));
    }

    #[test]
    fn order_error_on_empty() {
        let err = decide_upgrade_order(&[], RollingStrategy::FollowersFirst).unwrap_err();
        assert!(matches!(err, crate::UpdateError::Internal(_)));
    }

    #[test]
    fn order_single_leader_only() {
        // 只有 leader 无 follower：leader 单独排
        let members = vec![node("L", NodeRole::Leader)];
        let order = decide_upgrade_order(&members, RollingStrategy::FollowersFirst).unwrap();
        assert_eq!(order, vec![NodeId::new("L")]);
    }

    #[test]
    fn order_peer_treated_as_follower() {
        let members = vec![
            node("L", NodeRole::Leader),
            node("p2", NodeRole::Peer),
            node("p1", NodeRole::Peer),
        ];
        let order = decide_upgrade_order(&members, RollingStrategy::FollowersFirst).unwrap();
        assert_eq!(
            order,
            vec![NodeId::new("p1"), NodeId::new("p2"), NodeId::new("L")]
        );
    }

    #[test]
    fn order_standalone_after_followers_before_leader() {
        let members = vec![
            node("L", NodeRole::Leader),
            node("f", NodeRole::Follower),
            node("s", NodeRole::Standalone),
        ];
        let order = decide_upgrade_order(&members, RollingStrategy::FollowersFirst).unwrap();
        assert_eq!(
            order,
            vec![NodeId::new("f"), NodeId::new("s"), NodeId::new("L")]
        );
    }

    // —— RollingStateMachine ——

    fn plan(order: &[&str]) -> RollingPlan {
        RollingPlan {
            order: order.iter().map(|s| NodeId::new(*s)).collect(),
            strategy: RollingStrategy::FollowersFirst,
            per_node_verify: true,
        }
    }

    #[test]
    fn sm_full_cycle_succeeds() {
        let mut sm = RollingStateMachine::new(plan(&["a", "b", "L"]));
        sm.start().unwrap();
        // 当前 a
        assert!(matches!(&sm.state,
            RollingStatus::Upgrading { current_node, completed }
            if current_node.as_str() == "a" && completed.is_empty()));
        sm.on_node_succeeded().unwrap(); // a done → b
        sm.on_node_succeeded().unwrap(); // b done → L
        sm.on_node_succeeded().unwrap(); // L done → Completed
        assert!(sm.is_done());
        assert!(matches!(sm.state, RollingStatus::Completed));
        assert_eq!(sm.completed.len(), 3);
    }

    #[test]
    fn sm_start_requires_pending() {
        let mut sm = RollingStateMachine::new(plan(&["a"]));
        sm.start().unwrap();
        // 再次 start 应失败
        assert!(sm.start().is_err());
    }

    #[test]
    fn sm_empty_plan_start_fails() {
        let mut sm = RollingStateMachine::new(plan(&[]));
        assert!(sm.start().is_err());
    }

    #[test]
    fn sm_succeed_before_start_fails() {
        let mut sm = RollingStateMachine::new(plan(&["a"]));
        assert!(sm.on_node_succeeded().is_err());
    }

    #[test]
    fn sm_fail_terminates() {
        let mut sm = RollingStateMachine::new(plan(&["a", "b"]));
        sm.start().unwrap();
        sm.on_node_failed("boom").unwrap();
        match &sm.state {
            RollingStatus::Failed {
                failed_node,
                reason,
            } => {
                assert_eq!(failed_node.as_str(), "a");
                assert_eq!(reason, "boom");
            }
            _ => panic!("应是 Failed"),
        }
        // 失败后推进应失败
        assert!(sm.on_node_succeeded().is_err());
    }

    #[test]
    fn sm_progresses_through_each_node() {
        let mut sm = RollingStateMachine::new(plan(&["a", "b", "c"]));
        sm.start().unwrap();
        // a
        let s = sm.on_node_succeeded().unwrap();
        match s {
            RollingStatus::Upgrading {
                current_node,
                completed,
            } => {
                assert_eq!(current_node.as_str(), "b");
                assert_eq!(completed.len(), 1);
                assert_eq!(completed[0].as_str(), "a");
            }
            _ => panic!("应是 Upgrading b"),
        }
        // b
        sm.on_node_succeeded().unwrap();
        // c → Completed
        sm.on_node_succeeded().unwrap();
        assert!(sm.is_done());
    }
}
