//! `AppsRouteHandler` —— NexOS 应用包运行时（后端）REST 入口（docs/APPS.md）。
//!
//! 定位：把「应用包」（manifest.json + web/ 前端静态资源的 git 仓库）的
//! **安装 / 卸载 / 已装清单 / 商店目录 / 静态托管**暴露为 REST。应用不是
//! NexOS 内置功能——用户从应用中心（AppStore）浏览 NexHub `nexos-app-*`
//! 仓库并一键安装；装了应用才有对应桌面入口与业务端点（引擎门控见
//! [`AppRegistry::is_engine_enabled`] 与 film.rs 的门控接入）。
//!
//! # 应用包规范（冻结契约，与 docs/APPS.md、前端代理同款）
//!
//! manifest.json：`{id,name,version,category,icon,description,entry,
//! engine?,min_os_api?}`
//! - `id`：小写字母/数字/连字符（`^[a-z0-9-]+$`，≤64 字符）——同时是安装
//!   目录名 `/tank/os-data/apps/<id>/` 与静态托管段 `/apps-assets/<id>/…`。
//! - `entry`：相对包根的前端入口（如 `web/entry.js`）；不得含 `..`、不得
//!   以 `/` 开头；安装时校验文件存在。
//! - `version`：`x.y.z` 三段数字（可带 `-prerelease` / `+build` 后缀）。
//! - `engine`：可选。声明应用启用的内置引擎（如 film 影片管线引擎）——
//!   未装应用时引擎业务端点 404（引擎内置、应用按装启用）。
//! - `min_os_api`：可选 semver 下限，高于当前 os-api 版本拒绝安装。
//!
//! # 安装 / 升级 / 幂等（同步完成，appstore 安装任务同款即时记录）
//!
//! 1. `POST /api/v1/apps/install {"repo":"nexos-app-film"}`（repo 为 NexHub
//!    裸仓库名；本机 `<repos_dir>/<repo>.git` 优先 `file://` 直连，缺省
//!    `/tank/git-repos`；也接受完整 http(s) URL）。
//! 2. `git clone --depth 1` 到临时目录 → **发布根解析**（仓库根优先；源码
//!    +dist 双收仓库（apps/film 形态，根 manifest 的 entry 实际产物在
//!    `dist/web/`）回退取 `dist/`）→ 校验 manifest（id 非空合法 / entry
//!    存在 / 版本格式 / min_os_api）→ 拷贝发布根（除 `.git` 外）全部到
//!    `<apps_dir>/<id>/` → 登记 SQLite `apps` 表。
//! 3. 幂等：同 id 同版本重复安装 = no-op（200 提示）；同 id 异版本 =
//!    覆盖升级（201）；同 id 但 repo 不同 = 拒绝（409）。
//!
//! # SQLite 持久化（apps.db · apps 表）
//!
//! `CREATE TABLE apps (id TEXT PRIMARY KEY, name, version, category, icon,
//! description, entry, repo, engine, min_os_api, dir, installed_at,
//! updated_at)`——重启不丢；film 等引擎门控每请求直查此表（无缓存，
//! 安装/卸载即时生效）。
//!
//! # 路由表（6 条，component="apps"；读公开 / 写 admin）
//!
//! | method | path | 动作 |
//! |--------|------|------|
//! | GET | `/api/v1/apps` | 已装列表 `{"apps":[…]}` |
//! | POST | `/api/v1/apps/install` | 安装/升级（admin，同步完成 + 任务记录）|
//! | DELETE | `/api/v1/apps/:id` | 卸载（删目录 + 注销，admin）|
//! | GET | `/api/v1/apps/catalog` | 扫 NexHub `nexos-app-*` 裸仓库拉 manifest |
//! | GET | `/api/v1/apps/tasks` | 安装任务列表（appstore 任务框架同款）|
//! | GET | `/apps-assets/:id/*` | 应用静态资源（`<apps_dir>/<id>/web/` 下）|

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// 常量与 env
// ----------------------------------------------------------------------------

/// 应用安装根目录 env（缺省 `/tank/os-data/apps`）。
pub const ENV_APPS_DIR: &str = "NEXOS_APPS_DIR";

/// 应用注册表 SQLite 路径 env（缺省 `/tank/os-data/apps.db`）。
pub const ENV_APPS_DB: &str = "NEXOS_APPS_DB";

/// 应用安装根目录缺省值。
pub const DEFAULT_APPS_DIR: &str = "/tank/os-data/apps";

/// 应用注册表缺省值。
pub const DEFAULT_APPS_DB: &str = "/tank/os-data/apps.db";

/// catalog 只认 `nexos-app-` 前缀的裸仓库（NexHub 应用分发命名约定）。
pub const CATALOG_REPO_PREFIX: &str = "nexos-app-";

/// git clone 超时（秒）——file:// 本机克隆通常亚秒级，留足网络直连余量。
const CLONE_TIMEOUT_SECS: u64 = 300;

/// 仓库目录（NexHub 裸仓库根，与 os-nexhub `/git/*` CGI、code_repo 同源）。
fn apps_repos_dir() -> String {
    os_nexhub::repos_dir()
}

fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

// ----------------------------------------------------------------------------
// DTO：manifest / 已装记录 / catalog / 任务
// ----------------------------------------------------------------------------

/// 应用包 manifest（仓库根 manifest.json；安装校验的输入）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub description: String,
    /// 前端入口（相对包根，如 `web/entry.js`）。
    pub entry: String,
    /// 声明启用的内置引擎（如 `film`）。
    #[serde(default)]
    pub engine: Option<String>,
    /// os-api semver 下限。
    #[serde(default)]
    pub min_os_api: Option<String>,
}

/// `apps` 表一行（GET /api/v1/apps 元素；前 9 个字段为冻结契约面，其余为扩展）。
#[derive(Debug, Clone, Serialize)]
pub struct AppRecord {
    pub id: String,
    pub name: String,
    pub version: String,
    pub category: String,
    pub icon: String,
    pub description: String,
    pub entry: String,
    /// 安装目录（绝对路径）。
    pub dir: String,
    /// 安装时间（ISO 8601）。
    pub installed_at: String,
    // —— 扩展字段（冻结面之外，前端可选用）——
    /// 来源仓库名（如 `nexos-app-film`）。
    pub repo: String,
    /// 启用的引擎（可空）。
    pub engine: String,
    /// 声明的 os-api 下限（可空）。
    pub min_os_api: String,
    /// 最后一次升级时间。
    pub updated_at: String,
}

/// catalog 条目（GET /api/v1/apps/catalog 元素；manifest 不可读时仅 repo +
/// installed + error）。
#[derive(Debug, Clone, Serialize)]
pub struct CatalogEntry {
    /// 仓库目录名去 `.git`（如 `nexos-app-film`）。
    pub repo: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    /// 本机是否已安装（按 repo 匹配，回退按 id 匹配）。
    pub installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    /// manifest 不可读原因（空仓库 / JSON 损坏——如实呈现，不假成功）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 安装任务（appstore `InstallTask` 框架同款：同步执行即时终态 + 可观测记录）。
#[derive(Debug, Clone, Serialize)]
pub struct AppInstallTask {
    /// 任务 id（`app-task-N`）。
    pub id: String,
    /// 应用 id（manifest 校验失败时为仓库名）。
    pub app_id: String,
    pub repo: String,
    /// `install` / `upgrade` / `noop`。
    pub action: String,
    /// `completed` / `failed`。
    pub status: String,
    pub error: Option<String>,
    /// 过程摘要（尾部）。
    pub log_tail: Option<String>,
    pub created_at: String,
}

/// POST /install 请求体。
#[derive(Debug, Deserialize)]
struct InstallBody {
    repo: String,
}

// ----------------------------------------------------------------------------
// 纯函数：校验 / mime / 路径安全（易单测）
// ----------------------------------------------------------------------------

/// manifest.id 合法性：非空、`^[a-z0-9-]+$`、≤64 字符（防路径穿越与怪字符）。
#[must_use]
pub fn valid_app_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// 语义化版本格式校验：`x.y.z`（可带 `-prerelease` / `+build`）。
#[must_use]
pub fn valid_app_version(v: &str) -> bool {
    let v = v.trim();
    if v.is_empty() || v.len() > 64 {
        return false;
    }
    let core = v.split('+').next().unwrap_or_default();
    let core = core.split('-').next().unwrap_or_default();
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) && p.len() <= 8)
}

