//! `DownloadsRouteHandler` —— 下载中心桌面应用的 HTTP→真实 aria2 适配器。
//!
//! 定位：把网关 HTTP 请求（`/api/v1/downloads/*`）翻译为 aria2 JSON-RPC 调用，返回 JSON。
//! 这是 OS"系统类三件套"之一（下载中心）桌面应用的后端 REST 入口。
//!
//! # 实现策略：真实 aria2（JSON-RPC over HTTP，:6800）
//!
//! 首次 POST /tasks 时若 aria2 RPC 不在线，则 spawn
//! `aria2c --enable-rpc --rpc-listen-all --rpc-listen-port=6800 -d /tank/downloads`
//! 守护进程（后台跑、由 OS 收养）。后续 create/pause/resume/cancel/list 均经
//! `http://localhost:6800/jsonrpc` 的 `aria2.addUri` / `tellActive` / `tellWaiting` /
//! `tellStopped` / `pause` / `unpause` / `remove` / `removeDownloadResult` 完成。
//! aria2 未安装 / spawn 失败 / RPC 不通 → **降级**为空列表或 failed，绝不 panic。
//!
//! # 多协议任务（2026-08-23：磁力链 / ED2K / 种子）
//!
//! aria2c 原生支持 BitTorrent（magnet/torrent），ED2K 直接透传——按 URL 分类
//! （[`classify_download_url`]）走对应 aria2 方法：
//!
//! | 类型     | 输入                                  | aria2 调用          | 附加参数 |
//! |----------|---------------------------------------|---------------------|----------|
//! | `http`   | `http(s)://` / `ftp://` / `sftp://` 直链 | `aria2.addUri`   | — |
//! | `magnet` | `magnet:?xt=urn:btih:…`（缺 btih 拒绝）  | `aria2.addUri`   | `seed-ratio=0.0` `seed-time=0`（下完不分享） |
//! | `ed2k`   | `ed2k://…`                            | `aria2.addUri`（透传；aria2 不支持时其错误原样回显） | — |
//! | `torrent`| 服务器本地 `.torrent` 路径（POST /tasks 的 url）或 base64 上传（POST /torrent） | `aria2.addTorrent`（文件内容 base64） | 同 magnet |
//!
//! `.torrent` 上传走 base64-JSON 信封（同 files.rs `content_base64` 惯例——
//! 网关契约无 multipart），落盘 `/tmp/torrents/` 后把**内容** base64 交给
//! `aria2.addTorrent`（RPC 契约收内容而非路径；落盘副本供运维排查）。
//! 任务响应/列表统一带 `type` 字段（`http`/`magnet`/`ed2k`/`torrent`），前端
//! 据此渲染类型徽章。
//!
//! # 路由表
//!
//! | method | path                                | 动作 |
//! |--------|-------------------------------------|------|
//! | GET    | `/api/v1/downloads/tasks`           | 列全部任务（tellActive+Waiting+Stopped）|
//! | POST   | `/api/v1/downloads/tasks`           | 创建任务（直链/magnet/ed2k= addUri；本地 .torrent= addTorrent，需 admin）|
//! | POST   | `/api/v1/downloads/torrent`         | 上传种子创建任务（base64-JSON，落盘 /tmp/torrents，需 admin）|
//! | POST   | `/api/v1/downloads/tasks/:id/pause` | 暂停（aria2.pause，需 admin）|
//! | POST   | `/api/v1/downloads/tasks/:id/resume`| 继续（aria2.unpause，需 admin）|
//! | POST   | `/api/v1/downloads/tasks/:id/cancel`| 取消（aria2.remove，需 admin）|
//! | DELETE | `/api/v1/downloads/tasks/:id`       | 删除结果（aria2.removeDownloadResult，需 admin）|
//! | GET    | `/api/v1/downloads/stats`           | 统计 |

use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

/// 进程级共享 `reqwest::Client`（rustify：aria2 RPC 的 curl 子进程 → reqwest）。
static HTTP: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("构建共享 reqwest Client 失败")
});

/// aria2 RPC 端点。
const ARIA2_RPC_URL: &str = "http://localhost:6800/jsonrpc";
/// aria2 默认下载根目录。
const ARIA2_DOWNLOAD_DIR: &str = "/tank/downloads";
/// 上传种子的落盘目录（POST /downloads/torrent；副本供运维排查，aria2 收
/// 内容 base64 不读该目录）。
const TORRENT_UPLOAD_DIR: &str = "/tmp/torrents";
/// 种子内容大小上限（解码后字节；正常 .torrent 数 KiB～数百 KiB，10 MiB
/// 足够宽裕且拦得住误传大文件/滥用）。
const TORRENT_MAX_BYTES: usize = 10 * 1024 * 1024;

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 一条下载任务（响应给前端；字段与旧内存态保持兼容）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadTask {
    pub id: String,
    pub name: String,
    pub url: String,
    pub save_path: String,
    pub status: String,
    /// 进度 0-100
    pub progress: u32,
    pub size_bytes: u64,
    pub downloaded_bytes: u64,
    pub speed_bytes_sec: u64,
    pub created_at: String,
    /// 任务类型：`http`（直链）/ `magnet`（磁力链）/ `torrent`（种子）/
    /// `ed2k`。创建时由 [`classify_download_url`] 判定；列表时从 aria2 状态
    /// 推断（BT 任务 files[0].uris 带 magnet: → magnet；有 bittorrent 元数据
    /// 无 magnet → torrent）。serde default 兼容旧客户端。
    #[serde(rename = "type", default = "default_download_type")]
    pub download_type: String,
}

