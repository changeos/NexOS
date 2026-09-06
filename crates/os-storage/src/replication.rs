//! Replication trait —— ZFS send-recv 异步复制契约
//!
//! 用于跨节点/跨集群灾备。`send` 将快照流式传输到 target，`recv` 接收。
//! 由于复制耗时较长（可能数小时），返回 `TaskId` 供异步轮询进度（呼应 §3.7 agent 任务模型）。

use async_trait::async_trait;
use os_core::{DatasetId, Deserialize, Serialize, SnapshotId, TaskId};

/// 复制任务状态
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReplicationStatus {
    /// 排队中（上游忙，等待发送槽位）
    Pending,
    /// 进行中（`progress` 0.0–1.0；`speed_bps` 当前速率字节/秒）
    Running { progress: f32, speed_bps: u64 },
    /// 已完成
    Completed {
        /// 已传输字节数
        transferred_bytes: u64,
        /// 耗时（秒）
        elapsed_secs: u64,
    },
    /// 失败（含错误原因）
    Failed(String),
}

/// 复制配置（声明式，可持久化用于周期任务）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplicationConfig {
    /// 源数据集（必须已存在快照流）
    pub source: DatasetId,
    /// 目标数据集（远端 `<host>:<dataset>` 或本地）
    pub target: DatasetId,
    /// 调度（cron 表达式或 `"manual"`；实现自定义）
    #[serde(default)]
    pub schedule: Option<String>,
    /// 是否加密传输（中间链路；与数据集自身 native encryption 独立）
    #[serde(default)]
    pub encrypted: bool,
}

/// 复制 trait（异步）
///
/// 实现者：默认实现封装 `zfs send | ssh ... zfs recv` 管道，进度解析自 stderr。
#[async_trait]
pub trait Replication: Send + Sync {
    /// 将快照发送到 target 数据集（异步任务，立即返回 TaskId）
    ///
    /// `target` 可能是远端（实现自行处理 ssh/网络传输）。
    async fn send(&self, snapshot: &SnapshotId, target: &DatasetId)
        -> crate::StorageResult<TaskId>;

    /// 接收快照流（在 target 端执行；通常与 send 配对，远端 os-storage 调用）
    async fn recv(&self, source: &SnapshotId, target: &DatasetId) -> crate::StorageResult<TaskId>;

    /// 查询复制任务进度
    async fn replication_status(&self, task: &TaskId) -> crate::StorageResult<ReplicationStatus>;
}
