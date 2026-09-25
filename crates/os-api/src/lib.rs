//! os-api —— 内嵌 HTTP 网关（Axum REST + WebSocket，tower 中间件链：TLS / 限流 / 认证 / 审计）。
//!
//! 定位（规划文档 §3.6 / §9.1#10）：
//! - Axum REST + WebSocket 网关，内嵌于 osd（不独立成层）
//! - tower 中间件链：TLS / 限流 / 认证 / 审计
//! - 各业务组件经 `RouteHandler` 注册路由，网关聚合对外
//! - WebSocket 推送事件/进度/通知（对接 os-core EventBus）
//!
//! 本 crate 仅定义契约（trait + 数据结构 + Error），实现由 owner agent 后续填充。
//!
//! 契约规范：数据路径 trait 用原生 `async fn in trait`（无 `#[async_trait]`），
//! lib 顶部统一 `#![allow(async_fn_in_trait)]`；自定义 `ApiGatewayError`，
//! 并实现 `From<ApiGatewayError> for os_common::ApiError`。
//!
//! # 模块
//!
//! - [`gateway`]：网关契约——[`Gateway`] / [`RouteHandler`] trait + `ApiRequest`/`RouteSpec`/`TlsConfig`；`HttpMethod`/`ApiResponse`/`HandlerError` 下沉 os-common 后原位再导出（NexHub 独立化），并提供契约 handler 桥接 blanket impl。
//! - [`middleware`]：中间件契约——[`Middleware`] trait + TLS/Auth/RateLimit/Audit 中间件定义。
//! - [`chain`]：中间件链编排——[`MiddlewareChain`] + 限流器（`SlidingWindowRateLimiter`/`StatefulRateLimiter`）。
//! - [`routing`]：路由表——`RouteRegistry` + `PathParams`。
//! - [`websocket`]：WebSocket hub 契约——[`WebSocketHub`] trait + `WsMessage`。
//! - [`http`]：Axum 路由装配——`build_router`/`dispatch_handler`/`ws_handler`/`GatewayState`。
//! - [`gateway_impl`]：进程内网关实现——`InProcessGateway`。
//! - [`handlers`]：业务组件 RouteHandler 适配器——`StorageRouteHandler` / `ComputeRouteHandler` / `SystemRouteHandler`（供 binary 入口装配）。NexHub 两大 handler（code_repo / nexhub_lobby）已抽到独立 crate `os-nexhub`（审计 §6）。
//! - [`middleware_impl`]：限流算法实现——`SlidingWindow`/`TokenBucket`。
//! - [`ws_impl`]：WebSocket hub 实现——`WsHub`。
//! - [`webui`]：Web UI 静态资源内嵌（rust-embed）——`get_asset`。
//! - [`error`]：`ApiGatewayError` / `ApiGatewayResult`。
//! - `mock`：测试桩（仅 `mock` feature）。
//!
//! # 关键 trait
//!
//! - [`Gateway`]：HTTP 网关总入口（start/handle/dispatch）。
//! - [`RouteHandler`]：业务组件路由注册抽象（各组件实现它挂自己的路由）。
//! - [`Middleware`]：中间件统一抽象（process 请求/响应，返回 `MiddlewareDecision`）。
//! - [`WebSocketHub`]：WebSocket 推送抽象（broadcast 到 session，对接 os-core EventBus）。
//!
//! # feature 门控
//!
//! - `mock`（默认关）：开启 `mock` 模块，导出 `MockGateway`/`MockRouteHandler`/`MockWebSocketHub` 供下游测试注入。

#![allow(async_fn_in_trait)]

pub mod chain;
pub mod error;
pub mod gateway;
pub mod gateway_impl;
pub mod handlers;
pub mod http;
pub mod middleware;
pub mod middleware_impl;
pub mod routing;
pub mod websocket;
/// Web UI 静态资源内嵌（rust-embed）：把 crates/os-api/static/ 编译进 binary。
pub mod webui;
pub mod ws_impl;

/// Mock 实现（feature gate `mock`，供下游 client 测试）。
#[cfg(feature = "mock")]
pub mod mock;
#[cfg(feature = "mock")]
pub use mock::{MockGateway, MockRouteHandler, MockWebSocketHub};