/// `download_type` 缺省值（serde default，旧载荷兼容）。
fn default_download_type() -> String {
    "http".to_string()
}

/// `GET /api/v1/downloads/stats` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadStats {
    pub total: usize,
    pub downloading: usize,
    pub completed: usize,
    pub total_size_bytes: u64,
}

/// 创建任务请求体。
#[derive(Debug, Deserialize)]
struct CreateBody {
    url: String,
    save_path: String,
    #[serde(default)]
    name: Option<String>,
}

/// 种子上传请求体（base64-JSON 信封，同 files.rs 惯例——网关契约无 multipart）。
/// 必填字段缺失走 400（Option + handler 显式校验，而非 serde 报 500）。
#[derive(Debug, Deserialize)]
struct TorrentUploadBody {
    /// 种子文件名（可选；用于落盘命名。取 basename，不得携带路径）。
    #[serde(default)]
    filename: Option<String>,
    /// `.torrent` 文件内容 base64（标准字母表，无 data: 前缀）。
    #[serde(default)]
    content_base64: Option<String>,
    /// 保存路径（同 POST /tasks）。
    #[serde(default)]
    save_path: Option<String>,
    /// 可选任务名（缺省用种子文件名）。
    #[serde(default)]
    name: Option<String>,
}

// ----------------------------------------------------------------------------
// aria2 纯函数（易测试）
// ----------------------------------------------------------------------------

/// 校验并分类下载 URL（多协议支持，2026-08-23）：
///
/// - `http://` / `https://` / `ftp://` / `sftp://` → `Ok("http")`（直链）；
/// - `magnet:?` 且查询串含 `xt=urn:btih:`（大小写宽容）→ `Ok("magnet")`——
///   缺 btih 的 magnet 是坏链（aria2 会拒），提前 400 给出明确文案；
/// - `ed2k://` → `Ok("ed2k")`（透传给 aria2；其不支持时的错误原样回显）；
/// - 以 `.torrent` 结尾的**本地文件路径**（无 scheme）→ `Ok("torrent")`
///   （POST /tasks 直接引用服务器上已有的种子文件）；
/// - 其余 → `Err(原因)`（调用方转 400）。
pub fn classify_download_url(url: &str) -> Result<&'static str, String> {
    let u = url.trim();
    let lower = u.to_ascii_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("ftp://")
        || lower.starts_with("sftp://")
    {
        return Ok("http");
    }
    if lower.starts_with("magnet:?") {
        if lower.contains("xt=urn:btih:") {
            return Ok("magnet");
        }
        return Err("magnet 链接非法：须含 xt=urn:btih:（BitTorrent info-hash）".into());
    }
    if lower.starts_with("ed2k://") {
        return Ok("ed2k");
    }
    // 无 scheme 的本地 .torrent 路径（POST /tasks 透传服务器本地种子）
    if lower.ends_with(".torrent") && !u.contains("://") {
        return Ok("torrent");
    }
    Err(format!(
        "不支持的 URL：{u}（支持 HTTP/FTP/SFTP 直链、磁力链 magnet:?xt=urn:btih:、\
         ed2k:// 或 .torrent 本地文件路径）"
    ))
}

/// BT 任务"下完即止"参数（aria2 RPC options：`--seed-ratio=0.0 --seed-time=0`
/// 的 JSON 形式——下载完成立刻停止做种，不分享）。
fn bt_no_seed_options(save_path: &str) -> serde_json::Value {
    serde_json::json!({
        "dir": save_path,
        "seed-ratio": "0.0",
        "seed-time": "0",
    })
}

/// 构造 `aria2.addUri` 的 JSON-RPC 请求体字符串。
///
/// 形如：`{"jsonrpc":"2.0","method":"aria2.addUri","params":[["<url>"],{"dir":"<save_path>"}],"id":1}`
/// （url 与 save_path 经 serde_json 正确转义，避免手工拼接注入）。
/// magnet 链接自动附带 `seed-ratio=0.0`/`seed-time=0`（下完不分享，见模块文档）。
#[must_use]
pub fn build_aria2_add_cmd(url: &str, save_path: &str) -> String {
    let options = if url.trim().to_ascii_lowercase().starts_with("magnet:?") {
        bt_no_seed_options(save_path)
    } else {
        serde_json::json!({ "dir": save_path })
    };
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "aria2.addUri",
        "params": [[url], options],
        "id": 1
    });
    // 构造不会失败（输入均为合法 JSON Value）
    serde_json::to_string(&body).unwrap_or_else(|_| String::from("{}"))
}

