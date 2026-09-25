//! NexHub 大厅·链上身份与支付域（2026-09-25 大文件拆分批，纯搬运零行为变化）：
//! 调用方身份 Caller（链上 token 反查/admin 回落/owner 判定）+ 购买授权
//! （hub_entitlements 收据核验）+ 链上支付验真网关 ChainPayGate
//! （EvmTxVerifier 可注入 RPC + eth/ERC-20-USDT 双形态 + 金额规则）
//! + 悬赏（hub_bounties CRUD/认领/审核）。对外面经 lobby/mod.rs 重导出。

use super::*;

/// 已认证的 NexHub 调用方（`Authorization: Bearer` 解析结果）。
///
/// 解析顺序（[`NexHubLobbyRouteHandler::caller`]）：
/// 1. nexhub 链上 token（`/api/v1/nexhub/auth/verify` 签发）→ 反查 pubkey；
/// 2. 无/无效 → 回落系统 admin 判定（`NEXOS_ADMIN_TOKEN`/`OS_ADMIN_TOKEN`
///    精确比对，与 os-api 网关同一环境变量语义）；
/// 3. 两者皆非 → None（调用方回 401）。
pub(super) enum Caller {
    /// 链上身份：publisher/poster/hunter/buyer 全部归因到该 pubkey。
    Pubkey {
        pubkey: String,
        /// 展示名（pubkey 派生 EVM 地址）。
        display_name: String,
    },
    /// 系统 admin（平台托管/管理通道）。
    Admin,
}

impl Caller {
    /// 归因标识（写库的 owner/buyer/hunter 值）：pubkey 身份 → pubkey；
    /// admin → `"admin"`。
    pub(super) fn actor(&self) -> &str {
        match self {
            Caller::Pubkey { pubkey, .. } => pubkey,
            Caller::Admin => "admin",
        }
    }

    /// 是否为链上 pubkey 身份（非 admin）。
    pub(super) fn pubkey(&self) -> Option<&str> {
        match self {
            Caller::Pubkey { pubkey, .. } => Some(pubkey),
            Caller::Admin => None,
        }
    }
}

/// 条目 owner 是否为链上身份：publisher 字段是合法压缩公钥（`0x`+66 hex 可解析）
/// → owner_kind=pubkey；否则为存量字符串条目（NexOS/zcode/local/…）= 平台托管。
pub(super) fn entry_owner_is_pubkey(publisher: &str) -> bool {
    chain_auth::parse_pubkey(publisher).is_some()
}

// ----------------------------------------------------------------------------
// NexHubLobbyRouteHandler
// ----------------------------------------------------------------------------

/// 系统 admin token（env）：`NEXOS_ADMIN_TOKEN` 优先，回退 `OS_ADMIN_TOKEN`——
/// 与 os-api 网关（main.rs `set_admin_token`）同一环境变量语义，构造时定格
/// （避免运行中读 env 的竞态；None = 未启用 admin 回落）。
pub(super) fn admin_token_from_env() -> Option<String> {
    std::env::var("NEXOS_ADMIN_TOKEN")
        .or_else(|_| std::env::var("OS_ADMIN_TOKEN"))
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// 悬赏操作者判定：admin 恒可（含存量字符串 poster 的平台托管悬赏）；
/// pubkey 调用方须与 poster 同 pubkey（字符串 poster 的悬赏对链上身份 403）。
pub(super) fn caller_owns_bounty(caller: &Caller, poster: &str) -> bool {
    match caller.pubkey() {
        Some(pubkey) => poster == pubkey,
        None => true,
    }
}

/// PR 审核者判定（merge/reject）：admin 恒可；pubkey 调用方须为 repo owner
/// （大厅条目 publisher=pubkey 且同 pubkey）。无大厅条目（未发布到大厅的裸仓）
/// 或存量字符串条目 → 仅 admin——owner 判定以大厅发布索引为权威。
pub(super) fn caller_can_review_pr(caller: &Caller, entry: Option<&LobbyEntry>) -> bool {
    match caller.pubkey() {
        Some(pubkey) => {
            entry.is_some_and(|e| entry_owner_is_pubkey(&e.publisher) && e.publisher == pubkey)
        }
        None => true,
    }
}

/// 单条购买授权（hub_entitlement 行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entitlement {
    /// 大厅条目（仓库）名。
    pub repo_name: String,
    /// 购买者标识（钱包地址 / 用户 id；与 os-wallet `AddressId` 对齐）。
    pub buyer: String,
    /// 链：`btc` / `nex` / `usdc` / `eth`（与条目 currency 同域）。
    pub chain: String,
    /// 链上交易 id / 收据指纹（一期为收据指纹；二期对接 os-wallet 验真）。
    pub txid: String,
    /// 实际支付金额（最小单位），应 ≥ 条目 `price_sats`。
    pub amount_sats: u64,
    /// 计价货币（冗余存一份，便于审计）。
    pub currency: String,
    /// 支付时间（RFC3339）。
    pub paid_at: String,
    /// 链上核验事实（dApp 一期，2026-08-31）：核验通过时的**块高**；
    /// None = 未核验（自证收据 / RPC 降级 / 开关关闭）。
    #[serde(default)]
    pub chain_block: Option<u64>,
    /// 链上核验事实：链上**实付金额**（wei 十进制字符串，与 tx 的 value 一致）；
    /// None = 未核验。审计口径：`chain_block` 有值 ⇒ 该收据经真实 RPC 核验。
    #[serde(default)]
    pub chain_value_wei: Option<String>,
}

/// 列字段序（hub_entitlement INSERT/SELECT 共用）。
pub(super) const ENTITLEMENT_COLUMNS: &str =
    "repo_name,buyer,chain,txid,amount_sats,currency,paid_at,chain_block,chain_value_wei";

pub(super) fn insert_entitlement(conn: &Connection, e: &Entitlement) -> rusqlite::Result<()> {
    conn.execute(
        &format!(
            "INSERT OR REPLACE INTO hub_entitlement ({ENTITLEMENT_COLUMNS}) \
             VALUES (?,?,?,?,?,?,?,?,?)"
        ),
        params![
            e.repo_name,
            e.buyer,
            e.chain,
            e.txid,
            e.amount_sats,
            e.currency,
            e.paid_at,
            e.chain_block,
            e.chain_value_wei,
        ],
    )?;
    Ok(())
}

