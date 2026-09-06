//! 上下文共享黑板（规划文档 §3.7.2 协作原语3）
//!
//! agent 间通过 key-value 黑板共享中间结果与上下文，避免点对点耦合。
//! 任务完成后可按 task 清理对应条目。

use async_trait::async_trait;
use os_core::{DateTime, TaskId};
use serde::{Deserialize, Serialize};

use crate::agent::AgentId;

// ----------------------------------------------------------------------------
// 黑板条目
// ----------------------------------------------------------------------------

/// 黑板单条条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlackboardEntry {
    /// 键（建议命名空间化，如 `task.<id>.snapshot.id`）
    pub key: String,
    /// 值（JSON，开放结构）
    pub value: serde_json::Value,
    /// 写入者（agent ID）
    pub written_by: AgentId,
    /// 写入时间（UTC）
    pub timestamp: DateTime,
}

// ----------------------------------------------------------------------------
// SharedContext trait（async，黑板）
// ----------------------------------------------------------------------------

/// 共享上下文黑板——agent 间协作原语。
///
/// 实现者：`InMemoryBlackboard`（默认）/ `DistributedBlackboard`（基于 os-meta KV）。
/// 预期以 `Box<dyn SharedContext>` 注入，故用 `#[async_trait]` 保证 dyn 兼容（ADR-COMPAT-001）。
#[async_trait]
pub trait SharedContext: Send + Sync {
    /// 写入（覆盖同 key 旧值）。
    async fn put(
        &self,
        key: &str,
        value: serde_json::Value,
        writer: &AgentId,
    ) -> Result<(), crate::ImError>;

    /// 读取单个 key。
    async fn get(&self, key: &str) -> Option<BlackboardEntry>;

    /// 列出全部条目。
    async fn list(&self) -> Vec<BlackboardEntry>;

    /// 清理某任务相关的黑板条目（按 key 前缀约定，如 `task.<id>.*`）。
    async fn clear_for_task(&self, task: &TaskId);
}
