//! `SurveillanceRouteHandler` —— 监控摄像头 HTTP→RTSP/ONVIF 真实拉流 + 录像适配器。
//!
//! 定位：把网关 HTTP 请求（`/api/v1/surveillance/*`）翻译为摄像头管理 + ffmpeg
//! 拉流/录像子进程编排，返回 JSON。这是 OS"应用类三件套"之一（监控摄像头）
//! 桌面应用的后端 REST 入口。
//!
//! # 实现策略：真实拉流 + 录像（从内存态升级）
//!
//! - **配置持久化**：摄像头列表序列化为 `/tank/os-data/cameras.json`（ZFS 池挂载点
//!   存在即用；否则回退 `./cameras.json`）。`new()` 启动时加载；`POST`/`DELETE`
//!   同步写回。重启后重置运行态（record_pid/stream_pid/hls_dir 清空，recording=false）。
//! - **实时拉流（stream）**：`POST /cameras/:id/stream` 真实 spawn ffmpeg
//!   `RTSP → HLS`（`-c:v libx264 -preset ultrafast -tune zerolatency -f hls ...`），
//!   输出到 `/tank/hls/<id>/index.m3u8`（/tank 不可写降级 `/tmp/os-hls/<id>/`）。
//!   pid 存 `stream_pid`，前端 `<video>` 播放该 m3u8。
//! - **录像（record）**：`POST /cameras/:id/record` 真实 spawn ffmpeg
//!   `RTSP → MP4`（`-c copy -f mp4 -movflags +faststart`）落盘到
//!   `/tank/recordings/<id>/<YYYYMMDD>/<HHmmss>.mp4`（/tank 不可写降级 `/tmp/...`）。
//! - **探测（probe）**：`POST /cameras/:id/probe` 同步运行
//!   `ffmpeg -rtsp_transport tcp -i <url> -t 1 -f null -`（最多等 ~8s），据退出码
//!   判定 online/offline，写入 `status`。ffmpeg 不存在/RTSP 不可达 → offline，不 panic。
//! - **降级**：ffmpeg 不存在 / spawn 失败 / RTSP 不可达 均**不 panic**，错误记入
//!   `Camera::error`，状态保持/回落。命令构造为纯函数（易测试、零依赖外部进程）。
//!
//! - **网段扫描（scan）**：`POST /surveillance/scan` 对 /24（~ /32）网段并发 TCP
//!   探测摄像头特征端口（554 RTSP / 80 Web / 8000 海康 Web / 8899 ONVIF 发现），
//!   50 并发、单连接 300ms、整体 8s 上限（超时返回已得部分 + `timed_out:true`）；
//!   端口签名推厂商（554+8000 海康 / 554+80 大华 / 仅 554 通用 / 8899 ONVIF）并给
//!   RTSP 模板；已在库 IP 标 `added:true`。缺省网段取本机默认路由 `prefsrc`
//!   （`ip -j -4 route show default`，network.rs 同款子进程风格）。
//! - **全局设置（settings）**：`GET/POST /surveillance/settings` 配置录像根目录
//!   `recording_dir`（初始默认 env `NEXOS_SURVEILLANCE_DIR` → `/tank/recordings`），
//!   持久化到 cameras.json 同目录 `surveillance-settings.json`；改路径只影响新录像，
//!   存量录像留在原路径且列表仍可见（多根扫描，不迁移）。
//! - **批量添加（batch）**：`POST /surveillance/cameras/batch` 一次添加 N 台
//!   （扫描结果多选 + 统一账号密码替换模板 `user:pass` 占位），逐台反馈成败，
//!   单台失败不影响其余，名字自动编号 `prefix-1..N`。
//! - **快照（snapshot）**：`POST /cameras/:id/snapshot` ffmpeg 抓单帧 JPEG 落
//!   `/tank/snapshots/<id>/latest.jpg`（降级 /tmp），`GET /cameras/:id/snapshot`
//!   返回 base64 data URL（前端卡片占位图直接展示）。
//! - **进程自愈（reconcile）**：`GET /cameras` 时经 `/proc/<pid>` 检查 stream/record
//!   子进程存活；ffmpeg 已退出的自动清 pid、回落 offline（前端占位图恢复）。
//! - **探测详情（probe_detail）**：probe 从 ffmpeg stderr 解析编码/分辨率
//!   （`Stream ... Video: h264 ..., 1920x1080`），随探测结果返回。
//!
//! # 路由表（16 条）
//!
//! | method | path                                            | 动作 |
//! |--------|-------------------------------------------------|------|
//! | GET    | `/api/v1/surveillance/cameras`                  | 列全部摄像头（自愈 pid）|
//! | POST   | `/api/v1/surveillance/cameras`                  | 添加（admin）|
//! | POST   | `/api/v1/surveillance/cameras/batch`            | 批量添加（admin）|
//! | DELETE | `/api/v1/surveillance/cameras/:id`              | 删除（admin，停录像+拉流）|
//! | POST   | `/api/v1/surveillance/cameras/:id/probe`        | 探测在线+流参数（admin）|
//! | POST   | `/api/v1/surveillance/cameras/:id/stream`       | 启动实时转码（admin）|
//! | POST   | `/api/v1/surveillance/cameras/:id/stop-stream`  | 停止实时转码（admin）|
//! | POST   | `/api/v1/surveillance/cameras/:id/record`       | 开始录像（admin）|
//! | POST   | `/api/v1/surveillance/cameras/:id/stop-record`  | 停止录像（admin）|
//! | GET    | `/api/v1/surveillance/cameras/:id/recordings`   | 列录像文件（含旧路径）|
//! | POST   | `/api/v1/surveillance/cameras/:id/snapshot`     | 抓快照（admin）|
//! | GET    | `/api/v1/surveillance/cameras/:id/snapshot`     | 看最近快照 |
//! | POST   | `/api/v1/surveillance/scan`                     | 网段扫描（admin）|
//! | GET    | `/api/v1/surveillance/settings`                 | 读设置+占用概览 |
//! | POST   | `/api/v1/surveillance/settings`                 | 改录像根目录（admin）|
//! | GET    | `/api/v1/surveillance/stats`                    | 统计（真实占用）|

use std::path::Path;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 一条摄像头。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Camera {
    pub id: String,
    pub name: String,
    /// rtsp://user:pass@ip:554/stream
    pub url: String,
    /// rtsp / onvif
    #[serde(default)]
    pub protocol: String,
    pub enabled: bool,
    /// offline / online / recording
    pub status: String,
    pub recording: bool,
    /// ffmpeg 录像进程 pid
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_pid: Option<u32>,
    /// ffmpeg 实时转码 pid
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_pid: Option<u32>,
    /// HLS 输出目录（实时观看用）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hls_dir: Option<String>,
    pub created_at: String,
    /// 最近一次错误（None = 无异常）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `GET /api/v1/surveillance/stats` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurveillanceStats {
    pub camera_count: usize,
    pub online: usize,
    pub recording: usize,
    pub storage_used_bytes: u64,
}

/// `GET /api/v1/surveillance/cameras/:id/recordings` 返回的单条录像文件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingEntry {
    /// 文件名（含扩展名，如 `091530.mp4`）
    pub name: String,
    /// 文件大小（字节）
    pub size_bytes: u64,
    /// 最后修改时间（ISO8601）
    pub modified_at: String,
    /// 文件绝对路径
    pub path: String,
    /// 日期目录名（`YYYYMMDD`）
    pub date: String,
}

/// 添加摄像头请求体。
#[derive(Debug, Deserialize)]
struct CreateBody {
    name: String,
    url: String,
    #[serde(default)]
    protocol: Option<String>,
}

/// `POST /api/v1/surveillance/scan` 请求体（subnet 缺省 → 本机默认路由网段）。
#[derive(Debug, Default, Deserialize)]
struct ScanBody {
    #[serde(default)]
    subnet: Option<String>,
}

/// 扫描结果条目：一个疑似摄像头。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanHit {
    pub ip: String,
    /// 开放的摄像头特征端口（升序，如 [80, 554]）
    pub ports: Vec<u16>,
    /// 端口签名推的厂商：hikvision / dahua / generic / onvif
    pub vendor_guess: String,
    /// 厂商 RTSP 模板（含 `user:pass` 占位，添加时替换）
    pub rtsp_template: String,
    /// 该 IP 是否已在本摄像头库中
    pub added: bool,
}

/// `POST /api/v1/surveillance/scan` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanReport {
    /// 规范化后的网段（主机位已掩掉，如 `192.0.2.0/24`）
    pub subnet: String,
    /// 扫描主机数（不含网络/广播地址）
    pub scanned: usize,
    /// 命中（疑似摄像头）数
    pub found: usize,
    /// 是否触达整体超时（true = 返回的是已得部分）
    pub timed_out: bool,
    pub hits: Vec<ScanHit>,
}

/// 全局设置（持久化 `surveillance-settings.json`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurveillanceSettings {
    /// 录像根目录（新录像写 `<dir>/<id>/<YYYYMMDD>/<HHmmss>.mp4`）
    pub recording_dir: String,
}

/// `POST /api/v1/surveillance/settings` 请求体。
#[derive(Debug, Deserialize)]
struct UpdateSettingsBody {
    recording_dir: String,
}

/// `POST /api/v1/surveillance/cameras/batch` 请求体。
///
/// `items` 来自扫描结果多选；`rtsp_url` 用厂商模板（含 `user:pass` 占位），
/// `username`/`password` 统一替换占位后逐台创建；名字自动 `prefix-1..N`。
#[derive(Debug, Deserialize)]
struct BatchBody {
    items: Vec<BatchItem>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    name_prefix: Option<String>,
}

/// 批量添加的单台条目（rtsp_url 缺省/为空 → 该台失败，不影响其余）。
#[derive(Debug, Deserialize)]
struct BatchItem {
    #[serde(default)]
    #[allow(dead_code)] // 元数据，便于日志/前端回显；创建逻辑不依赖
    ip: Option<String>,
    #[serde(default)]
    rtsp_url: Option<String>,
    #[serde(default)]
    #[allow(dead_code)] // 厂商猜测，仅回显用
    vendor: Option<String>,
}

/// 批量添加单台结果（逐台反馈）。
#[derive(Debug, Clone, Serialize)]
struct BatchItemResult {
    index: usize,
    ok: bool,
    name: String,
    url: String,
    error: Option<String>,
    camera_id: Option<String>,
}

/// `POST /api/v1/surveillance/cameras/batch` 响应。
#[derive(Debug, Clone, Serialize)]
struct BatchReport {
    created: usize,
    failed: usize,
    results: Vec<BatchItemResult>,
}

/// 从 ffmpeg stderr 解析出的视频流参数（probe_detail）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamInfo {
    /// 编码名（如 h264 / mjpeg）
    pub codec: String,
    /// 分辨率（如 1920x1080；解析不到为空串）
    pub resolution: String,
}

/// `GET/POST /cameras/:id/snapshot` 响应（JPEG base64 data URL）。
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotView {
    pub camera_id: String,
    pub path: String,
    pub modified_at: String,
    /// `data:image/jpeg;base64,...`（前端直接 `<img :src>`）
    pub data_url: String,
}

// ----------------------------------------------------------------------------
// FFmpeg 命令构造器（纯函数，易测试；不含 `ffmpeg` 程序名，caller 负责拼）
// ----------------------------------------------------------------------------

/// 实时转码命令：RTSP → HLS（前端 `<video>` 播放）。
///
/// `ffmpeg -rtsp_transport tcp -i <url> -c:v libx264 -preset ultrafast -tune zerolatency
///   -f hls -hls_time 2 -hls_list_size 5 -hls_flags delete_segments <hls_dir>/index.m3u8`
#[must_use]
pub fn build_stream_cmd(rtsp_url: &str, hls_dir: &str) -> Vec<String> {
    let index = format!("{}/index.m3u8", hls_dir.trim_end_matches('/'));
    vec![
        "-rtsp_transport".into(),
        "tcp".into(),
        "-i".into(),
        rtsp_url.into(),
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        "ultrafast".into(),
        "-tune".into(),
        "zerolatency".into(),
        "-f".into(),
        "hls".into(),
        "-hls_time".into(),
        "2".into(),
        "-hls_list_size".into(),
        "5".into(),
        "-hls_flags".into(),
        "delete_segments".into(),
        index,
    ]
}

