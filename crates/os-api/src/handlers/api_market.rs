//! `ApiMarketRouteHandler` —— API 大厅（推理服务市场）REST API
//! （设计文档 `docs/API_MARKET.md`）。
//!
//! 定位：把本机/本节点对外提供的**推理服务端点**（LLM chat/completions、生图等）
//! 挂牌成「商品」——消费者在大厅**查价格、看服务器配置、看实时负载**，再拿着
//! `endpoint_url` 直连消费。对标 OpenRouter / One API 的渠道市场页。
//!
//! # 数据模型（SQLite `api_market` 表，复用 im.rs 的建库模式）
//!
//! | 字段 | 说明 |
//! |------|------|
//! | id | UUID（主键）|
//! | api_name | 商品名（如 `qwen3.5-9b chat`；同 pubkey 重复发布=刷新）|
//! | description | 描述 |
//! | endpoint_url | 消费端点（如 `http://host:8080/api/v1/gateway/v1/chat/completions`）|
//! | publisher_pubkey | 发布者身份（`0x`+66hex 压缩 secp256k1，token 反查，body 自报忽略）|
//! | publisher_display | EVM 派生展示名（`0x`+40hex）|
//! | server_config | 服务器配置 JSON（gpu/cpu/ram/model_name/…，本地探测+body 覆盖）|
//! | pricing | 计价 JSON（mode/price_per_1k_tokens/currency/note）|
//! | metrics_url | 发布者的负载监控端点（无新鲜心跳时代拉）|
//! | tags | 标签（JSON 数组）|
//! | status | 恒 `active`（一期无下线态；DELETE 直接删行）|
//! | created_at | 首次挂牌时间（刷新保留）|
//! | heartbeat_at | 最近一次心跳（RFC3339；60s 内=新鲜）|
//! | load | 最近一次心跳上报的负载指标（规范化 6 键 JSON）|
//! | download_count | 消费计数（刷新保留；本期调用闭环未接线，预置字段）|
//!
//! # 发布者身份（用户定稿：区块链公钥 = 唯一通道，无 admin 回落）
//!
//! 与 IM/NexHub 同款挑战-签名链上身份，但**更严**：本 handler 不设自己的
//! challenge/verify 端点，直接共享 nexhub-lobby 的 [`ChainAuth`] 实例
//! （main.rs 装配 `with_chain_auth(nexhub_chain_auth.clone())`）——即
//! `POST /api/v1/nexhub/auth/challenge|verify` 签发的 token 在此直接可用
//! （401 文案即引导该处）。**没有 admin token 回落**：`NEXOS_ADMIN_TOKEN`
//! 在 publish/delete/heartbeat 上一律 401/403（与 nexhub 的回落语义刻意
//! 不同——市场里的发布者必须是可以签名验证的链上身份，防止平台侧代发）。
//!
//! # 服务器配置（本地探测 + body 覆盖）
//!
//! `server_config` 缺省字段自动探测**本机**硬件（发布动作发生在服务所在节点）：
//! - `gpus`/`gpu_count`：`nvidia-smi --query-gpu=index,name,memory.total
//!   --format=csv,noheader,nounits` 逐行解析——每卡一条目 `{index,name,vram_mb}`
//!   （同型号多卡=多条目，index 区分；`gpu_count`=卡数）。无 nvidia-smi / 无卡 →
//!   `gpus` 空 + `gpu_count` 0，**静默不报错**（CPU-only 节点可发布）；
//!   **统一内存架构**（2026-09-03，DGX Spark GB10 实测）：GB10/Jetson 类超芯片
//!   无独立显存，csv 显存列报 `[N/A]`（`0, NVIDIA GB10, [N/A]`）——name 可解析
//!   即算有卡，`vram_mb=null` + `unified_memory=true`，`unified_vram_mb` 回退
//!   `/proc/meminfo` MemTotal（CPU/GPU 共享 LPDDR5x 池，大厅展示
//!   「GB10 · 统一内存 121.7 GB」；与 `ram_gb` 同源同池）；
//! - `gpu_name`/`gpu_vram_mb`：首卡（`gpus[0]`）镜像——向后兼容保留的旧字段
//!   （GB10 首卡 `vram_mb=null` 时镜像亦 null，真值在 `gpus[0].unified_vram_mb`）；
//! - `cpu_model`：`/proc/cpuinfo` 首个 `model name`；aarch64 无该行（GB10 实测
//!   cpuinfo 只有 `CPU part` MIDR 码）→ 回退 `lscpu` 的 `Model name:` 行（大小核
//!   去重保序拼接，如 `Cortex-X925 + Cortex-A725`）；`cpu_cores`：`processor` 行计数；
//! - `ram_gb`：`/proc/meminfo` 的 `MemTotal`（kB → GiB，保留一位小数）；
//! - `model_name`/`max_model_len`/`context_len`：硬件探测拿不到，从 body 带
//!   （`context_len`=上下文长度自报别名，2026-09-02 起透传；endpoint→llm
//!   实例配置的关联猜测本批不做——llm 实例态在另一 handler 内存中，跨组件
//!   读取引入装配耦合，得不偿失）。
//!
//! 优先级：**body 字段 > 本地探测**（探测填缺省、body 覆盖；GPU 系整组裁决——
//! body 带非空 `gpus`（简化形态 `[{name,vram_mb}]`×N，index 可省）时列表整体
//! 覆盖，`gpu_count`/旧字段未显式给则从胜出列表首卡推导，见 `merge_server_config`）
//! （探测不到且必填缺省 → 400，必填=`model_name`）。
//!
//! # 负载监控输出（heartbeat 优先，metrics_url 代拉兜底）
//!
//! 活节点定期 `POST /:id/heartbeat` 自报负载（running/waiting/gpu_cache/
//! tps/latency/load_pct），消费者 `GET /:id/metrics` 时：
//! 1. 有**新鲜心跳**（≤60s）→ 直接返回心跳数据（`stale:false`，零外呼）；
//! 2. 无新鲜心跳但挂牌带 `metrics_url` → 服务端代拉（reqwest，默认 5s 超时；
//!    按 vllm metrics 端点约定 `{metrics:{...}}` 或平铺对象规范化为 6 键），
//!    成功 `reachable:true`（`stale:true`），失败/超时 `reachable:false` 降级；
//! 3. 都没有 → `reachable:false, source:"none"`（附最后一次心跳数据若有）。
//!
//! **服务端常驻心跳兜底**（2026-09-03 根因修复：此前心跳靠发布者浏览器开着
//! 大厅页的前端 60s 自动上报，页面一关联邦对端就把条目看成「不可达」）：
//! handler 构造时常驻任务每 [`HEARTBEAT_SWEEP_INTERVAL`]（60s）对本节点
//! active 本地条目跑一轮 [`refresh_local_heartbeats`]（复用 `update_heartbeat`
//! 写路径，`heartbeat_at=now`、load 保留最后一次上报值；已新鲜的跳过——页面
//! 驱动心跳更真，兜底永不覆盖）。页面驱动的心跳端点保留不动。心跳刷新随
//! 联邦 30min 定期重播/上线补推自然扩散——**消费者侧（联邦条目）的心跳
//! 可见性延迟 ≤ 重播周期 30 分钟**，前端联邦徽章因此只展示「源节点心跳：
//! N 分钟前」时间差，不做主动探测判定可达。
//!
//! # 接入信息 access_info（消费者凭据，2026-08-31）
//!
//! 挂牌可携带消费者接入凭据（JSON 列 `access_info`）：
//!
//! | 字段 | 说明 |
//! |------|------|
//! | api_key | 消费者调用凭据（如网关 sk-os- 令牌）；**仅 publisher 本人与 admin 可见明文** |
//! | auth_header | 鉴权头用法（`"Authorization Bearer"` 缺省；自定义如 `"X-Api-Key: <key>"`）|
//! | notes | 接入备注（如额外参数/限流说明；非敏感，恒明文）|
//!
//! 脱敏规则（[`mask_api_key`]）：非特权视角（匿名/他人链上身份）列表与详情的
//! `access_info.api_key` 输出 `<前4>***<后4>`；长度 ≤8 的 key 全掩码 `****`（前4+
//! 后4 会拼出原文）。admin 明文视角与网关 `extract_principal` 同口径（2026-09-02
//! 修联邦导入丢 key）：`req.auth` 带 Admin 角色的 Principal 即明文——含测试期
//! 默认注入（无 Authorization 头 + `NEXOS_AUTH_DEFAULT_ADMIN≠0` 直接注入 admin，
//! 本节点浏览器一键导入联邦条目即此路）、`NEXOS_ADMIN_TOKEN` 精确匹配、admin
//! JWT；`NEXOS_AUTH_DEFAULT_ADMIN=0` 关闭注入即回匿名脱敏。发布时 body 带可选
//! `access_info` → 重发布可更新；缺省 →
//! 保留既有值（与 download_count 同款刷新保留语义）。
//!
//! curl 示例鉴权头（[`curl_auth_header_line`]，2026-09-02）：明文视角拼真实
//! key（`-H 'Authorization: Bearer sk-os-…'`）；脱敏视角拼占位符
//! `<你的令牌>` 并附说明（完整令牌需发布者本人/admin 视角或向发布者索取）
//! ——脱敏残值（`前4***后4`）永不进 curl。
//!
//! # 联邦大厅（P3，照 NexHub 两步语义，2026-08-31）
//!
//! fed kind [`FED_KIND_API_MARKET_LOBBY`] = `"api_market_lobby"`，载荷
//! `{"fed":…,"node":<发布节点>,"node_id":<发布节点 NodeID>,"entry":{完整
//! ApiListing}}`：
//! - **两步联邦**：publish 只写本地不广播；`POST /:id/federate`（owner pubkey）
//!   置 `federated=true` 并广播最新快照（重复推送=重新广播）；
//! - **双通道补覆盖**（2026-09-03，修 fed_broadcast 只发"当时已连接"peer 的
//!   覆盖缺口——严格 NAT 对端常年无活连接，会永远错过发布广播窗口）：
//!   ① **上线补推**（[`ApiMarketFedEndpoint::backfill_to`]）：p2p 连接建立
//!   （`crate::handlers::p2p::spawn_conn_watcher` 观测 task）→ 对新连 peer
//!   定向补推本节点全部 federated 条目快照；② **定期重播**
//!   （[`ApiMarketFedEndpoint::replay_round`]，常驻任务每 30 分钟）：广播相位
//!   对当前已连接 peer + **定向补播相位**对 node-meta Active ∖ connected 的
//!   已知活跃节点（中继可达但**无常驻连接**——真机实证中继路由按需逐消息
//!   送达，Spark 类对端 connected 恒 false，广播/watcher 都够不着，只能
//!   `send_to` 定向）。各通道均逐条限幅 100ms、只发 `source_node='local'`
//!   条目（远程条目不转播——防环）、沿用本地指纹自回路过滤；语义表见
//!   docs/API_MARKET.md §9；
//! - **接收端幂等合并**（[`ApiMarketFedEndpoint::ingest`]）：按 `id`（兜底
//!   `api_name+publisher_pubkey`）去重——同源重发=Refreshed（保留本地
//!   download_count），本地已有同 id 但来源不同=Skipped（保护本地条目），
//!   新条目=Written（`source_node`=来源节点、`source_node_id`=验签 NodeID、
//!   `federated:true`、计数清零起步）；
//! - **删除不撤远端**：本地下架只删本地行，不广播撤销——远端副本由源节点
//!   重新 publish+federate 刷新或在对端自然过期（与 NexHub 同款语义）；
//! - **心跳/代拉对联邦条目同样可用**：heartbeat 在发布节点上跑（owner token），
//!   消费端 `GET /:id/metrics` 无本地心跳时走 `metrics_url` 代拉（指向源节点）。
//!
//! # 跨网中继（fed kind `api_relay_req` / `api_relay_resp`，2026-09-02）
//!
//! 缺陷背景：联邦条目的 `endpoint_url` 常是发布者内网地址（如
//! `http://192.0.2.106:8558/v1`）——数据同步走 overlay 没问题，但消费者
//! llm_external 的 chat/test 直连 HTTP 够不着（`上游请求失败: error sending
//! request for url`）。修法：消费者侧条目带 `via_node`（来源 NodeID）时，
//! HTTP 请求改经 overlay 定向发给源节点，由源节点代发（单跳，源节点即出口）。
//!
//! - **白名单红线**：源节点只代发 URL 与**本节点已发布条目** `endpoint_url`
//!   精确匹配（规范化后；封闭集合 `{E, E/models, E/chat/completions}`）的
//!   请求，否则 403「该 URL 不属于本节点发布的条目」——绝不做开放代理
//!   （overlay 对端可伪造请求）；方法仅 GET/POST；联邦远程条目不参与白名单
//!   （不二次转发——单跳语义）。
//! - 分块：body/chunk > [`RELAY_CHUNK_BYTES`]（1 MiB，沿 transfer.rs/live
//!   中继分块先例）按块多帧（req 帧 ci/cn、resp 帧 seq 递增，帧序即字节序）；
//! - 超时（缺省 [`RelayLimits`]，可注入缩短测超时清理）：req 级 30s、流式
//!   首块 15s、流式空闲 60s；req_id 关联 map 由巡检任务定期清理。
//! - 详见本文件「跨网中继」节与 docs/API_MARKET.md §10 / docs/LLM_EXTERNAL_APIS.md。
//!
//! # 路由表（7 条，component="api-market"；链上 token 一律 handler 内自验，
//! requires_auth=false——网关系统中间件不认识链上 token，挂 true 会全拦）
//!
//! | method | path | 动作 |
//! |--------|------|------|
//! | POST   | `/api/v1/api-market/publish`        | 挂牌/刷新（链上 token；重复=刷新保留计数）|
//! | GET    | `/api/v1/api-market`                | 大厅列表（公开；`?q=` 搜索 `?sort=recent\|price` `?scope=all\|local\|fed`）|
//! | GET    | `/api/v1/api-market/:id`            | 详情（公开；含心跳新鲜度）|
//! | DELETE | `/api/v1/api-market/:id`            | 下架（仅 owner pubkey，403「仅发布者可下架」）|
//! | POST   | `/api/v1/api-market/:id/heartbeat`  | 心跳自报负载（链上 token，owner）|
//! | GET    | `/api/v1/api-market/:id/metrics`    | 负载监控输出（公开；心跳优先→代拉→降级）|
//! | POST   | `/api/v1/api-market/:id/federate`   | 推送/重新推送到联邦大厅（owner；两步联邦第二步）|

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use once_cell::sync::Lazy;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};
use os_common::chain_auth::{self, ChainAuth};

// ----------------------------------------------------------------------------
// 路径常量
// ----------------------------------------------------------------------------

/// 大厅列表（GET，公开；?q= 搜索 ?sort=recent|price）。
const PATH_LIST: &str = "/api/v1/api-market";
/// 挂牌/刷新（POST，链上 token）。
const PATH_PUBLISH: &str = "/api/v1/api-market/publish";
/// 详情（GET，公开）。
const PATH_DETAIL: &str = "/api/v1/api-market/:id";
/// 下架（DELETE，仅 owner pubkey）。
const PATH_UNLIST: &str = "/api/v1/api-market/:id";
/// 心跳自报（POST，链上 token + owner）。
const PATH_HEARTBEAT: &str = "/api/v1/api-market/:id/heartbeat";
/// 负载监控输出（GET，公开）。
const PATH_METRICS: &str = "/api/v1/api-market/:id/metrics";
/// 推送/重新推送到联邦大厅（POST，链上 token + owner——两步联邦第二步）。
const PATH_FEDERATE: &str = "/api/v1/api-market/:id/federate";

/// 本 handler 注册时的组件名（`RouteSpec::handler_component`）。
const COMPONENT: &str = "api-market";

/// 心跳新鲜窗口（秒）：`heartbeat_at` 距今 ≤ 60s 视为活节点（与 IM 大厅在线判定同款）。
pub const HEARTBEAT_FRESH_SECS: i64 = 60;

/// 服务端常驻心跳兜底周期（2026-09-03 根因修复：心跳此前依赖发布者浏览器
/// 开着大厅页的前端 60s 自动上报——页面一关 `heartbeat_at` 过期，联邦对端的
/// 消费者就看到「不可达」）。handler 构造时常驻任务每本周期对本节点 active
/// 本地条目跑一轮 [`refresh_local_heartbeats`]（复用既有 `update_heartbeat`
/// 写路径）。周期取 60s = 新鲜窗口长度：页面驱动心跳（更真——带实时负载）
/// 恒先到，服务端兜底只接住「无页面」的空窗。
pub const HEARTBEAT_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// metrics_url 代拉默认超时（秒）。handler 持有可注入副本（测试缩短到亚秒）。
pub const DEFAULT_METRICS_TIMEOUT_SECS: u64 = 5;

// ----------------------------------------------------------------------------
// 共享 HTTP 客户端（复用 api_gateway 的 Lazy 模式：连接池复用，
// 各调用处用 RequestBuilder::timeout 按语义覆盖——代拉用 handler 的超时配置）
// ----------------------------------------------------------------------------

static HTTP: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("构建共享 reqwest Client 失败")
});

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 单块 GPU（nvidia-smi 一行；body 覆盖可带简化形态 `{name,vram_mb}`，index 省略）。
///
/// 统一内存架构（GB10/Jetson）：nvidia-smi 显存列 `[N/A]` → `vram_mb=None` +
/// `unified_memory=true` + `unified_vram_mb`（/proc/meminfo 池总量 MiB）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GpuEntry {
    /// nvidia-smi 序号（0 起；body 简化形态省略——同型多卡展示不依赖 index）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u64>,
    /// 卡名（如 `NVIDIA GeForce RTX 4090` / `NVIDIA GB10`）。
    #[serde(default)]
    pub name: String,
    /// 独立显存 MiB（nvidia-smi memory.total）；`[N/A]`（统一内存）/解析失败 → None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vram_mb: Option<u64>,
    /// 统一内存架构标记（CPU/GPU 共享 LPDDR5x；显存报 `[N/A]` 时 true）。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unified_memory: bool,
    /// 统一内存池总量 MiB（/proc/meminfo MemTotal；unified_memory=true 时探测填）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unified_vram_mb: Option<u64>,
}

/// 服务器配置（server_config JSON；硬件字段可本地探测，model_name 必填）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ServerConfig {
    /// GPU 型号（如 `NVIDIA GeForce RTX 3090`；=首卡 `gpus[0]` 镜像，向后兼容保留）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_name: Option<String>,
    /// GPU 显存（MiB；首卡镜像，向后兼容保留）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_vram_mb: Option<u64>,
    /// GPU 数量（探测=`gpus.len()`；无卡=0——CPU-only 节点可发布）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_count: Option<u64>,
    /// 全部 GPU（逐卡 `index/name/vram_mb`；同型号多卡=多条目，index 区分）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpus: Vec<GpuEntry>,
    /// CPU 型号（/proc/cpuinfo 首个 `model name`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_model: Option<String>,
    /// CPU 核数（/proc/cpuinfo processor 行计数）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_cores: Option<u64>,
    /// 内存（GiB；/proc/meminfo MemTotal，保留一位小数）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ram_gb: Option<f64>,
    /// 服务模型名（**必填**：硬件探测拿不到，body 必须带，缺省 400）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    /// 模型上下文长度（vLLM `--max-model-len`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_model_len: Option<u64>,
    /// 模型上下文长度（**发布端自报别名**，2026-09-02：此前发布 body 带
    /// `context_len` 会被 serde 静默丢弃——大厅「上下文」恒显示 —。现与
    /// `max_model_len` 并列为独立字段透传/持久化/随联邦载荷分发；展示端
    /// 优先本字段、缺省回落 `max_model_len`，两者皆缺 = 真实无值显示 —
    /// （不猜）。硬件探测恒 None（探测拿不到，只认 body）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_len: Option<u64>,
    /// 量化方案（如 `awq` / `gptq` / `fp8`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quantization: Option<String>,
    /// 节点区域（如 `cn-east`；可选）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
}

/// 计价（pricing JSON）。
///
/// - `mode=free`：currency 强制 `free`，不得带价；
/// - `mode=per_token`：`price_per_1k_tokens` 必填且 >0（每 1k token 单价）；
/// - `mode=per_image`：`price_per_1k_tokens` 必填且 >0（字段复用=**每图单价**，
///   设计定稿单价格字段，见 docs/API_MARKET.md §4）；
/// - 付费模式 currency ∈ {`sats`, `credits`}（缺省 `sats`），不得为 `free`。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Pricing {
    /// `free` / `per_token` / `per_image`。
    #[serde(default)]
    pub mode: String,
    /// 单价（sats/credits 单位；free 恒 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_per_1k_tokens: Option<u64>,
    /// `free` / `sats` / `credits`。
    #[serde(default)]
    pub currency: String,
    /// 计价说明（如「按输入+输出合计」；可选）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Pricing {
    /// 有效价格（排序/展示用）：free=0，付费=单价。跨币种按数值排
    /// （sats 与 credits 数值不互换，市场页展示币种列；见 docs/API_MARKET.md §5）。
    #[must_use]
    pub fn effective_price(&self) -> u64 {
        self.price_per_1k_tokens.unwrap_or(0)
    }

    /// 价格排序键：付费条目按价升序在前（`(0, price)`），免费垫底（`(1, 0)`）。
    #[must_use]
    pub fn price_sort_key(&self) -> (u8, u64) {
        let price = self.effective_price();
        if price > 0 {
            (0, price)
        } else {
            (1, 0)
        }
    }

    /// 是否免费（mode=free 或无价）。
    #[must_use]
    pub fn is_free(&self) -> bool {
        self.mode == "free" || self.effective_price() == 0
    }
}

/// 校验并规范化计价（publish 入口；非法组合 → Err 文案，调用方转 400）。
fn validate_pricing(input: &Pricing) -> Result<Pricing, String> {
    let mode = input.mode.trim().to_ascii_lowercase();
    match mode.as_str() {
        "free" => {
            if input.price_per_1k_tokens.unwrap_or(0) > 0 {
                return Err(
                    "free 模式不得携带 price_per_1k_tokens（付费请用 per_token/per_image）".into(),
                );
            }
            Ok(Pricing {
                mode: "free".into(),
                price_per_1k_tokens: None,
                currency: "free".into(),
                note: input.note.clone(),
            })
        }
        "per_token" | "per_image" => {
            let Some(price) = input.price_per_1k_tokens.filter(|p| *p > 0) else {
                return Err(format!(
                    "{mode} 模式必须给出 price_per_1k_tokens > 0（per_image 模式该字段语义=每图单价）"
                ));
            };
            let currency = if input.currency.trim().is_empty() {
                "sats".to_string()
            } else {
                input.currency.trim().to_ascii_lowercase()
            };
            if !matches!(currency.as_str(), "sats" | "credits") {
                return Err(format!(
                    "付费模式 currency 只支持 sats/credits（收到 {currency}）"
                ));
            }
            Ok(Pricing {
                mode,
                price_per_1k_tokens: Some(price),
                currency,
                note: input.note.clone(),
            })
        }
        other => Err(format!(
            "pricing.mode 非法：{other:?}（可选 free/per_token/per_image）"
        )),
    }
}

/// 规范化负载指标（心跳自报与 metrics 代拉的统一 6 键输出；缺省键不序列化）。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct LoadMetrics {
    /// GPU 负载百分比（0-100）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_pct: Option<f64>,
    /// 运行中请求数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running: Option<f64>,
    /// 排队请求数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<f64>,
    /// GPU KV cache 使用率（0-100）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_cache: Option<f64>,
    /// 吞吐（tokens/sec）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_sec: Option<f64>,
    /// 端到端时延（毫秒）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<f64>,
}

/// 各指标的别名集（心跳 body 键 / vllm metrics 约定键都收，先命中先用）。
const ALIASES_LOAD_PCT: &[&str] = &["load_pct", "load", "gpu_util"];
const ALIASES_RUNNING: &[&str] = &["running", "running_req", "num_requests_running"];
const ALIASES_WAITING: &[&str] = &["waiting", "waiting_req", "num_requests_waiting"];
const ALIASES_GPU_CACHE: &[&str] = &["gpu_cache", "gpu_cache_usage", "kv_cache_usage"];
const ALIASES_TPS: &[&str] = &["tokens_per_sec", "token_throughput", "tps"];
const ALIASES_LATENCY: &[&str] = &["latency_ms", "latency", "e2e_latency_ms"];

/// 从 JSON 对象按别名取首个数值字段（整数/浮点都收）。
fn num_by_aliases(v: &serde_json::Value, keys: &[&str]) -> Option<f64> {
    let obj = v.as_object()?;
    keys.iter()
        .find_map(|k| obj.get(*k))
        .and_then(|x| x.as_f64())
}

impl LoadMetrics {
    /// 从开放 JSON（心跳 body / 代拉响应）规范化。未知键忽略，缺省字段 None。
    #[must_use]
    pub fn from_json(v: &serde_json::Value) -> Self {
        Self {
            load_pct: num_by_aliases(v, ALIASES_LOAD_PCT),
            running: num_by_aliases(v, ALIASES_RUNNING),
            waiting: num_by_aliases(v, ALIASES_WAITING),
            gpu_cache: num_by_aliases(v, ALIASES_GPU_CACHE),
            tokens_per_sec: num_by_aliases(v, ALIASES_TPS),
            latency_ms: num_by_aliases(v, ALIASES_LATENCY),
        }
    }
}

/// 消费者接入信息（access_info JSON 列，2026-08-31）。
///
/// 发布者挂牌时可携带消费者直连凭据：`api_key`（如网关 sk-os- 令牌——**仅
/// publisher 本人与 admin 可见明文**，其他身份列表/详情输出脱敏值，见
/// [`mask_api_key`]；admin 判定 2026-09-02 起与 `extract_principal` 同口径——
/// 注入后的 admin Principal 即明文，含测试期无头默认注入）、`auth_header`
/// （鉴权头用法，缺省 `"Authorization Bearer"`；
/// 自定义如 `"X-Api-Key: <key>"`——按字面拼进 curl 示例）、`notes`（接入备注，
/// 非敏感恒明文）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AccessInfo {
    /// 消费者调用凭据（输出面按视角脱敏；存储面明文——发布者自持）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// 鉴权头用法（缺省 `Authorization Bearer`；自定义如 `X-Api-Key: <key>`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_header: Option<String>,
    /// 接入备注（额外参数/限流说明等；非敏感恒明文）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// access_info.auth_header 缺省值：标准 Bearer 头。
pub const DEFAULT_AUTH_HEADER: &str = "Authorization Bearer";

/// 规范化接入信息：三字段 trim、空串→None；auth_header 缺省回填
/// [`DEFAULT_AUTH_HEADER`]（curl 示例拼装只需一条规则）。
#[must_use]
pub fn normalize_access_info(input: AccessInfo) -> AccessInfo {
    let clean = |s: String| {
        let t = s.trim().to_string();
        (!t.is_empty()).then_some(t)
    };
    AccessInfo {
        api_key: input.api_key.and_then(clean),
        auth_header: input.auth_header.and_then(clean),
        notes: input.notes.and_then(clean),
    }
}

/// 接入信息是否为空（三字段全缺省——空对象不占 JSON）。
#[must_use]
pub fn access_info_is_empty(info: &AccessInfo) -> bool {
    info.api_key.is_none() && info.auth_header.is_none() && info.notes.is_none()
}

/// API key 脱敏（非特权视角的输出契约）：`<前4>***<后4>`；长度 ≤8 全掩码
/// `****`（前4+后4 会拼出原文，短 key 必须全掩）。空/缺省原样返回（空串
/// 不存在泄露面，调用方一般直接不序列化）。
#[must_use]
pub fn mask_api_key(key: &str) -> String {
    let k = key.trim();
    if k.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = k.chars().collect();
    if chars.len() > 8 {
        let head: String = chars[..4].iter().collect();
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("{head}***{tail}")
    } else {
        "****".to_string()
    }
}

/// 输出视角下的 access_info JSON（在序列化后的条目 Value 上原地改写）：
/// 特权（publisher 本人 / admin）→ api_key 明文；其他 → [`mask_api_key`]。
/// `auth_header`/`notes` 非敏感恒原样。
fn apply_access_info_mask(
    out: &mut serde_json::Value,
    info: &AccessInfo,
    reveal: bool,
) {
    if access_info_is_empty(info) {
        return; // 空对象不序列化（serde skip 也做不到——手工保险）
    }
    if reveal {
        return; // 特权视角：明文已在（序列化原样）
    }
    if let Some(key) = info.api_key.as_deref().filter(|k| !k.trim().is_empty()) {
        out["access_info"]["api_key"] = serde_json::json!(mask_api_key(key));
    }
}

/// curl 示例鉴权头占位符缺省文案（前端 i18n 同义：`apiMarket.tokenPlaceholder`）。
pub const CURL_TOKEN_PLACEHOLDER: &str = "<你的令牌>";

/// curl 示例的鉴权头行（纯函数；与前端 `ApiGateway.vue` 的 curl 拼装同一规则，
/// 2026-09-02 修复「`-H 'Authorization Bearer'` 只有头名没有值」缺陷）。
///
/// 两分支（返回 `(头行, 是否占位)`）：
/// - **明文分支**（`reveal=true`：publisher 本人/admin 视角拿到了明文 key）→
///   令牌值=真实 key，如 `-H 'Authorization: Bearer sk-os-xxxx'`；
/// - **占位分支**（`reveal=false` 脱敏视角，或发布端未配 api_key）→ 令牌值=
///   `placeholder`（如 `<你的令牌>`），且**绝不**把脱敏残值（`前4***后4`）拼进
///   curl——复制即用才是示例的意义；调用方应附一行说明（完整令牌需发布者
///   本人/admin 视角或向发布者索取）。
///
/// 头形态（`auth_header` 缺省 [`DEFAULT_AUTH_HEADER`]）：
/// - 含 `<key>` 占位（如 `X-Api-Key: <key>`）→ 字面替换为令牌值；
/// - `Authorization Bearer` / `Authorization: Bearer`（标准形态）→ 规范化为
///   `Authorization: Bearer <令牌值>`（带冒号——缺陷正是旧规则按字面拼出无值头）；
/// - 其他自定义（如 `X-Api-Key`，不带占位不带冒号）→ 补冒号拼令牌值；
/// - 缺省 → 标准 Bearer 头。
#[must_use]
pub fn curl_auth_header_line(
    auth_header: Option<&str>,
    api_key: Option<&str>,
    reveal: bool,
    placeholder: &str,
) -> (String, bool) {
    let key = api_key.map(str::trim).filter(|k| !k.is_empty());
    // 明文令牌：特权视角 + key 真实存在（脱敏视角的 key 残值一律不用）。
    let plaintext = reveal.then_some(key).flatten();
    let uses_placeholder = plaintext.is_none();
    let token = plaintext.unwrap_or(placeholder);
    let header = auth_header
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .unwrap_or(DEFAULT_AUTH_HEADER);
    let line = if header.contains("<key>") {
        header.replace("<key>", token)
    } else if header.eq_ignore_ascii_case(DEFAULT_AUTH_HEADER)
        || header.eq_ignore_ascii_case("Authorization: Bearer")
    {
        // 标准形态：缺省值「Authorization Bearer」按字面拼会缺冒号缺值——
        // 规范化为「Authorization: Bearer <令牌>」。
        format!("Authorization: Bearer {token}")
    } else if header.contains(':') {
        // 自定义且已带冒号（如「X-Api-Key:」）→ 直接拼值。
        format!("{header} {token}")
    } else {
        // 自定义纯头名（如「X-Api-Key」）→ 补冒号。
        format!("{header}: {token}")
    };
    (format!("-H '{line}'"), uses_placeholder)
}

/// 挂牌条目（api_market 行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiListing {
    pub id: String,
    pub api_name: String,
    #[serde(default)]
    pub description: String,
    pub endpoint_url: String,
    /// 发布者身份（token 反查 pubkey；body 自报一律忽略）。
    pub publisher_pubkey: String,
    /// EVM 派生展示名（`0x`+40hex）。
    #[serde(default)]
    pub publisher_display: String,
    #[serde(default)]
    pub server_config: ServerConfig,
    #[serde(default)]
    pub pricing: Pricing,
    #[serde(default)]
    pub metrics_url: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// 恒 `active`。
    #[serde(default = "default_status_active")]
    pub status: String,
    /// 首次挂牌时间（刷新保留）。
    pub created_at: String,
    /// 最近心跳（RFC3339；None=从未上报）。
    #[serde(default)]
    pub heartbeat_at: Option<String>,
    /// 最近心跳负载（规范化 6 键）。
    #[serde(default)]
    pub load: Option<LoadMetrics>,
    #[serde(default)]
    pub download_count: u64,
    /// 消费者接入信息（2026-08-31）：api_key（输出按视角脱敏）/auth_header/notes。
    #[serde(default, skip_serializing_if = "access_info_is_empty")]
    pub access_info: AccessInfo,
    /// 联邦来源节点（P3，2026-08-31）：本地发布恒 `"local"`；经 os-p2p 联邦
    /// 同步来的远程条目 = 发布节点名（前端据此显示 🌐 远程徽章与联邦 Tab 分流）。
    /// serde default 兼容存量 JSON/旧 payload（无字段 → local）。
    #[serde(default = "default_source_node")]
    pub source_node: String,
    /// 联邦来源节点 NodeID（`0x`+66hex，2026-09-02 跨网中继）：本地发布空串；
    /// 联邦接收端 ingest 时记验签发送方（桥传入 `msg.from`——不可伪造）。
    /// 消费者一键导入外部 API 时作为 `via_node` 写入 llm_external_apis——
    /// 该表的 chat/test 据此走 overlay 中继而非直连。空串 = 直连语义。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_node_id: String,
    /// 是否已推送到联邦大厅（两步联邦，照 NexHub 语义）：本地 publish 恒 false
    /// （不广播）；`POST /:id/federate` 置 true 并广播；重发布保留既有值；
    /// 联邦接收端条目随载荷携带（source 侧恒 true——记录的是发布侧推送状态）。
    #[serde(default)]
    pub federated: bool,
}

/// 联邦来源节点默认值（本地发布）。
fn default_source_node() -> String {
    "local".to_string()
}

fn default_status_active() -> String {
    "active".to_string()
}

// ----------------------------------------------------------------------------
// 纯函数（时间窗口 / 探测解析 / 合并优先级——可单测）
// ----------------------------------------------------------------------------

/// 心跳年龄（秒）：`heartbeat_at`（RFC3339）距今（`now_secs`，unix 秒）。
/// 解析失败 → None（宁可判旧，不误报新鲜）。
#[must_use]
pub fn heartbeat_age_secs(heartbeat_at: &str, now_secs: i64) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(heartbeat_at)
        .ok()
        .map(|t| now_secs - t.timestamp())
}

/// 心跳是否新鲜（|年龄| ≤ 60s；负年龄=时钟轻微超前也宽容，与 IM 在线判定同款）。
#[must_use]
pub fn heartbeat_fresh(heartbeat_at: &str) -> bool {
    heartbeat_age_secs(heartbeat_at, chrono::Utc::now().timestamp())
        .is_some_and(|age| age.abs() <= HEARTBEAT_FRESH_SECS)
}

/// 解析 `/proc/cpuinfo` 文本 → CPU 型号（首个 `model name`；无 → None）。
///
/// 注意匹配带空格的 `model name` 键（不误吃 `model\t: 142` 的短 `model` 行）。
#[must_use]
pub fn parse_cpuinfo_model(content: &str) -> Option<String> {
    let line = content
        .lines()
        .find(|l| l.trim_start().starts_with("model name"))?;
    let model = line.split_once(':')?.1.trim();
    (!model.is_empty()).then(|| model.to_string())
}

/// 解析 `lscpu` 输出 → CPU 型号（aarch64 回退路径；`/proc/cpuinfo` 无
/// `model name` 行——GB10 实测只有 `CPU part` MIDR 码，型号只在 lscpu）。
///
/// 收集全部 `Model name:` 行（大小核分组各一行，DGX Spark 实测：
/// `Cortex-X925` ×10 + `Cortex-A725` ×10），去重保序以 ` + ` 拼接 →
/// `Cortex-X925 + Cortex-A725`。x86 单组形态 `Model name: Intel...` 原样。
#[must_use]
pub fn parse_lscpu_model(output: &str) -> Option<String> {
    let mut names: Vec<&str> = Vec::new();
    for line in output.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        if !k.trim().eq_ignore_ascii_case("Model name") {
            continue;
        }
        let v = v.trim();
        if v.is_empty()
            || v.eq_ignore_ascii_case("n/a")
            || v.eq_ignore_ascii_case("(null)")
            || names.contains(&v)
        {
            continue;
        }
        names.push(v);
    }
    (!names.is_empty()).then(|| names.join(" + "))
}

/// 解析 `/proc/cpuinfo` 文本 → 逻辑核数（`processor` 行计数；0 → None）。
#[must_use]
pub fn parse_cpuinfo_core_count(content: &str) -> Option<u64> {
    let n = u64::try_from(
        content
            .lines()
            .filter(|l| l.trim_start().starts_with("processor"))
            .count(),
    )
    .ok()?;
    (n > 0).then_some(n)
}

/// 解析 `/proc/meminfo` 文本 → 内存 GiB（`MemTotal: N kB`，KiB→GiB 保留一位小数）。
#[must_use]
pub fn parse_meminfo_ram_gb(content: &str) -> Option<f64> {
    let line = content.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    let gib = kb as f64 * 1024.0 / (1024.0 * 1024.0 * 1024.0);
    Some((gib * 10.0).round() / 10.0)
}

/// 解析一行 nvidia-smi csv（`index, name, memory.total(MiB)`，无表头无单位）→ GpuEntry。
///
/// name 可解析即成卡；显存 `[N/A]`（DGX Spark GB10 统一内存，实测
/// `0, NVIDIA GB10, [N/A]`）/解析失败 → `vram_mb=None`（**不判无卡**）。
#[must_use]
pub fn parse_nvidia_gpu_csv(line: &str) -> Option<GpuEntry> {
    let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
    if parts.len() < 3 {
        return None;
    }
    let index = parts[0].parse::<u64>().ok()?;
    let name = parts[1];
    if name.is_empty() || name.eq_ignore_ascii_case("[N/A]") {
        return None;
    }
    let vram_mb = parts[2].parse::<u64>().ok();
    Some(GpuEntry {
        index: Some(index),
        name: name.to_string(),
        // 显存总量报不出 = 驱动不管理独立显存 = 统一内存架构形态（GB10/Jetson）
        unified_memory: vram_mb.is_none(),
        vram_mb,
        unified_vram_mb: None,
    })
}

