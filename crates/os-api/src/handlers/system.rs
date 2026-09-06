//! 系统状态聚合 + 健康检查 + CPU 虚拟化检测 + 广告位管理 RouteHandler
//! （规划文档 §3.6 / §9.1#10——各业务组件经 `RouteHandler` 注册路由）。
//!
//! 提供 4 条只读 GET 路由 + 3 条广告位管理路由：
//! - `GET /status` —— 聚合系统状态（对齐 `os_mobile::SystemStatus` 客户端契约：
//!   `hostname`/`version`/`capacity`/`health`/`node_count` + 向前兼容扩展
//!   `cpu_virt`/`uptime`）；
//! - `GET /healthz` —— 简单健康检查（`{"status":"ok"}`），供探针 / k8s liveness 用；
//! - `GET /api/v1/version` —— 网关自身版本（`name` + `version`）；
//! - `GET /api/v1/system/virt-check` —— CPU 虚拟化能力详查（[`VirtCheckResult`] 全字段
//!   + `to_user_diagnostic()` 中文诊断字符串 + `is_usable` 综合判定）。
//!
//! # 实现要点
//!
//! - CPU 虚拟化检测来自 [`os_compute::detect_virt_capability`]（读 `/proc/cpuinfo` +
//!   `/dev/kvm` + `/proc/modules`，快速同步 IO）；本 handler 在 async 上下文中经
//!   `tokio::task::spawn_blocking` 调用，避免阻塞 tokio runtime。
//! - 路由匹配只看 `method == GET` 且 `path` 去掉 query 后等于声明路径（与
//!   `routing.rs` 的 specificity 匹配语义一致；query 参数不影响 dispatch）。

use async_trait::async_trait;

use os_compute::{detect_virt_capability, VirtCheckResult};

use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};
use crate::ApiGatewayError;

// ----------------------------------------------------------------------------
// 常量路由路径（避免散落字符串字面量，单测也复用）
// ----------------------------------------------------------------------------

/// `GET /status` —— 系统状态聚合。
const PATH_STATUS: &str = "/status";
/// `GET /healthz` —— 健康检查。
const PATH_HEALTHZ: &str = "/healthz";
/// `GET /api/v1/version` —— 网关版本。
const PATH_VERSION: &str = "/api/v1/version";
/// `GET /api/v1/system/virt-check` —— CPU 虚拟化详查。
const PATH_VIRT_CHECK: &str = "/api/v1/system/virt-check";
/// `GET /api/v1/system/ads` —— 获取广告位内容。
const PATH_ADS: &str = "/api/v1/system/ads";
const PATH_RESTART: &str = "/api/v1/system/restart";
/// `POST /api/v1/system/ads` —— 管理广告位（需 admin）。
const PATH_ADS_MANAGE: &str = "/api/v1/system/ads/manage";

/// 本 handler 注册时的组件名（`RouteSpec::handler_component`，统一为 `system`）。
const COMPONENT: &str = "system";

// ----------------------------------------------------------------------------
// 广告位数据
// ----------------------------------------------------------------------------

/// 广告位条目。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AdItem {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub link: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_sort")]
    pub sort: i32,
}

fn default_true() -> bool {
    true
}
fn default_sort() -> i32 {
    0
}

/// 默认广告位内容（首次加载时 seed）。
fn default_ads() -> Vec<AdItem> {
    vec![
        AdItem {
            id: "ad-1".into(),
            text: "🎉 NexOS — 连接 OS，打破信息孤岛".into(),
            link: None,
            icon: Some("🚀".into()),
            enabled: true,
            sort: 0,
        },
        AdItem {
            id: "ad-2".into(),
            text: "💡 支持 ZFS 存储池 · 虚拟机 · Docker · 流媒体转码".into(),
            link: None,
            icon: Some("💾".into()),
            enabled: true,
            sort: 1,
        },
        AdItem {
            id: "ad-3".into(),
            text: "🤖 AI 相册 · 模型管理 · API 网关 — 内置 Qwen3-VL 推理".into(),
            link: None,
            icon: Some("🧠".into()),
            enabled: true,
            sort: 2,
        },
        AdItem {
            id: "ad-4".into(),
            text: "🔒 端到端加密 · 多用户权限 · 数据安全有保障".into(),
            link: None,
            icon: Some("🔐".into()),
            enabled: true,
            sort: 3,
        },
        AdItem {
            id: "ad-5".into(),
            text: "📡 区块链管理 · 一键部署 RPC 节点 + 浏览器".into(),
            link: None,
            icon: Some("⛓️".into()),
            enabled: true,
            sort: 4,
        },
        AdItem {
            id: "ad-6".into(),
            text: "🎬 影院 · 音乐 · 相册 — TMDB 刮削 + AI 智能分类".into(),
            link: None,
            icon: Some("🍿".into()),
            enabled: true,
            sort: 5,
        },
        AdItem {
            id: "ad-7".into(),
            text: "🧩 NexHub — 本地代码仓库，AI 项目留存归档".into(),
            link: Some("/codehub".into()),
            icon: Some("🧩".into()),
            enabled: true,
            sort: 6,
        },
    ]
}

