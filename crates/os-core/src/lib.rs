//! os-core —— OS 系统基础层（领域 newtype ID / 领域模型 / 节点内事件总线）。
//!
//! 提供：领域 newtype ID、领域模型、`EventBus` trait（节点内事件总线，见规划文档 §9.1#9）。
//! 本 crate 无业务依赖，是所有其他 crate 的根基——任何 os-* crate 都可安全 `use os_core::*`。
//!
//! # 模块
//!
//! - [`ids`]：领域 newtype ID（`PoolId`/`DatasetId`/`SnapshotId`/`VolumeId`/`TaskId`/`NodeId` 等，统一 `Uuid` 包装）。
//! - [`types`]：跨 crate 共享的领域模型（`CommandOutput`/`ResourceQuota` 等值类型）。
//! - [`eventbus`]：节点内事件总线契约——[`EventBus`] + [`EventSubscriber`]、`Event`/`Topic`/`Severity`。
//! - [`bus`]：`EventBus` 默认实现 [`TokioBroadcastBus`]（基于 `tokio::sync::broadcast`）。
//! - [`error`]：`CoreError` / `CoreResult`（其他 crate Error 经 `From<CoreError>` 转换）。
//! - `mock`：测试桩（仅 `mock` feature 下编译，下游 dev-dependencies 启用）。
//!
//! # 关键 trait
//!
//! - [`EventBus`]：节点内事件总线（pub/sub，按 `Topic` 订阅，返回 `SubscriptionId`）。
//! - [`EventSubscriber`]：订阅者句柄（cancel/收消息），由 `EventBus::subscribe` 返回。
//!
//! # feature 门控
//!
//! - `mock`（默认关）：开启 `mock` 模块，导出 `MockEventBus` / `MockEventSubscriber` 供下游测试注入。
//!
//! # 常用类型快捷入口
//!
//! 本 crate 重导出 `chrono::Utc` / `serde::{Serialize, Deserialize}` / `uuid::Uuid`，
//! 并固定 `DateTime = chrono::DateTime<chrono::Utc>`（见 ADR-COMPAT-002：OS 内部统一 UTC）。
//!
//! 详见规划文档 §3.0/#1（SSOT 总表）与 §13.1（CoreAgent 拥有本 crate）。

pub mod error;
pub mod eventbus;
pub mod ids;
pub mod types;

/// EventBus 默认实现（基于 tokio::sync::broadcast）
pub mod bus;
/// Mock 实现（仅 `mock` feature 下编译）
#[cfg(feature = "mock")]
pub mod mock;

pub use bus::TokioBroadcastBus;
pub use error::{CoreError, CoreResult};
pub use eventbus::{Event, EventBus, EventSubscriber, Severity, SubscriptionId, Topic};
pub use ids::*;
pub use types::*;

// 重导出 mock 类型（仅 mock feature）——下游 dev-dependencies 启用后即可用。
#[cfg(feature = "mock")]
pub use mock::{MockEventBus, MockEventSubscriber};

// 重导出常用第三方类型，供下游 crate 统一引用。
//
// `DateTime` 统一固定为 UTC 时区（见 ADR-COMPAT-002）：OS 全系统用 UTC 作为内部
// 时间表示（日志/快照时间戳/事件时间/任务截止），前端展示时再转本地时区。
// 下游裸 `DateTime` 即 `chrono::DateTime<chrono::Utc>`；若某处确需其他时区，
// 显式用 `chrono::DateTime<Tz>`。
pub use chrono::Utc;
pub type DateTime = chrono::DateTime<chrono::Utc>;
pub use serde::{Deserialize, Serialize};
pub use uuid::Uuid;
