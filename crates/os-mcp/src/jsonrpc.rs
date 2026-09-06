//! MCP JSON-RPC 2.0 协议层——请求/响应序列化 + 方法分发。
//!
//! MCP（Model Context Protocol）本质就是 **JSON-RPC 2.0 over stdio**：
//! 客户端从 stdin 写一行 JSON 请求 → server 解析 → 分发 → 往 stdout 写一行 JSON 响应。
//!
//! 本模块把协议层与 IO 层解耦：
//! - [`Request`] / [`Response`] / [`ErrorData`]：serde 模型，可独立单测序列化格式。
//! - [`handle_request`]：纯函数（接收 Request + client 引用 → 返回 Response），
//!   单测可直接喂 Request 断言 Response，不碰 IO。
//! - IO 循环在 [`server`](crate::server) 模块（读 stdin / 写 stdout）。
//!
//! 支持的方法（MCP 核心三件 + tools 扩展）：
//! - `initialize`：握手，返回 server info + capabilities（声明支持 tools）。
//! - `tools/list`：返回全部 MCP tools（name + description + inputSchema）。
//! - `tools/call`：调一个 tool，返回结果文本（调 os-api GET → JSON）。
//! - `ping`（MCP 心跳）：返回空 result。

use crate::api::OsApiClient;
use crate::error::OsMcpError;
use crate::tools::{build_url, find_tool};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// JSON-RPC 2.0 标准 error code（[spec](https://www.jsonrpc.org/specification)）。
pub mod error_code {
    /// 解析错误（JSON 不合法）。
    pub const PARSE_ERROR: i32 = -32700;
    /// 无效请求（非合法 JSON-RPC 对象）。
    pub const INVALID_REQUEST: i32 = -32600;
    /// 方法不存在。
    pub const METHOD_NOT_FOUND: i32 = -32601;
    /// 参数无效。
    pub const INVALID_PARAMS: i32 = -32602;
    /// 内部错误。
    pub const INTERNAL_ERROR: i32 = -32603;
}

// ----------------------------------------------------------------------------
// JSON-RPC 数据模型
// ----------------------------------------------------------------------------

/// JSON-RPC 2.0 请求（MCP 客户端 → server）。
///
/// `id` 可为 number / string / null（spec 允许，用于配对请求/响应）。
/// 通知（notification）是 `id` 缺失的请求——server 不回响应；本实现把「id 缺失」
/// 解析为 `None`，dispatch 时跳过响应。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Request {
    /// 固定 "2.0"。
    pub jsonrpc: String,
    /// 方法名（`initialize` / `tools/list` / `tools/call` / `ping` / ...）。
    pub method: String,
    /// 参数（任意 JSON；`tools/call` 用 `{name, arguments}` 结构）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// 请求 id（缺省表示通知——server 不回响应）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
}

impl Request {
    /// 是否为通知（无 id，server 不回响应）。
    #[must_use]
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// JSON-RPC 2.0 error object（响应里 `error` 字段）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorData {
    /// error code（见 [`error_code`]）。
    pub code: i32,
    /// 人类可读描述。
    pub message: String,
    /// 可选附加数据。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// JSON-RPC 2.0 响应（server → 客户端）。
///
/// `result` 与 `error` 互斥：成功响应 `Some(result)` + `None` error；
/// 错误响应 `None` + `Some(error)`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    /// 固定 "2.0"。
    pub jsonrpc: String,
    /// 与请求配对的 id（number / string / null）。
    pub id: Value,
    /// 成功结果（与 error 互斥）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// 错误（与 result 互斥）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorData>,
}

impl Response {
    /// 构造成功响应（id + result value）。
    #[must_use]
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// 构造错误响应（id + error code/message/data）。
    #[must_use]
    pub fn error(id: Value, code: i32, message: impl Into<String>, data: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(ErrorData {
                code,
                message: message.into(),
                data,
            }),
        }
    }
}

// ----------------------------------------------------------------------------
// MCP 协议方法实现
// ----------------------------------------------------------------------------

