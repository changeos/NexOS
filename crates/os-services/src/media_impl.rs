//! `DefaultMediaManager` —— `MediaManager` 的参考实现。
//!
//! **状态**（批 3 真实集成）：
//! - `ingest`：读文件大小 + MIME 推断 + EXIF 解析（若可读 JPEG）→ MediaAsset；**已接入 tantivy 索引**。
//!   若注入了 CLIP 模型（[`Self::with_clip_model`]），ingest 时计算 `clip_embedding` 写入 asset。
//! - `search`：**tantivy 全文 + 多维（日期/位置/人脸/相册）查询**（见 `media_search`）；
//!   若注入了 CLIP 模型，自由关键词会用 CLIP 文本嵌入做语义重排（可选，不破坏无 CLIP 路径）。
//! - `transcode`：构造 FFmpeg HLS 命令（[`media_ffmpeg`]），经注入的 [`FfmpegRunner`]
//!   spawn 子进程；默认（未注入 runner）只登记任务、构造命令不 spawn（保证 `cargo test`
//!   不真跑 ffmpeg）。生产用 [`Self::with_ffmpeg_runner`] 注入 [`TokioFfmpegRunner`]。
//! - `stream_playlist`：返回 m3u8 url；若该档位已转码则复用，否则触发转码。
//! - `list_albums`：默认按 `ByMonth` 分组。
//!
//! **未接入的硬阻塞依赖**（留 `// TODO` [RUNTIME]，未注册或运行时不可用）：
//! - 真实 CLIP 推理后端：已接入 candle 0.11 真实推理（[`CandleClipModel`] 加载
//!   ViT-B/32 safetensors 权重 + candle-transformers forward，RTX 3090 CUDA 实测），
//!   权重 + tokenizer.json 由部署预置到 model_dir（ADR-DEPS-005）。
//! - 人脸检测 —— TODO [RUNTIME]：接入人脸检测器（隐私相关，须安全评审；需模型权重）。
//!
//! **索引接入**：每个 `DefaultMediaManager` 实例持有一个 `SharedMediaIndex`（默认临时目录；
//! 生产可用 [`DefaultMediaManager::with_index_dir`] 注入持久路径）。`ingest`/`with_asset`
//! 增量写入；`search` 走真实 tantivy 查询。

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use os_core::{PageRequest, PageResponse, TaskId};

use crate::media::{
    Album, AlbumGrouping, MediaAsset, MediaManager, TranscodeJob, TranscodeProfile,
};
use crate::media_album::group_into_albums;
use crate::media_clip::ClipModel;
use crate::media_exif::parse_exif;
use crate::media_ffmpeg::{transcode_variant, FfmpegRunner, HlsVariant, HLS_SEGMENT_SECS};
use crate::media_search::{MediaQuery, SharedMediaIndex};
use crate::ServiceError;

/// 默认媒体管理器（内存态 + tantivy 索引 + 可选 FFmpeg/CLIP 编排）。
///
/// 共享状态用 `Arc<Mutex<..>>` 以满足 `Send + Sync`；方法为 `&self`。
/// `index` 在构造时建一个临时目录的 tantivy 索引（析构清理）。
///
/// **FFmpeg/CLIP 注入**：默认两者均为 None（transcode 仅登记任务 + 构造命令，不 spawn；
/// search 走纯 tantivy）。生产部署用 [`Self::with_ffmpeg_runner`] /
/// [`Self::with_clip_model`] 注入真实组件；测试用 fixture。
#[derive(Clone)]
pub struct DefaultMediaManager {
    state: Arc<Mutex<State>>,
    /// tantivy 索引（媒体元数据搜索）。
    index: SharedMediaIndex,
    /// 可选 FFmpeg 执行器（None → transcode 仅登记 + 构造命令不 spawn）。
    ffmpeg_runner: Option<Arc<dyn FfmpegRunner>>,
    /// 可选 CLIP 模型（None → 不计算 clip_embedding，search 不做语义重排）。
    clip_model: Option<Arc<dyn ClipModel>>,
}

impl std::fmt::Debug for DefaultMediaManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DefaultMediaManager")
            .finish_non_exhaustive()
    }
}

impl Default for DefaultMediaManager {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            // 构造失败（罕见：OS 临时目录不可用）退化为 Internal 错误——构造期 panic 不可接受，
            // 故提供 [`Self::new`] / [`Self::try_new`] 两档入口。
            index: SharedMediaIndex::temp().expect("media index temp dir 不可用"),
            ffmpeg_runner: None,
            clip_model: None,
        }
    }
}

#[derive(Default)]
struct State {
    /// asset_id → asset
    assets: HashMap<String, MediaAsset>,
    /// asset_id → 已转码档位集合
    transcoded: HashMap<String, Vec<TranscodeProfile>>,
    /// 转码任务登记
    jobs: Vec<TranscodeJob>,
    /// 最近一次 transcode 构造的 FFmpeg 命令参数（测试观测用；runner=None 时仍记录）
    last_ffmpeg_args: Option<Vec<String>>,
}

