//! `MediaGenRouteHandler` —— 媒体生成（图片真实 sd-turbo / 视频任务框架）REST 入口。
//!
//! 定位（docs/MEDIA_GEN_AND_CHAIN_AUTH.md §A/§B）：模型管理桌面应用的「生成」区后端。
//! 与 `media.rs`（媒体库三件套）不同域：本 handler 只做**生成**，组件名 `media-gen`。
//!
//! # A. 图片生成（真实能力：本地 sd-turbo spawn python 管线）
//!
//! - `POST /api/v1/media/image` {prompt, width?, height?, steps?} → PNG base64
//!   （默认 768×432 / 4 步，与壁纸管线同款）。
//! - 生成经子进程 spawn python（diffusers `AutoPipelineForText2Image`，
//!   `/tank/models/sd-turbo`，fp16，模块级管道缓存），产出 PNG 落
//!   `/tmp/media-gen/<token>.png` 再 base64 编码返回；进程短生命周期（生成即退，
//!   显存随进程释放）。超时 60s kill（`kill_on_drop`）。
//! - **GPU 互斥**：llm-101（Qwen3，22G）运行时 sd-turbo 放不下——先 spawn
//!   `nvidia-smi --query-gpu=memory.free` 探测，空闲 < 6000 MiB → 503 + 明确提示
//!   "先停 LLM 实例再生成"；探测本身不可用 → 503（无法确认安全，默认拒绝）。
//!   **统一内存回退**（GB10/Jetson，2026-09-03）：`memory.free` 报 `[N/A]`（无
//!   独立显存）→ 回退 `/proc/meminfo` MemAvailable（CPU/GPU 同池，vLLM 占的
//!   就是它），闸门语义不变。
//! - `GET /api/v1/media/image/recent`：内存环形 Vec，最近 50 条生成记录
//!   （id / prompt 摘要 / 尺寸 / 步数 / 耗时 / 时间，**不含 base64**）。
//!
//! # B. 视频生成（任务框架先行，后端可插，诚实无后端）
//!
//! - [`VideoBackend`] trait：`local`（[`LocalVideoBackend`]，本地模型未就绪）/
//!   `external`（[`ExternalVideoBackend`]，读 env `NEXOS_VIDEO_API_URL` /
//!   `NEXOS_VIDEO_API_KEY`，未配明确报错）两个占位实现。
//! - `POST /api/v1/media/video` {prompt, duration_secs?, backend?} → 创建任务
//!   （status=queued）并**立即尝试 submit**：成功 → processing，失败 → failed
//!   （附清晰指引）。当前无任何后端能成功 → 任务创建即 failed（诚实，不假装排队）。
//! - `GET /api/v1/media/video/tasks` / `GET /api/v1/media/video/tasks/:id`：
//!   内存 `Mutex<Vec<VideoTask>>` 任务列表 / 详情。
//!
//! # 鉴权、链上身份归因与生图计费（2026-08-20 变现闭环接线）
//!
//! 本组件挂**独立** [`ChainAuth`] 实例（挑战-签名，IM/NexHub 同款契约；main.rs
//! 经 [`Self::with_chain_auth`] 注入共享 Arc），可选挂**共享**
//! [`ApiGatewayRouteHandler`]（[`Self::with_gateway`]——sk-os- 生图计费必须与
//! api_gateway 组件同一实例：`Mutex<Connection>` 是查-检-扣原子的边界）。
//!
//! - `POST /api/v1/media/auth/challenge|verify`：链上身份三步认证（公开，
//!   契约与 IM/NexHub 同款：nonce 60s 单次有效 + token 24h 单点登录）。
//! - `POST /api/v1/media/image` 身份解析（**handler 内自验**，requires_auth=false
//!   ——网关中间件无法识别链上/sk-os- token，走中间件会把非 admin 调用方全部
//!   挡在 401，同 NexHub 惯例）：
//!
//!   | 优先级 | 身份（Authorization: Bearer ...） | generated_by | 计费 |
//!   |--------|-----------------------------------|--------------|------|
//!   | 1 | 链上 token（media-gen 实例签发） | pubkey（display=派生 EVM 地址） | 不扣（billing=null）|
//!   | 2 | 系统 admin（`NEXOS_ADMIN_TOKEN`/`OS_ADMIN_TOKEN` 精确比对） | `"admin"` | 不扣（billing=null）|
//!   | 3 | sk-os- 网关令牌（`sk-os-` 前缀 + api_gateway 令牌表命中） | 令牌名 | 见下 |
//!   | — | 都无 / sk-os- 通道未注入 | — | 401 |
//!
//!   sk-os- 计费（生成前经 [`ApiGatewayRouteHandler::try_charge_image`]）：
//!   `free` → `billing="free"` 不扣费；`per_image`/`per_token`/`credits` → 扣
//!   [`IMAGE_PRICE_CREDITS`](crate::handlers::api_gateway::IMAGE_PRICE_CREDITS)
//!   =100 积分、`billing="image_credit"`；余额不足 → **402**（文案含"余额不足"
//!   哨兵 + 充值指引）；未知/禁用/过期令牌 → 401。
//!   落账顺序：参数校验（400）→ 显存探测（503）**之后**、GPU spawn **之前**——
//!   预检失败不扣费、余额不足不烧显存；扣费成功后生成失败暂不退款（已知限制）。
//! - `POST /api/v1/media/video`：维持网关层 admin 鉴权（requires_auth+admin）。
//! - recent 记录带 `generated_by` / `generated_by_display`（无归因的历史条目
//!   序列化为 null，前端兼容）。
//!
//! # 子进程注入点（env）
//!
//! - `NEXOS_IMGGEN_BIN`（生图可执行，默认 python3）、
//!   `NEXOS_IMGGEN_SCRIPT`（管线脚本路径，默认 `/tmp/nexos-imggen.py`，缺失自动落盘）、
//!   `NEXOS_IMGGEN_TIMEOUT_SECS`（超时秒数，默认 60）、`NEXOS_SMI_BIN`（显存探测
//!   二进制，默认 nvidia-smi）、`NEXOS_SD_MODEL`（sd-turbo 模型路径）。
//!
//! # 路由表（7 条）
//!
//! | method | path                              | 动作 |
//! |--------|-----------------------------------|------|
//! | POST   | `/api/v1/media/auth/challenge`    | 链上认证：签发挑战 nonce（公开）|
//! | POST   | `/api/v1/media/auth/verify`       | 链上认证：验签签发 token（公开）|
//! | POST   | `/api/v1/media/image`             | 生图（handler 内自验：链上/admin/sk-os-）|
//! | GET    | `/api/v1/media/image/recent`      | 近期生成记录（带归因）|
//! | POST   | `/api/v1/media/video`             | 创建视频任务（需 admin）|
//! | GET    | `/api/v1/media/video/tasks`       | 视频任务列表 |
//! | GET    | `/api/v1/media/video/tasks/:id`   | 视频任务详情 |

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use os_common::chain_auth::{self, ChainAuth};
use serde::{Deserialize, Serialize};

use super::api_gateway::{ApiGatewayRouteHandler, IMAGE_CHARGE_INSUFFICIENT_MARKER};
use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// 常量
// ----------------------------------------------------------------------------

/// sd-turbo 生图管线脚本落盘路径（首次调用自动写出，内容见 [`IMGGEN_SCRIPT_PY`]）。
const IMGGEN_SCRIPT_PATH: &str = "/tmp/nexos-imggen.py";

/// 生成产物目录（PNG 先落盘再 base64 返回）。
const MEDIA_GEN_DIR: &str = "/tmp/media-gen";

/// 显存门槛（MiB）：空闲低于此值判定 sd-turbo 放不下（Qwen3 22G 实例互斥）。
const VRAM_FREE_MIN_MIB: u64 = 6000;

/// 生图子进程超时（秒，可用 env `NEXOS_IMGGEN_TIMEOUT_SECS` 覆写，默认 60）。
const IMGGEN_TIMEOUT_SECS: u64 = 60;

/// recent 环形容量（最多保留最近 50 条）。
const RECENT_CAP: usize = 50;

/// prompt 长度上限（字）。
const PROMPT_MAX_CHARS: usize = 2000;

/// 宽高约束：64 的倍数，且 256..=1024。
const DIM_MIN: u32 = 256;
const DIM_MAX: u32 = 1024;
const DIM_STEP: u32 = 64;

/// 视频时长约束（秒）：1..=30，默认 5。
const VIDEO_DURATION_DEFAULT_SECS: u32 = 5;
const VIDEO_DURATION_MIN_SECS: u32 = 1;
const VIDEO_DURATION_MAX_SECS: u32 = 30;

/// 网关下游令牌前缀（api_gateway `generate_api_key` 产出 `sk-os-<32hex>`）。
/// Authorization Bearer 以此开头 → 走网关令牌计费路径（前缀不匹配的令牌一律
/// 不进网关查表，直接按无身份 401）。
const SK_OS_PREFIX: &str = "sk-os-";

/// sk-os- free 计费令牌的响应 `billing` 标签（未扣费放行）。
const BILLING_FREE: &str = "free";
/// sk-os- 已扣积分（per_image/per_token/credits）的响应 `billing` 标签。
const BILLING_IMAGE_CREDIT: &str = "image_credit";

/// sd-turbo 生图 python 管线（参数经 env 传入，输出 PNG 落 NEXOS_IMGGEN_OUT）。
///
/// - diffusers `AutoPipelineForText2Image.from_pretrained(/tank/models/sd-turbo,
///   float16, variant="fp16").to("cuda")`；模块级 `_PIPE` 缓存避免同进程重载。
/// - 模型路径可经 env `NEXOS_SD_MODEL` 覆写（默认 `/tank/models/sd-turbo`）。
const IMGGEN_SCRIPT_PY: &str = r#"#!/usr/bin/env python3
# NexOS sd-turbo 文生图管线（os-api media_gen.rs 自动落盘并 spawn 调用）。
# 参数经环境变量传入：NEXOS_IMGGEN_PROMPT / _WIDTH / _HEIGHT / _STEPS / _OUT。
import os
import sys

_PIPE = {}


def get_pipe():
    # 模块级管道缓存：同一 python 进程内多次生成不重载模型。
    if "pipe" not in _PIPE:
        import torch
        from diffusers import AutoPipelineForText2Image

        model = os.environ.get("NEXOS_SD_MODEL", "/tank/models/sd-turbo")
        pipe = AutoPipelineForText2Image.from_pretrained(
            model, torch_dtype=torch.float16, variant="fp16"
        )
        _PIPE["pipe"] = pipe.to("cuda")
    return _PIPE["pipe"]


def main():
    prompt = os.environ.get("NEXOS_IMGGEN_PROMPT", "").strip()
    out_path = os.environ.get("NEXOS_IMGGEN_OUT", "").strip()
    width = int(os.environ.get("NEXOS_IMGGEN_WIDTH", "768"))
    height = int(os.environ.get("NEXOS_IMGGEN_HEIGHT", "432"))
    steps = int(os.environ.get("NEXOS_IMGGEN_STEPS", "4"))
    if not prompt or not out_path:
        print("缺少 NEXOS_IMGGEN_PROMPT / NEXOS_IMGGEN_OUT 环境变量", file=sys.stderr)
        return 2
    try:
        image = get_pipe()(
            prompt=prompt,
            width=width,
            height=height,
            num_inference_steps=steps,
        ).images[0]
        os.makedirs(os.path.dirname(out_path) or ".", exist_ok=True)
        image.save(out_path, format="PNG")
    except Exception as exc:  # noqa: BLE001 —— stderr 原样回传给 Rust 侧摘要
        print(f"生成失败: {exc}", file=sys.stderr)
        return 3
    print(out_path)
    return 0


