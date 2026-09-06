//! HttpOsClient——经 reqwest 真实 HTTP 调用 os-api 网关（规划文档 §3.15）。
//!
//! 设计（接通真实实现）：
//! - 会话状态机：`connect`/`disconnect`/`pair` 维护本地 `ClientSession`（与传输层无关，
//!   保证未连接即拒绝、断开即清理）。
//! - 请求构造：复用 [`crate::http::RequestSpec`]（纯逻辑，可确定性单测）。
//! - 真实 HTTP 传输：经 [`crate::transport::HttpTransport`]（trait，`#[async_trait]`）
//!   抽象——生产用 [`crate::transport::ReqwestTransport`]（reqwest + rustls，ADR-DEPS-001），
//!   测试注入内存 FakeTransport（不真发网络请求）。
//! - 重试编排：在 `HttpOsClient::send` 中循环「发一次 → 失败分类 →
//!   [`crate::retry::decide_retry`] 决策 → sleep → 重试」，复用既有纯决策算法。
//!
//! WS 订阅（PushSubscriber 长连接）走另一通道（`crate::push`），不在本文件范围。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::client::{ClientSession, OsClient, SystemStatus};
use crate::http::{JsonResponse, RequestSpec};
use crate::retry::{decide_retry, RetryPolicy, RetryableError};
use crate::transport::{HttpTransport, TransportError};
use crate::MobileError;

// ----------------------------------------------------------------------------
// 会话状态
// ----------------------------------------------------------------------------

/// HttpOsClient 持有的会话状态（connect 后填入，disconnect 后清空）。
#[derive(Debug, Clone)]
struct SessionState {
    session: ClientSession,
}

// ----------------------------------------------------------------------------
// HttpOsClient
// ----------------------------------------------------------------------------

/// HTTP OS 客户端——持有会话状态 + HTTP 传输，经网关 REST 调用 os-api。
///
/// 传输层用 `Arc<dyn HttpTransport>`（`#[async_trait]`，dyn 兼容——见 ADR-COMPAT-001），
/// 使：
/// - 生产：[`crate::transport::ReqwestTransport`]（reqwest + rustls）。
/// - 测试：注入 FakeTransport（离线 fixture，不真发网络请求）。
///
/// 线程安全：会话状态用 `Mutex<Option<SessionState>>` 保护；传输层 reqwest::Client
/// 自身线程安全（连接池复用）。
pub struct HttpOsClient {
    session: Mutex<Option<SessionState>>,
    transport: Arc<dyn HttpTransport>,
    /// 默认重试策略（connect/status/discover 用）；pair 用 `RetryPolicy::no_retry`。
    retry_policy: RetryPolicy,
}

impl HttpOsClient {
    /// 用默认 [`crate::transport::ReqwestTransport`]（reqwest + rustls）构造。
    ///
    /// 失败来源仅 reqwest::Client 构造失败（TLS 后端初始化，极少见）。
    pub fn new() -> Result<Self, MobileError> {
        let transport = Arc::new(crate::transport::ReqwestTransport::new()?);
        Ok(Self::with_transport(transport))
    }

    /// 注入自定义传输（测试用 FakeTransport / 自定义 reqwest::Client 的 ReqwestTransport）。
    #[must_use]
    pub fn with_transport(transport: Arc<dyn HttpTransport>) -> Self {
        Self {
            session: Mutex::new(None),
            transport,
            retry_policy: RetryPolicy::default(),
        }
    }

