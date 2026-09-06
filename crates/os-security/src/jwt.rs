//! JWT——签发、校验、密钥轮换
//!
//! 决策依据：规划文档 §3.16 —— 对外 API 鉴权用 JWT（Access/Refresh），
//! 链上凭证访客用 `ChainCredential` 类型 token（呼应 §3.18）。

use serde::{Deserialize, Serialize};

// 重新导出 UserId/Role（与 auth 模块共享）
pub use crate::auth::{Role, UserId};

// ----------------------------------------------------------------------------
// JWT claims
// ----------------------------------------------------------------------------

/// token 类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenType {
    /// 访问令牌（短期）
    Access,
    /// 刷新令牌（长期，用于换取 Access）
    Refresh,
    /// 访客令牌
    Guest,
    /// 链上凭证令牌（呼应 §3.18，承载链上凭证声明）
    ChainCredential,
}

/// JWT claims（载荷）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JwtClaims {
    /// subject——用户 ID
    pub sub: UserId,
    /// 角色
    pub roles: Vec<Role>,
    /// 过期时间（Unix 秒）
    pub exp: i64,
    /// 签发时间（Unix 秒）
    pub iat: i64,
    /// token 类型
    pub token_type: TokenType,
    /// 自定义扩展字段（如链上凭证声明、来源 IP 等）
    pub custom: serde_json::Value,
}

// ----------------------------------------------------------------------------
// JwtIssuer trait（async）
// ----------------------------------------------------------------------------

/// JWT 签发器——签发、校验、密钥轮换。
///
/// 实现者：`JwtIssuerImpl`（HS256/RS256）；密钥可热轮换，旧 token 在宽限期内仍可校验。
///
/// 经 `Box<dyn JwtIssuer>` / `Arc<dyn JwtIssuer>` 运行期注入（如 os-guest
/// `DefaultChainOrchestrator`），故用 `#[async_trait]` 保证 dyn 兼容（ADR-COMPAT-001）。
#[async_trait::async_trait]
pub trait JwtIssuer: Send + Sync {
    /// 签发 token（编码为紧凑 JWT 字符串）。
    async fn issue(&self, claims: JwtClaims) -> Result<String, crate::SecurityError>;

    /// 校验 token（签名 + 过期 + 类型）；成功返回 claims。
    async fn verify(&self, token: &str) -> Result<JwtClaims, crate::SecurityError>;

    /// 轮换签名密钥（旧密钥进入宽限期，逐步淘汰）。
    async fn rotate_keys(&self) -> Result<(), crate::SecurityError>;
}
