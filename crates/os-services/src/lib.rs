//! os-services —— OS 业务功能服务层（备份 / 监控 / 媒体 / 文件 / 开发者工具 / 电源 七大组件）。
//!
//! 定位（规划文档 §3.16）：聚合 OS 面向终端用户的「功能服务」，包含七大独立组件：
//! - **backup**：备份 / 灾备 / 快照策略（cron 调度 + GFS 保留 + ZFS send-recv）
//! - **monitor**：监控 / 告警 / 可观测性（metric + log + alert 规则引擎）
//! - **media**：媒体 / 相册 / 流媒体（EXIF 解析 + 相册分组 + ABR 转码）
//! - **files**：文件管理 / 全文搜索 / 分享 / 同步（冲突解决）
//! - **devtools**：运维开发者工具（日志聚合 + 加密 KVS 密钥 + Git 服务）
//! - **power**：电源 / UPS / 硬件监控（SMART / 风扇 / 温度 / 断电保护）
//!
//! 各组件由独立 owner agent 实现（批 3），文件互不重叠；共享文件（lib.rs/Cargo.toml/
//! mock.rs）由 OrchestratorAgent 累积合并。真实第三方依赖逐步接入中：
//! - `tantivy`（全文搜索）已接入 files 组件（[`search_index`]）与 media 组件（`media_search`）；
//! - FFmpeg 转码编排（`media_ffmpeg`）已接入——外部二进制经 `tokio::process`
//!   spawn（不引 Rust crate），命令构造真实可测；真实 ffmpeg 二进制仍属运行时依赖；
//! - CLIP 向量识别（`media_clip`）接口抽象 + candle 0.11 后端真实推理（ADR-DEPS-005）
//!   已接入——[`media_clip::CandleClipModel`] 加载 ViT-B/32 safetensors 权重，经
//!   candle-transformers 真实 forward 产出 512 维语义嵌入（CUDA 加速经 crate feature
//!   `clip-cuda` 按需开启，RTX 3090 已实测）；权重 + tokenizer.json 由部署预置（不入 git），
//!   无 GPU/无权重环境回退 [`media_clip::PlaceholderClipModel`]。
//!
//! > **审计注记**：本 crate 的 TODO 已按类别（\[RUNTIME]/\[STUB]/\[DOC]/\[OBSOLETE]）分类，
//! > 详见 `docs/TODO_AUDIT.md`。\[STUB] 类已补实现（如 [`files_model::hash_password`]
//! > 升级为 SHA-256、[`search_index::SearchIndex::delete_by_path`] 真实接入 tantivy 删除）。
//!
//! 契约规范：
//! - 数据路径 trait 用原生 `async fn in trait`（无 `#[async_trait]`），lib 顶部统一 `#![allow(async_fn_in_trait)]`
//! - ID 复用 os-core 的 newtype（DatasetId / SnapshotId / PoolId / TaskId）
//! - 自定义 `ServiceError`，并实现 `From<ServiceError> for os_common::ApiError`
//!
//! # 模块（七大组件契约）
//!
//! - [`backup`]：备份/灾备/快照策略——[`BackupManager`] trait（cron 调度 + GFS 保留 + ZFS send-recv）。
//! - [`monitor`]：监控/告警/可观测性——[`Monitor`] trait（metric/log/alert 规则引擎）。
//! - [`media`]：媒体/相册/流媒体——[`MediaManager`] trait（EXIF + 相册分组 + ABR 转码）。
//! - [`files`]：文件管理/全文搜索/分享/同步——[`FileManager`] trait（冲突解决）。
//! - [`devtools`]：运维开发者工具——[`DevTools`] trait（日志聚合 + 加密 KVS 密钥 + Git 服务）。
//! - [`power`]：电源/UPS/硬件监控——[`PowerManager`] trait（SMART/风扇/温度/断电保护）。
//!
//! # 实现模块（各 owner agent 填充）
//!
//! - [`files_model`]：files 纯函数（分享 token / 冲突三路合并 / glob 匹配 / 分页排序）。
//! - [`search_index`]：tantivy 全文搜索索引（[`SearchIndex`]）。
//! - [`media_exif`] / [`media_album`] / [`media_abr`] / [`media_ffmpeg`] / [`media_clip`] / [`media_search`]：
//!   媒体各子能力（EXIF 解析 / 相册分组 / ABR 码率选择 / FFmpeg 转码编排 / CLIP 向量识别 / 媒体搜索）。
//! - [`error`]：`ServiceError` / `ServiceResult`。
//! - `mock`：聚合各组件 Mock（仅 `mock` feature）。
//!
//! # 关键 trait
//!
//! - [`BackupManager`] / [`Monitor`] / [`MediaManager`] / [`FileManager`] / [`DevTools`] / [`PowerManager`]：
//!   七大组件的数据路径 trait（均 async fn in trait）。
//! - [`ClipModel`]：CLIP 向量识别抽象（[`CandleClipModel`] 真实推理 / [`PlaceholderClipModel`] 无 GPU 回退）。
//! - [`FfmpegRunner`]：FFmpeg 外部二进制执行抽象（`TokioFfmpegRunner` 真实 spawn / `FixtureFfmpegRunner` 测试桩）。
//!
//! # feature 门控
//!
//! - `mock`（默认关）：开启 `mock` 模块，聚合导出 `MockBackupManager`/`MockFileManager`/... 供下游测试注入。
//! - `clip-cuda`（默认关）：开启 candle-core/nn/transformers 的 CUDA 后端，CLIP 推理走 GPU（[`CandleClipModel`]）。
//! - `git-remote`（默认关）：开启 gix 阻塞网络客户端，支持 devtools 组件远端 git clone（`git_clone_repo`）。