pub(super) fn entitlement_from_row(row: &rusqlite::Row) -> rusqlite::Result<Entitlement> {
    Ok(Entitlement {
        repo_name: row.get(0)?,
        buyer: row.get(1)?,
        chain: row.get(2)?,
        txid: row.get(3)?,
        amount_sats: row.get::<_, i64>(4)?.max(0) as u64,
        currency: row.get(5)?,
        paid_at: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
        chain_block: row.get::<_, Option<i64>>(7)?.map(|v| v.max(0) as u64),
        chain_value_wei: row.get(8)?,
    })
}

/// 查询某买家对某仓库的授权（存在即已付费，可克隆）。
pub(super) fn find_entitlement(
    conn: &Connection,
    name: &str,
    buyer: &str,
) -> rusqlite::Result<Option<Entitlement>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ENTITLEMENT_COLUMNS} FROM hub_entitlement WHERE repo_name=? AND buyer=?"
    ))?;
    stmt.query_row(params![name, buyer], entitlement_from_row)
        .optional()
}

/// 授权记录列表（`GET /api/v1/nexhub/lobby/entitlements`）：`repo` 按仓库过滤
/// （admin 审计某条目的全部买家）、`buyer` 按买家过滤（自查购买记录），均可选
/// 可组合，都不给则全量；按支付时间降序。
pub(super) fn list_entitlements(
    conn: &Connection,
    repo: Option<&str>,
    buyer: Option<&str>,
) -> rusqlite::Result<Vec<Entitlement>> {
    let mut conds: Vec<&'static str> = Vec::new();
    let mut bind: Vec<&str> = Vec::new();
    if let Some(r) = repo {
        conds.push("repo_name = ?");
        bind.push(r);
    }
    if let Some(b) = buyer {
        conds.push("buyer = ?");
        bind.push(b);
    }
    let mut sql = format!("SELECT {ENTITLEMENT_COLUMNS} FROM hub_entitlement");
    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }
    sql.push_str(" ORDER BY paid_at DESC");
    let mut stmt = conn.prepare(&sql)?;
    let iter = stmt.query_map(params_from_iter(bind.iter()), entitlement_from_row)?;
    let mut out = Vec::new();
    for e in iter {
        out.push(e?);
    }
    Ok(out)
}

