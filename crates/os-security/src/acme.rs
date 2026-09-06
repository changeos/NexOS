//! ACME 自动证书签续（instant-acme 真实实现）。
//!
//! 接通 RFC 8555 ACMEv2 客户端 [`instant_acme`]——为对外域名（如
//! `os.example.com`）走 Let's Encrypt 等公共 CA 自动签发与续期证书。
//!
//! # 架构
//!
//! - [`AcmeConfig`]：构造配置（directory URL + 联系邮箱 + challenge 完成策略）。
//!   注入到 [`crate::impls::CaCertManager`] 后，`acme_request` 走真实 ACME 流程；
//!   未注入则 `acme_request` 返回错误（内部 CA 自签作 fallback，调用方自行选
//!   `init_ca` + `sign`）。
//! - [`AcmeChallengeSolver`]：抽象 challenge 完成——HTTP-01 / DNS-01 由调用方
//!   注入（生产路径：HTTP-01 走内置 web server，DNS-01 走 DNS provider API）。
//!   测试用 [`AutoSolveSolver`]：fixture 服务器收到 `set_ready` 即置 Valid，
//!   故此 solver 仅记录调用历史，不真摆 challenge。
//!
//! # 流程（`CaCertManager::acme_request`）
//!
//! 1. 取 `AcmeConfig`；构造 `Account`。
//! 2. `new_order([Dns(domain)])` → `Order`（status=pending）。
//! 3. 遍历 `order.authorizations()`：选 challenge（HTTP-01 优先，回退 DNS-01）→
//!    solver 完成 → `set_ready`。
//! 4. `poll_ready` → `Ready`；`finalize`（rcgen 生成 CSR）→ `poll_certificate` → PEM 链。
//! 5. 解析 PEM → [`crate::cert::Certificate`] 元数据，入库（`auto_renew=true`）。
//!
//! # 测试
//!
//! 不真发 Let's Encrypt 请求（红线）。`FixtureAcmeServer` 实现
//! `instant_acme::HttpClient` trait，在内存中模拟一个最小 ACMEv2 服务器，
//! 用 rcgen 自签的 fixture CA 签发返回证书——instant-acme 的 JWS/nonce/重试
//! 逻辑真实跑，仅网络层替换为内存。详见 ADR-DEPS-004。

use crate::cert::Certificate;
use crate::SecurityError;
use os_core::DateTime;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// 选 challenge 类型并完成 challenge（solve + set_ready）。
///
/// 把 challenge 类型选择 + solver 调用 + set_ready 封装在单个函数内，`authz`
/// 仅在此函数内被借用一次（`authz.challenge(...)` 返回 `ChallengeHandle` 后，
/// 后续 `solve`/`set_ready` 都在该 handle 上，不再借 `authz`）——绕过 NLL 对
/// `Option<ChallengeHandle>` 跨语句保守借用判断。
async fn solve_challenge<'a>(
    authz: &'a mut instant_acme::AuthorizationHandle<'a>,
    config: &AcmeConfig,
) -> Result<(), SecurityError> {
    // 选 type：用配置的 preferred_challenge（生产路径调用方按部署选 HTTP-01 或
    // DNS-01；不再做「preferred 缺失回退另一类」——多 challenge 借用 authz 受 NLL
    // 限制，且现实部署通常只用一种）。若 preferred 不在 server 提供的 challenge 列表
    // 中，返回明确错误（调用方调整 config 重试）。
    let want_type = match config.preferred_challenge {
        AcmeChallengeKind::Http01 => instant_acme::ChallengeType::Http01,
        AcmeChallengeKind::Dns01 => instant_acme::ChallengeType::Dns01,
    };
    let mut challenge = authz.challenge(want_type.clone()).ok_or_else(|| {
        SecurityError::Internal(format!(
            "ACME: server 未提供 {want_type:?} challenge（调整 AcmeConfig.preferred_challenge）"
        ))
    })?;
    let key_auth = challenge.key_authorization();
    let want_kind = config.preferred_challenge;
    let domain_str = match challenge.identifier().identifier {
        instant_acme::Identifier::Dns(s) => s.clone(),
        instant_acme::Identifier::Ip(ip) => ip.to_string(),
        _ => "<unknown-identifier>".into(),
    };
    let cs = AcmeChallenge {
        domain: domain_str,
        kind: want_kind,
        key_authorization: key_auth.as_str().to_string(),
        dns_value: Some(key_auth.dns_value()),
    };
    // solver 完成 challenge 摆放。
    config.solver.solve(&cs).await?;
    // 通知 ACME 服务器 challenge ready。
    challenge.set_ready().await.map_err(acme_err)?;
    Ok(())
}

