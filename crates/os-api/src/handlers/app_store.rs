//! `AppStoreRouteHandler` —— 应用中心（仅 NexOS 官方应用）的 HTTP 适配器。
//!
//! 定位：把网关 HTTP 请求（`/api/v1/appstore/*`）翻译为 NexOS 原生应用（内置模块，
//! `source="nexos"`）的发布、浏览、安装管理，返回 JSON。这是 OS"应用中心"桌面应用的
//! 后端 REST 入口。
//!
//! # 来源策略（2026-08-23 需求）
//!
//! **应用中心只显示 NexOS 自己的应用**——不显示 Ubuntu snap 源（也不显示 apt/deb/flatpak
//! 等任何外部仓库）的软件：
//! - 预置目录全部为 NexOS 第一方应用（`source="nexos"`，`install_type="nexos"`），
//!   原 13 个 Ubuntu apt/deb/snap/flatpak 应用已移除。
//! - `all_apps()` 对全量列表做来源过滤（`retain(source == "nexos")`），发布通道即使
//!   夹带外部渠道应用也会被过滤。
//! - POST /publish 拒绝 `install_type` 为 apt/deb/snap/flatpak 的发布请求（400）。
//! - `build_install_cmd` 对任何类型都返回空命令（无外部安装渠道）；NexOS 内置应用
//!   安装任务即时完成（无 spawn）。
//! - 仅卸载路径保留 `flatpak uninstall`（用于管理本机已装的 flatpak 应用，属于
//!   "已安装"管理而非商店上架渠道）；apt/snap 卸载渠道已移除。
//!
//! # 实现策略
//!
//! - **预置目录**：构造时预置 15 个 NexOS 第一方应用（即时通讯 / 相册 / 视频 / 音乐 /
//!   流媒体 / AI 媒体生成 / 代码中心 / 容器 / 笔记 / 文件 / 快传 / 云同步 / 监控 /
//!   模型中心 / 智能监控），覆盖 media / dev / office / internet / system 五大分类。
//! - **用户发布**：POST /publish 把用户提交的应用追加到列表（category 默认 custom，
//!   install_type 固定 nexos），DELETE /published/:id 移除。
//! - **安装**：POST /install 对 nexos 类型任务直接置 completed（内置模块无需外部
//!   命令），记 log_tail 说明；不 spawn、不需要 sudo。
//! - **已安装探测**：GET /installed spawn_blocking 跑 `flatpak list`（dpkg / snap
//!   探测已按早期需求移除，不显示系统包与 snap 应用）。
//!
//! # 降级语义（flatpak 不可用也不 panic）
//!
//! flatpak 可能未安装或无权限 —— spawn 失败 / 进程退出码非 0 都降级为友好的 `failed`
//! 状态（记 stderr 尾部），绝不 panic。命令构造为纯函数（可单测，不真跑）。
//!
//! # 路由表（11 条，component="app_store"）
//!
//! | method | path                                | 动作 |
//! |--------|-------------------------------------|------|
//! | GET    | `/api/v1/appstore/apps`             | 列商店应用（仅 nexos 来源，支持 ?category= 过滤）|
//! | GET    | `/api/v1/appstore/apps/:id`         | 单应用详情 |
//! | GET    | `/api/v1/appstore/categories`       | 分类列表（含应用数）|
//! | GET    | `/api/v1/appstore/installed`        | 列已安装应用（flatpak list 探测）|
//! | POST   | `/api/v1/appstore/install`          | 安装应用（需 admin，内置模块即时完成）|
//! | POST   | `/api/v1/appstore/uninstall`        | 卸载 flatpak 应用（需 admin；nexos 内置应用拒绝卸载）|
//! | GET    | `/api/v1/appstore/tasks`            | 列安装任务 |
//! | GET    | `/api/v1/appstore/tasks/:id`        | 安装任务详情（含 log_tail）|
//! | POST   | `/api/v1/appstore/publish`          | 发布应用（需 admin，仅 nexos 渠道）|
//! | DELETE | `/api/v1/appstore/published/:id`    | 删发布的应用（需 admin）|
//! | GET    | `/api/v1/appstore/stats`            | 聚合统计 |

use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 应用来源常量：NexOS 官方（内置模块）。
pub const APP_SOURCE_NEXOS: &str = "nexos";

/// 商店里的应用（预置目录 + 用户发布）。仅展示 [`APP_SOURCE_NEXOS`] 来源。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreApp {
    /// 应用 id（`chat` / `notes` / `custom-xxx`）。
    pub id: String,
    /// 显示名。
    pub name: String,
    /// 简介。
    pub description: String,
    /// 分类：media / dev / office / internet / system / custom。
    pub category: String,
    /// 图标名/emoji。
    pub icon: String,
    /// 来源：`nexos`（NexOS 官方应用）。非 nexos 来源一律不出现在商店。
    #[serde(default = "default_app_source")]
    pub source: String,
    /// 安装类型：`nexos`（内置模块，即时就绪）/ custom。
    pub install_type: String,
    /// 安装目标：NexOS 模块名（如 `chat`）。
    pub install_target: String,
    /// 发布者：`NexOS 官方` / 用户名。
    pub publisher: String,
    /// 版本（安装后填）。
    pub version: Option<String>,
    /// 评分 0-5。
    pub rating: f32,
    /// 下载次数。
    pub downloads: u64,
    /// 截图占位 URL 列表。
    pub screenshot_urls: Vec<String>,
    /// 是否已安装（内置应用默认 true）。
    pub installed: bool,
}

/// `StoreApp.source` 的 serde 默认值（向后兼容旧 JSON）。
fn default_app_source() -> String {
    APP_SOURCE_NEXOS.to_string()
}

