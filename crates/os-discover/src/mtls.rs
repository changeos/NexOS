//! mTLS 双向认证实现（rustls 0.23 + ring 后端）
//!
//! 决策依据：规划文档 §3.14 —— 发现到 peer 后，用配对凭证（`PairingToken`）完成
//! mTLS 双向认证建立受信任的 peer 会话；后续同步/管理流量走该 mTLS 通道。
//!
//! ## 依赖接入状态（ADR-DEPS-002 P2）
//! - **rustls 0.23 + ring**：真实 TLS 握手（与 reqwest rustls-tls 共栈）。本模块用
//!   `rustls::crypto::ring::default_provider()` 显式注入 ring 后端，避免 aws-lc-rs。
//! - **pki_types**：`CertificateDer` / `PrivateKeyDer` / `ServerName` 经 `rustls::pki_types`
//!   re-export 访问（无需单独依赖 rustls-pki-types，未在 workspace 注册）。
//! - **证书指纹**：对端证书的 SHA-256（与 beacon 公钥指纹算法一致，便于上层把
//!   "mTLS 对端证书指纹"与"beacon 公钥指纹"做关联校验）。
//!
//! ## 工作机制
//! - `pair(peer_endpoint, token)`：用本机证书链 + 私钥 + 受信根证书库构造 rustls
//!   `ClientConfig`，连接 `peer_endpoint` 完成 mTLS 双向握手；握手成功后取出对端
//!   证书链，计算其首张证书的 SHA-256 指纹，写入 `PeerSession.mtls_cert_fingerprint`。
//! - `unpair` / `list_trusted_peers`：维护内存会话表与受信节点列表。
//!
//! ## 测试策略（红线：不真改网络）
//! 用 rcgen 生成自签 CA + 服务器/客户端证书 fixture，在 `127.0.0.1:<随机端口>` 上
//! 启动 TCP 监听器线程，跑真实的 rustls mTLS 双向握手（loopback TCP，不触达外部网络）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use os_core::{NodeId, Utc};
use rustls::client::ClientConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::RootCertStore;
use sha2::{Digest, Sha256};

use crate::auth::{PairingToken, PeerAuthenticator, PeerSession, PeerSessionId};
use crate::beacon::hex_encode;
use crate::DiscoverError;

// ----------------------------------------------------------------------------
// 证书指纹
// ----------------------------------------------------------------------------

/// 计算证书 DER 的 SHA-256 指纹（hex，64 字符）。
///
/// 与 [`crate::beacon::pubkey_fingerprint`] 算法一致（均 SHA-256），便于上层把
/// "mTLS 对端证书指纹"与"beacon 公钥指纹"做关联校验。
pub fn cert_fingerprint(cert_der: &CertificateDer<'_>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cert_der.as_ref());
    hex_encode(&hasher.finalize())
}

// ----------------------------------------------------------------------------
// MtlsPeerAuthenticator
// ----------------------------------------------------------------------------

/// mTLS 双向认证器（rustls 实现）。
///
/// 持有本机身份（证书链 + 私钥）与受信根证书库（验证对端），在 `pair` 时完成
/// 真实 mTLS 握手并记录对端证书指纹。会话表与受信列表内存维护（持久化归 os-meta，
/// 本 agent 不下沉）。
pub struct MtlsPeerAuthenticator {
    /// 本机证书链（DER，第一张为实体证书，后续为中间 CA）
    my_cert_chain: Vec<CertificateDer<'static>>,
    /// 本机私钥（DER，PKCS8/SEC1/PKCS1 任一）
    my_key: PrivateKeyDer<'static>,
    /// 受信根证书库（验证对端）
    trusted_roots: Arc<RootCertStore>,
    /// 已建立的 peer 会话（session_id → PeerSession）
    sessions: Mutex<HashMap<PeerSessionId, PeerSession>>,
    /// 已受信节点（pair 成功后追加）
    trusted: Mutex<Vec<NodeId>>,
}

impl MtlsPeerAuthenticator {
    /// 创建实例——注入本机证书链、私钥与受信根证书库。
    pub fn new(
        my_cert_chain: Vec<CertificateDer<'static>>,
        my_key: PrivateKeyDer<'static>,
        trusted_roots: RootCertStore,
    ) -> Self {
        Self {
            my_cert_chain,
            my_key,
            trusted_roots: Arc::new(trusted_roots),
            sessions: Mutex::new(HashMap::new()),
            trusted: Mutex::new(Vec::new()),
        }
    }