/// entry 路径安全：非空、不以 `/` 开头、无 `..` 段、无反斜杠、无 NUL。
#[must_use]
pub fn valid_entry_path(entry: &str) -> bool {
    let e = entry.trim();
    if e.is_empty() || e.starts_with('/') || e.contains('\\') || e.contains('\0') {
        return false;
    }
    !e.split('/').any(|seg| seg == ".." || seg.is_empty())
}

/// semver 下限比较：`current >= required`（仅数字主段；预发布段忽略）。
/// 解析失败（任一侧非 x.y.z）视为满足（不因格式误拒安装，如实透传给日志）。
#[must_use]
pub fn os_api_satisfies(current: &str, required: &str) -> bool {
    let parse = |v: &str| -> Option<Vec<u64>> {
        let core = v.split('+').next()?.split('-').next()?;
        let parts: Vec<Option<u64>> = core
            .split('.')
            .map(|p| p.parse::<u64>().ok())
            .collect();
        if parts.len() != 3 || parts.iter().any(Option::is_none) {
            return None;
        }
        Some(parts.into_iter().flatten().collect())
    };
    match (parse(current), parse(required)) {
        (Some(c), Some(r)) => {
            for i in 0..3 {
                match c[i].cmp(&r[i]) {
                    std::cmp::Ordering::Greater => return true,
                    std::cmp::Ordering::Less => return false,
                    std::cmp::Ordering::Equal => continue,
                }
            }
            true
        }
        _ => true,
    }
}

/// 校验 manifest（安装闸门）；Err 为 400 文案。
pub fn validate_manifest(m: &AppManifest) -> Result<(), String> {
    if !valid_app_id(&m.id) {
        return Err(format!(
            "manifest.id 非法（须为非空小写字母/数字/连字符，≤64 字符）: {:?}",
            m.id
        ));
    }
    if m.name.trim().is_empty() {
        return Err("manifest.name 不可为空".to_string());
    }
    if !valid_app_version(&m.version) {
        return Err(format!(
            "manifest.version 版本格式非法（须为 x.y.z 三段数字，可带 -pre/+build）: {:?}",
            m.version
        ));
    }
    if !valid_entry_path(&m.entry) {
        return Err(format!(
            "manifest.entry 非法（相对路径，不得含 .. 或以 / 开头）: {:?}",
            m.entry
        ));
    }
    Ok(())
}

/// 静态资源扩展名 → MIME（白名单；未知回 octet-stream）。
#[must_use]
pub fn asset_mime(path: &str) -> &'static str {
    let ext = path
        .rsplit('.')
        .next()
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "html" | "htm" => "text/html; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// 文本类 MIME（响应体按 UTF-8 字符串直传）。
fn is_text_mime(mime: &str) -> bool {
    matches!(
        mime,
        "text/javascript"
            | "text/css"
            | "text/html; charset=utf-8"
            | "application/json"
            | "image/svg+xml"
    )
}

