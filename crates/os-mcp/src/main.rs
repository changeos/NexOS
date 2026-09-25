//! `os-mcp` binary 入口——MCP server 进程（stdio JSON-RPC 2.0）。
//!
//! 启动方式（被 MCP 客户端拉起）：
//! ```text
//! os-mcp --server http://127.0.0.1:8080
//! ```
//! MCP 客户端（Claude / ChatGPT 等支持 MCP 的客户端）把本进程作为子进程拉起，
//! 经 stdin 收 JSON-RPC 请求、从 stdout 读响应。
//!
//! # 命令行参数
//!
//! - `--server <URL>`：os-api HTTP 网关地址（默认 `http://127.0.0.1:8080`）。
//!   MCP tools 内部用 reqwest 调该地址的对应路由。
//! - `--token <TOKEN>`：系统 admin token（env `OS_ADMIN_TOKEN` / `NEXOS_ADMIN_TOKEN`
//!   回落）——NexHub 写工具（写文件 / 开 Issue / 建 PR / merge）经 os-api 网关
//!   鉴权必需；只读工具可不带。
//! - `--check`：预检模式——打印 tools 列表 + server URL，不进 stdio 循环
//!   （便于诊断 / 验证 tools 注册）。

use std::process::ExitCode;

use clap::Parser;
use os_mcp::{all_tools, serve_stdio, OsApiClient, ReqwestBackend};

/// os-mcp 命令行参数（clap derive）。
#[derive(Debug, Clone, Parser)]
#[command(
    name = "os-mcp",
    version,
    about = "OS MCP Server——把 OS API 暴露为 MCP tools（stdio JSON-RPC 2.0）",
    long_about = "MCP server：被支持 MCP 的 AI 客户端（Claude / ChatGPT）拉起，\
 经 stdin/stdout 收发 JSON-RPC 2.0 消息。tools/list 返回 18 个 tool（10 个 OS \
 管理 + 8 个 NexHub 仓库操作），tools/call 内部调 os-api 对应路由，返回真实数据。"
)]
struct Cli {
    /// os-api HTTP 网关地址（如 `http://127.0.0.1:8080`）。MCP tools 内部调此地址。
    #[arg(
        long,
        value_name = "URL",
        default_value = "http://127.0.0.1:8080",
        env = "OS_API_URL"
    )]
    server: String,

    /// 系统 admin token（NexHub 写工具经 os-api 网关鉴权必需；env OS_ADMIN_TOKEN）。
    #[arg(
        long,
        value_name = "TOKEN",
        env = "OS_ADMIN_TOKEN",
        hide_env_values = true
    )]
    token: Option<String>,

    /// 预检模式：打印 tools 列表 + server URL，不进 stdio 循环。
    #[arg(long)]
    check: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    // token 回落链：--token / OS_ADMIN_TOKEN → NEXOS_ADMIN_TOKEN（与 os-api 同名变量）
    let token = cli.token.clone().or_else(|| {
        std::env::var("NEXOS_ADMIN_TOKEN")
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
    });

    if cli.check {
        return run_check(&cli.server, token.as_deref());
    }

    // 构造 client（真实 reqwest backend + 可选 admin token）→ 进 stdio 循环。
    let has_token = token.is_some();
    let client = OsApiClient::new(cli.server.clone(), ReqwestBackend::new()).with_token(token);
    eprintln!(
        "[os-mcp] 启动 MCP server（stdio JSON-RPC 2.0），os-api @ {}（{} 个 tools{}）",
        client.base(),
        all_tools().len(),
        if has_token { "，admin token 已配置" } else { "，无 token（只读面）" }
    );

    match serve_stdio(client).await {
        Ok(()) => {
            eprintln!("[os-mcp] stdin EOF，退出");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("[os-mcp] stdio 循环 IO 错误：{e}");
            ExitCode::FAILURE
        }
    }
}

/// 预检模式：打印 tools + server URL，返回 SUCCESS（不进 stdio 循环）。
fn run_check(server_url: &str, token: Option<&str>) -> ExitCode {
    println!("[os-mcp] 预检模式");
    println!("[os-mcp]   os-api server: {server_url}");
    println!(
        "[os-mcp]   admin token: {}",
        if token.is_some() { "已配置" } else { "未配置（NexHub 写工具不可用）" }
    );
    println!("[os-mcp]   tools（{} 个）:", all_tools().len());
    for t in all_tools() {
        println!(
            "[os-mcp]     {:<20} {:<4} {}   {}",
            t.name, t.method, t.api_path, t.description
        );
    }
    println!(
        "[os-mcp] MCP 协议：initialize / tools/list / tools/call / ping（JSON-RPC 2.0 over stdio）"
    );
    ExitCode::SUCCESS
}
