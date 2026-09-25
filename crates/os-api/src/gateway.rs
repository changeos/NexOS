//! 网关核心——Axum REST 入口与组件路由注册（规划文档 §3.6 / §9.1#10）
//!
//! 设计：内嵌网关（不独立成层）。各业务组件实现 `RouteHandler` 注册自己的路由，
//! 网关在启动时聚合所有路由表并对外提供统一 REST 入口。认证身份复用
//! `os_security::Principal`。
//!
//! NexHub 独立化（审计 §6）：`HttpMethod`/`RouteSpec`/`ApiResponse`/`HandlerError`
//! 下沉 `os_common::gateway`（此处原位再导出保持类型同一）；os-nexhub 的契约
//! handler 经下方 blanket impl 桥接进本网关。

use async_trait::async_trait;
use os_security::Principal;
use serde::{Deserialize, Serialize};

// ----------------------------------------------------------------------------
// 网关契约再导出（NexHub 独立化，审计 docs/COMPONENT_INDEPENDENCE_AUDIT.md §6.2）
// ----------------------------------------------------------------------------
//
// `HttpMethod` / `RouteSpec` / `ApiResponse` / `HandlerError` 已下沉 os-common
// `gateway` 模块（零 os-security/rusqlite 依赖，任何领域 crate 可实现自己的
// handler——第一个消费者是 os-nexhub）。这里原位再导出保证**类型同一**：
// 本 crate 其余 handler / 路由表 / 中间件的既有引用零改动。
//
// 完整 `ApiRequest`（含 `auth: Option<Principal>`）与 `TlsConfig` 留在本 crate：
// `Principal` 在 os-security，而 os-security 依赖 os-common，随契约下沉会成环。
// 契约版 `ApiRequest`（无 auth 字段）见 `os_common::gateway`。
pub use os_common::gateway::{ApiResponse, HandlerError, HttpMethod, RouteSpec};

// ----------------------------------------------------------------------------
// 请求 / 响应
// ----------------------------------------------------------------------------

/// 经网关的 API 请求（中间件可读写）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiRequest {
    /// HTTP 方法
    pub method: HttpMethod,
    /// 路径（含 query）
    pub path: String,
    /// 头部（开放结构）
    pub headers: serde_json::Value,
    /// 请求体（开放结构）
    pub body: serde_json::Value,
    /// 认证身份（None = 匿名；AuthMiddleware 解析后填充）
    pub auth: Option<Principal>,
}

// ----------------------------------------------------------------------------
// TLS
// ----------------------------------------------------------------------------

/// TLS 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    /// 证书路径（PEM）
    pub cert_path: String,
    /// 私钥路径（PEM）
    pub key_path: String,
}

// ----------------------------------------------------------------------------
// RouteHandler trait（async，每组件实现）
// ----------------------------------------------------------------------------

/// 路由处理器——每个业务组件实现它，把自己的路由注册进网关。
///
/// 实现者：`StorageRouteHandler` / `ComputeRouteHandler` 等各组件适配器。
/// 经 `Box<dyn RouteHandler>` 注册到 Gateway，故用 `#[async_trait]` 保证 dyn 兼容（ADR-COMPAT-001）。
#[async_trait]
pub trait RouteHandler: Send + Sync {
    /// 声明本组件提供的路由列表。
    async fn routes(&self) -> Vec<RouteSpec>;

    /// 处理落到本组件的请求。
    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, crate::ApiGatewayError>;
}

// ----------------------------------------------------------------------------
// 契约桥接（装配层）：os_common::gateway::RouteHandler → 本 trait
// ----------------------------------------------------------------------------

