//! `ModelHubRouteHandler` —— 模型仓库管理 + 模型大厅（多源共享/下载）桌面应用后端。
//!
//! 定位：把网关 HTTP 请求（`/api/v1/models/*`）翻译为本地模型库管理 + modelscope
//! 在线一键下载 + **模型大厅（发布/浏览/多源合并/跨机多源下载）**，返回 JSON。
//! 这是 OS"模型仓库/模型大厅"桌面应用（与 vLLM 实例管理并列但独立：本组件管
//! 模型**文件**，llm.rs 管 vLLM **推理实例**）的后端 REST 入口。
//! 全量设计（拓扑/数据流/调度算法）见 `docs/MODELHUB_LOBBY.md`。
//!
//! # 实现策略
//!
//! - **本地模型库**：真实扫描 `/tank/models/`（spawn_blocking），每个子目录算一个
//!   模型（含 `config.json` 的为完整模型），递归 du 算大小/文件数/最后修改时间。
//!   `/tank/models` 不存在时回退 `/var/lib/os/models`；`NEXOS_MODELS_DIR`/
//!   `OS_MODELS_DIR` 可覆盖（测试隔离/自定义库位）。
//! - **HF hub 缓存自动扫描（2026-09-03）**：`huggingface-cli`/`hf download` 把模型
//!   装进 `~/.cache/huggingface/hub/models--<org>--<name>/snapshots/<hash>/`——
//!   服务进程用户 ≠ 安装用户（如服务跑 root、模型装 /home/nvidia）时本地库扫描
//!   天生看不见。本地清单因此并入 HF 缓存扫描（候选链：`NEXOS_MODELHUB_HF_CACHE`
//!   （设置即替换全链）→ `HF_HUB_CACHE`/`HF_HOME/hub` → `/root/.cache/...` +
//!   `/home/*/.cache/...` glob 全用户），snapshot 取 `refs/main` 指向的 commit。
//!   条目 `id=org--name`、`display_name=org/name`、`source=hf_cache`、`path`=
//!   snapshot 真实目录（vLLM `--model <path>` 直接可吃）。大小/分片明细/detail
//!   端点对 HF 条目同样成立（同一套权重档案解析）；**删除被拒**（HF 缓存是
//!   huggingface 工具链私有布局，rm snapshot 留孤儿 blobs——指引 hf CLI）。
//!   与自家目录同名共存不去重（来源徽章区分）；手动添加走既有
//!   `POST /models/import`（任意本地路径 → 校验 → 符号链接入库）。
//! - **权重详细管理（A 面）**：`GET /models/:name/detail` 递归列全文件清单 +
//!   safetensors 分片序号解析（`*-0000X-of-0000Y.safetensors`）+ config.json
//!   架构解析 + 分片序列完整性判定；`DELETE /models/:name` rm -rf 前置安全校验
//!   （必须根目录直系、拒 `..`/嵌套/符号链接目标逃逸——导入的符号链接只解除
//!   链接不删目标）；`POST /models/import` 把库外模型目录符号链接进库（不复制）。
//! - **一键下载**：spawn `modelscope download --model <id> --local_dir <...>`
//!   （fire-and-forget，拿 pid）。modelscope 不存在/启动失败 → `status=failed`。
//! - **模型大厅（B 面）**：SQLite `model_lobby` 表（`model_lobby.db`，照 im.rs
//!   建库惯例）持久化发布条目；同 name 多发布者在大厅列表**合并为一条**，
//!   `sources` 数组聚合各发布者的 `source_url`（多人分享即多源）。文件共享端点
//!   `GET /models/share/:name/*` 是多源下载的 HTTP 传输面（token=admin token，
//!   路径白名单防穿越，offset/length 分段回传 base64 内容）。
//! - **多源下载（C 面）**：`POST /models/downloads` 携带 `sources` 数组即创建
//!   `lobby_multi` 任务——从首个可达源拉文件清单，文件级轮转分配到各源并行下载
//!   （.part 临时名 + 完成原子 rename + 断点续传 + 失败换源重试 + 终态 size 校验）。
//! - **Spark 专区（E 面）**：`GET /models/spark-zone` 静态策展表（真实 NVFP4 仓库，
//!   实测收录）+ 逐条两源（魔搭/HF 镜像）实时可用性探测（并行、单源 3s 超时、
//!   失败标 unavailable 不剔除）；env `NEXOS_SPARK_ZONE_FILE` 可覆盖策展表。
//!   下载复用 D 面在线源机制，无新下载器。**专区语义**：NVFP4 对 SM120 架构
//!   （DGX Spark / RTX 50 系等）有硬件级优化，但模型本身通用——专区只是策展入口。
//! - **进度估算**：GET /downloads/:id 时重扫 local_dir 算 current_size；pid 退出 +
//!   目录有 config.json → completed；pid 退出 + 无 config → failed。
//! - **推荐模型**：预置 Qwen3-VL/Qwen2.5 系列常用模型，扫描时标 downloaded。
//!
//! # 降级语义（modelscope 未安装也不 panic）
//!
//! modelscope CLI 可能未安装或路径不在 PATH —— spawn 失败 / 进程立刻退出都降级为
//! 友好的 `failed` 状态，绝不 panic。命令构造为纯函数（可单测，不真跑）。
//! 多源下载任一源不可达只影响该源上的文件（换下一个源重试），全部源清单拉取
//! 失败才整体 502 失败。
//!
//! # 路由表（20 条，component="model_hub"）
//!
//! | method | path                                | 动作 |
//! |--------|-------------------------------------|------|
//! | GET    | `/api/v1/models/local`              | 列本地模型（自家库 + HF 缓存合并，见上文 HF 扫描段）|
//! | GET    | `/api/v1/models/local/:id`          | 单模型详情（文件列表 + config.json）|
//! | DELETE | `/api/v1/models/local/:id`          | 删模型（需 admin；走同一安全校验）|
//! | GET    | `/api/v1/models/:name/detail`       | **权重详细管理**：全文件清单 + 分片解析 + 架构 + 完整性 |
//! | DELETE | `/api/v1/models/:name`              | 删模型（admin；安全校验矩阵，符号链接只解链）|
//! | POST   | `/api/v1/models/import`             | 导入库外模型目录为符号链接（admin）|
//! | GET    | `/api/v1/models/downloads`          | 列下载任务（modelscope + lobby_multi + remote_repo 混排）|
//! | POST   | `/api/v1/models/downloads`          | 创建下载任务（admin；`sources`→多源，`model_id`→modelscope）|
//! | DELETE | `/api/v1/models/downloads/:id`      | 取消下载（admin）|
//! | GET    | `/api/v1/models/downloads/:id`      | 下载任务详情（实时刷新进度）|
//! | GET    | `/api/v1/models/recommended`        | 推荐模型列表 |
//! | GET    | `/api/v1/models/stats`              | 聚合统计 |
//! | POST   | `/api/v1/models/lobby/publish`      | **大厅**：发布本地模型（admin）|
//! | GET    | `/api/v1/models/lobby`              | 大厅列表（`?name=` 精确 / `?q=` 搜索；同 name 合并多源）|
//! | GET    | `/api/v1/models/lobby/:name`        | 大厅单模型详情（聚合全部 sources）|
//! | DELETE | `/api/v1/models/lobby/:id`（段值=id）| 下架（admin 或同 sharer）|
//! | GET    | `/api/v1/models/share/:name/*`      | 文件共享端点（`?token=` 校验；多源下载传输面）|
//! | GET    | `/api/v1/models/remote/:kind/:org/:model` | **在线仓库探测**（kind=modelscope/hf；存在性+文件清单+默认勾选）|
//! | POST   | `/api/v1/models/remote/downloads`   | 创建在线仓库下载任务（admin；文件级勾选 + Range 续传 + 文件级并行，env `NEXOS_MODELHUB_DL_CONCURRENCY` 并发数缺省 3 上限 8）|
//! | GET    | `/api/v1/models/spark-zone`         | **Spark 专区**（E 面，公开；SM120/NVFP4 策展清单 + 逐条两源实时可用性，`?probe=0` 跳过探测；env `NEXOS_SPARK_ZONE_FILE` 覆盖策展表）|

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 本地已下载模型（`GET /api/v1/models/local` 元素）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalModel {
    /// 模型 id（目录名），如 "Qwen3-VL-8B-Instruct"。
    pub id: String,
    /// 完整路径 `/tank/models/Qwen3-VL-8B-Instruct`。
    pub path: String,
    /// 总大小（递归 du，字节）。
    pub size_bytes: u64,
    /// 文件数。
    pub file_count: u32,
    /// 最后修改时间（ISO 8601 字符串；不可用时为空）。
    pub modified_at: String,
    /// 是否含 `config.json`（是完整模型）。
    pub has_config: bool,
    /// 模型来源徽章：`local`（模型库目录/导入符号链接，缺省）| `hf_cache`
    /// （HuggingFace hub 缓存 snapshot，2026-09-03 起自动扫描并入清单）。
    #[serde(default = "default_model_source")]
    pub source: String,
    /// 显示名：HF 缓存条目为 `org/name`（如 `nvidia/Qwen3.6-27B-NVFP4`）；
    /// 本地条目与 id 相同。空串时前端回退用 id。
    #[serde(default)]
    pub display_name: String,
}

/// `LocalModel::source` 的 serde 缺省值（`local`——旧响应/测试构造无此字段时）。
fn default_model_source() -> String {
    "local".to_string()
}

/// 下载任务（POST 创建 / GET 列表 / GET 详情）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadTask {
    /// 任务 id。
    pub id: String,
    /// modelscope model id，如 "Qwen/Qwen3-VL-8B-Instruct"。
    pub model_id: String,
    /// 本地目录 `/tank/models/<name>`。
    pub local_dir: String,
    /// `pending` / `downloading` / `completed` / `failed`。
    pub status: String,
    /// 进度 0-100（通过目录大小/预估大小估算）。
    pub progress_pct: u8,
    /// 当前已下载大小（字节）。
    pub current_size_bytes: u64,
    /// 预估总大小（字节，0=未知）。
    pub estimated_size_bytes: u64,
    /// modelscope 进程 pid（运行中）。
    pub pid: Option<u32>,
    pub error: Option<String>,
    pub created_at: String,
}

/// 推荐模型（GET /api/v1/models/recommended 元素）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecommendedModel {
    /// "Qwen/Qwen3-VL-8B-Instruct"。
    pub model_id: String,
    /// 显示名。
    pub name: String,
    /// 预估大小（GB）。
    pub size_gb: f32,
    /// 简介。
    pub description: String,
    /// 标签，如 ["视觉","8B","最新"]。
    pub tags: Vec<String>,
    /// `vl` / `llm` / `embedding`。
    pub category: String,
    /// 本地是否已有。
    pub downloaded: bool,
}

/// 单模型详情（GET /api/v1/models/local/:id 响应）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalModelDetail {
    #[serde(flatten)]
    pub model: LocalModel,
    /// 文件列表（顶层）。
    pub files: Vec<ModelFile>,
    /// config.json 内容（不存在为 null）。
    pub config_json: Option<serde_json::Value>,
}

/// 模型目录下的一条文件条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelFile {
    pub name: String,
    pub size_bytes: u64,
    pub modified_at: String,
}

/// `GET /api/v1/models/stats` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelHubStats {
    pub local_total: usize,
    pub total_size_bytes: u64,
    pub downloads_active: usize,
    pub downloads_completed: usize,
}

/// 创建下载任务请求体（两种任务合一：`sources` → 多源 lobby_multi；否则 modelscope）。
#[derive(Debug, Deserialize)]
struct CreateDownloadBody {
    model_id: Option<String>,
    /// 多源任务：模型名（本地目录名，必填 when sources 非空）。
    name: Option<String>,
    /// 多源任务：来源基地址列表（大厅 sources 原样传入）。
    sources: Option<Vec<String>>,
}

// ----------------------------------------------------------------------------
// A 面 DTO：权重文件详细管理
// ----------------------------------------------------------------------------

/// config.json 解析出的架构信息（`GET /models/:name/detail` 的 config 字段）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfigInfo {
    /// `model_type`，如 "qwen2vl"。
    pub arch: String,
    /// `num_hidden_layers`。
    pub num_hidden_layers: Option<u64>,
    /// `hidden_size`。
    pub hidden_size: Option<u64>,
    /// `vocab_size`。
    pub vocab_size: Option<u64>,
    /// `max_position_embeddings`。
    pub max_position_embeddings: Option<u64>,
    /// 原始 config.json（前端需要其余字段时直读）。
    pub raw: serde_json::Value,
}

/// safetensors 分片解析结果（`*-0000X-of-0000Y.safetensors`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardRef {
    pub index: u32,
    pub total: u32,
}

/// 分片序列完整性判定（纯函数 [`analyze_shards`] 的产物）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardAnalysis {
    /// 是否检测到分片文件（`*-0000X-of-0000Y.safetensors`）。
    pub sharded: bool,
    /// 分片声明的总数（Y）。
    pub shard_total: u32,
    /// 实际在场的分片文件名列表（含序号）。
    pub shard_files: Vec<String>,
    /// 序列 1..=total 是否全部在场（无缺号）。
    pub sequence_complete: bool,
    /// 缺失的序号列表（sequence_complete=true 时空）。
    pub missing_shards: Vec<u32>,
    /// `model.safetensors.index.json` 是否存在。
    pub index_file_present: bool,
}

/// 权重文件清单条目（detail 专用，含分片序号解析）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelFileEx {
    /// 相对模型目录的路径（递归，`/` 分隔）。
    pub name: String,
    pub size_bytes: u64,
    pub modified_at: String,
    /// 分片序号（非分片文件为 null）。
    pub shard_index: Option<u32>,
    /// 分片总数（非分片文件为 null）。
    pub shard_total: Option<u32>,
}

/// `GET /api/v1/models/:name/detail` 响应——权重文件详细管理主 DTO。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelWeightDetail {
    /// 模型名（目录名）。
    pub name: String,
    /// 绝对路径。
    pub path: String,
    /// 总大小（递归，字节）。
    pub total_size_bytes: u64,
    /// 文件总数（递归）。
    pub file_count: usize,
    /// 是否完整（分片模型=序列连续+index.json 在场；单文件模型=有权重+config）。
    pub complete: bool,
    /// 分片完整性细节。
    pub shards: ShardAnalysis,
    /// config.json 解析（不存在为 null）。
    pub config: Option<ModelConfigInfo>,
    /// 全文件清单（递归、按路径排序）。
    pub files: Vec<ModelFileEx>,
}

/// 导入结果（`POST /api/v1/models/import` 响应）。
#[derive(Debug, Clone, Serialize)]
pub struct ImportOutcome {
    pub name: String,
    /// 新建的符号链接路径（库内）。
    pub link_path: String,
    /// 符号链接指向的库外真实目录。
    pub target_path: String,
}

// ----------------------------------------------------------------------------
// B 面 DTO：模型大厅
// ----------------------------------------------------------------------------

/// 大厅单个来源（一位发布者的分享）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LobbySource {
    /// 发布者名。
    pub sharer: String,
    /// 完整 REST 下载基地址（含 token）。
    pub source_url: String,
    pub size_bytes: u64,
    pub file_count: u32,
    pub created_at: String,
}

/// 大厅合并条目（同 name 多发布者聚合为一条，`GET /models/lobby` 元素）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LobbyMergedEntry {
    /// 模型名（多源合并键）。
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub tags: Vec<String>,
    pub arch: String,
    /// 各来源中最大的 size（同一模型不同快照，取最大者展示）。
    pub size_bytes: u64,
    pub file_count: u32,
    /// 全部来源 download_count 之和。
    pub download_count: u64,
    /// 聚合的来源列表（多人分享即多源）。
    pub sources: Vec<LobbySource>,
    /// 最早发布时间。
    pub created_at: String,
}

/// 发布请求体（`POST /api/v1/models/lobby/publish`）。
#[derive(Debug, Deserialize)]
struct PublishBody {
    name: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    sharer: Option<String>,
}

/// 文件共享端点响应（`GET /models/share/:name/*`，base64 分段信封）。
#[derive(Debug, Clone, Serialize)]
pub struct ShareFileResponse {
    pub ok: bool,
    pub name: String,
    /// 请求的相对路径。
    pub path: String,
    pub offset: u64,
    /// 本次回传字节数（content_base64 解码后长度）。
    pub length: u64,
    pub total_size: u64,
    /// offset+length 是否已达文件尾。
    pub eof: bool,
    pub content_base64: String,
}

// ----------------------------------------------------------------------------
// C 面 DTO：多源下载任务（lobby_multi）
// ----------------------------------------------------------------------------

/// 多源任务的清单文件（从首个可达源的 `/models/:name/detail` 拉取）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestFile {
    /// 相对路径。
    pub name: String,
    pub size_bytes: u64,
}

/// 单文件进度简报（任务状态里保留最近 5 条）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileProgress {
    pub file: String,
    /// 实际使用的源基地址（失败换源后为换到的源）。
    pub source: String,
    pub bytes: u64,
    /// `done` / `failed`。
    pub status: String,
    pub error: Option<String>,
}

/// `lobby_multi` 多源下载任务（`POST /models/downloads` 带 sources 创建）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LobbyMultiTask {
    pub id: String,
    /// 任务类型标识（前端区分 modelscope 任务）。
    pub r#type: String,
    pub name: String,
    pub local_dir: String,
    /// 来源基地址列表（大厅 sources 原样）。
    pub sources: Vec<String>,
    /// `downloading` / `completed` / `failed`。
    pub status: String,
    pub files_total: usize,
    pub files_done: usize,
    pub bytes_done: u64,
    /// 清单总大小（各文件 size 之和）。
    pub total_bytes: u64,
    /// 当前仍在供数的源（正在并行工作的源基地址）。
    pub active_sources: Vec<String>,
    /// 最近 5 条文件级简报。
    pub recent_files: Vec<FileProgress>,
    /// 取消标记（DELETE /downloads/:id 置 true，runner 分段间检查）。
    pub cancel_requested: bool,
    pub error: Option<String>,
    pub created_at: String,
}

// ----------------------------------------------------------------------------
// 纯函数（命令构造器，可单测，不执行）
// ----------------------------------------------------------------------------

/// 构造 modelscope download 命令参数（不含程序名，caller 拼 `Command::new(<bin>)`）。
///
/// 形如：`download --model <model_id> --local_dir <local_dir>`
#[must_use]
pub fn build_download_cmd(model_id: &str, local_dir: &str) -> Vec<String> {
    vec![
        "download".into(),
        "--model".into(),
        model_id.into(),
        "--local_dir".into(),
        local_dir.into(),
    ]
}

/// 从 modelscope model id 提取本地目录名。
///
/// `Qwen/Qwen3-VL-8B-Instruct` → `Qwen3-VL-8B-Instruct`（取最后一段）。
/// 无 `/` 时原样返回。
#[must_use]
pub fn model_dir_name(model_id: &str) -> String {
    model_id
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(model_id)
        .to_string()
}

/// 模型根目录（`/tank/models` 存在则用，否则回退 `/var/lib/os/models`）。
///
/// env 覆盖：`NEXOS_MODELS_DIR` / `OS_MODELS_DIR`（trim 后非空才生效）——
/// 测试隔离与自定义模型库位（与 `os_nexhub::repos_dir` 同款惯例）。
#[must_use]
pub fn models_root() -> String {
    if let Ok(dir) = std::env::var("NEXOS_MODELS_DIR").or_else(|_| std::env::var("OS_MODELS_DIR")) {
        let t = dir.trim().to_string();
        if !t.is_empty() {
            return t;
        }
    }
    if std::path::Path::new("/tank/models").is_dir() {
        "/tank/models".to_string()
    } else if std::path::Path::new("/tank").is_dir() {
        // /tank 存在但 /tank/models 不存在 → 用 /tank/models（创建由下载流程负责）
        "/tank/models".to_string()
    } else {
        "/var/lib/os/models".to_string()
    }
}

// ----------------------------------------------------------------------------
// HF hub 缓存扫描（2026-09-03）：服务进程用户 ≠ 安装用户是常态
// ----------------------------------------------------------------------------

/// HF hub 缓存扫描根的显式覆盖 env（**设置时整体替换默认候选链**——
/// 测试确定性隔离 + 特殊布局机器的逃生口；见 [`hf_cache_candidate_roots`]）。
const HF_CACHE_ENV: &str = "NEXOS_MODELHUB_HF_CACHE";

/// HF hub 缓存多用户 glob 的家目录基座（`<base>/*/.cache/huggingface/hub`）。
const HF_HOME_BASE: &str = "/home";

/// 解析 HF hub 缓存仓目录名：`models--<org>--<name>` → `Some((org, name))`。
///
/// 例：`models--nvidia--Qwen3.6-27B-NVFP4` → `("nvidia", "Qwen3.6-27B-NVFP4")`。
/// 非 `models--` 前缀 / org 或 name 为空 / 顶多个 `--` 段视为不合法（HF 仓
/// org/name 各自不含 `--`，多段无法归属，诚实拒绝而不是猜）。
#[must_use]
pub fn parse_hf_repo_dir(dir_name: &str) -> Option<(String, String)> {
    let rest = dir_name.strip_prefix("models--")?;
    let (org, name) = rest.split_once("--")?;
    if org.is_empty() || name.is_empty() || name.contains("--") {
        return None;
    }
    Some((org.to_string(), name.to_string()))
}

/// HF 缓存列表条目 id：`org/name` 仓 → `org--name`（不含 `models--` 前缀）。
///
/// id 用于 URL 路径段（`/models/:name/detail` 等），不能带 `/`；`org--name`
/// 可逆且不与自家目录命名冲突（ HF 仓不会有 `--`）。
#[must_use]
pub fn hf_cache_entry_id(org: &str, name: &str) -> String {
    format!("{org}--{name}")
}

/// HF hub 缓存扫描根候选链（按序，全部扫描、canonical 去重）。
///
/// 1. `NEXOS_MODELHUB_HF_CACHE`（显式指定——**设置即替换全链**，测试隔离用）；
/// 2. `HF_HUB_CACHE`（HF 官方 env，指向 hub 目录本身）；
/// 3. `HF_HOME/hub`（HF 官方约定的另一形态）；
/// 4. `/root/.cache/huggingface/hub`（服务常跑 root）；
/// 5. `/home/*/.cache/huggingface/hub`（**glob 全用户**——模型装在桌面用户
///    home、服务进程跑 root 的错位是常态，全用户扫描是诚实解）。
pub fn hf_cache_candidate_roots() -> Vec<String> {
    fn non_empty_env(key: &str) -> Option<String> {
        std::env::var(key)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }
    if let Some(p) = non_empty_env(HF_CACHE_ENV) {
        return vec![p];
    }
    let mut out: Vec<String> = Vec::new();
    if let Some(p) = non_empty_env("HF_HUB_CACHE") {
        out.push(p);
    }
    if let Some(home) = non_empty_env("HF_HOME") {
        out.push(format!("{home}/hub"));
    }
    out.push("/root/.cache/huggingface/hub".to_string());
    out.extend(glob_user_hf_caches(HF_HOME_BASE));
    out
}

/// glob `<user_homes>/*/.cache/huggingface/hub`（按用户名排序，只收目录）。
///
/// `user_homes` 参数化（生产 `/home`）——单测注入 tempdir 造假多用户布局，
/// 不碰真机 `/home`。
fn glob_user_hf_caches(user_homes: &str) -> Vec<String> {
    let Ok(read) = std::fs::read_dir(user_homes) else {
        return Vec::new();
    };
    let mut users: Vec<String> = read
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    users.sort();
    users
        .into_iter()
        .map(|u| format!("{user_homes}/{u}/.cache/huggingface/hub"))
        .filter(|p| std::path::Path::new(p).is_dir())
        .collect()
}

/// 取仓目录（`models--org--name/`）的**当前** snapshot 目录。
///
/// 优先 `refs/main` 指向的 commit hash（HF 官方语义——refs 文件存当前检出，
/// snapshot 目录名即 commit hash）；refs 缺失/指向失效时兜底 mtime 最新的
/// snapshot。候选 snapshot 还须像个模型（`is_valid_model_dir`），不像则试下一个。
fn latest_snapshot(repo_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let snaps = repo_dir.join("snapshots");
    if !snaps.is_dir() {
        return None;
    }
    // 1. refs/main → snapshots/<hash>（确定性最高）
    if let Ok(hash) = std::fs::read_to_string(repo_dir.join("refs/main")) {
        let h = hash.trim();
        if !h.is_empty() {
            let p = snaps.join(h);
            if p.is_dir() && is_valid_model_dir(&p) {
                return Some(p);
            }
        }
    }
    // 2. 兜底：mtime 最新（同刻并列时名字字典序大者胜，保证稳定）
    let mut best: Option<(std::time::SystemTime, String, std::path::PathBuf)> = None;
    let Ok(read) = std::fs::read_dir(&snaps) else {
        return None;
    };
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let p = entry.path();
        if !p.is_dir() || !is_valid_model_dir(&p) {
            continue;
        }
        let mtime = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let better = match &best {
            None => true,
            Some((bt, bn, _)) => {
                mtime > *bt || (mtime == *bt && name > *bn)
            }
        };
        if better {
            best = Some((mtime, name, p));
        }
    }
    best.map(|(_, _, p)| p)
}

/// 扫描单个 HF hub 缓存根（`<...>/huggingface/hub`），产出 `LocalModel` 列表。
///
/// 只认 `models--org--name/snapshots/<hash>/` 布局且 snapshot 里有 config.json
/// 或 `*.safetensors`（复用 [`is_valid_model_dir`]）。条目：
/// - `id` = `org--name`、`display_name` = `org/name`、`source` = `hf_cache`；
/// - `path` = snapshot **真实目录**（symlink 已解析过的 blob 布局对 vLLM 透明，
///   `--model <path>` 可直接建实例）；
/// - 大小/文件数/修改时间复用既有 du 口径（[`dir_size_and_count`] 跟随
///   blob symlink，真实占用）。
fn scan_hf_hub_root(hub_root: &std::path::Path) -> Vec<LocalModel> {
    let Ok(read) = std::fs::read_dir(hub_root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in read.flatten() {
        let dir_name = entry.file_name().to_string_lossy().into_owned();
        let Some((org, name)) = parse_hf_repo_dir(&dir_name) else {
            continue;
        };
        let Some(snap) = latest_snapshot(&entry.path()) else {
            continue;
        };
        let has_config = snap.join("config.json").exists();
        let (size_bytes, file_count) = dir_size_and_count(&snap);
        let modified_at = dir_modified(&snap);
        out.push(LocalModel {
            id: hf_cache_entry_id(&org, &name),
            path: snap.to_string_lossy().into_owned(),
            size_bytes,
            file_count,
            modified_at,
            has_config,
            source: "hf_cache".to_string(),
            display_name: format!("{org}/{name}"),
        });
    }
    out
}

/// 扫描全部 HF hub 缓存候选根（canonical 路径去重——env 候选与 glob 候选
/// 可能指向同一目录）。
fn scan_hf_cache_models_blocking() -> Vec<LocalModel> {
    let mut seen: Vec<std::path::PathBuf> = Vec::new();
    let mut out = Vec::new();
    for root in hf_cache_candidate_roots() {
        for m in scan_hf_hub_root(std::path::Path::new(&root)) {
            let key = std::path::Path::new(&m.path)
                .canonicalize()
                .unwrap_or_else(|_| std::path::PathBuf::from(&m.path));
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            out.push(m);
        }
    }
    out
}

/// 本地模型清单 = 自家模型库扫描 + HF hub 缓存扫描（合并展示，来源徽章区分；
/// 与自家目录**同名共存不去重**——两份都是真实可见的权重，删哪个由用户定）。
/// 排序：大小降序（与纯库扫描同口径）。
fn scan_all_local_models_blocking(root: &str) -> Vec<LocalModel> {
    let mut out = scan_local_models_blocking(root);
    out.extend(scan_hf_cache_models_blocking());
    out.sort_by_key(|m| std::cmp::Reverse(m.size_bytes));
    out
}

/// 按 id（`org--name`）反查 HF 缓存 snapshot 真实目录（detail/删除守卫用）。
///
/// 候选根链与列表扫描同源；任一命中即返回。
fn resolve_hf_snapshot_by_id(name: &str) -> Option<std::path::PathBuf> {
    for root in hf_cache_candidate_roots() {
        let repo_dir = std::path::Path::new(&root).join(format!("models--{name}"));
        if let Some(snap) = latest_snapshot(&repo_dir) {
            return Some(snap);
        }
    }
    None
}

/// HF 缓存条目的显示名：`org--name` id → `org/name`（无 `--` 时原样）。
#[must_use]
pub fn hf_display_from_id(id: &str) -> String {
    match id.split_once("--") {
        Some((org, name)) => format!("{org}/{name}"),
        None => id.to_string(),
    }
}

/// 返回预置推荐模型列表（GET /api/v1/models/recommended）。
#[must_use]
pub fn recommended_models() -> Vec<RecommendedModel> {
    vec![
        RecommendedModel {
            model_id: "Qwen/Qwen3-VL-8B-Instruct".into(),
            name: "千问3-VL-8B".into(),
            size_gb: 17.5,
            description: "最强视觉语言模型，支持 GUI 操作/OCR/空间感知".into(),
            tags: vec!["视觉".into(), "8B".into(), "最新".into()],
            category: "vl".into(),
            downloaded: false,
        },
        RecommendedModel {
            model_id: "Qwen/Qwen2.5-VL-7B-Instruct".into(),
            name: "千问2.5-VL-7B".into(),
            size_gb: 15.0,
            description: "视觉语言模型".into(),
            tags: vec!["视觉".into(), "7B".into()],
            category: "vl".into(),
            downloaded: false,
        },
        RecommendedModel {
            model_id: "Qwen/Qwen2.5-7B-Instruct".into(),
            name: "千问2.5-7B".into(),
            size_gb: 15.0,
            description: "通用对话模型".into(),
            tags: vec!["纯文本".into(), "7B".into()],
            category: "llm".into(),
            downloaded: false,
        },
        RecommendedModel {
            model_id: "Qwen/Qwen2.5-Coder-7B-Instruct".into(),
            name: "千问2.5-Coder".into(),
            size_gb: 15.0,
            description: "代码生成".into(),
            tags: vec!["代码".into(), "7B".into()],
            category: "llm".into(),
            downloaded: false,
        },
        RecommendedModel {
            model_id: "Qwen/Qwen3-8B-Instruct".into(),
            name: "千问3-8B".into(),
            size_gb: 16.0,
            description: "最新通用模型".into(),
            tags: vec!["纯文本".into(), "8B".into(), "最新".into()],
            category: "llm".into(),
            downloaded: false,
        },
    ]
}

// ----------------------------------------------------------------------------
// A 面纯函数：分片解析 / 完整性判定 / 名字校验 / 删除安全 / 导入
// ----------------------------------------------------------------------------

/// 从文件名解析 safetensors 分片序号（HF 命名 `*-0000X-of-0000Y.safetensors`）。
///
/// 例：`model-00003-of-00005.safetensors` → `ShardRef { index: 3, total: 5 }`。
/// 非分片文件（`model.safetensors` / 其他扩展名 / 序号非 5 位）返回 None。
#[must_use]
pub fn parse_shard_filename(name: &str) -> Option<ShardRef> {
    let stem = name.strip_suffix(".safetensors")?;
    // 形如 `<prefix>-NNNNN-of-NNNNN`：从右找最后一个 "-of-"
    let (left, right) = stem.rsplit_once("-of-")?;
    if right.len() != 5 || !right.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if left.len() < 6 {
        return None; // 至少 "-NNNNN"
    }
    let idx = &left[left.len() - 5..];
    let dash = &left[left.len() - 6..left.len() - 5];
    if dash != "-" || !idx.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let index = idx.parse().ok()?;
    let total = right.parse().ok()?;
    if index == 0 || total == 0 {
        return None;
    }
    Some(ShardRef { index, total })
}

/// 分片序列完整性判定（纯函数）。
///
/// - `file_names`：模型目录内全部文件名；`has_index_file`：`model.safetensors.index.json`
///   是否在场。
/// - 检出分片时以**声明的最大 total** 为准（多个 total 不一致取最大并判缺失）；
///   `sequence_complete` = 1..=total 全在场。
#[must_use]
pub fn analyze_shards(file_names: &[&str], has_index_file: bool) -> ShardAnalysis {
    let mut shard_total = 0u32;
    let mut present: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
    let mut shard_files = Vec::new();
    for f in file_names {
        if let Some(r) = parse_shard_filename(f) {
            shard_total = shard_total.max(r.total);
            present.insert(r.index);
            shard_files.push((*f).to_string());
        }
    }
    shard_files.sort();
    let sharded = !present.is_empty();
    let missing_shards: Vec<u32> = if sharded {
        (1..=shard_total).filter(|i| !present.contains(i)).collect()
    } else {
        Vec::new()
    };
    ShardAnalysis {
        sharded,
        shard_total,
        shard_files,
        sequence_complete: sharded && missing_shards.is_empty(),
        missing_shards,
        index_file_present: has_index_file,
    }
}

/// 模型整体完整性：分片模型 = 序列连续 + index.json 在场；单文件模型 =
/// 至少一个 safetensors 权重 + config.json 在场（调用方给 has_config）。
#[must_use]
pub fn judge_complete(shards: &ShardAnalysis, has_config: bool, has_any_weight: bool) -> bool {
    if shards.sharded {
        shards.sequence_complete && shards.index_file_present
    } else {
        has_config && has_any_weight
    }
}

/// 从 config.json JSON 解析架构信息（缺字段为 None，arch 缺省空串）。
#[must_use]
pub fn parse_config_info(v: &serde_json::Value) -> ModelConfigInfo {
    let num = |key: &str| -> Option<u64> {
        v.get(key).and_then(|x| {
            x.as_u64()
                .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
        })
    };
    ModelConfigInfo {
        arch: v
            .get("model_type")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        num_hidden_layers: num("num_hidden_layers"),
        hidden_size: num("hidden_size"),
        vocab_size: num("vocab_size"),
        max_position_embeddings: num("max_position_embeddings"),
        raw: v.clone(),
    }
}

/// 校验模型名（目录名）：非空、无 `..`/`.` 段、无 `/`、无 `\`、无 NUL、不以 `-` 开头。
pub fn validate_model_name(name: &str) -> Result<(), String> {
    let n = name.trim();
    if n.is_empty() {
        return Err("模型名不可为空".into());
    }
    if n == "." || n == ".." || n.contains("..") || n.contains('/') || n.contains('\\') {
        return Err(format!("非法模型名: {n:?}（不得含 .. / \\\\）"));
    }
    if n.contains('\0') {
        return Err("模型名不得含 NUL".into());
    }
    if n.starts_with('-') {
        return Err("模型名不得以 - 开头".into());
    }
    Ok(())
}

/// 删除目标的处置动作（安全校验产物）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteAction {
    /// 真实目录 → rm -rf（已确认 canonical 父目录 == 模型根）。
    RemoveDir(String),
    /// 符号链接 → 只 unlink 链接本身（导入产物；绝不跟进删目标目录）。
    UnlinkSymlink(String),
}

