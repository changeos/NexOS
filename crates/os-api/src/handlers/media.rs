//! `MediaRouteHandler` —— 影院 / 音乐 / 相册 媒体库管理的 HTTP→内存态适配器。
//!
//! 定位：把网关 HTTP 请求（`/api/v1/media/*`）翻译为媒体库查询，返回 JSON。
//! 这是 OS"媒体三件套"（Video / Music / Photo）桌面应用的后端 REST 入口。
//!
//! # 当前实现策略：真盘文件优先 + 内存 demo 回退
//!
//! `GET /library?type=<t>` 时，先 `spawn_blocking` 扫描真实根目录
//! （`/tank/media/<type>` → `/var/lib/os/media/<type>`），**只要扫到 ≥1 个真实文件
//! 就只返回真实文件**（每条 `demo: false`，绝不混入 demo 数据）；扫到 0 个文件时
//! 才回退内置 demo 数据（`demo: true`）。`GET /stats` 同样按"真盘优先"统计。
//! `POST /scan` 仅触发"扫描已开始"（内存态，无实际副作用），便于前端演示。
//!
//! # 路由表
//!
//! | method | path                            | 动作 |
//! |--------|---------------------------------|------|
//! | GET    | `/api/v1/media/library?type=`   | 媒体库列表（按 type 过滤；无 type 返回全部）|
//! | GET    | `/api/v1/media/stats`           | 各类计数 + 总大小 |
//! | GET    | `/api/v1/media/item/:id`         | 单条详情（含 stream_url）|
//! | POST   | `/api/v1/media/scan`            | 触发扫描（需 admin）|
//!
//! # 路径参数
//!
//! 网关 dispatch 当前不向 handler 传递 `PathParams`，故 `handle` 从 `req.path`
//! 字符串按段解析（先 `split('?')` 剥离 query，再 `split('/')` 取段；参考
//! `pxe.rs` 的 `path_segments` 模式）。`item/:id` 取末段为条目 id。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use base64::Engine;
use once_cell::sync::Lazy;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

/// 进程级共享 `reqwest::Client`（rustify：curl 子进程 → reqwest）。
static HTTP: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("构建共享 reqwest Client 失败")
});

// ----------------------------------------------------------------------------
// DTO（内存态媒体条目——JSON 结构与真实后端对齐，便于后续无缝替换）
// ----------------------------------------------------------------------------

/// 媒体类型枚举（影院 / 音乐 / 相册），serde snake_case。
///
/// 与 `GET /library?type=<...>` 的 query 参数取值一致（`"video"` / `"music"` /
/// `"photo"`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaType {
    /// 影院（视频）
    Video,
    /// 音乐
    Music,
    /// 相册（图片）
    Photo,
}

impl MediaType {
    /// 从 query 字符串宽松解析（未知值返回 None，由调用方决定回退行为）。
    fn from_query(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "video" | "videos" | "movie" | "movies" => Some(Self::Video),
            "music" | "audio" | "song" | "songs" => Some(Self::Music),
            "photo" | "photos" | "image" | "images" | "picture" | "pictures" => Some(Self::Photo),
            _ => None,
        }
    }

    /// 该类型对应的扫描根目录子段名（`video` / `music` / `photo`）。
    fn dir_segment(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Music => "music",
            Self::Photo => "photo",
        }
    }

    /// 三类枚举全集（用于聚合扫描 / 统计）。
    fn all() -> [Self; 3] {
        [Self::Video, Self::Music, Self::Photo]
    }
}

/// 一条媒体条目（影院 / 音乐 / 相册通用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaItem {
    /// 条目 ID（唯一，用于 `item/:id` 定位）
    pub id: String,
    /// 展示标题
    pub title: String,
    /// 文件路径（相对或绝对）
    pub path: String,
    /// MIME 类型（如 `video/mp4` / `audio/mpeg` / `image/jpeg`）
    pub mime_type: String,
    /// 文件大小（字节）
    pub size_bytes: u64,
    /// 时长（秒）；视频/音频有意义，图片为 None
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u64>,
    /// 缩略图 URL（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail_url: Option<String>,
    /// 创建时间（ISO 字符串或可读文本）
    pub created_at: String,
    /// 标签列表
    pub tags: Vec<String>,
    /// 是否为内置 demo 数据（真实磁盘文件为 false）。
    ///
    /// `GET /library` 在真盘扫到 ≥1 个文件时仅返回 `demo: false` 的真实条目；
    /// 扫到 0 个文件回退 demo 数据时，所有条目 `demo: true`。
    pub demo: bool,
}

/// 媒体库统计（`GET /api/v1/media/stats` 响应）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaStats {
    /// 视频条目数
    pub video_count: usize,
    /// 音乐条目数
    pub music_count: usize,
    /// 相册条目数
    pub photo_count: usize,
    /// 全部条目总大小（字节）
    pub total_size_bytes: u64,
}

/// 单条详情响应（含 mock 的流式播放 URL）。
#[derive(Debug, Clone, Serialize)]
struct ItemDetail {
    #[serde(flatten)]
    item: MediaItem,
    /// mock 流式播放 URL
    stream_url: String,
}

// ----------------------------------------------------------------------------
// TMDB 刮削元数据（落 SQLite）
// ----------------------------------------------------------------------------

/// 一条刮削后的媒体元数据（TMDB）。落 `media_metadata` 表。
///
/// `id` 由 `file_path` 经 FNV-1a 哈希得确定性 ID（同名重刮为 upsert，不重复）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaMetadata {
    pub id: String,
    pub file_path: String,
    pub title: String,
    pub overview: String,
    pub poster_url: String,
    pub backdrop_url: String,
    pub rating: f64,
    pub year: i64,
    /// `movie` / `tv`
    pub media_type: String,
    pub tmdb_id: i64,
    pub scraped_at: String,
}

/// 刮削任务状态（`GET /scrape/status` 响应）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ScrapeState {
    /// `idle` / `running` / `done` / `skipped` / `failed`
    status: String,
    last_run_at: Option<String>,
    scraped_count: u64,
    skipped_count: u64,
    failed_count: u64,
}

/// 单条刮削结果（内部传递用）。
#[derive(Debug)]
enum ScrapeOutcome {
    Ok(MediaMetadata),
    Skipped(String),
    Failed(String),
}

// ----------------------------------------------------------------------------
// AI 相册元数据（Qwen3-VL 视觉识别 → SQLite photo_ai 表）
// ----------------------------------------------------------------------------

/// 本机 vLLM 视觉模型推理端点（OpenAI 兼容），照片 AI 分析转发到此。
const PHOTO_VLLM_ENDPOINT: &str = "http://127.0.0.1:8000/v1/chat/completions";
/// vLLM 视觉模型名（与 llm.rs 对齐：--served-model-name qwen3-vl-8b）。
const PHOTO_VLLM_MODEL: &str = "qwen3-vl-8b";

/// 一张照片的 AI 分析结果。落 `photo_ai` 表（tags/colors 以 JSON 数组字符串存）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhotoAi {
    pub file_path: String,
    pub description: String,
    pub tags: Vec<String>,
    /// `landscape` / `portrait` / `food` / `architecture` / `animal` / `other`
    pub scene: String,
    pub has_people: bool,
    pub colors: Vec<String>,
    pub analyzed_at: String,
}

/// 单张照片分析结果（内部传递用）。
#[derive(Debug)]
enum PhotoAiOutcome {
    Ok(PhotoAi),
    Skipped(String),
    Failed(String),
}

/// 照片分析任务状态（`GET /api/v1/media/photo/analyze/:id` 响应）。
#[derive(Debug, Clone, Serialize)]
struct PhotoAnalyzeTask {
    id: String,
    total: usize,
    done: usize,
    /// `running` / `done` / `skipped` / `failed`
    status: String,
    error: Option<String>,
}

/// 场景分类计数（`GET /api/v1/media/photo/categories` 响应元素）。
#[derive(Debug, Clone, Serialize)]
struct SceneCategory {
    scene: String,
    count: usize,
}

// ----------------------------------------------------------------------------
// MediaRouteHandler
// ----------------------------------------------------------------------------

/// 媒体库路由处理器——HTTP 边界适配到"真盘优先 + 内存 demo 回退"的媒体库。
///
/// 持有 `Mutex<Vec<MediaItem>>`（demo 回退数据，构造时预置三类示例）以及一个可选的
/// 扫描根覆盖（`scan_root`，生产为 `None` 即扫描 `/tank/media`，测试可注入临时目录
/// 以保证确定性）。`new()` 是默认入口（带 demo 数据），`with_items(...)` 供测试注入
/// 空状态或定制数据。
pub struct MediaRouteHandler {
    /// demo 回退数据（真盘扫到文件时不使用）。
    items: Mutex<Vec<MediaItem>>,
    /// 扫描根覆盖。`None` → 生产路径（`/tank/media` → `/var/lib/os/media`）；
    /// `Some(p)` → 扫描 `p/<type>`（测试用，便于指向临时目录）。
    scan_root: Option<PathBuf>,
    /// TMDB 刮削元数据持久层（SQLite）。`Connection` 是 `Send` 非 `Sync`，
    /// 用 `Mutex` 包裹；短锁快查快放，不跨 `.await` 持锁（参考 api_gateway.rs）。
    db: Mutex<Connection>,
    /// 刮削任务状态（`GET /scrape/status`）。
    scrape_status: Mutex<ScrapeState>,
    /// AI 相册分析任务状态（`GET /photo/analyze/:id`），key = task_id。
    analyze_tasks: Mutex<HashMap<String, PhotoAnalyzeTask>>,
}

impl MediaRouteHandler {
    /// 构造 handler，预置 demo 媒体库（三类各 3-5 条，让真盘为空时 `GET` 也有非空响应）。
    #[must_use]
    pub fn new() -> Self {
        Self::build(demo_items(), None, open_media_db(&default_media_db_path()))
    }

    /// 用指定条目列表构造（测试注入空状态或定制数据；scan_root 走生产默认）。
    #[must_use]
    pub fn with_items(items: Vec<MediaItem>) -> Self {
        Self::build(items, None, open_media_db(&default_media_db_path()))
    }

    /// 测试专用构造：给定 items + scan_root，DB 走**内存库**（隔离，进程结束即丢，
    /// 并行测试互不干扰）。
    #[cfg(test)]
    fn new_for_test(items: Vec<MediaItem>, scan_root: Option<PathBuf>) -> Self {
        Self::build(items, scan_root, open_media_db_in_memory())
    }

