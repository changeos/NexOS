//! `StreamingRouteHandler` —— 流媒体中心桌面应用的 HTTP→内存态流编排适配器。
//!
//! 定位：把网关 HTTP 请求（`/api/v1/streaming/*`）翻译为流媒体编排，返回 JSON。
//! 这是 OS"流媒体中心"桌面应用（拉流/转码/推流/多机位切换）的后端 REST 入口。
//!
//! # 五个功能域
//!
//! - **拉流源（sources）**：管理 RTSP/RTMP/SRT/HTTP 拉流源，可启停录制。
//! - **节目输出（program）**：多机位切换的主输出（active_source + 预览列表）。
//! - **转码任务（transcode）**：FFmpeg NVENC 转码子进程管理（VOD mp4→HLS / 实时重编）。
//! - **推流目标（outputs）**：管理 RTMP/SRT 推流到外部平台（YouTube/B 站等）。
//! - **统计（stats）**：聚合各域计数。
//!
//! # 实现策略：内存态 + 调度框架（真实数据，无 demo 预置）
//!
//! `new()` 启动时空（sources / transcodes / outputs 全部空 vec![]），由用户自行添加。
//! 持有四把 `Mutex`（sources / transcodes / outputs / program）。MediaMTX / ffmpeg 的
//! 真实编排做成"调度框架"——管理任务生命周期 + 命令构造。转码子进程 spawn 真实
//! `tokio::process::Command`，拿到 pid 存入 task（后台跑）；**不强求 MediaMTX 在线**，
//! MediaMTX 不在线时降级为"已记录意图"，不 panic。FFmpeg 不存在时 task.status=failed，
//! 不报错。这样保证：编译通过 + 命令构造正确（纯函数可单测） + 测试可跑（不依赖外部进程）。
//!
//! # 转码对接真实文件
//!
//! - `GET /api/v1/streaming/transcode/sources` 扫描 `/tank/media/video/`（真盘优先），
//!   返回可用的真实本地视频文件，供前端在创建转码任务时选择输入。
//! - 创建 vod 任务时真实 spawn ffmpeg 生成 HLS：`-hwaccel auto -i <input> -c:v <codec>
//!   ... -f hls <output_dir>/index.m3u8`。用 `-hwaccel auto` 而非硬编码 `cuda`：
//!   ffmpeg 优先尝试 CUDA 硬解，不可用时自动回退软解（避免 detached 进程在无 GPU
//!   上下文时因 `-hwaccel cuda` 硬失败）。编码器仍用 nvenc（GPU 编码）。
//! - output_dir 默认 `/tank/hls/<name>`；创建目录失败（/tank 可能无写权限）时降级到
//!   `/tmp/os-hls/<name>`，error 字段记录 warning 但 status 仍 running，绝不 panic。
//! - 进程存活检测：list/detail 转码任务时用 `kill -0 <pid>` 探测，进程已退出则把
//!   running 状态推进为 completed（progress 100）。
//!
//! # 本机能力（已验证）
//!
//! - ffmpeg 8.0.1，编码器 h264_nvenc/hevc_nvenc/av1_nvenc 全可用；`-hwaccel auto`
//!   优先 CUDA 硬解、不可用回退软解（GPU 型号由 ffmpeg 自行探测，代码不硬编码）
//! - 真实样本：`/tank/media/video/test-video-{1,2,3}.mp4`
//! - MediaMTX 单二进制：REST API :9997 / RTSP :8554 / RTMP :1935 / SRT/WebRTC :8889 / HLS :8888
//!
//! # 路由表（18 条）
//!
//! | method | path                                       | 动作 |
//! |--------|--------------------------------------------|------|
//! | GET    | `/api/v1/streaming/sources`                | 列拉流源 |
//! | POST   | `/api/v1/streaming/sources`                | 添加源（需 admin）|
//! | DELETE | `/api/v1/streaming/sources/:id`            | 删源（需 admin）|
//! | POST   | `/api/v1/streaming/sources/:id/record/start` | 开始录制（需 admin）|
//! | POST   | `/api/v1/streaming/sources/:id/record/stop`  | 停止录制（需 admin）|
//! | GET    | `/api/v1/streaming/program`                | 取当前主输出 + 预览源 |
//! | POST   | `/api/v1/streaming/program/switch`         | 切换主输出（需 admin）|
//! | GET    | `/api/v1/streaming/transcode`              | 列转码任务 |
//! | GET    | `/api/v1/streaming/transcode/sources`      | 可用本地视频文件（转码输入源）|
//! | GET    | `/api/v1/streaming/transcode/:id`          | 转码任务详情（刷新状态）|
//! | POST   | `/api/v1/streaming/transcode`              | 创建转码任务（需 admin）|
//! | DELETE | `/api/v1/streaming/transcode/:id`          | 取消/删除任务（需 admin）|
//! | GET    | `/api/v1/streaming/outputs`                | 列推流目标 |
//! | POST   | `/api/v1/streaming/outputs`                | 添加推流目标（需 admin）|
//! | DELETE | `/api/v1/streaming/outputs/:id`            | 删推流目标（需 admin）|
//! | POST   | `/api/v1/streaming/outputs/:id/start`      | 启动推流（拉流转推，需 admin）|
//! | POST   | `/api/v1/streaming/outputs/:id/stop`       | 停止推流（需 admin）|
//! | GET    | `/api/v1/streaming/stats`                  | 聚合统计 |
//!
//! # 引擎门控（2026-09-05：流媒体中心剥离为独立应用，docs/APPS.md §7）
//!
//! streaming 引擎**内置**于 os-api（代码仍编译在二进制内），但按「装了应用
//! 才启用」架构运行（film 同款）：未安装声明 `engine="streaming"` 的应用包
//! （经应用中心安装，apps 表登记）时，上表全部业务端点一律 404
//! `{"error":"应用「流媒体中心」未安装：可在 应用中心 → 商店 安装"}`。门控
//! 每请求直查 apps 表（`AppRegistry::is_engine_enabled`，无缓存）——安装/卸载
//! **即时生效**；表损坏/锁失败 fail-closed（按未装处理）。未注入注册表
//! （单测直构）不门控，既有测试契约不变；生产 main.rs 恒注入。
//!
//! 与 P2P 联邦直播（live）的关系：`/api/v1/live/*` 是独立组件
//! （`handlers/live.rs`，component="live"），属 P2P 联邦基础能力**常开不门控**；
//! 本 handler 不代理任何 live 路由（transcode 的 `mode=live` 只是任务模式
//! 字符串），故上表 18 条整表门控。

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

/// 进程级共享 `reqwest::Client`（rustify：MediaMTX 探活的 curl 子进程 → reqwest）。
static HTTP: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("构建共享 reqwest Client 失败")
});

// MediaMTX REST API 默认地址（本机单二进制）。
const MEDIAMTX_API_BASE: &str = "http://localhost:9997";
/// MediaMTX RTSP 推流入口（实时转码推回的目标）。
const MEDIAMTX_RTSP_BASE: &str = "rtsp://localhost:8554";

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 拉流源（RTSP/RTMP/SRT/HTTP）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamSource {
    pub id: String,
    pub name: String,
    /// 源地址：rtsp://... / rtmp://... / srt://... / http://...
    pub url: String,
    /// 协议：rtsp / rtmp / srt / http / webrtc
    pub protocol: String,
    /// 分辨率标签：sd / 720p / 1080p / 2k / 4k / panorama（全景）
    pub resolution_tag: String,
    /// 状态：idle / connecting / live / error
    pub status: String,
    pub recording: bool,
    /// 是否同时保存本地录制（默认 false）。开启录制且为 true 时另起 ffmpeg 落盘。
    #[serde(default)]
    pub record_local: bool,
    /// 本地保存目录（如 `/tank/recordings/sources/<id>/`）。None 时后端按默认生成。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_path: Option<String>,
    /// 本地录制 ffmpeg 子进程 pid（recording 且 record_local 时）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_pid: Option<u32>,
    pub created_at: String,
}

/// 转码 ladder 的一档（多码率自适应）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolutionRung {
    /// 标签："4K" / "2K" / "1080p" / "720p"
    pub label: String,
    pub width: u32,
    pub height: u32,
    /// 码率："25M" / "12M" / "8M" / "4M"
    pub bitrate: String,
}

/// 转码任务（VOD mp4→HLS 或 实时 rtsp→重编→推回）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscodeTask {
    pub id: String,
    pub name: String,
    /// 输入：源 URL 或本地文件路径 /tank/media/video/xxx.mp4
    pub input: String,
    /// 输出目录 `/tank/hls/<name>/`
    pub output_dir: String,
    /// 模式：vod（点播 mp4→hls）/ live（实时 rtsp→重编→推回）
    pub mode: String,
    /// 编码器：h264_nvenc / hevc_nvenc / av1_nvenc / libx264
    pub codec: String,
    /// 多码率 ladder（空 = 单码率）
    pub ladder: Vec<ResolutionRung>,
    /// 状态：queued / running / completed / failed / cancelled
    pub status: String,
    /// 进度 0-100
    pub progress: u8,
    /// ffmpeg 子进程 pid（running 时）
    pub pid: Option<u32>,
    pub error: Option<String>,
    pub created_at: String,
}

/// `GET /api/v1/streaming/transcode/sources` 返回的可用本地视频文件条目。
///
/// 真盘扫描 `/tank/media/video/` 得到的真实文件（`demo: false`）。前端创建转码任务时
/// 可从中选择 input。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscodeSource {
    /// 文件绝对路径（作为转码 input）
    pub path: String,
    /// 文件名（含扩展名）
    pub name: String,
    /// 文件大小（字节）
    pub size_bytes: u64,
}

/// 推流目标（RTMP/SRT 推到 YouTube/B 站等）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamOutput {
    pub id: String,
    pub name: String,
    /// 目标 URL：rtmp://youtube.../key / srt://...
    pub url: String,
    /// 协议：rtmp / srt
    pub protocol: String,
    /// 绑定哪个拉流源
    pub source_id: Option<String>,
    pub enabled: bool,
    /// 状态：idle / pushing / error
    pub status: String,
    /// ffmpeg 推流子进程 pid（pushing 时）
    pub pid: Option<u32>,
    /// 是否同时保存本地录制（转推时边推边录）。默认 false。
    #[serde(default)]
    pub record_local: bool,
    /// 本地保存目录（如 `/tank/recordings/outputs/<id>/`）。None 时后端按默认生成。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_path: Option<String>,
    /// 本地录制 ffmpeg 子进程 pid（pushing 且 record_local 时）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_pid: Option<u32>,
    pub created_at: String,
}

/// 节目输出（多机位切换的主输出）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgramOut {
    pub active_source_id: Option<String>,
    /// 预览中的源 id 列表
    pub sources_preview: Vec<String>,
}

/// `GET /api/v1/streaming/stats` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamingStats {
    pub sources_total: usize,
    pub sources_live: usize,
    pub sources_recording: usize,
    pub transcodes_total: usize,
    pub transcodes_running: usize,
    pub transcodes_completed: usize,
    pub transcodes_failed: usize,
    pub outputs_total: usize,
    pub outputs_pushing: usize,
    pub program_has_active: bool,
}

/// 添加拉流源请求体。
#[derive(Debug, Deserialize)]
struct CreateSourceBody {
    name: String,
    url: String,
    #[serde(default)]
    protocol: Option<String>,
    #[serde(default)]
    resolution_tag: Option<String>,
    /// 是否同时保存本地录制（默认 false）。
    #[serde(default)]
    record_local: Option<bool>,
    /// 本地保存目录（留空则后端按默认生成 /tank/recordings/sources/<id>/）。
    #[serde(default)]
    record_path: Option<String>,
}