/// 录像命令：RTSP → MP4 落盘（`-c copy` 零损耗转封装）。
///
/// `ffmpeg -rtsp_transport tcp -i <url> -c copy -f mp4 -movflags +faststart <output>`
#[must_use]
pub fn build_record_cmd(rtsp_url: &str, output_path: &str) -> Vec<String> {
    vec![
        "-rtsp_transport".into(),
        "tcp".into(),
        "-i".into(),
        rtsp_url.into(),
        "-c".into(),
        "copy".into(),
        "-f".into(),
        "mp4".into(),
        "-movflags".into(),
        "+faststart".into(),
        output_path.into(),
    ]
}

/// 探测命令：RTSP 是否可达（拉 1s 空输出）。
///
/// `ffmpeg -rtsp_transport tcp -i <url> -t 1 -f null -`
#[must_use]
pub fn build_probe_cmd(rtsp_url: &str) -> Vec<String> {
    vec![
        "-rtsp_transport".into(),
        "tcp".into(),
        "-i".into(),
        rtsp_url.into(),
        "-t".into(),
        "1".into(),
        "-f".into(),
        "null".into(),
        "-".into(),
    ]
}

/// 快照命令：RTSP 抓单帧 JPEG（`-frames:v 1` 单帧，`-q:v 2` 高质量小体积）。
///
/// `ffmpeg -rtsp_transport tcp -i <url> -frames:v 1 -q:v 2 <out.jpg>`
#[must_use]
pub fn build_snapshot_cmd(rtsp_url: &str, output_path: &str) -> Vec<String> {
    vec![
        "-rtsp_transport".into(),
        "tcp".into(),
        "-i".into(),
        rtsp_url.into(),
        "-frames:v".into(),
        "1".into(),
        "-q:v".into(),
        "2".into(),
        output_path.into(),
    ]
}

// ----------------------------------------------------------------------------
// 网段扫描（纯函数部分：解析 / 展开 / 签名 / 模板）
// ----------------------------------------------------------------------------

/// 摄像头特征端口（TCP 探测）：554=RTSP、80=Web、8000=海康 Web/ONVIF、8899=ONVIF 发现。
pub const CAMERA_PORTS: [u16; 4] = [554, 80, 8000, 8899];
/// 扫描并发上限（同时在飞的 IP 数；每 IP 4 端口并发）。
pub const SCAN_CONCURRENCY: usize = 50;
/// 单 TCP 连接超时。
pub const SCAN_CONNECT_TIMEOUT: Duration = Duration::from_millis(300);
/// 扫描整体上限（超时返回已得部分 + `timed_out:true`）。
pub const SCAN_OVERALL_TIMEOUT: Duration = Duration::from_secs(8);

/// 解析网段字符串为（网络基址，前缀长度）。仅 IPv4。
///
/// 接受 `"192.0.2.0/24"`、`"192.0.2.77/24"`（主机位被掩成 .0）与
/// `"192.0.2.77"`（缺省 /24）。前缀必须 ≥24（/24~/32，扫描规模 ≤256 台，
/// 保证整体 8s 内收敛）。非法输入返回 None（caller 400）。
#[must_use]
pub fn parse_subnet(s: &str) -> Option<(u32, u8)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (ip_part, prefix) = match s.split_once('/') {
        Some((ip, p)) => (ip, p.parse::<u8>().ok()?),
        None => (s, 24),
    };
    if !(24..=32).contains(&prefix) {
        return None;
    }
    let octets: Vec<&str> = ip_part.split('.').collect();
    if octets.len() != 4 {
        return None;
    }
    let mut ip: u32 = 0;
    for o in octets {
        if o.is_empty() || o.len() > 3 || !o.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let v: u32 = o.parse().ok()?;
        if v > 255 {
            return None;
        }
        ip = (ip << 8) | v;
    }
    let mask = u32::MAX << (32 - u32::from(prefix));
    Some((ip & mask, prefix))
}

/// 展开网段内可扫描主机（跳过网络地址与广播地址；/31、/32 全保留）。
#[must_use]
pub fn subnet_hosts(base: u32, prefix: u8) -> Vec<String> {
    let count: u64 = 1u64 << (32 - u32::from(prefix));
    let skip_ends = count > 2;
    (0..count)
        .filter(|&i| !skip_ends || (i != 0 && i != count - 1))
        .map(|i| ipv4_to_string(base + i as u32))
        .collect()
}

fn ipv4_to_string(v: u32) -> String {
    format!(
        "{}.{}.{}.{}",
        (v >> 24) & 0xff,
        (v >> 16) & 0xff,
        (v >> 8) & 0xff,
        v & 0xff
    )
}

fn ipv4_from_str(s: &str) -> Option<u32> {
    let (base, _) = parse_subnet(&format!("{s}/32"))?;
    Some(base)
}

/// 端口签名 → （厂商猜测, RTSP 模板）。
///
/// - 554+8000 → 海康（Web 8000 为海康特征端口）
/// - 554+80   → 大华
/// - 仅 554   → 通用（路径需人工确认）
/// - 8000（无 554）→ 海康（RTSP 或被过滤，模板仍按海康给）
/// - 8899    → ONVIF 标准设备（模板按通用 RTSP 给）
/// - 仅 80    → None（通用 Web 服务非摄像头特征，不收录，避免路由器刷屏）
#[must_use]
pub fn vendor_signature(ip: &str, ports: &[u16]) -> Option<(&'static str, String)> {
    let has = |p: u16| ports.contains(&p);
    if has(554) && has(8000) {
        Some((
            "hikvision",
            format!("rtsp://user:pass@{ip}:554/h264/ch1/main/av_stream"),
        ))
    } else if has(554) && has(80) {
        Some((
            "dahua",
            format!("rtsp://user:pass@{ip}:554/cam/realmonitor?channel=1&subtype=0"),
        ))
    } else if has(554) {
        Some(("generic", format!("rtsp://user:pass@{ip}:554/")))
    } else if has(8000) {
        Some((
            "hikvision",
            format!("rtsp://user:pass@{ip}:554/h264/ch1/main/av_stream"),
        ))
    } else if has(8899) {
        Some(("onvif", format!("rtsp://user:pass@{ip}:554/")))
    } else {
        None
    }
}

/// 从 RTSP/HTTP URL 提取主机 IP（剥 scheme / userinfo / 端口）。
/// `rtsp://admin:pw@10.0.0.5:554/x` → `10.0.0.5`；无 scheme 返回 None。
#[must_use]
pub fn extract_host_from_url(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return None;
    }
    let hostport = match authority.rsplit_once('@') {
        Some((_, h)) => h,
        None => authority,
    };
    let host = match hostport.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => hostport,
    };
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// URL userinfo 最小百分号编码（RFC3986 unreserved 之外全编码）。
fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 把模板里的 `user:pass` 占位替换为真实凭证（百分号编码后的 `user:pass`）。
/// 凭证为空 / 模板无占位则原样返回（幂等，不重复替换）。
#[must_use]
pub fn apply_credentials(url: &str, username: &str, password: &str) -> String {
    if !url.contains("user:pass") || (username.is_empty() && password.is_empty()) {
        return url.to_string();
    }
    url.replace(
        "user:pass",
        &format!(
            "{}:{}",
            encode_uri_component(username),
            encode_uri_component(password)
        ),
    )
}

/// 从 ffmpeg stderr 解析视频流参数（编码 + 分辨率）。
///
/// 匹配形如 `Stream #0:0: Video: h264 (High), yuv420p, 1920x1080 ..., 25 fps`
/// 的行：codec = `Video:` 后首个词；resolution = 行内首个 `NxM`（各 2-5 位数字）。
/// 无 Video 行 / 解析不到编码 → None。
#[must_use]
pub fn parse_stream_info(stderr: &str) -> Option<StreamInfo> {
    let video_line = stderr.lines().find(|l| l.contains("Video:"))?;
    let after = video_line.split("Video:").nth(1)?;
    let codec = after
        .trim()
        .split(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or("")
        .trim_end_matches(',')
        .to_string();
    if codec.is_empty() {
        return None;
    }
    let mut resolution = String::new();
    for tok in after.split_whitespace() {
        let t = tok.trim_end_matches(',');
        let parts: Vec<&str> = t.split('x').collect();
        if parts.len() == 2
            && parts
                .iter()
                .all(|p| (2..=5).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()))
        {
            resolution = t.to_string();
            break;
        }
    }
    Some(StreamInfo { codec, resolution })
}

// ----------------------------------------------------------------------------
// SurveillanceRouteHandler
// ----------------------------------------------------------------------------

/// 监控摄像头路由处理器——HTTP 边界适配到 RTSP 拉流 + 录像编排。
pub struct SurveillanceRouteHandler {
    cameras: Mutex<Vec<Camera>>,
    counter: Mutex<u64>,
    /// 落盘路径（`None` = 纯内存态，测试用；写操作不触盘）
    persist_path: Option<String>,
    /// 全局设置（录像根目录等）
    settings: Mutex<SurveillanceSettings>,
    /// 设置落盘路径（`None` = 纯内存态）
    settings_path: Option<String>,
}

impl SurveillanceRouteHandler {
    /// 构造 handler：加载 `cameras.json` 与 `surveillance-settings.json`
    /// （缺失/空 → 空列表 / env 默认），开启落盘。
    /// 加载后重置运行态（pid/hls_dir/recording 清空），因为重启后子进程已不可达。
    #[must_use]
    pub fn new() -> Self {
        let path = cameras_file_path();
        let cameras = normalize_loaded(load_cameras_from(&path));
        let settings_path = sibling_settings_path(&path);
        let settings = match &settings_path {
            Some(sp) => load_settings_from(sp),
            None => default_settings(),
        };
        Self {
            cameras: Mutex::new(cameras),
            counter: Mutex::new(100),
            persist_path: Some(path),
            settings: Mutex::new(settings),
            settings_path,
        }
    }

    /// 用指定摄像头列表构造（**纯内存态**：测试注入，不落盘、不触外部进程）。
    #[must_use]
    pub fn with_cameras(cameras: Vec<Camera>) -> Self {
        Self {
            cameras: Mutex::new(cameras),
            counter: Mutex::new(100),
            persist_path: None,
            settings: Mutex::new(default_settings()),
            settings_path: None,
        }
    }

    /// 用指定摄像头列表 + 显式落盘路径构造（持久化测试用；设置文件取同目录兄弟名）。
    #[must_use]
    pub fn with_cameras_path(cameras: Vec<Camera>, path: String) -> Self {
        let settings_path = sibling_settings_path(&path);
        let settings = match &settings_path {
            Some(sp) => load_settings_from(sp),
            None => default_settings(),
        };
        Self {
            cameras: Mutex::new(cameras),
            counter: Mutex::new(100),
            persist_path: Some(path),
            settings: Mutex::new(settings),
            settings_path,
        }
    }

    /// 当前全量摄像头快照。
    #[must_use]
    pub fn cameras_snapshot(&self) -> Vec<Camera> {
        self.cameras.lock().expect("cameras poisoned").clone()
    }

    /// 当前落盘路径（诊断用；纯内存态返回 `None`）。
    #[must_use]
    pub fn persist_path(&self) -> Option<&str> {
        self.persist_path.as_deref()
    }

    /// 生成下一个 id。
    fn next_id(&self) -> String {
        let mut c = self.counter.lock().expect("counter poisoned");
        *c += 1;
        format!("cam-{}", *c)
    }

