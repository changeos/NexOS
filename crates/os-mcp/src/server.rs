//! MCP stdio server 循环——从 stdin 按行读 JSON-RPC → 分发 → 往 stdout 写响应。
//!
//! 设计：
//! - 每行一条 JSON-RPC 消息（MCP 客户端约定：newline-delimited JSON）。
//! - stdin EOF 触发退出（客户端关闭管道即结束）。
//! - stdout 写每条响应后 flush（确保客户端立刻收到，无缓冲延迟）。
//! - stderr 仅供诊断日志（不参与 MCP 协议，客户端按协议只读 stdout）。
//!
//! IO 与协议解耦：协议层在 [`jsonrpc::handle_request`](crate::jsonrpc::handle_request)（纯函数，可单测），
//! 本模块只做「按行读 → 解析 → 分发 → 序列化 → 写」。`serve_stdio` 是对前者的
//! IO 包装；单测用 `handle_line` 直接验证单行处理（不碰真实 stdin/stdout）。

use crate::api::{HttpBackend, OsApiClient};
use crate::jsonrpc::{handle_request, parse_error_response, response_to_line, Request, Response};
use serde_json;

/// 处理一行输入：解析为 Request → 分发 → 返回应写出 stdout 的响应行（若应回响应）。
///
/// 返回 `None`：该行是通知（无 id）或合法但无需响应（不应出现）。
/// 返回 `Some(line)`：序列化好的单行 JSON 响应。
/// 输入是空行：返回 `None`（忽略）。
/// 输入不是合法 JSON-RPC：返回 parse error 响应（id=null）。
///
/// 单测直接调本函数，不碰真实 IO。
pub async fn handle_line<B: HttpBackend>(line: &str, client: &OsApiClient<B>) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    // 解析为 Request；非法 JSON → parse error 响应（id=null）。
    let req: Request = match serde_json::from_str(trimmed) {
        Ok(r) => r,
        Err(_) => return Some(parse_error_response()),
    };
    // 分发（可能返回 None = 通知）。
    let resp: Response = handle_request(&req, client).await?;
    Some(response_to_line(&resp))
}

/// 运行 stdio MCP server：阻塞读 stdin 直到 EOF，每行分发，响应写 stdout。
///
/// 生产入口（binary main 调用）。`client` 持有 os-api base URL + HTTP 后端。
/// 返回 Ok(()) 当 stdin EOF（客户端关闭）；返回 Err 当 stdout flush 失败（管道断开）。
pub async fn serve_stdio<B: HttpBackend>(client: OsApiClient<B>) -> std::io::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    // 按行读 stdin；EOF（None）则退出循环。
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(resp_line) = handle_line(&line, &client).await {
            stdout.write_all(resp_line.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}