/// 创建转码任务请求体。
#[derive(Debug, Deserialize)]
struct CreateTranscodeBody {
    name: String,
    input: String,
    #[serde(default)]
    output_dir: Option<String>,
    /// vod / live，默认 vod
    #[serde(default)]
    mode: Option<String>,
    /// 默认 hevc_nvenc
    #[serde(default)]
    codec: Option<String>,
    /// 可选 ladder（空 = 单码率）
    #[serde(default)]
    ladder: Option<Vec<ResolutionRung>>,
    /// 是否立即 spawn（默认 false：仅 queued；测试用避免真起 ffmpeg）
    #[serde(default)]
    autostart: Option<bool>,
}

/// 添加推流目标请求体。
#[derive(Debug, Deserialize)]
struct CreateOutputBody {
    name: String,
    url: String,
    #[serde(default)]
    protocol: Option<String>,
    #[serde(default)]
    source_id: Option<String>,
    /// 是否同时保存本地录制（转推时边推边录，默认 false）。
    #[serde(default)]
    record_local: Option<bool>,
    /// 本地保存目录（留空则后端按默认生成 /tank/recordings/outputs/<id>/）。
    #[serde(default)]
    record_path: Option<String>,
}

/// 切换主输出请求体。
#[derive(Debug, Deserialize)]
struct SwitchProgramBody {
    source_id: String,
}

// ----------------------------------------------------------------------------
// FFmpeg 命令构造器（纯函数，易测试）
// ----------------------------------------------------------------------------

/// 构造 VOD 转码命令（mp4 → HLS，单码率或多码率 ladder）。
///
/// 单码率：`-hwaccel auto -i input -c:v <codec> -b:v <bitrate> -c:a aac`
/// 多码率 ladder：用 `-filter_complex split + scale` 软件缩放（稳妥可跑）+ 硬件编码，
/// 产出 master.m3u8 + 各 rung 独立 m3u8。
///
/// 返回的 Vec 不含 `ffmpeg` 程序名（caller 负责拼 `Command::new("ffmpeg").args(...)`）。
#[must_use]
pub fn build_vod_transcode_cmd(task: &TranscodeTask) -> Vec<String> {
    // ladder 为空时取一个默认档（1080p / 8M），保证命令可跑
    let default_rung = ResolutionRung {
        label: "1080p".into(),
        width: 1920,
        height: 1080,
        bitrate: "8M".into(),
    };
    if task.ladder.is_empty() {
        // —— 单码率 ——
        let r = &default_rung;
        let index = format!("{}/index.m3u8", task.output_dir);
        let seg = format!("{}/stream_%03d.ts", task.output_dir);
        vec![
            "-hwaccel".into(),
            "auto".into(),
            "-i".into(),
            task.input.clone(),
            "-vf".into(),
            format!("scale={}:{}", r.width, r.height),
            "-c:v".into(),
            task.codec.clone(),
            "-b:v".into(),
            r.bitrate.clone(),
            "-c:a".into(),
            "aac".into(),
            "-f".into(),
            "hls".into(),
            "-hls_time".into(),
            "6".into(),
            "-hls_playlist_type".into(),
            "vod".into(),
            "-hls_segment_filename".into(),
            seg,
            index,
        ]
    } else {
        // —— 多码率 ladder：filter_complex split + scale + master playlist ——
        let mut args: Vec<String> = vec![
            "-hwaccel".into(),
            "auto".into(),
            "-i".into(),
            task.input.clone(),
        ];
        // filter_complex: [0:v]split=<n>[v0]...[v<n-1>]; 每个 [vi] scale=w:h
        let n = task.ladder.len();
        let split_chain: String = format!(
            "[0:v]split={n}[{}]",
            (0..n)
                .map(|i| format!("v{i}"))
                .collect::<Vec<_>>()
                .join("][")
        );
        let scale_chains: Vec<String> = task
            .ladder
            .iter()
            .enumerate()
            .map(|(i, r)| format!("[v{i}]scale={}:{}[s{i}]", r.width, r.height))
            .collect();
        let filter = format!("{};{}", split_chain, scale_chains.join(";"));
        args.push("-filter_complex".into());
        args.push(filter);
        // 映射每个 rung 的缩放后视频流：-map "[s{i}]"（两个独立 token）
        // 注意：-map 与流说明符必须分两个 arg，否则 ffmpeg 报
        // "Trailing garbage after stream specifier"。
        for (i, _r) in task.ladder.iter().enumerate() {
            args.push("-map".into());
            args.push(format!("[s{i}]"));
        }
        // 音频映射（可选：a:0?，无音频流时不报错）+ hls master playlist
        args.extend(["-map".into(), "a:0?".into()]);
        // 每个 rung 的编码器 + 码率（-c:v:{i} / -b:v:{i}）
        for (i, r) in task.ladder.iter().enumerate() {
            args.push(format!("-c:v:{i}"));
            args.push(task.codec.clone());
            args.push(format!("-b:v:{i}"));
            args.push(r.bitrate.clone());
        }
        args.extend([
            "-c:a".into(),
            "aac".into(),
            "-f".into(),
            "hls".into(),
            "-hls_time".into(),
            "6".into(),
            "-master_pl_name".into(),
            "master.m3u8".into(),
            "-var_stream_map".into(),
            // var_stream_map 格式：空格分隔的流描述符，每个形如 "v:0"、"v:1"。
            // 注意：不要在开头加多余的 "v "（旧实现的 bug 导致 "v v:0" 被 muxer 拒绝，
            // 报 "Invalid keyval v" / "Variant stream info update failed"）。
            (0..n)
                .map(|i| format!("v:{i}"))
                .collect::<Vec<_>>()
                .join(" "),
            "-hls_segment_filename".into(),
            format!("{}/stream_%v_%03d.ts", task.output_dir),
            format!("{}/stream_%v.m3u8", task.output_dir),
        ]);
        args
    }
}

/// 构造实时转码命令（rtsp 拉流 → 重编码 → 推回 MediaMTX）。
///
/// `ffmpeg -rtsp_transport tcp -i <input> -c:v <codec> -b:v <bitrate> -f rtsp rtsp://localhost:8554/<name>`
///
/// 码率优先取 ladder 第一档；空则取默认 1080p/8M。
#[must_use]
pub fn build_live_transcode_cmd(task: &TranscodeTask) -> Vec<String> {
    let (width, height, bitrate) = task
        .ladder
        .first()
        .map(|r| (r.width, r.height, r.bitrate.clone()))
        .unwrap_or((1920, 1080, "8M".to_string()));
    let target = format!("{MEDIAMTX_RTSP_BASE}/{}", task.name);
    vec![
        "-rtsp_transport".into(),
        "tcp".into(),
        "-i".into(),
        task.input.clone(),
        "-vf".into(),
        format!("scale={width}:{height}"),
        "-c:v".into(),
        task.codec.clone(),
        "-b:v".into(),
        bitrate,
        "-c:a".into(),
        "aac".into(),
        "-f".into(),
        "rtsp".into(),
        target,
    ]
}

/// 构造"拉流转推流"命令：从 input（拉流源 URL 或本地文件）拉流，重封装推到 output.url。
///
/// - input 是 rtsp:// → 加 `-rtsp_transport tcp`
/// - input 是 rtmp/srt/http/本地文件 → 直接 `-i <input>`
/// - 默认 `-c copy`（不重编码，纯转封装，低延迟零损耗）
/// - output 协议 rtsp → `-f rtsp`；rtmp → `-f flv`；srt → `-f mpegts`
/// - 加 `-re`（按原始帧率读，推流必须）+ `-fflags +genpts`
///
/// 形如：`ffmpeg -re -fflags +genpts [-rtsp_transport tcp] -i <input> -c copy -f <fmt> <output>`
///
/// 返回的 Vec 不含 `ffmpeg` 程序名（caller 负责拼 `Command::new("ffmpeg").args(...)`）。
#[must_use]
pub fn build_relay_cmd(
    input: &str,
    input_protocol: &str,
    output: &str,
    output_protocol: &str,
) -> Vec<String> {
    let mut args: Vec<String> = vec!["-re".into(), "-fflags".into(), "+genpts".into()];
    // rtsp 拉流强制 tcp（避免 UDP 丢包）
    if input_protocol == "rtsp" {
        args.push("-rtsp_transport".into());
        args.push("tcp".into());
    }
    args.push("-i".into());
    args.push(input.into());
    // 纯转封装（不重编码，低延迟零损耗）
    args.push("-c".into());
    args.push("copy".into());
    // 输出格式映射
    let fmt = match output_protocol {
        "rtsp" => "rtsp",
        "rtmp" => "flv",
        "srt" => "mpegts",
        _ => "flv", // 默认 flv（rtmp 兼容）
    };
    args.push("-f".into());
    args.push(fmt.into());
    args.push(output.into());
    args
}

/// 构造"拉流并保存本地"命令：从 input 拉流，转封装写本地 mp4 文件。
///
/// - input 是 rtsp:// → 加 `-rtsp_transport tcp`（避免 UDP 丢包）
/// - `-c copy`（纯转封装，零损耗、低 CPU）
/// - 输出固定 mp4（兼容性好，可作为存档）
/// - 加 `-t` 超长上限（避免录制无限挂起；此处不加 -t，由调用方 record/stop 显式 kill）
///
/// 形如：`ffmpeg [-rtsp_transport tcp] -i <input> -c copy -f mp4 -movflags +faststart <outfile>`
///
/// 返回的 Vec 不含 `ffmpeg` 程序名（caller 负责拼 `Command::new("ffmpeg").args(...)`）。
#[must_use]
pub fn build_record_cmd(input: &str, input_protocol: &str, outfile: &str) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if input_protocol == "rtsp" {
        args.push("-rtsp_transport".into());
        args.push("tcp".into());
    }
    args.push("-i".into());
    args.push(input.into());
    args.push("-c".into());
    args.push("copy".into());
    args.push("-f".into());
    args.push("mp4".into());
    args.push("-movflags".into());
    args.push("+faststart".into());
    args.push(outfile.into());
    args
}

/// 构造添加拉流源到 MediaMTX 的配置 JSON（用于 `POST /v2/config/paths/add`）。
///
/// 根据 protocol 推断 sourceProtocol（rtsp 用 tcp，其余按 protocol 原样）。
#[must_use]
pub fn build_mediamtx_path_config(source: &StreamSource) -> serde_json::Value {
    let source_protocol = if source.protocol == "rtsp" {
        "tcp"
    } else {
        source.protocol.as_str()
    };
    serde_json::json!({
        "name": source.name,
        "source": source.url,
        "sourceProtocol": source_protocol,
        "sourceOnDemand": false,
        "record": source.recording,
    })
}

/// 检测 MediaMTX 是否在线（`GET :9997/v2/serverstate`）。
///
/// 不在线返回 `false`（不报错）。rustify：原 curl 子进程迁移为共享 reqwest Client
/// GET（2s 超时）；MediaMTX 不在线/网络不通均返回 false。
pub async fn check_mediamtx_alive() -> bool {
    let url = format!("{MEDIAMTX_API_BASE}/v2/serverstate");
    matches!(
        HTTP.get(&url).timeout(Duration::from_secs(2)).send().await,
        Ok(r) if r.status().is_success()
    )
}

