//! StorageBackend trait —— 核心存储操作契约（池/数据集/快照/配额）
//!
//! 这是 os-storage 最核心的 trait，覆盖 ZFS 池与数据集的全部管理操作。
//! 实现由 owner agent 提供（默认：subprocess 调用 `zpool`/`zfs` 命令）。
//!
//! 命名约定：动词开头（create_/destroy_/list_/snapshot_/set_/get_）。
//! 所有方法异步，返回 `StorageResult<T>`。

use crate::model::{Dataset, Pool, Quota, Snapshot, VdevSpec};
use crate::options::DatasetOptions;
use os_core::{DatasetId, PoolId, SnapshotId};

/// 存储后端 trait（异步，数据路径）
///
/// 实现者：`ZfsCliBackend`（默认，调用 zpool/zfs CLI）；可替换为 libzfs_core 绑定实现。
/// 并发性：同一数据集上的并发写操作（如同时 create_dataset + destroy_dataset）
/// 由实现保证串行化（内部锁）。
pub trait StorageBackend: Send + Sync {
    // —— Pool 操作 ——

    /// 创建存储池
    ///
    /// 失败：池已存在 / vdev 非法（成员盘数不足/已被使用），见 [`crate::StorageError`]
    async fn create_pool(&self, id: &PoolId, vdevs: Vec<VdevSpec>) -> crate::StorageResult<Pool>;

    /// 销毁存储池（含其下所有数据集/快照，高危！实现应要求二次确认）
    async fn destroy_pool(&self, id: &PoolId) -> crate::StorageResult<()>;

    /// 列出所有池
    async fn list_pools(&self) -> crate::StorageResult<Vec<Pool>>;

    // —— Dataset 操作 ——

    /// 创建数据集（文件系统或 zvol，取决于 options.volsize）
    async fn create_dataset(
        &self,
        name: &DatasetId,
        options: DatasetOptions,
    ) -> crate::StorageResult<Dataset>;

    /// 销毁数据集（含其下所有快照，高危！）
    async fn destroy_dataset(&self, name: &DatasetId) -> crate::StorageResult<()>;

    /// 列出指定池下所有数据集（pool=None 表示全池扫描）
    async fn list_datasets(&self, pool: Option<&PoolId>) -> crate::StorageResult<Vec<Dataset>>;

    // —— Snapshot 操作 ——

    /// 对指定数据集创建快照
    async fn snapshot(&self, dataset: &DatasetId, name: &str) -> crate::StorageResult<Snapshot>;

    /// 销毁快照
    async fn destroy_snapshot(&self, snapshot: &SnapshotId) -> crate::StorageResult<()>;

    /// 列出指定数据集的快照（dataset=None 表示全池扫描）
    async fn list_snapshots(
        &self,
        dataset: Option<&DatasetId>,
    ) -> crate::StorageResult<Vec<Snapshot>>;

    // —— Quota 操作 ——

    /// 设置数据集配额
    async fn set_quota(&self, dataset: &DatasetId, quota: Quota) -> crate::StorageResult<()>;

    /// 读取数据集当前配额
    async fn get_quota(&self, dataset: &DatasetId) -> crate::StorageResult<Quota>;
}