#![allow(async_fn_in_trait)]

// —— 各组件契约 trait + 数据结构 ——
pub mod backup;
pub mod devtools;
pub mod error;
pub mod files;
pub mod media;
pub mod monitor;
pub mod power;

// —— backup-agent 实现（批 3）——
mod backup_drill;
mod backup_retention;
mod backup_schedule;
mod impl_backup;

// —— devtools-agent 实现（批 3）——
mod impl_devtools;

// —— files-agent 实现（批 3）——
pub mod files_model;
mod impl_files;
pub mod search_index;

// —— media-agent 实现（批 3）——
pub mod media_abr;
pub mod media_album;
pub mod media_clip;
pub mod media_exif;
pub mod media_ffmpeg;
mod media_impl;
// media-agent（真实集成）：tantivy 媒体元数据搜索（接通 search 的子串占位）
pub mod media_search;

// —— monitor-agent 实现（批 3，扩在 monitor.rs 内）——

// —— power-agent 实现（批 3）——
mod impl_power;

// —— Mock（feature `mock`；聚合各组件 Mock 供下游测试注入）——
#[cfg(feature = "mock")]
pub mod mock;

// ============================================================================
// re-export：契约类型（trait + 数据结构）
// ============================================================================
pub use backup::{BackupJob, BackupManager, BackupPolicy, CronExpr, RetentionPolicy, ScrubReport};
pub use devtools::{CiPipeline, CiRun, CiStatus, DevTools, SecretEntry};
pub use error::{ServiceError, ServiceResult};
pub use files::{FileEntry, FileManager, SearchHit, ShareLink, SyncConfig};
pub use media::{Album, BBox, FaceTag, MediaAsset, MediaManager, TranscodeProfile};
pub use monitor::{Alert, AlertRule, LogEntry, LogFilter, LogLevel, Metric, MetricKind, Monitor};
pub use power::{FanReading, PowerManager, PowerSchedule, SmartReport, TempReading, UpsStatus};

// ============================================================================
// re-export：各组件实现（具体 struct，供直接消费或 mock 注入）
// ============================================================================

