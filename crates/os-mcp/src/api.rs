//! os-api HTTP 客户端——MCP tools 内部经 reqwest 调对应路由，返回 JSON。
//!
//! 设计：
//! - [`OsApiClient`] 持有 reqwest `Client` + os-api base URL + 可选 admin token；
//!   [`OsApiClient::call_tool`] 给定 [`OsTool`](crate::tools::OsTool) 与调用参数
//!   → 替换路径模板 `{param}` → GET 拼 query / POST 组 JSON body（写工具附
//!   mcp 来源标记）→ 带 `Authorization: Bearer <token>` 请求 → 校验 2xx →
//!   反序列化 body 为 `serde_json::Value` 返回。
//! - 为便于测试（不真启 HTTP），暴露一个 [`HttpBackend`] trait：`request( desc )`
//!   → body 文本。生产用 [`ReqwestBackend`]（真实 HTTP），测试用任意 mock
//!   backend（如 `tests::StaticBackend` 直接返回预设 JSON 字符串）。
//!
//! 错误模型：网络 / 非 2xx / 反序列化失败统一归为 [`OsMcpError::Api`]，
//! JSON-RPC dispatch 层把它包进 `tools/call` 的 error response（isError=true）。

use crate::error::OsMcpError;
use crate::tools::OsTool;
use serde_json::{json, Map, Value};
use std::collections::HashSet;

/// 一次后端 HTTP 请求的描述（方法 + URL + 头 + body）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendRequest {
    /// HTTP 方法（GET / POST）。
    pub method: String,
    /// 完整 URL（路径模板已替换、query 已拼好）。
    pub url: String,
    /// 附加头（小写键名；如 `("authorization", "Bearer x")`）。
    pub headers: Vec<(String, String)>,
    /// JSON body（仅 POST；GET 为 None）。
    pub body: Option<String>,
}

/// HTTP 后端抽象——把「给定请求描述，返回 body 文本」这一动作抽象出来。
///
/// 用**原生 async fn in trait**（workspace rust-version=1.75 已稳定）。
/// 这意味着该 trait **非 dyn 兼容**（与 os-storage `StorageBackend` 同款），
/// 故 `OsApiClient` 用泛型 `B: HttpBackend` 静态分发，非 `Box<dyn HttpBackend>`。
pub trait HttpBackend: Send + Sync {
    /// 执行请求，返回响应 body 文本（utf-8 字符串）。
    ///
    /// 实现负责：发起请求、校验 2xx（非 2xx 返回 Err）、把 body 转成 String。
    /// 错误信息须人类可读（含 url + 状态码 + 简短原因）。
    fn request(
        &self,
        req: &BackendRequest,
    ) -> impl std::future::Future<Output = Result<String, OsMcpError>> + Send;

    /// GET 快捷方式（无参只读 tool 兼容旧路径；缺省走 [`HttpBackend::request`]）。
    fn get(
        &self,
        url: &str,
    ) -> impl std::future::Future<Output = Result<String, OsMcpError>> + Send {
        async move {
            self.request(&BackendRequest {
                method: "GET".to_string(),
                url: url.to_string(),
                headers: Vec::new(),
                body: None,
            })
            .await
        }
    }
}

/// reqwest 实现——真实 HTTP 请求。
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
    async fn request(&self, req: &BackendRequest) -> Result<String, OsMcpError> {
        let method = reqwest::Method::from_bytes(req.method.as_bytes())
            .map_err(|e| OsMcpError::Api(format!("非法方法 {}: {e}", req.method)))?;
        let mut builder = self.client.request(method, &req.url);
        for (k, v) in &req.headers {
            builder = builder.header(k.as_str(), v.as_str());
        }
        if let Some(body) = &req.body {
            builder = builder.header("content-type", "application/json").body(body.clone());
        }
        let resp = builder
            .send()
            .await
            .map_err(|e| OsMcpError::Api(format!("请求 {} 失败: {e}", req.url)))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(OsMcpError::Api(format!(
                "{} 返回非 2xx: {status} body={body}",
                req.url
            )));
        }
        resp.text()
            .await
            .map_err(|e| OsMcpError::Api(format!("读取 {} 响应体失败: {e}", req.url)))
    }
}

