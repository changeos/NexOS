//! `ContainersRouteHandler` —— 容器管理桌面应用的 HTTP→真实 Docker 适配器。
//!
//! 定位：把网关 HTTP 请求（`/api/v1/containers/*`）翻译为真实 Docker 子进程调用，返回 JSON。
//! 这是 OS"系统类三件套"之一（容器管理）桌面应用的后端 REST 入口。
//!
//! # 实现策略：真实 Docker（经 `sg docker -c '...'` 子进程）
//!
//! list / create / start / stop / restart / delete / images / stats 全部 spawn
//! 真实 docker 命令（用户在 docker 组，但旧 session 需经 `sg docker -c` 重新初始化组
//! 成员身份才能访问 docker socket）。docker 不存在 / sg 失败 / 守护进程未运行时
//! **降级**为空列表或 failed 响应，绝不 panic。路由签名与 JSON 结构与旧内存态保持
//! 一致，前端自动适配真实数据。
//!
//! # 路由表
//!
//! | method | path                            | 动作 |
//! |--------|---------------------------------|------|
//! | GET    | `/api/v1/containers/list`       | 列容器（docker ps -a）|
//! | POST   | `/api/v1/containers/create`     | 创建容器（docker run -d，需 admin）|
//! | POST   | `/api/v1/containers/:id/start`  | 启动（docker start，需 admin）|
//! | POST   | `/api/v1/containers/:id/stop`   | 停止（docker stop，需 admin）|
//! | POST   | `/api/v1/containers/:id/restart`| 重启（docker restart，需 admin）|
//! | DELETE | `/api/v1/containers/:id`        | 删除（docker rm -f，需 admin）|
//! | GET    | `/api/v1/containers/images`     | 列镜像（docker images）|
//! | GET    | `/api/v1/containers/stats`      | 统计（从 docker ps 聚合）|

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use tokio::process::Command;

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 一条容器（响应给前端）。字段与旧内存态保持兼容；cpu/mem 在真实 docker ps 下取不到，
/// 固定为 0.0（如需可另接 `docker stats`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    pub status: String,
    pub ports: Vec<String>,
    pub created_at: String,
    pub cpu_percent: f64,
    pub mem_usage_mb: f64,
}

/// `docker ps -a --format json` 解析后的单条容器信息（docker 原生字段，未做语义映射）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    pub image: String,
    /// docker "Status" 文本，如 "Up 2 minutes" / "Exited (0) 3 minutes ago"。
    pub status: String,
    /// docker "State"，如 running / exited / paused / created / restarting。
    pub state: String,
    pub ports: Vec<String>,
    pub created_at: String,
}

/// 一条镜像。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Image {
    pub id: String,
    pub name: String,
    pub tag: String,
    pub size_bytes: u64,
    pub created_at: String,
}

/// `GET /api/v1/containers/stats` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerStats {
    pub container_count: usize,
    pub running: usize,
    pub image_count: usize,
}

/// 创建容器请求体。`ports` 可选（如 `["80:80","443:443"]`）。
#[derive(Debug, Deserialize)]
struct CreateBody {
    name: String,
    image: String,
    #[serde(default)]
    ports: Option<Vec<String>>,
}

// ----------------------------------------------------------------------------
// Docker 命令构造器（纯函数，易测试）
// ----------------------------------------------------------------------------

/// 构造 `docker ps -a --format json` 命令（经 `sg docker -c` 包装）。
///
/// 返回 `["sg","docker","-c","docker ps -a --format json"]`。caller 用
/// `Command::new(cmd[0]).args(&cmd[1..])` 执行。
#[must_use]
pub fn build_docker_ps_cmd() -> Vec<String> {
    vec![
        "sg".into(),
        "docker".into(),
        "-c".into(),
        "docker ps -a --format json".into(),
    ]
}

/// 构造 `docker images --format json` 命令（经 `sg docker -c` 包装）。
#[must_use]
pub fn build_docker_images_cmd() -> Vec<String> {
    vec![
        "sg".into(),
        "docker".into(),
        "-c".into(),
        "docker images --format json".into(),
    ]
}

/// 构造 `docker run -d --name <name> [-p p ...] <image>` 命令（经 `sg docker -c` 包装）。
///
/// `ports` 每项原样作为 `-p` 参数（形如 `80:80`）。caller 负责保证 name/image 合法。
#[must_use]
pub fn build_docker_run_cmd(name: &str, image: &str, ports: &[String]) -> Vec<String> {
    let mut docker_cmd = format!("docker run -d --name {name}");
    for p in ports {
        let p = p.trim();
        if !p.is_empty() {
            docker_cmd.push_str(&format!(" -p {p}"));
        }
    }
    docker_cmd.push(' ');
    docker_cmd.push_str(image);
    vec!["sg".into(), "docker".into(), "-c".into(), docker_cmd]
}