// ----------------------------------------------------------------------------
// 公共配置类型
// ----------------------------------------------------------------------------

/// ACME challenge 类型（与 `instant_acme::ChallengeType` 对应的简化枚举）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcmeChallengeKind {
    /// HTTP-01：在 `/.well-known/acme-challenge/<token>` 提供 key authorization。
    Http01,
    /// DNS-01：在 `_acme-challenge.<domain>` 提供 TXT 记录（key authorization 的
    /// base64url SHA-256 摘要）。
    Dns01,
}

/// ACME challenge 解析结果——交给 [`AcmeChallengeSolver`] 完成。
#[derive(Debug, Clone)]
pub struct AcmeChallenge {
    /// 域名（DNS identifier）。
    pub domain: String,
    /// challenge 类型。
    pub kind: AcmeChallengeKind,
    /// HTTP-01：摆在 `/.well-known/acme-challenge/<token>` 的 key authorization
    /// （`<token>.<account-thumbprint>`）。
    /// DNS-01：摆在 `_acme-challenge.<domain>` TXT 记录的值是 key authorization 的
    /// base64url SHA-256 摘要（见 [`AcmeChallenge::dns_value`]）。
    pub key_authorization: String,
    /// DNS-01 TXT 记录的值（base64url SHA-256 摘要 of key authorization）；
    /// HTTP-01 时为 None。
    pub dns_value: Option<String>,
}

/// ACME challenge 完成器（trait）——由调用方注入 HTTP-01/DNS-01 摆放策略。
///
/// `solve` 在 `challenge.set_ready()` **之前**调用：调用方在此完成 challenge
/// 资源摆放（启 HTTP server / 调 DNS API），返回后 `acme_request` 才通知
/// ACME 服务器 challenge ready。
///
/// 生产实现示例：
/// - HTTP-01：在 80 端口起临时 web server，路由 `/.well-known/acme-challenge/<token>`
///   返回 key authorization；`solve` 等待 server ready 后返回。
/// - DNS-01：调 DNS provider API 写 TXT 记录；`solve` 等待传播后返回。
pub trait AcmeChallengeSolver: Send + Sync {
    /// 完成 challenge（摆放 HTTP/DNS 资源）。失败返回 `Err`，`acme_request` 中止。
    fn solve(
        &self,
        challenge: &AcmeChallenge,
    ) -> Pin<Box<dyn Future<Output = Result<(), SecurityError>> + Send>>;
}

/// 自动完成 solver——测试专用。
///
/// 与 `FixtureAcmeServer` 配合：fixture 服务器在收到 `set_ready` 后直接将
/// challenge 置 Valid（不校验 key authorization 是否真摆放），故此 solver 仅
/// 记录调用历史（测试断言用），不真摆 challenge。
#[derive(Default)]
pub struct AutoSolveSolver {
    seen: std::sync::Mutex<Vec<AcmeChallenge>>,
}

impl AutoSolveSolver {
    /// 构造空 solver。
    pub fn new() -> Self {
        Self::default()
    }