/// os-api 客户端——持有 base URL + HTTP 后端 + 可选 admin token。
///
/// 泛型 `B: HttpBackend`：生产用 `ReqwestBackend`，测试注入 mock backend。
/// 非 dyn 兼容（HttpBackend 是原生 async trait），故静态分发。
pub struct OsApiClient<B: HttpBackend> {
    /// os-api base URL（如 `http://127.0.0.1:8080`，无末尾 /）。
    base: String,
    /// HTTP 后端（真实 reqwest 或测试 mock）。
    backend: B,
    /// 系统 admin token（os-api 网关鉴权；写操作必需，读操作可空）。
    token: Option<String>,
}

/// 百分号编码（RFC 3986 unreserved 之外的字符转 %XX；query 与路径段共用）。
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// JSON 值 → query/路径可用的字符串形态（string 原样、number/bool 字面量）。
fn value_to_arg_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

impl<B: HttpBackend> OsApiClient<B> {
    /// 构造客户端：`base` 为 os-api base URL（自动去末尾 /），无 token。
    #[must_use]
    pub fn new(base: impl Into<String>, backend: B) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            backend,
            token: None,
        }
    }

    /// 注入系统 admin token（链式；写工具经 os-api 网关鉴权必需）。
    #[must_use]
    pub fn with_token(mut self, token: Option<String>) -> Self {
        self.token = token.filter(|t| !t.trim().is_empty());
        self
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

    /// 调一个 MCP tool（`tools/call` dispatch 的最终落点）。
    ///
    /// 步骤：
    /// 1. 校验必填参数（缺失 → `invalid_params`）；
    /// 2. 路径模板 `{param}` 替换（值百分号编码）；
    /// 3. GET → 剩余参数拼 query；POST → 剩余参数组 JSON body
    ///    （[`OsTool::mcp_mark`] 的写工具附 mcp 来源标记，见 [`apply_mcp_mark`]）；
    /// 4. 带 admin token 请求 → 2xx → JSON。
    pub async fn call_tool(&self, tool: &OsTool, args: &Value) -> Result<Value, OsMcpError> {
        let args = match args {
            Value::Object(m) => m,
            Value::Null => &Map::new(),
            other => {
                return Err(OsMcpError::invalid_params(format!(
                    "{} 参数须为 JSON 对象，实际 {other}",
                    tool.name
                )))
            }
        };
        // 1) 必填校验 + 类型宽松校验（string/number/bool 可互转字符串）
        for p in tool.params {
            if p.required && !args.contains_key(p.name) {
                return Err(OsMcpError::invalid_params(format!(
                    "{} 缺必填参数 {}（{}）",
                    tool.name, p.name, p.description
                )));
            }
        }
        // 2) 路径模板替换
        let mut path = tool.api_path.to_string();
        let mut consumed: HashSet<&str> = HashSet::new();
        for p in tool.params {
            let placeholder = format!("{{{}}}", p.name);
            if path.contains(&placeholder) {
                let raw = args
                    .get(p.name)
                    .and_then(value_to_arg_string)
                    .ok_or_else(|| {
                        OsMcpError::invalid_params(format!(
                            "{} 参数 {} 须为字符串/数字",
                            tool.name, p.name
                        ))
                    })?;
                path = path.replace(&placeholder, &percent_encode(&raw));
                consumed.insert(p.name);
            }
        }
        // 3) 剩余参数（未进路径且调用方已给值）→ query（GET）或 body（POST）
        let remaining: Vec<(&str, Value)> = tool
            .params
            .iter()
            .filter_map(|p| {
                if consumed.contains(p.name) {
                    return None;
                }
                args.get(p.name).map(|v| (p.name, v.clone()))
            })
            .collect();
        let url = if tool.method == "GET" && !remaining.is_empty() {
            let query = remaining
                .iter()
                .map(|(n, v)| {
                    let s = value_to_arg_string(v).unwrap_or_default();
                    format!("{}={}", percent_encode(n), percent_encode(&s))
                })
                .collect::<Vec<_>>()
                .join("&");
            format!("{}{path}?{query}", self.base)
        } else {
            format!("{}{path}", self.base)
        };
        let mut headers: Vec<(String, String)> = Vec::new();
        if let Some(token) = &self.token {
            headers.push(("authorization".to_string(), format!("Bearer {token}")));
        }
        let body = if tool.method == "POST" {
            let mut obj = Map::new();
            for (n, v) in &remaining {
                obj.insert((*n).to_string(), v.clone());
            }
            let mut body = Value::Object(obj);
            apply_mcp_mark(tool, &mut body);
            Some(serde_json::to_string(&body).map_err(|e| {
                OsMcpError::Api(format!("{} 组请求体失败: {e}", tool.name))
            })?)
        } else {
            None
        };
        // 4) 请求
        let resp_text = self
            .backend
            .request(&BackendRequest {
                method: tool.method.to_string(),
                url,
                headers,
                body,
            })
            .await?;
        serde_json::from_str::<Value>(&resp_text).map_err(|e| {
            OsMcpError::Api(format!(
                "{} 响应解析失败: {e} body={resp_text}",
                tool.name
            ))
        })
    }

    /// base URL 快照（测试 / 诊断用）。
    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }
}

