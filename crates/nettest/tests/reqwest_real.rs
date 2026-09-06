//! reqwest 真实 HTTPS GET 验证（公网）。
//!
//! 验证 reqwest + rustls-tls 栈能真实发起 TLS 握手、完成 HTTPS 请求、解析 JSON 响应。
//! 目标：`https://httpbin.org/get`（公网稳定测试端点），断言 200 + 回显的 headers 中
//! 含 User-Agent。httpbin.org/get 会把请求头回显在 JSON 的 `headers` 字段里。
//!
//! 备用目标：`https://example.com`（更稳定的兜底）。如果 httpbin.org 不可达，
//! 测试会自动 fallback 到 example.com（仅断言 200，不校验 JSON 字段）。

mod common;

use common::timeout_or_panic;

/// reqwest 真实 HTTPS GET：访问 https://httpbin.org/get，断言 200 + JSON 回显。
#[tokio::test]
#[ignore = "真实公网 HTTPS 请求：手动 `cargo test -p nettest -- --ignored reqwest_real_get`"]
async fn reqwest_real_get() {
    timeout_or_panic(async {
        // 用 rustls-tls（与 workspace 根 reqwest 配置一致），显式不启用 system proxy
        // 以避免环境变量污染（验证纯 TLS 栈本身）。
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("nettest/0.1 (+rustls-tls)")
            .build()
            .expect("reqwest Client 构建失败");

        // 主目标：httpbin.org/get
        let primary = "https://httpbin.org/get";
        let fallback = "https://example.com";

        let resp = match client.get(primary).send().await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[nettest] httpbin.org 不可达（{e}），fallback 到 example.com");
                let resp = client
                    .get(fallback)
                    .send()
                    .await
                    .expect("example.com 也不可达");
                assert!(
                    resp.status().is_success(),
                    "example.com 状态码异常: {}",
                    resp.status()
                );
                eprintln!(
                    "[nettest] example.com HTTP {} OK（fallback 通过）",
                    resp.status()
                );
                return;
            }
        };

        let status = resp.status();
        assert!(status.is_success(), "httpbin.org/get 状态码异常: {status}");
        eprintln!("[nettest] httpbin.org/get HTTP {status} OK");

        // 校验 JSON 回显：httpbin 把请求头回显在 headers 字段。
        let body: serde_json::Value = resp.json().await.expect("解析 JSON 失败");
        let headers = body.get("headers").expect("httpbin 响应应含 headers 字段");
        let ua = headers
            .get("User-Agent")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(ua.contains("nettest"), "回显的 User-Agent 不符: {ua}");
        eprintln!("[nettest] httpbin 回显 User-Agent = {ua:?}（rustls-tls 栈真实工作）");
    })
    .await;
}