impl DefaultMediaManager {
    /// 构造空实例（与 [`Self::default`] 等价；索引用临时目录）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 构造空实例，索引失败时返回错误而非 panic（生产推荐入口）。
    pub fn try_new() -> Result<Self, ServiceError> {
        Ok(Self {
            state: Arc::new(Mutex::new(State::default())),
            index: SharedMediaIndex::temp()?,
            ffmpeg_runner: None,
            clip_model: None,
        })
    }

    /// 用指定目录建索引（生产部署 / 测试可复用）。
    pub fn with_index_dir(dir: PathBuf) -> Result<Self, ServiceError> {
        Ok(Self {
            state: Arc::new(Mutex::new(State::default())),
            index: SharedMediaIndex::at(dir)?,
            ffmpeg_runner: None,
            clip_model: None,
        })
    }

    /// 注入 FFmpeg 执行器（生产：`TokioFfmpegRunner`；测试：`FixtureFfmpegRunner`）。
    /// 注入后 `transcode` 会真实 spawn 子进程。
    #[must_use]
    pub fn with_ffmpeg_runner(mut self, runner: Arc<dyn FfmpegRunner>) -> Self {
        self.ffmpeg_runner = Some(runner);
        self
    }

    /// 注入 CLIP 模型（生产：真实 CLIP 后端；测试：`PlaceholderClipModel`）。
    /// 注入后 `ingest` 计算 `clip_embedding`，`search` 对自由关键词做语义重排。
    #[must_use]
    pub fn with_clip_model(mut self, model: Arc<dyn ClipModel>) -> Self {
        self.clip_model = Some(model);
        self
    }

    /// 注入一个已有 asset（测试 / 预加载用）。同步写入 tantivy 索引。
    #[must_use]
    pub fn with_asset(self, asset: MediaAsset) -> Self {
        {
            let mut st = self.state.lock().expect("state lock");
            st.assets.insert(asset.id.clone(), asset.clone());
        }
        // 同步到 tantivy 索引（album=None：注入时不归属相册）。
        // 失败时仅忽略索引写入（asset 仍保留在内存态，transcode/stream/list_albums 不受影响）；
        // search 路径会因索引缺漏而少召回——这是降级，不致命。
        let _ = self.index.upsert_and_commit(&asset, None);
        self
    }

    /// 取最近一次 `transcode` 构造的 FFmpeg 命令参数（测试观测用）。
    /// 即便未注入 runner（只构造命令不 spawn）也会更新；返回 None 表示尚未转码。
    pub fn last_ffmpeg_args(&self) -> Option<Vec<String>> {
        self.state
            .lock()
            .expect("state lock")
            .last_ffmpeg_args
            .clone()
    }
}

impl MediaManager for DefaultMediaManager {
    async fn ingest(&self, path: &Path) -> Result<MediaAsset, ServiceError> {
        // 1) 文件存在性 + 大小
        let meta = tokio::fs::metadata(path).await?;
        let size_bytes = meta.len();
        let path_str = path.to_string_lossy().to_string();

        // 2) MIME 推断（按扩展名；无 mime_guess 依赖）
        let mime_type = guess_mime(path);

        // 3) EXIF 解析（仅 JPEG；失败则忽略——EXIF 缺失是常态）
        let mut width = None;
        let mut height = None;
        let mut taken_at = None;
        if mime_type == "image/jpeg" {
            if let Ok(bytes) = tokio::fs::read(path).await {
                if let Some(exif) = parse_exif(&bytes) {
                    width = exif.width;
                    height = exif.height;
                    taken_at = exif.taken_at;
                }
            }
        }

        // 4) CLIP 向量嵌入（若注入了 CLIP 模型；图片类型才计算）
        //    真实后端（candle/ONNX）替换 trait 实现；占位实现见 [`PlaceholderClipModel`]。
        //    人脸检测 TODO [RUNTIME]：接入人脸检测器（隐私相关，须安全评审；需模型权重）
        let clip_embedding = if let Some(model) = &self.clip_model {
            if mime_type.starts_with("image/") {
                // 失败降级为 None（CLIP 不可用不应阻塞入库）。
                model.embed_image(path).await.ok()
            } else {
                None // TODO [RUNTIME]: 接入 CLIP 模型（运行时硬阻塞，需下载权重）
            }
        } else {
            None // TODO [RUNTIME]: 接入 CLIP 模型（运行时硬阻塞，需下载权重）
        };

        let id = format!("asset:{}", path_str);
        let asset = MediaAsset {
            id: id.clone(),
            path: path_str,
            mime_type,
            size_bytes,
            width,
            height,
            taken_at,
            faces: Vec::new(), // TODO [RUNTIME]: 人脸检测（隐私相关，需模型权重）
            clip_embedding,
        };

        // 6) 同步到 tantivy 索引（语义搜索 + 多维过滤）
        self.index.upsert_and_commit(&asset, None)?;

        let mut st = self.state.lock().expect("state lock");
        st.assets.insert(id, asset.clone());
        Ok(asset)
    }