/// 广告位存储（JSON 落盘，参考 shares/users 模式）。
fn ads_file() -> std::path::PathBuf {
    let base = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    let p = std::path::PathBuf::from(base).join("static-dist/../os-data/ads.json");
    if p.exists() {
        return p;
    }
    // fallback
    for candidate in ["/tank/os-data/ads.json", "./ads.json"] {
        let c = std::path::PathBuf::from(candidate);
        if c.exists() || c.parent().map(|d| d.exists()).unwrap_or(false) {
            return c;
        }
    }
    std::path::PathBuf::from("./ads.json")
}

// ----------------------------------------------------------------------------
// SystemRouteHandler
// ----------------------------------------------------------------------------

/// 系统状态聚合 + 健康检查 + CPU 虚拟化检测 + 广告位管理 RouteHandler。
pub struct SystemRouteHandler {
    ads: std::sync::Mutex<Vec<AdItem>>,
}

impl SystemRouteHandler {
    /// 构造。
    pub fn new() -> Self {
        let ads = load_ads();
        Self {
            ads: std::sync::Mutex::new(ads),
        }
    }
}

impl Default for SystemRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

fn load_ads() -> Vec<AdItem> {
    let path = ads_file();
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| default_ads()),
        Err(_) => {
            let d = default_ads();
            let _ = std::fs::write(&path, serde_json::to_string_pretty(&d).unwrap_or_default());
            d
        }
    }
}

fn save_ads(ads: &[AdItem]) {
    let path = ads_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, serde_json::to_string_pretty(ads).unwrap_or_default());
}

