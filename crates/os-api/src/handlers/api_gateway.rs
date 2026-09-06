//! `ApiGatewayRouteHandler` —— LLM API 网关（One API 风格）桌面应用适配器。
//!
//! 定位：把网关 HTTP 请求（`/api/v1/gateway/*`）翻译为 API 聚合/分发/计费层管理。
//! 这是与"模型管理"（[`crate::handlers::llm::LlmRouteHandler`]，管 vLLM 实例）不同
//! 层次的功能——本组件是 **API 聚合/分发/计费层**：聚合多个上游 LLM provider
//! （本地 vLLM 实例 + OpenAI + 第三方），生成下游 API Key（令牌）做配额管理，
//! 统一 OpenAI 兼容入口做代理转发。参考 One API（`songquanpeng/one-api`）。
//!
//! # 降级语义（上游不在线也不 panic）
//!
//! 上游 provider 可能不在线 / curl 不存在 / 网络不通——所有真实转发操作（代理转发 /
//! 渠道连通性测试）失败都降级为 `failed` 日志或 `error` 状态返回，绝不 panic。
//!
//! # 计费模式（billing_mode）
//!
//! 令牌（`ApiToken`）创建时可选四种计费模式（默认 `per_token`，向后兼容）：
//!
//! | billing_mode | 语义 |
//! |--------------|------|
//! | `free`       | 免费模式：转发不检查配额、不扣费（quota 字段忽略）|
//! | `per_token`  | 按 token 计费（现状）：`ModelRatio × GroupRatio` 扣 `quota_used` |
//! | `per_image`  | 按生成图计费：每次生图调用按固定价 [`IMAGE_PRICE_CREDITS`] 扣
//!                `quota_used`（生图端点经 [`ApiGatewayRouteHandler::try_charge_image`]
//!                扣费——media-gen `POST /api/v1/media/image` 在生成前调用）；
//!                其文本转发仍按 per_token 计量叠加（图片单价另算）|
//! | `credits`    | 预付积分：创建时可带初始积分（写进 `quota_limit`），一切消费扣积分，
//!                `quota_used >= quota_limit` 时按现有配额语义拒绝（429）|
//!
//! 加密货币充值（`PaymentOrder`）：`POST /payments` 创建订单（USDT/BTC/EVM，金额按
//! [`crypto_amount_for`] 价目换算），admin 经 `/payments/:id/confirm` 确认后给目标
//! 令牌加积分（`quota_limit += credits`，幂等）。**链上核验（dApp 一期，2026-08-31）
//! 已接线**：evm 订单 confirm 带 txid 时复用 NexHub 同一核验编排
//! （[`os_nexhub::nexhub_lobby::check_chain_payment`]——`eth_getTransactionByHash`
//! 核对收款地址/金额/执行成功；语义表与 env 清单见该段注释及
//! docs/GATEWAY_MONETIZATION.md「支付验真」节）；usdt/btc 订单仍为 admin 链下
//! 手动确认（响应带 `chain_verify.status="unverified"` 标注）。
//! **dApp 二期（2026-09-02）**：①usdt 订单定位到 EVM 链且有 ERC-20 合约
//! （body `erc20_contract` → env `NEXOS_USDT_EVM_CONTRACT`）时走 **Transfer 日志
//! 核验**（"10.00" 按 decimals=6 换算微单位；TRON 形态/缺合约仍人工）；②confirm
//! 金额规则改 **AtLeast**（≥应付额即过——充值多打不亏待用户，实付落库）。
//!
//! # 渠道中继（via_node，2026-09-03）
//!
//! 渠道 `via_node` 非空 = **中继渠道**：转发不直连上游，经 os-p2p overlay 定向
//! 该源节点（NodeID）代发——复用 api_market 的 `api_relay_req`/`api_relay_resp`
//! 分块协议执行层（`ApiMarketFedEndpoint::relay_roundtrip` / `relay_open_stream`）。
//! 一键导入 `from_external_api: <登记 id>` 从 llm_external_apis 行复制
//! name/base_url/api_key/models/via_node（models 空先探回填）。非流式在
//! `forward_channel` 分流、流式在 http.rs 特挂路径分流，失败照常按优先级故障
//! 转移；本地鉴权/计费照常（节点自治）。场景与信任链见
//! docs/GATEWAY_MONETIZATION.md §6、docs/LLM_EXTERNAL_APIS.md §5。
//!
//! # 路由表（32 条，component="api_gateway"）
//!
//! | method | path                                       | 动作 |
//! |--------|--------------------------------------------|------|
//! | GET    | `/api/v1/gateway/channels`                 | 列渠道 |
//! | POST   | `/api/v1/gateway/channels`                 | 添加渠道（admin，支持 `from_discovery` 从本地 vLLM 发现导入 / `from_external_api` 从外部 API 登记一键导入——via_node 复制成中继渠道，2026-09-03）|
//! | PUT    | `/api/v1/gateway/channels/:id`             | 更新渠道（admin）|
//! | DELETE | `/api/v1/gateway/channels/:id`             | 删渠道（admin）|
//! | POST   | `/api/v1/gateway/channels/:id/test`        | 测试渠道连通性（admin）|
//! | GET    | `/api/v1/gateway/channels/:id`             | 单渠道详情 |
//! | GET    | `/api/v1/gateway/tokens`                   | 列令牌 |
//! | POST   | `/api/v1/gateway/tokens`                   | 创建令牌（admin，自动生成 sk-os-xxx）|
//! | DELETE | `/api/v1/gateway/tokens/:id`               | 删令牌（admin）|
//! | POST   | `/api/v1/gateway/tokens/:id/disable`       | 禁用令牌（admin）|
//! | POST   | `/api/v1/gateway/tokens/:id/enable`        | 启用令牌（admin）|
//! | GET    | `/api/v1/gateway/logs`                     | 调用日志（?limit= 分页）|
//! | GET    | `/api/v1/gateway/stats`                    | 聚合统计 |
//! | GET    | `/api/v1/gateway/models`                   | 聚合可用模型（去重）|
//! | GET    | `/api/v1/gateway/mappings`                 | 列模型映射 |
//! | POST   | `/api/v1/gateway/mappings`                 | 添加映射（admin）|
//! | DELETE | `/api/v1/gateway/mappings/:name`           | 删映射（admin）|
//! | GET    | `/api/v1/gateway/model-ratios`             | 列模型倍率（ModelRatio）|
//! | POST   | `/api/v1/gateway/model-ratios`             | 设置模型倍率（admin）|
//! | DELETE | `/api/v1/gateway/model-ratios/:model`      | 删模型倍率（admin）|
//! | GET    | `/api/v1/gateway/group-ratios`             | 列用户组倍率（GroupRatio）|
//! | POST   | `/api/v1/gateway/group-ratios`             | 设置用户组倍率（admin）|
//! | GET    | `/api/v1/gateway/redeem-codes`             | 列兑换码（admin）|
//! | POST   | `/api/v1/gateway/redeem-codes`             | 生成兑换码（admin）|
//! | POST   | `/api/v1/gateway/redeem`                   | 兑换码兑换（任意 token 加配额）|
//! | POST   | `/api/v1/gateway/payments`                 | 创建充值订单（admin，USDT/BTC/EVM）|
//! | GET    | `/api/v1/gateway/payments`                 | 列充值订单（?status= 过滤）|
//! | POST   | `/api/v1/gateway/payments/:id/confirm`     | 确认到账（admin，给 token 加积分，幂等）|
//! | POST   | `/api/v1/gateway/payments/:id/reject`      | 拒绝订单（admin，记原因）|
//! | POST   | `/api/v1/gateway/v1/chat/completions`      | 代理转发（OpenAI 兼容；`stream:true` 经 http.rs SSE 特挂路由逐块透传）|
//! | POST   | `/api/v1/gateway/v1/completions`           | 代理转发（completions）|
//! | GET    | `/api/v1/gateway/v1/models`                | OpenAI 形态模型列表（同一 Bearer sk-os- 鉴权）|

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use once_cell::sync::Lazy;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};
// 链上支付验真（dApp 一期）：复用 os-nexhub 的业务接线层（可注入网关 + 语义
// 映射 + RPC 候选链）——NexHub 购买/悬赏与网关 PaymentOrder confirm 同一套语义。
use os_nexhub::nexhub_lobby::{
    chain_verify_json, check_chain_payment, ChainPayCheck, ChainPayGate, ChainPayHints,
};

// ----------------------------------------------------------------------------
// 共享 HTTP 客户端（rustify：curl 子进程 → reqwest）
// ----------------------------------------------------------------------------

/// 进程级共享 `reqwest::Client`（连接池复用，避免每请求新建）。
/// 默认 30s 超时仅为兜底；各调用处用 `RequestBuilder::timeout` 按语义覆盖
/// （探测 5s / 非流式转发 [`GATEWAY_UPSTREAM_TIMEOUT_DEFAULT_SECS`] 等）。
/// 连接建立 10s 独立上限：连接阶段被 connect_timeout 先掐（非流式转发总
/// 超时提至 300s 后，死/静默上游仍在 10s 内快速失败给故障转移）。
static HTTP: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("构建共享 reqwest Client 失败")
});

/// 流式转发专用客户端（SSE 逐块透传，2026-08-31）。
///
/// 与 [`HTTP`] 的关键差异：**无总超时**——reqwest 的 client timeout 覆盖到
/// "响应体读完"为止，SSE 长连接可能持续数分钟，30s 总超时会把流掐断。
/// 仅保留 10s **连接建立**超时（连不上上游照样快速失败给故障转移）。
/// 读超时不设：流式生成长间隔静默（思考模型）属正常，不视为故障。
static HTTP_STREAM: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("构建流式 reqwest Client 失败")
});

// ----------------------------------------------------------------------------
// 非流式转发超时（2026-09-03 60→300s + env 可配）
// ----------------------------------------------------------------------------

/// 非流式转发（直连 `forward_upstream` / 中继 `relay_roundtrip` 整包）的缺省
/// 总超时：**300s**。旧值 60s 会掐掉长生成——devdocs AI 翻译（6K 字块 + 思考
/// 段生成）实测超 60s 被掐出 502（vLLM 侧对应 auto-aborting request due to
/// drop）。连接失败不受影响：连接阶段由共享 client 的 10s connect_timeout
/// 独立先掐（连接拒绝/不可达秒级失败），死上游照样快速故障转移。
pub const GATEWAY_UPSTREAM_TIMEOUT_DEFAULT_SECS: u64 = 300;

/// env 可配的上下限（1s..=3600s——解析值越界回落缺省，不猜极端值）。
pub const GATEWAY_UPSTREAM_TIMEOUT_MIN_SECS: u64 = 1;
pub const GATEWAY_UPSTREAM_TIMEOUT_MAX_SECS: u64 = 3600;

/// 解析 env `NEXOS_GATEWAY_UPSTREAM_TIMEOUT_SECS`（纯函数，可单测）：
/// `Some(合法秒数)` → 该值（clamp 到上下限内）；`None` / 非数字 / 越界 →
/// [`GATEWAY_UPSTREAM_TIMEOUT_DEFAULT_SECS`]。
#[must_use]
pub fn parse_upstream_timeout_secs(raw: Option<&str>) -> u64 {
    let Some(v) = raw.and_then(|s| s.trim().parse::<u64>().ok()) else {
        return GATEWAY_UPSTREAM_TIMEOUT_DEFAULT_SECS;
    };
    if !(GATEWAY_UPSTREAM_TIMEOUT_MIN_SECS..=GATEWAY_UPSTREAM_TIMEOUT_MAX_SECS).contains(&v) {
        return GATEWAY_UPSTREAM_TIMEOUT_DEFAULT_SECS;
    }
    v
}

/// 非流式转发当前总超时（Duration 形态；调用处求值——env 热改对后续请求生效）。
fn upstream_timeout() -> Duration {
    Duration::from_secs(parse_upstream_timeout_secs(
        std::env::var("NEXOS_GATEWAY_UPSTREAM_TIMEOUT_SECS")
            .ok()
            .as_deref(),
    ))
}

// ----------------------------------------------------------------------------
// 计费模式与加密货币充值：常量价目表
// ----------------------------------------------------------------------------

/// per_image 计费模式：每次生图调用扣的固定积分价（起步值，调价改这里）。
pub const IMAGE_PRICE_CREDITS: u64 = 100;

/// `GET /api/v1/gateway/v1/models` 响应里 `created` 字段的固定占位时间戳
/// （2026-01-01T00:00:00Z）。OpenAI list 契约要求该字段，但它对网关无业务
/// 语义（模型没有"创建时间"可言——渠道 models 列表是配置不是实体表），
/// 固定常量即可，**不是**编造的业务数据。
pub const MODELS_LIST_CREATED_TS: i64 = 1_767_225_600;

/// [`ApiGatewayRouteHandler::try_charge_image`] **余额不足**错误的文案哨兵：
/// `Err` 文案含此片段 → 调用方（media-gen 生图端点）回 **402**（Payment Required，
/// 余额不足闸门，docs/GATEWAY_MONETIZATION.md §1「402 闸门」）；其余 `Err` 回 401。
pub const IMAGE_CHARGE_INSUFFICIENT_MARKER: &str = "余额不足";

/// [`ApiGatewayRouteHandler::try_charge_image`] 的成功结果。
///
/// - `charged=false`：free 计费模式，未扣费（quota 字段忽略）；
/// - `charged=true`：已扣 [`IMAGE_PRICE_CREDITS`] 积分（per_image / per_token /
///   credits 模式——「一切消费扣积分」）；
/// - `token_name`：命中的令牌名，调用方写进生图响应/recent 的 `generated_by` 归因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChargeOutcome {
    /// 是否实际扣了积分（free 模式恒 false）。
    pub charged: bool,
    /// 命中的令牌名（归因展示用，如 "前端应用"）。
    pub token_name: String,
}

/// USDT 价：1 积分 = 0.01 USDT（初版拍脑袋值，调价改这里）。
pub const PRICE_USDT_PER_CREDIT: f64 = 0.01;
/// BTC 价：1 积分 = 1500 聪（satoshi，初版拍脑袋值，调价改这里）。
pub const PRICE_SATS_PER_CREDIT: u64 = 1_500;
/// EVM 价：1 积分 = 0.02 ETH，以 wei 计（20e15，初版拍脑袋值，调价改这里）。
pub const PRICE_WEI_PER_CREDIT: u64 = 20_000_000_000_000_000;

/// 充值币种白名单：`usdt` / `btc` / `evm`。
fn is_valid_currency(c: &str) -> bool {
    matches!(c, "usdt" | "btc" | "evm")
}

/// 计费模式白名单：`free` / `per_token` / `per_image` / `credits`。
fn is_valid_billing_mode(m: &str) -> bool {
    matches!(m, "free" | "per_token" | "per_image" | "credits")
}

/// `ApiToken.billing_mode` 的 serde 默认值（旧 JSON 无此字段 → per_token）。
fn default_billing_mode() -> String {
    "per_token".to_string()
}

/// 价目换算纯函数：`credits` 积分 → 各币种应付金额字符串。
///
/// - `usdt`：`credits × 0.01`，两位小数（如 1000 积分 → `"10.00"` USDT）
/// - `btc`：以**聪（sat）**面额的整数字符串（如 1000 积分 → `"1500000"` sat）
/// - `evm`：以 **wei** 面额的整数字符串（如 1000 积分 → `"20000000000000000000"` wei）
/// - 其他币种 → `None`（caller 报 400）
///
/// 价目常量是初版拍脑袋值，调价改 [`PRICE_USDT_PER_CREDIT`] 等常量即可。
#[must_use]
pub fn crypto_amount_for(currency: &str, credits: u64) -> Option<String> {
    match currency {
        "usdt" => Some(format!("{:.2}", credits as f64 * PRICE_USDT_PER_CREDIT)),
        // sat/wei 最小面额用 u128 乘法：wei 级单价 × 积分会溢出 u64
        //（1000 积分 × 20e15 wei = 2e19 > u64::MAX ≈ 1.8e19）
        "btc" => Some((credits as u128 * PRICE_SATS_PER_CREDIT as u128).to_string()),
        "evm" => Some((credits as u128 * PRICE_WEI_PER_CREDIT as u128).to_string()),
        _ => None,
    }
}

/// 读充值收款地址 env：`NEXOS_PAY_USDT_ADDR` / `NEXOS_PAY_BTC_ADDR` / `NEXOS_PAY_EVM_ADDR`。
///
/// 未配置 → `(空串, Some(warning))`：订单仍创建（status=pending），但返回 warning
/// 提示管理员先配置地址再让用户打款；已配置 → `(地址, None)`。
fn pay_address_for(currency: &str) -> (String, Option<String>) {
    let key = match currency {
        "usdt" => "NEXOS_PAY_USDT_ADDR",
        "btc" => "NEXOS_PAY_BTC_ADDR",
        "evm" => "NEXOS_PAY_EVM_ADDR",
        _ => return (String::new(), Some(format!("未知币种: {currency}"))),
    };
    match std::env::var(key) {
        Ok(addr) if !addr.trim().is_empty() => (addr.trim().to_string(), None),
        _ => (
            String::new(),
            Some(format!(
                "未配置收款地址 env {key}，请管理员配置后再让用户打款"
            )),
        ),
    }
}

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 上游渠道（Channel）—— 聚合多个 LLM provider。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channel {
    pub id: String,
    /// 渠道名，如 "本地vLLM-7B" / "OpenAI官方" / "DeepSeek"。
    pub name: String,
    /// `openai` / `deepseek` / `anthropic` / `local-vllm` / `azure` / `ollama`。
    pub provider: String,
    /// 上游 API 地址，如 `http://localhost:8000/v1` 或 `https://api.openai.com/v1`。
    pub base_url: String,
    /// 上游密钥（上游的 key，不是下游令牌）。
    pub api_key: String,
    /// 该渠道支持的模型列表，如 `["gpt-4o","gpt-4o-mini"]`。
    pub models: Vec<String>,
    /// 优先级（数字越小越优先，故障转移用）。
    pub priority: u32,
    /// 权重（同优先级内负载均衡，默认 1）。
    pub weight: u32,
    /// `enabled` / `disabled` / `error`。
    pub status: String,
    pub enabled: bool,
    pub created_at: String,
    pub last_used: Option<String>,
    /// 累计请求数。
    pub request_count: u64,
    /// 联邦中继来源 NodeID（`0x`+66hex，2026-09-03 渠道中继）：非空 = **中继
    /// 渠道**——转发不直连上游，经 os-p2p overlay 定向该源节点代发（复用
    /// api_market 的 api_relay_req/resp 执行层）；空 = 直连语义不变。
    /// 一键导入（`from_external_api`）时从外部 API 行的 `via_node` 复制而来。
    #[serde(default)]
    pub via_node: String,
    /// film 成本估算三单价（2026-09-06 FilmHub 记账，docs/FILM_STUDIO.md）：
    /// `price_per_call` 元/次、`price_per_sec` 元/秒、`price_per_token` 元/千
    /// token——est = per_call + per_sec×wall + per_token×(tokens/1000)。
    /// serde default 0（旧渠道 JSON 零迁移；不配置则 film 只计量不计价）。
    #[serde(default)]
    pub price_per_call: f64,
    #[serde(default)]
    pub price_per_sec: f64,
    #[serde(default)]
    pub price_per_token: f64,
}

/// 下游令牌（API Key）—— 给消费者用的 `sk-xxx`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiToken {
    pub id: String,
    /// 令牌名/用途，如 "前端应用" / "测试key"。
    pub name: String,
    /// 完整 key，如 `sk-os-a1b2c3d4e5f6...`。
    pub key: String,
    /// `active` / `disabled` / `expired`。
    pub status: String,
    pub enabled: bool,
    /// 配额上限（token 数 或 额度分，0=无限）。
    pub quota_limit: u64,
    /// 已用配额。
    pub quota_used: u64,
    /// 允许调用的模型（空=全部）。
    pub allowed_models: Vec<String>,
    /// 允许路由的渠道（空=全部）。
    pub allowed_channels: Vec<String>,
    /// 用户组（default/vip/trial），决定组倍率（GroupRatio）。默认 "default"。
    pub group_name: String,
    /// 计费模式：`free` / `per_token` / `per_image` / `credits`，默认 `per_token`。
    ///
    /// serde default 兼容旧持久化 JSON（无此字段 → per_token，行为与历史版本一致）；
    /// SQLite 老库经 `migrate_add_billing_mode` 补列，默认值同为 'per_token'。
    #[serde(default = "default_billing_mode")]
    pub billing_mode: String,
    /// 过期时间（None=永不过期）。
    pub expires_at: Option<String>,
    pub created_at: String,
    pub last_used: Option<String>,
    pub request_count: u64,
}

/// 调用日志。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallLog {
    pub id: String,
    /// 用的哪个令牌。
    pub token_id: String,
    pub token_name: String,
    /// 路由到哪个渠道。
    pub channel_id: String,
    pub channel_name: String,
    /// 请求的模型。
    pub model: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    pub latency_ms: u64,
    /// `success` / `failed` / `timeout`。
    pub status: String,
    pub error: Option<String>,
    pub created_at: String,
}

/// 模型路由映射（把对外模型名映射到渠道 + 真实模型名）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMapping {
    /// 对外模型名，如 `gpt-4`。
    pub public_name: String,
    /// 路由到哪个渠道。
    pub channel_id: String,
    /// 上游真实模型名。
    pub upstream_model: String,
}

/// 模型计费倍率（ModelRatio）—— 不同模型不同倍率（gpt-4=15, gpt-3.5=0.75）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRatio {
    /// 模型名，如 `gpt-4o`。
    pub model: String,
    /// 倍率（1.0=标准）。
    pub ratio: f64,
    pub updated_at: String,
}

/// 用户组倍率（GroupRatio）—— 令牌分组的计费倍率（vip=0.8 打折, trial=2 加倍）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupRatio {
    /// 组名，如 `default` / `vip` / `trial`。
    pub group_name: String,
    /// 组倍率（1.0=标准）。
    pub ratio: f64,
    pub updated_at: String,
}

/// 兑换码（RedeemCode）—— 兑换后增加令牌配额。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedeemCode {
    /// 兑换码字符串，如 `REDEEM-XXXX-XXXX`。
    pub code: String,
    /// 兑换增加的配额（扣减 quota_used）。
    pub quota_amount: u64,
    /// 使用者 token id（None=未使用）。
    pub used_by: Option<String>,
    pub used_at: Option<String>,
    pub created_at: String,
    pub expires_at: Option<String>,
}

/// 加密货币充值订单（PaymentOrder）—— 链下记账，admin 手动确认到账后给令牌加积分。
///
/// 生命周期：`pending` →（admin 确认）`confirmed`（目标 token `quota_limit += credits`，
/// 幂等：重复 confirm 拒绝）/（admin 拒绝）`rejected`（记 [`Self::reject_reason`]）。
/// 链上自动核验是二期，本期 admin 对照 txid 手动确认。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentOrder {
    /// 订单 id，如 `pay-101`。
    pub id: String,
    /// 充值目标令牌 id。
    pub token_id: String,
    /// 币种：`usdt` / `btc` / `evm`。
    pub currency: String,
    /// 应付金额（字符串保精度）：USDT 为两位小数美元值；BTC 为**聪**、EVM 为 **wei**
    /// 面额的整数字符串（最小面额，避免浮点误差）。
    pub amount_crypto: String,
    /// 到账积分（确认后 `quota_limit += credits`）。
    pub credits: u64,
    /// 收款地址（env 未配置时为空串，响应附带 warning 提示管理员）。
    pub address: String,
    /// 附言（BTC/EVM 订单填面额单位提示，如 "amount 单位为 sat"，便于前端展示）。
    pub memo: Option<String>,
    /// `pending` / `confirmed` / `rejected`。
    pub status: String,
    /// 链上交易 id（admin 确认时可附带）。
    pub txid: Option<String>,
    pub created_at: String,
    pub confirmed_at: Option<String>,
    /// 拒绝原因（status=rejected 时记录）。
    pub reject_reason: Option<String>,
    /// 链上核验事实（dApp 一期，2026-08-31）：evm 订单 confirm 核验通过时的**块高**；
    /// None = 未核验（admin 手动确认 / RPC 降级 / 开关关闭）。
    #[serde(default)]
    pub chain_block: Option<u64>,
    /// 链上核验事实：链上**实付金额**（wei 十进制字符串，与 tx 的 value 一致）。
    #[serde(default)]
    pub chain_value_wei: Option<String>,
}

/// `GET /api/v1/gateway/stats` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayStats {
    pub channels_total: usize,
    pub channels_enabled: usize,
    pub tokens_total: usize,
    pub tokens_active: usize,
    pub total_requests: u64,
    pub total_tokens: u64,
    /// 成功率（0..=100）。
    pub success_rate: f64,
}

/// 添加渠道请求体。
///
/// 两条路（2026-08-30 添加渠道真实化）：
/// 1. **完整手填**：`name`/`provider`/`base_url` 直传（旧行为，全部可选化仅为
///    兼容 from_discovery 缺省——纯手填路径缺 name/base_url 仍 400）；
/// 2. **从本地发现导入**：只传 `from_discovery: {port, name?, models?}`——
///    后端复用 `GET /api/v1/llm/gateway/models` 的探测逻辑
///    （[`crate::handlers::llm::probe_vllm_models`]）实测该端口 /v1/models，
///    用返回的 `data[].id` 填 `models`，`base_url` 固定
///    `http://127.0.0.1:<port>/v1`、`provider="local-vllm"`。探测失败 502
///    不建渠道（绝不把猜的端口当可用）。
#[derive(Debug, Deserialize)]
struct CreateChannelBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    models: Option<Vec<String>>,
    #[serde(default)]
    priority: Option<u32>,
    #[serde(default)]
    weight: Option<u32>,
    #[serde(default)]
    via_node: Option<String>,
    /// film 成本估算三单价（元/次、元/秒、元/千 token；缺省 0=未配置）。
    #[serde(default)]
    price_per_call: Option<f64>,
    #[serde(default)]
    price_per_sec: Option<f64>,
    #[serde(default)]
    price_per_token: Option<f64>,
    #[serde(default)]
    from_discovery: Option<FromDiscoveryBody>,
    /// 从外部 API 登记一键导入（2026-09-03）：`llm_external_apis` 行 id——后端
    /// 复制 name/base_url/api_key/models/via_node 生成渠道（models 空则先探
    /// `<base_url>/models` 回填）；行不存在 404。
    #[serde(default)]
    from_external_api: Option<String>,
}

/// `from_discovery` 子体：按端口从本地 vLLM 发现结果建渠道。
#[derive(Debug, Deserialize)]
struct FromDiscoveryBody {
    /// 本地 vLLM 监听端口（后端实测该端口 /v1/models）。
    port: u16,
    /// 渠道名（缺省「发现的 vLLM :<port>」，与 gateway/models 发现条目同名）。
    #[serde(default)]
    name: Option<String>,
    /// 模型列表覆盖（缺省用探测到的 data[].id——一般不用传）。
    #[serde(default)]
    models: Option<Vec<String>>,
}

/// 更新渠道请求体（全部可选字段）。
#[derive(Debug, Deserialize)]
struct UpdateChannelBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    models: Option<Vec<String>>,
    #[serde(default)]
    priority: Option<u32>,
    #[serde(default)]
    weight: Option<u32>,
    #[serde(default)]
    enabled: Option<bool>,
    /// 中继来源 NodeID（提供即覆盖；空串 = 清除回直连语义；须 0x+66hex）。
    #[serde(default)]
    via_node: Option<String>,
    /// film 成本估算三单价（提供即覆盖）。
    #[serde(default)]
    price_per_call: Option<f64>,
    #[serde(default)]
    price_per_sec: Option<f64>,
    #[serde(default)]
    price_per_token: Option<f64>,
}

/// 创建令牌请求体。
#[derive(Debug, Deserialize)]
struct CreateTokenBody {
    name: String,
    #[serde(default)]
    quota_limit: Option<u64>,
    #[serde(default)]
    allowed_models: Option<Vec<String>>,
    #[serde(default)]
    allowed_channels: Option<Vec<String>>,
    #[serde(default)]
    group_name: Option<String>,
    #[serde(default)]
    expires_at: Option<String>,
    /// 计费模式（缺省 per_token；非法值 400）。
    #[serde(default)]
    billing_mode: Option<String>,
    /// credits 模式的初始积分（写入 quota_limit；其他模式忽略）。
    #[serde(default)]
    initial_credits: Option<u64>,
}

/// 添加映射请求体。
#[derive(Debug, Deserialize)]
struct CreateMappingBody {
    public_name: String,
    channel_id: String,
    upstream_model: String,
}

/// 设置模型倍率请求体（admin）。
#[derive(Debug, Deserialize)]
struct SetModelRatioBody {
    model: String,
    ratio: f64,
}

/// 设置用户组倍率请求体（admin）。
#[derive(Debug, Deserialize)]
struct SetGroupRatioBody {
    group_name: String,
    ratio: f64,
}

/// 生成兑换码请求体（admin）。
#[derive(Debug, Deserialize)]
struct CreateRedeemCodeBody {
    quota_amount: u64,
    #[serde(default)]
    count: Option<u32>,
}

/// 兑换码兑换请求体（任意 token）。
#[derive(Debug, Deserialize)]
struct RedeemBody {
    code: String,
}

/// 创建充值订单请求体（admin）。
#[derive(Debug, Deserialize)]
struct CreatePaymentBody {
    /// 充值目标令牌 id。
    token_id: String,
    /// 币种：`usdt` / `btc` / `evm`（非法值 400）。
    currency: String,
    /// 到账积分（>0，金额按 [`crypto_amount_for`] 价目换算）。
    credits: u64,
}

/// 确认订单到账请求体（admin，txid 可选；body 可为 null/缺省）。
///
/// dApp 一期链上核验定位字段（evm 订单 + txid 非空时生效）：
/// `chain_id`（缺省回 env `NEXOS_EVM_CHAIN_ID`）与 `rpc_url`（admin 自配，
/// RPC 候选链第一段）。
///
/// dApp 二期（2026-09-02）增量：`erc20_contract` / `erc20_decimals`——usdt
/// 订单走 ERC-20 Transfer 日志核验时定位代币合约（缺省回 env
/// `NEXOS_USDT_EVM_CONTRACT` / `NEXOS_USDT_EVM_DECIMALS`，默认 6；都无则
/// unverified 放行，不猜合约地址）。
#[derive(Debug, Default, Deserialize)]
struct ConfirmPaymentBody {
    #[serde(default)]
    txid: Option<String>,
    #[serde(default)]
    chain_id: Option<u64>,
    #[serde(default)]
    rpc_url: Option<String>,
    #[serde(default)]
    erc20_contract: Option<String>,
    #[serde(default)]
    erc20_decimals: Option<u8>,
}

/// 拒绝订单请求体（admin，reason 可选；body 可为 null/缺省）。
#[derive(Debug, Default, Deserialize)]
struct RejectPaymentBody {
    #[serde(default)]
    reason: Option<String>,
}

// ----------------------------------------------------------------------------
// 纯函数（可单测）
// ----------------------------------------------------------------------------

/// 脱敏密钥：只显示前 4 + 后 4 字符，中间用 `***`。
///
/// 漏洞3 修复：GET /channels / GET /tokens 响应里不回显完整密钥。
/// 规则：
/// - 长度 >= 8：显示 `<前4>***<后4>`，如 `sk-xe***1234` / `sk-os-abc***wxyz`
/// - 长度 < 8 且非空：全显示为 `***`
/// - 空串：返回 `***`（避免泄露"是否配置过 key"的信息）
#[must_use]
pub fn mask_secret(s: &str) -> String {
    if s.chars().count() < 8 {
        return "***".to_string();
    }
    // 按字符（Unicode 安全）取前 4 + 后 4
    let chars: Vec<char> = s.chars().collect();
    let head: String = chars.iter().take(4).collect();
    let tail: String = chars
        .iter()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}***{tail}")
}

/// 生成 `sk-os-<32位随机hex>` 格式的 API Key。
///
/// 用时间戳 + 计数器 + 线程局部伪随机源拼出 32 位 hex（不依赖系统随机设备，
/// 测试环境也能稳定产出）。caller 一般只在 admin 创建令牌时调用。
#[must_use]
pub fn generate_api_key() -> String {
    use std::cell::Cell;
    use std::time::{SystemTime, UNIX_EPOCH};
    thread_local! {
        static SEQ: Cell<u64> = const { Cell::new(0) };
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0xdead_beef);
    let seq = SEQ.with(|s| {
        let v = s.get().wrapping_add(0x9e37_79b9_7f4a_7c15);
        s.set(v);
        v
    });
    // xorshift 混合时间戳 + 序列，产出 32 hex（128 bit）
    let mut x = now ^ seq.wrapping_mul(0x1000_0000);
    let mut out = String::with_capacity(40);
    out.push_str("sk-os-");
    for _ in 0..32 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let nib = (x & 0xf) as u8;
        let c = if nib < 10 {
            b'0' + nib
        } else {
            b'a' + (nib - 10)
        };
        out.push(c as char);
    }
    out
}

