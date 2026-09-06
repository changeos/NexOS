# os-storage

> 存储层 · ZFS 池/数据集/快照/配额/加密/复制 + 块存储 export · owner：storage-agent

OS 存储管理 crate：用 Rust 编排 `zpool` / `zfs` CLI 完成池、数据集、快照与配额
管理，覆盖原生加密、send-recv 异步复制与 iSCSI / NVMe-oF 块存储导出
（规划文档 §3.2 / §9.1#11）。

## 核心能力

- **池/数据集/快照/配额**（`backend`）：`StorageBackend` trait（原生 async fn in
  trait）；默认实现 `ZfsCliBackend` 经 `tokio::process::Command` 调 `zpool`/`zfs`，
  统一 `-p -H` 机器可读格式；命令构造（`cli`）与输出解析（`model`）均为纯函数，
  无 ZFS 环境也可单测。
- **块存储 export**（`block`）：`BlockExport` trait——zvol → iSCSI target（LIO /
  configfs）/ NVMe-oF namespace（nvmet）；默认实现 `LioBlockExport`。
- **数据集加密**（`crypto`）：`CryptoManager` trait——native encryption 的
  load / unload / change-key；默认实现 `ZfsNativeCrypto`。
- **异步复制**（`replication`）：`Replication` trait——`zfs send | zfs recv`
  子进程管线，带 `ReplicationStatus` 进度上报（跨节点/跨集群灾备）；
  默认实现 `ZfsSendRecv`。
- **可测性**：`CommandRunner` 抽象（默认 `TokioCommandRunner`，可注入 fake
  runner）+ `parse_zpool_status` 输出解析 + `mock` feature 的
  `MockStorageBackend`（供 protocol/compute/meta/service/provision 测试注入）。

## 架构位置

**依赖**（上游）：`os-core`（newtype ID：`PoolId`/`DatasetId`/`SnapshotId`/
`VolumeId`/`TaskId`，`CommandOutput`）、`os-common`（`From<StorageError> for
ApiError`）。

**被用**（下游）：os-api、os-compute、osd、os-services、os-protocols、
os-provision。

## 独立使用

- **仓库外引用**：`os-storage = { git = "http://ub2604:8080/git/nexos.git" }`。
- **权限**：读操作（list/get）普通用户可执行；写操作（create/destroy/snapshot/
  set/load-key）与块 export（configfs/cryptsetup）需 **root**。
- **关键接口**：`StorageBackend` / `BlockExport` / `CryptoManager` /
  `Replication` 四 trait（单实现为主，不做 `Box<dyn>` 派发，下游以具体类型注入，
  ADR-COMPAT-001）；错误统一 `StorageError::CommandFailed`（保留 stderr）。
- **feature**：`mock`（默认关）——`MockStorageBackend` 测试桩。

## 测试

```bash
cargo test -p os-storage
```

纯函数（命令构造/解析）+ fake runner 单测默认跑；真实 ZFS / LIO 环境测在
`tests/*_real.rs` 中以 `#[ignore]` 标记，需真机手动 `-- --ignored`。