// ----------------------------------------------------------------------------
// StreamingRouteHandler
// ----------------------------------------------------------------------------

/// 流媒体中心路由处理器——HTTP 边界适配到内存态流编排。
pub struct StreamingRouteHandler {
    sources: Mutex<Vec<StreamSource>>,
    transcodes: Mutex<Vec<TranscodeTask>>,
    outputs: Mutex<Vec<StreamOutput>>,
    program: Mutex<ProgramOut>,
    counter: Mutex<u64>,
    /// 应用注册表（引擎门控）：注入后每请求查 apps 表——未安装 streaming 应用
    /// 则全部业务端点 404（引擎内置、应用按装启用，docs/APPS.md §7）。None =
    /// 未注入（单测直构），不门控；生产 main.rs 恒注入。
    app_registry: Option<Arc<super::apps_handler::AppRegistry>>,
}

impl StreamingRouteHandler {
    /// 构造 handler——**启动时空**，sources / transcodes / outputs 全部空列表。
    ///
    /// 不再预置 demo 数据；拉流源 / 转码任务 / 推流目标均由用户自行添加。
    /// 节目输出 active_source_id 为 None、预览列表为空。counter 从 100 起。
    #[must_use]
    pub fn new() -> Self {
        Self {
            sources: Mutex::new(vec![]),
            transcodes: Mutex::new(vec![]),
            outputs: Mutex::new(vec![]),
            program: Mutex::new(ProgramOut {
                active_source_id: None,
                sources_preview: vec![],
            }),
            counter: Mutex::new(100),
            app_registry: None,
        }
    }

    /// 链式注入应用注册表（引擎门控开启：未安装 streaming 应用 → 全部业务
    /// 端点 404；与 apps 组件 REST 面共享同一 SQLite，安装/卸载即时生效）。
    /// main.rs 生产装配恒调用；单测不注入则不门控（既有测试契约不变）。
    #[must_use]
    pub fn with_app_registry(mut self, reg: Arc<super::apps_handler::AppRegistry>) -> Self {
        self.app_registry = Some(reg);
        self
    }

    /// 用空列表构造（与 [`Self::new`] 同语义，保留供旧测试注入路径）。
    #[must_use]
    pub fn with_empty() -> Self {
        Self::new()
    }

    /// 当前全量拉流源快照。
    #[must_use]
    pub fn sources_snapshot(&self) -> Vec<StreamSource> {
        self.sources.lock().expect("sources poisoned").clone()
    }

    /// 当前全量转码任务快照。
    #[must_use]
    pub fn transcodes_snapshot(&self) -> Vec<TranscodeTask> {
        self.transcodes.lock().expect("transcodes poisoned").clone()
    }

    /// 当前全量推流目标快照。
    #[must_use]
    pub fn outputs_snapshot(&self) -> Vec<StreamOutput> {
        self.outputs.lock().expect("outputs poisoned").clone()
    }

    /// 当前节目输出快照。
    #[must_use]
    pub fn program_snapshot(&self) -> ProgramOut {
        self.program.lock().expect("program poisoned").clone()
    }

    /// 生成下一个 id。
    fn next_id(&self, prefix: &str) -> String {
        let mut c = self.counter.lock().expect("counter poisoned");
        *c += 1;
        format!("{prefix}-{}", *c)
    }

    /// 统计快照。
    fn stats_snapshot(&self) -> StreamingStats {
        let sources = self.sources.lock().expect("sources poisoned");
        let transcodes = self.transcodes.lock().expect("transcodes poisoned");
        let outputs = self.outputs.lock().expect("outputs poisoned");
        let program = self.program.lock().expect("program poisoned");
        let sources_live = sources.iter().filter(|s| s.status == "live").count();
        let sources_recording = sources.iter().filter(|s| s.recording).count();
        let transcodes_running = transcodes.iter().filter(|t| t.status == "running").count();
        let transcodes_completed = transcodes
            .iter()
            .filter(|t| t.status == "completed")
            .count();
        let transcodes_failed = transcodes.iter().filter(|t| t.status == "failed").count();
        let outputs_pushing = outputs.iter().filter(|o| o.status == "pushing").count();
        StreamingStats {
            sources_total: sources.len(),
            sources_live,
            sources_recording,
            transcodes_total: transcodes.len(),
            transcodes_running,
            transcodes_completed,
            transcodes_failed,
            outputs_total: outputs.len(),
            outputs_pushing,
            program_has_active: program.active_source_id.is_some(),
        }
    }