/// 安装任务（POST 创建 / GET 列表 / GET 详情）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallTask {
    /// 任务 id。
    pub id: String,
    /// 关联的应用 id。
    pub app_id: String,
    /// 应用名（冗余，便于列表展示）。
    pub app_name: String,
    /// 安装类型：`nexos`（内置模块）/ flatpak（仅历史任务）。
    pub install_type: String,
    /// `pending` / `installing` / `completed` / `failed`。
    pub status: String,
    /// 安装进程 pid（运行中）。
    pub pid: Option<u32>,
    /// 失败原因。
    pub error: Option<String>,
    /// 最后几行输出（stderr/stdout 尾部）。
    pub log_tail: Option<String>,
    /// 创建时间（ISO 8601）。
    pub created_at: String,
}

/// 已安装应用条目（GET /installed 元素）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledApp {
    /// flatpak 应用 id。
    pub name: String,
    /// 显示名（应用 id 兜底）。
    pub display_name: String,
    /// 版本（flatpak 报告的版本）。
    pub version: String,
    /// 来源类型：flatpak（dpkg / snap 探测已移除）。
    pub source: String,
}

/// `GET /api/v1/appstore/stats` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppStoreStats {
    /// 商店应用总数（预置 + 用户发布）。
    pub total_apps: usize,
    /// 已安装数量。
    pub installed: usize,
    /// 分类数量。
    pub categories: usize,
    /// 是否开启发布功能（本期固定 true）。
    pub publishing_enabled: bool,
}

/// 分类条目（GET /categories 元素）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryInfo {
    pub id: String,
    pub name: String,
    pub count: usize,
}

/// 安装请求体。
#[derive(Debug, Deserialize)]
struct InstallBody {
    app_id: String,
}

/// 卸载请求体。
#[derive(Debug, Deserialize)]
struct UninstallBody {
    app_id: String,
    install_type: String,
}

/// 发布应用请求体。
#[derive(Debug, Deserialize)]
struct PublishBody {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    install_type: String,
    #[serde(default)]
    install_target: String,
}

// ----------------------------------------------------------------------------
// 纯函数（命令构造器，可单测，不执行）
// ----------------------------------------------------------------------------

/// 构造安装命令（含程序名，caller 直接 `Command::new(cmd[0]).args(&cmd[1..])`）。
///
/// **2026-08-23 起应用中心不经过任何外部包管理器**：NexOS 原生应用为系统内置模块，
/// 对任何安装类型（含历史 apt/deb/snap/flatpak）都返回**空 Vec** —— caller 判空后将
/// 任务直接置为 `completed`（无需 spawn、无需 sudo）。这是对"不显示/不经 Ubuntu
/// snap 源"需求的硬保证：不存在任何能构造出 apt/snap 命令的路径。
#[must_use]
pub fn build_install_cmd(_install_type: &str, _target: &str) -> Vec<String> {
    Vec::new()
}

/// 构造卸载命令。
///
/// 仅保留 `flatpak uninstall` 渠道——用于"已安装"页管理本机已装的 flatpak 应用
/// （已安装探测只产出 flatpak 条目）。apt/snap 卸载渠道已移除；nexos 内置应用
/// 不允许卸载（在 HTTP 层拒绝，不会走到这里）。
///
/// - 任意类型 → `["flatpak","uninstall","-y", target]`
#[must_use]
pub fn build_uninstall_cmd(_install_type: &str, target: &str) -> Vec<String> {
    vec![
        "flatpak".into(),
        "uninstall".into(),
        "-y".into(),
        target.into(),
    ]
}

/// 返回预置商店应用目录（15 条 NexOS 第一方应用，覆盖 5 大分类）。
///
/// 2026-08-23 起：全部为 `source="nexos"` / `install_type="nexos"` 的 NexOS 官方
/// 内置应用（对应 OS 各桌面模块），**不再包含任何 Ubuntu apt/deb/snap/flatpak 应用**。
#[must_use]
pub fn preset_apps() -> Vec<StoreApp> {
    let nexos = |id: &str, name: &str, desc: &str, cat: &str, icon: &str, rating: f32, dl: u64| {
        StoreApp {
            id: id.into(),
            name: name.into(),
            description: desc.into(),
            category: cat.into(),
            icon: icon.into(),
            source: APP_SOURCE_NEXOS.into(),
            install_type: "nexos".into(),
            install_target: id.into(),
            publisher: "NexOS 官方".into(),
            version: None,
            rating,
            downloads: dl,
            screenshot_urls: vec![],
            // 内置应用随系统提供，默认已安装
            installed: true,
        }
    };
    vec![
        // —— 媒体 ——
        nexos(
            "photo",
            "NexOS 相册",
            "照片管理与智能相册",
            "media",
            "🖼️",
            4.8,
            8_000_000,
        ),
        nexos(
            "video",
            "NexOS 视频",
            "视频库与播放",
            "media",
            "🎬",
            4.7,
            6_500_000,
        ),
        nexos(
            "music",
            "NexOS 音乐",
            "本地音乐播放",
            "media",
            "🎵",
            4.6,
            5_000_000,
        ),
        nexos(
            "streaming",
            "NexOS 流媒体中心",
            "推流 / 直播 / 转发",
            "media",
            "📡",
            4.5,
            1_200_000,
        ),
        nexos(
            "media-gen",
            "NexOS AI 媒体生成",
            "本地模型文生图 / 文生视频",
            "media",
            "✨",
            4.9,
            2_300_000,
        ),
        // —— 开发 ——
        nexos(
            "codehub",
            "NexOS 代码中心",
            "CodeHub 代码协作",
            "dev",
            "🧑‍💻",
            4.7,
            900_000,
        ),
        nexos(
            "containers",
            "NexOS 容器工作台",
            "容器编排与管理",
            "dev",
            "📦",
            4.6,
            1_500_000,
        ),
        // —— 办公 ——
        nexos(
            "notes",
            "NexOS 笔记",
            "本地优先笔记",
            "office",
            "📝",
            4.8,
            3_400_000,
        ),
        nexos(
            "files",
            "NexOS 文件管理",
            "文件 / 网盘 / 分享",
            "office",
            "📁",
            4.7,
            9_800_000,
        ),
        // —— 网络 ——
        nexos(
            "chat",
            "NexOS 即时通讯",
            "端到端加密 IM",
            "internet",
            "💬",
            4.9,
            12_000_000,
        ),
        nexos(
            "qr-transfer",
            "NexOS 二维码快传",
            "跨设备二维码传输",
            "internet",
            "📶",
            4.4,
            2_100_000,
        ),
        nexos(
            "cloudsync",
            "NexOS 云同步",
            "多端数据同步",
            "internet",
            "☁️",
            4.5,
            2_800_000,
        ),
        // —— 系统 ——
        nexos(
            "monitor",
            "NexOS 系统监控",
            "资源 / 进程 / 告警",
            "system",
            "📊",
            4.8,
            7_600_000,
        ),
        nexos(
            "model-hub",
            "NexOS 模型中心",
            "本地大模型下载与管理",
            "system",
            "🤖",
            4.9,
            4_500_000,
        ),
        nexos(
            "surveillance",
            "NexOS 智能监控",
            "摄像头 / 移动侦测",
            "system",
            "📹",
            4.5,
            1_100_000,
        ),
    ]
}

