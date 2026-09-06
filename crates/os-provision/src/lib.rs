//! os-provision —— OS 系统分发/迁移（接口契约 + 纯逻辑实现）
//!
//! 定位（规划文档 §3.10 / §3.19）：
//! - PXE 自举裸机（手机换机式的"新机初始化"）
//! - 阶段化迁移：配置/共享/用户定义走迁移包，数据走 ZFS send/recv；
//!   密钥/密码按 §3.19 统一排除清单不传输
//!
//! 模块组织：
//! - [`error`] / [`provision`] / [`migration`]：trait 契约（不可改签名，走 ADR）。
//! - [`exclude`]：§3.19 敏感项排除清单的纯路径匹配算法（高价值，可单测）。
//! - [`phase`]：迁移阶段状态机（SystemInit/FileTransfer/ExcludeSensitive/FirstBoot）。
//! - [`checkpoint`]：断点续传模型 + 恢复决策算法。
//! - [`package`]：迁移包结构 + 打包/解包骨架（JSON，签名/压缩由下游补）。
//! - [`pxe`]：PXE 引导配置生成（iPXE 脚本 / pxelinux.cfg/default / DHCP next-server + bootfile / TFTP 布局）。纯逻辑，不真跑 PXE/TFTP。
//! - [`init_script`]：阶段1 系统初始化脚本骨架（分区/建池/装基础系统），shell 模板参数化。
//! - [`transfer`]：阶段2 传输编排骨架（ZFS send/recv 命令骨架 + 配置包导出/导入），不真跑 zfs send。
//! - `mock`：`MockProvisioner` / `MockMigrationEngine`（feature `mock`）。
//!
//! 契约规范：数据路径 trait 用原生 `async fn in trait`（无 `#[async_trait]`），
//! lib 顶部统一 `#![allow(async_fn_in_trait)]`；自定义 `ProvisionError`，
//! 并实现 `From<ProvisionError> for os-common::ApiError` 以统一对外错误。

#![allow(async_fn_in_trait)]

pub mod checkpoint;
pub mod error;
pub mod exclude;
pub mod init_script;
pub mod migration;
pub mod package;
pub mod phase;
pub mod provision;
pub mod pxe;
pub mod transfer;

#[cfg(feature = "mock")]
pub mod mock;
#[cfg(feature = "mock")]
pub use mock::{MockMigrationEngine, MockProvisioner};

pub use error::{ProvisionError, ProvisionResult};
pub use migration::{MigrationEngine, MigrationPlan, MigrationStatus};
pub use provision::{ProvisionConfig, ProvisionStatus, ProvisionTarget, Provisioner};