/// MCP server info（`initialize` 响应）。
///
/// 声明本 server 支持 `tools` capability（无 resources / prompts / sampling）。
fn server_info() -> Value {
    json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {
            "tools": { "listChanged": false }
        },
        "serverInfo": {
            "name": "os-mcp",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

/// 把一个 [`OsTool`](crate::tools::OsTool) 渲染为 MCP `tools/list` 元素
/// （含 `inputSchema`——本实现所有 tool 都无参，schema 为空对象 + 无 required）。
fn tool_to_mcp_json(t: &crate::tools::OsTool) -> Value {
    json!({
        "name": t.name,
        "description": t.description,
        "inputSchema": {
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }
    })
}

// ----------------------------------------------------------------------------
// 请求分发（纯函数，无 IO——便于单测）
// ----------------------------------------------------------------------------

/// 处理一条 JSON-RPC 请求 → 返回响应（或 None 表示通知不回响应）。
///
/// `client` 注入：`tools/call` 时用其 GET os-api。生产传 `OsApiClient<ReqwestBackend>`，
/// 单测可传 mock backend 的 client。
///
/// 返回 `None` 当且仅当请求是通知（无 id）。
pub async fn handle_request<B: crate::api::HttpBackend>(
    req: &Request,
    client: &OsApiClient<B>,
) -> Option<Response> {
    // 通知（无 id）不回响应——但仍执行副作用（本实现无副作用，直接返回 None）。
    if req.is_notification() {
        return None;
    }
    let id = req.id.clone().unwrap_or(Value::Null);
    let resp = dispatch(req, client).await;
    Some(match resp {
        Ok(v) => Response::success(id, v),
        Err(e) => match e {
            OsMcpError::JsonRpc { code, message } => Response::error(id, code, message, None),
            OsMcpError::Api(msg) => Response::error(
                id,
                error_code::INTERNAL_ERROR,
                format!("tool 调用 os-api 失败: {msg}"),
                None,
            ),
        },
    })
}

/// 内部分发：按 method 路由到对应处理器。
///
/// 返回 `Result<Value, OsMcpError>`：成功 = result value（由 handle_request 包成 success
/// response），失败 = error code/message（包成 error response）。
async fn dispatch<B: crate::api::HttpBackend>(
    req: &Request,
    client: &OsApiClient<B>,
) -> Result<Value, OsMcpError> {
    match req.method.as_str() {
        "initialize" => Ok(server_info()),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({
            "tools": crate::tools::all_tools().iter().map(tool_to_mcp_json).collect::<Vec<_>>()
        })),
        "tools/call" => handle_tool_call(req, client).await,
        other => Err(OsMcpError::method_not_found(other)),
    }
}

/// `tools/call` 处理：从 params.name 定位 tool → client.call_tool → MCP result。
///
/// MCP `tools/call` 响应结构：`{content: [{type:"text", text: "..."}], isError: bool}`。
/// 调用失败（os-api 不可达 / tool 不存在）时 `isError: true`，错误文本放 content[0].text。
async fn handle_tool_call<B: crate::api::HttpBackend>(
    req: &Request,
    client: &OsApiClient<B>,
) -> Result<Value, OsMcpError> {
    let params = req.params.as_ref().ok_or_else(|| {
        OsMcpError::invalid_params("tools/call 缺少 params（应为 {name, arguments}）")
    })?;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| OsMcpError::invalid_params("tools/call params 缺少 name 字段"))?;
    let tool = find_tool(name).ok_or_else(|| {
        OsMcpError::invalid_params(format!("未知 tool: {name}（用 tools/list 查可用 tools）"))
    })?;

    // 调 os-api；失败时包成 MCP error result（isError=true），而非 JSON-RPC error。
    match client.call_tool(tool).await {
        Ok(v) => Ok(json!({
            "content": [{ "type": "text", "text": v.to_string() }],
            "isError": false
        })),
        Err(e) => {
            // 把失败 URL 也塞进文本，便于 AI 助手 / 用户排障（「os-api 是否在跑？」）。
            let url = build_url(client.base(), tool);
            Ok(json!({
                "content": [{ "type": "text", "text": format!("调用 {name} 失败（GET {url}）: {e}") }],
                "isError": true
            }))
        }
    }
}

// ----------------------------------------------------------------------------
// 序列化辅助
// ----------------------------------------------------------------------------