/// 构造 `aria2.addTorrent` 的 JSON-RPC 请求体字符串。
///
/// `params = [torrent_base64, [], options]`（RPC 契约：第一个参数是种子文件
/// **内容** base64 而非路径；第二个是 Web-Seed URI 列表，留空）；options 附
/// `seed-ratio=0.0`/`seed-time=0`（下完不分享）。
#[must_use]
pub fn build_aria2_add_torrent_cmd(torrent_b64: &str, save_path: &str) -> String {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "aria2.addTorrent",
        "params": [torrent_b64, [], bt_no_seed_options(save_path)],
        "id": 1
    });
    serde_json::to_string(&body).unwrap_or_else(|_| String::from("{}"))
}

/// 从 aria2 任务状态推断类型标签（列表路径）：magnet/ed2k 看 files[0].uris[0]；
/// 有 `bittorrent` 元数据但无 magnet URI → torrent（addTorrent 创建的任务
/// uris 常为空）；否则 http。
fn classify_task_type(url: &str, has_bt_metadata: bool) -> &'static str {
    let lower = url.trim().to_ascii_lowercase();
    if lower.starts_with("magnet:?") {
        "magnet"
    } else if lower.starts_with("ed2k://") {
        "ed2k"
    } else if has_bt_metadata {
        "torrent"
    } else {
        "http"
    }
}

/// 把 aria2 原生状态归一化为 `active` / `waiting` / `complete` / `error` 四态。
#[must_use]
pub fn parse_aria2_status(status: &str) -> &'static str {
    match status {
        "active" => "active",
        "waiting" | "paused" => "waiting",
        "complete" | "completing" => "complete",
        "error" | "removed" => "error",
        _ => "error",
    }
}

/// 把 aria2 原生状态映射为前端期望的旧状态词汇（downloading/paused/pending/completed/error）。
fn aria2_to_legacy(status: &str) -> &'static str {
    match status {
        "active" => "downloading",
        "paused" => "paused",
        "waiting" => "pending",
        "complete" | "completing" => "completed",
        "error" | "removed" => "error",
        _ => "pending",
    }
}

/// 取路径的 basename（aria2 files[].path → 文件名）。
fn basename(path: &str) -> String {
    path.rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
        .to_string()
}

// ----------------------------------------------------------------------------
// DownloadsRouteHandler
// ----------------------------------------------------------------------------

/// 下载中心路由处理器——HTTP 边界适配到真实 aria2 RPC。
pub struct DownloadsRouteHandler {
    /// 一旦确认 aria2 不可用（未安装），负缓存避免每次请求都重试 spawn。
    unavailable: Mutex<bool>,
}

impl DownloadsRouteHandler {
    /// 构造 handler。
    #[must_use]
    pub fn new() -> Self {
        Self {
            unavailable: Mutex::new(false),
        }
    }

    /// 空构造（保留供测试/旧调用路径）。
    #[must_use]
    pub fn with_empty() -> Self {
        Self::new()
    }

    /// 标记/查询"已确认 aria2 不可用"。
    fn mark_unavailable(&self) {
        *self.unavailable.lock().expect("unavailable poisoned") = true;
    }
    fn is_known_unavailable(&self) -> bool {
        *self.unavailable.lock().expect("unavailable poisoned")
    }

    /// 探测 aria2 RPC 是否在线（aria2.getVersion；rustify：curl → reqwest POST）。
    async fn aria2_alive() -> bool {
        let body = r#"{"jsonrpc":"2.0","method":"aria2.getVersion","params":[],"id":1}"#;
        matches!(
            HTTP.post(ARIA2_RPC_URL)
                .timeout(Duration::from_secs(2))
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await,
            Ok(r) if r.status().is_success()
        )
    }