/// 计费纯函数：`ceil(total_tokens × model_ratio × group_ratio)`。
///
/// 对标 New API / One API 的 ModelRatio × GroupRatio 计费模型：
/// 实际扣费 = `total_tokens × model_ratio × group_ratio`，向上取整为 u64。
/// 任意输入 ≤ 0 的倍率视为 0 扣费（不扣），且结果不会下溢为负。
///
/// # 示例
/// ```
/// # use os_api::handlers::api_gateway::calc_cost;
/// assert_eq!(calc_cost(1000, 1.5, 0.8), 1200); // 1000 × 1.5 × 0.8 = 1200
/// assert_eq!(calc_cost(101, 0.75, 0.8), 61);   // 60.6 → ceil 61
/// ```
#[must_use]
pub fn calc_cost(total_tokens: u64, model_ratio: f64, group_ratio: f64) -> u64 {
    if total_tokens == 0 {
        return 0;
    }
    let raw = (total_tokens as f64) * model_ratio * group_ratio;
    if raw <= 0.0 {
        return 0;
    }
    raw.ceil() as u64
}

/// 生成 `REDEEM-XXXX-XXXX` 格式的兑换码（8 位大写 hex，两组各 4 字符）。
///
/// 用时间戳 + 线程局部序数 + xorshift 产出，测试环境也能稳定产出。
#[must_use]
pub fn generate_redeem_code() -> String {
    use std::cell::Cell;
    use std::time::{SystemTime, UNIX_EPOCH};
    thread_local! {
        static SEQ: Cell<u64> = const { Cell::new(0) };
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x1234_5678);
    let seq = SEQ.with(|s| {
        let v = s.get().wrapping_add(0x9e37_79b9_7f4a_7c15);
        s.set(v);
        v
    });
    let mut x = now ^ seq.wrapping_mul(0x1000_0000);
    let mut out = String::with_capacity(16);
    out.push_str("REDEEM-");
    for i in 0..8 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let nib = (x & 0xf) as u8;
        let c = if nib < 10 {
            b'0' + nib
        } else {
            b'A' + (nib - 10)
        };
        out.push(c as char);
        if i == 3 {
            out.push('-');
        }
    }
    out
}

/// 按优先级 + 权重从候选渠道选一个（纯函数，可测）。
///
/// 选择策略：
/// 1. 过滤 `enabled` 且 `models` 含目标模型（或渠道允许全部，但本实现按显式列表匹配）
///    的候选；若 `channels` 为空或无匹配，返回 `None`。
/// 2. 取候选中最小 `priority` 组（故障转移：先试最优先组）。
/// 3. 在该组内按 `weight` 加权随机选一个（权重越大越可能命中）。
#[must_use]
pub fn select_channel<'a>(channels: &'a [Channel], model: &str) -> Option<&'a Channel> {
    // 候选：enabled 且支持该模型
    let candidates: Vec<&Channel> = channels
        .iter()
        .filter(|c| c.enabled && c.models.iter().any(|m| m == model))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    // 最小 priority 组
    let min_prio = candidates.iter().map(|c| c.priority).min()?;
    let top: Vec<&Channel> = candidates
        .iter()
        .copied()
        .filter(|c| c.priority == min_prio)
        .collect();
    if top.is_empty() {
        return None;
    }
    if top.len() == 1 {
        return Some(top[0]);
    }
    // 加权随机（确定性：用模型名 hash 作种子，保证同 model 在同状态下稳定路由；
    // 真实场景下随时间变化由调用方传入不同种子，但本函数保持纯函数特性）
    let total_weight: u64 = top.iter().map(|c| c.weight.max(1) as u64).sum();
    if total_weight == 0 {
        return Some(top[0]);
    }
    let seed = fxhash_str(model);
    let mut pick = seed % total_weight;
    for c in &top {
        let w = c.weight.max(1) as u64;
        if pick < w {
            return Some(*c);
        }
        pick -= w;
    }
    Some(top[top.len() - 1])
}