/// 仓库名合法性：普通名（`[A-Za-z0-9][A-Za-z0-9._-]*`，可带 .git 后缀）或
/// 完整 http(s) URL（admin 面向的直连安装源）。
#[must_use]
pub fn valid_repo_name(repo: &str) -> bool {
    let r = repo.trim();
    if r.starts_with("http://") || r.starts_with("https://") {
        return r.len() <= 500;
    }
    let r = r.strip_suffix(".git").unwrap_or(r);
    !r.is_empty()
        && r.len() <= 100
        && r.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && r.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// 规范仓库名（去 `.git` 后缀；URL 原样）。
fn normalize_repo(repo: &str) -> String {
    let r = repo.trim();
    if r.starts_with("http://") || r.starts_with("https://") {
        return r.to_string();
    }
    r.strip_suffix(".git").unwrap_or(r).to_string()
}

// ----------------------------------------------------------------------------
// AppRegistry：SQLite 注册表 + 安装/卸载/目录扫描内核（film 门控共享实例）
// ----------------------------------------------------------------------------

/// 应用注册表（`Arc` 共享：apps 组件 REST 面 + film 等引擎门控每请求直查）。
pub struct AppRegistry {
    db: std::sync::Mutex<Connection>,
    /// 应用安装根目录（env `NEXOS_APPS_DIR`，缺省 /tank/os-data/apps）。
    pub apps_dir: String,
    /// NexHub 裸仓库根（os-nexhub repos_dir 同源；catalog 扫描 + file:// clone）。
    pub repos_dir: String,
    tasks: std::sync::Mutex<Vec<AppInstallTask>>,
    counter: std::sync::Mutex<u64>,
}

impl AppRegistry {
    /// 生产构造（main.rs：env 路径，建表幂等；打开失败降级内存库不挡启动）。
    #[must_use]
    pub fn new() -> Self {
        let db_path = env_non_empty(ENV_APPS_DB).unwrap_or_else(|| DEFAULT_APPS_DB.to_string());
        let apps_dir = env_non_empty(ENV_APPS_DIR).unwrap_or_else(|| DEFAULT_APPS_DIR.to_string());
        Self::with_paths(&db_path, &apps_dir, &apps_repos_dir())
    }

    /// 指定路径构造（测试注入：临时目录隔离，不碰真实 /tank）。
    #[must_use]
    pub fn with_paths(db_path: &str, apps_dir: &str, repos_dir: &str) -> Self {
        if let Some(parent) = Path::new(db_path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = match Connection::open(db_path) {
            Ok(c) => {
                if let Err(e) = create_schema(&c) {
                    eprintln!("[apps] 建表失败（{db_path}）: {e}");
                }
                c
            }
            Err(e) => {
                eprintln!("[apps] 打开 SQLite {db_path} 失败（{e}），降级到内存库");
                let c = Connection::open_in_memory().expect("内存库必成功");
                let _ = create_schema(&c);
                c
            }
        };
        Self {
            db: std::sync::Mutex::new(conn),
            apps_dir: apps_dir.trim_end_matches('/').to_string(),
            repos_dir: repos_dir.trim_end_matches('/').to_string(),
            tasks: std::sync::Mutex::new(vec![]),
            counter: std::sync::Mutex::new(0),
        }
    }

    /// 已装应用全量快照（按 id 排序）。
    pub fn installed_apps(&self) -> Vec<AppRecord> {
        let Ok(conn) = self.db.lock() else {
            return vec![];
        };
        load_apps(&conn)
    }

    /// 按 id 查已装记录。
    pub fn find_app(&self, id: &str) -> Option<AppRecord> {
        let conn = self.db.lock().ok()?;
        find_app(&conn, id)
    }

    /// 引擎门控：是否存在已装应用声明该引擎（`engine=?` 或 `id=?` 命中即开）。
    ///
    /// 每请求直查 SQLite（无缓存）——安装/卸载在任务完成后**即时生效**；
    /// 表损坏/锁失败按未启用处理（fail-closed：门控在，能力不裸奔）。
    pub fn is_engine_enabled(&self, engine: &str) -> bool {
        let Ok(conn) = self.db.lock() else {
            return false;
        };
        conn.query_row(
            "SELECT COUNT(*) FROM apps WHERE id = ?1 OR engine = ?1",
            params![engine],
            |row| row.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(false)
    }

    /// 安装任务列表快照。
    pub fn tasks_snapshot(&self) -> Vec<AppInstallTask> {
        self.tasks.lock().expect("apps tasks poisoned").clone()
    }

    fn next_task_id(&self) -> String {
        let mut c = self.counter.lock().expect("apps counter poisoned");
        *c += 1;
        format!("app-task-{}", *c)
    }

    /// 记录安装任务（终态；appstore record_install 同款即时记录语义）。
    fn record_task(&self, task: AppInstallTask) {
        self.tasks
            .lock()
            .expect("apps tasks poisoned")
            .push(task);
    }

    /// 卸载：删目录 + 注销行。Err 为 (status, 文案)。
    pub fn uninstall(&self, id: &str) -> Result<AppRecord, (u16, String)> {
        let Some(rec) = self.find_app(id) else {
            return Err((404, format!("应用未安装: {id}")));
        };
        // 防御：登记目录必须在 apps_dir 下（历史脏数据不误删别处）
        let root = Path::new(&self.apps_dir);
        let dir = Path::new(&rec.dir);
        if !dir.starts_with(root) {
            return Err((500, format!(
                "登记目录越界，拒绝卸载: {}（不在 {} 下）",
                rec.dir, self.apps_dir
            )));
        }
        {
            let conn = self.db
                .lock()
                .map_err(|_| (500, "apps db 锁失败".to_string()))?;
            if let Err(e) = conn.execute("DELETE FROM apps WHERE id = ?1", params![id]) {
                return Err((500, format!("注销失败: {e}")));
            }
        }
        match std::fs::remove_dir_all(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                eprintln!("[apps] 卸载目录删除失败（{}）: {e}", rec.dir);
                return Ok(rec); // 行已删即视为卸载；残留目录如实提示
            }
        }
        eprintln!("[apps] 应用卸载：{}（{}）", rec.id, rec.name);
        Ok(rec)
    }

    /// 安装内核（同步执行）：clone → 校验 → 拷贝 → 登记。
    /// `Err((status, msg))` 为 HTTP 错误响应；`Ok((action, record))`。
    pub async fn install(&self, repo: &str) -> Result<(String, AppRecord), (u16, String)> {
        let repo = normalize_repo(repo);
        if !valid_repo_name(&repo) {
            return Err((
                400,
                format!("repo 非法（仅限 NexHub 仓库名或 http(s) URL）: {repo}"),
            ));
        }
        // 源解析：本机裸仓库优先 file://；http(s) URL 直连
        let clone_url = if repo.starts_with("http://") || repo.starts_with("https://") {
            repo.clone()
        } else {
            let bare = format!("{}/{}.git", self.repos_dir, repo);
            if !Path::new(&bare).is_dir() {
                return Err((
                    404,
                    format!(
                        "仓库不存在: {bare}（先在 NexHub 创建 nexos-app-* 仓库并推送 manifest.json 与 web/）"
                    ),
                ));
            }
            format!("file://{bare}")
        };
        // 1. clone 到临时目录
        let tmp = clone_target_dir();
        let clone_res = git_clone(&clone_url, &tmp).await;
        if let Err(msg) = clone_res {
            let _ = std::fs::remove_dir_all(&tmp);
            eprintln!("[apps] 安装 clone 失败（{repo}）: {msg}");
            return Err((500, format!("git clone 失败（{clone_url}）: {msg}")));
        }
        // 2. 发布根解析 + manifest 校验（仓库根优先，源码+dist 双收回退 dist/；
        //    失败清理临时目录并 400）
        let (publish_root, manifest) = match resolve_publish_root(&tmp) {
            Ok(pair) => pair,
            Err(msg) => {
                let _ = std::fs::remove_dir_all(&tmp);
                eprintln!("[apps] 安装 manifest 校验失败（{repo}）: {msg}");
                return Err((400, format!("manifest 校验失败（{repo}）: {msg}")));
            }
        };
        // 3. min_os_api 下限
        if let Some(req) = manifest.min_os_api.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            if !os_api_satisfies(env!("CARGO_PKG_VERSION"), req) {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err((
                    400,
                    format!(
                        "manifest.min_os_api={req} 高于当前 os-api {}，拒绝安装",
                        env!("CARGO_PKG_VERSION")
                    ),
                ));
            }
        }
        // 4. 幂等：同 id 已装？
        if let Some(existing) = self.find_app(&manifest.id) {
            if existing.repo != repo {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err((
                    409,
                    format!(
                        "应用 id「{}」已安装（来源仓库 {}），与本次来源 {repo} 不同，拒绝覆盖",
                        manifest.id, existing.repo
                    ),
                ));
            }
            if existing.version == manifest.version {
                // 同版本重复安装 = no-op
                let _ = std::fs::remove_dir_all(&tmp);
                return Ok(("noop".to_string(), existing));
            }
        }
        // 5. 拷贝（发布根下除 .git 外全部）到 apps_dir/<id>/（升级=先清后拷）
        let dest = Path::new(&self.apps_dir).join(&manifest.id);
        let root_for_copy = publish_root.clone();
        let dest_for_copy = dest.clone();
        let copy_res = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            if dest_for_copy.exists() {
                std::fs::remove_dir_all(&dest_for_copy)?;
            }
            std::fs::create_dir_all(&dest_for_copy)?;
            copy_tree_excluding_git(&root_for_copy, &dest_for_copy)?;
            Ok(())
        })
        .await;
        let _ = std::fs::remove_dir_all(&tmp);
        let existed_before = self.find_app(&manifest.id).is_some();
        match copy_res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                eprintln!("[apps] 安装拷贝失败（{}/{}) : {e}", self.apps_dir, manifest.id);
                return Err((500, format!("安装文件落盘失败: {e}")));
            }
            Err(e) => return Err((500, format!("安装拷贝任务 join 失败: {e}"))),
        }
        // 6. 登记（upsert）
        let now = now_iso();
        let record = AppRecord {
            id: manifest.id.clone(),
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            category: if manifest.category.trim().is_empty() {
                "custom".to_string()
            } else {
                manifest.category.trim().to_string()
            },
            icon: if manifest.icon.trim().is_empty() {
                "📦".to_string()
            } else {
                manifest.icon.trim().to_string()
            },
            description: manifest.description.trim().to_string(),
            entry: manifest.entry.trim().to_string(),
            dir: dest.to_string_lossy().to_string(),
            installed_at: self
                .find_app(&manifest.id)
                .map(|r| r.installed_at)
                .unwrap_or_else(|| now.clone()),
            repo: repo.clone(),
            engine: manifest.engine.clone().unwrap_or_default(),
            min_os_api: manifest.min_os_api.clone().unwrap_or_default(),
            updated_at: now.clone(),
        };
        {
            let Ok(conn) = self.db.lock() else {
                return Err((500, "apps db 锁失败".to_string()));
            };
            if let Err(e) = upsert_app(&conn, &record) {
                return Err((500, format!("应用登记失败: {e}")));
            }
        }
        let action = if existed_before { "upgrade" } else { "install" };
        eprintln!(
            "[apps] 应用{}：{} v{}（{}，目录 {}）",
            if action == "upgrade" { "升级" } else { "安装" },
            record.id,
            record.version,
            record.repo,
            record.dir
        );
        Ok((action.to_string(), record))
    }

    /// catalog 扫描内核：`<repos_dir>/nexos-app-*.git` 逐仓 `git show
    /// HEAD:manifest.json`（空仓库/损坏如实落 error 字段，不假成功）。
    pub fn scan_catalog(&self) -> Vec<CatalogEntry> {
        let installed = self.installed_apps();
        scan_catalog_blocking(&self.repos_dir, &installed)
    }
}

impl Default for AppRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ----------------------------------------------------------------------------
// SQLite 层
// ----------------------------------------------------------------------------

fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS apps (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            version TEXT NOT NULL,
            category TEXT NOT NULL DEFAULT 'custom',
            icon TEXT NOT NULL DEFAULT '📦',
            description TEXT NOT NULL DEFAULT '',
            entry TEXT NOT NULL,
            repo TEXT NOT NULL,
            engine TEXT NOT NULL DEFAULT '',
            min_os_api TEXT NOT NULL DEFAULT '',
            dir TEXT NOT NULL,
            installed_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );",
    )
}

const APP_COLS: &str = "id,name,version,category,icon,description,entry,repo,engine,min_os_api,dir,installed_at,updated_at";

fn row_to_app(row: &rusqlite::Row<'_>) -> rusqlite::Result<AppRecord> {
    Ok(AppRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        version: row.get(2)?,
        category: row.get(3)?,
        icon: row.get(4)?,
        description: row.get(5)?,
        entry: row.get(6)?,
        repo: row.get(7)?,
        engine: row.get(8)?,
        min_os_api: row.get(9)?,
        dir: row.get(10)?,
        installed_at: row.get(11)?,
        updated_at: row.get(12)?,
    })
}

fn load_apps(conn: &Connection) -> Vec<AppRecord> {
    let Ok(mut stmt) = conn.prepare(&format!(
        "SELECT {APP_COLS} FROM apps ORDER BY id"
    )) else {
        return vec![];
    };
    stmt.query_map([], row_to_app)
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

fn find_app(conn: &Connection, id: &str) -> Option<AppRecord> {
    conn.query_row(
        &format!("SELECT {APP_COLS} FROM apps WHERE id = ?1"),
        params![id],
        row_to_app,
    )
    .ok()
}

fn upsert_app(conn: &Connection, r: &AppRecord) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO apps (id,name,version,category,icon,description,entry,repo,engine,min_os_api,dir,installed_at,updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
         ON CONFLICT(id) DO UPDATE SET
           name=?2,version=?3,category=?4,icon=?5,description=?6,entry=?7,repo=?8,
           engine=?9,min_os_api=?10,dir=?11,updated_at=?13",
        params![
            r.id,
            r.name,
            r.version,
            r.category,
            r.icon,
            r.description,
            r.entry,
            r.repo,
            r.engine,
            r.min_os_api,
            r.dir,
            r.installed_at,
            r.updated_at
        ],
    )?;
    Ok(())
}

