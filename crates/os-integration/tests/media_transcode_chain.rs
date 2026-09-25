//! 场景 11：媒体转码链路（integration-agent 规格书 §3 扩展场景）
//!
//! 链路：用户上传文件 → os-services 文件管理接收 → media_ffmpeg 转码编排骨架
//! → CLIP 向量生成（mock）→ tantivy 索引建/查。
//!
//! 之所以这样组织：
//! - OS 媒体管线天然跨多个子组件：文件落盘（storage）/ 转码（media_ffmpeg）/ 语义
//!   嵌入（media_clip）/ 全文索引（media_search / tantivy）/ 事件（EventBus）。
//!   各子组件单独已有单测，但「上传 → 转码 → 嵌入 → 索引 → 搜索」端到端链路尚未
//!   有集成测覆盖。本场景在测侧搭一层 `MediaTranscodePipeline` 编排骨架，把这些
//!   组件显式串通，验证链路完整 + 跨 crate 类型一致（呼应 backup_chain 场景的
//!   `BackupPipeline` 编排层风格，不改 trait / crate 源码）。
//! - 用全 mock 后端：`MockStorageBackend`（文件落盘 mock，snapshot 模拟持久化）、
//!   `FixtureFfmpegRunner`（ffmpeg 子进程 mock，返回 fixture 输出）、
//!   `PlaceholderClipModel`（CLIP 推理 mock，确定性哈希派生向量）、
//!   `SharedMediaIndex`（真实 tantivy RAM 索引，不依赖外部环境）、
//!   `MockEventBus`（事件总线 mock，记录发布事件供断言）。
//!
//! 重点验证：
//! - 完整链路：receive_file → transcode_abr → embed_clip → index_upsert → search
//!   全链通（含调用顺序断言）。
//! - 跨 crate 类型桥接：`MediaAsset.path`（os-services）经 storage mock 落盘后
//!   再喂给 ffmpeg 编排（`HlsVariant`/`TranscodeProfile`）与 CLIP 嵌入；`MediaAsset`
//!   的 `clip_embedding` 字段类型一致（`Vec<f32>`，L2 归一化）。
//! - FFmpeg 编排骨架：`FixtureFfmpegRunner` 收到正确构造的 HLS 命令（含 `-f hls` /
//!   `scale=-2:<h>`），失败注入时链路传播为 `ServiceError::Internal`。
//! - CLIP 嵌入：`PlaceholderClipModel` 产 64 维 L2 归一化向量，确定性（同 path 同向量）。
//! - tantivy 索引建/查：ingest 后按文件名/路径关键词命中，CLIP 向量字段保留可读。
//! - 事件链路：每个阶段发对应 EventBus 事件（media.ingested / transcode.completed /
//!   search.performed），全链通后 `MockEventBus` 收到按序事件序列。
//! - 错误传播：ffmpeg 注入非零退出 → 链路在 transcode 阶段中断，不发 transcode.completed，
//!   改发 transcode.failed Error 事件，后续 embed/index 阶段不执行。
//!
//! 红线：不改 trait 签名 / crate 源码——本测试只用 os-services / os-storage /
//! os-core 已暴露的公开 API（含各 crate feature `mock` 注入的 mock 实现）。

use std::sync::Arc;
use std::sync::Mutex;

use os_core::eventbus::{Event, EventBus, Severity, Topic};
use os_core::mock::MockEventBus;
use os_core::Utc;
use os_core::{DatasetId, PoolId};
use os_services::media::{MediaAsset, TranscodeProfile};
use os_services::media_clip::{cosine_similarity, normalize, ClipModel, PlaceholderClipModel};
use os_services::media_ffmpeg::{
    transcode_abr, FfmpegOutput, FfmpegRunner, FixtureFfmpegRunner, HlsVariant, HLS_SEGMENT_SECS,
};
use os_services::media_search::{MediaQuery, SharedMediaIndex};
use os_storage::backend::StorageBackend;
use os_storage::mock::MockStorageBackend;
use os_storage::model::Dataset;
use os_storage::DatasetOptions;

