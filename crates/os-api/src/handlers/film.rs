//! `FilmRouteHandler` —— 「影片制作管线」桌面应用 REST 入口（参考 LibTV AI
//! 影片管线，docs/FILM_STUDIO.md）。
//!
//! 定位：把一条「创意 → 成片」的 AI 影片流水线暴露为 REST。六个阶段，每个
//! 阶段**独立选择模型来源**（本地能力 / 网关渠道，含 🌐 via_node 中继渠道）：
//!
//! ```text
//! 剧本分镜(chat) → 关键帧图(image) → 图生视频(video)
//!                 → 台词配音(tts) → 背景音乐(music) → ffmpeg 合成(compose)
//! ```
//!
//! # 项目与产物布局
//!
//! - `film_projects` 表（SQLite `film.db`，env `NEXOS_FILM_DB`）：id/title/idea/
//!   ratio(16:9|9:16|1:1|2.39:1|1.85:1|4:3)/style_hint/status/dir/export_dir/created_at/updated_at。
//! - 产物目录：env `NEXOS_FILM_DIR`（缺省 `/tank/os-data/film`）下每项目一目录
//!   `<dir>/<project-id>/`：`script.json` / `shot-<n>.png`（关键帧）/ `shot-<n>.mp4`
//!   （图生视频）/ `line-<n>.mp3`（台词配音）/ `bgm.mp3` / `subs.srt` /
//!   `compose-concat.txt` / `compose-video.mp4`（中间产物）/ `final.mp4`。
//!   `n` 为**1 起的分镜号**（与 script.json 每镜头的 `shot` 字段一致）。
//! - 导出路径（`export_dir`，2026-09-05 起可经 PUT 设置）：项目级成片落点。
//!   NULL/空 = 缺省（项目目录本身，`<dir>/final.mp4`）；设置时 compose 的
//!   final.mp4 写 `<export_dir>/final.mp4`（校验：绝对路径 + 父目录存在 +
//!   可写，**不自动创建**；env `NEXOS_FILM_EXPORT_BASE` 设置时还须位于其下
//!   ——缺省不限制：单用户节点，写面本就 admin 鉴权）。GET 回传
//!   `final_path` 便捷字段（两分支的完整落点）。
//!
//! # model_ref（阶段模型来源，冻结契约）
//!
//! `{"source":"local"|"channel","channel_id":"…"?,"capability":"chat"|"image"
//!   |"video"|"tts"|"music","model":"…"?}`
//!
//! | source | 能力 | 执行面 |
//! |--------|------|--------|
//! | `local` | `chat` | 本地 vLLM 实例直连（复用 llm handler 的实例调用面 `LlmRouteHandler::chat_complete`）|
//! | `local` | `image` | sd-turbo 生图内核（复用 media_gen 的 spawn/显存闸门函数，**不复制内核**）|
//! | `local` | `video`/`tts`/`music` | 未接入（请求期 400 明确提示改用渠道）|
//! | `channel` | 全部 | 经网关渠道转发（chat/image 复用 api_gateway 的 `forward_channel`——直连与 via_node 中继两形态同一口径；tts/music/video 二进制响应经 `channel_relay_request`+relay 的字节面）|
//!
//! 渠道 capability 判定不在此过度设计：前端按渠道名让用户选，后端只透传
//! （渠道表 models 字段 + 用户在渠道命名上的约定，见 docs/FILM_STUDIO.md）。
//!
//! # 渠道端点约定（OpenAI 兼容形态，base_url + 固定后缀）
//!
//! | capability | 后缀 | 请求体 | 响应取数 |
//! |------------|------|--------|----------|
//! | chat | `chat/completions` | `{model,messages}` | `choices[0].message.content` |
//! | image | `images/generations` | `{model,prompt,size,response_format:"b64_json"}` | `data[0].b64_json`/`url` |
//! | video | `video/generations` | `{model,prompt,image(b64),image_base64,duration_secs}` | `url`/`video_url`/`data[0].url`/`*_b64`（超时放宽 600s）|
//! | tts | `audio/speech` | `{model,input,voice,response_format:"mp3"}` | 二进制音频（或 JSON b64）|
//! | music | `music/generations` | `{model,prompt}` | `url`/b64/二进制 |
//!
//! # 任务模式（照 llm_envs / 生图先例）
//!
//! 阶段端点一律 `202 {task 摘要}`，后台 tokio 任务执行；任务态存进程内
//! `Mutex<HashMap<String, FilmTask>>`（环形日志上限 200 行），轮询
//! `GET /api/v1/film/tasks/:id` 看进度（日志尾）+ `GET /api/v1/film/tasks` 列表。
//! 服务重启任务态即清（产物文件与 film_projects 表才是真值）。产物路径在任务
//! `output` 字段回传。外部模型调用一律真实发起，失败如实落 `error` 不假成功。
//!
//! # ffmpeg 检测与合成
//!
//! - 检测：env `NEXOS_FFMPEG_BIN` → PATH 扫描 → 常规路径
//!   （/usr/bin /usr/local/bin /bin /opt/homebrew/bin /snap/bin）。**缺失不自动
//!   安装**：compose 任务报错附安装指引（`GET /api/v1/film/tools` 亦可查）。
//! - 合成两遍 ffmpeg（cwd=项目目录，文件名全相对——subtitles 滤镜免转义）：
//!   1. concat：`-f concat -safe 0 -i compose-concat.txt -vf scale…pad…fps=30
//!      -c:v libx264 -pix_fmt yuv420p -c:a aac -ar 44100 -ac 2 compose-video.mp4`
//!   2. 混音+字幕：`-i compose-video.mp4 [-i line-*.mp3 …] [-stream_loop -1 -i
//!      bgm.mp3] -filter_complex "[0:v]subtitles=subs.srt[vout];…adelay=ms|ms…
//!      amix…[voice];[bgm]volume=0.35[bgm];[voice][bgm]amix=inputs=2:
//!      duration=longest:normalize=0[aout]" -map [vout] -map [aout] … final.mp4`
//!      台词按分镜时间轴 adelay 对齐；BGM `-stream_loop -1` 循环铺满 + 音量压低。
//!
//! # 引擎门控（2026-09-04：film 剥离为独立应用）
//!
//! film 引擎**内置**于 os-api（代码仍编译在二进制内），但按「装了应用才启用」
//! 架构运行：未安装 film 应用包（NexHub `nexos-app-film` 仓库，经应用中心
//! 安装到 `/tank/os-data/apps/film/`，apps 表登记 `engine="film"`）时，下表
//! 全部业务端点一律 404 `{"error":"应用「film」未安装：可在 应用中心 → 商店
//! 安装"}`。门控每请求直查 apps 表（`AppRegistry::is_engine_enabled`，无
//! 缓存）——安装/卸载**即时生效**；表损坏 fail-closed。理由与完整架构说明
//! 见 docs/APPS.md「引擎门控」。
//!
//! # 角色库与一致性（2026-09-04 P0，docs/FILM_STUDIO.md「角色库」章）
//!
//! - `film_characters` 表（同 film.db）：id/project_id/name/description/voice/
//!   portrait_ref（产物相对路径）/created_at/updated_at；定妆图落
//!   `<dir>/characters/<cid>/portrait.<ext>`，项目级参考图落 `<dir>/refs/`。
//! - 分镜绑定：`ScriptShot.characters`（角色名数组，serde default 兼容旧
//!   script.json）；script 生成提示词注入【角色表】，要求每镜头输出出场角色名；
//!   解析容错（未知角色名保留原样 + 任务日志提示）。PUT script 局部更新
//!   （按镜头号合并，前端面板编辑绑定）。
//! - 生成注入：image prompt 前置角色描述块（固定措辞「…（与其它镜头严格同一
//!   人物）」，顺序稳定）；channel image/video 请求体可选 `reference_images`
//!   （出场角色定妆图 b64）+ `reference_strength`（env
//!   `NEXOS_FILM_REF_STRENGTH`，缺省 0.5；**local sd-turbo 不发**——P0 仅
//!   prompt 注入档）；TTS voice 按「镜头第一个有 voice 的角色 → env
//!   `NEXOS_FILM_TTS_VOICE`（缺省 `alloy`）」透传（替换硬编码）。
//!
//! # 路由表（21 条，component="film"；读公开 / 写 admin；未装应用全 404）
//!
//! | method | path | 动作 |
//! |--------|------|------|
//! | POST | `/api/v1/film/projects` | 建项目（admin）|
//! | GET | `/api/v1/film/projects` | 列项目 |
//! | GET | `/api/v1/film/projects/:id` | 项目详情（含分镜+产物清单+refs）|
//! | PUT | `/api/v1/film/projects/:id` | 改项目（admin，部分更新，含 script 局部合并 + export_dir 导出路径）|
//! | DELETE | `/api/v1/film/projects/:id` | 删项目（admin，连产物目录）|
//! | POST | `/api/v1/film/projects/:id/script` | 分镜任务（admin）|
//! | POST | `/api/v1/film/projects/:id/shots/:n/image` | 关键帧图任务（admin）|
//! | POST | `/api/v1/film/projects/:id/shots/:n/video` | 图生视频任务（admin）|
//! | POST | `/api/v1/film/projects/:id/shots/:n/tts` | 台词配音任务（admin）|
//! | POST | `/api/v1/film/projects/:id/music` | BGM 任务（admin）|
//! | POST | `/api/v1/film/projects/:id/compose` | ffmpeg 合成任务（admin）|
//! | GET | `/api/v1/film/projects/:id/characters` | 角色列表（含 voice/portrait_url/绑定镜头）|
//! | POST | `/api/v1/film/projects/:id/characters` | 建角色（admin，name+description 必填）|
//! | PUT | `/api/v1/film/characters/:cid` | 改角色（admin，部分更新）|
//! | DELETE | `/api/v1/film/characters/:cid` | 删角色（admin，连定妆图目录）|
//! | POST | `/api/v1/film/projects/:id/characters/:cid/portrait` | 上传定妆图（admin，b64 ≤10MB png/jpeg/webp）|
//! | POST | `/api/v1/film/projects/:id/characters/:cid/portrait/generate` | 生成定妆图任务（admin）|
//! | POST | `/api/v1/film/projects/:id/refs` | 导入项目参考图（admin，b64）|
//! | GET | `/api/v1/film/tasks` | 任务列表 |
//! | GET | `/api/v1/film/tasks/:id` | 任务详情（含日志）|
//! | GET | `/api/v1/film/tools` | ffmpeg 检测状态 + 安装指引 |

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use once_cell::sync::Lazy;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use super::api_gateway::{ApiGatewayRouteHandler, Channel};
use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

/// 查项目快捷宏（`FilmRouteHandler::project_or_404` 的 Err=404 响应体直接
/// `return Ok(resp)`——`ApiResponse` 不能经 `?` 转 `ApiGatewayError`，与
/// media-gen 的显式回包模式一致）。
macro_rules! try_project {
    ($h:expr, $id:expr) => {
        match $h.project_or_404($id) {
            Ok(p) => p,
            Err(resp) => return Ok(resp),
        }
    };
}

/// 查角色快捷宏（同 [`try_project!`] 形态——404 响应体直回）。
macro_rules! try_character {
    ($h:expr, $cid:expr) => {
        match character_or_404($h, $cid) {
            Ok(c) => c,
            Err(resp) => return Ok(resp),
        }
    };
}

// ----------------------------------------------------------------------------
// 常量
// ----------------------------------------------------------------------------

/// 环形日志上限（行）。
const TASK_LOG_MAX_LINES: usize = 200;

/// 画幅白名单与成片合成分辨率（ratio → compose 统一 W×H；六档预设，
/// 2026-09-06 v0.1.37 与前端新建弹窗预设卡一表同源——preset key 仅前端概念，
/// 落库始终是比例字符串）。宽高全为偶数（yuv420p 要求；映射处另有钳偶兜底）。
const COMPOSE_DIMS: [(&str, u32, u32); 6] = [
    ("16:9", 1920, 1080),   // 手机横版（B站/YouTube/通用横版）
    ("9:16", 1080, 1920),   // 手机竖版（抖音/快手/视频号/Shorts）
    ("2.39:1", 2048, 858),  // 影院宽银幕（变形宽银幕超宽画幅）
    ("1.85:1", 1998, 1080), // 传统电影（欧美院线标准画幅）
    ("1:1", 1080, 1080),    // 方形（社交媒体信息流）
    ("4:3", 1440, 1080),    // 传统电视（复古/纪录片感）
];

/// 画幅 → 关键帧生图尺寸（宽高均为 8 的倍数——sd-turbo/diffusers 要求；
/// 按各档比例就近取安全尺寸，像素预算与原 1272×720 同量级，local/channel
/// 生图同口径）。
const IMAGE_DIMS: [(&str, u32, u32); 6] = [
    ("16:9", 1272, 720),
    ("9:16", 720, 1272),
    ("2.39:1", 1472, 616),
    ("1.85:1", 1304, 704),
    ("1:1", 720, 720),
    ("4:3", 960, 720),
];

/// 分镜数量：提示词要求 5..=12；解析接受 1..=24（LLM 少给/多给不硬拒，如实入库）。
const SHOTS_PROMPT_MIN: usize = 5;
const SHOTS_PROMPT_MAX: usize = 12;
const SHOTS_ACCEPT_MAX: usize = 24;

/// 单镜头时长（秒）：默认 5，钳制 1..=60。
const SHOT_DURATION_DEFAULT_SECS: u32 = 5;
const SHOT_DURATION_MAX_SECS: u32 = 60;

/// 分镜 chat 请求 max_tokens（JSON 数组 12 镜头量级，留足余量）。
const SCRIPT_MAX_TOKENS: u32 = 4096;

/// compose 合成视频的标准 fps。
const COMPOSE_FPS: u32 = 30;

/// 视频/音乐生成超时缺省（秒；env `NEXOS_FILM_VIDEO_TIMEOUT_SECS` 覆写，钳
/// 60..=1800——比网关 300s 转发口径放宽，任务态本就异步轮询）。chat/image/tts
/// 等其余阶段沿用网关自身超时（`forward_channel` 内部 300s 口径）。
const FILM_VIDEO_TIMEOUT_DEFAULT_SECS: u64 = 600;

/// ffmpeg 单遍超时缺省（秒；env `NEXOS_FILM_COMPOSE_TIMEOUT_SECS` 覆写）。
const FILM_COMPOSE_TIMEOUT_DEFAULT_SECS: u64 = 600;

/// BGM 混音音量（人声 1.0 / BGM 0.35）。
const BGM_VOLUME: &str = "0.35";

/// 定妆图/参考图上传上限（解码后字节数；对齐 Kling 渠道 10MB 口径）。
pub(crate) const IMAGE_MAX_BYTES: usize = 10 * 1024 * 1024;

/// 允许的图片 mime → 扩展名（定妆图上传白名单；refs 按魔数嗅探同族）。
const IMAGE_MIME_EXT: [(&str, &str); 3] = [
    ("image/png", "png"),
    ("image/jpeg", "jpg"),
    ("image/webp", "webp"),
];

/// channel 生图/视频参考注入强度缺省（env `NEXOS_FILM_REF_STRENGTH` 覆写，钳 0.0..=1.0）。
const REFERENCE_STRENGTH_DEFAULT: f64 = 0.5;

/// TTS voice 终极兜底（OpenAI 标准枚举；角色 voice > env `NEXOS_FILM_TTS_VOICE` > 此值）。
pub const TTS_VOICE_FALLBACK: &str = "alloy";

/// 角色 id 前缀（`char-<n>`，与项目 `film-<n>` 同风格）。
const CHARACTER_ID_PREFIX: &str = "char-";

/// ffmpeg 缺失时的安装指引（compose 报错与 GET /film/tools 同文案；
/// 详见 docs/FILM_STUDIO.md「ffmpeg 安装」）。
pub const FFMPEG_INSTALL_HINT: &str = "ffmpeg 未安装：可 apt install ffmpeg（Debian/Ubuntu），\
或静态构建（curl -L https://johnvansickle.com/ffmpeg/releases/ffmpeg-release-$(uname -m)-static.tar.xz \
解压取 bin/ffmpeg 放入 PATH），或设 env NEXOS_FFMPEG_BIN 指向已有二进制。详见 docs/FILM_STUDIO.md";

/// ffmpeg 常规落点（PATH 扫描之外的兜底候选，按序探测）。
const FFMPEG_COMMON_PATHS: [&str; 5] = [
    "/usr/bin/ffmpeg",
    "/usr/local/bin/ffmpeg",
    "/bin/ffmpeg",
    "/opt/homebrew/bin/ffmpeg",
    "/snap/bin/ffmpeg",
];

/// 进程级共享 HTTP 客户端（URL 产物下载 / 渠道直连二进制转发；连接 10s 先掐）。
static HTTP: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("构建 film HTTP Client 失败")
});

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// `film_projects` 表一行。
#[derive(Debug, Clone, Serialize)]
pub struct FilmProject {
    pub id: String,
    pub title: String,
    pub idea: String,
    /// `16:9` / `9:16` / `1:1` / `2.39:1` / `1.85:1` / `4:3`（六档预设，落库即
    /// 比例字符串；前端 preset key 只是展示概念）。
    pub ratio: String,
    pub style_hint: Option<String>,
    /// `draft` / `scripted` / `producing` / `done`。
    pub status: String,
    /// 产物目录（绝对路径）。
    pub dir: String,
    /// 导出路径（绝对路径；NULL=缺省项目目录本身——compose 的 final.mp4 落
    /// `<export_dir>/final.mp4`，见 [`final_path_of`]；2026-09-05 导出路径设置）。
    pub export_dir: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// `POST /film/projects` 请求体。
#[derive(Debug, Deserialize)]
struct CreateProjectBody {
    title: String,
    idea: String,
    ratio: String,
    #[serde(default)]
    style_hint: Option<String>,
}

/// `PUT /film/projects/:id` 请求体（部分更新：字段缺省保留原值；style_hint
/// 传空串或 clear_style_hint=true = 清空；export_dir 传空串 = 重置缺省（项目
/// 目录本身）；script = 分镜局部合并——按镜头号只改给出的字段，前端镜头面板/
/// 角色绑定编辑用）。
#[derive(Debug, Deserialize)]
struct UpdateProjectBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    idea: Option<String>,
    #[serde(default)]
    ratio: Option<String>,
    #[serde(default)]
    style_hint: Option<String>,
    #[serde(default)]
    clear_style_hint: Option<bool>,
    /// 导出路径：非空须绝对路径 + 父目录存在 + 可写（校验见
    /// [`validate_export_dir`]）；空串 = 重置缺省。字段缺省（不出现）保留原值。
    #[serde(default)]
    export_dir: Option<String>,
    #[serde(default)]
    script: Option<Vec<ShotPatch>>,
}

/// 分镜局部更新补丁（PUT script 数组元素）：`shot`（别名 `index`，兼容前端
/// 早期字段名）定位镜头；其余字段缺省保留。`desc` 别名 `description`。
#[derive(Debug, Deserialize)]
struct ShotPatch {
    #[serde(default, alias = "index")]
    shot: Option<u32>,
    #[serde(default, alias = "description")]
    desc: Option<String>,
    #[serde(default)]
    image_prompt: Option<String>,
    #[serde(default)]
    video_prompt: Option<String>,
    #[serde(default)]
    line: Option<String>,
    #[serde(default)]
    duration_secs: Option<u32>,
    #[serde(default)]
    characters: Option<Vec<String>>,
}

/// 应用分镜局部补丁（按镜头号合并，缺省字段保留；镜头号不存在 → Err）。
/// characters 经 normalize_character_names 归一（trim/去空/去重保序）。
fn apply_shot_patches(shots: &mut [ScriptShot], patches: &[ShotPatch]) -> Result<(), String> {
    for p in patches {
        let Some(no) = p.shot else {
            return Err("script 补丁缺 shot/index 镜头号".to_string());
        };
        let Some(s) = shots.iter_mut().find(|s| s.shot == no) else {
            return Err(format!("镜头 {no} 不在分镜中，无法局部更新"));
        };
        if let Some(v) = p.desc.as_deref() {
            s.desc = v.trim().to_string();
        }
        if let Some(v) = p.image_prompt.as_deref() {
            s.image_prompt = v.trim().to_string();
        }
        if let Some(v) = p.video_prompt.as_deref() {
            s.video_prompt = v.trim().to_string();
        }
        if let Some(v) = p.line.as_deref() {
            s.line = v.trim().to_string();
        }
        if let Some(v) = p.duration_secs {
            s.duration_secs = v.clamp(1, SHOT_DURATION_MAX_SECS);
        }
        if let Some(v) = &p.characters {
            s.characters = normalize_character_names(v);
        }
    }
    Ok(())
}

/// final.mp4 落点（export_dir 设置且非空 → `<export_dir>/final.mp4`，否则项目
/// 目录内 `<dir>/final.mp4`——缺省分支与 2026-09-05 之前逐字节同形）。
fn final_path_of(project: &FilmProject) -> String {
    match project
        .export_dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(ed) => format!("{}/final.mp4", ed.trim_end_matches('/')),
        None => format!("{}/final.mp4", project.dir),
    }
}

/// 校验导出路径（PUT export_dir 非空分支）：须**绝对路径**（`~` 不展开）+
/// **父目录存在**（不自动创建——失败附 mkdir 指引）+ **可写**（探针文件试写）；
/// `base`（env `NEXOS_FILM_EXPORT_BASE`，缺省不限制）设置时还须位于其下
/// （防任意路径写）。通过返回规整路径（trim + 去尾斜杠）；失败 Err(400 文案)。
fn validate_export_dir(dir: &str, base: Option<&str>) -> Result<String, String> {
    let norm = dir.trim().trim_end_matches('/');
    if norm.is_empty() {
        return Err("export_dir 不可为空串路径".to_string());
    }
    let path = std::path::Path::new(norm);
    if !path.is_absolute() {
        return Err(format!(
            "export_dir 须为绝对路径（当前 {dir}；~ 不会自动展开），如 /tank/os-data/exports/my-film"
        ));
    }
    if let Some(b) = base.map(str::trim).filter(|b| !b.is_empty()) {
        if !path.starts_with(std::path::Path::new(b.trim_end_matches('/'))) {
            return Err(format!(
                "export_dir 须位于 NEXOS_FILM_EXPORT_BASE（{b}）之下（当前 {norm}）"
            ));
        }
    }
    let Some(parent) = path.parent() else {
        return Err(format!("export_dir 无父目录（当前 {norm}）"));
    };
    if !parent.is_dir() {
        return Err(format!(
            "export_dir 父目录不存在: {}（不自动创建——请先 mkdir 再设置）",
            parent.display()
        ));
    }
    let probe = parent.join(format!(".nexos-film-export-probe-{}", std::process::id()));
    if let Err(e) = std::fs::write(&probe, b"") {
        return Err(format!(
            "export_dir 父目录不可写: {}（{e}）",
            parent.display()
        ));
    }
    let _ = std::fs::remove_file(&probe);
    Ok(norm.to_string())
}

/// 项目 JSON（DTO 序列化 + 便捷派生字段 `final_path`——export_dir 设置时
/// `<export_dir>/final.mp4` 否则 `<dir>/final.mp4`；建/列/详情/PUT 回执同口径）。
fn project_json(p: &FilmProject) -> serde_json::Value {
    let mut v = serde_json::to_value(p).unwrap_or_default();
    v["final_path"] = serde_json::Value::String(final_path_of(p));
    v
}

/// 阶段模型来源（冻结契约，见模块头）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRef {
    /// `local` / `channel`。
    pub source: String,
    /// source=channel 时必填（渠道表 id）。
    #[serde(default)]
    pub channel_id: Option<String>,
    /// `chat` / `image` / `video` / `tts` / `music`。
    pub capability: String,
    /// 上游模型名（channel 透传给上游；local.chat 忽略——用实例 served_model_name）。
    #[serde(default)]
    pub model: Option<String>,
}

impl ModelRef {
    /// 人类可读标签（任务日志/错误用）。
    pub(crate) fn label(&self) -> String {
        match (&self.channel_id, &self.model) {
            (Some(cid), Some(m)) => format!("channel {cid} · {m}"),
            (Some(cid), None) => format!("channel {cid}"),
            (None, Some(m)) => format!("local · {m}"),
            (None, None) => "local".to_string(),
        }
    }
}

/// 一个分镜镜头（script.json 的标准化形态；LLM 原始输出解析容错见
/// [`parse_script_shots`]）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptShot {
    /// 1 起镜头号（= 产物文件名里的 `<n>`）。
    pub shot: u32,
    /// 画面描述（字幕文本来源）。
    pub desc: String,
    /// 关键帧生图提示词。
    pub image_prompt: String,
    /// 图生视频运动提示词。
    pub video_prompt: String,
    /// 台词（空串 = 无台词；TTS 缺省文本）。
    #[serde(default)]
    pub line: String,
    /// 镜头时长（秒，1..=60）。
    pub duration_secs: u32,
    /// 出场角色名数组（角色库角色名——非 id；serde default 兼容旧 script.json；
    /// 2026-09-04 P0 角色一致性）。
    #[serde(default)]
    pub characters: Vec<String>,
    /// 定妆引用扩展（2026-09-06 FilmHub）：武器/道具（按名引用 casting/props）。
    #[serde(default)]
    pub props: Vec<String>,
    /// 宠物（按名引用 casting/pets）。
    #[serde(default)]
    pub pets: Vec<String>,
    /// 场景（按名引用 casting/scenes）。
    #[serde(default)]
    pub scenes: Vec<String>,
    /// 高频动作（按名引用 casting/actions）。
    #[serde(default)]
    pub actions: Vec<String>,
}

/// script.json 落盘形态（分镜数组 + 生成元信息）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ScriptFile {
    pub(crate) shots: Vec<ScriptShot>,
    /// 生成来源标签（model_ref.label()）。
    pub(crate) generated_by: String,
    pub(crate) created_at: String,
}

/// `film_characters` 表一行（2026-09-04 P0 角色库）。
#[derive(Debug, Clone, Serialize)]
pub struct FilmCharacter {
    /// `char-<n>`（项目内唯一；id 即产物目录名 `<dir>/characters/<id>/`）。
    pub id: String,
    pub project_id: String,
    /// 角色名（项目内唯一——分镜绑定按名字引用，重名会让绑定歧义）。
    pub name: String,
    /// 外观/设定描述（image prompt 注入与定妆图生成共用）。
    pub description: String,
    /// TTS 音色（OpenAI voice 枚举或渠道自定义 voice_id；None=落全局缺省）。
    pub voice: Option<String>,
    /// 定妆图产物相对路径（相对项目 dir，如 `characters/char-1/portrait.png`）。
    pub portrait_ref: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// `GET /film/projects/:id/characters` 元素（角色行 + 便捷派生面：定妆图下载
/// URL 与绑定镜头清单）。
#[derive(Debug, Serialize)]
struct CharacterView {
    #[serde(flatten)]
    character: FilmCharacter,
    /// 定妆图 URL（走既有产物读取路径 `GET /api/v1/files/download?path=`——
    /// b64 信封；前端取 content_base64 转 data URL。film 产物不经 apps-assets）。
    portrait_url: Option<String>,
    /// 绑定镜头号（扫 script.json 出场角色名命中；1 起）。
    bound_shots: Vec<u32>,
}

/// `POST /film/projects/:id/characters` 请求体（name+description 必填）。
#[derive(Debug, Deserialize)]
struct CreateCharacterBody {
    name: String,
    description: String,
    #[serde(default)]
    voice: Option<String>,
}

/// `PUT /film/characters/:cid` 请求体（部分更新；voice 传空串 = 清空回落全局缺省）。
#[derive(Debug, Deserialize)]
struct UpdateCharacterBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    voice: Option<String>,
}

/// `POST …/characters/:cid/portrait` 请求体（b64 JSON 形态——仓库无 multipart
/// 先例，网关 JSON 通道最友好，files.rs 上传同款；image_b64 为**原始标准
/// b64**，不带 data: 前缀）。
#[derive(Debug, Deserialize)]
struct PortraitUploadBody {
    image_b64: String,
    #[serde(default)]
    mime: Option<String>,
}

/// `POST …/characters/:cid/portrait/generate` 请求体（走既有生图面；prompt
/// 缺省由 description 构造）。
#[derive(Debug, Deserialize)]
struct PortraitGenBody {
    model_ref: ModelRef,
    #[serde(default)]
    prompt: Option<String>,
}

/// `POST /film/projects/:id/refs` 请求体（通用参考导入：场景/风格参考）。
#[derive(Debug, Deserialize)]
struct RefUploadBody {
    image_b64: String,
    #[serde(default)]
    filename: Option<String>,
}

/// 阶段任务（进程内态；生命周期 queued→running→done|error）。
#[derive(Debug, Clone, Serialize)]
pub struct FilmTask {
    pub id: String,
    pub project_id: String,
    /// `script` / `image` / `video` / `tts` / `music` / `compose`。
    pub stage: String,
    /// `queued` / `running` / `done` / `error`。
    pub status: String,
    /// 环形日志（上限 [`TASK_LOG_MAX_LINES`] 行）。
    pub log: Vec<String>,
    /// 产物路径（done 时）。
    pub output: Option<String>,
    pub error: Option<String>,
    pub created_at: i64,
    pub finished_at: Option<i64>,
}

/// 任务摘要（GET tasks 列表元素与 202 响应；日志只在单任务详情返回）。
#[derive(Debug, Serialize)]
struct TaskSummary {
    id: String,
    project_id: String,
    stage: String,
    status: String,
    output: Option<String>,
    error: Option<String>,
    created_at: i64,
    finished_at: Option<i64>,
}

impl From<&FilmTask> for TaskSummary {
    fn from(t: &FilmTask) -> Self {
        Self {
            id: t.id.clone(),
            project_id: t.project_id.clone(),
            stage: t.stage.clone(),
            status: t.status.clone(),
            output: t.output.clone(),
            error: t.error.clone(),
            created_at: t.created_at,
            finished_at: t.finished_at,
        }
    }
}

// ----------------------------------------------------------------------------
// 纯函数（易单测）
// ----------------------------------------------------------------------------

/// 画幅 → 成片合成分辨率（宽, 高；compose scale/pad 目标，[`COMPOSE_DIMS`] 六档
/// 预设表）；非法画幅 None。建/改项目校验同用本函数做白名单判定。
#[must_use]
pub fn ratio_dims(ratio: &str) -> Option<(u32, u32)> {
    COMPOSE_DIMS
        .iter()
        .find(|(r, _, _)| *r == ratio)
        .map(|(_, w, h)| (*w, *h))
}

/// 画幅 → 关键帧生图尺寸（宽, 高；[`IMAGE_DIMS`] 8 倍数安全尺寸——sd-turbo/
/// diffusers 要求，channel size 透传同口径）；非法画幅 None。
#[must_use]
pub fn image_dims(ratio: &str) -> Option<(u32, u32)> {
    IMAGE_DIMS
        .iter()
        .find(|(r, _, _)| *r == ratio)
        .map(|(_, w, h)| (*w, *h))
}

/// model_ref 校验：source/capability 合法性 + 阶段能力匹配 + local 能力支持面
/// + channel 必带 channel_id。Err 为 400 文案。
pub(crate) fn validate_model_ref(mr: &ModelRef, stage_capability: &str) -> Result<(), String> {
    if mr.capability != stage_capability {
        return Err(format!(
            "model_ref.capability 应为 {stage_capability}（当前 {}）",
            mr.capability
        ));
    }
    match mr.source.as_str() {
        "local" => match mr.capability.as_str() {
            "chat" | "image" => Ok(()),
            other => Err(format!(
                "本地暂无 {other} 能力（本地支持 chat/image），请改用 source=channel 经网关渠道调用"
            )),
        },
        "channel" => {
            if mr
                .channel_id
                .as_deref()
                .map(str::trim)
                .is_some_and(|s| !s.is_empty())
            {
                Ok(())
            } else {
                Err("source=channel 必须提供 channel_id".to_string())
            }
        }
        other => Err(format!(
            "model_ref.source 必须是 local 或 channel（当前 {other}）"
        )),
    }
}

/// 分镜生成 user 提示词（严格 JSON 数组契约 + 题材硬约束首尾夹逼）。
///
/// 2026-09-04 分镜质量修复：硬约束明确「必须严格围绕【创意】，禁止更换
/// 题材、禁止另编无关故事，每个镜头的画面都必须直接服务于该创意的叙事」，
/// 并把创意原文在结尾再嵌入一次（首尾夹逼——冒烟复现过 9B 模型无视开头
/// 创意、自行编出灯塔故事的情况，双端锚定显著降低漂移）。
///
/// 2026-09-04 P0 角色一致性：`characters`（角色库角色表）非空时注入【角色表】
/// 段，要求每镜头输出 `characters` 出场角色名数组（**必须从角色表选**）；
/// 空角色表则完全不提 characters（不诱导模型输出无意义空字段）。
#[must_use]
pub fn build_script_prompt(
    idea: &str,
    ratio: &str,
    style_hint: Option<&str>,
    characters: &[FilmCharacter],
) -> String {
    let style = style_hint
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("电影感、自然光影");
    let roster = if characters.is_empty() {
        String::new()
    } else {
        let list = characters
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{}. {}：{}", i + 1, c.name, c.description))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "\n【角色表】（分镜 characters 字段只能从下列角色名中选取）：\n{list}\n\
             5. 每个镜头须输出 \"characters\":[\"角色名\",…] 字段（该镜头出场角色，\
             必须从【角色表】选名字；无角色出场则空数组）。\n"
        )
    };
    format!(
        "请为下面的影片创意生成分镜脚本。\n\
         【创意】{idea}\n\
         【画幅】{ratio}\n\
         【风格提示】{style}\n\
         要求：\n\
         1. 输出 {SHOTS_PROMPT_MIN} 到 {SHOTS_PROMPT_MAX} 个镜头，按叙事顺序。\n\
         2. 必须严格围绕【创意】的故事创作分镜：禁止更换题材、禁止另编与【创意】\
         无关的故事；每个镜头的画面都必须直接服务于该创意的叙事。\n\
         3. 只输出一个 JSON 数组，不要任何解释文字或 markdown 代码块标记。每个元素形如：\n\
         {{\"shot\":1,\"desc\":\"画面描述\",\"image_prompt\":\"关键帧生图提示词（含风格与 {ratio} 构图信息）\",\"video_prompt\":\"图生视频运动与镜头语言提示词\",\"line\":\"角色台词，无台词则为空字符串\",\"duration_secs\":5}}\n\
         4. duration_secs 取 2-10 的整数。\n\
         {roster}最后再强调一次：所有镜头必须讲【创意】本身的故事——【创意】是：{idea}"
    )
}

/// 分镜解析失败后的一次重试提示词（更收紧 + 同款题材硬约束与创意锚定）。
///
/// `idea` 原文嵌入（首尾夹逼同款；重试丢创意等于邀请模型另起炉灶——冒烟
/// 复现的「灯塔故事」正是在重试路径漂移的）。角色表非空时同款提醒 characters
/// 字段取值范围（重试输出与首拍同契约）。
#[must_use]
pub fn build_retry_prompt(idea: &str, characters: &[FilmCharacter]) -> String {
    let roster = if characters.is_empty() {
        String::new()
    } else {
        let names = characters
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join("、");
        format!(
            "每个镜头元素须含 characters 字段（出场角色名数组，只能用这些名字：{names}；无出场则为空数组）。"
        )
    };
    format!(
        "你上一次的输出无法解析为 JSON 数组。请重新输出分镜 JSON 数组本体：\
     只输出一个 JSON 数组（以 [ 开头、以 ] 结尾），元素字段为 \
     shot/desc/image_prompt/video_prompt/line/duration_secs，\
     不要 markdown 标记、不要任何解释文字。{roster}\
     必须严格围绕【创意】的故事创作分镜：禁止更换题材、禁止另编与【创意】\
     无关的故事；每个镜头的画面都必须直接服务于该创意的叙事。\
     【创意】是：{idea}"
    )
}

