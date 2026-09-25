//! 链上凭证业务编排——chain-orchestrator（委派 os-wallet，本身不下沉）
//!
//! 决策依据：规划文档 §3.18 / §3.18.1 —— 访客可通过链上凭证验证身份（三因子：
//! 签名挑战 / 余额阈值 / 链上凭证）。本 crate 是**业务编排层**，签名/连接/凭证查询
//! **全部委派给 os-wallet**，本身不做任何密码学或链交互。
//!
//! ## 委派关系（关键设计点）
//! 本 trait 的实现者需持有以下 os-wallet 引用：
//! - `os_wallet::WalletConnector`：建立钱包 session、请求用户签名
//! - `os_wallet::ChainAdapter`：验签 / 查余额 / 查凭证（链无关抽象）
//! - `os_wallet::RpcRegistry`：探测链可用性（条件激活）
//!
//! ## 编排流程（`start_verification`）
//! 1. 经 `RpcRegistry.is_available(chain)` 判断链可用——不可用则按 `privacy_mode` 降级：
//!    - `Mandatory`：直接判失败（链不可用时拒绝放行）
//!    - `Optional`/`None`：降级为常规访客流程
//! 2. 经 `WalletConnector.connect(chain, kind)` 建立钱包 session（触发用户钱包确认）
//! 3. 经 `WalletConnector.request_signature(...)` 请求用户签名（BIP-322/EIP-191 等）
//! 4. 经 `ChainAdapter.verify_signature(...)` 验签（证明地址所有权）
//! 5. 按配置的 `required_factors` 依次校验：余额阈值（`query_balance`）/ 凭证（`query_credential`）
//! 6. 全部通过 → 经 os-security `JwtIssuer` 签发 `TokenType::ChainCredential` JWT，
//!    返回 `Completed{ address_hash }`（地址哈希化，避免明文落库）
//!
//! 每一步均把状态写回 `verification_status`，上层轮询获取进度。

use os_core::{Deserialize, GuestId, Serialize, TaskId, WalletSessionId};
// 注：WalletSessionId 定义于 os-core::ids；编排流程中建立的钱包 session 由
//   os-wallet WalletConnector.connect() 返回，其 session id 复用本类型。
use os_wallet::{ChainKind, VerificationFactor};

// ----------------------------------------------------------------------------
// PrivacyMode / ChainVerificationConfig
// ----------------------------------------------------------------------------

/// 隐私模式三档（§3.18.1）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyMode {
    /// 强制——链不可用或验证失败一律拒绝放行
    Mandatory,
    /// 可选——链不可用时降级为常规访客流程
    Optional,
    /// 无——不要求链上验证（纯常规访客）
    None,
}

/// 链上验证配置（编排入参）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainVerificationConfig {
    /// 要求的验证因子集合（签名挑战/余额阈值/链上凭证，见 os-wallet）
    pub required_factors: Vec<VerificationFactor>,
    /// 目标链大类
    pub chain: ChainKind,
    /// 验证通过后授予的角色名（None = 不授予特殊角色）
    pub role_on_success: Option<String>,
    /// 隐私模式（决定链不可用时的降级行为）
    pub privacy_mode: PrivacyMode,
}

// ----------------------------------------------------------------------------
// ChainVerificationStatus
// ----------------------------------------------------------------------------

/// 链上验证任务状态（编排流程各阶段）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum ChainVerificationStatus {
    /// 已入队待执行
    Pending,
    /// 等待用户在钱包侧完成签名（含已建立的 session id）
    WaitingSignature {
        /// 关联的钱包 session id（用于前端关联钱包弹窗）
        session_id: WalletSessionId,
    },
    /// 验签 / 查余额 / 查凭证 进行中
    Verifying,
    /// 全部因子通过，已签发链上凭证 JWT（地址哈希化）
    Completed {
        /// 通过验证的地址哈希（避免明文地址落库）
        address_hash: String,
    },
    /// 验证失败（签名无效/凭证不符/余额不足/链不可用且 Mandatory）
    Failed {
        /// 失败原因
        reason: String,
    },
}

// ----------------------------------------------------------------------------
// ChainOrchestrator trait（async，业务编排层；调 os-wallet）
// ----------------------------------------------------------------------------

/// 链上凭证业务编排器——编排 os-wallet 完成访客链上验证，本身不下沉密码学/链交互。
///
/// 实现者：`DefaultChainOrchestrator`（默认）。
///
/// ## 持有依赖（实现者构造时注入）
/// - `connector: Arc<dyn os_wallet::WalletConnector>`
/// - `adapter: Arc<dyn os_wallet::ChainAdapter>`（经 RpcRegistry 条件激活后注入对应链）
/// - `registry: Arc<dyn os_wallet::RpcRegistry>`
/// - `jwt: Arc<dyn os_security::JwtIssuer>`（签发 ChainCredential JWT）
///
/// ## 委派边界
/// 本 trait **仅做编排**：建 session → 请求签名 → 验签 → 查因子 → 签 JWT；
/// 所有签名/连接/凭证查询/余额查询**调 os-wallet**；JWT 签发**调 os-security**。
#[allow(async_fn_in_trait)]
pub trait ChainOrchestrator: Send + Sync {
    /// 启动链上验证流程（异步任务）。
    ///
    /// 编排：判链可用（RpcRegistry）→ 建 session（WalletConnector）→
    /// 请求签名 → 验签 + 查因子（ChainAdapter）→ 签 JWT（JwtIssuer）。
    /// 返回关联 TaskId，上层轮询 `verification_status`。
    async fn start_verification(
        &self,
        guest: &GuestId,
        config: &ChainVerificationConfig,
    ) -> Result<TaskId, crate::GuestError>;

    /// 查询验证任务状态（编排流程各阶段进度）。
    async fn verification_status(
        &self,
        task: &TaskId,
    ) -> Result<ChainVerificationStatus, crate::GuestError>;
}
