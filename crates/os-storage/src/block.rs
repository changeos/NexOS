//! BlockExport trait —— 块存储 export 契约（iSCSI / NVMe-oF）
//!
//! 决策依据：规划文档 §9.1#11 —— 块存储 export 由 os-storage 统管，
//! 不依赖外部 targetd/SCST 单独服务。实现可基于内核 LIO/tcmu-runner 或 SCST。
//!
//! 操作对象是 zvol（在 DatasetOptions.volsize 创建的块设备），将其导出为
//! iSCSI LUN 或 NVMe-oF namespace，供外部 initiator 连接。

use os_core::{Deserialize, Serialize, VolumeId};

/// iSCSI target（一个 target 可含多个 LUN）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IscsiTarget {
    /// target IQN（如 `iqn.2024-01.example.os:target-001`）
    pub iqn: String,
    /// 关联卷 ID（zvol）
    pub volume: VolumeId,
    /// LUN 编号（0–255）
    pub lun_id: u32,
    /// 允许连接的 initiator IQN 列表（空表示不限）
    pub initiators: Vec<String>,
    /// 监听地址（`host:port`）
    pub listen: String,
}

/// NVMe-oF namespace
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NvmeofNamespace {
    /// subsystem NQN（如 `nqn.2024-01.example.os:subsys-001`）
    pub nqn: String,
    /// 关联卷 ID（zvol）
    pub volume: VolumeId,
    /// namespace ID（NSID，1–0xFFFFFFFE）
    pub nsid: u32,
    /// 允许连接的 host NQN 列表（空表示不限）
    pub hosts: Vec<String>,
    /// 传输地址（如 `192.168.1.10:4420`，TCP transport）
    pub transport_addr: String,
}

/// 块存储 export trait（异步）
///
/// 实现者：默认实现封装内核 LIO 配置（configfs）或 SCST/tcmu-runner CLI。
pub trait BlockExport: Send + Sync {
    /// 将卷导出为 iSCSI target（指定 LUN 与允许的 initiator）
    async fn export_iscsi(
        &self,
        volume: &VolumeId,
        lun_id: u32,
        initiators: Vec<String>,
    ) -> crate::StorageResult<IscsiTarget>;

    /// 将卷导出为 NVMe-oF namespace（指定 subsystem NQN）
    async fn export_nvmeof(
        &self,
        volume: &VolumeId,
        nqn: &str,
    ) -> crate::StorageResult<NvmeofNamespace>;

    /// 取消导出（按 target 标识：IQN 或 NQN）
    ///
    /// `target_id` 即 `IscsiTarget::iqn` 或 `NvmeofNamespace::nqn`。
    async fn unexport(&self, target_id: &str) -> crate::StorageResult<()>;

    /// 列出当前所有 export
    async fn list_exports(&self) -> crate::StorageResult<(Vec<IscsiTarget>, Vec<NvmeofNamespace>)>;
}
