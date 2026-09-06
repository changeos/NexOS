//! chain_verify——链上支付验真（dApp 一期核心，2026-08-31 立项）
//!
//! 定位：把"自证支付"（txid 非空即过）升级为**真实 EVM RPC 核验**——
//! `eth_getTransactionByHash` + `eth_getTransactionReceipt` 校验
//! 收款地址（to）/金额（value，wei）/链（chainId）/执行成功（status==1）。
//!
//! 使用方：NexHub `verify_payment`（购买/悬赏验收）与网关 PaymentOrder confirm
//! （接线见 nexhub_lobby.rs / api_gateway.rs）。本模块**只做核验不做发链交易**。
//!
//! env（注入点 main.rs / os-nexhub 初始化，全部 NEXOS_ 前缀）：
//! | env | 默认 | 作用 |
//! |---|---|---|
//! | `NEXOS_CHAIN_VERIFY_ENABLED` | `1` | 总开关；`0`=降级回旧行（非空即过），兼容开关 |
//! | `NEXOS_CHAIN_RPC_URLS` | （空） | 节点级 RPC 预设，JSON 对象 `{"<chain_id>": "<url>" 或 ["<url>",...]}` |
//! | `NEXOS_CHAIN_VERIFY_TIMEOUT_SECS` | `10` | 单次 RPC 请求超时 |
//!
//! RPC 选择顺序：请求携带的显式 rpc_url → `NEXOS_CHAIN_RPC_URLS[chain_id]` →
//! blockchain.rs 既有链预设的公共 RPC 兜底（见 `fallback_rpc_for`）。
//!
//! 日志：eprintln! 带 `[chain-verify]` 前缀（os-api 网关进程不装 tracing subscriber）。
//!
//! **分层边界**：本模块 = 核验本体（纯函数式 RPC 核验，不读 env、不持可变状态）；
//! 装配门面（env 读取 / RPC 三段候选拼接 / 可注入执行器 `ChainVerifyGate`）在
//! 接线层 nexhub_lobby.rs「链上支付验真」段，os-api PaymentOrder confirm 复用之。
//!
//! # 实现注记（2026-08-31 子代理A）
//!
//! - **调用序列**（单家 RPC 内严格串行）：`eth_chainId` 对链 →
//!   `eth_getTransactionByHash`（null → NotFound）→ to/value 比对 →
//!   `eth_getTransactionReceipt`（null → Pending；status != 0x1 → Mismatch）→
//!   Verified（blockNumber 来自 receipt）。
//! - **failover 语义**：只有**传输层失败**（连接失败/超时/HTTP 非 2xx/响应非
//!   JSON/缺 result/JSON-RPC error 对象）才切换下一家；链上结论
//!   （Verified/Pending/Mismatch/NotFound）一旦得出立即返回，不再试别家。
//!   全部端点失败 → RpcError，detail 汇总各家原因（URL 凭据段打码）。
//! - **fail-closed**：金额超 u128（>3.4e38 wei，真实链上不可能出现的攻击形状）、
//!   凭证格式非法（tx_hash 非 0x+64hex、to 非 0x+40hex、value 非十进制）一律
//!   不判 Verified——走 Mismatch（field 指明字段），**不发起网络请求**。
//! - hex quantity 解析容错：`0x` 前缀、`0x` 空尾数（=0）、前导零；
//!   地址比较统一小写（兼容 EIP-55 大小写校验和输入）。
//!
//! # 二期增量（2026-09-02 子代理，dApp 二期任务 1/2）
//!
//! - **ERC-20 Transfer 核验**：`TxProof.erc20 = Some(Erc20Spec{contract, decimals})`
//!   时切换为 **receipt 日志路径**——跳过 tx 级 to/value 比对（ERC-20 转账的
//!   tx.value=0、tx.to=合约），改为在 `eth_getTransactionReceipt` 的 `logs[]`
//!   里找 `address==contract && topics[0]==Transfer 签名哈希 && topics[2]==收款方`
//!   的日志，`data`（32 字节 uint256，按**最小单位**）对账；无匹配日志 →
//!   `Mismatch("erc20_log")`。`from`（topics[1]）**不校验**——凭证不含付款方
//!   期望（收款方+金额+合约才是对账要素），多笔同收款方 Transfer **不合计**
//!   （单笔日志须独立满足金额规则，见下）。`decimals` 只用于接线层把人类
//!   单位（如 `"10.00"`）换算成最小单位（`nexhub_lobby::to_min_unit_str`），
//!   核验层口径**写死为最小单位**（与 native 的 wei 口径同构，文档双写）。
//! - **金额规则** `AmountRule`：`Exact`（等值，默认，一期行为零变化）|
//!   `AtLeast`（≥ 应付额即过，链上 value 更小 → `Mismatch("value")`）。
//!   `Verified` 恒携带链上**实付** `value_wei`（AtLeast 超额时调用方/文档展示
//!   实付而非应付）。接线层定稿：网关 confirm=AtLeast、NexHub purchase=Exact、
//!   bounty approve=AtLeast（理由见 docs/GATEWAY_MONETIZATION.md 支付验真节）。
//! - **契约兼容**：`erc20: None` + `amount_rule: Exact`（默认）时核验路径与
//!   一期逐字节同语义；`VerifyOutcome::Verified` 增 `token: Option<String>`
//!   （ERC-20 时=合约地址小写，native=None）——结构体字段可扩展是既定契约。

use std::time::Duration;

use once_cell::sync::Lazy;

/// 日志前缀铁律：os-api 网关进程不装 tracing subscriber，本模块只走 stderr。
const LOG: &str = "[chain-verify]";

/// 进程级共享 `reqwest::Client`（与 os-api blockchain.rs `HTTP` 同款模式）。
/// 不设全局超时——单请求超时由 [`verify_evm_tx`] 的 `timeout` 参数按请求注入
/// （RequestBuilder::timeout 覆盖连接→读完整响应全程）。
static HTTP: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .user_agent("nexos-chain-verify/1")
        .build()
        .expect("构建共享 reqwest Client 失败")
});

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 金额规则（dApp 二期）：等值 or ≥ 应付额。
///
/// - `Exact`（默认）：链上金额须与期望**相等**——商品定价等值对账（NexHub 购买）；
/// - `AtLeast`：链上金额**≥** 期望即过——充值/悬赏「多打不亏待用户」；
///   不足 → `Mismatch("value")`。两种规则下 `Verified.value_wei` 都是链上**实付**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AmountRule {
    /// 等值比对（一期行为，向后兼容默认）。
    #[default]
    Exact,
    /// ≥ 应付额即过（网关 confirm / 悬赏 approve 用）。
    AtLeast,
}

/// ERC-20 核验凭证（dApp 二期）：`Some` 时走 receipt Transfer 日志路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Erc20Spec {
    /// 代币合约地址（0x + 40 hex；比较前统一小写；格式非法 fail-closed 不打网络）。
    pub contract: String,
    /// 小数位（USDT 主流=6）。**核验层不使用**——金额口径恒为最小单位；
    /// 此字段供接线层把人类单位（"10.00"）换算成最小单位（to_min_unit_str）。
    pub decimals: u8,
}

/// 一笔待核验的链上支付凭证。
#[derive(Debug, Clone)]
pub struct TxProof {
    /// EVM 链 ID（1=以太坊主网，11155111=Sepolia，…；须与 blockchain.rs 预设一致）
    pub chain_id: u64,
    /// 交易哈希（0x + 64 hex）
    pub tx_hash: String,
    /// 期望收款地址（0x + 40 hex；比较前统一小写）
    pub expected_to: String,
    /// 期望金额（最小单位十进制字符串：native=wei、ERC-20=token 最小单位；
    /// 与 RPC 返回的 hex quantity 换算后按 [`AmountRule`] 比较）
    pub expected_value: String,
    /// 金额规则（默认 `Exact` = 一期等值比对）。
    pub amount_rule: AmountRule,
    /// ERC-20 凭证（`Some` = 核 Transfer 日志；`None` = native 币核 tx.value）。
    pub erc20: Option<Erc20Spec>,
}

