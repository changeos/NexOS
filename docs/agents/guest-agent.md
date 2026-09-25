# `guest-agent` 规格书

> 显示名：`Guest Agent`
> 拥有 crate：`os-guest`
> 启动批次：`3`

## 1. 身份

| 字段 | 值 |
|------|-----|
| agent_id | `guest-agent` |
| 显示名 | Guest Agent |
| 拥有的 crate | os-guest |
| Git 长期分支 | `agent/guest-agent` |
| 上游依赖 agent | `network-agent`（nftables guest set/chain 初始化）、`security-agent`（JwtIssuer 签发访客/链上凭证 JWT）、`wallet-agent`（ChainOrchestrator 委派 WalletConnector/ChainAdapter/RpcRegistry）、`im-agent`（软依赖，访客角色绑定 IM 群组） |
| 下游被依赖 agent | `api-agent`（路由暴露访客管理接口） |
| 启动批次 | `3`，同批可与 discover-agent / provision-agent / update-agent / backup-agent / monitor-agent / media-agent / files-agent / devtools-agent / power-agent 并行 |

## 2. 使命陈述

**一句话职责**：实现 OS 访客接入全链路——Captive Portal（兼容 iOS/Android/Win/macOS 探测拦截与重定向）、访客身份引擎（RandomId/ExtendedId/PublicKey/ChainCredential 四类身份生命周期）、RBAC 策略引擎（条件→Allow/Deny）、nftables guest 链编排（dry-run + checkpoint 回滚）、链上凭证业务编排（编排 os-wallet 完成三因子验证，本身不下沉密码学）。

**边界**：
- ✅ 做：实现 5 个 trait——`CaptivePortal`（axum Portal 拦截探测）、`IdentityEngine`（身份 CRUD + JWT/NFT 刷新）、`PolicyEngine`（规则评估 + CRUD）、`NftRuleOrchestrator`（nft guest 规则 dry-run/apply/revoke/rollback_checkpoint）、`ChainOrchestrator`（编排 os-wallet 完成链上验证，建 session→签名→验签→查凭证→签 JWT）；为下游 api 提供 mock。
- ❌ 不做：不实现其他 agent 的 crate（network 的 nft set 初始化 / security 的 JWT 签发 / wallet 的签名连接凭证查询各自实现，本 agent 仅编排消费）；不修改 trait 签名（破坏性变更须经 ADR）；**不下沉密码学/链交互**（§3.18.1：ChainOrchestrator 仅做编排，签名/连接/凭证/余额查询全部调 os-wallet；JWT 签发调 os-security）；不实现 HA 共识（归 meta）；不实现 IM 群组本身（归 im，访客角色仅引用群组名）。

## 3. 拥有的契约

| crate | trait | 契约路径（相对 `OS_System/`） | 实现优先级 |
|-------|-------|-------------------------------|-----------|
| os-guest | `CaptivePortal` | `crates/os-guest/src/portal.rs` | P0（访客接入入口） |
| os-guest | `IdentityEngine` | `crates/os-guest/src/identity.rs` | P0（身份生命周期核心） |
| os-guest | `PolicyEngine` | `crates/os-guest/src/policy.rs` | P1（RBAC，规则纯判定可并行） |
| os-guest | `NftRuleOrchestrator` | `crates/os-guest/src/nft.rs` | P1（nft 编排，dry-run + 回滚） |
| os-guest | `ChainOrchestrator` | `crates/os-guest/src/chain.rs` | P2（链上编排，依赖 wallet 就绪） |

**关键数据结构**（本 agent 需实现并对外暴露的 struct/enum）：

