//! os-network —— 网络与防火墙管理（接口 / VLAN / 桥 / 绑定 / nftables / DHCP / PXE / DNS / RDMA / DPU）。
//!
//! 提供：物理/虚拟接口管理（VLAN/桥/绑定）、防火墙（规则/NAT）、
//! 可插拔网络服务（DHCP/PXE/DNS）、IB-RoCE（RDMA）可选能力、DPU 带内/带外抽象。
//!
//! 详见规划文档 §3.9（OS_Network）与 §15「接口契约索引」。
//!
//! 契约规范：数据路径 trait 用原生 `async fn in trait`（无 `#[async_trait]`），
//! lib 顶部统一 `#![allow(async_fn_in_trait)]`；每 crate 自定义 `NetworkError`，
//! 并实现 `From<NetworkError> for os_common::ApiError` 以统一对外错误。
//!
//! # 模块
//!
//! - [`interface`]：物理/虚拟接口管理契约——[`NetworkManager`] trait（VLAN/桥/绑定/地址）。
//! - [`firewall`]：防火墙规则 + NAT 契约——[`Firewall`] trait。
//! - [`services`]：可插拔网络服务契约——[`DhcpServer`] / [`PxeServer`] / [`DnsServer`] trait。
//! - [`rdma`]：IB-RoCE（RDMA）能力契约——[`RdmaManager`] trait。
//! - [`dpu`]：DPU 带内/带外抽象——[`DpuBackend`] trait。
//! - [`backend`]：契约的内存/in-process 实现——[`NetlinkBackend`] / [`FirewallBackend`]（测试用）。
//! - [`rtnetlink_real`]：基于 `rtnetlink` crate 的真实 netlink 后端（接口 CRUD）。
//! - `nftnl_real`：基于 `nftnl`/`mnl` 的真实 nftables 后端（仅 `nftnl-ffi` feature；feature 关闭时本模块不存在）。
//! - [`error`]：`NetworkError` / `NetworkResult`。
//! - `mock`：测试桩（仅 `mock` feature，供下游测试注入）。
//!
//! # 关键 trait
//!
//! - [`NetworkManager`]：接口生命周期（create/list/up/down/delete + 地址/VLAN/桥/绑定）。
//! - [`Firewall`]：防火墙规则与 NAT（add/remove/list rules, set NAT）。
//! - [`DhcpServer`] / [`PxeServer`] / [`DnsServer`]：可插拔网络服务（start/stop/配置）。
//! - [`RdmaManager`]：RDMA（IB/RoCE）能力管理。
//! - [`DpuBackend`]：DPU 带内/带外通道抽象。
//! - [`NetlinkBackend`] / [`FirewallBackend`]：底层后端抽象，便于切真实/内存实现。
//!
//! # feature 门控
//!
//! - `mock`（默认关）：开启 `mock` 模块（`MockNetworkManager`/`MockFirewall`/...）供下游测试注入。
//! - `nftnl-ffi`（默认关）：引入 `nftnl`+`mnl` crate 依赖，启用 `nftnl_real` 真实 nftables FFI 后端模块。
//!
//! # 默认实现
//!
//! - [`NftFirewall`]：实现 [`Firewall`]，编排 nftables（内存后端或 `nftnl-ffi` 后端）。
//! - [`NetlinkManager`]：实现 [`NetworkManager`]，编排 rtnetlink。

#![allow(async_fn_in_trait)]

pub mod backend;
pub mod dpu;
pub mod error;
pub mod firewall;
pub mod interface;
#[cfg(feature = "nftnl-ffi")]
pub mod nftnl_real;
pub mod rdma;
pub mod rtnetlink_real;
pub mod services;

#[cfg(feature = "mock")]
pub mod mock;
#[cfg(feature = "mock")]
pub use mock::{
    MockDhcpServer, MockDnsServer, MockDpuBackend, MockFirewall, MockNetworkManager, MockPxeServer,
    MockRdmaManager,
};

pub use backend::{
    FirewallBackend, InMemoryFirewallBackend, InMemoryNetlinkBackend, NetlinkBackend,
    NetlinkManager, NftFirewall,
};
pub use dpu::*;
pub use error::{NetworkError, NetworkResult};
pub use firewall::*;
pub use interface::*;
pub use rdma::*;
pub use services::*;
