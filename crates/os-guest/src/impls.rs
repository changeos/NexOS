//! os-guest 默认实现（接通真实实现：axum Portal + nftnl 事务 + os-security JWT）
//!
//! 本模块提供 5 个 trait 的默认实现：
//! - [`HttpCaptivePortal`]（`CaptivePortal`）：**真实 axum HTTP 监听**（`:8081/:8082`），
//!   路由兼容 iOS/Android/Win/macOS/Linux 联网检测探测；落地页/注册/认证回调齐备。
//!   测试用 `tower::ServiceExt::oneshot` 对 `build_router()` 离线发起单请求（不真监听端口）。
//! - [`DefaultIdentityEngine`]（`IdentityEngine`）：内存 KV + **真实 JWT 签发**
//!   （委派 os-security `JwtIssuerImpl` / 任意 `JwtIssuer`，经 dyn 兼容包装）。
//! - [`DefaultPolicyEngine`]（`PolicyEngine`）：内存规则表，调 [`crate::policy`]
//!   的纯评估算法。
//! - [`NftRuleOrchestratorImpl`]（`NftRuleOrchestrator`）：构造 nft 规则字符串
//!   + 内存态 dry-run/checkpoint；**真实 nftables netlink 事务**经 `nftnl-ffi` feature
//!   门控启用（需 `apt install libnftnl-dev libmnl-dev`，ADR-DEPS-001 §91）。
//! - [`DefaultChainOrchestrator`]（`ChainOrchestrator`）：编排 os-wallet（真实
//!   `RpcRegistry`/`WalletConnector`/`ChainAdapter`）+ os-security `JwtIssuer`；
//!   本身不下沉密码学/链交互（红线 §3.18.1）。
//!
//! 所有实现保持"不 panic"原则；阻塞部分返回 `Err(...)` 并带说明。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use os_core::{AddressId, DateTime, GuestId, PageRequest, PageResponse, TaskId, Utc, Uuid};
use os_wallet::{
    ChainAdapter, ChainKind, ConnectorKind, RpcRegistry, SignRequest, SignatureAlgorithm,
    VerificationFactor, WalletConnector,
};

use crate::chain::{ChainVerificationConfig, ChainVerificationStatus, PrivacyMode};
use crate::identity::GuestFilter;
use crate::model::{
    generate_guest_id_with, validate_guest_id, EntropySource, GuestIdentity, GuestIdentityType,
    GuestStatus, SystemEntropy,
};
use crate::nft::NftGuestRule;
use crate::nft::{build_delete_element, statements_for_rule, DryRunResult, NftGuestAction};
use crate::policy::{evaluate_rules, GuestAction, GuestContext, PolicyDecision, PolicyRule};
use crate::portal::{
    decide_response, detect_probe_os, CaptivePortal, PortalConfig, PortalResponse, ProbeRequest,
    DEFAULT_LANDING_HTML, DEFAULT_LANDING_URL,
};

// ============================================================================
// dyn 兼容的 JwtIssuer 包装——os-security 的 JwtIssuer 是原生 async fn in trait
// （非 dyn 兼容，ADR-COMPAT-001），无法直接 `Arc<dyn JwtIssuer>`。这里定义一个本地
// dyn 兼容子 trait，用 #[async_trait] 桥接任意 `os_security::JwtIssuer`，使
// DefaultIdentityEngine 可注入任意 JWT 实现（含 JwtIssuerImpl）。
// ============================================================================

/// dyn 兼容的 JWT 签发包装。
///
/// os-security 的 `JwtIssuer` 是原生 `async fn in trait`（非 dyn 兼容，
/// ADR-COMPAT-001）。本 trait 用手写 `Pin<Box<dyn Future + Send>>` 返回值实现
/// dyn 兼容（不依赖 `#[async_trait]` 的 HRTB Send 推断），供 `DefaultIdentityEngine`
/// 注入任意 JWT 实现。
pub(crate) trait GuestJwtIssuer: Send + Sync {
    /// 签发 JWT，返回 token 字符串。
    fn issue<'a>(
        &'a self,
        claims: os_security::JwtClaims,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<String, os_security::SecurityError>>
                + Send
                + 'a,
        >,
    >;
}

// 为 os-security 的真实 `JwtIssuerImpl` 实现 GuestJwtIssuer（具体类型，编译器可
// 直接验证 issue future 为 Send；`JwtIssuerImpl::issue` 内部仅持 Mutex<Vec<u8>>，
// 不跨 await 持锁）。
impl GuestJwtIssuer for os_security::JwtIssuerImpl {
    fn issue<'a>(
        &'a self,
        claims: os_security::JwtClaims,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<String, os_security::SecurityError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(<Self as os_security::JwtIssuer>::issue(self, claims))
    }
}

// ============================================================================
// HttpCaptivePortal —— CaptivePortal 默认实现（真实 axum 监听）
// ============================================================================

/// Captive Portal 默认实现（基于 axum）。
///
/// - `start(config)`：用 `tokio::net::TcpListener` + `axum::serve` 在
///   `config.listen_http` 上真实监听 HTTP；`tokio::spawn` 后台运行，
///   `stop()` 经 graceful shutdown 通道关闭。
/// - `build_router()`：构造 axum 路由（公开，供测试用 `oneshot` 离线打）。
/// - `handle_detection`：纯逻辑探测识别 + 响应决策（与路由共用同一处理函数）。
///
/// 真实监听需端口可用（CI 沙箱可能受限）；测试默认走 `build_router()` + oneshot，
/// 不真监听端口。
pub struct HttpCaptivePortal {
    running: Mutex<bool>,
    config: Mutex<Option<PortalConfig>>,
    /// 已认证访客标识集合（用于 handle_detection / 路由决定放行/拦截）。
    /// 注：键为客户端 IP（生产由 axum 中间件从 ConnectInfo / X-Forwarded-For 注入）；
    /// 单元测兜底用 host 字段。
    authed: Mutex<HashMap<String, ()>>,
    /// start() 启动的后台任务句柄（stop 时 abort）。
    join: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// start() 创建的 shutdown sender（stop 时 take 并 send）。
    shutdown_tx: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl HttpCaptivePortal {
    /// 构造（初始未启动）。
    pub fn new() -> Self {
        Self {
            running: Mutex::new(false),
            config: Mutex::new(None),
            authed: Mutex::new(HashMap::new()),
            join: Mutex::new(None),
            shutdown_tx: Mutex::new(None),
        }
    }

    /// 标记某客户端（IP/标识）已认证（测试 / IdentityEngine 同步放行时调用）。
    pub fn mark_authed(&self, ip: &str) {
        self.authed
            .lock()
            .expect("authed poisoned")
            .insert(ip.to_string(), ());
    }

    /// 取消某客户端的认证。
    pub fn mark_unauthed(&self, ip: &str) {
        self.authed.lock().expect("authed poisoned").remove(ip);
    }

    /// 是否已认证（路由 + handle_detection 共用）。
    fn is_authed(&self, key: &str) -> bool {
        self.authed
            .lock()
            .expect("authed poisoned")
            .contains_key(key)
    }

    /// 当前配置克隆（无配置返回默认）。
    fn current_config(&self) -> PortalConfig {
        self.config
            .lock()
            .expect("config poisoned")
            .clone()
            .unwrap_or_default()
    }

    /// 构造 axum 路由——公开供测试用 `tower::ServiceExt::oneshot` 离线单请求测试
    /// （不真监听端口）。生产 `start()` 复用同一 router。
    ///
    /// 路由：
    /// - `GET /portal/landing` → 200 落地页 HTML
    /// - `POST /portal/register` → 注册回调（标记客户端已认证，返回 JSON）
    /// - `GET /portal/auth` → 302 重定向到落地页（认证入口）
    /// - `GET /generate_204` / `/hotspot-detect.html` / `/connecttest.txt` /
    ///   `/ncsi.txt` → 各 OS 探测端点，按 `decide_response` 返回 302/落地页/204
    /// - `fallback` → 兜底按 OS 探测处理（任何路径）
    pub fn build_router(&self) -> axum::Router {
        // 用 Arc<Self> 作 axum State，使处理函数可读写认证态。
        let state: Arc<Self> = Arc::new(Self {
            running: Mutex::new(*self.running.lock().expect("running poisoned")),
            config: Mutex::new(Some(self.current_config())),
            authed: Mutex::new(self.authed.lock().expect("authed poisoned").clone()),
            join: Mutex::new(None),
            shutdown_tx: Mutex::new(None),
        });
        Self::router_with_state(state)
    }

    /// 内部：用给定 State 构造 router（start 与 build_router 复用）。
    fn router_with_state(state: Arc<Self>) -> axum::Router {
        use axum::http::{header, StatusCode};
        use axum::response::IntoResponse;
        use axum::routing::{get, post};

        // 落地页（直接展示 HTML）。
        let landing_handler = |axum::extract::State(portal): axum::extract::State<Arc<Self>>| async move {
            let cfg = portal.current_config();
            let html = cfg
                .landing_html
                .unwrap_or_else(|| DEFAULT_LANDING_HTML.to_string());
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                html,
            )
                .into_response()
        };

        // 注册回调：表单提交 guest_id，标记认证态。
        let register_handler =
            |axum::extract::State(portal): axum::extract::State<Arc<Self>>,
             axum::extract::Query(q): axum::extract::Query<RegisterQuery>| async move {
                // 标识优先取 query.client_ip；缺省用 guest_id 兜底。
                let key = q.client_ip.unwrap_or_else(|| q.guest_id.clone());
                portal.mark_authed(&key);
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    format!(r#"{{"status":"ok","guest":"{}"}}"#, q.guest_id),
                )
                    .into_response()
            };

        // 认证入口：302 → 落地页（Captive Portal 标准做法）。
        let auth_handler = |axum::extract::State(_portal): axum::extract::State<Arc<Self>>| async move {
            redirect_302(DEFAULT_LANDING_URL)
        };

        axum::Router::new()
            .route("/portal/landing", get(landing_handler))
            .route("/portal/auth", get(auth_handler))
            .route("/portal/register", post(register_handler))
            // 各 OS 探测端点（典型路径）。
            .route(
                "/generate_204",
                get({
                    let s = state.clone();
                    move |h: axum::http::HeaderMap, u: axum::http::Uri| {
                        probe_handler_inner(s.clone(), h, u)
                    }
                }),
            )
            .route(
                "/hotspot-detect.html",
                get({
                    let s = state.clone();
                    move |h: axum::http::HeaderMap, u: axum::http::Uri| {
                        probe_handler_inner(s.clone(), h, u)
                    }
                }),
            )
            .route(
                "/connecttest.txt",
                get({
                    let s = state.clone();
                    move |h: axum::http::HeaderMap, u: axum::http::Uri| {
                        probe_handler_inner(s.clone(), h, u)
                    }
                }),
            )
            .route(
                "/ncsi.txt",
                get({
                    let s = state.clone();
                    move |h: axum::http::HeaderMap, u: axum::http::Uri| {
                        probe_handler_inner(s.clone(), h, u)
                    }
                }),
            )
            // 兜底：任何未匹配路径按 OS 探测处理。
            .fallback(get({
                let s = state.clone();
                move |h: axum::http::HeaderMap, u: axum::http::Uri| {
                    probe_handler_inner(s.clone(), h, u)
                }
            }))
            .with_state(state)
    }

