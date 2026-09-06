//! HTTP 传输层——`HttpTransport` trait + reqwest 真实实现。
//!
//! 设计动机（接通真实实现）：
//! - [`crate::client_impl::HttpOsClient`] 经网关 REST 调用 os-api（§3.15），需要把
//!   「构造好的 [`crate::http::RequestSpec`]」真正发出去。本模块把「发请求 → 拿响应」
//!   这一**有副作用**的环节抽成 `HttpTransport` trait，便于：
//!   1. 真实实现 [`ReqwestTransport`] 用 `reqwest::Client`（rustls-tls，ADR-DEPS-001）。
//!   2. 测试注入 `FakeTransport`（离线 fixture，确定性，不真发网络请求）。
//! - **重试编排**留在 [`crate::client_impl::HttpOsClient`] 层（它持有会话/策略，
//!   调 [`crate::retry::decide_retry`] 纯决策）；传输层只负责「发一次」，并把
//!   reqwest 的错误归一成 [`crate::retry::RetryableError`]，使重试决策与具体客户端解耦。
//! - WS 订阅（PushSubscriber 长连接）走另一通道，不在本 trait 范围。
//!
//! `HttpTransport` 用 `#[async_trait]` 以支持 `Box<dyn HttpTransport>` 运行期注入
//! （ADR-COMPAT-001：`Box<dyn>` 用的 async trait 一律 `#[async_trait]`）。

use async_trait::async_trait;

use crate::http::{JsonResponse, RequestSpec};
use crate::retry::RetryableError;
use crate::MobileError;

// ----------------------------------------------------------------------------
// 一次传输结果
// ----------------------------------------------------------------------------

/// `HttpTransport::send` 的失败——带可重试分类，供重试循环决策。
///
/// 设计：reqwest 的错误类别（连接/超时/DNS/状态码）很多，重试决策只需「这错误可重试吗 +
/// 是哪类」。本结构把「原始错误消息」与「分类」打包，避免传输层把 `reqwest::Error`
/// 直接漏到上层（上层不应耦合具体 HTTP 客户端）。
#[derive(Debug, Clone)]
pub struct TransportError {
    /// 人类可读错误消息（用于映射到 `MobileError::EndpointUnreachable`）。
    pub message: String,
    /// 错误分类（用于 [`crate::retry::decide_retry`]）。
    pub kind: RetryableError,
}

impl TransportError {
    /// 构造。
    #[must_use]
    pub fn new(message: impl Into<String>, kind: RetryableError) -> Self {
        Self {
            message: message.into(),
            kind,
        }
    }
}

/// 一次 HTTP 交换的结果：成功返回 `JsonResponse`，失败返回 `TransportError`。
pub type TransportResult = Result<JsonResponse, TransportError>;

// ----------------------------------------------------------------------------
// HttpTransport trait
// ----------------------------------------------------------------------------

/// HTTP 传输——把一个 [`RequestSpec`]（相对网关根）发到指定 base URL，返回 JSON 响应。
///
/// 职责（单次请求，不含重试）：
/// - 用 `base_url` + `RequestSpec` 拼完整 URL（复用 [`crate::http::RequestSpec::build_url`]）。
/// - 按方法/查询/header/body 发请求（GET 无 body；POST 用 `body` 字节作 JSON）。
/// - 收响应：状态码 + 字节体打包成 [`JsonResponse`]。
/// - 错误归一：reqwest 失败 / 非 2xx 状态码 → [`TransportError`]（带 `RetryableError` 分类）。
///
/// 重试由 [`crate::client_impl::HttpOsClient`] 在调用方侧编排（它知道策略与会话）。
#[async_trait]
pub trait HttpTransport: Send + Sync {
    /// 发送一次请求。
    async fn send(&self, base_url: &str, req: &RequestSpec) -> TransportResult;
}

// ----------------------------------------------------------------------------
// ReqwestTransport——reqwest::Client 真实实现
// ----------------------------------------------------------------------------