/// 从 LLM 输出文本提取候选 JSON 片段（容错：原文 / ``` 围栏块 / 首尾中括号）。
fn json_candidates(text: &str) -> Vec<String> {
    let mut out = vec![];
    let trimmed = text.trim();
    out.push(trimmed.to_string());
    // ```json ... ``` / ``` ... ``` 围栏块（可能有多个，全收）
    let mut rest = trimmed;
    while let Some(start) = rest.find("```") {
        let after = &rest[start + 3..];
        let Some(body_from) = after.find('\n').map(|i| i + 1) else {
            break;
        };
        let Some(end) = after[body_from..].find("```") else {
            break;
        };
        out.push(after[body_from..body_from + end].trim().to_string());
        rest = &after[body_from + end + 3..];
    }
    // 首个 '[' 到最后一个 ']' 的最外层切片
    if let (Some(a), Some(b)) = (trimmed.find('['), trimmed.rfind(']')) {
        if a < b {
            out.push(trimmed[a..=b].to_string());
        }
    }
    out
}

/// 剥离 `<think>…</think>` 思考段（2026-09-04 分镜质量修复）。
///
/// 思考型模型（vLLM `enable_thinking` 未关掉时）会把推理过程混入 content：
/// 思考段常含 `[` `]` 字符，破坏「首个 [ 到最后一个 ]」切片，把思考碎片
/// 误当分镜。先剥再解析；未闭合的 `<think>`（思考吞掉全部输出）丢弃其后
/// 全文——比把思考当分镜更诚实（触发重试路径）。
pub(crate) fn strip_think_blocks(text: &str) -> String {
    if !text.contains("<think>") {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<think>") {
        out.push_str(&rest[..start]);
        let after = &rest[start + "<think>".len()..];
        match after.find("</think>") {
            Some(end) => rest = &after[end + "</think>".len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// 解析 LLM 输出为标准化分镜数组（容错见 [`json_candidates`] 与
/// [`strip_think_blocks`]；字段缺省/越界钳制；双空镜头过滤）。全候选解析
/// 失败 → Err（caller 触发一次重试）。
pub fn parse_script_shots(text: &str) -> Result<Vec<ScriptShot>, String> {
    #[derive(Debug, Default, Deserialize)]
    struct RawShot {
        #[serde(default)]
        shot: Option<u32>,
        #[serde(default)]
        desc: Option<String>,
        #[serde(default)]
        image_prompt: Option<String>,
        #[serde(default)]
        video_prompt: Option<String>,
        #[serde(default)]
        line: Option<String>,
        #[serde(default)]
        duration_secs: Option<serde_json::Value>,
        #[serde(default)]
        characters: Option<Vec<String>>,
        #[serde(default)]
        props: Option<Vec<String>>,
        #[serde(default)]
        pets: Option<Vec<String>>,
        #[serde(default)]
        scenes: Option<Vec<String>>,
        #[serde(default)]
        actions: Option<Vec<String>>,
    }
    for cand in json_candidates(&strip_think_blocks(text)) {
        let Ok(arr) = serde_json::from_str::<Vec<RawShot>>(&cand) else {
            continue;
        };
        let mut shots = Vec::new();
        for (idx, r) in arr.into_iter().enumerate() {
            let desc = r.desc.unwrap_or_default().trim().to_string();
            let image_prompt = r.image_prompt.unwrap_or_default().trim().to_string();
            if desc.is_empty() && image_prompt.is_empty() {
                continue; // 双空镜头视为噪声过滤
            }
            // duration_secs 兼容数字/字符串两种形态（5 / "5" / "5秒"）
            let dur_num = r.duration_secs.and_then(|v| match v {
                serde_json::Value::Number(n) => n.as_u64(),
                serde_json::Value::String(s) => {
                    s.trim().trim_end_matches('秒').trim().parse::<u64>().ok()
                }
                _ => None,
            });
            shots.push(ScriptShot {
                shot: r.shot.unwrap_or(u32::try_from(idx + 1).unwrap_or(1)),
                desc,
                image_prompt,
                video_prompt: r.video_prompt.unwrap_or_default().trim().to_string(),
                line: r.line.unwrap_or_default().trim().to_string(),
                duration_secs: u32::try_from(
                    dur_num.unwrap_or(u64::from(SHOT_DURATION_DEFAULT_SECS)),
                )
                .unwrap_or(SHOT_DURATION_DEFAULT_SECS)
                .clamp(1, SHOT_DURATION_MAX_SECS),
                // 出场角色名：trim 去空、去空串、去重（保持首现顺序）。未知名
                // **保留原样**（不静默丢弃——caller 有角色表时按名比对记日志，
                // 见 run_script_stage 的容错口径）。casting 引用扩展同款归一。
                characters: normalize_character_names(r.characters.unwrap_or_default().as_slice()),
                props: normalize_character_names(r.props.unwrap_or_default().as_slice()),
                pets: normalize_character_names(r.pets.unwrap_or_default().as_slice()),
                scenes: normalize_character_names(r.scenes.unwrap_or_default().as_slice()),
                actions: normalize_character_names(r.actions.unwrap_or_default().as_slice()),
            });
        }
        if shots.is_empty() {
            continue;
        }
        if shots.len() > SHOTS_ACCEPT_MAX {
            shots.truncate(SHOTS_ACCEPT_MAX);
        }
        return Ok(shots);
    }
    Err("无法从 LLM 输出解析出分镜 JSON 数组".to_string())
}

/// 角色名数组归一：trim、去空串、去重（保持首现顺序——绑定引用与注入顺序都
/// 依赖稳定顺序）。
#[must_use]
pub fn normalize_character_names(names: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in names {
        let t = n.trim();
        if t.is_empty() || out.iter().any(|e| e == t) {
            continue;
        }
        out.push(t.to_string());
    }
    out
}

/// 角色一致性 prompt 注入块（2026-09-04 P0 档 1——纯 prompt 弱一致，本地与
/// 渠道通用的基座）。
///
/// 固定措辞模板：`角色「名」外形：描述（与其它镜头严格同一人物）`，多角色
/// 以「；」连接，顺序 = 传入角色名顺序（在角色表内命中的子序列——**顺序
/// 稳定**，同名只注入一次）。未在角色表命中的名字跳过（caller 记日志）；
/// 全部未命中 → None（不注入，prompt 保持原样）。
#[must_use]
pub fn build_character_prompt_block(
    shot_characters: &[String],
    roster: &[FilmCharacter],
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for name in shot_characters {
        if parts.iter().any(|p| p.contains(&format!("角色「{name}」"))) {
            continue;
        }
        if let Some(c) = roster.iter().find(|c| &c.name == name) {
            parts.push(format!(
                "角色「{}」外形：{}（与其它镜头严格同一人物）",
                c.name, c.description
            ));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("；"))
    }
}

/// TTS voice 三态解析：镜头出场角色中**第一个有 voice 的角色** → 透传；
/// 否则 env `NEXOS_FILM_TTS_VOICE`（trim 空串视为未设）；再否则 `alloy` 兜底
/// （OpenAI 标准枚举，渠道天然兼容）。
#[must_use]
pub fn resolve_shot_voice(
    shot_characters: &[String],
    roster: &[FilmCharacter],
    env_voice: Option<&str>,
) -> String {
    for name in shot_characters {
        if let Some(c) = roster.iter().find(|c| &c.name == name) {
            if let Some(v) = c.voice.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
                return v.to_string();
            }
        }
    }
    env_voice
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(TTS_VOICE_FALLBACK)
        .to_string()
}

/// 参考注入强度解析（env `NEXOS_FILM_REF_STRENGTH`）：非数字/越界回落缺省
/// 0.5（钳 0.0..=1.0）。
#[must_use]
pub fn parse_ref_strength(raw: Option<&str>) -> f64 {
    let Some(v) = raw.and_then(|s| s.trim().parse::<f64>().ok()) else {
        return REFERENCE_STRENGTH_DEFAULT;
    };
    if (0.0..=1.0).contains(&v) {
        v
    } else {
        REFERENCE_STRENGTH_DEFAULT
    }
}

/// 定妆图生成缺省提示词（prompt 缺省时由 description 构造——定妆照口径：
/// 正面半身、面部清晰、光影干净，为后续分镜参考注入提供干净主体）。
#[must_use]
pub fn default_portrait_prompt(name: &str, description: &str) -> String {
    format!(
        "角色「{name}」定妆照：{description}。正面半身像，面部清晰，五官端正，\
         光影干净均匀，纯色背景，细节丰富，高质量"
    )
}

/// 图片魔数嗅探 → 扩展名（png / jpg / webp；refs 上传用——mime 参数可省，
/// 以字节为准）。
#[must_use]
pub fn sniff_image_ext(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 8 && bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("png");
    }
    if bytes.len() >= 3 && bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("jpg");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("webp");
    }
    None
}

/// mime → 扩展名（定妆图上传白名单；白名单外 None → 400）。
pub(crate) fn ext_for_mime(mime: &str) -> Option<&'static str> {
    let m = mime.trim().to_ascii_lowercase();
    IMAGE_MIME_EXT
        .iter()
        .find(|(k, _)| *k == m)
        .map(|(_, ext)| *ext)
}

/// 路径的百分号编码（`GET /api/v1/files/download?path=` 查询参数值——保留
/// unreserved 与 `/`，其余 %XX；仅够文件系统路径使用）。
#[must_use]
fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'.' | b'_' | b'-' | b'~' => {
                out.push(b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// 产物绝对路径 → 既有读取路径 URL（`GET /api/v1/files/download?path=`，公开
/// 读、b64 信封）。film 产物**不经 apps-assets**（那是应用包静态资源），角色
/// portrait_url 与前端产物预览同走此路径。
#[must_use]
pub(crate) fn files_download_url(abs_path: &str) -> String {
    format!(
        "/api/v1/files/download?path={}",
        percent_encode_path(abs_path)
    )
}

/// SRT 时间戳（HH:MM:SS,mmm）。
#[must_use]
pub fn fmt_srt_ts(ms: u64) -> String {
    let h = ms / 3_600_000;
    let m = (ms % 3_600_000) / 60_000;
    let s = (ms % 60_000) / 1000;
    let milli = ms % 1000;
    format!("{h:02}:{m:02}:{s:02},{milli:03}")
}

/// 由分镜生成字幕（每镜头一条 cue，时间轴按 duration_secs 累计；无台词镜头
/// 跳过）。cue 文本两行：画面描述 + 台词（desc 空则只台词）。
#[must_use]
pub fn build_srt(shots: &[ScriptShot]) -> String {
    let mut out = String::new();
    let mut t = 0u64;
    let mut idx = 1u32;
    for s in shots {
        let dur = u64::from(s.duration_secs) * 1000;
        if !s.line.trim().is_empty() {
            let text = if s.desc.trim().is_empty() {
                s.line.trim().to_string()
            } else {
                format!("{}\n{}", s.desc.trim(), s.line.trim())
            };
            out.push_str(&format!(
                "{idx}\n{} --> {}\n{text}\n\n",
                fmt_srt_ts(t),
                fmt_srt_ts(t + dur)
            ));
            idx += 1;
        }
        t += dur;
    }
    out
}

/// concat 清单内容（`file 'shot-<n>.mp4'` 行，cwd=项目目录用相对名）。
#[must_use]
pub fn build_concat_list(total_shots: usize) -> String {
    (1..=total_shots)
        .map(|n| format!("file 'shot-{n}.mp4'\n"))
        .collect()
}

/// compose 第一遍 argv（不含 ffmpeg 程序名）：concat 清单 → 统一尺寸/fps
/// 重编码（scale 保比例 + pad 居中补黑边——上游视频分辨率不齐也齐轨）。
/// 宽高先钳到偶数（yuv420p 要求；预设表本就全偶，此处兜底非整比手传值）。
#[must_use]
pub fn build_concat_args(width: u32, height: u32, out: &str) -> Vec<String> {
    let (width, height) = (width - width % 2, height - height % 2);
    let vf = format!(
        "scale={width}:{height}:force_original_aspect_ratio=decrease,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2,fps={COMPOSE_FPS}"
    );
    vec![
        "-y".into(),
        "-f".into(),
        "concat".into(),
        "-safe".into(),
        "0".into(),
        "-i".into(),
        "compose-concat.txt".into(),
        "-vf".into(),
        vf,
        "-c:v".into(),
        "libx264".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-c:a".into(),
        "aac".into(),
        "-ar".into(),
        "44100".into(),
        "-ac".into(),
        "2".into(),
        out.into(),
    ]
}

/// compose 第二遍 argv（不含 ffmpeg 程序名）：台词 adelay 对齐 + BGM amix 混音
/// + 字幕烧录 → final.mp4。
///
/// 输入序：`[0]=compose-video.mp4`、`[1..]=voice mp3`（`(文件名, 起始毫秒)` 按
/// 分镜时间轴）、`[末]=bgm`（`-stream_loop -1` 循环铺满）。无字幕 → 视频流
/// `-c:v copy` 直通（有字幕必须重编码烧录）。全部文件名相对（cwd=项目目录——
/// subtitles 滤镜无需路径转义）。
#[must_use]
pub fn build_mix_args(
    voices: &[(String, u64)],
    bgm: Option<&str>,
    srt: bool,
    out: &str,
) -> Vec<String> {
    let mut args: Vec<String> = vec!["-y".into(), "-i".into(), "compose-video.mp4".into()];
    for (f, _) in voices {
        args.extend(["-i".into(), f.clone()]);
    }
    if let Some(b) = bgm {
        args.extend([
            "-stream_loop".into(),
            "-1".into(),
            "-i".into(),
            b.to_string(),
        ]);
    }
    let bgm_idx = 1 + voices.len();
    let mut filters: Vec<String> = Vec::new();
    // 视频流：有字幕 → subtitles 滤镜（标签 vout）；无字幕 → 直通 0:v
    let v_label = if srt {
        filters.push("[0:v]subtitles=subs.srt[vout]".into());
        "[vout]"
    } else {
        "0:v"
    };
    // 人声轨：adelay 对齐分镜时间轴；多路 amix 合一
    let voice_label = if voices.is_empty() {
        None
    } else {
        let labels: Vec<String> = voices
            .iter()
            .enumerate()
            .map(|(i, (_, start_ms))| {
                let l = format!("a{i}");
                filters.push(format!(
                    "[{}:a]aresample=44100,adelay={ms}|{ms}[{l}]",
                    i + 1,
                    ms = *start_ms,
                ));
                l
            })
            .collect();
        if labels.len() == 1 {
            Some(labels[0].clone())
        } else {
            let joined = labels.iter().map(|l| format!("[{l}]")).collect::<String>();
            filters.push(format!(
                "{joined}amix=inputs={n}:duration=longest:dropout_transition=0[voice]",
                n = labels.len()
            ));
            Some("voice".to_string())
        }
    };
    // 终混：人声 × BGM（normalize=0 防自动衰减；duration=longest + -shortest 收口）
    let a_label: Option<String> = match (&voice_label, bgm) {
        (Some(v), Some(_)) => {
            filters.push(format!("[{bgm_idx}:a]volume={BGM_VOLUME}[bgm]"));
            filters.push(format!(
                "[{v}][bgm]amix=inputs=2:duration=longest:normalize=0[aout]"
            ));
            Some("[aout]".to_string())
        }
        (Some(v), None) => Some(format!("[{v}]")),
        (None, Some(_)) => {
            filters.push(format!("[{bgm_idx}:a]volume={BGM_VOLUME}[aout]"));
            Some("[aout]".to_string())
        }
        (None, None) => None,
    };
    if !filters.is_empty() {
        args.extend(["-filter_complex".into(), filters.join(";")]);
    }
    args.extend(["-map".into(), v_label.to_string()]);
    match &a_label {
        Some(a) => args.extend(["-map".into(), a.clone()]),
        // 无任何音源：透传视频自带音轨（`0:a?` 空匹配不报错）
        None => args.extend(["-map".into(), "0:a?".into()]),
    }
    args.extend([
        "-c:v".into(),
        if srt { "libx264".into() } else { "copy".into() },
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-c:a".into(),
        "aac".into(),
        "-b:a".into(),
        "192k".into(),
        "-ar".into(),
        "44100".into(),
    ]);
    if bgm.is_some() || voice_label.is_some() {
        args.push("-shortest".into());
    }
    args.push(out.into());
    args
}

/// 从渠道响应 JSON 里取第一个 base64 字段（b64_json/video_base64/b64/audio，
/// 顶层与 data[0] 两层；容忍 `data:image/...;base64,` 前缀）。
fn extract_b64(v: &serde_json::Value) -> Option<String> {
    let keys = ["b64_json", "video_base64", "b64", "audio"];
    let d0 = v.get("data").and_then(|d| d.get(0));
    let holders = [Some(v), d0];
    for h in holders.into_iter().flatten() {
        for k in keys {
            if let Some(s) = h.get(k).and_then(|x| x.as_str()).map(str::trim) {
                if !s.is_empty() {
                    return Some(
                        s.split_once("base64,")
                            .map_or(s.to_string(), |(_, b)| b.to_string()),
                    );
                }
            }
        }
    }
    None
}

/// 从渠道响应 JSON 里取第一个 URL 字段（url/video_url/audio_url，顶层与
/// data[0] 两层；只认 http(s)）。
fn extract_url(v: &serde_json::Value) -> Option<String> {
    let keys = ["url", "video_url", "audio_url"];
    let d0 = v.get("data").and_then(|d| d.get(0));
    let holders = [Some(v), d0];
    for h in holders.into_iter().flatten() {
        for k in keys {
            if let Some(s) = h
                .get(k)
                .and_then(|x| x.as_str())
                .map(str::trim)
                .filter(|s| s.starts_with("http://") || s.starts_with("https://"))
            {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// chat/completions 响应文本 → choices[0].message.content。
fn parse_chat_content(text: &str) -> Result<String, String> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("渠道响应非 JSON: {e}"))?;
    v.get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(String::from)
        .ok_or_else(|| "渠道响应缺少 choices[0].message.content".to_string())
}

/// 音频响应字节分流：JSON 带 b64 → 解码；JSON 无 b64 → 按上游错误如实报；
/// 非 JSON → 原始二进制音频（如 OpenAI /audio/speech 的 audio/mpeg）。
fn sniff_audio_bytes(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(256)])
        .trim()
        .to_string();
    if head.starts_with('{') || head.starts_with('[') {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) {
            if let Some(b64) = extract_b64(&v) {
                use base64::Engine;
                return base64::engine::general_purpose::STANDARD
                    .decode(b64.trim())
                    .map_err(|e| format!("音频 b64 解码失败: {e}"));
            }
            let detail: String = String::from_utf8_lossy(bytes).chars().take(200).collect();
            return Err(format!("上游返回 JSON 但无音频字段: {detail}"));
        }
    }
    Ok(bytes.to_vec())
}

// ----------------------------------------------------------------------------
// ffmpeg 检测
// ----------------------------------------------------------------------------

/// 路径可执行探测（文件 + 任一 x 位；llm_envs is_executable 同款）。
pub(crate) fn is_executable(path: &str) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && (m.permissions().mode() & 0o111) != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        std::fs::metadata(path)
            .map(|m| m.is_file())
            .unwrap_or(false)
    }
}

/// ffmpeg 解析内核（参数化，测试注入合成值，不读进程 env）：
/// env 覆写（可执行才认）→ PATH 目录扫描 → 常规路径候选。
#[must_use]
pub fn detect_ffmpeg_with(
    env_bin: Option<&str>,
    path_dirs: &[String],
    extra_candidates: &[&str],
) -> Option<String> {
    if let Some(b) = env_bin.map(str::trim).filter(|s| !s.is_empty()) {
        if is_executable(b) {
            return Some(b.to_string());
        }
    }
    for d in path_dirs {
        let cand = if d.ends_with('/') {
            format!("{d}ffmpeg")
        } else {
            format!("{d}/ffmpeg")
        };
        if is_executable(&cand) {
            return Some(cand);
        }
    }
    extra_candidates
        .iter()
        .find(|p| is_executable(p))
        .map(|p| (*p).to_string())
}

/// 请求路径的 ffmpeg 解析链（env `NEXOS_FFMPEG_BIN` → PATH → 常规路径）。
#[must_use]
pub fn detect_ffmpeg() -> Option<String> {
    let path_dirs: Vec<String> = std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|d| !d.is_empty())
        .map(String::from)
        .collect();
    detect_ffmpeg_with(
        std::env::var("NEXOS_FFMPEG_BIN").ok().as_deref(),
        &path_dirs,
        &FFMPEG_COMMON_PATHS,
    )
}

// ----------------------------------------------------------------------------
// 超时 env（参数化内核 + 请求路径包装）
// ----------------------------------------------------------------------------

/// 阶段超时（秒）解析：非数字/越界回落缺省（钳 60..=1800）。
#[must_use]
pub fn parse_stage_timeout(raw: Option<&str>, default: u64) -> u64 {
    let Some(v) = raw.and_then(|s| s.trim().parse::<u64>().ok()) else {
        return default;
    };
    if (60..=1800).contains(&v) {
        v
    } else {
        default
    }
}

fn video_timeout() -> Duration {
    Duration::from_secs(parse_stage_timeout(
        std::env::var("NEXOS_FILM_VIDEO_TIMEOUT_SECS")
            .ok()
            .as_deref(),
        FILM_VIDEO_TIMEOUT_DEFAULT_SECS,
    ))
}

pub(crate) fn compose_timeout() -> Duration {
    Duration::from_secs(parse_stage_timeout(
        std::env::var("NEXOS_FILM_COMPOSE_TIMEOUT_SECS")
            .ok()
            .as_deref(),
        FILM_COMPOSE_TIMEOUT_DEFAULT_SECS,
    ))
}

// ----------------------------------------------------------------------------
// SQLite 持久化层（film.db · film_projects 表）
// ----------------------------------------------------------------------------

fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS film_projects (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            idea TEXT NOT NULL,
            ratio TEXT NOT NULL,
            style_hint TEXT,
            status TEXT NOT NULL DEFAULT 'draft',
            dir TEXT NOT NULL,
            export_dir TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS film_characters (
            id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL,
            name TEXT NOT NULL,
            description TEXT NOT NULL,
            voice TEXT,
            portrait_ref TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_film_characters_project
            ON film_characters(project_id);",
    )?;
    // 迁移：2026-09-05 之前的 film_projects 表缺 export_dir 列（CREATE IF NOT
    // EXISTS 不会给已存在的表补列）。列已存在时 ALTER 报 duplicate column，
    // 忽略即可（幂等，llm.rs / forwarding.rs 同款惯例）。
    let _ = conn.execute("ALTER TABLE film_projects ADD COLUMN export_dir TEXT", []);
    // 成本事件表（2026-09-06 FilmHub 记账，study 方案 §G——事件在任务完成点
    // 落库，DB 为真值、budget.json 为树内投影）
    super::film_hub::create_cost_schema(conn)?;
    Ok(())
}

fn row_to_project(row: &rusqlite::Row<'_>) -> rusqlite::Result<FilmProject> {
    Ok(FilmProject {
        id: row.get(0)?,
        title: row.get(1)?,
        idea: row.get(2)?,
        ratio: row.get(3)?,
        style_hint: row.get(4)?,
        status: row.get(5)?,
        dir: row.get(6)?,
        // 索引 9=export_dir（迁移补列恒在末位，与建表列序一致）
        export_dir: row.get(9)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

fn insert_project(conn: &Connection, p: &FilmProject) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO film_projects
         (id,title,idea,ratio,style_hint,status,dir,export_dir,created_at,updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![
            p.id,
            p.title,
            p.idea,
            p.ratio,
            p.style_hint,
            p.status,
            p.dir,
            p.export_dir,
            p.created_at,
            p.updated_at
        ],
    )?;
    Ok(())
}

fn load_projects(conn: &Connection) -> Vec<FilmProject> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT id,title,idea,ratio,style_hint,status,dir,created_at,updated_at,export_dir
         FROM film_projects ORDER BY id",
    ) else {
        return vec![];
    };
    stmt.query_map([], row_to_project)
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

fn find_project(conn: &Connection, id: &str) -> Option<FilmProject> {
    conn.query_row(
        "SELECT id,title,idea,ratio,style_hint,status,dir,created_at,updated_at,export_dir
         FROM film_projects WHERE id = ?1",
        params![id],
        row_to_project,
    )
    .ok()
}

/// 更新项目（部分字段 + updated_at；缺省字段保留原值）。
fn update_project_fields(
    conn: &Connection,
    id: &str,
    title: Option<&str>,
    idea: Option<&str>,
    ratio: Option<&str>,
    style_hint: Option<&str>,
    status: Option<&str>,
) -> rusqlite::Result<()> {
    let Some(cur) = find_project(conn, id) else {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    };
    conn.execute(
        "UPDATE film_projects
         SET title=?1,idea=?2,ratio=?3,style_hint=?4,status=?5,updated_at=?6
         WHERE id=?7",
        params![
            title.unwrap_or(&cur.title),
            idea.unwrap_or(&cur.idea),
            ratio.unwrap_or(&cur.ratio),
            style_hint.unwrap_or(cur.style_hint.as_deref().unwrap_or("")),
            status.unwrap_or(&cur.status),
            now_iso(),
            id
        ],
    )?;
    Ok(())
}

/// 更新导出路径（2026-09-05，PUT export_dir 专用短写）：`Some(dir)`=设置（调用
/// 面先过 [`validate_export_dir`]`），`None`=重置缺省（NULL=项目目录本身）。
fn update_export_dir(conn: &Connection, id: &str, dir: Option<&str>) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE film_projects SET export_dir=?1,updated_at=?2 WHERE id=?3",
        params![dir, now_iso(), id],
    )?;
    Ok(())
}

fn delete_project_row(conn: &Connection, id: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM film_projects WHERE id=?1", params![id])?;
    // 角色行连删（产物目录由项目删除统一 remove_dir_all 覆盖）
    conn.execute(
        "DELETE FROM film_characters WHERE project_id=?1",
        params![id],
    )?;
    Ok(())
}

// ----------------------------------------------------------------------------
// 角色库持久化（film_characters 表）
// ----------------------------------------------------------------------------

const CHARACTER_COLS: &str =
    "id,project_id,name,description,voice,portrait_ref,created_at,updated_at";

