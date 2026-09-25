//! os-iso —— OS 可安装 ISO 打包 + Rust 安装器（接口契约 + 骨架实现）
//!
//! 定位（规划文档 §3.11 / §3.19 / §10.2#17）：
//! - 标准 / 克隆两种 ISO 变体（构建期含组件二进制）
//! - Rust 安装器：硬件兼容性检测（HCL）+ 分区/建池/装系统
//! - 首启强制重设密码（§3.19）
//!
//! 模块组织：
//! - [`iso`] / [`installer`] / [`error`]：契约层（trait + 数据结构 + IsoError）
//! - [`XorrisoIsoBuilder`]：编排 xorriso + squashfs 产出 ISO（真执行留 TODO）
//! - [`RustInstaller`]：HCL 检测 + 裸机安装（真写盘留 TODO）
//! - `MockIsoBuilder` / `MockInstaller`：仅 `mock` feature，供下游测试（见 `mock` 模块）
//! - `cli`：xorriso / mksquashfs / sha256sum 命令构造（内部纯函数模块）
//! - `env`：ISO 构建工具链环境探测（xorriso/mksquashfs 存在性，供测决定是否跳过真实测）
//!
//! 契约规范：数据路径 trait 用原生 `async fn in trait`（无 `#[async_trait]`），
//! lib 顶部统一 `#![allow(async_fn_in_trait)]`；自定义 `IsoError`，
//! 并实现 `From<IsoError> for os_common::ApiError` 以统一对外错误。
//!
//! dyn 兼容性（ADR-COMPAT-001）：本 crate trait 保持原生 async（单实现为主），
//! 不能 `Box<dyn IsoBuilder>`。下游以具体类型/泛型注入。

#![allow(async_fn_in_trait)]

pub mod cli;
pub mod error;
pub mod install_cmds;
pub mod installer;
pub mod iso;

// —— 实现模块（owner agent iso-agent 填充）——
pub mod env;
mod impl_installer;
mod impl_iso;
pub mod runner;

pub use error::{IsoError, IsoResult};
pub use installer::{
    detect_kvm_support_from_cpuinfo, hcl_warnings, DiskInfo, HardwareReport, HclThresholds,
    InstallReport, InstallStep, InstallTarget, Installer,
};
pub use iso::{
    filter_sensitive, is_sensitive_key, IsoBuildResult, IsoBuildStatus, IsoBuilder, IsoSpec,
    IsoVariant, SENSITIVE_CONFIG_KEYS,
};

// —— 实现导出 ——
pub use impl_installer::RustInstaller;
pub use impl_iso::XorrisoIsoBuilder;

// —— Mock（仅 `mock` feature；供下游 update-agent / api-agent 测试注入）——
#[cfg(feature = "mock")]
pub mod mock;
#[cfg(feature = "mock")]
pub use mock::{MockInstaller, MockIsoBuilder};