/// 解析整段 nvidia-smi 输出 → GPU 列表（逐行、跳空行/坏行；空输出=无卡空列表）。
#[must_use]
pub fn parse_nvidia_gpus_output(output: &str) -> Vec<GpuEntry> {
    output
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(parse_nvidia_gpu_csv)
        .collect()
}

/// body 的 server_config 与本地探测结果合并：**body 字段优先**（Some 覆盖探测值，
/// None 落回探测值——探测不到则保持 None）。
///
/// GPU 系字段**整组裁决**（新旧字段不打架）：
/// - body 带非空 `gpus`（简化形态 `[{name,vram_mb}]`×N，index 可省）→ 列表整体
///   覆盖探测；`gpu_count` 与旧字段 `gpu_name`/`gpu_vram_mb` 未显式给时从
///   胜出列表首卡推导；
/// - body 仅带旧字段 `gpu_name`+`gpu_vram_mb`（老客户端，两者都给）→ 合成
///   单卡 `gpus`，`gpu_count`=1；
/// - body GPU 系全缺省 → 探测列表原样（无卡=`gpus` 空 + `gpu_count` 0）。
#[must_use]
pub fn merge_server_config(body: Option<ServerConfig>, probed: ServerConfig) -> ServerConfig {
    let Some(b) = body else {
        return probed;
    };
    let gpus = if !b.gpus.is_empty() {
        b.gpus.clone()
    } else if let (Some(name), Some(vram_mb)) = (b.gpu_name.clone(), b.gpu_vram_mb) {
        vec![GpuEntry {
            index: None,
            name,
            vram_mb: Some(vram_mb),
            ..Default::default()
        }]
    } else {
        probed.gpus.clone()
    };
    let gpu_name = b.gpu_name.or_else(|| gpus.first().map(|g| g.name.clone()));
    let gpu_vram_mb = b.gpu_vram_mb.or_else(|| gpus.first().and_then(|g| g.vram_mb));
    let gpu_count = b.gpu_count.or_else(|| u64::try_from(gpus.len()).ok());
    ServerConfig {
        gpu_name,
        gpu_vram_mb,
        gpu_count,
        gpus,
        cpu_model: b.cpu_model.or(probed.cpu_model),
        cpu_cores: b.cpu_cores.or(probed.cpu_cores),
        ram_gb: b.ram_gb.or(probed.ram_gb),
        model_name: b.model_name.or(probed.model_name),
        max_model_len: b.max_model_len.or(probed.max_model_len),
        context_len: b.context_len.or(probed.context_len),
        quantization: b.quantization.or(probed.quantization),
        region: b.region.or(probed.region),
    }
}

/// 排序参数规范化：`price` 原样，其余（含缺省）→ `recent`。
#[must_use]
pub fn normalize_sort(sort: Option<&str>) -> &'static str {
    match sort.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        Some("price") => "price",
        _ => "recent",
    }
}

/// 列表 scope 参数规范化（联邦扩展，2026-08-31）：`local`=仅本机发布条目、
/// `fed`=仅联邦远程条目、其余（含缺省）→ `all`（全量平铺数组——**向后兼容**，
/// 旧客户端/旧测试拿到的仍是同一形态的数组，只是元素多了 source_node 字段）。
#[must_use]
pub fn normalize_scope(scope: Option<&str>) -> &'static str {
    match scope.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        Some("local") => "local",
        Some("fed") => "fed",
        _ => "all",
    }
}

/// 条目是否本机发布（联邦 Tab 分流依据）：source_node == "local"。
#[must_use]
pub fn listing_is_local(listing: &ApiListing) -> bool {
    listing.source_node == default_source_node()
}

/// 联邦载荷类型标记（`payload.fed == "api_market_lobby"`，照 NexHub 命名惯例）。
pub const FED_KIND_API_MARKET_LOBBY: &str = "api_market_lobby";
/// 联邦载荷类型标记：跨网 API 中继请求（消费者 → 源节点，定向，可多帧）。
pub const FED_KIND_API_RELAY_REQ: &str = "api_relay_req";
/// 联邦载荷类型标记：跨网 API 中继响应（源节点 → 消费者，定向，可多帧）。
pub const FED_KIND_API_RELAY_RESP: &str = "api_relay_resp";

/// 中继分块大小（>1 MiB 的 body/chunk 按块多帧；os-p2p 帧上限 4 MiB，
/// base64 膨胀 4/3 后 1 MiB 块 ≈ 1.34 MiB 明文帧——安全余量充足，与
/// transfer.rs / live 中继分块先例同款口径）。
pub const RELAY_CHUNK_BYTES: usize = 1024 * 1024;

/// 中继缺省超时/限额（测试经 [`ApiMarketFedEndpoint::set_relay_limits_for_test`]
/// 注入缩短值快速验清理路径）。
#[derive(Debug, Clone, Copy)]
pub struct RelayLimits {
    /// req 级超时（非流式整包 / 无响应 pending 清理缺省）。
    pub req_timeout: Duration,
    /// 流式首块超时（Head 帧 15s 内未到 → 失败）。
    pub stream_first: Duration,
    /// 流式空闲超时（相邻 chunk 间 60s 无数据 → 断流）。
    pub stream_idle: Duration,
    /// 非流式响应聚合上限（防伪造超大响应撑爆内存）。
    pub max_resp_body: usize,
    /// req 分块重组上限（源端防伪造分块帧撑爆内存）。
    pub max_req_body: usize,
    /// pending/分块巡检周期。
    pub sweep_interval: Duration,
}

impl Default for RelayLimits {
    fn default() -> Self {
        Self {
            req_timeout: Duration::from_secs(30),
            stream_first: Duration::from_secs(15),
            stream_idle: Duration::from_secs(60),
            max_resp_body: 32 * 1024 * 1024,
            max_req_body: 32 * 1024 * 1024,
            sweep_interval: Duration::from_secs(10),
        }
    }
}

/// 源端一次待执行的代发请求（req 帧解析/重组完成后的内部形态）。
struct RelayJob {
    /// 请求方（resp 帧定向目标）。
    from: os_p2p::NodeId,
    req_id: String,
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    stream: bool,
    body: Vec<u8>,
}

/// 源端请求超时上限（非流式整包代发 600s 兜底——与 llm_external
/// CHAT_STREAM_TIMEOUT 同量级；流式同此总上限，空闲断流由消费者侧执行）。
const RELAY_UPSTREAM_TOTAL: Duration = Duration::from_secs(600);

/// 消费者侧一次中继 HTTP 请求（llm_external 组装：test=GET /models、
/// chat=POST /chat/completions；鉴权头在此注入，源节点只认白名单 URL）。
#[derive(Debug, Clone)]
pub struct ApiRelayRequest {
    /// `GET` / `POST`（小写会被归一大写；其余方法源端 403 拒绝）。
    pub method: String,
    /// 目标完整 URL（须命中源节点白名单封闭集合）。
    pub url: String,
    /// 请求头（原样转发，源端剥 hop-by-hop 头）。
    pub headers: Vec<(String, String)>,
    /// 请求体（POST；None=无体 GET）。
    pub body: Option<Vec<u8>>,
    /// true=流式（SSE 逐块透传）；false=整包。
    pub stream: bool,
}

/// 消费者侧收到的中继事件流（resp 帧序列的本地投影）。
#[derive(Debug, Clone)]
pub enum ApiRelayEvent {
    /// 首帧（seq=0）：上游状态码 + 响应头。
    Head {
        status: u16,
        headers: Vec<(String, String)>,
    },
    /// 数据块（帧序即字节序；Head 帧若带 chunk 会先于本事件投递）。
    Chunk(Vec<u8>),
    /// 收尾（done 帧；流就此正常结束）。
    Done,
}

/// 非流式中继完成形态（[`ApiMarketFedEndpoint::relay_roundtrip`] 产物）。
#[derive(Debug, Clone)]
pub struct RelayComplete {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// 流式中继句柄（[`ApiMarketFedEndpoint::relay_open_stream`] 产物）：Head 已
/// 消费（status/headers 即此），后续块经 [`RelayStream::next_chunk`] 逐块取
/// （空闲超时内部执行，超时/上游中断 → Err）。
pub struct RelayStream {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    rx: tokio::sync::mpsc::UnboundedReceiver<Result<ApiRelayEvent, String>>,
    idle: Duration,
}

impl RelayStream {
    /// 取下一块（None=正常收尾；Some(Err)=断流/超时原因）。空闲超时在内部
    /// 执行——超时即 Err（含「空闲 Ns」文案），调用方据此断流。
    pub async fn next_chunk(&mut self) -> Option<Result<Vec<u8>, String>> {
        match tokio::time::timeout(self.idle, self.rx.recv()).await {
            Err(_) => Some(Err(format!("流式空闲超时（{}s 无数据）", self.idle.as_secs()))),
            Ok(None) => None,
            Ok(Some(Err(e))) => Some(Err(e)),
            Ok(Some(Ok(ApiRelayEvent::Chunk(b)))) => Some(Ok(b)),
            // Head 不该再来（open_stream 已消费）；Done → None（正常收尾）。
            Ok(Some(Ok(ApiRelayEvent::Done))) => None,
            Ok(Some(Ok(ApiRelayEvent::Head { .. }))) => {
                Some(Err("协议错误：重复 Head 帧".into()))
            }
        }
    }
}

/// 联邦节点名净化（与 os_nexhub::sanitize_fed_node 同款规则——os-api 内自持
/// 一份，避免为两个常量反向依赖）：空/超长（>64 字符）回退 `"peer"`。
#[must_use]
pub fn sanitize_fed_node(node: &str) -> String {
    let n = node.trim();
    if n.is_empty() || n.chars().count() > 64 {
        "peer".to_string()
    } else {
        n.to_string()
    }
}

/// 构造 API 大厅联邦广播载荷（纯函数，发送端与测试共用，照 NexHub 形态）：
/// `{"fed":"api_market_lobby","node":<发布节点名>,"node_id":<发布节点 NodeID
/// hex>,"entry":{完整 ApiListing JSON}}`——`node_id` 供接收端记录
/// `source_node_id`（消费者一键导入外部 API 时的 `via_node` 来源，跨网中继
/// 定向目标）。
#[must_use]
pub fn build_api_market_fed_payload(
    node_hex: &str,
    node_name: &str,
    entry: &ApiListing,
) -> serde_json::Value {
    serde_json::json!({
        "fed": FED_KIND_API_MARKET_LOBBY,
        "node": sanitize_fed_node(node_name),
        "node_id": node_hex,
        "entry": entry,
    })
}

// ----------------------------------------------------------------------------
// blocking 硬件探测（spawn_blocking 内执行，失败降级 None 不 panic）
// ----------------------------------------------------------------------------

/// nvidia-smi 查询参数（与 llm.rs `build_nvidia_smi_cmd` 同风格：纯构造）。
#[must_use]
pub fn build_gpu_probe_cmd() -> Vec<String> {
    vec![
        "--query-gpu=index,name,memory.total".into(),
        "--format=csv,noheader,nounits".into(),
    ]
}

/// 探测全部 NVIDIA GPU（逐卡 index/卡名/显存 MiB）。无 nvidia-smi / 无卡 → 空列表
/// （静默降级不报错——CPU-only 节点可发布，不 500）。
///
/// 统一内存卡（GB10：显存 `[N/A]`）由 [`apply_unified_vram`] 回退填
/// `/proc/meminfo` 池总量（`unified_vram_mb`）。
fn probe_gpus_blocking() -> Vec<GpuEntry> {
    let out = std::process::Command::new("nvidia-smi")
        .args(build_gpu_probe_cmd())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .output();
    match out {
        // 命令失败（无卡/无命令）→ 静默空列表；成功 → 逐行解析（坏行跳过）。
        Ok(out) if out.status.success() => {
            let mut gpus = parse_nvidia_gpus_output(&String::from_utf8_lossy(&out.stdout));
            apply_unified_vram(&mut gpus);
            gpus
        }
        _ => Vec::new(),
    }
}

/// 统一内存回退：`unified_memory=true` 的条目填 `/proc/meminfo` MemTotal MiB
/// （与 `ram_gb` 同源同池；读失败保持 None，大厅如实展示型号不带容量）。
fn apply_unified_vram(gpus: &mut [GpuEntry]) {
    if !gpus.iter().any(|g| g.unified_memory) {
        return;
    }
    let (total_b, _, _, _) = crate::handlers::monitor::read_meminfo();
    if total_b == 0 {
        return;
    }
    let total_mib = total_b / (1024 * 1024);
    for g in gpus.iter_mut().filter(|g| g.unified_memory) {
        g.unified_vram_mb = Some(total_mib);
    }
}

/// 探测本机硬件 → 只填硬件字段的 ServerConfig（model 系字段探测不到恒 None）。
///
/// `cpu_model` 两级探测：`/proc/cpuinfo` 首个 `model name`（x86）→ 缺行
/// （aarch64：GB10 cpuinfo 只有 MIDR `CPU part` 码）回退 `lscpu` 的
/// `Model name:`（大小核去重保序拼接）。
fn probe_server_config_blocking() -> ServerConfig {
    let gpus = probe_gpus_blocking();
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let cpu_model = parse_cpuinfo_model(&cpuinfo).or_else(|| {
        std::process::Command::new("lscpu")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .stdin(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| parse_lscpu_model(&String::from_utf8_lossy(&o.stdout)))
    });
    ServerConfig {
        // 旧字段=首卡镜像（向后兼容）；gpu_count 恒有值（无卡=0）。
        // GB10 首卡 vram_mb=None → gpu_vram_mb=None（真值在 gpus[0].unified_vram_mb）
        gpu_name: gpus.first().map(|g| g.name.clone()),
        gpu_vram_mb: gpus.first().and_then(|g| g.vram_mb),
        gpu_count: u64::try_from(gpus.len()).ok(),
        gpus,
        cpu_model,
        cpu_cores: parse_cpuinfo_core_count(&cpuinfo),
        ram_gb: std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|c| parse_meminfo_ram_gb(&c)),
        model_name: None,
        max_model_len: None,
        // 上下文长度探测拿不到（只认 body 自报；见 ServerConfig::context_len 注释）。
        context_len: None,
        quantization: None,
        region: None,
    }
}

// ----------------------------------------------------------------------------
// ApiMarketRouteHandler
// ----------------------------------------------------------------------------

/// 已认证的发布者（链上 token 反查；无 admin 变体——设计定稿：公钥唯一通道）。
struct MarketCaller {
    pubkey: String,
    display_name: String,
}

/// API 大厅路由处理器——HTTP 边界适配到 SQLite `api_market` 挂牌索引 +
/// 本机硬件探测（spawn_blocking）+ metrics_url 代拉（reqwest）。
///
/// 持有 `Arc<Mutex<Connection>>`（短锁快放，不跨 `.await` 持锁；Arc 与联邦
/// 端点共享同一连接——发送端广播与接收端 ingest 写同一张表）+ 共享
/// [`ChainAuth`]（main.rs 装配时与 nexhub-lobby 同一实例——token 互通）+
/// 代拉超时（默认 5s，测试可注入亚秒值）。
pub struct ApiMarketRouteHandler {
    db: Arc<Mutex<Connection>>,
    auth: Arc<ChainAuth>,
    metrics_timeout: Duration,
    /// 系统 admin token（读面 access_info 明文视角用；构造时定格 env
    /// `NEXOS_ADMIN_TOKEN`/`OS_ADMIN_TOKEN`——与 im.rs 同款防请求期 env 竞态；
    /// 测试经 [`Self::with_admin_token`] 注入）。
    admin_token: Option<String>,
    /// 联邦端点（`federation()` 在 Box 进网关前取出——p2p 注入 + 桥分发共用）。
    fed: ApiMarketFedEndpoint,
    /// 服务端常驻心跳兜底周期（缺省 [`HEARTBEAT_SWEEP_INTERVAL`] 60s；测试经
    /// [`Self::set_heartbeat_sweep_interval_for_test`] 注入缩短值验证常驻任务
    /// 接线——任务每轮现读，注入即时生效）。Arc 共享给构造时 spawn 的兜底任务。
    heartbeat_sweep_interval: Arc<Mutex<Duration>>,
}

impl ApiMarketRouteHandler {
    /// 构造 handler：默认 DB 路径 + **独立** ChainAuth（本地诊断用——该实例
    /// 没有任何端点能签发 token，写操作实际不可用；生产装配走
    /// [`Self::with_chain_auth`] 与 nexhub 共享 token 桶）。
    #[must_use]
    pub fn new() -> Self {
        Self::open(&default_db_path(), Arc::new(ChainAuth::new()))
    }

    /// main.rs 装配构造：默认 DB 路径 + **共享**链上认证存储（传
    /// nexhub-lobby 的同一 `Arc<ChainAuth>`——`/api/v1/nexhub/auth/*` 签发的
    /// token 在 api-market 直接可用，401 文案即引导该处）。
    #[must_use]
    pub fn with_chain_auth(auth: Arc<ChainAuth>) -> Self {
        Self::open(&default_db_path(), auth)
    }

    /// 用指定 DB 路径 + 认证存储构造（集成测试/诊断注入）。
    #[must_use]
    pub fn with_db_path(path: &str, auth: Arc<ChainAuth>) -> Self {
        Self::open(path, auth)
    }

    /// 用临时内存库 + 注入认证存储构造（单元测试主入口：数据隔离，测试在
    /// 同一 ChainAuth 上走真挑战-签名流程拿 token）。
    #[must_use]
    pub fn with_auth(auth: Arc<ChainAuth>) -> Self {
        let mut h = Self::with_shared_db(Arc::new(Mutex::new(
            Connection::open_in_memory().expect("内存库必成功"),
        )));
        h.auth = auth;
        h
    }

    /// 内存库 + 已建表的共享连接构造（handler 与联邦端点写同一张表）。
    #[must_use]
    fn with_shared_db(db: Arc<Mutex<Connection>>) -> Self {
        {
            let conn = db.lock().expect("db poisoned");
            create_schema(&conn).expect("建表必成功");
        }
        let h = Self {
            fed: ApiMarketFedEndpoint::new(db.clone()),
            heartbeat_sweep_interval: Arc::new(Mutex::new(HEARTBEAT_SWEEP_INTERVAL)),
            db,
            auth: Arc::new(ChainAuth::new()),
            metrics_timeout: Duration::from_secs(DEFAULT_METRICS_TIMEOUT_SECS),
            admin_token: admin_token_from_env(),
        };
        h.install_heartbeat_sweep();
        h
    }

    /// 用临时内存库 + 独立认证存储构造（无注入场景的空 handler）。
    #[must_use]
    pub fn with_empty() -> Self {
        Self::with_auth(Arc::new(ChainAuth::new()))
    }

    /// 注入 metrics 代拉超时（链式构造器，测试用：缩短到亚秒快速验降级路径；
    /// 生产默认 [`DEFAULT_METRICS_TIMEOUT_SECS`]=5s）。
    #[must_use]
    pub fn with_metrics_timeout(mut self, timeout: Duration) -> Self {
        self.metrics_timeout = timeout;
        self
    }

    /// 注入系统 admin token（链式构造器，测试用——读面 access_info 明文视角；
    /// 生产构造时定格 env，见 [`Self::with_shared_db`]）。
    #[must_use]
    pub fn with_admin_token(mut self, token: &str) -> Self {
        let t = token.trim();
        self.admin_token = (!t.is_empty()).then(|| t.to_string());
        self
    }

    fn open(path: &str, auth: Arc<ChainAuth>) -> Self {
        let conn = open_db(path).unwrap_or_else(|e| {
            eprintln!("api-market: 打开 SQLite {path} 失败（{e}），降级到内存库");
            Connection::open_in_memory().expect("内存库必成功")
        });
        let mut h = Self::with_shared_db(Arc::new(Mutex::new(conn)));
        h.auth = auth;
        h
    }

    /// 共享认证存储引用（装配层/测试共享——测试在其上走真挑战-签名拿 token）。
    #[must_use]
    pub fn auth(&self) -> Arc<ChainAuth> {
        self.auth.clone()
    }

    /// 联邦端点引用（main.rs 装配：Box 进网关**前**取出——p2p spawn 后
    /// set_p2p 注入发送端 + FederationBridge 入站分发共用同一实例，照
    /// nexhub/live 的 federation() 模式）。
    #[must_use]
    pub fn federation(&self) -> ApiMarketFedEndpoint {
        self.fed.clone()
    }

    /// 服务端常驻心跳兜底任务（2026-09-03，构造时装配；与 fed 端点的 sweeper/
    /// 重播任务同款常驻语义）：每 [`HEARTBEAT_SWEEP_INTERVAL`]（60s，测试可
    /// 注入缩短）对本节点 active 本地条目跑一轮 [`refresh_local_heartbeats`]
    /// ——页面驱动的心跳端点保留不动（更真：带实时负载），服务端兜底只接住
    /// 「浏览器没开大厅页」的空窗。心跳刷新随联邦 30min 重播/上线补推自然
    /// 扩散（消费者侧心跳可见性 ≤ 重播周期 30 分钟，见 docs/API_MARKET.md §5.3）。
    ///
    /// 构造发生在 tokio runtime 内（main.rs `build_gateway` 是 async fn / 测试
    /// `#[tokio::test]`）→ `Handle::try_current` 拿当前 runtime spawn；纯同步
    /// 上下文（非 async 单测只借纯函数）拿不到 runtime 时静默跳过——构造不
    /// panic，兜底是后台语义不是硬依赖。
    fn install_heartbeat_sweep(&self) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return; // 无 runtime（同步单测/宿主直调）：不起兜底任务
        };
        let db = self.db.clone();
        let interval = self.heartbeat_sweep_interval.clone();
        handle.spawn(async move {
            loop {
                let d = interval
                    .lock()
                    .map(|g| *g)
                    .unwrap_or(HEARTBEAT_SWEEP_INTERVAL);
                tokio::time::sleep(d).await;
                let n = db
                    .lock()
                    .map(|conn| refresh_local_heartbeats(&conn))
                    .unwrap_or(0);
                if n > 0 {
                    eprintln!(
                        "[api-market] 心跳兜底：刷新 {n} 条本地条目（页面驱动心跳之外的存活证明）"
                    );
                }
            }
        });
    }

    /// 注入心跳兜底周期（测试专用：缩短到亚秒快速验证常驻任务接线；
    /// 生产缺省 [`HEARTBEAT_SWEEP_INTERVAL`]=60s）。
    pub fn set_heartbeat_sweep_interval_for_test(&self, interval: Duration) {
        *self
            .heartbeat_sweep_interval
            .lock()
            .expect("api-market heartbeat sweep interval poisoned") = interval;
    }

    /// 解析调用方：**仅**链上 token（Bearer → verify_token → pubkey → EVM 展示名）。
    ///
    /// 无 admin 回落（设计定稿）：`NEXOS_ADMIN_TOKEN` 在本 handler 无任何特权，
    /// 无/无效 token 一律 None → 调用方回 401。
    fn caller(&self, req: &ApiRequest) -> Option<MarketCaller> {
        let token = chain_auth::bearer_token(&req.headers)?;
        let pubkey = self.auth.verify_token(token)?;
        let vk = chain_auth::parse_pubkey(&pubkey)?;
        Some(MarketCaller {
            display_name: chain_auth::derive_display_name(&vk),
            pubkey,
        })
    }

    /// 读面特权判定（access_info 明文视角，2026-08-31；2026-09-02 修「默认注入
    /// admin 没吃进脱敏判定」）：**publisher 本人**（链上 token 反查 pubkey ==
    /// 条目 publisher_pubkey）或 **admin** → true。
    ///
    /// admin 判定与网关 [`crate::http::extract_principal`] 同口径（照
    /// model_hub/agent_coord 的 `req.auth` 惯用法）：`request_to_api` 对**所有**
    /// 请求（含本 handler 的 requires_auth=false 路由）都解析 `req.auth`，带
    /// Admin 角色的 Principal 即明文视角——覆盖三种来源：
    ///
    /// 1. **测试期默认注入**（`NEXOS_AUTH_DEFAULT_ADMIN≠0`，默认开）：无
    ///    Authorization 头的请求直接注入 admin Principal → 本节点浏览器无凭据
    ///    一键导入联邦条目也能带上明文 key（此前被当匿名脱敏，用户要手动补填）；
    /// 2. `NEXOS_ADMIN_TOKEN` Bearer 精确匹配（网关侧注入 admin Principal）；
    /// 3. 带 Admin 角色的 JWT。
    ///
    /// `NEXOS_AUTH_DEFAULT_ADMIN=0` 关闭注入后，无头请求 `req.auth=None` →
    /// 自然回到匿名脱敏。链上 token 的 caller 判定在最前（一个请求只有一个
    /// Authorization 头，链上身份与 admin 凭据互斥不冲突）。兜底保留构造期
    /// 定格系统 admin token 的精确匹配（handler 级直测不经网关 state 的路径）。
    ///
    /// 注意语义边界：admin 只在**读面**可见明文（运维视角——密钥泄露应急/排障），
    /// 写面（publish/delete/heartbeat/federate）仍无 admin 回落（设计定稿不变，
    /// 发布者必须是可验签的链上身份）。
    fn access_info_revealed(&self, req: &ApiRequest, publisher_pubkey: &str) -> bool {
        if let Some(caller) = self.caller(req) {
            return caller.pubkey == publisher_pubkey;
        }
        // 网关注入后的 Principal 带 Admin 角色 → 明文视角（extract_principal
        // 同口径：默认注入 admin / admin token / admin JWT 三源合一）。
        if req.auth.as_ref().is_some_and(|p| {
            p.roles.iter().any(|r| matches!(r, os_security::Role::Admin))
        }) {
            return true;
        }
        // 兜底：链上身份与 Principal 都不匹配 → 构造期定格的系统 admin token
        // 精确匹配（未配置 env 一律 false）。
        chain_auth::bearer_token(&req.headers).is_some_and(|t| {
            self.admin_token.as_deref().is_some_and(|admin| admin == t)
        })
    }

    /// metrics_url 代拉：reqwest GET（handler 的超时配置），响应按 vllm metrics
    /// 约定 `{metrics:{...}}`（或平铺对象）规范化。网络/HTTP/解析失败 → Err
    /// （调用方降级 `reachable:false`，不 panic）。
    async fn fetch_metrics(&self, url: &str) -> Result<LoadMetrics, String> {
        let resp = HTTP
            .get(url)
            .timeout(self.metrics_timeout)
            .send()
            .await
            .map_err(|e| {
                format!(
                    "代拉失败（请求发送失败/超时 {:#?}）: {e}",
                    self.metrics_timeout
                )
            })?;
        let resp = resp
            .error_for_status()
            .map_err(|e| format!("代拉失败（上游 HTTP 错误）: {e}"))?;
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("代拉失败（解析 JSON 失败）: {e}"))?;
        // vllm metrics 约定 {metrics:{...}}；平铺对象也收（取 .metrics 否则原样）。
        let inner = v.get("metrics").unwrap_or(&v);
        Ok(LoadMetrics::from_json(inner))
    }

    /// 当前全量挂牌快照（active 态；测试/诊断用）。
    #[must_use]
    pub fn listings_snapshot(&self) -> Vec<ApiListing> {
        let conn = self.db.lock().expect("db poisoned");
        load_listings(&conn, None, "recent").unwrap_or_default()
    }
}

impl Default for ApiMarketRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

// ----------------------------------------------------------------------------
// 联邦大厅端点（P3，照 os-nexhub 的 LobbyFedEndpoint 模式；api_market 在
// os-api 内自持一份——与 handler 共享同一 Arc<Mutex<Connection>>）
// ----------------------------------------------------------------------------

/// 联邦广播通道（os-p2p 或测试 fake overlay）：fire-and-forget 把一条载荷发给
/// 全部已连接 peer（实现方负责 fan-out）；未装配/失败静默丢弃——联邦是尽力而
/// 为的传播，不是可靠队列（与 nexhub LobbyFedTransport 同款语义，闭包形态省
/// 一个跨模块 trait）。
pub type FedBroadcastFn = Arc<dyn Fn(serde_json::Value) + Send + Sync>;

/// 联邦定向发送通道（中继协议用，消费者 ↔ 源节点；生产包 os_p2p
/// `Handle::send`，测试 fake overlay 注入对端 `dispatch` 直投，与 live.rs
/// `FedSendFn` 同款）。
pub type FedSendFn = Arc<dyn Fn(&os_p2p::NodeId, serde_json::Value) + Send + Sync>;

/// 定向补播目标集枚举（异步——生产实现要 `node_meta()`/`peers()` 命令往返）：
/// 返回**按需路由可达但无常驻连接**的已知活跃 NodeID 列表。生产语义 =
/// node-meta 注册表 Active ∖ 当前 connected ∖ 本机指纹（真机实证 2026-09-03：
/// 中继路由按需逐消息送达，`/p2p/connect` 走 relayed 成功也**不产生常驻
/// Conn**——Spark 类对端在 peers 表 connected 恒 false，广播/连接 watcher
/// 两通道都够不着，只能 `send_to` 逐消息定向）。
pub type FedKnownActiveFn = Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<os_p2p::NodeId>> + Send>>
        + Send
        + Sync,
>;

/// 定向补播目标集内核（纯函数，[`FedKnownActiveFn`] 生产闭包的过滤规则）：
/// node-meta 注册表 **Active** 条目 ∖ 当前 connected ∖ 本机指纹。
///
/// - Active ∖ connected → 收（中继可达的已知活跃节点——Spark 类）；
/// - Inactive → 不收（五振出局节点不补播）；
/// - connected → 不收（广播面已覆盖——定向重叠只会让对端多吃 Duplicate）；
/// - 本机指纹 → 不收（自回路防护，fed_broadcast 同语义）。
fn fed_direct_replay_targets(
    meta: &[os_p2p::NodeMetaEntry],
    connected: &std::collections::HashSet<os_p2p::NodeId>,
    self_id: &os_p2p::NodeId,
) -> Vec<os_p2p::NodeId> {
    meta.iter()
        .filter(|e| matches!(e.state, os_p2p::MetaState::Active { .. }))
        .map(|e| e.id.clone())
        .filter(|id| !connected.contains(id) && id != self_id)
        .collect()
}

/// 已装配的 overlay 通道：两个发送面 + 定向补播目标集 + 本节点身份（广播走
/// name，中继定向走 NodeID）。
struct ApiMarketFedTransport {
    broadcast: FedBroadcastFn,
    /// 定向发送（`set_transport` 简装时为 no-op——只有广播语义的旧用法不受影响）。
    send_to: FedSendFn,
    /// 定向补播目标集（None = 通道不支持——简装/fake 旧用法零影响；生产
    /// `set_p2p` 装配 node-meta Active ∖ connected，测试注入固定列表）。
    known_active: Option<FedKnownActiveFn>,
    /// 本节点 NodeID（`0x` + 66 hex；简装时空串=中继不可用）。
    node_hex: String,
    node_name: String,
}

/// 内存去重缓存容量（最近 1000 条——超出丢最旧，DB 判定兜底；与 nexhub 同款）。
const FED_SEEN_LIMIT: usize = 1000;

/// 补推/重播逐条发送间隔（限幅防 burst，2026-09-03 覆盖缺口修复）：100ms/条
/// ——条目多时补推是长尾滴流而非瞬时风暴，对端 ingest 幂等无需更快。
const FED_BACKFILL_SPACING: Duration = Duration::from_millis(100);

/// 定期重播周期：每 30 分钟把本节点全部 federated 条目重播一轮（联邦端点
/// install 时常驻任务，[`ApiMarketFedEndpoint::replay_round`] 一轮 = 广播相位
/// 对当前已连接 peer + 定向补播相位对 node-meta Active ∖ connected 的中继
/// 可达节点）。幂等零负担：同快照被对端 seen 缓存 Duplicate 拦截（零 DB
/// 触碰）；快照变（心跳/负载/重新 federate）则 Refreshed 自然更新——顺带
/// 补上"心跳不联邦传播"的观感缺口。
const FED_REPLAY_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// 消费者侧一个进行中的中继请求（resp 帧关联目标）。
struct PendingRelay {
    /// 定向目标（resp 帧发送方必须等于它——防第三方伪造应答）。
    target: os_p2p::NodeId,
    /// 事件通道（消费方持有 Receiver；send 失败=对端已放弃 → 清理）。
    tx: tokio::sync::mpsc::UnboundedSender<Result<ApiRelayEvent, String>>,
    last_seen: std::time::Instant,
}

/// 源端一个进行中的分块请求重组缓冲（cn>1 的 api_relay_req 多帧场景）。
struct PartialRelayReq {
    from: os_p2p::NodeId,
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    stream: bool,
    parts: Vec<Option<Vec<u8>>>,
    received: usize,
    total_bytes: usize,
    last_seen: std::time::Instant,
}

/// [`ApiMarketFedEndpoint::ingest`] 的处置结果（测试/诊断观测面，与 nexhub 的
/// `LobbyFedIngest` 同构）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiMarketFedIngest {
    /// 新条目已写入（source_node=来源节点，本地 download_count 清零起步）。
    Written,
    /// 同源（同 id 或同 api_name+publisher 且同 source_node）重发 → 刷新快照，
    /// 保留本地 download_count。
    Refreshed,
    /// 逐字节相同载荷重放（内存缓存命中），未触碰 DB。
    Duplicate,
    /// 本地已有同 id（或同 api_name+publisher）条目但来源不同（本机/他节点先到）
    /// → 保护本地条目，跳过。
    Skipped,
    /// 载荷非法（fed kind 不符/缺 node/entry 解析失败/必填缺失），丢弃。
    Invalid,
}

/// API 大厅联邦端点——`Clone` 共享同一内核（main.rs 装配：`federation()` 在
/// Box 进网关**前**取出，p2p spawn 成功后 `set_p2p` 注入发送端；入站载荷经
/// `handlers/p2p.rs` 的 FederationBridge 调 [`ApiMarketFedEndpoint::ingest`]）。
///
/// 与 handler 共享同一 `Arc<Mutex<Connection>>`（锁语义与重构前一致：短锁快放，
/// 不跨 await）——发送端广播的快照与接收端落库写的是同一张表。
#[derive(Clone)]
pub struct ApiMarketFedEndpoint {
    inner: Arc<ApiMarketFedInner>,
}

struct ApiMarketFedInner {
    db: Arc<Mutex<Connection>>,
    /// 注入的联邦通道 + 本节点身份（None = 未装配 os-p2p，广播/中继静默跳过）。
    transport: Mutex<Option<ApiMarketFedTransport>>,
    /// 近期已见联邦载荷键（`api_market\0node\0id\0<条目快照 JSON>`）内存缓存。
    /// 键含**完整快照串**：只拦逐字节相同的重放（p2p 层重投递）；同源**新快照**
    /// （发布侧重新 publish+federate）键不同 → 穿透到 DB 权威路径 Refreshed
    /// ——对端刷新永远到得了本节点（nexhub 2026-08-23 同款修复语义）。
    seen: Mutex<std::collections::VecDeque<String>>,
    /// 消费者侧中继关联表：req_id → pending（resp 帧回填事件流）。
    pending: Mutex<std::collections::HashMap<String, PendingRelay>>,
    /// 源端分块请求重组缓冲：req_id → partial。
    partial_reqs: Mutex<std::collections::HashMap<String, PartialRelayReq>>,
    /// 中继超时/限额（缺省 [`RelayLimits::default`]；测试注入缩短值）。
    limits: Mutex<RelayLimits>,
    /// 巡检任务防重（同一端点实例只起一条 sweeper）。
    sweeper_installed: std::sync::atomic::AtomicBool,
    /// 定期重播周期（缺省 [`FED_REPLAY_INTERVAL`] 30 分钟；测试注入缩短值
    /// 验证常驻重播任务的接线——每轮循环现读，注入即时生效）。
    replay_interval: Mutex<Duration>,
}

impl ApiMarketFedEndpoint {
    fn new(db: Arc<Mutex<Connection>>) -> Self {
        Self {
            inner: Arc::new(ApiMarketFedInner {
                db,
                transport: Mutex::new(None),
                seen: Mutex::new(std::collections::VecDeque::new()),
                pending: Mutex::new(std::collections::HashMap::new()),
                partial_reqs: Mutex::new(std::collections::HashMap::new()),
                limits: Mutex::new(RelayLimits::default()),
                sweeper_installed: std::sync::atomic::AtomicBool::new(false),
                replay_interval: Mutex::new(FED_REPLAY_INTERVAL),
            }),
        }
    }

    /// 注入 os-p2p Handle + 本节点名（main.rs 装配，p2p spawn 成功后调用）：
    /// 广播闭包内 `tokio::spawn` 走 [`crate::handlers::p2p::fed_broadcast`]
    /// （fire-and-forget——发布路径不被联邦传播阻塞；本地指纹目标在该处统一
    /// 跳过，防自回路重复入库）；定向闭包直调 `Handle::send`（中继帧送达走
    /// overlay 路由）；定向补播目标集闭包 = node-meta Active ∖ connected ∖
    /// 本机指纹（重播轮对中继可达但无常驻连接的节点定向补播——Spark 类，
    /// [`fed_direct_replay_targets`]）。重复注入覆盖旧通道（测试/热替换友好）。
    pub fn set_p2p(&self, handle: os_p2p::Handle, node: String) {
        let node_hex = handle.self_id().to_hex();
        // 节点名兜底（2026-09-03 真机跟进）：NEXOS_P2P_NAME 未设时空名经
        // sanitize 会变 "peer"——老接收端把 "peer" 当缺 node 拒收（静默），
        // 匿名发送端的联邦条目全军覆没。合成稳定短名 node-<NodeID 前 8 hex>，
        // 联邦侧归因可读且跨重启稳定（NodeID 持久不变）。
        let node = if node.trim().is_empty() {
            format!("node-{}", &node_hex[2..10])
        } else {
            node
        };
        let h_bcast = handle.clone();
        let broadcast: FedBroadcastFn = Arc::new(move |payload| {
            let h = h_bcast.clone();
            tokio::spawn(async move {
                crate::handlers::p2p::fed_broadcast(&h, payload).await;
            });
        });
        let h_direct = handle.clone();
        let send_to: FedSendFn = Arc::new(move |to, payload| h_direct.send(to, payload));
        // 定向补播目标集：node-meta 注册表快照 + 当前已连集合 → 纯过滤
        //（Active ∖ connected ∖ 本机指纹）。每次枚举现拉注册表——Inactive
        // 出局/复活的节点自然进出目标集。
        let h_targets = handle.clone();
        let known_active: FedKnownActiveFn = Arc::new(move || {
            let h = h_targets.clone();
            Box::pin(async move {
                let (meta, peers) = tokio::join!(h.node_meta(), h.peers());
                let connected: std::collections::HashSet<os_p2p::NodeId> = peers
                    .into_iter()
                    .filter(|p| p.connected)
                    .map(|p| p.id)
                    .collect();
                fed_direct_replay_targets(&meta, &connected, h.self_id())
            })
        });
        self.install_transport(send_to, broadcast, Some(known_active), node_hex, node);
    }

