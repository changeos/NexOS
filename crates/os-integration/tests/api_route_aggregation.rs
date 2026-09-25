//! 场景 6：API 路由聚合（integration-agent 规格书 §3 场景 6）
//!
//! 链路：os-api Gateway 注册多个 RouteHandler（storage / network / compute 各组件）
//! → 路由匹配（按 method+path 命中）→ 分发（调对应组件 handle）→ 响应聚合。
//!
//! 重点验证：
//! - 多组件路由注册：三个组件（storage/network/compute）各自的 RouteHandler
//!   经 `Gateway::register_component(Box<dyn RouteHandler>)` 注册到 InProcessGateway，
//!   `list_routes()` 应聚合三组件全部路由（去重 + 冲突检测）。
//! - 路由匹配：dispatch 按 (method, path) 精确命中目标组件，`matched.handler_component`
//!   与请求路径一致；未注册路由返回 404。
//! - 分发与响应聚合：每个 RouteHandler 各自产出 ApiResponse，dispatch 返回时 body 带
//!   组件标识，验证「同一入口聚合多组件响应」。
//! - 跨 crate 类型桥接：RouteSpec.handler_component 与 RouteHandler 实例的归属一致；
//!   ApiGatewayError::RouteConflict 在重复路由注册时被触发。
//!
//! 红线：不改 trait 签名 / 其他 crate 源码——本测试只用 os-api 已暴露的 Gateway /
//! RouteHandler / InProcessGateway（feature `mock` 不需要，本身即纯进程实现）。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

use os_api::error::ApiGatewayError;
use os_api::gateway::{ApiRequest, ApiResponse, Gateway, HttpMethod, RouteHandler, RouteSpec};
use os_api::gateway_impl::InProcessGateway;

// ----------------------------------------------------------------------------
// 通用 MockRouteHandler：声明若干路由，handle 时回 200 + body 带 component 名 + 路由参数。
// 关键：经 Box<dyn RouteHandler> 注入 Gateway，验证 dyn 兼容路径（ADR-COMPAT-001）。
// ----------------------------------------------------------------------------

struct MockRouteHandler {
    component: &'static str,
    routes: Vec<RouteSpec>,
    /// handle 调用计数（断言「分发真的命中了目标组件」）
    call_count: Arc<AtomicU32>,
}

impl MockRouteHandler {
    fn new(component: &'static str, routes: Vec<RouteSpec>, call_count: Arc<AtomicU32>) -> Self {
        Self {
            component,
            routes,
            call_count,
        }
    }
}

#[async_trait]
impl RouteHandler for MockRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        self.routes.clone()
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        Ok(ApiResponse {
            status: 200,
            body: serde_json::json!({
                "component": self.component,
                "method": format!("{:?}", req.method),
                "path": req.path,
                "echo": req.body,
            }),
            headers: serde_json::json!({
                "X-Served-By": self.component,
            }),
        })
    }
}

// ----------------------------------------------------------------------------
// 辅助：构造 RouteSpec（精简签名）
// ----------------------------------------------------------------------------

fn spec(method: HttpMethod, path: &str, comp: &str) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: comp.to_string(),
        requires_auth: false,
        required_roles: vec![],
    }
}

fn req(method: HttpMethod, path: &str) -> ApiRequest {
    ApiRequest {
        method,
        path: path.to_string(),
        headers: serde_json::json!({}),
        body: serde_json::Value::Null,
        auth: None,
    }
}

/// 构造三组件的完整路由集合（storage 3 条 / network 2 条 / compute 3 条 = 8 条）。
fn three_component_routes() -> (Vec<RouteSpec>, Vec<RouteSpec>, Vec<RouteSpec>) {
    let storage = vec![
        spec(HttpMethod::Get, "/api/v1/storage/pools", "storage"),
        spec(HttpMethod::Get, "/api/v1/storage/pools/:id", "storage"),
        spec(HttpMethod::Post, "/api/v1/storage/datasets", "storage"),
    ];
    let network = vec![
        spec(HttpMethod::Get, "/api/v1/network/interfaces", "network"),
        spec(HttpMethod::Put, "/api/v1/network/interfaces/:id", "network"),
    ];
    let compute = vec![
        spec(HttpMethod::Get, "/api/v1/compute/vms", "compute"),
        spec(HttpMethod::Post, "/api/v1/compute/vms", "compute"),
        spec(HttpMethod::Delete, "/api/v1/compute/vms/:id", "compute"),
    ];
    (storage, network, compute)
}

// ----------------------------------------------------------------------------
// 集成测
// ----------------------------------------------------------------------------

