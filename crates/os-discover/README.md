# os-discover

> 节点发现与联邦层 · mDNS 发现 + mTLS 配对 + HA 资格检测 + 联邦决策 · 规划 §3.14

OS 的节点发现与联邦 crate：LAN 内节点发现（mDNS / 组播 beacon，带 ed25519 防伪
签名）、凭证配对互联（mTLS 双向认证）、HA 资格检测（硬指标）与联邦分支决策
（自动加入 HA 集群 / 仅 peer 同步 / 保持单机）。契约 + 默认实现（已接通真实
mdns-sd / rustls / ed25519）。

## 核心能力

- **节点发现**（`discovery` / `impls`）：`Discovery` trait + 默认实现
  `MdnsDiscovery`——mdns-sd 真实 mDNS 组播广播/扫描（ADR-DEPS-002 接通），
  `NodeCapabilities` / `PeerNode` / `PeerCallback` 发现数据模型。
- **beacon 防伪**（`beacon`）：challenge/nonce 生成与比对 + ed25519 真实验签
  （`generate_keypair` / `sign_beacon` / `verify_beacon_signature` /
  `pubkey_fingerprint`；re-export `SigningKey` / `VerifyingKey` 免直接依赖
  ed25519-dalek）。
- **凭证配对**（`auth` / `mtls`）：`PeerAuthenticator` trait（`PairingToken` /
  `PairingScope` / `PeerSession`）+ 默认实现 `MtlsPeerAuthenticator`——rustls
  0.23 真实 mTLS 双向认证（含 `cert_fingerprint`）。
- **HA 资格检测**（`capabilities`）：`qualify_peer` / `version_satisfies` 纯算法
  ——节点数 / 带宽 / ZFS / KVM / 版本兼容硬指标（`PeerQualification`）。
- **联邦决策**（`federation` / `federation_sm` / `impls`）：`FederationPolicy`
  trait（默认 `DefaultFederationPolicy`）+ `FederationStateMachine` 状态机
  （Probing → Authenticating → Qualifying → … → Active，`FederationEvent` 驱动）。

## 架构位置

**依赖**（上游）：`os-core`、`os-common`（`From<DiscoverError> for ApiError`）；
第三方 mdns-sd / rustls（ring 后端）/ ed25519-dalek / rand_core。

**被用**（下游）：os-provision（首次组网）、os-api、os-cli、os-mobile（手机/
桌面客户端发现本机 OS——lib.rs 列明的复用方）。

## 独立使用

- **仓库外引用**：`os-discover = { git = "http://ub2604:8080/git/nexos.git" }`。
- **契约规范**：数据路径 trait 原生 async fn in trait；自定义 `DiscoverError`。
- **关键接口**：`Discovery` / `PeerAuthenticator` / `FederationPolicy` 三 trait +
  `qualify_peer` 资格纯算法 + `FederationStateMachine` 决策状态机（三者拼起来
  即一条完整发现→配对→入盟流水线）。
- **feature**：`mock`（默认关）——`MockDiscovery` / `MockPeerAuthenticator` /
  `MockFederationPolicy` 测试桩。

## 测试

```bash
cargo test -p os-discover
```

纯逻辑（资格算法 / 状态机 / beacon 签名往返 / 内存 fixture 发现路径）+ 真实
mdns-sd loopback 广播→扫描→TXT 编解码往返测（不真改网络配置）；跨层真机
回归另见 nettest crate。