/// 从 `Authorization` 头提取 bearer token。
///
/// 形如 `Bearer sk-os-xxx` → `Some("sk-os-xxx")`；无 `Bearer ` 前缀 / 空 → `None`。
#[must_use]
pub fn extract_bearer(auth_header: &str) -> Option<String> {
    let trimmed = auth_header.trim();
    let lower = trimmed.to_ascii_lowercase();
    lower.strip_prefix("bearer ")?;
    let token = trimmed[7..].trim(); // 7 = "Bearer ".len()
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

/// 简易字符串 hash（FNV-1a 64bit 变体），用于 select_channel 的确定性种子。
fn fxhash_str(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

// ----------------------------------------------------------------------------
// ApiGatewayRouteHandler
// ----------------------------------------------------------------------------

/// 转发前置决策（[`ApiGatewayRouteHandler::resolve_forward_plan`] 的产物）——
/// 鉴权 + 配额 + 模型白名单 + 候选渠道排序完成后的"怎么转发"快照。
///
/// 由非流式 `proxy_forward` 与流式转发（http.rs SSE 特挂路由）共用，保证两条
/// 路径的鉴权/选路口径**完全一致**（单一真相）。
pub struct ForwardPlan {
    /// 命中的下游令牌快照（记日志/扣配额用）。
    pub token: ApiToken,
    /// 客户端请求的对外模型名（记日志/查倍率用；转发给上游前可能被映射覆盖）。
    pub model: String,
    /// 故障转移顺序的候选渠道（priority 升序 + 同优先级加权首选打头）。
    pub ordered: Vec<Channel>,
    /// 映射命中的上游真实模型名（None=不覆盖请求体里的 model 字段）。
    pub upstream_model_override: Option<String>,
}

/// LLM API 网关路由处理器——HTTP 边界适配到 SQLite 持久化（渠道/令牌/日志/映射）。
///
/// rusqlite `Connection` 是 `Send` 但非 `Sync`，用 `Mutex<Connection>` 包裹。
/// 所有 DB 访问用短锁快查快放（同步执行，不跨 `.await` 持锁）。
pub struct ApiGatewayRouteHandler {
    db: Mutex<Connection>,
    counter: Mutex<u64>,
    /// 链上支付验真网关（dApp 一期）：构造时读 env 定格，测试经
    /// [`Self::with_chain_verify`] 注入（与 NexHub handler 同一类型/同一语义）。
    chain_verify: ChainPayGate,
    /// overlay 中继端点（2026-09-03 渠道中继）：`via_node` 非空渠道的执行通道
    /// ——main.rs 在 api_market 联邦端点建好后 `set_relay` 注入（与
    /// llm_external 同款模式）；None = 未装配（中继渠道报「中继失败：通道未
    /// 装配」，故障转移下一渠道）。
    relay: Mutex<Option<crate::handlers::api_market::ApiMarketFedEndpoint>>,
    /// 外部 API 登记读取源（一键导入 `from_external_api` 用）：main.rs 注入
    /// `LlmRouteHandler::external_state()`（同一 `Mutex<Connection>`，无跨连接
    /// 竞态）；None = 未装配（导入报 503）。
    external_source: Mutex<Option<std::sync::Arc<crate::handlers::llm_external::LlmExternalState>>>,
}

impl ApiGatewayRouteHandler {
    /// 构造 handler，打开/创建 SQLite 文件并建表（生产**不 seed demo 数据**——
    /// 全真实数据；默认倍率表照常 INSERT OR IGNORE，那是配置默认值不是假数据）。
    #[must_use]
    pub fn new() -> Self {
        Self::with_db_path(&default_db_path())
    }

    /// 用指定 DB 路径构造（测试注入）。
    ///
    /// 打开文件 → 建表（IF NOT EXISTS）→ seed 默认倍率（demo 渠道/令牌/日志
    /// 仅 `#[cfg(test)]` 填充——生产空库起步，见 [`demo_channels`]）。
    #[must_use]
    pub fn with_db_path(path: &str) -> Self {
        let conn = open_db(path).unwrap_or_else(|e| {
            // 降级到内存库，避免 panic（与原降级语义一致：上游不在线也不 panic）
            eprintln!("api_gateway: 打开 SQLite {path} 失败（{e}），降级到内存库");
            Connection::open_in_memory().expect("内存库必成功")
        });
        // 初始化 counter 为已有最大数字 ID（避免重启后 ID 碰撞）
        let max_id = Self::compute_max_numeric_id(&conn);
        Self {
            db: Mutex::new(conn),
            counter: Mutex::new(max_id.max(100)),
            chain_verify: ChainPayGate::from_env(),
            relay: Mutex::new(None),
            external_source: Mutex::new(None),
        }
    }

    /// 扫描所有表的数字后缀 ID，取最大值（用于初始化 counter，避免重启后 ID 碰撞）。
    fn compute_max_numeric_id(conn: &Connection) -> u64 {
        let mut max: u64 = 0;
        for table in &["channels", "tokens", "logs", "mappings", "payment_orders"] {
            let sql = format!("SELECT id FROM {table}");
            let ids: Vec<String> = conn
                .prepare(&sql)
                .and_then(|mut s| {
                    let rows = s.query_map([], |row| row.get::<_, String>(0))?;
                    let mut out = Vec::new();
                    for r in rows {
                        out.push(r?);
                    }
                    Ok(out)
                })
                .unwrap_or_default();
            for id in ids {
                if let Some(n) = id.rsplit('-').next().and_then(|s| s.parse::<u64>().ok()) {
                    if n > max {
                        max = n;
                    }
                }
            }
        }
        max
    }

    /// 用临时内存库构造（测试注入，数据隔离，进程结束即丢）。
    #[must_use]
    pub fn with_empty() -> Self {
        let conn = Connection::open_in_memory().expect("内存库必成功");
        create_schema(&conn).expect("建表必成功");
        Self {
            db: Mutex::new(conn),
            counter: Mutex::new(100),
            chain_verify: ChainPayGate::from_env(),
            relay: Mutex::new(None),
            external_source: Mutex::new(None),
        }
    }

    /// 用临时内存库构造并 seed demo 数据（测试注入：每个实例独立隔离，
    /// 避免 `new()` 的共享文件库在并行测试下互相干扰）。
    #[must_use]
    pub fn with_demo_data() -> Self {
        let conn = Connection::open_in_memory().expect("内存库必成功");
        create_schema(&conn).expect("建表必成功");
        seed_if_empty(&conn).expect("seed 必成功");
        Self {
            db: Mutex::new(conn),
            counter: Mutex::new(100),
            chain_verify: ChainPayGate::from_env(),
            relay: Mutex::new(None),
            external_source: Mutex::new(None),
        }
    }

    /// 注入链上支付验真网关（链式构造器，测试用：固定结论/调用计数执行器 +
    /// 全控配置；生产路径经 [`ChainPayGate::from_env`] 构造时定格）。
    #[must_use]
    pub fn with_chain_verify(mut self, gate: ChainPayGate) -> Self {
        self.chain_verify = gate;
        self
    }

    /// 注入 overlay 中继端点（main.rs 装配：api_market_fed 在 Box 进网关前取出
    /// 后传入——与 llm_external 的 set_relay 同款；Clone 共享内核，p2p spawn 后
    /// set_p2p 即生效）。测试注入 fake 互连端点（`set_full_transport` 互投）。
    pub fn set_relay(&self, relay: Option<crate::handlers::api_market::ApiMarketFedEndpoint>) {
        *self.relay.lock().expect("api-gateway relay poisoned") = relay;
    }

    /// 中继端点快照（`via_node` 渠道的非流式/流式执行分支取用；http.rs 特挂
    /// 路径经 [`Self::open_channel_relay_stream`] 间接取用；film.rs 的 tts/music
    /// 二进制中继转发同取此端点——pub(crate)，2026-09-04，仅可见性变化）。
    pub(crate) fn relay_endpoint(
        &self,
    ) -> Option<crate::handlers::api_market::ApiMarketFedEndpoint> {
        self.relay.lock().ok().and_then(|g| g.clone())
    }

    /// 注入外部 API 登记读取源（main.rs 装配：`llm.external_state()`——同一
    /// `Mutex<Connection>`，一键导入读行无跨连接竞态）。
    pub fn set_external_source(
        &self,
        state: Option<std::sync::Arc<crate::handlers::llm_external::LlmExternalState>>,
    ) {
        *self
            .external_source
            .lock()
            .expect("api-gateway external source poisoned") = state;
    }

    /// 当前全量渠道快照（从 DB 查）。
    #[must_use]
    pub fn channels_snapshot(&self) -> Vec<Channel> {
        let conn = self.db.lock().expect("db poisoned");
        load_all_channels(&conn).unwrap_or_default()
    }

    /// 当前全量令牌快照（从 DB 查）。
    #[must_use]
    pub fn tokens_snapshot(&self) -> Vec<ApiToken> {
        let conn = self.db.lock().expect("db poisoned");
        load_all_tokens(&conn).unwrap_or_default()
    }

    /// 当前全量调用日志快照（从 DB 查，按时间正序；测试断言 usage/备注用）。
    #[must_use]
    pub fn logs_snapshot(&self) -> Vec<CallLog> {
        let conn = self.db.lock().expect("db poisoned");
        load_all_logs(&conn).unwrap_or_default()
    }

    fn next_id(&self, prefix: &str) -> String {
        let mut c = self.counter.lock().expect("counter poisoned");
        *c += 1;
        format!("{prefix}-{}", *c)
    }

    /// 渠道连通性测试：reqwest GET 上游 `/v1/models`，成功返回 ok + 探测到的模型数。
    ///
    /// 上游不在线 / 网络不通 → 返回 Err（caller 包成 error 响应），不 panic。
    async fn test_channel_upstream(base_url: &str, api_key: &str) -> Result<Vec<String>, String> {
        let url = format!("{}/models", base_url.trim_end_matches('/'));
        let mut req = HTTP.get(&url).timeout(Duration::from_secs(5));
        if !api_key.trim().is_empty() {
            req = req.bearer_auth(api_key);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("上游探测失败（请求发送失败）: {e}"))?;
        let resp = resp
            .error_for_status()
            .map_err(|e| format!("上游探测失败（HTTP 错误）: {e}"))?;
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("解析上游响应失败: {e}"))?;
        let ids = v
            .get("data")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(ids)
    }

    /// 代理转发到上游：reqwest POST 上游 `/chat/completions` 或 `/completions`，
    /// 透传请求体。成功返回（响应体原文, usage 三元组），失败返回 Err。
    ///
    /// 非流式总超时 [`upstream_timeout`]（缺省 300s，env
    /// `NEXOS_GATEWAY_UPSTREAM_TIMEOUT_SECS` 可配——长生成（devdocs AI 翻译的
    /// 6K 字块 + 思考段）超旧值 60s 会被掐出 502）；连接阶段另有共享 client
    /// 的 10s connect_timeout 先掐，死上游不影响故障转移速度。
    async fn forward_upstream(
        base_url: &str,
        api_key: &str,
        suffix: &str, // "chat/completions" or "completions"
        body: &serde_json::Value,
    ) -> Result<(String, Option<(u32, u32, u32)>), String> {
        let url = format!("{base_url}/{suffix}");
        let payload =
            serde_json::to_string(body).map_err(|e| format!("构造转发请求体失败: {e}"))?;
        let payload = fixup_model_in_payload(&payload, body);
        let mut req = HTTP
            .post(&url)
            .timeout(upstream_timeout())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(payload);
        if !api_key.trim().is_empty() {
            req = req.bearer_auth(api_key);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("上游转发请求失败: {e}"))?;
        let resp = resp
            .error_for_status()
            .map_err(|e| format!("上游返回错误: {e}"))?;
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("读取上游响应失败: {e}"))?;
        let text = String::from_utf8_lossy(&bytes).to_string();
        // 解析 usage
        let usage = parse_usage(&bytes);
        Ok((text, usage))
    }

    /// 发起上游**流式**请求（SSE 逐块透传用）：reqwest POST 上游
    /// `/chat/completions` 或 `/completions`，不聚合响应体——返回
    /// `reqwest::Response`，由调用方经 `bytes_stream()` 逐块转发给客户端。
    ///
    /// 与 [`Self::forward_upstream`] 的差异：
    /// - 用 [`HTTP_STREAM`]（无总超时，SSE 长连接不被掐断）；
    /// - **不做** `error_for_status`——非 2xx 时调用方仍要读 body 拿错误详情
    ///   记日志，由调用方按 `status().is_success()` 决定"接管流"还是"故障转移"。
    ///
    /// 失败（连接/发送阶段）返回 `Err(错误文案)`；HTTP 层错误状态不在此判。
    pub async fn open_upstream_stream(
        base_url: &str,
        api_key: &str,
        suffix: &str,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response, String> {
        let url = format!("{base_url}/{suffix}");
        let payload =
            serde_json::to_string(body).map_err(|e| format!("构造转发请求体失败: {e}"))?;
        let mut req = HTTP_STREAM
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(payload);
        if !api_key.trim().is_empty() {
            req = req.bearer_auth(api_key);
        }
        req.send()
            .await
            .map_err(|e| format!("上游转发请求失败: {e}"))
    }

    // ------------------------------------------------------------------
    // 中继渠道执行（via_node 非空 → api_market relay 执行层，2026-09-03）
    // ------------------------------------------------------------------

    /// 中继渠道的整包转发超时（与直连 [`upstream_timeout`] 同源同值：缺省 300s、
    /// env `NEXOS_GATEWAY_UPSTREAM_TIMEOUT_SECS` 可配——中继多一跳 overlay 但
    /// 生成时长同量级，不应比直连更早掐；relay 协议自身另有源端 600s 总上限
    /// 兜底）。首帧窗口（流式 Head / 非流式整包完成）同此预算。
    fn relay_chat_timeout() -> Duration {
        upstream_timeout()
    }

    /// 中继渠道模型探测超时（GET /models，与 `test_channel_upstream` 5s/llm_external
    /// TEST_TIMEOUT 10s 同量级——中继多一跳 overlay，取 10s）。
    fn relay_test_timeout() -> Duration {
        Duration::from_secs(10)
    }

    /// 组装中继渠道的一次请求（公共面：非流式 [`Self::forward_channel`] 与流式
    /// [`Self::open_channel_relay_stream`] / http.rs 特挂共用——URL=base_url+path、
    /// 鉴权头=channel.api_key、via_node=channel.via_node，避免两处复制漂移）。
    ///
    /// 2026-09-04 起 `pub(crate)`：影片管线（film.rs）的非 chat 能力（tts/music
    /// 等二进制响应形态）经同一组装面走中继，与网关自身口径一致（仅可见性
    /// 变化，零行为回归）。
    pub(crate) fn channel_relay_request(
        ch: &Channel,
        suffix: &str,
        body: &serde_json::Value,
        stream: bool,
    ) -> Result<crate::handlers::api_market::ApiRelayRequest, String> {
        let url = format!("{}/{suffix}", ch.base_url.trim_end_matches('/'));
        let payload = if suffix == "models" {
            None
        } else {
            Some(serde_json::to_vec(body).map_err(|e| format!("构造转发请求体失败: {e}"))?)
        };
        let headers = if ch.api_key.trim().is_empty() {
            Vec::new()
        } else {
            vec![(
                "Authorization".to_string(),
                format!("Bearer {}", ch.api_key.trim()),
            )]
        };
        Ok(crate::handlers::api_market::ApiRelayRequest {
            method: if suffix == "models" { "GET" } else { "POST" }.to_string(),
            url,
            headers,
            body: payload,
            stream,
        })
    }

    /// 中继失败错误文案（与直连失败明确可区分，日志/响应透传展示）。
    fn relay_channel_fail(ch: &Channel, reason: String) -> String {
        format!(
            "经 {} 中继失败: {reason}",
            crate::handlers::api_market::short_node_label(&ch.via_node)
        )
    }

    /// 渠道转发统一入口（非流式）：`via_node` 非空走 api_market relay 执行层
    /// （overlay 定向源节点代发），否则直连 reqwest——`proxy_forward` 的故障
    /// 转移循环只认本函数的 `Ok/Err`，两条路径口径一致（usage 解析同款）。
    ///
    /// 2026-09-04 起 `pub(crate)`：影片管线（film.rs）的 chat/image 阶段经同一
    /// 入口复用渠道转发（直连 + 中继两形态零复制）；仅可见性变化，零回归。
    pub(crate) async fn forward_channel(
        &self,
        ch: &Channel,
        suffix: &str,
        body: &serde_json::Value,
    ) -> Result<(String, Option<(u32, u32, u32)>), String> {
        if ch.via_node.trim().is_empty() {
            return Self::forward_upstream(&ch.base_url, &ch.api_key, suffix, body).await;
        }
        // 中继路径：relay_roundtrip 整包（status/headers/body）→ 非 2xx 聚合错误
        // 详情（前 200 字符）→ 2xx 原文 + usage 解析（与直连同款产物形状）。
        let req = Self::channel_relay_request(ch, suffix, body, false)?;
        let Some(ep) = self.relay_endpoint() else {
            return Err(Self::relay_channel_fail(
                ch,
                "P2P 通道未装配（NEXOS_P2P_ENABLE=1 且对端组网后可用）".into(),
            ));
        };
        let done = ep
            .relay_roundtrip(&ch.via_node, req, Self::relay_chat_timeout())
            .await
            .map_err(|e| Self::relay_channel_fail(ch, e))?;
        if !(200..300).contains(&done.status) {
            let detail: String = String::from_utf8_lossy(&done.body)
                .chars()
                .take(200)
                .collect();
            return Err(Self::relay_channel_fail(
                ch,
                format!("上游返回错误: HTTP {} {detail}", done.status),
            ));
        }
        let text = String::from_utf8_lossy(&done.body).to_string();
        let usage = parse_usage(&done.body);
        Ok((text, usage))
    }

    /// 渠道流式打开统一入口（`via_node` 非空 → relay stream:true 分块回传）：
    /// 首帧（Head）即上游响应头/状态，返回 [`RelayStream`] 供调用方逐块透传。
    /// 供 http.rs SSE 特挂路径共用（与非流式 [`Self::forward_channel`] 同一
    /// 请求组装面）。首帧窗口与直连首块同口径（120s——思考模型 TTFT 余量）。
    pub(crate) async fn open_channel_relay_stream(
        &self,
        ch: &Channel,
        suffix: &str,
        body: &serde_json::Value,
    ) -> Result<crate::handlers::api_market::RelayStream, String> {
        let req = Self::channel_relay_request(ch, suffix, body, true)?;
        let Some(ep) = self.relay_endpoint() else {
            return Err(Self::relay_channel_fail(
                ch,
                "P2P 通道未装配（NEXOS_P2P_ENABLE=1 且对端组网后可用）".into(),
            ));
        };
        ep.relay_open_stream(&ch.via_node, req, Self::relay_chat_timeout())
            .await
            .map_err(|e| Self::relay_channel_fail(ch, e))
    }

    /// 中继渠道模型探测（GET `<base_url>/models` 经源节点代发）——一键导入
    /// `from_external_api` 的 models 空回填用；产物与直连
    /// [`Self::test_channel_upstream`] 同构（真实清单，零编造）。
    async fn relay_probe_models(&self, ch: &Channel) -> Result<Vec<String>, String> {
        let req = Self::channel_relay_request(ch, "models", &serde_json::Value::Null, false)?;
        let Some(ep) = self.relay_endpoint() else {
            return Err(Self::relay_channel_fail(
                ch,
                "P2P 通道未装配（NEXOS_P2P_ENABLE=1 且对端组网后可用）".into(),
            ));
        };
        let done = ep
            .relay_roundtrip(&ch.via_node, req, Self::relay_test_timeout())
            .await
            .map_err(|e| Self::relay_channel_fail(ch, e))?;
        if !(200..300).contains(&done.status) {
            return Err(Self::relay_channel_fail(
                ch,
                format!("HTTP {}", done.status),
            ));
        }
        let v: serde_json::Value = serde_json::from_slice(&done.body)
            .map_err(|e| Self::relay_channel_fail(ch, format!("响应非 JSON: {e}")))?;
        Ok(crate::handlers::llm_external::parse_model_ids(&v))
    }

    /// 转发前置决策：鉴权（Bearer sk-os-）→ 令牌有效性 → 配额闸门 → model 字段
    /// 校验 → 模型白名单 → 候选渠道（allowed_channels 限定 + 映射收敛 + 排序）。
    ///
    /// 抽自原 `proxy_forward` 前半段（2026-08-31 流式转发改造）：两条转发路径
    /// 共用同一决策，错误码语义不变（401/429/400/403/404，见各分支文案）。
    /// `Err(ApiResponse)` 为可直接回给客户端的错误响应；`Ok(plan)` 进入转发阶段。
    pub fn resolve_forward_plan(
        &self,
        headers: &serde_json::Value,
        body: &serde_json::Value,
    ) -> Result<ForwardPlan, ApiResponse> {
        // 1. 提取 bearer token
        let auth = headers
            .get("Authorization")
            .and_then(|v| v.as_str())
            .or_else(|| headers.get("authorization").and_then(|v| v.as_str()))
            .unwrap_or("");
        let Some(key) = extract_bearer(auth) else {
            return Err(error_response(401, "缺少 Authorization Bearer token"));
        };
        // 2. 查令牌（短锁 DB 快查，拿到快照后释放锁做异步转发）
        let token_snap = {
            let conn = self.db.lock().expect("db poisoned");
            find_token_by_key(&conn, &key).unwrap_or(None)
        };
        let Some(token) = token_snap else {
            return Err(error_response(401, "无效的 API Key"));
        };
        if !token.enabled || token.status == "disabled" {
            return Err(error_response(401, "令牌已禁用"));
        }
        if token.status == "expired" {
            return Err(error_response(401, "令牌已过期"));
        }
        if let Some(exp) = &token.expires_at {
            if !exp.is_empty() && is_expired(exp) {
                return Err(error_response(401, "令牌已过期"));
            }
        }
        // 计费模式分流：free 不检查配额；per_token/per_image/credits 沿用现有
        // 配额拒绝语义（quota_used >= quota_limit → 429）
        if token.billing_mode != "free"
            && token.quota_limit > 0
            && token.quota_used >= token.quota_limit
        {
            return Err(error_response(429, "配额已用尽"));
        }
        // 3. 取请求 body 的 model 字段
        let model = body
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        if model.is_empty() {
            return Err(error_response(400, "请求体缺少 model 字段"));
        }
        // 检查令牌允许的模型
        if !token.allowed_models.is_empty() && !token.allowed_models.iter().any(|m| m == &model) {
            return Err(error_response(403, "该令牌无权调用此模型"));
        }
        // 4. 查模型映射 → 候选渠道（短锁 DB 快查）
        let (channels_snap, mapping_snap) = {
            let conn = self.db.lock().expect("db poisoned");
            let chs = load_all_channels(&conn).unwrap_or_default();
            let maps = load_all_mappings(&conn).unwrap_or_default();
            (chs, maps)
        };
        // 候选渠道：限定令牌 allowed_channels（空=全部）
        let mut pool: Vec<Channel> = channels_snap
            .iter()
            .filter(|c| {
                c.enabled
                    && (token.allowed_channels.is_empty()
                        || token.allowed_channels.iter().any(|id| id == &c.id))
            })
            .cloned()
            .collect();
        // 应用映射：若存在 public_name=model 的映射，把映射的 channel+upstream_model 纳入
        let mut upstream_model_override: Option<String> = None;
        if let Some(m) = mapping_snap.iter().find(|m| m.public_name == model) {
            // 限定到映射指定的渠道
            pool.retain(|c| c.id == m.channel_id);
            upstream_model_override = Some(m.upstream_model.clone());
        }
        // 按 priority 排序候选
        pool.sort_by_key(|c| c.priority);
        if pool.is_empty() {
            return Err(error_response(404, "无可用渠道支持该模型"));
        }
        // 先按 select_channel 选首选（同模型 + 加权），其余按 priority 依次排后
        let mut ordered: Vec<Channel> = Vec::new();
        if let Some(first) = select_channel(&pool, &model) {
            ordered.push(first.clone());
            for c in &pool {
                if c.id != first.id {
                    ordered.push(c.clone());
                }
            }
        } else {
            ordered = pool.clone();
        }
        Ok(ForwardPlan {
            token,
            model,
            ordered,
            upstream_model_override,
        })
    }

    /// 代理转发核心：鉴权 + 选渠道 + 转发 + 记日志 + 配额扣减 + 故障转移。
    async fn proxy_forward(
        &self,
        req: ApiRequest,
        suffix: &str, // "chat/completions" or "completions"
    ) -> Result<ApiResponse, ApiGatewayError> {
        let started = std::time::Instant::now();
        let plan = match self.resolve_forward_plan(&req.headers, &req.body) {
            Ok(p) => p,
            Err(resp) => return Ok(resp),
        };
        let token = &plan.token;
        let model = plan.model.as_str();
        let upstream_model_override = &plan.upstream_model_override;
        let ordered = &plan.ordered;
        // 5. 故障转移：按 priority 依次尝试，直到成功或全失败（via_node 非空的
        //    中继渠道经 api_market relay 执行层代发，失败照常转移下一渠道）
        let mut last_err = String::from("无渠道可转发");
        for ch in ordered {
            // 修正请求体里的 model 字段（若映射覆盖）
            let mut fwd_body = req.body.clone();
            if let Some(up) = &upstream_model_override {
                if let serde_json::Value::Object(ref mut map) = fwd_body {
                    map.insert("model".into(), serde_json::Value::String(up.clone()));
                }
            }
            match self.forward_channel(ch, suffix, &fwd_body).await {
                Ok((resp_text, usage)) => {
                    let (pt, ct, tt) = usage.unwrap_or((0, 0, 0));
                    let latency = started.elapsed().as_millis() as u64;
                    // 记成功日志 + 配额扣减 + 渠道计数（写 DB）
                    self.record_success(token, ch, model, pt, ct, tt, latency, None);
                    return Ok(ApiResponse {
                        status: 200,
                        body: serde_json::from_str(&resp_text)
                            .unwrap_or_else(|_| serde_json::json!({ "raw": resp_text })),
                        headers: serde_json::json!({}),
                    });
                }
                Err(e) => {
                    last_err = e;
                    // 记失败日志（每个失败渠道一条），继续尝试下一个（写 DB）
                    self.record_failure(token, ch, model, &last_err);
                }
            }
        }
        // 6. 全失败 → 502
        Ok(error_response(
            502,
            &format!("所有渠道转发失败: {last_err}"),
        ))
    }

    /// 记录成功调用日志 + 扣配额 + 渠道/令牌计数（全部写 DB，短锁快放）。
    ///
    /// `note`（Some=非空文案）写进日志 `error` 字段——**成功但有事实性备注**的
    /// 场景：流式转发上游未上报 usage 时记 "上游未上报 usage"（token 记 0，
    /// 真实数据铁律：不估算不编造）。
    #[allow(clippy::too_many_arguments)]
    pub fn record_success(
        &self,
        token: &ApiToken,
        channel: &Channel,
        model: &str,
        pt: u32,
        ct: u32,
        tt: u32,
        latency_ms: u64,
        note: Option<&str>,
    ) {
        let now = now_iso();
        let log = CallLog {
            id: self.next_id("log"),
            token_id: token.id.clone(),
            token_name: token.name.clone(),
            channel_id: channel.id.clone(),
            channel_name: channel.name.clone(),
            model: model.to_string(),
            prompt_tokens: pt,
            completion_tokens: ct,
            total_tokens: tt,
            latency_ms,
            status: "success".into(),
            error: note.filter(|s| !s.is_empty()).map(String::from),
            created_at: now.clone(),
        };
        let conn = self.db.lock().expect("db poisoned");
        let _ = insert_log(&conn, &log);
        // 渠道计数 + last_used
        let _ = bump_channel(&conn, &channel.id, &now);
        // 计费扣减（按 billing_mode 分流）：
        // - free：不扣费（cost=0，仅累计 request_count/last_used）
        // - per_token / credits：cost = ceil(total_tokens × model_ratio × group_ratio)
        //   （credits 模式一切消费扣积分，与 per_token 同计量）
        // - per_image：文本转发也按现有 token 计量扣（图片单价另算叠加，
        //   生图端点另经 try_charge_image 扣 IMAGE_PRICE_CREDITS）
        let cost = match token.billing_mode.as_str() {
            "free" => 0,
            _ => {
                let model_ratio = get_model_ratio(&conn, model);
                let group_ratio = get_group_ratio(&conn, &token.group_name);
                calc_cost(tt as u64, model_ratio, group_ratio)
            }
        };
        let _ = bump_token(&conn, &token.id, cost, &now);
    }

    /// 记录失败调用日志（写 DB，短锁快放；流式转发的渠道级失败同样走这里）。
    pub fn record_failure(&self, token: &ApiToken, channel: &Channel, model: &str, err: &str) {
        let log = CallLog {
            id: self.next_id("log"),
            token_id: token.id.clone(),
            token_name: token.name.clone(),
            channel_id: channel.id.clone(),
            channel_name: channel.name.clone(),
            model: model.to_string(),
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            latency_ms: 0,
            status: "failed".into(),
            error: Some(err.to_string()),
            created_at: now_iso(),
        };
        let conn = self.db.lock().expect("db poisoned");
        let _ = insert_log(&conn, &log);
    }

    /// 生图扣费入口（原死代码 `charge_image_call` 的公开化改造，2026-08-20 接线）：
    /// 按 Bearer（sk-os- 令牌）**在同一次 DB 锁内**完成 查令牌 → 有效性校验 →
    /// 余额预检 → 扣 [`IMAGE_PRICE_CREDITS`]——查-检-扣原子，并发请求不会超扣。
    ///
    /// 调用方：media-gen `POST /api/v1/media/image`（生成前的最后一步预检，
    /// 失败即拒，不烧 GPU）。**须与 api_gateway 组件共享同一实例**（main.rs 经
    /// `Arc` 装配）：`Mutex<Connection>` 是原子性的边界，两个实例各持一条连接
    /// 会让 SELECT→UPDATE 之间出现竞态。
    ///
    /// 返回：
    /// - `Ok(outcome)`：见 [`ChargeOutcome`]（free → charged=false；其余 → 扣
    ///   `IMAGE_PRICE_CREDITS` 后 charged=true，token_name 供归因）。
    /// - `Err(msg)`：令牌表未命中 / 已禁用 / 已过期 → 调用方回 **401**；
    ///   余额不足（文案含 [`IMAGE_CHARGE_INSUFFICIENT_MARKER`]）→ 调用方回 **402**。
    ///
    /// 余额闸门：`quota_limit > 0`（0=无限，与 `ApiToken` 契约一致）且
    /// `quota_used + IMAGE_PRICE_CREDITS > quota_limit` → 拒绝且**不预扣不超扣**。
    /// 扣费成功同时累计 `request_count` / `last_used`（与转发成功同款语义）。
    pub async fn try_charge_image(&self, bearer: &str) -> Result<ChargeOutcome, String> {
        let key = bearer.trim();
        // 单次 DB 锁内完成：查令牌 → 校验 → 余额预检 → 扣费（原子，防并发超扣；
        // 全程无 await，锁不跨异步点）
        let conn = self.db.lock().expect("db poisoned");
        let token = find_token_by_key(&conn, key)
            .map_err(|e| format!("查询令牌失败: {e}"))?
            .ok_or_else(|| "无效的 API Key（令牌表未命中）".to_string())?;
        if !token.enabled || token.status == "disabled" {
            return Err("令牌已禁用".to_string());
        }
        if token.status == "expired" {
            return Err("令牌已过期".to_string());
        }
        if let Some(exp) = token.expires_at.as_deref().filter(|s| !s.is_empty()) {
            if is_expired(exp) {
                return Err("令牌已过期".to_string());
            }
        }
        if token.billing_mode == "free" {
            return Ok(ChargeOutcome {
                charged: false,
                token_name: token.name,
            });
        }
        if token.quota_limit > 0
            && token.quota_used.saturating_add(IMAGE_PRICE_CREDITS) > token.quota_limit
        {
            return Err(format!(
                "积分{IMAGE_CHARGE_INSUFFICIENT_MARKER}：生图需 {IMAGE_PRICE_CREDITS} 积分，令牌「{}」仅剩 {} 积分（已用 {}/{}），请充值后重试",
                token.name,
                token.quota_limit - token.quota_used,
                token.quota_used,
                token.quota_limit,
            ));
        }
        bump_token(&conn, &token.id, IMAGE_PRICE_CREDITS, &now_iso())
            .map_err(|e| format!("生图扣费写入失败: {e}"))?;
        Ok(ChargeOutcome {
            charged: true,
            token_name: token.name,
        })
    }
}

impl Default for ApiGatewayRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

// ----------------------------------------------------------------------------
// 漏洞3：GET 响应脱敏辅助（POST 创建时仍返回完整 key 一次）
// ----------------------------------------------------------------------------

/// 返回一份脱敏后的渠道快照（api_key 字段用 mask_secret 脱敏）。
///
/// 漏洞3：GET /channels / GET /channels/:id 响应里 Channel.api_key 不再明文回显。
fn masked_channels_snapshot(channels: &[Channel]) -> Vec<Channel> {
    channels
        .iter()
        .map(|c| {
            let mut m = c.clone();
            m.api_key = mask_secret(&c.api_key);
            m
        })
        .collect()
}

/// 返回一份脱敏后的令牌快照（key 字段用 mask_secret 脱敏）。
///
/// 漏洞3：GET /tokens 响应里 ApiToken.key 不再明文回显。
/// 注：POST /tokens 创建时仍返回完整 key 一次（One API 行为，便于 admin 复制）。
fn masked_tokens_snapshot(tokens: &[ApiToken]) -> Vec<ApiToken> {
    tokens
        .iter()
        .map(|t| {
            let mut m = t.clone();
            m.key = mask_secret(&t.key);
            m
        })
        .collect()
}

#[async_trait]
impl RouteHandler for ApiGatewayRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            // 渠道管理（6 条）
            spec(HttpMethod::Get, "/api/v1/gateway/channels", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/gateway/channels",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Put,
                "/api/v1/gateway/channels/:id",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/gateway/channels/:id",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/gateway/channels/:id/test",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/gateway/channels/:id",
                false,
                vec![],
            ),
            // 令牌管理（5 条）
            spec(HttpMethod::Get, "/api/v1/gateway/tokens", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/gateway/tokens",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/gateway/tokens/:id",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/gateway/tokens/:id/disable",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/gateway/tokens/:id/enable",
                true,
                vec!["admin".into()],
            ),
            // 日志与统计（3 条）
            spec(HttpMethod::Get, "/api/v1/gateway/logs", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/gateway/stats", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/gateway/models", false, vec![]),
            // 模型映射（2 条）
            spec(HttpMethod::Get, "/api/v1/gateway/mappings", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/gateway/mappings",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/gateway/mappings/:name",
                true,
                vec!["admin".into()],
            ),
            // 模型倍率（3 条）
            spec(
                HttpMethod::Get,
                "/api/v1/gateway/model-ratios",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/gateway/model-ratios",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/gateway/model-ratios/:model",
                true,
                vec!["admin".into()],
            ),
            // 用户组倍率（2 条）
            spec(
                HttpMethod::Get,
                "/api/v1/gateway/group-ratios",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/gateway/group-ratios",
                true,
                vec!["admin".into()],
            ),
            // 兑换码（3 条：GET/POST 列表+生成 admin，POST redeem 任意 token）
            spec(
                HttpMethod::Get,
                "/api/v1/gateway/redeem-codes",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/gateway/redeem-codes",
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Post, "/api/v1/gateway/redeem", false, vec![]),
            // 加密货币充值（4 条：写操作 admin——创建/确认/拒绝；GET 列表公开读，
            // 与 GET /tokens、GET /logs 等列表路由一致）
            spec(
                HttpMethod::Post,
                "/api/v1/gateway/payments",
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Get, "/api/v1/gateway/payments", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/gateway/payments/:id/confirm",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/gateway/payments/:id/reject",
                true,
                vec!["admin".into()],
            ),
            // 代理转发（2 条）—— 不要求认证（用令牌本身鉴权，不走 admin）。
            // 注：`stream:true` 请求由 http.rs SSE 特挂路由接管（同路径逐块透传），
            // 本表两条覆盖非流式（特挂路由对非流式回落到 dispatch，零行为差）。
            spec(
                HttpMethod::Post,
                "/api/v1/gateway/v1/chat/completions",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/gateway/v1/completions",
                false,
                vec![],
            ),
            // OpenAI 形态模型列表（对外接入，2026-08-31）：同一 Bearer sk-os-
            // 令牌鉴权——转发用什么 key 这里就验什么 key，不进 admin 鉴权面。
            spec(HttpMethod::Get, "/api/v1/gateway/v1/models", false, vec![]),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        let query = req.path.split('?').nth(1).unwrap_or("");
        match (req.method, segs.as_slice()) {
            // ============ 渠道管理 ============

            // —— GET /api/v1/gateway/channels —— 列渠道
            (HttpMethod::Get, ["api", "v1", "gateway", "channels"]) => {
                // 漏洞3：api_key 脱敏（不回显明文）
                let snap = self.channels_snapshot();
                Ok(ok_json(to_value(&masked_channels_snapshot(&snap))?))
            }

            // —— POST /api/v1/gateway/channels —— 添加渠道（admin，支持
            //    from_discovery / from_external_api 一键导入）
            (HttpMethod::Post, ["api", "v1", "gateway", "channels"]) => {
                let body: CreateChannelBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析添加渠道请求体失败: {e}"))
                })?;
                // from_external_api 路径（2026-09-03 联邦中继一键导入）：查
                // llm_external_apis 行 → 复制 name/base_url/api_key/models/via_node
                //（models 空 → 先探 `<base_url>/models` 回填：via_node 非空经
                // overlay 中继探测，否则直连——产物与 test_channel_upstream 同构）。
                // 行不存在 404；源未装配 503。探测失败不阻塞导入（字段全是登记
                // 表里的真实数据，非猜测——建渠道但响应带 warning，models 可后
                // 手动补/编辑）。
                if let Some(ext_id) = body
                    .from_external_api
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    let Some(src_state) = self
                        .external_source
                        .lock()
                        .expect("api-gateway external source poisoned")
                        .clone()
                    else {
                        return Ok(error_response(
                            503,
                            "外部 API 登记源未装配（llm 组件未注册）",
                        ));
                    };
                    let Some(api) = src_state.get(ext_id) else {
                        return Ok(error_response(
                            404,
                            &format!("外部 API 登记不存在: {ext_id}"),
                        ));
                    };
                    let provider = body.provider.clone().unwrap_or_else(|| "openai".into());
                    if !is_valid_provider(&provider) {
                        return Ok(error_response(
                            400,
                            "provider 必须是 openai/deepseek/anthropic/local-vllm/azure/ollama",
                        ));
                    }
                    let mut models = body
                        .models
                        .clone()
                        .filter(|m| !m.is_empty())
                        .unwrap_or_else(|| api.models.clone());
                    let mut warning: Option<String> = None;
                    if models.is_empty() {
                        // 探测回填：构造临时渠道形态复用 channel_relay_request 的
                        // 组装面（via_node 非空经中继、否则直连同款探测）。
                        let probe_ch = Channel {
                            id: String::new(),
                            name: String::new(),
                            provider: provider.clone(),
                            base_url: api.base_url.clone(),
                            api_key: api.api_key.clone(),
                            models: vec![],
                            priority: 0,
                            weight: 1,
                            status: String::new(),
                            enabled: true,
                            created_at: String::new(),
                            last_used: None,
                            request_count: 0,
                            via_node: api.via_node.clone(),
                            price_per_call: 0.0,
                            price_per_sec: 0.0,
                            price_per_token: 0.0,
                        };
                        let probed = if probe_ch.via_node.trim().is_empty() {
                            Self::test_channel_upstream(&probe_ch.base_url, &probe_ch.api_key).await
                        } else {
                            self.relay_probe_models(&probe_ch).await
                        };
                        match probed {
                            Ok(ids) => models = ids,
                            Err(e) => {
                                warning = Some(format!(
                                    "models 探测失败（渠道已建，可稍后测试/编辑回填）: {e}"
                                ));
                            }
                        }
                    }
                    let channel = Channel {
                        id: self.next_id("ch"),
                        name: api.name.clone(),
                        provider,
                        base_url: api.base_url.clone(),
                        api_key: api.api_key.clone(),
                        models,
                        priority: body.priority.unwrap_or(0),
                        weight: body.weight.unwrap_or(1),
                        status: "enabled".into(),
                        enabled: true,
                        created_at: now_iso(),
                        last_used: None,
                        request_count: 0,
                        via_node: api.via_node.clone(),
                        price_per_call: 0.0,
                        price_per_sec: 0.0,
                        price_per_token: 0.0,
                    };
                    let mut resp = to_value(&channel)?;
                    if let (Some(w), serde_json::Value::Object(map)) = (warning, &mut resp) {
                        map.insert("warning".into(), serde_json::Value::String(w));
                    }
                    {
                        let conn = self.db.lock().expect("db poisoned");
                        insert_channel(&conn, &channel)?;
                    }
                    return Ok(ApiResponse {
                        status: 201,
                        body: resp,
                        headers: serde_json::json!({}),
                    });
                }
                // from_discovery 路径：后端实测该端口 /v1/models 填充字段
                // （与 /api/v1/llm/gateway/models 同一探测函数，口径一致）。
                let (name, provider, base_url, models) = if let Some(fd) = &body.from_discovery {
                    match crate::handlers::llm::probe_vllm_models(fd.port).await {
                        Ok(probed) => {
                            let models = fd
                                .models
                                .clone()
                                .filter(|m| !m.is_empty())
                                .unwrap_or_else(|| probed.iter().map(|m| m.id.clone()).collect());
                            (
                                fd.name
                                    .clone()
                                    .or_else(|| body.name.clone())
                                    .unwrap_or_else(|| format!("发现的 vLLM :{}", fd.port)),
                                "local-vllm".to_string(),
                                format!("http://127.0.0.1:{}/v1", fd.port),
                                models,
                            )
                        }
                        Err(e) => {
                            // 探测失败不建渠道（绝不把猜的端口当可用）
                            return Ok(error_response(
                                502,
                                &format!("端口 {} /v1/models 探测失败，未创建渠道: {e}", fd.port),
                            ));
                        }
                    }
                } else {
                    (
                        body.name.clone().unwrap_or_default(),
                        body.provider.clone().unwrap_or_default(),
                        body.base_url.clone().unwrap_or_default(),
                        body.models.clone().unwrap_or_default(),
                    )
                };
                if name.trim().is_empty() {
                    return Ok(error_response(400, "name 不可为空"));
                }
                if base_url.trim().is_empty() {
                    return Ok(error_response(400, "base_url 不可为空"));
                }
                if !is_valid_provider(&provider) {
                    return Ok(error_response(
                        400,
                        "provider 必须是 openai/deepseek/anthropic/local-vllm/azure/ollama",
                    ));
                }
                // via_node（中继渠道）：非空须是合法 NodeID（0x+66hex——
                // 一键导入自动写入；手填走同校验，防拼错定向不到源节点）。
                let via_node = body.via_node.clone().unwrap_or_default();
                if !via_node.trim().is_empty() && os_p2p::NodeId::parse(via_node.trim()).is_none() {
                    return Ok(error_response(
                        400,
                        "via_node 非法（应为 0x+66 hex NodeID——外部 API 一键导入时自动写入）",
                    ));
                }
                let channel = Channel {
                    id: self.next_id("ch"),
                    name,
                    provider,
                    base_url,
                    api_key: body.api_key.unwrap_or_default(),
                    models,
                    priority: body.priority.unwrap_or(0),
                    weight: body.weight.unwrap_or(1),
                    status: "enabled".into(),
                    enabled: true,
                    created_at: now_iso(),
                    last_used: None,
                    request_count: 0,
                    via_node,
                    price_per_call: body.price_per_call.unwrap_or(0.0),
                    price_per_sec: body.price_per_sec.unwrap_or(0.0),
                    price_per_token: body.price_per_token.unwrap_or(0.0),
                };
                let resp = to_value(&channel)?;
                {
                    let conn = self.db.lock().expect("db poisoned");
                    insert_channel(&conn, &channel)?;
                }
                Ok(ApiResponse {
                    status: 201,
                    body: resp,
                    headers: serde_json::json!({}),
                })
            }

            // —— PUT /api/v1/gateway/channels/:id —— 更新渠道（admin）
            (HttpMethod::Put, ["api", "v1", "gateway", "channels", id]) => {
                let body: UpdateChannelBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析更新渠道请求体失败: {e}"))
                })?;
                if let Some(p) = &body.provider {
                    if !is_valid_provider(p) {
                        return Ok(error_response(
                            400,
                            "provider 必须是 openai/deepseek/anthropic/local-vllm/azure/ollama",
                        ));
                    }
                }
                // via_node 提供即覆盖（空串 = 清除回直连语义）；同 POST 校验。
                if let Some(v) = &body.via_node {
                    if !v.trim().is_empty() && os_p2p::NodeId::parse(v.trim()).is_none() {
                        return Ok(error_response(
                            400,
                            "via_node 非法（应为 0x+66 hex NodeID——外部 API 一键导入时自动写入）",
                        ));
                    }
                }
                let conn = self.db.lock().expect("db poisoned");
                let mut c = match find_channel(&conn, id)? {
                    Some(c) => c,
                    None => return Ok(error_response(404, &format!("渠道不存在: {id}"))),
                };
                if let Some(v) = body.name {
                    c.name = v;
                }
                if let Some(v) = body.provider {
                    c.provider = v;
                }
                if let Some(v) = body.base_url {
                    c.base_url = v;
                }
                if let Some(v) = body.api_key {
                    c.api_key = v;
                }
                if let Some(v) = body.models {
                    c.models = v;
                }
                if let Some(v) = body.priority {
                    c.priority = v;
                }
                if let Some(v) = body.weight {
                    c.weight = v;
                }
                if let Some(v) = body.enabled {
                    c.enabled = v;
                    c.status = if v {
                        "enabled".into()
                    } else {
                        "disabled".into()
                    };
                }
                if let Some(v) = body.via_node {
                    c.via_node = v.trim().to_string();
                }
                if let Some(v) = body.price_per_call {
                    c.price_per_call = v;
                }
                if let Some(v) = body.price_per_sec {
                    c.price_per_sec = v;
                }
                if let Some(v) = body.price_per_token {
                    c.price_per_token = v;
                }
                update_channel(&conn, &c)?;
                Ok(ok_json(to_value(&c)?))
            }

            // —— DELETE /api/v1/gateway/channels/:id —— 删渠道（admin）
            (HttpMethod::Delete, ["api", "v1", "gateway", "channels", id]) => {
                let conn = self.db.lock().expect("db poisoned");
                let affected = delete_channel(&conn, id)?;
                if affected == 0 {
                    return Ok(error_response(404, &format!("渠道不存在: {id}")));
                }
                Ok(ok_json(
                    serde_json::json!({"ok": true, "id": id, "action": "delete"}),
                ))
            }

            // —— POST /api/v1/gateway/channels/:id/test —— 测试渠道连通性（admin；
            //    via_node 非空 = 中继渠道 → 经源节点代发 GET /models）
            (HttpMethod::Post, ["api", "v1", "gateway", "channels", id, "test"]) => {
                let snap = {
                    let conn = self.db.lock().expect("db poisoned");
                    find_channel(&conn, id)?
                };
                let Some(ch) = snap else {
                    return Ok(error_response(404, &format!("渠道不存在: {id}")));
                };
                let outcome = if ch.via_node.trim().is_empty() {
                    Self::test_channel_upstream(&ch.base_url, &ch.api_key).await
                } else {
                    self.relay_probe_models(&ch).await
                };
                match outcome {
                    Ok(models) => {
                        // 探测成功 → 同步 status enabled（写 DB）
                        let conn = self.db.lock().expect("db poisoned");
                        let _ = update_channel_status(&conn, id, "enabled");
                        Ok(ok_json(serde_json::json!({
                            "ok": true,
                            "id": id,
                            "models_detected": models,
                            "models_count": models.len(),
                        })))
                    }
                    Err(e) => {
                        // 探测失败 → 标记 error 状态（写 DB）
                        let conn = self.db.lock().expect("db poisoned");
                        let _ = update_channel_status(&conn, id, "error");
                        Ok(error_response(502, &e))
                    }
                }
            }

            // —— GET /api/v1/gateway/channels/:id —— 单渠道详情
            (HttpMethod::Get, ["api", "v1", "gateway", "channels", id]) => {
                let conn = self.db.lock().expect("db poisoned");
                match find_channel(&conn, id)? {
                    // 漏洞3：api_key 脱敏
                    Some(c) => {
                        let mut m = c.clone();
                        m.api_key = mask_secret(&c.api_key);
                        Ok(ok_json(to_value(&m)?))
                    }
                    None => Ok(error_response(404, &format!("渠道不存在: {id}"))),
                }
            }

            // ============ 令牌管理 ============

            // —— GET /api/v1/gateway/tokens —— 列令牌
            (HttpMethod::Get, ["api", "v1", "gateway", "tokens"]) => {
                // 漏洞3：key 脱敏（不回显明文；POST 创建时仍返回完整 key 一次）
                let snap = self.tokens_snapshot();
                Ok(ok_json(to_value(&masked_tokens_snapshot(&snap))?))
            }

            // —— POST /api/v1/gateway/tokens —— 创建令牌（admin，自动生成 key）
            (HttpMethod::Post, ["api", "v1", "gateway", "tokens"]) => {
                let body: CreateTokenBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析创建令牌请求体失败: {e}"))
                })?;
                if body.name.trim().is_empty() {
                    return Ok(error_response(400, "name 不可为空"));
                }
                // 计费模式校验（缺省 per_token；非法值 400）
                let billing_mode = body
                    .billing_mode
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(default_billing_mode);
                if !is_valid_billing_mode(&billing_mode) {
                    return Ok(error_response(
                        400,
                        "billing_mode 必须是 free/per_token/per_image/credits",
                    ));
                }
                // credits 模式：初始积分写进 quota_limit（预付余额）；未给初始积分
                // 则沿用显式 quota_limit（都未给 = 0，创建后可经充值订单加积分）
                let quota_limit = if billing_mode == "credits" {
                    body.initial_credits.or(body.quota_limit).unwrap_or(0)
                } else {
                    body.quota_limit.unwrap_or(0)
                };
                let token = ApiToken {
                    id: self.next_id("tok"),
                    name: body.name,
                    key: generate_api_key(),
                    status: "active".into(),
                    enabled: true,
                    quota_limit,
                    quota_used: 0,
                    allowed_models: body.allowed_models.unwrap_or_default(),
                    allowed_channels: body.allowed_channels.unwrap_or_default(),
                    group_name: body
                        .group_name
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "default".into()),
                    billing_mode,
                    expires_at: body.expires_at.filter(|s| !s.is_empty()),
                    created_at: now_iso(),
                    last_used: None,
                    request_count: 0,
                };
                let resp = to_value(&token)?;
                {
                    let conn = self.db.lock().expect("db poisoned");
                    insert_token(&conn, &token)?;
                }
                Ok(ApiResponse {
                    status: 201,
                    body: resp,
                    headers: serde_json::json!({}),
                })
            }

            // —— DELETE /api/v1/gateway/tokens/:id —— 删令牌（admin）
            (HttpMethod::Delete, ["api", "v1", "gateway", "tokens", id]) => {
                let conn = self.db.lock().expect("db poisoned");
                let affected = delete_token(&conn, id)?;
                if affected == 0 {
                    return Ok(error_response(404, &format!("令牌不存在: {id}")));
                }
                Ok(ok_json(
                    serde_json::json!({"ok": true, "id": id, "action": "delete"}),
                ))
            }

            // —— POST /api/v1/gateway/tokens/:id/disable —— 禁用令牌（admin）
            (HttpMethod::Post, ["api", "v1", "gateway", "tokens", id, "disable"]) => {
                let conn = self.db.lock().expect("db poisoned");
                match find_token(&conn, id)? {
                    Some(mut t) => {
                        t.enabled = false;
                        t.status = "disabled".into();
                        update_token(&conn, &t)?;
                        Ok(ok_json(to_value(&t)?))
                    }
                    None => Ok(error_response(404, &format!("令牌不存在: {id}"))),
                }
            }

            // —— POST /api/v1/gateway/tokens/:id/enable —— 启用令牌（admin）
            (HttpMethod::Post, ["api", "v1", "gateway", "tokens", id, "enable"]) => {
                let conn = self.db.lock().expect("db poisoned");
                match find_token(&conn, id)? {
                    Some(mut t) => {
                        t.enabled = true;
                        t.status = "active".into();
                        update_token(&conn, &t)?;
                        Ok(ok_json(to_value(&t)?))
                    }
                    None => Ok(error_response(404, &format!("令牌不存在: {id}"))),
                }
            }

            // ============ 日志与统计 ============

            // —— GET /api/v1/gateway/logs —— 调用日志（?limit= 分页，默认 50）
            (HttpMethod::Get, ["api", "v1", "gateway", "logs"]) => {
                let limit = parse_query_int(query, "limit", 50).min(500);
                let conn = self.db.lock().expect("db poisoned");
                // 最新在前（ORDER BY created_at DESC + LIMIT），走 created_at 索引
                let snap = load_logs(&conn, limit)?;
                Ok(ok_json(to_value(&snap)?))
            }

            // —— GET /api/v1/gateway/stats —— 聚合统计
            (HttpMethod::Get, ["api", "v1", "gateway", "stats"]) => {
                let conn = self.db.lock().expect("db poisoned");
                let channels = load_all_channels(&conn).unwrap_or_default();
                let tokens = load_all_tokens(&conn).unwrap_or_default();
                let logs = load_all_logs(&conn).unwrap_or_default();
                let channels_enabled = channels.iter().filter(|c| c.enabled).count();
                let tokens_active = tokens
                    .iter()
                    .filter(|t| t.enabled && t.status == "active")
                    .count();
                let total_requests: u64 = logs
                    .iter()
                    .map(|_| 1u64)
                    .sum::<u64>()
                    .max(channels.iter().map(|c| c.request_count).sum::<u64>());
                let total_tokens: u64 = logs.iter().map(|l| l.total_tokens as u64).sum();
                let success_count = logs.iter().filter(|l| l.status == "success").count();
                let success_rate = if logs.is_empty() {
                    0.0
                } else {
                    (success_count as f64 / logs.len() as f64) * 100.0
                };
                Ok(ok_json(to_value(&GatewayStats {
                    channels_total: channels.len(),
                    channels_enabled,
                    tokens_total: tokens.len(),
                    tokens_active,
                    total_requests,
                    total_tokens,
                    success_rate,
                })?))
            }

            // —— GET /api/v1/gateway/models —— 聚合所有渠道的可用模型列表（去重）
            (HttpMethod::Get, ["api", "v1", "gateway", "models"]) => {
                let conn = self.db.lock().expect("db poisoned");
                let channels = load_all_channels(&conn).unwrap_or_default();
                let mut set: Vec<String> = Vec::new();
                for c in channels.iter() {
                    if !c.enabled {
                        continue;
                    }
                    for m in &c.models {
                        if !set.contains(m) {
                            set.push(m.clone());
                        }
                    }
                }
                set.sort();
                Ok(ok_json(serde_json::json!({
                    "models": set,
                    "count": set.len(),
                })))
            }

            // ============ 模型映射 ============

            // —— GET /api/v1/gateway/mappings —— 列映射
            (HttpMethod::Get, ["api", "v1", "gateway", "mappings"]) => {
                let conn = self.db.lock().expect("db poisoned");
                let mappings = load_all_mappings(&conn).unwrap_or_default();
                Ok(ok_json(to_value(&mappings)?))
            }

            // —— POST /api/v1/gateway/mappings —— 添加映射（admin）
            (HttpMethod::Post, ["api", "v1", "gateway", "mappings"]) => {
                let body: CreateMappingBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析添加映射请求体失败: {e}"))
                })?;
                if body.public_name.trim().is_empty() {
                    return Ok(error_response(400, "public_name 不可为空"));
                }
                if body.channel_id.trim().is_empty() {
                    return Ok(error_response(400, "channel_id 不可为空"));
                }
                if body.upstream_model.trim().is_empty() {
                    return Ok(error_response(400, "upstream_model 不可为空"));
                }
                let mapping = ModelMapping {
                    public_name: body.public_name.clone(),
                    channel_id: body.channel_id,
                    upstream_model: body.upstream_model,
                };
                let resp = to_value(&mapping)?;
                {
                    let conn = self.db.lock().expect("db poisoned");
                    // INSERT OR REPLACE：覆盖同名映射
                    upsert_mapping(&conn, &mapping)?;
                }
                Ok(ApiResponse {
                    status: 201,
                    body: resp,
                    headers: serde_json::json!({}),
                })
            }

            // —— DELETE /api/v1/gateway/mappings/:name —— 删映射（admin）
            (HttpMethod::Delete, ["api", "v1", "gateway", "mappings", name]) => {
                let conn = self.db.lock().expect("db poisoned");
                let affected = delete_mapping(&conn, name)?;
                if affected == 0 {
                    return Ok(error_response(404, &format!("映射不存在: {name}")));
                }
                Ok(ok_json(
                    serde_json::json!({"ok": true, "name": name, "action": "delete"}),
                ))
            }

            // ============ 模型倍率（ModelRatio）============

            // —— GET /api/v1/gateway/model-ratios —— 列模型倍率
            (HttpMethod::Get, ["api", "v1", "gateway", "model-ratios"]) => {
                let conn = self.db.lock().expect("db poisoned");
                let ratios = load_all_model_ratios(&conn).unwrap_or_default();
                Ok(ok_json(to_value(&ratios)?))
            }

            // —— POST /api/v1/gateway/model-ratios —— 设置/更新模型倍率（admin）
            (HttpMethod::Post, ["api", "v1", "gateway", "model-ratios"]) => {
                let body: SetModelRatioBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析设置模型倍率请求体失败: {e}"))
                })?;
                if body.model.trim().is_empty() {
                    return Ok(error_response(400, "model 不可为空"));
                }
                if body.ratio < 0.0 {
                    return Ok(error_response(400, "ratio 不可为负"));
                }
                let ratio = ModelRatio {
                    model: body.model.clone(),
                    ratio: body.ratio,
                    updated_at: now_iso(),
                };
                {
                    let conn = self.db.lock().expect("db poisoned");
                    upsert_model_ratio(&conn, &ratio)?;
                }
                Ok(ok_json(to_value(&ratio)?))
            }

            // —— DELETE /api/v1/gateway/model-ratios/:model —— 删模型倍率（admin）
            (HttpMethod::Delete, ["api", "v1", "gateway", "model-ratios", model]) => {
                let conn = self.db.lock().expect("db poisoned");
                let affected = delete_model_ratio(&conn, model)?;
                if affected == 0 {
                    return Ok(error_response(404, &format!("模型倍率不存在: {model}")));
                }
                Ok(ok_json(
                    serde_json::json!({"ok": true, "model": model, "action": "delete"}),
                ))
            }

            // ============ 用户组倍率（GroupRatio）============

            // —— GET /api/v1/gateway/group-ratios —— 列组倍率
            (HttpMethod::Get, ["api", "v1", "gateway", "group-ratios"]) => {
                let conn = self.db.lock().expect("db poisoned");
                let ratios = load_all_group_ratios(&conn).unwrap_or_default();
                Ok(ok_json(to_value(&ratios)?))
            }

            // —— POST /api/v1/gateway/group-ratios —— 设置组倍率（admin）
            (HttpMethod::Post, ["api", "v1", "gateway", "group-ratios"]) => {
                let body: SetGroupRatioBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析设置组倍率请求体失败: {e}"))
                })?;
                if body.group_name.trim().is_empty() {
                    return Ok(error_response(400, "group_name 不可为空"));
                }
                if body.ratio < 0.0 {
                    return Ok(error_response(400, "ratio 不可为负"));
                }
                let ratio = GroupRatio {
                    group_name: body.group_name.clone(),
                    ratio: body.ratio,
                    updated_at: now_iso(),
                };
                {
                    let conn = self.db.lock().expect("db poisoned");
                    upsert_group_ratio(&conn, &ratio)?;
                }
                Ok(ok_json(to_value(&ratio)?))
            }

            // ============ 兑换码（RedeemCode）============

            // —— GET /api/v1/gateway/redeem-codes —— 列兑换码（admin）
            (HttpMethod::Get, ["api", "v1", "gateway", "redeem-codes"]) => {
                let conn = self.db.lock().expect("db poisoned");
                let codes = load_all_redeem_codes(&conn).unwrap_or_default();
                Ok(ok_json(to_value(&codes)?))
            }

            // —— POST /api/v1/gateway/redeem-codes —— 生成兑换码（admin）
            (HttpMethod::Post, ["api", "v1", "gateway", "redeem-codes"]) => {
                let body: CreateRedeemCodeBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析生成兑换码请求体失败: {e}"))
                })?;
                if body.quota_amount == 0 {
                    return Ok(error_response(400, "quota_amount 必须大于 0"));
                }
                let count = body.count.unwrap_or(1).clamp(1, 1000);
                let now = now_iso();
                let mut created: Vec<RedeemCode> = Vec::with_capacity(count as usize);
                {
                    let conn = self.db.lock().expect("db poisoned");
                    for _ in 0..count {
                        let code = RedeemCode {
                            code: generate_redeem_code(),
                            quota_amount: body.quota_amount,
                            used_by: None,
                            used_at: None,
                            created_at: now.clone(),
                            expires_at: None,
                        };
                        insert_redeem_code(&conn, &code)?;
                        created.push(code);
                    }
                }
                Ok(ApiResponse {
                    status: 201,
                    body: serde_json::json!({ "created": to_value(&created)?, "count": created.len() }),
                    headers: serde_json::json!({}),
                })
            }

            // —— POST /api/v1/gateway/redeem —— 兑换（任意 token 加配额）
            (HttpMethod::Post, ["api", "v1", "gateway", "redeem"]) => {
                // 1. 鉴权（用令牌本身鉴权，找到 token id 作为受益人）
                let auth = req
                    .headers
                    .get("Authorization")
                    .and_then(|v| v.as_str())
                    .or_else(|| req.headers.get("authorization").and_then(|v| v.as_str()))
                    .unwrap_or("");
                let Some(key) = extract_bearer(auth) else {
                    return Ok(error_response(401, "缺少 Authorization Bearer token"));
                };
                let body: RedeemBody = serde_json::from_value(req.body)
                    .map_err(|e| ApiGatewayError::Internal(format!("解析兑换请求体失败: {e}")))?;
                if body.code.trim().is_empty() {
                    return Ok(error_response(400, "code 不可为空"));
                }
                // 2. 短锁：查令牌 + 查码 + 标记已用 + 扣减 quota_used
                let conn = self.db.lock().expect("db poisoned");
                let token = match find_token_by_key(&conn, &key)? {
                    Some(t) => t,
                    None => return Ok(error_response(401, "无效的 API Key")),
                };
                if !token.enabled || token.status == "disabled" {
                    return Ok(error_response(401, "令牌已禁用"));
                }
                let Some(mut code) = find_redeem_code(&conn, &body.code)? else {
                    return Ok(error_response(404, "兑换码不存在"));
                };
                if code.used_by.is_some() {
                    return Ok(error_response(409, "兑换码已被使用"));
                }
                // 过期校验（expires_at 非空且已过期）
                if let Some(exp) = &code.expires_at {
                    if !exp.is_empty() && is_expired(exp) {
                        return Ok(error_response(410, "兑换码已过期"));
                    }
                }
                // 标记已用
                let now = now_iso();
                code.used_by = Some(token.id.clone());
                code.used_at = Some(now.clone());
                mark_redeem_used(&conn, &code)?;
                // 扣减 quota_used（加配额，下限 0）
                adjust_token_quota(&conn, &token.id, code.quota_amount)?;
                Ok(ok_json(serde_json::json!({
                    "ok": true,
                    "code": code.code,
                    "token_id": token.id,
                    "added_quota": code.quota_amount,
                })))
            }

            // ============ 加密货币充值（PaymentOrder）============

            // —— POST /api/v1/gateway/payments —— 创建充值订单（admin）
            (HttpMethod::Post, ["api", "v1", "gateway", "payments"]) => {
                let body: CreatePaymentBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析创建充值订单请求体失败: {e}"))
                })?;
                if body.token_id.trim().is_empty() {
                    return Ok(error_response(400, "token_id 不可为空"));
                }
                if !is_valid_currency(&body.currency) {
                    return Ok(error_response(400, "currency 必须是 usdt/btc/evm"));
                }
                if body.credits == 0 {
                    return Ok(error_response(400, "credits 必须大于 0"));
                }
                let Some(amount) = crypto_amount_for(&body.currency, body.credits) else {
                    return Ok(error_response(400, "价目换算失败（credits 溢出?）"));
                };
                // 收款地址：读 env；未配置则订单仍创建（address 空串）+ warning 提示
                let (address, warning) = pay_address_for(&body.currency);
                // memo 填面额单位提示（BTC=sat / EVM=wei），便于前端展示金额单位
                let memo = match body.currency.as_str() {
                    "btc" => Some("amount_crypto 单位为 sat（聪）".into()),
                    "evm" => Some("amount_crypto 单位为 wei".into()),
                    _ => None,
                };
                let order = PaymentOrder {
                    id: self.next_id("pay"),
                    token_id: body.token_id.clone(),
                    currency: body.currency,
                    amount_crypto: amount,
                    credits: body.credits,
                    address,
                    memo,
                    status: "pending".into(),
                    txid: None,
                    created_at: now_iso(),
                    confirmed_at: None,
                    reject_reason: None,
                    chain_block: None,
                    chain_value_wei: None,
                };
                {
                    let conn = self.db.lock().expect("db poisoned");
                    // 目标令牌必须存在（404），且积分加给有效令牌
                    if find_token(&conn, &body.token_id)?.is_none() {
                        return Ok(error_response(
                            404,
                            &format!("令牌不存在: {}", body.token_id),
                        ));
                    }
                    insert_payment_order(&conn, &order)?;
                }
                // 响应 = 订单 JSON +（可选）warning 字段（env 未配地址时提示管理员）
                let mut resp = to_value(&order)?;
                if let (Some(w), serde_json::Value::Object(map)) = (warning, &mut resp) {
                    map.insert("warning".into(), serde_json::Value::String(w));
                }
                Ok(ApiResponse {
                    status: 201,
                    body: resp,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/gateway/payments —— 列充值订单（?status= 过滤）
            (HttpMethod::Get, ["api", "v1", "gateway", "payments"]) => {
                let status = parse_query_str(query, "status");
                let conn = self.db.lock().expect("db poisoned");
                let orders = load_payment_orders(&conn, status.as_deref())?;
                Ok(ok_json(to_value(&orders)?))
            }

            // —— POST /api/v1/gateway/payments/:id/confirm —— 确认到账（admin，
            //    给目标 token 加积分；幂等：仅 pending 可确认，重复/已拒 → 409）
            //    dApp 一期（2026-08-31）：evm 订单带 txid 时先做**链上核验**
            //    （复用 NexHub 同一编排 check_chain_payment）：核对收款地址=
            //    订单 address（env NEXOS_PAY_EVM_ADDR）、金额=订单 amount_crypto
            //    （wei 整数串；**18 位小数假设**——价目 PRICE_WEI_PER_CREDIT 已按
            //    wei 计，非 18 位链不适用，见 docs 限制）。核验不过 → 不确认
            //    （Mismatch/NotFound/Pending 409/400）；RPC 故障 → 降级放行 +
            //    warning（admin 仍在环内兜底）。usdt/btc 或未带 txid → admin 链下
            //    手动确认（unverified 标注）。锁外 await（rusqlite 守卫非 Send）。
            //    dApp 二期（2026-09-02）：①usdt 订单定位到 EVM 链（body/env
            //    chain_id）且有 ERC-20 合约（body erc20_contract → env
            //    NEXOS_USDT_EVM_CONTRACT）时走 **Transfer 日志核验**（amount_crypto
            //    "10.00" 按 decimals=6 换算成微单位）；TRON 形态（无 EVM 链 ID）
            //    或缺合约仍人工。②金额规则 **AtLeast**（≥应付额即过，多打不亏待
            //    用户；Verified 的 value_wei/链上实付落库可审计）。
            (HttpMethod::Post, ["api", "v1", "gateway", "payments", id, "confirm"]) => {
                let body: ConfirmPaymentBody = serde_json::from_value(req.body).unwrap_or_default(); // body 可为 null（txid 可选）
                                                                                                     // 查单（短锁快放——锁不能跨下面的核验 .await 持有）
                let found = {
                    let conn = self.db.lock().expect("db poisoned");
                    find_payment_order(&conn, id)?
                };
                let Some(mut order) = found else {
                    return Ok(error_response(404, &format!("订单不存在: {id}")));
                };
                if order.status != "pending" {
                    return Ok(error_response(
                        409,
                        &format!("订单状态为 {}，仅 pending 可确认", order.status),
                    ));
                }
                let txid = body.txid.filter(|s| !s.trim().is_empty());
                // 链上核验（无 txid → Unverified「admin 手动确认」；开关关闭 → Skipped）
                let check = check_chain_payment(
                    &self.chain_verify,
                    &order.currency,
                    txid.as_deref().unwrap_or_default(),
                    &order.amount_crypto,
                    &ChainPayHints {
                        chain_id: body.chain_id,
                        chain_str: None,
                        rpc_url: body.rpc_url.as_deref(),
                        pay_to: Some(order.address.as_str()),
                        // 订单自带收款地址（NEXOS_PAY_EVM_ADDR），不回落节点缺省
                        fallback_default_pay_to: false,
                        // 二期定稿：充值 AtLeast——多打照常入账订单积分，不足才拦
                        amount_rule: os_nexhub::chain_verify::AmountRule::AtLeast,
                        erc20_contract: body.erc20_contract.as_deref(),
                        erc20_decimals: body.erc20_decimals,
                    },
                )
                .await;
                if let ChainPayCheck::Denied { status, reason } = &check {
                    eprintln!("[gateway] 订单 {id} 链上核验拒绝，未确认：{reason}");
                    return Ok(error_response(*status, reason));
                }
                if let ChainPayCheck::Verified {
                    block_number,
                    value_wei,
                    ..
                } = &check
                {
                    order.chain_block = Some(*block_number);
                    order.chain_value_wei = Some(value_wei.clone());
                }
                // 确认：status + confirmed_at + 可选 txid，并给目标令牌加积分
                {
                    let conn = self.db.lock().expect("db poisoned");
                    let now = now_iso();
                    order.status = "confirmed".into();
                    order.confirmed_at = Some(now);
                    if let Some(txid) = txid {
                        order.txid = Some(txid);
                    }
                    insert_payment_order(&conn, &order)?;
                    add_token_credits(&conn, &order.token_id, order.credits)?;
                }
                let mut resp = serde_json::json!({
                    "ok": true,
                    "order": to_value(&order)?,
                    "added_credits": order.credits,
                });
                if let Some(marker) = chain_verify_json(&check) {
                    if let Some(map) = resp.as_object_mut() {
                        map.insert("chain_verify".into(), marker);
                    }
                }
                Ok(ok_json(resp))
            }

            // —— POST /api/v1/gateway/payments/:id/reject —— 拒绝订单（admin，记原因；
            //    仅 pending 可拒，重复/已确认 → 409，不加积分）
            (HttpMethod::Post, ["api", "v1", "gateway", "payments", id, "reject"]) => {
                let body: RejectPaymentBody = serde_json::from_value(req.body).unwrap_or_default(); // body 可为 null（reason 可选）
                let conn = self.db.lock().expect("db poisoned");
                let Some(mut order) = find_payment_order(&conn, id)? else {
                    return Ok(error_response(404, &format!("订单不存在: {id}")));
                };
                if order.status != "pending" {
                    return Ok(error_response(
                        409,
                        &format!("订单状态为 {}，仅 pending 可拒绝", order.status),
                    ));
                }
                order.status = "rejected".into();
                order.reject_reason = Some(
                    body.reason
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "admin 未填原因".into()),
                );
                insert_payment_order(&conn, &order)?;
                Ok(ok_json(to_value(&order)?))
            }

            // ============ 代理转发 ============

            // —— POST /api/v1/gateway/v1/chat/completions —— OpenAI 兼容代理转发
            (HttpMethod::Post, ["api", "v1", "gateway", "v1", "chat", "completions"]) => {
                self.proxy_forward(req, "chat/completions").await
            }

            // —— POST /api/v1/gateway/v1/completions —— completions 代理转发
            (HttpMethod::Post, ["api", "v1", "gateway", "v1", "completions"]) => {
                self.proxy_forward(req, "completions").await
            }

            // —— GET /api/v1/gateway/v1/models —— OpenAI 形态模型列表（对外接入，
            //    同一 Bearer sk-os- 令牌鉴权；见 routes() 同名条目注释）
            (HttpMethod::Get, ["api", "v1", "gateway", "v1", "models"]) => {
                self.openai_models_list(req).await
            }

            // —— 未覆盖路由 —— 兜底 404
            _ => Ok(error_response(404, "api_gateway: 未匹配的路由")),
        }
    }
}