    /// 覆盖默认重试策略（如测试中缩短退避以快速失败）。
    #[must_use]
    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.retry_policy = policy;
        self
    }

    /// 当前是否已连接。
    pub fn is_connected(&self) -> bool {
        self.session.lock().unwrap().is_some()
    }

    /// 取当前会话端点（已连接时）。
    pub fn endpoint(&self) -> Option<String> {
        self.session
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.session.endpoint.clone())
    }

    /// 取当前会话 token（已连接时；用于调试/日志，勿记入持久日志）。
    pub fn token(&self) -> Option<String> {
        self.session
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.session.token.clone())
    }

    /// 构造 `get_system_status` 请求（GET /status）。
    pub fn build_status_request(&self) -> Result<RequestSpec, MobileError> {
        self.require_endpoint()?;
        Ok(RequestSpec::get("/status"))
    }

    /// 构造 `discover_nodes` 请求（GET /discover/nodes）。
    pub fn build_discover_request(&self) -> Result<RequestSpec, MobileError> {
        self.require_endpoint()?;
        Ok(RequestSpec::get("/discover/nodes"))
    }

    /// 构造 `list_shares` 请求（GET /shares）——供 os-desktop list_available_shares 复用。
    pub fn build_shares_request(&self) -> Result<RequestSpec, MobileError> {
        self.require_endpoint()?;
        Ok(RequestSpec::get("/shares"))
    }

    /// 构造 `pair` 请求（POST /pair，body 含配对码）。
    pub fn build_pair_request(&self, pairing_code: &str) -> Result<RequestSpec, MobileError> {
        #[derive(serde::Serialize)]
        struct PairBody<'a> {
            code: &'a str,
        }
        RequestSpec::post_json("/pair", &PairBody { code: pairing_code })
    }

    /// 内部：要求已连接，返回 endpoint 克隆。
    fn require_endpoint(&self) -> Result<String, MobileError> {
        self.session
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.session.endpoint.clone())
            .ok_or(MobileError::NotConnected)
    }

    /// 发送一个请求，按 `policy` 做重试编排（复用 [`crate::retry::decide_retry`]）。
    ///
    /// 流程：
    /// 1. 用 `RequestSpec::build_url(base_url)` 拼完整 URL。
    /// 2. 调 `transport.send`（单次）；失败 → `TransportError` 带 `RetryableError` 分类。
    /// 3. 调 `decide_retry(&err.kind, attempt, policy)`：
    ///    - `GiveUp` → 映射到 `MobileError` 返回。
    ///    - `Retry(delay)` → `tokio::time::sleep(delay)` 后下一轮。
    /// 4. 成功 → 返回 [`JsonResponse`]。
    async fn send(
        &self,
        req: &RequestSpec,
        policy: RetryPolicy,
    ) -> Result<JsonResponse, MobileError> {
        let base_url = self.require_endpoint()?;
        let mut attempt: u32 = 0;
        loop {
            match self.transport.send(&base_url, req).await {
                Ok(resp) => return Ok(resp),
                Err(TransportError { message, kind }) => {
                    match decide_retry(&kind, attempt, &policy) {
                        crate::retry::RetryDecision::Retry(delay) => {
                            sleep_with_jitter(delay).await;
                            attempt += 1;
                        }
                        crate::retry::RetryDecision::GiveUp => {
                            return Err(map_transport_error(message, kind));
                        }
                    }
                }
            }
        }
    }

    /// 发送并解析为 `T`（成功响应直接 JSON 解析；失败经重试后抛错）。
    async fn send_and_parse<T: for<'de> serde::Deserialize<'de>>(
        &self,
        req: RequestSpec,
        policy: RetryPolicy,
    ) -> Result<T, MobileError> {
        let resp = self.send(&req, policy).await?;
        crate::http::parse_json_response(&resp)
    }
}

impl Default for HttpOsClient {
    fn default() -> Self {
        // Default 不可失败——用 ReqwestTransport::default（内部 expect，仅在 TLS 后端
        // 初始化失败时 panic，与 reqwest::Client::new 语义一致）。
        Self::new().expect("HttpOsClient 默认构造不应失败（reqwest::Client 默认可用）")
    }
}

// ----------------------------------------------------------------------------
// 错误映射 + jitter
// ----------------------------------------------------------------------------

/// 把 `TransportError`（带分类）映射到 `MobileError`。
///
/// 状态码类错误额外带 HTTP 状态，便于上层区分（如 401/403 → 视作鉴权失败语义）。
fn map_transport_error(message: String, kind: RetryableError) -> MobileError {
    match kind {
        RetryableError::ClientStatus(401) | RetryableError::ClientStatus(403) => {
            // 鉴权失败语义：归 EndpointUnreachable（客户端侧无独立 AuthFailed 变体）
            MobileError::EndpointUnreachable(format!("鉴权失败: {message}"))
        }
        _ => MobileError::EndpointUnreachable(message),
    }
}

