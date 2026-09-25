//! os-discover —— 节点发现与联邦（接口契约 + 默认实现）
//!
//! 定位（规划文档 §3.14）：
//! - LAN 节点发现（mDNS / 组播 beacon，带防伪签名）
//! - 凭证配对互联（mTLS 双向认证建立 peer 会话）
//! - HA 资格检测（硬指标：节点数/带宽/ZFS/KVM/版本兼容）
//! - 联邦分支决策（自动加入 HA 集群 / 仅 peer 同步 / 保持单机）
//!
//! 被复用方：os-provision（首次组网）、手机/桌面客户端（发现本机 OS）。
//!
//! 模块组织：
//! - [`discovery`] / [`auth`] / [`federation`] / [`error`]：trait 契约（已就绪）
//! - [`capabilities`]：节点能力模型 + HA 资格检测纯算法（本批新增）
//! - [`beacon`]：beacon 防伪签名 challenge/nonce 生成与比对 + ed25519 真实验签（本批接通）
//! - [`federation_sm`]：联邦决策状态机（Probing→Authenticating→Qualifying→…→Active）（本批新增）
//! - [`impls`]：`DefaultFederationPolicy` / `MdnsDiscovery` 默认实现（本批新增）；
//!   `MdnsDiscovery` 用 mdns-sd 做真实 mDNS 组播广播/扫描（ADR-DEPS-002 接通）。
//! - [`mtls`]：`MtlsPeerAuthenticator`（`PeerAuthenticator`）——rustls 0.23 真实 mTLS 双向
//!   认证（本批接通，ADR-DEPS-002）。
//! - `mock`：feature gate `mock`，供下游 provision/client 测试（本批新增）
//!
//! 契约规范：数据路径 trait 用原生 `async fn in trait`（无 `#[async_trait]`），
//! lib 顶部统一 `#![allow(async_fn_in_trait)]`；自定义 `DiscoverError`，
//! 并实现 `From<DiscoverError> for os_common::ApiError` 以统一对外错误。

#![allow(async_fn_in_trait)]

pub mod auth;
pub mod beacon;
pub mod capabilities;
pub mod discovery;
pub mod error;
pub mod federation;
pub mod federation_sm;
pub mod impls;
pub mod mtls;

#[cfg(feature = "mock")]
pub mod mock;
#[cfg(feature = "mock")]
pub use mock::{MockDiscovery, MockFederationPolicy, MockPeerAuthenticator};

pub use auth::{PairingScope, PairingToken, PeerAuthenticator, PeerSession, PeerSessionId};
pub use beacon::{
    generate_challenge_nonce, generate_challenge_nonce_with, generate_keypair,
    generate_keypair_with, hex_decode, hex_encode, pubkey_fingerprint, sign_beacon,
    verify_beacon_signature, BeaconPayload, BeaconVerifyOutcome,
};
pub use capabilities::{qualify_peer, version_satisfies, PeerQualification};
pub use discovery::{Discovery, NodeCapabilities, PeerCallback, PeerNode};
/// ed25519 公钥/私钥类型（re-export 自 ed25519-dalek），便于消费者在不直接依赖
/// ed25519-dalek 的情况下与 [`beacon::verify_beacon_signature`] / [`beacon::sign_beacon`]
/// 交互。
pub use ed25519_dalek::{Signature as Ed25519Signature, SigningKey, VerifyingKey};
pub use error::{DiscoverError, DiscoverResult};
pub use federation::{
    FederationAction, FederationChoice, FederationPolicy, HaEligibility, HaRequirement,
};
pub use federation_sm::{ActiveRole, FederationEvent, FederationState, FederationStateMachine};
pub use impls::{DefaultFederationPolicy, MdnsDiscovery};
pub use mtls::{cert_fingerprint, MtlsPeerAuthenticator};
