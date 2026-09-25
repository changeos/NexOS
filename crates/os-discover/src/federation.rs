//! HA 资格检测 + 联邦分支决策（规划文档 §3.14）
//!
//! 决策依据：§3.14 —— 发现到 peer 后，先做硬指标探测（节点数/带宽/ZFS/KVM/版本），
//! 判定本集群是否"具备 HA 资格"；再结合用户选择（自动/手动 HA/手动 peer/拒绝）
//! 决定联邦动作：加入既有 HA 集群 / 注册为 peer / 保持单机。

use os_core::{DateTime, Deserialize, Serialize, Utc};

use crate::discovery::PeerNode;

// ----------------------------------------------------------------------------
// HaRequirement / HaEligibility
// ----------------------------------------------------------------------------

/// HA 资格硬性要求（探测门槛）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HaRequirement {
    /// 最少节点数（法定成员数下限）
    pub min_nodes: u32,
    /// 最小互联带宽（Gbps）
    pub min_bandwidth_gbps: f32,
    /// 是否要求 ZFS（HA 存储复制依赖）
    pub require_zfs: bool,
    /// 是否要求 KVM（VM 迁移依赖）
    pub require_kvm: bool,
    /// 版本兼容范围（SemVer 约束字符串集合，如 [">=1.0.0,<2.0.0"]）
    pub version_compat: Vec<String>,
}

/// HA 资格检测结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HaEligibility {
    /// 是否具备 HA 资格（满足全部硬指标）
    pub eligible: bool,
    /// 不满足的原因列表（eligible=true 时为空）
    pub reasons: Vec<String>,
    /// 检测时间
    pub checked_at: DateTime,
}

impl HaEligibility {
    /// 构造一个当前时间戳的检测结果
    pub fn new(eligible: bool, reasons: Vec<String>) -> Self {
        Self {
            eligible,
            reasons,
            checked_at: Utc::now(),
        }
    }
}

// ----------------------------------------------------------------------------
// FederationChoice / FederationAction
// ----------------------------------------------------------------------------

/// 用户联邦选择（决定硬指标达标后的动作）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FederationChoice {
    /// 自动（系统按资格结果自行决定）
    Auto,
    /// 手动：组建/加入 HA 集群
    ManualHa,
    /// 手动：仅作为 peer 同步
    ManualPeer,
    /// 拒绝联邦（保持单机）
    Decline,
}

/// 联邦动作（决策结果）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FederationAction {
    /// 加入既有 HA 集群（leader_endpoint 为接入点）
    JoinHaCluster {
        /// 目标 leader 接入端点
        leader_endpoint: String,
    },
    /// 注册为 peer（仅同步，不进法定）
    RegisterAsPeer,
    /// 保持独立单机
    StayStandalone,
}

// ----------------------------------------------------------------------------
// FederationPolicy trait（async）
// ----------------------------------------------------------------------------

/// 联邦策略引擎——HA 资格检测 + 分支决策。
///
/// 实现者：`DefaultFederationPolicy`（默认，纯规则判定）；其他实现可替换。
/// 流程：`check_eligibility`（硬指标探测）→ `decide`（结合用户选择产动作）。
#[allow(async_fn_in_trait)]
pub trait FederationPolicy: Send + Sync {
    /// 检测 HA 资格——对照 `requirements` 探测 `peers` 硬指标。
    async fn check_eligibility(
        &self,
        peers: &[PeerNode],
        requirements: &HaRequirement,
    ) -> Result<HaEligibility, crate::DiscoverError>;

    /// 决策——结合资格结果与用户选择，产出联邦动作。
    async fn decide(
        &self,
        eligibility: &HaEligibility,
        user_choice: FederationChoice,
    ) -> Result<FederationAction, crate::DiscoverError>;
}