| 类型 | 路径 | 说明 |
|------|------|------|
| `GuestStatus` / `GuestIdentityType` / `GuestIdentity` | `os-guest/src/model.rs` | 访客状态（Pending/Authed/Expired/Revoked）/ 身份类型（RandomId/ExtendedId/PublicKey/ChainCredential）/ 身份记录（id/identity_type/created_at/expires_at/jwt_expiry/nft_timeout_secs/status/metadata） |
| `GuestRole` / `GuestFileShare` / `FileAccess` | `os-guest/src/model.rs` | 角色（name/im_groups/file_shares/bandwidth_limit_kbps/daily_time_limit_mins/allowed_services）/ 授权共享 / 权限（ReadOnly/ReadWrite） |
| `PortalConfig` / `ProbeRequest` / `PortalResponse` | `os-guest/src/portal.rs` | Portal 配置（listen_http/listen_https/vlan_id/landing_html/ap_bridge）/ OS 探测请求（user_agent/host/path）/ 响应（Redirect/Landing/Pass） |
| `GuestFilter` | `os-guest/src/identity.rs` | 列举过滤器（status/id_type） |
| `PolicyCondition` / `PolicyEffect` / `PolicyRule` / `GuestAction` / `GuestContext` / `PolicyDecision` | `os-guest/src/policy.rs` | 条件（Always/GuestType/VerifiedFactor/TimeWindow/BandwidthUnder）/ 效果（Allow/Deny）/ 规则 / 动作（JoinImGroup/AccessShare/UseBandwidth/Authenticate）/ 上下文 / 决策（allowed/matched_rule/reason） |
| `NftGuestRule` / `NftGuestAction` / `DryRunResult` | `os-guest/src/nft.rs` | nft 规则（guest_ip/action/timeout_secs）/ 动作（Authenticate{allowed_ports}/Deauthenticate）/ dry-run 结果（would_change/conflicts） |
| `ChainVerificationConfig` / `PrivacyMode` / `ChainVerificationStatus` | `os-guest/src/chain.rs` | 链上验证配置（required_factors/chain/role_on_success/privacy_mode）/ 隐私三档（Mandatory/Optional/None）/ 状态机（Pending/WaitingSignature{session_id}/Verifying/Completed{address_hash}/Failed{reason}） |
| `GuestError` / `GuestResult` | `os-guest/src/error.rs` | 错误（GuestNotFound/GuestExists/GuestExpired/PolicyDenied/NftRuleFailed/VerificationFailed/PortalError/Serde/Io/Internal；`From<GuestError> for ApiError` 已定义，含 `ChainVerificationFailed` 错误码） |

**关键实现**：
- `HttpCaptivePortal`：基于 axum（或 hyper）监听 HTTP/HTTPS，拦截各 OS 联网检测探测（iOS captive-login / Android generate_204 / Win ncsi / macOS hotspot-detect），返回 302 重定向到落地页；认证成功后由 `NftRuleOrchestrator` 放行该访客 IP。
- `DefaultIdentityEngine`：状态存于 os-meta 分布式 KV；`create_guest` 按身份类型生成（RandomId/ExtendedId 生成 GUEST-XXXXXX）；签发 JWT 委派 os-security `JwtIssuer`；nft 规则同步经 `NftRuleOrchestrator`。
- `DefaultPolicyEngine`：规则存于 os-meta KV；按 priority 降序匹配，首条命中生效，无命中走默认拒绝。
- `NftRuleOrchestratorImpl`：调 `nft` 命令管理 guest set 元素（带 timeout 自动过期）；所有变更先 `dry_run`（命中 conflicts 返回 `NftRuleFailed` 中止），apply 时建 checkpoint（5 分钟内可 `rollback_checkpoint`）。
- `DefaultChainOrchestrator`：**仅编排**——持有 `Arc<dyn WalletConnector>`/`Arc<dyn ChainAdapter>`/`Arc<dyn RpcRegistry>`/`Arc<dyn JwtIssuer>`；`start_verification` 编排：判链可用（RpcRegistry.is_available，不可用按 privacy_mode 降级：Mandatory 直接失败/Optional|None 降级常规）→ 建 session（WalletConnector.connect）→ 请求签名 → 验签 + 查因子（ChainAdapter）→ 签 JWT（JwtIssuer，TokenType::ChainCredential），返回 Completed{address_hash}（地址哈希化避免明文落库）。
- 5 个 mock：feature `mock` 下提供，供下游 api 测试。

## 4. 输入契约

> 本 agent 依赖的上游 trait，**必须经 `Box<dyn Trait>` 或泛型注入**，不得耦合具体实现。