// backup
pub use backup_drill::{DrillPlan, DrillResult, DrillStatus};
pub use backup_retention::{select_expired, RetentionRule as GfsRetentionRule, TimedSnapshot};
pub use backup_schedule::{CronSchedule, SchedulePolicy};
pub use impl_backup::ZfsBackupManager;

// devtools
pub use devtools::{
    Branch as GitBranch, Commit as GitCommit, DevLogEntry, DevLogLevel, LogQuery, RepoSpec,
    SecretAction, SecretAuditEntry, SecretAuditLog, SecretId, SecretMeta,
};
pub use impl_devtools::{
    commit_all as git_commit_all, create_branch as git_create_branch,
    head_commit as git_head_commit, init_repo as git_init_repo, list_branches as git_list_branches,
    log as git_log, DefaultDevTools,
};
// 远端 git clone（crate 级 `git-remote` feature 门控）：clone_repo + ClonedRepo 供
// tests/git_remote_real.rs #[ignore] 真实测直接调用（impl_devtools 是私有模块）。
#[cfg(feature = "git-remote")]
pub use impl_devtools::{clone_repo as git_clone_repo, ClonedRepo as GitClonedRepo};

// files
pub use files_model::{
    check_share_access, constant_time_eq, generate_share_token, glob_any_matches, glob_matches,
    paginate_hits, resolve_conflict, sort_entries, text_search, three_way_merge, AccessDecision,
    AccessRequest, ConflictSide, DirListing, FileKind, FileVersion, ListQuery, ResolveResult,
    ResolveStrategy, SortDir, SortKey, SyncConflict,
};
pub use impl_files::DefaultFileManager;
pub use search_index::{IndexedFile, SearchIndex};

// media
pub use media_abr::{select_profile, select_profile_for_bitrate, AbrConfig};
pub use media_album::group_into_albums;
pub use media_clip::{
    cluster_by_similarity, cosine_similarity, label_scene, normalize, CandleClipModel, ClipModel,
    Cluster, PlaceholderClipModel, SceneLabel, DEFAULT_SCENE_LABELS,
};
pub use media_exif::parse_exif;
pub use media_ffmpeg::{
    build_hls_args, build_master_playlist, build_media_playlist, transcode_abr, transcode_variant,
    FfmpegOutput, FfmpegRunner, FixtureFfmpegRunner, HlsVariant, TokioFfmpegRunner,
    HLS_SEGMENT_SECS,
};
pub use media_impl::DefaultMediaManager;

// monitor（实现扩在 monitor.rs 内的 mock 子模块）
#[cfg(feature = "mock")]
pub use monitor::mock::MockMonitor;

// power（整合同 power-agent worktree：把这些纯函数/类型也作为公开 API，
// 避免 mod 内 pub 项被 dead_code 警告；它们是 power-agent 的契约一部分）
pub use impl_power::{
    assess_smart_health, decide_ups_shutdown, is_valid_cron, parse_sensors_output,
    parse_smartctl_json, parse_upsc_output, AuditEntry, LinuxPowerManager, SmartAttribute,
    SmartHealth, UpsShutdownConfig, UpsShutdownDecision,
};

// ============================================================================
// Mock re-export（feature `mock`；聚合各组件 Mock）
// ============================================================================
#[cfg(feature = "mock")]
pub use mock::{
    MockBackupManager, MockDevTools, MockFileManager, MockMediaManager, MockPowerManager,
};

// ============================================================================
// 契约类型 serde 往返测试（lib 级聚合，避免分散到各 contract 文件）
// ============================================================================
#[cfg(test)]
mod contract_serde_tests {
    use crate::backup::{BackupJob, BackupPolicy, BackupStatus, CronExpr, RetentionPolicy};
    use crate::files::{FileEntry, SearchHit, ShareLink, SyncConfig};
    use crate::media::{Album, BBox, FaceTag, MediaAsset};
    use crate::monitor::{Alert, AlertRule, AlertSeverity, Metric, MetricKind};
    use crate::power::{FanReading, PowerSchedule, SmartReport, TempReading, UpsStatus};
    use os_core::{DateTime, Utc};

