//! os-meta —— OS 系统集群控制面（接口契约）
//!
//! 定位（规划文档 §3.5 / §9.1#7）：
//! - HA 集群共识 / 选主 / 分布式 KV / 故障转移 / 浮动 VIP（基于 openraft）
//! - 元数据 HA 复制：openraft 状态机内嵌 SQLite，快照随 log 复制
//!
//! 本 crate 仅定义契约（trait + 数据结构 + Error），实现由 owner agent 后续填充。
//!
//! 契约规范：按 ADR-COMPAT-001，5 个 async trait（Consensus/DistributedKv/MetaStore/
//! FailoverOrchestrator/VipManager）均经 `Box<dyn>` 运行期多态（见 mock.rs
//! `_assert_dyn_compatible`），故一律加 `#[async_trait]`（宏把 async fn 重写为
//! `Pin<Box<dyn Future + Send>>` 恢复对象安全）。trait 方法签名不变。
//! 自定义 `MetaError`，并实现 `From<MetaError> for os_common::ApiError` 统一对外错误。

#![allow(async_fn_in_trait)]

pub mod consensus;
pub mod error;
pub mod failover;
pub mod failover_sm;
pub mod impls;
pub mod kv;
pub mod meta_apply;
pub mod meta_store;
#[cfg(feature = "mock")]
pub mod mock;
pub mod raft;
pub mod raft_backend;
pub mod vip;
#[cfg(feature = "mock")]
pub use mock::{
    MockConsensus, MockDistributedKv, MockFailoverOrchestrator, MockMetaStore, MockVipManager,
};

pub use consensus::{ClusterConfig, ClusterState, ClusterStatus, Consensus};
pub use error::{MetaError, MetaResult};
pub use failover::{FailoverEvent, FailoverOrchestrator, FailoverStatus};
pub use kv::{DistributedKv, KvEntry};
pub use meta_store::{MetaSnapshot, MetaStore};
pub use vip::{VipConfig, VipManager};