/// 构造容器生命周期命令（start / stop / restart / "rm -f"），id 可为容器 ID 或 name。
#[must_use]
pub fn build_docker_action_cmd(action: &str, id: &str) -> Vec<String> {
    vec![
        "sg".into(),
        "docker".into(),
        "-c".into(),
        format!("docker {action} {id}"),
    ]
}

/// 解析 `docker ps -a --format json` 的输出（NDJSON，每行一个 JSON 对象）。
///
/// 空输出 / 全是乱码 → 返回空 Vec，不 panic。逐行解析，单行失败跳过该行。
#[must_use]
pub fn parse_docker_ps_json(output: &str) -> Vec<ContainerInfo> {
    let mut out = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let v = match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) => v,
            Err(_) => continue, // 跳过无法解析的行（降级）
        };
        let ports_str = v
            .get("Ports")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let ports: Vec<String> = ports_str
            .split(", ")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        out.push(ContainerInfo {
            id: v
                .get("ID")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            name: v
                .get("Names")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            image: v
                .get("Image")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            status: v
                .get("Status")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            state: v
                .get("State")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            ports,
            created_at: v
                .get("CreatedAt")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
        });
    }
    out
}

/// 解析 `docker images --format json` 的输出（NDJSON）。
#[must_use]
pub fn parse_docker_images_json(output: &str) -> Vec<Image> {
    let mut out = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let v = match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let raw_id = v
            .get("ID")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let id = raw_id
            .strip_prefix("sha256:")
            .unwrap_or(&raw_id)
            .to_string();
        let name = v
            .get("Repository")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let tag = v
            .get("Tag")
            .and_then(|x| x.as_str())
            .unwrap_or("latest")
            .to_string();
        let size_str = v
            .get("Size")
            .and_then(|x| x.as_str())
            .unwrap_or("0")
            .to_string();
        out.push(Image {
            id,
            name,
            tag,
            size_bytes: parse_size_to_bytes(&size_str),
            created_at: v
                .get("CreatedAt")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
        });
    }
    out
}

/// 把 docker 的 Size 文本（"192MB"/"1.5GB"/"0B"/"<none>"）解析为字节数。失败 → 0。
fn parse_size_to_bytes(s: &str) -> u64 {
    let s = s.trim();
    if s.is_empty() || s == "<none>" {
        return 0;
    }
    // 数字（含小数点）与单位的边界
    let split_at = s
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_digit() || *c == '.'))
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    let (num_str, unit_str) = s.split_at(split_at);
    let n: f64 = num_str.parse().unwrap_or(0.0);
    let mult: f64 = match unit_str.trim() {
        "B" => 1.0,
        "kB" | "KB" => 1_000.0,
        "MB" => 1_000_000.0,
        "GB" => 1_000_000_000.0,
        "TB" => 1_000_000_000_000.0,
        "PB" => 1_000_000_000_000_000.0,
        _ => 1.0,
    };
    (n * mult) as u64
}

/// 把 docker State（running/exited/paused/...）映射为前端期望的简洁状态。
fn state_to_status(state: &str) -> String {
    match state {
        "running" => "running".into(),
        "exited" | "dead" | "terminated" => "stopped".into(),
        "paused" => "paused".into(),
        other => other.to_string(),
    }
}

impl From<ContainerInfo> for Container {
    fn from(c: ContainerInfo) -> Self {
        Self {
            id: c.id,
            name: c.name,
            image: c.image,
            status: state_to_status(&c.state),
            ports: c.ports,
            created_at: c.created_at,
            cpu_percent: 0.0,
            mem_usage_mb: 0.0,
        }
    }
}

// ----------------------------------------------------------------------------
// ContainersRouteHandler
// ----------------------------------------------------------------------------

/// 容器管理路由处理器——HTTP 边界适配到真实 Docker（无内存态，docker 即数据源）。
pub struct ContainersRouteHandler;

impl ContainersRouteHandler {
    /// 构造 handler。
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// 空构造（保留供测试/旧调用路径，与 [`Self::new`] 同义）。
    #[must_use]
    pub fn with_empty() -> Self {
        Self
    }

