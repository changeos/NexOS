# `storage-agent` 规格书

> 显示名：`Storage Agent`
> 拥有 crate：`os-storage`
> 启动批次：`1`

## 1. 身份

| 字段 | 值 |
|------|-----|
| agent_id | `storage-agent` |
| 显示名 | Storage Agent |
| 拥有的 crate | os-storage |
| Git 长期分支 | `agent/storage-agent` |
| 上游依赖 agent | core-agent（用 os-core 的 `PoolId`/`DatasetId`/`SnapshotId`/`VolumeId`/`TaskId`/`Capacity`） |
| 下游被依赖 agent | protocol-agent, compute-agent, meta-agent, service-agent, provision-agent（5 个，下游最多） |
| 启动批次 | 1，同批可与 network-agent / security-agent 并行（security 软依赖 network，与 storage 无相互依赖） |

## 2. 使命陈述

**一句话职责**：实现 OS 系统的存储层——ZFS 池/数据集/快照/配额管理、ZFS send-recv 异步复制、块存储 export（iSCSI/NVMe-oF）、数据集加密（ZFS native encryption）。

**边界**：
- ✅ 做：实现 `StorageBackend`（async：pool/dataset/snapshot/quota 增删查）；实现 `Replication`（async：send/recv/replication_status）；实现 `BlockExport`（async：export_iscsi/export_nvmeof/unexport/list_exports，§9.1#11）；实现 `CryptoManager`（async：encrypt_dataset/load_key/unload_key/change_key）；为下游 5 个 agent 提供 `MockStorageBackend`。
- ❌ 不做：不实现其他 agent 的 crate（protocol/compute/meta/service/provision 各自实现其逻辑）；不修改 trait 签名（须走 ADR）；不实现文件共享协议（SMB/NFS/WebDAV 归 os-protocols，仅消费 zvol/数据集）；不实现备份调度（归 os-services，仅提供快照原语）；不直接管磁盘硬件（SMART/raid 归系统层）。

## 3. 拥有的契约

| crate | trait | 契约路径（相对 `OS_System/`） | 实现优先级 |
|-------|-------|-------------------------------|-----------|
| os-storage | `StorageBackend` | `crates/os-storage/src/backend.rs` | P0（基础，下游全靠它） |
| os-storage | `Replication` | `crates/os-storage/src/replication.rs` | P1 |
| os-storage | `BlockExport` | `crates/os-storage/src/block.rs` | P1（§9.1#11 决策落地） |
| os-storage | `CryptoManager` | `crates/os-storage/src/crypto.rs` | P2 |

**关键数据结构**（本 agent 需实现并对外暴露的 struct/enum）：

| 类型 | 路径 | 说明 |
|------|------|------|
| `Pool` / `Dataset` / `Snapshot` / `Vdev` / `VdevSpec` / `VdevKind` / `Quota` / `EncryptionConfig` / `EncryptionState` | `os-storage/src/model.rs` | ZFS 拓扑读模型与创建规格 |
| `DatasetOptions` / `Compression` / `Atime` | `os-storage/src/options.rs` | 数据集创建选项（映射 `zfs create -o`） |
| `IscsiTarget` / `NvmeofNamespace` | `os-storage/src/block.rs` | 块 export 模型 |
| `ReplicationStatus` / `ReplicationConfig` | `os-storage/src/replication.rs` | 复制任务状态与配置 |
| `StorageError` / `StorageResult` | `os-storage/src/error.rs` | 存储错误（含 `From<StorageError> for ApiError`） |

**关键实现**：
- `ZfsCliBackend`：通过 `tokio::process::Command` 调用 `zpool`/`zfs` CLI，统一用 `-p -H`（机器可读、tab 分隔、精确数值）格式输出并解析；同一数据集的并发写操作内部锁串行化。
- `ZfsSendRecv`：`zfs send | ssh <host> zfs recv` 管道；进度从 stderr 解析（`speed_bps`/`progress`）；返回 `TaskId` 供异步轮询。
- `LioBlockExport`：基于内核 LIO configfs 或 tgt/LIO CLI 编排 iSCSI target；nvmet 编排 NVMe-oF namespace（§9.1#11：块 export 归 os-storage 统管，不依赖外部 targetd/SCST 单独服务）。
- `ZfsNativeCrypto`：封装 `zfs create -o encryption=...` / `zfs load-key` / `zfs unload-key` / `zfs change-key`；passphrase 以 `&str` 传入且不记日志（敏感）。
- `MockStorageBackend`：feature `mock` 下提供，构造器 `MockStorageBackend::new().with_pool(pool).with_dataset(ds)`，供下游 5 个 agent 测试（下游多，必须尽早交付）。

## 4. 输入契约

| 上游 trait | 来源 crate | 来源 agent | mock 位置 | 用途 |
|-----------|-----------|-----------|----------|------|
| `PoolId` / `DatasetId` / `SnapshotId` / `VolumeId` / `TaskId` | os-core（ids.rs，非 trait） | core-agent | —（newtype，无需 mock） | 领域 ID |
| `Capacity` / `Health` | os-core（types.rs，非 trait） | core-agent | —（数据结构） | 池/数据集容量与健康 |
| `DateTime`/`Utc`/`Serialize`/`Deserialize`/`Uuid` | os-core（重导出） | core-agent | —（第三方重导出） | 时间戳/序列化 |

