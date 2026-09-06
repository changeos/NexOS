//! `LlmRouteHandler` —— 模型管理（vLLM 推理）桌面应用的 HTTP→实例管理适配器。
//!
//! 定位：把网关 HTTP 请求（`/api/v1/llm/*`）翻译为 vLLM 推理实例管理，返回 JSON。
//! 这是 OS"模型管理"桌面应用（vLLM serve 实例启停 / 健康探测 / 推理测试 / GPU 动态
//! 探测）的后端 REST 入口。
//!
//! # 实例定义持久化（2026-08-22）
//!
//! 实例定义（id/name/model/source_type/port/config）双写内存态 + SQLite
//! `llm_instances` 表（`llm.db`，forwarding.db 同款惯例：WAL + 幂等建表 +
//! 默认路径探测）。创建/删除/启停/健康状态变化时同步落表；**服务重启后从表
//! 恢复全部定义**——status 一律重置 `stopped`、pid/error 清空（旧 pid 不可信），
//! **不自动拉起**（用户裁决：手动启动即可）。首次开库表空即空（不再 seed demo——
//! 实例，之后以表内定义为准。DB 路径 env `NEXOS_LLM_DB` 覆盖。
//!
//! # 通用化（不绑定任何特定 GPU）
//!
//! GPU 信息全部**动态探测**：spawn_blocking 跑 `nvidia-smi`（解析 csv），失败再试
//! `rocm-smi`，都失败返回 `available=false`。**绝不硬编码 GPU 型号或显存数值**。
//! 后续可用于 NVIDIA / AMD / 多卡 / 无 GPU 的 CPU 模式。
//!
//! **统一内存架构**（2026-09-03，DGX Spark GB10 实测）：GB10/Jetson 类超芯片
//! CPU/GPU 共享 LPDDR5x、无独立显存，nvidia-smi csv 显存三列报 `[N/A]`
//! （`0, NVIDIA GB10, [N/A], [N/A], [N/A], 0`）——name 可解析即算有卡，
//! `memory_*` 置 `None` + `unified_memory=true`，并回退 `/proc/meminfo`
//! 填 `unified_memory_*`（MemTotal/MemAvailable 池口径，与监控页同源）。
//! 常规独立显存卡（RTX 3090 等数值形态）路径零变化。
//!
//! # 降级语义（vllm 未安装也不 panic）
//!
//! vLLM 可能未安装或无 GPU——所有真实操作（spawn / 探测 / 推理）失败都降级为
//! 友好的 `error` 状态或 `available=false`，绝不 panic。命令构造为纯函数（可单测，
//! 不真跑）。
//!
//! # 路由表（29 条：实例/网关/recipes 17 条 + 推理环境 7 条 + 外部 API 5 条）
//!
//! | method | path                                | 动作 |
//! |--------|-------------------------------------|------|
//! | GET    | `/api/v1/llm/gpu`                   | GPU 信息（动态探测）|
//! | GET    | `/api/v1/llm/instances`             | 列全部实例 |
//! | POST   | `/api/v1/llm/instances`             | 创建+启动实例（需 admin；可选 `port` 手动指定）|
//! | GET    | `/api/v1/llm/instances/:id`         | 单实例详情 |
//! | POST   | `/api/v1/llm/instances/:id/start`   | 启动实例（需 admin）|
//! | POST   | `/api/v1/llm/instances/:id/stop`    | 停止实例（需 admin）|
//! | DELETE | `/api/v1/llm/instances/:id`         | 删实例（需 admin）|
//! | POST   | `/api/v1/llm/instances/:id/health`  | 健康探测（需 admin）|
//! | POST   | `/api/v1/llm/instances/:id/chat`    | 推理测试（需 admin；兼容 reasoning/reasoning_content）|
//! | GET    | `/api/v1/llm/instances/:id/metrics` | 轻量监控（vLLM /metrics，公开读）|
//! | GET    | `/api/v1/llm/instances/:id/log`     | 实例拉起日志尾（公开读，`?tail=200&follow=0`）|
//! | GET    | `/api/v1/llm/gateway/models`        | 网关聚合视图（真实探测各实例 /v1/models，公开读）|
//! | GET    | `/api/v1/llm/gateway/health`        | 网关可达性汇总（running/可达/不可达 + GPU 总显存，公开读）|
//! | GET    | `/api/v1/llm/stats`                 | 聚合统计 |
//! | POST   | `/api/v1/llm/analyze-image`         | 截图分析（需 admin，给 AI 调用）|
//! | GET    | `/api/v1/llm/recipes/catalog`       | vLLM Recipes 官方配方目录（烘焙代理，公开读；`?refresh=1` 强制重拉）|
//! | GET    | `/api/v1/llm/recipes/recipe?hf_id=` | 单配方 JSON 透传（烘焙代理，公开读；缓存随目录刷新清空）|
//! | GET    | `/api/v1/llm/environments`          | 推理环境列表 + default_name（公开读，子模块）|
//! | POST   | `/api/v1/llm/environments`          | 创建环境 → 202 {task_id}（admin，子模块）|
//! | POST   | `/api/v1/llm/environments/:name/update` | 更新 vLLM 版本 → 202（admin，子模块）|
//! | DELETE | `/api/v1/llm/environments/:name`    | 删环境（admin，默认环境 409，子模块）|
//! | POST   | `/api/v1/llm/environments/:name/default` | 切换默认环境（admin，子模块）|
//! | GET    | `/api/v1/llm/environments/tasks`    | 环境任务列表（公开读，子模块）|
//! | GET    | `/api/v1/llm/environments/tasks/:id`| 单环境任务含日志尾（公开读，子模块）|
//! | GET    | `/api/v1/llm/external-apis`         | 外部 API 列表（key 脱敏，公开读，子模块）|
//! | POST   | `/api/v1/llm/external-apis`         | 登记外部 API（base_url 须 http(s)，admin，子模块）|
//! | PUT    | `/api/v1/llm/external-apis/:id`     | 编辑登记（部分更新，未提供字段保留原值，admin，子模块）|
//! | DELETE | `/api/v1/llm/external-apis/:id`     | 删除登记（admin，子模块）|
//! | POST   | `/api/v1/llm/external-apis/:id/test`| 连通测试：真实 GET /models（admin，子模块）|
//! | POST   | `/api/v1/llm/external-apis/:id/chat`| 对话直通（admin；stream:true 走 http.rs 特挂 SSE，子模块）|
//!
//! # 轻量监控（metrics）设计要点
//!
//! - **按需采集，零后台开销**：无轮询任务，API 调用时才抓 vLLM 的
//!   `GET http://127.0.0.1:<port>/metrics`（Prometheus 文本，轻量逐行解析，
//!   不引入 prometheus crate），同实例 **5s 内存缓存** 去抖，抓取超时 3s。
//! - **Counter 速率**：token / request_success 是 Counter，需两次采样差值算
//!   速率（无历史时为 null）；采样历史按实例存内存，重启即清零（重新预热）。
//! - **降级语义**：实例不存在 404；不可达时 200 + `reachable:false` +
//!   `metrics:null`（监控探测不是错误），绝不伪造。
//! - **模拟模式**：env `NEXOS_LLM_METRICS_SIMULATE=1` 开启——先 200ms 探测真实
//!   端口，通则用真实；不通才返回时间种子 sin 波合成的平滑模拟数据
//!   （`simulated:true`，供 GPU 被占用时前端联调）。默认纯真实。
//!
//! # vLLM Recipes 导入（2026-08-29）
//!
//! 「配方库」Tab 的后端：从 vLLM 官方部署配方站 `https://recipes.vllm.ai`
//! 导入社区维护的模型部署配方（推荐启动命令 / 硬件需求 / 精度变体）。浏览器
//! 直连外网会被 CORS 拦截，故**外网请求只在服务端做**（烘焙代理）：
//!
//! - `GET /recipes/catalog`：拉上游 `models.json`（361 项，15s 超时），精简为
//!   `[{hf_id,title,provider,date_updated}]`；响应信封 `{items, cached_at,
//!   from_cache}`。**常驻进程缓存（无 TTL，2026-09-02 起）**：进程生命周期内
//!   一直用、打开 Tab 只读缓存秒回零外呼；唯一刷新通道 `?refresh=1` 手动强制
//!   重拉并替换缓存（刷新成功连带清空单配方缓存；失败 502 且旧缓存保留）。
//! - `GET /recipes/recipe?hf_id=<HF模型ID>`：单配方 JSON（`/{hf_id}.json`）
//!   原样透传，常驻进程缓存（随目录 refresh 一并清空）。hf_id 校验拒绝
//!   `..`/`?`/`#`/空白（防穿越与 query 注入）。上游失败 502 带原因；参数
//!   缺失/非法 400。
//! - 测试不真连外网：`recipes_base` 字段注入本地 TcpListener mock（同 metrics
//!   假服务手法），缓存命中用「mock 只收 1 个连接」证明。
//!
//! # API 网关聚合（2026-08-30，gateway/models + gateway/health；同日真实化加固）
//!
//! API 网关计费/路由层需要知道「本机**现在真实**有哪几个模型可用」（不只是配置
//! 声称的——实例 status=running 不代表 vLLM 真的起来了/模型真的加载完了）：
//!
//! - `GET /gateway/models`：两段式真实探测——
//!   1) 扫描全部实例，对每个 running 实例**真实探测**其 vLLM
//!      `GET http://127.0.0.1:<port>/v1/models`（2s 超时，并发 join_all）。
//!      探测成功 → `gateway_visible`（带 vLLM 返回的原始模型对象 + 解析出的
//!      `data[].id` 列表）；失败 → `unreachable`（带原因）**且 status 回落
//!      stopped 并落库**（DB 声称 running 但端口已死 = 状态与实际脱节，当场修正）。
//!      **绝不凭 status 伪造可用性**——不可达就是不可达，200 语义返回两组列表。
//!   2) **端口扫描式发现**（2026-08-30 用户报告「模型明明启动了，可路由模型
//!      还是没有」）：vLLM 进程可能活着而 DB status 已回落，或用户手动启动的
//!      vLLM 根本不在实例表——对常见端口段（[`default_discovery_ports`]：
//!      8123 + 8000..=8010，跳过实例表已占端口，1s 快速失败）逐个 GET
//!      /v1/models，命中 → 追加 `discovered:true` 条目（`instance_id:null`，
//!      名「发现的 vLLM :<port>」）。实例表内条目恒 `discovered:false`。
//! - `GET /gateway/health`：汇总——running 数 / 可达数 / 不可达数 + 总 GPU
//!   显存（复用 [`detect_gpu`]）。reachable 只计实例表条目（发现条目另计），
//!   reachable + unreachable == running_total 不变量保持。
//! - `GET /instances` 列表**status 健康修正**（同日）：返回前对每实例探测——
//!   running→验活，死了改回 stopped；stopped→探测端口，活且 served_model_name
//!   匹配（未配置=任意）→ 修正 running。只改 status 字段（pid/error 不动），
//!   修正即落库——彻底解决「状态与实际脱节」。
//! - 测试同 metrics 假服务手法：本地 TcpListener mock /v1/models JSON +
//!   死端口不可达，不真起 vLLM；扫描段端口可注入（`discovery_ports` 字段，
//!   测试默认空防环境依赖）。
//!
//! # 端口选取 / 拉起日志 / 换口重试（2026-08-31）
//!
//! 生产踩坑复盘：8123 被外部进程占用时旧 `pick_free_port` 只查内存实例表，
//! 照样返回 8123 → vLLM 拉起 `Address already in use`（Errno 98）失败，且
//! 日志全混在共享 `/tmp/llm-vllm.log` 里无法按实例排查。本批三件事：
//!
//! - **真实试绑选口**：候选端口对 `0.0.0.0:<port>` 真实 `TcpListener::bind`
//!   （成功即 drop）+ 实例表去重，试绑失败跳过。注意 **TOCTOU 窗口**：试绑
//!   释放与 vLLM 子进程真绑之间第三方仍可能抢口——由下一条兜底。
//! - **spawn 后 30s 端口占用监测 + 换口重试（最多一次）**：后台任务轮询子进程
//!   `try_wait`，发现退出且日志尾含 "Address already in use"/"Errno 98" →
//!   [`pick_free_port_from`] 选下一个真实空闲口，追加日志分隔行、更新实例行
//!   （行 port + config.port 同步）并落库后重试拉起一次；再失败按原错误路径
//!   落 error。日志与实例行都体现最终端口。
//! - **按实例日志文件**：stdout+stderr 落 `<NEXOS_LLM_SPAWN_DIR>/llm-vllm-<id>.log`
//!   （默认 /tmp）；显式设旧 env `NEXOS_LLM_SPAWN_LOG` 则保持单文件模式（向后
//!   兼容，但所有实例共写一个文件——按实例日志端点会失真，文档注明限制）。
//!   `GET /instances/:id/log?tail=200&follow=0`（公开读，对齐 metrics）读日志
//!   尾 N 行（默认 200、上限 1000）。
//!
//! **端口唯一真相源（同日缺陷修复）**：实例端口曾双存于行字段 `port` 与
//! `config.port`——spawn 用 config.port、探测用行 port，两处不一致时实例永久
//! 卡 starting（实测：行改 8124、config 留 8123 → vLLM 绑 8123、探测打 8124）。
//! 现以**行 port 为唯一真相**：spawn/探测/metrics 全部取行 port；config JSON
//! 的 port 在创建、落库、重启恢复、列表修正（[`Self::reconcile_instance_statuses`]
//! 里的收敛）各写入点同步。`POST /instances` 可选 `port` 手动指定（1024..=65535、
//! 不与实例表冲突、不在保留段 8558/7070/11080/11081、真实试绑通过；冲突 409、
//! 越界 400；缺省走自动选）。starting 实例也在列表修正探测范围内（模型加载可
//! 远超拉起时的一次性探测窗口，探活 + /v1/models 就绪即翻 running，不再卡死）。
//!
//! # 推理输出 reasoning 双键兼容（2026-08-31）
//!
//! vLLM 0.28 起思考模型的推理段从 `choices[].message.reasoning_content` 改名
//! `reasoning`（0.27 为 reasoning_content）——`POST /instances/:id/chat` 两个键
//! 都解析并透出（`reasoning`/`finish_reason`/`usage`），小 max_tokens 下思考段
//! 吃满、content 为 null 时不再像故障（前端折叠展示思考段并提示 token 去向）。
//! 端点契约 / env / 拓扑图 / 避坑复盘详见 docs/LLM_INSTANCES.md。
//!
//! # 推理环境（2026-08-31，子模块 [`llm_envs`]）
//!
//! 机器重装后旧 `/home/oem/vllm-env` venv 丢失——「Python venv + 指定版本
//! vLLM」改为可管理的**推理环境**：uv 在 `~/llm-envs/<name>/` 建多个 venv，
//! 注册表 `llm_environments`（同库），页面一键创建/更新（202 异步任务 + 轮询）。
//! 实例 spawn 的 vllm 二进制改为 [`LlmRouteHandler::default_env_bin`] 解析默认
//! 环境（注册表无可用默认行时回退旧硬编码 [`VLLM_BIN`]，向后兼容）；
//! `POST /instances` 请求体新增可选 `env_name`。REST 契约/env 清单见
//! `llm_envs.rs` 模块头与 docs/LLM_ENVIRONMENTS.md。
//!
//! # 外部 API 接入（2026-08-31，子模块 [`llm_external`]）
//!
//! 「我要用别家的模型」：把其它节点/服务商的 OpenAI 兼容端点（如 106 节点
//! 网关的 qwen3.5-9b）登记到轻量表 `llm_external_apis`（同库），连通测试
//! （真实 GET `<base_url>/models`）+ 对话直通（转发
//! `<base_url>/chat/completions`，`stream:true` 由 http.rs 特挂路由做 SSE
//! 逐块透传）。与网关渠道（「我要卖我的模型」）边界、端点契约、拓扑见
//! `llm_external.rs` 模块头与 docs/LLM_EXTERNAL_APIS.md。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use once_cell::sync::Lazy;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use super::llm_envs::{self, LlmEnvState};
use super::llm_external::{self, LlmExternalState};
use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

/// 进程级共享 `reqwest::Client`（rustify：curl 子进程 → reqwest）。
/// 默认 30s 兜底；各调用处用 `RequestBuilder::timeout` 覆盖（探活 3s / 推理 60s）。
static HTTP: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("构建共享 reqwest Client 失败")
});

// ----------------------------------------------------------------------------
// 本机环境常量（对接真实 vLLM 安装路径）
// ----------------------------------------------------------------------------

/// vLLM 二进制绝对路径（**回退值**）。本机 vllm 原装在独立 Python 3.12 环境
/// （v0.26.0），机器重装后该 venv 丢失——2026-08-31 起实例拉起改用
/// [`LlmRouteHandler::default_env_bin`] 从推理环境注册表（`llm_environments`
/// 表，子模块 [`llm_envs`] 管理）解析默认环境的 `bin/vllm`；注册表无
/// is_default=1 且 status=ready 的行时才回退此硬编码路径（向后兼容存量部署）。
const VLLM_BIN: &str = "/home/oem/vllm-env/bin/vllm";

/// vLLM 独立环境的 bin 目录（**回退值**，语义同 [`VLLM_BIN`]）。vLLM 编译
/// CUDA kernel 时需要 ninja（位于此目录），故 spawn vllm 时 PATH 必须含它，
/// 否则 vLLM 启动期编译会失败。现由 [`LlmRouteHandler::default_env_bin`]
/// 按默认推理环境解析。
const VLLM_ENV_PATH: &str = "/home/oem/vllm-env/bin";

/// 本机默认 vLLM 视觉推理服务地址（OpenAI 兼容）。analyze-image 端点转发到此。
const VLLM_VL_ENDPOINT: &str = "http://127.0.0.1:8000/v1/chat/completions";

/// 本机默认 VL 模型名（--served-model-name）。
const VLLM_VL_MODEL: &str = "qwen3-vl-8b";

// ----------------------------------------------------------------------------
// 端口选取 / 拉起日志 常量（2026-08-31，见模块头 §端口选取）
// ----------------------------------------------------------------------------

/// 实例端口自动分配基点（2026-08-21 用户裁决：从 8000 迁 8123，远离 8000 段）。
const INSTANCE_PORT_BASE: u16 = 8123;

/// 手动指定端口下限（1024..=65535；<1024 需 root 特权，一律拒绝）。
const INSTANCE_PORT_MIN: u16 = 1024;

/// OS 保留端口（手动指定直接 409 拒绝，防 vLLM 抢占 OS 自身服务）：
/// 8558 = os-api HTTP（provisioning 缺省）、7070 = os-p2p overlay、
/// 11080/11081 = 网络出口双端 SOCKS5（入口/出口代拨）。
const RESERVED_INSTANCE_PORTS: [u16; 4] = [8558, 7070, 11080, 11081];

/// spawn 后端口占用监测窗口：前 30s 内子进程退出且日志含端口占用 → 换口重试。
const SPAWN_MONITOR_WINDOW: Duration = Duration::from_secs(30);

/// 监测轮询间隔（try_wait 非阻塞，500ms 一拍足够灵敏且零 CPU 占用）。
const SPAWN_MONITOR_POLL: Duration = Duration::from_millis(500);

/// 端口占用判定读取的日志尾字节数（vLLM/uvicorn 报错在最后几行）。
const SPAWN_LOG_TAIL_BYTES: u64 = 64 * 1024;

/// 实例日志端点默认 tail 行数。
const INSTANCE_LOG_TAIL_DEFAULT: usize = 200;

/// 实例日志端点 tail 行数上限（防一次拉回整个日志文件）。
const INSTANCE_LOG_TAIL_MAX: usize = 1000;

/// 实例日志端点单次读取字节上限（256KB；超大日志只看尾部）。
const INSTANCE_LOG_TAIL_BYTES: u64 = 256 * 1024;

/// 实例拉起日志默认目录（env `NEXOS_LLM_SPAWN_DIR` 覆盖；文件名
/// `llm-vllm-<instance_id>.log`——stdout+stderr 同文件）。
const SPAWN_LOG_DIR_DEFAULT: &str = "/tmp";

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 推理实例（一个 `vllm serve` 进程 = 一个实例）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInstance {
    pub id: String,
    /// 实例名（用户起）。
    pub name: String,
    /// 模型名或路径：`Qwen/Qwen2.5-7B-Instruct` 或 `/tank/models/xxx`。
    pub model: String,
    /// `huggingface` / `local`（HF 自动拉 vs 本地路径）。
    pub source_type: String,
    /// vllm 监听端口，如 8000。
    pub port: u16,
    /// `stopped` / `starting` / `running` / `error`。
    pub status: String,
    /// vllm 进程 pid（running 时）。
    pub pid: Option<u32>,
    /// 拉起用的推理环境名（可空 = 默认环境；见 llm_envs 子模块）。
    #[serde(default)]
    pub env_name: Option<String>,
    /// 最近一次真实拉起命令（完整 argv 单行；None = 从未拉起，响应里
    /// `launch_command` 按当前 config 构造——见
    /// [`LlmRouteHandler::effective_launch_command`]，「接入说明」面板用）。
    #[serde(default)]
    pub launch_command: Option<String>,
    /// 启动参数。
    pub config: VllmConfig,
    /// 最近一次健康探测。
    pub health: Option<HealthInfo>,
    pub created_at: String,
    pub error: Option<String>,
}

/// vLLM 启动参数配置（通用，不绑 GPU 型号）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VllmConfig {
    /// 默认 `0.0.0.0`。
    #[serde(default = "default_host")]
    pub host: String,
    /// 默认 8000（实际由 pick_free_port 覆盖分配）。
    #[serde(default = "default_port")]
    pub port: u16,
    /// 默认 1（多卡时增加）。
    #[serde(default = "default_tp")]
    pub tensor_parallel_size: u32,
    /// 默认 0.9。
    #[serde(default = "default_gmu")]
    pub gpu_memory_utilization: f32,
    /// 默认 8192。
    #[serde(default = "default_mml")]
    pub max_model_len: u32,
    /// `awq` / `gptq` / None。
    #[serde(default)]
    pub quantization: Option<String>,
    /// `auto` / `float16` / `bfloat16`。
    #[serde(default = "default_dtype")]
    pub dtype: String,
    /// API 对外的模型名（默认 = model）。
    #[serde(default)]
    pub served_model_name: Option<String>,
    /// 默认 false。
    #[serde(default)]
    pub trust_remote_code: bool,
    /// 透传其它 vllm 参数（原样追加）。
    #[serde(default)]
    pub extra_args: Vec<String>,
}

fn default_host() -> String {
    "0.0.0.0".into()
}
fn default_port() -> u16 {
    8000
}
fn default_tp() -> u32 {
    1
}
/// 读非空 env（llm 模块内联版，避免跨模块依赖）。
fn env_non_empty_llm(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}
fn default_gmu() -> f32 {
    0.9
}
fn default_mml() -> u32 {
    8192
}
fn default_dtype() -> String {
    "auto".into()
}

impl Default for VllmConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 8000,
            tensor_parallel_size: 1,
            gpu_memory_utilization: 0.9,
            max_model_len: 8192,
            quantization: None,
            dtype: "auto".into(),
            served_model_name: None,
            trust_remote_code: false,
            extra_args: vec![],
        }
    }
}

/// 健康探测结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthInfo {
    /// `/health` 返回 200。
    pub alive: bool,
    /// `/v1/models` 有返回。
    pub model_loaded: bool,
    /// 已加载模型名列表。
    pub models: Vec<String>,
    pub checked_at: String,
}

/// GPU 信息（动态探测，不硬编码）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuInfo {
    /// 是否有可用 GPU。
    pub available: bool,
    /// `cuda` / `rocm` / `none`。
    pub backend: String,
    /// 多卡列表。
    pub devices: Vec<GpuDevice>,
}

/// 单个 GPU 设备（型号/显存均动态探测）。
///
/// 统一内存架构（2026-09-03，DGX Spark GB10 实测）：GB10/Jetson 类超芯片
/// 无独立显存，nvidia-smi csv 显存三列报 `[N/A]`——`memory_*` 为 `None`
/// **不代表无卡**（name 可解析即成卡）；真值来源回退 `/proc/meminfo`
/// （CPU/GPU 共享同一 LPDDR5x 池），填入 `unified_memory_*` 并置
/// `unified_memory=true`，前端按「型号 · 统一内存 N GB」如实展示。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuDevice {
    pub index: u32,
    /// 动态探测，如 "NVIDIA GeForce RTX 3090" / "NVIDIA GB10"。
    pub name: String,
    /// 独立显存总量 MiB；统一内存架构（驱动报 `[N/A]`）或解析失败 → None。
    #[serde(default)]
    pub memory_total_mib: Option<u64>,
    /// 独立显存已用 MiB；N/A → None。
    #[serde(default)]
    pub memory_used_mib: Option<u64>,
    /// 独立显存空闲 MiB；N/A → None。
    #[serde(default)]
    pub memory_free_mib: Option<u64>,
    /// 统一内存架构标记（GB10/Jetson：CPU/GPU 共享内存，无独立显存）。
    #[serde(default)]
    pub unified_memory: bool,
    /// 统一内存池总量 MiB（/proc/meminfo MemTotal；unified_memory=true 时填）。
    #[serde(default)]
    pub unified_memory_total_mib: Option<u64>,
    /// 统一内存池已用 MiB（MemTotal − MemAvailable；unified 时填）。
    #[serde(default)]
    pub unified_memory_used_mib: Option<u64>,
    /// 统一内存池可用 MiB（MemAvailable；unified 时填）。
    #[serde(default)]
    pub unified_memory_free_mib: Option<u64>,
    /// GPU 使用率%（`[N/A]` → None）。
    #[serde(default)]
    pub utilization_pct: Option<u32>,
}

/// `GET /api/v1/llm/stats` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmStats {
    pub instances_total: usize,
    pub running: usize,
    pub stopped: usize,
    pub gpu_available: bool,
    pub gpu_devices: usize,
}

/// `GET /api/v1/llm/gateway/models` 响应（网关聚合视图）。
///
/// `gateway_visible` = 探测 /v1/models 成功的 running 实例（网关可真实路由）
/// **+ 实例表之外端口扫描发现的本地 vLLM**（`discovered:true`）；
/// `unreachable` = status=running 但探测失败的实例（配置声称在跑、实际不可达）。
/// 非 running 实例两组都不进（未探测，无话可说）——但 stopped 实例的端口若在
/// 扫描段内且活着，会以 discovered 条目出现（真实可路由，不因表状态丢失）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayModelsResponse {
    pub gateway_visible: Vec<GatewayModelEntry>,
    pub unreachable: Vec<GatewayUnreachableEntry>,
}

/// 网关可见的一条实例（/v1/models 探测成功；实例表内或扫描发现）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayModelEntry {
    /// 所属实例 id；**端口扫描发现的条目为 null**（不在实例表里）。
    pub instance_id: Option<String>,
    pub name: String,
    /// 实例配置的对外模型名（未配置时 null——vLLM 落到 model 路径名；
    /// 发现条目恒 null）。
    pub served_model_name: Option<String>,
    pub port: u16,
    /// 恒 true（可见即活；字段显式存在便于消费方统一判别）。
    pub alive: bool,
    /// vLLM `/v1/models` 返回的原始模型对象（`data[]` 原样）。
    pub models: Vec<serde_json::Value>,
    /// 从 `data[].id` 解析出的模型 id 列表（网关路由/计费的真实键）。
    pub model_ids: Vec<String>,
    /// 实例表内 false / 端口扫描发现 true（消费方据此区分「纳管的」与「野生的」）。
    pub discovered: bool,
}

/// status=running 但 /v1/models 探测失败的实例。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayUnreachableEntry {
    pub instance_id: String,
    pub name: String,
    pub port: u16,
    /// 失败原因（连接拒绝 / 超时 / HTTP 状态错 / JSON 解析失败）。
    pub reason: String,
}

/// `GET /api/v1/llm/gateway/health` 响应（网关可达性汇总）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayHealthResponse {
    /// status=running 的实例数（探测目标总数）。
    pub running_total: usize,
    /// /v1/models 探测成功数。
    pub reachable: usize,
    /// /v1/models 探测失败数（reachable + unreachable == running_total）。
    pub unreachable: usize,
    pub gpu_available: bool,
    /// `cuda` / `rocm` / `none`。
    pub gpu_backend: String,
    /// 全部 GPU 显存总和 MiB（复用 detect_gpu；无 GPU/统一内存架构报不出 → 0）。
    pub gpu_memory_total_mib: u64,
    /// 统一内存架构（GB10/Jetson：显存字段 [N/A]，gpu_memory_total_mib=0 时
    /// 真实容量在设备级 unified_memory_total_mib，不是"无显存"）。
    pub gpu_unified_memory: bool,
}

/// `GET /api/v1/llm/instances/:id/metrics` 响应（轻量监控）。
///
/// `reachable:false` 时 `metrics:null`（200 语义——监控探测不是错误）；
/// 模拟模式下真实端口不通时返回合成数据（`reachable:false` +
/// `simulated:true` + `metrics:{...合成}`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceMetricsResponse {
    pub instance_id: String,
    /// 真实 vLLM /metrics 是否抓取成功。
    pub reachable: bool,
    /// metrics 是否为合成模拟数据（仅模拟模式且真实端口不通时 true）。
    pub simulated: bool,
    /// 采集时刻（ISO 8601 本地时间）。
    pub collected_at: String,
    /// 抓取目标（`http://127.0.0.1:<port>`）。
    pub base_url: String,
    /// 指标快照；缺失字段为 null（vLLM 版本差异 / Counter 无历史）。
    pub metrics: Option<InstanceMetricsSnapshot>,
}

/// 单次采集的指标快照（Counter 速率由两次采样差值算出，首次无历史为 null）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceMetricsSnapshot {
    /// 运行中请求数（Gauge）。
    pub num_requests_running: Option<u64>,
    /// 排队请求数（Gauge）。
    pub num_requests_waiting: Option<u64>,
    /// KV cache 占用率 0-1（Gauge）。
    pub gpu_cache_usage: Option<f64>,
    /// prefix cache 命中率 0-1（Gauge）。
    pub prefix_cache_hit_rate: Option<f64>,
    /// 生成 token 速率（generation_tokens_total 差值/秒；无历史 null）。
    pub generation_tokens_per_sec: Option<f64>,
    /// prompt token 速率（prompt_tokens_total 差值/秒；无历史 null）。
    pub prompt_tokens_per_sec: Option<f64>,
    /// 完成请求速率（request_success_total 差值/秒；无历史 null）。
    pub requests_success_per_sec: Option<f64>,
    /// 端到端请求时延均值（e2e_request_latency_seconds sum/count，毫秒）。
    pub e2e_latency_ms: Option<f64>,
}

/// `GET /api/v1/llm/instances/:id/log?tail=200&follow=0` 响应（公开读，对齐
/// metrics 端点权限风格）。
///
/// 读该实例拉起日志文件（`<NEXOS_LLM_SPAWN_DIR>/llm-vllm-<id>.log`）的尾
/// `tail` 行（默认 200、上限 1000；单次读取字节上限 256KB）。`follow` 参数
/// 当前为拉取式实现（响应同构），持续跟随由前端 2s 轮询完成。实例不存在或
/// 日志文件尚未生成（从未拉起过）均 404。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceLogResponse {
    pub instance_id: String,
    /// 日志尾 N 行（按文件顺序）。
    pub lines: Vec<String>,
    /// 日志文件绝对路径（排查用；单文件模式（NEXOS_LLM_SPAWN_LOG）下为该文件）。
    pub file: String,
    /// 实例当前 status（starting 时看启动进度最常用）。
    pub status: String,
}

/// 创建实例请求体。
#[derive(Debug, Deserialize)]
struct CreateInstanceBody {
    name: String,
    model: String,
    /// 默认 huggingface
    #[serde(default)]
    source_type: Option<String>,
    /// 可选部分覆盖默认 VllmConfig
    #[serde(default)]
    config: Option<VllmConfig>,
    /// 可选手动指定监听端口（1024..=65535；不与实例表冲突、不在保留段
    /// 8558/7070/11080/11081、真实试绑通过，冲突 409 / 越界 400）。
    /// 缺省 = 自动选口（`pick_free_port`：实例表去重 + 真实试绑，8123 起）。
    /// 用 u64 承接：70000 这类超 u16 值也要走 400 校验路径而非 serde 解析失败。
    #[serde(default)]
    port: Option<u64>,
    /// 可选拉起环境：推理环境注册表里的环境名（缺省 = 默认环境；见 llm_envs）
    #[serde(default)]
    env_name: Option<String>,
    /// 是否创建后立即 spawn vllm（默认 false：仅 stopped 记录；测试避免真起 vllm）
    #[serde(default)]
    autostart: Option<bool>,
}

