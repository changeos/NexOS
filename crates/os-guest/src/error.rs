//! os-guest 错误类型
//!
//! 设计：每 crate 自定义 `GuestError`（thiserror），并实现
//! `From<GuestError> for os_common::ApiError`，由 os-api 网关统一序列化返回。
//! 链上验证相关错误映射到 `ChainVerificationFailed` 错误码。

use thiserror::Error;

/// os-guest 错误
#[derive(Debug, Error)]
pub enum GuestError {
    /// 访客不存在
    #[error("访客不存在: {0}")]
    GuestNotFound(String),

    /// 访客已存在（创建时 ID 冲突）
    #[error("访客已存在: {0}")]
    GuestExists(String),

    /// 访客已过期（JWT/NFT 超时，需续期或重新认证）
    #[error("访客已过期: {0}")]
    GuestExpired(String),

    /// 策略拒绝（RBAC 判定为 Deny）
    #[error("策略拒绝: {0}")]
    PolicyDenied(String),

    /// nftables 规则应用失败（dry-run 冲突 / 应用/回滚失败）
    #[error("nft 规则失败: {0}")]
    NftRuleFailed(String),

    /// 链上验证失败（签名无效/凭证不符/余额不足/链不可用且隐私档为 Mandatory）
    #[error("链上验证失败: {0}")]
    VerificationFailed(String),

    /// Portal 错误（监听失败/探测处理异常）
    #[error("Portal 错误: {0}")]
    PortalError(String),

    /// 序列化/反序列化错误
    #[error("序列化错误: {0}")]
    Serde(#[from] serde_json::Error),

    /// 系统 IO 错误
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    /// 内部错误
    #[error("内部错误: {0}")]
    Internal(String),
}

/// os-guest Result 别名
pub type GuestResult<T> = Result<T, GuestError>;

// —— From 转换：GuestError → ApiError（统一对外错误码）——
impl From<GuestError> for os_common::ApiError {
    fn from(e: GuestError) -> Self {
        use os_common::ApiErrorCode as Code;
        use GuestError as E;
        let (code, msg) = match e {
            E::GuestNotFound(m) => (Code::NotFound, m),
            E::GuestExists(m) => (Code::Conflict, m),
            E::GuestExpired(m) => (Code::PermissionDenied, m),
            E::PolicyDenied(m) => (Code::PermissionDenied, m),
            E::NftRuleFailed(m) => (Code::Internal, m),
            E::VerificationFailed(m) => (Code::ChainVerificationFailed, m),
            E::PortalError(m) => (Code::UpstreamUnavailable, m),
            E::Serde(m) => (Code::Internal, m.to_string()),
            E::Io(m) => (Code::Internal, m.to_string()),
            E::Internal(m) => (Code::Internal, m),
        };
        os_common::ApiError::new(code, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::GuestError as E;

    #[test]
    fn error_display_covers_all_variants() {
        assert!(format!("{}", E::GuestNotFound("g".into())).contains("访客不存在"));
        assert!(format!("{}", E::GuestExists("g".into())).contains("访客已存在"));
        assert!(format!("{}", E::GuestExpired("g".into())).contains("访客已过期"));
        assert!(format!("{}", E::PolicyDenied("p".into())).contains("策略拒绝"));
        assert!(format!("{}", E::NftRuleFailed("n".into())).contains("nft 规则失败"));
        assert!(format!("{}", E::VerificationFailed("v".into())).contains("链上验证失败"));
        assert!(format!("{}", E::PortalError("p".into())).contains("Portal 错误"));
        let serde_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        assert!(format!("{}", E::Serde(serde_err)).contains("序列化错误"));
        assert!(format!("{}", E::Io(std::io::Error::other("x"))).contains("IO 错误"));
        assert!(format!("{}", E::Internal("i".into())).contains("内部错误"));
    }
}
