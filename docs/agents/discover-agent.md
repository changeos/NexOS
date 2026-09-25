# `discover-agent` 规格书

> 显示名：`Discover Agent`
> 拥有 crate：`os-discover`
> 启动批次：`3`

## 1. 身份

| 字段 | 值 |
|------|-----|
| agent_id | `discover-agent` |
| 显示名 | Discover Agent |
| 拥有的 crate | os-discover |
| Git 长期分支 | `agent/discover-agent` |
| 上游依赖 agent | `core-agent`（`NodeId`/`DateTime`/`Utc` 等数据类型）、`meta-agent`（受信 peer 持久化，软依赖） |
| 下游被依赖 agent | `provision-agent`（首次组网发现 peer）、`client-agent`（手机/桌面客户端发现本机 OS） |
| 启动批次 | `3`，同批可与 provision-agent / guest-agent / update-agent / backup-agent / monitor-agent / media-agent / files-agent / devtools-agent / power-agent 并行 |

## 2. 使命陈述

**一句话职责**：实现 OS 节点的 LAN 发现与联邦——基于 mDNS/组播 beacon 广播与扫描（beacon 防伪签名），凭证配对互联（rustls mTLS 双向认证建立 peer 会话），HA 资格硬指标检测（节点数/带宽/ZFS/KVM/版本兼容），联邦分支决策（自动加入 HA 集群 / 仅 peer 同步 / 保持单机）。

**边界**：
- ✅ 做：实现 `Discovery`（start_advertising/stop_advertising/discover_peers/on_peer_discovered，mdns-sd 广播+扫描，beacon 签名防伪）；实现 `PeerCallback`（on_found/on_lost 事件回调）；实现 `PeerAuthenticator`（pair/unpair/list_trusted_peers，rustls mTLS）；实现 `FederationPolicy`（check_eligibility/decide，HA 资格硬指标检测 + 用户选择分支决策）；为下游 provision/client 提供 mock。
- ❌ 不做：不实现其他 agent 的 crate（security 的 CertManager 证书签发各自实现，本 agent 仅消费证书做 mTLS）；不修改 trait 签名（破坏性变更须经 ADR）；不下沉密码学（mTLS 握手用 rustls，证书签发/校验与 os-security CertManager 协同）；不实现 HA 共识协议本身（归 os-meta，本 agent 仅判定资格并产出动作）；不实现客户端 UI（os-mobile/desktop 消费本 crate 的 Discovery 协议）。

## 3. 拥有的契约

| crate | trait | 契约路径（相对 `OS_System/`） | 实现优先级 |
|-------|-------|-------------------------------|-----------|
| os-discover | `Discovery` | `crates/os-discover/src/discovery.rs` | P0（广播/扫描是发现入口） |
| os-discover | `PeerCallback` | `crates/os-discover/src/discovery.rs` | P0（与 Discovery 配套的事件回调） |
| os-discover | `PeerAuthenticator` | `crates/os-discover/src/auth.rs` | P1（mTLS 配对，依赖 rustls + 证书） |
| os-discover | `FederationPolicy` | `crates/os-discover/src/federation.rs` | P1（HA 资格检测 + 决策，纯规则判定） |

**关键数据结构**（本 agent 需实现并对外暴露的 struct/enum）：

| 类型 | 路径 | 说明 |
|------|------|------|
| `PeerNode` / `NodeCapabilities` | `os-discover/src/discovery.rs` | 发现到的对端节点（node_id/endpoints/version/arch/capabilities/beacon_signature）/ 能力声明（supports_ha/storage_capacity_gb/network_gbps/has_zfs/has_kvm/rdma/dpu——HA 资格硬指标输入） |
| `PairingToken` / `PairingScope` / `PeerSession` / `PeerSessionId` | `os-discover/src/auth.rs` | 配对凭证（token/expires_at/issued_by/scope）/ 作用域（JoinCluster/PeerSync/ClientAccess）/ 已建立会话（id/peer/established_at/mtls_cert_fingerprint）/ 会话 ID newtype |
| `HaRequirement` / `HaEligibility` | `os-discover/src/federation.rs` | HA 硬指标门槛（min_nodes/min_bandwidth_gbps/require_zfs/require_kvm/version_compat）/ 检测结果（eligible/reasons/checked_at） |
| `FederationChoice` / `FederationAction` | `os-discover/src/federation.rs` | 用户选择（Auto/ManualHa/ManualPeer/Decline）/ 决策动作（JoinHaCluster{leader_endpoint}/RegisterAsPeer/StayStandalone） |
| `DiscoverError` / `DiscoverResult` | `os-discover/src/error.rs` | 错误（PeerNotFound/PairingFailed/BeaconInvalid/MtlsHandshakeFailed/IncompatibleVersion/Io/Internal；`From<DiscoverError> for ApiError` 已定义） |