/// 把三组件 handler 注册到 gateway，返回 (gateway, 各组件 call_count)。
async fn gateway_with_three_components() -> (
    InProcessGateway,
    Arc<AtomicU32>,
    Arc<AtomicU32>,
    Arc<AtomicU32>,
) {
    let gw = InProcessGateway::new();
    let (storage_r, network_r, compute_r) = three_component_routes();

    let storage_cnt = Arc::new(AtomicU32::new(0));
    let network_cnt = Arc::new(AtomicU32::new(0));
    let compute_cnt = Arc::new(AtomicU32::new(0));

    gw.register_component(
        "storage",
        Box::new(MockRouteHandler::new(
            "storage",
            storage_r,
            storage_cnt.clone(),
        )),
    )
    .await
    .expect("注册 storage");
    gw.register_component(
        "network",
        Box::new(MockRouteHandler::new(
            "network",
            network_r,
            network_cnt.clone(),
        )),
    )
    .await
    .expect("注册 network");
    gw.register_component(
        "compute",
        Box::new(MockRouteHandler::new(
            "compute",
            compute_r,
            compute_cnt.clone(),
        )),
    )
    .await
    .expect("注册 compute");

    (gw, storage_cnt, network_cnt, compute_cnt)
}

#[tokio::test]
async fn register_aggregates_routes_from_three_components() {
    let (gw, _, _, _) = gateway_with_three_components().await;

    // list_routes 聚合三组件全部 8 条路由。
    let routes = gw.list_routes().await;
    assert_eq!(
        routes.len(),
        8,
        "应聚合 storage(3) + network(2) + compute(3) = 8 条路由，实得 {routes:?}"
    );

    // 各组件路由都在列表中。
    let paths: Vec<&str> = routes.iter().map(|r| r.path.as_str()).collect();
    assert!(paths.contains(&"/api/v1/storage/pools"), "缺 storage 路由");
    assert!(
        paths.contains(&"/api/v1/network/interfaces"),
        "缺 network 路由"
    );
    assert!(paths.contains(&"/api/v1/compute/vms"), "缺 compute 路由");

    // component_count == 3。
    assert_eq!(gw.component_count(), 3);

    // handler_component 字段与注册名一致（跨 crate 类型桥接：注册名 ↔ RouteSpec）。
    let storage_routes: Vec<_> = routes
        .iter()
        .filter(|r| r.handler_component == "storage")
        .collect();
    assert_eq!(storage_routes.len(), 3);
    let network_routes: Vec<_> = routes
        .iter()
        .filter(|r| r.handler_component == "network")
        .collect();
    assert_eq!(network_routes.len(), 2);
    let compute_routes: Vec<_> = routes
        .iter()
        .filter(|r| r.handler_component == "compute")
        .collect();
    assert_eq!(compute_routes.len(), 3);
}

