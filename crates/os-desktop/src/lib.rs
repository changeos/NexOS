//! os-desktop —— 桌面客户端（Windows 优先）Rust 核心 SDK（接口契约）
//!
//! 定位（规划文档 §3.15）：桌面软件的 Rust 核心。UI 层用 Tauri + Vue，
//! Rust 层在 os-mobile 客户端契约（发现/连接/状态/配对/推送）之上，额外提供
//! **一键挂载为网络驱动器**（SMB / WebDAV）能力。
//!
//! 客户端侧契约（`OsClient` / `ClientSession` / `SystemStatus`）直接复用 os-mobile，
//! 通过 `pub use` 重导出，避免重复定义、保证两端行为一致。
//!
//! 本 crate 仅定义契约（trait + 数据结构 + Error），实现由 owner agent 后续填充。
//! 客户端侧 trait 用原生 `async fn in trait`，lib 顶部统一 `#![allow(async_fn_in_trait)]`。

#![allow(async_fn_in_trait)]

pub mod client;
pub mod error;
pub mod mount;
pub mod mount_impl;

/// Mock 实现（feature gate `mock`，供前端/UI 测试）。
#[cfg(feature = "mock")]
pub mod mock;
#[cfg(feature = "mock")]
pub use mock::MockMountManager;

// 复用 os-mobile 的客户端契约（共享 trait，桌面/手机两端一致）
pub use client::{ClientSession, OsClient, SystemStatus};
pub use error::{DesktopError, DesktopResult};
pub use mount::{MountInfo, MountManager, MountProtocol, MountTarget, RemoteShare};
pub use mount_impl::{
    build_davfs2_command, build_fstab_line, build_net_use_command, Davfs2Command, NetUseCommand,
    SystemMountManager,
};