/// 领域 crate 契约 handler 的装配桥接（NexHub 独立化，审计 §6.2 方案 1）。
///
/// os-nexhub 等外部 crate 的 handler 实现
/// [`os_common::gateway::RouteHandler`]（轻量契约：契约 `ApiRequest` 无 auth 字段 +
/// [`HandlerError`]），经本 blanket impl 自动获得本网关的 [`RouteHandler`] 实现：
///
/// - **请求**：装配层剥离 `auth` 字段后传入契约请求——鉴权已由中间件按
///   `RouteSpec.requires_auth/required_roles` 完成，契约 handler 不消费认证身份；
/// - **错误**：[`HandlerError`] → [`crate::ApiGatewayError`] 身份映射（见
///   `error.rs` 的 `From` 实现），外部错误输出零变化。
///
/// 本 crate 自有 handler（storage/compute/system 等 26 个）仍直接实现本 trait
/// （`ApiGatewayError` 版），零改动。
#[async_trait]
impl<H> RouteHandler for H
where
    H: os_common::gateway::RouteHandler,
{
    async fn routes(&self) -> Vec<RouteSpec> {
        H::routes(self).await
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, crate::ApiGatewayError> {
        let contract_req = os_common::gateway::ApiRequest {
            method: req.method,
            path: req.path,
            headers: req.headers,
            body: req.body,
        };
        H::handle(self, contract_req)
            .await
            .map_err(crate::ApiGatewayError::from)
    }
}

// ----------------------------------------------------------------------------
// Gateway trait（async，网关）
// ----------------------------------------------------------------------------

/// API 网关——聚合各组件路由，提供统一 REST 入口。
///
/// 实现者：`AxumGateway`（默认，基于 Axum + tower 中间件链）。
/// 方法经 `#[async_trait]` 包装以与实现块的 async fn 一致并保证 dyn 兼容（ADR-COMPAT-001）。
#[async_trait]
pub trait Gateway: Send + Sync {
    /// 注册组件路由处理器。
    async fn register_component(
        &self,
        component: &str,
        handler: Box<dyn RouteHandler>,
    ) -> Result<(), crate::ApiGatewayError>;

    /// 列出全部已注册路由（聚合自各组件）。
    async fn list_routes(&self) -> Vec<RouteSpec>;

    /// 启动监听（可选 TLS）。
    async fn start(
        &self,
        addr: &str,
        tls_config: Option<TlsConfig>,
    ) -> Result<(), crate::ApiGatewayError>;

    /// 停止监听。
    async fn stop(&self);
}

// ----------------------------------------------------------------------------
// 单元测——HTTP 基元 / 请求响应模型 serde 往返
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_method_serde_roundtrip() {
        // HttpMethod serde 标签为 SCREAMING_SNAKE_CASE
        for (m, s) in [
            (HttpMethod::Get, "GET"),
            (HttpMethod::Post, "POST"),
            (HttpMethod::Put, "PUT"),
            (HttpMethod::Delete, "DELETE"),
            (HttpMethod::Patch, "PATCH"),
        ] {
            let json = serde_json::to_string(&m).unwrap();
            assert_eq!(json, format!("\"{s}\""));
            let back: HttpMethod = serde_json::from_str(&json).unwrap();
            assert_eq!(back, m);
        }
    }

    #[test]
    fn http_method_all_variants_covered() {
        // 确认 5 个动词都有，且不重复
        let all = [
            HttpMethod::Get,
            HttpMethod::Post,
            HttpMethod::Put,
            HttpMethod::Delete,
            HttpMethod::Patch,
        ];
        let uniq: std::collections::HashSet<_> = all.iter().collect();
        assert_eq!(uniq.len(), 5);
    }

    #[test]
    fn route_spec_serde_roundtrip() {
        let r = RouteSpec {
            method: HttpMethod::Get,
            path: "/api/v1/pools/:id".into(),
            handler_component: "os-storage".into(),
            requires_auth: true,
            required_roles: vec!["admin".into(), "operator".into()],
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: RouteSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back.method, HttpMethod::Get);
        assert_eq!(back.path, "/api/v1/pools/:id");
        assert_eq!(back.handler_component, "os-storage");
        assert!(back.requires_auth);
        assert_eq!(back.required_roles, vec!["admin", "operator"]);
    }

    #[test]
    fn api_request_serde_roundtrip() {
        let req = ApiRequest {
            method: HttpMethod::Post,
            path: "/x?y=1".into(),
            headers: serde_json::json!({"h": "v"}),
            body: serde_json::json!({"k": 1}),
            auth: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: ApiRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.method, HttpMethod::Post);
        assert_eq!(back.path, "/x?y=1");
        assert_eq!(back.headers["h"], "v");
        assert_eq!(back.body["k"], 1);
        assert!(back.auth.is_none());
    }

    #[test]
    fn api_response_serde_roundtrip() {
        let resp = ApiResponse {
            status: 201,
            body: serde_json::json!({"created": true}),
            headers: serde_json::json!({"location": "/x/1"}),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: ApiResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.status, 201);
        assert_eq!(back.body["created"], true);
        assert_eq!(back.headers["location"], "/x/1");
    }

    #[test]
    fn tls_config_serde_roundtrip() {
        let cfg = TlsConfig {
            cert_path: "/etc/ssl/cert.pem".into(),
            key_path: "/etc/ssl/key.pem".into(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: TlsConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cert_path, "/etc/ssl/cert.pem");
        assert_eq!(back.key_path, "/etc/ssl/key.pem");
    }

    #[test]
    fn http_method_equality_and_hash() {
        // Copy + PartialEq + Eq + Hash：可作 HashMap 键
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(HttpMethod::Get);
        set.insert(HttpMethod::Get);
        set.insert(HttpMethod::Post);
        assert_eq!(set.len(), 2);
        // Copy 语义：赋值后互不影响比较
        let a = HttpMethod::Get;
        let b = a;
        assert_eq!(a, b);
    }
}