    /// 执行一条 docker 命令（`["sg","docker","-c","..."]`），返回 (success, stdout)。
    ///
    /// docker / sg 不存在或 spawn 失败 → 返回 `None`（caller 降级）。
    async fn run_docker(cmd: &[String]) -> Option<(bool, String)> {
        if cmd.is_empty() {
            return None;
        }
        let out = Command::new(&cmd[0])
            .args(&cmd[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .ok()?;
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let combined = if stdout.is_empty() { stderr } else { stdout };
        Some((out.status.success(), combined))
    }

    /// 列容器：spawn `docker ps -a --format json`，解析为 Container 列表。失败 → 空。
    async fn list_containers() -> Vec<Container> {
        let cmd = build_docker_ps_cmd();
        match Self::run_docker(&cmd).await {
            Some((true, out)) => parse_docker_ps_json(&out)
                .into_iter()
                .map(Container::from)
                .collect(),
            _ => Vec::new(),
        }
    }

    /// 列镜像：spawn `docker images --format json`，解析为 Image 列表。失败 → 空。
    async fn list_images() -> Vec<Image> {
        let cmd = build_docker_images_cmd();
        match Self::run_docker(&cmd).await {
            Some((true, out)) => parse_docker_images_json(&out),
            _ => Vec::new(),
        }
    }
}

impl Default for ContainersRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RouteHandler for ContainersRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec(HttpMethod::Get, "/api/v1/containers/list", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/containers/create",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/containers/:id/start",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/containers/:id/stop",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/containers/:id/restart",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/containers/:id",
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Get, "/api/v1/containers/images", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/containers/stats", false, vec![]),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            // —— GET /api/v1/containers/list —— 列容器（真实 docker ps）
            (HttpMethod::Get, ["api", "v1", "containers", "list"]) => {
                let list = Self::list_containers().await;
                Ok(ok_json(to_value(&list)?))
            }

            // —— GET /api/v1/containers/images —— 列镜像（真实 docker images）
            (HttpMethod::Get, ["api", "v1", "containers", "images"]) => {
                let list = Self::list_images().await;
                Ok(ok_json(to_value(&list)?))
            }

            // —— GET /api/v1/containers/stats —— 统计（docker ps 聚合）
            (HttpMethod::Get, ["api", "v1", "containers", "stats"]) => {
                let cmd = build_docker_ps_cmd();
                let (running, total) = match Self::run_docker(&cmd).await {
                    Some((true, out)) => {
                        let infos = parse_docker_ps_json(&out);
                        let r = infos.iter().filter(|c| c.state == "running").count();
                        (r, infos.len())
                    }
                    _ => (0, 0),
                };
                let image_count = Self::list_images().await.len();
                Ok(ok_json(to_value(&ContainerStats {
                    container_count: total,
                    running,
                    image_count,
                })?))
            }

            // —— POST /api/v1/containers/create —— 创建（真实 docker run -d）
            (HttpMethod::Post, ["api", "v1", "containers", "create"]) => {
                let body: CreateBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析创建容器请求体失败: {e}"))
                })?;
                if body.name.trim().is_empty() {
                    return Ok(error_response(400, "name 不可为空"));
                }
                if body.image.trim().is_empty() {
                    return Ok(error_response(400, "image 不可为空"));
                }
                // 基本防注入：name 经 shell 插值，拒绝危险字符
                if has_shell_meta(&body.name) {
                    return Ok(error_response(400, "name 含非法字符"));
                }
                let ports = body.ports.unwrap_or_default();
                let cmd = build_docker_run_cmd(body.name.trim(), body.image.trim(), &ports);
                match Self::run_docker(&cmd).await {
                    Some((true, out)) => {
                        // docker run -d 把容器 ID 打印到 stdout（64 位 hex）
                        let cid = out.split_whitespace().next().unwrap_or("").to_string();
                        let c = Container {
                            id: cid,
                            name: body.name,
                            image: body.image,
                            status: "running".into(), // -d 创建即启动
                            ports,
                            created_at: now_iso(),
                            cpu_percent: 0.0,
                            mem_usage_mb: 0.0,
                        };
                        Ok(ApiResponse {
                            status: 201,
                            body: to_value(&c)?,
                            headers: serde_json::json!({}),
                        })
                    }
                    Some((false, detail)) => Ok(error_response(
                        502,
                        &format!("docker run 失败: {}", detail.trim()),
                    )),
                    None => Ok(error_response(502, "docker 不可用（sg/docker 未找到）")),
                }
            }