    /// 装配广播通道 + 本节点名（生产 set_p2p / 测试 fake overlay 共用；简装
    /// 无定向面——只有广播语义的旧测试不受影响）。
    pub fn set_transport(&self, broadcast: FedBroadcastFn, node: String) {
        let send_to: FedSendFn = Arc::new(|_to, _payload| {
            eprintln!("[api-market-fed] 简装通道无定向面，丢弃中继帧");
        });
        self.install_transport(send_to, broadcast, None, String::new(), node);
    }

    /// 全量装配（广播 + 定向 + NodeID）：fake overlay 测试互连用——A 的
    /// send_to 直投 B 的 `dispatch(from=A_id, payload)`，反向同理（llm_external
    /// 等兄弟模块测试共用；定向补播目标集缺省 None，测试用
    /// [`Self::set_known_active_for_test`] 注入）。
    pub fn set_full_transport(
        &self,
        send_to: FedSendFn,
        broadcast: FedBroadcastFn,
        node_hex: String,
        node: String,
    ) {
        self.install_transport(send_to, broadcast, None, node_hex, node);
    }

    /// 注入定向补播目标集（测试专用：fake 枚举返回固定列表——模拟"node-meta
    /// Active ∖ connected"的产物；生产在 [`Self::set_p2p`] 内装配真注册表
    /// 过滤）。须在通道已装配后调用（无通道时 no-op）。
    pub fn set_known_active_for_test(&self, targets: Vec<os_p2p::NodeId>) {
        let list = targets;
        let f: FedKnownActiveFn = Arc::new(move || {
            let list = list.clone();
            Box::pin(async move { list })
        });
        let mut guard = self
            .inner
            .transport
            .lock()
            .expect("api-market fed transport poisoned");
        if let Some(t) = guard.as_mut() {
            t.known_active = Some(f);
        }
    }

    /// 装配内核：写通道 + 起中继巡检任务（pending/partial 超时清理）。
    fn install_transport(
        &self,
        send_to: FedSendFn,
        broadcast: FedBroadcastFn,
        known_active: Option<FedKnownActiveFn>,
        node_hex: String,
        node_name: String,
    ) {
        *self
            .inner
            .transport
            .lock()
            .expect("api-market fed transport poisoned") = Some(ApiMarketFedTransport {
            broadcast,
            send_to,
            known_active,
            node_hex,
            node_name: sanitize_fed_node(&node_name),
        });
        // 巡检任务一次性装配（端点 Clone 共享 inner，多实例只起一条）。
        if !self
            .inner
            .sweeper_installed
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let inner = self.inner.clone();
            tokio::spawn(async move {
                loop {
                    let interval = inner
                        .limits
                        .lock()
                        .map(|l| l.sweep_interval)
                        .unwrap_or(Duration::from_secs(10));
                    tokio::time::sleep(interval).await;
                    let limits = inner
                        .limits
                        .lock()
                        .map(|l| *l)
                        .unwrap_or_default();
                    sweep_relay_state(&inner, &limits);
                }
            });
            // 定期重播任务（同一次性装配，2026-09-03 覆盖缺口修复）：每
            // [`FED_REPLAY_INTERVAL`]（30 分钟，测试可注入缩短）把本节点全部
            // federated 条目对当前已连接 peer 重播一遍——fed_broadcast 只发
            // "当时已连"的一跳，错过发布窗口的对端（严格 NAT 长期无活连接）
            // 靠这轮追上；幂等 ingest 拦重复零成本（详见 replay_round_inner）。
            let inner = self.inner.clone();
            tokio::spawn(async move {
                loop {
                    let interval = inner
                        .replay_interval
                        .lock()
                        .map(|d| *d)
                        .unwrap_or(FED_REPLAY_INTERVAL);
                    tokio::time::sleep(interval).await;
                    replay_round_inner(&inner).await;
                }
            });
        }
    }

    /// 是否已装配传输通道（未装配时发布不联邦——单机部署零开销）。
    #[must_use]
    pub fn is_federated(&self) -> bool {
        self.inner
            .transport
            .lock()
            .expect("api-market fed transport poisoned")
            .is_some()
    }

    /// 空端点（cfg(test)：llm_external 等兄弟模块的中继端到端测试——消费者
    /// 侧实例，`set_full_transport` 互连后用）。
    #[cfg(test)]
    pub(crate) fn test_endpoint() -> Self {
        Self::new(Arc::new(Mutex::new(
            Connection::open_in_memory().expect("内存库必成功"),
        )))
    }

    /// 种入一条本地已发布条目的端点（cfg(test)：中继白名单的源端 fixture——
    /// endpoint_url 即白名单基准）。
    #[cfg(test)]
    pub(crate) fn test_endpoint_with_local_listing(endpoint_url: &str) -> Self {
        let ep = Self::test_endpoint();
        {
            let conn = ep.inner.db.lock().expect("db poisoned");
            create_schema(&conn).expect("建表必成功");
            insert_listing(
                &conn,
                &ApiListing {
                    id: "relay-seed-1".into(),
                    api_name: "relay-seed".into(),
                    description: String::new(),
                    endpoint_url: endpoint_url.to_string(),
                    publisher_pubkey: "0xseed".into(),
                    publisher_display: String::new(),
                    server_config: ServerConfig::default(),
                    pricing: Pricing::default(),
                    metrics_url: None,
                    tags: vec![],
                    status: default_status_active(),
                    created_at: "t".into(),
                    heartbeat_at: None,
                    load: None,
                    download_count: 0,
                    access_info: AccessInfo::default(),
                    source_node: default_source_node(),
                    source_node_id: String::new(),
                    federated: true,
                },
            )
            .expect("种入条目必成功");
        }
        ep
    }

    /// 注入中继超时/限额（测试专用：缩短 sweep/req 超时快速验清理路径；
    /// 生产缺省 [`RelayLimits::default`]）。
    pub fn set_relay_limits_for_test(&self, limits: RelayLimits) {
        *self.inner.limits.lock().expect("api-market fed limits poisoned") = limits;
    }

    /// 发布路径联邦广播：构造载荷 → transport 广播给全部已连接 peer
    /// （调用方=handler 的 federate 端点，推送资格已裁决）。
    ///
    /// 未装配通道（P2P 未启用）静默跳过（不阻塞本地推送语义——federated 标志
    /// 已在调用方置位）。观测日志与 nexhub/im 的 fed 面同款（journalctl 可查）。
    pub fn broadcast_entry(&self, entry: &ApiListing) {
        let guard = self.inner.transport.lock().expect("api-market fed transport poisoned");
        let Some(t) = guard.as_ref() else {
            eprintln!(
                "[api-market-fed] 跳过广播 {}（P2P 通道未装配）",
                entry.api_name
            );
            return;
        };
        eprintln!(
            "[api-market-fed] 广播条目 {}（node={}）",
            entry.api_name, t.node_name
        );
        (t.broadcast)(build_api_market_fed_payload(&t.node_hex, &t.node_name, entry));
    }

    /// 上线补推（on-connect backfill，2026-09-03 覆盖缺口修复）：对本节点全部
    /// `federated=1` 且本地发布（`source_node='local'`——**远程条目不转播**，
    /// 防环红线沿用 fed_broadcast 的"接收方不转播"语义）的条目，按与
    /// broadcast_entry 同一 api_market_lobby 载荷形态逐条 `send_to` 定向发给
    /// 新连 peer。
    ///
    /// 触发点：`crate::handlers::p2p::spawn_conn_watcher` 感知连接建立 →
    /// main.rs 装配的回调（`backfill_to` spawn 异步跑，不阻塞观测 task）。
    /// 覆盖的缺陷：fed_broadcast 只发"当时已连接"的 peer——严格 NAT 对端
    /// （常年无活连接）永远错过发布广播窗口，此前只能手工种库。
    ///
    /// - **幂等零负担**：对端 ingest 对同快照重放返回 Duplicate（seen 缓存
    ///   拦截，零 DB 触碰）；快照变（心跳/负载/重新 federate）→ Refreshed；
    /// - **限幅**：逐条间隔 [`FED_BACKFILL_SPACING`]（100ms）防 burst；
    /// - **自回路防护**：目标指纹==本机 NodeID（node_hex）直接跳过——与
    ///   fed_broadcast 的本地指纹过滤同语义（观测 task 侧是第一道，此处兜底）；
    /// - 未装配通道 / 无 federated 条目 → 零帧（返回 0）。
    ///
    /// 返回补推条目数。
    pub async fn backfill_to(&self, peer: &os_p2p::NodeId) -> usize {
        // 短锁取通道三件套 + 自回路裁决（绝不持锁跨 sleep——通道闭包是
        // fire-and-forget，克隆 Arc 后即放）。
        let (send_to, node_hex, node_name) = {
            let guard = self
                .inner
                .transport
                .lock()
                .expect("api-market fed transport poisoned");
            let Some(t) = guard.as_ref() else {
                return 0; // P2P 未装配：单机部署零开销（静默——补推是后台语义）
            };
            if !t.node_hex.is_empty() && peer.to_hex() == t.node_hex {
                eprintln!("[api-market-fed] 补推跳过本地指纹目标（自回路防护）");
                return 0;
            }
            (t.send_to.clone(), t.node_hex.clone(), t.node_name.clone())
        };
        let entries = {
            let conn = self.inner.db.lock().expect("db poisoned");
            load_federated_local_listings(&conn).unwrap_or_default()
        };
        if entries.is_empty() {
            eprintln!("[api-market-fed] 补推零条目（本节点无 federated 条目，不发帧）");
            return 0;
        }
        for (i, entry) in entries.iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(FED_BACKFILL_SPACING).await;
            }
            (send_to)(peer, build_api_market_fed_payload(&node_hex, &node_name, entry));
        }
        // 条目数多时日志汇总（不逐条刷屏——每帧送达在对端 ingest 日志可见）。
        eprintln!(
            "[api-market-fed] 上线补推 {} 条联邦条目 → {}（限幅 {}/条）",
            entries.len(),
            short_node_label(&peer.to_hex()),
            FED_BACKFILL_SPACING.as_millis()
        );
        entries.len()
    }

    /// 定期重播一轮（内部常驻任务每 [`FED_REPLAY_INTERVAL`] 自动跑，测试直调）：
    /// 本节点全部 federated 条目走**两个相位**重播——① broadcast 面对**当前已
    /// 连接** peer（已连接集合由通道实现侧解析：生产 `fed_broadcast` 过滤
    /// connected + 本地指纹；fake overlay 测试互连直投）；② **定向补播面**对
    /// 目标集（node-meta Active ∖ connected——中继可达但无常驻连接的节点，
    /// Spark 类）逐条 `send_to`（2026-09-03 真机跟进：中继路由不产生常驻
    /// Conn，广播/watcher 够不着这类节点）。同快照被对端 seen 缓存 Duplicate
    /// 拦截零成本；快照变则 Refreshed。返回重播条目数（0 = 无 federated 条目，
    /// 零帧）。
    pub async fn replay_round(&self) -> usize {
        replay_round_inner(&self.inner).await
    }

    /// 注入定期重播周期（测试专用：缩短到亚秒快速验常驻任务接线；生产缺省
    /// [`FED_REPLAY_INTERVAL`] 30 分钟）。任务每轮循环现读，注入即时生效。
    pub fn set_replay_interval_for_test(&self, interval: Duration) {
        *self
            .inner
            .replay_interval
            .lock()
            .expect("api-market fed replay interval poisoned") = interval;
    }

    /// 接收端：解析联邦载荷 → 去重 → 幂等合并写本地 api_market（无验签发送方
    /// 的旧入口——`source_node_id` 留空；生产桥分发走 [`Self::dispatch`]，它
    /// 用验签 `msg.from` 记录来源 NodeID）。
    ///
    /// 载荷契约 `{"fed":"api_market_lobby","node":<来源节点>,"node_id":<来源
    /// NodeID hex>?,"entry":{ApiListing}}`：
    /// - 非 api_market_lobby / **node 字段整体缺失** / entry 解析失败 / 必填
    ///   缺失（id/api_name/endpoint_url/publisher_pubkey，endpoint 须
    ///   http(s)）→ `Invalid`（每个拒绝分支都落日志——2026-09-03 观测性
    ///   修复，真机排查不再有静默丢弃）；
    /// - **匿名节点收下**：`node="peer"`（发送端 NEXOS_P2P_NAME 未设的
    ///   sanitize 回退，2026-09-03 真机跟进——此前按"缺 node"拒收导致匿名
    ///   发送端静默全丢）→ 正常 Written/Refreshed，物理归因靠
    ///   `source_node_id`（验签 NodeID），匿名多节点靠 NodeID 兜底防碰撞；
    /// - **完全相同载荷**（node+id+快照串缓存命中）→ `Duplicate`（不触碰 DB）；
    /// - 去重键：先 `id`（主键），无则 `api_name+publisher_pubkey`（唯一索引）
    ///   ——本地无条目 → 写入（`source_node=node`、本地 download_count 清零起步）
    ///   → `Written`；
    /// - 已有条目且同 source_node（同源重发=对端刷新快照）→ 覆盖刷新（沿用
    ///   本地条目 id），保留本地 `download_count` → `Refreshed`；
    /// - 已有条目但来源不同（本机先发布或他节点先到；**或**同名不同 NodeID——
    ///   节点名可撞，物理节点以验签 NodeID 为准）→ `Skipped`（保护本地）。
    ///
    /// 删除不撤远端：本端下架不广播任何撤销载荷，远端副本由源节点重新
    /// publish+federate 刷新（照 NexHub 语义，见模块文档）。
    pub fn ingest(&self, payload: &serde_json::Value) -> ApiMarketFedIngest {
        self.ingest_inner(None, None, payload)
    }

    /// 带验签发送方的 ingest（FederationBridge `dispatch` 调用）：`from` 是
    /// os-p2p 验签出的发送方 NodeID（不可伪造）——写入条目的
    /// `source_node_id`；载荷自报 `node_id` 与之不符时以验签值为准。
    pub fn ingest_from(&self, from: &os_p2p::NodeId, payload: &serde_json::Value) -> ApiMarketFedIngest {
        self.ingest_inner(Some(from), None, payload)
    }

    /// ingest 内核（`verified` = 验签发送方；`fallback_node_id` = 载荷自报
    /// node_id——旧调用方无验签面时兜底记录）。
    fn ingest_inner(
        &self,
        verified: Option<&os_p2p::NodeId>,
        fallback_node_id: Option<&str>,
        payload: &serde_json::Value,
    ) -> ApiMarketFedIngest {
        // 2026-09-03 真机跟进：Invalid 早退分支此前全部静默——中继帧已送达、
        // bridge 已分发，但接收端丢弃无任何日志（真机现象：观测日志见帧、
        // 无 api-market-fed 日志、表空）。现在每个拒绝分支都落日志（journalctl
        // 可直接定位丢弃原因）。
        if payload.get("fed").and_then(|v| v.as_str()) != Some(FED_KIND_API_MARKET_LOBBY) {
            eprintln!(
                "[api-market-fed] 丢弃载荷（fed kind 非 api_market_lobby: {:?}）",
                payload.get("fed").and_then(|v| v.as_str())
            );
            return ApiMarketFedIngest::Invalid;
        }
        let node = sanitize_fed_node(
            payload
                .get("node")
                .and_then(|v| v.as_str())
                .unwrap_or_default(),
        );
        // 匿名节点（node="peer"，NEXOS_P2P_NAME 未设的发送端经 sanitize 回退）
        // **收下**——2026-09-03 真机跟进：此前按"缺 node"拒收，导致未设节点名
        // 的发送端（IM 同场景接受 "peer"，im.rs 同款回退）联邦条目被静默丢弃
        // ——"IM 能通、市场收不到"的根因。物理归因不依赖节点名：source_node_id
        // 是验签 NodeID，same_origin 判定对匿名多节点有 NodeID 兜底（同 "peer"
        // 名不同 NodeID → 异源保护），无碰撞风险。字段整体缺失仍是非法载荷。
        if payload.get("node").is_none() {
            eprintln!("[api-market-fed] 丢弃载荷（缺 node 字段）");
            return ApiMarketFedIngest::Invalid;
        }
        let Some(entry_val) = payload.get("entry") else {
            eprintln!("[api-market-fed] 丢弃载荷（缺 entry 字段）← {node}");
            return ApiMarketFedIngest::Invalid;
        };
        let Ok(mut entry) = serde_json::from_value::<ApiListing>(entry_val.clone()) else {
            eprintln!(
                "[api-market-fed] 丢弃载荷（entry 解析失败: {:?}）← {node}",
                serde_json::from_value::<ApiListing>(entry_val.clone()).err()
            );
            return ApiMarketFedIngest::Invalid;
        };
        // 必填校验（与本地发布同规则——联邦面不放宽：endpoint 须 http(s)、
        // 归因三件套齐全；缺 model_name 不拦——远程快照的硬件/模型字段由源
        // 节点发布路径校验过，接收端只验身份与地址合法性）。
        if entry.id.trim().is_empty()
            || entry.api_name.trim().is_empty()
            || entry.publisher_pubkey.trim().is_empty()
            || !(entry.endpoint_url.starts_with("http://")
                || entry.endpoint_url.starts_with("https://"))
        {
            eprintln!(
                "[api-market-fed] 丢弃载荷（必填缺失/非法：id 空={} api_name 空={} pubkey 空={} endpoint={:?}）← {node}",
                entry.id.trim().is_empty(),
                entry.api_name.trim().is_empty(),
                entry.publisher_pubkey.trim().is_empty(),
                entry.endpoint_url
            );
            return ApiMarketFedIngest::Invalid;
        }
        // 来源标记覆盖：条目自身的 source_node（发送端恒 local）改写为发布节点；
        // source_node_id = 验签发送方（优先）/ 载荷自报 node_id（兜底，须合法
        // NodeID 形态）/ 空（直连语义——老对端发的载荷）。
        entry.source_node = node.clone();
        entry.source_node_id = verified
            .map(|n| n.to_hex())
            .or_else(|| {
                fallback_node_id
                    .and_then(os_p2p::NodeId::parse)
                    .map(|n| n.to_hex())
            })
            .unwrap_or_default();
        // 去重键含完整快照串：相同载荷（重放）→ Duplicate；新快照 → 穿透 DB。
        let snapshot = entry_val.to_string();
        let key = format!("api_market\u{0}{node}\u{0}{}\u{0}{snapshot}", entry.id);
        {
            let mut seen = self.inner.seen.lock().expect("api-market fed seen poisoned");
            if seen.contains(&key) {
                eprintln!(
                    "[api-market-fed] 重复载荷丢弃 {} ← {node}（重放）",
                    entry.api_name
                );
                return ApiMarketFedIngest::Duplicate;
            }
            seen.push_back(key.clone());
            while seen.len() > FED_SEEN_LIMIT {
                seen.pop_front();
            }
        }
        let conn = self.inner.db.lock().expect("db poisoned");
        // 去重键一：id（主键）；无则键二：api_name+publisher_pubkey（唯一索引）。
        let existing = match find_by_id(&conn, &entry.id) {
            Ok(Some(old)) => Some(old),
            Ok(None) => find_by_name_owner(&conn, &entry.api_name, &entry.publisher_pubkey)
                .ok()
                .flatten(),
            Err(e) => {
                // DB 读失败 ≠ 载荷问题：撤掉刚压入的 seen 键（否则同载荷后续
                // 重放全变 Duplicate，掩盖真实故障——2026-09-03 观测性修复），
                // 落日志后按 Invalid 返回（可重试）。
                drop(conn);
                {
                    let mut seen =
                        self.inner.seen.lock().expect("api-market fed seen poisoned");
                    if seen.back().is_some_and(|k| *k == key) {
                        seen.pop_back();
                    }
                }
                eprintln!("[api-market-fed] 丢弃载荷（DB 读失败: {e}）← {node}");
                return ApiMarketFedIngest::Invalid;
            }
        };
        // 同源判定：来源节点名相同 **且** 双方 source_node_id 相容（其一为空=
        // 老条目/老对端，宽容；都非空且不等=同名不同物理节点 → 异源保护）。
        let same_origin = existing.as_ref().is_some_and(|old| {
            old.source_node == node
                && (old.source_node_id.is_empty()
                    || entry.source_node_id.is_empty()
                    || old.source_node_id == entry.source_node_id)
        });
        match existing {
            None => {
                // 新条目：本地消费计数从 0 起步（对端的计数是它的活跃度）。
                entry.download_count = 0;
                insert_listing(&conn, &entry).map_or_else(
                    |e| {
                        // 写失败留痕（表满/IO 故障——静默会掩盖部署问题）。
                        eprintln!(
                            "[api-market-fed] 写入失败（DB: {e}）: {} ← {node}",
                            entry.api_name
                        );
                        ApiMarketFedIngest::Invalid
                    },
                    |_| {
                        eprintln!(
                            "[api-market-fed] 收远程条目 {name} ← {node}（{}）",
                            short_node_label(&entry.source_node_id),
                            name = entry.api_name
                        );
                        ApiMarketFedIngest::Written
                    },
                )
            }
            Some(_) if same_origin => {
                // 同源重发 = 对端刷新快照：沿用本地 id（唯一索引主键稳定），
                // 保留本地 download_count；心跳/负载取源端最新快照（heartbeat
                // 在发布节点上跑，源端视角更新）。
                let old = existing.as_ref().expect("same_origin implies Some");
                entry.download_count = old.download_count;
                entry.id = old.id.clone();
                insert_listing(&conn, &entry).map_or_else(
                    |e| {
                        eprintln!(
                            "[api-market-fed] 刷新写入失败（DB: {e}）: {} ← {node}",
                            entry.api_name
                        );
                        ApiMarketFedIngest::Invalid
                    },
                    |_| {
                        eprintln!(
                            "[api-market-fed] 收远程刷新 {name} ← {node}（保留本地计数）",
                            name = entry.api_name
                        );
                        ApiMarketFedIngest::Refreshed
                    },
                )
            }
            Some(_) => {
                eprintln!(
                    "[api-market-fed] 跳过远程条目 {} ← {node}（本地已有同 id/同名条目，来源受保护）",
                    entry.api_name
                );
                ApiMarketFedIngest::Skipped
            }
        }
    }

    // ------------------------------------------------------------------
    // 跨网中继（api_relay_req / api_relay_resp）——消费者侧调用面
    // ------------------------------------------------------------------

    /// 网络入口（FederationBridge 分发）：按 `payload.fed` 路由——
    /// api_market_lobby → 带验签来源的幂等合并；api_relay_req → 源端代发
    /// （白名单裁决）；api_relay_resp → 消费端事件回填。其余忽略。
    pub fn dispatch(&self, from: &os_p2p::NodeId, payload: &serde_json::Value) {
        match payload.get("fed").and_then(|v| v.as_str()) {
            Some(FED_KIND_API_MARKET_LOBBY) => {
                self.ingest_from(from, payload);
            }
            Some(FED_KIND_API_RELAY_REQ) => {
                self.handle_relay_req(from, payload);
            }
            Some(FED_KIND_API_RELAY_RESP) => {
                self.ingest_relay_resp(from, payload);
            }
            _ => {}
        }
    }

    /// 消费者侧发起一次中继：注册 pending（req_id 关联）→ 按 1 MiB 分块发
    /// `api_relay_req` 帧 → 返回事件流 Receiver。调用方用
    /// [`Self::relay_roundtrip`]（整包聚合）/ [`Self::relay_open_stream`]（流式）
    /// 的封装，不直接用本方法。
    fn relay_call(
        &self,
        via_node_hex: &str,
        req: ApiRelayRequest,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<Result<ApiRelayEvent, String>>, String> {
        let target = os_p2p::NodeId::parse(via_node_hex.trim())
            .ok_or("via_node 非法（应为 0x+66 hex NodeID）")?;
        let req_id = new_uuid();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        // pending 容量防线（防泄漏/滥用撑爆关联表）。
        {
            let mut pending = self.inner.pending.lock().expect("api-market fed pending poisoned");
            if pending.len() >= 256 {
                return Err("中继并发请求过多（pending 关联表满）".into());
            }
            pending.insert(
                req_id.clone(),
                PendingRelay {
                    target: target.clone(),
                    tx,
                    last_seen: std::time::Instant::now(),
                },
            );
        }
        let (send_to, _node_hex) = {
            let guard = self
                .inner
                .transport
                .lock()
                .expect("api-market fed transport poisoned");
            let Some(t) = guard.as_ref() else {
                // 通道未装配：撤掉 pending 再报错（不留孤儿关联）。
                self.inner
                    .pending
                    .lock()
                    .expect("api-market fed pending poisoned")
                    .remove(&req_id);
                return Err("P2P 通道未装配（NEXOS_P2P_ENABLE=1 且对端组网后可用）".into());
            };
            if t.node_hex.is_empty() {
                self.inner
                    .pending
                    .lock()
                    .expect("api-market fed pending poisoned")
                    .remove(&req_id);
                return Err("P2P 通道未装配定向面（set_p2p 全量装配后可用）".into());
            }
            (t.send_to.clone(), t.node_hex.clone())
        };
        // 分块发帧：帧 0 带方法/URL/头/流式标志 + 首块；余帧只带块。
        let body = req.body.unwrap_or_default();
        let chunks: Vec<&[u8]> = if body.is_empty() {
            vec![&[][..]]
        } else {
            body.chunks(RELAY_CHUNK_BYTES).collect()
        };
        let cn = u32::try_from(chunks.len()).map_err(|_| "请求体过大")?;
        for (ci, chunk) in chunks.iter().enumerate() {
            let mut frame = serde_json::json!({
                "fed": FED_KIND_API_RELAY_REQ,
                "req_id": req_id,
                "method": req.method,
                "url": req.url,
                "headers": serde_json::Value::Object(headers_to_json_map(&req.headers)),
                "stream": req.stream,
                "ci": ci,
                "cn": cn,
            });
            if ci == 0 || !chunk.is_empty() {
                frame["body_b64"] = serde_json::Value::String(
                    base64::engine::general_purpose::STANDARD.encode(chunk),
                );
            }
            (send_to)(&target, frame);
        }
        eprintln!(
            "[api-market-relay] → {} {}（{} 帧，stream={}）",
            short_node_label(via_node_hex),
            req.url,
            cn,
            req.stream
        );
        Ok(rx)
    }

    /// 整包中继（非流式）：发 req → 等首帧（status/headers）→ 聚合全部块
    /// 直到 done。`timeout` 由调用方按语义给（test≈10s、chat≈120s；缺省语义
    /// 见 [`RelayLimits::req_timeout`]）——**整体预算**（deadline 制，非逐段
    /// 续期）。注意非流式源端在**读完上游**后才发首帧，故首帧窗口=整包预算
    /// （[`RelayLimits::stream_first`] 只约束流式 Head）。聚合上限
    /// [`RelayLimits::max_resp_body`]，超限 Err。
    pub async fn relay_roundtrip(
        &self,
        via_node_hex: &str,
        req: ApiRelayRequest,
        timeout: Duration,
    ) -> Result<RelayComplete, String> {
        let limits = *self
            .inner
            .limits
            .lock()
            .map_err(|_| "内部状态锁中毒")?;
        let mut rx = self.relay_call(via_node_hex, req)?;
        let deadline = tokio::time::Instant::now() + timeout;
        let budget = || deadline.saturating_duration_since(tokio::time::Instant::now());
        // 首帧 = 上游整包完成（非流式源端读完全量才回）。
        let first = tokio::time::timeout(budget(), rx.recv())
            .await
            .map_err(|_| format!("中继超时（{}s 内源节点未应答）", timeout.as_secs()))?
            .ok_or("中继通道关闭（源节点无应答）")?;
        let (status, headers) = match first? {
            ApiRelayEvent::Head { status, headers } => (status, headers),
            _ => return Err("协议错误：首帧不是 Head".into()),
        };
        let mut body: Vec<u8> = Vec::new();
        loop {
            match tokio::time::timeout(budget(), rx.recv()).await {
                Err(_) => {
                    return Err(format!("中继超时（{}s 未收完响应）", timeout.as_secs()))
                }
                Ok(None) => return Err("中继通道在收完前关闭".into()),
                Ok(Some(Err(e))) => return Err(e),
                Ok(Some(Ok(ApiRelayEvent::Chunk(b)))) => {
                    if body.len() + b.len() > limits.max_resp_body {
                        return Err(format!(
                            "中继响应超过聚合上限（{} MiB）",
                            limits.max_resp_body / 1024 / 1024
                        ));
                    }
                    body.extend_from_slice(&b);
                }
                Ok(Some(Ok(ApiRelayEvent::Done))) => {
                    return Ok(RelayComplete { status, headers, body })
                }
                Ok(Some(Ok(ApiRelayEvent::Head { .. }))) => {
                    return Err("协议错误：重复 Head 帧".into())
                }
            }
        }
    }

    /// 流式中继：发 req（stream=true）→ 等首帧（status/headers，
    /// [`RelayLimits::stream_first`] 窗口）→ 返回 [`RelayStream`]（后续块
    /// 逐块取，空闲超时内部执行）。上游非 2xx 时**照样返回**（status 在
    /// Head 里）——调用方自行裁决（与直连路径同一语义：非 2xx 读 body 报错）。
    pub async fn relay_open_stream(
        &self,
        via_node_hex: &str,
        req: ApiRelayRequest,
        first_timeout: Duration,
    ) -> Result<RelayStream, String> {
        let limits = *self
            .inner
            .limits
            .lock()
            .map_err(|_| "内部状态锁中毒")?;
        let mut rx = self.relay_call(via_node_hex, req)?;
        let first = tokio::time::timeout(first_timeout, rx.recv())
            .await
            .map_err(|_| format!("中继首帧超时（{}s 无响应头）", first_timeout.as_secs()))?
            .ok_or("中继通道关闭（源节点无应答）")?;
        let (status, headers) = match first? {
            ApiRelayEvent::Head { status, headers } => (status, headers),
            _ => return Err("协议错误：首帧不是 Head".into()),
        };
        Ok(RelayStream {
            status,
            headers,
            rx,
            idle: limits.stream_idle,
        })
    }

    // ------------------------------------------------------------------
    // 跨网中继——源端服务面（白名单裁决 + 真实代发）
    // ------------------------------------------------------------------

    /// 源端收到 `api_relay_req`：解析/重组（cn>1 多帧分块）→ 白名单裁决 →
    /// 真实 reqwest 代发 → resp 帧回传（流式逐块）。重活 spawn 异步干
    /// （bridge dispatch 是同步上下文）。
    fn handle_relay_req(&self, from: &os_p2p::NodeId, payload: &serde_json::Value) {
        let limits = *self.inner.limits.lock().expect("api-market fed limits poisoned");
        sweep_relay_state(&self.inner, &limits);
        let Some(req_id) = payload.get("req_id").and_then(|v| v.as_str()) else {
            return; // 无 req_id 无法关联——丢弃（记日志）
        };
        let req_id = req_id.to_string();
        let ci = payload.get("ci").and_then(|v| v.as_u64()).unwrap_or(0);
        let cn = payload.get("cn").and_then(|v| v.as_u64()).unwrap_or(1);
        // 上限防线：cn ≤ 32（32 MiB 请求上限）/ ci 在界内。
        if !(1..=32).contains(&cn) || ci >= cn {
            eprintln!("[api-market-relay] 丢弃非法分块参数 req_id={req_id} ci={ci} cn={cn}");
            return;
        }
        let Some(b64) = payload.get("body_b64").and_then(|v| v.as_str()) else {
            eprintln!("[api-market-relay] 帧缺 body_b64 req_id={req_id}");
            return;
        };
        let Ok(chunk) = base64::engine::general_purpose::STANDARD.decode(b64) else {
            eprintln!("[api-market-relay] 帧坏 base64 req_id={req_id}");
            return;
        };
        if ci == 0 {
            let method = payload
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim()
                .to_ascii_uppercase();
            let url = payload
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim()
                .to_string();
            let stream = payload.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
            let headers = payload
                .get("headers")
                .and_then(|v| v.as_object())
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if cn == 1 {
                // 单帧请求：直接执行（绝大多数场景——chat/test 请求体远小于 1 MiB）。
                self.spawn_relay_execute(
                    RelayJob {
                        from: from.clone(),
                        req_id,
                        method,
                        url,
                        headers,
                        stream,
                        body: chunk,
                    },
                    &limits,
                );
            } else {
                // 多帧首帧：建重组缓冲，等余块。
                let mut partials = self
                    .inner
                    .partial_reqs
                    .lock()
                    .expect("api-market fed partial poisoned");
                if partials.len() >= 16 && !partials.contains_key(&req_id) {
                    eprintln!("[api-market-relay] 丢弃 req_id={req_id}（重组缓冲满）");
                    return;
                }
                let parts: Vec<Option<Vec<u8>>> = (0..cn).map(|i| (i == 0).then(|| chunk.clone())).collect();
                partials.insert(
                    req_id,
                    PartialRelayReq {
                        from: from.clone(),
                        method,
                        url,
                        headers,
                        stream,
                        parts,
                        received: 1,
                        total_bytes: chunk.len(),
                        last_seen: std::time::Instant::now(),
                    },
                );
            }
            return;
        }
        // 后续帧：填入重组缓冲，齐了才执行。
        let ready = {
            let mut partials = self
                .inner
                .partial_reqs
                .lock()
                .expect("api-market fed partial poisoned");
            let Some(p) = partials.get_mut(&req_id) else {
                eprintln!("[api-market-relay] 孤儿分块帧 req_id={req_id} ci={ci}");
                return;
            };
            if p.parts.len() != cn as usize || p.parts.get(ci as usize).is_some_and(Option::is_some) {
                eprintln!("[api-market-relay] 分块帧重复/错位 req_id={req_id} ci={ci}");
                partials.remove(&req_id);
                return;
            }
            p.total_bytes += chunk.len();
            if p.total_bytes > limits.max_req_body {
                eprintln!("[api-market-relay] 丢弃 req_id={req_id}（请求体超上限）");
                partials.remove(&req_id);
                return;
            }
            p.parts[ci as usize] = Some(chunk);
            p.received += 1;
            p.last_seen = std::time::Instant::now();
            if p.received == p.parts.len() {
                // 齐块：按下标序拼回完整 body（分块即字节序，无重排）。
                let mut done = partials.remove(&req_id).expect("received==len 前刚 get_mut 命中");
                let body: Vec<u8> = std::mem::take(&mut done.parts)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .collect();
                Some((done, body))
            } else {
                None
            }
        };
        if let Some((p, body)) = ready {
            self.spawn_relay_execute(
                RelayJob {
                    from: p.from,
                    req_id,
                    method: p.method,
                    url: p.url,
                    headers: p.headers,
                    stream: p.stream,
                    body,
                },
                &limits,
            );
        }
    }

    /// 执行一次代发（白名单 → reqwest → resp 帧回传）；spawn 到后台
    /// （bridge dispatch 同步上下文不阻塞观测 task）。
    fn spawn_relay_execute(&self, job: RelayJob, limits: &RelayLimits) {
        let inner = self.inner.clone();
        let limits = *limits;
        tokio::spawn(async move {
            relay_execute_and_reply(inner, job, limits).await;
        });
    }

    /// 消费端收到 `api_relay_resp`：按 req_id 回填事件流（发送方必须等于
    /// pending 定向目标——第三方伪造应答无效）。
    fn ingest_relay_resp(&self, from: &os_p2p::NodeId, payload: &serde_json::Value) {
        let Some(req_id) = payload.get("req_id").and_then(|v| v.as_str()) else {
            return;
        };
        let done = payload.get("done").and_then(|v| v.as_bool()).unwrap_or(false);
        let error = payload.get("error").and_then(|v| v.as_str());
        let seq = payload.get("seq").and_then(|v| v.as_u64()).unwrap_or(0);
        let chunk = payload
            .get("chunk_base64")
            .and_then(|v| v.as_str())
            .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok());
        let mut pending = self.inner.pending.lock().expect("api-market fed pending poisoned");
        let Some(p) = pending.get_mut(req_id) else {
            return; // 未知/已清 req_id——迟到帧，忽略
        };
        if p.target != *from {
            eprintln!(
                "[api-market-relay] 丢弃伪造应答 req_id={req_id}（发送方 != 定向目标）"
            );
            return;
        }
        p.last_seen = std::time::Instant::now();
        let tx = p.tx.clone();
        let mut finished = false;
        if let Some(e) = error {
            let _ = tx.send(Err(e.to_string()));
            finished = true;
        } else {
            if seq == 0 {
                // 首帧：status + headers（Head 事件）；带块则紧跟 Chunk。
                let status = payload.get("status").and_then(|v| v.as_u64()).unwrap_or(502) as u16;
                let headers = payload
                    .get("headers")
                    .and_then(|v| v.as_object())
                    .map(|m| {
                        m.iter()
                            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let _ = tx.send(Ok(ApiRelayEvent::Head { status, headers }));
            }
            if let Some(b) = chunk {
                if !b.is_empty() {
                    let _ = tx.send(Ok(ApiRelayEvent::Chunk(b)));
                }
            }
            if done {
                let _ = tx.send(Ok(ApiRelayEvent::Done));
                finished = true;
            }
        }
        if finished {
            pending.remove(req_id);
        }
    }
}

// ----------------------------------------------------------------------------
// 跨网中继自由函数（白名单归一化 / 源端代发执行 / 状态巡检——可单测）
// ----------------------------------------------------------------------------

/// NodeID 短式（`0x1234…cdef`——错误信息/日志展示用，与 main.rs 同款）。
#[must_use]
pub fn short_node_label(hex: &str) -> String {
    let n = hex.len();
    if n <= 12 {
        hex.to_string()
    } else {
        format!("{}…{}", &hex[..8], &hex[n - 4..])
    }
}

/// URL 归一化（白名单比对基准）：`reqwest::Url` 解析（解析过程即做点段
/// 归并——`/v1/../metrics` 这类穿越形态会被还原，防前缀绕过）→ 去尾斜杠。
/// 解析失败 → None（不可归一化的 URL 一律不进白名单）。
#[must_use]
pub fn normalize_relay_url(url: &str) -> Option<String> {
    let u = reqwest::Url::parse(url.trim()).ok()?;
    if u.scheme() != "http" && u.scheme() != "https" {
        return None;
    }
    let mut s = u.to_string();
    while s.ends_with('/') {
        s.pop();
    }
    Some(s)
}

/// 白名单封闭集合：已发布条目 endpoint_url `E` 允许的请求 URL 集
/// `{E, E/models, E/chat/completions}`（各自归一化后精确比对）——覆盖
/// llm_external 的 test（`<base>/models`）与 chat（`<base>/chat/completions`）
/// 两条真实请求形态 + E 本身（endpoint 直填 chat 完整地址的发布形态）。
/// 不开放任意路径/任意主机——**绝不做开放代理**（红线）。
#[must_use]
pub fn relay_url_allowed(published: &[String], url: &str) -> bool {
    let Some(norm) = normalize_relay_url(url) else {
        return false;
    };
    published.iter().any(|e| {
        let Some(base) = normalize_relay_url(e) else {
            return false;
        };
        norm == base
            || norm == format!("{base}/models")
            || norm == format!("{base}/chat/completions")
    })
}

/// 源端白名单数据源：本节点**已发布**条目（source_node=local）的
/// endpoint_url 全集。联邦远程条目不参与（不二次转发——单跳语义）。
fn local_published_endpoints(conn: &Connection) -> Vec<String> {
    load_listings(conn, None, "recent")
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.status == "active" && listing_is_local(e))
        .map(|e| e.endpoint_url)
        .collect()
}

