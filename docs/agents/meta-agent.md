# `meta-agent` 规格书

> 显示名：`Meta Agent`
> 拥有 crate：`os-meta`
> 启动批次：`2`

## 1. 身份

| 字段 | 值 |
|------|-----|
| agent_id | `meta-agent` |
| 显示名 | Meta Agent |
| 拥有的 crate | `os-meta` |
| Git 长期分支 | `agent/meta-agent` |
| 上游依赖 agent | `core-agent`、`network-agent`（VIP 用 `IpCidr`） |
| 下游被依赖 agent | `discover-agent`（联邦）、`provision-agent`（入集群）、`api-agent`（多节点视图） |
| 启动批次 | `2`，同批可与 protocol-agent、compute-agent、wallet-agent 并行 |

## 2. 使命陈述

**一句话职责**：为 OS 系统提供 HA 集群元数据与协调核心——openraft 共识（选主/日志复制/快照）、分布式 KV（强一致 + CAS 乐观锁）、故障转移编排（迁移 VM + 切 VIP + 提升副本）、VIP 管理、元数据 HA 复制（openraft 状态机内嵌 SQLite）。

**边界**：
- ✅ 做：实现 `os-meta` 全部 5 个 trait；封装 openraft；状态机内嵌 SQLite（MetaStore，§9.1#7）；ZFS 快照点对齐避免脑裂；VIP 编排 ip/网络命名空间
- ❌ 不做：不实现其他 agent 的 crate；不修改 trait 签名（破坏性变更须经 ADR）；不实现 compute 的 VM 迁移执行（meta 编排，compute 执行）；不实现 network 的接口配置（VIP 复用 `IpCidr` 类型，编排层自行处理地址绑定）

## 3. 拥有的契约

| crate | trait | 契约路径（相对 `OS_System/`） | 实现优先级 |
|-------|-------|-------------------------------|-----------|
| `os-meta` | `Consensus` | `crates/os-meta/src/consensus.rs` | P0（核心） |
| `os-meta` | `DistributedKv` | `crates/os-meta/src/kv.rs` | P0（核心） |
| `os-meta` | `MetaStore` | `crates/os-meta/src/meta_store.rs` | P1（依赖 Consensus，§9.1#7 决策核心） |
| `os-meta` | `FailoverOrchestrator` | `crates/os-meta/src/failover.rs` | P1（可并行） |
| `os-meta` | `VipManager` | `crates/os-meta/src/vip.rs` | P1（可并行） |

**关键数据结构**（本 agent 需实现并对外暴露的 struct/enum）：
- `ClusterConfig`、`ClusterState`（`Leader`/`Follower`/`Candidate`/`Offline`/`Standalone`）、`ClusterStatus`
- `KvEntry`（含 `version: u64`，CAS 乐观锁依据）
- `FailoverEvent`、`FailoverStatus`（`Pending`/`Running`/`Completed`/`Failed`/`Aborted`）
- `VipConfig`（`ip: IpCidr`）、`MetaSnapshot`（SQLite dump）
- 实现 struct：`OpenraftConsensus`（共识）、`OpenraftKv`（KV）、`SqliteMetaStore`（§9.1#7 状态机内嵌 SQLite）、`HaFailoverOrchestrator`（故障转移）、`NetlinkVipManager`（VIP）

## 4. 输入契约

> 本 agent 依赖的上游 trait，**必须经 `Box<dyn Trait>` 或泛型注入**，不得耦合具体实现。

| 上游 trait | 来源 crate | 来源 agent | mock 位置 | 用途 |
|-----------|-----------|-----------|----------|------|
| `NodeId`/`NodeInfo`/`NodeRole`/`VmId`/`TaskId`/`DateTime`（数据类型） | `os-core` | `core-agent` | — | 节点/VM/任务标识，集群元数据结构 |
| `IpCidr`（数据类型） | `os-network` | `network-agent` | — | VIP 的 `VipConfig.ip` 字段类型 |
| `VmManager`（VM 迁移，故障转移编排间接消费） | `os-compute` | `compute-agent` | `crates/os-compute/src/mock.rs` | `FailoverOrchestrator` 迁移 VM 的执行方（meta 编排，compute 执行） |

**mock 策略**：core/network 的数据类型属纯结构，先交付即可；compute 的 `VmManager` mock 就绪前，故障转移用 stub（记录迁移意图，不真实迁移）跑通。

## 5. 输出要求

