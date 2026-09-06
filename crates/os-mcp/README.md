# os-mcp

> MCP Server（终端 binary crate）· JSON-RPC 2.0 over stdio · 运行期经 HTTP 对接 os-api

OS MCP Server：把 os-api HTTP 网关暴露为 MCP（Model Context Protocol）tools，
让支持 MCP 的 AI 助手（Claude / ChatGPT 等）通过 `tools/list` + `tools/call`
直接管理 OS——用户说「看看 OS 有几个池」，AI 调 `os_pool_list` → 本 server 经
reqwest GET `os-api /api/v1/pools` → 返回真实 zfs 池数据。

## 核心能力

- **MCP 协议层**（`jsonrpc`）：JSON-RPC 2.0 请求/响应模型 + 方法分发
  （`handle_request` 纯函数，单测不碰 IO）。支持方法：`initialize`（握手 +
  capabilities）、`tools/list`、`tools/call`、`ping`。
- **tools 注册表**（`tools`）：表驱动 10 个无参只读 GET tool——`os_status` /
  `os_pool_list` / `os_dataset_list` / `os_snapshot_list` / `os_vm_list` /
  `os_share_list` / `os_user_list` / `os_node_list` / `os_virt_check` / `os_health`。
- **os-api 客户端**（`api`）：`OsApiClient` + 可注入 `HttpBackend` trait
  （默认 `ReqwestBackend`，rustls-tls 与 os-cli 共栈）。
- **stdio 循环**（`server`）：按行读 stdin → 分发 → 写 stdout（每行一条
  JSON-RPC 消息），`serve_stdio` 供 MCP 客户端作为子进程拉起。
- **可切换官方 transport**：`rmcp-transport` feature 链入 rmcp（Anthropic 官方
  Rust SDK）`ServiceExt::serve(stdio())`；默认手写实现（表驱动与 rmcp `#[tool]`
  宏的静态方法设计冲突，详见 lib.rs 实现说明）。

## 架构位置

**依赖**（上游）：**无内部 crate 依赖**——与 os-api 完全解耦，运行期经 HTTP 调用
（`--api-url`，env `OS_API_URL`，默认 `http://127.0.0.1:8080`）；第三方 reqwest /
serde / serde_json / tokio /（可选）rmcp。

**被用**（下游）：无——终端 binary（`[[bin]] os-mcp`），被 MCP 客户端拉起。

## 独立使用

- **仓库外引用 / 运行**：`cargo run -p os-mcp -- --api-url http://<os-host>:8080`；
  MCP 客户端配置中把本二进制注册为 stdio server 即可。
- **关键接口**（lib 使用方）：
  - `jsonrpc::handle_request`：纯函数协议分发，可脱离 IO 断言响应。
  - `api::HttpBackend`：注入 fake backend 单测，不起真实 HTTP。
  - `tools::all_tools / find_tool / build_url`：tool 元数据与 URL 构造。
- **feature**：`rmcp-transport`（默认关）——切换到 rmcp 官方 transport。

## 测试

```bash
cargo test -p os-mcp
```

JSON-RPC 协议往返纯函数测（`jsonrpc`）+ 10 tool 注册表/URL 构造表驱动测
（`tools`）+ fake `HttpBackend` 下的客户端调用分发测（`api`，不起真实 HTTP）。
