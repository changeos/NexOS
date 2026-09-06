//! `QrTransferRouteHandler` —— 二维码文件传输桌面应用的 HTTP→内存态适配器。
//!
//! 定位：把网关 HTTP 请求（`/api/v1/qr/*`）翻译为 QR 编解码任务，返回 JSON。
//! 这是 OS"二维码文件传输"桌面应用（文件 → 跳动 QR 视频 → 解码回文件）的后端
//! REST 入口。
//!
//! # 两个功能域
//!
//! - **编码（encode）**：选一个本地文件 → 读二进制 → Base64 → 分块 → 每块生成
//!   一帧 QR PNG → ffmpeg 合成"跳动 QR 视频"（每帧一个 QR，按时序播放）。
//! - **解码（decode）**：上传 QR 视频 / 图片 → ffmpeg 拆帧 → 逐帧 rqrr 解码 →
//!   按 seq 拼接 Base64 → 解码回二进制 → 写文件。
//!
//! # 实现策略：内存态任务表 + 纯 Rust QR 编解码（rustify，真实数据，无 demo 预置）
//!
//! `new()` 启动时空（encode_tasks / decode_tasks 全部空 vec![]）。任务由前端发起：
//! POST /encode 创建编码任务（status=pending）→ tokio::spawn 后台跑真实编码流程
//! （纯 Rust [`generate_qr_png`] 逐块生成 QR 帧 + ffmpeg 合成 MP4）。
//! QR 生成/解码用 `qrcode`/`rqrr`/`image` 纯 Rust crate（原 Python qrcode/pyzbar
//! 子进程已移除）；ffmpeg 不存在 → 任务标 failed，**绝不 panic**。
//! 这样保证：编译通过 + 编解码正确（roundtrip 纯函数可单测）+ 测试可跑（不依赖外部进程）。
//!
//! # 分块协议
//!
//! - 文件二进制 → Base64 → 按 `chunk_size`（默认 2048）切片
//! - 每帧 QR 含 JSON header：`{seq, total, crc, data}`（seq 从 0 起；crc 为该块
//!   Base64 字符串的 zlib CRC32，hex 形式；data 为该块的 Base64 片段）
//! - 解码端按 seq 升序拼接 data → Base64 解码 → 二进制 → 写文件；任一块 CRC 不符
//!   → crc_ok=false（仍尝试输出，前端提示）
//!
//! # 路由表（9 条，全部归属 component="qr_transfer"）
//!
//! | method | path                              | 动作 |
//! |--------|-----------------------------------|------|
//! | POST   | `/api/v1/qr/encode`               | 编码文件为 QR 视频（admin）|
//! | GET    | `/api/v1/qr/encode/:id`           | 编码任务状态 + 视频 URL |
//! | GET    | `/api/v1/qr/encode/:id/video`     | 下载/流式播放生成的 QR 视频 |
//! | POST   | `/api/v1/qr/decode`              | 解码（admin）上传视频/图片 → 文件 |
//! | GET    | `/api/v1/qr/decode/:id`          | 解码任务状态 + 输出文件路径 |
//! | GET    | `/api/v1/qr/decode/:id/file`     | 下载解码后的文件 |
//! | POST   | `/api/v1/qr/encode-text`         | 文本 → QR 图片（admin，即时）|
//! | POST   | `/api/v1/qr/decode-text`         | QR 图片 → 文本（admin，即时）|
//! | GET    | `/api/v1/qr/stats`               | 聚合统计 |
//!
//! # 引擎门控（2026-09-05：二维码传输剥离为独立应用，docs/APPS.md §7）
//!
//! qr_transfer 引擎**内置**于 os-api（代码仍编译在二进制内），但按「装了应用
//! 才启用」架构运行（film 同款）：未安装声明 `engine="qrtransfer"` 的应用包
//! （经应用中心安装，apps 表登记）时，上表全部业务端点一律 404
//! `{"error":"应用「二维码传输」未安装：可在 应用中心 → 商店 安装"}`。门控
//! 每请求直查 apps 表（`AppRegistry::is_engine_enabled`，无缓存）——安装/卸载
//! **即时生效**；表损坏/锁失败 fail-closed（按未装处理）。未注入注册表
//! （单测直构）不门控，既有测试契约不变；生产 main.rs 恒注入。

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine;
use image::GrayImage;
use qrcode::EcLevel;
use serde::{Deserialize, Serialize};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

/// 生成的 QR 视频默认存放根目录（`/tank/os-data/qr-videos/<id>.mp4`）。
const QR_VIDEO_DIR: &str = "/tank/os-data/qr-videos";
/// QR 视频降级目录（`/tank` 不可写时）。
const QR_VIDEO_DIR_FALLBACK: &str = "/tmp/os-qr-videos";
/// 解码后输出文件默认目录（`/tank/os-data/qr-decoded/<id>.bin`）。
const QR_DECODED_DIR: &str = "/tank/os-data/qr-decoded";
/// 解码降级目录。
const QR_DECODED_DIR_FALLBACK: &str = "/tmp/os-qr-decoded";
/// 默认帧率（每秒 QR 帧数）。
const DEFAULT_FPS: u32 = 5;
/// 默认分块大小（Base64 字符数；约对应 ~1.5KB 原始字节，QR 容量安全）。
const DEFAULT_CHUNK_SIZE: usize = 2048;

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 编码任务（文件 → QR 视频）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncodeTask {
    pub id: String,
    /// 待编码的源文件绝对路径（如 /tank/media/video/test.mp4）。
    pub file_path: String,
    /// 状态：pending / encoding / completed / failed
    pub status: String,
    /// 总帧数（= 总块数；pending/encoding 时为 0 或估算值）。
    pub total_frames: u64,
    /// 源文件大小（字节）。
    pub file_size: u64,
    /// 帧率（每秒 QR 帧数）。
    pub fps: u32,
    /// 分块大小（Base64 字符数）。
    pub chunk_size: usize,
    /// 生成的视频绝对路径（completed 时）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_path: Option<String>,
    /// 视频下载/播放 URL（completed 时；同源相对路径）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_url: Option<String>,
    /// Python/ffmpeg 子进程 pid（encoding 时）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// 失败原因（failed 时）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: String,
}

/// 解码任务（QR 视频/图片 → 文件）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecodeTask {
    pub id: String,
    /// 状态：pending / decoding / completed / failed
    pub status: String,
    /// 输入媒体来源描述（`upload:<filename>` / `path:/xxx`）。
    pub source: String,
    /// 拆帧得到的总帧数（视频 = ffmpeg 拆出帧数；图片 = 1）。
    pub total_frames: u64,
    /// 成功解码（rqrr 读到 QR）的帧数。
    pub decoded_frames: u64,
    /// 是否所有块 CRC 校验通过。
    pub crc_ok: bool,
    /// 解码后输出文件绝对路径（completed 时）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_path: Option<String>,
    /// 输出文件下载 URL（completed 时）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_url: Option<String>,
    /// Python/ffmpeg 子进程 pid（decoding 时）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// 失败原因（failed 时）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: String,
}

/// `GET /api/v1/qr/stats` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QrStats {
    pub encode_total: usize,
    pub encode_pending: usize,
    pub encode_encoding: usize,
    pub encode_completed: usize,
    pub encode_failed: usize,
    pub decode_total: usize,
    pub decode_pending: usize,
    pub decode_decoding: usize,
    pub decode_completed: usize,
    pub decode_failed: usize,
}

/// `POST /api/v1/qr/encode` 请求体。
#[derive(Debug, Deserialize)]
struct EncodeBody {
    #[serde(default)]
    file_path: String,
    #[serde(default)]
    fps: Option<u32>,
    #[serde(default)]
    chunk_size: Option<usize>,
}

/// `POST /api/v1/qr/decode` 请求体。
///
/// 支持两种输入：
/// - `file_path`：服务端已有文件绝对路径（如 /tmp/upload.mp4）
/// - `media_base64` + `filename`：前端上传（base64 编码的二进制媒体）
#[derive(Debug, Deserialize)]
struct DecodeBody {
    #[serde(default)]
    file_path: Option<String>,
    #[serde(default)]
    media_base64: Option<String>,
    #[serde(default)]
    filename: Option<String>,
}

/// `POST /api/v1/qr/encode-text` 请求体（文本 → QR 图片）。
#[derive(Debug, Deserialize)]
struct TextEncodeBody {
    #[serde(default)]
    text: String,
    /// 纠错级别 L/M/Q/H，默认 L（容量最大）。
    #[serde(default)]
    error_level: Option<String>,
}

/// `POST /api/v1/qr/decode-text` 请求体（QR 图片 → 文本）。
#[derive(Debug, Deserialize)]
struct TextDecodeBody {
    #[serde(default)]
    image_base64: String,
}

// ----------------------------------------------------------------------------
// 纯函数：分块 + Python 脚本构造（易测试，不依赖外部进程）
// ----------------------------------------------------------------------------

/// 将字节序列按固定大小切片。
///
/// 空数据返回空 Vec；`size == 0` 视为 1（避免死循环；caller 应保证 size ≥ 1）。
#[must_use]
pub fn split_chunks(data: &[u8], size: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    if data.is_empty() {
        return out;
    }
    let chunk = if size == 0 { 1 } else { size };
    let mut i = 0;
    while i < data.len() {
        let end = (i + chunk).min(data.len());
        out.push(data[i..end].to_vec());
        i = end;
    }
    out
}

