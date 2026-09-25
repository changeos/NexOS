//! 媒体 / 相册 / 流媒体（规划文档 §3.16 media 组件）
//!
//! 职责：
//! - 媒体入库：扫描文件、提取元数据（尺寸/拍摄时间/MIME）、计算 CLIP 向量与检测人脸
//! - 检索：全文搜索 + 向量（语义）搜索 + 分页
//! - 流媒体：HLS 转码（多档码率）并产出 m3u8 播放地址

use std::path::Path;

use os_core::{DateTime, Deserialize, PageRequest, PageResponse, Serialize, TaskId};

use crate::ServiceError;

// ----------------------------------------------------------------------------
// 人脸 / 边界框
// ----------------------------------------------------------------------------

/// 边界框（人脸/物体在图片中的矩形区域，归一化或像素坐标由实现侧约定）
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// 人脸标签（检测到的人脸 + 可选命名）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FaceTag {
    /// 人脸命名（如 `"张三"`；None = 未命名聚类）
    pub name: Option<String>,
    /// 人脸边界框
    pub bbox: BBox,
}

// ----------------------------------------------------------------------------
// MediaAsset
// ----------------------------------------------------------------------------

/// 媒体资源（一张图片 / 一段视频 / 一段音频）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaAsset {
    /// 资源 ID（入库时分配）
    pub id: String,
    /// 文件路径
    pub path: String,
    /// MIME 类型（如 `"image/jpeg"`）
    pub mime_type: String,
    /// 文件大小（字节）
    pub size_bytes: u64,
    /// 宽度（图片/视频；None = 未知或不适用）
    pub width: Option<u32>,
    /// 高度
    pub height: Option<u32>,
    /// 拍摄时间（从 EXIF 等元数据提取；None = 未知）
    pub taken_at: Option<DateTime>,
    /// 检测到的人脸
    pub faces: Vec<FaceTag>,
    /// CLIP 向量嵌入（用于语义搜索；None = 未计算）
    pub clip_embedding: Option<Vec<f32>>,
}

/// 相册
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Album {
    /// 相册 ID
    pub id: String,
    /// 相册名
    pub name: String,
    /// 资源数量
    pub asset_count: u32,
}

// ----------------------------------------------------------------------------
// 扩展数据模型（批 3 纯逻辑骨架：媒体元数据 / GPS / 转码任务）
// ----------------------------------------------------------------------------

/// GPS 坐标（WGS84，十进制度）
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GpsCoord {
    /// 纬度（-90.0..=90.0，北正南负）
    pub lat: f64,
    /// 经度（-180.0..=180.0，东正西负）
    pub lon: f64,
    /// 海拔（米；None = 未知）
    pub altitude: Option<f64>,
}

impl GpsCoord {
    /// 构造一个无海拔的坐标。
    pub fn new(lat: f64, lon: f64) -> Self {
        Self {
            lat,
            lon,
            altitude: None,
        }
    }

    /// 经纬度合法性（粗校验，仅范围）。
    pub fn is_valid(&self) -> bool {
        (-90.0..=90.0).contains(&self.lat) && (-180.0..=180.0).contains(&self.lon)
    }

    /// Haversine 球面距离（米），用于按地点分组。
    pub fn distance_meters(&self, other: &GpsCoord) -> f64 {
        // WGS84 平均地球半径（米）
        const R: f64 = 6_371_000.0;
        let to_rad = |d: f64| d * std::f64::consts::PI / 180.0;
        let dlat = to_rad(other.lat - self.lat);
        let dlon = to_rad(other.lon - self.lon);
        let lat1 = to_rad(self.lat);
        let lat2 = to_rad(other.lat);
        let a = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
        let c = 2.0 * a.sqrt().asin();
        R * c
    }
}

/// EXIF 元数据（从 JPEG/HEIF 解析出的关键字段，纯逻辑解析见 `media_exif`）
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExifData {
    /// 拍摄时间（EXIF DateTimeOriginal）
    pub taken_at: Option<DateTime>,
    /// 像素宽
    pub width: Option<u32>,
    /// 像素高
    pub height: Option<u32>,
    /// GPS 坐标
    pub gps: Option<GpsCoord>,
    /// 方向（EXIF Orientation 1..=8；None = 未设置）
    pub orientation: Option<u16>,
    /// 相机制造商
    pub make: Option<String>,
    /// 相机型号
    pub model: Option<String>,
    /// 软件/后处理工具
    pub software: Option<String>,
    /// ISO 感光度
    pub iso: Option<u32>,
    /// 曝光时间分子（如 1/60 → (1, 60)）
    pub exposure_time: Option<(u32, u32)>,
    /// 焦距（毫米）
    pub focal_length: Option<f32>,
}