/// 把一个 Response 序列化为单行 JSON（不含换行）——stdout 写出前调用。
///
/// 失败几乎不可能（Response 全 serde 可序列化），失败时返回一个 fallback error JSON。
#[must_use]
pub fn response_to_line(resp: &Response) -> String {
    serde_json::to_string(resp).unwrap_or_else(|_| {
        // 兜底：序列化失败本身是 bug，回退一个固定 error。
        r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"响应序列化失败"}}"#
            .to_string()
    })
}

/// 把一个 JSON-RPC error（无 id，如解析失败时）序列化为单行 JSON。
#[must_use]
pub fn parse_error_response() -> String {
    let resp = Response::error(Value::Null, error_code::PARSE_ERROR, "Parse error", None);
    response_to_line(&resp)
}

// ----------------------------------------------------------------------------
// 单元测试——JSON-RPC 格式 + 方法分发（用 mock backend，离线）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::OsApiClient;
    use crate::tools::all_tools;
    use serde_json::json;

    /// 测试用 mock backend：所有 GET 返回预设 JSON 字符串。
    struct OkBackend {
        body: String,
    }
    impl crate::api::HttpBackend for OkBackend {
        async fn get(&self, _url: &str) -> Result<String, OsMcpError> {
            Ok(self.body.clone())
        }
    }

    fn mock_client(body: &str) -> OsApiClient<OkBackend> {
        OsApiClient::new(
            "http://127.0.0.1:8080",
            OkBackend {
                body: body.to_string(),
            },
        )
    }

    /// 解析一条 JSON-RPC 请求字符串为 Request。
    fn parse_req(s: &str) -> Request {
        serde_json::from_str(s).expect("合法 JSON-RPC 请求")
    }

    // —— 序列化格式 ——

    #[test]
    fn response_success_serializes_to_jsonrpc20() {
        let r = Response::success(json!(42), json!({"ok": true}));
        let v: Value = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 42);
        assert_eq!(v["result"]["ok"], true);
        assert!(v.get("error").is_none() || v["error"].is_null());
    }

    #[test]
    fn response_error_serializes_to_jsonrpc20() {
        let r = Response::error(json!("req-1"), error_code::METHOD_NOT_FOUND, "nope", None);
        let v: Value = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], "req-1");
        assert!(v.get("result").is_none() || v["result"].is_null());
        assert_eq!(v["error"]["code"], -32601);
        assert_eq!(v["error"]["message"], "nope");
    }

    #[test]
    fn response_to_line_is_single_line() {
        let r = Response::success(json!(1), json!({"a": "b"}));
        let line = response_to_line(&r);
        assert!(!line.contains('\n'), "响应行不应含换行");
        assert!(line.contains("\"jsonrpc\":\"2.0\""));
    }

    #[test]
    fn request_parses_id_number_string_null() {
        let r1: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"ping","id":7}"#).unwrap();
        assert_eq!(r1.id, Some(json!(7)));
        let r2: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"ping","id":"abc"}"#).unwrap();
        assert_eq!(r2.id, Some(json!("abc")));
        // 无 id = 通知
        let r3: Request = serde_json::from_str(r#"{"jsonrpc":"2.0","method":"ping"}"#).unwrap();
        assert!(r3.is_notification());
    }

    // —— 方法分发（mock backend，离线）——

    #[tokio::test]
    async fn initialize_returns_server_info_with_tools_capability() {
        let client = mock_client("{}");
        let req = parse_req(r#"{"jsonrpc":"2.0","method":"initialize","id":1}"#);
        let resp = handle_request(&req, &client).await.unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["serverInfo"]["name"], "os-mcp");
        assert_eq!(v["result"]["capabilities"]["tools"]["listChanged"], false);
        // protocolVersion 字段存在
        assert!(v["result"]["protocolVersion"].is_string());
    }

    #[tokio::test]
    async fn tools_list_returns_all_tools_with_input_schema() {
        let client = mock_client("{}");
        let req = parse_req(r#"{"jsonrpc":"2.0","method":"tools/list","id":2}"#);
        let resp = handle_request(&req, &client).await.unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        let tools = v["result"]["tools"].as_array().unwrap();
        assert_eq!(
            tools.len(),
            all_tools().len(),
            "tools/list 应返回全部 tools"
        );
        // 第一个 tool 含 name + description + inputSchema
        assert!(tools[0]["name"].is_string());
        assert!(tools[0]["description"].is_string());
        assert_eq!(tools[0]["inputSchema"]["type"], "object");
        assert!(tools[0]["inputSchema"]["properties"]
            .as_object()
            .unwrap()
            .is_empty());
        // 必需的 10 个 tool 全部出现
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for required in [
            "os_status",
            "os_pool_list",
            "os_dataset_list",
            "os_snapshot_list",
            "os_vm_list",
            "os_share_list",
            "os_user_list",
            "os_node_list",
            "os_virt_check",
            "os_health",
        ] {
            assert!(names.contains(&required), "tools/list 缺 {required}");
        }
    }

    #[tokio::test]
    async fn ping_returns_empty_object() {
        let client = mock_client("{}");
        let req = parse_req(r#"{"jsonrpc":"2.0","method":"ping","id":3}"#);
        let resp = handle_request(&req, &client).await.unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["result"], json!({}));
    }

    #[tokio::test]
    async fn tools_call_returns_text_content_with_api_json() {
        let client = mock_client(r#"{"status":"ok"}"#);
        let req = parse_req(
            r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"os_health","arguments":{}},"id":4}"#,
        );
        let resp = handle_request(&req, &client).await.unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["id"], 4);
        assert_eq!(v["result"]["content"][0]["type"], "text");
        // text 是 os-api 返回的 JSON 字符串
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("\"status\":\"ok\""));
        assert_eq!(v["result"]["isError"], false);
    }

    #[tokio::test]
    async fn tools_call_unknown_tool_returns_invalid_params_error() {
        let client = mock_client("{}");
        let req = parse_req(
            r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"no_such_tool"},"id":5}"#,
        );
        let resp = handle_request(&req, &client).await.unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        // JSON-RPC 层 error（非 tool isError）
        assert_eq!(v["error"]["code"], error_code::INVALID_PARAMS);
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no_such_tool"));
    }

    #[tokio::test]
    async fn tools_call_missing_name_returns_invalid_params() {
        let client = mock_client("{}");
        let req = parse_req(
            r#"{"jsonrpc":"2.0","method":"tools/call","params":{"arguments":{}},"id":6}"#,
        );
        let resp = handle_request(&req, &client).await.unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["error"]["code"], error_code::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn tools_call_no_params_returns_invalid_params() {
        let client = mock_client("{}");
        let req = parse_req(r#"{"jsonrpc":"2.0","method":"tools/call","id":7}"#);
        let resp = handle_request(&req, &client).await.unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["error"]["code"], error_code::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let client = mock_client("{}");
        let req = parse_req(r#"{"jsonrpc":"2.0","method":"resources/list","id":8}"#);
        let resp = handle_request(&req, &client).await.unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["error"]["code"], error_code::METHOD_NOT_FOUND);
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("resources/list"));
    }

    #[tokio::test]
    async fn notification_returns_none_no_response() {
        // 无 id 的请求 = 通知，server 不回响应。
        let client = mock_client("{}");
        let req = parse_req(r#"{"jsonrpc":"2.0","method":"ping"}"#);
        assert!(req.is_notification());
        let resp = handle_request(&req, &client).await;
        assert!(resp.is_none(), "通知不应产生响应");
    }

    #[tokio::test]
    async fn tools_call_backend_failure_returns_is_error_true() {
        // backend 总返回失败 → tools/call 应返回 isError=true 的 result（非 JSON-RPC error）。
        struct Fail;
        impl crate::api::HttpBackend for Fail {
            async fn get(&self, _url: &str) -> Result<String, OsMcpError> {
                Err(OsMcpError::Api("os-api 连接拒绝".to_string()))
            }
        }
        let client = OsApiClient::new("http://127.0.0.1:8080", Fail);
        let req = parse_req(
            r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"os_pool_list"},"id":9}"#,
        );
        let resp = handle_request(&req, &client).await.unwrap();
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["result"]["isError"], true);
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("os_pool_list"));
        assert!(text.contains("失败"));
    }

    /// parse_error_response 返回标准 JSON-RPC parse error 单行 JSON。
    #[test]
    fn parse_error_response_format() {
        let line = parse_error_response();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["error"]["code"], error_code::PARSE_ERROR);
    }
}
