//! clap 命令定义 + 子命令执行（接通真实命令解析，规划文档 §3.0/#19）。
//!
//! 本模块把 os-cli 的命令行入口从「自实现 `parse_args`」升级到「真实 clap derive」，
//! 并把解析后的子命令路由到经网关 REST 调用 os-api 的真实执行路径。
//!
//! ## 设计要点
//!
//! - **clap derive 顶层** [`Cli`]：暴露全局选项 `--server URL` / `--output FORMAT`
//!   (`text`/`json`/`yaml`) / `--token TOKEN`，以及子命令枚举 [`CliCommand`]。
//! - **子命令**：覆盖核心运维动作（`status` / `pool list` / `pool create` /
//!   `vm list` / `vm create` / `share list` / `user list` / `discover`）——这些是
//!   「查询/创建」型骨架命令；每条子命令构造对应的网关请求并由 runner 执行。
//! - **执行抽象 [`CommandRunner`]**：持有 `Arc<dyn HttpTransport>`
//!   （`#[async_trait]`，dyn 兼容——见 ADR-COMPAT-001），生产用 `ReqwestTransport`
//!   （reqwest + rustls），测试注入内存 `FakeTransport`（离线 fixture，不真发网络请求）。
//!   runner 调用 `transport.send(base, req)` 直发请求，把响应解析为 `CommandOutput`，
//!   再经 [`format_output`] 渲染为目标格式。
//!   - `status` / `discover` 复用 `os_mobile::OsClient`（HttpOsClient，含会话/重试编排），
//!     以验证「CLI 复用客户端契约」。
//!   - `pool` / `vm` / `share` / `user` 直接构造 `RequestSpec` 经 transport 发送
//!     （OsClient trait 未覆盖这些资源，故走通用 GET/POST）。
//! - **红线**：不真连 OS——真实网络测均 `#[ignore]`（默认不跑），
//!   fixture 测用 `FakeTransport` 离线回放，确定性、CI 友好。
//!
//! ## 与既有契约的关系
//!
//! - **不修改** `Command` / `OutputFormatter` trait 签名（红线）。
//! - `parse_args`（自实现解析器）保留：既有下游（`command_tree` 测试）仍依赖它，
//!   且 clap 路径与之并行，互不干扰。
//! - [`Cli::format`] 返回 [`OutputFormat`]（复用契约层的枚举），驱动
//!   [`format_output`] 选择 `Text`/`Json`/`Yaml` 渲染器。

use std::sync::Arc;

use clap::{Parser, Subcommand};

use os_mobile::client::SystemStatus;
use os_mobile::http::{JsonResponse, RequestSpec};
use os_mobile::retry::RetryableError;
use os_mobile::transport::HttpTransport;
use os_mobile::{HttpOsClient, OsClient};

use crate::command::{CommandContext, CommandOutput, OutputFormat};
use crate::command_tree::format_output;
use crate::error::CliError;

// ----------------------------------------------------------------------------
// clap derive：顶层 Cli + 子命令枚举
// ----------------------------------------------------------------------------

/// os-cli 顶层命令（clap derive）。
///
/// 全局选项可在任意子命令前/后出现（clap `global = true`），由 [`Cli::build_context`]
/// 折叠为 [`CommandContext`]（复用契约层的上下文结构）。
#[derive(Debug, Clone, Parser)]
#[command(
    name = "os-cli",
    version,
    about = "OS 管理命令行：连接 os-api 网关执行运维操作（status/pool/vm/share/user）",
    long_about = "经网关 REST 调用 os-api（rustls-tls）。真实网络测 #[ignore]；默认 fixture 测离线。"
)]
pub struct Cli {
    /// os-api 网关端点（如 `https://os.local:8443`）。
    ///
    /// 缺省时由子命令按需报错（多数子命令需要远端，少数如本地 help 不需要）。
    #[arg(long, global = true, value_name = "URL", env = "OS_SERVER")]
    pub server: Option<String>,