/// 头对 → JSON 对象（`(String, String)` → `Map<String, Value>`，中继帧
/// headers 字段的统一形态）。
fn headers_to_json_map(headers: &[(String, String)]) -> serde_json::Map<String, serde_json::Value> {
    headers
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect()
}

/// 源端执行代发并回传 resp 帧。
///
/// 顺序红线：**白名单先行**——URL 不属于本节点已发布条目（或方法非
/// GET/POST）→ 直接回 `{status:403, body:"该 URL 不属于本节点发布的条目"}`，
/// 不发任何外呼。白名单过了才组装真实 reqwest 请求（照 llm_external 的
/// 转发手法：鉴权头原样、总超时上限 600s；流式 `bytes_stream` 逐块回传）。
#[allow(unused_assignments)] // 尾帧 seq 自增在 return 分支无人读——宏展开固有
async fn relay_execute_and_reply(
    inner: Arc<ApiMarketFedInner>,
    job: RelayJob,
    _limits: RelayLimits,
) {
    let RelayJob { from, req_id, method, url, headers, stream, body } = job;
    // resp 发送面：定向回请求方（帧序即字节序，seq 单调递增）。
    let send_to = {
        let guard = inner
            .transport
            .lock()
            .expect("api-market fed transport poisoned");
        match guard.as_ref() {
            Some(t) => t.send_to.clone(),
            None => return, // 装配被撤（极端）：无处回帧
        }
    };
    // 纯函数构帧（seq 由调用侧自增——宏内做，避免 FnMut 闭包借用打架）。
    let mk_frame = |seq: u64,
                    status: Option<u16>,
                    resp_headers: Option<&[(String, String)]>,
                    chunk: Option<&[u8]>,
                    done: bool,
                    error: Option<&str>|
     -> serde_json::Value {
        let mut f = serde_json::json!({
            "fed": FED_KIND_API_RELAY_RESP,
            "req_id": req_id,
            "seq": seq,
            "done": done,
        });
        if let Some(s) = status {
            f["status"] = serde_json::json!(s);
        }
        if let Some(hs) = resp_headers {
            f["headers"] = serde_json::Value::Object(headers_to_json_map(hs));
        }
        if let Some(c) = chunk {
            f["chunk_base64"] = serde_json::Value::String(
                base64::engine::general_purpose::STANDARD.encode(c),
            );
        }
        if let Some(e) = error {
            f["error"] = serde_json::Value::String(e.to_string());
        }
        f
    };
    let mut seq: u64 = 0;
    macro_rules! push_frame {
        ($status:expr, $headers:expr, $chunk:expr, $done:expr, $error:expr) => {{
            let f = mk_frame(seq, $status, $headers, $chunk, $done, $error);
            seq += 1;
            send_to(&from, f);
        }};
    }
    let mut reply_403 = |msg: &str| {
        eprintln!("[api-market-relay] 拒绝 {} {} ← {}（{}）", method, url, from.to_hex(), msg);
        push_frame!(Some(403u16), Some(&[][..]), Some(msg.as_bytes()), true, None);
    };
    // —— 红线一：方法仅 GET/POST。 ——
    if method != "GET" && method != "POST" {
        reply_403("仅支持 GET/POST 中继");
        return;
    }
    // —— 红线二：URL 白名单（本节点已发布条目的封闭集合）。 ——
    let parsed_url = match reqwest::Url::parse(url.trim()) {
        Ok(u) if u.scheme() == "http" || u.scheme() == "https" => u,
        _ => {
            reply_403("目标 URL 非法");
            return;
        }
    };
    let allowed = {
        let conn = inner.db.lock().expect("db poisoned");
        local_published_endpoints(&conn)
    };
    if !relay_url_allowed(&allowed, &url) {
        reply_403("该 URL 不属于本节点发布的条目");
        return;
    }
    // —— 组装真实请求（照 llm_external 转发手法）。 ——
    let method_reqwest = reqwest::Method::from_bytes(method.as_bytes())
        .unwrap_or(reqwest::Method::GET);
    let mut req = reqwest::Request::new(method_reqwest, parsed_url);
    // 头透传，剥 hop-by-hop（Host/Content-Length 由 reqwest 自管）。
    for (k, v) in &headers {
        let lk = k.trim().to_ascii_lowercase();
        if matches!(
            lk.as_str(),
            "host" | "connection" | "content-length" | "transfer-encoding" | "keep-alive" | "upgrade"
        ) {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(k.trim().as_bytes()),
            reqwest::header::HeaderValue::from_str(v),
        ) {
            req.headers_mut().insert(name, value);
        }
    }
    *req.body_mut() = Some(reqwest::Body::from(body));
    // 源端总超时上限（reqwest 0.12 execute 无 builder 面——Request 自带超时槽）。
    *req.timeout_mut() = Some(RELAY_UPSTREAM_TOTAL);
    let resp = match HTTP.execute(req).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[api-market-relay] 代发失败 {} ← {}: {e}", url, from.to_hex());
            push_frame!(None, None, None, true, Some(&format!("上游请求失败: {e}")));
            return;
        }
    };
    let status = resp.status().as_u16();
    let resp_headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|s| (k.as_str().to_string(), s.to_string()))
        })
        .collect();
    eprintln!(
        "[api-market-relay] 代发 {} {} → {}（stream={stream}）",
        method, url, status
    );
    if !stream {
        // 整包：读 body，按 1 MiB 分块回帧（首帧带 status/headers，尾帧 done）。
        let body_bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                push_frame!(None, None, None, true, Some(&format!("上游读体失败: {e}")));
                return;
            }
        };
        if body_bytes.is_empty() {
            push_frame!(Some(status), Some(&resp_headers), None, true, None);
            return;
        }
        let chunks: Vec<&[u8]> = body_bytes.chunks(RELAY_CHUNK_BYTES).collect();
        for (i, c) in chunks.iter().enumerate() {
            let last = i == chunks.len() - 1;
            push_frame!(
                if i == 0 { Some(status) } else { None },
                if i == 0 { Some(&resp_headers) } else { None },
                Some(c),
                last,
                None
            );
        }
        return;
    }
    // 流式：首帧只带 status/headers（content-type 供消费者透传）；逐块回帧，
    // 尾帧 done=true。上游非 2xx 时读错误体（≤300 字符口径同直连路径）单帧收尾。
    use futures::StreamExt;
    if !resp.status().is_success() {
        let detail = match tokio::time::timeout(Duration::from_secs(5), resp.text()).await {
            Ok(Ok(t)) => t.chars().take(300).collect::<String>(),
            _ => String::new(),
        };
        push_frame!(
            Some(status),
            Some(&resp_headers),
            Some(detail.as_bytes()),
            true,
            None
        );
        return;
    }
    push_frame!(Some(status), Some(&resp_headers), None, false, None);
    let mut upstream = resp.bytes_stream();
    loop {
        match upstream.next().await {
            Some(Ok(chunk)) => {
                if !chunk.is_empty() {
                    push_frame!(None, None, Some(&chunk), false, None);
                }
            }
            Some(Err(e)) => {
                eprintln!("[api-market-relay] 上游流中断: {e}");
                push_frame!(None, None, None, true, Some(&format!("上游流中断: {e}")));
                return;
            }
            None => {
                push_frame!(None, None, None, true, None);
                return;
            }
        }
    }
}

/// 定期重播一轮的内核（[`ApiMarketFedEndpoint::replay_round`] 与端点内的
/// 常驻任务共用）：快照本节点全部 federated 条目 → 两个发送相位：
///
/// ① **广播相位**：逐条走 broadcast 面（fan-out 给当前已连接 peer）；
/// ② **定向补播相位**（2026-09-03 真机跟进：中继节点覆盖）：对目标集
///    （[`FedKnownActiveFn`] 枚举 = node-meta Active ∖ connected ∖ 本机指纹）
///    逐条 `send_to` 定向补播——中继路由按需逐消息送达，不产生常驻 Conn
///    （Spark 类对端 connected 恒 false，广播/连接 watcher 都够不着）。
///
/// 两相位均逐条限幅 [`FED_BACKFILL_SPACING`]。幂等语义：同快照重放在对端
/// seen 缓存命中 → Duplicate（零 DB 触碰）；快照变（心跳/负载/重新
/// federate）→ 键不同 → 穿透到 DB 权威路径 Refreshed。未装配通道 / 无
/// federated 条目 → 零帧。
async fn replay_round_inner(inner: &Arc<ApiMarketFedInner>) -> usize {
    let (broadcast, send_to, known_active, node_hex, node_name) = {
        let guard = inner
            .transport
            .lock()
            .expect("api-market fed transport poisoned");
        let Some(t) = guard.as_ref() else {
            return 0; // P2P 未装配：零开销跳过
        };
        (
            t.broadcast.clone(),
            t.send_to.clone(),
            t.known_active.clone(),
            t.node_hex.clone(),
            t.node_name.clone(),
        )
    };
    let entries = {
        let conn = inner.db.lock().expect("db poisoned");
        load_federated_local_listings(&conn).unwrap_or_default()
    };
    if entries.is_empty() {
        eprintln!("[api-market-fed] 重播零条目（本节点无 federated 条目，不发帧）");
        return 0;
    }
    // ① 广播相位：当前已连接 peer（fed_broadcast 语义——本地指纹在通道内过滤）。
    for (i, entry) in entries.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(FED_BACKFILL_SPACING).await;
        }
        (broadcast)(build_api_market_fed_payload(&node_hex, &node_name, entry));
    }
    // ② 定向补播相位：无常驻连接的已知活跃节点（中继可达——Spark 类）。
    //    目标集枚举是异步命令往返，锁外 await（transport 锁早已释放）；
    //    node_hex 兜底过滤自指纹（生产闭包已过滤，fake 注入可能带——双防线）。
    let targets: Vec<os_p2p::NodeId> = match known_active {
        Some(f) => f()
            .await
            .into_iter()
            .filter(|id| node_hex.is_empty() || id.to_hex() != node_hex)
            .collect(),
        None => Vec::new(),
    };
    if !targets.is_empty() {
        for (i, entry) in entries.iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(FED_BACKFILL_SPACING).await;
            }
            let payload = build_api_market_fed_payload(&node_hex, &node_name, entry);
            for target in &targets {
                (send_to)(target, payload.clone());
            }
        }
        eprintln!(
            "[api-market-fed] 定向补播 {} 条联邦条目 × {} 个已知活跃节点（无常驻连接，send_to 按需路由）",
            entries.len(),
            targets.len()
        );
    }
    eprintln!(
        "[api-market-fed] 定期重播 {} 条联邦条目（广播=当前已连接 peer + 定向=已知活跃节点；同快照对端 Duplicate 拦截零成本）",
        entries.len()
    );
    entries.len()
}

/// 巡检：清理超时 pending（消费者侧，无活动超 req_timeout+stream_idle）与
/// 超时分块重组缓冲（源端，超 req_timeout）；顺带丢弃 send 失败的 pending
/// （对端已放弃）。
fn sweep_relay_state(inner: &Arc<ApiMarketFedInner>, limits: &RelayLimits) {
    let now = std::time::Instant::now();
    let pending_ttl = limits.req_timeout + limits.stream_idle;
    {
        let mut pending = inner.pending.lock().expect("api-market fed pending poisoned");
        pending.retain(|_id, p| {
            let alive = now.duration_since(p.last_seen) <= pending_ttl;
            if !alive {
                eprintln!("[api-market-relay] 清理超时 pending（无活动 > {pending_ttl:?}）");
            }
            alive
        });
    }
    {
        let mut partials = inner
            .partial_reqs
            .lock()
            .expect("api-market fed partial poisoned");
        partials.retain(|_id, p| {
            let alive = now.duration_since(p.last_seen) <= limits.req_timeout;
            if !alive {
                eprintln!("[api-market-relay] 清理超时分块请求缓冲");
            }
            alive
        });
    }
}

// ----------------------------------------------------------------------------
// RouteHandler 实现（6 条路由）
// ----------------------------------------------------------------------------

#[async_trait]
impl RouteHandler for ApiMarketRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            // 写端点一律 requires_auth=false：链上 token 在 handler 内自验
            //（同 IM 用户面/nexhub 模式——网关系统中间件不识别链上 token）。
            spec(HttpMethod::Post, PATH_PUBLISH, false),
            // 读端点公开（市场页匿名可逛）。
            spec(HttpMethod::Get, PATH_LIST, false),
            spec(HttpMethod::Get, PATH_DETAIL, false),
            spec(HttpMethod::Delete, PATH_UNLIST, false),
            spec(HttpMethod::Post, PATH_HEARTBEAT, false),
            spec(HttpMethod::Get, PATH_METRICS, false),
            // 联邦推送（链上 token + owner，两步联邦第二步）。
            spec(HttpMethod::Post, PATH_FEDERATE, false),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        let query = req.path.split('?').nth(1).unwrap_or("");
        match (req.method, segs.as_slice()) {
            // —— POST /api/v1/api-market/publish —— 挂牌/刷新（链上 token 必需）
            //    body: { api_name, description?, endpoint_url, pricing,
            //            metrics_url?, tags?, server_config?, access_info? }
            //    publisher = token 反查 pubkey（body 无自报字段，身份不可伪造）；
            //    同 api_name + 同 pubkey 重复发布 = 刷新（保留 id/created_at/
            //    download_count/heartbeat/federated/access_info（body 未带时））；
            //    不同 pubkey 同名 = 各自独立条目。两步联邦：publish 只写本地
            //    不广播（推送走 POST /:id/federate）。
            (HttpMethod::Post, ["api", "v1", "api-market", "publish"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                #[derive(serde::Deserialize)]
                struct PublishBody {
                    #[serde(default)]
                    api_name: Option<String>,
                    #[serde(default)]
                    description: Option<String>,
                    #[serde(default)]
                    endpoint_url: Option<String>,
                    #[serde(default)]
                    pricing: Option<Pricing>,
                    #[serde(default)]
                    metrics_url: Option<String>,
                    #[serde(default)]
                    tags: Option<Vec<String>>,
                    #[serde(default)]
                    server_config: Option<ServerConfig>,
                    /// 消费者接入信息（可选；重发布带则更新、缺省保留既有）。
                    #[serde(default)]
                    access_info: Option<AccessInfo>,
                }
                let body: PublishBody = match serde_json::from_value(req.body) {
                    Ok(b) => b,
                    Err(e) => return Ok(error_response(400, &format!("解析挂牌请求体失败: {e}"))),
                };
                let api_name = match body
                    .api_name
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                {
                    Some(v) => v,
                    None => {
                        return Ok(error_response(
                            400,
                            "api_name 必填（如 \"qwen3.5-9b chat\"）",
                        ))
                    }
                };
                let endpoint_url = match body
                    .endpoint_url
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                {
                    Some(v) => v,
                    None => {
                        return Ok(error_response(
                            400,
                            "endpoint_url 必填（消费者直连的完整地址，如 http://host:8080/api/v1/gateway/v1/chat/completions）",
                        ))
                    }
                };
                if !(endpoint_url.starts_with("http://") || endpoint_url.starts_with("https://")) {
                    return Ok(error_response(
                        400,
                        "endpoint_url 必须以 http:// 或 https:// 开头",
                    ));
                }
                let Some(pricing_input) = body.pricing else {
                    return Ok(error_response(
                        400,
                        "pricing 必填（{mode: free|per_token|per_image, price_per_1k_tokens?, currency?, note?}）",
                    ));
                };
                let pricing = match validate_pricing(&pricing_input) {
                    Ok(p) => p,
                    Err(e) => return Ok(error_response(400, &format!("pricing 非法: {e}"))),
                };
                // 服务器配置：本地硬件探测（spawn_blocking）+ body 字段覆盖。
                let probed = tokio::task::spawn_blocking(probe_server_config_blocking)
                    .await
                    .map_err(|e| {
                        ApiGatewayError::Internal(format!("硬件探测任务 join 失败: {e}"))
                    })?;
                let server_config = merge_server_config(body.server_config, probed);
                if server_config.model_name.is_none() {
                    return Ok(error_response(
                        400,
                        "server_config.model_name 必填：硬件探测只能补 gpu/cpu/ram，模型名必须由 body 携带",
                    ));
                }
                // 归因（链上 token 反查；无自报通道）。
                let publisher_display = caller.display_name;
                let publisher_pubkey = caller.pubkey;
                // 重复发布（同 api_name + 同 pubkey）= 刷新：保留 id/created_at/
                // download_count/heartbeat（计数与节点活跃度是历史事实，不随改价清零）
                // + federated 推送状态 + access_info（body 未带时——凭据不因改价丢）。
                let existing = {
                    let conn = self.db.lock().expect("db poisoned");
                    find_by_name_owner(&conn, &api_name, &publisher_pubkey).map_err(db_err)?
                };
                let refreshed = existing.is_some();
                let now = now_iso();
                // 接入信息：body 带则更新（规范化：trim/空→None/auth_header 缺省
                // 不回填——缺省语义在 curl 拼装端兜底）；缺省保留既有值。
                let access_info = body
                    .access_info
                    .map(normalize_access_info)
                    .or_else(|| existing.as_ref().map(|e| e.access_info.clone()))
                    .unwrap_or_default();
                let listing = ApiListing {
                    id: existing
                        .as_ref()
                        .map(|e| e.id.clone())
                        .unwrap_or_else(new_uuid),
                    api_name: api_name.clone(),
                    description: body
                        .description
                        .map(|s| s.trim().to_string())
                        .unwrap_or_default(),
                    endpoint_url,
                    publisher_pubkey: publisher_pubkey.clone(),
                    publisher_display,
                    server_config,
                    pricing,
                    metrics_url: body
                        .metrics_url
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                    tags: body.tags.unwrap_or_default(),
                    status: default_status_active(),
                    created_at: existing
                        .as_ref()
                        .map(|e| e.created_at.clone())
                        .unwrap_or(now),
                    heartbeat_at: existing.as_ref().and_then(|e| e.heartbeat_at.clone()),
                    load: existing.as_ref().and_then(|e| e.load),
                    download_count: existing.as_ref().map(|e| e.download_count).unwrap_or(0),
                    access_info,
                    // 本地发布恒 local（联邦来源标记只在接收端 ingest 改写）；
                    // source_node_id 本地恒空（无中继语义——自己是出口）。
                    source_node: default_source_node(),
                    source_node_id: String::new(),
                    // 两步联邦：新条目未推送；重发布保留既有推送状态（对端快照
                    // 以「重新推送」（/:id/federate）刷新——照 NexHub 语义）。
                    federated: existing.as_ref().map(|e| e.federated).unwrap_or(false),
                };
                {
                    let conn = self.db.lock().expect("db poisoned");
                    insert_listing(&conn, &listing).map_err(db_err)?;
                }
                // 发布响应只回发布者本人（token 刚验过 = publisher）→ access_info
                // 序列化即明文，无需脱敏。
                let mut out = to_value(&listing)?;
                out["refreshed"] = serde_json::json!(refreshed);
                Ok(if refreshed {
                    ok_json(out)
                } else {
                    ApiResponse {
                        status: 201,
                        body: out,
                        headers: serde_json::json!({}),
                    }
                })
            }

            // —— GET /api/v1/api-market —— 大厅列表（公开）
            //    ?q= 关键词（api_name/description/tags LIKE）；?sort=recent（默认，
            //    created_at 降序）| price（付费单价升序，免费垫底）；
            //    ?scope=all（默认，全量平铺——**向后兼容**：旧客户端拿到的仍是
            //    同一形态数组，元素新增 source_node/federated/access_info 字段）
            //    | local（仅本机发布）| fed（仅联邦远程条目）。
            //    access_info.api_key 按视角脱敏（publisher 本人/admin 明文）。
            (HttpMethod::Get, ["api", "v1", "api-market"]) => {
                let q = parse_query_str(query, "q")
                    .map(|s| url_decode(&s).trim().to_string())
                    .filter(|s| !s.is_empty());
                let sort = normalize_sort(parse_query_str(query, "sort").as_deref());
                let scope = normalize_scope(parse_query_str(query, "scope").as_deref());
                let mut list = {
                    let conn = self.db.lock().expect("db poisoned");
                    load_listings(&conn, q.as_deref(), sort).map_err(db_err)?
                };
                if scope != "all" {
                    // scope 过滤（local=source_node=="local"；fed=其余——远程条目）。
                    list.retain(|e| (scope == "local") == listing_is_local(e));
                }
                if sort == "price" {
                    // 价格排名基础：付费升序在前，免费垫底（stable——同价位保持新近在前）。
                    list.sort_by_key(|e| e.pricing.price_sort_key());
                }
                let mut out = serde_json::json!([]);
                for e in &list {
                    let mut v = to_value(e)?;
                    let reveal = self.access_info_revealed(&req, &e.publisher_pubkey);
                    apply_access_info_mask(&mut v, &e.access_info, reveal);
                    out.as_array_mut().expect("json! 数组必成功").push(v);
                }
                Ok(ok_json(out))
            }

            // —— GET /api/v1/api-market/:id —— 详情（公开；附心跳新鲜度派生字段；
            //    access_info.api_key 按视角脱敏——publisher 本人/admin 明文）
            (HttpMethod::Get, ["api", "v1", "api-market", id]) => {
                let listing = {
                    let conn = self.db.lock().expect("db poisoned");
                    find_by_id(&conn, id).map_err(db_err)?
                };
                let Some(listing) = listing else {
                    return Ok(error_response(404, &format!("挂牌条目不存在: {id}")));
                };
                let mut out = to_value(&listing)?;
                out["heartbeat_fresh"] =
                    serde_json::json!(listing.heartbeat_at.as_deref().is_some_and(heartbeat_fresh));
                let reveal = self.access_info_revealed(&req, &listing.publisher_pubkey);
                apply_access_info_mask(&mut out, &listing.access_info, reveal);
                Ok(ok_json(out))
            }

            // —— DELETE /api/v1/api-market/:id —— 下架（仅 owner pubkey；
            //    无 admin 回落——admin token 无链上身份，连 401 都过不了）。
            //    只删**本地**行：不广播撤销载荷，联邦远端副本不受影响（照 NexHub
            //    语义——远端由源节点重新 publish+federate 刷新，见模块文档）。
            (HttpMethod::Delete, ["api", "v1", "api-market", id]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let listing = {
                    let conn = self.db.lock().expect("db poisoned");
                    find_by_id(&conn, id).map_err(db_err)?
                };
                let Some(listing) = listing else {
                    return Ok(error_response(404, &format!("挂牌条目不存在: {id}")));
                };
                if listing.publisher_pubkey != caller.pubkey {
                    return Ok(forbidden_unlist());
                }
                {
                    let conn = self.db.lock().expect("db poisoned");
                    delete_listing(&conn, id).map_err(db_err)?;
                }
                Ok(ok_json(serde_json::json!({ "deleted": true, "id": id })))
            }

            // —— POST /api/v1/api-market/:id/heartbeat —— 心跳自报（链上 token + owner）
            //    body: { running_req?, waiting_req?, gpu_cache_usage?,
            //            tokens_per_sec?, latency_ms?, load_pct? }
            //    → 更新 heartbeat_at + load（规范化 6 键 JSON）。
            (HttpMethod::Post, ["api", "v1", "api-market", id, "heartbeat"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let listing = {
                    let conn = self.db.lock().expect("db poisoned");
                    find_by_id(&conn, id).map_err(db_err)?
                };
                let Some(listing) = listing else {
                    return Ok(error_response(404, &format!("挂牌条目不存在: {id}")));
                };
                if listing.publisher_pubkey != caller.pubkey {
                    return Ok(forbidden_heartbeat());
                }
                let load = LoadMetrics::from_json(&req.body);
                let heartbeat_at = now_iso();
                {
                    let conn = self.db.lock().expect("db poisoned");
                    update_heartbeat(&conn, id, &heartbeat_at, &load).map_err(db_err)?;
                }
                Ok(ok_json(serde_json::json!({
                    "ok": true,
                    "id": id,
                    "heartbeat_at": heartbeat_at,
                    "stale": false,
                    "load": load,
                })))
            }

            // —— GET /api/v1/api-market/:id/metrics —— 负载监控输出（公开）
            //    优先级：新鲜心跳（≤60s，零外呼）→ metrics_url 代拉（5s 超时，
            //    {metrics:{...}} 规范化）→ 降级 unreachable（附最后一次心跳数据若有）。
            (HttpMethod::Get, ["api", "v1", "api-market", id, "metrics"]) => {
                let listing = {
                    let conn = self.db.lock().expect("db poisoned");
                    find_by_id(&conn, id).map_err(db_err)?
                };
                let Some(listing) = listing else {
                    return Ok(error_response(404, &format!("挂牌条目不存在: {id}")));
                };
                // 1) 新鲜心跳：直接返回（stale:false）。
                if let Some(hb) = listing.heartbeat_at.as_deref() {
                    if heartbeat_fresh(hb) {
                        return Ok(ok_json(serde_json::json!({
                            "id": id,
                            "reachable": true,
                            "stale": false,
                            "source": "heartbeat",
                            "metrics": listing.load.unwrap_or_default(),
                            "ts": hb,
                        })));
                    }
                }
                // 2) 无新鲜心跳但挂了 metrics_url → 服务端代拉（stale:true——
                //    节点未自报，数据来自拉取而非心跳）。
                if let Some(url) = listing
                    .metrics_url
                    .as_deref()
                    .map(str::trim)
                    .filter(|u| !u.is_empty())
                {
                    return match self.fetch_metrics(url).await {
                        Ok(metrics) => Ok(ok_json(serde_json::json!({
                            "id": id,
                            "reachable": true,
                            "stale": true,
                            "source": "metrics_url",
                            "metrics": metrics,
                            "ts": now_iso(),
                        }))),
                        Err(e) => Ok(ok_json(serde_json::json!({
                            "id": id,
                            "reachable": false,
                            "stale": true,
                            "source": "metrics_url",
                            "metrics": listing.load,
                            "ts": listing.heartbeat_at,
                            "error": e,
                        }))),
                    };
                }
                // 3) 既无新鲜心跳也无 metrics_url：unreachable（附旧心跳数据若有）。
                Ok(ok_json(serde_json::json!({
                    "id": id,
                    "reachable": false,
                    "stale": true,
                    "source": "none",
                    "metrics": listing.load,
                    "ts": listing.heartbeat_at,
                })))
            }

            // —— POST /api/v1/api-market/:id/federate —— 推送/重新推送到联邦大厅
            //    （两步联邦第二步，照 NexHub 语义：联邦条目只能从**本地已发布
            //    条目**推送——不存在「直接发布到联邦」的路径）。
            //    权限：owner pubkey（发布者本人）；无 admin 回落（与市场写面
            //    语义一致——推送者必须是可验签的链上身份）。
            //    动作：条目置 federated=true 落库 + fed.broadcast_entry 广播最新
            //    快照；重复调用=重新推送（对端同源刷新，保留本地计数）。
            //    P2P 未装配时广播静默跳过，但 federated 标志仍置位（发布侧决策）。
            (HttpMethod::Post, ["api", "v1", "api-market", id, "federate"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let listing = {
                    let conn = self.db.lock().expect("db poisoned");
                    find_by_id(&conn, id).map_err(db_err)?
                };
                let Some(listing) = listing else {
                    return Ok(error_response(
                        404,
                        &format!("挂牌条目不存在: {id}（先发布到本地大厅再推送联邦）"),
                    ));
                };
                if listing.publisher_pubkey != caller.pubkey {
                    return Ok(forbidden_federate());
                }
                // 仅本地条目可推送（联邦远程副本不能再转发——防转发链与来源
                // 混淆；要推送就在源节点上推）。
                if !listing_is_local(&listing) {
                    return Ok(error_response(
                        403,
                        &format!(
                            "联邦远程条目（来自 {}）不可在本节点推送——请在源节点发布者处推送",
                            listing.source_node
                        ),
                    ));
                }
                let saved = {
                    let conn = self.db.lock().expect("db poisoned");
                    let mut e2 = listing.clone();
                    e2.federated = true;
                    insert_listing(&conn, &e2).map_err(db_err)?;
                    e2
                };
                let first_push = !listing.federated;
                // 广播最新快照（含 access_info——凭据随条目联邦分发，对端输出
                // 仍按各自视角脱敏；接收端明文仅其 publisher/admin 可见）。
                self.fed.broadcast_entry(&saved);
                Ok(ok_json(serde_json::json!({
                    "ok": true,
                    "id": id,
                    "action": "federate",
                    "federated": true,
                    "first_push": first_push,
                    "source_node": saved.source_node,
                    "note": if first_push {
                        "已推送到联邦大厅（其他 NexOS 节点将自动收到）".to_string()
                    } else {
                        "已重新推送（广播最新快照，对端同源刷新）".to_string()
                    },
                })))
            }

            _ => Ok(error_response(404, "未知 api-market 路由")),
        }
    }
}

// ----------------------------------------------------------------------------
// 响应/辅助（与 im.rs 同款小工具）
// ----------------------------------------------------------------------------

/// 统一 401：写端点缺/无效链上 token。文案引导 nexhub 挑战-签名端点
/// （api-market 与 nexhub-lobby 共享 ChainAuth——token 互通），并明确无 admin 回落。
fn auth_required() -> ApiResponse {
    error_response(
        401,
        "需要 Authorization: Bearer <链上 token>（先 POST /api/v1/nexhub/auth/challenge + /auth/verify 签发；api-market 发布者身份=区块链公钥，不接受 admin token 回落）",
    )
}

/// 统一 403：下架者非 owner pubkey（用户定稿文案）。
fn forbidden_unlist() -> ApiResponse {
    error_response(
        403,
        "仅发布者可下架（publisher pubkey 不匹配；api-market 无 admin 通道）",
    )
}

/// 统一 403：心跳上报者非 owner pubkey。
fn forbidden_heartbeat() -> ApiResponse {
    error_response(403, "仅发布者可上报心跳（publisher pubkey 不匹配）")
}

/// 统一 403：联邦推送者非 owner pubkey（推送联邦=写面操作，同款无 admin 通道）。
fn forbidden_federate() -> ApiResponse {
    error_response(
        403,
        "仅发布者可推送联邦（publisher pubkey 不匹配；api-market 无 admin 通道）",
    )
}

/// 读系统 admin token env（构造期定格，与 im.rs `admin_token_from_env` 同款）：
/// `NEXOS_ADMIN_TOKEN` 优先，回落 `OS_ADMIN_TOKEN`；trim 后非空才算启用。
fn admin_token_from_env() -> Option<String> {
    std::env::var("NEXOS_ADMIN_TOKEN")
        .or_else(|_| std::env::var("OS_ADMIN_TOKEN"))
        .ok()
        .and_then(|t| {
            let t = t.trim().to_string();
            (!t.is_empty()).then_some(t)
        })
}

/// 构造一条 [`RouteSpec`]（component 固定 `api-market`；链上 token 一律
/// handler 内自验，requires_auth 恒 false）。
fn spec(method: HttpMethod, path: &str, requires_auth: bool) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: COMPONENT.to_string(),
        requires_auth,
        required_roles: vec![],
    }
}

/// 构造一个 200 JSON 响应（空 headers）。
fn ok_json(body: serde_json::Value) -> ApiResponse {
    ApiResponse {
        status: 200,
        body,
        headers: serde_json::json!({}),
    }
}

/// 构造一个最小 JSON 错误响应（status 由调用方指定）。
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

/// rusqlite 错误 → [`ApiGatewayError`]（显式映射，消息与 crate 既有 From 一致）。
fn db_err(e: rusqlite::Error) -> ApiGatewayError {
    ApiGatewayError::Internal(format!("数据库错误: {e}"))
}

/// 从请求路径中剥离 `?query` 后的纯 path 段（前后空段去除）。
fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

/// 从 query string 解析字符串参数。
fn parse_query_str(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut it = pair.splitn(2, '=');
        if it.next() == Some(key) {
            return it.next().map(|s| s.to_string());
        }
    }
    None
}

/// 简易 URL 解码（仅 %XX + `+` → 空格；与 nexhub_lobby 同款）。按字节累积后
/// 整体转 UTF-8，避免逐字节转 char 破坏多字节中文等非 ASCII 查询参数。
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'+' {
            out.push(b' ');
            i += 1;
        } else if b == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
            } else {
                out.push(b);
                i += 1;
            }
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 当前本地时间（RFC3339 / ISO8601 带时区）。
fn now_iso() -> String {
    chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

/// 生成一个新的 UUID v4 字符串。
fn new_uuid() -> String {
    os_core::Uuid::new_v4().to_string()
}

// ----------------------------------------------------------------------------
// SQLite 持久化层（复用 im.rs 的建库模式：WAL + 短锁快查快放）
// ----------------------------------------------------------------------------

/// 默认 DB 路径：优先 `/tank/os-data/api_market.db`，再 `/var/lib/os/api_market.db`，
/// 最后 `./api_market.db`。
fn default_db_path() -> String {
    for p in &["/tank/os-data/api_market.db", "/var/lib/os/api_market.db"] {
        if std::path::Path::new(p)
            .parent()
            .is_some_and(|d| d.exists() || std::fs::create_dir_all(d).is_ok())
        {
            return (*p).to_string();
        }
    }
    "./api_market.db".to_string()
}

/// 打开 SQLite 文件，建表（WAL）。
fn open_db(path: &str) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    create_schema(&conn)?;
    Ok(conn)
}

/// 建表（IF NOT EXISTS）+ 唯一约束（同 api_name + 同 pubkey 一条）+ 时间索引
/// + 老库迁移（access_info/source_node/federated/source_node_id 四列——
///   CREATE TABLE IF NOT EXISTS 不补列；2026-08-31 联邦化与接入信息扩展、
///   2026-09-02 跨网中继 source_node_id）。
fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS api_market (
            id TEXT PRIMARY KEY,
            api_name TEXT NOT NULL,
            description TEXT DEFAULT '',
            endpoint_url TEXT NOT NULL,
            publisher_pubkey TEXT NOT NULL,
            publisher_display TEXT DEFAULT '',
            server_config TEXT DEFAULT '{}',
            pricing TEXT DEFAULT '{}',
            metrics_url TEXT,
            tags TEXT DEFAULT '[]',
            status TEXT DEFAULT 'active',
            created_at TEXT,
            heartbeat_at TEXT,
            load TEXT,
            download_count INTEGER DEFAULT 0,
            access_info TEXT DEFAULT '',
            source_node TEXT DEFAULT 'local',
            federated INTEGER DEFAULT 0,
            source_node_id TEXT DEFAULT ''
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_api_market_name_owner
            ON api_market(api_name, publisher_pubkey);
        CREATE INDEX IF NOT EXISTS idx_api_market_created ON api_market(created_at DESC);
        ",
    )?;
    migrate_add_columns(conn)
}

/// 老库迁移：`PRAGMA table_info` 探测缺列 → `ALTER TABLE ADD COLUMN` 幂等补齐
/// （与 api_gateway/forwarding 的 migrate_add_* 同款手法）。
///
/// - `access_info TEXT DEFAULT ''`：接入信息 JSON（存量行空串=无接入信息）；
/// - `source_node TEXT DEFAULT 'local'`：联邦来源（存量行=本机发布）；
/// - `federated INTEGER DEFAULT 0`：两步联邦推送标志（存量行=未推送）；
/// - `source_node_id TEXT DEFAULT ''`：联邦来源 NodeID（存量行空=无中继定向，
///   源节点重新推送后按验签发送方回填）。
fn migrate_add_columns(conn: &Connection) -> rusqlite::Result<()> {
    let mut existing: Vec<String> = Vec::new();
    {
        let mut stmt = conn.prepare("PRAGMA table_info(api_market)")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
        for r in rows {
            existing.push(r?);
        }
    }
    for (col, ddl) in [
        ("access_info", "TEXT DEFAULT ''"),
        ("source_node", "TEXT DEFAULT 'local'"),
        ("federated", "INTEGER DEFAULT 0"),
        ("source_node_id", "TEXT DEFAULT ''"),
    ] {
        if !existing.iter().any(|c| c == col) {
            conn.execute(
                &format!("ALTER TABLE api_market ADD COLUMN {col} {ddl}"),
                [],
            )?;
        }
    }
    Ok(())
}

/// 列字段序（INSERT/SELECT 共用，与建表列序一致）。
const LISTING_COLUMNS: &str = "id,api_name,description,endpoint_url,publisher_pubkey,\
     publisher_display,server_config,pricing,metrics_url,tags,status,created_at,\
     heartbeat_at,load,download_count,access_info,source_node,federated,source_node_id";

fn insert_listing(conn: &Connection, l: &ApiListing) -> rusqlite::Result<()> {
    conn.execute(
        &format!(
            "INSERT OR REPLACE INTO api_market ({LISTING_COLUMNS}) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)"
        ),
        params![
            l.id,
            l.api_name,
            l.description,
            l.endpoint_url,
            l.publisher_pubkey,
            l.publisher_display,
            serde_json::to_string(&l.server_config).unwrap_or_else(|_| "{}".into()),
            serde_json::to_string(&l.pricing).unwrap_or_else(|_| "{}".into()),
            l.metrics_url,
            serde_json::to_string(&l.tags).unwrap_or_else(|_| "[]".into()),
            l.status,
            l.created_at,
            l.heartbeat_at,
            l.load
                .as_ref()
                .map(|m| serde_json::to_string(m).unwrap_or_else(|_| "{}".into())),
            l.download_count,
            serde_json::to_string(&l.access_info).unwrap_or_else(|_| "{}".into()),
            l.source_node,
            i64::from(l.federated),
            l.source_node_id,
        ],
    )?;
    Ok(())
}