/// 验证购买收据（设计文档 §10 货币化）：货币一致 + 金额足额 + txid 非空
/// （收据指纹的**最低门槛**）。链上验真（dApp 一期，2026-08-31）不在此函数——
/// 它是同步纯函数，只做自证面校验；真实 RPC 核验在其通过后由
/// [`check_chain_payment`]（异步、可注入）接力，见 purchase/approve 两处接线。
///
/// 返回 `Ok(())` 或描述拒绝原因（调用方转 402/400）。
pub(super) fn verify_payment(
    receipt: &Entitlement,
    price_sats: u64,
    currency: &str,
) -> Result<(), String> {
    // 货币必须一致
    if !receipt.currency.eq_ignore_ascii_case(currency) {
        return Err(format!(
            "货币不符：条目为 {currency}，收据为 {}",
            receipt.currency
        ));
    }
    // 金额必须足额
    if receipt.amount_sats < price_sats {
        return Err(format!(
            "支付不足：需 {price_sats} {currency}，实付 {}",
            receipt.amount_sats
        ));
    }
    // txid 非空是收据的最低门槛（链上核验在 check_chain_payment 接力）
    if receipt.txid.trim().is_empty() {
        return Err("txid/收据指纹不得为空".into());
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// 链上支付验真（dApp 一期接线层，2026-08-31；二期增量 2026-09-02）
// ----------------------------------------------------------------------------
//
// 定位：把「自证收据」（txid 非空即过，安全隐患台账 S1）升级为真实 EVM RPC
// 核验（docs/DAPP_RESEARCH.md §3 方向 1）。**核验本体**在 [`crate::chain_verify`]
// （独立实现，契约冻结）；本段是**业务接线层**，被两条业务线共用：
//
// 1. NexHub：`POST /lobby/:name/purchase`（购买授权）与 `POST /bounty/:id/approve`
//    （悬赏验收放款）；
// 2. os-api 网关：`POST /gateway/payments/:id/confirm`（PaymentOrder 确认到账，
//   `crates/os-api/src/handlers/api_gateway.rs` 直接 import 本段的 pub 项）。
//
// 接缝设计（[`EvmTxVerifier`] trait）：生产实现 [`RpcVerifier`] 直调
// `chain_verify::verify_evm_tx`；测试注入固定 [`VerifyOutcome`]。这层抽象同时是
// 未来换核验后端（自建节点 / 商业 RPC / 索引服务）的替换点——业务语义全部在
// 本段，执行器一换即迁移。
//
// RPC 来源链（[`ChainPayGate::rpc_candidates`]，三段拼接成候选列表，
// `verify_evm_tx` 按序 failover）：
//
// ```text
// 请求显式 rpc_url（body 可选字段，admin/条目 owner 自配）
//   → env NEXOS_CHAIN_RPC_URLS（JSON {"<chain_id>": "<url>" 或 ["<url>",...]}，
//     解析失败 eprintln 警告并忽略，绝不 panic）
//   → chain_verify::fallback_rpc_for(chain_id)（链预设公共 RPC 兜底）
// ```
//
// 与 `chain_verify::ChainVerifyGate`（core 侧装配门面）的关系：本段
// [`ChainPayGate`] 是**业务接线网关**，在 core 契约（`verify_evm_tx` /
// `fallback_rpc_for`）之上多管三件事——① per-request 显式 rpc_url 前置进候选链；
// ② NexHub/网关侧缺省（`NEXOS_HUB_PAY_TO` / `NEXOS_EVM_CHAIN_ID`）；③ 开关关闭
// 的 `Skipped` 语义（不产生任何链上事实与标注，比 core 的 legacy_autopass
// 假 Verified 更干净）。两类型并存不冲突：core 门面服务 chain_verify 自身测试。
//
// 结果语义表（[`verdict_for`]；⚠️ 信任模型与降级策略同步维护在
// docs/NEXHUB_LOBBY_DESIGN.md §10 与 docs/GATEWAY_MONETIZATION.md）：
//
// | VerifyOutcome | 业务动作 | HTTP |
// |---------------|----------|------|
// | Verified      | 放行；`block_number`/`value_wei` 落库到收据结构 | 200 |
// | Pending       | 拒绝（**可重试**——未上块≠欺诈，稍后重试即可） | 409 |
// | Mismatch      | 拒绝（错误信息带字段名与链上实际值） | 409 |
// | NotFound      | 拒绝（txid 有误或已被节点裁剪） | 400 |
// | RpcError      | **降级放行** + 日志警告（网络故障不应阻断交易；S1 缓解 =「RPC 可用时核验，不白嫖」） | 200 |
//
// 无法构造凭证（非核验域货币 / 缺链 ID / 缺收款地址 / usdt 缺 ERC-20 合约配置）
// → 放行但响应标注 `chain_verify.status="unverified"` + 日志警告（真实数据铁律：
// 是否核验过必须可见，不静默假装成功）。`NEXOS_CHAIN_VERIFY_ENABLED=0` → [`ChainPayCheck::Skipped`]
// = 整体回旧行为（非空即过，响应不带任何标注）。
//
// **二期增量（2026-09-02）**：
//
// 1. **ERC-20（USDT@EVM）**：`currency=usdt` 且链 ID 可定位（=EVM 链；TRON 上的
//    USDT 定位不到 EVM 链 ID，仍 Unverified 人工）时构造 `TxProof.erc20` 凭证，
//    核验切换为 receipt Transfer 日志对账（见 chain_verify.rs 二期注记）。合约
//    地址来源：body `erc20_contract` → env `NEXOS_USDT_EVM_CONTRACT` → 都无则
//    Unverified（**不猜合约地址**——猜错合约=放行假代币转账）；小数位：body
//    `erc20_decimals` → env `NEXOS_USDT_EVM_DECIMALS`（默认 6）。金额口径=
//    最小单位（`to_min_unit_str`：整数透传 / 小数按 decimals 换算）。
// 2. **金额规则** [`AmountRule`]（三处接线定稿，docs 双写）：
//
//    | 业务线 | 规则 | 理由 |
//    |--------|------|------|
//    | 网关 confirm（充值） | `AtLeast` | 充值多打不亏待用户——超额照常入账订单积分，不足才拦 |
//    | NexHub purchase（购买） | `Exact` | 商品定价等值——须按应付额整额打款，多/少都不对账 |
//    | bounty approve（放款） | `AtLeast` | 与自证面「金额足额」（≥ 奖励）语义对齐，多打不亏待 hunter |

/// 可替换的 EVM 核验执行器（注入接缝）。
///
/// 生产实现 [`RpcVerifier`] 直调 `chain_verify::verify_evm_tx`；测试实现注入固定
/// [`VerifyOutcome`]（并可携带调用计数，断言「非 EVM 货币不触发核验」等接线语义）。
/// 返回 boxed future 而非 async trait 方法：dyn 兼容且零新增依赖。
pub trait EvmTxVerifier: Send + Sync {
    /// 核验一笔交易（rpc_urls 已按候选链排好序；timeout 来自网关配置）。
    fn verify(
        &self,
        rpc_urls: &[String],
        proof: &TxProof,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = VerifyOutcome> + Send>>;
}

/// 生产执行器：直调 [`crate::chain_verify::verify_evm_tx`]。
pub(super) struct RpcVerifier;

impl EvmTxVerifier for RpcVerifier {
    fn verify(
        &self,
        rpc_urls: &[String],
        proof: &TxProof,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = VerifyOutcome> + Send>> {
        let rpcs = rpc_urls.to_vec();
        let proof = proof.clone();
        Box::pin(async move { crate::chain_verify::verify_evm_tx(&rpcs, &proof, timeout).await })
    }
}

/// 链上验真网关——env 配置（构造时定格，与 admin_token 同款模式）+ 可替换执行器。
///
/// env 清单（全部 `NEXOS_` 前缀；与 `chain_verify.rs` 模块头一致）：
///
/// | env | 默认 | 作用 |
/// |---|---|---|
/// | `NEXOS_CHAIN_VERIFY_ENABLED` | `1` | 总开关；`0`=回旧行为（非空即过，无标注） |
/// | `NEXOS_CHAIN_RPC_URLS` | （空） | 节点级 RPC 预设，JSON `{"<chain_id>": "<url>" 或 ["<url>",...]}` |
/// | `NEXOS_CHAIN_VERIFY_TIMEOUT_SECS` | `10` | 单次核验 RPC 超时（下限 1s） |
/// | `NEXOS_EVM_CHAIN_ID` | （无） | EVM 支付缺省链 ID（NexHub 购买/悬赏 + 网关 confirm 共用） |
/// | `NEXOS_HUB_PAY_TO` | （无） | NexHub 购买流缺省收款地址（节点运营者配置；悬赏不回落此值） |
/// | `NEXOS_USDT_EVM_CONTRACT` | （无） | USDT@EVM 的 ERC-20 合约地址（二期 ERC-20 核验；body `erc20_contract` 优先） |
/// | `NEXOS_USDT_EVM_DECIMALS` | `6` | USDT 小数位（主流链=6；body `erc20_decimals` 优先；非法值警告回默认） |
pub struct ChainPayGate {
    enabled: bool,
    timeout: Duration,
    /// `NEXOS_CHAIN_RPC_URLS` 原始串（构造时定格；解析在
    /// [`parse_chain_rpc_env`] 纯函数，坏配置警告+忽略不 panic）。
    rpc_env_raw: Option<String>,
    default_pay_to: Option<String>,
    default_chain_id: Option<u64>,
    /// USDT@EVM 合约地址缺省（env `NEXOS_USDT_EVM_CONTRACT`，二期）。
    usdt_evm_contract: Option<String>,
    /// USDT 小数位缺省（env `NEXOS_USDT_EVM_DECIMALS`，默认 6，二期）。
    usdt_evm_decimals: u8,
    verifier: Arc<dyn EvmTxVerifier>,
}

impl ChainPayGate {
    /// 生产构造：读 env 定格 + 生产执行器（handler 构造时调用一次）。
    #[must_use]
    pub fn from_env() -> Self {
        Self::with_parts(
            chain_verify_enabled_from_env(),
            std::env::var("NEXOS_CHAIN_RPC_URLS")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .as_deref(),
            non_empty_env("NEXOS_HUB_PAY_TO").as_deref(),
            std::env::var("NEXOS_EVM_CHAIN_ID")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok()),
            chain_verify_timeout_from_env(),
            non_empty_env("NEXOS_USDT_EVM_CONTRACT").as_deref(),
            usdt_evm_decimals_from_env(),
            Arc::new(RpcVerifier),
        )
    }

    /// 全字段注入构造（测试/诊断：绕开 env 并行竞态，执行器可控）。
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn with_parts(
        enabled: bool,
        rpc_env_raw: Option<&str>,
        default_pay_to: Option<&str>,
        default_chain_id: Option<u64>,
        timeout: Duration,
        usdt_evm_contract: Option<&str>,
        usdt_evm_decimals: u8,
        verifier: Arc<dyn EvmTxVerifier>,
    ) -> Self {
        Self {
            enabled,
            timeout,
            rpc_env_raw: rpc_env_raw
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            default_pay_to: default_pay_to
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            default_chain_id,
            usdt_evm_contract: usdt_evm_contract
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            usdt_evm_decimals,
            verifier,
        }
    }

    /// 总开关状态（false = 调用方整体回旧行为）。
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 节点级缺省收款地址（env `NEXOS_HUB_PAY_TO`）。
    #[must_use]
    pub fn default_pay_to(&self) -> Option<&str> {
        self.default_pay_to.as_deref()
    }

    /// 缺省链 ID（env `NEXOS_EVM_CHAIN_ID`）。
    #[must_use]
    pub fn default_chain_id(&self) -> Option<u64> {
        self.default_chain_id
    }

    /// USDT@EVM 合约地址缺省（env `NEXOS_USDT_EVM_CONTRACT`；二期 ERC-20）。
    #[must_use]
    pub fn usdt_evm_contract(&self) -> Option<&str> {
        self.usdt_evm_contract.as_deref()
    }

    /// USDT 小数位缺省（env `NEXOS_USDT_EVM_DECIMALS`，默认 6；二期 ERC-20）。
    #[must_use]
    pub fn usdt_evm_decimals(&self) -> u8 {
        self.usdt_evm_decimals
    }

    /// RPC 候选链：body 显式 → env `NEXOS_CHAIN_RPC_URLS[chain_id]` → 链预设兜底。
    #[must_use]
    pub fn rpc_candidates(&self, explicit: Option<&str>, chain_id: u64) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if let Some(url) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
            out.push(url.to_string());
        }
        if let Some(raw) = &self.rpc_env_raw {
            out.extend(parse_chain_rpc_env(raw, chain_id));
        }
        out.extend(crate::chain_verify::fallback_rpc_for(chain_id));
        out
    }

    /// 执行核验（候选链为空视作 RpcError——降级放行语义，见语义表）。
    pub async fn verify(&self, proof: &TxProof, explicit_rpc: Option<&str>) -> VerifyOutcome {
        let candidates = self.rpc_candidates(explicit_rpc, proof.chain_id);
        if candidates.is_empty() {
            return VerifyOutcome::RpcError {
                detail: format!(
                    "chain {} 无可用 RPC 候选（未配置 NEXOS_CHAIN_RPC_URLS 且无兜底预设）",
                    proof.chain_id
                ),
            };
        }
        self.verifier.verify(&candidates, proof, self.timeout).await
    }
}

/// `NEXOS_CHAIN_VERIFY_ENABLED` 解析：未设置默认开；`0`/`false`/`off`（大小写
/// 不敏感）关，其余任意值视为开（与 chain_verify.rs 模块头契约一致）。
pub(super) fn chain_verify_enabled_from_env() -> bool {
    !std::env::var("NEXOS_CHAIN_VERIFY_ENABLED")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off"
            )
        })
        .unwrap_or(false)
}