/// 分类显示名映射。
fn category_label(c: &str) -> &'static str {
    match c {
        "media" => "媒体",
        "dev" => "开发",
        "office" => "办公",
        "internet" => "网络",
        "system" => "系统",
        "game" => "游戏",
        "custom" => "自定义",
        _ => "其他",
    }
}

// ----------------------------------------------------------------------------
// 已安装探测（spawn_blocking，失败降级不 panic）
// ----------------------------------------------------------------------------

/// 探测当前系统已安装的第三方应用：dpkg + flatpak list（不含 snap）。
///
/// 失败（命令不存在/无权限）返回空 vec（不 panic）。
fn scan_installed_blocking() -> Vec<InstalledApp> {
    let mut out = Vec::new();
    // dpkg 探测已移除（按需求：只显示开发者发布的软件，不显示系统包）
    // snap 探测已移除（同上）
    // flatpak list（columns=application,version,branch）
    if let Ok(out_str) = std::process::Command::new("flatpak")
        .arg("list")
        .arg("--columns=application,version")
        .output()
    {
        if out_str.status.success() {
            let text = String::from_utf8_lossy(&out_str.stdout);
            for line in text.lines() {
                let parts: Vec<&str> = line.split(',').collect();
                let app_id = parts.first().copied().unwrap_or("").trim();
                if app_id.is_empty() {
                    continue;
                }
                let version = parts.get(1).copied().unwrap_or("").trim();
                out.push(InstalledApp {
                    name: app_id.into(),
                    display_name: app_id.into(),
                    version: version.into(),
                    source: "flatpak".into(),
                });
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

// ----------------------------------------------------------------------------
// AppStoreRouteHandler
// ----------------------------------------------------------------------------

/// 应用中心路由处理器——HTTP 边界适配到 NexOS 官方应用（内置模块）的浏览 / 发布 /
/// 安装管理，不经 apt/snap 等外部包管理渠道。
pub struct AppStoreRouteHandler {
    /// 用户发布的应用（追加在预置目录之后）。
    published: Mutex<Vec<StoreApp>>,
    /// 安装任务列表。
    tasks: Mutex<Vec<InstallTask>>,
    counter: Mutex<u64>,
}

impl AppStoreRouteHandler {
    /// 构造 handler（空发布列表 + 空任务列表）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            published: Mutex::new(vec![]),
            tasks: Mutex::new(vec![]),
            counter: Mutex::new(100),
        }
    }

    /// 用空列表构造（测试注入）。
    #[must_use]
    pub fn with_empty() -> Self {
        Self::new()
    }

    /// 当前全量应用快照（预置 + 用户发布，**仅 nexos 来源**）。
    ///
    /// 来源过滤（2026-08-23 需求）：非 `source="nexos"` 的条目一律剔除——即使
    /// 发布通道夹带了 Ubuntu/snap 等外部来源应用，也不会出现在商店列表 / 详情 /
    /// 分类 / 统计里。
    fn all_apps(&self) -> Vec<StoreApp> {
        let mut out = preset_apps();
        let published = self.published.lock().expect("published poisoned");
        out.extend(published.iter().cloned());
        out.retain(|a| a.source == APP_SOURCE_NEXOS);
        out
    }

    /// 当前全量安装任务快照。
    #[must_use]
    pub fn tasks_snapshot(&self) -> Vec<InstallTask> {
        self.tasks.lock().expect("tasks poisoned").clone()
    }

    fn next_id(&self) -> String {
        let mut c = self.counter.lock().expect("counter poisoned");
        *c += 1;
        format!("task-{}", *c)
    }

    /// 标记某 app installed=true（按 app_id 匹配）。
    fn mark_installed(&self, app_id: &str, installed: bool) {
        // 预置 app 的 installed 字段不持久化（每次 preset_apps 重新构造），
        // 仅对用户发布的 app 持久化 installed 状态。
        let mut published = self.published.lock().expect("published poisoned");
        for a in published.iter_mut() {
            if a.id == app_id {
                a.installed = installed;
            }
        }
    }

    /// 记录安装任务并返回最终任务。
    ///
    /// NexOS 原生应用为系统内置模块：**不 spawn 任何外部命令**（应用中心已移除
    /// apt/deb/snap/flatpak 全部安装渠道，[`build_install_cmd`] 保证恒为空命令），
    /// 任务直接置为 `completed`（log_tail 附说明）并标记应用已安装。
    fn record_install(&self, mut task: InstallTask, target: &str) -> InstallTask {
        task.status = "completed".into();
        task.log_tail = Some(format!(
            "NexOS 内置应用 {target}：系统内置模块，无需外部安装"
        ));
        let app_id = task.app_id.clone();
        {
            let mut tasks = self.tasks.lock().expect("tasks poisoned");
            tasks.push(task.clone());
        }
        self.mark_installed(&app_id, true);
        task
    }
}

impl Default for AppStoreRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RouteHandler for AppStoreRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec(HttpMethod::Get, "/api/v1/appstore/apps", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/appstore/apps/:id", false, vec![]),
            spec(
                HttpMethod::Get,
                "/api/v1/appstore/categories",
                false,
                vec![],
            ),
            spec(HttpMethod::Get, "/api/v1/appstore/installed", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/appstore/install",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/appstore/uninstall",
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Get, "/api/v1/appstore/tasks", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/appstore/tasks/:id", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/appstore/publish",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/appstore/published/:id",
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Get, "/api/v1/appstore/stats", false, vec![]),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        let query = query_params(&req.path);
        match (req.method, segs.as_slice()) {
            // —— GET /api/v1/appstore/apps —— 列商店应用（可选 ?category=）
            (HttpMethod::Get, ["api", "v1", "appstore", "apps"]) => {
                let mut apps = self.all_apps();
                if let Some(cat) = query.get("category") {
                    apps.retain(|a| a.category == *cat);
                }
                Ok(ok_json(to_value(&apps)?))
            }

            // —— GET /api/v1/appstore/apps/:id —— 单应用详情
            (HttpMethod::Get, ["api", "v1", "appstore", "apps", id]) => {
                let apps = self.all_apps();
                match apps.iter().find(|a| a.id == *id) {
                    Some(a) => Ok(ok_json(to_value(a)?)),
                    None => Ok(error_response(404, &format!("应用不存在: {id}"))),
                }
            }

            // —— GET /api/v1/appstore/categories —— 分类列表（含应用数）
            (HttpMethod::Get, ["api", "v1", "appstore", "categories"]) => {
                let apps = self.all_apps();
                let mut counts: std::collections::HashMap<String, usize> =
                    std::collections::HashMap::new();
                for a in &apps {
                    *counts.entry(a.category.clone()).or_insert(0) += 1;
                }
                // 按"既定顺序"输出：预置分类在前
                let order = [
                    "media", "dev", "office", "internet", "system", "game", "custom",
                ];
                let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
                let mut out: Vec<CategoryInfo> = Vec::new();
                for &c in order.iter() {
                    if let Some(n) = counts.get(c) {
                        out.push(CategoryInfo {
                            id: c.into(),
                            name: category_label(c).into(),
                            count: *n,
                        });
                        seen.insert(c);
                    }
                }
                // 其它（用户自定义分类名）追加在末尾
                for (c, n) in &counts {
                    if !seen.contains(c.as_str()) {
                        out.push(CategoryInfo {
                            id: c.clone(),
                            name: category_label(c).into(),
                            count: *n,
                        });
                    }
                }
                Ok(ok_json(to_value(&out)?))
            }

            // —— GET /api/v1/appstore/installed —— 列已安装应用
            (HttpMethod::Get, ["api", "v1", "appstore", "installed"]) => {
                let list = tokio::task::spawn_blocking(scan_installed_blocking)
                    .await
                    .map_err(|e| {
                        ApiGatewayError::Internal(format!("扫描已安装应用任务 join 失败: {e}"))
                    })?;
                Ok(ok_json(to_value(&list)?))
            }

            // —— POST /api/v1/appstore/install —— 安装应用（admin，NexOS 内置模块即时就绪）
            (HttpMethod::Post, ["api", "v1", "appstore", "install"]) => {
                let body: InstallBody = serde_json::from_value(req.body)
                    .map_err(|e| ApiGatewayError::Internal(format!("解析安装请求体失败: {e}")))?;
                if body.app_id.trim().is_empty() {
                    return Ok(error_response(400, "app_id 不可为空"));
                }
                // 找到对应 app 拿 install_type/target
                let apps = self.all_apps();
                let app = match apps.iter().find(|a| a.id == body.app_id) {
                    Some(a) => a.clone(),
                    None => {
                        return Ok(error_response(404, &format!("应用不存在: {}", body.app_id)))
                    }
                };
                let task = InstallTask {
                    id: self.next_id(),
                    app_id: app.id.clone(),
                    app_name: app.name.clone(),
                    install_type: app.install_type.clone(),
                    status: "pending".into(),
                    pid: None,
                    error: None,
                    log_tail: None,
                    created_at: now_iso(),
                };
                // NexOS 内置应用：任务即时完成（无外部命令、无 sudo、无包管理器）
                let final_task = self.record_install(task, &app.install_target);
                Ok(ApiResponse {
                    status: 201,
                    body: to_value(&final_task)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— POST /api/v1/appstore/uninstall —— 卸载 flatpak 应用（admin；nexos 内置拒绝）
            (HttpMethod::Post, ["api", "v1", "appstore", "uninstall"]) => {
                let body: UninstallBody = serde_json::from_value(req.body)
                    .map_err(|e| ApiGatewayError::Internal(format!("解析卸载请求体失败: {e}")))?;
                if body.app_id.trim().is_empty() {
                    return Ok(error_response(400, "app_id 不可为空"));
                }
                // 找到 target（若 app 在列表里用它，否则用 app_id 兜底当 target）
                let apps = self.all_apps();
                let found = apps.iter().find(|a| a.id == body.app_id);
                // NexOS 内置应用是 OS 的一部分，不允许通过应用中心卸载
                if let Some(a) = found {
                    if a.install_type == "nexos" {
                        return Ok(error_response(
                            400,
                            &format!("NexOS 内置应用 {} 不支持卸载", a.name),
                        ));
                    }
                }
                let (target, install_type, app_name) = match found {
                    Some(a) => (
                        a.install_target.clone(),
                        body.install_type.clone(),
                        a.name.clone(),
                    ),
                    None => (
                        body.app_id.clone(),
                        body.install_type.clone(),
                        body.app_id.clone(),
                    ),
                };
                // spawn 卸载命令（同 install 路径，但用 build_uninstall_cmd）
                let mut task = InstallTask {
                    id: self.next_id(),
                    app_id: body.app_id.clone(),
                    app_name,
                    install_type: body.install_type.clone(),
                    status: "pending".into(),
                    pid: None,
                    error: None,
                    log_tail: None,
                    created_at: now_iso(),
                };
                let cmd = build_uninstall_cmd(&install_type, &target);
                let program = cmd.first().cloned().unwrap_or_else(|| "sudo".into());
                let args_vec: Vec<String> = if cmd.len() > 1 {
                    cmd[1..].to_vec()
                } else {
                    vec![]
                };
                let mut proc = tokio::process::Command::new(&program);
                proc.args(&args_vec);
                proc.stdin(std::process::Stdio::null());
                proc.stdout(std::process::Stdio::piped());
                proc.stderr(std::process::Stdio::piped());
                {
                    let mut tasks = self.tasks.lock().expect("tasks poisoned");
                    tasks.push(task.clone());
                }
                let task_id = task.id.clone();
                match proc.spawn() {
                    Ok(child) => {
                        task.pid = child.id();
                        task.status = "installing".into();
                        {
                            let mut tasks = self.tasks.lock().expect("tasks poisoned");
                            if let Some(t) = tasks.iter_mut().find(|t| t.id == task_id) {
                                t.pid = task.pid;
                                t.status = "installing".into();
                            }
                        }
                        // 后台等待
                        let app_id = body.app_id.clone();
                        let result = child.wait_with_output().await;
                        let (status_str, error, log_tail): (
                            String,
                            Option<String>,
                            Option<String>,
                        ) = match result {
                            Ok(out) => {
                                if out.status.success() {
                                    ("completed".to_string(), None, None)
                                } else {
                                    let stderr = String::from_utf8_lossy(&out.stderr);
                                    let stdout = String::from_utf8_lossy(&out.stdout);
                                    let combined = if !stderr.is_empty() { stderr } else { stdout };
                                    let tail = combined
                                        .lines()
                                        .rev()
                                        .take(10)
                                        .collect::<Vec<_>>()
                                        .into_iter()
                                        .rev()
                                        .collect::<Vec<_>>()
                                        .join("\n");
                                    (
                                        "failed".to_string(),
                                        Some(format!("退出码 {:?}", out.status.code())),
                                        Some(tail),
                                    )
                                }
                            }
                            Err(e) => (
                                "failed".to_string(),
                                Some(format!("卸载进程错误: {e}")),
                                None,
                            ),
                        };
                        {
                            let mut tasks = self.tasks.lock().expect("tasks poisoned");
                            if let Some(t) = tasks.iter_mut().find(|t| t.id == task_id) {
                                t.status = status_str.clone();
                                t.pid = None;
                                t.error = error;
                                t.log_tail = log_tail;
                            }
                        }
                        if status_str == "completed" {
                            self.mark_installed(&app_id, false);
                        }
                    }
                    Err(e) => {
                        let mut tasks = self.tasks.lock().expect("tasks poisoned");
                        if let Some(t) = tasks.iter_mut().find(|t| t.id == task_id) {
                            t.status = "failed".into();
                            t.error = Some(format!("卸载命令启动失败: {e}"));
                        }
                    }
                }
                Ok(ok_json(serde_json::json!({
                    "ok": true,
                    "app_id": body.app_id,
                    "action": "uninstall",
                    "task_id": task_id,
                })))
            }

            // —— GET /api/v1/appstore/tasks —— 列安装任务
            (HttpMethod::Get, ["api", "v1", "appstore", "tasks"]) => {
                let tasks = self.tasks_snapshot();
                Ok(ok_json(to_value(&tasks)?))
            }

            // —— GET /api/v1/appstore/tasks/:id —— 安装任务详情
            (HttpMethod::Get, ["api", "v1", "appstore", "tasks", id]) => {
                let tasks = self.tasks_snapshot();
                match tasks.iter().find(|t| t.id == *id) {
                    Some(t) => Ok(ok_json(to_value(t)?)),
                    None => Ok(error_response(404, &format!("安装任务不存在: {id}"))),
                }
            }

            // —— POST /api/v1/appstore/publish —— 发布应用（admin，仅 NexOS 渠道）
            (HttpMethod::Post, ["api", "v1", "appstore", "publish"]) => {
                let body: PublishBody = serde_json::from_value(req.body)
                    .map_err(|e| ApiGatewayError::Internal(format!("解析发布请求体失败: {e}")))?;
                if body.name.trim().is_empty() {
                    return Ok(error_response(400, "name 不可为空"));
                }
                if body.install_target.trim().is_empty() {
                    return Ok(error_response(400, "install_target 不可为空"));
                }
                // 来源策略：拒绝 apt/deb/snap/flatpak 等外部渠道发布（2026-08-23 需求）
                const EXTERNAL_INSTALL_TYPES: [&str; 4] = ["apt", "deb", "snap", "flatpak"];
                if EXTERNAL_INSTALL_TYPES.contains(&body.install_type.trim()) {
                    return Ok(error_response(
                        400,
                        &format!(
                            "应用中心仅支持发布 NexOS 原生应用，不支持 {} 渠道",
                            body.install_type.trim()
                        ),
                    ));
                }
                let category = if body.category.trim().is_empty() {
                    "custom".into()
                } else {
                    body.category.trim().to_string()
                };
                let install_type = if body.install_type.trim().is_empty() {
                    "nexos".into()
                } else {
                    body.install_type.trim().to_string()
                };
                // 生成唯一 id（custom-<seq>）
                let seq = {
                    let mut c = self.counter.lock().expect("counter poisoned");
                    *c += 1;
                    *c
                };
                let id = format!("custom-{seq}");
                let app = StoreApp {
                    id: id.clone(),
                    name: body.name.trim().to_string(),
                    description: body.description.trim().to_string(),
                    category,
                    icon: "📦".into(),
                    source: APP_SOURCE_NEXOS.into(),
                    install_type,
                    install_target: body.install_target.trim().to_string(),
                    publisher: "用户发布".into(),
                    version: None,
                    rating: 0.0,
                    downloads: 0,
                    screenshot_urls: vec![],
                    installed: false,
                };
                let resp_body = to_value(&app)?;
                self.published.lock().expect("published poisoned").push(app);
                Ok(ApiResponse {
                    status: 201,
                    body: resp_body,
                    headers: serde_json::json!({}),
                })
            }

            // —— DELETE /api/v1/appstore/published/:id —— 删发布的应用（admin）
            (HttpMethod::Delete, ["api", "v1", "appstore", "published", id]) => {
                let mut published = self.published.lock().expect("published poisoned");
                let before = published.len();
                published.retain(|a| a.id != *id);
                if published.len() == before {
                    return Ok(error_response(404, &format!("发布的应用不存在: {id}")));
                }
                Ok(ok_json(serde_json::json!({
                    "ok": true,
                    "id": id,
                    "action": "delete"
                })))
            }

            // —— GET /api/v1/appstore/stats —— 聚合统计
            (HttpMethod::Get, ["api", "v1", "appstore", "stats"]) => {
                let apps = self.all_apps();
                let categories: std::collections::HashSet<&str> =
                    apps.iter().map(|a| a.category.as_str()).collect();
                // installed 数：扫描真实系统 + 标记的 installed 字段并集（取上限）
                let installed_sys = tokio::task::spawn_blocking(scan_installed_blocking)
                    .await
                    .unwrap_or_default();
                let marked = apps.iter().filter(|a| a.installed).count();
                let installed = installed_sys.len().max(marked);
                Ok(ok_json(to_value(&AppStoreStats {
                    total_apps: apps.len(),
                    installed,
                    categories: categories.len(),
                    publishing_enabled: true,
                })?))
            }

            // —— 未覆盖路由 —— 兜底 404
            _ => Ok(error_response(404, "app_store: 未匹配的路由")),
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
        handler_component: "app_store".to_string(),
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

/// 解析 query string 为 HashMap（仅取 key=value，重复取最后一个）。
fn query_params(path: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    if let Some(q) = path.split('?').nth(1) {
        for pair in q.split('&') {
            let mut it = pair.splitn(2, '=');
            if let Some(k) = it.next() {
                if k.is_empty() {
                    continue;
                }
                let v = it.next().unwrap_or("");
                // URL 解码（百分号解码）
                out.insert(k.to_string(), url_decode(v));
            }
        }
    }
    out
}

/// 简易 URL 解码（仅 %XX + + → 空格）。
fn url_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'+' {
            out.push(' ');
            i += 1;
        } else if b == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push(((h * 16 + l) as u8) as char);
                i += 3;
            } else {
                out.push(b as char);
                i += 1;
            }
        } else {
            out.push(b as char);
            i += 1;
        }
    }
    out
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

    // ---- 命令构造器测试（无外部安装渠道）----

    #[test]
    fn build_install_cmd_never_produces_external_channel() {
        // 2026-08-23 需求：应用中心不经 apt/deb/snap/flatpak 任何外部渠道，
        // 对所有安装类型都不得构造出安装命令（内置模块即时就绪）。
        for t in [
            "apt", "deb", "snap", "flatpak", "nexos", "custom", "unknown",
        ] {
            let cmd = build_install_cmd(t, "whatever");
            assert!(cmd.is_empty(), "安装命令应为空（不允许 {t} 渠道）: {cmd:?}");
        }
    }

    #[test]
    fn build_uninstall_cmd_is_flatpak_only() {
        // 卸载仅保留 flatpak 渠道（管理本机已装 flatpak 应用）
        let cmd = build_uninstall_cmd("flatpak", "com.example.App");
        let joined = cmd.join(" ");
        assert!(
            joined.contains("flatpak uninstall"),
            "卸载应走 flatpak uninstall: {joined}"
        );
        assert!(joined.contains("com.example.App"));
    }

    #[test]
    fn build_uninstall_cmd_has_no_apt_or_snap_channel() {
        // apt / snap 卸载渠道已移除：任何输入都不会构造出 apt-get / snap 命令
        for t in ["apt", "snap", "deb", "nexos"] {
            let joined = build_uninstall_cmd(t, "x").join(" ");
            assert!(!joined.contains("apt-get"), "不允许 apt-get: {joined}");
            assert!(!joined.contains("sudo"), "不允许 sudo: {joined}");
            assert!(
                !joined.contains("snap remove"),
                "不允许 snap remove: {joined}"
            );
        }
    }

    // ---- 路由声明测试 ----

    #[tokio::test]
    async fn routes_declares_eleven_endpoints_all_app_store() {
        let h = AppStoreRouteHandler::new();
        let routes = h.routes().await;
        assert_eq!(routes.len(), 11, "应有 11 条路由: {routes:?}");
        assert!(
            routes.iter().all(|r| r.handler_component == "app_store"),
            "全部归属 app_store 组件"
        );
        // 写操作（POST / DELETE）要求 admin
        for r in &routes {
            if r.method == HttpMethod::Post || r.method == HttpMethod::Delete {
                assert!(r.requires_auth, "写操作需 auth: {r:?}");
                assert_eq!(r.required_roles, vec!["admin".to_string()]);
            }
        }
        // GET 全部公开
        for r in &routes {
            if r.method == HttpMethod::Get {
                assert!(!r.requires_auth, "GET 应公开: {r:?}");
            }
        }
    }

    // ---- 预置应用测试（仅 NexOS 官方应用）----

    #[test]
    fn preset_apps_returns_at_least_ten() {
        let apps = preset_apps();
        assert!(apps.len() >= 10, "预置应用应 >=10 条: {}", apps.len());
        // 含关键字段
        for a in &apps {
            assert!(!a.id.is_empty());
            assert!(!a.name.is_empty());
            assert!(!a.install_type.is_empty());
            assert!(!a.install_target.is_empty());
        }
        // 含 NexOS 第一方应用
        assert!(apps.iter().any(|a| a.id == "chat"), "应含 chat");
        assert!(apps.iter().any(|a| a.id == "notes"), "应含 notes");
    }

    #[test]
    fn preset_apps_all_nexos_source_no_external_channel() {
        // 2026-08-23 需求：预置目录不得包含 Ubuntu snap / apt / deb / flatpak 应用
        let apps = preset_apps();
        assert!(!apps.is_empty());
        for a in &apps {
            assert_eq!(a.source, "nexos", "{} 来源应为 nexos", a.id);
            assert_eq!(a.install_type, "nexos", "{} 安装类型应为 nexos", a.id);
            assert_eq!(a.publisher, "NexOS 官方", "{} 发布者应为 NexOS 官方", a.id);
            assert!(!a.publisher.contains("Snap"), "不得有 Snap 发布者: {:?}", a);
        }
        // 旧的 Ubuntu 应用必须已移除
        for gone in [
            "vlc",
            "firefox",
            "gitkraken",
            "git-gui",
            "steam",
            "gimp",
            "obs",
        ] {
            assert!(
                !apps.iter().any(|a| a.id == gone),
                "Ubuntu 应用 {gone} 应已下架"
            );
        }
    }

    // ---- GET /apps 返回预置应用（仅 nexos）----

    #[tokio::test]
    async fn list_apps_returns_preset_at_least_ten() {
        let h = AppStoreRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/appstore/apps")).await.unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().expect("body 为数组");
        assert!(arr.len() >= 10, "应返回 >=10 条预置应用: {}", arr.len());
    }

    #[tokio::test]
    async fn list_apps_contains_only_nexos_source() {
        let h = AppStoreRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/appstore/apps")).await.unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().expect("body 为数组");
        assert!(!arr.is_empty());
        for a in arr {
            assert_eq!(a["source"], "nexos", "商店应只含 nexos 来源: {a:?}");
            assert_ne!(a["install_type"], "snap", "不得含 snap 渠道: {a:?}");
            assert_ne!(a["install_type"], "apt", "不得含 apt 渠道: {a:?}");
        }
    }

    #[tokio::test]
    async fn list_apps_category_filter_works() {
        let h = AppStoreRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/appstore/apps?category=media"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().expect("body 为数组");
        assert!(
            arr.iter().all(|a| a["category"] == "media"),
            "过滤后应全为 media 分类"
        );
        assert!(!arr.is_empty(), "media 分类应非空");
    }

    // ---- GET /categories 返回分类 ----

    #[tokio::test]
    async fn list_categories_returns_array() {
        let h = AppStoreRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/appstore/categories"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().expect("body 为数组");
        assert!(!arr.is_empty(), "应返回非空分类");
        for c in arr {
            assert!(c["id"].is_string());
            assert!(c["name"].is_string());
            assert!(c["count"].is_u64());
        }
        // 应含 media
        assert!(arr.iter().any(|c| c["id"] == "media"), "应含 media 分类");
    }

    // ---- POST /publish 追加到列表（仅 nexos 渠道）----

    #[tokio::test]
    async fn publish_app_adds_to_list() {
        let h = AppStoreRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/appstore/publish",
                serde_json::json!({
                    "name": "我的测试应用",
                    "description": "自发布测试",
                    "category": "custom",
                    "install_type": "nexos",
                    "install_target": "my-test-app"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "publish body: {resp:?}");
        let id = resp.body["id"].as_str().unwrap().to_string();
        assert!(id.starts_with("custom-"), "id 应以 custom- 开头: {id}");
        assert_eq!(resp.body["name"], "我的测试应用");
        assert_eq!(resp.body["category"], "custom");
        assert_eq!(resp.body["source"], "nexos", "发布应用来源应为 nexos");

        // 验证已追加到全量列表
        let resp2 = h.handle(get_req("/api/v1/appstore/apps")).await.unwrap();
        let arr = resp2.body.as_array().unwrap();
        assert!(arr.iter().any(|a| a["id"] == id), "发布后应在 apps 列表中");
        // 在 custom 分类下也能找到
        let resp3 = h
            .handle(get_req("/api/v1/appstore/apps?category=custom"))
            .await
            .unwrap();
        let arr3 = resp3.body.as_array().unwrap();
        assert!(arr3.iter().any(|a| a["id"] == id), "应在 custom 分类下");
    }

    #[tokio::test]
    async fn publish_rejects_external_install_types() {
        // 2026-08-23 需求：不得通过发布通道夹带 Ubuntu apt/deb/snap/flatpak 应用
        for bad in ["apt", "deb", "snap", "flatpak"] {
            let h = AppStoreRouteHandler::with_empty();
            let resp = h
                .handle(post_req(
                    "/api/v1/appstore/publish",
                    serde_json::json!({
                        "name": "外部渠道应用",
                        "install_type": bad,
                        "install_target": "some-pkg"
                    }),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 400, "{bad} 渠道发布应被拒绝: {resp:?}");
            // 且不会出现在商店列表
            let resp2 = h.handle(get_req("/api/v1/appstore/apps")).await.unwrap();
            let arr = resp2.body.as_array().unwrap();
            assert!(
                !arr.iter().any(|a| a["install_type"] == bad),
                "商店不得含 {bad} 渠道应用"
            );
        }
    }

    #[tokio::test]
    async fn publish_rejects_empty_name() {
        let h = AppStoreRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/appstore/publish",
                serde_json::json!({"name": "", "install_target": "x"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    #[tokio::test]
    async fn publish_rejects_empty_target() {
        let h = AppStoreRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/appstore/publish",
                serde_json::json!({"name": "foo", "install_target": ""}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    // ---- DELETE /published/:id ----

    #[tokio::test]
    async fn delete_published_removes_app() {
        let h = AppStoreRouteHandler::with_empty();
        // 先发布
        let resp = h
            .handle(post_req(
                "/api/v1/appstore/publish",
                serde_json::json!({
                    "name": "待删除",
                    "install_target": "to-delete"
                }),
            ))
            .await
            .unwrap();
        let id = resp.body["id"].as_str().unwrap().to_string();
        // 删除
        let resp = h
            .handle(del_req(&format!("/api/v1/appstore/published/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        // 列表不再含
        let resp = h.handle(get_req("/api/v1/appstore/apps")).await.unwrap();
        let arr = resp.body.as_array().unwrap();
        assert!(!arr.iter().any(|a| a["id"] == id), "删除后不应在列表中");
    }

    #[tokio::test]
    async fn delete_missing_returns_404() {
        let h = AppStoreRouteHandler::new();
        let resp = h
            .handle(del_req("/api/v1/appstore/published/nope"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // ---- GET /apps/:id 详情 ----

    #[tokio::test]
    async fn get_app_detail_returns_preset() {
        let h = AppStoreRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/appstore/apps/chat"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["id"], "chat");
        assert_eq!(resp.body["name"], "NexOS 即时通讯");
        assert_eq!(resp.body["source"], "nexos");
    }

    #[tokio::test]
    async fn get_app_detail_missing_returns_404() {
        let h = AppStoreRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/appstore/apps/__nope__"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // ---- stats ----

    #[tokio::test]
    async fn stats_returns_counts_without_panic() {
        let h = AppStoreRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/appstore/stats")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body["total_apps"].is_u64());
        assert!(resp.body["installed"].is_u64());
        assert!(resp.body["categories"].is_u64());
        assert_eq!(resp.body["publishing_enabled"], true);
        assert!(
            resp.body["total_apps"].as_u64().unwrap() >= 10,
            "total_apps 应 >=10"
        );
    }

    // ---- installed 探测（真实系统，至少不 panic）----

    #[tokio::test]
    async fn list_installed_returns_array_without_panic() {
        let h = AppStoreRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/appstore/installed"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body.is_array());
    }

    // ---- install 创建任务（NexOS 内置模块即时完成，无 spawn）----

    #[tokio::test]
    async fn install_creates_task_completes_immediately() {
        let h = AppStoreRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/appstore/install",
                serde_json::json!({"app_id": "monitor"}),
            ))
            .await
            .unwrap();
        // 201（任务已创建且即时完成——不经 apt/snap/flatpak）
        assert_eq!(resp.status, 201, "install body: {resp:?}");
        let id = resp.body["id"].as_str().unwrap().to_string();
        assert_eq!(resp.body["status"], "completed", "内置应用任务应即时完成");
        // 任务在列表里
        let resp = h.handle(get_req("/api/v1/appstore/tasks")).await.unwrap();
        let arr = resp.body.as_array().unwrap();
        assert!(arr.iter().any(|t| t["id"] == id), "任务应在列表中");
    }

    #[tokio::test]
    async fn install_missing_app_returns_404() {
        let h = AppStoreRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/appstore/install",
                serde_json::json!({"app_id": "__nope__"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn uninstall_nexos_builtin_is_rejected() {
        // NexOS 内置应用是 OS 一部分，不允许通过应用中心卸载
        let h = AppStoreRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/appstore/uninstall",
                serde_json::json!({"app_id": "chat", "install_type": "nexos"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "内置应用卸载应被拒绝: {resp:?}");
    }

    #[tokio::test]
    async fn get_task_detail_returns_task() {
        let h = AppStoreRouteHandler::with_empty();
        // 先创建任务
        let resp = h
            .handle(post_req(
                "/api/v1/appstore/install",
                serde_json::json!({"app_id": "monitor"}),
            ))
            .await
            .unwrap();
        let id = resp.body["id"].as_str().unwrap().to_string();
        // 查详情
        let resp = h
            .handle(get_req(&format!("/api/v1/appstore/tasks/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["id"], id);
        assert_eq!(resp.body["app_id"], "monitor");
        assert_eq!(resp.body["install_type"], "nexos");
    }

    #[tokio::test]
    async fn get_task_missing_returns_404() {
        let h = AppStoreRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/appstore/tasks/nope"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn unmatched_route_returns_404() {
        let h = AppStoreRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/appstore/nope")).await.unwrap();
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<AppStoreRouteHandler>();
    }
}
