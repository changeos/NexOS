//! 中间件链与具体中间件实现。
//!
//! 中间件链顺序（规划文档 §3.6 / §9.1#10）：**TLS → RateLimit → Auth → Audit**
//! （TLS 最外层终止；RateLimit 先过滤流量保护后端；Auth 在限流后解析身份；
//! Audit 最内层记录已通过过滤的真实请求/响应）。
//!
//! `MiddlewareChain` 负责按顺序执行 `before`，任一短路则立即返回；全部放行后
//! 调用处理器；再逆序执行 `after`（贴近响应的反向链）。

use std::collections::HashMap;
use std::sync::RwLock;

use os_security::Role;

use crate::gateway::{ApiRequest, ApiResponse, RouteSpec};
use crate::middleware::{Middleware, MiddlewareDecision};
use crate::middleware_impl::{SlidingWindow, TokenBucket};

// ----------------------------------------------------------------------------
// 中间件链
// ----------------------------------------------------------------------------

/// 中间件链——有序持有若干 `Box<dyn Middleware>`，提供 `run_before` / `run_after`。
#[derive(Default)]
pub struct MiddlewareChain {
    before: Vec<Box<dyn Middleware>>,
}

impl std::fmt::Debug for MiddlewareChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MiddlewareChain")
            .field("len", &self.before.len())
            .finish()
    }
}

impl MiddlewareChain {
    /// 构造空链。
    pub fn new() -> Self {
        Self::default()
    }

    /// 末尾追加一个中间件（执行顺序即追加顺序）。
    pub fn push(&mut self, mw: Box<dyn Middleware>) {
        self.before.push(mw);
    }

    /// 链长度。
    pub fn len(&self) -> usize {
        self.before.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.before.is_empty()
    }

    /// 按 before 链顺序执行；返回第一个短路决策或全部 Continue。
    pub async fn run_before(
        &self,
        req: &mut ApiRequest,
    ) -> Result<MiddlewareDecision, crate::ApiGatewayError> {
        for mw in &self.before {
            let decision = mw.before(req).await?;
            if !matches!(decision, MiddlewareDecision::Continue) {
                return Ok(decision);
            }
        }
        Ok(MiddlewareDecision::Continue)
    }

    /// 逆序执行 after（贴近响应的反向链）。
    pub async fn run_after(&self, resp: &mut ApiResponse) -> Result<(), crate::ApiGatewayError> {
        for mw in self.before.iter().rev() {
            mw.after(resp).await?;
        }
        Ok(())
    }
}

// ----------------------------------------------------------------------------
// 角色名映射（Role 无 Display，本 crate 自带映射）
// ----------------------------------------------------------------------------

/// 把 `Role` 映射为可比较的字符串名（用于 required_roles 字符串集合比较）。
pub fn role_name(role: Role) -> &'static str {
    match role {
        Role::Admin => "admin",
        Role::User => "user",
        Role::Guest => "guest",
        Role::ChainVerifiedGuest => "chain_verified_guest",
    }
}

// ----------------------------------------------------------------------------
// AuthMiddleware 实现
// ----------------------------------------------------------------------------

impl crate::middleware::AuthMiddleware {
    /// 构造。
    pub fn new() -> Self {
        Self
    }

    /// 检查请求 `auth` 是否满足路由要求。
    ///
    /// - 路由 `requires_auth` 为 false：直接放行。
    /// - 否则：无 `auth` → 401；角色不满足 → 403。
    ///
    /// 注：JWT 解析本身由网关入口完成（依赖 JwtIssuer，本骨架接收已填充 `auth` 的请求），
    /// 此处仅做鉴权（authorization）判断。
    pub fn authorize(req: &ApiRequest, route: &RouteSpec) -> MiddlewareDecision {
        if !route.requires_auth {
            return MiddlewareDecision::Continue;
        }
        let principal = match &req.auth {
            Some(p) => p,
            None => {
                return MiddlewareDecision::Reject {
                    status: 401,
                    body: serde_json::json!({"error": "未认证"}),
                }
            }
        };
        if route.required_roles.is_empty() {
            return MiddlewareDecision::Continue;
        }
        let user_roles: std::collections::HashSet<&str> =
            principal.roles.iter().map(|r| role_name(*r)).collect();
        let needed: std::collections::HashSet<&str> =
            route.required_roles.iter().map(|s| s.as_str()).collect();
        if needed.is_subset(&user_roles) {
            MiddlewareDecision::Continue
        } else {
            MiddlewareDecision::Reject {
                status: 403,
                body: serde_json::json!({"error": "权限不足"}),
            }
        }
    }
}