/// `NEXOS_CHAIN_VERIFY_TIMEOUT_SECS` 解析：默认 10s；非法值警告并回默认；下限 1s。
pub(super) fn chain_verify_timeout_from_env() -> Duration {
    const DEFAULT_SECS: u64 = 10;
    match std::env::var("NEXOS_CHAIN_VERIFY_TIMEOUT_SECS") {
        Ok(v) => match v.trim().parse::<u64>() {
            Ok(secs) => Duration::from_secs(secs.max(1)),
            Err(_) => {
                eprintln!("[chain-verify] NEXOS_CHAIN_VERIFY_TIMEOUT_SECS={v:?} 非法，回默认 {DEFAULT_SECS}s");
                Duration::from_secs(DEFAULT_SECS)
            }
        },
        Err(_) => Duration::from_secs(DEFAULT_SECS),
    }
}

/// `NEXOS_USDT_EVM_DECIMALS` 解析（二期 ERC-20）：默认 6（USDT 主流链小数位）；
/// 非法值警告回默认；上限 36（>36 必然溢出 u128，视为配置错误回默认）。
pub(super) fn usdt_evm_decimals_from_env() -> u8 {
    const DEFAULT: u8 = 6;
    match std::env::var("NEXOS_USDT_EVM_DECIMALS") {
        Ok(v) => match v.trim().parse::<u8>() {
            Ok(d) if d <= 36 => d,
            Ok(d) => {
                eprintln!("[chain-verify] NEXOS_USDT_EVM_DECIMALS={d} 超 36（u128 必然溢出），回默认 {DEFAULT}");
                DEFAULT
            }
            Err(_) => {
                eprintln!("[chain-verify] NEXOS_USDT_EVM_DECIMALS={v:?} 非法，回默认 {DEFAULT}");
                DEFAULT
            }
        },
        Err(_) => DEFAULT,
    }
}