#[tokio::test]
async fn dispatch_matches_correct_component_across_three() {
    let (gw, storage_cnt, network_cnt, compute_cnt) = gateway_with_three_components().await;

    // 三次请求，分别命中 storage / network / compute 的不同路由（含路径参数）。
    let cases = vec![
        (
            req(HttpMethod::Get, "/api/v1/storage/pools/tank"),
            "storage",
        ),
        (
            req(HttpMethod::Get, "/api/v1/network/interfaces"),
            "network",
        ),
        (req(HttpMethod::Post, "/api/v1/compute/vms"), "compute"),
        (
            req(HttpMethod::Delete, "/api/v1/compute/vms/vm-007"),
            "compute",
        ),
    ];

    for (req, expected_comp) in cases {
        let (resp, matched) = gw.dispatch(req).await;
        assert_eq!(
            resp.status, 200,
            "应 200（命中 {expected_comp}），实得 {} body={}",
            resp.status, resp.body
        );
        let matched = matched.expect("dispatch 应匹配到路由");
        assert_eq!(
            matched.handler_component, expected_comp,
            "matched.handler_component 应为 {expected_comp}"
        );
        // 响应 body 带组件标识（响应聚合：dispatch 把 handler 的 ApiResponse 透出）。
        assert_eq!(
            resp.body["component"].as_str(),
            Some(expected_comp),
            "body.component 应为 {expected_comp}"
        );
        // 响应 header 也带组件标识（验证 header 也透传）。
        assert_eq!(resp.headers["X-Served-By"].as_str(), Some(expected_comp));
    }

    // 各组件被调用的次数（compute 命中 2 次）。
    assert_eq!(storage_cnt.load(Ordering::SeqCst), 1);
    assert_eq!(network_cnt.load(Ordering::SeqCst), 1);
    assert_eq!(compute_cnt.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn dispatch_unmatched_path_returns_404() {
    let (gw, storage_cnt, network_cnt, compute_cnt) = gateway_with_three_components().await;

    let (resp, matched) = gw
        .dispatch(req(HttpMethod::Get, "/api/v1/unknown/endpoint"))
        .await;
    assert_eq!(resp.status, 404, "未注册路由应返回 404");
    assert!(matched.is_none(), "未匹配路由时 matched 应为 None");
    // 任何组件都未被调用（短路在路由匹配阶段）。
    assert_eq!(storage_cnt.load(Ordering::SeqCst), 0);
    assert_eq!(network_cnt.load(Ordering::SeqCst), 0);
    assert_eq!(compute_cnt.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn dispatch_method_mismatch_returns_404() {
    let (gw, _, _, _) = gateway_with_three_components().await;

    // /api/v1/storage/pools 注册的是 GET；用 POST 应未命中（method 不匹配）。
    let (resp, matched) = gw
        .dispatch(req(HttpMethod::Post, "/api/v1/storage/pools"))
        .await;
    assert_eq!(resp.status, 404, "method 不匹配应 404");
    assert!(matched.is_none());
}

#[tokio::test]
async fn register_duplicate_route_across_components_conflicts() {
    // 跨组件重复注册同 method+path → RouteConflict（路由聚合的冲突检测）。
    let gw = InProcessGateway::new();
    gw.register_component(
        "storage",
        Box::new(MockRouteHandler::new(
            "storage",
            vec![spec(HttpMethod::Get, "/api/v1/conflict", "storage")],
            Arc::new(AtomicU32::new(0)),
        )),
    )
    .await
    .unwrap();

    let err = gw
        .register_component(
            "network",
            Box::new(MockRouteHandler::new(
                "network",
                vec![spec(HttpMethod::Get, "/api/v1/conflict", "network")],
                Arc::new(AtomicU32::new(0)),
            )),
        )
        .await
        .expect_err("重复路由应报 RouteConflict");

    assert!(
        matches!(err, ApiGatewayError::RouteConflict(_)),
        "应 RouteConflict，实得 {err:?}"
    );
}

#[tokio::test]
async fn response_aggregation_preserves_per_component_body() {
    // 验证「响应聚合」：dispatch 把各 handler 产出的 ApiResponse（含 body/headers）
    // 原样透出，不被网关层改写。这是「同一入口聚合多组件响应」的核心契约。
    let (gw, _, _, _) = gateway_with_three_components().await;

    // 给 storage 发带 body 的 POST，验证 echo 透传。
    let mut storage_req = req(HttpMethod::Post, "/api/v1/storage/datasets");
    storage_req.body = serde_json::json!({"name": "tank/vm/x", "size": 1024});
    let (resp, matched) = gw.dispatch(storage_req).await;
    assert_eq!(resp.status, 200);
    assert_eq!(matched.unwrap().handler_component, "storage");
    // body 透传：component 标识 + echo 的请求 body。
    assert_eq!(resp.body["component"].as_str(), Some("storage"));
    assert_eq!(resp.body["echo"]["name"].as_str(), Some("tank/vm/x"));
    assert_eq!(resp.body["echo"]["size"].as_i64(), Some(1024));

    // 给 compute 发 GET，验证 method 字段也透传。
    let (resp, matched) = gw
        .dispatch(req(HttpMethod::Get, "/api/v1/compute/vms"))
        .await;
    assert_eq!(resp.status, 200);
    assert_eq!(matched.unwrap().handler_component, "compute");
    assert_eq!(resp.body["method"].as_str(), Some("Get"));
    assert_eq!(resp.body["path"].as_str(), Some("/api/v1/compute/vms"));
}

#[tokio::test]
async fn handler_returning_error_becomes_500_response() {
    // 验证错误传播：handler 返回 Err 时，dispatch 把它转成 5xx 响应（对齐
    // ha_failover 场景的错误传播模式）。
    struct FailingHandler;
    #[async_trait]
    impl RouteHandler for FailingHandler {
        async fn routes(&self) -> Vec<RouteSpec> {
            vec![spec(HttpMethod::Get, "/api/v1/boom", "failing")]
        }
        async fn handle(&self, _req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
            Err(ApiGatewayError::Internal("mock handler 故障".into()))
        }
    }

    let gw = InProcessGateway::new();
    gw.register_component("failing", Box::new(FailingHandler))
        .await
        .unwrap();

    let (resp, matched) = gw.dispatch(req(HttpMethod::Get, "/api/v1/boom")).await;
    assert!(
        resp.status >= 500,
        "handler Err 应转 5xx，实得 {}",
        resp.status
    );
    assert!(matched.is_some(), "路由仍应匹配（错误发生在 handle 阶段）");
}

#[tokio::test]
async fn route_spec_fields_preserved_through_aggregation() {
    // 跨 crate 类型一致性：注册时 RouteSpec 的所有字段（method/path/component/
    // requires_auth/required_roles）经聚合后从 list_routes 取出应原样保留。
    let gw = InProcessGateway::new();
    let custom = RouteSpec {
        method: HttpMethod::Patch,
        path: "/api/v1/custom/:id".to_string(),
        handler_component: "custom".to_string(),
        requires_auth: true,
        required_roles: vec!["admin".to_string(), "operator".to_string()],
    };
    gw.register_component(
        "custom",
        Box::new(MockRouteHandler::new(
            "custom",
            vec![custom.clone()],
            Arc::new(AtomicU32::new(0)),
        )),
    )
    .await
    .unwrap();

    let routes = gw.list_routes().await;
    assert_eq!(routes.len(), 1);
    let r = &routes[0];
    assert_eq!(r.method, HttpMethod::Patch);
    assert_eq!(r.path, "/api/v1/custom/:id");
    assert_eq!(r.handler_component, "custom");
    assert!(r.requires_auth);
    assert_eq!(r.required_roles, vec!["admin", "operator"]);
}
