//! Gateway 默认实现——进程内聚合网关（规划文档 §3.6 / §9.1#10）。
//!
//! 设计要点：
//! - **内嵌网关**（不独立成层）：`InProcessGateway` 是一个普通 struct，由 osd 在进程内
//!   持有并 `start`，不拆成独立服务（§9.1#10 红线，改架构须 ADR）。
//! - **路由聚合**：`register_component` 接收 `Box<dyn RouteHandler>`，把其 `routes()`
//!   并入 `RouteRegistry`；`list_routes` 返回聚合后的全量路由。
//! - **中间件链分发**：`dispatch` 按注册顺序跑 `MiddlewareChain::run_before`，命中短路
//!   决策（Reject/RateLimited）即直接返回对应响应；全部放行后按 `RouteRegistry`
//!   匹配路由并调用目标 `RouteHandler::handle`；最后 `run_after` 逆序处理响应。
//! - **真实 Axum 监听**（已接通）：`start` 调 [`crate::http::build_router`] 构造
//!   `axum::Router`，再 `axum::serve` 绑定到 `addr`。中间件链/路由匹配/组件分发
//!   逻辑不变（仍由 `dispatch` 处理），axum 仅完成 HTTP 编解码与 WS 升级握手。
//! - **状态共享**：所有可变状态用 `Arc<Mutex<..>>` 包装，使 `InProcessGateway`
//!   可被 `Clone`（廉价 Arc clone）；这样 [`crate::http::GatewayState`] 能持有一份
//!   `Arc<InProcessGateway>` 供 axum handler 调用 `dispatch`。
//! - **JWT 入口解析**：`set_jwt_issuer` 注入 `os_security::JwtIssuer`，
//!   `start` 时传给 axum state，HTTP 入口解析 `Authorization: Bearer` 后填充
//!   `ApiRequest.auth`。
//!
//! TLS 终止：rustls/openssl TLS 加载在 workspace 未启用对应 feature，`start` 内
//! 仅校验 `TlsConfig` 路径非空，真实证书加载与 TLS 监听留 TODO。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use os_security::JwtIssuerImpl;

use crate::chain::MiddlewareChain;
use crate::gateway::{ApiRequest, ApiResponse, Gateway, RouteHandler, RouteSpec, TlsConfig};
use crate::middleware::MiddlewareDecision;
use crate::routing::RouteRegistry;
use crate::ws_impl::WsHub;

// ----------------------------------------------------------------------------
// InProcessGateway
// ----------------------------------------------------------------------------

/// 已注册的组件处理器（持有 trait object，Arc 共享以便网关 Clone）。
struct Component {
    handler: Arc<dyn RouteHandler>,
}

/// 网关内部共享状态（Arc 包装，使 `InProcessGateway: Clone`）。
struct Shared {
    /// 组件名 → 处理器
    components: Mutex<HashMap<String, Component>>,
    /// 聚合路由注册表（按注册顺序 + 匹配算法）
    registry: Mutex<RouteRegistry>,
    /// 监听状态：Some(addr) 表示已 start；存放 serve 句柄用于 stop
    listening: Mutex<Option<ListeningState>>,
}

/// 监听状态：地址 + 可选的 shutdown 信号发送端。
struct ListeningState {
    /// 监听地址（保留用于日志/调试；当前未直接读取）
    #[allow(dead_code)]
    addr: String,
    /// 触发 axum::serve 优雅关闭的信号发送端（None = 内部测模式）。
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

/// 进程内聚合网关——持有组件路由表 + 中间件链 + 路由注册表 + WS Hub。
///
/// `Clone` 廉价（仅 Arc 引用计数），便于 axum handler 共享；中间件链在
/// `add_middleware` 期间独占 `&mut self` 修改（启动前阶段），运行期只读。
#[derive(Clone)]
pub struct InProcessGateway {
    /// 共享可变状态（components / registry / listening）
    shared: Arc<Shared>,
    /// 中间件链（push 顺序即 before 执行顺序）—— 非 Arc，需启动前配置好
    chain: Arc<MiddlewareChain>,
    /// WebSocket Hub（独立可变，内部 RwLock/broadcast 已线程安全）
    ws_hub_inner: Arc<WsHub>,
    /// JWT 签发/校验器（None = 不做 HTTP 入口解析）
    jwt: Arc<Mutex<Option<Arc<JwtIssuerImpl>>>>,
    /// 固定 admin token（OS_ADMIN_TOKEN 环境变量；None = 不启用）
    admin_token: Arc<Mutex<Option<Arc<String>>>>,
    /// IM 区块链认证存储（challenge/verify 签发的 nonce/token 桶）。
    ///
    /// 由 main.rs 装配：与 `ImRouteHandler` 共享同一 `Arc`，REST（handler 内）
    /// 与 WS 握手（http.rs `ws_handler`）验同一批 token。None = IM 未装配
    /// （WS 握手将一律拒绝 IM 用户）。
    im_auth: Arc<Mutex<Option<Arc<crate::handlers::im::ImAuth>>>>,
    /// API 网关共享实例（http.rs SSE 流式转发用，2026-08-31）。
    ///
    /// main.rs 装配：与注册的 "api_gateway" 组件、media-gen 的生图扣费
    /// **同一实例**（`Mutex<Connection>` 是查-扣原子的边界）。None = 未装配
    /// （`stream:true` 请求回落非流式整包路径，行为同旧版）。
    api_gateway: Arc<Mutex<Option<Arc<crate::handlers::api_gateway::ApiGatewayRouteHandler>>>>,
    /// LLM 外部 API 接入共享状态（http.rs SSE 流式直通用，2026-08-31）。
    ///
    /// main.rs 装配：与注册的 "llm" 组件（LlmRouteHandler）内的
    /// `llm_external_apis` 表**同一实例**（同一条 `Mutex<Connection>`，查/写
    /// 不跨连接）。None = 未装配（`stream:true` 请求回落组件整包路径，行为同
    /// 未特挂）。
    llm_external: Arc<Mutex<Option<Arc<crate::handlers::llm_external::LlmExternalState>>>>,
    /// WS 端点路径（None = 不挂载 WS；默认 /ws）
    ws_path: Arc<Mutex<Option<String>>>,
    /// 已启动的 axum 监听 JoinHandle（stop 时 abort）
    serve_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl std::fmt::Debug for InProcessGateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.shared.components.lock().map(|c| c.len()).unwrap_or(0);
        f.debug_struct("InProcessGateway")
            .field("components", &count)
            .field("chain_len", &self.chain.len())
            .finish()
    }
}