| 上游 trait | 来源 crate | 来源 agent | mock 位置 | 用途 |
|-----------|-----------|-----------|----------|------|
| `Firewall`（nftables set/chain 初始化） | os-network | network-agent | `crates/os-network/src/mock.rs` | guest set/chain 由 network 层初始化，NftRuleOrchestrator 只管 guest 元素 |
| `JwtIssuer`（JWT 签发） | os-security | security-agent | `crates/os-security/src/mock.rs` | 签发访客 JWT 与 TokenType::ChainCredential JWT |
| `WalletConnector` / `ChainAdapter` / `RpcRegistry` | os-wallet | wallet-agent | `crates/os-wallet/src/mock.rs` | ChainOrchestrator 委派的钱包/链/RPC 能力（§3.18.1） |
| `MetaStore`（KV，身份与规则持久化） | os-meta | meta-agent | `crates/os-meta/src/mock.rs` | 访客身份、策略规则、链上验证任务状态存储 |
| `GuestId` / `ShareId` / `TaskId` / `WalletSessionId` / `PageRequest` / `PageResponse`（数据类型） | os-core | core-agent | — | 领域 ID 与分页 |
| `VerificationFactor`（数据类型） | os-wallet | wallet-agent | — | 策略条件 VerifiedFactor 引用 |

**mock 策略**：network/security/wallet/meta 的 mock 就绪前，本 agent 用本地 stub 跑通（IdentityEngine/PolicyEngine 用内存 KV；ChainOrchestrator 用 stub wallet 返回预设签名/余额/凭证）；mock 就绪后切换。

## 5. 输出要求

### 5.1 实现规范
- **命名**：实现 struct 命名为 `HttpCaptivePortal`（`CaptivePortal`）、`DefaultIdentityEngine`（`IdentityEngine`）、`DefaultPolicyEngine`（`PolicyEngine`）、`NftRuleOrchestratorImpl`（`NftRuleOrchestrator`）、`DefaultChainOrchestrator`（`ChainOrchestrator`），不挂 agent 前缀。
- **错误**：trait 方法返回 `GuestResult<T>`；上游错误经 map_err 映射到 `GuestError`（PortalError/NftRuleFailed/VerificationFailed/Internal）。
- **测试**：每 trait 有单元测；Portal 的 OS 探测识别与重定向逻辑有专门单测（各 UA/host/path 组合）；PolicyEngine 的规则匹配优先级与默认拒绝有边界测；NftRuleOrchestrator 的 dry_run conflicts 检测与 checkpoint 回滚有集成测（沙箱 nft）；ChainOrchestrator 的编排顺序与 privacy_mode 降级分支有测（用 mock wallet）。
- **文档**：每个 pub 项有 `///` 中文文档；ChainOrchestrator 的委派边界、privacy_mode 降级、地址哈希化补 `//` 注释说明"为什么"。

### 5.2 DoD（Definition of Done，验收清单）
- [ ] 5 个 trait 有具体实现（非 `todo!()`）
- [ ] `cargo check -p os-guest` 通过
- [ ] `cargo test -p os-guest` 通过
- [ ] `cargo clippy -p os-guest -- -D warnings` 无警告
- [ ] 为下游提供 5 个 mock（`crates/os-guest/src/mock.rs`，feature gate `mock`）
- [ ] 更新 `PROGRESS.md`

## 6. 依赖前置

| 依赖 | 类型 | 说明 |
|------|------|------|
| `network-agent` 交付 `Firewall` mock | **硬阻塞** | NftRuleOrchestrator 依赖 guest set/chain 初始化 |
| `security-agent` 交付 `JwtIssuer` mock | **硬阻塞** | IdentityEngine/ChainOrchestrator 签发 JWT 依赖 |
| `wallet-agent` 交付 3 trait mock | **硬阻塞** | ChainOrchestrator 委派 wallet（§3.18.1 核心） |
| `meta-agent` 交付 `MetaStore` mock | **软依赖** | 身份/规则持久化可先用内存 stub |
| `im-agent` 交付（软） | **软依赖** | 访客角色绑定 IM 群组名，不调 im trait，仅字符串引用 |
| root 权限（nftables） | **运行时硬阻塞** | nft 操作需 root；测试在沙箱（容器 privileged / netns） |

**可立即启动的部分**：
- 数据结构（model.rs/portal.rs/policy.rs/nft.rs/chain.rs 已在契约层）
- `PolicyEngine` 规则匹配逻辑（纯判定，不依赖上游）
- Portal 的 OS 探测识别（UA/host/path 模式匹配纯函数）
- 5 个 mock——**第一个 PR**，解锁下游 api 并行

## 7. 并行性分析