/// 读非空 env（trim 后非空才算配置）。
pub(super) fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// 解析 `NEXOS_CHAIN_RPC_URLS`（纯函数，测试直测）：JSON 对象
/// `{"<chain_id>": "<url>" 或 ["<url>", ...]}`，取指定链的 URL 列表。
///
/// 容错（配置错误绝不 panic，坏值丢弃 + 警告）：非 JSON / 非对象 / 键形状非法
/// → 空列表 + eprintln；数组内非字符串/空串元素跳过。
#[must_use]
pub fn parse_chain_rpc_env(raw: &str, chain_id: u64) -> Vec<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Vec::new();
    }
    let parsed: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[chain-verify] NEXOS_CHAIN_RPC_URLS 解析失败（{e}），已忽略该配置");
            return Vec::new();
        }
    };
    let Some(obj) = parsed.as_object() else {
        eprintln!("[chain-verify] NEXOS_CHAIN_RPC_URLS 须为 JSON 对象 {{\"<chain_id>\": \"<url>\"|[urls]}}，已忽略");
        return Vec::new();
    };
    match obj.get(&chain_id.to_string()) {
        None => Vec::new(),
        Some(serde_json::Value::String(url)) if !url.trim().is_empty() => {
            vec![url.trim().to_string()]
        }
        Some(serde_json::Value::Array(urls)) => urls
            .iter()
            .filter_map(|u| u.as_str())
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .map(str::to_string)
            .collect(),
        Some(_) => {
            eprintln!("[chain-verify] NEXOS_CHAIN_RPC_URLS[{chain_id}] 形状非法（须 \"<url>\" 或 [urls]），已忽略");
            Vec::new()
        }
    }
}

/// 链 ID 解析（优先级）：body 显式 `chain_id` → `chain` 字符串可解析为数值时
/// （如 `"11155111"`；`"eth"` 等货币名忽略）→ env 缺省 `NEXOS_EVM_CHAIN_ID`。
#[must_use]
pub fn resolve_chain_id(
    explicit: Option<u64>,
    chain_str: Option<&str>,
    env_default: Option<u64>,
) -> Option<u64> {
    explicit
        .or_else(|| chain_str.and_then(|s| s.trim().parse::<u64>().ok()))
        .or(env_default)
}

/// 是否 EVM native 币（一期核验域）：NexHub 侧 `eth`、网关侧 `evm`。
/// `btc`/`nex`/`usdc` 不在核验域（usdc 的 ERC-20 接入是后续项，一期只配了
/// USDT 合约 env）。
#[must_use]
pub fn evm_native_currency(currency: &str) -> bool {
    matches!(currency.trim().to_ascii_lowercase().as_str(), "eth" | "evm")
}

/// 是否 USDT（二期 ERC-20 核验域）：`usdt`。配合**链 ID 可定位**才走 EVM
/// 路径——TRON 上的 USDT（无 EVM chain_id）仍 Unverified 人工确认。
#[must_use]
pub fn usdt_currency(currency: &str) -> bool {
    currency.trim().eq_ignore_ascii_case("usdt")
}

/// 金额 → 最小单位十进制字符串（通用版；**小数位由调用方给定**）。
///
/// - 纯整数（如 `"500"`、`"10000000"`）：视为**已是最小单位**，原样返回
///   （与 `LobbyEntry.price_sats`「最小货币单位」及网关 `PaymentOrder.amount_crypto`
///   的既有语义一致——NexHub usdt 条目的 amount_sats 即最小单位整数）；
/// - 带小数点（如 `"10.00"`，网关 usdt 订单的价目形状）：按 `decimals` 位换算
///   （USDT=6 → `"10000000"`；native 币=18），小数超 `decimals` 位/非数字 → None；
/// - 空串/非法 → None。
#[must_use]
pub fn to_min_unit_str(amount: &str, decimals: u8) -> Option<String> {
    let s = amount.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(min_unit) = s.parse::<u128>() {
        return Some(min_unit.to_string());
    }
    let (int_part, frac_part) = s.split_once('.')?;
    if int_part.is_empty() || !int_part.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let frac_len = usize::from(decimals);
    if frac_part.len() > frac_len || !frac_part.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let scaled = format!(
        "{int_part}{frac_part}{}",
        "0".repeat(frac_len - frac_part.len())
    );
    scaled.parse::<u128>().ok().map(|v| v.to_string())
}

/// 金额 → wei 十进制字符串（**18 位小数假设**，注释与文档双写；native 币路径）。
///
/// - 纯整数（如 `"500"`、`"10000000000000000000"`）：视为**已是最小单位 wei**，
///   原样返回（与 `LobbyEntry.price_sats`「最小货币单位」及网关
///   `PaymentOrder.amount_crypto`（evm 订单即 wei 整数串）既有语义一致）；
/// - 带小数点（如 `"0.02"`）：按 **18 位小数**换算（EVM 主流链 native 币均为
///   18 位；**非 18 位链不适用**，见 docs 限制清单），小数超 18 位/非数字 → None；
/// - 空串/非法 → None。
///
/// 二期起这是 [`to_min_unit_str`] 的 18 位特化（ERC-20 走 `to_min_unit_str` +
/// token decimals，如 USDT=6）。
#[must_use]
pub fn to_wei_str(amount: &str) -> Option<String> {
    to_min_unit_str(amount, 18)
}

/// [`VerifyOutcome`] → 业务判定（纯函数，语义表的代码化）。
#[derive(Debug, Clone, PartialEq)]
pub enum ChainPayVerdict {
    /// 放行：链上事实已核实（块高 + 实付最小单位 + ERC-20 时的代币合约）。
    Allow {
        block_number: u64,
        value_wei: String,
        /// ERC-20 路径 = 代币合约地址（展示/落库标注用）；native 恒 None。
        token: Option<String>,
    },
    /// 降级放行：RPC 故障（网络问题≠链上结论），调用方记警告日志。
    Degrade { detail: String },
    /// 拒绝：`status` + 人读原因；`retryable`=Pending（稍后重试）。
    Deny {
        status: u16,
        reason: String,
        retryable: bool,
    },
}

/// 语义映射（见上文语义表）。Pending 是**可重试**语义，错误文案已带「稍后重试」，
/// 不得当作欺诈处理。
#[must_use]
pub fn verdict_for(outcome: VerifyOutcome) -> ChainPayVerdict {
    match outcome {
        VerifyOutcome::Verified {
            block_number,
            value_wei,
            token,
            ..
        } => ChainPayVerdict::Allow {
            block_number,
            value_wei,
            token,
        },
        VerifyOutcome::Pending => ChainPayVerdict::Deny {
            status: 409,
            reason: "交易尚未上块确认（Pending）——非欺诈判定，请稍后重试".into(),
            retryable: true,
        },
        VerifyOutcome::Mismatch {
            field,
            expect,
            actual,
        } => ChainPayVerdict::Deny {
            status: 409,
            reason: format!("链上核验不符：{field} 期望 {expect}，链上实际 {actual}"),
            retryable: false,
        },
        VerifyOutcome::NotFound => ChainPayVerdict::Deny {
            status: 400,
            reason: "链上未找到该交易（txid 有误或已被节点裁剪）".into(),
            retryable: false,
        },
        VerifyOutcome::RpcError { detail } => ChainPayVerdict::Degrade { detail },
    }
}

