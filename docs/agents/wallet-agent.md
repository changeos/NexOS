# `wallet-agent` 规格书

> 显示名：`Wallet Agent`
> 拥有 crate：`os-wallet`
> 启动批次：`2`

## 1. 身份

| 字段 | 值 |
|------|-----|
| agent_id | `wallet-agent` |
| 显示名 | Wallet Agent |
| 拥有的 crate | `os-wallet` |
| Git 长期分支 | `agent/wallet-agent` |
| 上游依赖 agent | `core-agent`、`security-agent`（`JwtClaims`） |
| 下游被依赖 agent | `guest-agent`（`ChainOrchestrator` 调 wallet）、`im-agent`（Tool 调 wallet） |
| 启动批次 | `2`，同批可与 protocol-agent、compute-agent、meta-agent 并行 |

## 2. 使命陈述

**一句话职责**：为 OS 系统提供区块链钱包与链适配能力——钱包连接（WalletConnect v2/注入/二维码）、链适配（BTC BIP-322/Schnorr + EVM EIP-191/712 签名验证、余额/凭证查询）、RPC 注册表（条件激活核心，按链可用性注册 adapter）。

**边界**：
- ✅ 做：实现 `os-wallet` 全部 3 个 trait；封装 WalletConnect v2（Relay+DeepLink）、rust-bitcoin（BIP-322/Schnorr）、alloy（EIP-191/712）；条件激活驱动 adapter 注册
- ❌ 不做：不实现其他 agent 的 crate；不修改 trait 签名（破坏性变更须经 ADR）；不实现 guest 的访问三因子编排（guest 复用本 crate 的 `ChainAdapter`/`VerificationFactor`）；不实现 security 的 JWT 签发（仅消费 `JwtClaims`）

## 3. 拥有的契约

| crate | trait | 契约路径（相对 `OS_System/`） | 实现优先级 |
|-------|-------|-------------------------------|-----------|
| `os-wallet` | `WalletConnector` | `crates/os-wallet/src/connector.rs` | P0 |
| `os-wallet` | `ChainAdapter` | `crates/os-wallet/src/chain.rs` | P0 |
| `os-wallet` | `RpcRegistry` | `crates/os-wallet/src/registry.rs` | P1（条件激活核心，独立） |

**关键数据结构**（本 agent 需实现并对外暴露的 struct/enum）：
- `ChainKind`（`Bitcoin`/`Evm`）、`ChainConfig`、`AddressInfo`、`SignatureAlgorithm`（`Bip322`/`Schnorr`/`Ecdsa`/`Eip191`/`Eip712`）、`VerificationFactor`（访问三因子，呼应 §3.18.1）
- `ConnectorKind`（`WalletConnectV2`/`Injected`/`QrCode`）、`WalletSession`、`SignRequest`、`SignResponse`
- `CredentialSpec`（`Ordinal`/`Erc721`/`Erc1155`）
- `RpcSource`（`Local`/`Remote`）、`RpcStatus`
- 实现 struct：`WalletConnectV2Connector`（WC v2）、`InjectedConnector`、`QrCodeConnector`、`BitcoinAdapter`（rust-bitcoin）、`EvmAdapter`（alloy）、`RpcRegistryImpl`（条件激活）

## 4. 输入契约

> 本 agent 依赖的上游 trait，**必须经 `Box<dyn Trait>` 或泛型注入**，不得耦合具体实现。

| 上游 trait | 来源 crate | 来源 agent | mock 位置 | 用途 |
|-----------|-----------|-----------|----------|------|
| `WalletSessionId`/`ChainId`/`AddressId`（数据类型） | `os-core` | `core-agent` | — | 会话/链/地址标识 |
| `JwtClaims`（含 `TokenType::ChainCredential`） | `os-security` | `security-agent` | `crates/os-security/src/mock.rs` | 链上凭证访客的 token 校验 |

**mock 策略**：core/security 的数据类型属纯结构，先交付即可；security 的 `JwtIssuer` mock 就绪前，本 agent 用本地 stub（构造测试 `JwtClaims`）跑通。

## 5. 输出要求