/// rm -rf 前置安全校验（纯函数，真实 FS 探测但不做任何修改）。
///
/// 校验矩阵：
/// 1. 名字合法（`validate_model_name`）；
/// 2. `<root>/<name>` 存在（否则 Err 404 语义）；
/// 3. 是符号链接 → `UnlinkSymlink`（不管目标在哪——只解链，天然不逃逸）；
/// 4. 是真实目录 → canonicalize 后**父目录必须等于 canonicalize(root)**（拒绝
///    嵌套子目录、拒绝符号链接解析后逃逸到根外的目标）；否则 `RemoveDir`。
///
/// 文件（非目录非链接）→ 拒绝。
pub fn validate_delete_target(root: &str, name: &str) -> Result<DeleteAction, String> {
    validate_model_name(name)?;
    let root_path = std::path::Path::new(root);
    let root_canon = root_path
        .canonicalize()
        .map_err(|e| format!("模型根目录不可用 {root}: {e}"))?;
    let target = root_path.join(name);
    let meta = std::fs::symlink_metadata(&target).map_err(|_| format!("本地模型不存在: {name}"))?;
    if meta.file_type().is_symlink() {
        // 只解除链接本身——导入的符号链接目标在库外，删链接不动目标。
        return Ok(DeleteAction::UnlinkSymlink(
            target.to_string_lossy().into_owned(),
        ));
    }
    if !meta.is_dir() {
        return Err(format!("{name} 不是模型目录（拒绝删除）"));
    }
    let canon = target
        .canonicalize()
        .map_err(|e| format!("解析模型目录失败: {e}"))?;
    if canon.parent() != Some(&root_canon) {
        return Err(format!(
            "拒绝删除：{} 不是模型根目录 {} 的直系子目录（防路径逃逸）",
            canon.display(),
            root_canon.display()
        ));
    }
    Ok(DeleteAction::RemoveDir(
        canon.to_string_lossy().into_owned(),
    ))
}

/// 执行删除（先 [`validate_delete_target`] 校验，再按动作 rm -rf / unlink）。
pub fn delete_model_blocking(root: &str, name: &str) -> Result<DeleteAction, String> {
    let action = validate_delete_target(root, name)?;
    match &action {
        DeleteAction::RemoveDir(dir) => std::fs::remove_dir_all(dir)
            .map_err(|e| format!("删除模型目录失败: {e}"))
            .map(|_| action),
        DeleteAction::UnlinkSymlink(link) => std::fs::remove_file(link)
            .map_err(|e| format!("解除模型符号链接失败: {e}"))
            .map(|_| action),
    }
}

/// 目录是否像一个模型（顶层含 config.json 或任一 `*.safetensors`）。
#[must_use]
pub fn is_valid_model_dir(dir: &std::path::Path) -> bool {
    if dir.join("config.json").is_file() {
        return true;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return false;
    };
    read.flatten()
        .any(|e| e.file_name().to_string_lossy().ends_with(".safetensors"))
}

/// 判断 `path` 是否在 `root` 之内（canonicalize 双方后前缀比对）。
fn path_inside_root(root: &std::path::Path, path: &std::path::Path) -> bool {
    let Ok(rc) = root.canonicalize() else {
        return false;
    };
    let Ok(pc) = path.canonicalize() else {
        return false;
    };
    pc == rc || pc.starts_with(&rc)
}

/// 导入校验 + 建符号链接（纯 FS 函数，不修数据库）。
///
/// 规则：
/// 1. 源路径存在且为目录；
/// 2. 源含 config.json 或 `*.safetensors`（顶层）才认；
/// 3. 源必须**在模型根之外**（根内目录无需导入）；
/// 4. 库内重名（链接或目录已占位）→ 冲突拒绝；
/// 5. 通过 → `std::os::unix::fs::symlink`（不复制大文件）。
pub fn import_model_link(root: &str, src: &str) -> Result<ImportOutcome, String> {
    let src_path = std::path::Path::new(src);
    if !src_path.is_dir() {
        return Err(format!("源路径不存在或不是目录: {src}"));
    }
    if !is_valid_model_dir(src_path) {
        return Err(format!(
            "源目录不含 config.json 或 *.safetensors，不认为是模型: {src}"
        ));
    }
    let root_path = std::path::Path::new(root);
    if path_inside_root(root_path, src_path) {
        return Err("源目录已在模型库内，无需导入".into());
    }
    let name = src_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| "无法从源路径提取模型名".to_string())?;
    validate_model_name(&name)?;
    let link_path = root_path.join(&name);
    if link_path.symlink_metadata().is_ok() {
        return Err(format!("模型库内已存在同名条目: {name}"));
    }
    std::fs::create_dir_all(root_path).map_err(|e| format!("创建模型根目录失败: {e}"))?;
    let target_canon = src_path
        .canonicalize()
        .map_err(|e| format!("解析源目录失败: {e}"))?;
    std::os::unix::fs::symlink(&target_canon, &link_path)
        .map_err(|e| format!("创建符号链接失败: {e}"))?;
    Ok(ImportOutcome {
        name,
        link_path: link_path.to_string_lossy().into_owned(),
        target_path: target_canon.to_string_lossy().into_owned(),
    })
}

// ----------------------------------------------------------------------------
// B 面纯函数：地址构造 / 大厅合并 / 路径白名单
// ----------------------------------------------------------------------------

/// 大厅分享 host：`NEXOS_MODELHUB_SHARE_HOST` 覆盖，缺省取 `hostname` 命令
/// （回退 `localhost`；OnceLock 缓存，同 os-nexhub code_repo 的 cached_hostname）。
fn share_host() -> String {
    use std::sync::OnceLock;
    static HOST: OnceLock<String> = OnceLock::new();
    HOST.get_or_init(|| {
        std::env::var("NEXOS_MODELHUB_SHARE_HOST")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                std::process::Command::new("hostname")
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "localhost".to_string())
            })
    })
    .clone()
}

/// 分享端口（`NEXOS_HTTP_PORT` / `OS_HTTP_PORT`，默认 8080——与 code_repo 的
/// http_port 同款，指向 os-api 网关自身监听端口）。
fn share_port() -> String {
    std::env::var("NEXOS_HTTP_PORT")
        .or_else(|_| std::env::var("OS_HTTP_PORT"))
        .unwrap_or_else(|_| "8080".to_string())
}

/// 读系统 admin token env（与 media_gen/nexhub_lobby 同款语义）：
/// `NEXOS_ADMIN_TOKEN` 优先，回落 `OS_ADMIN_TOKEN`；trim 后非空才算启用。
fn admin_token_from_env() -> Option<String> {
    std::env::var("NEXOS_ADMIN_TOKEN")
        .or_else(|_| std::env::var("OS_ADMIN_TOKEN"))
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// 构造 source_url（纯函数）：`http://<host>:<port>/api/v1/models/share/<name>?token=<t>`。
///
/// token 为空则省略 `?token=`（该源无法被匿名拉取——share 端点会 401，发布响应
/// 里 `share_token` 同步为空，前端应提示补配 admin token）。
#[must_use]
pub fn build_source_url(host: &str, port: &str, name: &str, token: &str) -> String {
    let base = format!("http://{host}:{port}/api/v1/models/share/{name}");
    if token.is_empty() {
        base
    } else {
        format!("{base}?token={token}")
    }
}

/// sharer 名净化：仅保留 `[A-Za-z0-9._-]`，其余替换为 `-`（进 DB id/URL 安全）。
#[must_use]
pub fn sanitize_sharer(s: &str) -> String {
    let cleaned: String = s
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "admin".to_string()
    } else {
        cleaned
    }
}

/// 大厅条目 id：`<name>@<sharer>`（同 sharer 重复发布 = 刷新同一条，幂等）。
#[must_use]
pub fn lobby_id(name: &str, sharer: &str) -> String {
    format!("{name}@{}", sanitize_sharer(sharer))
}

/// 大厅行（model_lobby 表一行的内存镜像）。
#[derive(Debug, Clone)]
pub struct LobbyRow {
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub tags: Vec<String>,
    pub arch: String,
    pub size_bytes: u64,
    pub file_count: u32,
    pub sharer: String,
    pub source_url: String,
    pub created_at: String,
    pub download_count: u64,
}

/// 同 name 多发布者合并为一条（纯函数）：sources 按 created_at 升序聚合，
/// 展示字段取各来源中最"丰富"者（size/file_count 取最大、download_count 求和）。
#[must_use]
pub fn merge_lobby_rows(rows: &[LobbyRow]) -> Vec<LobbyMergedEntry> {
    // 按名分桶（保持首次出现顺序）
    let mut order: Vec<String> = Vec::new();
    let mut buckets: std::collections::HashMap<String, Vec<&LobbyRow>> =
        std::collections::HashMap::new();
    for r in rows {
        if !buckets.contains_key(&r.name) {
            order.push(r.name.clone());
        }
        buckets.entry(r.name.clone()).or_default().push(r);
    }
    let mut out: Vec<LobbyMergedEntry> = order
        .into_iter()
        .filter_map(|name| {
            let group = buckets.remove(&name)?;
            let mut sources: Vec<LobbySource> = group
                .iter()
                .map(|r| LobbySource {
                    sharer: r.sharer.clone(),
                    source_url: r.source_url.clone(),
                    size_bytes: r.size_bytes,
                    file_count: r.file_count,
                    created_at: r.created_at.clone(),
                })
                .collect();
            sources.sort_by(|a, b| a.created_at.cmp(&b.created_at));
            let head = group.first()?; // 展示字段基准行
            Some(LobbyMergedEntry {
                name,
                display_name: if head.display_name.is_empty() {
                    head.name.clone()
                } else {
                    head.display_name.clone()
                },
                description: head.description.clone(),
                tags: head.tags.clone(),
                arch: head.arch.clone(),
                size_bytes: group.iter().map(|r| r.size_bytes).max().unwrap_or(0),
                file_count: group.iter().map(|r| r.file_count).max().unwrap_or(0),
                download_count: group.iter().map(|r| r.download_count).sum(),
                sources,
                created_at: group
                    .iter()
                    .map(|r| r.created_at.as_str())
                    .min()
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect();
    // 下载量降序，其次名字升序（大厅默认排序）
    out.sort_by(|a, b| {
        b.download_count
            .cmp(&a.download_count)
            .then_with(|| a.name.cmp(&b.name))
    });
    out
}

/// 大厅搜索过滤（纯函数）：`name=` 精确匹配；`q=` 对
/// name/display_name/description/arch/tags 做大小写不敏感子串匹配。
#[must_use]
pub fn filter_lobby_entries(
    entries: Vec<LobbyMergedEntry>,
    name: Option<&str>,
    q: Option<&str>,
) -> Vec<LobbyMergedEntry> {
    entries
        .into_iter()
        .filter(|e| {
            if let Some(n) = name {
                if !n.is_empty() && e.name != n {
                    return false;
                }
            }
            if let Some(qs) = q {
                if !qs.is_empty() {
                    let ql = qs.to_lowercase();
                    let hay = format!(
                        "{} {} {} {} {}",
                        e.name,
                        e.display_name,
                        e.description,
                        e.arch,
                        e.tags.join(" ")
                    )
                    .to_lowercase();
                    if !hay.contains(&ql) {
                        return false;
                    }
                }
            }
            true
        })
        .collect()
}

/// 分享端点路径白名单（纯函数）：逐段校验相对路径（percent-decode 后），
/// 任一段为空 / `.` / `..`、含 `\` 或 NUL → 拒绝。
pub fn validate_share_rel_path(segments: &[&str]) -> Result<(), String> {
    if segments.is_empty() {
        return Err("缺少文件路径".into());
    }
    for seg in segments {
        if seg.is_empty() || *seg == "." || *seg == ".." {
            return Err(format!("非法路径段: {seg:?}"));
        }
        if seg.contains('\\') || seg.contains('\0') {
            return Err(format!("非法路径段: {seg:?}"));
        }
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// C 面纯函数：多源 URL 派生 / 轮转分配 / 续传判定
// ----------------------------------------------------------------------------

/// 从 source_url 拆出 `(scheme, host_port, token)`。
///
/// source_url 形如 `http://10.0.0.2:8080/api/v1/models/share/Qwen3?token=t`；
/// 解析失败（无 scheme/host）返回 None。
#[must_use]
pub fn split_source_url(source_url: &str) -> Option<(String, String, String)> {
    let (scheme, rest) = source_url.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let (authority, path_q) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
    if authority.is_empty() || authority.contains('/') {
        return None;
    }
    let token = path_q
        .split_once('?')
        .map(|(_, q)| q)
        .map(|query| {
            query
                .split('&')
                .find_map(|kv| kv.strip_prefix("token="))
                .unwrap_or("")
                .to_string()
        })
        .unwrap_or_default();
    Some((scheme.to_string(), authority.to_string(), token))
}

/// 派生远端 detail URL：`http://<authority>/api/v1/models/<name>/detail?token=<t>`。
///
/// 多源任务用它从每个候选源拉文件清单（同 token——大厅发布时 source_url 的
/// token 即该源的 admin token）。
#[must_use]
pub fn derive_detail_url(source_url: &str, name: &str) -> Option<String> {
    let (scheme, authority, token) = split_source_url(source_url)?;
    if token.is_empty() {
        Some(format!(
            "{scheme}://{authority}/api/v1/models/{name}/detail"
        ))
    } else {
        Some(format!(
            "{scheme}://{authority}/api/v1/models/{name}/detail?token={token}"
        ))
    }
}

/// 构造远端分享文件 URL：`…/api/v1/models/share/<name>/<rel>?token=&offset=&length=`。
#[must_use]
pub fn build_share_file_url(
    source_url: &str,
    name: &str,
    rel_path: &str,
    offset: u64,
    length: u64,
) -> Option<String> {
    let (scheme, authority, token) = split_source_url(source_url)?;
    let url = format!("{scheme}://{authority}/api/v1/models/share/{name}/{rel_path}");
    let mut q: Vec<String> = Vec::new();
    if !token.is_empty() {
        q.push(format!("token={token}"));
    }
    q.push(format!("offset={offset}"));
    q.push(format!("length={length}"));
    Some(format!("{url}?{}", q.join("&")))
}

/// 文件级轮转分配（纯函数）：文件 i → 源 `i % n_sources`。
///
/// 返回与 files 等长的源下标表。n_sources=0 返回空（调用方先校验）。
#[must_use]
pub fn assign_files_round_robin(n_files: usize, n_sources: usize) -> Vec<usize> {
    if n_sources == 0 {
        return Vec::new();
    }
    (0..n_files).map(|i| i % n_sources).collect()
}

/// 续传判定（纯函数）：.part 已有字节数对期望大小的安全偏移。
///
/// - part < expected → 从 part 处续传（返回 part 大小）；
/// - part >= expected（损坏/远端变更）→ 返回 0（从头重下，调用方先截断）。
#[must_use]
pub fn resume_offset_for(part_size: u64, expected_size: u64) -> u64 {
    if part_size < expected_size {
        part_size
    } else {
        0
    }
}

// ----------------------------------------------------------------------------
// 文件系统扫描（真实 spawn_blocking，失败降级不 panic）
// ----------------------------------------------------------------------------

/// 扫描本地模型库：遍历 `<root>/` 下子目录，含 `config.json` 的算一个模型。
///
/// 目录判定用 `path().is_dir()`（stat 跟随符号链接）——`POST /models/import`
/// 导入的符号链接模型因此与真实目录同权重可见。`DirEntry::file_type()` 不
/// 跟随链接，会把导入模型整个跳过，故不采用。
///
/// 失败（目录不存在/无权限）返回空 vec（不 panic）。
fn scan_local_models_blocking(root: &str) -> Vec<LocalModel> {
    let read = match std::fs::read_dir(root) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // 跳过隐藏目录
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        // stat 语义（跟随符号链接）：导入链接与真实目录一视同仁
        if !path.is_dir() {
            continue;
        }
        let path_str = path.to_string_lossy().into_owned();
        let has_config = path.join("config.json").exists();
        let (size_bytes, file_count) = dir_size_and_count(&path);
        let modified_at = dir_modified(&path);
        out.push(LocalModel {
            id: name.clone(),
            path: path_str,
            size_bytes,
            file_count,
            modified_at,
            has_config,
            source: "local".to_string(),
            display_name: name,
        });
    }
    // 按大小降序（大模型在前）
    out.sort_by_key(|m| std::cmp::Reverse(m.size_bytes));
    out
}

/// 递归算目录大小 + 文件数。
fn dir_size_and_count(p: &std::path::Path) -> (u64, u32) {
    let mut size: u64 = 0;
    let mut count: u32 = 0;
    let mut stack = vec![p.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                if let Ok(meta) = entry.metadata() {
                    size += meta.len();
                    count += 1;
                }
            }
        }
    }
    (size, count)
}

/// 取目录下最新修改时间（递归取所有条目 modified 的最大值，ISO 8601）。
fn dir_modified(p: &std::path::Path) -> String {
    let mut latest: Option<std::time::SystemTime> =
        std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let mut stack = vec![p.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            if let Ok(ft) = entry.file_type() {
                if ft.is_dir() {
                    stack.push(entry.path());
                }
            }
            if let Ok(meta) = entry.metadata() {
                if let Ok(m) = meta.modified() {
                    latest = Some(match latest {
                        Some(cur) if cur > m => cur,
                        _ => m,
                    });
                }
            }
        }
    }
    format_systemtime(latest.as_ref())
}

/// 把 SystemTime 格式化为 ISO 8601（失败/None 返回空串）。
fn format_systemtime(t: Option<&std::time::SystemTime>) -> String {
    let Some(t) = t else {
        return String::new();
    };
    use chrono::{DateTime, Local};
    DateTime::<Local>::from(*t)
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

/// 列模型目录顶层文件条目（不递归）。
fn list_model_files_blocking(dir: &str) -> Vec<ModelFile> {
    let mut out = Vec::new();
    let Ok(read) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let is_dir = meta.is_dir();
        out.push(ModelFile {
            name: if is_dir { format!("{name}/") } else { name },
            size_bytes: if is_dir { 0 } else { meta.len() },
            modified_at: format_systemtime(meta.modified().ok().as_ref()),
        });
    }
    out.sort_by_cached_key(|f| f.name.to_lowercase());
    out
}

/// 读取 config.json 并解析为 JSON Value（不存在/解析失败返回 None）。
fn read_config_json(dir: &str) -> Option<serde_json::Value> {
    let p = std::path::Path::new(dir).join("config.json");
    let text = std::fs::read_to_string(p).ok()?;
    serde_json::from_str(&text).ok()
}

// ----------------------------------------------------------------------------
// A 面：权重详细清单扫描（递归 + 分片解析 + config 解析，blocking）
// ----------------------------------------------------------------------------

/// 递归收集模型目录全部文件（相对路径 + 元数据），按路径排序。
fn walk_model_files_blocking(dir: &std::path::Path) -> Vec<ModelFileEx> {
    let mut out = Vec::new();
    let mut stack: Vec<(std::path::PathBuf, String)> = vec![(dir.to_path_buf(), String::new())];
    while let Some((cur, prefix)) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&cur) else {
            continue;
        };
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_dir() {
                stack.push((entry.path(), rel));
            } else if ft.is_file() {
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                let shard = parse_shard_filename(&name);
                out.push(ModelFileEx {
                    name: rel,
                    size_bytes: meta.len(),
                    modified_at: format_systemtime(meta.modified().ok().as_ref()),
                    shard_index: shard.as_ref().map(|s| s.index),
                    shard_total: shard.as_ref().map(|s| s.total),
                });
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// 权重详细扫描（blocking）：递归清单 + 分片完整性 + config 架构 + 总大小。
///
/// 目录解析顺序：`<root>/<name>`（自家库/导入链接，优先）→ 不在库内时按
/// `org--name` 反查 HF hub 缓存 snapshot（2026-09-03；大小/分片明细对 snapshot
/// 同样成立——文件 walker 与判定全复用）。两处都不存在返回 Err（404 语义）。
pub fn scan_model_weight_detail(root: &str, name: &str) -> Result<ModelWeightDetail, String> {
    validate_model_name(name)?;
    let own = std::path::Path::new(root).join(name);
    let dir = if own.is_dir() {
        own
    } else {
        resolve_hf_snapshot_by_id(name)
            .ok_or_else(|| format!("本地模型不存在: {name}"))?
    };
    let files = walk_model_files_blocking(&dir);
    let total_size_bytes: u64 = files.iter().map(|f| f.size_bytes).sum();
    let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
    let has_index_file = dir.join("model.safetensors.index.json").is_file();
    let shards = analyze_shards(&names, has_index_file);
    let has_config = dir.join("config.json").is_file();
    let has_any_weight = files.iter().any(|f| f.name.ends_with(".safetensors"));
    let config = read_config_json(&dir.to_string_lossy()).map(|v| parse_config_info(&v));
    Ok(ModelWeightDetail {
        name: name.to_string(),
        path: dir.to_string_lossy().into_owned(),
        total_size_bytes,
        file_count: files.len(),
        complete: judge_complete(&shards, has_config, has_any_weight),
        shards,
        config,
        files,
    })
}

// ----------------------------------------------------------------------------
// 进程探测
// ----------------------------------------------------------------------------

// ----------------------------------------------------------------------------
// HF 缓存删除守卫
// ----------------------------------------------------------------------------

/// HF 缓存条目删除守卫：库内无此名、但 HF 缓存命中 → Some(拒绝理由)。
///
/// HF hub 缓存是 huggingface 工具链的私有布局（blobs 硬链接复用 + refs 记账），
/// 只 rm snapshot 目录会留下孤儿 blobs、破坏缓存一致性——诚实拒绝，指引用
/// `hf` CLI 或整删 `models--<org>--<name>`。库内有同名目录/链接时返回 None
/// （走正常删除矩阵，自家目录优先）。
fn hf_delete_guard(root: &str, name: &str) -> Option<String> {
    let in_library =
        std::fs::symlink_metadata(std::path::Path::new(root).join(name)).is_ok();
    if in_library {
        return None;
    }
    resolve_hf_snapshot_by_id(name).map(|_| {
        format!(
            "HF 缓存模型 {name} 不在模型库删除范围——请用 huggingface CLI（hf cache）\
             清理，或整删 models--{name} 目录（含 blobs/refs/snapshots）"
        )
    })
}

/// 检查 pid 是否仍在运行（kill -0 探测）。pid 不存在或 kill 失败返回 false。
fn pid_alive(pid: u32) -> bool {
    // kill -0 不发信号，仅做存在性检查
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ----------------------------------------------------------------------------
// B 面 SQLite 持久化层（model_lobby 表，照 im.rs / nexhub_lobby.rs 建库惯例）
// ----------------------------------------------------------------------------

/// 默认 DB 路径：优先 `/tank/os-data/model_lobby.db`，再 `/var/lib/os/model_lobby.db`，
/// 最后 `./model_lobby.db`（与 im.rs 的 default_db_path 同模式）。
fn default_lobby_db_path() -> String {
    for p in &["/tank/os-data/model_lobby.db", "/var/lib/os/model_lobby.db"] {
        if std::path::Path::new(p)
            .parent()
            .is_some_and(|d| d.exists() || std::fs::create_dir_all(d).is_ok())
        {
            return (*p).to_string();
        }
    }
    "./model_lobby.db".to_string()
}

/// 打开 SQLite 文件 + WAL + 建表（失败降级内存库由调用方处理）。
fn open_lobby_db(path: &str) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    create_lobby_schema(&conn)?;
    Ok(conn)
}

/// 建表（IF NOT EXISTS）+ name 索引。
fn create_lobby_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS model_lobby (
            id              TEXT PRIMARY KEY,
            name            TEXT NOT NULL,
            display_name    TEXT DEFAULT '',
            description     TEXT DEFAULT '',
            tags            TEXT DEFAULT '[]',
            arch            TEXT DEFAULT '',
            size_bytes      INTEGER DEFAULT 0,
            file_count      INTEGER DEFAULT 0,
            sharer          TEXT DEFAULT 'admin',
            source_url      TEXT DEFAULT '',
            share_token     TEXT DEFAULT '',
            created_at      TEXT,
            download_count  INTEGER DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_model_lobby_name ON model_lobby(name);
        ",
    )
}

fn row_from_lobby(row: &rusqlite::Row) -> rusqlite::Result<LobbyRow> {
    let tags_json: String = row.get(4)?;
    let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
    let opt_i64 = |idx: usize| -> u64 {
        row.get::<_, Option<i64>>(idx)
            .ok()
            .flatten()
            .unwrap_or(0)
            .max(0) as u64
    };
    Ok(LobbyRow {
        id: row.get(0)?,
        name: row.get(1)?,
        display_name: row.get(2)?,
        description: row.get(3)?,
        tags,
        arch: row.get(5)?,
        size_bytes: opt_i64(6),
        file_count: opt_i64(7) as u32,
        sharer: row.get(8)?,
        source_url: row.get(9)?,
        created_at: row.get::<_, Option<String>>(11)?.unwrap_or_default(),
        download_count: opt_i64(12),
    })
}

/// 全量加载大厅行（created_at 升序）。
fn load_lobby_rows(conn: &Connection) -> Vec<LobbyRow> {
    let Ok(mut stmt) =
        conn.prepare("SELECT id,name,display_name,description,tags,arch,size_bytes,file_count,sharer,source_url,share_token,created_at,download_count FROM model_lobby ORDER BY created_at ASC, id ASC")
    else {
        return Vec::new();
    };
    stmt.query_map([], row_from_lobby)
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

/// 按 id 找单行。
fn find_lobby_row(conn: &Connection, id: &str) -> Option<LobbyRow> {
    conn.query_row(
        "SELECT id,name,display_name,description,tags,arch,size_bytes,file_count,sharer,source_url,share_token,created_at,download_count FROM model_lobby WHERE id = ?1",
        params![id],
        row_from_lobby,
    )
    .ok()
}

/// 插入/刷新一条发布（同 id 重复发布 = 刷新快照，保留 download_count）。
fn upsert_lobby_entry(conn: &Connection, r: &LobbyRow, share_token: &str) -> rusqlite::Result<()> {
    let kept: i64 = conn
        .query_row(
            "SELECT download_count FROM model_lobby WHERE id = ?1",
            params![r.id],
            |row| row.get(0),
        )
        .unwrap_or(0);
    conn.execute(
        "INSERT OR REPLACE INTO model_lobby \
         (id,name,display_name,description,tags,arch,size_bytes,file_count,sharer,source_url,share_token,created_at,download_count) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
        params![
            r.id,
            r.name,
            r.display_name,
            r.description,
            serde_json::to_string(&r.tags).unwrap_or_else(|_| "[]".into()),
            r.arch,
            r.size_bytes as i64,
            r.file_count as i64,
            r.sharer,
            r.source_url,
            share_token,
            r.created_at,
            kept,
        ],
    )?;
    Ok(())
}

/// 按 id 删除一行，返回是否删到。
fn delete_lobby_entry(conn: &Connection, id: &str) -> bool {
    conn.execute("DELETE FROM model_lobby WHERE id = ?1", params![id])
        .map(|n| n > 0)
        .unwrap_or(false)
}

/// 同 name 全部来源 download_count +1（多源任务完成时归因全体分享者）。
fn bump_lobby_downloads(conn: &Connection, name: &str) {
    let _ = conn.execute(
        "UPDATE model_lobby SET download_count = download_count + 1 WHERE name = ?1",
        params![name],
    );
}

// ----------------------------------------------------------------------------
// C 面：多源下载引擎（lobby_multi）
// ----------------------------------------------------------------------------

/// 单文件分块大小（4 MiB——内存上界 = 源数 × 4 MiB + base64 解码临时缓冲）。
const MULTI_CHUNK_BYTES: u64 = 4 * 1024 * 1024;

/// 服务端单次回传上限（share 端点按此拒绝超长请求，双方契约一致）。
const SHARE_MAX_CHUNK_BYTES: u64 = 64 * 1024 * 1024;

/// 进程级共享 HTTP 客户端（连接池复用；api_gateway 同款 once_cell 模式）。
///
/// 必须带 User-Agent：魔搭 CDN 的 WAF 对无 UA 请求回 403（2026-08-31 生产实测，
/// merges.txt 走 CDN 302 后 403，curl 带 UA 同 URL 206）。
static MULTI_HTTP: once_cell::sync::Lazy<reqwest::Client> = once_cell::sync::Lazy::new(|| {
    reqwest::Client::builder()
        .user_agent(concat!("nexos-os-api/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
});

/// 多源任务的执行计划（清单 + 轮转分配；runner 消费）。
#[derive(Debug, Clone)]
struct MultiPlan {
    name: String,
    local_dir: String,
    sources: Vec<String>,
    files: Vec<ManifestFile>,
    /// 与 files 等长的源下标表（`assign_files_round_robin` 产物）。
    assignments: Vec<usize>,
}

/// 原子更新共享任务列表里的一条任务（任务被取消移除后为 no-op）。
fn update_multi_task(
    tasks: &Arc<Mutex<Vec<LobbyMultiTask>>>,
    id: &str,
    f: impl FnOnce(&mut LobbyMultiTask),
) {
    let mut guard = tasks.lock().expect("multi tasks poisoned");
    if let Some(t) = guard.iter_mut().find(|t| t.id == id) {
        f(t);
    }
}

/// 任务是否应中止（从列表消失 = 已被 DELETE 取消）。
fn multi_task_aborted(tasks: &Arc<Mutex<Vec<LobbyMultiTask>>>, id: &str) -> bool {
    tasks
        .lock()
        .expect("multi tasks poisoned")
        .iter()
        .all(|t| t.id != id)
}

/// 推一条文件级简报（保留最近 5 条，最新在尾）。
fn push_file_progress(t: &mut LobbyMultiTask, fp: FileProgress) {
    t.recent_files.push(fp);
    let overflow = t.recent_files.len().saturating_sub(5);
    for _ in 0..overflow {
        t.recent_files.remove(0);
    }
}

/// 从首个可达源拉文件清单（`/models/:name/detail`，同 token）。
///
/// 逐源尝试（大厅 sources 顺序），HTTP 失败/非 200/JSON 解析失败都换下一个源；
/// 全部不可达 → Err（POST 创建端点据此 502）。
async fn fetch_manifest_from_sources(
    sources: &[String],
    name: &str,
) -> Result<(Vec<ManifestFile>, usize), String> {
    let mut last_err = String::new();
    for (i, src) in sources.iter().enumerate() {
        let Some(url) = derive_detail_url(src, name) else {
            last_err = format!("源地址非法: {src}");
            continue;
        };
        let attempt = MULTI_HTTP
            .get(&url)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await;
        let resp = match attempt {
            Ok(r) => r,
            Err(e) => {
                last_err = format!("源 {src} 不可达: {e}");
                continue;
            }
        };
        if !resp.status().is_success() {
            last_err = format!("源 {src} 返回 {}", resp.status());
            continue;
        }
        let body = match resp.json::<serde_json::Value>().await {
            Ok(b) => b,
            Err(e) => {
                last_err = format!("源 {src} 清单解析失败: {e}");
                continue;
            }
        };
        let Some(arr) = body.get("files").and_then(|f| f.as_array()) else {
            last_err = format!("源 {src} 清单缺 files 数组");
            continue;
        };
        let files: Vec<ManifestFile> = arr
            .iter()
            .filter_map(|f| {
                let n = f.get("name")?.as_str()?.to_string();
                let s = f.get("size_bytes")?.as_u64()?;
                Some(ManifestFile {
                    name: n,
                    size_bytes: s,
                })
            })
            .collect();
        if files.is_empty() {
            last_err = format!("源 {src} 清单为空");
            continue;
        }
        return Ok((files, i));
    }
    Err(format!(
        "全部 {} 个源的模型清单均不可用（最后错误: {last_err}）",
        sources.len()
    ))
}

/// 从单个源下载一个文件（断点续传 + 分块拉取 + 终态 size 校验 + 原子 rename）。
///
/// 返回 Ok(本次写入字节数)。失败保留 .part（供换源后续传）；size 不匹配时
/// 主动清掉 .part（内容不可信，从头重下）。
async fn download_file_with_source(
    source_url: &str,
    name: &str,
    local_dir: &std::path::Path,
    rel: &str,
    expected_size: u64,
    tasks: &Arc<Mutex<Vec<LobbyMultiTask>>>,
    task_id: &str,
) -> Result<u64, String> {
    let final_path = local_dir.join(rel);
    if let Some(parent) = final_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建目标目录失败 {}: {e}", parent.display()))?;
    }
    let part_path = std::path::PathBuf::from(format!("{}.part", final_path.display()));
    let part_size = part_path.metadata().map(|m| m.len()).unwrap_or(0);
    let mut offset = resume_offset_for(part_size, expected_size);
    if offset == 0 && part_path.exists() {
        std::fs::remove_file(&part_path).map_err(|e| format!("重置损坏的 .part 失败: {e}"))?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&part_path)
        .await
        .map_err(|e| format!("打开 .part 失败: {e}"))?;
    let mut written: u64 = 0;
    while offset < expected_size {
        if multi_task_aborted(tasks, task_id) {
            return Err("任务已取消".into());
        }
        let want = (expected_size - offset).min(MULTI_CHUNK_BYTES);
        let Some(url) = build_share_file_url(source_url, name, rel, offset, want) else {
            return Err(format!("源地址非法: {source_url}"));
        };
        let resp = MULTI_HTTP
            .get(&url)
            .timeout(std::time::Duration::from_secs(120))
            .send()
            .await
            .map_err(|e| format!("分块请求失败 @{offset}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("分块请求返回 {} @{offset}", resp.status()));
        }
        let body = resp
            .json::<serde_json::Value>()
            .await
            .map_err(|e| format!("分块响应解析失败 @{offset}: {e}"))?;
        let b64 = body
            .get("content_base64")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("分块响应缺 content_base64 @{offset}"))?;
        let data = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| format!("分块 base64 解码失败 @{offset}: {e}"))?;
        if data.len() as u64 != want {
            let _ = tokio::fs::remove_file(&part_path).await;
            return Err(format!(
                "分块长度不符 @{offset}: 期望 {want} 实得 {}",
                data.len()
            ));
        }
        use tokio::io::AsyncWriteExt;
        file.write_all(&data)
            .await
            .map_err(|e| format!("写入 .part 失败 @{offset}: {e}"))?;
        offset += want;
        written += want;
        let inc = want;
        update_multi_task(tasks, task_id, |t| {
            t.bytes_done += inc;
        });
    }
    // tokio::fs::File 有内部写缓冲，drop 是异步后台刷盘——必须显式 sync_all
    // 落盘后再做尺寸校验/rename，否则 metadata 读到滞后状态（实测 0/半截字节）。
    file.sync_all()
        .await
        .map_err(|e| format!("刷盘 .part 失败: {e}"))?;
    drop(file);
    // 终态校验：.part 尺寸必须精确匹配清单大小，随后原子 rename 落位
    let got = part_path.metadata().map(|m| m.len()).unwrap_or(0);
    if got != expected_size {
        let _ = tokio::fs::remove_file(&part_path).await;
        return Err(format!(
            "文件大小校验失败 {rel}: 期望 {expected_size} 实得 {got}"
        ));
    }
    tokio::fs::rename(&part_path, &final_path)
        .await
        .map_err(|e| format!("落位失败 {rel}: {e}"))?;
    Ok(written)
}

/// 单源 worker：顺序处理分到本源的文件，每文件失败换下一个源重试（全源轮一遍）。
///
/// 返回 Err((文件名, 错误))——首个彻底失败的文件（所有源都失败）。
async fn multi_worker(
    source_index: usize,
    plan: &MultiPlan,
    files: Vec<ManifestFile>,
    tasks: Arc<Mutex<Vec<LobbyMultiTask>>>,
    task_id: String,
) -> Result<(), (String, String)> {
    let n = plan.sources.len();
    for mf in files {
        if multi_task_aborted(&tasks, &task_id) {
            return Err((mf.name, "任务已取消".into()));
        }
        // 失败换源：从分配源起轮转整圈（单源时即重试 1 次）
        let mut last_err = String::new();
        let mut ok = false;
        for attempt in 0..n {
            let idx = (source_index + attempt) % n;
            let src = plan.sources[idx].clone();
            match download_file_with_source(
                &src,
                &plan.name,
                std::path::Path::new(&plan.local_dir),
                &mf.name,
                mf.size_bytes,
                &tasks,
                &task_id,
            )
            .await
            {
                Ok(bytes) => {
                    update_multi_task(&tasks, &task_id, |t| {
                        t.files_done += 1;
                        push_file_progress(
                            t,
                            FileProgress {
                                file: mf.name.clone(),
                                source: src.clone(),
                                bytes,
                                status: "done".into(),
                                error: None,
                            },
                        );
                    });
                    ok = true;
                    break;
                }
                Err(e) => {
                    last_err = e;
                    update_multi_task(&tasks, &task_id, |t| {
                        push_file_progress(
                            t,
                            FileProgress {
                                file: mf.name.clone(),
                                source: src.clone(),
                                bytes: 0,
                                status: "failed".into(),
                                error: Some(last_err.clone()),
                            },
                        );
                    });
                }
            }
        }
        if !ok {
            return Err((mf.name, last_err));
        }
    }
    Ok(())
}

/// 多源任务主 runner：n 源各起一个 worker 并行（文件级并行=n 源 n 文件并发），
/// 全部 join 后做逐文件终态校验，再落 completed/failed + download_count 归因。
async fn run_multi_download(
    plan: MultiPlan,
    tasks: Arc<Mutex<Vec<LobbyMultiTask>>>,
    lobby_db: Arc<Mutex<Connection>>,
    task_id: String,
) {
    // 按分配源分桶
    let mut buckets: Vec<Vec<ManifestFile>> = vec![Vec::new(); plan.sources.len()];
    for (mf, idx) in plan.files.iter().zip(plan.assignments.iter()) {
        if *idx < buckets.len() {
            buckets[*idx].push(mf.clone());
        }
    }
    // active_sources 初值 = 全部源；worker 收摊后清除
    update_multi_task(&tasks, &task_id, |t| {
        t.active_sources = plan.sources.clone();
    });
    let mut handles = Vec::new();
    for (idx, files) in buckets.into_iter().enumerate() {
        if files.is_empty() {
            continue;
        }
        let tasks_c = tasks.clone();
        let id_c = task_id.clone();
        let plan_c = plan.clone();
        handles.push(tokio::spawn(async move {
            multi_worker(idx, &plan_c, files, tasks_c, id_c).await
        }));
    }
    let mut first_err: Option<(String, String)> = None;
    for h in handles {
        match h.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => first_err = first_err.or(Some(e)),
            Err(e) => first_err = first_err.or(Some(("worker".into(), e.to_string()))),
        }
    }
    // 收摊：终态校验（每文件 size 必须匹配清单）→ completed/failed
    let aborted = multi_task_aborted(&tasks, &task_id);
    update_multi_task(&tasks, &task_id, |t| {
        t.active_sources.clear();
        if aborted {
            t.status = "failed".into();
            t.error = Some("用户取消".into());
            return;
        }
        if let Some((file, err)) = &first_err {
            t.status = "failed".into();
            t.error = Some(format!("文件 {file} 全源下载失败: {err}"));
            return;
        }
        // 逐文件终态校验
        for mf in &plan.files {
            let got = std::path::Path::new(&plan.local_dir)
                .join(&mf.name)
                .metadata()
                .map(|m| m.len())
                .unwrap_or(0);
            if got != mf.size_bytes {
                t.status = "failed".into();
                t.error = Some(format!(
                    "完成校验失败 {}: 期望 {} 实得 {got}",
                    mf.name, mf.size_bytes
                ));
                return;
            }
        }
        t.status = "completed".into();
        t.files_done = plan.files.len();
        t.bytes_done = t.total_bytes;
    });
    // 成功才给大厅全体分享者 download_count +1
    let completed = {
        tasks
            .lock()
            .expect("multi tasks poisoned")
            .iter()
            .any(|t| t.id == task_id && t.status == "completed")
    };
    if completed {
        if let Ok(conn) = lobby_db.lock() {
            bump_lobby_downloads(&conn, &plan.name);
        }
    }
}

// ----------------------------------------------------------------------------
// D 面：在线仓库源（ModelScope / HF 镜像 HTTP 直连下载，无 CLI 依赖）
// ----------------------------------------------------------------------------

/// 在线仓库源类型（模型大厅下载源抽象：与 lobby_multi 的 peer share 源并列，
/// 这两个源直连公网模型仓库的 HTTP API，不依赖本机安装任何 CLI）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteRepoKind {
    /// 魔搭 ModelScope（国内网络快，默认源）。
    Modelscope,
    /// HuggingFace 镜像（hf-mirror.com，HF 协议兼容）。
    HfMirror,
}

impl RemoteRepoKind {
    /// 从请求路径/body 的 kind 字符串解析（`modelscope` / `hf`）。
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "modelscope" => Some(Self::Modelscope),
            "hf" | "hf_mirror" | "hfmirror" => Some(Self::HfMirror),
            _ => None,
        }
    }

    /// slug（REST 路径与任务 JSON 里的稳定标识）。
    #[must_use]
    pub fn slug(&self) -> &'static str {
        match self {
            Self::Modelscope => "modelscope",
            Self::HfMirror => "hf",
        }
    }

    /// 展示名（前端徽章/日志）。
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Modelscope => "ModelScope",
            Self::HfMirror => "HF 镜像",
        }
    }

    /// API 基地址：`NEXOS_MODELSCOPE_BASE`（默认 https://www.modelscope.cn）/
    /// `NEXOS_HF_BASE`（默认 https://hf-mirror.com）——trim 非空生效，去尾 `/`。
    #[must_use]
    pub fn base(&self) -> String {
        let raw = match self {
            Self::Modelscope => std::env::var("NEXOS_MODELSCOPE_BASE")
                .unwrap_or_else(|_| "https://www.modelscope.cn".into()),
            Self::HfMirror => {
                std::env::var("NEXOS_HF_BASE").unwrap_or_else(|_| "https://hf-mirror.com".into())
            }
        };
        raw.trim().trim_end_matches('/').to_string()
    }

    /// 私有模型访问令牌（可选）：`NEXOS_MODELSCOPE_TOKEN` / `NEXOS_HF_TOKEN`，
    /// 请求时注入 `Authorization: Bearer <token>`（跨主机重定向由 reqwest 自动剥头）。
    #[must_use]
    pub fn token(&self) -> Option<String> {
        let v = match self {
            Self::Modelscope => std::env::var("NEXOS_MODELSCOPE_TOKEN").ok(),
            Self::HfMirror => std::env::var("NEXOS_HF_TOKEN").ok(),
        };
        v.map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
    }

    /// 文件清单 URL（实测 2026-08-31）：
    /// - ModelScope: `GET {base}/api/v1/models/<org>/<model>/repo/files?Recursive=true`
    ///   → `{"Code":200,"Data":{"Files":[{"Path","Size","Type":"blob"|"tree",...}]}}`
    ///   （任务简报里写的 `/download?FilePath=` 端点实测 404，不存在——勿用）
    /// - HF 镜像: `GET {base}/api/models/<org>/<model>/tree/main?recursive=true`
    ///   → `[{path,size,type:"file"|"directory",lfs?}]`
    #[must_use]
    pub fn files_url(&self, repo_id: &str) -> String {
        match self {
            Self::Modelscope => format!(
                "{}/api/v1/models/{}/repo/files?Recursive=true",
                self.base(),
                encode_path(repo_id)
            ),
            Self::HfMirror => format!(
                "{}/api/models/{}/tree/main?recursive=true",
                self.base(),
                encode_path(repo_id)
            ),
        }
    }

    /// 单文件下载 URL（Range 断点续传，二者均 `Accept-Ranges: bytes`）：
    /// - ModelScope: `{base}/<org>/<model>/resolve/master/<path>`（小文件 Range 回
    ///   200+Content-Range，LFS 大文件 302→CDN 206；**open-ended Range 被忽略**——
    ///   必须 bounded `bytes=a-b`）
    /// - HF 镜像: `{base}/<org>/<model>/resolve/main/<path>`（307→resolve-cache）
    #[must_use]
    pub fn resolve_url(&self, repo_id: &str, rel_path: &str) -> String {
        match self {
            Self::Modelscope => format!(
                "{}/{}/resolve/master/{}",
                self.base(),
                encode_path(repo_id),
                encode_path(rel_path)
            ),
            Self::HfMirror => format!(
                "{}/{}/resolve/main/{}",
                self.base(),
                encode_path(repo_id),
                encode_path(rel_path)
            ),
        }
    }
}

