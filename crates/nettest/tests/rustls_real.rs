//! rustls 真实 TLS 握手验证（loopback）。
//!
//! 验证 rustls（ring 后端，与 workspace 根配置一致）+ tokio-rustls 能真实完成 TLS
//! 握手 + 加密数据传输。用 rcgen 自签一张证书（CN=localhost + SAN localhost/127.0.0.1），
//! 服务端用这张证书终结 TLS，客户端用 root 信任该自签证书（自定义 ServerConfig），
//! 真实发起 TLS 连接 + 走一遍加密 echo。这一路验证 os-discover mTLS / os-security
//! 证书栈能真实工作。
//!
//! 注意：本测全程 loopback（127.0.0.1），不依赖任何公网或组播，结果稳定可复现。

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use common::timeout_or_panic;
use rustls::pki_types::{CertificateDer, DnsName, PrivateKeyDer, ServerName};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::{TlsAcceptor, TlsConnector};

/// rustls 真实 TLS 握手（loopback）：rcgen 自签证书 + 服务端终结 + 客户端验证 + 加密 echo。
#[tokio::test]
#[ignore = "真实 TLS 握手：手动 `cargo test -p nettest -- --ignored rustls_real_tls_handshake`"]
async fn rustls_real_tls_handshake() {
    timeout_or_panic(async {
        // 0. 安装 ring CryptoProvider（rustls 0.23 要求进程级显式选择 provider）。
        //    workspace 根 rustls 配置了 features=["ring"]，这里把 ring 装为默认。
        //    install_default 幂等（已装则返回 Err，忽略即可）。
        let _ = rustls::crypto::ring::default_provider().install_default();

        // 1. rcgen 自签证书：CN=localhost，SAN 包含 localhost + 127.0.0.1。
        //    rcgen 0.14 API：generate_simple_self_signed → CertifiedKey { cert, signing_key }。
        let rcgen::CertifiedKey { cert, signing_key } = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])
        .expect("rcgen 生成证书失败");

        let cert_der: CertificateDer<'static> = cert.der().clone();
        // signing_key.serialize_der() → Vec<u8>（PKCS#8 DER 私钥）。转 PrivateKeyDer（拥有所有权）。
        let key_der: PrivateKeyDer<'static> = PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der()),
        );
        eprintln!("[nettest] rcgen 自签证书已生成（CN=localhost, SAN=localhost,127.0.0.1）");

        // 2. 服务端 ServerConfig：用自签证书 + 私钥。
        let server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der.clone_key())
            .expect("ServerConfig 构造失败");
        let acceptor = TlsAcceptor::from(Arc::new(server_config));

        // 3. 客户端 ServerConfig：把自签证书作为唯一 root 信任（不校验系统 CA）。
        let mut root_store = rustls::RootCertStore::empty();
        root_store
            .add(cert_der.clone())
            .expect("add root cert 失败");
        let client_config = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(client_config));

        // 4. 起 loopback TCP 服务端，把 TLS acceptor 套上去 + 加密 echo。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("TcpListener bind 失败");
        let addr: SocketAddr = listener.local_addr().expect("local_addr 失败");
        eprintln!("[nettest] TLS 服务端监听 {addr}");

        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.expect("accept 失败");
            let mut tls = acceptor.accept(sock).await.expect("TLS accept 失败");
            eprintln!("[nettest] 服务端：TLS 握手完成");
            // echo：读一行回写。
            let mut buf = [0u8; 64];
            let n = tls.read(&mut buf).await.expect("服务端 read 失败");
            tls.write_all(&buf[..n]).await.expect("服务端 write 失败");
            tls.flush().await.expect("服务端 flush 失败");
            tls.shutdown().await.ok();
        });

        // 5. 客户端：真实 TLS 连接 + 发送 + 收回 echo。
        // 给服务端一点时间 accept。
        tokio::time::sleep(Duration::from_millis(100)).await;

        let sock = tokio::net::TcpStream::connect(addr)
            .await
            .expect("TCP connect 失败");
        // SAN 含 "localhost"，用 DnsName("localhost") 让 rustls 校验通过。
        let server_name: ServerName<'static> = DnsName::try_from("localhost")
            .map(ServerName::from)
            .expect("DnsName 构造失败");
        let mut tls = connector
            .connect(server_name, sock)
            .await
            .expect("TLS connect 失败");
        eprintln!("[nettest] 客户端：TLS 握手完成（rustls 真实验证自签证书通过）");

        let payload = b"nettest-tls-echo";
        tls.write_all(payload).await.expect("客户端 write 失败");
        tls.flush().await.expect("客户端 flush 失败");

        let mut buf = [0u8; 64];
        let n = tls.read(&mut buf).await.expect("客户端 read 失败");
        assert_eq!(&buf[..n], payload, "TLS echo 回环数据不符");
        eprintln!(
            "[nettest] rustls 加密 echo 通过：发送 {:?} → 收回 {:?}",
            &payload[..],
            &buf[..n]
        );

        server.await.expect("服务端 task panic");
    })
    .await;
}