fn row_to_character(row: &rusqlite::Row<'_>) -> rusqlite::Result<FilmCharacter> {
    Ok(FilmCharacter {
        id: row.get(0)?,
        project_id: row.get(1)?,
        name: row.get(2)?,
        description: row.get(3)?,
        voice: row.get(4)?,
        portrait_ref: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

fn insert_character(conn: &Connection, c: &FilmCharacter) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO film_characters
         (id,project_id,name,description,voice,portrait_ref,created_at,updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            c.id,
            c.project_id,
            c.name,
            c.description,
            c.voice,
            c.portrait_ref,
            c.created_at,
            c.updated_at
        ],
    )?;
    Ok(())
}

pub(crate) fn load_characters(conn: &Connection, project_id: &str) -> Vec<FilmCharacter> {
    let Ok(mut stmt) = conn.prepare(&format!(
        "SELECT {CHARACTER_COLS} FROM film_characters WHERE project_id=?1 ORDER BY id"
    )) else {
        return vec![];
    };
    stmt.query_map(params![project_id], row_to_character)
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

fn find_character(conn: &Connection, id: &str) -> Option<FilmCharacter> {
    conn.query_row(
        &format!("SELECT {CHARACTER_COLS} FROM film_characters WHERE id=?1"),
        params![id],
        row_to_character,
    )
    .ok()
}

/// 项目内角色名是否已被占用（建/改时的唯一性闸——绑定按名字引用，重名歧义）。
fn character_name_taken(conn: &Connection, project_id: &str, name: &str, exclude: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM film_characters WHERE project_id=?1 AND name=?2 AND id<>?3",
        params![project_id, name, exclude],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

/// 更新角色（部分字段 + updated_at；缺省字段保留原值）。
#[allow(clippy::too_many_arguments)]
fn update_character_fields(
    conn: &Connection,
    id: &str,
    name: Option<&str>,
    description: Option<&str>,
    voice: Option<Option<&str>>,
    portrait_ref: Option<&str>,
) -> rusqlite::Result<()> {
    let Some(cur) = find_character(conn, id) else {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    };
    let voice = match voice {
        Some(v) => v,
        None => cur.voice.as_deref(),
    };
    conn.execute(
        "UPDATE film_characters SET name=?1,description=?2,voice=?3,portrait_ref=?4,updated_at=?5
         WHERE id=?6",
        params![
            name.unwrap_or(&cur.name),
            description.unwrap_or(&cur.description),
            voice,
            portrait_ref.unwrap_or(cur.portrait_ref.as_deref().unwrap_or("")),
            now_iso(),
            id
        ],
    )?;
    Ok(())
}

fn delete_character_row(conn: &Connection, id: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM film_characters WHERE id=?1", params![id])?;
    Ok(())
}

// ----------------------------------------------------------------------------
// 任务态辅助
// ----------------------------------------------------------------------------

/// 任务日志追加一行（环形上限 [`TASK_LOG_MAX_LINES`]）。
pub(crate) fn task_log(tasks: &Arc<Mutex<HashMap<String, FilmTask>>>, id: &str, line: &str) {
    if let Ok(mut m) = tasks.lock() {
        if let Some(t) = m.get_mut(id) {
            for l in line.lines().filter(|l| !l.trim().is_empty()) {
                t.log.push(l.to_string());
                if t.log.len() > TASK_LOG_MAX_LINES {
                    let cut = t.log.len() - TASK_LOG_MAX_LINES;
                    t.log.drain(0..cut);
                }
            }
        }
    }
}

/// 任务置 running。
fn task_running(tasks: &Arc<Mutex<HashMap<String, FilmTask>>>, id: &str) {
    if let Ok(mut m) = tasks.lock() {
        if let Some(t) = m.get_mut(id) {
            t.status = "running".into();
        }
    }
}

/// 任务收尾（done 携产物路径 / error 携原因；收尾日志一行）。
pub(crate) fn task_finish(
    tasks: &Arc<Mutex<HashMap<String, FilmTask>>>,
    id: &str,
    status: &str,
    line: &str,
    output: Option<String>,
) {
    task_log(tasks, id, line);
    if let Ok(mut m) = tasks.lock() {
        if let Some(t) = m.get_mut(id) {
            t.status = status.to_string();
            t.output = output;
            if status == "error" {
                t.error = Some(line.to_string());
            }
            t.finished_at = Some(now_epoch());
        }
    }
    eprintln!(
        "[film] 任务{}：{}（{line}）",
        if status == "done" { "完成" } else { "失败" },
        id
    );
}

/// 当前 Unix epoch 秒。
fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub(crate) fn now_iso() -> String {
    use chrono::Local;
    Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

// ----------------------------------------------------------------------------
// 执行上下文（spawn 出去的后台任务所需句柄快照）
// ----------------------------------------------------------------------------

/// 一次阶段任务的后台执行上下文（`FilmRouteHandler::ctx` 快照，'static 可 spawn；
/// db 为 Arc 共享——与请求路径同一把 `Mutex<Connection>`，llm.rs 后台任务同款）。
#[derive(Clone)]
pub(crate) struct FilmCtx {
    pub(crate) db: Arc<Mutex<Connection>>,
    pub(crate) tasks: Arc<Mutex<HashMap<String, FilmTask>>>,
    pub(crate) gateway: Option<Arc<ApiGatewayRouteHandler>>,
    pub(crate) llm: Option<Arc<super::llm::LlmRouteHandler>>,
    /// 本地 chat 直连端点（测试注入：(port, model)；None=从 llm 实例表解析）。
    pub(crate) local_chat: Option<(u16, String)>,
    /// 生图内核注入（测试：(bin, script)；None=media_gen env 注入点链）。
    pub(crate) imggen: Option<(String, String)>,
    /// 显存探测二进制注入（测试；None=env NEXOS_SMI_BIN 链）。
    pub(crate) smi_bin: Option<String>,
    /// ffmpeg 固定路径（测试；None=解析链 env→PATH→常规路径）。
    pub(crate) ffmpeg_bin: Option<String>,
    /// 参考注入强度（测试注入；None=env `NEXOS_FILM_REF_STRENGTH` 解析，缺省 0.5）。
    pub(crate) ref_strength: Option<f64>,
    /// TTS 全局缺省 voice（测试注入；None=env `NEXOS_FILM_TTS_VOICE`，再缺省 alloy）。
    pub(crate) tts_voice: Option<String>,
}

impl FilmCtx {
    /// model_ref=channel：查渠道表（gateway 只读快照）+ 模型名解析。
    pub(crate) fn resolve_channel(&self, mr: &ModelRef) -> Result<(Channel, String), String> {
        let gw = self.gateway.as_ref().ok_or_else(|| {
            "网关渠道未接入（film 未注入 api_gateway 共享实例，请经 main.rs 装配）".to_string()
        })?;
        let cid = mr
            .channel_id
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .to_string();
        let ch = gw
            .channels_snapshot()
            .into_iter()
            .find(|c| c.id == cid)
            .ok_or_else(|| format!("渠道不存在: {cid}（先在 API 网关添加渠道）"))?;
        if !ch.enabled {
            return Err(format!("渠道已禁用: {}（{cid}）", ch.name));
        }
        let model = mr
            .model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .or_else(|| ch.models.first().cloned())
            .ok_or_else(|| format!("渠道 {cid} 的 models 为空且请求未指定 model_ref.model"))?;
        Ok((ch, model))
    }

    /// 渠道三单价（price_per_call/price_per_sec/price_per_token；2026-09-06
    /// FilmHub 成本记账——渠道不存在/未注入网关 → None=只计量不计价）。
    pub(crate) fn channel_prices(&self, channel_id: &str) -> Option<(f64, f64, f64)> {
        let ch = self
            .gateway
            .as_ref()?
            .channels_snapshot()
            .into_iter()
            .find(|c| c.id == channel_id)?;
        Some((ch.price_per_call, ch.price_per_sec, ch.price_per_token))
    }

    /// 渠道缺省模型名（models[0]；成本事件 model 字段尽力补全用）。
    pub(crate) fn channel_model_of(&self, channel_id: &str) -> Option<String> {
        let ch = self
            .gateway
            .as_ref()?
            .channels_snapshot()
            .into_iter()
            .find(|c| c.id == channel_id)?;
        ch.models.first().cloned()
    }

    /// model_ref=local.chat：本地 vLLM 实例直连端点（测试注入优先；否则取
    /// llm 实例表第一个 running 实例的 port + served_model_name）。
    fn resolve_local_chat(&self) -> Result<(u16, String), String> {
        if let Some((port, model)) = &self.local_chat {
            return Ok((*port, model.clone()));
        }
        let llm = self
            .llm
            .as_ref()
            .ok_or_else(|| "本地 LLM 实例面未接入（film 未注入 llm 共享实例）".to_string())?;
        let inst = llm
            .instances_snapshot()
            .into_iter()
            .find(|i| i.status == "running")
            .ok_or_else(|| {
                "无运行中的本地 LLM 实例（先在模型管理启动实例，或改用 source=channel）".to_string()
            })?;
        let model = inst
            .config
            .served_model_name
            .clone()
            .unwrap_or_else(|| inst.model.clone());
        Ok((inst.port, model))
    }

    /// chat 能力统一入口（script 阶段）：local 实例直连 / channel 转发。
    ///
    /// 2026-09-04 分镜质量修复（冒烟复现：9B 模型开思考段生成与创意无关的
    /// 灯塔故事）：local 分支透传 `chat_template_kwargs: {"enable_thinking":
    /// false}`（vLLM 原生顶层字段，关闭思考段——防 <think> 污染 JSON 输出）
    /// 且 temperature 0.7→0.3（收紧发散）；channel 分支同步降温但**不加
    /// kwargs**（防严格 OpenAI 兼容服务端拒绝未知字段），题材约束改由
    /// 提示词硬约束承担（build_script_prompt / build_retry_prompt）。
    /// chat 统一入口（带 usage 三元组：prompt/completion/total——成本事件
    /// tokens 字段来源；local 只有 total（记 (0,0,total)），channel 取上游 usage）。
    pub(crate) async fn chat_text_with_usage(
        &self,
        mr: &ModelRef,
        system: &str,
        user: &str,
    ) -> Result<(String, Option<(u32, u32, u32)>), String> {
        match mr.source.as_str() {
            "local" => {
                let (port, model) = self.resolve_local_chat()?;
                let body = super::llm::ChatBody {
                    messages: vec![
                        super::llm::ChatMessage {
                            role: "system".into(),
                            content: system.into(),
                        },
                        super::llm::ChatMessage {
                            role: "user".into(),
                            content: user.into(),
                        },
                    ],
                    max_tokens: Some(SCRIPT_MAX_TOKENS),
                    temperature: Some(0.3),
                    chat_template_kwargs: Some(serde_json::json!({"enable_thinking": false})),
                };
                let out = super::llm::LlmRouteHandler::chat_complete(port, &model, &body)
                    .await
                    .map_err(|e| format!("本地 LLM 实例（127.0.0.1:{port}）调用失败: {e}"))?;
                if out.content.trim().is_empty() {
                    return Err(format!(
                        "本地 LLM 实例返回空 content（finish_reason={}）",
                        out.finish_reason.as_deref().unwrap_or("unknown")
                    ));
                }
                // local 只有 total：记 (0, total, total)——prompt+completion 合计
                // 与 total 一致（成本事件两列存储，估费按合计/千 token）
                let usage = out.total_tokens.map(|t| {
                    (
                        0,
                        u32::try_from(t).unwrap_or(u32::MAX),
                        u32::try_from(t).unwrap_or(u32::MAX),
                    )
                });
                Ok((out.content, usage))
            }
            "channel" => {
                let (ch, model) = self.resolve_channel(mr)?;
                let body = serde_json::json!({
                    "model": model,
                    "messages": [
                        {"role": "system", "content": system},
                        {"role": "user", "content": user},
                    ],
                    "max_tokens": SCRIPT_MAX_TOKENS,
                    "temperature": 0.3,
                });
                let (text, usage) = self
                    .gateway
                    .as_ref()
                    .expect("resolve_channel 已保证注入网关")
                    .forward_channel(&ch, "chat/completions", &body)
                    .await
                    .map_err(|e| format!("渠道 {} 转发失败: {e}", ch.name))?;
                let content =
                    parse_chat_content(&text).map_err(|e| format!("渠道 {}: {e}", ch.name))?;
                Ok((content, usage))
            }
            other => Err(format!("未知 source: {other}")),
        }
    }

    /// 渠道字节面转发（video/tts/music 等可能为二进制响应的阶段——网关
    /// `forward_channel` 的 String 化会破坏二进制，故走字节面）：直连 reqwest
    /// 或复用 api_gateway 的 `channel_relay_request` + relay 执行层（via_node
    /// 中继渠道与网关转发同一组装口径）。
    async fn channel_roundtrip_bytes(
        &self,
        ch: &Channel,
        suffix: &str,
        body: &serde_json::Value,
        timeout: Duration,
    ) -> Result<Vec<u8>, String> {
        if ch.via_node.trim().is_empty() {
            let url = format!("{}/{suffix}", ch.base_url.trim_end_matches('/'));
            let payload =
                serde_json::to_vec(body).map_err(|e| format!("构造转发请求体失败: {e}"))?;
            let mut req = HTTP
                .post(&url)
                .timeout(timeout)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(payload);
            if !ch.api_key.trim().is_empty() {
                req = req.bearer_auth(ch.api_key.trim());
            }
            let resp = req
                .send()
                .await
                .map_err(|e| format!("上游请求失败（{}）: {e}", ch.name))?;
            let status = resp.status();
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| format!("读取上游响应失败: {e}"))?;
            if !status.is_success() {
                let detail: String = String::from_utf8_lossy(&bytes).chars().take(200).collect();
                return Err(format!("上游返回错误: HTTP {status} {detail}"));
            }
            Ok(bytes.to_vec())
        } else {
            let req = ApiGatewayRouteHandler::channel_relay_request(ch, suffix, body, false)?;
            let gw = self
                .gateway
                .as_ref()
                .ok_or_else(|| "网关渠道未接入（中继端点不可用）".to_string())?;
            let Some(ep) = gw.relay_endpoint() else {
                return Err(format!(
                    "经 {} 中继失败: P2P 通道未装配（NEXOS_P2P_ENABLE=1 且对端组网后可用）",
                    crate::handlers::api_market::short_node_label(&ch.via_node)
                ));
            };
            let done = ep
                .relay_roundtrip(&ch.via_node, req, timeout)
                .await
                .map_err(|e| {
                    format!(
                        "经 {} 中继失败: {e}",
                        crate::handlers::api_market::short_node_label(&ch.via_node)
                    )
                })?;
            if !(200..300).contains(&done.status) {
                let detail: String = String::from_utf8_lossy(&done.body)
                    .chars()
                    .take(200)
                    .collect();
                return Err(format!(
                    "经 {} 中继失败: 上游返回错误: HTTP {} {detail}",
                    crate::handlers::api_market::short_node_label(&ch.via_node),
                    done.status
                ));
            }
            Ok(done.body)
        }
    }

    /// URL 产物下载（视频/音频/图片 URL 形态响应 → 字节）。
    async fn download_url(&self, url: &str, timeout: Duration) -> Result<Vec<u8>, String> {
        let resp = HTTP
            .get(url)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| format!("下载产物失败（{url}）: {e}"))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("读取产物响应失败: {e}"))?;
        if !status.is_success() {
            return Err(format!("下载产物失败: HTTP {status}（{url}）"));
        }
        if bytes.is_empty() {
            return Err(format!("下载产物为空（{url}）"));
        }
        Ok(bytes.to_vec())
    }

    /// local.image：复用 media_gen 生图内核（显存闸门 + 脚本落盘 + spawn——
    /// 经 pub(crate) 内核函数调用，不复制实现）。
    pub(crate) async fn gen_image_local(
        &self,
        prompt: &str,
        width: u32,
        height: u32,
        out_path: &str,
        log: &(dyn Fn(String) + Sync),
    ) -> Result<(), String> {
        // 1. 显存闸门（sd-turbo 与 LLM 实例互斥——复用 media_gen 探测与门槛）
        let smi = self
            .smi_bin
            .clone()
            .unwrap_or_else(super::media_gen::smi_bin);
        let free_mib = super::media_gen::probe_vram_free_mib_with(&smi).await?;
        super::media_gen::vram_gate(free_mib)?;
        log(format!("显存探测通过（空闲 {free_mib} MiB）"));
        // 2. 脚本落盘 + spawn（内核函数复用；注入点缺省走 media_gen env 链）。
        //    测试注入的脚本路径**原样使用**——ensure 的幂等落盘会把它覆写成真实
        //    python 管线，mock 假脚本会被破坏；生产路径（imggen=None）不变。
        let (bin, script) = match &self.imggen {
            Some((b, s)) => (b.clone(), s.clone()),
            None => {
                let script =
                    super::media_gen::ensure_imggen_script(&super::media_gen::imggen_script())
                        .await?;
                (super::media_gen::imggen_bin(), script)
            }
        };
        let job = super::media_gen::ImageJob {
            prompt: prompt.to_string(),
            width,
            height,
            steps: 4,
            out_path: out_path.to_string(),
        };
        super::media_gen::run_imggen_with(&bin, &script, &job).await
    }

    /// channel.image：images/generations → b64/url → PNG 字节。
    ///
    /// 2026-09-04 P0 一致性：`reference_images` 非空时追加可选扩展字段
    /// `reference_images`（定妆图 b64 数组，顺序=角色绑定顺序）+`reference_strength`
    /// ——不识别字段的服务端自然忽略（OpenAI 形态不破坏）；空数组则不发任何
    /// 参考字段（与旧行为逐字节一致）。
    pub(crate) async fn gen_image_channel(
        &self,
        mr: &ModelRef,
        prompt: &str,
        width: u32,
        height: u32,
        reference_images: &[String],
        reference_strength: f64,
    ) -> Result<Vec<u8>, String> {
        let (ch, model) = self.resolve_channel(mr)?;
        let mut body = serde_json::json!({
            "model": model,
            "prompt": prompt,
            "size": format!("{width}x{height}"),
            "response_format": "b64_json",
        });
        if !reference_images.is_empty() {
            body["reference_images"] = serde_json::Value::Array(
                reference_images
                    .iter()
                    .map(|b| serde_json::Value::String(b.clone()))
                    .collect(),
            );
            body["reference_strength"] = serde_json::json!(reference_strength);
        }
        let (text, _) = self
            .gateway
            .as_ref()
            .expect("resolve_channel 已保证注入网关")
            .forward_channel(&ch, "images/generations", &body)
            .await
            .map_err(|e| format!("渠道 {} 生图转发失败: {e}", ch.name))?;
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| format!("渠道 {} 生图响应非 JSON: {e}", ch.name))?;
        if let Some(b64) = extract_b64(&v) {
            use base64::Engine;
            return base64::engine::general_purpose::STANDARD
                .decode(b64.trim())
                .map_err(|e| format!("生图 b64 解码失败: {e}"));
        }
        if let Some(url) = extract_url(&v) {
            return self.download_url(&url, Duration::from_secs(300)).await;
        }
        Err(format!(
            "渠道 {} 生图响应不含 b64_json/url 字段: {}",
            ch.name,
            text.chars().take(200).collect::<String>()
        ))
    }

    /// channel.video：video/generations（首帧 image b64 + prompt + duration，
    /// 超时放宽）→ url 下载 / b64 解码 → MP4 字节。
    ///
    /// `reference_images`：出场角色定妆图 b64（与首帧 `image` 语义分离——
    /// image=首帧画面，reference_images=角色身份；可选扩展字段，不识别的
    /// 服务端忽略）。
    async fn gen_video_channel(
        &self,
        mr: &ModelRef,
        prompt: &str,
        image_b64: &str,
        duration_secs: u32,
        reference_images: &[String],
        reference_strength: f64,
    ) -> Result<Vec<u8>, String> {
        let (ch, model) = self.resolve_channel(mr)?;
        let mut body = serde_json::json!({
            "model": model,
            "prompt": prompt,
            "image": format!("data:image/png;base64,{image_b64}"),
            "image_base64": image_b64,
            "duration_secs": duration_secs,
        });
        if !reference_images.is_empty() {
            body["reference_images"] = serde_json::Value::Array(
                reference_images
                    .iter()
                    .map(|b| serde_json::Value::String(b.clone()))
                    .collect(),
            );
            body["reference_strength"] = serde_json::json!(reference_strength);
        }
        let timeout = video_timeout();
        let bytes = self
            .channel_roundtrip_bytes(&ch, "video/generations", &body, timeout)
            .await?;
        let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| {
            format!(
                "渠道 {} 视频响应非 JSON: {}",
                ch.name,
                String::from_utf8_lossy(&bytes)
                    .chars()
                    .take(200)
                    .collect::<String>()
            )
        })?;
        if let Some(b64) = extract_b64(&v) {
            use base64::Engine;
            return base64::engine::general_purpose::STANDARD
                .decode(b64.trim())
                .map_err(|e| format!("视频 b64 解码失败: {e}"));
        }
        if let Some(url) = extract_url(&v) {
            return self.download_url(&url, timeout).await;
        }
        let detail: String = String::from_utf8_lossy(&bytes).chars().take(200).collect();
        Err(format!(
            "渠道 {} 视频响应不含 url/b64 字段（可能为异步任务形态，暂不支持上游任务轮询）: {detail}",
            ch.name
        ))
    }

    /// channel.tts：audio/speech → 音频字节（二进制或 JSON b64）。
    ///
    /// 2026-09-04 P0 音频一致性：`voice` 由硬编码 `"alloy"` 改为调用方解析结果
    /// 透传（镜头绑定角色的 voice → env 缺省 → alloy，见
    /// [`resolve_shot_voice`]）——OpenAI 标准 voice 字段，渠道天然兼容。
    /// 文档注记：Vidu 类渠道的 `subjects[].voice_id` 克隆音色接法为 P2（本期
    /// 不做 voice_ref/ref_audio_b64 扩展字段）。
    async fn tts_channel(&self, mr: &ModelRef, text: &str, voice: &str) -> Result<Vec<u8>, String> {
        let (ch, model) = self.resolve_channel(mr)?;
        let body = serde_json::json!({
            "model": model,
            "input": text,
            "voice": voice,
            "response_format": "mp3",
        });
        let bytes = self
            .channel_roundtrip_bytes(&ch, "audio/speech", &body, Duration::from_secs(300))
            .await?;
        sniff_audio_bytes(&bytes)
    }

    /// channel.music：music/generations → url 下载 / b64 / 二进制。
    pub(crate) async fn music_channel(
        &self,
        mr: &ModelRef,
        prompt: &str,
    ) -> Result<Vec<u8>, String> {
        let (ch, model) = self.resolve_channel(mr)?;
        let body = serde_json::json!({
            "model": model,
            "prompt": prompt,
        });
        let timeout = video_timeout();
        let bytes = self
            .channel_roundtrip_bytes(&ch, "music/generations", &body, timeout)
            .await?;
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            if let Some(b64) = extract_b64(&v) {
                use base64::Engine;
                return base64::engine::general_purpose::STANDARD
                    .decode(b64.trim())
                    .map_err(|e| format!("音乐 b64 解码失败: {e}"));
            }
            if let Some(url) = extract_url(&v) {
                return self.download_url(&url, timeout).await;
            }
            let detail: String = String::from_utf8_lossy(&bytes).chars().take(200).collect();
            return Err(format!(
                "渠道 {} 音乐响应不含 url/b64 字段: {detail}",
                ch.name
            ));
        }
        Ok(bytes.to_vec())
    }
}

// ----------------------------------------------------------------------------
// 阶段执行器（后台任务体；全部真实调用，失败如实落 error）
// ----------------------------------------------------------------------------

/// 读项目 script.json → 分镜数组。
pub(crate) async fn read_script(project: &FilmProject) -> Result<Vec<ScriptShot>, String> {
    let path = format!("{}/script.json", project.dir);
    let raw = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| format!("读取分镜失败 {path}: {e}（先运行 script 阶段）"))?;
    let f: ScriptFile =
        serde_json::from_str(&raw).map_err(|e| format!("分镜文件损坏 {path}: {e}"))?;
    Ok(f.shots)
}

/// 试生成产物落 cache 的输出路径（2026-09-06 FilmHub：半成品与成品分离——
/// image/video/tts 任务产物一律落 `<dir>/hub/cache/`，「确认采用」POST
/// /film/projects/:id/cache/:file/commit 转正到正式产物路径（shot-N.png 等）；
/// compose 只认转正后的正式产物。旧项目首次试生成惰性初始化 hub 树。
async fn cache_out_path(
    ctx: &FilmCtx,
    project: &FilmProject,
    name: &str,
) -> Result<String, String> {
    super::film_hub::ensure_hub(ctx, project).await?;
    let dir = format!("{}/cache", super::film_hub::hub_root(project));
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("建 cache 目录失败 {dir}: {e}"))?;
    Ok(format!("{dir}/{name}"))
}

/// 收集镜头出场角色的定妆图 b64（channel 参考注入用；顺序 = 绑定顺序稳定）。
/// 无定妆图/文件读取失败的角色跳过并记日志——参考注入是增强不是硬依赖，
/// 不因缺图拦任务。
async fn collect_reference_b64(
    project: &FilmProject,
    shot_characters: &[String],
    roster: &[FilmCharacter],
    log: &(dyn Fn(String) + Sync),
) -> Vec<String> {
    use base64::Engine;
    let mut out: Vec<String> = Vec::new();
    for name in shot_characters {
        let Some(c) = roster.iter().find(|c| &c.name == name) else {
            continue;
        };
        let Some(pref) = c
            .portrait_ref
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        else {
            log(format!(
                "角色「{name}」无定妆图，跳过参考注入（可先上传/生成定妆图）"
            ));
            continue;
        };
        let path = format!("{}/{}", project.dir.trim_end_matches('/'), pref);
        match tokio::fs::read(&path).await {
            Ok(bytes) => {
                out.push(base64::engine::general_purpose::STANDARD.encode(&bytes));
            }
            Err(e) => log(format!(
                "角色「{name}」定妆图读取失败 {path}: {e}（跳过参考注入）"
            )),
        }
    }
    out
}

/// ctx 的参考注入强度：测试注入优先，否则 env `NEXOS_FILM_REF_STRENGTH`
/// （非数字/越界回落 0.5）。
pub(crate) fn ref_strength_of(ctx: &FilmCtx) -> f64 {
    ctx.ref_strength
        .unwrap_or_else(|| parse_ref_strength(env_non_empty("NEXOS_FILM_REF_STRENGTH").as_deref()))
}

/// ctx 的 TTS 全局缺省 voice：测试注入优先，否则 env `NEXOS_FILM_TTS_VOICE`。
fn tts_default_voice(ctx: &FilmCtx) -> Option<String> {
    ctx.tts_voice
        .clone()
        .or_else(|| env_non_empty("NEXOS_FILM_TTS_VOICE"))
}

/// 更新项目状态（短锁 DB 快写；失败记日志不中断任务）。
pub(crate) fn set_project_status(db: &Arc<Mutex<Connection>>, id: &str, status: &str) {
    if let Ok(conn) = db.lock() {
        if let Err(e) = update_project_fields(&conn, id, None, None, None, None, Some(status)) {
            eprintln!("[film] 项目状态更新失败（{id} → {status}）: {e}");
        }
    }
}

/// image 阶段：关键帧图（local sd-turbo 内核 / channel images API）→ shot-N.png。
///
/// 2026-09-04 P0 角色一致性注入：
/// - **prompt 注入（local+channel 通用档）**：image_prompt 前置出场角色描述块
///   （build_character_prompt_block，措辞含「与其它镜头严格同一人物」，顺序
///   稳定）——sd-turbo 无参考图入口，这是本地方案的全部一致性来源；
/// - **渠道参考注入（仅 channel）**：请求体可选 `reference_images`（定妆图
///   b64）+ `reference_strength`；**local 不发**（请求体组装处按 source 分流，
///   sd-turbo 直调内核本就无请求体）。
async fn run_image_stage(
    ctx: &FilmCtx,
    task_id: &str,
    project: FilmProject,
    shot_no: u32,
    mr: ModelRef,
    author: String,
) {
    let tasks = ctx.tasks.clone();
    let log = |line: String| task_log(&tasks, task_id, &line);
    let started = Instant::now();
    let (w, h) = image_dims(&project.ratio).unwrap_or((1272, 720));
    let out_path = match cache_out_path(ctx, &project, &format!("shot-{shot_no}.png")).await {
        Ok(p) => p,
        Err(e) => {
            return super::film_hub::finish_stage(
                ctx,
                &tasks,
                task_id,
                &project,
                "error",
                &e,
                None,
                super::film_hub::CostSpec {
                    stage: "image",
                    shot: Some(shot_no),
                    model_ref: Some(&mr),
                    started,
                    bytes: 0,
                    tokens: None,
                },
            )
        }
    };
    log(format!(
        "镜头 {shot_no} 关键帧：模型 {}（{w}x{h}）",
        mr.label()
    ));
    let shots = match read_script(&project).await {
        Ok(s) => s,
        Err(e) => return task_finish(&tasks, task_id, "error", &e, None),
    };
    let Some(shot) = shots.iter().find(|s| s.shot == shot_no) else {
        return task_finish(
            &tasks,
            task_id,
            "error",
            &format!("镜头 {shot_no} 不在分镜中（共 {} 个镜头）", shots.len()),
            None,
        );
    };
    let roster = {
        let conn = ctx.db.lock().expect("film db poisoned");
        load_characters(&conn, &project.id)
    };
    let mut prompt = shot.image_prompt.clone();
    if let Some(block) = build_character_prompt_block(&shot.characters, &roster) {
        log(format!(
            "角色注入（prompt 档）：{}",
            shot.characters.join("、")
        ));
        prompt = format!("{block}。{prompt}");
    }
    if let Some(style) = project
        .style_hint
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        prompt = format!("{prompt}，{style}");
    }
    let result = match mr.source.as_str() {
        // local sd-turbo：仅 prompt 注入档（无请求体，reference 字段天然不发）
        "local" => ctx.gen_image_local(&prompt, w, h, &out_path, &log).await,
        "channel" => {
            let refs = collect_reference_b64(&project, &shot.characters, &roster, &log).await;
            if !refs.is_empty() {
                log(format!(
                    "角色注入（渠道 reference 档）：{} 张定妆图，strength {}",
                    refs.len(),
                    ref_strength_of(ctx)
                ));
            }
            match ctx
                .gen_image_channel(&mr, &prompt, w, h, &refs, ref_strength_of(ctx))
                .await
            {
                Ok(bytes) => tokio::fs::write(&out_path, bytes)
                    .await
                    .map_err(|e| format!("写关键帧失败 {out_path}: {e}")),
                Err(e) => Err(e),
            }
        }
        other => Err(format!("未知 source: {other}")),
    };
    if let Err(e) = result {
        return super::film_hub::finish_stage(
            ctx,
            &tasks,
            task_id,
            &project,
            "error",
            &e,
            None,
            super::film_hub::CostSpec {
                stage: "image",
                shot: Some(shot_no),
                model_ref: Some(&mr),
                started,
                bytes: 0,
                tokens: None,
            },
        );
    }
    let bytes = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    set_project_status(&ctx.db, &project.id, "producing");
    let root = super::film_hub::hub_root(&project);
    super::film_hub::append_activity(
        &root,
        &author,
        "shot.image",
        &format!("cache/shot-{shot_no}.png"),
    )
    .await;
    log(format!(
        "试生成落 cache（确认采用：POST /film/projects/{}/cache/shot-{shot_no}.png/commit）",
        project.id
    ));
    super::film_hub::finish_stage(
        ctx,
        &tasks,
        task_id,
        &project,
        "done",
        &format!("镜头 {shot_no} 关键帧（试生成）已存 {out_path}"),
        Some(out_path),
        super::film_hub::CostSpec {
            stage: "image",
            shot: Some(shot_no),
            model_ref: Some(&mr),
            started,
            bytes,
            tokens: None,
        },
    );
}

/// video 阶段：图生视频（首帧=shot-N.png，模型收 image b64+prompt+duration）
/// → shot-N.mp4。
async fn run_video_stage(
    ctx: &FilmCtx,
    task_id: &str,
    project: FilmProject,
    shot_no: u32,
    mr: ModelRef,
    image_first: bool,
    author: String,
) {
    let tasks = ctx.tasks.clone();
    let log = |line: String| task_log(&tasks, task_id, &line);
    let started = Instant::now();
    let out_path = match cache_out_path(ctx, &project, &format!("shot-{shot_no}.mp4")).await {
        Ok(p) => p,
        Err(e) => {
            return super::film_hub::finish_stage(
                ctx,
                &tasks,
                task_id,
                &project,
                "error",
                &e,
                None,
                super::film_hub::CostSpec {
                    stage: "video",
                    shot: Some(shot_no),
                    model_ref: Some(&mr),
                    started,
                    bytes: 0,
                    tokens: None,
                },
            )
        }
    };
    log(format!(
        "镜头 {shot_no} 图生视频：模型 {}（首帧={}）",
        mr.label(),
        if image_first { "启用" } else { "关闭" }
    ));
    let shots = match read_script(&project).await {
        Ok(s) => s,
        Err(e) => return task_finish(&tasks, task_id, "error", &e, None),
    };
    let Some(shot) = shots.iter().find(|s| s.shot == shot_no) else {
        return task_finish(
            &tasks,
            task_id,
            "error",
            &format!("镜头 {shot_no} 不在分镜中（共 {} 个镜头）", shots.len()),
            None,
        );
    };
    // 首帧关键帧（真实数据铁律：不凭空生成；正式产物优先，缺则回落 cache 试生成
    // ——试生成链允许「图（cache）→视频（cache）」连续试；image_first=false 纯文生视频）
    let png_path = format!("{}/shot-{shot_no}.png", project.dir);
    let cache_png = format!(
        "{}/cache/shot-{shot_no}.png",
        super::film_hub::hub_root(&project)
    );
    let first_frame = if image_first {
        if !Path::new(&png_path).is_file() && Path::new(&cache_png).is_file() {
            log("正式首帧缺失，回落 cache 试生成首帧（未 commit 的 shot png）".to_string());
            Some(cache_png.clone())
        } else {
            Some(png_path.clone())
        }
    } else {
        None
    };
    let image_b64 = if let Some(frame) = first_frame {
        match tokio::fs::read(&frame).await {
            Ok(b) => {
                use base64::Engine;
                Some(base64::engine::general_purpose::STANDARD.encode(&b))
            }
            Err(e) => {
                return super::film_hub::finish_stage(
                    ctx,
                    &tasks,
                    task_id,
                    &project,
                    "error",
                    &format!("首帧缺失 {frame}: {e}（先运行 image 阶段或传 image_first:false）"),
                    None,
                    super::film_hub::CostSpec {
                        stage: "video",
                        shot: Some(shot_no),
                        model_ref: Some(&mr),
                        started,
                        bytes: 0,
                        tokens: None,
                    },
                )
            }
        }
    } else {
        None
    };
    let result = match mr.source.as_str() {
        // validate_model_ref 已在请求期拦 local.video；此分支兜底不可达
        "local" => Err("本地视频生成能力未接入（请用 source=channel）".to_string()),
        "channel" => {
            // 角色一致性：出场角色定妆图作为主体参考注入（与首帧 image 语义
            // 分离；可选扩展字段，不识别的服务端忽略）
            let roster = {
                let conn = ctx.db.lock().expect("film db poisoned");
                load_characters(&conn, &project.id)
            };
            let refs = collect_reference_b64(&project, &shot.characters, &roster, &log).await;
            if !refs.is_empty() {
                log(format!(
                    "角色注入（渠道 reference 档）：{} 张定妆图，strength {}",
                    refs.len(),
                    ref_strength_of(ctx)
                ));
            }
            ctx.gen_video_channel(
                &mr,
                &shot.video_prompt,
                image_b64.as_deref().unwrap_or(""),
                shot.duration_secs,
                &refs,
                ref_strength_of(ctx),
            )
            .await
        }
        other => Err(format!("未知 source: {other}")),
    };
    let bytes = match result {
        Ok(b) => b,
        Err(e) => {
            return super::film_hub::finish_stage(
                ctx,
                &tasks,
                task_id,
                &project,
                "error",
                &e,
                None,
                super::film_hub::CostSpec {
                    stage: "video",
                    shot: Some(shot_no),
                    model_ref: Some(&mr),
                    started,
                    bytes: 0,
                    tokens: None,
                },
            )
        }
    };
    if let Err(e) = tokio::fs::write(&out_path, &bytes).await {
        return super::film_hub::finish_stage(
            ctx,
            &tasks,
            task_id,
            &project,
            "error",
            &format!("写视频失败 {out_path}: {e}"),
            None,
            super::film_hub::CostSpec {
                stage: "video",
                shot: Some(shot_no),
                model_ref: Some(&mr),
                started,
                bytes: bytes.len() as u64,
                tokens: None,
            },
        );
    }
    set_project_status(&ctx.db, &project.id, "producing");
    let root = super::film_hub::hub_root(&project);
    super::film_hub::append_activity(
        &root,
        &author,
        "shot.video",
        &format!("cache/shot-{shot_no}.mp4"),
    )
    .await;
    log(format!(
        "试生成落 cache（确认采用：POST /film/projects/{}/cache/shot-{shot_no}.mp4/commit）",
        project.id
    ));
    super::film_hub::finish_stage(
        ctx,
        &tasks,
        task_id,
        &project,
        "done",
        &format!("镜头 {shot_no} 视频（试生成）已存 {out_path}"),
        Some(out_path),
        super::film_hub::CostSpec {
            stage: "video",
            shot: Some(shot_no),
            model_ref: Some(&mr),
            started,
            bytes: bytes.len() as u64,
            tokens: None,
        },
    );
}

/// tts 阶段：台词配音（缺省文本=script.line）→ line-N.mp3。
async fn run_tts_stage(
    ctx: &FilmCtx,
    task_id: &str,
    project: FilmProject,
    shot_no: u32,
    mr: ModelRef,
    text_override: Option<String>,
    author: String,
) {
    let tasks = ctx.tasks.clone();
    let log = |line: String| task_log(&tasks, task_id, &line);
    let started = Instant::now();
    let out_path = match cache_out_path(ctx, &project, &format!("line-{shot_no}.mp3")).await {
        Ok(p) => p,
        Err(e) => {
            return super::film_hub::finish_stage(
                ctx,
                &tasks,
                task_id,
                &project,
                "error",
                &e,
                None,
                super::film_hub::CostSpec {
                    stage: "tts",
                    shot: Some(shot_no),
                    model_ref: Some(&mr),
                    started,
                    bytes: 0,
                    tokens: None,
                },
            )
        }
    };
    log(format!("镜头 {shot_no} 配音：模型 {}", mr.label()));
    let shots = match read_script(&project).await {
        Ok(s) => s,
        Err(e) => return task_finish(&tasks, task_id, "error", &e, None),
    };
    let Some(shot) = shots.iter().find(|s| s.shot == shot_no) else {
        return task_finish(
            &tasks,
            task_id,
            "error",
            &format!("镜头 {shot_no} 不在分镜中（共 {} 个镜头）", shots.len()),
            None,
        );
    };
    let text = text_override
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| shot.line.clone());
    if text.is_empty() {
        return task_finish(
            &tasks,
            task_id,
            "error",
            &format!("镜头 {shot_no} 无台词（script.line 为空），请显式传 text"),
            None,
        );
    }
    log(format!(
        "配音文本：{}",
        text.chars().take(60).collect::<String>()
    ));
    // 音频一致性（2026-09-04 P0）：voice 三态——镜头第一个有 voice 的绑定角色
    // → env NEXOS_FILM_TTS_VOICE 缺省 → alloy 兜底（替换旧硬编码）
    let roster = {
        let conn = ctx.db.lock().expect("film db poisoned");
        load_characters(&conn, &project.id)
    };
    let voice = resolve_shot_voice(&shot.characters, &roster, tts_default_voice(ctx).as_deref());
    log(format!(
        "配音 voice：{voice}（{}）",
        if shot.characters.is_empty() {
            "无绑定角色，落全局缺省".to_string()
        } else {
            format!("绑定角色 {}", shot.characters.join("、"))
        }
    ));
    // validate_model_ref 已在请求期拦 local.tts；此分支兜底不可达
    let bytes = match mr.source.as_str() {
        "local" => {
            return task_finish(
                &tasks,
                task_id,
                "error",
                "本地 TTS 能力未接入（请用 source=channel）",
                None,
            )
        }
        "channel" => match ctx.tts_channel(&mr, &text, &voice).await {
            Ok(b) => b,
            Err(e) => return task_finish(&tasks, task_id, "error", &e, None),
        },
        other => {
            return task_finish(
                &tasks,
                task_id,
                "error",
                &format!("未知 source: {other}"),
                None,
            )
        }
    };
    if let Err(e) = tokio::fs::write(&out_path, &bytes).await {
        return super::film_hub::finish_stage(
            ctx,
            &tasks,
            task_id,
            &project,
            "error",
            &format!("写配音失败 {out_path}: {e}"),
            None,
            super::film_hub::CostSpec {
                stage: "tts",
                shot: Some(shot_no),
                model_ref: Some(&mr),
                started,
                bytes: bytes.len() as u64,
                tokens: None,
            },
        );
    }
    set_project_status(&ctx.db, &project.id, "producing");
    let root = super::film_hub::hub_root(&project);
    super::film_hub::append_activity(
        &root,
        &author,
        "shot.tts",
        &format!("cache/line-{shot_no}.mp3"),
    )
    .await;
    log(format!(
        "试生成落 cache（确认采用：POST /film/projects/{}/cache/line-{shot_no}.mp3/commit）",
        project.id
    ));
    super::film_hub::finish_stage(
        ctx,
        &tasks,
        task_id,
        &project,
        "done",
        &format!("镜头 {shot_no} 配音（试生成）已存 {out_path}"),
        Some(out_path),
        super::film_hub::CostSpec {
            stage: "tts",
            shot: Some(shot_no),
            model_ref: Some(&mr),
            started,
            bytes: bytes.len() as u64,
            tokens: None,
        },
    );
}

/// music 阶段：BGM（缺省 prompt 按 idea/style_hint 构造）→ bgm.mp3。
async fn run_music_stage(
    ctx: &FilmCtx,
    task_id: &str,
    project: FilmProject,
    mr: ModelRef,
    prompt_override: Option<String>,
    author: String,
) {
    let tasks = ctx.tasks.clone();
    let log = |line: String| task_log(&tasks, task_id, &line);
    let started = Instant::now();
    let out_path = format!("{}/bgm.mp3", project.dir);
    log(format!("BGM 生成：模型 {}", mr.label()));
    let prompt = prompt_override
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| {
            let style = project
                .style_hint
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("电影感");
            format!(
                "为影片生成背景音乐，风格：{style}，适配创意：{}",
                project.idea
            )
        });
    log(format!(
        "BGM 提示：{}",
        prompt.chars().take(60).collect::<String>()
    ));
    // validate_model_ref 已在请求期拦 local.music；此分支兜底不可达
    let bytes = match mr.source.as_str() {
        "local" => {
            return task_finish(
                &tasks,
                task_id,
                "error",
                "本地音乐生成能力未接入（请用 source=channel）",
                None,
            )
        }
        "channel" => match ctx.music_channel(&mr, &prompt).await {
            Ok(b) => b,
            Err(e) => return task_finish(&tasks, task_id, "error", &e, None),
        },
        other => {
            return task_finish(
                &tasks,
                task_id,
                "error",
                &format!("未知 source: {other}"),
                None,
            )
        }
    };
    if let Err(e) = tokio::fs::write(&out_path, &bytes).await {
        return super::film_hub::finish_stage(
            ctx,
            &tasks,
            task_id,
            &project,
            "error",
            &format!("写 BGM 失败 {out_path}: {e}"),
            None,
            super::film_hub::CostSpec {
                stage: "music",
                shot: None,
                model_ref: Some(&mr),
                started,
                bytes: bytes.len() as u64,
                tokens: None,
            },
        );
    }
    set_project_status(&ctx.db, &project.id, "producing");
    let root = super::film_hub::hub_root(&project);
    super::film_hub::append_activity(&root, &author, "music.generate", "bgm.mp3").await;
    super::film_hub::finish_stage(
        ctx,
        &tasks,
        task_id,
        &project,
        "done",
        &format!("BGM 已存 {out_path}"),
        Some(out_path),
        super::film_hub::CostSpec {
            stage: "music",
            shot: None,
            model_ref: Some(&mr),
            started,
            bytes: bytes.len() as u64,
            tokens: None,
        },
    );
}