/// 将文本按 `max_bytes` 字节上限切片（**考虑 UTF-8 字符边界**，绝不截断多字节字符）。
///
/// 用于"文本 → QR"分块：QR 每张最多装 `max_bytes` 字节，但 UTF-8 中文一个字符
/// 占 3 字节，不能在字符中间切断（否则产生无效 UTF-8）。本函数在字节预算内回退
/// 到最近的字符边界；若预算小到放不下一个字符（如 max_bytes < 4 且遇 4 字节 emoji），
/// 则强制至少装一个完整字符向前推进。
///
/// - 空文本 → 空 Vec
/// - `max_bytes == 0` → 视为 1（避免死循环）
/// - 各片拼接后 == 原文本（字节级一致）
#[must_use]
pub fn split_text(text: &str, max_bytes: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let max = if max_bytes == 0 { 1 } else { max_bytes };
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < bytes.len() {
        let mut end = (start + max).min(bytes.len());
        // 回退到 UTF-8 字符边界（不在多字节字符中间切断）
        while end < bytes.len() && end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        // 预算小到放不下一个字符：强制推进一个完整字符
        if end == start {
            end = start + text[start..].chars().next().map_or(1, |c| c.len_utf8());
        }
        out.push(text[start..end].to_string());
        start = end;
    }
    out
}

// ----------------------------------------------------------------------------
// 纯 Rust QR 编解码（rustify：替代 python3 qrcode/pyzbar 子进程脚本构造）
// ----------------------------------------------------------------------------

/// QR 单张字节模式容量上限（Version 40 + 纠错 L）。
const QR_MAX_DATA: usize = 2953;
/// 文本分块 JSON header 预留字节（{"seq":NN,"total":NN,"data":"..."}）。
const TEXT_HEADER_ROOM: usize = 80;
/// 文本分块最大 QR 张数（超限提示改用文件传输）。
const MAX_TEXT_QR_COUNT: usize = 50;

/// CRC-32（IEEE 802.3，反射多项式 0xEDB88320，与 zlib.crc32 / Python zlib 兼容）。
/// 逐位无表实现（不引入外部 crate；协议兼容原 Python 脚本的分块校验）。
#[must_use]
pub fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// 纠错级别字符串 → `qrcode::EcLevel`（L/M/Q/H，非法值回退 L）。
fn ec_level(level: &str) -> EcLevel {
    match level.trim().to_ascii_uppercase().as_str() {
        "M" => EcLevel::M,
        "Q" => EcLevel::Q,
        "H" => EcLevel::H,
        _ => EcLevel::L,
    }
}

/// 生成一张 QR PNG（纯 Rust：`qrcode` 模块矩阵 + `image` PNG 编码）。
///
/// 布局：4 模块静区 + 每模块 8px（总像素恒为偶数，保证下游 ffmpeg libx264
/// yuv420p 编码可用）。payload 超容量 → Err（caller 标 failed，不 panic）。
pub fn generate_qr_png(data: &str, level: EcLevel) -> Result<Vec<u8>, String> {
    let code = qrcode::QrCode::with_error_correction_level(data, level)
        .map_err(|e| format!("QR 生成失败（payload 超容量？）: {e}"))?;
    let colors = code.to_colors();
    let modules = code.width();
    let quiet = 4usize;
    let scale = 8u32;
    let total = ((modules + quiet * 2) as u32) * scale;
    let img: GrayImage = image::ImageBuffer::from_fn(total, total, |x, y| {
        // 模块坐标（含 4 模块静区）；静区恒白，数据区按模块矩阵渲染
        let mx = x / scale;
        let my = y / scale;
        let quiet32 = quiet as u32;
        let in_code = mx >= quiet32
            && my >= quiet32
            && mx - quiet32 < modules as u32
            && my - quiet32 < modules as u32;
        let dark = in_code && {
            let qx = (mx - quiet32) as usize;
            let qy = (my - quiet32) as usize;
            colors[qy * modules + qx] == qrcode::Color::Dark
        };
        image::Luma([if dark { 0 } else { 255 }])
    });
    let mut png = Vec::new();
    image::DynamicImage::ImageLuma8(img)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| format!("PNG 编码失败: {e}"))?;
    Ok(png)
}

/// 从 PNG 字节解码 QR 内容（纯 Rust：`image` 解码 + `rqrr` 识别）。
///
/// 无 QR / 解码失败返回 Err（不 panic）。常见失败：图片模糊、无静区。
pub fn decode_qr(png_bytes: &[u8]) -> Result<String, String> {
    let img = image::load_from_memory(png_bytes).map_err(|e| format!("图片解码失败: {e}"))?;
    let gray = img.to_luma8();
    let mut prepared = rqrr::PreparedImage::prepare(gray);
    prepared
        .detect_grids()
        .into_iter()
        .next()
        .ok_or_else(|| "未检测到二维码".to_string())?
        .decode()
        .map(|(_meta, content)| content)
        .map_err(|e| format!("QR 解码失败: {e}"))
}

/// 构造文件编码协议的单帧 payload（与原 Python 脚本输出逐字节兼容）：
/// `{"seq":N,"total":M,"crc":"%08x","data":"<base64 片段>"}`。
fn encode_frame_payload(seq: usize, total: usize, data: &str) -> String {
    serde_json::to_string(&serde_json::json!({
        "seq": seq,
        "total": total,
        "crc": format!("{:08x}", crc32_ieee(data.as_bytes())),
        "data": data,
    }))
    .unwrap_or_default()
}

// ----------------------------------------------------------------------------
// QrTransferRouteHandler
// ----------------------------------------------------------------------------

/// 二维码文件传输路由处理器——HTTP 边界适配到内存态编/解码任务表。
pub struct QrTransferRouteHandler {
    encode_tasks: Mutex<Vec<EncodeTask>>,
    decode_tasks: Mutex<Vec<DecodeTask>>,
    counter: Mutex<u64>,
    /// 应用注册表（引擎门控）：注入后每请求查 apps 表——未安装 qrtransfer 应用
    /// 则全部业务端点 404（引擎内置、应用按装启用，docs/APPS.md §7）。None =
    /// 未注入（单测直构），不门控；生产 main.rs 恒注入。
    app_registry: Option<Arc<super::apps_handler::AppRegistry>>,
}

impl QrTransferRouteHandler {
    /// 构造 handler——**启动时空**，encode_tasks / decode_tasks 均为空列表。
    #[must_use]
    pub fn new() -> Self {
        Self {
            encode_tasks: Mutex::new(vec![]),
            decode_tasks: Mutex::new(vec![]),
            counter: Mutex::new(100),
            app_registry: None,
        }
    }

    /// 链式注入应用注册表（引擎门控开启：未安装 qrtransfer 应用 → 全部业务
    /// 端点 404；与 apps 组件 REST 面共享同一 SQLite，安装/卸载即时生效）。
    /// main.rs 生产装配恒调用；单测不注入则不门控（既有测试契约不变）。
    #[must_use]
    pub fn with_app_registry(mut self, reg: Arc<super::apps_handler::AppRegistry>) -> Self {
        self.app_registry = Some(reg);
        self
    }

    /// 当前全量编码任务快照。
    #[must_use]
    pub fn encode_tasks_snapshot(&self) -> Vec<EncodeTask> {
        self.encode_tasks
            .lock()
            .expect("encode_tasks poisoned")
            .clone()
    }

    /// 当前全量解码任务快照。
    #[must_use]
    pub fn decode_tasks_snapshot(&self) -> Vec<DecodeTask> {
        self.decode_tasks
            .lock()
            .expect("decode_tasks poisoned")
            .clone()
    }

    /// 生成下一个 id。
    fn next_id(&self, prefix: &str) -> String {
        let mut c = self.counter.lock().expect("counter poisoned");
        *c += 1;
        format!("{prefix}-{}", *c)
    }

    /// 统计快照。
    fn stats_snapshot(&self) -> QrStats {
        let enc = self.encode_tasks.lock().expect("encode_tasks poisoned");
        let dec = self.decode_tasks.lock().expect("decode_tasks poisoned");
        QrStats {
            encode_total: enc.len(),
            encode_pending: enc.iter().filter(|t| t.status == "pending").count(),
            encode_encoding: enc.iter().filter(|t| t.status == "encoding").count(),
            encode_completed: enc.iter().filter(|t| t.status == "completed").count(),
            encode_failed: enc.iter().filter(|t| t.status == "failed").count(),
            decode_total: dec.len(),
            decode_pending: dec.iter().filter(|t| t.status == "pending").count(),
            decode_decoding: dec.iter().filter(|t| t.status == "decoding").count(),
            decode_completed: dec.iter().filter(|t| t.status == "completed").count(),
            decode_failed: dec.iter().filter(|t| t.status == "failed").count(),
        }
    }

    /// 解析视频输出目录并保证其存在。
    ///
    /// 优先 `/tank/os-data/qr-videos`；不可写降级 `/tmp/os-qr-videos`。
    fn resolve_video_dir() -> String {
        for d in [QR_VIDEO_DIR, QR_VIDEO_DIR_FALLBACK] {
            if std::fs::create_dir_all(d).is_ok() {
                return d.trim_end_matches('/').to_string();
            }
        }
        QR_VIDEO_DIR_FALLBACK.to_string()
    }