impl ApiGatewayRouteHandler {
    /// OpenAI 兼容的 models 列表（`GET /api/v1/gateway/v1/models`）。
    ///
    /// 让 OpenAI SDK / 各类客户端"零改造"接入：`client.models.list()` 直接可用。
    /// 鉴权与转发同源（Bearer sk-os- 令牌：查表 + 启用/过期校验；**不查配额**——
    /// 列模型不产生消费，429 闸门只作用于真实调用）。
    ///
    /// 聚合口径（与实际可路由集合一致，不虚列）：
    /// - 启用渠道（再限定令牌 `allowed_channels`，空=全部）的 `models` 字段；
    /// - 映射 `public_name`（仅映射目标渠道仍启用且在令牌 allowed_channels 内）；
    /// - 去重后按令牌 `allowed_models` 过滤（空=不过滤），按 id 排序稳定输出。
    ///
    /// 响应形状（OpenAI list 契约）：`{object:"list",data:[{id,object:"model",
    /// created,owned_by:"nexos-gateway"}]}`。`created` 用固定协议占位常量
    /// [`MODELS_LIST_CREATED_TS`]（该字段在 OpenAI 侧也是无业务语义的占位）。
    async fn openai_models_list(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        // 1. Bearer 鉴权（查令牌 + 启用/过期校验——与 resolve_forward_plan 前半同款）
        let auth = req
            .headers
            .get("Authorization")
            .and_then(|v| v.as_str())
            .or_else(|| req.headers.get("authorization").and_then(|v| v.as_str()))
            .unwrap_or("");
        let Some(key) = extract_bearer(auth) else {
            return Ok(error_response(401, "缺少 Authorization Bearer token"));
        };
        let token = {
            let conn = self.db.lock().expect("db poisoned");
            find_token_by_key(&conn, &key).unwrap_or(None)
        };
        let Some(token) = token else {
            return Ok(error_response(401, "无效的 API Key"));
        };
        if !token.enabled || token.status == "disabled" {
            return Ok(error_response(401, "令牌已禁用"));
        }
        if token.status == "expired" {
            return Ok(error_response(401, "令牌已过期"));
        }
        if let Some(exp) = &token.expires_at {
            if !exp.is_empty() && is_expired(exp) {
                return Ok(error_response(401, "令牌已过期"));
            }
        }
        // 2. 聚合：渠道 models + 映射 public_name（短锁 DB 快查）
        let allowed_channel = |c: &Channel| {
            token.allowed_channels.is_empty() || token.allowed_channels.iter().any(|id| id == &c.id)
        };
        let (channels_snap, mapping_snap) = {
            let conn = self.db.lock().expect("db poisoned");
            let chs = load_all_channels(&conn).unwrap_or_default();
            let maps = load_all_mappings(&conn).unwrap_or_default();
            (chs, maps)
        };
        let mut ids: Vec<String> = Vec::new();
        for c in channels_snap
            .iter()
            .filter(|c| c.enabled && allowed_channel(c))
        {
            ids.extend(c.models.iter().cloned());
        }
        for m in &mapping_snap {
            // 映射目标渠道必须仍启用且在令牌 allowed_channels 内，否则不虚列
            let target_ok = channels_snap
                .iter()
                .any(|c| c.id == m.channel_id && c.enabled && allowed_channel(c));
            if target_ok {
                ids.push(m.public_name.clone());
            }
        }
        // 3. 去重 → allowed_models 过滤（空=不过滤）→ 排序
        ids.sort();
        ids.dedup();
        if !token.allowed_models.is_empty() {
            ids.retain(|id| token.allowed_models.iter().any(|m| m == id));
        }
        let data: Vec<serde_json::Value> = ids
            .into_iter()
            .map(|id| {
                serde_json::json!({
                    "id": id,
                    "object": "model",
                    "created": MODELS_LIST_CREATED_TS,
                    "owned_by": "nexos-gateway",
                })
            })
            .collect();
        Ok(ok_json(serde_json::json!({
            "object": "list",
            "data": data,
        })))
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
        handler_component: "api_gateway".to_string(),
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

/// [`error_response`] 的跨模块复用形（http.rs SSE 特挂路由的 401/502 等
/// 错误响应与 api_gateway 组件内错误同形，保持客户端可见契约一致）。
pub(crate) fn gateway_error_response(status: u16, msg: &str) -> ApiResponse {
    error_response(status, msg)
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

/// provider 白名单校验。
fn is_valid_provider(p: &str) -> bool {
    matches!(
        p,
        "openai" | "deepseek" | "anthropic" | "local-vllm" | "azure" | "ollama"
    )
}

/// 从 query string 解析整型参数，缺省返回 default。
fn parse_query_int(query: &str, key: &str, default: usize) -> usize {
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut it = pair.splitn(2, '=');
        if it.next() == Some(key) {
            if let Some(v) = it.next() {
                if let Ok(n) = v.parse::<usize>() {
                    return n;
                }
            }
        }
    }
    default
}

/// 从 query string 解析字符串参数（如 `?status=pending`），缺失返回 None。
fn parse_query_str(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut it = pair.splitn(2, '=');
        if it.next() == Some(key) {
            if let Some(v) = it.next() {
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// 解析上游响应的 usage（prompt_tokens/completion_tokens/total_tokens）。
fn parse_usage(stdout: &[u8]) -> Option<(u32, u32, u32)> {
    let v: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    let u = v.get("usage")?;
    let pt = u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
    let ct = u
        .get("completion_tokens")
        .and_then(|x| x.as_u64())
        .unwrap_or(0) as u32;
    let tt = u
        .get("total_tokens")
        .and_then(|x| x.as_u64())
        .unwrap_or((pt + ct) as u64) as u32;
    Some((pt, ct, tt))
}

/// 流式转发 usage 未上报时的日志备注文案（写进 CallLog.error，status 仍 success）。
pub const STREAM_USAGE_MISSING_NOTE: &str = "上游未上报 usage（流式计费记 0）";

/// 从 SSE 文本中解析 usage 三元组（流式转发计费用，真实数据铁律）。
///
/// OpenAI 语义：`stream_options.include_usage=true` 时上游在**最后一个**带
/// `usage` 字段的 `data:` 块里下发用量（其后是 `data: [DONE]`）。解析规则：
/// - 逐行扫 `data:` 前缀行，JSON 解析失败的行（含 `[DONE]`、注释、半截行）跳过；
/// - 取**最后一个**含 `usage` 对象的块（同块可能既有增量又有 usage）；
/// - `total_tokens` 缺省时按 pt+ct 求和（与 [`parse_usage`] 同口径）；
/// - 全程无 usage 块 → `None`——调用方记 0 并在日志 error 字段注明
///   [`STREAM_USAGE_MISSING_NOTE`]，**禁止估算编造**。
#[must_use]
pub fn parse_stream_usage(sse_text: &str) -> Option<(u32, u32, u32)> {
    let mut last: Option<(u32, u32, u32)> = None;
    for line in sse_text.lines() {
        let line = line.trim();
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
            continue;
        };
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            let pt = u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
            let ct = u
                .get("completion_tokens")
                .and_then(|x| x.as_u64())
                .unwrap_or(0) as u32;
            let tt = u
                .get("total_tokens")
                .and_then(|x| x.as_u64())
                .unwrap_or((pt + ct) as u64) as u32;
            last = Some((pt, ct, tt));
        }
    }
    last
}

/// 若请求体含 model 字段且映射覆盖了它，把 payload 字符串里的 model 修正。
///
/// 这里 payload 已是字符串，简单做：直接重新序列化（forward_upstream 调用前已修改 body），
/// 本函数仅做透传（修正逻辑在 caller）。
fn fixup_model_in_payload(payload: &str, _body: &serde_json::Value) -> String {
    payload.to_string()
}

/// 简易过期判断：只支持 ISO 8601 字符串字典序比较（同格式下成立）。
fn is_expired(exp: &str) -> bool {
    let now = now_iso();
    // 字典序比较：若 exp <= now 视为过期（要求两者同格式同长度才严格正确，
    // 这里宽松判断，前端应传 RFC3339）
    exp.trim() <= now.as_str()
}

// ----------------------------------------------------------------------------
// SQLite 持久化层
// ----------------------------------------------------------------------------

/// 默认 DB 路径：优先 /tank/os-data/gateway.db，再 /var/lib/os/gateway.db，
/// 最后 ./gateway.db（保底）。
fn default_db_path() -> String {
    for p in &["/tank/os-data/gateway.db", "/var/lib/os/gateway.db"] {
        if std::path::Path::new(p)
            .parent()
            .is_some_and(|d| d.exists() || std::fs::create_dir_all(d).is_ok())
        {
            return (*p).to_string();
        }
    }
    "./gateway.db".to_string()
}

/// 打开 SQLite 文件，建表，seed 默认倍率（demo 渠道/令牌/日志仅测试填充）。
fn open_db(path: &str) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    // WAL 提升并发读写（文件库场景），失败不致命（忽略）。
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    create_schema(&conn)?;
    // 生产空表起步（demo_* 在 cfg(not(test)) 下为空——2026-08-30 真实化：
    // 用户报告「删除的不是真实的数据」，首次空表 seed 的假数据全撤）；
    // 默认计费倍率照常 seed（INSERT OR IGNORE，配置默认值非假数据）。
    seed_if_empty(&conn)?;
    Ok(conn)
}

/// 建表（IF NOT EXISTS）+ logs.created_at 索引 + 计费倍率三张表。
fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS channels (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            provider TEXT NOT NULL,
            base_url TEXT NOT NULL,
            api_key TEXT NOT NULL,
            models TEXT NOT NULL,
            priority INTEGER NOT NULL,
            weight INTEGER NOT NULL,
            status TEXT NOT NULL,
            enabled INTEGER NOT NULL,
            created_at TEXT NOT NULL,
            last_used TEXT,
            request_count INTEGER NOT NULL,
            via_node TEXT NOT NULL DEFAULT ''
        );
        CREATE TABLE IF NOT EXISTS tokens (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            key TEXT NOT NULL,
            status TEXT NOT NULL,
            enabled INTEGER NOT NULL,
            quota_limit INTEGER NOT NULL,
            quota_used INTEGER NOT NULL,
            allowed_models TEXT NOT NULL,
            allowed_channels TEXT NOT NULL,
            group_name TEXT NOT NULL DEFAULT 'default',
            expires_at TEXT,
            created_at TEXT NOT NULL,
            last_used TEXT,
            request_count INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS logs (
            id TEXT PRIMARY KEY,
            token_id TEXT NOT NULL,
            token_name TEXT NOT NULL,
            channel_id TEXT NOT NULL,
            channel_name TEXT NOT NULL,
            model TEXT NOT NULL,
            prompt_tokens INTEGER NOT NULL,
            completion_tokens INTEGER NOT NULL,
            total_tokens INTEGER NOT NULL,
            latency_ms INTEGER NOT NULL,
            status TEXT NOT NULL,
            error TEXT,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS mappings (
            public_name TEXT PRIMARY KEY,
            channel_id TEXT NOT NULL,
            upstream_model TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS model_ratios (
            model TEXT PRIMARY KEY,
            ratio REAL NOT NULL DEFAULT 1,
            updated_at TEXT
        );
        CREATE TABLE IF NOT EXISTS group_ratios (
            group_name TEXT PRIMARY KEY,
            ratio REAL NOT NULL DEFAULT 1,
            updated_at TEXT
        );
        CREATE TABLE IF NOT EXISTS redeem_codes (
            code TEXT PRIMARY KEY,
            quota_amount INTEGER NOT NULL,
            used_by TEXT,
            used_at TEXT,
            created_at TEXT,
            expires_at TEXT
        );
        CREATE TABLE IF NOT EXISTS payment_orders (
            id TEXT PRIMARY KEY,
            token_id TEXT NOT NULL,
            currency TEXT NOT NULL,
            amount_crypto TEXT NOT NULL,
            credits INTEGER NOT NULL,
            address TEXT NOT NULL DEFAULT '',
            memo TEXT,
            status TEXT NOT NULL DEFAULT 'pending',
            txid TEXT,
            created_at TEXT NOT NULL,
            confirmed_at TEXT,
            reject_reason TEXT,
            chain_block INTEGER,
            chain_value_wei TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_logs_created_at ON logs(created_at);
        ",
    )?;
    // 老库迁移：tokens 表可能缺 group_name / billing_mode 列
    //（CREATE TABLE IF NOT EXISTS 不补列）
    migrate_add_group_name(conn)?;
    migrate_add_billing_mode(conn)?;
    // 老库迁移（dApp 一期）：payment_orders 补链上核验事实两列（存量行 NULL=未核验）
    migrate_add_payment_chain_facts(conn)?;
    // 老库迁移（2026-09-03 渠道中继）：channels 补 via_node 列（存量行 ''=直连）
    migrate_add_channel_via_node(conn)?;
    // 老库迁移（2026-09-06 FilmHub 记账）：channels 补三单价列（存量行 0=未配置）
    migrate_add_channel_prices(conn)?;
    Ok(())
}

/// 老库迁移（2026-09-06 FilmHub 成本记账）：`channels` 缺三单价列则补
/// （PRAGMA table_info 探测幂等；存量行 0 = 未配置——只计量不计价）。
fn migrate_add_channel_prices(conn: &Connection) -> rusqlite::Result<()> {
    let mut existing: Vec<String> = Vec::new();
    {
        let mut stmt = conn.prepare("PRAGMA table_info(channels)")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
        for r in rows {
            existing.push(r?);
        }
    }
    for col in [
        ("price_per_call", "REAL NOT NULL DEFAULT 0"),
        ("price_per_sec", "REAL NOT NULL DEFAULT 0"),
        ("price_per_token", "REAL NOT NULL DEFAULT 0"),
    ] {
        if !existing.iter().any(|c| c == col.0) {
            conn.execute(
                &format!("ALTER TABLE channels ADD COLUMN {} {}", col.0, col.1),
                [],
            )?;
        }
    }
    Ok(())
}

/// 老库迁移（2026-09-03 渠道中继）：`channels` 缺 `via_node` 列则补
/// （`PRAGMA table_info` 探测幂等；存量行 `''` = 直连语义，行为与迁移前不变）。
fn migrate_add_channel_via_node(conn: &Connection) -> rusqlite::Result<()> {
    let mut existing: Vec<String> = Vec::new();
    {
        let mut stmt = conn.prepare("PRAGMA table_info(channels)")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
        for r in rows {
            existing.push(r?);
        }
    }
    if !existing.iter().any(|c| c == "via_node") {
        conn.execute(
            "ALTER TABLE channels ADD COLUMN via_node TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    Ok(())
}

/// 老库迁移（dApp 一期，2026-08-31）：`payment_orders` 缺 `chain_block` /
/// `chain_value_wei` 列则补（`PRAGMA table_info` 探测幂等；存量行 NULL =
/// 未核验的历史订单，语义见 [`PaymentOrder`]）。
fn migrate_add_payment_chain_facts(conn: &Connection) -> rusqlite::Result<()> {
    let mut existing: Vec<String> = Vec::new();
    {
        let mut stmt = conn.prepare("PRAGMA table_info(payment_orders)")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
        for r in rows {
            existing.push(r?);
        }
    }
    for (col, ddl) in [("chain_block", "INTEGER"), ("chain_value_wei", "TEXT")] {
        if !existing.iter().any(|c| c == col) {
            conn.execute(
                &format!("ALTER TABLE payment_orders ADD COLUMN {col} {ddl}"),
                [],
            )?;
        }
    }
    Ok(())
}

/// 老库迁移：若 tokens 表无 group_name 列则补上（DEFAULT 'default'）。
fn migrate_add_group_name(conn: &Connection) -> rusqlite::Result<()> {
    let has_col: bool = {
        let mut stmt = conn.prepare("PRAGMA table_info(tokens)")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
        for r in rows {
            if r? == "group_name" {
                return Ok(());
            }
        }
        false
    };
    if !has_col {
        conn.execute(
            "ALTER TABLE tokens ADD COLUMN group_name TEXT NOT NULL DEFAULT 'default'",
            [],
        )?;
    }
    Ok(())
}

/// 老库迁移：若 tokens 表无 billing_mode 列则补上（DEFAULT 'per_token'）。
///
/// 兼容语义：历史持久化的令牌一律回落 per_token（与 ApiToken 的 serde default
/// 一致），行为与迁移前完全不变。
fn migrate_add_billing_mode(conn: &Connection) -> rusqlite::Result<()> {
    let has_col: bool = {
        let mut stmt = conn.prepare("PRAGMA table_info(tokens)")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
        for r in rows {
            if r? == "billing_mode" {
                return Ok(());
            }
        }
        false
    };
    if !has_col {
        conn.execute(
            "ALTER TABLE tokens ADD COLUMN billing_mode TEXT NOT NULL DEFAULT 'per_token'",
            [],
        )?;
    }
    Ok(())
}

/// 首次空表时 seed（demo 渠道/令牌/日志**仅 cfg(test) 填充**，生产空表起步；
/// 默认倍率恒 seed——INSERT OR IGNORE 幂等，不覆盖已配置值）。已存在数据则跳过。
fn seed_if_empty(conn: &Connection) -> rusqlite::Result<()> {
    let channel_count: i64 = conn.query_row("SELECT COUNT(*) FROM channels", [], |r| r.get(0))?;
    if channel_count == 0 {
        for c in demo_channels() {
            insert_channel(conn, &c)?;
        }
    }
    let token_count: i64 = conn.query_row("SELECT COUNT(*) FROM tokens", [], |r| r.get(0))?;
    if token_count == 0 {
        for t in demo_tokens() {
            insert_token(conn, &t)?;
        }
    }
    let log_count: i64 = conn.query_row("SELECT COUNT(*) FROM logs", [], |r| r.get(0))?;
    if log_count == 0 {
        for l in demo_logs() {
            insert_log(conn, &l)?;
        }
    }
    // 默认计费倍率（INSERT OR IGNORE：不覆盖已配置值）
    seed_default_ratios(conn)?;
    Ok(())
}

/// seed 默认模型倍率 + 用户组倍率（INSERT OR IGNORE，幂等）。
fn seed_default_ratios(conn: &Connection) -> rusqlite::Result<()> {
    let now = now_iso();
    for (model, ratio) in &[
        ("gpt-4o", 15.0_f64),
        ("gpt-4o-mini", 0.75),
        ("gpt-4", 15.0),
        ("gpt-3.5-turbo", 0.75),
        ("claude-3.5-sonnet", 15.0),
        ("qwen3-vl-8b", 0.5),
        ("qwen2.5-7b", 0.5),
        ("deepseek-chat", 0.5),
        ("deepseek-coder", 0.5),
    ] {
        conn.execute(
            "INSERT OR IGNORE INTO model_ratios (model, ratio, updated_at) VALUES (?,?,?)",
            params![model, ratio, now],
        )?;
    }
    for (group, ratio) in &[("default", 1.0_f64), ("vip", 0.8), ("trial", 2.0)] {
        conn.execute(
            "INSERT OR IGNORE INTO group_ratios (group_name, ratio, updated_at) VALUES (?,?,?)",
            params![group, ratio, now],
        )?;
    }
    Ok(())
}

// ---- channels CRUD ----

/// `channels` 列清单（INSERT/SELECT 共用；2026-09-03 增 via_node）。
const CHANNEL_COLUMNS: &str = "id,name,provider,base_url,api_key,models,priority,weight,status,enabled,created_at,last_used,request_count,via_node,price_per_call,price_per_sec,price_per_token";

fn insert_channel(conn: &Connection, c: &Channel) -> rusqlite::Result<()> {
    conn.execute(
        &format!(
            "INSERT OR REPLACE INTO channels
             ({CHANNEL_COLUMNS})
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)"
        ),
        params![
            c.id,
            c.name,
            c.provider,
            c.base_url,
            c.api_key,
            serde_json::to_string(&c.models).unwrap_or_else(|_| "[]".into()),
            c.priority as i64,
            c.weight as i64,
            c.status,
            c.enabled as i64,
            c.created_at,
            c.last_used.as_deref(),
            c.request_count as i64,
            c.via_node,
            c.price_per_call,
            c.price_per_sec,
            c.price_per_token,
        ],
    )?;
    Ok(())
}

fn update_channel(conn: &Connection, c: &Channel) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE channels SET
            name=?, provider=?, base_url=?, api_key=?, models=?,
            priority=?, weight=?, status=?, enabled=?, created_at=?,
            last_used=?, request_count=?, via_node=?,
            price_per_call=?, price_per_sec=?, price_per_token=?
         WHERE id=?",
        params![
            c.name,
            c.provider,
            c.base_url,
            c.api_key,
            serde_json::to_string(&c.models).unwrap_or_else(|_| "[]".into()),
            c.priority as i64,
            c.weight as i64,
            c.status,
            c.enabled as i64,
            c.created_at,
            c.last_used.as_deref(),
            c.request_count as i64,
            c.via_node,
            c.price_per_call,
            c.price_per_sec,
            c.price_per_token,
            c.id,
        ],
    )?;
    Ok(())
}

fn update_channel_status(conn: &Connection, id: &str, status: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE channels SET status=? WHERE id=?",
        params![status, id],
    )?;
    Ok(())
}

/// 渠道计数 +1 + last_used（记录成功转发）。
fn bump_channel(conn: &Connection, id: &str, now: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE channels SET request_count = request_count + 1, last_used=? WHERE id=?",
        params![now, id],
    )?;
    Ok(())
}

fn delete_channel(conn: &Connection, id: &str) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM channels WHERE id=?", params![id])
}

fn find_channel(conn: &Connection, id: &str) -> rusqlite::Result<Option<Channel>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {CHANNEL_COLUMNS}
         FROM channels WHERE id=?"
    ))?;
    stmt.query_row(params![id], channel_from_row).optional()
}

fn load_all_channels(conn: &Connection) -> rusqlite::Result<Vec<Channel>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {CHANNEL_COLUMNS}
         FROM channels ORDER BY created_at"
    ))?;
    let iter = stmt.query_map([], channel_from_row)?;
    let mut out = Vec::new();
    for c in iter {
        out.push(c?);
    }
    Ok(out)
}

fn channel_from_row(row: &rusqlite::Row) -> rusqlite::Result<Channel> {
    let models_json: String = row.get(5)?;
    let models: Vec<String> = serde_json::from_str(&models_json).unwrap_or_default();
    let last_used: Option<String> = row.get(11)?;
    Ok(Channel {
        id: row.get(0)?,
        name: row.get(1)?,
        provider: row.get(2)?,
        base_url: row.get(3)?,
        api_key: row.get(4)?,
        models,
        priority: row.get::<_, i64>(6)? as u32,
        weight: row.get::<_, i64>(7)? as u32,
        status: row.get(8)?,
        enabled: row.get::<_, i64>(9)? != 0,
        created_at: row.get(10)?,
        last_used,
        request_count: row.get::<_, i64>(12)? as u64,
        // 老库行缺列迁移后为 ''；缺列容错回落空串（直连语义）
        via_node: row
            .get::<_, Option<String>>(13)?
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_default(),
        // film 三单价（老库缺列迁移后 DEFAULT 0）
        price_per_call: row.get::<_, Option<f64>>(14)?.unwrap_or(0.0),
        price_per_sec: row.get::<_, Option<f64>>(15)?.unwrap_or(0.0),
        price_per_token: row.get::<_, Option<f64>>(16)?.unwrap_or(0.0),
    })
}

// ---- tokens CRUD ----

fn insert_token(conn: &Connection, t: &ApiToken) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO tokens
         (id,name,key,status,enabled,quota_limit,quota_used,allowed_models,allowed_channels,group_name,billing_mode,expires_at,created_at,last_used,request_count)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        params![
            t.id, t.name, t.key, t.status, t.enabled as i64,
            t.quota_limit as i64, t.quota_used as i64,
            serde_json::to_string(&t.allowed_models).unwrap_or_else(|_| "[]".into()),
            serde_json::to_string(&t.allowed_channels).unwrap_or_else(|_| "[]".into()),
            t.group_name,
            t.billing_mode,
            t.expires_at.as_deref(),
            t.created_at, t.last_used.as_deref(), t.request_count as i64,
        ],
    )?;
    Ok(())
}