    /// 取已观察到的 challenge 列表（测试断言用）。
    pub fn observed(&self) -> Vec<AcmeChallenge> {
        self.seen.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

impl AcmeChallengeSolver for AutoSolveSolver {
    fn solve(
        &self,
        challenge: &AcmeChallenge,
    ) -> Pin<Box<dyn Future<Output = Result<(), SecurityError>> + Send>> {
        let c = challenge.clone();
        // 先取锁、push、立刻释放（避免跨 await 持锁——Send 安全）。
        let res = self
            .seen
            .lock()
            .map(|mut guard| {
                guard.push(c);
            })
            .map_err(|e| SecurityError::Internal(format!("seen 锁中毒: {e}")));
        Box::pin(async move {
            res?;
            Ok(())
        })
    }
}

/// ACME 配置——注入 `CaCertManager` 后激活 `acme_request` 真实路径。
///
/// 生产路径用 [`AcmeConfig::lets_encrypt_staging`]（测试）或
/// [`AcmeConfig::lets_encrypt_production`]（生产）构造；测试路径用
/// [`AcmeConfig::with_directory`] 指向 `FixtureAcmeServer` 的内存 URL，
/// 并配 [`AcmeConfig::with_http`] 注入 fixture 的 `HttpClient`。
#[derive(Clone)]
pub struct AcmeConfig {
    /// ACME directory URL（如 `https://acme-staging-v02.api.letsencrypt.org/directory`）。
    pub directory_url: String,
    /// 联系邮箱列表（ACME account contact，形如 `mailto:admin@example.com`）。
    pub contacts: Vec<String>,
    /// challenge solver（HTTP-01/DNS-01 摆放）。
    pub solver: Arc<dyn AcmeChallengeSolver>,
    /// challenge 类型偏好（优先尝试；缺失时回退另一类）。
    pub preferred_challenge: AcmeChallengeKind,
    /// 自定义 HTTP 客户端（测试用：注入 `FixtureAcmeServer`；生产用 None 走默认）。
    http: Option<Arc<dyn instant_acme::HttpClient>>,
}

impl AcmeConfig {
    /// Let's Encrypt Staging（测试环境，签发的证书不受公共信任，但不触发速率限制）。
    pub fn lets_encrypt_staging(
        contacts: Vec<String>,
        solver: Arc<dyn AcmeChallengeSolver>,
        preferred: AcmeChallengeKind,
    ) -> Self {
        Self {
            directory_url: instant_acme::LetsEncrypt::Staging.url().to_string(),
            contacts,
            solver,
            preferred_challenge: preferred,
            http: None,
        }
    }

    /// Let's Encrypt Production（生产环境，签发的证书受公共信任）。
    pub fn lets_encrypt_production(
        contacts: Vec<String>,
        solver: Arc<dyn AcmeChallengeSolver>,
        preferred: AcmeChallengeKind,
    ) -> Self {
        Self {
            directory_url: instant_acme::LetsEncrypt::Production.url().to_string(),
            contacts,
            solver,
            preferred_challenge: preferred,
            http: None,
        }
    }

    /// 自定义 directory URL（测试用：指向 fixture 服务器）。
    pub fn with_directory(
        directory_url: impl Into<String>,
        contacts: Vec<String>,
        solver: Arc<dyn AcmeChallengeSolver>,
        preferred: AcmeChallengeKind,
    ) -> Self {
        Self {
            directory_url: directory_url.into(),
            contacts,
            solver,
            preferred_challenge: preferred,
            http: None,
        }
    }

    /// 注入自定义 HTTP 客户端（测试用：注入 fixture；调用后 `directory_url`
    /// 通常也指向 fixture 的内存 URL）。
    pub fn with_http(mut self, http: Arc<dyn instant_acme::HttpClient>) -> Self {
        self.http = Some(http);
        self
    }
}

// ----------------------------------------------------------------------------
// ACME 流程编排（核心：被 CaCertManager::acme_request 调用）
// ----------------------------------------------------------------------------

/// 执行完整 ACME 流程，返回 PEM 证书链 + 私钥 PEM。
///
/// 流程：account → order → challenge(s) → finalize → poll_certificate。
/// 是否真发 LE 请求取决于 `config.directory_url`——指向 fixture 即零网络。
pub(crate) async fn run_acme_order(
    config: &AcmeConfig,
    domain: &str,
) -> Result<
    (
        String, /* cert chain PEM */
        String, /* priv key PEM */
    ),
    SecurityError,
> {
    // 1. 构造 Account（每次新建——credentials 持久化留 TODO）。
    //    生产路径用 `Account::builder()`（hyper-rustls 默认 client）；
    //    测试路径用 `builder_with_http` 注入 fixture HttpClient。
    let contacts: Vec<&str> = config.contacts.iter().map(|s| s.as_str()).collect();
    let new_account = instant_acme::NewAccount {
        contact: &contacts,
        terms_of_service_agreed: true,
        only_return_existing: false,
    };
    let (account, _credentials) = match &config.http {
        Some(h) => {
            // 测试路径：注入自定义 HttpClient（fixture）。
            let http: Box<dyn instant_acme::HttpClient> = Box::new(ArcCloneHttp(h.clone()));
            instant_acme::Account::builder_with_http(http)
                .create(&new_account, config.directory_url.clone(), None)
                .await
                .map_err(acme_err)?
        }
        None => {
            // 生产路径：用默认 hyper-rustls client（需 `hyper-rustls` feature——
            // workspace 已开）。
            let builder = instant_acme::Account::builder().map_err(acme_err)?;
            builder
                .create(&new_account, config.directory_url.clone(), None)
                .await
                .map_err(acme_err)?
        }
    };

    // 2. 创建 order。
    let identifiers = vec![instant_acme::Identifier::Dns(domain.to_string())];
    let mut order = account
        .new_order(&instant_acme::NewOrder::new(identifiers.as_slice()))
        .await
        .map_err(acme_err)?;

    // 3. 遍历 authorizations，完成 challenge。
    let mut authorizations = order.authorizations();
    while let Some(result) = authorizations.next().await {
        let mut authz = result.map_err(acme_err)?;
        // authz derefs to AuthorizationState（含 status）。已 valid 的跳过。
        let already_valid = matches!(authz.status, instant_acme::AuthorizationStatus::Valid);
        if already_valid {
            continue;
        }
        // challenge 选择 + solve + set_ready 封装在 solve_challenge 内（单次借用 authz）。
        solve_challenge(&mut authz, config).await?;
    }
    // authorizations 借用随循环结束而释放（NLL）；显式 drop 被 clippy 标记为多余
    // （Authorizations 无 Drop impl）。

    // 4. poll_ready → finalize → poll_certificate。
    let status = order
        .poll_ready(&instant_acme::RetryPolicy::default())
        .await
        .map_err(acme_err)?;
    if status != instant_acme::OrderStatus::Ready {
        return Err(SecurityError::Internal(format!(
            "ACME: order 未就绪（status={status:?}）"
        )));
    }
    let private_key_pem = order.finalize().await.map_err(acme_err)?;
    let cert_chain_pem = order
        .poll_certificate(&instant_acme::RetryPolicy::default())
        .await
        .map_err(acme_err)?;

    Ok((cert_chain_pem, private_key_pem))
}

/// 把 `Arc<dyn HttpClient>` 适配为 `Box<dyn HttpClient>` 路径上的克隆句柄
/// （`Account::builder_with_http` 接 `Box<dyn HttpClient>`，但测试需共享同一
/// fixture 状态——用 Arc 包裹后克隆）。
struct ArcCloneHttp(Arc<dyn instant_acme::HttpClient>);

impl instant_acme::HttpClient for ArcCloneHttp {
    fn request(
        &self,
        req: http::Request<instant_acme::BodyWrapper<bytes::Bytes>>,
    ) -> Pin<
        Box<dyn Future<Output = Result<instant_acme::BytesResponse, instant_acme::Error>> + Send>,
    > {
        self.0.request(req)
    }
}

/// 把 instant-acme `Error` 映射到 `SecurityError::Internal`（不新增 variant，
/// 与 ADR-DEPS-004 §「trait 不改签名 / SecurityError 不新增 variant」一致）。
fn acme_err(e: instant_acme::Error) -> SecurityError {
    SecurityError::Internal(format!("ACME 错误: {e}"))
}

// ----------------------------------------------------------------------------
// PEM 解析（证书链 → Certificate 元数据）
// ----------------------------------------------------------------------------

/// 从 PEM 证书链（多张证书，第一张是叶子）解析出 `Certificate` 元数据。
///
/// 用 x509-parser 解析第一张（叶子）证书的 not_before/not_after/serial/issuer/subject。
/// `id` 用 serial hex；`common_name` 优先取 subject CN，回退首行 SAN DNS。
pub(crate) fn cert_meta_from_pem_chain(
    pem_chain: &str,
    auto_renew: bool,
) -> Result<Certificate, SecurityError> {
    let der = pem_first_cert_to_der(pem_chain)?;
    let (_rest, x509) = x509_parser::parse_x509_certificate(&der)
        .map_err(|e| SecurityError::Internal(format!("解析 ACME 证书 DER 失败: {e}")))?;
    let not_before = to_date_time(x509.validity().not_before.to_datetime());
    let not_after = to_date_time(x509.validity().not_after.to_datetime());
    let serial_hex = format!("{}", x509.serial);
    // CN 优先；缺失（LE 证书常无 CN，仅 SAN）时回退首 SAN DNS。
    let cn = x509
        .subject()
        .iter_common_name()
        .next()
        .and_then(|c| c.as_str().ok())
        .map(|s| s.to_string())
        .or_else(|| {
            x509.subject_alternative_name()
                .ok()
                .flatten()
                .and_then(|san| {
                    san.value.general_names.iter().find_map(|g| match g {
                        x509_parser::extensions::GeneralName::DNSName(s) => Some((*s).to_string()),
                        _ => None,
                    })
                })
        })
        .unwrap_or_else(|| "<acme-cert>".into());
    let issuer = x509
        .issuer()
        .iter_common_name()
        .next()
        .and_then(|c| c.as_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "<acme-issuer>".into());
    Ok(Certificate {
        id: serial_hex.clone(),
        common_name: cn,
        not_before,
        not_after,
        issuer,
        serial: serial_hex,
        auto_renew,
    })
}

/// time::OffsetDateTime → os-core DateTime（与 impls.rs CaCertManager::to_date_time 同实现）。
fn to_date_time(od: time::OffsetDateTime) -> DateTime {
    let ts = od.unix_timestamp();
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0).unwrap_or_else(chrono::Utc::now)
}

/// PEM 证书链 → 第一张证书的 DER（base64 解码 BEGIN/END 之间的内容）。
fn pem_first_cert_to_der(pem: &str) -> Result<Vec<u8>, SecurityError> {
    use base64::Engine;
    let mut in_cert = false;
    let mut b64 = String::new();
    for line in pem.lines() {
        if line.starts_with("-----BEGIN CERTIFICATE-----") {
            in_cert = true;
            continue;
        }
        if line.starts_with("-----END CERTIFICATE-----") {
            // 第一张证书到此结束。
            break;
        }
        if in_cert {
            b64.push_str(line.trim());
        }
    }
    if b64.is_empty() {
        return Err(SecurityError::Internal(
            "PEM 证书链中未找到 CERTIFICATE 块".into(),
        ));
    }
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| SecurityError::Internal(format!("PEM base64 解码失败: {e}")))
}