/// 推理测试请求体。
///
/// 2026-09-04 起 `pub(crate)`（含字段）：film.rs 的 local.chat 复用
/// [`LlmRouteHandler::chat_complete`] 实例调用面时构造请求体；仅可见性变化。
///
/// 2026-09-04（第二批，分镜质量修复）：新增 `chat_template_kwargs` 顶层透传字段
/// （vLLM OpenAI 兼容面原生支持，如 `{"enable_thinking": false}` 关闭思考段——
/// 9B 级小模型开思考时分镜 JSON 会被 <think> 段污染）。None 时序列化不出现该键
/// （`skip_serializing_if`），对无此字段的服务端零影响；Some 时
/// [`LlmRouteHandler::chat_complete`] 直接 serde 序列化整个请求体原样透传。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ChatBody {
    pub(crate) messages: Vec<ChatMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) temperature: Option<f32>,
    /// vLLM chat template 关键字透传（顶层字段；None 序列化时省略）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) chat_template_kwargs: Option<serde_json::Value>,
}

/// OpenAI chat 消息（2026-09-04 起 `pub(crate)`：同 [`ChatBody`]，film 复用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ChatMessage {
    pub(crate) role: String,
    pub(crate) content: String,
}

/// `POST /instances/:id/chat` 的一次推理结果（content + 思考段 + 计量）。
///
/// `reasoning` 双键兼容（vLLM 0.28 `reasoning` / 0.27 `reasoning_content`，
/// 服务端归一为 `reasoning`）；小 max_tokens 下思考段吃满时 content 为空串但
/// reasoning 非空——不是故障，前端折叠展示思考段并提示 token 去向。
///
/// 2026-09-04 起 `pub(crate)`（含字段）：film.rs 的 local.chat 复用实例调用面；
/// 仅可见性变化。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ChatOutcome {
    pub(crate) content: String,
    /// 思考段（无则为空串；两键都缺失时空串）。
    #[serde(default)]
    pub(crate) reasoning: String,
    /// `stop` / `length` / …（缺失 null）。
    #[serde(default)]
    pub(crate) finish_reason: Option<String>,
    /// usage.total_tokens（缺失 null）。
    #[serde(default)]
    pub(crate) total_tokens: Option<u64>,
}

/// `POST /api/v1/llm/analyze-image` 请求体（给 AI 主代理调用分析截图）。
#[derive(Debug, Deserialize)]
struct AnalyzeImageBody {
    /// base64 编码的图片（不含 `data:image/png;base64,` 前缀）。
    image_base64: String,
    /// 可选，默认 image/png。
    #[serde(default)]
    image_mime: Option<String>,
    /// 问题/指令，如 "描述这张截图的内容"。
    prompt: String,
    /// 可选，默认 400。
    #[serde(default)]
    max_tokens: Option<u32>,
}

/// `POST /api/v1/llm/analyze-image` 成功响应。
#[derive(Debug, Serialize)]
struct AnalyzeImageResponse {
    description: String,
    model: String,
    tokens_used: u64,
}

// ----------------------------------------------------------------------------
// 命令构造器（纯函数，易测试）
// ----------------------------------------------------------------------------

/// 构造 nvidia-smi 查询命令参数（不执行，纯构造，便于测试）。
///
/// caller 负责 `Command::new("nvidia-smi").args(build_nvidia_smi_cmd())`。
#[must_use]
pub fn build_nvidia_smi_cmd() -> Vec<String> {
    vec![
        "--query-gpu=index,name,memory.total,memory.used,memory.free,utilization.gpu".into(),
        "--format=csv,noheader,nounits".into(),
    ]
}

/// 构造 vllm serve 启动命令参数（不含程序名，caller 拼 `Command::new("vllm")`）。
///
/// 形如：`serve <model> --host <host> --port <port> --tensor-parallel-size <tp>
/// --gpu-memory-utilization <gmu> --max-model-len <mml>
/// (quantization 时) --quantization <q> --dtype <dt>
/// (served_model_name 时) --served-model-name <name>
/// (trust_remote_code=true 时) --trust-remote-code
/// extra_args 原样追加`
#[must_use]
pub fn build_vllm_serve_cmd(model: &str, config: &VllmConfig) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "serve".into(),
        model.into(),
        "--host".into(),
        config.host.clone(),
        "--port".into(),
        config.port.to_string(),
        "--tensor-parallel-size".into(),
        config.tensor_parallel_size.to_string(),
        "--gpu-memory-utilization".into(),
        format_gpu_mem(config.gpu_memory_utilization),
        "--max-model-len".into(),
        config.max_model_len.to_string(),
        "--dtype".into(),
        config.dtype.clone(),
    ];
    if let Some(q) = &config.quantization {
        let q = q.trim();
        if !q.is_empty() {
            args.push("--quantization".into());
            args.push(q.into());
        }
    }
    if let Some(smn) = &config.served_model_name {
        let smn = smn.trim();
        if !smn.is_empty() {
            args.push("--served-model-name".into());
            args.push(smn.into());
        }
    }
    if config.trust_remote_code {
        args.push("--trust-remote-code".into());
    }
    // extra_args 原样追加（用户透传）
    for a in &config.extra_args {
        args.push(a.clone());
    }
    args
}

/// 把 gpu_memory_utilization 格式化为短小字符串（0.9 → "0.9"，0.95 → "0.95"）。
fn format_gpu_mem(v: f32) -> String {
    // 保留最多两位小数，去尾零
    let s = format!("{:.2}", v);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

// ----------------------------------------------------------------------------
// GPU 探测（真实 spawn_blocking，失败降级不 panic）
// ----------------------------------------------------------------------------

/// 动态探测 GPU 信息：先试 nvidia-smi，失败再试 rocm-smi，都失败返回 available=false。
pub async fn detect_gpu() -> GpuInfo {
    // nvidia-smi
    if let Some(info) = detect_nvidia().await {
        return info;
    }
    // rocm-smi
    if let Some(info) = detect_rocm().await {
        return info;
    }
    // 都失败
    GpuInfo {
        available: false,
        backend: "none".into(),
        devices: vec![],
    }
}

/// 探测 NVIDIA GPU（nvidia-smi）。失败/无卡返回 None。
///
/// **有输出即算有 GPU**：csv 行 name 可解析即成卡（GB10/Jetson 统一内存
/// 架构显存列报 `[N/A]`，不再是"无卡"）；显存 `[N/A]` 的设备由
/// [`apply_unified_meminfo`] 回退填 `/proc/meminfo` 统一内存池数值。
async fn detect_nvidia() -> Option<GpuInfo> {
    let out = tokio::process::Command::new("nvidia-smi")
        .args(build_nvidia_smi_cmd())
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut devices: Vec<GpuDevice> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(parse_nvidia_smi_line)
        .collect();
    if devices.is_empty() {
        return None;
    }
    apply_unified_meminfo(&mut devices);
    Some(GpuInfo {
        available: true,
        backend: "cuda".into(),
        devices,
    })
}

/// 统一内存回退：`unified_memory=true` 的设备填 `/proc/meminfo` 池数值
/// （MemTotal/MemAvailable → total/free，used=差值；读失败保持 None，前端如实展示）。
fn apply_unified_meminfo(devices: &mut [GpuDevice]) {
    if !devices.iter().any(|d| d.unified_memory) {
        return;
    }
    // /proc/meminfo 常驻虚拟文件，一次读 ~1KB，同步读开销可忽略
    let (total_mib, free_mib) = unified_meminfo_mib();
    let used_mib = match (total_mib, free_mib) {
        (Some(t), Some(f)) => Some(t.saturating_sub(f)),
        _ => None,
    };
    for d in devices.iter_mut().filter(|d| d.unified_memory) {
        d.unified_memory_total_mib = total_mib;
        d.unified_memory_used_mib = used_mib;
        d.unified_memory_free_mib = free_mib;
    }
}

/// 读 `/proc/meminfo` → (总量 MiB, 可用 MiB)（失败 (None, None)）。
/// 复用 monitor::read_meminfo（与 /monitor/metrics 同一口径）。
fn unified_meminfo_mib() -> (Option<u64>, Option<u64>) {
    let (total_b, avail_b, _, _) = crate::handlers::monitor::read_meminfo();
    let mib = |b: u64| (b > 0).then_some(bytes_to_mib(b));
    (mib(total_b), mib(avail_b))
}

/// 解析一行 nvidia-smi csv（无表头，无单位）：
/// `0, NVIDIA GeForce RTX 3090, 24576, 1024, 23552, 5`（常规独立显存卡）
/// `0, NVIDIA GB10, [N/A], [N/A], [N/A], 0`（DGX Spark GB10 统一内存，实测形态）
/// 列序：index, name, memory.total(MiB), memory.used(MiB), memory.free(MiB), utilization.gpu(%)。
///
/// name 可解析即成卡；显存/使用率字段 `[N/A]` 或解析失败 → `None`
/// （统一内存架构由 [`apply_unified_meminfo`] 回退，**不判无卡**）。
fn parse_nvidia_smi_line(line: &str) -> Option<GpuDevice> {
    let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
    if parts.len() < 6 {
        return None;
    }
    let index = parts[0].parse::<u32>().ok()?;
    let name = parts[1];
    if name.is_empty() || name.eq_ignore_ascii_case("[N/A]") {
        return None;
    }
    let total = parse_nvidia_smi_num(parts[2]);
    let used = parse_nvidia_smi_num(parts[3]);
    let free = parse_nvidia_smi_num(parts[4]);
    let util = parse_nvidia_smi_num(parts[5]).map(|u| u as u32);
    Some(GpuDevice {
        index,
        name: name.to_string(),
        memory_total_mib: total,
        memory_used_mib: used,
        memory_free_mib: free,
        // 显存总量报不出（[N/A]）= 驱动不管理独立显存 = 统一内存架构形态
        unified_memory: total.is_none(),
        unified_memory_total_mib: None,
        unified_memory_used_mib: None,
        unified_memory_free_mib: None,
        utilization_pct: util,
    })
}

/// 解析 nvidia-smi csv 数值字段：数字 → Some；`[N/A]` / 空串 / 不可解析 → None。
fn parse_nvidia_smi_num(s: &str) -> Option<u64> {
    s.trim().parse::<u64>().ok()
}

/// 探测 AMD ROCm GPU（rocm-smi）。失败/无卡返回 None。
///
/// 简化处理：用 `rocm-smi --showproductname --showmeminfo vram --json`，解析 JSON。
async fn detect_rocm() -> Option<GpuInfo> {
    let out = tokio::process::Command::new("rocm-smi")
        .args([
            "--showproductname",
            "--showmeminfo",
            "vram",
            "--showuse",
            "gpu",
            "--json",
        ])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    // rocm-smi --json 以 card 键分组，形如 {"card0": {"Card series": "...", "VRAM Total Memory (B)": "...", ...}}
    let mut devices = Vec::new();
    if let Some(obj) = value.as_object() {
        let mut idx = 0u32;
        for (key, val) in obj {
            if !key.starts_with("card") {
                continue;
            }
            let name = val
                .get("Card series")
                .and_then(|v| v.as_str())
                .or_else(|| val.get("Card model").and_then(|v| v.as_str()))
                .unwrap_or("AMD GPU")
                .trim()
                .trim_matches('"')
                .to_string();
            let total_b = parse_rocm_num(val.get("VRAM Total Memory (B)"));
            let used_b = parse_rocm_num(val.get("VRAM Total Used Memory (B)"));
            let free_b = total_b.saturating_sub(used_b);
            let util = parse_rocm_num(val.get("GPU use (%)")) as u32;
            devices.push(GpuDevice {
                index: idx,
                name,
                memory_total_mib: Some(bytes_to_mib(total_b)),
                memory_used_mib: Some(bytes_to_mib(used_b)),
                memory_free_mib: Some(bytes_to_mib(free_b)),
                // ROCm 卡均为独立显存（HBM/GDDR），无统一内存回退
                unified_memory: false,
                unified_memory_total_mib: None,
                unified_memory_used_mib: None,
                unified_memory_free_mib: None,
                utilization_pct: Some(util),
            });
            idx += 1;
        }
    }
    if devices.is_empty() {
        return None;
    }
    Some(GpuInfo {
        available: true,
        backend: "rocm".into(),
        devices,
    })
}

/// 解析 rocm-smi json 数值字段（字符串或数字，带可能的前后空格/引号）。
fn parse_rocm_num(v: Option<&serde_json::Value>) -> u64 {
    let v = match v {
        Some(v) => v,
        None => return 0,
    };
    if let Some(n) = v.as_u64() {
        return n;
    }
    if let Some(n) = v.as_f64() {
        return n as u64;
    }
    if let Some(s) = v.as_str() {
        return s.trim().trim_matches('"').parse::<u64>().unwrap_or(0);
    }
    0
}

/// 字节 → MiB（向下取整）。
fn bytes_to_mib(b: u64) -> u64 {
    b / (1024 * 1024)
}

// ----------------------------------------------------------------------------
// 轻量监控：Prometheus 文本解析 / 抓取 / 缓存 / Counter 速率 / 模拟合成
// ----------------------------------------------------------------------------

/// metrics 缓存 TTL（同实例去抖；窗口内重复调用直接回缓存，零重复抓取）。
const METRICS_CACHE_TTL: Duration = Duration::from_secs(5);

/// 真实模式 /metrics 抓取超时。
const METRICS_FETCH_TIMEOUT: Duration = Duration::from_secs(3);

/// 模拟模式的真实端口探测超时（快速失败，避免拖慢响应）。
const METRICS_SIMULATE_PROBE_TIMEOUT: Duration = Duration::from_millis(200);

/// 模拟模式开关 env（=1 开启；默认纯真实模式，绝不伪造）。
const SIMULATE_ENV: &str = "NEXOS_LLM_METRICS_SIMULATE";

/// 一次 /metrics 抓取解析出的原始指标（缺失为 None → 响应 null）。
#[derive(Debug, Clone, Default)]
struct RawVllmMetrics {
    num_requests_running: Option<u64>,
    num_requests_waiting: Option<u64>,
    gpu_cache_usage: Option<f64>,
    prefix_cache_hit_rate: Option<f64>,
    generation_tokens_total: Option<f64>,
    prompt_tokens_total: Option<f64>,
    request_success_total: Option<f64>,
    e2e_latency_seconds: Option<f64>,
}

/// Counter 采样点（速率 = 两次采样差值 / 间隔秒；字段 None = 该轮抓取缺失）。
#[derive(Debug, Clone, Copy, PartialEq)]
struct CounterSample {
    at: std::time::Instant,
    generation_tokens_total: Option<f64>,
    prompt_tokens_total: Option<f64>,
    request_success_total: Option<f64>,
}

/// 由两次 Counter 采样算出的速率（无历史/回绕 → None）。
#[derive(Debug, Clone, Copy, Default)]
struct CounterRates {
    generation: Option<f64>,
    prompt: Option<f64>,
    success: Option<f64>,
}

/// metrics 响应缓存条目（存序列化后的完整响应体，含 collected_at）。
#[derive(Debug, Clone)]
struct MetricsCacheEntry {
    at: std::time::Instant,
    body: serde_json::Value,
}

/// 模拟模式是否开启（`NEXOS_LLM_METRICS_SIMULATE=1` 或 `true`）。
fn simulate_enabled() -> bool {
    std::env::var(SIMULATE_ENV)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// 缓存新鲜度判定（纯函数；`now` 可注入，便于测试推进时间；时钟倒挂视为过期）。
fn metrics_cache_is_fresh(cached_at: std::time::Instant, now: std::time::Instant) -> bool {
    now.checked_duration_since(cached_at)
        .map(|d| d < METRICS_CACHE_TTL)
        .unwrap_or(false)
}

/// 从 Prometheus 文本中取名为 `name` 的第一个样本值。
///
/// 轻量逐行解析（不引入 prometheus crate）：跳过空行与 `# HELP/# TYPE` 注释
/// 行；容忍样本名后的 `{label="v",...}` 标签（取 `{` 前的名字）；值为样本名
/// 后第一个 token（可能跟时间戳，忽略之）；支持科学计数（`4.2e-1`），
/// `NaN/Inf` 视为缺失。未命中返回 None。
fn parse_prometheus_value(text: &str, name: &str) -> Option<f64> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let sample = parts.next()?;
        let metric_name = sample.split('{').next().unwrap_or(sample);
        if metric_name != name {
            continue;
        }
        if let Some(v) = parts.next().and_then(|v| v.parse::<f64>().ok()) {
            if v.is_finite() {
                return Some(v);
            }
        }
    }
    None
}

/// 解析一次 vLLM /metrics 抓取文本 → 原始指标。
///
/// prefix 命中率做兼容：新版 `vllm:prefix_cache_hit_rate`，缺失时回退
/// `vllm:gpu_prefix_cache_hit_rate`。e2e 时延取 Summary 的 `_sum/_count`
/// 均值，无 Summary 时按 Gauge 直取。
fn parse_vllm_metrics(text: &str) -> RawVllmMetrics {
    RawVllmMetrics {
        num_requests_running: parse_prometheus_value(text, "vllm:num_requests_running")
            .map(|v| v.max(0.0) as u64),
        num_requests_waiting: parse_prometheus_value(text, "vllm:num_requests_waiting")
            .map(|v| v.max(0.0) as u64),
        gpu_cache_usage: parse_prometheus_value(text, "vllm:gpu_cache_usage_perc"),
        prefix_cache_hit_rate: parse_prometheus_value(text, "vllm:prefix_cache_hit_rate")
            .or_else(|| parse_prometheus_value(text, "vllm:gpu_prefix_cache_hit_rate")),
        generation_tokens_total: parse_prometheus_value(text, "vllm:generation_tokens_total"),
        prompt_tokens_total: parse_prometheus_value(text, "vllm:prompt_tokens_total"),
        request_success_total: parse_prometheus_value(text, "vllm:request_success_total"),
        e2e_latency_seconds: parse_e2e_latency_seconds(text),
    }
}

/// e2e 时延：优先 Summary `_sum/_count` 均值（count>0），否则按 Gauge 直取。
fn parse_e2e_latency_seconds(text: &str) -> Option<f64> {
    let sum = parse_prometheus_value(text, "vllm:e2e_request_latency_seconds_sum");
    let count = parse_prometheus_value(text, "vllm:e2e_request_latency_seconds_count");
    if let (Some(s), Some(c)) = (sum, count) {
        if c > 0.0 {
            return Some(s / c);
        }
    }
    parse_prometheus_value(text, "vllm:e2e_request_latency_seconds")
}

/// 单个 Counter 速率：差值/间隔秒；间隔非法（<=0）或回绕（cur<prev，
/// vLLM 重启过）→ None。
fn counter_rate(prev: f64, cur: f64, elapsed_secs: f64) -> Option<f64> {
    if elapsed_secs <= 0.0 || cur < prev {
        return None;
    }
    Some((cur - prev) / elapsed_secs)
}

/// 可选 Counter 速率（任一轮采样缺失 → None）。
fn opt_counter_rate(prev: Option<f64>, cur: Option<f64>, elapsed_secs: f64) -> Option<f64> {
    counter_rate(prev?, cur?, elapsed_secs)
}

/// 两次 Counter 采样 → 三路速率（无上一采样 → 全 None）。
fn compute_counter_rates(prev: Option<CounterSample>, cur: &CounterSample) -> CounterRates {
    let Some(prev) = prev else {
        return CounterRates::default();
    };
    let elapsed = cur.at.duration_since(prev.at).as_secs_f64();
    CounterRates {
        generation: opt_counter_rate(
            prev.generation_tokens_total,
            cur.generation_tokens_total,
            elapsed,
        ),
        prompt: opt_counter_rate(prev.prompt_tokens_total, cur.prompt_tokens_total, elapsed),
        success: opt_counter_rate(
            prev.request_success_total,
            cur.request_success_total,
            elapsed,
        ),
    }
}

/// 原始指标 + Counter 速率 → 响应快照（缺失字段保持 None → JSON null）。
fn build_metrics_snapshot(raw: &RawVllmMetrics, rates: CounterRates) -> InstanceMetricsSnapshot {
    InstanceMetricsSnapshot {
        num_requests_running: raw.num_requests_running,
        num_requests_waiting: raw.num_requests_waiting,
        gpu_cache_usage: raw.gpu_cache_usage,
        prefix_cache_hit_rate: raw.prefix_cache_hit_rate,
        generation_tokens_per_sec: rates.generation,
        prompt_tokens_per_sec: rates.prompt,
        requests_success_per_sec: rates.success,
        e2e_latency_ms: raw.e2e_latency_seconds.map(|s| s * 1000.0),
    }
}

/// 抓取 vLLM Prometheus 文本（`GET http://127.0.0.1:<port>/metrics`）。
///
/// 失败（连接拒绝/超时/非 2xx）返回 Err——真实模式下即不可达。
async fn fetch_metrics_text(port: u16, timeout: Duration) -> Result<String, String> {
    let url = format!("http://127.0.0.1:{port}/metrics");
    let resp = HTTP
        .get(&url)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| format!("metrics 抓取失败: {e}"))?;
    let resp = resp
        .error_for_status()
        .map_err(|e| format!("metrics HTTP 状态错误: {e}"))?;
    resp.text()
        .await
        .map_err(|e| format!("metrics 响应读取失败: {e}"))
}

/// 当前 Unix epoch 秒（模拟合成的时间种子；f64 保留亚秒精度）。
fn epoch_secs_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// 合成平滑模拟指标（确定性：同一实例同一时刻输出恒定，无需随机库）。
///
/// 算法：`t` = epoch 秒 + 实例 id 字节和 ×0.37 的相位偏移（各实例波形错开）；
/// 负载 `load` = 长周期 sin 基波（37s）+ 次级波（7.3s），clamp 0-1。派生指标
/// 保持物理合理：
/// - `running` = round(load×8)（0-8 整数）；`waiting` 仅 load>0.55 时随 load
///   线性增长（负载高才排队）；
/// - `gpu_cache_usage` = 0.30+0.60×load（clamp 0-0.9）；
/// - `latency_ms` = 150+850×load²（负载平方放大排队时延）；
/// - token 速率与 latency 负相关：`gen_tps` = 420/(1+1.8×load²)，
///   `prompt_tps` ≈ 2-3×gen_tps；
/// - `success_rps` ≈ running×(1000/latency_ms)×0.7（Little's law）。
///
/// 各指标叠加不同周期的确定性 sin 抖动，曲线平滑且不显机械。
fn synthetic_metrics(instance_id: &str, t_secs: f64) -> InstanceMetricsSnapshot {
    let phase = instance_id.bytes().map(f64::from).sum::<f64>() * 0.37;
    let t = t_secs + phase;
    let load = (0.45 + 0.32 * (t / 37.0).sin() + 0.18 * (t / 7.3).sin()).clamp(0.0, 1.0);
    let jitter = 0.03 * (t / 1.7).sin();

    let running = (load * 8.0).round().clamp(0.0, 8.0) as u64;
    let waiting = ((load - 0.55).max(0.0) * 20.0).floor() as u64;
    let gpu_cache_usage = (0.30 + 0.60 * load + jitter).clamp(0.0, 0.90);
    let prefix_cache_hit_rate = (0.62 + 0.30 * (t / 11.7).sin() + jitter).clamp(0.0, 1.0);
    let e2e_latency_ms =
        (150.0 + 850.0 * load * load + 40.0 * (t / 3.1).sin()).clamp(80.0, 3_000.0);
    let generation_tps =
        (420.0 / (1.0 + 1.8 * load * load) * (1.0 + 0.08 * (t / 2.3).sin())).max(0.0);
    let prompt_tps = (generation_tps * (2.4 + 0.6 * (t / 5.3).sin())).max(0.0);
    let success_rps = (running as f64 * (1000.0 / e2e_latency_ms) * 0.7).max(0.0);

    InstanceMetricsSnapshot {
        num_requests_running: Some(running),
        num_requests_waiting: Some(waiting),
        gpu_cache_usage: Some(gpu_cache_usage),
        prefix_cache_hit_rate: Some(prefix_cache_hit_rate),
        generation_tokens_per_sec: Some(generation_tps),
        prompt_tokens_per_sec: Some(prompt_tps),
        requests_success_per_sec: Some(success_rps),
        e2e_latency_ms: Some(e2e_latency_ms),
    }
}

// ----------------------------------------------------------------------------
// vLLM Recipes 导入（烘焙代理：浏览器直连外网被 CORS 挡，服务端统一拉取）
// ----------------------------------------------------------------------------

/// vLLM Recipes 官方站点根地址（公开无鉴权；测试注入本地 mock 覆盖）。
const RECIPES_BASE_URL: &str = "https://recipes.vllm.ai";

/// recipes 上游拉取超时（外网，15s——够慢链路又不会拖死请求方）。
const RECIPES_FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// recipes 响应缓存条目（存序列化后的响应体 + 采集墙钟时刻——供响应
/// `cached_at` 展示「上次刷新」；**无 TTL**：进程生命周期内常驻，唯一刷新
/// 通道是 `?refresh=1` 手动强制重拉，2026-09-02 起）。
#[derive(Debug, Clone)]
struct RecipesCacheEntry {
    /// 墙钟采集时刻（RFC3339 序列化给前端展示；Instant 不可序列化故不用）。
    fetched_at: chrono::DateTime<chrono::Utc>,
    body: serde_json::Value,
}

/// 缓存条目 → 响应信封（`{items, cached_at, from_cache}`；`from_cache` 标记
/// 本次响应是进程缓存秒回还是刚从上游拉取，测试/前端均可据此断言）。
fn recipes_envelope(entry: &RecipesCacheEntry, from_cache: bool) -> serde_json::Value {
    serde_json::json!({
        "items": entry.body.clone(),
        "cached_at": entry.fetched_at.to_rfc3339(),
        "from_cache": from_cache,
    })
}

/// 精简目录项（`GET /recipes/catalog` 数组元素）：只留目录表格要展示的四列。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipeCatalogItem {
    /// HF 模型 ID（如 `meta-llama/Llama-3.1-8B`），查单配方与拼官网链接的键。
    pub hf_id: String,
    pub title: String,
    pub provider: String,
    /// 更新日期（上游索引当前未提供该字段 → null；配方详情 meta 才有，留扩展位）。
    pub date_updated: Option<String>,
}

/// hf_id 合法性（纯函数）：非空、不以 `/` 开头、不含 `..`/`?`/`#`/空白——
/// 拼上游 URL 前拦截路径穿越与 query 注入（合法形如 `meta-llama/Llama-3.1-8B`）。
fn valid_recipe_hf_id(hf_id: &str) -> bool {
    let t = hf_id.trim();
    !t.is_empty()
        && !t.starts_with('/')
        && !t.contains("..")
        && !t.contains('?')
        && !t.contains('#')
        && !t.contains(char::is_whitespace)
}

/// 拉取上游目录 `models.json` → 精简目录列表（15s 超时；失败 Err 带原因）。
async fn fetch_recipes_catalog(base: &str) -> Result<Vec<RecipeCatalogItem>, String> {
    let url = format!("{base}/models.json");
    let resp = HTTP
        .get(&url)
        .timeout(RECIPES_FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("上游目录拉取失败（{url}）: {e}"))?;
    let resp = resp
        .error_for_status()
        .map_err(|e| format!("上游目录 HTTP 状态错误: {e}"))?;
    let raw: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("解析上游目录 JSON 失败: {e}"))?;
    let arr = raw
        .as_array()
        .ok_or_else(|| "上游目录不是 JSON 数组".to_string())?;
    Ok(arr
        .iter()
        .map(|item| {
            let hf_id = item
                .get("hf_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            RecipeCatalogItem {
                title: item
                    .get("title")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or(&hf_id)
                    .to_string(),
                provider: item
                    .get("provider")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                date_updated: item
                    .get("date_updated")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                hf_id,
            }
        })
        .filter(|i| !i.hf_id.is_empty())
        .collect())
}

/// 拉取单配方 `/{hf_id}.json` → 原样 JSON 透传（15s 超时；失败 Err 带原因）。
async fn fetch_recipes_recipe(base: &str, hf_id: &str) -> Result<serde_json::Value, String> {
    let url = format!("{base}/{}.json", hf_id.trim());
    let resp = HTTP
        .get(&url)
        .timeout(RECIPES_FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("上游配方拉取失败（{url}）: {e}"))?;
    let resp = resp
        .error_for_status()
        .map_err(|e| format!("上游配方 HTTP 状态错误（hf_id 不存在？）: {e}"))?;
    resp.json::<serde_json::Value>()
        .await
        .map_err(|e| format!("解析上游配方 JSON 失败: {e}"))
}

// ----------------------------------------------------------------------------
// API 网关聚合：/v1/models 真实探测（gateway/models + gateway/health）
// ----------------------------------------------------------------------------

/// 单实例 /v1/models 探测超时（网关聚合视图要求快速失败，2s）。
const GATEWAY_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// 端口扫描发现的探测超时（扫描段可能大面积死端口，更激进地快速失败 1s）。
const DISCOVERY_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// 实例表之外的本机 vLLM 常见端口扫描段：8123（本组件 `pick_free_port` 基点）
/// + 8000..=8010（vLLM 社区默认段；去重排序，8123 与 8000 段不重叠但保持防御）。
///
/// 生产 [`LlmRouteHandler::new`] 用它初始化 `discovery_ports`；测试注入空/指定
/// 端口（本机可能真有 vLLM 在默认段监听，防环境依赖的非确定性）。
fn default_discovery_ports() -> Vec<u16> {
    let mut ports = vec![8123];
    ports.extend(8000..=8010);
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// 一次 /v1/models 探测解析出的模型条目（原始对象 + 解析出的 id）。
/// `pub(crate)`：随 [`probe_vllm_models`] 供 api_gateway（from_discovery）复用。
#[derive(Debug, Clone)]
pub(crate) struct GatewayProbedModel {
    /// `data[]` 元素原样（OpenAI 兼容模型对象）。
    pub(crate) raw: serde_json::Value,
    /// `id` 字段（缺失 id 的元素整条丢弃——网关路由/计费无键可用）。
    pub(crate) id: String,
}

/// 探测 vLLM `GET http://127.0.0.1:<port>/v1/models`（[`GATEWAY_PROBE_TIMEOUT`]）。
///
/// 成功 → 解析 `data[]` 为模型条目列表（含原始对象 + id）；连接拒绝/超时/
/// 非 2xx/JSON 解析失败/缺 `data` 数组 → Err 带原因（绝不把猜测当可用）。
/// `pub(crate)`：api_gateway 的 `POST /channels`（from_discovery）复用同一探测
/// 逻辑拿端口 model_ids，保证「网关可路由模型」与「渠道导入」口径一致。
pub(crate) async fn probe_vllm_models(port: u16) -> Result<Vec<GatewayProbedModel>, String> {
    probe_vllm_models_with_timeout(port, GATEWAY_PROBE_TIMEOUT).await
}

/// [`probe_vllm_models`] 的可调超时版（端口扫描发现用 1s 激进超时）。
async fn probe_vllm_models_with_timeout(
    port: u16,
    timeout: Duration,
) -> Result<Vec<GatewayProbedModel>, String> {
    let url = format!("http://127.0.0.1:{port}/v1/models");
    let resp = HTTP
        .get(&url)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| format!("/v1/models 探测失败: {e}"))?;
    let resp = resp
        .error_for_status()
        .map_err(|e| format!("/v1/models HTTP 状态错误: {e}"))?;
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("解析 /v1/models JSON 失败: {e}"))?;
    let arr = v
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| "响应缺少 data 数组".to_string())?;
    Ok(arr
        .iter()
        .filter_map(|m| {
            let id = m.get("id").and_then(|i| i.as_str())?.to_string();
            Some(GatewayProbedModel { raw: m.clone(), id })
        })
        .collect())
}

// ----------------------------------------------------------------------------
// LlmRouteHandler
// ----------------------------------------------------------------------------

