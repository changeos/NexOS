//! `CloudSyncRouteHandler` —— 云同步桌面应用的 HTTP→真实 rclone 适配器。
//!
//! 定位：把网关 HTTP 请求（`/api/v1/cloudsync/*`）翻译为 rclone 子进程编排，返回 JSON。
//! 这是 OS"应用类三件套"之一（云同步）桌面应用的后端 REST 入口。
//!
//! # 实现策略：真实 rclone spawn（任务定义在内存，同步动作真跑 rclone）
//!
//! 任务定义（id/name/local_path/remote_provider/remote_path/sync_mode）保存在内存——
//! 因为 rclone 本身无状态（每次运行即一次性同步）。`sync` / `resume` 真实 spawn
//! `rclone sync <local> <remote>:<path> --progress`（后台跑、stderr 落日志、存 pid）。
//! `pause` 用 kill pid（rclone 无原生 pause）。`GET /tasks` 探测每个任务 pid 存活以
//! 刷新状态（syncing → idle）。rclone 未安装 / spawn 失败 → **降级**为 error，绝不 panic。
//!
//! rclone 远程配置由用户事先用 `rclone config` 创建（S3/WebDAV/OneDrive/Google Drive/
//! 阿里云 OSS），存 `~/.config/rclone/rclone.conf`；`remote_provider` 应填已配置的
//! rclone remote 名（如 `mys3`）。
//!
//! # 路由表
//!
//! | method | path                                   | 动作 |
//! |--------|----------------------------------------|------|
//! | GET    | `/api/v1/cloudsync/tasks`              | 列全部任务（刷新 pid 状态）|
//! | POST   | `/api/v1/cloudsync/tasks`              | 创建（需 admin）|
//! | POST   | `/api/v1/cloudsync/tasks/:id/sync`     | 触发同步（spawn rclone，需 admin）|
//! | POST   | `/api/v1/cloudsync/tasks/:id/pause`    | 暂停（kill pid，需 admin）|
//! | POST   | `/api/v1/cloudsync/tasks/:id/resume`   | 继续（重新 spawn，需 admin）|
//! | DELETE | `/api/v1/cloudsync/tasks/:id`          | 删除（kill pid + 移除，需 admin）|
//! | GET    | `/api/v1/cloudsync/stats`              | 统计 |

use std::path::Path;
use std::process::Stdio;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 一条云同步任务。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncTask {
    pub id: String,
    pub name: String,
    pub local_path: String,
    /// 已配置的 rclone remote 名（如 mys3 / myod），亦作 provider 标签：
    /// s3 / onedrive / google / webdav / aliyun
    pub remote_provider: String,
    pub remote_path: String,
    /// one_way_up / one_way_down / two_way
    pub sync_mode: String,
    /// idle / syncing / error / paused
    pub status: String,
    pub last_sync_at: Option<String>,
    pub files_synced: u64,
    pub total_size_bytes: u64,
    /// 运行中的 rclone 子进程 pid（syncing 时）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// rclone stderr 日志路径（`/tmp/os-rclone-<id>.log`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,
    /// 错误信息（spawn 失败等）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `GET /api/v1/cloudsync/stats` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudSyncStats {
    pub total_tasks: usize,
    pub syncing: usize,
    pub providers_used: Vec<String>,
    pub total_synced_bytes: u64,
}

/// 创建任务请求体。
#[derive(Debug, Deserialize)]
struct CreateBody {
    name: String,
    local_path: String,
    remote_provider: String,
    remote_path: String,
    #[serde(default)]
    sync_mode: Option<String>,
}

const VALID_PROVIDERS: &[&str] = &["s3", "onedrive", "google", "webdav", "aliyun"];
const VALID_MODES: &[&str] = &["one_way_up", "one_way_down", "two_way"];

// ----------------------------------------------------------------------------
// rclone 纯函数（易测试）
// ----------------------------------------------------------------------------

/// 构造 `rclone sync <local> <remote>:<path> --progress` 命令。
///
/// `remote`（第二参数，目的地）的解析规则：
/// - 已含 `:`（形如 `mys3:bucket`）→ 原样用作目的地；
/// - 以 `/` 开头（本地绝对路径，用于 one_way_down 反向）→ 原样用作目的地；
/// - 否则视作远端子路径，前置 `provider:`（如 `s3:photo`）。
///
/// 返回的 Vec 不含程序名以外的转义——caller 直接 `Command::new(&v[0]).args(&v[1..])`。
#[must_use]
pub fn build_rclone_sync_cmd(local: &str, remote: &str, provider: &str) -> Vec<String> {
    let target = if remote.contains(':') || remote.starts_with('/') {
        remote.to_string()
    } else {
        format!("{provider}:{remote}")
    };
    vec![
        "rclone".into(),
        "sync".into(),
        local.into(),
        target,
        "--progress".into(),
    ]
}