    /// 统一构造器：组装四个字段。DB 打开失败时由调用方降级（生产）或预期成功（测试）。
    fn build(items: Vec<MediaItem>, scan_root: Option<PathBuf>, db: Connection) -> Self {
        Self {
            items: Mutex::new(items),
            scan_root,
            db: Mutex::new(db),
            scrape_status: Mutex::new(ScrapeState::default()),
            analyze_tasks: Mutex::new(HashMap::new()),
        }
    }

    /// 当前 demo 回退条目快照（测试 / 诊断用）。
    #[must_use]
    pub fn items_snapshot(&self) -> Vec<MediaItem> {
        self.items.lock().expect("items poisoned").clone()
    }

    /// 返回某类型的"生效条目"：真盘扫到 ≥1 个文件则只返真盘条目（`demo:false`），
    /// 否则回退该类型的 demo 数据（`demo:true`，按 MIME 过滤）。
    ///
    /// 这是"真盘优先、不混 demo"语义的核心。
    async fn effective_items_for_type(&self, t: MediaType) -> Vec<MediaItem> {
        let real = scan_media_items(t, self.scan_root.as_deref()).await;
        if !real.is_empty() {
            return real;
        }
        let items = self.items.lock().expect("items poisoned").clone();
        items
            .into_iter()
            .filter(|i| mime_matches_type(&i.mime_type, t))
            .collect()
    }

    /// 按各类生效条目统计（真盘优先）：逐类 `effective_items_for_type` 后计数。
    async fn compute_stats(&self) -> MediaStats {
        let mut video_count = 0usize;
        let mut music_count = 0usize;
        let mut photo_count = 0usize;
        let mut total = 0u64;
        for t in MediaType::all() {
            for it in self.effective_items_for_type(t).await.iter() {
                total += it.size_bytes;
                match t {
                    MediaType::Video => video_count += 1,
                    MediaType::Music => music_count += 1,
                    MediaType::Photo => photo_count += 1,
                }
            }
        }
        MediaStats {
            video_count,
            music_count,
            photo_count,
            total_size_bytes: total,
        }
    }

    // —— TMDB 刮削 ——（见路由 /scrape /scrape/all）

    /// 刮削单个视频文件：读 `TMDB_API_KEY` → 调 TMDB → 存 SQLite。
    ///
    /// `TMDB_API_KEY` 未配置时降级为 `Skipped`，**绝不 panic**。
    async fn scrape_one(&self, file_path: &str, media_type: &str) -> ScrapeOutcome {
        let api_key = std::env::var("TMDB_API_KEY").unwrap_or_default();
        self.scrape_with_key(file_path, media_type, &api_key).await
    }

    /// 刮削核心（接受注入的 api_key，便于测试降级路径，不读 env）。
    async fn scrape_with_key(
        &self,
        file_path: &str,
        media_type: &str,
        api_key: &str,
    ) -> ScrapeOutcome {
        if api_key.trim().is_empty() {
            return ScrapeOutcome::Skipped("TMDB_API_KEY 未配置".into());
        }
        let fname = Path::new(file_path)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| file_path.to_string());
        let title = extract_search_title(&fname);
        if title.trim().is_empty() {
            return ScrapeOutcome::Skipped("无法从文件名提取标题".into());
        }
        let url = build_tmdb_url(&title, api_key, media_type);
        let resp = match fetch_tmdb(&url).await {
            Some(v) => v,
            None => return ScrapeOutcome::Failed("TMDB 请求失败（网络/curl 不可用）".into()),
        };
        let first = resp
            .get("results")
            .and_then(|r| r.as_array())
            .and_then(|a| a.first())
            .cloned();
        let first = match first {
            Some(f) => f,
            None => return ScrapeOutcome::Failed("TMDB 无匹配结果".into()),
        };
        let meta = parse_tmdb_result(&first, file_path, media_type);
        {
            let conn = self.db.lock().expect("db poisoned");
            if upsert_metadata(&conn, &meta).is_err() {
                return ScrapeOutcome::Failed("元数据写入 SQLite 失败".into());
            }
        }
        ScrapeOutcome::Ok(meta)
    }

    /// 把单次刮削结果合并进 scrape_status（计数控件）。
    fn tally_outcome(&self, outcome: &ScrapeOutcome) {
        let mut s = self.scrape_status.lock().expect("status poisoned");
        s.last_run_at = Some(now_iso());
        match outcome {
            ScrapeOutcome::Ok(_) => {
                s.status = "done".into();
                s.scraped_count += 1;
            }
            ScrapeOutcome::Skipped(_) => {
                s.status = "skipped".into();
                s.skipped_count += 1;
            }
            ScrapeOutcome::Failed(_) => {
                s.status = "failed".into();
                s.failed_count += 1;
            }
        }
    }
}