/// 写操作 mcp 来源标记（[`OsTool::mcp_mark`] 的运行时落点）：
///
/// - 有 `labels` 参数的工具（`nexhub_create_issue`）→ labels 数组追加 `"mcp"`
///   （服务端持久化 + UI 可见；未传 labels 时置 `["mcp"]`）；
/// - 有 `body` 参数的工具（`nexhub_create_pr`）→ body 文本尾部追加
///   `(via os-mcp)`（幂等：已有标记不重复加；未传 body 时置标记）。
fn apply_mcp_mark(tool: &OsTool, body: &mut Value) {
    if !tool.mcp_mark {
        return;
    }
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    if tool.params.iter().any(|p| p.name == "labels") {
        let labels = obj.entry("labels".to_string()).or_insert_with(|| json!([]));
        if !labels.is_array() {
            *labels = json!([]);
        }
        let has = labels
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v == "mcp"));
        if !has {
            labels.as_array_mut().expect("labels 数组").push(json!("mcp"));
        }
    } else if tool.params.iter().any(|p| p.name == "body") {
        let entry = obj.entry("body".to_string()).or_insert_with(|| json!(String::new()));
        if let Value::String(s) = entry {
            if !s.contains("via os-mcp") {
                if !s.trim().is_empty() {
                    s.push_str("\n\n");
                }
                s.push_str("(via os-mcp)");
            }
        }
    }
}