if __name__ == "__main__":
    sys.exit(main())
"#;

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// `POST /api/v1/media/image` 请求体。
#[derive(Debug, Deserialize)]
struct ImageGenBody {
    prompt: String,
    /// 默认 768（64 的倍数，256..=1024）。
    #[serde(default)]
    width: Option<u32>,
    /// 默认 432（64 的倍数，256..=1024）。
    #[serde(default)]
    height: Option<u32>,
    /// 默认 4，1..=8。
    #[serde(default)]
    steps: Option<u32>,
}

/// `POST /api/v1/media/image` 成功响应。
#[derive(Debug, Serialize)]
struct ImageGenResponse {
    id: String,
    png_base64: String,
    width: u32,
    height: u32,
    elapsed_ms: u64,
    file_path: String,
    /// 本次调用是否实际扣了积分（仅 sk-os- 令牌路径可为 true）。
    charged: bool,
    /// 计费标签：`"free"`（sk-os- free 令牌，未扣）| `"image_credit"`（sk-os-
    /// 令牌已扣 100 积分）| `null`（链上 token / admin——不走计费）。
    billing: Option<String>,
    /// 生成归因：链上 pubkey / `"admin"` / sk-os- 令牌名。
    generated_by: Option<String>,
    /// 链上身份展示名（pubkey 派生 EVM 地址）；非链上身份为 null。
    generated_by_display: Option<String>,
}

/// recent 环形里的单条生成记录（不含 base64）。
///
/// `generated_by` / `generated_by_display` 为 2026-08-20 归因接线新增：无归因的
/// 条目序列化为 `null`（前端/历史消费者按可选字段兼容）。
#[derive(Debug, Clone, Serialize)]
pub struct ImageRecentItem {
    pub id: String,
    /// prompt 摘要（前 120 字）。
    pub prompt_summary: String,
    pub width: u32,
    pub height: u32,
    pub steps: u32,
    pub elapsed_ms: u64,
    pub created_at: String,
    /// 生成归因（链上 pubkey / "admin" / sk-os- 令牌名）；无归因 → null。
    pub generated_by: Option<String>,
    /// 链上身份展示名（EVM 地址）；非链上身份 → null。
    pub generated_by_display: Option<String>,
}

/// `POST /api/v1/media/video` 请求体。
#[derive(Debug, Deserialize)]
struct VideoGenBody {
    prompt: String,
    /// 默认 5，1..=30。
    #[serde(default)]
    duration_secs: Option<u32>,
    /// `external`（默认）/ `local`。
    #[serde(default)]
    backend: Option<String>,
}

/// 视频生成任务（生命周期 queued→processing→completed(url)|failed(error)）。
#[derive(Debug, Clone, Serialize)]
pub struct VideoTask {
    pub id: String,
    pub prompt: String,
    pub duration_secs: u32,
    /// `external` / `local`。
    pub backend: String,
    /// `queued` / `processing` / `completed` / `failed`。
    pub status: String,
    pub video_url: Option<String>,
    pub error: Option<String>,
    pub created_at: String,
}

// ----------------------------------------------------------------------------
// 视频后端抽象（可插，当前两个占位实现均无成功路径——诚实不假装排队）
// ----------------------------------------------------------------------------

/// 视频生成后端契约：`local`（未来本地模型）/ `external`（外部 API）可插拔。
#[async_trait]
pub trait VideoBackend: Send + Sync {
    /// 后端名（与请求 `backend` 字段对应）。
    fn name(&self) -> &str;
    /// 提交任务：Ok → 任务转 processing；Err(reason) → 任务转 failed(reason)。
    async fn submit(&self, task: &VideoTask) -> Result<(), String>;
}

/// 本地视频后端（占位）：本地视频模型未就绪，submit 一律失败。
pub struct LocalVideoBackend;

#[async_trait]
impl VideoBackend for LocalVideoBackend {
    fn name(&self) -> &str {
        "local"
    }

    async fn submit(&self, _task: &VideoTask) -> Result<(), String> {
        Err("本地视频模型未就绪".to_string())
    }
}

/// 外部视频后端（占位）：读 env `NEXOS_VIDEO_API_URL` / `NEXOS_VIDEO_API_KEY`。
pub struct ExternalVideoBackend;

#[async_trait]
impl VideoBackend for ExternalVideoBackend {
    fn name(&self) -> &str {
        "external"
    }

    async fn submit(&self, _task: &VideoTask) -> Result<(), String> {
        let url = std::env::var("NEXOS_VIDEO_API_URL")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let Some(_url) = url else {
            return Err("未配置外部视频后端 env NEXOS_VIDEO_API_URL".to_string());
        };
        // URL 已配置但 HTTP 客户端尚未接入——诚实失败，不假装排队。
        Err("外部视频后端已配置但调用客户端尚未接入（NEXOS_VIDEO_API_URL 已设置）".to_string())
    }
}

/// 按名字选择后端（`external` / `local`；非法名在请求校验期已拦 400）。
fn backend_for(name: &str) -> Box<dyn VideoBackend> {
    match name {
        "local" => Box::new(LocalVideoBackend),
        _ => Box::new(ExternalVideoBackend),
    }
}

/// 视频任务失败时的用户指引（拼在 error 后）。
#[must_use]
pub fn video_failure_guidance(backend: &str) -> String {
    if backend == "local" {
        "指引：本地视频推理后端尚未就绪，请等待后续版本接入本地模型".to_string()
    } else {
        "指引：设置 NEXOS_VIDEO_API_URL 与 NEXOS_VIDEO_API_KEY 接入外部视频生成 API 后重试"
            .to_string()
    }
}

// ----------------------------------------------------------------------------
// 纯函数（易单测）
// ----------------------------------------------------------------------------

/// 构造 nvidia-smi 显存查询参数（不执行，caller 拼 `Command::new(smi_bin)`）。
#[must_use]
pub fn build_nvidia_smi_vram_cmd() -> Vec<String> {
    vec![
        "--query-gpu=memory.free".into(),
        "--format=csv,noheader,nounits".into(),
    ]
}

/// 解析 nvidia-smi 显存输出（每行一个 GPU 的 free MiB），取**最大**空闲值
/// （多卡时以最佳候选卡为准）。无法解析返回 None。
#[must_use]
pub fn parse_vram_free_mib(stdout: &str) -> Option<u64> {
    stdout
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .filter_map(|l| l.parse::<u64>().ok())
        .max()
}

/// 显存门槛检查：空闲 < 6000 MiB → Err（提示先停 LLM 实例）。
/// （返回 `Result` 已是 must_use，不再重复标注——clippy double_must_use。）
pub fn vram_gate(free_mib: u64) -> Result<(), String> {
    if free_mib < VRAM_FREE_MIN_MIB {
        Err(format!(
            "显存不足（推理实例占用中），先停 LLM 实例再生成（当前空闲 {free_mib} MiB < {VRAM_FREE_MIN_MIB} MiB）"
        ))
    } else {
        Ok(())
    }
}

/// stderr 摘要：去空白，超 400 字取尾部（错误通常在末尾）。
#[must_use]
pub fn summarize_stderr(stderr: &str) -> String {
    let s = stderr.trim();
    if s.chars().count() > 400 {
        let tail: String = s.chars().skip(s.chars().count() - 400).collect();
        format!("…{tail}")
    } else {
        s.to_string()
    }
}

/// 图片生成参数校验：prompt 非空 ≤2000 字；**显式传入的**宽高须为 64 的倍数且
/// 256..=1024；steps 1..=8。
///
/// 默认宽高 768×432（壁纸管线同款 16:9）不受 64 倍数约束——432 本就不是 64 的
/// 倍数（64×6.75），sd-turbo/diffusers 只要求 8 的倍数即可正常出图；故 64 倍数
/// 规则仅约束调用方显式传值的场景（防误传 433/100 这类笔误），默认值放行。
fn validate_image_params(
    prompt: &str,
    width: Option<u32>,
    height: Option<u32>,
    steps: u32,
) -> Result<(), String> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Err("prompt 不可为空".to_string());
    }
    if prompt.chars().count() > PROMPT_MAX_CHARS {
        return Err(format!("prompt 长度不可超过 {PROMPT_MAX_CHARS} 字"));
    }
    for (label, v) in [("width", width), ("height", height)] {
        let Some(v) = v else {
            continue; // 未显式传入 → 用默认值（768×432），不做 64 倍数约束
        };
        if v % DIM_STEP != 0 {
            return Err(format!("{label} 必须是 {DIM_STEP} 的倍数（当前 {v}）"));
        }
        if !(DIM_MIN..=DIM_MAX).contains(&v) {
            return Err(format!("{label} 必须在 {DIM_MIN}..{DIM_MAX}（当前 {v}）"));
        }
    }
    if !(1..=8).contains(&steps) {
        return Err(format!("steps 必须在 1..=8（当前 {steps}）"));
    }
    Ok(())
}

/// 视频生成参数校验：prompt 非空 ≤2000 字；duration 1..=30；backend external|local。
fn validate_video_params(prompt: &str, duration: u32, backend: &str) -> Result<(), String> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Err("prompt 不可为空".to_string());
    }
    if prompt.chars().count() > PROMPT_MAX_CHARS {
        return Err(format!("prompt 长度不可超过 {PROMPT_MAX_CHARS} 字"));
    }
    if !(VIDEO_DURATION_MIN_SECS..=VIDEO_DURATION_MAX_SECS).contains(&duration) {
        return Err(format!(
            "duration_secs 必须在 {VIDEO_DURATION_MIN_SECS}..={VIDEO_DURATION_MAX_SECS}（当前 {duration}）"
        ));
    }
    if backend != "external" && backend != "local" {
        return Err(format!(
            "backend 必须是 external 或 local（当前 {backend}）"
        ));
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// 注入点（env 覆写，测试/运维用）
// ----------------------------------------------------------------------------

/// 生图可执行路径：env `NEXOS_IMGGEN_BIN` 覆写（默认 python3）。
/// 2026-09-04 起 `pub(crate)`：film.rs 影片管线的 local.image 复用生图内核
/// （同一注入点）；仅可见性变化，零行为回归。
pub(crate) fn imggen_bin() -> String {
    std::env::var("NEXOS_IMGGEN_BIN")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "python3".to_string())
}

/// 生图管线脚本路径：env `NEXOS_IMGGEN_SCRIPT` 覆写（默认 /tmp/nexos-imggen.py）。
/// 2026-09-04 起 `pub(crate)`：film.rs 复用（同一注入点）；仅可见性变化。
pub(crate) fn imggen_script() -> String {
    std::env::var("NEXOS_IMGGEN_SCRIPT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| IMGGEN_SCRIPT_PATH.to_string())
}

/// 显存探测二进制：env `NEXOS_SMI_BIN` 覆写（默认 nvidia-smi）。
/// 2026-09-04 起 `pub(crate)`：film.rs 复用生图内核（同一探测注入点）。
pub(crate) fn smi_bin() -> String {
    std::env::var("NEXOS_SMI_BIN")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "nvidia-smi".to_string())
}

/// 生图超时秒数：env `NEXOS_IMGGEN_TIMEOUT_SECS` 覆写（默认 60，钳制 1..=300）。
fn imggen_timeout() -> Duration {
    let secs = std::env::var("NEXOS_IMGGEN_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(IMGGEN_TIMEOUT_SECS)
        .clamp(1, 300);
    Duration::from_secs(secs)
}

// ----------------------------------------------------------------------------
// 真实子进程（显存探测 / 生图 spawn，失败降级不 panic）
// ----------------------------------------------------------------------------

/// 一次生图任务参数（spawn 传参用）。
///
/// 2026-09-04 起 `pub(crate)`（含字段）：film.rs 的 local.image 经
/// [`run_imggen_with`] 复用生图内核（不复制 spawn 逻辑）；仅可见性变化。
pub(crate) struct ImageJob {
    pub(crate) prompt: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) steps: u32,
    pub(crate) out_path: String,
}

