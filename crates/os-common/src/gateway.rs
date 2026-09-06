//! 网关契约——RouteHandler trait + HTTP 基元（从 os-api gateway.rs 下沉，NexHub 独立化）。
//!
//! 定位：**领域 crate 自带 RouteHandler 的公共契约**。os-api 仍是组合根（网关装配 /
//! 中间件 / Git Smart HTTP CGI），但 handler 本体可以长在任何 crate（第一个消费者是
//! os-nexhub：代码仓库中心 + 大厅发现层），经本契约与网关对接：
//!
//! ```text
//! os-nexhub handler ──实现──▶ os_common::gateway::RouteHandler（本模块，轻量契约）
//!        ▲                                  │ 装配层桥接（os-api gateway.rs blanket impl）
//!        │                                  ▼
//!   /api/v1/coderepo/*  ◀── HTTP ── os-api 网关（RouteHandler + ApiGatewayError）
//! ```
//!
//! # 与 os-api 侧 `gateway` 模块的分工（审计 docs/COMPONENT_INDEPENDENCE_AUDIT.md §6.2）
//!
//! - **本模块（可下沉）**：`HttpMethod` / `RouteSpec` / `ApiResponse` /
//!   契约 `ApiRequest`（**无 `auth` 字段**）/ `HandlerError`（轻量）/ `RouteHandler`
//!   trait——零 os-security / rusqlite 依赖，任何 crate 都可实现自己的 handler。
//! - **os-api 侧保留**：完整 `ApiRequest`（含 `auth: Option<Principal>`——`Principal`
//!   在 os-security，而 os-security 依赖 os-common，下沉会成环）、`TlsConfig`、
//!   `Gateway` trait 与 os-api 自己的 `RouteHandler`（错误类型为 `ApiGatewayError`，
//!   含 `From<rusqlite::Error>`）。os-api 对 `HttpMethod`/`RouteSpec`/`ApiResponse`
//!   做原位再导出保证类型同一，其余 handler 零改动。
//! - **错误模型**：handler 层错误用本模块的 [`HandlerError`]（仅
//!   `Unauthorized`/`Internal`，无持久化层 From）；os-api 装配层做
//!   `HandlerError → ApiGatewayError` 身份映射，外部行为不变。
//!
//! # 鉴权语义
//!
//! 契约 `ApiRequest` 不携带认证身份：鉴权由网关装配层按 [`RouteSpec::requires_auth`]
//! / [`RouteSpec::required_roles`] 在中间件完成，handler 收到的请求已过闸。需要身份
//! 的 handler（如 IM）仍应长在 os-api 侧消费完整 `ApiRequest`。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// ----------------------------------------------------------------------------
// HTTP 基元
// ----------------------------------------------------------------------------

/// HTTP 方法（ Subset，网关仅支持常用动词）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HttpMethod {
    /// GET
    Get,
    /// POST
    Post,
    /// PUT
    Put,
    /// DELETE
    Delete,
    /// PATCH
    Patch,
}

// ----------------------------------------------------------------------------
// 路由声明
// ----------------------------------------------------------------------------

/// 单条路由规格（由组件声明）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteSpec {
    /// HTTP 方法
    pub method: HttpMethod,
    /// 路径（如 `/api/v1/storage/pools`，支持 Axum 风格参数）
    pub path: String,
    /// 提供该路由的组件名（如 `os-storage`）
    pub handler_component: String,
    /// 是否需要认证
    pub requires_auth: bool,
    /// 需要的角色（如 `["admin"]`；空 = 已认证即可）
    pub required_roles: Vec<String>,
}

// ----------------------------------------------------------------------------
// 请求 / 响应
// ----------------------------------------------------------------------------

/// 经网关的 API 请求（契约版，供领域 crate 的 handler 消费）。
///
/// 与 os-api 侧完整 `ApiRequest` 的差异：**无 `auth` 字段**——认证身份
/// （`os_security::Principal`）留在 os-security，无法随本模块下沉（依赖环）；
/// 鉴权由网关装配层按 `RouteSpec` 完成，handler 无需感知身份（NexHub 两 handler
/// 均不消费 `auth`）。os-api 装配层在派发时剥离 `auth` 字段后传入。
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
}

/// API 响应（中间件可改写）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiResponse {
    /// HTTP 状态码
    pub status: u16,
    /// 响应体（开放结构）
    pub body: serde_json::Value,
    /// 响应头部（开放结构）
    pub headers: serde_json::Value,
}

// ----------------------------------------------------------------------------
// HandlerError（轻量 handler 错误）
// ----------------------------------------------------------------------------

/// 路由处理器错误——领域 crate handler 层的轻量错误类型。
///
/// 只覆盖 handler 能产生的错误形态（未授权 / 内部错误），**不含**持久化层
/// `From<rusqlite::Error>` 等重依赖转换（那是 os-api `ApiGatewayError` 的职责，
/// 避免本 crate 被拖入 rusqlite 依赖——审计 §6.2 方案 1）。DB 错误由各 handler
/// 自行 `map_err` 为 [`HandlerError::Internal`]（消息格式自定，通常与既有
/// `ApiGatewayError` 的映射保持一致以稳定日志输出）。
#[derive(Debug, thiserror::Error)]
pub enum HandlerError {
    /// 未认证/权限不足
    #[error("未授权: {0}")]
    Unauthorized(String),

    /// 内部错误（IO / 子进程 / 序列化 / 数据库等一切降级路径）
    #[error("内部错误: {0}")]
    Internal(String),
}

// ----------------------------------------------------------------------------
// RouteHandler trait（async，每组件实现——契约版）
// ----------------------------------------------------------------------------

/// 路由处理器——每个业务组件实现它，把自己的路由注册进网关（契约版）。
///
/// 实现者：os-nexhub 的 `CodeRepoRouteHandler` / `NexHubLobbyRouteHandler` 等
/// 长在领域 crate 里的 handler。经 `Box<dyn RouteHandler>` 注册到网关，故用
/// `#[async_trait]` 保证 dyn 兼容（ADR-COMPAT-001）。os-api 装配层为其提供到
/// os-api 网关 `RouteHandler`（`ApiGatewayError` 版）的桥接实现。
#[async_trait]
pub trait RouteHandler: Send + Sync {
    /// 声明本组件提供的路由列表。
    async fn routes(&self) -> Vec<RouteSpec>;

    /// 处理落到本组件的请求。
    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, HandlerError>;
}

// ----------------------------------------------------------------------------
// 单元测——HTTP 基元 / 请求响应模型 serde 往返（随类型从 os-api gateway.rs 迁移）
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
        // 契约版无 auth 字段：JSON 不应出现 "auth" 键
        let req = ApiRequest {
            method: HttpMethod::Post,
            path: "/x?y=1".into(),
            headers: serde_json::json!({"h": "v"}),
            body: serde_json::json!({"k": 1}),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("auth"), "契约请求不应有序列化 auth: {json}");
        let back: ApiRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.method, HttpMethod::Post);
        assert_eq!(back.path, "/x?y=1");
        assert_eq!(back.headers["h"], "v");
        assert_eq!(back.body["k"], 1);
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
    fn handler_error_display_covers_all_variants() {
        assert!(format!("{}", HandlerError::Unauthorized("u".into())).contains("未授权"));
        assert!(format!("{}", HandlerError::Internal("i".into())).contains("内部错误"));
    }
}
