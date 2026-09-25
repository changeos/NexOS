//! 备份 / 灾备 / 快照策略（规划文档 §3.16 backup 组件 + §10.2#11 ZFS scrub 调度）
//!
//! 职责：
//! - 按 `BackupPolicy`（cron 调度 + 保留策略）周期性创建 ZFS 快照 / 远程灾备副本
//! - 触发即时备份 / 从快照恢复
//! - 周期性 ZFS scrub（数据校验）并上报 `ScrubReport`

use os_core::{DatasetId, DateTime, PoolId, SnapshotId, TaskId};
use os_core::{Deserialize, Serialize};

use crate::ServiceError;

// ----------------------------------------------------------------------------
// Cron 表达式 / 保留策略 / 备份策略
// ----------------------------------------------------------------------------

/// Cron 表达式（newtype String）
///
/// 格式遵循标准 5 段 cron：`分 时 日 月 周`，例如 `"0 3 * * *"` 表示每天 03:00。
/// 实现侧负责解析与调度（可用 `cron` crate）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CronExpr(pub String);

impl CronExpr {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Display for CronExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 快照 / 备份保留策略
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionPolicy {
    /// 保留最近 N 份（按时间倒序）
    pub keep_last: u32,
    /// 保留最近 N 天内的份
    pub keep_days: u32,
}

/// 备份策略（定义一个备份任务的「做什么 / 何时做 / 留多久」）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupPolicy {
    /// 策略名（人类可读，如 `"media-daily"`）
    pub name: String,
    /// 调度（cron 表达式）
    pub schedule: CronExpr,
    /// 保留策略
    pub retention: RetentionPolicy,
    /// 源数据集（被备份的 ZFS dataset）
    pub source: DatasetId,
    /// 远程灾备目标（None = 仅本地快照；Some = 远端地址，触发 send-recv 复制）
    pub target_remote: Option<String>,
}

/// 备份任务运行状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupStatus {
    /// 已调度、等待触发
    Scheduled,
    /// 运行中
    Running,
    /// 上次成功
    Success,
    /// 上次失败
    Failed,
}

/// 备份任务（已调度的策略实例）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupJob {
    /// 任务 ID（schedule 返回）
    pub id: String,
    /// 关联策略
    pub policy: BackupPolicy,
    /// 上次执行时间
    pub last_run: Option<DateTime>,
    /// 下次预计执行时间
    pub next_run: Option<DateTime>,
    /// 当前状态
    pub status: BackupStatus,
}

// ----------------------------------------------------------------------------
// Scrub 报告（§10.2#11 ZFS scrub 调度）
// ----------------------------------------------------------------------------

/// ZFS scrub 报告（数据校验结果）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrubReport {
    /// 检测到的错误数
    pub errors: u64,
    /// 已修复的错误数
    pub repaired: u64,
    /// 上次完成时间
    pub last_finished: Option<DateTime>,
    /// 上次 scrub 耗时（秒）
    pub duration_secs: u64,
}

// ----------------------------------------------------------------------------
// BackupManager trait（async）
// ----------------------------------------------------------------------------

/// 备份管理器——调度/触发/恢复备份，并周期性校验存储池完整性。
///
/// 实现者：`ZfsBackupManager`（默认，基于 ZFS 快照 + send-recv）。
#[allow(async_fn_in_trait)]
pub trait BackupManager: Send + Sync {
    /// 调度一个备份策略，返回 job id。
    async fn schedule(&self, policy: BackupPolicy) -> Result<String, ServiceError>;

    /// 取消已调度的备份任务。
    async fn unschedule(&self, job_id: &str) -> Result<(), ServiceError>;

    /// 列出所有已调度的备份任务。
    async fn list_jobs(&self) -> Result<Vec<BackupJob>, ServiceError>;

    /// 立即触发一次备份（不等待 cron），返回追踪用的任务 ID。
    async fn trigger_now(&self, job_id: &str) -> Result<TaskId, ServiceError>;

    /// 查询存储池 scrub 状态（§10.2#11 ZFS scrub 调度）。
    async fn scrub_status(&self, pool: &PoolId) -> Result<ScrubReport, ServiceError>;

    /// 从快照恢复到目标数据集，返回追踪用的任务 ID。
    async fn restore(
        &self,
        snapshot: &SnapshotId,
        target: &DatasetId,
    ) -> Result<TaskId, ServiceError>;
}
