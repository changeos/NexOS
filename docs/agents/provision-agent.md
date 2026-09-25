# `provision-agent` 规格书

> 显示名：`Provision Agent`
> 拥有 crate：`os-provision`
> 启动批次：`3`

## 1. 身份

| 字段 | 值 |
|------|-----|
| agent_id | `provision-agent` |
| 显示名 | Provision Agent |
| 拥有的 crate | os-provision |
| Git 长期分支 | `agent/provision-agent` |
| 上游依赖 agent | `network-agent`（PxeServer 提供 PXE 引导能力）、`discover-agent`（首次组网发现 peer）、`meta-agent`（KV 存迁移断点锚点）、`storage-agent`（ZFS send/recv） |
| 下游被依赖 agent | 无（provision 是终端编排方，不被其他 owner agent 依赖） |
| 启动批次 | `3`，同批可与 discover-agent / guest-agent / update-agent / backup-agent / monitor-agent / media-agent / files-agent / devtools-agent / power-agent 并行 |

## 2. 使命陈述

**一句话职责**：实现 OS 系统的分发与迁移——PXE 自举裸机目标节点（阶段1：分区/装基础系统/建 ZFS 池/拉起 osd 空壳），阶段化迁移源节点内容到目标节点（配置/共享/用户定义走迁移包，数据集走 ZFS send/recv，密钥按 §3.19 排除清单不传输），支持断点续传。

**边界**：
- ✅ 做：实现 `Provisioner`（boot_via_pxe/init_system/status，编排 os-network PxeServer + os-storage 建池 + osd 空壳拉起）；实现 `MigrationEngine`（plan/execute/resume/status，编排 ZFS send/recv + 配置包导入导出 + 排除清单）；为下游（若有，如 cli/api 暴露命令）提供 mock。
- ❌ 不做：不实现其他 agent 的 crate（network 的 PXE / storage 的 ZFS / meta 的 KV 各自实现，本 agent 仅编排）；不修改 trait 签名（破坏性变更须经 ADR）；不实现 ISO 打包（已拆给 iso-agent，构建期）；不实现 OTA 升级/回滚（已拆给 update-agent，运行时升级）；不下沉 ZFS 命令本身（调 os-storage 的 Replication）；不预置明文密码（§3.19：root_password_hash 仅为临时占位，首启强制重设）。

## 3. 拥有的契约

| crate | trait | 契约路径（相对 `OS_System/`） | 实现优先级 |
|-------|-------|-------------------------------|-----------|
| os-provision | `Provisioner` | `crates/os-provision/src/provision.rs` | P0（PXE 自举是部署入口） |
| os-provision | `MigrationEngine` | `crates/os-provision/src/migration.rs` | P1（迁移依赖 storage 复制能力就绪） |

**关键数据结构**（本 agent 需实现并对外暴露的 struct/enum）：

| 类型 | 路径 | 说明 |
|------|------|------|
| `ProvisionTarget` / `ProvisionConfig` / `ProvisionStatus` | `os-provision/src/provision.rs` | 自举目标节点 / 配置（base_image + root_password_hash + zfs_pool_disks + network_config）/ 阶段状态机（Booting/Installing/FormingPool/Ready{node_id}/Failed{reason}） |
| `MigrationPlan` / `MigrationStatus` | `os-provision/src/migration.rs` | 迁移计划（source/target/datasets/exclude_keys/resume_point）/ 迁移状态机（Pending/Transferring{progress,current_dataset}/Verifying/Completed/Failed{reason}） |
| `ProvisionError` / `ProvisionResult` | `os-provision/src/error.rs` | 错误（PxeBootFailed/InitFailed/MigrationFailed/TargetUnreachable/InvalidConfig/Internal；`From<ProvisionError> for ApiError` 已定义） |

