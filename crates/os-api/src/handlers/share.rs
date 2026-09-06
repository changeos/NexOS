//! `ShareRouteHandler` —— 文件共享（SMB / NFS）的 HTTP→共享管理适配器
//! （规划文档 §3.5 / §3.6）。
//!
//! 定位：把网关 HTTP 请求（`/shares*` / `/api/v1/exports`）翻译为共享管理调用，
//! 返回共享 / NFS 导出列表 JSON。这是 os CLI `share list` 命令对应的后端路由。
//!
//! # 持久化策略（落盘 + 真实 Samba 导出）
//!
//! - 共享配置序列化为 `shares.json` 落盘。目录选择优先级：
//!   1. `/tank/os-data/shares.json`（ZFS 池挂载点，目录存在即用）
//!   2. `./shares.json`（fallback，cwd）
//! - `ShareRouteHandler::new()` 启动时加载该 JSON（缺失/空 → 预置示例数据）；
//!   `POST` / `DELETE` 同步写回 JSON，**重启不丢**。
//! - `POST /shares` 创建 SMB 共享时，经 `spawn_blocking` 真实写 `/etc/samba/smb.conf`
//!   的 `[<name>]` section（`sudo tee`）+ `sudo smbcontrol all reload-config`（失败再
//!   `sudo systemctl restart smbd`）；`DELETE` 移除对应 section + reload。
//! - **降级**：smb.conf 写入/reload 失败**不 panic**，错误记入 `ShareInfo::last_error`，
//!   共享配置仍落盘 JSON。系统调用经 `std::env::var("NEXOS_APPLY_SYSTEM").or_else(|_| std::env::var("OS_APPLY_SYSTEM"))` 显式开关
//!   （未设置时仅落盘，不触碰系统；生产部署设 `OS_APPLY_SYSTEM=1` 启用真实导出）。
//!
//! # 路由表
//!
//! | method | path                | 动作 |
//! |--------|---------------------|------|
//! | GET    | `/shares`           | 列出共享 |
//! | POST   | `/shares`           | 创建共享 |
//! | DELETE | `/shares/:id`       | 删除共享 |
//! | GET    | `/api/v1/exports`   | 列出 NFS 导出 |
//!
//! # 路径参数
//!
//! 网关 dispatch 当前不向 handler 传递 `PathParams`，故 `handle` 从 `req.path`
//! 字符串按段解析（先 `split('?')` 剥离 query，再 `split('/')` 取段；参考
//! `compute.rs` 的 `path_segments` 模式）。

use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// DTO（共享 / NFS 导出——JSON 结构与真实后端对齐，便于后续无缝替换）
// ----------------------------------------------------------------------------

/// 单个文件共享的对外表示（SMB / NFS / WebDAV 等）。
///
/// 字段命名与 `os_protocols::common::Share` 对齐（`id` / `name` / `protocol` /
/// `path` / `read_only` / `enabled`），故未来切真实后端时 JSON 结构不变。
/// 新增 `comment` / `allowed_users` / `writable` 字段（均带 `#[serde(default)]`，
/// 旧 JSON 可平滑兼容），用于真实 Samba 导出与用户权限联动。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareInfo {
    /// 共享 ID
    pub id: String,
    /// 共享名（对外展示，亦作 smb.conf 段名）
    pub name: String,
    /// 协议（`smb` / `nfs` / `webdav` / ...）
    pub protocol: String,
    /// 共享的数据集路径（如 `/tank/media`）
    pub path: String,
    /// 是否只读
    pub read_only: bool,
    /// 是否启用
    pub enabled: bool,
    /// 备注（smb.conf `comment` 行；默认空）
    #[serde(default)]
    pub comment: String,
    /// 允许访问的用户名列表（smb.conf `valid users`；空 = 全部允许）
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// 是否可写（vs readonly；smb.conf 由 `read_only` 派生，此字段为元数据）
    #[serde(default)]
    pub writable: bool,
    /// 最近一次系统侧（smb.conf 写入/reload）的错误信息（None = 无异常）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// 单个 NFS 导出的对外表示。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NfsExport {
    /// 导出路径（如 `/tank/media`）
    pub path: String,
    /// 允许访问的客户端（CIDR / 主机名；`*` 表示不限）
    pub client: String,
    /// 权限（`ro` / `rw`）
    pub options: String,
}

