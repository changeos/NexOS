//! os-common —— API 通用层（统一错误码 / API 版本封装 / 通用 DTO）。
//!
//! 提供：`ApiError`（统一对外 API 错误码，serde 友好）、`Versioned` trait（API 版本规范，
//! 呼应 §12.3）、通用 DTO。
//!
//! 各 crate 的 Error 通过 `impl From<XxxError> for ApiError` 转换为统一 API 错误，
//! os-api 网关统一序列化返回前端。
//!
//! # 模块
//!
//! - [`error`]：`ApiError` / `ApiErrorCode` / `ApiResult`——对外错误码枚举（snake_case serde），
//!   各 crate Error 经 `From<XxxError> for ApiError` 汇聚到此。
//! - [`versioned`]：`Versioned` trait + `VersionedEnvelope<T>` 封装（`#[serde(flatten)]`，
//!   统一带 `api_version` 字段），常量 [`CURRENT_API_VERSION`]。
//!
//! - [`chain_auth`]：链上身份认证内核（`ChainAuth` nonce/token 桶 + k256 验签 +
//!   EVM 展示名派生）——IM 与 NexHub 共享的挑战-签名模式（设计
//!   docs/MEDIA_GEN_AND_CHAIN_AUTH.md §C，从 os-api `ImAuth` 泛化下沉）。
//!
//! - [`gateway`]：网关契约（NexHub 独立化下沉，审计 docs/COMPONENT_INDEPENDENCE_AUDIT.md
//!   §6.2 方案 1）——`RouteHandler` trait + `HttpMethod`/`RouteSpec`/`ApiRequest`
//!   （契约版，无 auth）/`ApiResponse`/`HandlerError`，供 os-nexhub 等领域 crate 自带
//!   handler；os-api 装配层桥接到网关版 `RouteHandler`（`ApiGatewayError`）。
//!
//! # 关键 trait
//!
//! - [`Versioned`]：标记 trait，默认实现返回 [`CURRENT_API_VERSION`]；DTO 实现它即声明自身的 API 版本。
//! - [`gateway::RouteHandler`]：领域 crate 路由注册抽象（轻量契约版，错误为 `HandlerError`）。
//!
//! # 错误码约定
//!
//! `ApiErrorCode` 序列化为 snake_case（如 `"permission_denited"`），与 `Display` 一致，
//! 拒绝 PascalCase 输入——前端按字符串字面量匹配。可选字段 `task_id` / `details` 为 `None`
//! 时被 serde 跳过。

pub mod chain_auth;
pub mod error;
pub mod gateway;
pub mod versioned;

pub use chain_auth::ChainAuth;
pub use error::{ApiError, ApiErrorCode, ApiResult};
pub use gateway::{ApiRequest, ApiResponse, HandlerError, HttpMethod, RouteHandler, RouteSpec};
pub use versioned::{Versioned, VersionedEnvelope, CURRENT_API_VERSION};