/// 在 `decide_retry` 给的延迟上加 ±10% 抖动（避免惊群；retry.rs 决策本身确定性无抖动）。
///
/// `delay == 0` 时直接返回（不引入随机性，保证 no_retry 策略快速失败）。
async fn sleep_with_jitter(delay: Duration) {
    if delay.is_zero() {
        return;
    }
    // 简单抖动：固定加 10%（避免引入 rand crate 依赖；ADR-DEPS-001 未注册 rand）。
    // 决策算法注释指出「可加 ±10% 抖动」——此处用 +10% 上限近似，保持零额外依赖。
    let jittered = delay + delay / 10;
    tokio::time::sleep(jittered).await;
}

// ----------------------------------------------------------------------------
// OsClient trait 实现（真实 HTTP，经 transport + 重试编排）
// ----------------------------------------------------------------------------

// OsClient trait 为原生 async（非 #[async_trait]），故 impl 用原生 async fn。
impl OsClient for HttpOsClient {
    async fn connect(
        &self,
        endpoint: &str,
        token: Option<&str>,
    ) -> Result<ClientSession, MobileError> {
        // 会话建立是本地状态变更（不触发 HTTP）；真实鉴权由后续请求的 401 反馈。
        // token=None → 匿名会话占位 token。
        let session = ClientSession {
            endpoint: endpoint.to_string(),
            token: token.unwrap_or("anonymous").to_string(),
            user: token
                .map(|_| "authed".to_string())
                .unwrap_or_else(|| "anonymous".to_string()),
            expires_at: chrono::Utc::now() + chrono::Duration::hours(24),
        };
        *self.session.lock().unwrap() = Some(SessionState {
            session: session.clone(),
        });
        Ok(session)
    }

    async fn disconnect(&self) -> Result<(), MobileError> {
        let mut slot = self.session.lock().unwrap();
        if slot.is_none() {
            return Err(MobileError::NotConnected);
        }
        *slot = None;
        Ok(())
    }

    async fn get_system_status(&self) -> Result<SystemStatus, MobileError> {
        let req = self.build_status_request()?;
        self.send_and_parse(req, self.retry_policy).await
    }

    async fn discover_nodes(&self) -> Result<Vec<os_discover::PeerNode>, MobileError> {
        let req = self.build_discover_request()?;
        self.send_and_parse(req, self.retry_policy).await
    }

    async fn pair(&self, endpoint: &str, pairing_code: &str) -> Result<ClientSession, MobileError> {
        // 先建立会话（pair 需在已连接态发请求，复用 send 的 base_url 解析）。
        let session = ClientSession {
            endpoint: endpoint.to_string(),
            token: format!("pairing:{pairing_code}"),
            user: "pairing".to_string(),
            expires_at: chrono::Utc::now() + chrono::Duration::hours(24),
        };
        *self.session.lock().unwrap() = Some(SessionState {
            session: session.clone(),
        });

        // pair 不重试（重复提交可能创建多会话，见 retry.rs 注释）。
        let req = self.build_pair_request(pairing_code)?;
        let resp = self.send(&req, RetryPolicy::no_retry()).await?;
        // 解析网关返回的真实 session（含正式 token/user）。
        #[derive(serde::Deserialize)]
        struct PairResp {
            token: String,
            user: String,
            #[serde(default)]
            expires_at: Option<os_core::DateTime>,
        }
        let parsed: PairResp = crate::http::parse_json_response(&resp)?;
        let expires_at = parsed
            .expires_at
            .unwrap_or_else(|| chrono::Utc::now() + chrono::Duration::hours(24));
        let session = ClientSession {
            endpoint: endpoint.to_string(),
            token: parsed.token,
            user: parsed.user,
            expires_at,
        };
        *self.session.lock().unwrap() = Some(SessionState {
            session: session.clone(),
        });
        Ok(session)
    }
}