// ----------------------------------------------------------------------------
// MediaTranscodePipeline：业务编排层——把文件接收 / 转码 / CLIP / 索引 / 事件串通。
// 这是 integration-agent 搭的「跨 crate 编排骨架」，验证各子组件协作。
//
// 各阶段职责（呼应 media_impl.rs 的 DefaultMediaManager，但本编排层把 storage 落盘 +
// EventBus 事件显式串通，覆盖跨 crate 链路）：
//   1) receive_file：用 storage mock 落盘（snapshot 模拟持久化）→ 记 asset.path
//   2) transcode：调 media_ffmpeg::transcode_abr（注入 FixtureFfmpegRunner）
//   3) embed_clip：调 PlaceholderClipModel.embed_image（mock CLIP）→ 写 asset.clip_embedding
//   4) index_upsert：把 asset 索引到 SharedMediaIndex（tantivy）
//   5) search：用 MediaQuery DSL 查索引（验证建/查闭环）
// 每阶段发 EventBus 事件；调用顺序记录到 call_log 供断言。
// ----------------------------------------------------------------------------

struct MediaTranscodePipeline {
    /// 文件落盘 mock（snapshot 模拟上传文件的持久化）。
    backend: Arc<MockStorageBackend>,
    /// ffmpeg 子进程 mock（返回 fixture 输出，不真跑 ffmpeg）。
    runner: Arc<FixtureFfmpegRunner>,
    /// CLIP 推理 mock（确定性哈希派生向量，不真跑 candle/ONNX）。
    clip_model: Arc<PlaceholderClipModel>,
    /// tantivy 真实索引（RAM 临时目录，不依赖外部环境）。
    index: Arc<SharedMediaIndex>,
    /// 事件总线 mock（记录发布事件供断言）。
    bus: Arc<MockEventBus>,
    /// 调用顺序记录（断言阶段 ↔ 组件调用对应）。
    call_log: Mutex<Vec<String>>,
}

impl MediaTranscodePipeline {
    #[allow(clippy::too_many_arguments)]
    fn new(
        backend: Arc<MockStorageBackend>,
        runner: Arc<FixtureFfmpegRunner>,
        clip_model: Arc<PlaceholderClipModel>,
        index: Arc<SharedMediaIndex>,
        bus: Arc<MockEventBus>,
    ) -> Self {
        Self {
            backend,
            runner,
            clip_model,
            index,
            bus,
            call_log: Mutex::new(Vec::new()),
        }
    }

    fn log(&self, entry: String) {
        self.call_log.lock().expect("call_log").push(entry);
    }

    fn call_log(&self) -> Vec<String> {
        self.call_log.lock().expect("call_log").clone()
    }

    /// 阶段 1：接收上传文件——用 storage mock 落盘（snapshot 模拟持久化）。
    ///
    /// 返回分配的 asset_id（形如 `asset:/tank/media/<filename>`）。
    async fn receive_file(&self, dataset: &DatasetId, filename: &str) -> (String, u64) {
        let now = Utc::now();
        let snap_name = format!("upload-{}", now.format("%Y%m%dT%H%M%S%3f"));
        // 落盘模拟：对 dataset 建快照（代表上传文件已持久化到存储层）。
        let snap = self
            .backend
            .snapshot(dataset, &snap_name)
            .await
            .expect("上传落盘（snapshot mock）应成功");
        let size_bytes = snap.id.as_str().len() as u64 + 64; // 占位大小
        let asset_path = format!("{}/{}", dataset.as_str(), filename);
        let asset_id = format!("asset:/{}", asset_path);
        self.log(format!(
            "receive_file({asset_path}): snap={}",
            snap.id.as_str()
        ));

        let _ = self
            .bus
            .publish(Event {
                source: "os-services/media".into(),
                topic: Topic::Storage,
                kind: "media.ingested".into(),
                severity: Severity::Info,
                task_id: None,
                payload: serde_json::json!({
                    "asset_id": asset_id,
                    "path": asset_path,
                    "snapshot": snap.id.as_str(),
                    "size_bytes": size_bytes,
                }),
                timestamp: now,
            })
            .await;
        (asset_id, size_bytes)
    }