fn listing_from_row(row: &rusqlite::Row) -> rusqlite::Result<ApiListing> {
    Ok(ApiListing {
        id: row.get(0)?,
        api_name: row.get(1)?,
        description: row.get(2)?,
        endpoint_url: row.get(3)?,
        publisher_pubkey: row.get(4)?,
        publisher_display: row.get(5)?,
        server_config: serde_json::from_str(&row.get::<_, String>(6)?).unwrap_or_default(),
        pricing: serde_json::from_str(&row.get::<_, String>(7)?).unwrap_or_default(),
        metrics_url: row.get(8)?,
        tags: serde_json::from_str(&row.get::<_, String>(9)?).unwrap_or_default(),
        status: row
            .get::<_, Option<String>>(10)?
            .unwrap_or_else(default_status_active),
        created_at: row.get::<_, Option<String>>(11)?.unwrap_or_default(),
        heartbeat_at: row.get(12)?,
        load: row
            .get::<_, Option<String>>(13)?
            .and_then(|s| serde_json::from_str(&s).ok()),
        download_count: row.get::<_, i64>(14)?.max(0) as u64,
        // 空串/坏 JSON → 空 AccessInfo（存量行/手改库容错）。
        access_info: row
            .get::<_, Option<String>>(15)?
            .filter(|s| !s.trim().is_empty())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
        // 空串/缺列 → local（serde default 同款兜底）。
        source_node: row
            .get::<_, Option<String>>(16)?
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(default_source_node),
        federated: row.get::<_, Option<i64>>(17)?.unwrap_or(0) != 0,
        // 空串 = 无中继定向（存量行/本机发布）。
        source_node_id: row
            .get::<_, Option<String>>(18)?
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_default(),
    })
}

fn find_by_id(conn: &Connection, id: &str) -> rusqlite::Result<Option<ApiListing>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {LISTING_COLUMNS} FROM api_market WHERE id=?"
    ))?;
    stmt.query_row(params![id], listing_from_row).optional()
}

fn find_by_name_owner(
    conn: &Connection,
    api_name: &str,
    pubkey: &str,
) -> rusqlite::Result<Option<ApiListing>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {LISTING_COLUMNS} FROM api_market WHERE api_name=? AND publisher_pubkey=?"
    ))?;
    stmt.query_row(params![api_name, pubkey], listing_from_row)
        .optional()
}

/// 大厅列表：active 态 + `q` 关键词（api_name/description/tags LIKE），
/// `recent` 按 created_at 降序（rowid 决胜——同秒发布新者在前）；
/// `price` 排序在调用方 Rust 侧做（pricing 是 JSON 列，SQL 排不动）。
fn load_listings(
    conn: &Connection,
    q: Option<&str>,
    sort: &str,
) -> rusqlite::Result<Vec<ApiListing>> {
    let mut bind: Vec<String> = Vec::new();
    let mut sql = format!("SELECT {LISTING_COLUMNS} FROM api_market WHERE status='active'");
    if let Some(q) = q {
        sql.push_str(" AND (api_name LIKE ? OR description LIKE ? OR tags LIKE ?)");
        let like = format!("%{q}%");
        bind.push(like.clone());
        bind.push(like.clone());
        bind.push(like);
    }
    sql.push_str(if sort == "price" {
        // 拉全量后由调用方按价格排序；SQL 侧仍按新近序拉，stable sort 保留同价位新近在前。
        " ORDER BY created_at DESC, rowid DESC"
    } else {
        " ORDER BY created_at DESC, rowid DESC"
    });
    let mut stmt = conn.prepare(&sql)?;
    let iter = stmt.query_map(params_from_iter(bind.iter()), listing_from_row)?;
    let mut out = Vec::new();
    for e in iter {
        out.push(e?);
    }
    Ok(out)
}

/// 本节点已推送联邦的条目快照（补推 [`ApiMarketFedEndpoint::backfill_to`] /
/// 定期重播 `replay_round` 用）：`source_node='local'`（本地发布——**远程条目
/// 不转播**，防环红线：fed_broadcast 是一跳语义，接收方再广播会污染来源
/// 归因）+ `federated=1` + active 态；created_at/rowid 降序（与大厅列表同序，
/// 补推/重播顺序稳定可测）。
fn load_federated_local_listings(conn: &Connection) -> rusqlite::Result<Vec<ApiListing>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {LISTING_COLUMNS} FROM api_market \
         WHERE status='active' AND federated=1 AND source_node='local' \
         ORDER BY created_at DESC, rowid DESC"
    ))?;
    let iter = stmt.query_map([], listing_from_row)?;
    let mut out = Vec::new();
    for e in iter {
        out.push(e?);
    }
    Ok(out)
}

fn delete_listing(conn: &Connection, id: &str) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM api_market WHERE id=?", params![id])
}

fn update_heartbeat(
    conn: &Connection,
    id: &str,
    heartbeat_at: &str,
    load: &LoadMetrics,
) -> rusqlite::Result<usize> {
    let load_json = serde_json::to_string(load).unwrap_or_else(|_| "{}".into());
    conn.execute(
        "UPDATE api_market SET heartbeat_at=?, load=? WHERE id=?",
        params![heartbeat_at, load_json, id],
    )
}

/// 服务端常驻心跳兜底一轮（[`install_heartbeat_sweep`] 每 60s 调用，内核纯
/// 同步可单测）：对本节点全部 **active 本地条目**（`source_node='local'`，
/// 联邦远程副本不碰——它们的活性归源节点管）刷新 `heartbeat_at=now`（复用
/// 既有 [`update_heartbeat`] 写路径；**load 保留最后一次上报值**——存活证明
/// 不是负载探测，不造数据）。已新鲜（≤60s——页面驱动心跳刚写过，更真）的
/// 条目跳过：服务端兜底永不覆盖页面上报。节点活着 = 兜底任务活着，心跳
/// 恒新鲜；节点挂了 = 两者一起沉默，心跳自然过期（诚实降级）。返回本轮
/// 实际刷新的条数（测试/日志观测面）。
fn refresh_local_heartbeats(conn: &Connection) -> usize {
    let Ok(entries) = load_listings(conn, None, "recent") else {
        return 0; // 读失败：本轮放弃（下一轮再来），不 panic
    };
    let now = now_iso();
    let mut refreshed = 0;
    for e in entries.iter().filter(|e| {
        listing_is_local(e) && e.status == "active" // load_listings 已滤 active，此处双保险
    }) {
        // 新鲜心跳跳过：页面驱动上报（带实时负载）恒先于 60s 兜底到站。
        if e.heartbeat_at.as_deref().is_some_and(heartbeat_fresh) {
            continue;
        }
        let load = e.load.unwrap_or_default();
        if update_heartbeat(conn, &e.id, &now, &load).unwrap_or(0) > 0 {
            refreshed += 1;
        }
    }
    refreshed
}