**关键实现**：
- `MdnsDiscovery`：基于 mdns-sd（或组播 beacon）周期性广播 `PeerNode`（含 `beacon_signature`——节点私钥签名 beacon 载荷，发现方校验防伪）；`discover_peers` 为一次性扫描（timeout_ms 内应答）；`on_peer_discovered` 注册持续扫描回调。beacon 签名无效的节点丢弃并记 `BeaconInvalid`。
- `MtlsPeerAuthenticator`：基于 rustls + 本地受信证书库（与 os-security CertManager 协同）；`pair` 用 `PairingToken` 完成 mTLS 双向认证，建立 `PeerSession`（记录 `mtls_cert_fingerprint`）；`unpair` 断开并移除受信关系；`list_trusted_peers` 列出受信节点。
- `DefaultFederationPolicy`：纯规则判定；`check_eligibility` 对照 `HaRequirement` 探测 `peers` 硬指标（节点数≥min_nodes、带宽≥min_bandwidth_gbps、require_zfs/kvm、版本在 version_compat 范围内）；`decide` 结合 `HaEligibility` 与用户 `FederationChoice` 产出 `FederationAction`。
- `MockDiscovery` / `MockPeerAuthenticator` / `MockFederationPolicy`：feature `mock` 下提供，供下游 provision/client 测试。

## 4. 输入契约

> 本 agent 依赖的上游 trait，**必须经 `Box<dyn Trait>` 或泛型注入**，不得耦合具体实现。

| 上游 trait | 来源 crate | 来源 agent | mock 位置 | 用途 |
|-----------|-----------|-----------|----------|------|
| `NodeId` / `DateTime` / `Utc` / `Serialize` / `Deserialize`（数据类型） | os-core | core-agent | — | 领域 ID / 时间戳 / 序列化 |
| `CertManager`（证书签发/校验，软） | os-security | security-agent | `crates/os-security/src/mock.rs` | mTLS 证书签发与指纹校验（与 PeerAuthenticator 协同） |
| `MetaStore`（受信 peer 持久化，软） | os-meta | meta-agent | `crates/os-meta/src/mock.rs` | 受信 peer 列表与配对凭证持久化 |

**mock 策略**：core 的数据类型属纯结构，先交付即可；security/meta 的 mock 就绪前，本 agent 用本地内存 stub（证书自签、受信列表内存存）跑通；mock 就绪后切换。

## 5. 输出要求

### 5.1 实现规范
- **命名**：实现 struct 命名为 `MdnsDiscovery`（`Discovery`）、`MtlsPeerAuthenticator`（`PeerAuthenticator`）、`DefaultFederationPolicy`（`FederationPolicy`），不挂 agent 前缀。
- **错误**：trait 方法返回 `DiscoverResult<T>`；mdns-sd/rustls 错误映射到 `DiscoverError`（Io/MtlsHandshakeFailed/Internal）；beacon 签名校验失败映射 `BeaconInvalid`；版本不兼容映射 `IncompatibleVersion`。
- **测试**：`MdnsDiscovery` 的广播/扫描/beacon 签名校验有单元测（loopback 组播或 mock）；`MtlsPeerAuthenticator` 的配对/解配/受信列表有集成测（自签证书双向握手）；`FederationPolicy` 的硬指标判定与分支决策有充分单测（边界值：刚好达标/差一项）。
- **文档**：每个 pub 项有 `///` 中文文档；beacon 签名防伪、mTLS 证书指纹校验、HA 资格硬指标逻辑补 `//` 注释说明"为什么"。

### 5.2 DoD（Definition of Done，验收清单）
- [ ] `Discovery` / `PeerCallback` / `PeerAuthenticator` / `FederationPolicy` 有具体实现（非 `todo!()`）
- [ ] `cargo check -p os-discover` 通过
- [ ] `cargo test -p os-discover` 通过
- [ ] `cargo clippy -p os-discover -- -D warnings` 无警告
- [ ] 为下游 agent 提供 mock（`crates/os-discover/src/mock.rs`，feature gate `mock`）：`MockDiscovery`/`MockPeerAuthenticator`/`MockFederationPolicy`
- [ ] 更新 `PROGRESS.md`