    /// 同步把当前摄像头列表写回 JSON（仅当 `persist_path` 为 `Some`）。
    fn persist(&self) {
        if let Some(path) = &self.persist_path {
            let list = self.cameras.lock().expect("cameras poisoned").clone();
            if let Err(e) = save_cameras_to(path, &list) {
                eprintln!("[surveillance] 落盘失败 {path}: {e}");
            }
        }
    }

    /// 统计快照。storage 优先真实目录占用（recording_dir 递归 du）；
    /// 目录为空时回落"录制中每路估 4.5 GiB/小时"占位估算。
    fn stats_snapshot(&self) -> SurveillanceStats {
        let cameras = self.cameras.lock().expect("cameras poisoned");
        let mut online = 0usize;
        let mut recording = 0usize;
        for c in cameras.iter() {
            if c.status == "online" || c.status == "recording" {
                online += 1;
            }
            if c.recording || c.status == "recording" {
                recording += 1;
            }
        }
        drop(cameras);
        let dir = self
            .settings
            .lock()
            .expect("settings poisoned")
            .recording_dir
            .clone();
        let mut storage = dir_usage(&dir).0;
        if storage == 0 {
            storage = recording as u64 * 4_500_000_000;
        }
        SurveillanceStats {
            camera_count: self.cameras.lock().expect("cameras poisoned").len(),
            online,
            recording,
            storage_used_bytes: storage,
        }
    }

    /// 当前录像根目录快照（不 clone 结构体，避免锁穿透）。
    fn recording_dir(&self) -> String {
        self.settings
            .lock()
            .expect("settings poisoned")
            .recording_dir
            .clone()
    }

    /// 同步把设置写回 JSON（仅当 `settings_path` 为 `Some`）。
    fn persist_settings(&self) {
        if let Some(path) = &self.settings_path {
            let s = self.settings.lock().expect("settings poisoned").clone();
            if let Err(e) = save_settings_to(path, &s) {
                eprintln!("[surveillance] 设置落盘失败 {path}: {e}");
            }
        }
    }

    /// 进程自愈：清掉已退出的 ffmpeg 子进程运行态（`/proc/<pid>` 检测）。
    /// stream_pid 死 → 清 pid、online 回落 offline；record_pid 死 → 停录像标记。
    /// 只修内存态不落盘（重启加载时 normalize_loaded 会做同样的事）。
    fn reconcile_runtime(&self) {
        let mut cameras = self.cameras.lock().expect("cameras poisoned");
        for c in cameras.iter_mut() {
            if let Some(p) = c.stream_pid {
                if !pid_alive(p) {
                    c.stream_pid = None;
                    if c.status == "online" {
                        c.status = "offline".into();
                    }
                }
            }
            if let Some(p) = c.record_pid {
                if !pid_alive(p) {
                    c.record_pid = None;
                    c.recording = false;
                    if c.status == "recording" {
                        c.status = if c.stream_pid.is_some() {
                            "online".into()
                        } else {
                            "offline".into()
                        };
                    }
                }
            }
        }
    }

    /// 真实 spawn ffmpeg 子进程（fire-and-forget），成功返回 pid。
    /// ffmpeg 不存在 / spawn 失败返回 Err（caller 降级，不 panic）。
    fn spawn_ffmpeg(args: &[String]) -> Result<u32, String> {
        let mut cmd = std::process::Command::new("ffmpeg");
        cmd.args(args);
        cmd.stdout(Stdio::null());
        cmd.stdin(Stdio::null());
        // stderr → 临时日志（便于诊断 ffmpeg 失败原因）
        let stderr_log =
            std::env::temp_dir().join(format!("os-cam-ffmpeg-{}.log", std::process::id()));
        let stderr_file = std::fs::File::create(&stderr_log)
            .map(Stdio::from)
            .unwrap_or(Stdio::null());
        cmd.stderr(stderr_file);
        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id();
                drop(child); // 不等待：drop 后由 OS 收养（后台继续跑）
                Ok(pid)
            }
            Err(e) => Err(format!("spawn ffmpeg 失败: {e}")),
        }
    }

    /// 杀掉子进程（SIGTERM）。pid 无效/kill 失败返回 Err，但 caller 仍可继续。
    fn kill_pid(pid: u32) -> Result<(), String> {
        let out = std::process::Command::new("kill")
            .arg(pid.to_string())
            .output();
        match out {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => Err(format!(
                "kill {pid} 退出码 {:?}: {}",
                o.status.code(),
                String::from_utf8_lossy(&o.stderr).trim()
            )),
            Err(e) => Err(format!("kill {pid} 失败: {e}")),
        }
    }

    /// 同步探测 RTSP 是否可达：运行 `ffmpeg -t 1 -f null -`，最多等 ~8s。
    /// ffmpeg 退出码 0 → online；非 0 / spawn 失败 / 超时 → offline（带原因）。
    /// 同时从 stderr 解析视频流参数（编码/分辨率，见 [`parse_stream_info`]）。
    async fn probe_stream(rtsp_url: &str) -> (bool, Option<String>, Option<StreamInfo>) {
        let args = build_probe_cmd(rtsp_url);
        let fut = tokio::process::Command::new("ffmpeg")
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output();
        match tokio::time::timeout(Duration::from_secs(8), fut).await {
            Ok(Ok(o)) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                let info = parse_stream_info(&stderr);
                if o.status.success() {
                    (true, None, info)
                } else {
                    (
                        false,
                        Some(format!("探测失败（退出码 {:?}）", o.status.code())),
                        info,
                    )
                }
            }
            Ok(Err(e)) => (false, Some(format!("ffmpeg 不可用: {e}")), None),
            Err(_) => (false, Some("探测超时（>8s）".into()), None),
        }
    }

    /// 解析 HLS 输出目录并保证其存在。
    /// 优先 `/tank/hls/<id>`；不可写降级 `/tmp/os-hls/<id>`。返回 (实际目录, Option<warning>)。
    #[must_use]
    fn resolve_hls_dir(camera_id: &str) -> (String, Option<String>) {
        let pref = format!("/tank/hls/{camera_id}");
        if std::fs::create_dir_all(&pref).is_ok() {
            return (pref, None);
        }
        let fb = format!("/tmp/os-hls/{camera_id}");
        let _ = std::fs::create_dir_all(&fb);
        (
            fb.clone(),
            Some(format!("无法创建 HLS 目录 {pref}（降级到 {fb}）")),
        )
    }

    /// 生成录像文件全路径并保证其父目录存在（基于配置的录像根目录）。
    /// `<base>/<id>/<YYYYMMDD>/<HHmmss>.mp4`（base 不可写降级 `/tmp/recordings/...`）。
    /// 返回 (全路径, Option<warning>)。
    #[must_use]
    fn record_filepath_in(base_dir: &str, camera_id: &str) -> (String, Option<String>) {
        use chrono::Local;
        let date = Local::now().format("%Y%m%d").to_string();
        let ts = Local::now().format("%H%M%S").to_string();
        let fname = format!("{ts}.mp4");
        let pref_dir = format!("{}/{camera_id}/{date}", base_dir.trim_end_matches('/'));
        if std::fs::create_dir_all(&pref_dir).is_ok() {
            return (format!("{pref_dir}/{fname}"), None);
        }
        let fb_dir = format!("/tmp/recordings/{camera_id}/{date}");
        let _ = std::fs::create_dir_all(&fb_dir);
        (
            format!("{fb_dir}/{fname}"),
            Some(format!("无法创建录像目录 {pref_dir}（降级到 {fb_dir}）")),
        )
    }

    /// 扫描某摄像头的录像文件：多个根目录（当前配置 + 历史 `/tank/recordings` +
    /// 降级 `/tmp/recordings`）去重合并，递归 `<date>/*.mp4`，
    /// 返回文件清单（含 name/size/mtime/path/date）。
    /// 目录不存在/不可读返回空 Vec（不 panic）。
    #[must_use]
    fn scan_recordings_in(roots: &[String], camera_id: &str) -> Vec<RecordingEntry> {
        let mut dedup: Vec<String> = Vec::new();
        for r in roots {
            let r = r.trim_end_matches('/').to_string();
            if !r.is_empty() && !dedup.contains(&r) {
                dedup.push(r);
            }
        }
        let mut out = Vec::new();
        for root in dedup {
            let base = Path::new(&root).join(camera_id);
            if !base.is_dir() {
                continue;
            }
            let date_dirs = match std::fs::read_dir(&base) {
                Ok(d) => d,
                Err(_) => continue,
            };
            for de in date_dirs.flatten() {
                let dp = de.path();
                if !dp.is_dir() {
                    continue;
                }
                let date = dp
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                let files = match std::fs::read_dir(&dp) {
                    Ok(d) => d,
                    Err(_) => continue,
                };
                for fe in files.flatten() {
                    let fp = fe.path();
                    if !fp.is_file() {
                        continue;
                    }
                    let ext = fp
                        .extension()
                        .and_then(|s| s.to_str())
                        .map(|s| s.to_ascii_lowercase())
                        .unwrap_or_default();
                    if ext != "mp4" {
                        continue;
                    }
                    let name = fp
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string();
                    let meta = std::fs::metadata(&fp).ok();
                    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
                    let mtime = meta
                        .as_ref()
                        .and_then(|m| m.modified().ok())
                        .map(systemtime_to_iso)
                        .unwrap_or_default();
                    out.push(RecordingEntry {
                        name,
                        size_bytes: size,
                        modified_at: mtime,
                        path: fp.to_string_lossy().into_owned(),
                        date: date.clone(),
                    });
                }
            }
        }
        // 日期降序、文件名升序（前端"最新在上"）
        out.sort_by(|a, b| b.date.cmp(&a.date).then(a.name.cmp(&b.name)));
        out
    }

    /// 录像根目录集合：当前配置 + 历史默认 + 降级目录（存量录像不迁移仍可见）。
    fn recording_roots(&self) -> Vec<String> {
        vec![
            self.recording_dir(),
            "/tank/recordings".into(),
            "/tmp/recordings".into(),
        ]
    }

    // ------------------------------------------------------------------
    // 网段扫描（核心编排；探测函数可注入，测试不触真实网络）
    // ------------------------------------------------------------------

    /// 真实 TCP 探测：单 IP 的 4 个特征端口并发 connect，各 300ms 超时。
    /// 返回开放端口（升序）。connect 超时/拒绝 = 该端口关闭（不报错）。
    fn tcp_probe(ip: String) -> futures::future::BoxFuture<'static, Vec<u16>> {
        Box::pin(async move {
            let futs: Vec<_> = CAMERA_PORTS
                .iter()
                .map(|&p| {
                    let ip = ip.clone();
                    async move {
                        let ok = tokio::time::timeout(
                            SCAN_CONNECT_TIMEOUT,
                            tokio::net::TcpStream::connect((ip.as_str(), p)),
                        )
                        .await
                        .map(|r| r.is_ok())
                        .unwrap_or(false);
                        (p, ok)
                    }
                })
                .collect();
            let mut open: Vec<u16> = futures::future::join_all(futs)
                .await
                .into_iter()
                .filter(|(_, ok)| *ok)
                .map(|(p, _)| p)
                .collect();
            open.sort_unstable();
            open
        })
    }

    /// 扫描一个网段（探测函数注入版）：/24 展开 → 50 并发探测 → 端口签名
    /// 过滤 + 厂商模板 + added 标注 → 整体超时返回已得部分。
    /// 探测函数签名为 `Fn(ip) -> Future<Vec<u16>>`（开放特征端口）。
    async fn scan_subnet_with(
        subnet: &str,
        added_ips: std::collections::HashSet<String>,
        overall: Duration,
        probe: std::sync::Arc<
            dyn Fn(String) -> futures::future::BoxFuture<'static, Vec<u16>> + Send + Sync,
        >,
    ) -> Result<ScanReport, String> {
        let (base, prefix) = parse_subnet(subnet).ok_or_else(|| {
            format!("无法解析网段 {subnet:?}（形如 192.0.2.0/24，仅支持 /24 ~ /32）")
        })?;
        let hosts = subnet_hosts(base, prefix);
        let scanned = hosts.len();
        let hits: std::sync::Arc<Mutex<Vec<ScanHit>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
        use futures::StreamExt;
        let mut stream = futures::stream::iter(hosts)
            .map(|ip| {
                let p = probe.clone();
                async move {
                    let ports = p(ip.clone()).await;
                    (ip, ports)
                }
            })
            .buffer_unordered(SCAN_CONCURRENCY);
        let collector = hits.clone();
        let timed_out = tokio::time::timeout(overall, async {
            while let Some((ip, ports)) = stream.next().await {
                if let Some((vendor, tpl)) = vendor_signature(&ip, &ports) {
                    collector.lock().expect("scan hits poisoned").push(ScanHit {
                        ip: ip.clone(),
                        ports,
                        vendor_guess: vendor.to_string(),
                        rtsp_template: tpl,
                        added: added_ips.contains(&ip),
                    });
                }
            }
        })
        .await
        .is_err();
        let mut hit_vec = hits.lock().expect("scan hits poisoned").clone();
        hit_vec.sort_by_key(|h| ipv4_from_str(&h.ip).unwrap_or(0));
        Ok(ScanReport {
            subnet: format!("{}/{}", ipv4_to_string(base), prefix),
            scanned,
            found: hit_vec.len(),
            timed_out,
            hits: hit_vec,
        })
    }

    // ------------------------------------------------------------------
    // 快照
    // ------------------------------------------------------------------

    /// 解析快照目录并保证其存在：`/tank/snapshots/<id>`（降级 `/tmp/os-snapshots/<id>`）。
    #[must_use]
    fn snapshot_dir_candidates(camera_id: &str) -> [String; 2] {
        [
            format!("/tank/snapshots/{camera_id}"),
            format!("/tmp/os-snapshots/{camera_id}"),
        ]
    }

    /// 读取现有 latest.jpg 为 SnapshotView（data URL）。文件不存在返回 None。
    fn read_latest_snapshot(camera_id: &str) -> Option<SnapshotView> {
        use base64::Engine as _;
        for dir in Self::snapshot_dir_candidates(camera_id) {
            let f = format!("{}/latest.jpg", dir.trim_end_matches('/'));
            let Ok(bytes) = std::fs::read(&f) else {
                continue;
            };
            let mtime = std::fs::metadata(&f)
                .ok()
                .and_then(|m| m.modified().ok())
                .map(systemtime_to_iso)
                .unwrap_or_default();
            return Some(SnapshotView {
                camera_id: camera_id.to_string(),
                path: f,
                modified_at: mtime,
                data_url: format!(
                    "data:image/jpeg;base64,{}",
                    base64::engine::general_purpose::STANDARD.encode(bytes)
                ),
            });
        }
        None
    }
}

