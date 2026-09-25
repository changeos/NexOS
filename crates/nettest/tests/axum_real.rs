//! axum 真实端口监听 + reqwest 真实 HTTP 请求验证（loopback）。
//!
//! 验证 axum + hyper 真实绑定一个 OS 分配的端口（127.0.0.1:0），用 reqwest 真实发起
//! TCP 连接打这个端口，断言响应。这一路验证的是 os-api/os-guest 选用的服务端栈
//! 能真实监听 + 处理请求。

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use axum::{routing::get, Router};
use common::timeout_or_panic;

/// axum 真实监听 + reqwest 真实 HTTP：绑随机端口，reqwest 打真实请求，断言 200 + body。
#[tokio::test]
#[ignore = "真实端口监听：手动 `cargo test -p nettest -- --ignored axum_real_listen_and_get`"]
async fn axum_real_listen_and_get() {
    timeout_or_panic(async {
        // 简单 handler：返回固定文本。
        let app = Router::new().route("/healthz", get(|| async { "nettest-ok" }));

        // 绑 127.0.0.1:0 → OS 分配真实可用端口。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("TcpListener bind 失败");
        let addr: SocketAddr = listener.local_addr().expect("local_addr 失败");
        eprintln!("[nettest] axum 监听 {addr}");

        // 服务端：spawn accept loop（axum 0.8 API）。
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("axum::serve 出错");
        });

        // 给内核一点时间把 listen 队列建好。
        tokio::time::sleep(Duration::from_millis(100)).await;

        // 客户端：reqwest 真实 HTTP GET。
        let url = format!("http://{addr}/healthz");
        let resp = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest Client 构建失败")
            .get(&url)
            .send()
            .await
            .expect("reqwest GET 失败");

        let status = resp.status();
        assert!(status.is_success(), "状态码异常: {status}");
        let body = resp.text().await.expect("读取 body 失败");
        assert_eq!(body, "nettest-ok", "响应 body 不符");
        eprintln!("[nettest] reqwest 打 axum 真实端口 {addr} → HTTP {status}, body = {body:?}");

        // 收尾：关闭服务端。
        server.abort();
    })
    .await;
}