/// 核验结论。`Mismatch`/`Verified` 携带链上事实供调用方落库与展示。
#[derive(Debug, Clone, PartialEq)]
pub enum VerifyOutcome {
    /// 核验通过：已上块且 receipt.status==1，收款方/金额按 [`AmountRule`] 对上；
    /// `token`=Some(合约地址) 表示 ERC-20 路径（native 恒 None）。
    Verified {
        block_number: u64,
        to: String,
        value_wei: String,
        token: Option<String>,
    },
    /// 交易存在但未上块（内存池/未确认）——不判死，调用方可稍后重试
    Pending,
    /// 链上事实与期望不符。field ∈ "to" | "value" | "chain" | "status" |
    /// "tx_hash"（凭证格式非法，fail-closed 不打网络）| "erc20_contract"（合约
    /// 地址格式非法）| "erc20_log"（receipt 无匹配的 Transfer 日志）
    Mismatch {
        field: String,
        expect: String,
        actual: String,
    },
    /// 节点不认识这笔交易（typo 的 txid 或太老被裁剪）
    NotFound,
    /// RPC 不可达/响应非法/超时——网络问题而非链上结论
    RpcError { detail: String },
}

/// 预检+规范化后的凭证（内部用）：tx_hash 小写、to 小写、value 十进制 u128、
/// ERC-20 合约小写（decimals 原样透传，核验层不用）。
struct NormProof {
    chain_id: u64,
    tx_hash: String,
    to: String,
    value: u128,
    amount_rule: AmountRule,
    erc20: Option<Erc20Spec>,
}

// ----------------------------------------------------------------------------
// 核验主流程
// ----------------------------------------------------------------------------

/// 核验一笔 EVM 交易（按 rpc_urls 顺序 failover，全部失败 → RpcError）。
///
/// 步骤：eth_chainId 对链 → eth_getTransactionByHash（无 → NotFound）→
/// to/value 比对（不合 → Mismatch）→ eth_getTransactionReceipt
/// （无 → Pending；status!=1 → Mismatch("status")）→ Verified。
pub async fn verify_evm_tx(
    rpc_urls: &[String],
    proof: &TxProof,
    timeout: Duration,
) -> VerifyOutcome {
    // —— 0) 凭证预检（fail-closed：格式非法直接 Mismatch，不打网络）——
    let norm = match normalize_proof(proof) {
        Ok(n) => n,
        Err(outcome) => {
            if let VerifyOutcome::Mismatch { field, .. } = &outcome {
                eprintln!("{LOG} 凭证格式非法（field={field}），拒绝核验");
            }
            return outcome;
        }
    };

    if rpc_urls.is_empty() {
        eprintln!("{LOG} chain={} 未配置任何 RPC 端点", norm.chain_id);
        return VerifyOutcome::RpcError {
            detail: "未配置任何 RPC 端点".into(),
        };
    }
    eprintln!(
        "{LOG} chain={} tx={} 开始核验（{} 个端点，单请求超时 {:?}）",
        norm.chain_id,
        norm.tx_hash,
        rpc_urls.len(),
        timeout
    );

    // —— 1) 顺序 failover：传输层失败才换下家，链上结论立即返回 ——
    let mut failures: Vec<String> = Vec::new();
    for url in rpc_urls {
        match verify_against(url, &norm, timeout).await {
            Ok(outcome) => {
                eprintln!("{LOG} {} → {:?}", display_url(url), outcome_kind(&outcome));
                return outcome;
            }
            Err(reason) => {
                eprintln!("{LOG} {} 失败：{reason}，尝试下一端点", display_url(url));
                failures.push(format!("{}: {}", display_url(url), reason));
            }
        }
    }

    let detail = format!(
        "全部 {} 个 RPC 端点失败——{}",
        failures.len(),
        failures.join("；")
    );
    eprintln!("{LOG} {detail}");
    VerifyOutcome::RpcError { detail }
}

/// 收据公共段的结论（[`fetch_confirmed_receipt`] 输出）。
enum ReceiptStep {
    /// 未上块（内存池）。
    Pending,
    /// status != 0x1（执行回滚）——带既有 Mismatch 形状。
    Failed(VerifyOutcome),
    /// 已上块且成功，携带块高。
    Confirmed(u64),
}

/// 收据公共段：null → Pending；status != 0x1 → Mismatch("status")；
/// 否则 Ok(块高)。Err = 传输层失败（failover 信号）。
async fn fetch_confirmed_receipt(
    url: &str,
    p: &NormProof,
    timeout: Duration,
) -> Result<ReceiptStep, String> {
    let receipt = rpc_call(
        url,
        "eth_getTransactionReceipt",
        serde_json::json!([p.tx_hash]),
        timeout,
    )
    .await?;
    if receipt.is_null() {
        return Ok(ReceiptStep::Pending);
    }
    let rc = receipt
        .as_object()
        .ok_or_else(|| "eth_getTransactionReceipt 响应非对象".to_string())?;
    let status_hex = rc
        .get("status")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "receipt 缺 status 字段".to_string())?;
    let status = parse_quantity_u128(status_hex)
        .map_err(|e| format!("receipt.status 解析失败：{}", e.as_str()))?;
    if status != 1 {
        return Ok(ReceiptStep::Failed(VerifyOutcome::Mismatch {
            field: "status".into(),
            expect: "0x1".into(),
            actual: clip(status_hex, 18).to_string(),
        }));
    }
    let block_hex = rc
        .get("blockNumber")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "receipt 缺 blockNumber 字段".to_string())?;
    let block_number = parse_quantity_u64(block_hex)
        .map_err(|e| format!("receipt.blockNumber 解析失败：{}", e.as_str()))?;
    Ok(ReceiptStep::Confirmed(block_number))
}

/// 对单家 RPC 执行完整核验序列。
///
/// 返回 `Ok(VerifyOutcome)` = 链上结论（含 Mismatch/NotFound/Pending）；
/// 返回 `Err(reason)` = 传输层失败（调用方换下家）。
///
/// 二期分叉：`p.erc20 = Some` → ERC-20 路径（receipt 日志对账，tx 级 to/value
/// 不比对）；`None` → native 路径（一期语义，仅金额比对增加 AmountRule 分支）。
async fn verify_against(
    url: &str,
    p: &NormProof,
    timeout: Duration,
) -> Result<VerifyOutcome, String> {
    // 1) 对链：端点配错链（如拿 Sepolia 端点验主网单）必须拦下。
    let chain_resp = rpc_call(url, "eth_chainId", serde_json::json!([]), timeout).await?;
    let chain_str = chain_resp
        .as_str()
        .ok_or_else(|| "eth_chainId 响应非字符串".to_string())?;
    let actual_chain = parse_quantity_u64(chain_str)
        .map_err(|e| format!("eth_chainId 解析失败：{}", e.as_str()))?;
    if actual_chain != p.chain_id {
        return Ok(VerifyOutcome::Mismatch {
            field: "chain".into(),
            expect: p.chain_id.to_string(),
            actual: actual_chain.to_string(),
        });
    }

    // 2) 交易存在性（两条路径共用：NotFound / Pending 的区分依据）。
    let tx = rpc_call(
        url,
        "eth_getTransactionByHash",
        serde_json::json!([p.tx_hash]),
        timeout,
    )
    .await?;
    if tx.is_null() {
        return Ok(VerifyOutcome::NotFound);
    }

    // —— ERC-20 路径（二期）：tx 级字段不作对账依据，直接进收据日志。 ——
    if let Some(spec) = &p.erc20 {
        return verify_erc20_logs(url, p, spec, timeout).await;
    }

    let tx_obj = tx
        .as_object()
        .ok_or_else(|| "eth_getTransactionByHash 响应非对象".to_string())?;

    // 3) 收款地址比对（统一小写；合约创建交易 to=null 直接不符）。
    let actual_to = match tx_obj.get("to").and_then(|v| v.as_str()) {
        Some(s) => normalize_addr(s).ok_or_else(|| format!("链上 to 字段非法：{}", clip(s, 18)))?,
        None => {
            return Ok(VerifyOutcome::Mismatch {
                field: "to".into(),
                expect: p.to.clone(),
                actual: "null（合约创建交易）".into(),
            });
        }
    };
    if actual_to != p.to {
        return Ok(VerifyOutcome::Mismatch {
            field: "to".into(),
            expect: p.to.clone(),
            actual: actual_to,
        });
    }

    // 4) 金额比对（hex quantity → u128 → 按 AmountRule 比较）。
    let value_hex = tx_obj
        .get("value")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "交易缺 value 字段".to_string())?;
    let actual_value = match parse_quantity_u128(value_hex) {
        Ok(v) => v,
        // fail-closed：真实链不可能出现的天文数字，判不符而非报错重试。
        Err(QuantityError::Overflow) => {
            return Ok(VerifyOutcome::Mismatch {
                field: "value".into(),
                expect: p.value.to_string(),
                actual: format!("超出 u128 上限（{}…）", clip(value_hex, 34)),
            });
        }
        Err(QuantityError::Invalid) => {
            return Err(format!("value 非法 hex quantity：{}", clip(value_hex, 18)));
        }
    };
    let amount_ok = match p.amount_rule {
        AmountRule::Exact => actual_value == p.value,
        AmountRule::AtLeast => actual_value >= p.value,
    };
    if !amount_ok {
        return Ok(VerifyOutcome::Mismatch {
            field: "value".into(),
            expect: p.value.to_string(),
            actual: actual_value.to_string(),
        });
    }

    // 5) 收据：null=未上块（Pending）；status!=0x1=执行回滚。
    match fetch_confirmed_receipt(url, p, timeout).await? {
        ReceiptStep::Pending => Ok(VerifyOutcome::Pending),
        ReceiptStep::Failed(outcome) => Ok(outcome),
        ReceiptStep::Confirmed(block_number) => Ok(VerifyOutcome::Verified {
            block_number,
            to: actual_to,
            value_wei: actual_value.to_string(),
            token: None,
        }),
    }
}

