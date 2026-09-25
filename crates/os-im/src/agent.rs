//! 多 agent 协作——领域 agent 契约（规划文档 §3.7.2 核心）
//!
//! 中枢（orchestrator）按能力发现把任务委派给对应领域 agent（storage/vm/...）。
//! agent 间通过 SharedContext 黑板共享上下文，副作用以 Event 上报。

use async_trait::async_trait;
use os_core::{DateTime, Event, HealthReport, TaskId};
use serde::{Deserialize, Serialize};

// ----------------------------------------------------------------------------
// Agent 标识与能力
// ----------------------------------------------------------------------------

/// agent ID（newtype String，如 `storage-agent` / `vm-agent` / `wallet-agent`）
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(pub String);

impl AgentId {
    /// 从字符串构造
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    /// 取字符串切片
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl From<String> for AgentId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// agent 能力声明（注册到中枢用于能力发现）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCapability {
    /// agent ID
    pub agent_id: AgentId,
    /// 该 agent 可处理的 Tool name 列表
    pub tools: Vec<String>,
    /// 所属领域（如 `storage` / `vm` / `wallet` / `network`）
    pub domain: String,
}

// ----------------------------------------------------------------------------
// 委派任务与结果
// ----------------------------------------------------------------------------

/// 中枢委派给 agent 的任务
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTask {
    /// 任务 ID
    pub id: TaskId,
    /// 被指派的 agent
    pub assigned_to: AgentId,
    /// 任务描述（自然语言或结构化指令）
    pub description: String,
    /// 共享黑板数据（来自 SharedContext 的上下文片段）
    pub context: serde_json::Value,
    /// 截止时间（None = 不限）
    pub deadline: Option<DateTime>,
}

/// agent 任务结果（含副作用事件）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTaskResult {
    /// 关联任务 ID
    pub task_id: TaskId,
    /// 是否成功
    pub success: bool,
    /// 输出（JSON，开放结构）
    pub output: serde_json::Value,
    /// 执行过程中产生的副作用事件（由中枢经 EventBus 广播）
    pub side_effects: Vec<Event>,
}

// ----------------------------------------------------------------------------
// Agent trait（async，领域 agent 契约）
// ----------------------------------------------------------------------------

/// 领域 agent——接收中枢委派并执行领域任务。
///
/// 实现者：`StorageAgent` / `VmAgent` / `WalletAgent` 等。
///
/// 该 trait 经 `AgentOrchestrator::register_agent(agent: Box<dyn Agent>)` 以
/// trait object 注入中枢，故用 `#[async_trait]` 保证 dyn 兼容
/// （呼应横切决策：凡 `Box<dyn>` 用的 async trait 一律 `#[async_trait]`）。
#[async_trait]
pub trait Agent: Send + Sync {
    /// agent ID。
    async fn id(&self) -> AgentId;

    /// 能力声明（用于中枢能力发现）。
    async fn capabilities(&self) -> AgentCapability;

    /// 处理中枢委派的任务。
    async fn handle_task(&self, task: &AgentTask) -> Result<AgentTaskResult, crate::ImError>;

    /// 自身健康（中枢用于调度决策）。
    async fn health(&self) -> HealthReport;
}