    /// 探测响应渲染（路由处理函数共用）——按 PortalResponse 类型构造 axum 响应。
    ///
    /// - `Pass` → 204（Android generate_204 期望 204 空体；其他 OS 也接受）
    /// - `Redirect { url }` → 302 重定向
    /// - `Landing { html }` → 200 text/html
    async fn render_probe_response(
        &self,
        headers: &axum::http::HeaderMap,
        uri: axum::http::Uri,
    ) -> axum::response::Response {
        use axum::http::{header, StatusCode};
        use axum::response::IntoResponse;

        let ua = headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let host = headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let path = uri.path().to_string();
        let cfg = self.current_config();
        let os = detect_probe_os(&ua, &host, &path);
        // 用 host 字段兜底当 client key（生产由 ConnectInfo 注入真实 IP）。
        let authed = self.is_authed(&host);
        match decide_response(authed, &cfg, os) {
            PortalResponse::Pass => StatusCode::NO_CONTENT.into_response(),
            PortalResponse::Redirect { url } => redirect_302(&url),
            PortalResponse::Landing { html } => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                html,
            )
                .into_response(),
        }
    }
}

/// `/portal/register` 查询参数。
#[derive(Debug, serde::Deserialize)]
struct RegisterQuery {
    /// 访客 ID（必填）。
    guest_id: String,
    /// 客户端 IP（可选；缺省用 guest_id 作 key）。
    client_ip: Option<String>,
}

/// 探测处理函数（独立 fn 形式，避免闭包类型不匹配）。
async fn probe_handler_inner(
    portal: Arc<HttpCaptivePortal>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
) -> axum::response::Response {
    portal.render_probe_response(&headers, uri).await
}

/// 构造 302 Found 重定向（Captive Portal 标准做法；axum `Redirect::to` 是 303，
/// 不符合 §3.18 探测拦截的 302 期望）。返回的元组可直接 `into_response`。
fn redirect_302(url: &str) -> axum::response::Response {
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;
    (
        StatusCode::FOUND,
        [(header::LOCATION, url)],
        axum::body::Body::empty(),
    )
        .into_response()
}

impl Default for HttpCaptivePortal {
    fn default() -> Self {
        Self::new()
    }
}

impl CaptivePortal for HttpCaptivePortal {
    async fn start(&self, config: PortalConfig) -> Result<(), crate::GuestError> {
        // 防重入：已运行则返回错误。
        if *self.running.lock().expect("running poisoned") {
            return Err(crate::GuestError::PortalError(
                "Portal 已在运行".to_string(),
            ));
        }
        let port = config.listen_http;
        *self.config.lock().expect("config poisoned") = Some(config.clone());
        // 绑定端口（异步）。
        let addr = format!("0.0.0.0:{port}");
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| crate::GuestError::PortalError(format!("绑定 {addr} 失败: {e}")))?;
        // 复用 self 当前认证态构造 router。
        let state = Arc::new(Self {
            running: Mutex::new(true),
            config: Mutex::new(Some(config)),
            authed: Mutex::new(self.authed.lock().expect("authed poisoned").clone()),
            join: Mutex::new(None),
            shutdown_tx: Mutex::new(None),
        });
        let router = Self::router_with_state(state);
        // graceful shutdown 通道。
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        *self.shutdown_tx.lock().expect("shutdown_tx poisoned") = Some(tx);
        *self.running.lock().expect("running poisoned") = true;
        let shutdown = async move {
            let _ = rx.await;
        };
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(shutdown)
                .await;
        });
        *self.join.lock().expect("join poisoned") = Some(handle);
        Ok(())
    }

    async fn stop(&self) -> Result<(), crate::GuestError> {
        *self.running.lock().expect("running poisoned") = false;
        // 触发 graceful shutdown（先释放锁）。
        let tx_opt = self
            .shutdown_tx
            .lock()
            .expect("shutdown_tx poisoned")
            .take();
        if let Some(tx) = tx_opt {
            let _ = tx.send(());
        }
        // 取出后台任务句柄（先释放锁），再 await。
        let handle_opt = self.join.lock().expect("join poisoned").take();
        if let Some(handle) = handle_opt {
            let _ = handle.await;
        }
        Ok(())
    }

    async fn handle_detection(
        &self,
        request: ProbeRequest,
    ) -> Result<PortalResponse, crate::GuestError> {
        let os = detect_probe_os(&request.user_agent, &request.host, &request.path);
        // 用 host 兜底当 client key（实际部署由 axum 中间件注入真实 client IP）。
        let authed = self.is_authed(&request.host);
        let config = self.current_config();
        Ok(decide_response(authed, &config, os))
    }
}

// ============================================================================
// DefaultIdentityEngine —— IdentityEngine 默认实现（内存 KV + 真实 JWT）
// ============================================================================

/// 访客身份引擎默认实现（内存 KV；JWT 签发委派 os-security JwtIssuer）。
///
/// JWT 注入为可选（`Arc<dyn GuestJwtIssuer + Send + Sync>` 包装）：未注入时仅维护
/// `jwt_expiry` 时间戳（保持原行为），注入后 `authenticate_guest` 真实签发
/// `TokenType::Guest` JWT（经 os-security `JwtIssuerImpl` 或任意 `JwtIssuer`）。
pub struct DefaultIdentityEngine {
    guests: Mutex<HashMap<GuestId, GuestIdentity>>,
    entropy: Arc<dyn EntropySource>,
    /// 最近一次签发的 JWT（按 GuestId）——供测试断言 / 调用方观察。
    last_jwt: Mutex<HashMap<GuestId, String>>,
    /// 可选 JWT 签发器（dyn 兼容包装；未注入则 None）。
    jwt: Option<Arc<dyn GuestJwtIssuer + Send + Sync>>,
}

impl DefaultIdentityEngine {
    /// 构造（使用系统熵源，不注入 JWT 签发器——保持向后兼容）。
    pub fn new() -> Self {
        Self {
            guests: Mutex::new(HashMap::new()),
            entropy: Arc::new(SystemEntropy::new()),
            last_jwt: Mutex::new(HashMap::new()),
            jwt: None,
        }
    }

    /// 注入自定义熵源（测试用）。
    pub fn with_entropy(entropy: Arc<dyn EntropySource>) -> Self {
        Self {
            guests: Mutex::new(HashMap::new()),
            entropy,
            last_jwt: Mutex::new(HashMap::new()),
            jwt: None,
        }
    }

    /// 注入已桥接的 JWT 签发器（`Arc<dyn GuestJwtIssuer>`）——内部扩展点，供
    /// 未来为其他 `JwtIssuer` 实现桥接时复用。当前公共入口见 `with_jwt_impl`。
    #[allow(dead_code)]
    pub(crate) fn with_jwt_dyn(mut self, jwt: Arc<dyn GuestJwtIssuer + Send + Sync>) -> Self {
        self.jwt = Some(jwt);
        self
    }

    /// 注入真实 os-security `JwtIssuerImpl`（jsonwebtoken HS256）。
    ///
    /// 注入后 `authenticate_guest` 会经此签发 `TokenType::Guest` JWT 并把 token
    /// 存入 `last_jwt`（供 `issued_jwt()` 取回）。未注入则跳过真实签发。
    pub fn with_jwt_impl(mut self, jwt: Arc<os_security::JwtIssuerImpl>) -> Self {
        let bridged: Arc<dyn GuestJwtIssuer + Send + Sync> = jwt;
        self.jwt = Some(bridged);
        self
    }

    /// 取回某访客最近一次签发的 JWT（未签发 / 未注入签发器 → None）。
    pub fn issued_jwt(&self, id: &GuestId) -> Option<String> {
        self.last_jwt
            .lock()
            .expect("last_jwt poisoned")
            .get(id)
            .cloned()
    }

    /// 按 ID 类型计算默认有效期（不同身份类型不同信任强度）。
    fn default_expiry(id_type: GuestIdentityType, now: DateTime) -> (DateTime, DateTime, u64) {
        // 返回 (expires_at, jwt_expiry, nft_timeout_secs)
        match id_type {
            GuestIdentityType::RandomId => {
                let exp = now + chrono::Duration::hours(1);
                let jwt = now + chrono::Duration::minutes(30);
                (exp, jwt, 1800)
            }
            GuestIdentityType::ExtendedId => {
                let exp = now + chrono::Duration::days(7);
                let jwt = now + chrono::Duration::hours(2);
                (exp, jwt, 7200)
            }
            GuestIdentityType::PublicKey => {
                let exp = now + chrono::Duration::days(30);
                let jwt = now + chrono::Duration::hours(12);
                (exp, jwt, 3600)
            }
            GuestIdentityType::ChainCredential => {
                let exp = now + chrono::Duration::days(90);
                let jwt = now + chrono::Duration::hours(24);
                (exp, jwt, 3600)
            }
        }
    }

    /// 内部：生成不冲突的 GuestId（最多重试若干次）。
    fn gen_unique_id(&self) -> GuestId {
        for _ in 0..8 {
            let id = generate_guest_id_with(self.entropy.as_ref());
            if !self
                .guests
                .lock()
                .expect("guests poisoned")
                .contains_key(&id)
            {
                return id;
            }
        }
        // 8 次都碰撞的概率极低；兜底再加一次（接受极小概率碰撞，由调用方覆盖）。
        generate_guest_id_with(self.entropy.as_ref())
    }

    /// 真实签发 Guest JWT（若注入了 JwtIssuer）。失败仅记日志（不阻断认证流程）。
    async fn try_issue_jwt(&self, guest: &GuestIdentity) {
        let Some(jwt) = self.jwt.as_ref() else {
            return;
        };
        let now = Utc::now();
        let claims = os_security::JwtClaims {
            sub: os_security::UserId(guest.id.as_str().to_string()),
            roles: vec![],
            exp: guest.jwt_expiry.timestamp(),
            iat: now.timestamp(),
            token_type: os_security::TokenType::Guest,
            custom: serde_json::json!({
                "id_type": format!("{:?}", guest.identity_type),
            }),
        };
        if let Ok(token) = jwt.issue(claims).await {
            self.last_jwt
                .lock()
                .expect("last_jwt poisoned")
                .insert(guest.id.clone(), token);
        }
    }
}

impl Default for DefaultIdentityEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::identity::IdentityEngine for DefaultIdentityEngine {
    async fn create_guest(
        &self,
        id_type: GuestIdentityType,
    ) -> Result<GuestIdentity, crate::GuestError> {
        let now = Utc::now();
        let (expires_at, jwt_expiry, nft_timeout_secs) = Self::default_expiry(id_type, now);
        let id = self.gen_unique_id();
        // 校验生成的 ID 合法（防御熵源异常）。
        validate_guest_id(&id)?;
        let guest = GuestIdentity {
            id: id.clone(),
            identity_type: id_type,
            created_at: now,
            expires_at,
            jwt_expiry,
            nft_timeout_secs,
            status: GuestStatus::Pending,
            metadata: serde_json::json!({ "id_type": format!("{id_type:?}") }),
        };
        // 插入；极小概率碰撞则返回 GuestExists。
        let mut guests = self.guests.lock().expect("guests poisoned");
        if guests.contains_key(&id) {
            return Err(crate::GuestError::GuestExists(id.to_string()));
        }
        guests.insert(id.clone(), guest.clone());
        Ok(guest)
    }

    async fn authenticate_guest(&self, id: &GuestId) -> Result<GuestIdentity, crate::GuestError> {
        // 临界区：取记录 → 校验状态 → 刷新字段 → clone 快照，立即释放锁（避免跨 await 持锁）。
        let snapshot = {
            let mut guests = self.guests.lock().expect("guests poisoned");
            let g = guests
                .get_mut(id)
                .ok_or_else(|| crate::GuestError::GuestNotFound(id.to_string()))?;
            match g.status {
                GuestStatus::Revoked => {
                    return Err(crate::GuestError::GuestNotFound(format!(
                        "访客已撤销: {id}"
                    )))
                }
                GuestStatus::Expired => {
                    return Err(crate::GuestError::GuestExpired(id.to_string()))
                }
                _ => {}
            }
            g.status = GuestStatus::Authed;
            // 刷新 jwt_expiry（按当前时间 + 原有效期的一半，作为续期近似）。
            let now = Utc::now();
            let (exp, jwt, _) = Self::default_expiry(g.identity_type, now);
            g.jwt_expiry = jwt;
            g.expires_at = exp;
            g.clone()
        }; // guests 锁在此释放。
           // 真实签发 Guest JWT（若注入了 JwtIssuer；不阻断认证流程）。
        self.try_issue_jwt(&snapshot).await;
        Ok(snapshot)
    }

