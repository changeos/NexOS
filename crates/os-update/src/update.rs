//! A/B 双槽位 OTA 更新（规划文档 §3.12）
//!
//! 流程：检查更新 → 下载 → 校验（ed25519 签名 + sha256）→ 写入非活动槽 → 激活切换。
//! 双槽保证失败可回滚（见 rollback.rs）。

use os_core::TaskId;
use serde::{Deserialize, Serialize};
use std::path::Path;

// ----------------------------------------------------------------------------
// 更新清单与组件
// ----------------------------------------------------------------------------

/// 单个组件的更新条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentUpdate {
    /// 组件名（如 `osd` / `os-storage` / `samba`）
    pub name: String,
    /// 目标版本
    pub version: String,
    /// 是否需要重启该组件
    pub restart_required: bool,
}

/// 更新清单（来自更新源）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateManifest {
    /// 目标版本号
    pub version: String,
    /// 发行说明
    pub release_notes: String,
    /// 包大小（字节）
    pub size_bytes: u64,
    /// SHA256 校验和
    pub sha256: String,
    /// ed25519 签名（Base64）
    pub signature: String,
    /// 可从当前版本升级的最小版本（None = 不限制）
    pub min_current_version: Option<String>,
    /// 含组件级更新条目
    pub components: Vec<ComponentUpdate>,
}

// ----------------------------------------------------------------------------
// A/B 槽位与状态
// ----------------------------------------------------------------------------

/// A/B 双槽位（启动槽标识）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateSlot {
    /// 槽 A
    A,
    /// 槽 B
    B,
}

/// OTA 更新状态机
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum UpdateStatus {
    /// 下载中（附进度）
    Downloading {
        /// 进度 0.0 ~ 1.0
        progress: f32,
    },
    /// 校验中
    Verifying,
    /// 写入非活动槽
    Writing,
    /// 激活切换中
    Activating,
    /// 完成
    Completed,
    /// 失败（附原因）
    Failed {
        /// 失败原因
        reason: String,
    },
}

// ----------------------------------------------------------------------------
// UpdateEngine trait（async）
// ----------------------------------------------------------------------------

/// OTA 更新引擎——A/B 双槽位更新编排。
///
/// 实现者：`AbUpdateEngine`（默认）。安全：所有更新须签名校验通过方可激活。
#[allow(async_fn_in_trait)]
pub trait UpdateEngine: Send + Sync {
    /// 检查可用更新清单。
    async fn check_updates(&self) -> Result<Vec<UpdateManifest>, crate::UpdateError>;

    /// 下载指定清单的更新包（返回任务 ID）。
    async fn download(&self, manifest: &UpdateManifest) -> Result<TaskId, crate::UpdateError>;

    /// 校验已下载文件（签名 + sha256）。
    async fn verify(
        &self,
        manifest: &UpdateManifest,
        downloaded_path: &Path,
    ) -> Result<bool, crate::UpdateError>;

    /// 把更新写入非活动槽（返回被写入的槽）。
    async fn write_to_inactive_slot(
        &self,
        manifest: &UpdateManifest,
    ) -> Result<UpdateSlot, crate::UpdateError>;

    /// 激活指定槽（切换下次启动项）。
    async fn activate_slot(&self, slot: UpdateSlot) -> Result<(), crate::UpdateError>;

    /// 查询更新任务状态。
    async fn status(&self, task: &TaskId) -> UpdateStatus;
}