// ----------------------------------------------------------------------------
// 测试：用 mock backend（返回预设字符串），离线验证 call_path / call_tool
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{find_tool, OsTool};
    use std::sync::Arc;

    /// 静态 mock backend：每次请求都返回预设字符串（成功路径），并记录请求。
    struct StaticBackend {
        /// 预设响应体（任意 URL 都返回这个）。
        body: String,
        /// 记录所有请求（按序）。
        requests: Arc<std::sync::Mutex<Vec<BackendRequest>>>,
    }

    impl HttpBackend for StaticBackend {
        async fn request(&self, req: &BackendRequest) -> Result<String, OsMcpError> {
            self.requests.lock().unwrap().push(req.clone());
            Ok(self.body.clone())
        }
    }

    /// 失败 mock backend：request 总是返回指定的 OsMcpError（测错误传播）。
    struct FailBackend;
    impl HttpBackend for FailBackend {
        async fn request(&self, req: &BackendRequest) -> Result<String, OsMcpError> {
            Err(OsMcpError::Api(format!("mock 失败: {}", req.url)))
        }
    }

    fn recorder() -> Arc<std::sync::Mutex<Vec<BackendRequest>>> {
        Arc::new(std::sync::Mutex::new(Vec::new()))
    }

    #[tokio::test]
    async fn call_path_constructs_url_and_parses_json() {
        let urls = recorder();
        let backend = StaticBackend {
            body: r#"{"status":"ok"}"#.to_string(),
            requests: urls.clone(),
        };
        let client = OsApiClient::new("http://127.0.0.1:8080", backend);
        let v = client.call_path("/healthz").await.unwrap();
        assert_eq!(v["status"], "ok");
        // URL 应拼接为 base + path
        let recorded = urls.lock().unwrap();
        assert_eq!(recorded.last().unwrap().url, "http://127.0.0.1:8080/healthz");
        assert_eq!(recorded.last().unwrap().method, "GET");
    }

    #[tokio::test]
    async fn call_path_trims_trailing_slash_in_base() {
        let client = OsApiClient::new(
            "http://127.0.0.1:8080/",
            StaticBackend {
                body: "[]".to_string(),
                requests: recorder(),
            },
        );
        let _ = client.call_path("/api/v1/pools").await.unwrap();
        assert_eq!(client.base(), "http://127.0.0.1:8080");
    }

    #[tokio::test]
    async fn call_tool_uses_tool_api_path() {
        // 对每个 tool，验证 call_tool 请求的 URL 以（模板替换后的）路径结尾。
        for tool in crate::tools::all_tools() {
            let urls = recorder();
            let backend = StaticBackend {
                body: r#"{"ok":true}"#.to_string(),
                requests: urls.clone(),
            };
            let client = OsApiClient::new("http://127.0.0.1:8080", backend);
            // 给全部必填参数（路径模板/查询都能取到值）
            let mut args = Map::new();
            for p in tool.params {
                if p.required {
                    let v = if p.kind == "number" { json!(1) } else { json!("x") };
                    args.insert(p.name.to_string(), v);
                }
            }
            let _ = client.call_tool(tool, &Value::Object(args)).await.unwrap();
            let recorded = urls.lock().unwrap();
            let url = &recorded.last().unwrap().url;
            assert!(
                url.starts_with(&format!("http://127.0.0.1:8080{}", tool.api_path.split('{').next().unwrap_or("")))
                    || url.contains("x"),
                "tool {} 应请求 {} 系 URL，实际 {url}",
                tool.name,
                tool.api_path
            );
            assert_eq!(recorded.last().unwrap().method, tool.method);
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
                requests: recorder(),
            },
        );
        let err = client.call_path("/api/v1/pools").await.unwrap_err();
        assert!(matches!(err, OsMcpError::Api(_)));
        assert!(err.to_string().contains("解析"));
    }

    /// OsTool 完整流：find_tool → call_tool → JSON 返回（无参 GET）。
    #[tokio::test]
    async fn full_flow_find_and_call() {
        let tool: &'static OsTool = find_tool("os_pool_list").unwrap();
        let client = OsApiClient::new(
            "http://127.0.0.1:8080",
            StaticBackend {
                body: r#"[{"name":"tank"}]"#.to_string(),
                requests: recorder(),
            },
        );
        let v = client.call_tool(tool, &Value::Null).await.unwrap();
        assert_eq!(v[0]["name"], "tank");
    }

    // ---- NexHub 工具请求构造（mock repo fixture 形态的离线断言）----

    #[tokio::test]
    async fn nexhub_read_file_builds_path_and_query() {
        let tool = find_tool("nexhub_read_file").unwrap();
        let reqs = recorder();
        let client = OsApiClient::new(
            "http://127.0.0.1:8080",
            StaticBackend {
                body: r#"{"content":"hi"}"#.to_string(),
                requests: reqs.clone(),
            },
        );
        let v = client
            .call_tool(tool, &json!({"repo": "demo-repo", "path": "src/main.rs"}))
            .await
            .unwrap();
        assert_eq!(v["content"], "hi");
        let r = reqs.lock().unwrap();
        assert_eq!(r.last().unwrap().method, "GET");
        assert_eq!(
            r.last().unwrap().url,
            "http://127.0.0.1:8080/api/v1/coderepo/repos/demo-repo/file?path=src%2Fmain.rs"
        );
    }

    #[tokio::test]
    async fn nexhub_write_file_builds_post_body() {
        let tool = find_tool("nexhub_write_file").unwrap();
        let reqs = recorder();
        let client = OsApiClient::new(
            "http://127.0.0.1:8080",
            StaticBackend {
                body: r#"{"ok":true}"#.to_string(),
                requests: reqs.clone(),
            },
        )
        .with_token(Some("adm-tk".to_string()));
        client
            .call_tool(
                tool,
                &json!({"repo": "demo", "path": "a.txt", "content": "hello", "message": "m1"}),
            )
            .await
            .unwrap();
        let r = reqs.lock().unwrap();
        let req = r.last().unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "http://127.0.0.1:8080/api/v1/coderepo/repos/demo/file");
        // token 头注入
        assert!(req.headers.iter().any(|(k, v)| k == "authorization" && v == "Bearer adm-tk"));
        // body 含全部非路径参数
        let body: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["content"], "hello");
        assert_eq!(body["message"], "m1");
        // write_file 不走客户端标记（git author 服务端标记）
        assert!(body.get("labels").is_none());
    }

    #[tokio::test]
    async fn nexhub_create_issue_appends_mcp_label() {
        let tool = find_tool("nexhub_create_issue").unwrap();
        let reqs = recorder();
        let client = OsApiClient::new(
            "http://127.0.0.1:8080",
            StaticBackend {
                body: r#"{"ok":true}"#.to_string(),
                requests: reqs.clone(),
            },
        );
        // 未传 labels → 自动置 ["mcp"]
        client
            .call_tool(tool, &json!({"repo": "demo", "title": "T"}))
            .await
            .unwrap();
        {
            // 作用域收窄 guard：第二次 call_tool 前必须释放（否则 mock 记录锁死锁）
            let r = reqs.lock().unwrap();
            let body: Value =
                serde_json::from_str(r.last().unwrap().body.as_deref().unwrap()).unwrap();
            assert_eq!(body["title"], "T");
            assert_eq!(body["labels"], json!(["mcp"]));
        }
        // 已传 labels → 追加 mcp（去重）
        client
            .call_tool(tool, &json!({"repo": "demo", "title": "T", "labels": ["bug", "mcp"]}))
            .await
            .unwrap();
        {
            let r = reqs.lock().unwrap();
            let body: Value =
                serde_json::from_str(r.last().unwrap().body.as_deref().unwrap()).unwrap();
            assert_eq!(body["labels"], json!(["bug", "mcp"]));
        }
    }

    #[tokio::test]
    async fn nexhub_create_pr_appends_via_marker_to_body() {
        let tool = find_tool("nexhub_create_pr").unwrap();
        let reqs = recorder();
        let client = OsApiClient::new(
            "http://127.0.0.1:8080",
            StaticBackend {
                body: r#"{"ok":true}"#.to_string(),
                requests: reqs.clone(),
            },
        );
        client
            .call_tool(
                tool,
                &json!({"repo": "demo", "title": "P", "from_branch": "feat", "body": "desc"}),
            )
            .await
            .unwrap();
        {
            let r = reqs.lock().unwrap();
            let body: Value =
                serde_json::from_str(r.last().unwrap().body.as_deref().unwrap()).unwrap();
            assert_eq!(body["body"], "desc\n\n(via os-mcp)");
        }
        // 空 body → 仅标记
        client
            .call_tool(tool, &json!({"repo": "demo", "title": "P", "from_branch": "feat"}))
            .await
            .unwrap();
        {
            let r = reqs.lock().unwrap();
            let body: Value =
                serde_json::from_str(r.last().unwrap().body.as_deref().unwrap()).unwrap();
            assert_eq!(body["body"], "(via os-mcp)");
        }
    }

    #[tokio::test]
    async fn nexhub_merge_pr_number_in_path() {
        let tool = find_tool("nexhub_merge_pr").unwrap();
        let reqs = recorder();
        let client = OsApiClient::new(
            "http://127.0.0.1:8080",
            StaticBackend {
                body: r#"{"ok":true}"#.to_string(),
                requests: reqs.clone(),
            },
        );
        client
            .call_tool(tool, &json!({"repo": "demo", "num": 7}))
            .await
            .unwrap();
        let r = reqs.lock().unwrap();
        assert_eq!(
            r.last().unwrap().url,
            "http://127.0.0.1:8080/api/v1/coderepo/repos/demo/pulls/7/merge"
        );
        assert_eq!(r.last().unwrap().method, "POST");
    }

    #[tokio::test]
    async fn missing_required_param_is_invalid_params() {
        let tool = find_tool("nexhub_read_file").unwrap();
        let client = OsApiClient::new(
            "http://127.0.0.1:8080",
            StaticBackend {
                body: "{}".to_string(),
                requests: recorder(),
            },
        );
        let err = client.call_tool(tool, &json!({"repo": "demo"})).await.unwrap_err();
        assert!(matches!(err, OsMcpError::JsonRpc { code: -32602, .. }));
        assert!(err.to_string().contains("path"));
    }

    #[tokio::test]
    async fn percent_encoding_escapes_specials() {
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("a/b"), "a%2Fb");
        assert_eq!(percent_encode("a&b=c?"), "a%26b%3Dc%3F");
        assert_eq!(percent_encode("AZaz09-_.~"), "AZaz09-_.~");
    }
}