    /// 真实 spawn ffmpeg 子进程，成功返回 pid。
    ///
    /// 用 `std::process::Command` 而非 tokio 的：我们要 fire-and-forget（后台跑，
    /// 不 await 完成），drop 句柄后进程由 OS 收养继续运行。tokio 的 `Child` 在
    /// drop 时对 detached 进程的处理不如 std 稳妥。ffmpeg 不存在 / spawn 失败返回
    /// Err（caller 降级为 failed）。
    fn spawn_ffmpeg(args: &[String]) -> Result<u32, String> {
        let mut cmd = std::process::Command::new("ffmpeg");
        cmd.args(args);
        // stdout/stdin 静默；stderr 重定向到日志文件（便于诊断 ffmpeg 失败原因）
        cmd.stdout(std::process::Stdio::null());
        cmd.stdin(std::process::Stdio::null());
        let stderr_log = std::env::temp_dir().join(format!("os-ffmpeg-{}.log", std::process::id()));
        let stderr_file = std::fs::File::create(&stderr_log)
            .map(std::process::Stdio::from)
            .unwrap_or(std::process::Stdio::null());
        cmd.stderr(stderr_file);
        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id();
                // 不等待：drop 后由 OS 收养（后台继续跑）
                drop(child);
                Ok(pid)
            }
            Err(e) => Err(format!("spawn ffmpeg 失败: {e}")),
        }
    }

    /// 杀掉转码子进程（SIGTERM）。pid 无效或 kill 失败返回 Err，但仍允许从列表删。
    fn kill_transcode(pid: u32) -> Result<(), String> {
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

    /// 探测 pid 对应进程是否仍"有效存活"（非僵尸）。
    ///
    /// 用于转码任务的轻量状态推进：spawn 后 child 不 await（后台跑），每次 list/detail
    /// 转码任务时探测 pid，若已退出（或成僵尸——ffmpeg 失败后未被 wait 会变 defunct）
    /// 则把 running 推进为 completed（progress 100）。
    ///
    /// 实现：先 `kill -0`；若存活再读 `/proc/<pid>/stat`，状态字符为 `Z`（僵尸）视为
    /// 已死。无 /proc（非 Linux）时退化为仅 `kill -0`。
    #[must_use]
    fn pid_alive(pid: u32) -> bool {
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !alive {
            return false;
        }
        // 僵尸进程（ffmpeg 已退出但未被 wait）视为已死
        let stat_path = format!("/proc/{pid}/stat");
        if let Ok(content) = std::fs::read_to_string(&stat_path) {
            // /proc/<pid>/stat 格式：pid (comm) state ...；state 在最后 ')' 之后
            if let Some(after_comm) = content.rsplit(')').next() {
                let state = after_comm.trim_start().chars().next().unwrap_or(' ');
                if state == 'Z' {
                    return false; // 僵尸，视为已退出
                }
            }
        }
        true
    }

    /// 刷新单个转码任务状态：若 status=="running" 且 pid 已不存活 → 据输出文件判定
    /// completed（生成了 m3u8）或 failed（无输出，ffmpeg 失败）。
    ///
    /// 返回是否被本次调用改写（用于 detail 响应反映最新状态）。
    fn refresh_one_transcode(&self, id: &str) -> bool {
        let mut transcodes = self.transcodes.lock().expect("transcodes poisoned");
        if let Some(t) = transcodes.iter_mut().find(|t| t.id == id) {
            if t.status == "running" {
                if let Some(pid) = t.pid {
                    if !Self::pid_alive(pid) {
                        if Self::transcode_succeeded(&t.output_dir) {
                            t.status = "completed".into();
                            t.progress = 100;
                        } else {
                            t.status = "failed".into();
                            if t.error.is_none() {
                                t.error = Some(
                                    "ffmpeg 已退出但未生成 HLS 输出（检查输入文件/编解码器）"
                                        .into(),
                                );
                            }
                        }
                        return true;
                    }
                }
            }
        }
        false
    }

    /// 批量刷新所有转码任务（用于 list / stats 端点保持状态新鲜）。
    fn refresh_all_transcodes(&self) {
        let mut transcodes = self.transcodes.lock().expect("transcodes poisoned");
        for t in transcodes.iter_mut() {
            if t.status == "running" {
                if let Some(pid) = t.pid {
                    if !Self::pid_alive(pid) {
                        if Self::transcode_succeeded(&t.output_dir) {
                            t.status = "completed".into();
                            t.progress = 100;
                        } else {
                            t.status = "failed".into();
                            if t.error.is_none() {
                                t.error = Some(
                                    "ffmpeg 已退出但未生成 HLS 输出（检查输入文件/编解码器）"
                                        .into(),
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// 检查转码输出目录是否含 .m3u8 文件（判定 ffmpeg 是否成功产出 HLS）。
    #[must_use]
    fn transcode_succeeded(output_dir: &str) -> bool {
        let path = Path::new(output_dir);
        if !path.is_dir() {
            return false;
        }
        std::fs::read_dir(path)
            .map(|entries| {
                entries.flatten().any(|e| {
                    e.path()
                        .extension()
                        .and_then(|x| x.to_str())
                        .map(|x| x.eq_ignore_ascii_case("m3u8"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    }

    /// 解析转码输出目录并保证其存在。
    ///
    /// 优先 `/tank/hls/<name>`；若该目录无法创建（/tank 可能无写权限，或路径不可达），
    /// 降级到 `/tmp/os-hls/<name>`。返回 (实际目录, Option<warning>)——warning 不影响
    /// status=running，仅记录到 task.error 供前端提示。
    #[must_use]
    fn resolve_output_dir(preferred: &str, name: &str) -> (String, Option<String>) {
        let clean = |s: &str| s.trim_end_matches('/').to_string();
        // 先尝试 preferred（调用方可能已带 /tank/hls/<name> 或 /tmp/... 自定义）
        let pref = clean(preferred);
        if std::fs::create_dir_all(&pref).is_ok() {
            return (pref, None);
        }
        // 降级到 /tmp/os-hls/<name>
        let fallback = clean(&format!("/tmp/os-hls/{name}"));
        let _ = std::fs::create_dir_all(&fallback);
        (
            fallback.clone(),
            Some(format!("无法创建输出目录 {pref}（降级到 {fallback}）")),
        )
    }

    /// 解析录制保存目录并保证其存在（同 [`resolve_output_dir`] 语义，但默认根不同）。
    ///
    /// preferred 为空时按 kind（"sources"/"outputs"）+ key 生成默认路径
    /// `/tank/recordings/<kind>/<key>/`；preferred 非空则用之。/tank 不可写降级
    /// `/tmp/recordings/<kind>/<key>/`。返回 (实际目录, Option<warning>)。
    #[must_use]
    fn resolve_record_dir(
        preferred: Option<&str>,
        kind: &str,
        key: &str,
    ) -> (String, Option<String>) {
        let clean = |s: &str| s.trim_end_matches('/').to_string();
        let pref = match preferred.map(clean).filter(|s| !s.is_empty()) {
            Some(p) => p,
            None => format!("/tank/recordings/{kind}/{key}"),
        };
        if std::fs::create_dir_all(&pref).is_ok() {
            return (pref, None);
        }
        let fallback = clean(&format!("/tmp/recordings/{kind}/{key}"));
        let _ = std::fs::create_dir_all(&fallback);
        (
            fallback.clone(),
            Some(format!("无法创建录制目录 {pref}（降级到 {fallback}）")),
        )
    }

    /// 生成一个带时间戳的录制文件名（mp4）。
    #[must_use]
    fn record_filename() -> String {
        use chrono::Local;
        let ts = Local::now().format("%Y%m%d-%H%M%S");
        format!("rec-{ts}.mp4")
    }
}

impl Default for StreamingRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RouteHandler for StreamingRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            // —— 拉流源 ——
            spec(HttpMethod::Get, "/api/v1/streaming/sources", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/streaming/sources",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/streaming/sources/:id",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/streaming/sources/:id/record/start",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/streaming/sources/:id/record/stop",
                true,
                vec!["admin".into()],
            ),
            // —— 节目输出 ——
            spec(HttpMethod::Get, "/api/v1/streaming/program", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/streaming/program/switch",
                true,
                vec!["admin".into()],
            ),
            // —— 转码任务 ——
            spec(
                HttpMethod::Get,
                "/api/v1/streaming/transcode",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/streaming/transcode/sources",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/streaming/transcode/:id",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/streaming/transcode",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/streaming/transcode/:id",
                true,
                vec!["admin".into()],
            ),
            // —— 推流目标 ——
            spec(HttpMethod::Get, "/api/v1/streaming/outputs", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/streaming/outputs",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/streaming/outputs/:id",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/streaming/outputs/:id/start",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/streaming/outputs/:id/stop",
                true,
                vec!["admin".into()],
            ),
            // —— 统计 ——
            spec(HttpMethod::Get, "/api/v1/streaming/stats", false, vec![]),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        // —— 引擎门控（2026-09-05：流媒体中心剥离为独立应用，docs/APPS.md §7）——
        // streaming 引擎代码仍编译在 os-api（引擎内置），但未安装 streaming
        // 应用时**零入口零可用**：全部业务端点 404 + 安装指引（语义对齐手机
        // 系统+应用）。每请求直查 apps 表（无缓存）——安装/卸载即时生效；表
        // 损坏/锁失败 fail-closed（按未装处理）。未注入注册表（单测直构）不
        // 门控，既有测试契约不变。注：P2P 联邦直播（/api/v1/live/*）是独立
        // 组件（live.rs），不经本 handler，常开不门控。
        if let Some(reg) = &self.app_registry {
            if !reg.is_engine_enabled("streaming") {
                return Ok(error_response(
                    404,
                    "应用「流媒体中心」未安装：可在 应用中心 → 商店 安装",
                ));
            }
        }
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            // ===================== 拉流源 =====================
            // —— GET /api/v1/streaming/sources —— 列全部
            (HttpMethod::Get, ["api", "v1", "streaming", "sources"]) => {
                Ok(ok_json(to_value(&self.sources_snapshot())?))
            }

            // —— POST /api/v1/streaming/sources —— 添加源
            (HttpMethod::Post, ["api", "v1", "streaming", "sources"]) => {
                let body: CreateSourceBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析添加拉流源请求体失败: {e}"))
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
                let resolution_tag = body
                    .resolution_tag
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "1080p".to_string());
                let record_local = body.record_local.unwrap_or(false);
                // record_path：用户指定 → 用之；否则留 None，待 record/start 时按默认生成。
                // （这里不预创建目录，避免空源也建目录。）
                let src = StreamSource {
                    id: self.next_id("src"),
                    name: body.name,
                    url: body.url,
                    protocol,
                    resolution_tag,
                    status: "idle".into(),
                    recording: false,
                    record_local,
                    record_path: body
                        .record_path
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                    record_pid: None,
                    created_at: now_iso(),
                };
                // 同时加入节目预览列表
                self.program
                    .lock()
                    .expect("program poisoned")
                    .sources_preview
                    .push(src.id.clone());
                let resp_body = to_value(&src)?;
                self.sources.lock().expect("sources poisoned").push(src);
                Ok(ApiResponse {
                    status: 201,
                    body: resp_body,
                    headers: serde_json::json!({}),
                })
            }

            // —— DELETE /api/v1/streaming/sources/:id —— 删源
            (HttpMethod::Delete, ["api", "v1", "streaming", "sources", id]) => {
                let mut sources = self.sources.lock().expect("sources poisoned");
                let before = sources.len();
                sources.retain(|s| s.id != *id);
                if sources.len() == before {
                    return Ok(error_response(404, &format!("拉流源不存在: {id}")));
                }
                // 同步从节目预览/主输出移除
                let mut program = self.program.lock().expect("program poisoned");
                program.sources_preview.retain(|s| s != *id);
                if program.active_source_id.as_deref() == Some(*id) {
                    program.active_source_id = sources.first().map(|s| s.id.clone());
                }
                Ok(ok_json(
                    serde_json::json!({"ok": true, "id": id, "action": "delete"}),
                ))
            }

            // —— POST /api/v1/streaming/sources/:id/record/start —— 开始录制
            (HttpMethod::Post, ["api", "v1", "streaming", "sources", id, "record", "start"]) => {
                // 先快照源（含 url/protocol/record_local/record_path）——锁立即释放，避免
                // 在 await ffmpeg spawn 时持锁。
                let snap = {
                    let sources = self.sources.lock().expect("sources poisoned");
                    sources.iter().find(|s| s.id == *id).cloned()
                };
                let s = match snap {
                    Some(s) => s,
                    None => return Ok(error_response(404, &format!("拉流源不存在: {id}"))),
                };
                // 若 record_local：另起 ffmpeg 把流落盘到本地 mp4（真实录制）
                let mut record_info = serde_json::json!({});
                if s.record_local {
                    // 解析录制目录（/tank 不可写降级 /tmp），并构造输出文件
                    let pref_owned = s.record_path.clone();
                    let id_for_dir = s.id.clone();
                    let pref_for_closure = pref_owned.clone();
                    let (dir, warn) = tokio::task::spawn_blocking(move || {
                        Self::resolve_record_dir(
                            pref_for_closure.as_deref(),
                            "sources",
                            &id_for_dir,
                        )
                    })
                    .await
                    .unwrap_or((pref_owned.unwrap_or_default(), None));
                    let outfile =
                        format!("{}/{}", dir.trim_end_matches('/'), Self::record_filename());
                    let cmd = build_record_cmd(&s.url, &s.protocol, &outfile);
                    match Self::spawn_ffmpeg(&cmd) {
                        Ok(pid) => {
                            record_info = serde_json::json!({
                                "record_local": true,
                                "record_pid": pid,
                                "record_file": outfile.clone(),
                                "record_dir": dir.clone(),
                            });
                            // 写回 source：更新 record_path（实际生效目录）+ record_pid
                            let mut sources = self.sources.lock().expect("sources poisoned");
                            if let Some(src) = sources.iter_mut().find(|x| x.id == s.id) {
                                src.record_path = Some(dir);
                                src.record_pid = Some(pid);
                            }
                        }
                        Err(e) => {
                            // spawn 失败：仍标记 recording=true（意图记录），但回传错误
                            record_info = serde_json::json!({
                                "record_local": true,
                                "record_error": e,
                                "record_dir": dir,
                            });
                        }
                    }
                    if let Some(w) = &warn {
                        if let serde_json::Value::Object(ref mut m) = record_info {
                            m.insert(
                                "record_warning".into(),
                                serde_json::Value::String(w.clone()),
                            );
                        }
                    }
                }
                // 标记 recording=true（无论是否本地录制，均记录"录制中"意图）
                let updated = {
                    let mut sources = self.sources.lock().expect("sources poisoned");
                    if let Some(src) = sources.iter_mut().find(|x| x.id == *id) {
                        src.recording = true;
                    }
                    sources.iter().find(|x| x.id == *id).cloned()
                };
                let updated = updated.unwrap_or(s);
                let mut body = serde_json::json!({
                    "ok": true,
                    "id": id,
                    "recording": true,
                    "mediamtx_config": build_mediamtx_path_config(&updated),
                });
                if let (serde_json::Value::Object(ref mut m), serde_json::Value::Object(ri)) =
                    (&mut body, record_info)
                {
                    for (k, v) in ri {
                        m.insert(k, v);
                    }
                }
                Ok(ok_json(body))
            }

            // —— POST /api/v1/streaming/sources/:id/record/stop —— 停止录制
            (HttpMethod::Post, ["api", "v1", "streaming", "sources", id, "record", "stop"]) => {
                let mut sources = self.sources.lock().expect("sources poisoned");
                match sources.iter_mut().find(|s| s.id == *id) {
                    Some(s) => {
                        s.recording = false;
                        // 若有本地录制子进程，kill 之（杀不掉也继续）
                        if let Some(pid) = s.record_pid.take() {
                            let _ = Self::kill_transcode(pid);
                        }
                        Ok(ok_json(serde_json::json!({
                            "ok": true,
                            "id": id,
                            "recording": false,
                        })))
                    }
                    None => Ok(error_response(404, &format!("拉流源不存在: {id}"))),
                }
            }

            // ===================== 节目输出 =====================
            // —— GET /api/v1/streaming/program —— 取主输出 + 预览源
            (HttpMethod::Get, ["api", "v1", "streaming", "program"]) => {
                Ok(ok_json(to_value(&self.program_snapshot())?))
            }

            // —— POST /api/v1/streaming/program/switch —— 切换主输出
            (HttpMethod::Post, ["api", "v1", "streaming", "program", "switch"]) => {
                let body: SwitchProgramBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析切换主输出请求体失败: {e}"))
                })?;
                let sources = self.sources.lock().expect("sources poisoned");
                if !sources.iter().any(|s| s.id == body.source_id) {
                    return Ok(error_response(
                        404,
                        &format!("拉流源不存在: {}", body.source_id),
                    ));
                }
                let mut program = self.program.lock().expect("program poisoned");
                program.active_source_id = Some(body.source_id.clone());
                Ok(ok_json(to_value(&*program)?))
            }

            // ===================== 转码任务 =====================
            // —— GET /api/v1/streaming/transcode —— 列全部
            //
            // 列出前先 refresh：对 running 任务探测 pid 是否仍在，已退出 → completed。
            (HttpMethod::Get, ["api", "v1", "streaming", "transcode"]) => {
                self.refresh_all_transcodes();
                Ok(ok_json(to_value(&self.transcodes_snapshot())?))
            }

            // —— GET /api/v1/streaming/transcode/sources —— 可用本地视频文件（转码输入源）
            //
            // 真盘优先扫描 /tank/media/video/（再回退 /var/lib/os/media/video/），返回真实
            // 视频文件列表，供前端创建转码任务时选择 input。每个条目含 path / name /
            // size_bytes。无真实文件时返回空数组（不 panic）。
            (HttpMethod::Get, ["api", "v1", "streaming", "transcode", "sources"]) => {
                let sources = scan_local_video_files().await;
                Ok(ok_json(to_value(&sources)?))
            }

            // —— GET /api/v1/streaming/transcode/:id —— 单任务详情（先刷新状态再返回）
            //
            // 先 refresh_one_transcode：若该任务 running 且 pid 已退出 → 标记 completed。
            // 不存在返回 404。
            (HttpMethod::Get, ["api", "v1", "streaming", "transcode", id]) => {
                self.refresh_one_transcode(id);
                let task = {
                    let transcodes = self.transcodes.lock().expect("transcodes poisoned");
                    transcodes.iter().find(|t| t.id == *id).cloned()
                };
                match task {
                    Some(t) => Ok(ok_json(to_value(&t)?)),
                    None => Ok(error_response(404, &format!("转码任务不存在: {id}"))),
                }
            }

            // —— POST /api/v1/streaming/transcode —— 创建转码任务
            (HttpMethod::Post, ["api", "v1", "streaming", "transcode"]) => {
                let body: CreateTranscodeBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析创建转码任务请求体失败: {e}"))
                })?;
                if body.name.trim().is_empty() {
                    return Ok(error_response(400, "name 不可为空"));
                }
                if body.input.trim().is_empty() {
                    return Ok(error_response(400, "input 不可为空"));
                }
                let mode = body
                    .mode
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "vod".to_string());
                if mode != "vod" && mode != "live" {
                    return Ok(error_response(400, "mode 必须是 vod 或 live"));
                }
                let codec = body
                    .codec
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "hevc_nvenc".to_string());
                let preferred_dir = body
                    .output_dir
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| format!("/tank/hls/{}", body.name));
                // 真实解析并创建输出目录：/tank/hls/<name> 创建失败降级 /tmp/os-hls/<name>
                let name_for_dir = body.name.clone();
                let preferred_for_closure = preferred_dir.clone();
                let (output_dir, dir_warning) = tokio::task::spawn_blocking(move || {
                    Self::resolve_output_dir(&preferred_for_closure, &name_for_dir)
                })
                .await
                .unwrap_or((preferred_dir, None));
                let ladder = body.ladder.unwrap_or_default();
                let mut task = TranscodeTask {
                    id: self.next_id("tc"),
                    name: body.name,
                    input: body.input,
                    output_dir,
                    mode: mode.clone(),
                    codec,
                    ladder,
                    status: "queued".into(),
                    progress: 0,
                    pid: None,
                    error: dir_warning,
                    created_at: now_iso(),
                };
                // 构造命令（无论是否 spawn，都可用于调试/回显）
                let cmd = if mode == "live" {
                    build_live_transcode_cmd(&task)
                } else {
                    build_vod_transcode_cmd(&task)
                };
                // 默认不 autostart（保持 queued，测试可确定性）；autostart=true 时真 spawn ffmpeg
                if body.autostart.unwrap_or(false) {
                    match Self::spawn_ffmpeg(&cmd) {
                        Ok(pid) => {
                            task.status = "running".into();
                            task.pid = Some(pid);
                        }
                        Err(e) => {
                            task.status = "failed".into();
                            // 保留目录降级 warning（若有），追加 spawn 失败原因
                            task.error = Some(match task.error.take() {
                                Some(w) => format!("{w}; {e}"),
                                None => e,
                            });
                        }
                    }
                }
                let resp_body = to_value(&task)?;
                self.transcodes
                    .lock()
                    .expect("transcodes poisoned")
                    .push(task);
                Ok(ApiResponse {
                    status: 201,
                    body: resp_body,
                    headers: serde_json::json!({}),
                })
            }

            // —— DELETE /api/v1/streaming/transcode/:id —— 取消/删除任务
            (HttpMethod::Delete, ["api", "v1", "streaming", "transcode", id]) => {
                let mut transcodes = self.transcodes.lock().expect("transcodes poisoned");
                let before = transcodes.len();
                // 先找 running 任务的 pid，若有则 kill
                if let Some(t) = transcodes
                    .iter()
                    .find(|t| t.id == *id && t.status == "running")
                {
                    if let Some(pid) = t.pid {
                        let _ = Self::kill_transcode(pid); // 杀不掉也继续删
                    }
                }
                transcodes.retain(|t| t.id != *id);
                if transcodes.len() == before {
                    return Ok(error_response(404, &format!("转码任务不存在: {id}")));
                }
                Ok(ok_json(
                    serde_json::json!({"ok": true, "id": id, "action": "cancel"}),
                ))
            }

            // ===================== 推流目标 =====================
            // —— GET /api/v1/streaming/outputs —— 列全部
            (HttpMethod::Get, ["api", "v1", "streaming", "outputs"]) => {
                Ok(ok_json(to_value(&self.outputs_snapshot())?))
            }

            // —— POST /api/v1/streaming/outputs —— 添加推流目标
            (HttpMethod::Post, ["api", "v1", "streaming", "outputs"]) => {
                let body: CreateOutputBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析添加推流目标请求体失败: {e}"))
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
                let record_local = body.record_local.unwrap_or(false);
                let out = StreamOutput {
                    id: self.next_id("out"),
                    name: body.name,
                    url: body.url,
                    protocol,
                    source_id: body.source_id,
                    enabled: true,
                    status: "idle".into(),
                    pid: None,
                    record_local,
                    record_path: body
                        .record_path
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                    record_pid: None,
                    created_at: now_iso(),
                };
                let resp_body = to_value(&out)?;
                self.outputs.lock().expect("outputs poisoned").push(out);
                Ok(ApiResponse {
                    status: 201,
                    body: resp_body,
                    headers: serde_json::json!({}),
                })
            }

            // —— DELETE /api/v1/streaming/outputs/:id —— 删推流目标
            (HttpMethod::Delete, ["api", "v1", "streaming", "outputs", id]) => {
                let mut outputs = self.outputs.lock().expect("outputs poisoned");
                let before = outputs.len();
                outputs.retain(|o| o.id != *id);
                if outputs.len() == before {
                    return Ok(error_response(404, &format!("推流目标不存在: {id}")));
                }
                Ok(ok_json(
                    serde_json::json!({"ok": true, "id": id, "action": "delete"}),
                ))
            }

            // —— POST /api/v1/streaming/outputs/:id/start —— 启动推流（拉流转推）
            (HttpMethod::Post, ["api", "v1", "streaming", "outputs", id, "start"]) => {
                // 先快照 output（含 source_id、url、protocol）——锁立即释放
                let out_snapshot = {
                    let outputs = self.outputs.lock().expect("outputs poisoned");
                    outputs.iter().find(|o| o.id == *id).cloned()
                };
                let out = match out_snapshot {
                    Some(o) => o,
                    None => return Ok(error_response(404, &format!("推流目标不存在: {id}"))),
                };
                // source_id 必须非空且对应源存在
                let sid = match &out.source_id {
                    Some(s) if !s.is_empty() => s.clone(),
                    _ => return Ok(error_response(400, "未绑定拉流源")),
                };
                let (source_url, source_protocol) = {
                    let sources = self.sources.lock().expect("sources poisoned");
                    match sources.iter().find(|s| s.id == sid) {
                        Some(s) => (s.url.clone(), s.protocol.clone()),
                        None => return Ok(error_response(400, "未绑定拉流源")),
                    }
                };
                // 构造 relay 命令并 spawn
                let cmd = build_relay_cmd(&source_url, &source_protocol, &out.url, &out.protocol);
                match Self::spawn_ffmpeg(&cmd) {
                    Ok(pid) => {
                        // 若 output.record_local：另起一个 ffmpeg 把源落盘到本地 mp4
                        let mut record_info = serde_json::json!({});
                        if out.record_local {
                            let pref_owned = out.record_path.clone();
                            let oid = out.id.clone();
                            let pref_for_closure = pref_owned.clone();
                            let (dir, warn) = tokio::task::spawn_blocking(move || {
                                Self::resolve_record_dir(
                                    pref_for_closure.as_deref(),
                                    "outputs",
                                    &oid,
                                )
                            })
                            .await
                            .unwrap_or((pref_owned.unwrap_or_default(), None));
                            let outfile = format!(
                                "{}/{}",
                                dir.trim_end_matches('/'),
                                Self::record_filename()
                            );
                            let rec_cmd = build_record_cmd(&source_url, &source_protocol, &outfile);
                            match Self::spawn_ffmpeg(&rec_cmd) {
                                Ok(rpid) => {
                                    record_info = serde_json::json!({
                                        "record_local": true,
                                        "record_pid": rpid,
                                        "record_file": outfile,
                                        "record_dir": dir.clone(),
                                    });
                                    let mut outputs =
                                        self.outputs.lock().expect("outputs poisoned");
                                    if let Some(o) = outputs.iter_mut().find(|o| o.id == out.id) {
                                        o.record_pid = Some(rpid);
                                        o.record_path = Some(dir);
                                    }
                                }
                                Err(e) => {
                                    record_info = serde_json::json!({
                                        "record_local": true,
                                        "record_error": e,
                                        "record_dir": dir,
                                    });
                                }
                            }
                            if let Some(w) = warn {
                                if let serde_json::Value::Object(ref mut m) = record_info {
                                    m.insert("record_warning".into(), serde_json::Value::String(w));
                                }
                            }
                        }
                        let mut outputs = self.outputs.lock().expect("outputs poisoned");
                        if let Some(o) = outputs.iter_mut().find(|o| o.id == out.id) {
                            o.pid = Some(pid);
                            o.status = "pushing".into();
                            o.enabled = true;
                        }
                        let updated = outputs
                            .iter()
                            .find(|o| o.id == out.id)
                            .cloned()
                            .unwrap_or(out);
                        // 序列化 output 为响应 JSON，并合并本次 record 信息（不持久化，仅回显）
                        let mut body = to_value(&updated)?;
                        if let (
                            serde_json::Value::Object(ref mut m),
                            serde_json::Value::Object(ri),
                        ) = (&mut body, record_info)
                        {
                            for (k, v) in ri {
                                // record_dir 已持久化到 record_path，跳过；其余 record_* 回显
                                if k != "record_dir" {
                                    m.insert(k, v);
                                }
                            }
                        }
                        Ok(ok_json(body))
                    }
                    Err(e) => {
                        // spawn 失败降级为 error（不 panic）
                        let mut outputs = self.outputs.lock().expect("outputs poisoned");
                        if let Some(o) = outputs.iter_mut().find(|o| o.id == out.id) {
                            o.status = "error".into();
                        }
                        let updated = outputs
                            .iter()
                            .find(|o| o.id == out.id)
                            .cloned()
                            .unwrap_or_else(|| out.clone());
                        let mut body = to_value(&updated)?;
                        if let serde_json::Value::Object(ref mut map) = body {
                            map.insert("error".into(), serde_json::Value::String(e));
                        }
                        Ok(ApiResponse {
                            status: 200,
                            body,
                            headers: serde_json::json!({}),
                        })
                    }
                }
            }

            // —— POST /api/v1/streaming/outputs/:id/stop —— 停止推流
            (HttpMethod::Post, ["api", "v1", "streaming", "outputs", id, "stop"]) => {
                let mut outputs = self.outputs.lock().expect("outputs poisoned");
                let out = match outputs.iter_mut().find(|o| o.id == *id) {
                    Some(o) => o,
                    None => return Ok(error_response(404, &format!("推流目标不存在: {id}"))),
                };
                if let Some(pid) = out.pid {
                    // 杀推流进程（杀不掉也继续）
                    let _ = std::process::Command::new("kill")
                        .arg(pid.to_string())
                        .spawn();
                }
                if let Some(rpid) = out.record_pid.take() {
                    // 杀本地录制进程（若有）
                    let _ = Self::kill_transcode(rpid);
                }
                out.status = "idle".into();
                out.pid = None;
                let updated = out.clone();
                Ok(ok_json(to_value(&updated)?))
            }

            // ===================== 统计 =====================
            // —— GET /api/v1/streaming/stats —— 聚合统计
            (HttpMethod::Get, ["api", "v1", "streaming", "stats"]) => {
                Ok(ok_json(to_value(&self.stats_snapshot())?))
            }

            // —— 未覆盖路由 —— 兜底 404
            _ => Ok(error_response(404, "streaming: 未匹配的路由")),
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
        handler_component: "streaming".to_string(),
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

/// 从 URL 推断协议（rtsp:// / rtmp:// / srt:// / http://）。
fn infer_protocol(url: &str) -> String {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("rtsp://") {
        "rtsp".into()
    } else if lower.starts_with("rtmp://") {
        "rtmp".into()
    } else if lower.starts_with("srt://") {
        "srt".into()
    } else if lower.starts_with("http://") || lower.starts_with("https://") {
        "http".into()
    } else {
        "rtsp".into()
    }
}

/// 扫描本地视频目录（`/tank/media/video/` → `/var/lib/os/media/video/`），返回真实
/// 视频文件清单（用于 `GET /api/v1/streaming/transcode/sources`）。
///
/// spawn_blocking 读目录，按扩展名过滤常见视频格式。任一根目录扫到 ≥1 个文件即返回
/// 该目录全部视频；都无文件返回空 Vec（前端据此显示空态）。本函数不依赖外部进程，
/// 目录不存在/不可读返回空 Vec，不 panic。
async fn scan_local_video_files() -> Vec<TranscodeSource> {
    tokio::task::spawn_blocking(|| {
        let roots = [
            Path::new("/tank/media/video"),
            Path::new("/var/lib/os/media/video"),
        ];
        for root in roots {
            if let Ok(entries) = std::fs::read_dir(root) {
                let mut items = Vec::new();
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_file() {
                        continue;
                    }
                    let fname = match path.file_name().and_then(|s| s.to_str()) {
                        Some(s) => s.to_string(),
                        None => continue,
                    };
                    if !is_video_ext(&fname) {
                        continue;
                    }
                    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    items.push(TranscodeSource {
                        path: path.to_string_lossy().into_owned(),
                        name: fname,
                        size_bytes: size,
                    });
                }
                if !items.is_empty() {
                    // 按文件名排序，便于前端稳定展示
                    items.sort_by(|a, b| a.name.cmp(&b.name));
                    return items;
                }
            }
        }
        Vec::new()
    })
    .await
    .unwrap_or_default()
}

/// 判断文件名扩展名是否属于常见视频格式。
fn is_video_ext(fname: &str) -> bool {
    let ext = fname
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "mp4"
            | "mkv"
            | "mov"
            | "webm"
            | "avi"
            | "m4v"
            | "ts"
            | "wmv"
            | "flv"
            | "mpg"
            | "mpeg"
            | "m2ts"
    )
}

// ----------------------------------------------------------------------------
// demo 数据（仅 #[cfg(test)] 使用，生产 new() 不调用）
// ----------------------------------------------------------------------------

#[cfg(test)]
/// demo 拉流源（1 rtsp 4k + 1 rtmp 全景）。
fn demo_sources() -> Vec<StreamSource> {
    vec![
        StreamSource {
            id: "src-1".into(),
            name: "客厅摄像头".into(),
            url: "rtsp://192.168.1.50:554/stream1".into(),
            protocol: "rtsp".into(),
            resolution_tag: "4k".into(),
            status: "live".into(),
            recording: false,
            record_local: false,
            record_path: None,
            record_pid: None,
            created_at: "2026-08-08T09:00:00+08:00".into(),
        },
        StreamSource {
            id: "src-2".into(),
            name: "全景直播".into(),
            url: "rtmp://192.168.1.60:1935/live/pano".into(),
            protocol: "rtmp".into(),
            resolution_tag: "panorama".into(),
            status: "live".into(),
            recording: true,
            record_local: false,
            record_path: None,
            record_pid: None,
            created_at: "2026-08-08T09:05:00+08:00".into(),
        },
    ]
}

#[cfg(test)]
/// demo 转码任务（vod hevc_nvenc completed）。
fn demo_transcodes() -> Vec<TranscodeTask> {
    vec![TranscodeTask {
        id: "tc-1".into(),
        name: "家庭影院-假期旅行".into(),
        input: "/tank/media/video/family-trip-2025.mp4".into(),
        output_dir: "/tank/hls/family-trip-2025".into(),
        mode: "vod".into(),
        codec: "hevc_nvenc".into(),
        ladder: vec![],
        status: "completed".into(),
        progress: 100,
        pid: None,
        error: None,
        created_at: "2026-08-08T08:00:00+08:00".into(),
    }]
}

#[cfg(test)]
/// demo 推流目标（rtmp idle）。
fn demo_outputs() -> Vec<StreamOutput> {
    vec![StreamOutput {
        id: "out-1".into(),
        name: "YouTube 直播".into(),
        url: "rtmp://a.rtmp.youtube.com/live2/xxxx-yyyy-zzzz".into(),
        protocol: "rtmp".into(),
        source_id: Some("src-1".into()),
        enabled: false,
        status: "idle".into(),
        pid: None,
        record_local: false,
        record_path: None,
        record_pid: None,
        created_at: "2026-08-08T09:10:00+08:00".into(),
    }]
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

    fn make_task(codec: &str, ladder: Vec<ResolutionRung>) -> TranscodeTask {
        TranscodeTask {
            id: "tc-x".into(),
            name: "test".into(),
            input: "/tank/media/video/x.mp4".into(),
            output_dir: "/tank/hls/test".into(),
            mode: "vod".into(),
            codec: codec.into(),
            ladder,
            status: "queued".into(),
            progress: 0,
            pid: None,
            error: None,
            created_at: "2026-08-08T09:00:00+08:00".into(),
        }
    }

    /// 构造一个**预置 demo 数据**的 handler（供依赖旧 demo 数据的断言使用）。
    ///
    /// 生产 `new()` 现在启动时空，但部分测试断言（如"切换主输出""stats 聚合"）需要
    /// 已有数据；这些测试改为通过本 helper 显式注入 demo 数据，不再依赖 `new()`。
    fn with_demo_data() -> StreamingRouteHandler {
        let sources = demo_sources();
        let first_id = sources.first().map(|s| s.id.clone());
        let program = ProgramOut {
            active_source_id: first_id,
            sources_preview: sources.iter().map(|s| s.id.clone()).collect(),
        };
        StreamingRouteHandler {
            sources: Mutex::new(sources),
            transcodes: Mutex::new(demo_transcodes()),
            outputs: Mutex::new(demo_outputs()),
            program: Mutex::new(program),
            counter: Mutex::new(100),
            app_registry: None,
        }
    }

    // ---- 命令构造器测试 ----

    #[test]
    fn build_vod_single_rate_contains_hevc_and_hls() {
        let task = make_task("hevc_nvenc", vec![]);
        let cmd = build_vod_transcode_cmd(&task);
        let joined = cmd.join(" ");
        assert!(joined.contains("hevc_nvenc"), "缺 hevc_nvenc: {joined}");
        assert!(joined.contains("-f hls"), "缺 -f hls: {joined}");
        assert!(joined.contains("index.m3u8"), "缺 index.m3u8: {joined}");
        assert!(joined.contains("-hls_playlist_type vod"), "缺 vod playlist");
        assert!(joined.contains("-hwaccel auto"), "缺 hwaccel auto");
    }

    #[test]
    fn build_vod_multi_rate_contains_master_playlist() {
        let ladder = vec![
            ResolutionRung {
                label: "1080p".into(),
                width: 1920,
                height: 1080,
                bitrate: "8M".into(),
            },
            ResolutionRung {
                label: "720p".into(),
                width: 1280,
                height: 720,
                bitrate: "4M".into(),
            },
        ];
        let task = make_task("h264_nvenc", ladder);
        let cmd = build_vod_transcode_cmd(&task);
        let joined = cmd.join(" ");
        assert!(
            joined.contains("master.m3u8"),
            "多码率缺 master.m3u8: {joined}"
        );
        assert!(joined.contains("split=2"), "缺 split=2: {joined}");
        assert!(joined.contains("scale=1920:1080"), "缺 1080p scale");
        assert!(joined.contains("scale=1280:720"), "缺 720p scale");
        assert!(joined.contains("8M") && joined.contains("4M"), "缺码率档");
    }

    #[test]
    fn build_live_transcode_contains_rtsp_output() {
        let mut task = make_task("hevc_nvenc", vec![]);
        task.mode = "live".into();
        task.input = "rtsp://192.168.1.50:554/stream1".into();
        let cmd = build_live_transcode_cmd(&task);
        let joined = cmd.join(" ");
        assert!(
            joined.contains("-rtsp_transport tcp"),
            "缺 rtsp_transport: {joined}"
        );
        assert!(
            joined.contains(&format!("{MEDIAMTX_RTSP_BASE}/test")),
            "缺 mediamtx rtsp 推流目标: {joined}"
        );
        assert!(joined.contains("-f rtsp"), "缺 -f rtsp 输出格式");
    }

    #[test]
    fn build_mediamtx_path_config_contains_source_url() {
        let src = StreamSource {
            id: "src-1".into(),
            name: "客厅".into(),
            url: "rtsp://192.168.1.50:554/stream1".into(),
            protocol: "rtsp".into(),
            resolution_tag: "4k".into(),
            status: "live".into(),
            recording: true,
            record_local: false,
            record_path: None,
            record_pid: None,
            created_at: "2026-08-08T09:00:00+08:00".into(),
        };
        let cfg = build_mediamtx_path_config(&src);
        assert_eq!(cfg["name"], "客厅");
        assert_eq!(cfg["source"], "rtsp://192.168.1.50:554/stream1");
        assert_eq!(cfg["sourceProtocol"], "tcp", "rtsp 应映射为 tcp");
        assert_eq!(cfg["record"], true);
    }

    // ---- 路由声明测试 ----

    #[tokio::test]
    async fn routes_declares_endpoints_all_streaming() {
        let h = StreamingRouteHandler::new();
        let routes = h.routes().await;
        assert_eq!(routes.len(), 18, "应有 18 条路由: {routes:?}");
        assert!(
            routes.iter().all(|r| r.handler_component == "streaming"),
            "全部归属 streaming 组件"
        );
        // 含转码输入源扫描端点（新增）
        assert!(
            routes
                .iter()
                .any(|r| r.method == HttpMethod::Get
                    && r.path == "/api/v1/streaming/transcode/sources"),
            "缺 transcode/sources 路由"
        );
        // 含转码任务详情端点（新增）
        assert!(
            routes.iter().any(|r| r.method == HttpMethod::Get
                && r.path == "/api/v1/streaming/transcode/:id"),
            "缺 transcode/:id 路由"
        );
        // 含两条推流路由
        assert!(
            routes
                .iter()
                .any(|r| r.method == HttpMethod::Post && r.path.ends_with("/outputs/:id/start")),
            "缺 start 路由"
        );
        assert!(
            routes
                .iter()
                .any(|r| r.method == HttpMethod::Post && r.path.ends_with("/outputs/:id/stop")),
            "缺 stop 路由"
        );
        // 写操作都要求 admin
        for r in &routes {
            if r.method == HttpMethod::Post || r.method == HttpMethod::Delete {
                assert!(r.requires_auth, "写操作需 auth: {r:?}");
                assert_eq!(r.required_roles, vec!["admin".to_string()]);
            }
        }
        // GET 全部公开
        for r in &routes {
            if r.method == HttpMethod::Get {
                assert!(!r.requires_auth);
            }
        }
    }

    // ---- sources CRUD ----

    #[tokio::test]
    async fn create_source_then_list_contains_new() {
        let h = StreamingRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/streaming/sources",
                serde_json::json!({
                    "name": "新摄像头",
                    "url": "rtsp://x/stream",
                    "protocol": "rtsp",
                    "resolution_tag": "4k"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "create body: {resp:?}");
        assert_eq!(resp.body["protocol"], "rtsp");
        let id = resp.body["id"].as_str().unwrap().to_string();
        // 列表含新源
        let resp = h
            .handle(get_req("/api/v1/streaming/sources"))
            .await
            .unwrap();
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], id);
        assert_eq!(arr[0]["status"], "idle");
        // 预览列表也加入
        let prog = h.program_snapshot();
        assert!(prog.sources_preview.contains(&id));
    }

    #[tokio::test]
    async fn create_source_infers_protocol_from_url() {
        let h = StreamingRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/streaming/sources",
                serde_json::json!({"name": "s", "url": "rtmp://x/live"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["protocol"], "rtmp", "应从 url 推断 rtmp");
    }

    // ---- 录制 ----

    #[tokio::test]
    async fn record_start_sets_recording_true() {
        let h = with_demo_data();
        // 先停 src-1 录制（默认 false）再开
        let resp = h
            .handle(post_req(
                "/api/v1/streaming/sources/src-1/record/start",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["recording"], true);
        assert!(
            resp.body["mediamtx_config"]["source"].is_string(),
            "返回 mediamtx 配置"
        );
        // 内存态确实改了
        let srcs = h.sources_snapshot();
        let s1 = srcs.iter().find(|s| s.id == "src-1").unwrap();
        assert!(s1.recording);
        // stop
        let resp = h
            .handle(post_req(
                "/api/v1/streaming/sources/src-1/record/stop",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.body["recording"], false);
    }

    #[tokio::test]
    async fn record_start_missing_returns_404() {
        let h = StreamingRouteHandler::new();
        let resp = h
            .handle(post_req(
                "/api/v1/streaming/sources/nope/record/start",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // ---- program switch ----

    #[tokio::test]
    async fn program_switch_updates_active_source() {
        let h = with_demo_data();
        let before = h.program_snapshot();
        assert_eq!(before.active_source_id.as_deref(), Some("src-1"));
        let resp = h
            .handle(post_req(
                "/api/v1/streaming/program/switch",
                serde_json::json!({"source_id": "src-2"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["active_source_id"], "src-2");
        // 持久化
        assert_eq!(
            h.program_snapshot().active_source_id.as_deref(),
            Some("src-2")
        );
    }

    #[tokio::test]
    async fn program_switch_missing_source_returns_404() {
        let h = StreamingRouteHandler::new();
        let resp = h
            .handle(post_req(
                "/api/v1/streaming/program/switch",
                serde_json::json!({"source_id": "nope"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // ---- transcode CRUD ----

    #[tokio::test]
    async fn create_transcode_defaults_to_queued() {
        let h = StreamingRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/streaming/transcode",
                serde_json::json!({
                    "name": "x",
                    "input": "/tank/media/video/x.mp4"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "create body: {resp:?}");
        // 不 autostart 默认 queued
        assert_eq!(resp.body["status"], "queued");
        assert_eq!(resp.body["mode"], "vod");
        assert_eq!(resp.body["codec"], "hevc_nvenc");
        // output_dir：优先 /tank/hls/x；/tank 不可写时降级 /tmp/os-hls/x（本机常态）
        let out_dir = resp.body["output_dir"].as_str().unwrap().to_string();
        assert!(
            out_dir == "/tank/hls/x" || out_dir == "/tmp/os-hls/x",
            "output_dir 应为 /tank/hls/x 或其降级 /tmp/os-hls/x: {out_dir}"
        );
        // 降级时应有 warning 记录在 error 字段
        if out_dir == "/tmp/os-hls/x" {
            assert!(
                resp.body["error"].as_str().unwrap_or("").contains("降级"),
                "降级应有 warning: {resp:?}"
            );
        }
    }

    #[tokio::test]
    async fn delete_transcode_reduces_list() {
        let h = with_demo_data();
        let before = h.transcodes_snapshot().len();
        assert!(before >= 1);
        let resp = h
            .handle(del_req("/api/v1/streaming/transcode/tc-1"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["ok"], true);
        assert_eq!(h.transcodes_snapshot().len(), before - 1);
    }

    #[tokio::test]
    async fn create_transcode_invalid_mode_returns_400() {
        let h = StreamingRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/streaming/transcode",
                serde_json::json!({"name": "x", "input": "/tank/x.mp4", "mode": "bogus"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    // ---- outputs CRUD ----

    #[tokio::test]
    async fn create_output_then_list_contains_new() {
        let h = StreamingRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/streaming/outputs",
                serde_json::json!({
                    "name": "B 站直播",
                    "url": "rtmp://live-push.bilivideo.com/live-bvc/?streamkey=xxx"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "create body: {resp:?}");
        assert_eq!(resp.body["protocol"], "rtmp", "从 url 推断 rtmp");
        assert_eq!(resp.body["status"], "idle");
        // 列表含新
        let resp = h
            .handle(get_req("/api/v1/streaming/outputs"))
            .await
            .unwrap();
        assert_eq!(resp.body.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn delete_output_removes() {
        let h = with_demo_data();
        let before = h.outputs_snapshot().len();
        let resp = h
            .handle(del_req("/api/v1/streaming/outputs/out-1"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(h.outputs_snapshot().len(), before - 1);
    }

    // ---- stats ----

    #[tokio::test]
    async fn stats_aggregates_counts() {
        let h = with_demo_data();
        let resp = h.handle(get_req("/api/v1/streaming/stats")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["sources_total"], 2);
        assert_eq!(resp.body["sources_live"], 2, "两个 demo 源都 live");
        assert_eq!(resp.body["sources_recording"], 1, "src-2 录制中");
        assert_eq!(resp.body["transcodes_total"], 1);
        assert_eq!(resp.body["transcodes_completed"], 1);
        assert_eq!(resp.body["transcodes_running"], 0);
        assert_eq!(resp.body["outputs_total"], 1);
        assert_eq!(resp.body["program_has_active"], true);
    }

    // ---- build_relay_cmd 拉流转推流命令构造 ----

    #[test]
    fn build_relay_cmd_rtsp_input_adds_rtsp_transport_tcp() {
        let cmd = build_relay_cmd(
            "rtsp://192.168.1.50:554/stream1",
            "rtsp",
            "rtmp://a.rtmp.youtube.com/live2/key",
            "rtmp",
        );
        let joined = cmd.join(" ");
        assert!(
            joined.contains("-rtsp_transport tcp"),
            "rtsp 输入应含 -rtsp_transport tcp: {joined}"
        );
        assert!(
            joined.contains("-i rtsp://192.168.1.50:554/stream1"),
            "缺 -i 输入"
        );
    }

    #[test]
    fn build_relay_cmd_rtmp_input_has_no_rtsp_transport() {
        let cmd = build_relay_cmd(
            "rtmp://192.168.1.60:1935/live/pano",
            "rtmp",
            "rtmp://live.example.com/live/key",
            "rtmp",
        );
        let joined = cmd.join(" ");
        assert!(
            !joined.contains("-rtsp_transport"),
            "rtmp 输入不应含 rtsp_transport: {joined}"
        );
        assert!(joined.contains("-i rtmp://192.168.1.60:1935/live/pano"));
    }

    #[test]
    fn build_relay_cmd_rtsp_output_uses_f_rtsp() {
        let cmd = build_relay_cmd(
            "rtsp://192.168.1.50:554/stream1",
            "rtsp",
            "rtsp://localhost:8554/relay",
            "rtsp",
        );
        let joined = cmd.join(" ");
        assert!(
            joined.contains("-f rtsp"),
            "rtsp 输出应含 -f rtsp: {joined}"
        );
        assert!(joined.contains("-c copy"), "缺 -c copy");
        assert!(joined.contains("rtsp://localhost:8554/relay"), "缺输出 url");
    }

    #[test]
    fn build_relay_cmd_srt_output_uses_f_mpegts_and_copy() {
        let cmd = build_relay_cmd(
            "/tank/media/video/clip.mp4",
            "file",
            "srt://192.168.1.70:8888?streamid=test",
            "srt",
        );
        let joined = cmd.join(" ");
        assert!(
            joined.contains("-f mpegts"),
            "srt 输出应含 -f mpegts: {joined}"
        );
        assert!(joined.contains("-c copy"), "缺 -c copy（纯转封装）");
        assert!(joined.contains("-re "), "缺 -re（按帧率读）");
        assert!(joined.contains("-fflags +genpts"), "缺 genpts");
    }

    #[test]
    fn build_relay_cmd_rtmp_output_uses_f_flv() {
        let cmd = build_relay_cmd(
            "rtsp://192.168.1.50:554/stream1",
            "rtsp",
            "rtmp://a.rtmp.youtube.com/live2/key",
            "rtmp",
        );
        let joined = cmd.join(" ");
        assert!(joined.contains("-f flv"), "rtmp 输出应含 -f flv: {joined}");
    }

    // ---- build_record_cmd 拉流保存本地命令构造 ----

    #[test]
    fn build_record_cmd_rtsp_input_adds_tcp_and_mp4_output() {
        let cmd = build_record_cmd(
            "rtsp://192.168.1.50:554/stream1",
            "rtsp",
            "/tank/recordings/sources/src-1/rec.mp4",
        );
        let joined = cmd.join(" ");
        assert!(
            joined.contains("-rtsp_transport tcp"),
            "rtsp 输入应含 -rtsp_transport tcp: {joined}"
        );
        assert!(
            joined.contains("-c copy"),
            "录制应纯转封装 -c copy: {joined}"
        );
        assert!(joined.contains("-f mp4"), "录制输出应为 mp4: {joined}");
        assert!(
            joined.contains("-movflags +faststart"),
            "应加 faststart 便于流式播放: {joined}"
        );
        assert!(
            joined.contains("/tank/recordings/sources/src-1/rec.mp4"),
            "缺输出文件: {joined}"
        );
    }

    #[test]
    fn build_record_cmd_non_rtsp_input_has_no_rtsp_transport() {
        let cmd = build_record_cmd("rtmp://192.168.1.60:1935/live/pano", "rtmp", "/tmp/rec.mp4");
        let joined = cmd.join(" ");
        assert!(
            !joined.contains("-rtsp_transport"),
            "非 rtsp 输入不应含 rtsp_transport: {joined}"
        );
        assert!(joined.contains("-i rtmp://192.168.1.60:1935/live/pano"));
    }

    #[test]
    fn resolve_record_dir_falls_back_to_tmp_when_tank_unwritable() {
        let (dir, warn) = StreamingRouteHandler::resolve_record_dir(
            Some("/tank/recordings/__os_test__/sub"),
            "sources",
            "__os_test_src__",
        );
        // 降级到 /tmp 或成功在 /tank
        assert!(
            dir.contains("/recordings/"),
            "录制目录应含 recordings: {dir}"
        );
        if dir.starts_with("/tmp/recordings/") {
            assert!(warn.is_some(), "降级应有 warning");
            assert!(std::path::Path::new(&dir).exists(), "目录应已创建: {dir}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    // ---- 推流 start/stop 路由 ----

    #[tokio::test]
    async fn start_output_without_source_id_returns_400() {
        // with_empty 创建一个无 source_id 的 output
        let h = StreamingRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/streaming/outputs",
                serde_json::json!({
                    "name": "无源推流",
                    "url": "rtmp://x/live",
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        let id = resp.body["id"].as_str().unwrap().to_string();
        // start 应返回 400（未绑定拉流源）
        let resp = h
            .handle(post_req(
                &format!("/api/v1/streaming/outputs/{id}/start"),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "无 source_id 应 400: {resp:?}");
        assert_eq!(resp.body["error"], "未绑定拉流源");
    }

    #[tokio::test]
    async fn start_output_missing_output_returns_404() {
        let h = StreamingRouteHandler::new();
        let resp = h
            .handle(post_req(
                "/api/v1/streaming/outputs/nope/start",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn stop_output_missing_returns_404() {
        let h = StreamingRouteHandler::new();
        let resp = h
            .handle(post_req(
                "/api/v1/streaming/outputs/nope/stop",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn stop_output_resets_to_idle() {
        // out-1 默认 idle（无 pid）→ stop 应直接置 idle 且 pid 清空
        let h = with_demo_data();
        let resp = h
            .handle(post_req(
                "/api/v1/streaming/outputs/out-1/stop",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "stop body: {resp:?}");
        assert_eq!(resp.body["status"], "idle");
        assert!(resp.body["pid"].is_null(), "pid 应清空");
    }

    // ---- 默认 trait ----

    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<StreamingRouteHandler>();
    }

    #[tokio::test]
    async fn delete_source_resets_program_active() {
        let h = with_demo_data();
        // 当前 active 是 src-1，删之 → active 回落到第一个剩余源（src-2）
        let resp = h
            .handle(del_req("/api/v1/streaming/sources/src-1"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let prog = h.program_snapshot();
        assert_eq!(prog.active_source_id.as_deref(), Some("src-2"));
        assert!(!prog.sources_preview.contains(&"src-1".to_string()));
    }

    #[tokio::test]
    async fn unmatched_route_returns_404() {
        let h = StreamingRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/streaming/nope")).await.unwrap();
        assert_eq!(resp.status, 404);
    }

    // ---- 真实数据行为（去 demo 预置）----

    #[tokio::test]
    async fn new_starts_empty_no_demo_data() {
        // 生产 new() 启动时空：sources/transcodes/outputs 全空，program 无 active
        let h = StreamingRouteHandler::new();
        assert_eq!(h.sources_snapshot().len(), 0, "sources 应空");
        assert_eq!(h.transcodes_snapshot().len(), 0, "transcodes 应空");
        assert_eq!(h.outputs_snapshot().len(), 0, "outputs 应空");
        let prog = h.program_snapshot();
        assert!(prog.active_source_id.is_none(), "program 无 active");
        assert!(prog.sources_preview.is_empty(), "preview 应空");
        // list 端点也都返回空数组
        let resp = h
            .handle(get_req("/api/v1/streaming/sources"))
            .await
            .unwrap();
        assert_eq!(resp.body.as_array().unwrap().len(), 0);
        let resp = h
            .handle(get_req("/api/v1/streaming/transcode"))
            .await
            .unwrap();
        assert_eq!(resp.body.as_array().unwrap().len(), 0);
        let resp = h
            .handle(get_req("/api/v1/streaming/outputs"))
            .await
            .unwrap();
        assert_eq!(resp.body.as_array().unwrap().len(), 0);
        // stats 全 0
        let resp = h.handle(get_req("/api/v1/streaming/stats")).await.unwrap();
        assert_eq!(resp.body["sources_total"], 0);
        assert_eq!(resp.body["transcodes_total"], 0);
        assert_eq!(resp.body["outputs_total"], 0);
        assert_eq!(resp.body["program_has_active"], false);
    }

    #[tokio::test]
    async fn create_source_starts_idle_not_live() {
        // 添加拉流源：status 应为 idle（不假装在线）
        let h = StreamingRouteHandler::new();
        let resp = h
            .handle(post_req(
                "/api/v1/streaming/sources",
                serde_json::json!({"name": "cam", "url": "rtsp://x/s"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["status"], "idle", "新源 status 应为 idle");
    }

    #[tokio::test]
    async fn transcode_sources_endpoint_returns_array() {
        // GET /transcode/sources 返回数组（真盘有文件则非空，无则空数组，不 panic）
        let h = StreamingRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/streaming/transcode/sources"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body.is_array(), "应为数组: {resp:?}");
        // 若本机 /tank/media/video 有真实 mp4，则每条含 path/name/size_bytes
        for item in resp.body.as_array().unwrap() {
            assert!(item["path"].is_string(), "条目应有 path: {item:?}");
            assert!(item["name"].is_string(), "条目应有 name: {item:?}");
            assert!(item["size_bytes"].is_number(), "条目应有 size_bytes");
        }
    }

    #[test]
    fn resolve_output_dir_falls_back_to_tmp_when_tank_unwritable() {
        // /tank 通常无写权限 → 降级 /tmp/os-hls/<name>，返回 warning
        let (dir, warn) = StreamingRouteHandler::resolve_output_dir(
            "/tank/hls/__os_test_noexist__/sub",
            "__os_test_task__",
        );
        // 降级路径在 /tmp 下
        assert!(
            dir.starts_with("/tmp/os-hls/") || dir.contains("/tank/"),
            "降级目录应在 /tmp 或 /tank: {dir}"
        );
        // 若降级到 /tmp，目录应被创建且 warning 非空
        if dir.starts_with("/tmp/os-hls/") {
            assert!(std::path::Path::new(&dir).exists(), "目录应已创建: {dir}");
            assert!(warn.is_some(), "降级应返回 warning");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn is_video_ext_recognizes_common_formats() {
        assert!(is_video_ext("clip.mp4"));
        assert!(is_video_ext("movie.MKV")); // 大小写不敏感
        assert!(is_video_ext("a.webm"));
        assert!(is_video_ext("b.mov"));
        assert!(!is_video_ext("song.mp3"));
        assert!(!is_video_ext("pic.jpg"));
        assert!(!is_video_ext("readme"));
    }

    #[tokio::test]
    async fn create_source_with_record_local_persists_fields() {
        // 创建带"同时保存本地"的拉流源：record_local + record_path 应持久化
        let h = StreamingRouteHandler::new();
        let resp = h
            .handle(post_req(
                "/api/v1/streaming/sources",
                serde_json::json!({
                    "name": "带录制源",
                    "url": "rtsp://x/s",
                    "record_local": true,
                    "record_path": "/tank/recordings/sources/my-cam/"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "create body: {resp:?}");
        assert_eq!(resp.body["record_local"], true);
        assert_eq!(resp.body["record_path"], "/tank/recordings/sources/my-cam/");
        // record_pid 在创建时不应已存在（待 record/start 才 spawn）
        assert!(resp.body["record_pid"].is_null(), "创建时不应有 record_pid");
    }

    #[tokio::test]
    async fn create_output_with_record_local_persists_fields() {
        let h = StreamingRouteHandler::new();
        let resp = h
            .handle(post_req(
                "/api/v1/streaming/outputs",
                serde_json::json!({
                    "name": "转推+录",
                    "url": "rtmp://x/live",
                    "record_local": true
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "create body: {resp:?}");
        assert_eq!(resp.body["record_local"], true);
        // record_path 未指定 → 为 null（待 start 时按默认生成）
        assert!(
            resp.body["record_path"].is_null(),
            "未指定 record_path 应为 null"
        );
    }

    // ------------------------------------------------------------------
    // 引擎门控（2026-09-05：流媒体中心剥离为独立应用——装了才启用）
    // ------------------------------------------------------------------

    /// 建一个声明 engine=streaming 的应用裸仓库 fixture（真实 git），返回
    /// (AppRegistry, repo 名)——安装经 registry.install 真实 clone。id 与
    /// engine 刻意不同（streaming-hub / streaming），验证门控键走 engine 列。
    async fn streaming_app_registry(
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
        let bare = repos.join("nexos-app-streaming.git");
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
        let work = dir.join(".streaming-work");
        std::fs::create_dir_all(work.join("web")).unwrap();
        std::fs::write(
            work.join("manifest.json"),
            serde_json::json!({
                "id": "streaming-hub",
                "name": "NexOS 流媒体中心",
                "version": "0.1.0",
                "category": "media",
                "icon": "📡",
                "description": "拉流/转码/推流/多机位切换流媒体编排",
                "entry": "web/entry.js",
                "engine": "streaming",
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
        (reg, "nexos-app-streaming".to_string())
    }

    /// 每测独立临时目录（进程 id + 测名唯一，防并行互踩；apps fixture 用）。
    fn temp_dir_for(test: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nexos-streaming-{test}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn gating_blocks_all_streaming_endpoints_until_app_installed() {
        let (reg, repo) = streaming_app_registry("gate").await;
        let h = StreamingRouteHandler::new().with_app_registry(Arc::clone(&reg));
        // 未安装 → 全部业务端点 404 + 精确安装指引文案（读 + 写都拦；五大功能
        // 域各抽代表端点。/api/v1/live/* 属独立联邦直播组件不在本 handler，
        // 无 live 例外——transcode 的 mode=live 只是任务模式字符串）
        for (method, path, body) in [
            (
                HttpMethod::Get,
                "/api/v1/streaming/sources",
                serde_json::Value::Null,
            ),
            (
                HttpMethod::Get,
                "/api/v1/streaming/program",
                serde_json::Value::Null,
            ),
            (
                HttpMethod::Get,
                "/api/v1/streaming/transcode",
                serde_json::Value::Null,
            ),
            (
                HttpMethod::Get,
                "/api/v1/streaming/transcode/sources",
                serde_json::Value::Null,
            ),
            (
                HttpMethod::Get,
                "/api/v1/streaming/outputs",
                serde_json::Value::Null,
            ),
            (
                HttpMethod::Get,
                "/api/v1/streaming/stats",
                serde_json::Value::Null,
            ),
            (
                HttpMethod::Post,
                "/api/v1/streaming/sources",
                serde_json::json!({"name": "cam", "url": "rtsp://x/y"}),
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
                "应用「流媒体中心」未安装：可在 应用中心 → 商店 安装",
                "{path} 文案: {resp:?}"
            );
        }
        // 被拦期间未落任何状态（sources 表仍空）
        assert!(h.sources_snapshot().is_empty(), "被拦期间不应建拉流源");
        // fake 安装（真实 git clone）→ 门开：读 200 + 写放行（POST 201）
        let (action, rec) = reg.install(&repo).await.expect("安装应成功");
        assert_eq!(action, "install");
        assert_eq!(rec.id, "streaming-hub");
        assert_eq!(rec.engine, "streaming");
        let resp = h
            .handle(get_req("/api/v1/streaming/stats"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "装后应放行: {resp:?}");
        let resp = h
            .handle(post_req(
                "/api/v1/streaming/sources",
                serde_json::json!({"name": "cam", "url": "rtsp://x/y"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "装后写端点放行: {resp:?}");
        // 卸载 → 即时回 404
        reg.uninstall("streaming-hub").expect("卸载应成功");
        let resp = h
            .handle(get_req("/api/v1/streaming/stats"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404, "卸载即时生效: {resp:?}");
    }

    #[tokio::test]
    async fn streaming_gating_inactive_without_registry_injection() {
        // 未注入注册表（既有单测直构形态）→ 不门控（兼容基线测试契约）
        let h = StreamingRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/streaming/stats"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "未注入不门控: {resp:?}");
    }
}
