//! os-mobile 错误类型
//!
//! 设计：每 crate 自定义 `MobileError`（thiserror），并实现
//! `From<MobileError> for os_common::ApiError`，由调用方（Capacitor 桥）统一对外。

use thiserror::Error;

/// os-mobile 错误
#[derive(Debug, Error)]
pub enum MobileError {
    /// 未连接（调用需连接的方法前未 connect / 已 disconnect）
    #[error("未连接 OS")]
    NotConnected,

    /// 端点不可达（网络错误 / 超时 / DNS 失败）
    #[error("端点不可达: {0}")]
    EndpointUnreachable(String),

    /// 配对失败（配对码无效 / 已过期 / 被拒绝）
    #[error("配对失败: {0}")]
    PairingFailed(String),

    /// 推送失败（注册/下发失败，FCM/APNs 不可用）
    #[error("推送失败: {0}")]
    PushFailed(String),

    /// 系统 IO 错误
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    /// 内部错误
    #[error("内部错误: {0}")]
    Internal(String),
}

/// os-mobile Result 别名
pub type MobileResult<T> = Result<T, MobileError>;

// —— From 转换：MobileError → ApiError（统一对外错误码）——
impl From<MobileError> for os_common::ApiError {
    fn from(e: MobileError) -> Self {
        use os_common::ApiErrorCode as Code;
        use MobileError as E;
        let (code, msg) = match e {
            E::NotConnected => (Code::PermissionDenied, "未连接 OS".to_string()),
            E::EndpointUnreachable(m) => (Code::UpstreamUnavailable, m),
            E::PairingFailed(m) => (Code::PermissionDenied, m),
            E::PushFailed(m) => (Code::UpstreamUnavailable, m),
            E::Io(m) => (Code::Internal, m.to_string()),
            E::Internal(m) => (Code::Internal, m),
        };
        os_common::ApiError::new(code, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::MobileError as E;
    use os_common::{ApiError, ApiErrorCode as Code};

    #[test]
    fn error_display_covers_all_variants() {
        assert!(format!("{}", E::NotConnected).contains("未连接 OS"));
        assert!(format!("{}", E::EndpointUnreachable("e".into())).contains("端点不可达"));
        assert!(format!("{}", E::PairingFailed("p".into())).contains("配对失败"));
        assert!(format!("{}", E::PushFailed("p".into())).contains("推送失败"));
        assert!(format!("{}", E::Io(std::io::Error::other("x"))).contains("IO 错误"));
        assert!(format!("{}", E::Internal("i".into())).contains("内部错误"));
    }

    // —— 扩展边界（覆盖率补测：From<MobileError> for ApiError 全变体映射）——

    #[test]
    fn not_connected_maps_to_permission_denied() {
        let api: ApiError = E::NotConnected.into();
        assert_eq!(api.code, Code::PermissionDenied);
        assert_eq!(api.message, "未连接 OS");
    }

    #[test]
    fn endpoint_unreachable_maps_to_upstream_unavailable() {
        let api: ApiError = E::EndpointUnreachable("net err".into()).into();
        assert_eq!(api.code, Code::UpstreamUnavailable);
        assert_eq!(api.message, "net err");
    }

    #[test]
    fn pairing_failed_maps_to_permission_denied() {
        let api: ApiError = E::PairingFailed("bad code".into()).into();
        assert_eq!(api.code, Code::PermissionDenied);
        assert_eq!(api.message, "bad code");
    }

    #[test]
    fn push_failed_maps_to_upstream_unavailable() {
        let api: ApiError = E::PushFailed("fcm down".into()).into();
        assert_eq!(api.code, Code::UpstreamUnavailable);
        assert_eq!(api.message, "fcm down");
    }

    #[test]
    fn io_error_maps_to_internal() {
        let api: ApiError = E::Io(std::io::Error::other("disk fail")).into();
        assert_eq!(api.code, Code::Internal);
        assert!(api.message.contains("disk fail"));
    }

    #[test]
    fn internal_maps_to_internal() {
        let api: ApiError = E::Internal("boom".into()).into();
        assert_eq!(api.code, Code::Internal);
        assert_eq!(api.message, "boom");
    }

    #[test]
    fn error_source_chain() {
        // thiserror #[error] + #[from]：Io 的 source 应是内部 io::Error
        let io_err = std::io::Error::other("x");
        let e = E::Io(io_err);
        // std::error::Error::source
        use std::error::Error as _;
        assert!(e.source().is_some());
    }

    #[test]
    fn error_from_io_error_kind() {
        // 从 std::io::Error 的常见 kind 构造（#[from] 路径）
        let e1 = E::from(std::io::Error::new(std::io::ErrorKind::NotFound, "no file"));
        let e2 = E::from(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "denied",
        ));
        assert!(format!("{}", e1).contains("IO 错误"));
        assert!(format!("{}", e2).contains("IO 错误"));
    }

    #[test]
    fn error_debug_format_all_variants() {
        // Debug 派生间接覆盖
        let _d1 = format!("{:?}", E::NotConnected);
        let _d2 = format!("{:?}", E::EndpointUnreachable("e".into()));
        let _d3 = format!("{:?}", E::Internal("i".into()));
    }
}