// ----------------------------------------------------------------------------
// git / 文件系统辅助
// ----------------------------------------------------------------------------

/// 临时 clone 目录（temp 下唯一名，防并行安装互踩）。
fn clone_target_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("nexos-app-install-{}-{nanos}", std::process::id()))
}

/// `git clone --depth 1 <url> <dst>`（tokio 子进程 + 超时；输出尾部带出）。
async fn git_clone(url: &str, dst: &Path) -> Result<(), String> {
    let url = url.to_string();
    let dst = dst.to_path_buf();
    let fut = tokio::process::Command::new("git")
        .arg("clone")
        .arg("--depth")
        .arg("1")
        .arg(&url)
        .arg(&dst)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output();
    let out = tokio::time::timeout(Duration::from_secs(CLONE_TIMEOUT_SECS), fut)
        .await
        .map_err(|_| format!("clone 超时（>{CLONE_TIMEOUT_SECS}s）"))?
        .map_err(|e| format!("git 进程启动失败（git 未安装？）: {e}"))?;
    if !out.status.success() {
        let tail = tail_lines(&String::from_utf8_lossy(&out.stderr), 5);
        return Err(format!("退出码 {:?}: {}", out.status.code(), tail));
    }
    Ok(())
}

/// 输出尾部 N 行。
fn tail_lines(s: &str, n: usize) -> String {
    s.lines()
        .rev()
        .take(n)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

/// 读指定发布根的 manifest.json + 三重校验（字段合法 / entry 文件存在 /
/// manifest 可解析）。
fn read_manifest_at(root: &Path) -> Result<AppManifest, String> {
    let path = root.join("manifest.json");
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("读取 manifest.json 失败: {e}"))?;
    let m: AppManifest =
        serde_json::from_str(&raw).map_err(|e| format!("manifest.json 解析失败: {e}"))?;
    validate_manifest(&m)?;
    let entry = root.join(m.entry.trim());
    if !entry.is_file() {
        return Err(format!("entry 文件不存在于包内: {}（{entry:?}）", m.entry));
    }
    Ok(m)
}

/// 发布根解析（2026-09-04 与前端 apps/film 实况对齐）：
///
/// 应用仓库有两种形态，按序探测：
/// 1. **发布根=仓库根**（纯产物仓）：根 `manifest.json` + entry 在根下。
/// 2. **源码+dist 双收**（apps/film 形态，README 明示「dist/ 是安装产物」）：
///    根 manifest 声明 `entry:"web/entry.js"` 但产物在 `dist/web/entry.js`
///    （dist/manifest.json 同步拷贝）——根校验失败且 `dist/` 下自洽时，取
///    `dist/` 为发布根（只拷贝产物，src/node_modules 不进安装目录）。
///
/// 两种形态都失败 → Err（透出**仓库根**的校验错误——主形态诊断信息优先）。
fn resolve_publish_root(clone_dir: &Path) -> Result<(PathBuf, AppManifest), String> {
    let root_err = match read_manifest_at(clone_dir) {
        Ok(m) => return Ok((clone_dir.to_path_buf(), m)),
        Err(e) => e,
    };
    let dist = clone_dir.join("dist");
    if dist.is_dir() {
        if let Ok(m) = read_manifest_at(&dist) {
            eprintln!(
                "[apps] 发布根解析：{} 为源码+dist 双收仓库，取 dist/ 为发布根",
                m.id
            );
            return Ok((dist, m));
        }
    }
    Err(root_err)
}