### 5.1 实现规范
- **命名**：实现 struct 命名为 `<Chain>Adapter`/`<Tool>Connector`（如 `BitcoinAdapter`、`EvmAdapter`、`WalletConnectV2Connector`、`RpcRegistryImpl`），不挂 agent 前缀
- **错误**：实现方法返回 `Result<T, WalletError>`；内部错误映射到 `WalletError` 枚举（实现 `From<WalletError> for os_common::ApiError`）
- **测试**：每个公开方法有单元测试；BIP-322/EIP-191 签名验证、WC v2 配对、RPC 探活需集成测（含降级场景）
- **文档**：每个 pub 项有 `///` 中文文档；条件激活/降级逻辑补 `//` 内联注释说明"为什么"

### 5.2 DoD（验收清单）
- [ ] 所有拥有的 trait 有具体实现（非 `todo!()`）
- [ ] `cargo check -p os-wallet` 通过
- [ ] `cargo test -p os-wallet` 通过
- [ ] `cargo clippy -p os-wallet -- -D warnings` 无警告
- [ ] 为下游 agent 提供 mock 实现（`crates/os-wallet/src/mock.rs`，feature gate `mock`）：`MockWalletConnector`/`MockChainAdapter`/`MockRpcRegistry`
- [ ] 更新 `PROGRESS.md`

## 6. 依赖前置

| 依赖 | 类型 | 说明 |
|------|------|------|
| `core-agent` 交付 `os-core` mock（WalletSessionId/ChainId/AddressId） | **硬阻塞** | 本 agent 启动前必须有此 mock |
| `security-agent` 交付 `JwtClaims` 类型 + `JwtIssuer` mock | **硬阻塞** | 链上凭证访客的 token 校验依赖；`TokenType::ChainCredential` 类型须先稳定 |
| `security-agent` 交付 `JwtIssuer` 真实实现 | **软依赖** | 可先用 mock 并行 |

**可立即启动的部分**：`ChainKind`/`SignatureAlgorithm`/`CredentialSpec` 等数据结构；`BitcoinAdapter`/`EvmAdapter` 的签名验证逻辑（rust-bitcoin/alloy 纯计算，不依赖 security）；`RpcRegistry` 的探活逻辑骨架。

## 7. 并行性分析

- **可并行实现的 trait**：`WalletConnector` / `ChainAdapter`（BTC + EVM 分两个子任务）两者相互独立，可多任务并行
- **有内部顺序的 trait**：`RpcRegistry`（条件激活核心）须能驱动 `ChainAdapter` 注册——但 `RpcRegistry` 本身独立于具体 adapter，可并行开发，集成时再串联
- **瓶颈点**：`ChainAdapter` 的多链签名验证（BTC BIP-322/Schnorr + EVM EIP-191/712）是串行关键路径（算法正确性要求高）
- **条件激活**：`RpcRegistry.is_available` → `register_adapter`（可用时注入）/ `unregister_adapter`（不可用时注销）驱动 `ChainAdapter` 的生命周期

## 8. 验收标准

| 维度 | 标准 |
|------|------|
| 编译 | `cargo check -p os-wallet` 通过 |
| 测试 | `cargo test -p os-wallet` 通过；关键路径（BIP-322/EIP-191 验签、WC v2 配对、RPC 探活/降级）覆盖率 ≥ 80% |
| 契约 | 未修改 trait 签名（除非有 ADR）；`cargo doc` 无警告 |
| mock | 下游可用的 mock 已提交（3 个） |
| 文档 | pub 项有中文 `///`；PROGRESS.md 已更新 |

## 9. 风险红线

🔴 **严禁**：
- 修改其他 agent 的 crate（仅可改 `os-wallet`）
- 修改 trait 签名（破坏性变更须经 ADR + 受影响 agent 会签）
- 虚构未发布的依赖（WalletConnect v2/rust-bitcoin/alloy 须在 workspace 已注册）
- 删除或重命名既有 pub 项（同上，走 ADR）
- 跳过测试直接提 PR