    /// 解析解码输出目录并保证其存在。
    fn resolve_decoded_dir() -> String {
        for d in [QR_DECODED_DIR, QR_DECODED_DIR_FALLBACK] {
            if std::fs::create_dir_all(d).is_ok() {
                return d.trim_end_matches('/').to_string();
            }
        }
        QR_DECODED_DIR_FALLBACK.to_string()
    }
}

impl Default for QrTransferRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

/// 从解码脚本 stdout 解析 `decoded=N total_frames=M`。失败返回 (0,0)。
fn parse_decode_stdout(stdout: &[u8]) -> (u64, u64) {
    let s = String::from_utf8_lossy(stdout);
    let mut decoded = 0u64;
    let mut total = 0u64;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("OK ") {
            for token in rest.split_whitespace() {
                if let Some(v) = token.strip_prefix("decoded=") {
                    decoded = v.parse::<u64>().unwrap_or(0);
                } else if let Some(v) = token.strip_prefix("total_frames=") {
                    total = v.parse::<u64>().unwrap_or(0);
                }
            }
        }
    }
    (decoded, total)
}

#[async_trait]
impl RouteHandler for QrTransferRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            // —— 编码 ——
            spec(
                HttpMethod::Post,
                "/api/v1/qr/encode",
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Get, "/api/v1/qr/encode/:id", false, vec![]),
            spec(
                HttpMethod::Get,
                "/api/v1/qr/encode/:id/video",
                false,
                vec![],
            ),
            // —— 解码 ——
            spec(
                HttpMethod::Post,
                "/api/v1/qr/decode",
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Get, "/api/v1/qr/decode/:id", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/qr/decode/:id/file", false, vec![]),
            // —— 文本编解码（即时，无视频）——
            spec(
                HttpMethod::Post,
                "/api/v1/qr/encode-text",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/qr/decode-text",
                true,
                vec!["admin".into()],
            ),
            // —— 统计 ——
            spec(HttpMethod::Get, "/api/v1/qr/stats", false, vec![]),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        // —— 引擎门控（2026-09-05：二维码传输剥离为独立应用，docs/APPS.md §7）——
        // qr_transfer 引擎代码仍编译在 os-api（引擎内置），但未安装 qrtransfer
        // 应用时**零入口零可用**：全部业务端点 404 + 安装指引（语义对齐手机系统
        // 服务+应用）。每请求直查 apps 表（无缓存）——安装/卸载即时生效；表
        // 损坏/锁失败 fail-closed（按未装处理）。未注入注册表（单测直构）不
        // 门控，既有测试契约不变。
        if let Some(reg) = &self.app_registry {
            if !reg.is_engine_enabled("qrtransfer") {
                return Ok(error_response(
                    404,
                    "应用「二维码传输」未安装：可在 应用中心 → 商店 安装",
                ));
            }
        }
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            // ===================== 编码 =====================
            // —— POST /api/v1/qr/encode —— 创建编码任务
            (HttpMethod::Post, ["api", "v1", "qr", "encode"]) => {
                let body: EncodeBody = serde_json::from_value(req.body)
                    .map_err(|e| ApiGatewayError::Internal(format!("解析编码请求体失败: {e}")))?;
                if body.file_path.trim().is_empty() {
                    return Ok(error_response(400, "file_path 不可为空"));
                }
                let fps = body.fps.unwrap_or(DEFAULT_FPS).max(1);
                let chunk_size = body.chunk_size.unwrap_or(DEFAULT_CHUNK_SIZE).max(64);
                let file_path = body.file_path.trim().to_string();
                let id = self.next_id("qr-enc");
                // spawn_blocking 探测源文件 + 预创建输出目录（rustify：不再写 Python 脚本）
                let video_dir = Self::resolve_video_dir();
                let frames_dir = format!("/tmp/qr-frames/{id}");
                let video_path = format!("{video_dir}/{id}.mp4");
                let probe = tokio::task::spawn_blocking({
                    let file_path = file_path.clone();
                    let frames_dir = frames_dir.clone();
                    let video_dir = video_dir.clone();
                    move || -> (u64, Option<String>) {
                        let size = std::fs::metadata(&file_path).map(|m| m.len()).unwrap_or(0);
                        if !Path::new(&file_path).is_file() {
                            return (0, Some(format!("源文件不存在或不可读: {file_path}")));
                        }
                        let _ = std::fs::create_dir_all(&frames_dir);
                        let _ = std::fs::create_dir_all(&video_dir);
                        (size, None)
                    }
                })
                .await
                .unwrap_or((0, Some("编码任务探测失败".into())));
                let task = EncodeTask {
                    id: id.clone(),
                    file_path: file_path.clone(),
                    status: "pending".into(),
                    total_frames: 0,
                    file_size: probe.0,
                    fps,
                    chunk_size,
                    video_path: None,
                    video_url: None,
                    pid: None,
                    error: probe.1.clone(),
                    created_at: now_iso(),
                };
                let resp_body = to_value(&task)?;
                // 若探测已失败（源文件不存在），直接标 failed；否则后台跑纯 Rust 编码
                if probe.1.is_some() {
                    let mut t = task;
                    t.status = "failed".into();
                    let resp_body = to_value(&t)?;
                    self.encode_tasks
                        .lock()
                        .expect("encode_tasks poisoned")
                        .push(t);
                    Ok(ApiResponse {
                        status: 201,
                        body: resp_body,
                        headers: serde_json::json!({}),
                    })
                } else {
                    // fire-and-forget 后台任务（不持 self 引用；纯 Rust QR 帧生成 +
                    // ffmpeg 合成 MP4）。任务状态推进依赖 GET /encode/:id 时的
                    // refresh_encode 探测日志/视频文件是否存在；外部依赖缺失降级 failed。
                    spawn_encode_detached(
                        &id,
                        file_path.clone(),
                        fps,
                        chunk_size,
                        frames_dir.clone(),
                        video_path.clone(),
                    );
                    Ok(ApiResponse {
                        status: 201,
                        body: resp_body,
                        headers: serde_json::json!({}),
                    })
                }
            }

            // —— GET /api/v1/qr/encode/:id —— 任务状态 + 视频 URL
            (HttpMethod::Get, ["api", "v1", "qr", "encode", id]) => {
                self.refresh_encode(id).await;
                let task = {
                    let tasks = self.encode_tasks.lock().expect("encode_tasks poisoned");
                    tasks.iter().find(|t| t.id == *id).cloned()
                };
                match task {
                    Some(t) => Ok(ok_json(to_value(&t)?)),
                    None => Ok(error_response(404, &format!("编码任务不存在: {id}"))),
                }
            }

            // —— GET /api/v1/qr/encode/:id/video —— 下载/流式播放视频
            //
            // 网关响应体为 JSON，无法直接吐二进制字节流。这里返回 JSON 信封
            // {ok, video_url, video_path, size, content_type}；前端用 <video src=video_url>
            // 或 fetch(url).blob() 取真实字节。视频不存在 → 404。
            (HttpMethod::Get, ["api", "v1", "qr", "encode", id, "video"]) => {
                let snap = {
                    let tasks = self.encode_tasks.lock().expect("encode_tasks poisoned");
                    tasks.iter().find(|t| t.id == *id).cloned()
                };
                let task = match snap {
                    Some(t) => t,
                    None => return Ok(error_response(404, &format!("编码任务不存在: {id}"))),
                };
                let video_path = task.video_path.clone().unwrap_or_else(|| {
                    let dir = Self::resolve_video_dir();
                    format!("{dir}/{id}.mp4")
                });
                let exists_size = tokio::task::spawn_blocking({
                    let vp = video_path.clone();
                    move || std::fs::metadata(&vp).map(|m| m.len()).ok()
                })
                .await
                .unwrap_or(None);
                match exists_size {
                    Some(size) => Ok(ok_json(serde_json::json!({
                        "ok": true,
                        "id": id,
                        "video_path": video_path,
                        "video_url": format!("/api/v1/qr/encode/{id}/video"),
                        "size": size,
                        "content_type": "video/mp4",
                    }))),
                    None => Ok(error_response(
                        404,
                        &format!("视频尚未生成或不存在（任务状态: {}）", task.status),
                    )),
                }
            }

            // ===================== 解码 =====================
            // —— POST /api/v1/qr/decode —— 创建解码任务
            (HttpMethod::Post, ["api", "v1", "qr", "decode"]) => {
                let body: DecodeBody = serde_json::from_value(req.body)
                    .map_err(|e| ApiGatewayError::Internal(format!("解析解码请求体失败: {e}")))?;
                // 解析输入：file_path 优先；否则 media_base64 + filename 落盘
                let id = self.next_id("qr-dec");
                let upload_dir = "/tmp/qr-uploads";
                let (input_path, source_desc) = match body.file_path.as_deref().map(str::trim) {
                    Some(p) if !p.is_empty() => (p.to_string(), format!("path:{p}")),
                    _ => {
                        let b64 = match body.media_base64.as_deref().map(str::trim) {
                            Some(s) if !s.is_empty() => s,
                            _ => {
                                return Ok(error_response(
                                    400,
                                    "需提供 file_path 或 media_base64 之一",
                                ))
                            }
                        };
                        let fname = body
                            .filename
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| format!("{id}.bin"));
                        // spawn_blocking 落盘
                        let saved = tokio::task::spawn_blocking({
                            let b64 = b64.to_string();
                            let dir = upload_dir.to_string();
                            let fname = fname.clone();
                            move || -> Result<String, String> {
                                use base64::Engine;
                                let bytes = base64::engine::general_purpose::STANDARD
                                    .decode(b64.as_bytes())
                                    .map_err(|e| format!("media_base64 解码失败: {e}"))?;
                                let _ = std::fs::create_dir_all(&dir);
                                let p = format!("{dir}/{fname}");
                                std::fs::write(&p, &bytes)
                                    .map_err(|e| format!("写入上传文件失败: {e}"))?;
                                Ok(p)
                            }
                        })
                        .await
                        .unwrap_or(Err("上传落盘任务失败".into()));
                        match saved {
                            Ok(p) => (p.clone(), format!("upload:{fname}")),
                            Err(e) => {
                                let task = DecodeTask {
                                    id: id.clone(),
                                    status: "failed".into(),
                                    source: format!("upload:{}", body.filename.unwrap_or_default()),
                                    total_frames: 0,
                                    decoded_frames: 0,
                                    crc_ok: true,
                                    output_path: None,
                                    output_url: None,
                                    pid: None,
                                    error: Some(e),
                                    created_at: now_iso(),
                                };
                                let resp_body = to_value(&task)?;
                                self.decode_tasks
                                    .lock()
                                    .expect("decode_tasks poisoned")
                                    .push(task);
                                return Ok(ApiResponse {
                                    status: 201,
                                    body: resp_body,
                                    headers: serde_json::json!({}),
                                });
                            }
                        }
                    }
                };
                // rustify：不再写 Python 解码脚本；预检输入存在 + 预创建目录
                let frames_dir = format!("/tmp/qr-decode-frames/{id}");
                let decoded_dir = Self::resolve_decoded_dir();
                let output_path = format!("{decoded_dir}/{id}.bin");
                let write_res = tokio::task::spawn_blocking({
                    let input_path = input_path.clone();
                    let frames_dir = frames_dir.clone();
                    let output_path = output_path.clone();
                    move || -> Option<String> {
                        if !Path::new(&input_path).is_file() {
                            return Some(format!("输入媒体不存在或不可读: {input_path}"));
                        }
                        let _ = std::fs::create_dir_all(&frames_dir);
                        let _ = std::fs::create_dir_all(decoded_dir_placeholder(&output_path));
                        None
                    }
                })
                .await
                .unwrap_or(Some("解码任务准备失败".into()));
                let task = DecodeTask {
                    id: id.clone(),
                    status: "pending".into(),
                    source: source_desc,
                    total_frames: 0,
                    decoded_frames: 0,
                    crc_ok: true,
                    output_path: None,
                    output_url: None,
                    pid: None,
                    error: write_res.clone(),
                    created_at: now_iso(),
                };
                let resp_body = to_value(&task)?;
                if write_res.is_some() {
                    let mut t = task;
                    t.status = "failed".into();
                    let resp_body = to_value(&t)?;
                    self.decode_tasks
                        .lock()
                        .expect("decode_tasks poisoned")
                        .push(t);
                    Ok(ApiResponse {
                        status: 201,
                        body: resp_body,
                        headers: serde_json::json!({}),
                    })
                } else {
                    self.decode_tasks
                        .lock()
                        .expect("decode_tasks poisoned")
                        .push(task.clone());
                    spawn_decode_detached(
                        &id,
                        input_path.clone(),
                        frames_dir.clone(),
                        output_path.clone(),
                    );
                    Ok(ApiResponse {
                        status: 201,
                        body: resp_body,
                        headers: serde_json::json!({}),
                    })
                }
            }

            // —— GET /api/v1/qr/decode/:id —— 任务状态 + 输出文件路径
            (HttpMethod::Get, ["api", "v1", "qr", "decode", id]) => {
                self.refresh_decode(id).await;
                let task = {
                    let tasks = self.decode_tasks.lock().expect("decode_tasks poisoned");
                    tasks.iter().find(|t| t.id == *id).cloned()
                };
                match task {
                    Some(t) => Ok(ok_json(to_value(&t)?)),
                    None => Ok(error_response(404, &format!("解码任务不存在: {id}"))),
                }
            }

            // —— GET /api/v1/qr/decode/:id/file —— 下载解码后的文件
            //
            // 同 /video：返回 JSON 信封（{ok, output_path, output_url, size, content_type}），
            // 前端用 output_url fetch 取字节。文件不存在 → 404。
            (HttpMethod::Get, ["api", "v1", "qr", "decode", id, "file"]) => {
                let snap = {
                    let tasks = self.decode_tasks.lock().expect("decode_tasks poisoned");
                    tasks.iter().find(|t| t.id == *id).cloned()
                };
                let task = match snap {
                    Some(t) => t,
                    None => return Ok(error_response(404, &format!("解码任务不存在: {id}"))),
                };
                let output_path = task.output_path.clone().unwrap_or_else(|| {
                    let dir = Self::resolve_decoded_dir();
                    format!("{dir}/{id}.bin")
                });
                let exists_size = tokio::task::spawn_blocking({
                    let op = output_path.clone();
                    move || std::fs::metadata(&op).map(|m| m.len()).ok()
                })
                .await
                .unwrap_or(None);
                match exists_size {
                    Some(size) => Ok(ok_json(serde_json::json!({
                        "ok": true,
                        "id": id,
                        "output_path": output_path,
                        "output_url": format!("/api/v1/qr/decode/{id}/file"),
                        "size": size,
                        "content_type": "application/octet-stream",
                    }))),
                    None => Ok(error_response(
                        404,
                        &format!("输出文件尚未生成或不存在（任务状态: {}）", task.status),
                    )),
                }
            }

            // ===================== 文本编解码（即时，无视频）=====================
            // —— POST /api/v1/qr/encode-text —— 文本 -> 单/多张 QR 图片（base64）
            (HttpMethod::Post, ["api", "v1", "qr", "encode-text"]) => {
                let body: TextEncodeBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析文本编码请求体失败: {e}"))
                })?;
                if body.text.is_empty() {
                    return Ok(error_response(400, "text 不可为空"));
                }
                let error_level = body.error_level.unwrap_or_else(|| "L".to_string());
                let el = match error_level.as_str() {
                    "L" | "M" | "Q" | "H" => error_level,
                    _ => return Ok(error_response(400, "error_level 取值应为 L/M/Q/H")),
                };
                if body.text.len() > 50_000 {
                    return Ok(error_response(400, "文本超过50KB，请使用文件传输"));
                }
                let text = body.text;
                let original_size = text.len();
                // rustify：纯 Rust 生成。≤2953B 单张原文 QR；超限按 UTF-8 边界安全
                // 分块（原 gzip 压缩路径移除，data 为明文块），每块包 JSON header。
                let payloads: Vec<String> = if original_size <= QR_MAX_DATA {
                    vec![text.clone()]
                } else {
                    let chunk_budget = QR_MAX_DATA.saturating_sub(TEXT_HEADER_ROOM).max(1);
                    let chunks = split_text(&text, chunk_budget);
                    if chunks.len() > MAX_TEXT_QR_COUNT {
                        return Ok(error_response(400, "文本过大，建议使用文件传输"));
                    }
                    let total = chunks.len();
                    chunks
                        .iter()
                        .enumerate()
                        .map(|(seq, data)| {
                            serde_json::to_string(&serde_json::json!({
                                "seq": seq, "total": total, "data": data,
                            }))
                            .unwrap_or_default()
                        })
                        .collect()
                };
                let el = ec_level(&el);
                // QR 渲染为 CPU 密集操作 → spawn_blocking
                let gen = tokio::task::spawn_blocking(move || -> Result<Vec<String>, String> {
                    let mut out = Vec::with_capacity(payloads.len());
                    for p in &payloads {
                        let png = generate_qr_png(p, el)?;
                        out.push(base64::engine::general_purpose::STANDARD.encode(&png));
                    }
                    Ok(out)
                })
                .await
                .unwrap_or_else(|_| Err("QR 生成任务调度失败".into()));
                let qr_images = match gen {
                    Ok(v) => v,
                    Err(e) => return Ok(error_response(500, &e)),
                };
                // 无 gzip：compressed_size == original_size（保持响应字段兼容）
                Ok(ok_json(serde_json::json!({
                    "qr_count": qr_images.len(),
                    "qr_images": qr_images,
                    "original_size": original_size,
                    "compressed_size": original_size,
                })))
            }

            // —— POST /api/v1/qr/decode-text —— QR 图片(base64) -> 文本
            (HttpMethod::Post, ["api", "v1", "qr", "decode-text"]) => {
                let body: TextDecodeBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析文本解码请求体失败: {e}"))
                })?;
                let b64 = body.image_base64.trim().to_string();
                if b64.is_empty() {
                    return Ok(error_response(400, "image_base64 不可为空"));
                }
                // rustify：纯 Rust 解码（base64 → decode_qr → 识别分块协议），无临时文件
                let decoded =
                    tokio::task::spawn_blocking(move || -> Result<serde_json::Value, String> {
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(b64.as_bytes())
                            .map_err(|e| format!("image_base64 解码失败: {e}"))?;
                        let raw = decode_qr(&bytes)?;
                        // 尝试识别分块协议 header {"seq":N,"total":M,"data":"..."}
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                            let is_chunk = v.get("seq").is_some()
                                && v.get("total").is_some()
                                && v.get("data").is_some();
                            if is_chunk {
                                let seq = v.get("seq").and_then(|x| x.as_u64()).unwrap_or(0);
                                let total = v.get("total").and_then(|x| x.as_u64()).unwrap_or(0);
                                let data = v.get("data").and_then(|x| x.as_str()).unwrap_or("");
                                if total > 1 {
                                    // 多块之一：单张无法重组全文，返回 partial
                                    let head: String = data.chars().take(200).collect();
                                    return Ok(serde_json::json!({
                                        "partial": true, "seq": seq, "total": total, "text": head,
                                    }));
                                }
                                // 单块（total<=1）：data 即原文
                                return Ok(serde_json::json!({
                                    "text": data, "char_count": data.chars().count(),
                                }));
                            }
                        }
                        // 普通文本 QR → 原文返回
                        Ok(serde_json::json!({
                            "text": raw, "char_count": raw.chars().count(),
                        }))
                    })
                    .await
                    .unwrap_or_else(|_| Err("QR 解码任务调度失败".into()));
                match decoded {
                    Ok(v) => Ok(ok_json(v)),
                    Err(e) => Ok(error_response(500, &e)),
                }
            }

            // ===================== 统计 =====================
            // —— GET /api/v1/qr/stats —— 聚合统计
            (HttpMethod::Get, ["api", "v1", "qr", "stats"]) => {
                Ok(ok_json(to_value(&self.stats_snapshot())?))
            }

            _ => Ok(error_response(404, &format!("未知 QR 路由: {}", req.path))),
        }
    }
}