    /// 确保 aria2 RPC 在线：先探测，不在线则 spawn 守护进程；返回是否可用。
    async fn ensure_aria2(&self) -> bool {
        if self.is_known_unavailable() {
            return false;
        }
        if Self::aria2_alive().await {
            return true;
        }
        // spawn aria2c 守护进程（后台跑）
        let _ = std::fs::create_dir_all(ARIA2_DOWNLOAD_DIR);
        let spawned = Command::new("aria2c")
            .args([
                "--enable-rpc",
                "--rpc-listen-all",
                "--rpc-listen-port=6800",
                "-d",
                ARIA2_DOWNLOAD_DIR,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if spawned.is_err() {
            // aria2 未安装：负缓存，后续直接降级
            self.mark_unavailable();
            return false;
        }
        // 轮询等待 RPC 就绪（最多 ~2s）
        for _ in 0..20 {
            if Self::aria2_alive().await {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    /// 发起一次 aria2 JSON-RPC 调用，返回 `result` 字段。失败/不可用 → None。
    /// （rustify：curl 子进程 → reqwest POST JSON）
    async fn rpc(&self, method: &str, params: &serde_json::Value) -> Option<serde_json::Value> {
        if !self.ensure_aria2().await {
            return None;
        }
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": 1
        });
        let resp = HTTP
            .post(ARIA2_RPC_URL)
            .timeout(Duration::from_secs(8))
            .json(&body)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: serde_json::Value = resp.json().await.ok()?;
        if v.get("error").is_some() {
            return None;
        }
        v.get("result").cloned()
    }

    /// 拉取全部任务（active + waiting + stopped）并合并为 DownloadTask 列表。
    async fn list_tasks(&self) -> Vec<DownloadTask> {
        let mut all: Vec<serde_json::Value> = Vec::new();
        // tellActive() → params=[]；tellWaiting/tellStopped(offset,num) → params=[0,1000]
        if let Some(serde_json::Value::Array(a)) =
            self.rpc("aria2.tellActive", &serde_json::json!([])).await
        {
            all.extend(a);
        }
        if let Some(serde_json::Value::Array(a)) = self
            .rpc("aria2.tellWaiting", &serde_json::json!([0, 1000]))
            .await
        {
            all.extend(a);
        }
        if let Some(serde_json::Value::Array(a)) = self
            .rpc("aria2.tellStopped", &serde_json::json!([0, 1000]))
            .await
        {
            all.extend(a);
        }
        all.into_iter().map(task_from_aria2).collect()
    }
}

impl Default for DownloadsRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

/// 把 aria2 单条状态对象映射为 DownloadTask。
fn task_from_aria2(v: serde_json::Value) -> DownloadTask {
    let s_str = |key: &str| {
        v.get(key)
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string()
    };
    let s_u64 = |key: &str| s_str(key).parse::<u64>().unwrap_or(0);
    let gid = s_str("gid");
    let status_raw = s_str("status");
    let total = s_u64("totalLength");
    let completed = s_u64("completedLength");
    let progress = if total > 0 {
        ((completed as f64 / total as f64) * 100.0).round() as u32
    } else {
        0
    };
    // name / url / save_path 从 files[0] 取
    let file0 = v.get("files").and_then(|f| f.get(0));
    let path = file0
        .and_then(|f| f.get("path"))
        .and_then(|p| p.as_str())
        .unwrap_or("")
        .to_string();
    let url = file0
        .and_then(|f| f.get("uris"))
        .and_then(|u| u.get(0))
        .and_then(|u| u.get("uri"))
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .to_string();
    let name = if path.is_empty() {
        gid.clone()
    } else {
        basename(&path)
    };
    let save_path = path
        .rsplit_once('/')
        .map(|(dir, _)| dir.to_string())
        .unwrap_or_default();
    let has_bt_metadata = v.get("bittorrent").is_some();
    let download_type = classify_task_type(&url, has_bt_metadata).to_string();
    DownloadTask {
        id: gid,
        name,
        url,
        save_path,
        status: aria2_to_legacy(&status_raw).to_string(),
        progress: progress.min(100),
        size_bytes: total,
        downloaded_bytes: completed,
        speed_bytes_sec: s_u64("downloadSpeed"),
        created_at: String::new(),
        download_type,
    }
}

#[async_trait]
impl RouteHandler for DownloadsRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec(HttpMethod::Get, "/api/v1/downloads/tasks", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/downloads/tasks",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/downloads/torrent",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/downloads/tasks/:id/pause",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/downloads/tasks/:id/resume",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/downloads/tasks/:id/cancel",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/downloads/tasks/:id",
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Get, "/api/v1/downloads/stats", false, vec![]),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            // —— GET /api/v1/downloads/tasks —— 列全部
            (HttpMethod::Get, ["api", "v1", "downloads", "tasks"]) => {
                let tasks = self.list_tasks().await;
                Ok(ok_json(to_value(&tasks)?))
            }

            // —— GET /api/v1/downloads/stats —— 统计
            (HttpMethod::Get, ["api", "v1", "downloads", "stats"]) => {
                let tasks = self.list_tasks().await;
                let downloading = tasks.iter().filter(|t| t.status == "downloading").count();
                let completed = tasks.iter().filter(|t| t.status == "completed").count();
                let total_size = tasks.iter().map(|t| t.size_bytes).sum();
                Ok(ok_json(to_value(&DownloadStats {
                    total: tasks.len(),
                    downloading,
                    completed,
                    total_size_bytes: total_size,
                })?))
            }

            // —— POST /api/v1/downloads/tasks —— 创建（按 URL 分类：
            //    直链/magnet/ed2k → aria2.addUri；本地 .torrent → aria2.addTorrent）
            (HttpMethod::Post, ["api", "v1", "downloads", "tasks"]) => {
                let body: CreateBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析创建下载任务请求体失败: {e}"))
                })?;
                if body.url.trim().is_empty() {
                    return Ok(error_response(400, "url 不可为空"));
                }
                if body.save_path.trim().is_empty() {
                    return Ok(error_response(400, "save_path 不可为空"));
                }
                let kind = match classify_download_url(&body.url) {
                    Ok(k) => k,
                    Err(msg) => return Ok(error_response(400, &msg)),
                };
                let name = body
                    .name
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| basename(body.url.trim()));
                let save_path = body.save_path.trim().to_string();
                // 本地 .torrent 路径：读内容 → base64 → aria2.addTorrent（RPC 收
                // 内容而非路径）；其余（直链/magnet/ed2k）直接 addUri 透传。
                let rpc_body = if kind == "torrent" {
                    let path = body.url.trim();
                    match tokio::fs::read(path).await {
                        Ok(bytes) => {
                            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                            build_aria2_add_torrent_cmd(&b64, &save_path)
                        }
                        Err(e) => {
                            return Ok(error_response(
                                400,
                                &format!("种子文件不可读（{path}）: {e}"),
                            ))
                        }
                    }
                } else {
                    build_aria2_add_cmd(body.url.trim(), &save_path)
                };
                let method = if kind == "torrent" {
                    "aria2.addTorrent"
                } else {
                    "aria2.addUri"
                };
                let task = self
                    .submit_rpc_create(&rpc_body, method, &name, &body.url, &save_path, kind)
                    .await?;
                Ok(task)
            }

            // —— POST /api/v1/downloads/torrent —— 上传种子创建任务
            //    body: {filename?, content_base64, save_path, name?}（base64-JSON
            //    信封，同 files.rs 惯例）→ 落盘 /tmp/torrents/ → aria2.addTorrent。
            (HttpMethod::Post, ["api", "v1", "downloads", "torrent"]) => {
                let body: TorrentUploadBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析种子上传请求体失败: {e}"))
                })?;
                let Some(save_path) = body
                    .save_path
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                else {
                    return Ok(error_response(400, "save_path 不可为空"));
                };
                let Some(content_b64) = body
                    .content_base64
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                else {
                    return Ok(error_response(400, "content_base64 不可为空"));
                };
                // 文件名：取 basename（拒路径穿越）+ 保证 .torrent 后缀；缺省
                // 用时间戳生成。
                let mut filename = body
                    .filename
                    .as_deref()
                    .map(|f| basename(f.trim()))
                    .unwrap_or_default();
                if filename.is_empty() {
                    filename = format!(
                        "upload-{}.torrent",
                        chrono::Local::now().format("%Y%m%d%H%M%S%3f")
                    );
                }
                if !filename.to_ascii_lowercase().ends_with(".torrent") {
                    filename.push_str(".torrent");
                }
                // base64 解码（标准字母表）+ 大小上限
                let bytes = match base64::engine::general_purpose::STANDARD.decode(content_b64) {
                    Ok(b) => b,
                    Err(_) => {
                        return Ok(error_response(
                            400,
                            "content_base64 非法（标准 base64，无 data: 前缀）",
                        ))
                    }
                };
                if bytes.is_empty() {
                    return Ok(error_response(400, "种子内容为空"));
                }
                if bytes.len() > TORRENT_MAX_BYTES {
                    return Ok(error_response(
                        400,
                        &format!(
                            "种子过大（{} 字节 > 上限 {}）",
                            bytes.len(),
                            TORRENT_MAX_BYTES
                        ),
                    ));
                }
                // 落盘副本（/tmp/torrents/<filename>；aria2 收内容 base64，不读
                // 该文件——副本仅供运维排查）。落盘失败不阻塞任务创建。
                let disk_path = format!("{TORRENT_UPLOAD_DIR}/{filename}");
                if std::fs::create_dir_all(TORRENT_UPLOAD_DIR).is_ok() {
                    let _ = std::fs::write(&disk_path, &bytes);
                }
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                let rpc_body = build_aria2_add_torrent_cmd(&b64, save_path);
                let name = body
                    .name
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| filename.clone());
                let task = self
                    .submit_rpc_create(
                        &rpc_body,
                        "aria2.addTorrent",
                        &name,
                        &disk_path,
                        save_path,
                        "torrent",
                    )
                    .await?;
                Ok(task)
            }

            // —— POST /api/v1/downloads/tasks/:id/pause|resume|cancel ——
            (HttpMethod::Post, ["api", "v1", "downloads", "tasks", id, action])
                if matches!(*action, "pause" | "resume" | "cancel") =>
            {
                let (method, next_status) = match *action {
                    "pause" => ("aria2.pause", "paused"),
                    "resume" => ("aria2.unpause", "downloading"),
                    "cancel" => ("aria2.remove", "error"),
                    _ => unreachable!(),
                };
                match self.rpc(method, &serde_json::json!([id])).await {
                    Some(_) => Ok(ok_json(serde_json::json!({
                        "ok": true,
                        "id": id,
                        "action": *action,
                        "status": next_status,
                        "aria2": true,
                    }))),
                    None => Ok(error_response(502, &format!("aria2 {action} 失败或不可用"))),
                }
            }

            // —— DELETE /api/v1/downloads/tasks/:id —— 删除结果
            (HttpMethod::Delete, ["api", "v1", "downloads", "tasks", id]) => {
                match self
                    .rpc("aria2.removeDownloadResult", &serde_json::json!([id]))
                    .await
                {
                    Some(_) => Ok(ok_json(
                        serde_json::json!({"ok": true, "id": id, "action": "delete"}),
                    )),
                    None => Ok(error_response(
                        502,
                        "aria2.removeDownloadResult 失败或不可用",
                    )),
                }
            }

            // —— 未覆盖路由 —— 兜底 404
            _ => Ok(error_response(404, "downloads: 未匹配的路由")),
        }
    }
}