            // —— POST /api/v1/containers/:id/start|stop|restart —— 真实 docker 生命周期
            (HttpMethod::Post, ["api", "v1", "containers", id, action])
                if matches!(*action, "start" | "stop" | "restart") =>
            {
                if has_shell_meta(id) {
                    return Ok(error_response(400, "id 含非法字符"));
                }
                let cmd = build_docker_action_cmd(action, id);
                match Self::run_docker(&cmd).await {
                    Some((true, _)) => {
                        let status = match *action {
                            "start" | "restart" => "running",
                            "stop" => "stopped",
                            _ => "unknown",
                        };
                        Ok(ok_json(serde_json::json!({
                            "ok": true,
                            "id": id,
                            "action": *action,
                            "status": status,
                            "docker": true,
                        })))
                    }
                    Some((false, detail)) => Ok(error_response(
                        502,
                        &format!("docker {action} 失败: {}", detail.trim()),
                    )),
                    None => Ok(error_response(502, "docker 不可用（sg/docker 未找到）")),
                }
            }

            // —— DELETE /api/v1/containers/:id —— 删除（真实 docker rm -f）
            (HttpMethod::Delete, ["api", "v1", "containers", id]) => {
                if has_shell_meta(id) {
                    return Ok(error_response(400, "id 含非法字符"));
                }
                let cmd = build_docker_action_cmd("rm -f", id);
                match Self::run_docker(&cmd).await {
                    Some((true, _)) => Ok(ok_json(
                        serde_json::json!({"ok": true, "id": id, "action": "delete"}),
                    )),
                    Some((false, detail)) => Ok(error_response(
                        502,
                        &format!("docker rm 失败: {}", detail.trim()),
                    )),
                    None => Ok(error_response(502, "docker 不可用（sg/docker 未找到）")),
                }
            }

            // —— 未覆盖路由 —— 兜底 404
            _ => Ok(error_response(404, "containers: 未匹配的路由")),
        }
    }
}

// ----------------------------------------------------------------------------
// 内部辅助
// ----------------------------------------------------------------------------

fn spec(
    method: HttpMethod,
    path: &str,
    requires_auth: bool,
    required_roles: Vec<String>,
) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: "containers".to_string(),
        requires_auth,
        required_roles,
    }
}

fn ok_json(body: serde_json::Value) -> ApiResponse {
    ApiResponse {
        status: 200,
        body,
        headers: serde_json::json!({}),
    }
}

fn error_response(status: u16, msg: &str) -> ApiResponse {
    ApiResponse {
        status,
        body: serde_json::json!({"error": msg}),
        headers: serde_json::json!({}),
    }
}

fn to_value<T: Serialize>(v: &T) -> Result<serde_json::Value, ApiGatewayError> {
    serde_json::to_value(v).map_err(|e| ApiGatewayError::Internal(format!("响应序列化失败: {e}")))
}

fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