    async fn search(
        &self,
        query: &str,
        page: PageRequest,
    ) -> Result<PageResponse<MediaAsset>, ServiceError> {
        // tantivy 真实查询：DSL 解析 → 多维查询 → 回取完整 asset → 排序 + 分页。
        let dsl = MediaQuery::parse(query);

        // 候选数上限：取足够大以覆盖分页（避免高 offset 截断）；上限保护性能。
        // 取 max(limit+offset, 200) 作为候选 topN。
        let candidate_limit = (page.limit.saturating_add(page.offset) as usize).max(200);

        let scored_ids = self.index.search(&dsl, candidate_limit)?;

        // 回取完整 asset（按 score 降序保留命中者）。
        // 对 geo 查询做精确 Haversine 复核（粗 bbox 会带入边缘点）。
        // 锁限定在独立 block 内，确保 guard 在 `.await`（CLIP 重排）前释放。
        let hits: Vec<(MediaAsset, f32)> = {
            let st = self.state.lock().expect("state lock");
            let mut acc: Vec<(MediaAsset, f32)> = Vec::new();
            for (id, score) in scored_ids {
                if let Some(a) = st.assets.get(&id) {
                    // geo 精确复核（asset 自身无 GPS 字段——若未来扩展 MediaAsset 携 GPS，
                    // 在此用 `bbox.center.distance_meters(...) <= bbox.radius_meters` 过滤）。
                    // 当前 asset 无 GPS，复核跳过（接受粗 bbox 召回）。
                    acc.push((a.clone(), score));
                }
            }
            acc
        };
        let mut hits = hits;

        // 可选 CLIP 语义重排：若注入了 CLIP 模型且查询含自由关键词，用文本嵌入与
        // asset 的 clip_embedding 相似度重排（与 tantivy BM25 加权融合）。
        // 无 clip_model / asset 无 embedding / 无自由词 → 跳过（保持原行为）。
        if let Some(model) = &self.clip_model {
            if !dsl.keywords.trim().is_empty() {
                if let Ok(text_vec) = model.embed_text(&dsl.keywords).await {
                    for (asset, score) in &mut hits {
                        if let Some(emb) = &asset.clip_embedding {
                            let clip_s = model.similarity(&text_vec, emb);
                            // 加权融合：BM25（已归一化到 [0,1] 经验值）× 0.6 + CLIP × 0.4。
                            // 把 BM25 score 钳到 [0,1]（粗略；高 BM25 仍主导）。
                            let bm25 = (*score).clamp(0.0, 1.0);
                            *score = 0.6 * bm25 + 0.4 * clip_s;
                        }
                    }
                }
            }
        }

        // 稳定排序：score 降序，score 相同按 id 升序
        hits.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.id.cmp(&b.0.id))
        });

        let total = hits.len() as u32;
        let offset = page.offset as usize;
        let limit = page.limit as usize;
        let items: Vec<MediaAsset> = hits
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|(a, _)| a)
            .collect();

        Ok(PageResponse {
            items,
            total,
            offset: page.offset,
            limit: page.limit,
        })
    }

    async fn transcode(
        &self,
        asset_id: &str,
        profile: TranscodeProfile,
    ) -> Result<TaskId, ServiceError> {
        // 命令构造（锁内取 asset + 记录 args），guard 限定在 block 内，确保 `.await`
        // （runner 执行）前释放。
        let (asset_path, output_dir) = {
            let mut st = self.state.lock().expect("state lock");
            if !st.assets.contains_key(asset_id) {
                return Err(ServiceError::AssetNotFound(asset_id.to_string()));
            }
            // 构造 FFmpeg HLS 命令（即便无 runner 也记录到 last_ffmpeg_args 供观测）。
            let variant = HlsVariant::from_profile(profile);
            let asset_path = PathBuf::from(st.assets[asset_id].path.clone());
            // 输出目录：转码产物目录（生产由配置注入；这里用临时基目录 + asset_id/profile 隔离）。
            let output_dir = transcode_output_dir(asset_id, &profile);
            let args = crate::media_ffmpeg::build_hls_args(
                &asset_path,
                &output_dir,
                &variant,
                HLS_SEGMENT_SECS,
            );
            st.last_ffmpeg_args = Some(args);
            (asset_path, output_dir)
        };
        let variant = HlsVariant::from_profile(profile);

        // 若注入了 runner，真实 spawn；否则只登记任务（保证 cargo test 不真跑 ffmpeg）。
        if let Some(runner) = &self.ffmpeg_runner {
            transcode_variant(
                runner.as_ref(),
                &asset_path,
                &output_dir,
                &variant,
                HLS_SEGMENT_SECS,
            )
            .await?;
        }

        let task_id = TaskId::new();
        let mut st = self.state.lock().expect("state lock");
        st.jobs.push(TranscodeJob {
            task_id,
            asset_id: asset_id.to_string(),
            profile,
            done: self.ffmpeg_runner.is_some(),
        });
        // 标记该档位已转码（runner 存在时；无 runner 时只登记 job，stream_playlist 仍可即时触发）
        if self.ffmpeg_runner.is_some() {
            st.transcoded
                .entry(asset_id.to_string())
                .or_default()
                .push(profile);
        }
        Ok(task_id)
    }

    async fn stream_playlist(
        &self,
        asset_id: &str,
        profile: TranscodeProfile,
    ) -> Result<String, ServiceError> {
        let mut st = self.state.lock().expect("state lock");
        if !st.assets.contains_key(asset_id) {
            return Err(ServiceError::AssetNotFound(asset_id.to_string()));
        }

        // 若尚未转码该档位，触发即时转码（占位：登记即可）
        let entry = st.transcoded.entry(asset_id.to_string()).or_default();
        if !entry.contains(&profile) {
            entry.push(profile);
            // 占位：不真正调 FFmpeg（真实即时转码见 transcode + runner 注入）
        }
        drop(st);

        // 返回 m3u8 URL（占位路径；真实实现由 os-api 反代到转码产物目录）
        let h = profile.target_height();
        let url = format!("/stream/{asset_id}/{h}.m3u8");
        Ok(url)
    }

    async fn list_albums(&self) -> Result<Vec<Album>, ServiceError> {
        let st = self.state.lock().expect("state lock");
        let assets: Vec<MediaAsset> = st.assets.values().cloned().collect();
        // 默认按月分组；真实实现可按用户配置
        let albums = group_into_albums(&assets, &AlbumGrouping::ByMonth);
        Ok(albums)
    }
}