    /// 输出格式：`text`（默认，人读）/ `json`（脚本）/ `yaml`（配置）。
    #[arg(long, global = true, value_name = "FORMAT", default_value = "text")]
    pub output: OutputFormatArg,

    /// 认证 token（Bearer）。缺省进入匿名会话（部分接口会 401）。
    #[arg(long, global = true, value_name = "TOKEN", env = "OS_TOKEN")]
    pub token: Option<String>,

    /// 子命令。
    #[command(subcommand)]
    pub command: CliCommand,
}

/// 输出格式命令行参数（独立 enum，便于 clap derive + 复用契约层 [`OutputFormat`]）。
///
/// 序列化与 [`OutputFormat`] 一致（snake_case）。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormatArg {
    /// 纯文本（表格/对齐，便于人读）
    Text,
    /// JSON
    Json,
    /// YAML
    Yaml,
}

impl From<OutputFormatArg> for OutputFormat {
    fn from(arg: OutputFormatArg) -> Self {
        match arg {
            OutputFormatArg::Text => OutputFormat::Text,
            OutputFormatArg::Json => OutputFormat::Json,
            OutputFormatArg::Yaml => OutputFormat::Yaml,
        }
    }
}

/// 子命令骨架（运维动作）。
#[derive(Debug, Clone, Subcommand)]
pub enum CliCommand {
    /// 查询 OS 系统状态（GET /status）。
    Status,
    /// 列出/创建存储池（GET/POST /api/v1/pools）。
    #[command(name = "pool")]
    Pool {
        #[command(subcommand)]
        action: PoolAction,
    },
    /// 列出/创建虚拟机（GET/POST /api/v1/vms）。
    #[command(name = "vm")]
    Vm {
        #[command(subcommand)]
        action: VmAction,
    },
    /// 列出共享（GET /shares）。
    #[command(name = "share")]
    Share {
        #[command(subcommand)]
        action: ShareAction,
    },
    /// 列出用户（GET /api/v1/users）。
    #[command(name = "user")]
    User {
        #[command(subcommand)]
        action: UserAction,
    },
    /// 发现局域网内 OS 节点（GET /discover/nodes）。
    Discover,
}

/// `pool` 子命令动作。
#[derive(Debug, Clone, Subcommand)]
pub enum PoolAction {
    /// 列出全部存储池。
    List,
    /// 创建存储池（POST /api/v1/pools，body 含名字）。
    Create {
        /// 池名（如 `tank`）。
        name: String,
    },
}

/// `vm` 子命令动作。
#[derive(Debug, Clone, Subcommand)]
pub enum VmAction {
    /// 列出全部虚拟机。
    List,
    /// 创建虚拟机（POST /api/v1/vms，body 含名字）。
    Create {
        /// VM 名（如 `vm-1`）。
        name: String,
    },
}

/// `share` 子命令动作。
#[derive(Debug, Clone, Subcommand)]
pub enum ShareAction {
    /// 列出全部共享。
    List,
}

/// `user` 子命令动作。
#[derive(Debug, Clone, Subcommand)]
pub enum UserAction {
    /// 列出全部用户。
    List,
}

// ----------------------------------------------------------------------------
// Cli 便捷方法：折叠全局选项为 CommandContext
// ----------------------------------------------------------------------------

impl Cli {
    /// 把全局选项折叠为 [`CommandContext`]（复用契约层结构，供下游格式化器消费）。
    ///
    /// `--server None` → `api_endpoint: None`（本地直调语义；子命令执行时若需要
    /// 远端会自行报错）。
    #[must_use]
    pub fn build_context(&self) -> CommandContext {
        CommandContext {
            api_endpoint: self.server.clone(),
            token: self.token.clone(),
            format: self.output.into(),
        }
    }

    /// 解析后的输出格式（折叠为契约层枚举）。
    #[must_use]
    pub fn format(&self) -> OutputFormat {
        self.output.into()
    }
}

// ----------------------------------------------------------------------------
// CommandRunner：注入 HttpTransport，执行子命令并产出渲染后的字符串
// ----------------------------------------------------------------------------