    /// 序列化 → 反序列化 → 再序列化，比较两次 JSON 字符串是否完全一致
    /// （证明往返不变性，不需要类型实现 PartialEq）。
    fn roundtrip_stable<T: serde::Serialize + serde::de::DeserializeOwned>(v: &T) {
        let j1 = serde_json::to_string(v).expect("serialize 1");
        let back: T = serde_json::from_str(&j1).expect("deserialize");
        let j2 = serde_json::to_string(&back).expect("serialize 2");
        assert_eq!(j1, j2, "serde 往返不稳定");
    }

    #[test]
    fn power_types_serde_roundtrip() {
        let ups = UpsStatus {
            online: true,
            battery_level: Some(80),
            estimated_minutes: Some(40),
            model: "APC".into(),
        };
        roundtrip_stable(&ups);

        let fan = FanReading {
            label: "cpu_fan".into(),
            rpm: 1500,
        };
        roundtrip_stable(&fan);

        let temp = TempReading {
            label: "cpu".into(),
            celsius: 45.5,
        };
        roundtrip_stable(&temp);

        let smart = SmartReport {
            disk: "/dev/sda".into(),
            passed: true,
            temperature: 35.0,
            reallocated_sectors: 0,
            power_on_hours: 12000,
        };
        roundtrip_stable(&smart);

        let sched = PowerSchedule {
            power_on_cron: Some("0 3 * * *".into()),
            shutdown_cron: None,
        };
        roundtrip_stable(&sched);
        // 字段级抽查
        let back: PowerSchedule =
            serde_json::from_str(&serde_json::to_string(&sched).unwrap()).unwrap();
        assert_eq!(back.power_on_cron.as_deref(), Some("0 3 * * *"));
        assert!(back.shutdown_cron.is_none());
    }

    #[test]
    fn files_types_serde_roundtrip() {
        let now: DateTime = Utc::now();
        let entry = FileEntry {
            name: "a.txt".into(),
            is_dir: false,
            size: 1024,
            modified: now,
            permissions: "rw-r--r--".into(),
        };
        roundtrip_stable(&entry);

        let share = ShareLink {
            id: "id1".into(),
            target_path: "/x".into(),
            token: "tok".into(),
            expires_at: Some(now),
            password_hash: Some("h".into()),
            rate_limit_kbps: Some(100),
            created_by: "u".into(),
        };
        roundtrip_stable(&share);

        let hit = SearchHit {
            path: "/p".into(),
            snippet: "snip".into(),
            score: 1.5,
        };
        roundtrip_stable(&hit);

        let sync = SyncConfig {
            enabled: true,
            interval_secs: 60,
            excludes: vec!["*.tmp".into()],
        };
        roundtrip_stable(&sync);
        // 字段级抽查
        let back: SyncConfig =
            serde_json::from_str(&serde_json::to_string(&sync).unwrap()).unwrap();
        assert!(back.enabled);
        assert_eq!(back.excludes, vec!["*.tmp".to_string()]);
    }

    #[test]
    fn backup_types_serde_roundtrip() {
        let cron = CronExpr::new("0 3 * * *");
        roundtrip_stable(&cron);
        // CronExpr serde transparent —— 应直接是字符串
        let json = serde_json::to_value(&cron).unwrap();
        assert_eq!(json, serde_json::json!("0 3 * * *"));

        let retention = RetentionPolicy {
            keep_last: 7,
            keep_days: 30,
        };
        roundtrip_stable(&retention);

        let policy = BackupPolicy {
            name: "daily".into(),
            schedule: cron.clone(),
            retention: retention.clone(),
            source: os_core::DatasetId::new("tank/media"),
            target_remote: Some("remote://backup".into()),
        };
        roundtrip_stable(&policy);

        let job = BackupJob {
            id: "job1".into(),
            policy: policy.clone(),
            last_run: Some(Utc::now()),
            next_run: None,
            status: BackupStatus::Scheduled,
        };
        roundtrip_stable(&job);

        // BackupStatus serde snake_case
        let json = serde_json::to_value(BackupStatus::Running).unwrap();
        assert_eq!(json, serde_json::json!("running"));
        let json = serde_json::to_value(BackupStatus::Success).unwrap();
        assert_eq!(json, serde_json::json!("success"));
    }