/// 从 output_path 推断其父目录（仅用于 create_dir_all）。
fn decoded_dir_placeholder(output_path: &str) -> String {
    Path::new(output_path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/tmp/os-qr-decoded".into())
}

/// 视频扩展名（解码端判断是否需 ffmpeg 拆帧；与原 Python 脚本一致）。
const VIDEO_EXTS: [&str; 11] = [
    ".mp4", ".mkv", ".mov", ".webm", ".avi", ".m4v", ".ts", ".wmv", ".flv", ".mpg", ".mpeg",
];

fn is_video_media(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    VIDEO_EXTS.iter().any(|e| lower.ends_with(e))
}

/// fire-and-forget 执行文件编码（纯 Rust QR 帧生成 + ffmpeg 合成 MP4 子进程）。
///
/// 与原 python3 子进程版语义一致：不持 handler 引用，结果只写文件系统
/// （`/tmp/qr_encode_<id>.log` + `<video_dir>/<id>.mp4`），任务状态推进依赖
/// GET /encode/:id 时的 [`QrTransferRouteHandler::refresh_encode`] 探测。
/// 任一步失败 → 日志写 `QR_ENCODE_FAILED: <原因>`（refresh 探测到即标 failed）。
fn spawn_encode_detached(
    task_id: &str,
    file_path: String,
    fps: u32,
    chunk_size: usize,
    frames_dir: String,
    video_path: String,
) {
    let task_id = task_id.to_string();
    tokio::spawn(async move {
        let log_path = format!("/tmp/qr_encode_{task_id}.log");
        // 1. QR 帧生成（CPU 密集 → spawn_blocking；纯 Rust，无 Python 子进程）
        let frames = tokio::task::spawn_blocking({
            let file_path = file_path.clone();
            let frames_dir = frames_dir.clone();
            move || -> Result<usize, String> {
                let raw = std::fs::read(&file_path).map_err(|e| format!("源文件读取失败: {e}"))?;
                use base64::Engine;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
                let chunk = if chunk_size == 0 { 1 } else { chunk_size };
                let total = if b64.is_empty() {
                    1
                } else {
                    b64.len().div_ceil(chunk)
                };
                std::fs::create_dir_all(&frames_dir).map_err(|e| format!("创建帧目录失败: {e}"))?;
                for seq in 0..total {
                    let start = seq * chunk;
                    let end = ((seq + 1) * chunk).min(b64.len());
                    let payload = encode_frame_payload(seq, total, &b64[start..end]);
                    let png = generate_qr_png(&payload, EcLevel::M)?;
                    std::fs::write(format!("{frames_dir}/frame_{seq:06}.png"), png)
                        .map_err(|e| format!("写帧失败: {e}"))?;
                }
                Ok(total)
            }
        })
        .await
        .unwrap_or_else(|e| Err(format!("编码任务调度失败: {e}")));
        let total = match frames {
            Ok(t) => t,
            Err(e) => {
                let _ = std::fs::write(&log_path, format!("QR_ENCODE_FAILED: {e}\n"));
                return;
            }
        };
        // 2. ffmpeg 合成 MP4（子进程保留：视频合成用 ffmpeg 合理）
        let out = tokio::process::Command::new("ffmpeg")
            .arg("-y")
            .arg("-framerate")
            .arg(fps.to_string())
            .arg("-i")
            .arg(format!("{frames_dir}/frame_%06d.png"))
            .arg("-c:v")
            .arg("libx264")
            .arg("-pix_fmt")
            .arg("yuv420p")
            .arg(&video_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .output()
            .await;
        match out {
            Ok(o) if o.status.success() => {
                let _ = std::fs::write(&log_path, format!("OK total={total} video={video_path}\n"));
            }
            Ok(o) => {
                let head: String = String::from_utf8_lossy(&o.stderr)
                    .chars()
                    .take(300)
                    .collect();
                let _ = std::fs::write(
                    &log_path,
                    format!("QR_ENCODE_FAILED: ffmpeg 合成失败: {head}\n"),
                );
            }
            Err(e) => {
                let _ = std::fs::write(
                    &log_path,
                    format!("QR_ENCODE_FAILED: ffmpeg 调用失败（未安装？）: {e}\n"),
                );
            }
        }
    });
}

/// fire-and-forget 执行文件解码（ffmpeg 拆帧保留 + 纯 Rust rqrr 逐帧识别）。
///
/// 结果写 `/tmp/qr_decode_<id>.log`（`OK decoded=N total_frames=M ...` /
/// `CRC_MISMATCH count=N` / `QR_DECODE_FAILED: <原因>`），状态推进依赖
/// [`QrTransferRouteHandler::refresh_decode`]。
fn spawn_decode_detached(
    task_id: &str,
    input_media: String,
    frames_dir: String,
    output_file: String,
) {
    let task_id = task_id.to_string();
    tokio::spawn(async move {
        let log_path = format!("/tmp/qr_decode_{task_id}.log");
        // 1. 视频先 ffmpeg 拆帧（子进程保留）；图片直接单帧
        let frame_files: Vec<String> = if is_video_media(&input_media) {
            let _ = std::fs::create_dir_all(&frames_dir);
            let out = tokio::process::Command::new("ffmpeg")
                .arg("-y")
                .arg("-i")
                .arg(&input_media)
                .arg("-vsync")
                .arg("0")
                .arg(format!("{frames_dir}/%06d.png"))
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .output()
                .await;
            match out {
                Ok(o) if o.status.success() => {}
                Ok(o) => {
                    let head: String = String::from_utf8_lossy(&o.stderr)
                        .chars()
                        .take(300)
                        .collect();
                    let _ = std::fs::write(
                        &log_path,
                        format!("QR_DECODE_FAILED: ffmpeg 拆帧失败: {head}\n"),
                    );
                    return;
                }
                Err(e) => {
                    let _ = std::fs::write(
                        &log_path,
                        format!("QR_DECODE_FAILED: ffmpeg 调用失败（未安装？）: {e}\n"),
                    );
                    return;
                }
            }
            let mut files: Vec<String> = std::fs::read_dir(&frames_dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("png"))
                        .map(|e| e.path().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            files.sort();
            files
        } else {
            vec![input_media.clone()]
        };
        if frame_files.is_empty() {
            let _ = std::fs::write(&log_path, "QR_DECODE_FAILED: 无可解码帧\n");
            return;
        }
        // 2. 逐帧 rqrr 解码 → 解析 {seq,total,crc,data} → 按 seq 重组（spawn_blocking）
        let total_frames = frame_files.len();
        let output_file_log = output_file.clone();
        let decoded = tokio::task::spawn_blocking(move || -> Result<(usize, usize), String> {
            use base64::Engine;
            use std::collections::BTreeMap;
            let mut pieces: BTreeMap<usize, String> = BTreeMap::new();
            let mut crc_bad = 0usize;
            let mut decoded_count = 0usize;
            for f in &frame_files {
                let bytes = match std::fs::read(f) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let raw = match decode_qr(&bytes) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                let v: serde_json::Value = match serde_json::from_str(&raw) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let Some(seq) = v.get("seq").and_then(|s| s.as_u64()) else {
                    continue;
                };
                let data = v.get("data").and_then(|d| d.as_str()).unwrap_or("");
                let claimed = v.get("crc").and_then(|c| c.as_str()).unwrap_or("");
                let actual = format!("{:08x}", crc32_ieee(data.as_bytes()));
                if !claimed.is_empty() && claimed != actual {
                    crc_bad += 1;
                }
                pieces.insert(seq as usize, data.to_string());
                decoded_count += 1;
            }
            if pieces.is_empty() {
                return Err("未从任何帧解码出 QR 数据".into());
            }
            let b64: String = pieces.values().map(String::as_str).collect();
            let raw_bytes = base64::engine::general_purpose::STANDARD
                .decode(b64.as_bytes())
                .map_err(|e| format!("Base64 重组解码失败: {e}"))?;
            if let Some(parent) = Path::new(&output_file).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(&output_file, &raw_bytes).map_err(|e| format!("写出文件失败: {e}"))?;
            Ok((decoded_count, crc_bad))
        })
        .await
        .unwrap_or_else(|e| Err(format!("解码任务调度失败: {e}")));
        match decoded {
            Ok((count, crc_bad)) => {
                let mut log = format!(
                    "OK decoded={count} total_frames={total_frames} pieces={count} out={output_file_log}\n"
                );
                if crc_bad > 0 {
                    log.push_str(&format!("CRC_MISMATCH count={crc_bad}\n"));
                }
                let _ = std::fs::write(&log_path, log);
            }
            Err(e) => {
                let _ = std::fs::write(&log_path, format!("QR_DECODE_FAILED: {e}\n"));
            }
        }
    });
}

impl QrTransferRouteHandler {
    /// 刷新单个编码任务状态：若 pending/encoding 且视频文件已生成 → completed；
    /// 若编码日志含 FAILED 标记 → failed。
    async fn refresh_encode(&self, id: &str) {
        let snap = {
            let tasks = self.encode_tasks.lock().expect("encode_tasks poisoned");
            tasks.iter().find(|t| t.id == id).cloned()
        };
        let Some(t) = snap else { return };
        if t.status == "completed" || t.status == "failed" {
            return;
        }
        let video_dir = Self::resolve_video_dir();
        let video_path = t
            .video_path
            .clone()
            .unwrap_or_else(|| format!("{video_dir}/{}.mp4", t.id));
        let log_path = format!("/tmp/qr_encode_{}.log", t.id);
        let task_id = t.id.clone();
        let probe = tokio::task::spawn_blocking(move || -> (bool, Option<String>) {
            let exists = Path::new(&video_path).is_file();
            let failed = std::fs::read_to_string(&log_path)
                .map(|s| s.contains("QR_ENCODE_FAILED"))
                .unwrap_or(false);
            let total = std::fs::read_to_string(&log_path)
                .ok()
                .and_then(|s| {
                    s.lines().find_map(|l| {
                        l.strip_prefix("OK total=").and_then(|x| {
                            x.split_whitespace()
                                .next()
                                .and_then(|n| n.parse::<u64>().ok())
                        })
                    })
                })
                .unwrap_or(0);
            if exists {
                (true, Some(format!("completed:{total}")))
            } else if failed {
                let detail = std::fs::read_to_string(&log_path)
                    .map(|s| {
                        s.lines()
                            .find(|l| l.contains("QR_ENCODE_FAILED"))
                            .unwrap_or("编码失败")
                            .trim()
                            .chars()
                            .take(300)
                            .collect::<String>()
                    })
                    .unwrap_or_else(|_| "编码失败".into());
                (false, Some(format!("failed:{detail}")))
            } else {
                (false, None)
            }
        })
        .await
        .unwrap_or((false, None));
        let mut tasks = self.encode_tasks.lock().expect("encode_tasks poisoned");
        if let Some(t) = tasks.iter_mut().find(|t| t.id == task_id) {
            if let Some(tag) = probe.1 {
                if let Some(rest) = tag.strip_prefix("completed:") {
                    t.status = "completed".into();
                    t.total_frames = rest.parse::<u64>().unwrap_or(0);
                    t.video_path = Some(format!("{video_dir}/{}.mp4", t.id));
                    t.video_url = Some(format!("/api/v1/qr/encode/{}/video", t.id));
                    t.pid = None;
                    t.error = None;
                } else if let Some(rest) = tag.strip_prefix("failed:") {
                    t.status = "failed".into();
                    t.error = Some(rest.to_string());
                    t.pid = None;
                }
            }
        }
    }

    /// 刷新单个解码任务状态（同 [`refresh_encode`] 语义，针对输出文件 + 解码日志）。
    async fn refresh_decode(&self, id: &str) {
        let snap = {
            let tasks = self.decode_tasks.lock().expect("decode_tasks poisoned");
            tasks.iter().find(|t| t.id == id).cloned()
        };
        let Some(t) = snap else { return };
        if t.status == "completed" || t.status == "failed" {
            return;
        }
        let decoded_dir = Self::resolve_decoded_dir();
        let output_path = t
            .output_path
            .clone()
            .unwrap_or_else(|| format!("{decoded_dir}/{}.bin", t.id));
        let log_path = format!("/tmp/qr_decode_{}.log", t.id);
        let task_id = t.id.clone();
        let probe = tokio::task::spawn_blocking(move || -> (bool, Option<String>) {
            let exists = Path::new(&output_path).is_file();
            let failed = std::fs::read_to_string(&log_path)
                .map(|s| s.contains("QR_DECODE_FAILED"))
                .unwrap_or(false);
            let crc_bad = std::fs::read_to_string(&log_path)
                .map(|s| s.contains("CRC_MISMATCH"))
                .unwrap_or(false);
            if exists {
                let (decoded, total) = std::fs::read_to_string(&log_path)
                    .ok()
                    .map(|s| parse_decode_stdout(s.as_bytes()))
                    .unwrap_or((0, 0));
                let crc_flag = if crc_bad { "0" } else { "1" };
                (
                    true,
                    Some(format!("completed:{decoded}:{total}:{crc_flag}")),
                )
            } else if failed {
                let detail = std::fs::read_to_string(&log_path)
                    .map(|s| {
                        s.lines()
                            .find(|l| l.contains("QR_DECODE_FAILED"))
                            .unwrap_or("解码失败")
                            .trim()
                            .chars()
                            .take(300)
                            .collect::<String>()
                    })
                    .unwrap_or_else(|_| "解码失败".into());
                (false, Some(format!("failed:{detail}")))
            } else {
                (false, None)
            }
        })
        .await
        .unwrap_or((false, None));
        let mut tasks = self.decode_tasks.lock().expect("decode_tasks poisoned");
        if let Some(t) = tasks.iter_mut().find(|t| t.id == task_id) {
            if let Some(tag) = probe.1 {
                if let Some(rest) = tag.strip_prefix("completed:") {
                    let mut parts = rest.split(':');
                    let decoded = parts
                        .next()
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(0);
                    let total = parts
                        .next()
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(0);
                    let crc_ok = parts.next().map(|s| s == "1").unwrap_or(true);
                    t.status = "completed".into();
                    t.decoded_frames = decoded;
                    t.total_frames = total;
                    t.crc_ok = crc_ok;
                    t.output_path = Some(format!("{decoded_dir}/{}.bin", t.id));
                    t.output_url = Some(format!("/api/v1/qr/decode/{}/file", t.id));
                    t.pid = None;
                    t.error = None;
                } else if let Some(rest) = tag.strip_prefix("failed:") {
                    t.status = "failed".into();
                    t.error = Some(rest.to_string());
                    t.pid = None;
                }
            }
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
        handler_component: "qr_transfer".to_string(),
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

    // ---- 路由表测试 ----

    #[tokio::test]
    async fn routes_count_is_nine_and_all_qr_transfer() {
        let h = QrTransferRouteHandler::new();
        let routes = h.routes().await;
        assert_eq!(routes.len(), 9, "QR 路由数应为 9，实际 {}", routes.len());
        for r in &routes {
            assert_eq!(
                r.handler_component,
                "qr_transfer",
                "路由 {} {} 未归属 qr_transfer",
                r.method_as_str(),
                r.path
            );
        }
        // 验证关键路径都存在
        let paths: Vec<&str> = routes.iter().map(|r| r.path.as_str()).collect();
        assert!(paths.contains(&"/api/v1/qr/encode"), "缺 POST encode");
        assert!(
            paths.contains(&"/api/v1/qr/encode/:id"),
            "缺 GET encode/:id"
        );
        assert!(
            paths.contains(&"/api/v1/qr/encode/:id/video"),
            "缺 GET encode/:id/video"
        );
        assert!(paths.contains(&"/api/v1/qr/decode"), "缺 POST decode");
        assert!(
            paths.contains(&"/api/v1/qr/decode/:id"),
            "缺 GET decode/:id"
        );
        assert!(
            paths.contains(&"/api/v1/qr/decode/:id/file"),
            "缺 GET decode/:id/file"
        );
        // 新增：文本编解码
        assert!(
            paths.contains(&"/api/v1/qr/encode-text"),
            "缺 POST encode-text"
        );
        assert!(
            paths.contains(&"/api/v1/qr/decode-text"),
            "缺 POST decode-text"
        );
        assert!(paths.contains(&"/api/v1/qr/stats"), "缺 GET stats");
    }

    #[tokio::test]
    async fn encode_routes_require_admin_auth() {
        let h = QrTransferRouteHandler::new();
        let routes = h.routes().await;
        let encode_post = routes
            .iter()
            .find(|r| r.method == HttpMethod::Post && r.path == "/api/v1/qr/encode")
            .expect("缺 POST /api/v1/qr/encode");
        assert!(encode_post.requires_auth, "encode POST 应需认证");
        assert!(
            encode_post.required_roles.iter().any(|r| r == "admin"),
            "encode POST 应需 admin 角色"
        );
        let decode_post = routes
            .iter()
            .find(|r| r.method == HttpMethod::Post && r.path == "/api/v1/qr/decode")
            .expect("缺 POST /api/v1/qr/decode");
        assert!(decode_post.requires_auth, "decode POST 应需认证");
        assert!(
            decode_post.required_roles.iter().any(|r| r == "admin"),
            "decode POST 应需 admin 角色"
        );
        // GET 路由不应需 admin
        let stats = routes
            .iter()
            .find(|r| r.path == "/api/v1/qr/stats")
            .expect("缺 GET /api/v1/qr/stats");
        assert!(!stats.requires_auth, "stats 不应需认证");
    }

    #[tokio::test]
    async fn text_routes_require_admin_auth() {
        let h = QrTransferRouteHandler::new();
        let routes = h.routes().await;
        let enc = routes
            .iter()
            .find(|r| r.method == HttpMethod::Post && r.path == "/api/v1/qr/encode-text")
            .expect("缺 POST /api/v1/qr/encode-text");
        assert!(enc.requires_auth, "encode-text 应需认证");
        assert!(
            enc.required_roles.iter().any(|r| r == "admin"),
            "encode-text 应需 admin 角色"
        );
        let dec = routes
            .iter()
            .find(|r| r.method == HttpMethod::Post && r.path == "/api/v1/qr/decode-text")
            .expect("缺 POST /api/v1/qr/decode-text");
        assert!(dec.requires_auth, "decode-text 应需认证");
        assert!(
            dec.required_roles.iter().any(|r| r == "admin"),
            "decode-text 应需 admin 角色"
        );
    }

    // ------------------------------------------------------------------
    // 引擎门控（2026-09-05：二维码传输剥离为独立应用——装了才启用）
    // ------------------------------------------------------------------

    /// 建一个声明 engine=qrtransfer 的应用裸仓库 fixture（真实 git），返回
    /// (AppRegistry, repo 名)——安装经 registry.install 真实 clone。id 与
    /// engine 刻意不同（qr-transfer / qrtransfer），验证门控键走 engine 列。
    async fn qr_app_registry(
        test: &str,
    ) -> (Arc<crate::handlers::apps_handler::AppRegistry>, String) {
        let dir = temp_dir_for(test);
        let repos = dir.join("repos");
        std::fs::create_dir_all(&repos).unwrap();
        let ok = |args: &[&str]| {
            matches!(
                std::process::Command::new(args[0]).args(&args[1..]).output(),
                Ok(o) if o.status.success()
            )
        };
        let bare = repos.join("nexos-app-qr-transfer.git");
        assert!(ok(&["git", "init", "--bare", bare.to_str().unwrap()]));
        // HEAD → main（code_repo 建仓同款；否则 clone 工作树为空）
        assert!(ok(&[
            "git",
            "--git-dir",
            bare.to_str().unwrap(),
            "symbolic-ref",
            "HEAD",
            "refs/heads/main"
        ]));
        let work = dir.join(".qr-work");
        std::fs::create_dir_all(work.join("web")).unwrap();
        std::fs::write(
            work.join("manifest.json"),
            serde_json::json!({
                "id": "qr-transfer",
                "name": "NexOS 二维码传输",
                "version": "0.1.0",
                "category": "tools",
                "icon": "🔡",
                "description": "文件 → 跳动 QR 视频 → 解码回文件（隔空/离线传输）",
                "entry": "web/entry.js",
                "engine": "qrtransfer",
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(work.join("web/entry.js"), "export default {}").unwrap();
        assert!(ok(&[
            "git",
            "-c",
            "init.defaultBranch=main",
            "init",
            work.to_str().unwrap()
        ]));
        assert!(ok(&["git", "-C", work.to_str().unwrap(), "add", "-A"]));
        assert!(ok(&[
            "git",
            "-C",
            work.to_str().unwrap(),
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "init"
        ]));
        assert!(ok(&[
            "git",
            "-C",
            work.to_str().unwrap(),
            "push",
            bare.to_str().unwrap(),
            "HEAD:main"
        ]));
        let _ = std::fs::remove_dir_all(&work);
        let reg = Arc::new(crate::handlers::apps_handler::AppRegistry::with_paths(
            dir.join("apps.db").to_str().unwrap(),
            dir.join("apps-root").to_str().unwrap(),
            repos.to_str().unwrap(),
        ));
        (reg, "nexos-app-qr-transfer".to_string())
    }

    /// 每测独立临时目录（进程 id + 测名唯一，防并行互踩；apps fixture 用）。
    fn temp_dir_for(test: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nexos-qr-{test}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn gating_blocks_all_qr_endpoints_until_app_installed() {
        let (reg, repo) = qr_app_registry("gate").await;
        let h = QrTransferRouteHandler::new().with_app_registry(Arc::clone(&reg));
        // 未安装 → 全部业务端点 404 + 精确安装指引文案（读 + 写都拦）
        for (method, path, body) in [
            (HttpMethod::Get, "/api/v1/qr/stats", serde_json::Value::Null),
            (
                HttpMethod::Get,
                "/api/v1/qr/encode/qr-1",
                serde_json::Value::Null,
            ),
            (
                HttpMethod::Get,
                "/api/v1/qr/decode/qr-1/file",
                serde_json::Value::Null,
            ),
            (
                HttpMethod::Post,
                "/api/v1/qr/encode-text",
                serde_json::json!({"text": "hello"}),
            ),
        ] {
            let req = ApiRequest {
                method,
                path: path.into(),
                headers: serde_json::json!({}),
                body,
                auth: None,
            };
            let resp = h.handle(req).await.unwrap();
            assert_eq!(resp.status, 404, "{path} 未装应 404: {resp:?}");
            assert_eq!(
                resp.body["error"].as_str().unwrap(),
                "应用「二维码传输」未安装：可在 应用中心 → 商店 安装",
                "{path} 文案: {resp:?}"
            );
        }
        // 被拦期间未落任何任务（encode_tasks 表仍空）
        assert!(
            h.encode_tasks_snapshot().is_empty(),
            "被拦期间不应建编码任务"
        );
        // fake 安装（真实 git clone）→ 门开：读 200 + 写放行（encode-text 即时成功）
        let (action, rec) = reg.install(&repo).await.expect("安装应成功");
        assert_eq!(action, "install");
        assert_eq!(rec.id, "qr-transfer");
        assert_eq!(rec.engine, "qrtransfer");
        let resp = h.handle(get_req("/api/v1/qr/stats")).await.unwrap();
        assert_eq!(resp.status, 200, "装后应放行: {resp:?}");
        let resp = h
            .handle(post_req(
                "/api/v1/qr/encode-text",
                serde_json::json!({"text": "hello"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "装后写端点放行: {resp:?}");
        // 卸载 → 即时回 404
        reg.uninstall("qr-transfer").expect("卸载应成功");
        let resp = h.handle(get_req("/api/v1/qr/stats")).await.unwrap();
        assert_eq!(resp.status, 404, "卸载即时生效: {resp:?}");
    }

    #[tokio::test]
    async fn qr_gating_inactive_without_registry_injection() {
        // 未注入注册表（既有单测直构形态）→ 不门控（兼容基线测试契约）
        let h = QrTransferRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/qr/stats")).await.unwrap();
        assert_eq!(resp.status, 200, "未注入不门控: {resp:?}");
    }

    // ---- 纯函数测试：split_text（UTF-8 边界安全）----

    #[test]
    fn split_text_short_returns_single_chunk() {
        let chunks = split_text("hello", 2953);
        assert_eq!(chunks.len(), 1, "短文本应为 1 块");
        assert_eq!(chunks[0], "hello");
        // 拼接还原
        assert_eq!(chunks.concat(), "hello");
    }

    #[test]
    fn split_text_long_multi_chunk_utf8_boundary() {
        // 构造长度刚好跨越块边界、且边界处是多字节中文字符的文本
        // 6000 字节 ASCII + 中文（每字 3 字节），按 1000 字节切
        let mut s = String::new();
        for _ in 0..1500 {
            s.push('A'); // 1500 字节 ASCII
        }
        s.push_str("中文测试边界安全"); // 9*3 = 27 字节
        let chunks = split_text(&s, 1000);
        assert!(chunks.len() > 1, "应切成多块，实际 {}", chunks.len());
        // 拼接必须字节级还原
        assert_eq!(chunks.concat(), s, "多块拼接应等于原文");
        // 每块（除末块）字节数 ≤ 1000
        for c in &chunks {
            assert!(c.len() <= 1000, "块长度 {} 超过预算 1000", c.len());
            // 每块必须是有效 UTF-8（String 天生有效，此处二次确认）
            assert!(
                std::str::from_utf8(c.as_bytes()).is_ok(),
                "块不是有效 UTF-8"
            );
        }
    }

    #[test]
    fn split_text_empty_returns_empty() {
        assert!(split_text("", 2953).is_empty(), "空文本应返回空 Vec");
        // size=0 不应死循环
        let chunks = split_text("abc", 0);
        assert_eq!(chunks.len(), 3, "size=0 时每字符一块（a/b/c）");
        assert_eq!(chunks.concat(), "abc");
    }

    // ---- 纯函数测试：build_text_encode_script ----

    // ---- 纯函数测试：build_text_decode_script ----

    // ---- 纯 Rust QR 编解码 roundtrip（rustify）----

    #[test]
    fn crc32_ieee_matches_zlib_test_vector() {
        // CRC-32/ISO-HDLC 标准测试向量：zlib.crc32(b"123456789") == 0xCBF43926
        assert_eq!(crc32_ieee(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32_ieee(b""), 0x0000_0000);
    }

    #[test]
    fn generate_then_decode_qr_roundtrip() {
        let png = generate_qr_png("hello rust qr", ec_level("L")).expect("QR 生成");
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']), "应为 PNG 魔数");
        let text = decode_qr(&png).expect("QR 解码");
        assert_eq!(text, "hello rust qr");
    }

    #[test]
    fn qr_roundtrip_frame_payload_protocol() {
        // 文件传输协议帧：{seq,total,crc,data} 生成 → 解码 → 解析校验
        let payload = encode_frame_payload(0, 1, "QUJDRA==");
        let png = generate_qr_png(&payload, EcLevel::M).expect("QR 生成");
        let decoded = decode_qr(&png).expect("QR 解码");
        let v: serde_json::Value = serde_json::from_str(&decoded).expect("应为 JSON payload");
        assert_eq!(v["seq"].as_u64(), Some(0));
        assert_eq!(v["total"].as_u64(), Some(1));
        assert_eq!(v["data"].as_str(), Some("QUJDRA=="));
        assert_eq!(
            v["crc"].as_str(),
            Some(format!("{:08x}", crc32_ieee(b"QUJDRA==")).as_str())
        );
    }

    #[tokio::test]
    async fn encode_text_then_decode_text_roundtrip() {
        let h = QrTransferRouteHandler::new();
        let enc = h
            .handle(post_req(
                "/api/v1/qr/encode-text",
                serde_json::json!({"text": "你好，世界！NexOS 纯 Rust QR", "error_level": "L"}),
            ))
            .await
            .unwrap();
        assert_eq!(enc.status, 200, "文本编码应 200: {}", enc.body);
        assert_eq!(enc.body["qr_count"].as_u64(), Some(1), "短文本应 1 张 QR");
        let img = enc.body["qr_images"][0]
            .as_str()
            .expect("qr_images[0]")
            .to_string();
        let dec = h
            .handle(post_req(
                "/api/v1/qr/decode-text",
                serde_json::json!({ "image_base64": img }),
            ))
            .await
            .unwrap();
        assert_eq!(dec.status, 200, "文本解码应 200: {}", dec.body);
        assert_eq!(
            dec.body["text"].as_str(),
            Some("你好，世界！NexOS 纯 Rust QR")
        );
    }

    #[tokio::test]
    async fn encode_text_multi_chunk_reports_partial_on_single_decode() {
        let h = QrTransferRouteHandler::new();
        // > 2953 字节 → 多块；50KB 上限内（约 4000 字节 → 2 块）
        let big = format!("{}尾巴", "x".repeat(3000));
        let enc = h
            .handle(post_req(
                "/api/v1/qr/encode-text",
                serde_json::json!({ "text": big }),
            ))
            .await
            .unwrap();
        assert_eq!(enc.status, 200, "多块编码应 200: {}", enc.body);
        let count = enc.body["qr_count"].as_u64().unwrap_or(0);
        assert!(count >= 2, "应至少 2 张 QR，实际 {count}");
        // 解码单张多块 QR → partial 分支
        let img = enc.body["qr_images"][0]
            .as_str()
            .expect("qr_images[0]")
            .to_string();
        let dec = h
            .handle(post_req(
                "/api/v1/qr/decode-text",
                serde_json::json!({ "image_base64": img }),
            ))
            .await
            .unwrap();
        assert_eq!(dec.status, 200, "partial 解码应 200: {}", dec.body);
        assert_eq!(dec.body["partial"].as_bool(), Some(true));
        assert_eq!(dec.body["total"].as_u64(), Some(count));
    }

    // ---- handler 校验测试（不依赖外部进程，快速失败）----

    #[tokio::test]
    async fn encode_text_empty_returns_400() {
        let h = QrTransferRouteHandler::new();
        let req = post_req("/api/v1/qr/encode-text", serde_json::json!({"text": ""}));
        let resp = h.handle(req).await.unwrap();
        assert_eq!(resp.status, 400, "空文本应 400");
    }

    #[tokio::test]
    async fn encode_text_invalid_error_level_returns_400() {
        let h = QrTransferRouteHandler::new();
        let req = post_req(
            "/api/v1/qr/encode-text",
            serde_json::json!({"text": "hi", "error_level": "X"}),
        );
        let resp = h.handle(req).await.unwrap();
        assert_eq!(resp.status, 400, "非法 error_level 应 400");
    }

    #[tokio::test]
    async fn encode_text_over_50kb_returns_400() {
        let h = QrTransferRouteHandler::new();
        // 50001 字节文本，超 50KB 上限，应在生成 QR 前拒绝
        let big = "a".repeat(50_001);
        let req = post_req("/api/v1/qr/encode-text", serde_json::json!({"text": big}));
        let resp = h.handle(req).await.unwrap();
        assert_eq!(resp.status, 400, "超 50KB 应 400");
        assert!(
            resp.body["error"].as_str().unwrap_or("").contains("50KB"),
            "error 应提示 50KB"
        );
    }

    #[tokio::test]
    async fn decode_text_empty_base64_returns_400() {
        let h = QrTransferRouteHandler::new();
        let req = post_req("/api/v1/qr/decode-text", serde_json::json!({}));
        let resp = h.handle(req).await.unwrap();
        assert_eq!(resp.status, 400, "空 image_base64 应 400");
    }

    // ---- helper 解析测试 ----

    // ---- 纯函数测试：build_encode_script ----

    // ---- 纯函数测试：build_decode_script ----

    // ---- 纯函数测试：split_chunks ----

    #[test]
    fn split_chunks_basic() {
        let data = b"abcdefghij"; // 10 字节
        let chunks = split_chunks(data, 3);
        assert_eq!(chunks.len(), 4, "10 字节按 3 切应为 4 块");
        assert_eq!(chunks[0], b"abc");
        assert_eq!(chunks[1], b"def");
        assert_eq!(chunks[2], b"ghi");
        assert_eq!(chunks[3], b"j"); // 末块不足 3
                                     // 拼接还原
        let restored: Vec<u8> = chunks.into_iter().flatten().collect();
        assert_eq!(restored, data);
    }

    #[test]
    fn split_chunks_empty_and_aligned() {
        assert!(split_chunks(b"", 100).is_empty(), "空数据应返回空 Vec");
        let chunks = split_chunks(b"abc", 3);
        assert_eq!(chunks.len(), 1, "3 字节按 3 切应为 1 块");
        assert_eq!(chunks[0], b"abc");
        // size=0 视为 1（不死循环）
        let chunks = split_chunks(b"ab", 0);
        assert_eq!(chunks.len(), 2, "size=0 时每字节一块");
    }

    // ---- 任务创建 / 统计 ----

    #[tokio::test]
    async fn encode_task_created_for_missing_file_marks_failed() {
        // 源文件不存在 → 任务直接 failed（不启动后台编码）
        let h = QrTransferRouteHandler::new();
        let req = post_req(
            "/api/v1/qr/encode",
            serde_json::json!({"file_path": "/tmp/__definitely_not_exists_qr_test__.bin"}),
        );
        let resp = h.handle(req).await.unwrap();
        assert_eq!(resp.status, 201, "缺失文件仍应 201 返回任务");
        let status = resp.body["status"].as_str().unwrap_or("");
        assert_eq!(status, "failed", "缺失文件应标 failed，实际 {status:?}");
        assert!(
            resp.body["error"].as_str().unwrap_or("").contains("不存在"),
            "error 应含'不存在'"
        );
        // 任务已入列表
        assert_eq!(h.encode_tasks_snapshot().len(), 1);
    }

    #[tokio::test]
    async fn encode_missing_file_path_returns_400() {
        let h = QrTransferRouteHandler::new();
        let req = post_req("/api/v1/qr/encode", serde_json::json!({}));
        let resp = h.handle(req).await.unwrap();
        assert_eq!(resp.status, 400, "缺 file_path 应 400");
    }

    #[tokio::test]
    async fn decode_requires_input_returns_400() {
        let h = QrTransferRouteHandler::new();
        let req = post_req("/api/v1/qr/decode", serde_json::json!({}));
        let resp = h.handle(req).await.unwrap();
        assert_eq!(resp.status, 400, "decode 缺输入应 400");
    }

    #[tokio::test]
    async fn stats_endpoint_returns_zero_counts_initially() {
        let h = QrTransferRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/qr/stats")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["encode_total"].as_u64(), Some(0));
        assert_eq!(resp.body["decode_total"].as_u64(), Some(0));
        assert_eq!(resp.body["encode_completed"].as_u64(), Some(0));
        assert_eq!(resp.body["decode_failed"].as_u64(), Some(0));
    }

    #[tokio::test]
    async fn unknown_route_returns_404() {
        let h = QrTransferRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/qr/unknown")).await.unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn encode_status_404_for_unknown_id() {
        let h = QrTransferRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/qr/encode/qr-enc-9999"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn decode_status_404_for_unknown_id() {
        let h = QrTransferRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/qr/decode/qr-dec-9999"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // ---- 辅助函数测试 ----

    #[test]
    fn parse_decode_stdout_extracts_counts() {
        let stdout = b"OK decoded=10 total_frames=12 pieces=10 out=/tmp/x.bin";
        let (d, t) = parse_decode_stdout(stdout);
        assert_eq!(d, 10);
        assert_eq!(t, 12);
    }

    // ---- 用于断言路由方法名的辅助 trait（仅测试用）----
    trait RouteSpecTestExt {
        fn method_as_str(&self) -> &'static str;
    }
    impl RouteSpecTestExt for RouteSpec {
        fn method_as_str(&self) -> &'static str {
            match self.method {
                HttpMethod::Get => "GET",
                HttpMethod::Post => "POST",
                HttpMethod::Put => "PUT",
                HttpMethod::Delete => "DELETE",
                HttpMethod::Patch => "PATCH",
            }
        }
    }
}