**关键实现**：
- `PxeProvisioner`：编排 `os-network::PxeServer` 引导 `ProvisionTarget.mac` 所指裸机；阶段1 `init_system` 串行执行分区→装基础系统（`ProvisionConfig.base_image`）→建 ZFS 池（`zfs_pool_disks`）→拉起 osd 空壳；产出新 `NodeId`（`Ready{node_id}`）。
- `ZfsMigrationEngine`：`plan` 扫描源数据集并生成 `exclude_keys`（§3.19 统一排除清单：JWT 密钥/TOTP secret/钱包密钥/数据库密码）；`execute` 配置/共享/用户定义走迁移包（结构化导出/导入），数据集走 `os-storage::Replication` 的 ZFS send/recv（增量）；命中排除清单的项不传输，目标节点需重新生成或独立导入；`resume` 基于 `resume_point` 锚点续传（锚点存于 os-meta KV）。
- `MockProvisioner` / `MockMigrationEngine`：feature `mock` 下提供，构造器设置预期状态，供 cli/api 等下游测试。

## 4. 输入契约

> 本 agent 依赖的上游 trait，**必须经 `Box<dyn Trait>` 或泛型注入**，不得耦合具体实现。

| 上游 trait | 来源 crate | 来源 agent | mock 位置 | 用途 |
|-----------|-----------|-----------|----------|------|
| `PxeServer` | os-network | network-agent | `crates/os-network/src/mock.rs` | PXE 引导裸机（boot_via_pxe 编排它） |
| `Replication` | os-storage | storage-agent | `crates/os-storage/src/mock.rs` | ZFS send/recv 数据集迁移（execute 编排它） |
| `MetaStore`（KV） | os-meta | meta-agent | `crates/os-meta/src/mock.rs` | 存迁移断点锚点（resume_point）与任务状态 |
| `Discovery`（可选，首次组网） | os-discover | discover-agent | `crates/os-discover/src/mock.rs` | 首次组网时发现目标 peer 节点 |
| `NodeId` / `DatasetId` / `TaskId`（数据类型） | os-core | core-agent | — | 领域 ID |

**mock 策略**：上游 4 个 trait 的 mock 就绪前，本 agent 用本地临时 stub 跑通编排骨架（返回预设 TaskId/状态）；mock 就绪后切换为注入真实/mock 依赖。

## 5. 输出要求

### 5.1 实现规范
- **命名**：实现 struct 命名为 `PxeProvisioner`（`Provisioner`）、`ZfsMigrationEngine`（`MigrationEngine`），不挂 agent 前缀。
- **错误**：trait 方法返回 `ProvisionResult<T>`；上游错误（PxeServer/Replication/MetaStore）经 `map_err` 映射到 `ProvisionError`（PxeBootFailed/MigrationFailed/Internal）；CLI 非零退出保留 stderr。
- **测试**：`Provisioner`/`MigrationEngine` 编排逻辑有单元测（用 mock 上游验证调用顺序/参数/状态流转）；ZFS send/recv 的进度解析与排除清单匹配有专门单测；`resume` 断点续传路径有测试覆盖；mock 实现覆盖各返回路径。
- **文档**：每个 pub 项有 `///` 中文文档；编排实现补 `//` 注释说明阶段顺序、排除清单匹配规则、断点锚点存储约定。

### 5.2 DoD（Definition of Done，验收清单）
- [ ] `Provisioner` / `MigrationEngine` 有具体实现（非 `todo!()`）
- [ ] `cargo check -p os-provision` 通过
- [ ] `cargo test -p os-provision` 通过
- [ ] `cargo clippy -p os-provision -- -D warnings` 无警告
- [ ] 为下游提供 `MockProvisioner` / `MockMigrationEngine`（`crates/os-provision/src/mock.rs`，feature gate `mock`）
- [ ] 更新 `PROGRESS.md`

## 6. 依赖前置

| 依赖 | 类型 | 说明 |
|------|------|------|
| `network-agent` 交付 `PxeServer` mock | **硬阻塞** | boot_via_pxe 依赖 PXE 能力，无 mock 无法跑通编排 |
| `storage-agent` 交付 `Replication` mock | **硬阻塞** | 数据集迁移依赖 ZFS send/recv 抽象 |
| `meta-agent` 交付 `MetaStore` mock | **软依赖** | 断点锚点存储可用临时内存 stub 替代，真实 KV 就绪后切换 |
| `discover-agent` 交付 `Discovery` mock | **软依赖** | 首次组网场景，非核心路径，可先用 stub |
| `core-agent` 交付 os-core ID 类型 | **软依赖** | 契约层，`cargo check` 通过即可 |
| root 权限（PXE/ZFS/分区） | **运行时硬阻塞** | 自举与迁移操作需 root；测试在沙箱（容器 privileged / VM） |

