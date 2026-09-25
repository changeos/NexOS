//! # nettest — 网络栈真机连通性验证
//!
//! 临时验证 crate（feature/nettest 分支）。本 crate 的唯一目的是：在真实机器上
//! 真实跑通 OS 系统选定的网络栈（reqwest / axum / mdns-sd / rustls），证明这套
//! 纯 Rust TLS 栈（rustls + ring 后端，无 openssl）能真实联网、真实监听、真实组播、
//! 真实完成 TLS 握手。
//!
//! ## 为什么需要它
//!
//! 在此之前，所有网络相关 crate（os-mobile / os-api / os-discover / os-guest /
//! os-wallet / os-update 等）的网络测试都用 fixture 或 loopback mock，从未验证
//! 真实网络层能跑。本 crate 补这一层回归测。
//!
//! ## 如何运行
//!
//! 所有真实网络测试都标记了 `#[ignore]`，默认 `cargo test` 不执行（保持 CI 干净）。
//! 手动真机验证时运行：
//!
//! ```sh
//! cargo test -p nettest -- --ignored --nocapture --test-threads=1
//! ```
//!
//! 默认（非 ignored）测试只有一个 `smoke`，证明 crate 本身能编译通过。
//!
//! ## 测试分类
//!
//! 网络栈（默认依赖，无 feature 门控）：
//!
//! | 测试                       | 验证内容                              | 网络 |
//! |----------------------------|---------------------------------------|------|
//! | `reqwest_real_get`         | reqwest 真实 HTTPS GET（公网）        | 公网 |
//! | `axum_real_listen_and_get` | axum 真实端口监听 + reqwest 真实 HTTP | loopback |
//! | `mdns_real_broadcast`      | mdns-sd 真实 mDNS 广播 + browse       | 组播 |
//! | `rustls_real_tls_handshake`| rcgen 自签证书 + rustls 真实 TLS 握手 | loopback |
//!
//! 存储执行层（subprocess，无 feature 门控）：
//!
//! | 测试            | 验证内容                                        | 依赖 |
//! |-----------------|-------------------------------------------------|------|
//! | `zfs_real_smoke`| 真实 spawn `zfs --version` + `zpool list`，验证 | zfsutils-linux +
//! |                 | zfs 二进制 + 内核 ZFS 模块加载（os-storage 栈）| 加载 zfs 模块 |
//!
//! 网络执行层（rtnetlink 默认编译；nftnl 受 `nftnl-ffi` feature 门控）：
//!
//! | 测试                       | 验证内容                                          | 依赖 |
//! |----------------------------|---------------------------------------------------|------|
//! | `rtnetlink_real_link_list` | rtnetlink 真实 netlink 查询网卡列表，断言含 lo    | netlink（建议 CAP_NET_ADMIN）|
//! | `nftnl_real_smoke`         | nftnl + mnl 真实提交 nft 事务（建表/链/accept lo）| root + libnftnl-dev + libmnl-dev（需 `--features nftnl-ffi`）|
//!
//! `nftnl_real_smoke` 整文件在 `#![cfg(feature = "nftnl-ffi")]` 门控下：不开 feature
//! 时该集成测目标为空（不进默认套件）；开 feature 需宿主装 `libnftnl-dev` +
//! `libmnl-dev`，详见 docs/SANDBOX.md §5.3。

#![forbid(unsafe_code)]

/// 非 ignored 的占位测试：证明 nettest crate 能编译通过、workspace members 配置正确。
#[test]
fn smoke() {
    assert_eq!(2 + 2, 4);
}
