//! os-discover 错误类型
//!
//! 设计：每 crate 自定义 `DiscoverError`（thiserror），并实现
//! `From<DiscoverError> for os_common::ApiError`，由 os-api 网关统一序列化返回。

use thiserror::Error;

/// os-discover 错误
#[derive(Debug, Error)]
pub enum DiscoverError {
    /// 未发现任何 peer（扫描超时且局域网无应答）
    #[error("未发现 peer: {0}")]
    PeerNotFound(String),

    /// 配对失败（凭证无效/已过期/被对端拒绝）
    #[error("配对失败: {0}")]
    PairingFailed(String),

    /// beacon 签名无效（防伪校验未通过，疑似伪造节点）
    #[error("beacon 签名无效: {0}")]
    BeaconInvalid(String),

    /// mTLS 握手失败（证书不受信/指纹不匹配/握手超时）
    #[error("mTLS 握手失败: {0}")]
    MtlsHandshakeFailed(String),

    /// 版本不兼容（对端版本不在兼容范围内，无法组网）
    #[error("版本不兼容: {0}")]
    IncompatibleVersion(String),

    /// 系统 IO 错误
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    /// 内部错误
    #[error("内部错误: {0}")]
    Internal(String),
}

/// os-discover Result 别名
pub type DiscoverResult<T> = Result<T, DiscoverError>;

// —— From 转换：DiscoverError → ApiError（统一对外错误码）——
impl From<DiscoverError> for os_common::ApiError {
    fn from(e: DiscoverError) -> Self {
        use os_common::ApiErrorCode as Code;
        use DiscoverError as E;
        let (code, msg) = match e {
            E::PeerNotFound(m) => (Code::NotFound, m),
            E::PairingFailed(m) => (Code::PermissionDenied, m),
            // ERROR_GUIDE §3.3 P3 保留：beacon 签名属防伪校验（"身份未通过"），
            // 非链上密码学验证，归 PermissionDenied 符合 §1.2。
            E::BeaconInvalid(m) => (Code::PermissionDenied, m),
            // ERROR_GUIDE §3.3 P3 保留：mTLS 握手失败多因证书不受信，
            // 归 PermissionDenied 符合 §1.2（本地非链上密码校验）。
            E::MtlsHandshakeFailed(m) => (Code::PermissionDenied, m),
            E::IncompatibleVersion(m) => (Code::Conflict, m),
            E::Io(m) => (Code::Internal, m.to_string()),
            E::Internal(m) => (Code::Internal, m),
        };
        os_common::ApiError::new(code, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::DiscoverError as E;

    #[test]
    fn error_display_covers_all_variants() {
        assert!(format!("{}", E::PeerNotFound("p".into())).contains("未发现 peer"));
        assert!(format!("{}", E::PairingFailed("p".into())).contains("配对失败"));
        assert!(format!("{}", E::BeaconInvalid("b".into())).contains("beacon 签名无效"));
        assert!(format!("{}", E::MtlsHandshakeFailed("m".into())).contains("mTLS 握手失败"));
        assert!(format!("{}", E::IncompatibleVersion("v".into())).contains("版本不兼容"));
        assert!(format!("{}", E::Io(std::io::Error::other("x"))).contains("IO 错误"));
        assert!(format!("{}", E::Internal("i".into())).contains("内部错误"));
    }
}