/// 模型管理路由处理器——HTTP 边界适配到 vLLM 实例列表。
///
/// 实例**定义**（id/name/model/config/port 等）双写：内存态 `instances`（运行时
/// 真值）+ SQLite `llm_instances` 表（重启恢复用，forwarding.db 同款惯例）。
/// 运行时字段（status/pid/error）同步落表，但**服务重启后一律重置**——
/// 恢复时 status='stopped'、pid/error 清空（不自动拉起，见模块头 §持久化）。
pub struct LlmRouteHandler {
    /// 实例表（运行时真值）。`Arc` 共享给 spawn 后台监测任务（换口重试要
    /// 更新实例行；见 [`monitor_addr_in_use`]）。
    instances: Arc<Mutex<Vec<ModelInstance>>>,
    counter: Mutex<u64>,
    /// 实例定义持久化（llm.db 的 llm_instances 表；建表幂等）。Arc 共享给
    /// 推理环境后台任务线程（llm_envs 子模块写 llm_environments 表）与
    /// spawn 监测任务（换口重试落库）。
    db: Arc<Mutex<Connection>>,
    /// 推理环境（vLLM venv）管理态：注册表 + 异步任务 + uv 执行器（子模块）。
    env_state: LlmEnvState,
    /// 外部 API 接入态：`llm_external_apis` 表（同库同连接；子模块。SSE 流式
    /// 特挂路由经 main.rs 注入的同一 Arc 消费——见
    /// [`LlmRouteHandler::external_state`]）。
    external: Arc<LlmExternalState>,
    /// metrics 响应缓存（instance_id → 5s TTL 条目；按需采集去抖，零后台任务）。
    metrics_cache: Mutex<std::collections::HashMap<String, MetricsCacheEntry>>,
    /// Counter 采样历史（instance_id → 最近一次抓取的 counter 值，算速率用）。
    counter_history: Mutex<std::collections::HashMap<String, CounterSample>>,
    /// recipes 目录缓存（**常驻无 TTL**：进程生命周期内一直用；唯一刷新通道
    /// `?refresh=1` 手动强制重拉并替换，刷新成功连带清空下方详情缓存）。
    recipes_catalog_cache: Mutex<Option<RecipesCacheEntry>>,
    /// recipes 单配方缓存（hf_id → 常驻条目；随目录 refresh 一并清空）。
    recipes_recipe_cache: Mutex<std::collections::HashMap<String, RecipesCacheEntry>>,
    /// recipes 上游根地址（生产恒 [`RECIPES_BASE_URL`]；测试注入本地 mock）。
    recipes_base: String,
    /// 实例表之外的本地 vLLM 端口扫描段（gateway/models 第二段发现用）。
    /// 生产 [`default_discovery_ports`]；测试注入空（默认关扫描，防本机真实
    /// vLLM 监听引入环境依赖）或指定端口（发现行为测试显式开）。
    discovery_ports: Vec<u16>,
    /// 实例拉起日志目录（env `NEXOS_LLM_SPAWN_DIR`，默认 /tmp；文件名
    /// `llm-vllm-<instance_id>.log`）。测试用 [`Self::with_spawn_log_dir`] 注入。
    spawn_log_dir: String,
    /// 旧 env `NEXOS_LLM_SPAWN_LOG` 单文件模式（Some = 所有实例共写该文件，
    /// 向后兼容；按实例日志端点此时失真，文档已注明限制）。
    spawn_log_single: Option<String>,
}

impl LlmRouteHandler {
    /// 构造 handler（生产入口，main.rs 注册用）：打开默认 DB 路径 + 建表 +
    /// 从表恢复全部实例定义（status=stopped / pid=None / error=None）。
    ///
    /// 首次启动（表空）即空表（全真实数据，无演示实例）；
    /// 之后重启以表内定义为准，**不再重复 seed、不自动拉起**。
    #[must_use]
    pub fn new() -> Self {
        Self::from_db_path(&default_db_path())
    }

    /// 用指定 DB 路径构造（测试/诊断注入：重启恢复语义可对同一文件二次构造验证）。
    #[must_use]
    pub fn with_db_path(path: &str) -> Self {
        Self::from_db_path(path)
    }

    /// 打开 DB → 建表 → 恢复（表空即空）；id 计数器越过已恢复
    /// 的最大数字后缀，避免新建实例与恢复实例撞 id。
    fn from_db_path(path: &str) -> Self {
        let conn = open_db(path).expect("打开 llm.db 失败");
        let mut instances = load_persisted_instances(&conn).unwrap_or_default();
        if instances.is_empty() {
            let demo = demo_instances();
            for i in &demo {
                let _ = persist_instance(&conn, i);
            }
            instances = demo;
        }
        let counter = instances
            .iter()
            .filter_map(|i| i.id.strip_prefix("llm-"))
            .filter_map(|s| s.parse::<u64>().ok())
            .max()
            .unwrap_or(100);
        let db = Arc::new(Mutex::new(conn));
        let (spawn_log_dir, spawn_log_single) = spawn_log_paths_from_env();
        Self {
            instances: Arc::new(Mutex::new(instances)),
            counter: Mutex::new(counter),
            env_state: LlmEnvState::new(Arc::clone(&db)),
            external: Arc::new(LlmExternalState::new(Arc::clone(&db))),
            db,
            metrics_cache: Mutex::new(std::collections::HashMap::new()),
            counter_history: Mutex::new(std::collections::HashMap::new()),
            recipes_catalog_cache: Mutex::new(None),
            recipes_recipe_cache: Mutex::new(std::collections::HashMap::new()),
            recipes_base: RECIPES_BASE_URL.into(),
            discovery_ports: default_discovery_ports(),
            spawn_log_dir,
            spawn_log_single,
        }
    }

    /// 用空列表构造（测试注入：内存库，零实例；端口扫描默认关——发现行为
    /// 测试显式设 `discovery_ports`，防本机真实 vLLM 引入环境依赖）。
    #[must_use]
    pub fn with_empty() -> Self {
        let conn = Self::in_memory_conn();
        Self::with_empty_env_state(LlmEnvState::new(Arc::new(Mutex::new(conn))))
    }

    /// 用内存库 + 注入环境态构造（测试注入：推理环境任务不真跑 uv/网络——
    /// llm_envs 子模块测试用；实例表语义与 [`Self::with_empty`] 一致）。
    #[must_use]
    pub fn with_empty_env_state(env_state: LlmEnvState) -> Self {
        let db = env_state.db_handle();
        let (spawn_log_dir, spawn_log_single) = spawn_log_paths_from_env();
        Self {
            instances: Arc::new(Mutex::new(vec![])),
            counter: Mutex::new(100),
            env_state,
            external: Arc::new(LlmExternalState::new(Arc::clone(&db))),
            db,
            metrics_cache: Mutex::new(std::collections::HashMap::new()),
            counter_history: Mutex::new(std::collections::HashMap::new()),
            recipes_catalog_cache: Mutex::new(None),
            recipes_recipe_cache: Mutex::new(std::collections::HashMap::new()),
            recipes_base: RECIPES_BASE_URL.into(),
            discovery_ports: Vec::new(),
            spawn_log_dir,
            spawn_log_single,
        }
    }

    /// 注入实例拉起日志目录（测试/诊断用：链式调用，单文件模式
    /// （`NEXOS_LLM_SPAWN_LOG`）优先、不被本方法覆盖）。生产日志目录由 env
    /// `NEXOS_LLM_SPAWN_DIR` 在构造时解析，默认 `/tmp`。
    #[must_use]
    pub fn with_spawn_log_dir(mut self, dir: &str) -> Self {
        if self.spawn_log_single.is_none() {
            self.spawn_log_dir = dir.to_string();
        }
        self
    }

    /// 内存库连接 + 全部建表（实例表 + 环境表）。
    fn in_memory_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("内存库必成功");
        create_schema(&conn).expect("建表必成功");
        conn
    }

    /// 用内存库 + 2 个 demo 实例构造（测试注入：与旧 `new()` 的 demo 语义一致，
    /// 数据隔离不落盘——依赖 demo 端口 8000/8001 的既有测试用）。
    #[must_use]
    pub fn with_demo() -> Self {
        let conn = Self::in_memory_conn();
        let demo = demo_instances();
        for i in &demo {
            let _ = persist_instance(&conn, i);
        }
        let db = Arc::new(Mutex::new(conn));
        let (spawn_log_dir, spawn_log_single) = spawn_log_paths_from_env();
        Self {
            instances: Arc::new(Mutex::new(demo)),
            counter: Mutex::new(100),
            env_state: LlmEnvState::new(Arc::clone(&db)),
            external: Arc::new(LlmExternalState::new(Arc::clone(&db))),
            db,
            metrics_cache: Mutex::new(std::collections::HashMap::new()),
            counter_history: Mutex::new(std::collections::HashMap::new()),
            recipes_catalog_cache: Mutex::new(None),
            recipes_recipe_cache: Mutex::new(std::collections::HashMap::new()),
            recipes_base: RECIPES_BASE_URL.into(),
            discovery_ports: Vec::new(),
            spawn_log_dir,
            spawn_log_single,
        }
    }

    /// 实例定义变更后同步落表（写失败仅影响重启恢复，不影响当次请求——
    /// 内存态才是运行时真值；错误吞掉并继续，与 forwarding 同款容错）。
    fn persist(&self, inst: &ModelInstance) {
        if let Ok(conn) = self.db.lock() {
            let _ = persist_instance(&conn, inst);
        }
    }

    /// 删除实例时同步删表行（同上容错）。
    fn persist_remove(&self, id: &str) {
        if let Ok(conn) = self.db.lock() {
            let _ = delete_instance_row(&conn, id);
        }
    }

    /// 当前全量实例快照。
    #[must_use]
    pub fn instances_snapshot(&self) -> Vec<ModelInstance> {
        self.instances.lock().expect("instances poisoned").clone()
    }

    /// 外部 API 接入态句柄（main.rs 经 `set_llm_external` 注入 GatewayState，
    /// http.rs 的 SSE 流式特挂路由与组件 REST 走同一 `Mutex<Connection>`）。
    #[must_use]
    pub fn external_state(&self) -> Arc<LlmExternalState> {
        Arc::clone(&self.external)
    }

    fn next_id(&self) -> String {
        let mut c = self.counter.lock().expect("counter poisoned");
        *c += 1;
        format!("llm-{}", *c)
    }

    /// 从 8123 递增找「实例表未占 + 真实试绑通过」的端口（自动选口入口）。
    ///
    /// 2026-08-21 用户裁决：实例端口基点从 8000 迁 8123——远离 8000 段
    /// （OCR:8006/网关常用段的递增碰撞区）；IM 助手探测顺序同步 8123 优先
    /// （见 im.rs probe_live_llm_url）。存量 800x 实例重启后自然迁移。
    ///
    /// 2026-08-31 升级：候选端口除实例表去重外，还要对 `0.0.0.0:<port>` 真实
    /// `TcpListener::bind` 试绑（成功即 drop）——生产 8123 被外部进程占用时旧
    /// 算法照样返回 8123，vLLM 拉起 `Address already in use` 失败。**注意
    /// TOCTOU 窗口**：试绑释放与 vLLM 子进程真绑之间第三方仍可能抢口——由
    /// spawn 后 30s 监测 + 换口重试（[`Self::spawn_vllm`]）兜底。
    fn pick_free_port(&self) -> u16 {
        let used = used_ports(&self.instances);
        pick_free_port_from(INSTANCE_PORT_BASE, &used)
    }

    /// 手动指定端口校验（`POST /instances` 的 `port` 字段；None = 自动选）。
    ///
    /// 越界（<1024 或 >65535）→ 400；实例表冲突 / OS 保留段（8558/7070/
    /// 11080/11081）/ 真实试绑被占 → 409（带可读原因）。入参 u64——超 u16
    /// 的值也统一走 400，而不是 serde 解析失败。
    fn validate_manual_port(&self, port: u64) -> Result<(), (u16, String)> {
        if !(u64::from(INSTANCE_PORT_MIN)..=u64::from(u16::MAX)).contains(&port) {
            return Err((
                400,
                format!("port {port} 越界（须 {INSTANCE_PORT_MIN}..=65535）"),
            ));
        }
        let port = u16::try_from(port).unwrap_or(INSTANCE_PORT_MIN);
        if RESERVED_INSTANCE_PORTS.contains(&port) {
            return Err((
                409,
                format!(
                    "port {port} 是 OS 保留端口（8558=os-api / 7070=p2p / \
                     11080,11081=网络出口 SOCKS5），不可用于 vLLM 实例"
                ),
            ));
        }
        {
            let instances = self.instances.lock().expect("instances poisoned");
            if let Some(i) = instances.iter().find(|i| i.port == port) {
                return Err((
                    409,
                    format!("port {port} 已被实例 {}（{}）占用", i.id, i.name),
                ));
            }
        }
        if !port_bindable(port) {
            return Err((
                409,
                format!("port {port} 已被本机其它进程占用（真实试绑失败）"),
            ));
        }
        Ok(())
    }

    /// 该实例的拉起日志文件路径（spawn 写入与 `GET /:id/log` 读取同一口径：
    /// 单文件模式（env `NEXOS_LLM_SPAWN_LOG`）→ 该文件；否则
    /// `<spawn_log_dir>/llm-vllm-<instance_id>.log`）。
    fn instance_log_path(&self, instance_id: &str) -> String {
        instance_log_path_with(
            &self.spawn_log_dir,
            self.spawn_log_single.as_deref(),
            instance_id,
        )
    }

    /// 统计快照（不探测 GPU，由 /stats handler 再叠加 GPU 探测）。
    fn stats_snapshot(&self) -> (usize, usize, usize) {
        let instances = self.instances.lock().expect("instances poisoned");
        let total = instances.len();
        let running = instances.iter().filter(|i| i.status == "running").count();
        let stopped = instances.iter().filter(|i| i.status == "stopped").count();
        (total, running, stopped)
    }

    /// 实例拉起用的 vllm 二进制 + bin 目录（默认环境；注册表无可用默认行时
    /// 回退旧硬编码 [`VLLM_BIN`]/[`VLLM_ENV_PATH`]，向后兼容存量部署）。
    ///
    /// 2026-08-31 起机器 venv 由推理环境注册表（[`llm_envs`] 子模块）管理：
    /// 查 `llm_environments` 表 is_default=1 且 status=ready 的行 → 其
    /// `<path>/bin/vllm` 与 `<path>/bin`。
    fn default_env_bin(&self) -> (String, String) {
        let resolved = match self.db.lock() {
            Ok(conn) => llm_envs::default_ready_env(&conn).map(|r| {
                (
                    format!("{}/bin/vllm", r.path),
                    format!("{}/bin", r.path),
                    r.name,
                )
            }),
            Err(_) => None,
        };
        match resolved {
            Some((bin, dir, name)) => {
                eprintln!("[llm-env] 默认推理环境 {name} → {bin}");
                (bin, dir)
            }
            None => {
                eprintln!("[llm-env] 注册表无 ready 默认环境，回退硬编码 {VLLM_BIN}");
                (VLLM_BIN.to_string(), VLLM_ENV_PATH.to_string())
            }
        }
    }

    /// 按 env_name 解析拉起环境：None → [`Self::default_env_bin`]（含回退）；
    /// 指定名 → 该环境行（必须存在且 ready，否则 Err——用户显式指定的环境
    /// 不可用时**不静默回退**，避免在旧版本上误拉起）。
    fn env_bin_for(&self, env_name: Option<&str>) -> Result<(String, String), String> {
        let Some(name) = env_name else {
            return Ok(self.default_env_bin());
        };
        let row = self
            .db
            .lock()
            .ok()
            .and_then(|conn| llm_envs::env_row_by_name(&conn, name));
        match row {
            Some(r) if r.status == "ready" => {
                let bin = format!("{}/bin/vllm", r.path);
                eprintln!("[llm-env] 指定推理环境 {name} → {bin}");
                Ok((bin, format!("{}/bin", r.path)))
            }
            Some(r) => Err(format!(
                "推理环境 {name} 状态为 {}（需 ready 才能拉起实例）",
                r.status
            )),
            None => Err(format!("推理环境 {name} 不存在")),
        }
    }

    /// 真实 spawn vllm serve 子进程，成功返回 pid。
    ///
    /// 对接推理环境注册表（[`llm_envs`] 子模块）：vllm 不在默认 PATH，必须用
    /// 绝对路径（默认环境 [`Self::default_env_bin`] 解析，`env_name` 显式指定
    /// 时按名解析）；且 PATH 必须含该环境 bin 目录（vLLM 编译 CUDA kernel
    /// 需要 ninja，ninja 位于该目录）。不 await 完成（后台跑）。vllm 不存在 /
    /// spawn 失败返回 Err（caller 降级为 error）。
    ///
    /// 2026-08-31：stdout+stderr 落**按实例**日志文件（[`Self::instance_log_path`]，
    /// env `NEXOS_LLM_SPAWN_DIR`/`NEXOS_LLM_SPAWN_LOG`）；并启动 30s 后台监测
    /// （[`monitor_addr_in_use`]）——子进程退出且日志含端口占用 → 换口重试一次。
    /// spawn 命令以**行 port 为唯一真相源**构造（config.port 强制对齐，见模块头
    /// §端口唯一真相源）。
    async fn spawn_vllm(&self, inst: &ModelInstance) -> Result<(u32, String), String> {
        let (vllm_bin, vllm_bin_dir) = self.env_bin_for(inst.env_name.as_deref())?;
        let log_path = self.instance_log_path(&inst.id);
        // 行 port 唯一真相：spawn 参数强制用行 port（config JSON 里的旧值不信任）
        let mut config = inst.config.clone();
        config.port = inst.port;
        let spawned = spawn_vllm_child(&inst.model, &config, &vllm_bin, &vllm_bin_dir, &log_path)?;
        // 真实拉起命令（完整 argv 单行；比构造值多真实二进制路径，start 响应
        // 与落库的 launch_command 由此而来）
        let real_command = format!("{vllm_bin} {}", build_vllm_serve_cmd(&inst.model, &config).join(" "));
        let pid = spawned.pid;
        // 后台监测：前 30s 内退出 + 日志含 Address already in use → 换口重试一次
        let instances = Arc::clone(&self.instances);
        let db = Arc::clone(&self.db);
        let model = inst.model.clone();
        let instance_id = inst.id.clone();
        let spawn_fn: VllmSpawnFn =
            Arc::new(move |cfg| spawn_vllm_child(&model, cfg, &vllm_bin, &vllm_bin_dir, &log_path));
        tokio::spawn(async move {
            monitor_addr_in_use(
                SpawnMonitorCtx {
                    instances,
                    db,
                    instance_id,
                    config,
                },
                spawn_fn,
                spawned,
                SPAWN_MONITOR_WINDOW,
                SPAWN_MONITOR_POLL,
            )
            .await;
        });
        Ok((pid, real_command))
    }

    /// 停止实例子进程（kill pid）。
    fn kill_instance(pid: u32) {
        // 杀不掉也继续（实例状态由 caller 改为 stopped）
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .spawn();
    }

    /// 健康探测：取实例 port → reqwest GET /health + /v1/models。
    ///
    /// 探测失败（vllm 未运行/网络不通）→ alive=false，不 panic。
    async fn probe_health(port: u16) -> HealthInfo {
        let checked_at = now_iso();
        let health_url = format!("http://127.0.0.1:{port}/health");
        let models_url = format!("http://127.0.0.1:{port}/v1/models");
        // /health
        let alive = HTTP
            .get(&health_url)
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map(|_| true)
            .unwrap_or(false);
        // /v1/models
        let (model_loaded, models) = if alive {
            match HTTP
                .get(&models_url)
                .timeout(Duration::from_secs(3))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    match resp.json::<serde_json::Value>().await {
                        Ok(v) => {
                            let ids = v
                                .get("data")
                                .and_then(|d| d.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|m| {
                                            m.get("id").and_then(|i| i.as_str()).map(String::from)
                                        })
                                        .collect::<Vec<_>>()
                                })
                                .unwrap_or_default();
                            (!ids.is_empty(), ids)
                        }
                        Err(_) => (false, vec![]),
                    }
                }
                _ => (false, vec![]),
            }
        } else {
            (false, vec![])
        };
        HealthInfo {
            alive,
            model_loaded,
            models,
            checked_at,
        }
    }

    /// 推理测试：取 port → reqwest POST /v1/chat/completions。
    ///
    /// 失败返回 Err（caller 包成 error 响应），不 panic。
    ///
    /// 2026-08-31：兼容 vLLM 0.28 思考输出的 `reasoning` 键（0.27 为
    /// `reasoning_content`）——小 max_tokens 下思考段吃满、content 为 null 时
    /// 不再报「缺少 content」像故障，而是返回空 content + reasoning 段 +
    /// finish_reason/usage（前端折叠展示并提示 token 去向）。
    ///
    /// 2026-09-04 起 `pub(crate)`：影片管线（film.rs）的 local.chat（本地 LLM
    /// 实例直连）经同一实例调用面复用（连同 [`ChatBody`]/[`ChatMessage`]/
    /// [`ChatOutcome`] 的 crate 内可见）；仅可见性变化，零行为回归。
    ///
    /// 2026-09-04（第二批）：请求体改为 serde 直接序列化 [`ChatBody`]（缺省值
    /// 就地补齐）——`chat_template_kwargs` 等顶层透传字段原样到达 vLLM（旧
    /// `json!` 手拼会静默丢掉未知字段）。默认 max_tokens=256 / temperature=0.7
    /// 与旧版逐字节一致（缺省字段补齐后序列化）。
    pub(crate) async fn chat_complete(
        port: u16,
        model_name: &str,
        body: &ChatBody,
    ) -> Result<ChatOutcome, String> {
        let url = format!("http://127.0.0.1:{port}/v1/chat/completions");
        // 缺省补齐后整包序列化：None 的可选键不出现在请求体（skip_serializing_if）
        let outbound = ChatBody {
            messages: body.messages.clone(),
            max_tokens: Some(body.max_tokens.unwrap_or(256)),
            temperature: Some(body.temperature.unwrap_or(0.7)),
            chat_template_kwargs: body.chat_template_kwargs.clone(),
        };
        let mut payload = serde_json::to_value(&outbound)
            .map_err(|e| format!("构造推理请求体失败: {e}"))?;
        // vLLM 设了 --served-model-name 时只认该名字；未设时接受模型路径。
        payload["model"] = serde_json::Value::String(model_name.to_string());
        let mut req = HTTP.post(&url).timeout(Duration::from_secs(60));
        // vLLM 实例启用了 --api-key 时（env NEXOS_VLLM_API_KEY 透传），内部转发也要带
        if let Some(k) = env_non_empty_llm("NEXOS_VLLM_API_KEY") {
            req = req.bearer_auth(k);
        }
        let resp = req
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("推理请求发送失败（vllm 未运行？）: {e}"))?;
        let resp = resp
            .error_for_status()
            .map_err(|e| format!("推理请求失败（HTTP 错误）: {e}"))?;
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("解析推理响应失败: {e}"))?;
        if let Some(err) = v.get("error") {
            return Err(format!("vllm 返回错误: {err}"));
        }
        let choice = v
            .get("choices")
            .and_then(|c| c.get(0))
            .ok_or_else(|| "推理响应缺少 choices[0]".to_string())?;
        let message = choice.get("message");
        let content = message
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        // 思考段双键兼容：vLLM 0.28 `reasoning` / 0.27 `reasoning_content`
        let reasoning = message
            .and_then(|m| {
                m.get("reasoning")
                    .or_else(|| m.get("reasoning_content"))
                    .and_then(|r| r.as_str())
                    .filter(|r| !r.is_empty())
                    .map(String::from)
            })
            .unwrap_or_default();
        let finish_reason = choice
            .get("finish_reason")
            .and_then(|f| f.as_str())
            .map(String::from);
        let total_tokens = v
            .get("usage")
            .and_then(|u| u.get("total_tokens"))
            .and_then(|t| t.as_u64());
        if content.is_empty() && reasoning.is_empty() {
            return Err(format!(
                "推理响应 choices[0].message 无 content/reasoning（finish_reason={}；小 max_tokens 可能全被思考段吃掉，可调大 max_tokens 重试）",
                finish_reason.as_deref().unwrap_or("unknown")
            ));
        }
        Ok(ChatOutcome {
            content,
            reasoning,
            finish_reason,
            total_tokens,
        })
    }

    /// 截图分析：转发 base64 图片 + prompt 到本机 vLLM 视觉推理（OpenAI 兼容）。
    ///
    /// **先探活 vLLM**（reqwest GET :8000/health），不在线直接返回 Err（caller 降级 503），
    /// 不 panic。在线时构造 OpenAI 兼容多模态请求（image_url data URL + text）转发，
    /// 解析 `choices[0].message.content` + `usage.total_tokens`。
    ///
    /// 这是给 AI 主代理调用的工具，用于分析截图。
    async fn analyze_image(body: &AnalyzeImageBody) -> Result<AnalyzeImageResponse, String> {
        // 1. 先探活 vLLM（:8000/health），不在线直接降级
        let alive = HTTP
            .get("http://127.0.0.1:8000/health")
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map(|_| true)
            .unwrap_or(false);
        if !alive {
            return Err("vLLM 服务未运行，请先在模型管理启动实例".to_string());
        }
        // 2. 校验输入
        if body.image_base64.trim().is_empty() {
            return Err("image_base64 不可为空".to_string());
        }
        if body.prompt.trim().is_empty() {
            return Err("prompt 不可为空".to_string());
        }
        let mime = body
            .image_mime
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .unwrap_or("image/png");
        let max_tokens = body.max_tokens.unwrap_or(400);
        // 3. 构造 OpenAI 兼容多模态请求
        let data_url = format!("data:{mime};base64,{}", body.image_base64);
        let payload = serde_json::json!({
            "model": VLLM_VL_MODEL,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image_url", "image_url": {"url": data_url}},
                    {"type": "text", "text": body.prompt},
                ]
            }],
            "max_tokens": max_tokens,
        });
        // 4. reqwest 转发到本机 vLLM
        let resp = HTTP
            .post(VLLM_VL_ENDPOINT)
            .timeout(Duration::from_secs(60))
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("视觉推理请求发送失败: {e}"))?;
        let resp = resp
            .error_for_status()
            .map_err(|e| format!("视觉推理请求失败（HTTP 错误）: {e}"))?;
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("解析视觉推理响应失败: {e}"))?;
        if let Some(err) = v.get("error") {
            return Err(format!("vLLM 返回错误: {err}"));
        }
        let description = v
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .ok_or_else(|| "视觉推理响应缺少 choices[0].message.content".to_string())?
            .to_string();
        let tokens_used = v
            .get("usage")
            .and_then(|u| u.get("total_tokens"))
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        Ok(AnalyzeImageResponse {
            description,
            model: VLLM_VL_MODEL.to_string(),
            tokens_used,
        })
    }

    /// 采集单实例轻量监控指标（`GET .../instances/:id/metrics` 的核心）。
    ///
    /// 按需采集：命中 5s 缓存直接返回；否则抓 vLLM /metrics（真实 3s /
    /// 模拟探测 200ms 超时）。真实模式下不可达 → `reachable:false` +
    /// `metrics:null`（200 语义）；模拟模式下不可达才合成（`simulated:true`）。
    /// `now` 可注入（测试推进时间验证缓存过期与 Counter 速率）。
    async fn collect_metrics(
        &self,
        instance_id: &str,
        port: u16,
        now: std::time::Instant,
    ) -> serde_json::Value {
        // 1) 缓存命中（同实例 5s 去抖，窗口内零抓取）
        {
            let cache = self.metrics_cache.lock().expect("metrics cache poisoned");
            if let Some(entry) = cache.get(instance_id) {
                if metrics_cache_is_fresh(entry.at, now) {
                    return entry.body.clone();
                }
            }
        }
        // 2) 抓取（模拟模式先 200ms 探测真实端口，通则仍用真实数据）
        let simulate = simulate_enabled();
        let timeout = if simulate {
            METRICS_SIMULATE_PROBE_TIMEOUT
        } else {
            METRICS_FETCH_TIMEOUT
        };
        let (reachable, simulated, snapshot) = match fetch_metrics_text(port, timeout).await {
            Ok(text) => {
                let raw = parse_vllm_metrics(&text);
                // Counter 速率：与上一采样差值 / 间隔（仅真实抓取更新历史）
                let cur = CounterSample {
                    at: now,
                    generation_tokens_total: raw.generation_tokens_total,
                    prompt_tokens_total: raw.prompt_tokens_total,
                    request_success_total: raw.request_success_total,
                };
                let prev = self
                    .counter_history
                    .lock()
                    .expect("counter history poisoned")
                    .get(instance_id)
                    .copied();
                let rates = compute_counter_rates(prev, &cur);
                self.counter_history
                    .lock()
                    .expect("counter history poisoned")
                    .insert(instance_id.to_string(), cur);
                (true, false, Some(build_metrics_snapshot(&raw, rates)))
            }
            Err(_) if simulate => (
                false,
                true,
                Some(synthetic_metrics(instance_id, epoch_secs_f64())),
            ),
            Err(_) => (false, false, None),
        };
        // 3) 组装 + 写缓存（不可达也缓存 5s，避免高频探测打死端口）
        let body = serde_json::to_value(InstanceMetricsResponse {
            instance_id: instance_id.to_string(),
            reachable,
            simulated,
            collected_at: now_iso(),
            base_url: format!("http://127.0.0.1:{port}"),
            metrics: snapshot,
        })
        .unwrap_or(serde_json::Value::Null);
        self.metrics_cache
            .lock()
            .expect("metrics cache poisoned")
            .insert(
                instance_id.to_string(),
                MetricsCacheEntry {
                    at: now,
                    body: body.clone(),
                },
            );
        body
    }

    /// `GET /api/v1/llm/recipes/catalog[?refresh=1]` 核心：**常驻进程缓存**
    /// （无 TTL，进程生命周期内一直用）→ 命中即秒回不打上游；`refresh=1`
    /// 强制重拉上游 `models.json`（15s 超时）并替换缓存，同时**清空单配方
    /// 详情缓存**（目录刷新带动详情刷新——上游目录变化时旧详情一并作废）。
    /// 失败返回 (502, 原因) 且**保留旧缓存**（下次正常读仍可用）。
    async fn recipes_catalog_body(
        &self,
        refresh: bool,
    ) -> Result<serde_json::Value, (u16, String)> {
        // 1) 缓存命中（常驻无过期；仅手动 refresh=1 绕过）
        if !refresh {
            let cache = self
                .recipes_catalog_cache
                .lock()
                .expect("recipes cache poisoned");
            if let Some(entry) = cache.as_ref() {
                return Ok(recipes_envelope(entry, true));
            }
        }
        // 2) 外网拉取 + 精简映射（浏览器直连会被 CORS 挡，代理只在服务端做）
        let items = fetch_recipes_catalog(&self.recipes_base)
            .await
            .map_err(|e| (502u16, e))?;
        let body = serde_json::to_value(&items).unwrap_or_else(|_| serde_json::json!([]));
        let entry = RecipesCacheEntry {
            fetched_at: chrono::Utc::now(),
            body,
        };
        // 3) 写缓存（常驻）+ 清空单配方详情缓存（跟随目录刷新）
        *self
            .recipes_catalog_cache
            .lock()
            .expect("recipes cache poisoned") = Some(entry.clone());
        self.recipes_recipe_cache
            .lock()
            .expect("recipes cache poisoned")
            .clear();
        Ok(recipes_envelope(&entry, false))
    }

    /// `GET /api/v1/llm/recipes/recipe?hf_id=` 核心：hf_id 校验 → **常驻进程
    /// 缓存**（无 TTL；随目录 refresh 一并清空，无独立刷新参数——保持简单）
    /// → 未命中才拉上游 `/{hf_id}.json` 原样透传。参数缺失/非法 400；上游
    /// 失败 502 带原因。
    async fn recipes_recipe_body(
        &self,
        hf_id: &str,
    ) -> Result<serde_json::Value, (u16, String)> {
        let hf_id = hf_id.trim();
        if !valid_recipe_hf_id(hf_id) {
            return Err((
                400u16,
                "hf_id 参数缺失或非法（形如 meta-llama/Llama-3.1-8B）".into(),
            ));
        }
        // 1) 缓存命中（常驻无过期）
        {
            let cache = self
                .recipes_recipe_cache
                .lock()
                .expect("recipes cache poisoned");
            if let Some(entry) = cache.get(hf_id) {
                return Ok(entry.body.clone());
            }
        }
        // 2) 外网拉取（原样透传，不加工）
        let body = fetch_recipes_recipe(&self.recipes_base, hf_id)
            .await
            .map_err(|e| (502u16, e))?;
        // 3) 写缓存（常驻）
        self.recipes_recipe_cache
            .lock()
            .expect("recipes cache poisoned")
            .insert(
                hf_id.to_string(),
                RecipesCacheEntry {
                    fetched_at: chrono::Utc::now(),
                    body: body.clone(),
                },
            );
        Ok(body)
    }

    /// `GET /api/v1/llm/gateway/models` 核心（两段式真实探测）：
    ///
    /// 1) 实例表 running 实例**并发探测** `/v1/models`（join_all，各自 2s 超时
    ///    互不拖累）。成功 → `gateway_visible`（`discovered:false`，原始模型
    ///    对象 + 解析出的 id 列表）；失败 → `unreachable`（带原因）**且 status
    ///    回落 stopped 并落库**（声称 running 但端口已死 = 状态脱节，当场修正）。
    ///    status 非 running 的实例两组都不进。
    /// 2) **实例表之外的本地 vLLM 端口扫描发现**：`discovery_ports`（生产
    ///    [`default_discovery_ports`]）去掉实例表已占端口后逐个探测（1s 快速
    ///    失败，并发）；命中 → `discovered:true` 条目（`instance_id:null`，名
    ///    「发现的 vLLM :<port>」）。扫描失败静默跳过（猜的端口死着不是异常）。
    async fn gateway_models_body(&self) -> GatewayModelsResponse {
        // 锁内只做快照（running 探测目标 + 全表端口去重集），探测在锁外并发跑
        // （持锁 await 是反模式）
        let instances: Vec<ModelInstance> = {
            let instances = self.instances.lock().expect("instances poisoned");
            instances.clone()
        };
        let running: Vec<ModelInstance> = instances
            .iter()
            .filter(|i| i.status == "running")
            .cloned()
            .collect();
        let probes = running
            .iter()
            .map(|i| async move { probe_vllm_models(i.port).await });
        let results = futures::future::join_all(probes).await;
        let mut gateway_visible = Vec::new();
        let mut unreachable = Vec::new();
        let mut demoted: Vec<ModelInstance> = Vec::new();
        for (inst, res) in running.iter().zip(results) {
            match res {
                Ok(models) => gateway_visible.push(GatewayModelEntry {
                    instance_id: Some(inst.id.clone()),
                    name: inst.name.clone(),
                    served_model_name: inst.config.served_model_name.clone(),
                    port: inst.port,
                    alive: true,
                    model_ids: models.iter().map(|m| m.id.clone()).collect(),
                    models: models.into_iter().map(|m| m.raw).collect(),
                    discovered: false,
                }),
                Err(reason) => {
                    unreachable.push(GatewayUnreachableEntry {
                        instance_id: inst.id.clone(),
                        name: inst.name.clone(),
                        port: inst.port,
                        reason,
                    });
                    // status 健康修正：DB 声称 running 但端口已死 → 回落 stopped
                    let mut d = (*inst).clone();
                    d.status = "stopped".into();
                    demoted.push(d);
                }
            }
        }
        self.apply_status_correction(&demoted);

        // —— 第二段：实例表之外的本地 vLLM 端口扫描发现 ——
        // 去重口径 = 实例表**全部**端口（不论状态）：实例表已纳管的端口由第一段
        // （或 /instances 的 stopped→running 修正）负责，扫描段不得重复上报。
        let known_ports: std::collections::HashSet<u16> =
            instances.iter().map(|i| i.port).collect();
        let candidates: Vec<u16> = self
            .discovery_ports
            .iter()
            .copied()
            .filter(|p| !known_ports.contains(p))
            .collect();
        let scans = futures::future::join_all(candidates.iter().map(|p| async move {
            (
                *p,
                probe_vllm_models_with_timeout(*p, DISCOVERY_PROBE_TIMEOUT).await,
            )
        }))
        .await;
        for (port, res) in scans {
            if let Ok(models) = res {
                gateway_visible.push(GatewayModelEntry {
                    instance_id: None,
                    name: format!("发现的 vLLM :{port}"),
                    served_model_name: None,
                    port,
                    alive: true,
                    model_ids: models.iter().map(|m| m.id.clone()).collect(),
                    models: models.into_iter().map(|m| m.raw).collect(),
                    discovered: true,
                });
            }
        }
        GatewayModelsResponse {
            gateway_visible,
            unreachable,
        }
    }

    /// 实例 status 修正后回写（内存 + DB）。`updated` 为修正后的实例快照——
    /// 调用方保证**只改了 status 字段**（pid/error/health 原样），此处按 id
    /// 只同步 status 到内存态，再全量 persist（INSERT OR REPLACE 幂等落库）。
    fn apply_status_correction(&self, updated: &[ModelInstance]) {
        if updated.is_empty() {
            return;
        }
        {
            let mut instances = self.instances.lock().expect("instances poisoned");
            for u in updated {
                if let Some(i) = instances.iter_mut().find(|i| i.id == u.id) {
                    i.status = u.status.clone();
                    // 端口收敛（行 port 唯一真相）：config.port 随行 port 覆盖
                    if i.config.port != i.port {
                        i.config.port = i.port;
                    }
                }
            }
        }
        for u in updated {
            self.persist(u);
        }
    }

    /// `GET /api/v1/llm/instances` 列表返回前的**status 健康修正 + 端口收敛**
    /// （返回修正后的全量快照）：
    ///
    /// - running → /v1/models 验活；死了改回 stopped（落库）；
    /// - stopped → 探测端口；活且 served_model_name 匹配（[`served_model_name_matches`]）
    ///   → 修正 running（落库）——用户手动起的 vLLM 恰好落在实例端口上时不再
    ///   「明明活着却显示停止」；
    /// - starting → 探测端口；活且 /v1/models 就绪（模型加载完）→ 修正 running
    ///   （2026-08-31 缺陷修复：模型加载可远超拉起时的一次性探测窗口（实测
    ///   19G 权重 ~80s+），此前 starting 永久卡死；探测仍不通保持 starting，
    ///   不猜成 error）；死了保持 starting（spawn 后 30s 监测负责换口/落 error）；
    /// - error 不动（错误态有自己的语义，不靠探测猜）。
    ///
    /// **端口收敛（同日缺陷修复）**：行 port 是唯一真相源——任何行 config.port
    /// ≠ 行 port 的双写残留（历史手动改库造成「spawn 绑 A、探测打 B」永久卡
    /// starting）在本次修正中统一覆盖为行 port 并落库。
    ///
    /// **只修正 status/port 字段**（pid/error/health 一律不动——pid 不可凭探测
    /// 反推）。探测并发跑（各 2s 超时互不拖累），回写一次锁完成。
    async fn reconcile_instance_statuses(&self) -> Vec<ModelInstance> {
        let snapshot: Vec<ModelInstance> =
            self.instances.lock().expect("instances poisoned").clone();
        // 端口收敛：行 port 唯一真相，config JSON 只是随写镜像
        let port_fixed: Vec<ModelInstance> = snapshot
            .into_iter()
            .map(|mut i| {
                if i.config.port != i.port {
                    i.config.port = i.port;
                }
                i
            })
            .collect();
        let mut need_port_persist: Vec<ModelInstance> = Vec::new();
        {
            let instances = self.instances.lock().expect("instances poisoned");
            for i in &port_fixed {
                if let Some(cur) = instances.iter().find(|c| c.id == i.id) {
                    if cur.config.port != cur.port {
                        need_port_persist.push(i.clone());
                    }
                }
            }
        }
        // 只探测需要修正判定的实例（running/stopped/starting），其余原样返回
        let targets: Vec<&ModelInstance> = port_fixed
            .iter()
            .filter(|i| i.status == "running" || i.status == "stopped" || i.status == "starting")
            .collect();
        if targets.is_empty() {
            self.apply_status_correction(&need_port_persist);
            return port_fixed;
        }
        let probes = targets
            .iter()
            .map(|i| async move { (i.id.clone(), probe_vllm_models(i.port).await) });
        let results = futures::future::join_all(probes).await;
        let mut corrected: Vec<ModelInstance> = need_port_persist;
        let mut out = port_fixed;
        for (id, res) in results {
            let Some(inst) = out.iter_mut().find(|i| i.id == id) else {
                continue;
            };
            match (inst.status.as_str(), res) {
                ("running", Err(_)) => {
                    inst.status = "stopped".into();
                    corrected.push(inst.clone());
                }
                ("stopped", Ok(models)) if served_model_name_matches(inst, &models) => {
                    inst.status = "running".into();
                    corrected.push(inst.clone());
                }
                // starting + 探活成功 + 模型就绪 → running（不再卡 starting）
                ("starting", Ok(models)) if served_model_name_matches(inst, &models) => {
                    inst.status = "running".into();
                    corrected.push(inst.clone());
                }
                _ => {}
            }
        }
        self.apply_status_correction(&corrected);
        out
    }

    /// `GET /api/v1/llm/gateway/health` 核心：running 数、可达数、不可达数，
    /// 以及总 GPU 显存（复用 [`detect_gpu`]，无 GPU 为 0）。可达性口径与
    /// [`Self::gateway_models_body`] 完全一致（同一次真实探测语义）；reachable
    /// 只计实例表条目（`discovered:false`）——端口扫描发现的另算，保持
    /// reachable + unreachable == running_total 不变量。
    async fn gateway_health_body(&self) -> GatewayHealthResponse {
        let running_total = {
            let instances = self.instances.lock().expect("instances poisoned");
            instances.iter().filter(|i| i.status == "running").count()
        };
        let view = self.gateway_models_body().await;
        let gpu = detect_gpu().await;
        GatewayHealthResponse {
            running_total,
            reachable: view
                .gateway_visible
                .iter()
                .filter(|e| !e.discovered)
                .count(),
            unreachable: view.unreachable.len(),
            gpu_available: gpu.available,
            gpu_backend: gpu.backend,
            // 统一内存卡（GB10）memory_total_mib=None 不计入和（诚实 0 +
            // gpu_unified_memory=true 告知消费方容量语义变了）
            gpu_memory_total_mib: gpu.devices.iter().filter_map(|d| d.memory_total_mib).sum(),
            gpu_unified_memory: gpu.devices.iter().any(|d| d.unified_memory),
        }
    }
}