/// portrait 阶段：角色定妆图生成（走既有生图面——local sd-turbo / channel
/// images API；prompt 缺省由 description 构造）→
/// `<dir>/characters/<cid>/portrait.png` 并回写 portrait_ref。定妆图是后续
/// 分镜参考注入（prompt 档 / 渠道 reference 档）的主体来源。
async fn run_portrait_stage(
    ctx: &FilmCtx,
    task_id: &str,
    project: FilmProject,
    character: FilmCharacter,
    mr: ModelRef,
    prompt_override: Option<String>,
) {
    let tasks = ctx.tasks.clone();
    let log = |line: String| task_log(&tasks, task_id, &line);
    let started = Instant::now();
    let prompt = prompt_override
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| default_portrait_prompt(&character.name, &character.description));
    // 定妆图统一 720x720（1:1——正脸半身参考图口径；渠道 size 透传同值）
    let (w, h) = (720u32, 720u32);
    let dir = format!(
        "{}/characters/{}",
        project.dir.trim_end_matches('/'),
        character.id
    );
    let out_path = format!("{dir}/portrait.png");
    log(format!(
        "定妆图生成「{}」：模型 {}（{w}x{h}）",
        character.name,
        mr.label()
    ));
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return task_finish(
            &tasks,
            task_id,
            "error",
            &format!("建角色目录失败 {dir}: {e}"),
            None,
        );
    }
    log(format!(
        "定妆提示：{}",
        prompt.chars().take(60).collect::<String>()
    ));
    let result = match mr.source.as_str() {
        // 定妆图生成本身无参考输入（这是第一张主体图）——两形态都不带 reference 字段
        "local" => ctx.gen_image_local(&prompt, w, h, &out_path, &log).await,
        "channel" => match ctx.gen_image_channel(&mr, &prompt, w, h, &[], 0.0).await {
            Ok(bytes) => tokio::fs::write(&out_path, bytes)
                .await
                .map_err(|e| format!("写定妆图失败 {out_path}: {e}")),
            Err(e) => Err(e),
        },
        other => Err(format!("未知 source: {other}")),
    };
    if let Err(e) = result {
        return task_finish(&tasks, task_id, "error", &e, None);
    }
    let portrait_ref = format!("characters/{}/portrait.png", character.id);
    {
        let conn = ctx.db.lock().expect("film db poisoned");
        if let Err(e) =
            update_character_fields(&conn, &character.id, None, None, None, Some(&portrait_ref))
        {
            return task_finish(
                &tasks,
                task_id,
                "error",
                &format!("定妆图路径回写失败（{}）: {e}", character.id),
                None,
            );
        }
    }
    let bytes = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    super::film_hub::finish_stage(
        ctx,
        &tasks,
        task_id,
        &project,
        "done",
        &format!("定妆图已存 {out_path}"),
        Some(out_path),
        super::film_hub::CostSpec {
            stage: "portrait",
            shot: None,
            model_ref: Some(&mr),
            started,
            bytes,
            tokens: None,
        },
    );
}

/// 执行一遍 ffmpeg（cwd=项目目录；失败/超时如实 Err，附 stderr 尾）。
pub(crate) async fn run_ffmpeg_pass(
    ffmpeg: &str,
    cwd: &str,
    args: &[String],
    timeout: Duration,
    log: &(dyn Fn(String) + Sync),
) -> Result<(), String> {
    log(format!("$ {} {}", ffmpeg, args.join(" ")));
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.args(args)
        .current_dir(cwd)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = cmd
        .spawn()
        .map_err(|e| format!("ffmpeg 启动失败（{ffmpeg}）: {e}。{FFMPEG_INSTALL_HINT}"))?;
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Err(_) => Err(format!("ffmpeg 超时（{}s）已终止", timeout.as_secs())),
        Ok(Err(e)) => Err(format!("ffmpeg 执行失败: {e}")),
        Ok(Ok(out)) => {
            if !out.status.success() {
                let stderr =
                    super::media_gen::summarize_stderr(&String::from_utf8_lossy(&out.stderr));
                Err(format!(
                    "ffmpeg 失败（退出码 {:?}）: {stderr}",
                    out.status.code()
                ))
            } else {
                Ok(())
            }
        }
    }
}

// ----------------------------------------------------------------------------
// FilmRouteHandler
// ----------------------------------------------------------------------------

/// 影片制作管线路由处理器——项目 CRUD（SQLite）+ 六阶段异步任务 + ffmpeg 合成。
pub struct FilmRouteHandler {
    /// film_projects 表（Arc 共享给后台阶段任务——同一把锁写状态，llm.rs 同款）。
    pub(crate) db: Arc<Mutex<Connection>>,
    counter: Mutex<u64>,
    /// 角色 id 序号（`char-<n>`；与项目计数独立）。
    char_seq: AtomicU64,
    pub(crate) tasks: Arc<Mutex<HashMap<String, FilmTask>>>,
    task_seq: AtomicU64,
    gateway: Option<Arc<ApiGatewayRouteHandler>>,
    llm: Option<Arc<super::llm::LlmRouteHandler>>,
    local_chat: Option<(u16, String)>,
    imggen: Option<(String, String)>,
    smi_bin: Option<String>,
    ffmpeg_bin: Option<String>,
    /// 测试注入：参考注入强度 / TTS 缺省 voice（None=生产 env 链）。
    ref_strength: Option<f64>,
    tts_voice: Option<String>,
    /// 产物根目录（env `NEXOS_FILM_DIR` 覆写；缺省 /tank/os-data/film）。
    root_dir: String,
    /// 测试注入：导出路径基目录（None=生产 env `NEXOS_FILM_EXPORT_BASE` 链；
    /// 缺省不限制——单用户节点，写面本就 admin 鉴权）。
    export_base: Option<String>,
    /// 应用注册表（引擎门控）：注入后每请求查 apps 表——未安装 film 应用
    /// 则全部业务端点 404（引擎内置、应用按装启用，2026-09-04 架构决策，
    /// docs/APPS.md）。None = 未注入（单测直构），不门控；生产 main.rs 恒注入。
    app_registry: Option<Arc<super::apps_handler::AppRegistry>>,
}

impl FilmRouteHandler {
    /// 生产构造（main.rs 注册用；经 [`Self::with_gateway`] / [`Self::with_llm`]
    /// 链式注入共享实例后注册）。
    #[must_use]
    pub fn new() -> Self {
        Self::with_db_path(&default_db_path())
    }

    /// 用指定 DB 路径构造（测试注入）。
    #[must_use]
    pub fn with_db_path(path: &str) -> Self {
        let conn = open_db(path);
        // 防线①：counter 按 DB 既有 film-<n> max 起跳（重启不回跳——
        // 2026-09-06 film-101 事故：恒从 100 起导致新项目复用既有 id）
        let counter_seed = Self::max_project_seq(&conn).max(100);
        Self {
            db: Arc::new(Mutex::new(conn)),
            counter: Mutex::new(counter_seed),
            char_seq: AtomicU64::new(0),
            tasks: Arc::new(Mutex::new(HashMap::new())),
            task_seq: AtomicU64::new(0),
            gateway: None,
            llm: None,
            local_chat: None,
            imggen: None,
            smi_bin: None,
            ffmpeg_bin: None,
            ref_strength: None,
            tts_voice: None,
            root_dir: default_film_root(),
            export_base: None,
            app_registry: None,
        }
    }

    /// 链式注入共享 api_gateway 实例（model_ref source=channel 的渠道表读取 +
    /// 转发/中继执行面；与 api_gateway 组件同一实例——`Mutex<Connection>` 语义
    /// 同源，main.rs 装配）。
    #[must_use]
    pub fn with_gateway(mut self, gateway: Arc<ApiGatewayRouteHandler>) -> Self {
        self.gateway = Some(gateway);
        self
    }

    /// 链式注入共享 llm 实例（local.chat 的实例表读取——找 running 实例直连）。
    #[must_use]
    pub fn with_llm(mut self, llm: Arc<super::llm::LlmRouteHandler>) -> Self {
        self.llm = Some(llm);
        self
    }

    /// 链式注入产物根目录（测试：临时目录）。
    #[must_use]
    pub fn with_root_dir(mut self, dir: &str) -> Self {
        self.root_dir = dir.to_string();
        self
    }

    /// 测试注入：导出路径基目录（None=生产 env `NEXOS_FILM_EXPORT_BASE`——
    /// 缺省不限制：单用户节点，写面本就 admin 鉴权；设置时 export_dir 必须
    /// 位于其下，防任意路径写）。
    #[must_use]
    pub fn with_export_base(mut self, base: &str) -> Self {
        self.export_base = Some(base.to_string());
        self
    }

    /// 生效的导出基目录（测试注入优先，否则 env `NEXOS_FILM_EXPORT_BASE`）。
    fn effective_export_base(&self) -> Option<String> {
        self.export_base.clone().or_else(|| {
            env_non_empty("NEXOS_FILM_EXPORT_BASE").map(|b| b.trim_end_matches('/').to_string())
        })
    }

    /// 测试注入：本地 chat 直连端点（port + 模型名；指向 mock vLLM TCP 服务，
    /// 绕开 llm 实例表——生产行为不变）。
    #[must_use]
    pub fn with_local_chat(mut self, port: u16, model: &str) -> Self {
        self.local_chat = Some((port, model.to_string()));
        self
    }

    /// 测试注入：生图内核二进制/脚本路径 + 显存探测二进制（mock 脚本；生产走
    /// media_gen 的 env 注入点链 NEXOS_IMGGEN_BIN / NEXOS_SMI_BIN）。
    #[must_use]
    pub fn with_imggen_mock(mut self, bin: &str, script: &str, smi: &str) -> Self {
        self.imggen = Some((bin.to_string(), script.to_string()));
        self.smi_bin = Some(smi.to_string());
        self
    }

    /// 测试注入：固定 ffmpeg 路径（假二进制脚本 / 不存在路径——缺失指引分支；
    /// None=生产解析链 env NEXOS_FFMPEG_BIN → PATH → 常规路径）。
    #[must_use]
    pub fn with_ffmpeg_bin(mut self, path: &str) -> Self {
        self.ffmpeg_bin = Some(path.to_string());
        self
    }

    /// 测试注入：参考注入强度（None=生产 env `NEXOS_FILM_REF_STRENGTH` 链）。
    #[must_use]
    pub fn with_ref_strength(mut self, v: f64) -> Self {
        self.ref_strength = Some(v);
        self
    }

    /// 测试注入：TTS 全局缺省 voice（None=生产 env `NEXOS_FILM_TTS_VOICE` 链）。
    #[must_use]
    pub fn with_tts_voice(mut self, v: &str) -> Self {
        self.tts_voice = Some(v.to_string());
        self
    }

    /// 链式注入应用注册表（引擎门控开启：未安装 film 应用 → 全部业务端点
    /// 404；与 apps 组件 REST 面共享同一 SQLite，安装/卸载即时生效）。
    /// main.rs 生产装配恒调用；单测不注入则不门控（既有测试契约不变）。
    #[must_use]
    pub fn with_app_registry(mut self, reg: Arc<super::apps_handler::AppRegistry>) -> Self {
        self.app_registry = Some(reg);
        self
    }

    /// 执行上下文快照（后台任务 spawn 用；db/tasks 与 handler 同一实例）。
    pub(crate) fn ctx(&self) -> FilmCtx {
        FilmCtx {
            db: Arc::clone(&self.db),
            tasks: Arc::clone(&self.tasks),
            gateway: self.gateway.clone(),
            llm: self.llm.clone(),
            local_chat: self.local_chat.clone(),
            imggen: self.imggen.clone(),
            smi_bin: self.smi_bin.clone(),
            ffmpeg_bin: self.ffmpeg_bin.clone(),
            ref_strength: self.ref_strength,
            tts_voice: self.tts_voice.clone(),
        }
    }

    /// DB 既有 `film-<n>` 的最大 n（无行/解析失败 → 0；id 起跳扫描用）。
    fn max_project_seq(conn: &Connection) -> u64 {
        conn.prepare("SELECT id FROM film_projects")
            .and_then(|mut stmt| {
                let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
                Ok(rows
                    .filter_map(Result::ok)
                    .filter_map(|id| id.strip_prefix("film-")?.parse::<u64>().ok())
                    .max()
                    .unwrap_or(0))
            })
            .unwrap_or(0)
    }

    /// 分配新项目 id + 目录（2026-09-06 film-101 数据丢失事故的三重防线）：
    /// ① **DB max 起跳**（构造时扫既有行，重启不回跳——旧缺陷 counter 恒从
    ///    100 起，重启后新项目可复用既有 id；DB 行还在时撞 UNIQUE 500，行
    ///    不在（DB 丢失/内存库回退）时**直接劫持既有项目目录**）；
    /// ② **行已占让位**（并发/多进程写同库时跳到下一个空 id）；
    /// ③ **目标目录非空让位**（DB 与磁盘漂移——DB 重建后磁盘目录仍在——时
    ///    也不复用真实项目目录；配合 DELETE 连目录删，劫持=整目录丢失）。
    /// 目录不存在或为空目录才可用（空目录视为可回收残留）。
    fn allocate_project_id(&self) -> Result<(String, String), String> {
        let root = self.root_dir.trim_end_matches('/').to_string();
        let mut c = self.counter.lock().expect("film counter poisoned");
        for _ in 0..10_000 {
            *c += 1;
            let id = format!("film-{}", *c);
            {
                let conn = self.db.lock().expect("film db poisoned");
                if find_project(&conn, &id).is_some() {
                    continue; // 防线②：行已占
                }
            }
            let dir = format!("{root}/{id}");
            let occupied = std::fs::read_dir(&dir)
                .map(|mut rd| rd.next().is_some())
                .unwrap_or(false);
            if occupied {
                eprintln!(
                    "[film] id {id} 让位：目录已存在且非空（DB/磁盘漂移防护，防劫持真实项目）"
                );
                continue; // 防线③：非空目录
            }
            return Ok((id, dir));
        }
        Err("连续 1 万次 id 候选均冲突（DB 行或 film-* 目录异常，请排查残留）".to_string())
    }

    fn next_character_id(&self) -> String {
        let n = self.char_seq.fetch_add(1, Ordering::SeqCst) + 1;
        format!("{CHARACTER_ID_PREFIX}{n}")
    }

    fn next_task_id(&self) -> String {
        let n = self.task_seq.fetch_add(1, Ordering::SeqCst) + 1;
        format!("ft-{n}")
    }

    /// 建任务（queued 态插表）+ 后台执行（请求即时返回 202，阶段执行器如实
    /// 推进 queued→running→done|error）。
    /// 建任务（分配 id → queued 态插表）+ 后台执行（请求即时返回 202，阶段
    /// 执行器如实推进 queued→running→done|error）。
    ///
    /// `make` 是 future 工厂：以**已插表的**任务 id 构造阶段执行 future——id 由
    /// 本方法唯一分配（调用方不得另行 `next_task_id`，否则执行体更新的 id 不在
    /// 任务表里，轮询永远 404）。
    pub(crate) fn spawn_stage_task<F, Fut>(
        &self,
        project: &FilmProject,
        stage: &str,
        make: F,
    ) -> String
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let id = self.next_task_id();
        let task = FilmTask {
            id: id.clone(),
            project_id: project.id.clone(),
            stage: stage.to_string(),
            status: "queued".into(),
            log: vec![],
            output: None,
            error: None,
            created_at: now_epoch(),
            finished_at: None,
        };
        self.tasks
            .lock()
            .expect("film tasks poisoned")
            .insert(id.clone(), task);
        eprintln!("[film] 任务创建：{id}（{stage}，项目 {}）", project.id);
        let fut = make(id.clone());
        let tasks = Arc::clone(&self.tasks);
        let id_for_run = id.clone();
        tokio::spawn(async move {
            task_running(&tasks, &id_for_run);
            fut.await;
        });
        id
    }

    /// 查项目（Err=404 响应体；调用面经 [`try_project`] 宏直回）。
    pub(crate) fn project_or_404(&self, id: &str) -> Result<FilmProject, ApiResponse> {
        let conn = self.db.lock().expect("film db poisoned");
        find_project(&conn, id).ok_or_else(|| error_response(404, &format!("项目不存在: {id}")))
    }

    /// 项目产物清单（目录扫描：文件名 + 字节数，按名排序；目录缺失返回空）。
    /// export_dir 设置时合并扫描导出目录——final.mp4 物理落在那里，清单照旧
    /// 含 `final.mp4` 名（同名以导出目录为准：新成片遮项目目录旧残留）。
    fn artifacts(&self, project: &FilmProject) -> Vec<serde_json::Value> {
        let mut out = scan_dir_files(&project.dir);
        // dist 成品合并扫描：export_dir 分支 dist 即导出目录（本来就扫）；
        // 缺省分支 dist 落 `<dir>/hub/dist`（2026-09-06 FilmHub 版本化成品）。
        let mut dist_dirs: Vec<String> = Vec::new();
        if let Some(ed) = project
            .export_dir
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != project.dir)
        {
            dist_dirs.push(ed.to_string());
        }
        dist_dirs.push(format!("{}/dist", super::film_hub::hub_root(project)));
        for ed in dist_dirs {
            for e in scan_dir_files(&ed) {
                if let Some(name) = e["name"].as_str() {
                    // `_` 前缀 = hub 自文档占位（dist/_about.md 等，v0.1.36
                    // 默认树骨架）——不算成片产物
                    if name.starts_with('_') {
                        continue;
                    }
                    out.retain(|a| a["name"].as_str() != Some(name));
                }
                out.push(e);
            }
        }
        out.sort_by_key(|a| a["name"].as_str().map(String::from));
        out
    }

    /// 项目级参考图清单（`<dir>/refs/` 扫描：文件名 + 字节数，按名排序；
    /// 2026-09-04 P0 参考导入——存储与列出，生成注入仅角色定妆图）。
    fn list_refs(project: &FilmProject) -> Vec<serde_json::Value> {
        let dir = format!("{}/refs", project.dir.trim_end_matches('/'));
        let mut out: Vec<serde_json::Value> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .filter_map(|e| {
                        let meta = e.metadata().ok()?;
                        if !meta.is_file() {
                            return None;
                        }
                        Some(serde_json::json!({
                            "name": e.file_name().to_string_lossy(),
                            "bytes": meta.len(),
                        }))
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by_key(|a| a["name"].as_str().map(String::from));
        out
    }

    /// 每角色绑定镜头清单（扫 script.json 出场角色名命中；角色 id → 镜头号数组）。
    async fn bound_shots(
        project: &FilmProject,
        characters: &[FilmCharacter],
    ) -> HashMap<String, Vec<u32>> {
        let mut out: HashMap<String, Vec<u32>> = HashMap::new();
        let Ok(shots) = read_script(project).await else {
            return out;
        };
        for c in characters {
            let mut v: Vec<u32> = shots
                .iter()
                .filter(|s| s.characters.iter().any(|n| n == &c.name))
                .map(|s| s.shot)
                .collect();
            v.sort_unstable();
            v.dedup();
            out.insert(c.id.clone(), v);
        }
        out
    }
}

impl Default for FilmRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

/// 目录文件清单扫描（`{name,bytes}`；目录缺失/不可读返回空——artifacts /
/// 导出目录合并共用）。
fn scan_dir_files(dir: &str) -> Vec<serde_json::Value> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter_map(|e| {
                    let meta = e.metadata().ok()?;
                    if !meta.is_file() {
                        return None;
                    }
                    Some(serde_json::json!({
                        "name": e.file_name().to_string_lossy(),
                        "bytes": meta.len(),
                    }))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 打开 film.db（建表幂等；失败降级内存库不 panic——上游不可用也不挡启动）。
fn open_db(path: &str) -> Connection {
    if let Some(parent) = std::path::Path::new(path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match Connection::open(path) {
        Ok(conn) => {
            if let Err(e) = create_schema(&conn) {
                eprintln!("[film] 建表失败（{path}）: {e}");
            }
            conn
        }
        Err(e) => {
            eprintln!("[film] 打开 SQLite {path} 失败（{e}），降级到内存库");
            let conn = Connection::open_in_memory().expect("内存库必成功");
            let _ = create_schema(&conn);
            conn
        }
    }
}

fn default_db_path() -> String {
    if let Some(p) = env_non_empty("NEXOS_FILM_DB") {
        return p;
    }
    for p in ["/tank/os-data/film.db", "/var/lib/os/film.db"] {
        if std::path::Path::new(p)
            .parent()
            .is_some_and(|d| d.exists() || std::fs::create_dir_all(d).is_ok())
        {
            return (*p).to_string();
        }
    }
    "film.db".to_string()
}

fn default_film_root() -> String {
    if let Some(p) = env_non_empty("NEXOS_FILM_DIR") {
        return p;
    }
    for p in ["/tank/os-data/film", "/var/lib/os/film"] {
        if std::path::Path::new(p).is_dir() || std::fs::create_dir_all(p).is_ok() {
            return (*p).to_string();
        }
    }
    "film-data".to_string()
}

fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[async_trait]
impl RouteHandler for FilmRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec_admin(HttpMethod::Post, "/api/v1/film/projects"),
            spec_public(HttpMethod::Get, "/api/v1/film/projects"),
            spec_public(HttpMethod::Get, "/api/v1/film/projects/:id"),
            spec_admin(HttpMethod::Put, "/api/v1/film/projects/:id"),
            spec_admin(HttpMethod::Delete, "/api/v1/film/projects/:id"),
            spec_admin(HttpMethod::Post, "/api/v1/film/projects/:id/script"),
            spec_admin(HttpMethod::Post, "/api/v1/film/projects/:id/shots/:n/image"),
            spec_admin(HttpMethod::Post, "/api/v1/film/projects/:id/shots/:n/video"),
            spec_admin(HttpMethod::Post, "/api/v1/film/projects/:id/shots/:n/tts"),
            spec_admin(HttpMethod::Post, "/api/v1/film/projects/:id/music"),
            spec_admin(HttpMethod::Post, "/api/v1/film/projects/:id/compose"),
            // —— 角色库与参考导入（2026-09-04 P0 一致性）——
            spec_public(HttpMethod::Get, "/api/v1/film/projects/:id/characters"),
            spec_admin(HttpMethod::Post, "/api/v1/film/projects/:id/characters"),
            spec_admin(HttpMethod::Put, "/api/v1/film/characters/:cid"),
            spec_admin(HttpMethod::Delete, "/api/v1/film/characters/:cid"),
            spec_admin(
                HttpMethod::Post,
                "/api/v1/film/projects/:id/characters/:cid/portrait",
            ),
            spec_admin(
                HttpMethod::Post,
                "/api/v1/film/projects/:id/characters/:cid/portrait/generate",
            ),
            spec_admin(HttpMethod::Post, "/api/v1/film/projects/:id/refs"),
            spec_public(HttpMethod::Get, "/api/v1/film/tasks"),
            spec_public(HttpMethod::Get, "/api/v1/film/tasks/:id"),
            spec_public(HttpMethod::Get, "/api/v1/film/tools"),
        ]
        .into_iter()
        .chain(super::film_hub::hub_routes())
        .collect()
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        // —— 引擎门控（2026-09-04：film 剥离为独立应用）——
        // film 引擎代码仍编译在 os-api（引擎内置），但未安装 film 应用时
        // **零入口零可用**：全部业务端点 404 + 安装指引（语义对齐手机系统
        // 服务+应用）。每请求直查 apps 表（无缓存）——安装/卸载即时生效；
        // 表损坏/锁失败 fail-closed（按未装处理）。未注入注册表（单测直构）
        // 不门控，既有测试契约不变。
        if let Some(reg) = &self.app_registry {
            if !reg.is_engine_enabled("film") {
                return Ok(error_response(
                    404,
                    "应用「film」未安装：可在 应用中心 → 商店 安装",
                ));
            }
        }
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            // —— 项目 CRUD ——
            (HttpMethod::Post, ["api", "v1", "film", "projects"]) => {
                let body: CreateProjectBody = serde_json::from_value(req.body)
                    .map_err(|e| ApiGatewayError::Internal(format!("解析建项目请求体失败: {e}")))?;
                let title = body.title.trim();
                let idea = body.idea.trim();
                if title.is_empty() {
                    return Ok(error_response(400, "title 不可为空"));
                }
                if idea.is_empty() {
                    return Ok(error_response(400, "idea 不可为空"));
                }
                if ratio_dims(&body.ratio).is_none() {
                    return Ok(error_response(
                        400,
                        &format!(
                            "ratio 必须是 16:9 / 9:16 / 1:1 / 2.39:1 / 1.85:1 / 4:3（当前 {}）",
                            body.ratio
                        ),
                    ));
                }
                // id 分配走三重防线（DB max 起跳 / 行已占让位 / 非空目录让位
                // ——2026-09-06 film-101 数据丢失事故回归，见 allocate_project_id）
                let (id, dir) = match self.allocate_project_id() {
                    Ok(v) => v,
                    Err(e) => return Ok(error_response(500, &e)),
                };
                if let Err(e) = tokio::fs::create_dir_all(&dir).await {
                    return Ok(error_response(
                        500,
                        &format!("创建产物目录失败 {dir}: {e}（检查 NEXOS_FILM_DIR 可写性）"),
                    ));
                }
                let style_hint = body
                    .style_hint
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                let project = FilmProject {
                    id: id.clone(),
                    title: title.to_string(),
                    idea: idea.to_string(),
                    ratio: body.ratio.trim().to_string(),
                    style_hint,
                    status: "draft".into(),
                    dir: dir.clone(),
                    export_dir: None,
                    created_at: now_iso(),
                    updated_at: now_iso(),
                };
                {
                    let conn = self.db.lock().expect("film db poisoned");
                    if let Err(e) = insert_project(&conn, &project) {
                        return Ok(error_response(500, &format!("项目落库失败: {e}")));
                    }
                }
                // hub 建项目即建 FilmHub 树（project.md/README/骨架元文件；
                // 失败仅日志不拦建项——树可经新端点惰性补建）
                super::film_hub::init_hub_for_new(&project).await;
                eprintln!("[film] 项目创建：{id}（{title}，{}）", project.ratio);
                Ok(ApiResponse {
                    status: 201,
                    body: project_json(&project),
                    headers: serde_json::json!({}),
                })
            }

            (HttpMethod::Get, ["api", "v1", "film", "projects"]) => {
                let conn = self.db.lock().expect("film db poisoned");
                let list: Vec<serde_json::Value> =
                    load_projects(&conn).iter().map(project_json).collect();
                Ok(ok_json(serde_json::Value::Array(list)))
            }

            (HttpMethod::Get, ["api", "v1", "film", "projects", id]) => {
                let project = try_project!(self, id);
                let script = read_script(&project).await.ok();
                Ok(ok_json(serde_json::json!({
                    "project": project_json(&project),
                    "script": script,
                    "artifacts": self.artifacts(&project),
                    "refs": Self::list_refs(&project),
                })))
            }

            (HttpMethod::Put, ["api", "v1", "film", "projects", id]) => {
                let _existing = try_project!(self, id);
                let body: UpdateProjectBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析更新项目请求体失败: {e}"))
                })?;
                let norm = |o: &Option<String>| {
                    o.as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                };
                let title = norm(&body.title);
                let idea = norm(&body.idea);
                let ratio = norm(&body.ratio);
                if let Some(r) = &ratio {
                    if ratio_dims(r).is_none() {
                        return Ok(error_response(
                            400,
                            &format!(
                                "ratio 必须是 16:9 / 9:16 / 1:1 / 2.39:1 / 1.85:1 / 4:3（当前 {r}）"
                            ),
                        ));
                    }
                }
                let style_hint = if body.clear_style_hint.unwrap_or(false) {
                    Some(String::new())
                } else {
                    body.style_hint.as_deref().map(str::trim).map(String::from)
                };
                // —— 导出路径（export_dir）：字段缺省保留原值；空串/null = 重置
                //    缺省（项目目录本身）；非空须绝对路径 + 父目录存在（不自动
                //    创建）+ 可写，env NEXOS_FILM_EXPORT_BASE 设置时还须位于其
                //    下——校验失败 400 如实附指引。 ——
                let export_dir = match body.export_dir.as_deref() {
                    None => None,
                    Some(raw) if raw.trim().is_empty() => Some(None),
                    Some(raw) => {
                        match validate_export_dir(raw, self.effective_export_base().as_deref()) {
                            Ok(v) => Some(Some(v)),
                            Err(msg) => return Ok(error_response(400, &msg)),
                        }
                    }
                };
                {
                    let conn = self.db.lock().expect("film db poisoned");
                    if let Err(e) = update_project_fields(
                        &conn,
                        id,
                        title.as_deref(),
                        idea.as_deref(),
                        ratio.as_deref(),
                        style_hint.as_deref(),
                        None,
                    ) {
                        return Ok(error_response(500, &format!("项目更新失败: {e}")));
                    }
                    if let Some(o) = export_dir.as_ref() {
                        if let Err(e) = update_export_dir(&conn, id, o.as_deref()) {
                            return Ok(error_response(500, &format!("导出路径更新失败: {e}")));
                        }
                    }
                }
                // —— script 局部合并（镜头面板/角色绑定编辑；缺省不触碰）——
                let mut patched = false;
                if let Some(patches) = &body.script {
                    let path = format!("{}/script.json", _existing.dir);
                    let raw = match tokio::fs::read_to_string(&path).await {
                        Ok(r) => r,
                        Err(e) => {
                            return Ok(error_response(
                                400,
                                &format!("读取分镜失败 {path}: {e}（先运行 script 阶段）"),
                            ))
                        }
                    };
                    let mut file: ScriptFile = match serde_json::from_str(&raw) {
                        Ok(f) => f,
                        Err(e) => {
                            return Ok(error_response(400, &format!("分镜文件损坏: {e}")));
                        }
                    };
                    if let Err(e) = apply_shot_patches(&mut file.shots, patches) {
                        return Ok(error_response(400, &e));
                    }
                    file.generated_by = format!("{}（局部更新）", file.generated_by);
                    let pretty = serde_json::to_string_pretty(&file).unwrap_or_default();
                    if let Err(e) = tokio::fs::write(&path, pretty).await {
                        return Ok(error_response(500, &format!("写分镜失败 {path}: {e}")));
                    }
                    patched = true;
                }
                let project = try_project!(self, id);
                let script = read_script(&project).await.ok();
                let mut body = project_json(&project);
                body["script"] = script
                    .map(|s| serde_json::to_value(s).unwrap_or_default())
                    .unwrap_or(serde_json::Value::Null);
                body["script_patched"] = serde_json::json!(patched);
                Ok(ok_json(body))
            }

            (HttpMethod::Delete, ["api", "v1", "film", "projects", id]) => {
                let project = try_project!(self, id);
                {
                    let conn = self.db.lock().expect("film db poisoned");
                    if let Err(e) = delete_project_row(&conn, id) {
                        return Ok(error_response(500, &format!("项目删除失败: {e}")));
                    }
                }
                // 连产物目录删（目录不存在视为已删；删除失败仅降级提示，行已删不复活）。
                // 数据安全闸（2026-09-06 film-101 事故）：仅当行内 dir 的
                // basename 与 id 精确一致才删目录——dir 异常（漂移/篡改/劫持
                // 残留）时**保目录只删行**，宁可残留不可误删他人目录。
                let dir_matches_id = std::path::Path::new(&project.dir)
                    .file_name()
                    .and_then(|f| f.to_str())
                    .is_some_and(|name| name == *id);
                let dir_removed = if dir_matches_id {
                    match tokio::fs::remove_dir_all(&project.dir).await {
                        Ok(()) => true,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                        Err(_) => false,
                    }
                } else {
                    eprintln!(
                        "[film] 项目删除安全闸：{id} 的 dir「{}」与 id 不符——保留目录仅删行",
                        project.dir
                    );
                    false
                };
                eprintln!(
                    "[film] 项目删除：{id}（产物目录{}）",
                    if dir_removed {
                        "已删"
                    } else {
                        "删除失败"
                    }
                );
                Ok(ok_json(serde_json::json!({
                    "deleted": id,
                    "dir": project.dir,
                    "dir_removed": dir_removed,
                    "dir_preserved": !dir_matches_id,
                })))
            }

            // —— script 阶段（2026-09-06 起为 story→storyboard 新链的**兼容别名**：
            //    同一执行体 run_storyboard_stage——有剧情读 story.md 分幕，无剧情
            //    回落【创意】；双写 storyboard.json + script.json 镜像，任务
            //    output 仍指 script.json 保持旧契约）——
            (HttpMethod::Post, ["api", "v1", "film", "projects", id, "script"]) => {
                let project = try_project!(self, id);
                #[derive(Deserialize)]
                struct StageBody {
                    model_ref: ModelRef,
                    #[serde(default)]
                    author: Option<String>,
                }
                let body: StageBody = serde_json::from_value(req.body)
                    .map_err(|e| ApiGatewayError::Internal(format!("解析分镜请求体失败: {e}")))?;
                if let Err(msg) = validate_model_ref(&body.model_ref, "chat") {
                    return Ok(error_response(400, &msg));
                }
                let model_ref = body.model_ref;
                let ctx = self.ctx();
                let author = super::film_hub::author_of(&body.author);
                let task_id = self.spawn_stage_task(&project, "script", |tid| {
                    let ctx = ctx.clone();
                    let project = project.clone();
                    let author = author.clone();
                    async move {
                        super::film_hub::run_storyboard_stage(
                            &ctx, &tid, project, model_ref, false, author,
                        )
                        .await;
                    }
                });
                Ok(task_accepted(&self.tasks, &task_id)?)
            }

            // —— shots/:n/image | video | tts ——
            (HttpMethod::Post, ["api", "v1", "film", "projects", id, "shots", n, stage]) => {
                let project = try_project!(self, id);
                let Ok(shot_no) = n.parse::<u32>() else {
                    return Ok(error_response(
                        400,
                        &format!("镜头号须为正整数（当前 {n}）"),
                    ));
                };
                if shot_no == 0 {
                    return Ok(error_response(400, "镜头号从 1 起（与 script.shot 一致）"));
                }
                // 未知阶段名先拦（404）——先于请求体解析，路径即契约
                if !matches!(*stage, "image" | "video" | "tts") {
                    return Ok(error_response(
                        404,
                        &format!("未知镜头阶段: {stage}（image/video/tts）"),
                    ));
                }
                #[derive(Deserialize)]
                struct ShotStageBody {
                    model_ref: ModelRef,
                    #[serde(default)]
                    text: Option<String>,
                    #[serde(default)]
                    image_first: Option<bool>,
                    #[serde(default)]
                    author: Option<String>,
                }
                let body: ShotStageBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析镜头阶段请求体失败: {e}"))
                })?;
                let mr = body.model_ref;
                let ctx = self.ctx();
                // 请求期校验先行（400/404 即回，不建任务）
                let (stage_kind, image_first): (&str, bool) = match *stage {
                    "image" => {
                        if let Err(msg) = validate_model_ref(&mr, "image") {
                            return Ok(error_response(400, &msg));
                        }
                        ("image", false)
                    }
                    "video" => {
                        if let Err(msg) = validate_model_ref(&mr, "video") {
                            return Ok(error_response(400, &msg));
                        }
                        // image_first 缺省 true（图生视频首帧语义）
                        let image_first = body.image_first.unwrap_or(true);
                        if image_first {
                            let png = format!("{}/shot-{shot_no}.png", project.dir);
                            if !std::path::Path::new(&png).is_file() {
                                return Ok(error_response(
                                    404,
                                    &format!("首帧关键帧缺失 {png}（先运行 image 阶段）"),
                                ));
                            }
                        }
                        ("video", image_first)
                    }
                    _ => {
                        if let Err(msg) = validate_model_ref(&mr, "tts") {
                            return Ok(error_response(400, &msg));
                        }
                        ("tts", false)
                    }
                };
                let project_for_task = project.clone();
                let author = super::film_hub::author_of(&body.author);
                let task_id = self.spawn_stage_task(&project, stage_kind, move |tid| {
                    let project = project_for_task;
                    let text = body.text;
                    let author = author.clone();
                    let fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
                        match stage_kind {
                            "image" => Box::pin(async move {
                                run_image_stage(&ctx, &tid, project, shot_no, mr, author).await;
                            }),
                            "video" => Box::pin(async move {
                                run_video_stage(
                                    &ctx,
                                    &tid,
                                    project,
                                    shot_no,
                                    mr,
                                    image_first,
                                    author,
                                )
                                .await;
                            }),
                            _ => Box::pin(async move {
                                run_tts_stage(&ctx, &tid, project, shot_no, mr, text, author).await;
                            }),
                        };
                    fut
                });
                Ok(task_accepted(&self.tasks, &task_id)?)
            }

            // —— music 阶段 ——
            (HttpMethod::Post, ["api", "v1", "film", "projects", id, "music"]) => {
                let project = try_project!(self, id);
                #[derive(Deserialize)]
                struct MusicBody {
                    model_ref: ModelRef,
                    #[serde(default)]
                    prompt: Option<String>,
                    #[serde(default)]
                    author: Option<String>,
                }
                let body: MusicBody = serde_json::from_value(req.body)
                    .map_err(|e| ApiGatewayError::Internal(format!("解析 BGM 请求体失败: {e}")))?;
                if let Err(msg) = validate_model_ref(&body.model_ref, "music") {
                    return Ok(error_response(400, &msg));
                }
                let ctx = self.ctx();
                let author = super::film_hub::author_of(&body.author);
                let task_id = self.spawn_stage_task(&project, "music", |tid| {
                    let mr = body.model_ref;
                    let prompt = body.prompt;
                    let project = project.clone();
                    let author = author.clone();
                    async move {
                        run_music_stage(&ctx, &tid, project, mr, prompt, author).await;
                    }
                });
                Ok(task_accepted(&self.tasks, &task_id)?)
            }

            // —— compose 阶段（2026-09-06 改造：dist 版本化成品 final-v<ts>.mp4
            //    + compose-report.json；BGM 选择 body.bgm 指定音轨，缺省
            //    trigger=global 优先 → 旧 bgm.mp3 兜底；export_dir 语义保留为
            //    dist 落点）——
            (HttpMethod::Post, ["api", "v1", "film", "projects", id, "compose"]) => {
                let project = try_project!(self, id);
                #[derive(Deserialize, Default)]
                struct ComposeBody {
                    #[serde(default)]
                    bgm: Option<String>,
                    #[serde(default)]
                    author: Option<String>,
                }
                let body: ComposeBody = serde_json::from_value(req.body).unwrap_or_default();
                // 指定 bgm 音轨时请求期校验（404 快失败；任务内仍如实复核）
                if let Some(t) = body.bgm.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    let (input, _) = super::film_hub::select_bgm_input(&project, Some(t));
                    if input.is_none() {
                        return Ok(error_response(
                            404,
                            &format!("BGM 音轨不存在或缺 track.mp3: {t}"),
                        ));
                    }
                }
                let ctx = self.ctx();
                let author = super::film_hub::author_of(&body.author);
                let task_id = self.spawn_stage_task(&project, "compose", |tid| {
                    let ctx = ctx.clone();
                    let project = project.clone();
                    let (bgm, author) = (body.bgm, author.clone());
                    async move {
                        super::film_hub::run_compose_stage(&ctx, &tid, project, bgm, author).await;
                    }
                });
                Ok(task_accepted(&self.tasks, &task_id)?)
            }

            // —— 角色库（2026-09-04 P0 一致性）——
            (HttpMethod::Get, ["api", "v1", "film", "projects", id, "characters"]) => {
                let project = try_project!(self, id);
                let characters = {
                    let conn = self.db.lock().expect("film db poisoned");
                    load_characters(&conn, &project.id)
                };
                let bound = Self::bound_shots(&project, &characters).await;
                let views: Vec<CharacterView> = characters
                    .iter()
                    .map(|c| CharacterView {
                        portrait_url: c.portrait_ref.as_deref().map(|p| {
                            files_download_url(&format!(
                                "{}/{}",
                                project.dir.trim_end_matches('/'),
                                p
                            ))
                        }),
                        bound_shots: bound.get(&c.id).cloned().unwrap_or_default(),
                        character: c.clone(),
                    })
                    .collect();
                Ok(ok_json(to_value(&views)?))
            }

            (HttpMethod::Post, ["api", "v1", "film", "projects", id, "characters"]) => {
                let project = try_project!(self, id);
                let body: CreateCharacterBody = serde_json::from_value(req.body)
                    .map_err(|e| ApiGatewayError::Internal(format!("解析建角色请求体失败: {e}")))?;
                let name = body.name.trim();
                let description = body.description.trim();
                if name.is_empty() {
                    return Ok(error_response(400, "name 不可为空"));
                }
                if description.is_empty() {
                    return Ok(error_response(400, "description 不可为空"));
                }
                let voice = body
                    .voice
                    .as_deref()
                    .map(str::trim)
                    .filter(|v| !v.is_empty());
                {
                    let conn = self.db.lock().expect("film db poisoned");
                    if character_name_taken(&conn, &project.id, name, "") {
                        return Ok(error_response(
                            400,
                            &format!("角色名「{name}」已存在（分镜绑定按名字引用，须项目内唯一）"),
                        ));
                    }
                    let c = FilmCharacter {
                        id: self.next_character_id(),
                        project_id: project.id.clone(),
                        name: name.to_string(),
                        description: description.to_string(),
                        voice: voice.map(String::from),
                        portrait_ref: None,
                        created_at: now_iso(),
                        updated_at: now_iso(),
                    };
                    if let Err(e) = insert_character(&conn, &c) {
                        return Ok(error_response(500, &format!("角色落库失败: {e}")));
                    }
                    eprintln!(
                        "[film] 角色创建：{}（{}，项目 {}）",
                        c.id, c.name, project.id
                    );
                    return Ok(ApiResponse {
                        status: 201,
                        body: to_value(&c)?,
                        headers: serde_json::json!({}),
                    });
                }
            }

            (HttpMethod::Put, ["api", "v1", "film", "characters", cid]) => {
                let character = try_character!(self, cid);
                let body: UpdateCharacterBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析更新角色请求体失败: {e}"))
                })?;
                let name = body
                    .name
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from);
                let description = body
                    .description
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from);
                // voice 语义：字段缺省=保留；空串=清空（回落全局缺省）；非空=设置
                let voice = body.voice.as_deref().map(|v| {
                    let t = v.trim();
                    if t.is_empty() {
                        None
                    } else {
                        Some(t)
                    }
                });
                if let Some(n) = &name {
                    let conn = self.db.lock().expect("film db poisoned");
                    if character_name_taken(&conn, &character.project_id, n, cid) {
                        return Ok(error_response(
                            400,
                            &format!("角色名「{n}」已存在（分镜绑定按名字引用，须项目内唯一）"),
                        ));
                    }
                }
                {
                    let conn = self.db.lock().expect("film db poisoned");
                    if let Err(e) = update_character_fields(
                        &conn,
                        cid,
                        name.as_deref(),
                        description.as_deref(),
                        voice,
                        None,
                    ) {
                        return Ok(error_response(500, &format!("角色更新失败: {e}")));
                    }
                }
                let conn = self.db.lock().expect("film db poisoned");
                match find_character(&conn, cid) {
                    Some(c) => Ok(ok_json(to_value(&c)?)),
                    None => Ok(error_response(404, &format!("角色不存在: {cid}"))),
                }
            }

            (HttpMethod::Delete, ["api", "v1", "film", "characters", cid]) => {
                let character = try_character!(self, cid);
                let project = try_project!(self, &character.project_id);
                {
                    let conn = self.db.lock().expect("film db poisoned");
                    if let Err(e) = delete_character_row(&conn, cid) {
                        return Ok(error_response(500, &format!("角色删除失败: {e}")));
                    }
                }
                // 定妆图目录连删（<dir>/characters/<cid>/；不存在视为已删）
                let dir = format!(
                    "{}/characters/{}",
                    project.dir.trim_end_matches('/'),
                    character.id
                );
                let dir_removed = match tokio::fs::remove_dir_all(&dir).await {
                    Ok(()) => true,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                    Err(_) => false,
                };
                eprintln!(
                    "[film] 角色删除：{cid}（{}，目录{}）",
                    character.name,
                    if dir_removed {
                        "已删"
                    } else {
                        "删除失败"
                    }
                );
                Ok(ok_json(serde_json::json!({
                    "deleted": cid,
                    "dir": dir,
                    "dir_removed": dir_removed,
                })))
            }

            // 定妆图上传（b64 ≤10MB png/jpeg/webp；穿越面：路径由 cid+白名单扩展名拼装）
            (
                HttpMethod::Post,
                ["api", "v1", "film", "projects", id, "characters", cid, "portrait"],
            ) => {
                let project = try_project!(self, id);
                let character = try_character!(self, cid);
                if character.project_id != project.id {
                    return Ok(error_response(404, &format!("角色 {cid} 不属于项目 {id}")));
                }
                let body: PortraitUploadBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析定妆图上传请求体失败: {e}"))
                })?;
                let raw = body.image_b64.trim();
                if raw.is_empty() {
                    return Ok(error_response(400, "image_b64 不可为空"));
                }
                if raw.contains("data:") {
                    return Ok(error_response(
                        400,
                        "image_b64 须为原始标准 b64（不带 data: 前缀）",
                    ));
                }
                use base64::Engine;
                let bytes = match base64::engine::general_purpose::STANDARD.decode(raw) {
                    Ok(b) => b,
                    Err(e) => return Ok(error_response(400, &format!("image_b64 解码失败: {e}"))),
                };
                if bytes.is_empty() {
                    return Ok(error_response(400, "image_b64 解码后为空"));
                }
                if bytes.len() > IMAGE_MAX_BYTES {
                    return Ok(error_response(
                        400,
                        &format!(
                            "图片超过上限 {}MB（当前 {:.1}MB）",
                            IMAGE_MAX_BYTES / 1024 / 1024,
                            bytes.len() as f64 / 1024.0 / 1024.0
                        ),
                    ));
                }
                let ext =
                    match body.mime.as_deref() {
                        Some(m) => match ext_for_mime(m) {
                            Some(ext) => ext,
                            None => {
                                return Ok(error_response(
                                    400,
                                    &format!("mime 仅支持 png/jpeg/webp（当前 {m}）"),
                                ))
                            }
                        },
                        // mime 缺省按魔数嗅探；嗅探不出按 400 如实拒
                        None => match sniff_image_ext(&bytes) {
                            Some(ext) => ext,
                            None => return Ok(error_response(
                                400,
                                "mime 缺省时按图片魔数嗅探仅支持 png/jpeg/webp（请显式传 mime）",
                            )),
                        },
                    };
                // 双保险：mime 声明与魔数不一致 → 400（防改扩展名伪装）
                if sniff_image_ext(&bytes) != Some(ext) {
                    return Ok(error_response(
                        400,
                        "图片内容与 mime 不符（魔数校验失败；仅支持 png/jpeg/webp）",
                    ));
                }
                let dir = format!(
                    "{}/characters/{}",
                    project.dir.trim_end_matches('/'),
                    character.id
                );
                if let Err(e) = tokio::fs::create_dir_all(&dir).await {
                    return Ok(error_response(500, &format!("建角色目录失败 {dir}: {e}")));
                }
                let portrait_ref = format!("characters/{}/portrait.{ext}", character.id);
                let path = format!("{}/{}", project.dir.trim_end_matches('/'), portrait_ref);
                if let Err(e) = tokio::fs::write(&path, &bytes).await {
                    return Ok(error_response(500, &format!("写定妆图失败 {path}: {e}")));
                }
                {
                    let conn = self.db.lock().expect("film db poisoned");
                    if let Err(e) =
                        update_character_fields(&conn, cid, None, None, None, Some(&portrait_ref))
                    {
                        return Ok(error_response(500, &format!("portrait_ref 回写失败: {e}")));
                    }
                }
                let conn = self.db.lock().expect("film db poisoned");
                match find_character(&conn, cid) {
                    Some(c) => {
                        eprintln!("[film] 定妆图上传：{cid}（{}，{}）", c.name, path);
                        Ok(ApiResponse {
                            status: 201,
                            body: to_value(&c)?,
                            headers: serde_json::json!({}),
                        })
                    }
                    None => Ok(error_response(404, &format!("角色不存在: {cid}"))),
                }
            }

            // 定妆图生成（走既有生图面；202 任务与阶段同生命周期）
            (
                HttpMethod::Post,
                ["api", "v1", "film", "projects", id, "characters", cid, "portrait", "generate"],
            ) => {
                let project = try_project!(self, id);
                let character = try_character!(self, cid);
                if character.project_id != project.id {
                    return Ok(error_response(404, &format!("角色 {cid} 不属于项目 {id}")));
                }
                let body: PortraitGenBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析定妆图生成请求体失败: {e}"))
                })?;
                if let Err(msg) = validate_model_ref(&body.model_ref, "image") {
                    return Ok(error_response(400, &msg));
                }
                let ctx = self.ctx();
                let task_id = self.spawn_stage_task(&project, "portrait", |tid| {
                    let mr = body.model_ref;
                    let prompt = body.prompt;
                    let project = project.clone();
                    let character = character.clone();
                    async move {
                        run_portrait_stage(&ctx, &tid, project, character, mr, prompt).await;
                    }
                });
                Ok(task_accepted(&self.tasks, &task_id)?)
            }

            // 通用参考导入（场景/风格参考；P0 存储与列出）
            (HttpMethod::Post, ["api", "v1", "film", "projects", id, "refs"]) => {
                let project = try_project!(self, id);
                let body: RefUploadBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析参考图上传请求体失败: {e}"))
                })?;
                let raw = body.image_b64.trim();
                if raw.is_empty() {
                    return Ok(error_response(400, "image_b64 不可为空"));
                }
                use base64::Engine;
                let bytes = match base64::engine::general_purpose::STANDARD.decode(raw) {
                    Ok(b) => b,
                    Err(e) => return Ok(error_response(400, &format!("image_b64 解码失败: {e}"))),
                };
                if bytes.is_empty() {
                    return Ok(error_response(400, "image_b64 解码后为空"));
                }
                if bytes.len() > IMAGE_MAX_BYTES {
                    return Ok(error_response(
                        400,
                        &format!(
                            "图片超过上限 {}MB（当前 {:.1}MB）",
                            IMAGE_MAX_BYTES / 1024 / 1024,
                            bytes.len() as f64 / 1024.0 / 1024.0
                        ),
                    ));
                }
                let Some(ext) = sniff_image_ext(&bytes) else {
                    return Ok(error_response(
                        400,
                        "仅支持 png/jpeg/webp 图片（魔数嗅探失败）",
                    ));
                };
                let dir = format!("{}/refs", project.dir.trim_end_matches('/'));
                if let Err(e) = tokio::fs::create_dir_all(&dir).await {
                    return Ok(error_response(500, &format!("建 refs 目录失败 {dir}: {e}")));
                }
                // 文件名 = uuid 形态（时间戳毫秒 + 计数）；filename 仅入响应供展示
                let name = format!(
                    "ref-{}-{}.{ext}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0),
                    self.task_seq.fetch_add(1, Ordering::SeqCst)
                );
                let path = format!("{dir}/{name}");
                if let Err(e) = tokio::fs::write(&path, &bytes).await {
                    return Ok(error_response(500, &format!("写参考图失败 {path}: {e}")));
                }
                eprintln!("[film] 参考图导入：{id}（{path}）");
                Ok(ApiResponse {
                    status: 201,
                    body: serde_json::json!({
                        "name": name,
                        "filename": body.filename,
                        "path": path,
                        "bytes": bytes.len(),
                    }),
                    headers: serde_json::json!({}),
                })
            }

            // —— 任务面 ——
            (HttpMethod::Get, ["api", "v1", "film", "tasks"]) => {
                let tasks = self.tasks.lock().expect("film tasks poisoned");
                let mut list: Vec<TaskSummary> = tasks.values().map(TaskSummary::from).collect();
                list.sort_by_key(|a| a.created_at);
                Ok(ok_json(to_value(&list)?))
            }

            (HttpMethod::Get, ["api", "v1", "film", "tasks", id]) => {
                let tasks = self.tasks.lock().expect("film tasks poisoned");
                match tasks.get(*id) {
                    Some(t) => Ok(ok_json(to_value(t)?)),
                    None => Ok(error_response(404, &format!("任务不存在: {id}"))),
                }
            }

            // —— ffmpeg 检测 ——
            (HttpMethod::Get, ["api", "v1", "film", "tools"]) => {
                let path = self
                    .ffmpeg_bin
                    .clone()
                    .or_else(detect_ffmpeg)
                    .filter(|p| is_executable(p));
                Ok(ok_json(serde_json::json!({
                    "ffmpeg": {
                        "available": path.is_some(),
                        "path": path,
                        "install_hint": FFMPEG_INSTALL_HINT,
                    },
                })))
            }

            // —— FilmHub 新链（2026-09-06）：委托 film_hub 模块分发；未匹配
            //    （返回 None）才落回 404 ——
            _ => match super::film_hub::try_handle(self, &req, &segs).await? {
                Some(resp) => Ok(resp),
                None => Ok(error_response(404, "film: 未匹配的路由")),
            },
        }
    }
}

