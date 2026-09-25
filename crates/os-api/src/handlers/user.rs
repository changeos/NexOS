//! `UserRouteHandler` —— 用户管理的 HTTP→用户管理适配器
//! （规划文档 §3.18 / §3.6）。
//!
//! 定位：把网关 HTTP 请求（`/api/v1/users*`）翻译为用户管理调用，返回用户列表
//! JSON。这是 os CLI `user list` 命令对应的后端路由。
//!
//! # 持久化策略（落盘 + 系统用户创建）
//!
//! - 用户配置序列化为 `users.json` 落盘。目录选择优先级：
//!   1. `/tank/os-data/users.json`（ZFS 池挂载点，目录存在即用）
//!   2. `./users.json`（fallback，cwd）
//! - `UserRouteHandler::new()` 启动时加载该 JSON（缺失/空 → 预置示例数据）；
//!   `POST` / `DELETE` 同步写回 JSON，**重启不丢**。
//! - `POST /api/v1/users` 创建非访客用户时，经 `spawn_blocking` 真实执行
//!   `sudo useradd -m -s /bin/bash [-G groups] <username>`；`DELETE` 执行
//!   `sudo userdel -r <username>`。
//! - **降级**：useradd/userdel 失败**不 panic**，错误记入 `UserInfo::last_error`，
//!   用户配置仍落盘 JSON。系统调用经 `std::env::var("NEXOS_APPLY_SYSTEM").or_else(|_| std::env::var("OS_APPLY_SYSTEM"))` 显式开关
//!   （未设置时仅落盘，不触碰系统；生产部署设 `OS_APPLY_SYSTEM=1` 启用真实创建）。
//!
//! # 路由表
//!
//! | method | path                  | 动作 |
//! |--------|-----------------------|------|
//! | GET    | `/api/v1/users`       | 列出用户 |
//! | POST   | `/api/v1/users`       | 创建用户 |
//! | DELETE | `/api/v1/users/:id`   | 删除用户 |
//!
//! # 路径参数
//!
//! 网关 dispatch 当前不向 handler 传递 `PathParams`，故 `handle` 从 `req.path`
//! 字符串按段解析（先 `split('?')` 剥离 query，再 `split('/')` 取段；参考
//! `compute.rs` 的 `path_segments` 模式）。

use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// DTO（用户——JSON 结构与真实后端对齐，便于后续无缝替换）
// ----------------------------------------------------------------------------

/// 单个用户的对外表示。
///
/// 字段覆盖常见用户属性（`id` / `name` / 角色 / 是否启用 / 是否访客），
/// 与 `os_guest::model::GuestIdentity` 的核心字段对齐（`id` / 角色 / 状态），
/// 故未来切真实后端时 JSON 结构可平滑迁移。新增 `role` / `groups` /
/// `system_user` / `email` / `created_at` 字段（均带 `#[serde(default)]`，旧 JSON
/// 可平滑兼容），用于系统用户创建与共享权限联动。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfo {
    /// 用户 ID（系统用户名或访客 ID；系统用户创建时作为 `useradd` 用户名）
    pub id: String,
    /// 显示名
    pub name: String,
    /// 角色列表（如 `["admin"]` / `["operator"]` / `["guest"]`）
    pub roles: Vec<String>,
    /// 是否启用
    pub enabled: bool,
    /// 是否访客身份（区别于系统用户）
    pub is_guest: bool,
    /// 主角色（`admin` / `user` / `guest`；与 `roles` 互补，便于权限判断）
    #[serde(default)]
    pub role: String,
    /// 用户组（如 `sambashare` / `docker`；useradd `-G` 参数）
    #[serde(default)]
    pub groups: Vec<String>,
    /// 是否已创建系统用户（useradd 成功后置 true）
    #[serde(default)]
    pub system_user: bool,
    /// 邮箱
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub email: String,
    /// 创建时间（ISO 8601）
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub created_at: String,
    /// 最近一次系统侧（useradd/userdel）的错误信息（None = 无异常）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

// ----------------------------------------------------------------------------
// UserRouteHandler
// ----------------------------------------------------------------------------

/// 用户管理路由处理器——HTTP 边界适配到**落盘用户管理 + 系统用户创建**。
///
/// 持有一份 `Mutex<Vec<UserInfo>>`（用户列表）+ 可选 `persist_path`（落盘路径）。
/// `new()` 加载 JSON（或示例数据）；`POST` / `DELETE` 同步写回 JSON，并对非访客
/// 用户真实调 `useradd` / `userdel`。`with_users(...)` 供测试注入纯内存态
/// （`persist_path = None`，不落盘、不触系统）。
pub struct UserRouteHandler {
    users: Mutex<Vec<UserInfo>>,
    /// 落盘路径（`None` = 纯内存态，测试用；写操作不触盘、不调系统命令）
    persist_path: Option<String>,
}

