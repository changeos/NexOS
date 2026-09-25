//! Mock 实现（feature gate `mock`）——供下游 agent（client）测试用。
//!
//! 约定（_conventions.md §5）：实现完整 trait（不 panic 的默认返回），
//! 提供 builder 构造器，纯内存、确定性。
//!
//! 提供：`MockGateway` / `MockRouteHandler` / `MockWebSocketHub`。

#![cfg(feature = "mock")]

use std::sync::Mutex;

use async_trait::async_trait;

use crate::gateway::{ApiRequest, ApiResponse, Gateway, RouteHandler, RouteSpec, TlsConfig};
use crate::websocket::{WebSocketHub, WsMessage};
use crate::ApiGatewayError;
use os_core::SubscriptionId;

// ============================================================
// MockRouteHandler
// ============================================================

/// Mock `RouteHandler`——可配置声明的路由与固定响应。
pub struct MockRouteHandler {
    component: String,
    routes: Vec<RouteSpec>,
    response: Mutex<ApiResponse>,
    invoke_count: Mutex<u32>,
}

impl MockRouteHandler {
    /// 构造：声明一组路由，handle 时回固定响应（默认 200）。
    pub fn new(component: impl Into<String>, routes: Vec<RouteSpec>) -> Self {
        Self {
            component: component.into(),
            routes,
            response: Mutex::new(ApiResponse {
                status: 200,
                body: serde_json::json!({"mock": true}),
                headers: serde_json::json!({}),
            }),
            invoke_count: Mutex::new(0),
        }
    }

    /// 注入 handle 返回的响应。
    pub fn with_response(self, resp: ApiResponse) -> Self {
        *self.response.lock().unwrap() = resp;
        self
    }

    /// handle 被调用次数。
    pub fn invoke_count(&self) -> u32 {
        *self.invoke_count.lock().unwrap()
    }

    /// 组件名。
    pub fn component(&self) -> &str {
        &self.component
    }
}

#[async_trait]
impl RouteHandler for MockRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        self.routes.clone()
    }
    async fn handle(&self, _req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        *self.invoke_count.lock().unwrap() += 1;
        Ok(self.response.lock().unwrap().clone())
    }
}

// ============================================================
// MockGateway
// ============================================================

/// Mock `Gateway`——包装 [`crate::gateway_impl::InProcessGateway`]，
/// 保留 register/list/start/stop 语义，便于下游注入测试。
pub struct MockGateway {
    inner: crate::gateway_impl::InProcessGateway,
}

impl MockGateway {
    /// 创建空 mock 网关。
    pub fn new() -> Self {
        Self {
            inner: crate::gateway_impl::InProcessGateway::new(),
        }
    }

    /// 内部分发（透传，便于下游测路由命中）。
    pub async fn dispatch(&self, req: ApiRequest) -> (ApiResponse, Option<RouteSpec>) {
        self.inner.dispatch(req).await
    }

    /// 取中间件链长度。
    pub fn middleware_count(&self) -> usize {
        self.inner.middleware_count()
    }

    /// 组件数。
    pub fn component_count(&self) -> usize {
        self.inner.component_count()
    }

    /// 是否监听中。
    pub fn is_listening(&self) -> bool {
        self.inner.is_listening()
    }
}

impl Default for MockGateway {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Gateway for MockGateway {
    async fn register_component(
        &self,
        component: &str,
        handler: Box<dyn RouteHandler>,
    ) -> Result<(), ApiGatewayError> {
        self.inner.register_component(component, handler).await
    }
    async fn list_routes(&self) -> Vec<RouteSpec> {
        self.inner.list_routes().await
    }
    async fn start(&self, addr: &str, tls: Option<TlsConfig>) -> Result<(), ApiGatewayError> {
        self.inner.start(addr, tls).await
    }
    async fn stop(&self) {
        self.inner.stop().await
    }
}

// ============================================================
// MockWebSocketHub
// ============================================================

/// Mock `WebSocketHub`——包装 [`crate::ws_impl::WsHub`]，
/// 暴露订阅/广播/定向推送测试入口。
pub struct MockWebSocketHub {
    inner: crate::ws_impl::WsHub,
}

impl MockWebSocketHub {
    /// 默认构造（通道容量 256）。
    pub fn new() -> Self {
        Self {
            inner: crate::ws_impl::WsHub::default(),
        }
    }

    /// 当前活跃订阅数。
    pub fn subscriber_count(&self) -> usize {
        self.inner.subscriber_count()
    }

    /// 某用户订阅数。
    pub fn subscriber_count_for(&self, user: &str) -> usize {
        self.inner.subscriber_count_for(user)
    }
}

impl Default for MockWebSocketHub {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl WebSocketHub for MockWebSocketHub {
    async fn broadcast(&self, msg: WsMessage) -> Result<(), ApiGatewayError> {
        self.inner.broadcast(msg).await
    }
    async fn send_to(&self, user: &str, msg: WsMessage) -> Result<(), ApiGatewayError> {
        self.inner.send_to(user, msg).await
    }
    async fn subscribe(&self, user: &str) -> Result<SubscriptionId, ApiGatewayError> {
        self.inner.subscribe(user).await
    }
    async fn unsubscribe(&self, id: SubscriptionId) {
        self.inner.unsubscribe(id).await
    }
}

// ============================================================
// 单元测——Mock builder 行为
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::HttpMethod;
    use os_core::{Event, Topic};

