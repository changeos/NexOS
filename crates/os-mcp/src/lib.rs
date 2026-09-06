//! os-mcp——OS MCP Server，把 os-api HTTP 网关暴露为 MCP tools。
//!
//! 定位（规划文档 §3.x）：让支持 MCP（Model Context Protocol）的 AI 助手
//! （Claude / ChatGPT 等支持 MCP 的客户端）通过 `tools/list` + `tools/call`
//! 管理 OS——用户对 AI 说「看看 OS 有几个池」，AI 调 `os_pool_list` tool →
//! os-mcp 经 reqwest GET `os-api /api/v1/pools` → 返回真实 zfs 池数据。
//!
//! # 协议
//!
//! MCP = JSON-RPC 2.0 over stdio。本 server 进程被 MCP 客户端作为子进程拉起，
//! 客户端经 stdin 写请求、从 stdout 读响应（每行一条 JSON-RPC 消息）。
//!
//! 支持方法：
//! - `initialize`：握手，返回 server info + tools capability。
//! - `tools/list`：返回 10 个 OS tool（name + description + inputSchema）。
//! - `tools/call`：调一个 tool（→ GET os-api 路由 → JSON 返回）。
//! - `ping`：MCP 心跳，返回空 result。
//!
//! # 模块
//!
//! - [`tools`]：MCP tools 注册表（表驱动，10 个无参只读 GET tool）。
//! - [`api`]：os-api HTTP 客户端（reqwest + 可注入的 HttpBackend trait）。
//! - [`jsonrpc`]：JSON-RPC 2.0 协议层（请求/响应模型 + 方法分发，纯函数可单测）。
//! - [`server`]：stdio 循环（按行读 stdin → 分发 → 写 stdout）。
//! - [`error`]：统一错误类型。
//!
//! # 实现说明
//!
//! 默认走**手写最小 JSON-RPC over stdio**（[`server::serve_stdio`]），而非 rmcp
//! 官方 transport。理由：
//! 1. MCP 协议本质就是 JSON-RPC 2.0 over stdio，手写不复杂；
//! 2. 手写层把协议（[`jsonrpc`]）与 IO（[`server`]）解耦，[`jsonrpc::handle_request`]
//!    是纯函数，单测可不碰 IO 直接断言响应；
//! 3. 10 个 tool 全是无参 GET→JSON，表驱动（[`tools::all_tools`]）与 rmcp 的
//!    `#[tool]` 宏（要求每个 tool 是静态方法）设计冲突。
//!
//! rmcp 仍注册到 workspace deps（`rmcp.workspace = true`，`rmcp-transport` feature
//! 开启后链入），作为「官方 transport 接通」的可选入口，保持与官方 SDK 的兼容路径。

pub mod api;
pub mod error;
pub mod jsonrpc;
pub mod server;
pub mod tools;

pub use api::{HttpBackend, OsApiClient, ReqwestBackend};
pub use error::OsMcpError;
pub use jsonrpc::{error_code, handle_request, ErrorData, Request, Response};
pub use server::{handle_line, serve_stdio};
pub use tools::{all_tools, build_url, find_tool, OsTool, ALL_TOOLS};