/// 业务侧核验结论（[`check_chain_payment`] 的输出，调用方据此放行/拒绝/标注）。
#[derive(Debug, Clone)]
pub enum ChainPayCheck {
    /// 开关关闭（`NEXOS_CHAIN_VERIFY_ENABLED=0`）——回旧行为，调用方不加任何标注。
    Skipped,
    /// 核验通过：链上事实（落库到收据结构）。`token`=Some 表示 ERC-20 路径。
    Verified {
        chain_id: u64,
        block_number: u64,
        value_wei: String,
        token: Option<String>,
    },
    /// 未核验即放行（非 EVM 货币 / 缺链 ID / 缺收款地址 / 金额无法换算 /
    /// usdt 缺 ERC-20 合约配置）：自证收据 + 响应标注 `unverified` + 日志警告。
    Unverified(String),
    /// RPC 故障降级放行（自证收据 + 响应标注 `degraded` + 日志警告）。
    Degraded(String),
    /// 拒绝（status + 原因；Pending 可重试语义已写进原因文案）。
    Denied { status: u16, reason: String },
}

/// 链上核验输入 hints（两条业务线共用 [`check_chain_payment`]）。
#[derive(Debug, Clone, Copy, Default)]
pub struct ChainPayHints<'a> {
    /// 显式链 ID（body `chain_id`；缺省回落 `chain_str` 数值 → env 缺省）。
    pub chain_id: Option<u64>,
    /// 链字符串（可解析为数值时作链 ID，如 `"11155111"`；`"eth"` 等忽略）。
    pub chain_str: Option<&'a str>,
    /// 显式 RPC（body `rpc_url`，admin/条目 owner 自配——候选链第一段）。
    pub rpc_url: Option<&'a str>,
    /// 收款地址（悬赏 approve=poster 提供的 hunter 收款地址；网关=订单收款地址）。
    pub pay_to: Option<&'a str>,
    /// `pay_to` 缺失时是否回落节点级缺省（env `NEXOS_HUB_PAY_TO`）——**仅购买流**
    /// 置 true（条目收益归本节点运营者）；悬赏置 false（回落节点地址会错杀
    /// 发给 hunter 的真实支付），网关置 false（订单自带地址）。
    pub fallback_default_pay_to: bool,
    /// 金额规则（二期，默认 `Exact`）。接线定稿：**网关 confirm 与悬赏 approve
    /// 置 `AtLeast`**（充值/放款多打不亏待用户），**NexHub 购买保持 `Exact`**
    /// （商品定价等值——多打/少打都不对账，须按应付额整额打款）。
    pub amount_rule: AmountRule,
    /// ERC-20 合约地址（body `erc20_contract`，usdt@EVM 用；缺省回落网关 env
    /// `NEXOS_USDT_EVM_CONTRACT`。信任模型与 `rpc_url` 同款：请求方可指向
    /// 自选合约，链上事实/合约地址落库可审计）。
    pub erc20_contract: Option<&'a str>,
    /// ERC-20 小数位（body `erc20_decimals`；缺省回落网关 env
    /// `NEXOS_USDT_EVM_DECIMALS`，默认 6）。
    pub erc20_decimals: Option<u8>,
}

/// 业务核验编排（NexHub 购买/悬赏验收 + 网关 PaymentOrder confirm 共用的入口）。
///
/// 步骤：开关关 → [`ChainPayCheck::Skipped`]；txid 空 / 非 EVM 域货币
/// （native eth/evm 或 usdt）→ [`ChainPayCheck::Unverified`]（放行 + 标注，
/// 不静默）；链 ID 不可解析 → 同（usdt 特别说明：TRON 上的 USDT 无 EVM 链 ID，
/// 即落在此分支——人工通道）；金额换算 / ERC-20 合约定位 / 收款地址缺失 →
/// Unverified；否则构造 [`TxProof`]（按货币分 native / ERC-20 两路）走
/// [`ChainPayGate::verify`] 并按 [`verdict_for`] 语义映射。
///
/// `expected_value`：整数串 = 已是最小单位；带小数点 = native 按 18 位 /
/// ERC-20 按 token decimals 换算（[`to_min_unit_str`]）。
pub async fn check_chain_payment(
    gate: &ChainPayGate,
    currency: &str,
    txid: &str,
    expected_value: &str,
    hints: &ChainPayHints<'_>,
) -> ChainPayCheck {
    if !gate.enabled() {
        return ChainPayCheck::Skipped;
    }
    let txid = txid.trim();
    if txid.is_empty() {
        return ChainPayCheck::Unverified("txid 为空，无法链上核验".into());
    }
    // —— 货币分流（二期）：eth/evm 走 native；usdt 走 ERC-20（链 ID 可定位时）；
    //    其余（btc/nex/usdc/…）不在核验域。——
    let is_native = evm_native_currency(currency);
    let is_usdt = usdt_currency(currency);
    if !is_native && !is_usdt {
        return ChainPayCheck::Unverified(format!(
            "货币 {currency} 非 EVM 核验域（支持 eth/evm native 与 usdt@EVM ERC-20；btc/nex/usdc 仍自证）"
        ));
    }
    // —— 链 ID（usdt 也在此分流：定位不到 EVM 链 = TRON/人工通道）——
    let Some(chain_id) = resolve_chain_id(hints.chain_id, hints.chain_str, gate.default_chain_id())
    else {
        return ChainPayCheck::Unverified(if is_usdt {
            "usdt 未定位 EVM 链 ID（TRON 上的 USDT 不核验，走人工确认；EVM 链须 body chain_id / 数值 chain，或 env NEXOS_EVM_CHAIN_ID）".into()
        } else {
            "缺链 ID（body chain_id / 数值 chain，或 env NEXOS_EVM_CHAIN_ID）".into()
        });
    };
    // —— 金额换算 + ERC-20 凭证构造（usdt：body 合约 → env 合约；无则不猜）——
    let (expected_min_unit, erc20) = if is_native {
        match to_wei_str(expected_value) {
            Some(v) => (v, None),
            None => {
                return ChainPayCheck::Unverified(format!(
                    "应付金额 {expected_value:?} 无法换算为 wei（18 位小数假设）"
                ));
            }
        }
    } else {
        let contract = hints
            .erc20_contract
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| gate.usdt_evm_contract())
            .map(str::to_string);
        let Some(contract) = contract else {
            return ChainPayCheck::Unverified(
                "usdt@EVM 核验缺合约地址（body erc20_contract 或 env NEXOS_USDT_EVM_CONTRACT）——不猜合约地址，走人工确认".into(),
            );
        };
        let decimals = hints
            .erc20_decimals
            .unwrap_or_else(|| gate.usdt_evm_decimals());
        match to_min_unit_str(expected_value, decimals) {
            Some(v) => (v, Some(Erc20Spec { contract, decimals })),
            None => {
                return ChainPayCheck::Unverified(format!(
                    "应付金额 {expected_value:?} 无法换算为最小单位（USDT 小数位 {decimals}，env NEXOS_USDT_EVM_DECIMALS 可调）"
                ));
            }
        }
    };
    let pay_to = match hints
        .pay_to
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            hints
                .fallback_default_pay_to
                .then_some(())
                .and_then(|_| gate.default_pay_to())
        }) {
        Some(p) => p.to_string(),
        None => {
            return ChainPayCheck::Unverified(
                "缺收款地址（悬赏 approve 须 body pay_to；购买流须 env NEXOS_HUB_PAY_TO）".into(),
            );
        }
    };
    let proof = TxProof {
        chain_id,
        tx_hash: txid.to_string(),
        expected_to: pay_to,
        expected_value: expected_min_unit,
        amount_rule: hints.amount_rule,
        erc20,
    };
    let outcome = gate.verify(&proof, hints.rpc_url).await;
    match verdict_for(outcome) {
        ChainPayVerdict::Allow {
            block_number,
            value_wei,
            token,
        } => {
            let token_note = token
                .as_deref()
                .map(|c| format!(" token={c}"))
                .unwrap_or_default();
            eprintln!(
                "[chain-verify] 核验通过：chain={chain_id} tx={txid} block={block_number} value={value_wei}（最小单位）{token_note}"
            );
            ChainPayCheck::Verified {
                chain_id,
                block_number,
                value_wei,
                token,
            }
        }
        ChainPayVerdict::Degrade { detail } => {
            eprintln!(
                "[chain-verify] RPC 故障，降级放行（自证收据；S1 缓解=RPC 可用时核验）：{detail}"
            );
            ChainPayCheck::Degraded(detail)
        }
        ChainPayVerdict::Deny { status, reason, .. } => {
            eprintln!("[chain-verify] 拒绝：{reason}");
            ChainPayCheck::Denied { status, reason }
        }
    }
}