**可立即启动的部分**：
- 数据结构定义（`ProvisionTarget`/`ProvisionConfig`/`MigrationPlan` 等已在契约层）
- 排除清单匹配逻辑（§3.19 统一清单的纯函数匹配器，不依赖上游）
- `MockProvisioner` / `MockMigrationEngine`——**第一个 PR**，解锁下游并行
- 编排骨架（注入 stub 上游，跑通状态机流转）

## 7. 并行性分析

- **可并行实现的 trait**：`Provisioner` 与 `MigrationEngine` 二者相对独立（一个管阶段1自举，一个管阶段2迁移），可分配给两个子任务并行。
- **有内部顺序的 trait**：业务上 `Provisioner.init_system`（阶段1）须先于 `MigrationEngine.execute`（阶段2）——先有目标节点才能迁数据；但二者是不同任务，实现上无代码依赖，可并行开发，集成时再串联顺序。
- **瓶颈点**：ZFS send/recv 迁移是串行关键路径（大数据集传输耗时，断点续传正确性要求高）；PXE 自举依赖真实裸机/VM 环境，沙箱测试覆盖度受限。

## 8. 验收标准

| 维度 | 标准 |
|------|------|
| 编译 | `cargo check -p os-provision` 通过 |
| 测试 | `cargo test -p os-provision` 通过；覆盖率 ≥ 80%（编排顺序、排除清单匹配、断点续传、状态流转是关键路径） |
| 契约 | 未修改 trait 签名（除非有 ADR）；`cargo doc -p os-provision` 无警告 |
| mock | `MockProvisioner` / `MockMigrationEngine` 已提交并 feature gate 正确 |
| 文档 | pub 项有中文 `///`；PROGRESS.md 已更新 |
| 错误映射 | `From<ProvisionError> for ApiError` 完整（已在 error.rs 定义，新增错误变体须同步映射） |
| 安全 | 文档标注密钥排除清单（§3.19）；root_password_hash 不记明文日志 |

## 9. 风险红线

🔴 **严禁**：
- 修改其他 agent 的 crate（仅可改 os-provision）
- 修改 trait 签名（`Provisioner`/`MigrationEngine` 方法增删改须经 ADR + 受影响 agent 会签）
- 虚构未发布的依赖（无新第三方依赖；tokio::process 已在 workspace）
- 在迁移包中包含密钥/密码（违反 §3.19 统一排除清单，安全红线——exclude_keys 必须覆盖 JWT 密钥/TOTP secret/钱包密钥/数据库密码）
- 预置明文 root 密码（root_password_hash 仅为哈希占位，首启强制重设）
- 把 ISO 打包或 OTA 升级塞回本 crate（已拆给 iso-agent / update-agent，须 ADR 才能合并）
- 跳过测试直接提 PR

🟡 **谨慎**：
- 改 ZFS send/recv 的传输方式（ssh 管道 ↔ mbuffer/压缩，影响网络与认证，须 ADR + 会签 network-agent）
- 改断点锚点存储介质（os-meta KV ↔ 本地文件，影响 HA 一致性，须 ADR + 会签 meta-agent）
- 改排除清单匹配规则（安全相关，须 ReviewAgent + 安全评审）
- root 操作（PXE/ZFS/分区）须在沙箱测试，禁止直连生产裸机

## 10. 示例工作流

> 典型任务：实现 `MigrationEngine.execute`（阶段2 数据迁移）。