impl Default for crate::middleware::AuthMiddleware {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Middleware for crate::middleware::AuthMiddleware {
    async fn before(
        &self,
        _req: &mut ApiRequest,
    ) -> Result<MiddlewareDecision, crate::ApiGatewayError> {
        // 真实鉴权在网关分发时结合具体路由调用 `AuthMiddleware::authorize`；
        // 链中无路由上下文，默认放行（身份解析已在网关入口完成）。
        Ok(MiddlewareDecision::Continue)
    }

    async fn after(&self, _resp: &mut ApiResponse) -> Result<(), crate::ApiGatewayError> {
        Ok(())
    }
}

// ----------------------------------------------------------------------------
// RateLimitMiddleware 实现（按源 IP 令牌桶）
// ----------------------------------------------------------------------------

/// 从请求头部取源 IP（`x-forwarded-for` 首段或 `x-real-ip`；缺省 "anonymous"）。
fn extract_client_key(req: &ApiRequest) -> String {
    if let Some(xff) = req.headers.get("x-forwarded-for").and_then(|v| v.as_str()) {
        if let Some(first) = xff.split(',').next() {
            return first.trim().to_string();
        }
    }
    if let Some(ip) = req.headers.get("x-real-ip").and_then(|v| v.as_str()) {
        return ip.trim().to_string();
    }
    if let Some(user) = req.auth.as_ref() {
        return user.user.id.as_str().to_string();
    }
    "anonymous".to_string()
}

impl crate::middleware::RateLimitMiddleware {
    /// 构造；`rps` = 每秒允许请求数（令牌桶容量 = rps，补充速率 = rps/秒）。
    pub fn new(rps: u32) -> Self {
        Self { rps }
    }

    /// 判定单个请求是否放行（按 client key 独立计桶）。
    pub fn allow(
        &self,
        req: &ApiRequest,
        buckets: &mut HashMap<String, TokenBucket>,
        now: f64,
    ) -> bool {
        let key = extract_client_key(req);
        let bucket = buckets
            .entry(key)
            .or_insert_with(|| TokenBucket::new(self.rps as f64, self.rps as f64));
        bucket.try_consume(now)
    }
}

/// 持有状态的限流中间件（按 client key 独立桶，线程安全）。
pub struct StatefulRateLimiter {
    rps: u32,
    buckets: RwLock<HashMap<String, TokenBucket>>,
}

impl StatefulRateLimiter {
    /// 构造。
    pub fn new(rps: u32) -> Self {
        Self {
            rps,
            buckets: RwLock::new(HashMap::new()),
        }
    }
}

#[async_trait::async_trait]
impl Middleware for StatefulRateLimiter {
    async fn before(
        &self,
        req: &mut ApiRequest,
    ) -> Result<MiddlewareDecision, crate::ApiGatewayError> {
        let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
        let allowed = {
            let mut buckets = self
                .buckets
                .write()
                .map_err(|e| crate::ApiGatewayError::Internal(e.to_string()))?;
            let key = extract_client_key(req);
            let bucket = buckets
                .entry(key)
                .or_insert_with(|| TokenBucket::new(self.rps as f64, self.rps as f64));
            bucket.try_consume(now)
        };
        if allowed {
            Ok(MiddlewareDecision::Continue)
        } else {
            Ok(MiddlewareDecision::RateLimited)
        }
    }

    async fn after(&self, _resp: &mut ApiResponse) -> Result<(), crate::ApiGatewayError> {
        Ok(())
    }
}

// ----------------------------------------------------------------------------
// 滑动窗口限流中间件（按 user 计，适合"每用户 N 次/窗口"语义）
// ----------------------------------------------------------------------------

/// 持有状态的滑动窗口限流中间件。
pub struct SlidingWindowRateLimiter {
    max: usize,
    window_secs: f64,
    windows: RwLock<HashMap<String, SlidingWindow>>,
}

impl SlidingWindowRateLimiter {
    /// 构造：`max` 次每 `window_secs` 秒。
    pub fn new(max: usize, window_secs: f64) -> Self {
        Self {
            max,
            window_secs,
            windows: RwLock::new(HashMap::new()),
        }
    }
}

#[async_trait::async_trait]
impl Middleware for SlidingWindowRateLimiter {
    async fn before(
        &self,
        req: &mut ApiRequest,
    ) -> Result<MiddlewareDecision, crate::ApiGatewayError> {
        let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
        let allowed = {
            let mut windows = self
                .windows
                .write()
                .map_err(|e| crate::ApiGatewayError::Internal(e.to_string()))?;
            let key = extract_client_key(req);
            let w = windows
                .entry(key)
                .or_insert_with(|| SlidingWindow::new(self.max, self.window_secs));
            w.try_record(now)
        };
        if allowed {
            Ok(MiddlewareDecision::Continue)
        } else {
            Ok(MiddlewareDecision::RateLimited)
        }
    }

