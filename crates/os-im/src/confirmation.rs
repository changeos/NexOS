//! 确认与投票（规划文档 §3.7.2 协作原语4）
//!
//! 高危操作（ToolDescriptor.requires_confirmation = true）执行前需经确认门：
//! - 用户在 IM 内确认（user_confirm）
//! - 多 agent 会签（agent_vote，达法定人数）

use async_trait::async_trait;
use os_core::{DateTime, TaskId};
use serde::{Deserialize, Serialize};

use crate::agent::AgentId;

// ----------------------------------------------------------------------------
// 风险级别
// ----------------------------------------------------------------------------

/// 操作风险级别
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    /// 低（仅记录）
    Low,
    /// 中（提示用户）
    Medium,
    /// 高（需用户确认）
    High,
    /// 严重（需用户确认 + 多 agent 会签）
    Critical,
}

// ----------------------------------------------------------------------------
// 确认请求与投票
// ----------------------------------------------------------------------------

/// 确认请求（由 agent/工具发起）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfirmationRequest {
    /// 请求 ID（客户端生成）
    pub id: String,
    /// 关联任务
    pub task_id: TaskId,
    /// 待确认操作的描述（呈现给用户）
    pub description: String,
    /// 风险级别
    pub risk_level: RiskLevel,
    /// 发起者（agent ID）
    pub requested_by: AgentId,
    /// 创建时间（UTC）
    pub created_at: DateTime,
}

/// 单条投票记录（多 agent 会签）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoteRecord {
    /// 投票 agent
    pub agent: AgentId,
    /// 是否赞成
    pub approve: bool,
    /// 理由（可选）
    pub reason: Option<String>,
}

/// 确认状态
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum ConfirmationStatus {
    /// 等待中
    Pending,
    /// 用户已批准
    UserApproved,
    /// 用户已拒绝
    UserRejected,
    /// 会签达到法定（附是否通过）
    QuorumReached {
        /// 是否通过
        approved: bool,
    },
    /// 超时过期
    Expired,
}

// ----------------------------------------------------------------------------
// ConfirmationGate trait（async）
// ----------------------------------------------------------------------------

/// 确认门——高危操作的双轨确认（用户 + 多 agent 会签）。
///
/// 实现者：`DefaultConfirmationGate`（默认）。Critical 级需用户确认 + 会签双满足。
/// 预期以 `Box<dyn ConfirmationGate>` 注入，故用 `#[async_trait]` 保证 dyn 兼容（ADR-COMPAT-001）。
#[async_trait]
pub trait ConfirmationGate: Send + Sync {
    /// 发起确认请求，返回 request id。
    async fn request(&self, req: ConfirmationRequest) -> Result<String, crate::ImError>;

    /// 用户在 IM 内给出确认/拒绝。
    async fn user_confirm(&self, req_id: &str, approved: bool) -> Result<(), crate::ImError>;

    /// agent 投票会签。
    async fn agent_vote(&self, req_id: &str, vote: VoteRecord) -> Result<(), crate::ImError>;

    /// 查询确认状态。
    async fn status(&self, req_id: &str) -> ConfirmationStatus;
}