impl UserRouteHandler {
    /// 构造 handler：加载 `users.json`（缺失/空 → 示例数据），开启落盘。
    #[must_use]
    pub fn new() -> Self {
        let path = users_file_path();
        let users = load_users_from(&path);
        let users = if users.is_empty() {
            default_users()
        } else {
            users
        };
        Self {
            users: Mutex::new(users),
            persist_path: Some(path),
        }
    }

    /// 用指定用户列表构造（**纯内存态**：测试注入空列表或定制数据，
    /// 不落盘、不触系统命令）。
    #[must_use]
    pub fn with_users(users: Vec<UserInfo>) -> Self {
        Self {
            users: Mutex::new(users),
            persist_path: None,
        }
    }

    /// 用指定用户列表 + 显式落盘路径构造（持久化测试用）。
    #[must_use]
    pub fn with_users_path(users: Vec<UserInfo>, path: String) -> Self {
        Self {
            users: Mutex::new(users),
            persist_path: Some(path),
        }
    }

    /// 当前用户列表快照（测试 / 诊断用）。
    #[must_use]
    pub fn users_snapshot(&self) -> Vec<UserInfo> {
        self.users.lock().expect("users poisoned").clone()
    }

    /// 当前落盘路径（诊断用；纯内存态返回 `None`）。
    #[must_use]
    pub fn persist_path(&self) -> Option<&str> {
        self.persist_path.as_deref()
    }

    /// 同步把当前用户列表写回 JSON（仅当 `persist_path` 为 `Some`）。
    fn persist(&self) {
        if let Some(path) = &self.persist_path {
            let list = self.users.lock().expect("users poisoned").clone();
            if let Err(e) = save_users_to(path, &list) {
                eprintln!("[user] 落盘失败 {path}: {e}");
            }
        }
    }

    /// 是否允许真实操作系统（useradd / smb.conf 等）。由环境变量显式开关，
    /// 测试 / 默认开发态不触碰系统；生产部署设 `OS_APPLY_SYSTEM=1`。
    fn system_effects_enabled() -> bool {
        matches!(
            std::env::var("NEXOS_APPLY_SYSTEM")
                .or_else(|_| std::env::var("OS_APPLY_SYSTEM"))
                .ok()
                .as_deref(),
            Some("1" | "true" | "yes")
        )
    }
}

impl Default for UserRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RouteHandler for UserRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec(HttpMethod::Get, "/api/v1/users", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/users",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/users/:id",
                true,
                vec!["admin".into()],
            ),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            // —— GET /api/v1/users —— 列出用户
            (HttpMethod::Get, ["api", "v1", "users"]) => {
                let list = self.users.lock().expect("users poisoned").clone();
                Ok(ok_json(serde_json::to_value(&list).map_err(map_json_err)?))
            }

            // —— POST /api/v1/users —— 创建用户（body: UserInfo）
            (HttpMethod::Post, ["api", "v1", "users"]) => {
                let mut info: UserInfo = serde_json::from_value(req.body)
                    .map_err(|e| ApiGatewayError::Internal(format!("解析用户请求体失败: {e}")))?;
                if info.created_at.is_empty() {
                    info.created_at = now_iso();
                }
                // 落盘（按 id 覆盖或新增）
                {
                    let mut list = self.users.lock().expect("users poisoned");
                    match list.iter().position(|u| u.id == info.id) {
                        Some(i) => list[i] = info.clone(),
                        None => list.push(info.clone()),
                    }
                }
                self.persist();
                // 真实系统用户创建（仅非访客；降级不 panic）
                if Self::system_effects_enabled() && !info.is_guest && !info.system_user {
                    let uname = info.id.clone();
                    let groups = info.groups.clone();
                    let outcome = tokio::task::spawn_blocking(move || {
                        create_system_user_sync(&uname, &groups)
                    })
                    .await;
                    let err = match outcome {
                        Ok(Ok(())) => None,
                        Ok(Err(e)) => Some(format!("useradd: {e}")),
                        Err(e) => Some(format!("useradd join: {e}")),
                    };
                    match err {
                        None => {
                            info.system_user = true;
                            let mut list = self.users.lock().expect("users poisoned");
                            if let Some(i) = list.iter().position(|u| u.id == info.id) {
                                list[i] = info.clone();
                            }
                            drop(list);
                            self.persist();
                        }
                        Some(e) => {
                            info.last_error = Some(e);
                            let mut list = self.users.lock().expect("users poisoned");
                            if let Some(i) = list.iter().position(|u| u.id == info.id) {
                                list[i] = info.clone();
                            }
                            drop(list);
                            self.persist();
                        }
                    }
                }
                Ok(ApiResponse {
                    status: 201,
                    body: serde_json::to_value(&info).map_err(map_json_err)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— DELETE /api/v1/users/:id —— 删除用户（不存在返回 404 body）
            (HttpMethod::Delete, ["api", "v1", "users", id]) => {
                let removed: Option<UserInfo>;
                {
                    let mut list = self.users.lock().expect("users poisoned");
                    match list.iter().position(|u| u.id == *id) {
                        Some(i) => removed = Some(list.remove(i)),
                        None => removed = None,
                    }
                }
                let Some(user) = removed else {
                    return Ok(error_response(404, &format!("用户不存在: {id}")));
                };
                self.persist();
                // 系统用户删除（仅非访客且曾创建过系统用户；降级不 panic）
                if Self::system_effects_enabled() && !user.is_guest && user.system_user {
                    let uname = user.id.clone();
                    let _ =
                        tokio::task::spawn_blocking(move || delete_system_user_sync(&uname)).await;
                }
                Ok(ApiResponse {
                    status: 204,
                    body: serde_json::Value::Null,
                    headers: serde_json::json!({}),
                })
            }

            // —— 未覆盖路由 —— 兜底 404
            _ => Ok(error_response(404, "user: 未匹配的路由")),
        }
    }
}