impl InProcessGateway {
    /// 创建空网关（WS Hub 默认 broadcast 容量 256，WS 路径默认 `/ws`）。
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                components: Mutex::new(HashMap::new()),
                registry: Mutex::new(RouteRegistry::new()),
                listening: Mutex::new(None),
            }),
            chain: Arc::new(MiddlewareChain::new()),
            ws_hub_inner: Arc::new(WsHub::default()),
            jwt: Arc::new(Mutex::new(None)),
            admin_token: Arc::new(Mutex::new(None)),
            im_auth: Arc::new(Mutex::new(None)),
            api_gateway: Arc::new(Mutex::new(None)),
            llm_external: Arc::new(Mutex::new(None)),
            ws_path: Arc::new(Mutex::new(Some("/ws".to_string()))),
            serve_handle: Arc::new(Mutex::new(None)),
        }
    }

    /// 追加一个中间件到链尾（执行顺序 = 追加顺序）。
    ///
    /// 注：因 `chain` 现为 `Arc<MiddlewareChain>` 共享，`add_middleware` 需 `&mut self`；
    /// 调用方应在 `start` 前完成全部中间件配置（与原 trait 用法一致）。
    pub fn add_middleware(&mut self, mw: Box<dyn crate::middleware::Middleware>) {
        // Arc::get_mut 仅在没有其他持有者时成功；启动前阶段满足。
        // 否则回退为通过 Arc::make_mut（MiddlewareChain 未 impl Clone，故用 get_mut）。
        if let Some(chain) = Arc::get_mut(&mut self.chain) {
            chain.push(mw);
        } else {
            // 共享已发生：无法安全追加（中间件链不可变后追加）；
            // 调用方应避免在 start 后改链。这里 panic 以暴露误用。
            panic!("InProcessGateway::add_middleware 在网关已共享后调用（应在 start 前）");
        }
    }

    /// 取中间件链长度（测试可见）。
    pub fn middleware_count(&self) -> usize {
        self.chain.len()
    }

    /// 已注册组件数。
    pub fn component_count(&self) -> usize {
        self.shared.components.lock().map(|c| c.len()).unwrap_or(0)
    }

    /// 设置 JWT 签发/校验器（HTTP 入口用于解析 `Authorization: Bearer` 头）。
    ///
    /// 传 `None` 关闭入口解析（auth 字段始终为 None，留给下游决策）。
    pub fn set_jwt_issuer(&self, jwt: Option<Arc<JwtIssuerImpl>>) {
        *self.jwt.lock().expect("jwt lock") = jwt;
    }

    /// 设置固定 admin token（来自 `OS_ADMIN_TOKEN` 环境变量）。
    ///
    /// 漏洞2 修复：当请求 `Authorization: Bearer <token>` 与之精确匹配时，HTTP 入口
    /// 注入 admin Principal（绕过 JWT 链路）。传 `None` 关闭该机制（仅 JWT 鉴权）。
    pub fn set_admin_token(&self, admin_token: Option<Arc<String>>) {
        *self.admin_token.lock().expect("admin_token lock") = admin_token;
    }

    /// 注入 IM 认证存储（与注册的 `ImRouteHandler` 共享同一实例；见字段注释）。
    ///
    /// main.rs 在注册 "im" 组件后、`start` 前调用；传 `None` 表示 IM 未装配。
    pub fn set_im_auth(&self, auth: Option<Arc<crate::handlers::im::ImAuth>>) {
        *self.im_auth.lock().expect("im_auth lock") = auth;
    }

    /// 取 IM 认证存储（http.rs WS 握手验 `?user=&token=` 用；None = 未装配）。
    pub fn im_auth(&self) -> Option<Arc<crate::handlers::im::ImAuth>> {
        self.im_auth.lock().expect("im_auth lock").clone()
    }

    /// 注入 API 网关共享实例（http.rs SSE 流式转发用；见字段注释）。
    ///
    /// main.rs 在注册 "api_gateway" 组件后、`start` 前调用；传 `None` 表示
    /// 未装配（流式请求回落整包路径）。须 `start` 前调用：start 后修改不影响
    /// 已构建的 Router。
    pub fn set_api_gateway(
        &self,
        gw: Option<Arc<crate::handlers::api_gateway::ApiGatewayRouteHandler>>,
    ) {
        *self.api_gateway.lock().expect("api_gateway lock") = gw;
    }

    /// 取 API 网关共享实例（http.rs `GatewayState` 构造用；None = 未装配）。
    pub fn api_gateway(&self) -> Option<Arc<crate::handlers::api_gateway::ApiGatewayRouteHandler>> {
        self.api_gateway.lock().expect("api_gateway lock").clone()
    }

    /// 注入 LLM 外部 API 接入共享状态（http.rs SSE 流式直通用；见字段注释）。
    ///
    /// main.rs 在注册 "llm" 组件后、`start` 前调用，传
    /// `LlmRouteHandler::external_state()`；传 None 表示未装配（流式请求回落
    /// 组件整包路径）。须 `start` 前调用：start 后修改不影响已构建的 Router。
    pub fn set_llm_external(
        &self,
        state: Option<Arc<crate::handlers::llm_external::LlmExternalState>>,
    ) {
        *self.llm_external.lock().expect("llm_external lock") = state;
    }

    /// 取 LLM 外部 API 接入共享状态（http.rs `GatewayState` 构造用；None = 未装配）。
    pub fn llm_external(&self) -> Option<Arc<crate::handlers::llm_external::LlmExternalState>> {
        self.llm_external.lock().expect("llm_external lock").clone()
    }

    /// 设置 WS 端点路径（`None` = 不挂载 WS 升级路由；默认 `/ws`）。
    ///
    /// 须在 `start` 前调用：start 后修改不影响已构建的 Router。
    pub fn set_ws_path(&self, path: Option<String>) {
        *self.ws_path.lock().expect("ws_path lock") = path;
    }

    /// 取 WS Hub 引用（Arc clone，供 axum WS handler 使用）。
    pub fn ws_hub(&self) -> WsHub {
        (*self.ws_hub_inner).clone()
    }

    /// 同步版路由列表（给 [`crate::http::build_router`] 用，避免 async 锁）。
    pub(crate) fn list_routes_inner(&self) -> Vec<RouteSpec> {
        self.shared
            .registry
            .lock()
            .expect("registry lock")
            .all()
            .to_vec()
    }

    /// 内部分发：跑中间件链 + 路由匹配 + 调用处理器 + 逆序 after。
    ///
    /// 返回 `(响应, 匹配到的路由规格)`；未匹配路由时返回 404。
    /// 暴露为 pub 便于集成测 / 单测（网关骨架的关键路径）。
    pub async fn dispatch(&self, mut req: ApiRequest) -> (ApiResponse, Option<RouteSpec>) {
        // 1) 中间件 before 链
        match self.chain.run_before(&mut req).await {
            Ok(MiddlewareDecision::Continue) => {}
            Ok(MiddlewareDecision::Reject { status, body }) => {
                return (
                    ApiResponse {
                        status,
                        body,
                        headers: serde_json::json!({}),
                    },
                    None,
                );
            }
            Ok(MiddlewareDecision::RateLimited) => {
                return (
                    ApiResponse {
                        status: 429,
                        body: serde_json::json!({"error": "rate limited"}),
                        headers: serde_json::json!({}),
                    },
                    None,
                );
            }
            Err(e) => {
                return (
                    ApiResponse {
                        status: 500,
                        body: serde_json::json!({"error": e.to_string()}),
                        headers: serde_json::json!({}),
                    },
                    None,
                );
            }
        }

        // 2) 路由匹配
        let matched: Option<(usize, crate::routing::PathParams)> = {
            let reg = self.shared.registry.lock().expect("registry lock");
            reg.match_api_request(&req)
        };

        let (idx, _params) = match matched {
            Some(x) => x,
            None => {
                let mut resp = ApiResponse {
                    status: 404,
                    body: serde_json::json!({"error": "not found"}),
                    headers: serde_json::json!({}),
                };
                let _ = self.chain.run_after(&mut resp).await;
                return (resp, None);
            }
        };

        // 3) 取路由规格（用于返回）与目标处理器句柄
        let route_spec = {
            let reg = self.shared.registry.lock().expect("registry lock");
            reg.all().get(idx).cloned()
        };

        // 3.5) 【鉴权强制】路由匹配成功后、调用 handler 前，强制执行角色检查。
        //   漏洞1 修复：原 dispatch 未调用 authorize，导致所有 required_roles:["admin"]
        //   的写路由实际任何人可调。此处短路：route.requires_auth==true 时
        //   按 AuthMiddleware::authorize 判定 401（未认证）/403（权限不足）。
        //   requires_auth==false 或 required_roles 为空 → 放行（不破坏只读 GET）。
        if let Some(route) = route_spec.as_ref() {
            let decision = crate::middleware::AuthMiddleware::authorize(&req, route);
            if let MiddlewareDecision::Reject { status, body } = decision {
                let mut resp = ApiResponse {
                    status,
                    body,
                    headers: serde_json::json!({}),
                };
                let _ = self.chain.run_after(&mut resp).await;
                return (resp, route_spec);
            }
        }

        let component_name = route_spec.as_ref().map(|r| r.handler_component.clone());

        // 4) 取出 handler 的 Arc clone（不持锁调用 handle，避免跨 await 锁）
        let handler: Option<Arc<dyn RouteHandler>> = match &component_name {
            None => None,
            Some(name) => self
                .shared
                .components
                .lock()
                .expect("components lock")
                .get(name.as_str())
                .map(|c| c.handler.clone()),
        };

        let handler_result = match handler {
            None => Err(crate::ApiGatewayError::ComponentNotFound(
                component_name.unwrap_or_else(|| "无匹配组件".to_string()),
            )),
            Some(h) => h.handle(req).await,
        };

        // 5) 构造响应 + 逆序 after
        let mut resp = match handler_result {
            Ok(r) => r,
            Err(e) => ApiResponse {
                status: 500,
                body: serde_json::json!({"error": e.to_string()}),
                headers: serde_json::json!({}),
            },
        };
        let _ = self.chain.run_after(&mut resp).await;
        (resp, route_spec)
    }

    /// 是否正在监听。
    pub fn is_listening(&self) -> bool {
        self.shared
            .listening
            .lock()
            .map(|l| l.is_some())
            .unwrap_or(false)
    }
}