fn update_token(conn: &Connection, t: &ApiToken) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE tokens SET
            name=?, key=?, status=?, enabled=?, quota_limit=?, quota_used=?,
            allowed_models=?, allowed_channels=?, group_name=?, billing_mode=?, expires_at=?, created_at=?,
            last_used=?, request_count=?
         WHERE id=?",
        params![
            t.name, t.key, t.status, t.enabled as i64,
            t.quota_limit as i64, t.quota_used as i64,
            serde_json::to_string(&t.allowed_models).unwrap_or_else(|_| "[]".into()),
            serde_json::to_string(&t.allowed_channels).unwrap_or_else(|_| "[]".into()),
            t.group_name,
            t.billing_mode,
            t.expires_at.as_deref(),
            t.created_at, t.last_used.as_deref(), t.request_count as i64,
            t.id,
        ],
    )?;
    Ok(())
}

/// 令牌配额扣减 + 请求计数 + last_used（记录成功转发）。
fn bump_token(conn: &Connection, id: &str, tokens_used: u64, now: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE tokens SET quota_used = quota_used + ?, request_count = request_count + 1, last_used=?
         WHERE id=?",
        params![tokens_used as i64, now, id],
    )?;
    Ok(())
}

fn delete_token(conn: &Connection, id: &str) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM tokens WHERE id=?", params![id])
}

fn find_token(conn: &Connection, id: &str) -> rusqlite::Result<Option<ApiToken>> {
    let mut stmt = conn.prepare(
        "SELECT id,name,key,status,enabled,quota_limit,quota_used,allowed_models,allowed_channels,group_name,billing_mode,expires_at,created_at,last_used,request_count
         FROM tokens WHERE id=?",
    )?;
    stmt.query_row(params![id], token_from_row).optional()
}

fn find_token_by_key(conn: &Connection, key: &str) -> rusqlite::Result<Option<ApiToken>> {
    let mut stmt = conn.prepare(
        "SELECT id,name,key,status,enabled,quota_limit,quota_used,allowed_models,allowed_channels,group_name,billing_mode,expires_at,created_at,last_used,request_count
         FROM tokens WHERE key=?",
    )?;
    stmt.query_row(params![key], token_from_row).optional()
}

fn load_all_tokens(conn: &Connection) -> rusqlite::Result<Vec<ApiToken>> {
    let mut stmt = conn.prepare(
        "SELECT id,name,key,status,enabled,quota_limit,quota_used,allowed_models,allowed_channels,group_name,billing_mode,expires_at,created_at,last_used,request_count
         FROM tokens ORDER BY created_at",
    )?;
    let iter = stmt.query_map([], token_from_row)?;
    let mut out = Vec::new();
    for t in iter {
        out.push(t?);
    }
    Ok(out)
}

fn token_from_row(row: &rusqlite::Row) -> rusqlite::Result<ApiToken> {
    let allowed_models_json: String = row.get(7)?;
    let allowed_models: Vec<String> =
        serde_json::from_str(&allowed_models_json).unwrap_or_default();
    let allowed_channels_json: String = row.get(8)?;
    let allowed_channels: Vec<String> =
        serde_json::from_str(&allowed_channels_json).unwrap_or_default();
    let group_name: String = row.get(9).unwrap_or_else(|_| "default".into());
    // billing_mode：老库行可能读到 NULL/非法值（迁移兜底）→ 回落 per_token
    let billing_mode: String = row
        .get::<_, Option<String>>(10)
        .ok()
        .flatten()
        .filter(|m| is_valid_billing_mode(m))
        .unwrap_or_else(default_billing_mode);
    let expires_at: Option<String> = row.get(11)?;
    let last_used: Option<String> = row.get(13)?;
    Ok(ApiToken {
        id: row.get(0)?,
        name: row.get(1)?,
        key: row.get(2)?,
        status: row.get(3)?,
        enabled: row.get::<_, i64>(4)? != 0,
        quota_limit: row.get::<_, i64>(5)? as u64,
        quota_used: row.get::<_, i64>(6)? as u64,
        allowed_models,
        allowed_channels,
        group_name,
        billing_mode,
        expires_at: expires_at.filter(|s| !s.is_empty()),
        created_at: row.get(12)?,
        last_used,
        request_count: row.get::<_, i64>(14)? as u64,
    })
}

// ---- logs CRUD ----

fn insert_log(conn: &Connection, l: &CallLog) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO logs
         (id,token_id,token_name,channel_id,channel_name,model,prompt_tokens,completion_tokens,total_tokens,latency_ms,status,error,created_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
        params![
            l.id, l.token_id, l.token_name, l.channel_id, l.channel_name, l.model,
            l.prompt_tokens as i64, l.completion_tokens as i64, l.total_tokens as i64,
            l.latency_ms as i64, l.status, l.error.as_deref(), l.created_at,
        ],
    )?;
    Ok(())
}

/// 分页查日志：最新在前（ORDER BY created_at DESC LIMIT ?），走 created_at 索引。
fn load_logs(conn: &Connection, limit: usize) -> rusqlite::Result<Vec<CallLog>> {
    let mut stmt = conn.prepare(
        "SELECT id,token_id,token_name,channel_id,channel_name,model,prompt_tokens,completion_tokens,total_tokens,latency_ms,status,error,created_at
         FROM logs ORDER BY created_at DESC LIMIT ?",
    )?;
    let iter = stmt.query_map(params![limit as i64], log_from_row)?;
    let mut out = Vec::new();
    for l in iter {
        out.push(l?);
    }
    Ok(out)
}

fn load_all_logs(conn: &Connection) -> rusqlite::Result<Vec<CallLog>> {
    let mut stmt = conn.prepare(
        "SELECT id,token_id,token_name,channel_id,channel_name,model,prompt_tokens,completion_tokens,total_tokens,latency_ms,status,error,created_at
         FROM logs ORDER BY created_at",
    )?;
    let iter = stmt.query_map([], log_from_row)?;
    let mut out = Vec::new();
    for l in iter {
        out.push(l?);
    }
    Ok(out)
}

fn log_from_row(row: &rusqlite::Row) -> rusqlite::Result<CallLog> {
    let error: Option<String> = row.get(11)?;
    Ok(CallLog {
        id: row.get(0)?,
        token_id: row.get(1)?,
        token_name: row.get(2)?,
        channel_id: row.get(3)?,
        channel_name: row.get(4)?,
        model: row.get(5)?,
        prompt_tokens: row.get::<_, i64>(6)? as u32,
        completion_tokens: row.get::<_, i64>(7)? as u32,
        total_tokens: row.get::<_, i64>(8)? as u32,
        latency_ms: row.get::<_, i64>(9)? as u64,
        status: row.get(10)?,
        error,
        created_at: row.get(12)?,
    })
}

// ---- mappings CRUD ----

fn upsert_mapping(conn: &Connection, m: &ModelMapping) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO mappings (public_name,channel_id,upstream_model) VALUES (?,?,?)",
        params![m.public_name, m.channel_id, m.upstream_model],
    )?;
    Ok(())
}

fn delete_mapping(conn: &Connection, name: &str) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM mappings WHERE public_name=?", params![name])
}

fn load_all_mappings(conn: &Connection) -> rusqlite::Result<Vec<ModelMapping>> {
    let mut stmt = conn.prepare(
        "SELECT public_name,channel_id,upstream_model FROM mappings ORDER BY public_name",
    )?;
    let iter = stmt.query_map([], |row| {
        Ok(ModelMapping {
            public_name: row.get(0)?,
            channel_id: row.get(1)?,
            upstream_model: row.get(2)?,
        })
    })?;
    let mut out = Vec::new();
    for m in iter {
        out.push(m?);
    }
    Ok(out)
}

// ---- model_ratios CRUD ----

fn upsert_model_ratio(conn: &Connection, r: &ModelRatio) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO model_ratios (model, ratio, updated_at) VALUES (?,?,?)",
        params![r.model, r.ratio, r.updated_at],
    )?;
    Ok(())
}

fn delete_model_ratio(conn: &Connection, model: &str) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM model_ratios WHERE model=?", params![model])
}

fn load_all_model_ratios(conn: &Connection) -> rusqlite::Result<Vec<ModelRatio>> {
    let mut stmt =
        conn.prepare("SELECT model, ratio, updated_at FROM model_ratios ORDER BY model")?;
    let iter = stmt.query_map([], |row| {
        Ok(ModelRatio {
            model: row.get(0)?,
            ratio: row.get(1)?,
            updated_at: row
                .get::<_, Option<String>>(2)
                .unwrap_or_default()
                .unwrap_or_default(),
        })
    })?;
    let mut out = Vec::new();
    for r in iter {
        out.push(r?);
    }
    Ok(out)
}

/// 查模型倍率，缺失返回默认 1.0。
fn get_model_ratio(conn: &Connection, model: &str) -> f64 {
    conn.query_row(
        "SELECT ratio FROM model_ratios WHERE model=?",
        params![model],
        |r| r.get::<_, f64>(0),
    )
    .unwrap_or(1.0)
}

// ---- group_ratios CRUD ----

fn upsert_group_ratio(conn: &Connection, r: &GroupRatio) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO group_ratios (group_name, ratio, updated_at) VALUES (?,?,?)",
        params![r.group_name, r.ratio, r.updated_at],
    )?;
    Ok(())
}

fn load_all_group_ratios(conn: &Connection) -> rusqlite::Result<Vec<GroupRatio>> {
    let mut stmt =
        conn.prepare("SELECT group_name, ratio, updated_at FROM group_ratios ORDER BY group_name")?;
    let iter = stmt.query_map([], |row| {
        Ok(GroupRatio {
            group_name: row.get(0)?,
            ratio: row.get(1)?,
            updated_at: row
                .get::<_, Option<String>>(2)
                .unwrap_or_default()
                .unwrap_or_default(),
        })
    })?;
    let mut out = Vec::new();
    for r in iter {
        out.push(r?);
    }
    Ok(out)
}

/// 查用户组倍率，缺失返回默认 1.0。
fn get_group_ratio(conn: &Connection, group_name: &str) -> f64 {
    conn.query_row(
        "SELECT ratio FROM group_ratios WHERE group_name=?",
        params![group_name],
        |r| r.get::<_, f64>(0),
    )
    .unwrap_or(1.0)
}

// ---- redeem_codes CRUD ----

fn insert_redeem_code(conn: &Connection, c: &RedeemCode) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO redeem_codes
         (code,quota_amount,used_by,used_at,created_at,expires_at)
         VALUES (?,?,?,?,?,?)",
        params![
            c.code,
            c.quota_amount as i64,
            c.used_by.as_deref(),
            c.used_at.as_deref(),
            c.created_at,
            c.expires_at.as_deref(),
        ],
    )?;
    Ok(())
}

fn find_redeem_code(conn: &Connection, code: &str) -> rusqlite::Result<Option<RedeemCode>> {
    let mut stmt = conn.prepare(
        "SELECT code,quota_amount,used_by,used_at,created_at,expires_at
         FROM redeem_codes WHERE code=?",
    )?;
    stmt.query_row(params![code], redeem_code_from_row)
        .optional()
}

fn load_all_redeem_codes(conn: &Connection) -> rusqlite::Result<Vec<RedeemCode>> {
    let mut stmt = conn.prepare(
        "SELECT code,quota_amount,used_by,used_at,created_at,expires_at
         FROM redeem_codes ORDER BY created_at DESC",
    )?;
    let iter = stmt.query_map([], redeem_code_from_row)?;
    let mut out = Vec::new();
    for c in iter {
        out.push(c?);
    }
    Ok(out)
}

fn redeem_code_from_row(row: &rusqlite::Row) -> rusqlite::Result<RedeemCode> {
    let used_by: Option<String> = row.get(2)?;
    let used_at: Option<String> = row.get(3)?;
    let created_at: String = row
        .get::<_, Option<String>>(4)
        .unwrap_or_default()
        .unwrap_or_default();
    let expires_at: Option<String> = row.get(5)?;
    Ok(RedeemCode {
        code: row.get(0)?,
        quota_amount: row.get::<_, i64>(1)? as u64,
        used_by,
        used_at,
        created_at,
        expires_at: expires_at.filter(|s| !s.is_empty()),
    })
}

/// 标记兑换码已被使用（used_by + used_at）。
fn mark_redeem_used(conn: &Connection, c: &RedeemCode) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE redeem_codes SET used_by=?, used_at=? WHERE code=?",
        params![c.used_by.as_deref(), c.used_at.as_deref(), c.code],
    )?;
    Ok(())
}

/// 扣减 quota_used（加配额）：`quota_used = MAX(0, quota_used - amount)`。
fn adjust_token_quota(conn: &Connection, id: &str, amount: u64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE tokens SET quota_used = MAX(0, quota_used - ?) WHERE id=?",
        params![amount as i64, id],
    )?;
    Ok(())
}

/// 加积分（充值确认到账）：`quota_limit += credits`。
///
/// 与兑换码加额度同机制（都增加"可用余量 = quota_limit - quota_used"），只是兑换码
/// 走扣减 quota_used（见 [`adjust_token_quota`]），充值走抬高 quota_limit——credits
/// 模式下 quota_limit 即预付余额，per_token 模式下则是放宽配额上限，两者语义都对。
fn add_token_credits(conn: &Connection, id: &str, credits: u64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE tokens SET quota_limit = quota_limit + ? WHERE id=?",
        params![credits as i64, id],
    )?;
    Ok(())
}

// ---- payment_orders CRUD ----

/// `payment_orders` 列清单（INSERT/SELECT 共用；dApp 一期增链上核验两列）。
const PAYMENT_ORDER_COLUMNS: &str = "id,token_id,currency,amount_crypto,credits,address,memo,status,txid,created_at,confirmed_at,reject_reason,chain_block,chain_value_wei";

fn insert_payment_order(conn: &Connection, o: &PaymentOrder) -> rusqlite::Result<()> {
    conn.execute(
        &format!(
            "INSERT OR REPLACE INTO payment_orders
             ({PAYMENT_ORDER_COLUMNS})
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)"
        ),
        params![
            o.id,
            o.token_id,
            o.currency,
            o.amount_crypto,
            o.credits as i64,
            o.address,
            o.memo.as_deref(),
            o.status,
            o.txid.as_deref(),
            o.created_at,
            o.confirmed_at.as_deref(),
            o.reject_reason.as_deref(),
            o.chain_block,
            o.chain_value_wei,
        ],
    )?;
    Ok(())
}

fn find_payment_order(conn: &Connection, id: &str) -> rusqlite::Result<Option<PaymentOrder>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {PAYMENT_ORDER_COLUMNS}
         FROM payment_orders WHERE id=?"
    ))?;
    stmt.query_row(params![id], payment_order_from_row)
        .optional()
}

/// 列充值订单（status 过滤：None=全部；Some=按状态）。最新在前。
fn load_payment_orders(
    conn: &Connection,
    status: Option<&str>,
) -> rusqlite::Result<Vec<PaymentOrder>> {
    let (sql, params_vec): (String, Vec<String>) = match status {
        Some(s) if !s.is_empty() => (
            format!(
                "SELECT {PAYMENT_ORDER_COLUMNS}
                 FROM payment_orders WHERE status=? ORDER BY created_at DESC"
            ),
            vec![s.to_string()],
        ),
        _ => (
            format!(
                "SELECT {PAYMENT_ORDER_COLUMNS}
                 FROM payment_orders ORDER BY created_at DESC"
            ),
            vec![],
        ),
    };
    let mut stmt = conn.prepare(sql.as_str())?;
    let iter = stmt.query_map(
        rusqlite::params_from_iter(params_vec),
        payment_order_from_row,
    )?;
    let mut out = Vec::new();
    for o in iter {
        out.push(o?);
    }
    Ok(out)
}

fn payment_order_from_row(row: &rusqlite::Row) -> rusqlite::Result<PaymentOrder> {
    Ok(PaymentOrder {
        id: row.get(0)?,
        token_id: row.get(1)?,
        currency: row.get(2)?,
        amount_crypto: row.get(3)?,
        credits: row.get::<_, i64>(4)? as u64,
        address: row.get(5)?,
        memo: row.get(6)?,
        status: row.get(7)?,
        txid: row.get(8)?,
        created_at: row.get(9)?,
        confirmed_at: row.get(10)?,
        reject_reason: row.get(11)?,
        chain_block: row.get::<_, Option<i64>>(12)?.map(|v| v.max(0) as u64),
        chain_value_wei: row.get(13)?,
    })
}

// ----------------------------------------------------------------------------
// demo 数据（仅 cfg(test)——测试确定性填充；生产不 seed）
// ----------------------------------------------------------------------------

/// demo 渠道（2 条）。**生产清空**（2026-08-30 用户报告「删除的不是真实的数据」：
/// 首次空表 seed 的假渠道/令牌/日志让用户删的全是占位数据）。仅 `#[cfg(test)]`
/// 保留填充（`with_demo_data` / `with_db_path` 测试路径依赖 ch-1/ch-2 的确定性
/// 环境）；手法与 llm.rs `demo_instances` 同款。存量库里的 demo 残留不自动删
/// （用户可能已改过），可手动 `DELETE FROM channels WHERE id IN ('ch-1','ch-2')`
/// 等清理。
fn demo_channels() -> Vec<Channel> {
    #[cfg(test)]
    {
        vec![
            Channel {
                id: "ch-1".into(),
                name: "本地vLLM-7B".into(),
                provider: "local-vllm".into(),
                base_url: "http://localhost:8000/v1".into(),
                api_key: String::new(),
                models: vec!["Qwen2.5-7B".into()],
                priority: 0,
                weight: 1,
                status: "enabled".into(),
                enabled: true,
                created_at: "2026-08-08T09:00:00+08:00".into(),
                last_used: Some("2026-08-08T10:00:00+08:00".into()),
                request_count: 42,
                via_node: String::new(),
                price_per_call: 0.0,
                price_per_sec: 0.0,
                price_per_token: 0.0,
            },
            Channel {
                id: "ch-2".into(),
                name: "OpenAI官方".into(),
                provider: "openai".into(),
                base_url: "https://api.openai.com/v1".into(),
                api_key: "sk-xxx".into(),
                models: vec!["gpt-4o".into(), "gpt-4o-mini".into()],
                priority: 10,
                weight: 1,
                status: "enabled".into(),
                enabled: true,
                created_at: "2026-08-08T09:30:00+08:00".into(),
                last_used: None,
                request_count: 0,
                via_node: String::new(),
                price_per_call: 0.0,
                price_per_sec: 0.0,
                price_per_token: 0.0,
            },
        ]
    }
    #[cfg(not(test))]
    {
        Vec::new()
    }
}

/// demo 令牌（1 条）。生产清空（同 [`demo_channels`] 注释）。
fn demo_tokens() -> Vec<ApiToken> {
    #[cfg(test)]
    {
        vec![ApiToken {
            id: "tok-1".into(),
            name: "默认令牌".into(),
            key: "sk-os-demo123456".into(),
            status: "active".into(),
            enabled: true,
            quota_limit: 1_000_000,
            quota_used: 12_350,
            allowed_models: vec![],
            allowed_channels: vec![],
            group_name: "default".into(),
            billing_mode: "per_token".into(),
            expires_at: None,
            created_at: "2026-08-08T09:00:00+08:00".into(),
            last_used: Some("2026-08-08T10:00:00+08:00".into()),
            request_count: 42,
        }]
    }
    #[cfg(not(test))]
    {
        Vec::new()
    }
}

