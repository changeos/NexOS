//! os-compute —— 计算层（KVM/libvirt 虚拟机 + youki OCI 容器 + 自研 CNI 容器网络 + apt/dpkg 包管理）。
//!
//! 定位（规划文档 §3.4）：
//! - KVM 虚拟机：编排 libvirt（domain 生命周期/迁移，zvol 作为磁盘后端）
//! - 容器：youki（OCI runtime），补齐 youki 短板的是自研 CNI 容器网络
//! - 第三方包：os-pkg（apt/dpkg 编排，第三方带图标应用归"未知来源"）
//!
//! 本 crate 仅定义契约（trait + 数据结构 + Error），实现由 owner agent 后续填充。
//!
//! 设计要点：
//! - VM 磁盘后端用 zvol（`VolumeId`，复用 os-storage/os-core 的 newtype）
//! - 容器端口映射复用 `os_network::Protocol`（TCP/UDP/Any）
//! - 容器网络复用 `os_network::IpCidr`
//! - 所有数据路径 trait 用原生 `async fn in trait`（无 `#[async_trait]`）
//!
//! # 模块
//!
//! - [`vm`]：虚拟机契约——[`VmManager`] trait + `Vm`/`VmSpec`/`VmNic`/`CpuTopology`/`VmState`。
//! - [`container`]：容器契约——[`ContainerRuntime`] trait + `Container`/`ContainerSpec`/`ContainerState`/`PortMapping`。
//! - [`runtime`]：容器运行时编排骨架——youki/runc 命令构造（纯函数）+ [`ContainerRuntimeImpl`] + [`ContainerRuntimeRunner`] / [`ImagePuller`]。
//! - [`container_net`]：自研容器网络契约——[`ContainerNetwork`] trait（补齐 youki 短板）。
//! - [`cni`]：CNI 容器网络实现（plugin 编排）。
//! - [`oci`]：OCI runtime 抽象（spec 渲染）。
//! - [`pkg`]：第三方包契约——[`PackageManager`] trait + `PackageInfo`/`PackageState`。
//! - [`apt`]：apt/dpkg 编排契约——`apt::AptRunner` trait（命令构造纯函数 + `TokioAptRunner`/`FixtureAptRunner`）。
//! - [`desktop`]：桌面/显示契约（VM/容器 GUI 接入）。
//! - [`virt_check`]：CPU 虚拟化能力检测（KVM 前置：VT-x/AMD-V/嵌套/KVM 模块）。
//! - [`error`]：`ComputeError` / `ComputeResult`。
//! - `mock`：测试桩（仅 `mock` feature）。
//!
//! # 关键 trait
//!
//! - [`VmManager`]：VM 生命周期（define/start/stop/migrate/snapshot，zvol 磁盘后端）。
//! - [`ContainerRuntime`]：容器生命周期 + 状态机（`ContainerState` 转换由 `can_transition`/`validate_transition` 守卫）。
//! - [`ContainerNetwork`]：容器网络（端口映射 + IpCidr，复用 os_network 类型）。
//! - [`PackageManager`]：第三方包安装/升级/查询（apt 实现见 `apt::AptRunner`）。
//! - [`ImagePuller`]：容器镜像拉取抽象（默认 `StubImagePuller`，youki 接入后换真）。
//! - [`ContainerRuntimeRunner`]：OCI runtime 执行抽象（`YoukiRunner` 真实执行需 root + youki 二进制）。
//!
//! # feature 门控
//!
//! - `mock`（默认关）：开启 `mock` 模块（`MockVmManager`/`MockContainerRuntime`/`MockPackageManager`/`MockContainerNetwork`）。
//! - `virt-ffi`（默认关）：引入 `virt` crate 依赖，启用真实 libvirt FFI 后端 [`LibvirtVmManager`]
//!   （需 `apt install libvirt-dev`）；默认仅内存态骨架（XML 渲染 + 状态机可测，无系统依赖）。
//!
//! # 默认实现
//!
//! - [`LibvirtVmManager`]：实现 [`VmManager`]（feature `virt-ffi` 下真实 libvirt，否则内存态骨架）。
//! - [`ContainerRuntimeImpl`]：实现 [`ContainerRuntime`]，编排 youki（命令构造真实可测，真执行需 root）。

#![allow(async_fn_in_trait)]

pub mod apt;
pub mod cni;
pub mod container;
pub mod container_net;
pub mod desktop;
pub mod error;
#[cfg(feature = "mock")]
pub mod mock;
pub mod oci;
pub mod pkg;
pub mod runtime;
pub mod virt_check;
pub mod vm;

mod impl_vm;

pub use container::{
    can_transition, validate_transition, Container, ContainerMount, ContainerRuntime,
    ContainerSpec, ContainerState, ImageInfo, MountSource, PortMapping,
};
pub use container_net::{ContainerNetwork, NetworkDriver, NetworkInfo};
pub use error::{ComputeError, ComputeResult};
pub use pkg::{
    can_transition_package, PackageId, PackageInfo, PackageManager, PackageSource, PackageState,
};
// 容器运行时编排骨架（youki/runc 命令构造 + spawn 抽象 + 编排实现）。
// - trait 抽象先行（youki 未注册，批 3 引 youki 后接真实执行）；
// - 命令构造层（*_argv）真实可测；YoukiRunner 真实执行需 root + youki 二进制（#[ignore]）。
pub use runtime::{
    parse_state_status, status_to_state, ContainerRuntimeImpl, ContainerRuntimeRunner, ImagePuller,
    StubImagePuller, YoukiRunner, DEFAULT_RUNTIME_BIN, DEFAULT_STATE_ROOT, DEFAULT_STOP_SIGNAL,
    FORCE_STOP_SIGNAL,
};
pub use vm::{CpuTopology, NicModel, Vm, VmFirmware, VmManager, VmNic, VmSpec, VmState};

// CPU 虚拟化能力检测（KVM 前置检查）。
// - 启动 VM 前调 preflight_virt_check / detect_virt_capability，把"CPU 不支持 /
//   BIOS 未开 VT-x / KVM 模块未加载"翻译成用户能懂的诊断信息，避免等到 libvirt
//   启动失败才看到晦涩错误。
// - 纯逻辑（parse_cpuinfo/parse_modules/诊断生成）与真实 I/O 分离，前者全覆盖单测。
pub use virt_check::{
    detect_virt_capability, parse_cpuinfo, parse_modules, preflight_virt_check, CpuVendor,
    NestedVirtStatus, VirtCheckResult,
};

// VM 实现（vm-agent）。
// - 默认路径：内存态骨架（XML 渲染 + 状态机 + 内存索引可测），无系统依赖。
// - feature `virt-ffi`：真实 libvirt FFI（virt crate），需 `apt install libvirt-dev`。
pub use impl_vm::{LibvirtDomainState, LibvirtVmManager};

// VM mock + Container mock 统一归并到 `mock` 模块（review2 P-R2-2 / R1 P5）：
// 原 `mock_vm.rs` 已并入 `mock.rs`，下游统一从 `mock` re-export 取齐——
// VM 与 container 的 mock 符号零重叠（不同 id/状态机），归并后无冲突。
#[cfg(feature = "mock")]
pub use mock::{MockContainerNetwork, MockContainerRuntime, MockPackageManager, MockVmManager};