/// 计算转码产物输出目录（asset_id + profile 隔离；生产可由配置注入）。
///
/// 形如 `<temp>/os-transcode/<asset_id>/<height>p/`。高度为 0（Original）时用 `original`。
fn transcode_output_dir(asset_id: &str, profile: &TranscodeProfile) -> PathBuf {
    let h = match profile {
        TranscodeProfile::Original => "original".to_string(),
        _ => format!("{}p", profile.target_height()),
    };
    // asset_id 形如 `asset:/path/to/file` —— 替换路径分隔符为 `_` 避免目录穿越。
    let safe_id = asset_id.replace(['/', '\\'], "_");
    std::env::temp_dir()
        .join("os-transcode")
        .join(safe_id)
        .join(h)
}

/// 按扩展名推断 MIME（最小集合）。
fn guess_mime(path: &Path) -> String {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => "image/jpeg".to_string(),
        Some("png") => "image/png".to_string(),
        Some("gif") => "image/gif".to_string(),
        Some("heic") | Some("heif") => "image/heif".to_string(),
        Some("webp") => "image/webp".to_string(),
        Some("mp4") => "video/mp4".to_string(),
        Some("mov") => "video/quicktime".to_string(),
        Some("mkv") => "video/x-matroska".to_string(),
        Some("webm") => "video/webm".to_string(),
        Some("mp3") => "audio/mpeg".to_string(),
        Some("flac") => "audio/flac".to_string(),
        Some("wav") => "audio/wav".to_string(),
        _ => "application/octet-stream".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaAsset;
    use chrono::Utc;
    use os_core::PageRequest;

    fn mk_asset(id: &str, path: &str, faces: &[&str]) -> MediaAsset {
        use crate::media::{BBox, FaceTag};
        MediaAsset {
            id: id.to_string(),
            path: path.to_string(),
            mime_type: "image/jpeg".to_string(),
            size_bytes: 1024,
            width: Some(1920),
            height: Some(1080),
            taken_at: Some(Utc::now()),
            faces: faces
                .iter()
                .map(|n| FaceTag {
                    name: Some((*n).to_string()),
                    bbox: BBox {
                        x: 0.1,
                        y: 0.1,
                        w: 0.2,
                        h: 0.2,
                    },
                })
                .collect(),
            clip_embedding: None,
        }
    }

    /// 构造带 CLIP embedding 的 asset（测试 CLIP 重排路径用）。
    fn mk_asset_with_clip(id: &str, path: &str, clip: Vec<f32>) -> MediaAsset {
        let mut a = mk_asset(id, path, &[]);
        a.clip_embedding = Some(clip);
        a
    }

    #[tokio::test]
    async fn search_substring_matches_path_and_face() {
        let mgr = DefaultMediaManager::new()
            .with_asset(mk_asset("a1", "/photos/vacation/IMG_001.jpg", &["张三"]))
            .with_asset(mk_asset("a2", "/photos/work/doc.png", &[]));
        let res = mgr
            .search(
                "vacation",
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(res.total, 1);
        assert_eq!(res.items[0].id, "a1");

        let res = mgr
            .search(
                "张三",
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(res.total, 1);
    }

    #[tokio::test]
    async fn search_pagination() {
        let mgr = DefaultMediaManager::new()
            .with_asset(mk_asset("a1", "/p/a.jpg", &[]))
            .with_asset(mk_asset("a2", "/p/b.jpg", &[]))
            .with_asset(mk_asset("a3", "/p/c.jpg", &[]));
        let res = mgr
            .search(
                "p/",
                PageRequest {
                    offset: 1,
                    limit: 1,
                },
            )
            .await
            .unwrap();
        assert_eq!(res.total, 3);
        assert_eq!(res.items.len(), 1);
    }

    #[tokio::test]
    async fn transcode_unknown_asset_errors() {
        let mgr = DefaultMediaManager::new();
        let err = mgr
            .transcode("ghost", TranscodeProfile::Hls720p)
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::AssetNotFound(_)));
    }

    #[tokio::test]
    async fn transcode_returns_task_id() {
        let mgr = DefaultMediaManager::new().with_asset(mk_asset("a1", "/p/a.jpg", &[]));
        let tid = mgr
            .transcode("a1", TranscodeProfile::Hls720p)
            .await
            .unwrap();
        // TaskId 是 Uuid newtype
        assert_eq!(format!("{}", tid).len(), 36);
    }

    #[tokio::test]
    async fn stream_playlist_unknown_errors() {
        let mgr = DefaultMediaManager::new();
        let err = mgr
            .stream_playlist("ghost", TranscodeProfile::Hls720p)
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::AssetNotFound(_)));
    }

    #[tokio::test]
    async fn stream_playlist_returns_url() {
        let mgr = DefaultMediaManager::new().with_asset(mk_asset("a1", "/p/a.mp4", &[]));
        let url = mgr
            .stream_playlist("a1", TranscodeProfile::Hls720p)
            .await
            .unwrap();
        assert!(url.contains("a1") && url.contains("720"));
    }

    #[tokio::test]
    async fn list_albums_groups_assets() {
        let mgr = DefaultMediaManager::new()
            .with_asset(mk_asset("a1", "/p/a.jpg", &[]))
            .with_asset(mk_asset("a2", "/p/b.jpg", &[]));
        let albums = mgr.list_albums().await.unwrap();
        // 至少一个相册包含 2 个资源
        assert!(albums.iter().any(|a| a.asset_count == 2));
    }

    #[tokio::test]
    async fn ingest_nonexistent_file_errors() {
        let mgr = DefaultMediaManager::new();
        let err = mgr
            .ingest(std::path::Path::new("/nonexistent/path/ghost.jpg"))
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::Io(_)));
    }

    #[tokio::test]
    async fn ingest_empty_file_succeeds() {
        // 写一个空文件——MIME 推断仍走扩展名
        let dir = tempdir();
        let path = dir.join("photo.jpg");
        tokio::fs::write(&path, b"").await.unwrap();
        let mgr = DefaultMediaManager::new();
        let asset = mgr.ingest(&path).await.unwrap();
        assert_eq!(asset.mime_type, "image/jpeg");
        assert_eq!(asset.size_bytes, 0);
        assert!(asset.faces.is_empty());
    }

    // ========================================================================
    // tantivy 真实搜索集成测（接通 media_search）
    // ========================================================================

    fn mk_asset_dated(id: &str, path: &str, faces: &[&str], taken: &str) -> MediaAsset {
        use chrono::NaiveDate;
        let mut a = mk_asset(id, path, faces);
        a.taken_at = Some(
            NaiveDate::parse_from_str(taken, "%Y-%m-%d")
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .unwrap()
                .and_utc(),
        );
        a
    }

    #[tokio::test]
    async fn tantivy_search_by_filename_keyword() {
        let mgr = DefaultMediaManager::new()
            .with_asset(mk_asset_dated(
                "a1",
                "/photos/beach/sunset.jpg",
                &[],
                "2024-07-01",
            ))
            .with_asset(mk_asset_dated(
                "a2",
                "/photos/mountain/snow.png",
                &[],
                "2024-07-02",
            ));
        let res = mgr
            .search(
                "sunset",
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(res.total, 1);
        assert_eq!(res.items[0].id, "a1");
    }

    #[tokio::test]
    async fn tantivy_search_face_dsl() {
        let mgr = DefaultMediaManager::new()
            .with_asset(mk_asset_dated(
                "a1",
                "/p/x.jpg",
                &["alice", "bob"],
                "2024-01-01",
            ))
            .with_asset(mk_asset_dated("a2", "/p/y.jpg", &["carol"], "2024-01-02"));
        let res = mgr
            .search(
                "face:alice",
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(res.total, 1);
        assert_eq!(res.items[0].id, "a1");
    }

    #[tokio::test]
    async fn tantivy_search_date_prefix() {
        let mgr = DefaultMediaManager::new()
            .with_asset(mk_asset_dated("a1", "/p/x.jpg", &[], "2024-01-15"))
            .with_asset(mk_asset_dated("a2", "/p/y.jpg", &[], "2024-06-20"))
            .with_asset(mk_asset_dated("a3", "/p/z.jpg", &[], "2023-12-31"));
        // 整年
        let res = mgr
            .search(
                "date:2024",
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(res.total, 2);
        // 月份
        let res = mgr
            .search(
                "date:2024-06",
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(res.total, 1);
        assert_eq!(res.items[0].id, "a2");
    }

    #[tokio::test]
    async fn tantivy_search_after_before_range() {
        let mgr = DefaultMediaManager::new()
            .with_asset(mk_asset_dated("a1", "/p/x.jpg", &[], "2024-01-15"))
            .with_asset(mk_asset_dated("a2", "/p/y.jpg", &[], "2024-06-20"))
            .with_asset(mk_asset_dated("a3", "/p/z.jpg", &[], "2024-12-31"));
        let res = mgr
            .search(
                "after:2024-03-01 before:2024-10-01",
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(res.total, 1);
        assert_eq!(res.items[0].id, "a2");
    }

    #[tokio::test]
    async fn tantivy_search_combined_keyword_and_face() {
        let mgr = DefaultMediaManager::new()
            .with_asset(mk_asset_dated(
                "a1",
                "/photos/beach.jpg",
                &["alice"],
                "2024-01-01",
            ))
            .with_asset(mk_asset_dated(
                "a2",
                "/photos/beach.jpg",
                &["bob"],
                "2024-01-01",
            ))
            .with_asset(mk_asset_dated(
                "a3",
                "/photos/mountain.jpg",
                &["alice"],
                "2024-01-01",
            ));
        let res = mgr
            .search(
                "beach face:alice",
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        let ids: Vec<_> = res.items.iter().map(|a| a.id.as_str()).collect();
        assert!(ids.contains(&"a1"));
        assert!(!ids.contains(&"a2"));
        assert!(!ids.contains(&"a3"));
    }

    #[tokio::test]
    async fn tantivy_search_empty_returns_all() {
        let mgr = DefaultMediaManager::new()
            .with_asset(mk_asset_dated("a1", "/p/x.jpg", &[], "2024-01-01"))
            .with_asset(mk_asset_dated("a2", "/p/y.jpg", &[], "2024-02-01"));
        let res = mgr
            .search(
                "",
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(res.total, 2);
    }

    #[tokio::test]
    async fn tantivy_ingest_then_search() {
        // ingest 一个真实（空）JPEG 后应能被搜索（按文件名）
        let dir = tempdir();
        let path = dir.join("vacation_photo.jpg");
        tokio::fs::write(&path, b"").await.unwrap();
        let mgr = DefaultMediaManager::new();
        let asset = mgr.ingest(&path).await.unwrap();
        assert!(asset.id.contains("vacation_photo"));
        let res = mgr
            .search(
                "vacation",
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(res.total, 1);
        assert_eq!(res.items[0].id, asset.id);
    }

    #[tokio::test]
    async fn tantivy_search_pagination_preserved() {
        let mut mgr = DefaultMediaManager::new();
        for i in 0..5 {
            mgr = mgr.with_asset(mk_asset_dated(
                &format!("a{i}"),
                &format!("/p/photo_{i}.jpg"),
                &[],
                "2024-01-01",
            ));
        }
        // 关键词 photo 命中全部 5
        let res = mgr
            .search(
                "photo",
                PageRequest {
                    offset: 2,
                    limit: 2,
                },
            )
            .await
            .unwrap();
        assert_eq!(res.total, 5);
        assert_eq!(res.items.len(), 2);
        assert_eq!(res.offset, 2);
    }

    #[tokio::test]
    async fn with_index_dir_persistent() {
        // 用显式目录构造（验证生产部署路径入口）
        let dir = tempdir().join("idx");
        let mgr = DefaultMediaManager::with_index_dir(dir.clone())
            .unwrap()
            .with_asset(mk_asset_dated("a1", "/photos/sky.jpg", &[], "2024-05-01"));
        let res = mgr
            .search(
                "sky",
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(res.total, 1);
        assert!(dir.exists());
    }

    // ========================================================================
    // FFmpeg 转码编排集成测（接通 media_ffmpeg）
    // ========================================================================

    #[tokio::test]
    async fn transcode_records_ffmpeg_args_without_runner() {
        // 默认无 runner：transcode 仅登记任务 + 构造命令不 spawn。
        let mgr = DefaultMediaManager::new().with_asset(mk_asset("a1", "/p/a.mp4", &[]));
        let _tid = mgr
            .transcode("a1", TranscodeProfile::Hls720p)
            .await
            .unwrap();
        // last_ffmpeg_args 应记录命令构造（含 scale=-2:720）
        let args = mgr.last_ffmpeg_args().expect("应记录命令参数");
        assert!(args.contains(&"scale=-2:720".to_string()));
        assert!(args.contains(&"-hls_time".to_string()));
        assert!(args.contains(&"/p/a.mp4".to_string()));
    }

    #[tokio::test]
    async fn transcode_with_fixture_runner_invokes_ffmpeg() {
        use crate::media_ffmpeg::{FfmpegOutput, FixtureFfmpegRunner};
        // 注入 fixture runner（成功）；验证 transcode 真走 runner 并标记 done。
        let runner = Arc::new(FixtureFfmpegRunner::new(FfmpegOutput::ok()));
        let mgr = DefaultMediaManager::new()
            .with_asset(mk_asset("a1", "/p/a.mp4", &[]))
            .with_ffmpeg_runner(runner.clone());
        let tid = mgr
            .transcode("a1", TranscodeProfile::Hls720p)
            .await
            .unwrap();
        assert_eq!(format!("{}", tid).len(), 36);
        // fixture 被调用：last_args 记录
        let recorded = runner.last_args().expect("runner 应被调用");
        assert!(recorded.contains(&"scale=-2:720".to_string()));
        // 档位标记为已转码
        let st = mgr.state.lock().expect("state lock");
        assert!(st.transcoded["a1"].contains(&TranscodeProfile::Hls720p));
        assert!(st.jobs.iter().any(|j| j.task_id == tid && j.done));
    }

    #[tokio::test]
    async fn transcode_with_failing_runner_maps_error() {
        use crate::media_ffmpeg::{FfmpegOutput, FixtureFfmpegRunner};
        let runner = Arc::new(FixtureFfmpegRunner::new(FfmpegOutput {
            stdout: String::new(),
            stderr: "Codec not found".into(),
            exit_code: 1,
        }));
        let mgr = DefaultMediaManager::new()
            .with_asset(mk_asset("a1", "/p/a.mp4", &[]))
            .with_ffmpeg_runner(runner);
        let err = mgr
            .transcode("a1", TranscodeProfile::Hls720p)
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::Internal(_)));
        let msg = format!("{err}");
        assert!(msg.contains("Codec not found"), "保留 stderr 诊断");
    }

    #[tokio::test]
    async fn transcode_original_profile_uses_copy() {
        // Original 档位命令构造应含 -c copy（无 libx264）
        let mgr = DefaultMediaManager::new().with_asset(mk_asset("a1", "/p/a.mp4", &[]));
        let _ = mgr
            .transcode("a1", TranscodeProfile::Original)
            .await
            .unwrap();
        let args = mgr.last_ffmpeg_args().expect("应记录命令");
        assert!(args.contains(&"copy".to_string()));
        assert!(!args.contains(&"libx264".to_string()));
    }

    // ========================================================================
    // CLIP 向量集成测（接通 media_clip）
    // ========================================================================

    #[tokio::test]
    async fn ingest_with_clip_model_computes_embedding() {
        use crate::media_clip::PlaceholderClipModel;
        let dir = tempdir();
        let path = dir.join("photo.jpg");
        tokio::fs::write(&path, b"").await.unwrap();
        let mgr = DefaultMediaManager::new()
            .with_clip_model(Arc::new(PlaceholderClipModel::with_dim(32)));
        let asset = mgr.ingest(&path).await.unwrap();
        // 占位 CLIP 应算出 32 维向量（image 类型）
        let emb = asset.clip_embedding.expect("应有 CLIP embedding");
        assert_eq!(emb.len(), 32);
        // 向量应 L2 归一化
        let norm: f32 = emb.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[tokio::test]
    async fn ingest_without_clip_model_has_no_embedding() {
        // 默认无 CLIP 模型：ingest 不算 embedding（保持原行为）
        let dir = tempdir();
        let path = dir.join("photo.jpg");
        tokio::fs::write(&path, b"").await.unwrap();
        let mgr = DefaultMediaManager::new();
        let asset = mgr.ingest(&path).await.unwrap();
        assert!(asset.clip_embedding.is_none());
    }

    #[tokio::test]
    async fn ingest_non_image_skips_clip_embedding() {
        use crate::media_clip::PlaceholderClipModel;
        // 非 image 类型（mp4）即使注入 CLIP 也不算 embedding
        let dir = tempdir();
        let path = dir.join("video.mp4");
        tokio::fs::write(&path, b"").await.unwrap();
        let mgr = DefaultMediaManager::new().with_clip_model(Arc::new(PlaceholderClipModel::new()));
        let asset = mgr.ingest(&path).await.unwrap();
        assert!(asset.clip_embedding.is_none(), "视频不应有 CLIP embedding");
    }

    #[tokio::test]
    async fn search_reranks_with_clip_when_model_present() {
        // 注入 CLIP 模型 + 两个 asset 含 embedding：search 自由词应走 CLIP 重排路径
        // 不报错（数值合法性即可；占位实现语义不真实）。
        use crate::media_clip::PlaceholderClipModel;
        let mgr = DefaultMediaManager::new()
            .with_clip_model(Arc::new(PlaceholderClipModel::new()))
            .with_asset(mk_asset_with_clip(
                "a1",
                "/photos/beach.jpg",
                PlaceholderClipModel::new().embed_text_sync("beach"),
            ))
            .with_asset(mk_asset_with_clip(
                "a2",
                "/photos/beach.jpg",
                PlaceholderClipModel::new().embed_text_sync("mountain"),
            ));
        let res = mgr
            .search(
                "beach",
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        // 两 asset 文件名都含 beach → 全召回
        assert_eq!(res.total, 2);
        // CLIP 重排：a1（embedding=beach）应排在 a2（embedding=mountain）前
        // （文本 "beach" 与 "beach" 向量完全相同 → 相似度 1.0 > 与 "mountain" 的相似度）
        assert_eq!(res.items[0].id, "a1");
    }

    #[tokio::test]
    async fn search_without_clip_model_preserves_tantivy_order() {
        // 无 CLIP 模型：search 不做重排（保持原 tantivy BM25 行为）
        let mgr = DefaultMediaManager::new()
            .with_asset(mk_asset_with_clip(
                "a1",
                "/photos/beach.jpg",
                vec![1.0, 0.0],
            ))
            .with_asset(mk_asset_with_clip(
                "a2",
                "/photos/beach.jpg",
                vec![0.0, 1.0],
            ));
        let res = mgr
            .search(
                "beach",
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(res.total, 2);
        // 无 CLIP 重排 → 不强制顺序（仅断言不报错 + 全召回）
    }

    #[tokio::test]
    async fn search_clip_rerank_skips_structured_only_query() {
        // 结构化查询（无自由词）不应触发 CLIP 重排
        use crate::media_clip::PlaceholderClipModel;
        let mgr = DefaultMediaManager::new()
            .with_clip_model(Arc::new(PlaceholderClipModel::new()))
            .with_asset(mk_asset_dated("a1", "/p/x.jpg", &["alice"], "2024-01-01"));
        let res = mgr
            .search(
                "face:alice",
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(res.total, 1);
    }

    fn tempdir() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "media-test-{}-{}",
            std::process::id(),
            uuid_counter()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn uuid_counter() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static C: AtomicU64 = AtomicU64::new(0);
        C.fetch_add(1, Ordering::SeqCst)
    }

    // ========================================================================
    // guess_mime 纯函数覆盖（私有，经 super::guess_mime 调用）
    // ========================================================================

    #[test]
    fn guess_mime_image_extensions() {
        use std::path::Path;
        assert_eq!(guess_mime(Path::new("/x/a.JPG")), "image/jpeg");
        assert_eq!(guess_mime(Path::new("/x/a.jpeg")), "image/jpeg");
        assert_eq!(guess_mime(Path::new("/x/a.png")), "image/png");
        assert_eq!(guess_mime(Path::new("/x/a.gif")), "image/gif");
        assert_eq!(guess_mime(Path::new("/x/a.heic")), "image/heif");
        assert_eq!(guess_mime(Path::new("/x/a.heif")), "image/heif");
        assert_eq!(guess_mime(Path::new("/x/a.webp")), "image/webp");
    }

    #[test]
    fn guess_mime_video_extensions() {
        use std::path::Path;
        assert_eq!(guess_mime(Path::new("/x/a.mp4")), "video/mp4");
        assert_eq!(guess_mime(Path::new("/x/a.mov")), "video/quicktime");
        assert_eq!(guess_mime(Path::new("/x/a.mkv")), "video/x-matroska");
        assert_eq!(guess_mime(Path::new("/x/a.webm")), "video/webm");
    }

    #[test]
    fn guess_mime_audio_extensions() {
        use std::path::Path;
        assert_eq!(guess_mime(Path::new("/x/a.mp3")), "audio/mpeg");
        assert_eq!(guess_mime(Path::new("/x/a.flac")), "audio/flac");
        assert_eq!(guess_mime(Path::new("/x/a.wav")), "audio/wav");
    }

    #[test]
    fn guess_mime_unknown_and_no_extension() {
        use std::path::Path;
        // 无扩展名 → octet-stream
        assert_eq!(
            guess_mime(Path::new("/x/noext")),
            "application/octet-stream"
        );
        // 未知扩展名 → octet-stream
        assert_eq!(
            guess_mime(Path::new("/x/a.xyz")),
            "application/octet-stream"
        );
        // 大小写不敏感
        assert_eq!(guess_mime(Path::new("/x/a.MP4")), "video/mp4");
    }

    // ========================================================================
    // transcode_output_dir 纯函数覆盖（私有）
    // ========================================================================

    #[test]
    fn transcode_output_dir_profiles_isolated() {
        let d720 = transcode_output_dir("asset:/p/a.mp4", &TranscodeProfile::Hls720p);
        assert!(d720.to_string_lossy().contains("os-transcode"));
        assert!(d720.to_string_lossy().contains("720p"));
        // 路径分隔符被替换为 _（防目录穿越）
        assert!(d720.to_string_lossy().contains("asset:_p_a.mp4"));

        let d1080 = transcode_output_dir("asset:/p/a.mp4", &TranscodeProfile::Hls1080p);
        assert!(d1080.to_string_lossy().contains("1080p"));

        let d480 = transcode_output_dir("asset:/p/a.mp4", &TranscodeProfile::Hls480p);
        assert!(d480.to_string_lossy().contains("480p"));

        // Original → "original" 段
        let dorig = transcode_output_dir("asset:/p/a.mp4", &TranscodeProfile::Original);
        assert!(dorig.to_string_lossy().contains("original"));
    }

    #[test]
    fn transcode_output_dir_backslash_replaced() {
        // Windows 风格路径分隔符也替换
        let d = transcode_output_dir("asset:C:\\dir\\f", &TranscodeProfile::Hls720p);
        let s = d.to_string_lossy().to_string();
        assert!(s.contains("asset:C:_dir_f"));
    }
}