    /// 阶段 2：转码——调 media_ffmpeg::transcode_abr（注入 FixtureFfmpegRunner）。
    ///
    /// 对给定 asset 路径做多档位 ABR 转码，返回 master playlist 文本。
    /// ffmpeg 失败时返回 ServiceError（保留 stderr 诊断）。
    async fn transcode(
        &self,
        asset_id: &str,
        asset_path: &str,
        output_dir: &std::path::Path,
        variants: &[TranscodeProfile],
    ) -> Result<String, os_services::ServiceError> {
        let now = Utc::now();
        let hls_variants: Vec<HlsVariant> = variants
            .iter()
            .map(|p| HlsVariant::from_profile(*p))
            .collect();

        match transcode_abr(
            self.runner.as_ref() as &dyn FfmpegRunner,
            std::path::Path::new(asset_path),
            output_dir,
            &hls_variants,
            HLS_SEGMENT_SECS,
        )
        .await
        {
            Ok(master) => {
                self.log(format!(
                    "transcode_abr({asset_id}, {} variants): Ok master_len={}",
                    hls_variants.len(),
                    master.len()
                ));
                let _ = self
                    .bus
                    .publish(Event {
                        source: "os-services/media".into(),
                        topic: Topic::Storage,
                        kind: "transcode.completed".into(),
                        severity: Severity::Info,
                        task_id: None,
                        payload: serde_json::json!({
                            "asset_id": asset_id,
                            "variants": variants.iter().map(|p| format!("{p:?}")).collect::<Vec<_>>(),
                            "master_len": master.len(),
                        }),
                        timestamp: now,
                    })
                    .await;
                Ok(master)
            }
            Err(e) => {
                self.log(format!("transcode_abr({asset_id}): Err({e})"));
                let _ = self
                    .bus
                    .publish(Event {
                        source: "os-services/media".into(),
                        topic: Topic::Storage,
                        kind: "transcode.failed".into(),
                        severity: Severity::Error,
                        task_id: None,
                        payload: serde_json::json!({
                            "asset_id": asset_id,
                            "error": e.to_string(),
                        }),
                        timestamp: now,
                    })
                    .await;
                Err(e)
            }
        }
    }

    /// 阶段 3：CLIP 嵌入——调 PlaceholderClipModel.embed_image（mock CLIP）。
    ///
    /// 返回 L2 归一化的确定性向量（同 path 同向量）。失败映射为 ServiceError。
    async fn embed_clip(&self, asset_path: &str) -> Result<Vec<f32>, os_services::ServiceError> {
        let model: &dyn ClipModel = self.clip_model.as_ref();
        let vec = model.embed_image(std::path::Path::new(asset_path)).await?;
        self.log(format!(
            "embed_clip({asset_path}): dim={} norm_ok={}",
            vec.len(),
            is_unit_norm(&vec)
        ));
        Ok(vec)
    }

    /// 阶段 4：索引建——把 asset（含 clip_embedding）upsert 到 tantivy 索引。
    fn index_upsert(&self, asset: &MediaAsset) -> Result<(), os_services::ServiceError> {
        self.index.upsert_and_commit(asset, None)?;
        self.log(format!("index_upsert({}): Ok", asset.id));
        Ok(())
    }

    /// 阶段 5：查——用 MediaQuery DSL 查索引，返回命中的 (asset_id, score)。
    async fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(String, f32)>, os_services::ServiceError> {
        let dsl = MediaQuery::parse(query);
        let hits = self.index.search(&dsl, limit)?;
        self.log(format!("search({query:?}): hits={}", hits.len()));
        let _ = self
            .bus
            .publish(Event {
                source: "os-services/media".into(),
                topic: Topic::Storage,
                kind: "search.performed".into(),
                severity: Severity::Info,
                task_id: None,
                payload: serde_json::json!({
                    "query": query,
                    "hits": hits.len(),
                }),
                timestamp: Utc::now(),
            })
            .await;
        Ok(hits)
    }