fn now_iso() -> String {
    use chrono::Local;
    Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

/// 检测字符串是否含 shell 元字符（防注入：name/id 经 `sg docker -c` 字符串插值）。
fn has_shell_meta(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(
            c,
            ';' | '&'
                | '|'
                | '$'
                | '`'
                | '"'
                | '\''
                | '\\'
                | '\n'
                | '\r'
                | '>'
                | '<'
                | '('
                | ')'
                | '{'
                | '}'
        )
    })
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn get_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    fn post_req(path: &str, body: serde_json::Value) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Post,
            path: path.into(),
            headers: serde_json::json!({}),
            body,
            auth: None,
        }
    }

    fn del_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Delete,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    // ---- 纯函数：build_docker_ps_cmd ----

    #[test]
    fn build_docker_ps_cmd_uses_sg_wrapper_and_json_format() {
        let cmd = build_docker_ps_cmd();
        assert_eq!(
            cmd,
            vec!["sg", "docker", "-c", "docker ps -a --format json"]
        );
    }

    // ---- 纯函数：build_docker_run_cmd ----

    #[test]
    fn build_docker_run_cmd_without_ports() {
        let cmd = build_docker_run_cmd("web", "nginx:1.27", &[]);
        assert_eq!(cmd[0..3], ["sg", "docker", "-c"]);
        assert_eq!(cmd[3], "docker run -d --name web nginx:1.27");
    }

    #[test]
    fn build_docker_run_cmd_with_ports() {
        let ports = vec!["80:80".into(), "443:443".into()];
        let cmd = build_docker_run_cmd("web", "nginx", &ports);
        assert_eq!(cmd[3], "docker run -d --name web -p 80:80 -p 443:443 nginx");
    }

    // ---- 纯函数：parse_docker_ps_json ----

    #[test]
    fn parse_docker_ps_json_parses_ndjson_lines() {
        let out = concat!(
            r#"{"ID":"a1b2c3","Names":"web","Image":"nginx","Status":"Up 2 minutes","State":"running","Ports":"0.0.0.0:80->80/tcp","CreatedAt":"2026-08-01"}"#,
            "\n",
            r#"{"ID":"d4e5f6","Names":"db","Image":"postgres","Status":"Exited (0)","State":"exited","Ports":"","CreatedAt":"2026-07-01"}"#,
        );
        let list = parse_docker_ps_json(out);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "a1b2c3");
        assert_eq!(list[0].name, "web");
        assert_eq!(list[0].ports, vec!["0.0.0.0:80->80/tcp"]);
        assert_eq!(list[1].state, "exited");
        assert!(list[1].ports.is_empty());
    }

    #[test]
    fn parse_docker_ps_json_empty_and_garbage_degrades_to_empty() {
        assert!(parse_docker_ps_json("").is_empty());
        assert!(parse_docker_ps_json("not json\nalso not").is_empty());
        // 混合：一行合法 + 一行乱码 → 只保留合法那行
        let mixed = "garbage\n{\"ID\":\"x\",\"Names\":\"y\",\"Image\":\"z\",\"Status\":\"\",\"State\":\"created\",\"Ports\":\"\"}";
        let list = parse_docker_ps_json(mixed);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "x");
    }

    #[test]
    fn parse_docker_images_json_strips_sha256_and_parses_size() {
        let out = r#"{"ID":"sha256:abc123","Repository":"nginx","Tag":"1.27","Size":"192MB","CreatedAt":"2026-08-01"}"#;
        let list = parse_docker_images_json(out);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "abc123");
        assert_eq!(list[0].name, "nginx");
        assert_eq!(list[0].size_bytes, 192_000_000);
    }

    #[test]
    fn state_to_status_maps_exited_to_stopped() {
        assert_eq!(state_to_status("running"), "running");
        assert_eq!(state_to_status("exited"), "stopped");
        assert_eq!(state_to_status("paused"), "paused");
        assert_eq!(state_to_status("restarting"), "restarting");
    }

    // ---- 路由声明 ----

    #[tokio::test]
    async fn routes_declares_eight_endpoints() {
        let h = ContainersRouteHandler::new();
        let routes = h.routes().await;
        assert_eq!(routes.len(), 8);
        assert!(routes.iter().all(|r| r.handler_component == "containers"));
        for r in &routes {
            if r.method == HttpMethod::Post || r.method == HttpMethod::Delete {
                assert!(r.requires_auth);
                assert_eq!(r.required_roles, vec!["admin".to_string()]);
            }
        }
    }

    // ---- handler：list/stats 返回数组/对象（真实 docker，可能为空，不 panic）----

    #[tokio::test]
    async fn list_returns_array_without_panicking() {
        let h = ContainersRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/containers/list")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body.is_array(), "list 必须返回数组: {resp:?}");
    }

    #[tokio::test]
    async fn stats_returns_counts_object() {
        let h = ContainersRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/containers/stats")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body["container_count"].is_u64());
        assert!(resp.body["running"].is_u64());
        assert!(resp.body["image_count"].is_u64());
    }

    // ---- handler：create 校验 ----

    #[tokio::test]
    async fn create_validates_empty_name() {
        let h = ContainersRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/containers/create",
                serde_json::json!({"name": "", "image": "alpine"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    #[tokio::test]
    async fn create_rejects_shell_meta_in_name() {
        let h = ContainersRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/containers/create",
                serde_json::json!({"name": "evil;rm -rf /", "image": "alpine"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    // ---- handler：start 不存在的 id 应返回错误（docker start 失败），不 panic ----

    #[tokio::test]
    async fn start_nonexistent_returns_error_without_panicking() {
        let h = ContainersRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/containers/nope/start",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_ne!(resp.status, 200, "不存在的容器不应 200: {resp:?}");
    }

    #[tokio::test]
    async fn delete_nonexistent_does_not_panic() {
        // `docker rm -f` 对不存在的容器是幂等的（可能返回 0）；这里只验证不 panic、
        // 返回结构合法（200 ok 或 502 error 均可接受）。
        let h = ContainersRouteHandler::with_empty();
        let resp = h
            .handle(del_req("/api/v1/containers/definitely-not-exist-xyz"))
            .await
            .unwrap();
        assert!(
            resp.status == 200 || resp.status == 502,
            "删除应返回 200/502: {resp:?}"
        );
    }

    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<ContainersRouteHandler>();
    }
}
