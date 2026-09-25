//! 阶段化迁移（规划文档 §3.10 阶段2 / §3.19 密钥排除清单）
//!
//! 迁移内容分两类：
//! - 配置/共享/用户定义 → 走迁移包（结构化导出/导入）
//! - 数据集内容 → 走 ZFS send/recv（增量）
//!
//! 安全：密码/密钥走 §3.19 统一排除清单，绝不随迁移包传输。

use os_core::{DatasetId, NodeId, TaskId};
use serde::{Deserialize, Serialize};

// ----------------------------------------------------------------------------
// 迁移计划
// ----------------------------------------------------------------------------

/// 迁移计划（源→目标节点的数据集 + 密钥排除）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationPlan {
    /// 源节点
    pub source_node: NodeId,
    /// 目标节点
    pub target_node: NodeId,
    /// 待迁移数据集列表
    pub datasets: Vec<DatasetId>,
    /// 密钥/密码排除清单（§3.19：JWT 密钥/TOTP secret/钱包密钥/数据库密码等），
    /// 命中的配置项不随迁移包传输，目标节点需重新生成或独立导入
    pub exclude_keys: Vec<String>,
    /// 断点续传锚点（None 表示全新迁移；中断后填入以续传）
    pub resume_point: Option<String>,
}

// ----------------------------------------------------------------------------
// 迁移状态
// ----------------------------------------------------------------------------

/// 迁移状态机
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum MigrationStatus {
    /// 排队中
    Pending,
    /// 传输中（附进度与当前数据集）
    Transferring {
        /// 进度 0.0 ~ 1.0
        progress: f32,
        /// 当前正在传输的数据集（None 表示在传配置包阶段）
        current_dataset: Option<DatasetId>,
    },
    /// 校验中（完整性/哈希比对）
    Verifying,
    /// 完成
    Completed,
    /// 失败（附原因）
    Failed {
        /// 失败原因
        reason: String,
    },
}

// ----------------------------------------------------------------------------
// MigrationEngine trait（async）
// ----------------------------------------------------------------------------

/// 迁移引擎——阶段化把源节点内容迁到目标节点。
///
/// 实现者：编排 os-storage 的 ZFS send/recv + 配置包导出/导入。
/// 安全：始终按 `exclude_keys` 排除敏感项（§3.19）。
#[allow(async_fn_in_trait)]
pub trait MigrationEngine: Send + Sync {
    /// 规划迁移：扫描源数据集 + 生成排除清单 +（可选）续传锚点。
    async fn plan(
        &self,
        source: &NodeId,
        target: &NodeId,
        datasets: &[DatasetId],
    ) -> Result<MigrationPlan, crate::ProvisionError>;

    /// 执行迁移：配置/共享/用户定义走迁移包，数据集走 ZFS send/recv；
    /// 密码密钥按排除清单不传输。
    async fn execute(&self, plan: MigrationPlan) -> Result<TaskId, crate::ProvisionError>;

    /// 断点续传——基于既有 `plan_id` 的锚点继续。
    async fn resume(&self, plan_id: &str) -> Result<TaskId, crate::ProvisionError>;

    /// 查询迁移任务状态。
    async fn status(&self, task: &TaskId) -> MigrationStatus;
}

// ----------------------------------------------------------------------------
// 单元测——MigrationPlan / MigrationStatus serde 往返
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ds(s: &str) -> DatasetId {
        DatasetId::new(s)
    }

    #[test]
    fn migration_plan_serde_roundtrip() {
        let plan = MigrationPlan {
            source_node: NodeId::new("node-a"),
            target_node: NodeId::new("node-b"),
            datasets: vec![ds("tank/media"), ds("tank/photos")],
            exclude_keys: vec!["/etc/shadow".into(), "/etc/os/jwt.key".into()],
            resume_point: Some("anchor-1".into()),
        };
        let json = serde_json::to_string(&plan).unwrap();
        let back: MigrationPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(back.source_node, plan.source_node);
        assert_eq!(back.target_node, plan.target_node);
        assert_eq!(back.datasets.len(), 2);
        assert_eq!(back.exclude_keys.len(), 2);
        assert_eq!(back.resume_point.as_deref(), Some("anchor-1"));
    }

    #[test]
    fn migration_plan_resume_point_none_roundtrip() {
        let plan = MigrationPlan {
            source_node: NodeId::new("a"),
            target_node: NodeId::new("b"),
            datasets: vec![],
            exclude_keys: vec![],
            resume_point: None,
        };
        let json = serde_json::to_string(&plan).unwrap();
        let back: MigrationPlan = serde_json::from_str(&json).unwrap();
        assert!(back.resume_point.is_none());
        assert!(back.datasets.is_empty());
    }

    #[test]
    fn migration_status_pending_roundtrip() {
        let s = MigrationStatus::Pending;
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"phase\":\"pending\""));
        let back: MigrationStatus = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, MigrationStatus::Pending));
    }

    #[test]
    fn migration_status_transferring_roundtrip() {
        let s = MigrationStatus::Transferring {
            progress: 0.75,
            current_dataset: Some(ds("tank/media")),
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"phase\":\"transferring\""));
        let back: MigrationStatus = serde_json::from_str(&json).unwrap();
        match back {
            MigrationStatus::Transferring {
                progress,
                current_dataset,
            } => {
                assert!((progress - 0.75).abs() < 1e-6);
                assert_eq!(
                    current_dataset.as_ref().map(|d| d.as_str()),
                    Some("tank/media")
                );
            }
            _ => panic!("应反序列化为 Transferring"),
        }
    }

    #[test]
    fn migration_status_transferring_no_dataset_roundtrip() {
        let s = MigrationStatus::Transferring {
            progress: 0.0,
            current_dataset: None,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: MigrationStatus = serde_json::from_str(&json).unwrap();
        match back {
            MigrationStatus::Transferring {
                current_dataset, ..
            } => {
                assert!(current_dataset.is_none());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn migration_status_verifying_roundtrip() {
        let s = MigrationStatus::Verifying;
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"phase\":\"verifying\""));
        let back: MigrationStatus = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, MigrationStatus::Verifying));
    }

    #[test]
    fn migration_status_completed_roundtrip() {
        let s = MigrationStatus::Completed;
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"phase\":\"completed\""));
        let back: MigrationStatus = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, MigrationStatus::Completed));
    }

    #[test]
    fn migration_status_failed_roundtrip() {
        let s = MigrationStatus::Failed {
            reason: "checksum mismatch".into(),
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"phase\":\"failed\""));
        let back: MigrationStatus = serde_json::from_str(&json).unwrap();
        match back {
            MigrationStatus::Failed { reason } => assert_eq!(reason, "checksum mismatch"),
            _ => panic!("应反序列化为 Failed"),
        }
    }
}
