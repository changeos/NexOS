//! os-storage —— ZFS 存储管理（池 / dataset / snapshot / 配额 / 加密 / send-recv 复制 / 块 export）。
//!
//! 定位（规划文档 §3.2 / §9.1#11）：
//! - ZFS 池/数据集/快照/配额管理（Rust 编排 `zfs`/`zpool` 命令）
//! - 数据集加密（native encryption：load/unload/change key）
//! - send-recv 异步复制（带进度上报，跨节点/跨集群灾备）
//! - 块存储 export：iSCSI target / NVMe-oF namespace（zvol → LUN/NSID）
//!
//! 实现要点：
//! - `ZfsCliBackend`（`backend_impl`）：通过 `tokio::process::Command` 调用
//!   `zpool`/`zfs` CLI，统一用 `-p -H` 机器可读格式；命令构造（[`cli`]）与输出解析
//!   （[`model`]）均为纯函数，可在无 ZFS 环境单测。
//! - ID 全部复用 os-core 的 newtype（PoolId/DatasetId/SnapshotId/VolumeId/TaskId）
//! - 命令失败统一封装为 `StorageError::CommandFailed(String)`，保留 stderr 供诊断
//!
//! 权限：`zpool`/`zfs` 写操作（create/destroy/snapshot/set/load-key）与块 export
//! （LIO/nvmet/configfs、cryptsetup）需 **root**；读操作（list/get）普通用户可执行。
//!
//! # 模块
//!
//! - [`backend`]：池/dataset/snapshot/quota 契约——[`StorageBackend`] trait。
//! - [`block`]：块存储 export 契约——[`BlockExport`] trait（iSCSI target / NVMe-oF namespace）。
//! - [`crypto`]：数据集加密契约——[`CryptoManager`] trait（load/unload/change key）。
//! - [`replication`]：send-recv 异步复制契约——[`Replication`] trait（带进度上报）。
//! - [`cli`]：`zpool`/`zfs` 命令构造纯函数（`-p -H` 机器可读格式，无 ZFS 也可单测）。
//! - [`model`]：领域模型（`Pool`/`Dataset`/`Snapshot`/`Quota`/`Vdev`/`VdevSpec`/`EncryptionConfig`）。
//! - [`options`]：`DatasetOptions`（属性/配额/预留等 dataset 属性集）。
//! - [`error`]：`StorageError` / `StorageResult`（命令失败带 stderr）。
//! - `mock`：测试桩（仅 `mock` feature，供 protocol/compute/meta/service/provision 测试注入）。
//!
//! # 关键 trait
//!
//! - [`StorageBackend`]：ZFS 池/dataset/snapshot/quota 管理数据路径（async fn in trait）。
//! - [`BlockExport`]：zvol → iSCSI LUN / NVMe-oF namespace 导出。
//! - [`CryptoManager`]：native encryption key load/unload/change。
//! - [`Replication`]：`zfs send | zfs recv` 跨节点/跨集群灾备（带 `ReplicationStatus`）。
//! - [`CommandRunner`]：命令执行抽象（`TokioCommandRunner` 为默认实现，便于注入 fake runner 单测）。
//!
//! # feature 门控
//!
//! - `mock`（默认关）：开启 `mock` 模块，导出 `MockStorageBackend` 供下游测试注入。
//!
//! # 默认实现
//!
//! - [`ZfsCliBackend`]：实现 [`StorageBackend`]，编排 `zpool`/`zfs` CLI。
//! - [`LioBlockExport`]：实现 [`BlockExport`]，操作 LIO/configfs（iSCSI）+ nvmet（NVMe-oF）。
//! - [`ZfsNativeCrypto`]：实现 [`CryptoManager`]，编排 `zfs load-key`/`change-key`。
//! - [`ZfsSendRecv`]：实现 [`Replication`]，`zfs send | recv` 子进程管线。

// 原生 async fn in trait，不做 dyn 派发（ZfsCliBackend 等是单实现，不需 Box<dyn>）。
// 注：本 crate 的 4 个 trait 均保持原生 async fn；其单实现 struct 用原生 async impl。
#![allow(async_fn_in_trait)]

pub mod backend;
pub mod block;
pub mod cli;
pub mod crypto;
pub mod error;
pub mod model;
pub mod options;
pub mod replication;

// —— 实现模块（owner agent storage-agent 填充）——
mod backend_impl;
mod block_impl;
mod crypto_impl;
mod replication_impl;

pub use backend::StorageBackend;
pub use block::{BlockExport, IscsiTarget, NvmeofNamespace};
pub use crypto::CryptoManager;
pub use error::{StorageError, StorageResult};
pub use model::{Dataset, EncryptionConfig, Pool, Quota, Snapshot, Vdev, VdevSpec};
pub use options::DatasetOptions;
pub use replication::{Replication, ReplicationConfig, ReplicationStatus};

// —— 实现导出 ——
// CommandOutput 现统一来自 os-core（review2 P-R2-1）；此处 re-export 保持下游 import 路径稳定。
pub use backend_impl::{
    parse_zpool_status, CommandRunner, PoolStatus, TokioCommandRunner, ZfsCliBackend,
};
pub use block_impl::LioBlockExport;
pub use crypto_impl::ZfsNativeCrypto;
pub use os_core::CommandOutput;
pub use replication_impl::ZfsSendRecv;

// —— Mock（仅 `mock` feature；供下游 protocol/compute/meta/service/provision 测试注入）——
#[cfg(feature = "mock")]
pub mod mock;
#[cfg(feature = "mock")]
pub use mock::MockStorageBackend;