### 5.1 实现规范
- **命名**：实现 struct 命名为 `<Tool>Consensus`/`<Tool>Orchestrator`/`<Tool>Store`/`<Tool>Manager`（如 `OpenraftConsensus`、`SqliteMetaStore`、`HaFailoverOrchestrator`、`NetlinkVipManager`），不挂 agent 前缀
- **错误**：实现方法返回 `Result<T, MetaError>`；内部错误映射到 `MetaError` 枚举（实现 `From<MetaError> for os_common::ApiError`）
- **测试**：每个公开方法有单元测试；共识选主/KV CAS/快照恢复/VIP 漂移需集成测（多节点用沙箱模拟）
- **文档**：每个 pub 项有 `///` 中文文档；openraft/SQLite/脑裂防护补 `//` 内联注释说明"为什么"

### 5.2 DoD（验收清单）
- [ ] 所有拥有的 trait 有具体实现（非 `todo!()`）
- [ ] `cargo check -p os-meta` 通过
- [ ] `cargo test -p os-meta` 通过
- [ ] `cargo clippy -p os-meta -- -D warnings` 无警告
- [ ] 为下游 agent 提供 mock 实现（`crates/os-meta/src/mock.rs`，feature gate `mock`）：`MockConsensus`/`MockDistributedKv`/`MockMetaStore`/`MockFailoverOrchestrator`/`MockVipManager`
- [ ] 更新 `PROGRESS.md`

## 6. 依赖前置

| 依赖 | 类型 | 说明 |
|------|------|------|
| `core-agent` 交付 `os-core` mock（NodeId/NodeInfo/NodeRole/VmId/TaskId） | **硬阻塞** | 本 agent 启动前必须有此 mock |
| `network-agent` 交付 `IpCidr` 类型 | **硬阻塞** | VIP 的 `VipConfig.ip` 字段类型依赖 |
| `compute-agent` 交付 `VmManager` mock | **软依赖** | 故障转移迁移 VM 依赖，可先用 stub 并行 |
| `compute-agent` 交付 `VmManager` 真实实现 | **软依赖** | 真实迁移就绪后切换 |

**可立即启动的部分**：`Consensus`/`DistributedKv` 的 openraft 封装（不依赖 compute）；`MetaStore` 的 SQLite 内嵌状态机（§9.1#7，独立）；VIP 管理的地址绑定逻辑骨架。

## 7. 并行性分析

- **可并行实现的 trait**：`Consensus` + `DistributedKv`（一组，核心，先行）与 `FailoverOrchestrator` + `VipManager`（一组）两组间可并行
- **有内部顺序的 trait**：`Consensus` + `DistributedKv` **须最先**（核心）；`MetaStore` **须后于** `Consensus`（MetaStore 是 openraft 状态机的持久化后端，apply_log 由状态机 apply 钩子调用）
- **瓶颈点**：`MetaStore`（§9.1#7 决策核心）是串行关键路径——openraft 内嵌 SQLite，写路径经 log 强一致复制 + apply 到各节点本地 SQLite，快照 = SQLite dump 随 openraft snapshot 流转
- **CAS 乐观锁**：`DistributedKv.cas`（`expected_version` 控制"仅创建"/"仅更新"）是无锁协调核心，版本不符返回 `CasConflict`

## 8. 验收标准

| 维度 | 标准 |
|------|------|
| 编译 | `cargo check -p os-meta` 通过 |
| 测试 | `cargo test -p os-meta` 通过；关键路径（共识选主、KV CAS、快照恢复、VIP 漂移、故障转移）覆盖率 ≥ 80% |
| 契约 | 未修改 trait 签名（除非有 ADR）；`cas` 乐观锁语义保持；`cargo doc` 无警告 |
| mock | 下游可用的 mock 已提交（5 个） |
| 文档 | pub 项有中文 `///`；PROGRESS.md 已更新 |

## 9. 风险红线

🔴 **严禁**：
- 修改其他 agent 的 crate（仅可改 `os-meta`）
- 修改 trait 签名（破坏性变更须经 ADR + 受影响 agent 会签）
- 虚构未发布的依赖（openraft/sqlite 须在 workspace 已注册）
- 删除或重命名既有 pub 项（同上，走 ADR）
- 跳过测试直接提 PR

