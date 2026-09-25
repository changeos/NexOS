//! os-security —— 安全与认证（用户认证 / JWT / CA + ACME 证书 / TOTP 双因素 / WireGuard VPN）。
//!
//! 提供：用户认证（[`AuthProvider`]）、JWT 签发/校验（[`JwtIssuer`]）、
//! 证书管理 CA + ACME（[`CertManager`]）、双因素认证 TOTP（[`TwoFactor`]）、
//! VPN 基于 WireGuard/boringtun（[`VpnManager`]）。
//!
//! 详见规划文档 §3.16（os-security 子项）与 §15「接口契约索引」。
//!
//! 契约规范：数据路径 trait 用原生 `async fn in trait`（无 `#[async_trait]`），
//! lib 顶部统一 `#![allow(async_fn_in_trait)]`；每 crate 自定义 `SecurityError`，
//! 并实现 `From<SecurityError> for os_common::ApiError` 以统一对外错误。
//!
//! 安全约束：本 crate 不持有任何明文密码/密钥；密码以 `password_hash` 存储，
//! TOTP secret 以加密形式（`encrypted`）存储。
//!
//! # 模块
//!
//! - [`auth`]：用户名/密码认证契约——[`AuthProvider`] trait。
//! - [`jwt`]：JWT 签发/校验契约——[`JwtIssuer`] trait。
//! - [`cert`]：X.509 证书管理契约——[`CertManager`] trait（自签 CA + 服务端证书）。
//! - [`acme`]：ACME（Let's Encrypt）自动化——`acme::AcmeChallengeSolver` trait（+ `AcmeConfig`/`AutoSolveSolver`）。
//! - [`totp`]：TOTP 算法实现（RFC 6238，基于 `totp-rs`）。
//! - [`twofactor`]：双因素认证契约——[`TwoFactor`] trait（绑定/校验 TOTP）。
//! - [`password`]：密码哈希（基于 `password_hash`/Argon2）。
//! - [`vpn`]：WireGuard VPN 契约——[`VpnManager`] trait（基于 boringtun）。
//! - [`impls`]：各 trait 的默认实现（真实算法后端）。
//! - [`error`]：`SecurityError` / `SecurityResult`。
//! - `mock`：测试桩（仅 `mock` feature）。
//!
//! # 关键 trait
//!
//! - [`AuthProvider`]：用户认证（verify_credentials / 用户 CRUD）。
//! - [`JwtIssuer`]：JWT 签发与校验（issue / verify / refresh）。
//! - [`CertManager`]：证书生命周期（自签 CA / 签发服务端证书 / 吊销）。
//! - `acme::AcmeChallengeSolver`：ACME challenge 应答抽象（DNS-01 / HTTP-01）。
//! - [`TwoFactor`]：双因素绑定/校验/恢复码。
//! - [`VpnManager`]：WireGuard peer 管理（add/remove peer，密钥协商）。
//!
//! # feature 门控
//!
//! - `mock`（默认关）：开启 `mock` 模块，导出 `MockAuthProvider`/`MockJwtIssuer`/`MockCertManager`/`MockTwoFactor`/`MockVpnManager` 供下游测试注入。

#![allow(async_fn_in_trait)]

pub mod acme;
pub mod auth;
pub mod cert;
pub mod error;
pub mod impls;
pub mod jwt;
pub mod password;
pub mod totp;
pub mod twofactor;
pub mod vpn;

#[cfg(feature = "mock")]
pub mod mock;
#[cfg(feature = "mock")]
pub use mock::{MockAuthProvider, MockCertManager, MockJwtIssuer, MockTwoFactor, MockVpnManager};

pub use auth::*;
pub use cert::*;
pub use error::{SecurityError, SecurityResult};
pub use impls::*;
pub use jwt::*;
pub use twofactor::*;
pub use vpn::*;