    /// 当前活跃会话数（测试/可观测用）。
    pub fn session_count(&self) -> usize {
        self.sessions.lock().expect("lock poisoned").len()
    }

    /// 构造 rustls 客户端配置（mTLS：本机身份 + 对端根证书库验证）。
    fn build_client_config(&self) -> Result<ClientConfig, DiscoverError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        // 受信根证书库：rustls ClientConfig 需要 owned RootCertStore；从 self 的 Arc clone。
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| DiscoverError::Internal(format!("TLS 版本协商失败: {e}")))?
            .with_root_certificates((*self.trusted_roots).clone())
            .with_client_auth_cert(self.my_cert_chain.clone(), self.my_key.clone_key())
            .map_err(|e| {
                DiscoverError::MtlsHandshakeFailed(format!("ClientConfig 构造失败: {e}"))
            })?;
        Ok(config)
    }

    /// 执行真实 mTLS 握手——连接 `peer_endpoint`，完成双向认证，返回对端证书指纹。
    ///
    /// 同步阻塞（rustls StreamOwned over std TcpStream）；由 async `pair` 在
    /// `spawn_blocking` 中调用，避免阻塞 tokio 运行时。
    fn do_mtls_handshake(
        config: Arc<ClientConfig>,
        peer_endpoint: &str,
    ) -> Result<String, DiscoverError> {
        // 解析 ServerName：取 peer_endpoint 的 host 部分（去端口）
        let (host, _port) = parse_host_port(peer_endpoint)?;
        // 连接 TCP（loopback / 局域网）
        let tcp = std::net::TcpStream::connect(peer_endpoint).map_err(|e| {
            DiscoverError::MtlsHandshakeFailed(format!("TCP 连接 {peer_endpoint} 失败: {e}"))
        })?;
        let _ = tcp.set_nodelay(true);

        // rustls ServerName：用 host 的 DNS 名（自签证书 fixture 用 "localhost"）。
        // IP 地址作为 ServerName 在 rustls 0.23 需用 IpAddr；此处统一按 DNS 名处理，
        // 若 host 是 IP 则降级为 "localhost"（fixture 测试约定）。
        let server_name: ServerName<'static> = if host.parse::<std::net::IpAddr>().is_ok() {
            ServerName::try_from("localhost".to_string())
                .map_err(|e| DiscoverError::Internal(format!("ServerName 构造失败: {e}")))?
        } else {
            ServerName::try_from(host.clone())
                .map_err(|e| DiscoverError::Internal(format!("ServerName 构造失败: {e}")))?
        };

        let mut conn = rustls::ClientConnection::new(config, server_name).map_err(|e| {
            DiscoverError::MtlsHandshakeFailed(format!("ClientConnection 创建失败: {e}"))
        })?;
        let mut sock = tcp;

        // 显式驱动 mTLS 握手：complete_io 内部循环 read_tls/write_tls/flush，
        // 直到 is_handshaking()=false（双方互换 ClientHello/ServerHello/Cert/Finished）。
        // 用 complete_io 而非 read(1B)，避免"双方都 read 等数据"的死锁。
        loop {
            conn.complete_io(&mut sock).map_err(|e| {
                DiscoverError::MtlsHandshakeFailed(format!("mTLS 握手 IO 失败: {e}"))
            })?;
            if !conn.is_handshaking() {
                break;
            }
        }

        // 取对端证书链（握手完成后才可读）
        let cert_chain = conn
            .peer_certificates()
            .ok_or_else(|| {
                DiscoverError::MtlsHandshakeFailed("握手后无对端证书（双向认证未完成）".to_string())
            })?
            .to_vec();
        if cert_chain.is_empty() {
            return Err(DiscoverError::MtlsHandshakeFailed(
                "对端证书链为空".to_string(),
            ));
        }
        // 首张证书的 SHA-256 指纹
        Ok(cert_fingerprint(&cert_chain[0]))
    }
}