impl Default for InProcessGateway {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Gateway for InProcessGateway {
    async fn register_component(
        &self,
        component: &str,
        handler: Box<dyn RouteHandler>,
    ) -> Result<(), crate::ApiGatewayError> {
        // 先取该组件的路由，注册进 registry（检测冲突）
        let routes = handler.routes().await;
        {
            let mut reg = self.shared.registry.lock().expect("registry lock");
            for r in routes {
                reg.register(r)?;
            }
        }
        self.shared
            .components
            .lock()
            .expect("components lock")
            .insert(
                component.to_string(),
                Component {
                    handler: Arc::from(handler),
                },
            );
        Ok(())
    }

    async fn list_routes(&self) -> Vec<RouteSpec> {
        self.list_routes_inner()
    }

    async fn start(
        &self,
        addr: &str,
        tls_config: Option<TlsConfig>,
    ) -> Result<(), crate::ApiGatewayError> {
        // TLS 配置校验（若提供）
        if let Some(cfg) = tls_config.as_ref() {
            crate::middleware::TlsMiddleware::validate_config(cfg)?;
        }
        if tls_config.is_some() {
            // TODO(TLS): rustls/axum-server TLS 加载未启用 feature；
            // 此处仅校验配置合法，仍按明文 HTTP 启动（生产部署应配置反代终止 TLS）。
            // 待 rustls feature 注册后改为 axum_server::config_rustls 真实 TLS 监听。
        }

        // 构建 axum Router（注册路由 + 可选 WS）
        let ws_path = self
            .ws_path
            .lock()
            .expect("ws_path lock")
            .as_deref()
            .map(|s| s.to_string());
        let jwt = self.jwt.lock().expect("jwt lock").clone();
        let admin_token = self.admin_token.lock().expect("admin_token lock").clone();
        let api_gateway = self.api_gateway.lock().expect("api_gateway lock").clone();
        let llm_external = self.llm_external.lock().expect("llm_external lock").clone();
        let state = crate::http::GatewayState {
            gateway: Arc::new(self.clone()),
            jwt,
            admin_token,
            git_repos_root: None,
            api_gateway,
            llm_external,
        };
        let router = crate::http::build_router(state, ws_path.as_deref());

        // 绑定地址并 axum::serve（后台 task 持有 serve，stop 时关闭）
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| crate::ApiGatewayError::Internal(format!("bind {addr} 失败: {e}")))?;
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let serve_addr = addr.to_string();
        let handle = tokio::spawn(async move {
            let serve = axum::serve(listener, router);
            // graceful_shutdown：等 stop 触发 shutdown_tx 后退出
            let _ = serve
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await;
        });
        *self.shared.listening.lock().expect("listening lock") = Some(ListeningState {
            addr: serve_addr,
            shutdown_tx: Some(shutdown_tx),
        });
        *self.serve_handle.lock().expect("serve_handle lock") = Some(handle);
        Ok(())
    }

