//! `NexhubCliRouteHandler` —— nexhub CLI 分发端点（NexHub 网页/CLI 重排 P2，
//! docs/research/NEXHUB_WEB_CLI_DESIGN.md §B / §4.1）。
//!
//! 定位：让一台**没有任何预装**的机器用一条 curl 命令获得 `nexhub` CLI——
//! 脚本本体（POSIX sh，单文件自包含）由 [`NEXHUB_CLI_SCRIPT_TEMPLATE`]
//! （`include_str!` 资产 `assets/nexhub-cli.sh`，随二进制分发）按请求动态
//! 渲染，整份照搬 `provisioning.rs` install.sh 的成熟先例：
//!
//! 1. **Host 头推导节点地址**（[`node_base_url`]）：`X-Forwarded-Host` →
//!    `Host` → env `NEXOS_GIT_ADVERTISE_HOST` → `127.0.0.1:8558` 兜底；
//!    缺省端口 8558（无端口 Host 自动补齐）——任一节点都能当分发源，
//!    下载到的脚本缺省连回提供它的节点。
//! 2. **text 直传**：handler 返回 `body: Value::String` +
//!    `content-type: text/x-shellscript; charset=utf-8`，http.rs
//!    `direct_passthrough_bytes()` 对 `text/*` 原文直传（不走 JSON 信封），
//!    `curl | sh` 即可用；另附 `X-Content-Type-Options: nosniff` 防嗅探。
//! 3. **公开读**：路由声明 `requires_auth=false`（未登录机器可达）。
//!
//! 渲染占位符（均为脚本内单引号字面量赋值，值经 [`strip_single_quotes`]
//! 净化，同 `render_install_script` 先例）：
//!
//! - `@@NEXHUB_NODE@@`：缺省节点 base URL（登录凭据缺省值）
//! - `@@NEXHUB_CLI_URL@@`：cli.sh 自身 URL（安装模式重新下载 / self-update 用）
//! - `@@NEXHUB_VERSION@@`：脚本版本 = 运行二进制 `CARGO_PKG_VERSION`
//!
//! 脚本命令面（细节见 docs/NEXHUB.md）：`login / whoami / ping /
//! repo list|create|delete|info / clone / apps list|deploy|remove /
//! self-update / help`；token 经 `curl -H @file` 注入不进 argv；
//! curl 必须、jq 首选、python3 降级。

use async_trait::async_trait;

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// 常量
// ----------------------------------------------------------------------------

/// nexhub CLI 脚本资产（仓库源：`crates/os-api/src/assets/nexhub-cli.sh`）。
///
/// `@@NEXHUB_NODE@@` / `@@NEXHUB_CLI_URL@@` / `@@NEXHUB_VERSION@@` 三个占位符
/// 由 [`render_cli_script`] 替换；资产本体同样是合法 shell（占位符在单引号
/// 字面量内），可独立 `bash -n` 校验。
pub const NEXHUB_CLI_SCRIPT_TEMPLATE: &str = include_str!("../assets/nexhub-cli.sh");

/// os-api HTTP 端口缺省约定（生产 unit `--addr 0.0.0.0:8558`，与
/// `provisioning.rs` 的 install.sh 先例一致）。
pub const DEFAULT_API_PORT: u16 = 8558;

/// 路由路径（设计文档 §6.1 P2 唯一新增端点）。
pub const CLI_SCRIPT_PATH: &str = "/api/v1/coderepo/cli.sh";

// ----------------------------------------------------------------------------
// Host 推导与渲染（纯函数，易单测）
// ----------------------------------------------------------------------------

/// 从请求头取值（大小写不敏感；非字符串值忽略）。
fn header_value<'a>(req: &'a ApiRequest, name: &str) -> Option<&'a str> {
    if let serde_json::Value::Object(map) = &req.headers {
        if let Some((_, v)) = map.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
            if let Some(s) = v.as_str() {
                return Some(s);
            }
        }
    }
    None
}

/// host 规格化为 `http://<host>[:8558]` base URL：已带端口原样（IPv4/域名/
/// `[v6]:port` 形态；与 provisioning `source_base_url` 同一 `rfind(':')`
/// 判定惯例），无端口补缺省 8558。空值返回 None。
fn normalize_host_url(host: &str) -> Option<String> {
    let h = host.trim();
    if h.is_empty() {
        return None;
    }
    if h.rfind(':').is_some() {
        Some(format!("http://{h}"))
    } else {
        Some(format!("http://{h}:{DEFAULT_API_PORT}"))
    }
}