// ----------------------------------------------------------------------------
// 集成测（真密钥对走链上认证：challenge → sign → verify → token）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // —— 测试辅助（与 im.rs 测试同款风格）——

    /// 详情/下架路径实例化（/api/v1/api-market/:id）。
    fn detail_path(id: &str) -> String {
        format!("{PATH_LIST}/{id}")
    }

    /// 心跳路径实例化（/api/v1/api-market/:id/heartbeat）。
    fn hb_path(id: &str) -> String {
        format!("{PATH_LIST}/{id}/heartbeat")
    }

    /// metrics 路径实例化（/api/v1/api-market/:id/metrics）。
    fn metrics_path(id: &str) -> String {
        format!("{PATH_LIST}/{id}/metrics")
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

    fn authed(method: HttpMethod, path: &str, token: &str, body: serde_json::Value) -> ApiRequest {
        ApiRequest {
            method,
            path: path.into(),
            headers: serde_json::json!({ "authorization": format!("Bearer {token}") }),
            body,
            auth: None,
        }
    }

    /// 生成真 secp256k1 密钥对（CSPRNG，k256 与生产同栈）。
    fn new_key() -> k256::ecdsa::SigningKey {
        use k256::elliptic_curve::rand_core::OsRng;
        k256::ecdsa::SigningKey::random(&mut OsRng)
    }

    /// 私钥 → 身份（0x + 66 hex 压缩公钥）。
    fn pubkey_hex(sk: &k256::ecdsa::SigningKey) -> String {
        format!(
            "0x{}",
            hex::encode(sk.verifying_key().to_encoded_point(true).as_bytes())
        )
    }

    /// 客户端签名：SHA-256(nonce UTF-8) → RFC6979 ECDSA（65 字节 r||s||v，
    /// 与前端 @noble/secp256k1 sign(sha256(nonce)) 同构——共享内核验签同规则）。
    fn sign_nonce(sk: &k256::ecdsa::SigningKey, nonce: &str) -> [u8; 65] {
        use sha2::Digest;
        let digest = sha2::Sha256::new_with_prefix(nonce.as_bytes());
        let (sig, recid) = sk.sign_digest_recoverable(digest).expect("签名必成功");
        let mut out = [0u8; 65];
        out[..64].copy_from_slice(&sig.to_bytes());
        out[64] = u8::from(recid);
        out
    }

    /// 真密钥对全流程登录（共享 ChainAuth 内核上跑 challenge→sign→verify→token；
    /// api-market 无自己的 auth 端点，生产路径由 nexhub 的同名端点完成——
    /// 同一个 `ChainAuth` 实例，内核函数逐字节一致）。
    fn login(h: &ApiMarketRouteHandler, sk: &k256::ecdsa::SigningKey) -> (String, String) {
        let pubkey = pubkey_hex(sk);
        let auth = h.auth();
        let nonce = auth.create_nonce(&pubkey);
        let sig = sign_nonce(sk, &nonce);
        assert!(
            auth.take_nonce(&pubkey, &nonce),
            "nonce 匹配且未过期（challenge 应成功）"
        );
        assert!(chain_auth::verify_nonce_signature(
            sk.verifying_key(),
            &nonce,
            &sig
        ));
        let (token, _) = auth.issue_token(&pubkey);
        (pubkey, token)
    }

    /// 标准挂牌 body 构造器（per_token 价 price；缺省字段由各测试覆盖）。
    fn publish_body(api_name: &str, price: Option<u64>) -> serde_json::Value {
        serde_json::json!({
            "api_name": api_name,
            "description": format!("{api_name} 描述"),
            "endpoint_url": "http://127.0.0.1:8080/api/v1/gateway/v1/chat/completions",
            "pricing": if let Some(p) = price {
                serde_json::json!({ "mode": "per_token", "price_per_1k_tokens": p })
            } else {
                serde_json::json!({ "mode": "free" })
            },
            "server_config": { "model_name": "Qwen3.5-9B" },
        })
    }

    /// 挂牌并断言成功，返回条目 id。
    async fn publish_ok(
        h: &ApiMarketRouteHandler,
        token: &str,
        api_name: &str,
        price: Option<u64>,
    ) -> String {
        let resp = h
            .handle(authed(
                HttpMethod::Post,
                PATH_PUBLISH,
                token,
                publish_body(api_name, price),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "挂牌应 201: {}", resp.body);
        resp.body["id"].as_str().unwrap().to_string()
    }

    /// 起一个极简 JSON HTTP 服务（std TcpListener，真实 reqwest 端到端可达）。
    /// 依次响应 `bodies`；返回 (端口, 命中计数)——计数用于断言「未代拉」。
    fn spawn_fake_json_server(
        bodies: Vec<String>,
    ) -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 失败");
        let port = listener.local_addr().expect("local_addr 失败").port();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits_clone = hits.clone();
        std::thread::spawn(move || {
            for body in bodies {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                hits_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
        (port, hits)
    }

    /// 起一个「接受连接但永不响应」的服务（验代拉超时降级；随进程退出回收）。
    fn spawn_hanging_server() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 失败");
        let port = listener.local_addr().expect("local_addr 失败").port();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                drop(stream); // 持连接不写，等客户端超时
                std::thread::sleep(Duration::from_secs(10));
            }
        });
        port
    }

    /// 找一个几乎必然关闭的本机端口（bind 后立刻释放）。
    fn closed_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }

    /// 距 now 偏移 offset_secs 秒的 RFC3339（与 now_iso 同格式）。
    fn iso_offset_secs(offset_secs: i64) -> String {
        (chrono::Local::now() + chrono::Duration::seconds(offset_secs))
            .format("%Y-%m-%dT%H:%M:%S%:z")
            .to_string()
    }

    /// 直接 SQL 注入旧心跳（同模块测试可触达私有字段）。
    fn inject_heartbeat_at(h: &ApiMarketRouteHandler, id: &str, at: &str) {
        let conn = h.db.lock().expect("db poisoned");
        conn.execute(
            "UPDATE api_market SET heartbeat_at=? WHERE id=?",
            params![at, id],
        )
        .expect("注入心跳时间必成功");
    }

    // 1. 路由表：7 条、component=api-market、requires_auth 全 false（链上 token handler 内自验）
    #[tokio::test]
    async fn routes_declares_all_endpoints() {
        let h = ApiMarketRouteHandler::with_empty();
        let routes = h.routes().await;
        assert_eq!(routes.len(), 7, "应声明 7 条路由: {routes:?}");
        assert!(routes.iter().all(|r| r.handler_component == COMPONENT));
        assert!(
            routes.iter().all(|r| !r.requires_auth),
            "链上 token 在 handler 内自验，requires_auth 应全 false"
        );
        let pairs: Vec<(HttpMethod, &str)> =
            routes.iter().map(|r| (r.method, r.path.as_str())).collect();
        for (m, p) in [
            (HttpMethod::Post, PATH_PUBLISH),
            (HttpMethod::Get, PATH_LIST),
            (HttpMethod::Get, PATH_DETAIL),
            (HttpMethod::Delete, PATH_UNLIST),
            (HttpMethod::Post, PATH_HEARTBEAT),
            (HttpMethod::Get, PATH_METRICS),
            (HttpMethod::Post, PATH_FEDERATE),
        ] {
            assert!(pairs.contains(&(m, p)), "缺少路由 {m:?} {p}");
        }
    }

    // 2. 发布鉴权：无 token / 垃圾 token / admin token 一律 401（无回落），
    //    文案引导 nexhub 挑战端点。
    #[tokio::test]
    async fn publish_requires_chain_token_no_admin_fallback() {
        let h = ApiMarketRouteHandler::with_empty();
        // 无 token
        let resp = h
            .handle(ApiRequest {
                method: HttpMethod::Post,
                path: PATH_PUBLISH.into(),
                headers: serde_json::json!({}),
                body: publish_body("x", Some(1)),
                auth: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.status, 401, "无 token 应 401");
        assert!(
            resp.body["error"]
                .as_str()
                .unwrap()
                .contains("/api/v1/nexhub/auth/challenge"),
            "401 文案应引导 nexhub 挑战端点: {}",
            resp.body
        );
        // 空 Bearer（bearer_token 过滤空值 → 同 401 路径）
        let resp = h
            .handle(authed(
                HttpMethod::Post,
                PATH_PUBLISH,
                "",
                publish_body("x", Some(1)),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 401, "空 token 应 401");
        // 垃圾 token
        let resp = h
            .handle(authed(
                HttpMethod::Post,
                PATH_PUBLISH,
                "garbage-token",
                publish_body("x", Some(1)),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 401, "无效 token 应 401");
        // admin token（NEXOS_ADMIN_TOKEN 值）也不回落——发布者必须可验签的链上身份
        let resp = h
            .handle(authed(
                HttpMethod::Post,
                PATH_PUBLISH,
                "nexos-admin-secret-token",
                publish_body("x", Some(1)),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 401, "admin token 不应回落放行");
        assert!(
            resp.body["error"]
                .as_str()
                .unwrap()
                .contains("不接受 admin"),
            "文案应明确无 admin 回落: {}",
            resp.body
        );
        // 未挂牌成功——列表为空
        let resp = h.handle(get_req(PATH_LIST)).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body.as_array().unwrap().len(), 0);
    }

    // 3. 发布归因：publisher_pubkey=token 反查 pubkey，display=EVM 派生
    #[tokio::test]
    async fn publish_attributes_pubkey_and_derives_display() {
        let h = ApiMarketRouteHandler::with_empty();
        let sk = new_key();
        let (pubkey, token) = login(&h, &sk);
        let id = publish_ok(&h, &token, "qwen3.5-9b chat", Some(50)).await;
        let resp = h
            .handle(authed(
                HttpMethod::Get,
                &detail_path(&id),
                &token,
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(
            resp.body["publisher_pubkey"], pubkey,
            "归因=token 反查 pubkey"
        );
        let expect_display = chain_auth::derive_display_name(
            &chain_auth::parse_pubkey(&pubkey).expect("pubkey 应可解析"),
        );
        assert_eq!(resp.body["publisher_display"], expect_display);
        // 列表摘要也带 publisher_display / server_config / pricing
        let resp = h.handle(get_req(PATH_LIST)).await.unwrap();
        let item = &resp.body.as_array().unwrap()[0];
        assert_eq!(item["publisher_display"], expect_display);
        assert_eq!(item["server_config"]["model_name"], "Qwen3.5-9B");
        assert_eq!(item["pricing"]["mode"], "per_token");
        assert_eq!(item["pricing"]["price_per_1k_tokens"], 50);
        assert_eq!(item["pricing"]["currency"], "sats", "缺省币种=sats");
    }

    // 4. 自动探测 + body 覆盖优先级：body 显式字段胜出，缺省字段被本地探测补齐
    //    （cpu/ram 在 Linux CI 上恒可探测；GPU 视机器而定，body 值必胜出）
    #[tokio::test]
    async fn publish_probes_local_and_body_overrides() {
        let h = ApiMarketRouteHandler::with_empty();
        let (_, token) = login(&h, &new_key());
        let body = serde_json::json!({
            "api_name": "probe-test",
            "endpoint_url": "http://10.0.0.1:9000/v1/chat/completions",
            "pricing": { "mode": "free" },
            "server_config": {
                "gpu_name": "NVIDIA GeForce RTX 9999 Fake",
                "gpu_vram_mb": 123456,
                "model_name": "Qwen3.5-9B",
                "max_model_len": 32768,
            },
        });
        let resp = h
            .handle(authed(HttpMethod::Post, PATH_PUBLISH, &token, body))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "{}", resp.body);
        let sc = &resp.body["server_config"];
        // body 字段原样保留（覆盖探测——即便本机真有 GPU，body 值也胜出）
        assert_eq!(sc["gpu_name"], "NVIDIA GeForce RTX 9999 Fake");
        assert_eq!(sc["gpu_vram_mb"], 123456);
        assert_eq!(sc["max_model_len"], 32768);
        // 旧式覆盖（gpu_name+gpu_vram_mb 都给）→ 合成单卡 gpus + gpu_count=1
        assert_eq!(sc["gpu_count"], 1, "旧式覆盖合成单卡: {sc}");
        assert_eq!(sc["gpus"].as_array().map(Vec::len), Some(1));
        assert_eq!(sc["gpus"][0]["name"], "NVIDIA GeForce RTX 9999 Fake");
        assert_eq!(sc["gpus"][0]["vram_mb"], 123456);
        assert!(
            sc["gpus"][0].get("index").is_none(),
            "合成条目不带 index: {sc}"
        );
        // 缺省字段被本地探测补齐（/proc 在 Linux 上必可读）
        assert!(
            sc["cpu_cores"].as_u64().is_some_and(|n| n > 0),
            "cpu_cores 应被 /proc/cpuinfo 探测补齐: {sc}"
        );
        assert!(
            sc["cpu_model"].as_str().is_some_and(|m| !m.is_empty()),
            "cpu_model 应被 /proc/cpuinfo model name 探测补齐: {sc}"
        );
        assert!(
            sc["ram_gb"].as_f64().is_some_and(|n| n > 0.0),
            "ram_gb 应被 /proc/meminfo 探测补齐: {sc}"
        );
        // body 未带的探测型字段（quantization/region）保持缺省
        assert!(sc.get("quantization").is_none());
        assert!(sc.get("region").is_none());
    }

    // 5. 必填缺失 400：model_name 探测不到且 body 未带
    #[tokio::test]
    async fn publish_missing_model_name_400() {
        let h = ApiMarketRouteHandler::with_empty();
        let (_, token) = login(&h, &new_key());
        let body = serde_json::json!({
            "api_name": "no-model",
            "endpoint_url": "http://10.0.0.1:9000/v1/chat/completions",
            "pricing": { "mode": "free" },
        });
        let resp = h
            .handle(authed(HttpMethod::Post, PATH_PUBLISH, &token, body))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        assert!(
            resp.body["error"].as_str().unwrap().contains("model_name"),
            "错误文案应点名 model_name: {}",
            resp.body
        );
    }

    // 6. 其余必填/格式校验 400：api_name / endpoint_url / pricing / URL scheme
    #[tokio::test]
    async fn publish_missing_required_fields_400() {
        let h = ApiMarketRouteHandler::with_empty();
        let (_, token) = login(&h, &new_key());
        let cases: Vec<(serde_json::Value, &str)> = vec![
            (
                serde_json::json!({
                    "endpoint_url": "http://a/b",
                    "pricing": { "mode": "free" },
                }),
                "api_name",
            ),
            (
                serde_json::json!({
                    "api_name": "x",
                    "pricing": { "mode": "free" },
                }),
                "endpoint_url",
            ),
            (
                serde_json::json!({
                    "api_name": "x",
                    "endpoint_url": "ftp://bad/scheme",
                    "pricing": { "mode": "free" },
                }),
                "http://",
            ),
            (
                serde_json::json!({
                    "api_name": "x",
                    "endpoint_url": "http://a/b",
                }),
                "pricing",
            ),
        ];
        for (body, needle) in cases {
            let resp = h
                .handle(authed(HttpMethod::Post, PATH_PUBLISH, &token, body.clone()))
                .await
                .unwrap();
            assert_eq!(resp.status, 400, "应 400: {body}");
            assert!(
                resp.body["error"].as_str().unwrap().contains(needle),
                "文案应点名 {needle}: {}",
                resp.body
            );
        }
    }

    // 7. 重复发布=刷新：同 api_name + 同 pubkey → 200 refreshed、保留 id/计数；
    //    不同 pubkey 同名 → 独立条目（201）
    #[tokio::test]
    async fn republish_refreshes_and_preserves_count() {
        let h = ApiMarketRouteHandler::with_empty();
        let sk1 = new_key();
        let (_, t1) = login(&h, &sk1);
        let id1 = publish_ok(&h, &t1, "shared-name", Some(10)).await;
        // 手工置 download_count=7（模拟历史消费计数）
        {
            let conn = h.db.lock().expect("db poisoned");
            conn.execute(
                "UPDATE api_market SET download_count=7 WHERE id=?",
                params![id1],
            )
            .unwrap();
        }
        // 同 pubkey 重发（改价）→ 200 refreshed，id/计数保留
        let resp = h
            .handle(authed(
                HttpMethod::Post,
                PATH_PUBLISH,
                &t1,
                publish_body("shared-name", Some(20)),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "重复发布应 200 刷新: {}", resp.body);
        assert_eq!(resp.body["refreshed"], true);
        assert_eq!(resp.body["id"], id1, "刷新保留原 id");
        assert_eq!(resp.body["download_count"], 7, "刷新保留计数");
        assert_eq!(
            resp.body["pricing"]["price_per_1k_tokens"], 20,
            "价格已更新"
        );
        // 另一 pubkey 同名 → 独立新条目（201）
        let sk2 = new_key();
        let (_, t2) = login(&h, &sk2);
        let id2 = publish_ok(&h, &t2, "shared-name", Some(5)).await;
        assert_ne!(id1, id2, "不同发布者同名条目各自独立");
        let resp = h.handle(get_req(PATH_LIST)).await.unwrap();
        assert_eq!(resp.body.as_array().unwrap().len(), 2);
    }

    // 8. 计价校验矩阵：非法组合 400 / 合法缺省补齐
    #[tokio::test]
    async fn pricing_validation_matrix() {
        // per_token 无价 / free 带价 / 坏 mode / 坏 currency / 空 mode → 400
        for bad in [
            serde_json::json!({ "mode": "per_token" }),
            serde_json::json!({ "mode": "free", "price_per_1k_tokens": 5 }),
            serde_json::json!({ "mode": "per_hour", "price_per_1k_tokens": 5 }),
            serde_json::json!({ "mode": "per_token", "price_per_1k_tokens": 5, "currency": "usd" }),
            serde_json::json!({ "mode": "per_image" }),
            serde_json::json!({}),
        ] {
            assert!(
                validate_pricing(&serde_json::from_value(bad.clone()).unwrap()).is_err(),
                "应拒绝: {bad}"
            );
        }
        // 合法：per_image 复用单价格字段（每图单价）+ credits 币种
        let ok = validate_pricing(
            &serde_json::from_value(serde_json::json!({
                "mode": "per_image", "price_per_1k_tokens": 100, "currency": "credits"
            }))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(ok.mode, "per_image");
        assert_eq!(ok.currency, "credits");
        assert_eq!(ok.effective_price(), 100);
        // 合法：free 规范化（currency 强制 free、价清空；空 mode 本身是 400——
        // free 判定在缺省 Pricing 上也成立，但发布入口要求显式 mode）
        let free = validate_pricing(&Pricing {
            mode: "free".into(),
            ..Default::default()
        })
        .unwrap();
        assert!(free.is_free());
        assert_eq!(free.currency, "free");
        assert_eq!(free.price_per_1k_tokens, None);
    }

    // 9. 价格排序：付费单价升序在前、免费垫底（价格排名基础）
    #[tokio::test]
    async fn list_sort_price_ascending_free_last() {
        let h = ApiMarketRouteHandler::with_empty();
        let (_, token) = login(&h, &new_key());
        publish_ok(&h, &token, "expensive", Some(500)).await;
        publish_ok(&h, &token, "cheap", Some(2)).await;
        publish_ok(&h, &token, "mid", Some(50)).await;
        publish_ok(&h, &token, "gratis", None).await;
        let resp = h
            .handle(get_req(&format!("{PATH_LIST}?sort=price")))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let names: Vec<&str> = resp
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["api_name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["cheap", "mid", "expensive", "gratis"],
            "付费升序 + free 垫底"
        );
    }

    // 10. 默认新近排序 + q 搜索
    #[tokio::test]
    async fn list_recent_and_q_search() {
        let h = ApiMarketRouteHandler::with_empty();
        let (_, token) = login(&h, &new_key());
        publish_ok(&h, &token, "alpha-llm", Some(1)).await;
        publish_ok(&h, &token, "beta-diffusion", None).await;
        publish_ok(&h, &token, "gamma-llm", Some(3)).await;
        // 默认（recent）：最新挂牌在前
        let resp = h.handle(get_req(PATH_LIST)).await.unwrap();
        let names: Vec<&str> = resp
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["api_name"].as_str().unwrap())
            .collect();
        assert_eq!(names.first(), Some(&"gamma-llm"), "默认新近降序: {names:?}");
        // q 命中 api_name 子串
        let resp = h
            .handle(get_req(&format!("{PATH_LIST}?q=llm")))
            .await
            .unwrap();
        let names: Vec<&str> = resp
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["api_name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["gamma-llm", "alpha-llm"],
            "q=llm 命中两条（新近序）"
        );
        // q 命中 description（publish_body 的描述含 api_name 原文）
        let resp = h
            .handle(get_req(&format!("{PATH_LIST}?q=diffusion")))
            .await
            .unwrap();
        assert_eq!(resp.body.as_array().unwrap().len(), 1);
        // q 支持 URL 编码（前端百分号编码中文/空格：%6C%6C%6D = "llm"）
        let resp = h
            .handle(get_req(&format!("{PATH_LIST}?q=%6C%6C%6D")))
            .await
            .unwrap();
        assert_eq!(
            resp.body.as_array().unwrap().len(),
            2,
            "URL 编码的 q 应先解码再匹配"
        );
        // q 无命中 → 空数组
        let resp = h
            .handle(get_req(&format!("{PATH_LIST}?q=nonexistent")))
            .await
            .unwrap();
        assert_eq!(resp.body.as_array().unwrap().len(), 0);
        // sort=recent 显式同默认
        let resp = h
            .handle(get_req(&format!("{PATH_LIST}?sort=recent")))
            .await
            .unwrap();
        let names: Vec<&str> = resp
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["api_name"].as_str().unwrap())
            .collect();
        assert_eq!(names.first(), Some(&"gamma-llm"));
    }

    // 11. 详情：全字段 + heartbeat_fresh 派生；未知 id 404
    #[tokio::test]
    async fn detail_returns_full_listing_and_404() {
        let h = ApiMarketRouteHandler::with_empty();
        let sk = new_key();
        let (pubkey, token) = login(&h, &sk);
        let id = publish_ok(&h, &token, "detail-target", Some(9)).await;
        let resp = h.handle(get_req(&detail_path(&id))).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["id"], id);
        assert_eq!(resp.body["api_name"], "detail-target");
        assert_eq!(resp.body["publisher_pubkey"], pubkey);
        assert_eq!(
            resp.body["endpoint_url"],
            "http://127.0.0.1:8080/api/v1/gateway/v1/chat/completions"
        );
        assert_eq!(resp.body["heartbeat_fresh"], false, "从未心跳不新鲜");
        assert_eq!(resp.body["heartbeat_at"], serde_json::Value::Null);
        // 未知 id → 404
        let resp = h
            .handle(get_req(&format!("{PATH_LIST}/no-such-id")))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // 12. 下架 owner-only：无 token 401 / admin token 401（无回落）/
    //     他人 pubkey 403「仅发布者可下架」/ owner 200 → 再查 404
    #[tokio::test]
    async fn delete_owner_only_admin_also_rejected() {
        let h = ApiMarketRouteHandler::with_empty();
        let owner = new_key();
        let (_, owner_token) = login(&h, &owner);
        let id = publish_ok(&h, &owner_token, "to-delete", None).await;
        // 无 token → 401
        let resp = h
            .handle(ApiRequest {
                method: HttpMethod::Delete,
                path: detail_path(&id),
                headers: serde_json::json!({}),
                body: serde_json::Value::Null,
                auth: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.status, 401);
        // admin token → 401（不回落：无链上身份）
        let resp = h
            .handle(authed(
                HttpMethod::Delete,
                &detail_path(&id),
                "nexos-admin-secret-token",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 401, "admin token 不应可下架");
        // 他人链上身份 → 403 + 定稿文案
        let other = new_key();
        let (_, other_token) = login(&h, &other);
        let resp = h
            .handle(authed(
                HttpMethod::Delete,
                &detail_path(&id),
                &other_token,
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 403);
        assert!(
            resp.body["error"]
                .as_str()
                .unwrap()
                .contains("仅发布者可下架"),
            "403 文案契约: {}",
            resp.body
        );
        // owner → 200
        let resp = h
            .handle(authed(
                HttpMethod::Delete,
                &detail_path(&id),
                &owner_token,
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["deleted"], true);
        // 已下架 → 详情 404；再删 → 404
        let resp = h.handle(get_req(&detail_path(&id))).await.unwrap();
        assert_eq!(resp.status, 404);
        let resp = h
            .handle(authed(
                HttpMethod::Delete,
                &detail_path(&id),
                &owner_token,
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // 13. 心跳：owner 自报更新 heartbeat_at + load；metrics 端点判新鲜/过期
    //     （注入 2 分钟前的心跳 → stale）
    #[tokio::test]
    async fn heartbeat_updates_and_stale_judgement() {
        let h = ApiMarketRouteHandler::with_empty();
        let (_, token) = login(&h, &new_key());
        let id = publish_ok(&h, &token, "hb-target", None).await;
        // owner 心跳（心跳 body 键名：running_req/waiting_req/gpu_cache_usage）
        let resp = h
            .handle(authed(
                HttpMethod::Post,
                &hb_path(&id),
                &token,
                serde_json::json!({
                    "running_req": 3,
                    "waiting_req": 1,
                    "gpu_cache_usage": 72.5,
                    "tokens_per_sec": 128,
                    "latency_ms": 340,
                    "load_pct": 66,
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert_eq!(resp.body["ok"], true);
        assert!(resp.body["heartbeat_at"].as_str().is_some());
        // 心跳即新鲜
        let metrics = h.handle(get_req(&metrics_path(&id))).await.unwrap();
        assert_eq!(metrics.status, 200);
        assert_eq!(metrics.body["reachable"], true);
        assert_eq!(metrics.body["stale"], false);
        assert_eq!(metrics.body["source"], "heartbeat");
        assert_eq!(metrics.body["metrics"]["running"], 3.0);
        assert_eq!(metrics.body["metrics"]["waiting"], 1.0);
        assert_eq!(metrics.body["metrics"]["gpu_cache"], 72.5);
        assert_eq!(metrics.body["metrics"]["tokens_per_sec"], 128.0);
        assert_eq!(metrics.body["metrics"]["latency_ms"], 340.0);
        assert_eq!(metrics.body["metrics"]["load_pct"], 66.0);
        // 详情也反映心跳
        let detail = h.handle(get_req(&detail_path(&id))).await.unwrap();
        assert_eq!(detail.body["heartbeat_fresh"], true);
        // 注入 2 分钟前心跳 → stale（无 metrics_url → reachable:false 走 none 分支）
        inject_heartbeat_at(&h, &id, &iso_offset_secs(-120));
        let metrics = h.handle(get_req(&metrics_path(&id))).await.unwrap();
        assert_eq!(metrics.body["stale"], true, "过期心跳应标 stale");
        assert_eq!(metrics.body["reachable"], false);
        assert_eq!(metrics.body["source"], "none");
        assert_eq!(
            metrics.body["metrics"]["running"], 3.0,
            "过期心跳数据仍随附（供前端展示最后已知负载）"
        );
        let detail = h.handle(get_req(&detail_path(&id))).await.unwrap();
        assert_eq!(detail.body["heartbeat_fresh"], false);
    }

    // 14. 心跳鉴权：无 token 401 / 他人 403 / 未知 id 404
    #[tokio::test]
    async fn heartbeat_owner_only_and_404() {
        let h = ApiMarketRouteHandler::with_empty();
        let (_, token) = login(&h, &new_key());
        let id = publish_ok(&h, &token, "hb-guard", None).await;
        let hb = hb_path(&id);
        // 无 token
        let resp = h
            .handle(ApiRequest {
                method: HttpMethod::Post,
                path: hb.clone(),
                headers: serde_json::json!({}),
                body: serde_json::json!({ "load_pct": 1 }),
                auth: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.status, 401);
        // 他人
        let (_, other_token) = login(&h, &new_key());
        let resp = h
            .handle(authed(
                HttpMethod::Post,
                &hb,
                &other_token,
                serde_json::json!({ "load_pct": 1 }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 403);
        assert!(
            resp.body["error"].as_str().unwrap().contains("仅发布者"),
            "403 文案: {}",
            resp.body
        );
        // 未知 id
        let resp = h
            .handle(authed(
                HttpMethod::Post,
                &hb_path("no-such"),
                &token,
                serde_json::json!({ "load_pct": 1 }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // 14b. 服务端常驻心跳兜底内核（纯同步，不起 runtime）：过期/无心跳的本地
    //      条目刷新 heartbeat_at 且保留既有 load；新鲜本地条目跳过（页面驱动
    //      上报不被覆盖）；联邦远程条目不碰（活性归源节点管）。
    #[test]
    fn heartbeat_sweep_refreshes_stale_local_only() {
        let h = ApiMarketRouteHandler::with_empty(); // 同步上下文：不起兜底任务
        let stale_hb = iso_offset_secs(-3600); // 1 小时前=过期
        let load = LoadMetrics {
            load_pct: Some(66.0),
            running: Some(3.0),
            ..LoadMetrics::default()
        };
        let mk = |id: &str, source: &str, hb: Option<String>, ld: Option<LoadMetrics>| ApiListing {
            id: id.into(),
            api_name: format!("sweep-{id}"),
            description: String::new(),
            endpoint_url: "http://127.0.0.1:8080/v1".into(),
            publisher_pubkey: format!("0x{id}"),
            publisher_display: String::new(),
            server_config: ServerConfig::default(),
            pricing: Pricing::default(),
            metrics_url: None,
            tags: vec![],
            status: default_status_active(),
            created_at: "2026-09-03T00:00:00+08:00".into(),
            heartbeat_at: hb,
            load: ld,
            download_count: 0,
            access_info: AccessInfo::default(),
            source_node: source.into(),
            source_node_id: String::new(),
            federated: true,
        };
        {
            let conn = h.db.lock().expect("db poisoned");
            insert_listing(&conn, &mk("sweep-none", "local", None, None))
                .expect("种入无心跳条目");
            insert_listing(&conn, &mk("sweep-stale", "local", Some(stale_hb.clone()), Some(load)))
                .expect("种入过期心跳条目");
            insert_listing(&conn, &mk("sweep-fresh", "local", Some(iso_offset_secs(0)), None))
                .expect("种入新鲜心跳条目");
            insert_listing(&conn, &mk("sweep-fed", "spark-node", Some(stale_hb.clone()), Some(load)))
                .expect("种入联邦远程条目");
        }
        let n = {
            let conn = h.db.lock().expect("db poisoned");
            refresh_local_heartbeats(&conn)
        };
        assert_eq!(n, 2, "只刷新过期/无心跳的本地条目（none+stale）");
        {
            let conn = h.db.lock().expect("db poisoned");
            for id in ["sweep-none", "sweep-stale"] {
                let e = find_by_id(&conn, id).expect("查询必成功").expect("条目必在");
                assert!(
                    e.heartbeat_at.as_deref().is_some_and(heartbeat_fresh),
                    "{id} 兜底后心跳应新鲜"
                );
            }
            // load 保留：兜底只证明存活，不造负载数据
            let stale = find_by_id(&conn, "sweep-stale")
                .expect("查询必成功")
                .expect("条目必在");
            assert_eq!(stale.load.as_ref().and_then(|l| l.load_pct), Some(66.0));
            assert_eq!(stale.load.as_ref().and_then(|l| l.running), Some(3.0));
            // 新鲜条目不动（页面驱动心跳的时间戳不被覆盖）
            let fresh = find_by_id(&conn, "sweep-fresh")
                .expect("查询必成功")
                .expect("条目必在");
            assert_eq!(
                fresh.heartbeat_at.as_deref(),
                Some(iso_offset_secs(0).as_str()),
                "新鲜心跳不被兜底覆盖"
            );
            // 联邦远程条目不动
            let fed = find_by_id(&conn, "sweep-fed")
                .expect("查询必成功")
                .expect("条目必在");
            assert_eq!(fed.heartbeat_at.as_deref(), Some(stale_hb.as_str()));
            // 再跑一轮：全部本地条目已新鲜 → 零刷新（幂等静默）
            assert_eq!(refresh_local_heartbeats(&conn), 0);
        }
    }

    // 14c. 常驻兜底任务接线（tokio：构造即 spawn，注入亚秒周期轮询 DB 验证
    //      「无页面驱动心跳也保新鲜」——根因场景：浏览器没开大厅页）。
    #[tokio::test]
    async fn heartbeat_sweep_task_keeps_local_fresh_without_page() {
        let h = ApiMarketRouteHandler::with_empty();
        h.set_heartbeat_sweep_interval_for_test(Duration::from_millis(50));
        let (_, token) = login(&h, &new_key());
        let id = publish_ok(&h, &token, "sweep-live", None).await;
        // 发布时 heartbeat_at=None；等待常驻任务兜底（最多 3s，周期 50ms 足够富余）
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let fresh = {
                let conn = h.db.lock().expect("db poisoned");
                find_by_id(&conn, &id)
                    .expect("查询必成功")
                    .expect("条目必在")
                    .heartbeat_at
                    .as_deref()
                    .is_some_and(heartbeat_fresh)
            };
            if fresh {
                break; // 兜底任务已刷新
            }
            assert!(
                std::time::Instant::now() < deadline,
                "3s 内常驻心跳兜底任务应刷新本地条目"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // metrics 端点反映兜底心跳：走 heartbeat 分支（不再「不可达」）
        let metrics = h.handle(get_req(&metrics_path(&id))).await.unwrap();
        assert_eq!(metrics.status, 200);
        assert_eq!(metrics.body["reachable"], true, "{}", metrics.body);
        assert_eq!(metrics.body["stale"], false);
        assert_eq!(metrics.body["source"], "heartbeat");
    }

    // 15. metrics 优先心跳：有新鲜心跳时**不**代拉 metrics_url（命中计数=0）
    #[tokio::test]
    async fn metrics_prefers_fresh_heartbeat_no_proxy() {
        let h = ApiMarketRouteHandler::with_empty();
        let (_, token) = login(&h, &new_key());
        let (port, hits) = spawn_fake_json_server(vec![serde_json::json!({
            "metrics": { "running": 99 }
        })
        .to_string()]);
        // 挂牌带 metrics_url + 立即心跳
        let body = serde_json::json!({
            "api_name": "hb-first",
            "endpoint_url": "http://10.0.0.1:9000/v1/chat/completions",
            "pricing": { "mode": "free" },
            "metrics_url": format!("http://127.0.0.1:{port}/metrics"),
            "server_config": { "model_name": "Qwen3.5-9B" },
        });
        let resp = h
            .handle(authed(HttpMethod::Post, PATH_PUBLISH, &token, body))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        let id = resp.body["id"].as_str().unwrap().to_string();
        let _ = h
            .handle(authed(
                HttpMethod::Post,
                &hb_path(&id),
                &token,
                serde_json::json!({ "running_req": 2, "load_pct": 10 }),
            ))
            .await
            .unwrap();
        // metrics：心跳优先，未外呼
        let metrics = h.handle(get_req(&metrics_path(&id))).await.unwrap();
        assert_eq!(metrics.body["source"], "heartbeat");
        assert_eq!(
            metrics.body["metrics"]["running"], 2.0,
            "心跳数据而非代拉 99"
        );
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "新鲜心跳时不应代拉 metrics_url"
        );
    }

    // 16. 无心跳 → 服务端代拉：{metrics:{...}} 规范化（vllm 约定键名）
    #[tokio::test]
    async fn metrics_proxies_metrics_url_when_no_heartbeat() {
        let h = ApiMarketRouteHandler::with_empty();
        let (_, token) = login(&h, &new_key());
        let (port, _hits) = spawn_fake_json_server(vec![serde_json::json!({
            "metrics": {
                "num_requests_running": 4,
                "num_requests_waiting": 2,
                "gpu_cache_usage": 88.5,
                "token_throughput": 210.7,
                "e2e_latency_ms": 456,
                "load": 51,
            }
        })
        .to_string()]);
        let body = serde_json::json!({
            "api_name": "proxy-target",
            "endpoint_url": "http://10.0.0.1:9000/v1/chat/completions",
            "pricing": { "mode": "per_token", "price_per_1k_tokens": 3 },
            "metrics_url": format!("http://127.0.0.1:{port}/metrics"),
            "server_config": { "model_name": "Qwen3.5-9B" },
        });
        let resp = h
            .handle(authed(HttpMethod::Post, PATH_PUBLISH, &token, body))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        let id = resp.body["id"].as_str().unwrap().to_string();
        let metrics = h.handle(get_req(&metrics_path(&id))).await.unwrap();
        assert_eq!(metrics.status, 200);
        assert_eq!(metrics.body["reachable"], true);
        assert_eq!(metrics.body["stale"], true, "无心跳来源=代拉，标 stale");
        assert_eq!(metrics.body["source"], "metrics_url");
        assert_eq!(
            metrics.body["metrics"]["running"], 4.0,
            "num_requests_running 别名"
        );
        assert_eq!(
            metrics.body["metrics"]["waiting"], 2.0,
            "num_requests_waiting 别名"
        );
        assert_eq!(
            metrics.body["metrics"]["gpu_cache"], 88.5,
            "gpu_cache_usage 别名"
        );
        assert_eq!(
            metrics.body["metrics"]["tokens_per_sec"], 210.7,
            "token_throughput 别名"
        );
        assert_eq!(
            metrics.body["metrics"]["latency_ms"], 456.0,
            "e2e_latency_ms 别名"
        );
        assert_eq!(metrics.body["metrics"]["load_pct"], 51.0, "load 别名");
    }

    // 17. 代拉降级：挂死服务（超时）/连接拒绝端口 → reachable:false；
    //      无心跳无 metrics_url → source:none
    #[tokio::test]
    async fn metrics_timeout_and_unreachable_degrade() {
        // 挂死端口 + 亚秒超时（注入 metrics_timeout=300ms 快速走完降级路径）
        let h =
            ApiMarketRouteHandler::with_empty().with_metrics_timeout(Duration::from_millis(300));
        let (_, token) = login(&h, &new_key());
        let hang_port = spawn_hanging_server();
        let body = serde_json::json!({
            "api_name": "hang-target",
            "endpoint_url": "http://10.0.0.1:9000/v1/chat/completions",
            "pricing": { "mode": "free" },
            "metrics_url": format!("http://127.0.0.1:{hang_port}/metrics"),
            "server_config": { "model_name": "Qwen3.5-9B" },
        });
        let resp = h
            .handle(authed(HttpMethod::Post, PATH_PUBLISH, &token, body))
            .await
            .unwrap();
        let id = resp.body["id"].as_str().unwrap().to_string();
        let started = std::time::Instant::now();
        let metrics = h.handle(get_req(&metrics_path(&id))).await.unwrap();
        assert_eq!(
            metrics.body["reachable"], false,
            "挂死上游应降级 unreachable"
        );
        assert_eq!(metrics.body["source"], "metrics_url");
        assert!(
            metrics.body["error"].as_str().is_some(),
            "降级响应应附原因: {}",
            metrics.body
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "应按注入超时快速返回，而非默认 5s"
        );
        // 连接拒绝（关闭端口）→ 同样 reachable:false
        let body = serde_json::json!({
            "api_name": "refused-target",
            "endpoint_url": "http://10.0.0.1:9000/v1/chat/completions",
            "pricing": { "mode": "free" },
            "metrics_url": format!("http://127.0.0.1:{}/metrics", closed_port()),
            "server_config": { "model_name": "Qwen3.5-9B" },
        });
        let resp = h
            .handle(authed(HttpMethod::Post, PATH_PUBLISH, &token, body))
            .await
            .unwrap();
        let id2 = resp.body["id"].as_str().unwrap().to_string();
        let metrics = h.handle(get_req(&metrics_path(&id2))).await.unwrap();
        assert_eq!(metrics.body["reachable"], false, "连接拒绝应降级");
        // 无心跳无 metrics_url → none 分支
        let id3 = publish_ok(&h, &token, "bare-target", None).await;
        let metrics = h.handle(get_req(&metrics_path(&id3))).await.unwrap();
        assert_eq!(metrics.body["reachable"], false);
        assert_eq!(metrics.body["source"], "none");
        assert_eq!(metrics.body["metrics"], serde_json::Value::Null);
    }

    // 18. 路由归属与鉴权矩阵：读公开（匿名可达）、写需链上 token（匿名 401）、
    //     未知路由/未知方法 404
    #[tokio::test]
    async fn route_and_auth_matrix() {
        let h = ApiMarketRouteHandler::with_empty();
        let (_, token) = login(&h, &new_key());
        let id = publish_ok(&h, &token, "matrix", None).await;
        // 读端点匿名可达
        assert_eq!(h.handle(get_req(PATH_LIST)).await.unwrap().status, 200);
        assert_eq!(
            h.handle(get_req(&detail_path(&id))).await.unwrap().status,
            200
        );
        assert_eq!(
            h.handle(get_req(&metrics_path(&id))).await.unwrap().status,
            200
        );
        // 写端点匿名 401（publish 在测试 2 已覆盖，此处补 heartbeat/delete）
        for path in [format!("{}{}", PATH_LIST, "/x"), hb_path("x")] {
            let resp = h
                .handle(ApiRequest {
                    method: if path.ends_with("/heartbeat") {
                        HttpMethod::Post
                    } else {
                        HttpMethod::Delete
                    },
                    path,
                    headers: serde_json::json!({}),
                    body: serde_json::Value::Null,
                    auth: None,
                })
                .await
                .unwrap();
            assert_eq!(resp.status, 401, "{resp:?}");
        }
        // 未知路由 → 404（不落到别的端点）
        let resp = h
            .handle(get_req("/api/v1/api-market/unknown/sub/route"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
        // 未知方法（PUT）→ 404
        let resp = h
            .handle(ApiRequest {
                method: HttpMethod::Put,
                path: PATH_LIST.into(),
                headers: serde_json::json!({}),
                body: serde_json::Value::Null,
                auth: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // 19. Default trait 可用（new() 路径）
    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<ApiMarketRouteHandler>();
    }

    // —— 纯函数单测 ——

    // 20. LoadMetrics 别名规范化：心跳键 / vllm 键 / 平铺 / 缺失
    #[test]
    fn load_metrics_from_json_aliases() {
        // 心跳 body 键名
        let hb = LoadMetrics::from_json(&serde_json::json!({
            "running_req": 3, "waiting_req": 1, "gpu_cache_usage": 72.5,
            "tokens_per_sec": 128, "latency_ms": 340, "load_pct": 66,
        }));
        assert_eq!(hb.running, Some(3.0));
        assert_eq!(hb.waiting, Some(1.0));
        assert_eq!(hb.gpu_cache, Some(72.5));
        assert_eq!(hb.tokens_per_sec, Some(128.0));
        assert_eq!(hb.latency_ms, Some(340.0));
        assert_eq!(hb.load_pct, Some(66.0));
        // vllm 约定键名
        let vllm = LoadMetrics::from_json(&serde_json::json!({
            "num_requests_running": 4, "num_requests_waiting": 0,
            "gpu_cache_usage": 90, "token_throughput": 55.5,
            "e2e_latency_ms": 12, "gpu_util": 77,
        }));
        assert_eq!(vllm.running, Some(4.0));
        assert_eq!(vllm.waiting, Some(0.0));
        assert_eq!(vllm.gpu_cache, Some(90.0));
        assert_eq!(vllm.tokens_per_sec, Some(55.5));
        assert_eq!(vllm.latency_ms, Some(12.0));
        assert_eq!(vllm.load_pct, Some(77.0), "gpu_util 别名");
        // 空对象 / 非对象 → 全 None；未知键忽略
        assert_eq!(
            LoadMetrics::from_json(&serde_json::json!({})),
            LoadMetrics::default()
        );
        assert_eq!(
            LoadMetrics::from_json(&serde_json::json!("not an object")),
            LoadMetrics::default()
        );
        assert_eq!(
            LoadMetrics::from_json(&serde_json::json!({ "whatever": 1 })),
            LoadMetrics::default()
        );
        // serde 往返（load 列 JSON 持久化路径）
        let back: LoadMetrics = serde_json::from_str(&serde_json::to_string(&hb).unwrap()).unwrap();
        assert_eq!(back, hb);
    }

    // 21. 价格排序键：付费升序在前、免费垫底；free 判定
    #[test]
    fn price_sort_key_orders_paid_then_free() {
        let free = Pricing {
            mode: "free".into(),
            currency: "free".into(),
            ..Default::default()
        };
        let p2 = Pricing {
            mode: "per_token".into(),
            price_per_1k_tokens: Some(2),
            currency: "sats".into(),
            ..Default::default()
        };
        let p9 = Pricing {
            mode: "per_image".into(),
            price_per_1k_tokens: Some(9),
            currency: "credits".into(),
            ..Default::default()
        };
        let mut keys = vec![
            free.price_sort_key(),
            p9.price_sort_key(),
            p2.price_sort_key(),
        ];
        keys.sort();
        assert_eq!(keys, vec![(0, 2), (0, 9), (1, 0)], "价低在前，free 垫底");
        assert!(free.is_free());
        assert!(!p2.is_free());
        assert_eq!(normalize_sort(Some("price")), "price");
        assert_eq!(normalize_sort(Some("recent")), "recent");
        assert_eq!(normalize_sort(None), "recent", "缺省 recent");
        assert_eq!(normalize_sort(Some("bogus")), "recent", "非法值回落 recent");
    }

    // 22. 探测解析纯函数：cpuinfo（型号+核数）/ meminfo / nvidia-smi csv / merge
    #[test]
    fn parse_probe_helpers() {
        let cpuinfo = "processor\t: 0\nvendor_id: x\nprocessor\t: 1\nprocessor: 2\n";
        assert_eq!(parse_cpuinfo_core_count(cpuinfo), Some(3));
        assert_eq!(parse_cpuinfo_core_count(""), None, "0 核 → None");
        let meminfo = "MemTotal:       16326448 kB\nMemFree:         1024 kB\n";
        assert_eq!(
            parse_meminfo_ram_gb(meminfo),
            Some(15.6),
            "16GiB 级保留一位小数"
        );
        assert_eq!(
            parse_meminfo_ram_gb("MemFree: 1 kB\n"),
            None,
            "无 MemTotal → None"
        );
        assert_eq!(
            parse_nvidia_gpu_csv("0, NVIDIA GeForce RTX 3090, 24576"),
            Some(GpuEntry {
                index: Some(0),
                name: "NVIDIA GeForce RTX 3090".into(),
                vram_mb: Some(24576),
                ..Default::default()
            })
        );
        assert_eq!(parse_nvidia_gpu_csv("only-name"), None, "缺列 → None");
        assert_eq!(parse_nvidia_gpu_csv("0, , 123"), None, "空卡名 → None");
        // 非数字显存：卡保留、vram None + 统一内存标记（驱动报不出独立显存）
        assert_eq!(
            parse_nvidia_gpu_csv("0, GPU, abc"),
            Some(GpuEntry {
                index: Some(0),
                name: "GPU".into(),
                vram_mb: None,
                unified_memory: true,
                unified_vram_mb: None,
            }),
            "显存解析失败不判无卡"
        );
        assert_eq!(
            parse_nvidia_gpu_csv("x, GPU, 100"),
            None,
            "非数字 index → None"
        );
        // DGX Spark GB10 实测形态（2026-09-03 隧道采集）：显存 [N/A]
        assert_eq!(
            parse_nvidia_gpu_csv("0, NVIDIA GB10, [N/A]"),
            Some(GpuEntry {
                index: Some(0),
                name: "NVIDIA GB10".into(),
                vram_mb: None,
                unified_memory: true,
                unified_vram_mb: None,
            }),
            "GB10 有输出即算有卡（旧解析器丢行误判无卡）"
        );
        // merge：body Some 覆盖探测，body None 落回探测
        let probed = ServerConfig {
            gpu_name: Some("Probed GPU".into()),
            gpu_vram_mb: Some(100),
            gpu_count: Some(1),
            gpus: vec![GpuEntry {
                index: Some(0),
                name: "Probed GPU".into(),
                vram_mb: Some(100),
                ..Default::default()
            }],
            cpu_model: Some("Intel Probed".into()),
            cpu_cores: Some(8),
            ram_gb: Some(32.5),
            ..Default::default()
        };
        let body = ServerConfig {
            gpu_name: Some("Body GPU".into()),
            model_name: Some("Qwen".into()),
            ..Default::default()
        };
        let merged = merge_server_config(Some(body), probed.clone());
        assert_eq!(
            merged.gpu_name.as_deref(),
            Some("Body GPU"),
            "body 覆盖探测"
        );
        assert_eq!(merged.cpu_cores, Some(8), "body 缺省落回探测");
        assert_eq!(
            merged.cpu_model.as_deref(),
            Some("Intel Probed"),
            "body 缺省落回探测型号"
        );
        assert_eq!(merged.model_name.as_deref(), Some("Qwen"));
        assert_eq!(
            merged.gpu_vram_mb,
            Some(100),
            "部分覆盖不合成，vram 落回探测首卡"
        );
        assert_eq!(merged.ram_gb, Some(32.5));
        assert_eq!(
            merge_server_config(None, probed.clone()).gpu_name,
            probed.gpu_name,
            "无 body 全探测"
        );
        assert_eq!(
            merge_server_config(None, probed.clone()),
            probed,
            "无 body 探测原样透传（含 gpus/gpu_count/cpu_model）"
        );
    }

    // 22a. 多 GPU 解析：逐行 csv（index,name,vram）→ 每卡一条目；空行/坏行跳过
    #[test]
    fn parse_multi_gpu_output() {
        let out = "0, NVIDIA GeForce RTX 4090, 24576\n\
                   1, NVIDIA GeForce RTX 4090, 24576\n\
                   2, NVIDIA A100 80GB PCIe, 81920\n";
        let gpus = parse_nvidia_gpus_output(out);
        assert_eq!(gpus.len(), 3, "三卡逐行各一条目: {gpus:?}");
        assert_eq!(gpus[0].index, Some(0));
        assert_eq!(gpus[0].name, "NVIDIA GeForce RTX 4090");
        assert_eq!(gpus[0].vram_mb, Some(24576));
        assert!(!gpus[0].unified_memory, "数值显存=独立显存卡");
        assert_eq!(gpus[1].index, Some(1), "index 逐卡递增区分同型号");
        assert_eq!(gpus[2].name, "NVIDIA A100 80GB PCIe");
        assert_eq!(gpus[2].vram_mb, Some(81920));
        // 坏行（缺列/非数字 index）跳过不拖垮整段；显存坏值行保留为 unified 卡
        let messy = "\n0, Good GPU, 100\nnot a csv\nx, Bad Index, 100\n\n";
        let gpus = parse_nvidia_gpus_output(messy);
        assert_eq!(gpus.len(), 1, "坏行跳过好行保留: {gpus:?}");
        assert_eq!(gpus[0].name, "Good GPU");
    }

    // 22b. 同型号多卡聚合语义：gpus 每卡一条目（不合并丢卡）+ gpu_count=卡数，
    //      旧字段 gpu_name/gpu_vram_mb=首卡镜像（向后兼容）
    #[test]
    fn same_model_dual_cards_keep_entries_and_count() {
        let gpus = parse_nvidia_gpus_output(
            "0, NVIDIA GeForce RTX 4090, 24576\n1, NVIDIA GeForce RTX 4090, 24576\n",
        );
        // 探测侧等价构造（probe_server_config_blocking 的纯函数路径）
        let probed = ServerConfig {
            gpu_name: gpus.first().map(|g| g.name.clone()),
            gpu_vram_mb: gpus.first().and_then(|g| g.vram_mb),
            gpu_count: u64::try_from(gpus.len()).ok(),
            gpus: gpus.clone(),
            ..Default::default()
        };
        assert_eq!(probed.gpus.len(), 2, "同型双卡=两条目");
        assert_eq!(probed.gpu_count, Some(2), "gpu_count=物理卡数");
        assert_eq!(
            probed.gpu_name.as_deref(),
            Some("NVIDIA GeForce RTX 4090"),
            "旧字段=首卡镜像"
        );
        assert_eq!(probed.gpu_vram_mb, Some(24576));
        // 序列化形状：两条目 + gpu_count 2 + 旧字段保留
        let json = serde_json::to_value(&probed).unwrap();
        assert_eq!(json["gpu_count"], 2);
        assert_eq!(json["gpu_name"], "NVIDIA GeForce RTX 4090");
        assert_eq!(json["gpu_vram_mb"], 24576);
        assert_eq!(json["gpus"].as_array().map(Vec::len), Some(2));
        assert_eq!(json["gpus"][1]["index"], 1, "第二条目 index=1");
    }

    // 22c. 无卡（CPU-only）：空 nvidia-smi 输出 → gpus 空 + gpu_count 0，不报错可发布
    #[test]
    fn no_gpu_yields_empty_gpus_and_zero_count() {
        assert!(parse_nvidia_gpus_output("").is_empty(), "空输出=无卡");
        assert!(parse_nvidia_gpus_output("\n \n").is_empty(), "全空行=无卡");
        // 探测侧等价构造（probe_gpus_blocking 失败路径的纯函数等价物）
        let probed = ServerConfig {
            gpu_count: Some(0),
            gpus: Vec::new(),
            ..Default::default()
        };
        let merged = merge_server_config(
            Some(ServerConfig {
                model_name: Some("Qwen3.5-9B".into()),
                ..Default::default()
            }),
            probed,
        );
        assert!(merged.gpus.is_empty(), "无卡发布 gpus 空: {merged:?}");
        assert_eq!(merged.gpu_count, Some(0), "CPU-only 节点 gpu_count=0");
        assert_eq!(
            merged.model_name.as_deref(),
            Some("Qwen3.5-9B"),
            "可正常发布"
        );
        // 序列化形状：空 gpus 不占 JSON、gpu_count=0 序列化（消费端可区分「无卡」）
        let json = serde_json::to_value(&merged).unwrap();
        assert_eq!(json["gpu_count"], 0);
        assert!(json.get("gpus").is_none(), "空 gpus 不序列化");
    }

    // 22d. cpu_model 解析：取首个 model name（不误吃短 model 行）；无 → None
    #[test]
    fn parse_cpuinfo_model_first_model_name() {
        let cpuinfo = "processor\t: 0\nvendor_id: GenuineIntel\nmodel\t\t: 142\n\
                       model name\t: Intel(R) Core(TM) i7-9700K CPU @ 3.60GHz\n\
                       processor\t: 1\nmodel name\t: Intel(R) Core(TM) i7-9700K CPU @ 3.60GHz\n";
        assert_eq!(
            parse_cpuinfo_model(cpuinfo),
            Some("Intel(R) Core(TM) i7-9700K CPU @ 3.60GHz".into()),
            "取首个 model name"
        );
        assert_eq!(
            parse_cpuinfo_model("processor: 0\n"),
            None,
            "无 model name 行 → None"
        );
        assert_eq!(
            parse_cpuinfo_model("model name\t:   \n"),
            None,
            "空型号 → None"
        );
    }

    // 22d-2. lscpu 回退（aarch64：GB10 /proc/cpuinfo 无 model name，实测只有
    //        MIDR CPU part 码；型号只在 lscpu 的 Model name 行，大小核分组各一行）
    #[test]
    fn parse_lscpu_model_unifies_big_little_groups() {
        // DGX Spark 实测片段（2026-09-03，lscpu | head -30 摘录）
        let spark_lscpu = "\
Architecture:                            aarch64\n\
CPU op-mode(s):                          64-bit\n\
Byte Order:                              Little Endian\n\
CPU(s):                                  20\n\
Vendor ID:                               ARM\n\
Model name:                              Cortex-X925\n\
Model:                                   1\n\
Thread(s) per core:                      1\n\
Core(s) per socket:                      10\n\
Socket(s):                               1\n\
Model name:                              Cortex-A725\n\
Model:                                   1\n\
Thread(s) per core:                      1\n\
Core(s) per socket:                      10\n\
Socket(s):                               1\n";
        assert_eq!(
            parse_lscpu_model(spark_lscpu),
            Some("Cortex-X925 + Cortex-A725".into()),
            "大小核两组去重保序拼接"
        );
        // x86 单组形态原样
        assert_eq!(
            parse_lscpu_model("Architecture: x86_64\nModel name: Intel i7\n"),
            Some("Intel i7".into())
        );
        // 同名多组去重
        assert_eq!(
            parse_lscpu_model("Model name: Apple M1\nModel name: Apple M1\n"),
            Some("Apple M1".into())
        );
        assert_eq!(parse_lscpu_model(""), None, "无 Model name 行 → None");
        assert_eq!(
            parse_lscpu_model("Model name: N/A\nModel name:  \n"),
            None,
            "N/A/空值不算型号"
        );
    }

    // 22d-3. 统一内存回退：unified 条目填 /proc/meminfo 池总量，独立显存卡不动
    #[test]
    fn apply_unified_vram_fills_unified_entries_only() {
        let mut gpus = vec![
            parse_nvidia_gpu_csv("0, NVIDIA GB10, [N/A]").unwrap(),
            parse_nvidia_gpu_csv("1, NVIDIA GeForce RTX 3090, 24576").unwrap(),
        ];
        apply_unified_vram(&mut gpus);
        // Linux 测试环境 /proc/meminfo 恒可读：GB10 条目带上池容量
        let gb10 = &gpus[0];
        assert!(gb10.unified_vram_mb.unwrap_or(0) > 0, "池总量应填: {gb10:?}");
        assert_eq!(gb10.vram_mb, None, "独立显存字段保持 null 语义");
        let rtx = &gpus[1];
        assert_eq!(rtx.vram_mb, Some(24576), "独立显存卡零回归");
        assert!(!rtx.unified_memory);
        assert_eq!(rtx.unified_vram_mb, None, "非 unified 不填");
    }

    // 22d-4. GB10 发布条目序列化形状：vram_mb=null 不占 JSON、unified 字段在
    #[test]
    fn gb10_entry_serializes_unified_fields() {
        let gpus = vec![parse_nvidia_gpu_csv("0, NVIDIA GB10, [N/A]").unwrap()];
        let probed = ServerConfig {
            gpu_name: gpus.first().map(|g| g.name.clone()),
            gpu_vram_mb: gpus.first().and_then(|g| g.vram_mb),
            gpu_count: u64::try_from(gpus.len()).ok(),
            gpus,
            ..Default::default()
        };
        assert_eq!(probed.gpu_name.as_deref(), Some("NVIDIA GB10"));
        assert_eq!(probed.gpu_vram_mb, None, "首卡 vram null → 镜像 null");
        let json = serde_json::to_value(&probed).unwrap();
        assert_eq!(json["gpu_count"], 1, "GB10 算 1 卡");
        assert_eq!(json["gpus"][0]["name"], "NVIDIA GB10");
        assert_eq!(json["gpus"][0]["unified_memory"], true);
        assert!(json["gpus"][0].get("vram_mb").is_none(), "null 不序列化");
        // 往返（联邦载荷/DB）：缺字段默认反序列化等价
        let back: ServerConfig = serde_json::from_value(json).unwrap();
        assert_eq!(back, probed);
    }

    // 22e. body 覆盖多卡（简化形态）：gpus=[{name,vram_mb}]×N（无 index）整组覆盖
    //      探测单卡；gpu_count/旧字段由胜出列表首卡推导
    #[test]
    fn merge_body_multi_gpu_simplified_form() {
        let probed = ServerConfig {
            gpu_name: Some("Probed Single".into()),
            gpu_vram_mb: Some(8000),
            gpu_count: Some(1),
            gpus: vec![GpuEntry {
                index: Some(0),
                name: "Probed Single".into(),
                vram_mb: Some(8000),
                ..Default::default()
            }],
            cpu_cores: Some(8),
            ..Default::default()
        };
        let body = ServerConfig {
            gpus: vec![
                GpuEntry {
                    index: None,
                    name: "NVIDIA GeForce RTX 4090".into(),
                    vram_mb: Some(24576),
                    ..Default::default()
                },
                GpuEntry {
                    index: None,
                    name: "NVIDIA GeForce RTX 4090".into(),
                    vram_mb: Some(24576),
                    ..Default::default()
                },
            ],
            model_name: Some("Qwen3.5-9B".into()),
            ..Default::default()
        };
        let merged = merge_server_config(Some(body), probed);
        assert_eq!(merged.gpus.len(), 2, "body 多卡整组覆盖探测单卡");
        assert_eq!(merged.gpu_count, Some(2), "gpu_count 从胜出列表推导");
        assert_eq!(
            merged.gpu_name.as_deref(),
            Some("NVIDIA GeForce RTX 4090"),
            "旧字段=胜出列表首卡"
        );
        assert_eq!(merged.gpu_vram_mb, Some(24576));
        assert_eq!(merged.cpu_cores, Some(8), "body 缺省落回探测");
        assert_eq!(merged.model_name.as_deref(), Some("Qwen3.5-9B"));
        // serde 形状：简化条目省略 index（发布表单组装端不带）
        let json = serde_json::to_value(&merged).unwrap();
        assert_eq!(json["gpus"].as_array().map(Vec::len), Some(2));
        assert!(
            json["gpus"][0].get("index").is_none(),
            "简化条目不序列化 index"
        );
        assert_eq!(json["gpus"][0]["name"], "NVIDIA GeForce RTX 4090");
        assert_eq!(json["gpus"][0]["vram_mb"], 24576);
        // 反序列化回来（DB 往返）：缺 index 默认 None
        let back: ServerConfig = serde_json::from_value(json).unwrap();
        assert_eq!(back, merged);
    }

    // 22f. 旧字段兼容：仅 body gpu_name+gpu_vram_mb（老客户端）→ 合成单卡 gpus；
    //      仅 gpu_name（部分覆盖）→ 列表落回探测、标量 body 胜出（旧语义保留）
    #[test]
    fn merge_legacy_gpu_fields_stay_compatible() {
        let probed = ServerConfig {
            gpu_name: Some("Probed GPU".into()),
            gpu_vram_mb: Some(100),
            gpu_count: Some(1),
            gpus: vec![GpuEntry {
                index: Some(0),
                name: "Probed GPU".into(),
                vram_mb: Some(100),
                ..Default::default()
            }],
            ..Default::default()
        };
        let body = ServerConfig {
            gpu_name: Some("Body GPU".into()),
            gpu_vram_mb: Some(200),
            ..Default::default()
        };
        let merged = merge_server_config(Some(body), probed.clone());
        assert_eq!(
            merged.gpu_name.as_deref(),
            Some("Body GPU"),
            "body 覆盖探测"
        );
        assert_eq!(merged.gpu_vram_mb, Some(200));
        assert_eq!(
            merged.gpus,
            vec![GpuEntry {
                index: None,
                name: "Body GPU".into(),
                vram_mb: Some(200),
                ..Default::default()
            }],
            "旧式双字段=合成单卡 gpus（新旧字段不打架）"
        );
        assert_eq!(merged.gpu_count, Some(1));
        // 仅 gpu_name（无 vram）：不合成，探测列表保留、标量覆盖
        let partial = merge_server_config(
            Some(ServerConfig {
                gpu_name: Some("Only Name".into()),
                ..Default::default()
            }),
            probed,
        );
        assert_eq!(partial.gpu_name.as_deref(), Some("Only Name"));
        assert_eq!(partial.gpus.len(), 1, "部分覆盖不合成，探测列表保留");
        assert_eq!(partial.gpu_vram_mb, Some(100), "缺省 vram 落回探测首卡");
    }

    // 23. 心跳时间窗：新鲜/过期/时钟超前宽容/解析失败
    #[test]
    fn heartbeat_age_window() {
        let now = chrono::Utc::now().timestamp();
        let iso = |offset: i64| iso_offset_secs(offset);
        assert_eq!(heartbeat_age_secs(&iso(0), now).map(|a| a.abs()), Some(0));
        assert!(
            heartbeat_age_secs(&iso(-30), now).is_some_and(|a| a <= 60),
            "30s 前=新鲜"
        );
        assert!(
            !heartbeat_age_secs(&iso(-120), now).is_some_and(|a| a <= 60),
            "2min 前=过期"
        );
        assert_eq!(heartbeat_age_secs("garbage", now), None, "解析失败 → None");
        assert!(!heartbeat_fresh("garbage"), "解析失败不判新鲜");
        assert!(heartbeat_fresh(&iso(0)), "刚上报=新鲜");
        assert!(!heartbeat_fresh(&iso(-61)), "61s 前=过期");
        assert!(heartbeat_fresh(&iso(5)), "时钟轻微超前宽容");
    }

    // =========================================================================
    // access_info（接入信息，2026-08-31）：迁移 / 发布更新 / 三视角脱敏
    // =========================================================================

    /// 24. 老库迁移：15 列旧表 → open（create_schema 内 migrate_add_columns）
    ///     幂等补 access_info/source_node/federated 三列，旧行按缺省读回。
    #[test]
    fn migrate_adds_federation_and_access_columns_idempotently() {
        let dir = std::env::temp_dir().join(format!(
            "api-market-mig-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("old.db");
        // 手工建 15 列旧表（2026-08-31 之前的形态）+ 一行存量数据。
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE api_market (
                    id TEXT PRIMARY KEY, api_name TEXT NOT NULL, description TEXT DEFAULT '',
                    endpoint_url TEXT NOT NULL, publisher_pubkey TEXT NOT NULL,
                    publisher_display TEXT DEFAULT '', server_config TEXT DEFAULT '{}',
                    pricing TEXT DEFAULT '{}', metrics_url TEXT, tags TEXT DEFAULT '[]',
                    status TEXT DEFAULT 'active', created_at TEXT, heartbeat_at TEXT,
                    load TEXT, download_count INTEGER DEFAULT 0
                );
                INSERT INTO api_market (id, api_name, endpoint_url, publisher_pubkey,
                    created_at, download_count)
                VALUES ('legacy-1', 'legacy-api', 'http://10.0.0.9:1/v1', '0xdead', 't', 5);",
            )
            .unwrap();
        }
        // 经 handler 构造打开（open_db → create_schema → migrate）。
        let h = ApiMarketRouteHandler::with_db_path(
            db_path.to_str().unwrap(),
            Arc::new(ChainAuth::new()),
        );
        let legacy = h
            .listings_snapshot()
            .into_iter()
            .find(|e| e.id == "legacy-1")
            .expect("存量行迁移后仍可读");
        assert_eq!(legacy.access_info, AccessInfo::default(), "旧行无接入信息");
        assert_eq!(legacy.source_node, "local", "旧行来源=本机发布");
        assert!(!legacy.federated, "旧行未推送联邦");
        assert_eq!(legacy.download_count, 5, "存量计数保留");
        // 幂等：再次建 schema（新 handler 同库）不报错、列不重复。
        let _h2 = ApiMarketRouteHandler::with_db_path(
            db_path.to_str().unwrap(),
            Arc::new(ChainAuth::new()),
        );
        {
            let conn = h.db.lock().expect("db poisoned");
            let mut cols = Vec::new();
            let mut stmt = conn.prepare("PRAGMA table_info(api_market)").unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, String>(1)).unwrap();
            for r in rows {
                cols.push(r.unwrap());
            }
            for col in ["access_info", "source_node", "federated"] {
                assert_eq!(
                    cols.iter().filter(|c| *c == col).count(),
                    1,
                    "{col} 恰一列（幂等迁移）: {cols:?}"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 25. access_info 发布：body 带则落库（发布响应明文——本人）；重发布
    ///     缺省保留既有；重发布带新值=更新（凭据轮换）。
    #[tokio::test]
    async fn access_info_publish_refresh_and_rotate() {
        let h = ApiMarketRouteHandler::with_empty();
        let (_, token) = login(&h, &new_key());
        let mut body = publish_body("access-target", Some(5));
        body["access_info"] = serde_json::json!({
            "api_key": "sk-os-abcdef1234567890",
            "auth_header": "X-Api-Key: <key>",
            "notes": "限流 10 qps",
        });
        let resp = h
            .handle(authed(HttpMethod::Post, PATH_PUBLISH, &token, body))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "{}", resp.body);
        let id = resp.body["id"].as_str().unwrap().to_string();
        assert_eq!(
            resp.body["access_info"]["api_key"], "sk-os-abcdef1234567890",
            "发布响应只回本人 → 明文"
        );
        assert_eq!(resp.body["access_info"]["auth_header"], "X-Api-Key: <key>");
        // 重发布（不带 access_info）→ 保留既有。
        let resp = h
            .handle(authed(
                HttpMethod::Post,
                PATH_PUBLISH,
                &token,
                publish_body("access-target", Some(6)),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "刷新: {}", resp.body);
        assert_eq!(
            resp.body["access_info"]["api_key"], "sk-os-abcdef1234567890",
            "缺省保留既有凭据"
        );
        // 重发布（带新值）→ 更新（轮换）；空串字段规范化为缺省（不序列化）。
        let mut body = publish_body("access-target", Some(6));
        body["access_info"] = serde_json::json!({ "api_key": " sk-os-rotated-9999 ", "notes": "  " });
        let resp = h
            .handle(authed(HttpMethod::Post, PATH_PUBLISH, &token, body))
            .await
            .unwrap();
        assert_eq!(
            resp.body["access_info"]["api_key"], "sk-os-rotated-9999",
            "新值覆盖 + trim"
        );
        assert!(
            resp.body["access_info"].get("notes").is_none(),
            "空串 notes 规范化掉: {}",
            resp.body["access_info"]
        );
        // 无 access_info 的普通挂牌：对象不出现（不占 JSON）。
        let other = publish_ok(&h, &token, "no-access", None).await;
        let detail = h.handle(get_req(&detail_path(&other))).await.unwrap();
        assert!(
            detail.body.get("access_info").is_none(),
            "空接入信息不序列化: {}",
            detail.body
        );
        let _ = id;
    }

    /// 26. access_info 脱敏三视角（列表+详情）：匿名 → <前4>***<后4>；他人链上
    ///     身份 → 同脱敏；publisher 本人 → 明文；admin（注入 token）→ 明文；
    ///     短 key（≤8 字符）→ 全掩 ****。
    #[tokio::test]
    async fn access_info_masking_by_viewer_matrix() {
        let h = ApiMarketRouteHandler::with_empty().with_admin_token("mkt-admin-tk");
        // 发布者 A 带 13 字符 key；发布者 B 带 8 字符短 key（全掩分支）。
        let sk_a = new_key();
        let (_, tok_a) = login(&h, &sk_a);
        let mut body_a = publish_body("mask-a", None);
        body_a["access_info"] = serde_json::json!({
            "api_key": "sk-os-abcdefgh1234",
            "auth_header": "X-Api-Key: <key>",
            "notes": "非敏感备注",
        });
        let id_a = publish_ok_body(&h, &tok_a, body_a).await;
        let sk_b = new_key();
        let (_, tok_b) = login(&h, &sk_b);
        let mut body_b = publish_body("mask-b", None);
        body_b["access_info"] = serde_json::json!({ "api_key": "short8ky" });
        let id_b = publish_ok_body(&h, &tok_b, body_b).await;

        // ① 匿名：列表 + 详情都脱敏（auth_header/notes 非敏感保持明文）。
        let list = h.handle(get_req(PATH_LIST)).await.unwrap();
        let a = list
            .body
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == id_a.as_str())
            .unwrap();
        assert_eq!(a["access_info"]["api_key"], "sk-o***1234", "匿名列表脱敏");
        assert_eq!(
            a["access_info"]["auth_header"],
            "X-Api-Key: <key>",
            "非敏感字段明文"
        );
        assert_eq!(a["access_info"]["notes"], "非敏感备注");
        let detail = h.handle(get_req(&detail_path(&id_a))).await.unwrap();
        assert_eq!(detail.body["access_info"]["api_key"], "sk-o***1234", "匿名详情脱敏");
        // 短 key：全掩（前4+后4 会拼出原文）。
        let detail_b = h.handle(get_req(&detail_path(&id_b))).await.unwrap();
        assert_eq!(detail_b.body["access_info"]["api_key"], "****", "短 key 全掩");

        // ② 他人链上身份（B 看 A 的条目）：脱敏。
        let other_view = h
            .handle(authed(
                HttpMethod::Get,
                &detail_path(&id_a),
                &tok_b,
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(
            other_view.body["access_info"]["api_key"], "sk-o***1234",
            "他人链上身份仍脱敏"
        );

        // ③ publisher 本人：明文（列表 + 详情）。
        let own_detail = h
            .handle(authed(
                HttpMethod::Get,
                &detail_path(&id_a),
                &tok_a,
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(
            own_detail.body["access_info"]["api_key"],
            "sk-os-abcdefgh1234",
            "本人明文"
        );
        let own_list = h
            .handle(authed(
                HttpMethod::Get,
                PATH_LIST,
                &tok_a,
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        let a = own_list
            .body
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == id_a.as_str())
            .unwrap();
        assert_eq!(a["access_info"]["api_key"], "sk-os-abcdefgh1234", "本人列表明文");
        // B 的短 key 对 B 本人也是明文（本人无脱敏分支）。
        let b_own = h
            .handle(authed(
                HttpMethod::Get,
                &detail_path(&id_b),
                &tok_b,
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(b_own.body["access_info"]["api_key"], "short8ky", "本人短 key 明文");

        // ④ admin（注入 token，读面特权）：明文；写面不回落（见测试 2/12 语义）。
        let admin_view = h
            .handle(authed(
                HttpMethod::Get,
                &detail_path(&id_a),
                "mkt-admin-tk",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(
            admin_view.body["access_info"]["api_key"],
            "sk-os-abcdefgh1234",
            "admin 读面明文"
        );
        // 垃圾 token（非 admin 非链上）→ 脱敏（caller None + admin 不匹配）。
        let junk_view = h
            .handle(authed(
                HttpMethod::Get,
                &detail_path(&id_a),
                "definitely-not-admin",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(junk_view.body["access_info"]["api_key"], "sk-o***1234");
    }

    /// 26b. access_info 脱敏 admin 新口径（2026-09-02 修「联邦导入丢 key」）：
    ///     admin 判定吃网关注入后的 Principal（`req.auth` 与
    ///     `crate::http::extract_principal` 同口径），四场景矩阵——
    ///     ① 默认注入 admin（无 Authorization 头 + NEXOS_AUTH_DEFAULT_ADMIN≠0）
    ///        → 明文（本节点浏览器无凭据一键导入联邦条目即此路）；
    ///     ② 带真 admin token（Bearer 精确等于系统 admin token）→ 明文；
    ///     ③ 他人链上 token → 脱敏（链上 token 不产 Principal，caller≠publisher）；
    ///     ④ 无注入模式（NEXOS_AUTH_DEFAULT_ADMIN=0 且无头）→ 脱敏。
    ///     Principal 一律用生产同款 `extract_principal` 构造（注入语义单一真相，
    ///     不手搓），并断言注入结果本身防组合回归。
    ///     env 竞态：复用 http.rs 的 TERMINAL_ENV_LOCK（改 NEXOS_AUTH_DEFAULT_ADMIN
    ///     的测试全走同一把锁串行，防并行互染）。
    #[tokio::test]
    async fn access_info_admin_principal_reveal_matrix() {
        let _env_guard = crate::http::tests::TERMINAL_ENV_LOCK.lock().await;

        // 发布者 A 挂牌带 key（形状同 26 号测试）。
        let h = ApiMarketRouteHandler::with_empty().with_admin_token("mkt-admin-tk");
        let sk_a = new_key();
        let (_, tok_a) = login(&h, &sk_a);
        let mut body = publish_body("reveal-a", None);
        body["access_info"] = serde_json::json!({ "api_key": "sk-os-reveal12345" });
        let id = publish_ok_body(&h, &tok_a, body).await;
        let path = detail_path(&id);
        // 网关 state 的 admin token（生产与 handler 同源于 NEXOS_ADMIN_TOKEN）。
        let gw_admin = Arc::new("mkt-admin-tk".to_string());
        let is_admin = |p: Option<&os_security::Principal>| {
            p.is_some_and(|p| p.roles.iter().any(|r| matches!(r, os_security::Role::Admin)))
        };

        // ① 默认注入 admin（无头）→ 明文。判别性场景：无头 → caller None、
        //    兜底 bearer 匹配也 None，只有 Principal 分支能揭示——旧实现此处
        //    被当匿名脱敏（用户一键导入没带上 key 的根因）。
        std::env::set_var("NEXOS_AUTH_DEFAULT_ADMIN", "1");
        let mut req = get_req(&path);
        req.auth =
            crate::http::extract_principal(&req.headers, None, Some(&gw_admin)).await;
        assert!(is_admin(req.auth.as_ref()), "无头请求默认注入 admin Principal");
        let resp = h.handle(req).await.unwrap();
        assert_eq!(
            resp.body["access_info"]["api_key"],
            "sk-os-reveal12345",
            "① 默认注入 admin → 明文"
        );

        // ② 真 admin token → 明文（带凭据请求不受注入开关影响，走精确匹配）。
        let mut req = authed(HttpMethod::Get, &path, "mkt-admin-tk", serde_json::Value::Null);
        req.auth =
            crate::http::extract_principal(&req.headers, None, Some(&gw_admin)).await;
        assert!(is_admin(req.auth.as_ref()), "admin token 精确匹配注入 admin Principal");
        let resp = h.handle(req).await.unwrap();
        assert_eq!(
            resp.body["access_info"]["api_key"],
            "sk-os-reveal12345",
            "② 真 admin token → 明文"
        );

        // ③ 他人链上 token → 脱敏（链上 token 非 admin/JWT → Principal None；
        //    handler caller 判定 B ≠ 发布者 A）。
        let sk_b = new_key();
        let (_, tok_b) = login(&h, &sk_b);
        let mut req = authed(HttpMethod::Get, &path, &tok_b, serde_json::Value::Null);
        req.auth =
            crate::http::extract_principal(&req.headers, None, Some(&gw_admin)).await;
        assert!(req.auth.is_none(), "链上 token 不产 Principal（保持脱敏）");
        let resp = h.handle(req).await.unwrap();
        assert_eq!(
            resp.body["access_info"]["api_key"],
            "sk-o***2345",
            "③ 他人链上身份 → 脱敏"
        );

        // ④ 无注入模式（env=0，无头）→ 脱敏（关闭测试期注入自然回匿名口径）。
        std::env::set_var("NEXOS_AUTH_DEFAULT_ADMIN", "0");
        let mut req = get_req(&path);
        req.auth =
            crate::http::extract_principal(&req.headers, None, Some(&gw_admin)).await;
        assert!(req.auth.is_none(), "关闭注入后无头请求为匿名");
        let resp = h.handle(req).await.unwrap();
        assert_eq!(
            resp.body["access_info"]["api_key"],
            "sk-o***2345",
            "④ 无注入模式 → 脱敏"
        );

        std::env::remove_var("NEXOS_AUTH_DEFAULT_ADMIN");
    }

    // —— 脱敏纯函数 ——

    // 26a. mask_api_key 边界：长 key 前4后4 / 短 key 全掩 / 空串原样。
    #[test]
    fn mask_api_key_boundaries() {
        assert_eq!(mask_api_key("sk-os-abcdefgh1234"), "sk-o***1234");
        assert_eq!(mask_api_key("123456789"), "1234***6789", "9 字符可拆");
        assert_eq!(mask_api_key("12345678"), "****", "8 字符全掩");
        assert_eq!(mask_api_key("abc"), "****", "超短全掩");
        assert_eq!(mask_api_key("  "), "", "空白 trim 后空串原样");
        // normalize_access_info：trim / 空→None；normalize_scope 三态。
        let n = normalize_access_info(AccessInfo {
            api_key: Some("  k  ".into()),
            auth_header: Some("  ".into()),
            notes: Some(" n ".into()),
        });
        assert_eq!(n.api_key.as_deref(), Some("k"));
        assert_eq!(n.auth_header, None);
        assert_eq!(n.notes.as_deref(), Some("n"));
        assert!(access_info_is_empty(&AccessInfo::default()));
        assert!(!access_info_is_empty(&n));
        assert_eq!(normalize_scope(None), "all", "缺省 all（兼容）");
        assert_eq!(normalize_scope(Some("local")), "local");
        assert_eq!(normalize_scope(Some("fed")), "fed");
        assert_eq!(normalize_scope(Some("bogus")), "all", "非法回落 all");
    }

    // 26b. curl 鉴权头两分支（2026-09-02 修复「-H 'Authorization Bearer' 无值」）：
    //     明文视角拼真实 key / 脱敏视角拼占位符（脱敏残值永不进 curl）；
    //     头形态：缺省与标准 Authorization Bearer 规范化带冒号、<key> 字面替换、
    //     自定义补冒号。
    #[test]
    fn curl_auth_header_line_plaintext_and_placeholder_branches() {
        let ph = CURL_TOKEN_PLACEHOLDER;
        // —— 明文分支（publisher/admin 视角）——
        // 缺省 auth_header（None）：标准 Bearer 头 + 真实 key。
        assert_eq!(
            curl_auth_header_line(None, Some("sk-os-abc123"), true, ph),
            ("-H 'Authorization: Bearer sk-os-abc123'".into(), false)
        );
        // 缺省值的字面形态「Authorization Bearer」（旧缺陷现场：按字面拼会无冒号
        // 无值）→ 规范化为带冒号带值。
        assert_eq!(
            curl_auth_header_line(Some("Authorization Bearer"), Some("sk-os-abc123"), true, ph),
            ("-H 'Authorization: Bearer sk-os-abc123'".into(), false)
        );
        assert_eq!(
            curl_auth_header_line(Some("authorization: bearer"), Some("k9"), true, ph),
            ("-H 'Authorization: Bearer k9'".into(), false)
        );
        // 自定义含 <key> 占位 → 字面替换。
        assert_eq!(
            curl_auth_header_line(Some("X-Api-Key: <key>"), Some("sk-os-abc123"), true, ph),
            ("-H 'X-Api-Key: sk-os-abc123'".into(), false)
        );
        // 自定义纯头名（无占位无冒号）→ 补冒号。
        assert_eq!(
            curl_auth_header_line(Some("X-Api-Key"), Some("k1"), true, ph),
            ("-H 'X-Api-Key: k1'".into(), false)
        );
        // 自定义已带冒号 → 直接拼值。
        assert_eq!(
            curl_auth_header_line(Some("X-Auth:"), Some("k2"), true, ph),
            ("-H 'X-Auth: k2'".into(), false)
        );

        // —— 占位分支（脱敏视角：api_key 是 <前4>***<后4> 残值）——
        // 残值绝不拼进 curl；输出占位符 + placeholder=true（调用方附索取说明）。
        assert_eq!(
            curl_auth_header_line(None, Some("sk-o***1234"), false, ph),
            (
                format!("-H 'Authorization: Bearer {ph}'"),
                true
            )
        );
        assert_eq!(
            curl_auth_header_line(
                Some("X-Api-Key: <key>"),
                Some("sk-o***1234"),
                false,
                "<TOKEN>"
            ),
            ("-H 'X-Api-Key: <TOKEN>'".into(), true)
        );
        // 特权视角但发布端没配 key → 同占位分支（占位符可由调用方本地化）。
        assert_eq!(
            curl_auth_header_line(None, None, true, ph),
            (
                format!("-H 'Authorization: Bearer {ph}'"),
                true
            )
        );
        // key 空白串等同缺配。
        assert_eq!(
            curl_auth_header_line(None, Some("   "), false, ph),
            (
                format!("-H 'Authorization: Bearer {ph}'"),
                true
            )
        );
        // auth_header 空白串等同缺省（回落标准 Bearer）。
        assert_eq!(
            curl_auth_header_line(Some("  "), Some("sk-os-x"), true, ph),
            ("-H 'Authorization: Bearer sk-os-x'".into(), false)
        );
    }

    // 26c. context_len 契约（2026-09-02）：发布 body 带 context_len → 透传到
    //     响应/列表/详情（此前被 serde 丢弃 → 大厅「上下文」恒 —）；
    //     merge：body 胜出、缺省落回探测（探测恒 None）；serde 往返 +
    //     存量 JSON（无 context_len）反序列化 = None（不猜）。
    #[test]
    fn context_len_serde_and_merge_contract() {
        // 发布 body 的 context_len 不再被丢弃。
        let sc: ServerConfig =
            serde_json::from_value(serde_json::json!({ "model_name": "M", "context_len": 16384 }))
                .unwrap();
        assert_eq!(sc.context_len, Some(16384));
        assert_eq!(sc.max_model_len, None, "两字段独立（互不覆盖）");
        // 序列化形状：字段出现；DB 往返（serde_json round-trip）保真。
        let json = serde_json::to_value(&sc).unwrap();
        assert_eq!(json["context_len"], 16384);
        assert_eq!(serde_json::from_value::<ServerConfig>(json).unwrap(), sc);
        // 存量 JSON（2026-09-02 之前落库的行）无 context_len → None（缺省兼容）。
        let legacy: ServerConfig =
            serde_json::from_value(serde_json::json!({ "model_name": "M" })).unwrap();
        assert_eq!(legacy.context_len, None);
        // max_model_len 老字段照旧透传（展示端回落用）。
        let old: ServerConfig =
            serde_json::from_value(serde_json::json!({ "model_name": "M", "max_model_len": 32768 }))
                .unwrap();
        assert_eq!(old.max_model_len, Some(32768));
        // merge：body context_len 胜出探测（探测恒 None）；body 缺省落回探测值。
        let probed = ServerConfig {
            cpu_cores: Some(8),
            ..Default::default()
        };
        let merged = merge_server_config(Some(sc), probed.clone());
        assert_eq!(merged.context_len, Some(16384));
        let none_body = ServerConfig {
            model_name: Some("M".into()),
            ..Default::default()
        };
        assert_eq!(
            merge_server_config(Some(none_body), probed).context_len,
            None,
            "body 缺省 → 探测（None）——真实无值，不猜"
        );
    }

    // 26d. context_len 端到端：发布 → 列表/详情/联邦载荷自然携带（不额外接线，
    //     序列化即透传）；未带 context_len 的条目输出不出现该字段。
    #[tokio::test]
    async fn context_len_publish_list_detail_and_fed_passthrough() {
        let (h, captured) = federated("node-207");
        let (_, token) = login(&h, &new_key());
        // 带 context_len 发布（发布表单直连 body 形态）。
        let mut body = publish_body("ctx-reporter", Some(3));
        body["server_config"] = serde_json::json!({
            "model_name": "Qwen3.5-9B",
            "context_len": 16384,
            "max_model_len": 8192
        });
        let resp = h
            .handle(authed(HttpMethod::Post, PATH_PUBLISH, &token, body))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "{}", resp.body);
        assert_eq!(resp.body["server_config"]["context_len"], 16384);
        assert_eq!(resp.body["server_config"]["max_model_len"], 8192);
        let id = resp.body["id"].as_str().unwrap().to_string();
        // 列表与详情同样携带（同一序列化路径）。
        let list = h.handle(get_req(PATH_LIST)).await.unwrap();
        assert_eq!(list.body[0]["server_config"]["context_len"], 16384);
        let detail = h.handle(get_req(&detail_path(&id))).await.unwrap();
        assert_eq!(detail.body["server_config"]["context_len"], 16384);
        // DB 往返（insert_listing → load_listings）保真。
        assert_eq!(
            h.listings_snapshot()[0].server_config.context_len,
            Some(16384)
        );
        // 联邦载荷自然携带（entry 序列化即透传，无需专门接线）。
        h.handle(authed(
            HttpMethod::Post,
            &federate_path(&id),
            &token,
            serde_json::Value::Null,
        ))
        .await
        .unwrap();
        {
            let payloads = captured.0.lock().unwrap();
            assert_eq!(payloads[0]["entry"]["server_config"]["context_len"], 16384);
        } // 锁作用域结束，不跨 await
        // 对照组：不带 context_len 的存量形态发布 → 输出不出现该字段（不猜 0/null）。
        let id2 = publish_ok(&h, &token, "ctx-legacy", None).await;
        let detail = h.handle(get_req(&detail_path(&id2))).await.unwrap();
        assert!(
            detail.body["server_config"].get("context_len").is_none(),
            "未上报 context_len 的条目不输出该字段: {}",
            detail.body["server_config"]
        );
    }

    // =========================================================================
    // 联邦大厅（api_market_lobby，2026-08-31）
    // =========================================================================

    /// 捕获型联邦通道（fake overlay：记录全部广播载荷——照 nexhub
    /// CapturedTransport 手法）。
    struct CapturedFed(std::sync::Mutex<Vec<serde_json::Value>>);

    /// 测试 fixture：内存库 handler + 已注入捕获通道。
    fn federated(node: &str) -> (ApiMarketRouteHandler, Arc<CapturedFed>) {
        let h = ApiMarketRouteHandler::with_empty();
        let captured = Arc::new(CapturedFed(std::sync::Mutex::new(Vec::new())));
        let sink = captured.clone();
        h.federation().set_transport(
            Arc::new(move |p| sink.0.lock().unwrap().push(p)),
            node.to_string(),
        );
        (h, captured)
    }

    /// 带 access_info 的挂牌 body（federate 载荷断言用）。
    async fn publish_ok_body(
        h: &ApiMarketRouteHandler,
        token: &str,
        body: serde_json::Value,
    ) -> String {
        let resp = h
            .handle(authed(HttpMethod::Post, PATH_PUBLISH, token, body))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "挂牌应 201: {}", resp.body);
        resp.body["id"].as_str().unwrap().to_string()
    }

    /// federate 路径实例化（/api/v1/api-market/:id/federate）。
    fn federate_path(id: &str) -> String {
        format!("{PATH_LIST}/{id}/federate")
    }

    /// 27. 两步联邦：publish 只写本地（不广播、federated=false）→ federate
    ///     端点推送 → 广播载荷 {fed, node, entry} 字段完整（owner 本人）。
    #[tokio::test]
    async fn fed_publish_local_then_federate_broadcasts_payload() {
        let (h, t) = federated("node-106");
        let (pubkey, token) = login(&h, &new_key());
        // 第一步：发布 → 仅本地（两步联邦，发布不广播）。
        let mut body = publish_body("qwen3.5-9b chat", Some(50));
        body["access_info"] = serde_json::json!({ "api_key": "sk-os-fed-123456" });
        let resp = h
            .handle(authed(HttpMethod::Post, PATH_PUBLISH, &token, body))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["federated"], false, "发布恒未推送（两步联邦第一步）");
        assert_eq!(resp.body["source_node"], "local");
        assert!(
            t.0.lock().unwrap().is_empty(),
            "发布不广播——联邦只能从本地已发布条目推送"
        );
        let id = resp.body["id"].as_str().unwrap().to_string();
        // 第二步：owner 本人推送 → 广播一次。
        let resp = h
            .handle(authed(
                HttpMethod::Post,
                &federate_path(&id),
                &token,
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "推送应 200: {resp:?}");
        assert_eq!(resp.body["ok"], true);
        assert_eq!(resp.body["federated"], true);
        assert_eq!(resp.body["first_push"], true);
        {
            let payloads = t.0.lock().unwrap();
            assert_eq!(payloads.len(), 1, "推送应广播一次: {payloads:?}");
            let p = &payloads[0];
            assert_eq!(p["fed"], FED_KIND_API_MARKET_LOBBY);
            assert_eq!(p["node"], "node-106");
            assert_eq!(p["entry"]["id"], id.as_str());
            assert_eq!(p["entry"]["api_name"], "qwen3.5-9b chat");
            assert_eq!(p["entry"]["publisher_pubkey"], pubkey);
            assert_eq!(p["entry"]["source_node"], "local", "发送端条目恒 local");
            assert_eq!(p["entry"]["federated"], true, "载荷携带推送标志");
            assert_eq!(
                p["entry"]["access_info"]["api_key"], "sk-os-fed-123456",
                "凭据随快照联邦分发（对端输出仍按视角脱敏）"
            );
        } // 锁作用域结束，不跨下方 await
        // 标志落库：DB 快照 + HTTP 列表（前端 🌐 标记依据）。
        assert!(h.listings_snapshot()[0].federated);
        let list = h.handle(get_req(PATH_LIST)).await.unwrap();
        assert_eq!(list.body[0]["federated"], true);
        // 重复推送 = 重新广播（first_push=false，载荷再来一次）。
        let resp = h
            .handle(authed(
                HttpMethod::Post,
                &federate_path(&id),
                &token,
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.body["first_push"], false, "重复推送非首次");
        assert_eq!(t.0.lock().unwrap().len(), 2, "重新广播一次");
    }

    /// 28. federate 鉴权矩阵：无 token 401 / admin token 401（无回落）/
    ///     他人 403「仅发布者可推送联邦」/ 未知 id 404 / 远程条目不可在本节点推送。
    #[tokio::test]
    async fn federate_owner_only_and_404() {
        let (h, t) = federated("node-a");
        let owner = new_key();
        let (pubkey, owner_token) = login(&h, &owner);
        let id = publish_ok(&h, &owner_token, "fed-guard", None).await;
        let path = federate_path(&id);
        // 无 token → 401。
        let resp = h
            .handle(ApiRequest {
                method: HttpMethod::Post,
                path: path.clone(),
                headers: serde_json::json!({}),
                body: serde_json::Value::Null,
                auth: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.status, 401);
        // admin token → 401（写面无 admin 回落——推送者必须是链上身份）。
        let resp = h
            .handle(authed(
                HttpMethod::Post,
                &path,
                "nexos-admin-secret-token",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 401, "admin token 不应可推送联邦");
        // 他人链上身份 → 403 + 定稿文案。
        let (_, other_token) = login(&h, &new_key());
        let resp = h
            .handle(authed(
                HttpMethod::Post,
                &path,
                &other_token,
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 403);
        assert!(
            resp.body["error"].as_str().unwrap().contains("仅发布者可推送联邦"),
            "403 文案契约: {}",
            resp.body
        );
        // 未知 id → 404。
        let resp = h
            .handle(authed(
                HttpMethod::Post,
                &federate_path("no-such"),
                &owner_token,
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
        assert!(
            t.0.lock().unwrap().is_empty(),
            "全部失败路径零广播"
        );
        // 远程条目不可在本节点推送：落一条**同 owner pubkey** 的远程条目再试
        // （pubkey 一致才能过 owner 校验，走到远程条目分支——引导回源节点）。
        let remote_entry = serde_json::json!({
            "id": "remote-9", "api_name": "remote-api", "endpoint_url": "http://10.0.0.9:1/v1",
            "publisher_pubkey": pubkey, "created_at": "t",
        });
        assert_eq!(
            h.federation().ingest(&serde_json::json!({
                "fed": FED_KIND_API_MARKET_LOBBY, "node": "node-b", "entry": remote_entry,
            })),
            ApiMarketFedIngest::Written
        );
        let resp = h
            .handle(authed(
                HttpMethod::Post,
                &federate_path("remote-9"),
                &owner_token,
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 403, "远程条目不可转发: {resp:?}");
        assert!(
            resp.body["error"]
                .as_str()
                .unwrap()
                .contains("不可在本节点推送"),
            "文案应引导回源节点: {}",
            resp.body
        );
    }

    /// 29. 接收端幂等合并：新快照 Written（计数清零、来源标记）→ 逐字节重放
    ///     Duplicate → 同源新快照 Refreshed（保留本地计数）→ 异源同 id Skipped。
    #[tokio::test]
    async fn fed_ingest_idempotent_merge() {
        let (h, _t) = federated("node-113");
        let entry = |price: u64, name: &str| {
            serde_json::json!({
                "id": "fed-entry-1", "api_name": name, "endpoint_url": "http://10.0.0.9:1/v1",
                "publisher_pubkey": "0xabc", "publisher_display": "0x1111",
                "pricing": { "mode": "per_token", "price_per_1k_tokens": price },
                "server_config": { "model_name": "Qwen3.5-9B" },
                "created_at": "2026-08-31T10:00:00+08:00",
                "download_count": 42, "federated": true,
                "access_info": { "api_key": "sk-os-fed-abcdef99" },
            })
        };
        let payload = |node: &str, e: serde_json::Value| {
            serde_json::json!({ "fed": FED_KIND_API_MARKET_LOBBY, "node": node, "entry": e })
        };
        // ① 新条目 → Written：source_node=载荷来源节点、本地计数清零（对端
        //    计数是它的活跃度）、federated 随载荷、access_info 落库。
        assert_eq!(
            h.federation().ingest(&payload("node-106", entry(10, "fed-api"))),
            ApiMarketFedIngest::Written
        );
        let saved = &h.listings_snapshot()[0];
        assert_eq!(saved.id, "fed-entry-1");
        assert_eq!(saved.source_node, "node-106", "来源改写为载荷发布节点");
        assert_eq!(saved.download_count, 0, "本地计数清零起步");
        assert!(saved.federated, "推送标志随载荷");
        assert_eq!(saved.access_info.api_key.as_deref(), Some("sk-os-fed-abcdef99"));
        // ② 逐字节重放 → Duplicate（未触碰 DB——计数注入后不变）。
        {
            let conn = h.db.lock().expect("db poisoned");
            conn.execute(
                "UPDATE api_market SET download_count=7 WHERE id='fed-entry-1'",
                [],
            )
            .unwrap();
        }
        assert_eq!(
            h.federation().ingest(&payload("node-106", entry(10, "fed-api"))),
            ApiMarketFedIngest::Duplicate
        );
        assert_eq!(h.listings_snapshot()[0].download_count, 7, "重放不触碰 DB");
        // ③ 同源新快照（改价）→ Refreshed：覆盖快照、保留本地计数。
        assert_eq!(
            h.federation().ingest(&payload("node-106", entry(20, "fed-api"))),
            ApiMarketFedIngest::Refreshed
        );
        let saved = &h.listings_snapshot()[0];
        assert_eq!(saved.pricing.price_per_1k_tokens, Some(20), "快照已刷新");
        assert_eq!(saved.download_count, 7, "本地计数保留");
        assert_eq!(saved.source_node, "node-106", "同源刷新来源不变");
        // ④ 异源同 id → Skipped（保护本地条目，快照不动）。
        assert_eq!(
            h.federation().ingest(&payload("node-b", entry(99, "fed-api"))),
            ApiMarketFedIngest::Skipped
        );
        assert_eq!(
            h.listings_snapshot()[0].pricing.price_per_1k_tokens,
            Some(20),
            "异源被拒，快照保持"
        );
        // ⑤ 非法载荷矩阵：错 fed kind / 缺 node / 缺 entry / 必填缺失 / 坏 URL。
        assert_eq!(
            h.federation().ingest(&serde_json::json!({"fed": "nexhub_lobby"})),
            ApiMarketFedIngest::Invalid
        );
        assert_eq!(
            h.federation().ingest(&serde_json::json!({
                "fed": FED_KIND_API_MARKET_LOBBY, "entry": {}
            })),
            ApiMarketFedIngest::Invalid,
            "缺 node"
        );
        assert_eq!(
            h.federation().ingest(&serde_json::json!({
                "fed": FED_KIND_API_MARKET_LOBBY, "node": "n"
            })),
            ApiMarketFedIngest::Invalid,
            "缺 entry"
        );
        assert_eq!(
            h.federation().ingest(&payload("n", serde_json::json!({
                "id": "x", "endpoint_url": "http://a/b", "publisher_pubkey": "0x1",
                "created_at": "t",
            }))),
            ApiMarketFedIngest::Invalid,
            "缺 api_name"
        );
        assert_eq!(
            h.federation().ingest(&payload("n", serde_json::json!({
                "id": "x", "api_name": "y", "endpoint_url": "ftp://bad",
                "publisher_pubkey": "0x1", "created_at": "t",
            }))),
            ApiMarketFedIngest::Invalid,
            "非 http(s) endpoint"
        );
    }

    /// 30. 名+发布者兜底去重：payload id 与本地不同但 api_name+publisher_pubkey
    ///     命中同源条目 → Refreshed 沿用本地 id（唯一索引主键稳定）。
    #[tokio::test]
    async fn fed_ingest_name_owner_fallback_refresh() {
        let (h, _t) = federated("node-x");
        // 先收一条 id=A 的远程条目。
        let mk = |id: &str| {
            serde_json::json!({
                "id": id, "api_name": "same-name", "endpoint_url": "http://10.0.0.9:1/v1",
                "publisher_pubkey": "0xsame", "created_at": "t", "download_count": 0,
                "pricing": { "mode": "free" },
            })
        };
        h.federation()
            .ingest(&serde_json::json!({ "fed": FED_KIND_API_MARKET_LOBBY, "node": "node-x", "entry": mk("id-a") }));
        // 对端换了 id（重建条目）但名+发布者相同、同源 → Refreshed 沿用本地 id-a。
        assert_eq!(
            h.federation().ingest(&serde_json::json!({
                "fed": FED_KIND_API_MARKET_LOBBY, "node": "node-x", "entry": mk("id-b")
            })),
            ApiMarketFedIngest::Refreshed
        );
        let list = h.listings_snapshot();
        assert_eq!(list.len(), 1, "不重复建行: {list:?}");
        assert_eq!(list[0].id, "id-a", "沿用本地 id");
    }

    /// 31. 联邦桥分发：FederationBridge{api_market} 收 api_market_lobby 载荷 →
    ///     ingest 落库 → scope=fed 列表可见（bridge 集成最小闭环）。
    #[tokio::test]
    async fn bridge_dispatches_api_market_lobby_payload() {
        use crate::handlers::p2p::FederationBridge;
        let (h, _t) = federated("node-113");
        let bridge = FederationBridge {
            im: None,
            nexhub: None,
            live: None,
            api_market: Some(h.federation()),
        };
        let payload = serde_json::json!({
            "fed": FED_KIND_API_MARKET_LOBBY, "node": "node-106",
            "entry": {
                "id": "br-1", "api_name": "bridge-api", "endpoint_url": "http://10.0.0.9:1/v1",
                "publisher_pubkey": "0xpk", "created_at": "t",
                "access_info": { "api_key": "sk-os-bridge-1122" },
            },
        });
        let from = os_p2p::NodeIdentity::generate().node_id();
        bridge.dispatch(&os_p2p::P2pMsg {
            from: from.clone(),
            hops: 0,
            ttl: 8,
            payload: payload.clone(),
        });
        // scope=fed 可见；scope=local 不可见；缺省 all 可见（向后兼容平铺）。
        let fed_list = h.handle(get_req(&format!("{PATH_LIST}?scope=fed"))).await.unwrap();
        assert_eq!(fed_list.body.as_array().unwrap().len(), 1);
        assert_eq!(fed_list.body[0]["api_name"], "bridge-api");
        assert_eq!(fed_list.body[0]["source_node"], "node-106");
        // 桥路径记录验签来源 NodeID（dispatch → ingest_from；via_node 的来源）。
        assert_eq!(fed_list.body[0]["source_node_id"], from.to_hex());
        // 接收端匿名视角：api_key 脱敏（凭据联邦分发但输出按视角脱敏）。
        assert_eq!(fed_list.body[0]["access_info"]["api_key"], "sk-o***1122");
        let local_list = h.handle(get_req(&format!("{PATH_LIST}?scope=local"))).await.unwrap();
        assert_eq!(local_list.body.as_array().unwrap().len(), 0);
        let all_list = h.handle(get_req(PATH_LIST)).await.unwrap();
        assert_eq!(all_list.body.as_array().unwrap().len(), 1, "缺省 all 兼容");
        // 未注册的 fed kind（无 api_market 消费端的桥不炸——这里已注册，
        // 验证未知 kind 静默忽略零写入）。
        bridge.dispatch(&os_p2p::P2pMsg {
            from: os_p2p::NodeId::from_verifying_key(new_key().verifying_key()),
            hops: 0,
            ttl: 8,
            payload: serde_json::json!({"fed": "unknown_kind"}),
        });
        assert_eq!(h.listings_snapshot().len(), 1);
    }

    /// 32. 列表 scope 过滤 + 本地/联邦混合排序兼容：响应恒为平铺数组（旧客户
    ///     端零改动——元素新增 source_node/federated/access_info 字段）。
    #[tokio::test]
    async fn list_scope_filters_keep_flat_array_shape() {
        let (h, _t) = federated("node-113");
        let (_, token) = login(&h, &new_key());
        publish_ok(&h, &token, "local-one", Some(1)).await;
        h.federation().ingest(&serde_json::json!({
            "fed": FED_KIND_API_MARKET_LOBBY, "node": "node-106",
            "entry": {
                "id": "f1", "api_name": "fed-one", "endpoint_url": "http://10.0.0.9:1/v1",
                "publisher_pubkey": "0xpk", "created_at": "t2",
            },
        }));
        // 缺省 all：两元素数组（向后兼容形态）。
        let all = h.handle(get_req(PATH_LIST)).await.unwrap();
        assert_eq!(all.body.as_array().unwrap().len(), 2);
        assert!(all.body.is_array(), "响应保持数组（不换成 {{local,federated}} 对象）");
        // scope=local / scope=fed。
        let local = h.handle(get_req(&format!("{PATH_LIST}?scope=local"))).await.unwrap();
        let names: Vec<&str> = local
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["api_name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["local-one"]);
        let fed = h.handle(get_req(&format!("{PATH_LIST}?scope=fed"))).await.unwrap();
        let names: Vec<&str> = fed
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["api_name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["fed-one"]);
        // q 搜索与 scope 正交组合。
        let q = h
            .handle(get_req(&format!("{PATH_LIST}?scope=fed&q=fed")))
            .await
            .unwrap();
        assert_eq!(q.body.as_array().unwrap().len(), 1);
        let q2 = h
            .handle(get_req(&format!("{PATH_LIST}?scope=fed&q=local")))
            .await
            .unwrap();
        assert_eq!(q2.body.as_array().unwrap().len(), 0);
        // sort=price 与 scope 组合（fed 条目无价=免费垫底不影响形态）。
        let ps = h
            .handle(get_req(&format!("{PATH_LIST}?scope=all&sort=price")))
            .await
            .unwrap();
        assert_eq!(ps.body.as_array().unwrap().len(), 2);
    }

    /// 33. 本地下架不撤远端：DELETE 零广播（远端副本由源节点重新推送刷新——
    ///     照 NexHub 语义，模块文档已写明）。
    #[tokio::test]
    async fn delete_does_not_broadcast_unfederate() {
        let (h, t) = federated("node-106");
        let (_, token) = login(&h, &new_key());
        let id = publish_ok(&h, &token, "to-unlist", None).await;
        // 推送（一次广播）→ 下架（零新增广播）。
        let _ = h
            .handle(authed(
                HttpMethod::Post,
                &federate_path(&id),
                &token,
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(t.0.lock().unwrap().len(), 1, "仅 federate 广播过一次");
        let resp = h
            .handle(authed(
                HttpMethod::Delete,
                &detail_path(&id),
                &token,
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(
            t.0.lock().unwrap().len(),
            1,
            "下架不广播撤销载荷（远端副本不受影响）"
        );
    }

    /// 34. 联邦条目的 metrics 代拉：本地无心跳 → metrics_url 代拉（指向源节点）
    ///     ——消费者节点上的联邦条目负载监控同样可用。
    #[tokio::test]
    async fn federated_entry_metrics_falls_back_to_metrics_url() {
        let (h, _t) = federated("node-113");
        let (port, _hits) = spawn_fake_json_server(vec![serde_json::json!({
            "metrics": { "running": 5 }
        })
        .to_string()]);
        h.federation().ingest(&serde_json::json!({
            "fed": FED_KIND_API_MARKET_LOBBY, "node": "node-106",
            "entry": {
                "id": "fm-1", "api_name": "fed-metrics", "endpoint_url": "http://10.0.0.9:1/v1",
                "publisher_pubkey": "0xpk", "created_at": "t",
                "metrics_url": format!("http://127.0.0.1:{port}/metrics"),
            },
        }));
        let metrics = h.handle(get_req(&metrics_path("fm-1"))).await.unwrap();
        assert_eq!(metrics.status, 200);
        assert_eq!(metrics.body["source"], "metrics_url", "无本地心跳走代拉");
        assert_eq!(metrics.body["reachable"], true);
        assert_eq!(metrics.body["metrics"]["running"], 5.0);
    }

    // =========================================================================
    // 跨网中继（api_relay_req / api_relay_resp，2026-09-02）
    // =========================================================================

    /// 全量 mock 上游（真 TCP）：读完整个请求（头 + Content-Length 体），
    /// script 拿请求原文与流写出响应。返回 (port, 收到的原始请求文本)。
    fn spawn_full_upstream<F>(script: F) -> (u16, std::sync::Arc<std::sync::Mutex<String>>)
    where
        F: FnOnce(&str, &mut std::net::TcpStream) + Send + 'static,
    {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 失败");
        let port = listener.local_addr().expect("local_addr 失败").port();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let seen2 = seen.clone();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
            let mut buf: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 8192];
            loop {
                match stream.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        let s = String::from_utf8_lossy(&buf);
                        if let Some(pos) = s.find("\r\n\r\n") {
                            let cl = s[..pos]
                                .lines()
                                .find_map(|l| {
                                    let (k, v) = l.split_once(':')?;
                                    if k.trim().eq_ignore_ascii_case("content-length") {
                                        v.trim().parse::<usize>().ok()
                                    } else {
                                        None
                                    }
                                })
                                .unwrap_or(0);
                            if buf.len() >= pos + 4 + cl {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            let req_text = String::from_utf8_lossy(&buf).to_string();
            *seen2.lock().unwrap() = req_text.clone();
            script(&req_text, &mut stream);
            let _ = stream.flush();
        });
        (port, seen)
    }

    /// fake 互连 overlay fixture（live.rs 同款手法）：消费者 a ↔ 源端 source
    /// 定向互投（send_to → 对端 dispatch，验签 from 用对端真实 NodeID）。
    /// 返回 (消费者端点, 源端 NodeID hex)。
    fn relay_pair(source: ApiMarketFedEndpoint) -> (ApiMarketFedEndpoint, String) {
        let a = ApiMarketFedEndpoint::test_endpoint();
        let a_id = os_p2p::NodeIdentity::generate().node_id();
        let b_id = os_p2p::NodeIdentity::generate().node_id();
        let a_hex = a_id.to_hex();
        let b_hex = b_id.to_hex();
        // A → B 定向：帧直达 B 的 dispatch（验签方 = A）。
        let b2 = source.clone();
        let b_target = b_id.clone();
        let a_from = a_id.clone();
        a.set_full_transport(
            Arc::new(move |to, payload| {
                if *to == b_target {
                    b2.dispatch(&a_from, &payload);
                }
            }),
            Arc::new(|_| {}),
            a_hex,
            "node-a".into(),
        );
        // B → A 定向：resp 帧直达 A 的 dispatch（验签方 = B——伪造应答测试的锚）。
        let a3 = a.clone();
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
            "node-b".into(),
        );
        (a, b_hex)
    }

    fn chat_json_body() -> String {
        serde_json::json!({
            "id": "chatcmpl-relay-1", "model": "qwen3.5-9b",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "经中继的回复"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 7, "total_tokens": 12},
        })
        .to_string()
    }

    /// 35. 白名单封闭集合（纯函数）：仅 {E, E/models, E/chat/completions}
    ///     （归一化后精确比对）；尾斜杠等价；任意其他路径/主机/端口/穿越全拒。
    #[test]
    fn relay_whitelist_closed_set_unit() {
        let published = vec!["http://10.0.0.9:8000/v1".to_string()];
        for allowed in [
            "http://10.0.0.9:8000/v1",
            "http://10.0.0.9:8000/v1/",
            "http://10.0.0.9:8000/v1/models",
            "http://10.0.0.9:8000/v1/chat/completions",
        ] {
            assert!(relay_url_allowed(&published, allowed), "应放行: {allowed}");
        }
        for denied in [
            "http://10.0.0.9:8000/v1/completions",
            "http://10.0.0.9:8000/",
            "http://10.0.0.9:8000",
            "http://10.0.0.9:8001/v1/models",
            "http://evil.com/v1/models",
            "http://10.0.0.9:8000/v1/../metrics",
            "ftp://10.0.0.9:8000/v1",
            "not a url",
        ] {
            assert!(!relay_url_allowed(&published, denied), "应拒绝: {denied}");
        }
        // 空发布集 = 全拒（未发布任何条目的节点不是代理）。
        assert!(!relay_url_allowed(&[], "http://10.0.0.9:8000/v1/models"));
        // 尾斜杠形态的已发布条目与请求两侧等价归一。
        let trailing = vec!["http://10.0.0.9:8000/v1/".to_string()];
        assert!(relay_url_allowed(&trailing, "http://10.0.0.9:8000/v1/models"));
    }

    /// 36. 端到端·非流式 chat：消费者 → fake overlay → 源端白名单放行 →
    ///     真实代发 mock 上游 → resp 帧回传聚合；鉴权头透传；usage 原样。
    #[tokio::test]
    async fn relay_roundtrip_chat_via_fake_overlay() {
        let (port, seen) = spawn_full_upstream(|req, s| {
            assert!(
                req.starts_with("POST /v1/chat/completions "),
                "上游应收到 chat 路径: {req}"
            );
            use std::io::Write;
            let body = chat_json_body();
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            );
        });
        let base = format!("http://127.0.0.1:{port}/v1");
        let b = ApiMarketFedEndpoint::test_endpoint_with_local_listing(&base);
        let (a, b_hex) = relay_pair(b);
        let req = ApiRelayRequest {
            method: "POST".into(),
            url: format!("{base}/chat/completions"),
            headers: vec![("Authorization".into(), "Bearer sk-relay-key".into())],
            body: Some(br#"{"model":"qwen3.5-9b","messages":[]}"#.to_vec()),
            stream: false,
        };
        let done = a
            .relay_roundtrip(&b_hex, req, Duration::from_secs(5))
            .await
            .expect("中继整包应成功");
        assert_eq!(done.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&done.body).expect("上游 JSON 原样");
        assert_eq!(v["choices"][0]["message"]["content"], "经中继的回复");
        assert_eq!(v["usage"]["total_tokens"], 12);
        assert!(
            done.headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("content-type")
                    && v.contains("application/json")),
            "响应头透传: {:?}",
            done.headers
        );
        // 请求侧：鉴权头原样到达上游（hyper 小写头名，大小写不敏感比对）。
        let seen = seen.lock().unwrap().clone();
        assert!(
            seen.to_ascii_lowercase()
                .contains("authorization: bearer sk-relay-key"),
            "鉴权头应经中继透传: {seen}"
        );
    }

    /// 37. 白名单拒绝（403 定稿文案）与方法限制（仅 GET/POST）——绝不做开放代理。
    #[tokio::test]
    async fn relay_whitelist_and_method_rejections() {
        // 上游正常起（若源端误外呼会命中 script——测试失败兜底），但请求的
        // URL（127.0.0.1:1）不属任何已发布条目。
        let (port, seen) = spawn_full_upstream(|_req, s| {
            use std::io::Write;
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok");
        });
        let base = format!("http://127.0.0.1:{port}/v1");
        let b = ApiMarketFedEndpoint::test_endpoint_with_local_listing(&base);
        let (a, b_hex) = relay_pair(b);
        // 未发布 URL → 403 + 定稿文案（即便同主机不同端口）。
        let req = ApiRelayRequest {
            method: "GET".into(),
            url: "http://127.0.0.1:1/v1/models".into(),
            headers: vec![],
            body: None,
            stream: false,
        };
        let done = a
            .relay_roundtrip(&b_hex, req, Duration::from_secs(5))
            .await
            .expect("403 也是一次完整中继应答");
        assert_eq!(done.status, 403);
        assert_eq!(
            String::from_utf8_lossy(&done.body),
            "该 URL 不属于本节点发布的条目"
        );
        // 方法限制：DELETE → 403。
        let req = ApiRelayRequest {
            method: "DELETE".into(),
            url: format!("{base}/models"),
            headers: vec![],
            body: None,
            stream: false,
        };
        let done = a
            .relay_roundtrip(&b_hex, req, Duration::from_secs(5))
            .await
            .expect("方法拒绝也是一次完整中继应答");
        assert_eq!(done.status, 403);
        assert!(
            String::from_utf8_lossy(&done.body).contains("仅支持 GET/POST"),
            "方法限制文案: {}",
            String::from_utf8_lossy(&done.body)
        );
        // 两次拒绝均零外呼（mock 上游未被触碰）。
        assert!(seen.lock().unwrap().is_empty(), "拒绝路径不得外呼上游");
    }

    /// 38. 端到端·流式：SSE 逐块透传（帧序即字节序），Head 带 status/headers，
    ///     尾帧 Done → next_chunk 收 None 正常断流。
    #[tokio::test]
    async fn relay_stream_chunks_in_order() {
        let (port, _seen) = spawn_full_upstream(|_req, s| {
            use std::io::Write;
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n");
            let _ = s.flush();
            std::thread::sleep(Duration::from_millis(50));
            let _ = s.write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"chunk-a\"}}]}\n\n");
            let _ = s.flush();
            std::thread::sleep(Duration::from_millis(50));
            let _ = s.write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"chunk-b\"}}]}\n\n");
            let _ = s.flush();
            std::thread::sleep(Duration::from_millis(50));
            let _ = s.write_all(b"data: [DONE]\n\n");
        });
        let base = format!("http://127.0.0.1:{port}/v1");
        let b = ApiMarketFedEndpoint::test_endpoint_with_local_listing(&base);
        let (a, b_hex) = relay_pair(b);
        let req = ApiRelayRequest {
            method: "POST".into(),
            url: format!("{base}/chat/completions"),
            headers: vec![],
            body: Some(br#"{"model":"m","stream":true}"#.to_vec()),
            stream: true,
        };
        let mut stream = a
            .relay_open_stream(&b_hex, req, Duration::from_secs(5))
            .await
            .expect("流式中继应建立");
        assert_eq!(stream.status, 200);
        assert!(
            stream
                .headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("content-type")
                    && v.contains("text/event-stream")),
            "Head 帧带响应头: {:?}",
            stream.headers
        );
        let mut collected: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next_chunk().await {
            collected.extend_from_slice(&chunk.expect("块应无错"));
        }
        let text = String::from_utf8_lossy(&collected);
        assert!(text.contains("\"content\":\"chunk-a\""), "第一块在前: {text}");
        assert!(text.contains("\"content\":\"chunk-b\""), "第二块在后: {text}");
        assert!(text.contains("data: [DONE]"), "收尾帧透传: {text}");
        assert!(
            text.find("\"content\":\"chunk-a\"").unwrap() < text.find("\"content\":\"chunk-b\"").unwrap(),
            "帧序即字节序"
        );
    }

    /// 39. 分块重组：>1 MiB 请求体按 1 MiB 分块多帧（cn=2），源端按 ci 序拼回
    ///     完整 body 再代发——上游收到的字节数与发送侧一致。
    #[tokio::test]
    async fn relay_request_body_chunked_over_1mib() {
        // echo 长度上游：读完请求后回请求体字节数（content-length 已在
        // spawn_full_upstream 的读循环里对齐——脚本拿到的即完整请求）。
        let (port, seen) = spawn_full_upstream(|req, s| {
            use std::io::Write;
            let body = req.split("\r\n\r\n").nth(1).unwrap_or("");
            let resp = body.len().to_string();
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{resp}",
                    resp.len()
                )
                .as_bytes(),
            );
        });
        let base = format!("http://127.0.0.1:{port}/v1");
        let b = ApiMarketFedEndpoint::test_endpoint_with_local_listing(&base);
        let (a, b_hex) = relay_pair(b);
        let payload = vec![b'x'; RELAY_CHUNK_BYTES + RELAY_CHUNK_BYTES / 2]; // 1.5 MiB → 2 帧
        let req = ApiRelayRequest {
            method: "POST".into(),
            url: format!("{base}/chat/completions"),
            headers: vec![],
            body: Some(payload.clone()),
            stream: false,
        };
        let done = a
            .relay_roundtrip(&b_hex, req, Duration::from_secs(10))
            .await
            .expect("分块请求应重组成功");
        assert_eq!(done.status, 200);
        assert_eq!(
            String::from_utf8_lossy(&done.body),
            payload.len().to_string(),
            "上游收到的字节数 = 发送侧完整 body"
        );
        // 请求侧确实以 content-length 整体到达（非分块残缺）。
        let seen = seen.lock().unwrap().clone();
        assert!(
            seen.contains(&format!("content-length: {}", payload.len())),
            "上游应见完整 content-length: {}",
            &seen[..seen.len().min(200)]
        );
    }

    /// 40. 超时清理：黑洞通道（对端不回帧）→ roundtrip Err（首帧超时）；
    ///     pending 关联不即时消失（无 resp 可清），巡检 sweep 按 TTL 收走。
    #[tokio::test]
    async fn relay_pending_timeout_and_sweep_cleanup() {
        let a = ApiMarketFedEndpoint::test_endpoint();
        let a_id = os_p2p::NodeIdentity::generate().node_id();
        // 黑洞定向面：帧发出去但没有对端（对端失联/未组网场景）。
        a.set_full_transport(
            Arc::new(|_to, _payload| {}),
            Arc::new(|_| {}),
            a_id.to_hex(),
            "node-a".into(),
        );
        let limits = RelayLimits {
            req_timeout: Duration::from_secs(1),
            stream_first: Duration::from_millis(500),
            stream_idle: Duration::from_secs(1),
            sweep_interval: Duration::from_millis(100),
            ..RelayLimits::default()
        };
        a.set_relay_limits_for_test(limits);
        let b_hex = os_p2p::NodeIdentity::generate().node_id().to_hex();
        let req = ApiRelayRequest {
            method: "GET".into(),
            url: "http://10.0.0.9:1/v1/models".into(),
            headers: vec![],
            body: None,
            stream: false,
        };
        // 整包超时（400ms）先于巡检 TTL（1s+1s）到——错误指明超时而非通道关闭。
        let err = a
            .relay_roundtrip(&b_hex, req, Duration::from_millis(400))
            .await
            .expect_err("黑洞通道必超时");
        assert!(err.contains("中继超时"), "错误应指明整包超时: {err}");
        // 超时后 pending 仍在（无 resp 触发即时清理）——由巡检收。
        assert_eq!(
            a.inner.pending.lock().unwrap().len(),
            1,
            "超时后 pending 待巡检"
        );
        // TTL（2s）过后，巡检按 last_seen 收走。
        tokio::time::sleep(Duration::from_millis(2400)).await;
        sweep_relay_state(&a.inner, &limits);
        assert!(
            a.inner.pending.lock().unwrap().is_empty(),
            "巡检应按 TTL 清走超时 pending"
        );
    }

    /// 41. 伪造应答防御：pending 定向 B，第三方节点 C 回的 resp 帧被丢弃
    ///     （事件不达），B 的合法应答照常回填。
    #[tokio::test]
    async fn relay_resp_from_wrong_node_ignored() {
        // 黑洞消费者 + 手工造 pending（relay_call 私有，in-module 直用）。
        let a = ApiMarketFedEndpoint::test_endpoint();
        let a_id = os_p2p::NodeIdentity::generate().node_id();
        a.set_full_transport(
            Arc::new(|_to, _payload| {}),
            Arc::new(|_| {}),
            a_id.to_hex(),
            "node-a".into(),
        );
        let b_id = os_p2p::NodeIdentity::generate().node_id();
        let c_id = os_p2p::NodeIdentity::generate().node_id();
        let req = ApiRelayRequest {
            method: "GET".into(),
            url: "http://10.0.0.9:1/v1/models".into(),
            headers: vec![],
            body: None,
            stream: false,
        };
        let mut rx = a.relay_call(&b_id.to_hex(), req).expect("发起应成功");
        let req_id = a
            .inner
            .pending
            .lock()
            .unwrap()
            .keys()
            .next()
            .cloned()
            .expect("pending 应有刚注册的 req_id");
        // C 的应答：吞掉（pending 保留）。
        a.ingest_relay_resp(
            &c_id,
            &serde_json::json!({
                "fed": FED_KIND_API_RELAY_RESP, "req_id": req_id, "seq": 0,
                "status": 200, "headers": {}, "done": true,
            }),
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(150), rx.recv())
                .await
                .is_err(),
            "第三方应答不得产生事件"
        );
        assert_eq!(a.inner.pending.lock().unwrap().len(), 1, "pending 保留");
        // B（定向目标本人）的应答：Head 正常回填。
        a.ingest_relay_resp(
            &b_id,
            &serde_json::json!({
                "fed": FED_KIND_API_RELAY_RESP, "req_id": req_id, "seq": 0,
                "status": 200, "headers": {}, "done": true,
            }),
        );
        match rx.recv().await {
            Some(Ok(ApiRelayEvent::Head { status, .. })) => assert_eq!(status, 200),
            other => panic!("定向目标的应答应回填 Head: {other:?}"),
        }
    }

    /// 42. ingest 记录验签来源 NodeID：bridge dispatch 路径（ingest_from）
    ///     写 source_node_id；同名不同 NodeID（物理节点不同）→ Skipped 保护。
    #[tokio::test]
    async fn fed_ingest_records_verified_source_node_id() {
        let h = ApiMarketRouteHandler::with_empty();
        let entry = serde_json::json!({
            "id": "nid-1", "api_name": "nid-api", "endpoint_url": "http://10.0.0.9:1/v1",
            "publisher_pubkey": "0xpk", "created_at": "t",
        });
        let from = os_p2p::NodeIdentity::generate().node_id();
        assert_eq!(
            h.federation().ingest_from(
                &from,
                &serde_json::json!({
                    "fed": FED_KIND_API_MARKET_LOBBY, "node": "ub2604", "entry": entry,
                })
            ),
            ApiMarketFedIngest::Written
        );
        let saved = &h.listings_snapshot()[0];
        assert_eq!(saved.source_node, "ub2604");
        assert_eq!(
            saved.source_node_id, from.to_hex(),
            "验签发送方 NodeID 落列（via_node 的来源）"
        );
        // 列表输出暴露 source_node_id（前端导入取它作 via_node）。
        let list = h.handle(get_req(&format!("{PATH_LIST}?scope=fed"))).await.unwrap();
        assert_eq!(list.body[0]["source_node_id"], from.to_hex());
        // 同名（node=ub2604）不同 NodeID 的"刷新"→ 异源保护 Skipped。
        let other = os_p2p::NodeIdentity::generate().node_id();
        assert_eq!(
            h.federation().ingest_from(
                &other,
                &serde_json::json!({
                    "fed": FED_KIND_API_MARKET_LOBBY, "node": "ub2604",
                    "entry": {
                        "id": "nid-1", "api_name": "nid-api", "endpoint_url": "http://10.0.0.9:2/v1",
                        "publisher_pubkey": "0xpk", "created_at": "t2",
                    },
                })
            ),
            ApiMarketFedIngest::Skipped,
            "节点名可撞——物理节点以验签 NodeID 为准"
        );
        // 同节点（同 NodeID）重发新快照 → Refreshed。
        assert_eq!(
            h.federation().ingest_from(
                &from,
                &serde_json::json!({
                    "fed": FED_KIND_API_MARKET_LOBBY, "node": "ub2604",
                    "entry": {
                        "id": "nid-1", "api_name": "nid-api", "endpoint_url": "http://10.0.0.9:3/v1",
                        "publisher_pubkey": "0xpk", "created_at": "t3",
                    },
                })
            ),
            ApiMarketFedIngest::Refreshed
        );
    }

    // =========================================================================
    // 双通道补覆盖（on-connect 补推 + 定期重播，2026-09-03 覆盖缺口修复）
    // =========================================================================

    /// 双端 fake overlay fixture（补推/重播测试共用）：A/B 各一个完整 handler
    /// （REST 发布 + 联邦端点共享同一 SQLite）。A 的两个发送面都直投 B 的
    /// `ingest_from`（验签方 = A），**广播面受 `b_online` 开关门控**——false
    /// 模拟"B 断连错过广播窗口"（真机缺陷：fed_broadcast 只发当时已连 peer）。
    /// B 侧 ingest 结果逐条记录（断言 Written/Duplicate/Refreshed 语义）。
    struct BackfillPair {
        a: ApiMarketRouteHandler,
        b: ApiMarketRouteHandler,
        a_fed: ApiMarketFedEndpoint,
        b_id: os_p2p::NodeId,
        a_id: os_p2p::NodeId,
        /// B 侧逐条 ingest 结果。
        results: Arc<std::sync::Mutex<Vec<ApiMarketFedIngest>>>,
        /// B 在线开关（广播面是否投得出）。
        b_online: Arc<std::sync::atomic::AtomicBool>,
    }

    fn backfill_pair() -> BackfillPair {
        let a = ApiMarketRouteHandler::with_empty();
        let b = ApiMarketRouteHandler::with_empty();
        let a_fed = a.federation();
        let b_fed = b.federation();
        let a_id = os_p2p::NodeIdentity::generate().node_id();
        let b_id = os_p2p::NodeIdentity::generate().node_id();
        let results: Arc<std::sync::Mutex<Vec<ApiMarketFedIngest>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let b_online = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // 定向面（补推/中继）：直投 B 的带验签 ingest（连接回调语义——
        // 对端刚连上，帧定向可达）。
        let b_direct = b_fed.clone();
        let a_from_d = a_id.clone();
        let res_d = results.clone();
        let send_to: FedSendFn = Arc::new(move |_to, payload| {
            let r = b_direct.ingest_from(&a_from_d, &payload);
            res_d.lock().unwrap().push(r);
        });
        // 广播面（federate/重播）：只在 b_online=true 时投得出。
        let b_bcast = b_fed.clone();
        let a_from_b = a_id.clone();
        let res_b = results.clone();
        let online = b_online.clone();
        let broadcast: FedBroadcastFn = Arc::new(move |payload| {
            if online.load(std::sync::atomic::Ordering::SeqCst) {
                let r = b_bcast.ingest_from(&a_from_b, &payload);
                res_b.lock().unwrap().push(r);
            }
        });
        a_fed.set_full_transport(send_to, broadcast, a_id.to_hex(), "node-a".into());
        BackfillPair {
            a,
            b,
            a_fed,
            b_id,
            a_id,
            results,
            b_online,
        }
    }

    /// 43. 上线补推：A 发布+federate 时 B 断连（广播面投不出去，零落地）→
    ///     B 重连（连接事件回调触发 backfill_to 定向补推）→ B 全部 Written
    ///     落库；同快照重复补推 → Duplicate（LRU 拦截零 DB 触碰）；本地指纹
    ///     目标跳过（自回路防护）。
    #[tokio::test]
    async fn fed_backfill_on_connect_covers_missed_broadcast() {
        let p = backfill_pair();
        // A 发布 2 条并 federate——B"断连"（b_online=false）：广播零投递。
        let (_, token) = login(&p.a, &new_key());
        let id1 = publish_ok(&p.a, &token, "backfill-one", Some(30)).await;
        let id2 = publish_ok(&p.a, &token, "backfill-two", None).await;
        for id in [&id1, &id2] {
            let resp = p
                .a
                .handle(authed(
                    HttpMethod::Post,
                    &federate_path(id),
                    &token,
                    serde_json::Value::Null,
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 200, "federate 应 200: {resp:?}");
        }
        assert!(
            p.b.listings_snapshot().is_empty(),
            "发布窗口 B 断连——联邦广播零落地（真机缺陷复现）"
        );
        assert!(p.results.lock().unwrap().is_empty(), "广播面被门控，零投递");
        // B 重连：模拟连接事件回调（main.rs 装配 spawn_conn_watcher →
        // backfill_to）——定向补推本节点全部 federated 条目。
        assert_eq!(p.a_fed.backfill_to(&p.b_id).await, 2, "补推 2 条");
        {
            let rs = p.results.lock().unwrap();
            assert_eq!(rs.len(), 2, "两条定向送达: {rs:?}");
            assert!(
                rs.iter().all(|r| *r == ApiMarketFedIngest::Written),
                "首次补推全部 Written: {rs:?}"
            );
        }
        let b_list = p.b.listings_snapshot();
        assert_eq!(b_list.len(), 2);
        for e in &b_list {
            assert_eq!(e.source_node, "node-a", "来源标记发布节点");
            assert_eq!(e.download_count, 0, "本地计数清零起步");
            assert!(e.federated, "推送标志随快照");
        }
        // 重复补推（同快照重放）→ Duplicate：seen 缓存拦截，零 DB 触碰
        //（行数不变——LRU 防线，对端零成本）。
        assert_eq!(p.a_fed.backfill_to(&p.b_id).await, 2);
        {
            let rs = p.results.lock().unwrap();
            assert_eq!(rs.len(), 4);
            assert!(
                rs[2..].iter().all(|r| *r == ApiMarketFedIngest::Duplicate),
                "同快照重放全部 Duplicate: {rs:?}"
            );
        }
        assert_eq!(p.b.listings_snapshot().len(), 2, "重放零 DB 触碰");
        // 自回路防护：目标指纹==本机 NodeID → 跳过（零帧）。
        assert_eq!(p.a_fed.backfill_to(&p.a_id).await, 0, "本地指纹目标跳过");
        assert_eq!(p.results.lock().unwrap().len(), 4, "自回路零帧");
    }

    /// 44. 定期重播：federate（B 在线）→ Written；replay_round 同快照 →
    ///     Duplicate（零 DB 触碰，行数/计数不变）；A 心跳后快照变 → 再重播 →
    ///     Refreshed（心跳联邦传播，快照刷新且保留本地计数）。
    #[tokio::test]
    async fn fed_replay_round_duplicate_then_refresh_after_heartbeat() {
        let p = backfill_pair();
        p.b_online
            .store(true, std::sync::atomic::Ordering::SeqCst); // B 在线收广播
        let (_, token) = login(&p.a, &new_key());
        let id = publish_ok(&p.a, &token, "replay-api", Some(9)).await;
        let resp = p
            .a
            .handle(authed(
                HttpMethod::Post,
                &federate_path(&id),
                &token,
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(p.results.lock().unwrap()[0], ApiMarketFedIngest::Written);
        assert_eq!(p.b.listings_snapshot().len(), 1);
        // 同快照重播 → Duplicate（LRU 拦截，不触碰 DB）。
        assert_eq!(p.a_fed.replay_round().await, 1, "重播 1 条");
        assert_eq!(p.results.lock().unwrap()[1], ApiMarketFedIngest::Duplicate);
        assert_eq!(p.b.listings_snapshot().len(), 1, "重播零 DB 触碰");
        // A 心跳（owner）→ 快照变（heartbeat_at/load 入快照）→ 重播穿透
        // LRU → 同源 Refreshed（"心跳不联邦传播"的观感缺口顺带补上）。
        let resp = p
            .a
            .handle(authed(
                HttpMethod::Post,
                &hb_path(&id),
                &token,
                serde_json::json!({ "load_pct": 66 }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "心跳应 200: {resp:?}");
        assert_eq!(p.a_fed.replay_round().await, 1);
        assert_eq!(p.results.lock().unwrap()[2], ApiMarketFedIngest::Refreshed);
        let saved = &p.b.listings_snapshot()[0];
        assert!(saved.heartbeat_at.is_some(), "心跳快照已联邦传播");
        assert_eq!(
            saved.load.as_ref().and_then(|l| l.load_pct),
            Some(66.0),
            "负载快照随重播刷新"
        );
        assert_eq!(saved.download_count, 0, "Refreshed 保留本地计数");
    }

    /// 45. 无 federated 条目零帧：只发布未 federate 的本地条目 + federated=1
    ///     的**远程**条目（source_node=他节点）都不参与补推/重播——远程条目
    ///     不转播是防环红线（fed_broadcast 一跳语义的补覆盖通道沿用）。
    #[tokio::test]
    async fn fed_backfill_and_replay_zero_frames_without_local_federated_entries() {
        let (h, t) = federated("node-solo");
        let (_, token) = login(&h, &new_key());
        let _id = publish_ok(&h, &token, "unfed-api", None).await; // 只发布未推送
        // 远程条目（federated 随载荷=1，但 source_node=远程）。
        assert_eq!(
            h.federation().ingest(&serde_json::json!({
                "fed": FED_KIND_API_MARKET_LOBBY, "node": "node-remote",
                "entry": {
                    "id": "r-1", "api_name": "remote-only", "endpoint_url": "http://10.0.0.9:1/v1",
                    "publisher_pubkey": "0xpk", "created_at": "t", "federated": true,
                },
            })),
            ApiMarketFedIngest::Written
        );
        let peer = os_p2p::NodeIdentity::generate().node_id();
        assert_eq!(
            h.federation().backfill_to(&peer).await,
            0,
            "无本节点 federated 条目——补推零帧"
        );
        assert_eq!(h.federation().replay_round().await, 0, "重播零帧");
        assert!(
            t.0.lock().unwrap().is_empty(),
            "捕获通道零载荷（未推送本地 + 远程条目均不转播）"
        );
    }

    /// 46. 常驻重播任务接线：注入缩短周期（50ms）→ federate 后**不直调**
    ///     replay_round，轮询等常驻任务自动重播（captured ≥ 2：首次推送 1 +
    ///     定期重播 ≥1——验证 install_transport 内的定时循环真实在跑）。
    #[tokio::test]
    async fn fed_periodic_replay_task_fires_with_injected_interval() {
        let (h, t) = federated("node-replay");
        h.federation()
            .set_replay_interval_for_test(Duration::from_millis(50));
        let (_, token) = login(&h, &new_key());
        let id = publish_ok(&h, &token, "replay-task-api", None).await;
        let resp = h
            .handle(authed(
                HttpMethod::Post,
                &federate_path(&id),
                &token,
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(t.0.lock().unwrap().len(), 1, "首次推送恰一条");
        // 2s 预算内常驻任务至少自动重播一轮（每轮载荷与首推同形）。
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if t.0.lock().unwrap().len() >= 2 || std::time::Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let payloads = t.0.lock().unwrap();
        assert!(
            payloads.len() >= 2,
            "常驻重播任务应自动重播（captured={}）: {payloads:?}",
            payloads.len()
        );
        assert_eq!(payloads[1]["fed"], FED_KIND_API_MARKET_LOBBY);
        assert_eq!(payloads[1]["entry"]["id"], id.as_str());
    }

    /// 47. 定向补播目标集过滤（纯函数，真机跟进 2026-09-03）：node-meta
    ///     Active ∖ connected ∖ 本机指纹——Active 且无连接的收（Spark 类）；
    ///     Inactive 不收；connected 不收（广播面已覆盖）；自指纹不收（防自回路）。
    #[test]
    fn fed_direct_replay_targets_filter_matrix() {
        let self_id = os_p2p::NodeIdentity::generate().node_id();
        let mk_id = || os_p2p::NodeIdentity::generate().node_id();
        let meta_entry = |id: &os_p2p::NodeId, state: os_p2p::MetaState| {
            os_p2p::NodeMetaEntry {
                id: id.clone(),
                addrs: vec![],
                first_seen: 1,
                last_seen: 2,
                state,
                source: os_p2p::MetaSource::Gossip,
                verified: false,
                exit_offered: false,
            }
        };
        let spark = mk_id(); // Active 且无连接——中继可达目标（应收）
        let dead = mk_id(); // Inactive——五振出局（不应收）
        let wired = mk_id(); // Active 且已连接——广播面覆盖（不应收）
        let meta = vec![
            meta_entry(&spark, os_p2p::MetaState::Active { score: 80, consec_fail: 0 }),
            meta_entry(&dead, os_p2p::MetaState::Inactive { since: 42 }),
            meta_entry(&wired, os_p2p::MetaState::Active { score: 90, consec_fail: 0 }),
            // 本机指纹也在注册表（gossip 回声）——自回路防护。
            meta_entry(&self_id, os_p2p::MetaState::Active { score: 100, consec_fail: 0 }),
        ];
        let connected: std::collections::HashSet<os_p2p::NodeId> =
            [wired.clone()].into_iter().collect();
        let targets = fed_direct_replay_targets(&meta, &connected, &self_id);
        assert_eq!(targets, vec![spark.clone()], "仅 Active ∖ connected ∖ 自指纹");
    }

    /// 48. 定向补播端到端（fake，真机跟进 2026-09-03）：B 在目标集（模拟
    ///     node-meta Active）但从未"连接"（广播面空投——Spark 类对端
    ///     connected 恒 false）→ replay_round 经 send_to 定向送达（断言路由
    ///     目标与载荷形态）→ B ingest Written 落库；同快照再播 → Duplicate；
    ///     目标集清空 → 零定向帧；注入自指纹目标被端点侧兜底过滤。
    #[tokio::test]
    async fn fed_replay_directed_to_known_active_without_conn() {
        let a = ApiMarketRouteHandler::with_empty();
        let b = ApiMarketRouteHandler::with_empty();
        let a_fed = a.federation();
        let b_fed = b.federation();
        let a_id = os_p2p::NodeIdentity::generate().node_id();
        let b_id = os_p2p::NodeIdentity::generate().node_id();
        // 定向面：记录 (目标, 载荷) 并投 B 的带验签 ingest。
        let sent: Arc<std::sync::Mutex<Vec<(os_p2p::NodeId, serde_json::Value)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let results: Arc<std::sync::Mutex<Vec<ApiMarketFedIngest>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let b_direct = b_fed.clone();
        let a_from = a_id.clone();
        let res_d = results.clone();
        let sent_d = sent.clone();
        let send_to: FedSendFn = Arc::new(move |to, payload| {
            sent_d.lock().unwrap().push((to.clone(), payload.clone()));
            let r = b_direct.ingest_from(&a_from, &payload);
            res_d.lock().unwrap().push(r);
        });
        // 广播面：空投（B 无常驻连接——fed_broadcast 遍历 connected 到不了它）。
        let broadcast: FedBroadcastFn = Arc::new(|_| {});
        a_fed.set_full_transport(
            send_to,
            broadcast,
            a_id.to_hex(),
            "node-a".into(),
        );
        // A 发布 + federate：广播空投，B 零落地（缺口复现）。
        let (_, token) = login(&a, &new_key());
        let id = publish_ok(&a, &token, "relay-target-api", Some(7)).await;
        let resp = a
            .handle(authed(
                HttpMethod::Post,
                &federate_path(&id),
                &token,
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert!(b.listings_snapshot().is_empty(), "无常驻连接——广播到不了 B");
        assert!(sent.lock().unwrap().is_empty());
        // 注入定向目标集（模拟"node-meta Active ∖ connected"的产物；混入
        // 自指纹验证端点侧兜底过滤——生产闭包过滤之外的第二道防线）。
        a_fed.set_known_active_for_test(vec![b_id.clone(), a_id.clone()]);
        assert_eq!(a_fed.replay_round().await, 1, "重播 1 条");
        {
            let s = sent.lock().unwrap();
            assert_eq!(s.len(), 1, "仅 B 一个定向目标（自指纹被兜底过滤）: {s:?}");
            assert_eq!(s[0].0, b_id, "路由目标 = 目标集里的 B");
            assert_eq!(s[0].1["fed"], FED_KIND_API_MARKET_LOBBY, "载荷形态同广播");
            assert_eq!(s[0].1["node"], "node-a");
            assert_eq!(s[0].1["node_id"], a_id.to_hex());
            assert_eq!(s[0].1["entry"]["id"], id.as_str());
            assert_eq!(s[0].1["entry"]["api_name"], "relay-target-api");
            assert_eq!(s[0].1["entry"]["federated"], true);
        }
        assert_eq!(results.lock().unwrap()[0], ApiMarketFedIngest::Written);
        let saved = &b.listings_snapshot()[0];
        assert_eq!(saved.source_node, "node-a", "来源标记发布节点");
        assert_eq!(saved.source_node_id, a_id.to_hex(), "验签来源 NodeID");
        assert_eq!(saved.download_count, 0, "本地计数清零起步");
        // 同快照再播 → 定向重投 B，对端 Duplicate（幂等，零 DB 触碰）。
        assert_eq!(a_fed.replay_round().await, 1);
        assert_eq!(sent.lock().unwrap().len(), 2, "定向重投 B（幂等由对端拦截）");
        assert_eq!(results.lock().unwrap()[1], ApiMarketFedIngest::Duplicate);
        assert_eq!(b.listings_snapshot().len(), 1);
        // 目标集清空 → 零定向帧（条目仍重播广播面，但 send_to 不再触发）。
        a_fed.set_known_active_for_test(vec![]);
        assert_eq!(a_fed.replay_round().await, 1);
        assert_eq!(
            sent.lock().unwrap().len(),
            2,
            "目标集空 → 定向零帧"
        );
        assert_eq!(results.lock().unwrap().len(), 2);
    }

    /// 49. 匿名节点收下（2026-09-03 真机根因修复）：node="peer"（发送端
    ///     NEXOS_P2P_NAME 未设的 sanitize 回退）→ 正常 Written（物理归因靠
    ///     验签 source_node_id）；node 字段整体缺失仍 Invalid；同 "peer" 名
    ///     不同 NodeID → 异源 Skipped（匿名多节点防碰撞）。
    #[tokio::test]
    async fn fed_ingest_accepts_anonymous_peer_node() {
        let (h, _t) = federated("node-113");
        let entry = |id: &str| {
            serde_json::json!({
                "id": id, "api_name": "anon-api", "endpoint_url": "http://10.0.0.9:1/v1",
                "publisher_pubkey": "0xabc", "created_at": "t",
                "pricing": { "mode": "free" },
            })
        };
        let payload = |node: Option<&str>, e: serde_json::Value| match node {
            Some(n) => serde_json::json!({ "fed": FED_KIND_API_MARKET_LOBBY, "node": n, "entry": e }),
            None => serde_json::json!({ "fed": FED_KIND_API_MARKET_LOBBY, "entry": e }),
        };
        // 匿名 "peer" 发送端 → Written（此前被当"缺 node"静默拒收——真机
        // "IM 能通、市场收不到"的根因：IM 接受 "peer"，api_market 拒收）。
        let from_a = os_p2p::NodeIdentity::generate().node_id();
        assert_eq!(
            h.federation()
                .ingest_from(&from_a, &payload(Some("peer"), entry("anon-1"))),
            ApiMarketFedIngest::Written,
            "匿名节点（node=peer）应收下"
        );
        let saved = &h.listings_snapshot()[0];
        assert_eq!(saved.source_node, "peer", "匿名标签照记");
        assert_eq!(
            saved.source_node_id,
            from_a.to_hex(),
            "物理归因=验签 NodeID（不依赖节点名）"
        );
        // node 字段整体缺失 → 仍 Invalid（真畸形载荷，非匿名）。
        assert_eq!(
            h.federation().ingest(&payload(None, entry("anon-2"))),
            ApiMarketFedIngest::Invalid,
            "缺 node 字段仍非法"
        );
        // 同 "peer" 名、不同 NodeID → 异源保护 Skipped（匿名碰撞防线：
        // id 兜底键 api_name+publisher_pubkey 命中同源条目，NodeID 不符）。
        let from_b = os_p2p::NodeIdentity::generate().node_id();
        let mut other_snapshot = entry("anon-1");
        other_snapshot["created_at"] = serde_json::json!("t-later"); // 快照不同才过 seen 缓存
        assert_eq!(
            h.federation()
                .ingest_from(&from_b, &payload(Some("peer"), other_snapshot)),
            ApiMarketFedIngest::Skipped,
            "同 peer 名不同物理节点 → 保护本地"
        );
        // 原匿名节点（同 NodeID）同快照重放 → Duplicate（seen 缓存）。
        assert_eq!(
            h.federation()
                .ingest_from(&from_a, &payload(Some("peer"), entry("anon-1"))),
            ApiMarketFedIngest::Duplicate,
            "同快照重放 → Duplicate"
        );
    }

}