/// 子命令执行器——持有 HTTP 传输（生产 reqwest / 测试 FakeTransport），路由子命令到
/// 网关 REST 调用。
///
/// 设计：
/// - **注入 `Arc<dyn HttpTransport>`**（`#[async_trait]`，dyn 兼容）而非 `Arc<dyn OsClient>`
///   ——`OsClient` 是原生 `async fn in trait`（不可 dyn，见 ADR-COMPAT-001），
///   `HttpTransport` 才是 dyn 友好的注入点（与 os-mobile 自身策略一致）。
/// - `status` / `discover`：构造 `HttpOsClient::with_transport`，`connect` 后调
///   `OsClient::get_system_status` / `discover_nodes`（复用客户端契约 + 重试编排）。
/// - `pool` / `vm` / `share` / `user`：直接 `transport.send`（OsClient trait 未覆盖
///   这些资源，走通用 GET/POST）。
/// - 返回 [`CommandOutput`]（结构化），由调用方经 [`format_output`] 渲染。
pub struct CommandRunner {
    transport: Arc<dyn HttpTransport>,
}

impl CommandRunner {
    /// 用注入的传输构造（测试入口：传 FakeTransport）。
    #[must_use]
    pub fn with_transport(transport: Arc<dyn HttpTransport>) -> Self {
        Self { transport }
    }

    /// 用生产默认 `ReqwestTransport`（reqwest + rustls）构造。
    ///
    /// 失败仅 reqwest::Client 构造失败（TLS 后端初始化，极少见）。
    pub fn new() -> Result<Self, CliError> {
        let transport = Arc::new(
            os_mobile::transport::ReqwestTransport::new()
                .map_err(|e| CliError::Internal(format!("reqwest 构造失败: {e}")))?,
        );
        Ok(Self::with_transport(transport))
    }
}

impl Default for CommandRunner {
    fn default() -> Self {
        Self::new().expect("CommandRunner 默认构造不应失败（reqwest::Client 默认可用）")
    }
}

impl CommandRunner {
    /// 执行已解析的 [`Cli`]，返回渲染后的输出字符串。
    ///
    /// 流程：
    /// 1. 取 `--server`（多数子命令必需，缺则报 `InvalidArgs`）。
    /// 2. 取 `--token`（可选，匿名为 None）。
    /// 3. 按 `Cli::command` 分发到具体子命令 handler，产出 `CommandOutput`。
    /// 4. 用 `Cli::format()` 选格式化器渲染。
    pub async fn run(&self, cli: &Cli) -> Result<String, CliError> {
        let out = self.dispatch(cli).await?;
        format_output(&out, cli.format())
    }

    /// 分发到具体子命令 handler，产出原始 [`CommandOutput`]（未格式化）。
    ///
    /// 暴露为 pub 便于测试断言结构化字段（不经格式化器）。
    pub async fn dispatch(&self, cli: &Cli) -> Result<CommandOutput, CliError> {
        let server = cli.server.as_deref().ok_or_else(|| {
            CliError::InvalidArgs("缺少 --server <URL>（多数子命令需要远端网关）".to_string())
        })?;
        let token = cli.token.as_deref();
        match &cli.command {
            CliCommand::Status => self.run_status(server, token).await,
            CliCommand::Pool { action } => self.run_pool(server, token, action).await,
            CliCommand::Vm { action } => self.run_vm(server, token, action).await,
            CliCommand::Share { action } => self.run_share(server, token, action).await,
            CliCommand::User { action } => self.run_user(server, token, action).await,
            CliCommand::Discover => self.run_discover(server, token).await,
        }
    }

    // —— status / discover：复用 OsClient（HttpOsClient）——

    async fn run_status(
        &self,
        server: &str,
        token: Option<&str>,
    ) -> Result<CommandOutput, CliError> {
        let client = HttpOsClient::with_transport(self.transport.clone());
        client
            .connect(server, token)
            .await
            .map_err(map_mobile_error)?;
        let status = client.get_system_status().await.map_err(map_mobile_error)?;
        Ok(status_to_output(status))
    }