/// 由请求推导缺省节点 base URL（脚本 `NEXHUB_NODE_DEFAULT` / 安装源）：
///
/// `X-Forwarded-Host`（反代场景，逗号链取第一段）→ `Host` → env
/// `NEXOS_GIT_ADVERTISE_HOST` → `127.0.0.1:8558` 兜底。推导失败不硬错——
/// 脚本侧 `NEXHUB_NODE` env / `nexhub login <node-url>` 均可覆盖。
#[must_use]
pub fn node_base_url(req: &ApiRequest) -> String {
    for name in ["x-forwarded-host", "host"] {
        if let Some(v) = header_value(req, name) {
            let first = v.split(',').next().unwrap_or("").trim();
            if let Some(url) = normalize_host_url(first) {
                return url;
            }
        }
    }
    let advertise = std::env::var("NEXOS_GIT_ADVERTISE_HOST")
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    normalize_host_url(&advertise).unwrap_or_else(|| format!("http://127.0.0.1:{DEFAULT_API_PORT}"))
}

/// 把单引号从嵌入值中剥除（值只进脚本单引号字面量，防注入的最小处理；
/// URL 是机器形态，正常输入不含 `'`）。
fn strip_single_quotes(s: &str) -> String {
    s.replace('\'', "")
}

/// 渲染 nexhub CLI 脚本：烘焙缺省节点地址 / cli.sh 自身 URL / 版本号。
///
/// 版本取运行二进制 `CARGO_PKG_VERSION`（主代理统一 bump，脚本随二进制
/// 天然同版本）；`bash -n` 语法在测试与脚本 self-update/安装路径双重校验。
#[must_use]
pub fn render_cli_script(node_url: &str) -> String {
    let node = strip_single_quotes(node_url);
    let cli_url = format!("{node}/api/v1/coderepo/cli.sh");
    NEXHUB_CLI_SCRIPT_TEMPLATE
        .replace("@@NEXHUB_NODE@@", &node)
        .replace("@@NEXHUB_CLI_URL@@", &cli_url)
        .replace("@@NEXHUB_VERSION@@", env!("CARGO_PKG_VERSION"))
}

// ----------------------------------------------------------------------------
// Handler
// ----------------------------------------------------------------------------

/// nexhub CLI 分发路由处理器——单端点（GET cli.sh），无状态。
pub struct NexhubCliRouteHandler;

impl NexhubCliRouteHandler {
    /// 构造（无状态，无副作用）。
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for NexhubCliRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RouteHandler for NexhubCliRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![RouteSpec {
            method: HttpMethod::Get,
            path: CLI_SCRIPT_PATH.to_string(),
            handler_component: "nexhub_cli".to_string(),
            requires_auth: false,
            required_roles: vec![],
        }]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let script = render_cli_script(&node_base_url(&req));
        Ok(ApiResponse {
            status: 200,
            // Value::String + text/* → 网关直传通道按原文返回（非 JSON 信封）
            body: serde_json::Value::String(script),
            headers: serde_json::json!({
                "content-type": "text/x-shellscript; charset=utf-8",
                "x-content-type-options": "nosniff",
                "content-disposition": "attachment; filename=\"nexhub-cli.sh\"",
            }),
        })
    }
}

