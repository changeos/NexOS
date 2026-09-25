//! os-mobile —— 手机客户端（iOS/Android）Rust 核心 SDK（接口契约）
//!
//! 定位（规划文档 §3.15）：手机 App 的 Rust 核心。UI 层用 Vue + Capacitor，
//! Rust 层提供：发现 OS、连接/断开、查询系统状态、配对、订阅推送。
//!
//! 被复用方：os-desktop（桌面客户端复用本 crate 的 `OsClient` / `ClientSession` /
//! `SystemStatus`，避免重复定义客户端契约）。
//!
//! 本 crate 仅定义契约（trait + 数据结构 + Error），实现由 owner agent 后续填充。
//! 客户端侧 trait 用原生 `async fn in trait`，lib 顶部统一 `#![allow(async_fn_in_trait)]`。

#![allow(async_fn_in_trait)]

pub mod client;
pub mod client_impl;
pub mod error;
pub mod http;
pub mod push;
pub mod retry;
pub mod transport;

/// Mock 实现（feature gate `mock`，供前端/UI 测试）。
#[cfg(feature = "mock")]
pub mod mock;
#[cfg(feature = "mock")]
pub use mock::{MockOsClient, MockPushCallback, MockPushSubscriber};

pub use client::{ClientSession, OsClient, SystemStatus};
pub use client_impl::HttpOsClient;
pub use error::{MobileError, MobileResult};
pub use push::{
    InMemoryPushSubscriber, NotificationQueue, PushCallback, PushNotification, PushSeverity,
    PushSubscriber, PushSubscriptionState,
};
pub use retry::{decide_retry, RetryDecision, RetryPolicy, RetryableError};
pub use transport::{HttpTransport, ReqwestTransport, TransportError, TransportResult};