pub use chain::{AuditRecord, MiddlewareChain, SlidingWindowRateLimiter, StatefulRateLimiter};
pub use error::{ApiGatewayError, ApiGatewayResult};
pub use gateway::{
    ApiRequest, ApiResponse, Gateway, HandlerError, HttpMethod, RouteHandler, RouteSpec, TlsConfig,
};
pub use gateway_impl::InProcessGateway;
pub use http::{build_router, dispatch_handler, terminal_ws_handler, ws_handler, GatewayState};
pub use middleware::{
    AuditMiddleware, AuthMiddleware, Middleware, MiddlewareDecision, RateLimitMiddleware,
    TlsMiddleware,
};
pub use middleware_impl::{SlidingWindow, TokenBucket};
pub use routing::{PathParams, RouteRegistry};
pub use websocket::{WebSocketHub, WsMessage};
pub use ws_impl::WsHub;

// ----------------------------------------------------------------------------
// 日志辅助
// ----------------------------------------------------------------------------

/// 入站 p2p 消息 payload 的日志形态（审计 A4-1 / 快赢#5，2026-09-24 批 1/3）。
///
/// 联邦桥 recv 观测日志原先打全 payload——gossip digest（上限 64KB）、市场
/// 联邦条目广播、IM 大厅消息全文进 journald（噪音/IO/磁盘三重浪费）。现截断
/// 至 [`P2P_PAYLOAD_LOG_LIMIT`] 字节（不劈开多字节 UTF-8 字符）+ 总长标注。
///
/// 注：配套的 tracing subscriber 装配（快赢#8）不在本批——os-api binary 保持
/// eprintln → journald 的现行纪律，不装 subscriber。
pub const P2P_PAYLOAD_LOG_LIMIT: usize = 128;

/// 把 payload 序列化为截断日志字符串（≤128 字节原样；超限则截断 + 总长标注）。
#[must_use]
pub fn p2p_payload_for_log(payload: &serde_json::Value) -> String {
    let s = payload.to_string();
    if s.len() <= P2P_PAYLOAD_LOG_LIMIT {
        return s;
    }
    // 不劈开多字节 UTF-8 字符（截断点回退到字符边界）
    let mut end = P2P_PAYLOAD_LOG_LIMIT;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}…（payload 共 {} 字节，日志截断至 {} 字节）",
        &s[..end],
        s.len(),
        end
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p2p_payload_log_short_passthrough() {
        // 短 payload 原样输出（含边界：恰好 128 字节）
        let short = serde_json::json!({"kind": "ping"});
        let logged = p2p_payload_for_log(&short);
        assert_eq!(logged, short.to_string());
        let exact = serde_json::Value::String("x".repeat(126)); // 2 引号 + 126 = 128 字节
        assert_eq!(p2p_payload_for_log(&exact).len(), 128);
    }

    #[test]
    fn p2p_payload_log_long_truncated_with_length() {
        // 长帧：截断至 ≤128 字节 + 总长标注（不假定 serde_json 键序——
        // Map 按 BTreeMap 排序，截断头部可能是任意键）
        let big = serde_json::json!({"kind": "gossip", "digest": "x".repeat(4096)});
        let logged = p2p_payload_for_log(&big);
        assert!(logged.starts_with('{'), "仍以 JSON 起始: {logged}");
        assert!(!logged.ends_with('}'), "长帧不可能全文放行");
        assert!(logged.contains(&format!("payload 共 {} 字节", big.to_string().len())));
        assert!(logged.contains("日志截断"));
        // 截断段本身不超过上限
        let head = logged.split('…').next().unwrap_or_default();
        assert!(head.len() <= P2P_PAYLOAD_LOG_LIMIT);
    }

    #[test]
    fn p2p_payload_log_multibyte_boundary() {
        // 截断点落在多字节 UTF-8 字符中间时回退到字符边界（不 panic、不劈字）
        let big = serde_json::json!({"text": "汉".repeat(200)});
        let logged = p2p_payload_for_log(&big);
        assert!(logged.len() > 128);
        assert!(logged.contains("payload 共"));
        assert!(std::str::from_utf8(logged.as_bytes()).is_ok());
    }
}