    /// 驱动一次完整的「上传 → 转码 → 嵌入 → 索引」端到端链路。
    ///
    /// 成功返回建出的 MediaAsset（含 clip_embedding）；转码失败时在 transcode 阶段
    /// 中断，返回原始 ServiceError（下游断言错误传播）。
    async fn run_full_chain(
        &self,
        dataset: &DatasetId,
        filename: &str,
        variants: &[TranscodeProfile],
        output_dir: &std::path::Path,
    ) -> Result<MediaAsset, os_services::ServiceError> {
        // 1) 接收上传
        let (asset_id, size_bytes) = self.receive_file(dataset, filename).await;
        let asset_path = format!("{}/{}", dataset.as_str(), filename);

        // 2) 转码（失败则中断）
        let _master = self
            .transcode(&asset_id, &asset_path, output_dir, variants)
            .await?;

        // 3) CLIP 嵌入
        let clip_embedding = Some(self.embed_clip(&asset_path).await?);

        // 4) 构造 MediaAsset + 索引
        let asset = MediaAsset {
            id: asset_id.clone(),
            path: asset_path.clone(),
            mime_type: guess_mime(filename).to_string(),
            size_bytes,
            width: Some(1920),
            height: Some(1080),
            taken_at: None,
            faces: vec![],
            clip_embedding,
        };
        self.index_upsert(&asset)?;

        Ok(asset)
    }
}

// ----------------------------------------------------------------------------
// 辅助构造
// ----------------------------------------------------------------------------

/// 判断向量是否 L2 归一化（范数 ≈ 1.0）。
fn is_unit_norm(v: &[f32]) -> bool {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    (norm - 1.0).abs() < 1e-5
}

/// 按扩展名猜 MIME（与 media_impl 的 guess_mime 对齐；纯逻辑）。
fn guess_mime(filename: &str) -> &'static str {
    let lower = filename.to_lowercase();
    if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".mp4") {
        "video/mp4"
    } else if lower.ends_with(".mkv") {
        "video/x-matroska"
    } else {
        "application/octet-stream"
    }
}

/// 构造一个含 tank/media dataset 的 storage mock（媒体库落盘点）。
fn backend_with_media_dataset() -> MockStorageBackend {
    MockStorageBackend::new()
        .with_pool(os_storage::model::Pool {
            id: PoolId::new("tank"),
            name: "tank".into(),
            vdevs: vec![],
            capacity: os_core::Capacity {
                used_bytes: 0,
                total_bytes: 100 * 1024 * 1024 * 1024,
            },
            health: os_core::Health::Healthy,
        })
        .with_dataset(Dataset {
            id: DatasetId::new("tank/media"),
            pool: PoolId::new("tank"),
            name: "media".into(),
            used_bytes: 0,
            avail_bytes: 0,
            mounted: true,
            encryption: os_storage::model::EncryptionState::Off,
        })
}

// ----------------------------------------------------------------------------
// 集成测
// ----------------------------------------------------------------------------

/// 构造一条完整管线（全 mock 后端）。
fn build_pipeline(runner: FixtureFfmpegRunner) -> MediaTranscodePipeline {
    let backend = Arc::new(backend_with_media_dataset());
    let runner = Arc::new(runner);
    let clip_model = Arc::new(PlaceholderClipModel::new());
    let index = Arc::new(SharedMediaIndex::temp().expect("media index temp dir"));
    let bus = Arc::new(MockEventBus::new());
    MediaTranscodePipeline::new(backend, runner, clip_model, index, bus)
}