/// demo 调用日志（2 条）。生产清空（同 [`demo_channels`] 注释）。
fn demo_logs() -> Vec<CallLog> {
    #[cfg(test)]
    {
        vec![
            CallLog {
                id: "log-1".into(),
                token_id: "tok-1".into(),
                token_name: "默认令牌".into(),
                channel_id: "ch-1".into(),
                channel_name: "本地vLLM-7B".into(),
                model: "Qwen2.5-7B".into(),
                prompt_tokens: 120,
                completion_tokens: 80,
                total_tokens: 200,
                latency_ms: 1850,
                status: "success".into(),
                error: None,
                created_at: "2026-08-08T10:00:00+08:00".into(),
            },
            CallLog {
                id: "log-2".into(),
                token_id: "tok-1".into(),
                token_name: "默认令牌".into(),
                channel_id: "ch-2".into(),
                channel_name: "OpenAI官方".into(),
                model: "gpt-4o-mini".into(),
                prompt_tokens: 50,
                completion_tokens: 30,
                total_tokens: 80,
                latency_ms: 920,
                status: "success".into(),
                error: None,
                created_at: "2026-08-08T10:05:00+08:00".into(),
            },
        ]
    }
    #[cfg(not(test))]
    {
        Vec::new()
    }
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

    fn post_req_with_auth(path: &str, body: serde_json::Value, bearer: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Post,
            path: path.into(),
            headers: serde_json::json!({ "Authorization": format!("Bearer {bearer}") }),
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

    // ---- 纯函数测试 ----

    #[test]
    fn mask_secret_long_string_shows_head_and_tail() {
        // 漏洞3：长度 >= 8 → 前4 + *** + 后4
        // "sk-xe1234567890"（14 字符）：前4="sk-x"，后4="7890"
        assert_eq!(mask_secret("sk-xe1234567890"), "sk-x***7890");
        assert_eq!(mask_secret("abcdefgh"), "abcd***efgh");
        // "sk-os-deadbeefcafef00d"：前4="sk-n"，后4="f00d"
        assert_eq!(mask_secret("sk-os-deadbeefcafef00d"), "sk-o***f00d");
    }

    #[test]
    fn mask_secret_short_string_returns_masked() {
        // 长度 < 8 且非空 → ***
        assert_eq!(mask_secret("short"), "***");
        assert_eq!(mask_secret("1234567"), "***");
        // 空串 → ***（不泄露是否配置）
        assert_eq!(mask_secret(""), "***");
    }

    #[test]
    fn mask_secret_unicode_safe() {
        // Unicode 字符按字符计数（不按字节）
        // "密钥abcdefgh"（10 字符）：前4="密钥ab"，后4="efgh"
        assert_eq!(mask_secret("密钥abcdefgh"), "密钥ab***efgh");
    }

    #[tokio::test]
    async fn get_channels_response_masks_api_key() {
        // 漏洞3：GET /channels 响应里 api_key 必须脱敏（不含完整明文）
        let h = ApiGatewayRouteHandler::with_demo_data();
        let resp = h.handle(get_req("/api/v1/gateway/channels")).await.unwrap();
        let arr = resp.body.as_array().unwrap();
        // 至少应含 demo 渠道（ch-2 的明文 api_key 是 "sk-xxx"）
        for c in arr {
            let key = c["api_key"].as_str().unwrap_or("");
            // 完整明文 "sk-xxx"（6 字符 < 8）→ 应脱敏为 "***"
            assert!(
                !key.contains("xxx") || key == "***",
                "GET /channels api_key 必须脱敏，实际: {key}"
            );
        }
    }

    #[tokio::test]
    async fn get_channel_detail_response_masks_api_key() {
        // 漏洞3：GET /channels/:id 响应里 api_key 必须脱敏
        let h = ApiGatewayRouteHandler::with_demo_data();
        let resp = h
            .handle(get_req("/api/v1/gateway/channels/ch-2"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let key = resp.body["api_key"].as_str().unwrap_or("");
        assert!(
            !key.contains("xxx") || key == "***",
            "GET /channels/:id api_key 必须脱敏，实际: {key}"
        );
    }

    #[tokio::test]
    async fn get_tokens_response_masks_key() {
        // 漏洞3：GET /tokens 响应里 key 必须脱敏
        let h = ApiGatewayRouteHandler::with_demo_data();
        let resp = h.handle(get_req("/api/v1/gateway/tokens")).await.unwrap();
        let arr = resp.body.as_array().unwrap();
        for t in arr {
            let key = t["key"].as_str().unwrap_or("");
            // demo 令牌 key = "sk-os-demo123456"（17 字符）→ 应脱敏为 "sk-n***3456"
            assert!(
                !key.contains("demo123456"),
                "GET /tokens key 必须脱敏，实际: {key}"
            );
        }
    }

    #[tokio::test]
    async fn post_token_returns_full_key_once() {
        // 漏洞3：POST /tokens 创建时仍返回完整 key 一次（One API 行为）
        let h = ApiGatewayRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/tokens",
                serde_json::json!({"name": "test"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        let key = resp.body["key"].as_str().unwrap();
        assert!(
            key.starts_with("sk-os-") && key.len() > 20,
            "POST /tokens 创建时应返回完整 key: {key}"
        );
        assert!(!key.contains("***"), "POST /tokens 创建响应不应脱敏: {key}");
    }

    #[test]
    fn generate_api_key_has_correct_prefix_and_length() {
        let k = generate_api_key();
        assert!(k.starts_with("sk-os-"), "前缀应为 sk-os-: {k}");
        // sk-os- (7) + 32 hex
        assert_eq!(k.len(), 6 + 32, "总长度应为 38: {k}");
        let hex = &k[6..];
        assert!(
            hex.chars().all(|c| c.is_ascii_hexdigit()),
            "应为 hex: {hex}"
        );
    }

    #[test]
    fn generate_api_key_produces_varied_output() {
        // 多次调用应产生不同 key（不严格要求唯一，但应高熵）
        let k1 = generate_api_key();
        let k2 = generate_api_key();
        assert_ne!(k1, k2, "连续调用应不同");
    }

    #[test]
    fn select_channel_picks_lowest_priority() {
        let channels = vec![
            Channel {
                id: "a".into(),
                name: "A".into(),
                provider: "openai".into(),
                base_url: "http://a".into(),
                api_key: String::new(),
                models: vec!["gpt-4o".into()],
                priority: 5,
                weight: 1,
                status: "enabled".into(),
                enabled: true,
                created_at: String::new(),
                last_used: None,
                request_count: 0,
                via_node: String::new(),
                price_per_call: 0.0,
                price_per_sec: 0.0,
                price_per_token: 0.0,
            },
            Channel {
                id: "b".into(),
                name: "B".into(),
                provider: "local-vllm".into(),
                base_url: "http://b".into(),
                api_key: String::new(),
                models: vec!["gpt-4o".into()],
                priority: 1,
                weight: 1,
                status: "enabled".into(),
                enabled: true,
                created_at: String::new(),
                last_used: None,
                request_count: 0,
                via_node: String::new(),
                price_per_call: 0.0,
                price_per_sec: 0.0,
                price_per_token: 0.0,
            },
        ];
        let sel = select_channel(&channels, "gpt-4o").expect("应选中");
        assert_eq!(sel.id, "b", "应选 priority 最小的 b");
    }

    #[test]
    fn select_channel_weight_weighted_within_same_priority() {
        // 同 priority 下，weight=10 的渠道应大概率（这里确定性 hash）命中
        let mut channels = vec![];
        for i in 0..3 {
            channels.push(Channel {
                id: format!("c{i}"),
                name: format!("C{i}"),
                provider: "openai".into(),
                base_url: format!("http://c{i}"),
                api_key: String::new(),
                models: vec!["m".into()],
                priority: 0,
                weight: if i == 0 { 100 } else { 1 },
                status: "enabled".into(),
                enabled: true,
                created_at: String::new(),
                last_used: None,
                request_count: 0,
                via_node: String::new(),
                price_per_call: 0.0,
                price_per_sec: 0.0,
                price_per_token: 0.0,
            });
        }
        // 加权随机下 c0 (weight=100) 应被选中
        let sel = select_channel(&channels, "m").expect("应选中");
        assert_eq!(sel.id, "c0", "weight 大的应被选中");
    }

    #[test]
    fn select_channel_returns_none_when_no_matching_model() {
        let channels = vec![Channel {
            id: "a".into(),
            name: "A".into(),
            provider: "openai".into(),
            base_url: "http://a".into(),
            api_key: String::new(),
            models: vec!["gpt-4o".into()],
            priority: 0,
            weight: 1,
            status: "enabled".into(),
            enabled: true,
            created_at: String::new(),
            last_used: None,
            request_count: 0,
            via_node: String::new(),
            price_per_call: 0.0,
            price_per_sec: 0.0,
            price_per_token: 0.0,
        }];
        assert!(
            select_channel(&channels, "claude-3").is_none(),
            "无匹配模型应 None"
        );
    }

    #[test]
    fn select_channel_skips_disabled() {
        let channels = vec![Channel {
            id: "a".into(),
            name: "A".into(),
            provider: "openai".into(),
            base_url: "http://a".into(),
            api_key: String::new(),
            models: vec!["gpt-4o".into()],
            priority: 0,
            weight: 1,
            status: "disabled".into(),
            enabled: false,
            created_at: String::new(),
            last_used: None,
            request_count: 0,
            via_node: String::new(),
            price_per_call: 0.0,
            price_per_sec: 0.0,
            price_per_token: 0.0,
        }];
        assert!(
            select_channel(&channels, "gpt-4o").is_none(),
            "禁用渠道不应选中"
        );
    }

    #[test]
    fn extract_bearer_normal() {
        assert_eq!(
            extract_bearer("Bearer sk-os-abc123").as_deref(),
            Some("sk-os-abc123")
        );
    }

    #[test]
    fn extract_bearer_case_insensitive_prefix() {
        assert_eq!(
            extract_bearer("bearer sk-os-xyz").as_deref(),
            Some("sk-os-xyz")
        );
    }

    #[test]
    fn extract_bearer_no_prefix_returns_none() {
        assert!(
            extract_bearer("sk-os-abc123").is_none(),
            "无 Bearer 前缀应 None"
        );
    }

    #[test]
    fn extract_bearer_empty_returns_none() {
        assert!(extract_bearer("").is_none(), "空串应 None");
        assert!(extract_bearer("Bearer ").is_none(), "Bearer 后空应 None");
        assert!(extract_bearer("Bearer").is_none(), "仅 Bearer 应 None");
    }

    // ---- 路由声明测试 ----

    #[tokio::test]
    async fn routes_declares_nineteen_endpoints_all_api_gateway() {
        let h = ApiGatewayRouteHandler::with_demo_data();
        let routes = h.routes().await;
        // 6 渠道 + 5 令牌 + 3 日志/统计/模型 + 3 映射 + 3 模型倍率 + 2 组倍率
        // + 3 兑换码/兑换 + 4 充值订单 + 2 代理转发 = 31
        assert_eq!(routes.len(), 32, "应有 32 条路由: {routes:?}");
        assert!(
            routes.iter().all(|r| r.handler_component == "api_gateway"),
            "全部归属 api_gateway 组件"
        );
    }

    #[tokio::test]
    async fn routes_payments_write_ops_require_admin() {
        // 充值路由归属与鉴权：POST 创建 / POST confirm / POST reject 是 admin 写操作；
        // GET 列表公开读（与 GET /tokens、GET /logs 一致）
        let h = ApiGatewayRouteHandler::with_demo_data();
        let routes = h.routes().await;
        let find = |method: HttpMethod, path: &str| {
            routes
                .iter()
                .find(|r| r.method == method && r.path == path)
                .unwrap_or_else(|| panic!("缺少路由 {method:?} {path}"))
        };
        for path in &[
            "/api/v1/gateway/payments",
            "/api/v1/gateway/payments/:id/confirm",
            "/api/v1/gateway/payments/:id/reject",
        ] {
            let r = find(HttpMethod::Post, path);
            assert!(r.requires_auth, "POST {path} 写操作应要求认证");
            assert_eq!(
                r.required_roles,
                vec!["admin".to_string()],
                "POST {path} 应 admin"
            );
        }
        let r = find(HttpMethod::Get, "/api/v1/gateway/payments");
        assert!(!r.requires_auth, "GET /payments 公开读");
    }

    // ---- 渠道 CRUD ----

    #[tokio::test]
    async fn create_channel_then_list_contains_new() {
        let h = ApiGatewayRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/channels",
                serde_json::json!({
                    "name": "test-ch",
                    "provider": "openai",
                    "base_url": "https://api.openai.com/v1",
                    "api_key": "sk-xxx",
                    "models": ["gpt-4o"]
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "create body: {resp:?}");
        assert_eq!(resp.body["provider"], "openai");
        assert_eq!(resp.body["status"], "enabled");
        assert_eq!(resp.body["enabled"], true);
        let id = resp.body["id"].as_str().unwrap().to_string();
        // 列表含新渠道
        let resp = h.handle(get_req("/api/v1/gateway/channels")).await.unwrap();
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], id);
    }

    #[tokio::test]
    async fn create_channel_validates_empty_name() {
        let h = ApiGatewayRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/channels",
                serde_json::json!({"name": "", "provider": "openai", "base_url": "x"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    #[tokio::test]
    async fn create_channel_rejects_bad_provider() {
        let h = ApiGatewayRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/channels",
                serde_json::json!({"name": "x", "provider": "bogus", "base_url": "y"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    // ---- 添加渠道：from_discovery 从本地 vLLM 发现导入（2026-08-30）----

    /// 起一个极简 HTTP 服务回 vLLM /v1/models JSON（手法同 llm.rs 的假服务：
    /// std TcpListener，依次响应 `n` 次请求，多余请求阻塞在 accept）。
    fn spawn_fake_v1_models(model_ids: &[&str], responses: usize) -> u16 {
        use std::io::{Read, Write};
        let data: Vec<serde_json::Value> = model_ids
            .iter()
            .map(|id| serde_json::json!({"id": id, "object": "model", "owned_by": "vllm"}))
            .collect();
        let body = serde_json::json!({"object": "list", "data": data}).to_string();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 失败");
        let port = listener.local_addr().expect("local_addr 失败").port();
        std::thread::spawn(move || {
            for _ in 0..responses {
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

    /// 找一个几乎必然关闭的本机端口（bind 临时 listener 拿空闲端口后立刻释放）。
    fn closed_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 失败");
        listener.local_addr().expect("local_addr 失败").port()
    }

    #[tokio::test]
    async fn create_channel_from_discovery_probes_and_fills_models() {
        // 后端实测端口 /v1/models → models=data[].id、base_url=http://127.0.0.1:<port>/v1、
        // provider=local-vllm、name 缺省「发现的 vLLM :<port>」
        let port = spawn_fake_v1_models(&["qwen3-vl-8b", "qwen2.5-7b"], 2);
        let h = ApiGatewayRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/channels",
                serde_json::json!({"from_discovery": {"port": port}}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "create body: {resp:?}");
        assert_eq!(resp.body["provider"], "local-vllm");
        assert_eq!(resp.body["base_url"], format!("http://127.0.0.1:{port}/v1"));
        assert_eq!(resp.body["name"], format!("发现的 vLLM :{port}"));
        assert_eq!(
            resp.body["models"],
            serde_json::json!(["qwen3-vl-8b", "qwen2.5-7b"]),
            "models 取自探测到的 data[].id: {resp:?}"
        );
        assert_eq!(resp.body["status"], "enabled");
        // 聚合可用模型（GET /gateway/models）随之出现（去重）
        let resp = h.handle(get_req("/api/v1/gateway/models")).await.unwrap();
        assert_eq!(resp.body["models"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn create_channel_from_discovery_honors_custom_name_and_models() {
        let port = spawn_fake_v1_models(&["upstream-name"], 2);
        let h = ApiGatewayRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/channels",
                serde_json::json!({
                    "from_discovery": {"port": port, "name": "本机大模型", "models": ["my-alias"]}
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "body: {resp:?}");
        assert_eq!(resp.body["name"], "本机大模型");
        assert_eq!(
            resp.body["models"],
            serde_json::json!(["my-alias"]),
            "显式 models 覆盖探测结果: {resp:?}"
        );
    }

    #[tokio::test]
    async fn create_channel_from_discovery_dead_port_502_no_channel() {
        // 探测失败 502 且不建渠道（绝不把猜的端口当可用）
        let h = ApiGatewayRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/channels",
                serde_json::json!({"from_discovery": {"port": closed_port()}}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 502, "body: {resp:?}");
        assert!(
            resp.body["error"]
                .as_str()
                .unwrap_or("")
                .contains("/v1/models"),
            "错误带可排查前缀: {resp:?}"
        );
        assert_eq!(h.channels_snapshot().len(), 0, "不得创建渠道");
    }

    #[tokio::test]
    async fn delete_channel_removes() {
        let h = ApiGatewayRouteHandler::with_demo_data();
        let before = h.channels_snapshot().len();
        let resp = h
            .handle(del_req("/api/v1/gateway/channels/ch-2"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(h.channels_snapshot().len(), before - 1);
    }

    #[tokio::test]
    async fn get_channel_detail_returns_200() {
        let h = ApiGatewayRouteHandler::with_demo_data();
        let resp = h
            .handle(get_req("/api/v1/gateway/channels/ch-1"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["id"], "ch-1");
        assert_eq!(resp.body["provider"], "local-vllm");
    }

    // ---- 令牌 CRUD ----

    #[tokio::test]
    async fn create_token_generates_key() {
        let h = ApiGatewayRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/tokens",
                serde_json::json!({"name": "测试key", "quota_limit": 5000}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "create body: {resp:?}");
        let key = resp.body["key"].as_str().unwrap();
        assert!(key.starts_with("sk-os-"), "应生成 sk-os- 前缀 key: {key}");
        assert!(key.len() > 20, "key 应有足够长度: {key}");
        assert_eq!(resp.body["quota_limit"], 5000);
        assert_eq!(resp.body["status"], "active");
        assert_eq!(resp.body["enabled"], true);
    }

    #[tokio::test]
    async fn disable_then_enable_token() {
        let h = ApiGatewayRouteHandler::with_demo_data();
        // 禁用
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/tokens/tok-1/disable",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "disable body: {resp:?}");
        assert_eq!(resp.body["status"], "disabled");
        assert_eq!(resp.body["enabled"], false);
        // 启用
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/tokens/tok-1/enable",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["status"], "active");
        assert_eq!(resp.body["enabled"], true);
    }

    #[tokio::test]
    async fn delete_token_removes() {
        let h = ApiGatewayRouteHandler::with_demo_data();
        let before = h.tokens_snapshot().len();
        let resp = h
            .handle(del_req("/api/v1/gateway/tokens/tok-1"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(h.tokens_snapshot().len(), before - 1);
    }

    // ---- 日志与统计 ----

    #[tokio::test]
    async fn logs_returns_list_with_limit() {
        let h = ApiGatewayRouteHandler::with_demo_data();
        let resp = h
            .handle(get_req("/api/v1/gateway/logs?limit=1"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 1, "limit=1 应只返回 1 条");
    }

    #[tokio::test]
    async fn stats_aggregates_counts() {
        let h = ApiGatewayRouteHandler::with_demo_data();
        let resp = h.handle(get_req("/api/v1/gateway/stats")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["channels_total"], 2, "2 个 demo 渠道");
        assert_eq!(resp.body["channels_enabled"], 2);
        assert_eq!(resp.body["tokens_total"], 1);
        assert_eq!(resp.body["tokens_active"], 1);
        assert!(
            resp.body["total_tokens"].as_u64().unwrap() > 0,
            "应有 token 累计"
        );
        // demo 2 条日志全 success → 100%
        assert!((resp.body["success_rate"].as_f64().unwrap() - 100.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn models_aggregates_and_dedups() {
        let h = ApiGatewayRouteHandler::with_demo_data();
        let resp = h.handle(get_req("/api/v1/gateway/models")).await.unwrap();
        assert_eq!(resp.status, 200);
        let models = resp.body["models"].as_array().unwrap();
        // Qwen2.5-7B + gpt-4o + gpt-4o-mini = 3（去重后）
        assert_eq!(models.len(), 3, "应去重聚合: {models:?}");
    }

    // ---- OpenAI 形态模型列表（GET /api/v1/gateway/v1/models，对外接入）----

    fn get_req_with_auth(path: &str, bearer: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.into(),
            headers: serde_json::json!({ "Authorization": format!("Bearer {bearer}") }),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    /// 造一条测试令牌（db 直插，同 proxy 测试手法）。
    fn seed_token(
        h: &ApiGatewayRouteHandler,
        id: &str,
        key: &str,
        allowed_models: Vec<String>,
        allowed_channels: Vec<String>,
    ) {
        let conn = h.db.lock().unwrap();
        insert_token(
            &conn,
            &ApiToken {
                id: id.into(),
                name: format!("tok-{id}"),
                key: key.into(),
                status: "active".into(),
                enabled: true,
                quota_limit: 0,
                quota_used: 0,
                allowed_models,
                allowed_channels,
                group_name: "default".into(),
                billing_mode: "per_token".into(),
                expires_at: None,
                created_at: String::new(),
                last_used: None,
                request_count: 0,
            },
        )
        .unwrap();
    }

    fn seed_channel(
        h: &ApiGatewayRouteHandler,
        id: &str,
        models: &[&str],
        enabled: bool,
        priority: u32,
    ) {
        let conn = h.db.lock().unwrap();
        insert_channel(
            &conn,
            &Channel {
                id: id.into(),
                name: format!("ch-{id}"),
                provider: "openai".into(),
                base_url: format!("http://127.0.0.1:1/v1/{id}"),
                api_key: String::new(),
                models: models.iter().map(|s| s.to_string()).collect(),
                priority,
                weight: 1,
                status: if enabled { "enabled" } else { "disabled" }.into(),
                enabled,
                created_at: String::new(),
                last_used: None,
                request_count: 0,
                via_node: String::new(),
                price_per_call: 0.0,
                price_per_sec: 0.0,
                price_per_token: 0.0,
            },
        )
        .unwrap();
    }

    #[tokio::test]
    async fn openai_models_requires_valid_bearer_token() {
        let h = ApiGatewayRouteHandler::with_empty();
        seed_channel(&h, "c1", &["gpt-4o"], true, 0);
        seed_token(&h, "t1", "sk-os-list", vec![], vec![]);
        // 无 Authorization → 401
        let resp = h
            .handle(get_req("/api/v1/gateway/v1/models"))
            .await
            .unwrap();
        assert_eq!(resp.status, 401, "无 Bearer 应 401: {resp:?}");
        // 错 key → 401
        let resp = h
            .handle(get_req_with_auth(
                "/api/v1/gateway/v1/models",
                "sk-os-wrong",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 401, "错误 key 应 401");
        // 对 key → 200
        let resp = h
            .handle(get_req_with_auth("/api/v1/gateway/v1/models", "sk-os-list"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
    }

    #[tokio::test]
    async fn openai_models_openai_shape_and_mapping_names() {
        let h = ApiGatewayRouteHandler::with_empty();
        seed_channel(&h, "c1", &["gpt-4o", "gpt-4o-mini"], true, 0);
        seed_channel(&h, "c2", &["gpt-4o", "qwen"], true, 1); // gpt-4o 与 c1 重复
        seed_token(&h, "t1", "sk-os-list", vec![], vec![]);
        // 映射：对外 gpt-4 → c1 上游 gpt-4o（public_name 也进列表）
        {
            let conn = h.db.lock().unwrap();
            upsert_mapping(
                &conn,
                &ModelMapping {
                    public_name: "gpt-4".into(),
                    channel_id: "c1".into(),
                    upstream_model: "gpt-4o".into(),
                },
            )
            .unwrap();
        }
        let resp = h
            .handle(get_req_with_auth("/api/v1/gateway/v1/models", "sk-os-list"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        // OpenAI 形状
        assert_eq!(resp.body["object"], "list");
        let data = resp.body["data"].as_array().expect("data 数组");
        let ids: Vec<&str> = data
            .iter()
            .map(|m| m["id"].as_str().expect("id 字符串"))
            .collect();
        // 去重 + 含映射名 + 排序稳定：gpt-4 < gpt-4o < gpt-4o-mini < qwen
        assert_eq!(
            ids,
            vec!["gpt-4", "gpt-4o", "gpt-4o-mini", "qwen"],
            "{ids:?}"
        );
        for m in data {
            assert_eq!(m["object"], "model");
            assert_eq!(m["owned_by"], "nexos-gateway");
            assert_eq!(m["created"], MODELS_LIST_CREATED_TS);
        }
    }

    #[tokio::test]
    async fn openai_models_filters_by_allowed_models_and_channels() {
        let h = ApiGatewayRouteHandler::with_empty();
        seed_channel(&h, "c1", &["gpt-4o", "gpt-4o-mini"], true, 0);
        seed_channel(&h, "c2", &["qwen"], true, 1);
        seed_channel(&h, "c3", &["llama"], false, 0); // 禁用渠道不进列表
        seed_token(
            &h,
            "t1",
            "sk-os-limited",
            vec!["gpt-4o".into(), "qwen".into()], // allowed_models 白名单
            vec!["c1".into(), "c3".into()],       // allowed_channels：排除 c2
        );
        let resp = h
            .handle(get_req_with_auth(
                "/api/v1/gateway/v1/models",
                "sk-os-limited",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let ids: Vec<&str> = resp.body["data"]
            .as_array()
            .expect("data 数组")
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        // c2（qwen）被 allowed_channels 排除；c3 禁用排除；只剩 allowed_models 命中的 gpt-4o
        assert_eq!(
            ids,
            vec!["gpt-4o"],
            "allowed_models × allowed_channels 双过滤: {ids:?}"
        );
    }

    #[tokio::test]
    async fn openai_models_excludes_mapping_whose_channel_unavailable() {
        // 映射目标渠道被禁用/不在 allowed_channels → public_name 不虚列
        let h = ApiGatewayRouteHandler::with_empty();
        seed_channel(&h, "c1", &["gpt-4o"], true, 0);
        seed_channel(&h, "c2", &["qwen"], true, 1);
        {
            let conn = h.db.lock().unwrap();
            upsert_mapping(
                &conn,
                &ModelMapping {
                    public_name: "big-model".into(),
                    channel_id: "c2".into(),
                    upstream_model: "qwen".into(),
                },
            )
            .unwrap();
        }
        // 令牌只允许 c1 → c2 不可用 → big-model 不出现
        seed_token(&h, "t1", "sk-os-c1only", vec![], vec!["c1".into()]);
        let resp = h
            .handle(get_req_with_auth(
                "/api/v1/gateway/v1/models",
                "sk-os-c1only",
            ))
            .await
            .unwrap();
        let ids: Vec<&str> = resp.body["data"]
            .as_array()
            .expect("data 数组")
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["gpt-4o"], "不可路由的映射名不虚列: {ids:?}");
    }

    // ---- 流式 usage 解析（parse_stream_usage 纯函数）----

    #[test]
    fn parse_stream_usage_takes_last_usage_chunk() {
        let sse = "\
data: {\"id\":1,\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n\
data: {\"id\":1,\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n\
data: {\"id\":1,\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3,\"total_tokens\":10}}\n\n\
data: [DONE]\n\n";
        assert_eq!(parse_stream_usage(sse), Some((7, 3, 10)));
    }

    #[test]
    fn parse_stream_usage_none_when_not_reported() {
        // 上游未上报 usage（未带 stream_options.include_usage）→ None（调用方记 0）
        let sse = "\
data: {\"id\":1,\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
data: [DONE]\n\n";
        assert_eq!(parse_stream_usage(sse), None);
    }

    #[test]
    fn parse_stream_usage_skips_done_partial_and_comment_lines() {
        let sse = ": keep-alive\n\n\
data: {not-json-partial\n\n\
data: [DONE]\n\n\
event: ping\n\n";
        assert_eq!(parse_stream_usage(sse), None);
    }

    #[test]
    fn parse_stream_usage_defaults_total_to_sum_when_missing() {
        let sse = "data: {\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":6}}\n\n";
        // total 缺省 → pt+ct（与 parse_usage 同口径）
        assert_eq!(parse_stream_usage(sse), Some((4, 6, 10)));
    }

    #[test]
    fn parse_stream_usage_takes_last_when_multiple_usage_chunks() {
        // 非规范上游多个 usage 块 → 取最后一个
        let sse = "\
data: {\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n\
data: {\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":8,\"total_tokens\":17}}\n\n";
        assert_eq!(parse_stream_usage(sse), Some((9, 8, 17)));
    }

    // ---- 模型映射 ----

    #[tokio::test]
    async fn mapping_crud() {
        let h = ApiGatewayRouteHandler::with_demo_data();
        // 添加
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/mappings",
                serde_json::json!({
                    "public_name": "gpt-4",
                    "channel_id": "ch-2",
                    "upstream_model": "gpt-4o"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "create mapping: {resp:?}");
        // 列
        let resp = h.handle(get_req("/api/v1/gateway/mappings")).await.unwrap();
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["public_name"], "gpt-4");
        // 删
        let resp = h
            .handle(del_req("/api/v1/gateway/mappings/gpt-4"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        // 列为空
        let resp = h.handle(get_req("/api/v1/gateway/mappings")).await.unwrap();
        assert_eq!(resp.body.as_array().unwrap().len(), 0);
    }

    // ---- 代理转发（降级，不 panic）----

    #[tokio::test]
    async fn proxy_no_token_returns_401() {
        let h = ApiGatewayRouteHandler::with_demo_data();
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/v1/chat/completions",
                serde_json::json!({"model": "gpt-4o", "messages": []}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 401, "无 token 应 401: {resp:?}");
    }

    #[tokio::test]
    async fn proxy_invalid_token_returns_401() {
        let h = ApiGatewayRouteHandler::with_demo_data();
        let resp = h
            .handle(post_req_with_auth(
                "/api/v1/gateway/v1/chat/completions",
                serde_json::json!({"model": "gpt-4o", "messages": []}),
                "sk-os-bogus",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 401, "无效 token 应 401");
    }

    #[tokio::test]
    async fn proxy_quota_exceeded_returns_429() {
        // 构造一个已超配额的令牌
        let h = ApiGatewayRouteHandler::with_empty();
        {
            let conn = h.db.lock().unwrap();
            insert_token(
                &conn,
                &ApiToken {
                    id: "tok-x".into(),
                    name: "超额".into(),
                    key: "sk-os-over".into(),
                    status: "active".into(),
                    enabled: true,
                    quota_limit: 100,
                    quota_used: 200,
                    allowed_models: vec![],
                    allowed_channels: vec![],
                    group_name: "default".into(),
                    billing_mode: "per_token".into(),
                    expires_at: None,
                    created_at: String::new(),
                    last_used: None,
                    request_count: 0,
                },
            )
            .unwrap();
        }
        let resp = h
            .handle(post_req_with_auth(
                "/api/v1/gateway/v1/chat/completions",
                serde_json::json!({"model": "gpt-4o", "messages": []}),
                "sk-os-over",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 429, "超额应 429: {resp:?}");
    }

    #[tokio::test]
    async fn proxy_disabled_token_returns_401() {
        let h = ApiGatewayRouteHandler::with_empty();
        {
            let conn = h.db.lock().unwrap();
            insert_token(
                &conn,
                &ApiToken {
                    id: "tok-d".into(),
                    name: "禁用".into(),
                    key: "sk-os-disabled".into(),
                    status: "disabled".into(),
                    enabled: false,
                    quota_limit: 0,
                    quota_used: 0,
                    allowed_models: vec![],
                    allowed_channels: vec![],
                    group_name: "default".into(),
                    billing_mode: "per_token".into(),
                    expires_at: None,
                    created_at: String::new(),
                    last_used: None,
                    request_count: 0,
                },
            )
            .unwrap();
        }
        let resp = h
            .handle(post_req_with_auth(
                "/api/v1/gateway/v1/chat/completions",
                serde_json::json!({"model": "gpt-4o", "messages": []}),
                "sk-os-disabled",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 401, "禁用令牌应 401");
    }

    #[tokio::test]
    async fn proxy_no_matching_channel_returns_404_or_502_without_panic() {
        // 有令牌但无匹配渠道（上游不在线）：应返回 404/502，不 panic
        let h = ApiGatewayRouteHandler::with_empty();
        {
            let conn = h.db.lock().unwrap();
            insert_token(
                &conn,
                &ApiToken {
                    id: "tok-ok".into(),
                    name: "可用".into(),
                    key: "sk-os-ok".into(),
                    status: "active".into(),
                    enabled: true,
                    quota_limit: 0,
                    quota_used: 0,
                    allowed_models: vec![],
                    allowed_channels: vec![],
                    group_name: "default".into(),
                    billing_mode: "per_token".into(),
                    expires_at: None,
                    created_at: String::new(),
                    last_used: None,
                    request_count: 0,
                },
            )
            .unwrap();
        }
        // 无渠道 → 404
        let resp = h
            .handle(post_req_with_auth(
                "/api/v1/gateway/v1/chat/completions",
                serde_json::json!({"model": "gpt-4o", "messages": [{"role":"user","content":"hi"}]}),
                "sk-os-ok",
            ))
            .await
            .unwrap();
        assert!(
            resp.status == 404 || resp.status == 502,
            "无渠道应 404 或 502: {}",
            resp.status
        );
    }

    #[tokio::test]
    async fn proxy_missing_model_field_returns_400() {
        let h = ApiGatewayRouteHandler::with_demo_data();
        let resp = h
            .handle(post_req_with_auth(
                "/api/v1/gateway/v1/chat/completions",
                serde_json::json!({"messages": []}),
                "sk-os-demo123456",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "缺 model 应 400");
    }

    // ---- 兜底 ----

    #[tokio::test]
    async fn unmatched_route_returns_404() {
        let h = ApiGatewayRouteHandler::with_demo_data();
        let resp = h.handle(get_req("/api/v1/gateway/nope")).await.unwrap();
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<ApiGatewayRouteHandler>();
    }

    #[test]
    fn parse_query_int_works() {
        assert_eq!(parse_query_int("limit=10", "limit", 50), 10);
        assert_eq!(parse_query_int("foo=1&limit=25", "limit", 50), 25);
        assert_eq!(parse_query_int("", "limit", 50), 50);
        assert_eq!(parse_query_int("limit=abc", "limit", 50), 50);
    }

    #[test]
    fn parse_usage_extracts_three_fields() {
        let body = br#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":20,"total_tokens":30}}"#;
        let (p, c, t) = parse_usage(body).expect("应解析");
        assert_eq!((p, c, t), (10, 20, 30));
    }

    // ---- 非流式转发超时（60→300s + env 可配，2026-09-03）----

    #[test]
    fn parse_upstream_timeout_secs_defaults_and_bounds() {
        // 缺省：None / 空串 / 非数字 / 纯空白 → 300
        assert_eq!(parse_upstream_timeout_secs(None), 300);
        assert_eq!(parse_upstream_timeout_secs(Some("")), 300);
        assert_eq!(parse_upstream_timeout_secs(Some("abc")), 300);
        assert_eq!(parse_upstream_timeout_secs(Some("  ")), 300);
        assert_eq!(
            parse_upstream_timeout_secs(Some("60")),
            60,
            "合法值透传（部署方显式要旧值 60 也尊重）"
        );
        assert_eq!(
            parse_upstream_timeout_secs(Some(" 120 ")),
            120,
            "容忍首尾空白"
        );
        assert_eq!(
            parse_upstream_timeout_secs(Some("0")),
            300,
            "下界外回落缺省"
        );
        assert_eq!(
            parse_upstream_timeout_secs(Some("99999")),
            300,
            "上界外回落缺省（不猜极端值）"
        );
        assert_eq!(parse_upstream_timeout_secs(Some("-5")), 300, "负数回落");
        assert_eq!(
            parse_upstream_timeout_secs(Some("3600")),
            3600,
            "上界内合法（1h）"
        );
        assert_eq!(
            parse_upstream_timeout_secs(Some("1")),
            1,
            "下界内合法（测试可注入 1s）"
        );
    }

    #[test]
    fn parse_usage_returns_none_without_usage() {
        let body = br#"{"choices":[]}"#;
        assert!(parse_usage(body).is_none());
    }

    #[test]
    fn is_valid_provider_whitelist() {
        assert!(is_valid_provider("openai"));
        assert!(is_valid_provider("deepseek"));
        assert!(is_valid_provider("anthropic"));
        assert!(is_valid_provider("local-vllm"));
        assert!(is_valid_provider("azure"));
        assert!(is_valid_provider("ollama"));
        assert!(!is_valid_provider("bogus"));
        assert!(!is_valid_provider(""));
    }

    // ---- SQLite 持久化新增测试 ----

    /// 唯一临时 DB 文件路径（进程隔离 + 线程隔离，测试结束清理）。
    fn unique_temp_db_path() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        format!("/tmp/os-gateway-test-{pid}-{n}.db")
    }

    #[tokio::test]
    async fn channel_persists_across_reopen() {
        // (a) 创建 channel 后"重启"（新 Connection 同文件）能读到
        let path = unique_temp_db_path();
        // 用空库起步（不 seed demo），创建一个渠道
        let h = ApiGatewayRouteHandler::with_db_path(&path);
        // 清掉可能 seed 的 demo 数据，确保从干净状态验证
        {
            let conn = h.db.lock().unwrap();
            conn.execute("DELETE FROM channels", []).unwrap();
        }
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/channels",
                serde_json::json!({
                    "name": "持久渠道",
                    "provider": "deepseek",
                    "base_url": "https://api.deepseek.com/v1",
                    "models": ["deepseek-chat"]
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "create: {resp:?}");
        let id = resp.body["id"].as_str().unwrap().to_string();
        // 模拟重启：丢弃旧 handler（旧 Connection 关闭），用同路径新建
        drop(h);
        let h2 = ApiGatewayRouteHandler::with_db_path(&path);
        // seed_if_empty 见表非空不再 seed，新渠道应在
        let resp = h2
            .handle(get_req("/api/v1/gateway/channels"))
            .await
            .unwrap();
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 1, "重启后应仅 1 个渠道（新建的）: {arr:?}");
        assert_eq!(arr[0]["id"], id, "重启后渠道 id 应一致");
        assert_eq!(arr[0]["name"], "持久渠道");
        // 清理临时文件
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path}-wal"));
        let _ = std::fs::remove_file(format!("{path}-shm"));
    }

    #[tokio::test]
    async fn logs_pagination_limit_enforced() {
        // (b) logs 分页 LIMIT 生效：插 5 条日志，limit=2 只返回 2 条
        let h = ApiGatewayRouteHandler::with_empty();
        {
            let conn = h.db.lock().unwrap();
            for i in 0..5 {
                insert_log(
                    &conn,
                    &CallLog {
                        id: format!("log-p{i}"),
                        token_id: "tok-1".into(),
                        token_name: "t".into(),
                        channel_id: "ch-1".into(),
                        channel_name: "c".into(),
                        model: "m".into(),
                        prompt_tokens: 1,
                        completion_tokens: 1,
                        total_tokens: 2,
                        latency_ms: 10,
                        status: "success".into(),
                        error: None,
                        // created_at 递增，确保 ORDER BY 后顺序稳定（最新在前）
                        created_at: format!("2026-08-08T10:0{i}:00+08:00"),
                    },
                )
                .unwrap();
            }
        }
        let resp = h
            .handle(get_req("/api/v1/gateway/logs?limit=2"))
            .await
            .unwrap();
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 2, "limit=2 应只返回 2 条: {arr:?}");
        // 最新在前：log-p4（最新）应在首位
        assert_eq!(arr[0]["id"], "log-p4", "最新日志应在前");
    }

    // ---- 计费倍率体系测试 ----

    #[test]
    fn calc_cost_basic_multiplication() {
        // 1000 tokens × 1.5 × 0.8 = 1200
        assert_eq!(calc_cost(1000, 1.5, 0.8), 1200);
    }

    #[test]
    fn calc_cost_rounds_up() {
        // 101 × 0.75 × 0.8 = 60.6 → ceil 61
        assert_eq!(calc_cost(101, 0.75, 0.8), 61);
        // 1 × 0.1 × 0.1 = 0.01 → ceil 1（至少 1，非零向上取整）
        assert_eq!(calc_cost(1, 0.1, 0.1), 1);
        // 0 tokens → 0
        assert_eq!(calc_cost(0, 15.0, 2.0), 0);
        // 负倍率 → 0（不下溢）
        assert_eq!(calc_cost(100, -1.0, 1.0), 0);
    }

    #[test]
    fn generate_redeem_code_has_correct_format() {
        let c = generate_redeem_code();
        // "REDEEM-XXXX-XXXX" = REDEEM-(7) + 4 + -(1) + 4 = 16 字符
        assert!(c.starts_with("REDEEM-"), "前缀应为 REDEEM-: {c}");
        assert_eq!(c.len(), 16, "长度应为 16: {c}");
        let rest = &c[7..];
        let parts: Vec<&str> = rest.split('-').collect();
        assert_eq!(parts.len(), 2, "应有 2 段: {parts:?}");
        assert_eq!(parts[0].len(), 4, "第一段 4 字符: {parts:?}");
        assert_eq!(parts[1].len(), 4, "第二段 4 字符: {parts:?}");
        // 字符应是大写 hex（数字 0-9 或大写 A-F）
        assert!(
            parts[0]
                .chars()
                .all(|c| c.is_ascii_digit() || ('A'..='F').contains(&c)),
            "应为大写 hex: {parts:?}"
        );
        assert!(
            parts[1]
                .chars()
                .all(|c| c.is_ascii_digit() || ('A'..='F').contains(&c)),
            "应为大写 hex: {parts:?}"
        );
    }

    #[test]
    fn generate_redeem_code_produces_varied_output() {
        let c1 = generate_redeem_code();
        let c2 = generate_redeem_code();
        assert_ne!(c1, c2, "连续调用应不同");
    }

    #[tokio::test]
    async fn model_ratios_crud() {
        let h = ApiGatewayRouteHandler::with_demo_data();
        // seed 后应含默认 gpt-4o
        let resp = h
            .handle(get_req("/api/v1/gateway/model-ratios"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().unwrap();
        assert!(
            arr.iter()
                .any(|m| m["model"] == "gpt-4o" && m["ratio"] == 15.0),
            "应含 seed gpt-4o=15.0: {arr:?}"
        );
        // 设置/更新（admin）
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/model-ratios",
                serde_json::json!({"model": "new-model", "ratio": 2.5}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "set model ratio: {resp:?}");
        assert_eq!(resp.body["model"], "new-model");
        assert_eq!(resp.body["ratio"], 2.5);
        // 列表含新建
        let resp = h
            .handle(get_req("/api/v1/gateway/model-ratios"))
            .await
            .unwrap();
        let arr = resp.body.as_array().unwrap();
        assert!(
            arr.iter()
                .any(|m| m["model"] == "new-model" && m["ratio"] == 2.5),
            "列表应含 new-model: {arr:?}"
        );
        // 删除
        let resp = h
            .handle(del_req("/api/v1/gateway/model-ratios/new-model"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        // 删除后不含
        let resp = h
            .handle(get_req("/api/v1/gateway/model-ratios"))
            .await
            .unwrap();
        let arr = resp.body.as_array().unwrap();
        assert!(
            !arr.iter().any(|m| m["model"] == "new-model"),
            "删除后应不含 new-model"
        );
        // 删除不存在 → 404
        let resp = h
            .handle(del_req("/api/v1/gateway/model-ratios/never-exists"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn group_ratios_set_and_list() {
        let h = ApiGatewayRouteHandler::with_demo_data();
        // seed 后应含 default/vip/trial
        let resp = h
            .handle(get_req("/api/v1/gateway/group-ratios"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().unwrap();
        assert!(
            arr.iter()
                .any(|g| g["group_name"] == "vip" && g["ratio"] == 0.8),
            "应含 seed vip=0.8: {arr:?}"
        );
        // 设置新组
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/group-ratios",
                serde_json::json!({"group_name": "svip", "ratio": 0.5}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["group_name"], "svip");
        assert_eq!(resp.body["ratio"], 0.5);
        // 更新已有（vip → 0.7）
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/group-ratios",
                serde_json::json!({"group_name": "vip", "ratio": 0.7}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.body["ratio"], 0.7);
        // 列表反映更新
        let resp = h
            .handle(get_req("/api/v1/gateway/group-ratios"))
            .await
            .unwrap();
        let arr = resp.body.as_array().unwrap();
        let vip = arr.iter().find(|g| g["group_name"] == "vip").unwrap();
        assert_eq!(vip["ratio"], 0.7, "vip 应更新为 0.7");
        // 负值拒绝
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/group-ratios",
                serde_json::json!({"group_name": "bad", "ratio": -1.0}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    #[tokio::test]
    async fn redeem_valid_code_increases_quota() {
        let h = ApiGatewayRouteHandler::with_empty();
        // 建一个 token，已用 500 配额
        {
            let conn = h.db.lock().unwrap();
            insert_token(
                &conn,
                &ApiToken {
                    id: "tok-r".into(),
                    name: "兑换测试".into(),
                    key: "sk-os-redeem".into(),
                    status: "active".into(),
                    enabled: true,
                    quota_limit: 10_000,
                    quota_used: 500,
                    allowed_models: vec![],
                    allowed_channels: vec![],
                    group_name: "default".into(),
                    billing_mode: "per_token".into(),
                    expires_at: None,
                    created_at: String::new(),
                    last_used: None,
                    request_count: 0,
                },
            )
            .unwrap();
            // 直接插一张可用兑换码（额度 200）
            insert_redeem_code(
                &conn,
                &RedeemCode {
                    code: "REDEEM-TEST-0001".into(),
                    quota_amount: 200,
                    used_by: None,
                    used_at: None,
                    created_at: String::new(),
                    expires_at: None,
                },
            )
            .unwrap();
        }
        // 兑换
        let resp = h
            .handle(post_req_with_auth(
                "/api/v1/gateway/redeem",
                serde_json::json!({"code": "REDEEM-TEST-0001"}),
                "sk-os-redeem",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "兑换应成功: {resp:?}");
        assert_eq!(resp.body["added_quota"], 200);
        // quota_used 应从 500 → 300（扣减 200）
        let tokens = h.tokens_snapshot();
        let t = tokens.iter().find(|t| t.id == "tok-r").unwrap();
        assert_eq!(t.quota_used, 300, "兑换后 quota_used 应为 300");
    }

    #[tokio::test]
    async fn redeem_already_used_code_rejected() {
        let h = ApiGatewayRouteHandler::with_empty();
        {
            let conn = h.db.lock().unwrap();
            insert_token(
                &conn,
                &ApiToken {
                    id: "tok-r2".into(),
                    name: "兑换".into(),
                    key: "sk-os-redeem2".into(),
                    status: "active".into(),
                    enabled: true,
                    quota_limit: 0,
                    quota_used: 100,
                    allowed_models: vec![],
                    allowed_channels: vec![],
                    group_name: "default".into(),
                    billing_mode: "per_token".into(),
                    expires_at: None,
                    created_at: String::new(),
                    last_used: None,
                    request_count: 0,
                },
            )
            .unwrap();
            // 已被使用的码
            insert_redeem_code(
                &conn,
                &RedeemCode {
                    code: "REDEEM-USED-0001".into(),
                    quota_amount: 200,
                    used_by: Some("tok-other".into()),
                    used_at: Some("2026-08-01T00:00:00+08:00".into()),
                    created_at: String::new(),
                    expires_at: None,
                },
            )
            .unwrap();
        }
        let resp = h
            .handle(post_req_with_auth(
                "/api/v1/gateway/redeem",
                serde_json::json!({"code": "REDEEM-USED-0001"}),
                "sk-os-redeem2",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 409, "已用码应 409: {resp:?}");
        // 配额不应变化（仍 100）
        let tokens = h.tokens_snapshot();
        let t = tokens.iter().find(|t| t.id == "tok-r2").unwrap();
        assert_eq!(t.quota_used, 100, "已用码兑换后配额不应变");
    }

    #[tokio::test]
    async fn redeem_unknown_code_returns_404() {
        let h = ApiGatewayRouteHandler::with_empty();
        {
            let conn = h.db.lock().unwrap();
            insert_token(
                &conn,
                &ApiToken {
                    id: "tok-r3".into(),
                    name: "兑换".into(),
                    key: "sk-os-redeem3".into(),
                    status: "active".into(),
                    enabled: true,
                    quota_limit: 0,
                    quota_used: 10,
                    allowed_models: vec![],
                    allowed_channels: vec![],
                    group_name: "default".into(),
                    billing_mode: "per_token".into(),
                    expires_at: None,
                    created_at: String::new(),
                    last_used: None,
                    request_count: 0,
                },
            )
            .unwrap();
        }
        let resp = h
            .handle(post_req_with_auth(
                "/api/v1/gateway/redeem",
                serde_json::json!({"code": "REDEEM-NOPE-0000"}),
                "sk-os-redeem3",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404, "不存在码应 404");
    }

    #[tokio::test]
    async fn create_redeem_codes_admin_endpoint() {
        let h = ApiGatewayRouteHandler::with_empty();
        // 生成 3 张兑换码（admin）
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/redeem-codes",
                serde_json::json!({"quota_amount": 500, "count": 3}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "生成兑换码: {resp:?}");
        assert_eq!(resp.body["count"], 3);
        let created = resp.body["created"].as_array().unwrap();
        assert_eq!(created.len(), 3);
        // 每张都应是 REDEEM-XXXX-XXXX 格式
        for c in created {
            let code = c["code"].as_str().unwrap();
            assert!(
                code.starts_with("REDEEM-") && code.len() == 16,
                "格式: {code}"
            );
            assert_eq!(c["quota_amount"], 500);
        }
        // 列表（admin）含 3 张
        let resp = h
            .handle(get_req("/api/v1/gateway/redeem-codes"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body.as_array().unwrap().len(), 3);
    }

    #[test]
    fn get_model_ratio_defaults_to_one_when_missing() {
        let h = ApiGatewayRouteHandler::with_empty();
        let conn = h.db.lock().unwrap();
        // 未配置的模型 → 默认 1.0
        assert_eq!(get_model_ratio(&conn, "never-configured"), 1.0);
        // 配置后取配置值
        upsert_model_ratio(
            &conn,
            &ModelRatio {
                model: "gpt-x".into(),
                ratio: 12.5,
                updated_at: String::new(),
            },
        )
        .unwrap();
        assert_eq!(get_model_ratio(&conn, "gpt-x"), 12.5);
    }

    #[test]
    fn get_group_ratio_defaults_to_one_when_missing() {
        let h = ApiGatewayRouteHandler::with_empty();
        let conn = h.db.lock().unwrap();
        assert_eq!(get_group_ratio(&conn, "unknown-group"), 1.0);
        upsert_group_ratio(
            &conn,
            &GroupRatio {
                group_name: "vip".into(),
                ratio: 0.8,
                updated_at: String::new(),
            },
        )
        .unwrap();
        assert_eq!(get_group_ratio(&conn, "vip"), 0.8);
    }

    #[tokio::test]
    async fn seed_default_ratios_present_in_demo_data() {
        // with_demo_data seed 后应含完整默认倍率表
        let h = ApiGatewayRouteHandler::with_demo_data();
        let resp = h
            .handle(get_req("/api/v1/gateway/model-ratios"))
            .await
            .unwrap();
        let models = resp.body.as_array().unwrap();
        // 至少 9 个默认模型
        assert!(models.len() >= 9, "应含至少 9 个默认模型倍率: {models:?}");
        assert!(
            models
                .iter()
                .any(|m| m["model"] == "gpt-4o"
                    && (m["ratio"].as_f64().unwrap() - 15.0).abs() < 1e-9),
            "gpt-4o 应为 15.0"
        );
        let resp = h
            .handle(get_req("/api/v1/gateway/group-ratios"))
            .await
            .unwrap();
        let groups = resp.body.as_array().unwrap();
        assert_eq!(groups.len(), 3, "应含 default/vip/trial 三组: {groups:?}");
    }

    #[tokio::test]
    async fn token_default_group_name_is_default() {
        // 创建令牌不带 group_name → 默认 "default"
        let h = ApiGatewayRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/tokens",
                serde_json::json!({"name": "g"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["group_name"], "default");
        // 带 group_name → 用指定值
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/tokens",
                serde_json::json!({"name": "v", "group_name": "vip"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.body["group_name"], "vip");
    }

    // ====================================================================
    // 计费模式（billing_mode）与加密货币充值（PaymentOrder）新增测试
    // ====================================================================

    /// 测试用 ApiToken 字面量辅助（billing_mode 可指定，其余合理默认）。
    fn test_token(
        id: &str,
        key: &str,
        billing_mode: &str,
        quota_limit: u64,
        quota_used: u64,
    ) -> ApiToken {
        ApiToken {
            id: id.into(),
            name: format!("token-{id}"),
            key: key.into(),
            status: "active".into(),
            enabled: true,
            quota_limit,
            quota_used,
            allowed_models: vec![],
            allowed_channels: vec![],
            group_name: "default".into(),
            billing_mode: billing_mode.into(),
            expires_at: None,
            created_at: String::new(),
            last_used: None,
            request_count: 0,
        }
    }

    // ---- 1. billing_mode 默认值兼容旧 JSON ----

    #[test]
    fn billing_mode_serde_default_compat_old_json() {
        // 旧持久化 JSON 无 billing_mode 字段 → 反序列化默认 per_token（不炸）
        let old_json = serde_json::json!({
            "id": "tok-old",
            "name": "旧令牌",
            "key": "sk-os-oldkey123456",
            "status": "active",
            "enabled": true,
            "quota_limit": 1000,
            "quota_used": 10,
            "allowed_models": [],
            "allowed_channels": [],
            "group_name": "default",
            "expires_at": null,
            "created_at": "2026-01-01T00:00:00+08:00",
            "last_used": null,
            "request_count": 0
        });
        let t: ApiToken = serde_json::from_value(old_json).expect("旧 JSON 应能反序列化");
        assert_eq!(t.billing_mode, "per_token", "缺省应回落 per_token");
        // 显式给出则保留
        let v = serde_json::to_value(test_token("t", "k", "credits", 1, 0)).unwrap();
        let t2: ApiToken = serde_json::from_value(v).unwrap();
        assert_eq!(t2.billing_mode, "credits");
    }

    #[tokio::test]
    async fn billing_mode_db_migration_defaults_per_token() {
        // 老库迁移：tokens 表无 billing_mode 列 → with_db_path 建连时补列，
        // 旧行读出 per_token
        let path = unique_temp_db_path();
        {
            // 手工构造"老版本"库：只有无 billing_mode 的 tokens 表 + 一行数据
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE tokens (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    key TEXT NOT NULL,
                    status TEXT NOT NULL,
                    enabled INTEGER NOT NULL,
                    quota_limit INTEGER NOT NULL,
                    quota_used INTEGER NOT NULL,
                    allowed_models TEXT NOT NULL,
                    allowed_channels TEXT NOT NULL,
                    group_name TEXT NOT NULL DEFAULT 'default',
                    expires_at TEXT,
                    created_at TEXT NOT NULL,
                    last_used TEXT,
                    request_count INTEGER NOT NULL
                );
                INSERT INTO tokens VALUES
                ('tok-old','旧','sk-os-legacykey99','active',1,1000,50,'[]','[]','default',NULL,'2026-01-01',NULL,3);",
            )
            .unwrap();
        }
        // 打开即迁移（create_schema → migrate_add_billing_mode）
        let h = ApiGatewayRouteHandler::with_db_path(&path);
        let tokens = h.tokens_snapshot();
        let t = tokens
            .iter()
            .find(|t| t.id == "tok-old")
            .expect("旧令牌应在");
        assert_eq!(t.billing_mode, "per_token", "迁移后旧行应回落 per_token");
        assert_eq!(t.quota_used, 50, "旧数据其他字段不受迁移影响");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path}-wal"));
        let _ = std::fs::remove_file(format!("{path}-shm"));
    }

    // ---- 2. 创建令牌的 billing_mode / initial_credits ----

    #[tokio::test]
    async fn create_token_billing_modes_and_validation() {
        let h = ApiGatewayRouteHandler::with_empty();
        // 缺省 → per_token（向后兼容）
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/tokens",
                serde_json::json!({"name": "d"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["billing_mode"], "per_token");
        // free / per_image
        for mode in &["free", "per_image"] {
            let resp = h
                .handle(post_req(
                    "/api/v1/gateway/tokens",
                    serde_json::json!({"name": mode, "billing_mode": mode}),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 201, "{mode} 创建应成功");
            assert_eq!(resp.body["billing_mode"], *mode);
        }
        // credits + 初始积分 → 写进 quota_limit
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/tokens",
                serde_json::json!({"name": "c", "billing_mode": "credits", "initial_credits": 5000}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["billing_mode"], "credits");
        assert_eq!(resp.body["quota_limit"], 5000, "初始积分应写入 quota_limit");
        assert_eq!(resp.body["quota_used"], 0);
        // 非法值 → 400
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/tokens",
                serde_json::json!({"name": "bad", "billing_mode": "monthly"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "非法 billing_mode 应 400");
    }

    // ---- 3. 转发计费分流 ----

    #[tokio::test]
    async fn free_mode_skips_quota_check_and_charge() {
        let h = ApiGatewayRouteHandler::with_empty();
        {
            let conn = h.db.lock().unwrap();
            // free 令牌：配额已"耗尽"（used=100 >= limit=10），但 free 不检查
            insert_token(
                &conn,
                &test_token("tok-free", "sk-os-free", "free", 10, 100),
            )
            .unwrap();
        }
        // 转发：应越过配额门（无 429），因无渠道 → 404（降级不 panic）
        let resp = h
            .handle(post_req_with_auth(
                "/api/v1/gateway/v1/chat/completions",
                serde_json::json!({"model": "gpt-4o", "messages": []}),
                "sk-os-free",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404, "free 应跳过配额检查（非 429）: {resp:?}");
        // record_success 也不扣费（quota_used 不变，仅计数 +1）
        let token = h
            .tokens_snapshot()
            .into_iter()
            .find(|t| t.id == "tok-free")
            .unwrap();
        let ch = Channel {
            id: "ch-x".into(),
            name: "X".into(),
            provider: "openai".into(),
            base_url: "http://x".into(),
            api_key: String::new(),
            models: vec![],
            priority: 0,
            weight: 1,
            status: "enabled".into(),
            enabled: true,
            created_at: String::new(),
            last_used: None,
            request_count: 0,
            via_node: String::new(),
            price_per_call: 0.0,
            price_per_sec: 0.0,
            price_per_token: 0.0,
        };
        h.record_success(&token, &ch, "gpt-4o", 100, 100, 200, 5, None);
        let t = h
            .tokens_snapshot()
            .into_iter()
            .find(|t| t.id == "tok-free")
            .unwrap();
        assert_eq!(t.quota_used, 100, "free 模式 record_success 不应扣费");
        assert_eq!(t.request_count, 1, "free 模式仍累计请求计数");
    }

    #[tokio::test]
    async fn per_token_mode_charges_on_relay_success_as_before() {
        let h = ApiGatewayRouteHandler::with_empty();
        {
            let conn = h.db.lock().unwrap();
            insert_token(
                &conn,
                &test_token("tok-pt", "sk-os-pt", "per_token", 1_000_000, 0),
            )
            .unwrap();
        }
        let token = h
            .tokens_snapshot()
            .into_iter()
            .find(|t| t.id == "tok-pt")
            .unwrap();
        let ch = Channel {
            id: "ch-x".into(),
            name: "X".into(),
            provider: "openai".into(),
            base_url: "http://x".into(),
            api_key: String::new(),
            models: vec![],
            priority: 0,
            weight: 1,
            status: "enabled".into(),
            enabled: true,
            created_at: String::new(),
            last_used: None,
            request_count: 0,
            via_node: String::new(),
            price_per_call: 0.0,
            price_per_sec: 0.0,
            price_per_token: 0.0,
        };
        // total_tokens=200，未配置倍率 → model_ratio=1.0、group_ratio=1.0 → 扣 200
        h.record_success(
            &token,
            &ch,
            "never-configured-model",
            100,
            100,
            200,
            5,
            None,
        );
        let t = h
            .tokens_snapshot()
            .into_iter()
            .find(|t| t.id == "tok-pt")
            .unwrap();
        assert_eq!(t.quota_used, 200, "per_token 应照旧按 token 计量扣费");
    }

    #[tokio::test]
    async fn credits_mode_exhausted_rejected_but_remaining_passes() {
        let h = ApiGatewayRouteHandler::with_empty();
        {
            let conn = h.db.lock().unwrap();
            // 积分耗尽（used >= limit）→ 沿用现有配额拒绝语义 429
            insert_token(
                &conn,
                &test_token("tok-c1", "sk-os-cred1", "credits", 100, 100),
            )
            .unwrap();
            // 还有余量（99 < 100）→ 越过配额门（无渠道 → 404）
            insert_token(
                &conn,
                &test_token("tok-c2", "sk-os-cred2", "credits", 100, 99),
            )
            .unwrap();
        }
        let resp = h
            .handle(post_req_with_auth(
                "/api/v1/gateway/v1/chat/completions",
                serde_json::json!({"model": "gpt-4o", "messages": []}),
                "sk-os-cred1",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 429, "积分耗尽应 429（沿用现有语义）");
        let resp = h
            .handle(post_req_with_auth(
                "/api/v1/gateway/v1/chat/completions",
                serde_json::json!({"model": "gpt-4o", "messages": []}),
                "sk-os-cred2",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404, "有余量应放行（非 429，无渠道 → 404）");
    }

    // ---- 4. 生图扣费（try_charge_image：查-检-扣单锁原子，2026-08-20 接线） ----

    #[tokio::test]
    async fn try_charge_image_free_mode_skips_charge() {
        let h = ApiGatewayRouteHandler::with_empty();
        {
            let conn = h.db.lock().unwrap();
            insert_token(&conn, &test_token("tok-f", "sk-os-f", "free", 0, 0)).unwrap();
        }
        let out = h.try_charge_image("sk-os-f").await.unwrap();
        assert!(!out.charged, "free 模式不扣费");
        assert_eq!(out.token_name, "token-tok-f", "token_name 供归因");
        let t = h
            .tokens_snapshot()
            .into_iter()
            .find(|t| t.id == "tok-f")
            .unwrap();
        assert_eq!(t.quota_used, 0, "quota_used 不应变");
    }

    #[tokio::test]
    async fn try_charge_image_per_image_deducts_100_accumulating() {
        let h = ApiGatewayRouteHandler::with_empty();
        {
            let conn = h.db.lock().unwrap();
            insert_token(
                &conn,
                &test_token("tok-img", "sk-os-img", "per_image", 1_000_000, 0),
            )
            .unwrap();
        }
        let out = h.try_charge_image("sk-os-img").await.unwrap();
        assert!(out.charged, "per_image 应扣费");
        assert_eq!(out.token_name, "token-tok-img");
        let t = h
            .tokens_snapshot()
            .into_iter()
            .find(|t| t.id == "tok-img")
            .unwrap();
        assert_eq!(t.quota_used, IMAGE_PRICE_CREDITS, "quota_used 应 +100");
        // 再扣一次叠加（同令牌两次生图）
        let _ = h.try_charge_image("sk-os-img").await.unwrap();
        let t = h
            .tokens_snapshot()
            .into_iter()
            .find(|t| t.id == "tok-img")
            .unwrap();
        assert_eq!(t.quota_used, IMAGE_PRICE_CREDITS * 2, "两次生图应扣 200");
        // 计数 / last_used 同步累计（与转发成功同款语义）
        assert_eq!(t.request_count, 2);
        assert!(t.last_used.is_some(), "last_used 应被记录");
    }

    #[tokio::test]
    async fn try_charge_image_insufficient_quota_errs_with_marker_and_boundary() {
        let h = ApiGatewayRouteHandler::with_empty();
        {
            let conn = h.db.lock().unwrap();
            // 剩 50 积分 < 单价 100 → 拒
            insert_token(
                &conn,
                &test_token("tok-low", "sk-os-low", "per_image", 150, 100),
            )
            .unwrap();
            // 恰好 100 积分 = 单价 → 放行（边界：剩 ≥ 单价即可）
            insert_token(
                &conn,
                &test_token("tok-fit", "sk-os-fit", "credits", 100, 0),
            )
            .unwrap();
        }
        let err = h
            .try_charge_image("sk-os-low")
            .await
            .expect_err("余额不足应 Err");
        assert!(
            err.contains(IMAGE_CHARGE_INSUFFICIENT_MARKER),
            "余额不足文案须含哨兵（402 判定依据）: {err}"
        );
        assert!(err.contains("50"), "应带剩余积分: {err}");
        let t = h
            .tokens_snapshot()
            .into_iter()
            .find(|t| t.id == "tok-low")
            .unwrap();
        assert_eq!(t.quota_used, 100, "拒绝时不得预扣");
        // 边界：恰好够 → 扣满
        let out = h.try_charge_image("sk-os-fit").await.unwrap();
        assert!(out.charged);
        let t = h
            .tokens_snapshot()
            .into_iter()
            .find(|t| t.id == "tok-fit")
            .unwrap();
        assert_eq!(t.quota_used, 100, "恰好够应扣到满");
        // 再扣 → 余额不足
        let err = h
            .try_charge_image("sk-os-fit")
            .await
            .expect_err("扣满后再扣应 Err");
        assert!(err.contains(IMAGE_CHARGE_INSUFFICIENT_MARKER));
    }

    #[tokio::test]
    async fn try_charge_image_unknown_or_disabled_key_errs_without_charge() {
        let h = ApiGatewayRouteHandler::with_empty();
        // 未知 key（sk-os- 前缀但令牌表未命中）
        let err = h
            .try_charge_image("sk-os-bogus")
            .await
            .expect_err("未知 key 应 Err");
        assert!(err.contains("无效的 API Key"), "应说明未命中: {err}");
        assert!(!err.contains(IMAGE_CHARGE_INSUFFICIENT_MARKER));
        // 已禁用令牌
        {
            let conn = h.db.lock().unwrap();
            let mut t = test_token("tok-off", "sk-os-off", "per_image", 1_000, 0);
            t.enabled = false;
            t.status = "disabled".into();
            insert_token(&conn, &t).unwrap();
        }
        let err = h
            .try_charge_image("sk-os-off")
            .await
            .expect_err("禁用令牌应 Err");
        assert!(err.contains("已禁用"), "应说明禁用: {err}");
        assert!(h.tokens_snapshot().iter().all(|t| t.quota_used == 0));
    }

    #[tokio::test]
    async fn try_charge_image_concurrent_never_overcharges() {
        // 余额恰够 1 张图（limit=100, used=0）：8 路并发抢扣 → 恰 1 个成功，
        // 其余 Err（余额不足）；最终 quota_used == 100（不超 limit）。
        // 原子性来自单实例 Mutex<Connection>（查-检-扣同一锁内），这正是
        // media-gen 必须与 api_gateway 组件共享同一实例的原因。
        let h = std::sync::Arc::new(ApiGatewayRouteHandler::with_empty());
        {
            let conn = h.db.lock().unwrap();
            insert_token(
                &conn,
                &test_token("tok-race", "sk-os-race", "per_image", 100, 0),
            )
            .unwrap();
        }
        let mut jobs = Vec::new();
        for _ in 0..8 {
            let h2 = h.clone();
            jobs.push(tokio::spawn(async move {
                h2.try_charge_image("sk-os-race")
                    .await
                    .is_ok_and(|o| o.charged)
            }));
        }
        let mut wins = 0;
        for j in jobs {
            if j.await.unwrap() {
                wins += 1;
            }
        }
        assert_eq!(wins, 1, "余额只够 1 张图，应恰 1 个成功");
        let t = h
            .tokens_snapshot()
            .into_iter()
            .find(|t| t.id == "tok-race")
            .unwrap();
        assert_eq!(t.quota_used, IMAGE_PRICE_CREDITS, "扣费总额不超 limit");
    }

    // ---- 5. 价目换算 ----

    #[test]
    fn crypto_price_conversion_table() {
        // USDT：1000 积分 × 0.01 = "10.00"
        assert_eq!(crypto_amount_for("usdt", 1000).as_deref(), Some("10.00"));
        assert_eq!(crypto_amount_for("usdt", 1).as_deref(), Some("0.01"));
        assert_eq!(crypto_amount_for("usdt", 0).as_deref(), Some("0.00"));
        // BTC：以聪计（1000 × 1500 = 1500000 sat）
        assert_eq!(crypto_amount_for("btc", 1000).as_deref(), Some("1500000"));
        // EVM：以 wei 计（1000 × 20e15 = 2e19 wei）
        assert_eq!(
            crypto_amount_for("evm", 1000).as_deref(),
            Some("20000000000000000000")
        );
        // 未知币种 → None
        assert!(crypto_amount_for("doge", 100).is_none());
        assert!(crypto_amount_for("", 100).is_none());
    }

    // ---- 6. 充值订单 CRUD ----

    /// 建一个可充值的令牌并返回 id（订单需挂到真实令牌上）。
    async fn seeded_token_id(h: &ApiGatewayRouteHandler) -> String {
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/tokens",
                serde_json::json!({"name": "充值目标", "billing_mode": "credits"}),
            ))
            .await
            .unwrap();
        resp.body["id"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn create_payment_order_env_unset_returns_warning() {
        // 未配置收款地址：订单仍创建（pending），address 空串 + warning 提示
        std::env::remove_var("NEXOS_PAY_USDT_ADDR");
        let h = ApiGatewayRouteHandler::with_empty();
        let tok_id = seeded_token_id(&h).await;
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/payments",
                serde_json::json!({"token_id": tok_id, "currency": "usdt", "credits": 1000}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "创建订单: {resp:?}");
        assert_eq!(resp.body["status"], "pending");
        assert_eq!(resp.body["currency"], "usdt");
        assert_eq!(resp.body["amount_crypto"], "10.00", "1000 积分 × 0.01 USDT");
        assert_eq!(resp.body["credits"], 1000);
        assert_eq!(resp.body["address"], "", "未配 env 地址应为空串");
        assert!(
            resp.body["warning"]
                .as_str()
                .unwrap_or("")
                .contains("NEXOS_PAY_USDT_ADDR"),
            "应返回 warning 提示管理员配置: {resp:?}"
        );
        assert_eq!(resp.body["memo"], serde_json::Value::Null, "USDT 无 memo");
    }

    #[tokio::test]
    async fn create_payment_order_env_configured_address() {
        std::env::set_var("NEXOS_PAY_BTC_ADDR", "bc1q-test-recv-addr");
        let h = ApiGatewayRouteHandler::with_empty();
        let tok_id = seeded_token_id(&h).await;
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/payments",
                serde_json::json!({"token_id": tok_id, "currency": "btc", "credits": 2000}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["address"], "bc1q-test-recv-addr", "应读 env 地址");
        assert_eq!(resp.body["amount_crypto"], "3000000", "2000 × 1500 sat");
        assert!(
            resp.body["warning"].is_null(),
            "已配地址不应有 warning: {resp:?}"
        );
        assert!(
            resp.body["memo"].as_str().unwrap_or("").contains("sat"),
            "BTC 订单 memo 应提示聪面额"
        );
        std::env::remove_var("NEXOS_PAY_BTC_ADDR");
    }

    #[tokio::test]
    async fn create_payment_order_validations() {
        let h = ApiGatewayRouteHandler::with_empty();
        let tok_id = seeded_token_id(&h).await;
        // 非法币种 → 400
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/payments",
                serde_json::json!({"token_id": tok_id, "currency": "doge", "credits": 100}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // credits=0 → 400
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/payments",
                serde_json::json!({"token_id": tok_id, "currency": "usdt", "credits": 0}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // 空 token_id → 400
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/payments",
                serde_json::json!({"token_id": "", "currency": "usdt", "credits": 100}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // 不存在的令牌 → 404
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/payments",
                serde_json::json!({"token_id": "tok-nope", "currency": "usdt", "credits": 100}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn confirm_payment_adds_credits_idempotent() {
        let h = ApiGatewayRouteHandler::with_empty();
        let tok_id = seeded_token_id(&h).await;
        // 建单（credits 模式 token 初始 quota_limit=0）
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/payments",
                serde_json::json!({"token_id": tok_id, "currency": "usdt", "credits": 3000}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        let pay_id = resp.body["id"].as_str().unwrap().to_string();
        // 确认 → 加积分 + 状态流转 + 记 txid
        let resp = h
            .handle(post_req(
                &format!("/api/v1/gateway/payments/{pay_id}/confirm"),
                serde_json::json!({"txid": "0xabc123"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "confirm: {resp:?}");
        assert_eq!(resp.body["added_credits"], 3000);
        assert_eq!(resp.body["order"]["status"], "confirmed");
        assert_eq!(resp.body["order"]["txid"], "0xabc123", "应记录 txid");
        assert!(
            resp.body["order"]["confirmed_at"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "应记 confirmed_at"
        );
        let t = h
            .tokens_snapshot()
            .into_iter()
            .find(|t| t.id == tok_id)
            .unwrap();
        assert_eq!(t.quota_limit, 3000, "确认后 quota_limit += credits");
        // 重复确认 → 409，积分不再加（幂等）
        let resp = h
            .handle(post_req(
                &format!("/api/v1/gateway/payments/{pay_id}/confirm"),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 409, "重复 confirm 应拒绝");
        let t = h
            .tokens_snapshot()
            .into_iter()
            .find(|t| t.id == tok_id)
            .unwrap();
        assert_eq!(t.quota_limit, 3000, "重复确认不应再加积分");
        // 不存在的订单 → 404
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/payments/pay-nope/confirm",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn reject_payment_records_reason_and_blocks_confirm() {
        let h = ApiGatewayRouteHandler::with_empty();
        let tok_id = seeded_token_id(&h).await;
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/payments",
                serde_json::json!({"token_id": tok_id, "currency": "evm", "credits": 500}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        let pay_id = resp.body["id"].as_str().unwrap().to_string();
        // 拒绝（带原因）
        let resp = h
            .handle(post_req(
                &format!("/api/v1/gateway/payments/{pay_id}/reject"),
                serde_json::json!({"reason": "未收到款"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "reject: {resp:?}");
        assert_eq!(resp.body["status"], "rejected");
        assert_eq!(resp.body["reject_reason"], "未收到款", "应记原因");
        // 拒后确认 → 409（不能先拒后确认）
        let resp = h
            .handle(post_req(
                &format!("/api/v1/gateway/payments/{pay_id}/confirm"),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 409, "已拒订单不可确认");
        // 重复拒绝 → 409
        let resp = h
            .handle(post_req(
                &format!("/api/v1/gateway/payments/{pay_id}/reject"),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 409, "重复 reject 应拒绝");
        // 不加积分
        let t = h
            .tokens_snapshot()
            .into_iter()
            .find(|t| t.id == tok_id)
            .unwrap();
        assert_eq!(t.quota_limit, 0, "被拒订单不应加积分");
        // 拒绝不填原因 → 记默认占位
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/payments",
                serde_json::json!({"token_id": tok_id, "currency": "usdt", "credits": 100}),
            ))
            .await
            .unwrap();
        let pay2 = resp.body["id"].as_str().unwrap().to_string();
        let resp = h
            .handle(post_req(
                &format!("/api/v1/gateway/payments/{pay2}/reject"),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert!(
            resp.body["reject_reason"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "未填原因应记默认占位"
        );
    }

    #[tokio::test]
    async fn list_payments_filters_by_status() {
        let h = ApiGatewayRouteHandler::with_empty();
        let tok_id = seeded_token_id(&h).await;
        // 两单：一单确认、一单保持 pending
        let mut ids = Vec::new();
        for cur in &["usdt", "btc"] {
            let resp = h
                .handle(post_req(
                    "/api/v1/gateway/payments",
                    serde_json::json!({"token_id": tok_id, "currency": cur, "credits": 100}),
                ))
                .await
                .unwrap();
            ids.push(resp.body["id"].as_str().unwrap().to_string());
        }
        let resp = h
            .handle(post_req(
                &format!("/api/v1/gateway/payments/{}/confirm", ids[0]),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        // 无过滤 → 2
        let resp = h.handle(get_req("/api/v1/gateway/payments")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body.as_array().unwrap().len(), 2);
        // ?status=pending → 1（且是未确认那单）
        let resp = h
            .handle(get_req("/api/v1/gateway/payments?status=pending"))
            .await
            .unwrap();
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 1, "pending 过滤: {arr:?}");
        assert_eq!(arr[0]["id"], ids[1]);
        // ?status=confirmed → 1
        let resp = h
            .handle(get_req("/api/v1/gateway/payments?status=confirmed"))
            .await
            .unwrap();
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 1, "confirmed 过滤");
        assert_eq!(arr[0]["id"], ids[0]);
        // ?status=rejected → 0
        let resp = h
            .handle(get_req("/api/v1/gateway/payments?status=rejected"))
            .await
            .unwrap();
        assert_eq!(resp.body.as_array().unwrap().len(), 0);
    }

    #[test]
    fn parse_query_str_works() {
        assert_eq!(
            parse_query_str("status=pending", "status").as_deref(),
            Some("pending")
        );
        assert_eq!(
            parse_query_str("a=1&status=confirmed", "status").as_deref(),
            Some("confirmed")
        );
        assert_eq!(parse_query_str("limit=10", "status"), None);
        assert_eq!(parse_query_str("status=", "status"), None, "空值视为缺失");
        assert_eq!(parse_query_str("", "status"), None);
    }

    // ==========================================================================
    // 链上支付验真（dApp 一期接线，2026-08-31）
    //
    // 测试策略：与 os-nexhub 同款——不触网，经 os_nexhub::nexhub_lobby 的
    // ChainPayGate 注入固定 VerifyOutcome 替身（EvmTxVerifier 接缝），断言
    // PaymentOrder confirm 的接线语义（放行/拒绝/降级/标注/开关回旧/非 EVM 跳过）。
    // 订单收款地址经 env NEXOS_PAY_EVM_ADDR 注入（与既有 payment 测试同款 env
    // set/remove 范式）。
    // ==========================================================================

    /// 计数替身执行器：恒返回固定 outcome，并记录调用次数。
    struct GwCountingVerifier {
        outcome: os_nexhub::chain_verify::VerifyOutcome,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl os_nexhub::nexhub_lobby::EvmTxVerifier for GwCountingVerifier {
        fn verify(
            &self,
            _rpc_urls: &[String],
            _proof: &os_nexhub::chain_verify::TxProof,
            _timeout: std::time::Duration,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = os_nexhub::chain_verify::VerifyOutcome> + Send>,
        > {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let o = self.outcome.clone();
            Box::pin(async move { o })
        }
    }

    /// 构造带核验替身的网关 handler（开关 + 缺省链 ID 注入，绕开 env 竞态）。
    fn gw_with_outcome(
        outcome: os_nexhub::chain_verify::VerifyOutcome,
        enabled: bool,
    ) -> (
        ApiGatewayRouteHandler,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let gate = os_nexhub::nexhub_lobby::ChainPayGate::with_parts(
            enabled,
            None,
            None,
            Some(11155111),
            std::time::Duration::from_secs(1),
            None,
            6,
            std::sync::Arc::new(GwCountingVerifier {
                outcome,
                calls: calls.clone(),
            }),
        );
        (
            ApiGatewayRouteHandler::with_empty().with_chain_verify(gate),
            calls,
        )
    }

    /// 直接落一张 evm 订单（收款地址入参固定）——绕开 env `NEXOS_PAY_EVM_ADDR`
    /// 的并行测试竞态（建单端点的 env 读取已由既有 payment 测试覆盖）；金额
    /// = 500 积分 × 20e15 = "10000000000000000000" wei（与 crypto_amount_for
    /// 价目一致，18 位小数假设）。订单 id 派生自每测新建的令牌 id，天然唯一。
    async fn seeded_evm_order_at(h: &ApiGatewayRouteHandler, address: &str) -> String {
        let tok_id = seeded_token_id(h).await;
        let pay_id = format!("pay-{tok_id}");
        let order = PaymentOrder {
            id: pay_id.clone(),
            token_id: tok_id,
            currency: "evm".into(),
            amount_crypto: "10000000000000000000".into(),
            credits: 500,
            address: address.to_string(),
            memo: None,
            status: "pending".into(),
            txid: None,
            created_at: now_iso(),
            confirmed_at: None,
            reject_reason: None,
            chain_block: None,
            chain_value_wei: None,
        };
        {
            let conn = h.db.lock().expect("db poisoned");
            insert_payment_order(&conn, &order).expect("落订单必成功");
        }
        pay_id
    }

    /// 标准收款地址的 evm 订单（500 积分）。
    async fn seeded_evm_order(h: &ApiGatewayRouteHandler) -> String {
        seeded_evm_order_at(h, "0xgw-recv-addr").await
    }

    async fn confirm(
        h: &ApiGatewayRouteHandler,
        pay_id: &str,
        body: serde_json::Value,
    ) -> ApiResponse {
        h.handle(post_req(
            &format!("/api/v1/gateway/payments/{pay_id}/confirm"),
            body,
        ))
        .await
        .unwrap()
    }

    // GW-CV1. evm 订单 + txid + Verified → 确认 200 + chain_verify 标注 +
    //        链上事实落库（订单行 chain_block/chain_value_wei）+ 积分入账
    #[tokio::test]
    async fn confirm_evm_verified_persists_chain_facts() {
        let (h, calls) = gw_with_outcome(
            os_nexhub::chain_verify::VerifyOutcome::Verified {
                block_number: 77,
                to: "0xgw-recv-addr".into(),
                value_wei: "10000000000000000000".into(),
                token: None,
            },
            true,
        );
        let pay_id = seeded_evm_order(&h).await;
        let resp = confirm(&h, &pay_id, serde_json::json!({"txid": "0xreal"})).await;
        assert_eq!(resp.status, 200, "confirm: {resp:?}");
        assert_eq!(resp.body["added_credits"], 500);
        assert_eq!(resp.body["chain_verify"]["status"], "verified");
        assert_eq!(resp.body["chain_verify"]["block_number"], 77);
        assert_eq!(
            resp.body["chain_verify"]["value_wei"],
            "10000000000000000000"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "核验恰一次"
        );
        // 落库：GET /payments 订单行带链上事实
        let resp = h.handle(get_req("/api/v1/gateway/payments")).await.unwrap();
        let order = &resp.body.as_array().unwrap()[0];
        assert_eq!(order["chain_block"], 77, "块高落库: {order:?}");
        assert_eq!(order["chain_value_wei"], "10000000000000000000");
    }

    // GW-CV2. Mismatch → 409 不确认：订单留 pending、积分不加
    #[tokio::test]
    async fn confirm_evm_mismatch_keeps_pending() {
        let (h, _calls) = gw_with_outcome(
            os_nexhub::chain_verify::VerifyOutcome::Mismatch {
                field: "value".into(),
                expect: "10000000000000000000".into(),
                actual: "1".into(),
            },
            true,
        );
        let pay_id = seeded_evm_order(&h).await;
        let quota_before: u64 = h.tokens_snapshot().iter().map(|t| t.quota_limit).sum();
        let resp = confirm(&h, &pay_id, serde_json::json!({"txid": "0xshort"})).await;
        assert_eq!(resp.status, 409, "Mismatch 应拒绝: {resp:?}");
        let err = resp.body["error"].as_str().unwrap_or_default();
        assert!(
            err.contains("value") && err.contains('1'),
            "带字段+链上实际值: {err}"
        );
        let resp = h.handle(get_req("/api/v1/gateway/payments")).await.unwrap();
        assert_eq!(resp.body[0]["status"], "pending", "订单不确认: {resp:?}");
        let quota_after: u64 = h.tokens_snapshot().iter().map(|t| t.quota_limit).sum();
        assert_eq!(quota_after, quota_before, "积分不得入账");
    }

    // GW-CV3. Pending → 409 可重试（不当欺诈），订单留 pending（稍后可重试确认）
    #[tokio::test]
    async fn confirm_evm_pending_retryable() {
        let (h, _calls) = gw_with_outcome(os_nexhub::chain_verify::VerifyOutcome::Pending, true);
        let pay_id = seeded_evm_order(&h).await;
        let resp = confirm(&h, &pay_id, serde_json::json!({"txid": "0xinflight"})).await;
        assert_eq!(resp.status, 409, "Pending 应 409: {resp:?}");
        assert!(
            resp.body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("重试"),
            "应提示稍后重试: {resp:?}"
        );
        let resp = h.handle(get_req("/api/v1/gateway/payments")).await.unwrap();
        assert_eq!(resp.body[0]["status"], "pending", "Pending 不消耗订单");
    }

    // GW-CV4. NotFound → 400
    #[tokio::test]
    async fn confirm_evm_notfound_400() {
        let (h, _calls) = gw_with_outcome(os_nexhub::chain_verify::VerifyOutcome::NotFound, true);
        let pay_id = seeded_evm_order(&h).await;
        let resp = confirm(&h, &pay_id, serde_json::json!({"txid": "0xforged"})).await;
        assert_eq!(resp.status, 400, "NotFound 应 400: {resp:?}");
    }

    // GW-CV5. RpcError → 降级放行：确认 200 + degraded 标注 + 积分入账
    //        （网络故障不阻断交易；admin 仍在环内兜底）
    #[tokio::test]
    async fn confirm_evm_rpc_error_degrades() {
        let (h, _calls) = gw_with_outcome(
            os_nexhub::chain_verify::VerifyOutcome::RpcError {
                detail: "全部 RPC 不可达".into(),
            },
            true,
        );
        let pay_id = seeded_evm_order(&h).await;
        let resp = confirm(&h, &pay_id, serde_json::json!({"txid": "0xmaybe"})).await;
        assert_eq!(resp.status, 200, "降级放行: {resp:?}");
        assert_eq!(resp.body["chain_verify"]["status"], "degraded");
        assert_eq!(resp.body["added_credits"], 500, "降级仍入账");
        let resp = h.handle(get_req("/api/v1/gateway/payments")).await.unwrap();
        assert_eq!(
            resp.body[0]["chain_block"],
            serde_json::Value::Null,
            "降级无链上事实"
        );
    }

    // GW-CV6. 开关关闭 → 完全回旧行为（txid 直接过、无标注、核验零调用）
    #[tokio::test]
    async fn confirm_evm_disabled_falls_back_to_legacy() {
        let (h, calls) = gw_with_outcome(
            os_nexhub::chain_verify::VerifyOutcome::NotFound, // 若误核即拒——证明没核
            false,
        );
        let pay_id = seeded_evm_order(&h).await;
        let resp = confirm(&h, &pay_id, serde_json::json!({"txid": "0xforged"})).await;
        assert_eq!(resp.status, 200, "开关关闭=旧行为: {resp:?}");
        assert!(
            resp.body.get("chain_verify").is_none(),
            "旧行为无任何标注: {resp:?}"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "核验零调用"
        );
    }

    // GW-CV7. usdt 订单带 txid 但未配 ERC-20 合约（gw_with_outcome 不注入合约）
    //        → 不猜合约地址，admin 手动确认 + unverified（二期起 usdt 配了
    //        合约即走 ERC-20 核验，见 GW-CV11/12）
    #[tokio::test]
    async fn confirm_usdt_with_txid_skips_verify() {
        let (h, calls) = gw_with_outcome(
            os_nexhub::chain_verify::VerifyOutcome::NotFound, // 若误核即拒——证明没核
            true,
        );
        let tok_id = seeded_token_id(&h).await;
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/payments",
                serde_json::json!({"token_id": tok_id, "currency": "usdt", "credits": 1000}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        let pay_id = resp.body["id"].as_str().unwrap().to_string();
        let resp = confirm(&h, &pay_id, serde_json::json!({"txid": "0xusdt-tx"})).await;
        assert_eq!(resp.status, 200, "usdt 仍走 admin 手动确认: {resp:?}");
        assert_eq!(resp.body["chain_verify"]["status"], "unverified");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "核验零调用"
        );
    }

    // GW-CV8. evm 订单不带 txid → admin 链下手动确认（unverified），不核验
    #[tokio::test]
    async fn confirm_evm_without_txid_manual() {
        let (h, calls) = gw_with_outcome(os_nexhub::chain_verify::VerifyOutcome::NotFound, true);
        let pay_id = seeded_evm_order(&h).await;
        let resp = confirm(&h, &pay_id, serde_json::Value::Null).await;
        assert_eq!(resp.status, 200, "无 txid=链下判断: {resp:?}");
        assert_eq!(resp.body["chain_verify"]["status"], "unverified");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    // GW-CV9. 收款地址为空（NEXOS_PAY_EVM_ADDR 未配置的订单形态）→
    //        unverified 放行（不静默假装核过；提示配地址）
    #[tokio::test]
    async fn confirm_evm_no_address_marks_unverified() {
        let (h, calls) = gw_with_outcome(
            os_nexhub::chain_verify::VerifyOutcome::Verified {
                block_number: 1,
                to: String::new(),
                value_wei: String::new(),
                token: None,
            },
            true,
        );
        let pay_id = seeded_evm_order_at(&h, "").await;
        let resp = confirm(&h, &pay_id, serde_json::json!({"txid": "0xtx"})).await;
        assert_eq!(resp.status, 200, "缺地址不硬拒: {resp:?}");
        assert_eq!(resp.body["chain_verify"]["status"], "unverified");
        assert!(
            resp.body["chain_verify"]["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("收款地址"),
            "应说明缺收款地址: {resp:?}"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    // GW-CV10. body 显式 chain_id 优先于网关缺省（对准 admin 指定的链核验）
    #[tokio::test]
    async fn confirm_evm_explicit_chain_id_wins() {
        let (h, _calls) = gw_with_outcome(
            os_nexhub::chain_verify::VerifyOutcome::Verified {
                block_number: 5,
                to: "0xgw-recv-addr".into(),
                value_wei: "10000000000000000000".into(),
                token: None,
            },
            true,
        );
        let pay_id = seeded_evm_order(&h).await;
        let resp = confirm(
            &h,
            &pay_id,
            serde_json::json!({"txid": "0xon137", "chain_id": 1337}),
        )
        .await;
        assert_eq!(resp.status, 200);
        assert_eq!(
            resp.body["chain_verify"]["chain_id"], 1337,
            "显式 chain_id 优先"
        );
    }

    // ==========================================================================
    // dApp 二期（2026-09-02）：usdt@EVM ERC-20 核验 + AtLeast 金额规则
    // ==========================================================================

    /// 凭证捕获替身（网关版）：记录收到的 TxProof（断言 erc20 凭证构造与
    /// 金额规则/换算接线），恒返回固定 outcome。
    struct GwProofCaptureVerifier {
        outcome: os_nexhub::chain_verify::VerifyOutcome,
        proof: std::sync::Arc<std::sync::Mutex<Option<os_nexhub::chain_verify::TxProof>>>,
    }

    impl os_nexhub::nexhub_lobby::EvmTxVerifier for GwProofCaptureVerifier {
        fn verify(
            &self,
            _rpc_urls: &[String],
            proof: &os_nexhub::chain_verify::TxProof,
            _timeout: std::time::Duration,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = os_nexhub::chain_verify::VerifyOutcome> + Send>,
        > {
            *self.proof.lock().unwrap() = Some(proof.clone());
            let o = self.outcome.clone();
            Box::pin(async move { o })
        }
    }

    /// 主网 USDT 合约（测试常量）。
    const GW_USDT_CONTRACT: &str = "0xdac17f958d2ee523a2206206994597c13d831ec7";

    /// 带凭证捕获的网关 handler（USDT 合约经网关注入=模拟 env
    /// NEXOS_USDT_EVM_CONTRACT；链 ID 缺省可关——TRON 形态用）。
    fn gw_with_capture(
        outcome: os_nexhub::chain_verify::VerifyOutcome,
        default_chain_id: Option<u64>,
        usdt_contract: Option<&str>,
    ) -> (
        ApiGatewayRouteHandler,
        std::sync::Arc<std::sync::Mutex<Option<os_nexhub::chain_verify::TxProof>>>,
    ) {
        let proof = std::sync::Arc::new(std::sync::Mutex::new(None));
        let gate = os_nexhub::nexhub_lobby::ChainPayGate::with_parts(
            true,
            None,
            None,
            default_chain_id,
            std::time::Duration::from_secs(1),
            usdt_contract,
            6,
            std::sync::Arc::new(GwProofCaptureVerifier {
                outcome,
                proof: proof.clone(),
            }),
        );
        (
            ApiGatewayRouteHandler::with_empty().with_chain_verify(gate),
            proof,
        )
    }

    /// 直接落一张 usdt 订单：1000 积分 × 0.01 = "10.00" USDT（两位小数人类单位，
    /// crypto_amount_for 价目形状——ERC-20 核验须按 decimals=6 换算成微单位）。
    async fn seeded_usdt_order(h: &ApiGatewayRouteHandler, address: &str) -> String {
        let tok_id = seeded_token_id(h).await;
        let pay_id = format!("pay-{tok_id}");
        let order = PaymentOrder {
            id: pay_id.clone(),
            token_id: tok_id,
            currency: "usdt".into(),
            amount_crypto: "10.00".into(),
            credits: 1000,
            address: address.to_string(),
            memo: None,
            status: "pending".into(),
            txid: None,
            created_at: now_iso(),
            confirmed_at: None,
            reject_reason: None,
            chain_block: None,
            chain_value_wei: None,
        };
        {
            let conn = h.db.lock().expect("db poisoned");
            insert_payment_order(&conn, &order).expect("落订单必成功");
        }
        pay_id
    }

    // GW-CV11. usdt 订单 + txid + 合约可定位（body）→ ERC-20 核验接线：
    //         凭证带 Erc20Spec（body 合约 + 6 位换算 "10.00"→"10000000"）、
    //         金额规则 AtLeast；Verified 带回 token/实付并落库。
    #[tokio::test]
    async fn confirm_usdt_evm_erc20_verified() {
        let (h, proof) = gw_with_capture(
            os_nexhub::chain_verify::VerifyOutcome::Verified {
                block_number: 88,
                to: "0xgw-recv-addr".into(),
                value_wei: "10000000".into(),
                token: Some(GW_USDT_CONTRACT.into()),
            },
            Some(11155111),
            None, // env 不配——本例走 body erc20_contract
        );
        let pay_id = seeded_usdt_order(&h, "0xgw-recv-addr").await;
        let resp = confirm(
            &h,
            &pay_id,
            serde_json::json!({"txid": "0xusdt-tx", "erc20_contract": GW_USDT_CONTRACT}),
        )
        .await;
        assert_eq!(resp.status, 200, "ERC-20 核验通过应确认: {resp:?}");
        assert_eq!(resp.body["chain_verify"]["status"], "verified");
        assert_eq!(resp.body["chain_verify"]["token"], GW_USDT_CONTRACT);
        assert_eq!(resp.body["chain_verify"]["value_wei"], "10000000");
        assert_eq!(resp.body["added_credits"], 1000);
        let p = proof.lock().unwrap().clone().expect("应捕获凭证");
        assert_eq!(
            p.erc20,
            Some(os_nexhub::chain_verify::Erc20Spec {
                contract: GW_USDT_CONTRACT.to_string(),
                decimals: 6,
            }),
            "body 合约 + 缺省 decimals=6"
        );
        assert_eq!(
            p.expected_value, "10000000",
            "10.00 USDT 按两位小数×10^6 换算成微单位"
        );
        assert_eq!(
            p.amount_rule,
            os_nexhub::chain_verify::AmountRule::AtLeast,
            "网关 confirm=AtLeast（多打不亏待用户）"
        );
        assert_eq!(p.expected_to, "0xgw-recv-addr", "收款方=订单地址");
        // 落库：订单行带链上事实
        let resp = h.handle(get_req("/api/v1/gateway/payments")).await.unwrap();
        let order = &resp.body.as_array().unwrap()[0];
        assert_eq!(order["chain_block"], 88);
        assert_eq!(order["chain_value_wei"], "10000000");
    }

    // GW-CV12. usdt 订单 + 合约走网关缺省（=env NEXOS_USDT_EVM_CONTRACT 形态）
    //         → 同样构造 ERC-20 凭证（body 不带 erc20_contract）。
    #[tokio::test]
    async fn confirm_usdt_erc20_contract_from_gate_default() {
        let (h, proof) = gw_with_capture(
            os_nexhub::chain_verify::VerifyOutcome::Verified {
                block_number: 89,
                to: "0xgw-recv-addr".into(),
                value_wei: "10000000".into(),
                token: Some(GW_USDT_CONTRACT.into()),
            },
            Some(1),
            Some(GW_USDT_CONTRACT),
        );
        let pay_id = seeded_usdt_order(&h, "0xgw-recv-addr").await;
        let resp = confirm(&h, &pay_id, serde_json::json!({"txid": "0xusdt-tx"})).await;
        assert_eq!(resp.status, 200, "env 合约形态应核验: {resp:?}");
        assert_eq!(resp.body["chain_verify"]["status"], "verified");
        let p = proof.lock().unwrap().clone().expect("应捕获凭证");
        assert_eq!(
            p.erc20.as_ref().map(|s| s.contract.as_str()),
            Some(GW_USDT_CONTRACT),
            "网关缺省（env 形态）兜底"
        );
        assert_eq!(p.chain_id, 1, "网关缺省链 ID");
    }

    // GW-CV13. usdt 订单定位不到 EVM 链（TRON 形态：网关无缺省链 ID、body 不带）
    //         → unverified 人工通道，不构造凭证（不猜链也不猜合约）。
    #[tokio::test]
    async fn confirm_usdt_tron_form_stays_manual() {
        let (h, proof) = gw_with_capture(
            os_nexhub::chain_verify::VerifyOutcome::NotFound, // 若误核即拒——证明没核
            None,                                             // 无缺省链 ID = TRON 形态
            Some(GW_USDT_CONTRACT),
        );
        let pay_id = seeded_usdt_order(&h, "0xgw-recv-addr").await;
        let resp = confirm(&h, &pay_id, serde_json::json!({"txid": "0xtron-style-tx"})).await;
        assert_eq!(resp.status, 200, "TRON usdt 人工确认: {resp:?}");
        assert_eq!(resp.body["chain_verify"]["status"], "unverified");
        assert!(
            resp.body["chain_verify"]["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("EVM"),
            "应说明未定位 EVM 链: {resp:?}"
        );
        assert!(proof.lock().unwrap().is_none(), "不构造 EVM 凭证");
    }

    // GW-CV14. usdt 订单 + EVM 链但合约无处可寻 → unverified（不猜合约地址）。
    #[tokio::test]
    async fn confirm_usdt_without_contract_unverified() {
        let (h, proof) = gw_with_capture(
            os_nexhub::chain_verify::VerifyOutcome::NotFound,
            Some(11155111),
            None,
        );
        let pay_id = seeded_usdt_order(&h, "0xgw-recv-addr").await;
        let resp = confirm(&h, &pay_id, serde_json::json!({"txid": "0xusdt"})).await;
        assert_eq!(resp.status, 200, "缺合约不硬拒（人工兜底）: {resp:?}");
        assert_eq!(resp.body["chain_verify"]["status"], "unverified");
        assert!(
            resp.body["chain_verify"]["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("合约"),
            "应说明缺合约配置: {resp:?}"
        );
        assert!(proof.lock().unwrap().is_none());
    }

    // ====================================================================
    // 渠道中继（via_node 非空 → api_market relay 执行层，2026-09-03）
    // ====================================================================

    /// fake 互连 overlay fixture（api_market 测试同款手法）：消费者端点（注入
    /// handler.set_relay）↔ 源端端点（白名单=base）定向互投。返回
    /// （消费者端点, 源端 NodeID hex——即渠道 via_node 值）。
    fn gw_relay_pair(base: &str) -> (crate::handlers::api_market::ApiMarketFedEndpoint, String) {
        use std::sync::Arc;
        let consumer = crate::handlers::api_market::ApiMarketFedEndpoint::test_endpoint();
        let source =
            crate::handlers::api_market::ApiMarketFedEndpoint::test_endpoint_with_local_listing(
                base,
            );
        let a_id = os_p2p::NodeIdentity::generate().node_id();
        let b_id = os_p2p::NodeIdentity::generate().node_id();
        let a_hex = a_id.to_hex();
        let b_hex = b_id.to_hex();
        // 消费者 → 源端定向：req 帧直达源端 dispatch（验签方 = 消费者 NodeID）。
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
            "gw-consumer".into(),
        );
        // 源端 → 消费者定向：resp 帧直达消费者 dispatch。
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
            "gw-source".into(),
        );
        (consumer, b_hex)
    }

    /// mock 非流式 chat 上游（真 TCP，单次请求）：回 OpenAI 形态 JSON（含
    /// usage）。返回端口。
    fn spawn_relay_chat_json_upstream() -> u16 {
        use std::io::{Read, Write};
        let body = serde_json::json!({
            "id": "chatcmpl-gw-relay-1", "model": "qwen3.5-9b",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "经中继的回复"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 7, "total_tokens": 12},
        })
        .to_string();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 失败");
        let port = listener.local_addr().expect("local_addr 失败").port();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        });
        port
    }

    /// 直插一条中继渠道（via_node 非空；models/priority 可指定）。
    fn seed_relay_channel(
        h: &ApiGatewayRouteHandler,
        id: &str,
        base_url: &str,
        via_node: &str,
        models: &[&str],
        priority: u32,
    ) {
        let conn = h.db.lock().unwrap();
        insert_channel(
            &conn,
            &Channel {
                id: id.into(),
                name: format!("relay-{id}"),
                provider: "openai".into(),
                base_url: base_url.into(),
                api_key: "sk-fed-key".into(),
                models: models.iter().map(|s| s.to_string()).collect(),
                priority,
                weight: 1,
                status: "enabled".into(),
                enabled: true,
                created_at: String::new(),
                last_used: None,
                request_count: 0,
                via_node: via_node.into(),
                price_per_call: 0.0,
                price_per_sec: 0.0,
                price_per_token: 0.0,
            },
        )
        .unwrap();
    }

    /// GW-RL1. 中继渠道非流式往返：客户端 → 网关（sk-os- 令牌鉴权/计费照常）
    /// → relay 执行层（fake overlay）→ 源端白名单放行 → mock 上游 → 原样回。
    #[tokio::test]
    async fn relay_channel_nonstream_roundtrip_via_fake_overlay() {
        let port = spawn_relay_chat_json_upstream();
        let base = format!("http://127.0.0.1:{port}/v1");
        let (consumer, source_hex) = gw_relay_pair(&base);
        let h = ApiGatewayRouteHandler::with_empty();
        h.set_relay(Some(consumer));
        seed_relay_channel(&h, "ch-rl", &base, &source_hex, &["qwen3.5-9b"], 0);
        seed_token(&h, "tok-rl", "sk-os-relay", vec![], vec![]);
        let resp = h
            .handle(post_req_with_auth(
                "/api/v1/gateway/v1/chat/completions",
                serde_json::json!({
                    "model": "qwen3.5-9b",
                    "messages": [{"role": "user", "content": "hi"}]
                }),
                "sk-os-relay",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "中继整包应 200: {resp:?}");
        assert_eq!(
            resp.body["choices"][0]["message"]["content"], "经中继的回复",
            "上游 JSON 原样透传: {resp:?}"
        );
        // usage 解析照常 → 成功日志 + 配额扣减（本地计费/鉴权不受中继影响）
        let logs = h.logs_snapshot();
        let log = logs
            .iter()
            .find(|l| l.status == "success")
            .expect("应有 success 日志");
        assert_eq!(log.channel_id, "ch-rl");
        assert_eq!(
            (log.prompt_tokens, log.completion_tokens, log.total_tokens),
            (5, 7, 12)
        );
        let tokens = h.tokens_snapshot();
        assert_eq!(
            tokens[0].quota_used, 12,
            "per_token 计费照扣（12 × 1.0 × 1.0）"
        );
    }

    /// GW-RL2. 中继渠道失败（通道未装配）→ 故障转移到直连渠道成功：
    /// 失败记 failed 日志（带「经 … 中继失败」可区分文案），成功走直连。
    #[tokio::test]
    async fn relay_channel_failure_fails_over_to_direct_channel() {
        let port = spawn_relay_chat_json_upstream();
        let direct_base = format!("http://127.0.0.1:{port}/v1");
        let h = ApiGatewayRouteHandler::with_empty();
        // 不 set_relay（通道未装配）——via_node 指向合法 NodeID 但无执行通道。
        let ghost_hex = os_p2p::NodeIdentity::generate().node_id().to_hex();
        seed_relay_channel(
            &h,
            "ch-rl-dead",
            "http://10.0.0.9:8000/v1",
            &ghost_hex,
            &["m-1"],
            0,
        );
        seed_channel(&h, "ch-direct", &["m-1"], true, 1);
        // 直连渠道 base_url 指向 mock 上游（seed_channel 固定 127.0.0.1:1——改写）
        {
            let conn = h.db.lock().unwrap();
            let mut c = find_channel(&conn, "ch-direct").unwrap().unwrap();
            c.base_url = direct_base;
            update_channel(&conn, &c).unwrap();
        }
        seed_token(&h, "tok-fo", "sk-os-fo", vec![], vec![]);
        let resp = h
            .handle(post_req_with_auth(
                "/api/v1/gateway/v1/chat/completions",
                serde_json::json!({"model": "m-1", "messages": []}),
                "sk-os-fo",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "应转移到直连渠道成功: {resp:?}");
        assert_eq!(
            resp.body["choices"][0]["message"]["content"],
            "经中继的回复"
        );
        let logs = h.logs_snapshot();
        let failed = logs
            .iter()
            .find(|l| l.status == "failed")
            .expect("应有 failed 日志");
        assert_eq!(failed.channel_id, "ch-rl-dead");
        assert!(
            failed.error.as_deref().unwrap_or("").contains("中继失败"),
            "中继失败文案可区分: {failed:?}"
        );
        assert!(logs
            .iter()
            .any(|l| l.status == "success" && l.channel_id == "ch-direct"));
    }

    /// GW-RL3. 渠道 via_node 契约：手填非法 NodeID 400；PUT 可清除回直连。
    #[tokio::test]
    async fn channel_via_node_validation_and_update_clear() {
        let h = ApiGatewayRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/channels",
                serde_json::json!({
                    "name": "bad-relay", "provider": "openai",
                    "base_url": "http://10.0.0.9:8000/v1",
                    "models": ["m"], "via_node": "not-a-node-id"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "非法 via_node 应 400: {resp:?}");
        // 合法 via_node → 201 且透出
        let good_hex = os_p2p::NodeIdentity::generate().node_id().to_hex();
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/channels",
                serde_json::json!({
                    "name": "ok-relay", "provider": "openai",
                    "base_url": "http://10.0.0.9:8000/v1",
                    "models": ["m"], "via_node": good_hex
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["via_node"], good_hex);
        let id = resp.body["id"].as_str().unwrap().to_string();
        // 列表透出 + 脱敏不受影响
        let resp = h.handle(get_req("/api/v1/gateway/channels")).await.unwrap();
        assert_eq!(resp.body[0]["via_node"], good_hex);
        // PUT 清除（空串）→ 回直连语义
        let resp = h
            .handle(ApiRequest {
                method: HttpMethod::Put,
                path: format!("/api/v1/gateway/channels/{id}"),
                headers: serde_json::json!({}),
                body: serde_json::json!({"via_node": ""}),
                auth: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["via_node"], "", "空串清除回直连: {resp:?}");
        // 老库迁移：无 via_node 列的存量库打开即补列（''=直连）
        let path = unique_temp_db_path();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE channels (
                    id TEXT PRIMARY KEY, name TEXT NOT NULL, provider TEXT NOT NULL,
                    base_url TEXT NOT NULL, api_key TEXT NOT NULL, models TEXT NOT NULL,
                    priority INTEGER NOT NULL, weight INTEGER NOT NULL, status TEXT NOT NULL,
                    enabled INTEGER NOT NULL, created_at TEXT NOT NULL, last_used TEXT,
                    request_count INTEGER NOT NULL
                );
                INSERT INTO channels VALUES
                ('ch-old','旧','openai','http://a/v1','','[\"m\"]',0,1,'enabled',1,'t',NULL,0);",
            )
            .unwrap();
        }
        let h2 = ApiGatewayRouteHandler::with_db_path(&path);
        let ch = h2
            .channels_snapshot()
            .into_iter()
            .find(|c| c.id == "ch-old")
            .expect("旧渠道应在");
        assert_eq!(ch.via_node, "", "迁移后旧行直连语义");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path}-wal"));
        let _ = std::fs::remove_file(format!("{path}-shm"));
    }

    /// 造外部 API 登记源（内存库；经组件 handle 走真实创建路径——id 由服务端
    /// 计数器生成，返回 (状态, 实际登记 id)）。
    async fn gw_ext_source(
        state: std::sync::Arc<crate::handlers::llm_external::LlmExternalState>,
        name: &str,
        base_url: &str,
        api_key: &str,
        models: &[&str],
        via_node: &str,
    ) -> String {
        let resp = crate::handlers::llm_external::handle(
            &state,
            HttpMethod::Post,
            &[],
            serde_json::json!({
                "name": name, "base_url": base_url,
                "api_key": api_key,
                "models": models,
                "via_node": via_node,
            }),
        )
        .await
        .unwrap();
        assert_eq!(resp.status, 201, "造登记行: {resp:?}");
        resp.body["id"].as_str().unwrap().to_string()
    }

    /// GW-RL4. 一键导入字段复制：from_external_api 复制
    /// name/base_url/api_key/models/via_node（provider 缺省 openai）。
    #[tokio::test]
    async fn create_channel_from_external_api_copies_fields() {
        let via = os_p2p::NodeIdentity::generate().node_id().to_hex();
        let ext =
            std::sync::Arc::new(crate::handlers::llm_external::LlmExternalState::with_memory());
        let ext_id = gw_ext_source(
            ext.clone(),
            "联邦 qwen",
            "http://10.0.0.9:8000/v1",
            "sk-fed-secret",
            &["qwen3.5-9b"],
            &via,
        )
        .await;
        let h = ApiGatewayRouteHandler::with_empty();
        h.set_external_source(Some(ext));
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/channels",
                serde_json::json!({"from_external_api": ext_id}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "一键导入应 201: {resp:?}");
        assert_eq!(resp.body["name"], "联邦 qwen");
        assert_eq!(resp.body["base_url"], "http://10.0.0.9:8000/v1");
        assert_eq!(
            resp.body["api_key"], "sk-fed-secret",
            "明文 key 复制进渠道（POST 响应）"
        );
        assert_eq!(resp.body["models"], serde_json::json!(["qwen3.5-9b"]));
        assert_eq!(resp.body["via_node"], via, "via_node 复制 → 中继渠道");
        assert_eq!(resp.body["provider"], "openai", "provider 缺省 openai");
        // 落库核对（渠道可路由）
        let ch = h.channels_snapshot().into_iter().next().unwrap();
        assert_eq!(ch.via_node, via);
        assert_eq!(ch.api_key, "sk-fed-secret");
    }

    /// GW-RL5. from_external_api 不存在 → 404（不建渠道）。
    #[tokio::test]
    async fn create_channel_from_external_api_unknown_404() {
        let ext =
            std::sync::Arc::new(crate::handlers::llm_external::LlmExternalState::with_memory());
        let _ext_id =
            gw_ext_source(ext.clone(), "a", "http://10.0.0.9:8000/v1", "", &["m"], "").await;
        let h = ApiGatewayRouteHandler::with_empty();
        h.set_external_source(Some(ext));
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/channels",
                serde_json::json!({"from_external_api": "xapi-404"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404, "未知登记应 404: {resp:?}");
        assert_eq!(h.channels_snapshot().len(), 0, "不得创建渠道");
    }

    /// GW-RL6. models 空探测回填：登记行 models 空 + via_node 非空 → 导入时经
    /// 中继 GET /models（fake overlay + mock 上游）拿真实清单回填。
    #[tokio::test]
    async fn create_channel_from_external_api_probes_models_via_relay() {
        // mock /models 上游（真 TCP）
        let body = serde_json::json!({
            "object": "list",
            "data": [
                {"id": "qwen3.5-9b", "object": "model"},
                {"id": "qwen2.5-7b", "object": "model"},
            ]
        })
        .to_string();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 失败");
        let port = listener.local_addr().expect("local_addr 失败").port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        });
        let base = format!("http://127.0.0.1:{port}/v1");
        let (consumer, source_hex) = gw_relay_pair(&base);
        let ext =
            std::sync::Arc::new(crate::handlers::llm_external::LlmExternalState::with_memory());
        let ext_id =
            gw_ext_source(ext.clone(), "联邦空清单", &base, "sk-k", &[], &source_hex).await;
        let h = ApiGatewayRouteHandler::with_empty();
        h.set_relay(Some(consumer));
        h.set_external_source(Some(ext));
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/channels",
                serde_json::json!({"from_external_api": ext_id}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "导入应 201: {resp:?}");
        assert_eq!(
            resp.body["models"],
            serde_json::json!(["qwen3.5-9b", "qwen2.5-7b"]),
            "models 空探测回填（经中继 GET /models）: {resp:?}"
        );
        assert_eq!(resp.body["via_node"], source_hex);
        assert!(resp.body.get("warning").is_none(), "探测成功无 warning");
    }

    /// GW-RL7. 导入探测失败不阻塞（warning 标注，渠道照建——字段全来自真实登记）。
    #[tokio::test]
    async fn create_channel_from_external_api_probe_failure_still_creates() {
        // via_node 非空但不装 relay 端点 → 中继探测失败 → 渠道照建 + warning
        let ghost = os_p2p::NodeIdentity::generate().node_id().to_hex();
        let ext =
            std::sync::Arc::new(crate::handlers::llm_external::LlmExternalState::with_memory());
        let ext_id = gw_ext_source(
            ext.clone(),
            "联邦探测失败",
            "http://10.0.0.9:8000/v1",
            "",
            &[],
            &ghost,
        )
        .await;
        let h = ApiGatewayRouteHandler::with_empty();
        h.set_external_source(Some(ext));
        let resp = h
            .handle(post_req(
                "/api/v1/gateway/channels",
                serde_json::json!({"from_external_api": ext_id}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "探测失败仍建渠道: {resp:?}");
        assert_eq!(resp.body["models"], serde_json::json!([]));
        let warn = resp.body["warning"].as_str().unwrap_or("");
        assert!(warn.contains("探测失败"), "warning 标注探测失败: {resp:?}");
    }
}