impl Default for MediaRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RouteHandler for MediaRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec(HttpMethod::Get, "/api/v1/media/library", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/media/stats", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/media/item/:id", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/media/scan",
                true,
                vec!["admin".into()],
            ),
            // —— TMDB 刮削（4 条新增）——
            spec(
                HttpMethod::Post,
                "/api/v1/media/scrape",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/media/scrape/status",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/media/scrape/all",
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Get, "/api/v1/media/metadata", false, vec![]),
            // —— AI 相册（5 条新增）——
            spec(
                HttpMethod::Post,
                "/api/v1/media/photo/analyze",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/media/photo/ai-metadata",
                false,
                vec![],
            ),
            spec(HttpMethod::Get, "/api/v1/media/photo/search", false, vec![]),
            spec(
                HttpMethod::Get,
                "/api/v1/media/photo/categories",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/media/photo/analyze/:id",
                false,
                vec![],
            ),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            // —— GET /api/v1/media/library —— 媒体库列表（按 ?type= 过滤）
            //
            // 真盘优先：有效 type 则扫描该类型目录（spawn_blocking），扫到 ≥1 个真实文件
            // 则**只返回真实文件**（demo:false）；扫到 0 个则回退该类型 demo 数据
            // （demo:true）。无 type 参数则逐类聚合（每类独立判断真盘/demo）。
            (HttpMethod::Get, ["api", "v1", "media", "library"]) => {
                let type_q = query_param(&req.path, "type");
                let items: Vec<MediaItem> = match type_q.as_deref().and_then(MediaType::from_query)
                {
                    Some(t) => self.effective_items_for_type(t).await,
                    None => {
                        let mut all = Vec::new();
                        for t in MediaType::all() {
                            all.extend(self.effective_items_for_type(t).await);
                        }
                        all
                    }
                };
                Ok(ok_json(to_value(&items)?))
            }

            // —— GET /api/v1/media/stats —— 媒体库统计（真盘优先）
            (HttpMethod::Get, ["api", "v1", "media", "stats"]) => {
                let stats = self.compute_stats().await;
                Ok(ok_json(to_value(&stats)?))
            }

            // —— GET /api/v1/media/item/:id —— 单条详情（含 stream_url）
            (HttpMethod::Get, ["api", "v1", "media", "item", id]) => {
                let items = self.items.lock().expect("items poisoned").clone();
                let found = items.into_iter().find(|i| i.id == *id);
                match found {
                    Some(item) => {
                        let detail = ItemDetail {
                            stream_url: format!("/api/v1/media/stream/{id}"),
                            item,
                        };
                        Ok(ok_json(to_value(&detail)?))
                    }
                    None => Ok(error_response(404, &format!("媒体条目不存在: {id}"))),
                }
            }

            // —— POST /api/v1/media/scan —— 触发扫描（内存态，无副作用）
            //
            // 需 admin（见 routes 声明）；返回 status=started 的 JSON 占位。
            (HttpMethod::Post, ["api", "v1", "media", "scan"]) => Ok(ok_json(serde_json::json!({
                "status": "started",
                "message": "扫描已触发"
            }))),

            // —— POST /api/v1/media/scrape —— 刮削指定视频文件（admin）
            //
            // body: `{"file_path":"...","media_type":"movie|tv"}`（media_type 默认 movie）。
            // 读 TMDB_API_KEY → 调 TMDB → 存 SQLite。key 未配置降级为 skipped，不 panic。
            (HttpMethod::Post, ["api", "v1", "media", "scrape"]) => {
                let fp = req
                    .body
                    .get("file_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if fp.is_empty() {
                    return Ok(error_response(400, "缺少 file_path"));
                }
                let mt = req
                    .body
                    .get("media_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("movie");
                {
                    let mut s = self.scrape_status.lock().expect("status poisoned");
                    s.status = "running".into();
                }
                let outcome = self.scrape_one(fp, mt).await;
                self.tally_outcome(&outcome);
                Ok(scrape_outcome_response(outcome))
            }

            // —— GET /api/v1/media/scrape/status —— 刮削任务状态
            (HttpMethod::Get, ["api", "v1", "media", "scrape", "status"]) => {
                let s = self.scrape_status.lock().expect("status poisoned").clone();
                Ok(ok_json(serde_json::to_value(&s).unwrap_or_default()))
            }

            // —— POST /api/v1/media/scrape/all —— 批量刮削所有视频（admin）
            //
            // 逐条对生效视频列表调用 scrape_one（TMDB_API_KEY 未配置则全部 skipped）。
            (HttpMethod::Post, ["api", "v1", "media", "scrape", "all"]) => {
                {
                    let mut s = self.scrape_status.lock().expect("status poisoned");
                    s.status = "running".into();
                }
                let videos = self.effective_items_for_type(MediaType::Video).await;
                let total = videos.len();
                let mut scraped = 0usize;
                let mut skipped = 0usize;
                let mut failed = 0usize;
                for v in &videos {
                    match self.scrape_one(&v.path, "movie").await {
                        ScrapeOutcome::Ok(_) => scraped += 1,
                        ScrapeOutcome::Skipped(_) => skipped += 1,
                        ScrapeOutcome::Failed(_) => failed += 1,
                    }
                }
                {
                    let mut s = self.scrape_status.lock().expect("status poisoned");
                    s.status = "done".into();
                    s.last_run_at = Some(now_iso());
                    s.scraped_count += scraped as u64;
                    s.skipped_count += skipped as u64;
                    s.failed_count += failed as u64;
                }
                Ok(ok_json(serde_json::json!({
                    "status": "done",
                    "total": total,
                    "scraped": scraped,
                    "skipped": skipped,
                    "failed": failed,
                })))
            }

            // —— GET /api/v1/media/metadata —— 刮削后的元数据列表（含海报/剧情/评分）
            (HttpMethod::Get, ["api", "v1", "media", "metadata"]) => {
                let list = {
                    let conn = self.db.lock().expect("db poisoned");
                    load_all_metadata(&conn).unwrap_or_default()
                };
                Ok(ok_json(to_value(&list)?))
            }

            // —— POST /api/v1/media/photo/analyze —— AI 分析照片（admin）
            //
            // body: `{file_path?}`。有 file_path → 分析单张；无 → 分析全部未分析的照片。
            // 对每张图：spawn_blocking 读 base64 → spawn curl 调 vLLM :8000 → 解析 JSON →
            // 存 photo_ai 表。vLLM 不在线（:8000 不通）降级为 skipped，**不 panic**。
            (HttpMethod::Post, ["api", "v1", "media", "photo", "analyze"]) => {
                let file_path = req
                    .body
                    .get("file_path")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let task_id = format!("photo-{}", now_ts_millis());
                if let Some(fp) = file_path {
                    // —— 单张分析 ——
                    {
                        let mut tasks = self.analyze_tasks.lock().expect("tasks poisoned");
                        tasks.insert(
                            task_id.clone(),
                            PhotoAnalyzeTask {
                                id: task_id.clone(),
                                total: 1,
                                done: 0,
                                status: "running".into(),
                                error: None,
                            },
                        );
                    }
                    let outcome = analyze_photo_one(&fp).await;
                    let body = match &outcome {
                        PhotoAiOutcome::Ok(p) => {
                            {
                                let conn = self.db.lock().expect("db poisoned");
                                let _ = upsert_photo_ai(&conn, p);
                            }
                            let mut tasks = self.analyze_tasks.lock().expect("tasks poisoned");
                            if let Some(t) = tasks.get_mut(&task_id) {
                                t.done = 1;
                                t.status = "done".into();
                            }
                            serde_json::json!({
                                "status": "ok", "task_id": task_id, "metadata": p
                            })
                        }
                        PhotoAiOutcome::Skipped(reason) => {
                            let mut tasks = self.analyze_tasks.lock().expect("tasks poisoned");
                            if let Some(t) = tasks.get_mut(&task_id) {
                                t.status = "skipped".into();
                                t.error = Some(reason.clone());
                            }
                            serde_json::json!({
                                "status": "skipped", "task_id": task_id, "reason": reason
                            })
                        }
                        PhotoAiOutcome::Failed(reason) => {
                            let mut tasks = self.analyze_tasks.lock().expect("tasks poisoned");
                            if let Some(t) = tasks.get_mut(&task_id) {
                                t.status = "failed".into();
                                t.error = Some(reason.clone());
                            }
                            serde_json::json!({
                                "status": "failed", "task_id": task_id, "reason": reason
                            })
                        }
                    };
                    Ok(ok_json(body))
                } else {
                    // —— 全部未分析照片 ——
                    let photos = self.effective_items_for_type(MediaType::Photo).await;
                    let existing: std::collections::HashSet<String> = {
                        let conn = self.db.lock().expect("db poisoned");
                        load_all_photo_ai(&conn)
                            .unwrap_or_default()
                            .into_iter()
                            .map(|p| p.file_path)
                            .collect()
                    };
                    let targets: Vec<String> = photos
                        .iter()
                        .map(|p| p.path.clone())
                        .filter(|p| !existing.contains(p))
                        .collect();
                    let total = targets.len();
                    {
                        let mut tasks = self.analyze_tasks.lock().expect("tasks poisoned");
                        tasks.insert(
                            task_id.clone(),
                            PhotoAnalyzeTask {
                                id: task_id.clone(),
                                total,
                                done: 0,
                                status: "running".into(),
                                error: None,
                            },
                        );
                    }
                    let mut done = 0usize;
                    let mut ok = 0usize;
                    let mut skipped = 0usize;
                    let mut failed = 0usize;
                    for fp in &targets {
                        match analyze_photo_one(fp).await {
                            PhotoAiOutcome::Ok(p) => {
                                let conn = self.db.lock().expect("db poisoned");
                                let _ = upsert_photo_ai(&conn, &p);
                                ok += 1;
                            }
                            PhotoAiOutcome::Skipped(_) => skipped += 1,
                            PhotoAiOutcome::Failed(_) => failed += 1,
                        }
                        done += 1;
                        let mut tasks = self.analyze_tasks.lock().expect("tasks poisoned");
                        if let Some(t) = tasks.get_mut(&task_id) {
                            t.done = done;
                        }
                    }
                    {
                        let mut tasks = self.analyze_tasks.lock().expect("tasks poisoned");
                        if let Some(t) = tasks.get_mut(&task_id) {
                            t.status = if total > 0 && skipped == total {
                                "skipped".into()
                            } else {
                                "done".into()
                            };
                        }
                    }
                    Ok(ok_json(serde_json::json!({
                        "status": if total > 0 && skipped == total { "skipped" } else { "done" },
                        "task_id": task_id,
                        "total": total,
                        "analyzed": ok,
                        "skipped": skipped,
                        "failed": failed,
                    })))
                }
            }

            // —— GET /api/v1/media/photo/ai-metadata —— 列全部 AI 元数据
            (HttpMethod::Get, ["api", "v1", "media", "photo", "ai-metadata"]) => {
                let list = {
                    let conn = self.db.lock().expect("db poisoned");
                    load_all_photo_ai(&conn).unwrap_or_default()
                };
                Ok(ok_json(to_value(&list)?))
            }

            // —— GET /api/v1/media/photo/search?q= —— 语义搜索（按 tags/description/scene 模糊匹配）
            (HttpMethod::Get, ["api", "v1", "media", "photo", "search"]) => {
                let q = query_param(&req.path, "q").unwrap_or_default();
                let list = {
                    let conn = self.db.lock().expect("db poisoned");
                    if q.trim().is_empty() {
                        load_all_photo_ai(&conn).unwrap_or_default()
                    } else {
                        search_photo_ai(&conn, q.trim()).unwrap_or_default()
                    }
                };
                Ok(ok_json(to_value(&list)?))
            }

            // —— GET /api/v1/media/photo/categories —— 按场景分类统计
            (HttpMethod::Get, ["api", "v1", "media", "photo", "categories"]) => {
                let list = {
                    let conn = self.db.lock().expect("db poisoned");
                    count_photo_categories(&conn).unwrap_or_default()
                };
                Ok(ok_json(to_value(&list)?))
            }

            // —— GET /api/v1/media/photo/analyze/:id —— 分析任务状态
            (HttpMethod::Get, ["api", "v1", "media", "photo", "analyze", id]) => {
                let task = {
                    let tasks = self.analyze_tasks.lock().expect("tasks poisoned");
                    tasks.get(*id).cloned()
                };
                match task {
                    Some(t) => Ok(ok_json(to_value(&t)?)),
                    None => Ok(error_response(404, &format!("分析任务不存在: {id}"))),
                }
            }

            // —— 未覆盖路由 —— 兜底 404（Ok，非 Err，便于上层定位）
            _ => Ok(error_response(404, "media: 未匹配的路由")),
        }
    }
}

// ----------------------------------------------------------------------------
// 内部辅助
// ----------------------------------------------------------------------------

/// 构造一条 [`RouteSpec`]（component 固定 `media`）。
fn spec(
    method: HttpMethod,
    path: &str,
    requires_auth: bool,
    required_roles: Vec<String>,
) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: "media".to_string(),
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

/// 把可序列化结果转成 `serde_json::Value`，序列化失败统一映射为 `ApiGatewayError::Internal`。
fn to_value<T: Serialize>(v: &T) -> Result<serde_json::Value, ApiGatewayError> {
    serde_json::to_value(v).map_err(|e| ApiGatewayError::Internal(format!("响应序列化失败: {e}")))
}

/// 把单次刮削结果转成 200 JSON 响应（ok 带 metadata；skipped/failed 带 reason）。
fn scrape_outcome_response(outcome: ScrapeOutcome) -> ApiResponse {
    match outcome {
        ScrapeOutcome::Ok(m) => {
            let body = serde_json::to_value(&m).unwrap_or(serde_json::json!({}));
            ok_json(serde_json::json!({ "status": "ok", "metadata": body }))
        }
        ScrapeOutcome::Skipped(reason) => ok_json(serde_json::json!({
            "status": "skipped", "reason": reason
        })),
        ScrapeOutcome::Failed(reason) => ok_json(serde_json::json!({
            "status": "failed", "reason": reason
        })),
    }
}

// ----------------------------------------------------------------------------
// TMDB 刮削纯函数 + HTTP
// ----------------------------------------------------------------------------

/// 从视频文件名提取 TMDB 搜索关键词。
///
/// 规则：取去掉扩展名的 stem，按 `.` / `_` / `-` 分词；遇到 **年份（1900-2099 的 4 位
/// 数字）** 或 **常见质量/分辨率噪声 token**（1080p / bluray / x264 …）即截断；
/// 剩余 token 用空格拼接。中文文件名（无分隔符）原样保留。
///
/// 例：`Family.Trip.2025.1080p.mp4` → `Family Trip`
///     `家庭旅行.mp4` → `家庭旅行`
///     `The.Matrix.1999.720p.BrRip.mkv` → `The Matrix`
pub fn extract_search_title(filename: &str) -> String {
    let stem = filename
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(filename);
    let mut words: Vec<&str> = Vec::new();
    for part in stem.split(['.', '_', '-']) {
        let t = part.trim();
        if t.is_empty() {
            continue;
        }
        if is_noise_token(t) {
            break;
        }
        words.push(t);
    }
    words.join(" ").trim().to_string()
}

/// 判断一个分词是否为"噪声/终止" token（年份或分辨率/质量标记）。
fn is_noise_token(t: &str) -> bool {
    if t.len() == 4 && t.chars().all(|c| c.is_ascii_digit()) {
        let n: u32 = t.parse().unwrap_or(0);
        if (1900..=2099).contains(&n) {
            return true;
        }
    }
    matches!(
        t.to_ascii_lowercase().as_str(),
        "1080p"
            | "720p"
            | "480p"
            | "1440p"
            | "2160p"
            | "4k"
            | "uhd"
            | "hd"
            | "sd"
            | "bluray"
            | "blu-ray"
            | "bdrip"
            | "brrip"
            | "dvdrip"
            | "webrip"
            | "web-dl"
            | "webdl"
            | "hdtv"
            | "cam"
            | "hdr"
            | "hdr10"
            | "x264"
            | "x265"
            | "h264"
            | "h265"
            | "hevc"
            | "aac"
            | "ac3"
            | "dts"
            | "10bit"
            | "remux"
            | "amzn"
            | "netflix"
            | "atmos"
            | "proper"
            | "repack"
            | "extended"
            | "imax"
    )
}