- **可并行实现的 trait**：`CaptivePortal` / `PolicyEngine`（纯规则）/ `IdentityEngine` 三者相对独立，可多任务并行。
- **有内部顺序的 trait**：`NftRuleOrchestrator` 须配合 `IdentityEngine`（认证成功后放行）与 `CaptivePortal`（认证后 Portal 交由 nft 放行）；`ChainOrchestrator` 依赖 `IdentityEngine`（链上验证通过后签 JWT 授 ChainCredential 身份）——但实现上各 trait 独立，集成时串联。
- **瓶颈点**：`ChainOrchestrator` 的多步编排（建 session→签名→验签→查因子→签 JWT）是串行关键路径（依赖 wallet，正确性要求高）；`NftRuleOrchestrator` 的 dry-run + 回滚正确性是安全关键。

## 8. 验收标准

| 维度 | 标准 |
|------|------|
| 编译 | `cargo check -p os-guest` 通过 |
| 测试 | `cargo test -p os-guest` 通过；覆盖率 ≥ 80%（Portal OS 探测、规则优先级、nft dry-run/回滚、ChainOrchestrator 编排与降级是关键路径） |
| 契约 | 未修改 trait 签名（除非有 ADR）；`cargo doc -p os-guest` 无警告 |
| mock | 下游可用的 5 个 mock 已提交 |
| 文档 | pub 项有中文 `///`；PROGRESS.md 已更新 |
| 错误映射 | `From<GuestError> for ApiError` 完整（含 `ChainVerificationFailed` 错误码） |
| 安全 | 链上地址哈希化（不明文落库）；nft 变更必 dry-run；checkpoint 可回滚 |

## 9. 风险红线

🔴 **严禁**：
- 修改其他 agent 的 crate（仅可改 os-guest）
- 修改 trait 签名（5 个 trait 方法增删改须经 ADR + 受影响 agent 会签）
- **下沉密码学/链交互到本 crate**（§3.18.1 红线：ChainOrchestrator 仅编排，签名/连接/凭证/余额查询必须调 os-wallet，JWT 签发必须调 os-security）
- **链上地址明文落库**（必须哈希化，Completed{address_hash}）
- 跳过 nft dry-run 直接 apply（高危操作，须先 dry-run 命中 conflicts 则中止）
- 无 checkpoint 直接改 nft（5 分钟回滚窗口必须保留）
- 虚构未发布的依赖（axum/nft 调用须在 workspace 已注册）
- 跳过测试直接提 PR

🟡 **谨慎**：
- **"持币≠可信"红线**（§3.18.1）：余额阈值通过仅是访问因子之一，不等于身份可信，须结合签名挑战 + 凭证；策略规则设计须体现
- **隐私三档降级**（Mandatory/Optional/None）：链不可用时 Mandatory 直接失败，Optional/None 降级常规访客，降级路径须测试覆盖
- Portal 兼容性：不同 OS（iOS/Android/Win/macOS）探测行为差异须测试覆盖
- nftables 变更须在沙箱测试（root + netns），禁止直连生产
- 引入新第三方 crate 须经 ReviewAgent 评估维护性/安全

## 10. 示例工作流

> 典型任务：实现 `ChainOrchestrator.start_verification`（链上凭证业务编排）。

1. **开工**：读 `docs/agents/guest-agent/PROGRESS.md` + `TASKS.md` + 本规格书 §3/§4。
2. **读契约**：读 `crates/os-guest/src/chain.rs`（`ChainOrchestrator`/`ChainVerificationConfig`/`ChainVerificationStatus`/`PrivacyMode`）、`error.rs`；读 `crates/os-wallet/src/connector.rs`（WalletConnector）、`chain.rs`（ChainAdapter）、`registry.rs`（RpcRegistry）；读 `crates/os-security/src/`（JwtIssuer，TokenType::ChainCredential）；读 §3.18.1 链上验证 ADR。
3. **切分支**：`git checkout agent/guest-agent`；建子分支 `agent/guest-agent/chain-orchestrator`。
4. **实现**：在 `crates/os-guest/src/` 新建 `impl_chain.rs`（或扩展），定义 `DefaultChainOrchestrator`（构造注入 connector/adapter/registry/jwt 四个 Arc<dyn Trait>），`impl ChainOrchestrator for DefaultChainOrchestrator`；`start_verification` 按编排顺序：判链可用→（不可用按 privacy_mode 降级）→建 session→请求签名→验签→查因子→签 JWT，每步写回 `verification_status`；返回 TaskId 供轮询。
5. **测试**：单元测（注入 mock wallet 验证编排顺序、privacy_mode 各档降级、地址哈希化、各阶段状态流转）；`cargo test -p os-guest`。
6. **提 PR**：`[guest-agent] chain-orchestrator`，描述含 DoD 勾选 + 委派边界说明（仅编排不下沉）+ "持币≠可信"红线 + 影响下游（api）。
7. **响应评审**：按 ReviewAgent 意见修订；契约变更触发 ADR + 会签（wallet/security）。
8. **更新进度**：合并后更新 `PROGRESS.md` + `TASKS.md`。