/// `Transfer(address,address,uint256)` 的事件签名哈希（keccak256，全网常量）。
const TRANSFER_TOPIC0: &str =
    "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

/// ERC-20 路径（二期）：在 receipt.logs[] 里找
/// `address==contract && topics[0]==Transfer && topics[2]==收款方` 的日志，
/// `data`（uint256 最小单位）按 [`AmountRule`] 对账。
///
/// - 无任何「合约+签名+收款方」都匹配的日志 → `Mismatch("erc20_log")`；
/// - 有匹配日志但金额不满足规则 → `Mismatch("value")`（actual=匹配日志金额，
///   多笔用 `/` 连接；金额无法解析的日志计 `(unparsed)`）；
/// - `topics[1]`（from）不校验（凭证无付款方期望）；多笔**不合计**（单笔独立满足）。
async fn verify_erc20_logs(
    url: &str,
    p: &NormProof,
    spec: &Erc20Spec,
    timeout: Duration,
) -> Result<VerifyOutcome, String> {
    let receipt = rpc_call(
        url,
        "eth_getTransactionReceipt",
        serde_json::json!([p.tx_hash]),
        timeout,
    )
    .await?;
    if receipt.is_null() {
        return Ok(VerifyOutcome::Pending);
    }
    let rc = receipt
        .as_object()
        .ok_or_else(|| "eth_getTransactionReceipt 响应非对象".to_string())?;
    let status_hex = rc
        .get("status")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "receipt 缺 status 字段".to_string())?;
    let status = parse_quantity_u128(status_hex)
        .map_err(|e| format!("receipt.status 解析失败：{}", e.as_str()))?;
    if status != 1 {
        return Ok(VerifyOutcome::Mismatch {
            field: "status".into(),
            expect: "0x1".into(),
            actual: clip(status_hex, 18).to_string(),
        });
    }
    let block_hex = rc
        .get("blockNumber")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "receipt 缺 blockNumber 字段".to_string())?;
    let block_number = parse_quantity_u64(block_hex)
        .map_err(|e| format!("receipt.blockNumber 解析失败：{}", e.as_str()))?;

    // —— 日志扫描（形状非法的日志跳过——fail-closed：跳过只会更难通过，不会更易）。 ——
    let logs = rc
        .get("logs")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "receipt 缺 logs 字段（ERC-20 核验必需）".to_string())?;
    let mut matched_amounts: Vec<String> = Vec::new();
    let mut satisfied: Option<u128> = None;
    for log in logs {
        let Some(obj) = log.as_object() else { continue };
        // 合约地址匹配（小写）
        let log_addr = obj
            .get("address")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or_default();
        if !log_addr.eq_ignore_ascii_case(&spec.contract) {
            continue;
        }
        // topics[0]=Transfer 签名、topics[2]=收款方（32 字节 ABI 编码，取低 20 字节）
        let Some(topics) = obj.get("topics").and_then(|v| v.as_array()) else {
            continue;
        };
        if topics.len() < 3 {
            continue;
        }
        let topic0_ok = topics[0]
            .as_str()
            .map(|t| t.eq_ignore_ascii_case(TRANSFER_TOPIC0))
            .unwrap_or(false);
        if !topic0_ok {
            continue;
        }
        let topic2_ok = topics[2]
            .as_str()
            .and_then(topic_addr)
            .is_some_and(|a| a == p.to);
        if !topic2_ok {
            continue;
        }
        // 命中一条「合约+签名+收款方」的 Transfer：解析 data 金额（32 字节 uint256）。
        let data = obj.get("data").and_then(|v| v.as_str()).unwrap_or_default();
        match data_amount(data) {
            Some(amount) => {
                matched_amounts.push(amount.to_string());
                let ok = match p.amount_rule {
                    AmountRule::Exact => amount == p.value,
                    AmountRule::AtLeast => amount >= p.value,
                };
                if ok {
                    // 多笔不合计：单笔独立满足即记实付（取首个满足的）。
                    satisfied.get_or_insert(amount);
                }
            }
            // 金额无法解析（非 0x+64hex / 超 u128）：计入 actual 供人工复核，
            // 该日志不参与满足判定（fail-closed）。
            None => matched_amounts.push("(unparsed)".into()),
        }
    }
    match satisfied {
        Some(amount) => Ok(VerifyOutcome::Verified {
            block_number,
            to: p.to.clone(),
            value_wei: amount.to_string(),
            token: Some(spec.contract.clone()),
        }),
        None if matched_amounts.is_empty() => Ok(VerifyOutcome::Mismatch {
            field: "erc20_log".into(),
            expect: format!("Transfer(contract={}, to={})", spec.contract, p.to),
            actual: format!("receipt 无匹配日志（共 {} 条）", logs.len()),
        }),
        None => Ok(VerifyOutcome::Mismatch {
            field: "value".into(),
            expect: p.value.to_string(),
            actual: matched_amounts.join("/"),
        }),
    }
}

/// 32 字节 topic（0x + 64 hex）低 20 字节取地址（0x 前缀 + 小写）；形状非法 None。
fn topic_addr(topic: &str) -> Option<String> {
    let t = topic.strip_prefix("0x")?;
    if t.len() != 64 || !t.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("0x{}", &t[24..]))
}

/// 32 字节 uint256 data（0x + 64 hex）→ u128；形状非法/超 u128 → None
/// （真实代币金额远小于 u128；无法解析按 fail-closed 处理，见调用方）。
fn data_amount(data: &str) -> Option<u128> {
    let d = data.strip_prefix("0x")?;
    if d.len() != 64 || !d.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    parse_quantity_u128(data).ok()
}

// ----------------------------------------------------------------------------
// JSON-RPC over HTTP（共享 Client，单请求超时注入）
// ----------------------------------------------------------------------------

/// 发一次 JSON-RPC POST，取 `result` 字段值。
///
/// Err 条件（= failover 信号）：连接失败/超时、HTTP 非 2xx、响应非 JSON、
/// 缺 result、JSON-RPC error 对象（detail 带服务器 message）。
async fn rpc_call(
    url: &str,
    method: &str,
    params: serde_json::Value,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    // 敏感信息纪律：不落整段请求/响应体，错误 detail 只带状态码与服务器 message（截断）。
    let resp = HTTP
        .post(url)
        .json(&body)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| format!("请求失败：{}", clip(&e.to_string(), 160)))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("读取响应失败：{}", clip(&e.to_string(), 160)))?;
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("响应非 JSON：{e}"))?;
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("(服务器未给 message)");
        let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        return Err(format!("JSON-RPC error {code}: {}", clip(msg, 160)));
    }
    v.get("result")
        .cloned()
        .ok_or_else(|| "响应缺 result 字段".to_string())
}

// ----------------------------------------------------------------------------
// 解析/校验工具（纯函数，单测直测）
// ----------------------------------------------------------------------------

/// hex quantity 解析失败原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuantityError {
    /// 非 0x 前缀或含非 hex 字符
    Invalid,
    /// 超出 u128 表示范围（真实链上不可能，攻击形状）
    Overflow,
}

impl QuantityError {
    fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "非法 hex quantity",
            Self::Overflow => "超出 u128 上限",
        }
    }
}

/// 解析 JSON-RPC hex quantity（`0x` 前缀 / `0x` 空尾数=0 / 前导零 / u128 溢出）。
fn parse_quantity_u128(s: &str) -> Result<u128, QuantityError> {
    let digits = s.strip_prefix("0x").ok_or(QuantityError::Invalid)?;
    let mut acc: u128 = 0;
    for c in digits.chars() {
        let d = u128::from(c.to_digit(16).ok_or(QuantityError::Invalid)?);
        acc = acc
            .checked_mul(16)
            .and_then(|a| a.checked_add(d))
            .ok_or(QuantityError::Overflow)?;
    }
    Ok(acc)
}