#[tokio::test]
async fn full_chain_upload_transcode_embed_index_search_succeeds() {
    // 完整链路：receive_file → transcode_abr → embed_clip → index_upsert → search
    // 全部 mock 后端，不真跑 ffmpeg/CLIP。
    let runner = FixtureFfmpegRunner::new(FfmpegOutput::ok());
    let pipeline = build_pipeline(runner);

    let output_dir = std::env::temp_dir().join(format!(
        "media-chain-full-{}-{}",
        std::process::id(),
        unique_counter()
    ));

    let variants = vec![TranscodeProfile::Hls1080p, TranscodeProfile::Hls720p];
    let asset = pipeline
        .run_full_chain(
            &DatasetId::new("tank/media"),
            "vacation.mp4",
            &variants,
            &output_dir,
        )
        .await
        .expect("完整链路应成功");

    // 1) asset 落盘：storage mock 上多了 1 个快照（上传持久化）。
    assert_eq!(pipeline.backend.snapshot_count(), 1, "上传应落盘（1 快照）");

    // 2) asset 字段跨 crate 一致：path 含 dataset + filename；mime 推断正确。
    assert!(asset.path.contains("tank/media"));
    assert!(asset.path.ends_with("vacation.mp4"));
    assert_eq!(asset.mime_type, "video/mp4");

    // 3) CLIP 嵌入：64 维 + L2 归一化。
    let emb = asset.clip_embedding.as_ref().expect("应有 clip_embedding");
    assert_eq!(emb.len(), 64, "PlaceholderClipModel 默认 64 维");
    assert!(is_unit_norm(emb), "CLIP 向量应 L2 归一化");

    // 4) 索引建/查：按文件名关键词命中。
    let hits = pipeline.search("vacation", 10).await.expect("搜索应成功");
    assert!(
        hits.iter().any(|(id, _)| id == &asset.id),
        "按 vacation 命中刚索引的 asset: {hits:?}"
    );

    // 5) 调用顺序：receive → transcode → embed → index → search（5 阶段）。
    let log = pipeline.call_log();
    let receive_idx = log
        .iter()
        .position(|s| s.contains("receive_file"))
        .expect("receive");
    let transcode_idx = log
        .iter()
        .position(|s| s.contains("transcode_abr"))
        .expect("transcode");
    let embed_idx = log
        .iter()
        .position(|s| s.contains("embed_clip"))
        .expect("embed");
    let index_idx = log
        .iter()
        .position(|s| s.contains("index_upsert"))
        .expect("index");
    let search_idx = log
        .iter()
        .position(|s| s.contains("search"))
        .expect("search");
    assert!(
        receive_idx < transcode_idx
            && transcode_idx < embed_idx
            && embed_idx < index_idx
            && index_idx < search_idx,
        "链路顺序错乱: {log:?}"
    );

    // 6) EventBus 事件：ingested + transcode.completed + search.performed（按序）。
    assert_eq!(
        pipeline.bus.published_count_for(Topic::Storage),
        3,
        "应发 3 个 Storage 事件"
    );
    let published = pipeline.bus.published();
    let kinds: Vec<&str> = published.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        vec!["media.ingested", "transcode.completed", "search.performed"],
        "事件顺序错乱: {kinds:?}"
    );
    // transcode.completed payload 带 variants 列表（跨 crate 类型桥接）。
    let tc_ev = published
        .iter()
        .find(|e| e.kind == "transcode.completed")
        .expect("应有 transcode.completed");
    assert_eq!(tc_ev.payload["variants"].as_array().unwrap().len(), 2);

    // 7) ffmpeg runner 收到正确构造的 HLS 命令（含 -f hls + scale）。
    let last_args = pipeline.runner.last_args().expect("runner 应被调用");
    assert!(last_args.contains(&"-f".to_string()), "应含 -f 旗标");
    assert!(last_args.contains(&"hls".to_string()), "应含 hls muxer");
    assert!(
        last_args.iter().any(|a| a.starts_with("scale=-2:")),
        "应含 scale 滤镜（目标高度）: {last_args:?}"
    );

    // 清理临时输出目录。
    let _ = std::fs::remove_dir_all(&output_dir);
}