/// 构造 TMDB v3 search URL（含 api_key + query + language=zh-CN）。
///
/// `media_type` 为 `movie` / `tv`（大小写不敏感，其它一律按 movie）。
pub fn build_tmdb_url(title: &str, api_key: &str, media_type: &str) -> String {
    let kind = if media_type.eq_ignore_ascii_case("tv") {
        "tv"
    } else {
        "movie"
    };
    let q = url_encode(title);
    format!("https://api.themoviedb.org/3/search/{kind}?api_key={api_key}&query={q}&language=zh-CN")
}

/// URL 百分号编码（query 用）。保留 `[A-Za-z0-9-_.~]`，空格→`%20`，其余→`%XX`。
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 由 file_path 生成确定性 ID（FNV-1a 64bit → hex），重刮为 upsert。
fn id_for_path(file_path: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in file_path.as_bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("media-{hash:016x}")
}

/// 当前 ISO8601 时间戳（含本地时区偏移）。
fn now_iso() -> String {
    chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

/// 用 `reqwest` 抓取 TMDB JSON（rustify：原 curl 子进程迁移）。失败返回 None（不 panic）。
async fn fetch_tmdb(url: &str) -> Option<serde_json::Value> {
    let resp = HTTP
        .get(url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    resp.json::<serde_json::Value>().await.ok()
}

/// 把 TMDB search 的单条结果解析为 [`MediaMetadata`]。
///
/// 电影取 `title` / `release_date`；剧集取 `name` / `first_air_date`。海报拼
/// `https://image.tmdb.org/t/p/w500<poster_path>`，背景图拼 `w780`。
fn parse_tmdb_result(item: &serde_json::Value, file_path: &str, media_type: &str) -> MediaMetadata {
    let poster_path = item
        .get("poster_path")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let backdrop_path = item
        .get("backdrop_path")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let title = item
        .get("title")
        .and_then(|v| v.as_str())
        .or_else(|| item.get("name").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();
    let overview = item
        .get("overview")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let rating = item
        .get("vote_average")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let tmdb_id = item.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
    let date_str = item
        .get("release_date")
        .and_then(|v| v.as_str())
        .or_else(|| item.get("first_air_date").and_then(|v| v.as_str()))
        .unwrap_or("");
    let year = date_str
        .get(0..4)
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let poster_url = if poster_path.is_empty() {
        String::new()
    } else {
        format!("https://image.tmdb.org/t/p/w500{poster_path}")
    };
    let backdrop_url = if backdrop_path.is_empty() {
        String::new()
    } else {
        format!("https://image.tmdb.org/t/p/w780{backdrop_path}")
    };
    MediaMetadata {
        id: id_for_path(file_path),
        file_path: file_path.to_string(),
        title,
        overview,
        poster_url,
        backdrop_url,
        rating,
        year,
        media_type: media_type.to_string(),
        tmdb_id,
        scraped_at: now_iso(),
    }
}

// ----------------------------------------------------------------------------
// AI 相册：纯函数 + vLLM 视觉分析
// ----------------------------------------------------------------------------

/// 构造 vLLM 分析请求体（rustify：原 curl 命令参数构造改为直接构造 JSON payload，
/// 由 caller 经共享 reqwest Client POST 到 `PHOTO_VLLM_ENDPOINT`）。
///
/// 请求体为 OpenAI 兼容多模态格式：model=`qwen3-vl-8b`，messages 含 `image_url`
/// （data URL，base64）+ text（结构化 JSON 输出指令），max_tokens=200。
/// `file_path` 仅用于签名语义（caller 透传），不进入请求体。
#[must_use]
pub fn build_photo_analyze_payload(base64_data: &str) -> serde_json::Value {
    let prompt = "分析这张图片，用JSON返回：{\"description\":\"描述\",\"tags\":[\"标签1\",\"标签2\"],\"scene\":\"landscape|portrait|food|architecture|animal|other\",\"has_people\":true/false,\"colors\":[\"主色1\",\"主色2\"]}";
    let data_url = format!("data:image/png;base64,{base64_data}");
    serde_json::json!({
        "model": PHOTO_VLLM_MODEL,
        "messages": [{
            "role": "user",
            "content": [
                {"type": "image_url", "image_url": {"url": data_url}},
                {"type": "text", "text": prompt},
            ]
        }],
        "max_tokens": 200,
    })
}

/// 从 vLLM 文本响应提取 JSON 字符串（处理 ```json 包裹、裸 JSON、前后噪声文本）。
///
/// 策略：① 优先匹配 ` ```json ... ``` ` / ` ``` ... ``` ` 代码围栏；② 否则取首个 `{`
/// 到末个 `}` 的子串；③ 对候选做 `serde_json` 校验，合法才返回，否则 None。
#[must_use]
pub fn extract_ai_json(raw_text: &str) -> Option<String> {
    let trimmed = raw_text.trim();
    // ① 代码围栏
    if let Some(start) = trimmed.find("```") {
        let after = &trimmed[start + 3..];
        // 去掉可选的语言标签（如 json），允许其后紧跟换行
        let after = after
            .strip_prefix("json")
            .or_else(|| after.strip_prefix("JSON"))
            .unwrap_or(after);
        let after = after.trim_start();
        if let Some(end) = after.find("```") {
            let candidate = after[..end].trim();
            if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
                return Some(candidate.to_string());
            }
        }
    }
    // ② 裸 JSON 子串（首 { 到末 }）
    if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}')) {
        if end > start {
            let candidate = &trimmed[start..=end];
            if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

/// 场景英文键转中文展示名。
#[must_use]
pub fn scene_to_cn(scene: &str) -> &'static str {
    match scene.trim().to_ascii_lowercase().as_str() {
        "landscape" => "风景",
        "portrait" => "人物",
        "food" => "食物",
        "architecture" => "建筑",
        "animal" => "动物",
        _ => "其它",
    }
}

/// 把模型返回的 scene 归一化到合法枚举（非法值 → other）。
fn normalize_scene(s: &str) -> String {
    match s.trim().to_ascii_lowercase().as_str() {
        "landscape" | "portrait" | "food" | "architecture" | "animal" => {
            s.trim().to_ascii_lowercase()
        }
        _ => "other".into(),
    }
}

/// 探活本机 vLLM（:8000/health，reqwest GET）。失败返回 false（不 panic）。
async fn is_vllm_alive() -> bool {
    HTTP.get("http://127.0.0.1:8000/health")
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map(|_| true)
        .unwrap_or(false)
}

/// 读图片文件并 base64 编码（spawn_blocking 调用，文件不存在/读失败返回 None）。
fn read_file_base64(path: &str) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.is_empty() {
        return None;
    }
    Some(base64::engine::general_purpose::STANDARD.encode(&bytes))
}

/// 从 OpenAI 兼容 chat/completions 响应抽取 `choices[0].message.content` 文本。
fn extract_chat_content(resp: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(resp).ok()?;
    v.get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(String::from)
}

/// 分析单张照片：探活 → 读 base64 → reqwest POST vLLM → 解析 JSON → 构造 PhotoAi。
///
/// **降级**：vLLM 不在线（:8000 不通）→ `Skipped("vLLM 未运行")`，绝不 panic。
async fn analyze_photo_one(file_path: &str) -> PhotoAiOutcome {
    // 1. 探活 vLLM
    if !is_vllm_alive().await {
        return PhotoAiOutcome::Skipped("vLLM 未运行".into());
    }
    // 2. 读 base64（spawn_blocking，避免阻塞 runtime）
    let fp_owned = file_path.to_string();
    let b64 = match tokio::task::spawn_blocking(move || read_file_base64(&fp_owned)).await {
        Ok(Some(b)) => b,
        _ => return PhotoAiOutcome::Failed("读取图片失败（文件不存在或为空）".into()),
    };
    // 3. reqwest POST vLLM（rustify：原 curl 子进程迁移）
    let payload = build_photo_analyze_payload(&b64);
    let resp = match HTTP
        .post(PHOTO_VLLM_ENDPOINT)
        .timeout(Duration::from_secs(60))
        .json(&payload)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => return PhotoAiOutcome::Failed(format!("vLLM 请求失败（HTTP {}）", r.status())),
        Err(_) => return PhotoAiOutcome::Failed("vLLM 请求发送失败".into()),
    };
    let resp_text = match resp.text().await {
        Ok(t) => t,
        Err(_) => return PhotoAiOutcome::Failed("读取 vLLM 响应失败".into()),
    };
    // 4. 抽取 content 文本
    let content = match extract_chat_content(&resp_text) {
        Some(c) => c,
        None => return PhotoAiOutcome::Failed("解析 vLLM 响应失败".into()),
    };
    // 5. 提取 JSON
    let json_str = match extract_ai_json(&content) {
        Some(j) => j,
        None => return PhotoAiOutcome::Failed("响应中无合法 JSON".into()),
    };
    let v: serde_json::Value = match serde_json::from_str(&json_str) {
        Ok(v) => v,
        Err(_) => return PhotoAiOutcome::Failed("JSON 解析失败".into()),
    };
    // 6. 构造 PhotoAi
    let description = v
        .get("description")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let tags: Vec<String> = v
        .get("tags")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let scene = normalize_scene(v.get("scene").and_then(|x| x.as_str()).unwrap_or("other"));
    let has_people = v
        .get("has_people")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let colors: Vec<String> = v
        .get("colors")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    PhotoAiOutcome::Ok(PhotoAi {
        file_path: file_path.to_string(),
        description,
        tags,
        scene,
        has_people,
        colors,
        analyzed_at: now_iso(),
    })
}

/// 当前毫秒时间戳（用于生成唯一任务 id）。
fn now_ts_millis() -> i64 {
    use chrono::Local;
    Local::now().timestamp_millis()
}

// ----------------------------------------------------------------------------
// 元数据 SQLite 持久层（rusqlite，bundled；参考 api_gateway.rs 模式）
// ----------------------------------------------------------------------------