/// hex quantity → u64（chainId/blockNumber 等窄量；溢出报 Overflow）。
fn parse_quantity_u64(s: &str) -> Result<u64, QuantityError> {
    let wide = parse_quantity_u128(s)?;
    u64::try_from(wide).map_err(|_| QuantityError::Overflow)
}

/// 校验 `0x` + `n` 位 hex（tx_hash n=64，地址 n=40），大小写不限。
fn is_0x_hex(s: &str, hex_len: usize) -> bool {
    match s.strip_prefix("0x") {
        Some(rest) => rest.len() == hex_len && rest.chars().all(|c| c.is_ascii_hexdigit()),
        None => false,
    }
}

/// 地址规范化：校验 `0x`+40hex 后转小写（兼容 EIP-55 校验和输入）。
fn normalize_addr(s: &str) -> Option<String> {
    let t = s.trim();
    if is_0x_hex(t, 40) {
        Some(t.to_ascii_lowercase())
    } else {
        None
    }
}

/// 十进制 wei 字符串 → u128（拒绝空白/非数字/溢出）。
fn parse_dec_u128(s: &str) -> Result<u128, QuantityError> {
    let t = s.trim();
    if t.is_empty() || !t.chars().all(|c| c.is_ascii_digit()) {
        return Err(QuantityError::Invalid);
    }
    let mut acc: u128 = 0;
    for c in t.chars() {
        let d = u128::from(c.to_digit(10).ok_or(QuantityError::Invalid)?);
        acc = acc
            .checked_mul(10)
            .and_then(|a| a.checked_add(d))
            .ok_or(QuantityError::Overflow)?;
    }
    Ok(acc)
}

/// 凭证预检+规范化。Err(Mismatch) = 格式非法（field 指明字段，fail-closed）。
fn normalize_proof(proof: &TxProof) -> Result<NormProof, VerifyOutcome> {
    let tx_hash = proof.tx_hash.trim().to_ascii_lowercase();
    if !is_0x_hex(&tx_hash, 64) {
        return Err(VerifyOutcome::Mismatch {
            field: "tx_hash".into(),
            expect: "0x + 64 位 hex".into(),
            actual: clip(&proof.tx_hash, 34).to_string(),
        });
    }
    let Some(to) = normalize_addr(&proof.expected_to) else {
        return Err(VerifyOutcome::Mismatch {
            field: "to".into(),
            expect: "0x + 40 位 hex".into(),
            actual: clip(&proof.expected_to, 26).to_string(),
        });
    };
    let value = match parse_dec_u128(&proof.expected_value) {
        Ok(v) => v,
        Err(e) => {
            return Err(VerifyOutcome::Mismatch {
                field: "value".into(),
                expect: clip(&proof.expected_value, 26).to_string(),
                actual: format!("期望金额非法（{}）", e.as_str()),
            });
        }
    };
    // ERC-20 合约地址格式预检（fail-closed：不猜、不打网络）。
    let erc20 = match &proof.erc20 {
        None => None,
        Some(spec) => match normalize_addr(&spec.contract) {
            Some(contract) => Some(Erc20Spec {
                contract,
                decimals: spec.decimals,
            }),
            None => {
                return Err(VerifyOutcome::Mismatch {
                    field: "erc20_contract".into(),
                    expect: "0x + 40 位 hex".into(),
                    actual: clip(&spec.contract, 26).to_string(),
                });
            }
        },
    };
    Ok(NormProof {
        chain_id: proof.chain_id,
        tx_hash,
        to,
        value,
        amount_rule: proof.amount_rule,
        erc20,
    })
}

/// 截断长串（错误 detail 防日志放大 + 不落整段响应体）。
fn clip(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

/// 日志用 URL 打码：剥掉 `scheme://user:pass@` 的凭据段（如 API-key 型 RPC）。
fn display_url(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => match rest.split_once('@') {
            Some((_, host)) => format!("{scheme}://***@{host}"),
            None => url.to_string(),
        },
        None => url.to_string(),
    }
}

/// 结论的短名（日志用，不带负载数据）。
fn outcome_kind(o: &VerifyOutcome) -> &'static str {
    match o {
        VerifyOutcome::Verified { .. } => "Verified",
        VerifyOutcome::Pending => "Pending",
        VerifyOutcome::Mismatch { .. } => "Mismatch",
        VerifyOutcome::NotFound => "NotFound",
        VerifyOutcome::RpcError { .. } => "RpcError",
    }
}

// ----------------------------------------------------------------------------
// 公共 RPC 兜底
// ----------------------------------------------------------------------------

/// blockchain.rs 链预设（`chain_presets()`，7 条）的公共 RPC 兜底列表。
///
/// 覆盖范围：公链 ethereum(1)/sepolia(11155111)/optimism(10)/arbitrum(42161)/
/// base(8453) 给无 key 公共端点；dev(1337) 给本机 anvil/ganache 默认端口
/// （blockchain.rs「JSON-RPC 端口默认 8545」）；custom(0) 与未知链无兜底（空）。
///
/// **注意：公共端点可用性/限流不作任何保证**（无 SLA、可能区域性不可达或停服），
/// 生产环境务必用 `NEXOS_CHAIN_RPC_URLS` 自配付费/自建节点，本表只是「没配置时
/// 也别退回白嫖校验」的最后防线。
pub fn fallback_rpc_for(chain_id: u64) -> Vec<String> {
    match chain_id {
        // Ethereum 主网（2026-08-31 国内直连实测换血：cloudflare 拒答/ankr 要 key/llamarpc 停服 → 剔除）
        1 => vec![
            "https://ethereum-rpc.publicnode.com".into(),
        ],
        // Sepolia 测试网（rpc.sepolia.org 404、rpc2.sepolia.org 超时 → 剔除）
        11_155_111 => vec![
            "https://ethereum-sepolia-rpc.publicnode.com".into(),
            "https://sepolia.gateway.tenderly.co".into(),
        ],
        // Optimism（mainnet.optimism.io 直连超时 → 剔除）
        10 => vec![
            "https://optimism-rpc.publicnode.com".into(),
        ],
        // Arbitrum One
        42_161 => vec![
            "https://arbitrum-one-rpc.publicnode.com".into(),
            "https://arb1.arbitrum.io/rpc".into(),
        ],
        // Base
        8453 => vec![
            "https://base-rpc.publicnode.com".into(),
            "https://mainnet.base.org".into(),
        ],
        // 本地开发链（anvil/ganache 默认 8545；连不上即连接拒绝，fail-fast）
        1337 => vec!["http://127.0.0.1:8545".into()],
        // custom(0) / 未预设链：无公共兜底
        _ => Vec::new(),
    }
}

