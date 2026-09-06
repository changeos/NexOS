//! os-api HTTP 客户端——MCP tools 内部用 reqwest GET 对应路由，返回 JSON。
//!
//! 设计：
//! - [`OsApiClient`] 持有 reqwest `Client` + os-api base URL；`call_path` 给定
//!   相对路径（如 `/api/v1/pools`）→ 拼成完整 URL → GET → 校验 2xx → 反序列化 body
//!   为 `serde_json::Value` 返回。
//! - 为便于测试（不真启 HTTP），暴露一个 [`HttpBackend`] trait：`get(url) -> body`。
//!   生产用 [`ReqwestBackend`]（真实 HTTP），测试用任意 mock backend（如
//!   `tests::StaticBackend` 直接返回预设 JSON 字符串）。
//!
//! 错误模型：网络 / 非 2xx / 反序列化失败统一归为 [`OsMcpError::Api`]，
//! JSON-RPC dispatch 层把它包进 `tools/call` 的 error response（isError=true）。

use crate::error::OsMcpError;
use serde_json::Value;

/// HTTP 后端抽象——把「给定 URL，返回 body 文本」这一动作抽象出来。
///
/// 用**原生 async fn in trait**（workspace rust-version=1.75 已稳定）。
/// 这意味着该 trait **非 dyn 兼容**（与 os-storage `StorageBackend` 同款），
/// 故 `OsApiClient` 用泛型 `B: HttpBackend` 静态分发，非 `Box<dyn HttpBackend>`。
pub trait HttpBackend: Send + Sync {
    /// GET `url`，返回响应 body 文本（utf-8 字符串）。
    ///
    /// 实现负责：发起请求、校验 2xx（非 2xx 返回 Err）、把 body 转成 String。
    /// 错误信息须人类可读（含 url + 状态码 + 简短原因）。
    fn get(
        &self,
        url: &str,
    ) -> impl std::future::Future<Output = Result<String, OsMcpError>> + Send;
}

/// reqwest 实现——真实 HTTP GET。
///
/// 持有共享 `reqwest::Client`（连接池复用）；构造零配置（默认 rustls-tls）。
#[derive(Clone, Default)]
pub struct ReqwestBackend {
    client: reqwest::Client,
}

impl ReqwestBackend {
    /// 构造默认 reqwest 客户端（rustls-tls，无自定义超时——沿用 reqwest 默认）。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl HttpBackend for ReqwestBackend {
    async fn get(&self, url: &str) -> Result<String, OsMcpError> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| OsMcpError::Api(format!("请求 {url} 失败: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(OsMcpError::Api(format!(
                "{url} 返回非 2xx: {status} body={body}"
            )));
        }
        resp.text()
            .await
            .map_err(|e| OsMcpError::Api(format!("读取 {url} 响应体失败: {e}")))
    }
}

/// os-api 客户端——持有 base URL + HTTP 后端，提供 `call_path` / `call_tool` 入口。
///
/// 泛型 `B: HttpBackend`：生产用 `ReqwestBackend`，测试注入 mock backend。
/// 非 dyn 兼容（HttpBackend 是原生 async trait），故静态分发。
pub struct OsApiClient<B: HttpBackend> {
    /// os-api base URL（如 `http://127.0.0.1:8080`，无末尾 /）。
    base: String,
    /// HTTP 后端（真实 reqwest 或测试 mock）。
    backend: B,
}

impl<B: HttpBackend> OsApiClient<B> {
    /// 构造客户端：`base` 为 os-api base URL（自动去末尾 /）。
    #[must_use]
    pub fn new(base: impl Into<String>, backend: B) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            backend,
        }
    }

    /// 调一条 os-api GET 路由（相对路径，如 `/api/v1/pools`），返回解析后的 JSON。
    ///
    /// 步骤：拼 URL → backend.get → serde_json::from_str → Value。
    /// body 不是合法 JSON 时返回 `Api` 错误。
    pub async fn call_path(&self, path: &str) -> Result<Value, OsMcpError> {
        let url = format!("{}{path}", self.base);
        let body = self.backend.get(&url).await?;
        serde_json::from_str::<Value>(&body)
            .map_err(|e| OsMcpError::Api(format!("解析 {url} 响应 JSON 失败: {e} body={body}")))
    }

    /// 调一个 MCP tool（按 tool 的 api_path GET，返回 JSON）。
    ///
    /// 这是 `tools/call` dispatch 的最终落点：tool name → OsTool → call_path。
    pub async fn call_tool(&self, tool: &crate::tools::OsTool) -> Result<Value, OsMcpError> {
        self.call_path(tool.api_path).await
    }

    /// base URL 快照（测试 / 诊断用）。
    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }
}

