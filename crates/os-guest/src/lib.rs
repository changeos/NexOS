//! os-guest —— 访客接入管理（接口契约）
//!
//! 定位（规划文档 §3.18）：
//! - Captive Portal（兼容 iOS/Android/Win/macOS 探测，302 重定向到落地页）
//! - 访客身份引擎（RandomId / ExtendedId / PublicKey / ChainCredential 四类身份）
//! - RBAC 策略引擎（条件→Allow/Deny，支撑 IM 入群/共享访问/带宽/认证授权）
//! - nftables guest 链编排（带 timeout 自动过期 + dry-run + checkpoint 回滚）
//! - 链上凭证业务编排（chain-orchestrator：编排 os-wallet 完成验证，本身不下沉签名/连接）
//!
//! 关键设计点（§3.18.1）：
//! - 链上验证的签名/连接/凭证查询全部**委派给 os-wallet**（WalletConnector/ChainAdapter/RpcRegistry），
//!   本 crate 仅做业务编排（建 session → 请求签名 → 验签 → 查凭证 → 签 JWT）。
//! - 链不可用时经 RpcRegistry.is_available 判定后降级（隐私三档：Mandatory/Optional/None）。
//!
//! 本 crate 仅定义契约（trait + 数据结构 + Error），实现由 owner agent 后续填充。
//!
//! 契约规范：数据路径 trait 用原生 `async fn in trait`（无 `#[async_trait]`），
//! lib 顶部统一 `#![allow(async_fn_in_trait)]`；自定义 `GuestError`，
//! 并实现 `From<GuestError> for os_common::ApiError` 以统一对外错误。

#![allow(async_fn_in_trait)]

pub mod chain;
pub mod error;
pub mod identity;
pub mod model;
pub mod nft;
pub mod policy;
pub mod portal;

/// 默认实现（trait 骨架，纯内存态 + 委派上游）。
pub mod impls;

/// Mock 实现（仅 `mock` feature 下编译）。
#[cfg(feature = "mock")]
pub mod mock;

pub use chain::{ChainOrchestrator, ChainVerificationConfig, ChainVerificationStatus, PrivacyMode};
pub use error::{GuestError, GuestResult};
pub use identity::{GuestFilter, IdentityEngine};
pub use model::{
    generate_guest_id, generate_guest_id_with, validate_guest_id, EntropySource, FileAccess,
    GuestFileShare, GuestIdentity, GuestIdentityType, GuestRole, GuestStatus, SystemEntropy,
};
pub use nft::{
    build_add_element, build_checkpoint_statement, build_delete_element, build_port_accept_rule,
    statements_for_rule, DryRunResult, NftGuestAction, NftGuestRule, NftRuleOrchestrator,
};
pub use policy::{
    condition_matches, evaluate_rules, GuestAction, GuestContext, PolicyCondition, PolicyDecision,
    PolicyEffect, PolicyEngine, PolicyRule,
};
pub use portal::{
    decide_response, detect_probe_os, CaptivePortal, PortalConfig, PortalResponse, ProbeOs,
    ProbeRequest,
};

// 重导出默认实现（顶层便捷访问）。
pub use impls::{
    DefaultChainOrchestrator, DefaultIdentityEngine, DefaultPolicyEngine, HttpCaptivePortal,
    NftRuleOrchestratorImpl,
};

// 重导出 mock 类型（仅 mock feature）——下游 dev-dependencies 启用后即可用。
#[cfg(feature = "mock")]
pub use mock::{
    MockCaptivePortal, MockChainOrchestrator, MockIdentityEngine, MockNftRuleOrchestrator,
    MockPolicyEngine,
};