// ----------------------------------------------------------------------------
// CloudSyncRouteHandler
// ----------------------------------------------------------------------------

/// 云同步路由处理器——HTTP 边界适配到内存任务定义 + 真实 rclone spawn。
pub struct CloudSyncRouteHandler {
    tasks: Mutex<Vec<SyncTask>>,
    counter: Mutex<u64>,
}

impl CloudSyncRouteHandler {
    /// 构造 handler——启动时空任务列表（不再预置 demo）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(vec![]),
            counter: Mutex::new(100),
        }
    }

    /// 用空列表构造（测试注入）。
    #[must_use]
    pub fn with_empty() -> Self {
        Self::new()
    }

    /// 当前全量任务快照（先刷新 pid 状态）。
    #[must_use]
    pub fn tasks_snapshot(&self) -> Vec<SyncTask> {
        self.refresh_all();
        self.tasks.lock().expect("tasks poisoned").clone()
    }

    fn next_id(&self) -> String {
        let mut c = self.counter.lock().expect("counter poisoned");
        *c += 1;
        format!("sync-{}", *c)
    }

    /// 探测 pid 是否仍"有效存活"（非僵尸）。无 /proc 时退化为 `kill -0`。
    fn pid_alive(pid: u32) -> bool {
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !alive {
            return false;
        }
        let stat_path = format!("/proc/{pid}/stat");
        if let Ok(content) = std::fs::read_to_string(&stat_path) {
            if let Some(after_comm) = content.rsplit(')').next() {
                let state = after_comm.trim_start().chars().next().unwrap_or(' ');
                if state == 'Z' {
                    return false; // 僵尸，视为已退出
                }
            }
        }
        true
    }

    /// 刷新所有 syncing 任务的 pid 存活状态：pid 已退出 → status=idle。
    fn refresh_all(&self) {
        let mut tasks = self.tasks.lock().expect("tasks poisoned");
        for t in tasks.iter_mut() {
            if t.status == "syncing" {
                if let Some(pid) = t.pid {
                    if !Self::pid_alive(pid) {
                        t.status = "idle".into();
                        t.last_sync_at = Some(now_iso());
                        t.pid = None;
                    }
                } else {
                    // syncing 但无 pid（异常）→ 回收为 idle
                    t.status = "idle".into();
                }
            }
        }
    }

    /// 真实 spawn rclone（fire-and-forget），stderr 落 log_path。成功返回 pid。
    fn spawn_rclone(args: &[String], log_path: &Path) -> Result<u32, String> {
        if args.is_empty() {
            return Err("rclone 命令为空".into());
        }
        let mut cmd = std::process::Command::new(&args[0]);
        cmd.args(&args[1..]);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::null());
        let stderr_file = std::fs::File::create(log_path)
            .map(Stdio::from)
            .unwrap_or(Stdio::null());
        cmd.stderr(stderr_file);
        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id();
                drop(child); // 由 OS 收养，后台继续跑
                Ok(pid)
            }
            Err(e) => Err(format!("spawn rclone 失败: {e}")),
        }
    }

    /// kill 一个 pid（SIGTERM）。失败返回 Err，caller 仍可继续。
    fn kill_pid(pid: u32) -> Result<(), String> {
        match std::process::Command::new("kill")
            .arg(pid.to_string())
            .output()
        {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => Err(format!(
                "kill {pid} 退出码 {:?}: {}",
                o.status.code(),
                String::from_utf8_lossy(&o.stderr).trim()
            )),
            Err(e) => Err(format!("kill {pid} 失败: {e}")),
        }
    }

    /// 为指定任务构造 rclone 命令（按 sync_mode 决定方向）。
    fn build_cmd_for(task: &SyncTask) -> Vec<String> {
        match task.sync_mode.as_str() {
            // 反向：rclone sync <remote_target> <local_path>
            "one_way_down" => {
                let remote_target = if task.remote_path.contains(':') {
                    task.remote_path.clone()
                } else {
                    format!("{}:{}", task.remote_provider, task.remote_path)
                };
                build_rclone_sync_cmd(&remote_target, &task.local_path, &task.remote_provider)
            }
            // 上行 / 双向（双向退化为上行 sync）
            _ => build_rclone_sync_cmd(&task.local_path, &task.remote_path, &task.remote_provider),
        }
    }
}