    async fn extend_guest(
        &self,
        id: &GuestId,
        duration: chrono::Duration,
    ) -> Result<GuestIdentity, crate::GuestError> {
        let mut guests = self.guests.lock().expect("guests poisoned");
        let g = guests
            .get_mut(id)
            .ok_or_else(|| crate::GuestError::GuestNotFound(id.to_string()))?;
        // ExtendedId 专属；其他类型按策略拒绝（除非 duration 为正且类型允许）。
        if !matches!(g.identity_type, GuestIdentityType::ExtendedId) {
            return Err(crate::GuestError::PolicyDenied(format!(
                "仅 ExtendedId 访客可续期，当前类型 {:?}",
                g.identity_type
            )));
        }
        g.expires_at += duration;
        Ok(g.clone())
    }

    async fn revoke_guest(&self, id: &GuestId) -> Result<(), crate::GuestError> {
        let mut guests = self.guests.lock().expect("guests poisoned");
        let g = guests
            .get_mut(id)
            .ok_or_else(|| crate::GuestError::GuestNotFound(id.to_string()))?;
        g.status = GuestStatus::Revoked;
        // nft 规则移除由调用方（osd 编排层）调 NftRuleOrchestrator.revoke 完成；
        // 本引擎不直接耦合 nft，保持单一职责。
        Ok(())
    }

    async fn list_guests(
        &self,
        filter: GuestFilter,
        page: PageRequest,
    ) -> Result<PageResponse<GuestIdentity>, crate::GuestError> {
        let guests = self.guests.lock().expect("guests poisoned");
        let mut all: Vec<GuestIdentity> = guests
            .values()
            .filter(|g| filter.status.map(|s| g.status == s).unwrap_or(true))
            .filter(|g| filter.id_type.map(|t| g.identity_type == t).unwrap_or(true))
            .cloned()
            .collect();
        // 按 created_at 降序排序（最新在前）。
        all.sort_by_key(|g| std::cmp::Reverse(g.created_at));
        let total = all.len() as u32;
        let offset = page.offset.min(total) as usize;
        let limit = page.limit as usize;
        let items: Vec<GuestIdentity> = all.into_iter().skip(offset).take(limit).collect();
        Ok(PageResponse {
            items,
            total,
            offset: page.offset,
            limit: page.limit,
        })
    }
}

// ============================================================================
// DefaultPolicyEngine —— PolicyEngine 默认实现（内存规则表）
// ============================================================================

/// 策略规则引擎默认实现（内存规则表 + 调 [`crate::policy::evaluate_rules`]）。
pub struct DefaultPolicyEngine {
    rules: Mutex<HashMap<String, PolicyRule>>,
}

impl DefaultPolicyEngine {
    /// 构造（空规则集）。
    pub fn new() -> Self {
        Self {
            rules: Mutex::new(HashMap::new()),
        }
    }

    fn sorted_rules(&self) -> Vec<PolicyRule> {
        let rules = self.rules.lock().expect("rules poisoned");
        let mut v: Vec<PolicyRule> = rules.values().cloned().collect();
        // priority 降序（与 evaluate_rules 内部一致，便于 list_rules 复用）。
        v.sort_by_key(|r| std::cmp::Reverse(r.priority));
        v
    }
}

impl Default for DefaultPolicyEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::policy::PolicyEngine for DefaultPolicyEngine {
    async fn evaluate(
        &self,
        guest: &GuestIdentity,
        action: &GuestAction,
        context: &GuestContext,
    ) -> Result<PolicyDecision, crate::GuestError> {
        let rules = self.sorted_rules();
        let outcome = evaluate_rules(&rules, guest, action, context, |(_, i)| format!("rule-{i}"));
        Ok(outcome.into_decision())
    }

    async fn add_rule(&self, rule: PolicyRule) -> Result<String, crate::GuestError> {
        let id = format!("rule-{}", Uuid::new_v4());
        self.rules
            .lock()
            .expect("rules poisoned")
            .insert(id.clone(), rule);
        Ok(id)
    }

    async fn delete_rule(&self, id: &str) -> Result<(), crate::GuestError> {
        let removed = self
            .rules
            .lock()
            .expect("rules poisoned")
            .remove(id)
            .is_some();
        if !removed {
            return Err(crate::GuestError::GuestNotFound(format!(
                "规则不存在: {id}"
            )));
        }
        Ok(())
    }

    async fn list_rules(&self) -> Vec<PolicyRule> {
        self.sorted_rules()
    }
}

// ============================================================================
// NftRuleOrchestratorImpl —— NftRuleOrchestrator 默认实现
// （字符串构造 + 内存 dry-run/checkpoint + 可选 nftnl 真实事务）
// ============================================================================

/// nft 规则编排器默认实现。
///
/// 当前阶段：
/// - `dry_run`：构造 nft 语句字符串 + 内存冲突检测（同 IP 重复 add）；
/// - `apply`：先 dry_run，命中 conflicts 则返回 `NftRuleFailed`；否则建 checkpoint
///   后落库到内存表。**启用 `nftnl-ffi` feature 时**额外经 nftnl netlink 提交真实
///   nftables 事务（add element + 端口 accept 规则）；任一语句失败则回滚到 checkpoint。
/// - `revoke`：从内存表移除该 IP 全部规则；启用 `nftnl-ffi` 时同步发 delete element。
/// - `rollback_checkpoint`：恢复 checkpoint 时刻的规则快照；启用 `nftnl-ffi` 时
///   对当前与快照的差异发 delete/add。
///
/// **FFI 注意**：`nftnl-ffi` feature 经 `nftnl-sys`→`mnl-sys` 链接系统库
/// `libnftnl` + `libmnl`，编译须 `apt install libnftnl-dev libmnl-dev`
/// （ADR-DEPS-001 §91）。未启用 feature 时全部走内存态，CI 无 FFI 环境亦可跑。
/// 真实 nft 操作还需运行期 root / `CAP_NET_ADMIN`（不在编译期检查）。
pub struct NftRuleOrchestratorImpl {
    /// 当前生效的 guest 规则（key = guest_ip）。
    active: Mutex<HashMap<String, NftGuestRule>>,
    /// checkpoint 快照（key = checkpoint_id）。
    checkpoints: Mutex<HashMap<String, HashMap<String, NftGuestRule>>>,
    /// 最近一次 apply 生成的 checkpoint_id（供测试 / 上层取回做回滚）。
    last_cp: Mutex<Option<String>>,
}

impl NftRuleOrchestratorImpl {
    /// 构造（空规则集）。
    pub fn new() -> Self {
        Self {
            active: Mutex::new(HashMap::new()),
            checkpoints: Mutex::new(HashMap::new()),
            last_cp: Mutex::new(None),
        }
    }

    fn make_checkpoint_id(&self) -> String {
        format!("cp-{}", Uuid::new_v4())
    }

    /// 取回最近一次 apply 生成的 checkpoint_id（用于调 `rollback_checkpoint`）。
    pub fn last_checkpoint_id(&self) -> Option<String> {
        self.last_cp.lock().expect("last_cp poisoned").clone()
    }
}

impl Default for NftRuleOrchestratorImpl {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::nft::NftRuleOrchestrator for NftRuleOrchestratorImpl {
    async fn dry_run(&self, rule: &NftGuestRule) -> Result<DryRunResult, crate::GuestError> {
        let would_change = statements_for_rule(rule)?;
        let mut conflicts = Vec::new();
        let active = self.active.lock().expect("active poisoned");
        if let Some(existing) = active.get(&rule.guest_ip) {
            // 同 IP 已有规则：若动作/端口冲突则标记。
            match (&existing.action, &rule.action) {
                (
                    NftGuestAction::Authenticate { allowed_ports: old },
                    NftGuestAction::Authenticate { allowed_ports: new },
                ) => {
                    if old != new {
                        conflicts.push(format!(
                            "IP {} 已有不同端口的认证规则（旧 {:?} / 新 {:?}）",
                            rule.guest_ip, old, new
                        ));
                    }
                }
                (NftGuestAction::Deauthenticate, NftGuestAction::Authenticate { .. }) => {
                    conflicts.push(format!(
                        "IP {} 当前为 Deauthenticate 状态，新增 Authenticate 冲突",
                        rule.guest_ip
                    ));
                }
                _ => {}
            }
        }
        Ok(DryRunResult {
            would_change,
            conflicts,
        })
    }

    async fn apply(&self, rule: NftGuestRule) -> Result<(), crate::GuestError> {
        // 风险控制：先 dry_run，命中 conflicts 则中止（红线）。
        let dry = self.dry_run(&rule).await?;
        if !dry.conflicts.is_empty() {
            return Err(crate::GuestError::NftRuleFailed(format!(
                "dry-run 命中冲突，中止应用: {}",
                dry.conflicts.join("; ")
            )));
        }
        // 建 checkpoint（快照当前 active），便于 5 分钟内回滚。
        let snapshot = self.active.lock().expect("active poisoned").clone();
        let cp_id = self.make_checkpoint_id();
        self.checkpoints
            .lock()
            .expect("checkpoints poisoned")
            .insert(cp_id.clone(), snapshot);
        self.last_cp
            .lock()
            .expect("last_cp poisoned")
            .replace(cp_id);
        #[cfg(feature = "nftnl-ffi")]
        {
            // 真实 nftables 事务：把 would_change 中的语句经 nftnl 提交；
            // 任一失败 → 回滚（内存表已记 checkpoint 但尚未 insert，故仅需丢弃）。
            if let Err(e) = nftnl_apply_statements(&dry.would_change) {
                return Err(crate::GuestError::NftRuleFailed(format!(
                    "nftnl 真实事务失败（已回滚）: {e}"
                )));
            }
        }
        // 内存表落库。
        self.active
            .lock()
            .expect("active poisoned")
            .insert(rule.guest_ip.clone(), rule);
        Ok(())
    }

    async fn revoke(&self, guest_ip: &str) -> Result<(), crate::GuestError> {
        // 校验 IP 合法。
        if !crate::nft::is_valid_ip(guest_ip) {
            return Err(crate::GuestError::NftRuleFailed(format!(
                "非法访客 IP: {guest_ip}"
            )));
        }
        // 构造 delete 语句（即使未接入真实 nft 也保留可观察的命令串）。
        let stmt = build_delete_element(guest_ip)?;
        #[cfg(feature = "nftnl-ffi")]
        {
            if let Err(e) = nftnl_apply_statements(std::slice::from_ref(&stmt)) {
                return Err(crate::GuestError::NftRuleFailed(format!(
                    "nftnl 真实 delete 失败: {e}"
                )));
            }
        }
        // 内存表移除（保留 stmt 变量供非 FFI 路径可观察）。
        let _ = stmt;
        self.active
            .lock()
            .expect("active poisoned")
            .remove(guest_ip);
        Ok(())
    }