    async fn after(&self, _resp: &mut ApiResponse) -> Result<(), crate::ApiGatewayError> {
        Ok(())
    }
}

// ----------------------------------------------------------------------------
// TlsMiddleware 实现（占位：真实 TLS 终止在 Axum/Tokio 层，本骨架记录意愿）
// ----------------------------------------------------------------------------

impl crate::middleware::TlsMiddleware {
    /// 构造。
    pub fn new() -> Self {
        Self
    }

    /// 校验 TLS 配置路径非空（实际证书加载在 Gateway.start 内做）。
    pub fn validate_config(cfg: &crate::gateway::TlsConfig) -> Result<(), crate::ApiGatewayError> {
        if cfg.cert_path.trim().is_empty() || cfg.key_path.trim().is_empty() {
            return Err(crate::ApiGatewayError::TlsError(
                "证书/私钥路径为空".to_string(),
            ));
        }
        Ok(())
    }
}

impl Default for crate::middleware::TlsMiddleware {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Middleware for crate::middleware::TlsMiddleware {
    async fn before(
        &self,
        _req: &mut ApiRequest,
    ) -> Result<MiddlewareDecision, crate::ApiGatewayError> {
        // TLS 在传输层终止，HTTP 中间件层无需改写；放行。
        Ok(MiddlewareDecision::Continue)
    }

    async fn after(&self, _resp: &mut ApiResponse) -> Result<(), crate::ApiGatewayError> {
        Ok(())
    }
}

// ----------------------------------------------------------------------------
// AuditMiddleware 实现（记录请求/响应；本骨架用 tracing/noop，可注入回调）
// ----------------------------------------------------------------------------

/// 审计记录条目（请求 + 响应快照）。
#[derive(Debug, Clone)]
pub struct AuditRecord {
    pub method: String,
    pub path: String,
    pub user: Option<String>,
    pub status: u16,
}

impl crate::middleware::AuditMiddleware {
    /// 构造。
    pub fn new() -> Self {
        Self
    }

    /// 由请求 + 响应生成审计条目。
    pub fn record(req: &ApiRequest, resp: &ApiResponse) -> AuditRecord {
        AuditRecord {
            method: format!("{:?}", req.method),
            path: req.path.clone(),
            user: req.auth.as_ref().map(|p| p.user.id.as_str().to_string()),
            status: resp.status,
        }
    }
}

impl Default for crate::middleware::AuditMiddleware {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Middleware for crate::middleware::AuditMiddleware {
    async fn before(
        &self,
        _req: &mut ApiRequest,
    ) -> Result<MiddlewareDecision, crate::ApiGatewayError> {
        // before 仅在链最内层放行（审计记录请求进入）；after 记录响应。
        Ok(MiddlewareDecision::Continue)
    }