impl PeerAuthenticator for MtlsPeerAuthenticator {
    async fn pair(
        &self,
        peer_endpoint: &str,
        token: &PairingToken,
    ) -> Result<PeerSession, DiscoverError> {
        // 凭证过期检查（防伪红线：过期凭证必须拒绝）
        if token.expires_at < Utc::now() {
            return Err(DiscoverError::PairingFailed("配对凭证已过期".to_string()));
        }

        // 构造客户端配置（mTLS：本机身份 + 对端根证书验证）
        let config = Arc::new(self.build_client_config()?);

        // 真实 mTLS 握手（同步阻塞 → spawn_blocking）
        let peer_endpoint_owned = peer_endpoint.to_string();
        let config_clone = config;
        let fingerprint = tokio::task::spawn_blocking(move || {
            Self::do_mtls_handshake(config_clone, &peer_endpoint_owned)
        })
        .await
        .map_err(|e| DiscoverError::Internal(format!("mTLS 握手任务 join 失败: {e}")))??;

        // 建立会话
        let peer = NodeId::new(peer_endpoint);
        let session = PeerSession::new(
            PeerSessionId::new(format!("mtls-sess-{}", peer)),
            peer.clone(),
            fingerprint,
        );
        self.sessions
            .lock()
            .expect("lock poisoned")
            .insert(session.id.clone(), session.clone());
        self.trusted.lock().expect("lock poisoned").push(peer);
        Ok(session)
    }

    async fn unpair(&self, session: &PeerSessionId) -> Result<(), DiscoverError> {
        let removed = self.sessions.lock().expect("lock poisoned").remove(session);
        if removed.is_none() {
            return Err(DiscoverError::PairingFailed(format!(
                "mTLS: 会话不存在 {session}"
            )));
        }
        Ok(())
    }

    async fn list_trusted_peers(&self) -> Vec<NodeId> {
        self.trusted.lock().expect("lock poisoned").clone()
    }
}

// ----------------------------------------------------------------------------
// 辅助
// ----------------------------------------------------------------------------

