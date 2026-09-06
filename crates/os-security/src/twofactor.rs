//! 双因素认证（2FA / TOTP）
//!
//! 决策依据：规划文档 §3.16 —— 管理员等高权角色强制启用 2FA。
//! 安全约束：TOTP secret 以加密形式存储（`encrypted` 字段占位），不落明文。

use crate::auth::UserId;
use serde::{Deserialize, Serialize};

// ----------------------------------------------------------------------------
// 2FA secret（加密存储）
// ----------------------------------------------------------------------------

/// 双因素密钥（加密形式；`encrypted` 为占位字段，实际为 AEAD 密文）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwoFactorSecret {
    /// 关联用户 ID
    pub user_id: UserId,
    /// 加密后的 TOTP secret（不存明文）
    pub encrypted: String,
    /// otpauth URI（仅在 enable 时一次性返回给客户端扫码；不持久化）
    #[serde(skip_serializing)]
    pub otpauth_uri: Option<String>,
}

// ----------------------------------------------------------------------------
// TwoFactor trait（async）
// ----------------------------------------------------------------------------

/// 双因素认证——启用、校验、禁用。
///
/// 实现者：`TotpTwoFactor`（RFC 6238 TOTP，30s 窗口）。
#[allow(async_fn_in_trait)]
pub trait TwoFactor: Send + Sync {
    /// 为用户启用 2FA——生成 TOTP secret（加密存储）并返回含 otpauth URI 的对象供扫码。
    async fn enable(&self, user: &UserId) -> Result<TwoFactorSecret, crate::SecurityError>;

    /// 校验用户提交的 6 位 TOTP code；返回是否通过。
    async fn verify(&self, user: &UserId, code: &str) -> Result<bool, crate::SecurityError>;

    /// 禁用用户 2FA。
    async fn disable(&self, user: &UserId) -> Result<(), crate::SecurityError>;
}