/// 默认元数据 DB 路径：优先 `/tank/os-data/media.db`，再 `/var/lib/os/media.db`，
/// 最后 `./media.db`（保底）。
fn default_media_db_path() -> String {
    for p in &["/tank/os-data/media.db", "/var/lib/os/media.db"] {
        if Path::new(p)
            .parent()
            .is_some_and(|d| d.exists() || std::fs::create_dir_all(d).is_ok())
        {
            return (*p).to_string();
        }
    }
    "./media.db".to_string()
}

/// 打开 SQLite 文件库 + 建表。失败时调用方降级到内存库。
fn open_media_db(path: &str) -> Connection {
    match Connection::open(path).and_then(|c| {
        let _ = c.pragma_update(None, "journal_mode", "WAL");
        create_media_schema(&c)?;
        Ok(c)
    }) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("media: 打开 SQLite {path} 失败（{e}），降级到内存库");
            open_media_db_in_memory()
        }
    }
}

/// 打开内存库 + 建表（测试隔离用）。
fn open_media_db_in_memory() -> Connection {
    let conn = Connection::open_in_memory().expect("内存库必成功");
    create_media_schema(&conn).expect("建表必成功");
    conn
}

/// 建 `media_metadata` 表 + `photo_ai` 表（IF NOT EXISTS）。
fn create_media_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS media_metadata (
            id TEXT PRIMARY KEY,
            file_path TEXT NOT NULL,
            title TEXT NOT NULL,
            overview TEXT NOT NULL DEFAULT '',
            poster_url TEXT NOT NULL DEFAULT '',
            backdrop_url TEXT NOT NULL DEFAULT '',
            rating REAL NOT NULL DEFAULT 0,
            year INTEGER NOT NULL DEFAULT 0,
            media_type TEXT NOT NULL,
            tmdb_id INTEGER NOT NULL DEFAULT 0,
            scraped_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS photo_ai (
            file_path TEXT PRIMARY KEY,
            description TEXT NOT NULL DEFAULT '',
            tags TEXT NOT NULL DEFAULT '[]',
            scene TEXT NOT NULL DEFAULT 'other',
            has_people INTEGER NOT NULL DEFAULT 0,
            colors TEXT NOT NULL DEFAULT '[]',
            analyzed_at TEXT NOT NULL
        );",
    )
}

/// 插入或更新一条元数据（按 id upsert）。
fn upsert_metadata(conn: &Connection, m: &MediaMetadata) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO media_metadata
            (id,file_path,title,overview,poster_url,backdrop_url,rating,year,media_type,tmdb_id,scraped_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
         ON CONFLICT(id) DO UPDATE SET
            title=excluded.title, overview=excluded.overview, poster_url=excluded.poster_url,
            backdrop_url=excluded.backdrop_url, rating=excluded.rating, year=excluded.year,
            media_type=excluded.media_type, tmdb_id=excluded.tmdb_id, scraped_at=excluded.scraped_at",
        params![
            m.id,
            m.file_path,
            m.title,
            m.overview,
            m.poster_url,
            m.backdrop_url,
            m.rating,
            m.year,
            m.media_type,
            m.tmdb_id,
            m.scraped_at,
        ],
    )?;
    Ok(())
}

/// 读取全部元数据（按 title 升序）。
fn load_all_metadata(conn: &Connection) -> rusqlite::Result<Vec<MediaMetadata>> {
    let mut stmt = conn.prepare(
        "SELECT id,file_path,title,overview,poster_url,backdrop_url,rating,year,media_type,tmdb_id,scraped_at
         FROM media_metadata ORDER BY title",
    )?;
    let rows = stmt.query_map([], metadata_from_row)?;
    rows.collect()
}

/// 单行 → [`MediaMetadata`]。
fn metadata_from_row(row: &rusqlite::Row) -> rusqlite::Result<MediaMetadata> {
    Ok(MediaMetadata {
        id: row.get(0)?,
        file_path: row.get(1)?,
        title: row.get(2)?,
        overview: row.get(3)?,
        poster_url: row.get(4)?,
        backdrop_url: row.get(5)?,
        rating: row.get(6)?,
        year: row.get(7)?,
        media_type: row.get(8)?,
        tmdb_id: row.get(9)?,
        scraped_at: row.get(10)?,
    })
}

// —— photo_ai 表持久层 ——

/// 插入或更新一条照片 AI 元数据（按 file_path upsert）。
fn upsert_photo_ai(conn: &Connection, p: &PhotoAi) -> rusqlite::Result<()> {
    let tags_json = serde_json::to_string(&p.tags).unwrap_or_else(|_| "[]".into());
    let colors_json = serde_json::to_string(&p.colors).unwrap_or_else(|_| "[]".into());
    conn.execute(
        "INSERT INTO photo_ai
            (file_path,description,tags,scene,has_people,colors,analyzed_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(file_path) DO UPDATE SET
            description=excluded.description, tags=excluded.tags, scene=excluded.scene,
            has_people=excluded.has_people, colors=excluded.colors,
            analyzed_at=excluded.analyzed_at",
        params![
            p.file_path,
            p.description,
            tags_json,
            p.scene,
            i64::from(p.has_people),
            colors_json,
            p.analyzed_at,
        ],
    )?;
    Ok(())
}

/// 单行 → [`PhotoAi`]（tags/colors 从 JSON 字符串反序列化）。
fn photo_ai_from_row(row: &rusqlite::Row) -> rusqlite::Result<PhotoAi> {
    let tags_str: String = row.get(2)?;
    let colors_str: String = row.get(5)?;
    let has_people: i64 = row.get(4)?;
    let tags: Vec<String> = serde_json::from_str(&tags_str).unwrap_or_default();
    let colors: Vec<String> = serde_json::from_str(&colors_str).unwrap_or_default();
    Ok(PhotoAi {
        file_path: row.get(0)?,
        description: row.get(1)?,
        tags,
        scene: row.get(3)?,
        has_people: has_people != 0,
        colors,
        analyzed_at: row.get(6)?,
    })
}

/// 读取全部照片 AI 元数据（按 analyzed_at 降序）。
fn load_all_photo_ai(conn: &Connection) -> rusqlite::Result<Vec<PhotoAi>> {
    let mut stmt = conn.prepare(
        "SELECT file_path,description,tags,scene,has_people,colors,analyzed_at
         FROM photo_ai ORDER BY analyzed_at DESC",
    )?;
    let rows = stmt.query_map([], photo_ai_from_row)?;
    rows.collect()
}

/// 按 query 在 description/tags/scene 模糊搜索（LIKE %q%）。返回匹配条目。
fn search_photo_ai(conn: &Connection, q: &str) -> rusqlite::Result<Vec<PhotoAi>> {
    let like = format!("%{q}%");
    let mut stmt = conn.prepare(
        "SELECT file_path,description,tags,scene,has_people,colors,analyzed_at
         FROM photo_ai
         WHERE description LIKE ?1 OR tags LIKE ?1 OR scene LIKE ?1
         ORDER BY analyzed_at DESC",
    )?;
    let rows = stmt.query_map(params![like], photo_ai_from_row)?;
    rows.collect()
}

/// 按场景分组计数（count 降序）。
fn count_photo_categories(conn: &Connection) -> rusqlite::Result<Vec<SceneCategory>> {
    let mut stmt = conn
        .prepare("SELECT scene, COUNT(*) AS cnt FROM photo_ai GROUP BY scene ORDER BY cnt DESC")?;
    let rows = stmt.query_map([], |row| {
        Ok(SceneCategory {
            scene: row.get(0)?,
            count: row.get::<_, i64>(1)? as usize,
        })
    })?;
    rows.collect()
}

/// 从请求路径中剥离 `?query` 后的纯 path 段（前后空段去除）。
///
/// 例：`/api/v1/media/library?type=video` → `["api", "v1", "media", "library"]`。
fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

/// 从请求路径的 query string 中提取指定参数（首个匹配）。
///
/// 例：`/api/v1/media/library?type=video&x=1` 取 `"type"` → `Some("video")`。
fn query_param(path: &str, key: &str) -> Option<String> {
    let q = path.split('?').nth(1)?;
    for kv in q.split('&') {
        let mut it = kv.splitn(2, '=');
        if it.next()? == key {
            let v = it.next().unwrap_or("");
            let decoded = url_decode(v);
            if decoded.is_empty() {
                return None;
            }
            return Some(decoded);
        }
    }
    None
}

/// 极简 URL 解码（仅处理 `+` → 空格 与 `%XX`）。
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let Ok(b) =
                    u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
                {
                    out.push(b);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 判断 MIME 前缀是否属于指定媒体类型。
fn mime_matches_type(mime: &str, t: MediaType) -> bool {
    match t {
        MediaType::Video => mime.starts_with("video"),
        MediaType::Music => mime.starts_with("audio"),
        MediaType::Photo => mime.starts_with("image"),
    }
}

/// 扫描某类型媒体目录（spawn_blocking），返回真实磁盘上的 [`MediaItem`] 列表。
///
/// `base` 为 `None` 时优先 `/tank/media/<type>`，不存在则回退
/// `/var/lib/os/media/<type>`；`base` 为 `Some(p)` 时只扫描 `p/<type>`（测试用）。
/// 任一根目录扫到 ≥1 个真实文件即返回该目录的全部条目；都没文件返回空 Vec
/// （调用方据此回退 demo 数据）。本函数不修改内存库。
async fn scan_media_items(t: MediaType, base: Option<&Path>) -> Vec<MediaItem> {
    let seg = t.dir_segment().to_string();
    let base_owned = base.map(|p| p.to_path_buf());
    tokio::task::spawn_blocking(move || {
        let mut roots: Vec<PathBuf> = Vec::new();
        if let Some(b) = base_owned {
            roots.push(b.join(&seg));
        } else {
            roots.push(PathBuf::from(format!("/tank/media/{seg}")));
            roots.push(PathBuf::from(format!("/var/lib/os/media/{seg}")));
        }
        for root in roots {
            if let Ok(entries) = std::fs::read_dir(&root) {
                let mut items = Vec::new();
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file() {
                        if let Some(item) = file_to_media_item(&path, t) {
                            items.push(item);
                        }
                    }
                }
                if !items.is_empty() {
                    return items;
                }
            }
        }
        Vec::new()
    })
    .await
    .unwrap_or_default()
}

/// 把一个真实磁盘文件转成 [`MediaItem`]（`demo: false`）。
///
/// 读取大小与 mtime；MIME 由扩展名推断（未知扩展名回退到该类型的通用 MIME，
/// 保证真盘文件一定被列出）。元数据读取失败的文件被跳过（返回 None）。
fn file_to_media_item(path: &Path, t: MediaType) -> Option<MediaItem> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let fname = path.file_name()?.to_string_lossy().to_string();
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| fname.clone());
    let mime = guess_mime(&fname, t);
    let created_at = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|| "0".to_string());
    Some(MediaItem {
        id: format!("real:{}", path.to_string_lossy()),
        title: stem,
        path: path.to_string_lossy().to_string(),
        mime_type: mime.to_string(),
        size_bytes: meta.len(),
        duration_secs: None,
        thumbnail_url: None,
        created_at,
        tags: Vec::new(),
        demo: false,
    })
}