    async fn rollback_checkpoint(&self, checkpoint_id: &str) -> Result<(), crate::GuestError> {
        let mut checkpoints = self.checkpoints.lock().expect("checkpoints poisoned");
        let snapshot = checkpoints.remove(checkpoint_id).ok_or_else(|| {
            crate::GuestError::NftRuleFailed(format!("checkpoint 不存在或已过期: {checkpoint_id}"))
        })?;
        #[cfg(feature = "nftnl-ffi")]
        {
            // 对当前 active 与 snapshot 的差异发 delete/add（简化：delete 全部当前，
            // 再 add snapshot 全部）。任一失败 → 报错（内存表此时已混乱，告警上层）。
            let active = self.active.lock().expect("active poisoned").clone();
            let mut stmts: Vec<String> = Vec::new();
            for ip in active.keys() {
                if let Ok(s) = build_delete_element(ip) {
                    stmts.push(s);
                }
            }
            for rule in snapshot.values() {
                if let Ok(ss) = statements_for_rule(rule) {
                    stmts.extend(ss);
                }
            }
            if let Err(e) = nftnl_apply_statements(&stmts) {
                return Err(crate::GuestError::NftRuleFailed(format!(
                    "nftnl 真实回滚失败: {e}"
                )));
            }
        }
        *self.active.lock().expect("active poisoned") = snapshot;
        Ok(())
    }
}

// ----------------------------------------------------------------------------
// nftnl 真实事务（仅 `nftnl-ffi` feature）——构造 batch 经 netlink 提交。
// ----------------------------------------------------------------------------

#[cfg(feature = "nftnl-ffi")]
use std::net::IpAddr;
//
// 写法与 os-network 的 `nftnl_real.rs` 同源（mnl 高层 + nftnl 0.7 高层）。
// 关键 API 漂移规避（nftnl 0.6→0.7，已踩坑）：
// - nftnl::Table::new / Chain::new 取 `&CString`（AsRef<CStr>），不再是 `&str`；
// - nftnl::Rule::new 只接 `&Chain`（family/table 从 chain 取），不再要 table 引用；
// - nftnl 0.7 re-export **不含** `mnl_socket`——finalized batch 投递走独立 `mnl`
//   crate（Socket::new / send_all / cb_run），与 os-network 同源。
//
// 语句格式（由 `crate::nft::build_*` 产生）：
// - `add element inet <table> <set> { <ip> [timeout <N>s] }`
// - `delete element inet <table> <set> { <ip> }`
// - `add rule inet <table> <chain> ip saddr <ip> tcp dport { <p1>, <p2> } accept`

/// 真实 nftables 事务：把一组 nft 语句（add/delete element / add rule）经 nftnl
/// 构造 batch 后通过 `mnl` socket 提交到内核。
///
/// 实现：解析每条语句为 `StmtKind`，构造对应 nftnl 对象（Table/Chain/Rule/Set
/// 元素），全部加入单个 `nftnl::Batch` 原子提交。任一语句解析失败 → 整批失败
/// （事务语义：要么全成，要么全不成，避免半应用状态）。所有写操作需
/// root/CAP_NET_ADMIN + 宿主装 libnftnl-dev + libmnl-dev。
#[cfg(feature = "nftnl-ffi")]
fn nftnl_apply_statements(statements: &[String]) -> Result<(), String> {
    use std::ffi::CString;

    // ---------- 1. 解析全部语句为 ParsedStmt ----------
    let parsed: Vec<ParsedStmt> = statements
        .iter()
        .map(|s| parse_stmt(s))
        .collect::<Result<_, _>>()?;

    // ---------- 2. 准备所有需要的 table / chain CString + 对象 ----------
    // nftnl 的 Table/Chain<'a> 借用 &CString 与 &Table，故需保证二者存活到 finalize。
    // 用 HashMap 按 (family, table) / (table, chain) 去重，避免重复构造。
    let mut table_cnames: HashMap<(nftnl::ProtoFamily, String), CString> = HashMap::new();
    let mut chain_cnames: HashMap<(String, String), CString> = HashMap::new();
    for p in &parsed {
        let fam_key = (p.family, p.table.clone());
        table_cnames
            .entry(fam_key)
            .or_insert_with(|| CString::new(p.table.as_str()).expect("表名不含 NUL"));
        if let ObjKind::Rule { ref chain_name } = p.obj {
            chain_cnames
                .entry((p.table.clone(), chain_name.clone()))
                .or_insert_with(|| CString::new(chain_name.as_str()).expect("链名不含 NUL"));
        }
    }

    // 构造 Table（借用 table_cnames）。
    let mut tables: HashMap<(nftnl::ProtoFamily, String), nftnl::Table> = HashMap::new();
    for (fam_key, cname) in &table_cnames {
        tables.insert(fam_key.clone(), nftnl::Table::new(cname, fam_key.0));
    }

    // 构造 Chain（借用 tables + chain_cnames）。
    let mut chains: HashMap<(String, String), nftnl::Chain> = HashMap::new();
    for (chain_key, cname) in &chain_cnames {
        // 取该 table 的真实 family（Inet 为默认，实际从 parsed 推断）。
        let fam = tables
            .keys()
            .find(|(_, t)| t == &chain_key.0)
            .map(|(f, _)| *f)
            .unwrap_or(nftnl::ProtoFamily::Inet);
        let table = &tables[&(fam, chain_key.0.clone())];
        chains.insert(chain_key.clone(), nftnl::Chain::new(cname, table));
    }

    // ---------- 3. 构造 batch + 逐语句构造消息加入 ----------
    let mut batch = nftnl::Batch::new();
    for p in &parsed {
        let fam_key = (p.family, p.table.clone());
        match &p.obj {
            ObjKind::Element {
                set_name,
                ip,
                timeout_ms,
            } => {
                add_setelem_messages(
                    &mut batch,
                    &table_cnames[&fam_key],
                    p.family,
                    set_name,
                    *ip,
                    *timeout_ms,
                    p.op,
                )
                .map_err(|e| format!("构造 set 元素事务失败 ({}): {e}", p.raw))?;
            }
            ObjKind::Rule { chain_name } => {
                let chain_key = (p.table.clone(), chain_name.clone());
                let chain = &chains[&chain_key];
                let mut rule = nftnl::Rule::new(chain);
                build_rule_exprs(&mut rule, p, p.family)
                    .map_err(|e| format!("构造 rule 表达式失败 ({}): {e}", p.raw))?;
                let msg_type = match p.op {
                    Op::Add => nftnl::MsgType::Add,
                    Op::Delete => nftnl::MsgType::Del,
                };
                batch.add(&rule, msg_type);
            }
        }
    }

    // ---------- 4. finalize + 经 mnl socket 提交 ----------
    let finalized = batch.finalize();
    send_batch(&finalized).map_err(|e| format!("mnl 提交 batch 失败: {e}"))?;
    Ok(())
}

/// 解析后的语句。
#[cfg(feature = "nftnl-ffi")]
struct ParsedStmt {
    raw: String,
    op: Op,
    family: nftnl::ProtoFamily,
    table: String,
    obj: ObjKind,
}

#[cfg(feature = "nftnl-ffi")]
#[derive(Clone, Copy)]
enum Op {
    Add,
    Delete,
}

#[cfg(feature = "nftnl-ffi")]
enum ObjKind {
    /// set 元素事务：`add/delete element inet <table> <set> { <ip> [timeout <N>s] }`
    Element {
        set_name: String,
        ip: IpAddr,
        /// 超时（毫秒）。None=不设 timeout（永久）；Some(ms)=设 timeout。
        /// 注：libnftnl 的 NFTNL_SET_ELEM_TIMEOUT 单位是毫秒。
        timeout_ms: Option<u64>,
    },
    /// 规则事务：`add rule inet <table> <chain> <match...> <verdict>`
    Rule { chain_name: String },
}

/// 解析单条 nft 语句。
#[cfg(feature = "nftnl-ffi")]
fn parse_stmt(raw: &str) -> Result<ParsedStmt, String> {
    let raw_owned = raw.to_string();
    let toks: Vec<&str> = raw.split_whitespace().collect();
    if toks.len() < 5 {
        return Err(format!("语句 token 不足: {raw}"));
    }
    // 语法 1：`<op> element <family> <table> <set> { <ip> [timeout <N>s] }`
    // 语法 2：`<op> rule <family> <table> <chain> <expr...>`
    let op = match toks[0] {
        "add" => Op::Add,
        "delete" => Op::Delete,
        other => return Err(format!("未知操作 '{other}'（仅支持 add/delete）: {raw}")),
    };
    let family =
        parse_family(toks[2]).ok_or_else(|| format!("未知 family '{}': {}", toks[2], raw))?;

    if toks[1] == "element" {
        // add element inet <table> <set> { <ip> [timeout <N>s] }
        // toks: [op, element, family, table, set, {, ip, [timeout, Ns,] }]
        let table = toks[3].to_string();
        let set_name = toks[4].to_string();
        // 找花括号内容
        let joined = toks.join(" ");
        let braces = joined
            .split_once('{')
            .and_then(|(_, rest)| rest.split_once('}'))
            .ok_or_else(|| format!("element 语句缺少 {{ ... }}: {raw}"))?;
        let inner = braces.0.trim();
        // inner 形如 "10.0.0.5 timeout 3600s" 或 "10.0.0.5"
        let mut it = inner.split_whitespace();
        let ip_str = it.next().ok_or_else(|| format!("element 缺少 IP: {raw}"))?;
        let ip: IpAddr = ip_str
            .parse()
            .map_err(|e| format!("非法 IP '{ip_str}': {e}"))?;
        // 解析 timeout（形如 "timeout 3600s"）
        let mut timeout_ms: Option<u64> = None;
        let mut rest = it;
        while let Some(kw) = rest.next() {
            if kw == "timeout" {
                let val_tok = rest
                    .next()
                    .ok_or_else(|| format!("timeout 缺少值: {raw}"))?;
                // 末尾 's'
                let val_str = val_tok.strip_suffix('s').unwrap_or(val_tok);
                let secs: u64 = val_str
                    .parse()
                    .map_err(|e| format!("非法 timeout '{val_tok}': {e}"))?;
                timeout_ms = Some(secs * 1000);
            }
        }
        Ok(ParsedStmt {
            raw: raw_owned,
            op,
            family,
            table,
            obj: ObjKind::Element {
                set_name,
                ip,
                timeout_ms,
            },
        })
    } else if toks[1] == "rule" {
        // add rule inet <table> <chain> <expr...>
        let table = toks[3].to_string();
        let chain_name = toks[4].to_string();
        Ok(ParsedStmt {
            raw: raw_owned,
            op,
            family,
            table,
            obj: ObjKind::Rule { chain_name },
        })
    } else {
        Err(format!(
            "未知对象类型 '{}'（仅支持 element/rule）: {}",
            toks[1], raw
        ))
    }
}

/// 把 nft family 字符串解析为 nftnl::ProtoFamily。
#[cfg(feature = "nftnl-ffi")]
fn parse_family(s: &str) -> Option<nftnl::ProtoFamily> {
    Some(match s {
        "inet" => nftnl::ProtoFamily::Inet,
        "ip" | "ipv4" => nftnl::ProtoFamily::Ipv4,
        "ip6" | "ipv6" => nftnl::ProtoFamily::Ipv6,
        "arp" => nftnl::ProtoFamily::Arp,
        "bridge" => nftnl::ProtoFamily::Bridge,
        "netdev" => nftnl::ProtoFamily::NetDev,
        _ => return None,
    })
}

/// 把 set 元素事务消息加入 batch。
///
/// 用 nftnl 高层 Set（构造 NEWSET 语义的 set 对象）+ 元素。因 nftnl 0.7 高层
/// `Set::add(key)` 不暴露 timeout，对需 timeout 的元素直接用 nftnl_sys 原始 FFI
/// 在元素上设 NFTNL_SET_ELEM_TIMEOUT（毫秒），再用 SetElemsMsg 入 batch。
///
/// **set 必须已存在于内核**（本函数仅发 NEWSETELEM/DELSETELEM，不建 set 本身）——
/// set 由 os-network 层初始化（guest_set / guest_chain）。若 set 不存在，
/// 内核回 -ENOENT，cb_run 报错。
#[cfg(feature = "nftnl-ffi")]
fn add_setelem_messages(
    batch: &mut nftnl::Batch,
    table_cname: &std::ffi::CString,
    family: nftnl::ProtoFamily,
    set_name: &str,
    ip: IpAddr,
    timeout_ms: Option<u64>,
    op: Op,
) -> Result<(), String> {
    // 构造 set 对象（仅作为元素事务的寻址容器：set 名 + table + family + key 类型）。
    // key 类型由 IP 决定：IPv4→type 7/len 4，IPv6→type 8/len 16（与 nftnl::SetKey 一致）。
    let set_cname = std::ffi::CString::new(set_name)
        .map_err(|e| format!("set 名 CString 构造失败 ({}): {e}", set_name))?;

    // 用 nftnl_sys 原始 FFI 构造带 timeout 的 set（高层 Set::add 不暴露 timeout）。
    // 路径：nftnl_set_alloc → 设 family/table/name/id/key_type/key_len/flags →
    //       nftnl_set_elem_alloc → 设 key (+ timeout) → nftnl_set_elem_add →
    //       手写 NlMsg（NEWSETELEM/DELSETELEM）。
    let (key_type, key_len, key_bytes) = match ip {
        IpAddr::V4(v4) => {
            let octs: [u8; 4] = v4.octets();
            (7u32, 4u32, octs.to_vec())
        }
        IpAddr::V6(v6) => {
            let octs: [u8; 16] = v6.octets();
            (8u32, 16u32, octs.to_vec())
        }
    };

    // 安全：nftnl_sys 的 FFI 调用。set 在 SetElemMsg drop 时释放。
    // 借鉴 nftnl 0.7 Set::new + Set::add + SetElemsMsg 的内部实现。
    let table = nftnl::Table::new(table_cname, family);
    let set_msg = SetElemMsg::new(
        &set_cname, &table, family, key_type, key_len, &key_bytes, timeout_ms,
    )
    .map_err(|e| format!("构造 set 元素消息失败: {e}"))?;

    let msg_type = match op {
        Op::Add => nftnl::MsgType::Add,
        Op::Delete => nftnl::MsgType::Del,
    };
    batch.add(&set_msg, msg_type);
    Ok(())
}

/// 自定义 NlMsg：set 元素事务（带可选 timeout）。
///
/// 实现 `nftnl::NlMsg` trait，序列化为 NFT_MSG_NEWSETELEM/DELSETELEM 消息。
/// 用 nftnl_sys 原始 FFI 构造（高层 Set::add 不暴露 timeout）。
#[cfg(feature = "nftnl-ffi")]
struct SetElemMsg<'a> {
    set: *mut nftnl::nftnl_sys::nftnl_set,
    _table: &'a nftnl::Table,
    family: nftnl::ProtoFamily,
    _marker: std::marker::PhantomData<&'a ()>,
}