#[async_trait]
impl RouteHandler for SystemRouteHandler {
    /// 声明路由（4 GET + 2 广告位管理）。
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec(HttpMethod::Get, PATH_STATUS),
            spec(HttpMethod::Get, PATH_HEALTHZ),
            spec(HttpMethod::Get, PATH_VERSION),
            spec(HttpMethod::Get, PATH_VIRT_CHECK),
            spec(HttpMethod::Get, PATH_ADS),
            crate::gateway::RouteSpec {
                method: HttpMethod::Post,
                path: PATH_RESTART.into(),
                handler_component: COMPONENT.into(),
                requires_auth: true,
                required_roles: vec!["admin".into()],
            },
            crate::gateway::RouteSpec {
                method: HttpMethod::Post,
                path: PATH_ADS_MANAGE.into(),
                handler_component: COMPONENT.into(),
                requires_auth: true,
                required_roles: vec!["admin".into()],
            },
            crate::gateway::RouteSpec {
                method: HttpMethod::Delete,
                path: format!("{}{{id}}", PATH_ADS_MANAGE),
                handler_component: COMPONENT.into(),
                requires_auth: true,
                required_roles: vec!["admin".into()],
            },
        ]
    }

    /// 按 `path`（去掉 query）分发到对应处理器。
    ///
    /// 未命中声明路径时返回 404（`ApiGatewayError::ComponentNotFound` 语义）。
    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let path = req.path.split('?').next().unwrap_or("");
        let method = req.method;
        match (method, path) {
            (_, p) if p == PATH_STATUS => Ok(handle_status().await),
            (_, p) if p == PATH_HEALTHZ => Ok(handle_healthz()),
            (_, p) if p == PATH_VERSION => Ok(handle_version()),
            (_, p) if p == PATH_VIRT_CHECK => handle_virt_check().await,
            (HttpMethod::Get, p) if p == PATH_ADS => {
                let ads = self.ads.lock().expect("ads poisoned");
                let enabled: Vec<&AdItem> = ads.iter().filter(|a| a.enabled).collect();
                Ok(ApiResponse {
                    status: 200,
                    body: serde_json::to_value(&enabled).unwrap_or_default(),
                    headers: serde_json::json!({}),
                })
            }
            // —— POST /system/restart —— 重启：默认软重启 os-api；{scope:"host"}
            // 重启整机（sudo systemctl reboot——113/106 的 oem 有 sudo，经 -S 传密码；
            // aliyun root 直跑无需密码）
            (HttpMethod::Post, p) if p == PATH_RESTART => {
                #[derive(serde::Deserialize)]
                struct RestartBody {
                    #[serde(default)]
                    scope: Option<String>,
                }
                let body: RestartBody =
                    serde_json::from_value(req.body.clone()).unwrap_or(RestartBody { scope: None });
                let host_scope = body.scope.as_deref() == Some("host");

                // 整机重启需要 root：sudo -S 从 NEXOS_SUDO_PASS（env）喂入；未设则 sudo 拒绝（安全默认）
                let script = if host_scope {
                    r#"#!/bin/bash
echo "${NEXOS_SUDO_PASS:?NEXOS_SUDO_PASS not set}" | sudo -S systemctl reboot
"#
                    .to_string()
                } else {
                    "#!/bin/bash
sleep 1; systemctl restart os-api
"
                    .to_string()
                };
                let tmp = format!("/tmp/nexos-restart-{}.sh", std::process::id());
                std::fs::write(&tmp, &script).ok();
                std::process::Command::new("chmod")
                    .args(["+x", &tmp])
                    .status()
                    .ok();
                let tmp2 = tmp.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                    match tokio::process::Command::new("bash")
                        .arg(tmp2)
                        .status()
                        .await
                    {
                        Ok(st) => eprintln!(
                            "[system] 执行重启({}) exit={}",
                            if host_scope { "host" } else { "os-api" },
                            st
                        ),
                        Err(e) => eprintln!("[system] 重启命令失败: {e}"),
                    }
                });
                let (note, delay) = if host_scope {
                    (
                        "整机将在约 1-3 秒后重启（所有服务中断，恢复取决于开机时间）",
                        "1-3s",
                    )
                } else {
                    ("os-api 将在约 1 秒后重启（连接会中断数秒）", "~1s")
                };
                Ok(ok_json(serde_json::json!({
                    "ok": true,
                    "scope": if host_scope { "host" } else { "os-api" },
                    "note": note,
                    "delay": delay,
                })))
            }
            (HttpMethod::Post, p) if p == PATH_ADS_MANAGE => {
                // 添加/更新广告
                #[derive(serde::Deserialize)]
                struct AdBody {
                    text: String,
                    #[serde(default)]
                    link: Option<String>,
                    #[serde(default)]
                    icon: Option<String>,
                    #[serde(default)]
                    enabled: Option<bool>,
                    #[serde(default)]
                    sort: Option<i32>,
                }
                let body: AdBody = serde_json::from_value(req.body)
                    .map_err(|e| ApiGatewayError::Internal(format!("解析广告体失败: {e}")))?;
                let mut ads = self.ads.lock().expect("ads poisoned");
                let id = format!("ad-{}", ads.len() + 1);
                ads.push(AdItem {
                    id: id.clone(),
                    text: body.text,
                    link: body.link,
                    icon: body.icon,
                    enabled: body.enabled.unwrap_or(true),
                    sort: body.sort.unwrap_or(0),
                });
                save_ads(&ads);
                Ok(ApiResponse {
                    status: 201,
                    body: serde_json::json!({"ok": true, "id": id}),
                    headers: serde_json::json!({}),
                })
            }
            (HttpMethod::Delete, p) if p.starts_with(PATH_ADS_MANAGE) => {
                let id = p.trim_start_matches(PATH_ADS_MANAGE);
                let mut ads = self.ads.lock().expect("ads poisoned");
                ads.retain(|a| a.id != id);
                save_ads(&ads);
                Ok(ApiResponse {
                    status: 200,
                    body: serde_json::json!({"ok": true}),
                    headers: serde_json::json!({}),
                })
            }
            (_, other) => Err(ApiGatewayError::ComponentNotFound(format!(
                "system handler 未提供路由: {other}"
            ))),
        }
    }
}