/// 按文件名扩展名 + 媒体类型推断 MIME。未知扩展名回退到该类型通用 MIME。
fn guess_mime(fname: &str, t: MediaType) -> &'static str {
    let lower = fname.to_ascii_lowercase();
    let ext = lower.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match t {
        MediaType::Video => match ext {
            "mp4" => "video/mp4",
            "mkv" => "video/x-matroska",
            "mov" => "video/quicktime",
            "webm" => "video/webm",
            "avi" => "video/x-msvideo",
            "m4v" => "video/x-m4v",
            "ts" => "video/mp2t",
            "wmv" => "video/x-ms-wmv",
            "flv" => "video/x-flv",
            _ => "video/mp4",
        },
        MediaType::Music => match ext {
            "mp3" => "audio/mpeg",
            "flac" => "audio/flac",
            "m4a" => "audio/mp4",
            "aac" => "audio/aac",
            "wav" => "audio/wav",
            "ogg" => "audio/ogg",
            "opus" => "audio/opus",
            "wma" => "audio/x-ms-wma",
            _ => "audio/mpeg",
        },
        MediaType::Photo => match ext {
            "jpg" | "jpeg" => "image/jpeg",
            "png" => "image/png",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "bmp" => "image/bmp",
            "tiff" | "tif" => "image/tiff",
            "heic" => "image/heic",
            _ => "image/jpeg",
        },
    }
}

