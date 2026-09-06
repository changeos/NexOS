//! 中枢 Agent——多 agent 协作编排（规划文档 §3.7.2）
//!
//! 职责：领域 agent 注册 / 能力发现 / 任务委派（含循环检测）/ 状态聚合。
//! 任务依赖图必须无环——delegate 时做拓扑检查，命中环返回 TaskCycle。

use os_core::TaskId;
use serde::{Deserialize, Serialize};

use crate::agent::{Agent, AgentCapability, AgentId, AgentTask};

// ----------------------------------------------------------------------------
// 编排状态
// ----------------------------------------------------------------------------

/// 中枢对委派任务的状态视图
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum OrchestrationStatus {
    /// 排队中
    Pending,
    /// 已委派给某 agent
    Delegated {
        /// 被委派的 agent
        to: AgentId,
    },
    /// 等待确认（高危操作经确认门）
    AwaitingConfirmation {
        /// 等待原因（如 `高危删除需用户确认`）
        reason: String,
    },
    /// 完成（附结果）
    Completed {
        /// agent 返回的结果（JSON，开放结构）
        result: serde_json::Value,
    },
    /// 失败（附原因）
    Failed {
        /// 失败原因
        reason: String,
    },
}

// ----------------------------------------------------------------------------
// AgentOrchestrator trait（async，中枢）
// ----------------------------------------------------------------------------

/// 中枢 agent——多 agent 协作运行时核心。
///
/// 实现者：`CentralOrchestrator`（默认）。约束：
/// - 任务依赖图必须无环（delegate 时拓扑检查，命中环返回 `TaskCycle`）
/// - 能力发现：按 tools/domain 路由任务
#[allow(async_fn_in_trait)]
pub trait AgentOrchestrator: Send + Sync {
    /// 注册领域 agent。
    async fn register_agent(&self, agent: Box<dyn Agent>) -> Result<(), crate::ImError>;

    /// 注销领域 agent。
    async fn unregister_agent(&self, id: &AgentId);

    /// 能力发现——列出全部已注册 agent 的能力。
    async fn list_agents(&self) -> Vec<AgentCapability>;

    /// 委派任务（含循环检测：任务图必须无环，否则返回 `TaskCycle`）。
    async fn delegate(&self, task: AgentTask) -> Result<TaskId, crate::ImError>;

    /// 查询委派任务状态。
    async fn task_status(&self, task: &TaskId) -> OrchestrationStatus;
}