/// stopped→running 修正的匹配判定：实例未配置 served_model_name（None）时端口
/// 应答即采信（vLLM 默认落到 model 路径名，无从预判）；已配置时要求 /v1/models
/// 的 `data[].id` 含该名（防端口上恰好跑着别的服务被误标 running）。
fn served_model_name_matches(inst: &ModelInstance, models: &[GatewayProbedModel]) -> bool {
    match inst.config.served_model_name.as_deref() {
        None => true,
        Some(name) => models.iter().any(|m| m.id == name),
    }
}

// ----------------------------------------------------------------------------
// 端口选取 / 拉起日志 / spawn 监测（2026-08-31，见模块头 §端口选取）
// ----------------------------------------------------------------------------

/// 端口真实占用探测：对 `0.0.0.0:<port>` 做一次真实 TCP 试绑（成功即 drop）。
/// 只绑 0.0.0.0（与 vLLM 默认 `--host 0.0.0.0` 同口径）。**TOCTOU 说明**：试绑
/// drop 与 vLLM 子进程真绑之间第三方仍可能抢口——该窗口由 spawn 后 30s 监测
/// （[`monitor_addr_in_use`]）换口重试兜底，不在此处消除。
fn port_bindable(port: u16) -> bool {
    std::net::TcpListener::bind(("0.0.0.0", port)).is_ok()
}

/// 从 `start` 递增选「表内未占 + 真实试绑通过」的端口（[`LlmRouteHandler::
/// pick_free_port`] 的核心，抽自由函数便于单测）。全段被占时退回表内未占的
/// 第一个（语义同旧版，现实不可能触达——由 spawn 重试链路兜底）。
fn pick_free_port_from(start: u16, used: &std::collections::HashSet<u16>) -> u16 {
    for port in start..=u16::MAX {
        if !used.contains(&port) && port_bindable(port) {
            return port;
        }
    }
    (start..=u16::MAX)
        .find(|p| !used.contains(p))
        .unwrap_or(start)
}

/// 实例表当前占用端口集合。
fn used_ports(instances: &Mutex<Vec<ModelInstance>>) -> std::collections::HashSet<u16> {
    instances
        .lock()
        .expect("instances poisoned")
        .iter()
        .map(|i| i.port)
        .collect()
}

/// 拉起日志 env 解析（构造时读一次，运行期不再碰 env）：
/// 返回 (目录, 单文件覆盖)。设了 `NEXOS_LLM_SPAWN_LOG` → 单文件模式（向后
/// 兼容：所有实例共写一个文件，按实例日志端点此时失真）；否则目录取
/// `NEXOS_LLM_SPAWN_DIR`（默认 `/tmp`），文件按实例分。
fn spawn_log_paths_from_env() -> (String, Option<String>) {
    let single = env_non_empty_llm("NEXOS_LLM_SPAWN_LOG");
    let dir =
        env_non_empty_llm("NEXOS_LLM_SPAWN_DIR").unwrap_or_else(|| SPAWN_LOG_DIR_DEFAULT.into());
    (dir, single)
}

/// 实例拉起日志文件路径（spawn 写入与 `GET /:id/log` 读取共用，测试可直接注入）。
fn instance_log_path_with(dir: &str, single: Option<&str>, instance_id: &str) -> String {
    if let Some(p) = single {
        return p.to_string();
    }
    format!("{dir}/llm-vllm-{instance_id}.log")
}

/// 日志尾是否报「端口被占」（vLLM/uvicorn 两种形态：Python `OSError: [Errno
/// 98] Address already in use`、uvicorn `error while attempting to bind on
/// address ('0.0.0.0', 8000): address already in use`）。
fn log_says_addr_in_use(log_tail: &str) -> bool {
    let lower = log_tail.to_ascii_lowercase();
    lower.contains("address already in use")
        || lower.contains("errno 98")
        || lower.contains("eaddrinuse")
}

/// 读文件尾至多 `max_bytes` 字节（不足全读；UTF-8 边界用 lossy 容错）。
fn read_log_tail_bytes(path: &str, max_bytes: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let Ok(meta) = f.metadata() else {
        return String::new();
    };
    let len = meta.len();
    let start = len.saturating_sub(max_bytes);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buf = Vec::with_capacity((len - start) as usize);
    if f.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// 读日志文件尾 `max_lines` 行（单次读取字节上限 `max_bytes`；文件不存在 →
/// `io::ErrorKind::NotFound`）。截断边界（`start > 0`）时丢弃首个不完整行。
fn read_log_tail_lines(
    path: &str,
    max_lines: usize,
    max_bytes: u64,
) -> std::io::Result<Vec<String>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(max_bytes);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if start > 0 && lines.len() > 1 {
        lines.remove(0); // 头部截断的半行不可信
    }
    if lines.len() > max_lines {
        let cut = lines.len() - max_lines;
        lines.drain(..cut);
    }
    Ok(lines)
}

/// 向实例日志追加一行（换口重试的分隔标记等；失败静默——日志尽力而为）。
fn append_log_line(log_path: &str, line: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// 一次 vllm spawn 的产物（pid + 子进程句柄 + 日志路径）。
struct VllmSpawn {
    pid: u32,
    child: tokio::process::Child,
    log_path: String,
}

/// spawn 执行器（换口重试注入点；生产 = [`spawn_vllm_child`] 闭包捕获
/// bin/日志路径，测试注入 fake 子进程——见 tests::monitor_addr_in_use_*）。
type VllmSpawnFn = std::sync::Arc<dyn Fn(&VllmConfig) -> Result<VllmSpawn, String> + Send + Sync>;

/// 真实拉起 vllm serve 子进程（stdout+stderr 落 `log_path`，stdin 静默；
/// PATH 含环境 bin 目录——vLLM 编译 CUDA kernel 要找 ninja）。子进程由 OS
/// 收养后台跑，句柄留给 30s 监测用。
fn spawn_vllm_child(
    model: &str,
    config: &VllmConfig,
    vllm_bin: &str,
    vllm_bin_dir: &str,
    log_path: &str,
) -> Result<VllmSpawn, String> {
    let args = build_vllm_serve_cmd(model, config);
    let mut cmd = tokio::process::Command::new(vllm_bin);
    cmd.args(&args);
    cmd.env(
        "PATH",
        format!("{vllm_bin_dir}:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"),
    );
    // stdout/stderr 同文件（既有做法保持）；打不开文件降级 null（不阻塞拉起）
    let log_out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map(std::process::Stdio::from)
        .unwrap_or_else(|_| std::process::Stdio::null());
    let log_err = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map(std::process::Stdio::from)
        .unwrap_or_else(|_| std::process::Stdio::null());
    cmd.stdout(log_out);
    cmd.stderr(log_err);
    cmd.stdin(std::process::Stdio::null());
    let child = cmd
        .spawn()
        .map_err(|e| format!("vllm 命令未找到或启动失败: {e}"))?;
    let pid = child
        .id()
        .ok_or_else(|| "vllm spawn 后无 pid".to_string())?;
    Ok(VllmSpawn {
        pid,
        child,
        log_path: log_path.to_string(),
    })
}

/// 换口重试成功：实例行端口（行 port + config.port **两处同步**——行 port 是
/// 唯一真相源，config JSON 只是随写镜像）+ pid 更新、status 保持 starting、
/// 清 error，并落库。
fn apply_spawn_retry_to_row(
    instances: &Mutex<Vec<ModelInstance>>,
    db: &Mutex<Connection>,
    instance_id: &str,
    new_port: u16,
    new_pid: u32,
) {
    let snapshot = {
        let mut guard = instances.lock().expect("instances poisoned");
        let Some(i) = guard.iter_mut().find(|i| i.id == instance_id) else {
            return;
        };
        // 已存真实命令同步换口（build_vllm_serve_cmd 恒产出 `--port <n>`，精确替换）
        if let Some(cmd) = &i.launch_command {
            i.launch_command =
                Some(cmd.replace(&format!("--port {}", i.port), &format!("--port {new_port}")));
        }
        i.port = new_port;
        i.config.port = new_port;
        i.pid = Some(new_pid);
        i.status = "starting".into();
        i.error = None;
        i.clone()
    };
    if let Ok(conn) = db.lock() {
        let _ = persist_instance(&conn, &snapshot);
    }
}

/// 按原错误路径落 error（重试也失败 / 子进程二度端口占用退出）。
fn mark_instance_error(
    instances: &Mutex<Vec<ModelInstance>>,
    db: &Mutex<Connection>,
    instance_id: &str,
    err: &str,
) {
    let snapshot = {
        let mut guard = instances.lock().expect("instances poisoned");
        let Some(i) = guard.iter_mut().find(|i| i.id == instance_id) else {
            return;
        };
        i.status = "error".into();
        i.pid = None;
        i.error = Some(err.to_string());
        i.clone()
    };
    if let Ok(conn) = db.lock() {
        let _ = persist_instance(&conn, &snapshot);
    }
}

/// spawn 监测上下文（聚合参数，防 clippy too_many_arguments）。
struct SpawnMonitorCtx {
    instances: Arc<Mutex<Vec<ModelInstance>>>,
    db: Arc<Mutex<Connection>>,
    instance_id: String,
    config: VllmConfig,
}

/// spawn 后端口占用监测 + 换口重试（最多一次；见模块头 §端口选取）。
///
/// 轮询 `child.try_wait`（非阻塞）：`window` 内子进程退出且日志尾含端口占用
/// → [`pick_free_port_from`] 选下一个真实空闲口 → 日志追加分隔行 → 经
/// `spawn_fn` 重拉一次（成功：[`apply_spawn_retry_to_row`] 更新两处端口并
/// 落库，继续盯新子进程；失败：[`mark_instance_error`]）。二次端口占用退出
/// 或重拉失败均按原错误路径落 error；非端口占用原因的退出不重试（保持
/// starting，交给健康修正/用户裁决）。`window`/`poll` 参数化供测试注入短窗。
async fn monitor_addr_in_use(
    ctx: SpawnMonitorCtx,
    spawn_fn: VllmSpawnFn,
    mut spawned: VllmSpawn,
    window: Duration,
    poll: Duration,
) {
    let SpawnMonitorCtx {
        instances,
        db,
        instance_id,
        mut config,
    } = ctx;
    let deadline = std::time::Instant::now() + window;
    let mut retried = false;
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(poll).await;
        match spawned.child.try_wait() {
            // 还在跑（vLLM 加载模型可长达分钟级）——继续等
            Ok(None) => {}
            Ok(Some(_)) => {
                let tail = read_log_tail_bytes(&spawned.log_path, SPAWN_LOG_TAIL_BYTES);
                if !log_says_addr_in_use(&tail) {
                    return; // 非端口占用退出：不猜原因，保持现状态
                }
                if retried {
                    let err = format!(
                        "vllm 端口 {} 被占用，换口重试后仍失败（Address already in use）",
                        config.port
                    );
                    append_log_line(&spawned.log_path, &format!("=== {err} ==="));
                    mark_instance_error(&instances, &db, &instance_id, &err);
                    return;
                }
                let new_port = pick_free_port_from(INSTANCE_PORT_BASE, &used_ports(&instances));
                append_log_line(
                    &spawned.log_path,
                    &format!(
                        "=== 端口 {} 被占用（Address already in use），自动换口 {} 重试拉起 ===",
                        config.port, new_port
                    ),
                );
                config.port = new_port;
                match spawn_fn(&config) {
                    Ok(sp2) => {
                        eprintln!(
                            "[llm-spawn] 实例 {instance_id} 端口被占用，已换 {new_port} 重试（pid {}）",
                            sp2.pid
                        );
                        apply_spawn_retry_to_row(&instances, &db, &instance_id, new_port, sp2.pid);
                        spawned = sp2;
                        retried = true;
                    }
                    Err(e) => {
                        let err = format!("端口 {new_port} 换口重试拉起失败: {e}");
                        append_log_line(&spawned.log_path, &format!("=== {err} ==="));
                        mark_instance_error(&instances, &db, &instance_id, &err);
                        return;
                    }
                }
            }
            Err(e) => {
                eprintln!("[llm-spawn] 实例 {instance_id} 子进程 wait 失败: {e}");
                return;
            }
        }
    }
    // 窗口结束仍存活：正常路径（vLLM 慢慢加载），监测自然退出
}