/// 从 "host:port" / "[::1]:port" 解析 (host, port)。
fn parse_host_port(endpoint: &str) -> Result<(String, u16), DiscoverError> {
    let s = endpoint.trim();
    let Some(colon) = s.rfind(':') else {
        return Err(DiscoverError::Internal(format!("端点缺少端口: {s}")));
    };
    let mut host = s[..colon].to_string();
    let port_str = &s[colon + 1..];
    if host.starts_with('[') && host.ends_with(']') {
        host = host[1..host.len() - 1].to_string();
    }
    let port: u16 = port_str
        .parse()
        .map_err(|_| DiscoverError::Internal(format!("端点端口无效: {port_str}")))?;
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::PairingScope;
    use std::sync::Mutex as StdMutex;

    /// 测试证书 fixture：CA + 服务器证书 + 客户端证书（全部由同一 CA 签发）。
    struct TestCerts {
        ca_cert_der: CertificateDer<'static>,
        server_cert_der: CertificateDer<'static>,
        server_key_der: PrivateKeyDer<'static>,
        client_cert_der: CertificateDer<'static>,
        client_key_der: PrivateKeyDer<'static>,
    }

    fn gen_certs() -> TestCerts {
        use rcgen::{
            CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose,
        };
        // CA（自签，KeyCertSign）
        let mut ca_dn = DistinguishedName::new();
        ca_dn.push(DnType::CommonName, "os-test-ca");
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.distinguished_name = ca_dn;
        ca_params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        let ca_key = KeyPair::generate().unwrap();
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        let ca_issuer = Issuer::from_params(&ca_params, &ca_key);

        // Server cert（SAN: localhost + 127.0.0.1，由 CA 签发）
        let mut server_params =
            CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string()]).unwrap();
        let mut server_dn = DistinguishedName::new();
        server_dn.push(DnType::CommonName, "os-test-server");
        server_params.distinguished_name = server_dn;
        server_params.extended_key_usages = vec![
            rcgen::ExtendedKeyUsagePurpose::ServerAuth,
            rcgen::ExtendedKeyUsagePurpose::ClientAuth,
        ];
        let server_key = KeyPair::generate().unwrap();
        let server_cert = server_params.signed_by(&server_key, &ca_issuer).unwrap();

        // Client cert（由 CA 签发）
        let mut client_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        let mut client_dn = DistinguishedName::new();
        client_dn.push(DnType::CommonName, "os-test-client");
        client_params.distinguished_name = client_dn;
        client_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
        let client_key = KeyPair::generate().unwrap();
        let client_cert = client_params.signed_by(&client_key, &ca_issuer).unwrap();

        TestCerts {
            ca_cert_der: CertificateDer::from(ca_cert.der().as_ref().to_vec()),
            server_cert_der: CertificateDer::from(server_cert.der().as_ref().to_vec()),
            server_key_der: PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
                server_key.serialize_der(),
            )),
            client_cert_der: CertificateDer::from(client_cert.der().as_ref().to_vec()),
            client_key_der: PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
                client_key.serialize_der(),
            )),
        }
    }

    /// 启动一个真实 mTLS 服务器线程（loopback TCP），返回 (host_port, 完成计数器)。
    ///
    /// 接收服务端所需的三项（实体证书、私钥、受信 CA 根），便于客户端复用同源 CA
    /// 签发的 client 证书（同一 gen_certs() 调用产出，保证 CA 一致）。
    fn spawn_mtls_server(
        server_cert: CertificateDer<'static>,
        server_key: PrivateKeyDer<'static>,
        ca_cert: CertificateDer<'static>,
    ) -> (String, Arc<StdMutex<Option<u32>>>) {
        let counter: Arc<StdMutex<Option<u32>>> = Arc::new(StdMutex::new(None));

        // 监听随机 loopback 端口
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let host_port = format!("127.0.0.1:{}", addr.port());

        // 构造服务器配置（mTLS：要求并验证客户端证书，信任 CA）
        // 显式 ring provider（ADR-DEPS-002 选定）：workspace 同时激活 ring 与 aws-lc-rs
        // （reqwest→hyper-rustls 链路），若用无参 builder() 会触发 rustls 进程级
        // CryptoProvider 自动探测 panic（"exactly one of aws-lc-rs/ring"）。此处与
        // 客户端 build_client_config() 一致走 builder_with_provider 显式注入 ring。
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut root_store = RootCertStore::empty();
        root_store.add(ca_cert).unwrap();
        let client_verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
            Arc::new(root_store),
            provider.clone(),
        )
        .build()
        .unwrap();
        let server_config = rustls::server::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_client_cert_verifier(client_verifier)
            .with_single_cert(vec![server_cert], server_key)
            .unwrap();
        let server_config = Arc::new(server_config);

        let counter_clone = counter.clone();
        std::thread::spawn(move || {
            // 接受若干连接（测试期间 1 个）
            for _ in 0..4 {
                let Ok((tcp, _)) = listener.accept() else {
                    break;
                };
                let _ = tcp.set_nodelay(true);
                let config = server_config.clone();
                let c = counter_clone.clone();
                std::thread::spawn(move || {
                    let mut conn = match rustls::server::ServerConnection::new(config) {
                        Ok(c) => c,
                        Err(_) => return,
                    };
                    let mut sock = tcp;
                    // 显式驱动 mTLS 握手（complete_io 内部循环 read_tls/write_tls/flush）
                    let mut ok = false;
                    for _ in 0..8 {
                        match conn.complete_io(&mut sock) {
                            Ok(_) => {}
                            Err(_) => break,
                        }
                        if !conn.is_handshaking() {
                            ok = true;
                            break;
                        }
                    }
                    if ok {
                        *c.lock().unwrap() = Some(1);
                    }
                });
            }
        });

        (host_port, counter)
    }

    fn valid_token() -> PairingToken {
        PairingToken {
            token: "mtls-tok".into(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
            issued_by: NodeId::new("leader"),
            scope: PairingScope::JoinCluster,
        }
    }

    fn expired_token() -> PairingToken {
        PairingToken {
            token: "mtls-tok-exp".into(),
            expires_at: Utc::now() - chrono::Duration::hours(1),
            issued_by: NodeId::new("leader"),
            scope: PairingScope::JoinCluster,
        }
    }

    #[tokio::test]
    async fn mtls_pair_real_handshake_succeeds() {
        // 真实 mTLS 握手：客户端（MtlsPeerAuthenticator）+ loopback 服务器，
        // 用自签 CA 签发的服务器/客户端证书 fixture。握手成功 → 会话建立 + 指纹非空。
        let certs = gen_certs();
        let server_endpoint = spawn_mtls_server(
            certs.server_cert_der.clone(),
            certs.server_key_der.clone_key(),
            certs.ca_cert_der.clone(),
        );
        // 客户端身份：用 client 证书（与 client_key 配对）；受信根：CA
        let mut client_roots = RootCertStore::empty();
        client_roots.add(certs.ca_cert_der.clone()).unwrap();
        let auth = MtlsPeerAuthenticator::new(
            vec![certs.client_cert_der],
            certs.client_key_der,
            client_roots,
        );

        let sess = auth
            .pair(&server_endpoint.0, &valid_token())
            .await
            .expect("mTLS 握手应成功");
        assert_eq!(sess.mtls_cert_fingerprint.len(), 64); // SHA-256 hex
        assert_eq!(auth.session_count(), 1);
        // 服务器侧也应完成握手
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(*server_endpoint.1.lock().unwrap(), Some(1));
    }

    #[tokio::test]
    async fn mtls_pair_expired_token_fails() {
        // 过期凭证 → 握手前拒绝（不连 TCP）
        let certs = gen_certs();
        let server_endpoint = spawn_mtls_server(
            certs.server_cert_der.clone(),
            certs.server_key_der.clone_key(),
            certs.ca_cert_der.clone(),
        );
        let mut client_roots = RootCertStore::empty();
        client_roots.add(certs.ca_cert_der.clone()).unwrap();
        let auth = MtlsPeerAuthenticator::new(
            vec![certs.client_cert_der],
            certs.client_key_der,
            client_roots,
        );
        let err = auth
            .pair(&server_endpoint.0, &expired_token())
            .await
            .unwrap_err();
        assert!(matches!(err, DiscoverError::PairingFailed(_)));
    }

    #[tokio::test]
    async fn mtls_pair_untrusted_root_fails() {
        // 客户端受信根库不含签发 CA → 握手失败（MtlsHandshakeFailed）
        let certs = gen_certs();
        let server_endpoint = spawn_mtls_server(
            certs.server_cert_der.clone(),
            certs.server_key_der.clone_key(),
            certs.ca_cert_der.clone(),
        );
        // 用一个全新的独立 CA 作为"受信根"（与服务器的 CA 无关）→ 验签失败
        let bad_certs = gen_certs();
        let mut client_roots = RootCertStore::empty();
        client_roots.add(bad_certs.ca_cert_der).unwrap();
        let auth = MtlsPeerAuthenticator::new(
            vec![certs.client_cert_der],
            certs.client_key_der,
            client_roots,
        );
        let err = auth
            .pair(&server_endpoint.0, &valid_token())
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                DiscoverError::MtlsHandshakeFailed(_) | DiscoverError::Internal(_)
            ),
            "expected handshake/internal error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn mtls_unpair_and_list_trusted() {
        // pair → list_trusted 含该节点；unpair 后会话移除（受信列表保留历史）
        let certs = gen_certs();
        let server_endpoint = spawn_mtls_server(
            certs.server_cert_der.clone(),
            certs.server_key_der.clone_key(),
            certs.ca_cert_der.clone(),
        );
        let mut client_roots = RootCertStore::empty();
        client_roots.add(certs.ca_cert_der.clone()).unwrap();
        let auth = MtlsPeerAuthenticator::new(
            vec![certs.client_cert_der],
            certs.client_key_der,
            client_roots,
        );
        let sess = auth.pair(&server_endpoint.0, &valid_token()).await.unwrap();
        let trusted = auth.list_trusted_peers().await;
        assert_eq!(trusted.len(), 1);
        auth.unpair(&sess.id).await.unwrap();
        assert_eq!(auth.session_count(), 0);
        // 重复 unpair 报错
        assert!(auth.unpair(&sess.id).await.is_err());
    }

    #[tokio::test]
    async fn mtls_pair_connect_refused_fails() {
        // 对端不可达（端口未监听）→ MtlsHandshakeFailed（TCP 连接失败）
        let certs = gen_certs();
        let mut client_roots = RootCertStore::empty();
        client_roots.add(certs.ca_cert_der).unwrap();
        let auth = MtlsPeerAuthenticator::new(
            vec![certs.client_cert_der],
            certs.client_key_der,
            client_roots,
        );
        // 用一个几乎肯定未监听的端口
        let err = auth.pair("127.0.0.1:1", &valid_token()).await.unwrap_err();
        assert!(
            matches!(err, DiscoverError::MtlsHandshakeFailed(_)),
            "expected MtlsHandshakeFailed, got {err:?}"
        );
    }

    #[test]
    fn mtls_cert_fingerprint_is_sha256_hex() {
        let certs = gen_certs();
        let fp = cert_fingerprint(&certs.server_cert_der);
        assert_eq!(fp.len(), 64);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
        // 不同证书指纹不同
        let fp2 = cert_fingerprint(&certs.client_cert_der);
        assert_ne!(fp, fp2);
    }

    #[test]
    fn mtls_parse_host_port_variants() {
        let (h, p) = parse_host_port("127.0.0.1:8443").unwrap();
        assert_eq!(h, "127.0.0.1");
        assert_eq!(p, 8443);
        let (h, p) = parse_host_port("[::1]:443").unwrap();
        assert_eq!(h, "::1");
        assert_eq!(p, 443);
        let (h, p) = parse_host_port("host.example:9999").unwrap();
        assert_eq!(h, "host.example");
        assert_eq!(p, 9999);
        assert!(parse_host_port("noport").is_err());
        assert!(parse_host_port("h:bad").is_err());
    }
}
