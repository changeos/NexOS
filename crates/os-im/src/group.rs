//! IM 群组管理——接口契约（规划文档 §3.7 群组扩展）。
//!
//! OS 节点可创建聊天群，其他 OS 节点通过 IP 接入群（以邀请码鉴权）。
//!
//! 本文件仅定义契约（trait + 数据结构），实现见 [`crate::group_impl`]。
//!
//! 契约规范：[`GroupManager`] 需 `Box<dyn GroupManager>` 运行期多态，
//! 故采用 `#[async_trait]`（呼应横切决策：凡 `Box<dyn>` 用的 async trait 一律 `#[async_trait]`）。

use async_trait::async_trait;
use os_core::DateTime;
use serde::{Deserialize, Serialize};

use crate::error::ImError;

// ----------------------------------------------------------------------------
// 群组 ID
// ----------------------------------------------------------------------------

/// 群组 ID（newtype String）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GroupId(pub String);

impl GroupId {
    /// 从字符串构造。
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    /// 取字符串切片。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Display for GroupId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl From<String> for GroupId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

// ----------------------------------------------------------------------------
// 成员
// ----------------------------------------------------------------------------

/// 群组成员类型（权限分级）。
///
/// - [`MemberType::Owner`]：群主，最高权限（可踢 Admin/Member/Guest，可解散群）。
/// - [`MemberType::Admin`]：管理员（可踢 Member/Guest，可邀请）。
/// - [`MemberType::Member`]：普通成员（可发言/退群）。
/// - [`MemberType::Guest`]：访客（受限，仅可发言）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberType {
    /// 群主
    Owner,
    /// 管理员
    Admin,
    /// 普通成员
    Member,
    /// 访客
    Guest,
}

impl MemberType {
    /// 是否可执行管理操作（踢人 / 邀请）。Owner / Admin 返回 true。
    pub fn can_manage(&self) -> bool {
        matches!(self, MemberType::Owner | MemberType::Admin)
    }
}

/// 群组成员。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupMember {
    /// 成员所在 OS 节点 ID
    pub node_id: String,
    /// 显示名
    pub display_name: String,
    /// 成员类型
    pub member_type: MemberType,
    /// 加入时间（UTC）
    pub joined_at: DateTime,
    /// 是否在线
    pub online: bool,
}

// ----------------------------------------------------------------------------
// 群组
// ----------------------------------------------------------------------------

/// 群组。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    /// 群组 ID
    pub id: GroupId,
    /// 群名
    pub name: String,
    /// 群主节点 ID
    pub owner: String,
    /// 成员列表
    pub members: Vec<GroupMember>,
    /// 创建时间（UTC）
    pub created_at: DateTime,
    /// 群描述
    pub description: Option<String>,
    /// 当前有效邀请码（其他节点凭此码加入；None 表示需重新生成）
    pub invite_code: Option<String>,
    /// 成员上限
    pub max_members: u32,
}

impl Group {
    /// 取当前成员数。
    pub fn member_count(&self) -> usize {
        self.members.len()
    }

    /// 是否已满员。
    pub fn is_full(&self) -> bool {
        self.member_count() >= self.max_members as usize
    }

    /// 按 node_id 查找成员。
    pub fn find_member(&self, node_id: &str) -> Option<&GroupMember> {
        self.members.iter().find(|m| m.node_id == node_id)
    }

    /// 按 node_id 查找成员（可变）。
    pub fn find_member_mut(&mut self, node_id: &str) -> Option<&mut GroupMember> {
        self.members.iter_mut().find(|m| m.node_id == node_id)
    }
}

// ----------------------------------------------------------------------------
// GroupManager trait（async，需 Box<dyn> 多态 → #[async_trait]）
// ----------------------------------------------------------------------------

/// 群组管理——创建 / 加入 / 退出 / 邀请 / 踢人 / 查询。
///
/// 实现者：[`crate::group_impl::InMemoryGroupManager`]（内存，单节点轻量场景）；
/// 生产实现可基于 os-meta 持久化并跨节点同步。
#[async_trait]
pub trait GroupManager: Send + Sync {
    /// 创建群组，群主自动作为 Owner 成员加入。返回新建群组。
    async fn create_group(&self, name: &str, owner: &str) -> Result<Group, ImError>;

    /// 节点加入群组。若群要求邀请码（有 `invite_code` 设置），则需传入匹配且未过期的码。
    async fn join_group(
        &self,
        group_id: &GroupId,
        node_id: &str,
        invite_code: Option<&str>,
    ) -> Result<(), ImError>;

    /// 节点退出群组。群主退群需先把群主转让（本实现：群主不可直接退群，返回错误）。
    async fn leave_group(&self, group_id: &GroupId, node_id: &str) -> Result<(), ImError>;

    /// 邀请成员（生成邀请码并返回）。需 Owner/Admin 权限。
    async fn invite_member(&self, group_id: &GroupId, node_id: &str) -> Result<String, ImError>;

    /// 踢出成员。仅 Owner/Admin 可执行；不可踢群主；不可踢自己（Owner 不可踢自己）。
    async fn kick_member(&self, group_id: &GroupId, node_id: &str, by: &str)
        -> Result<(), ImError>;

    /// 列出全部群组（克隆快照）。
    async fn list_groups(&self) -> Vec<Group>;

    /// 取单个群组（克隆快照）。
    async fn get_group(&self, group_id: &GroupId) -> Option<Group>;

    /// 列出群组成员。
    async fn list_members(&self, group_id: &GroupId) -> Vec<GroupMember>;

    /// 生成新的邀请码（覆盖旧码）。需 Owner/Admin 权限。
    async fn generate_invite_code(&self, group_id: &GroupId) -> Result<String, ImError>;
}