/// 照片元数据（在 MediaAsset 之上的领域细分）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhotoMeta {
    /// 像素宽
    pub width: u32,
    /// 像素高
    pub height: u32,
    /// EXIF 数据（若来源无 EXIF 则为 None）
    pub exif: Option<ExifData>,
}

/// 视频元数据
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoMeta {
    /// 像素宽
    pub width: u32,
    /// 像素高
    pub height: u32,
    /// 时长（秒）
    pub duration_secs: f64,
    /// 视频码率（bps；None = 未知）
    pub bitrate: Option<u64>,
    /// 帧率（fps；None = 未知）
    pub fps: Option<f32>,
    /// 编码格式（如 `"h264"`/`"hevc"`）
    pub codec: Option<String>,
}

/// 转码任务
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscodeJob {
    /// 任务 ID
    pub task_id: TaskId,
    /// 待转码资源 ID
    pub asset_id: String,
    /// 目标档位
    pub profile: TranscodeProfile,
    /// 是否完成
    pub done: bool,
}

/// 相册分组策略
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AlbumGrouping {
    /// 按日期（同一天拍摄的归一组）
    ByDay,
    /// 按月份
    ByMonth,
    /// 按地点（GPS 半径 `meters` 米内归一组）
    ByLocation { meters: u32 },
    /// 按人脸聚类（同一 name FaceTag 归一组）
    ByFace,
    /// 单一相册（全部）
    Single { name: String },
}

// ----------------------------------------------------------------------------
// 转码 profile
// ----------------------------------------------------------------------------

/// HLS 转码档位
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscodeProfile {
    /// 1080p HLS
    Hls1080p,
    /// 720p HLS
    Hls720p,
    /// 480p HLS
    Hls480p,
    /// 原始码流（不转码）
    Original,
}

impl TranscodeProfile {
    /// 该档位的目标垂直分辨率（像素）；Original 返回 0 表示无下采样。
    pub fn target_height(&self) -> u32 {
        match self {
            Self::Hls1080p => 1080,
            Self::Hls720p => 720,
            Self::Hls480p => 480,
            Self::Original => 0,
        }
    }

    /// 大致的目标码率（bps），用于 ABR 估算。
    pub fn target_bitrate_bps(&self) -> u64 {
        // 经验值：参考 HLS 推荐码率表（Apple）。
        match self {
            Self::Hls1080p => 5_000_000,
            Self::Hls720p => 2_800_000,
            Self::Hls480p => 1_400_000,
            Self::Original => 0,
        }
    }

    /// 列出所有转码档位（不含 Original），按分辨率从高到低。
    pub fn variants() -> &'static [TranscodeProfile] {
        &[
            TranscodeProfile::Hls1080p,
            TranscodeProfile::Hls720p,
            TranscodeProfile::Hls480p,
        ]
    }
}

// ----------------------------------------------------------------------------
// MediaManager trait（async）
// ----------------------------------------------------------------------------

/// 媒体管理器——入库、检索、转码、流媒体。
///
/// 实现者：`DefaultMediaManager`（基于 ffmpeg + CLIP 模型 + 向量库）。
#[allow(async_fn_in_trait)]
pub trait MediaManager: Send + Sync {
    /// 入库一个媒体文件：扫描 + 提取元数据 + 计算 CLIP/人脸。
    async fn ingest(&self, path: &Path) -> Result<MediaAsset, ServiceError>;

    /// 检索媒体（全文 + 向量混合搜索），分页返回。
    async fn search(
        &self,
        query: &str,
        page: PageRequest,
    ) -> Result<PageResponse<MediaAsset>, ServiceError>;

    /// 触发转码，返回追踪用的任务 ID。
    async fn transcode(
        &self,
        asset_id: &str,
        profile: TranscodeProfile,
    ) -> Result<TaskId, ServiceError>;

    /// 取流媒体播放地址（HLS m3u8 url；若尚未转码则触发即时转码）。
    async fn stream_playlist(
        &self,
        asset_id: &str,
        profile: TranscodeProfile,
    ) -> Result<String, ServiceError>;

    /// 列出所有相册。
    async fn list_albums(&self) -> Result<Vec<Album>, ServiceError>;
}

// ============================================================================
// 单元测试（纯方法：TranscodeProfile / GpsCoord）
// ============================================================================
#[cfg(test)]
mod tests {
    use super::*;

    // ---- TranscodeProfile 方法 ----

    #[test]
    fn transcode_profile_target_heights() {
        assert_eq!(TranscodeProfile::Hls1080p.target_height(), 1080);
        assert_eq!(TranscodeProfile::Hls720p.target_height(), 720);
        assert_eq!(TranscodeProfile::Hls480p.target_height(), 480);
        assert_eq!(TranscodeProfile::Original.target_height(), 0);
    }