impl Default for SurveillanceRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RouteHandler for SurveillanceRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec(
                HttpMethod::Get,
                "/api/v1/surveillance/cameras",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/surveillance/cameras",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/surveillance/cameras/:id",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/surveillance/cameras/:id/probe",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/surveillance/cameras/:id/stream",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/surveillance/cameras/:id/stop-stream",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/surveillance/cameras/:id/record",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/surveillance/cameras/:id/stop-record",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/surveillance/cameras/:id/recordings",
                false,
                vec![],
            ),
            spec(HttpMethod::Get, "/api/v1/surveillance/stats", false, vec![]),
            // ===================== 网段扫描 / 全局设置 / 批量添加 / 快照 =====================
            spec(
                HttpMethod::Post,
                "/api/v1/surveillance/scan",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/surveillance/settings",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/surveillance/settings",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/surveillance/cameras/batch",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/surveillance/cameras/:id/snapshot",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/surveillance/cameras/:id/snapshot",
                false,
                vec![],
            ),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            // ===================== 列表 / 统计 =====================
            // —— GET /api/v1/surveillance/cameras —— 列全部（先自愈已退出的子进程）
            (HttpMethod::Get, ["api", "v1", "surveillance", "cameras"]) => {
                self.reconcile_runtime();
                Ok(ok_json(to_value(&self.cameras_snapshot())?))
            }

            // —— GET /api/v1/surveillance/stats —— 统计
            (HttpMethod::Get, ["api", "v1", "surveillance", "stats"]) => {
                Ok(ok_json(to_value(&self.stats_snapshot())?))
            }

            // —— GET /api/v1/surveillance/cameras/:id/recordings —— 列录像文件
            // 多根合并（当前配置 + 历史 /tank + 降级 /tmp）：改路径后存量录像仍可见
            (HttpMethod::Get, ["api", "v1", "surveillance", "cameras", id, "recordings"]) => {
                let entries = Self::scan_recordings_in(&self.recording_roots(), id);
                Ok(ok_json(to_value(&entries)?))
            }

            // ===================== 增删 =====================
            // —— POST /api/v1/surveillance/cameras —— 添加
            (HttpMethod::Post, ["api", "v1", "surveillance", "cameras"]) => {
                let body: CreateBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析添加摄像头请求体失败: {e}"))
                })?;
                if body.name.trim().is_empty() {
                    return Ok(error_response(400, "name 不可为空"));
                }
                if body.url.trim().is_empty() {
                    return Ok(error_response(400, "url 不可为空"));
                }
                let protocol = body
                    .protocol
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| infer_protocol(&body.url));
                let cam = Camera {
                    id: self.next_id(),
                    name: body.name,
                    url: body.url,
                    protocol,
                    enabled: true,
                    status: "offline".into(),
                    recording: false,
                    record_pid: None,
                    stream_pid: None,
                    hls_dir: None,
                    created_at: now_iso(),
                    error: None,
                };
                let resp_body = to_value(&cam)?;
                self.cameras.lock().expect("cameras poisoned").push(cam);
                self.persist();
                Ok(ApiResponse {
                    status: 201,
                    body: resp_body,
                    headers: serde_json::json!({}),
                })
            }

            // —— DELETE /api/v1/surveillance/cameras/:id —— 删除（停录像+拉流）
            (HttpMethod::Delete, ["api", "v1", "surveillance", "cameras", id]) => {
                let mut cameras = self.cameras.lock().expect("cameras poisoned");
                // 先 kill 运行中的录像/拉流子进程（杀不掉也继续删）
                if let Some(c) = cameras.iter().find(|c| c.id == *id) {
                    if let Some(p) = c.record_pid {
                        let _ = Self::kill_pid(p);
                    }
                    if let Some(p) = c.stream_pid {
                        let _ = Self::kill_pid(p);
                    }
                }
                let before = cameras.len();
                cameras.retain(|c| c.id != *id);
                if cameras.len() == before {
                    return Ok(error_response(404, &format!("摄像头不存在: {id}")));
                }
                drop(cameras);
                self.persist();
                Ok(ok_json(
                    serde_json::json!({"ok": true, "id": id, "action": "delete"}),
                ))
            }

            // ===================== 探测 =====================
            // —— POST /api/v1/surveillance/cameras/:id/probe —— 探测是否在线 + 流参数
            (HttpMethod::Post, ["api", "v1", "surveillance", "cameras", id, "probe"]) => {
                // 先快照 url（锁立即释放，避免 await probe 时持锁）
                let url = {
                    let cameras = self.cameras.lock().expect("cameras poisoned");
                    cameras.iter().find(|c| c.id == *id).map(|c| c.url.clone())
                };
                let Some(url) = url else {
                    return Ok(error_response(404, &format!("摄像头不存在: {id}")));
                };
                let (online, err, info) = Self::probe_stream(&url).await;
                let mut cameras = self.cameras.lock().expect("cameras poisoned");
                let Some(c) = cameras.iter_mut().find(|c| c.id == *id) else {
                    return Ok(error_response(404, &format!("摄像头不存在: {id}")));
                };
                c.error = err;
                if online {
                    // 录制中保持 recording；否则标记 online
                    if !c.recording {
                        c.status = "online".into();
                    } else if c.status == "offline" {
                        c.status = "recording".into();
                    }
                } else {
                    c.status = "offline".into();
                }
                let mut body = to_value(&*c)?;
                // 附加流参数详情（诊断信息，不落库）
                body["probe_detail"] = serde_json::json!({
                    "online": online,
                    "codec": info.as_ref().map(|i| i.codec.clone()),
                    "resolution": info.as_ref().map(|i| i.resolution.clone()),
                });
                drop(cameras);
                Ok(ok_json(body))
            }

            // ===================== 实时拉流 =====================
            // —— POST /api/v1/surveillance/cameras/:id/stream —— 启动实时转码
            (HttpMethod::Post, ["api", "v1", "surveillance", "cameras", id, "stream"]) => {
                let snap = {
                    let cameras = self.cameras.lock().expect("cameras poisoned");
                    cameras
                        .iter()
                        .find(|c| c.id == *id)
                        .map(|c| (c.id.clone(), c.url.clone()))
                };
                let Some((cid, url)) = snap else {
                    return Ok(error_response(404, &format!("摄像头不存在: {id}")));
                };
                let (hls_dir, mut warn) = Self::resolve_hls_dir(&cid);
                let cmd = build_stream_cmd(&url, &hls_dir);
                let mut cameras = self.cameras.lock().expect("cameras poisoned");
                let Some(c) = cameras.iter_mut().find(|c| c.id == cid) else {
                    return Ok(error_response(404, &format!("摄像头不存在: {id}")));
                };
                match Self::spawn_ffmpeg(&cmd) {
                    Ok(pid) => {
                        c.stream_pid = Some(pid);
                        c.hls_dir = Some(hls_dir);
                        if c.status == "offline" {
                            c.status = "online".into();
                        }
                        c.error = warn;
                    }
                    Err(e) => {
                        c.error = Some(match warn.take() {
                            Some(w) => format!("{w}; {e}"),
                            None => e,
                        });
                    }
                }
                Ok(ok_json(to_value(c)?))
            }

            // —— POST /api/v1/surveillance/cameras/:id/stop-stream —— 停止实时转码
            (HttpMethod::Post, ["api", "v1", "surveillance", "cameras", id, "stop-stream"]) => {
                let mut cameras = self.cameras.lock().expect("cameras poisoned");
                let Some(c) = cameras.iter_mut().find(|c| c.id == *id) else {
                    return Ok(error_response(404, &format!("摄像头不存在: {id}")));
                };
                if let Some(pid) = c.stream_pid.take() {
                    let _ = Self::kill_pid(pid);
                }
                // 不再实时观看：若未在录制则降回 offline
                if !c.recording && c.status == "online" {
                    c.status = "offline".into();
                }
                Ok(ok_json(to_value(c)?))
            }

            // ===================== 录像 =====================
            // —— POST /api/v1/surveillance/cameras/:id/record —— 开始录像
            (HttpMethod::Post, ["api", "v1", "surveillance", "cameras", id, "record"]) => {
                let snap = {
                    let cameras = self.cameras.lock().expect("cameras poisoned");
                    cameras
                        .iter()
                        .find(|c| c.id == *id)
                        .map(|c| (c.id.clone(), c.url.clone()))
                };
                let Some((cid, url)) = snap else {
                    return Ok(error_response(404, &format!("摄像头不存在: {id}")));
                };
                let (outfile, mut warn) = Self::record_filepath_in(&self.recording_dir(), &cid);
                let cmd = build_record_cmd(&url, &outfile);
                let mut cameras = self.cameras.lock().expect("cameras poisoned");
                let Some(c) = cameras.iter_mut().find(|c| c.id == cid) else {
                    return Ok(error_response(404, &format!("摄像头不存在: {id}")));
                };
                match Self::spawn_ffmpeg(&cmd) {
                    Ok(pid) => {
                        c.recording = true;
                        c.record_pid = Some(pid);
                        c.status = "recording".into();
                        c.error = warn;
                    }
                    Err(e) => {
                        c.error = Some(match warn.take() {
                            Some(w) => format!("{w}; {e}"),
                            None => e,
                        });
                    }
                }
                Ok(ok_json(to_value(c)?))
            }

            // —— POST /api/v1/surveillance/cameras/:id/stop-record —— 停止录像
            (HttpMethod::Post, ["api", "v1", "surveillance", "cameras", id, "stop-record"]) => {
                let mut cameras = self.cameras.lock().expect("cameras poisoned");
                let Some(c) = cameras.iter_mut().find(|c| c.id == *id) else {
                    return Ok(error_response(404, &format!("摄像头不存在: {id}")));
                };
                if let Some(pid) = c.record_pid.take() {
                    let _ = Self::kill_pid(pid);
                }
                c.recording = false;
                if c.status == "recording" {
                    c.status = if c.stream_pid.is_some() {
                        "online".into()
                    } else {
                        "offline".into()
                    };
                }
                Ok(ok_json(to_value(c)?))
            }

            // ===================== 网段扫描 =====================
            // —— POST /api/v1/surveillance/scan —— 扫描网段发现摄像头（admin）
            (HttpMethod::Post, ["api", "v1", "surveillance", "scan"]) => {
                let body: ScanBody = serde_json::from_value(req.body).unwrap_or_default();
                let subnet = match body
                    .subnet
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    Some(s) => s.to_string(),
                    None => match infer_local_subnet() {
                        Some(s) => s,
                        None => {
                            return Ok(error_response(
                                400,
                                "无法推断本机网段（无默认路由），请显式指定 subnet，如 192.0.2.0/24",
                            ));
                        }
                    },
                };
                // 预检：非法网段直接 400，不进扫描
                if parse_subnet(&subnet).is_none() {
                    return Ok(error_response(
                        400,
                        &format!(
                            "subnet 非法: {subnet:?}（形如 192.0.2.0/24，仅支持 /24 ~ /32）"
                        ),
                    ));
                }
                // 已添加 IP 集合（锁内快照后立刻释放，避免扫描 8s 期间持锁）
                let added_ips: std::collections::HashSet<String> = {
                    let cameras = self.cameras.lock().expect("cameras poisoned");
                    cameras
                        .iter()
                        .filter_map(|c| extract_host_from_url(&c.url))
                        .collect()
                };
                let report = Self::scan_subnet_with(
                    &subnet,
                    added_ips,
                    SCAN_OVERALL_TIMEOUT,
                    std::sync::Arc::new(Self::tcp_probe),
                )
                .await
                .map_err(ApiGatewayError::Internal)?;
                Ok(ok_json(to_value(&report)?))
            }

            // ===================== 全局设置 =====================
            // —— GET /api/v1/surveillance/settings —— 读设置 + 可写性/占用概览
            (HttpMethod::Get, ["api", "v1", "surveillance", "settings"]) => {
                let s = self.settings.lock().expect("settings poisoned").clone();
                let writable = dir_writable(&s.recording_dir);
                let (usage_bytes, file_count) = dir_usage(&s.recording_dir);
                let mut legacy_dirs = vec!["/tank/recordings".to_string()];
                legacy_dirs.retain(|d| d != &s.recording_dir);
                Ok(ok_json(serde_json::json!({
                    "recording_dir": s.recording_dir,
                    "default_recording_dir": resolve_default_recording_dir(None),
                    "writable": writable,
                    "usage_bytes": usage_bytes,
                    "file_count": file_count,
                    "legacy_dirs": legacy_dirs,
                    "note": "修改后新录像写入新路径；存量录像保留在原路径且录像列表仍可见（不迁移）",
                })))
            }

            // —— POST /api/v1/surveillance/settings —— 改录像根目录（admin）
            (HttpMethod::Post, ["api", "v1", "surveillance", "settings"]) => {
                let body: UpdateSettingsBody = serde_json::from_value(req.body)
                    .map_err(|e| ApiGatewayError::Internal(format!("解析设置请求体失败: {e}")))?;
                let dir = body.recording_dir.trim().trim_end_matches('/').to_string();
                if !dir.starts_with('/') || dir.len() < 2 || dir.contains("..") {
                    return Ok(error_response(
                        400,
                        "recording_dir 必须为绝对路径（/ 开头）且不含 ..",
                    ));
                }
                if !dir_writable(&dir) {
                    return Ok(error_response(400, &format!("目录不可写/无法创建: {dir}")));
                }
                *self.settings.lock().expect("settings poisoned") = SurveillanceSettings {
                    recording_dir: dir.clone(),
                };
                self.persist_settings();
                Ok(ok_json(serde_json::json!({
                    "ok": true,
                    "recording_dir": dir,
                    "note": "已更新：新录像将写入新路径；存量录像保留在原路径（录像列表仍可见）",
                })))
            }

            // ===================== 批量添加 =====================
            // —— POST /api/v1/surveillance/cameras/batch —— 扫描结果多选批量添加（admin）
            (HttpMethod::Post, ["api", "v1", "surveillance", "cameras", "batch"]) => {
                let body: BatchBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析批量添加请求体失败: {e}"))
                })?;
                if body.items.is_empty() {
                    return Ok(error_response(400, "items 不可为空"));
                }
                let prefix = body
                    .name_prefix
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("cam");
                let username = body.username.as_deref().unwrap_or("");
                let password = body.password.as_deref().unwrap_or("");
                let mut results = Vec::new();
                {
                    let mut cameras = self.cameras.lock().expect("cameras poisoned");
                    for (idx, item) in body.items.iter().enumerate() {
                        let name = format!("{prefix}-{}", idx + 1);
                        let raw = item.rtsp_url.as_deref().map(str::trim).unwrap_or("");
                        if raw.is_empty() {
                            results.push(BatchItemResult {
                                index: idx,
                                ok: false,
                                name,
                                url: String::new(),
                                error: Some("rtsp_url 为空".into()),
                                camera_id: None,
                            });
                            continue; // 单台失败不影响其余
                        }
                        let url = apply_credentials(raw, username, password);
                        let cam = Camera {
                            id: self.next_id(),
                            name: name.clone(),
                            url: url.clone(),
                            protocol: infer_protocol(&url),
                            enabled: true,
                            status: "offline".into(),
                            recording: false,
                            record_pid: None,
                            stream_pid: None,
                            hls_dir: None,
                            created_at: now_iso(),
                            error: None,
                        };
                        results.push(BatchItemResult {
                            index: idx,
                            ok: true,
                            name,
                            url,
                            error: None,
                            camera_id: Some(cam.id.clone()),
                        });
                        cameras.push(cam);
                    }
                }
                self.persist();
                let created = results.iter().filter(|r| r.ok).count();
                let report = BatchReport {
                    created,
                    failed: results.len() - created,
                    results,
                };
                Ok(ok_json(to_value(&report)?))
            }

            // ===================== 快照 =====================
            // —— POST /api/v1/surveillance/cameras/:id/snapshot —— 抓单帧 JPEG（admin）
            (HttpMethod::Post, ["api", "v1", "surveillance", "cameras", id, "snapshot"]) => {
                let url = {
                    let cameras = self.cameras.lock().expect("cameras poisoned");
                    cameras.iter().find(|c| c.id == *id).map(|c| c.url.clone())
                };
                let Some(url) = url else {
                    return Ok(error_response(404, &format!("摄像头不存在: {id}")));
                };
                // 目录：优先 /tank/snapshots/<id>，降级 /tmp/os-snapshots/<id>
                let dirs = Self::snapshot_dir_candidates(id);
                let mut chosen: Option<(String, Option<String>)> = None; // (dir, warning)
                for d in &dirs {
                    if std::fs::create_dir_all(d).is_ok() {
                        chosen = Some((d.clone(), None));
                        break;
                    }
                }
                let (dir, warn) = match chosen {
                    Some((d, w)) => (d, w),
                    None => (
                        dirs[1].clone(),
                        Some(format!(
                            "无法创建快照目录 {}（降级到 {}）",
                            dirs[0], dirs[1]
                        )),
                    ),
                };
                let outfile = format!("{}/latest.jpg", dir.trim_end_matches('/'));
                let args = build_snapshot_cmd(&url, &outfile);
                let fut = tokio::process::Command::new("ffmpeg")
                    .args(&args)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .output();
                let (ok, warn, err): (bool, Option<String>, Option<String>) =
                    match tokio::time::timeout(Duration::from_secs(8), fut).await {
                        Ok(Ok(o)) if o.status.success() && Path::new(&outfile).exists() => {
                            (true, warn, None)
                        }
                        Ok(Ok(_)) => (
                            false,
                            None,
                            Some("抓帧失败（ffmpeg 退出非 0 或未产出文件）".into()),
                        ),
                        Ok(Err(e)) => (false, None, Some(format!("ffmpeg 不可用: {e}"))),
                        Err(_) => (false, None, Some("快照超时（>8s）".into())),
                    };
                if !ok {
                    // 失败记入摄像头 error（不 panic，与降级风格一致）
                    let msg = err.unwrap_or_else(|| "快照失败".into());
                    let mut cameras = self.cameras.lock().expect("cameras poisoned");
                    if let Some(c) = cameras.iter_mut().find(|c| c.id == *id) {
                        c.error = Some(msg.clone());
                    }
                    return Ok(error_response(500, &msg));
                }
                if let Some(w) = &warn {
                    let mut cameras = self.cameras.lock().expect("cameras poisoned");
                    if let Some(c) = cameras.iter_mut().find(|c| c.id == *id) {
                        c.error = Some(w.clone());
                    }
                }
                let view = Self::read_latest_snapshot(id)
                    .ok_or_else(|| ApiGatewayError::Internal("读取快照文件失败".into()))?;
                Ok(ok_json(to_value(&view)?))
            }

            // —— GET /api/v1/surveillance/cameras/:id/snapshot —— 看最近快照
            (HttpMethod::Get, ["api", "v1", "surveillance", "cameras", id, "snapshot"]) => {
                match Self::read_latest_snapshot(id) {
                    Some(v) => Ok(ok_json(to_value(&v)?)),
                    None => Ok(error_response(
                        404,
                        "暂无快照（先 POST /cameras/:id/snapshot 抓取）",
                    )),
                }
            }

            // —— 未覆盖路由 —— 兜底 404
            _ => Ok(error_response(404, "surveillance: 未匹配的路由")),
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
        handler_component: "surveillance".to_string(),
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