impl Default for LlmRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RouteHandler for LlmRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec(HttpMethod::Get, "/api/v1/llm/gpu", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/llm/instances", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/llm/instances",
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Get, "/api/v1/llm/instances/:id", false, vec![]),
            spec(
                HttpMethod::Get,
                "/api/v1/llm/instances/:id/metrics",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/llm/instances/:id/log",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/llm/instances/:id/start",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/llm/instances/:id/stop",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/llm/instances/:id",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/llm/instances/:id/health",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/llm/instances/:id/chat",
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Get, "/api/v1/llm/stats", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/llm/gateway/models", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/llm/gateway/health", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/llm/analyze-image",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/llm/recipes/catalog",
                false,
                vec![],
            ),
            spec(HttpMethod::Get, "/api/v1/llm/recipes/recipe", false, vec![]),
        ]
        .into_iter()
        // 推理环境（vLLM venv 管理）7 条：GET 公开读，POST/DELETE 需 admin
        // （契约见 llm_envs::route_specs 与 docs/LLM_ENVIRONMENTS.md）
        .chain(llm_envs::route_specs())
        // 外部 API 接入 5 条：GET 公开读，写/test/chat 需 admin（契约见
        // llm_external::route_specs 与 docs/LLM_EXTERNAL_APIS.md；其中
        // POST /:id/chat 在 http.rs 特挂为 SSE 流式路由，spec 循环跳过）
        .chain(llm_external::route_specs())
        .collect()
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        // —— /api/v1/llm/environments* —— 推理环境管理（整体委托 llm_envs 子模块：
        // 注册表 + 202 异步任务；路由 specs 同源 llm_envs::route_specs）——
        if matches!(segs.as_slice(), ["api", "v1", "llm", "environments", ..]) {
            return llm_envs::handle(&self.env_state, req.method, &segs[4..], req.body);
        }
        // —— /api/v1/llm/external-apis* —— 外部 API 接入（整体委托
        // llm_external 子模块：登记/连通测试/对话直通；流式分支由 http.rs
        // 特挂路由处理，不经此路径）
        if matches!(segs.as_slice(), ["api", "v1", "llm", "external-apis", ..]) {
            return llm_external::handle(&self.external, req.method, &segs[4..], req.body).await;
        }
        match (req.method, segs.as_slice()) {
            // —— GET /api/v1/llm/gpu —— GPU 信息（动态探测）
            (HttpMethod::Get, ["api", "v1", "llm", "gpu"]) => {
                let info = detect_gpu().await;
                Ok(ok_json(to_value(&info)?))
            }

            // —— GET /api/v1/llm/instances —— 列全部（返回前做 status 健康修正：
            // running 验活回落 / stopped 端口活且模型名匹配则修正 running，落库）
            (HttpMethod::Get, ["api", "v1", "llm", "instances"]) => {
                let list = self.reconcile_instance_statuses().await;
                // launch_command 逐行注入（真实命令/构造命令，见 instance_json）
                let arr: Vec<serde_json::Value> =
                    list.iter().map(instance_json).collect::<Result<_, _>>()?;
                Ok(ok_json(serde_json::Value::Array(arr)))
            }

            // —— POST /api/v1/llm/instances —— 创建+启动实例
            (HttpMethod::Post, ["api", "v1", "llm", "instances"]) => {
                let body: CreateInstanceBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析创建实例请求体失败: {e}"))
                })?;
                if body.name.trim().is_empty() {
                    return Ok(error_response(400, "name 不可为空"));
                }
                if body.model.trim().is_empty() {
                    return Ok(error_response(400, "model 不可为空"));
                }
                let source_type = body
                    .source_type
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "huggingface".to_string());
                if source_type != "huggingface" && source_type != "local" {
                    return Ok(error_response(
                        400,
                        "source_type 必须是 huggingface 或 local",
                    ));
                }
                // 可选指定推理环境：必须存在于注册表（fail-fast；是否 ready 留给
                // 启动时判——环境可能还在创建中，先建实例定义再等环境 ready）
                let env_name = body
                    .env_name
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                if let Some(n) = &env_name {
                    let exists = self
                        .db
                        .lock()
                        .map(|conn| llm_envs::env_row_by_name(&conn, n).is_some())
                        .unwrap_or(false);
                    if !exists {
                        return Ok(error_response(
                            400,
                            &format!("推理环境不存在: {n}（先在「推理环境」Tab 创建）"),
                        ));
                    }
                }
                // 端口：手动指定（校验：范围/表内冲突/保留段/真实试绑）或自动选
                // （实例表去重 + 真实试绑，8123 起）。行 port 与 config.port 同源写入。
                let port = match body.port {
                    Some(p) => {
                        if let Err((status, msg)) = self.validate_manual_port(p) {
                            return Ok(error_response(status, &msg));
                        }
                        u16::try_from(p).unwrap_or(INSTANCE_PORT_MIN)
                    }
                    None => self.pick_free_port(),
                };
                let mut config = body.config.unwrap_or_default();
                config.port = port;
                if config.host.trim().is_empty() {
                    config.host = "0.0.0.0".into();
                }
                let mut instance = ModelInstance {
                    id: self.next_id(),
                    name: body.name,
                    model: body.model.clone(),
                    source_type,
                    port,
                    status: "stopped".into(),
                    pid: None,
                    env_name: env_name.clone(),
                    launch_command: None,
                    config,
                    health: None,
                    created_at: now_iso(),
                    error: None,
                };
                // 默认不 autostart（避免测试真起 vllm）；autostart=true 时真 spawn
                if body.autostart.unwrap_or(false) {
                    match self.spawn_vllm(&instance).await {
                        Ok((pid, real_command)) => {
                            instance.status = "starting".into();
                            instance.pid = Some(pid);
                            instance.launch_command = Some(real_command);
                        }
                        Err(e) => {
                            instance.status = "error".into();
                            instance.error = Some(e);
                        }
                    }
                }
                let resp_body = instance_json(&instance)?;
                self.persist(&instance); // 定义持久化（重启恢复用）
                self.instances
                    .lock()
                    .expect("instances poisoned")
                    .push(instance);
                Ok(ApiResponse {
                    status: 201,
                    body: resp_body,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/llm/instances/:id —— 单实例详情
            (HttpMethod::Get, ["api", "v1", "llm", "instances", id]) => {
                let instances = self.instances.lock().expect("instances poisoned");
                match instances.iter().find(|i| i.id == *id) {
                    Some(i) => Ok(ok_json(instance_json(i)?)),
                    None => Ok(error_response(404, &format!("实例不存在: {id}"))),
                }
            }

            // —— POST /api/v1/llm/instances/:id/start —— 启动实例（spawn vllm）
            (HttpMethod::Post, ["api", "v1", "llm", "instances", id, "start"]) => {
                // 先快照实例配置（锁立即释放）
                let snap = {
                    let instances = self.instances.lock().expect("instances poisoned");
                    instances.iter().find(|i| i.id == *id).cloned()
                };
                let Some(mut inst) = snap else {
                    return Ok(error_response(404, &format!("实例不存在: {id}")));
                };
                // 端口唯一真相源：行 port 若与 config.port 不一致（历史双写残留），
                // 拉起前先收敛到行 port（spawn 参数与探测同源，防「绑 A 探 B」卡死）
                if inst.config.port != inst.port {
                    inst.config.port = inst.port;
                }
                match self.spawn_vllm(&inst).await {
                    Ok((pid, real_command)) => {
                        inst.status = "starting".into();
                        inst.pid = Some(pid);
                        inst.launch_command = Some(real_command);
                        inst.error = None;
                    }
                    Err(e) => {
                        inst.status = "error".into();
                        inst.error = Some(e);
                    }
                }
                self.persist(&inst); // 启停同步落表
                let updated = inst.clone();
                let mut instances = self.instances.lock().expect("instances poisoned");
                if let Some(i) = instances.iter_mut().find(|i| i.id == *id) {
                    *i = inst;
                }
                Ok(ok_json(instance_json(&updated)?))
            }

            // —— POST /api/v1/llm/instances/:id/stop —— 停止实例（kill pid）
            (HttpMethod::Post, ["api", "v1", "llm", "instances", id, "stop"]) => {
                let mut instances = self.instances.lock().expect("instances poisoned");
                match instances.iter_mut().find(|i| i.id == *id) {
                    Some(i) => {
                        if let Some(pid) = i.pid {
                            Self::kill_instance(pid);
                        }
                        i.status = "stopped".into();
                        i.pid = None;
                        let snapshot = i.clone();
                        drop(instances); // 持久化不占 instances 锁
                        self.persist(&snapshot); // 启停同步落表
                        Ok(ok_json(instance_json(&snapshot)?))
                    }
                    None => Ok(error_response(404, &format!("实例不存在: {id}"))),
                }
            }

            // —— DELETE /api/v1/llm/instances/:id —— 删实例
            (HttpMethod::Delete, ["api", "v1", "llm", "instances", id]) => {
                let mut instances = self.instances.lock().expect("instances poisoned");
                // 先 kill running 实例的 pid
                if let Some(i) = instances.iter().find(|i| i.id == *id) {
                    if let Some(pid) = i.pid {
                        Self::kill_instance(pid);
                    }
                }
                let before = instances.len();
                instances.retain(|i| i.id != *id);
                if instances.len() == before {
                    return Ok(error_response(404, &format!("实例不存在: {id}")));
                }
                drop(instances);
                self.persist_remove(id); // 删除同步落表
                Ok(ok_json(
                    serde_json::json!({"ok": true, "id": id, "action": "delete"}),
                ))
            }

            // —— POST /api/v1/llm/instances/:id/health —— 健康探测
            (HttpMethod::Post, ["api", "v1", "llm", "instances", id, "health"]) => {
                let port = {
                    let instances = self.instances.lock().expect("instances poisoned");
                    match instances.iter().find(|i| i.id == *id) {
                        Some(i) => i.port,
                        None => return Ok(error_response(404, &format!("实例不存在: {id}"))),
                    }
                };
                let health = Self::probe_health(port).await;
                let updated_health = health.clone();
                let mut instances = self.instances.lock().expect("instances poisoned");
                let mut snapshot: Option<ModelInstance> = None;
                if let Some(i) = instances.iter_mut().find(|i| i.id == *id) {
                    i.health = Some(health);
                    // 探测活则同步 status → running
                    if updated_health.alive && i.status != "stopped" {
                        i.status = "running".into();
                    }
                    snapshot = Some(i.clone());
                }
                drop(instances);
                if let Some(inst) = snapshot {
                    self.persist(&inst); // status 翻转（starting→running）同步落表
                }
                Ok(ok_json(to_value(&updated_health)?))
            }

            // —— POST /api/v1/llm/instances/:id/chat —— 推理测试
            (HttpMethod::Post, ["api", "v1", "llm", "instances", id, "chat"]) => {
                let body: ChatBody = serde_json::from_value(req.body)
                    .map_err(|e| ApiGatewayError::Internal(format!("解析推理请求体失败: {e}")))?;
                if body.messages.is_empty() {
                    return Ok(error_response(400, "messages 不可为空"));
                }
                // 校验 role 非空
                if body.messages.iter().any(|m| m.role.trim().is_empty()) {
                    return Ok(error_response(400, "每条 message 的 role 不可为空"));
                }
                let snap = {
                    let instances = self.instances.lock().expect("instances poisoned");
                    instances.iter().find(|i| i.id == *id).cloned()
                };
                let Some(inst) = snap else {
                    return Ok(error_response(404, &format!("实例不存在: {id}")));
                };
                if inst.status != "running" && inst.status != "starting" {
                    return Ok(error_response(400, "实例未运行（请先启动）"));
                }
                let model_name = inst
                    .config
                    .served_model_name
                    .clone()
                    .unwrap_or_else(|| inst.model.clone());
                match Self::chat_complete(inst.port, &model_name, &body).await {
                    Ok(outcome) => Ok(ok_json(serde_json::json!({
                        "id": id,
                        "content": outcome.content,
                        "reasoning": outcome.reasoning,
                        "finish_reason": outcome.finish_reason,
                        "total_tokens": outcome.total_tokens,
                        "model": inst.model,
                    }))),
                    Err(e) => Ok(error_response(502, &e)),
                }
            }

            // —— GET /api/v1/llm/instances/:id/metrics —— 轻量监控（公开读）
            //
            // 按需抓取 vLLM /metrics（5s 缓存去抖，零后台开销）。实例不存在
            // 404；不可达 200 + reachable:false（监控探测不是错误）。
            (HttpMethod::Get, ["api", "v1", "llm", "instances", id, "metrics"]) => {
                let port = {
                    let instances = self.instances.lock().expect("instances poisoned");
                    match instances.iter().find(|i| i.id == *id) {
                        Some(i) => i.port, // 行 port 唯一真相源（不再读 config.port）
                        None => return Ok(error_response(404, &format!("实例不存在: {id}"))),
                    }
                };
                let body = self
                    .collect_metrics(id, port, std::time::Instant::now())
                    .await;
                Ok(ok_json(body))
            }

            // —— GET /api/v1/llm/instances/:id/log?tail=200&follow=0 —— 实例拉起
            //    日志尾（公开读，对齐 metrics 权限风格）
            //
            // 读 `<NEXOS_LLM_SPAWN_DIR>/llm-vllm-<id>.log` 尾 N 行（默认 200、
            // 上限 1000，单次读取 ≤256KB）。follow 参数当前为拉取式实现（响应
            // 同构），持续跟随由前端 2s 轮询完成。实例不存在 404；日志文件尚未
            // 生成（从未拉起）404。
            (HttpMethod::Get, ["api", "v1", "llm", "instances", id, "log"]) => {
                let status = {
                    let instances = self.instances.lock().expect("instances poisoned");
                    match instances.iter().find(|i| i.id == *id) {
                        Some(i) => i.status.clone(),
                        None => return Ok(error_response(404, &format!("实例不存在: {id}"))),
                    }
                };
                let tail = query_param(&req.path, "tail")
                    .and_then(|s| s.parse::<usize>().ok())
                    .filter(|n| *n > 0)
                    .unwrap_or(INSTANCE_LOG_TAIL_DEFAULT)
                    .min(INSTANCE_LOG_TAIL_MAX);
                let file = self.instance_log_path(id);
                match read_log_tail_lines(&file, tail, INSTANCE_LOG_TAIL_BYTES) {
                    Ok(lines) => Ok(ok_json(to_value(&InstanceLogResponse {
                        instance_id: id.to_string(),
                        lines,
                        file,
                        status,
                    })?)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(error_response(
                        404,
                        &format!("日志文件不存在（实例尚未拉起过）: {file}"),
                    )),
                    Err(e) => Ok(error_response(500, &format!("读取日志失败: {e}"))),
                }
            }

            // —— GET /api/v1/llm/stats —— 聚合统计
            (HttpMethod::Get, ["api", "v1", "llm", "stats"]) => {
                let (total, running, stopped) = self.stats_snapshot();
                let gpu = detect_gpu().await;
                Ok(ok_json(to_value(&LlmStats {
                    instances_total: total,
                    running,
                    stopped,
                    gpu_available: gpu.available,
                    gpu_devices: gpu.devices.len(),
                })?))
            }

            // —— GET /api/v1/llm/gateway/models —— 网关聚合视图（公开读）
            //
            // 对每个 running 实例真实探测 /v1/models（2s 超时，并发）：成功进
            // gateway_visible（原始模型对象 + id 列表），失败进 unreachable
            // （带原因）。计费/路由层据此知道「本机现在真实有哪几个模型」。
            (HttpMethod::Get, ["api", "v1", "llm", "gateway", "models"]) => {
                let body = self.gateway_models_body().await;
                Ok(ok_json(to_value(&body)?))
            }

            // —— GET /api/v1/llm/gateway/health —— 网关可达性汇总（公开读）
            //
            // running 数 / 可达数 / 不可达数 + 总 GPU 显存（复用 detect_gpu）。
            (HttpMethod::Get, ["api", "v1", "llm", "gateway", "health"]) => {
                let body = self.gateway_health_body().await;
                Ok(ok_json(to_value(&body)?))
            }

            // —— POST /api/v1/llm/analyze-image —— 截图分析（给 AI 调用分析图片）
            //
            // 接收 base64 图片 + prompt，转发到本机 vLLM 视觉推理，返回描述文本。
            // vLLM 不在线时降级 503，不 panic。
            (HttpMethod::Post, ["api", "v1", "llm", "analyze-image"]) => {
                let body: AnalyzeImageBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析截图分析请求体失败: {e}"))
                })?;
                match Self::analyze_image(&body).await {
                    Ok(resp) => Ok(ok_json(to_value(&resp)?)),
                    Err(e) => {
                        // vLLM 不在线的降级语义：503；其余错误 502
                        let status = if e.contains("vLLM 服务未运行") {
                            503
                        } else {
                            502
                        };
                        Ok(error_response(status, &e))
                    }
                }
            }

            // —— GET /api/v1/llm/recipes/catalog[?refresh=1] —— vLLM Recipes 目录
            //    （烘焙代理，公开读）
            //
            // 服务端拉上游 models.json（15s 超时）→ 精简目录；**常驻进程缓存**
            // （无 TTL，打开 Tab 只读缓存秒回），`?refresh=1` 手动强制重拉并
            // 更新缓存（详情缓存随之清空）。响应信封 `{items, cached_at,
            // from_cache}`。上游失败 502 带原因（外网不通 ≠ 崩溃，旧缓存保留，
            // 前端照常渲染错误横幅）。
            (HttpMethod::Get, ["api", "v1", "llm", "recipes", "catalog"]) => {
                let refresh = is_refresh_param(&req.path);
                match self.recipes_catalog_body(refresh).await {
                    Ok(body) => Ok(ok_json(body)),
                    Err((status, e)) => Ok(error_response(status, &e)),
                }
            }

            // —— GET /api/v1/llm/recipes/recipe?hf_id= —— 单配方 JSON 透传（公开读）
            //
            // hf_id 缺失/非法 400；上游失败 502；常驻进程缓存（随目录 refresh 清空）。
            (HttpMethod::Get, ["api", "v1", "llm", "recipes", "recipe"]) => {
                let hf_id = query_param(&req.path, "hf_id").unwrap_or_default();
                match self.recipes_recipe_body(&hf_id).await {
                    Ok(body) => Ok(ok_json(body)),
                    Err((status, e)) => Ok(error_response(status, &e)),
                }
            }

            // —— 未覆盖路由 —— 兜底 404
            _ => Ok(error_response(404, "llm: 未匹配的路由")),
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
        handler_component: "llm".to_string(),
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

/// 实例的有效启动命令（「接入说明」面板「启动参数」块的数据源）：
/// 曾拉起 → 最近一次真实 argv（含真实推理环境二进制路径）；从未拉起 →
/// 按当前 config 用 [`build_vllm_serve_cmd`] 构造（`vllm` 占位二进制名，
/// 与 spawn 时同函数同参，不漂移）。
fn effective_launch_command(inst: &ModelInstance) -> String {
    inst.launch_command.clone().unwrap_or_else(|| {
        format!(
            "vllm {}",
            build_vllm_serve_cmd(&inst.model, &inst.config).join(" ")
        )
    })
}

/// 单实例响应 JSON（在序列化行上注入 `launch_command` 恒有值字段）。
fn instance_json(inst: &ModelInstance) -> Result<serde_json::Value, ApiGatewayError> {
    let mut v = to_value(inst)?;
    if let serde_json::Value::Object(ref mut map) = v {
        map.insert(
            "launch_command".into(),
            serde_json::Value::String(effective_launch_command(inst)),
        );
    }
    Ok(v)
}

fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

/// 从请求路径的 query string 中提取指定参数（files.rs 同款：`+` 与 `%XX` 解码；
/// 值解码后为空视为缺失）。
fn query_param(path: &str, key: &str) -> Option<String> {
    let q = path.split('?').nth(1)?;
    for kv in q.split('&') {
        let mut it = kv.splitn(2, '=');
        if it.next()? == key {
            let decoded = url_decode(it.next().unwrap_or(""));
            if decoded.is_empty() {
                return None;
            }
            return Some(decoded);
        }
    }
    None
}

/// `?refresh=` 参数真值判定（`1`/`true`（忽略大小写）为真，其余/缺省 false）。
/// recipes 目录手动刷新入口（常驻缓存的唯一失效通道）。
fn is_refresh_param(path: &str) -> bool {
    query_param(path, "refresh")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// 极简 URL 解码（仅处理 `+` → 空格 与 `%XX`；非法/截断序列原样保留）。
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
            c => {
                out.push(c);
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
// demo 数据
// ----------------------------------------------------------------------------

/// demo 实例（1 个 running 示例 + 1 个 stopped 本地模型示例）。
fn demo_instances() -> Vec<ModelInstance> {
    // 用户 2026-08-27 指示：全用真实数据——生产路径不再 seed 演示实例
    // （历史占位数据会让「实例不可达/模型管理空转」的排查变复杂）。
    // 仅 #[cfg(test)] 保留填充（26 处测试依赖 llm-1/llm-2 的确定性环境）。
    #[cfg(test)]
    {
        vec![
            ModelInstance {
                id: "llm-1".into(),
                name: "Qwen2.5-7B 对话".into(),
                model: "Qwen/Qwen2.5-7B-Instruct".into(),
                source_type: "huggingface".into(),
                port: 8000,
                status: "running".into(),
                pid: None,
                env_name: None,
                launch_command: None,
                config: VllmConfig {
                    host: "0.0.0.0".into(),
                    port: 8000,
                    tensor_parallel_size: 1,
                    gpu_memory_utilization: 0.9,
                    max_model_len: 8192,
                    quantization: None,
                    dtype: "auto".into(),
                    served_model_name: Some("qwen2.5-7b".into()),
                    trust_remote_code: false,
                    extra_args: vec![],
                },
                health: None,
                created_at: "2026-08-08T09:00:00+08:00".into(),
                error: None,
            },
            ModelInstance {
                id: "llm-2".into(),
                name: "DeepSeek 对话（演示占位）".into(),
                model: "deepseek-ai/DeepSeek-V4-Flash".into(),
                source_type: "huggingface".into(),
                port: 8001,
                status: "stopped".into(),
                pid: None,
                env_name: None,
                launch_command: None,
                config: VllmConfig {
                    host: "0.0.0.0".into(),
                    port: 8001,
                    tensor_parallel_size: 1,
                    gpu_memory_utilization: 0.9,
                    max_model_len: 8192,
                    quantization: None,
                    dtype: "auto".into(),
                    served_model_name: Some("deepseek-v4".into()),
                    trust_remote_code: false,
                    extra_args: vec![],
                },
                health: None,
                created_at: "2026-08-08T09:00:00+08:00".into(),
                error: None,
            },
        ]
    }
    #[cfg(not(test))]
    {
        Vec::new()
    }
}

// ----------------------------------------------------------------------------
// SQLite 持久化层（llm.db · llm_instances 表，forwarding.db 同款惯例）
// ----------------------------------------------------------------------------

/// 默认 DB 路径：env `NEXOS_LLM_DB` 覆盖 → `/tank/os-data/llm.db` →
/// `/var/lib/os/llm.db` → `./llm.db`（保底）。
fn default_db_path() -> String {
    if let Some(p) = env_non_empty_llm("NEXOS_LLM_DB") {
        return p;
    }
    for p in &["/tank/os-data/llm.db", "/var/lib/os/llm.db"] {
        if std::path::Path::new(p)
            .parent()
            .is_some_and(|d| d.exists() || std::fs::create_dir_all(d).is_ok())
        {
            return (*p).to_string();
        }
    }
    "./llm.db".to_string()
}

/// 打开 SQLite 文件，WAL + 建表（不 seed——首次 seed 在 from_db_path 里做）。
fn open_db(path: &str) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    create_schema(&conn)?;
    Ok(conn)
}

/// 建表（IF NOT EXISTS）+ 存量库迁移（ALTER 补列，已存在则忽略）。config 列存
/// [`VllmConfig`] 的 JSON 序列化；推理环境表 `llm_environments` 由 llm_envs
/// 子模块建（同连接幂等）。
fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS llm_instances (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            model TEXT NOT NULL,
            source_type TEXT NOT NULL DEFAULT 'huggingface',
            port INTEGER NOT NULL DEFAULT 8123,
            config TEXT NOT NULL DEFAULT '{}',
            status TEXT NOT NULL DEFAULT 'stopped',
            pid INTEGER,
            error TEXT,
            created_at TEXT
        );",
    )?;
    // 迁移：2026-08-31 之前的 llm_instances 表缺 env_name 列（CREATE IF NOT
    // EXISTS 不会给已存在的表补列）。列已存在时 ALTER 报 duplicate column，
    // 忽略即可（幂等，forwarding.rs 同款惯例）。
    let _ = conn.execute("ALTER TABLE llm_instances ADD COLUMN env_name TEXT", []);
    // 迁移：2026-08-31 起新增 launch_command（最近一次真实拉起命令；接入说明
    // 面板「启动参数」块）。列已存在时 ALTER 报 duplicate column，忽略即可。
    let _ = conn.execute("ALTER TABLE llm_instances ADD COLUMN launch_command TEXT", []);
    // 推理环境注册表（子模块管理；同库同连接）
    llm_envs::create_env_schema(conn)?;
    // 外部 API 接入表（子模块管理；同库同连接）
    llm_external::create_schema(conn)?;
    Ok(())
}

/// 实例落表（INSERT OR REPLACE——创建/启停/健康状态变化全量覆盖）。
fn persist_instance(conn: &Connection, i: &ModelInstance) -> rusqlite::Result<()> {
    let config_json = serde_json::to_string(&i.config).unwrap_or_else(|_| "{}".into());
    conn.execute(
        "INSERT OR REPLACE INTO llm_instances
         (id,name,model,source_type,port,config,status,pid,error,created_at,env_name,launch_command)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
        params![
            i.id,
            i.name,
            i.model,
            i.source_type,
            i64::from(i.port),
            config_json,
            i.status,
            i.pid.map(i64::from),
            i.error.as_deref(),
            i.created_at,
            i.env_name.as_deref(),
            i.launch_command.as_deref(),
        ],
    )?;
    Ok(())
}

/// 删表行（DELETE 实例时同步）。
fn delete_instance_row(conn: &Connection, id: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM llm_instances WHERE id=?", params![id])?;
    Ok(())
}