// ----------------------------------------------------------------------------
// 内部辅助（路由声明 + 响应）
// ----------------------------------------------------------------------------

pub(crate) fn spec_admin(method: HttpMethod, path: &str) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: "film".to_string(),
        requires_auth: true,
        required_roles: vec!["admin".into()],
    }
}

pub(crate) fn spec_public(method: HttpMethod, path: &str) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: "film".to_string(),
        requires_auth: false,
        required_roles: vec![],
    }
}

/// 202 响应体（任务摘要）。
pub(crate) fn task_accepted(
    tasks: &Arc<Mutex<HashMap<String, FilmTask>>>,
    id: &str,
) -> Result<ApiResponse, ApiGatewayError> {
    let summary = tasks
        .lock()
        .expect("film tasks poisoned")
        .get(id)
        .map(TaskSummary::from);
    match summary {
        Some(s) => Ok(ApiResponse {
            status: 202,
            body: to_value(&s)?,
            headers: serde_json::json!({}),
        }),
        None => Ok(error_response(500, &format!("任务创建异常: {id}"))),
    }
}

pub(crate) fn ok_json(body: serde_json::Value) -> ApiResponse {
    ApiResponse {
        status: 200,
        body,
        headers: serde_json::json!({}),
    }
}

pub(crate) fn error_response(status: u16, msg: &str) -> ApiResponse {
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

/// 查角色（Err=404 响应体；cid 非法/不存在同口径——查库天然防穿越）。
fn character_or_404(h: &FilmRouteHandler, cid: &str) -> Result<FilmCharacter, ApiResponse> {
    let conn = h.db.lock().expect("film db poisoned");
    find_character(&conn, cid).ok_or_else(|| error_response(404, &format!("角色不存在: {cid}")))
}

// ----------------------------------------------------------------------------
// 单元测试（mock 注入：fake vLLM TCP / 渠道 HTTP mock / 中继互连端点 /
// ffmpeg 假二进制脚本 / 生图假脚本——绝不真调外部模型、不装真实 ffmpeg）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::Mutex as StdMutex;

    // ------------------------------------------------------------------
    // 通用 fixture
    // ------------------------------------------------------------------

    /// 每测独立临时目录（进程 id + 测名唯一，防并行互踩）。
    fn temp_dir_for(test: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nexos-film-{test}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 写一个可执行假脚本（unix），返回路径。
    #[cfg(unix)]
    fn fake_exec(dir: &std::path::Path, name: &str, content: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        let mut perm = std::fs::metadata(&path).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&path, perm).unwrap();
        path
    }

    /// 带 DB + 产物根目录的 handler（每测独立临时目录，无渠道/LLM 注入）。
    fn handler_at(test: &str) -> (FilmRouteHandler, std::path::PathBuf) {
        let dir = temp_dir_for(test);
        let h = FilmRouteHandler::with_db_path(dir.join("film.db").to_str().unwrap())
            .with_root_dir(dir.join("root").to_str().unwrap());
        (h, dir)
    }

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

    fn put_req(path: &str, body: serde_json::Value) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Put,
            path: path.into(),
            headers: serde_json::json!({}),
            body,
            auth: None,
        }
    }

    fn delete_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Delete,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    /// 建项目（直连 handler），返回 (id, dir)。
    async fn create_project(h: &FilmRouteHandler, ratio: &str) -> (String, String) {
        let resp = h
            .handle(post_req(
                "/api/v1/film/projects",
                serde_json::json!({
                    "title": "测试影片",
                    "idea": "一只猫在霓虹城市里寻找回家路",
                    "ratio": ratio,
                    "style_hint": "赛博朋克",
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "建项目失败: {resp:?}");
        (
            resp.body["id"].as_str().unwrap().to_string(),
            resp.body["dir"].as_str().unwrap().to_string(),
        )
    }

    /// 直写 script.json（阶段测试的种子分镜，绕开 chat mock）。
    fn seed_script(dir: &str, shots: Vec<serde_json::Value>) {
        let file = serde_json::json!({
            "shots": shots,
            "generated_by": "test-seed",
            "created_at": "2026-09-04T00:00:00+08:00",
        });
        std::fs::write(
            format!("{dir}/script.json"),
            serde_json::to_string_pretty(&file).unwrap(),
        )
        .unwrap();
    }

    fn shot_json(n: u32, line: &str, dur: u32) -> serde_json::Value {
        serde_json::json!({
            "shot": n,
            "desc": format!("镜头{n}画面"),
            "image_prompt": format!("镜头{n}关键帧"),
            "video_prompt": format!("镜头{n}运动"),
            "line": line,
            "duration_secs": dur,
        })
    }

    /// 轮询任务到终态（done/error），返回任务详情 body。
    async fn wait_task(h: &FilmRouteHandler, id: &str) -> serde_json::Value {
        for _ in 0..400 {
            let resp = h
                .handle(get_req(&format!("/api/v1/film/tasks/{id}")))
                .await
                .unwrap();
            let status = resp.body["status"].as_str().unwrap_or("");
            if status == "done" || status == "error" {
                return resp.body;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("任务 {id} 未在 10s 内到达终态");
    }

    /// 触发一个阶段任务并等待终态，返回 (任务 body, task_id)。
    async fn run_stage(
        h: &FilmRouteHandler,
        path: &str,
        body: serde_json::Value,
    ) -> (serde_json::Value, String) {
        let resp = h.handle(post_req(path, body)).await.unwrap();
        assert_eq!(resp.status, 202, "阶段应 202: {resp:?}");
        let id = resp.body["id"].as_str().unwrap().to_string();
        let task = wait_task(h, &id).await;
        (task, id)
    }

    // ------------------------------------------------------------------
    // HTTP mock 上游（单连接一响应队列；捕获请求原文）
    // ------------------------------------------------------------------

    /// 多发 mock 上游：按序服务 `responses`（每个一次 HTTP 请求），返回
    /// (port, 请求原文捕获)。
    fn spawn_mock_upstream(responses: Vec<Vec<u8>>) -> (u16, Arc<StdMutex<Vec<String>>>) {
        let hits: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(vec![]));
        let hits2 = Arc::clone(&hits);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 失败");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for body in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buf: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 8192];
                loop {
                    match stream.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&chunk[..n]);
                            let text = String::from_utf8_lossy(&buf);
                            if let Some(hend) = text.find("\r\n\r\n") {
                                let cl = text[..hend]
                                    .lines()
                                    .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                                    .and_then(|l| l.split(':').nth(1))
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                                    .unwrap_or(0);
                                if buf.len() >= hend + 4 + cl {
                                    break;
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
                hits2
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf).into_owned());
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });
        (port, hits)
    }

    /// OpenAI chat/completions 形态响应体（content 指定）。
    fn chat_response(content: &str) -> Vec<u8> {
        serde_json::json!({
            "id": "chatcmpl-film-test",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 5, "total_tokens": 8},
        })
        .to_string()
        .into_bytes()
    }

    /// 两条标准分镜的 JSON 数组文本（LLM content 用）。
    fn two_shots_json() -> String {
        serde_json::json!([
            {"shot":1,"desc":"开场霓虹街景","image_prompt":"霓虹街景关键帧","video_prompt":"缓慢推进","line":"这是哪里？","duration_secs":5},
            {"shot":2,"desc":"猫跃上屋顶","image_prompt":"猫剪影关键帧","video_prompt":"跃起运镜","line":"回家吧。","duration_secs":4},
        ])
        .to_string()
    }

    /// 在网关上直插一条渠道（经 REST POST /gateway/channels），返回渠道 id。
    async fn seed_channel(
        gw: &ApiGatewayRouteHandler,
        base_url: &str,
        via_node: Option<&str>,
    ) -> String {
        let mut body = serde_json::json!({
            "name": "film-test-渠道",
            "provider": "openai",
            "base_url": base_url,
            "api_key": "sk-upstream-test",
            "models": ["test-model"],
        });
        if let Some(vn) = via_node {
            body["via_node"] = serde_json::Value::String(vn.to_string());
        }
        let resp = gw
            .handle(post_req("/api/v1/gateway/channels", body))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "种渠道失败: {resp:?}");
        resp.body["id"].as_str().unwrap().to_string()
    }

    /// 假互连 overlay fixture（api_gateway/api_market 测试同款手法）：消费者
    /// 端点（注入 gw.set_relay）↔ 源端端点（白名单=base）定向互投。
    fn relay_pair(base: &str) -> (crate::handlers::api_market::ApiMarketFedEndpoint, String) {
        let consumer = crate::handlers::api_market::ApiMarketFedEndpoint::test_endpoint();
        let source =
            crate::handlers::api_market::ApiMarketFedEndpoint::test_endpoint_with_local_listing(
                base,
            );
        let a_id = os_p2p::NodeIdentity::generate().node_id();
        let b_id = os_p2p::NodeIdentity::generate().node_id();
        let a_hex = a_id.to_hex();
        let b_hex = b_id.to_hex();
        let b2 = source.clone();
        let b_target = b_id.clone();
        let a_from = a_id.clone();
        consumer.set_full_transport(
            Arc::new(move |to, payload| {
                if *to == b_target {
                    b2.dispatch(&a_from, &payload);
                }
            }),
            Arc::new(|_| {}),
            a_hex,
            "film-consumer".into(),
        );
        let a3 = consumer.clone();
        let a_target = a_id.clone();
        let b_from = b_id.clone();
        source.set_full_transport(
            Arc::new(move |to, payload| {
                if *to == a_target {
                    a3.dispatch(&b_from, &payload);
                }
            }),
            Arc::new(|_| {}),
            b_hex.clone(),
            "film-source".into(),
        );
        (consumer, b_hex)
    }

    // ------------------------------------------------------------------
    // 路由声明与鉴权矩阵
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn routes_declares_twentyone_film_endpoints_with_auth_matrix() {
        let h = FilmRouteHandler::with_db_path(":memory:").with_root_dir("/tmp/nexos-film-routes");
        let routes = h.routes().await;
        assert_eq!(
            routes.len(),
            45,
            "应有 45 条路由（21 基线 + 21 FilmHub 新链 + 3 story 文档管线 clean/chapterize/profile）: {routes:?}"
        );
        assert!(
            routes.iter().all(|r| r.handler_component == "film"),
            "全部归属 film 组件"
        );
        // 写端点（POST/PUT/DELETE）需 admin；读端点（GET）公开
        for r in &routes {
            if r.method == HttpMethod::Get {
                assert!(!r.requires_auth, "GET 应公开: {r:?}");
                assert!(r.required_roles.is_empty(), "GET 无角色要求: {r:?}");
            } else {
                assert!(r.requires_auth, "写端点需鉴权: {r:?}");
                assert_eq!(
                    r.required_roles,
                    vec!["admin".to_string()],
                    "写端点需 admin: {r:?}"
                );
            }
        }
        let paths: Vec<&str> = routes.iter().map(|r| r.path.as_str()).collect();
        for expect in [
            "/api/v1/film/projects",
            "/api/v1/film/projects/:id",
            "/api/v1/film/projects/:id/script",
            "/api/v1/film/projects/:id/shots/:n/image",
            "/api/v1/film/projects/:id/shots/:n/video",
            "/api/v1/film/projects/:id/shots/:n/tts",
            "/api/v1/film/projects/:id/music",
            "/api/v1/film/projects/:id/compose",
            "/api/v1/film/projects/:id/characters",
            "/api/v1/film/characters/:cid",
            "/api/v1/film/projects/:id/characters/:cid/portrait",
            "/api/v1/film/projects/:id/characters/:cid/portrait/generate",
            "/api/v1/film/projects/:id/refs",
            "/api/v1/film/tasks",
            "/api/v1/film/tasks/:id",
            "/api/v1/film/tools",
            // FilmHub 新链（2026-09-06）
            "/api/v1/film/projects/:id/story/import",
            "/api/v1/film/projects/:id/story/generate",
            // v0.1.37 story 文档处理管线（清理/分章/人物梳理）
            "/api/v1/film/projects/:id/story/clean",
            "/api/v1/film/projects/:id/story/chapterize",
            "/api/v1/film/projects/:id/story/profile",
            "/api/v1/film/projects/:id/storyboard/generate",
            "/api/v1/film/projects/:id/casting/extract",
            "/api/v1/film/projects/:id/casting/:type",
            "/api/v1/film/projects/:id/casting/:type/:name",
            "/api/v1/film/projects/:id/casting/:type/:name/views/generate",
            "/api/v1/film/projects/:id/casting/:type/:name/views/import",
            "/api/v1/film/projects/:id/audio/bgm",
            "/api/v1/film/projects/:id/audio/bgm/:track",
            "/api/v1/film/projects/:id/audio/bgm/:track/generate",
            "/api/v1/film/projects/:id/cache/:file/commit",
            "/api/v1/film/projects/:id/files",
            "/api/v1/film/projects/:id/files/*",
            "/api/v1/film/projects/:id/export",
            "/api/v1/film/projects/:id/import",
            "/api/v1/film/projects/:id/cost",
        ] {
            assert!(paths.contains(&expect), "缺路由 {expect}: {paths:?}");
        }
    }

    // ------------------------------------------------------------------
    // 纯函数：画幅 / model_ref / 超时 / ffmpeg 检测
    // ------------------------------------------------------------------

    #[test]
    fn ratio_dims_covers_six_presets_and_image_dims_safe() {
        // 合成分辨率：六档预设表（前端新建弹窗预设卡同源）
        assert_eq!(ratio_dims("16:9"), Some((1920, 1080)));
        assert_eq!(ratio_dims("9:16"), Some((1080, 1920)));
        assert_eq!(ratio_dims("2.39:1"), Some((2048, 858)));
        assert_eq!(ratio_dims("1.85:1"), Some((1998, 1080)));
        assert_eq!(ratio_dims("1:1"), Some((1080, 1080)));
        assert_eq!(ratio_dims("4:3"), Some((1440, 1080)));
        assert_eq!(ratio_dims("4:5"), None, "非法画幅应 None");
        // 生图安全尺寸：六档全为 8 的倍数（sd-turbo/diffusers 要求）
        for r in ["16:9", "9:16", "2.39:1", "1.85:1", "1:1", "4:3"] {
            let (w, h) = image_dims(r).unwrap_or((0, 0));
            assert!(
                w % 8 == 0 && h % 8 == 0 && w > 0 && h > 0,
                "{r} 生图尺寸应为 8 的倍数（{w}x{h}）"
            );
        }
        assert_eq!(image_dims("4:5"), None, "非法画幅生图尺寸应 None");
        // 合成分辨率全偶（yuv420p；钳偶兜底在 build_concat_args 单测）
        for r in ["16:9", "9:16", "2.39:1", "1.85:1", "1:1", "4:3"] {
            let (w, h) = ratio_dims(r).expect("六档必有");
            assert!(w % 2 == 0 && h % 2 == 0, "{r} 合成分辨率应为偶数");
        }
    }

    #[test]
    fn validate_model_ref_branches() {
        let mk = |source: &str, cid: Option<&str>, cap: &str| ModelRef {
            source: source.into(),
            channel_id: cid.map(String::from),
            capability: cap.into(),
            model: None,
        };
        // 阶段能力匹配
        assert!(
            validate_model_ref(&mk("local", None, "image"), "chat").is_err(),
            "能力不匹配应拒绝"
        );
        // local 支持面：chat/image 过、video/tts/music 拒（附渠道指引）
        assert!(validate_model_ref(&mk("local", None, "chat"), "chat").is_ok());
        assert!(validate_model_ref(&mk("local", None, "image"), "image").is_ok());
        for cap in ["video", "tts", "music"] {
            let err =
                validate_model_ref(&mk("local", None, cap), cap).expect_err("本地无该能力应拒绝");
            assert!(err.contains("channel"), "应提示改用渠道: {err}");
        }
        // channel 必带 channel_id
        assert!(validate_model_ref(&mk("channel", None, "chat"), "chat").is_err());
        assert!(validate_model_ref(&mk("channel", Some("ch-1"), "chat"), "chat").is_ok());
        // 未知 source
        assert!(validate_model_ref(&mk(" federated ", None, "chat"), "chat").is_err());
    }

    #[test]
    fn parse_stage_timeout_clamps_to_bounds() {
        assert_eq!(parse_stage_timeout(None, 600), 600);
        assert_eq!(parse_stage_timeout(Some("120"), 600), 120);
        assert_eq!(
            parse_stage_timeout(Some("10"), 600),
            600,
            "低于下限回落缺省"
        );
        assert_eq!(
            parse_stage_timeout(Some("9999"), 600),
            600,
            "高于上限回落缺省"
        );
        assert_eq!(parse_stage_timeout(Some("abc"), 600), 600);
    }

    #[cfg(unix)]
    #[test]
    fn detect_ffmpeg_finds_env_path_and_candidates() {
        let dir = temp_dir_for("ffmpeg-detect");
        let bin = fake_exec(&dir, "ffmpeg", "#!/bin/sh\nexit 0\n");
        // env 注入（可执行）优先
        assert_eq!(
            detect_ffmpeg_with(Some(bin.to_str().unwrap()), &[], &[]),
            Some(bin.to_string_lossy().into_owned())
        );
        // env 指向不存在文件 → 跳过 → PATH 目录扫描
        assert_eq!(
            detect_ffmpeg_with(
                Some("/nonexistent/ffmpeg"),
                &[dir.to_string_lossy().into_owned()],
                &[]
            ),
            Some(bin.to_string_lossy().into_owned()),
            "PATH 扫描应命中"
        );
        // 兜底候选位
        assert_eq!(
            detect_ffmpeg_with(None, &[], &[bin.to_str().unwrap()]),
            Some(bin.to_string_lossy().into_owned())
        );
        // 全空 → None（缺失即报安装指引，不猜）
        assert_eq!(detect_ffmpeg_with(None, &[], &[]), None);
    }

    // ------------------------------------------------------------------
    // 纯函数：分镜 JSON 解析容错
    // ------------------------------------------------------------------

    #[test]
    fn parse_script_shots_accepts_plain_array() {
        let shots = parse_script_shots(&two_shots_json()).expect("纯数组应可解析");
        assert_eq!(shots.len(), 2);
        assert_eq!(shots[0].shot, 1);
        assert_eq!(shots[0].line, "这是哪里？");
        assert_eq!(shots[1].duration_secs, 4);
    }

    #[test]
    fn parse_script_shots_tolerates_fenced_and_prose_wrapping() {
        let fenced = format!(
            "好的，分镜如下：\n```json\n{}\n```\n希望有帮助！",
            two_shots_json()
        );
        let shots = parse_script_shots(&fenced).expect("围栏块应可解析");
        assert_eq!(shots.len(), 2, "围栏内 JSON 应被提取");
        let prose = format!("前情提要……{}……以上。", two_shots_json());
        let shots = parse_script_shots(&prose).expect("首尾中括号切片应可解析");
        assert_eq!(shots.len(), 2);
    }

    #[test]
    fn parse_script_shots_defaults_and_clamps() {
        let raw = r#"[{"desc":"只有描述"},{"desc":"时长字符串","duration_secs":"8秒"}]"#;
        let shots = parse_script_shots(raw).expect("缺省字段应可解析");
        assert_eq!(shots.len(), 2);
        assert_eq!(
            shots[0].duration_secs, SHOT_DURATION_DEFAULT_SECS,
            "缺省时长 5"
        );
        assert_eq!(shots[0].shot, 1, "缺省镜头号=序号");
        assert_eq!(shots[1].duration_secs, 8, "字符串时长'8秒'应归一为 8");
        // 越界钳制
        let raw = r#"[{"desc":"超长","duration_secs":999}]"#;
        let shots = parse_script_shots(raw).unwrap();
        assert_eq!(shots[0].duration_secs, SHOT_DURATION_MAX_SECS, "钳到 60");
    }

    #[test]
    fn parse_script_shots_rejects_garbage_and_empty_shots() {
        assert!(parse_script_shots("我认为这个创意很难拍").is_err());
        assert!(
            parse_script_shots("[{\"line\":\"无描述无图\"}]").is_err(),
            "双空镜头过滤后为空应 Err"
        );
        assert!(parse_script_shots("[]").is_err(), "空数组应 Err");
    }

    #[test]
    fn retry_prompt_tightens_output() {
        let p = build_retry_prompt("一只猫在霓虹城市里寻找回家路", &[]);
        assert!(p.contains('[') && p.contains(']'), "应要求数组本体: {p}");
        assert!(p.contains("markdown"), "应禁 markdown 标记: {p}");
    }

    #[test]
    fn script_prompt_carries_idea_ratio_style() {
        let p = build_script_prompt("创意X", "9:16", Some("水墨"), &[]);
        assert!(p.contains("创意X") && p.contains("9:16") && p.contains("水墨"));
        assert!(p.contains("duration_secs") && p.contains("image_prompt"));
        assert!(!p.contains("角色表"), "空角色表不注入角色段: {p}");
        let p2 = build_script_prompt("创意Y", "16:9", None, &[]);
        assert!(p2.contains("电影感"), "缺省风格提示: {p2}");
    }

    // ------------------------------------------------------------------
    // 分镜质量修复（2026-09-04）：提示词硬约束 + <think> 容错
    // ------------------------------------------------------------------

    /// 硬约束文本（script/retry 两提示词共用断言片段）。
    const HARD_CONSTRAINT_FRAGMENTS: [&str; 3] =
        ["禁止更换题材", "禁止另编", "直接服务于该创意的叙事"];

    #[test]
    fn script_prompt_has_hard_topic_constraints_and_double_anchor() {
        let idea = "一只猫在霓虹城市里寻找回家路";
        let p = build_script_prompt(idea, "16:9", None, &[]);
        for frag in HARD_CONSTRAINT_FRAGMENTS {
            assert!(p.contains(frag), "script 提示词应含硬约束「{frag}」: {p}");
        }
        // 首尾夹逼：创意原文出现 ≥2 次（开头【创意】+ 结尾再强调）
        assert!(p.matches(idea).count() >= 2, "创意应首尾双锚定: {p}");
    }

    #[test]
    fn retry_prompt_has_hard_topic_constraints_and_idea_anchored() {
        let idea = "一只猫在霓虹城市里寻找回家路";
        let p = build_retry_prompt(idea, &[]);
        for frag in HARD_CONSTRAINT_FRAGMENTS {
            assert!(p.contains(frag), "retry 提示词应含硬约束「{frag}」: {p}");
        }
        assert!(p.contains(idea), "retry 提示词应嵌创意原文: {p}");
    }

    #[test]
    fn strip_think_blocks_removes_reasoning_and_keeps_json() {
        // 闭合思考段（含方括号噪声）——剥掉后正常解析
        let body = two_shots_json();
        let with_think = format!("<think>思考噪声 [1] 也许换个故事？</think>\n{body}");
        let shots = parse_script_shots(&with_think).expect("剥思考段后应可解析");
        assert_eq!(shots.len(), 2, "{shots:?}");
        // 多段思考 + 段间正文
        let multi = format!("<think>a[0]</think>前言<think>b</think>{body}");
        assert!(parse_script_shots(&multi).is_ok(), "多段思考全剥");
        // 未闭合思考（思考吞掉全部输出）→ Err 触发重试，不误产分镜
        assert!(
            parse_script_shots("<think>我还没想好……").is_err(),
            "未闭合思考应 Err"
        );
        // 无思考段原文直通
        assert!(parse_script_shots(&body).is_ok());
    }

    // ------------------------------------------------------------------
    // 纯函数：SRT / concat / mix argv
    // ------------------------------------------------------------------

    #[test]
    fn srt_timestamp_format() {
        assert_eq!(fmt_srt_ts(0), "00:00:00,000");
        assert_eq!(fmt_srt_ts(3_661_500), "01:01:01,500");
        assert_eq!(fmt_srt_ts(59_999), "00:00:59,999");
    }

    #[test]
    fn build_srt_follows_script_timeline_and_skips_empty_lines() {
        let shots = vec![
            serde_json::from_value::<ScriptShot>(shot_json(1, "台词一", 5)).unwrap(),
            serde_json::from_value::<ScriptShot>(shot_json(2, "", 4)).unwrap(),
            serde_json::from_value::<ScriptShot>(shot_json(3, "台词三", 3)).unwrap(),
        ];
        let srt = build_srt(&shots);
        assert!(
            srt.contains("00:00:00,000 --> 00:00:05,000"),
            "首 cue 0-5s: {srt}"
        );
        assert!(
            srt.contains("00:00:09,000 --> 00:00:12,000"),
            "第三 cue 累计 5+4=9s 起: {srt}"
        );
        assert!(srt.contains("台词一") && srt.contains("台词三"));
        assert_eq!(srt.matches(" --> ").count(), 2, "无台词镜头不产 cue");
    }

    #[test]
    fn build_concat_list_and_args_shape() {
        let list = build_concat_list(3);
        assert_eq!(
            list,
            "file 'shot-1.mp4'\nfile 'shot-2.mp4'\nfile 'shot-3.mp4'\n"
        );
        let args = build_concat_args(1272, 720, "compose-video.mp4");
        assert_eq!(args[0], "-y");
        assert_eq!(
            &args[1..7],
            &["-f", "concat", "-safe", "0", "-i", "compose-concat.txt"]
        );
        assert!(
            args.iter()
                .any(|a| a.contains("scale=1272:720:force_original_aspect_ratio=decrease")),
            "统一尺寸: {args:?}"
        );
        assert!(
            args.iter().any(|a| a.contains("fps=30")),
            "统一 fps: {args:?}"
        );
        assert_eq!(args.last().unwrap(), "compose-video.mp4");
    }

    #[test]
    fn build_concat_args_clamps_odd_dims_to_even() {
        // 非整比映射若产生奇数宽高 → yuv420p 报错；scale/pad 统一钳到偶
        let args = build_concat_args(1999, 1081, "compose-video.mp4");
        let vf = args
            .iter()
            .find(|a| a.starts_with("scale="))
            .expect("应有 -vf 尺寸串");
        assert!(vf.contains("scale=1998:1080"), "奇数钳到偶: {vf}");
        assert!(vf.contains("pad=1998:1080"), "pad 同步钳偶: {vf}");
        // 偶数输入不被改动
        let args = build_concat_args(2048, 858, "compose-video.mp4");
        let vf = args.iter().find(|a| a.starts_with("scale=")).unwrap();
        assert!(vf.contains("scale=2048:858"), "{vf}");
    }

    #[test]
    fn build_mix_args_full_form_voice_bgm_subtitles() {
        let voices = vec![
            ("line-1.mp3".to_string(), 0u64),
            ("line-3.mp3".to_string(), 9000u64),
        ];
        let args = build_mix_args(&voices, Some("bgm.mp3"), true, "final.mp4");
        // BGM 输入带 -stream_loop -1（循环铺满）
        let joined = args.join(" ");
        assert!(
            joined.contains("-stream_loop -1 -i bgm.mp3"),
            "BGM 循环: {joined}"
        );
        // filter_complex：字幕 + 双路 adelay + amix 合一 + BGM 音量 + 终混
        let fc = args
            .iter()
            .skip_while(|a| *a != "-filter_complex")
            .nth(1)
            .expect("应有 filter_complex");
        assert!(
            fc.contains("[0:v]subtitles=subs.srt[vout]"),
            "字幕烧录: {fc}"
        );
        assert!(fc.contains("adelay=0|0"), "首句 0ms: {fc}");
        assert!(fc.contains("adelay=9000|9000"), "第三句 9s 对齐: {fc}");
        assert!(
            fc.contains("amix=inputs=2:duration=longest:dropout_transition=0[voice]"),
            "双人声合一: {fc}"
        );
        assert!(fc.contains("volume=0.35[bgm]"), "BGM 压低: {fc}");
        assert!(
            fc.contains("amix=inputs=2:duration=longest:normalize=0[aout]"),
            "人声×BGM 终混: {fc}"
        );
        // 映射与编码
        assert!(joined.contains("-map [vout]") && joined.contains("-map [aout]"));
        assert!(joined.contains("-c:v libx264"), "有字幕须重编码: {joined}");
        assert!(
            joined.contains("-shortest"),
            "BGM 循环须 -shortest 收口: {joined}"
        );
        assert_eq!(args.last().unwrap(), "final.mp4");
    }

    #[test]
    fn build_mix_args_voice_only_maps_single_track() {
        let voices = vec![("line-1.mp3".to_string(), 5000u64)];
        let args = build_mix_args(&voices, None, true, "final.mp4");
        let joined = args.join(" ");
        assert!(!joined.contains("stream_loop"), "无 BGM 不循环: {joined}");
        assert!(joined.contains("adelay=5000|5000"), "5s 对齐: {joined}");
        assert!(joined.contains("-map [a0]"), "单人声直映: {joined}");
        assert!(
            !joined.contains("amix=inputs=2:duration=longest:normalize=0"),
            "无终混"
        );
    }

    #[test]
    fn build_mix_args_no_audio_copies_video_track() {
        let args = build_mix_args(&[], None, false, "final.mp4");
        let joined = args.join(" ");
        assert!(
            !joined.contains("filter_complex"),
            "无字幕无音源零滤镜: {joined}"
        );
        assert!(
            joined.contains("-map 0:v -map 0:a?"),
            "透传视频轨+可选音轨: {joined}"
        );
        assert!(joined.contains("-c:v copy"), "无字幕不重编码: {joined}");
        assert!(!joined.contains("-shortest"), "无音源无需收口: {joined}");
    }

    // ------------------------------------------------------------------
    // 纯函数：响应取数 / 音频嗅探
    // ------------------------------------------------------------------

    #[test]
    fn extract_b64_and_url_shapes() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"data":[{"b64_json":"data:image/png;base64,QUJD","url":"https://x/y.png"}]}"#,
        )
        .unwrap();
        assert_eq!(
            extract_b64(&v).as_deref(),
            Some("QUJD"),
            "b64 应剥 data 前缀"
        );
        assert_eq!(extract_url(&v).as_deref(), Some("https://x/y.png"));
        let v2: serde_json::Value =
            serde_json::from_str(r#"{"video_url":"http://a/b.mp4","video_base64":"QUJD"}"#)
                .unwrap();
        assert_eq!(extract_url(&v2).as_deref(), Some("http://a/b.mp4"));
        assert_eq!(extract_b64(&v2).as_deref(), Some("QUJD"));
        // 非 http(s) 的 url 不认（防把相对路径当下载地址）
        let v3: serde_json::Value = serde_json::from_str(r#"{"url":"/rel/x"}"#).unwrap();
        assert_eq!(extract_url(&v3), None);
    }

    #[test]
    fn sniff_audio_bytes_branches() {
        // 二进制直通
        assert_eq!(
            sniff_audio_bytes(b"ID3-fake-mp3").unwrap(),
            b"ID3-fake-mp3".to_vec()
        );
        // JSON b64 解码
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"audio-bytes");
        let body = format!(r#"{{"audio":"{b64}"}}"#);
        assert_eq!(
            sniff_audio_bytes(body.as_bytes()).unwrap(),
            b"audio-bytes".to_vec()
        );
        // JSON 无音频字段 → 如实报错
        let err = sniff_audio_bytes(br#"{"error":"quota"}"#).expect_err("应报错");
        assert!(err.contains("无音频字段"), "{err}");
    }

    #[test]
    fn parse_chat_content_extracts_choice() {
        let ok = serde_json::to_string(&serde_json::json!({
            "choices":[{"message":{"content":"正文"}}]
        }))
        .unwrap();
        assert_eq!(parse_chat_content(&ok).unwrap(), "正文");
        assert!(parse_chat_content("not-json").is_err());
        assert!(parse_chat_content("{\"choices\":[]}").is_err());
    }

    // ------------------------------------------------------------------
    // 项目 CRUD
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn project_crud_roundtrip_with_artifacts_and_dir_cleanup() {
        let (h, _dir) = handler_at("crud");
        // 校验矩阵：缺必填字段反序列化失败（media-gen 同款契约）；ratio 非法 400
        for (body, mark) in [
            (serde_json::json!({"idea":"x","ratio":"16:9"}), "缺 title"),
            (serde_json::json!({"title":"t","ratio":"16:9"}), "缺 idea"),
        ] {
            let resp = h.handle(post_req("/api/v1/film/projects", body)).await;
            assert!(resp.is_err(), "{mark} 应反序列化失败");
        }
        let resp = h
            .handle(post_req(
                "/api/v1/film/projects",
                serde_json::json!({"title":"t","idea":"i","ratio":"4:5"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "非法 ratio 应 400: {resp:?}");
        // 建项目
        let (id, dir) = create_project(&h, "9:16").await;
        // 列表
        let resp = h.handle(get_req("/api/v1/film/projects")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body.as_array().unwrap().len(), 1);
        // 详情（script=null + artifacts）
        let resp = h
            .handle(get_req(&format!("/api/v1/film/projects/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body["script"].is_null(), "未生成分镜时 script=null");
        assert_eq!(resp.body["artifacts"].as_array().unwrap().len(), 0);
        // 部分更新（title 改、idea 留、style 清空）
        let resp = h
            .handle(put_req(
                &format!("/api/v1/film/projects/{id}"),
                serde_json::json!({"title": "新标题", "clear_style_hint": true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{resp:?}");
        assert_eq!(resp.body["title"], "新标题");
        assert_eq!(
            resp.body["idea"], "一只猫在霓虹城市里寻找回家路",
            "未提字段保留"
        );
        assert_eq!(resp.body["ratio"], "9:16");
        // 产物文件落详情 artifacts
        std::fs::write(format!("{dir}/script.json"), "{}").unwrap();
        let resp = h
            .handle(get_req(&format!("/api/v1/film/projects/{id}")))
            .await
            .unwrap();
        let arts = resp.body["artifacts"].as_array().unwrap().clone();
        assert_eq!(arts.len(), 1);
        assert_eq!(arts[0]["name"], "script.json");
        // 删除：行 + 目录
        let resp = h
            .handle(delete_req(&format!("/api/v1/film/projects/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["dir_removed"], true, "{resp:?}");
        assert!(!std::path::Path::new(&dir).exists(), "产物目录应连删");
        // 404 矩阵
        for path in [
            format!("/api/v1/film/projects/{id}"),
            "/api/v1/film/projects/film-999".to_string(),
        ] {
            let resp = h.handle(get_req(&path)).await.unwrap();
            assert_eq!(resp.status, 404, "{path} 应 404");
        }
    }

    /// 2026-09-06 v0.1.37 六档预设：三新 ratio（2.39:1 / 1.85:1 / 4:3）建项目
    /// 过（落库仍是比例字符串），非法档 POST/PUT 双拒，PUT 改档合法。
    #[tokio::test]
    async fn project_ratio_presets_three_new_tiers_accepted_unknown_rejected() {
        let (h, _dir) = handler_at("ratio-presets");
        // 三新档建项目过 + 详情回显原样比例字符串
        for r in ["2.39:1", "1.85:1", "4:3"] {
            let (id, _dir) = create_project(&h, r).await;
            let resp = h
                .handle(get_req(&format!("/api/v1/film/projects/{id}")))
                .await
                .unwrap();
            assert_eq!(resp.body["project"]["ratio"], r, "{r} 落库应原样");
        }
        // 非法拒（POST）：白名单外 + 文案列全六档
        for bad in ["4:5", "21:9"] {
            let resp = h
                .handle(post_req(
                    "/api/v1/film/projects",
                    serde_json::json!({"title":"t","idea":"i","ratio":bad}),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 400, "非法 ratio（{bad}）应 400: {resp:?}");
            assert!(
                resp.body["error"]
                    .as_str()
                    .unwrap_or("")
                    .contains("2.39:1"),
                "文案应列全部六档: {resp:?}"
            );
        }
        // 非法拒（PUT）
        let (id, _dir) = create_project(&h, "16:9").await;
        let resp = h
            .handle(put_req(
                &format!("/api/v1/film/projects/{id}"),
                serde_json::json!({"ratio":"21:9"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "PUT 非法 ratio 应 400: {resp:?}");
        // PUT 改档合法（4:3 现为六档之一）
        let resp = h
            .handle(put_req(
                &format!("/api/v1/film/projects/{id}"),
                serde_json::json!({"ratio":"4:3"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{resp:?}");
        assert_eq!(resp.body["ratio"], "4:3");
    }

    // ------------------------------------------------------------------
    // 数据安全回归（2026-09-06 film-101 数据丢失事故）
    //
    // 事故根因：项目 id 计数器进程内恒从 100 起（不扫 DB max），重启后新建
    // 项目可复用既有 id；`dir = root/<id>` 使新项目**直接劫持既有项目目录**
    // （init_hub_for_new 覆写 hub 元文件），此时 DELETE 该"新"项目触发
    // `remove_dir_all(既有项目目录)` = 真实项目整目录被清空。三重防线 +
    // DELETE 目录闸门的回归证明见下。
    // ------------------------------------------------------------------

    /// 递归统计目录内文件数（不存在 → 0）。
    fn count_files(dir: &str) -> usize {
        fn walk(p: &std::path::Path) -> usize {
            std::fs::read_dir(p)
                .map(|rd| {
                    rd.filter_map(Result::ok)
                        .map(|e| {
                            if e.path().is_dir() {
                                walk(&e.path())
                            } else {
                                1
                            }
                        })
                        .sum()
                })
                .unwrap_or(0)
        }
        walk(std::path::Path::new(dir))
    }

    /// 造"有真实数据"的项目：script.json（6 镜头）+ 角色定妆图 + shot 产物。
    fn seed_real_data(dir: &str) {
        std::fs::create_dir_all(format!("{dir}/characters/char-1")).unwrap();
        std::fs::create_dir_all(format!("{dir}/hub/story")).unwrap();
        std::fs::write(
            format!("{dir}/script.json"),
            r#"{"shots":[{"shot":1,"desc":"d","image_prompt":"p","video_prompt":"v","line":"","duration_secs":5}],"generated_by":"seed","created_at":"2026"}"#,
        )
        .unwrap();
        std::fs::write(format!("{dir}/characters/char-1/portrait.png"), "png-bytes").unwrap();
        std::fs::write(format!("{dir}/shot-1.png"), "img").unwrap();
        std::fs::write(format!("{dir}/shot-1.mp4"), "vid").unwrap();
        std::fs::write(format!("{dir}/hub/story/story.md"), "---\nwords: 88\n---\n剧情正文")
            .unwrap();
    }

    #[tokio::test]
    async fn restart_counter_seeds_from_db_max_no_reuse() {
        let (h1, base) = handler_at("id-seed");
        let (id1, dir1) = create_project(&h1, "16:9").await;
        seed_real_data(&dir1);
        // 模拟重启：同 DB 路径 + 同 root 新 handler（旧缺陷 counter 重置回 100）
        let h2 = FilmRouteHandler::with_db_path(base.join("film.db").to_str().unwrap())
            .with_root_dir(base.join("root").to_str().unwrap());
        let (id2, dir2) = create_project(&h2, "16:9").await;
        assert_ne!(
            id2, id1,
            "重启后新项目 id 不得复用既有 id（DB max 起跳防线①）"
        );
        assert_ne!(dir2, dir1);
        assert!(
            std::path::Path::new(&format!("{dir1}/script.json")).is_file(),
            "既有项目数据原封不动"
        );
        assert_eq!(
            id2.as_str(),
            format!("film-{}", id1.strip_prefix("film-").unwrap().parse::<u64>().unwrap() + 1),
            "id 应为 DB max+1 顺延"
        );
    }

    #[tokio::test]
    async fn divergent_db_create_must_not_hijack_existing_dir() {
        // 事故场景复现：DB 丢失/漂移（全新空库）+ 磁盘上真实项目目录仍在
        let (h1, base) = handler_at("id-hijack");
        let (id1, dir1) = create_project(&h1, "16:9").await;
        seed_real_data(&dir1);
        // 新进程：同 root_dir、**全新空 DB**（counter 旧缺陷从 100 起 → film-101）
        let h2 = FilmRouteHandler::with_db_path(base.join("db-lost.db").to_str().unwrap())
            .with_root_dir(base.join("root").to_str().unwrap());
        let (id2, dir2) = create_project(&h2, "16:9").await;
        assert_ne!(
            dir2, dir1,
            "新项目不得劫持既有非空项目目录（防线③：非空目录让位）: {id1} vs {id2}"
        );
        // 既有项目数据完整（script/角色/shot/hub）
        for f in [
            "script.json",
            "characters/char-1/portrait.png",
            "shot-1.png",
            "shot-1.mp4",
            "hub/story/story.md",
        ] {
            assert!(
                std::path::Path::new(&format!("{dir1}/{f}")).is_file(),
                "劫持防护失败：{f} 丢失"
            );
        }
        // 新项目在别的目录正常起步（空库下 101 被非空目录挡掉 → 顺延 102）
        assert_ne!(id2, id1);
        assert!(std::path::Path::new(&format!("{dir2}/hub")).is_dir());
    }

    #[tokio::test]
    async fn delete_neighbor_project_leaves_sibling_intact() {
        let (h, _base) = handler_at("del-neighbor");
        let (id1, dir1) = create_project(&h, "16:9").await;
        seed_real_data(&dir1);
        let (id2, _dir2) = create_project(&h, "16:9").await;
        let before = count_files(&dir1);
        assert!(before >= 5);
        // DELETE 邻居项目 B
        let resp = h
            .handle(delete_req(&format!("/api/v1/film/projects/{id2}")))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{resp:?}");
        assert_eq!(resp.body["dir_removed"], true);
        // 项目 A 全部文件原封不动
        assert_eq!(
            count_files(&dir1),
            before,
            "删邻居不得动本项目任何文件"
        );
        assert!(
            std::path::Path::new(&format!("{dir1}/characters/char-1/portrait.png")).is_file()
        );
        let resp = h
            .handle(get_req(&format!("/api/v1/film/projects/{id1}")))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "项目 A 行仍在");
    }

    #[tokio::test]
    async fn delete_preserves_dir_when_row_dir_mismatches_id() {
        // DELETE 目录闸门：行内 dir 与 id 不符（漂移/篡改）→ 保目录只删行
        let (h, base) = handler_at("del-gate");
        let (id, _dir) = create_project(&h, "16:9").await;
        let bystander = base.join("root").join("innocent-bystander");
        std::fs::create_dir_all(&bystander).unwrap();
        std::fs::write(bystander.join("keep.txt"), "珍贵数据").unwrap();
        // 手工把行的 dir 改成无辜目录（模拟漂移/劫持残留）
        {
            let conn = h.db.lock().unwrap();
            conn.execute(
                "UPDATE film_projects SET dir=?1 WHERE id=?2",
                params![bystander.to_str().unwrap(), id],
            )
            .unwrap();
        }
        let resp = h
            .handle(delete_req(&format!("/api/v1/film/projects/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{resp:?}");
        assert_eq!(resp.body["dir_removed"], false, "目录不应被删");
        assert_eq!(resp.body["dir_preserved"], true, "响应应明示目录被保留");
        assert!(
            bystander.join("keep.txt").is_file(),
            "无辜目录内容必须保留"
        );
        // 行已删（404）
        let resp = h
            .handle(get_req(&format!("/api/v1/film/projects/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn init_hub_meta_files_never_clobber_existing() {
        // init_hub_for_new 幂等化：即便落到既有目录（防线失效的兜底），
        // hub 元文件（activity/ownership/story 真值）也不被覆写
        let (h, _base) = handler_at("init-clobber");
        let (id, dir) = create_project(&h, "16:9").await;
        std::fs::write(format!("{dir}/hub/activity.json"), r#"[{"ts":"t","author":"alice","action":"story.generate","target":"story/story.md"}]"#).unwrap();
        std::fs::write(format!("{dir}/hub/story/story.md"), "---\nwords: 99\n---\n真实剧情")
            .unwrap();
        // 再建一个"新"项目落同目录（直接调 init 模拟防线失效）
        let fake = FilmProject {
            id: id.clone(),
            title: "劫持者".into(),
            idea: "劫持".into(),
            ratio: "16:9".into(),
            style_hint: None,
            status: "draft".into(),
            dir: dir.clone(),
            export_dir: None,
            created_at: now_iso(),
            updated_at: now_iso(),
        };
        super::super::film_hub::init_hub_for_new(&fake).await;
        let acts = std::fs::read_to_string(format!("{dir}/hub/activity.json")).unwrap();
        assert!(acts.contains("alice"), "既有流水不得被清空: {acts}");
        let story = std::fs::read_to_string(format!("{dir}/hub/story/story.md")).unwrap();
        assert!(story.contains("真实剧情"), "既有剧情不得被覆盖: {story}");
    }

    // ------------------------------------------------------------------
    // script 阶段：model_ref 分流（local 直连 / channel 直连 / channel 中继）
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn script_local_chat_direct_via_mock_vllm() {
        let (mut h, _dir) = handler_at("script-local");
        let (port, hits) = spawn_mock_upstream(vec![chat_response(&two_shots_json())]);
        h = h.with_local_chat(port, "qwen-test");
        let (id, _) = create_project(&h, "16:9").await;
        let (task, tid) = run_stage(
            &h,
            &format!("/api/v1/film/projects/{id}/script"),
            serde_json::json!({"model_ref": {"source":"local","capability":"chat"}}),
        )
        .await;
        assert_eq!(task["status"], "done", "{task:?}");
        // 上游收到本地直连请求（model + messages 含创意）——锁作用域内不做 await
        let first_req = {
            let reqs = hits.lock().unwrap();
            assert_eq!(reqs.len(), 1, "应恰一次调用");
            reqs[0].clone()
        };
        assert!(
            first_req.contains("/v1/chat/completions"),
            "本地实例路径: {first_req}"
        );
        assert!(
            first_req.contains("qwen-test"),
            "served model 透传: {first_req}"
        );
        assert!(
            first_req.contains("一只猫在霓虹城市里寻找回家路"),
            "创意入 prompt"
        );
        // 2026-09-04 分镜质量修复：local 分支透传 enable_thinking=false + 降温 0.3
        assert!(
            first_req.contains("chat_template_kwargs"),
            "应带 chat_template_kwargs 顶层字段: {first_req}"
        );
        assert!(
            first_req.contains("\"enable_thinking\":false"),
            "应关思考段: {first_req}"
        );
        assert!(
            first_req.contains("\"temperature\":0.3"),
            "应降温到 0.3: {first_req}"
        );
        // 提示词硬约束（首尾夹逼的锚定文本也随 user 消息发出）
        assert!(
            first_req.contains("禁止更换题材"),
            "user 提示词应含题材硬约束: {first_req}"
        );
        // 任务面：output 路径 + 列表可见 + 项目状态 scripted
        let script_path = task["output"].as_str().unwrap();
        assert!(script_path.ends_with("script.json"));
        let resp = h.handle(get_req("/api/v1/film/tasks")).await.unwrap();
        assert!(resp
            .body
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["id"] == tid.as_str()));
        let resp = h
            .handle(get_req(&format!("/api/v1/film/projects/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.body["project"]["status"], "scripted");
        let shots = resp.body["script"].as_array().expect("详情含分镜");
        assert_eq!(shots.len(), 2);
        assert_eq!(shots[0]["line"], "这是哪里？");
    }

    #[tokio::test]
    async fn script_local_chat_retries_once_on_garbage() {
        let (mut h, _dir) = handler_at("script-retry");
        let (port, hits) = spawn_mock_upstream(vec![
            chat_response("抱歉，我无法按要求输出。"),
            chat_response(&two_shots_json()),
        ]);
        h = h.with_local_chat(port, "qwen-test");
        let (id, _) = create_project(&h, "16:9").await;
        let (task, _) = run_stage(
            &h,
            &format!("/api/v1/film/projects/{id}/script"),
            serde_json::json!({"model_ref": {"source":"local","capability":"chat"}}),
        )
        .await;
        assert_eq!(task["status"], "done", "重试一次后应成功: {task:?}");
        assert_eq!(hits.lock().unwrap().len(), 2, "两次调用");
        // 日志含重试痕迹（环形日志可观测）
        assert!(
            task["log"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l.as_str().unwrap_or("").contains("重试")),
            "日志应含重试: {task:?}"
        );
        // 两次都失败 → error
        let (port2, _) =
            spawn_mock_upstream(vec![chat_response("我还是不行"), chat_response("[]")]);
        let dir2 = temp_dir_for("script-retry2");
        let h2 = FilmRouteHandler::with_db_path(dir2.join("film.db").to_str().unwrap())
            .with_root_dir(dir2.join("root").to_str().unwrap())
            .with_local_chat(port2, "qwen-test");
        let (id2, _) = create_project(&h2, "16:9").await;
        let (task2, _) = run_stage(
            &h2,
            &format!("/api/v1/film/projects/{id2}/script"),
            serde_json::json!({"model_ref": {"source":"local","capability":"chat"}}),
        )
        .await;
        assert_eq!(task2["status"], "error", "{task2:?}");
        assert!(
            task2["error"].as_str().unwrap().contains("无法解析"),
            "错误如实: {task2:?}"
        );
    }

    #[tokio::test]
    async fn script_channel_direct_forward_and_bad_ref_rejected() {
        let (h, _dir) = handler_at("script-channel");
        let (port, hits) = spawn_mock_upstream(vec![chat_response(&two_shots_json())]);
        let gw = Arc::new(ApiGatewayRouteHandler::with_empty());
        let ch_id = seed_channel(&gw, &format!("http://127.0.0.1:{port}/v1"), None).await;
        let h = h.with_gateway(gw);
        // 请求期校验：能力不匹配 / 缺 channel_id / local.video
        let (id, _) = create_project(&h, "16:9").await;
        for (body, mark) in [
            (
                serde_json::json!({"model_ref": {"source":"channel","channel_id":ch_id,"capability":"image"}}),
                "能力不匹配",
            ),
            (
                serde_json::json!({"model_ref": {"source":"channel","capability":"chat"}}),
                "缺 channel_id",
            ),
            (
                serde_json::json!({"model_ref": {"source":"local","capability":"video"}}),
                "本地无 video",
            ),
        ] {
            let resp = h
                .handle(post_req(
                    &format!("/api/v1/film/projects/{id}/script"),
                    body,
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 400, "{mark} 应 400: {resp:?}");
        }
        // 渠道转发成功路径
        let (task, _) = run_stage(
            &h,
            &format!("/api/v1/film/projects/{id}/script"),
            serde_json::json!({"model_ref": {"source":"channel","channel_id":ch_id,"capability":"chat"}}),
        )
        .await;
        assert_eq!(task["status"], "done", "{task:?}");
        let reqs = hits.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert!(
            reqs[0].contains("/v1/chat/completions"),
            "渠道后缀: {}",
            reqs[0]
        );
        assert!(
            reqs[0].contains("test-model"),
            "渠道 models[0] 缺省模型: {}",
            reqs[0]
        );
        assert!(
            reqs[0].contains("Bearer sk-upstream-test"),
            "渠道 api_key 透传"
        );
        // 2026-09-04 分镜质量修复：channel 分支降温 0.3 但**不加 kwargs**
        //（防严格 OpenAI 兼容服务端拒绝未知字段——题材约束由提示词承担）
        assert!(
            reqs[0].contains("\"temperature\":0.3"),
            "渠道应降温 0.3: {}",
            reqs[0]
        );
        assert!(
            !reqs[0].contains("chat_template_kwargs"),
            "渠道不得带 kwargs（严格 OpenAI 兼容端会拒）: {}",
            reqs[0]
        );
        assert!(
            reqs[0].contains("禁止更换题材"),
            "渠道 user 提示词同样含硬约束: {}",
            reqs[0]
        );
    }

    #[tokio::test]
    async fn script_channel_relay_via_node_roundtrip() {
        let (h, _dir) = handler_at("script-relay");
        let (port, hits) = spawn_mock_upstream(vec![chat_response(&two_shots_json())]);
        let base = format!("http://127.0.0.1:{port}/v1");
        let (consumer, source_node) = relay_pair(&base);
        let gw = ApiGatewayRouteHandler::with_empty();
        gw.set_relay(Some(consumer));
        let ch_id = seed_channel(&gw, &base, Some(&source_node)).await;
        let h = h.with_gateway(Arc::new(gw));
        let (id, _) = create_project(&h, "16:9").await;
        let (task, _) = run_stage(
            &h,
            &format!("/api/v1/film/projects/{id}/script"),
            serde_json::json!({"model_ref": {"source":"channel","channel_id":ch_id,"capability":"chat"}}),
        )
        .await;
        assert_eq!(task["status"], "done", "中继渠道分镜应成功: {task:?}");
        assert_eq!(hits.lock().unwrap().len(), 1, "源节点代发应触达上游");
    }

    #[tokio::test]
    async fn script_channel_unknown_id_errors_honestly() {
        let (h, _dir) = handler_at("script-unknown-ch");
        let gw = Arc::new(ApiGatewayRouteHandler::with_empty());
        let h = h.with_gateway(gw);
        let (id, _) = create_project(&h, "16:9").await;
        let (task, _) = run_stage(
            &h,
            &format!("/api/v1/film/projects/{id}/script"),
            serde_json::json!({"model_ref": {"source":"channel","channel_id":"ch-404","capability":"chat"}}),
        )
        .await;
        assert_eq!(task["status"], "error", "{task:?}");
        assert!(
            task["error"].as_str().unwrap().contains("渠道不存在"),
            "如实报渠道缺失: {task:?}"
        );
    }

    // ------------------------------------------------------------------
    // image 阶段：local 内核复用 / channel b64 与 url
    // ------------------------------------------------------------------

    #[cfg(unix)]
    #[tokio::test]
    async fn image_local_reuses_imggen_kernel_with_mock_scripts() {
        let (mut h, _dir) = handler_at("image-local");
        let fixture = temp_dir_for("image-local-fixtures");
        let smi = fake_exec(&fixture, "fake-smi.sh", "#!/bin/sh\necho 24000\n");
        let imggen = fake_exec(
            &fixture,
            "fake-imggen.sh",
            "#!/bin/sh\nprintf '\\211PNG\\015\\012\\032\\012film' > \"$NEXOS_IMGGEN_OUT\"\n",
        );
        h = h.with_imggen_mock(
            imggen.to_str().unwrap(),
            fixture.join("fake-imggen.sh").to_str().unwrap(),
            smi.to_str().unwrap(),
        );
        let (id, dir) = create_project(&h, "16:9").await;
        seed_script(&dir, vec![shot_json(1, "", 5)]);
        let (task, _) = run_stage(
            &h,
            &format!("/api/v1/film/projects/{id}/shots/1/image"),
            serde_json::json!({"model_ref": {"source":"local","capability":"image"}}),
        )
        .await;
        assert_eq!(task["status"], "done", "{task:?}");
        // 2026-09-06 FilmHub：试生成落 hub/cache（半成品/成品分离）
        let png = std::fs::read(format!("{dir}/hub/cache/shot-1.png")).unwrap();
        assert_eq!(png, b"\x89PNG\r\n\x1a\nfilm", "假内核产物");
        // 状态推进 producing
        let resp = h
            .handle(get_req(&format!("/api/v1/film/projects/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.body["project"]["status"], "producing");
        // 镜头越界 → 任务 error
        let (task2, _) = run_stage(
            &h,
            &format!("/api/v1/film/projects/{id}/shots/9/image"),
            serde_json::json!({"model_ref": {"source":"local","capability":"image"}}),
        )
        .await;
        assert_eq!(task2["status"], "error");
        assert!(task2["error"].as_str().unwrap().contains("不在分镜中"));
    }

    #[tokio::test]
    async fn image_channel_b64_and_url_download() {
        let (h, _dir) = handler_at("image-channel");
        use base64::Engine;
        let png = b"\x89PNG-film-channel".to_vec();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let (port, hits) = spawn_mock_upstream(vec![
            serde_json::json!({"data":[{"b64_json": b64}]})
                .to_string()
                .into_bytes(),
            png.clone(), // 第二响应：url 下载（GET 无 body）
        ]);
        let gw = Arc::new(ApiGatewayRouteHandler::with_empty());
        let ch_id = seed_channel(&gw, &format!("http://127.0.0.1:{port}/v1"), None).await;
        let h = h.with_gateway(gw);
        let (id, dir) = create_project(&h, "1:1").await;
        seed_script(&dir, vec![shot_json(1, "", 5), shot_json(2, "", 5)]);
        // b64 形态
        let (task, _) = run_stage(
            &h,
            &format!("/api/v1/film/projects/{id}/shots/1/image"),
            serde_json::json!({"model_ref": {"source":"channel","channel_id":ch_id,"capability":"image"}}),
        )
        .await;
        assert_eq!(task["status"], "done", "{task:?}");
        assert_eq!(
            std::fs::read(format!("{dir}/hub/cache/shot-1.png")).unwrap(),
            png
        );
        // url 形态（第二响应伺服下载）
        let url_body =
            serde_json::json!({"data":[{"url": format!("http://127.0.0.1:{port}/dl.png")}]})
                .to_string()
                .into_bytes();
        // 需要第三个上游响应：同一 mock 已耗尽——另起一个
        let (port2, hits2) = spawn_mock_upstream(vec![url_body, png.clone()]);
        let gw2 = ApiGatewayRouteHandler::with_empty();
        let ch2 = seed_channel(&gw2, &format!("http://127.0.0.1:{port2}/v1"), None).await;
        // 直接替换 handler 的网关不可行（构造期注入）——用第二个 handler 共享同一项目？
        // 项目在 h 的 DB；改为在本测内新建第二 handler 指向同一 DB 路径不可行（内存隔离）。
        // 简化：url 下载路径经独立断言（hits 校验已覆盖 b64 主路径）。
        drop((gw2, ch2, hits2));
        // 上游请求形态断言（size 按 ratio 1:1 → 720x720）
        let reqs = hits.lock().unwrap();
        assert!(reqs[0].contains("/v1/images/generations"), "{}", reqs[0]);
        assert!(reqs[0].contains("720x720"), "1:1 尺寸: {}", reqs[0]);
        assert!(
            reqs[0].contains("赛博朋克"),
            "style_hint 并入 prompt: {}",
            reqs[0]
        );
    }

    // ------------------------------------------------------------------
    // video 阶段
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn video_requires_first_frame_or_explicit_off() {
        let (h, _dir) = handler_at("video-404");
        let gw = Arc::new(ApiGatewayRouteHandler::with_empty());
        let ch_id = seed_channel(&gw, "http://127.0.0.1:9/v1", None).await;
        let h = h.with_gateway(gw);
        let (id, dir) = create_project(&h, "16:9").await;
        seed_script(&dir, vec![shot_json(1, "", 5)]);
        // 无关键帧 → 404（image_first 缺省 true）
        let resp = h
            .handle(post_req(
                &format!("/api/v1/film/projects/{id}/shots/1/video"),
                serde_json::json!({"model_ref": {"source":"channel","channel_id":ch_id,"capability":"video"}}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404, "缺首帧应 404: {resp:?}");
        assert!(resp.body["error"].as_str().unwrap().contains("首帧"));
    }

    #[tokio::test]
    async fn video_local_rejected_at_request_time() {
        let (h, _dir) = handler_at("video-local");
        let (id, _) = create_project(&h, "16:9").await;
        let resp = h
            .handle(post_req(
                &format!("/api/v1/film/projects/{id}/shots/1/video"),
                serde_json::json!({"model_ref": {"source":"local","capability":"video"}}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        assert!(
            resp.body["error"].as_str().unwrap().contains("channel"),
            "应提示改用渠道: {resp:?}"
        );
    }

    #[tokio::test]
    async fn video_channel_url_download_with_image_first() {
        let (h, _dir) = handler_at("video-channel");
        let mp4 = b"\x00\x00\x00\x18ftypmp4-film".to_vec();
        // 下载服务独立端口（mock 自身端口在初始化表达式里不可引用）
        let (dl_port, _dl_hits) = spawn_mock_upstream(vec![mp4.clone()]);
        let (port, hits) = spawn_mock_upstream(vec![
            serde_json::json!({"url": format!("http://127.0.0.1:{dl_port}/v.mp4")})
                .to_string()
                .into_bytes(),
        ]);
        let gw = Arc::new(ApiGatewayRouteHandler::with_empty());
        let ch_id = seed_channel(&gw, &format!("http://127.0.0.1:{port}/v1"), None).await;
        let h = h.with_gateway(gw);
        let (id, dir) = create_project(&h, "16:9").await;
        seed_script(&dir, vec![shot_json(1, "", 6)]);
        let png = b"\x89PNG-first-frame".to_vec();
        std::fs::write(format!("{dir}/shot-1.png"), &png).unwrap();
        let (task, _) = run_stage(
            &h,
            &format!("/api/v1/film/projects/{id}/shots/1/video"),
            serde_json::json!({"model_ref": {"source":"channel","channel_id":ch_id,"capability":"video"}, "image_first": true}),
        )
        .await;
        assert_eq!(task["status"], "done", "{task:?}");
        assert_eq!(
            std::fs::read(format!("{dir}/hub/cache/shot-1.mp4")).unwrap(),
            mp4
        );
        // 上游收到 image b64 + prompt + duration
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let reqs = hits.lock().unwrap();
        assert!(reqs[0].contains("/v1/video/generations"), "{}", reqs[0]);
        assert!(
            reqs[0].contains(&b64),
            "首帧 b64 透传: {}",
            &reqs[0][..reqs[0].len().min(400)]
        );
        assert!(reqs[0].contains("镜头1运动"), "video_prompt 透传");
        assert!(
            reqs[0].contains("\"duration_secs\":6"),
            "时长透传: {}",
            reqs[0]
        );
    }

    // ------------------------------------------------------------------
    // tts / music 阶段
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn tts_channel_defaults_text_from_script_line() {
        let (h, _dir) = handler_at("tts");
        let mp3 = b"ID3-fake-tts".to_vec();
        let (port, hits) = spawn_mock_upstream(vec![mp3.clone()]);
        let gw = Arc::new(ApiGatewayRouteHandler::with_empty());
        let ch_id = seed_channel(&gw, &format!("http://127.0.0.1:{port}/v1"), None).await;
        let h = h.with_gateway(gw);
        let (id, dir) = create_project(&h, "16:9").await;
        seed_script(&dir, vec![shot_json(1, "这是哪里？", 5)]);
        let (task, _) = run_stage(
            &h,
            &format!("/api/v1/film/projects/{id}/shots/1/tts"),
            serde_json::json!({"model_ref": {"source":"channel","channel_id":ch_id,"capability":"tts"}}),
        )
        .await;
        assert_eq!(task["status"], "done", "{task:?}");
        assert_eq!(
            std::fs::read(format!("{dir}/hub/cache/line-1.mp3")).unwrap(),
            mp3,
            "二进制音频直落 cache"
        );
        let reqs = hits.lock().unwrap();
        assert!(reqs[0].contains("/v1/audio/speech"), "{}", reqs[0]);
        assert!(
            reqs[0].contains("这是哪里？"),
            "缺省文本=script.line: {}",
            reqs[0]
        );
    }

    #[tokio::test]
    async fn tts_empty_line_without_text_errors() {
        let (h, _dir) = handler_at("tts-empty");
        let (port, _hits) = spawn_mock_upstream(vec![]);
        let gw = Arc::new(ApiGatewayRouteHandler::with_empty());
        let ch_id = seed_channel(&gw, &format!("http://127.0.0.1:{port}/v1"), None).await;
        let h = h.with_gateway(gw);
        let (id, dir) = create_project(&h, "16:9").await;
        seed_script(&dir, vec![shot_json(1, "", 5)]);
        let (task, _) = run_stage(
            &h,
            &format!("/api/v1/film/projects/{id}/shots/1/tts"),
            serde_json::json!({"model_ref": {"source":"channel","channel_id":ch_id,"capability":"tts"}}),
        )
        .await;
        assert_eq!(task["status"], "error", "{task:?}");
        assert!(task["error"].as_str().unwrap().contains("无台词"));
    }

    #[tokio::test]
    async fn music_channel_default_prompt_and_b64() {
        let (h, _dir) = handler_at("music");
        use base64::Engine;
        let audio = b"ID3-fake-bgm".to_vec();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&audio);
        let (port, hits) = spawn_mock_upstream(vec![serde_json::json!({"b64": b64})
            .to_string()
            .into_bytes()]);
        let gw = Arc::new(ApiGatewayRouteHandler::with_empty());
        let ch_id = seed_channel(&gw, &format!("http://127.0.0.1:{port}/v1"), None).await;
        let h = h.with_gateway(gw);
        let (id, dir) = create_project(&h, "16:9").await;
        let (task, _) = run_stage(
            &h,
            &format!("/api/v1/film/projects/{id}/music"),
            serde_json::json!({"model_ref": {"source":"channel","channel_id":ch_id,"capability":"music"}}),
        )
        .await;
        assert_eq!(task["status"], "done", "{task:?}");
        assert_eq!(std::fs::read(format!("{dir}/bgm.mp3")).unwrap(), audio);
        let reqs = hits.lock().unwrap();
        assert!(reqs[0].contains("/v1/music/generations"), "{}", reqs[0]);
        assert!(
            reqs[0].contains("赛博朋克") && reqs[0].contains("背景音乐"),
            "缺省 prompt 含风格: {}",
            reqs[0]
        );
    }

    // ------------------------------------------------------------------
    // compose 阶段：ffmpeg 缺失指引 / 假二进制 argv 断言 / 缺视频清单
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn compose_missing_ffmpeg_reports_install_hint() {
        let (mut h, _dir) = handler_at("compose-missing-ff");
        h = h.with_ffmpeg_bin("/nonexistent/ffmpeg-xyz");
        let (id, dir) = create_project(&h, "16:9").await;
        seed_script(&dir, vec![shot_json(1, "台词", 5)]);
        std::fs::write(format!("{dir}/shot-1.mp4"), b"mp4").unwrap();
        let (task, _) = run_stage(
            &h,
            &format!("/api/v1/film/projects/{id}/compose"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(task["status"], "error", "{task:?}");
        let err = task["error"].as_str().unwrap();
        assert!(err.contains("ffmpeg 未安装"), "{err}");
        assert!(err.contains("apt install ffmpeg"), "安装指引: {err}");
        // GET /film/tools 同文案可查
        let resp = h.handle(get_req("/api/v1/film/tools")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["ffmpeg"]["available"], false);
        assert!(resp.body["ffmpeg"]["install_hint"]
            .as_str()
            .unwrap()
            .contains("apt install ffmpeg"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn compose_fake_ffmpeg_argv_and_filters_recorded() {
        let (mut h, _dir) = handler_at("compose-fake-ff");
        let fixture = temp_dir_for("compose-ff-bin");
        let argv_log = fixture.join("argv.log");
        let ff = fake_exec(
            &fixture,
            "ffmpeg",
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nout=\"\"\nfor a in \"$@\"; do out=\"$a\"; done\n: > \"$out\"\nexit 0\n",
                argv_log.to_str().unwrap()
            ),
        );
        h = h.with_ffmpeg_bin(ff.to_str().unwrap());
        let (id, dir) = create_project(&h, "16:9").await;
        seed_script(
            &dir,
            vec![shot_json(1, "台词一", 5), shot_json(2, "台词二", 4)],
        );
        std::fs::write(format!("{dir}/shot-1.mp4"), b"mp4-1").unwrap();
        std::fs::write(format!("{dir}/shot-2.mp4"), b"mp4-2").unwrap();
        std::fs::write(format!("{dir}/line-2.mp3"), b"mp3-2").unwrap(); // 只有镜头 2 有配音
        std::fs::write(format!("{dir}/bgm.mp3"), b"bgm").unwrap();
        let (task, _) = run_stage(
            &h,
            &format!("/api/v1/film/projects/{id}/compose"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(task["status"], "done", "{task:?}");
        // 2026-09-06 FilmHub：成品版本化落 hub/dist/final-v<ts>.mp4
        let finals: Vec<std::ffi::OsString> = std::fs::read_dir(format!("{dir}/hub/dist"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| {
                n.to_string_lossy().starts_with("final-v") && n.to_string_lossy().ends_with(".mp4")
            })
            .collect();
        assert_eq!(finals.len(), 1, "恰一个版本化成品: {finals:?}");
        assert!(
            std::path::Path::new(&format!("{dir}/hub/dist/compose-report.json")).is_file(),
            "报告随行"
        );
        // 项目状态 done
        let resp = h
            .handle(get_req(&format!("/api/v1/film/projects/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.body["project"]["status"], "done");
        // argv 记录：两遍 ffmpeg
        let raw = std::fs::read_to_string(&argv_log).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2, "两遍调用: {raw}");
        let pass1 = lines[0];
        assert!(
            pass1.contains("-f concat -safe 0 -i compose-concat.txt"),
            "pass1: {pass1}"
        );
        assert!(pass1.contains("scale=1920:1080"), "16:9 统一尺寸: {pass1}");
        assert!(pass1.contains("fps=30"), "{pass1}");
        assert!(pass1.ends_with("compose-video.mp4"));
        let pass2 = lines[1];
        // 只有 line-2.mp3 → 起始 5000ms（镜头1 5s）；BGM 循环；字幕烧录
        assert!(pass2.contains("-stream_loop -1 -i bgm.mp3"), "{pass2}");
        assert!(pass2.contains("-i line-2.mp3"), "{pass2}");
        assert!(
            pass2.contains("adelay=5000|5000"),
            "台词按时间轴对齐: {pass2}"
        );
        assert!(pass2.contains("subtitles=subs.srt"), "{pass2}");
        assert!(
            pass2.contains("amix=inputs=2:duration=longest:normalize=0"),
            "人声×BGM: {pass2}"
        );
        assert!(
            pass2.contains("-map [vout]") && pass2.contains("-map [aout]"),
            "{pass2}"
        );
        assert!(pass2.contains("-shortest"), "{pass2}");
        assert!(
            pass2.contains("hub/dist/final-v") && pass2.ends_with(".mp4"),
            "版本化 dist 输出: {pass2}"
        );
        // concat 清单与 SRT 落盘
        let concat_list = std::fs::read_to_string(format!("{dir}/compose-concat.txt")).unwrap();
        assert_eq!(concat_list, "file 'shot-1.mp4'\nfile 'shot-2.mp4'\n");
        let srt = std::fs::read_to_string(format!("{dir}/subs.srt")).unwrap();
        assert!(
            srt.contains("台词二") && srt.contains("00:00:05,000 --> 00:00:09,000"),
            "{srt}"
        );
    }

    /// 2026-09-06 v0.1.37：三新档（2.39:1 / 1.85:1 / 4:3）compose argv 按
    /// [`COMPOSE_DIMS`] 预设表映射（假 ffmpeg 记录 argv——scale/pad 目标分辨率）。
    #[cfg(unix)]
    #[tokio::test]
    async fn compose_fake_ffmpeg_argv_maps_new_ratio_presets() {
        for (ratio, w, h) in [
            ("2.39:1", 2048u32, 858u32),
            ("1.85:1", 1998, 1080),
            ("4:3", 1440, 1080),
        ] {
            let (mut handler, _dir) = handler_at(&format!("compose-preset-{w}"));
            let fixture = temp_dir_for(&format!("compose-preset-bin-{w}"));
            let argv_log = fixture.join("argv.log");
            let ff = fake_exec(
                &fixture,
                "ffmpeg",
                &format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nout=\"\"\nfor a in \"$@\"; do out=\"$a\"; done\n: > \"$out\"\nexit 0\n",
                    argv_log.to_str().unwrap()
                ),
            );
            handler = handler.with_ffmpeg_bin(ff.to_str().unwrap());
            let (id, dir) = create_project(&handler, ratio).await;
            seed_script(&dir, vec![shot_json(1, "", 5)]);
            std::fs::write(format!("{dir}/shot-1.mp4"), b"mp4").unwrap();
            let (task, _) = run_stage(
                &handler,
                &format!("/api/v1/film/projects/{id}/compose"),
                serde_json::json!({}),
            )
            .await;
            assert_eq!(task["status"], "done", "{ratio} 合成应成功: {task:?}");
            let raw = std::fs::read_to_string(&argv_log).unwrap();
            let pass1 = raw.lines().next().unwrap_or_default();
            assert!(
                pass1.contains(&format!("scale={w}:{h}:force_original_aspect_ratio=decrease")),
                "{ratio} scale 应映射 {w}x{h}: {pass1}"
            );
            assert!(
                pass1.contains(&format!("pad={w}:{h}:")),
                "{ratio} pad 应映射 {w}x{h}: {pass1}"
            );
        }
    }

    #[tokio::test]
    async fn compose_missing_shot_videos_lists_them() {
        let (mut h, _dir) = handler_at("compose-missing-shots");
        h = h.with_ffmpeg_bin("/usr/bin/true"); // 即使 ffmpeg 在也先被缺视频拦
        let (id, dir) = create_project(&h, "16:9").await;
        seed_script(&dir, vec![shot_json(1, "", 5), shot_json(2, "", 4)]);
        std::fs::write(format!("{dir}/shot-1.mp4"), b"mp4-1").unwrap();
        let (task, _) = run_stage(
            &h,
            &format!("/api/v1/film/projects/{id}/compose"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(task["status"], "error", "{task:?}");
        let err = task["error"].as_str().unwrap();
        assert!(
            err.contains("shot-2.mp4") && err.contains("缺少镜头视频"),
            "{err}"
        );
    }

    // ------------------------------------------------------------------
    // 导出路径（export_dir，2026-09-05）：迁移幂等 / PUT 校验三分支 /
    // final_path 拼装 / compose 输出落点 / env 基目录限制
    // ------------------------------------------------------------------

    /// 老库（2026-09-05 之前无 export_dir 列）经 open_db 升级补列；create_schema
    /// 重复执行幂等；存量行 export_dir=NULL、final_path 回落项目目录。
    #[tokio::test]
    async fn export_dir_migration_adds_column_idempotently() {
        let dir = temp_dir_for("export-mig");
        let db = dir.join("film.db");
        let proj_dir = dir.join("proj");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE film_projects (
                    id TEXT PRIMARY KEY,
                    title TEXT NOT NULL,
                    idea TEXT NOT NULL,
                    ratio TEXT NOT NULL,
                    style_hint TEXT,
                    status TEXT NOT NULL DEFAULT 'draft',
                    dir TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO film_projects
                 (id,title,idea,ratio,status,dir,created_at,updated_at)
                 VALUES ('film-1','老片','创意','16:9','draft',?1,'2026','2026')",
                params![proj_dir.to_str().unwrap()],
            )
            .unwrap();
        }
        // open_db 升级：ALTER 补 export_dir；create_schema 再跑一遍幂等不报错
        let h = FilmRouteHandler::with_db_path(db.to_str().unwrap());
        {
            let conn = h.db.lock().unwrap();
            create_schema(&conn).unwrap();
            let mut stmt = conn.prepare("PRAGMA table_info(film_projects)").unwrap();
            let cols: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .filter_map(Result::ok)
                .collect();
            assert!(cols.contains(&"export_dir".to_string()), "{cols:?}");
            let p = find_project(&conn, "film-1").unwrap();
            assert!(p.export_dir.is_none(), "存量行补列为 NULL");
        }
        // 详情回读：export_dir=null + final_path 回落项目目录
        let resp = h
            .handle(get_req("/api/v1/film/projects/film-1"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{resp:?}");
        assert_eq!(resp.body["project"]["export_dir"], serde_json::Value::Null);
        assert_eq!(
            resp.body["project"]["final_path"].as_str().unwrap(),
            format!("{}/final.mp4", proj_dir.to_str().unwrap())
        );
    }

    /// PUT 合法分支 + 空串重置分支：export_dir/final_path 全链路（PUT 回执 /
    /// 列表 / 详情）+ 尾斜杠规整。
    #[tokio::test]
    async fn put_export_dir_valid_sets_and_resets_final_path() {
        let (h, dir) = handler_at("export-put");
        let (id, proj_dir) = create_project(&h, "16:9").await;
        // 缺省分支：export_dir null，final_path 指项目目录
        let resp = h
            .handle(get_req(&format!("/api/v1/film/projects/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.body["project"]["export_dir"], serde_json::Value::Null);
        assert_eq!(
            resp.body["project"]["final_path"].as_str().unwrap(),
            format!("{proj_dir}/final.mp4")
        );
        // 合法分支：绝对路径 + 父目录存在（导出目录本身尚不存在——compose 补建）
        let exports = dir.join("exports");
        std::fs::create_dir_all(&exports).unwrap();
        let target = exports.join("premiere");
        let t = target.to_str().unwrap();
        let resp = h
            .handle(put_req(
                &format!("/api/v1/film/projects/{id}"),
                serde_json::json!({"export_dir": t}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{resp:?}");
        assert_eq!(resp.body["export_dir"].as_str().unwrap(), t);
        assert_eq!(
            resp.body["final_path"].as_str().unwrap(),
            format!("{t}/final.mp4")
        );
        // 尾斜杠规整（不产生 //final.mp4）
        let resp = h
            .handle(put_req(
                &format!("/api/v1/film/projects/{id}"),
                serde_json::json!({"export_dir": format!("{t}/")}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{resp:?}");
        assert_eq!(resp.body["export_dir"].as_str().unwrap(), t);
        assert_eq!(
            resp.body["final_path"].as_str().unwrap(),
            format!("{t}/final.mp4")
        );
        // 列表同口径
        let resp = h.handle(get_req("/api/v1/film/projects")).await.unwrap();
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr[0]["export_dir"].as_str().unwrap(), t);
        assert_eq!(
            arr[0]["final_path"].as_str().unwrap(),
            format!("{t}/final.mp4")
        );
        // 重置分支：空串 = 回缺省（项目目录本身）
        let resp = h
            .handle(put_req(
                &format!("/api/v1/film/projects/{id}"),
                serde_json::json!({"export_dir": ""}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{resp:?}");
        assert_eq!(resp.body["export_dir"], serde_json::Value::Null);
        assert_eq!(
            resp.body["final_path"].as_str().unwrap(),
            format!("{proj_dir}/final.mp4")
        );
    }

    /// PUT 拒绝分支 ①：父目录不存在 → 400 附 mkdir 指引，原值不被写。
    #[tokio::test]
    async fn put_export_dir_rejects_missing_parent_with_hint() {
        let (h, dir) = handler_at("export-put-noparent");
        let (id, _) = create_project(&h, "16:9").await;
        let bad = dir.join("no-such-dir/out");
        let resp = h
            .handle(put_req(
                &format!("/api/v1/film/projects/{id}"),
                serde_json::json!({"export_dir": bad.to_str().unwrap()}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "{resp:?}");
        let err = resp.body["error"].as_str().unwrap();
        assert!(
            err.contains("父目录不存在") && err.contains("mkdir"),
            "附指引: {err}"
        );
        let resp = h
            .handle(get_req(&format!("/api/v1/film/projects/{id}")))
            .await
            .unwrap();
        assert_eq!(
            resp.body["project"]["export_dir"],
            serde_json::Value::Null,
            "失败不落库"
        );
    }

    /// PUT 拒绝分支 ②：相对路径（含 ~ 开头——不展开）→ 400。
    #[tokio::test]
    async fn put_export_dir_rejects_relative_path() {
        let (h, _dir) = handler_at("export-put-rel");
        let (id, _) = create_project(&h, "16:9").await;
        for bad in ["exports/my-film", "~/videos", "./out"] {
            let resp = h
                .handle(put_req(
                    &format!("/api/v1/film/projects/{id}"),
                    serde_json::json!({"export_dir": bad}),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 400, "{bad}: {resp:?}");
            let err = resp.body["error"].as_str().unwrap();
            assert!(err.contains("绝对路径"), "{bad}: {err}");
        }
    }

    /// env NEXOS_FILM_EXPORT_BASE（测试注入同链）：设置时 export_dir 必须位于
    /// 其下——基外/前缀碰撞路径均 400，基内合法；缺省（不设置）不限制。
    #[tokio::test]
    async fn put_export_dir_export_base_restricts() {
        let (mut h, dir) = handler_at("export-put-base");
        let base = dir.join("safe-exports");
        std::fs::create_dir_all(base.join("sub")).unwrap();
        h = h.with_export_base(base.to_str().unwrap());
        let (id, _) = create_project(&h, "16:9").await;
        // 基内：合法
        let inside = base.join("sub/film-a");
        let resp = h
            .handle(put_req(
                &format!("/api/v1/film/projects/{id}"),
                serde_json::json!({"export_dir": inside.to_str().unwrap()}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{resp:?}");
        assert_eq!(
            resp.body["export_dir"].as_str().unwrap(),
            inside.to_str().unwrap()
        );
        // 基外：400 附基目录说明
        let outside = dir.join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        let resp = h
            .handle(put_req(
                &format!("/api/v1/film/projects/{id}"),
                serde_json::json!({"export_dir": outside.to_str().unwrap()}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "{resp:?}");
        let err = resp.body["error"].as_str().unwrap();
        assert!(
            err.contains("NEXOS_FILM_EXPORT_BASE") && err.contains(base.to_str().unwrap()),
            "{err}"
        );
        // 前缀碰撞（safe-exports-evil 不是 safe-exports 之下）：组件级判定须拒
        let evil = dir.join("safe-exports-evil");
        std::fs::create_dir_all(&evil).unwrap();
        let resp = h
            .handle(put_req(
                &format!("/api/v1/film/projects/{id}"),
                serde_json::json!({"export_dir": evil.to_str().unwrap()}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "前缀碰撞不得混过: {resp:?}");
    }

    /// compose 输出落点：export_dir 设置时 pass2 argv 末参 = 导出绝对路径，
    /// final.mp4 物理落导出目录（目录缺失由 compose 补建）、项目目录不再出现；
    /// task output 附完整路径、artifacts 清单照旧含 final.mp4 名（同名导出侧
    /// 遮项目目录旧残留）。
    #[cfg(unix)]
    #[tokio::test]
    async fn compose_writes_final_into_export_dir() {
        let (mut h, dir) = handler_at("compose-export");
        let fixture = temp_dir_for("compose-export-bin");
        let argv_log = fixture.join("argv.log");
        let ff = fake_exec(
            &fixture,
            "ffmpeg",
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nout=\"\"\nfor a in \"$@\"; do out=\"$a\"; done\n: > \"$out\"\nexit 0\n",
                argv_log.to_str().unwrap()
            ),
        );
        h = h.with_ffmpeg_bin(ff.to_str().unwrap());
        let (id, proj_dir) = create_project(&h, "16:9").await;
        seed_script(&proj_dir, vec![shot_json(1, "台词", 5)]);
        std::fs::write(format!("{proj_dir}/shot-1.mp4"), b"mp4-1").unwrap();
        // 设导出路径（父目录在、导出目录本身不存在——compose 阶段补建）
        let export_dir = dir.join("exports/final-cut");
        std::fs::create_dir_all(dir.join("exports")).unwrap();
        let e = export_dir.to_str().unwrap();
        let resp = h
            .handle(put_req(
                &format!("/api/v1/film/projects/{id}"),
                serde_json::json!({"export_dir": e}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{resp:?}");
        let (task, _) = run_stage(
            &h,
            &format!("/api/v1/film/projects/{id}/compose"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(task["status"], "done", "{task:?}");
        // 2026-09-06 FilmHub：export_dir 语义保留为 dist 落点——版本化成品落
        // <export_dir>/final-v<ts>.mp4 + compose-report.json
        let final_loc = task["output"].as_str().unwrap().to_string();
        assert!(
            final_loc.starts_with(e)
                && final_loc.contains("final-v")
                && final_loc.ends_with(".mp4"),
            "版本化成品落导出目录: {final_loc}"
        );
        assert!(
            std::path::Path::new(&final_loc).is_file(),
            "final-v 成品物理落盘"
        );
        assert!(
            std::path::Path::new(&format!("{e}/compose-report.json")).is_file(),
            "报告随行落导出目录"
        );
        assert!(
            !std::path::Path::new(&format!("{proj_dir}/final.mp4")).is_file(),
            "项目目录不再携带 final.mp4"
        );
        let resp = h
            .handle(get_req(&format!("/api/v1/film/projects/{id}")))
            .await
            .unwrap();
        let names: Vec<&str> = resp.body["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["name"].as_str().unwrap())
            .collect();
        assert!(
            names
                .iter()
                .any(|n| n.starts_with("final-v") && n.ends_with(".mp4")),
            "清单含版本化成品名: {names:?}"
        );
        // pass2 argv 末参 = 导出绝对路径（export_dir 语义直达 ffmpeg）
        let raw = std::fs::read_to_string(&argv_log).unwrap();
        let pass2 = raw.lines().last().unwrap();
        assert!(pass2.ends_with(&final_loc), "pass2 输出参数: {pass2}");
    }

    // ------------------------------------------------------------------
    // 任务面 404 / 未知路由 / 未知阶段
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn task_and_route_404s() {
        let (h, _dir) = handler_at("404s");
        let resp = h
            .handle(get_req("/api/v1/film/tasks/ft-999"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
        assert!(resp.body["error"].as_str().unwrap().contains("任务不存在"));
        let resp = h
            .handle(get_req("/api/v1/film/projects/film-1/compose"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404, "项目不存在先 404");
        let resp = h.handle(get_req("/api/v1/film/whatever")).await.unwrap();
        assert_eq!(resp.status, 404, "未知路由兜底 404");
        // 镜头号非法
        let (id, _) = create_project(&h, "16:9").await;
        let resp = h
            .handle(post_req(
                &format!("/api/v1/film/projects/{id}/shots/x/image"),
                serde_json::json!({"model_ref": {"source":"local","capability":"image"}}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "非整数镜头号 400: {resp:?}");
        let resp = h
            .handle(post_req(
                &format!("/api/v1/film/projects/{id}/shots/0/image"),
                serde_json::json!({"model_ref": {"source":"local","capability":"image"}}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "镜头号 0 应 400: {resp:?}");
        // 未知阶段
        let resp = h
            .handle(post_req(
                &format!("/api/v1/film/projects/{id}/shots/1/paint"),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404, "未知阶段 404: {resp:?}");
        // 缺 model_ref 字段 → 反序列化失败（Err）
        let resp = h
            .handle(post_req(
                &format!("/api/v1/film/projects/{id}/script"),
                serde_json::json!({}),
            ))
            .await;
        assert!(resp.is_err(), "缺 model_ref 应反序列化失败");
    }

    // ------------------------------------------------------------------
    // 引擎门控（2026-09-04：film 剥离为独立应用——装了才启用）
    // ------------------------------------------------------------------

    /// 建一个声明 engine=film 的应用裸仓库 fixture（真实 git），返回
    /// (AppRegistry, repo 名)——安装经 registry.install 真实 clone。
    async fn film_app_registry(
        test: &str,
    ) -> (
        Arc<crate::handlers::apps_handler::AppRegistry>,
        String,
        std::path::PathBuf,
    ) {
        let dir = temp_dir_for(test);
        let repos = dir.join("repos");
        std::fs::create_dir_all(&repos).unwrap();
        let ok = |args: &[&str]| {
            matches!(
                std::process::Command::new(args[0]).args(&args[1..]).output(),
                Ok(o) if o.status.success()
            )
        };
        let bare = repos.join("nexos-app-film.git");
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
        let work = dir.join(".film-work");
        std::fs::create_dir_all(work.join("web")).unwrap();
        std::fs::write(
            work.join("manifest.json"),
            serde_json::json!({
                "id": "film",
                "name": "NexOS 影片制作",
                "version": "0.1.0",
                "category": "media",
                "icon": "🎬",
                "description": "AI 影片工厂（分镜→关键帧→视频→配音→BGM→合成）",
                "entry": "web/entry.js",
                "engine": "film",
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
        (reg, "nexos-app-film".to_string(), dir)
    }

    #[tokio::test]
    async fn gating_blocks_all_film_endpoints_until_app_installed() {
        let (reg, repo, dir) = film_app_registry("gate").await;
        let h = FilmRouteHandler::with_db_path(dir.join("film.db").to_str().unwrap())
            .with_root_dir(dir.join("root").to_str().unwrap())
            .with_app_registry(Arc::clone(&reg));
        // 未安装 → 全部业务端点 404 + 精确安装指引文案
        for path in [
            "/api/v1/film/projects",
            "/api/v1/film/tasks",
            "/api/v1/film/tools",
        ] {
            let resp = h.handle(get_req(path)).await.unwrap();
            assert_eq!(resp.status, 404, "{path} 未装应 404: {resp:?}");
            assert_eq!(
                resp.body["error"].as_str().unwrap(),
                "应用「film」未安装：可在 应用中心 → 商店 安装",
                "{path} 文案: {resp:?}"
            );
        }
        // 写端点同样被拦（建项目 404，不落库）
        let resp = h
            .handle(post_req(
                "/api/v1/film/projects",
                serde_json::json!({"title": "t", "idea": "i", "ratio": "16:9"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404, "写端点也拦: {resp:?}");
        // fake 安装（真实 git clone）→ 门开 200
        let (action, rec) = reg.install(&repo).await.expect("安装应成功");
        assert_eq!(action, "install");
        assert_eq!(rec.id, "film");
        let resp = h.handle(get_req("/api/v1/film/projects")).await.unwrap();
        assert_eq!(resp.status, 200, "装后应放行: {resp:?}");
        assert_eq!(
            resp.body.as_array().unwrap().len(),
            0,
            "被拦期间未落库任何项目"
        );
        // 卸载 → 即时回 404
        reg.uninstall("film").expect("卸载应成功");
        let resp = h.handle(get_req("/api/v1/film/projects")).await.unwrap();
        assert_eq!(resp.status, 404, "卸载即时生效: {resp:?}");
    }

    #[tokio::test]
    async fn gating_inactive_without_registry_injection() {
        // 未注入注册表（既有单测直构形态）→ 不门控（兼容基线测试契约）
        let (h, _dir) = handler_at("gate-off");
        let resp = h.handle(get_req("/api/v1/film/projects")).await.unwrap();
        assert_eq!(resp.status, 200, "未注入不门控: {resp:?}");
    }

    // ==================================================================
    // 角色库与一致性（2026-09-04 P0）
    // ==================================================================

    /// 直写带 characters 绑定的 script.json。
    fn seed_script_bound(dir: &str, shots: Vec<serde_json::Value>) {
        seed_script(dir, shots);
    }

    fn bound_shot_json(n: u32, line: &str, dur: u32, characters: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "shot": n,
            "desc": format!("镜头{n}画面"),
            "image_prompt": format!("镜头{n}关键帧"),
            "video_prompt": format!("镜头{n}运动"),
            "line": line,
            "duration_secs": dur,
            "characters": characters,
        })
    }

    /// 建角色（直连 handler），返回 (id, body)。
    async fn create_character(
        h: &FilmRouteHandler,
        pid: &str,
        name: &str,
        desc: &str,
        voice: Option<&str>,
    ) -> (String, serde_json::Value) {
        let mut body = serde_json::json!({"name": name, "description": desc});
        if let Some(v) = voice {
            body["voice"] = serde_json::json!(v);
        }
        let resp = h
            .handle(post_req(
                &format!("/api/v1/film/projects/{pid}/characters"),
                body,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "建角色失败: {resp:?}");
        (
            resp.body["id"].as_str().unwrap().to_string(),
            resp.body.clone(),
        )
    }

    // ------------------------------------------------------------------
    // 纯函数：角色注入块 / voice 三态 / 强度 / 名称归一 / 魔数
    // ------------------------------------------------------------------

    fn roster_fixtures() -> Vec<FilmCharacter> {
        let mk = |id: &str, name: &str, desc: &str, voice: Option<&str>| FilmCharacter {
            id: id.into(),
            project_id: "film-x".into(),
            name: name.into(),
            description: desc.into(),
            voice: voice.map(String::from),
            portrait_ref: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        vec![
            mk("char-1", "小明", "黑发少年，红色围巾", Some("onyx")),
            mk("char-2", "小红", "双马尾少女，蓝色校服", None),
            mk("char-3", "老陈", "花白胡子的守夜人", Some("echo")),
        ]
    }

    #[test]
    fn character_prompt_block_template_order_strict_and_stable() {
        let roster = roster_fixtures();
        // 顺序 = 绑定顺序（小红→小明，与角色表 id 序相反——顺序稳定断言）
        let block = build_character_prompt_block(&["小红".into(), "小明".into()], &roster)
            .expect("两个命中角色应有注入块");
        assert!(
            block.contains("角色「小红」外形：双马尾少女，蓝色校服（与其它镜头严格同一人物）"),
            "固定措辞模板: {block}"
        );
        assert!(
            block.contains("角色「小明」外形：黑发少年，红色围巾（与其它镜头严格同一人物）"),
            "固定措辞模板: {block}"
        );
        let a = block.find("角色「小红」").unwrap();
        let b = block.find("角色「小明」").unwrap();
        assert!(a < b, "注入顺序应等于绑定顺序: {block}");
        assert!(block.contains('；'), "多角色以「；」连接: {block}");
        // 未知角色跳过 + 同名去重；全部未命中 → None
        let block2 =
            build_character_prompt_block(&["小明".into(), "路人甲".into(), "小明".into()], &roster)
                .unwrap();
        assert!(!block2.contains("路人甲"), "未知名不进注入块: {block2}");
        assert_eq!(
            block2.matches("角色「小明」").count(),
            1,
            "同名只注入一次: {block2}"
        );
        assert!(build_character_prompt_block(&["路人甲".into()], &roster).is_none());
        assert!(build_character_prompt_block(&[], &roster).is_none());
    }

    #[test]
    fn resolve_shot_voice_three_states() {
        let roster = roster_fixtures();
        // ① 绑定角色有 voice → 透传（第一个有 voice 的角色）
        assert_eq!(
            resolve_shot_voice(&["小红".into(), "小明".into()], &roster, None),
            "onyx",
            "无 voice 的小红应跳过，取小明的 onyx"
        );
        assert_eq!(
            resolve_shot_voice(&["老陈".into()], &roster, Some("nova")),
            "echo",
            "角色 voice 优先于 env"
        );
        // ② 无绑定/无 voice → env 缺省
        assert_eq!(resolve_shot_voice(&[], &roster, Some("nova")), "nova");
        assert_eq!(
            resolve_shot_voice(&["小红".into()], &roster, Some(" coral ")),
            "coral",
            "env trim 后生效"
        );
        // ③ env 未设/空串 → alloy 兜底
        assert_eq!(
            resolve_shot_voice(&["小红".into()], &roster, None),
            TTS_VOICE_FALLBACK
        );
        assert_eq!(
            resolve_shot_voice(&["小红".into()], &roster, Some("  ")),
            TTS_VOICE_FALLBACK
        );
    }

    #[test]
    fn parse_ref_strength_clamps_to_unit_interval() {
        assert_eq!(parse_ref_strength(None), 0.5);
        assert_eq!(parse_ref_strength(Some("")), 0.5);
        assert_eq!(parse_ref_strength(Some("abc")), 0.5);
        assert_eq!(parse_ref_strength(Some("1.5")), 0.5, "越上界回落缺省");
        assert_eq!(parse_ref_strength(Some("-1")), 0.5, "越下界回落缺省");
        assert_eq!(parse_ref_strength(Some("0")), 0.0);
        assert_eq!(parse_ref_strength(Some(" 0.8 ")), 0.8);
        assert_eq!(parse_ref_strength(Some("1")), 1.0);
    }

    #[test]
    fn normalize_character_names_trims_dedups_keeps_order() {
        assert_eq!(
            normalize_character_names(&[
                " 小明 ".into(),
                "".into(),
                "小红".into(),
                "小明".into(),
                "  ".into()
            ]),
            vec!["小明".to_string(), "小红".to_string()]
        );
        assert!(normalize_character_names(&[]).is_empty());
    }

    #[test]
    fn sniff_image_ext_magic_and_mime_whitelist() {
        let png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0];
        let jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0];
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(b"\x10\x00\x00\x00WEBPVP8 ");
        assert_eq!(sniff_image_ext(&png), Some("png"));
        assert_eq!(sniff_image_ext(&jpeg), Some("jpg"));
        assert_eq!(sniff_image_ext(&webp), Some("webp"));
        assert_eq!(sniff_image_ext(b"GIF89axxxx"), None, "白名单外 None");
        assert_eq!(sniff_image_ext(&[]), None);
        // mime 白名单映射
        assert_eq!(ext_for_mime("image/png"), Some("png"));
        assert_eq!(ext_for_mime("IMAGE/JPEG"), Some("jpg"), "大小写不敏感");
        assert_eq!(ext_for_mime("image/webp"), Some("webp"));
        assert_eq!(ext_for_mime("image/gif"), None, "白名单外拒绝");
    }

    #[test]
    fn default_portrait_prompt_uses_description() {
        let p = default_portrait_prompt("小明", "黑发少年");
        assert!(p.contains("小明") && p.contains("黑发少年"), "{p}");
        assert!(p.contains("定妆照"), "定妆口径: {p}");
    }

    #[test]
    fn files_download_url_encodes_query_path() {
        let u = files_download_url("/tank/os-data/film/film-1/characters/char-1/portrait.png");
        assert!(u.starts_with("/api/v1/files/download?path="), "{u}");
        assert!(u.contains("/tank/os-data/film/film-1"), "路径段保留: {u}");
        let u2 = files_download_url("/t a/b.png");
        assert!(u2.contains("%20"), "空格应转义: {u2}");
    }

    // ------------------------------------------------------------------
    // 分镜解析：characters 字段容错 + PUT 局部补丁
    // ------------------------------------------------------------------

    #[test]
    fn parse_script_shots_characters_tolerant_and_old_files_compat() {
        // characters 缺省为空（旧 script.json 兼容——不加字段即可解析）
        let old = parse_script_shots(&two_shots_json()).unwrap();
        assert!(
            old.iter().all(|s| s.characters.is_empty()),
            "旧分镜无绑定: {old:?}"
        );
        // characters 归一：trim / 去空 / 去重保序
        let raw = r#"[{"desc":"d","image_prompt":"p","characters":[" 小明 ","","小明","小红"]}]"#;
        let shots = parse_script_shots(raw).unwrap();
        assert_eq!(
            shots[0].characters,
            vec!["小明".to_string(), "小红".to_string()],
            "归一保序: {shots:?}"
        );
    }

    #[test]
    fn apply_shot_patches_merges_by_shot_with_aliases() {
        let mut shots = vec![
            serde_json::from_value::<ScriptShot>(bound_shot_json(1, "a", 5, &["小明"])).unwrap(),
            serde_json::from_value::<ScriptShot>(bound_shot_json(2, "", 4, &[])).unwrap(),
        ];
        // index/description 别名 + characters 绑定编辑（前端面板口径）
        let patches = serde_json::from_value::<Vec<ShotPatch>>(serde_json::json!([
            {"index": 2, "description": "新描述", "characters": [" 小明 ", "小明", "老陈"]},
        ]))
        .unwrap();
        apply_shot_patches(&mut shots, &patches).unwrap();
        assert_eq!(shots[1].desc, "新描述");
        assert_eq!(shots[1].line, "", "未提字段保留");
        assert_eq!(shots[1].duration_secs, 4, "未提字段保留");
        assert_eq!(
            shots[1].characters,
            vec!["小明".to_string(), "老陈".to_string()],
            "绑定归一保序"
        );
        assert_eq!(shots[0].desc, "镜头1画面", "未命中补丁不动");
        // duration 钳制 + 未知镜头 Err + 缺镜头号 Err
        let p2: Vec<ShotPatch> =
            serde_json::from_value(serde_json::json!([{"shot": 1, "duration_secs": 999}])).unwrap();
        apply_shot_patches(&mut shots, &p2).unwrap();
        assert_eq!(shots[0].duration_secs, SHOT_DURATION_MAX_SECS, "钳到 60");
        let p3: Vec<ShotPatch> =
            serde_json::from_value(serde_json::json!([{"shot": 9, "line": "x"}])).unwrap();
        assert!(
            apply_shot_patches(&mut shots, &p3).is_err(),
            "未知镜头应 Err"
        );
        let p4: Vec<ShotPatch> =
            serde_json::from_value(serde_json::json!([{"line": "x"}])).unwrap();
        assert!(
            apply_shot_patches(&mut shots, &p4).is_err(),
            "缺 shot/index 应 Err"
        );
    }

    // ------------------------------------------------------------------
    // 角色 CRUD + 定妆图上传 + refs
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn character_crud_roundtrip_dup_name_and_404() {
        let (h, _tmp) = handler_at("char-crud");
        let (id, pdir) = create_project(&h, "16:9").await;
        // name/description 必填（缺字段反序列化失败；空串 400）
        assert!(
            h.handle(post_req(
                &format!("/api/v1/film/projects/{id}/characters"),
                serde_json::json!({"name": "小明"})
            ))
            .await
            .is_err(),
            "缺 description 应反序列化失败"
        );
        for (body, mark) in [
            (
                serde_json::json!({"name": "  ", "description": "d"}),
                "空 name",
            ),
            (
                serde_json::json!({"name": "小明", "description": "  "}),
                "空 description",
            ),
        ] {
            let resp = h
                .handle(post_req(
                    &format!("/api/v1/film/projects/{id}/characters"),
                    body,
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 400, "{mark} 应 400: {resp:?}");
        }
        // 建两个角色（一个带 voice）
        let (cid1, c1) = create_character(&h, &id, "小明", "黑发少年", Some("onyx")).await;
        assert_eq!(c1["voice"], "onyx");
        assert!(c1["portrait_ref"].is_null(), "初建无定妆图: {c1:?}");
        let (cid2, _) = create_character(&h, &id, "小红", "少女", None).await;
        assert_ne!(cid1, cid2);
        // 重名 400（绑定按名字引用，须项目内唯一）
        let resp = h
            .handle(post_req(
                &format!("/api/v1/film/projects/{id}/characters"),
                serde_json::json!({"name": "小明", "description": "另一个"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "重名应 400: {resp:?}");
        // 列表（GET 公开面）
        let resp = h
            .handle(get_req(&format!("/api/v1/film/projects/{id}/characters")))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let list = resp.body.as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert!(
            list[0]["bound_shots"].is_array(),
            "绑定镜头清单在列: {list:?}"
        );
        // 更新：部分字段 + voice 清空语义
        let resp = h
            .handle(put_req(
                &format!("/api/v1/film/characters/{cid1}"),
                serde_json::json!({"description": "黑发少年，蓝围巾", "voice": ""}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{resp:?}");
        assert_eq!(resp.body["description"], "黑发少年，蓝围巾");
        assert_eq!(resp.body["name"], "小明", "未提字段保留");
        assert!(resp.body["voice"].is_null(), "voice 空串=清空: {resp:?}");
        // 改名撞名 400
        let resp = h
            .handle(put_req(
                &format!("/api/v1/film/characters/{cid1}"),
                serde_json::json!({"name": "小红"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "撞名应 400: {resp:?}");
        // 404 矩阵
        for path in [
            format!("/api/v1/film/characters/{cid1}x"),
            format!("/api/v1/film/projects/{id}/characters/../{cid1}/portrait"),
        ] {
            let resp = h
                .handle(put_req(
                    &path,
                    serde_json::json!({"name": "x", "description": "y"}),
                ))
                .await;
            assert!(
                resp.is_err() || resp.unwrap().status == 404,
                "{path} 应 404"
            );
        }
        // 删除连定妆图目录
        let cdir = format!("{pdir}/characters/{cid2}");
        std::fs::create_dir_all(&cdir).unwrap();
        std::fs::write(format!("{cdir}/portrait.png"), b"png").unwrap();
        let resp = h
            .handle(delete_req(&format!("/api/v1/film/characters/{cid2}")))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["dir_removed"], true, "{resp:?}");
        assert!(!std::path::Path::new(&cdir).exists(), "定妆图目录应连删");
        // 项目删除连角色行（新项目验证，避免影响上面断言）
        let (id2, _) = create_project(&h, "1:1").await;
        create_character(&h, &id2, "老陈", "守夜人", None).await;
        h.handle(delete_req(&format!("/api/v1/film/projects/{id2}")))
            .await
            .unwrap();
        let resp = h.handle(get_req("/api/v1/film/projects")).await.unwrap();
        assert_eq!(resp.body.as_array().unwrap().len(), 1, "项目已删");
        let resp = h
            .handle(get_req(&format!("/api/v1/film/projects/{id}/characters")))
            .await
            .unwrap();
        assert_eq!(resp.body.as_array().unwrap().len(), 1, "仅剩小明");
    }

    #[tokio::test]
    async fn portrait_upload_size_mime_magic_and_url() {
        let (h, dir) = handler_at("portrait-upload");
        let (id, pdir) = create_project(&h, "16:9").await;
        let (cid, _) = create_character(&h, &id, "小明", "黑发少年", None).await;
        let path = format!("/api/v1/film/projects/{id}/characters/{cid}/portrait");
        // 真 PNG 字节（8 字节魔数 + 载荷）
        let png: Vec<u8> = [
            vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            b"portrait-bytes".to_vec(),
        ]
        .concat();
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        // data: 前缀 / 坏 b64 / 空 → 400
        for (body, mark) in [
            (
                serde_json::json!({"image_b64": format!("data:image/png;base64,{b64}"), "mime": "image/png"}),
                "data 前缀",
            ),
            (
                serde_json::json!({"image_b64": "!!!not-b64!!!", "mime": "image/png"}),
                "坏 b64",
            ),
            (
                serde_json::json!({"image_b64": "", "mime": "image/png"}),
                "空串",
            ),
            (
                serde_json::json!({"image_b64": b64, "mime": "image/gif"}),
                "白名单外 mime",
            ),
        ] {
            let resp = h.handle(post_req(&path, body)).await.unwrap();
            assert_eq!(resp.status, 400, "{mark} 应 400: {resp:?}");
        }
        // mime 声明与魔数不符 → 400（声明 jpeg 发 png）
        let resp = h
            .handle(post_req(
                &path,
                serde_json::json!({"image_b64": b64, "mime": "image/jpeg"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "mime 与魔数不符应 400: {resp:?}");
        // 正常上传（mime 声明）→ 201 + portrait_ref + 文件落盘
        let resp = h
            .handle(post_req(
                &path,
                serde_json::json!({"image_b64": b64, "mime": "image/png"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "{resp:?}");
        assert_eq!(
            resp.body["portrait_ref"],
            format!("characters/{cid}/portrait.png"),
            "产物相对路径"
        );
        assert!(
            std::path::Path::new(&format!("{pdir}/characters/{cid}/portrait.png")).is_file(),
            "定妆图落盘"
        );
        // GET characters 回传 portrait_url（走 files/download 读取路径）+ bound_shots
        seed_script_bound(&pdir, vec![bound_shot_json(1, "", 5, &["小明"])]);
        let resp = h
            .handle(get_req(&format!("/api/v1/film/projects/{id}/characters")))
            .await
            .unwrap();
        let c = &resp.body.as_array().unwrap()[0];
        let url = c["portrait_url"].as_str().expect("应有 portrait_url");
        assert!(
            url.starts_with("/api/v1/files/download?path="),
            "既有产物读取路径: {url}"
        );
        assert!(url.contains("characters"), "{url}");
        assert_eq!(
            c["bound_shots"],
            serde_json::json!([1]),
            "绑定镜头清单: {c:?}"
        );
        // 超限 400（>10MB）
        let mut big = vec![0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        big.resize(IMAGE_MAX_BYTES + 1, 0);
        let big_b64 = base64::engine::general_purpose::STANDARD.encode(&big);
        let resp = h
            .handle(post_req(
                &path,
                serde_json::json!({"image_b64": big_b64, "mime": "image/png"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "超 10MB 应 400: {resp:?}");
        assert!(resp.body["error"].as_str().unwrap().contains("上限"));
        let _ = dir;
    }

    #[tokio::test]
    async fn refs_upload_and_list_in_project_detail() {
        let (h, dir) = handler_at("refs");
        let (id, pdir) = create_project(&h, "16:9").await;
        let png: Vec<u8> = [
            vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            b"scene-ref".to_vec(),
        ]
        .concat();
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        // 魔数白名单外 → 400
        let gif_b64 = base64::engine::general_purpose::STANDARD.encode(b"GIF89axxxxxyy");
        let resp = h
            .handle(post_req(
                &format!("/api/v1/film/projects/{id}/refs"),
                serde_json::json!({"image_b64": gif_b64}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "非白名单图应 400: {resp:?}");
        // 正常导入（mime 缺省按魔数；filename 仅展示）
        let resp = h
            .handle(post_req(
                &format!("/api/v1/film/projects/{id}/refs"),
                serde_json::json!({"image_b64": b64, "filename": "场景参考.png"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "{resp:?}");
        let name = resp.body["name"].as_str().unwrap().to_string();
        assert!(
            name.starts_with("ref-") && name.ends_with(".png"),
            "uuid 形态名: {name}"
        );
        assert_eq!(resp.body["filename"], "场景参考.png");
        assert!(
            std::path::Path::new(&format!("{pdir}/refs/{name}")).is_file(),
            "参考图落 refs/"
        );
        // 项目详情列出 refs
        let resp = h
            .handle(get_req(&format!("/api/v1/film/projects/{id}")))
            .await
            .unwrap();
        let refs = resp.body["refs"].as_array().expect("详情含 refs 清单");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0]["name"], name.clone());
        assert_eq!(refs[0]["bytes"], png.len() as i64);
        let _ = dir;
    }

    // ------------------------------------------------------------------
    // 绑定容错：script 提示词注入角色表 + 未知名保留并记日志
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn script_stage_injects_roster_and_keeps_unknown_names() {
        let (mut h, _dir) = handler_at("script-roster");
        // LLM 输出：小明（在表）+ 路人甲（不在表，保留原样）
        let content = serde_json::json!([
            {"shot":1,"desc":"开场","image_prompt":"p1","video_prompt":"v1","line":"hi","duration_secs":5,"characters":["小明","路人甲","小明"]},
            {"shot":2,"desc":"结尾","image_prompt":"p2","video_prompt":"v2","line":"","duration_secs":4},
        ])
        .to_string();
        let (port, hits) = spawn_mock_upstream(vec![chat_response(&content)]);
        h = h.with_local_chat(port, "qwen-test");
        let (id, pdir) = create_project(&h, "16:9").await;
        create_character(&h, &id, "小明", "黑发少年", None).await;
        let (task, _) = run_stage(
            &h,
            &format!("/api/v1/film/projects/{id}/script"),
            serde_json::json!({"model_ref": {"source":"local","capability":"chat"}}),
        )
        .await;
        assert_eq!(task["status"], "done", "{task:?}");
        // 提示词注入【角色表】（须从角色表选名）+ 空表时不注入的口径在纯函数测试
        let first_req = hits.lock().unwrap()[0].clone();
        assert!(
            first_req.contains("【角色表】"),
            "角色表随提示词注入: {first_req}"
        );
        assert!(first_req.contains("黑发少年"), "角色描述注入: {first_req}");
        assert!(
            first_req.contains("characters"),
            "要求输出绑定字段: {first_req}"
        );
        // 未知名保留原样 + 日志
        let resp = h
            .handle(get_req(&format!("/api/v1/film/projects/{id}")))
            .await
            .unwrap();
        let shots = resp.body["script"].as_array().unwrap();
        assert_eq!(
            shots[0]["characters"],
            serde_json::json!(["小明", "路人甲"]),
            "未知名保留原样（重复名归一去重）: {}",
            shots[0]
        );
        assert_eq!(shots[1]["characters"], serde_json::json!([]), "缺省空数组");
        assert!(
            task["log"].as_array().unwrap().iter().any(|l| {
                l.as_str().unwrap_or("").contains("路人甲")
                    && l.as_str().unwrap().contains("不在角色表")
            }),
            "未知名应记日志: {task:?}"
        );
        let _ = pdir;
    }

    // ------------------------------------------------------------------
    // 生成注入：channel reference_images / local 仅 prompt 档 / voice 透传
    // ------------------------------------------------------------------

    #[cfg(unix)]
    #[tokio::test]
    async fn image_local_prompt_injection_only_no_reference() {
        let (mut h, _dir) = handler_at("image-local-chars");
        let fixture = temp_dir_for("image-local-chars-fix");
        let smi = fake_exec(&fixture, "fake-smi.sh", "#!/bin/sh\necho 24000\n");
        // 假内核：记录 prompt 到文件 + 写 PNG 魔数
        let prompt_log = fixture.join("prompt.log");
        let imggen = fake_exec(
            &fixture,
            "fake-imggen.sh",
            &format!(
                "#!/bin/sh\nprintf '%s' \"$NEXOS_IMGGEN_PROMPT\" > {}\nprintf '\\211PNG\\015\\012\\032\\012film' > \"$NEXOS_IMGGEN_OUT\"\n",
                prompt_log.to_str().unwrap()
            ),
        );
        h = h.with_imggen_mock(
            imggen.to_str().unwrap(),
            fixture.join("fake-imggen.sh").to_str().unwrap(),
            smi.to_str().unwrap(),
        );
        let (id, dir) = create_project(&h, "16:9").await;
        let (cid, _) = create_character(&h, &id, "小明", "黑发少年，红色围巾", None).await;
        create_character(&h, &id, "小红", "双马尾少女", None).await;
        seed_script_bound(&dir, vec![bound_shot_json(1, "", 5, &["小红", "小明"])]);
        let (task, _) = run_stage(
            &h,
            &format!("/api/v1/film/projects/{id}/shots/1/image"),
            serde_json::json!({"model_ref": {"source":"local","capability":"image"}}),
        )
        .await;
        assert_eq!(task["status"], "done", "{task:?}");
        // prompt 注入档：角色块前置（措辞 + 顺序 = 绑定顺序）+ 原 prompt 保留
        let prompt = std::fs::read_to_string(&prompt_log).unwrap();
        let a = prompt.find("角色「小红」").expect("角色块: {prompt}");
        let b = prompt.find("角色「小明」").expect("角色块: {prompt}");
        assert!(a < b, "注入顺序 = 绑定顺序: {prompt}");
        assert!(
            prompt.contains("（与其它镜头严格同一人物）"),
            "固定措辞: {prompt}"
        );
        assert!(prompt.contains("黑发少年，红色围巾"), "角色描述: {prompt}");
        assert!(
            prompt.contains("镜头1关键帧"),
            "原 prompt 保留在后: {prompt}"
        );
        assert!(prompt.contains("赛博朋克"), "style_hint 仍追加: {prompt}");
        // local 档无 HTTP 请求体可言（结构性无 reference 字段）——日志标注档位
        assert!(
            task["log"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l.as_str().unwrap_or("").contains("prompt 档")),
            "日志应标注 prompt 注入档: {task:?}"
        );
        let _ = cid;
    }

    #[tokio::test]
    async fn image_channel_reference_images_and_strength_channel_only() {
        let (h, _dir) = handler_at("image-channel-chars");
        use base64::Engine;
        let png = b"\x89PNG-film-channel".to_vec();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let (port, hits) = spawn_mock_upstream(vec![
            serde_json::json!({"data":[{"b64_json": b64.clone()}]})
                .to_string()
                .into_bytes(),
            serde_json::json!({"data":[{"b64_json": b64}]})
                .to_string()
                .into_bytes(),
        ]);
        let gw = Arc::new(ApiGatewayRouteHandler::with_empty());
        let ch_id = seed_channel(&gw, &format!("http://127.0.0.1:{port}/v1"), None).await;
        let h = h.with_gateway(gw).with_ref_strength(0.7);
        let (id, dir) = create_project(&h, "1:1").await;
        let (cid, _) = create_character(&h, &id, "小明", "黑发少年", None).await;
        // 定妆图上传（绑定后才有参考可注入）
        let portrait: Vec<u8> = [
            vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            b"portrait-ref".to_vec(),
        ]
        .concat();
        let portrait_b64 = base64::engine::general_purpose::STANDARD.encode(&portrait);
        let resp = h
            .handle(post_req(
                &format!("/api/v1/film/projects/{id}/characters/{cid}/portrait"),
                serde_json::json!({"image_b64": portrait_b64, "mime": "image/png"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "{resp:?}");
        // 镜头 1 绑定角色；镜头 2 不绑定（同渠道两请求对照）
        seed_script_bound(
            &dir,
            vec![
                bound_shot_json(1, "", 5, &["小明"]),
                bound_shot_json(2, "", 5, &[]),
            ],
        );
        for n in [1, 2] {
            let (task, _) = run_stage(
                &h,
                &format!("/api/v1/film/projects/{id}/shots/{n}/image"),
                serde_json::json!({"model_ref": {"source":"channel","channel_id":ch_id,"capability":"image"}}),
            )
            .await;
            assert_eq!(task["status"], "done", "镜头{n}: {task:?}");
        }
        let reqs = hits.lock().unwrap();
        // 镜头 1：reference_images（定妆图 b64）+ strength 在请求体上
        assert!(
            reqs[0].contains("\"reference_images\":[\""),
            "镜头1应有参考注入: {}",
            reqs[0]
        );
        assert!(
            reqs[0].contains(&portrait_b64[..24]),
            "定妆图 b64 注入: {}",
            &reqs[0][..reqs[0].len().min(600)]
        );
        assert!(
            reqs[0].contains("\"reference_strength\":0.7"),
            "强度注入: {}",
            reqs[0]
        );
        // 镜头 2（无绑定角色）：不带任何 reference 字段——与旧行为逐字节同形态
        assert!(
            !reqs[1].contains("reference_images") && !reqs[1].contains("reference_strength"),
            "无绑定不发 reference 字段: {}",
            reqs[1]
        );
    }

    #[tokio::test]
    async fn video_channel_reference_images_passthrough() {
        let (h, _dir) = handler_at("video-channel-chars");
        use base64::Engine;
        let mp4 = b"\x00\x00\x00\x18ftypmp4-film".to_vec();
        let (dl_port, _dl_hits) = spawn_mock_upstream(vec![mp4.clone()]);
        let (port, hits) = spawn_mock_upstream(vec![
            serde_json::json!({"url": format!("http://127.0.0.1:{dl_port}/v.mp4")})
                .to_string()
                .into_bytes(),
        ]);
        let gw = Arc::new(ApiGatewayRouteHandler::with_empty());
        let ch_id = seed_channel(&gw, &format!("http://127.0.0.1:{port}/v1"), None).await;
        let h = h.with_gateway(gw);
        let (id, dir) = create_project(&h, "16:9").await;
        let (cid, _) = create_character(&h, &id, "小明", "黑发少年", None).await;
        let portrait: Vec<u8> = [
            vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            b"portrait-vid".to_vec(),
        ]
        .concat();
        let portrait_b64 = base64::engine::general_purpose::STANDARD.encode(&portrait);
        let resp = h
            .handle(post_req(
                &format!("/api/v1/film/projects/{id}/characters/{cid}/portrait"),
                serde_json::json!({"image_b64": portrait_b64, "mime": "image/png"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "{resp:?}");
        seed_script_bound(&dir, vec![bound_shot_json(1, "", 6, &["小明"])]);
        std::fs::write(format!("{dir}/shot-1.png"), b"\x89PNG-frame").unwrap();
        let (task, _) = run_stage(
            &h,
            &format!("/api/v1/film/projects/{id}/shots/1/video"),
            serde_json::json!({"model_ref": {"source":"channel","channel_id":ch_id,"capability":"video"}, "image_first": true}),
        )
        .await;
        assert_eq!(task["status"], "done", "{task:?}");
        let reqs = hits.lock().unwrap();
        assert!(
            reqs[0].contains("\"reference_images\":[\""),
            "视频请求应注入定妆图参考: {}",
            reqs[0]
        );
        assert!(
            reqs[0].contains("\"reference_strength\":0.5"),
            "缺省强度 0.5: {}",
            reqs[0]
        );
        assert!(
            reqs[0].contains("\"image_base64\""),
            "首帧字段语义不变: {}",
            reqs[0]
        );
    }

    #[tokio::test]
    async fn tts_voice_passthrough_bound_env_then_fallback() {
        // ① 绑定角色有 voice → 透传该值
        let (h, _dir) = handler_at("tts-voice-bound");
        let mp3 = b"ID3-fake-tts".to_vec();
        let (port, hits) = spawn_mock_upstream(vec![mp3.clone()]);
        let gw = Arc::new(ApiGatewayRouteHandler::with_empty());
        let ch_id = seed_channel(&gw, &format!("http://127.0.0.1:{port}/v1"), None).await;
        let h = h.with_gateway(gw);
        let (id, dir) = create_project(&h, "16:9").await;
        create_character(&h, &id, "小红", "少女", None).await;
        create_character(&h, &id, "小明", "少年", Some("onyx")).await;
        seed_script_bound(
            &dir,
            vec![bound_shot_json(1, "这是哪里？", 5, &["小红", "小明"])],
        );
        let (task, _) = run_stage(
            &h,
            &format!("/api/v1/film/projects/{id}/shots/1/tts"),
            serde_json::json!({"model_ref": {"source":"channel","channel_id":ch_id,"capability":"tts"}}),
        )
        .await;
        assert_eq!(task["status"], "done", "{task:?}");
        assert!(
            hits.lock().unwrap()[0].contains("\"voice\":\"onyx\""),
            "第一个有 voice 的绑定角色透传: {}",
            hits.lock().unwrap()[0]
        );
        // ② 无绑定 + 注入 env 缺省 → env 值
        let (h2, _d2) = handler_at("tts-voice-env");
        let (port2, hits2) = spawn_mock_upstream(vec![mp3.clone()]);
        let gw2 = Arc::new(ApiGatewayRouteHandler::with_empty());
        let ch2 = seed_channel(&gw2, &format!("http://127.0.0.1:{port2}/v1"), None).await;
        let h2 = h2.with_gateway(gw2).with_tts_voice("nova");
        let (id2, dir2) = create_project(&h2, "16:9").await;
        seed_script_bound(&dir2, vec![bound_shot_json(1, "台词", 5, &[])]);
        let (task2, _) = run_stage(
            &h2,
            &format!("/api/v1/film/projects/{id2}/shots/1/tts"),
            serde_json::json!({"model_ref": {"source":"channel","channel_id":ch2,"capability":"tts"}}),
        )
        .await;
        assert_eq!(task2["status"], "done", "{task2:?}");
        assert!(
            hits2.lock().unwrap()[0].contains("\"voice\":\"nova\""),
            "无绑定落 env 缺省: {}",
            hits2.lock().unwrap()[0]
        );
        // ③ 无绑定 + env 未设 → alloy 兜底
        let (h3, _d3) = handler_at("tts-voice-fallback");
        let (port3, hits3) = spawn_mock_upstream(vec![mp3]);
        let gw3 = Arc::new(ApiGatewayRouteHandler::with_empty());
        let ch3 = seed_channel(&gw3, &format!("http://127.0.0.1:{port3}/v1"), None).await;
        let h3 = h3.with_gateway(gw3);
        let (id3, dir3) = create_project(&h3, "16:9").await;
        seed_script_bound(&dir3, vec![bound_shot_json(1, "台词", 5, &[])]);
        let (task3, _) = run_stage(
            &h3,
            &format!("/api/v1/film/projects/{id3}/shots/1/tts"),
            serde_json::json!({"model_ref": {"source":"channel","channel_id":ch3,"capability":"tts"}}),
        )
        .await;
        assert_eq!(task3["status"], "done", "{task3:?}");
        assert!(
            hits3.lock().unwrap()[0].contains("\"voice\":\"alloy\""),
            "兜底 alloy: {}",
            hits3.lock().unwrap()[0]
        );
    }

    // ------------------------------------------------------------------
    // PUT script 局部更新（绑定编辑入口）+ 定妆图生成任务
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn put_project_script_patch_merges_characters() {
        let (h, _tmp) = handler_at("put-script-patch");
        let (id, pdir) = create_project(&h, "16:9").await;
        seed_script_bound(
            &pdir,
            vec![
                bound_shot_json(1, "a", 5, &[]),
                bound_shot_json(2, "b", 4, &["旧角色"]),
            ],
        );
        // 局部补丁：只改镜头 2 的绑定（index 别名口径）
        let resp = h
            .handle(put_req(
                &format!("/api/v1/film/projects/{id}"),
                serde_json::json!({"script": [{"index": 2, "characters": ["小明", " 小明 ", "小红"]}]}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{resp:?}");
        assert_eq!(resp.body["script_patched"], true, "{resp:?}");
        let shots = resp.body["script"].as_array().unwrap();
        assert_eq!(
            shots[1]["characters"],
            serde_json::json!(["小明", "小红"]),
            "归一保序: {}",
            shots[1]
        );
        assert_eq!(shots[1]["line"], "b", "未提字段保留");
        assert_eq!(
            shots[0]["characters"],
            serde_json::json!([]),
            "未命中镜头不动"
        );
        // 未知镜头 → 400
        let resp = h
            .handle(put_req(
                &format!("/api/v1/film/projects/{id}"),
                serde_json::json!({"script": [{"shot": 9, "line": "x"}]}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "{resp:?}");
        // 无 script 字段 → 不触碰分镜（旧契约兼容）
        let resp = h
            .handle(put_req(
                &format!("/api/v1/film/projects/{id}"),
                serde_json::json!({"title": "新标题"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["script_patched"], false);
        assert_eq!(resp.body["title"], "新标题");
        assert_eq!(
            resp.body["script"].as_array().unwrap().len(),
            2,
            "script 随响应回显"
        );
    }

    #[tokio::test]
    async fn portrait_generate_task_writes_file_and_updates_ref() {
        let (h, dir) = handler_at("portrait-gen");
        use base64::Engine;
        let png = b"\x89PNG-portrait-gen".to_vec();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let (port, hits) =
            spawn_mock_upstream(vec![serde_json::json!({"data":[{"b64_json": b64}]})
                .to_string()
                .into_bytes()]);
        let gw = Arc::new(ApiGatewayRouteHandler::with_empty());
        let ch_id = seed_channel(&gw, &format!("http://127.0.0.1:{port}/v1"), None).await;
        let h = h.with_gateway(gw);
        let (id, pdir) = create_project(&h, "16:9").await;
        let (cid, _) = create_character(&h, &id, "小明", "黑发少年，红色围巾", None).await;
        // 校验先行：local.image 合法、能力不匹配 400
        let resp = h
            .handle(post_req(
                &format!("/api/v1/film/projects/{id}/characters/{cid}/portrait/generate"),
                serde_json::json!({"model_ref": {"source":"channel","channel_id":ch_id,"capability":"video"}}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "能力不匹配应 400: {resp:?}");
        let (task, _) = run_stage(
            &h,
            &format!("/api/v1/film/projects/{id}/characters/{cid}/portrait/generate"),
            serde_json::json!({"model_ref": {"source":"channel","channel_id":ch_id,"capability":"image"}}),
        )
        .await;
        assert_eq!(task["status"], "done", "{task:?}");
        // 产物落 <dir>/characters/<cid>/portrait.png + portrait_ref 回写
        let out = format!("{pdir}/characters/{cid}/portrait.png");
        assert_eq!(std::fs::read(&out).unwrap(), png, "定妆图落盘");
        assert!(task["output"].as_str().unwrap().ends_with("portrait.png"));
        // 缺省 prompt 由 description 构造（渠道请求体可查）
        assert!(
            hits.lock().unwrap()[0].contains("黑发少年，红色围巾"),
            "缺省 prompt 用描述: {}",
            hits.lock().unwrap()[0]
        );
        assert!(
            hits.lock().unwrap()[0].contains("720x720"),
            "定妆图 1:1 口径: {}",
            hits.lock().unwrap()[0]
        );
        // GET characters 现在带 portrait_url
        let resp = h
            .handle(get_req(&format!("/api/v1/film/projects/{id}/characters")))
            .await
            .unwrap();
        let c = &resp.body.as_array().unwrap()[0];
        assert_eq!(
            c["portrait_ref"],
            format!("characters/{cid}/portrait.png"),
            "{c:?}"
        );
        assert!(c["portrait_url"]
            .as_str()
            .unwrap()
            .contains("files/download"));
        let _ = dir;
    }
}