    async fn stop(&self) {
        // 触发 graceful shutdown（先取 shutdown_tx，释放 listening 锁）
        let shutdown_tx = self
            .shared
            .listening
            .lock()
            .expect("listening lock")
            .take()
            .and_then(|s| s.shutdown_tx);
        if let Some(tx) = shutdown_tx {
            let _ = tx.send(());
        }
        // 取出 serve handle 后立即释放 serve_handle 锁，再 await
        let handle_opt = self.serve_handle.lock().expect("serve_handle lock").take();
        if let Some(handle) = handle_opt {
            // 等待 serve task 结束（最多 1s 超时避免测试卡死）
            let _ = tokio::time::timeout(std::time::Duration::from_secs(1), handle).await;
        }
    }
}

// ----------------------------------------------------------------------------
// 单元测——路由聚合 + 中间件链分发 + 启停状态
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::{ApiRequest, HttpMethod, RouteSpec};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// 测试用 stub RouteHandler：声明若干路由，handle 时自增计数并回 200。
    struct StubHandler {
        name: String,
        routes: Vec<RouteSpec>,
        counter: AtomicU32,
    }

    #[async_trait]
    impl RouteHandler for StubHandler {
        async fn routes(&self) -> Vec<RouteSpec> {
            self.routes.clone()
        }
        async fn handle(&self, _req: ApiRequest) -> Result<ApiResponse, crate::ApiGatewayError> {
            self.counter.fetch_add(1, Ordering::SeqCst);
            Ok(ApiResponse {
                status: 200,
                body: serde_json::json!({"component": self.name}),
                headers: serde_json::json!({}),
            })
        }
    }

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

    #[tokio::test]
    async fn register_aggregates_routes_from_components() {
        let gw = InProcessGateway::new();
        let storage = StubHandler {
            name: "storage".to_string(),
            routes: vec![
                spec(HttpMethod::Get, "/api/v1/pools", "storage"),
                spec(HttpMethod::Get, "/api/v1/pools/:id", "storage"),
            ],
            counter: AtomicU32::new(0),
        };
        let compute = StubHandler {
            name: "compute".to_string(),
            routes: vec![spec(HttpMethod::Get, "/api/v1/vms", "compute")],
            counter: AtomicU32::new(0),
        };
        gw.register_component("storage", Box::new(storage))
            .await
            .unwrap();
        gw.register_component("compute", Box::new(compute))
            .await
            .unwrap();
        // 聚合后 3 条路由
        assert_eq!(gw.list_routes().await.len(), 3);
        assert_eq!(gw.component_count(), 2);
    }

    #[tokio::test]
    async fn dispatch_matches_and_calls_handler() {
        let gw = InProcessGateway::new();
        let handler = StubHandler {
            name: "storage".to_string(),
            routes: vec![spec(HttpMethod::Get, "/api/v1/pools/:id", "storage")],
            counter: AtomicU32::new(0),
        };
        // 通过 Arc 共享计数器以便 dispatch 后断言
        let counter = std::sync::Arc::new(AtomicU32::new(0));
        struct CountingHandler {
            routes: Vec<RouteSpec>,
            counter: std::sync::Arc<AtomicU32>,
        }
        #[async_trait]
        impl RouteHandler for CountingHandler {
            async fn routes(&self) -> Vec<RouteSpec> {
                self.routes.clone()
            }
            async fn handle(
                &self,
                _req: ApiRequest,
            ) -> Result<ApiResponse, crate::ApiGatewayError> {
                self.counter.fetch_add(1, Ordering::SeqCst);
                Ok(ApiResponse {
                    status: 200,
                    body: serde_json::json!({"ok": true}),
                    headers: serde_json::json!({}),
                })
            }
        }
        let _ = handler; // 占位避免未使用警告（改用 CountingHandler）
        gw.register_component(
            "storage",
            Box::new(CountingHandler {
                routes: vec![spec(HttpMethod::Get, "/api/v1/pools/:id", "storage")],
                counter: counter.clone(),
            }),
        )
        .await
        .unwrap();

        let (resp, route) = gw
            .dispatch(req(HttpMethod::Get, "/api/v1/pools/tank"))
            .await;
        assert_eq!(resp.status, 200);
        assert!(route.is_some());
        assert_eq!(route.unwrap().handler_component, "storage");
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn dispatch_unmatched_returns_404() {
        let gw = InProcessGateway::new();
        let (resp, route) = gw.dispatch(req(HttpMethod::Get, "/nope")).await;
        assert_eq!(resp.status, 404);
        assert!(route.is_none());
    }

    #[tokio::test]
    async fn dispatch_rate_limited_short_circuits_429() {
        let mut gw = InProcessGateway::new();
        // 加一个会限流的中间件（用 StatefulRateLimiter rps=0 → 必然拒绝）
        gw.add_middleware(Box::new(crate::chain::StatefulRateLimiter::new(0)));
        let (resp, _route) = gw.dispatch(req(HttpMethod::Get, "/api/v1/pools")).await;
        assert_eq!(resp.status, 429);
    }

    #[tokio::test]
    async fn start_stop_toggle_listening() {
        let gw = InProcessGateway::new();
        assert!(!gw.is_listening());
        // 用 0 端口让 OS 分配临时端口，避免固定端口冲突
        gw.start("127.0.0.1:0", None).await.unwrap();
        assert!(gw.is_listening());
        gw.stop().await;
        assert!(!gw.is_listening());
    }

    #[tokio::test]
    async fn start_with_invalid_tls_errors() {
        let gw = InProcessGateway::new();
        let err = gw
            .start(
                "127.0.0.1:0",
                Some(TlsConfig {
                    cert_path: String::new(),
                    key_path: "k".to_string(),
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, crate::ApiGatewayError::TlsError(_)));
    }

    /// 真实 HTTP 监听集成测：start 后用 reqwest 发起请求验证端到端可达。
    #[tokio::test]
    async fn start_serves_real_http_and_dispatches() {
        let gw = InProcessGateway::new();
        gw.register_component(
            "storage",
            Box::new(StubHandler {
                name: "storage".to_string(),
                routes: vec![spec(HttpMethod::Get, "/api/v1/pools", "storage")],
                counter: AtomicU32::new(0),
            }),
        )
        .await
        .unwrap();
        // 让 OS 分配端口，再从 TcpListener 取出真实端口
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener); // 释放以便网关重新绑定

        gw.start(&format!("127.0.0.1:{}", addr.port()), None)
            .await
            .unwrap();
        assert!(gw.is_listening());

        // 等待一小段让 axum::serve 就绪（绑定是同步的，应已就绪）
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://127.0.0.1:{}/api/v1/pools", addr.port()))
            .send()
            .await
            .expect("HTTP 请求应可达");
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["component"], "storage");

        // 未注册的非 API 路径（无扩展名）→ SPA fallback（200 index.html）
        let resp_spa = client
            .get(format!("http://127.0.0.1:{}/nope", addr.port()))
            .send()
            .await
            .unwrap();
        assert_eq!(resp_spa.status(), 200);
        let spa_body = resp_spa.text().await.unwrap();
        assert!(
            spa_body.contains("NexOS"),
            "/nope 应回退到 index.html（SPA fallback）"
        );

        // 形如 API 的未注册路径 → 404（不降级为 HTML）
        let resp404 = client
            .get(format!(
                "http://127.0.0.1:{}/api/v1/does-not-exist",
                addr.port()
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp404.status(), 404);

        gw.stop().await;
    }

    #[tokio::test]
    async fn register_duplicate_route_conflicts() {
        let gw = InProcessGateway::new();
        let h1 = StubHandler {
            name: "a".to_string(),
            routes: vec![spec(HttpMethod::Get, "/api/v1/pools", "a")],
            counter: AtomicU32::new(0),
        };
        let h2 = StubHandler {
            name: "b".to_string(),
            routes: vec![spec(HttpMethod::Get, "/api/v1/pools", "b")],
            counter: AtomicU32::new(0),
        };
        gw.register_component("a", Box::new(h1)).await.unwrap();
        let err = gw.register_component("b", Box::new(h2)).await.unwrap_err();
        assert!(matches!(err, crate::ApiGatewayError::RouteConflict(_)));
    }

    // —— 覆盖率补测：Debug / set_jwt / set_ws_path / Reject / handler error / panic ——

    #[tokio::test]
    async fn gateway_debug_shows_component_and_chain_len() {
        let gw = InProcessGateway::new();
        gw.register_component(
            "x",
            Box::new(StubHandler {
                name: "x".to_string(),
                routes: vec![spec(HttpMethod::Get, "/x", "x")],
                counter: AtomicU32::new(0),
            }),
        )
        .await
        .unwrap();
        let dbg = format!("{:?}", gw);
        assert!(dbg.contains("components"));
        assert!(dbg.contains("chain_len"));
    }

    #[tokio::test]
    async fn middleware_count_tracks_added() {
        let mut gw = InProcessGateway::new();
        assert_eq!(gw.middleware_count(), 0);
        gw.add_middleware(Box::new(crate::middleware::TlsMiddleware::new()));
        assert_eq!(gw.middleware_count(), 1);
        gw.add_middleware(Box::new(crate::middleware::AuditMiddleware::new()));
        assert_eq!(gw.middleware_count(), 2);
    }

    #[tokio::test]
    async fn set_jwt_issuer_and_ws_path_toggle() {
        let gw = InProcessGateway::new();
        // 默认 ws_path = Some("/ws")；make_state 在 start 时消费
        gw.set_ws_path(None);
        gw.set_ws_path(Some("/custom-ws".into()));
        // JWT 注入 None（关闭入口解析）
        gw.set_jwt_issuer(None);
        let issuer = std::sync::Arc::new(os_security::JwtIssuerImpl::new(b"k".to_vec()));
        gw.set_jwt_issuer(Some(issuer));
        gw.set_jwt_issuer(None); // 再次关闭
    }

    #[tokio::test]
    async fn dispatch_reject_middleware_short_circuits_with_body() {
        // 自定义中间件返回 Reject{403} → dispatch 直接回 403 + body
        use async_trait::async_trait;
        struct Forbid;
        #[async_trait]
        impl crate::middleware::Middleware for Forbid {
            async fn before(
                &self,
                _req: &mut ApiRequest,
            ) -> Result<crate::middleware::MiddlewareDecision, crate::ApiGatewayError> {
                Ok(crate::middleware::MiddlewareDecision::Reject {
                    status: 403,
                    body: serde_json::json!({"error": "forbidden"}),
                })
            }
            async fn after(&self, _resp: &mut ApiResponse) -> Result<(), crate::ApiGatewayError> {
                Ok(())
            }
        }
        let mut gw = InProcessGateway::new();
        gw.add_middleware(Box::new(Forbid));
        let (resp, route) = gw.dispatch(req(HttpMethod::Get, "/anything")).await;
        assert_eq!(resp.status, 403);
        assert_eq!(resp.body["error"], "forbidden");
        assert!(route.is_none(), "Reject 不应匹配到路由");
    }

    #[tokio::test]
    async fn dispatch_internal_error_middleware_yields_500() {
        // 中间件返回 Err → dispatch 回 500
        use async_trait::async_trait;
        struct Boom;
        #[async_trait]
        impl crate::middleware::Middleware for Boom {
            async fn before(
                &self,
                _req: &mut ApiRequest,
            ) -> Result<crate::middleware::MiddlewareDecision, crate::ApiGatewayError> {
                Err(crate::ApiGatewayError::Internal("middleware boom".into()))
            }
            async fn after(&self, _resp: &mut ApiResponse) -> Result<(), crate::ApiGatewayError> {
                Ok(())
            }
        }
        let mut gw = InProcessGateway::new();
        gw.add_middleware(Box::new(Boom));
        let (resp, _route) = gw.dispatch(req(HttpMethod::Get, "/x")).await;
        assert_eq!(resp.status, 500);
        assert!(resp.body["error"]
            .as_str()
            .unwrap()
            .contains("middleware boom"));
    }

    #[tokio::test]
    async fn dispatch_handler_error_yields_500() {
        // 命中路由但 handler 返回 Err → 500
        use async_trait::async_trait;
        struct FailHandler;
        #[async_trait]
        impl RouteHandler for FailHandler {
            async fn routes(&self) -> Vec<RouteSpec> {
                vec![spec(HttpMethod::Get, "/api/v1/fail", "fail")]
            }
            async fn handle(
                &self,
                _req: ApiRequest,
            ) -> Result<ApiResponse, crate::ApiGatewayError> {
                Err(crate::ApiGatewayError::Internal("handler boom".into()))
            }
        }
        let gw = InProcessGateway::new();
        gw.register_component("fail", Box::new(FailHandler))
            .await
            .unwrap();
        let (resp, route) = gw.dispatch(req(HttpMethod::Get, "/api/v1/fail")).await;
        assert_eq!(resp.status, 500);
        assert!(resp.body["error"]
            .as_str()
            .unwrap()
            .contains("handler boom"));
        // 路由仍被匹配（返回 route_spec），仅 handler 内部失败
        assert!(route.is_some());
    }

    #[tokio::test]
    #[should_panic(expected = "add_middleware 在网关已共享后调用")]
    async fn add_middleware_panics_after_shared() {
        // Arc::clone 使 Arc::get_mut 失败 → add_middleware panic
        let mut gw = InProcessGateway::new();
        let _shared = gw.clone(); // 触发 Arc 多持有者
        gw.add_middleware(Box::new(crate::middleware::TlsMiddleware::new()));
    }

    // —— 漏洞1 修复验证：dispatch 强制鉴权（401/403/200）——

    fn admin_spec(method: HttpMethod, path: &str, comp: &str) -> RouteSpec {
        RouteSpec {
            method,
            path: path.to_string(),
            handler_component: comp.to_string(),
            requires_auth: true,
            required_roles: vec!["admin".to_string()],
        }
    }

    fn req_with_admin_auth(method: HttpMethod, path: &str) -> ApiRequest {
        use os_security::{Principal, Role, User, UserId};
        let now = chrono::Utc::now();
        let user = User::new(
            UserId::new("admin".to_string()),
            "admin".to_string(),
            vec![Role::Admin],
            now,
        )
        .unwrap();
        let principal = Principal::new(user, vec![Role::Admin], now).unwrap();
        ApiRequest {
            method,
            path: path.to_string(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: Some(principal),
        }
    }

    fn req_with_user_auth(method: HttpMethod, path: &str) -> ApiRequest {
        use os_security::{Principal, Role, User, UserId};
        let now = chrono::Utc::now();
        let user = User::new(
            UserId::new("u1".to_string()),
            "alice".to_string(),
            vec![Role::User],
            now,
        )
        .unwrap();
        let principal = Principal::new(user, vec![Role::User], now).unwrap();
        ApiRequest {
            method,
            path: path.to_string(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: Some(principal),
        }
    }

    #[tokio::test]
    async fn dispatch_admin_route_without_auth_returns_401() {
        // 漏洞1：未认证调 admin 写路由 → 401
        let gw = InProcessGateway::new();
        gw.register_component(
            "storage",
            Box::new(StubHandler {
                name: "storage".to_string(),
                routes: vec![admin_spec(HttpMethod::Post, "/api/v1/pools", "storage")],
                counter: AtomicU32::new(0),
            }),
        )
        .await
        .unwrap();
        let (resp, _route) = gw.dispatch(req(HttpMethod::Post, "/api/v1/pools")).await;
        assert_eq!(resp.status, 401, "未认证调 admin 写路由应 401");
    }

    #[tokio::test]
    async fn dispatch_admin_route_with_wrong_role_returns_403() {
        // 漏洞1：认证但角色非 admin → 403
        let gw = InProcessGateway::new();
        gw.register_component(
            "storage",
            Box::new(StubHandler {
                name: "storage".to_string(),
                routes: vec![admin_spec(HttpMethod::Post, "/api/v1/pools", "storage")],
                counter: AtomicU32::new(0),
            }),
        )
        .await
        .unwrap();
        let (resp, _route) = gw
            .dispatch(req_with_user_auth(HttpMethod::Post, "/api/v1/pools"))
            .await;
        assert_eq!(resp.status, 403, "非 admin 角色调 admin 写路由应 403");
    }

    #[tokio::test]
    async fn dispatch_admin_route_with_admin_auth_passes() {
        // 漏洞1：admin 认证 → 200，handler 被调用
        let gw = InProcessGateway::new();
        gw.register_component(
            "storage",
            Box::new(StubHandler {
                name: "storage".to_string(),
                routes: vec![admin_spec(HttpMethod::Post, "/api/v1/pools", "storage")],
                counter: AtomicU32::new(0),
            }),
        )
        .await
        .unwrap();
        let (resp, _route) = gw
            .dispatch(req_with_admin_auth(HttpMethod::Post, "/api/v1/pools"))
            .await;
        assert_eq!(resp.status, 200, "admin 认证调 admin 写路由应 200");
    }

    #[tokio::test]
    async fn dispatch_public_route_without_auth_passes() {
        // 漏洞1：requires_auth=false 的只读路由不拦（无 auth 也 200）
        let gw = InProcessGateway::new();
        gw.register_component(
            "storage",
            Box::new(StubHandler {
                name: "storage".to_string(),
                routes: vec![spec(HttpMethod::Get, "/api/v1/pools", "storage")],
                counter: AtomicU32::new(0),
            }),
        )
        .await
        .unwrap();
        let (resp, _route) = gw.dispatch(req(HttpMethod::Get, "/api/v1/pools")).await;
        assert_eq!(resp.status, 200, "只读路由不应拦截");
    }

    #[tokio::test]
    async fn is_listening_reflects_start_stop() {
        // 覆盖 is_listening 在 start/stop 前后的状态读取（含 lock 成功分支）
        let gw = InProcessGateway::new();
        assert!(!gw.is_listening());
        gw.start("127.0.0.1:0", None).await.unwrap();
        assert!(gw.is_listening());
        gw.stop().await;
        assert!(!gw.is_listening());
    }

    #[tokio::test]
    async fn start_with_valid_tls_config_passes_validation() {
        // TLS 配置非空 → 校验通过，仍按明文启动（rustls 未启用，留 TODO）
        let gw = InProcessGateway::new();
        gw.start(
            "127.0.0.1:0",
            Some(TlsConfig {
                cert_path: "/x/cert.pem".into(),
                key_path: "/x/key.pem".into(),
            }),
        )
        .await
        .unwrap();
        assert!(gw.is_listening());
        gw.stop().await;
    }
}
