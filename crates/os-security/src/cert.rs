//! 证书管理——内部 CA + ACME 自动签续
//!
//! 决策依据：规划文档 §3.16 —— 内部服务用自建 CA 签发；
//! 对外域名走 ACME（Let's Encrypt 等）自动签发与续期。

use os_core::DateTime;
use serde::{Deserialize, Serialize};

// ----------------------------------------------------------------------------
// 证书模型
// ----------------------------------------------------------------------------

/// 证书（X.509）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Certificate {
    /// 证书 ID（序列号或内部 ID）
    pub id: String,
    /// 通用名（CN）
    pub common_name: String,
    /// 生效起始时间（UTC）
    pub not_before: DateTime,
    /// 过期时间（UTC）
    pub not_after: DateTime,
    /// 签发者（issuer CN）
    pub issuer: String,
    /// 序列号
    pub serial: String,
    /// 是否启用自动续期
    pub auto_renew: bool,
}

// ----------------------------------------------------------------------------
// CertManager trait（async）
// ----------------------------------------------------------------------------

/// 证书管理器——内部 CA 维护与 ACME 自动签续。
///
/// 实现者：`CaCertManager`（内部 CA，基于 rcgen/openssl）+ `AcmeClient`（ACME）。
#[allow(async_fn_in_trait)]
pub trait CertManager: Send + Sync {
    /// 初始化内部 CA（生成根证书/密钥）；已存在则返回错误或现有证书（由实现决定）。
    async fn init_ca(&self, common_name: &str) -> Result<Certificate, crate::SecurityError>;

    /// 用内部 CA 签发证书（输入 CSR 字节，返回签发后的证书字节）。
    ///
    /// - `csr`：PEM/DER 编码的 CSR
    /// - `days`：有效期天数
    async fn sign(&self, csr: &[u8], days: u32) -> Result<Vec<u8>, crate::SecurityError>;

    /// 列出所有已签发证书。
    async fn list_certs(&self) -> Result<Vec<Certificate>, crate::SecurityError>;

    /// 续期指定证书（重新签发，保持 CN）。
    async fn renew(&self, id: &str) -> Result<(), crate::SecurityError>;

    /// 通过 ACME 为指定域名申请证书（自动完成 challenge 与签发）。
    async fn acme_request(&self, domain: &str) -> Result<Certificate, crate::SecurityError>;
}