/// 把 [`ChainPayCheck`] 折成响应标注字段 `chain_verify`（None = 不标注——
/// 开关关闭的回旧行为 / 拒绝路径直接回错误响应）。
#[must_use]
pub fn chain_verify_json(check: &ChainPayCheck) -> Option<serde_json::Value> {
    match check {
        ChainPayCheck::Skipped | ChainPayCheck::Denied { .. } => None,
        ChainPayCheck::Verified {
            chain_id,
            block_number,
            value_wei,
            token,
        } => {
            let mut marker = serde_json::json!({
                "status": "verified",
                "chain_id": chain_id,
                "block_number": block_number,
                "value_wei": value_wei,
            });
            if let Some(contract) = token {
                marker["token"] = serde_json::json!(contract);
            }
            Some(marker)
        }
        ChainPayCheck::Degraded(detail) => Some(serde_json::json!({
            "status": "degraded",
            "detail": detail,
            "note": "RPC 故障降级放行（自证收据）",
        })),
        ChainPayCheck::Unverified(reason) => Some(serde_json::json!({
            "status": "unverified",
            "reason": reason,
        })),
    }
}

// ----------------------------------------------------------------------------
// ----------------------------------------------------------------------------
// 悬赏（bounty）持久化层（设计文档 §11 悬赏：大厅内「出资求活」的发现子资源）
// ----------------------------------------------------------------------------

/// 悬赏条目（hub_bounty 行）。
///
/// 与货币化（§10）的关系：货币化是「卖我的成果」（付费克隆），悬赏是「出钱求别人做
/// 某事」（如更新一个停更的 GitHub 仓库）。二者共用同一套虚拟货币与（一期自证 /
/// 二期链上）支付机制，但语义不同：悬赏必有奖励（`reward_sats>0` 且 `currency` 为
/// 真实链），且存在 `open→claimed→submitted→paid` 生命周期。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bounty {
    /// 悬赏 id（唯一键，服务端生成；不含空格/特殊字符）。
    pub id: String,
    /// 标题（想做什么）。
    #[serde(default)]
    pub title: String,
    /// 需求描述（目标 / 验收标准）。
    #[serde(default)]
    pub description: String,
    /// 标签（JSON 数组持久化）。
    #[serde(default)]
    pub tags: Vec<String>,
    /// 悬赏发布者（出资方）。
    #[serde(default)]
    pub poster: String,
    /// 奖励金额（最小货币单位；BTC=聪）。悬赏**必须** > 0。
    #[serde(default)]
    pub reward_sats: u64,
    /// 奖励货币：btc/nex/usdc/eth（与 os-wallet `ChainKind` 对齐）。
    #[serde(default = "default_bounty_currency")]
    pub currency: String,
    /// 目标链接（可选）：如停更的 GitHub 仓库 URL / issue。仅作参考，不强制抓取。
    #[serde(default)]
    pub target_url: String,
    /// 状态：open / claimed / submitted / paid / cancelled。
    #[serde(default = "default_bounty_status")]
    pub status: String,
    /// 认领者（hunter）。open/cancelled 时为 ""。
    #[serde(default)]
    pub claimed_by: String,
    /// 交付物链接（hunter 提交：PR / 仓库 URL）。
    #[serde(default)]
    pub solution_url: String,
    /// 截止时间（可选，ISO）。
    #[serde(default)]
    pub deadline: String,
    /// 创建时间（RFC3339）。
    pub created_at: String,
    /// 更新时间（RFC3339）。
    #[serde(default)]
    pub updated_at: String,
    /// 支付时间（paid 时填）。
    #[serde(default)]
    pub paid_at: String,
    /// 支付收据（自证 txid；phase-2 替换为链上验真）。paid 时填。
    #[serde(default)]
    pub payout_txid: String,
}