/// reqwest 真实 HTTP 传输（rustls-tls，无 openssl，见 ADR-DEPS-001）。
///
/// 持有一个 `reqwest::Client`（内部连接池复用）。`new` 用默认 builder；调用方需要
/// 自定义（如代理/超时/自定义根证书）时用 [`ReqwestTransport::with_client`] 注入。
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    /// 用默认 reqwest 客户端构造（rustls-tls，跟随重定向，默认无代理）。
    ///
    /// 失败（极少见：TLS 后端初始化失败）映射到 `MobileError::Internal`。
    pub fn new() -> Result<Self, MobileError> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| MobileError::Internal(format!("reqwest Client 构造失败: {e}")))?;
        Ok(Self { client })
    }

    /// 注入预构造的 `reqwest::Client`（自定义超时/代理/根证书等）。
    #[must_use]
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }

    /// 把 reqwest::Error 归一成 [`TransportError`]（分类连接/超时/DNS）。
    fn classify_reqwest_error(err: reqwest::Error) -> TransportError {
        // reqwest 暴露 is_connect / is_timeout / is_request；DNS 归入 connect 类
        // （reqwest 未细分 DNS，统一视作「连接阶段失败」——可重试）。
        if err.is_timeout() {
            TransportError::new(err.to_string(), RetryableError::Timeout)
        } else if err.is_connect() {
            // DNS 失败通常表现为 connect 阶段错误；保守归 Connect（可重试）
            TransportError::new(err.to_string(), RetryableError::Connect)
        } else {
            // 其余（如 builder/解码错误）一般不可重试——归 ClientStatus(0) 占位
            TransportError::new(err.to_string(), RetryableError::ClientStatus(0))
        }
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new().expect("reqwest::Client 默认构造不应失败（无自定义根证书/代理）")
    }
}

#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn send(&self, base_url: &str, req: &RequestSpec) -> TransportResult {
        // 1) 拼 URL（RequestSpec::build_url 已做 base/path/query 编码）
        let url = req.build_url(base_url);
        // 2) 构造 reqwest RequestBuilder（按方法）
        let method = reqwest::Method::from_bytes(req.method.as_str().as_bytes()).map_err(|e| {
            TransportError::new(
                format!("非法 HTTP 方法: {e}"),
                RetryableError::ClientStatus(0),
            )
        })?;
        let mut rb = self.client.request(method, &url);
        // 3) header（按字典序，BTreeMap 天然有序；调用方设的 Content-Type 等透传）
        for (k, v) in &req.headers {
            rb = rb.header(k, v);
        }
        // 4) body（POST/PUT 用 body 字节；GET 不应有 body，传了 reqwest 会忽略）
        if let Some(body) = &req.body {
            rb = rb.body(body.clone());
        }
        // 5) 发送
        let resp = rb.send().await.map_err(Self::classify_reqwest_error)?;
        // 6) 状态码 + body 字节
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.map_err(|e| {
            TransportError::new(format!("读取响应体失败: {e}"), RetryableError::Connect)
        })?;
        // 7) 非 2xx → TransportError（带状态码分类，供 decide_retry）
        if !(200..300).contains(&status) {
            return Err(TransportError::new(
                format!("HTTP {status}: {}", String::from_utf8_lossy(&bytes)),
                RetryableError::from_status(status),
            ));
        }
        Ok(JsonResponse::new(status, bytes.to_vec()))
    }
}

// ----------------------------------------------------------------------------
// 单元测——reqwest 客户端构造（不发网络）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reqwest_transport_default_constructs() {
        // 仅验证 reqwest::Client 默认构造成功（rustls 后端初始化不报错）。
        let _t = ReqwestTransport::default();
    }

    #[test]
    fn reqwest_transport_with_client_injects() {
        let client = reqwest::Client::new();
        let t = ReqwestTransport::with_client(client);
        // 无从外部观测 client；仅验证构造不 panic。
        let _ = format!("{:?}", t.client); // reqwest::Client: Debug
    }

    // —— 扩展边界（覆盖率补测）——

    #[test]
    fn reqwest_transport_new_succeeds() {
        // new() 返回 Ok（rustls 后端可用）
        let t = ReqwestTransport::new();
        assert!(t.is_ok());
    }

    #[test]
    fn transport_error_new_and_fields() {
        let e = TransportError::new("connect refused", RetryableError::Connect);
        assert_eq!(e.message, "connect refused");
        assert_eq!(e.kind, RetryableError::Connect);
        // Debug/Clone 派生间接覆盖
        let _dbg = format!("{:?}", e);
        let _clone = e.clone();
    }

    #[test]
    fn transport_error_new_with_string() {
        // impl Into<String> 接受 &str 与 String
        let e1 = TransportError::new("literal", RetryableError::Timeout);
        let e2 = TransportError::new(String::from("owned"), RetryableError::Dns);
        assert_eq!(e1.message, "literal");
        assert_eq!(e2.message, "owned");
    }

    #[test]
    fn transport_error_kind_variants() {
        // 各 RetryableError 变体都能装进 TransportError
        for kind in [
            RetryableError::Connect,
            RetryableError::Timeout,
            RetryableError::Dns,
            RetryableError::RateLimited,
            RetryableError::ServerStatus(500),
            RetryableError::ClientStatus(404),
        ] {
            let e = TransportError::new("x", kind.clone());
            assert_eq!(e.kind, kind);
        }
    }

    #[test]
    fn reqwest_transport_with_custom_builder() {
        // 用 builder 注入自定义 client（如自定义超时）
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap();
        let _t = ReqwestTransport::with_client(client);
    }
}