#[tokio::test]
async fn transcode_abr_produces_master_playlist_with_all_variants() {
    // 验证转码编排骨架：ABR 多档位 → master playlist 含全部变体引用。
    let runner = FixtureFfmpegRunner::new(FfmpegOutput::ok());
    let pipeline = build_pipeline(runner);

    let output_dir = std::env::temp_dir().join(format!(
        "media-chain-abr-{}-{}",
        std::process::id(),
        unique_counter()
    ));

    let variants = vec![
        TranscodeProfile::Hls1080p,
        TranscodeProfile::Hls720p,
        TranscodeProfile::Hls480p,
    ];
    let master = pipeline
        .transcode(
            "asset:/tank/media/trailer.mp4",
            "tank/media/trailer.mp4",
            &output_dir,
            &variants,
        )
        .await
        .expect("ABR 转码应成功");

    // master playlist 含全部三档位的 m3u8 引用 + 分辨率标注。
    assert!(master.starts_with("#EXTM3U"), "master 应是合法 m3u8 头");
    assert!(master.contains("1080p.m3u8"), "应含 1080p 变体");
    assert!(master.contains("720p.m3u8"), "应含 720p 变体");
    assert!(master.contains("480p.m3u8"), "应含 480p 变体");
    assert!(master.contains("RESOLUTION=1920x1080"), "应含 1080p 分辨率");

    // master.m3u8 已落盘（transcode_abr 写盘）。
    let written =
        std::fs::read_to_string(output_dir.join("master.m3u8")).expect("master.m3u8 应落盘");
    assert_eq!(written, master);

    // EventBus 发 transcode.completed（含 3 变体）。
    let published = pipeline.bus.published();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].kind, "transcode.completed");
    assert_eq!(published[0].severity, Severity::Info);
    assert_eq!(
        published[0].payload["variants"].as_array().unwrap().len(),
        3
    );

    let _ = std::fs::remove_dir_all(&output_dir);
}

#[tokio::test]
async fn ffmpeg_failure_aborts_chain_and_emits_error_event() {
    // ffmpeg 注入非零退出 → transcode 阶段失败 → 链路中断，不发 transcode.completed，
    // 改发 transcode.failed Error 事件，后续 embed/index 阶段不执行。
    let runner = FixtureFfmpegRunner::new(FfmpegOutput {
        exit_code: 1,
        stdout: String::new(),
        stderr: "Invalid data found when processing input".into(),
    });
    let pipeline = build_pipeline(runner);

    let output_dir = std::env::temp_dir().join(format!(
        "media-chain-fail-{}-{}",
        std::process::id(),
        unique_counter()
    ));

    let result = pipeline
        .run_full_chain(
            &DatasetId::new("tank/media"),
            "broken.mp4",
            &[TranscodeProfile::Hls720p],
            &output_dir,
        )
        .await;
    assert!(result.is_err(), "ffmpeg 失败应传播为 Err");
    let err = result.unwrap_err();
    assert!(
        matches!(err, os_services::ServiceError::Internal(ref msg) if msg.contains("ffmpeg")),
        "应是 Internal 且含 ffmpeg 诊断，实得 {err:?}"
    );

    // 1) 接收阶段已执行（落盘 1 快照）。
    assert_eq!(pipeline.backend.snapshot_count(), 1);

    // 2) 后续阶段未执行（embed/index 不在调用日志）。
    let log = pipeline.call_log();
    assert!(log
        .iter()
        .any(|s| s.contains("transcode_abr") && s.contains("Err")));
    assert!(
        !log.iter().any(|s| s.contains("embed_clip")),
        "ffmpeg 失败时不应执行 embed_clip"
    );
    assert!(
        !log.iter().any(|s| s.contains("index_upsert")),
        "ffmpeg 失败时不应执行 index_upsert"
    );

    // 3) EventBus 发 transcode.failed Error 事件（不发 completed）。
    let published = pipeline.bus.published();
    let kinds: Vec<&str> = published.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds, vec!["media.ingested", "transcode.failed"]);
    let fail_ev = published
        .iter()
        .find(|e| e.kind == "transcode.failed")
        .expect("应有 transcode.failed");
    assert_eq!(fail_ev.severity, Severity::Error);
    assert_eq!(
        fail_ev.payload["asset_id"].as_str(),
        Some("asset:/tank/media/broken.mp4")
    );

    let _ = std::fs::remove_dir_all(&output_dir);
}

