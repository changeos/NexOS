# os-network

> 网络层 · 接口/VLAN/桥/绑定 + nftables 防火墙 + DHCP/PXE/DNS + RDMA/DPU 抽象

OS 网络与防火墙管理 crate：物理/虚拟接口生命周期、防火墙规则与 NAT、可插拔
网络服务（DHCP/PXE/DNS），以及 IB-RoCE（RDMA）与 DPU 带内/带外抽象
（规划文档 §3.9 / §15 接口契约索引）。

## 核心能力

- **接口管理**（`interface`）：`NetworkManager` trait——接口 create/list/up/down/
  delete + 地址 / VLAN / 桥 / 绑定配置；默认实现 `NetlinkManager` 编排 rtnetlink
  （`rtnetlink_real` 真实 netlink 后端）。
- **防火墙与 NAT**（`firewall`）：`Firewall` trait——规则 add/remove/list + NAT；
  默认实现 `NftFirewall`，可在内存后端（测试）与 `nftnl-ffi` 后端（真实
  nftables）间切换。
- **可插拔网络服务**（`services`）：`DhcpServer` / `PxeServer` / `DnsServer`
  trait（start/stop/配置）。
- **RDMA 与 DPU**（`rdma` / `dpu`）：`RdmaManager`（IB/RoCE 能力管理）与
  `DpuBackend`（带内/带外通道抽象）。
- **后端可换**（`backend`）：`NetlinkBackend` / `FirewallBackend` 底层抽象 +
  InMemory 实现，真实/内存实现可切换；`mock` feature 提供
  `MockNetworkManager` / `MockFirewall` 等 7 个测试桩。

## 架构位置

**依赖**（上游）：`os-core`、`os-common`（`From<NetworkError> for ApiError`）；
第三方 rtnetlink；`nftnl-ffi` feature 引入 nftnl + mnl（需 libmnl-dev，缺库不进编译）。

**被用**（下游）：os-provision（PXE 组网）、os-compute（容器/VM 网络）、
os-meta（VIP）、os-security（VPN 防火墙编排）、os-guest（Captive Portal /
nftables 编排）。

## 独立使用

- **仓库外引用**：`os-network = { git = "http://ub2604:8080/git/nexos.git" }`。
- **契约规范**：数据路径 trait 原生 `async fn in trait`（无 `#[async_trait]`，
  lib 顶部统一 `#![allow(async_fn_in_trait)]`）；自定义 `NetworkError` 统一错误。
- **关键接口**：`NetworkManager` / `Firewall` / `DhcpServer` / `PxeServer` /
  `DnsServer` / `RdmaManager` / `DpuBackend` + 底层 `NetlinkBackend` /
  `FirewallBackend`。
- **feature**：`mock`（默认关，测试桩）；`nftnl-ffi`（默认关，真实 nftables
  FFI 执行层 `NftnlFirewallBackend`）。

## 测试

```bash
cargo test -p os-network
```

契约/内存后端/fake runner 单测默认跑；真实 netlink / devlink 环境测在
`tests/dpu_devlink_real.rs` 中以 `#[ignore]` 标记。
