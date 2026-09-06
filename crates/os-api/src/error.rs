//! os-api 错误类型
//!
//! 设计：每 crate 自定义 `ApiGatewayError`（thiserror），并实现
//! `From<ApiGatewayError> for os_common::ApiError`。
//! 注意：本 crate 的产出即为对外 API，故转换多为身份映射（错误码一一对应）。

use thiserror::Error;

/// os-api 网关错误
#[derive(Debug, Error)]
pub enum ApiGatewayError {
    /// 路由冲突（多组件注册了相同 method+path）
    #[error("路由冲突: {0}")]
    RouteConflict(String),

    /// 组件未注册（handle 调用到未注册组件）
    #[error("组件未注册: {0}")]
    ComponentNotFound(String),

    /// TLS 错误（证书加载/握手失败）
    #[error("TLS 错误: {0}")]
    TlsError(String),

    /// 限流触发
    #[error("限流: {0}")]
    RateLimited(String),

    /// 未认证/权限不足
    #[error("未授权: {0}")]
    Unauthorized(String),

    /// 内部错误
    #[error("内部错误: {0}")]
    Internal(String),
}

/// os-api Result 别名
pub type ApiGatewayResult<T> = Result<T, ApiGatewayError>;

// —— From 转换：rusqlite::Error → ApiGatewayError ——
// api_gateway.rs 持久化层（渠道/令牌/日志/映射 CRUD）的 SQLite 错误统一映射为
// Internal，便于 handler 用 `?` 短路（DB 故障降级为 5xx，不 panic）。
impl From<rusqlite::Error> for ApiGatewayError {
    fn from(e: rusqlite::Error) -> Self {
        ApiGatewayError::Internal(format!("数据库错误: {e}"))
    }
}

// —— From 转换：HandlerError → ApiGatewayError ——
// NexHub 独立化（审计 §6.2 方案 1）：os-nexhub 等领域 crate 的契约 handler 错误用
// os-common 的轻量 HandlerError（无 rusqlite From），装配层（gateway.rs 的契约
// 桥接 blanket impl）经此身份映射收敛为网关错误，外部行为不变。
impl From<crate::gateway::HandlerError> for ApiGatewayError {
    fn from(e: crate::gateway::HandlerError) -> Self {
        use crate::gateway::HandlerError as H;
        match e {
            H::Unauthorized(m) => ApiGatewayError::Unauthorized(m),
            H::Internal(m) => ApiGatewayError::Internal(m),
        }
    }
}

// —— From 转换：ApiGatewayError → ApiError ——
// 网关是对外出口，错误码多为身份映射。
impl From<ApiGatewayError> for os_common::ApiError {
    fn from(e: ApiGatewayError) -> Self {
        use os_common::ApiErrorCode as Code;
        use ApiGatewayError as E;
        let (code, msg) = match e {
            E::RouteConflict(m) => (Code::Conflict, m),
            E::ComponentNotFound(m) => (Code::NotFound, m),
            E::TlsError(m) => (Code::Internal, m),
            E::RateLimited(m) => (Code::RateLimited, m),
            E::Unauthorized(m) => (Code::PermissionDenied, m),
            E::Internal(m) => (Code::Internal, m),
        };
        os_common::ApiError::new(code, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::ApiGatewayError as E;
    use os_common::{ApiError, ApiErrorCode};

    #[test]
    fn error_display_covers_all_variants() {
        assert!(format!("{}", E::RouteConflict("r".into())).contains("路由冲突"));
        assert!(format!("{}", E::ComponentNotFound("c".into())).contains("组件未注册"));
        assert!(format!("{}", E::TlsError("t".into())).contains("TLS"));
        assert!(format!("{}", E::RateLimited("r".into())).contains("限流"));
        assert!(format!("{}", E::Unauthorized("u".into())).contains("未授权"));
        assert!(format!("{}", E::Internal("i".into())).contains("内部错误"));
    }

    /// 覆盖 `From<ApiGatewayError> for ApiError` 的所有变体 → 错误码身份映射。
    #[test]
    fn error_to_api_error_maps_all_variants() {
        let cases: [(E, ApiErrorCode); 6] = [
            (E::RouteConflict("r".into()), ApiErrorCode::Conflict),
            (E::ComponentNotFound("c".into()), ApiErrorCode::NotFound),
            (E::TlsError("t".into()), ApiErrorCode::Internal),
            (E::RateLimited("x".into()), ApiErrorCode::RateLimited),
            (E::Unauthorized("u".into()), ApiErrorCode::PermissionDenied),
            (E::Internal("i".into()), ApiErrorCode::Internal),
        ];
        for (err, expected_code) in cases {
            let api: ApiError = err.into();
            assert_eq!(api.code, expected_code);
        }
    }

    /// round-trip：错误消息透传不丢失。
    #[test]
    fn error_to_api_error_preserves_message() {
        let api: ApiError = E::Internal("boom-细节".into()).into();
        assert_eq!(api.message, "boom-细节");
    }

    /// 覆盖 `From<HandlerError> for ApiGatewayError` 的身份映射（NexHub 契约桥接）。
    #[test]
    fn handler_error_maps_identity_to_api_gateway_error() {
        use crate::gateway::HandlerError as H;
        let e: E = H::Unauthorized("u".into()).into();
        assert!(matches!(e, E::Unauthorized(m) if m == "u"));
        let e: E = H::Internal("i".into()).into();
        assert!(matches!(e, E::Internal(m) if m == "i"));
        // 消息透传到 ApiError 不丢失
        let api: ApiError = E::from(H::Internal("db down".into())).into();
        assert_eq!(api.message, "db down");
        assert_eq!(api.code, ApiErrorCode::Internal);
    }
}