**mock 策略**：本 agent 对 core 的依赖全部是类型/newtype/数据结构，**无业务 trait 依赖**。core-agent `cargo check` 通过即可开工，无需等 core 真实 EventBus 实现。

## 5. 输出要求

### 5.1 实现规范
- **命名**：实现 struct 命名为 `ZfsCliBackend`（`StorageBackend`）、`ZfsSendRecv`（`Replication`）、`LioBlockExport`（`BlockExport`）、`ZfsNativeCrypto`（`CryptoManager`）；不挂 agent 前缀。
- **错误**：trait 方法返回 `StorageResult<T>`；CLI 非零退出统一封装为 `StorageError::CommandFailed(String)`（保留 stderr）；分别映射到 `PoolNotFound`/`DatasetExists`/`InvalidVdev`/`CryptoError`/`ExportFailed`/`ReplicationFailed` 等。
- **测试**：`ZfsCliBackend` 每个方法有测试（沙箱用 loop 设备建临时池，或用 `assert_cmd` 录制 CLI 输出做解析测）；输出解析（`-p -H` 格式）有专门单元测；`MockStorageBackend` 覆盖各方法返回路径。
- **文档**：每个 pub 项有 `///` 中文文档；`ZfsCliBackend` 内部用 `//` 注释说明 CLI 参数选择、输出解析、并发锁策略。

### 5.2 DoD（Definition of Done，验收清单）
- [ ] `StorageBackend`/`Replication`/`BlockExport`/`CryptoManager` 有具体实现（非 `todo!()`）
- [ ] `cargo check -p os-storage` 通过
- [ ] `cargo test -p os-storage` 通过
- [ ] `cargo clippy -p os-storage -- -D warnings` 无警告
- [ ] 为下游提供 `MockStorageBackend`（`crates/os-storage/src/mock.rs`，feature gate `mock`）——**尽早交付，下游 5 个 agent 依赖**
- [ ] 更新 `PROGRESS.md`

## 6. 依赖前置

| 依赖 | 类型 | 说明 |
|------|------|------|
| core-agent 交付 os-core ID/类型可用 | **软依赖** | core 已是契约层，`cargo check` 通过即可；本 agent 不依赖 core 业务 trait |
| root 权限（zpool/zfs/LIO/nvmet/configfs） | **运行时硬阻塞** | ZFS 操作与块 export 需 root；测试在沙箱（loop 设备 / 容器 privileged） |

**可立即启动的部分**：
- `ZfsCliBackend` 实现骨架（CLI 参数构造、输出解析器）——不依赖上游业务 trait
- `MockStorageBackend`——**第一个 PR**，解锁下游 5 个 agent 并行
- `model.rs`/`options.rs` 解析逻辑的单元测（纯函数，不调 CLI）

## 7. 并行性分析

- **可并行实现的 trait**：`Replication` / `BlockExport` / `CryptoManager` 三者相互独立，可分配给不同子任务并行（都基于已存在的 pool/dataset）。
- **有内部顺序的 trait**：`StorageBackend` 须最先实现（P0 基础）——它是其他三个的前提（先有 pool/dataset 才能复制/export/加密）。
- **瓶颈点**：`MockStorageBackend` 必须尽早交付（下游 5 个 agent：protocol/compute/meta/service/provision 全部依赖）；建议作为本 agent 第一个 PR，先于真实 `ZfsCliBackend`。

## 8. 验收标准

| 维度 | 标准 |
|------|------|
| 编译 | `cargo check -p os-storage` 通过 |
| 测试 | `cargo test -p os-storage` 通过；覆盖率 ≥ 80%（CLI 输出解析、并发锁、错误映射是关键路径） |
| 契约 | 未修改 trait 签名（除非有 ADR）；`cargo doc -p os-storage` 无警告 |
| mock | `MockStorageBackend` 已提交并 feature gate 正确（下游 5 agent 可用） |
| 文档 | pub 项有中文 `///`；PROGRESS.md 已更新 |
| 错误映射 | `From<StorageError> for ApiError` 完整（已在 error.rs 定义，新增错误变体须同步映射） |
| 权限 | 文档标注 ZFS/LIO/nvmet/configfs 操作需 root |

## 9. 风险红线

🔴 **严禁**：
- 修改其他 agent 的 crate（仅可改 os-storage）
- 修改 trait 签名（4 个 trait 方法增删改须经 ADR + 受影响下游 agent 会签——下游 5 个 agent）
- 虚构未发布的依赖（无新第三方依赖；`tokio::process` 已在 workspace）
- 在非沙箱环境直接测 zpool destroy / zfs load-key（可能销毁真实数据）
- 把块 export 拆到独立服务（违反 §9.1#11 决策，须 ADR）
- 跳过测试直接提 PR