// ============================================================================
// 测试（review2 P6 / R1 P6）：os-common 曾长期零测试（纯 DTO/构造器）。
// 这里补一组冒烟测，零成本回归保险——覆盖：
//   - ApiErrorCode 各变体 Display 与 serde snake_case 双向一致
//   - ApiError 构造器（new / not_found / invalid / permission / internal）
//   - From<CoreError> for ApiError 转换
//   - VersionedEnvelope::new 与 api_version
//   - Versioned trait 默认实现
// 目标：os-common lib unittest 从 0 → ≥5（见 docs/REVIEW.md §R2.5 / §R2.3 P-R2-3）。
// ============================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use os_core::CoreError;

    // —— ApiErrorCode：Display 与 serde snake_case 一致性 ——

    #[test]
    fn error_code_display_matches_expected_snake_case() {
        // Display 输出应与 #[error("...")] 字面量一致（日志/前端契约）
        assert_eq!(ApiErrorCode::NotFound.to_string(), "not_found");
        assert_eq!(ApiErrorCode::InvalidInput.to_string(), "invalid_input");
        assert_eq!(
            ApiErrorCode::PermissionDenied.to_string(),
            "permission_denied"
        );
        assert_eq!(ApiErrorCode::Conflict.to_string(), "conflict");
        assert_eq!(ApiErrorCode::RateLimited.to_string(), "rate_limited");
        assert_eq!(
            ApiErrorCode::UpstreamUnavailable.to_string(),
            "upstream_unavailable"
        );
        assert_eq!(ApiErrorCode::Internal.to_string(), "internal");
        assert_eq!(ApiErrorCode::FailoverFailed.to_string(), "failover_failed");
        assert_eq!(
            ApiErrorCode::ChainVerificationFailed.to_string(),
            "chain_verification_failed"
        );
        assert_eq!(
            ApiErrorCode::ConfirmationRequired.to_string(),
            "confirmation_required"
        );
    }

    #[test]
    fn error_code_serde_roundtrip_snake_case() {
        // serde 序列化为 snake_case（#[serde(rename_all = "snake_case")]），且与 Display 一致
        for code in [
            ApiErrorCode::NotFound,
            ApiErrorCode::InvalidInput,
            ApiErrorCode::PermissionDenied,
            ApiErrorCode::Conflict,
            ApiErrorCode::RateLimited,
            ApiErrorCode::UpstreamUnavailable,
            ApiErrorCode::Internal,
            ApiErrorCode::FailoverFailed,
            ApiErrorCode::ChainVerificationFailed,
            ApiErrorCode::ConfirmationRequired,
        ] {
            let json = serde_json::to_string(&code).expect("serialize");
            // 序列化结果应为 "snake_case" 字面量（与 Display 一致）
            assert_eq!(json, format!("\"{}\"", code));
            let back: ApiErrorCode = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, code, "serde roundtrip mismatch for {code}");
        }
    }

    #[test]
    fn error_code_serde_uses_snake_case_not_pascal() {
        // 关键契约：PascalCase 变体必须序列化为 snake_case，拒绝 PascalCase 输入
        let json = serde_json::to_string(&ApiErrorCode::PermissionDenied).unwrap();
        assert_eq!(json, "\"permission_denied\"");
        assert!(serde_json::from_str::<ApiErrorCode>("\"PermissionDenied\"").is_err());
        assert!(serde_json::from_str::<ApiErrorCode>("\"permission_denied\"").is_ok());
    }

    // —— ApiError 构造器 ——

    #[test]
    fn api_error_new_sets_code_and_message_only() {
        let e = ApiError::new(ApiErrorCode::Conflict, "已存在");
        assert_eq!(e.code, ApiErrorCode::Conflict);
        assert_eq!(e.message, "已存在");
        assert!(e.task_id.is_none());
        assert!(e.details.is_none());
    }

    #[test]
    fn api_error_shortcut_constructors_map_to_correct_codes() {
        assert_eq!(ApiError::not_found("x").code, ApiErrorCode::NotFound);
        assert_eq!(ApiError::invalid("x").code, ApiErrorCode::InvalidInput);
        assert_eq!(
            ApiError::permission("x").code,
            ApiErrorCode::PermissionDenied
        );
        assert_eq!(ApiError::internal("x").code, ApiErrorCode::Internal);
        // 消息被透传
        assert_eq!(ApiError::not_found("missing").message, "missing");
        assert_eq!(ApiError::invalid("bad input").message, "bad input");
    }

    #[test]
    fn api_error_display_format() {
        // #[error("[{code}] {message}")]
        let e = ApiError::new(ApiErrorCode::Internal, "boom");
        assert_eq!(format!("{e}"), "[internal] boom");
    }

    #[test]
    fn api_error_serde_skips_optional_none_fields() {
        // task_id / details 为 None 时不应出现在 JSON
        let e = ApiError::not_found("缺");
        let json = serde_json::to_string(&e).unwrap();
        assert!(
            !json.contains("task_id"),
            "none task_id must be skipped: {json}"
        );
        assert!(
            !json.contains("details"),
            "none details must be skipped: {json}"
        );
        assert!(json.contains("\"code\":\"not_found\""));
        assert!(json.contains("\"message\":\"缺\""));
    }

    #[test]
    fn api_error_serde_roundtrip_with_optionals() {
        let mut e = ApiError::internal("oops");
        e.task_id = Some(os_core::TaskId(os_core::Uuid::nil()));
        e.details = Some(serde_json::json!({"k": 42}));
        let json = serde_json::to_string(&e).unwrap();
        let back: ApiError = serde_json::from_str(&json).unwrap();
        assert_eq!(back.code, e.code);
        assert_eq!(back.message, e.message);
        assert_eq!(back.task_id, e.task_id);
        assert_eq!(back.details, e.details);
    }

    // —— From<CoreError> for ApiError ——

    #[test]
    fn from_core_error_maps_to_internal_with_message() {
        let core = CoreError::EventBus("channel closed".into());
        let api: ApiError = core.into();
        assert_eq!(api.code, ApiErrorCode::Internal);
        // CoreError 的 Display 文本被带入 message（to_string）
        assert!(
            api.message.contains("channel closed"),
            "msg={}",
            api.message
        );
    }

    #[test]
    fn from_core_error_serde_variant_propagates() {
        let core = CoreError::Internal("xxx".into());
        let api: ApiError = core.into();
        assert_eq!(api.code, ApiErrorCode::Internal);
        assert!(api.message.contains("xxx"));
    }

    // —— VersionedEnvelope ——

    #[test]
    fn versioned_envelope_new_stamps_current_api_version() {
        let env = VersionedEnvelope::new(42_u32);
        assert_eq!(env.api_version, CURRENT_API_VERSION);
        assert_eq!(env.data, 42);
    }

    #[test]
    fn versioned_envelope_flattens_data_on_serialize() {
        // #[serde(flatten)] data：JSON 顶层应同时含 api_version 与 data 字段
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Payload {
            name: String,
        }
        let env = VersionedEnvelope::new(Payload { name: "abc".into() });
        let json = serde_json::to_string(&env).unwrap();
        assert!(
            json.contains("\"api_version\""),
            "missing api_version: {json}"
        );
        assert!(
            json.contains("\"name\":\"abc\""),
            "data not flattened: {json}"
        );
        // 反序列化回来仍能复原
        let back: VersionedEnvelope<Payload> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.api_version, CURRENT_API_VERSION);
        assert_eq!(back.data, env.data);
    }

    // —— Versioned trait 默认实现 ——

    #[test]
    fn versioned_default_impl_returns_current_api_version() {
        struct DummyDto;
        impl Versioned for DummyDto {}

        let d = DummyDto;
        assert_eq!(d.api_version(), CURRENT_API_VERSION);
        assert_eq!(Versioned::api_version(&d), CURRENT_API_VERSION);
    }
}
