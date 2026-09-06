//! osd —— 系统编排守护进程（PID1 之后的核心 orchestrator：进程监管 + cgroup v2 资源隔离 + NTP 时间同步）。
//!
//! 定位（规划文档 §3.13 / §9.1#8）：
//! - 进程管理：拉起/停止/重启各业务组件进程（os-storage / os-meta / os-api ...）
//! - cgroup v2 资源隔离：每个组件按 `ResourceQuota` 限制 CPU/内存/IO
//! - NTP 时间同步：作为系统时钟权威来源（HA 集群一致性前提）
//!
//! 本 crate 仅定义契约（trait + 数据结构 + Error），实现由 owner agent 后续填充。
//! 数据路径 trait（Orchestrator / HealthProbe / NtpManager）用原生 `async fn in trait`。
//!
//! 决策依据：
//! - §9.1#8 —— NTP 由 osd 统管（不依赖外部 ntpd，避免双源冲突）
//! - §3.13 —— 组件按依赖拓扑启动（实现负责拓扑排序，循环依赖报 DependencyCycle）
//!
//! # 模块
//!
//! - [`orchestrator`]：组件编排契约——[`Orchestrator`] trait（按依赖拓扑拉起/停止组件）。
//! - [`component`]：组件描述符（`ComponentDescriptor`/`ComponentId`/`ComponentStatus`）。
//! - [`topo`]：依赖拓扑排序（`topological_sort`，循环依赖报 `DependencyCycle`）。
//! - [`health`]：健康探针契约——[`HealthProbe`] trait。
//! - [`cgroup`]：cgroup v2 资源配额——[`CgroupBackend`] trait + `CgroupsRsBackend`/`InMemoryCgroupBackend`。
//! - [`systemd_runner`]：systemd 进程监管后端——[`SystemdRunner`] trait + `TokioSystemdRunner`/`InMemorySystemdRunner`。
//! - [`ntp`]：NTP 管理契约——[`NtpManager`] trait + `NtpStatus`。
//! - [`ntp_impl`]：chrony 编排实现——`ChronyNtp`（`tokio::process` 跑 `chronyc`）+ [`NtpRunner`] trait。
//! - [`impl_orchestrator`]：编排框架实现——`SystemdOrchestrator`/`ComponentRegistry`。
//! - [`error`]：`OrchestratorError` / `OrchestratorResult`。
//! - `mock`：测试桩（仅 `mock` feature）。
//!
//! # 关键 trait
//!
//! - [`Orchestrator`]：组件编排总入口（start/stop/restart 全组件，按拓扑序）。
//! - [`HealthProbe`]：组件健康检查（ping/status）。
//! - [`CgroupBackend`]：cgroup v2 配额后端（set/get CPU/内存/IO 限制）。
//! - [`SystemdRunner`]：systemd 调用后端（start/stop/enable unit，便于注入内存后端单测）。
//! - [`NtpManager`] / [`NtpRunner`]：NTP 状态查询 / 配置写入抽象。
//!
//! # feature 门控
//!
//! - `mock`（默认关）：开启 `mock` 模块，导出 `MockHealthProbe` 供下游测试注入。
//!
//! # 默认实现
//!
//! - `SystemdOrchestrator`：实现 [`Orchestrator`]，拓扑排序 + 状态机 + 同组件串行化，接通 [`SystemdRunner`] 与 [`CgroupBackend`]。
//! - `ChronyNtp`：实现 [`NtpManager`]，编排 chrony（需 root + CAP_SYS_TIME）。
//! - `CgroupsRsBackend`：实现 [`CgroupBackend`]，基于 `cgroups-rs`（需 root）。

// 原生 async fn in trait，不做 dyn 派发；如需 trait object，由各 trait 单独标注 Send bound。
#![allow(async_fn_in_trait)]

pub mod component;
pub mod error;
pub mod health;
pub mod ntp;
pub mod orchestrator;
pub mod topo;

// cgroup v2 配额：基于 cgroups-rs 的真实后端 + 测试用内存后端（CgroupBackend trait）
pub mod cgroup;

// systemd 进程监管后端：TokioSystemdRunner（真实 systemctl/systemd-run）+ InMemorySystemdRunner（no-op）
pub mod systemd_runner;

// 框架实现（SystemdOrchestrator）：拓扑排序 + 状态机 + 同组件串行化可用，
// 真实 systemd 调用（已接通 SystemdRunner）+ 真实 cgroup 写入（已接通 cgroups-rs，需 root）见模块文档。
pub mod impl_orchestrator;

// ChronyNtp：NtpManager 的真实 chrony 编排实现（tokio::process 跑 chronyc，
// 需 root + CAP_SYS_TIME；测试注入 FakeRunner 避免真改系统时间）。见模块文档。
pub mod ntp_impl;

// Mock 实现：feature gate `mock` 守护（规格书 §5.1 / §5.2）
#[cfg(feature = "mock")]
pub mod mock;
#[cfg(feature = "mock")]
pub use mock::MockHealthProbe;

pub use cgroup::{CgroupBackend, CgroupQuota, CgroupsRsBackend, InMemoryCgroupBackend};
pub use component::{ComponentDescriptor, ComponentId, ComponentStatus};
pub use error::{OrchestratorError, OrchestratorResult};
pub use health::HealthProbe;
pub use impl_orchestrator::{ComponentRegistry, SystemdOrchestrator};
pub use ntp::{NtpManager, NtpStatus};
pub use ntp_impl::{
    parse_conf_servers, parse_tracking, rewrite_conf_servers, ChronyNtp, ChronyRunner, FakeRunner,
    NtpRunner, TrackingParsed, TRACKING_SAMPLE,
};
pub use orchestrator::Orchestrator;
pub use systemd_runner::{
    InMemorySystemdRunner, RecordedCall, SystemdRunner, TokioSystemdRunner, UnitType,
};
pub use topo::topological_sort;