/// 单文件分块大小（16 MiB——bounded Range；内存上界 = 并发任务数 × 64 KiB 流缓冲）。
const REMOTE_CHUNK_BYTES: u64 = 16 * 1024 * 1024;

/// 单文件下载失败重试次数（单源无换源可选，网络抖动靠重试 + .part 续传兜底）。
const REMOTE_FILE_RETRIES: usize = 3;

/// 探测响应里的文件条目（名称/大小/向导默认勾选标记）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRepoFile {
    /// 相对仓库根的路径（`/` 分隔，含子目录）。
    pub name: String,
    pub size_bytes: u64,
    /// 添加模型向导的默认勾选（权重/config/tokenizer 勾，README/LICENSE/图片不勾）。
    pub default_selected: bool,
}

/// `GET /api/v1/models/remote/:kind/:org/:model` 响应（探测 = 存在性 + 文件清单）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteRepoProbe {
    pub ok: bool,
    /// 源 slug（`modelscope` / `hf`）。
    pub kind: String,
    /// `Qwen/Qwen3-VL-8B-Instruct`。
    pub repo_id: String,
    /// 本地目录名建议（repo_id 末段）。
    pub name: String,
    /// 文件总数（已滤目录条目）。
    pub file_count: usize,
    /// 全部文件大小之和（下载计划估算）。
    pub total_size_bytes: u64,
    pub files: Vec<RemoteRepoFile>,
}

/// `remote_repo` 在线仓库下载任务（`POST /models/remote/downloads` 创建）。
///
/// 状态字段与 `LobbyMultiTask` 同构（files_done/bytes_done/total_bytes/recent_files），
/// 前端下载页同一套卡片渲染。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteRepoTask {
    pub id: String,
    /// 恒 `remote_repo`（与 modelscope / lobby_multi 混排区分）。
    pub r#type: String,
    /// 源 slug（`modelscope` / `hf`）。
    pub kind: String,
    pub repo_id: String,
    /// 本地模型目录名（= local_dir 末段）。
    pub name: String,
    pub local_dir: String,
    /// `downloading` / `completed` / `failed`。
    pub status: String,
    pub files_total: usize,
    pub files_done: usize,
    pub bytes_done: u64,
    pub total_bytes: u64,
    /// 最近 5 条文件级简报（复用 FileProgress）。
    pub recent_files: Vec<FileProgress>,
    pub cancel_requested: bool,
    pub error: Option<String>,
    pub created_at: String,
}

/// 校验 `org/model` 形态的仓库 id（两源同构；段内仅 `[A-Za-z0-9._-]`——
/// 天然免 URL 注入，无需编码）。
pub fn validate_repo_id(repo_id: &str) -> Result<(String, String), String> {
    let id = repo_id.trim();
    let Some((org, model)) = id.split_once('/') else {
        return Err(format!("仓库 id 须为 org/model 形态（如 Qwen/Qwen3-VL-8B-Instruct）: {id}"));
    };
    let seg_ok = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    };
    if !seg_ok(org) || !seg_ok(model) || model.contains('/') {
        return Err(format!(
            "仓库 id 段仅允许字母/数字/./_/-(且须恰好一段 org + 一段 model): {id}"
        ));
    }
    Ok((org.to_string(), model.to_string()))
}

/// 路径逐段 percent-encode（`/` 保留为分隔符；unreserved 集外全部 `%XX`）。
///
/// 与 files.rs 的 `url_decode` 互逆；仓库 id 已由 [`validate_repo_id`] 限定字符集，
/// 此处主要服务含中文/空格/`+` 的**文件路径**。
#[must_use]
pub fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 添加模型向导默认勾选规则（纯函数）：权重扩展名 + config/tokenizer 系勾，
/// README/LICENSE/.gitattributes/图片等附件不勾（用户可手动补勾）。
#[must_use]
pub fn is_default_selected(rel_path: &str) -> bool {
    let lower = rel_path.to_lowercase();
    const EXTS: [&str; 8] = [
        ".safetensors", ".bin", ".pt", ".pth", ".gguf", ".onnx", ".json", ".txt",
    ];
    if EXTS.iter().any(|e| lower.ends_with(e)) {
        return true;
    }
    // sentencepiece 系（无扩展名或 .model 结尾）
    lower.ends_with("spiece.model")
        || lower.ends_with("tokenizer.model")
        || lower.ends_with("sentencepiece.bpe.model")
}

/// 解析 ModelScope files API 响应（纯函数）：`Data.Files[]` 里 `Type=="blob"` 的
/// 条目取 `Path`/`Size`（`tree` 为目录条目，Size=0，滤除）；按路径排序。
pub fn parse_modelscope_files(body: &serde_json::Value) -> Result<Vec<ManifestFile>, String> {
    if body.get("Code").and_then(|c| c.as_i64()) != Some(200) {
        let msg = body
            .get("Message")
            .and_then(|m| m.as_str())
            .unwrap_or("Code 非 200");
        return Err(format!("ModelScope API 拒绝: {msg}"));
    }
    let arr = body
        .pointer("/Data/Files")
        .and_then(|f| f.as_array())
        .ok_or("响应缺 Data.Files 数组")?;
    let mut files: Vec<ManifestFile> = arr
        .iter()
        .filter(|f| f.get("Type").and_then(|t| t.as_str()) == Some("blob"))
        .filter_map(|f| {
            let name = f.get("Path").and_then(|p| p.as_str())?.to_string();
            let size = f.get("Size").and_then(|s| s.as_u64())?;
            if name.is_empty() {
                None
            } else {
                Some(ManifestFile {
                    name,
                    size_bytes: size,
                })
            }
        })
        .collect();
    files.sort_by(|a, b| a.name.cmp(&b.name));
    if files.is_empty() {
        return Err("文件清单为空（私有仓库需配 NEXOS_MODELSCOPE_TOKEN）".into());
    }
    Ok(files)
}

/// 解析 HF 镜像 tree API 响应（纯函数）：顶层数组里 `type=="file"` 的条目取
/// `path`/`size`（LFS 文件的 `size` 即逻辑大小，无需看 `lfs.size`）。
pub fn parse_hf_tree(body: &serde_json::Value) -> Result<Vec<ManifestFile>, String> {
    let arr = body.as_array().ok_or("响应不是文件数组（模型不存在或私有）")?;
    let mut files: Vec<ManifestFile> = arr
        .iter()
        .filter(|f| f.get("type").and_then(|t| t.as_str()) == Some("file"))
        .filter_map(|f| {
            let name = f.get("path").and_then(|p| p.as_str())?.to_string();
            let size = f.get("size").and_then(|s| s.as_u64())?;
            if name.is_empty() {
                None
            } else {
                Some(ManifestFile {
                    name,
                    size_bytes: size,
                })
            }
        })
        .collect();
    files.sort_by(|a, b| a.name.cmp(&b.name));
    if files.is_empty() {
        return Err("文件清单为空（私有仓库需配 NEXOS_HF_TOKEN）".into());
    }
    Ok(files)
}

/// 原子更新共享远程任务列表里的一条（任务被取消移除后为 no-op）。
fn update_remote_task(
    tasks: &Arc<Mutex<Vec<RemoteRepoTask>>>,
    id: &str,
    f: impl FnOnce(&mut RemoteRepoTask),
) {
    let mut guard = tasks.lock().expect("remote tasks poisoned");
    if let Some(t) = guard.iter_mut().find(|t| t.id == id) {
        f(t);
    }
}

/// 远程任务是否应中止（从列表消失 = 已被 DELETE 取消）。
fn remote_task_aborted(tasks: &Arc<Mutex<Vec<RemoteRepoTask>>>, id: &str) -> bool {
    tasks
        .lock()
        .expect("remote tasks poisoned")
        .iter()
        .all(|t| t.id != id)
}

/// 探测在线仓库（存在性 + 文件清单；files 接口即存在性——HTTP 404 / Code!=200
/// / 非数组都归一为 Err，POST 创建与 GET 探测共用）。
pub async fn probe_remote_repo(
    kind: RemoteRepoKind,
    repo_id: &str,
) -> Result<RemoteRepoProbe, String> {
    let (org, model) = validate_repo_id(repo_id)?;
    let repo_id = format!("{org}/{model}");
    let url = kind.files_url(&repo_id);
    let mut req = MULTI_HTTP
        .get(&url)
        .timeout(std::time::Duration::from_secs(30))
        .header("Accept", "application/json");
    if let Some(t) = kind.token() {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("{} 探测请求失败: {e}", kind.label()))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(format!("仓库不存在（或无权访问）: {repo_id}"));
    }
    if !resp.status().is_success() {
        return Err(format!(
            "{} 探测返回 {}（仓库 {}）",
            kind.label(),
            resp.status(),
            repo_id
        ));
    }
    let body = resp
        .json::<serde_json::Value>()
        .await
        .map_err(|e| format!("{} 清单解析失败: {e}", kind.label()))?;
    let files = match kind {
        RemoteRepoKind::Modelscope => parse_modelscope_files(&body)?,
        RemoteRepoKind::HfMirror => parse_hf_tree(&body)?,
    };
    let total: u64 = files.iter().map(|f| f.size_bytes).sum();
    Ok(RemoteRepoProbe {
        ok: true,
        kind: kind.slug().to_string(),
        repo_id,
        name: model,
        file_count: files.len(),
        total_size_bytes: total,
        files: files
            .into_iter()
            .map(|f| RemoteRepoFile {
                default_selected: is_default_selected(&f.name),
                name: f.name,
                size_bytes: f.size_bytes,
            })
            .collect(),
    })
}

/// 从在线仓库下载一个文件（bounded Range 分块 + `.part` 续传 + 流式落盘 +
/// 终态 size 校验 + 原子 rename）。返回 Ok(本次写入字节数)。
///
/// 实测坑：ModelScope 对 open-ended Range（`bytes=N-`）**忽略并回全量**，故必须
/// 用 bounded `bytes=a-b` 逐块拉；块内流式读取边收边写（内存 64 KiB 级），
/// 收满 `want` 字节即止——服务端若忽略 Range 回超量数据会在收满后被截断检测。
async fn download_remote_file(
    kind: RemoteRepoKind,
    repo_id: &str,
    local_dir: &std::path::Path,
    rel: &str,
    expected_size: u64,
    tasks: &Arc<Mutex<Vec<RemoteRepoTask>>>,
    task_id: &str,
) -> Result<u64, String> {
    let final_path = local_dir.join(rel);
    if let Some(parent) = final_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建目标目录失败 {}: {e}", parent.display()))?;
    }
    let part_path = std::path::PathBuf::from(format!("{}.part", final_path.display()));
    let part_size = part_path.metadata().map(|m| m.len()).unwrap_or(0);
    let mut offset = resume_offset_for(part_size, expected_size);
    if offset == 0 && part_path.exists() {
        std::fs::remove_file(&part_path).map_err(|e| format!("重置损坏的 .part 失败: {e}"))?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&part_path)
        .await
        .map_err(|e| format!("打开 .part 失败: {e}"))?;
    let mut written: u64 = 0;
    while offset < expected_size {
        if remote_task_aborted(tasks, task_id) {
            return Err("任务已取消".into());
        }
        let want = (expected_size - offset).min(REMOTE_CHUNK_BYTES);
        let url = kind.resolve_url(repo_id, rel);
        let mut req = MULTI_HTTP
            .get(&url)
            .timeout(std::time::Duration::from_secs(600))
            .header("Range", format!("bytes={}-{}", offset, offset + want - 1));
        if let Some(t) = kind.token() {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("分块请求失败 @{offset}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("分块请求返回 {} @{offset}", resp.status()));
        }
        // 流式收取恰好 want 字节（多收=Range 被忽略 → 拒绝；少收=连接截断 → 报错保留 .part）
        let mut resp = resp;
        let mut received: u64 = 0;
        use tokio::io::AsyncWriteExt;
        while received < want {
            let chunk = resp
                .chunk()
                .await
                .map_err(|e| format!("分块流读取失败 @{offset}+{received}: {e}"))?;
            let Some(data) = chunk else { break };
            if received + data.len() as u64 > want {
                // 服务端忽略 Range 回了超量内容——内容起点仍正确但尾部越界，
                // 截断到 want 以内继续（保守做法：只取本块应得部分）
                let keep = (want - received) as usize;
                file.write_all(&data[..keep])
                    .await
                    .map_err(|e| format!("写入 .part 失败 @{offset}+{received}: {e}"))?;
                received += keep as u64;
                break;
            }
            file.write_all(&data)
                .await
                .map_err(|e| format!("写入 .part 失败 @{offset}+{received}: {e}"))?;
            received += data.len() as u64;
        }
        if received != want {
            return Err(format!(
                "分块长度不符 @{offset}: 期望 {want} 实得 {received}（连接截断，.part 已保留可续传）"
            ));
        }
        offset += want;
        written += want;
        let inc = want;
        update_remote_task(tasks, task_id, |t| {
            t.bytes_done += inc;
        });
    }
    file.sync_all()
        .await
        .map_err(|e| format!("刷盘 .part 失败: {e}"))?;
    drop(file);
    let got = part_path.metadata().map(|m| m.len()).unwrap_or(0);
    if got != expected_size {
        let _ = tokio::fs::remove_file(&part_path).await;
        return Err(format!(
            "文件大小校验失败 {rel}: 期望 {expected_size} 实得 {got}"
        ));
    }
    tokio::fs::rename(&part_path, &final_path)
        .await
        .map_err(|e| format!("落位失败 {rel}: {e}"))?;
    Ok(written)
}

/// 远程下载文件级并发数：env `NEXOS_MODELHUB_DL_CONCURRENCY`（缺省 3，
/// 解析失败回缺省；<1 收敛 1，>8 收敛 8——对上游限流的礼貌上限）。
const REMOTE_DL_CONCURRENCY_DEFAULT: usize = 3;
const REMOTE_DL_CONCURRENCY_MAX: usize = 8;

/// 解析并发数（纯函数，测试直测）：空/非法 → 3；`0`/负数 → 1；超上限 → 8。
fn remote_dl_concurrency(raw: &str) -> usize {
    let n = raw.trim().parse::<usize>().unwrap_or(REMOTE_DL_CONCURRENCY_DEFAULT);
    n.clamp(1, REMOTE_DL_CONCURRENCY_MAX)
}

/// 单文件重试循环（每文件 REMOTE_FILE_RETRIES 次，单源无换源；取消快速失败）。
/// 成功 → files_done 递增 + done 简报；耗尽 → Err（含"重试 N 次仍失败"前缀）。
async fn run_remote_file_with_retries(
    kind: RemoteRepoKind,
    repo_id: &str,
    dir: &std::path::Path,
    mf: &ManifestFile,
    tasks: &Arc<Mutex<Vec<RemoteRepoTask>>>,
    task_id: &str,
) -> Result<(), String> {
    let mut last_err = String::new();
    for attempt in 1..=REMOTE_FILE_RETRIES {
        if remote_task_aborted(tasks, task_id) {
            return Err("任务已取消".into());
        }
        match download_remote_file(
            kind,
            repo_id,
            dir,
            &mf.name,
            mf.size_bytes,
            tasks,
            task_id,
        )
        .await
        {
            Ok(bytes) => {
                // files_done 原子递增（update_remote_task 持锁内 +1，并行无丢失）
                update_remote_task(tasks, task_id, |t| {
                    t.files_done += 1;
                    push_file_progress_remote(
                        t,
                        FileProgress {
                            file: mf.name.clone(),
                            source: kind.label().to_string(),
                            bytes,
                            status: "done".into(),
                            error: None,
                        },
                    );
                });
                return Ok(());
            }
            Err(e) => {
                last_err = e;
                let cancelled = last_err.contains("取消");
                update_remote_task(tasks, task_id, |t| {
                    push_file_progress_remote(
                        t,
                        FileProgress {
                            file: mf.name.clone(),
                            source: kind.label().to_string(),
                            bytes: 0,
                            status: "failed".into(),
                            error: Some(last_err.clone()),
                        },
                    );
                });
                if cancelled {
                    return Err("任务已取消".into());
                }
                if attempt < REMOTE_FILE_RETRIES {
                    tokio::time::sleep(std::time::Duration::from_secs(2u64 * attempt as u64))
                        .await;
                }
            }
        }
    }
    Err(format!("重试 {REMOTE_FILE_RETRIES} 次仍失败: {last_err}"))
}

/// 远程仓库任务主 runner：**文件级并行**（`buffer_unordered(concurrency)`；
/// 并发数 env `NEXOS_MODELHUB_DL_CONCURRENCY`，缺省 3、上限 8，=1 即与旧
/// 顺序实现行为一致——按清单顺序逐文件）。每文件重试 / `.part` 续传 / 原子
/// rename 语义不变（见 [`download_remote_file`]）。**失败隔离**：某文件重试
/// 耗尽不拖垮其他文件——已完成文件照常保留落盘，收摊时整体置 failed 并在
/// error 里列出失败文件；全成功才走终态校验 → completed。
async fn run_remote_download(
    kind: RemoteRepoKind,
    repo_id: String,
    plan_files: Vec<ManifestFile>,
    local_dir: String,
    tasks: Arc<Mutex<Vec<RemoteRepoTask>>>,
    task_id: String,
    concurrency: usize,
) {
    use futures::StreamExt;
    let dir = std::path::Path::new(&local_dir).to_path_buf();
    let concurrency = concurrency.clamp(1, REMOTE_DL_CONCURRENCY_MAX);
    let outcomes: Vec<(ManifestFile, Result<(), String>)> =
        futures::stream::iter(plan_files.into_iter().map(|mf| {
            let repo_id = repo_id.clone();
            let dir = dir.clone();
            let tasks = tasks.clone();
            let task_id = task_id.clone();
            async move {
                let outcome =
                    run_remote_file_with_retries(kind, &repo_id, &dir, &mf, &tasks, &task_id)
                        .await;
                (mf, outcome)
            }
        }))
        .buffer_unordered(concurrency)
        .collect()
        .await;
    // 收摊：取消 → failed(用户取消)；有失败 → failed + 失败文件清单（成功件保留）；
    // 全成功 → 逐文件终态校验（大小对拍）→ completed/failed
    let aborted = remote_task_aborted(&tasks, &task_id);
    let mut ok_files: Vec<&ManifestFile> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    for (mf, res) in &outcomes {
        match res {
            Ok(()) => ok_files.push(mf),
            Err(e) if e.contains("取消") => {}
            Err(e) => failures.push(format!("文件 {}: {e}", mf.name)),
        }
    }
    update_remote_task(&tasks, &task_id, |t| {
        if aborted {
            t.status = "failed".into();
            t.error = Some("用户取消".into());
            return;
        }
        if !failures.is_empty() {
            t.status = "failed".into();
            t.error = Some(format!(
                "{} 个文件重试 {REMOTE_FILE_RETRIES} 次仍失败: {}",
                failures.len(),
                failures.join("; ")
            ));
            return;
        }
        for mf in &ok_files {
            let got = dir.join(&mf.name).metadata().map(|m| m.len()).unwrap_or(0);
            if got != mf.size_bytes {
                t.status = "failed".into();
                t.error = Some(format!(
                    "完成校验失败 {}: 期望 {} 实得 {got}",
                    mf.name, mf.size_bytes
                ));
                return;
            }
        }
        t.status = "completed".into();
        t.files_done = outcomes.len();
        t.bytes_done = t.total_bytes;
    });
}

/// 推一条远程任务文件级简报（保留最近 5 条，最新在尾；与 lobby_multi 的
/// `push_file_progress` 同语义，作用于 RemoteRepoTask）。
fn push_file_progress_remote(t: &mut RemoteRepoTask, fp: FileProgress) {
    t.recent_files.push(fp);
    let overflow = t.recent_files.len().saturating_sub(5);
    for _ in 0..overflow {
        t.recent_files.remove(0);
    }
}

// ----------------------------------------------------------------------------
// E 面：Spark 专区（SM120/NVFP4 精选策展 + 逐条实时可用性）
// ----------------------------------------------------------------------------

/// Spark 专区策展条目（内置静态表与 env 覆盖文件同构）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SparkZoneEntry {
    /// 仓库 id（`org/model`，须过 [`validate_repo_id`]）。
    pub repo: String,
    /// 发布组织（= repo 首段；冗余存储便于前端直读展示）。
    pub org: String,
    /// 量化格式（专区策展恒 "NVFP4"；字段化便于 env 扩展未来格式）。
    pub quant: String,
    /// 参数量标签（如 "27B" / "35B-A3B"）。
    pub params: String,
    /// 一句话简述（展示用）。
    pub note: String,
}

/// 专区条目单源可用性（探测产物；失败只标 unavailable，**不剔除条目**——诚实降级）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SparkZoneSourceStatus {
    /// 源 slug（`modelscope` / `hf`）。
    pub kind: String,
    pub available: bool,
    /// 探测到的文件总数（不可用 / 未探测为 null）。
    pub file_count: Option<usize>,
    /// 探测到的全量大小（不可用 / 未探测为 null）。
    pub total_size_bytes: Option<u64>,
    pub error: Option<String>,
}

/// `GET /api/v1/models/spark-zone` 条目（策展条目 + 两源探测态 + 本地在库标记）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SparkZoneItem {
    pub repo: String,
    pub org: String,
    pub quant: String,
    pub params: String,
    pub note: String,
    /// 本地模型库已有同名目录（repo 末段）——已下载不再重复拉。
    pub downloaded: bool,
    /// 恒两元素：`[modelscope, hf]`（探测顺序固定，前端徽章按 kind 取）。
    pub sources: Vec<SparkZoneSourceStatus>,
}

/// `GET /api/v1/models/spark-zone` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SparkZoneResponse {
    pub ok: bool,
    /// 本次是否做了实时探测（`?probe=0` → false，sources 全为"未探测"态）。
    pub probed: bool,
    /// 清单来源：`builtin`（内置表）/ `env`（NEXOS_SPARK_ZONE_FILE 生效）。
    pub origin: String,
    pub entries: Vec<SparkZoneItem>,
}

/// 专区单源探测超时（秒）——策展清单页要快，宁可标 unavailable 不让页面卡 30s。
const SPARK_ZONE_PROBE_TIMEOUT_SECS: u64 = 3;

/// 内置策展清单（实测收录 2026-09-02，探测记录见 docs/MODELHUB_LOBBY.md §5E.2）：
/// 全部经 ModelScope files API / HF 镜像 tree API 实探确认存在；nv-community 系列
/// 为魔搭独占（HF 镜像 401），unsloth / RedHatAI 双源可用。体积以"能塞进 Spark
/// 128GB 统一内存并留 KV cache 余量"为准（≤25GiB 级；433GiB 的 GLM-5.2-NVFP4
/// 等超大体量模型不收录，见文档边界）。
#[must_use]
pub fn builtin_spark_zone_entries() -> Vec<SparkZoneEntry> {
    [
        ("nv-community/Qwen3.6-27B-NVFP4", "27B", "Qwen3.6 27B NVFP4（ModelOpt 量化）——27B 级均衡之选，Spark 上留足 KV cache 空间"),
        ("unsloth/Qwen3.8-27B-NVFP4", "27B", "Qwen3.8 27B NVFP4（compressed-tensors 量化）——双源可下，最新一代 27B"),
        ("unsloth/Qwen3.6-27B-NVFP4", "27B", "Qwen3.6 27B NVFP4 unsloth 复刻——与 nv-community 版同源模型的另一发布渠道"),
        ("nv-community/Qwen3.6-35B-A3B-NVFP4", "35B-A3B", "Qwen3.6 35B MoE（激活 3B）NVFP4——MoE 低激活高吞吐，Spark 单机长文本优选"),
        ("unsloth/Qwen3.6-35B-A3B-NVFP4", "35B-A3B", "Qwen3.6 35B-A3B NVFP4 unsloth 复刻——双源可下"),
        ("RedHatAI/Qwen3.6-35B-A3B-NVFP4", "35B-A3B", "Qwen3.6 35B-A3B NVFP4 RedHatAI 发行——NeMo/ModelOpt 生态量化，双源可下"),
        ("nv-community/NVIDIA-Nemotron-3.5-Lightning-30B-A3B-NVFP4", "30B-A3B", "NVIDIA Nemotron 3.5 Lightning 30B-A3B NVFP4——NVIDIA 官方系 MoE，SM120 原生适配"),
    ]
    .into_iter()
    .map(|(repo, params, note)| SparkZoneEntry {
        org: repo.split('/').next().unwrap_or_default().to_string(),
        repo: repo.to_string(),
        quant: "NVFP4".into(),
        params: params.to_string(),
        note: note.to_string(),
    })
    .collect()
}