## 6. 依赖前置

| 依赖 | 类型 | 说明 |
|------|------|------|
| `core-agent` 交付 os-core 数据类型 | **软依赖** | 契约层，`cargo check` 通过即可 |
| `security-agent` 交付 `CertManager` mock | **软依赖** | mTLS 证书可先用自签 stub，真实 CertManager mock 就绪后切换 |
| `meta-agent` 交付 `MetaStore` mock | **软依赖** | 受信 peer 持久化可先用内存 stub |
| mdns-sd / rustls 在 workspace 注册 | **硬阻塞** | 第三方依赖须已注册（虚构依赖违反红线） |

**可立即启动的部分**：
- 数据结构（`PeerNode`/`NodeCapabilities`/`PairingToken`/`HaRequirement` 等已在契约层）
- `DefaultFederationPolicy` 的硬指标判定与分支决策（纯规则，不依赖上游）
- beacon 签名/校验逻辑（密码学纯计算，不依赖上游业务 trait）
- `MockDiscovery`/`MockPeerAuthenticator`/`MockFederationPolicy`——**第一个 PR**，解锁下游 provision/client 并行

## 7. 并行性分析

- **可并行实现的 trait**：`Discovery`（广播/扫描）与 `FederationPolicy`（资格检测/决策）二者独立，可并行；`PeerAuthenticator`（mTLS）相对独立，也可并行。
- **有内部顺序的 trait**：业务上 `Discovery.discover_peers` → `FederationPolicy.check_eligibility` → `FederationPolicy.decide` → `PeerAuthenticator.pair`（先发现→判资格→决策→配对）；但实现上各 trait 独立，可并行开发，集成时串联流程。
- **瓶颈点**：`MdnsDiscovery` 的 beacon 签名防伪与跨平台组播兼容性是关键路径（网络层不确定性高，需实测）；`PeerAuthenticator` 的 mTLS 双向握手正确性要求高。

## 8. 验收标准

| 维度 | 标准 |
|------|------|
| 编译 | `cargo check -p os-discover` 通过 |
| 测试 | `cargo test -p os-discover` 通过；覆盖率 ≥ 80%（beacon 签名校验、mTLS 握手、HA 资格边界判定、决策分支是关键路径） |
| 契约 | 未修改 trait 签名（除非有 ADR）；`cargo doc -p os-discover` 无警告 |
| mock | 下游可用的 mock 已提交（3 个） |
| 文档 | pub 项有中文 `///`；PROGRESS.md 已更新 |
| 错误映射 | `From<DiscoverError> for ApiError` 完整（已在 error.rs 定义，新增错误变体须同步映射） |
| 安全 | beacon 签名防伪、mTLS 证书指纹校验有测试覆盖 |

## 9. 风险红线

🔴 **严禁**：
- 修改其他 agent 的 crate（仅可改 os-discover）
- 修改 trait 签名（4 个 trait 方法增删改须经 ADR + 受影响下游 agent 会签——provision/client）
- 虚构未发布的依赖（mdns-sd/rustls 须在 workspace 已注册）
- 接受无 beacon 签名或签名无效的 peer（防伪红线，必须丢弃并记 `BeaconInvalid`）
- 跳过 mTLS 双向认证直连 peer（凭证配对是安全前提）
- 删除或重命名既有 pub 项（同上，走 ADR）
- 跳过测试直接提 PR

🟡 **谨慎**：
- 改 beacon 签名算法/密钥派生（安全相关，须 ReviewAgent + 安全评审）
- 改 mTLS 证书指纹算法（SHA-256 ↔ 其他，须 ADR + 会签 security-agent）
- 改 HA 资格硬指标门槛（min_nodes/min_bandwidth_gbps 等，影响集群稳定性，须 ADR）
- 跨平台组播兼容性（不同 OS 的 mDNS 行为差异须测试覆盖）
- 引入新第三方 crate 须经 ReviewAgent 评估维护性/安全

## 10. 示例工作流

> 典型任务：实现 `FederationPolicy.check_eligibility` + `decide`（HA 资格检测 + 分支决策）。

