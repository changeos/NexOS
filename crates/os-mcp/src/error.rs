//! os-mcp 错误类型（thiserror）。
//!
//! 错误分两类：
//! - [`OsMcpError::Api`]：调 os-api HTTP 网关失败（网络 / 非 2xx / 反序列化）。
//! - [`OsMcpError::JsonRpc`]：MCP JSON-RPC 协议层错误（未知方法 / 未知 tool /
//!   参数缺失），携带 JSON-RPC error code（见 [`jsonrpc::error_code`](crate::jsonrpc::error_code)）。
//!
//! 设计：本 crate 的对外 fn（lib API）返回 `Result<T, OsMcpError>`；
//! JSON-RPC dispatch 层把 `OsMcpError` 翻译成 MCP 客户端可见的 error response。

use thiserror::Error;

/// os-mcp 统一错误类型。
#[derive(Debug, Error)]
pub enum OsMcpError {
    /// 调 os-api HTTP 网关失败（连接拒绝 / DNS 失败 / 超时 / 非 2xx / 反序列化失败）。
    #[error("os-api 调用失败: {0}")]
    Api(String),

    /// JSON-RPC 协议错误——携带标准 JSON-RPC error code（-32601 方法不存在 /
    /// -32602 参数无效 / -32603 内部错误 / 等），由 dispatch 层翻译成 error response。
    #[error("JSON-RPC error ({code}): {message}")]
    JsonRpc {
        /// JSON-RPC error code（见 ErrorCode 常量）。
        code: i32,
        /// 人类可读错误描述。
        message: String,
    },
}

impl OsMcpError {
    /// 构造一个 JSON-RPC「方法不存在」错误（code -32601）。
    pub fn method_not_found(method: impl Into<String>) -> Self {
        Self::JsonRpc {
            code: -32601,
            message: format!("方法不存在: {}", method.into()),
        }
    }

    /// 构造一个 JSON-RPC「参数无效」错误（code -32602）。
    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self::JsonRpc {
            code: -32602,
            message: msg.into(),
        }
    }

    /// 构造一个 JSON-RPC「内部错误」（code -32603）。
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::JsonRpc {
            code: -32603,
            message: msg.into(),
        }
    }
}