/// 内置表 + env 表合并（纯函数）：env 同 repo 条目**整条覆盖**内置，新 repo 追加；
/// 顺序 = 内置序在前、env 新条目按文件原序在后（去重以 repo 为键）。
#[must_use]
pub fn merge_spark_zone_entries(
    builtin: Vec<SparkZoneEntry>,
    env: Vec<SparkZoneEntry>,
) -> Vec<SparkZoneEntry> {
    let mut out = builtin;
    for e in env {
        match out.iter_mut().find(|x| x.repo == e.repo) {
            Some(slot) => *slot = e,
            None => out.push(e),
        }
    }
    out
}

/// env `NEXOS_SPARK_ZONE_FILE` 清单文件解析（纯函数，测试直测）。两种形态：
/// - JSON 数组 `[entry, …]` → **合并**语义（同 repo 覆盖内置，新条目追加）；
/// - JSON 对象 `{"replace": [entry, …]}` → **整体替换**（运维可删内置条目）。
///
/// 返回 (entries, replace)；解析失败 Err（caller 降级内置表）。
fn parse_spark_zone_env(body: &str) -> Result<(Vec<SparkZoneEntry>, bool), String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("JSON 解析失败: {e}"))?;
    match &v {
        serde_json::Value::Array(_) => {
            let entries = serde_json::from_value(v).map_err(|e| format!("条目反序列化失败: {e}"))?;
            Ok((entries, false))
        }
        serde_json::Value::Object(_) => {
            let arr = v
                .get("replace")
                .ok_or("对象形态须含 replace 数组（数组形态=与内置表合并）")?
                .clone();
            let entries: Vec<SparkZoneEntry> =
                serde_json::from_value(arr).map_err(|e| format!("replace 反序列化失败: {e}"))?;
            Ok((entries, true))
        }
        _ => Err("顶层须为 JSON 数组或 {\"replace\": […]} 对象".into()),
    }
}

/// 当前生效的策展清单：env `NEXOS_SPARK_ZONE_FILE` 可读可解析 → 合并/替换内置表；
/// 未设置 / 读失败 / 解析失败 → 内置表（eprintln `[modelhub]` 前缀记原因，降级不 panic）。
/// 返回 (entries, origin)，origin ∈ {"builtin", "env"}。
pub fn spark_zone_entries() -> (Vec<SparkZoneEntry>, String) {
    let builtin = builtin_spark_zone_entries();
    let path = match std::env::var("NEXOS_SPARK_ZONE_FILE") {
        Ok(p) if !p.trim().is_empty() => p.trim().to_string(),
        _ => return (builtin, "builtin".into()),
    };
    let body = match std::fs::read_to_string(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[modelhub] spark-zone: 读取 NEXOS_SPARK_ZONE_FILE={path} 失败（{e}），回退内置表");
            return (builtin, "builtin".into());
        }
    };
    match parse_spark_zone_env(&body) {
        Err(e) => {
            eprintln!("[modelhub] spark-zone: 解析 {path} 失败（{e}），回退内置表");
            (builtin, "builtin".into())
        }
        Ok((env_entries, replace)) => {
            // repo 形态不合法的条目剔除并记日志（诚实降级，不整表报废）
            let (valid, bad): (Vec<_>, Vec<_>) = env_entries
                .into_iter()
                .partition(|e| validate_repo_id(&e.repo).is_ok());
            for b in &bad {
                eprintln!("[modelhub] spark-zone: 剔除非法 repo 条目 {}（须为 org/model）", b.repo);
            }
            let out = if replace {
                valid
            } else {
                merge_spark_zone_entries(builtin, valid)
            };
            (out, "env".into())
        }
    }
}

/// 专区单源轻量探测（3s 超时；存在性 + 件数/总大小，复用 D 面解析器——不拉文件
/// 正文）。任何失败 → `available=false` + error 说明，绝不 Err 上抛（条目不剔除）。
async fn probe_spark_source(kind: RemoteRepoKind, repo_id: &str) -> SparkZoneSourceStatus {
    let unavailable = |err: String| SparkZoneSourceStatus {
        kind: kind.slug().to_string(),
        available: false,
        file_count: None,
        total_size_bytes: None,
        error: Some(err),
    };
    let url = kind.files_url(repo_id);
    let mut req = MULTI_HTTP
        .get(&url)
        .timeout(std::time::Duration::from_secs(SPARK_ZONE_PROBE_TIMEOUT_SECS))
        .header("Accept", "application/json");
    if let Some(t) = kind.token() {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => return unavailable(format!("探测请求失败: {e}")),
    };
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return unavailable(format!("仓库不存在（或无权访问）: {repo_id}"));
    }
    if !resp.status().is_success() {
        return unavailable(format!("探测返回 {}", resp.status()));
    }
    let body = match resp.json::<serde_json::Value>().await {
        Ok(b) => b,
        Err(e) => return unavailable(format!("清单解析失败: {e}")),
    };
    let files = match kind {
        RemoteRepoKind::Modelscope => parse_modelscope_files(&body),
        RemoteRepoKind::HfMirror => parse_hf_tree(&body),
    };
    match files {
        Ok(files) => SparkZoneSourceStatus {
            kind: kind.slug().to_string(),
            available: true,
            file_count: Some(files.len()),
            total_size_bytes: Some(files.iter().map(|f| f.size_bytes).sum()),
            error: None,
        },
        Err(e) => unavailable(e),
    }
}

// ----------------------------------------------------------------------------
// ModelHubRouteHandler
// ----------------------------------------------------------------------------

/// 模型仓库 + 模型大厅路由处理器——本地模型库 + modelscope 下载 + 大厅发布
/// + 多源下载 + 在线仓库源（HTTP 边界适配）。
pub struct ModelHubRouteHandler {
    downloads: Mutex<Vec<DownloadTask>>,
    counter: Mutex<u64>,
    /// 大厅发布索引（SQLite model_lobby，Arc 供多源 runner 归因 download_count）。
    lobby_db: Arc<Mutex<Connection>>,
    /// 多源下载任务（内存态——与既有 modelscope 任务一致，重启即失，报告说明）。
    multi: Arc<Mutex<Vec<LobbyMultiTask>>>,
    /// 在线仓库源下载任务（D 面，内存态同上；重启即失）。
    remote: Arc<Mutex<Vec<RemoteRepoTask>>>,
    /// 系统 admin token（构造时定格 env；share 端点 token= 与发布 source_url 用）。
    admin_token: Option<String>,
}

impl ModelHubRouteHandler {
    /// 构造 handler（空任务列表 + 文件 SQLite 大厅库）。
    #[must_use]
    pub fn new() -> Self {
        Self::open_lobby(&default_lobby_db_path())
    }

    /// 用空列表构造（测试注入：内存大厅库，任务/发布互不落盘）。
    #[must_use]
    pub fn with_empty() -> Self {
        Self::open_lobby(":memory:").with_admin_token_default()
    }

    fn open_lobby(db_path: &str) -> Self {
        let conn = open_lobby_db(db_path).unwrap_or_else(|e| {
            eprintln!("model_hub: 打开大厅 SQLite {db_path} 失败（{e}），降级到内存库");
            Connection::open_in_memory().expect("内存库必成功")
        });
        Self {
            downloads: Mutex::new(vec![]),
            counter: Mutex::new(100),
            lobby_db: Arc::new(Mutex::new(conn)),
            multi: Arc::new(Mutex::new(vec![])),
            remote: Arc::new(Mutex::new(vec![])),
            admin_token: admin_token_from_env(),
        }
    }

    /// 链式注入系统 admin token（测试用：绕开 env 并行竞态；生产经
    /// [`admin_token_from_env`] 构造时定格）。
    #[must_use]
    pub fn with_admin_token(mut self, token: &str) -> Self {
        self.admin_token = Some(token.to_string());
        self
    }

    /// env 里已有 admin token 时补默认（with_empty 测试构造用）。
    #[must_use]
    fn with_admin_token_default(mut self) -> Self {
        if self.admin_token.is_none() {
            self.admin_token = admin_token_from_env();
        }
        self
    }

    /// 大厅行快照（测试/诊断）。
    #[must_use]
    pub fn lobby_rows_snapshot(&self) -> Vec<LobbyRow> {
        match self.lobby_db.lock() {
            Ok(conn) => load_lobby_rows(&conn),
            Err(_) => Vec::new(),
        }
    }

    /// 多源任务快照（测试/诊断）。
    #[must_use]
    pub fn multi_tasks_snapshot(&self) -> Vec<LobbyMultiTask> {
        self.multi.lock().expect("multi tasks poisoned").clone()
    }

    /// 在线仓库源任务快照（测试/诊断）。
    #[must_use]
    pub fn remote_tasks_snapshot(&self) -> Vec<RemoteRepoTask> {
        self.remote.lock().expect("remote tasks poisoned").clone()
    }

    /// 当前全量下载任务快照。
    #[must_use]
    pub fn downloads_snapshot(&self) -> Vec<DownloadTask> {
        self.downloads.lock().expect("downloads poisoned").clone()
    }

    fn next_id(&self, prefix: &str) -> String {
        let mut c = self.counter.lock().expect("counter poisoned");
        *c += 1;
        format!("{prefix}-{}", *c)
    }

    /// 真实 spawn modelscope download 子进程，成功返回 pid。
    ///
    /// 不 await 完成（后台跑）。modelscope 不存在 / spawn 失败返回 Err（caller 降级为 failed）。
    async fn spawn_modelscope(model_id: &str, local_dir: &str) -> Result<u32, String> {
        // modelscope CLI 路径：优先 ~/.local/bin/modelscope，回退 PATH 中的 modelscope
        let home = std::env::var("HOME").unwrap_or_default();
        let local_bin = format!("{home}/.local/bin/modelscope");
        let bin = if std::path::Path::new(&local_bin).exists() {
            local_bin
        } else {
            "modelscope".to_string()
        };
        let args = build_download_cmd(model_id, local_dir);
        let mut cmd = tokio::process::Command::new(&bin);
        cmd.args(&args);
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        cmd.stdin(std::process::Stdio::null());
        match cmd.spawn() {
            Ok(child) => {
                let pid = child
                    .id()
                    .ok_or_else(|| "modelscope spawn 后无 pid".to_string())?;
                drop(child); // 由 OS 收养，后台继续跑
                Ok(pid)
            }
            Err(e) => Err(format!("modelscope 命令未找到或启动失败: {e}")),
        }
    }

    /// 刷新单个下载任务的进度（重扫 local_dir + 探测 pid）。
    ///
    /// 在调用方持锁前完成 spawn_blocking，避免 await 期间持锁。
    async fn refresh_task(&self, task: &mut DownloadTask) {
        // 已完成/失败的不刷新
        if task.status == "completed" || task.status == "failed" {
            return;
        }
        let local_dir = task.local_dir.clone();
        // 重扫目录大小（spawn_blocking）
        let (current_size, _) = tokio::task::spawn_blocking(move || {
            dir_size_and_count(std::path::Path::new(&local_dir))
        })
        .await
        .unwrap_or((0, 0));
        task.current_size_bytes = current_size;
        // 进度估算
        if task.estimated_size_bytes > 0 {
            let pct = (current_size as f64 / task.estimated_size_bytes as f64 * 100.0) as u8;
            task.progress_pct = pct.min(100);
        }
        // pid 探测：判断完成/失败
        if let Some(pid) = task.pid {
            if !pid_alive(pid) {
                // 进程已退出：有 config.json → completed，否则 failed
                let done = std::path::Path::new(&task.local_dir)
                    .join("config.json")
                    .exists();
                if done {
                    task.status = "completed".into();
                    task.progress_pct = 100;
                    task.pid = None;
                } else {
                    task.status = "failed".into();
                    task.pid = None;
                    task.error = Some(
                        "modelscope 进程已退出但未生成 config.json（可能下载失败或被中断）".into(),
                    );
                }
            } else {
                task.status = "downloading".into();
            }
        } else {
            // 无 pid（spawn 失败）：保持 failed
        }
    }

    // ------------------------------------------------------------------
    // B 面：大厅发布 + 文件共享
    // ------------------------------------------------------------------

    /// `POST /api/v1/models/lobby/publish` 处理：本地存在才可发布；source_url
    /// 自动生成本机地址 + token=admin token；同 (name, sharer) 重复发布=刷新。
    async fn handle_lobby_publish(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let body: PublishBody = serde_json::from_value(req.body)
            .map_err(|e| ApiGatewayError::Internal(format!("解析发布请求体失败: {e}")))?;
        if let Err(e) = validate_model_name(body.name.trim()) {
            return Ok(error_response(400, &e));
        }
        let name = body.name.trim().to_string();
        // 本地必须存在且像个模型（config.json 或 *.safetensors）
        let root = models_root();
        let local_dir = std::path::Path::new(&root).join(&name);
        let valid = tokio::task::spawn_blocking({
            let p = local_dir.clone();
            move || p.is_dir() && is_valid_model_dir(&p)
        })
        .await
        .unwrap_or(false);
        if !valid {
            return Ok(error_response(
                404,
                &format!("本地模型不存在或不完整（缺 config.json/*.safetensors）: {name}"),
            ));
        }
        // 扫权重详情拿 arch/size/file_count（失败降级空值，不阻塞发布）
        let detail = {
            let r = root.clone();
            let n = name.clone();
            tokio::task::spawn_blocking(move || scan_model_weight_detail(&r, &n))
                .await
                .unwrap_or_else(|_| Err("join 失败".into()))
                .ok()
        };
        let (size_bytes, file_count, arch) = detail
            .map(|d| {
                (
                    d.total_size_bytes,
                    d.file_count as u32,
                    d.config.map(|c| c.arch).unwrap_or_default(),
                )
            })
            .unwrap_or((0, 0, String::new()));
        // sharer：body > 认证身份 > "admin"
        let sharer = body
            .sharer
            .clone()
            .unwrap_or_else(|| {
                req.auth
                    .as_ref()
                    .map(|p| p.user.name.clone())
                    .unwrap_or_else(|| "admin".into())
            })
            .trim()
            .to_string();
        let sharer = sanitize_sharer(&sharer);
        // source_url：本机地址 + admin token（凭据随 URL 分发——风险见文档）
        let token = self.admin_token.clone().unwrap_or_default();
        let source_url = build_source_url(&share_host(), &share_port(), &name, &token);
        let row = LobbyRow {
            id: lobby_id(&name, &sharer),
            display_name: body.display_name.clone().unwrap_or_default(),
            name,
            description: body.description.clone().unwrap_or_default(),
            tags: body.tags.clone().unwrap_or_default(),
            arch,
            size_bytes,
            file_count,
            sharer: sharer.clone(),
            source_url: source_url.clone(),
            created_at: now_iso(),
            download_count: 0,
        };
        {
            let conn = self.lobby_db.lock().expect("lobby db poisoned");
            upsert_lobby_entry(&conn, &row, &token)
                .map_err(|e| ApiGatewayError::Internal(format!("大厅发布落库失败: {e}")))?;
        }
        let body = serde_json::json!({
            "ok": true,
            "id": row.id,
            "name": row.name,
            "display_name": if row.display_name.is_empty() { row.name.clone() } else { row.display_name.clone() },
            "description": row.description,
            "tags": row.tags,
            "arch": row.arch,
            "size_bytes": row.size_bytes,
            "file_count": row.file_count,
            "sharer": row.sharer,
            "source_url": row.source_url,
            "share_token": token,
            "created_at": row.created_at,
        });
        Ok(ApiResponse {
            status: 201,
            body,
            headers: serde_json::json!({}),
        })
    }

    /// `GET /api/v1/models/share/:name/*` 处理：token==admin + 路径白名单 +
    /// offset/length 分段回传 base64 内容（目录 400 / 越界 400 / 不存在 404）。
    async fn handle_share_file(
        &self,
        name: &str,
        rest_segs: &[&str],
        req: &ApiRequest,
    ) -> Result<ApiResponse, ApiGatewayError> {
        // token 校验（query 参数携带——远端拉取方不走网关 Bearer 鉴权）
        let token = query_param(&req.path, "token").unwrap_or_default();
        let expected = self.admin_token.clone().unwrap_or_default();
        if expected.is_empty() || token != expected {
            return Ok(error_response(
                401,
                "token 无效或未配置（须为系统 admin token）",
            ));
        }
        if let Err(e) = validate_model_name(name) {
            return Ok(error_response(400, &e));
        }
        // 路径白名单：percent-decode 后逐段校验（拒绝 ../穿越）
        let decoded: Vec<String> = rest_segs.iter().map(|s| url_decode(s)).collect();
        let seg_refs: Vec<&str> = decoded.iter().map(|s| s.as_str()).collect();
        if let Err(e) = validate_share_rel_path(&seg_refs) {
            return Ok(error_response(400, &e));
        }
        let rel = seg_refs.join("/");
        let root = models_root();
        let model_dir = std::path::Path::new(&root).join(name);
        let target = model_dir.join(&rel);
        // 双保险：canonicalize 后必须仍在模型目录内（防符号链接中途逃逸）
        let meta = match target.metadata() {
            Ok(m) => m,
            Err(_) => return Ok(error_response(404, &format!("文件不存在: {rel}"))),
        };
        if !meta.is_file() {
            return Ok(error_response(
                400,
                "目标是目录（不可整体下载），请指定具体文件路径",
            ));
        }
        if !path_inside_root(&model_dir, &target) {
            return Ok(error_response(400, "路径越出模型目录（拒绝）"));
        }
        // offset/length 分段（缺省=整文件；服务端上界 64 MiB 防内存放大）
        let total = meta.len();
        let offset: u64 = match query_param(&req.path, "offset") {
            Some(s) => match s.parse() {
                Ok(v) => v,
                Err(_) => return Ok(error_response(400, "offset 须为非负整数")),
            },
            None => 0,
        };
        if offset > total {
            return Ok(error_response(
                400,
                &format!("offset {offset} 超出文件大小 {total}"),
            ));
        }
        let length: u64 = match query_param(&req.path, "length") {
            Some(s) => match s.parse() {
                Ok(v) => v,
                Err(_) => return Ok(error_response(400, "length 须为正整数")),
            },
            None => total - offset,
        };
        if length > SHARE_MAX_CHUNK_BYTES {
            return Ok(error_response(
                400,
                &format!("length 超过单次上限 {SHARE_MAX_CHUNK_BYTES} 字节，请分段请求"),
            ));
        }
        let length = length.min(total - offset);
        // 读段（spawn_blocking，spawn_blocking 内同步 IO）
        let path = target.clone();
        let read = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
            use std::io::{Read, Seek, SeekFrom};
            let mut f = std::fs::File::open(&path).map_err(|e| format!("打开失败: {e}"))?;
            f.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
            let mut buf = vec![0u8; length as usize];
            f.read_exact(&mut buf)
                .map_err(|e| format!("读取失败: {e}"))?;
            Ok(buf)
        })
        .await
        .map_err(|e| ApiGatewayError::Internal(format!("读取分享文件任务 join 失败: {e}")))?
        .map_err(ApiGatewayError::Internal)?;
        let eof = offset + length >= total;
        let resp = ShareFileResponse {
            ok: true,
            name: name.to_string(),
            path: rel,
            offset,
            length,
            total_size: total,
            eof,
            content_base64: base64::engine::general_purpose::STANDARD.encode(&read),
        };
        Ok(ok_json(to_value(&resp)?))
    }

    // ------------------------------------------------------------------
    // C 面：lobby_multi 多源任务创建
    // ------------------------------------------------------------------

    /// 创建 lobby_multi 任务：校验 → 同步拉清单（全源失败 502）→ 轮转分配 →
    /// 后台 runner 并行下载。返回 201 + 任务快照。
    async fn create_multi_task(
        &self,
        sources: &[String],
        name: Option<&str>,
    ) -> Result<ApiResponse, ApiGatewayError> {
        let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
            return Ok(error_response(400, "多源任务必须提供 name（模型名）"));
        };
        if let Err(e) = validate_model_name(name) {
            return Ok(error_response(400, &e));
        }
        if sources.iter().any(|s| split_source_url(s).is_none()) {
            return Ok(error_response(
                400,
                "sources 含非法 URL（须 http(s)://host[:port]…）",
            ));
        }
        // 同步拉清单：从首个可达源（全部失败 → 502，任务不入列）
        let (files, _first_ok) = match fetch_manifest_from_sources(sources, name).await {
            Ok(x) => x,
            Err(e) => {
                return Ok(ApiResponse {
                    status: 502,
                    body: serde_json::json!({"error": e}),
                    headers: serde_json::json!({}),
                })
            }
        };
        let total_bytes: u64 = files.iter().map(|f| f.size_bytes).sum();
        let assignments = assign_files_round_robin(files.len(), sources.len());
        let root = models_root();
        let local_dir = format!("{root}/{name}");
        let root_clone = root.clone();
        let _ = tokio::task::spawn_blocking(move || std::fs::create_dir_all(&root_clone)).await;
        // 已有 .part 的续传起点计入 bytes_done（进度不从 0 假起）
        let initial_bytes: u64 = files
            .iter()
            .map(|f| {
                std::path::Path::new(&local_dir)
                    .join(format!("{}.part", f.name))
                    .metadata()
                    .map(|m| m.len().min(f.size_bytes))
                    .unwrap_or(0)
            })
            .sum();
        let task = LobbyMultiTask {
            id: self.next_id("mdlm"),
            r#type: "lobby_multi".into(),
            name: name.to_string(),
            local_dir: local_dir.clone(),
            sources: sources.to_vec(),
            status: "downloading".into(),
            files_total: files.len(),
            files_done: 0,
            bytes_done: initial_bytes,
            total_bytes,
            active_sources: sources.to_vec(),
            recent_files: Vec::new(),
            cancel_requested: false,
            error: None,
            created_at: now_iso(),
        };
        let resp = to_value(&task)?;
        let plan = MultiPlan {
            name: name.to_string(),
            local_dir,
            sources: sources.to_vec(),
            files,
            assignments,
        };
        self.multi.lock().expect("multi tasks poisoned").push(task);
        tokio::spawn(run_multi_download(
            plan,
            self.multi.clone(),
            self.lobby_db.clone(),
            resp["id"].as_str().unwrap_or_default().to_string(),
        ));
        Ok(ApiResponse {
            status: 201,
            body: resp,
            headers: serde_json::json!({}),
        })
    }

    // ------------------------------------------------------------------
    // D 面：在线仓库源（ModelScope / HF 镜像）探测 + 下载任务创建
    // ------------------------------------------------------------------

    /// 创建 remote_repo 任务：校验 kind/repo_id/文件清单交集 → 同步探测（失败 502）
    /// → 后台 runner 逐文件下载。返回 201 + 任务快照。
    async fn create_remote_task(
        &self,
        kind: RemoteRepoKind,
        repo_id: &str,
        name: Option<&str>,
        files: Option<&[String]>,
    ) -> Result<ApiResponse, ApiGatewayError> {
        // 本地目录名：body.name > repo_id 末段（须过模型名校验——与本地库同规则）
        let (org, model) = match validate_repo_id(repo_id) {
            Ok(x) => x,
            Err(e) => return Ok(error_response(400, &e)),
        };
        let repo_id = format!("{org}/{model}");
        let name = name
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .unwrap_or(&model);
        if let Err(e) = validate_model_name(name) {
            return Ok(error_response(400, &e));
        }
        // 同步探测（清单 = 下载计划 + 选中文件校验基准；失败 502 任务不入列）
        let probe = match probe_remote_repo(kind, &repo_id).await {
            Ok(p) => p,
            Err(e) => {
                return Ok(ApiResponse {
                    status: 502,
                    body: serde_json::json!({"error": e}),
                    headers: serde_json::json!({}),
                })
            }
        };
        // 选中文件与清单求交（保持清单顺序——稳定下载计划）；未知路径 400
        let plan_files: Vec<ManifestFile> = match files {
            None => probe
                .files
                .iter()
                .map(|f| ManifestFile {
                    name: f.name.clone(),
                    size_bytes: f.size_bytes,
                })
                .collect(),
            Some([]) => {
                return Ok(error_response(400, "files 不可为空数组（缺省=全部文件）"));
            }
            Some(sel) => {
                let sel_set: std::collections::HashSet<&str> =
                    sel.iter().map(|s| s.trim()).collect();
                let unknown: Vec<&str> = sel
                    .iter()
                    .map(|s| s.trim())
                    .filter(|s| !probe.files.iter().any(|f| f.name == *s))
                    .collect();
                if !unknown.is_empty() {
                    return Ok(error_response(
                        400,
                        &format!(
                            "files 含清单外路径: {}",
                            unknown.join(", ")
                        ),
                    ));
                }
                probe
                    .files
                    .iter()
                    .filter(|f| sel_set.contains(f.name.as_str()))
                    .map(|f| ManifestFile {
                        name: f.name.clone(),
                        size_bytes: f.size_bytes,
                    })
                    .collect()
            }
        };
        if plan_files.is_empty() {
            return Ok(error_response(400, "选中文件为空"));
        }
        let total_bytes: u64 = plan_files.iter().map(|f| f.size_bytes).sum();
        let root = models_root();
        let local_dir = format!("{root}/{name}");
        let root_clone = root.clone();
        let _ = tokio::task::spawn_blocking(move || std::fs::create_dir_all(&root_clone)).await;
        // 已有 .part 续传起点计入 bytes_done（进度不从 0 假起——与 lobby_multi 同口径）
        let initial_bytes: u64 = plan_files
            .iter()
            .map(|f| {
                std::path::Path::new(&local_dir)
                    .join(format!("{}.part", f.name))
                    .metadata()
                    .map(|m| m.len().min(f.size_bytes))
                    .unwrap_or(0)
            })
            .sum();
        let task = RemoteRepoTask {
            id: self.next_id("mdlrm"),
            r#type: "remote_repo".into(),
            kind: kind.slug().to_string(),
            repo_id: probe.repo_id.clone(),
            name: name.to_string(),
            local_dir: local_dir.clone(),
            status: "downloading".into(),
            files_total: plan_files.len(),
            files_done: 0,
            bytes_done: initial_bytes,
            total_bytes,
            recent_files: Vec::new(),
            cancel_requested: false,
            error: None,
            created_at: now_iso(),
        };
        let resp = to_value(&task)?;
        let repo = probe.repo_id.clone();
        self.remote
            .lock()
            .expect("remote tasks poisoned")
            .push(task);
        let tasks_arc = self.remote.clone();
        let id = resp["id"].as_str().unwrap_or_default().to_string();
        // 并发数在 spawn 前定格（runner 内不读 env——测试 ScopedEnvs 窗口更稳）
        let concurrency =
            remote_dl_concurrency(&std::env::var("NEXOS_MODELHUB_DL_CONCURRENCY").unwrap_or_default());
        tokio::spawn(run_remote_download(
            kind,
            repo,
            plan_files,
            local_dir,
            tasks_arc,
            id,
            concurrency,
        ));
        Ok(ApiResponse {
            status: 201,
            body: resp,
            headers: serde_json::json!({}),
        })
    }

    // ------------------------------------------------------------------
    // E 面：Spark 专区（SM120/NVFP4 策展 + 逐条实时可用性）
    // ------------------------------------------------------------------

    /// `GET /api/v1/models/spark-zone`：策展清单 + 两源实时可用性。
    ///
    /// 探测全并行（条目 × 2 源 join_all，单源 3s 超时）；`probe=false`
    /// （`?probe=0`）跳过探测，sources 为"未探测"占位。失败源标 unavailable、
    /// 条目不剔除；`downloaded` 按本地库同名目录（repo 末段）存在判定。
    async fn handle_spark_zone(&self, probe: bool) -> Result<ApiResponse, ApiGatewayError> {
        let (entries, origin) = spark_zone_entries();
        let root = models_root();
        let to_item = |e: SparkZoneEntry, sources: Vec<SparkZoneSourceStatus>| {
            let downloaded = std::path::Path::new(&root)
                .join(model_dir_name(&e.repo))
                .is_dir();
            SparkZoneItem {
                repo: e.repo,
                org: e.org,
                quant: e.quant,
                params: e.params,
                note: e.note,
                downloaded,
                sources,
            }
        };
        let items = if probe {
            let probes = entries.iter().map(|e| {
                let repo = e.repo.clone();
                async move {
                    futures::future::join(
                        probe_spark_source(RemoteRepoKind::Modelscope, &repo),
                        probe_spark_source(RemoteRepoKind::HfMirror, &repo),
                    )
                    .await
                }
            });
            let statuses = futures::future::join_all(probes).await;
            entries
                .into_iter()
                .zip(statuses)
                .map(|(e, (ms, hf))| to_item(e, vec![ms, hf]))
                .collect()
        } else {
            let unknown = |kind: &str| SparkZoneSourceStatus {
                kind: kind.to_string(),
                available: false,
                file_count: None,
                total_size_bytes: None,
                error: Some("未探测（?probe=0 跳过）".into()),
            };
            entries
                .into_iter()
                .map(|e| to_item(e, vec![unknown("modelscope"), unknown("hf")]))
                .collect()
        };
        Ok(ok_json(to_value(&SparkZoneResponse {
            ok: true,
            probed: probe,
            origin,
            entries: items,
        })?))
    }
}