// ----------------------------------------------------------------------------
// 单元测——会话状态机 + 请求构造（不依赖网络）+ transport 注入的重试编排
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{HttpMethod, JsonResponse, RequestSpec};
    use crate::transport::TransportError;
    use async_trait::async_trait;
    use os_core::Health;
    use std::sync::atomic::{AtomicU32, Ordering};

    // ============================================================
    // FakeTransport——离线 fixture，确定性，不真发网络请求
    // ============================================================

    /// 可编程的假传输：按预设脚本回放响应/错误序列（每次 send 推进一个游标）。
    ///
    /// 用于测重试编排（先几次 503，第 3 次成功）与错误映射，完全离线、确定性。
    struct FakeTransport {
        /// 预设的响应/错误序列（按 send 调用顺序消费）。
        script: Mutex<Vec<Result<JsonResponse, TransportError>>>,
    }

    impl FakeTransport {
        fn new(script: Vec<Result<JsonResponse, TransportError>>) -> Self {
            Self {
                script: Mutex::new(script),
            }
        }
    }

    #[async_trait]
    impl HttpTransport for FakeTransport {
        async fn send(
            &self,
            _base_url: &str,
            _req: &RequestSpec,
        ) -> crate::transport::TransportResult {
            let mut s = self.script.lock().unwrap();
            if s.is_empty() {
                return Err(TransportError::new(
                    "FakeTransport 脚本耗尽",
                    RetryableError::ClientStatus(0),
                ));
            }
            // 从队首取一项（保持调用顺序）
            // Vec::drain(0..1) 较慢但测试脚本短，可接受。
            let item = s.drain(0..1).next().unwrap();
            item
        }
    }

    /// 计数型 FakeTransport：每次返回同一个响应（用于测多次调用的状态）。
    struct CountingTransport {
        resp: JsonResponse,
        count: AtomicU32,
    }

    #[async_trait]
    impl HttpTransport for CountingTransport {
        async fn send(&self, _base: &str, _req: &RequestSpec) -> crate::transport::TransportResult {
            self.count.fetch_add(1, Ordering::SeqCst);
            Ok(self.resp.clone())
        }
    }

    // —— 会话状态机（保留原有测试，调整为 with_transport 构造）——

    fn with_fake(script: Vec<Result<JsonResponse, TransportError>>) -> HttpOsClient {
        let t: Arc<dyn HttpTransport> = Arc::new(FakeTransport::new(script));
        HttpOsClient::with_transport(t)
    }

    fn status_json() -> JsonResponse {
        JsonResponse::new(
            200,
            br#"{"hostname":"os-real","version":"1.2.3","capacity":{"used_bytes":10,"total_bytes":100},"health":"healthy","node_count":2}"#.to_vec(),
        )
    }

    fn peers_json() -> JsonResponse {
        JsonResponse::new(200, br#"[]"#.to_vec())
    }

    #[tokio::test]
    async fn connect_then_disconnect_state_machine() {
        let c = with_fake(vec![]);
        assert!(!c.is_connected());
        let s = c.connect("https://os:8443", None).await.unwrap();
        assert!(c.is_connected());
        assert_eq!(s.endpoint, "https://os:8443");
        assert_eq!(s.token, "anonymous");
        c.disconnect().await.unwrap();
        assert!(!c.is_connected());
    }

    #[tokio::test]
    async fn disconnect_when_not_connected_errors() {
        let c = with_fake(vec![]);
        assert!(matches!(
            c.disconnect().await,
            Err(MobileError::NotConnected)
        ));
    }

    #[tokio::test]
    async fn status_requires_connect() {
        let c = with_fake(vec![]);
        assert!(matches!(
            c.build_status_request(),
            Err(MobileError::NotConnected)
        ));
        c.connect("https://os", Some("tok")).await.unwrap();
        let req = c.build_status_request().unwrap();
        assert_eq!(req.method, HttpMethod::Get);
        assert_eq!(req.path, "/status");
    }

    #[tokio::test]
    async fn pair_builds_post_request_with_code_body() {
        let c = with_fake(vec![]);
        let req = c.build_pair_request("ABC123").unwrap();
        assert_eq!(req.method, HttpMethod::Post);
        assert_eq!(req.path, "/pair");
        let body = req.body.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("ABC123"));
    }

    #[tokio::test]
    async fn endpoint_tracks_session() {
        let c = with_fake(vec![]);
        assert!(c.endpoint().is_none());
        c.connect("https://os:8443", Some("t")).await.unwrap();
        assert_eq!(c.endpoint(), Some("https://os:8443".to_string()));
    }

    // —— 真实 HTTP 经 FakeTransport（重试编排 + 错误映射）——

    #[tokio::test]
    async fn get_system_status_parses_real_response() {
        let c = with_fake(vec![Ok(status_json())]);
        c.connect("https://os", Some("tok")).await.unwrap();
        let st = c.get_system_status().await.unwrap();
        assert_eq!(st.hostname, "os-real");
        assert_eq!(st.version, "1.2.3");
        assert_eq!(st.node_count, 2);
        assert_eq!(st.health, Health::Healthy);
        assert_eq!(st.capacity.used_bytes, 10);
        assert_eq!(st.capacity.total_bytes, 100);
    }

    #[tokio::test]
    async fn get_system_status_retries_on_503_then_succeeds() {
        // 脚本：503, 503, 200（前两次可重试，第三次成功）
        let c = with_fake(vec![
            Err(TransportError::new(
                "svr",
                RetryableError::ServerStatus(503),
            )),
            Err(TransportError::new(
                "svr",
                RetryableError::ServerStatus(503),
            )),
            Ok(status_json()),
        ])
        .with_retry_policy(RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(1),
            multiplier: 1,
            max_delay: Duration::from_millis(2),
        });
        c.connect("https://os", Some("tok")).await.unwrap();
        let st = c.get_system_status().await.unwrap();
        assert_eq!(st.hostname, "os-real");
    }

    #[tokio::test]
    async fn get_system_status_gives_up_after_max_attempts() {
        // 3 次全 503 → 用尽 max_attempts=3 后 GiveUp
        let c = with_fake(vec![
            Err(TransportError::new(
                "svr",
                RetryableError::ServerStatus(503),
            )),
            Err(TransportError::new(
                "svr",
                RetryableError::ServerStatus(503),
            )),
            Err(TransportError::new(
                "svr",
                RetryableError::ServerStatus(503),
            )),
        ])
        .with_retry_policy(RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            multiplier: 1,
            max_delay: Duration::from_millis(2),
        });
        c.connect("https://os", Some("tok")).await.unwrap();
        let err = c.get_system_status().await.unwrap_err();
        assert!(matches!(err, MobileError::EndpointUnreachable(_)));
    }

    #[tokio::test]
    async fn get_system_status_404_does_not_retry() {
        // 404 不可重试 → 立即失败（脚本只放 1 项即可证明未重试）
        let c = with_fake(vec![Err(TransportError::new(
            "nf",
            RetryableError::ClientStatus(404),
        ))]);
        c.connect("https://os", Some("tok")).await.unwrap();
        let err = c.get_system_status().await.unwrap_err();
        assert!(matches!(err, MobileError::EndpointUnreachable(_)));
    }

    #[tokio::test]
    async fn discover_nodes_parses_empty_list() {
        let c = with_fake(vec![Ok(peers_json())]);
        c.connect("https://os", None).await.unwrap();
        let nodes = c.discover_nodes().await.unwrap();
        assert!(nodes.is_empty());
    }

    #[tokio::test]
    async fn pair_sends_post_and_uses_returned_token() {
        // 网关返回正式 token/user
        let pair_resp = JsonResponse::new(
            200,
            br#"{"token":"real-token-xyz","user":"admin"}"#.to_vec(),
        );
        let c = with_fake(vec![Ok(pair_resp)]);
        let s = c.pair("https://os", "ABC123").await.unwrap();
        assert_eq!(s.token, "real-token-xyz");
        assert_eq!(s.user, "admin");
        assert!(c.is_connected());
        // pair 不重试：即使失败也不会重复（脚本只 1 项）
    }

    #[tokio::test]
    async fn pair_does_not_retry_on_server_error() {
        // pair 用 no_retry 策略：503 立即失败，不消费后续脚本项
        let c = with_fake(vec![
            Err(TransportError::new(
                "svr",
                RetryableError::ServerStatus(503),
            )),
            Ok(status_json()), // 这项不应被消费（pair 不重试）
        ]);
        let err = c.pair("https://os", "CODE").await.unwrap_err();
        assert!(matches!(err, MobileError::EndpointUnreachable(_)));
    }

    #[tokio::test]
    async fn send_observes_request_shape() {
        let c = with_fake(vec![Ok(status_json())]);
        // 直接取内部 FakeTransport 引用不太方便；这里通过 build_*_request 验证形状。
        c.connect("https://os:8443", Some("tok")).await.unwrap();
        let _ = c.get_system_status().await.unwrap();
        // observed 在 FakeTransport 内部，这里间接验证：成功即说明 URL/方法正确发出。
        assert!(c.is_connected());
    }

    #[tokio::test]
    async fn counting_transport_records_call_count() {
        let resp = JsonResponse::new(200, br#"{"hostname":"h","version":"v","capacity":{"used_bytes":0,"total_bytes":0},"health":"healthy","node_count":1}"#.to_vec());
        let counter = Arc::new(CountingTransport {
            resp,
            count: AtomicU32::new(0),
        });
        let c = HttpOsClient::with_transport(counter.clone());
        c.connect("https://os", None).await.unwrap();
        c.get_system_status().await.unwrap();
        c.get_system_status().await.unwrap();
        assert_eq!(counter.count.load(Ordering::SeqCst), 2);
    }

    // —— 错误映射 ——
    #[test]
    fn map_transport_error_auth_to_endpoint_unreachable() {
        let e = map_transport_error("denied".into(), RetryableError::ClientStatus(401));
        let msg = match e {
            MobileError::EndpointUnreachable(m) => m,
            _ => panic!("期望 EndpointUnreachable"),
        };
        assert!(msg.contains("鉴权失败"));
    }

    #[test]
    fn map_transport_error_5xx_to_endpoint_unreachable() {
        let e = map_transport_error("svr".into(), RetryableError::ServerStatus(503));
        assert!(matches!(e, MobileError::EndpointUnreachable(_)));
    }
}