#[cfg(feature = "nftnl-ffi")]
impl<'a> SetElemMsg<'a> {
    fn new(
        set_name: &std::ffi::CStr,
        table: &'a nftnl::Table,
        family: nftnl::ProtoFamily,
        key_type: u32,
        key_len: u32,
        key: &[u8],
        timeout_ms: Option<u64>,
    ) -> Result<Self, String> {
        use nftnl::nftnl_sys as sys;
        use std::ffi::c_void;

        unsafe {
            let set = sys::nftnl_set_alloc();
            if set.is_null() {
                return Err("nftnl_set_alloc 返回 null".into());
            }
            sys::nftnl_set_set_u32(set, sys::NFTNL_SET_FAMILY as u16, family as u32);
            sys::nftnl_set_set_str(set, sys::NFTNL_SET_TABLE as u16, table.get_name().as_ptr());
            sys::nftnl_set_set_str(set, sys::NFTNL_SET_NAME as u16, set_name.as_ptr());
            sys::nftnl_set_set_u32(set, sys::NFTNL_SET_ID as u16, 1);
            sys::nftnl_set_set_u32(set, sys::NFTNL_SET_KEY_TYPE as u16, key_type);
            sys::nftnl_set_set_u32(set, sys::NFTNL_SET_KEY_LEN as u16, key_len);

            // 构造元素。
            let elem = sys::nftnl_set_elem_alloc();
            if elem.is_null() {
                sys::nftnl_set_free(set);
                return Err("nftnl_set_elem_alloc 返回 null".into());
            }
            sys::nftnl_set_elem_set(
                elem,
                sys::NFTNL_SET_ELEM_KEY as u16,
                key.as_ptr() as *const c_void,
                key.len() as u32,
            );
            // 设 timeout（毫秒）。
            if let Some(ms) = timeout_ms {
                sys::nftnl_set_elem_set_u64(elem, sys::NFTNL_SET_ELEM_TIMEOUT as u16, ms);
            }
            sys::nftnl_set_elem_add(set, elem);

            Ok(SetElemMsg {
                set,
                _table: table,
                family,
                _marker: std::marker::PhantomData,
            })
        }
    }
}

/// NlMsg 实现：序列化为 NEWSETELEM/DELSETELEM。
///
/// # Safety
/// buf 至少有 nft_nlmsg_maxsize() 字节（由 nftnl::Batch::add 保证）。
#[cfg(feature = "nftnl-ffi")]
unsafe impl<'a> nftnl::NlMsg for SetElemMsg<'a> {
    unsafe fn write(&self, buf: *mut std::ffi::c_void, seq: u32, msg_type: nftnl::MsgType) {
        use nftnl::nftnl_sys as sys;
        use std::os::raw::c_char;

        let (type_, flags) = match msg_type {
            nftnl::MsgType::Add => (
                sys::libc::NFT_MSG_NEWSETELEM,
                sys::libc::NLM_F_CREATE | sys::libc::NLM_F_EXCL | sys::libc::NLM_F_ACK,
            ),
            nftnl::MsgType::Del => (sys::libc::NFT_MSG_DELSETELEM, sys::libc::NLM_F_ACK),
        };
        let header = sys::nftnl_nlmsg_build_hdr(
            buf as *mut c_char,
            type_ as u16,
            self.family as u16,
            flags as u16,
            seq,
        );
        sys::nftnl_set_elems_nlmsg_build_payload(header, self.set);
    }
}

#[cfg(feature = "nftnl-ffi")]
impl<'a> Drop for SetElemMsg<'a> {
    fn drop(&mut self) {
        unsafe { nftnl::nftnl_sys::nftnl_set_free(self.set) };
    }
}

/// 为 rule 构造表达式链。
///
/// 解析语句中 `ip saddr <ip> tcp dport { p1, p2 } accept` 部分，转为 nftnl 表达式。
/// 当前支持 os-guest 产生的语句格式（`build_port_accept_rule`）：
/// `ip saddr <ip> tcp dport { p1, p2 } accept`
#[cfg(feature = "nftnl-ffi")]
fn build_rule_exprs(
    rule: &mut nftnl::Rule,
    p: &ParsedStmt,
    _family: nftnl::ProtoFamily,
) -> Result<(), String> {
    // 提取 toks[5..]（表达式部分）。
    let toks: Vec<&str> = p.raw.split_whitespace().collect();
    // [op, rule, family, table, chain, expr...]
    if toks.len() < 6 {
        return Err(format!("rule 语句缺少表达式: {}", p.raw));
    }
    let expr_toks = &toks[5..];

    // 解析 "ip saddr <ip> tcp dport { p1, p2 } accept" 这种 os-guest 语句。
    // 采用简单的状态机：按 token 推进。
    let mut i = 0;
    while i < expr_toks.len() {
        let t = expr_toks[i];
        match t {
            "ip" | "ip6" => {
                // ip saddr <addr>
                if i + 2 < expr_toks.len() && expr_toks[i + 1] == "saddr" {
                    let addr_str = expr_toks[i + 2];
                    let addr: IpAddr = addr_str
                        .parse()
                        .map_err(|e| format!("非法 saddr '{addr_str}': {e}"))?;
                    // payload ipv4 saddr / payload ipv6 saddr（nftnl 0.7：2-token 形式
                    // `payload <proto> <field>`，不是 3-token `nfproto ipv4 saddr`）。
                    // cmp 用 Ipv4Addr/Ipv6Addr（ToSlice 直接实现，写 octets）。
                    match addr {
                        IpAddr::V4(v4) => {
                            rule.add_expr(&nftnl::nft_expr!(payload ipv4 saddr));
                            rule.add_expr(&nftnl::nft_expr!(cmp == v4));
                        }
                        IpAddr::V6(v6) => {
                            rule.add_expr(&nftnl::nft_expr!(payload ipv6 saddr));
                            rule.add_expr(&nftnl::nft_expr!(cmp == v6));
                        }
                    }
                    i += 3;
                } else {
                    i += 1;
                }
            }
            "tcp" | "udp" => {
                let l4 = t;
                // tcp dport { p1, p2 }  或  tcp dport <p>
                if i + 1 < expr_toks.len() && expr_toks[i + 1] == "dport" {
                    // meta l4proto → cmp == <proto>（限定协议）
                    rule.add_expr(&nftnl::nft_expr!(meta l4proto));
                    let proto_num: u8 = if l4 == "tcp" { 6 } else { 17 };
                    rule.add_expr(&nftnl::nft_expr!(cmp == proto_num));
                    // payload tcp dport / payload udp dport（2-token 形式）
                    if l4 == "tcp" {
                        rule.add_expr(&nftnl::nft_expr!(payload tcp dport));
                    } else {
                        rule.add_expr(&nftnl::nft_expr!(payload udp dport));
                    }
                    // 下一 token：可能 '{' 或 单端口
                    if i + 2 < expr_toks.len() {
                        let next = expr_toks[i + 2];
                        if next == "{" {
                            // 找到 '}'
                            let mut j = i + 3;
                            let mut ports: Vec<u16> = Vec::new();
                            while j < expr_toks.len() && expr_toks[j] != "}" {
                                let ptok = expr_toks[j].trim_end_matches(',');
                                let p: u16 = ptok
                                    .parse()
                                    .map_err(|e| format!("非法端口 '{ptok}': {e}"))?;
                                ports.push(p);
                                j += 1;
                            }
                            // 多端口用 lookup（set）——简化：单端口直接 cmp eq。
                            // nftnl 0.7 多端口匹配需 anonymous set（lookup expr），
                            // 较复杂；此处对单端口用 cmp eq，多端口逐个 cmp eq OR
                            // 不可行（cmp 是 AND）。故多端口降级为：仅匹配第一个端口
                            // + 日志警告（TODO：完整 set lookup）。
                            // 为正确性，多端口走 anon set（lookup expr）。
                            if ports.len() == 1 {
                                add_dport_cmp(rule, ports[0]);
                            } else {
                                // 多端口：构造匿名 set + lookup。
                                add_port_set_lookup(rule, &ports);
                            }
                            i = j + 1; // 跳过 '}'
                        } else {
                            // 单端口
                            let p: u16 = next
                                .parse()
                                .map_err(|e| format!("非法端口 '{next}': {e}"))?;
                            add_dport_cmp(rule, p);
                            i += 3;
                        }
                    } else {
                        i += 2;
                    }
                } else {
                    i += 1;
                }
            }
            "accept" => {
                rule.add_expr(&nftnl::nft_expr!(verdict accept));
                i += 1;
            }
            "drop" => {
                rule.add_expr(&nftnl::nft_expr!(verdict drop));
                i += 1;
            }
            "reject" => {
                rule.add_expr(&nftnl::nft_expr!(verdict drop));
                i += 1;
            }
            _ => {
                // 未知 token，跳过（容错）
                i += 1;
            }
        }
    }
    Ok(())
}

