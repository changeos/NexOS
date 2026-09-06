//! os-wallet 错误类型
//!
//! 设计：每 crate 自定义 `WalletError`（thiserror），并实现
//! `From<WalletError> for os_common::ApiError`，由 os-api 网关统一序列化返回。
//! 链上验证相关错误映射到 `ChainVerificationFailed` 错误码。

use thiserror::Error;

/// os-wallet 错误
#[derive(Debug, Error)]
pub enum WalletError {
    /// 会话不存在（WalletConnect session 已断开或未建立）
    #[error("钱包会话不存在: {0}")]
    SessionNotFound(String),

    /// 会话已过期
    #[error("会话已过期: {0}")]
    SessionExpired(String),

    /// 签名无效（验签失败）
    #[error("签名无效: {0}")]
    SignatureInvalid(String),

    /// 链不支持（未配置/未激活的 chain kind）
    #[error("链不支持: {0}")]
    ChainUnsupported(String),

    /// RPC 不可用（本地 + 远程均不可达，adapter 已注销）
    #[error("RPC 不可用: {0}")]
    RpcUnavailable(String),

    /// 凭证不存在（未持有对应 Ordinal/NFT）
    #[error("凭证不存在: {0}")]
    CredentialNotFound(String),

    /// 连接失败（WalletConnect relay 不可达 / 钱包拒绝配对）
    #[error("钱包连接失败: {0}")]
    ConnectFailed(String),

    /// 用户在钱包侧拒绝签名
    #[error("用户拒绝签名: {0}")]
    WalletRejected(String),

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

/// os-wallet Result 别名
pub type WalletResult<T> = Result<T, WalletError>;

// —— From 转换：WalletError → ApiError（统一对外错误码）——
impl From<WalletError> for os_common::ApiError {
    fn from(e: WalletError) -> Self {
        use os_common::ApiErrorCode as Code;
        use WalletError as E;
        let (code, msg) = match e {
            E::SessionNotFound(m) => (Code::NotFound, m),
            E::CredentialNotFound(m) => (Code::NotFound, m),
            E::SessionExpired(m) => (Code::PermissionDenied, m),
            E::SignatureInvalid(m) => (Code::ChainVerificationFailed, m),
            // 按 ERROR_GUIDE §3.3：链不支持是能力/配置缺失（用户指定了不支持的链），
            // 属输入参数非法，非密码学验证失败；故归 InvalidInput
            // （原 ChainVerificationFailed 已修正）。
            E::ChainUnsupported(m) => (Code::InvalidInput, m),
            E::RpcUnavailable(m) => (Code::UpstreamUnavailable, m),
            E::ConnectFailed(m) => (Code::UpstreamUnavailable, m),
            E::WalletRejected(m) => (Code::ChainVerificationFailed, m),
            E::Serde(m) => (Code::Internal, m.to_string()),
            E::Io(m) => (Code::Internal, m.to_string()),
            E::Internal(m) => (Code::Internal, m),
        };
        os_common::ApiError::new(code, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::WalletError as E;

    #[test]
    fn error_display_covers_all_variants() {
        assert!(format!("{}", E::SessionNotFound("s".into())).contains("钱包会话不存在"));
        assert!(format!("{}", E::SessionExpired("s".into())).contains("会话已过期"));
        assert!(format!("{}", E::SignatureInvalid("s".into())).contains("签名无效"));
        assert!(format!("{}", E::ChainUnsupported("c".into())).contains("链不支持"));
        assert!(format!("{}", E::RpcUnavailable("r".into())).contains("RPC 不可用"));
        assert!(format!("{}", E::CredentialNotFound("c".into())).contains("凭证不存在"));
        assert!(format!("{}", E::ConnectFailed("c".into())).contains("钱包连接失败"));
        assert!(format!("{}", E::WalletRejected("w".into())).contains("用户拒绝签名"));
        let serde_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        assert!(format!("{}", E::Serde(serde_err)).contains("序列化错误"));
        assert!(format!("{}", E::Io(std::io::Error::other("x"))).contains("IO 错误"));
        assert!(format!("{}", E::Internal("i".into())).contains("内部错误"));
    }
}