1. **开工**：读 `docs/agents/provision-agent/PROGRESS.md` + `TASKS.md` + 本规格书 §3/§4。
2. **读契约**：读 `crates/os-provision/src/migration.rs`（`MigrationEngine`/`MigrationPlan`/`MigrationStatus`）、`error.rs`；读 `crates/os-storage/src/replication.rs`（`Replication`，迁移数据集调它）；读 §3.19 密钥排除清单 ADR。
3. **切分支**：`git checkout agent/provision-agent`；建子分支 `agent/provision-agent/migration-execute`。
4. **实现**：在 `crates/os-provision/src/` 新建 `impl_migration.rs`（或扩展），定义 `ZfsMigrationEngine`，`impl MigrationEngine for ZfsMigrationEngine`；`execute` 先导出配置/共享/用户定义迁移包（按 `exclude_keys` 过滤），再逐数据集调 `Replication` 做 ZFS send/recv，每完成一个数据集更新 `resume_point` 锚点（存 os-meta KV）并推进 `Transferring{progress,current_dataset}`。
5. **测试**：单元测（注入 mock 上游验证调用顺序、排除清单过滤、断点续传路径、状态流转）；`cargo test -p os-provision`。
6. **提 PR**：`[provision-agent] migration-execute`，描述含 DoD 勾选 + §3.19 排除清单覆盖说明 + 所需依赖（storage/network/meta）。
7. **响应评审**：按 ReviewAgent 意见修订；契约变更触发 ADR + 会签。
8. **更新进度**：合并后更新 `PROGRESS.md` + `TASKS.md`。

## 11. 启动 prompt 模板

> 复制以下 prompt 启动本 agent 会话（替换 `<...>`）：

```
你是 OS 系统的 Provision Agent（agent_id: provision-agent）。
你的规格书在 OS_System/docs/agents/provision-agent.md——请先完整读取它。
协作约定在 OS_System/docs/agents/_conventions.md——也请读取。
你拥有的契约在 OS_System/crates/os-provision/src/*.rs（provision.rs / migration.rs / error.rs）。

本次任务：<具体任务描述，或"按 TASKS.md 取下一个任务；优先交付 MockProvisioner/MockMigrationEngine 解锁下游">

开工前必读：
1. OS_System/docs/agents/provision-agent.md（你的规格）
2. OS_System/docs/agents/_conventions.md（协作约定）
3. OS_System/docs/agents/provision-agent/PROGRESS.md（你的进度，恢复上下文）
4. OS_System/docs/agents/provision-agent/TASKS.md（你的任务队列）
5. 你拥有的 crate 的 src/*.rs（契约）
6. 上游契约：crates/os-network/src/pxe.rs（PxeServer）、crates/os-storage/src/replication.rs（Replication）、crates/os-meta/src/（MetaStore KV）
7. 相关 ADR（OS_System/docs/adr/），特别是 §3.19 密钥排除清单决策

特别注意：PXE 自举（阶段1）+ 阶段化迁移（阶段2，ZFS send/recv + 排除清单）；
密钥/密码按 §3.19 统一排除清单绝不随迁移包传输（JWT 密钥/TOTP secret/钱包密钥/数据库密码）；
root_password_hash 仅为哈希占位，首启强制重设；
ISO 打包归 iso-agent，OTA 升级归 update-agent，本 crate 不含；
断点续传锚点存 os-meta KV；PXE/ZFS/分区操作需 root，测试在沙箱。
不得修改其他 agent 的 crate；不得破坏 trait 签名（走 ADR）。
完成后：更新 PROGRESS.md + TASKS.md，按 DoD 自检，提 PR。
```

## 12. 恢复协议

> 会话重启后（上下文丢失），按以下顺序恢复：

1. 读 `OS_System/docs/agents/provision-agent.md`（本规格书，重识身份）
2. 读 `OS_System/docs/agents/_conventions.md`（重识协作规则）
3. 读 `OS_System/docs/agents/provision-agent/PROGRESS.md`（**最关键**——你之前做到哪了）
4. 读 `OS_System/docs/agents/provision-agent/TASKS.md`（下一个任务）
5. `git log agent/provision-agent --oneline -20`（看最近提交，了解工作状态）
6. `cargo check -p os-provision`（确认当前代码状态）
7. 继续未完成任务；若阻塞，在 PROGRESS.md 记录并在 TASKS.md 标 `blocked`

> **若 PROGRESS.md 不存在**（首次启动或丢失）：从规格书 §3 取契约清单（`Provisioner`/`MigrationEngine`），从 `git log` 推断进度，重建 PROGRESS.md。优先确认 `MockProvisioner`/`MockMigrationEngine` 是否已交付（未交付则阻塞下游并行）；确认排除清单（§3.19）覆盖是否完整。