#[tokio::test]
async fn clip_embedding_is_deterministic_and_normalized() {
    // 验证 CLIP 编排骨架：同 path → 同向量（确定性）+ L2 归一化。
    // 这是语义搜索（向量近邻）的基础契约。
    let runner = FixtureFfmpegRunner::new(FfmpegOutput::ok());
    let pipeline = build_pipeline(runner);

    let v1 = pipeline.embed_clip("tank/media/photo1.jpg").await.unwrap();
    let v2 = pipeline.embed_clip("tank/media/photo1.jpg").await.unwrap();
    let v3 = pipeline.embed_clip("tank/media/photo2.jpg").await.unwrap();

    // 同 path 同向量（确定性）。
    assert_eq!(v1, v2, "同 path 应得同向量");
    // 自相似度 = 1.0。
    let self_sim = cosine_similarity(&v1, &v2);
    assert!((self_sim - 1.0).abs() < 1e-5, "自相似度应 = 1.0");

    // 不同 path 大概率不同向量（不严格断言哈希碰撞，但相似度 ∈ [-1,1]）。
    let cross_sim = cosine_similarity(&v1, &v3);
    assert!(
        (-1.0..=1.0).contains(&cross_sim),
        "余弦相似度应在 [-1,1]: {cross_sim}"
    );

    // L2 归一化（normalize 纯函数对已归一化向量是 no-op）。
    let mut v_norm = v1.clone();
    normalize(&mut v_norm);
    assert_eq!(v_norm, v1, "已归一化向量再 normalize 应不变");

    // CLIP 模型维度上报正确（跨 crate 类型桥接）。
    assert_eq!(pipeline.clip_model.embedding_dim(), 64);
}

#[tokio::test]
async fn index_search_round_trip_multiple_assets() {
    // 验证 tantivy 索引建/查闭环：多个 asset 入库后按不同关键词命中不同 asset。
    let runner = FixtureFfmpegRunner::new(FfmpegOutput::ok());
    let pipeline = build_pipeline(runner);

    // 手动构造两个 asset 直接 upsert（绕过转码，专注索引建/查）。
    let asset1 = MediaAsset {
        id: "asset:/tank/media/beach_sunset.jpg".into(),
        path: "tank/media/beach_sunset.jpg".into(),
        mime_type: "image/jpeg".into(),
        size_bytes: 2048,
        width: Some(4000),
        height: Some(3000),
        taken_at: None,
        faces: vec![],
        clip_embedding: None,
    };
    let asset2 = MediaAsset {
        id: "asset:/tank/media/mountain_hike.mp4".into(),
        path: "tank/media/mountain_hike.mp4".into(),
        mime_type: "video/mp4".into(),
        size_bytes: 50_000_000,
        width: Some(1920),
        height: Some(1080),
        taken_at: None,
        faces: vec![],
        clip_embedding: None,
    };
    pipeline.index_upsert(&asset1).unwrap();
    pipeline.index_upsert(&asset2).unwrap();

    // 按 beach 命中 asset1，不命中 asset2。
    let beach_hits = pipeline.search("beach", 10).await.unwrap();
    assert!(beach_hits.iter().any(|(id, _)| id == &asset1.id));
    assert!(!beach_hits.iter().any(|(id, _)| id == &asset2.id));

    // 按 mountain 命中 asset2，不命中 asset1。
    let mtn_hits = pipeline.search("mountain", 10).await.unwrap();
    assert!(mtn_hits.iter().any(|(id, _)| id == &asset2.id));
    assert!(!mtn_hits.iter().any(|(id, _)| id == &asset1.id));

    // 按文件名 stem（sunset / hike）也命中。
    let sunset_hits = pipeline.search("sunset", 10).await.unwrap();
    assert!(sunset_hits.iter().any(|(id, _)| id == &asset1.id));
    let hike_hits = pipeline.search("hike", 10).await.unwrap();
    assert!(hike_hits.iter().any(|(id, _)| id == &asset2.id));

    // 空查询返回全部（tantivy AllQuery）。
    let all_hits = pipeline.search("", 10).await.unwrap();
    assert_eq!(all_hits.len(), 2, "空查询应命中全部 2 个 asset");
}

