//! 中间件——tower 风格（规划文档 §3.6 / §9.1#10）
//!
//! 每个中间件实现 before/after 钩子：before 可改写/拒绝请求，after 可改写响应。
//! 具体中间件用 struct 声明（实现本 trait 由 owner agent 填充）。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::gateway::{ApiRequest, ApiResponse};

// ----------------------------------------------------------------------------
// 中间件决策
// ----------------------------------------------------------------------------

/// before 钩子的决策
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MiddlewareDecision {
    /// 放行（请求可被 before 改写后继续）
    Continue,
    /// 拒绝（短路返回，附状态码与响应体）
    Reject {
        /// HTTP 状态码
        status: u16,
        /// 响应体
        body: serde_json::Value,
    },
    /// 限流命中（短路，通常返回 429）
    RateLimited,
}

// ----------------------------------------------------------------------------
// Middleware trait（async，tower 风格）
// ----------------------------------------------------------------------------

/// 中间件——请求/响应前后钩子。
///
/// 实现者：见下方具体中间件 struct。
/// 经 `MiddlewareChain` 以 `Box<dyn Middleware>` 持有，故用 `#[async_trait]`
/// 保证 dyn 兼容（ADR-COMPAT-001）。
#[async_trait]
pub trait Middleware: Send + Sync {
    /// 请求到达处理器之前（可改写 req 或短路拒绝）。
    async fn before(
        &self,
        req: &mut ApiRequest,
    ) -> Result<MiddlewareDecision, crate::ApiGatewayError>;

    /// 响应返回客户端之前（可改写 resp）。
    async fn after(&self, resp: &mut ApiResponse) -> Result<(), crate::ApiGatewayError>;
}

// ----------------------------------------------------------------------------
// 具体中间件（仅声明 struct，实现由 owner agent 填充）
// ----------------------------------------------------------------------------

/// 认证中间件——解析 JWT/Session，填充 `ApiRequest.auth`（Principal）。
pub struct AuthMiddleware;

/// 限流中间件——按源 IP/用户令牌桶限流，超限返回 429。
pub struct RateLimitMiddleware {
    /// 每秒请求数上限
    pub rps: u32,
}

/// TLS 终止中间件——在网关层做 TLS 握手/卸载。
pub struct TlsMiddleware;

/// 审计中间件——记录请求/响应到审计日志（呼应 §3.16 安全审计）。
pub struct AuditMiddleware;