// ----------------------------------------------------------------------------
// 系统侧同步操作（useradd / userdel；均降级返回 Result）
// ----------------------------------------------------------------------------

/// 创建系统用户：`sudo useradd -m -s /bin/bash [-G g1,g2] <username>`。
///
/// 成功返回 `Ok(())`；失败返回 `Err(诊断串)`，调用方降级处理（不 panic）。
fn create_system_user_sync(username: &str, groups: &[String]) -> Result<(), String> {
    let mut cmd = std::process::Command::new("sudo");
    cmd.args(["useradd", "-m", "-s", "/bin/bash"]);
    if !groups.is_empty() {
        cmd.args(["-G", &groups.join(",")]);
    }
    cmd.arg(username);
    let out = cmd.output().map_err(|e| format!("spawn useradd: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "useradd 退出 {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// 删除系统用户：`sudo userdel -r <username>`。
fn delete_system_user_sync(username: &str) -> Result<(), String> {
    let out = std::process::Command::new("sudo")
        .args(["userdel", "-r", username])
        .output()
        .map_err(|e| format!("spawn userdel: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "userdel 退出 {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

// ----------------------------------------------------------------------------
// 持久化辅助（JSON 落盘）——部分对外（供 share.rs 权限联动查询）
// ----------------------------------------------------------------------------

/// 解析用户配置落盘路径：`/tank/os-data` 目录存在则用其下 `users.json`，
/// 否则 fallback 到 cwd 下 `./users.json`。
fn users_file_path() -> String {
    let dir = "/tank/os-data";
    if Path::new(dir).is_dir() {
        format!("{dir}/users.json")
    } else {
        "./users.json".to_string()
    }
}

/// 从 JSON 文件加载用户列表（缺失/解析失败 → 空列表，由调用方判空后填默认）。
fn load_users_from(path: &str) -> Vec<UserInfo> {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// 把用户列表写回 JSON 文件（覆盖；自动建父目录）。
fn save_users_to(path: &str, list: &[UserInfo]) -> std::io::Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(list).map_err(std::io::Error::other)?;
    std::fs::write(path, body)
}

/// 当前已知用户名集合（读 `users.json`；供共享 handler 权限联动校验
/// `allowed_users`）。
///
/// 文件缺失/解析失败返回空集合（调用方据此降级为"不阻断"）。
#[must_use]
pub fn known_usernames() -> HashSet<String> {
    load_users_from(&users_file_path())
        .into_iter()
        .map(|u| u.id)
        .collect()
}

// ----------------------------------------------------------------------------
// 内部辅助
// ----------------------------------------------------------------------------

/// 构造一条 [`RouteSpec`]（component 固定 `user`）。
fn spec(
    method: HttpMethod,
    path: &str,
    requires_auth: bool,
    required_roles: Vec<String>,
) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: "user".to_string(),
        requires_auth,
        required_roles,
    }
}

/// 构造一个 200 JSON 响应（空 headers）。
fn ok_json(body: serde_json::Value) -> ApiResponse {
    ApiResponse {
        status: 200,
        body,
        headers: serde_json::json!({}),
    }
}

/// 构造一个最小 JSON 错误响应（status 由调用方指定）。
fn error_response(status: u16, msg: &str) -> ApiResponse {
    ApiResponse {
        status,
        body: serde_json::json!({"error": msg}),
        headers: serde_json::json!({}),
    }
}

/// serde 序列化错误 → [`ApiGatewayError`]。
fn map_json_err(e: serde_json::Error) -> ApiGatewayError {
    ApiGatewayError::Internal(format!("响应序列化失败: {e}"))
}

/// 从请求路径中剥离 `?query` 后的纯 path 段（前后空段去除）。
///
/// 例：`/api/v1/users/abc?x=1` → `["api", "v1", "users", "abc"]`。
/// 与 `compute.rs::path_segments` 同款实现（handler 间不共享私有 fn，按模块内复刻）。
fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

/// 当前时间 ISO 8601 字符串（与 notes.rs 同款）。
fn now_iso() -> String {
    use chrono::Local;
    Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

/// 默认示例用户列表（让 `GET /api/v1/users` 首次即返回非空）。
fn default_users() -> Vec<UserInfo> {
    vec![
        UserInfo {
            id: "admin".into(),
            name: "Administrator".into(),
            roles: vec!["admin".into()],
            enabled: true,
            is_guest: false,
            role: "admin".into(),
            groups: vec!["sambashare".into()],
            system_user: false,
            email: "admin@os.local".into(),
            created_at: String::new(),
            last_error: None,
        },
        UserInfo {
            id: "operator".into(),
            name: "Operator".into(),
            roles: vec!["operator".into()],
            enabled: true,
            is_guest: false,
            role: "user".into(),
            groups: vec!["sambashare".into()],
            system_user: false,
            email: String::new(),
            created_at: String::new(),
            last_error: None,
        },
        UserInfo {
            id: "guest".into(),
            name: "Guest".into(),
            roles: vec!["guest".into()],
            enabled: false,
            is_guest: true,
            role: "guest".into(),
            groups: Vec::new(),
            system_user: false,
            email: String::new(),
            created_at: String::new(),
            last_error: None,
        },
    ]
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个空状态 handler（纯内存态）。
    fn empty_handler() -> UserRouteHandler {
        UserRouteHandler::with_users(Vec::new())
    }

    /// 构造一个 GET 请求。
    fn get_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    #[tokio::test]
    async fn routes_declares_three_endpoints() {
        let h = UserRouteHandler::new();
        let routes = h.routes().await;
        assert_eq!(routes.len(), 3);
        assert!(routes.iter().all(|r| r.handler_component == "user"));
        let pairs: Vec<(HttpMethod, &str)> =
            routes.iter().map(|r| (r.method, r.path.as_str())).collect();
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/users")));
        assert!(pairs.contains(&(HttpMethod::Post, "/api/v1/users")));
        assert!(pairs.contains(&(HttpMethod::Delete, "/api/v1/users/:id")));
        // 写操作要求 admin
        let post = routes
            .iter()
            .find(|r| r.method == HttpMethod::Post && r.path == "/api/v1/users")
            .unwrap();
        assert!(post.requires_auth);
        assert_eq!(post.required_roles, vec!["admin".to_string()]);
    }

    #[tokio::test]
    async fn get_users_default_returns_three_items() {
        let h = UserRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/users")).await.unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().expect("body 为数组");
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["id"], "admin");
        assert_eq!(arr[1]["id"], "operator");
        assert_eq!(arr[2]["id"], "guest");
        assert_eq!(arr[2]["is_guest"], true);
    }

    #[tokio::test]
    async fn get_users_empty_returns_empty_array() {
        let h = empty_handler();
        let resp = h.handle(get_req("/api/v1/users")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, serde_json::json!([]));
    }

    #[tokio::test]
    async fn get_users_strips_query_string() {
        let h = UserRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/users?include_disabled=1"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body.as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn post_users_creates_and_returns_201() {
        let h = empty_handler();
        let req = ApiRequest {
            method: HttpMethod::Post,
            path: "/api/v1/users".into(),
            headers: serde_json::json!({}),
            body: serde_json::json!({
                "id": "alice",
                "name": "Alice",
                "roles": ["operator"],
                "enabled": true,
                "is_guest": false,
            }),
            auth: None,
        };
        let resp = h.handle(req).await.expect("create 应成功");
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["id"], "alice");
        assert_eq!(h.users_snapshot().len(), 1);
    }

    #[tokio::test]
    async fn post_users_invalid_body_returns_err() {
        let h = empty_handler();
        let req = ApiRequest {
            method: HttpMethod::Post,
            path: "/api/v1/users".into(),
            headers: serde_json::json!({}),
            body: serde_json::json!({ "id": "x" }), // 缺字段
            auth: None,
        };
        let err = h.handle(req).await.unwrap_err();
        assert!(matches!(err, ApiGatewayError::Internal(_)));
    }

    #[tokio::test]
    async fn delete_user_existing_returns_204() {
        // 用 with_users 注入纯内存态（guest 删除不触系统命令）
        let h = UserRouteHandler::with_users(default_users());
        let req = ApiRequest {
            method: HttpMethod::Delete,
            path: "/api/v1/users/guest".into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        };
        let resp = h.handle(req).await.unwrap();
        assert_eq!(resp.status, 204);
        assert_eq!(resp.body, serde_json::Value::Null);
        assert_eq!(h.users_snapshot().len(), 2);
    }

    #[tokio::test]
    async fn delete_user_missing_returns_404_body() {
        let h = empty_handler();
        let req = ApiRequest {
            method: HttpMethod::Delete,
            path: "/api/v1/users/nope".into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        };
        let resp = h.handle(req).await.unwrap();
        assert_eq!(resp.status, 404);
        assert!(resp.body["error"].as_str().unwrap().contains("不存在"));
    }

    #[tokio::test]
    async fn unmatched_route_returns_404_body() {
        let h = UserRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/users/foo/bar")).await.unwrap();
        assert_eq!(resp.status, 404);
        assert!(resp.body["error"].as_str().unwrap().contains("未匹配"));
    }

    #[test]
    fn path_segments_parses_correctly() {
        assert_eq!(path_segments("/api/v1/users"), vec!["api", "v1", "users"]);
        assert_eq!(
            path_segments("/api/v1/users/abc?x=1"),
            vec!["api", "v1", "users", "abc"]
        );
        assert!(path_segments("/").is_empty());
    }

    #[test]
    fn user_info_round_trips_serde() {
        let u = UserInfo {
            id: "bob".into(),
            name: "Bob".into(),
            roles: vec!["operator".into()],
            enabled: true,
            is_guest: false,
            role: "user".into(),
            groups: vec!["sambashare".into()],
            system_user: true,
            email: "bob@x".into(),
            created_at: "2026-08-12T00:00:00+08:00".into(),
            last_error: None,
        };
        let v = serde_json::to_value(&u).unwrap();
        let back: UserInfo = serde_json::from_value(v).unwrap();
        assert_eq!(back.id, "bob");
        assert_eq!(back.roles, vec!["operator".to_string()]);
        assert!(!back.is_guest);
        assert!(back.system_user);
        assert_eq!(back.groups, vec!["sambashare".to_string()]);
    }

    #[test]
    fn user_info_old_json_backwards_compatible() {
        // 旧格式（仅含原 5 字段）应能反序列化，新字段取默认
        let old = serde_json::json!({
            "id": "x",
            "name": "X",
            "roles": ["operator"],
            "enabled": true,
            "is_guest": false
        });
        let u: UserInfo = serde_json::from_value(old).unwrap();
        assert_eq!(u.id, "x");
        assert!(u.groups.is_empty());
        assert!(!u.system_user);
        assert!(u.role.is_empty());
    }

    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<UserRouteHandler>();
    }

    // —— 新增：users.json 落盘 roundtrip ——

    /// 生成唯一临时文件路径（避免并行测试互扰）。
    fn unique_tmp_path(prefix: &str) -> String {
        static C: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = C.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("/tmp/os-test-{prefix}-{n:x}-{nanos:x}.json")
    }

    #[tokio::test]
    async fn persist_writes_and_reads_back_json() {
        let path = unique_tmp_path("users");
        let _ = std::fs::remove_file(&path);
        // with_users_path 开启落盘；POST 一个访客用户（不触 useradd）
        let h = UserRouteHandler::with_users_path(Vec::new(), path.clone());
        let req = ApiRequest {
            method: HttpMethod::Post,
            path: "/api/v1/users".into(),
            headers: serde_json::json!({}),
            body: serde_json::json!({
                "id": "visitor",
                "name": "Visitor",
                "roles": ["guest"],
                "enabled": true,
                "is_guest": true,
            }),
            auth: None,
        };
        let resp = h.handle(req).await.expect("create 应成功");
        assert_eq!(resp.status, 201);
        // 文件确实写入
        assert!(Path::new(&path).exists(), "users.json 应已落盘");
        // 读回内容一致
        let loaded = load_users_from(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "visitor");
        assert!(loaded[0].is_guest);
        let _ = std::fs::remove_file(&path);
    }
}