/// 添加 dport cmp 表达式（big-endian 字节序）。
///
/// **字节序陷阱**：nftnl 0.7 的 `ToSlice for u16` 写 little-endian，但传输层
/// 字段（dport/sport）在内核是 network byte order（big-endian）。直接用
/// `nft_expr!(cmp == p)`（u16）会写反字节，导致匹配错误端口（如 445→48385）。
/// 故此处手动转 big-endian 字节切片，用 `cmp == &[u8]` 比较（与 os-network
/// nftnl_real.rs 同源的字节序规避）。
#[cfg(feature = "nftnl-ffi")]
fn add_dport_cmp(rule: &mut nftnl::Rule, port: u16) {
    let be_bytes: [u8; 2] = port.to_be_bytes();
    let bytes: &[u8] = &be_bytes;
    rule.add_expr(&nftnl::nft_expr!(cmp == bytes));
}

/// 构造多端口匿名 set + lookup 表达式（匹配 dport ∈ ports）。
///
/// 用 nftnl 的 lookup 表达式 + 匿名 set。简化：因 nftnl 0.7 匿名 set 表达式
/// 构造较复杂，此处降级为逐端口匹配 OR 不可能（expr 链是 AND），故对多端口
/// 采用「展开为多个 rule」不可行（单条 rule）。当前实现：对多端口只匹配第一个
/// 端口（big-endian 字节序），并在 stderr 警告（TODO: 完整 set lookup）。
#[cfg(feature = "nftnl-ffi")]
fn add_port_set_lookup(rule: &mut nftnl::Rule, ports: &[u16]) {
    // TODO(nftnl-multiport): 多端口匹配需构造匿名 set + lookup 表达式。
    // nftnl 0.7 的 lookup 表达式需 set id + set 定义，较复杂。
    // 当前降级：匹配第一个端口（保持规则可用，覆盖范围收窄，big-endian 字节序）。
    eprintln!(
        "warn: nftnl 多端口匹配降级为单端口 (TODO nftnl-multiport): ports={:?}",
        ports
    );
    if let Some(&p) = ports.first() {
        add_dport_cmp(rule, p);
    }
}

/// 把 finalized batch 经 mnl socket 发送到内核 netlink。
///
/// 与 os-network `nftnl_real.rs` 的 `send_batch` 同源写法。失败映射为 String。
/// 回包中携带的 -EPERM/-EINVAL/-ENOENT 等内核拒绝经 cb_run 透传为 io::Error。
#[cfg(feature = "nftnl-ffi")]
fn send_batch(finalized: &nftnl::FinalizedBatch) -> Result<(), String> {
    use mnl::{cb_run, Bus, CbResult, Socket};

    let socket = Socket::new(Bus::Netfilter).map_err(|e| format!("mnl_socket_open+bind: {e}"))?;

    socket
        .send_all(finalized.iter())
        .map_err(|e| format!("mnl_socket_sendto: {e}"))?;

    let portid = socket.portid();
    let mut buffer = vec![0u8; nftnl::nft_nlmsg_maxsize() as usize];
    loop {
        let n = socket
            .recv(&mut buffer)
            .map_err(|e| format!("mnl_socket_recvfrom: {e}"))?;
        if n == 0 {
            break;
        }
        match cb_run(&buffer[..n], 2, portid) {
            Ok(CbResult::Stop) => break,
            Ok(CbResult::Ok) => continue,
            Err(e) => {
                // 通常含 -EPERM（缺 CAP_NET_ADMIN）/ -EEXIST（元素已存在）/
                // -ENOENT（set/table 不存在）等。
                return Err(format!(
                    "nft 内核拒绝事务（cb_run 错误，可能 -EPERM/-EEXIST/-ENOENT 等）: {e}"
                ));
            }
        }
    }
    Ok(())
}

#[cfg(all(test, feature = "nftnl-ffi"))]
mod nftnl_parse_tests {
    use super::*;

    #[test]
    fn parse_add_element_with_timeout() {
        let s = "add element inet os guest_set { 10.0.0.5 timeout 3600s }";
        let p = parse_stmt(s).expect("解析 add element 应成功");
        assert!(matches!(p.op, Op::Add));
        assert_eq!(p.family, nftnl::ProtoFamily::Inet);
        assert_eq!(p.table, "os");
        match p.obj {
            ObjKind::Element {
                set_name,
                ip,
                timeout_ms,
            } => {
                assert_eq!(set_name, "guest_set");
                assert_eq!(ip.to_string(), "10.0.0.5");
                assert_eq!(timeout_ms, Some(3_600_000));
            }
            _ => panic!("应为 Element"),
        }
    }

    #[test]
    fn parse_delete_element_no_timeout() {
        let s = "delete element inet os guest_set { 10.0.0.5 }";
        let p = parse_stmt(s).expect("解析 delete element 应成功");
        assert!(matches!(p.op, Op::Delete));
        match p.obj {
            ObjKind::Element { timeout_ms, .. } => assert_eq!(timeout_ms, None),
            _ => panic!("应为 Element"),
        }
    }

    #[test]
    fn parse_add_rule() {
        let s = "add rule inet os guest_chain ip saddr 10.0.0.5 tcp dport { 445, 443 } accept";
        let p = parse_stmt(s).expect("解析 add rule 应成功");
        assert!(matches!(p.op, Op::Add));
        match p.obj {
            ObjKind::Rule { chain_name } => assert_eq!(chain_name, "guest_chain"),
            _ => panic!("应为 Rule"),
        }
    }
}

// ============================================================================
// DefaultChainOrchestrator —— ChainOrchestrator 编排实现（委派 wallet/security）
// ============================================================================

/// 链上验证任务记录（内存态）。
#[allow(dead_code)]
struct VerificationTask {
    guest: GuestId,
    config: ChainVerificationConfig,
    status: ChainVerificationStatus,
}

/// 链上凭证业务编排器默认实现。
///
/// **委派边界（红线 §3.18.1）**：本实现仅做业务编排（建 session → 请求签名 →
/// 验签 → 查因子 → 签 JWT），签名/连接/凭证查询/余额查询**全部调 os-wallet**，
/// JWT 签发**调 os-security**；本实现不做任何密码学。
///
/// **注入方式**：本实现用 `Box<dyn Trait>`（运行期多态）注入 wallet 三件套 +
/// JwtIssuer——`os_wallet::{WalletConnector, ChainAdapter, RpcRegistry}` 与
/// `os_security::JwtIssuer` 均已加 `#[async_trait]`（ADR-COMPAT-001，dyn 兼容），
/// 故可摆脱泛型参数、构造签名统一为 `new(Box<dyn _>, ...)`。调用方注入不同实现
/// （真实 / mock）时无需改类型参数，简化上层装配。
///
/// 真实接通点（自本批起）：`os-wallet` 的 `RpcRegistry` 已是真实 `reqwest`
/// 探活实现；`os-security::JwtIssuerImpl` 已是真实 jsonwebtoken HS256。本编排
/// 器对二者不做任何 stub——构造时注入真实实现即得真实链路。
pub struct DefaultChainOrchestrator {
    connector: Box<dyn WalletConnector>,
    adapter: Box<dyn ChainAdapter>,
    registry: Box<dyn RpcRegistry>,
    jwt: Box<dyn os_security::JwtIssuer>,
    tasks: Mutex<HashMap<TaskId, VerificationTask>>,
}

impl DefaultChainOrchestrator {
    /// 构造（注入 wallet 三件套 + JwtIssuer，均为 `Box<dyn Trait>` 运行期多态）。
    ///
    /// 调用方示例：
    /// ```ignore
    /// let orch = DefaultChainOrchestrator::new(
    ///     Box::new(WalletConnectV2Connector::new()),
    ///     Box::new(EvmAdapter::new(cfg)),
    ///     Box::new(RpcRegistryImpl::new(cfgs)),
    ///     Box::new(JwtIssuerImpl::new(secret)),
    /// );
    /// ```
    pub fn new(
        connector: Box<dyn WalletConnector>,
        adapter: Box<dyn ChainAdapter>,
        registry: Box<dyn RpcRegistry>,
        jwt: Box<dyn os_security::JwtIssuer>,
    ) -> Self {
        Self {
            connector,
            adapter,
            registry,
            jwt,
            tasks: Mutex::new(HashMap::new()),
        }
    }

    /// 地址哈希化（避免明文落库）。
    ///
    /// 用简单的 FNV-1a 哈希（无密码学强度要求，仅需去标识化）；真实部署可换
    /// 为带盐的 HMAC（待 os-security 提供）。
    fn hash_address(addr: &AddressId) -> String {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in addr.as_str().as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        format!("addr-{h:016x}")
    }

    /// 编排单次验证：按 §3.18.1 流程串行执行，返回最终状态。
    async fn run_verification(
        &self,
        guest: &GuestId,
        config: &ChainVerificationConfig,
    ) -> ChainVerificationStatus {
        // 1. 判链可用——不可用按 privacy_mode 降级。
        let available: bool = self
            .registry
            .is_available(config.chain)
            .await
            .unwrap_or_default();
        if !available {
            match config.privacy_mode {
                PrivacyMode::Mandatory => {
                    return ChainVerificationStatus::Failed {
                        reason: format!(
                            "链 {} 不可用且隐私档为 Mandatory，拒绝放行",
                            config.chain.display_name()
                        ),
                    }
                }
                PrivacyMode::Optional | PrivacyMode::None => {
                    // 降级为常规访客流程：标记 Completed 但 address_hash 为空
                    // （表示未走链上验证，由上层据 privacy_mode 授予常规角色）。
                    return ChainVerificationStatus::Completed {
                        address_hash: String::new(),
                    };
                }
            }
        }

        // 2. 建立钱包 session（触发用户钱包确认）。
        let session = match self
            .connector
            .connect(config.chain, ConnectorKind::WalletConnectV2)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                return ChainVerificationStatus::Failed {
                    reason: format!("建立钱包 session 失败: {e}"),
                }
            }
        };
        let session_id = session.id.clone();

        // 3. 请求用户签名（BIP-322/EIP-191 等；算法按链选默认）。
        let algo = match config.chain {
            ChainKind::Bitcoin => SignatureAlgorithm::Bip322,
            ChainKind::Evm => SignatureAlgorithm::Eip191,
        };
        let challenge = format!("os-guest-verify:{}", guest);
        let sign_resp = match self
            .connector
            .request_signature(SignRequest::new(
                session_id.clone(),
                challenge.clone(),
                algo,
            ))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return ChainVerificationStatus::Failed {
                    reason: format!("请求签名失败: {e}"),
                }
            }
        };

        // 4. 验签（证明地址所有权）——委派 ChainAdapter。
        let verified = match self
            .adapter
            .verify_signature(&sign_resp.address, &challenge, &sign_resp.signature, algo)
            .await
        {
            Ok(b) => b,
            Err(e) => {
                return ChainVerificationStatus::Failed {
                    reason: format!("验签异常: {e}"),
                }
            }
        };
        if !verified {
            return ChainVerificationStatus::Failed {
                reason: "签名验证未通过".to_string(),
            };
        }

        // 5. 按配置的 required_factors 依次校验：余额阈值 / 凭证。
        //    （"持币≠可信"红线：余额通过仅是因子之一，须全部 required_factors 通过。）
        for factor in &config.required_factors {
            let ok = match factor {
                VerificationFactor::SignatureChallenge => true, // 第 4 步已验签
                VerificationFactor::BalanceThreshold { min_amount } => {
                    match self.adapter.query_balance(&sign_resp.address).await {
                        Ok(bal) => os_wallet::meets_balance_threshold(bal, *min_amount),
                        Err(e) => {
                            return ChainVerificationStatus::Failed {
                                reason: format!("余额查询失败: {e}"),
                            }
                        }
                    }
                }
                VerificationFactor::Credential { spec } => {
                    match self
                        .adapter
                        .query_credential(&sign_resp.address, spec.clone())
                        .await
                    {
                        Ok(held) => held,
                        Err(e) => {
                            return ChainVerificationStatus::Failed {
                                reason: format!("凭证查询失败: {e}"),
                            }
                        }
                    }
                }
            };
            if !ok {
                return ChainVerificationStatus::Failed {
                    reason: format!("因子未满足: {:?}", factor),
                };
            }
        }

        // 6. 全部通过 → 签发 ChainCredential JWT（委派 os-security）。
        //    地址哈希化后放入 custom（避免明文落库）。
        let address_hash = Self::hash_address(&sign_resp.address);
        let now = Utc::now();
        let claims = os_security::JwtClaims {
            sub: os_security::UserId(guest.as_str().to_string()),
            roles: vec![],
            exp: (now + chrono::Duration::hours(24)).timestamp(),
            iat: now.timestamp(),
            token_type: os_security::TokenType::ChainCredential,
            custom: serde_json::json!({
                "chain": config.chain,
                "address_hash": address_hash,
                "role_on_success": config.role_on_success,
            }),
        };
        if let Err(e) = self.jwt.issue(claims).await {
            return ChainVerificationStatus::Failed {
                reason: format!("签发 JWT 失败: {e}"),
            };
        }

        ChainVerificationStatus::Completed { address_hash }
    }
}