/// 默认 demo 媒体库（三类各若干条，真盘为空时作为回退，`demo: true`）。
fn demo_items() -> Vec<MediaItem> {
    vec![
        // —— 视频 ——
        MediaItem {
            id: "v-demo-1".into(),
            title: "家庭旅行 2025".into(),
            path: "/tank/media/video/family-trip-2025.mp4".into(),
            mime_type: "video/mp4".into(),
            size_bytes: 1_200_000_000,
            duration_secs: Some(3_600),
            thumbnail_url: Some("https://picsum.photos/seed/vid1/640/360".into()),
            created_at: "2025-07-12T10:30:00+08:00".into(),
            tags: vec!["旅行".into(), "家庭".into()],
            demo: true,
        },
        MediaItem {
            id: "v-demo-2".into(),
            title: "生日聚会".into(),
            path: "/tank/media/video/birthday.mkv".into(),
            mime_type: "video/x-matroska".into(),
            size_bytes: 850_000_000,
            duration_secs: Some(2_400),
            thumbnail_url: Some("https://picsum.photos/seed/vid2/640/360".into()),
            created_at: "2025-05-01T18:00:00+08:00".into(),
            tags: vec!["家庭".into()],
            demo: true,
        },
        MediaItem {
            id: "v-demo-3".into(),
            title: "教程：OS 备份策略".into(),
            path: "/tank/media/video/backup-tutorial.mp4".into(),
            mime_type: "video/mp4".into(),
            size_bytes: 420_000_000,
            duration_secs: Some(1_500),
            thumbnail_url: Some("https://picsum.photos/seed/vid3/640/360".into()),
            created_at: "2025-03-20T09:00:00+08:00".into(),
            tags: vec!["教程".into()],
            demo: true,
        },
        // —— 音乐 ——
        MediaItem {
            id: "m-demo-1".into(),
            title: "夜的钢琴曲五".into(),
            path: "/tank/media/music/yedeqq.mp3".into(),
            mime_type: "audio/mpeg".into(),
            size_bytes: 8_500_000,
            duration_secs: Some(245),
            thumbnail_url: Some("https://picsum.photos/seed/mus1/300/300".into()),
            created_at: "2025-06-15T20:10:00+08:00".into(),
            tags: vec!["钢琴".into(), "轻音乐".into()],
            demo: true,
        },
        MediaItem {
            id: "m-demo-2".into(),
            title: "Canon in D".into(),
            path: "/tank/media/music/canon.flac".into(),
            mime_type: "audio/flac".into(),
            size_bytes: 32_000_000,
            duration_secs: Some(310),
            thumbnail_url: Some("https://picsum.photos/seed/mus2/300/300".into()),
            created_at: "2025-04-02T14:00:00+08:00".into(),
            tags: vec!["古典".into()],
            demo: true,
        },
        MediaItem {
            id: "m-demo-3".into(),
            title: "Summer Breeze".into(),
            path: "/tank/media/music/summer-breeze.m4a".into(),
            mime_type: "audio/mp4".into(),
            size_bytes: 6_200_000,
            duration_secs: Some(198),
            thumbnail_url: Some("https://picsum.photos/seed/mus3/300/300".into()),
            created_at: "2025-02-10T11:25:00+08:00".into(),
            tags: vec!["流行".into()],
            demo: true,
        },
        // —— 相册 ——
        MediaItem {
            id: "p-demo-1".into(),
            title: "海边日落".into(),
            path: "/tank/media/photo/sunset-001.jpg".into(),
            mime_type: "image/jpeg".into(),
            size_bytes: 4_800_000,
            duration_secs: None,
            thumbnail_url: Some("https://picsum.photos/seed/ph1/600/400".into()),
            created_at: "2025-07-30T18:45:00+08:00".into(),
            tags: vec!["风景".into(), "日落".into()],
            demo: true,
        },
        MediaItem {
            id: "p-demo-2".into(),
            title: "城市夜景".into(),
            path: "/tank/media/photo/city-night.png".into(),
            mime_type: "image/png".into(),
            size_bytes: 12_000_000,
            duration_secs: None,
            thumbnail_url: Some("https://picsum.photos/seed/ph2/600/400".into()),
            created_at: "2025-06-22T22:15:00+08:00".into(),
            tags: vec!["城市".into(), "夜景".into()],
            demo: true,
        },
        MediaItem {
            id: "p-demo-3".into(),
            title: "春日花开".into(),
            path: "/tank/media/photo/spring-flowers.jpg".into(),
            mime_type: "image/jpeg".into(),
            size_bytes: 3_500_000,
            duration_secs: None,
            thumbnail_url: Some("https://picsum.photos/seed/ph3/600/400".into()),
            created_at: "2025-04-05T09:30:00+08:00".into(),
            tags: vec!["自然".into(), "花".into()],
            demo: true,
        },
        MediaItem {
            id: "p-demo-4".into(),
            title: "雪山远眺".into(),
            path: "/tank/media/photo/mountain.jpg".into(),
            mime_type: "image/jpeg".into(),
            size_bytes: 7_900_000,
            duration_secs: None,
            thumbnail_url: Some("https://picsum.photos/seed/ph4/600/400".into()),
            created_at: "2025-01-18T13:00:00+08:00".into(),
            tags: vec!["风景".into(), "雪山".into()],
            demo: true,
        },
    ]
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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

    /// 构造一个 POST 请求（body 可空）。
    fn post_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Post,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    /// 构造一个唯一临时扫描根（含 `<tag>` 标识），保证并行测试互不干扰。
    fn temp_base(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let p =
            std::env::temp_dir().join(format!("os-media-test-{tag}-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// demo 数据 handler，scan_root 指向"不存在的路径"→ 真盘扫描恒为 0 → 强制 demo
    /// 回退。这样原 demo 断言不依赖生产机 /tank/media 的真实内容。DB 走内存库。
    fn demo_handler() -> MediaRouteHandler {
        let nowhere = std::env::temp_dir().join(format!("os-media-nowhere-{}", std::process::id()));
        MediaRouteHandler::new_for_test(demo_items(), Some(nowhere))
    }

    // —— routes() 声明 ——

    #[tokio::test]
    async fn routes_declares_all_endpoints() {
        let h = MediaRouteHandler::new();
        let routes = h.routes().await;
        // 4 原始 + 4 刮削/元数据 + 5 AI 相册 = 13
        assert_eq!(routes.len(), 13);
        assert!(routes.iter().all(|r| r.handler_component == "media"));
        let pairs: Vec<(HttpMethod, &str)> =
            routes.iter().map(|r| (r.method, r.path.as_str())).collect();
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/media/library")));
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/media/stats")));
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/media/item/:id")));
        assert!(pairs.contains(&(HttpMethod::Post, "/api/v1/media/scan")));
        // 刮削 4 条
        assert!(pairs.contains(&(HttpMethod::Post, "/api/v1/media/scrape")));
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/media/scrape/status")));
        assert!(pairs.contains(&(HttpMethod::Post, "/api/v1/media/scrape/all")));
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/media/metadata")));
        // AI 相册 5 条
        assert!(pairs.contains(&(HttpMethod::Post, "/api/v1/media/photo/analyze")));
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/media/photo/ai-metadata")));
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/media/photo/search")));
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/media/photo/categories")));
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/media/photo/analyze/:id")));
        // scan / scrape / scrape/all / photo/analyze 要求 admin；其余公开
        for path in &[
            "/api/v1/media/scan",
            "/api/v1/media/scrape",
            "/api/v1/media/scrape/all",
            "/api/v1/media/photo/analyze",
        ] {
            let r = routes
                .iter()
                .find(|r| r.method == HttpMethod::Post && &r.path == path)
                .unwrap();
            assert!(r.requires_auth, "{path} 应要求认证");
            assert_eq!(r.required_roles, vec!["admin".to_string()]);
        }
        // AI 相册只读端点公开
        for path in &[
            "/api/v1/media/photo/ai-metadata",
            "/api/v1/media/photo/search",
            "/api/v1/media/photo/categories",
            "/api/v1/media/photo/analyze/:id",
        ] {
            let r = routes.iter().find(|r| &r.path == path).unwrap();
            assert!(!r.requires_auth, "{path} 应公开");
        }
        let md = routes
            .iter()
            .find(|r| r.path == "/api/v1/media/metadata")
            .unwrap();
        assert!(!md.requires_auth);
    }

    // —— TMDB 刮削纯函数 ——

    #[test]
    fn extract_search_title_strips_year_resolution_ext() {
        assert_eq!(
            extract_search_title("Family.Trip.2025.1080p.mp4"),
            "Family Trip"
        );
        assert_eq!(
            extract_search_title("The.Matrix.1999.720p.BrRip.x264.mkv"),
            "The Matrix"
        );
        assert_eq!(extract_search_title("clip.mp4"), "clip");
        // 无扩展名 / 无年份
        assert_eq!(extract_search_title("Inception"), "Inception");
    }

    #[test]
    fn extract_search_title_preserves_chinese() {
        assert_eq!(extract_search_title("家庭旅行.mp4"), "家庭旅行");
        assert_eq!(
            extract_search_title("我和我的祖国.2019.1080p.mp4"),
            "我和我的祖国"
        );
        assert_eq!(extract_search_title("流浪地球2.mkv"), "流浪地球2");
    }

    #[test]
    fn build_tmdb_url_contains_key_query_language() {
        let url = build_tmdb_url("Family Trip", "SECRETKEY", "movie");
        assert!(url.contains("api_key=SECRETKEY"));
        assert!(url.contains("query=Family%20Trip"));
        assert!(url.contains("language=zh-CN"));
        assert!(url.starts_with("https://api.themoviedb.org/3/search/movie"));
        // tv 类型走 search/tv
        let tv = build_tmdb_url("Dark", "K", "tv");
        assert!(tv.contains("/search/tv?"));
    }

    #[test]
    fn id_for_path_is_deterministic() {
        let a = id_for_path("/tank/media/video/a.mp4");
        let b = id_for_path("/tank/media/video/a.mp4");
        assert_eq!(a, b);
        assert!(a.starts_with("media-"));
        assert_ne!(a, id_for_path("/tank/media/video/b.mp4"));
    }

    // —— SQLite 元数据 roundtrip ——

    #[tokio::test]
    async fn metadata_sqlite_roundtrip() {
        let h = MediaRouteHandler::new_for_test(Vec::new(), None);
        let m = MediaMetadata {
            id: id_for_path("/x/a.mp4"),
            file_path: "/x/a.mp4".into(),
            title: "测试电影".into(),
            overview: "剧情简介".into(),
            poster_url: "https://image.tmdb.org/t/p/w500/abc.jpg".into(),
            backdrop_url: String::new(),
            rating: 8.5,
            year: 2024,
            media_type: "movie".into(),
            tmdb_id: 12345,
            scraped_at: "2026-01-01T00:00:00+08:00".into(),
        };
        {
            let conn = h.db.lock().expect("db poisoned");
            upsert_metadata(&conn, &m).expect("upsert");
        }
        let resp = h.handle(get_req("/api/v1/media/metadata")).await.unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().expect("数组");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["title"], "测试电影");
        assert_eq!(arr[0]["rating"], 8.5);
        assert_eq!(arr[0]["year"], 2024);
        assert_eq!(
            arr[0]["poster_url"],
            "https://image.tmdb.org/t/p/w500/abc.jpg"
        );
        // upsert 同 id 不新增
        {
            let conn = h.db.lock().expect("db poisoned");
            upsert_metadata(&conn, &m).expect("upsert2");
        }
        let resp2 = h.handle(get_req("/api/v1/media/metadata")).await.unwrap();
        assert_eq!(resp2.body.as_array().unwrap().len(), 1);
    }

    // —— TMDB_API_KEY 未设降级不 panic ——

    #[tokio::test]
    async fn scrape_without_api_key_degrades() {
        // 注入空 api_key，走降级路径（不读 env，避免并行测试竞争）
        let h = MediaRouteHandler::new_for_test(Vec::new(), None);
        let outcome = h
            .scrape_with_key("/tank/media/video/x.mp4", "movie", "")
            .await;
        match outcome {
            ScrapeOutcome::Skipped(reason) => assert!(reason.contains("TMDB_API_KEY")),
            other => panic!("期望 Skipped，得到 {other:?}"),
        }
        // 经 HTTP 调用也应返回 skipped，不 panic
        let mut req = post_req("/api/v1/media/scrape");
        req.body = serde_json::json!({"file_path": "/tank/media/video/x.mp4"});
        let resp = h.handle(req).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["status"], "skipped");
    }

    #[tokio::test]
    async fn parse_tmdb_result_builds_poster_urls() {
        let item = serde_json::json!({
            "id": 680,
            "title": "Pulp Fiction",
            "overview": "剧情…",
            "vote_average": 8.5,
            "release_date": "1994-09-10",
            "poster_path": "/dM2W4Q…jpg",
            "backdrop_path": "/suaEO…jpg"
        });
        let m = parse_tmdb_result(&item, "/v/p.mkv", "movie");
        assert_eq!(m.title, "Pulp Fiction");
        assert_eq!(m.year, 1994);
        assert_eq!(m.tmdb_id, 680);
        assert!(m.poster_url.starts_with("https://image.tmdb.org/t/p/w500"));
        assert!(m
            .backdrop_url
            .starts_with("https://image.tmdb.org/t/p/w780"));
        assert_eq!(m.media_type, "movie");
    }

    // —— POST /api/v1/media/scan ——

    #[tokio::test]
    async fn scan_returns_started_status() {
        let h = MediaRouteHandler::new();
        let resp = h.handle(post_req("/api/v1/media/scan")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["status"], "started");
        assert!(resp.body["message"].as_str().unwrap().contains("扫描"));
    }

    // —— GET /api/v1/media/stats ——（demo 回退，scan_root 不命中真盘）

    #[tokio::test]
    async fn stats_returns_counts_and_total() {
        let h = demo_handler();
        let resp = h.handle(get_req("/api/v1/media/stats")).await.unwrap();
        assert_eq!(resp.status, 200);
        // demo: 3 video + 3 music + 4 photo
        assert_eq!(resp.body["video_count"], 3);
        assert_eq!(resp.body["music_count"], 3);
        assert_eq!(resp.body["photo_count"], 4);
        assert!(resp.body["total_size_bytes"].as_u64().unwrap() > 0);
    }

    // —— GET /api/v1/media/library?type= —— 按类型过滤（demo 回退）——

    #[tokio::test]
    async fn library_filters_by_type_video() {
        let h = demo_handler();
        let resp = h
            .handle(get_req("/api/v1/media/library?type=video"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().expect("body 为数组");
        assert_eq!(arr.len(), 3);
        assert!(arr
            .iter()
            .all(|i| i["mime_type"].as_str().unwrap().starts_with("video")));
        // demo 回退时全部 demo:true
        assert!(arr.iter().all(|i| i["demo"] == true));
    }

    #[tokio::test]
    async fn library_filters_by_type_music() {
        let h = demo_handler();
        let resp = h
            .handle(get_req("/api/v1/media/library?type=music"))
            .await
            .unwrap();
        let arr = resp.body.as_array().expect("body 为数组");
        assert_eq!(arr.len(), 3);
        assert!(arr
            .iter()
            .all(|i| i["mime_type"].as_str().unwrap().starts_with("audio")));
    }

    #[tokio::test]
    async fn library_filters_by_type_photo() {
        let h = demo_handler();
        let resp = h
            .handle(get_req("/api/v1/media/library?type=photo"))
            .await
            .unwrap();
        let arr = resp.body.as_array().expect("body 为数组");
        assert_eq!(arr.len(), 4);
        assert!(arr
            .iter()
            .all(|i| i["mime_type"].as_str().unwrap().starts_with("image")));
    }

    #[tokio::test]
    async fn library_no_type_returns_all() {
        let h = demo_handler();
        let resp = h.handle(get_req("/api/v1/media/library")).await.unwrap();
        let arr = resp.body.as_array().expect("body 为数组");
        assert_eq!(arr.len(), 10); // 3 + 3 + 4
    }

    // —— 真盘优先语义（核心新行为）——

    #[tokio::test]
    async fn library_real_files_override_demo() {
        // 临时根下放 1 个真实视频文件
        let base = temp_base("real");
        let vdir = base.join("video");
        std::fs::create_dir_all(&vdir).unwrap();
        std::fs::write(vdir.join("clip.mp4"), b"hello-world-payload").unwrap();
        let h = MediaRouteHandler::new_for_test(demo_items(), Some(base.clone()));
        let resp = h
            .handle(get_req("/api/v1/media/library?type=video"))
            .await
            .unwrap();
        let arr = resp.body.as_array().expect("body 为数组");
        // 真盘有文件 → 只返真盘那 1 条，绝不混 demo
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["demo"], false);
        assert_eq!(arr[0]["mime_type"], "video/mp4");
        assert_eq!(
            arr[0]["path"],
            vdir.join("clip.mp4").to_string_lossy().to_string()
        );
        assert!(arr.iter().all(|i| i["demo"] == false));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn library_empty_real_falls_back_to_demo() {
        // 临时根下 video 目录存在但为空 → 回退 demo
        let base = temp_base("empty");
        std::fs::create_dir_all(base.join("video")).unwrap();
        let h = MediaRouteHandler::new_for_test(demo_items(), Some(base.clone()));
        let resp = h
            .handle(get_req("/api/v1/media/library?type=video"))
            .await
            .unwrap();
        let arr = resp.body.as_array().expect("body 为数组");
        assert_eq!(arr.len(), 3); // demo 视频
        assert!(arr.iter().all(|i| i["demo"] == true));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn stats_prefers_real_files_over_demo() {
        // video 真盘 1 条 → 计 1；music / photo 真盘为空 → demo 计数（3 / 4）
        let base = temp_base("stats");
        std::fs::create_dir_all(base.join("video")).unwrap();
        std::fs::write(base.join("video").join("a.mp4"), b"abcd").unwrap();
        let h = MediaRouteHandler::new_for_test(demo_items(), Some(base.clone()));
        let resp = h.handle(get_req("/api/v1/media/stats")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["video_count"], 1); // 真盘
        assert_eq!(resp.body["music_count"], 3); // demo 回退
        assert_eq!(resp.body["photo_count"], 4); // demo 回退
        let _ = std::fs::remove_dir_all(&base);
    }

    // —— GET /api/v1/media/item/:id ——

    #[tokio::test]
    async fn item_detail_returns_stream_url() {
        let h = MediaRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/media/item/v-demo-1"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["id"], "v-demo-1");
        assert_eq!(resp.body["stream_url"], "/api/v1/media/stream/v-demo-1");
    }

    #[tokio::test]
    async fn item_detail_missing_returns_404() {
        let h = MediaRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/media/item/nope")).await.unwrap();
        assert_eq!(resp.status, 404);
        assert!(resp.body["error"].as_str().unwrap().contains("不存在"));
    }

    // —— 兜底 ——

    #[tokio::test]
    async fn unmatched_route_returns_404_body() {
        let h = MediaRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/media/unknown")).await.unwrap();
        assert_eq!(resp.status, 404);
        assert!(resp.body["error"].as_str().unwrap().contains("未匹配"));
    }

    // —— 辅助函数 ——

    #[test]
    fn path_segments_parses_correctly() {
        assert_eq!(
            path_segments("/api/v1/media/library?type=video"),
            vec!["api", "v1", "media", "library"]
        );
        assert_eq!(
            path_segments("/api/v1/media/item/abc"),
            vec!["api", "v1", "media", "item", "abc"]
        );
        assert!(path_segments("/").is_empty());
    }

    #[test]
    fn query_param_extracts_value() {
        assert_eq!(
            query_param("/api/v1/media/library?type=video", "type"),
            Some("video".into())
        );
        assert_eq!(
            query_param("/api/v1/media/library?type=photo&x=1", "type"),
            Some("photo".into())
        );
        assert_eq!(query_param("/api/v1/media/library", "type"), None);
        // URL 编码值
        assert_eq!(
            query_param("/api/v1/media/library?type=video%20clip", "type"),
            Some("video clip".into())
        );
    }

    #[test]
    fn media_type_serde_snake_case() {
        let v = serde_json::to_value(MediaType::Video).unwrap();
        assert_eq!(v, "video");
        let m: MediaType = serde_json::from_value(serde_json::json!("music")).unwrap();
        assert_eq!(m, MediaType::Music);
    }

    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<MediaRouteHandler>();
    }

    #[test]
    fn guess_mime_covers_common_extensions() {
        assert_eq!(guess_mime("a.mp4", MediaType::Video), "video/mp4");
        assert_eq!(guess_mime("a.mkv", MediaType::Video), "video/x-matroska");
        assert_eq!(guess_mime("a.mp3", MediaType::Music), "audio/mpeg");
        assert_eq!(guess_mime("a.flac", MediaType::Music), "audio/flac");
        assert_eq!(guess_mime("a.jpg", MediaType::Photo), "image/jpeg");
        assert_eq!(guess_mime("a.png", MediaType::Photo), "image/png");
        // 未知扩展名回退到该类型通用 MIME
        assert_eq!(guess_mime("a.xyz", MediaType::Video), "video/mp4");
        assert_eq!(guess_mime("noext", MediaType::Music), "audio/mpeg");
        assert_eq!(guess_mime("a.bin", MediaType::Photo), "image/jpeg");
    }

    // —— AI 相册：纯函数 ——

    #[test]
    fn build_photo_analyze_payload_contains_base64_and_model() {
        let payload = build_photo_analyze_payload("QkFTRTY0REFUQQ==");
        let s = serde_json::to_string(&payload).unwrap();
        assert!(s.contains("qwen3-vl-8b"), "缺模型名: {s}");
        assert!(s.contains("QkFTRTY0REFUQQ=="), "缺 base64 数据: {s}");
        assert!(
            payload["messages"][0]["content"][0]["image_url"]["url"]
                .as_str()
                .unwrap_or("")
                .starts_with("data:image/png;base64,"),
            "image_url 应为 data URL"
        );
    }

    #[test]
    fn extract_ai_json_handles_plain_json() {
        let raw = r#"{"description":"海边日落","tags":["海边","日落"],"scene":"landscape","has_people":false,"colors":["橙色"]}"#;
        let j = extract_ai_json(raw).expect("应提取到 JSON");
        let v: serde_json::Value = serde_json::from_str(&j).unwrap();
        assert_eq!(v["scene"], "landscape");
        assert_eq!(v["description"], "海边日落");
    }

    #[test]
    fn extract_ai_json_handles_code_fence() {
        // 带 ```json 包裹 + 前后噪声文本
        let raw = "好的，这是分析结果：\n```json\n{\"description\":\"猫咪\",\"scene\":\"animal\"}\n```\n以上。";
        let j = extract_ai_json(raw).expect("应从围栏提取 JSON");
        let v: serde_json::Value = serde_json::from_str(&j).unwrap();
        assert_eq!(v["description"], "猫咪");
        assert_eq!(v["scene"], "animal");
        // 普通无语言标签围栏
        let raw2 = "```\n{\"scene\":\"food\"}\n```";
        let j2 = extract_ai_json(raw2).expect("应从无标签围栏提取");
        let v2: serde_json::Value = serde_json::from_str(&j2).unwrap();
        assert_eq!(v2["scene"], "food");
    }

    #[test]
    fn extract_ai_json_returns_none_when_no_json() {
        assert!(extract_ai_json("这不是 JSON，没有花括号").is_none());
        assert!(extract_ai_json("").is_none());
        // 有花括号但不是合法 JSON → None
        assert!(extract_ai_json("{broken").is_none());
    }

    #[test]
    fn scene_to_cn_maps_correctly() {
        assert_eq!(scene_to_cn("landscape"), "风景");
        assert_eq!(scene_to_cn("portrait"), "人物");
        assert_eq!(scene_to_cn("food"), "食物");
        assert_eq!(scene_to_cn("architecture"), "建筑");
        assert_eq!(scene_to_cn("animal"), "动物");
        assert_eq!(scene_to_cn("other"), "其它");
        // 未知值兜底为"其它"；大小写/空白容忍
        assert_eq!(scene_to_cn("unknown"), "其它");
        assert_eq!(scene_to_cn("  LANDSCAPE "), "风景");
    }

    // —— AI 相册：SQLite roundtrip + 端点 ——

    #[tokio::test]
    async fn photo_ai_sqlite_roundtrip_search_categories() {
        let h = MediaRouteHandler::new_for_test(Vec::new(), None);
        let recs = vec![
            PhotoAi {
                file_path: "/tank/media/photo/sunset.jpg".into(),
                description: "海边日落，金色海面".into(),
                tags: vec!["海边".into(), "日落".into(), "风景".into()],
                scene: "landscape".into(),
                has_people: false,
                colors: vec!["橙色".into(), "蓝色".into()],
                analyzed_at: "2026-08-12T10:00:00+08:00".into(),
            },
            PhotoAi {
                file_path: "/tank/media/photo/cat.png".into(),
                description: "一只橘猫在睡觉".into(),
                tags: vec!["猫".into(), "动物".into()],
                scene: "animal".into(),
                has_people: false,
                colors: vec!["橙色".into()],
                analyzed_at: "2026-08-12T11:00:00+08:00".into(),
            },
        ];
        {
            let conn = h.db.lock().expect("db poisoned");
            for r in &recs {
                upsert_photo_ai(&conn, r).expect("upsert");
            }
        }
        // ai-metadata 全量
        let resp = h
            .handle(get_req("/api/v1/media/photo/ai-metadata"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().expect("数组");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["scene"], "animal"); // analyzed_at 降序 → cat 在前

        // search 命中"海边"
        let resp = h
            .handle(get_req("/api/v1/media/photo/search?q=%E6%B5%B7%E8%BE%B9"))
            .await
            .unwrap();
        let arr = resp.body.as_array().expect("数组");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["file_path"], "/tank/media/photo/sunset.jpg");

        // search 命中标签"猫"
        let resp = h
            .handle(get_req("/api/v1/media/photo/search?q=%E7%8C%AB"))
            .await
            .unwrap();
        assert_eq!(resp.body.as_array().unwrap().len(), 1);

        // categories 分组
        let resp = h
            .handle(get_req("/api/v1/media/photo/categories"))
            .await
            .unwrap();
        let arr = resp.body.as_array().expect("数组");
        assert_eq!(arr.len(), 2); // landscape + animal
        let total: u64 = arr.iter().map(|c| c["count"].as_u64().unwrap_or(0)).sum();
        assert_eq!(total, 2);

        // upsert 同 file_path 不新增
        {
            let conn = h.db.lock().expect("db poisoned");
            upsert_photo_ai(&conn, &recs[0]).expect("upsert2");
        }
        let resp = h
            .handle(get_req("/api/v1/media/photo/ai-metadata"))
            .await
            .unwrap();
        assert_eq!(resp.body.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn photo_analyze_single_degrades_without_panic() {
        // 单张分析：vLLM :8000 多半不在线（CI/本机未启），应降级 skipped，不 panic。
        // 指向一个不存在文件也无妨——探活在读文件之前，先 skipped。
        let h = MediaRouteHandler::new_for_test(Vec::new(), None);
        let mut req = post_req("/api/v1/media/photo/analyze");
        req.body = serde_json::json!({"file_path": "/tank/media/photo/nope.png"});
        let resp = h.handle(req).await.unwrap();
        assert_eq!(resp.status, 200);
        // 状态码 200；body.status ∈ {ok, skipped, failed}（不 panic 即通过）
        let st = resp.body["status"].as_str().unwrap_or("");
        assert!(
            matches!(st, "ok" | "skipped" | "failed"),
            "状态应可预测: {st}"
        );
        // 应记录一个任务 id（无论成功失败）
        assert!(resp.body["task_id"].is_string());
        let task_id = resp.body["task_id"].as_str().unwrap().to_string();
        // 任务状态端点可查（不 panic）
        let resp2 = h
            .handle(get_req(&format!("/api/v1/media/photo/analyze/{task_id}")))
            .await
            .unwrap();
        assert_eq!(resp2.status, 200);
        assert_eq!(resp2.body["id"], task_id);
    }

    #[tokio::test]
    async fn photo_analyze_task_missing_returns_404() {
        let h = MediaRouteHandler::new_for_test(Vec::new(), None);
        let resp = h
            .handle(get_req("/api/v1/media/photo/analyze/nonexistent-task"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
        assert!(resp.body["error"].as_str().unwrap().contains("不存在"));
    }

    #[tokio::test]
    async fn photo_ai_endpoints_empty_state_returns_200() {
        let h = MediaRouteHandler::new_for_test(Vec::new(), None);
        // 空库下三个只读端点都应 200 + 空数组，不 panic
        for path in &[
            "/api/v1/media/photo/ai-metadata",
            "/api/v1/media/photo/search?q=test",
            "/api/v1/media/photo/categories",
        ] {
            let resp = h.handle(get_req(path)).await.unwrap();
            assert_eq!(resp.status, 200, "{path}");
            assert!(resp.body.is_array(), "{path} 应返回数组");
            assert!(resp.body.as_array().unwrap().is_empty(), "{path} 应为空");
        }
    }
}