// ----------------------------------------------------------------------------
// 测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn get_req(path: &str, headers: serde_json::Value) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.to_string(),
            headers,
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    fn rendered_with_host(host: Option<&str>, forwarded: Option<&str>) -> ApiResponse {
        let mut headers = serde_json::Map::new();
        if let Some(h) = host {
            headers.insert("host".into(), serde_json::json!(h));
        }
        if let Some(f) = forwarded {
            headers.insert("x-forwarded-host".into(), serde_json::json!(f));
        }
        let req = get_req(CLI_SCRIPT_PATH, serde_json::Value::Object(headers));
        let resp = tokio_test_block_on(NexhubCliRouteHandler::new().handle(req))
            .expect("cli.sh handler 不应报错");
        assert_eq!(resp.status, 200, "cli.sh 端点应 200");
        resp
    }

    // 简单块执行（不引 tokio 测试宏依赖——handler 均为同步渲染包装）
    fn tokio_test_block_on<F: std::future::Future>(fut: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("构建测试 runtime");
        rt.block_on(fut)
    }

    /// 路由声明：公开读 + 正确 component。
    #[test]
    fn routes_declare_public_cli_script() {
        let h = NexhubCliRouteHandler::new();
        let routes = tokio_test_block_on(h.routes());
        assert_eq!(routes.len(), 1, "单端点 handler");
        let r = &routes[0];
        assert_eq!(r.method, HttpMethod::Get);
        assert_eq!(r.path, CLI_SCRIPT_PATH);
        assert_eq!(r.handler_component, "nexhub_cli");
        assert!(!r.requires_auth, "CLI 分发必须公开（未登录机器可达）");
        assert!(r.required_roles.is_empty());
    }

    /// 端点契约：200 + text/x-shellscript 直传 + nosniff + 内容含版本号与
    /// NEXHUB_NODE 字样（任务书要求的最低断言面）。
    #[test]
    fn endpoint_serves_script_with_content_type() {
        let resp = rendered_with_host(Some("203.0.113.9:8558"), None);
        let ct = resp.headers["content-type"].as_str().unwrap();
        assert!(
            ct.starts_with("text/x-shellscript"),
            "text/* 直传通道依赖该 content-type: {ct}"
        );
        assert_eq!(
            resp.headers["x-content-type-options"].as_str().unwrap(),
            "nosniff",
            "防 MIME 嗅探"
        );
        let body = resp.body.as_str().unwrap();
        assert!(body.starts_with("#!/usr/bin/env"), "应为可执行脚本: 首行");
        assert!(
            body.contains(env!("CARGO_PKG_VERSION")),
            "脚本内容应含运行二进制版本号"
        );
        assert!(body.contains("NEXHUB_NODE"), "脚本应含 NEXHUB_NODE 字样");
        assert!(
            body.contains("nexhub login"),
            "脚本应含 login 命令实现"
        );
        assert!(
            body.contains("curl -fsSL"),
            "help 顶注应含 curl|sh 安装一行命令"
        );
    }

    /// Host 头推导：带端口原样；无端口补缺省 8558。
    #[test]
    fn host_header_derivation_with_and_without_port() {
        let with_port = rendered_with_host(Some("203.0.113.9:8558"), None);
        assert!(
            with_port
                .body
                .as_str()
                .unwrap()
                .contains("NEXHUB_NODE_DEFAULT='http://203.0.113.9:8558'"),
            "带端口 Host 应原样烘焙"
        );
        let no_port = rendered_with_host(Some("hub.example.com"), None);
        assert!(
            no_port
                .body
                .as_str()
                .unwrap()
                .contains("NEXHUB_NODE_DEFAULT='http://hub.example.com:8558'"),
            "无端口 Host 应补缺省端口 8558"
        );
        // 渲染占位符必须全部被替换（不留 @@NEXHUB_ 残迹）
        assert!(
            !no_port.body.as_str().unwrap().contains("@@NEXHUB_"),
            "占位符残留"
        );
    }

    /// X-Forwarded-Host 优先于 Host（反代后推导真实对外地址）。
    #[test]
    fn x_forwarded_host_takes_priority() {
        let resp = rendered_with_host(
            Some("internal-host:9999"),
            Some("203.0.113.77:8558, 10.0.0.1"),
        );
        let body = resp.body.as_str().unwrap();
        assert!(
            body.contains("NEXHUB_NODE_DEFAULT='http://203.0.113.77:8558'"),
            "X-Forwarded-Host 首段应优先: 片段={}",
            body.lines()
                .find(|l| l.contains("NEXHUB_NODE_DEFAULT"))
                .unwrap_or("<缺>")
        );
        assert!(!body.contains("internal-host"), "Host 不应胜出");
    }

    /// 双头缺省 → 通告地址 env → 127.0.0.1:8558 兜底（脚本缺省恒有效）。
    #[test]
    fn missing_headers_fall_back_to_loopback_default() {
        let resp = rendered_with_host(None, None);
        assert!(
            resp.body
                .as_str()
                .unwrap()
                .contains("NEXHUB_NODE_DEFAULT='http://127.0.0.1:8558'"),
            "无 Host 时应回落 127.0.0.1:8558（NEXOS_GIT_ADVERTISE_HOST 未设）"
        );
    }

    /// 纯函数：Host 规格化（空值 / IPv6 带端口形态）。
    #[test]
    fn normalize_host_url_edge_cases() {
        assert_eq!(normalize_host_url(""), None);
        assert_eq!(normalize_host_url("  "), None);
        assert_eq!(
            normalize_host_url("[::1]:8558").as_deref(),
            Some("http://[::1]:8558")
        );
    }

    /// 渲染净化：单引号剥除（同 render_install_script 的防注入处理）。
    #[test]
    fn render_strips_single_quotes() {
        let out = render_cli_script("http://o'brien:8558',x='y");
        assert!(
            out.contains("NEXHUB_NODE_DEFAULT='http://obrien:8558,x=y'"),
            "单引号应被剥除: {}",
            out.lines()
                .find(|l| l.contains("NEXHUB_NODE_DEFAULT"))
                .unwrap_or("<缺>")
        );
        // cli.sh 自身 URL 同源推导
        assert!(out.contains("http://obrien:8558,x=y/api/v1/coderepo/cli.sh"));
    }

    /// 脚本资产通过 `bash -n` 语法校验（渲染后写出临时文件执行；bash
    /// 缺失时跳过——CI/开发机均有 bash）。
    #[test]
    fn rendered_script_passes_bash_syntax_check() {
        let script = render_cli_script("http://203.0.113.9:8558");
        let dir = std::env::temp_dir().join(format!("nexhub-cli-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        let path = dir.join("nexhub");
        std::fs::write(&path, script).expect("写出渲染脚本");
        let ok = match Command::new("bash").arg("-n").arg(&path).output() {
            Ok(out) => {
                assert!(
                    out.status.success(),
                    "bash -n 失败: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false, // 无 bash：跳过
            Err(e) => panic!("执行 bash -n 失败: {e}"),
        };
        let _ = std::fs::remove_dir_all(&dir);
        if !ok {
            eprintln!("跳过 bash -n（环境无 bash）");
        }
    }

    /// 凭据 0600 权限断言：跑渲染脚本 `login <node> <token>`（纯本地写文件，
    /// 零网络），断言 credentials 权限 600 / 内容格式 / **重复 login 收紧已有
    /// 宽松文件权限**。root 下 chmod 语义失真 → 跳过（任务书要求）。
    #[test]
    fn login_writes_credentials_with_0600() {
        if is_root() {
            eprintln!("跳过 0600 断言（root 环境权限语义失真）");
            return;
        }
        let ok = bash_available();
        assert!(ok, "本测试需要 bash");
        let script = render_cli_script("http://127.0.0.1:65530");
        let dir = std::env::temp_dir().join(format!("nexhub-cred-test-{}", std::process::id()));
        let home = dir.join("home");
        std::fs::create_dir_all(&home).expect("创建临时 HOME");
        let script_path = dir.join("nexhub");
        std::fs::write(&script_path, script).expect("写出渲染脚本");

        // 预置一个 0644 的旧凭据文件：login 必须把它收紧到 0600
        let cred_dir = home.join(".config/nexhub");
        std::fs::create_dir_all(&cred_dir).expect("预置凭据目录");
        let cred_file = cred_dir.join("credentials");
        std::fs::write(&cred_file, "NODE_URL=http://old:1\nTOKEN=stale\n").expect("预置旧凭据");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&cred_file, std::fs::Permissions::from_mode(0o644))
                .expect("置 0644");
        }

        let out = Command::new("bash")
            .arg(&script_path)
            .args(["login", "http://10.1.2.3:8558", "secret-token-abcdef"])
            .env("HOME", &home)
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .output()
            .expect("执行 nexhub login");
        assert!(
            out.status.success(),
            "login 应成功: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let saved = std::fs::read_to_string(&cred_file).expect("读回凭据文件");
        assert!(
            saved.contains("NODE_URL=http://10.1.2.3:8558\n"),
            "凭据应含 NODE_URL 行: {saved}"
        );
        assert!(
            saved.contains("TOKEN=secret-token-abcdef\n"),
            "凭据应含 TOKEN 行: {saved}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&cred_file)
                .expect("读凭据文件元数据")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "credentials 必须 0600（预置 0644 被 login 收紧）"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn is_root() -> bool {
        // 零依赖 euid 探测：id -u（linux/mac 皆有）；失败保守视为非 root 继续跑
        matches!(Command::new("id").arg("-u").output(),
            Ok(out) if String::from_utf8_lossy(&out.stdout).trim() == "0")
    }

    fn bash_available() -> bool {
        Command::new("bash")
            .arg("-c")
            .arg("true")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}