impl DownloadsRouteHandler {
    /// 用预构造的 JSON-RPC body 字符串直接 POST（仅 create 用，保证 build_aria2_add_cmd
    /// 为请求体的唯一真理源）。返回完整响应 JSON（含 result/error）。
    /// （rustify：curl 子进程 → reqwest POST JSON）
    async fn rpc_raw_body(&self, body: &str) -> Option<serde_json::Value> {
        if !self.ensure_aria2().await {
            return None;
        }
        let resp = HTTP
            .post(ARIA2_RPC_URL)
            .timeout(Duration::from_secs(15))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string())
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json().await.ok()
    }

    /// 创建路径共用收尾：提交预构造 RPC body → 解析 gid → 构造 201 任务响应
    /// （带 `type` 类型标注）。aria2 不可用 → 502；aria2 报错 → 502 附其 message。
    async fn submit_rpc_create(
        &self,
        rpc_body: &str,
        method: &str,
        name: &str,
        url: &str,
        save_path: &str,
        download_type: &str,
    ) -> Result<ApiResponse, ApiGatewayError> {
        match self.rpc_raw_body(rpc_body).await {
            Some(resp) if resp.get("error").is_none() => {
                let gid = resp
                    .get("result")
                    .and_then(|r| r.as_str())
                    .unwrap_or("")
                    .to_string();
                if gid.is_empty() {
                    return Ok(error_response(502, &format!("{method} 未返回 gid")));
                }
                let task = DownloadTask {
                    id: gid,
                    name: name.to_string(),
                    url: url.to_string(),
                    save_path: save_path.to_string(),
                    status: "pending".into(),
                    progress: 0,
                    size_bytes: 0,
                    downloaded_bytes: 0,
                    speed_bytes_sec: 0,
                    created_at: now_iso(),
                    download_type: download_type.to_string(),
                };
                Ok(ApiResponse {
                    status: 201,
                    body: to_value(&task)?,
                    headers: serde_json::json!({}),
                })
            }
            Some(resp) => {
                let detail = resp
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown");
                Ok(error_response(502, &format!("{method} 失败: {detail}")))
            }
            None => Ok(error_response(502, "aria2 不可用（未安装或启动失败）")),
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
        handler_component: "downloads".to_string(),
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

    // ---- 纯函数：build_aria2_add_cmd ----

    #[test]
    fn build_aria2_add_cmd_produces_valid_adduri_rpc() {
        let body = build_aria2_add_cmd("https://x/y.iso", "/tank/iso");
        let v: serde_json::Value = serde_json::from_str(&body).expect("必须是合法 JSON");
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "aria2.addUri");
        // params[0] = ["<url>"]
        assert_eq!(v["params"][0][0], "https://x/y.iso");
        // params[1] = {"dir": "<save_path>"}
        assert_eq!(v["params"][1]["dir"], "/tank/iso");
        assert_eq!(v["id"], 1);
    }

    #[test]
    fn build_aria2_add_cmd_escapes_quotes_in_url() {
        let body = build_aria2_add_cmd("https://x/\"injected\";rm", "/d");
        let v: serde_json::Value = serde_json::from_str(&body).expect("含特殊字符也须合法 JSON");
        assert_eq!(v["params"][0][0], "https://x/\"injected\";rm");
    }

    // ---- 纯函数：parse_aria2_status ----

    #[test]
    fn parse_aria2_status_maps_all_known_states() {
        assert_eq!(parse_aria2_status("active"), "active");
        assert_eq!(parse_aria2_status("waiting"), "waiting");
        assert_eq!(parse_aria2_status("paused"), "waiting");
        assert_eq!(parse_aria2_status("complete"), "complete");
        assert_eq!(parse_aria2_status("completing"), "complete");
        assert_eq!(parse_aria2_status("error"), "error");
        assert_eq!(parse_aria2_status("removed"), "error");
    }

    #[test]
    fn parse_aria2_status_unknown_defaults_to_error() {
        assert_eq!(parse_aria2_status("???"), "error");
        assert_eq!(parse_aria2_status(""), "error");
    }

    #[test]
    fn aria2_to_legacy_matches_frontend_vocab() {
        assert_eq!(aria2_to_legacy("active"), "downloading");
        assert_eq!(aria2_to_legacy("paused"), "paused");
        assert_eq!(aria2_to_legacy("waiting"), "pending");
        assert_eq!(aria2_to_legacy("complete"), "completed");
        assert_eq!(aria2_to_legacy("error"), "error");
    }

    #[test]
    fn basename_extracts_filename() {
        assert_eq!(basename("/tank/iso/x.iso"), "x.iso");
        assert_eq!(basename("plain.bin"), "plain.bin");
        assert_eq!(basename("/"), "/");
    }

    #[test]
    fn task_from_aria2_maps_fields_and_progress() {
        let v = serde_json::json!({
            "gid": "abc",
            "status": "active",
            "totalLength": "1000",
            "completedLength": "250",
            "downloadSpeed": "128",
            "files": [{ "path": "/tank/dl/file.bin", "uris": [{"uri":"https://x/file.bin"}] }]
        });
        let t = task_from_aria2(v);
        assert_eq!(t.id, "abc");
        assert_eq!(t.status, "downloading");
        assert_eq!(t.size_bytes, 1000);
        assert_eq!(t.downloaded_bytes, 250);
        assert_eq!(t.progress, 25);
        assert_eq!(t.speed_bytes_sec, 128);
        assert_eq!(t.name, "file.bin");
        assert_eq!(t.save_path, "/tank/dl");
        assert_eq!(t.url, "https://x/file.bin");
    }

    #[test]
    fn task_from_aria2_labels_download_type() {
        // magnet：BT 任务 + files[0].uris 带 magnet URI
        let magnet = task_from_aria2(serde_json::json!({
            "gid": "m1", "status": "active",
            "files": [{ "path": "/tank/dl/x", "uris": [{"uri":"magnet:?xt=urn:btih:abc"}] }],
            "bittorrent": { "info": { "name": "x" } }
        }));
        assert_eq!(magnet.download_type, "magnet");
        // torrent：BT 任务 + 无 URI（addTorrent 创建的典型形态）
        let torrent = task_from_aria2(serde_json::json!({
            "gid": "t1", "status": "active",
            "files": [{ "path": "/tank/dl/x", "uris": [] }],
            "bittorrent": { "info": { "name": "x" } }
        }));
        assert_eq!(torrent.download_type, "torrent");
        // http：普通直链任务
        let http = task_from_aria2(serde_json::json!({
            "gid": "h1", "status": "active",
            "files": [{ "path": "/tank/dl/y.iso", "uris": [{"uri":"https://x/y.iso"}] }]
        }));
        assert_eq!(http.download_type, "http");
        // ed2k：uris 带 ed2k:// 透传链接
        let ed2k = task_from_aria2(serde_json::json!({
            "gid": "e1", "status": "active",
            "files": [{ "path": "/tank/dl/z", "uris": [{"uri":"ed2k://|file|z|1|h|/"}] }]
        }));
        assert_eq!(ed2k.download_type, "ed2k");
    }

    // ---- 路由声明 ----

    #[tokio::test]
    async fn routes_declares_eight_endpoints() {
        let h = DownloadsRouteHandler::new();
        let routes = h.routes().await;
        assert_eq!(routes.len(), 8);
        assert!(routes.iter().all(|r| r.handler_component == "downloads"));
        for r in &routes {
            if r.method == HttpMethod::Post || r.method == HttpMethod::Delete {
                assert!(r.requires_auth);
                assert_eq!(r.required_roles, vec!["admin".to_string()]);
            }
        }
    }

    // ---- handler：create 校验（不触达 aria2）----

    #[tokio::test]
    async fn create_validates_empty_url() {
        let h = DownloadsRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/downloads/tasks",
                serde_json::json!({"url": "", "save_path": "/tank"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    #[tokio::test]
    async fn create_validates_empty_save_path() {
        let h = DownloadsRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/downloads/tasks",
                serde_json::json!({"url": "https://x/y", "save_path": ""}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    // ---- 多协议：classify_download_url 分类矩阵 ----

    #[test]
    fn classify_accepts_magnet_ed2k_torrent_and_direct_links() {
        // 磁力链（大小写宽容）
        assert_eq!(
            classify_download_url("magnet:?xt=urn:btih:abcdef0123456789&dn=x"),
            Ok("magnet")
        );
        assert_eq!(
            classify_download_url("MAGNET:?XT=URN:BTIH:ABCDEF&dn=x"),
            Ok("magnet")
        );
        // ED2K 透传
        assert_eq!(
            classify_download_url("ed2k://|file|xx.iso|123|hash|/"),
            Ok("ed2k")
        );
        // 直链四协议
        for u in [
            "http://x/f.iso",
            "https://x/f.iso",
            "ftp://x/f.iso",
            "sftp://x/f.iso",
        ] {
            assert_eq!(classify_download_url(u), Ok("http"), "{u}");
        }
        // 服务器本地 .torrent 路径
        assert_eq!(
            classify_download_url("/tank/seeds/ubuntu.torrent"),
            Ok("torrent")
        );
        assert_eq!(classify_download_url("seeds/rel.torrent"), Ok("torrent"));
    }

    #[test]
    fn classify_rejects_bad_urls() {
        // magnet 缺 btih（如 sha1 tree）→ 明确 400 文案
        assert!(classify_download_url("magnet:?xt=urn:sha1:XYZ&dn=x").is_err());
        assert!(classify_download_url("magnet:?dn=无hash").is_err());
        // 未知 scheme / 裸字符串 / file:// → 拒绝
        assert!(classify_download_url("file:///etc/passwd").is_err());
        assert!(classify_download_url("not-a-url").is_err());
        assert!(classify_download_url("").is_err());
        // http URL 以 .torrent 结尾按直链处理（不误判 torrent）
        assert_eq!(
            classify_download_url("https://x/get/ubuntu.torrent"),
            Ok("http")
        );
    }

    // ---- 多协议：aria2 参数透传（seed-ratio/seed-time）----

    #[test]
    fn build_aria2_add_cmd_magnet_appends_no_seed_options() {
        let body = build_aria2_add_cmd("magnet:?xt=urn:btih:abcdef0123456789&dn=x", "/tank/dl");
        let v: serde_json::Value = serde_json::from_str(&body).expect("合法 JSON");
        assert_eq!(v["method"], "aria2.addUri");
        assert_eq!(v["params"][1]["dir"], "/tank/dl");
        assert_eq!(v["params"][1]["seed-ratio"], "0.0", "下完不分享");
        assert_eq!(v["params"][1]["seed-time"], "0", "立即停止做种");
        // 直链不带 seed 参数
        let plain = build_aria2_add_cmd("https://x/y.iso", "/tank/dl");
        let pv: serde_json::Value = serde_json::from_str(&plain).expect("合法 JSON");
        assert!(pv["params"][1].get("seed-ratio").is_none());
        assert!(pv["params"][1].get("seed-time").is_none());
    }

    #[test]
    fn build_aria2_add_torrent_cmd_shape() {
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"d4:info6:xxxxxe");
        let body = build_aria2_add_torrent_cmd(&b64, "/tank/dl");
        let v: serde_json::Value = serde_json::from_str(&body).expect("合法 JSON");
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "aria2.addTorrent");
        // params[0] = 种子内容 base64（RPC 契约：内容而非路径）
        assert_eq!(v["params"][0], b64);
        // params[1] = Web-Seed URI 空列表
        assert_eq!(v["params"][1], serde_json::json!([]));
        // options：dir + 下完不分享
        assert_eq!(v["params"][2]["dir"], "/tank/dl");
        assert_eq!(v["params"][2]["seed-ratio"], "0.0");
        assert_eq!(v["params"][2]["seed-time"], "0");
    }

    // ---- handler：多协议校验（400 路径不触达 aria2）----

    #[tokio::test]
    async fn create_rejects_magnet_without_btih() {
        let h = DownloadsRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/downloads/tasks",
                serde_json::json!({"url": "magnet:?xt=urn:sha1:XYZ", "save_path": "/tank"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        assert!(
            resp.body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("btih"),
            "错误文案应指明 btih: {resp:?}"
        );
    }

    #[tokio::test]
    async fn create_rejects_unsupported_url_scheme() {
        let h = DownloadsRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/downloads/tasks",
                serde_json::json!({"url": "file:///etc/passwd", "save_path": "/tank"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    #[tokio::test]
    async fn create_torrent_path_missing_file_is_400() {
        let h = DownloadsRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/downloads/tasks",
                serde_json::json!({"url": "/nonexistent/x.torrent", "save_path": "/tank"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        assert!(
            resp.body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("不可读"),
            "应报种子文件不可读: {resp:?}"
        );
    }

    // ---- handler：POST /downloads/torrent 校验（400 路径不触达 aria2）----

    fn torrent_req(body: serde_json::Value) -> ApiRequest {
        post_req("/api/v1/downloads/torrent", body)
    }

    #[tokio::test]
    async fn torrent_upload_validates_body() {
        let h = DownloadsRouteHandler::with_empty();
        // 缺 content_base64 → 400
        let r = h
            .handle(torrent_req(serde_json::json!({"save_path": "/tank"})))
            .await
            .unwrap();
        assert_eq!(r.status, 400, "缺 content_base64");
        // 缺 save_path → 400
        let r = h
            .handle(torrent_req(
                serde_json::json!({"content_base64": "ZDQ6aW5mbw=="}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 400, "缺 save_path");
        // 非法 base64 → 400
        let r = h
            .handle(torrent_req(serde_json::json!({
                "content_base64": "@@not-base64@@",
                "save_path": "/tank",
            })))
            .await
            .unwrap();
        assert_eq!(r.status, 400, "非法 base64");
        // 解码为空 → 400
        let r = h
            .handle(torrent_req(serde_json::json!({
                "content_base64": "",
                "save_path": "/tank",
            })))
            .await
            .unwrap();
        assert_eq!(r.status, 400, "空内容");
    }

    #[tokio::test]
    async fn torrent_upload_rejects_oversize_content() {
        let h = DownloadsRouteHandler::with_empty();
        // 超过 TORRENT_MAX_BYTES（构造 10MiB+1 的零字节载荷）
        let big = vec![0u8; TORRENT_MAX_BYTES + 1];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&big);
        let r = h
            .handle(torrent_req(serde_json::json!({
                "filename": "big.torrent",
                "content_base64": b64,
                "save_path": "/tank",
            })))
            .await
            .unwrap();
        assert_eq!(r.status, 400, "超大种子");
        assert!(
            r.body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("过大"),
            "应报种子过大: {r:?}"
        );
    }

    // ---- 降级：aria2 不可用时 list/stats 不 panic，返回空 ----

    #[tokio::test]
    async fn list_degrades_to_empty_without_panicking() {
        let h = DownloadsRouteHandler::with_empty();
        let resp = h.handle(get_req("/api/v1/downloads/tasks")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body.is_array(), "list 必须返回数组: {resp:?}");
        // aria2 未装时为空；即便装了也无任务，长度为 0
        assert_eq!(resp.body.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn stats_degrades_to_zero_without_panicking() {
        let h = DownloadsRouteHandler::with_empty();
        let resp = h.handle(get_req("/api/v1/downloads/stats")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["total"], 0);
        assert_eq!(resp.body["downloading"], 0);
    }

    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<DownloadsRouteHandler>();
    }
}
