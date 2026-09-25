//! 访客身份引擎——创建/认证/续期/撤销/列举
//!
//! 决策依据：规划文档 §3.18 —— 访客身份生命周期管理。
//! - 创建：按身份类型生成（RandomId 生成 GUEST-XXXXXX；ChainCredential 由 chain 模块触发）
//! - 认证：校验访客有效性并刷新 JWT/NFT
//! - 续期：延长过期时间（ExtendedId 专属）
//! - 撤销：吊销身份（nft 规则同步移除）

use os_core::{Deserialize, GuestId, PageRequest, PageResponse, Serialize};

use crate::model::{GuestIdentity, GuestIdentityType, GuestStatus};

// ----------------------------------------------------------------------------
// GuestFilter
// ----------------------------------------------------------------------------

/// 访客列举过滤器
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GuestFilter {
    /// 按状态过滤（None = 不过滤）
    pub status: Option<GuestStatus>,
    /// 按身份类型过滤（None = 不过滤）
    pub id_type: Option<GuestIdentityType>,
}

// ----------------------------------------------------------------------------
// IdentityEngine trait（async）
// ----------------------------------------------------------------------------

/// 访客身份引擎——管理访客身份全生命周期。
///
/// 实现者：`DefaultIdentityEngine`（默认，状态存于 os-meta 分布式 KV）；
/// 签发 JWT 委派 os-security JwtIssuer；nft 规则同步经 NftRuleOrchestrator。
#[allow(async_fn_in_trait)]
pub trait IdentityEngine: Send + Sync {
    /// 创建访客（按身份类型生成；RandomId/ExtendedId 生成 GUEST-XXXXXX）。
    async fn create_guest(
        &self,
        id_type: GuestIdentityType,
    ) -> Result<GuestIdentity, crate::GuestError>;

    /// 认证访客——校验有效性并刷新 JWT/NFT；不存在/已撤销返回错误。
    async fn authenticate_guest(&self, id: &GuestId) -> Result<GuestIdentity, crate::GuestError>;

    /// 续期访客（延长过期时间；ExtendedId 专属，其他类型按策略可能拒绝）。
    async fn extend_guest(
        &self,
        id: &GuestId,
        duration: chrono::Duration,
    ) -> Result<GuestIdentity, crate::GuestError>;

    /// 撤销访客（吊销身份 + 同步移除 nft 规则）。
    async fn revoke_guest(&self, id: &GuestId) -> Result<(), crate::GuestError>;

    /// 分页列举访客（分页用 os-core::PageRequest/PageResponse）。
    async fn list_guests(
        &self,
        filter: GuestFilter,
        page: PageRequest,
    ) -> Result<PageResponse<GuestIdentity>, crate::GuestError>;
}