    async fn run_discover(
        &self,
        server: &str,
        token: Option<&str>,
    ) -> Result<CommandOutput, CliError> {
        let client = HttpOsClient::with_transport(self.transport.clone());
        client
            .connect(server, token)
            .await
            .map_err(map_mobile_error)?;
        let nodes = client.discover_nodes().await.map_err(map_mobile_error)?;
        Ok(ok_output(
            serde_json::to_value(&nodes)
                .map_err(|e| CliError::OutputFailed(format!("序列化失败: {e}")))?,
            Some(format!("发现 {} 个节点", nodes.len())),
        ))
    }

    // —— pool / vm / share / user：通用 GET/POST 经 transport 直发 ——

    async fn run_pool(
        &self,
        server: &str,
        token: Option<&str>,
        action: &PoolAction,
    ) -> Result<CommandOutput, CliError> {
        match action {
            PoolAction::List => {
                let req = authed(RequestSpec::get("/api/v1/pools"), token);
                let resp = self.send(server, &req).await?;
                list_output(resp, "存储池")
            }
            PoolAction::Create { name } => {
                #[derive(serde::Serialize)]
                struct Body<'a> {
                    name: &'a str,
                }
                let req = RequestSpec::post_json("/api/v1/pools", &Body { name })
                    .map_err(|e| CliError::Internal(format!("请求构造失败: {e}")))?;
                let req = authed(req, token);
                let resp = self.send(server, &req).await?;
                create_output(resp, "存储池", name)
            }
        }
    }

    async fn run_vm(
        &self,
        server: &str,
        token: Option<&str>,
        action: &VmAction,
    ) -> Result<CommandOutput, CliError> {
        match action {
            VmAction::List => {
                let req = authed(RequestSpec::get("/api/v1/vms"), token);
                let resp = self.send(server, &req).await?;
                list_output(resp, "虚拟机")
            }
            VmAction::Create { name } => {
                #[derive(serde::Serialize)]
                struct Body<'a> {
                    name: &'a str,
                }
                let req = RequestSpec::post_json("/api/v1/vms", &Body { name })
                    .map_err(|e| CliError::Internal(format!("请求构造失败: {e}")))?;
                let req = authed(req, token);
                let resp = self.send(server, &req).await?;
                create_output(resp, "虚拟机", name)
            }
        }
    }

    async fn run_share(
        &self,
        server: &str,
        token: Option<&str>,
        action: &ShareAction,
    ) -> Result<CommandOutput, CliError> {
        match action {
            ShareAction::List => {
                let req = authed(RequestSpec::get("/shares"), token);
                let resp = self.send(server, &req).await?;
                list_output(resp, "共享")
            }
        }
    }

    async fn run_user(
        &self,
        server: &str,
        token: Option<&str>,
        action: &UserAction,
    ) -> Result<CommandOutput, CliError> {
        match action {
            UserAction::List => {
                let req = authed(RequestSpec::get("/api/v1/users"), token);
                let resp = self.send(server, &req).await?;
                list_output(resp, "用户")
            }
        }
    }

    /// 经注入的 transport 直发一次请求（带 base_url），返回 `JsonResponse`。
    async fn send(&self, base_url: &str, req: &RequestSpec) -> Result<JsonResponse, CliError> {
        self.transport
            .send(base_url, req)
            .await
            .map_err(|e| map_transport_error(e.message, e.kind))
    }
}

// ----------------------------------------------------------------------------
// 内部工具：错误映射 + 输出构造 + 请求装饰
// ----------------------------------------------------------------------------

/// 给请求注入 Authorization 头（token=Some 时；None 不动）。
fn authed(mut req: RequestSpec, token: Option<&str>) -> RequestSpec {
    if let Some(t) = token {
        req = req.with_header("Authorization", format!("Bearer {t}"));
    }
    req
}