// ============================================================================
// FixtureAcmeServer —— in-memory ACME 服务器（测试专用，零网络）
// ============================================================================

#[cfg(test)]
pub mod fixture {
    use super::*;
    use bytes::Bytes;
    use http::header::CONTENT_TYPE;
    use http::{Method, Request, Response, StatusCode};
    use http_body_util::{BodyExt, Full};
    use instant_acme::{BodyWrapper, BytesResponse, HttpClient};
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// 构造 `Full<Bytes>` body（实现 `http_body::Body`，满足 `BytesResponse::from`
    /// 的 `B: Body` 约束——`Bytes` 自身不实现 `Body`）。
    fn fbody(bytes: Vec<u8>) -> Full<Bytes> {
        Full::new(Bytes::from(bytes))
    }

    /// 解码 JWS（JoseJson）请求的 payload 字段。
    ///
    /// ACME 请求 body 形如 `{"protected":"...","payload":"<base64url>","signature":"..."}`。
    /// fixture 需取 payload（如 NewOrder.identifiers）以确定域名。失败返回 Null Value。
    fn decode_jws_payload(body: &[u8]) -> serde_json::Value {
        use base64::Engine;
        let envelope: serde_json::Value =
            serde_json::from_slice(body).unwrap_or(serde_json::Value::Null);
        let payload_b64 = envelope
            .get("payload")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // empty payload（如 newAccount 的 ""）→ Null。
        if payload_b64.is_empty() {
            return serde_json::Value::Null;
        }
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload_b64)
            .unwrap_or_default();
        serde_json::from_slice(&decoded).unwrap_or(serde_json::Value::Null)
    }

    /// 内存 ACME 服务器（测试专用）。
    ///
    /// 模拟最小 RFC 8555 流程：directory → newNonce → newAccount → newOrder →
    /// authz → challenge → finalize → cert。所有响应带 `Replay-Nonce: fixed-nonce`。
    /// challenge 在收到 `set_ready` 后立即置 Valid（不校验摆放）；finalize 后
    /// order 置 Valid 并签发证书（用 rcgen 自签 fixture CA）。
    ///
    /// 状态可变（orders/certs/authz_ready），故用 `Arc<Inner>` 持有——`HttpClient`
    /// trait 的 `request` 返回 `'static` Future，须在 async 块内经 Arc 克隆捕获。
    pub struct FixtureAcmeServer {
        inner: Arc<FixtureInner>,
    }

    struct FixtureInner {
        /// fixture CA（rcgen 自签），签发返回的叶子证书。
        ca_params: rcgen::CertificateParams,
        ca_key: rcgen::KeyPair,
        /// order 状态：order_url → OrderRec。
        orders: Mutex<HashMap<String, OrderRec>>,
        /// 已签发证书：cert_url → PEM。
        certs: Mutex<HashMap<String, String>>,
        /// authz 是否已 set_ready：authz_url → bool。
        authz_ready: Mutex<HashMap<String, bool>>,
    }

    #[derive(Debug, Clone)]
    #[allow(dead_code)] // finalize_url 保留供未来 order 状态查询 API；当前路由按 URL 路径分派。
    struct OrderRec {
        domain: String,
        finalize_url: String,
        /// finalize 后置 Some(cert_url)。
        cert_url: Option<String>,
    }

    impl FixtureAcmeServer {
        /// 构造 fixture 服务器（生成一个自签 CA）。
        pub fn new() -> Result<Self, SecurityError> {
            let mut params = rcgen::CertificateParams::default();
            params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
            params
                .distinguished_name
                .push(rcgen::DnType::CommonName, "OS ACME Fixture CA");
            let key = rcgen::KeyPair::generate().map_err(|e| {
                SecurityError::Internal(format!("fixture CA KeyPair 生成失败: {e}"))
            })?;
            // 自签 CA 证书（保留 params + key 用于签发叶子；CA 证书实体本身不存——
            // signed_by 仅需 issuer params + key）。
            let _ca_cert = params
                .self_signed(&key)
                .map_err(|e| SecurityError::Internal(format!("fixture CA 自签失败: {e}")))?;
            Ok(Self {
                inner: Arc::new(FixtureInner {
                    ca_params: params,
                    ca_key: key,
                    orders: Mutex::new(HashMap::new()),
                    certs: Mutex::new(HashMap::new()),
                    authz_ready: Mutex::new(HashMap::new()),
                }),
            })
        }

        /// directory URL（任意字符串，fixture 按此 URL 路由）。
        pub fn directory_url(&self) -> &'static str {
            "mem://acme-fixture/directory"
        }

        /// 为给定域名签发一张叶子证书（fixture CA 签）。
        fn issue_cert(inner: &FixtureInner, domain: &str) -> Result<String, SecurityError> {
            let mut params = rcgen::CertificateParams::new(vec![domain.to_string()])
                .map_err(|e| SecurityError::Internal(format!("fixture 叶子参数构造失败: {e}")))?;
            params.distinguished_name = rcgen::DistinguishedName::new();
            let leaf_key = rcgen::KeyPair::generate().map_err(|e| {
                SecurityError::Internal(format!("fixture 叶子 KeyPair 生成失败: {e}"))
            })?;
            let issuer = rcgen::Issuer::from_params(&inner.ca_params, &inner.ca_key);
            let cert = params
                .signed_by(&leaf_key, &issuer)
                .map_err(|e| SecurityError::Internal(format!("fixture 叶子签发失败: {e}")))?;
            Ok(cert.pem())
        }

        /// 构造 200 JSON 响应（带 Replay-Nonce + 可选 Location）。
        fn json_ok(body: Vec<u8>, location: Option<&str>) -> BytesResponse {
            let mut builder = Response::builder()
                .status(StatusCode::OK)
                .header("Replay-Nonce", Self::fixed_nonce())
                .header(CONTENT_TYPE, "application/json");
            if let Some(loc) = location {
                builder = builder.header("Location", loc);
            }
            let rsp = builder.body(fbody(body)).expect("fixture 响应构造");
            BytesResponse::from(rsp)
        }

        fn fixed_nonce() -> &'static str {
            "fixed-nonce-AAAAAAAAAAA"
        }

        fn directory_json() -> Vec<u8> {
            let dir = serde_json::json!({
                "newNonce": "mem://acme-fixture/new-nonce",
                "newAccount": "mem://acme-fixture/new-account",
                "newOrder": "mem://acme-fixture/new-order",
                "revokeCert": "mem://acme-fixture/revoke-cert",
                "keyChange": "mem://acme-fixture/key-change",
            });
            serde_json::to_vec(&dir).expect("directory 序列化")
        }

        /// 同步路由：根据 (method, uri, body) 返回响应。
        fn route(inner: &FixtureInner, method: &Method, uri: &str, body: &[u8]) -> BytesResponse {
            let dir = Self::directory_json();
            match (method.clone(), uri) {
                (m, _) if uri == "mem://acme-fixture/directory" && m == Method::GET => {
                    Self::json_ok(dir, None)
                }
                (Method::HEAD, "mem://acme-fixture/new-nonce") => {
                    let rsp = Response::builder()
                        .status(StatusCode::OK)
                        .header("Replay-Nonce", Self::fixed_nonce())
                        .body(fbody(Vec::new()))
                        .unwrap();
                    BytesResponse::from(rsp)
                }
                (Method::POST, "mem://acme-fixture/new-account") => {
                    let acct = serde_json::json!({ "status": "valid", "contact": [] });
                    Self::json_ok(
                        serde_json::to_vec(&acct).unwrap(),
                        Some("mem://acme-fixture/account/1"),
                    )
                }
                (Method::POST, "mem://acme-fixture/new-order") => {
                    // body 是 JoseJson（{protected, payload, signature}），payload 是
                    // base64url 编码的 NewOrder JSON。解码 payload 取 identifiers[0]。
                    let payload = decode_jws_payload(body);
                    let domain = payload
                        .get("identifiers")
                        .and_then(|a| a.as_array())
                        .and_then(|a| a.first())
                        .and_then(|v| v.get("value"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("example.com")
                        .to_string();
                    let order_url = format!("mem://acme-fixture/order/{domain}");
                    let authz_url = format!("mem://acme-fixture/authz/{domain}");
                    let finalize_url = format!("mem://acme-fixture/finalize/{domain}");
                    {
                        let mut orders = inner.orders.lock().unwrap();
                        orders.insert(
                            order_url.clone(),
                            OrderRec {
                                domain: domain.clone(),
                                finalize_url: finalize_url.clone(),
                                cert_url: None,
                            },
                        );
                    }
                    let order = serde_json::json!({
                        "status": "pending",
                        "authorizations": [authz_url],
                        "finalize": finalize_url,
                        "certificate": serde_json::Value::Null,
                    });
                    Self::json_ok(serde_json::to_vec(&order).unwrap(), Some(&order_url))
                }
                (Method::POST, u) if u.starts_with("mem://acme-fixture/authz/") => {
                    let domain = u.trim_start_matches("mem://acme-fixture/authz/");
                    let ready = inner
                        .authz_ready
                        .lock()
                        .unwrap()
                        .get(u)
                        .copied()
                        .unwrap_or(false);
                    let challenge_status = if ready { "valid" } else { "pending" };
                    let authz_status = if ready { "valid" } else { "pending" };
                    let challenge_url = format!("mem://acme-fixture/challenge/{domain}");
                    let authz = serde_json::json!({
                        "identifier": {"type": "dns", "value": domain},
                        "status": authz_status,
                        "challenges": [{
                            "type": "http-01",
                            "url": challenge_url,
                            "token": "fixture-token-0123456789AB",
                            "status": challenge_status,
                        }],
                        "wildcard": false,
                    });
                    Self::json_ok(serde_json::to_vec(&authz).unwrap(), None)
                }
                (Method::POST, u) if u.starts_with("mem://acme-fixture/challenge/") => {
                    let domain = u.trim_start_matches("mem://acme-fixture/challenge/");
                    let authz_url = format!("mem://acme-fixture/authz/{domain}");
                    inner.authz_ready.lock().unwrap().insert(authz_url, true);
                    let challenge = serde_json::json!({
                        "type": "http-01",
                        "url": u,
                        "token": "fixture-token-0123456789AB",
                        "status": "processing",
                    });
                    Self::json_ok(serde_json::to_vec(&challenge).unwrap(), None)
                }
                (Method::POST, u) if u.starts_with("mem://acme-fixture/finalize/") => {
                    let domain = u.trim_start_matches("mem://acme-fixture/finalize/");
                    let order_url = format!("mem://acme-fixture/order/{domain}");
                    let cert_pem = match Self::issue_cert(inner, domain) {
                        Ok(p) => p,
                        Err(e) => {
                            let rsp = Response::builder()
                                .status(StatusCode::INTERNAL_SERVER_ERROR)
                                .body(fbody(e.to_string().into_bytes()))
                                .unwrap();
                            return BytesResponse::from(rsp);
                        }
                    };
                    let cert_url = format!("mem://acme-fixture/cert/{domain}");
                    {
                        let mut orders = inner.orders.lock().unwrap();
                        if let Some(o) = orders.get_mut(&order_url) {
                            o.cert_url = Some(cert_url.clone());
                        }
                        let mut certs = inner.certs.lock().unwrap();
                        certs.insert(cert_url.clone(), cert_pem);
                    }
                    let order = serde_json::json!({
                        "status": "valid",
                        "authorizations": [format!("mem://acme-fixture/authz/{domain}")],
                        "finalize": u,
                        "certificate": cert_url,
                    });
                    Self::json_ok(serde_json::to_vec(&order).unwrap(), Some(&order_url))
                }
                (Method::POST, u) if u.starts_with("mem://acme-fixture/order/") => {
                    let domain = u.trim_start_matches("mem://acme-fixture/order/");
                    let (status, cert_url) = {
                        let orders = inner.orders.lock().unwrap();
                        let o = match orders.get(u) {
                            Some(o) => o.clone(),
                            None => {
                                let rsp = Response::builder()
                                    .status(StatusCode::NOT_FOUND)
                                    .body(fbody(Vec::new()))
                                    .unwrap();
                                return BytesResponse::from(rsp);
                            }
                        };
                        let status = if o.cert_url.is_some() {
                            "valid"
                        } else {
                            let authz_url = format!("mem://acme-fixture/authz/{}", o.domain);
                            let ready = inner
                                .authz_ready
                                .lock()
                                .unwrap()
                                .get(&authz_url)
                                .copied()
                                .unwrap_or(false);
                            if ready {
                                "ready"
                            } else {
                                "pending"
                            }
                        };
                        (status, o.cert_url.clone())
                    };
                    let order = serde_json::json!({
                        "status": status,
                        "authorizations": [format!("mem://acme-fixture/authz/{domain}")],
                        "finalize": format!("mem://acme-fixture/finalize/{domain}"),
                        "certificate": cert_url,
                    });
                    Self::json_ok(serde_json::to_vec(&order).unwrap(), None)
                }
                (Method::POST, u) if u.starts_with("mem://acme-fixture/cert/") => {
                    let pem = match inner.certs.lock().unwrap().get(u).cloned() {
                        Some(p) => p,
                        None => {
                            let rsp = Response::builder()
                                .status(StatusCode::NOT_FOUND)
                                .body(fbody(Vec::new()))
                                .unwrap();
                            return BytesResponse::from(rsp);
                        }
                    };
                    let rsp = Response::builder()
                        .status(StatusCode::OK)
                        .header("Replay-Nonce", Self::fixed_nonce())
                        .header(CONTENT_TYPE, "application/pem-certificate-chain")
                        .body(fbody(pem.into_bytes()))
                        .unwrap();
                    BytesResponse::from(rsp)
                }
                _ => {
                    let rsp = Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .body(fbody(Vec::new()))
                        .unwrap();
                    BytesResponse::from(rsp)
                }
            }
        }
    }

    impl HttpClient for FixtureAcmeServer {
        fn request(
            &self,
            req: Request<BodyWrapper<Bytes>>,
        ) -> Pin<Box<dyn Future<Output = Result<BytesResponse, instant_acme::Error>> + Send>>
        {
            // Clone Arc<Inner> 进 async 块（Future 须 'static）。
            let inner = self.inner.clone();
            let method = req.method().clone();
            let uri = req.uri().to_string();
            Box::pin(async move {
                // 读 body：BodyWrapper<Bytes> impl http_body::Body；用 BodyExt::collect
                // 拼帧为完整字节（单帧即全部——BodyWrapper 单帧设计）。
                let body_bytes = req.into_body().collect().await.map_err(|e| {
                    instant_acme::Error::Other(Box::new(std::io::Error::other(format!(
                        "fixture 读 body 失败: {e}"
                    ))))
                })?;
                let body_bytes = body_bytes.to_bytes().to_vec();
                // 路由（同步——状态锁内）。
                Ok(Self::route(&inner, &method, &uri, &body_bytes))
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pem_first_cert_extracts_single() {
        let pem = "-----BEGIN CERTIFICATE-----\nSGVsbG8=\n-----END CERTIFICATE-----\n";
        let der = pem_first_cert_to_der(pem).expect("parse");
        assert_eq!(der, b"Hello");
    }

    #[test]
    fn pem_first_cert_missing_rejected() {
        assert!(pem_first_cert_to_der("no cert here").is_err());
    }

    #[test]
    fn acme_challenge_kind_eq() {
        assert_eq!(AcmeChallengeKind::Http01, AcmeChallengeKind::Http01);
        assert_ne!(AcmeChallengeKind::Http01, AcmeChallengeKind::Dns01);
    }

    #[tokio::test]
    async fn auto_solve_records_challenge() {
        let s = AutoSolveSolver::new();
        let c = AcmeChallenge {
            domain: "x.example".into(),
            kind: AcmeChallengeKind::Http01,
            key_authorization: "tok.thumb".into(),
            dns_value: Some("dns-val".into()),
        };
        s.solve(&c).await.unwrap();
        let seen = s.observed();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].domain, "x.example");
        assert_eq!(seen[0].dns_value.as_deref(), Some("dns-val"));
    }
}