// ----------------------------------------------------------------------------
// ShareRouteHandler
// ----------------------------------------------------------------------------

/// 文件共享路由处理器——HTTP 边界适配到**落盘共享管理 + 真实 Samba 导出**。
///
/// 持有一份 `Mutex<Vec<ShareInfo>>`（共享列表）+ `Mutex<Vec<NfsExport>>`
/// （NFS 导出列表）+ 可选 `persist_path`（落盘路径）。`new()` 加载 JSON（或示例
/// 数据）；`POST` / `DELETE` 同步写回 JSON，并对 SMB 共享真实操作 smb.conf。
/// `with_state(...)` 供测试注入纯内存态（`persist_path = None`，不落盘、不触系统）。
pub struct ShareRouteHandler {
    shares: Mutex<Vec<ShareInfo>>,
    exports: Mutex<Vec<NfsExport>>,
    /// 落盘路径（`None` = 纯内存态，测试用；写操作不触盘、不调系统命令）
    persist_path: Option<String>,
}

impl ShareRouteHandler {
    /// 构造 handler：加载 `shares.json`（缺失/空 → 示例数据），开启落盘。
    #[must_use]
    pub fn new() -> Self {
        let path = shares_file_path();
        let shares = load_shares_from(&path);
        let shares = if shares.is_empty() {
            default_shares()
        } else {
            shares
        };
        Self {
            shares: Mutex::new(shares),
            exports: Mutex::new(default_exports()),
            persist_path: Some(path),
        }
    }

    /// 用指定共享 / 导出列表构造（**纯内存态**：测试注入空列表或定制数据，
    /// 不落盘、不触系统命令）。
    #[must_use]
    pub fn with_state(shares: Vec<ShareInfo>, exports: Vec<NfsExport>) -> Self {
        Self {
            shares: Mutex::new(shares),
            exports: Mutex::new(exports),
            persist_path: None,
        }
    }

    /// 用指定共享 / 导出列表 + 显式落盘路径构造（持久化测试用）。
    #[must_use]
    pub fn with_state_path(shares: Vec<ShareInfo>, exports: Vec<NfsExport>, path: String) -> Self {
        Self {
            shares: Mutex::new(shares),
            exports: Mutex::new(exports),
            persist_path: Some(path),
        }
    }

    /// 当前共享列表快照（测试 / 诊断用）。
    #[must_use]
    pub fn shares_snapshot(&self) -> Vec<ShareInfo> {
        self.shares.lock().expect("shares poisoned").clone()
    }

    /// 当前落盘路径（诊断用；纯内存态返回 `None`）。
    #[must_use]
    pub fn persist_path(&self) -> Option<&str> {
        self.persist_path.as_deref()
    }

    /// 同步把当前共享列表写回 JSON（仅当 `persist_path` 为 `Some`）。
    fn persist(&self) {
        if let Some(path) = &self.persist_path {
            let list = self.shares.lock().expect("shares poisoned").clone();
            if let Err(e) = save_shares_to(path, &list) {
                eprintln!("[share] 落盘失败 {path}: {e}");
            }
        }
    }

    /// 是否允许真实操作系统（smb.conf / useradd 等）。由环境变量显式开关，
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

impl Default for ShareRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RouteHandler for ShareRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec(HttpMethod::Get, "/shares", false, vec![]),
            spec(HttpMethod::Post, "/shares", true, vec!["admin".into()]),
            spec(
                HttpMethod::Delete,
                "/shares/:id",
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Get, "/api/v1/exports", false, vec![]),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            // —— GET /shares —— 列出共享
            (HttpMethod::Get, ["shares"]) => {
                let list = self.shares.lock().expect("shares poisoned").clone();
                Ok(ok_json(serde_json::to_value(&list).map_err(map_json_err)?))
            }