/// 把 `os-mobile` 的 `MobileError` 映射到 [`CliError`]。
fn map_mobile_error(e: os_mobile::MobileError) -> CliError {
    use os_mobile::MobileError;
    match e {
        MobileError::NotConnected => {
            CliError::InvalidArgs("未连接 os-api（需 --server）".to_string())
        }
        MobileError::EndpointUnreachable(m) => {
            // 鉴权失败（401/403）也归 EndpointUnreachable（os-mobile 无独立 AuthFailed）
            if m.contains("鉴权失败") {
                CliError::AuthFailed(m)
            } else {
                CliError::ApiConnectionFailed(m)
            }
        }
        other => CliError::Internal(other.to_string()),
    }
}

/// 把 `TransportError`（带分类）映射到 [`CliError`]。
fn map_transport_error(message: String, kind: RetryableError) -> CliError {
    match kind {
        RetryableError::ClientStatus(401) | RetryableError::ClientStatus(403) => {
            CliError::AuthFailed(message)
        }
        RetryableError::Connect | RetryableError::Dns | RetryableError::Timeout => {
            CliError::ApiConnectionFailed(message)
        }
        _ => CliError::ApiConnectionFailed(message),
    }
}

/// 构造成功 `CommandOutput`。
fn ok_output(data: serde_json::Value, message: Option<String>) -> CommandOutput {
    CommandOutput {
        success: true,
        data,
        message,
    }
}

/// 数组 data 的元素数；非数组返回 0（用于「N 个池/VM」消息）。
fn count_items(data: &serde_json::Value) -> usize {
    data.as_array().map_or(0, std::vec::Vec::len)
}

/// 把 list 响应解析为 `CommandOutput`（data=数组，message 含计数）。
fn list_output(resp: JsonResponse, what: &str) -> Result<CommandOutput, CliError> {
    let body = serde_json::from_slice::<serde_json::Value>(&resp.body)
        .map_err(|e| CliError::OutputFailed(format!("响应解析失败: {e}")))?;
    let count = count_items(&body);
    Ok(ok_output(body, Some(format!("{count} 个{what}"))))
}

/// 把 create 响应解析为 `CommandOutput`（data=响应体，message 含名字）。
fn create_output(resp: JsonResponse, what: &str, name: &str) -> Result<CommandOutput, CliError> {
    let body = serde_json::from_slice::<serde_json::Value>(&resp.body)
        .map_err(|e| CliError::OutputFailed(format!("响应解析失败: {e}")))?;
    Ok(ok_output(body, Some(format!("已创建{what}: {name}"))))
}

/// 把 `SystemStatus` 序列化为 `CommandOutput`（data 为对象，message 为概要）。
fn status_to_output(status: SystemStatus) -> CommandOutput {
    let data = serde_json::to_value(&status).unwrap_or(serde_json::Value::Null);
    let message = format!(
        "{} @ v{} ({} 节点，{:?})",
        status.hostname, status.version, status.node_count, status.health
    );
    ok_output(data, Some(message))
}