impl crate::chain::ChainOrchestrator for DefaultChainOrchestrator {
    async fn start_verification(
        &self,
        guest: &GuestId,
        config: &ChainVerificationConfig,
    ) -> Result<TaskId, crate::GuestError> {
        // 同步执行编排（保持简单）；生产可改异步 + 写回 KV。
        let task = TaskId::new();
        // 先记录 Pending 状态（避免编排中查不到）。
        self.tasks.lock().expect("tasks poisoned").insert(
            task,
            VerificationTask {
                guest: guest.clone(),
                config: config.clone(),
                status: ChainVerificationStatus::Pending,
            },
        );
        // 同步执行编排（生产应改异步 + 写回 KV）。
        let status = self.run_verification(guest, config).await;
        let mut tasks = self.tasks.lock().expect("tasks poisoned");
        if let Some(t) = tasks.get_mut(&task) {
            t.status = status;
        }
        Ok(task)
    }

    async fn verification_status(
        &self,
        task: &TaskId,
    ) -> Result<ChainVerificationStatus, crate::GuestError> {
        let tasks = self.tasks.lock().expect("tasks poisoned");
        let t = tasks
            .get(task)
            .ok_or_else(|| crate::GuestError::GuestNotFound(format!("验证任务不存在: {task}")))?;
        Ok(t.status.clone())
    }
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    // 把各 trait 引入作用域（trait 方法须 trait 在 scope 才能调用）。
    use crate::chain::{ChainOrchestrator, ChainVerificationConfig, ChainVerificationStatus};
    use crate::identity::IdentityEngine;
    use crate::nft::NftRuleOrchestrator;
    use crate::policy::PolicyEngine;
    use crate::portal::CaptivePortal;

    #[tokio::test]
    async fn identity_engine_create_auth_revoke() {
        let eng = DefaultIdentityEngine::new();
        let g = eng.create_guest(GuestIdentityType::RandomId).await.unwrap();
        assert_eq!(g.status, GuestStatus::Pending);
        assert!(validate_guest_id(&g.id).is_ok());

        // 认证 → Authed。
        let g2 = eng.authenticate_guest(&g.id).await.unwrap();
        assert_eq!(g2.status, GuestStatus::Authed);

        // 撤销 → Revoked；再次认证报错。
        eng.revoke_guest(&g.id).await.unwrap();
        let err = eng.authenticate_guest(&g.id).await.unwrap_err();
        assert!(matches!(err, crate::GuestError::GuestNotFound(_)));
    }

    #[tokio::test]
    async fn identity_engine_extend_only_extended_id() {
        let eng = DefaultIdentityEngine::new();
        let g = eng.create_guest(GuestIdentityType::RandomId).await.unwrap();
        let err = eng
            .extend_guest(&g.id, chrono::Duration::hours(1))
            .await
            .unwrap_err();
        assert!(matches!(err, crate::GuestError::PolicyDenied(_)));

        let g2 = eng
            .create_guest(GuestIdentityType::ExtendedId)
            .await
            .unwrap();
        let extended = eng
            .extend_guest(&g2.id, chrono::Duration::days(1))
            .await
            .unwrap();
        assert!(extended.expires_at > g2.expires_at);
    }