            // —— POST /shares —— 创建共享（body: ShareInfo）
            (HttpMethod::Post, ["shares"]) => {
                let mut info: ShareInfo = serde_json::from_value(req.body)
                    .map_err(|e| ApiGatewayError::Internal(format!("解析共享请求体失败: {e}")))?;
                // 权限联动：allowed_users 中的用户名必须存在于用户列表
                let known = crate::handlers::user::known_usernames();
                if let Err(msg) = validate_allowed_users(&info.allowed_users, &known) {
                    return Ok(error_response(400, &msg));
                }
                // 落盘（按 id 覆盖或新增）
                {
                    let mut list = self.shares.lock().expect("shares poisoned");
                    match list.iter().position(|s| s.id == info.id) {
                        Some(i) => list[i] = info.clone(),
                        None => list.push(info.clone()),
                    }
                }
                self.persist();
                // 真实 Samba 导出（降级不 panic）
                if Self::system_effects_enabled()
                    && info.enabled
                    && info.protocol.eq_ignore_ascii_case("smb")
                {
                    let info_for_smb = info.clone();
                    let outcome =
                        tokio::task::spawn_blocking(move || apply_smb_share_sync(&info_for_smb))
                            .await;
                    let err = match outcome {
                        Ok(Ok(())) => None,
                        Ok(Err(e)) => Some(format!("samba: {e}")),
                        Err(e) => Some(format!("samba join: {e}")),
                    };
                    if let Some(e) = err {
                        info.last_error = Some(e);
                        let mut list = self.shares.lock().expect("shares poisoned");
                        if let Some(i) = list.iter().position(|s| s.id == info.id) {
                            list[i] = info.clone();
                        }
                        drop(list);
                        self.persist();
                    }
                }
                Ok(ApiResponse {
                    status: 201,
                    body: serde_json::to_value(&info).map_err(map_json_err)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— DELETE /shares/:id —— 删除共享（不存在返回 404 body）
            (HttpMethod::Delete, ["shares", id]) => {
                let removed: Option<ShareInfo>;
                {
                    let mut list = self.shares.lock().expect("shares poisoned");
                    match list.iter().position(|s| s.id == *id) {
                        Some(i) => removed = Some(list.remove(i)),
                        None => removed = None,
                    }
                }
                let Some(share) = removed else {
                    return Ok(error_response(404, &format!("共享不存在: {id}")));
                };
                self.persist();
                // 从 smb.conf 移除 section + reload（降级不 panic）
                if Self::system_effects_enabled() && share.protocol.eq_ignore_ascii_case("smb") {
                    let name = share.name.clone();
                    let _ = tokio::task::spawn_blocking(move || remove_smb_share_sync(&name)).await;
                }
                Ok(ApiResponse {
                    status: 204,
                    body: serde_json::Value::Null,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/exports —— 列出 NFS 导出
            (HttpMethod::Get, ["api", "v1", "exports"]) => {
                let list = self.exports.lock().expect("exports poisoned").clone();
                Ok(ok_json(serde_json::to_value(&list).map_err(map_json_err)?))
            }

            // —— 未覆盖路由 —— 兜底 404（Ok，非 Err，便于上层定位）
            _ => Ok(error_response(404, "share: 未匹配的路由")),
        }
    }
}

// ----------------------------------------------------------------------------
// smb.conf 生成（pub 纯函数 + section 替换）
// ----------------------------------------------------------------------------

/// 生成单个共享对应的 smb.conf `[<name>]` 段文本（含尾部换行）。
///
/// 输出字段顺序遵循 Samba 推荐：`path / comment / read only / browsable /
/// valid users`。`read only` 由 `ShareInfo::read_only` 派生；`valid users` 由
/// `allowed_users` 空格拼接（空则省略，表示不限）。本函数为**纯函数**，可单测。
#[must_use]
pub fn build_smb_config(share: &ShareInfo) -> String {
    let yn = |b: bool| if b { "yes" } else { "no" };
    let mut out = String::new();
    out.push_str(&format!("[{}]\n", share.name));
    out.push_str(&format!("    path = {}\n", share.path));
    if !share.comment.is_empty() {
        out.push_str(&format!("    comment = {}\n", share.comment));
    }
    out.push_str(&format!("    read only = {}\n", yn(share.read_only)));
    out.push_str("    browsable = yes\n");
    if !share.allowed_users.is_empty() {
        out.push_str(&format!(
            "    valid users = {}\n",
            share.allowed_users.join(" ")
        ));
    }
    out
}

/// 在 smb.conf 全文中替换（或追加）一个 `[name]` section。
///
/// 已存在同名 section 时整段移除后再追加 `new_section`；不存在则直接追加。
/// 传入 `new_section = ""` 即为**移除**该 section。纯函数，可单测。
#[must_use]
pub fn replace_or_append_section(conf: &str, name: &str, new_section: &str) -> String {
    let header = format!("[{name}]");
    let mut out = String::new();
    let mut skipping = false;
    for line in conf.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            // 任一 section 头都会终止"跳过"状态；只有目标头才开启跳过
            skipping = trimmed == header;
        }
        if skipping {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(new_section);
    out
}

// ----------------------------------------------------------------------------
// 系统侧同步操作（smb.conf 写入 / smbd reload；均降级返回 Result）
// ----------------------------------------------------------------------------

/// smb.conf 路径。
fn smb_conf_path() -> &'static str {
    "/etc/samba/smb.conf"
}

/// 读当前 smb.conf 全文（`sudo cat`）；失败返回空串（按"无既有配置"处理）。
fn read_smb_conf_sync() -> String {
    std::process::Command::new("sudo")
        .args(["cat", smb_conf_path()])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// 把全文写回 smb.conf（`sudo tee`，覆盖）。
fn write_smb_conf_sync(content: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("sudo")
        .args(["tee", smb_conf_path()])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn tee: {e}"))?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(content.as_bytes())
            .map_err(|e| format!("write smb.conf: {e}"))?;
    }
    let status = child.wait().map_err(|e| format!("wait tee: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("tee 退出非零: {status}"))
    }
}

/// 热重载 smbd：先 `smbcontrol all reload-config`，失败再 `systemctl restart smbd`。
fn reload_smbd_sync() -> Result<(), String> {
    let primary = std::process::Command::new("sudo")
        .args(["smbcontrol", "all", "reload-config"])
        .output()
        .map_err(|e| format!("spawn smbcontrol: {e}"))?;
    if primary.status.success() {
        return Ok(());
    }
    let fallback = std::process::Command::new("sudo")
        .args(["systemctl", "restart", "smbd"])
        .output()
        .map_err(|e| format!("spawn systemctl: {e}"))?;
    if fallback.status.success() {
        Ok(())
    } else {
        Err(format!(
            "reload smbd 失败: smbcontrol={} systemd={}",
            primary.status.code().unwrap_or(-1),
            fallback.status.code().unwrap_or(-1)
        ))
    }
}

/// 写入/更新一个共享到 smb.conf 并 reload（同步；失败返回 Err，不 panic）。
fn apply_smb_share_sync(share: &ShareInfo) -> Result<(), String> {
    let cur = read_smb_conf_sync();
    let section = build_smb_config(share);
    let new_conf = replace_or_append_section(&cur, &share.name, &section);
    write_smb_conf_sync(&new_conf)?;
    reload_smbd_sync()?;
    Ok(())
}

/// 从 smb.conf 移除一个共享 section 并 reload（同步；失败返回 Err，不 panic）。
fn remove_smb_share_sync(name: &str) -> Result<(), String> {
    let cur = read_smb_conf_sync();
    let new_conf = replace_or_append_section(&cur, name, "");
    write_smb_conf_sync(&new_conf)?;
    reload_smbd_sync()?;
    Ok(())
}

// ----------------------------------------------------------------------------
// 持久化辅助（JSON 落盘）
// ----------------------------------------------------------------------------

/// 解析共享配置落盘路径：`/tank/os-data` 目录存在则用其下 `shares.json`，
/// 否则 fallback 到 cwd 下 `./shares.json`。
fn shares_file_path() -> String {
    let dir = "/tank/os-data";
    if Path::new(dir).is_dir() {
        format!("{dir}/shares.json")
    } else {
        "./shares.json".to_string()
    }
}

/// 从 JSON 文件加载共享列表（缺失/解析失败 → 空列表，由调用方判空后填默认）。
fn load_shares_from(path: &str) -> Vec<ShareInfo> {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// 把共享列表写回 JSON 文件（覆盖；自动建父目录）。
fn save_shares_to(path: &str, list: &[ShareInfo]) -> std::io::Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(list).map_err(std::io::Error::other)?;
    std::fs::write(path, body)
}

// ----------------------------------------------------------------------------
// 权限联动辅助
// ----------------------------------------------------------------------------

/// 校验 `allowed_users` 中的用户名是否全部存在于已知用户集合。
///
/// - `allowed_users` 为空 → 直接通过（空 = 全部允许）。
/// - 已知用户集合为空（无法判定，如用户表尚未落盘）→ 降级通过，不阻断创建。
/// - 否则任一用户名不在集合中即返回 `Err(提示)`，调用方据此刻意返回 400。
fn validate_allowed_users(
    allowed: &[String],
    known: &std::collections::HashSet<String>,
) -> Result<(), String> {
    if allowed.is_empty() || known.is_empty() {
        return Ok(());
    }
    for u in allowed {
        if !known.contains(u) {
            return Err(format!("用户不存在，无法加入 allowed_users: {u}"));
        }
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// 内部辅助
// ----------------------------------------------------------------------------

/// 构造一条 [`RouteSpec`]（component 固定 `share`）。
fn spec(
    method: HttpMethod,
    path: &str,
    requires_auth: bool,
    required_roles: Vec<String>,
) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: "share".to_string(),
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
/// 例：`/shares/abc?x=1` → `["shares", "abc"]`。
/// 与 `compute.rs::path_segments` 同款实现（handler 间不共享私有 fn，按模块内复刻）。
fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

/// 默认示例共享列表（让 `GET /shares` 首次即返回非空）。
fn default_shares() -> Vec<ShareInfo> {
    vec![
        ShareInfo {
            id: "media".into(),
            name: "media".into(),
            protocol: "smb".into(),
            path: "/tank/media".into(),
            read_only: false,
            enabled: true,
            comment: "媒体库".into(),
            allowed_users: Vec::new(),
            writable: true,
            last_error: None,
        },
        ShareInfo {
            id: "backups".into(),
            name: "backups".into(),
            protocol: "nfs".into(),
            path: "/tank/backups".into(),
            read_only: true,
            enabled: true,
            comment: "备份".into(),
            allowed_users: Vec::new(),
            writable: false,
            last_error: None,
        },
    ]
}

/// 默认示例 NFS 导出列表。
fn default_exports() -> Vec<NfsExport> {
    vec![NfsExport {
        path: "/tank/backups".into(),
        client: "192.168.1.0/24".into(),
        options: "ro".into(),
    }]
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个空状态 handler（纯内存态；测试 list 空态 / create / delete 干净场景）。
    fn empty_handler() -> ShareRouteHandler {
        ShareRouteHandler::with_state(Vec::new(), Vec::new())
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
    async fn routes_declares_four_endpoints() {
        let h = ShareRouteHandler::new();
        let routes = h.routes().await;
        assert_eq!(routes.len(), 4);
        assert!(routes.iter().all(|r| r.handler_component == "share"));
        let pairs: Vec<(HttpMethod, &str)> =
            routes.iter().map(|r| (r.method, r.path.as_str())).collect();
        assert!(pairs.contains(&(HttpMethod::Get, "/shares")));
        assert!(pairs.contains(&(HttpMethod::Post, "/shares")));
        assert!(pairs.contains(&(HttpMethod::Delete, "/shares/:id")));
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/exports")));
        // 写操作要求 admin
        let post = routes
            .iter()
            .find(|r| r.method == HttpMethod::Post && r.path == "/shares")
            .unwrap();
        assert!(post.requires_auth);
        assert_eq!(post.required_roles, vec!["admin".to_string()]);
    }

    #[tokio::test]
    async fn get_shares_default_returns_two_items() {
        let h = ShareRouteHandler::new();
        let resp = h.handle(get_req("/shares")).await.unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().expect("body 为数组");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["id"], "media");
        assert_eq!(arr[0]["protocol"], "smb");
        assert_eq!(arr[1]["id"], "backups");
        assert_eq!(arr[1]["protocol"], "nfs");
    }

    #[tokio::test]
    async fn get_shares_empty_returns_empty_array() {
        let h = empty_handler();
        let resp = h.handle(get_req("/shares")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, serde_json::json!([]));
    }

    #[tokio::test]
    async fn get_shares_strips_query_string() {
        let h = ShareRouteHandler::new();
        let resp = h.handle(get_req("/shares?verbose=1")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn post_shares_creates_and_returns_201() {
        let h = empty_handler();
        let req = ApiRequest {
            method: HttpMethod::Post,
            path: "/shares".into(),
            headers: serde_json::json!({}),
            body: serde_json::json!({
                "id": "photos",
                "name": "photos",
                "protocol": "smb",
                "path": "/tank/photos",
                "read_only": false,
                "enabled": true,
            }),
            auth: None,
        };
        let resp = h.handle(req).await.expect("create 应成功");
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["id"], "photos");
        // 列表现在含新共享
        assert_eq!(h.shares_snapshot().len(), 1);
    }

    #[tokio::test]
    async fn post_shares_invalid_body_returns_err() {
        let h = empty_handler();
        let req = ApiRequest {
            method: HttpMethod::Post,
            path: "/shares".into(),
            headers: serde_json::json!({}),
            body: serde_json::json!({ "id": "x" }), // 缺字段
            auth: None,
        };
        let err = h.handle(req).await.unwrap_err();
        assert!(matches!(err, ApiGatewayError::Internal(_)));
    }

    #[tokio::test]
    async fn delete_share_existing_returns_204() {
        // 用 with_state 注入纯内存态，避免触发落盘/系统命令
        let h = ShareRouteHandler::with_state(default_shares(), default_exports());
        let req = ApiRequest {
            method: HttpMethod::Delete,
            path: "/shares/media".into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        };
        let resp = h.handle(req).await.unwrap();
        assert_eq!(resp.status, 204);
        assert_eq!(resp.body, serde_json::Value::Null);
        assert_eq!(h.shares_snapshot().len(), 1); // 剩 backups
    }

    #[tokio::test]
    async fn delete_share_missing_returns_404_body() {
        let h = empty_handler();
        let req = ApiRequest {
            method: HttpMethod::Delete,
            path: "/shares/nope".into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        };
        let resp = h.handle(req).await.unwrap();
        assert_eq!(resp.status, 404);
        assert!(resp.body["error"].as_str().unwrap().contains("不存在"));
    }

    #[tokio::test]
    async fn get_exports_returns_array() {
        let h = ShareRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/exports")).await.unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["path"], "/tank/backups");
        assert_eq!(arr[0]["client"], "192.168.1.0/24");
    }

    #[tokio::test]
    async fn unmatched_route_returns_404_body() {
        let h = ShareRouteHandler::new();
        let resp = h.handle(get_req("/shares/foo/bar")).await.unwrap();
        assert_eq!(resp.status, 404);
        assert!(resp.body["error"].as_str().unwrap().contains("未匹配"));
    }

    #[test]
    fn path_segments_parses_correctly() {
        assert_eq!(path_segments("/shares"), vec!["shares"]);
        assert_eq!(path_segments("/shares/abc?x=1"), vec!["shares", "abc"]);
        assert_eq!(
            path_segments("/api/v1/exports"),
            vec!["api", "v1", "exports"]
        );
        assert!(path_segments("/").is_empty());
    }

    #[test]
    fn share_info_round_trips_serde() {
        let s = ShareInfo {
            id: "x".into(),
            name: "X".into(),
            protocol: "smb".into(),
            path: "/p".into(),
            read_only: true,
            enabled: false,
            comment: String::new(),
            allowed_users: Vec::new(),
            writable: false,
            last_error: None,
        };
        let v = serde_json::to_value(&s).unwrap();
        let back: ShareInfo = serde_json::from_value(v).unwrap();
        assert_eq!(back.id, "x");
        assert!(back.read_only);
        assert!(!back.enabled);
    }

    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<ShareRouteHandler>();
    }

    // —— 新增：build_smb_config 纯函数 ——

    #[test]
    fn build_smb_config_contains_required_lines() {
        let s = ShareInfo {
            id: "m".into(),
            name: "media".into(),
            protocol: "smb".into(),
            path: "/tank/media".into(),
            read_only: false,
            enabled: true,
            comment: "媒体库".into(),
            allowed_users: vec!["alice".into(), "bob".into()],
            writable: true,
            last_error: None,
        };
        let conf = build_smb_config(&s);
        assert!(conf.contains("[media]"), "conf={conf}");
        assert!(conf.contains("path = /tank/media"));
        assert!(conf.contains("read only = no"));
        assert!(conf.contains("browsable = yes"));
        assert!(conf.contains("valid users = alice bob"));
        assert!(conf.contains("comment = 媒体库"));
    }

    #[test]
    fn build_smb_config_omits_optional_lines_when_empty() {
        let s = ShareInfo {
            id: "x".into(),
            name: "ro-share".into(),
            protocol: "smb".into(),
            path: "/tank/ro".into(),
            read_only: true,
            enabled: true,
            comment: String::new(),
            allowed_users: Vec::new(),
            writable: false,
            last_error: None,
        };
        let conf = build_smb_config(&s);
        assert!(conf.contains("read only = yes"));
        assert!(!conf.contains("valid users"));
        assert!(!conf.contains("comment ="));
    }

    // —— 新增：section 替换 ——

    #[test]
    fn replace_or_append_section_appends_when_absent() {
        let conf = "[global]\n    workgroup = WG\n";
        let out = replace_or_append_section(conf, "media", "[media]\n    path = /x\n");
        assert!(out.contains("[global]"));
        assert!(out.contains("[media]"));
        assert!(out.contains("path = /x"));
    }

    #[test]
    fn replace_or_append_section_replaces_existing() {
        let conf =
            "[global]\n    workgroup = WG\n\n[media]\n    path = /old\n\n[docs]\n    path = /d\n";
        let out = replace_or_append_section(conf, "media", "[media]\n    path = /new\n");
        assert!(out.contains("path = /new"));
        assert!(!out.contains("/old"));
        // 其它 section 保留
        assert!(out.contains("[docs]"));
        assert!(out.contains("path = /d"));
    }

    #[test]
    fn replace_or_append_section_with_empty_removes() {
        let conf = "[global]\n\n[media]\n    path = /x\n";
        let out = replace_or_append_section(conf, "media", "");
        assert!(!out.contains("[media]"));
        assert!(!out.contains("path = /x"));
        assert!(out.contains("[global]"));
    }

    // —— 新增：shares.json 落盘 roundtrip ——

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
        let path = unique_tmp_path("shares");
        let _ = std::fs::remove_file(&path);
        // with_state_path 开启落盘；POST 一个 nfs 共享（避免触发 smb 系统命令）
        let h = ShareRouteHandler::with_state_path(Vec::new(), Vec::new(), path.clone());
        let req = ApiRequest {
            method: HttpMethod::Post,
            path: "/shares".into(),
            headers: serde_json::json!({}),
            body: serde_json::json!({
                "id": "photos",
                "name": "photos",
                "protocol": "nfs",
                "path": "/tank/photos",
                "read_only": false,
                "enabled": true,
            }),
            auth: None,
        };
        let resp = h.handle(req).await.expect("create 应成功");
        assert_eq!(resp.status, 201);
        // 文件确实写入
        assert!(Path::new(&path).exists(), "shares.json 应已落盘");
        // 读回内容一致
        let loaded = load_shares_from(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "photos");
        assert_eq!(loaded[0].protocol, "nfs");
        let _ = std::fs::remove_file(&path);
    }

    // —— 新增：权限联动 ——

    #[test]
    fn validate_allowed_users_passes_when_known() {
        let mut known = std::collections::HashSet::new();
        known.insert("alice".to_string());
        known.insert("bob".to_string());
        assert!(validate_allowed_users(&["alice".into()], &known).is_ok());
        assert!(validate_allowed_users(&["alice".into(), "bob".into()], &known).is_ok());
    }

    #[test]
    fn validate_allowed_users_rejects_unknown() {
        let mut known = std::collections::HashSet::new();
        known.insert("alice".to_string());
        let err = validate_allowed_users(&["alice".into(), "nobody".into()], &known).unwrap_err();
        assert!(err.contains("nobody"));
    }

    #[test]
    fn validate_allowed_users_empty_passes() {
        let known = std::collections::HashSet::<String>::new();
        assert!(validate_allowed_users(&[], &known).is_ok());
        // 空已知集合（无法判定）也降级通过
        assert!(validate_allowed_users(&["x".into()], &std::collections::HashSet::new()).is_ok());
    }
}