    fn route(method: HttpMethod, path: &str, comp: &str) -> RouteSpec {
        RouteSpec {
            method,
            path: path.to_string(),
            handler_component: comp.to_string(),
            requires_auth: false,
            required_roles: vec![],
        }
    }

    #[tokio::test]
    async fn mock_route_handler_returns_configured_response() {
        let h = MockRouteHandler::new(
            "storage",
            vec![route(HttpMethod::Get, "/api/v1/pools", "storage")],
        )
        .with_response(ApiResponse {
            status: 201,
            body: serde_json::json!({"created": true}),
            headers: serde_json::json!({}),
        });
        assert_eq!(h.routes().await.len(), 1);
        let resp = h
            .handle(ApiRequest {
                method: HttpMethod::Get,
                path: "/api/v1/pools".to_string(),
                headers: serde_json::json!({}),
                body: serde_json::Value::Null,
                auth: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        assert_eq!(h.invoke_count(), 1);
    }

    #[tokio::test]
    async fn mock_gateway_aggregates_and_dispatches() {
        let gw = MockGateway::new();
        let h = MockRouteHandler::new(
            "compute",
            vec![route(HttpMethod::Get, "/api/v1/vms", "compute")],
        );
        gw.register_component("compute", Box::new(h)).await.unwrap();
        assert_eq!(gw.list_routes().await.len(), 1);
        let (resp, route) = gw
            .dispatch(ApiRequest {
                method: HttpMethod::Get,
                path: "/api/v1/vms".to_string(),
                headers: serde_json::json!({}),
                body: serde_json::Value::Null,
                auth: None,
            })
            .await;
        assert_eq!(resp.status, 200);
        assert_eq!(route.unwrap().handler_component, "compute");
    }

    #[tokio::test]
    async fn mock_gateway_start_stop() {
        let gw = MockGateway::new();
        assert!(!gw.is_listening());
        gw.start("0.0.0.0:9000", None).await.unwrap();
        assert!(gw.is_listening());
        gw.stop().await;
        assert!(!gw.is_listening());
    }

    #[tokio::test]
    async fn mock_ws_hub_subscribe_and_broadcast() {
        let hub = MockWebSocketHub::new();
        let _id = hub.subscribe("alice").await.unwrap();
        assert_eq!(hub.subscriber_count(), 1);
        let msg = WsMessage::Event {
            event: Event::new("e", Topic::System, "src"),
        };
        hub.broadcast(msg).await.unwrap();
    }

    // —— 覆盖率补测：builder 方法 + 默认响应 + mock 计数 ——

    #[tokio::test]
    async fn mock_route_handler_default_response_and_component() {
        // 默认响应（未调 with_response）：status=200, body={"mock": true}
        let h = MockRouteHandler::new(
            "storage",
            vec![route(HttpMethod::Get, "/api/v1/pools", "storage")],
        );
        assert_eq!(h.component(), "storage");
        assert_eq!(h.invoke_count(), 0);
        let resp = h
            .handle(ApiRequest {
                method: HttpMethod::Get,
                path: "/api/v1/pools".to_string(),
                headers: serde_json::json!({}),
                body: serde_json::Value::Null,
                auth: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["mock"], true);
        assert_eq!(h.invoke_count(), 1);
        // 再次 handle 累加计数
        let _ = h
            .handle(ApiRequest {
                method: HttpMethod::Get,
                path: "/x".to_string(),
                headers: serde_json::json!({}),
                body: serde_json::Value::Null,
                auth: None,
            })
            .await;
        assert_eq!(h.invoke_count(), 2);
    }

    #[tokio::test]
    async fn mock_gateway_default_state() {
        let gw = MockGateway::new();
        assert_eq!(gw.middleware_count(), 0);
        assert_eq!(gw.component_count(), 0);
        assert!(!gw.is_listening());
    }

    #[tokio::test]
    async fn mock_gateway_dispatch_404_on_unmatched() {
        let gw = MockGateway::new();
        let (resp, route) = gw
            .dispatch(ApiRequest {
                method: HttpMethod::Get,
                path: "/nope".to_string(),
                headers: serde_json::json!({}),
                body: serde_json::Value::Null,
                auth: None,
            })
            .await;
        assert_eq!(resp.status, 404);
        assert!(route.is_none());
    }

    #[tokio::test]
    async fn mock_ws_hub_default_is_empty() {
        let hub = MockWebSocketHub::default();
        assert_eq!(hub.subscriber_count(), 0);
        assert_eq!(hub.subscriber_count_for("alice"), 0);
    }

    #[tokio::test]
    async fn mock_ws_hub_send_to_unknown_user_ok() {
        let hub = MockWebSocketHub::new();
        let msg = WsMessage::Event {
            event: Event::new("e", Topic::System, "src"),
        };
        // 给未订阅用户推送 → trait 仍返回 Ok
        hub.send_to("nobody", msg).await.unwrap();
    }

    #[tokio::test]
    async fn mock_ws_hub_unsubscribe_via_trait() {
        let hub = MockWebSocketHub::new();
        let id = hub.subscribe("alice").await.unwrap();
        assert_eq!(hub.subscriber_count(), 1);
        hub.unsubscribe(id).await;
        assert_eq!(hub.subscriber_count(), 0);
    }
}
