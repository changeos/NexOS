//! os-update —— OS 系统更新（接口契约 + 纯逻辑骨架）
//!
//! 定位（规划文档 §3.12）：
//! - A/B 双槽位 OTA + ed25519 签名校验
//! - watchdog 自动回滚（启动探活失败回退旧槽）
//! - CVE 监听（Samba/QEMU/rdma-core 等 C 依赖）
//! - HA 集群滚动升级（follower 先，leader 最后）
//!
//! 模块组织：
//! - [`update`] / [`rollback`] / [`cve`] / [`rolling`] / [`error`]：契约层
//!   （trait + 数据结构 + UpdateError）。
//! - [`slot`]：A/B 槽位状态机 + 槽位切换决策（纯逻辑，无 bootloader 依赖）。
//! - [`version`]：更新包模型 + semver 比较 + 升级路径决策（纯逻辑）。
//! - [`rolling`] 内含节点顺序决策 + 滚动状态机推进器（纯逻辑）。
//! - [`rollback`] 内含回滚策略 + 触发条件判定（纯逻辑）。
//! - `impls`：四个 trait 的默认实现（AbUpdateEngine / AbRollbackManager /
//!   NvdCveMonitor / HaRollingUpgrade）；`activate_slot` 已接通真实 bootloader
//!   编排（见 [`bootloader`]）；其余真实 I/O 待 ostree/NVD 依赖注册后填充。
//! - `bootloader`：GRUB / systemd-boot A/B 槽位激活——配置生成 + bootloader 工具
//!   执行抽象（BootloaderRunner trait）；测试用 fixture，真实执行需 root（`#[ignore]`）。
//! - `mock`：仅 `mock` feature，供下游 api-agent 测试注入。
//!
//! 契约规范：数据路径 trait 用原生 `async fn in trait`（无 `#[async_trait]`），
//! lib 顶部统一 `#![allow(async_fn_in_trait)]`；`CveCallback` 经 `Box<dyn>` 多态故
//! 用 `#[async_trait]`（ADR-COMPAT-001）；自定义 `UpdateError`，
//! 并实现 `From<UpdateError> for os_common::ApiError` 以统一对外错误。

#![allow(async_fn_in_trait)]

pub mod bootloader;
pub mod cve;
pub mod error;
pub mod real;
pub mod rollback;
pub mod rolling;
pub mod slot;
pub mod update;
pub mod version;

// —— 默认实现骨架（owner agent update-agent 填充）——
mod impls;

pub use bootloader::{
    ActivationPlan, BootloaderCommandOutput, BootloaderConfig, BootloaderKind, BootloaderRunner,
    SlotBootEntry, TokioBootloaderRunner,
};
pub use cve::{CveAdvisory, CveCallback, CveMonitor, CveSeverity};
pub use error::{UpdateError, UpdateResult};
pub use impls::{AbRollbackManager, AbUpdateEngine, HaRollingUpgrade, NvdCveMonitor};
pub use rollback::{
    should_rollback, RollbackContext, RollbackDecision, RollbackManager, RollbackPoint,
    RollbackPolicy,
};
pub use rolling::{
    decide_upgrade_order, RollingPlan, RollingStateMachine, RollingStatus, RollingStrategy,
    RollingUpgrade,
};
pub use slot::{SlotManager, SlotState, SlotStatus, SlotSwitchDecision};
pub use update::{ComponentUpdate, UpdateEngine, UpdateManifest, UpdateSlot, UpdateStatus};
pub use version::{compare_versions, upgrade_decision, UpdatePackage, UpgradeDecision, Version};

// —— Mock（仅 `mock` feature；供下游 api-agent 测试注入）——
#[cfg(feature = "mock")]
pub mod mock;
#[cfg(feature = "mock")]
pub use mock::{MockCveMonitor, MockRollbackManager, MockRollingUpgrade, MockUpdateEngine};