// ----------------------------------------------------------------------------
// 测试：本地真实 TCP HTTP mock JSON-RPC（std TcpListener，reqwest 端到端可达）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// 单请求 mock 行为。
    enum Mock {
        /// JSON-RPC result 字段值
        Result(serde_json::Value),
        /// JSON-RPC error 对象（message 透传到 RpcError detail）
        RpcErr(&'static str),
        /// HTTP 500（传输层失败 → 应 failover）
        Http500,
        /// 接受连接但不写响应（验超时 → 应 failover）
        Hang,
    }

    /// 起一个真实 TCP JSON-RPC mock：按请求里的 `method` 查 `plan` 出响应。
    /// 返回（URL, 收到的 method 序列）——后者用于断言调用次序与「未打网络」。
    /// 上限 64 个请求后自然停（测试远用不完，防线程泄漏）。
    fn spawn_rpc_mock(
        plan: impl Fn(&str) -> Mock + Send + Sync + 'static,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 失败");
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = seen.clone();
        std::thread::spawn(move || {
            for _ in 0..64 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                // 读完整 HTTP 请求（头 + content-length 定界的 body）
                let mut buf: Vec<u8> = Vec::new();
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                while take_http_body(&buf).is_none() && std::time::Instant::now() < deadline {
                    let mut chunk = [0u8; 4096];
                    match stream.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        Err(_) => break,
                    }
                }
                let body = take_http_body(&buf).unwrap_or_default();
                let method = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v.get("method").and_then(|m| m.as_str()).map(str::to_owned))
                    .unwrap_or_default();
                seen_clone
                    .lock()
                    .expect("seen poisoned")
                    .push(method.clone());
                match plan(&method) {
                    Mock::Result(v) => {
                        let payload = format!(r#"{{"jsonrpc":"2.0","id":1,"result":{v}}}"#);
                        write_http(&mut stream, 200, &payload);
                    }
                    Mock::RpcErr(msg) => {
                        let payload = format!(
                            r#"{{"jsonrpc":"2.0","id":1,"error":{{"code":-32000,"message":"{msg}"}}}}"#
                        );
                        write_http(&mut stream, 200, &payload);
                    }
                    Mock::Http500 => write_http(&mut stream, 500, ""),
                    Mock::Hang => std::thread::sleep(Duration::from_secs(5)), // 不写，逼客户端超时
                }
            }
        });
        (url, seen)
    }

    /// 从原始字节里取出 HTTP body（按 Content-Length 定界；未收全 → None）。
    fn take_http_body(buf: &[u8]) -> Option<String> {
        let sep = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
        let head = String::from_utf8_lossy(&buf[..sep]).to_ascii_lowercase();
        let len: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:").map(str::trim))?
            .parse()
            .ok()?;
        let start = sep + 4;
        (buf.len() >= start + len)
            .then(|| String::from_utf8_lossy(&buf[start..start + len]).into_owned())
    }

    fn write_http(stream: &mut std::net::TcpStream, status: u16, body: &str) {
        let reason = if status == 200 {
            "OK"
        } else {
            "Internal Server Error"
        };
        let resp = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(resp.as_bytes());
        let _ = stream.flush();
    }

    /// 全绿 mock 计划（chainId/to/value/块高可定制）。
    fn green(
        chain: &'static str,
        to: String,
        value_hex: String,
    ) -> impl Fn(&str) -> Mock + Send + Sync + 'static {
        move |m: &str| match m {
            "eth_chainId" => Mock::Result(serde_json::json!(chain)),
            "eth_getTransactionByHash" => {
                Mock::Result(serde_json::json!({ "to": to, "value": value_hex }))
            }
            "eth_getTransactionReceipt" => Mock::Result(serde_json::json!({
                "status": "0x1",
                "blockNumber": "0x112a8", // 70312
            })),
            _ => Mock::Result(serde_json::Value::Null),
        }
    }

    /// 标准凭证：主网 1 ETH → 0xcdcd…cd（40 位）。
    fn proof() -> TxProof {
        TxProof {
            chain_id: 1,
            tx_hash: format!("0x{}", "ab".repeat(32)),
            expected_to: format!("0x{}", "cd".repeat(20)),
            expected_value: "1000000000000000000".into(),
            amount_rule: AmountRule::Exact,
            erc20: None,
        }
    }

    // ---- ERC-20 mock 工具（二期）----

    /// 测试用 USDT 形状凭证：主网、合约 0xee…ee、收款方 0xcd…cd、10_000_000
    /// 最小单位（decimals=6 → 10 USDT）。
    fn erc20_proof() -> TxProof {
        TxProof {
            chain_id: 1,
            tx_hash: format!("0x{}", "ab".repeat(32)),
            expected_to: format!("0x{}", "cd".repeat(20)),
            expected_value: "10000000".into(),
            amount_rule: AmountRule::Exact,
            erc20: Some(Erc20Spec {
                contract: format!("0x{}", "ee".repeat(20)),
                decimals: 6,
            }),
        }
    }

    /// 32 字节 topic（地址左补 24 个零字节；0x 前缀大小写兼容）。
    fn addr_topic(addr: &str) -> String {
        let a = addr.trim();
        let hex = a
            .strip_prefix("0x")
            .or_else(|| a.strip_prefix("0X"))
            .unwrap_or(a);
        format!("0x000000000000000000000000{}", hex.to_ascii_lowercase())
    }

    /// 构造一条 Transfer 日志（address/topics/data 全可控）。
    fn transfer_log(contract: &str, from: &str, to: &str, amount: u128) -> serde_json::Value {
        serde_json::json!({
            "address": contract,
            "topics": [TRANSFER_TOPIC0, addr_topic(from), addr_topic(to)],
            "data": format!("0x{:064x}", amount),
        })
    }

    /// ERC-20 全绿 mock：tx.to=合约、value=0（ERC-20 转账的 tx 级形状）；
    /// receipt 带给定的日志列表（status 1 / 块高 70312）。
    fn green_erc20(
        logs: Vec<serde_json::Value>,
    ) -> impl Fn(&str) -> Mock + Send + Sync + 'static {
        move |m: &str| match m {
            "eth_chainId" => Mock::Result(serde_json::json!("0x1")),
            "eth_getTransactionByHash" => Mock::Result(serde_json::json!({
                "to": format!("0x{}", "ee".repeat(20)),
                "value": "0x0",
            })),
            "eth_getTransactionReceipt" => Mock::Result(serde_json::json!({
                "status": "0x1",
                "blockNumber": "0x112a8",
                "logs": logs,
            })),
            _ => Mock::Result(serde_json::Value::Null),
        }
    }

    /// 标准方向的 ERC-20 凭证对应的成对地址（from=0x11..，payee=0xcd..）。
    const ERC_FROM: &str = "0x1111111111111111111111111111111111111111";

    /// 1 ETH 的 64 位定宽 hex quantity（带前导零，验解析容错）。
    fn one_eth_padded_hex() -> String {
        format!("0x{:064x}", 1_000_000_000_000_000_000u128)
    }

    // 1. 全绿路径：chainId→tx→receipt 依次各一次；前导零 quantity、
    //    EIP-55 大写期望地址（大小写不敏感）都能过。
    #[tokio::test]
    async fn verified_green_path_with_leading_zeros_and_checksummed_to() {
        let (url, seen) = spawn_rpc_mock(green(
            "0x1",
            format!("0x{}", "cd".repeat(20)),
            one_eth_padded_hex(),
        ));
        // 期望地址故意大写（校验和形状），链上返回小写 → 应判一致
        let mut p = proof();
        p.expected_to = format!("0x{}", "CD".repeat(20));
        let out = verify_evm_tx(std::slice::from_ref(&url), &p, Duration::from_secs(2)).await;
        assert_eq!(
            out,
            VerifyOutcome::Verified {
                block_number: 70_312, // 0x112a8
                to: format!("0x{}", "cd".repeat(20)),
                value_wei: "1000000000000000000".into(),
                token: None,
            },
            "前导零/大小写不应影响核验"
        );
        assert_eq!(
            *seen.lock().unwrap(),
            [
                "eth_chainId",
                "eth_getTransactionByHash",
                "eth_getTransactionReceipt"
            ],
            "应按契约序列恰好调用 3 次"
        );
    }

    // 2. Pending：有交易无收据（内存池/未确认）。
    #[tokio::test]
    async fn pending_when_tx_has_no_receipt() {
        let (url, _) = spawn_rpc_mock(|m: &str| match m {
            "eth_getTransactionReceipt" => Mock::Result(serde_json::Value::Null),
            _ => green(
                "0x1",
                format!("0x{}", "cd".repeat(20)),
                one_eth_padded_hex(),
            )(m),
        });
        let out = verify_evm_tx(&[url], &proof(), Duration::from_secs(2)).await;
        assert_eq!(out, VerifyOutcome::Pending);
    }

    // 3. Mismatch-to：链上收款地址与期望不符，actual 带链上事实。
    #[tokio::test]
    async fn mismatch_to_reports_onchain_address() {
        let bad_to = format!("0x{}", "ee".repeat(20));
        let (url, _) = spawn_rpc_mock(green("0x1", bad_to.clone(), one_eth_padded_hex()));
        let out = verify_evm_tx(&[url], &proof(), Duration::from_secs(2)).await;
        assert_eq!(
            out,
            VerifyOutcome::Mismatch {
                field: "to".into(),
                expect: format!("0x{}", "cd".repeat(20)),
                actual: bad_to,
            }
        );
    }

    // 4. Mismatch-value：金额不符，expect/actual 均为 wei 十进制。
    #[tokio::test]
    async fn mismatch_value_reports_onchain_wei() {
        let (url, _) = spawn_rpc_mock(green("0x1", format!("0x{}", "cd".repeat(20)), "0x2".into()));
        let out = verify_evm_tx(&[url], &proof(), Duration::from_secs(2)).await;
        assert_eq!(
            out,
            VerifyOutcome::Mismatch {
                field: "value".into(),
                expect: "1000000000000000000".into(),
                actual: "2".into(),
            }
        );
    }

    // 5. Mismatch-chain：端点配错链（0x89=Polygon 对 1=主网），应在查交易前短路。
    #[tokio::test]
    async fn mismatch_chain_short_circuits_before_tx_lookup() {
        let (url, seen) = spawn_rpc_mock(green(
            "0x89",
            format!("0x{}", "cd".repeat(20)),
            one_eth_padded_hex(),
        ));
        let out = verify_evm_tx(&[url], &proof(), Duration::from_secs(2)).await;
        assert_eq!(
            out,
            VerifyOutcome::Mismatch {
                field: "chain".into(),
                expect: "1".into(),
                actual: "137".into(),
            }
        );
        assert_eq!(
            *seen.lock().unwrap(),
            ["eth_chainId"],
            "链不符应立即短路，不再查交易"
        );
    }

    // 6. NotFound：节点不认识这笔交易（typo 的 txid）。
    #[tokio::test]
    async fn tx_not_found() {
        let (url, _) = spawn_rpc_mock(|m: &str| match m {
            "eth_getTransactionByHash" => Mock::Result(serde_json::Value::Null),
            "eth_chainId" => Mock::Result(serde_json::json!("0x1")),
            _ => Mock::Result(serde_json::Value::Null),
        });
        let out = verify_evm_tx(&[url], &proof(), Duration::from_secs(2)).await;
        assert_eq!(out, VerifyOutcome::NotFound);
    }

    // 7. status != 0x1：交易上块但执行回滚（reverted），不得放款。
    #[tokio::test]
    async fn reverted_status_is_mismatch() {
        let (url, _) = spawn_rpc_mock(|m: &str| match m {
            "eth_getTransactionReceipt" => {
                Mock::Result(serde_json::json!({ "status": "0x0", "blockNumber": "0x1" }))
            }
            _ => green(
                "0x1",
                format!("0x{}", "cd".repeat(20)),
                one_eth_padded_hex(),
            )(m),
        });
        let out = verify_evm_tx(&[url], &proof(), Duration::from_secs(2)).await;
        assert_eq!(
            out,
            VerifyOutcome::Mismatch {
                field: "status".into(),
                expect: "0x1".into(),
                actual: "0x0".into(),
            }
        );
    }

    // 8. to=null（合约创建交易）：不可能是给固定收款地址的转账。
    #[tokio::test]
    async fn contract_creation_null_to_is_mismatch() {
        let (url, _) = spawn_rpc_mock(|m: &str| match m {
            "eth_getTransactionByHash" => Mock::Result(serde_json::json!({
                "to": serde_json::Value::Null,
                "value": one_eth_padded_hex(),
            })),
            _ => green(
                "0x1",
                format!("0x{}", "cd".repeat(20)),
                one_eth_padded_hex(),
            )(m),
        });
        let out = verify_evm_tx(&[url], &proof(), Duration::from_secs(2)).await;
        assert_eq!(
            out,
            VerifyOutcome::Mismatch {
                field: "to".into(),
                expect: format!("0x{}", "cd".repeat(20)),
                actual: "null（合约创建交易）".into(),
            }
        );
    }

    // 9. RPC 500 → failover 到第二家成功（链上结论只采信健康端点）。
    #[tokio::test]
    async fn rpc_500_fails_over_to_second_provider() {
        let (bad, seen_bad) = spawn_rpc_mock(|_: &str| Mock::Http500);
        let (good, _) = spawn_rpc_mock(green(
            "0x1",
            format!("0x{}", "cd".repeat(20)),
            one_eth_padded_hex(),
        ));
        let out = verify_evm_tx(&[bad, good], &proof(), Duration::from_secs(2)).await;
        assert!(
            matches!(out, VerifyOutcome::Verified { .. }),
            "应换到第二家并验过: {out:?}"
        );
        assert_eq!(
            seen_bad.lock().unwrap().len(),
            1,
            "第一家只应被打一次（chainId 即失败）"
        );
    }

    // 10. JSON-RPC error 对象：RpcError detail 带服务器 message。
    #[tokio::test]
    async fn jsonrpc_error_surfaces_server_message() {
        let (url, _) = spawn_rpc_mock(|_: &str| Mock::RpcErr("rate limit exceeded"));
        let out = verify_evm_tx(&[url], &proof(), Duration::from_secs(2)).await;
        match out {
            VerifyOutcome::RpcError { detail } => {
                assert!(
                    detail.contains("rate limit exceeded"),
                    "应带服务器 message: {detail}"
                );
            }
            other => panic!("应 RpcError: {other:?}"),
        }
    }

    // 11. 全部端点超时 → RpcError，detail 汇总两家失败。
    #[tokio::test]
    async fn all_providers_timeout_yields_rpc_error() {
        let (a, _) = spawn_rpc_mock(|_: &str| Mock::Hang);
        let (b, _) = spawn_rpc_mock(|_: &str| Mock::Hang);
        let start = std::time::Instant::now();
        let out = verify_evm_tx(&[a, b], &proof(), Duration::from_millis(200)).await;
        match out {
            VerifyOutcome::RpcError { detail } => {
                assert!(detail.contains("全部 2 个"), "应汇总两家: {detail}");
            }
            other => panic!("应 RpcError: {other:?}"),
        }
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "超时应在两家×单请求量级内返回，实测 {:?}",
            start.elapsed()
        );
    }

    // 12. 未配置端点 → RpcError（而非静默放行）。
    #[tokio::test]
    async fn empty_rpc_urls_is_rpc_error() {
        let out = verify_evm_tx(&[], &proof(), Duration::from_secs(1)).await;
        match out {
            VerifyOutcome::RpcError { detail } => {
                assert!(detail.contains("未配置"), "detail: {detail}");
            }
            other => panic!("应 RpcError: {other:?}"),
        }
    }

    // 13. 非法 tx_hash：格式不过直接 Mismatch，一次网络请求都不发。
    #[tokio::test]
    async fn invalid_tx_hash_rejected_without_network() {
        let (url, seen) = spawn_rpc_mock(green(
            "0x1",
            format!("0x{}", "cd".repeat(20)),
            one_eth_padded_hex(),
        ));
        let mut p = proof();
        p.tx_hash = "0xdeadbeef".into(); // 只有 8 位 hex
        let out = verify_evm_tx(&[url], &p, Duration::from_secs(1)).await;
        match out {
            VerifyOutcome::Mismatch { field, expect, .. } => {
                assert_eq!(field, "tx_hash");
                assert_eq!(expect, "0x + 64 位 hex");
            }
            other => panic!("应 Mismatch(tx_hash): {other:?}"),
        }
        assert!(seen.lock().unwrap().is_empty(), "格式非法不应打网络");
    }

    // 14. 期望地址/金额格式非法：同样 fail-closed 不打网络。
    #[tokio::test]
    async fn invalid_expected_to_and_value_rejected_upfront() {
        let (url, seen) = spawn_rpc_mock(green(
            "0x1",
            format!("0x{}", "cd".repeat(20)),
            one_eth_padded_hex(),
        ));
        let mut p = proof();
        p.expected_to = "0xzz-not-an-address".into();
        let out = verify_evm_tx(std::slice::from_ref(&url), &p, Duration::from_secs(1)).await;
        assert_eq!(outcome_kind(&out), "Mismatch", "地址非法: {out:?}");
        p = proof();
        p.expected_value = "12.5ETH".into();
        let out = verify_evm_tx(&[url], &p, Duration::from_secs(1)).await;
        assert_eq!(outcome_kind(&out), "Mismatch", "金额非法: {out:?}");
        assert!(seen.lock().unwrap().is_empty(), "参数非法不应打网络");
    }

    // 15. 链上金额超 u128（2^128 wei）：fail-closed 判 Mismatch 而非报错重试。
    #[tokio::test]
    async fn onchain_value_beyond_u128_is_mismatch() {
        // 0x1 + 32 个 0 = 2^128，恰好超出 u128
        let huge_hex = format!("0x1{}", "0".repeat(32));
        let (url, _) = spawn_rpc_mock(green("0x1", format!("0x{}", "cd".repeat(20)), huge_hex));
        let mut p = proof();
        p.expected_value = "340282366920938463463374607431768211456".into(); // 2^128
        let out = verify_evm_tx(&[url], &p, Duration::from_secs(2)).await;
        match out {
            VerifyOutcome::Mismatch { field, actual, .. } => {
                assert_eq!(field, "value");
                assert!(actual.contains("u128"), "actual 应说明溢出: {actual}");
            }
            other => panic!("天文数字应 fail-closed: {other:?}"),
        }
    }

    // 16. 期望金额超 u128：预检阶段拦下（fail-closed），不打网络。
    #[tokio::test]
    async fn expected_value_beyond_u128_rejected_upfront() {
        let (url, seen) = spawn_rpc_mock(green(
            "0x1",
            format!("0x{}", "cd".repeat(20)),
            one_eth_padded_hex(),
        ));
        let mut p = proof();
        p.expected_value = format!("1{}", "0".repeat(39)); // 10^39 > u128::MAX(~3.4e38)
        let out = verify_evm_tx(&[url], &p, Duration::from_secs(1)).await;
        assert_eq!(outcome_kind(&out), "Mismatch");
        assert!(seen.lock().unwrap().is_empty());
    }

    // 17. fallback_rpc_for：7 条链预设的兜底覆盖（公链给无钥端点，dev 给本机）。
    #[test]
    fn fallback_rpc_covers_chain_presets() {
        let mainnet = fallback_rpc_for(1);
        assert!(mainnet
            .iter()
            .any(|u| u.contains("ethereum-rpc.publicnode.com")));
        assert!(
            !mainnet.iter().any(|u| u.contains("cloudflare-eth.com")),
            "2026-08-31 国内实测拒答已剔除"
        );
        let sepolia = fallback_rpc_for(11_155_111);
        assert!(sepolia
            .iter()
            .any(|u| u.contains("ethereum-sepolia-rpc.publicnode.com")));
        assert!(sepolia.iter().any(|u| u.contains("tenderly.co")));
        assert!(!fallback_rpc_for(10).is_empty(), "Optimism");
        assert!(!fallback_rpc_for(42_161).is_empty(), "Arbitrum One");
        assert!(!fallback_rpc_for(8453).is_empty(), "Base");
        assert_eq!(
            fallback_rpc_for(1337),
            vec!["http://127.0.0.1:8545"],
            "dev 链默认本机节点"
        );
        assert!(fallback_rpc_for(0).is_empty(), "custom 链无兜底");
        assert!(fallback_rpc_for(999_999).is_empty(), "未知链无兜底");
    }

    // 18. 解析工具直测：`0x`=0、前导零、非法前缀、u64 窄化。
    #[test]
    fn quantity_parsing_edges() {
        assert_eq!(parse_quantity_u128("0x"), Ok(0));
        assert_eq!(parse_quantity_u128("0x0"), Ok(0));
        assert_eq!(
            parse_quantity_u128(
                "0x0000000000000000000000000000000000000000000000000000000000002710"
            ),
            Ok(10_000)
        );
        assert_eq!(parse_quantity_u128("0x10"), Ok(16));
        assert_eq!(
            parse_quantity_u128("10"),
            Err(QuantityError::Invalid),
            "缺 0x 前缀"
        );
        assert_eq!(parse_quantity_u128("0xgg"), Err(QuantityError::Invalid));
        assert_eq!(parse_quantity_u128("0x1"), Ok(1));
        assert_eq!(
            parse_quantity_u128("0xffffffffffffffffffffffffffffffff"),
            Ok(u128::MAX)
        );
        assert_eq!(
            parse_quantity_u128(&format!("0x1{}", "0".repeat(32))),
            Err(QuantityError::Overflow),
            "2^128 溢出"
        );
        assert_eq!(parse_quantity_u64("0x1"), Ok(1));
        assert_eq!(
            parse_quantity_u64("0x10000000000000000"),
            Err(QuantityError::Overflow),
            "2^64 溢出 u64"
        );
        assert_eq!(parse_dec_u128("0012"), Ok(12), "十进制前导零");
        assert_eq!(parse_dec_u128(" 42 "), Ok(42), "容忍空白");
        assert_eq!(parse_dec_u128("-1"), Err(QuantityError::Invalid));
        assert_eq!(parse_dec_u128(""), Err(QuantityError::Invalid));
    }

    // ============================================================================
    // 二期（2026-09-02）：ERC-20 Transfer 日志核验 + AmountRule（≥应付额）
    // ============================================================================

    // 19. ERC-20 全绿：正确合约/topic0/收款方/金额 → Verified（token=合约小写、
    //     value=最小单位实付）；调用序列与 native 同构（chainId→ByHash→Receipt）。
    #[tokio::test]
    async fn erc20_verified_green_path() {
        let contract = format!("0x{}", "ee".repeat(20));
        let payee = format!("0x{}", "cd".repeat(20));
        let (url, seen) = spawn_rpc_mock(green_erc20(vec![transfer_log(
            &contract,
            ERC_FROM,
            &payee,
            10_000_000,
        )]));
        let out = verify_evm_tx(&[url], &erc20_proof(), Duration::from_secs(2)).await;
        assert_eq!(
            out,
            VerifyOutcome::Verified {
                block_number: 70_312,
                to: payee,
                value_wei: "10000000".into(),
                token: Some(contract),
            },
            "正确 Transfer 日志应过（金额=最小单位）"
        );
        assert_eq!(
            *seen.lock().unwrap(),
            [
                "eth_chainId",
                "eth_getTransactionByHash",
                "eth_getTransactionReceipt"
            ],
            "ERC-20 也按契约序列恰好调用 3 次"
        );
    }

    // 20. ERC-20 金额不符（Exact）：日志命中但 data 少打 → Mismatch("value")。
    #[tokio::test]
    async fn erc20_amount_mismatch_reports_onchain_amount() {
        let (url, _) = spawn_rpc_mock(green_erc20(vec![transfer_log(
            &format!("0x{}", "ee".repeat(20)),
            ERC_FROM,
            &format!("0x{}", "cd".repeat(20)),
            9_999_999,
        )]));
        let out = verify_evm_tx(&[url], &erc20_proof(), Duration::from_secs(2)).await;
        assert_eq!(
            out,
            VerifyOutcome::Mismatch {
                field: "value".into(),
                expect: "10000000".into(),
                actual: "9999999".into(),
            }
        );
    }

    // 21. ERC-20 收款方不符：topic2 打给别家 → 无匹配日志 Mismatch("erc20_log")。
    #[tokio::test]
    async fn erc20_wrong_payee_is_log_mismatch() {
        let (url, _) = spawn_rpc_mock(green_erc20(vec![transfer_log(
            &format!("0x{}", "ee".repeat(20)),
            ERC_FROM,
            &format!("0x{}", "99".repeat(20)),
            10_000_000,
        )]));
        let out = verify_evm_tx(&[url], &erc20_proof(), Duration::from_secs(2)).await;
        match out {
            VerifyOutcome::Mismatch { field, expect, actual } => {
                assert_eq!(field, "erc20_log");
                assert!(expect.contains("Transfer("), "expect 描述期望事件: {expect}");
                assert!(actual.contains("无匹配日志"), "actual 说明无匹配: {actual}");
            }
            other => panic!("应 Mismatch(erc20_log): {other:?}"),
        }
    }

    // 22. ERC-20 无日志（纯 native 转账的 receipt）→ Mismatch("erc20_log")。
    #[tokio::test]
    async fn erc20_receipt_without_logs_is_mismatch() {
        let (url, _) = spawn_rpc_mock(green_erc20(vec![]));
        let out = verify_evm_tx(&[url], &erc20_proof(), Duration::from_secs(2)).await;
        assert!(
            matches!(&out, VerifyOutcome::Mismatch { field, .. } if field == "erc20_log"),
            "无日志应 Mismatch(erc20_log): {out:?}"
        );
    }

    // 23. ERC-20 topic0 不匹配（别的事件签名，如 Approval）→ Mismatch("erc20_log")。
    #[tokio::test]
    async fn erc20_wrong_event_signature_is_log_mismatch() {
        let contract = format!("0x{}", "ee".repeat(20));
        let payee = format!("0x{}", "cd".repeat(20));
        let approval = serde_json::json!({
            "address": contract,
            // Approval(address,address,uint256) 签名哈希（与 Transfer 不同）
            "topics": [
                "0x8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925",
                addr_topic(ERC_FROM),
                addr_topic(&payee),
            ],
            "data": format!("0x{:064x}", 10_000_000u128),
        });
        let (url, _) = spawn_rpc_mock(green_erc20(vec![approval]));
        let out = verify_evm_tx(&[url], &erc20_proof(), Duration::from_secs(2)).await;
        assert!(
            matches!(&out, VerifyOutcome::Mismatch { field, .. } if field == "erc20_log"),
            "topic0 不匹配应 Mismatch(erc20_log): {out:?}"
        );
    }

    // 24. ERC-20 合约不符：日志发自别的合约（假币合约）→ Mismatch("erc20_log")。
    #[tokio::test]
    async fn erc20_wrong_contract_is_log_mismatch() {
        let (url, _) = spawn_rpc_mock(green_erc20(vec![transfer_log(
            &format!("0x{}", "ab".repeat(20)),
            ERC_FROM,
            &format!("0x{}", "cd".repeat(20)),
            10_000_000,
        )]));
        let out = verify_evm_tx(&[url], &erc20_proof(), Duration::from_secs(2)).await;
        assert!(
            matches!(&out, VerifyOutcome::Mismatch { field, .. } if field == "erc20_log"),
            "假合约日志应 Mismatch(erc20_log): {out:?}"
        );
    }

    // 25. ERC-20 多日志扫描：噪音日志（别的事件/别家合约/别个收款方）不影响，
    //     第二条日志命中即过。
    #[tokio::test]
    async fn erc20_scans_all_logs_until_match() {
        let contract = format!("0x{}", "ee".repeat(20));
        let payee = format!("0x{}", "cd".repeat(20));
        let logs = vec![
            // 别家合约发给 payee 的 Transfer（噪音）
            transfer_log(&format!("0x{}", "12".repeat(20)), ERC_FROM, &payee, 10_000_000),
            // 本合约 Approval（噪音）
            serde_json::json!({
                "address": contract,
                "topics": [
                    "0x8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925",
                    addr_topic(ERC_FROM), addr_topic(&payee),
                ],
                "data": format!("0x{:064x}", 10_000_000u128),
            }),
            // 本合约 Transfer 但打给别家（噪音）
            transfer_log(&contract, ERC_FROM, &format!("0x{}", "99".repeat(20)), 10_000_000),
            // 本合约 Transfer 给 payee、金额正确 → 命中
            transfer_log(&contract, ERC_FROM, &payee, 10_000_000),
        ];
        let (url, _) = spawn_rpc_mock(green_erc20(logs));
        let out = verify_evm_tx(&[url], &erc20_proof(), Duration::from_secs(2)).await;
        assert_eq!(
            out,
            VerifyOutcome::Verified {
                block_number: 70_312,
                to: payee,
                value_wei: "10000000".into(),
                token: Some(contract),
            },
            "应扫到第四条命中日志: {out:?}"
        );
    }

    // 26. ERC-20 凭证格式预检：合约地址非法 → fail-closed Mismatch("erc20_contract")，
    //     不打网络。
    #[tokio::test]
    async fn erc20_invalid_contract_rejected_without_network() {
        let (url, seen) = spawn_rpc_mock(green_erc20(vec![]));
        let mut p = erc20_proof();
        p.erc20 = Some(Erc20Spec {
            contract: "0xnot-a-contract".into(),
            decimals: 6,
        });
        let out = verify_evm_tx(&[url], &p, Duration::from_secs(1)).await;
        assert_eq!(
            out,
            VerifyOutcome::Mismatch {
                field: "erc20_contract".into(),
                expect: "0x + 40 位 hex".into(),
                actual: "0xnot-a-contract".into(),
            }
        );
        assert!(seen.lock().unwrap().is_empty(), "格式非法不应打网络");
    }

    // 27. ERC-20 + AtLeast 三分支：恰等/大于 → Verified（value_wei=实付）；
    //     小于 → Mismatch("value")。
    #[tokio::test]
    async fn erc20_at_least_three_branches() {
        let contract = format!("0x{}", "ee".repeat(20));
        let payee = format!("0x{}", "cd".repeat(20));
        let mut p = erc20_proof();
        p.amount_rule = AmountRule::AtLeast;
        // 恰等 → Verified
        let (url, _) = spawn_rpc_mock(green_erc20(vec![transfer_log(
            &contract, ERC_FROM, &payee, 10_000_000,
        )]));
        let out = verify_evm_tx(&[url], &p, Duration::from_secs(2)).await;
        assert!(matches!(&out, VerifyOutcome::Verified { value_wei, .. } if value_wei == "10000000"), "恰等: {out:?}");
        // 大于（多打 2.5 USDT）→ Verified 且实付=链上金额
        let (url, _) = spawn_rpc_mock(green_erc20(vec![transfer_log(
            &contract, ERC_FROM, &payee, 12_500_000,
        )]));
        let out = verify_evm_tx(&[url], &p, Duration::from_secs(2)).await;
        assert!(
            matches!(&out, VerifyOutcome::Verified { value_wei, token, .. }
                if value_wei == "12500000" && token.as_deref() == Some(contract.as_str())),
            "大于应过且携带实付: {out:?}"
        );
        // 小于 → Mismatch("value")
        let (url, _) = spawn_rpc_mock(green_erc20(vec![transfer_log(
            &contract, ERC_FROM, &payee, 9_000_000,
        )]));
        let out = verify_evm_tx(&[url], &p, Duration::from_secs(2)).await;
        assert_eq!(
            out,
            VerifyOutcome::Mismatch {
                field: "value".into(),
                expect: "10000000".into(),
                actual: "9000000".into(),
            },
            "不足应付额必须拦"
        );
    }

    // 28. native + AtLeast：恰等/大于 → Verified（value_wei=实付）；小于 → Mismatch。
    //     （Exact 的 native 行为已由用例 1/4 覆盖——零回归。）
    #[tokio::test]
    async fn native_at_least_three_branches() {
        let to = format!("0x{}", "cd".repeat(20));
        let mut p = proof();
        p.amount_rule = AmountRule::AtLeast;
        // 恰等（前导零形状）
        let (url, _) = spawn_rpc_mock(green("0x1", to.clone(), one_eth_padded_hex()));
        let out = verify_evm_tx(&[url], &p, Duration::from_secs(2)).await;
        assert!(matches!(&out, VerifyOutcome::Verified { value_wei, .. } if value_wei == "1000000000000000000"), "恰等: {out:?}");
        // 大于（2 wei > 1 wei 期望）
        let mut small = p.clone();
        small.expected_value = "1".into();
        let (url, _) = spawn_rpc_mock(green("0x1", to.clone(), "0x2".into()));
        let out = verify_evm_tx(&[url], &small, Duration::from_secs(2)).await;
        assert!(
            matches!(&out, VerifyOutcome::Verified { value_wei, .. } if value_wei == "2"),
            "大于应过且实付=2: {out:?}"
        );
        // 小于（链上 2 < 期望 1 ETH）→ Mismatch("value")
        let (url, _) = spawn_rpc_mock(green("0x1", to, "0x2".into()));
        let out = verify_evm_tx(&[url], &p, Duration::from_secs(2)).await;
        assert_eq!(
            out,
            VerifyOutcome::Mismatch {
                field: "value".into(),
                expect: "1000000000000000000".into(),
                actual: "2".into(),
            }
        );
    }

    // 29. ERC-20 data 形状非法（非 0x+64hex）：日志命中但金额无法解析 →
    //     计入 actual、不参与满足判定 → Mismatch("value")（fail-closed）。
    #[tokio::test]
    async fn erc20_malformed_data_is_value_mismatch() {
        let contract = format!("0x{}", "ee".repeat(20));
        let payee = format!("0x{}", "cd".repeat(20));
        let bad_data = serde_json::json!({
            "address": contract,
            "topics": [TRANSFER_TOPIC0, addr_topic(ERC_FROM), addr_topic(&payee)],
            "data": "0x1234", // 非 32 字节定宽
        });
        let (url, _) = spawn_rpc_mock(green_erc20(vec![bad_data]));
        let out = verify_evm_tx(&[url], &erc20_proof(), Duration::from_secs(2)).await;
        assert_eq!(
            out,
            VerifyOutcome::Mismatch {
                field: "value".into(),
                expect: "10000000".into(),
                actual: "(unparsed)".into(),
            },
            "金额不可解析不得放行"
        );
    }

    // 30. 工具直测：topic_addr / data_amount / AmountRule 默认值。
    #[test]
    fn erc20_parsing_tools_edges() {
        let addr = "0xcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";
        assert_eq!(topic_addr(&addr_topic(addr)).as_deref(), Some(addr));
        // 大写 EIP-55 输入也归一
        assert_eq!(
            topic_addr(&addr_topic(&addr.to_ascii_uppercase())).as_deref(),
            Some(addr),
            "topic 大小写不敏感"
        );
        assert!(topic_addr("0xcdcd").is_none(), "非 64 hex");
        assert!(topic_addr("cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd").is_none(), "缺 0x 前缀");
        assert_eq!(data_amount(&format!("0x{:064x}", 10_000_000u128)), Some(10_000_000));
        assert_eq!(data_amount("0x"), None, "空 data 非 32 字节定宽");
        assert_eq!(data_amount("0x1234"), None, "短 data");
        assert_eq!(
            data_amount(&format!("0x{:064x}", u128::MAX)),
            Some(u128::MAX)
        );
        // 2^128 溢出（真实代币不可能的攻击形状）→ None（fail-closed）
        assert_eq!(data_amount(&format!("0x01{}", "0".repeat(62))), None);
        assert_eq!(AmountRule::default(), AmountRule::Exact, "默认 Exact 向后兼容");
    }
}
