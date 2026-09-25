# os-core

> 基础层 · 零内部依赖（workspace 根基 crate）· owner：CoreAgent（规划文档 §13.1）

OS 系统的基础层：领域 newtype ID、跨 crate 共享领域模型与节点内事件总线契约——
所有其他 os-* crate 的根基，本 crate 自身不依赖任何业务 crate。

## 核心能力

- **领域 newtype ID**（`ids`）：`PoolId` / `DatasetId` / `SnapshotId` / `VolumeId` /
  `TaskId` / `NodeId` 等统一 `Uuid` 包装，杜绝跨 crate 裸字符串 ID。
- **共享领域模型**（`types`）：`CommandOutput` / `ResourceQuota` 等跨 crate 值类型。
- **事件总线契约**（`eventbus`）：`EventBus` / `EventSubscriber` trait（按 `Topic`
  pub/sub，返回 `SubscriptionId`）+ `Event` / `Severity` 数据结构（规划文档 §9.1#9）。
- **默认实现**（`bus`）：`TokioBroadcastBus`——基于 `tokio::sync::broadcast`。
- **统一错误根**（`error`）：`CoreError` / `CoreResult`，其他 crate 经
  `From<CoreError>` 转换（如 `From<CoreError> for os_common::ApiError`）。
- **常用类型快捷入口**：重导出 `chrono::Utc` / `serde::{Serialize, Deserialize}` /
  `uuid::Uuid`；`DateTime` 固定为 UTC（ADR-COMPAT-002：OS 内部统一 UTC）。

## 架构位置

**依赖**（上游）：无内部依赖；第三方仅 serde / serde_json / thiserror / uuid /
chrono / tokio / async-trait。

**被用**（下游）：workspace 全部 20 个 crate（os-common 起所有领域 crate、osd、
os-api、os-cli、客户端 SDK 等）均可安全 `use os_core::*`。

## 独立使用

- **仓库外引用**：`os-core = { git = "http://ub2604:8080/git/nexos.git" }`
  （或 vendored / path 方式；无业务依赖，单独抽出成本低）。
- **关键接口**：
  - `EventBus` / `EventSubscriber`：事件契约（`EventSubscriber` 为 `#[async_trait]`
    dyn 兼容修正版，`TokioBroadcastBus` 为默认实现）。
  - `CoreError`：错误汇聚根，下游实现 `From<CoreError>` 接入统一错误链。
- **feature**：`mock`（默认关）——开启 `mock` 模块导出 `MockEventBus` /
  `MockEventSubscriber`，下游以
  `[dev-dependencies] os-core = { workspace = true, features = ["mock"] }` 注入测试。

## 测试

```bash
cargo test -p os-core
```

lib 单测 + `tests/coverage_smoke.rs` 冒烟（34 测：领域模型 serde 往返、
`ResourceQuota`/容量换算、`NodeRole` 等值类型行为）。