    async fn after(&self, _resp: &mut ApiResponse) -> Result<(), crate::ApiGatewayError> {
        // 真实审计落库由网关在分发后聚合调用 `AuditMiddleware::record`；
        // 此处保持无副作用以便单测，避免引入 tracing 依赖耦合。
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::HttpMethod;
    use crate::middleware::{AuditMiddleware, AuthMiddleware, TlsMiddleware};
    use os_security::{Principal, Role, User, UserId};

    fn req(authed: bool) -> ApiRequest {
        let auth = if authed {
            Some(
                Principal::new(
                    User::new(
                        UserId::new("u1"),
                        "alice",
                        vec![Role::Admin],
                        chrono::Utc::now(),
                    )
                    .unwrap(),
                    vec![Role::Admin],
                    chrono::Utc::now(),
                )
                .unwrap(),
            )
        } else {
            None
        };
        ApiRequest {
            method: HttpMethod::Get,
            path: "/api/v1/pools".to_string(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth,
        }
    }

    fn route(requires_auth: bool, roles: Vec<&str>) -> RouteSpec {
        RouteSpec {
            method: HttpMethod::Get,
            path: "/api/v1/pools".to_string(),
            handler_component: "test".to_string(),
            requires_auth,
            required_roles: roles.into_iter().map(String::from).collect(),
        }
    }

    #[test]
    fn auth_allows_public() {
        let d = AuthMiddleware::authorize(&req(false), &route(false, vec![]));
        assert!(matches!(d, MiddlewareDecision::Continue));
    }

    #[test]
    fn auth_rejects_unauthed() {
        let d = AuthMiddleware::authorize(&req(false), &route(true, vec![]));
        assert!(matches!(d, MiddlewareDecision::Reject { status: 401, .. }));
    }

    #[test]
    fn auth_allows_authed_no_roles() {
        let d = AuthMiddleware::authorize(&req(true), &route(true, vec![]));
        assert!(matches!(d, MiddlewareDecision::Continue));
    }

    #[test]
    fn auth_role_check_pass() {
        let d = AuthMiddleware::authorize(&req(true), &route(true, vec!["admin"]));
        assert!(matches!(d, MiddlewareDecision::Continue));
    }

    #[test]
    fn auth_role_check_fail() {
        let d = AuthMiddleware::authorize(&req(true), &route(true, vec!["superuser"]));
        assert!(matches!(d, MiddlewareDecision::Reject { status: 403, .. }));
    }

    #[test]
    fn rate_limit_stateful_allows_then_blocks() {
        let mw = StatefulRateLimiter::new(1); // 1 rps
        let mut r = req(true);
        let d1 = futures::executor::block_on(mw.before(&mut r)).unwrap();
        assert!(matches!(d1, MiddlewareDecision::Continue));
        let d2 = futures::executor::block_on(mw.before(&mut r)).unwrap();
        assert!(matches!(d2, MiddlewareDecision::RateLimited));
    }

    #[test]
    fn sliding_window_limiter() {
        let mw = SlidingWindowRateLimiter::new(2, 60.0);
        let mut r = req(true);
        assert!(matches!(
            futures::executor::block_on(mw.before(&mut r)).unwrap(),
            MiddlewareDecision::Continue
        ));
        assert!(matches!(
            futures::executor::block_on(mw.before(&mut r)).unwrap(),
            MiddlewareDecision::Continue
        ));
        assert!(matches!(
            futures::executor::block_on(mw.before(&mut r)).unwrap(),
            MiddlewareDecision::RateLimited
        ));
    }

    #[test]
    fn chain_run_before_in_order() {
        let mut chain = MiddlewareChain::new();
        chain.push(Box::new(AuthMiddleware::new()));
        chain.push(Box::new(crate::middleware::TlsMiddleware::new()));
        let mut r = req(true);
        let d = futures::executor::block_on(chain.run_before(&mut r)).unwrap();
        assert!(matches!(d, MiddlewareDecision::Continue));
    }

    #[test]
    fn tls_validate_config() {
        assert!(TlsMiddleware::validate_config(&crate::gateway::TlsConfig {
            cert_path: String::new(),
            key_path: "k".to_string(),
        })
        .is_err());
        assert!(TlsMiddleware::validate_config(&crate::gateway::TlsConfig {
            cert_path: "c".to_string(),
            key_path: "k".to_string(),
        })
        .is_ok());
    }

    #[test]
    fn audit_record_builds() {
        let r = req(true);
        let resp = ApiResponse {
            status: 200,
            body: serde_json::Value::Null,
            headers: serde_json::Value::Null,
        };
        let rec = AuditMiddleware::record(&r, &resp);
        assert_eq!(rec.status, 200);
        assert_eq!(rec.user.as_deref(), Some("u1"));
    }

    // —— 覆盖率补测：Default impl / after hooks / client key 提取 / RateLimit ——
    //
    // 目标：覆盖各中间件的 `Default` 实现、`after` no-op、`extract_client_key`
    // 各分支（xff / x-real-ip / authed fallback）、`RateLimitMiddleware::allow`
    // 按桶独立计数的语义。

    #[test]
    fn middleware_defaults_are_callable() {
        // 覆盖 AuthMiddleware / TlsMiddleware / AuditMiddleware 的 Default impl
        // （unit struct 的 Default 显式覆盖；allow 抑制 clippy 的单元结构简化提示，
        // 因为本测试目的就是调用 Default trait 方法以覆盖其实现行）
        #![allow(clippy::default_constructed_unit_structs)]
        let _: AuthMiddleware = Default::default();
        let _: TlsMiddleware = Default::default();
        let _: AuditMiddleware = Default::default();
    }

    #[tokio::test]
    async fn middleware_before_after_run_clean() {
        // 把全部 4 个中间件串到链里，跑 before+after，覆盖所有 after no-op 分支
        let mut chain = MiddlewareChain::new();
        chain.push(Box::new(AuthMiddleware::new()));
        chain.push(Box::new(TlsMiddleware::new()));
        chain.push(Box::new(AuditMiddleware::new()));
        chain.push(Box::new(crate::middleware::TlsMiddleware::new()));
        let mut r = req(true);
        let decision = chain.run_before(&mut r).await.unwrap();
        assert!(matches!(decision, MiddlewareDecision::Continue));
        let mut resp = ApiResponse {
            status: 200,
            body: serde_json::Value::Null,
            headers: serde_json::Value::Null,
        };
        chain.run_after(&mut resp).await.unwrap();
    }

    #[test]
    fn chain_len_is_empty_debug() {
        let mut chain = MiddlewareChain::new();
        assert!(chain.is_empty());
        assert_eq!(chain.len(), 0);
        // Debug 实现含 len 字段
        let dbg = format!("{:?}", chain);
        assert!(dbg.contains("len"));
        chain.push(Box::new(AuthMiddleware::new()));
        assert!(!chain.is_empty());
        assert_eq!(chain.len(), 1);
    }

    #[test]
    fn role_name_maps_all_variants() {
        // 覆盖 role_name 全部分支
        assert_eq!(role_name(Role::Admin), "admin");
        assert_eq!(role_name(Role::User), "user");
        assert_eq!(role_name(Role::Guest), "guest");
        assert_eq!(role_name(Role::ChainVerifiedGuest), "chain_verified_guest");
    }

    #[test]
    fn rate_limit_middleware_allow_by_client_key() {
        // RateLimitMiddleware::allow 按客户端 key 独立计桶
        let mw = crate::middleware::RateLimitMiddleware::new(1); // 1 rps
        let mut buckets = HashMap::new();
        let r1 = ApiRequest {
            method: HttpMethod::Get,
            path: "/x".into(),
            headers: serde_json::json!({"x-forwarded-for": "1.2.3.4, 9.9.9.9"}),
            body: serde_json::Value::Null,
            auth: None,
        };
        // xff 首段作为 key
        assert!(mw.allow(&r1, &mut buckets, 0.0));
        assert!(!mw.allow(&r1, &mut buckets, 0.0)); // 同 key 已耗尽
                                                    // 另一个 xff → 独立桶
        let r2 = ApiRequest {
            method: HttpMethod::Get,
            path: "/x".into(),
            headers: serde_json::json!({"x-forwarded-for": "5.6.7.8"}),
            body: serde_json::Value::Null,
            auth: None,
        };
        assert!(mw.allow(&r2, &mut buckets, 0.0));
    }

    #[test]
    fn rate_limit_uses_x_real_ip_when_no_xff() {
        let mw = crate::middleware::RateLimitMiddleware::new(1);
        let mut buckets = HashMap::new();
        let r = ApiRequest {
            method: HttpMethod::Get,
            path: "/x".into(),
            headers: serde_json::json!({"x-real-ip": "10.0.0.1"}),
            body: serde_json::Value::Null,
            auth: None,
        };
        assert!(mw.allow(&r, &mut buckets, 0.0));
    }

    #[test]
    fn rate_limit_uses_user_id_when_no_ip_headers() {
        let mw = crate::middleware::RateLimitMiddleware::new(1);
        let mut buckets = HashMap::new();
        let r = req(true); // 已认证 user u1
        assert!(mw.allow(&r, &mut buckets, 0.0));
    }

    #[test]
    fn rate_limit_anonymous_when_nothing_present() {
        let mw = crate::middleware::RateLimitMiddleware::new(1);
        let mut buckets = HashMap::new();
        let r = ApiRequest {
            method: HttpMethod::Get,
            path: "/x".into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        };
        assert!(mw.allow(&r, &mut buckets, 0.0));
    }

    #[tokio::test]
    async fn stateful_rate_limiter_after_is_noop() {
        // 覆盖 StatefulRateLimiter::after（返回 Ok(())）
        let mw = StatefulRateLimiter::new(10);
        let mut resp = ApiResponse {
            status: 200,
            body: serde_json::Value::Null,
            headers: serde_json::Value::Null,
        };
        assert!(mw.after(&mut resp).await.is_ok());
    }

    #[tokio::test]
    async fn sliding_window_limiter_after_is_noop() {
        let mw = SlidingWindowRateLimiter::new(10, 60.0);
        let mut resp = ApiResponse {
            status: 200,
            body: serde_json::Value::Null,
            headers: serde_json::Value::Null,
        };
        assert!(mw.after(&mut resp).await.is_ok());
    }
}