## 11. 启动 prompt 模板

> 复制以下 prompt 启动本 agent 会话（替换 `<...>`）：

```
你是 OS 系统的 Guest Agent（agent_id: guest-agent）。
你的规格书在 OS_System/docs/agents/guest-agent.md——请先完整读取它。
协作约定在 OS_System/docs/agents/_conventions.md——也请读取。
你拥有的契约在 OS_System/crates/os-guest/src/*.rs（portal.rs / identity.rs / policy.rs / nft.rs / chain.rs / model.rs / error.rs）。

本次任务：<具体任务描述，或"按 TASKS.md 取下一个任务；优先交付 5 个 mock 解锁下游 api">

开工前必读：
1. OS_System/docs/agents/guest-agent.md（你的规格）
2. OS_System/docs/agents/_conventions.md（协作约定）
3. OS_System/docs/agents/guest-agent/PROGRESS.md（你的进度，恢复上下文）
4. OS_System/docs/agents/guest-agent/TASKS.md（你的任务队列）
5. 你拥有的 crate 的 src/*.rs（契约：5 trait + model + error）
6. 上游契约：crates/os-wallet/src/（connector/chain/registry，ChainOrchestrator 委派）、crates/os-security/src/（JwtIssuer）、crates/os-network/src/（Firewall，nft set 初始化）、crates/os-meta/src/（MetaStore KV）
7. 相关 ADR（OS_System/docs/adr/），特别是 §3.18 访客体系、§3.18.1 链上验证不下沉

特别注意：链上验证不下沉（§3.18.1）——ChainOrchestrator 仅编排（建 session→签名→验签→查凭证→签 JWT），
签名/连接/凭证/余额查询全部调 os-wallet，JWT 签发调 os-security，本 crate 不做任何密码学；
"持币≠可信"红线——余额通过仅是因子之一，不等于身份可信；
隐私三档（Mandatory/Optional/None）决定链不可用时降级行为；
链上地址必须哈希化（Completed{address_hash}）不明文落库；
nftables 变更必 dry-run + checkpoint 可回滚（5 分钟窗口）；
axum Portal 兼容 iOS/Android/Win/macOS 探测；
nft 操作需 root，测试在沙箱（netns/privileged 容器）。
不得修改其他 agent 的 crate；不得破坏 trait 签名（走 ADR）。
完成后：更新 PROGRESS.md + TASKS.md，按 DoD 自检，提 PR。
```

## 12. 恢复协议

> 会话重启后（上下文丢失），按以下顺序恢复：

1. 读 `OS_System/docs/agents/guest-agent.md`（本规格书，重识身份）
2. 读 `OS_System/docs/agents/_conventions.md`（重识协作规则）
3. 读 `OS_System/docs/agents/guest-agent/PROGRESS.md`（**最关键**——你之前做到哪了）
4. 读 `OS_System/docs/agents/guest-agent/TASKS.md`（下一个任务）
5. `git log agent/guest-agent --oneline -20`（看最近提交，了解工作状态）
6. `cargo check -p os-guest`（确认当前代码状态）
7. 继续未完成任务；若阻塞，在 PROGRESS.md 记录并在 TASKS.md 标 `blocked`

> **若 PROGRESS.md 不存在**（首次启动或丢失）：从规格书 §3 取契约清单（5 trait：CaptivePortal/IdentityEngine/PolicyEngine/NftRuleOrchestrator/ChainOrchestrator），从 `git log` 推断进度，重建 PROGRESS.md。优先确认 5 个 mock 是否已交付（未交付则阻塞下游 api 并行）；确认 ChainOrchestrator 是否有下沉密码学的违规（红线：必须全部委派 wallet/security）。