/// SystemTime → ISO8601 字符串（本地时区）。
fn systemtime_to_iso(t: std::time::SystemTime) -> String {
    use chrono::{DateTime, Local};
    DateTime::<Local>::from(t)
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

/// 推断本机主网卡网段（`"a.b.c.0/24"`）：`ip -j -4 route show default` 的
/// `prefsrc`（默认路由出口地址，network.rs 同款子进程风格，os-api 内轻量复制）。
/// 无默认路由 / 命令失败 / 解析失败 → None（caller 提示用户显式指定 subnet）。
fn infer_local_subnet() -> Option<String> {
    let out = std::process::Command::new("ip")
        .args(["-j", "-4", "route", "show", "default"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let ip = v.as_array()?.first()?.get("prefsrc")?.as_str()?;
    if ip.is_empty() {
        return None;
    }
    Some(format!("{ip}/24"))
}

/// 递归统计目录占用（总字节, 文件数）。目录不存在返回 (0, 0)；深度上限 8 防环。
fn dir_usage(path: &str) -> (u64, u64) {
    fn walk(p: &Path, depth: u32, bytes: &mut u64, files: &mut u64) {
        if depth > 8 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(p) else {
            return;
        };
        for e in rd.flatten() {
            let fp = e.path();
            if fp.is_dir() {
                walk(&fp, depth + 1, bytes, files);
            } else if let Ok(m) = e.metadata() {
                *bytes += m.len();
                *files += 1;
            }
        }
    }
    let (mut b, mut f) = (0u64, 0u64);
    if Path::new(path).is_dir() {
        walk(Path::new(path), 0, &mut b, &mut f);
    }
    (b, f)
}

/// 目录可写校验：create_dir_all + 写探测文件（简化校验，替代完整 du/权限检查）。
fn dir_writable(dir: &str) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = Path::new(dir).join(".nexos-write-probe");
    let ok = std::fs::write(&probe, b"ok").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// pid 存活检测（Linux `/proc/<pid>` 存在即活；不存在视为已退出）。
fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// 从 URL 推断协议（rtsp:// / onvif / http://）。
fn infer_protocol(url: &str) -> String {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("rtsp://") {
        "rtsp".into()
    } else if lower.starts_with("http://") || lower.starts_with("https://") {
        "http".into()
    } else {
        "rtsp".into()
    }
}

// ----------------------------------------------------------------------------
// 配置落盘
// ----------------------------------------------------------------------------

/// 解析 cameras.json 路径：`/tank/os-data/cameras.json`（目录存在即用），否则回退 `./cameras.json`。
fn cameras_file_path() -> String {
    let dir = "/tank/os-data";
    if Path::new(dir).is_dir() {
        format!("{dir}/cameras.json")
    } else {
        "./cameras.json".to_string()
    }
}

/// 从 JSON 文件加载摄像头列表（缺失/解析失败 → 空列表）。
fn load_cameras_from(path: &str) -> Vec<Camera> {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// 把摄像头列表写回 JSON 文件（覆盖；自动建父目录）。
fn save_cameras_to(path: &str, list: &[Camera]) -> std::io::Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(list).map_err(std::io::Error::other)?;
    std::fs::write(path, body)
}

/// 重启加载后重置运行态：pid/hls_dir/recording 清空（子进程已不可达）。
/// "recording" 状态回落 "offline"；保留 id/name/url/protocol/enabled/created_at。
fn normalize_loaded(mut cams: Vec<Camera>) -> Vec<Camera> {
    for c in &mut cams {
        c.recording = false;
        c.record_pid = None;
        c.stream_pid = None;
        c.hls_dir = None;
        if c.status == "recording" {
            c.status = "offline".into();
        }
    }
    cams
}

// ----------------------------------------------------------------------------
// 设置落盘（surveillance-settings.json，与 cameras.json 同目录）
// ----------------------------------------------------------------------------

/// 录像根目录初始默认：env `NEXOS_SURVEILLANCE_DIR`（非空即用）→ `/tank/recordings`
/// （沿用历史落盘点，env 未设时零行为变化）。参数化 env 值便于测试。
#[must_use]
pub fn resolve_default_recording_dir(env_override: Option<String>) -> String {
    match env_override {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => "/tank/recordings".to_string(),
    }
}

/// 默认设置（读 env `NEXOS_SURVEILLANCE_DIR`）。
fn default_settings() -> SurveillanceSettings {
    SurveillanceSettings {
        recording_dir: resolve_default_recording_dir(std::env::var("NEXOS_SURVEILLANCE_DIR").ok()),
    }
}

/// 设置文件路径 = cameras.json 同目录兄弟名（`cameras.json` → `cameras-settings.json`）。
fn sibling_settings_path(cameras_path: &str) -> Option<String> {
    let p = Path::new(cameras_path);
    let file = p.file_name()?.to_str()?;
    let stem = file.strip_suffix(".json").unwrap_or(file);
    let dir = p.parent()?;
    Some(
        dir.join(format!("{stem}-settings.json"))
            .to_string_lossy()
            .into_owned(),
    )
}

/// 从 JSON 文件加载设置（缺失/解析失败 → env 默认，永不 panic）。
fn load_settings_from(path: &str) -> SurveillanceSettings {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| default_settings()),
        Err(_) => default_settings(),
    }
}

/// 把设置写回 JSON 文件（覆盖；自动建父目录）。
fn save_settings_to(path: &str, s: &SurveillanceSettings) -> std::io::Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(s).map_err(std::io::Error::other)?;
    std::fs::write(path, body)
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

    fn make_cam(id: &str, status: &str, recording: bool) -> Camera {
        Camera {
            id: id.into(),
            name: format!("cam-{id}"),
            url: "rtsp://192.168.1.50:554/stream1".into(),
            protocol: "rtsp".into(),
            enabled: true,
            status: status.into(),
            recording,
            record_pid: None,
            stream_pid: None,
            hls_dir: None,
            created_at: "2026-08-12T09:00:00+08:00".into(),
            error: None,
        }
    }

    /// 唯一临时 JSON 路径（无 tempfile 依赖，用 pid+序号避免并发冲突）。
    fn temp_json_path() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        format!("/tmp/os-cam-test-{}-{n}.json", std::process::id())
    }

    #[test]
    fn build_stream_cmd_has_hls_and_libx264() {
        let cmd = build_stream_cmd("rtsp://x/stream", "/tank/hls/cam-1");
        // 程序名不在 vec 内；断言关键 token 存在
        assert!(cmd.iter().any(|a| a == "libx264"), "应含 libx264 编码器");
        assert!(cmd.iter().any(|a| a == "hls"), "应含 hls 格式");
        assert!(
            cmd.iter().any(|a| a == "ultrafast"),
            "应含 ultrafast preset"
        );
        assert!(
            cmd.iter().any(|a| a == "zerolatency"),
            "应含 zerolatency tune"
        );
        assert!(cmd.iter().any(|a| a == "tcp"), "应强制 tcp 传输");
        // 输出 m3u8 路径
        assert!(
            cmd.iter().any(|a| a.ends_with("/index.m3u8")),
            "应以 index.m3u8 结尾"
        );
        assert!(cmd.iter().any(|a| a == "rtsp://x/stream"));
    }

    #[test]
    fn build_record_cmd_has_mp4() {
        let cmd = build_record_cmd(
            "rtsp://x/stream",
            "/tank/recordings/cam-1/20260812/090000.mp4",
        );
        assert!(cmd.iter().any(|a| a == "mp4"), "应含 mp4 格式");
        assert!(cmd.iter().any(|a| a == "copy"), "应 -c copy 零损耗");
        assert!(cmd.iter().any(|a| a == "+faststart"), "应 +faststart");
        assert!(cmd.iter().any(|a| a == "tcp"));
        assert!(
            cmd.iter().any(|a| a.ends_with("/090000.mp4")),
            "应以输出文件结尾"
        );
    }

    #[test]
    fn build_probe_cmd_has_null() {
        let cmd = build_probe_cmd("rtsp://x/stream");
        // `-f null -`：null 容器 + 空输出
        assert!(cmd.iter().any(|a| a == "null"), "应含 null 容器");
        assert!(cmd.iter().any(|a| a == "-"), "应含空输出 -");
        assert!(cmd.iter().any(|a| a == "1"), "应 -t 1（拉 1 秒）");
        assert!(cmd.iter().any(|a| a == "tcp"));
    }

    #[test]
    fn cameras_json_roundtrip() {
        let path = temp_json_path();
        let cams = vec![
            make_cam("cam-1", "online", false),
            make_cam("cam-2", "recording", true),
        ];
        save_cameras_to(&path, &cams).expect("写入应成功");
        let loaded = load_cameras_from(&path);
        assert_eq!(loaded.len(), 2, "应回读 2 条");
        assert_eq!(loaded[0].id, "cam-1");
        assert_eq!(loaded[0].name, "cam-cam-1");
        assert!(loaded[1].recording);
        // 清理
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn normalize_resets_runtime_state() {
        let mut cams = vec![make_cam("cam-1", "recording", true)];
        cams[0].record_pid = Some(12345);
        cams[0].hls_dir = Some("/tank/hls/cam-1".into());
        let out = normalize_loaded(cams);
        assert!(!out[0].recording, "recording 应被重置");
        assert_eq!(out[0].record_pid, None, "record_pid 应被清空");
        assert_eq!(out[0].hls_dir, None, "hls_dir 应被清空");
        assert_eq!(out[0].status, "offline", "recording 状态应回落 offline");
    }

    #[tokio::test]
    async fn routes_declares_sixteen_endpoints() {
        let h = SurveillanceRouteHandler::with_cameras(vec![]);
        let routes = h.routes().await;
        assert_eq!(routes.len(), 16, "应声明 16 条路由");
        assert!(routes.iter().all(|r| r.handler_component == "surveillance"));
        // 写操作均要求 admin（含 scan / settings POST / batch / snapshot POST）
        for r in &routes {
            if r.method == HttpMethod::Post || r.method == HttpMethod::Delete {
                assert!(r.requires_auth, "{:?} {} 应要求认证", r.method, r.path);
                assert_eq!(r.required_roles, vec!["admin".to_string()]);
            }
        }
        // GET（含 recordings / snapshot / settings / stats）不要求认证
        for r in &routes {
            if r.method == HttpMethod::Get {
                assert!(!r.requires_auth, "{:?} {} 不应要求认证", r.method, r.path);
            }
        }
    }

    #[tokio::test]
    async fn create_then_delete_camera() {
        let h = SurveillanceRouteHandler::with_cameras(vec![]);
        // 添加
        let resp = h
            .handle(post_req(
                "/api/v1/surveillance/cameras",
                serde_json::json!({"name": "前门", "url": "rtsp://10.0.0.1/stream"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        let id = resp.body["id"].as_str().unwrap().to_string();
        assert_eq!(resp.body["status"], "offline");
        assert_eq!(resp.body["protocol"], "rtsp");
        assert_eq!(resp.body["recording"], false);
        assert_eq!(h.cameras_snapshot().len(), 1);
        // 列表可见
        let resp = h
            .handle(get_req("/api/v1/surveillance/cameras"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body.as_array().unwrap().len(), 1);
        // 删除
        let resp = h
            .handle(del_req(&format!("/api/v1/surveillance/cameras/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["ok"], true);
        assert_eq!(h.cameras_snapshot().len(), 0, "删除后应为空");
    }

    #[tokio::test]
    async fn create_validates_empty_fields() {
        let h = SurveillanceRouteHandler::with_cameras(vec![]);
        let resp = h
            .handle(post_req(
                "/api/v1/surveillance/cameras",
                serde_json::json!({"name": "", "url": "rtsp://x"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        let resp = h
            .handle(post_req(
                "/api/v1/surveillance/cameras",
                serde_json::json!({"name": "ok", "url": ""}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        assert_eq!(h.cameras_snapshot().len(), 0);
    }

    #[tokio::test]
    async fn stats_aggregation() {
        let h = SurveillanceRouteHandler::with_cameras(vec![
            make_cam("cam-1", "online", false),
            make_cam("cam-2", "recording", true),
            make_cam("cam-3", "offline", false),
        ]);
        let resp = h
            .handle(get_req("/api/v1/surveillance/stats"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["camera_count"], 3);
        // online + recording 均计入"在线"
        assert_eq!(resp.body["online"], 2);
        assert_eq!(resp.body["recording"], 1);
        assert!(resp.body["storage_used_bytes"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn recordings_scan_missing_returns_empty() {
        let resp = h_get_recordings_for("cam-nonexistent-xyz-999").await;
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body.as_array().unwrap().len(), 0);
    }

    async fn h_get_recordings_for(id: &str) -> ApiResponse {
        let h = SurveillanceRouteHandler::with_cameras(vec![]);
        h.handle(get_req(&format!(
            "/api/v1/surveillance/cameras/{id}/recordings"
        )))
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn delete_missing_returns_404() {
        let h = SurveillanceRouteHandler::with_cameras(vec![]);
        let resp = h
            .handle(del_req("/api/v1/surveillance/cameras/nope"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn recordings_lists_real_files() {
        // 在 /tmp/recordings/<id>/<date>/ 下造一个 mp4，扫描应能找到
        use std::fs;
        let id = format!("cam-realtest-{}", std::process::id());
        let date = "20260812";
        let dir = format!("/tmp/recordings/{id}/{date}");
        fs::create_dir_all(&dir).unwrap();
        let file = format!("{dir}/091530.mp4");
        fs::write(&file, b"fake-mp4-content").unwrap();
        let resp = h_get_recordings_for(&id).await;
        let arr = resp.body.as_array().unwrap();
        assert!(!arr.is_empty(), "应扫到至少 1 个录像");
        let e = &arr[0];
        assert_eq!(e["name"], "091530.mp4");
        assert_eq!(e["date"], date);
        assert!(e["size_bytes"].as_u64().unwrap() > 0);
        // 清理
        let _ = fs::remove_dir_all(format!("/tmp/recordings/{id}"));
    }

    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<SurveillanceRouteHandler>();
    }

    // ========================================================================
    // 网段扫描
    // ========================================================================

    #[test]
    fn parse_subnet_masks_host_bits_and_accepts_forms() {
        // 标准 "a.b.c.0/24"
        assert_eq!(parse_subnet("192.0.2.0/24"), Some((0x0AA80A00, 24)));
        // 主机位被掩掉
        assert_eq!(parse_subnet("192.0.2.77/24"), Some((0x0AA80A00, 24)));
        // 缺省 /24
        assert_eq!(parse_subnet("192.0.2.77"), Some((0x0AA80A00, 24)));
        // /32 单机
        assert_eq!(parse_subnet("10.0.0.5/32"), Some((0x0A000005, 32)));
        // 空白容错
        assert_eq!(parse_subnet("  10.0.0.0/24 "), Some((0x0A000000, 24)));
    }

    #[test]
    fn parse_subnet_rejects_invalid() {
        for bad in [
            "",
            "  ",
            "abc",
            "10.0.0",
            "10.0.0.1.2",
            "300.1.1.1",
            "10.0.0.-1",
            "10.0.0.0/16", // 前缀 <24（扫描规模失控）
            "10.0.0.0/23", // 同上
            "10.0.0.0/33", // 前缀越界
            "10.0.0.0/xx",
            "10.0.0.0/",
        ] {
            assert!(parse_subnet(bad).is_none(), "{bad:?} 应被拒绝");
        }
    }

    #[test]
    fn subnet_hosts_skips_network_and_broadcast() {
        let hosts = subnet_hosts(0x0AA80A00, 24);
        assert_eq!(hosts.len(), 254, "/24 应有 254 台可扫描主机");
        assert_eq!(hosts.first().unwrap(), "192.0.2.1");
        assert_eq!(hosts.last().unwrap(), "192.0.2.254");
        // /32 全保留（单机）
        assert_eq!(subnet_hosts(0x0A000005, 32), vec!["10.0.0.5".to_string()]);
    }

    #[test]
    fn vendor_signature_maps_port_signatures() {
        // 554+8000 → 海康模板
        let (v, t) = vendor_signature("10.0.0.2", &[554, 8000]).unwrap();
        assert_eq!(v, "hikvision");
        assert_eq!(t, "rtsp://user:pass@10.0.0.2:554/h264/ch1/main/av_stream");
        // 554+80 → 大华模板
        let (v, t) = vendor_signature("10.0.0.3", &[80, 554]).unwrap();
        assert_eq!(v, "dahua");
        assert_eq!(
            t,
            "rtsp://user:pass@10.0.0.3:554/cam/realmonitor?channel=1&subtype=0"
        );
        // 仅 554 → 通用
        let (v, t) = vendor_signature("10.0.0.4", &[554]).unwrap();
        assert_eq!(v, "generic");
        assert_eq!(t, "rtsp://user:pass@10.0.0.4:554/");
        // 8000（无 554）→ 海康（RTSP 或被过滤）
        assert_eq!(
            vendor_signature("10.0.0.5", &[8000]).unwrap().0,
            "hikvision"
        );
        // 8899 → ONVIF
        assert_eq!(
            vendor_signature("10.0.0.6", &[8899, 80]).unwrap().0,
            "onvif"
        );
        // 仅 80 → 不收录（通用 Web 服务非摄像头特征）
        assert!(vendor_signature("10.0.0.7", &[80]).is_none());
        // 554+80+8000 → 海康优先（8000 特征更强）
        assert_eq!(
            vendor_signature("10.0.0.8", &[554, 80, 8000]).unwrap().0,
            "hikvision"
        );
        // 无端口 → 不收录
        assert!(vendor_signature("10.0.0.9", &[]).is_none());
    }

    #[test]
    fn extract_host_from_url_variants() {
        assert_eq!(
            extract_host_from_url("rtsp://admin:pw@10.0.0.5:554/stream1").as_deref(),
            Some("10.0.0.5")
        );
        assert_eq!(
            extract_host_from_url("rtsp://10.0.0.6:554/").as_deref(),
            Some("10.0.0.6")
        );
        assert_eq!(
            extract_host_from_url("http://10.0.0.7:80/onvif").as_deref(),
            Some("10.0.0.7")
        );
        assert_eq!(extract_host_from_url("10.0.0.8"), None, "无 scheme 拒绝");
        assert_eq!(extract_host_from_url(""), None);
    }

    #[test]
    fn apply_credentials_encodes_and_replaces() {
        let tpl = "rtsp://user:pass@10.0.0.2:554/cam/realmonitor?channel=1&subtype=0";
        // 常规替换
        assert_eq!(
            apply_credentials(tpl, "admin", "secret"),
            "rtsp://admin:secret@10.0.0.2:554/cam/realmonitor?channel=1&subtype=0"
        );
        // 特殊字符百分号编码（@ # : / 不破坏 URL）
        assert_eq!(
            apply_credentials(tpl, "ad@min", "a:b#c"),
            "rtsp://ad%40min:a%3Ab%23c@10.0.0.2:554/cam/realmonitor?channel=1&subtype=0"
        );
        // 凭证为空 → 原样
        assert_eq!(apply_credentials(tpl, "", ""), tpl);
        // 无占位 → 原样
        assert_eq!(
            apply_credentials("rtsp://10.0.0.2:554/", "admin", "pw"),
            "rtsp://10.0.0.2:554/"
        );
    }

    /// 注入式探测函数的签名类型（不触真实网络）。
    fn fake_probe(
        f: impl Fn(String) -> Vec<u16> + Send + Sync + 'static,
    ) -> std::sync::Arc<dyn Fn(String) -> futures::future::BoxFuture<'static, Vec<u16>> + Send + Sync>
    {
        std::sync::Arc::new(
            move |ip: String| -> futures::future::BoxFuture<'static, Vec<u16>> {
                let out = f(ip);
                Box::pin(async move { out })
            },
        )
    }

    #[tokio::test]
    async fn scan_with_injected_prober_marks_added_and_vendor() {
        // 已添加 IP 集合（模拟库内已有 192.168.1.50）
        let mut added = std::collections::HashSet::new();
        added.insert("192.168.1.50".to_string());
        let probe = fake_probe(|ip| match ip.as_str() {
            "10.90.0.2" => vec![554, 80],
            "10.90.0.3" => vec![554, 8000],
            "10.90.0.4" => vec![80], // 仅 80：不收录
            "10.90.0.5" => vec![8899],
            _ => vec![],
        });
        let report = SurveillanceRouteHandler::scan_subnet_with(
            "10.90.0.0/29",
            added,
            Duration::from_secs(2),
            probe,
        )
        .await
        .expect("扫描应成功");
        assert!(!report.timed_out);
        assert_eq!(report.subnet, "10.90.0.0/29");
        // /29 → 6 台可扫描主机
        assert_eq!(report.scanned, 6);
        assert_eq!(report.found, 3, "仅 80 的主机不应收录");
        let by_ip: Vec<&ScanHit> = report.hits.iter().collect();
        assert_eq!(by_ip[0].ip, "10.90.0.2");
        assert_eq!(by_ip[0].vendor_guess, "dahua");
        assert_eq!(by_ip[1].ip, "10.90.0.3");
        assert_eq!(by_ip[1].vendor_guess, "hikvision");
        assert_eq!(by_ip[2].ip, "10.90.0.5");
        assert_eq!(by_ip[2].vendor_guess, "onvif");
        assert!(!by_ip.iter().any(|x| x.added), "本网段无已添加 IP");
    }

    #[tokio::test]
    async fn scan_marks_added_ip_from_existing_camera_urls() {
        let cam = Camera {
            url: "rtsp://admin:pw@10.90.0.2:554/x".into(),
            ..make_cam("cam-1", "offline", false)
        };
        let added: std::collections::HashSet<String> =
            vec![extract_host_from_url(&cam.url).unwrap()]
                .into_iter()
                .collect();
        let probe = fake_probe(|ip| {
            if ip == "10.90.0.2" {
                vec![554, 80]
            } else {
                vec![]
            }
        });
        let report = SurveillanceRouteHandler::scan_subnet_with(
            "10.90.0.0/29",
            added,
            Duration::from_secs(2),
            probe,
        )
        .await
        .unwrap();
        assert_eq!(report.found, 1);
        assert!(report.hits[0].added, "已添加 IP 应标 added:true");
    }

    #[tokio::test]
    async fn scan_timeout_returns_partial_results() {
        // .1 立即返回，其余沉睡 5s → 整体 400ms 截断：只收 .1 且 timed_out:true
        let slow_probe = std::sync::Arc::new(
            |ip: String| -> futures::future::BoxFuture<'static, Vec<u16>> {
                Box::pin(async move {
                    if ip.ends_with(".1") {
                        vec![554]
                    } else {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        vec![]
                    }
                })
            },
        );
        let report = SurveillanceRouteHandler::scan_subnet_with(
            "10.99.0.0/24",
            Default::default(),
            Duration::from_millis(400),
            slow_probe,
        )
        .await
        .unwrap();
        assert!(report.timed_out, "应触达整体超时");
        assert_eq!(report.scanned, 254);
        assert_eq!(report.found, 1, "只应收到快宿主 .1 的结果");
        assert!(report.hits[0].ip.ends_with(".1"));
    }

    #[tokio::test]
    async fn scan_endpoint_rejects_invalid_subnet() {
        let h = SurveillanceRouteHandler::with_cameras(vec![]);
        for bad in ["10.0.0.0/16", "zzz", "10.0.0.0/40"] {
            let resp = h
                .handle(post_req(
                    "/api/v1/surveillance/scan",
                    serde_json::json!({"subnet": bad}),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 400, "{bad:?} 应 400");
        }
    }

    #[tokio::test]
    async fn tcp_probe_converges_within_timeout_budget() {
        // RFC5737 保留段（TEST-NET-1）实际不可达：真实探测应被 300ms 单连接
        // 超时截断（4 端口并发），整体在秒级预算内收敛，而不是挂死。
        let start = std::time::Instant::now();
        let ports = SurveillanceRouteHandler::tcp_probe("192.0.2.1".into()).await;
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "真实探测应在预算内收敛（实测 {:?}, ports={ports:?}）",
            start.elapsed()
        );
    }

    #[test]
    fn infer_local_subnet_is_parseable_when_present() {
        // 本机有默认路由时应得到可解析网段；无默认路由（CI 特殊环境）也允许 None。
        if let Some(s) = infer_local_subnet() {
            assert!(
                parse_subnet(&s).is_some(),
                "推断网段 {s:?} 必须可被 parse_subnet 解析"
            );
        }
    }

    // ========================================================================
    // 全局设置（录像根目录）
    // ========================================================================

    #[test]
    fn resolve_default_recording_dir_env_or_fallback() {
        assert_eq!(
            resolve_default_recording_dir(Some("/tank/surveillance".into())),
            "/tank/surveillance"
        );
        assert_eq!(
            resolve_default_recording_dir(Some("  ".into())),
            "/tank/recordings",
            "空白 env 回落默认"
        );
        assert_eq!(
            resolve_default_recording_dir(None),
            "/tank/recordings",
            "未设 env 沿用历史落盘点"
        );
    }

    #[tokio::test]
    async fn settings_post_updates_dir_persists_and_feeds_recordings() {
        let cpath = temp_json_path();
        let h = SurveillanceRouteHandler::with_cameras_path(vec![], cpath.clone());
        // GET 初始（env 默认）
        let get0 = h
            .handle(get_req("/api/v1/surveillance/settings"))
            .await
            .unwrap();
        assert_eq!(get0.status, 200);
        assert!(get0.body["recording_dir"].as_str().is_some());
        assert!(get0.body["usage_bytes"].as_u64().is_some());
        // POST 改到 /tmp 新根
        let dir = format!("/tmp/os-recdir-{}", std::process::id());
        let post = h
            .handle(post_req(
                "/api/v1/surveillance/settings",
                serde_json::json!({"recording_dir": dir}),
            ))
            .await
            .unwrap();
        assert_eq!(post.status, 200);
        assert_eq!(post.body["ok"], true);
        assert_eq!(post.body["recording_dir"], dir);
        // GET 反映新值
        let get1 = h
            .handle(get_req("/api/v1/surveillance/settings"))
            .await
            .unwrap();
        assert_eq!(get1.body["recording_dir"], dir);
        // 持久化文件存在且值正确
        let spath = sibling_settings_path(&cpath).unwrap();
        assert!(Path::new(&spath).exists(), "设置应落盘 {spath}");
        assert_eq!(load_settings_from(&spath).recording_dir, dir);
        // 新根目录下的录像在列表可见（多根合并）
        std::fs::create_dir_all(format!("{dir}/cam-x/20260820")).unwrap();
        std::fs::write(format!("{dir}/cam-x/20260820/120000.mp4"), b"mp4").unwrap();
        let recs = h
            .handle(get_req("/api/v1/surveillance/cameras/cam-x/recordings"))
            .await
            .unwrap();
        assert!(
            recs.body
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["name"] == "120000.mp4"),
            "新根目录录像应可见"
        );
        // 清理
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&spath);
        let _ = std::fs::remove_file(&cpath);
    }

    #[tokio::test]
    async fn settings_post_rejects_relative_and_unwritable() {
        let h = SurveillanceRouteHandler::with_cameras(vec![]);
        // 相对路径 → 400
        let resp = h
            .handle(post_req(
                "/api/v1/surveillance/settings",
                serde_json::json!({"recording_dir": "relative/dir"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // 含 .. → 400
        let resp = h
            .handle(post_req(
                "/api/v1/surveillance/settings",
                serde_json::json!({"recording_dir": "/tmp/../etc"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // 不可写（父路径是文件）→ 400
        let resp = h
            .handle(post_req(
                "/api/v1/surveillance/settings",
                serde_json::json!({"recording_dir": "/dev/null/sub"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn record_filepath_in_uses_base_dir_and_falls_back() {
        let (p, w) = SurveillanceRouteHandler::record_filepath_in("/tmp/os-recbase", "cam-7");
        assert!(
            p.starts_with("/tmp/os-recbase/cam-7/"),
            "应落在配置根下: {p}"
        );
        assert!(p.ends_with(".mp4"));
        assert!(w.is_none());
        // 基目录不可创建 → 降级 /tmp/recordings + warning
        let (p, w) = SurveillanceRouteHandler::record_filepath_in("/dev/null/sub", "cam-7");
        assert!(p.starts_with("/tmp/recordings/cam-7/"), "应降级: {p}");
        assert!(w.is_some());
    }

    #[test]
    fn scan_recordings_in_merges_multiple_roots() {
        let id = format!("cam-merge-{}", std::process::id());
        for (root, date, name) in [
            ("/tmp/os-merge-a", "20260801", "090000.mp4"),
            ("/tmp/os-merge-b", "20260802", "100000.mp4"),
        ] {
            std::fs::create_dir_all(format!("{root}/{id}/{date}")).unwrap();
            std::fs::write(format!("{root}/{id}/{date}/{name}"), b"x").unwrap();
        }
        let roots = vec![
            "/tmp/os-merge-a".to_string(),
            "/tmp/os-merge-b".to_string(),
            "/tmp/os-merge-a/".to_string(), // 尾斜杠去重
        ];
        let out = SurveillanceRouteHandler::scan_recordings_in(&roots, &id);
        assert_eq!(out.len(), 2, "两个根的录像都应可见");
        // 日期降序（最新在前）
        assert_eq!(out[0].date, "20260802");
        assert_eq!(out[1].date, "20260801");
        let _ = std::fs::remove_dir_all("/tmp/os-merge-a");
        let _ = std::fs::remove_dir_all("/tmp/os-merge-b");
    }

    #[test]
    fn dir_usage_counts_files_and_bytes() {
        let dir = format!("/tmp/os-usage-{}", std::process::id());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(format!("{dir}/a.mp4"), vec![0u8; 100]).unwrap();
        std::fs::write(format!("{dir}/b.mp4"), vec![0u8; 50]).unwrap();
        let (bytes, files) = dir_usage(&dir);
        assert_eq!(bytes, 150);
        assert_eq!(files, 2);
        // 不存在的目录 → (0, 0)
        assert_eq!(dir_usage("/tmp/os-usage-none-xyz"), (0, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ========================================================================
    // 批量添加
    // ========================================================================

    #[tokio::test]
    async fn batch_create_mixed_results_with_credentials() {
        let h = SurveillanceRouteHandler::with_cameras(vec![]);
        let resp = h
            .handle(post_req(
                "/api/v1/surveillance/cameras/batch",
                serde_json::json!({
                    "items": [
                        {"ip": "10.0.0.2", "rtsp_url": "rtsp://user:pass@10.0.0.2:554/cam/realmonitor?channel=1&subtype=0"},
                        {"ip": "10.0.0.3"},
                        {"rtsp_url": "rtsp://user:pass@10.0.0.4:554/h264/ch1/main/av_stream"}
                    ],
                    "username": "admin",
                    "password": "a@b#c",
                    "name_prefix": "车库"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["created"], 2);
        assert_eq!(resp.body["failed"], 1);
        let results = resp.body["results"].as_array().unwrap();
        // 成功台：名字自动编号 + 凭证替换进模板
        assert_eq!(results[0]["name"], "车库-1");
        assert_eq!(results[0]["ok"], true);
        assert_eq!(
            results[0]["url"],
            "rtsp://admin:a%40b%23c@10.0.0.2:554/cam/realmonitor?channel=1&subtype=0"
        );
        assert_eq!(results[2]["name"], "车库-3");
        assert_eq!(
            results[2]["url"],
            "rtsp://admin:a%40b%23c@10.0.0.4:554/h264/ch1/main/av_stream"
        );
        // 失败台：逐台反馈，不影响其余
        assert_eq!(results[1]["ok"], false);
        assert!(results[1]["error"].as_str().unwrap().contains("rtsp_url"));
        assert_eq!(results[1]["camera_id"], serde_json::Value::Null);
        // 只入 2 台
        let cams = h.cameras_snapshot();
        assert_eq!(cams.len(), 2);
        assert!(cams.iter().all(|c| c.name.starts_with("车库-")));
        assert!(cams.iter().all(|c| c.url.starts_with("rtsp://admin:")));
    }

    #[tokio::test]
    async fn batch_empty_items_rejected() {
        let h = SurveillanceRouteHandler::with_cameras(vec![]);
        let resp = h
            .handle(post_req(
                "/api/v1/surveillance/cameras/batch",
                serde_json::json!({"items": []}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    #[tokio::test]
    async fn batch_default_prefix_when_missing() {
        let h = SurveillanceRouteHandler::with_cameras(vec![]);
        let resp = h
            .handle(post_req(
                "/api/v1/surveillance/cameras/batch",
                serde_json::json!({"items": [{"rtsp_url": "rtsp://user:pass@10.0.0.9:554/"}]}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["results"][0]["name"], "cam-1", "缺省前缀 cam");
        // 无凭证 → 模板原样入库（user:pass 占位保留）
        assert_eq!(
            resp.body["results"][0]["url"],
            "rtsp://user:pass@10.0.0.9:554/"
        );
    }

    // ========================================================================
    // 快照 / 探测详情 / 进程自愈
    // ========================================================================

    #[test]
    fn build_snapshot_cmd_tokens() {
        let cmd = build_snapshot_cmd("rtsp://x/stream", "/tank/snapshots/cam-1/latest.jpg");
        assert!(cmd.iter().any(|a| a == "-frames:v"), "应抓单帧");
        assert!(cmd.iter().any(|a| a == "1"));
        assert!(cmd.iter().any(|a| a == "-q:v"), "应设质量");
        assert!(cmd.iter().any(|a| a == "tcp"));
        assert!(
            cmd.iter().any(|a| a.ends_with("/latest.jpg")),
            "应输出 latest.jpg"
        );
    }

    #[test]
    fn parse_stream_info_from_ffmpeg_stderr() {
        let h264 = "Input #0, rtsp, from 'rtsp://x':\n  Duration: N/A, start: 0.000000, bitrate: N/A\n    Stream #0:0: Video: h264 (High), yuv420p(progressive), 1920x1080 [SAR 1:1 DAR 16:9], 25 fps, 25 tbr, 90k tbn\n";
        let info = parse_stream_info(h264).unwrap();
        assert_eq!(info.codec, "h264");
        assert_eq!(info.resolution, "1920x1080");

        let mjpeg =
            "    Stream #0:0: Video: mjpeg, yuvj422p(pc, bt470bg), 1280x720, 15 fps, 15 tbr\n";
        let info = parse_stream_info(mjpeg).unwrap();
        assert_eq!(info.codec, "mjpeg");
        assert_eq!(info.resolution, "1280x720");

        // 无分辨率（仍给编码）
        let nores = "    Stream #0:0: Video: rv40 (RealVideo 4.0), none\n";
        let info = parse_stream_info(nores).unwrap();
        assert_eq!(info.codec, "rv40");
        assert_eq!(info.resolution, "");

        // 无 Video 行 → None
        assert!(parse_stream_info("Input #0 ... Audio: aac").is_none());
        assert!(parse_stream_info("").is_none());
    }

    #[tokio::test]
    async fn snapshot_get_missing_returns_404() {
        let h = SurveillanceRouteHandler::with_cameras(vec![]);
        let resp = h
            .handle(get_req(
                "/api/v1/surveillance/cameras/cam-no-snap-xyz-999/snapshot",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
        assert!(resp.body["error"].as_str().unwrap().contains("快照"));
    }

    #[tokio::test]
    async fn snapshot_view_reads_latest_jpg() {
        use base64::Engine as _;
        let id = format!("cam-snapview-{}", std::process::id());
        let dir = format!("/tmp/os-snapshots/{id}");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(format!("{dir}/latest.jpg"), b"fake-jpeg-bytes").unwrap();
        let view = SurveillanceRouteHandler::read_latest_snapshot(&id).unwrap();
        assert_eq!(view.camera_id, id);
        assert!(view.path.ends_with("latest.jpg"));
        assert!(
            view.data_url.starts_with("data:image/jpeg;base64,")
                && view.data_url.ends_with(
                    &base64::engine::general_purpose::STANDARD.encode(b"fake-jpeg-bytes")
                )
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn list_reconciles_dead_pids() {
        // c1: stream pid 已死 → 清 pid + offline；c2: record pid 已死 → 停录像标记；
        // c3: 本测试进程 pid（必活）→ 保留 online
        let mut c1 = make_cam("cam-1", "online", false);
        c1.stream_pid = Some(4_000_000_000);
        let mut c2 = make_cam("cam-2", "recording", true);
        c2.record_pid = Some(4_000_000_001);
        let mut c3 = make_cam("cam-3", "online", false);
        c3.stream_pid = Some(std::process::id());
        let h = SurveillanceRouteHandler::with_cameras(vec![c1, c2, c3]);
        let resp = h
            .handle(get_req("/api/v1/surveillance/cameras"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().unwrap();
        assert!(arr[0]["stream_pid"].is_null(), "死 stream_pid 应清除");
        assert_eq!(arr[0]["status"], "offline");
        assert_eq!(arr[1]["recording"], false, "死 record_pid 应停录像标记");
        assert_eq!(arr[1]["status"], "offline");
        assert!(arr[2]["stream_pid"].is_number(), "活 pid 应保留");
        assert_eq!(arr[2]["status"], "online");
    }
}