/// 探测 GPU 空闲显存（MiB，多卡取最大）。spawn 失败 / 非零退出 → Err。
///
/// **统一内存回退**（2026-09-03，DGX Spark GB10 实测）：GB10/Jetson 无独立显存，
/// `memory.free` 报 `[N/A]`——回退 `/proc/meminfo` MemAvailable（CPU/GPU 共享
/// 同一 LPDDR5x 池，vLLM 占的就是它，闸门语义不变）。其余不可解析仍 Err
/// （无法确认安全，默认拒绝）。
///
/// 2026-09-04 起 `pub(crate)`：film.rs 复用（bin 参数化，不读 env——注入点由
/// 调用方解析）；仅可见性变化。
pub(crate) async fn probe_vram_free_mib_with(bin: &str) -> Result<u64, String> {
    let out = tokio::process::Command::new(bin)
        .args(build_nvidia_smi_vram_cmd())
        .output()
        .await
        .map_err(|e| format!("无法探测显存（{bin} 不可用）: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "无法探测显存（{bin} 退出码 {:?}）",
            out.status.code()
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    if let Some(v) = parse_vram_free_mib(&stdout) {
        return Ok(v);
    }
    if stdout.contains("[N/A]") {
        // 统一内存形态（驱动报不出独立显存）：MemAvailable 即 GPU 可用池
        let (_, avail_b, _, _) = crate::handlers::monitor::read_meminfo();
        if avail_b > 0 {
            return Ok(avail_b / (1024 * 1024));
        }
        return Err(format!(
            "无法探测显存（{bin} 输出 [N/A] 且 /proc/meminfo 不可读）"
        ));
    }
    Err(format!("无法探测显存（{bin} 输出不可解析）"))
}

/// 请求路径用的显存探测（读 [`smi_bin`] 注入点）。
async fn probe_vram_free_mib() -> Result<u64, String> {
    probe_vram_free_mib_with(&smi_bin()).await
}

/// 确保生图脚本已落盘（内容有变化才重写），返回脚本路径。
///
/// 2026-09-04 起 `pub(crate)`：film.rs 复用（同一脚本落盘语义）；仅可见性变化。
pub(crate) async fn ensure_imggen_script(script: &str) -> Result<String, String> {
    let unchanged = tokio::fs::read_to_string(script)
        .await
        .map(|content| content == IMGGEN_SCRIPT_PY)
        .unwrap_or(false);
    if !unchanged {
        tokio::fs::write(script, IMGGEN_SCRIPT_PY)
            .await
            .map_err(|e| format!("写出生图脚本失败 {script}: {e}"))?;
    }
    Ok(script.to_string())
}

/// spawn 生图子进程并等待完成（env 传参 + 超时 kill）。
///
/// 超时经 `tokio::time::timeout` 包 `wait_with_output`；`kill_on_drop(true)` 保证
/// 超时后 future 被 drop 时子进程被 kill（60s 兜底）。成功后 caller 自行读输出文件。
///
/// 2026-09-04 起 `pub(crate)`：film.rs 的 local.image 经此复用 sd-turbo 生图内核
/// （进程语义/env 传参零复制）；仅可见性变化，零行为回归。
pub(crate) async fn run_imggen_with(bin: &str, script: &str, job: &ImageJob) -> Result<(), String> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg(script)
        .env("NEXOS_IMGGEN_PROMPT", &job.prompt)
        .env("NEXOS_IMGGEN_WIDTH", job.width.to_string())
        .env("NEXOS_IMGGEN_HEIGHT", job.height.to_string())
        .env("NEXOS_IMGGEN_STEPS", job.steps.to_string())
        .env("NEXOS_IMGGEN_OUT", &job.out_path)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = cmd
        .spawn()
        .map_err(|e| format!("生图进程启动失败（{bin}）: {e}"))?;
    let timeout_secs = imggen_timeout();
    match tokio::time::timeout(timeout_secs, child.wait_with_output()).await {
        Err(_) => Err(format!(
            "生成超时（{}s），已终止生图进程",
            timeout_secs.as_secs()
        )),
        Ok(Err(e)) => Err(format!("生图进程执行失败: {e}")),
        Ok(Ok(out)) => {
            if !out.status.success() {
                let stderr = summarize_stderr(&String::from_utf8_lossy(&out.stderr));
                return Err(format!(
                    "生图脚本失败（退出码 {:?}）: {stderr}",
                    out.status.code()
                ));
            }
            Ok(())
        }
    }
}

// ----------------------------------------------------------------------------
// MediaGenRouteHandler
// ----------------------------------------------------------------------------

/// `POST /api/v1/media/image` 的调用方身份（`Authorization: Bearer` 解析结果，
/// 不含计费——sk-os- 的扣费在生成前最后一步落账）。
///
/// 解析顺序见 [`MediaGenRouteHandler::resolve_image_caller`]；`generated_by`
/// 归因与计费矩阵见模块头。
enum ImageCaller {
    /// 链上身份（media-gen ChainAuth 签发的 token 反查）：归因 pubkey，
    /// 展示名为派生 EVM 地址；不扣费。
    Chain {
        pubkey: String,
        display_name: String,
    },
    /// 系统 admin（`NEXOS_ADMIN_TOKEN`/`OS_ADMIN_TOKEN` 精确比对）：归因
    /// `"admin"`；不扣费。
    Admin,
    /// sk-os- 网关令牌（前缀匹配，有效性与余额在扣费时经 `try_charge_image`
    /// 判定）：归因令牌名；按 billing_mode 计费。
    GatewayKey {
        /// 原始 Bearer 凭据（去前缀后的完整 sk-os- key）。
        bearer: String,
    },
}

/// 一次生图调用的计费/归因上下文（charge_for_caller 产出，写入响应与 recent）。
struct BillingContext {
    charged: bool,
    /// `"free"` / `"image_credit"` / None（链上与 admin 不计费）。
    billing: Option<&'static str>,
    generated_by: Option<String>,
    generated_by_display: Option<String>,
}

/// 读系统 admin token env（与 nexhub_lobby 同款语义）：`NEXOS_ADMIN_TOKEN`
/// 优先，回落 `OS_ADMIN_TOKEN`；trim 后非空才算启用。
fn admin_token_from_env() -> Option<String> {
    std::env::var("NEXOS_ADMIN_TOKEN")
        .or_else(|_| std::env::var("OS_ADMIN_TOKEN"))
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// 媒体生成路由处理器——图片真实 spawn 生成 + 视频任务框架（内存态）+
/// 链上身份归因 + sk-os- 生图计费。
pub struct MediaGenRouteHandler {
    /// recent 环形（Vec，超容量逐出最旧）。
    recent: Mutex<Vec<ImageRecentItem>>,
    counter: Mutex<u64>,
    video_tasks: Mutex<Vec<VideoTask>>,
    video_counter: Mutex<u64>,
    /// 链上身份认证存储（challenge/verify 的 nonce/token 桶；独立实例——
    /// 与 IM/NexHub 的 token 桶互不相通，同一密钥可分别三处认证）。
    auth: Arc<ChainAuth>,
    /// 系统 admin token（构造时定格 env；None = 未配置 admin 通道）。
    admin_token: Option<String>,
    /// 共享 api_gateway 实例（sk-os- 生图计费入口 `try_charge_image`）。
    /// None = sk-os- 路径未接入（resolve 期即 401，明确不静默放行）。
    gateway: Option<Arc<ApiGatewayRouteHandler>>,
}

impl MediaGenRouteHandler {
    /// 构造 handler（空态起步：无 demo 数据，生成记录/任务由真实请求产生）；
    /// 独立 ChainAuth + env admin token，**未接** sk-os- 计费（测试/最小部署）。
    #[must_use]
    pub fn new() -> Self {
        Self::open(Arc::new(ChainAuth::new()), admin_token_from_env(), None)
    }

    /// main.rs 装配构造：注入**共享**链上认证存储（照 nexhub 的 with_chain_auth
    /// 模式——装配层与 handler 验同一批 token）。链式接 [`Self::with_gateway`]。
    #[must_use]
    pub fn with_chain_auth(auth: Arc<ChainAuth>) -> Self {
        Self::open(auth, admin_token_from_env(), None)
    }

    /// 链式注入共享 api_gateway 实例（sk-os- 生图计费；须与 api_gateway 组件
    /// **同一实例**——查-检-扣的原子性依赖同一 `Mutex<Connection>`）。
    #[must_use]
    pub fn with_gateway(mut self, gateway: Arc<ApiGatewayRouteHandler>) -> Self {
        self.gateway = Some(gateway);
        self
    }

    /// 链式注入系统 admin token（测试用：绕开 env 的并行竞态；生产路径经
    /// [`admin_token_from_env`] 构造时定格）。
    #[must_use]
    pub fn with_admin_token(mut self, token: &str) -> Self {
        self.admin_token = Some(token.to_string());
        self
    }

    fn open(
        auth: Arc<ChainAuth>,
        admin_token: Option<String>,
        gateway: Option<Arc<ApiGatewayRouteHandler>>,
    ) -> Self {
        Self {
            recent: Mutex::new(vec![]),
            counter: Mutex::new(0),
            video_tasks: Mutex::new(vec![]),
            video_counter: Mutex::new(0),
            auth,
            admin_token,
            gateway,
        }
    }

    fn next_id(&self, prefix: &str) -> String {
        let mut c = self.counter.lock().expect("counter poisoned");
        *c += 1;
        format!("{prefix}-{}", *c)
    }

    fn next_video_id(&self) -> String {
        let mut c = self.video_counter.lock().expect("video counter poisoned");
        *c += 1;
        format!("vid-{}", *c)
    }

    /// recent 环形快照（时间序，最旧在前）。
    #[must_use]
    pub fn recent_snapshot(&self) -> Vec<ImageRecentItem> {
        self.recent.lock().expect("recent poisoned").clone()
    }

    /// 视频任务快照（创建序）。
    #[must_use]
    pub fn video_tasks_snapshot(&self) -> Vec<VideoTask> {
        self.video_tasks
            .lock()
            .expect("video tasks poisoned")
            .clone()
    }

    /// 写入 recent 环形（超 [`RECENT_CAP`] 逐出最旧一条）。
    fn push_recent(&self, item: ImageRecentItem) {
        let mut r = self.recent.lock().expect("recent poisoned");
        r.push(item);
        let overflow = r.len().saturating_sub(RECENT_CAP);
        for _ in 0..overflow {
            r.remove(0);
        }
    }

    /// 解析 `POST /api/v1/media/image` 调用方身份（不含计费，见 [`ImageCaller`]）。
    ///
    /// - 链上 token 反查 pubkey（body/query 自报身份一律忽略）；
    /// - 无效/缺失 → admin token 精确比对；
    /// - 再无 → `sk-os-` 前缀走网关令牌（有效性在扣费时判定）；
    /// - 都不匹配 / sk-os- 但网关未注入 → `Err(401 响应)`。
    fn resolve_image_caller(&self, req: &ApiRequest) -> Result<ImageCaller, ApiResponse> {
        let Some(token) = chain_auth::bearer_token(&req.headers) else {
            return Err(error_response(
                401,
                "缺少有效身份：链上 token / 系统 admin / sk-os- 网关令牌三选一",
            ));
        };
        if let Some(pubkey) = self.auth.verify_token(token) {
            if let Some(vk) = chain_auth::parse_pubkey(&pubkey) {
                return Ok(ImageCaller::Chain {
                    pubkey,
                    display_name: chain_auth::derive_display_name(&vk),
                });
            }
        }
        if self.admin_token.as_deref() == Some(token) {
            return Ok(ImageCaller::Admin);
        }
        if token.starts_with(SK_OS_PREFIX) {
            if self.gateway.is_none() {
                return Err(error_response(
                    401,
                    "sk-os- 令牌路径未接入（未注入 api_gateway 共享实例）",
                ));
            }
            return Ok(ImageCaller::GatewayKey {
                bearer: token.to_string(),
            });
        }
        Err(error_response(
            401,
            "缺少有效身份：链上 token / 系统 admin / sk-os- 网关令牌三选一",
        ))
    }

    /// 生成前计费落账 + 归因上下文（仅 sk-os- 路径真正扣费）。
    ///
    /// 调用时机：参数校验与显存探测**之后**、GPU spawn **之前**——预检失败不
    /// 扣费、余额不足（402）不烧显存。链上/admin 直接构造 `billing=null` 上下文。
    /// sk-os- 错误映射：文案含 [`IMAGE_CHARGE_INSUFFICIENT_MARKER`] → 402，
    /// 其余（未命中/禁用/过期）→ 401。
    async fn charge_for_caller(&self, caller: &ImageCaller) -> Result<BillingContext, ApiResponse> {
        match caller {
            ImageCaller::Chain {
                pubkey,
                display_name,
            } => Ok(BillingContext {
                charged: false,
                billing: None,
                generated_by: Some(pubkey.clone()),
                generated_by_display: Some(display_name.clone()),
            }),
            ImageCaller::Admin => Ok(BillingContext {
                charged: false,
                billing: None,
                generated_by: Some("admin".to_string()),
                generated_by_display: None,
            }),
            ImageCaller::GatewayKey { bearer } => {
                let gateway = self
                    .gateway
                    .as_ref()
                    .expect("resolve_image_caller 已保证 sk-os- 路径注入网关");
                match gateway.try_charge_image(bearer).await {
                    Ok(outcome) => Ok(BillingContext {
                        charged: outcome.charged,
                        billing: Some(if outcome.charged {
                            BILLING_IMAGE_CREDIT
                        } else {
                            BILLING_FREE
                        }),
                        generated_by: Some(outcome.token_name),
                        generated_by_display: None,
                    }),
                    Err(msg) => {
                        let status = if msg.contains(IMAGE_CHARGE_INSUFFICIENT_MARKER) {
                            402
                        } else {
                            401
                        };
                        Err(error_response(status, &msg))
                    }
                }
            }
        }
    }

    /// 图片生成全流程：身份归因 → 校验 → 显存探测 → sk-os- 计费落账 → 脚本落盘
    /// → spawn → 读 PNG → base64。
    ///
    /// 错误语义：身份缺失 401（caller 侧）；参数非法 400；显存不足/探测不可用
    /// 503；计费余额不足 402 / 令牌无效 401；生成失败/超时 502（均以
    /// `Ok(error_response(..))` 返回，仅序列化失败走 `Err`）。
    async fn generate_image(
        &self,
        body: &ImageGenBody,
        caller: ImageCaller,
    ) -> Result<ApiResponse, ApiGatewayError> {
        let prompt = body.prompt.trim().to_string();
        let width = body.width.unwrap_or(768);
        let height = body.height.unwrap_or(432);
        let steps = body.steps.unwrap_or(4);
        // 1. 参数校验（400；宽高只校验显式传入值，见 validate_image_params 文档）
        if let Err(msg) = validate_image_params(&prompt, body.width, body.height, steps) {
            return Ok(error_response(400, &msg));
        }
        // 2. 显存探测（探测不可用 / 空闲不足 → 503，先于扣费与任何 spawn）
        let free_mib = match probe_vram_free_mib().await {
            Ok(v) => v,
            Err(e) => return Ok(error_response(503, &e)),
        };
        if let Err(msg) = vram_gate(free_mib) {
            return Ok(error_response(503, &msg));
        }
        // 3. sk-os- 计费落账（预检全过后、spawn 前最后一步：402 余额不足闸门 /
        //    401 令牌无效；链上/admin 不扣费 billing=null）
        let billing = match self.charge_for_caller(&caller).await {
            Ok(ctx) => ctx,
            Err(resp) => return Ok(resp),
        };
        // 4. 产物目录 + 脚本落盘 + spawn（失败 → 502 带 stderr 摘要）
        if let Err(e) = tokio::fs::create_dir_all(MEDIA_GEN_DIR).await {
            return Ok(error_response(502, &format!("创建产物目录失败: {e}")));
        }
        let script = match ensure_imggen_script(&imggen_script()).await {
            Ok(s) => s,
            Err(e) => return Ok(error_response(502, &e)),
        };
        let id = self.next_id("img");
        let job = ImageJob {
            out_path: format!("{MEDIA_GEN_DIR}/{}.png", file_token(&id)),
            prompt: prompt.clone(),
            width,
            height,
            steps,
        };
        let started = Instant::now();
        if let Err(e) = run_imggen_with(&imggen_bin(), &script, &job).await {
            return Ok(error_response(502, &e));
        }
        let elapsed_ms = started.elapsed().as_millis() as u64;
        // 5. 读 PNG → base64（读不到文件 = 生成失败语义，502）
        let png = match tokio::fs::read(&job.out_path).await {
            Ok(b) => b,
            Err(e) => {
                return Ok(error_response(
                    502,
                    &format!("读取生成产物失败 {}: {e}", job.out_path),
                ));
            }
        };
        use base64::Engine;
        let png_base64 = base64::engine::general_purpose::STANDARD.encode(&png);
        // 6. recent 环形（不含 base64；带归因）
        self.push_recent(ImageRecentItem {
            id: id.clone(),
            prompt_summary: prompt.chars().take(120).collect(),
            width,
            height,
            steps,
            elapsed_ms,
            created_at: now_iso(),
            generated_by: billing.generated_by.clone(),
            generated_by_display: billing.generated_by_display.clone(),
        });
        Ok(ok_json(to_value(&ImageGenResponse {
            id,
            png_base64,
            width,
            height,
            elapsed_ms,
            file_path: job.out_path,
            charged: billing.charged,
            billing: billing.billing.map(String::from),
            generated_by: billing.generated_by,
            generated_by_display: billing.generated_by_display,
        })?))
    }

    /// 视频任务创建：queued → 立即 submit → processing | failed（附指引）。
    async fn create_video_task(&self, body: &VideoGenBody) -> Result<ApiResponse, ApiGatewayError> {
        let prompt = body.prompt.trim().to_string();
        let duration = body.duration_secs.unwrap_or(VIDEO_DURATION_DEFAULT_SECS);
        let backend = body
            .backend
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("external")
            .to_string();
        if let Err(msg) = validate_video_params(&prompt, duration, &backend) {
            return Ok(error_response(400, &msg));
        }
        let mut task = VideoTask {
            id: self.next_video_id(),
            prompt,
            duration_secs: duration,
            backend: backend.clone(),
            status: "queued".to_string(),
            video_url: None,
            error: None,
            created_at: now_iso(),
        };
        // 立即尝试提交：占位后端一律 Err → 任务即 failed（诚实，不假装排队）。
        match backend_for(&backend).submit(&task).await {
            Ok(()) => task.status = "processing".to_string(),
            Err(e) => {
                task.status = "failed".to_string();
                task.error = Some(format!("{e}；{}", video_failure_guidance(&backend)));
            }
        }
        let resp_body = to_value(&task)?;
        self.video_tasks
            .lock()
            .expect("video tasks poisoned")
            .push(task);
        Ok(ApiResponse {
            status: 202,
            body: resp_body,
            headers: serde_json::json!({}),
        })
    }
}

impl Default for MediaGenRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RouteHandler for MediaGenRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            // —— 链上身份认证（公开挑战-签名，IM/NexHub 同款契约）——
            spec(
                HttpMethod::Post,
                "/api/v1/media/auth/challenge",
                false,
                vec![],
            ),
            spec(HttpMethod::Post, "/api/v1/media/auth/verify", false, vec![]),
            // 生图：handler 内自验（链上 token / admin / sk-os-，见 resolve_image_caller）
            // ——网关中间件无法识别链上/sk-os- token，requires_auth=false 同 NexHub 惯例
            spec(HttpMethod::Post, "/api/v1/media/image", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/media/image/recent", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/media/video",
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Get, "/api/v1/media/video/tasks", false, vec![]),
            spec(
                HttpMethod::Get,
                "/api/v1/media/video/tasks/:id",
                false,
                vec![],
            ),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            // —— POST /api/v1/media/auth/challenge —— 签发挑战 nonce（公开）
            //    body: {pubkey} → {nonce, expires_in, display_name}
            (HttpMethod::Post, ["api", "v1", "media", "auth", "challenge"]) => {
                #[derive(Deserialize)]
                struct ChallengeBody {
                    pubkey: String,
                }
                let body: ChallengeBody = serde_json::from_value(req.body)
                    .map_err(|e| ApiGatewayError::Internal(format!("解析挑战请求体失败: {e}")))?;
                let vk = match chain_auth::parse_pubkey(&body.pubkey) {
                    Some(v) => v,
                    None => {
                        return Ok(error_response(
                            400,
                            "pubkey 非法：应为 0x + 66 hex（33 字节压缩 secp256k1）",
                        ));
                    }
                };
                let nonce = self.auth.create_nonce(&body.pubkey);
                Ok(ok_json(serde_json::json!({
                    "nonce": nonce,
                    "expires_in": chain_auth::NONCE_TTL_SECS,
                    "display_name": chain_auth::derive_display_name(&vk),
                })))
            }

            // —— POST /api/v1/media/auth/verify —— 验签 + 签发 token（公开）
            //    body: {pubkey, nonce, signature(0x+130 hex, 65 字节 r||s||v)}
            //    → {token, expires_in, pubkey, display_name}（24h 单点登录）
            (HttpMethod::Post, ["api", "v1", "media", "auth", "verify"]) => {
                #[derive(Deserialize)]
                struct VerifyBody {
                    pubkey: String,
                    nonce: String,
                    signature: String,
                }
                let body: VerifyBody = serde_json::from_value(req.body)
                    .map_err(|e| ApiGatewayError::Internal(format!("解析验签请求体失败: {e}")))?;
                let vk = match chain_auth::parse_pubkey(&body.pubkey) {
                    Some(v) => v,
                    None => {
                        return Ok(error_response(
                            400,
                            "pubkey 非法：应为 0x + 66 hex（33 字节压缩 secp256k1）",
                        ));
                    }
                };
                let sig_hex = body.signature.trim().trim_start_matches("0x");
                let sig = match hex::decode(sig_hex) {
                    Ok(s) if s.len() == 65 => s,
                    _ => {
                        return Ok(error_response(
                            400,
                            "signature 非法：应为 65 字节 r||s||v 的 hex（可带 0x 前缀）",
                        ));
                    }
                };
                // nonce 用后即焚（签名失败同样烧掉，防暴力尝试）
                if !self.auth.take_nonce(&body.pubkey, &body.nonce) {
                    return Ok(error_response(401, "nonce 无效、已用或已过期（60s）"));
                }
                if !chain_auth::verify_nonce_signature(&vk, &body.nonce, &sig) {
                    return Ok(error_response(401, "签名验证失败"));
                }
                let (token, expires_in) = self.auth.issue_token(&body.pubkey);
                Ok(ok_json(serde_json::json!({
                    "token": token,
                    "expires_in": expires_in,
                    "pubkey": body.pubkey,
                    "display_name": chain_auth::derive_display_name(&vk),
                })))
            }

            // —— POST /api/v1/media/image —— 真实 sd-turbo 生图
            //    身份：链上 token → admin → sk-os-（handler 内自验，都无 401）；
            //    计费仅 sk-os- 路径（生成前落账，见 generate_image）
            (HttpMethod::Post, ["api", "v1", "media", "image"]) => {
                let body: ImageGenBody = serde_json::from_value(req.body.clone())
                    .map_err(|e| ApiGatewayError::Internal(format!("解析生图请求体失败: {e}")))?;
                let caller = match self.resolve_image_caller(&req) {
                    Ok(c) => c,
                    Err(resp) => return Ok(resp),
                };
                self.generate_image(&body, caller).await
            }

            // —— GET /api/v1/media/image/recent —— 近期生成记录（不含 base64）
            (HttpMethod::Get, ["api", "v1", "media", "image", "recent"]) => {
                Ok(ok_json(to_value(&self.recent_snapshot())?))
            }

            // —— POST /api/v1/media/video —— 创建视频任务（需 admin）
            (HttpMethod::Post, ["api", "v1", "media", "video"]) => {
                let body: VideoGenBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析视频任务请求体失败: {e}"))
                })?;
                self.create_video_task(&body).await
            }

            // —— GET /api/v1/media/video/tasks —— 任务列表
            (HttpMethod::Get, ["api", "v1", "media", "video", "tasks"]) => {
                Ok(ok_json(to_value(&self.video_tasks_snapshot())?))
            }

            // —— GET /api/v1/media/video/tasks/:id —— 任务详情
            (HttpMethod::Get, ["api", "v1", "media", "video", "tasks", id]) => {
                let tasks = self.video_tasks.lock().expect("video tasks poisoned");
                match tasks.iter().find(|t| t.id == *id) {
                    Some(t) => Ok(ok_json(to_value(t)?)),
                    None => Ok(error_response(404, &format!("视频任务不存在: {id}"))),
                }
            }

            // —— 未覆盖路由 —— 兜底 404
            _ => Ok(error_response(404, "media-gen: 未匹配的路由")),
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
        handler_component: "media-gen".to_string(),
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