/// 递归拷贝（跳过任何层级的 `.git` 目录）。
fn copy_tree_excluding_git(src: &Path, dst: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == ".git" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(name.as_ref());
        if from.is_dir() {
            std::fs::create_dir_all(&to)?;
            copy_tree_excluding_git(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// catalog 扫描（阻塞内核；handler 侧 spawn_blocking 包装）。
fn scan_catalog_blocking(repos_dir: &str, installed: &[AppRecord]) -> Vec<CatalogEntry> {
    let mut repos: Vec<String> = std::fs::read_dir(repos_dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| n.starts_with(CATALOG_REPO_PREFIX) && n.ends_with(".git"))
                .collect()
        })
        .unwrap_or_default();
    repos.sort();
    let mut out = Vec::new();
    for name in repos {
        let repo = name.trim_end_matches(".git").to_string();
        let bare = format!("{repos_dir}/{name}");
        let manifest = git_show_manifest(&bare);
        let (by_repo, by_id) = (
            installed.iter().find(|a| a.repo == repo),
            manifest
                .as_ref()
                .ok()
                .and_then(|m| installed.iter().find(|a| a.id == m.id)),
        );
        let hit = by_repo.or(by_id);
        let entry = match &manifest {
            Ok(m) => CatalogEntry {
                repo: repo.clone(),
                id: Some(m.id.clone()),
                name: Some(m.name.clone()),
                version: Some(m.version.clone()),
                category: if m.category.trim().is_empty() {
                    None
                } else {
                    Some(m.category.trim().to_string())
                },
                icon: if m.icon.trim().is_empty() {
                    None
                } else {
                    Some(m.icon.trim().to_string())
                },
                description: if m.description.trim().is_empty() {
                    None
                } else {
                    Some(m.description.trim().to_string())
                },
                engine: m.engine.clone().filter(|e| !e.trim().is_empty()),
                installed: hit.is_some(),
                installed_version: hit.map(|a| a.version.clone()),
                error: None,
            },
            Err(msg) => CatalogEntry {
                repo: repo.clone(),
                id: None,
                name: None,
                version: None,
                category: None,
                icon: None,
                description: None,
                engine: None,
                installed: installed.iter().any(|a| a.repo == repo),
                installed_version: installed
                    .iter()
                    .find(|a| a.repo == repo)
                    .map(|a| a.version.clone()),
                error: Some(msg.clone()),
            },
        };
        out.push(entry);
    }
    out
}

/// `git --git-dir=<bare> show HEAD:manifest.json`（无 HEAD / 无文件 / 损坏 → Err）。
fn git_show_manifest(bare: &str) -> Result<AppManifest, String> {
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(bare)
        .arg("show")
        .arg("HEAD:manifest.json")
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("git 进程启动失败: {e}"))?;
    if !out.status.success() {
        return Err("manifest 不可读（空仓库或未推送 manifest.json）".to_string());
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(&raw).map_err(|e| format!("manifest 解析失败: {e}"))
}

/// 静态资源服务内核（阻塞读 + 穿越防护；handler 侧 spawn_blocking 包装）。
///
/// 路径安全三道闸：id/子路径字符白名单 → 拒 `..` 段 → canonicalize 后必须
/// 仍在 `<apps_dir>/<id>/web/` 内（符号链接逃逸也拦得住）。
///
/// URL 约定：`<path>` 相对应用 `web/` 目录；**兼容剥前导 `web/` 段**——前端
/// 运行时把 manifest.entry 原样拼进 URL（`/apps-assets/film/web/entry.js`），
/// 剥段后与 `/apps-assets/film/entry.js` 同指 `web/entry.js`（两种写法等价）。
/// `web/` 之外的包根文件（manifest.json 等）不可达。
pub fn serve_asset_blocking(apps_dir: &str, id: &str, sub: &str) -> Result<(Vec<u8>, &'static str), (u16, String)> {
    if !valid_app_id(id) {
        return Err((404, "应用不存在".to_string()));
    }
    let sub = sub.trim_start_matches('/');
    if sub.is_empty() || sub.contains('\0') || sub.contains('\\') {
        return Err((404, "资源不存在".to_string()));
    }
    if sub.split('/').any(|seg| seg == ".." || seg.is_empty()) {
        return Err((404, "资源不存在".to_string()));
    }
    // 兼容剥前导 web/ 段（manifest.entry 含目录前缀时 URL 与 web/ 根对齐）
    let rel = sub.strip_prefix("web/").unwrap_or(sub);
    let root = Path::new(apps_dir).join(id).join("web");
    let root_canon = root
        .canonicalize()
        .map_err(|_| (404, format!("应用未安装或资源不存在: {id}")))?;
    let target = root_canon.join(rel);
    let target_canon = target
        .canonicalize()
        .map_err(|_| (404, "资源不存在".to_string()))?;
    if !target_canon.starts_with(&root_canon) || !target_canon.is_file() {
        return Err((404, "资源不存在".to_string()));
    }
    let bytes = std::fs::read(&target_canon)
        .map_err(|e| (404, format!("资源读取失败: {e}")))?;
    Ok((bytes, asset_mime(&target_canon.to_string_lossy())))
}

// ----------------------------------------------------------------------------
// AppsRouteHandler（HTTP 适配器；与 film 门控共享 AppRegistry 实例）
// ----------------------------------------------------------------------------

/// 应用包运行时路由处理器——安装/卸载/清单/catalog/静态托管。
pub struct AppsRouteHandler {
    /// 共享注册表（main.rs 构造后同时注入 film 门控；同一 SQLite）。
    pub registry: Arc<AppRegistry>,
}

impl AppsRouteHandler {
    /// 生产构造。
    #[must_use]
    pub fn new(registry: Arc<AppRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl RouteHandler for AppsRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec_public(HttpMethod::Get, "/api/v1/apps"),
            spec_admin(HttpMethod::Post, "/api/v1/apps/install"),
            spec_admin(HttpMethod::Delete, "/api/v1/apps/:id"),
            spec_public(HttpMethod::Get, "/api/v1/apps/catalog"),
            spec_public(HttpMethod::Get, "/api/v1/apps/tasks"),
            spec_public(HttpMethod::Get, "/apps-assets/:id/*"),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            // —— GET /api/v1/apps —— 已装列表（冻结契约形态 {"apps":[…]}）
            (HttpMethod::Get, ["api", "v1", "apps"]) => {
                let apps = self.registry.installed_apps();
                Ok(ok_json(serde_json::json!({ "apps": apps })))
            }

            // —— POST /api/v1/apps/install —— 同步安装/升级 + 任务记录
            (HttpMethod::Post, ["api", "v1", "apps", "install"]) => {
                let body: InstallBody = serde_json::from_value(req.body)
                    .map_err(|e| ApiGatewayError::Internal(format!("解析安装请求体失败: {e}")))?;
                if body.repo.trim().is_empty() {
                    return Ok(error_response(400, "repo 不可为空"));
                }
                let repo = normalize_repo(&body.repo);
                let (action, record, status, task) = match self.registry.install(&repo).await {
                    Ok((action, rec)) => {
                        let task = AppInstallTask {
                            id: self.registry.next_task_id(),
                            app_id: rec.id.clone(),
                            repo: repo.clone(),
                            action: action.clone(),
                            status: "completed".into(),
                            error: None,
                            log_tail: Some(format!(
                                "{} v{}（来源 {repo}）",
                                if action == "upgrade" { "升级到" } else { "已安装" },
                                rec.version
                            )),
                            created_at: now_iso(),
                        };
                        let status = if action == "noop" { 200 } else { 201 };
                        (action, rec, status, task)
                    }
                    Err((code, msg)) => {
                        let task = AppInstallTask {
                            id: self.registry.next_task_id(),
                            app_id: repo.clone(),
                            repo: repo.clone(),
                            action: "install".into(),
                            status: "failed".into(),
                            error: Some(msg.clone()),
                            log_tail: Some(msg.clone()),
                            created_at: now_iso(),
                        };
                        self.registry.record_task(task);
                        return Ok(error_response(code, &msg));
                    }
                };
                self.registry.record_task(task);
                Ok(ApiResponse {
                    status,
                    body: serde_json::json!({
                        "ok": true,
                        "action": action,
                        "app": record,
                    }),
                    headers: serde_json::json!({}),
                })
            }

            // —— DELETE /api/v1/apps/:id —— 卸载（删目录 + 注销）
            (HttpMethod::Delete, ["api", "v1", "apps", id]) => {
                match self.registry.uninstall(id) {
                    Ok(rec) => Ok(ok_json(serde_json::json!({
                        "ok": true,
                        "id": rec.id,
                        "dir": rec.dir,
                        "action": "uninstall",
                    }))),
                    Err((code, msg)) => Ok(error_response(code, &msg)),
                }
            }

            // —— GET /api/v1/apps/catalog —— 扫 NexHub nexos-app-* 裸仓库
            (HttpMethod::Get, ["api", "v1", "apps", "catalog"]) => {
                let reg = Arc::clone(&self.registry);
                let apps = tokio::task::spawn_blocking(move || reg.scan_catalog())
                    .await
                    .map_err(|e| {
                        ApiGatewayError::Internal(format!("catalog 扫描任务 join 失败: {e}"))
                    })?;
                Ok(ok_json(serde_json::json!({ "apps": apps })))
            }

            // —— GET /api/v1/apps/tasks —— 安装任务列表（appstore 任务面同款）
            (HttpMethod::Get, ["api", "v1", "apps", "tasks"]) => {
                Ok(ok_json(serde_json::json!({ "tasks": self.registry.tasks_snapshot() })))
            }

            // —— GET /apps-assets/:id/* —— 应用静态资源（web/ 下，防穿越）
            (HttpMethod::Get, ["apps-assets", id, rest @ ..]) => {
                let sub = rest.join("/");
                let apps_dir = self.registry.apps_dir.clone();
                let id = id.to_string();
                let res = tokio::task::spawn_blocking(move || {
                    serve_asset_blocking(&apps_dir, &id, &sub)
                })
                .await
                .map_err(|e| ApiGatewayError::Internal(format!("资源读取任务 join 失败: {e}")))?;
                match res {
                    Ok((bytes, mime)) => {
                        if is_text_mime(mime) {
                            Ok(ApiResponse {
                                status: 200,
                                body: serde_json::Value::String(
                                    String::from_utf8_lossy(&bytes).into_owned(),
                                ),
                                headers: serde_json::json!({ "content-type": mime }),
                            })
                        } else {
                            use base64::Engine;
                            Ok(ApiResponse {
                                status: 200,
                                body: serde_json::Value::String(
                                    base64::engine::general_purpose::STANDARD.encode(&bytes),
                                ),
                                headers: serde_json::json!({ "content-type": mime }),
                            })
                        }
                    }
                    Err((code, msg)) => Ok(error_response(code, &msg)),
                }
            }

            // —— 未覆盖路由 —— 兜底 404
            _ => Ok(error_response(404, "apps: 未匹配的路由")),
        }
    }
}

// ----------------------------------------------------------------------------
// 内部辅助
// ----------------------------------------------------------------------------

fn spec_admin(method: HttpMethod, path: &str) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: "apps".to_string(),
        requires_auth: true,
        required_roles: vec!["admin".into()],
    }
}

fn spec_public(method: HttpMethod, path: &str) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: "apps".to_string(),
        requires_auth: false,
        required_roles: vec![],
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

fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

