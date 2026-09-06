//! os-wallet —— 多链钱包与签名中枢契约
//!
//! 定位（规划文档 §3.17 / §3.18.1）：
//! - 多链钱包连接（BTC + EVM；WalletConnect v2 / 注入 / 二维码）
//! - 签名与验证（BIP-322 / Schnorr / ECDSA / EIP-191 / EIP-712）
//! - 链上凭证查询（持有 Ordinal/NFT？）——支撑访客访问三因子之一
//! - RPC 条件激活：链适配器按 RPC 可用性动态注册/注销（§3.17 核心）
//!
//! 本 crate 仅定义契约（trait + 数据结构 + Error），实现由 owner agent 后续填充。
//!
//! 设计要点：
//! - `ChainAdapter` 是链无关核心抽象；`RpcRegistry` 按 RPC 状态条件激活 adapter
//! - 链上凭证 JWT 用 `os_security::JwtClaims` / `TokenType::ChainCredential` 承载
//! - 所有数据路径 trait 用原生 `async fn in trait`（无 `#[async_trait]`）

#![allow(async_fn_in_trait)]

pub mod chain;
pub mod connector;
pub mod error;
pub mod model;
pub mod registry;
pub mod signing;

/// Mock 实现（仅 `mock` feature 下编译）。
#[cfg(feature = "mock")]
pub mod mock;

pub use chain::{BitcoinAdapter, ChainAdapter, CredentialSpec, EvmAdapter};
pub use connector::{
    ConnectorKind, InjectedConnector, QrCodeConnector, SessionStore, SignRequest, SignResponse,
    WalletConnectV2Connector, WalletConnector, WalletSession,
};
pub use error::{WalletError, WalletResult};
pub use model::{
    meets_balance_threshold, validate_evm_address, AddressInfo, ChainConfig, ChainKind,
    SignatureAlgorithm, SignatureResult, VerificationFactor,
};
pub use registry::{RpcCacheEntry, RpcRegistry, RpcRegistryImpl, RpcSource, RpcState, RpcStatus};

// 重导出 mock 类型（仅 mock feature）——下游 dev-dependencies 启用后即可用。
#[cfg(feature = "mock")]
pub use mock::{MockChainAdapter, MockRpcRegistry, MockWalletConnector};