1. **开工**：读 `docs/agents/discover-agent/PROGRESS.md` + `TASKS.md` + 本规格书 §3/§4。
2. **读契约**：读 `crates/os-discover/src/federation.rs`（`FederationPolicy`/`HaRequirement`/`HaEligibility`/`FederationChoice`/`FederationAction`）、`discovery.rs`（`PeerNode`/`NodeCapabilities`）、`error.rs`；读 §3.14 联邦决策 ADR。
3. **切分支**：`git checkout agent/discover-agent`；建子分支 `agent/discover-agent/federation-policy`。
4. **实现**：在 `crates/os-discover/src/` 新建 `impl_federation.rs`（或扩展），定义 `DefaultFederationPolicy`，`impl FederationPolicy for DefaultFederationPolicy`；`check_eligibility` 逐项对照 `HaRequirement` 探测 peers（节点数、带宽、ZFS/KVM、版本兼容），不满足项收集到 `reasons`；`decide` 按 `FederationChoice`（Auto 时按 eligible 决定）产出 `FederationAction`。
5. **测试**：单元测（边界值：刚好达标/差一项/版本边界/各 FederationChoice 分支）；`cargo test -p os-discover`。
6. **提 PR**：`[discover-agent] federation-policy`，描述含 DoD 勾选 + 硬指标门槛说明 + 影响下游（provision/client）。
7. **响应评审**：按 ReviewAgent 意见修订；契约变更触发 ADR + 会签。
8. **更新进度**：合并后更新 `PROGRESS.md` + `TASKS.md`。

## 11. 启动 prompt 模板

> 复制以下 prompt 启动本 agent 会话（替换 `<...>`）：

```
你是 OS 系统的 Discover Agent（agent_id: discover-agent）。
你的规格书在 OS_System/docs/agents/discover-agent.md——请先完整读取它。
协作约定在 OS_System/docs/agents/_conventions.md——也请读取。
你拥有的契约在 OS_System/crates/os-discover/src/*.rs（discovery.rs / auth.rs / federation.rs / error.rs）。

本次任务：<具体任务描述，或"按 TASKS.md 取下一个任务；优先交付 MockDiscovery/MockPeerAuthenticator/MockFederationPolicy 解锁下游 provision/client">

开工前必读：
1. OS_System/docs/agents/discover-agent.md（你的规格）
2. OS_System/docs/agents/_conventions.md（协作约定）
3. OS_System/docs/agents/discover-agent/PROGRESS.md（你的进度，恢复上下文）
4. OS_System/docs/agents/discover-agent/TASKS.md（你的任务队列）
5. 你拥有的 crate 的 src/*.rs（契约）
6. 相关 ADR（OS_System/docs/adr/），特别是 §3.14 联邦决策、beacon 防伪签名
7. 上游：crates/os-security/src/（CertManager，mTLS 证书协同）、crates/os-meta/src/（MetaStore，受信 peer 持久化）

特别注意：mdns-sd 广播/扫描 + beacon 私钥签名防伪（签名无效必丢弃）；
rustls mTLS 双向认证配对（记录 mtls_cert_fingerprint）；
HA 资格硬指标检测（节点数/带宽/ZFS/KVM/版本兼容）+ 联邦分支决策（Auto/ManualHa/ManualPeer/Decline）；
HA 共识协议本身归 os-meta，本 agent 仅判资格产动作；
客户端 UI 归 os-mobile/desktop，本 crate 仅提供协议。
不得修改其他 agent 的 crate；不得破坏 trait 签名（走 ADR）。
完成后：更新 PROGRESS.md + TASKS.md，按 DoD 自检，提 PR。
```

## 12. 恢复协议

> 会话重启后（上下文丢失），按以下顺序恢复：

1. 读 `OS_System/docs/agents/discover-agent.md`（本规格书，重识身份）
2. 读 `OS_System/docs/agents/_conventions.md`（重识协作规则）
3. 读 `OS_System/docs/agents/discover-agent/PROGRESS.md`（**最关键**——你之前做到哪了）
4. 读 `OS_System/docs/agents/discover-agent/TASKS.md`（下一个任务）
5. `git log agent/discover-agent --oneline -20`（看最近提交，了解工作状态）
6. `cargo check -p os-discover`（确认当前代码状态）
7. 继续未完成任务；若阻塞，在 PROGRESS.md 记录并在 TASKS.md 标 `blocked`

> **若 PROGRESS.md 不存在**（首次启动或丢失）：从规格书 §3 取契约清单（4 trait：Discovery/PeerCallback/PeerAuthenticator/FederationPolicy），从 `git log` 推断进度，重建 PROGRESS.md。优先确认 3 个 mock 是否已交付（未交付则阻塞下游 provision/client 并行）；确认 beacon 签名防伪与 mTLS 指纹校验是否有测试覆盖。