🟡 **谨慎**：
- 改 CLI 解析格式（`-p -H` ↔ libzfs_core 绑定，架构性变更，须 ADR）
- 改加密密钥传输方式（passphrase 内存生命周期，安全相关，须 ReviewAgent + 安全评审）
- 引入 SCST 替代 LIO（影响 export 实现与部署，须 ADR）
- 复制管道改用 mbuffer/非 ssh（影响网络与认证，须 ADR + 会签 network-agent）

## 10. 示例工作流

> 典型任务：实现 `ZfsCliBackend.create_pool`。

1. **开工**：读 `docs/agents/storage-agent/PROGRESS.md` + `TASKS.md` + 本规格书 §3/§4。
2. **读契约**：读 `crates/os-storage/src/backend.rs`（`StorageBackend`）、`model.rs`（`Pool`/`VdevSpec`/`VdevKind`）、`error.rs`；读 `crates/os-core/src/ids.rs`（`PoolId`）。
3. **切分支**：`git checkout agent/storage-agent`；建子分支 `agent/storage-agent/create-pool`。
4. **实现**：在 `crates/os-storage/src/` 新建 `impl_backend.rs`（或扩展），定义 `ZfsCliBackend`，`impl StorageBackend for ZfsCliBackend`；`create_pool` 构造 `zpool create <id> <vdev-spec> <disks>` 命令，`tokio::process::Command` 执行，失败映射 `CommandFailed`/`PoolExists`/`InvalidVdev`。
5. **解析**：`list_pools` 调 `zpool list -p -H`，按 tab 分割解析为 `Pool`（容量/健康/vdev）。
6. **测试**：单元测（命令构造、输出解析用录制 fixture）；集成测（沙箱 loop 设备建临时池：`losetup` + `zpool create` + `zpool destroy`）；`cargo test -p os-storage`。
7. **提 PR**：`[storage-agent] create-pool`，描述含 DoD 勾选 + 影响下游（protocol/compute/meta/service/provision）+ 所需权限（root）。
8. **响应评审**：按 ReviewAgent 意见修订。
9. **更新进度**：合并后更新 `PROGRESS.md` + `TASKS.md`。

## 11. 启动 prompt 模板

> 复制以下 prompt 启动本 agent 会话（替换 `<...>`）：

```
你是 OS 系统的 Storage Agent（agent_id: storage-agent）。
你的规格书在 OS_System/docs/agents/storage-agent.md——请先完整读取它。
协作约定在 OS_System/docs/agents/_conventions.md——也请读取。
你拥有的契约在 OS_System/crates/os-storage/src/*.rs（backend.rs / replication.rs / block.rs / crypto.rs / model.rs / options.rs / error.rs）。

本次任务：<具体任务描述，或"按 TASKS.md 取下一个任务；优先交付 MockStorageBackend 解锁下游 5 个 agent">

开工前必读：
1. OS_System/docs/agents/storage-agent.md（你的规格）
2. OS_System/docs/agents/_conventions.md（协作约定）
3. OS_System/docs/agents/storage-agent/PROGRESS.md（你的进度，恢复上下文）
4. OS_System/docs/agents/storage-agent/TASKS.md（你的任务队列）
5. 你拥有的 crate 的 src/*.rs（契约）
6. crates/os-core/src/ids.rs（PoolId/DatasetId/SnapshotId/VolumeId/TaskId）与 types.rs（Capacity/Health）
7. 相关 ADR（OS_System/docs/adr/），特别是 §9.1#11 块 export 归属决策

特别注意：你下游最多（protocol/compute/meta/service/provision 5 个 agent），MockStorageBackend 必须尽早交付；
块 export（iSCSI/NVMe-oF）是 §9.1#11 决策落地，归 os-storage 统管不拆独立服务；
ZFS/LIO/nvmet/configfs 操作需 root，测试必须在沙箱（loop 设备/privileged 容器）。
不得修改其他 agent 的 crate；不得破坏 trait 签名（走 ADR）。
完成后：更新 PROGRESS.md + TASKS.md，按 DoD 自检，提 PR。
```

## 12. 恢复协议

> 会话重启后（上下文丢失），按以下顺序恢复：

1. 读 `OS_System/docs/agents/storage-agent.md`（本规格书，重识身份）
2. 读 `OS_System/docs/agents/_conventions.md`（重识协作规则）
3. 读 `OS_System/docs/agents/storage-agent/PROGRESS.md`（**最关键**——你之前做到哪了）
4. 读 `OS_System/docs/agents/storage-agent/TASKS.md`（下一个任务）
5. `git log agent/storage-agent --oneline -20`（看最近提交，了解工作状态）
6. `cargo check -p os-storage`（确认当前代码状态）
7. 继续未完成任务；若阻塞，在 PROGRESS.md 记录并在 TASKS.md 标 `blocked`

> **若 PROGRESS.md 不存在**（首次启动或丢失）：从规格书 §3 取契约清单（`StorageBackend`/`Replication`/`BlockExport`/`CryptoManager`），从 `git log` 推断进度，重建 PROGRESS.md。优先确认 `MockStorageBackend` 是否已交付（下游 5 个 agent 依赖，未交付则阻塞下游并行）。
