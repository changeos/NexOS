//! 统一 API 错误（对外 HTTP/WS 错误模型）
//!
//! 各 crate 的 Error 实现 `From<XxxError> for ApiError`；os-api 网关把任意 crate Error
//! 转为 ApiError 后序列化返回。错误码分域，前端据此做差异化提示。

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// API 错误码（分域 + 类别）
///
/// `Display` 输出的字符串与 serde 序列化的 `snake_case` 形态一致，
/// 使得日志/文本表示与前端收到的 JSON 错误码字段保持统一（呼应 §15.1 错误模型规范）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum ApiErrorCode {
    // —— 通用类 ——
    /// 资源不存在
    #[error("not_found")]
    NotFound,
    /// 输入参数非法
    #[error("invalid_input")]
    InvalidInput,
    /// 权限不足/未认证
    #[error("permission_denied")]
    PermissionDenied,
    /// 冲突（资源已存在/状态冲突）
    #[error("conflict")]
    Conflict,
    /// 限流
    #[error("rate_limited")]
    RateLimited,
    /// 上游依赖不可用（如 RPC 挂了/钱包未连接）
    #[error("upstream_unavailable")]
    UpstreamUnavailable,
    /// 内部错误
    #[error("internal")]
    Internal,

    // —— 领域类（可选细化）——
    /// HA 故障转移失败
    #[error("failover_failed")]
    FailoverFailed,
    /// 链上验证失败（签名无效/凭证不符/余额不足）
    #[error("chain_verification_failed")]
    ChainVerificationFailed,
    /// 高危操作待确认（需用户在 IM 内确认，见 §3.7.2）
    #[error("confirmation_required")]
    ConfirmationRequired,
}

/// 统一 API 错误
#[derive(Debug, Clone, Error, Serialize, Deserialize)]
#[error("[{code}] {message}")]
pub struct ApiError {
    pub code: ApiErrorCode,
    /// 人类可读消息（本地化由前端用 os-i18n 处理）
    pub message: String,
    /// 关联任务（便于前端追踪）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<os_core::TaskId>,
    /// 附加详情（开放结构）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl ApiError {
    /// 用指定错误码与消息构造（task_id / details 留空）
    pub fn new(code: ApiErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            task_id: None,
            details: None,
        }
    }
    /// 快捷构造"资源不存在"错误
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::new(ApiErrorCode::NotFound, msg)
    }
    /// 快捷构造"输入参数非法"错误
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::new(ApiErrorCode::InvalidInput, msg)
    }
    /// 快捷构造"权限不足/未认证"错误
    pub fn permission(msg: impl Into<String>) -> Self {
        Self::new(ApiErrorCode::PermissionDenied, msg)
    }
    /// 快捷构造"内部错误"错误（兜底类别，具体原因放 message）
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new(ApiErrorCode::Internal, msg)
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

// —— From 转换：os-core 错误 → ApiError ——
impl From<os_core::CoreError> for ApiError {
    fn from(e: os_core::CoreError) -> Self {
        ApiError::internal(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{ApiError, ApiErrorCode as C};

    #[test]
    fn error_code_display_covers_all_variants() {
        assert_eq!(C::NotFound.to_string(), "not_found");
        assert_eq!(C::InvalidInput.to_string(), "invalid_input");
        assert_eq!(C::PermissionDenied.to_string(), "permission_denied");
        assert_eq!(C::Conflict.to_string(), "conflict");
        assert_eq!(C::RateLimited.to_string(), "rate_limited");
        assert_eq!(C::UpstreamUnavailable.to_string(), "upstream_unavailable");
        assert_eq!(C::Internal.to_string(), "internal");
        assert_eq!(C::FailoverFailed.to_string(), "failover_failed");
        assert_eq!(
            C::ChainVerificationFailed.to_string(),
            "chain_verification_failed"
        );
        assert_eq!(C::ConfirmationRequired.to_string(), "confirmation_required");
    }

    #[test]
    fn api_error_display_format() {
        let e = ApiError::new(C::NotFound, "资源缺失");
        let s = format!("{}", e);
        assert!(s.contains("[not_found]"), "got: {s}");
        assert!(s.contains("资源缺失"), "got: {s}");
    }
}