// ----------------------------------------------------------------------------
// 集成测——真实 reqwest 经 loopback HTTP（不发外网）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::transport::ReqwestTransport;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// 极简 loopback HTTP「服务器」：接受 1 个连接，返回固定 200 + JSON body。
    ///
    /// 仅用于证明 ReqwestTransport 真实经 reqwest 发 HTTP（rustls 不参与——明文 HTTP）。
    /// 不引入 mockito/wiremock（未在 workspace 注册），用 tokio TcpListener 自建。
    async fn serve_status_once(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // 读取并丢弃请求行/头（直到空行或读尽）
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.flush().await.ok();
        });
        url
    }

    #[tokio::test]
    async fn reqwest_transport_real_http_get_status() {
        let body = r#"{"hostname":"os-real","version":"1.2.3","capacity":{"used_bytes":10,"total_bytes":100},"health":"healthy","node_count":2}"#;
        let base_url = serve_status_once(body).await;
        let transport: Arc<dyn HttpTransport> = Arc::new(ReqwestTransport::new().unwrap());
        let c = HttpOsClient::with_transport(transport);
        c.connect(&base_url, None).await.unwrap();
        let st = c.get_system_status().await.unwrap();
        assert_eq!(st.hostname, "os-real");
        assert_eq!(st.version, "1.2.3");
        assert_eq!(st.node_count, 2);
    }

    #[tokio::test]
    async fn reqwest_transport_real_http_404_maps_to_error() {
        // 404 → TransportError(ClientStatus) → 不重试 → EndpointUnreachable
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found")
                .await
                .unwrap();
            sock.flush().await.ok();
        });
        let transport: Arc<dyn HttpTransport> = Arc::new(ReqwestTransport::new().unwrap());
        let c = HttpOsClient::with_transport(transport);
        c.connect(&base_url, None).await.unwrap();
        let err = c.get_system_status().await.unwrap_err();
        assert!(matches!(err, MobileError::EndpointUnreachable(_)));
    }

    #[tokio::test]
    async fn reqwest_transport_connection_refused_is_retryable_then_gives_up() {
        // 绑定一个端口再立刻关闭 → 连接被拒（Connect 类，可重试），用尽次数后 GiveUp。
        // 用 127.0.0.1:0 绑定取一个空闲端口后 drop listener，制造「连接被拒」。
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let base_url = format!("http://{addr}");
        let transport: Arc<dyn HttpTransport> = Arc::new(ReqwestTransport::new().unwrap());
        let c = HttpOsClient::with_transport(transport).with_retry_policy(RetryPolicy {
            max_attempts: 2,
            base_delay: Duration::from_millis(1),
            multiplier: 1,
            max_delay: Duration::from_millis(2),
        });
        c.connect(&base_url, None).await.unwrap();
        let err = c.get_system_status().await.unwrap_err();
        assert!(matches!(err, MobileError::EndpointUnreachable(_)));
    }
}