/// 产物文件名 token：id + 时间纳秒 + pid（进程内 counter 已保 id 唯一，纳秒/pid
/// 兜底跨重启不撞名）。
fn file_token(id: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{id}-{:x}-{:x}", nanos, std::process::id())
}

// ----------------------------------------------------------------------------
// 单元测试（真实子进程经 env 注入假脚本：NEXOS_SMI_BIN / NEXOS_IMGGEN_BIN）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// env 触碰互斥：并行测试下 env 是进程级全局，凡 set/remove 注入变量的测试
    /// 必须持此锁串行（forwarding.rs SSH_BIN_LOCK 同款思路）。
    static ENV_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

    /// env 快照恢复 guard：Drop 时把记录的变量恢复原值（未设置则移除）。
    struct EnvRestore(Vec<(&'static str, Option<String>)>);

    impl EnvRestore {
        fn new(vars: &[&'static str]) -> Self {
            Self(vars.iter().map(|v| (*v, std::env::var(v).ok())).collect())
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (k, prev) in &self.0 {
                match prev {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
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

    /// 带 Bearer 凭据的 POST（身份归因/计费路径测试用）。
    fn post_req_bearer(path: &str, body: serde_json::Value, bearer: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Post,
            path: path.into(),
            headers: serde_json::json!({ "Authorization": format!("Bearer {bearer}") }),
            body,
            auth: None,
        }
    }

    /// 固定 admin token 的 handler（绕开 env 并行竞态；身份解析测试的基准通道）。
    fn admin_handler() -> MediaGenRouteHandler {
        MediaGenRouteHandler::new().with_admin_token("test-admin")
    }

    /// admin 身份的生图请求。
    fn admin_image_req(body: serde_json::Value) -> ApiRequest {
        post_req_bearer("/api/v1/media/image", body, "test-admin")
    }

    /// 在共享网关实例上经 HTTP 创建令牌，返回 (完整 sk-os- key, 令牌名)。
    async fn gw_create_token(
        gw: &ApiGatewayRouteHandler,
        name: &str,
        billing_mode: &str,
        quota_limit: u64,
    ) -> (String, String) {
        let resp = gw
            .handle(post_req(
                "/api/v1/gateway/tokens",
                serde_json::json!({
                    "name": name,
                    "billing_mode": billing_mode,
                    "quota_limit": quota_limit,
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "seed 令牌创建失败: {resp:?}");
        (
            resp.body["key"].as_str().unwrap().to_string(),
            name.to_string(),
        )
    }

    /// 查网关令牌当前 quota_used（按 key 前缀定位，seed key 唯一即可）。
    fn gw_quota_used(gw: &ApiGatewayRouteHandler, key: &str) -> u64 {
        gw.tokens_snapshot()
            .iter()
            .find(|t| t.key == key)
            .unwrap_or_else(|| panic!("令牌 {key} 应存在"))
            .quota_used
    }

    /// 生成真 secp256k1 密钥对（CSPRNG，链上身份测试同栈）。
    fn new_key() -> k256::ecdsa::SigningKey {
        use k256::elliptic_curve::rand_core::OsRng;
        k256::ecdsa::SigningKey::random(&mut OsRng)
    }

    /// 私钥 → 用户名（0x + 66 hex 压缩公钥）。
    fn pubkey_hex(sk: &k256::ecdsa::SigningKey) -> String {
        format!(
            "0x{}",
            hex::encode(sk.verifying_key().to_encoded_point(true).as_bytes())
        )
    }

    /// 客户端签名：SHA-256(nonce UTF-8) → RFC6979 ECDSA（65 字节 r||s||v，0x hex）。
    fn sign_nonce(sk: &k256::ecdsa::SigningKey, nonce: &str) -> String {
        use sha2::Digest;
        let digest = sha2::Sha256::new_with_prefix(nonce.as_bytes());
        let (sig, recid) = sk.sign_digest_recoverable(digest).expect("签名必成功");
        let mut out = [0u8; 65];
        out[..64].copy_from_slice(&sig.to_bytes());
        out[64] = u8::from(recid);
        format!("0x{}", hex::encode(out))
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

    #[cfg(unix)]
    fn temp_dir_for(test: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nexos-mediagen-{test}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ---- 路由归属与鉴权矩阵 ----

    #[tokio::test]
    async fn routes_declares_seven_endpoints_media_gen_with_auth_matrix() {
        let h = MediaGenRouteHandler::new();
        let routes = h.routes().await;
        assert_eq!(routes.len(), 7, "应有 7 条路由: {routes:?}");
        assert!(
            routes.iter().all(|r| r.handler_component == "media-gen"),
            "全部归属 media-gen 组件"
        );
        // 生图 POST：handler 内自验（链上/admin/sk-os- 三路），不走网关中间件
        let image = routes
            .iter()
            .find(|r| r.path == "/api/v1/media/image")
            .unwrap();
        assert!(!image.requires_auth, "生图身份在 handler 内自验");
        assert!(image.required_roles.is_empty(), "自验路由无角色要求");
        // 视频任务 POST：维持网关层 admin 鉴权
        let video = routes
            .iter()
            .find(|r| r.path == "/api/v1/media/video")
            .unwrap();
        assert!(video.requires_auth, "视频写操作仍需网关 auth");
        assert_eq!(video.required_roles, vec!["admin".to_string()]);
        // 认证 + 读操作（challenge/verify/recent/tasks/tasks/:id）公开
        for r in &routes {
            if r.method == HttpMethod::Get || r.path.contains("/auth/") {
                assert!(!r.requires_auth, "认证/GET 应公开: {r:?}");
                assert!(r.required_roles.is_empty(), "无角色要求: {r:?}");
            }
        }
        // 具体路径齐备
        let paths: Vec<&str> = routes.iter().map(|r| r.path.as_str()).collect();
        for expect in [
            "/api/v1/media/auth/challenge",
            "/api/v1/media/auth/verify",
            "/api/v1/media/image",
            "/api/v1/media/image/recent",
            "/api/v1/media/video",
            "/api/v1/media/video/tasks",
            "/api/v1/media/video/tasks/:id",
        ] {
            assert!(paths.contains(&expect), "缺路由 {expect}: {paths:?}");
        }
    }

    // ---- 图片参数校验矩阵（admin 通道；401 身份闸在前，见下方身份测试） ----

    #[tokio::test]
    async fn image_rejects_empty_prompt() {
        let h = admin_handler();
        let resp = h
            .handle(admin_image_req(serde_json::json!({"prompt": "   "})))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        assert!(resp.body["error"].as_str().unwrap().contains("prompt"));
    }

    #[tokio::test]
    async fn image_rejects_prompt_over_2000_chars() {
        let h = admin_handler();
        let resp = h
            .handle(admin_image_req(
                serde_json::json!({"prompt": "x".repeat(2001)}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        assert!(resp.body["error"].as_str().unwrap().contains("2000"));
    }

    #[tokio::test]
    async fn image_rejects_dimensions_not_multiple_of_64() {
        let h = admin_handler();
        let resp = h
            .handle(admin_image_req(
                serde_json::json!({"prompt": "a cat", "width": 100}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "width=100 非 64 倍数应 400");
        let resp = h
            .handle(admin_image_req(
                serde_json::json!({"prompt": "a cat", "height": 433}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "height=433 非 64 倍数应 400");
    }

    #[tokio::test]
    async fn image_rejects_dimensions_out_of_range() {
        let h = admin_handler();
        let resp = h
            .handle(admin_image_req(
                serde_json::json!({"prompt": "a cat", "width": 192}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "width=192 < 256 应 400");
        let resp = h
            .handle(admin_image_req(
                serde_json::json!({"prompt": "a cat", "height": 1088}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "height=1088 > 1024 应 400");
    }

    #[test]
    fn image_default_dimensions_exempt_from_64_multiple_rule() {
        // 默认 768×432（壁纸管线同款 16:9；432 非 64 倍数）放行——64 倍数规则仅
        // 约束显式传入值（防 433/100 笔误），显式 432 同样拒绝以保持规则一致。
        assert!(
            validate_image_params("a cat", None, None, 4).is_ok(),
            "默认宽高应放行（432 非 64 倍数但为契约默认值）"
        );
        assert!(validate_image_params("a cat", Some(512), Some(512), 4).is_ok());
        assert!(
            validate_image_params("a cat", Some(768), Some(432), 4).is_err(),
            "显式传 432 应按 64 倍数规则拒绝"
        );
    }

    #[tokio::test]
    async fn image_rejects_steps_out_of_range() {
        let h = admin_handler();
        for steps in [0u32, 9] {
            let resp = h
                .handle(admin_image_req(
                    serde_json::json!({"prompt": "a cat", "steps": steps}),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 400, "steps={steps} 应 400");
        }
    }

    #[tokio::test]
    async fn image_missing_prompt_field_fails_deserialize() {
        let h = MediaGenRouteHandler::new();
        let resp = h
            .handle(post_req(
                "/api/v1/media/image",
                serde_json::json!({"width": 768}),
            ))
            .await;
        assert!(resp.is_err(), "缺 prompt 字段应反序列化失败");
    }

    // ---- 显存探测分支（抽函数 + 输出注入） ----

    #[test]
    fn parse_vram_free_mib_takes_max_across_gpus() {
        assert_eq!(parse_vram_free_mib("23552\n"), Some(23_552));
        assert_eq!(parse_vram_free_mib("5120\n 24000\n"), Some(24_000));
        assert_eq!(parse_vram_free_mib(""), None, "空输出不可解析");
        assert_eq!(parse_vram_free_mib("not-a-number\n"), None);
        // DGX Spark GB10 实测形态（2026-09-03）：memory.free 报 [N/A]
        assert_eq!(parse_vram_free_mib("[N/A]\n"), None, "N/A 纯解析层不可解析");
    }

    /// GB10 统一内存回退：假 smi 输出 `[N/A]` → probe 落 /proc/meminfo
    /// MemAvailable（Linux 测试环境恒可读，>0），不再误 503"输出不可解析"。
    #[cfg(unix)]
    #[tokio::test]
    async fn probe_vram_na_falls_back_to_unified_meminfo() {
        let dir = temp_dir_for("vram-na");
        // DGX Spark GB10 实测输出形态
        let smi = fake_exec(&dir, "fake-smi-na.sh", "#!/bin/sh\necho '[N/A]'\n");
        let v = probe_vram_free_mib_with(smi.to_str().unwrap())
            .await
            .expect("统一内存形态应回退 meminfo 而非 Err");
        assert!(v > 0, "MemAvailable MiB 应为正: {v}");
    }

    /// 非统一内存的坏输出（无 [N/A] 标记）仍拒绝：无法确认安全，默认 503。
    #[cfg(unix)]
    #[tokio::test]
    async fn probe_vram_garbage_without_na_still_errors() {
        let dir = temp_dir_for("vram-garbage");
        let smi = fake_exec(&dir, "fake-smi-bad.sh", "#!/bin/sh\necho garbage\n");
        let err = probe_vram_free_mib_with(smi.to_str().unwrap())
            .await
            .expect_err("坏输出不可解析应 Err");
        assert!(err.contains("不可解析"), "应保持默认拒绝语义: {err}");
    }

    #[test]
    fn vram_gate_branches_on_6000_mib_threshold() {
        assert!(vram_gate(6000).is_ok(), "6000 MiB 恰好达标应放行");
        let err = vram_gate(5999).expect_err("5999 MiB 应拦截");
        assert!(err.contains("显存不足"), "缺显存提示: {err}");
        assert!(err.contains("先停 LLM 实例"), "缺操作指引: {err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn image_low_vram_returns_503_with_stop_llm_hint() {
        let _guard = ENV_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let _restore = EnvRestore::new(&["NEXOS_SMI_BIN", "NEXOS_IMGGEN_BIN"]);
        let dir = temp_dir_for("low-vram");
        // 假 nvidia-smi：输出 5120 MiB（< 6000）
        let smi = fake_exec(&dir, "fake-smi.sh", "#!/bin/sh\necho 5120\n");
        // 假生图 bin：若被调到则失败（断言探测先行拦截）
        let imggen = fake_exec(&dir, "fake-imggen-fail.sh", "#!/bin/sh\nexit 9\n");
        std::env::set_var("NEXOS_SMI_BIN", smi.to_str().unwrap());
        std::env::set_var("NEXOS_IMGGEN_BIN", imggen.to_str().unwrap());
        let h = admin_handler();
        let resp = h
            .handle(admin_image_req(serde_json::json!({"prompt": "a cat"})))
            .await
            .unwrap();
        assert_eq!(resp.status, 503, "body: {resp:?}");
        let err = resp.body["error"].as_str().unwrap();
        assert!(err.contains("显存不足"), "缺显存提示: {err}");
        assert!(err.contains("先停 LLM 实例"), "缺操作指引: {err}");
        assert!(err.contains("5120"), "应带实测空闲值: {err}");
        // 拦截在先：不产生 recent 记录
        assert!(h.recent_snapshot().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn image_smi_unavailable_returns_503() {
        let _guard = ENV_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let _restore = EnvRestore::new(&["NEXOS_SMI_BIN"]);
        // 探测不可用（/bin/false 退出非零）：无法确认安全 → 默认拒绝 503
        std::env::set_var("NEXOS_SMI_BIN", "/bin/false");
        let h = admin_handler();
        let resp = h
            .handle(admin_image_req(serde_json::json!({"prompt": "a cat"})))
            .await
            .unwrap();
        assert_eq!(resp.status, 503, "body: {resp:?}");
        assert!(
            resp.body["error"]
                .as_str()
                .unwrap()
                .contains("无法探测显存"),
            "应说明探测失败"
        );
    }

    // ---- 生成成功路径（NEXOS_IMGGEN_BIN 注入假脚本输出 PNG 字节） ----

    #[cfg(unix)]
    #[tokio::test]
    async fn image_generation_success_with_fake_bin() {
        let _guard = ENV_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let _restore = EnvRestore::new(&["NEXOS_SMI_BIN", "NEXOS_IMGGEN_BIN"]);
        let dir = temp_dir_for("success");
        let smi = fake_exec(&dir, "fake-smi.sh", "#!/bin/sh\necho 24000\n");
        // 假生图：把固定 PNG 魔数字节写到 $NEXOS_IMGGEN_OUT
        let imggen = fake_exec(
            &dir,
            "fake-imggen.sh",
            "#!/bin/sh\nprintf '\\211PNG\\015\\012\\032\\012fakepngdata' > \"$NEXOS_IMGGEN_OUT\"\n",
        );
        std::env::set_var("NEXOS_SMI_BIN", smi.to_str().unwrap());
        std::env::set_var("NEXOS_IMGGEN_BIN", imggen.to_str().unwrap());
        let h = admin_handler();
        let resp = h
            .handle(admin_image_req(
                serde_json::json!({"prompt": "a cat", "width": 512, "height": 512}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "body: {resp:?}");
        assert!(
            resp.body["id"].as_str().unwrap().starts_with("img-"),
            "id 应 img- 前缀"
        );
        assert_eq!(resp.body["width"], 512);
        assert_eq!(resp.body["height"], 512);
        assert!(
            resp.body["elapsed_ms"].as_u64().is_some(),
            "elapsed_ms 应为数值"
        );
        let file_path = resp.body["file_path"].as_str().unwrap().to_string();
        assert!(
            file_path.starts_with("/tmp/media-gen/"),
            "产物应落 /tmp/media-gen: {file_path}"
        );
        // base64 解码回 PNG 字节（与假脚本写入一致）
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(resp.body["png_base64"].as_str().unwrap())
            .expect("png_base64 应可解码");
        assert_eq!(bytes, b"\x89PNG\r\n\x1a\nfakepngdata");
        // admin 身份归因 + 不计费
        assert_eq!(resp.body["generated_by"], "admin", "admin 归因");
        assert!(resp.body["generated_by_display"].is_null());
        assert!(!resp.body["charged"].as_bool().unwrap(), "admin 不扣费");
        assert!(resp.body["billing"].is_null(), "admin billing=null");
        // recent 含该条（不含 base64 字段；带归因）
        let recent = h.recent_snapshot();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].id, resp.body["id"]);
        assert_eq!(recent[0].prompt_summary, "a cat");
        assert_eq!(recent[0].width, 512);
        assert_eq!(recent[0].steps, 4, "steps 默认 4");
        assert_eq!(recent[0].generated_by.as_deref(), Some("admin"));
        assert!(recent[0].generated_by_display.is_none());
        // GET recent 与快照一致且无 base64 泄漏
        let resp = h
            .handle(get_req("/api/v1/media/image/recent"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert!(arr[0].get("png_base64").is_none(), "recent 不含 base64");
        assert_eq!(arr[0]["generated_by"], "admin", "recent 归因回显");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn image_generation_failure_returns_502_with_stderr_summary() {
        let _guard = ENV_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let _restore = EnvRestore::new(&["NEXOS_SMI_BIN", "NEXOS_IMGGEN_BIN"]);
        let dir = temp_dir_for("fail");
        let smi = fake_exec(&dir, "fake-smi.sh", "#!/bin/sh\necho 24000\n");
        let imggen = fake_exec(
            &dir,
            "fake-imggen-fail.sh",
            "#!/bin/sh\necho 'boom: sd-turbo model missing' >&2\nexit 3\n",
        );
        std::env::set_var("NEXOS_SMI_BIN", smi.to_str().unwrap());
        std::env::set_var("NEXOS_IMGGEN_BIN", imggen.to_str().unwrap());
        let h = admin_handler();
        let resp = h
            .handle(admin_image_req(serde_json::json!({"prompt": "a cat"})))
            .await
            .unwrap();
        assert_eq!(resp.status, 502, "body: {resp:?}");
        let err = resp.body["error"].as_str().unwrap();
        assert!(err.contains("boom"), "应带 stderr 摘要: {err}");
        assert!(h.recent_snapshot().is_empty(), "失败不入 recent");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn image_generation_timeout_returns_502() {
        let _guard = ENV_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let _restore = EnvRestore::new(&[
            "NEXOS_SMI_BIN",
            "NEXOS_IMGGEN_BIN",
            "NEXOS_IMGGEN_TIMEOUT_SECS",
        ]);
        let dir = temp_dir_for("timeout");
        let smi = fake_exec(&dir, "fake-smi.sh", "#!/bin/sh\necho 24000\n");
        // 假生图卡 5s，超时覆写为 1s → 超时分支 kill
        let imggen = fake_exec(&dir, "fake-imggen-slow.sh", "#!/bin/sh\nexec sleep 5\n");
        std::env::set_var("NEXOS_SMI_BIN", smi.to_str().unwrap());
        std::env::set_var("NEXOS_IMGGEN_BIN", imggen.to_str().unwrap());
        std::env::set_var("NEXOS_IMGGEN_TIMEOUT_SECS", "1");
        let h = admin_handler();
        let started = Instant::now();
        let resp = h
            .handle(admin_image_req(serde_json::json!({"prompt": "a cat"})))
            .await
            .unwrap();
        assert_eq!(resp.status, 502, "body: {resp:?}");
        assert!(
            resp.body["error"].as_str().unwrap().contains("超时"),
            "应说明超时: {resp:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "应在超时后及时返回（kill 生效），耗时 {:?}",
            started.elapsed()
        );
    }

    // ---- recent 环形 ----

    #[tokio::test]
    async fn recent_ring_caps_at_50_evicting_oldest() {
        let h = MediaGenRouteHandler::new();
        for i in 0..55 {
            h.push_recent(ImageRecentItem {
                id: format!("img-{i}"),
                prompt_summary: format!("prompt-{i}"),
                width: 768,
                height: 432,
                steps: 4,
                elapsed_ms: 100,
                created_at: now_iso(),
                generated_by: None,
                generated_by_display: None,
            });
        }
        let recent = h.recent_snapshot();
        assert_eq!(recent.len(), RECENT_CAP, "应钳制在 50 条");
        // 最旧 5 条（img-0..img-4）被逐出，首条为 img-5，末条为 img-54
        assert_eq!(recent[0].id, "img-5", "最旧应被逐出");
        assert_eq!(recent[49].id, "img-54", "最新应保留");
        // 空态 recent 端点返回空数组
        let h2 = MediaGenRouteHandler::new();
        let resp = h2
            .handle(get_req("/api/v1/media/image/recent"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body.as_array().unwrap().len(), 0);
    }

    // ====================================================================
    // 链上身份归因 + sk-os- 生图计费（2026-08-20 变现闭环接线）
    // ====================================================================

    #[tokio::test]
    async fn no_identity_image_returns_401() {
        // 无 Authorization / 非 Bearer / 未知裸 token（非 sk-os- 前缀）→ 401
        let h = admin_handler();
        let resp = h
            .handle(post_req(
                "/api/v1/media/image",
                serde_json::json!({"prompt": "a cat"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 401, "body: {resp:?}");
        let err = resp.body["error"].as_str().unwrap();
        assert!(err.contains("三选一"), "应指引三种身份: {err}");
        // 未知裸 token（非 sk-os- 前缀不进网关查表）
        let resp = h
            .handle(post_req_bearer(
                "/api/v1/media/image",
                serde_json::json!({"prompt": "a cat"}),
                "bogus-token",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 401);
        // 401 在显存探测之前：不产生 recent 记录
        assert!(h.recent_snapshot().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn chain_token_image_attributed_to_pubkey() {
        let _guard = ENV_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let _restore = EnvRestore::new(&["NEXOS_SMI_BIN", "NEXOS_IMGGEN_BIN"]);
        let dir = temp_dir_for("chain-attr");
        let smi = fake_exec(&dir, "fake-smi.sh", "#!/bin/sh\necho 24000\n");
        let imggen = fake_exec(
            &dir,
            "fake-imggen.sh",
            "#!/bin/sh\nprintf 'png' > \"$NEXOS_IMGGEN_OUT\"\n",
        );
        std::env::set_var("NEXOS_SMI_BIN", smi.to_str().unwrap());
        std::env::set_var("NEXOS_IMGGEN_BIN", imggen.to_str().unwrap());
        // 链上身份：直接在注入的 ChainAuth 实例上签发（等价 verify 后持有 token）
        let auth = Arc::new(ChainAuth::new());
        let sk = new_key();
        let pubkey = pubkey_hex(&sk);
        let (token, _) = auth.issue_token(&pubkey);
        let h = MediaGenRouteHandler::with_chain_auth(auth).with_admin_token("test-admin");
        let resp = h
            .handle(post_req_bearer(
                "/api/v1/media/image",
                serde_json::json!({"prompt": "on-chain cat"}),
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "body: {resp:?}");
        assert_eq!(resp.body["generated_by"], pubkey, "归因应为 pubkey");
        assert_eq!(
            resp.body["generated_by_display"],
            chain_auth::derive_display_name(sk.verifying_key()),
            "展示名应为 pubkey 派生 EVM 地址"
        );
        assert!(!resp.body["charged"].as_bool().unwrap(), "链上身份不扣费");
        assert!(resp.body["billing"].is_null(), "链上身份 billing=null");
        // recent 同步带归因
        let recent = h.recent_snapshot();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].generated_by.as_deref(), Some(pubkey.as_str()));
        assert!(recent[0].generated_by_display.is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sk_os_free_token_generates_without_charge() {
        let _guard = ENV_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let _restore = EnvRestore::new(&["NEXOS_SMI_BIN", "NEXOS_IMGGEN_BIN"]);
        let dir = temp_dir_for("sk-free");
        let smi = fake_exec(&dir, "fake-smi.sh", "#!/bin/sh\necho 24000\n");
        let imggen = fake_exec(
            &dir,
            "fake-imggen.sh",
            "#!/bin/sh\nprintf 'png' > \"$NEXOS_IMGGEN_OUT\"\n",
        );
        std::env::set_var("NEXOS_SMI_BIN", smi.to_str().unwrap());
        std::env::set_var("NEXOS_IMGGEN_BIN", imggen.to_str().unwrap());
        let gw = Arc::new(ApiGatewayRouteHandler::with_empty());
        let (key, name) = gw_create_token(&gw, "内测free", "free", 0).await;
        let h = admin_handler().with_gateway(gw.clone());
        let resp = h
            .handle(post_req_bearer(
                "/api/v1/media/image",
                serde_json::json!({"prompt": "free cat"}),
                &key,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "body: {resp:?}");
        assert!(!resp.body["charged"].as_bool().unwrap(), "free 不扣费");
        assert_eq!(resp.body["billing"], "free", "billing 标签应为 free");
        assert_eq!(resp.body["generated_by"], name, "归因为令牌名");
        assert_eq!(gw_quota_used(&gw, &key), 0, "网关 quota_used 不变");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sk_os_per_image_token_charges_100_and_records_recent() {
        let _guard = ENV_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let _restore = EnvRestore::new(&["NEXOS_SMI_BIN", "NEXOS_IMGGEN_BIN"]);
        let dir = temp_dir_for("sk-charge");
        let smi = fake_exec(&dir, "fake-smi.sh", "#!/bin/sh\necho 24000\n");
        let imggen = fake_exec(
            &dir,
            "fake-imggen.sh",
            "#!/bin/sh\nprintf 'png' > \"$NEXOS_IMGGEN_OUT\"\n",
        );
        std::env::set_var("NEXOS_SMI_BIN", smi.to_str().unwrap());
        std::env::set_var("NEXOS_IMGGEN_BIN", imggen.to_str().unwrap());
        let gw = Arc::new(ApiGatewayRouteHandler::with_empty());
        let (key, name) = gw_create_token(&gw, "付费生图key", "per_image", 10_000).await;
        let h = admin_handler().with_gateway(gw.clone());
        let resp = h
            .handle(post_req_bearer(
                "/api/v1/media/image",
                serde_json::json!({"prompt": "paid cat"}),
                &key,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "body: {resp:?}");
        assert!(resp.body["charged"].as_bool().unwrap(), "per_image 应扣费");
        assert_eq!(resp.body["billing"], "image_credit");
        assert_eq!(resp.body["generated_by"], name);
        // 网关侧真实落账 100 积分（IMAGE_PRICE_CREDITS）
        assert_eq!(
            gw_quota_used(&gw, &key),
            crate::handlers::api_gateway::IMAGE_PRICE_CREDITS
        );
        // recent 记录带归因（令牌名）
        let recent = h.recent_snapshot();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].generated_by.as_deref(), Some(name.as_str()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sk_os_insufficient_quota_returns_402_before_generation() {
        let _guard = ENV_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let _restore = EnvRestore::new(&["NEXOS_SMI_BIN", "NEXOS_IMGGEN_BIN"]);
        let dir = temp_dir_for("sk-402");
        // 显存充足 + 生图脚本若被调到会失败（退出 9）——402 闸门必须在 spawn 前
        let smi = fake_exec(&dir, "fake-smi.sh", "#!/bin/sh\necho 24000\n");
        let imggen = fake_exec(&dir, "fake-imggen-fail.sh", "#!/bin/sh\nexit 9\n");
        std::env::set_var("NEXOS_SMI_BIN", smi.to_str().unwrap());
        std::env::set_var("NEXOS_IMGGEN_BIN", imggen.to_str().unwrap());
        let gw = Arc::new(ApiGatewayRouteHandler::with_empty());
        // 余额 50 < 单价 100 → 402
        let (key, _name) = gw_create_token(&gw, "余额不足key", "per_image", 50).await;
        let h = admin_handler().with_gateway(gw.clone());
        let resp = h
            .handle(post_req_bearer(
                "/api/v1/media/image",
                serde_json::json!({"prompt": "poor cat"}),
                &key,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 402, "余额不足应 402: {resp:?}");
        let err = resp.body["error"].as_str().unwrap();
        assert!(err.contains("余额不足"), "应带余额不足提示: {err}");
        assert!(err.contains("充值"), "应带充值指引: {err}");
        // 不产生生成记录、不扣费、不烧 GPU（imggen 失败脚本未被调到——否则 502）
        assert!(h.recent_snapshot().is_empty());
        assert_eq!(gw_quota_used(&gw, &key), 0, "拒绝时不得扣费");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sk_os_unknown_key_rejected_401() {
        let _guard = ENV_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let _restore = EnvRestore::new(&["NEXOS_SMI_BIN", "NEXOS_IMGGEN_BIN"]);
        let dir = temp_dir_for("sk-unknown");
        let smi = fake_exec(&dir, "fake-smi.sh", "#!/bin/sh\necho 24000\n");
        let imggen = fake_exec(&dir, "fake-imggen-fail.sh", "#!/bin/sh\nexit 9\n");
        std::env::set_var("NEXOS_SMI_BIN", smi.to_str().unwrap());
        std::env::set_var("NEXOS_IMGGEN_BIN", imggen.to_str().unwrap());
        let gw = Arc::new(ApiGatewayRouteHandler::with_empty());
        let h = admin_handler().with_gateway(gw.clone());
        let resp = h
            .handle(post_req_bearer(
                "/api/v1/media/image",
                serde_json::json!({"prompt": "a cat"}),
                "sk-os-doesnotexist",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 401, "未知 sk-os- 应 401: {resp:?}");
        assert!(
            resp.body["error"]
                .as_str()
                .unwrap()
                .contains("无效的 API Key"),
            "应说明令牌未命中"
        );
        assert!(h.recent_snapshot().is_empty());
    }

    #[tokio::test]
    async fn sk_os_without_gateway_injection_rejected_401() {
        // sk-os- 前缀但未注入网关（装配缺失）→ 明确 401，不静默放行
        let h = admin_handler(); // 无 with_gateway
        let resp = h
            .handle(post_req_bearer(
                "/api/v1/media/image",
                serde_json::json!({"prompt": "a cat"}),
                "sk-os-anykey",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 401, "body: {resp:?}");
        assert!(
            resp.body["error"]
                .as_str()
                .unwrap()
                .contains("sk-os- 令牌路径未接入"),
            "应说明未接入: {resp:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn charge_skipped_on_bad_params_and_low_vram() {
        let _guard = ENV_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let _restore = EnvRestore::new(&["NEXOS_SMI_BIN", "NEXOS_IMGGEN_BIN"]);
        let dir = temp_dir_for("skip-charge");
        // 显存不足（5120 < 6000）：计费落账在显存探测之后 → 不扣
        let smi_low = fake_exec(&dir, "fake-smi-low.sh", "#!/bin/sh\necho 5120\n");
        let smi_ok = fake_exec(&dir, "fake-smi-ok.sh", "#!/bin/sh\necho 24000\n");
        let imggen = fake_exec(
            &dir,
            "fake-imggen.sh",
            "#!/bin/sh\nprintf 'png' > \"$NEXOS_IMGGEN_OUT\"\n",
        );
        std::env::set_var("NEXOS_IMGGEN_BIN", imggen.to_str().unwrap());
        let gw = Arc::new(ApiGatewayRouteHandler::with_empty());
        let (key, _) = gw_create_token(&gw, "预检顺序key", "per_image", 10_000).await;
        let h = admin_handler().with_gateway(gw.clone());
        // (a) 参数非法（400）→ 不扣费
        std::env::set_var("NEXOS_SMI_BIN", smi_ok.to_str().unwrap());
        let resp = h
            .handle(post_req_bearer(
                "/api/v1/media/image",
                serde_json::json!({"prompt": "a cat", "width": 100}),
                &key,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        assert_eq!(gw_quota_used(&gw, &key), 0, "参数 400 不应扣费");
        // (b) 显存不足（503）→ 不扣费
        std::env::set_var("NEXOS_SMI_BIN", smi_low.to_str().unwrap());
        let resp = h
            .handle(post_req_bearer(
                "/api/v1/media/image",
                serde_json::json!({"prompt": "a cat"}),
                &key,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 503);
        assert_eq!(gw_quota_used(&gw, &key), 0, "显存 503 不应扣费");
        assert!(h.recent_snapshot().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn media_auth_challenge_verify_then_generate_e2e() {
        let _guard = ENV_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let _restore = EnvRestore::new(&["NEXOS_SMI_BIN", "NEXOS_IMGGEN_BIN"]);
        let dir = temp_dir_for("auth-e2e");
        let smi = fake_exec(&dir, "fake-smi.sh", "#!/bin/sh\necho 24000\n");
        let imggen = fake_exec(
            &dir,
            "fake-imggen.sh",
            "#!/bin/sh\nprintf 'png' > \"$NEXOS_IMGGEN_OUT\"\n",
        );
        std::env::set_var("NEXOS_SMI_BIN", smi.to_str().unwrap());
        std::env::set_var("NEXOS_IMGGEN_BIN", imggen.to_str().unwrap());
        let h = admin_handler();
        let sk = new_key();
        let pubkey = pubkey_hex(&sk);
        // 1) 非法 pubkey → 400
        let resp = h
            .handle(post_req(
                "/api/v1/media/auth/challenge",
                serde_json::json!({"pubkey": "not-a-pubkey"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // 2) challenge → nonce（带展示名）
        let resp = h
            .handle(post_req(
                "/api/v1/media/auth/challenge",
                serde_json::json!({"pubkey": pubkey}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "body: {resp:?}");
        let nonce = resp.body["nonce"].as_str().unwrap().to_string();
        assert_eq!(resp.body["expires_in"], 60);
        assert!(!resp.body["display_name"].as_str().unwrap().is_empty());
        // 3) 错误签名 → 401（nonce 已烧）
        let evil = new_key();
        let resp = h
            .handle(post_req(
                "/api/v1/media/auth/verify",
                serde_json::json!({
                    "pubkey": pubkey,
                    "nonce": nonce,
                    "signature": sign_nonce(&evil, &nonce),
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 401, "伪造签名应 401");
        // 4) nonce 已被烧 → 再用真签名也 401（防暴力尝试）
        let resp = h
            .handle(post_req(
                "/api/v1/media/auth/verify",
                serde_json::json!({
                    "pubkey": pubkey,
                    "nonce": nonce,
                    "signature": sign_nonce(&sk, &nonce),
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 401, "nonce 单次使用");
        // 5) 重走 challenge → 真签名 → 拿 token
        let nonce2 = h
            .handle(post_req(
                "/api/v1/media/auth/challenge",
                serde_json::json!({"pubkey": pubkey}),
            ))
            .await
            .unwrap()
            .body["nonce"]
            .as_str()
            .unwrap()
            .to_string();
        let token = h
            .handle(post_req(
                "/api/v1/media/auth/verify",
                serde_json::json!({
                    "pubkey": pubkey,
                    "nonce": nonce2,
                    "signature": sign_nonce(&sk, &nonce2),
                }),
            ))
            .await
            .unwrap()
            .body["token"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(!token.is_empty());
        // 6) token 生图 → pubkey 归因
        let resp = h
            .handle(post_req_bearer(
                "/api/v1/media/image",
                serde_json::json!({"prompt": "e2e cat"}),
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "body: {resp:?}");
        assert_eq!(resp.body["generated_by"], pubkey, "token 反查 pubkey 归因");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn billing_matrix_across_all_caller_kinds() {
        // 身份/计费决策矩阵：4 类调用方 × (charged, billing, generated_by)
        let _guard = ENV_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let _restore = EnvRestore::new(&["NEXOS_SMI_BIN", "NEXOS_IMGGEN_BIN"]);
        let dir = temp_dir_for("matrix");
        let smi = fake_exec(&dir, "fake-smi.sh", "#!/bin/sh\necho 24000\n");
        let imggen = fake_exec(
            &dir,
            "fake-imggen.sh",
            "#!/bin/sh\nprintf 'png' > \"$NEXOS_IMGGEN_OUT\"\n",
        );
        std::env::set_var("NEXOS_SMI_BIN", smi.to_str().unwrap());
        std::env::set_var("NEXOS_IMGGEN_BIN", imggen.to_str().unwrap());
        let gw = Arc::new(ApiGatewayRouteHandler::with_empty());
        let (free_key, free_name) = gw_create_token(&gw, "矩阵free", "free", 0).await;
        let (paid_key, paid_name) = gw_create_token(&gw, "矩阵付费", "per_image", 9_999).await;
        let auth = Arc::new(ChainAuth::new());
        let sk = new_key();
        let pubkey = pubkey_hex(&sk);
        let (chain_token, _) = auth.issue_token(&pubkey);
        let h = MediaGenRouteHandler::with_chain_auth(auth)
            .with_admin_token("test-admin")
            .with_gateway(gw.clone());
        let body = serde_json::json!({"prompt": "matrix cat"});
        for (bearer, charged, billing, generated_by) in [
            (
                chain_token.as_str(),
                false,
                serde_json::Value::Null,
                serde_json::json!(pubkey),
            ),
            (
                "test-admin",
                false,
                serde_json::Value::Null,
                serde_json::json!("admin"),
            ),
            (
                free_key.as_str(),
                false,
                serde_json::json!("free"),
                serde_json::json!(free_name),
            ),
            (
                paid_key.as_str(),
                true,
                serde_json::json!("image_credit"),
                serde_json::json!(paid_name),
            ),
        ] {
            let resp = h
                .handle(post_req_bearer("/api/v1/media/image", body.clone(), bearer))
                .await
                .unwrap();
            assert_eq!(resp.status, 200, "{}: {resp:?}", bearer);
            assert_eq!(resp.body["charged"], charged, "charged 矩阵 {bearer}");
            assert_eq!(resp.body["billing"], billing, "billing 矩阵 {bearer}");
            assert_eq!(
                resp.body["generated_by"], generated_by,
                "generated_by 矩阵 {bearer}"
            );
        }
        // 4 次生成全部入 recent；网关侧只有 paid 令牌被扣 100
        assert_eq!(h.recent_snapshot().len(), 4);
        assert_eq!(gw_quota_used(&gw, &paid_key), 100);
        assert_eq!(gw_quota_used(&gw, &free_key), 0);
    }

    #[test]
    fn recent_item_serializes_null_generated_by_for_legacy_compat() {
        // 历史条目（无归因）序列化为 null——前端按可选字段兼容
        let item = ImageRecentItem {
            id: "img-legacy".into(),
            prompt_summary: "old".into(),
            width: 768,
            height: 432,
            steps: 4,
            elapsed_ms: 1,
            created_at: "2026-01-01T00:00:00+08:00".into(),
            generated_by: None,
            generated_by_display: None,
        };
        let v = serde_json::to_value(&item).unwrap();
        assert!(v["generated_by"].is_null(), "无归因 → null: {v}");
        assert!(v["generated_by_display"].is_null());
        // 带归因条目两字段都回显
        let item = ImageRecentItem {
            generated_by: Some("0xabc".into()),
            generated_by_display: Some("0xdef".into()),
            ..item
        };
        let v = serde_json::to_value(&item).unwrap();
        assert_eq!(v["generated_by"], "0xabc");
        assert_eq!(v["generated_by_display"], "0xdef");
    }

    // ---- 视频任务框架 ----

    #[cfg(unix)]
    #[tokio::test]
    async fn video_task_unconfigured_external_fails_immediately_with_reason() {
        let _guard = ENV_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let _restore = EnvRestore::new(&["NEXOS_VIDEO_API_URL", "NEXOS_VIDEO_API_KEY"]);
        std::env::remove_var("NEXOS_VIDEO_API_URL");
        let h = MediaGenRouteHandler::new();
        let resp = h
            .handle(post_req(
                "/api/v1/media/video",
                serde_json::json!({"prompt": "a sunset timelapse"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 202, "任务已创建（尝试后失败）: {resp:?}");
        assert_eq!(resp.body["status"], "failed", "无后端应立即 failed");
        assert_eq!(resp.body["backend"], "external", "默认 external");
        assert_eq!(resp.body["duration_secs"], 5, "duration 默认 5");
        assert!(resp.body["video_url"].is_null(), "无 video_url");
        let err = resp.body["error"].as_str().unwrap();
        assert!(
            err.contains("未配置外部视频后端 env NEXOS_VIDEO_API_URL"),
            "缺未配置原因: {err}"
        );
        assert!(err.contains("指引"), "应附指引: {err}");
        // 任务已入列表，详情可查
        let id = resp.body["id"].as_str().unwrap().to_string();
        let resp = h
            .handle(get_req(&format!("/api/v1/media/video/tasks/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["status"], "failed");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn video_task_local_backend_reports_not_ready() {
        let _guard = ENV_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let _restore = EnvRestore::new(&["NEXOS_VIDEO_API_URL"]);
        std::env::remove_var("NEXOS_VIDEO_API_URL");
        let h = MediaGenRouteHandler::new();
        let resp = h
            .handle(post_req(
                "/api/v1/media/video",
                serde_json::json!({"prompt": "waves", "backend": "local", "duration_secs": 10}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 202);
        assert_eq!(resp.body["backend"], "local");
        assert_eq!(resp.body["duration_secs"], 10);
        let err = resp.body["error"].as_str().unwrap();
        assert!(err.contains("本地视频模型未就绪"), "缺未就绪原因: {err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn video_task_external_configured_still_honest_failure() {
        let _guard = ENV_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let _restore = EnvRestore::new(&["NEXOS_VIDEO_API_URL", "NEXOS_VIDEO_API_KEY"]);
        // 已配置 URL：客户端尚未接入 → 仍诚实失败（无成功路径）
        std::env::set_var("NEXOS_VIDEO_API_URL", "https://video.example.test/api");
        let h = MediaGenRouteHandler::new();
        let resp = h
            .handle(post_req(
                "/api/v1/media/video",
                serde_json::json!({"prompt": "city night"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 202);
        assert_eq!(resp.body["status"], "failed");
        let err = resp.body["error"].as_str().unwrap();
        assert!(err.contains("尚未接入"), "已配置但未接入应说明: {err}");
    }

    #[tokio::test]
    async fn video_validates_duration_and_backend() {
        let h = MediaGenRouteHandler::new();
        for body in [
            serde_json::json!({"prompt": "x", "duration_secs": 0}),
            serde_json::json!({"prompt": "x", "duration_secs": 31}),
            serde_json::json!({"prompt": "x", "backend": "bogus"}),
            serde_json::json!({"prompt": "", "duration_secs": 5}),
        ] {
            let resp = h
                .handle(post_req("/api/v1/media/video", body))
                .await
                .unwrap();
            assert_eq!(resp.status, 400, "非法参数应 400: {resp:?}");
        }
        assert!(h.video_tasks_snapshot().is_empty(), "非法请求不建任务");
    }

    #[tokio::test]
    async fn video_task_list_and_detail_404() {
        let h = MediaGenRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/media/video/tasks"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body.as_array().unwrap().len(), 0, "初始空列表");
        // 详情不存在 → 404
        let resp = h
            .handle(get_req("/api/v1/media/video/tasks/vid-nope"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
        assert!(
            resp.body["error"].as_str().unwrap().contains("不存在"),
            "应说明任务不存在"
        );
    }

    #[tokio::test]
    async fn backend_for_selects_by_name() {
        assert_eq!(backend_for("local").name(), "local");
        assert_eq!(backend_for("external").name(), "external");
        // 未知名兜底 external（请求校验期已拦 400，此处只测选择纯函数）
        assert_eq!(backend_for("whatever").name(), "external");
    }

    #[tokio::test]
    async fn backend_submit_errors_are_distinct() {
        let task = VideoTask {
            id: "vid-t".into(),
            prompt: "p".into(),
            duration_secs: 5,
            backend: "local".into(),
            status: "queued".into(),
            video_url: None,
            error: None,
            created_at: now_iso(),
        };
        let local_err = LocalVideoBackend
            .submit(&task)
            .await
            .expect_err("local 应失败");
        assert_eq!(local_err, "本地视频模型未就绪");
    }

    #[test]
    fn summarize_stderr_trims_and_truncates_tail() {
        assert_eq!(summarize_stderr("  boom  \n"), "boom");
        let long = "x".repeat(500);
        let s = summarize_stderr(&long);
        assert_eq!(s.chars().count(), 401, "尾部 400 字 + 省略号");
        assert!(s.starts_with('…'), "应带省略号前缀");
    }

    #[test]
    fn build_nvidia_smi_vram_cmd_shape() {
        let cmd = build_nvidia_smi_vram_cmd();
        assert_eq!(cmd[0], "--query-gpu=memory.free");
        assert_eq!(cmd[1], "--format=csv,noheader,nounits");
    }

    #[tokio::test]
    async fn unmatched_route_returns_404() {
        let h = MediaGenRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/media/nope")).await.unwrap();
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<MediaGenRouteHandler>();
    }
}