// ----------------------------------------------------------------------------
// 单元测——clap 解析 + 子命令路由（fixture：FakeTransport，离线确定性）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use os_mobile::http::{HttpMethod, JsonResponse, RequestSpec};
    use os_mobile::transport::{HttpTransport, TransportError, TransportResult};
    use std::sync::Mutex;

    // ============================================================
    // FakeTransport——离线 fixture，可编程回放响应（确定性，不真发网络）
    // ============================================================

    struct FakeTransport {
        script: Mutex<Vec<TransportResult>>,
    }

    impl FakeTransport {
        fn new(script: Vec<TransportResult>) -> Self {
            Self {
                script: Mutex::new(script),
            }
        }
    }

    #[async_trait]
    impl HttpTransport for FakeTransport {
        async fn send(&self, _base_url: &str, _req: &RequestSpec) -> TransportResult {
            let mut s = self.script.lock().unwrap();
            if s.is_empty() {
                return Err(TransportError::new(
                    "FakeTransport 脚本耗尽",
                    RetryableError::ClientStatus(0),
                ));
            }
            let item = s.drain(0..1).next().unwrap();
            item
        }
    }

    fn runner(script: Vec<TransportResult>) -> CommandRunner {
        let t: Arc<dyn HttpTransport> = Arc::new(FakeTransport::new(script));
        CommandRunner::with_transport(t)
    }

    fn status_resp() -> JsonResponse {
        JsonResponse::new(
            200,
            br#"{"hostname":"os-1","version":"2.0.0","capacity":{"used_bytes":10,"total_bytes":100},"health":"healthy","node_count":3}"#.to_vec(),
        )
    }

    fn list_resp() -> JsonResponse {
        JsonResponse::new(200, br#"[{"id":"a"},{"id":"b"}]"#.to_vec())
    }

    fn create_resp() -> JsonResponse {
        JsonResponse::new(201, br#"{"id":"new-1","created":true}"#.to_vec())
    }

    fn empty_arr_resp() -> JsonResponse {
        JsonResponse::new(200, br#"[]"#.to_vec())
    }

    // —— clap 解析测 ——

    #[test]
    fn clap_parses_status_with_global_server_and_output() {
        let cli = Cli::try_parse_from([
            "os-cli",
            "--server",
            "https://os:8443",
            "--output",
            "json",
            "status",
        ])
        .unwrap();
        assert_eq!(cli.server.as_deref(), Some("https://os:8443"));
        assert_eq!(cli.output, OutputFormatArg::Json);
        assert!(matches!(cli.command, CliCommand::Status));
    }

    #[test]
    fn clap_parses_pool_create_with_positional_name() {
        let cli =
            Cli::try_parse_from(["os-cli", "--server", "https://os", "pool", "create", "tank"])
                .unwrap();
        match &cli.command {
            CliCommand::Pool { action } => match action {
                PoolAction::Create { name } => assert_eq!(name, "tank"),
                other => panic!("期望 PoolAction::Create，got {other:?}"),
            },
            other => panic!("期望 CliCommand::Pool，got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_vm_list_default_text_output() {
        let cli = Cli::try_parse_from(["os-cli", "--server", "https://os", "vm", "list"]).unwrap();
        assert_eq!(cli.output, OutputFormatArg::Text);
        assert!(matches!(
            &cli.command,
            CliCommand::Vm {
                action: VmAction::List
            }
        ));
    }

    #[test]
    fn clap_parses_share_and_user_list() {
        let cli =
            Cli::try_parse_from(["os-cli", "--server", "https://os", "share", "list"]).unwrap();
        assert!(matches!(
            &cli.command,
            CliCommand::Share {
                action: ShareAction::List
            }
        ));
        let cli =
            Cli::try_parse_from(["os-cli", "--server", "https://os", "user", "list"]).unwrap();
        assert!(matches!(
            &cli.command,
            CliCommand::User {
                action: UserAction::List
            }
        ));
    }

    #[test]
    fn clap_parses_token_flag() {
        let cli = Cli::try_parse_from([
            "os-cli",
            "--server",
            "https://os",
            "--token",
            "abc",
            "status",
        ])
        .unwrap();
        assert_eq!(cli.token.as_deref(), Some("abc"));
    }

    #[test]
    fn clap_parses_global_options_after_subcommand() {
        // clap global 允许全局选项出现在子命令之后
        let cli = Cli::try_parse_from([
            "os-cli",
            "status",
            "--server",
            "https://os",
            "--output",
            "yaml",
        ])
        .unwrap();
        assert_eq!(cli.server.as_deref(), Some("https://os"));
        assert_eq!(cli.output, OutputFormatArg::Yaml);
    }

    #[test]
    fn clap_rejects_unknown_subcommand() {
        let r = Cli::try_parse_from(["os-cli", "--server", "https://os", "bogus"]);
        assert!(r.is_err(), "未知子命令应被 clap 拒绝");
    }

    #[test]
    fn clap_rejects_pool_create_without_name() {
        // create 需要必填位置参数 name
        let r = Cli::try_parse_from(["os-cli", "--server", "https://os", "pool", "create"]);
        assert!(r.is_err());
    }

    #[test]
    fn build_context_folds_globals() {
        let cli = Cli::try_parse_from([
            "os-cli",
            "--server",
            "https://os",
            "--output",
            "yaml",
            "--token",
            "t",
            "status",
        ])
        .unwrap();
        let ctx = cli.build_context();
        assert_eq!(ctx.api_endpoint.as_deref(), Some("https://os"));
        assert_eq!(ctx.token.as_deref(), Some("t"));
        assert_eq!(ctx.format, OutputFormat::Yaml);
    }

    // —— 子命令执行测（fixture：FakeTransport 离线回放）——

    #[tokio::test]
    async fn status_command_renders_json() {
        let r = runner(vec![Ok(status_resp())]);
        let cli = Cli::try_parse_from([
            "os-cli",
            "--server",
            "https://os",
            "--output",
            "json",
            "status",
        ])
        .unwrap();
        let out = r.run(&cli).await.unwrap();
        // JSON envelope 含 success=true + data.hostname
        assert!(out.contains("\"success\":true"));
        assert!(out.contains("os-1"));
    }

    #[tokio::test]
    async fn status_command_renders_text_with_summary() {
        let r = runner(vec![Ok(status_resp())]);
        let cli = Cli::try_parse_from(["os-cli", "--server", "https://os", "status"]).unwrap();
        let out = r.run(&cli).await.unwrap();
        assert!(out.contains("os-1"));
        assert!(out.contains("2.0.0"));
    }

    #[tokio::test]
    async fn missing_server_errors() {
        let r = runner(vec![]);
        let cli = Cli::try_parse_from(["os-cli", "status"]).unwrap();
        let err = r.run(&cli).await.unwrap_err();
        assert!(matches!(err, CliError::InvalidArgs(_)));
    }

    #[tokio::test]
    async fn auth_error_maps_to_auth_failed() {
        // status 走 OsClient→HttpOsClient→transport；401 经 mobile 错误映射含「鉴权失败」
        let r = runner(vec![Err(TransportError::new(
            "denied",
            RetryableError::ClientStatus(401),
        ))]);
        let cli = Cli::try_parse_from(["os-cli", "--server", "https://os", "status"]).unwrap();
        let err = r.run(&cli).await.unwrap_err();
        assert!(matches!(err, CliError::AuthFailed(_)));
    }

    #[tokio::test]
    async fn pool_list_returns_count() {
        let r = runner(vec![Ok(list_resp())]);
        let cli =
            Cli::try_parse_from(["os-cli", "--server", "https://os", "pool", "list"]).unwrap();
        let out = r.dispatch(&cli).await.unwrap();
        assert!(out.success);
        assert_eq!(out.message.as_deref(), Some("2 个存储池"));
    }

    #[tokio::test]
    async fn pool_create_includes_name_in_message() {
        let r = runner(vec![Ok(create_resp())]);
        let cli =
            Cli::try_parse_from(["os-cli", "--server", "https://os", "pool", "create", "tank"])
                .unwrap();
        let out = r.dispatch(&cli).await.unwrap();
        assert!(out.message.as_deref().unwrap().contains("tank"));
        assert_eq!(out.data["id"], "new-1");
    }

    #[tokio::test]
    async fn vm_list_runs() {
        let r = runner(vec![Ok(list_resp())]);
        let cli = Cli::try_parse_from(["os-cli", "--server", "https://os", "vm", "list"]).unwrap();
        let out = r.dispatch(&cli).await.unwrap();
        assert_eq!(out.message.as_deref(), Some("2 个虚拟机"));
    }

    #[tokio::test]
    async fn vm_create_runs() {
        let r = runner(vec![Ok(create_resp())]);
        let cli = Cli::try_parse_from(["os-cli", "--server", "https://os", "vm", "create", "vm-1"])
            .unwrap();
        let out = r.dispatch(&cli).await.unwrap();
        assert!(out.message.as_deref().unwrap().contains("vm-1"));
    }

    #[tokio::test]
    async fn share_list_runs() {
        let r = runner(vec![Ok(empty_arr_resp())]);
        let cli =
            Cli::try_parse_from(["os-cli", "--server", "https://os", "share", "list"]).unwrap();
        let out = r.dispatch(&cli).await.unwrap();
        assert_eq!(out.message.as_deref(), Some("0 个共享"));
    }

    #[tokio::test]
    async fn user_list_runs() {
        let r = runner(vec![Ok(list_resp())]);
        let cli =
            Cli::try_parse_from(["os-cli", "--server", "https://os", "user", "list"]).unwrap();
        let out = r.dispatch(&cli).await.unwrap();
        assert_eq!(out.message.as_deref(), Some("2 个用户"));
    }

    #[tokio::test]
    async fn discover_runs_with_empty_list() {
        let r = runner(vec![Ok(empty_arr_resp())]);
        let cli = Cli::try_parse_from(["os-cli", "--server", "https://os", "discover"]).unwrap();
        let out = r.dispatch(&cli).await.unwrap();
        assert_eq!(out.message.as_deref(), Some("发现 0 个节点"));
    }

    #[tokio::test]
    async fn pool_list_403_maps_to_auth_failed() {
        // pool 走通用 transport.send → map_transport_error（403→AuthFailed）
        let r = runner(vec![Err(TransportError::new(
            "forbidden",
            RetryableError::ClientStatus(403),
        ))]);
        let cli =
            Cli::try_parse_from(["os-cli", "--server", "https://os", "pool", "list"]).unwrap();
        let err = r.dispatch(&cli).await.unwrap_err();
        assert!(matches!(err, CliError::AuthFailed(_)));
    }

    #[tokio::test]
    async fn pool_list_503_maps_to_api_connection_failed() {
        let r = runner(vec![Err(TransportError::new(
            "svr",
            RetryableError::ServerStatus(503),
        ))]);
        let cli =
            Cli::try_parse_from(["os-cli", "--server", "https://os", "pool", "list"]).unwrap();
        let err = r.dispatch(&cli).await.unwrap_err();
        assert!(matches!(err, CliError::ApiConnectionFailed(_)));
    }

    #[tokio::test]
    async fn token_adds_authorization_header() {
        // 通过 authed() 装饰验证 token 注入（不真发网络）
        let req = authed(RequestSpec::get("/x"), Some("tok123"));
        assert_eq!(req.headers.get("Authorization").unwrap(), "Bearer tok123");
        // None 不注入
        let req2 = authed(RequestSpec::get("/x"), None);
        assert!(!req2.headers.contains_key("Authorization"));
    }

    #[test]
    fn output_format_arg_to_output_format() {
        assert_eq!(
            OutputFormat::from(OutputFormatArg::Text),
            OutputFormat::Text
        );
        assert_eq!(
            OutputFormat::from(OutputFormatArg::Json),
            OutputFormat::Json
        );
        assert_eq!(
            OutputFormat::from(OutputFormatArg::Yaml),
            OutputFormat::Yaml
        );
    }

    #[test]
    fn count_items_handles_array_and_non_array() {
        assert_eq!(count_items(&serde_json::json!([1, 2, 3])), 3);
        assert_eq!(count_items(&serde_json::json!("x")), 0);
        assert_eq!(count_items(&serde_json::Value::Null), 0);
    }

    #[test]
    fn http_method_used() {
        assert_eq!(HttpMethod::Get.as_str(), "GET");
    }

    // —— 真实网络测（手动 --ignored；不进 CI）——

    #[tokio::test]
    #[ignore = "真连 OS（需本地起 os-api 网关）；手动 cargo test -- --ignored"]
    async fn real_status_against_local_gateway() {
        let r = CommandRunner::default();
        let cli =
            Cli::try_parse_from(["os-cli", "--server", "http://127.0.0.1:8080", "status"]).unwrap();
        let _ = r.run(&cli).await.expect("本地网关应可连");
    }
}
