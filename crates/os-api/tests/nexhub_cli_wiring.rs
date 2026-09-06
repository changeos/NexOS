//! os-api 集成测——nexhub CLI 分发端点（`GET /api/v1/coderepo/cli.sh`）网关接线
//! （NexHub 网页/CLI 重排 P2，docs/research/NEXHUB_WEB_CLI_DESIGN.md §B / §4.1，
//! 文档 docs/NEXHUB.md）。
//!
//! 验证 **os-api 装配层**契约（handler/渲染单测在 `handlers/nexhub_cli.rs`）：
//!
//! 1. **路由接线**：`NexhubCliRouteHandler` 经 `register_component` 注册后，
//!    `InProcessGateway::dispatch` 完整链路（中间件 → 路由 → handler）可达，
//!    且**无身份 200**（公开读，`requires_auth=false`——未登录机器可达）。
//! 2. **Host 推导穿越网关**：X-Forwarded-Host / Host / 全缺省三档的缺省节点
//!    地址烘焙进脚本（`curl | sh` 装完即连回提供它的节点）。
//! 3. **text 直传契约**：`body` 为 `Value::String` + `text/x-shellscript`
//!    content-type——http.rs `direct_passthrough_bytes()` 据此原文直传
//!    （不走 JSON 信封），本层断言该组合不被破坏。

use os_api::gateway::{ApiRequest, Gateway, HttpMethod};
use os_api::handlers::NexhubCliRouteHandler;
use os_api::InProcessGateway;

const CLI_PATH: &str = "/api/v1/coderepo/cli.sh";

fn req(headers: serde_json::Value) -> ApiRequest {
    ApiRequest {
        method: HttpMethod::Get,
        path: CLI_PATH.to_string(),
        headers,
        body: serde_json::Value::Null,
        auth: None,
    }
}

async fn gw() -> InProcessGateway {
    let gw = InProcessGateway::new();
    gw.register_component("nexhub_cli", Box::new(NexhubCliRouteHandler::new()))
        .await
        .expect("注册 nexhub_cli 应成功");
    gw
}

fn node_default_line(body: &str) -> String {
    body.lines()
        .find(|l| l.contains("NEXHUB_NODE_DEFAULT"))
        .unwrap_or("<NEXHUB_NODE_DEFAULT 缺失>")
        .to_string()
}

/// 完整网关链路：无身份 GET → 200 + text/x-shellscript + 脚本原文直传。
#[tokio::test]
async fn cli_script_served_public_through_gateway() {
    let resp = gw().await.dispatch(req(serde_json::json!({}))).await.0;
    assert_eq!(resp.status, 200, "公开读无需身份");
    let ct = resp.headers["content-type"].as_str().unwrap();
    assert!(ct.starts_with("text/x-shellscript"), "content-type: {ct}");
    let script = resp.body.as_str().expect("body 应为脚本原文（Value::String）");
    assert!(script.starts_with("#!/usr/bin/env"), "脚本原文（非 JSON 信封）");
    assert!(script.contains("NEXHUB_NODE"), "脚本含 NEXHUB_NODE 字样");
    assert!(
        script.contains(env!("CARGO_PKG_VERSION")),
        "脚本烘焙运行二进制版本号"
    );
}

/// Host 推导穿越网关：X-Forwarded-Host 优先，其次 Host，双缺省回落 127.0.0.1:8558。
#[tokio::test]
async fn cli_script_host_derivation_through_gateway() {
    let g = gw().await;
    let forwarded = g
        .dispatch(req(serde_json::json!({
            "host": "internal:9999",
            "x-forwarded-host": "203.0.113.77:8558, 10.0.0.2",
        })))
        .await
        .0;
    assert!(node_default_line(forwarded.body.as_str().unwrap())
        .contains("'http://203.0.113.77:8558'"));

    let host_only = g
        .dispatch(req(serde_json::json!({ "host": "hub.example.com" })))
        .await
        .0;
    assert!(node_default_line(host_only.body.as_str().unwrap())
        .contains("'http://hub.example.com:8558'"), "无端口补 8558");

    let none = g.dispatch(req(serde_json::json!({}))).await.0;
    assert!(node_default_line(none.body.as_str().unwrap())
        .contains("'http://127.0.0.1:8558'"));
}
