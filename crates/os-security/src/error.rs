//! os-security 错误类型
//!
//! 设计：每 crate 自定义 `SecurityError`（thiserror），并实现
//! `From<SecurityError> for os_common::ApiError`，由 os-api 网关统一序列化返回。

use thiserror::Error;

/// os-security 错误
#[derive(Debug, Error)]
pub enum SecurityError {
    /// 用户不存在
    #[error("用户不存在: {0}")]
    UserNotFound(String),

    /// 认证失败（用户名/密码错误）
    #[error("认证失败")]
    AuthFailed,

    /// 用户已存在（创建冲突）
    #[error("用户已存在: {0}")]
    UserExists(String),

    /// 证书已过期
    #[error("证书已过期: {0}")]
    CertExpired(String),

    /// JWT 无效（签名错误/过期/格式错）
    #[error("JWT 无效: {0}")]
    JwtInvalid(String),

    /// 双因素认证失败（code 错误/未启用）
    #[error("双因素认证失败: {0}")]
    TwoFactorFailed(String),

    /// VPN 错误（WireGuard 配置/运行失败）
    #[error("VPN 错误: {0}")]
    VpnError(String),

    /// 内部错误
    #[error("内部错误: {0}")]
    Internal(String),
}

/// os-security Result 别名
pub type SecurityResult<T> = Result<T, SecurityError>;

// —— From 转换：SecurityError → ApiError（统一对外错误码）——
impl From<SecurityError> for os_common::ApiError {
    fn from(e: SecurityError) -> Self {
        use os_common::ApiErrorCode as Code;
        use SecurityError as E;
        let (code, msg) = match e {
            E::UserNotFound(m) => (Code::NotFound, m),
            E::AuthFailed => (Code::PermissionDenied, "认证失败".into()),
            E::UserExists(m) => (Code::Conflict, m),
            // 按 ERROR_GUIDE §3.3：证书过期属凭证失效，应引导续签/重认证，
            // 与同 crate 的 JwtInvalid/SessionExpired 一致归 PermissionDenied
            // （原 UpstreamUnavailable 已修正）。
            E::CertExpired(m) => (Code::PermissionDenied, m),
            E::JwtInvalid(m) => (Code::PermissionDenied, m),
            E::TwoFactorFailed(m) => (Code::PermissionDenied, m),
            E::VpnError(m) => (Code::Internal, m),
            E::Internal(m) => (Code::Internal, m),
        };
        os_common::ApiError::new(code, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::SecurityError as E;

    #[test]
    fn error_display_covers_all_variants() {
        assert!(format!("{}", E::UserNotFound("u".into())).contains("用户不存在"));
        assert!(format!("{}", E::AuthFailed).contains("认证失败"));
        assert!(format!("{}", E::UserExists("u".into())).contains("用户已存在"));
        assert!(format!("{}", E::CertExpired("c".into())).contains("证书已过期"));
        assert!(format!("{}", E::JwtInvalid("j".into())).contains("JWT 无效"));
        assert!(format!("{}", E::TwoFactorFailed("t".into())).contains("双因素认证失败"));
        assert!(format!("{}", E::VpnError("v".into())).contains("VPN 错误"));
        assert!(format!("{}", E::Internal("i".into())).contains("内部错误"));
    }
}