/// 从表恢复全部实例定义：**status 一律 'stopped'、pid/error 清空、health 置
/// None**——服务重启后旧进程已不可信（pid 可能被复用），用户明确要求手动拉起，
/// 不做自动恢复运行态。config/source_type/port/name/model/created_at 原样还原。
fn load_persisted_instances(conn: &Connection) -> rusqlite::Result<Vec<ModelInstance>> {
    let mut stmt = conn.prepare(
        "SELECT id,name,model,source_type,port,config,created_at,env_name,launch_command
         FROM llm_instances
         ORDER BY created_at, id",
    )?;
    let iter = stmt.query_map([], |row| {
        let config_json: String = row.get(5)?;
        let port = u16::try_from(row.get::<_, i64>(4)?).unwrap_or(8123);
        let mut config: VllmConfig = serde_json::from_str(&config_json).unwrap_or_default();
        // 端口唯一真相源（2026-08-31）：行 port 为准，config JSON 的 port 只是
        // 随写镜像——历史双写残留（行改了、config 没改）在恢复时即收敛
        config.port = port;
        Ok(ModelInstance {
            id: row.get(0)?,
            name: row.get(1)?,
            model: row.get(2)?,
            source_type: row.get(3)?,
            port,
            status: "stopped".into(),
            pid: None,
            env_name: row.get(7)?,
            launch_command: row.get(8)?,
            config,
            health: None,
            created_at: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
            error: None,
        })
    })?;
    let mut out = Vec::new();
    for i in iter {
        out.push(i?);
    }
    Ok(out)
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

    // ---- chat_template_kwargs 透传（2026-09-04 分镜质量修复）----

    fn chat_body_fixture(kwargs: Option<serde_json::Value>) -> ChatBody {
        ChatBody {
            messages: vec![ChatMessage {
                role: "user".into(),
                content: "生成 5 个镜头".into(),
            }],
            max_tokens: Some(64),
            temperature: Some(0.3),
            chat_template_kwargs: kwargs,
        }
    }

    #[test]
    fn chat_body_serialization_omits_none_kwargs() {
        let v = serde_json::to_value(chat_body_fixture(None)).unwrap();
        assert!(
            v.get("chat_template_kwargs").is_none(),
            "None 时序列化不得出现该键: {v}"
        );
        // 其余字段原样（temperature f32→f64 有浮点尾差，容差比较）
        assert!(v["messages"].is_array());
        assert_eq!(v["max_tokens"], 64);
        assert!(
            (v["temperature"].as_f64().unwrap() - 0.3).abs() < 1e-6,
            "temperature 应为 0.3: {}",
            v["temperature"]
        );
    }

    #[test]
    fn chat_body_serialization_carries_kwargs_value() {
        let v = serde_json::to_value(chat_body_fixture(Some(serde_json::json!({
            "enable_thinking": false
        }))))
        .unwrap();
        assert_eq!(
            v["chat_template_kwargs"],
            serde_json::json!({"enable_thinking": false}),
            "Some 时键与值应原样透传: {v}"
        );
    }

    #[test]
    fn chat_body_deserialization_accepts_missing_and_present_kwargs() {
        // 旧请求体（无该字段）仍可解析（serde default）——REST 面向后兼容
        let old: ChatBody = serde_json::from_value(serde_json::json!({
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .unwrap();
        assert!(old.chat_template_kwargs.is_none());
        assert!(old.max_tokens.is_none());
        // 新请求体可解析出 kwargs
        let new: ChatBody = serde_json::from_value(serde_json::json!({
            "messages": [{"role": "user", "content": "hi"}],
            "chat_template_kwargs": {"enable_thinking": false},
        }))
        .unwrap();
        assert_eq!(
            new.chat_template_kwargs,
            Some(serde_json::json!({"enable_thinking": false}))
        );
    }

    // ---- 命令构造器测试 ----

    #[test]
    fn build_vllm_serve_cmd_contains_model_and_port_and_tp() {
        let cfg = VllmConfig::default();
        let cmd = build_vllm_serve_cmd("Qwen/Qwen2.5-7B-Instruct", &cfg);
        let joined = cmd.join(" ");
        assert!(
            joined.contains("serve Qwen/Qwen2.5-7B-Instruct"),
            "缺 model: {joined}"
        );
        assert!(joined.contains("--port"), "缺 --port: {joined}");
        assert!(joined.contains("--tensor-parallel-size"), "缺 tp: {joined}");
        assert!(
            joined.contains("--gpu-memory-utilization"),
            "缺 gmu: {joined}"
        );
        assert!(joined.contains("--max-model-len"), "缺 mml: {joined}");
        assert!(joined.contains("--dtype auto"), "缺 dtype: {joined}");
    }

    #[test]
    fn build_vllm_serve_cmd_includes_quantization_when_set() {
        let cfg = VllmConfig {
            quantization: Some("awq".into()),
            ..Default::default()
        };
        let cmd = build_vllm_serve_cmd("mymodel", &cfg);
        let joined = cmd.join(" ");
        assert!(
            joined.contains("--quantization awq"),
            "缺 --quantization: {joined}"
        );
    }

    #[test]
    fn build_vllm_serve_cmd_no_quantization_when_none() {
        let cfg = VllmConfig::default();
        let cmd = build_vllm_serve_cmd("mymodel", &cfg);
        assert!(
            !cmd.iter().any(|a| a == "--quantization"),
            "quantization=None 不应有该参数"
        );
    }

    #[test]
    fn build_vllm_serve_cmd_trust_remote_code_when_true() {
        let cfg = VllmConfig {
            trust_remote_code: true,
            ..Default::default()
        };
        let cmd = build_vllm_serve_cmd("mymodel", &cfg);
        assert!(
            cmd.iter().any(|a| a == "--trust-remote-code"),
            "trust_remote_code=true 应含该参数"
        );
    }

    #[test]
    fn build_vllm_serve_cmd_passes_extra_args() {
        let cfg = VllmConfig {
            extra_args: vec!["--enable-prefix-caching".into(), "--enforce-eager".into()],
            ..Default::default()
        };
        let cmd = build_vllm_serve_cmd("mymodel", &cfg);
        assert!(
            cmd.iter().any(|a| a == "--enable-prefix-caching"),
            "extra_args 应透传"
        );
        assert!(
            cmd.iter().any(|a| a == "--enforce-eager"),
            "extra_args 应透传"
        );
    }

    #[test]
    fn build_vllm_serve_cmd_served_model_name_when_set() {
        let cfg = VllmConfig {
            served_model_name: Some("my-alias".into()),
            ..Default::default()
        };
        let cmd = build_vllm_serve_cmd("mymodel", &cfg);
        let joined = cmd.join(" ");
        assert!(
            joined.contains("--served-model-name my-alias"),
            "缺 served-model-name: {joined}"
        );
    }

    #[test]
    fn build_nvidia_smi_cmd_contains_query_gpu_and_format() {
        let cmd = build_nvidia_smi_cmd();
        let joined = cmd.join(" ");
        assert!(
            joined.contains("--query-gpu=index,name,memory.total"),
            "缺 query-gpu: {joined}"
        );
        assert!(
            joined.contains("--format=csv,noheader,nounits"),
            "缺 format csv noheader nounits: {joined}"
        );
    }

    #[test]
    fn vllm_config_defaults_are_sane() {
        let cfg = VllmConfig::default();
        assert_eq!(cfg.host, "0.0.0.0");
        assert_eq!(cfg.port, 8000);
        assert_eq!(cfg.tensor_parallel_size, 1);
        assert!((cfg.gpu_memory_utilization - 0.9).abs() < 1e-6);
        assert_eq!(cfg.max_model_len, 8192);
        assert!(cfg.quantization.is_none());
        assert_eq!(cfg.dtype, "auto");
        assert!(!cfg.trust_remote_code);
        assert!(cfg.extra_args.is_empty());
    }

    #[test]
    fn parse_nvidia_smi_line_parses_six_fields() {
        let d = parse_nvidia_smi_line("0, NVIDIA GeForce RTX 3090, 24576, 1024, 23552, 5")
            .expect("应解析成功");
        assert_eq!(d.index, 0);
        assert_eq!(d.name, "NVIDIA GeForce RTX 3090");
        assert_eq!(d.memory_total_mib, Some(24576));
        assert_eq!(d.memory_used_mib, Some(1024));
        assert_eq!(d.memory_free_mib, Some(23552));
        assert_eq!(d.utilization_pct, Some(5));
        assert!(!d.unified_memory, "数值显存=独立显存卡");
    }

    /// DGX Spark GB10 实测形态（2026-09-03 隧道采集）：
    /// `nvidia-smi --query-gpu=index,name,memory.total,memory.used,memory.free,utilization.gpu
    ///  --format=csv,noheader,nounits` → `0, NVIDIA GB10, [N/A], [N/A], [N/A], 0`
    /// 旧解析器 [N/A].parse::<u64>() 失败丢行 → 误判"未检测到 GPU"——修复后
    /// name 可解析即成卡，显存 N/A → None + unified_memory=true。
    #[test]
    fn parse_nvidia_smi_line_gb10_unified_memory() {
        let d = parse_nvidia_smi_line("0, NVIDIA GB10, [N/A], [N/A], [N/A], 0")
            .expect("GB10 有输出即算有 GPU，不再因 [N/A] 丢行");
        assert_eq!(d.index, 0);
        assert_eq!(d.name, "NVIDIA GB10");
        assert_eq!(d.memory_total_mib, None);
        assert_eq!(d.memory_used_mib, None);
        assert_eq!(d.memory_free_mib, None);
        assert_eq!(d.utilization_pct, Some(0));
        assert!(d.unified_memory, "显存 N/A = 统一内存架构标记");
        // 统一内存池数值由 apply_unified_meminfo 填（此处纯解析层，恒 None）
        assert_eq!(d.unified_memory_total_mib, None);
    }

    /// 统一内存回退：unified 设备填 meminfo 池数值，独立显存卡不动。
    #[test]
    fn apply_unified_meminfo_fills_unified_devices_only() {
        let mut devices = vec![
            parse_nvidia_smi_line("0, NVIDIA GB10, [N/A], [N/A], [N/A], 0").unwrap(),
            parse_nvidia_smi_line("1, NVIDIA GeForce RTX 3090, 24576, 1024, 23552, 5").unwrap(),
        ];
        apply_unified_meminfo(&mut devices);
        let gb10 = &devices[0];
        // 真实机器上读 /proc/meminfo（Linux 测试环境恒在）：总量/可用/已用自洽
        let (t, f) = (gb10.unified_memory_total_mib, gb10.unified_memory_free_mib);
        assert!(t.unwrap_or(0) > 0, "MemTotal 池数值应填入: {gb10:?}");
        assert!(f.is_some());
        assert_eq!(gb10.unified_memory_used_mib, t.zip(f).map(|(t, f)| t - f));
        assert_eq!(gb10.memory_total_mib, None, "独立显存字段保持 N/A 语义");
        let rtx = &devices[1];
        assert_eq!(rtx.memory_total_mib, Some(24576), "独立显存卡零回归");
        assert_eq!(rtx.unified_memory_total_mib, None, "非 unified 不填池数值");
        assert!(!rtx.unified_memory);
    }

    /// 全独立显存卡时 apply_unified_meminfo 不读 meminfo、不碰字段。
    #[test]
    fn apply_unified_meminfo_noop_without_unified() {
        let mut devices =
            vec![parse_nvidia_smi_line("0, NVIDIA GeForce RTX 3090, 24576, 1024, 23552, 5")
                .unwrap()];
        let before = devices.clone();
        apply_unified_meminfo(&mut devices);
        assert_eq!(devices, before);
    }

    #[test]
    fn parse_nvidia_smi_line_rejects_short() {
        assert!(parse_nvidia_smi_line("short").is_none());
        assert!(parse_nvidia_smi_line("").is_none());
        assert!(parse_nvidia_smi_line("0, name, 100").is_none());
        // 空卡名 / 卡名 N/A → 丢行（无有效设备标识）
        assert!(parse_nvidia_smi_line("0, , 100, 1, 99, 0").is_none());
        assert!(parse_nvidia_smi_line("0, [N/A], 100, 1, 99, 0").is_none());
        // 非数字 index → 丢行
        assert!(parse_nvidia_smi_line("x, NVIDIA GB10, [N/A], [N/A], [N/A], 0").is_none());
    }

    // ---- 路由声明测试 ----

    #[tokio::test]
    async fn routes_declares_all_llm_endpoints() {
        let h = LlmRouteHandler::with_demo();
        let routes = h.routes().await;
        assert_eq!(routes.len(), 30, "应有 30 条路由（17 实例 + 7 环境 + 6 外部 API）: {routes:?}");
        assert!(
            routes.iter().all(|r| r.handler_component == "llm"),
            "全部归属 llm 组件"
        );
        // 写操作都要求 admin
        for r in &routes {
            if r.method == HttpMethod::Post
                || r.method == HttpMethod::Delete
                || r.method == HttpMethod::Put
            {
                assert!(r.requires_auth, "写操作需 auth: {r:?}");
                assert_eq!(r.required_roles, vec!["admin".to_string()]);
            }
        }
        // GET 全部公开（gpu/instances/instances/:id/instances/:id/metrics/
        // instances/:id/log/gateway/models/gateway/health/stats/environments*/
        // tasks*）
        for r in &routes {
            if r.method == HttpMethod::Get {
                assert!(!r.requires_auth, "GET 应公开: {r:?}");
            }
        }
        // metrics 路由存在且公开读
        let m = routes
            .iter()
            .find(|r| r.path == "/api/v1/llm/instances/:id/metrics")
            .expect("应有 metrics 路由");
        assert_eq!(m.method, HttpMethod::Get);
        assert!(!m.requires_auth, "metrics 公开读");
        // 实例日志路由存在且公开读（权限风格对齐 metrics）
        let l = routes
            .iter()
            .find(|r| r.path == "/api/v1/llm/instances/:id/log")
            .expect("应有实例日志路由");
        assert_eq!(l.method, HttpMethod::Get);
        assert!(!l.requires_auth, "实例日志公开读");
        // gateway 聚合路由存在且公开读
        for p in ["/api/v1/llm/gateway/models", "/api/v1/llm/gateway/health"] {
            let g = routes
                .iter()
                .find(|r| r.path == p)
                .unwrap_or_else(|| panic!("应有 {p} 路由"));
            assert_eq!(g.method, HttpMethod::Get);
            assert!(!g.requires_auth, "{p} 公开读");
        }
        // 推理环境路由（llm_envs 子模块）全量在册
        for p in [
            "/api/v1/llm/environments",
            "/api/v1/llm/environments/tasks",
            "/api/v1/llm/environments/tasks/:id",
            "/api/v1/llm/environments/:name/update",
            "/api/v1/llm/environments/:name/default",
            "/api/v1/llm/environments/:name",
        ] {
            assert!(routes.iter().any(|r| r.path == p), "应有推理环境路由 {p}");
        }
    }

    // ---- 实例 CRUD ----

    #[tokio::test]
    async fn create_instance_then_list_contains_new() {
        let h = LlmRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/llm/instances",
                serde_json::json!({
                    "name": "test-llm",
                    "model": "Qwen/Qwen2.5-7B-Instruct",
                    "source_type": "huggingface"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "create body: {resp:?}");
        assert_eq!(resp.body["status"], "stopped", "不 autostart 默认 stopped");
        assert_eq!(resp.body["source_type"], "huggingface");
        let id = resp.body["id"].as_str().unwrap().to_string();
        let port = resp.body["port"].as_u64().unwrap();
        assert!(port >= 8000, "端口应 >= 8000");
        // 列表含新实例
        let resp = h.handle(get_req("/api/v1/llm/instances")).await.unwrap();
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], id);
    }

    #[tokio::test]
    async fn create_instance_defaults_source_type_to_huggingface() {
        let h = LlmRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/llm/instances",
                serde_json::json!({"name": "x", "model": "Qwen/Qwen2.5-7B"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["source_type"], "huggingface");
    }

    #[tokio::test]
    async fn create_instance_validates_empty_name() {
        let h = LlmRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/llm/instances",
                serde_json::json!({"name": "", "model": "x"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    #[tokio::test]
    async fn create_instance_rejects_bad_source_type() {
        let h = LlmRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/llm/instances",
                serde_json::json!({"name": "x", "model": "y", "source_type": "bogus"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    #[tokio::test]
    async fn create_instance_picks_distinct_port() {
        let h = LlmRouteHandler::with_demo(); // 已有 8000/8001 demo
        let resp = h
            .handle(post_req(
                "/api/v1/llm/instances",
                serde_json::json!({"name": "x", "model": "y"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        let port = resp.body["port"].as_u64().unwrap();
        // 应跳过 8000/8001 → 8002
        assert!(port >= 8002, "新实例端口应避开 demo 占用: {port}");
    }

    #[tokio::test]
    async fn get_instance_returns_detail() {
        let h = LlmRouteHandler::with_demo();
        let resp = h
            .handle(get_req("/api/v1/llm/instances/llm-1"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["id"], "llm-1");
        assert_eq!(resp.body["model"], "Qwen/Qwen2.5-7B-Instruct");
    }

    #[tokio::test]
    async fn get_instance_missing_returns_404() {
        let h = LlmRouteHandler::with_demo();
        let resp = h
            .handle(get_req("/api/v1/llm/instances/nope"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn stop_sets_status_stopped_and_clears_pid() {
        let h = LlmRouteHandler::with_demo();
        // llm-1 默认 running（pid=None 占位），stop 应置 stopped
        let resp = h
            .handle(post_req(
                "/api/v1/llm/instances/llm-1/stop",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "stop body: {resp:?}");
        assert_eq!(resp.body["status"], "stopped");
        assert!(resp.body["pid"].is_null(), "pid 应清空");
    }

    #[tokio::test]
    async fn stop_missing_returns_404() {
        let h = LlmRouteHandler::with_demo();
        let resp = h
            .handle(post_req(
                "/api/v1/llm/instances/nope/stop",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn delete_instance_removes() {
        let h = LlmRouteHandler::with_demo();
        let before = h.instances_snapshot().len();
        let resp = h
            .handle(del_req("/api/v1/llm/instances/llm-2"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["ok"], true);
        assert_eq!(h.instances_snapshot().len(), before - 1);
    }

    #[tokio::test]
    async fn delete_missing_returns_404() {
        let h = LlmRouteHandler::with_demo();
        let resp = h
            .handle(del_req("/api/v1/llm/instances/nope"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // ---- SQLite 持久化（llm_instances 表；2026-08-22：重启恢复定义，不自动拉起）----

    /// 临时 llm.db 路径（测试隔离，用后删）。
    fn tmp_db_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("nexos-llm-{tag}-{}.db", os_core::Uuid::new_v4()))
    }

    /// 经 API 创建一个全字段实例，返回响应 body（含 id）。
    async fn create_via_api(h: &LlmRouteHandler, name: &str, model: &str) -> serde_json::Value {
        let resp = h
            .handle(post_req(
                "/api/v1/llm/instances",
                serde_json::json!({
                    "name": name,
                    "model": model,
                    "source_type": "local",
                    "config": {
                        "tensor_parallel_size": 2,
                        "gpu_memory_utilization": 0.85,
                        "max_model_len": 4096,
                        "quantization": "awq",
                        "dtype": "float16",
                        "served_model_name": "my-alias",
                        "trust_remote_code": true,
                        "extra_args": ["--swap-space", "4"]
                    }
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "create body: {resp:?}");
        resp.body
    }

    /// 清掉首次开库 seed 的 2 个 demo 实例（让计数断言只看测试自建实例）。
    async fn clear_demo(h: &LlmRouteHandler) {
        for id in ["llm-1", "llm-2"] {
            let path = format!("/api/v1/llm/instances/{id}");
            let resp = h.handle(del_req(&path)).await.unwrap();
            assert_eq!(resp.status, 200, "删 demo {id}");
        }
    }

    // P1. 创建 → 重启（同库重开）→ 定义在：status=stopped、pid/error 清空、
    //     config/source_type/port/name/model/created_at 字段完整还原
    #[tokio::test]
    async fn persist_create_survives_restart_as_stopped() {
        let db = tmp_db_path("create");
        let id = {
            let h = LlmRouteHandler::with_db_path(db.to_str().unwrap());
            clear_demo(&h).await;
            let body = create_via_api(&h, "推理A", "/tank/models/qwen-7b").await;
            let id = body["id"].as_str().unwrap().to_string();
            assert_eq!(body["status"], "stopped", "不 autostart 默认 stopped");
            assert!(!body["created_at"].as_str().unwrap_or("").is_empty());
            // 表里同步有行（status/pid/error 落表）
            {
                let conn = h.db.lock().unwrap();
                let (status, pid, error): (String, Option<i64>, Option<String>) = conn
                    .query_row(
                        "SELECT status,pid,error FROM llm_instances WHERE id=?1",
                        params![id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .unwrap();
                assert_eq!(status, "stopped");
                assert_eq!(pid, None);
                assert_eq!(error, None);
            }
            id
        };
        // 模拟服务重启：同一 DB 重开（handler drop 即旧进程消亡）
        let h2 = LlmRouteHandler::with_db_path(db.to_str().unwrap());
        let snap = h2.instances_snapshot();
        let inst = snap.iter().find(|i| i.id == id).expect("重启后定义应在");
        assert_eq!(inst.name, "推理A");
        assert_eq!(inst.model, "/tank/models/qwen-7b");
        assert_eq!(inst.source_type, "local");
        assert!(!inst.created_at.is_empty(), "created_at 应还原");
        // 运行态字段一律重置（不自动拉起）
        assert_eq!(inst.status, "stopped", "重启后应 stopped");
        assert!(inst.pid.is_none(), "重启后 pid 清空");
        assert!(inst.error.is_none(), "重启后 error 清空");
        assert!(inst.health.is_none());
        // config JSON 完整 roundtrip（全字段逐项核对）
        assert_eq!(inst.config.tensor_parallel_size, 2);
        assert!((inst.config.gpu_memory_utilization - 0.85).abs() < f32::EPSILON);
        assert_eq!(inst.config.max_model_len, 4096);
        assert_eq!(inst.config.quantization.as_deref(), Some("awq"));
        assert_eq!(inst.config.dtype, "float16");
        assert_eq!(inst.config.served_model_name.as_deref(), Some("my-alias"));
        assert!(inst.config.trust_remote_code);
        assert_eq!(inst.config.extra_args, vec!["--swap-space", "4"]);
        assert!(inst.port >= 8123, "端口应还原且在 8123 基点段");
        let _ = std::fs::remove_file(&db);
    }

    // P2. 删除 → 重启 → 不在（表行与内存态同步消失）
    #[tokio::test]
    async fn persist_delete_gone_after_restart() {
        let db = tmp_db_path("del");
        let id = {
            let h = LlmRouteHandler::with_db_path(db.to_str().unwrap());
            clear_demo(&h).await;
            let body = create_via_api(&h, "待删除", "Qwen/Qwen2.5-7B-Instruct").await;
            let id = body["id"].as_str().unwrap().to_string();
            let del = h
                .handle(del_req(&format!("/api/v1/llm/instances/{id}")))
                .await
                .unwrap();
            assert_eq!(del.status, 200);
            id
        };
        let h2 = LlmRouteHandler::with_db_path(db.to_str().unwrap());
        assert!(
            !h2.instances_snapshot().iter().any(|i| i.id == id),
            "删除后重启不应恢复"
        );
        let gone = h2
            .handle(get_req(&format!("/api/v1/llm/instances/{id}")))
            .await
            .unwrap();
        assert_eq!(gone.status, 404);
        let _ = std::fs::remove_file(&db);
    }

    // P3. 启停同步：运行态（running+pid）落表 → stop 端点同步 stopped/pid=NULL
    //     （start 端点会真 spawn vllm，测试不走——运行态由直写内存+persist 模拟）
    #[tokio::test]
    async fn persist_stop_syncs_table() {
        let db = tmp_db_path("stop");
        let h = LlmRouteHandler::with_db_path(db.to_str().unwrap());
        clear_demo(&h).await;
        let body = create_via_api(&h, "停止同步", "Qwen/X").await;
        let id = body["id"].as_str().unwrap().to_string();
        // 模拟运行态：内存改 running + pid（9_999_999 超出默认 pid_max，kill 必落空）
        {
            let mut insts = h.instances.lock().unwrap();
            let snap = {
                let i = insts.iter_mut().find(|i| i.id == id).unwrap();
                i.status = "running".into();
                i.pid = Some(9_999_999);
                i.error = Some("上次启动超时".into());
                i.clone()
            };
            h.persist(&snap);
        }
        // 表同步为 running + pid + error
        {
            let conn = h.db.lock().unwrap();
            let (status, pid, error): (String, Option<i64>, Option<String>) = conn
                .query_row(
                    "SELECT status,pid,error FROM llm_instances WHERE id=?1",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();
            assert_eq!(status, "running", "启停同步：running 应落表");
            assert_eq!(pid, Some(9_999_999));
            assert_eq!(error.as_deref(), Some("上次启动超时"));
        }
        // stop 端点 → 表同步 stopped / pid NULL
        let resp = h
            .handle(post_req(
                &format!("/api/v1/llm/instances/{id}/stop"),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["status"], "stopped");
        {
            let conn = h.db.lock().unwrap();
            let (status, pid): (String, Option<i64>) = conn
                .query_row(
                    "SELECT status,pid FROM llm_instances WHERE id=?1",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(status, "stopped", "stop 后表应同步 stopped");
            assert_eq!(pid, None, "stop 后表 pid 应清空");
        }
        let _ = std::fs::remove_file(&db);
    }

    // P4. 运行态落表 → 重启 → 强制重置（表里即便有 running/pid/error，恢复
    //     一律 stopped/pid=None/error=None——旧 pid 不可信，不自动拉起）
    #[tokio::test]
    async fn persist_running_state_reset_on_restart() {
        let db = tmp_db_path("reset");
        let h = LlmRouteHandler::with_db_path(db.to_str().unwrap());
        clear_demo(&h).await;
        let body = create_via_api(&h, "运行重启", "Qwen/Y").await;
        let id = body["id"].as_str().unwrap().to_string();
        {
            let mut insts = h.instances.lock().unwrap();
            let snap = {
                let i = insts.iter_mut().find(|i| i.id == id).unwrap();
                i.status = "running".into();
                i.pid = Some(9_999_998);
                i.clone()
            };
            h.persist(&snap);
        }
        drop(h);
        let h2 = LlmRouteHandler::with_db_path(db.to_str().unwrap());
        let inst = h2
            .instances_snapshot()
            .iter()
            .find(|i| i.id == id)
            .unwrap()
            .clone();
        assert_eq!(inst.status, "stopped", "重启后运行态应重置 stopped");
        assert!(inst.pid.is_none());
        assert!(inst.error.is_none());
        let _ = std::fs::remove_file(&db);
    }

    // P5. 多实例恢复 + id 计数器越过已恢复最大后缀（新建不撞 id）
    #[tokio::test]
    async fn persist_multi_instance_restore_and_id_continuity() {
        let db = tmp_db_path("multi");
        let mut ids = Vec::new();
        {
            let h = LlmRouteHandler::with_db_path(db.to_str().unwrap());
            clear_demo(&h).await;
            for (name, model) in [
                ("实例一", "/tank/models/a"),
                ("实例二", "/tank/models/b"),
                ("实例三", "Qwen/Qwen2.5-14B-Instruct"),
            ] {
                let body = create_via_api(&h, name, model).await;
                ids.push(body["id"].as_str().unwrap().to_string());
            }
            assert_eq!(h.instances_snapshot().len(), 3);
        }
        let h2 = LlmRouteHandler::with_db_path(db.to_str().unwrap());
        let snap = h2.instances_snapshot();
        assert_eq!(snap.len(), 3, "多实例应全部恢复");
        for (i, id) in ids.iter().enumerate() {
            let inst = snap.iter().find(|x| &x.id == id).expect("恢复缺实例");
            assert_eq!(inst.name, format!("实例{}", ["一", "二", "三"][i]));
            assert_eq!(inst.status, "stopped");
        }
        // 各实例端口互不冲突地还原
        let mut ports: Vec<u16> = snap.iter().map(|i| i.port).collect();
        ports.sort_unstable();
        ports.dedup();
        assert_eq!(ports.len(), 3, "恢复端口应互不相同");
        // 新建实例 id 不与恢复 id 冲突（计数器越过最大后缀）
        let body = create_via_api(&h2, "实例四", "/tank/models/d").await;
        let new_id = body["id"].as_str().unwrap().to_string();
        assert!(!ids.contains(&new_id), "新 id 不应与恢复 id 冲突");
        assert_eq!(h2.instances_snapshot().len(), 4);
        let _ = std::fs::remove_file(&db);
    }

    // ---- 推理环境集成（llm_envs 子模块；环境行直插模拟已建好，不真跑 uv）----

    /// 直插一条环境行（绕过任务线程——测试只关心注册表解析语义）。
    fn insert_env_row(h: &LlmRouteHandler, name: &str, status: &str, is_default: bool) {
        let conn = h.db.lock().unwrap();
        conn.execute(
            "INSERT INTO llm_environments
             (name,path,python_version,vllm_version_requested,vllm_version_installed,
              is_default,status,created_at,updated_at,size_bytes)
             VALUES (?1,?2,'3.12','latest','0.26.0',?3,?4,1,1,0)",
            params![
                name,
                format!("/tmp/llm-envs/{name}"),
                i64::from(is_default),
                status
            ],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn environments_routes_delegated_to_env_module() {
        let h = LlmRouteHandler::with_empty();
        let resp = h.handle(get_req("/api/v1/llm/environments")).await.unwrap();
        assert_eq!(resp.status, 200, "env list body: {resp:?}");
        assert_eq!(resp.body["environments"].as_array().unwrap().len(), 0);
        assert!(resp.body["default_name"].is_null());
        // 任务列表同样委托（空任务表）
        let resp = h
            .handle(get_req("/api/v1/llm/environments/tasks"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["tasks"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn default_env_bin_falls_back_without_ready_default_row() {
        let h = LlmRouteHandler::with_empty();
        // 注册表空 → 回退旧硬编码（向后兼容）
        assert_eq!(
            h.default_env_bin(),
            (VLLM_BIN.to_string(), VLLM_ENV_PATH.to_string())
        );
        // 有默认行但非 ready（还在创建）→ 仍回退
        insert_env_row(&h, "pending", "creating", true);
        assert_eq!(
            h.default_env_bin(),
            (VLLM_BIN.to_string(), VLLM_ENV_PATH.to_string()),
            "非 ready 默认行不可用，应回退"
        );
    }

    #[tokio::test]
    async fn default_env_bin_resolves_ready_default_row() {
        let h = LlmRouteHandler::with_empty();
        insert_env_row(&h, "main", "ready", true);
        assert_eq!(
            h.default_env_bin(),
            (
                "/tmp/llm-envs/main/bin/vllm".to_string(),
                "/tmp/llm-envs/main/bin".to_string()
            )
        );
    }

    #[tokio::test]
    async fn env_bin_for_named_env_requires_exists_and_ready() {
        let h = LlmRouteHandler::with_empty();
        insert_env_row(&h, "staging", "creating", false);
        // 指定环境不存在 / 非 ready → Err（显式指定不静默回退）
        assert!(h.env_bin_for(Some("ghost")).is_err(), "不存在应 Err");
        assert!(h.env_bin_for(Some("staging")).is_err(), "非 ready 应 Err");
        // 未指定 → 回退语义 Ok
        assert!(h.env_bin_for(None).is_ok());
        // ready 后可用
        {
            let conn = h.db.lock().unwrap();
            conn.execute("UPDATE llm_environments SET status='ready'", [])
                .unwrap();
        }
        let (bin, dir) = h.env_bin_for(Some("staging")).expect("ready 环境应可解析");
        assert_eq!(bin, "/tmp/llm-envs/staging/bin/vllm");
        assert_eq!(dir, "/tmp/llm-envs/staging/bin");
    }

    #[tokio::test]
    async fn create_instance_rejects_unknown_env_name() {
        let h = LlmRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/llm/instances",
                serde_json::json!({"name": "x", "model": "y", "env_name": "ghost"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "未知环境应 400: {resp:?}");
    }

    #[tokio::test]
    async fn create_instance_records_env_name_and_survives_restart() {
        let db = tmp_db_path("envname");
        {
            let h = LlmRouteHandler::with_db_path(db.to_str().unwrap());
            insert_env_row(&h, "main", "ready", true);
            let resp = h
                .handle(post_req(
                    "/api/v1/llm/instances",
                    serde_json::json!({"name": "绑定环境", "model": "Qwen/X", "env_name": "main"}),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 201, "create body: {resp:?}");
            assert_eq!(resp.body["env_name"], "main");
            let id = resp.body["id"].as_str().unwrap().to_string();
            // 表行同步含 env_name
            let conn = h.db.lock().unwrap();
            let env: Option<String> = conn
                .query_row(
                    "SELECT env_name FROM llm_instances WHERE id=?1",
                    params![id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(env.as_deref(), Some("main"));
        }
        // 重启恢复 env_name
        let h2 = LlmRouteHandler::with_db_path(db.to_str().unwrap());
        let snap = h2.instances_snapshot();
        let inst = snap
            .iter()
            .find(|i| i.name == "绑定环境")
            .expect("重启后实例应在");
        assert_eq!(inst.env_name.as_deref(), Some("main"));
        let _ = std::fs::remove_file(&db);
    }

    // ---- 健康探测 / 推理测试（降级，不 panic）----

    #[tokio::test]
    async fn health_probe_missing_returns_404() {
        let h = LlmRouteHandler::with_demo();
        let resp = h
            .handle(post_req(
                "/api/v1/llm/instances/nope/health",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn health_probe_updates_instance_health_without_panic() {
        // llm-1 端口 8000：本机可能恰好有 vllm 在跑（alive=true）也可能没有
        // （alive=false）。测试只断言"不 panic + 响应结构正确 + health 字段已写入"，
        // 不绑定端口 8000 的实际占用情况（环境无关）。
        let h = LlmRouteHandler::with_demo();
        let resp = h
            .handle(post_req(
                "/api/v1/llm/instances/llm-1/health",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "health body: {resp:?}");
        assert!(
            resp.body["alive"].is_boolean(),
            "alive 应为布尔（环境决定真假）: {resp:?}"
        );
        // 实例 health 字段已写入
        let snap = h.instances_snapshot();
        let i = snap.iter().find(|i| i.id == "llm-1").unwrap();
        assert!(i.health.is_some(), "health 字段应已写入");
    }

    #[tokio::test]
    async fn chat_missing_returns_404() {
        let h = LlmRouteHandler::with_demo();
        let resp = h
            .handle(post_req(
                "/api/v1/llm/instances/nope/chat",
                serde_json::json!({"messages": [{"role": "user", "content": "hi"}]}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn chat_rejects_empty_messages() {
        let h = LlmRouteHandler::with_demo();
        let resp = h
            .handle(post_req(
                "/api/v1/llm/instances/llm-1/chat",
                serde_json::json!({"messages": []}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    #[tokio::test]
    async fn chat_rejects_stopped_instance() {
        // llm-2 是 stopped，chat 应返回 400（实例未运行）
        let h = LlmRouteHandler::with_demo();
        let resp = h
            .handle(post_req(
                "/api/v1/llm/instances/llm-2/chat",
                serde_json::json!({"messages": [{"role": "user", "content": "hi"}]}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        assert!(resp.body["error"].as_str().unwrap().contains("未运行"));
    }

    // ---- 端口选取（真实试绑）与手动端口（2026-08-31）----

    /// 拿一个几乎必然空闲的 0.0.0.0 端口（bind :0 后立即释放；无连接无
    /// TIME_WAIT，立即可重绑）。
    fn free_wildcard_port() -> u16 {
        let l = std::net::TcpListener::bind(("0.0.0.0", 0)).expect("bind 失败");
        l.local_addr().expect("local_addr 失败").port()
    }

    #[test]
    fn pick_free_port_from_skips_real_occupied_port() {
        // 需求 1 核心：先真实占一个口（0.0.0.0，与试绑同口径），选口必须跳过它
        let holder = std::net::TcpListener::bind(("0.0.0.0", 0)).expect("bind 失败");
        let occupied = holder.local_addr().expect("local_addr 失败").port();
        let used = std::collections::HashSet::new();
        let picked = pick_free_port_from(occupied, &used);
        assert_ne!(picked, occupied, "真实被外部进程占用的端口应被跳过");
        assert!(port_bindable(picked), "返回的端口必须真实可绑: {picked}");
        drop(holder);
    }

    #[test]
    fn pick_free_port_from_dedupes_table_ports() {
        // 实例表去重保留：8123/8124 在表内 → 选口越过它们
        let used: std::collections::HashSet<u16> = [INSTANCE_PORT_BASE, INSTANCE_PORT_BASE + 1]
            .into_iter()
            .collect();
        let picked = pick_free_port_from(INSTANCE_PORT_BASE, &used);
        assert!(
            picked > INSTANCE_PORT_BASE + 1,
            "应跳过表内 8123/8124 与真实被占口: {picked}"
        );
        assert!(port_bindable(picked));
    }

    #[tokio::test]
    async fn pick_free_port_returns_bindable_port() {
        // 环境无关断言：无论本机 8123 是否被生产 os-api/vLLM 真实占用，选出的
        // 口必须 >= 8123 且真实可绑（旧算法只查表不试绑，被占照样返回）
        let h = LlmRouteHandler::with_empty();
        let picked = h.pick_free_port();
        assert!(picked >= INSTANCE_PORT_BASE, "基点 8123 起: {picked}");
        assert!(port_bindable(picked), "选出的端口必须真实可绑: {picked}");
    }

    #[tokio::test]
    async fn create_instance_manual_port_happy_path() {
        let h = LlmRouteHandler::with_empty();
        let p = free_wildcard_port();
        let resp = h
            .handle(post_req(
                "/api/v1/llm/instances",
                serde_json::json!({"name": "手动端口", "model": "Qwen/X", "port": p}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "create body: {resp:?}");
        assert_eq!(resp.body["port"].as_u64().unwrap(), u64::from(p));
        // 唯一真相源镜像：config JSON 的 port 与行 port 同源写入
        assert_eq!(
            resp.body["config"]["port"].as_u64().unwrap(),
            u64::from(p),
            "config.port 应与行 port 一致"
        );
    }

    #[tokio::test]
    async fn create_instance_manual_port_rejects_out_of_range() {
        let h = LlmRouteHandler::with_empty();
        // 1023/0 越下界；70000 超 u16——用 u64 承接，统一走 400 而非解析失败
        for bad in [1023u64, 0, 70000] {
            let resp = h
                .handle(post_req(
                    "/api/v1/llm/instances",
                    serde_json::json!({"name": "x", "model": "y", "port": bad}),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 400, "port {bad} 越界应 400: {resp:?}");
            assert!(resp.body["error"].as_str().unwrap().contains("越界"));
        }
    }

    #[tokio::test]
    async fn create_instance_manual_port_rejects_table_conflict() {
        let h = LlmRouteHandler::with_demo(); // demo 占 8000/8001
        let resp = h
            .handle(post_req(
                "/api/v1/llm/instances",
                serde_json::json!({"name": "x", "model": "y", "port": 8000}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 409, "表内冲突应 409: {resp:?}");
        assert!(
            resp.body["error"].as_str().unwrap().contains("llm-1"),
            "错误应带占用实例 id: {resp:?}"
        );
    }

    #[tokio::test]
    async fn create_instance_manual_port_rejects_reserved_segment() {
        let h = LlmRouteHandler::with_empty();
        for p in RESERVED_INSTANCE_PORTS {
            let resp = h
                .handle(post_req(
                    "/api/v1/llm/instances",
                    serde_json::json!({"name": "x", "model": "y", "port": p}),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 409, "保留段 {p} 应 409: {resp:?}");
            assert!(resp.body["error"].as_str().unwrap().contains("保留"));
        }
    }

    #[tokio::test]
    async fn create_instance_manual_port_rejects_real_occupied() {
        // 真实试绑被占 → 409 带原因（生产 8123 被外部进程占的复盘场景）
        let holder = std::net::TcpListener::bind(("0.0.0.0", 0)).expect("bind 失败");
        let occupied = holder.local_addr().expect("local_addr 失败").port();
        let h = LlmRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/llm/instances",
                serde_json::json!({"name": "x", "model": "y", "port": occupied}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 409, "真实被占应 409: {resp:?}");
        assert!(resp.body["error"].as_str().unwrap().contains("占用"));
        drop(holder);
    }

    #[tokio::test]
    async fn create_instance_default_port_falls_back_to_auto() {
        // 未给 port → 自动选（真实试绑 + 表去重）
        let h = LlmRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/llm/instances",
                serde_json::json!({"name": "自动", "model": "Qwen/X"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "create body: {resp:?}");
        let port = resp.body["port"].as_u64().unwrap();
        assert!(port >= u64::from(INSTANCE_PORT_BASE));
        assert!(port_bindable(u16::try_from(port).unwrap()));
    }

    // ---- 端口唯一真相源：config.port ≠ 行 port 的收敛（2026-08-31 缺陷）----

    #[tokio::test]
    async fn reconcile_converges_config_port_to_row_port() {
        // 双写残留（行 8124、config 8123 → 旧代码 spawn 绑 8123、探测打 8124
        // 永久卡 starting）：列表修正后 config.port 收敛到行 port 且落库
        let h = LlmRouteHandler::with_empty();
        let p = closed_port();
        inject_instance_full(&h, "llm-dual", p, "error", None); // error 不触发探测，隔离收敛逻辑
        {
            let mut insts = h.instances.lock().unwrap();
            let i = insts.iter_mut().find(|i| i.id == "llm-dual").unwrap();
            assert!(i.port > 1024, "测试前提：端口足够大可制造残留");
            i.config.port = i.port - 1; // 历史双写残留
        }
        let list = h.reconcile_instance_statuses().await;
        let inst = list.iter().find(|i| i.id == "llm-dual").unwrap();
        assert_eq!(inst.config.port, inst.port, "config.port 应收敛到行 port");
        assert_eq!(inst.port, p, "行 port（真相源）不动");
        // 内存态与 DB 行同步收敛
        let snap = h.instances_snapshot();
        let i = snap.iter().find(|i| i.id == "llm-dual").unwrap();
        assert_eq!(i.config.port, i.port);
        let conn = h.db.lock().unwrap();
        let (row_port, config_json): (i64, String) = conn
            .query_row(
                "SELECT port, config FROM llm_instances WHERE id='llm-dual'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(row_port, i64::from(p));
        let cfg: VllmConfig = serde_json::from_str(&config_json).unwrap();
        assert_eq!(cfg.port, p, "落库的 config JSON port 应已收敛");
    }

    #[tokio::test]
    async fn persisted_dual_port_converges_on_restart_load() {
        // 同一残留落库后重启恢复：load 时 config.port 直接对齐行 port
        let db = tmp_db_path("dualport");
        {
            let h = LlmRouteHandler::with_db_path(db.to_str().unwrap());
            clear_demo(&h).await;
            let body = create_via_api(&h, "双写残留", "/tank/models/x").await;
            let id = body["id"].as_str().unwrap().to_string();
            // 直接以 config.port=p-1 的形态覆写表行（模拟历史 bug 的落库产物）
            let conn = h.db.lock().unwrap();
            let inst = h
                .instances_snapshot()
                .into_iter()
                .find(|i| i.id == id)
                .unwrap();
            let mut broken = inst.clone();
            broken.config.port = broken.port - 1;
            persist_instance(&conn, &broken).unwrap();
        }
        let h2 = LlmRouteHandler::with_db_path(db.to_str().unwrap());
        let inst = h2
            .instances_snapshot()
            .into_iter()
            .find(|i| i.name == "双写残留")
            .unwrap();
        assert_eq!(
            inst.config.port, inst.port,
            "重启恢复即应收敛 config.port → 行 port"
        );
        let _ = std::fs::remove_file(&db);
    }

    // ---- starting→running 列表修正（2026-08-31 缺陷：加载超窗不再卡死）----

    #[tokio::test]
    async fn instances_list_corrects_starting_to_running_when_port_alive() {
        // 模型加载 > 拉起时一次性探测窗口（实测 19G 权重 ~80s+）→ 卡 starting；
        // 列表修正把 starting 纳入探测：/v1/models 就绪即翻 running
        let port = spawn_fake_v1_models_server(vec![sample_v1_models_json(&["my-alias"])]);
        let h = LlmRouteHandler::with_empty();
        inject_instance_full(&h, "llm-booting", port, "starting", Some("my-alias"));
        let resp = h.handle(get_req("/api/v1/llm/instances")).await.unwrap();
        assert_eq!(resp.status, 200, "body: {resp:?}");
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr[0]["status"], "running", "starting + 模型就绪 → running");
        assert_eq!(
            db_instance_status(&h, "llm-booting"),
            "running",
            "修正应落库"
        );
    }

    #[tokio::test]
    async fn instances_list_keeps_starting_when_port_dead() {
        // 探测不通保持 starting（不猜 error——spawn 监测负责换口/落 error）
        let h = LlmRouteHandler::with_empty();
        inject_instance_full(&h, "llm-booting-dead", closed_port(), "starting", None);
        let resp = h.handle(get_req("/api/v1/llm/instances")).await.unwrap();
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr[0]["status"], "starting", "端口未就绪不应离开 starting");
    }

    // ---- launch_command（接入说明面板「启动参数」块，2026-08-31）----

    #[tokio::test]
    async fn instance_responses_expose_launch_command_constructed_when_never_spawned() {
        let h = LlmRouteHandler::with_empty();
        let body = create_via_api(&h, "接入说明实例", "/tank/models/Qwen3.5-9B").await;
        // 创建响应（未拉起）：按当前 config 构造（vllm 占位二进制 + build_vllm_serve_cmd 全参）
        let cmd = body["launch_command"].as_str().unwrap().to_string();
        assert!(
            cmd.starts_with("vllm serve /tank/models/Qwen3.5-9B "),
            "应以 vllm serve <model> 开头: {cmd}"
        );
        // create_via_api 的固定 config 关键参数逐项在场（与 build_vllm_serve_cmd 同源）
        assert!(cmd.contains("--served-model-name my-alias"), "{cmd}");
        assert!(cmd.contains("--max-model-len 4096"), "{cmd}");
        assert!(cmd.contains("--gpu-memory-utilization 0.85"), "{cmd}");
        assert!(cmd.contains("--dtype float16"), "{cmd}");
        assert!(cmd.contains("--quantization awq"), "{cmd}");
        assert!(cmd.contains("--trust-remote-code"), "{cmd}");
        assert!(cmd.contains("--swap-space 4"), "extra_args 应原样追加: {cmd}");
        let port = body["port"].as_u64().unwrap();
        assert!(
            cmd.contains(&format!("--port {port}")),
            "端口应与行 port 同源: {cmd}"
        );
        // 列表与详情同字段同值
        let list = h.handle(get_req("/api/v1/llm/instances")).await.unwrap();
        assert_eq!(
            list.body.as_array().unwrap()[0]["launch_command"]
                .as_str()
                .unwrap(),
            cmd,
            "列表也应注入 launch_command"
        );
        let id = body["id"].as_str().unwrap();
        let detail = h
            .handle(get_req(&format!("/api/v1/llm/instances/{id}")))
            .await
            .unwrap();
        assert_eq!(detail.body["launch_command"].as_str().unwrap(), cmd);
        // 结构体原生字段（从未拉起）为 null——响应字段是注入的恒有值
        assert!(body["launch_command"].is_string());
    }

    #[tokio::test]
    async fn launch_command_persists_real_command_across_reopen() {
        // 直接落一行带真实命令的实例（等价 spawn 后落库），重开同一文件恢复
        let dir = std::env::temp_dir().join(format!("nexos-llm-lc-{}", os_core::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("llm.db");
        {
            let h = LlmRouteHandler::with_db_path(db.to_str().unwrap());
            let mut inst = h.instances_snapshot().into_iter().next().unwrap();
            inst.launch_command = Some(
                "/home/oem/llm-envs/default/bin/vllm serve /m --port 8123 --max-model-len 8192"
                    .into(),
            );
            h.persist(&inst);
        }
        let h2 = LlmRouteHandler::with_db_path(db.to_str().unwrap());
        let inst = h2.instances_snapshot().into_iter().next().unwrap();
        // 恢复行保留真实命令（status 重置 stopped，launch_command 不清——它描述
        // 最近一次拉起，不是运行态）
        assert_eq!(
            inst.launch_command.as_deref(),
            Some("/home/oem/llm-envs/default/bin/vllm serve /m --port 8123 --max-model-len 8192")
        );
        assert!(
            effective_launch_command(&inst).starts_with("/home/oem/llm-envs"),
            "有真实命令时不再用 vllm 占位构造"
        );
        let _ = std::fs::remove_file(&db);
    }

    #[test]
    fn effective_launch_command_falls_back_to_construction() {
        let h = LlmRouteHandler::with_demo();
        let inst = h.instances_snapshot().into_iter().find(|i| i.id == "llm-1").unwrap();
        let cmd = effective_launch_command(&inst);
        assert!(cmd.starts_with("vllm serve Qwen/Qwen2.5-7B-Instruct"), "{cmd}");
        assert!(cmd.contains("--port 8000"), "{cmd}");
    }

    // ---- 按实例日志文件 + GET /:id/log（2026-08-31）----

    #[test]
    fn instance_log_path_modes() {
        // 默认：目录 + llm-vllm-<id>.log；单文件模式（NEXOS_LLM_SPAWN_LOG）优先
        assert_eq!(
            instance_log_path_with("/tmp", None, "llm-9"),
            "/tmp/llm-vllm-llm-9.log"
        );
        assert_eq!(
            instance_log_path_with("/x", Some("/tmp/single.log"), "llm-9"),
            "/tmp/single.log"
        );
    }

    #[tokio::test]
    async fn instance_log_endpoint_returns_tail_lines() {
        let dir = std::env::temp_dir().join(format!("nexos-llm-log-{}", os_core::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let h = LlmRouteHandler::with_empty().with_spawn_log_dir(dir.to_str().unwrap());
        let body = create_via_api(&h, "带日志", "/tank/models/x").await;
        let id = body["id"].as_str().unwrap().to_string();
        let file = dir.join(format!("llm-vllm-{id}.log"));
        // 写 1200 行（默认 200 / 指定 10 / 上限 1000 三种裁剪可验）
        let mut content = String::new();
        for i in 0..1200 {
            content.push_str(&format!("line-{i}\n"));
        }
        std::fs::write(&file, content).unwrap();

        // 默认 tail=200：取最后 200 行（line-1000..line-1199）
        let resp = h
            .handle(get_req(&format!("/api/v1/llm/instances/{id}/log")))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "log body: {resp:?}");
        assert_eq!(resp.body["instance_id"], id);
        assert_eq!(resp.body["status"], "stopped");
        assert_eq!(resp.body["file"], file.to_str().unwrap());
        let lines = resp.body["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 200, "默认 200 行");
        assert_eq!(lines[0], "line-1000");
        assert_eq!(lines[199], "line-1199");

        // 指定 tail=10
        let resp = h
            .handle(get_req(&format!("/api/v1/llm/instances/{id}/log?tail=10")))
            .await
            .unwrap();
        let lines = resp.body["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 10);
        assert_eq!(lines[0], "line-1190");
        assert_eq!(lines[9], "line-1199");

        // 超上限 clamp 到 1000
        let resp = h
            .handle(get_req(&format!(
                "/api/v1/llm/instances/{id}/log?tail=5000"
            )))
            .await
            .unwrap();
        let lines = resp.body["lines"].as_array().unwrap();
        assert_eq!(lines.len(), INSTANCE_LOG_TAIL_MAX);
        assert_eq!(lines[0], "line-200");

        // follow 参数容忍存在（拉取式实现，响应同构）
        let resp = h
            .handle(get_req(&format!(
                "/api/v1/llm/instances/{id}/log?tail=5&follow=1"
            )))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["lines"].as_array().unwrap().len(), 5);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn instance_log_endpoint_missing_semantics() {
        let dir =
            std::env::temp_dir().join(format!("nexos-llm-log-empty-{}", os_core::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let h = LlmRouteHandler::with_empty().with_spawn_log_dir(dir.to_str().unwrap());
        let body = create_via_api(&h, "无日志", "/tank/models/x").await;
        let id = body["id"].as_str().unwrap().to_string();
        // 从未拉起 → 日志文件不存在 → 404 语义
        let resp = h
            .handle(get_req(&format!("/api/v1/llm/instances/{id}/log")))
            .await
            .unwrap();
        assert_eq!(resp.status, 404, "无日志文件应 404: {resp:?}");
        assert!(resp.body["error"].as_str().unwrap().contains("不存在"));
        // 实例本身不存在 → 404
        let resp = h
            .handle(get_req("/api/v1/llm/instances/nope/log"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- spawn 监测：端口占用判定 + 换口重试（注入 fake 子进程）----

    #[test]
    fn log_says_addr_in_use_matches_both_dialects() {
        // Python OSError 形态（vLLM 0.27 前后通用）
        assert!(log_says_addr_in_use(
            "Traceback ...\nOSError: [Errno 98] Address already in use\n"
        ));
        // uvicorn 小写形态
        assert!(log_says_addr_in_use(
            "ERROR: [Errno 98] error while attempting to bind on address ('0.0.0.0', 8123): address already in use"
        ));
        // 裸 Errno 98
        assert!(log_says_addr_in_use("bind: Errno 98"));
        // 反例：其它错误不触发换口
        assert!(!log_says_addr_in_use("CUDA out of memory"));
        assert!(!log_says_addr_in_use("Address in use（不完整）"));
        assert!(!log_says_addr_in_use(""));
    }

    #[tokio::test]
    async fn monitor_retries_once_with_new_port_and_updates_row() {
        let dir = std::env::temp_dir().join(format!("nexos-llm-mon-{}", os_core::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let h = LlmRouteHandler::with_empty();
        let p0 = free_wildcard_port();
        inject_instance_full(&h, "llm-mon", p0, "starting", None);
        let log_path = instance_log_path_with(dir.to_str().unwrap(), None, "llm-mon");
        // 首次拉起即端口占用退出（日志含 Errno 98），重试拉起换存活子进程
        std::fs::write(
            &log_path,
            "INFO: starting\nOSError: [Errno 98] Address already in use\n",
        )
        .unwrap();

        let ports: Arc<Mutex<Vec<u16>>> = Arc::new(Mutex::new(vec![]));
        // 初始 spawn：立即退出（模拟 vLLM 绑口失败）
        let initial = {
            let mut c = tokio::process::Command::new("sh");
            c.arg("-c").arg("exit 0");
            let child = c.spawn().unwrap();
            ports.lock().unwrap().push(p0);
            VllmSpawn {
                pid: child.id().unwrap(),
                child,
                log_path: log_path.clone(),
            }
        };
        // 重试执行器：返回存活子进程（sleep 5），窗口内不再退出 → 修正后 starting
        let ports_retry = Arc::clone(&ports);
        let retry_log = log_path.clone();
        let retry_fn: VllmSpawnFn = Arc::new(move |cfg: &VllmConfig| {
            ports_retry.lock().unwrap().push(cfg.port);
            let mut c = tokio::process::Command::new("sleep");
            c.arg("5");
            let child = c.spawn().unwrap();
            Ok(VllmSpawn {
                pid: child.id().unwrap(),
                child,
                log_path: retry_log.clone(),
            })
        });
        let config = VllmConfig {
            port: p0,
            ..Default::default()
        };
        monitor_addr_in_use(
            SpawnMonitorCtx {
                instances: Arc::clone(&h.instances),
                db: Arc::clone(&h.db),
                instance_id: "llm-mon".into(),
                config,
            },
            retry_fn,
            initial,
            Duration::from_millis(600),
            Duration::from_millis(50),
        )
        .await;

        let called = ports.lock().unwrap().clone();
        assert_eq!(called.len(), 2, "初始 + 换口重试各一次: {called:?}");
        let new_port = called[1];
        assert_ne!(new_port, p0, "重试必须换口");
        assert!(
            new_port >= INSTANCE_PORT_BASE,
            "换口从 8123 基点起: {new_port}"
        );
        assert!(port_bindable(new_port), "换的口必须真实空闲");
        // 实例行：两处端口同步 + pid 更新 + 保持 starting
        let inst = h
            .instances_snapshot()
            .into_iter()
            .find(|i| i.id == "llm-mon")
            .unwrap();
        assert_eq!(inst.port, new_port, "行 port 更新为最终端口");
        assert_eq!(inst.config.port, new_port, "config.port 同步（唯一真相源）");
        assert!(inst.pid.is_some());
        assert_eq!(inst.status, "starting");
        assert!(inst.error.is_none());
        // DB 行同步
        let conn = h.db.lock().unwrap();
        let (row_port, config_json): (i64, String) = conn
            .query_row(
                "SELECT port, config FROM llm_instances WHERE id='llm-mon'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(row_port, i64::from(new_port));
        let cfg: VllmConfig = serde_json::from_str(&config_json).unwrap();
        assert_eq!(cfg.port, new_port);
        // 日志体现换口（分隔标记行）
        let log = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            log.contains(&format!("自动换口 {new_port}")),
            "日志应体现最终端口: {log}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn monitor_marks_error_when_retry_also_hits_addr_in_use() {
        // 最多重试一次：重试后子进程二度端口占用退出 → 按原错误路径落 error
        let dir = std::env::temp_dir().join(format!("nexos-llm-mon2-{}", os_core::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let h = LlmRouteHandler::with_empty();
        let p0 = free_wildcard_port();
        inject_instance_full(&h, "llm-mon2", p0, "starting", None);
        let log_path = instance_log_path_with(dir.to_str().unwrap(), None, "llm-mon2");
        std::fs::write(&log_path, "[Errno 98] Address already in use\n").unwrap();

        let ports: Arc<Mutex<Vec<u16>>> = Arc::new(Mutex::new(vec![]));
        let spawn_all_exit: VllmSpawnFn = {
            let log_path = log_path.clone();
            let ports = Arc::clone(&ports);
            Arc::new(move |cfg: &VllmConfig| {
                ports.lock().unwrap().push(cfg.port);
                let mut c = tokio::process::Command::new("sh");
                c.arg("-c").arg("exit 0");
                let child = c.spawn().unwrap();
                Ok(VllmSpawn {
                    pid: child.id().unwrap(),
                    child,
                    log_path: log_path.clone(),
                })
            })
        };
        let initial = spawn_all_exit(&VllmConfig {
            port: p0,
            ..Default::default()
        })
        .unwrap();
        monitor_addr_in_use(
            SpawnMonitorCtx {
                instances: Arc::clone(&h.instances),
                db: Arc::clone(&h.db),
                instance_id: "llm-mon2".into(),
                config: VllmConfig {
                    port: p0,
                    ..Default::default()
                },
            },
            spawn_all_exit,
            initial,
            Duration::from_secs(2),
            Duration::from_millis(50),
        )
        .await;

        assert_eq!(ports.lock().unwrap().len(), 2, "初始 + 重试一次，不再多");
        let inst = h
            .instances_snapshot()
            .into_iter()
            .find(|i| i.id == "llm-mon2")
            .unwrap();
        assert_eq!(inst.status, "error", "二度失败应落 error: {inst:?}");
        assert!(inst.pid.is_none());
        assert!(
            inst.error.as_deref().unwrap().contains("被占用"),
            "error 应带端口占用原因: {:?}",
            inst.error
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn monitor_ignores_exit_without_addr_in_use() {
        // 非端口占用退出（如模型加载 OOM）：不重试、不动实例行
        let dir = std::env::temp_dir().join(format!("nexos-llm-mon3-{}", os_core::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let h = LlmRouteHandler::with_empty();
        let p0 = free_wildcard_port();
        inject_instance_full(&h, "llm-mon3", p0, "starting", None);
        let log_path = instance_log_path_with(dir.to_str().unwrap(), None, "llm-mon3");
        std::fs::write(&log_path, "CUDA out of memory\n").unwrap();

        let ports: Arc<Mutex<Vec<u16>>> = Arc::new(Mutex::new(vec![]));
        let spawn_fn: VllmSpawnFn = {
            let log_path = log_path.clone();
            let ports = Arc::clone(&ports);
            Arc::new(move |cfg: &VllmConfig| {
                ports.lock().unwrap().push(cfg.port);
                let mut c = tokio::process::Command::new("sh");
                c.arg("-c").arg("exit 0");
                let child = c.spawn().unwrap();
                Ok(VllmSpawn {
                    pid: child.id().unwrap(),
                    child,
                    log_path: log_path.clone(),
                })
            })
        };
        let initial = spawn_fn(&VllmConfig {
            port: p0,
            ..Default::default()
        })
        .unwrap();
        monitor_addr_in_use(
            SpawnMonitorCtx {
                instances: Arc::clone(&h.instances),
                db: Arc::clone(&h.db),
                instance_id: "llm-mon3".into(),
                config: VllmConfig {
                    port: p0,
                    ..Default::default()
                },
            },
            spawn_fn,
            initial,
            Duration::from_secs(2),
            Duration::from_millis(50),
        )
        .await;

        assert_eq!(ports.lock().unwrap().len(), 1, "非端口占用不重试");
        let inst = h
            .instances_snapshot()
            .into_iter()
            .find(|i| i.id == "llm-mon3")
            .unwrap();
        assert_eq!(
            inst.status, "starting",
            "保持 starting（交给健康修正/用户）"
        );
        assert_eq!(inst.port, p0, "端口不动");
    }

    // ---- chat 推理输出 reasoning 双键兼容（2026-08-31 缺陷）----

    /// 起极简 HTTP 服务回固定 JSON（/v1/chat/completions 假服务；读一次请求
    /// 即响应，手法同 spawn_fake_v1_models_server）。
    fn spawn_fake_chat_server(bodies: Vec<String>) -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 失败");
        let port = listener.local_addr().expect("local_addr 失败").port();
        std::thread::spawn(move || {
            for body in bodies {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });
        port
    }

    #[tokio::test]
    async fn chat_accepts_reasoning_key_vllm_028() {
        // vLLM 0.28：思考段键名 reasoning；小 max_tokens 下 content 为 null、
        // finish_reason=length——不是故障，应透出 reasoning + 计量
        let port = spawn_fake_chat_server(vec![serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": null, "reasoning": "让我想想…"},
                "finish_reason": "length"
            }],
            "usage": {"total_tokens": 200}
        })
        .to_string()]);
        let h = LlmRouteHandler::with_empty();
        inject_instance_full(&h, "llm-r1", port, "running", None);
        let resp = h
            .handle(post_req(
                "/api/v1/llm/instances/llm-r1/chat",
                serde_json::json!({"messages": [{"role": "user", "content": "hi"}], "max_tokens": 200}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "chat body: {resp:?}");
        assert_eq!(resp.body["content"], "", "content 空串（非错误）");
        assert_eq!(resp.body["reasoning"], "让我想想…");
        assert_eq!(resp.body["finish_reason"], "length");
        assert_eq!(resp.body["total_tokens"], 200);
    }

    #[tokio::test]
    async fn chat_accepts_reasoning_content_key_vllm_027() {
        // vLLM 0.27：reasoning_content 键 + 正常 content 并存
        let port = spawn_fake_chat_server(vec![
            serde_json::json!({
                "choices": [{
                    "message": {"role": "assistant", "content": "你好！", "reasoning_content": "先思考"},
                    "finish_reason": "stop"
                }],
                "usage": {"total_tokens": 42}
            })
            .to_string(),
        ]);
        let h = LlmRouteHandler::with_empty();
        inject_instance_full(&h, "llm-r2", port, "running", None);
        let resp = h
            .handle(post_req(
                "/api/v1/llm/instances/llm-r2/chat",
                serde_json::json!({"messages": [{"role": "user", "content": "hi"}]}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "chat body: {resp:?}");
        assert_eq!(resp.body["content"], "你好！");
        assert_eq!(resp.body["reasoning"], "先思考");
        assert_eq!(resp.body["finish_reason"], "stop");
    }

    #[tokio::test]
    async fn chat_without_content_or_reasoning_is_error() {
        // 两者都缺才是错误（带 finish_reason 提示，方便用户调大 max_tokens）
        let port = spawn_fake_chat_server(vec![
            serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": null}, "finish_reason": "length"}]
            })
            .to_string(),
        ]);
        let h = LlmRouteHandler::with_empty();
        inject_instance_full(&h, "llm-r3", port, "running", None);
        let resp = h
            .handle(post_req(
                "/api/v1/llm/instances/llm-r3/chat",
                serde_json::json!({"messages": [{"role": "user", "content": "hi"}]}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 502);
        assert!(
            resp.body["error"].as_str().unwrap().contains("max_tokens"),
            "错误应带 max_tokens 提示: {resp:?}"
        );
    }

    // ---- GPU 探测降级 ----

    #[tokio::test]
    async fn detect_gpu_returns_without_panic() {
        // 无 GPU / 无 nvidia-smi 时应返回 available=false，不 panic
        let info = detect_gpu().await;
        // 无论是否有 GPU，都不应 panic；backend 字段有效
        assert!(matches!(info.backend.as_str(), "cuda" | "rocm" | "none"));
        if !info.available {
            assert!(info.devices.is_empty());
            assert_eq!(info.backend, "none");
        }
    }

    #[tokio::test]
    async fn gpu_endpoint_returns_200_without_panic() {
        let h = LlmRouteHandler::with_demo();
        let resp = h.handle(get_req("/api/v1/llm/gpu")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body["available"].is_boolean());
        assert!(resp.body["backend"].is_string());
    }

    // ---- stats ----

    #[tokio::test]
    async fn stats_aggregates_counts_without_panic() {
        let h = LlmRouteHandler::with_demo();
        let resp = h.handle(get_req("/api/v1/llm/stats")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["instances_total"], 2, "2 个 demo 实例");
        assert!(resp.body["running"].as_u64().unwrap() >= 1, "llm-1 running");
        assert!(resp.body["stopped"].as_u64().unwrap() >= 1, "llm-2 stopped");
        assert!(resp.body["gpu_available"].is_boolean());
        assert!(resp.body["gpu_devices"].is_u64());
    }

    #[test]
    fn format_gpu_mem_trims_zeros() {
        assert_eq!(format_gpu_mem(0.9), "0.9");
        assert_eq!(format_gpu_mem(0.95), "0.95");
        assert_eq!(format_gpu_mem(1.0), "1");
    }

    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<LlmRouteHandler>();
    }

    #[tokio::test]
    async fn unmatched_route_returns_404() {
        let h = LlmRouteHandler::with_demo();
        let resp = h.handle(get_req("/api/v1/llm/nope")).await.unwrap();
        assert_eq!(resp.status, 404);
    }

    // ---- analyze-image 截图分析端点 ----

    #[tokio::test]
    async fn analyze_image_route_declared_admin() {
        let h = LlmRouteHandler::with_demo();
        let routes = h.routes().await;
        let r = routes
            .iter()
            .find(|r| r.path == "/api/v1/llm/analyze-image")
            .expect("应有 analyze-image 路由");
        assert_eq!(r.method, HttpMethod::Post);
        assert!(r.requires_auth, "analyze-image 需 admin");
        assert_eq!(r.required_roles, vec!["admin".to_string()]);
    }

    #[tokio::test]
    async fn analyze_image_missing_vllm_returns_503_or_502_without_panic() {
        // 本机若 vLLM 未跑（8000 不通），analyze_image 应降级返回 503（服务未运行）；
        // 若 vLLM 在跑（8000 通），则返回 200 或 502（取决于模型是否支持）。
        // 无论如何都不应 panic。这里传合法但极小的 PNG base64，prompt 非空。
        // （1x1 透明 PNG 的 base64）
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";
        let h = LlmRouteHandler::with_demo();
        let resp = h
            .handle(post_req(
                "/api/v1/llm/analyze-image",
                serde_json::json!({
                    "image_base64": png_b64,
                    "prompt": "describe",
                }),
            ))
            .await
            .unwrap();
        // 不 panic 即通过；状态码应在 {200, 502, 503} 之内
        assert!(
            matches!(resp.status, 200 | 502 | 503),
            "analyze-image 状态码应可预测: {}",
            resp.status
        );
        // body 必为 JSON 对象（含 description 或 error 字段）
        assert!(resp.body.is_object(), "响应应为 JSON 对象: {resp:?}");
    }

    #[tokio::test]
    async fn analyze_image_rejects_empty_prompt() {
        let h = LlmRouteHandler::with_demo();
        let resp = h
            .handle(post_req(
                "/api/v1/llm/analyze-image",
                serde_json::json!({
                    "image_base64": "abc",
                    "prompt": "",
                }),
            ))
            .await
            .unwrap();
        // prompt 为空：vLLM 不在线时 503（先探活）；在线时 analyze_image 校验 prompt 返回 502
        // 无论如何不 panic
        assert!(
            matches!(resp.status, 200 | 502 | 503),
            "空 prompt 不应 panic: {}",
            resp.status
        );
    }

    #[tokio::test]
    async fn analyze_image_rejects_bad_body() {
        // 缺少 image_base64 字段 → 反序列化失败 → 500（Internal error），不 panic
        let h = LlmRouteHandler::with_demo();
        let resp = h
            .handle(post_req(
                "/api/v1/llm/analyze-image",
                serde_json::json!({"prompt": "hi"}),
            ))
            .await;
        // serde 反序列化失败 → ApiGatewayError::Internal（Result::Err），unwrap 应 panic
        // 这里改为匹配 Err
        assert!(resp.is_err(), "缺 image_base64 应反序列化失败");
    }

    // ---- 轻量监控（GET /api/v1/llm/instances/:id/metrics）----

    /// env 是进程全局态：触碰 `NEXOS_LLM_METRICS_SIMULATE` 或经 handle() 打到
    /// metrics 端点的测试共用此锁串行，避免 set_var/remove_var 竞态导致 flaky。
    /// （用 tokio Mutex：async 测试里 guard 需跨 await 点持有。）
    static METRICS_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// 固定 Prometheus 样本文本（含注释行/标签/科学计数；e2e 为 Summary 形态）。
    fn sample_metrics_text(gen_total: f64, prompt_total: f64, success_total: f64) -> String {
        format!(
            "# HELP vllm:num_requests_running Number of requests currently running.\n\
             # TYPE vllm:num_requests_running gauge\n\
             vllm:num_requests_running{{model_name=\"qwen\"}} 2\n\
             # HELP vllm:num_requests_waiting Number of requests waiting.\n\
             # TYPE vllm:num_requests_waiting gauge\n\
             vllm:num_requests_waiting 0\n\
             # TYPE vllm:gpu_cache_usage_perc gauge\n\
             vllm:gpu_cache_usage_perc 4.2e-1\n\
             vllm:gpu_prefix_cache_hit_rate 8.7e-1\n\
             vllm:generation_tokens_total {gen_total}\n\
             vllm:prompt_tokens_total {prompt_total}\n\
             vllm:request_success_total {success_total}\n\
             vllm:e2e_request_latency_seconds_sum 1.6864\n\
             vllm:e2e_request_latency_seconds_count 2\n"
        )
    }

    /// 起一个极简 HTTP 服务回固定 Prometheus 文本（std TcpListener，真实模式
    /// 端到端联调用）。依次响应 `bodies` 各文本；请求次数少于文本数时线程阻塞
    /// 在 accept（随进程退出回收，不影响测试）。
    fn spawn_fake_metrics_server(bodies: Vec<String>) -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 失败");
        let port = listener.local_addr().expect("local_addr 失败").port();
        std::thread::spawn(move || {
            for body in bodies {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                // 读掉请求头（GET 无 body；读到多少算多少，即可响应）
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/plain; version=0.0.4\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });
        port
    }

    /// 找一个几乎必然关闭的本机端口（bind 临时 listener 拿空闲端口后立刻释放）。
    fn closed_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 失败");
        listener.local_addr().expect("local_addr 失败").port()
    }

    /// 直接向 handler 注入一个指定端口的实例（测试用，绕过 pick_free_port）。
    fn inject_instance(h: &LlmRouteHandler, id: &str, port: u16) {
        inject_instance_full(h, id, port, "running", None);
    }

    /// 注入实例（完整形态：指定状态与 served_model_name；gateway 聚合测试用）。
    fn inject_instance_full(
        h: &LlmRouteHandler,
        id: &str,
        port: u16,
        status: &str,
        served_model_name: Option<&str>,
    ) {
        let config = VllmConfig {
            port,
            served_model_name: served_model_name.map(String::from),
            ..Default::default()
        };
        let inst = ModelInstance {
            id: id.into(),
            name: format!("test-{id}"),
            model: "test/model".into(),
            source_type: "local".into(),
            port,
            status: status.into(),
            pid: None,
            env_name: None,
            launch_command: None,
            config,
            health: None,
            created_at: now_iso(),
            error: None,
        };
        h.instances.lock().expect("instances poisoned").push(inst);
    }

    /// 起一个极简 HTTP 服务回固定 JSON（vLLM /v1/models 假服务，gateway 探测
    /// 端到端联调用；手法同 spawn_fake_metrics_server）。依次响应 `bodies` 各
    /// 文本；多余请求阻塞在 accept（随进程退出回收）。
    fn spawn_fake_v1_models_server(bodies: Vec<String>) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 失败");
        spawn_fake_v1_models_server_on(listener, bodies)
    }

    /// [`spawn_fake_v1_models_server`] 的指定 listener 版：调用方先 bind 目标
    /// 端口（端口发现测试优先试 8123，被占则退任意空闲口——本机可能真有
    /// vLLM 在跑，测试不因环境端口占用而挂）。
    fn spawn_fake_v1_models_server_on(listener: std::net::TcpListener, bodies: Vec<String>) -> u16 {
        use std::io::{Read, Write};
        let port = listener.local_addr().expect("local_addr 失败").port();
        std::thread::spawn(move || {
            for body in bodies {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });
        port
    }

    /// vLLM /v1/models 响应样例（OpenAI 兼容：object=list，data[].id=模型名）。
    fn sample_v1_models_json(ids: &[&str]) -> String {
        let data: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| {
                serde_json::json!({
                    "id": id,
                    "object": "model",
                    "created": 1_700_000_000i64,
                    "owned_by": "vllm",
                    "max_model_len": 8192,
                })
            })
            .collect();
        serde_json::json!({ "object": "list", "data": data }).to_string()
    }

    #[test]
    fn parse_prometheus_value_skips_comments_labels_scientific_and_nan() {
        let text = "# HELP vllm:x help text\n# TYPE vllm:x gauge\n\
                    vllm:x{model_name=\"a\"} 4.2e-1\n\
                    vllm:x_suffix 9.9\n\
                    vllm:y NaN\n\
                    vllm:z 3 1700000000000\n";
        assert_eq!(
            parse_prometheus_value(text, "vllm:x"),
            Some(0.42),
            "应跳过注释/标签并以科学计数取值"
        );
        assert_eq!(parse_prometheus_value(text, "vllm:x_suffix"), Some(9.9));
        assert_eq!(parse_prometheus_value(text, "vllm:missing"), None);
        assert_eq!(parse_prometheus_value(text, "vllm:y"), None, "NaN 视为缺失");
        assert_eq!(
            parse_prometheus_value(text, "vllm:z"),
            Some(3.0),
            "容忍尾随时间戳"
        );
    }

    #[test]
    fn parse_vllm_metrics_extracts_all_metrics_from_full_text() {
        let raw = parse_vllm_metrics(&sample_metrics_text(1000.0, 3000.0, 12.0));
        assert_eq!(raw.num_requests_running, Some(2));
        assert_eq!(raw.num_requests_waiting, Some(0));
        assert_eq!(raw.gpu_cache_usage, Some(0.42));
        assert_eq!(
            raw.prefix_cache_hit_rate,
            Some(0.87),
            "新版缺失时应回退 gpu_prefix_cache_hit_rate"
        );
        assert_eq!(raw.generation_tokens_total, Some(1000.0));
        assert_eq!(raw.prompt_tokens_total, Some(3000.0));
        assert_eq!(raw.request_success_total, Some(12.0));
        assert!(
            (raw.e2e_latency_seconds.unwrap() - 0.8432).abs() < 1e-9,
            "e2e 应取 sum/count 均值"
        );
    }

    #[test]
    fn parse_vllm_metrics_tolerates_missing_or_garbage_text() {
        // 空文本：全 None，不 panic
        let raw = parse_vllm_metrics("");
        assert!(raw.num_requests_running.is_none());
        assert!(raw.num_requests_waiting.is_none());
        assert!(raw.gpu_cache_usage.is_none());
        assert!(raw.prefix_cache_hit_rate.is_none());
        assert!(raw.generation_tokens_total.is_none());
        assert!(raw.e2e_latency_seconds.is_none());
        // 非 Prometheus 文本 + e2e Gauge 直取形态
        let raw2 =
            parse_vllm_metrics("not prometheus at all\nvllm:e2e_request_latency_seconds 0.5\n");
        assert_eq!(raw2.num_requests_running, None);
        assert_eq!(
            raw2.e2e_latency_seconds,
            Some(0.5),
            "无 Summary 时按 Gauge 直取"
        );
    }

    #[test]
    fn metrics_cache_freshness_with_injected_time() {
        let now = std::time::Instant::now();
        assert!(metrics_cache_is_fresh(
            now,
            now + Duration::from_millis(4999)
        ));
        assert!(
            !metrics_cache_is_fresh(now, now + Duration::from_secs(5)),
            "恰好 TTL 到期"
        );
        assert!(
            !metrics_cache_is_fresh(now + Duration::from_secs(5), now),
            "时钟倒挂视为过期"
        );
    }

    #[test]
    fn counter_rates_from_two_samples_and_edge_cases() {
        // 差值/间隔秒
        assert_eq!(counter_rate(100.0, 130.0, 10.0), Some(3.0));
        // 间隔非法（<=0）
        assert_eq!(counter_rate(100.0, 130.0, 0.0), None);
        // 回绕（cur<prev，vLLM 重启过）
        assert_eq!(counter_rate(500.0, 100.0, 10.0), None);

        let t0 = std::time::Instant::now();
        let prev = CounterSample {
            at: t0,
            generation_tokens_total: Some(1000.0),
            prompt_tokens_total: Some(3000.0),
            request_success_total: Some(10.0),
        };
        let cur = CounterSample {
            at: t0 + Duration::from_secs(10),
            generation_tokens_total: Some(2000.0),
            prompt_tokens_total: Some(6000.0),
            request_success_total: Some(18.0),
        };
        let rates = compute_counter_rates(Some(prev), &cur);
        assert_eq!(rates.generation, Some(100.0));
        assert_eq!(rates.prompt, Some(300.0));
        assert_eq!(rates.success, Some(0.8));
        // 无历史 → 全 None
        let none = compute_counter_rates(None, &cur);
        assert!(none.generation.is_none());
        assert!(none.prompt.is_none());
        assert!(none.success.is_none());
        // 单侧缺失 → 仅该路 None
        let partial_prev = CounterSample {
            at: t0,
            generation_tokens_total: None,
            prompt_tokens_total: Some(3000.0),
            request_success_total: Some(10.0),
        };
        let r2 = compute_counter_rates(Some(partial_prev), &cur);
        assert!(r2.generation.is_none());
        assert_eq!(r2.prompt, Some(300.0));
    }

    #[tokio::test]
    async fn metrics_endpoint_missing_instance_returns_404() {
        let h = LlmRouteHandler::with_demo();
        let resp = h
            .handle(get_req("/api/v1/llm/instances/nope/metrics"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn metrics_real_mode_unreachable_returns_reachable_false_null_metrics() {
        let _guard = METRICS_ENV_LOCK.lock().await;
        std::env::remove_var(SIMULATE_ENV); // 纯真实模式：env 未设绝不模拟
        let h = LlmRouteHandler::with_demo();
        inject_instance(&h, "llm-closed", closed_port());
        let resp = h
            .handle(get_req("/api/v1/llm/instances/llm-closed/metrics"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "监控探测不是错误: {resp:?}");
        assert_eq!(resp.body["reachable"], false);
        assert_eq!(resp.body["simulated"], false, "env 未设绝不伪造");
        assert!(resp.body["metrics"].is_null());
        assert_eq!(resp.body["instance_id"], "llm-closed");
        assert!(resp.body["base_url"]
            .as_str()
            .unwrap()
            .starts_with("http://127.0.0.1:"));
        assert!(resp.body["collected_at"].is_string());
    }

    #[tokio::test]
    async fn metrics_real_mode_fake_http_server_cache_and_rate_deltas() {
        let _guard = METRICS_ENV_LOCK.lock().await;
        std::env::remove_var(SIMULATE_ENV);
        let port = spawn_fake_metrics_server(vec![
            sample_metrics_text(1000.0, 3000.0, 10.0),
            sample_metrics_text(2000.0, 6000.0, 18.0),
        ]);
        let h = LlmRouteHandler::with_demo();
        inject_instance(&h, "llm-fake", port);

        // 第一次抓取：值来自假 HTTP 服务；Counter 首次无历史 → 速率 null
        let now0 = std::time::Instant::now();
        let first = h.collect_metrics("llm-fake", port, now0).await;
        assert_eq!(first["reachable"], true);
        assert_eq!(first["simulated"], false);
        assert_eq!(first["metrics"]["num_requests_running"], 2);
        assert!((first["metrics"]["gpu_cache_usage"].as_f64().unwrap() - 0.42).abs() < 1e-9);
        assert!((first["metrics"]["prefix_cache_hit_rate"].as_f64().unwrap() - 0.87).abs() < 1e-9);
        assert!((first["metrics"]["e2e_latency_ms"].as_f64().unwrap() - 843.2).abs() < 1e-6);
        assert!(
            first["metrics"]["generation_tokens_per_sec"].is_null(),
            "首次无 Counter 历史"
        );

        // 缓存窗口内（+2s）：命中缓存，响应与第一次完全一致（未触发第二次抓取）
        let second = h
            .collect_metrics("llm-fake", port, now0 + Duration::from_secs(2))
            .await;
        assert_eq!(second, first, "5s 内应回缓存去抖");

        // 注入 +6s 过期：抓第二个样本 → 速率 = 差值/6s
        let third = h
            .collect_metrics("llm-fake", port, now0 + Duration::from_secs(6))
            .await;
        assert_eq!(third["reachable"], true);
        let gen = third["metrics"]["generation_tokens_per_sec"]
            .as_f64()
            .unwrap();
        assert!((gen - 1000.0 / 6.0).abs() < 0.01, "差值速率: {gen}");
        let prompt = third["metrics"]["prompt_tokens_per_sec"].as_f64().unwrap();
        assert!((prompt - 3000.0 / 6.0).abs() < 0.01);
        let succ = third["metrics"]["requests_success_per_sec"]
            .as_f64()
            .unwrap();
        assert!((succ - 8.0 / 6.0).abs() < 0.001);
    }

    #[tokio::test]
    async fn metrics_simulate_env_unreachable_returns_sane_synthetic_data() {
        let _guard = METRICS_ENV_LOCK.lock().await;
        std::env::set_var(SIMULATE_ENV, "1");
        let h = LlmRouteHandler::with_demo();
        inject_instance(&h, "llm-sim", closed_port());
        // 经完整路由分发（公开 GET）
        let resp = h
            .handle(get_req("/api/v1/llm/instances/llm-sim/metrics"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["simulated"], true, "真实不通时合成");
        assert_eq!(resp.body["reachable"], false, "真实端口确实不通");
        let m = &resp.body["metrics"];
        assert!(m.is_object(), "合成数据非 null: {m}");
        let running = m["num_requests_running"].as_u64().unwrap();
        assert!(running <= 8, "running 0-8 整数");
        assert!((0.0..=0.9).contains(&m["gpu_cache_usage"].as_f64().unwrap()));
        assert!((0.0..=1.0).contains(&m["prefix_cache_hit_rate"].as_f64().unwrap()));
        assert!((80.0..=3000.0).contains(&m["e2e_latency_ms"].as_f64().unwrap()));
        for k in [
            "generation_tokens_per_sec",
            "prompt_tokens_per_sec",
            "requests_success_per_sec",
        ] {
            assert!(m[k].as_f64().unwrap() >= 0.0, "{k} 应非负");
        }
        std::env::remove_var(SIMULATE_ENV);
    }

    #[tokio::test]
    async fn metrics_simulate_env_reachable_still_uses_real_data() {
        let _guard = METRICS_ENV_LOCK.lock().await;
        std::env::set_var(SIMULATE_ENV, "1");
        let port = spawn_fake_metrics_server(vec![sample_metrics_text(1000.0, 3000.0, 10.0)]);
        let h = LlmRouteHandler::with_demo();
        inject_instance(&h, "llm-sim-real", port);
        let resp = h
            .handle(get_req("/api/v1/llm/instances/llm-sim-real/metrics"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["simulated"], false, "端口通则用真实");
        assert_eq!(resp.body["reachable"], true);
        assert_eq!(resp.body["metrics"]["num_requests_running"], 2);
        std::env::remove_var(SIMULATE_ENV);
    }

    #[test]
    fn synthetic_metrics_deterministic_physically_sane() {
        // 确定性：同一实例同一时刻输出恒定
        let a = synthetic_metrics("llm-1", 1000.0);
        let b = synthetic_metrics("llm-1", 1000.0);
        assert_eq!(a.num_requests_running, b.num_requests_running);
        assert_eq!(a.gpu_cache_usage, b.gpu_cache_usage);
        // 不同实例相位错开（至少一项不同）
        let c = synthetic_metrics("llm-2", 1000.0);
        assert!(
            a.num_requests_running != c.num_requests_running
                || (a.gpu_cache_usage.unwrap() - c.gpu_cache_usage.unwrap()).abs() > 1e-9,
            "不同实例波形应错开"
        );
        // 采样 300 个时刻：范围合法 + waiting 随 running 高而增长 + 速率与 latency 负相关
        let mut latencies = Vec::new();
        let mut gens = Vec::new();
        let mut high_load_waiting_min = u64::MAX;
        let mut low_load_waiting_max = 0u64;
        for i in 0..300 {
            let t = 1000.0 + f64::from(i) * 3.0;
            let m = synthetic_metrics("llm-x", t);
            let running = m.num_requests_running.unwrap();
            let waiting = m.num_requests_waiting.unwrap();
            assert!(running <= 8, "running 0-8 整数");
            assert!((0.0..=0.9).contains(&m.gpu_cache_usage.unwrap()));
            assert!((0.0..=1.0).contains(&m.prefix_cache_hit_rate.unwrap()));
            assert!((80.0..=3000.0).contains(&m.e2e_latency_ms.unwrap()));
            assert!(m.generation_tokens_per_sec.unwrap() >= 0.0);
            assert!(m.prompt_tokens_per_sec.unwrap() >= 0.0);
            assert!(m.requests_success_per_sec.unwrap() >= 0.0);
            if running >= 7 {
                high_load_waiting_min = high_load_waiting_min.min(waiting);
            }
            if running <= 2 {
                low_load_waiting_max = low_load_waiting_max.max(waiting);
            }
            latencies.push(m.e2e_latency_ms.unwrap());
            gens.push(m.generation_tokens_per_sec.unwrap());
        }
        assert!(
            high_load_waiting_min >= 2,
            "高负载（running>=7）时 waiting 应显著排队: {high_load_waiting_min}"
        );
        assert!(
            low_load_waiting_max <= 1,
            "低负载（running<=2）时 waiting 应基本为 0: {low_load_waiting_max}"
        );
        // 协方差 < 0：token 速率与 latency 负相关
        let n = f64::from(latencies.len() as u32);
        let ml = latencies.iter().sum::<f64>() / n;
        let mg = gens.iter().sum::<f64>() / n;
        let cov = latencies
            .iter()
            .zip(&gens)
            .map(|(l, g)| (l - ml) * (g - mg))
            .sum::<f64>()
            / n;
        assert!(cov < 0.0, "速率应与 latency 负相关: cov={cov}");
    }

    // ---- vLLM Recipes 导入（烘焙代理：catalog / recipe / 缓存 / 降级）----

    /// 起一个极简 JSON 服务（TcpListener，同 metrics 假服务手法）：依次响应
    /// `bodies` 各文本（content-type application/json）；文本耗尽后线程阻塞在
    /// accept——若被测代码真发了多余请求会挂到 15s 超时，据此证明缓存命中。
    fn spawn_fake_json_server(bodies: Vec<String>) -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 失败");
        let port = listener.local_addr().expect("local_addr 失败").port();
        std::thread::spawn(move || {
            for body in bodies {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });
        port
    }

    /// 上游 models.json 单条目样例（与真实站点形状一致：无 date_updated 字段）。
    fn sample_catalog_json() -> String {
        serde_json::json!([
            {
                "hf_id": "meta-llama/Llama-3.1-8B",
                "title": "Llama-3.1-8B",
                "provider": "Meta",
                "url": "/meta-llama/Llama-3.1-8B",
                "json": "/meta-llama/Llama-3.1-8B.json"
            },
            {
                "hf_id": "Qwen/Qwen2.5-7B-Instruct",
                "provider": "Qwen"
            }
        ])
        .to_string()
    }

    /// 上游单配方样例（与真实站点形状一致的裁剪版）。
    fn sample_recipe_json() -> String {
        serde_json::json!({
            "hf_id": "meta-llama/Llama-3.1-8B",
            "meta": {
                "title": "Llama-3.1-8B",
                "provider": "Meta",
                "description": "Meta Llama 3.1 8B dense base model.",
                "date_added": "2026-08-19",
                "date_updated": "2026-08-19",
                "difficulty": "beginner",
                "tasks": ["text"]
            },
            "recommended_command": {
                "hardware": "h200",
                "strategy": "single_node_tp",
                "docker_image": "vllm/vllm-openai:latest",
                "command": "vllm serve meta-llama/Llama-3.1-8B --tensor-parallel-size 1",
                "docker_command": "docker run --gpus all vllm/vllm-openai:latest"
            },
            "variants": {
                "w8a8": { "precision": "int8", "vram_minimum_gb": 8 }
            },
            "guide": "# Llama-3.1-8B deploy guide"
        })
        .to_string()
    }

    #[tokio::test]
    async fn recipes_catalog_maps_compact_items_and_caches_until_manual_refresh() {
        // mock 备 2 份 body：① 首次拉取 ② refresh=1 强制重拉的新目录。
        // 中间穿插一次普通 GET——若它真发外呼请求会挂在 accept 上（15s 超时
        // → 502），据此证明常驻缓存命中零外呼。
        let port = spawn_fake_json_server(vec![
            sample_catalog_json(),
            serde_json::json!([
                {
                    "hf_id": "Qwen/Qwen3-8B",
                    "title": "Qwen3-8B",
                    "provider": "Qwen"
                }
            ])
            .to_string(),
        ]);
        let mut h = LlmRouteHandler::with_empty();
        h.recipes_base = format!("http://127.0.0.1:{port}");

        // 首次：外呼拉取 + 精简映射（from_cache=false）
        let first = h
            .handle(get_req("/api/v1/llm/recipes/catalog"))
            .await
            .unwrap();
        assert_eq!(first.status, 200, "catalog body: {first:?}");
        assert_eq!(first.body["from_cache"], false, "首次应为真实拉取");
        assert!(
            first.body["cached_at"].as_str().is_some_and(|s| !s.is_empty()),
            "信封应带 cached_at（RFC3339）"
        );
        let arr = first.body["items"].as_array().expect("信封 items 应为数组");
        assert_eq!(arr.len(), 2, "精简目录保留全部条目");
        assert_eq!(arr[0]["hf_id"], "meta-llama/Llama-3.1-8B");
        assert_eq!(arr[0]["title"], "Llama-3.1-8B");
        assert_eq!(arr[0]["provider"], "Meta");
        assert!(arr[0]["date_updated"].is_null(), "上游未提供 → null");
        assert_eq!(arr[1]["hf_id"], "Qwen/Qwen2.5-7B-Instruct");
        // 缺 title 回退 hf_id
        assert_eq!(arr[1]["title"], "Qwen/Qwen2.5-7B-Instruct");

        // 第二次（无 refresh）：常驻缓存命中——秒回同内容、零外呼
        let second = h
            .handle(get_req("/api/v1/llm/recipes/catalog"))
            .await
            .unwrap();
        assert_eq!(second.status, 200, "常驻缓存期内应命中而非打上游");
        assert_eq!(second.body["from_cache"], true, "应标记缓存命中");
        assert_eq!(second.body["items"], first.body["items"], "内容与首次一致");

        // refresh=1：强制外呼重拉 → 新目录替换缓存
        let refreshed = h
            .handle(get_req("/api/v1/llm/recipes/catalog?refresh=1"))
            .await
            .unwrap();
        assert_eq!(refreshed.status, 200, "refresh body: {refreshed:?}");
        assert_eq!(refreshed.body["from_cache"], false, "refresh 应真外呼");
        let rarr = refreshed.body["items"].as_array().expect("items 数组");
        assert_eq!(rarr.len(), 1, "refresh 后应为 mock 第二份新目录");
        assert_eq!(rarr[0]["hf_id"], "Qwen/Qwen3-8B");

        // refresh 之后的普通 GET：回新缓存
        let after = h
            .handle(get_req("/api/v1/llm/recipes/catalog"))
            .await
            .unwrap();
        assert_eq!(after.status, 200);
        assert_eq!(after.body["from_cache"], true);
        assert_eq!(after.body["items"], refreshed.body["items"], "缓存已更新");
    }

    #[tokio::test]
    async fn recipes_catalog_refresh_failure_keeps_cached_copy() {
        // 先用活 mock 拉一份目录入缓存；再把上游指向死端口 refresh → 502；
        // 随后普通 GET 仍回旧缓存（刷新失败不清缓存）。
        let port = spawn_fake_json_server(vec![sample_catalog_json()]);
        let mut h = LlmRouteHandler::with_empty();
        h.recipes_base = format!("http://127.0.0.1:{port}");
        let first = h
            .handle(get_req("/api/v1/llm/recipes/catalog"))
            .await
            .unwrap();
        assert_eq!(first.status, 200);

        h.recipes_base = format!("http://127.0.0.1:{}", closed_port());
        let failed = h
            .handle(get_req("/api/v1/llm/recipes/catalog?refresh=1"))
            .await
            .unwrap();
        assert_eq!(failed.status, 502, "refresh 上游不可达应 502: {failed:?}");
        assert!(
            failed.body["error"]
                .as_str()
                .unwrap()
                .contains("拉取失败"),
            "错误应带原因"
        );

        let fallback = h
            .handle(get_req("/api/v1/llm/recipes/catalog"))
            .await
            .unwrap();
        assert_eq!(fallback.status, 200, "刷新失败后旧缓存应保留");
        assert_eq!(fallback.body["from_cache"], true);
        assert_eq!(fallback.body["items"], first.body["items"]);
    }

    #[tokio::test]
    async fn recipes_catalog_refresh_clears_recipe_detail_cache() {
        // 目录 refresh 成功 → 单配方详情缓存一并清空（跟随目录刷新）：
        // 刷新后再 GET 同一配方必须重新外呼拿新内容。
        let mutated_recipe = {
            let mut v: serde_json::Value =
                serde_json::from_str(&sample_recipe_json()).unwrap();
            v["guide"] = "# refreshed guide".into();
            v.to_string()
        };
        let port = spawn_fake_json_server(vec![
            sample_catalog_json(),     // ① 目录首拉
            sample_recipe_json(),      // ② 配方首拉（入缓存）
            sample_catalog_json(),     // ③ 目录 refresh=1 强制重拉
            mutated_recipe,            // ④ 配方缓存被清 → 重新外呼
        ]);
        let mut h = LlmRouteHandler::with_empty();
        h.recipes_base = format!("http://127.0.0.1:{port}");

        let cat = h
            .handle(get_req("/api/v1/llm/recipes/catalog"))
            .await
            .unwrap();
        assert_eq!(cat.status, 200);
        let recipe_path = "/api/v1/llm/recipes/recipe?hf_id=meta-llama/Llama-3.1-8B";
        let r1 = h.handle(get_req(recipe_path)).await.unwrap();
        assert_eq!(r1.status, 200);
        assert_eq!(r1.body["guide"], "# Llama-3.1-8B deploy guide");

        // 目录 refresh（成功）→ 详情缓存清空
        let re = h
            .handle(get_req("/api/v1/llm/recipes/catalog?refresh=1"))
            .await
            .unwrap();
        assert_eq!(re.status, 200);
        assert_eq!(re.body["from_cache"], false);

        // 再取同配方：必须重新外呼（mock 第 4 份 body），拿到新 guide
        let r2 = h.handle(get_req(recipe_path)).await.unwrap();
        assert_eq!(r2.status, 200);
        assert_eq!(
            r2.body["guide"], "# refreshed guide",
            "目录刷新后详情缓存应失效重拉"
        );
    }

    #[tokio::test]
    async fn recipes_recipe_passthrough_shape_and_persistent_cache() {
        let port = spawn_fake_json_server(vec![sample_recipe_json()]);
        let mut h = LlmRouteHandler::with_empty();
        h.recipes_base = format!("http://127.0.0.1:{port}");

        let path = "/api/v1/llm/recipes/recipe?hf_id=meta-llama/Llama-3.1-8B";
        let resp = h.handle(get_req(path)).await.unwrap();
        assert_eq!(resp.status, 200, "recipe body: {resp:?}");
        // 原样透传：上游字段全保留，不改名不裁剪
        assert_eq!(resp.body["hf_id"], "meta-llama/Llama-3.1-8B");
        assert_eq!(resp.body["meta"]["difficulty"], "beginner");
        assert_eq!(resp.body["meta"]["tasks"][0], "text");
        assert_eq!(
            resp.body["recommended_command"]["command"],
            "vllm serve meta-llama/Llama-3.1-8B --tensor-parallel-size 1"
        );
        assert_eq!(
            resp.body["recommended_command"]["docker_image"],
            "vllm/vllm-openai:latest"
        );
        assert_eq!(resp.body["variants"]["w8a8"]["precision"], "int8");
        assert_eq!(resp.body["variants"]["w8a8"]["vram_minimum_gb"], 8);
        assert_eq!(resp.body["guide"], "# Llama-3.1-8B deploy guide");

        // 常驻缓存：同 hf_id 第二次不打上游（mock 无第二份 body）
        let again = h.handle(get_req(path)).await.unwrap();
        assert_eq!(again.status, 200);
        assert_eq!(again.body, resp.body, "常驻缓存期内应回缓存");
    }

    #[tokio::test]
    async fn recipes_upstream_failure_returns_502_with_reason() {
        // 指向必然关闭的端口：连接拒绝立即失败（不触发 15s 超时）
        let mut h = LlmRouteHandler::with_empty();
        h.recipes_base = format!("http://127.0.0.1:{}", closed_port());
        let resp = h
            .handle(get_req("/api/v1/llm/recipes/catalog"))
            .await
            .unwrap();
        assert_eq!(resp.status, 502, "上游不可达应 502: {resp:?}");
        let err = resp.body["error"].as_str().unwrap();
        assert!(err.contains("拉取失败"), "错误应带原因: {err}");

        // recipe 同款降级
        let mut h2 = LlmRouteHandler::with_empty();
        h2.recipes_base = format!("http://127.0.0.1:{}", closed_port());
        let resp = h2
            .handle(get_req(
                "/api/v1/llm/recipes/recipe?hf_id=meta-llama/Llama-3.1-8B",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 502);
        assert!(resp.body["error"].as_str().unwrap().contains("拉取失败"));
    }

    #[tokio::test]
    async fn recipes_recipe_rejects_missing_or_unsafe_hf_id() {
        let mut h = LlmRouteHandler::with_empty();
        // 兜底指向本机死端口：万一有漏网 case 也不得打真实外网（测试红线）
        h.recipes_base = format!("http://127.0.0.1:{}", closed_port());
        // 缺参数 → 400
        let resp = h
            .handle(get_req("/api/v1/llm/recipes/recipe"))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "缺 hf_id 应 400: {resp:?}");
        // 空值 → 400
        let resp = h
            .handle(get_req("/api/v1/llm/recipes/recipe?hf_id="))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // 路径穿越 / 编码穿越 / 片段符 / 空白 / 绝对路径 → 400（不到上游）
        //（`?` 形态经 query 解析即被截断，纯函数边界测试已单测 valid_recipe_hf_id）
        for bad in ["..", "a/../b", "%2e%2e/x", "x%23frag", "a%20b", "/abs"] {
            let path = format!("/api/v1/llm/recipes/recipe?hf_id={bad}");
            let resp = h.handle(get_req(&path)).await.unwrap();
            assert_eq!(resp.status, 400, "非法 hf_id {bad:?} 应 400: {resp:?}");
        }
    }

    #[test]
    fn valid_recipe_hf_id_boundaries() {
        assert!(valid_recipe_hf_id("meta-llama/Llama-3.1-8B"));
        assert!(
            valid_recipe_hf_id("  Qwen/Qwen2.5-7B  "),
            "首尾空白 trim 后合法"
        );
        assert!(!valid_recipe_hf_id(""));
        assert!(!valid_recipe_hf_id("   "));
        assert!(!valid_recipe_hf_id("/abs"));
        assert!(!valid_recipe_hf_id("a/../b"));
        assert!(!valid_recipe_hf_id("x?y=1"));
        assert!(!valid_recipe_hf_id("x#frag"));
        assert!(!valid_recipe_hf_id("a b"));
    }

    #[test]
    fn query_param_decodes_percent_and_plus() {
        assert_eq!(
            query_param(
                "/api/v1/llm/recipes/recipe?hf_id=meta-llama%2FLlama-3.1-8B",
                "hf_id"
            ),
            Some("meta-llama/Llama-3.1-8B".to_string()),
            "%2F 解码（前端 encodeURIComponent 形态）"
        );
        assert_eq!(
            query_param("/p?hf_id=a+b", "hf_id"),
            Some("a b".to_string())
        );
        assert_eq!(query_param("/p?other=1", "hf_id"), None);
        assert_eq!(query_param("/p", "hf_id"), None);
        // 前段路径含同名 key 不影响
        assert_eq!(
            query_param("/hf_id=decoy?hf_id=real", "hf_id"),
            Some("real".to_string())
        );
    }

    // ---- API 网关聚合（gateway/models + gateway/health）----
    //
    // 全部用 with_empty + 显式注入（不用 with_demo：llm-1 在 8000 端口
    // "running"，本机可能真有 vLLM 监听，会引入环境依赖的非确定性）。

    #[tokio::test]
    async fn gateway_models_empty_when_no_instances() {
        let h = LlmRouteHandler::with_empty();
        let resp = h
            .handle(get_req("/api/v1/llm/gateway/models"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert!(
            resp.body["gateway_visible"].as_array().unwrap().is_empty(),
            "空表 → 无可见模型: {resp:?}"
        );
        assert!(
            resp.body["unreachable"].as_array().unwrap().is_empty(),
            "空表 → 无不可达: {resp:?}"
        );
    }

    #[tokio::test]
    async fn gateway_models_running_reachable_lists_raw_models_and_ids() {
        let port = spawn_fake_v1_models_server(vec![sample_v1_models_json(&["qwen2.5-7b"])]);
        let h = LlmRouteHandler::with_empty();
        inject_instance_full(&h, "llm-gw1", port, "running", Some("qwen2.5-7b"));
        let resp = h
            .handle(get_req("/api/v1/llm/gateway/models"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "body: {resp:?}");
        let visible = resp.body["gateway_visible"].as_array().unwrap();
        assert_eq!(visible.len(), 1, "reachable 实例应可见: {resp:?}");
        let entry = &visible[0];
        assert_eq!(entry["instance_id"], "llm-gw1");
        assert_eq!(entry["name"], "test-llm-gw1");
        assert_eq!(entry["served_model_name"], "qwen2.5-7b");
        assert_eq!(entry["port"], port);
        assert_eq!(entry["alive"], true);
        // 原始模型对象原样透传 + 解析出的 data[].id 列表
        assert_eq!(entry["models"][0]["id"], "qwen2.5-7b");
        assert_eq!(entry["models"][0]["object"], "model");
        assert_eq!(entry["models"][0]["owned_by"], "vllm");
        assert_eq!(
            entry["model_ids"],
            serde_json::json!(["qwen2.5-7b"]),
            "alive 实例额外解析 data[].id"
        );
        assert!(resp.body["unreachable"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn gateway_models_running_unreachable_lists_reason() {
        let dead_port = closed_port();
        let h = LlmRouteHandler::with_empty();
        inject_instance_full(&h, "llm-dead", dead_port, "running", None);
        let resp = h
            .handle(get_req("/api/v1/llm/gateway/models"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "探测失败不是错误，200 语义: {resp:?}");
        assert!(
            resp.body["gateway_visible"].as_array().unwrap().is_empty(),
            "不可达绝不进可见组（绝不凭 status 伪造）: {resp:?}"
        );
        let dead = resp.body["unreachable"].as_array().unwrap();
        assert_eq!(dead.len(), 1, "body: {resp:?}");
        assert_eq!(dead[0]["instance_id"], "llm-dead");
        assert_eq!(dead[0]["name"], "test-llm-dead");
        assert_eq!(dead[0]["port"], dead_port);
        assert!(
            dead[0]["reason"].as_str().unwrap().contains("/v1/models"),
            "reason 带可排查前缀: {resp:?}"
        );
    }

    #[tokio::test]
    async fn gateway_models_skips_non_running_instances() {
        let h = LlmRouteHandler::with_empty();
        // stopped / starting / error 一律不探测、两组都不进
        inject_instance_full(&h, "llm-stopped", closed_port(), "stopped", None);
        inject_instance_full(&h, "llm-starting", closed_port(), "starting", None);
        inject_instance_full(&h, "llm-err", closed_port(), "error", None);
        let resp = h
            .handle(get_req("/api/v1/llm/gateway/models"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body["gateway_visible"].as_array().unwrap().is_empty());
        assert!(
            resp.body["unreachable"].as_array().unwrap().is_empty(),
            "非 running 实例未探测，无话可说: {resp:?}"
        );
    }

    #[tokio::test]
    async fn gateway_health_counts_reachable_unreachable_and_gpu_total_mem() {
        let port = spawn_fake_v1_models_server(vec![sample_v1_models_json(&["demo-model"])]);
        let h = LlmRouteHandler::with_empty();
        inject_instance_full(&h, "llm-ok", port, "running", None);
        inject_instance_full(&h, "llm-dead", closed_port(), "running", None);
        inject_instance_full(&h, "llm-stopped", closed_port(), "stopped", None);
        let resp = h
            .handle(get_req("/api/v1/llm/gateway/health"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "body: {resp:?}");
        assert_eq!(resp.body["running_total"], 2, "只有 running 进探测口径");
        assert_eq!(resp.body["reachable"], 1);
        assert_eq!(resp.body["unreachable"], 1);
        // GPU 字段来自 detect_gpu（环境相关，只验型别与口径，不硬编码数值）
        assert!(resp.body["gpu_available"].is_boolean());
        assert!(resp.body["gpu_memory_total_mib"].is_u64());
        assert!(resp.body["gpu_unified_memory"].is_boolean());
        assert!(
            ["cuda", "rocm", "none"].contains(&resp.body["gpu_backend"].as_str().unwrap_or("")),
            "backend 枚举: {resp:?}"
        );
    }

    // ---- 端口扫描发现 + 实例 status 健康修正（2026-08-30 真实化加固）----

    /// 起 /v1/models 假服务：**只绑临时端口**（127.0.0.1:0）。
    ///
    /// 2026-08-31 修正：不再优先绑生产实例基点 8123——挂死的测试二进制会把
    /// 8123 连同假模型列表一起泄漏到生产（真 vLLM 绑不上 + 健康探测误报 +
    /// 网关发现假条目，实测踩过）。调用方一律把返回端口注入
    /// `discovery_ports`，无需固定 8123，语义不变。
    ///
    /// 备多份响应体：并行测试创建的实例会落在相邻端口并被 /instances 修正
    /// 探测打到，单个响应体会被陌生连接消耗掉，导致本测试自己的扫描探测
    /// 连接被拒（flaky）。
    fn spawn_fake_vllm_prefer_8123(model_ids: &[&str]) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 失败");
        let bodies: Vec<String> = (0..32).map(|_| sample_v1_models_json(model_ids)).collect();
        spawn_fake_v1_models_server_on(listener, bodies)
    }

    /// 直查 handler 内存库的实例 status 行（修正是否落库）。
    fn db_instance_status(h: &LlmRouteHandler, id: &str) -> String {
        let conn = h.db.lock().unwrap();
        conn.query_row(
            "SELECT status FROM llm_instances WHERE id=?",
            params![id],
            |r| r.get::<_, String>(0),
        )
        .unwrap_or_else(|e| panic!("查 {id} status 失败: {e}"))
    }

    #[tokio::test]
    async fn gateway_models_discovers_vllm_outside_instance_table() {
        // 根因1：vLLM 活着但不在实例表（手动启动）——此前可路由模型恒空，
        // 端口扫描发现后以 discovered 条目出现
        let port = spawn_fake_vllm_prefer_8123(&["qwen3-vl-8b", "qwen2.5-7b"]);
        let mut h = LlmRouteHandler::with_empty();
        h.discovery_ports = vec![port];
        let resp = h
            .handle(get_req("/api/v1/llm/gateway/models"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "body: {resp:?}");
        let visible = resp.body["gateway_visible"].as_array().unwrap();
        assert_eq!(visible.len(), 1, "扫描命中应有 1 条: {resp:?}");
        let e = &visible[0];
        assert!(e["instance_id"].is_null(), "发现条目不在实例表: {e:?}");
        assert_eq!(e["name"], format!("发现的 vLLM :{port}"));
        assert_eq!(e["port"], port);
        assert_eq!(e["alive"], true);
        assert_eq!(e["discovered"], true, "扫描发现条目 discovered=true");
        assert_eq!(
            e["model_ids"],
            serde_json::json!(["qwen3-vl-8b", "qwen2.5-7b"]),
            "model_ids 取自 /v1/models data[].id: {e:?}"
        );
        assert_eq!(e["models"][0]["id"], "qwen3-vl-8b", "原始模型对象透传");
        assert!(resp.body["unreachable"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn gateway_models_running_dead_port_demotes_status_and_persists() {
        // 根因1 另一面：DB 声称 running 但端口已死 → unreachable + status 回落
        // stopped（内存 + DB），不再「明明死了还显示 running」
        let dead = closed_port();
        let h = LlmRouteHandler::with_empty();
        inject_instance_full(&h, "llm-zombie", dead, "running", None);
        let resp = h
            .handle(get_req("/api/v1/llm/gateway/models"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "探测失败不是错误: {resp:?}");
        let un = resp.body["unreachable"].as_array().unwrap();
        assert_eq!(un.len(), 1, "body: {resp:?}");
        assert_eq!(un[0]["instance_id"], "llm-zombie");
        assert!(
            resp.body["gateway_visible"].as_array().unwrap().is_empty(),
            "不可达绝不进可见组: {resp:?}"
        );
        assert_eq!(
            h.instances_snapshot()[0].status,
            "stopped",
            "内存态应回落 stopped"
        );
        assert_eq!(
            db_instance_status(&h, "llm-zombie"),
            "stopped",
            "回落应落库（inject 未落表，修正 persist 补写）"
        );
    }

    #[tokio::test]
    async fn instances_list_corrects_stopped_to_running_when_port_alive() {
        // 根因1 落点：status 已回落 stopped 但 vLLM 还活着 → 列表返回前修正 running
        let port = spawn_fake_v1_models_server(vec![sample_v1_models_json(&["my-alias"])]);
        let h = LlmRouteHandler::with_empty();
        inject_instance_full(&h, "llm-alive", port, "stopped", Some("my-alias"));
        let resp = h.handle(get_req("/api/v1/llm/instances")).await.unwrap();
        assert_eq!(resp.status, 200, "body: {resp:?}");
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(
            arr[0]["status"], "running",
            "stopped 实例端口活且 served_model_name 匹配 → 修正 running"
        );
        assert!(
            arr[0]["pid"].is_null(),
            "修正只动 status，pid 不凭探测反推: {arr:?}"
        );
        assert_eq!(db_instance_status(&h, "llm-alive"), "running", "修正应落库");
    }

    #[tokio::test]
    async fn instances_list_corrects_running_to_stopped_when_port_dead() {
        let h = LlmRouteHandler::with_empty();
        inject_instance_full(&h, "llm-dying", closed_port(), "running", None);
        let resp = h.handle(get_req("/api/v1/llm/instances")).await.unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().unwrap();
        assert_eq!(
            arr[0]["status"], "stopped",
            "running 实例端口死 → 回落 stopped"
        );
        assert_eq!(db_instance_status(&h, "llm-dying"), "stopped", "回落应落库");
    }

    #[tokio::test]
    async fn instances_list_keeps_stopped_when_served_model_name_mismatches() {
        // 端口上应答的是别的服务（模型名不匹配）→ 不误标 running
        let port = spawn_fake_v1_models_server(vec![sample_v1_models_json(&["other-model"])]);
        let h = LlmRouteHandler::with_empty();
        inject_instance_full(&h, "llm-mismatch", port, "stopped", Some("my-alias"));
        let resp = h.handle(get_req("/api/v1/llm/instances")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(
            resp.body[0]["status"], "stopped",
            "模型名不匹配不得修正 running（防误纳管端口上的陌生服务）"
        );
    }

    #[tokio::test]
    async fn gateway_models_dedupes_scan_port_already_held_by_instance() {
        // 实例表端口与扫描段重叠 → 只报实例条目一次，不追加 discovered 重复条目
        // （备 2 份响应体：若实现退化成二次探测，第二条也能应答并使 len 断言失败）
        let port = spawn_fake_v1_models_server(vec![
            sample_v1_models_json(&["dup-model"]),
            sample_v1_models_json(&["dup-model"]),
        ]);
        let mut h = LlmRouteHandler::with_empty();
        h.discovery_ports = vec![port];
        inject_instance_full(&h, "llm-held", port, "running", None);
        let resp = h
            .handle(get_req("/api/v1/llm/gateway/models"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let visible = resp.body["gateway_visible"].as_array().unwrap();
        assert_eq!(visible.len(), 1, "重叠端口只报一次: {resp:?}");
        assert_eq!(visible[0]["instance_id"], "llm-held", "报的是实例条目");
        assert_eq!(
            visible[0]["discovered"], false,
            "实例表条目 discovered=false"
        );
        assert!(resp.body["unreachable"].as_array().unwrap().is_empty());
    }
}