🟡 **谨慎**：
- **条件激活/优雅降级**：链 RPC 不可用时，`RpcRegistry` 注销 adapter，业务侧须能降级（如禁用该链签名/凭证校验，或 fallback 远程只读节点）
- **WC Relay 默认公共可切自托管**（§9.1#13）：配置项 `wc_relay_url`，默认公共 relay，可切换自托管
- **Ordinals 自托管 ord index**（§9.1#12）：EVM 直查 RPC，Ordinals 优先自托管 ord index，外部 fallback 补充
- **多钱包兼容测试**：不同钱包（MetaMask/Trust Wallet/Unisat 等）的 WC v2 兼容性须测试覆盖
- 引入新第三方 crate 须经 ReviewAgent 评估维护性/安全

## 10. 示例工作流

> 以"实现 `ChainAdapter`（BTC + EVM 双链签名验证）"为例：

1. **开工**：读 `PROGRESS.md`（恢复上下文）+ `TASKS.md`（取任务）+ 本规格书 §3/§4
2. **读契约**：读 `crates/os-wallet/src/chain.rs` 的 `ChainAdapter` trait + `CredentialSpec`/`SignatureAlgorithm` 模型 + 相关 ADR
3. **切分支**：`git checkout agent/wallet-agent`；为新任务建子分支 `agent/wallet-agent/chain-adapter-btc-evm`
4. **实现**：创建 `BitcoinAdapter`（rust-bitcoin，BIP-322/Schnorr）+ `EvmAdapter`（alloy，EIP-191/712），分别 `impl ChainAdapter`；先骨架（verify_signature → query_balance → query_credential → chain_kind）后填充
5. **测试**：写单元测试（已知向量验签、余额/凭证查询、降级）；`cargo test -p os-wallet`
6. **提 PR**：推到远程，PR 标题 `[wallet-agent] chain-adapter-btc-evm`，描述含 DoD 勾选状态 + 多钱包兼容测试说明
7. **响应评审**：按 ReviewAgent 意见修订；契约变更触发 ADR + 会签（guest/im）
8. **更新进度**：合并后更新 `PROGRESS.md` + `TASKS.md`

## 11. 启动 prompt 模板

> 复制以下 prompt 启动本 agent 会话（替换 `<...>`）：

```
你是 OS 系统的 Wallet Agent（agent_id: wallet-agent）。
你的规格书在 OS_System/docs/agents/wallet-agent.md——请先完整读取它。
协作约定在 OS_System/docs/agents/_conventions.md——也请读取。
你拥有的契约在 OS_System/crates/os-wallet/src/*.rs。

本次任务：<具体任务描述，或"按 TASKS.md 取下一个任务">

开工前必读：
1. OS_System/docs/agents/wallet-agent.md（你的规格）
2. OS_System/docs/agents/_conventions.md（协作约定）
3. OS_System/docs/agents/wallet-agent/PROGRESS.md（你的进度，恢复上下文）
4. OS_System/docs/agents/wallet-agent/TASKS.md（你的任务队列）
5. 你拥有的 crate 的 src/*.rs（契约：connector/chain/registry/model）
6. 相关 ADR（OS_System/docs/adr/）

完成后：更新 PROGRESS.md + TASKS.md，按 DoD 自检，提 PR。
不得修改其他 agent 的 crate；不得破坏 trait 签名（走 ADR）。
特殊注意：条件激活（RpcRegistry 驱动 ChainAdapter 注册，不可用降级）；WC Relay 默认公共可切自托管（§9.1#13）；Ordinals 自托管 ord index（§9.1#12）；多钱包兼容测试。
```

## 12. 恢复协议

> 会话重启后（上下文丢失），按以下顺序恢复：

1. 读 `OS_System/docs/agents/wallet-agent.md`（本规格书，重识身份）
2. 读 `OS_System/docs/agents/_conventions.md`（重识协作规则）
3. 读 `OS_System/docs/agents/wallet-agent/PROGRESS.md`（**最关键**——你之前做到哪了）
4. 读 `OS_System/docs/agents/wallet-agent/TASKS.md`（下一个任务）
5. `git log agent/wallet-agent --oneline -20`（看最近提交，了解工作状态）
6. `cargo check -p os-wallet`（确认当前代码状态）
7. 继续未完成任务；若阻塞，在 PROGRESS.md 记录并在 TASKS.md 标 `blocked`

> **若 PROGRESS.md 不存在**（首次启动或丢失）：从规格书 §3 取契约清单（3 trait），从 `git log` 推断进度，重建 PROGRESS.md。