    #[test]
    fn media_types_serde_roundtrip() {
        let bbox = BBox {
            x: 0.1,
            y: 0.2,
            w: 0.3,
            h: 0.4,
        };
        roundtrip_stable(&bbox);

        let face = FaceTag {
            name: Some("alice".into()),
            bbox,
        };
        roundtrip_stable(&face);

        let asset = MediaAsset {
            id: "a1".into(),
            path: "/p/a.jpg".into(),
            mime_type: "image/jpeg".into(),
            size_bytes: 1024,
            width: Some(1920),
            height: Some(1080),
            taken_at: Some(Utc::now()),
            faces: vec![face.clone()],
            clip_embedding: Some(vec![0.1, 0.2]),
        };
        roundtrip_stable(&asset);
        // 字段级抽查
        let back: MediaAsset =
            serde_json::from_str(&serde_json::to_string(&asset).unwrap()).unwrap();
        assert_eq!(back.id, "a1");
        assert_eq!(back.faces.len(), 1);

        let album = Album {
            id: "alb1".into(),
            name: "2024-06".into(),
            asset_count: 5,
        };
        roundtrip_stable(&album);
    }

    #[test]
    fn monitor_types_serde_roundtrip() {
        let metric = Metric::gauge("cpu", 0.5, Utc::now()).with_label("host", "os1");
        roundtrip_stable(&metric);
        // 字段级抽查
        let back: Metric = serde_json::from_str(&serde_json::to_string(&metric).unwrap()).unwrap();
        assert_eq!(back.kind, MetricKind::Gauge);
        assert_eq!(back.labels.get("host").map(|s| s.as_str()), Some("os1"));

        // MetricKind serde snake_case
        assert_eq!(
            serde_json::to_value(MetricKind::Counter).unwrap(),
            serde_json::json!("counter")
        );
        assert_eq!(
            serde_json::to_value(MetricKind::Histogram).unwrap(),
            serde_json::json!("histogram")
        );

        let rule = AlertRule {
            name: "cpu_high".into(),
            metric: "cpu".into(),
            condition: ">0.9".into(),
            for_duration_secs: 60,
            severity: AlertSeverity::Warning,
        };
        roundtrip_stable(&rule);
        // AlertSeverity serde snake_case
        assert_eq!(
            serde_json::to_value(AlertSeverity::Critical).unwrap(),
            serde_json::json!("critical")
        );

        let alert = Alert {
            rule_name: "cpu_high".into(),
            severity: AlertSeverity::Critical,
            fired_at: Utc::now(),
            resolved: false,
            message: "cpu 0.95".into(),
        };
        roundtrip_stable(&alert);
        let back: Alert = serde_json::from_str(&serde_json::to_string(&alert).unwrap()).unwrap();
        assert!(!back.resolved);
    }

    #[test]
    fn upc_status_none_fields_serde() {
        // None 字段正确序列化为 null（而非缺省）
        let ups = UpsStatus {
            online: false,
            battery_level: None,
            estimated_minutes: None,
            model: "unknown".into(),
        };
        let json = serde_json::to_value(&ups).unwrap();
        assert!(json.get("battery_level").unwrap().is_null());
        assert!(json.get("estimated_minutes").unwrap().is_null());
        roundtrip_stable(&ups);
    }
}