impl Default for ModelHubRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RouteHandler for ModelHubRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            // 本地模型库（3 条）
            spec(HttpMethod::Get, "/api/v1/models/local", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/models/local/:id", false, vec![]),
            spec(
                HttpMethod::Delete,
                "/api/v1/models/local/:id",
                true,
                vec!["admin".into()],
            ),
            // A 面权重详细管理（3 条）
            spec(
                HttpMethod::Get,
                "/api/v1/models/:name/detail",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/models/:name",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/models/import",
                true,
                vec!["admin".into()],
            ),
            // 下载任务（4 条；POST body 含 sources 时创建 lobby_multi 任务）
            spec(HttpMethod::Get, "/api/v1/models/downloads", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/models/downloads",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/models/downloads/:id",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/models/downloads/:id",
                false,
                vec![],
            ),
            // 推荐 + 统计（2 条）
            spec(HttpMethod::Get, "/api/v1/models/recommended", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/models/stats", false, vec![]),
            // B 面模型大厅（5 条；下架鉴权在 handler 内做 admin-or-sharer 细判）
            spec(
                HttpMethod::Post,
                "/api/v1/models/lobby/publish",
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Get, "/api/v1/models/lobby", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/models/lobby/:name", false, vec![]),
            spec(
                HttpMethod::Delete,
                "/api/v1/models/lobby/:name",
                true,
                vec![],
            ),
            // C 面文件共享端点（token query 自鉴权，故路由层不要求 auth）
            spec(
                HttpMethod::Get,
                "/api/v1/models/share/:name/*",
                false,
                vec![],
            ),
            // D 面在线仓库源（2 条；探测=公开读，创建下载=admin 写）
            spec(
                HttpMethod::Get,
                "/api/v1/models/remote/:kind/:org/:model",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/models/remote/downloads",
                true,
                vec!["admin".into()],
            ),
            // E 面 Spark 专区（1 条；公开读，?probe=0 跳过实时探测）
            spec(HttpMethod::Get, "/api/v1/models/spark-zone", false, vec![]),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            // ============ 本地模型库 ============

            // —— GET /api/v1/models/local —— 列本地模型（自家库 + HF 缓存合并）
            (HttpMethod::Get, ["api", "v1", "models", "local"]) => {
                let root = models_root();
                let models =
                    tokio::task::spawn_blocking(move || scan_all_local_models_blocking(&root))
                        .await
                        .map_err(|e| {
                            ApiGatewayError::Internal(format!("扫描本地模型任务 join 失败: {e}"))
                        })?;
                Ok(ok_json(to_value(&models)?))
            }

            // —— GET /api/v1/models/local/:id —— 单模型详情
            //
            // 自家库 `<root>/<id>` 优先；不在库内时按 `org--name` 反查 HF 缓存
            // snapshot（与列表扫描同一解析链，path 返回 snapshot 真实目录）。
            (HttpMethod::Get, ["api", "v1", "models", "local", id]) => {
                let root = models_root();
                let id_owned = (*id).to_string();
                let dir_path = std::path::Path::new(&root).join(&id_owned);
                let own_str = dir_path.to_string_lossy().into_owned();
                let dir = if dir_path.is_dir() {
                    dir_path
                } else {
                    match resolve_hf_snapshot_by_id(&id_owned) {
                        Some(p) => p,
                        None => {
                            return Ok(error_response(
                                404,
                                &format!("本地模型不存在: {id_owned}"),
                            ))
                        }
                    }
                };
                let dir_str = dir.to_string_lossy().into_owned();
                let id_for_closure = id_owned.clone();
                let is_hf = dir_str != own_str;
                let detail =
                    tokio::task::spawn_blocking(move || -> Result<LocalModelDetail, String> {
                        let path = std::path::Path::new(&dir_str);
                        let has_config = path.join("config.json").exists();
                        let (size_bytes, file_count) = dir_size_and_count(path);
                        let modified_at = dir_modified(path);
                        let files = list_model_files_blocking(&dir_str);
                        let config_json = if has_config {
                            read_config_json(&dir_str)
                        } else {
                            None
                        };
                        Ok(LocalModelDetail {
                            model: LocalModel {
                                id: id_for_closure.clone(),
                                path: dir_str.clone(),
                                size_bytes,
                                file_count,
                                modified_at,
                                has_config,
                                source: if is_hf {
                                    "hf_cache".to_string()
                                } else {
                                    "local".to_string()
                                },
                                display_name: if is_hf {
                                    hf_display_from_id(&id_for_closure)
                                } else {
                                    id_for_closure
                                },
                            },
                            files,
                            config_json,
                        })
                    })
                    .await
                    .map_err(|e| ApiGatewayError::Internal(format!("模型详情任务 join 失败: {e}")))?
                    .map_err(ApiGatewayError::Internal)?;
                Ok(ok_json(to_value(&detail)?))
            }

            // —— DELETE /api/v1/models/local/:id —— 删模型（admin；rm -rf 前置安全校验）
            //
            // 与 DELETE /api/v1/models/:name 走同一 [`delete_model_blocking`]：
            // 名字校验 + canonical 父目录必须等于模型根 + 符号链接只解链不删目标。
            (HttpMethod::Delete, ["api", "v1", "models", "local", id]) => {
                let root = models_root();
                let id_owned = (*id).to_string();
                if let Some(reason) = hf_delete_guard(&root, &id_owned) {
                    return Ok(error_response(400, &reason));
                }
                let result =
                    tokio::task::spawn_blocking(move || delete_model_blocking(&root, &id_owned))
                        .await
                        .map_err(|e| {
                            ApiGatewayError::Internal(format!("删除模型任务 join 失败: {e}"))
                        })?;
                match result {
                    Ok(action) => Ok(ok_json(serde_json::json!({
                        "ok": true,
                        "id": id,
                        "action": match action {
                            DeleteAction::RemoveDir(_) => "delete",
                            DeleteAction::UnlinkSymlink(_) => "unlink",
                        },
                    }))),
                    Err(e) if e.contains("不存在") => Ok(error_response(404, &e)),
                    Err(e) => Ok(error_response(400, &e)),
                }
            }

            // ============ A 面：权重文件详细管理 ============

            // —— GET /api/v1/models/:name/detail —— 全文件清单 + 分片解析 + 完整性
            (HttpMethod::Get, ["api", "v1", "models", name, "detail"]) => {
                let root = models_root();
                let name_owned = (*name).to_string();
                let detail = tokio::task::spawn_blocking(move || {
                    scan_model_weight_detail(&root, &name_owned)
                })
                .await
                .map_err(|e| ApiGatewayError::Internal(format!("权重详情任务 join 失败: {e}")))?
                .map_err(|e| {
                    if e.contains("不存在") {
                        error_response(404, &e)
                    } else {
                        error_response(400, &e)
                    }
                });
                match detail {
                    Ok(d) => Ok(ok_json(to_value(&d)?)),
                    Err(resp) => Ok(resp),
                }
            }

            // —— DELETE /api/v1/models/:name —— 删模型（admin；安全校验矩阵）
            (HttpMethod::Delete, ["api", "v1", "models", name]) => {
                let root = models_root();
                let name_owned = (*name).to_string();
                if let Some(reason) = hf_delete_guard(&root, &name_owned) {
                    return Ok(error_response(400, &reason));
                }
                let result =
                    tokio::task::spawn_blocking(move || delete_model_blocking(&root, &name_owned))
                        .await
                        .map_err(|e| {
                            ApiGatewayError::Internal(format!("删除模型任务 join 失败: {e}"))
                        })?;
                match result {
                    Ok(action) => Ok(ok_json(serde_json::json!({
                        "ok": true,
                        "name": name,
                        "action": match action {
                            DeleteAction::RemoveDir(_) => "delete",
                            DeleteAction::UnlinkSymlink(_) => "unlink",
                        },
                    }))),
                    Err(e) if e.contains("不存在") => Ok(error_response(404, &e)),
                    Err(e) => Ok(error_response(400, &e)),
                }
            }

            // —— POST /api/v1/models/import —— 导入库外模型目录为符号链接（admin）
            (HttpMethod::Post, ["api", "v1", "models", "import"]) => {
                let body: serde_json::Value = req.body.clone();
                let Some(path) = body.get("path").and_then(|v| v.as_str()) else {
                    return Ok(error_response(400, "body.path 不可为空"));
                };
                if path.trim().is_empty() {
                    return Ok(error_response(400, "path 不可为空"));
                }
                let root = models_root();
                let path_owned = path.trim().to_string();
                let outcome =
                    tokio::task::spawn_blocking(move || import_model_link(&root, &path_owned))
                        .await
                        .map_err(|e| {
                            ApiGatewayError::Internal(format!("导入任务 join 失败: {e}"))
                        })?;
                match outcome {
                    Ok(o) => Ok(ApiResponse {
                        status: 201,
                        body: to_value(&o)?,
                        headers: serde_json::json!({}),
                    }),
                    Err(e) if e.contains("已存在") => Ok(error_response(409, &e)),
                    Err(e) if e.contains("不存在") || e.contains("不是目录") => {
                        Ok(error_response(404, &e))
                    }
                    Err(e) => Ok(error_response(400, &e)),
                }
            }

            // ============ 下载任务（modelscope + lobby_multi 混排） ============

            // —— GET /api/v1/models/downloads —— 列下载任务（三类任务统一数组）
            (HttpMethod::Get, ["api", "v1", "models", "downloads"]) => {
                // 先刷新所有非终态 modelscope 任务的进度
                let mut tasks = self.downloads.lock().expect("downloads poisoned").clone();
                for t in &mut tasks {
                    if t.status != "completed" && t.status != "failed" {
                        self.refresh_task(t).await;
                    }
                }
                // 回写刷新后的状态
                {
                    let mut guard = self.downloads.lock().expect("downloads poisoned");
                    *guard = tasks.clone();
                }
                // modelscope 任务标 type 后与 lobby_multi / remote_repo 任务拼接
                let mut out: Vec<serde_json::Value> = tasks
                    .iter()
                    .filter_map(|t| {
                        let mut v = to_value(t).ok()?;
                        v["type"] = serde_json::json!("modelscope");
                        Some(v)
                    })
                    .collect();
                out.extend(
                    self.multi
                        .lock()
                        .expect("multi tasks poisoned")
                        .iter()
                        .filter_map(|t| to_value(t).ok()),
                );
                out.extend(
                    self.remote
                        .lock()
                        .expect("remote tasks poisoned")
                        .iter()
                        .filter_map(|t| to_value(t).ok()),
                );
                Ok(ok_json(serde_json::Value::Array(out)))
            }

            // —— POST /api/v1/models/downloads —— 创建下载任务（admin）
            //
            // body 带 `sources` → lobby_multi 多源任务（清单同步拉取，失败 502）；
            // 否则 `model_id` → modelscope 任务（原语义不变）。
            (HttpMethod::Post, ["api", "v1", "models", "downloads"]) => {
                let body: CreateDownloadBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析创建下载任务请求体失败: {e}"))
                })?;
                // ---- C 面：lobby_multi 多源任务 ----
                let sources = body.sources.unwrap_or_default();
                if !sources.is_empty() {
                    return self.create_multi_task(&sources, body.name.as_deref()).await;
                }
                // ---- 旧路径：modelscope 单源任务 ----
                let Some(model_id) = body.model_id else {
                    return Ok(error_response(400, "model_id 或 sources 不可为空"));
                };
                if model_id.trim().is_empty() {
                    return Ok(error_response(400, "model_id 不可为空"));
                }
                let name = model_dir_name(&model_id);
                let root = models_root();
                let local_dir = format!("{root}/{name}");
                // 预估大小：从推荐模型反查
                let estimated = recommended_models()
                    .iter()
                    .find(|r| r.model_id == model_id)
                    .map(|r| (r.size_gb * 1024.0 * 1024.0 * 1024.0) as u64)
                    .unwrap_or(0);
                let mut task = DownloadTask {
                    id: self.next_id("mdl"),
                    model_id: model_id.clone(),
                    local_dir: local_dir.clone(),
                    status: "pending".into(),
                    progress_pct: 0,
                    current_size_bytes: 0,
                    estimated_size_bytes: estimated,
                    pid: None,
                    error: None,
                    created_at: now_iso(),
                };
                // 确保 models 根目录存在（spawn_blocking）
                let root_clone = root.clone();
                let _ =
                    tokio::task::spawn_blocking(move || std::fs::create_dir_all(&root_clone)).await;
                // spawn modelscope download
                match Self::spawn_modelscope(&model_id, &local_dir).await {
                    Ok(pid) => {
                        task.status = "downloading".into();
                        task.pid = Some(pid);
                    }
                    Err(e) => {
                        task.status = "failed".into();
                        task.error = Some(e);
                    }
                }
                let resp = to_value(&task)?;
                self.downloads
                    .lock()
                    .expect("downloads poisoned")
                    .push(task);
                Ok(ApiResponse {
                    status: 201,
                    body: resp,
                    headers: serde_json::json!({}),
                })
            }

            // —— DELETE /api/v1/models/downloads/:id —— 取消下载（admin）
            (HttpMethod::Delete, ["api", "v1", "models", "downloads", id]) => {
                // 先查 lobby_multi：置取消标记并移除（runner 感知后收摊）
                {
                    let mut multi = self.multi.lock().expect("multi tasks poisoned");
                    let before = multi.len();
                    multi.retain(|t| t.id != *id);
                    if multi.len() != before {
                        return Ok(ok_json(serde_json::json!({
                            "ok": true, "id": id, "action": "cancel", "type": "lobby_multi"
                        })));
                    }
                }
                // 再查 remote_repo：同语义移除（runner 感知后收摊，.part 保留可续传）
                {
                    let mut remote = self.remote.lock().expect("remote tasks poisoned");
                    let before = remote.len();
                    remote.retain(|t| t.id != *id);
                    if remote.len() != before {
                        return Ok(ok_json(serde_json::json!({
                            "ok": true, "id": id, "action": "cancel", "type": "remote_repo"
                        })));
                    }
                }
                let mut tasks = self.downloads.lock().expect("downloads poisoned");
                let before = tasks.len();
                // 先 kill 运行中的 pid
                if let Some(t) = tasks.iter_mut().find(|t| t.id == *id) {
                    if let Some(pid) = t.pid {
                        let _ = std::process::Command::new("kill")
                            .arg(pid.to_string())
                            .spawn();
                    }
                    t.status = "failed".into();
                    t.pid = None;
                    t.error = Some("用户取消".into());
                }
                tasks.retain(|t| t.id != *id);
                if tasks.len() == before {
                    return Ok(error_response(404, &format!("下载任务不存在: {id}")));
                }
                Ok(ok_json(serde_json::json!({
                    "ok": true,
                    "id": id,
                    "action": "cancel"
                })))
            }

            // —— GET /api/v1/models/downloads/:id —— 下载任务详情（实时刷新进度）
            (HttpMethod::Get, ["api", "v1", "models", "downloads", id]) => {
                // lobby_multi 任务由后台 runner 持续更新，直接快照返回
                {
                    let multi = self.multi.lock().expect("multi tasks poisoned");
                    if let Some(t) = multi.iter().find(|t| t.id == *id) {
                        return Ok(ok_json(to_value(t)?));
                    }
                }
                // remote_repo 任务同上（后台 runner 持续更新）
                {
                    let remote = self.remote.lock().expect("remote tasks poisoned");
                    if let Some(t) = remote.iter().find(|t| t.id == *id) {
                        return Ok(ok_json(to_value(t)?));
                    }
                }
                let mut task = {
                    let tasks = self.downloads.lock().expect("downloads poisoned");
                    match tasks.iter().find(|t| t.id == *id).cloned() {
                        Some(t) => t,
                        None => return Ok(error_response(404, &format!("下载任务不存在: {id}"))),
                    }
                };
                self.refresh_task(&mut task).await;
                // 回写
                {
                    let mut tasks = self.downloads.lock().expect("downloads poisoned");
                    if let Some(t) = tasks.iter_mut().find(|t| t.id == *id) {
                        *t = task.clone();
                    }
                }
                Ok(ok_json(to_value(&task)?))
            }

            // ============ 推荐 + 统计 ============

            // —— GET /api/v1/models/recommended —— 推荐模型（标注 downloaded）
            (HttpMethod::Get, ["api", "v1", "models", "recommended"]) => {
                let root = models_root();
                let mut recs = recommended_models();
                // 扫描本地，对每个推荐模型检查 config.json 是否存在
                let local_ids: std::collections::HashSet<String> =
                    tokio::task::spawn_blocking(move || scan_local_models_blocking(&root))
                        .await
                        .unwrap_or_default()
                        .into_iter()
                        .map(|m| m.id)
                        .collect();
                for r in &mut recs {
                    let name = model_dir_name(&r.model_id);
                    if local_ids.contains(&name) {
                        r.downloaded = true;
                    }
                }
                Ok(ok_json(to_value(&recs)?))
            }

            // —— GET /api/v1/models/stats —— 聚合统计（自家库 + HF 缓存同口径）
            (HttpMethod::Get, ["api", "v1", "models", "stats"]) => {
                let root = models_root();
                let locals =
                    tokio::task::spawn_blocking(move || scan_all_local_models_blocking(&root))
                        .await
                        .unwrap_or_default();
                let total_size: u64 = locals.iter().map(|m| m.size_bytes).sum();
                let tasks = self.downloads.lock().expect("downloads poisoned");
                let active = tasks
                    .iter()
                    .filter(|t| t.status == "downloading" || t.status == "pending")
                    .count();
                let completed = tasks.iter().filter(|t| t.status == "completed").count();
                drop(tasks);
                // 多源任务并入同一口径（downloading 计 active、completed 计 completed）
                let multi = self.multi.lock().expect("multi tasks poisoned");
                let active = active + multi.iter().filter(|t| t.status == "downloading").count();
                let completed =
                    completed + multi.iter().filter(|t| t.status == "completed").count();
                drop(multi);
                // 在线仓库源任务同口径并入
                let remote = self.remote.lock().expect("remote tasks poisoned");
                let active = active + remote.iter().filter(|t| t.status == "downloading").count();
                let completed =
                    completed + remote.iter().filter(|t| t.status == "completed").count();
                drop(remote);
                Ok(ok_json(to_value(&ModelHubStats {
                    local_total: locals.len(),
                    total_size_bytes: total_size,
                    downloads_active: active,
                    downloads_completed: completed,
                })?))
            }

            // ============ B 面：模型大厅 ============

            // —— POST /api/v1/models/lobby/publish —— 发布本地模型（admin）
            (HttpMethod::Post, ["api", "v1", "models", "lobby", "publish"]) => {
                self.handle_lobby_publish(req).await
            }

            // —— GET /api/v1/models/lobby —— 大厅列表（?name= / ?q=；同 name 多源合并）
            (HttpMethod::Get, ["api", "v1", "models", "lobby"]) => {
                let rows = self.lobby_rows_snapshot();
                let entries = merge_lobby_rows(&rows);
                let name = query_param(&req.path, "name");
                let q = query_param(&req.path, "q");
                let filtered = filter_lobby_entries(entries, name.as_deref(), q.as_deref());
                Ok(ok_json(to_value(&filtered)?))
            }

            // —— GET /api/v1/models/lobby/:name —— 大厅单模型详情（聚合 sources）
            (HttpMethod::Get, ["api", "v1", "models", "lobby", name]) => {
                let rows = self.lobby_rows_snapshot();
                let entries = merge_lobby_rows(&rows);
                match entries.into_iter().find(|e| e.name == *name) {
                    Some(e) => Ok(ok_json(to_value(&e)?)),
                    None => Ok(error_response(404, &format!("大厅无此模型: {name}"))),
                }
            }

            // —— DELETE /api/v1/models/lobby/:id —— 下架（admin 或同 sharer）
            (HttpMethod::Delete, ["api", "v1", "models", "lobby", id]) => {
                let Some(row) = self
                    .lobby_db
                    .lock()
                    .ok()
                    .and_then(|conn| find_lobby_row(&conn, id))
                else {
                    return Ok(error_response(404, &format!("大厅条目不存在: {id}")));
                };
                // 权限：admin 角色放行；否则须是同 sharer（JWT 用户名 == sharer）
                let is_admin = req.auth.as_ref().is_some_and(|p| {
                    p.roles
                        .iter()
                        .any(|r| matches!(r, os_security::Role::Admin))
                });
                let caller_name = req
                    .auth
                    .as_ref()
                    .map(|p| p.user.name.clone())
                    .unwrap_or_default();
                if !is_admin && caller_name != row.sharer {
                    return Ok(error_response(
                        403,
                        &format!(
                            "仅 admin 或发布者 {} 可下架（当前身份: {}）",
                            row.sharer,
                            if caller_name.is_empty() {
                                "匿名"
                            } else {
                                &caller_name
                            }
                        ),
                    ));
                }
                let deleted = self
                    .lobby_db
                    .lock()
                    .ok()
                    .map(|conn| delete_lobby_entry(&conn, id))
                    .unwrap_or(false);
                if !deleted {
                    return Ok(error_response(404, &format!("大厅条目不存在: {id}")));
                }
                Ok(ok_json(serde_json::json!({
                    "ok": true,
                    "id": id,
                    "name": row.name,
                    "sharer": row.sharer,
                    "action": "unpublish"
                })))
            }

            // ============ D 面：在线仓库源（ModelScope / HF 镜像） ============

            // —— GET /api/v1/models/remote/:kind/:org/:model —— 探测（公开读）
            //
            // 存在性 + 文件清单（名称/大小/默认勾选）。kind=modelscope|hf；
            // 仓库不存在 404 / kind 非法 400 / 上游不可达 502。
            (HttpMethod::Get, ["api", "v1", "models", "remote", kind, org, model]) => {
                let Some(k) = RemoteRepoKind::parse(kind) else {
                    return Ok(error_response(
                        400,
                        "kind 须为 modelscope 或 hf（HF 镜像）",
                    ));
                };
                let repo_id = format!("{org}/{model}");
                if let Err(e) = validate_repo_id(&repo_id) {
                    return Ok(error_response(400, &e));
                }
                match probe_remote_repo(k, &repo_id).await {
                    Ok(p) => Ok(ok_json(to_value(&p)?)),
                    Err(e) if e.contains("不存在") => Ok(error_response(404, &e)),
                    Err(e) if e.contains("探测请求失败") || e.contains("探测返回") => {
                        Ok(ApiResponse {
                            status: 502,
                            body: serde_json::json!({"error": e}),
                            headers: serde_json::json!({}),
                        })
                    }
                    Err(e) => Ok(error_response(502, &e)),
                }
            }

            // —— POST /api/v1/models/remote/downloads —— 创建在线仓库下载任务（admin）
            //
            // body: { kind, repo_id, name?, files? }（files 缺省=全部文件；向导传勾选子集）。
            (HttpMethod::Post, ["api", "v1", "models", "remote", "downloads"]) => {
                #[derive(Deserialize)]
                struct CreateRemoteBody {
                    kind: String,
                    repo_id: String,
                    #[serde(default)]
                    name: Option<String>,
                    #[serde(default)]
                    files: Option<Vec<String>>,
                }
                let body: CreateRemoteBody =
                    serde_json::from_value(req.body).map_err(|e| {
                        ApiGatewayError::Internal(format!("解析在线仓库下载请求体失败: {e}"))
                    })?;
                let Some(kind) = RemoteRepoKind::parse(&body.kind) else {
                    return Ok(error_response(
                        400,
                        "kind 须为 modelscope 或 hf（HF 镜像）",
                    ));
                };
                if body.repo_id.trim().is_empty() {
                    return Ok(error_response(400, "repo_id 不可为空（org/model 形态）"));
                }
                self.create_remote_task(
                    kind,
                    body.repo_id.trim(),
                    body.name.as_deref(),
                    body.files.as_deref(),
                )
                .await
            }

            // ============ E 面：Spark 专区（SM120/NVFP4 策展） ============

            // —— GET /api/v1/models/spark-zone —— 策展清单 + 逐条两源实时可用性
            //
            // 公开读；?probe=0 跳过探测（sources 全为"未探测"态，省 3s×并行）。
            // 注意：字面量 arm 须先于任何同段数通配 arm 匹配（当前无 4 段 GET 通配，
            // 此处防御性放置在 share 通配之前）。
            (HttpMethod::Get, ["api", "v1", "models", "spark-zone"]) => {
                let probe = query_param(&req.path, "probe").map(|v| v != "0").unwrap_or(true);
                self.handle_spark_zone(probe).await
            }

            // ============ C 面：文件共享端点（多源下载的 HTTP 传输面） ============

            // —— GET /api/v1/models/share/:name/*path?token= —— 校验 token + 流式回传
            (HttpMethod::Get, ["api", "v1", "models", "share", name, rest @ ..]) => {
                self.handle_share_file(name, rest, &req).await
            }

            // —— 未覆盖路由 —— 兜底 404
            _ => Ok(error_response(404, "model_hub: 未匹配的路由")),
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
        handler_component: "model_hub".to_string(),
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

/// 从请求路径的 query string 中提取指定参数（空值返回 None；files.rs 同款）。
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

/// 极简 URL 解码（仅处理 `+` → 空格 与 `%XX`；files.rs 同款）。
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
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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

    fn del_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Delete,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    // ---- 命令构造器测试 ----

    #[test]
    fn build_download_cmd_contains_model_and_local_dir() {
        let cmd = build_download_cmd("Qwen/Qwen3-VL-8B-Instruct", "/tank/models/Qwen3-VL");
        let joined = cmd.join(" ");
        assert!(
            joined.contains("--model Qwen/Qwen3-VL-8B-Instruct"),
            "缺 --model: {joined}"
        );
        assert!(
            joined.contains("--local_dir /tank/models/Qwen3-VL"),
            "缺 --local_dir: {joined}"
        );
        assert!(
            joined.starts_with("download"),
            "应以 download 开头: {joined}"
        );
    }

    #[test]
    fn model_dir_name_extracts_last_segment() {
        assert_eq!(
            model_dir_name("Qwen/Qwen3-VL-8B-Instruct"),
            "Qwen3-VL-8B-Instruct"
        );
        assert_eq!(model_dir_name("standalone-model"), "standalone-model");
        assert_eq!(model_dir_name("a/b/c"), "c");
    }

    // ---- 扫描本地模型测试 ----

    #[test]
    fn scan_local_models_empty_dir_returns_empty() {
        let tmp = std::env::temp_dir().join(format!(
            "os-modelhub-empty-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let models = scan_local_models_blocking(tmp.to_str().unwrap());
        assert!(models.is_empty(), "空目录应返回空列表");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn scan_local_models_with_config_counts_as_model() {
        let tmp = std::env::temp_dir().join(format!(
            "os-modelhub-cfg-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        // 模型 1：含 config.json
        let m1 = tmp.join("ModelA");
        std::fs::create_dir_all(&m1).unwrap();
        std::fs::write(m1.join("config.json"), r#"{"model_type":"qwen"}"#).unwrap();
        std::fs::write(m1.join("weight.bin"), vec![0u8; 1024]).unwrap();
        // 模型 2：不含 config.json（下载未完成）
        let m2 = tmp.join("ModelB");
        std::fs::create_dir_all(&m2).unwrap();
        std::fs::write(m2.join("partial.bin"), vec![0u8; 512]).unwrap();
        let models = scan_local_models_blocking(tmp.to_str().unwrap());
        assert_eq!(models.len(), 2, "应扫到 2 个子目录");
        let a = models.iter().find(|m| m.id == "ModelA").unwrap();
        assert!(a.has_config, "ModelA 应有 config.json");
        assert_eq!(a.file_count, 2, "ModelA 应有 2 个文件");
        assert!(a.size_bytes >= 1024, "ModelA 大小应 >= 1024");
        let b = models.iter().find(|m| m.id == "ModelB").unwrap();
        assert!(!b.has_config, "ModelB 不应有 config.json");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ---- HF hub 缓存扫描测试（2026-09-03：服务用户 ≠ 安装用户错位）----

    /// 造一个 HF hub 布局仓：`models--<org>--<name>/snapshots/<hash>/`（config +
    /// safetensors 字节可控）+ 可选 `refs/main` 指向某 hash。
    fn write_hf_repo(
        hub: &std::path::Path,
        org_name: &str,
        snapshots: &[(&str, &[(&str, u16)])],
        refs_main: Option<&str>,
    ) {
        let repo = hub.join(format!("models--{org_name}"));
        for (hash, files) in snapshots {
            let snap = repo.join("snapshots").join(hash);
            std::fs::create_dir_all(&snap).unwrap();
            for (fname, size) in *files {
                std::fs::write(snap.join(fname), vec![0u8; u32::from(*size) as usize]).unwrap();
            }
        }
        if let Some(h) = refs_main {
            std::fs::create_dir_all(repo.join("refs")).unwrap();
            std::fs::write(repo.join("refs/main"), format!("{h}\n")).unwrap();
        }
    }

    #[test]
    fn parse_hf_repo_dir_variants() {
        assert_eq!(
            parse_hf_repo_dir("models--nvidia--Qwen3.6-27B-NVFP4").unwrap(),
            ("nvidia".into(), "Qwen3.6-27B-NVFP4".into())
        );
        assert_eq!(
            parse_hf_repo_dir("models--Qwen--Qwen3-VL-8B-Instruct").unwrap(),
            ("Qwen".into(), "Qwen3-VL-8B-Instruct".into())
        );
        // 非缓存布局 / 缺段 / 多段（org/name 自身不含 --，归属不明诚实拒绝）
        assert!(parse_hf_repo_dir("plain-dir").is_none());
        assert!(parse_hf_repo_dir("models--orgonly").is_none());
        assert!(parse_hf_repo_dir("models----name").is_none());
        assert!(parse_hf_repo_dir("models--org--").is_none());
        assert!(parse_hf_repo_dir("models--a--b--c").is_none());
    }

    #[test]
    fn scan_hf_hub_root_full_layout() {
        let hub = temp_dir("hf-hub");
        std::fs::create_dir_all(&hub).unwrap();
        // 仓 1：两个 snapshot，refs/main 指新的 → 取新（旧的文件更大也不算）
        write_hf_repo(
            &hub,
            "nvidia--Qwen3.6-27B-NVFP4",
            &[
                ("aaaaaaaa", &[("config.json", 10), ("w.safetensors", 900)]),
                ("bbbbbbbb", &[("config.json", 10), ("w.safetensors", 20)]),
            ],
            Some("bbbbbbbb"),
        );
        // 仓 2：snapshot 只有 README（无 config/safetensors）→ 剔除
        write_hf_repo(&hub, "qwen--Partial", &[("cccc", &[("README.md", 5)])], None);
        // 仓 3：无 snapshots 子目录 → 剔除
        std::fs::create_dir_all(hub.join("models--x--NoSnap/refs")).unwrap();
        // 非 models-- 前缀目录 → 忽略
        std::fs::create_dir_all(hub.join("unrelated")).unwrap();

        let models = scan_hf_hub_root(&hub);
        assert_eq!(models.len(), 1, "只应识别 1 个模型: {models:?}");
        let m = &models[0];
        assert_eq!(m.id, "nvidia--Qwen3.6-27B-NVFP4");
        assert_eq!(m.display_name, "nvidia/Qwen3.6-27B-NVFP4");
        assert_eq!(m.source, "hf_cache");
        assert!(
            m.path.ends_with("snapshots/bbbbbbbb"),
            "path 应为 refs/main 指向的 snapshot: {}",
            m.path
        );
        assert!(m.has_config);
        assert_eq!(m.file_count, 2);
        assert_eq!(m.size_bytes, 30, "取新 snapshot 的真实占用");
        assert!(!m.modified_at.is_empty());
        let _ = std::fs::remove_dir_all(&hub);
    }

    #[test]
    fn latest_snapshot_falls_back_to_mtime_when_no_refs() {
        let hub = temp_dir("hf-mtime");
        std::fs::create_dir_all(&hub).unwrap();
        // 无 refs 文件（如手工搬来的缓存）→ mtime 最新的 snapshot 胜出；
        // 先造 "aaa" 再造 "zzz"（间隔 15ms 保证 mtime 严格递增）
        write_hf_repo(
            &hub,
            "org--M",
            &[("aaa", &[("config.json", 5), ("a.safetensors", 7)])],
            None,
        );
        std::thread::sleep(std::time::Duration::from_millis(15));
        write_hf_repo(
            &hub,
            "org--M",
            &[("zzz", &[("config.json", 5), ("z.safetensors", 7)])],
            None,
        );
        let models = scan_hf_hub_root(&hub);
        assert_eq!(models.len(), 1);
        assert!(models[0].path.ends_with("snapshots/zzz"), "应取 mtime 更新的");
        let _ = std::fs::remove_dir_all(&hub);
    }

    #[test]
    fn glob_user_hf_caches_multiuser() {
        // 假家目录基座（生产 /home 的参数化替身）：
        // alice 有完整 hub、dave 有 hub、bob 缺 hub 子目录、普通文件被跳过
        let base = temp_dir("hf-homes");
        std::fs::create_dir_all(base.join("alice/.cache/huggingface/hub")).unwrap();
        std::fs::create_dir_all(base.join("bob/.cache/huggingface")).unwrap();
        std::fs::create_dir_all(base.join("dave/.cache/huggingface/hub")).unwrap();
        std::fs::write(base.join("notadir.txt"), b"x").unwrap();
        let roots = glob_user_hf_caches(base.to_str().unwrap());
        assert_eq!(
            roots,
            vec![
                base.join("alice/.cache/huggingface/hub")
                    .to_string_lossy()
                    .into_owned(),
                base.join("dave/.cache/huggingface/hub")
                    .to_string_lossy()
                    .into_owned(),
            ],
            "按用户名排序，只收真实存在的 hub 目录"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn hf_cache_candidate_roots_env_chain() {
        // NEXOS_MODELHUB_HF_CACHE 设置 → 整体替换候选链（测试确定性隔离）
        let _g = ScopedEnvs::set(&[("NEXOS_MODELHUB_HF_CACHE", "/tmp/hf-explicit")]);
        assert_eq!(
            hf_cache_candidate_roots(),
            vec!["/tmp/hf-explicit".to_string()]
        );
        drop(_g);
        // HF 官方 env 候选在前，常规候选（/root + /home/* glob）在后
        let _g = ScopedEnvs::set(&[("HF_HUB_CACHE", "/u1"), ("HF_HOME", "/u2")]);
        let roots = hf_cache_candidate_roots();
        assert_eq!(roots.first().map(String::as_str), Some("/u1"));
        assert_eq!(roots.get(1).map(String::as_str), Some("/u2/hub"));
        assert!(
            roots.contains(&"/root/.cache/huggingface/hub".to_string()),
            "常规 root 候选应在链上: {roots:?}"
        );
        // glob 多用户候选的收录行为由 glob_user_hf_caches_multiuser 单测覆盖
        // （真机 /home 是否有 hub 目录不可假设，这里不对其出现与否做断言）
    }

    #[tokio::test]
    async fn list_local_models_merges_hf_cache_e2e() {
        let root = temp_dir("hf-merge-own");
        std::fs::create_dir_all(&root).unwrap();
        write_plain_model(&root, "Qwen3.5-9B", true);
        let hub = temp_dir("hf-merge-hub");
        write_hf_repo(
            &hub,
            "nvidia--Qwen3.6-27B-NVFP4",
            &[(
                "deadbeef",
                &[
                    ("config.json", 30),
                    ("model-00001-of-00002.safetensors", 100),
                    ("model-00002-of-00002.safetensors", 100),
                    ("model.safetensors.index.json", 10),
                ],
            )],
            Some("deadbeef"),
        );
        // 注意：NEXOS_MODELS_DIR 与 HF 缓存根都走 ScopedEnvs 一把锁设置
        // （ScopedModelsRoot 与 ScopedEnvs 抢同一把 ENV_MUTEX，嵌套持锁会死锁）。
        // 块作用域保证守卫先于清理 drop。
        {
            let _g = ScopedEnvs::set(&[
                ("NEXOS_MODELS_DIR", root.to_str().unwrap()),
                ("NEXOS_MODELHUB_HF_CACHE", hub.to_str().unwrap()),
            ]);
            let h = ModelHubRouteHandler::new();

        // 列表：自家 1 + HF 缓存 1 合并
        let resp = h.handle(get_req("/api/v1/models/local")).await.unwrap();
        assert_eq!(resp.status, 200, "body: {}", resp.body);
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 2, "自家 + HF 缓存合并: {arr:?}");
        let hf = arr
            .iter()
            .find(|m| m["source"] == "hf_cache")
            .expect("应有 HF 缓存条目");
        assert_eq!(hf["id"], "nvidia--Qwen3.6-27B-NVFP4");
        assert_eq!(hf["display_name"], "nvidia/Qwen3.6-27B-NVFP4");
        assert_eq!(
            hf["path"],
            hub.join("models--nvidia--Qwen3.6-27B-NVFP4/snapshots/deadbeef")
                .to_string_lossy()
                .into_owned()
        );
        assert_eq!(hf["file_count"], 4);
        assert!(
            arr.iter().any(|m| m["source"] == "local"),
            "自家条目应带 local 徽章"
        );

        // 权重档案（A 面）对 HF 条目同样成立：分片解析走 snapshot 真实目录
        let resp = h
            .handle(get_req("/api/v1/models/nvidia--Qwen3.6-27B-NVFP4/detail"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "body: {}", resp.body);
        assert_eq!(resp.body["shards"]["sharded"], true);
        assert_eq!(resp.body["shards"]["sequence_complete"], true);
        assert!(resp.body["path"]
            .as_str()
            .unwrap()
            .ends_with("snapshots/deadbeef"));

        // /models/local/:id 详情走同一 HF 解析（LocalModelDetail 是 flatten 结构，
        // source/display_name 直接在顶层）
        let resp = h
            .handle(get_req("/api/v1/models/local/nvidia--Qwen3.6-27B-NVFP4"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "body: {}", resp.body);
        assert_eq!(resp.body["source"], "hf_cache");
        assert_eq!(resp.body["display_name"], "nvidia/Qwen3.6-27B-NVFP4");
        assert!(resp.body["path"]
            .as_str()
            .unwrap_or("")
            .ends_with("snapshots/deadbeef"));

        // 删除被拒（HF 缓存是 huggingface 工具链私有布局）且 snapshot 原样保留
        let resp = h
            .handle(del_req("/api/v1/models/nvidia--Qwen3.6-27B-NVFP4"))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "body: {}", resp.body);
        assert!(
            resp.body["error"].as_str().unwrap_or("").contains("HF 缓存"),
            "拒绝理由应说明 HF 缓存语义"
        );
        assert!(hub.join("models--nvidia--Qwen3.6-27B-NVFP4/snapshots/deadbeef").is_dir());

        // 统计同口径（合并计数）
            let resp = h.handle(get_req("/api/v1/models/stats")).await.unwrap();
            assert_eq!(resp.status, 200);
            assert_eq!(resp.body["local_total"], 2);
        }
        // 守卫先于清理 drop：NEXOS_MODELS_DIR 指向已删目录的窗口会污染并发测试的
        // models_root()（"模型根目录不可用" → 400 而非 404）
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&hub);
    }

    #[tokio::test]
    async fn hf_and_own_same_name_coexist() {
        // 自家目录与 HF 缓存同名（id 相同）→ 两份都在列表（来源徽章区分，不静默去重）
        let root = temp_dir("hf-coexist-own");
        write_plain_model(&root, "nvidia--Qwen3.6-27B-NVFP4", true);
        let hub = temp_dir("hf-coexist-hub");
        write_hf_repo(
            &hub,
            "nvidia--Qwen3.6-27B-NVFP4",
            &[("cafe", &[("config.json", 8), ("m.safetensors", 64)])],
            Some("cafe"),
        );
        {
            let _g = ScopedEnvs::set(&[
                ("NEXOS_MODELS_DIR", root.to_str().unwrap()),
                ("NEXOS_MODELHUB_HF_CACHE", hub.to_str().unwrap()),
            ]);
            let h = ModelHubRouteHandler::new();
            let resp = h.handle(get_req("/api/v1/models/local")).await.unwrap();
            let arr = resp.body.as_array().unwrap();
            assert_eq!(arr.len(), 2, "同名共存不去重: {arr:?}");
            assert_eq!(
                arr.iter()
                    .filter(|m| m["id"] == "nvidia--Qwen3.6-27B-NVFP4")
                    .count(),
                2
            );
            // detail 优先自家库（root 直下同名目录）
            let resp = h
                .handle(get_req("/api/v1/models/nvidia--Qwen3.6-27B-NVFP4/detail"))
                .await
                .unwrap();
            assert!(resp.body["path"]
                .as_str()
                .unwrap()
                .starts_with(root.to_str().unwrap()));
        }
        // 守卫先 drop 再清理（同 list_local_models_merges_hf_cache_e2e）
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&hub);
    }

    // ---- 路由声明测试 ----

    #[tokio::test]
    async fn routes_declares_twenty_endpoints_all_model_hub() {
        let h = ModelHubRouteHandler::new();
        let routes = h.routes().await;
        assert_eq!(routes.len(), 20, "应有 20 条路由: {routes:?}");
        assert!(
            routes.iter().all(|r| r.handler_component == "model_hub"),
            "全部归属 model_hub 组件"
        );
        // 写操作（DELETE / POST）都要求认证；除 lobby 下架外都要求 admin
        // （下架允许"同 sharer"非管理员——handler 内细判，路由层只要求已认证）
        for r in &routes {
            if r.method == HttpMethod::Post || r.method == HttpMethod::Delete {
                assert!(r.requires_auth, "写操作需 auth: {r:?}");
                if r.path != "/api/v1/models/lobby/:name" {
                    assert_eq!(r.required_roles, vec!["admin".to_string()]);
                } else {
                    assert!(r.required_roles.is_empty(), "下架角色在 handler 细判");
                }
            }
        }
        // GET 全部公开（share 端点经 ?token= 自鉴权）
        for r in &routes {
            if r.method == HttpMethod::Get {
                assert!(!r.requires_auth, "GET 应公开: {r:?}");
            }
        }
    }

    // ---- POST 创建下载任务测试 ----

    #[tokio::test]
    async fn create_download_task_added_to_list() {
        let h = ModelHubRouteHandler::with_empty();
        // 创建一个不存在的模型（modelscope spawn 会失败 → status=failed，但不 panic）
        let resp = h
            .handle(post_req(
                "/api/v1/models/downloads",
                serde_json::json!({"model_id": "test/no-such-model-xyz"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "create body: {resp:?}");
        assert!(
            resp.body["status"].as_str().unwrap() == "downloading"
                || resp.body["status"].as_str().unwrap() == "failed",
            "status 应为 downloading 或 failed（取决于 modelscope 是否安装）: {resp:?}"
        );
        assert_eq!(
            resp.body["model_id"], "test/no-such-model-xyz",
            "model_id 回显"
        );
        let id = resp.body["id"].as_str().unwrap().to_string();
        // 列表含新任务
        let tasks = h.downloads_snapshot();
        assert!(tasks.iter().any(|t| t.id == id), "列表应含新任务");
    }

    #[tokio::test]
    async fn create_download_rejects_empty_model_id() {
        let h = ModelHubRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/models/downloads",
                serde_json::json!({"model_id": ""}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    // ---- 推荐模型测试 ----

    #[tokio::test]
    async fn recommended_returns_five_entries() {
        let h = ModelHubRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/models/recommended"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().expect("body 为数组");
        assert_eq!(arr.len(), 5, "应返回 5 条推荐");
        // 含关键字段
        for r in arr {
            assert!(r["model_id"].is_string());
            assert!(r["name"].is_string());
            assert!(r["size_gb"].is_number());
            assert!(r["tags"].is_array());
            assert!(r["downloaded"].is_boolean());
        }
        // 含千问3-VL
        assert!(
            arr.iter()
                .any(|r| r["model_id"] == "Qwen/Qwen3-VL-8B-Instruct"),
            "应含 Qwen3-VL"
        );
    }

    #[test]
    fn recommended_models_static_list_has_five() {
        let recs = recommended_models();
        assert_eq!(recs.len(), 5);
        assert!(recs.iter().all(|r| !r.downloaded), "默认 downloaded=false");
    }

    // ---- 删除任务测试 ----

    #[tokio::test]
    async fn cancel_download_removes_task() {
        let h = ModelHubRouteHandler::with_empty();
        // 先创建
        let resp = h
            .handle(post_req(
                "/api/v1/models/downloads",
                serde_json::json!({"model_id": "test/cancel-me"}),
            ))
            .await
            .unwrap();
        let id = resp.body["id"].as_str().unwrap().to_string();
        assert_eq!(resp.status, 201);
        // 取消
        let resp = h
            .handle(del_req(&format!("/api/v1/models/downloads/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["ok"], true);
        // 列表不再含
        assert!(
            !h.downloads_snapshot().iter().any(|t| t.id == id),
            "取消后任务应移除"
        );
    }

    #[tokio::test]
    async fn cancel_missing_returns_404() {
        let h = ModelHubRouteHandler::new();
        let resp = h
            .handle(del_req("/api/v1/models/downloads/nope"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // ---- stats 测试 ----

    #[tokio::test]
    async fn stats_returns_counts_without_panic() {
        let h = ModelHubRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/models/stats")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body["local_total"].is_u64());
        assert!(resp.body["total_size_bytes"].is_u64());
        assert!(resp.body["downloads_active"].is_u64());
        assert!(resp.body["downloads_completed"].is_u64());
    }

    // ---- 本地模型列表测试（真实 FS，至少不 panic）----

    #[tokio::test]
    async fn list_local_models_returns_array_without_panic() {
        let h = ModelHubRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/models/local")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body.is_array());
    }

    // ---- 删除模型 path 穿越防护 ----

    #[tokio::test]
    async fn delete_model_rejects_dotdot() {
        let h = ModelHubRouteHandler::new();
        let resp = h.handle(del_req("/api/v1/models/local/..")).await.unwrap();
        assert_eq!(resp.status, 400);
    }

    #[tokio::test]
    async fn delete_model_missing_returns_404() {
        let h = ModelHubRouteHandler::new();
        // 使用带 __never__ 前缀的 id，避免与下载测试的 model_dir_name 冲突（并发时
        // modelscope 可能创建 /tank/models/<name> 目录导致 is_dir 误判）。
        let resp = h
            .handle(del_req("/api/v1/models/local/__never_exists_zzz__"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404, "body: {}", resp.body);
    }

    // ---- 单模型详情 ----

    #[tokio::test]
    async fn get_local_model_detail_missing_returns_404() {
        let h = ModelHubRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/models/local/__never_exists_zzz__"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn unmatched_route_returns_404() {
        let h = ModelHubRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/models/nope")).await.unwrap();
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<ModelHubRouteHandler>();
    }

    // ====================================================================
    // 测试助手（A/B/C 共用）
    // ====================================================================

    /// models_root() env 覆盖的全局互斥（并行测试下防串写）。
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// env 覆盖守卫：drop 时移除 `NEXOS_MODELS_DIR`（断言 panic 也不泄漏给其他测试）。
    /// 字段仅用于持有互斥锁到 drop（永不读——RAII）。
    struct ScopedModelsRoot(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

    impl ScopedModelsRoot {
        fn set(root: &std::path::Path) -> Self {
            let g: std::sync::MutexGuard<'static, ()> =
                ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            std::env::set_var("NEXOS_MODELS_DIR", root);
            Self(g)
        }
    }

    impl Drop for ScopedModelsRoot {
        fn drop(&mut self) {
            std::env::remove_var("NEXOS_MODELS_DIR");
        }
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "os-modelhub-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    /// 测试用 config.json 内容（A 面多处复用；字节数参与大小断言）。
    const TEST_CONFIG: &str = r#"{"model_type":"qwen3vl","num_hidden_layers":36,"hidden_size":2048,"vocab_size":151936,"max_position_embeddings":262144}"#;

    /// 写一个分片模型目录（present 缺号即不完整；with_index 控制 index.json）。
    fn write_sharded_model(
        root: &std::path::Path,
        name: &str,
        total: u32,
        present: &[u32],
        with_index: bool,
    ) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        for i in present {
            std::fs::write(
                dir.join(format!("model-{i:05}-of-{total:05}.safetensors")),
                vec![*i as u8; 100],
            )
            .unwrap();
        }
        if with_index {
            std::fs::write(dir.join("model.safetensors.index.json"), "{}").unwrap();
        }
        std::fs::write(dir.join("config.json"), TEST_CONFIG).unwrap();
    }

    /// 写一个单权重文件模型目录。
    fn write_plain_model(root: &std::path::Path, name: &str, with_config: bool) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("model.safetensors"), vec![7u8; 256]).unwrap();
        if with_config {
            std::fs::write(dir.join("config.json"), r#"{"model_type":"qwen2"}"#).unwrap();
        }
    }

    /// 构造认证身份（lobby 下架权限矩阵用）。
    fn principal(name: &str, admin: bool) -> os_security::Principal {
        let now = chrono::Utc::now();
        let roles = if admin {
            vec![os_security::Role::Admin]
        } else {
            vec![os_security::Role::User]
        };
        let user = os_security::User::new(os_security::UserId::new(name), name, roles.clone(), now)
            .unwrap();
        os_security::Principal::new(user, roles, now).unwrap()
    }

    fn del_req_auth(path: &str, auth: Option<os_security::Principal>) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Delete,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth,
        }
    }

    // ====================================================================
    // A 面：权重文件详细管理（11 个测试）
    // ====================================================================

    #[test]
    fn parse_shard_filename_variants() {
        assert_eq!(
            parse_shard_filename("model-00001-of-00005.safetensors"),
            Some(ShardRef { index: 1, total: 5 })
        );
        assert_eq!(
            parse_shard_filename("qwen-00012-of-00012.safetensors"),
            Some(ShardRef {
                index: 12,
                total: 12
            })
        );
        // 非分片命名
        assert_eq!(parse_shard_filename("model.safetensors"), None);
        assert_eq!(parse_shard_filename("README.md"), None);
        assert_eq!(parse_shard_filename("model-1-of-5.safetensors"), None);
        assert_eq!(
            parse_shard_filename("model-00000-of-00005.safetensors"),
            None
        );
    }

    #[test]
    fn analyze_shards_full_sequence_complete() {
        let files = [
            "config.json",
            "model.safetensors.index.json",
            "model-00001-of-00003.safetensors",
            "model-00002-of-00003.safetensors",
            "model-00003-of-00003.safetensors",
        ];
        let a = analyze_shards(&files, true);
        assert!(a.sharded);
        assert_eq!(a.shard_total, 3);
        assert_eq!(a.shard_files.len(), 3);
        assert!(a.sequence_complete, "1..=3 全在场应连续");
        assert!(a.missing_shards.is_empty());
        assert!(a.index_file_present);
        assert!(judge_complete(&a, true, true), "连续+index → 完整");
    }

    #[test]
    fn analyze_shards_missing_middle_reports_gap() {
        let files = [
            "model-00001-of-00004.safetensors",
            "model-00002-of-00004.safetensors",
            "model-00004-of-00004.safetensors",
        ];
        let a = analyze_shards(&files, true);
        assert_eq!(a.shard_total, 4);
        assert!(!a.sequence_complete, "缺 3 号应不连续");
        assert_eq!(a.missing_shards, vec![3]);
        assert!(!judge_complete(&a, true, true), "缺号 → 不完整");
        // 缺 index.json 同样不完整
        let a2 = analyze_shards(&files, false);
        assert!(!judge_complete(&a2, true, true), "缺 index.json → 不完整");
    }

    #[test]
    fn judge_complete_unsharded_requires_config_and_weight() {
        let files = ["model.safetensors"];
        let a = analyze_shards(&files, false);
        assert!(!a.sharded);
        assert!(judge_complete(&a, true, true), "有权重+config → 完整");
        assert!(!judge_complete(&a, false, true), "缺 config → 不完整");
        assert!(!judge_complete(&a, true, false), "无权重 → 不完整");
    }

    #[test]
    fn parse_config_info_extracts_arch_fields() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"model_type":"qwen3vl","num_hidden_layers":36,"hidden_size":2048,
                "vocab_size":"151936","max_position_embeddings":262144,"torch_dtype":"bfloat16"}"#,
        )
        .unwrap();
        let c = parse_config_info(&v);
        assert_eq!(c.arch, "qwen3vl");
        assert_eq!(c.num_hidden_layers, Some(36));
        assert_eq!(c.hidden_size, Some(2048));
        assert_eq!(c.vocab_size, Some(151936), "字符串数字也应解析");
        assert_eq!(c.max_position_embeddings, Some(262144));
        assert_eq!(c.raw["torch_dtype"], "bfloat16", "raw 保留原始字段");
    }

    #[test]
    fn scan_model_weight_detail_three_models_real_assertions() {
        let root = temp_dir("detail-scan");
        std::fs::create_dir_all(&root).unwrap();
        // 模型 1：2 分片完整 + index + config
        write_sharded_model(&root, "ShardedFull", 2, &[1, 2], true);
        // 模型 2：3 分片缺 2 号（无 index）
        write_sharded_model(&root, "ShardedGap", 3, &[1, 3], false);
        // 模型 3：单权重 + config
        write_plain_model(&root, "PlainModel", true);

        let d1 = scan_model_weight_detail(root.to_str().unwrap(), "ShardedFull").unwrap();
        assert!(d1.complete, "ShardedFull 应完整");
        assert_eq!(d1.shards.shard_total, 2);
        assert_eq!(d1.file_count, 4, "2 分片+index+config");
        assert_eq!(
            d1.total_size_bytes,
            2 * 100 + 2 + TEST_CONFIG.len() as u64,
            "2×100 分片 + 2 字节 index + config"
        );
        assert_eq!(d1.config.as_ref().unwrap().arch, "qwen3vl");
        assert_eq!(d1.config.as_ref().unwrap().num_hidden_layers, Some(36));
        // 分片序号挂在文件条目上
        let shard_file = d1
            .files
            .iter()
            .find(|f| f.name.ends_with("00001-of-00002.safetensors"))
            .unwrap();
        assert_eq!(shard_file.shard_index, Some(1));
        assert_eq!(shard_file.shard_total, Some(2));

        let d2 = scan_model_weight_detail(root.to_str().unwrap(), "ShardedGap").unwrap();
        assert!(!d2.complete, "缺 2 号 + 无 index → 不完整");
        assert_eq!(d2.shards.missing_shards, vec![2]);

        let d3 = scan_model_weight_detail(root.to_str().unwrap(), "PlainModel").unwrap();
        assert!(d3.complete);
        assert_eq!(d3.file_count, 2);
        assert!(!d3.shards.sharded);

        // 不存在的模型
        assert!(scan_model_weight_detail(root.to_str().unwrap(), "Nope").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn get_model_detail_endpoint_e2e() {
        let root = temp_dir("detail-http");
        std::fs::create_dir_all(&root).unwrap();
        write_sharded_model(&root, "Qwen3-VL-8B", 3, &[1, 2, 3], true);
        let h = ModelHubRouteHandler::new();
        let _g = ScopedModelsRoot::set(&root);
        let resp = h
            .handle(get_req("/api/v1/models/Qwen3-VL-8B/detail"))
            .await
            .unwrap();

        assert_eq!(resp.status, 200, "body: {}", resp.body);
        assert_eq!(resp.body["name"], "Qwen3-VL-8B");
        assert_eq!(resp.body["complete"], true);
        assert_eq!(resp.body["shards"]["shard_total"], 3);
        assert_eq!(resp.body["shards"]["sequence_complete"], true);
        assert_eq!(resp.body["config"]["arch"], "qwen3vl");
        assert_eq!(resp.body["config"]["hidden_size"], 2048);
        assert_eq!(resp.body["files"].as_array().unwrap().len(), 5);
        // 不存在 → 404；非法名 → 400
        assert_eq!(
            h.handle(get_req("/api/v1/models/__nope__/detail"))
                .await
                .unwrap()
                .status,
            404
        );
        assert_eq!(
            h.handle(get_req("/api/v1/models/..%2Fetc/detail"))
                .await
                .unwrap()
                .status,
            400
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_safety_matrix_validate() {
        let root = temp_dir("del-matrix");
        std::fs::create_dir_all(root.join("Good")).unwrap();
        std::fs::create_dir_all(root.join("Good/Nested")).unwrap();
        std::fs::create_dir_all(root.join("Outside")).unwrap();
        std::fs::write(root.join("afile.txt"), b"x").unwrap();
        // 外部目录（导入目标模拟）
        let ext = temp_dir("del-matrix-ext");
        std::fs::create_dir_all(&ext).unwrap();
        std::os::unix::fs::symlink(&ext, root.join("Linked")).unwrap();

        let r = root.to_str().unwrap();
        // 合法直系目录 → RemoveDir
        assert!(matches!(
            validate_delete_target(r, "Good"),
            Ok(DeleteAction::RemoveDir(_))
        ));
        // `..` / 含斜杠 / 空 → 拒绝
        assert!(validate_delete_target(r, "..").is_err());
        assert!(validate_delete_target(r, "a/../b").is_err());
        assert!(validate_delete_target(r, "a/b").is_err());
        assert!(validate_delete_target(r, "").is_err());
        // 不存在 → 拒绝（404 语义）
        assert!(validate_delete_target(r, "Missing").is_err());
        // 嵌套子目录（路径在根内但非直系）→ 拒绝
        assert!(validate_delete_target(r, "Nested").is_err());
        // 普通文件 → 拒绝
        assert!(validate_delete_target(r, "afile.txt").is_err());
        // 符号链接 → 只解链（目标在库外也不跟进）
        match validate_delete_target(r, "Linked") {
            Ok(DeleteAction::UnlinkSymlink(_)) => {}
            other => panic!("符号链接应 UnlinkSymlink: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&ext);
    }

    #[test]
    fn delete_symlink_unlinks_without_touching_target() {
        let root = temp_dir("del-link");
        std::fs::create_dir_all(&root).unwrap();
        let ext = temp_dir("del-link-ext");
        std::fs::create_dir_all(ext.join("MyModel")).unwrap();
        std::fs::write(ext.join("MyModel/config.json"), b"{}").unwrap();
        std::fs::write(ext.join("MyModel/w.safetensors"), vec![0u8; 16]).unwrap();
        std::os::unix::fs::symlink(ext.join("MyModel"), root.join("MyModel")).unwrap();

        let action = delete_model_blocking(root.to_str().unwrap(), "MyModel").unwrap();
        assert!(matches!(action, DeleteAction::UnlinkSymlink(_)));
        assert!(!root.join("MyModel").exists(), "链接应被解除");
        // 目标目录完好（导入源不被 rm -rf 波及）
        assert!(ext.join("MyModel/config.json").exists());
        assert!(ext.join("MyModel/w.safetensors").exists());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&ext);
    }

    #[test]
    fn import_model_link_validates_and_becomes_visible_in_list() {
        let root = temp_dir("import");
        std::fs::create_dir_all(&root).unwrap();
        // 库外合法模型
        let ext = temp_dir("import-ext");
        let src = ext.join("Qwen2.5-7B");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("config.json"), b"{\"model_type\":\"qwen2\"}").unwrap();
        std::fs::write(src.join("w.safetensors"), vec![0u8; 32]).unwrap();
        // 库外非模型目录（无 config 无 safetensors）
        let bad = ext.join("NotAModel");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("notes.txt"), b"x").unwrap();

        let r = root.to_str().unwrap();
        // 非模型目录 → 拒绝
        let err = import_model_link(r, bad.to_str().unwrap()).unwrap_err();
        assert!(err.contains("不认为是模型"), "err: {err}");
        // 不存在 → 拒绝
        assert!(import_model_link(r, "/nonexistent/xyz").is_err());
        // 根内目录 → 拒绝（无需导入）
        assert!(import_model_link(r, r).is_err());
        // 合法导入 → 符号链接 + list 可见
        let out = import_model_link(r, src.to_str().unwrap()).unwrap();
        assert_eq!(out.name, "Qwen2.5-7B");
        assert!(out.link_path.contains("Qwen2.5-7B"));
        assert!(std::path::Path::new(&out.link_path).is_symlink());
        let models = scan_local_models_blocking(r);
        let m = models.iter().find(|m| m.id == "Qwen2.5-7B").unwrap();
        assert!(m.has_config, "经链接应读到 config.json");
        assert_eq!(m.file_count, 2);
        // 重复导入 → 冲突
        assert!(import_model_link(r, src.to_str().unwrap())
            .unwrap_err()
            .contains("已存在"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&ext);
    }

    #[tokio::test]
    async fn import_endpoint_e2e_creates_link() {
        let root = temp_dir("import-http");
        std::fs::create_dir_all(&root).unwrap();
        let ext = temp_dir("import-http-ext");
        let src = ext.join("ExternalModel");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("w.safetensors"), vec![1u8; 64]).unwrap();
        let h = ModelHubRouteHandler::new();
        let _g = ScopedModelsRoot::set(&root);
        let resp = h
            .handle(post_req(
                "/api/v1/models/import",
                serde_json::json!({"path": src.to_str().unwrap()}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "body: {}", resp.body);
        assert_eq!(resp.body["name"], "ExternalModel");
        // local 列表可见
        let list = h.handle(get_req("/api/v1/models/local")).await.unwrap();
        assert!(list
            .body
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "ExternalModel"));
        // 非模型目录 → 400；缺 path → 400
        let bad = ext.join("EmptyDir");
        std::fs::create_dir_all(&bad).unwrap();
        let resp = h
            .handle(post_req(
                "/api/v1/models/import",
                serde_json::json!({"path": bad.to_str().unwrap()}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        let resp = h
            .handle(post_req("/api/v1/models/import", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&ext);
    }

    // ====================================================================
    // B 面：模型大厅（10 个测试）
    // ====================================================================

    #[test]
    fn build_source_url_and_lobby_id_pure() {
        assert_eq!(
            build_source_url("ub2604", "8080", "Qwen3", "tk"),
            "http://ub2604:8080/api/v1/models/share/Qwen3?token=tk"
        );
        // 空 token 省略 query
        assert_eq!(
            build_source_url("h", "1", "M", ""),
            "http://h:1/api/v1/models/share/M"
        );
        assert_eq!(lobby_id("Qwen3", "alice"), "Qwen3@alice");
        assert_eq!(lobby_id("Qwen3", "a b/c"), "Qwen3@a-b-c", "非法字符净化");
        assert_eq!(sanitize_sharer("  "), "admin", "空名回退 admin");
    }

    fn lobby_row(name: &str, sharer: &str, downloads: u64) -> LobbyRow {
        LobbyRow {
            id: lobby_id(name, sharer),
            name: name.into(),
            display_name: format!("展示-{name}"),
            description: format!("desc-{sharer}"),
            tags: vec!["vl".into()],
            arch: "qwen3vl".into(),
            size_bytes: 100,
            file_count: 3,
            sharer: sharer.into(),
            source_url: format!("http://{sharer}:8080/api/v1/models/share/{name}?token=t"),
            created_at: format!(
                "2026-08-2{sharer_len}:00:00",
                sharer_len = if sharer == "alice" { "1" } else { "2" }
            ),
            download_count: downloads,
        }
    }

    #[test]
    fn merge_lobby_rows_merges_same_name_and_sorts() {
        let rows = vec![
            lobby_row("ModelA", "alice", 2),
            lobby_row("ModelB", "bob", 7),
            lobby_row("ModelA", "bob", 3),
        ];
        let merged = merge_lobby_rows(&rows);
        assert_eq!(merged.len(), 2, "同 name 合并为一条");
        // 下载量降序：ModelB (7) 在前
        assert_eq!(merged[0].name, "ModelB");
        let a = merged.iter().find(|e| e.name == "ModelA").unwrap();
        assert_eq!(a.sources.len(), 2, "两发布者聚合为两个来源");
        assert_eq!(a.download_count, 5, "下载量求和");
        // sources 按发布时间升序（alice 的 created_at 更小在前）
        assert_eq!(a.sources[0].sharer, "alice");
        assert_eq!(a.sources[1].sharer, "bob");
        assert_eq!(a.arch, "qwen3vl");
    }

    #[test]
    fn filter_lobby_entries_name_and_query() {
        let entries = merge_lobby_rows(&[
            lobby_row("Qwen3-VL", "alice", 1),
            lobby_row("Llama3", "bob", 9),
        ]);
        // name 精确
        let by_name = filter_lobby_entries(entries.clone(), Some("Llama3"), None);
        assert_eq!(by_name.len(), 1);
        assert_eq!(by_name[0].name, "Llama3");
        // q 子串（description / 名字 / 大小写不敏感）
        assert_eq!(
            filter_lobby_entries(entries.clone(), None, Some("desc-alice")).len(),
            1
        );
        assert_eq!(
            filter_lobby_entries(entries.clone(), None, Some("llama")).len(),
            1
        );
        assert_eq!(
            filter_lobby_entries(entries.clone(), None, Some("qwen3vl")).len(),
            2,
            "arch 命中两个模型"
        );
        assert_eq!(filter_lobby_entries(entries, None, Some("nope")).len(), 0);
        // 空 filter 原样
        let all = filter_lobby_entries(merge_lobby_rows(&[lobby_row("X", "a", 0)]), None, None);
        assert_eq!(all.len(), 1);
    }

    /// 发布前置模型目录 + 注入 env 的组合助手（guard drop 自动清 env）。
    async fn publish_ok(
        h: &ModelHubRouteHandler,
        root: &std::path::Path,
        body: serde_json::Value,
    ) -> ApiResponse {
        let _g = ScopedModelsRoot::set(root);
        h.handle(post_req("/api/v1/models/lobby/publish", body))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn publish_requires_local_model() {
        let root = temp_dir("pub-404");
        std::fs::create_dir_all(&root).unwrap();
        let h = ModelHubRouteHandler::with_empty().with_admin_token("tk");
        let resp = publish_ok(
            &h,
            &root,
            serde_json::json!({"name": "GhostModel", "description": "x"}),
        )
        .await;
        assert_eq!(resp.status, 404, "本地不存在不可发布: {}", resp.body);
        // 非法名 → 400
        let resp = publish_ok(&h, &root, serde_json::json!({"name": "../etc"})).await;
        assert_eq!(resp.status, 400);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn publish_creates_entry_with_source_url() {
        let root = temp_dir("pub-ok");
        write_sharded_model(&root, "Qwen3-VL-8B", 2, &[1, 2], true);
        let h = ModelHubRouteHandler::with_empty().with_admin_token("tk-1");
        let resp = publish_ok(
            &h,
            &root,
            serde_json::json!({
                "name": "Qwen3-VL-8B",
                "display_name": "千问3-VL",
                "description": "视觉语言模型",
                "tags": ["vl", "8B"],
                "sharer": "alice"
            }),
        )
        .await;
        assert_eq!(resp.status, 201, "body: {}", resp.body);
        assert_eq!(resp.body["id"], "Qwen3-VL-8B@alice");
        assert_eq!(resp.body["arch"], "qwen3vl");
        assert_eq!(resp.body["file_count"], 4);
        assert_eq!(resp.body["sharer"], "alice");
        assert_eq!(resp.body["share_token"], "tk-1");
        let url = resp.body["source_url"].as_str().unwrap();
        assert!(
            url.starts_with("http://")
                && url.ends_with("/api/v1/models/share/Qwen3-VL-8B?token=tk-1"),
            "source_url 形状: {url}"
        );
        // DB 快照有该行
        assert_eq!(h.lobby_rows_snapshot().len(), 1);
        // 同 (name, sharer) 重复发布 = 刷新（幂等，不新增行）
        let resp2 = publish_ok(
            &h,
            &root,
            serde_json::json!({"name": "Qwen3-VL-8B", "sharer": "alice"}),
        )
        .await;
        assert_eq!(resp2.status, 201);
        assert_eq!(h.lobby_rows_snapshot().len(), 1, "同 id 刷新不重复");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn lobby_list_merges_dual_publishers_and_detail() {
        // 两个"发布者"：同一模型名在两个独立 handler（模拟两台机器的库）各发布一次，
        // 这里用同一 handler 两个 sharer 验证合并逻辑（sources 聚合）
        let root = temp_dir("lobby-merge");
        write_plain_model(&root, "SharedModel", true);
        let h = ModelHubRouteHandler::with_empty().with_admin_token("tk");
        for sharer in ["alice", "bob"] {
            let resp = publish_ok(
                &h,
                &root,
                serde_json::json!({"name": "SharedModel", "sharer": sharer}),
            )
            .await;
            assert_eq!(resp.status, 201);
        }
        // 列表：一条 + sources 两个
        let list = h.handle(get_req("/api/v1/models/lobby")).await.unwrap();
        let arr = list.body.as_array().unwrap();
        assert_eq!(arr.len(), 1, "同 name 合并: {arr:?}");
        assert_eq!(arr[0]["sources"].as_array().unwrap().len(), 2);
        let sharers: Vec<&str> = arr[0]["sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["sharer"].as_str().unwrap())
            .collect();
        assert!(sharers.contains(&"alice") && sharers.contains(&"bob"));
        // 详情
        let detail = h
            .handle(get_req("/api/v1/models/lobby/SharedModel"))
            .await
            .unwrap();
        assert_eq!(detail.status, 200);
        assert_eq!(detail.body["sources"].as_array().unwrap().len(), 2);
        // 搜索：?name= 精确 / ?q=
        let by_q = h
            .handle(get_req("/api/v1/models/lobby?q=sharedmodel"))
            .await
            .unwrap();
        assert_eq!(by_q.body.as_array().unwrap().len(), 1);
        let by_name = h
            .handle(get_req("/api/v1/models/lobby?name=SharedModel"))
            .await
            .unwrap();
        assert_eq!(by_name.body.as_array().unwrap().len(), 1);
        let none = h
            .handle(get_req("/api/v1/models/lobby?q=zzz"))
            .await
            .unwrap();
        assert_eq!(none.body.as_array().unwrap().len(), 0);
        assert_eq!(
            h.handle(get_req("/api/v1/models/lobby/Missing"))
                .await
                .unwrap()
                .status,
            404
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn lobby_delete_permission_matrix() {
        let root = temp_dir("lobby-del");
        write_plain_model(&root, "DelModel", true);
        let h = ModelHubRouteHandler::with_empty().with_admin_token("tk");
        let resp = publish_ok(
            &h,
            &root,
            serde_json::json!({"name": "DelModel", "sharer": "bob"}),
        )
        .await;
        let id = resp.body["id"].as_str().unwrap().to_string();
        // 匿名 → 路由层已拦（这里直接构造带 None 的请求验证 handler 兜底 403）
        let r = h
            .handle(del_req_auth(&format!("/api/v1/models/lobby/{id}"), None))
            .await
            .unwrap();
        assert_eq!(r.status, 403, "匿名不可下架: {}", r.body);
        // 他人非 admin → 403
        let r = h
            .handle(del_req_auth(
                &format!("/api/v1/models/lobby/{id}"),
                Some(principal("carol", false)),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 403);
        // 同 sharer 非 admin → 放行
        let r = h
            .handle(del_req_auth(
                &format!("/api/v1/models/lobby/{id}"),
                Some(principal("bob", false)),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "body: {}", r.body);
        assert_eq!(r.body["action"], "unpublish");
        assert!(h.lobby_rows_snapshot().is_empty(), "下架后清空");
        // admin 删他人条目：重新发布后用 admin 身份删
        publish_ok(
            &h,
            &root,
            serde_json::json!({"name": "DelModel", "sharer": "bob"}),
        )
        .await;
        let r = h
            .handle(del_req_auth(
                &format!("/api/v1/models/lobby/{id}"),
                Some(principal("root", true)),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "admin 可删任意条目: {}", r.body);
        // 不存在 → 404
        let r = h
            .handle(del_req_auth(
                "/api/v1/models/lobby/none@none",
                Some(principal("root", true)),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 404);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn share_rejects_bad_token() {
        let root = temp_dir("share-tok");
        write_plain_model(&root, "TokModel", true);
        let h = ModelHubRouteHandler::with_empty().with_admin_token("right-token");
        // 错 token / 无 token → 401
        for q in ["?token=wrong", ""] {
            let r = h
                .handle(get_req(&format!(
                    "/api/v1/models/share/TokModel/config.json{q}"
                )))
                .await
                .unwrap();
            assert_eq!(r.status, 401, "q={q:?} body: {}", r.body);
        }
        // 未配置 admin token 的实例（open_lobby 直构，admin_token=None）→ 一律 401
        let h2 = ModelHubRouteHandler::open_lobby(":memory:");
        let r = h2
            .handle(get_req(
                "/api/v1/models/share/TokModel/config.json?token=anything",
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 401, "未配置 admin token 应拒绝: {}", r.body);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn share_rejects_path_traversal() {
        let root = temp_dir("share-trav");
        write_plain_model(&root, "TravModel", true);
        let h = ModelHubRouteHandler::with_empty().with_admin_token("tk");
        let _g = ScopedModelsRoot::set(&root);
        // 明文 .. 段
        let r = h
            .handle(get_req(
                "/api/v1/models/share/TravModel/../Other/config.json?token=tk",
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 400, "body: {}", r.body);
        // percent-encoded %2e%2e
        let r = h
            .handle(get_req(
                "/api/v1/models/share/TravModel/%2e%2e/config.json?token=tk",
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 400, "编码后的 .. 也要拒: {}", r.body);
        // 根外不存在模型 → 404
        let r = h
            .handle(get_req("/api/v1/models/share/Nope/config.json?token=tk"))
            .await
            .unwrap();
        assert_eq!(r.status, 404);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn share_serves_file_chunks_and_rejects_directory() {
        let root = temp_dir("share-file");
        let dir = root.join("ChunkModel");
        std::fs::create_dir_all(dir.join("subdir")).unwrap();
        let content: Vec<u8> = (0..100u8).collect();
        std::fs::write(dir.join("weights.bin"), &content).unwrap();
        std::fs::write(dir.join("config.json"), b"{\"model_type\":\"x\"}").unwrap();
        let h = ModelHubRouteHandler::with_empty().with_admin_token("tk");
        let _g = ScopedModelsRoot::set(&root);
        // 整文件
        let r = h
            .handle(get_req(
                "/api/v1/models/share/ChunkModel/weights.bin?token=tk",
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200, "body: {r:?}");
        assert_eq!(r.body["total_size"], 100);
        assert_eq!(r.body["eof"], true);
        assert_eq!(r.body["length"], 100);
        let b64 = r.body["content_base64"].as_str().unwrap();
        let got = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        assert_eq!(got, content, "base64 往返无损");
        // 分段：offset=10 length=5
        let r = h
            .handle(get_req(
                "/api/v1/models/share/ChunkModel/weights.bin?token=tk&offset=10&length=5",
            ))
            .await
            .unwrap();
        assert_eq!(r.body["offset"], 10);
        assert_eq!(r.body["length"], 5);
        assert_eq!(r.body["eof"], false);
        let got = base64::engine::general_purpose::STANDARD
            .decode(r.body["content_base64"].as_str().unwrap())
            .unwrap();
        assert_eq!(got, content[10..15], "分段切片正确");
        // 嵌套子目录文件
        std::fs::write(dir.join("subdir/deep.bin"), b"deep").unwrap();
        let r = h
            .handle(get_req(
                "/api/v1/models/share/ChunkModel/subdir/deep.bin?token=tk",
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        // 目录 → 400
        let r = h
            .handle(get_req("/api/v1/models/share/ChunkModel/subdir?token=tk"))
            .await
            .unwrap();
        assert_eq!(r.status, 400, "目录不可整体下载: {}", r.body);
        // 不存在文件 → 404；offset 越界 → 400；超长 length → 400
        assert_eq!(
            h.handle(get_req("/api/v1/models/share/ChunkModel/nope.bin?token=tk"))
                .await
                .unwrap()
                .status,
            404
        );
        assert_eq!(
            h.handle(get_req(
                "/api/v1/models/share/ChunkModel/weights.bin?token=tk&offset=999"
            ))
            .await
            .unwrap()
            .status,
            400
        );
        assert_eq!(
            h.handle(get_req(&format!(
                "/api/v1/models/share/ChunkModel/weights.bin?token=tk&length={}",
                SHARE_MAX_CHUNK_BYTES + 1
            )))
            .await
            .unwrap()
            .status,
            400
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn validate_share_rel_path_pure() {
        assert!(validate_share_rel_path(&["config.json"]).is_ok());
        assert!(validate_share_rel_path(&["sub", "w.safetensors"]).is_ok());
        assert!(validate_share_rel_path(&[]).is_err(), "空路径拒绝");
        assert!(validate_share_rel_path(&[".."]).is_err());
        assert!(validate_share_rel_path(&["a", "..", "b"]).is_err());
        assert!(validate_share_rel_path(&["."]).is_err());
        assert!(validate_share_rel_path(&[""]).is_err());
        assert!(validate_share_rel_path(&["a\\b"]).is_err(), "反斜杠拒绝");
    }

    // ====================================================================
    // C 面：多源下载（9 个测试；本地 std TcpListener 假双源 HTTP 服务端到端）
    // ====================================================================

    /// 假源 HTTP 服务：最小实现（GET only，Connection: close）。
    struct FakeSource {
        base: String,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl FakeSource {
        /// `files`：(相对名, 字节)；`fail_files`：这些文件回 404（模拟换源场景）；
        /// `size_inflate`：清单声明大小比实际字节多出的量（模拟远端损坏）。
        fn start(
            name: &str,
            files: Vec<(String, Vec<u8>)>,
            fail_files: Vec<String>,
            size_inflate: u64,
        ) -> Self {
            use std::io::{Read, Write};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let req_c = requests.clone();
            let req_kept = requests.clone();
            let name = name.to_string();
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    let Ok(mut stream) = conn else { continue };
                    let mut buf = Vec::new();
                    let mut b = [0u8; 4096];
                    loop {
                        match stream.read(&mut b) {
                            Ok(0) => break,
                            Ok(n) => {
                                buf.extend_from_slice(&b[..n]);
                                if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 65536 {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    let first = text.lines().next().unwrap_or_default().to_string();
                    let target = first
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_string();
                    req_c.lock().unwrap().push(target.clone());
                    let resp =
                        fake_source_respond(&name, &target, &files, &fail_files, size_inflate);
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.flush();
                }
            });
            Self {
                base: format!("http://{addr}"),
                requests: req_kept,
            }
        }

        fn source_url(&self, name: &str) -> String {
            format!("{}/api/v1/models/share/{name}?token=fake", self.base)
        }

        fn requested(&self, needle: &str) -> bool {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.contains(needle))
        }
    }

    fn fake_source_respond(
        name: &str,
        target: &str,
        files: &[(String, Vec<u8>)],
        fail_files: &[String],
        size_inflate: u64,
    ) -> String {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let q = |k: &str| -> Option<String> {
            query
                .split('&')
                .find_map(|kv| kv.strip_prefix(&format!("{k}=")))
                .map(String::from)
        };
        let json_resp = |code: u16, reason: &str, body: String| {
            format!(
                "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        };
        // detail 端点（清单）
        if path == format!("/api/v1/models/{name}/detail") {
            let files_json: Vec<String> = files
                .iter()
                .map(|(n, d)| {
                    format!(
                        r#"{{"name":"{n}","size_bytes":{}}}"#,
                        d.len() as u64 + size_inflate
                    )
                })
                .collect();
            return json_resp(
                200,
                "OK",
                format!(r#"{{"name":"{name}","files":[{}]}}"#, files_json.join(",")),
            );
        }
        // share 文件端点（offset/length 分段 + base64）
        let prefix = format!("/api/v1/models/share/{name}/");
        if let Some(rel) = path.strip_prefix(&prefix) {
            if fail_files.iter().any(|f| f == rel) {
                return json_resp(
                    404,
                    "Not Found",
                    format!(r#"{{"error":"{rel} disabled on this source"}}"#),
                );
            }
            let Some((_, data)) = files.iter().find(|(n, _)| n == rel) else {
                return json_resp(404, "Not Found", r#"{"error":"no such file"}"#.into());
            };
            let total = data.len() as u64;
            let offset: u64 = q("offset").and_then(|v| v.parse().ok()).unwrap_or(0);
            let length: u64 = q("length").and_then(|v| v.parse().ok()).unwrap_or(total);
            let start = (offset as usize).min(data.len());
            let end = ((offset + length) as usize).min(data.len());
            let chunk = &data[start..end];
            let b64 = base64::engine::general_purpose::STANDARD.encode(chunk);
            let eof = offset + chunk.len() as u64 >= total;
            let body = format!(
                r#"{{"ok":true,"name":"{name}","path":"{rel}","offset":{offset},"length":{},"total_size":{total},"eof":{eof},"content_base64":"{b64}"}}"#,
                chunk.len()
            );
            return json_resp(200, "OK", body);
        }
        json_resp(404, "Not Found", r#"{"error":"unmatched"}"#.into())
    }

    #[test]
    fn assign_files_round_robin_pure() {
        // 5 文件 2 源 → 0,1,0,1,0
        assert_eq!(assign_files_round_robin(5, 2), vec![0, 1, 0, 1, 0]);
        // 3 文件 1 源 → 全 0
        assert_eq!(assign_files_round_robin(3, 1), vec![0, 0, 0]);
        // 源多于文件 → 前几个源各拿一个
        assert_eq!(assign_files_round_robin(2, 4), vec![0, 1]);
        // 0 源 → 空
        assert!(assign_files_round_robin(3, 0).is_empty());
    }

    #[test]
    fn source_url_derivation_pure() {
        let src = "http://10.0.0.2:8080/api/v1/models/share/Qwen3?token=tk";
        assert_eq!(
            split_source_url(src),
            Some((
                "http".to_string(),
                "10.0.0.2:8080".to_string(),
                "tk".to_string()
            ))
        );
        assert_eq!(
            derive_detail_url(src, "Qwen3").unwrap(),
            "http://10.0.0.2:8080/api/v1/models/Qwen3/detail?token=tk"
        );
        assert_eq!(
            build_share_file_url(src, "Qwen3", "model.safetensors", 100, 50).unwrap(),
            "http://10.0.0.2:8080/api/v1/models/share/Qwen3/model.safetensors?token=tk&offset=100&length=50"
        );
        // 无 token 源
        let no_tok = "http://h:1/api/v1/models/share/M";
        assert_eq!(
            derive_detail_url(no_tok, "M").unwrap(),
            "http://h:1/api/v1/models/M/detail"
        );
        // 非法 URL
        assert!(split_source_url("ftp://x/y").is_none());
        assert!(split_source_url("not-a-url").is_none());
        assert!(derive_detail_url("garbage", "M").is_none());
    }

    #[test]
    fn resume_offset_pure() {
        assert_eq!(resume_offset_for(0, 100), 0);
        assert_eq!(resume_offset_for(40, 100), 40, "不足期望 → 续传");
        assert_eq!(resume_offset_for(99, 100), 99);
        assert_eq!(
            resume_offset_for(100, 100),
            0,
            "等于期望（越界等同损坏）→ 重下"
        );
        assert_eq!(resume_offset_for(150, 100), 0, "超过期望 → 重下");
    }

    #[tokio::test]
    async fn fetch_manifest_picks_first_reachable() {
        let name = "ManifestModel";
        let src = FakeSource::start(
            name,
            vec![
                ("a.safetensors".into(), vec![1u8; 10]),
                ("b.safetensors".into(), vec![2u8; 20]),
            ],
            vec![],
            0,
        );
        // 首源为死端口 → 回落到第二源
        let dead = "http://127.0.0.1:1/api/v1/models/share/ManifestModel?token=x";
        let (files, idx) =
            fetch_manifest_from_sources(&[dead.to_string(), src.source_url(name)], name)
                .await
                .expect("应回落到可达源");
        assert_eq!(idx, 1, "清单来自第二个源");
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0],
            ManifestFile {
                name: "a.safetensors".into(),
                size_bytes: 10
            }
        );
        assert_eq!(files[1].size_bytes, 20);
        // 全死 → Err
        assert!(fetch_manifest_from_sources(&[dead.to_string()], name)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn multi_post_all_sources_dead_returns_502() {
        let h = ModelHubRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/models/downloads",
                serde_json::json!({
                    "name": "DeadModel",
                    "sources": ["http://127.0.0.1:1/api/v1/models/share/DeadModel?token=x"]
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 502, "body: {}", resp.body);
        assert!(
            resp.body["error"].as_str().unwrap().contains("清单"),
            "错误应说明清单拉取失败: {}",
            resp.body
        );
        assert!(h.multi_tasks_snapshot().is_empty(), "失败任务不入列");
        // 非法 sources URL / 缺 name → 400
        let resp = h
            .handle(post_req(
                "/api/v1/models/downloads",
                serde_json::json!({"name": "M", "sources": ["garbage"]}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        let resp = h
            .handle(post_req(
                "/api/v1/models/downloads",
                serde_json::json!({"sources": ["http://127.0.0.1:1/x"]}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "缺 name 应 400");
    }

    /// 轮询任务到终态（completed/failed），返回 (status, 最后响应体)。
    async fn wait_multi_done(h: &ModelHubRouteHandler, id: &str) -> (String, serde_json::Value) {
        let mut last = serde_json::Value::Null;
        for _ in 0..400 {
            let r = h
                .handle(get_req(&format!("/api/v1/models/downloads/{id}")))
                .await
                .unwrap();
            last = r.body.clone();
            let s = last["status"].as_str().unwrap_or_default().to_string();
            if s == "completed" || s == "failed" {
                return (s, last);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        (String::new(), last)
    }

    #[tokio::test]
    async fn multi_download_e2e_dual_source_failover() {
        let name = "FakeVL";
        let f1 = (
            "model-00001-of-00002.safetensors".to_string(),
            vec![1u8; 300],
        );
        let f2 = (
            "model-00002-of-00002.safetensors".to_string(),
            vec![2u8; 5000],
        );
        let f3 = (
            "config.json".to_string(),
            b"{\"model_type\":\"fake\"}".to_vec(),
        );
        // 源 A（全量，清单源）：detail 列全 3 文件
        let src_a = FakeSource::start(name, vec![f1.clone(), f2.clone(), f3.clone()], vec![], 0);
        // 源 B（部分源）：detail 也在但故意 404 掉 f2（分配到 B 的文件强制换源回 A）
        let src_b = FakeSource::start(name, vec![f1.clone(), f3.clone()], vec![f2.0.clone()], 0);
        let root = temp_dir("multi-e2e");
        std::fs::create_dir_all(&root).unwrap();
        let h = ModelHubRouteHandler::with_empty().with_admin_token("tk");
        // 预插两条大厅行（两位分享者）验证完成后 download_count 归因
        {
            let conn = h.lobby_db.lock().unwrap();
            for sharer in ["peer-a", "peer-b"] {
                upsert_lobby_entry(
                    &conn,
                    &LobbyRow {
                        id: lobby_id(name, sharer),
                        name: name.into(),
                        display_name: name.into(),
                        description: String::new(),
                        tags: vec![],
                        arch: "fake".into(),
                        size_bytes: 0,
                        file_count: 0,
                        sharer: sharer.into(),
                        source_url: src_b.source_url(name),
                        created_at: now_iso(),
                        download_count: 0,
                    },
                    "tk",
                )
                .unwrap();
            }
        }
        let _g = ScopedModelsRoot::set(&root);
        // sources 顺序 = [A, B]：清单取自 A（首个可达源），轮转分配 f1→A f2→B f3→A
        let resp = h
            .handle(post_req(
                "/api/v1/models/downloads",
                serde_json::json!({
                    "name": name,
                    "sources": [src_a.source_url(name), src_b.source_url(name)]
                }),
            ))
            .await
            .unwrap();

        assert_eq!(resp.status, 201, "body: {}", resp.body);
        assert_eq!(resp.body["type"], "lobby_multi");
        assert_eq!(resp.body["files_total"], 3);
        let total: u64 = 300 + 5000 + 21;
        assert_eq!(resp.body["total_bytes"], total);
        let id = resp.body["id"].as_str().unwrap().to_string();

        let (status, last) = wait_multi_done(&h, &id).await;
        assert_eq!(status, "completed", "终态体: {last}");
        assert_eq!(last["files_done"], 3);
        assert_eq!(last["bytes_done"], total);
        assert!(last["active_sources"].as_array().unwrap().is_empty());
        // 简报：3 条 done + 至少 1 条 failed（f2 在源 B 的 404 尝试）= 4 条
        let recents = last["recent_files"].as_array().unwrap();
        let done_n = recents.iter().filter(|r| r["status"] == "done").count();
        let fail_n = recents.iter().filter(|r| r["status"] == "failed").count();
        assert_eq!(done_n, 3, "3 文件全 done: {recents:?}");
        assert!(fail_n >= 1, "f2 换源应留 failed 简报: {recents:?}");
        // 本地文件逐字节校验（完成校验已过，这里验证内容正确）
        let dir = root.join(name);
        assert_eq!(std::fs::read(dir.join(&f1.0)).unwrap(), f1.1);
        assert_eq!(std::fs::read(dir.join(&f2.0)).unwrap(), f2.1);
        assert_eq!(std::fs::read(dir.join(&f3.0)).unwrap(), f3.1);
        assert!(
            !dir.join(format!("{}.part", f1.0)).exists(),
            ".part 应已 rename"
        );
        // 换源确实发生：f2 先打到 B（404）再回落 A
        assert!(src_b.requested(&f2.0), "f2 应先在源 B 被尝试");
        assert!(src_a.requested(&f2.0), "f2 应回落到源 A");
        // 大厅归因：同 name 两位分享者 download_count 均 +1
        let rows = h.lobby_rows_snapshot();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter().all(|r| r.download_count == 1),
            "完成应给全体分享者计数: {rows:?}"
        );
        // 任务列表混排可见
        let list = h.handle(get_req("/api/v1/models/downloads")).await.unwrap();
        assert!(list.body.as_array().unwrap().iter().any(|t| t["id"] == *id));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn multi_download_resumes_existing_part() {
        let name = "ResumeModel";
        let f1 = (
            "big.safetensors".to_string(),
            (0..=250u8).cycle().take(1000).collect(),
        );
        let src = FakeSource::start(name, vec![f1.clone()], vec![], 0);
        let root = temp_dir("multi-resume");
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        // 预置 .part = 前 400 字节（模拟中断现场）
        std::fs::write(dir.join("big.safetensors.part"), &f1.1[..400]).unwrap();
        let h = ModelHubRouteHandler::with_empty();
        let _g = ScopedModelsRoot::set(&root);
        let resp = h
            .handle(post_req(
                "/api/v1/models/downloads",
                serde_json::json!({"name": name, "sources": [src.source_url(name)]}),
            ))
            .await
            .unwrap();

        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["bytes_done"], 400, "续传起点计入进度");
        let id = resp.body["id"].as_str().unwrap().to_string();
        let (status, last) = wait_multi_done(&h, &id).await;
        assert_eq!(status, "completed", "终态体: {last}");
        // 续传语义：服务端应收到 offset=400 的请求（未从 0 重下）
        assert!(
            src.requested("offset=400"),
            "应从 400 续传，实际请求: {:?}",
            src.requests.lock().unwrap()
        );
        assert_eq!(std::fs::read(dir.join("big.safetensors")).unwrap(), f1.1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn multi_download_size_mismatch_fails() {
        let name = "CorruptModel";
        let f1 = ("w.safetensors".to_string(), vec![9u8; 100]);
        // size_inflate=50：清单声明 150 字节，实际只有 100 → 分块长度不符 → 全源失败
        let src_a = FakeSource::start(name, vec![f1.clone()], vec![], 50);
        let src_b = FakeSource::start(name, vec![f1.clone()], vec![], 50);
        let root = temp_dir("multi-mismatch");
        std::fs::create_dir_all(&root).unwrap();
        let h = ModelHubRouteHandler::with_empty();
        let _g = ScopedModelsRoot::set(&root);
        let resp = h
            .handle(post_req(
                "/api/v1/models/downloads",
                serde_json::json!({
                    "name": name,
                    "sources": [src_a.source_url(name), src_b.source_url(name)]
                }),
            ))
            .await
            .unwrap();

        assert_eq!(resp.status, 201);
        let id = resp.body["id"].as_str().unwrap().to_string();
        let (status, last) = wait_multi_done(&h, &id).await;
        assert_eq!(status, "failed", "尺寸不符应失败: {last}");
        let err = last["error"].as_str().unwrap();
        assert!(
            err.contains("全源下载失败") || err.contains("校验失败"),
            "错误信息应指明失败文件: {err}"
        );
        assert_eq!(last["files_done"], 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn multi_task_lifecycle_get_delete() {
        let name = "LifeModel";
        let f1 = ("only.safetensors".to_string(), vec![5u8; 64]);
        let src = FakeSource::start(name, vec![f1.clone()], vec![], 0);
        let root = temp_dir("multi-life");
        std::fs::create_dir_all(&root).unwrap();
        let h = ModelHubRouteHandler::with_empty();
        let _g = ScopedModelsRoot::set(&root);
        let resp = h
            .handle(post_req(
                "/api/v1/models/downloads",
                serde_json::json!({"name": name, "sources": [src.source_url(name)]}),
            ))
            .await
            .unwrap();

        let id = resp.body["id"].as_str().unwrap().to_string();
        let (status, _) = wait_multi_done(&h, &id).await;
        assert_eq!(status, "completed");
        // 详情可查；取消（删除）后 404（与 modelscope 任务取消语义一致：移除）
        let got = h
            .handle(get_req(&format!("/api/v1/models/downloads/{id}")))
            .await
            .unwrap();
        assert_eq!(got.status, 200);
        assert_eq!(got.body["type"], "lobby_multi");
        let del = h
            .handle(del_req(&format!("/api/v1/models/downloads/{id}")))
            .await
            .unwrap();
        assert_eq!(del.status, 200);
        assert_eq!(del.body["type"], "lobby_multi");
        let gone = h
            .handle(get_req(&format!("/api/v1/models/downloads/{id}")))
            .await
            .unwrap();
        assert_eq!(gone.status, 404);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ====================================================================
    // D 面：在线仓库源（ModelScope / HF 镜像；纯函数 + 本地 TcpListener 假源）
    // ====================================================================

    /// 一组 env 键的 RAII 覆盖（与 ScopedModelsRoot 同一把 ENV_MUTEX，防并行串写）。
    /// 一次锁覆盖**多个**键（std Mutex 不可重入——逐键各拿一把会自锁死）。
    /// 元组字段仅用于持锁到 drop（永不读——RAII，同 ScopedModelsRoot 口径）。
    struct ScopedEnvs(
        Vec<&'static str>,
        #[allow(dead_code)] std::sync::MutexGuard<'static, ()>,
    );

    impl ScopedEnvs {
        /// `pairs`：[(key, value), …]；drop 时全部 remove_var。
        fn set(pairs: &[(&'static str, &str)]) -> Self {
            let g: std::sync::MutexGuard<'static, ()> =
                ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            for (k, v) in pairs {
                std::env::set_var(k, v);
            }
            Self(pairs.iter().map(|(k, _)| *k).collect(), g)
        }
    }

    impl Drop for ScopedEnvs {
        fn drop(&mut self) {
            for k in &self.0 {
                std::env::remove_var(k);
            }
        }
    }

    #[test]
    fn validate_repo_id_pure() {
        assert_eq!(
            validate_repo_id("Qwen/Qwen3-VL-8B-Instruct").unwrap(),
            ("Qwen".into(), "Qwen3-VL-8B-Instruct".into())
        );
        // 单段 / 三段 / 空段 / 非法字符 全拒
        assert!(validate_repo_id("standalone").is_err());
        assert!(validate_repo_id("a/b/c").is_err());
        assert!(validate_repo_id("a/").is_err());
        assert!(validate_repo_id("/b").is_err());
        assert!(validate_repo_id("../x").is_err());
        assert!(validate_repo_id("a/b c").is_err(), "空格拒绝");
        assert!(validate_repo_id("a/b?c").is_err(), "URL 元字符拒绝");
        assert!(validate_repo_id("a/b%2Fc").is_err(), "编码斜杠拒绝");
    }

    #[test]
    fn encode_path_pure() {
        assert_eq!(encode_path("model.safetensors"), "model.safetensors");
        assert_eq!(encode_path("sub dir/a b.bin"), "sub%20dir/a%20b.bin");
        assert_eq!(encode_path("中文/model.bin"), "%E4%B8%AD%E6%96%87/model.bin");
        assert_eq!(encode_path("a+b#c.bin"), "a%2Bb%23c.bin");
        assert_eq!(encode_path("x~y-z_9.txt"), "x~y-z_9.txt");
    }

    #[test]
    fn is_default_selected_pure() {
        // 权重 + config/tokenizer 系默认勾
        for f in [
            "model.safetensors",
            "model-00001-of-00002.safetensors",
            "pytorch_model.bin",
            "model.pt",
            "model.gguf",
            "config.json",
            "tokenizer_config.json",
            "merges.txt",
            "vocab.json",
            "sub/spiece.model",
            "tokenizer.model",
        ] {
            assert!(is_default_selected(f), "{f} 应默认勾选");
        }
        // README/LICENSE/git 元数据/图片 不勾
        for f in [
            "README.md",
            "LICENSE",
            ".gitattributes",
            ".gitignore",
            "preview.png",
            "assets/logo.jpeg",
            "paper.pdf",
        ] {
            assert!(!is_default_selected(f), "{f} 不应默认勾选");
        }
    }

    #[test]
    fn parse_modelscope_files_pure() {
        // 实测响应形态（2026-08-31 curl Qwen/Qwen2.5-0.5B-Instruct，截取）
        let body: serde_json::Value = serde_json::json!({
            "Code": 200,
            "Data": {
                "Files": [
                    {"Name": "sub", "Path": "sub", "Size": 0, "Type": "tree"},
                    {"Name": "b.bin", "Path": "sub/b.bin", "Size": 200, "Type": "blob", "Sha256": "ff"},
                    {"Name": "a.safetensors", "Path": "a.safetensors", "Size": 988097824, "Type": "blob"},
                    {"Name": "config.json", "Path": "config.json", "Size": 659, "Type": "blob"}
                ]
            }
        });
        let files = parse_modelscope_files(&body).unwrap();
        assert_eq!(files.len(), 3, "tree 目录条目应滤除: {files:?}");
        assert_eq!(files[0].name, "a.safetensors", "按路径排序");
        assert_eq!(files[0].size_bytes, 988097824);
        assert_eq!(files[2].name, "sub/b.bin", "子目录文件保留相对路径");
        // Code != 200（私有/不存在）
        let err = parse_modelscope_files(&serde_json::json!({
            "Code": 10010205001_i64, "Message": "获取模型信息失败，信息：record not found", "Success": false
        }))
        .unwrap_err();
        assert!(err.contains("record not found"), "带上游 Message: {err}");
        // 缺 Data.Files / 空清单
        assert!(parse_modelscope_files(&serde_json::json!({"Code": 200})).is_err());
        assert!(
            parse_modelscope_files(&serde_json::json!({"Code": 200, "Data": {"Files": []}}))
                .is_err()
        );
    }

    #[test]
    fn parse_hf_tree_pure() {
        // 实测响应形态（2026-08-31 curl hf-mirror.com .../tree/main?recursive=true，截取）
        let body: serde_json::Value = serde_json::json!([
            {"type": "directory", "path": "sub", "size": 0},
            {"type": "file", "path": "sub/b.bin", "size": 200, "lfs": {"size": 200}},
            {"type": "file", "path": "a.safetensors", "size": 988097824, "lfs": {"size": 988097824}},
            {"type": "file", "path": "config.json", "size": 659}
        ]);
        let files = parse_hf_tree(&body).unwrap();
        assert_eq!(files.len(), 3, "directory 条目应滤除: {files:?}");
        assert_eq!(files[0].name, "a.safetensors");
        assert_eq!(files[0].size_bytes, 988097824, "LFS 逻辑大小取顶层 size");
        // 非数组（404 页/错误对象）→ Err
        assert!(parse_hf_tree(&serde_json::json!({"error": "Repository not found"})).is_err());
        assert!(parse_hf_tree(&serde_json::json!([])).is_err(), "空清单 Err");
    }

    #[test]
    fn remote_repo_kind_parse_and_slug() {
        assert_eq!(RemoteRepoKind::parse("modelscope"), Some(RemoteRepoKind::Modelscope));
        assert_eq!(RemoteRepoKind::parse("ModelScope"), Some(RemoteRepoKind::Modelscope));
        assert_eq!(RemoteRepoKind::parse("hf"), Some(RemoteRepoKind::HfMirror));
        assert_eq!(RemoteRepoKind::parse("hf_mirror"), Some(RemoteRepoKind::HfMirror));
        assert_eq!(RemoteRepoKind::parse("github"), None);
        assert_eq!(RemoteRepoKind::Modelscope.slug(), "modelscope");
        assert_eq!(RemoteRepoKind::HfMirror.slug(), "hf");
    }

    /// 假在线仓库源（std TcpListener + 线程）：同时实现 ModelScope 与 HF 镜像
    /// 两套端点（files/tree JSON + resolve 二进制，resolve 认真实现 Range: bytes=a-b）。
    struct FakeRemoteRepo {
        base: String,
        heads: Arc<Mutex<Vec<String>>>,
    }

    impl FakeRemoteRepo {
        /// `files`：(相对路径, 字节)；`not_found`：resolve 时 404 的路径。
        fn start(files: Vec<(String, Vec<u8>)>, not_found: Vec<String>) -> Self {
            use std::io::{Read, Write};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let heads = Arc::new(Mutex::new(Vec::new()));
            let heads_c = heads.clone();
            let heads_k = heads.clone();
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    let Ok(mut stream) = conn else { continue };
                    let mut buf = Vec::new();
                    let mut b = [0u8; 8192];
                    loop {
                        match stream.read(&mut b) {
                            Ok(0) => break,
                            Ok(n) => {
                                buf.extend_from_slice(&b[..n]);
                                if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 65536 {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let head = String::from_utf8_lossy(&buf).into_owned();
                    heads_c.lock().unwrap().push(head.clone());
                    let resp = fake_remote_respond(&head, &files, &not_found);
                    let _ = stream.write_all(&resp);
                    let _ = stream.flush();
                }
            });
            Self {
                base: format!("http://{addr}"),
                heads: heads_k,
            }
        }

        /// 收到的原始请求头里是否有 needle（断言 Range 格式用）。
        fn saw(&self, needle: &str) -> bool {
            self.heads.lock().unwrap().iter().any(|h| h.contains(needle))
        }
    }

    /// 解析请求头 → 响应字节（files/tree 端点 + resolve 端点带 Range）。
    fn fake_remote_respond(
        head: &str,
        files: &[(String, Vec<u8>)],
        not_found: &[String],
    ) -> Vec<u8> {
        let target = head.lines().next().unwrap_or_default();
        let target = target.split_whitespace().nth(1).unwrap_or_default();
        let (path, _query) = target.split_once('?').unwrap_or((target, ""));
        let range = head.lines().find_map(|l| {
            let lower = l.to_lowercase();
            lower
                .strip_prefix("range:")
                .map(|v| v.trim().to_string())
        });
        let json_ok = |body: String| {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .into_bytes()
        };
        // ModelScope files / HF tree 端点：files 空视为仓库不存在（真实上游对
        // 不存在/私有仓库回 HTTP 404——这里同型，触发后端"不存在"归一分支）
        if (path.contains("/repo/files") || path.contains("/tree/main")) && files.is_empty() {
            return b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec();
        }
        // ModelScope files 端点（含一个 tree 目录条目验证过滤）
        if path.contains("/repo/files") {
            let mut entries = vec![r#"{"Name":"sub","Path":"sub","Size":0,"Type":"tree"}"#.to_string()];
            entries.extend(files.iter().map(|(n, d)| {
                format!(r#"{{"Name":"{n}","Path":"{n}","Size":{},"Type":"blob"}}"#, d.len())
            }));
            return json_ok(format!(
                r#"{{"Code":200,"Data":{{"Files":[{}]}}}}"#,
                entries.join(",")
            ));
        }
        // HF 镜像 tree 端点
        if path.contains("/tree/main") {
            let arr: Vec<String> = files
                .iter()
                .map(|(n, d)| format!(r#"{{"type":"file","path":"{n}","size":{}}}"#, d.len()))
                .collect();
            return json_ok(format!("[{}]", arr.join(",")));
        }
        // resolve 端点（两协议同型：/<repo>/resolve/<rev>/<rel>）
        if let Some(pos) = path.find("/resolve/") {
            let rel_raw = &path[pos + "/resolve/".len()..];
            // rel_raw 形如 "master/sub/a.bin"：去掉修订段，其余整段是相对路径
            let rel = rel_raw.split_once('/').map_or("", |(_, r)| r);
            let rel = percent_decode(rel);
            if not_found.contains(&rel) || rel.is_empty() {
                return b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec();
            }
            let Some((_, data)) = files.iter().find(|(n, _)| *n == rel) else {
                return b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec();
            };
            // Range: bytes=a-b（闭区间）；无 Range 回全量
            let (start, end) = match range
                .as_deref()
                .and_then(|r| r.strip_prefix("bytes="))
                .and_then(|spec| spec.split_once('-'))
            {
                Some((a, b)) => (
                    a.parse::<usize>().unwrap_or(0),
                    b.parse::<usize>().unwrap_or(0),
                ),
                None => (0, data.len().saturating_sub(1)),
            };
            let start = start.min(data.len());
            let end = (end + 1).min(data.len()).max(start);
            let chunk = &data[start..end];
            let mut resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                chunk.len(),
                start,
                end.saturating_sub(1),
                data.len()
            )
            .into_bytes();
            resp.extend_from_slice(chunk);
            return resp;
        }
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
    }

    /// 极简 percent-decode（测试假源用；对 encode_path 产的 %XX 还原）。
    fn percent_decode(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' && i + 2 < b.len() {
                if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
            }
            out.push(b[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    #[tokio::test]
    async fn remote_probe_modelscope_and_hf_mock() {
        let fake = FakeRemoteRepo::start(
            vec![
                ("model.safetensors".into(), vec![7u8; 300]),
                ("config.json".into(), br#"{"model_type":"fake"}"#.to_vec()),
                ("README.md".into(), b"# readme".to_vec()),
            ],
            vec![],
        );
        // 一把锁同时覆盖两源 base（std Mutex 不可重入，不可各拿一把）
        let envs = ScopedEnvs::set(&[
            ("NEXOS_MODELSCOPE_BASE", fake.base.as_str()),
            ("NEXOS_HF_BASE", fake.base.as_str()),
        ]);
        // ModelScope 探测：blob 条目 + tree 条目滤除 + 默认勾选标记
        let probe = probe_remote_repo(RemoteRepoKind::Modelscope, "Org/FakeModel")
            .await
            .expect("探测应成功");
        assert_eq!(probe.kind, "modelscope");
        assert_eq!(probe.repo_id, "Org/FakeModel");
        assert_eq!(probe.name, "FakeModel", "本地目录名 = repo 末段");
        assert_eq!(probe.file_count, 3);
        assert_eq!(probe.total_size_bytes, 300 + 21 + 8);
        let by_name = |n: &str| probe.files.iter().find(|f| f.name == n).unwrap().clone();
        assert!(by_name("model.safetensors").default_selected);
        assert!(by_name("config.json").default_selected);
        assert!(!by_name("README.md").default_selected, "README 不默认勾");
        // HF 镜像探测：同一假源 tree 端点
        let hf = probe_remote_repo(RemoteRepoKind::HfMirror, "Org/FakeModel")
            .await
            .expect("HF 探测应成功");
        assert_eq!(hf.kind, "hf");
        assert_eq!(hf.file_count, 3);
        // 上游 404（假源不认识的路径）→ Err 提示不存在
        drop(envs);
        let fake404 = FakeRemoteRepo::start(vec![], vec![]);
        let _envs2 = ScopedEnvs::set(&[("NEXOS_MODELSCOPE_BASE", fake404.base.as_str())]);
        let err = probe_remote_repo(RemoteRepoKind::Modelscope, "Org/NoSuchModel")
            .await
            .unwrap_err();
        assert!(err.contains("不存在"), "404 应归一为不存在: {err}");
    }

    #[tokio::test]
    async fn remote_probe_get_endpoint_routing() {
        let fake = FakeRemoteRepo::start(
            vec![("config.json".into(), b"{}".to_vec())],
            vec![],
        );
        let envs = ScopedEnvs::set(&[("NEXOS_MODELSCOPE_BASE", fake.base.as_str())]);
        let h = ModelHubRouteHandler::with_empty();
        // 正常探测（GET 公开读）
        let resp = h
            .handle(get_req("/api/v1/models/remote/modelscope/Org/FakeModel"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "body: {}", resp.body);
        assert_eq!(resp.body["kind"], "modelscope");
        assert_eq!(resp.body["file_count"], 1);
        assert_eq!(resp.body["files"][0]["name"], "config.json");
        // kind 非法 → 400
        let resp = h
            .handle(get_req("/api/v1/models/remote/github/Org/FakeModel"))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // repo_id 非法（三段）→ 路由形态即不匹配（:kind/:org/:model 恰三段）→ 404；
        // 深层校验（validate_repo_id 400）在 POST 创建路径与探测函数内生效
        let resp = h
            .handle(get_req("/api/v1/models/remote/modelscope/a/b/c"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
        // 仓库不存在 → 404
        drop(envs);
        let fake404 = FakeRemoteRepo::start(vec![], vec![]);
        let _envs2 = ScopedEnvs::set(&[("NEXOS_MODELSCOPE_BASE", fake404.base.as_str())]);
        let resp = h
            .handle(get_req("/api/v1/models/remote/modelscope/Org/Gone"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404, "body: {}", resp.body);
    }

    /// 轮询 remote_repo 任务到终态（重试路径含 2s+4s 退避，比 wait_multi_done 宽裕）。
    async fn wait_remote_done(h: &ModelHubRouteHandler, id: &str) -> (String, serde_json::Value) {
        let mut last = serde_json::Value::Null;
        for _ in 0..1500 {
            let r = h
                .handle(get_req(&format!("/api/v1/models/downloads/{id}")))
                .await
                .unwrap();
            last = r.body.clone();
            let s = last["status"].as_str().unwrap_or_default().to_string();
            if s == "completed" || s == "failed" {
                return (s, last);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        (String::new(), last)
    }

    #[tokio::test]
    async fn remote_download_e2e_selected_files_range_resume() {
        let f1 = ("model.safetensors".to_string(), (0..=255u8).cycle().take(1200).collect::<Vec<u8>>());
        let f2 = ("config.json".to_string(), br#"{"model_type":"fake"}"#.to_vec());
        let f3 = ("README.md".to_string(), b"# readme doc".to_vec());
        let fake = FakeRemoteRepo::start(vec![f1.clone(), f2.clone(), f3.clone()], vec![]);
        let root = temp_dir("remote-e2e");
        std::fs::create_dir_all(&root).unwrap();
        let h = ModelHubRouteHandler::with_empty();
        // 一把锁同时覆盖 models 根 + 源 base（ScopedModelsRoot 也持 ENV_MUTEX，不可叠加）
        let _envs = ScopedEnvs::set(&[
            ("NEXOS_MODELS_DIR", root.to_str().unwrap()),
            ("NEXOS_MODELSCOPE_BASE", fake.base.as_str()),
        ]);
        // 预置 .part = f1 前 500 字节（模拟中断现场 → 断点续传起点）
        std::fs::create_dir_all(root.join("FakeModel")).unwrap();
        std::fs::write(root.join("FakeModel/model.safetensors.part"), &f1.1[..500]).unwrap();
        // 勾选 f1 + f2（README 不下）
        let resp = h
            .handle(post_req(
                "/api/v1/models/remote/downloads",
                serde_json::json!({
                    "kind": "modelscope",
                    "repo_id": "Org/FakeModel",
                    "files": [f1.0, f2.0]
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "body: {}", resp.body);
        assert_eq!(resp.body["type"], "remote_repo");
        assert_eq!(resp.body["kind"], "modelscope");
        assert_eq!(resp.body["repo_id"], "Org/FakeModel");
        assert_eq!(resp.body["name"], "FakeModel");
        assert_eq!(resp.body["files_total"], 2, "只下勾选的 2 个文件");
        assert_eq!(resp.body["total_bytes"], 1200 + 21);
        assert_eq!(resp.body["bytes_done"], 500, ".part 续传起点计入进度");
        let id = resp.body["id"].as_str().unwrap().to_string();
        let (status, last) = wait_remote_done(&h, &id).await;
        assert_eq!(status, "completed", "终态体: {last}");
        assert_eq!(last["files_done"], 2);
        assert_eq!(last["bytes_done"], 1200 + 21);
        // 内容逐字节校验 + 原子落位（无 .part 残留）
        let dir = root.join("FakeModel");
        assert_eq!(std::fs::read(dir.join(&f1.0)).unwrap(), f1.1);
        assert_eq!(std::fs::read(dir.join(&f2.0)).unwrap(), f2.1);
        assert!(!dir.join("README.md").exists(), "未勾选文件不应落盘");
        assert!(!dir.join(format!("{}.part", f1.0)).exists());
        // 续传：f1 首个 Range 应从 500 起（bounded bytes=500-1199；
        // reqwest/hyper 发小写头名 `range:`，断言不区分大小写子串）
        assert!(
            fake.saw("bytes=500-1199"),
            "应发 bounded Range 从 500 续传: {:?}",
            fake.heads.lock().unwrap().first()
        );
        // 任务在混排列表可见
        let list = h.handle(get_req("/api/v1/models/downloads")).await.unwrap();
        assert!(list.body.as_array().unwrap().iter().any(|t| t["id"] == *id));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn remote_download_file_retry_then_fail() {
        // 一个 3 次重试都 404 的文件 → 任务 failed 且错误指明文件
        let f1 = ("model.safetensors".to_string(), vec![1u8; 64]);
        let fake = FakeRemoteRepo::start(vec![f1.clone()], vec![f1.0.clone()]);
        let root = temp_dir("remote-404");
        std::fs::create_dir_all(&root).unwrap();
        let h = ModelHubRouteHandler::with_empty();
        let _envs = ScopedEnvs::set(&[
            ("NEXOS_MODELS_DIR", root.to_str().unwrap()),
            ("NEXOS_MODELSCOPE_BASE", fake.base.as_str()),
        ]);
        let resp = h
            .handle(post_req(
                "/api/v1/models/remote/downloads",
                serde_json::json!({"kind": "modelscope", "repo_id": "Org/Flaky"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        let id = resp.body["id"].as_str().unwrap().to_string();
        let (status, last) = wait_remote_done(&h, &id).await;
        assert_eq!(status, "failed", "重试耗尽应失败: {last}");
        let err = last["error"].as_str().unwrap();
        assert!(err.contains("model.safetensors") && err.contains("3 次"), "错误应指明文件与次数: {err}");
        assert_eq!(last["files_done"], 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    // —— 并发数解析（纯函数）：缺省 3 / 非法回缺省 / <1 收敛 1 / >8 收敛 8 ——

    #[test]
    fn remote_dl_concurrency_parse_pure() {
        assert_eq!(remote_dl_concurrency(""), 3, "缺省 3");
        assert_eq!(remote_dl_concurrency("  "), 3);
        assert_eq!(remote_dl_concurrency("abc"), 3, "非法回缺省");
        assert_eq!(remote_dl_concurrency("3"), 3);
        assert_eq!(remote_dl_concurrency(" 4 "), 4, "容忍空白");
        assert_eq!(remote_dl_concurrency("1"), 1);
        assert_eq!(remote_dl_concurrency("0"), 1, "0 收敛 1");
        assert_eq!(remote_dl_concurrency("8"), 8);
        assert_eq!(remote_dl_concurrency("9"), 8, "上限 8");
        assert_eq!(remote_dl_concurrency("9999"), 8);
    }

    // —— 失败隔离：并行下某文件重试耗尽不拖垮其他（成功件保留落盘）——

    #[tokio::test]
    async fn remote_download_parallel_failure_isolation() {
        let fa = ("a.safetensors".to_string(), vec![1u8; 100]);
        let fb = ("b.safetensors".to_string(), vec![2u8; 64]); // resolve 恒 404
        let fc = ("c.json".to_string(), br#"{"k":1}"#.to_vec());
        let fake = FakeRemoteRepo::start(
            vec![fa.clone(), fb.clone(), fc.clone()],
            vec![fb.0.clone()], // b 在 not_found 名单
        );
        let root = temp_dir("remote-iso");
        std::fs::create_dir_all(&root).unwrap();
        let h = ModelHubRouteHandler::with_empty();
        let _envs = ScopedEnvs::set(&[
            ("NEXOS_MODELS_DIR", root.to_str().unwrap()),
            ("NEXOS_MODELSCOPE_BASE", fake.base.as_str()),
        ]);
        let resp = h
            .handle(post_req(
                "/api/v1/models/remote/downloads",
                serde_json::json!({"kind": "modelscope", "repo_id": "Org/Iso"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        let id = resp.body["id"].as_str().unwrap().to_string();
        let (status, last) = wait_remote_done(&h, &id).await;
        assert_eq!(status, "failed", "有失败文件整体应 failed: {last}");
        let err = last["error"].as_str().unwrap();
        assert!(
            err.contains("b.safetensors") && err.contains("3 次"),
            "错误应指明失败文件与次数: {err}"
        );
        // 失败隔离：另两个文件照常完成且保留（内容逐字节 + 无 .part 残留）
        assert_eq!(last["files_done"], 2, "files_done 只数成功件: {last}");
        let dir = root.join("Iso");
        assert_eq!(std::fs::read(dir.join(&fa.0)).unwrap(), fa.1, "成功件 a 保留");
        assert_eq!(std::fs::read(dir.join(&fc.0)).unwrap(), fc.1, "成功件 c 保留");
        assert!(!dir.join(format!("{}.part", fa.0)).exists());
        assert!(!dir.join(format!("{}.part", fc.0)).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 从假源记录的原始请求头里按序提取 resolve GET 路径（顺序断言用）。
    fn resolve_get_paths(heads: &[String]) -> Vec<String> {
        heads
            .iter()
            .filter_map(|h| {
                let first = h.lines().next()?;
                if first.starts_with("GET ") && first.contains("/resolve/") {
                    Some(first.split_whitespace().nth(1)?.to_string())
                } else {
                    None
                }
            })
            .collect()
    }

    // —— 并发=1：与旧顺序实现行为一致（resolve 请求严格按清单顺序）——

    #[tokio::test]
    async fn remote_download_concurrency_one_sequential_order() {
        let files = vec![
            ("a.safetensors".to_string(), vec![1u8; 50]),
            ("b.safetensors".to_string(), vec![2u8; 60]),
            ("c.json".to_string(), vec![3u8; 70]),
        ];
        let fake = FakeRemoteRepo::start(files.clone(), vec![]);
        let root = temp_dir("remote-seq");
        std::fs::create_dir_all(&root).unwrap();
        let h = ModelHubRouteHandler::with_empty();
        let _envs = ScopedEnvs::set(&[
            ("NEXOS_MODELS_DIR", root.to_str().unwrap()),
            ("NEXOS_MODELSCOPE_BASE", fake.base.as_str()),
            ("NEXOS_MODELHUB_DL_CONCURRENCY", "1"),
        ]);
        let resp = h
            .handle(post_req(
                "/api/v1/models/remote/downloads",
                serde_json::json!({"kind": "modelscope", "repo_id": "Org/Seq"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        let id = resp.body["id"].as_str().unwrap().to_string();
        let (status, last) = wait_remote_done(&h, &id).await;
        assert_eq!(status, "completed", "{last}");
        assert_eq!(last["files_done"], 3);
        // 顺序门：单文件 16MiB 内一块完成 → 每文件恰一个 resolve，顺序 = 清单序
        let order = resolve_get_paths(&fake.heads.lock().unwrap());
        assert_eq!(
            order,
            vec![
                "/Org/Seq/resolve/master/a.safetensors",
                "/Org/Seq/resolve/master/b.safetensors",
                "/Org/Seq/resolve/master/c.json",
            ],
            "并发=1 必须严格按清单顺序逐文件: {order:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 并发观测假源：与 [`FakeRemoteRepo`] 同协议（复用 [`fake_remote_respond`]），
    /// 但每连接独立线程 + 固定 150ms 响应延迟，并记录**同时在线连接峰值**
    ///（探测/下载每请求一条连接——`Connection: close`）。
    struct GatedRemoteRepo {
        base: String,
        peak_inflight: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl GatedRemoteRepo {
        fn start(files: Vec<(String, Vec<u8>)>) -> Self {
            use std::io::{Read, Write};
            use std::sync::atomic::{AtomicUsize, Ordering};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let files = Arc::new(files);
            let peak = Arc::new(AtomicUsize::new(0));
            let inflight = Arc::new(AtomicUsize::new(0));
            let peak_k = Arc::clone(&peak);
            let inflight_k = Arc::clone(&inflight);
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    let Ok(mut stream) = conn else { continue };
                    let files = Arc::clone(&files);
                    let peak = Arc::clone(&peak_k);
                    let inflight = Arc::clone(&inflight_k);
                    std::thread::spawn(move || {
                        let mut buf = Vec::new();
                        let mut b = [0u8; 8192];
                        loop {
                            match stream.read(&mut b) {
                                Ok(0) => break,
                                Ok(n) => {
                                    buf.extend_from_slice(&b[..n]);
                                    if buf.windows(4).any(|w| w == b"\r\n\r\n")
                                        || buf.len() > 65536
                                    {
                                        break;
                                    }
                                }
                                Err(_) => return,
                            }
                        }
                        let head = String::from_utf8_lossy(&buf).into_owned();
                        let now = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(150));
                        let resp = fake_remote_respond(&head, &files, &[]);
                        let _ = stream.write_all(&resp);
                        let _ = stream.flush();
                        inflight.fetch_sub(1, Ordering::SeqCst);
                    });
                }
            });
            Self {
                base: format!("http://{addr}"),
                peak_inflight: peak,
            }
        }
    }

    // —— 并发可观测：并发 3 → 同时在线连接 ≥2；并发 1 → 峰值恰 1 ——

    #[tokio::test]
    async fn remote_download_parallel_inflight_observed() {
        let files = vec![
            ("a.safetensors".to_string(), vec![1u8; 64]),
            ("b.safetensors".to_string(), vec![2u8; 64]),
            ("c.json".to_string(), vec![3u8; 64]),
        ];
        let gated = GatedRemoteRepo::start(files.clone());
        let root = temp_dir("remote-par");
        std::fs::create_dir_all(&root).unwrap();
        let h = ModelHubRouteHandler::with_empty();
        let _envs = ScopedEnvs::set(&[
            ("NEXOS_MODELS_DIR", root.to_str().unwrap()),
            ("NEXOS_MODELSCOPE_BASE", gated.base.as_str()),
            ("NEXOS_MODELHUB_DL_CONCURRENCY", "3"),
        ]);
        let resp = h
            .handle(post_req(
                "/api/v1/models/remote/downloads",
                serde_json::json!({"kind": "modelscope", "repo_id": "Org/Par"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        let id = resp.body["id"].as_str().unwrap().to_string();
        let (status, last) = wait_remote_done(&h, &id).await;
        assert_eq!(status, "completed", "{last}");
        assert_eq!(last["files_done"], 3);
        assert!(
            gated.peak_inflight.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "并发 3 应观测到 ≥2 条同时在线连接（文件级并行未生效？）"
        );
        // 并行不破坏内容正确性
        let dir = root.join("Par");
        for (name, data) in &files {
            assert_eq!(std::fs::read(dir.join(name)).unwrap(), *data, "{name}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn remote_download_concurrency_one_inflight_peak_is_one() {
        let files = vec![
            ("a.safetensors".to_string(), vec![1u8; 64]),
            ("b.safetensors".to_string(), vec![2u8; 64]),
            ("c.json".to_string(), vec![3u8; 64]),
        ];
        let gated = GatedRemoteRepo::start(files);
        let root = temp_dir("remote-par1");
        std::fs::create_dir_all(&root).unwrap();
        let h = ModelHubRouteHandler::with_empty();
        let _envs = ScopedEnvs::set(&[
            ("NEXOS_MODELS_DIR", root.to_str().unwrap()),
            ("NEXOS_MODELSCOPE_BASE", gated.base.as_str()),
            ("NEXOS_MODELHUB_DL_CONCURRENCY", "1"),
        ]);
        let resp = h
            .handle(post_req(
                "/api/v1/models/remote/downloads",
                serde_json::json!({"kind": "modelscope", "repo_id": "Org/Par1"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        let id = resp.body["id"].as_str().unwrap().to_string();
        let (status, last) = wait_remote_done(&h, &id).await;
        assert_eq!(status, "completed", "{last}");
        assert_eq!(
            gated.peak_inflight.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "并发=1 时峰值恰 1（连接 = 旧顺序行为）"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn remote_download_validation_rejections() {
        let fake = FakeRemoteRepo::start(
            vec![("config.json".into(), b"{}".to_vec())],
            vec![],
        );
        let envs = ScopedEnvs::set(&[("NEXOS_MODELSCOPE_BASE", fake.base.as_str())]);
        let h = ModelHubRouteHandler::with_empty();
        // kind 非法 / repo_id 非法 → 400
        let resp = h
            .handle(post_req(
                "/api/v1/models/remote/downloads",
                serde_json::json!({"kind": "gitlab", "repo_id": "a/b"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        let resp = h
            .handle(post_req(
                "/api/v1/models/remote/downloads",
                serde_json::json!({"kind": "modelscope", "repo_id": "not-org-model"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // 清单外文件路径 → 400
        let resp = h
            .handle(post_req(
                "/api/v1/models/remote/downloads",
                serde_json::json!({"kind": "modelscope", "repo_id": "Org/Fake", "files": ["no-such.bin"]}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "body: {}", resp.body);
        assert!(resp.body["error"].as_str().unwrap().contains("no-such.bin"));
        // 空 files 数组 → 400
        let resp = h
            .handle(post_req(
                "/api/v1/models/remote/downloads",
                serde_json::json!({"kind": "modelscope", "repo_id": "Org/Fake", "files": []}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // 探测不可达 → 502 任务不入列（先释放上一把锁再换死端口 base）
        drop(envs);
        let _dead = ScopedEnvs::set(&[("NEXOS_MODELSCOPE_BASE", "http://127.0.0.1:1")]);
        let resp = h
            .handle(post_req(
                "/api/v1/models/remote/downloads",
                serde_json::json!({"kind": "modelscope", "repo_id": "Org/Dead"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 502);
        assert!(h.remote_tasks_snapshot().is_empty(), "失败任务不入列");
    }

    #[tokio::test]
    async fn remote_task_cancel_lifecycle() {
        // 大文件 + 慢速不需要：创建后立刻 DELETE = 取消，任务从列表消失
        let f1 = ("model.safetensors".to_string(), vec![9u8; 4 * 1024 * 1024]);
        let fake = FakeRemoteRepo::start(vec![f1.clone()], vec![]);
        let root = temp_dir("remote-cancel");
        std::fs::create_dir_all(&root).unwrap();
        let h = ModelHubRouteHandler::with_empty();
        let _envs = ScopedEnvs::set(&[
            ("NEXOS_MODELS_DIR", root.to_str().unwrap()),
            ("NEXOS_MODELSCOPE_BASE", fake.base.as_str()),
        ]);
        let resp = h
            .handle(post_req(
                "/api/v1/models/remote/downloads",
                serde_json::json!({"kind": "modelscope", "repo_id": "Org/Big"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        let id = resp.body["id"].as_str().unwrap().to_string();
        // 立刻取消：runner 在分块边界感知（列表移除 = abort）
        let del = h
            .handle(del_req(&format!("/api/v1/models/downloads/{id}")))
            .await
            .unwrap();
        assert_eq!(del.status, 200);
        assert_eq!(del.body["type"], "remote_repo");
        // 轮询到任务从详情端点消失（runner 收摊移除或已移除 → 404）
        let mut gone = false;
        for _ in 0..400 {
            let got = h
                .handle(get_req(&format!("/api/v1/models/downloads/{id}")))
                .await
                .unwrap();
            if got.status == 404 {
                gone = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(gone, "取消后任务应 404");
        let _ = std::fs::remove_dir_all(&root);
    }

    // ====================================================================
    // E 面：Spark 专区（策展清单 / env 覆盖 / 探测语义 / 端点契约）
    // ====================================================================

    #[test]
    fn builtin_spark_zone_entries_valid_and_serializable() {
        let entries = builtin_spark_zone_entries();
        assert!(entries.len() >= 5, "策展表应含至少 5 条（实测收录）: {}", entries.len());
        for e in &entries {
            assert!(validate_repo_id(&e.repo).is_ok(), "repo 形态须合法: {}", e.repo);
            assert_eq!(e.quant, "NVFP4", "专区条目恒 NVFP4: {}", e.repo);
            assert_eq!(e.org, e.repo.split('/').next().unwrap_or_default());
            assert!(!e.params.is_empty() && !e.note.is_empty(), "参数量/简述非空: {}", e.repo);
        }
        // 用户点名的两个仓必须在策展表（2026-09-02 双端实探存在）
        let repos: Vec<&str> = entries.iter().map(|e| e.repo.as_str()).collect();
        assert!(repos.contains(&"nv-community/Qwen3.6-27B-NVFP4"), "缺用户点名仓: {repos:?}");
        assert!(repos.contains(&"unsloth/Qwen3.8-27B-NVFP4"), "缺用户点名仓: {repos:?}");
        // 无重复 repo
        let mut sorted = repos.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), repos.len(), "策展表 repo 不应重复");
        // 序列化 roundtrip（env 文件与响应同构）
        let json = serde_json::to_string(&entries).unwrap();
        let back: Vec<SparkZoneEntry> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), entries.len());
        assert_eq!(back[0].repo, entries[0].repo);
        assert_eq!(back[0].org, entries[0].org);
        assert_eq!(back[0].note, entries[0].note);
    }

    #[test]
    fn merge_spark_zone_entries_override_and_append() {
        let mk = |repo: &str, note: &str| SparkZoneEntry {
            repo: repo.into(),
            org: repo.split('/').next().unwrap_or_default().into(),
            quant: "NVFP4".into(),
            params: "27B".into(),
            note: note.into(),
        };
        let builtin = vec![mk("a/One", "内置一"), mk("a/Two", "内置二")];
        // env 覆盖同 repo +追加新 repo：顺序 = 内置序 + env 新条目原序
        let env = vec![mk("b/New", "新增"), mk("a/One", "运营改写")];
        let merged = merge_spark_zone_entries(builtin.clone(), env);
        assert_eq!(merged.len(), 3, "去重以 repo 为键");
        assert_eq!(
            merged.iter().map(|e| e.repo.as_str()).collect::<Vec<_>>(),
            vec!["a/One", "a/Two", "b/New"],
            "覆盖保位、追加殿后"
        );
        assert_eq!(merged[0].note, "运营改写", "env 同名整条覆盖");
        // 空 env = 原表
        assert_eq!(
            merge_spark_zone_entries(builtin.clone(), vec![]).len(),
            builtin.len()
        );
    }

    #[test]
    fn parse_spark_zone_env_shapes() {
        let entry_json = r#"{"repo":"a/One","org":"a","quant":"NVFP4","params":"27B","note":"n"}"#;
        // 数组 → 合并语义
        let (es, replace) = parse_spark_zone_env(&format!("[{entry_json}]")).unwrap();
        assert!(!replace && es.len() == 1 && es[0].repo == "a/One");
        // 对象 {"replace": […]} → 替换语义
        let (es, replace) =
            parse_spark_zone_env(&format!(r#"{{"replace":[{entry_json}]}}"#)).unwrap();
        assert!(replace && es.len() == 1);
        // 非法形态：坏 JSON / 对象缺 replace / replace 非数组 / 顶层标量 / 字段缺失
        assert!(parse_spark_zone_env("not json").is_err());
        assert!(parse_spark_zone_env("{}").is_err());
        assert!(parse_spark_zone_env(r#"{"replace":42}"#).is_err());
        assert!(parse_spark_zone_env("7").is_err());
        assert!(parse_spark_zone_env(r#"[{"repo":"a/One"}]"#).is_err(), "条目缺字段应 Err");
    }

    #[test]
    fn spark_zone_env_file_merge_replace_fallback() {
        let dir = temp_dir("spark-env");
        std::fs::create_dir_all(&dir).unwrap();
        // 合并形态：追加一条新仓 + 覆盖一条内置仓
        let merge_file = dir.join("merge.json");
        std::fs::write(
            &merge_file,
            r#"[
                {"repo":"acme/Test-NVFP4","org":"acme","quant":"NVFP4","params":"9B","note":"新增"},
                {"repo":"unsloth/Qwen3.8-27B-NVFP4","org":"unsloth","quant":"NVFP4","params":"27B","note":"运营改写"}
            ]"#,
        )
        .unwrap();
        let _envs = ScopedEnvs::set(&[("NEXOS_SPARK_ZONE_FILE", merge_file.to_str().unwrap())]);
        let builtin = builtin_spark_zone_entries();
        let (merged, origin) = spark_zone_entries();
        assert_eq!(origin, "env");
        assert_eq!(merged.len(), builtin.len() + 1, "覆盖不计新增");
        assert!(
            merged.iter().any(|e| e.repo == "acme/Test-NVFP4"),
            "env 新条目应追加"
        );
        let touched = merged.iter().find(|e| e.repo == "unsloth/Qwen3.8-27B-NVFP4").unwrap();
        assert_eq!(touched.note, "运营改写", "env 同名整条覆盖");
        drop(_envs); // std Mutex 不可重入：换 env 前先放手（§5D.3 避坑 8）
        // 替换形态：只剩自定义（内置全部让位，运维可删条目）
        let replace_file = dir.join("replace.json");
        std::fs::write(
            &replace_file,
            r#"{"replace":[{"repo":"acme/Only-NVFP4","org":"acme","quant":"NVFP4","params":"7B","note":"唯一"}]}"#,
        )
        .unwrap();
        let _envs2 = ScopedEnvs::set(&[("NEXOS_SPARK_ZONE_FILE", replace_file.to_str().unwrap())]);
        let (replaced, origin2) = spark_zone_entries();
        assert_eq!(origin2, "env");
        assert_eq!(replaced.len(), 1);
        assert_eq!(replaced[0].repo, "acme/Only-NVFP4");
        drop(_envs2);
        // 读失败 / 解析失败 / 非法 repo 条目剔除 → 诚实降级内置表
        let _envs3 =
            ScopedEnvs::set(&[("NEXOS_SPARK_ZONE_FILE", "/no/such/spark-zone-file.json")]);
        let (fb, origin3) = spark_zone_entries();
        assert_eq!(origin3, "builtin", "读失败回退内置表");
        assert_eq!(fb.len(), builtin.len());
        drop(_envs3);
        let bad_file = dir.join("bad.json");
        std::fs::write(&bad_file, "{{{ not json").unwrap();
        let _envs4 = ScopedEnvs::set(&[("NEXOS_SPARK_ZONE_FILE", bad_file.to_str().unwrap())]);
        let (_fb2, origin4) = spark_zone_entries();
        assert_eq!(origin4, "builtin", "解析失败回退内置表");
        drop(_envs4);
        let bad_repo_file = dir.join("badrepo.json");
        std::fs::write(
            &bad_repo_file,
            r#"{"replace":[
                {"repo":"非法仓","org":"x","quant":"NVFP4","params":"7B","note":"n"},
                {"repo":"acme/Ok-NVFP4","org":"acme","quant":"NVFP4","params":"7B","note":"n"}
            ]}"#,
        )
        .unwrap();
        let _envs5 = ScopedEnvs::set(&[("NEXOS_SPARK_ZONE_FILE", bad_repo_file.to_str().unwrap())]);
        let (filtered, origin5) = spark_zone_entries();
        assert_eq!(origin5, "env");
        assert_eq!(filtered.len(), 1, "非法 repo 条目剔除、合法保留");
        assert_eq!(filtered[0].repo, "acme/Ok-NVFP4");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn spark_zone_endpoint_probe_semantics_mock() {
        // 双源可达的假上游 + replace 形态 env 清单（两条测试仓）
        let fake = FakeRemoteRepo::start(
            vec![
                ("model.safetensors".into(), vec![7u8; 300]),
                ("config.json".into(), b"{}".to_vec()),
            ],
            vec![],
        );
        let root = temp_dir("spark-probe");
        std::fs::create_dir_all(&root).unwrap();
        // 预放一个同名目录 → downloaded 标记
        std::fs::create_dir_all(root.join("Alpha-NVFP4")).unwrap();
        let zone_file = root.join("zone.json");
        std::fs::write(
            &zone_file,
            r#"{"replace":[
                {"repo":"acme/Alpha-NVFP4","org":"acme","quant":"NVFP4","params":"27B","note":"甲"},
                {"repo":"acme/Beta-NVFP4","org":"acme","quant":"NVFP4","params":"35B-A3B","note":"乙"}
            ]}"#,
        )
        .unwrap();
        let h = ModelHubRouteHandler::with_empty();
        {
            let _envs = ScopedEnvs::set(&[
                ("NEXOS_MODELS_DIR", root.to_str().unwrap()),
                ("NEXOS_SPARK_ZONE_FILE", zone_file.to_str().unwrap()),
                ("NEXOS_MODELSCOPE_BASE", fake.base.as_str()),
                ("NEXOS_HF_BASE", fake.base.as_str()),
            ]);
            let resp = h
                .handle(get_req("/api/v1/models/spark-zone"))
                .await
                .unwrap();
            assert_eq!(resp.status, 200, "body: {}", resp.body);
            assert_eq!(resp.body["ok"], true);
            assert_eq!(resp.body["probed"], true, "缺省即探测");
            assert_eq!(resp.body["origin"], "env");
            let entries = resp.body["entries"].as_array().unwrap();
            assert_eq!(entries.len(), 2);
            for e in entries {
                let sources = e["sources"].as_array().unwrap();
                assert_eq!(sources.len(), 2, "恒 [modelscope, hf] 两源");
                assert_eq!(sources[0]["kind"], "modelscope");
                assert_eq!(sources[1]["kind"], "hf");
                for s in sources {
                    assert_eq!(s["available"], true, "双源假上游应可用: {e}");
                    assert_eq!(s["file_count"], 2);
                    assert_eq!(s["total_size_bytes"], 302);
                }
            }
            // downloaded 标记：Alpha 预放了目录、Beta 没有
            let alpha = &entries[0];
            let beta = &entries[1];
            assert_eq!(alpha["repo"], "acme/Alpha-NVFP4");
            assert_eq!(alpha["downloaded"], true, "本地同名目录 → downloaded");
            assert_eq!(beta["downloaded"], false);
            // ?probe=1 显式开启（与缺省同义；须在同一把锁内完成——std Mutex 不可
            // 重入，嵌套 ScopedEnvs::set 会自死锁，见 §5D.3 避坑 8）
            let resp = h
                .handle(get_req("/api/v1/models/spark-zone?probe=1"))
                .await
                .unwrap();
            assert_eq!(resp.body["probed"], true);
        }
        // ?probe=0：跳过探测 + 零上游请求（锁已释放后换新假源验 heads；env 释放后
        // 走内置表，但 probe=0 不发任何网络请求）
        {
            let fake2 = FakeRemoteRepo::start(
                vec![("config.json".into(), b"{}".to_vec())],
                vec![],
            );
            let _envs2 = ScopedEnvs::set(&[
                ("NEXOS_MODELSCOPE_BASE", fake2.base.as_str()),
                ("NEXOS_HF_BASE", fake2.base.as_str()),
            ]);
            let resp = h
                .handle(get_req("/api/v1/models/spark-zone?probe=0"))
                .await
                .unwrap();
            assert_eq!(resp.status, 200);
            assert_eq!(resp.body["probed"], false, "?probe=0 跳过探测");
            assert_eq!(resp.body["origin"], "builtin", "env 释放后回内置表");
            for e in resp.body["entries"].as_array().unwrap() {
                assert_eq!(e["quant"], "NVFP4", "内置表条目恒 NVFP4");
                for s in e["sources"].as_array().unwrap() {
                    assert_eq!(s["available"], false);
                    assert!(s["error"].as_str().unwrap().contains("未探测"), "{s}");
                }
            }
            assert!(
                fake2.heads.lock().unwrap().is_empty(),
                "probe=0 不应发任何上游请求"
            );
        }
        // 单侧源死（HF base 指向拒连端口）：该源 unavailable 但条目不剔除
        {
            let _envs = ScopedEnvs::set(&[
                ("NEXOS_MODELS_DIR", root.to_str().unwrap()),
                ("NEXOS_SPARK_ZONE_FILE", zone_file.to_str().unwrap()),
                ("NEXOS_MODELSCOPE_BASE", fake.base.as_str()),
                ("NEXOS_HF_BASE", "http://127.0.0.1:1"),
            ]);
            let resp = h
                .handle(get_req("/api/v1/models/spark-zone"))
                .await
                .unwrap();
            assert_eq!(resp.status, 200);
            assert_eq!(resp.body["probed"], true);
            let entries = resp.body["entries"].as_array().unwrap();
            assert_eq!(entries.len(), 2, "全源失败也不剔除条目");
            for e in entries {
                assert_eq!(e["sources"][0]["available"], true, "魔搭侧仍可用");
                assert_eq!(e["sources"][1]["available"], false, "HF 侧死端口");
                assert!(e["sources"][1]["error"].as_str().is_some());
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn routes_declare_spark_zone_public_read() {
        let h = ModelHubRouteHandler::new();
        let routes = h.routes().await;
        let r = routes
            .iter()
            .find(|r| r.path == "/api/v1/models/spark-zone")
            .expect("应注册 spark-zone 路由");
        assert_eq!(r.method, HttpMethod::Get);
        assert!(!r.requires_auth, "专区为公开读");
        assert_eq!(r.handler_component, "model_hub");
    }
}