#[tokio::test]
async fn media_asset_path_cross_crate_type_identity() {
    // 静态验证：MediaAsset.path（os-services）与 storage dataset 路径、ffmpeg input
    // 路径、CLIP embed path 同一字符串来源——跨 crate 字符串拼接不错位。
    let runner = FixtureFfmpegRunner::new(FfmpegOutput::ok());
    let pipeline = build_pipeline(runner);

    let dataset = DatasetId::new("tank/media");
    let filename = "clip_test.mp4";

    // receive_file 用 dataset.as_str() + filename 拼 asset_path
    let (asset_id, _) = pipeline.receive_file(&dataset, filename).await;
    let asset_path = format!("{}/{}", dataset.as_str(), filename);

    // asset_id 形如 "asset:/tank/media/clip_test.mp4"（跨 crate 字符串一致）。
    assert_eq!(asset_id, format!("asset:/{asset_path}"));

    // 同一 asset_path 喂给 ffmpeg 编排（transcode_abr）与 CLIP embed（编译期类型校验）。
    let output_dir = std::env::temp_dir().join(format!(
        "media-chain-type-{}-{}",
        std::process::id(),
        unique_counter()
    ));
    let _master = pipeline
        .transcode(
            &asset_id,
            &asset_path,
            &output_dir,
            &[TranscodeProfile::Hls480p],
        )
        .await
        .unwrap();
    let _vec = pipeline.embed_clip(&asset_path).await.unwrap();

    // ffmpeg runner 记录的 input 路径与 asset_path 一致。
    let last_args = pipeline.runner.last_args().unwrap();
    assert!(
        last_args.iter().any(|a| a == &asset_path),
        "ffmpeg input 应是 asset_path: {last_args:?}"
    );

    let _ = std::fs::remove_dir_all(&output_dir);
}

#[tokio::test]
async fn storage_dataset_missing_propagates() {
    // 源 dataset 不存在（receive_file 指向 ghost dataset）→ snapshot 返回 DatasetNotFound。
    let runner = FixtureFfmpegRunner::new(FfmpegOutput::ok());
    let pipeline = build_pipeline(runner);

    let ghost = DatasetId::new("tank/nonexistent");
    // receive_file 内部调 snapshot，dataset 不存在应失败 → 这里直接验 snapshot 错误。
    let err = pipeline.backend.snapshot(&ghost, "snap").await.unwrap_err();
    assert!(
        matches!(err, os_storage::StorageError::DatasetNotFound(_)),
        "应是 DatasetNotFound，实得 {err:?}"
    );
}

#[tokio::test]
async fn storage_backend_dataset_options_default_compiles() {
    // 跨 crate 类型桥接：DatasetOptions（storage 入参）可跨 crate 构造。
    // 验证 storage mock 的 create_dataset 接口可用（媒体库初始化场景）。
    let backend = backend_with_media_dataset();
    let opts = DatasetOptions::default();
    // create_dataset 在 mock 上成功（即便 pool 存在同名也由 mock 简化处理）。
    let _ds = backend
        .create_dataset(&DatasetId::new("tank/media/sub"), opts)
        .await
        .expect("create_dataset 应在 mock 上成功");
    assert_eq!(backend.dataset_count(), 2, "应多一个 dataset");
}

// ----------------------------------------------------------------------------
// 测试辅助：唯一计数器（避免临时目录碰撞）
// ----------------------------------------------------------------------------

fn unique_counter() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static C: AtomicU64 = AtomicU64::new(0);
    C.fetch_add(1, Ordering::SeqCst)
}