// ----------------------------------------------------------------------------
// 各路由处理器（私有，返回 ApiResponse 或 Error）
// ----------------------------------------------------------------------------

/// 构造一条 GET 路由规格（统一 `system` 组件名 + 免认证）。
fn spec(method: HttpMethod, path: &str) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: COMPONENT.to_string(),
        requires_auth: false,
        required_roles: Vec::new(),
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

/// `GET /healthz` —— 简单健康检查（`{"status":"ok"}`）。
fn handle_healthz() -> ApiResponse {
    ok_json(serde_json::json!({"status": "ok"}))
}

/// `GET /api/v1/version` —— 网关版本（`name` + `version`）。
fn handle_version() -> ApiResponse {
    ok_json(serde_json::json!({
        "name": "os-api",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// `GET /status` —— 聚合系统状态（对齐 `os_mobile::SystemStatus` 客户端契约）。
///
/// **契约对齐**（修复 batch14 发现的 schema 错配 bug）：本响应体必须能反序列化为
/// [`os_mobile::client::SystemStatus`]（CLI/desktop 共用），故包含其全部 5 个字段：
/// `hostname` / `version` / `capacity` / `health` / `node_count`。
///
/// **向前兼容**：额外保留 `cpu_virt`（CPU 虚拟化能力）与 `uptime`（秒）扩展字段，
/// 既不破坏既有 `/status` 调用者（serde 默认忽略未知字段），也让 `SystemStatus`
/// 反序列化成功（SystemStatus 也不带 `deny_unknown_fields`）。
///
/// CPU 虚拟化检测在 `spawn_blocking` 里跑（快速同步文件 IO，但走 blocking 池
/// 以严格不阻塞 runtime）；检测本身失败时把错误塞进 `cpu_virt` 字段而不是
/// 整个 503——`/status` 旨在"尽力聚合"，单项失败不应让探针误判整机宕机。
async fn handle_status() -> ApiResponse {
    let cpu_virt = match detect_virt_blocking().await {
        Ok(v) => serde_json::to_value(&v).unwrap_or(serde_json::json!(null)),
        Err(e) => serde_json::json!({
            "error": format!("CPU 虚拟化检测失败: {e}"),
        }),
    };
    let uptime = current_uptime_seconds();

    // —— SystemStatus 契约字段（CLI/desktop 反序列化必需）——
    // capacity/health/node_count 此处给保守默认（单机 + 健康 + 零容量占位）；
    // 真实存储容量由后续 batch 接通 os-storage 探测填入，health 由 os-monitor 填入。
    ok_json(serde_json::json!({
        "hostname": detect_hostname(),
        "version": env!("CARGO_PKG_VERSION"),
        "capacity": {
            "used_bytes": 0u64,
            "total_bytes": 0u64,
        },
        "health": "healthy",
        "node_count": 1u32,
        // —— 向前兼容扩展字段（SystemStatus 反序列化时被忽略）——
        "cpu_virt": cpu_virt,
        "uptime": uptime,
    }))
}

/// `GET /api/v1/system/virt-check` —— CPU 虚拟化能力详查。
///
/// 返回 [`VirtCheckResult`] 全字段（serde 序列化为对象，字段名与 struct 一致）
/// 以及 `is_usable` 综合判定与 `diagnostic` 中文诊断字符串。
/// 检测本身失败（如 `/proc` 不可读）返回 500。
async fn handle_virt_check() -> Result<ApiResponse, ApiGatewayError> {
    let result = detect_virt_blocking()
        .await
        .map_err(|e| ApiGatewayError::Internal(format!("CPU 虚拟化检测失败: {e}")))?;
    Ok(ok_json(serde_json::json!({
        "result": result,
        "is_usable": result.is_usable(),
        "diagnostic": result.to_user_diagnostic(),
    })))
}

// ----------------------------------------------------------------------------
// 工具：blocking 池跑 virt 检测 + 取 uptime
// ----------------------------------------------------------------------------

/// 在 `spawn_blocking` 池中调 [`detect_virt_capability`]（同步文件 IO，不阻塞 runtime）。
///
/// 把 `std::io::Error` 透传给调用方（由各 handler 决定是塞进字段还是返回 500）。
async fn detect_virt_blocking() -> Result<VirtCheckResult, std::io::Error> {
    tokio::task::spawn_blocking(detect_virt_capability)
        .await
        .map_err(|join_err| {
            // JoinError（panic / cancel）转成 io::Error 让上层统一处理
            std::io::Error::other(join_err.to_string())
        })?
}

/// 取本进程的 uptime（秒）——`std::time::Instant::now() - 进程启动时刻`。
///
/// 用 `OnceLock` 缓存进程启动时刻（首次调用时记录），后续每次调用算差值。
/// 这是"网关进程自身 uptime"，非主机 uptime（主机 uptime 需额外依赖且跨平台差异大）。
fn current_uptime_seconds() -> u64 {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    let start = *START.get_or_init(std::time::Instant::now);
    start.elapsed().as_secs()
}

/// 探测本机主机名（`hostname` 命令；失败回退 `"local"`）。
///
/// 与 `handlers::discover::detect_hostname` 同款实现（构造期同步执行 `hostname`
/// 命令开销极小 <5ms；避免引第三方 hostname/gethostname crate，workspace 未注册）。
/// 用于填充 `GET /status` 的 `hostname` 字段（对齐 SystemStatus 客户端契约）。
fn detect_hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "local".to_string())
}

// ----------------------------------------------------------------------------
// 单元测试——路由声明 + 各路径响应 JSON 结构
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 辅助：构造一个对指定路径的 GET 请求。
    fn get_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.to_string(),
            headers: serde_json::json!({}),
            body: serde_json::json!({}),
            auth: None,
        }
    }

    /// routes() 声明 4 条 GET 路由，全部 `system` 组件 + 免认证。
    #[tokio::test]
    async fn routes_declares_seven_endpoints() {
        let h = SystemRouteHandler::new();
        let routes = h.routes().await;
        assert_eq!(
            routes.len(),
            8,
            "应声明 8 条路由（4 GET + restart + 2 广告管理 + 1 广告删除）"
        );
        for r in &routes {
            assert_eq!(r.handler_component, COMPONENT);
        }
        let paths: Vec<&str> = routes.iter().map(|r| r.path.as_str()).collect();
        assert!(paths.contains(&PATH_STATUS));
        assert!(paths.contains(&PATH_HEALTHZ));
        assert!(paths.contains(&PATH_VERSION));
        assert!(paths.contains(&PATH_VIRT_CHECK));
    }

    /// `/healthz` 返回 `{"status":"ok"}` + 200。
    #[tokio::test]
    async fn healthz_returns_status_ok() {
        let h = SystemRouteHandler::new();
        let resp = h.handle(get_req(PATH_HEALTHZ)).await.expect("healthz ok");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["status"], "ok");
    }

    /// `/healthz` 带 query 时仍命中（query 不影响 dispatch）。
    #[tokio::test]
    async fn healthz_ignores_query_string() {
        let h = SystemRouteHandler::new();
        let resp = h.handle(get_req("/healthz?probe=1")).await.expect("ok");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["status"], "ok");
    }

    /// `/api/v1/version` 返回 `name` + 非空 `version`（取自 CARGO_PKG_VERSION）。
    #[tokio::test]
    async fn version_returns_name_and_version() {
        let h = SystemRouteHandler::new();
        let resp = h.handle(get_req(PATH_VERSION)).await.expect("version ok");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["name"], "os-api");
        let v = resp.body["version"].as_str().expect("version 是字符串");
        assert!(!v.is_empty(), "version 非空（来自 CARGO_PKG_VERSION）");
    }

    /// `/status` 返回对齐 `os_mobile::SystemStatus` 客户端契约的结构：
    /// 必含 `hostname`/`version`/`capacity`/`health`/`node_count` 5 字段（CLI 反序列化必需），
    /// 另含向前兼容扩展 `cpu_virt`（对象/null/error）+ `uptime`。
    ///
    /// CI/容器内 `/proc/cpuinfo` 通常可读，`cpu_virt` 应为对象且含 `cpu_vendor`；
    /// 极端情况（/proc 不可读）会回退成 `{error: ...}`，本测对两种情况都容忍。
    ///
    /// 本测同时验证响应体能被反序列化为真实的 `SystemStatus`（schema 错配回归测）。
    #[tokio::test]
    async fn status_matches_system_status_contract() {
        use os_mobile::client::SystemStatus;

        let h = SystemRouteHandler::new();
        let resp = h.handle(get_req(PATH_STATUS)).await.expect("status ok");
        assert_eq!(resp.status, 200);

        // —— 必须能反序列化为 SystemStatus（batch14 schema 错配回归断言）——
        let status: SystemStatus = serde_json::from_value(resp.body.clone())
            .expect("/status 响应必须能反序列化为 SystemStatus");
        // hostname 非空（detect_hostname 失败回退 "local" 也是非空）
        assert!(!status.hostname.is_empty(), "hostname 非空");
        // version 字段恒定非空（CARGO_PKG_VERSION）
        assert!(!status.version.is_empty(), "version 非空");
        // capacity 两个 u64 字段存在（本 handler 给 0 占位）
        let _ = status.capacity.used_bytes;
        let _ = status.capacity.total_bytes;
        // health 反序列化为 Healthy（本 handler 写 "healthy"）
        assert_eq!(status.health, os_core::Health::Healthy);
        // node_count 至少 1（单机占位）
        assert!(status.node_count >= 1, "node_count >= 1");

        // —— 向前兼容扩展字段仍存在（不破坏既有 /status 调用者）——
        // version 字段恒定非空
        let v = resp.body["version"].as_str().expect("version 是字符串");
        assert!(!v.is_empty());
        // uptime 是非负整数（刚启动可能是 0）
        let uptime = resp.body["uptime"].as_u64().expect("uptime 是 u64");
        // cpu_virt 要么是 VirtCheckResult 对象（含 cpu_vendor），要么是 error 回退
        if resp.body["cpu_virt"].is_object() {
            assert!(
                resp.body["cpu_virt"].get("cpu_vendor").is_some(),
                "cpu_virt 对象应含 cpu_vendor 字段"
            );
        } else {
            // 回退路径：含 error 字符串
            assert!(resp.body["cpu_virt"]["error"].is_string());
        }
        let _ = uptime; // 仅证明可解析
    }

    /// `/api/v1/system/virt-check` 返回详查结果：`result.cpu_vendor` + `is_usable` + `diagnostic`。
    ///
    /// 真实 CI 环境下 `/proc/cpuinfo` 一般可读；若 /proc 不可读则整测跳过（不硬失败）。
    #[tokio::test]
    async fn virt_check_returns_full_result_and_diagnostic() {
        let h = SystemRouteHandler::new();
        let resp = match h.handle(get_req(PATH_VIRT_CHECK)).await {
            Ok(r) => r,
            Err(ApiGatewayError::Internal(_)) => {
                // /proc/cpuinfo 不可读（极端沙箱）：本 handler 返回 500 属预期，跳过结构断言
                eprintln!("[test] /proc/cpuinfo 不可读，virt-check 返回 500，跳过结构断言");
                return;
            }
            Err(e) => panic!("virt-check 不应返回该错误: {e:?}"),
        };
        assert_eq!(resp.status, 200);
        // result 是 VirtCheckResult 序列化对象，含 cpu_vendor 字段
        let result = &resp.body["result"];
        assert!(
            result.get("cpu_vendor").is_some(),
            "result 应含 cpu_vendor 字段"
        );
        // is_usable 是布尔
        assert!(resp.body["is_usable"].is_boolean(), "is_usable 应为布尔");
        // diagnostic 是非空中文诊断字符串
        let diag = resp.body["diagnostic"]
            .as_str()
            .expect("diagnostic 是字符串");
        assert!(!diag.is_empty(), "诊断字符串非空");
    }

    /// 未声明路径返回 `ComponentNotFound`（兜底分支，理论上 dispatch 不送来）。
    #[tokio::test]
    async fn unknown_path_returns_component_not_found() {
        let h = SystemRouteHandler::new();
        let err = h
            .handle(get_req("/api/v1/system/no-such-route"))
            .await
            .expect_err("未声明路径应报错");
        assert!(matches!(err, ApiGatewayError::ComponentNotFound(_)));
    }

    /// `Default` trait 已实现（`SystemRouteHandler: Default`），便于下游用
    /// `Default::default()` 泛型构造。仅断言 trait bound 成立，不构造实例
    /// （unit struct 的 `Default` 与 `new()` 等价，构造路径已被其它测试覆盖）。
    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<SystemRouteHandler>();
    }
}