🟡 **谨慎**：
- **脑裂防护**：ZFS 快照点须与 openraft snapshot 对齐，避免故障转移时数据不一致（脑裂）
- **强一致**：写操作经 leader 复制到 quorum 后才提交（线性一致）；非 leader 节点写操作返回 `NotLeader`
- **VIP 漂移**：`VipManager.assign` 须 ARP 广播通告漂移；VIP 已被其他节点持有时返回 `VipConflict`
- **故障转移异步**：`trigger_failover` 返回 `TaskId`（异步任务：迁移 VM + 切 VIP + 提升副本），上层轮询 `failover_status`
- **§9.1#7 决策核心**：MetaStore 是元数据 HA 复制的核心（openraft 内嵌 SQLite，强一致 + 快照），实现须严格遵循决策
- 引入新第三方 crate 须经 ReviewAgent 评估维护性/安全

## 10. 示例工作流

> 以"实现 `MetaStore`（§9.1#7 openraft 内嵌 SQLite）"为例：

1. **开工**：读 `PROGRESS.md`（恢复上下文）+ `TASKS.md`（取任务）+ 本规格书 §3/§4
2. **读契约**：读 `crates/os-meta/src/meta_store.rs` 的 `MetaStore` trait + `MetaSnapshot` 模型 + §9.1#7 相关 ADR + `consensus.rs`（状态机关联）
3. **切分支**：`git checkout agent/meta-agent`；为新任务建子分支 `agent/meta-agent/meta-store-sqlite`
4. **实现**：创建 `SqliteMetaStore` struct，`impl MetaStore for SqliteMetaStore`；先骨架（apply_log → snapshot → restore → query）后填充（SQLite 单库 + WAL，apply 钩子作用业务 JSON 命令，快照 = dump）
5. **测试**：写单元测试（log apply、快照 dump/restore、本地只读 query）；`cargo test -p os-meta`（多节点脑裂场景用沙箱模拟）
6. **提 PR**：推到远程，PR 标题 `[meta-agent] meta-store-sqlite`，描述含 DoD 勾选状态 + §9.1#7 决策对齐说明
7. **响应评审**：按 ReviewAgent 意见修订；契约变更触发 ADR + 会签（discover/provision/api）
8. **更新进度**：合并后更新 `PROGRESS.md` + `TASKS.md`

## 11. 启动 prompt 模板

> 复制以下 prompt 启动本 agent 会话（替换 `<...>`）：

```
你是 OS 系统的 Meta Agent（agent_id: meta-agent）。
你的规格书在 OS_System/docs/agents/meta-agent.md——请先完整读取它。
协作约定在 OS_System/docs/agents/_conventions.md——也请读取。
你拥有的契约在 OS_System/crates/os-meta/src/*.rs。

本次任务：<具体任务描述，或"按 TASKS.md 取下一个任务">

开工前必读：
1. OS_System/docs/agents/meta-agent.md（你的规格）
2. OS_System/docs/agents/_conventions.md（协作约定）
3. OS_System/docs/agents/meta-agent/PROGRESS.md（你的进度，恢复上下文）
4. OS_System/docs/agents/meta-agent/TASKS.md（你的任务队列）
5. 你拥有的 crate 的 src/*.rs（契约：consensus/kv/meta_store/failover/vip）
6. 相关 ADR（OS_System/docs/adr/，特别是 §9.1#7 MetaStore 决策）

完成后：更新 PROGRESS.md + TASKS.md，按 DoD 自检，提 PR。
不得修改其他 agent 的 crate；不得破坏 trait 签名（走 ADR）。
特殊注意：MetaStore 是 §9.1#7 决策核心（openraft 内嵌 SQLite，强一致+快照）；cas 乐观锁；Failover 迁移 VM（编排 compute）；ZFS 快照点对齐避免脑裂；VIP 漂移 ARP 通告。
优先级：Consensus + DistributedKv 先行（核心），MetaStore 后（依赖 Consensus）。
```

## 12. 恢复协议

> 会话重启后（上下文丢失），按以下顺序恢复：

1. 读 `OS_System/docs/agents/meta-agent.md`（本规格书，重识身份）
2. 读 `OS_System/docs/agents/_conventions.md`（重识协作规则）
3. 读 `OS_System/docs/agents/meta-agent/PROGRESS.md`（**最关键**——你之前做到哪了）
4. 读 `OS_System/docs/agents/meta-agent/TASKS.md`（下一个任务）
5. `git log agent/meta-agent --oneline -20`（看最近提交，了解工作状态）
6. `cargo check -p os-meta`（确认当前代码状态）
7. 继续未完成任务；若阻塞，在 PROGRESS.md 记录并在 TASKS.md 标 `blocked`

> **若 PROGRESS.md 不存在**（首次启动或丢失）：从规格书 §3 取契约清单（5 trait），从 `git log` 推断进度，重建 PROGRESS.md。