    #[test]
    fn transcode_profile_bitrates_match_apple_hls_table() {
        assert_eq!(TranscodeProfile::Hls1080p.target_bitrate_bps(), 5_000_000);
        assert_eq!(TranscodeProfile::Hls720p.target_bitrate_bps(), 2_800_000);
        assert_eq!(TranscodeProfile::Hls480p.target_bitrate_bps(), 1_400_000);
        assert_eq!(TranscodeProfile::Original.target_bitrate_bps(), 0);
    }

    #[test]
    fn transcode_profile_variants_sorted_high_to_low_excludes_original() {
        let v = TranscodeProfile::variants();
        // 不含 Original
        assert!(!v.contains(&TranscodeProfile::Original));
        // 高到低：1080 → 720 → 480
        assert_eq!(
            v,
            &[
                TranscodeProfile::Hls1080p,
                TranscodeProfile::Hls720p,
                TranscodeProfile::Hls480p,
            ]
        );
        // 高度递减
        assert!(v[0].target_height() > v[1].target_height());
        assert!(v[1].target_height() > v[2].target_height());
    }

    #[test]
    fn transcode_profile_serde_snake_case() {
        // serde rename_all snake_case
        let json = serde_json::to_value(TranscodeProfile::Hls720p).unwrap();
        assert_eq!(json, serde_json::json!("hls720p"));
        let back: TranscodeProfile = serde_json::from_value(json).unwrap();
        assert_eq!(back, TranscodeProfile::Hls720p);

        let json = serde_json::to_value(TranscodeProfile::Original).unwrap();
        assert_eq!(json, serde_json::json!("original"));
    }

    // ---- GpsCoord 方法 ----

    #[test]
    fn gps_coord_new_has_no_altitude() {
        let g = GpsCoord::new(10.0, 20.0);
        assert_eq!(g.lat, 10.0);
        assert_eq!(g.lon, 20.0);
        assert_eq!(g.altitude, None);
    }

    #[test]
    fn gps_coord_validity_boundaries() {
        // 端点合法
        assert!(GpsCoord::new(-90.0, -180.0).is_valid());
        assert!(GpsCoord::new(90.0, 180.0).is_valid());
        // 0,0 合法（Null Island）
        assert!(GpsCoord::new(0.0, 0.0).is_valid());
        // 越界
        assert!(!GpsCoord::new(90.0001, 0.0).is_valid());
        assert!(!GpsCoord::new(-90.0001, 0.0).is_valid());
        assert!(!GpsCoord::new(0.0, 180.0001).is_valid());
        assert!(!GpsCoord::new(0.0, -180.0001).is_valid());
    }

    #[test]
    fn gps_coord_distance_same_point_is_zero() {
        let g = GpsCoord::new(31.23, 121.47);
        assert!(g.distance_meters(&g).abs() < 1e-3);
    }

    #[test]
    fn gps_coord_distance_known_segment() {
        // 1 度纬度 ≈ 111km
        let a = GpsCoord::new(0.0, 0.0);
        let b = GpsCoord::new(1.0, 0.0);
        let d = a.distance_meters(&b);
        assert!((d - 111_195.0).abs() < 500.0, "1° lat ≈ 111km, got {d}");
    }

    #[test]
    fn gps_coord_distance_symmetric() {
        let a = GpsCoord::new(31.23, 121.47);
        let b = GpsCoord::new(40.0, 116.0);
        let d_ab = a.distance_meters(&b);
        let d_ba = b.distance_meters(&a);
        assert!((d_ab - d_ba).abs() < 1e-3);
    }

    // ---- AlbumGrouping serde ----

    #[test]
    fn album_grouping_serde_roundtrip() {
        // 往返不变性（不假设具体的 serde rename 策略，只验证编解码一致）
        fn roundtrip_stable<T: serde::Serialize + serde::de::DeserializeOwned>(v: &T) {
            let j1 = serde_json::to_string(v).expect("serialize 1");
            let back: T = serde_json::from_str(&j1).expect("deserialize");
            let j2 = serde_json::to_string(&back).expect("serialize 2");
            assert_eq!(j1, j2, "serde 往返不稳定");
        }
        roundtrip_stable(&AlbumGrouping::ByDay);
        roundtrip_stable(&AlbumGrouping::ByMonth);
        roundtrip_stable(&AlbumGrouping::ByFace);
        roundtrip_stable(&AlbumGrouping::ByLocation { meters: 500 });
        roundtrip_stable(&AlbumGrouping::Single {
            name: "全部".into(),
        });

        // 字段变体含参数：ByLocation 的 meters 应正确编解码
        let g = AlbumGrouping::ByLocation { meters: 750 };
        let json = serde_json::to_value(&g).unwrap();
        let back: AlbumGrouping = serde_json::from_value(json).unwrap();
        assert_eq!(back, g);
        assert!(matches!(back, AlbumGrouping::ByLocation { meters: 750 }));
    }
}