/// 悬赏默认状态（open）。
pub(super) fn default_bounty_status() -> String {
    "open".to_string()
}

/// 悬赏默认货币（btc）；创建时仍须 `reward_sats>0` 且非 `free`（由 `resolve_price` 校验）。
pub(super) fn default_bounty_currency() -> String {
    "btc".to_string()
}

/// 生成悬赏 id（时间戳纳秒 base36，足够唯一）。
pub(super) fn new_bounty_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("bty{:x}", nanos)
}

/// 列字段序（hub_bounty INSERT/SELECT 共用）。
pub(super) const BOUNTY_COLUMNS: &str = "id,title,description,tags,poster,reward_sats,currency,\
     target_url,status,claimed_by,solution_url,deadline,created_at,updated_at,paid_at,payout_txid";

pub(super) fn insert_bounty(conn: &Connection, b: &Bounty) -> rusqlite::Result<()> {
    conn.execute(
        &format!(
            "INSERT OR REPLACE INTO hub_bounty ({BOUNTY_COLUMNS}) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)"
        ),
        params![
            b.id,
            b.title,
            b.description,
            serde_json::to_string(&b.tags).unwrap_or_else(|_| "[]".into()),
            b.poster,
            b.reward_sats,
            b.currency,
            b.target_url,
            b.status,
            b.claimed_by,
            b.solution_url,
            b.deadline,
            b.created_at,
            b.updated_at,
            b.paid_at,
            b.payout_txid,
        ],
    )?;
    Ok(())
}

pub(super) fn bounty_from_row(row: &rusqlite::Row) -> rusqlite::Result<Bounty> {
    let tags_json: String = row.get(3)?;
    let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
    Ok(Bounty {
        id: row.get(0)?,
        title: row.get(1)?,
        description: row.get(2)?,
        tags,
        poster: row.get(4)?,
        reward_sats: row.get::<_, i64>(5)?.max(0) as u64,
        currency: row
            .get::<_, Option<String>>(6)?
            .unwrap_or_else(default_bounty_currency),
        target_url: row.get(7)?,
        status: row
            .get::<_, Option<String>>(8)?
            .unwrap_or_else(default_bounty_status),
        claimed_by: row.get(9)?,
        solution_url: row.get(10)?,
        deadline: row.get(11)?,
        created_at: row.get::<_, Option<String>>(12)?.unwrap_or_default(),
        updated_at: row.get::<_, Option<String>>(13)?.unwrap_or_default(),
        paid_at: row.get::<_, Option<String>>(14)?.unwrap_or_default(),
        payout_txid: row.get(15)?,
    })
}

/// 查询单条悬赏（按 id）。
pub(super) fn find_bounty(conn: &Connection, id: &str) -> rusqlite::Result<Option<Bounty>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {BOUNTY_COLUMNS} FROM hub_bounty WHERE id=?"
    ))?;
    stmt.query_row(params![id], bounty_from_row).optional()
}

/// [`claim_bounty`] 的判定结果（handler 映射 200 / 404 / 409）。
/// `Claimed` 装 Box：`Bounty` 本体 368 字节远大于另两个变体，避免整枚举膨胀
/// （clippy::large_enum_variant）。
pub(super) enum ClaimOutcome {
    /// 认领成功（携带更新后的悬赏，响应体与旧实现一致）。
    Claimed(Box<Bounty>),
    /// 悬赏不存在。
    NotFound,
    /// 非 open 状态（携带当前状态，用于 409 提示文案）。
    NotOpen(String),
}

/// 原子认领（P1 竞态修复）：`UPDATE ... WHERE id=? AND status='open'` 把
/// 「查→判 open→写」压进单语句，以影响行数判定结果。两个并发认领只有一个
/// UPDATE 命中，后到者 0 行 → [`ClaimOutcome::NotOpen`]（409），杜绝旧
/// find(锁1)→判→insert(锁2) 跨锁段的后写覆盖先写者且双双 200 的问题。
pub(super) fn claim_bounty(
    conn: &Connection,
    id: &str,
    hunter: &str,
) -> rusqlite::Result<ClaimOutcome> {
    let changed = conn.execute(
        "UPDATE hub_bounty SET status='claimed', claimed_by=?1, updated_at=?2 \
         WHERE id=?3 AND status='open'",
        params![hunter, now_iso(), id],
    )?;
    if changed == 0 {
        // 0 行两因：不存在（404）或已被认领/状态不符（409），补一次读区分
        return Ok(match find_bounty(conn, id)? {
            None => ClaimOutcome::NotFound,
            Some(b) => ClaimOutcome::NotOpen(b.status),
        });
    }
    let b = find_bounty(conn, id)?.expect("UPDATE 刚命中该行，回读必存在");
    Ok(ClaimOutcome::Claimed(Box::new(b)))
}

/// 悬赏列表：`status` 精确状态过滤、`q` 关键词（title/description/tags LIKE）、
/// 默认按创建时间降序。
pub(super) fn load_bounties(
    conn: &Connection,
    status: Option<&str>,
    q: Option<&str>,
) -> rusqlite::Result<Vec<Bounty>> {
    let mut conds: Vec<String> = Vec::new();
    let mut bind: Vec<String> = Vec::new();
    if let Some(s) = status {
        conds.push("status = ?".to_string());
        bind.push(s.to_string());
    }
    if let Some(q) = q {
        conds.push("(title LIKE ? OR description LIKE ? OR tags LIKE ?)".to_string());
        let like = format!("%{q}%");
        bind.push(like.clone());
        bind.push(like.clone());
        bind.push(like);
    }
    let mut sql = format!("SELECT {BOUNTY_COLUMNS} FROM hub_bounty");
    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }
    sql.push_str(" ORDER BY created_at DESC");
    let mut stmt = conn.prepare(&sql)?;
    let iter = stmt.query_map(params_from_iter(bind.iter()), bounty_from_row)?;
    let mut out = Vec::new();
    for e in iter {
        out.push(e?);
    }
    Ok(out)
}

// ----------------------------------------------------------------------------
// PR 审核流（2026-08-23 定稿：轻量版——git 通道提交分支 + SQLite 状态机；
// 分支经既有 git push 到裸仓，本层只做归因/审核/合并执行）
// ----------------------------------------------------------------------------