impl Default for CloudSyncRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RouteHandler for CloudSyncRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec(HttpMethod::Get, "/api/v1/cloudsync/tasks", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/cloudsync/tasks",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/cloudsync/tasks/:id/sync",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/cloudsync/tasks/:id/pause",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/cloudsync/tasks/:id/resume",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/cloudsync/tasks/:id",
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Get, "/api/v1/cloudsync/stats", false, vec![]),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            // —— GET /api/v1/cloudsync/tasks —— 列全部（先刷新状态）
            (HttpMethod::Get, ["api", "v1", "cloudsync", "tasks"]) => {
                let tasks = self.tasks_snapshot();
                Ok(ok_json(to_value(&tasks)?))
            }

            // —— GET /api/v1/cloudsync/stats —— 统计
            (HttpMethod::Get, ["api", "v1", "cloudsync", "stats"]) => {
                let tasks = self.tasks_snapshot();
                let syncing = tasks.iter().filter(|t| t.status == "syncing").count();
                let bytes = tasks.iter().map(|t| t.total_size_bytes).sum();
                let mut providers: Vec<String> = Vec::new();
                for t in &tasks {
                    if !providers.iter().any(|p| p == &t.remote_provider) {
                        providers.push(t.remote_provider.clone());
                    }
                }
                Ok(ok_json(to_value(&CloudSyncStats {
                    total_tasks: tasks.len(),
                    syncing,
                    providers_used: providers,
                    total_synced_bytes: bytes,
                })?))
            }

            // —— POST /api/v1/cloudsync/tasks —— 创建
            (HttpMethod::Post, ["api", "v1", "cloudsync", "tasks"]) => {
                let body: CreateBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析创建同步任务请求体失败: {e}"))
                })?;
                if body.name.trim().is_empty() {
                    return Ok(error_response(400, "name 不可为空"));
                }
                if body.local_path.trim().is_empty() {
                    return Ok(error_response(400, "local_path 不可为空"));
                }
                if body.remote_path.trim().is_empty() {
                    return Ok(error_response(400, "remote_path 不可为空"));
                }
                let provider = body.remote_provider.trim().to_lowercase();
                if !VALID_PROVIDERS.contains(&provider.as_str()) {
                    return Ok(error_response(
                        400,
                        &format!("remote_provider 取值须为 {:?}", VALID_PROVIDERS),
                    ));
                }
                let mode = body
                    .sync_mode
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "one_way_up".to_string());
                if !VALID_MODES.contains(&mode.as_str()) {
                    return Ok(error_response(
                        400,
                        &format!("sync_mode 取值须为 {:?}", VALID_MODES),
                    ));
                }
                let task = SyncTask {
                    id: self.next_id(),
                    name: body.name,
                    local_path: body.local_path,
                    remote_provider: provider,
                    remote_path: body.remote_path,
                    sync_mode: mode,
                    status: "idle".into(),
                    last_sync_at: None,
                    files_synced: 0,
                    total_size_bytes: 0,
                    pid: None,
                    log_path: None,
                    error: None,
                };
                let resp_body = to_value(&task)?;
                self.tasks.lock().expect("tasks poisoned").push(task);
                Ok(ApiResponse {
                    status: 201,
                    body: resp_body,
                    headers: serde_json::json!({}),
                })
            }

            // —— POST /api/v1/cloudsync/tasks/:id/sync —— 触发同步（spawn rclone）
            (HttpMethod::Post, ["api", "v1", "cloudsync", "tasks", id, "sync"]) => {
                let snap = {
                    let tasks = self.tasks.lock().expect("tasks poisoned");
                    tasks.iter().find(|t| t.id == *id).cloned()
                };
                let Some(task) = snap else {
                    return Ok(error_response(404, &format!("同步任务不存在: {id}")));
                };
                let cmd = Self::build_cmd_for(&task);
                let log_path = std::env::temp_dir().join(format!("os-rclone-{id}.log"));
                let log_str = log_path.to_string_lossy().to_string();
                let spawn_res = Self::spawn_rclone(&cmd, &log_path);
                let mut tasks = self.tasks.lock().expect("tasks poisoned");
                let t = match tasks.iter_mut().find(|t| t.id == *id) {
                    Some(t) => t,
                    None => return Ok(error_response(404, &format!("同步任务不存在: {id}"))),
                };
                match spawn_res {
                    Ok(pid) => {
                        t.status = "syncing".into();
                        t.last_sync_at = Some(now_iso());
                        t.pid = Some(pid);
                        t.log_path = Some(log_str);
                        t.error = None;
                    }
                    Err(e) => {
                        t.status = "error".into();
                        t.error = Some(e);
                    }
                }
                Ok(ok_json(to_value(&t)?))
            }

            // —— POST /api/v1/cloudsync/tasks/:id/pause —— 暂停（kill pid）
            (HttpMethod::Post, ["api", "v1", "cloudsync", "tasks", id, "pause"]) => {
                let mut tasks = self.tasks.lock().expect("tasks poisoned");
                let t = match tasks.iter_mut().find(|t| t.id == *id) {
                    Some(t) => t,
                    None => return Ok(error_response(404, &format!("同步任务不存在: {id}"))),
                };
                if let Some(pid) = t.pid.take() {
                    let _ = Self::kill_pid(pid); // 杀不掉也继续
                }
                t.status = "paused".into();
                Ok(ok_json(to_value(&t)?))
            }

            // —— POST /api/v1/cloudsync/tasks/:id/resume —— 继续（重新 spawn）
            (HttpMethod::Post, ["api", "v1", "cloudsync", "tasks", id, "resume"]) => {
                let snap = {
                    let tasks = self.tasks.lock().expect("tasks poisoned");
                    tasks.iter().find(|t| t.id == *id).cloned()
                };
                let Some(task) = snap else {
                    return Ok(error_response(404, &format!("同步任务不存在: {id}")));
                };
                let cmd = Self::build_cmd_for(&task);
                let log_path = std::env::temp_dir().join(format!("os-rclone-{id}.log"));
                let log_str = log_path.to_string_lossy().to_string();
                let spawn_res = Self::spawn_rclone(&cmd, &log_path);
                let mut tasks = self.tasks.lock().expect("tasks poisoned");
                let t = match tasks.iter_mut().find(|t| t.id == *id) {
                    Some(t) => t,
                    None => return Ok(error_response(404, &format!("同步任务不存在: {id}"))),
                };
                match spawn_res {
                    Ok(pid) => {
                        t.status = "syncing".into();
                        t.last_sync_at = Some(now_iso());
                        t.pid = Some(pid);
                        t.log_path = Some(log_str);
                        t.error = None;
                    }
                    Err(e) => {
                        t.status = "error".into();
                        t.error = Some(e);
                    }
                }
                Ok(ok_json(to_value(&t)?))
            }

            // —— DELETE /api/v1/cloudsync/tasks/:id —— 删除（kill pid + 移除）
            (HttpMethod::Delete, ["api", "v1", "cloudsync", "tasks", id]) => {
                let mut tasks = self.tasks.lock().expect("tasks poisoned");
                let before = tasks.len();
                // 先 kill 运行中的 pid
                if let Some(t) = tasks.iter().find(|t| t.id == *id) {
                    if let Some(pid) = t.pid {
                        let _ = Self::kill_pid(pid);
                    }
                }
                tasks.retain(|t| t.id != *id);
                if tasks.len() == before {
                    return Ok(error_response(404, &format!("同步任务不存在: {id}")));
                }
                Ok(ok_json(
                    serde_json::json!({"ok": true, "id": id, "action": "delete"}),
                ))
            }

            // —— 未覆盖路由 —— 兜底 404
            _ => Ok(error_response(404, "cloudsync: 未匹配的路由")),
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
        handler_component: "cloudsync".to_string(),
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

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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

    fn get_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    // ---- 纯函数：build_rclone_sync_cmd ----

    #[test]
    fn build_rclone_sync_cmd_up_direction_prefixes_provider() {
        let cmd = build_rclone_sync_cmd("/tank/photo", "photo", "mys3");
        assert_eq!(cmd[0], "rclone");
        assert_eq!(cmd[1], "sync");
        assert_eq!(cmd[2], "/tank/photo");
        assert_eq!(cmd[3], "mys3:photo");
        assert_eq!(cmd[4], "--progress");
    }

    #[test]
    fn build_rclone_sync_cmd_keeps_already_prefixed_remote() {
        let cmd = build_rclone_sync_cmd("/tank/d", "myod:/OS/docs", "onedrive");
        assert_eq!(cmd[3], "myod:/OS/docs");
    }

    #[test]
    fn build_rclone_sync_cmd_down_direction_uses_local_as_dest() {
        // one_way_down：source=remote_target(含:)，dest=local(以/开头，不前缀)
        let cmd = build_rclone_sync_cmd("mys3:photo", "/tank/photo", "mys3");
        assert_eq!(cmd[2], "mys3:photo");
        assert_eq!(cmd[3], "/tank/photo");
    }

    // ---- 路由声明 ----

    #[tokio::test]
    async fn routes_declares_seven_endpoints() {
        let h = CloudSyncRouteHandler::new();
        let routes = h.routes().await;
        assert_eq!(routes.len(), 7);
        assert!(routes.iter().all(|r| r.handler_component == "cloudsync"));
        for r in &routes {
            if r.method == HttpMethod::Post || r.method == HttpMethod::Delete {
                assert!(r.requires_auth);
                assert_eq!(r.required_roles, vec!["admin".to_string()]);
            }
        }
    }

    // ---- 创建校验 ----

    #[tokio::test]
    async fn create_task_succeeds_and_starts_idle() {
        let h = CloudSyncRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/cloudsync/tasks",
                serde_json::json!({
                    "name": "照片",
                    "local_path": "/tank/photo",
                    "remote_provider": "s3",
                    "remote_path": "photo"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "create body: {resp:?}");
        assert_eq!(resp.body["status"], "idle");
        assert_eq!(resp.body["sync_mode"], "one_way_up");
        assert!(resp.body["id"].as_str().unwrap().starts_with("sync-"));
    }

    #[tokio::test]
    async fn create_rejects_invalid_provider() {
        let h = CloudSyncRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/cloudsync/tasks",
                serde_json::json!({
                    "name": "x",
                    "local_path": "/tank",
                    "remote_provider": "dropbox",
                    "remote_path": "/x"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    #[tokio::test]
    async fn create_rejects_empty_name() {
        let h = CloudSyncRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/cloudsync/tasks",
                serde_json::json!({
                    "name": "",
                    "local_path": "/tank",
                    "remote_provider": "s3",
                    "remote_path": "/x"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    // ---- sync / pause / resume / delete（rclone 未装时降级 error，不 panic）----

    fn make_handler_with_task() -> CloudSyncRouteHandler {
        let h = CloudSyncRouteHandler::with_empty();
        // 注入一个任务（绕过 HTTP，直接写内存）
        h.tasks.lock().expect("tasks poisoned").push(SyncTask {
            id: "sync-test".into(),
            name: "测试".into(),
            local_path: "/tmp/os-cs-src".into(),
            remote_provider: "s3".into(),
            remote_path: "bucket/path".into(),
            sync_mode: "one_way_up".into(),
            status: "idle".into(),
            last_sync_at: None,
            files_synced: 0,
            total_size_bytes: 0,
            pid: None,
            log_path: None,
            error: None,
        });
        h
    }

    #[tokio::test]
    async fn sync_degrades_to_error_when_rclone_missing() {
        let h = make_handler_with_task();
        let resp = h
            .handle(post_req(
                "/api/v1/cloudsync/tasks/sync-test/sync",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        // rclone 未装 → status=error；若装了 → syncing。二者都不 panic，状态非 idle。
        assert_ne!(resp.body["status"], "idle", "sync 后状态应变: {resp:?}");
    }

    #[tokio::test]
    async fn pause_sets_paused_status() {
        let h = make_handler_with_task();
        let resp = h
            .handle(post_req(
                "/api/v1/cloudsync/tasks/sync-test/pause",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["status"], "paused");
    }

    #[tokio::test]
    async fn sync_missing_returns_404() {
        let h = CloudSyncRouteHandler::new();
        let resp = h
            .handle(post_req(
                "/api/v1/cloudsync/tasks/nope/sync",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn delete_removes_task() {
        let h = make_handler_with_task();
        let resp = h
            .handle(del_req("/api/v1/cloudsync/tasks/sync-test"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["ok"], true);
        assert!(h.tasks_snapshot().is_empty());
    }

    // ---- list / stats 降级不 panic ----

    #[tokio::test]
    async fn list_and_stats_return_empty_without_panicking() {
        let h = CloudSyncRouteHandler::with_empty();
        let resp = h.handle(get_req("/api/v1/cloudsync/tasks")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body.as_array().unwrap().len(), 0);
        let resp = h.handle(get_req("/api/v1/cloudsync/stats")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["total_tasks"], 0);
    }

    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<CloudSyncRouteHandler>();
    }
}