// ----------------------------------------------------------------------------
// 测试：用静态 mock backend（返回预设字符串），离线验证 call_path / call_tool
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{find_tool, OsTool};
    use std::sync::Arc;

    /// 静态 mock backend：每次 get 都返回预设字符串（成功路径）。
    ///
    /// 用 Arc<Vec<...>> 记录所有请求过的 URL，断言「调对了 URL」。
    struct StaticBackend {
        /// 预设响应体（任意 URL 都返回这个）。
        body: String,
        /// 记录所有请求过的 URL（按序）。
        urls: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl HttpBackend for StaticBackend {
        async fn get(&self, url: &str) -> Result<String, OsMcpError> {
            self.urls.lock().unwrap().push(url.to_string());
            Ok(self.body.clone())
        }
    }

    /// 失败 mock backend：get 总是返回指定的 OsMcpError（测错误传播）。
    struct FailBackend;
    impl HttpBackend for FailBackend {
        async fn get(&self, url: &str) -> Result<String, OsMcpError> {
            Err(OsMcpError::Api(format!("mock 失败: {url}")))
        }
    }

    #[tokio::test]
    async fn call_path_constructs_url_and_parses_json() {
        let urls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let backend = StaticBackend {
            body: r#"{"status":"ok"}"#.to_string(),
            urls: urls.clone(),
        };
        let client = OsApiClient::new("http://127.0.0.1:8080", backend);
        let v = client.call_path("/healthz").await.unwrap();
        assert_eq!(v["status"], "ok");
        // URL 应拼接为 base + path
        let recorded = urls.lock().unwrap();
        assert_eq!(
            recorded.last().map(String::as_str),
            Some("http://127.0.0.1:8080/healthz")
        );
    }

    #[tokio::test]
    async fn call_path_trims_trailing_slash_in_base() {
        let client = OsApiClient::new(
            "http://127.0.0.1:8080/",
            StaticBackend {
                body: "[]".to_string(),
                urls: Arc::new(std::sync::Mutex::new(Vec::new())),
            },
        );
        let _ = client.call_path("/api/v1/pools").await.unwrap();
        assert_eq!(client.base(), "http://127.0.0.1:8080");
    }

    #[tokio::test]
    async fn call_tool_uses_tool_api_path() {
        // 对每个 tool，验证 call_tool GET 的 URL 以 tool.api_path 结尾。
        for tool in crate::tools::all_tools() {
            let urls = Arc::new(std::sync::Mutex::new(Vec::new()));
            let backend = StaticBackend {
                body: r#"{"ok":true}"#.to_string(),
                urls: urls.clone(),
            };
            let client = OsApiClient::new("http://127.0.0.1:8080", backend);
            let _ = client.call_tool(tool).await.unwrap();
            let recorded = urls.lock().unwrap();
            assert!(
                recorded.last().unwrap().ends_with(tool.api_path),
                "tool {} 应 GET {}，实际 {}",
                tool.name,
                tool.api_path,
                recorded.last().unwrap()
            );
        }
    }

    #[tokio::test]
    async fn call_path_propagates_backend_error() {
        let client = OsApiClient::new("http://127.0.0.1:8080", FailBackend);
        let err = client.call_path("/api/v1/pools").await.unwrap_err();
        assert!(matches!(err, OsMcpError::Api(_)));
        assert!(err.to_string().contains("mock 失败"));
    }

    #[tokio::test]
    async fn call_path_returns_error_on_invalid_json() {
        let client = OsApiClient::new(
            "http://127.0.0.1:8080",
            StaticBackend {
                body: "not json".to_string(),
                urls: Arc::new(std::sync::Mutex::new(Vec::new())),
            },
        );
        let err = client.call_path("/api/v1/pools").await.unwrap_err();
        assert!(matches!(err, OsMcpError::Api(_)));
        assert!(err.to_string().contains("解析"));
    }

    /// OsTool 完整流：find_tool → call_tool → JSON 返回。
    #[tokio::test]
    async fn full_flow_find_and_call() {
        let tool: &'static OsTool = find_tool("os_pool_list").unwrap();
        let client = OsApiClient::new(
            "http://127.0.0.1:8080",
            StaticBackend {
                body: r#"[{"name":"tank"}]"#.to_string(),
                urls: Arc::new(std::sync::Mutex::new(Vec::new())),
            },
        );
        let v = client.call_tool(tool).await.unwrap();
        assert_eq!(v[0]["name"], "tank");
    }
}