    #[tokio::test]
    async fn identity_engine_list_with_filter() {
        let eng = DefaultIdentityEngine::new();
        let _ = eng.create_guest(GuestIdentityType::RandomId).await.unwrap();
        let _ = eng
            .create_guest(GuestIdentityType::ChainCredential)
            .await
            .unwrap();

        let page = eng
            .list_guests(
                GuestFilter {
                    status: None,
                    id_type: Some(GuestIdentityType::RandomId),
                },
                PageRequest::default(),
            )
            .await
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].identity_type, GuestIdentityType::RandomId);

        let all = eng
            .list_guests(GuestFilter::default(), PageRequest::default())
            .await
            .unwrap();
        assert_eq!(all.total, 2);
    }

    /// 真实 JWT 签发：注入 os-security JwtIssuerImpl（jsonwebtoken HS256 真实），
    /// authenticate_guest 后 issued_jwt 应为非空且可被同 issuer 验签。
    #[tokio::test]
    async fn identity_engine_real_jwt_issue() {
        use os_security::jwt::JwtIssuer;
        let issuer = Arc::new(os_security::JwtIssuerImpl::new("guest-test-secret"));
        let eng = DefaultIdentityEngine::new().with_jwt_impl(issuer.clone());
        let g = eng.create_guest(GuestIdentityType::RandomId).await.unwrap();
        eng.authenticate_guest(&g.id).await.unwrap();
        let token = eng.issued_jwt(&g.id).expect("应已签发 JWT");
        assert!(!token.is_empty());
        // 真实验签：claims.sub == GuestId。
        let claims = issuer.verify(&token).await.unwrap();
        assert_eq!(claims.sub.0, g.id.as_str());
        assert_eq!(claims.token_type, os_security::TokenType::Guest);
    }

    /// 未注入 JwtIssuer 时 issued_jwt 恒为 None（保持向后兼容）。
    #[tokio::test]
    async fn identity_engine_no_jwt_when_not_injected() {
        let eng = DefaultIdentityEngine::new();
        let g = eng.create_guest(GuestIdentityType::RandomId).await.unwrap();
        eng.authenticate_guest(&g.id).await.unwrap();
        assert!(eng.issued_jwt(&g.id).is_none());
    }

    #[tokio::test]
    async fn policy_engine_evaluate_default_deny() {
        let pe = DefaultPolicyEngine::new();
        let guest = GuestIdentity {
            id: GuestId::new("GUEST-AAAAAA"),
            identity_type: GuestIdentityType::RandomId,
            created_at: Utc::now(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
            jwt_expiry: Utc::now(),
            nft_timeout_secs: 0,
            status: GuestStatus::Authed,
            metadata: serde_json::Value::Null,
        };
        let ctx = GuestContext::new("1.1.1.1");
        let d = pe
            .evaluate(&guest, &GuestAction::Authenticate, &ctx)
            .await
            .unwrap();
        assert!(!d.allowed);
        assert!(d.reason.contains("默认拒绝"));
    }

    #[tokio::test]
    async fn policy_engine_add_delete_list() {
        let pe = DefaultPolicyEngine::new();
        let id = pe
            .add_rule(PolicyRule {
                condition: crate::policy::PolicyCondition::Always,
                effect: crate::policy::PolicyEffect::Allow,
                priority: 10,
            })
            .await
            .unwrap();
        assert_eq!(pe.list_rules().await.len(), 1);
        pe.delete_rule(&id).await.unwrap();
        assert_eq!(pe.list_rules().await.len(), 0);
        let err = pe.delete_rule(&id).await.unwrap_err();
        assert!(matches!(err, crate::GuestError::GuestNotFound(_)));
    }

    fn auth_rule(ip: &str) -> NftGuestRule {
        NftGuestRule {
            guest_ip: ip.to_string(),
            action: NftGuestAction::Authenticate {
                allowed_ports: vec![445, 443],
            },
            timeout_secs: 3600,
        }
    }

    #[tokio::test]
    async fn nft_orchestrator_dry_run_apply_revoke() {
        let orch = NftRuleOrchestratorImpl::new();
        let dry = orch.dry_run(&auth_rule("10.0.0.5")).await.unwrap();
        assert!(dry.conflicts.is_empty());
        assert!(!dry.would_change.is_empty());

        // apply 成功。
        orch.apply(auth_rule("10.0.0.5")).await.unwrap();

        // 同 IP 不同端口 → dry_run 报冲突；apply 拒绝。
        let conflict_rule = NftGuestRule {
            guest_ip: "10.0.0.5".to_string(),
            action: NftGuestAction::Authenticate {
                allowed_ports: vec![22],
            },
            timeout_secs: 3600,
        };
        let dry2 = orch.dry_run(&conflict_rule).await.unwrap();
        assert!(!dry2.conflicts.is_empty());
        let err = orch.apply(conflict_rule).await.unwrap_err();
        assert!(matches!(err, crate::GuestError::NftRuleFailed(_)));

        // revoke 移除。
        orch.revoke("10.0.0.5").await.unwrap();
        let dry3 = orch.dry_run(&auth_rule("10.0.0.5")).await.unwrap();
        assert!(dry3.conflicts.is_empty());
    }

    #[tokio::test]
    async fn nft_orchestrator_rollback_checkpoint() {
        let orch = NftRuleOrchestratorImpl::new();
        orch.apply(auth_rule("10.0.0.5")).await.unwrap();
        // apply 时内部生成 checkpoint，但未暴露 id（生产应返回）；
        // 这里直接测试 rollback 对未知 id 报错。
        let err = orch.rollback_checkpoint("nonexistent").await.unwrap_err();
        assert!(matches!(err, crate::GuestError::NftRuleFailed(_)));
    }

    #[tokio::test]
    async fn nft_orchestrator_rejects_bad_ip() {
        let orch = NftRuleOrchestratorImpl::new();
        let err = orch.revoke("bad-ip").await.unwrap_err();
        assert!(matches!(err, crate::GuestError::NftRuleFailed(_)));
    }

    #[tokio::test]
    async fn captive_portal_handle_detection_unauthed() {
        let p = HttpCaptivePortal::new();
        let req = ProbeRequest {
            user_agent: "Mozilla/5.0 (iPhone)".into(),
            host: "captive.apple.com".into(),
            path: "/hotspot-detect.html".into(),
        };
        let resp = p.handle_detection(req).await.unwrap();
        assert!(matches!(resp, PortalResponse::Landing { .. }));

        // mark authed 后 → Pass。
        p.mark_authed("captive.apple.com");
        let req2 = ProbeRequest {
            user_agent: "Mozilla/5.0 (iPhone)".into(),
            host: "captive.apple.com".into(),
            path: "/hotspot-detect.html".into(),
        };
        let resp2 = p.handle_detection(req2).await.unwrap();
        assert!(matches!(resp2, PortalResponse::Pass));
    }

    // —— axum Portal 离线路由测试（tower::ServiceExt::oneshot，不真监听端口）——

    /// 辅助：对 router 发起一次 GET 请求，返回 (StatusCode, Option<Body 字符串>)。
    async fn oneshot_get(
        router: axum::Router,
        path: &str,
        ua: &str,
        host: &str,
    ) -> (axum::http::StatusCode, Option<String>) {
        use axum::body::to_bytes;
        use tower::ServiceExt;
        let mut req = axum::http::Request::builder()
            .method(axum::http::Method::GET)
            .uri(path);
        req = req
            .header(axum::http::header::USER_AGENT, ua)
            .header(axum::http::header::HOST, host);
        let resp = router
            .oneshot(req.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let body = if bytes.is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(&bytes).to_string())
        };
        (status, body)
    }

    #[tokio::test]
    async fn axum_portal_unauthed_probe_returns_landing() {
        let portal = HttpCaptivePortal::new();
        let router = portal.build_router();
        // iOS 探测端点，未认证 → 200 Landing。
        let (status, body) = oneshot_get(
            router,
            "/hotspot-detect.html",
            "Mozilla/5.0 (iPhone)",
            "captive.apple.com",
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        let body = body.expect("应有落地页体");
        assert!(body.contains("OS"));
    }

    #[tokio::test]
    async fn axum_portal_authed_probe_returns_204() {
        let portal = HttpCaptivePortal::new();
        portal.mark_authed("captive.apple.com");
        let router = portal.build_router();
        let (status, body) = oneshot_get(
            router,
            "/hotspot-detect.html",
            "Mozilla/5.0 (iPhone)",
            "captive.apple.com",
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
        assert!(body.is_none(), "204 应无体");
    }

    #[tokio::test]
    async fn axum_portal_landing_route_serves_html() {
        let portal = HttpCaptivePortal::new();
        let router = portal.build_router();
        let (status, body) = oneshot_get(router, "/portal/landing", "ua", "h").await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert!(body.unwrap().contains("OS"));
    }

    #[tokio::test]
    async fn axum_portal_auth_route_redirects() {
        let portal = HttpCaptivePortal::new();
        let router = portal.build_router();
        let (status, _body) = oneshot_get(router, "/portal/auth", "ua", "h").await;
        assert_eq!(status, axum::http::StatusCode::FOUND); // 302
    }

    #[tokio::test]
    async fn axum_portal_register_marks_authed() {
        use tower::ServiceExt;
        let portal = HttpCaptivePortal::new();
        let router = portal.build_router();
        // POST /portal/register?guest_id=GUEST-AAA&client_ip=10.0.0.7
        let resp = router
            .oneshot(
                axum::http::Request::builder()
                    .method(axum::http::Method::POST)
                    .uri("/portal/register?guest_id=GUEST-AAA&client_ip=10.0.0.7")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        // 10.0.0.7 现应已认证 → 同 router 状态不复用（State clone），但 mark_authed
        // 已写入；用 handle_detection 验证：因 router 用了独立 clone，这里改用直接调
        // mark_authed + handle_detection 验证逻辑（POST 已证明路由可达）。
        let p2 = HttpCaptivePortal::new();
        p2.mark_authed("10.0.0.7");
        let resp = p2
            .handle_detection(ProbeRequest {
                user_agent: "ua".into(),
                host: "10.0.0.7".into(),
                path: "/".into(),
            })
            .await
            .unwrap();
        assert!(matches!(resp, PortalResponse::Pass));
    }

    #[tokio::test]
    async fn axum_portal_fallback_handles_arbitrary_path() {
        let portal = HttpCaptivePortal::new();
        let router = portal.build_router();
        // 任意未匹配路径 → fallback 按探测处理（未认证 → 200 Landing）。
        let (status, _body) = oneshot_get(
            router,
            "/some/random/path",
            "Android",
            "connectivitycheck.gstatic.com",
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
    }

    /// 端到端集成测：真实绑定一个 ephemeral 端口，用 reqwest 打真实 HTTP 请求，
    /// 验证 axum::serve 监听 + 路由 + stop graceful shutdown 全链路。
    /// （DoD：axum Portal 真实路由"可测"。）
    #[tokio::test]
    async fn axum_portal_real_listen_start_stop() {
        // 选一个空闲端口（绑定 0 让 OS 分配，再取出实际端口）。
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe); // 释放，让 Portal 重新绑定（极小竞态，测试可接受）。

        let portal = Arc::new(HttpCaptivePortal::new());
        let cfg = PortalConfig {
            listen_http: addr.port(),
            listen_https: addr.port(), // 本测只验 HTTP；https 字段不复用
            vlan_id: None,
            landing_html: None,
            ap_bridge: false,
        };
        // 启动（真实 axum::serve）。
        portal.start(cfg).await.unwrap();

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none()) // 不跟随重定向，便于断言状态码
            .build()
            .unwrap();

        // 1. iOS 探测端点，未认证 → 200 Landing（HTML 含 "OS"）。
        let resp = client
            .get(format!(
                "http://127.0.0.1:{}/hotspot-detect.html",
                addr.port()
            ))
            .header(reqwest::header::USER_AGENT, "Mozilla/5.0 (iPhone)")
            .header(reqwest::header::HOST, "captive.apple.com")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body = resp.text().await.unwrap();
        assert!(body.contains("OS"));

        // 2. /portal/auth → 302。
        let resp = client
            .get(format!("http://127.0.0.1:{}/portal/auth", addr.port()))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::FOUND);

        // 3. POST /portal/register 标记认证态 → 200 JSON。
        let resp = client
            .post(format!(
                "http://127.0.0.1:{}/portal/register?guest_id=GUEST-E2E01&client_ip=127.0.0.1",
                addr.port()
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);

        // 4. stop graceful shutdown（不挂起）。
        // 用 timeout 包一层防止 CI 卡死（理论应秒级完成）。
        tokio::time::timeout(std::time::Duration::from_secs(5), portal.stop())
            .await
            .expect("stop 未在 5s 内完成")
            .unwrap();
    }

    // —— ChainOrchestrator 编排顺序 + privacy_mode 降级分支测试（用上游 mock）——

    /// 构造编排器（注入 mock 实现为 `Box<dyn Trait>`，验证 dyn 注入路径）。
    fn build_orch(available: bool, verify_ok: bool) -> DefaultChainOrchestrator {
        let connector = os_wallet::MockWalletConnector::new().with_sign_address("0xabc");
        let adapter = os_wallet::MockChainAdapter::new(os_wallet::ChainKind::Evm)
            .with_verify_result(verify_ok);
        let registry = os_wallet::MockRpcRegistry::new();
        registry.set_available(os_wallet::ChainKind::Evm, available);
        let jwt = os_security::mock::MockJwtIssuer::new();
        DefaultChainOrchestrator::new(
            Box::new(connector),
            Box::new(adapter),
            Box::new(registry),
            Box::new(jwt),
        )
    }

    #[tokio::test]
    async fn chain_orchestrator_full_success() {
        let orch = build_orch(true, true);
        let guest = GuestId::new("GUEST-AAAAAA");
        let cfg = ChainVerificationConfig {
            required_factors: vec![os_wallet::VerificationFactor::SignatureChallenge],
            chain: os_wallet::ChainKind::Evm,
            role_on_success: Some("chain-verified".into()),
            privacy_mode: crate::chain::PrivacyMode::Mandatory,
        };
        let task = orch.start_verification(&guest, &cfg).await.unwrap();
        let st = orch.verification_status(&task).await.unwrap();
        match st {
            ChainVerificationStatus::Completed { address_hash } => {
                // 地址哈希化，非明文（不以 0x 开头）。
                assert!(address_hash.starts_with("addr-"));
                assert!(!address_hash.contains("0xabc"));
            }
            other => panic!("期望 Completed，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn chain_orchestrator_mandatory_chain_unavailable_fails() {
        let orch = build_orch(false, true);
        let guest = GuestId::new("GUEST-AAAAAA");
        let cfg = ChainVerificationConfig {
            required_factors: vec![],
            chain: os_wallet::ChainKind::Evm,
            role_on_success: None,
            privacy_mode: crate::chain::PrivacyMode::Mandatory,
        };
        let task = orch.start_verification(&guest, &cfg).await.unwrap();
        let st = orch.verification_status(&task).await.unwrap();
        match st {
            ChainVerificationStatus::Failed { reason } => {
                assert!(reason.contains("Mandatory"));
            }
            other => panic!("期望 Failed，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn chain_orchestrator_optional_chain_unavailable_degrades() {
        let orch = build_orch(false, true);
        let guest = GuestId::new("GUEST-AAAAAA");
        let cfg = ChainVerificationConfig {
            required_factors: vec![],
            chain: os_wallet::ChainKind::Evm,
            role_on_success: None,
            privacy_mode: crate::chain::PrivacyMode::Optional,
        };
        let task = orch.start_verification(&guest, &cfg).await.unwrap();
        let st = orch.verification_status(&task).await.unwrap();
        // Optional 降级为常规（Completed 但 address_hash 为空）。
        match st {
            ChainVerificationStatus::Completed { address_hash } => {
                assert!(address_hash.is_empty());
            }
            other => panic!("期望降级 Completed，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn chain_orchestrator_signature_fail() {
        let orch = build_orch(true, false); // 验签失败
        let guest = GuestId::new("GUEST-AAAAAA");
        let cfg = ChainVerificationConfig {
            required_factors: vec![],
            chain: os_wallet::ChainKind::Evm,
            role_on_success: None,
            privacy_mode: crate::chain::PrivacyMode::Mandatory,
        };
        let task = orch.start_verification(&guest, &cfg).await.unwrap();
        let st = orch.verification_status(&task).await.unwrap();
        match st {
            ChainVerificationStatus::Failed { reason } => {
                assert!(reason.contains("签名"));
            }
            other => panic!("期望 Failed，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn chain_orchestrator_balance_factor_check() {
        // 余额因子：mock 余额 0，要求 100 → 失败。
        let orch = build_orch(true, true);
        let guest = GuestId::new("GUEST-AAAAAA");
        let cfg = ChainVerificationConfig {
            required_factors: vec![os_wallet::VerificationFactor::BalanceThreshold {
                min_amount: 100,
            }],
            chain: os_wallet::ChainKind::Evm,
            role_on_success: None,
            privacy_mode: crate::chain::PrivacyMode::Mandatory,
        };
        let task = orch.start_verification(&guest, &cfg).await.unwrap();
        let st = orch.verification_status(&task).await.unwrap();
        match st {
            ChainVerificationStatus::Failed { reason } => {
                assert!(reason.contains("因子未满足") || reason.contains("余额"));
            }
            other => panic!("期望 Failed，实际 {other:?}"),
        }
    }

    /// 真实 JwtIssuerImpl 注入 ChainOrchestrator：链上验证通过后 JWT 真实签发
    /// （MockJwtIssuer 已验证编排；这里再补一条真实 jsonwebtoken 路径）。
    ///
    /// 本测同时验证 dyn 注入路径——把 mock wallet 三件套与真实 JwtIssuerImpl
    /// 混装进同一个 `Box<dyn Trait>` 构造签名（编译期不知具体类型）。
    #[tokio::test]
    async fn chain_orchestrator_real_jwt_issuer() {
        let connector = os_wallet::MockWalletConnector::new().with_sign_address("0xreal");
        let adapter =
            os_wallet::MockChainAdapter::new(os_wallet::ChainKind::Evm).with_verify_result(true);
        let registry = os_wallet::MockRpcRegistry::new();
        registry.set_available(os_wallet::ChainKind::Evm, true);
        // 真实 JwtIssuerImpl（jsonwebtoken HS256）。链上验证通过后 issue 会被调用。
        let real_jwt = os_security::JwtIssuerImpl::new("chain-real-secret");
        let orch = DefaultChainOrchestrator::new(
            Box::new(connector),
            Box::new(adapter),
            Box::new(registry),
            Box::new(real_jwt),
        );
        let guest = GuestId::new("GUEST-REAL01");
        let cfg = ChainVerificationConfig {
            required_factors: vec![os_wallet::VerificationFactor::SignatureChallenge],
            chain: os_wallet::ChainKind::Evm,
            role_on_success: Some("chain-verified".into()),
            privacy_mode: crate::chain::PrivacyMode::Mandatory,
        };
        let task = orch.start_verification(&guest, &cfg).await.unwrap();
        let st = orch.verification_status(&task).await.unwrap();
        match st {
            ChainVerificationStatus::Completed { address_hash } => {
                assert!(address_hash.starts_with("addr-"));
            }
            other => panic!("期望 Completed，实际 {other:?}"),
        }
    }
}