fn now_iso() -> String {
    use chrono::Local;
    Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

// ----------------------------------------------------------------------------
// 单元测试（真实 git 裸仓库 fixture；不碰真实 /tank——全部临时目录隔离）
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

    fn temp_dir_for(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nexos-apps-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn run_git(args: &[&str]) -> bool {
        matches!(
            std::process::Command::new(args[0]).args(&args[1..]).output(),
            Ok(o) if o.status.success()
        )
    }

    /// 建 NexHub 裸仓库 fixture：manifest.json（可覆写字段）+ web/entry.js。
    /// 返回 (repos_dir, repo 名)。
    fn make_app_repo(dir: &Path, repo: &str, manifest_overrides: serde_json::Value) -> (PathBuf, String) {
        let repos = dir.join("repos");
        std::fs::create_dir_all(&repos).unwrap();
        let bare = repos.join(format!("{repo}.git"));
        assert!(run_git(&["git", "init", "--bare", bare.to_str().unwrap()]), "init bare");
        // HEAD → main（code_repo 建仓同款：否则 clone 提示 "remote HEAD refers
        // to nonexistent ref"，工作树为空）
        assert!(run_git(&[
            "git", "--git-dir", bare.to_str().unwrap(),
            "symbolic-ref", "HEAD", "refs/heads/main"
        ]));
        let work = dir.join(format!(".{repo}-work"));
        std::fs::create_dir_all(work.join("web")).unwrap();
        let mut manifest = serde_json::json!({
            "id": "demo-app",
            "name": "演示应用",
            "version": "0.1.0",
            "category": "media",
            "icon": "🎬",
            "description": "安装测试用演示应用",
            "entry": "web/entry.js",
        });
        if let (Some(obj), Some(dst)) = (manifest_overrides.as_object(), manifest.as_object_mut()) {
            for (k, v) in obj {
                dst.insert(k.clone(), v.clone());
            }
        }
        std::fs::write(work.join("manifest.json"), manifest.to_string()).unwrap();
        std::fs::write(work.join("web/entry.js"), "export default { mount(){} };").unwrap();
        assert!(run_git(&[
            "git", "-c", "init.defaultBranch=main", "init", work.to_str().unwrap()
        ]));
        assert!(run_git(&["git", "-C", work.to_str().unwrap(), "add", "-A"]));
        assert!(run_git(&[
            "git", "-C", work.to_str().unwrap(),
            "-c", "user.name=T", "-c", "user.email=t@t",
            "commit", "-m", "init"
        ]));
        assert!(run_git(&[
            "git", "-C", work.to_str().unwrap(),
            "push", bare.to_str().unwrap(), "HEAD:main"
        ]));
        let _ = std::fs::remove_dir_all(&work);
        (repos, repo.to_string())
    }

    fn handler_at(test: &str) -> (AppsRouteHandler, PathBuf) {
        let dir = temp_dir_for(test);
        let reg = Arc::new(AppRegistry::with_paths(
            dir.join("apps.db").to_str().unwrap(),
            dir.join("apps-root").to_str().unwrap(),
            dir.join("repos").to_str().unwrap(),
        ));
        (AppsRouteHandler::new(reg), dir)
    }

    // ---- 校验纯函数 ----

    #[test]
    fn valid_app_id_rules() {
        assert!(valid_app_id("film"));
        assert!(valid_app_id("nexos-app-film"));
        assert!(valid_app_id("a1-b2"));
        assert!(!valid_app_id(""));
        assert!(!valid_app_id("Film"));
        assert!(!valid_app_id("has space"));
        assert!(!valid_app_id("../escape"));
        assert!(!valid_app_id("a/b"));
        assert!(!valid_app_id(&"x".repeat(65)));
    }

    #[test]
    fn valid_app_version_rules() {
        assert!(valid_app_version("0.1.0"));
        assert!(valid_app_version("1.2.3-beta.1"));
        assert!(valid_app_version("10.20.30+build.5"));
        assert!(!valid_app_version(""));
        assert!(!valid_app_version("1.2"));
        assert!(!valid_app_version("v1.2.3"));
        assert!(!valid_app_version("1.2.x"));
        assert!(!valid_app_version("1.2.3.4"));
    }

    #[test]
    fn valid_entry_path_rules() {
        assert!(valid_entry_path("web/entry.js"));
        assert!(valid_entry_path("web/assets/app.css"));
        assert!(!valid_entry_path(""));
        assert!(!valid_entry_path("/abs/entry.js"));
        assert!(!valid_entry_path("web/../secret.js"));
        assert!(!valid_entry_path("web//entry.js"));
        assert!(!valid_entry_path("web\\entry.js"));
    }

    #[test]
    fn os_api_satisfies_semver_compare() {
        assert!(os_api_satisfies("0.1.25", "0.1.0"));
        assert!(os_api_satisfies("0.1.25", "0.1.25"));
        assert!(!os_api_satisfies("0.1.24", "0.1.25"));
        assert!(!os_api_satisfies("0.2.0", "1.0.0"));
        // 格式失败 → 不拦（透传日志）
        assert!(os_api_satisfies("0.1.25", "latest"));
    }

    #[test]
    fn asset_mime_whitelist() {
        assert_eq!(asset_mime("entry.js"), "text/javascript");
        assert_eq!(asset_mime("app.mjs"), "text/javascript");
        assert_eq!(asset_mime("style.css"), "text/css");
        assert_eq!(asset_mime("standalone.html"), "text/html; charset=utf-8");
        assert_eq!(asset_mime("index.htm"), "text/html; charset=utf-8");
        assert_eq!(asset_mime("i18n.json"), "application/json");
        assert_eq!(asset_mime("logo.svg"), "image/svg+xml");
        assert_eq!(asset_mime("icon.png"), "image/png");
        assert_eq!(asset_mime("font.woff2"), "font/woff2");
        assert_eq!(asset_mime("blob.bin"), "application/octet-stream");
    }

    #[test]
    fn valid_repo_name_rules() {
        assert!(valid_repo_name("nexos-app-film"));
        assert!(valid_repo_name("nexos-app-film.git"));
        assert!(valid_repo_name("https://example.com/a/b.git"));
        assert!(!valid_repo_name(""));
        assert!(!valid_repo_name("../etc"));
        assert!(!valid_repo_name("a/b"));
        assert!(!valid_repo_name(".hidden"));
    }

    #[test]
    fn validate_manifest_rejects_bad_shapes() {
        let mut m = AppManifest {
            id: "demo".into(),
            name: "演示".into(),
            version: "0.1.0".into(),
            category: "media".into(),
            icon: "🎬".into(),
            description: "".into(),
            entry: "web/entry.js".into(),
            engine: None,
            min_os_api: None,
        };
        assert!(validate_manifest(&m).is_ok());
        m.id = "UPPER".into();
        assert!(validate_manifest(&m).is_err());
        m.id = "demo".into();
        m.version = "1.2".into();
        assert!(validate_manifest(&m).is_err());
        m.version = "0.1.0".into();
        m.entry = "../x.js".into();
        assert!(validate_manifest(&m).is_err());
        m.entry = "web/entry.js".into();
        m.name = "  ".into();
        assert!(validate_manifest(&m).is_err());
    }

    // ---- 路由声明 ----

    #[tokio::test]
    async fn routes_declares_six_endpoints_with_auth_matrix() {
        let h = handler_at("routes").0;
        let routes = h.routes().await;
        assert_eq!(routes.len(), 6, "{routes:?}");
        assert!(routes.iter().all(|r| r.handler_component == "apps"));
        for r in &routes {
            match r.method {
                HttpMethod::Post | HttpMethod::Delete => {
                    assert!(r.requires_auth, "写操作需 admin: {r:?}");
                    assert_eq!(r.required_roles, vec!["admin".to_string()]);
                }
                _ => assert!(!r.requires_auth, "读公开: {r:?}"),
            }
        }
        assert!(routes
            .iter()
            .any(|r| r.path == "/apps-assets/:id/*" && r.method == HttpMethod::Get));
    }

    // ---- 安装 / 幂等 / 升级 / 卸载（真实 git fixture）----

    #[tokio::test]
    async fn install_uninstall_roundtrip_with_real_git_repo() {
        let dir = temp_dir_for("roundtrip");
        let (_repos, _) = make_app_repo(&dir, "nexos-app-demo", serde_json::json!({}));
        let h = AppsRouteHandler::new(Arc::new(AppRegistry::with_paths(
            dir.join("apps.db").to_str().unwrap(),
            dir.join("apps-root").to_str().unwrap(),
            dir.join("repos").to_str().unwrap(),
        )));
        // 初始为空
        let resp = h.handle(get_req("/api/v1/apps")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["apps"].as_array().unwrap().len(), 0);
        // 安装
        let resp = h
            .handle(post_req(
                "/api/v1/apps/install",
                serde_json::json!({"repo": "nexos-app-demo"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "{resp:?}");
        assert_eq!(resp.body["action"], "install");
        assert_eq!(resp.body["app"]["id"], "demo-app");
        assert_eq!(resp.body["app"]["version"], "0.1.0");
        // 文件落位（.git 不拷贝 / entry 存在）
        let app_dir = resp.body["app"]["dir"].as_str().unwrap();
        assert!(Path::new(app_dir).join("web/entry.js").is_file());
        assert!(!Path::new(app_dir).join(".git").exists());
        // GET /apps 出现该应用（冻结契约字段齐）
        let resp = h.handle(get_req("/api/v1/apps")).await.unwrap();
        let apps = resp.body["apps"].as_array().unwrap();
        assert_eq!(apps.len(), 1);
        for key in [
            "id", "name", "version", "category", "icon", "description", "entry", "dir",
            "installed_at",
        ] {
            assert!(apps[0][key].is_string(), "缺冻结字段 {key}: {apps:?}");
        }
        // 任务面可见 completed
        let resp = h.handle(get_req("/api/v1/apps/tasks")).await.unwrap();
        let tasks = resp.body["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["status"], "completed");
        // 卸载 → 目录删 + 行删
        let resp = h
            .handle(del_req("/api/v1/apps/demo-app"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{resp:?}");
        assert!(!Path::new(app_dir).exists());
        let resp = h.handle(get_req("/api/v1/apps")).await.unwrap();
        assert_eq!(resp.body["apps"].as_array().unwrap().len(), 0);
        // 再卸载 → 404
        let resp = h
            .handle(del_req("/api/v1/apps/demo-app"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn install_same_version_is_noop_and_upgrade_overwrites() {
        let dir = temp_dir_for("idempotent");
        let (_r, _) = make_app_repo(&dir, "nexos-app-demo", serde_json::json!({}));
        let h = AppsRouteHandler::new(Arc::new(AppRegistry::with_paths(
            dir.join("apps.db").to_str().unwrap(),
            dir.join("apps-root").to_str().unwrap(),
            dir.join("repos").to_str().unwrap(),
        )));
        // 首装
        let resp = h
            .handle(post_req(
                "/api/v1/apps/install",
                serde_json::json!({"repo": "nexos-app-demo"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        // 同版本重装 → 200 noop
        let resp = h
            .handle(post_req(
                "/api/v1/apps/install",
                serde_json::json!({"repo": "nexos-app-demo"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{resp:?}");
        assert_eq!(resp.body["action"], "noop");
        // 推新版本 → upgrade 201 + 版本更新
        push_version_bump(&dir, "nexos-app-demo", "0.2.0");
        let resp = h
            .handle(post_req(
                "/api/v1/apps/install",
                serde_json::json!({"repo": "nexos-app-demo"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "{resp:?}");
        assert_eq!(resp.body["action"], "upgrade");
        assert_eq!(resp.body["app"]["version"], "0.2.0");
        // installed_at 保持首装时间
        let resp = h.handle(get_req("/api/v1/apps")).await.unwrap();
        let apps = resp.body["apps"].as_array().unwrap();
        assert_eq!(apps[0]["version"], "0.2.0");
    }

    /// 往既有 fixture 仓库追加一个新版本 commit（改 manifest version）。
    fn push_version_bump(dir: &Path, repo: &str, version: &str) {
        let bare = dir.join("repos").join(format!("{repo}.git"));
        let work = dir.join(format!(".{repo}-bump"));
        let _ = std::fs::remove_dir_all(&work);
        assert!(run_git(&[
            "git", "clone", bare.to_str().unwrap(), work.to_str().unwrap()
        ]));
        let manifest_path = work.join("manifest.json");
        let mut m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
        m["version"] = serde_json::json!(version);
        std::fs::write(&manifest_path, m.to_string()).unwrap();
        assert!(run_git(&["git", "-C", work.to_str().unwrap(), "add", "-A"]));
        assert!(run_git(&[
            "git", "-C", work.to_str().unwrap(),
            "-c", "user.name=T", "-c", "user.email=t@t",
            "commit", "-m", "bump"
        ]));
        assert!(run_git(&[
            "git", "-C", work.to_str().unwrap(),
            "push", "origin", "HEAD:main"
        ]));
        let _ = std::fs::remove_dir_all(&work);
    }

    #[tokio::test]
    async fn install_rejects_repo_conflict_and_missing_repo() {
        let dir = temp_dir_for("conflict");
        // 两个仓库声明同一个 app id
        let _ = make_app_repo(&dir, "nexos-app-a", serde_json::json!({"id": "clash"}));
        let _ = make_app_repo(&dir, "nexos-app-b", serde_json::json!({"id": "clash"}));
        let h = AppsRouteHandler::new(Arc::new(AppRegistry::with_paths(
            dir.join("apps.db").to_str().unwrap(),
            dir.join("apps-root").to_str().unwrap(),
            dir.join("repos").to_str().unwrap(),
        )));
        let resp = h
            .handle(post_req(
                "/api/v1/apps/install",
                serde_json::json!({"repo": "nexos-app-a"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "{resp:?}");
        // 同 id 不同 repo → 409
        let resp = h
            .handle(post_req(
                "/api/v1/apps/install",
                serde_json::json!({"repo": "nexos-app-b"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 409, "{resp:?}");
        // 仓库不存在 → 404
        let resp = h
            .handle(post_req(
                "/api/v1/apps/install",
                serde_json::json!({"repo": "nexos-app-nope"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404, "{resp:?}");
        // 非法名 → 400
        let resp = h
            .handle(post_req(
                "/api/v1/apps/install",
                serde_json::json!({"repo": "../etc/passwd"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "{resp:?}");
        // 空体 → 400
        let resp = h
            .handle(post_req(
                "/api/v1/apps/install",
                serde_json::json!({"repo": ""}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    /// 建「源码+dist 双收」仓库 fixture（apps/film 实况形态）：根 manifest
    /// 声明 entry=web/entry.js 但产物只在 dist/web/（dist/manifest.json 同步
    /// 拷贝），src/ 是源码（不应进安装目录）。
    fn make_dual_layout_repo(dir: &Path, repo: &str) -> (PathBuf, String) {
        let repos = dir.join("repos");
        std::fs::create_dir_all(&repos).unwrap();
        let bare = repos.join(format!("{repo}.git"));
        assert!(run_git(&["git", "init", "--bare", bare.to_str().unwrap()]));
        assert!(run_git(&[
            "git", "--git-dir", bare.to_str().unwrap(),
            "symbolic-ref", "HEAD", "refs/heads/main"
        ]));
        let work = dir.join(format!(".{repo}-work"));
        let manifest = serde_json::json!({
            "id": "dual-app",
            "name": "双收形态应用",
            "version": "0.1.0",
            "category": "media",
            "icon": "🎬",
            "description": "源码+dist 双收形态",
            "entry": "web/entry.js",
        });
        // 根 manifest + src/（源码） + dist/（产物：manifest + web/entry.js）
        std::fs::create_dir_all(work.join("src")).unwrap();
        std::fs::create_dir_all(work.join("dist/web")).unwrap();
        std::fs::write(work.join("manifest.json"), manifest.to_string()).unwrap();
        std::fs::write(work.join("src/App.vue"), "<template/>").unwrap();
        std::fs::write(work.join("dist/manifest.json"), manifest.to_string()).unwrap();
        std::fs::write(work.join("dist/web/entry.js"), "export default function register(){}")
            .unwrap();
        assert!(run_git(&["git", "-c", "init.defaultBranch=main", "init", work.to_str().unwrap()]));
        assert!(run_git(&["git", "-C", work.to_str().unwrap(), "add", "-A"]));
        assert!(run_git(&[
            "git", "-C", work.to_str().unwrap(),
            "-c", "user.name=T", "-c", "user.email=t@t",
            "commit", "-m", "init"
        ]));
        assert!(run_git(&[
            "git", "-C", work.to_str().unwrap(),
            "push", bare.to_str().unwrap(), "HEAD:main"
        ]));
        let _ = std::fs::remove_dir_all(&work);
        (repos, repo.to_string())
    }

    #[tokio::test]
    async fn install_resolves_dist_publish_root_for_dual_layout_repo() {
        // apps/film 实况：根 manifest 的 entry 只存在于 dist/web/ → 发布根
        // 回退 dist/，安装目录只含产物（src 不拷贝），静态托管可命中。
        let dir = temp_dir_for("dual");
        let _ = make_dual_layout_repo(&dir, "nexos-app-dual");
        let h = AppsRouteHandler::new(Arc::new(AppRegistry::with_paths(
            dir.join("apps.db").to_str().unwrap(),
            dir.join("apps-root").to_str().unwrap(),
            dir.join("repos").to_str().unwrap(),
        )));
        let resp = h
            .handle(post_req(
                "/api/v1/apps/install",
                serde_json::json!({"repo": "nexos-app-dual"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "双收形态应装成功: {resp:?}");
        assert_eq!(resp.body["app"]["id"], "dual-app");
        let app_dir = resp.body["app"]["dir"].as_str().unwrap();
        // 产物就位；src/（发布根外）不进安装目录
        assert!(Path::new(app_dir).join("web/entry.js").is_file(), "{app_dir}");
        assert!(Path::new(app_dir).join("manifest.json").is_file());
        assert!(!Path::new(app_dir).join("src").exists(), "src 不应拷贝");
        assert!(!Path::new(app_dir).join("dist").exists(), "dist 前缀不应保留");
        // 静态托管两写法均命中
        for url in ["/apps-assets/dual-app/entry.js", "/apps-assets/dual-app/web/entry.js"] {
            let resp = h.handle(get_req(url)).await.unwrap();
            assert_eq!(resp.status, 200, "{url}: {resp:?}");
        }
    }

    #[tokio::test]
    async fn install_rejects_bad_manifests() {
        let dir = temp_dir_for("badmanifest");
        // 版本格式坏
        let _ = make_app_repo(&dir, "nexos-app-badver", serde_json::json!({"version": "1.2"}));
        // entry 不存在
        let _ = make_app_repo(&dir, "nexos-app-badentry", serde_json::json!({"entry": "web/nope.js"}));
        // id 非法（大写）
        let _ = make_app_repo(&dir, "nexos-app-badid", serde_json::json!({"id": "Bad-ID"}));
        // min_os_api 超前
        let _ = make_app_repo(
            &dir,
            "nexos-app-future",
            serde_json::json!({"min_os_api": "999.0.0"}),
        );
        let h = AppsRouteHandler::new(Arc::new(AppRegistry::with_paths(
            dir.join("apps.db").to_str().unwrap(),
            dir.join("apps-root").to_str().unwrap(),
            dir.join("repos").to_str().unwrap(),
        )));
        for repo in ["nexos-app-badver", "nexos-app-badentry", "nexos-app-badid", "nexos-app-future"] {
            let resp = h
                .handle(post_req(
                    "/api/v1/apps/install",
                    serde_json::json!({"repo": repo}),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 400, "{repo} 应被 manifest 校验拒绝: {resp:?}");
            // 失败也留任务记录（failed，可观测）
            let resp = h.handle(get_req("/api/v1/apps/tasks")).await.unwrap();
            let tasks = resp.body["tasks"].as_array().unwrap();
            assert!(
                tasks.iter().any(|t| t["repo"] == repo && t["status"] == "failed"),
                "{repo} 应有 failed 任务记录"
            );
        }
        // 无一入表
        let resp = h.handle(get_req("/api/v1/apps")).await.unwrap();
        assert_eq!(resp.body["apps"].as_array().unwrap().len(), 0);
    }

    // ---- catalog 扫描（真实裸仓库：好仓库 / 空仓库 / 非应用仓库不出现）----

    #[tokio::test]
    async fn catalog_scans_nexos_app_repos() {
        let dir = temp_dir_for("catalog");
        let _ = make_app_repo(&dir, "nexos-app-good", serde_json::json!({"id": "good-app"}));
        // 空裸仓库（无 commit）——manifest 不可读，但如实出现
        let repos = dir.join("repos");
        assert!(run_git(&[
            "git", "init", "--bare",
            repos.join("nexos-app-empty.git").to_str().unwrap()
        ]));
        // 非应用命名仓库——不出现
        assert!(run_git(&[
            "git", "init", "--bare",
            repos.join("nexos.git").to_str().unwrap()
        ]));
        let h = AppsRouteHandler::new(Arc::new(AppRegistry::with_paths(
            dir.join("apps.db").to_str().unwrap(),
            dir.join("apps-root").to_str().unwrap(),
            repos.to_str().unwrap(),
        )));
        let resp = h.handle(get_req("/api/v1/apps/catalog")).await.unwrap();
        assert_eq!(resp.status, 200);
        let apps = resp.body["apps"].as_array().unwrap();
        assert_eq!(apps.len(), 2, "只含 nexos-app-* 两个: {apps:?}");
        let good = apps
            .iter()
            .find(|a| a["repo"] == "nexos-app-good")
            .expect("好仓库在列");
        assert_eq!(good["id"], "good-app");
        assert_eq!(good["installed"], false);
        assert!(good["name"].is_string());
        let empty = apps
            .iter()
            .find(|a| a["repo"] == "nexos-app-empty")
            .expect("空仓库在列");
        assert!(empty["error"].is_string(), "空仓库如实报错: {empty:?}");
        // 安装后 installed/installed_version 翻转
        let resp = h
            .handle(post_req(
                "/api/v1/apps/install",
                serde_json::json!({"repo": "nexos-app-good"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        let resp = h.handle(get_req("/api/v1/apps/catalog")).await.unwrap();
        let good = resp.body["apps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["repo"] == "nexos-app-good")
            .cloned()
            .unwrap();
        assert_eq!(good["installed"], true);
        assert_eq!(good["installed_version"], "0.1.0");
    }

    // ---- 静态托管（穿越攻击 / mime / 命中）----

    #[tokio::test]
    async fn assets_serve_and_block_traversal() {
        let dir = temp_dir_for("assets");
        let _ = make_app_repo(&dir, "nexos-app-web", serde_json::json!({"id": "webapp"}));
        let h = AppsRouteHandler::new(Arc::new(AppRegistry::with_paths(
            dir.join("apps.db").to_str().unwrap(),
            dir.join("apps-root").to_str().unwrap(),
            dir.join("repos").to_str().unwrap(),
        )));
        let resp = h
            .handle(post_req(
                "/api/v1/apps/install",
                serde_json::json!({"repo": "nexos-app-web"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "{resp:?}");
        // 命中：js 文本直传 + 正确 mime。两种 URL 写法等价：path 相对 web/，
        // 或 manifest.entry 原样（带 web/ 前缀，兼容剥段）。
        for url in ["/apps-assets/webapp/entry.js", "/apps-assets/webapp/web/entry.js"] {
            let resp = h.handle(get_req(url)).await.unwrap();
            assert_eq!(resp.status, 200, "{url}: {resp:?}");
            assert_eq!(resp.headers["content-type"], "text/javascript");
            assert!(
                resp.body.as_str().unwrap().contains("export default"),
                "{url} JS 原文直传: {resp:?}"
            );
        }
        // 未知应用 → 404
        let resp = h
            .handle(get_req("/apps-assets/ghost/entry.js"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
        // 穿越攻击三连：.. 段 / apps 根外 / web 外（manifest 在 web 之外）
        for path in [
            "/apps-assets/webapp/../manifest.json",
            "/apps-assets/webapp/../../etc/passwd",
            "/apps-assets/webapp/manifest.json",
            "/apps-assets/webapp/web/../manifest.json",
        ] {
            let resp = h.handle(get_req(path)).await.unwrap();
            assert_eq!(resp.status, 404, "穿越应 404: {path} → {resp:?}");
        }
        // 未安装应用 id 合法但目录缺失 → 404
        let resp = h
            .handle(get_req("/apps-assets/not-installed/entry.js"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // ---- 引擎门控查询 ----

    #[tokio::test]
    async fn is_engine_enabled_matches_id_or_engine() {
        let dir = temp_dir_for("gate");
        let _ = make_app_repo(
            &dir,
            "nexos-app-film",
            serde_json::json!({"id": "film", "engine": "film"}),
        );
        let reg = Arc::new(AppRegistry::with_paths(
            dir.join("apps.db").to_str().unwrap(),
            dir.join("apps-root").to_str().unwrap(),
            dir.join("repos").to_str().unwrap(),
        ));
        assert!(!reg.is_engine_enabled("film"), "未装 → 关");
        let h = AppsRouteHandler::new(Arc::clone(&reg));
        let resp = h
            .handle(post_req(
                "/api/v1/apps/install",
                serde_json::json!({"repo": "nexos-app-film"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        assert!(reg.is_engine_enabled("film"), "装了（id 或 engine 命中）→ 开");
        reg.uninstall("film").unwrap();
        assert!(!reg.is_engine_enabled("film"), "卸载 → 关");
    }

    #[tokio::test]
    async fn unmatched_route_returns_404() {
        let (h, _) = handler_at("404");
        let resp = h.handle(get_req("/api/v1/apps/nope")).await.unwrap();
        assert_eq!(resp.status, 404);
    }

    /// 端到端：组件注册 → build_router（axum 通配路由 + 直传层）→ oneshot。
    /// 证明 /apps-assets/:id/* 经完整 HTTP 栈后字节与 content-type 正确
    /// （handler 直测只到 ApiResponse，未过 api_to_response 的文本直传）。
    #[tokio::test]
    async fn assets_served_through_full_axum_router() {
        use tower::ServiceExt;

        let dir = temp_dir_for("e2e-assets");
        let _ = make_app_repo(&dir, "nexos-app-e2e", serde_json::json!({"id": "e2eapp"}));
        let reg = Arc::new(AppRegistry::with_paths(
            dir.join("apps.db").to_str().unwrap(),
            dir.join("apps-root").to_str().unwrap(),
            dir.join("repos").to_str().unwrap(),
        ));
        let (action, _) = reg.install("nexos-app-e2e").await.expect("安装");
        assert_eq!(action, "install");
        let gw = crate::InProcessGateway::new();
        crate::gateway::Gateway::register_component(
            &gw,
            "apps",
            Box::new(AppsRouteHandler::new(reg)),
        )
        .await
        .expect("注册 apps");
        let router = crate::http::build_router(gw.make_state(None, None), None);
        // js：文本直传（无 JSON 引号）+ 正确 MIME
        let resp = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/apps-assets/e2eapp/entry.js")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            &"text/javascript".parse::<axum::http::HeaderValue>().unwrap()
        );
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.contains("export default"), "原文直传: {text}");
        assert!(!text.starts_with('"'), "不得 JSON 引号包裹");
        // 穿越在 axum 层同样被拦
        let resp = router
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/apps-assets/e2eapp/../manifest.json")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }
}
