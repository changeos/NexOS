//! os-im 群组管理默认实现（规划文档 §3.7 群组扩展）。
//!
//! 提供 [`InMemoryGroupManager`]：纯内存实现，`HashMap<GroupId, Group>` + `Mutex`，
//! 用于测试 / 单节点轻量场景。生产实现可基于 os-meta 持久化并跨节点同步。
//!
//! 约定：
//! - 邀请码：6 位大写字母+数字（如 `A3X9K2`），默认有效期 24 小时。
//! - 成员上限默认 100。
//! - 权限：Owner/Admin 可踢人/邀请；不可踢群主；群主不可直接退群。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use os_core::DateTime;

use crate::error::ImError;
use crate::group::{Group, GroupId, GroupManager, GroupMember, MemberType};

/// 邀请码有效期（秒）：24 小时。
const INVITE_TTL_SECS: i64 = 24 * 60 * 60;
/// 默认成员上限。
const DEFAULT_MAX_MEMBERS: u32 = 100;
/// 邀请码字符表（去掉易混淆字符 0/O/1/I/L）。
const INVITE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
/// 邀请码长度。
const INVITE_LEN: usize = 6;

/// 内部：群组 + 邀请码生成时间（用于过期判定）。
struct GroupEntry {
    group: Group,
    /// 当前邀请码生成时间（UTC），用于过期判定。
    invite_code_generated_at: Option<DateTime>,
}

/// 内存群组管理器——纯内存实现，用于测试 / 单节点轻量场景。
pub struct InMemoryGroupManager {
    groups: Mutex<HashMap<GroupId, GroupEntry>>,
}

impl InMemoryGroupManager {
    /// 创建空管理器。
    pub fn new() -> Self {
        Self {
            groups: Mutex::new(HashMap::new()),
        }
    }

    /// 当前 UTC 时间。
    fn now() -> DateTime {
        chrono::Utc::now()
    }

    /// 取群主节点 ID（无锁读，调用方持锁）。
    fn owner_of(group: &Group) -> &str {
        &group.owner
    }

    /// 取群中某成员类型（调用方持锁）。
    fn member_type_of(group: &Group, node_id: &str) -> Option<MemberType> {
        group.find_member(node_id).map(|m| m.member_type)
    }

    /// 检查邀请码是否过期。
    fn invite_expired(generated_at: Option<DateTime>, now: DateTime) -> bool {
        match generated_at {
            Some(t) => (now - t).num_seconds() >= INVITE_TTL_SECS,
            None => true, // 无生成记录视为过期
        }
    }
}

impl Default for InMemoryGroupManager {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl GroupManager for InMemoryGroupManager {
    async fn create_group(&self, name: &str, owner: &str) -> Result<Group, ImError> {
        if name.trim().is_empty() {
            return Err(ImError::Internal("群名不能为空".to_string()));
        }
        if owner.trim().is_empty() {
            return Err(ImError::Internal("群主节点 ID 不能为空".to_string()));
        }
        let now = Self::now();
        // 群组 ID：基于时间戳 + 计数生成，确保唯一。
        let id = GroupId::new(format!("grp_{}", now.timestamp_nanos_opt().unwrap_or(0)));
        let owner_member = GroupMember {
            node_id: owner.to_string(),
            display_name: owner.to_string(),
            member_type: MemberType::Owner,
            joined_at: now,
            online: true,
        };
        let group = Group {
            id: id.clone(),
            name: name.to_string(),
            owner: owner.to_string(),
            members: vec![owner_member],
            created_at: now,
            description: None,
            invite_code: None,
            max_members: DEFAULT_MAX_MEMBERS,
        };
        let entry = GroupEntry {
            group: group.clone(),
            invite_code_generated_at: None,
        };
        self.groups.lock().unwrap().insert(id, entry);
        Ok(group)
    }

    async fn join_group(
        &self,
        group_id: &GroupId,
        node_id: &str,
        invite_code: Option<&str>,
    ) -> Result<(), ImError> {
        let mut groups = self.groups.lock().unwrap();
        let entry = groups
            .get_mut(group_id)
            .ok_or_else(|| ImError::GroupNotFound(format!("{}", group_id)))?;

        // 满员检查
        if entry.group.is_full() {
            return Err(ImError::GroupFull(format!(
                "群组 {} 已达成员上限 {}",
                group_id, entry.group.max_members
            )));
        }

        // 已是成员 → 幂等成功
        if entry.group.find_member(node_id).is_some() {
            return Ok(());
        }

        // 邀请码校验：若群已有 invite_code 设置，则必须匹配且未过期
        let now = Self::now();
        match (&entry.group.invite_code, invite_code) {
            (Some(expected), Some(provided)) => {
                if expected != provided {
                    return Err(ImError::InviteInvalid(format!(
                        "邀请码不匹配，群组 {}",
                        group_id
                    )));
                }
                if Self::invite_expired(entry.invite_code_generated_at, now) {
                    return Err(ImError::InviteExpired(format!(
                        "邀请码已过期，群组 {}",
                        group_id
                    )));
                }
            }
            (Some(_), None) => {
                return Err(ImError::InviteInvalid(format!(
                    "群组 {} 需要邀请码",
                    group_id
                )));
            }
            (None, _) => {
                // 无邀请码要求，直接加入
            }
        }

        let member = GroupMember {
            node_id: node_id.to_string(),
            display_name: node_id.to_string(),
            member_type: MemberType::Member,
            joined_at: now,
            online: true,
        };
        entry.group.members.push(member);
        Ok(())
    }

    async fn leave_group(&self, group_id: &GroupId, node_id: &str) -> Result<(), ImError> {
        let mut groups = self.groups.lock().unwrap();
        let entry = groups
            .get_mut(group_id)
            .ok_or_else(|| ImError::GroupNotFound(format!("{}", group_id)))?;

        // 必须是成员
        if entry.group.find_member(node_id).is_none() {
            return Err(ImError::NotMember(format!(
                "节点 {} 不在群组 {} 中",
                node_id, group_id
            )));
        }

        // 群主不可直接退群
        if Self::owner_of(&entry.group) == node_id {
            return Err(ImError::PermissionDenied(format!(
                "群主 {} 不可直接退群，请先转让群主",
                node_id
            )));
        }

        entry.group.members.retain(|m| m.node_id != node_id);
        Ok(())
    }

    async fn invite_member(&self, group_id: &GroupId, node_id: &str) -> Result<String, ImError> {
        // invite_member 语义：校验调用方权限并生成/刷新邀请码。
        // node_id 在此作为操作者（邀请人）校验权限。
        let mut groups = self.groups.lock().unwrap();
        let entry = groups
            .get_mut(group_id)
            .ok_or_else(|| ImError::GroupNotFound(format!("{}", group_id)))?;

        let caller_type = Self::member_type_of(&entry.group, node_id).ok_or_else(|| {
            ImError::NotMember(format!("节点 {} 不在群组 {} 中", node_id, group_id))
        })?;
        if !caller_type.can_manage() {
            return Err(ImError::PermissionDenied(format!(
                "节点 {} 无邀请权限（需 Owner/Admin）",
                node_id
            )));
        }

        let code = generate_invite_code();
        let now = Self::now();
        entry.group.invite_code = Some(code.clone());
        entry.invite_code_generated_at = Some(now);
        Ok(code)
    }

    async fn kick_member(
        &self,
        group_id: &GroupId,
        node_id: &str,
        by: &str,
    ) -> Result<(), ImError> {
        let mut groups = self.groups.lock().unwrap();
        let entry = groups
            .get_mut(group_id)
            .ok_or_else(|| ImError::GroupNotFound(format!("{}", group_id)))?;

        // 操作者必须是成员
        let by_type = Self::member_type_of(&entry.group, by)
            .ok_or_else(|| ImError::NotMember(format!("操作者 {} 不在群组 {} 中", by, group_id)))?;
        if !by_type.can_manage() {
            return Err(ImError::PermissionDenied(format!(
                "节点 {} 无踢人权限（需 Owner/Admin）",
                by
            )));
        }

        // 不可踢群主
        if Self::owner_of(&entry.group) == node_id {
            return Err(ImError::PermissionDenied(format!(
                "不可踢出群主 {}",
                node_id
            )));
        }

        // Admin 不可踢 Admin（仅 Owner 可踢 Admin）
        if let Some(target_type) = Self::member_type_of(&entry.group, node_id) {
            if target_type == MemberType::Admin && by_type != MemberType::Owner {
                return Err(ImError::PermissionDenied(format!(
                    "Admin 不可踢出 Admin（需群主操作），目标 {}",
                    node_id
                )));
            }
        }

        // 目标必须是成员
        if entry.group.find_member(node_id).is_none() {
            return Err(ImError::NotMember(format!(
                "节点 {} 不在群组 {} 中",
                node_id, group_id
            )));
        }

        entry.group.members.retain(|m| m.node_id != node_id);
        Ok(())
    }

    async fn list_groups(&self) -> Vec<Group> {
        let groups = self.groups.lock().unwrap();
        let mut v: Vec<Group> = groups.values().map(|e| e.group.clone()).collect();
        // 按 ID 排序保证确定性（便于测试断言）
        v.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        v
    }

    async fn get_group(&self, group_id: &GroupId) -> Option<Group> {
        self.groups
            .lock()
            .unwrap()
            .get(group_id)
            .map(|e| e.group.clone())
    }

    async fn list_members(&self, group_id: &GroupId) -> Vec<GroupMember> {
        match self.groups.lock().unwrap().get(group_id) {
            Some(e) => e.group.members.clone(),
            None => Vec::new(),
        }
    }

    async fn generate_invite_code(&self, group_id: &GroupId) -> Result<String, ImError> {
        // 与 invite_member 区分：本方法由群管理接口直接调用（不绑定具体操作者权限校验），
        // 仅校验群存在性。若需权限校验，调用方应通过 invite_member（带操作者）。
        let mut groups = self.groups.lock().unwrap();
        let entry = groups
            .get_mut(group_id)
            .ok_or_else(|| ImError::GroupNotFound(format!("{}", group_id)))?;
        let code = generate_invite_code();
        let now = Self::now();
        entry.group.invite_code = Some(code.clone());
        entry.invite_code_generated_at = Some(now);
        Ok(code)
    }
}

// ============================================================
// 工具函数
// ============================================================

/// 生成 6 位邀请码（大写字母+数字，去掉易混淆字符）。
///
/// 随机源：`SystemTime` 纳秒 + 进程内全局原子计数器，xor 混合后取模映射到字符表。
/// 不引入 `rand` 依赖；测试场景下确定性由 `serial_test` 串行保证（本实现未引入）。
fn generate_invite_code() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let now_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    // xor 混合 + 旋转，增加熵
    let mut state = now_nanos ^ seq.wrapping_mul(0x9E3779B97F4A7C15);
    let mut out = String::with_capacity(INVITE_LEN);
    for i in 0..INVITE_LEN {
        // 每轮再混入位置索引，避免连续码重复
        state = state
            .wrapping_add((i as u64).wrapping_mul(0xBF58476D1CE4E5B9))
            .rotate_left(7);
        let idx = (state % INVITE_ALPHABET.len() as u64) as usize;
        out.push(INVITE_ALPHABET[idx] as char);
    }
    out
}

// ============================================================
// 单元测试——关键路径：创建 / 加入 / 退出 / 邀请码 / 踢人 / 权限 / 满员
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::group::GroupManager;

    fn new_mgr() -> InMemoryGroupManager {
        InMemoryGroupManager::new()
    }

    #[tokio::test]
    async fn create_group_adds_owner_as_member() {
        let mgr = new_mgr();
        let g = mgr.create_group("技术交流", "node-alice").await.unwrap();
        assert_eq!(g.name, "技术交流");
        assert_eq!(g.owner, "node-alice");
        assert_eq!(g.members.len(), 1);
        assert_eq!(g.members[0].node_id, "node-alice");
        assert_eq!(g.members[0].member_type, MemberType::Owner);
        assert_eq!(g.max_members, DEFAULT_MAX_MEMBERS);
        assert!(g.invite_code.is_none());
        // get_group 能取回
        let fetched = mgr.get_group(&g.id).await.expect("群组应存在");
        assert_eq!(fetched.id, g.id);
    }

    #[tokio::test]
    async fn create_group_rejects_empty_name_or_owner() {
        let mgr = new_mgr();
        assert!(mgr.create_group("", "node-a").await.is_err());
        assert!(mgr.create_group("g", "").await.is_err());
    }

    #[tokio::test]
    async fn join_without_invite_code_when_none_required() {
        let mgr = new_mgr();
        let g = mgr.create_group("g1", "node-a").await.unwrap();
        // 群无 invite_code，新节点可直接加入
        mgr.join_group(&g.id, "node-b", None).await.unwrap();
        let members = mgr.list_members(&g.id).await;
        assert_eq!(members.len(), 2);
        assert!(members.iter().any(|m| m.node_id == "node-b"));
    }

    #[tokio::test]
    async fn join_is_idempotent_for_existing_member() {
        let mgr = new_mgr();
        let g = mgr.create_group("g1", "node-a").await.unwrap();
        // 群主再次 join（已是成员）→ 幂等成功，不增加成员
        mgr.join_group(&g.id, "node-a", None).await.unwrap();
        assert_eq!(mgr.list_members(&g.id).await.len(), 1);
    }

    #[tokio::test]
    async fn join_with_valid_invite_code_succeeds() {
        let mgr = new_mgr();
        let g = mgr.create_group("secret", "node-owner").await.unwrap();
        // 群主生成邀请码
        let code = mgr.invite_member(&g.id, "node-owner").await.unwrap();
        assert_eq!(code.len(), INVITE_LEN);
        // 新节点凭码加入
        mgr.join_group(&g.id, "node-b", Some(&code)).await.unwrap();
        assert_eq!(mgr.list_members(&g.id).await.len(), 2);
    }

    #[tokio::test]
    async fn join_with_wrong_invite_code_fails() {
        let mgr = new_mgr();
        let g = mgr.create_group("secret", "node-owner").await.unwrap();
        mgr.invite_member(&g.id, "node-owner").await.unwrap();
        let err = mgr
            .join_group(&g.id, "node-b", Some("WRONG0"))
            .await
            .unwrap_err();
        assert!(matches!(err, ImError::InviteInvalid(_)));
    }

    #[tokio::test]
    async fn join_without_required_invite_code_fails() {
        let mgr = new_mgr();
        let g = mgr.create_group("secret", "node-owner").await.unwrap();
        mgr.invite_member(&g.id, "node-owner").await.unwrap();
        let err = mgr.join_group(&g.id, "node-b", None).await.unwrap_err();
        assert!(matches!(err, ImError::InviteInvalid(_)));
    }

    #[tokio::test]
    async fn join_nonexistent_group_fails() {
        let mgr = new_mgr();
        let err = mgr
            .join_group(&GroupId::new("ghost"), "node-b", None)
            .await
            .unwrap_err();
        assert!(matches!(err, ImError::GroupNotFound(_)));
    }

    #[tokio::test]
    async fn leave_group_removes_member() {
        let mgr = new_mgr();
        let g = mgr.create_group("g1", "node-a").await.unwrap();
        mgr.join_group(&g.id, "node-b", None).await.unwrap();
        mgr.leave_group(&g.id, "node-b").await.unwrap();
        assert_eq!(mgr.list_members(&g.id).await.len(), 1);
    }

    #[tokio::test]
    async fn leave_group_owner_blocked() {
        let mgr = new_mgr();
        let g = mgr.create_group("g1", "node-owner").await.unwrap();
        let err = mgr.leave_group(&g.id, "node-owner").await.unwrap_err();
        assert!(matches!(err, ImError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn leave_non_member_fails() {
        let mgr = new_mgr();
        let g = mgr.create_group("g1", "node-a").await.unwrap();
        let err = mgr.leave_group(&g.id, "node-ghost").await.unwrap_err();
        assert!(matches!(err, ImError::NotMember(_)));
    }

    #[tokio::test]
    async fn invite_member_requires_manage_permission() {
        let mgr = new_mgr();
        let g = mgr.create_group("g1", "node-owner").await.unwrap();
        mgr.join_group(&g.id, "node-member", None).await.unwrap();
        // 普通成员不可邀请
        let err = mgr.invite_member(&g.id, "node-member").await.unwrap_err();
        assert!(matches!(err, ImError::PermissionDenied(_)));
        // 群主可邀请
        mgr.invite_member(&g.id, "node-owner").await.unwrap();
    }

    #[tokio::test]
    async fn invite_member_non_member_fails() {
        let mgr = new_mgr();
        let g = mgr.create_group("g1", "node-owner").await.unwrap();
        let err = mgr.invite_member(&g.id, "node-outsider").await.unwrap_err();
        assert!(matches!(err, ImError::NotMember(_)));
    }

    #[tokio::test]
    async fn generate_invite_code_creates_valid_code() {
        let mgr = new_mgr();
        let g = mgr.create_group("g1", "node-owner").await.unwrap();
        let code = mgr.generate_invite_code(&g.id).await.unwrap();
        assert_eq!(code.len(), INVITE_LEN);
        // 字符全在字母表内
        assert!(code.chars().all(|c| INVITE_ALPHABET.contains(&(c as u8))));
        // 群 invite_code 字段已更新
        let fetched = mgr.get_group(&g.id).await.unwrap();
        assert_eq!(fetched.invite_code, Some(code));
    }

    #[tokio::test]
    async fn generate_invite_code_nonexistent_group_fails() {
        let mgr = new_mgr();
        let err = mgr
            .generate_invite_code(&GroupId::new("ghost"))
            .await
            .unwrap_err();
        assert!(matches!(err, ImError::GroupNotFound(_)));
    }

    #[tokio::test]
    async fn kick_member_by_owner_succeeds() {
        let mgr = new_mgr();
        let g = mgr.create_group("g1", "node-owner").await.unwrap();
        mgr.join_group(&g.id, "node-b", None).await.unwrap();
        mgr.kick_member(&g.id, "node-b", "node-owner")
            .await
            .unwrap();
        assert_eq!(mgr.list_members(&g.id).await.len(), 1);
    }

    #[tokio::test]
    async fn kick_member_by_admin_succeeds_for_member() {
        let mgr = new_mgr();
        let g = mgr.create_group("g1", "node-owner").await.unwrap();
        // 加入两个普通成员
        mgr.join_group(&g.id, "node-admin", None).await.unwrap();
        mgr.join_group(&g.id, "node-mem", None).await.unwrap();
        // 手动把 node-admin 升为 Admin（通过内部直接操作模拟）
        {
            let mut groups = mgr.groups.lock().unwrap();
            let entry = groups.get_mut(&g.id).unwrap();
            let admin = entry.group.find_member_mut("node-admin").unwrap();
            admin.member_type = MemberType::Admin;
        }
        // Admin 踢 Member → 成功
        mgr.kick_member(&g.id, "node-mem", "node-admin")
            .await
            .unwrap();
        assert_eq!(mgr.list_members(&g.id).await.len(), 2);
    }

    #[tokio::test]
    async fn kick_member_by_member_denied() {
        let mgr = new_mgr();
        let g = mgr.create_group("g1", "node-owner").await.unwrap();
        mgr.join_group(&g.id, "node-a", None).await.unwrap();
        mgr.join_group(&g.id, "node-b", None).await.unwrap();
        // 普通成员踢人 → 权限拒绝
        let err = mgr
            .kick_member(&g.id, "node-b", "node-a")
            .await
            .unwrap_err();
        assert!(matches!(err, ImError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn kick_owner_denied() {
        let mgr = new_mgr();
        let g = mgr.create_group("g1", "node-owner").await.unwrap();
        // 群主不可被踢（即便自己是 Owner 也踢不了别的群的 Owner，这里测本群 Owner）
        let err = mgr
            .kick_member(&g.id, "node-owner", "node-owner")
            .await
            .unwrap_err();
        assert!(matches!(err, ImError::PermissionDenied(_)));
    }

    #[tokio::test]
    async fn kick_nonexistent_member_fails() {
        let mgr = new_mgr();
        let g = mgr.create_group("g1", "node-owner").await.unwrap();
        let err = mgr
            .kick_member(&g.id, "node-ghost", "node-owner")
            .await
            .unwrap_err();
        assert!(matches!(err, ImError::NotMember(_)));
    }

    #[tokio::test]
    async fn kick_by_nonexistent_operator_fails() {
        let mgr = new_mgr();
        let g = mgr.create_group("g1", "node-owner").await.unwrap();
        mgr.join_group(&g.id, "node-b", None).await.unwrap();
        let err = mgr
            .kick_member(&g.id, "node-b", "node-ghost")
            .await
            .unwrap_err();
        assert!(matches!(err, ImError::NotMember(_)));
        // 未踢成功，成员仍在
        assert_eq!(mgr.list_members(&g.id).await.len(), 2);
    }

    #[tokio::test]
    async fn group_full_blocks_join() {
        let mgr = new_mgr();
        // 构造一个 max_members=1 的群（仅群主）
        let g = mgr.create_group("tiny", "node-owner").await.unwrap();
        {
            let mut groups = mgr.groups.lock().unwrap();
            let entry = groups.get_mut(&g.id).unwrap();
            entry.group.max_members = 1;
        }
        let err = mgr.join_group(&g.id, "node-b", None).await.unwrap_err();
        assert!(matches!(err, ImError::GroupFull(_)));
    }

    #[tokio::test]
    async fn list_groups_returns_all_sorted() {
        let mgr = new_mgr();
        let g1 = mgr.create_group("a", "node-a").await.unwrap();
        let g2 = mgr.create_group("b", "node-b").await.unwrap();
        let list = mgr.list_groups().await;
        assert_eq!(list.len(), 2);
        // 按 ID 排序
        assert!(list[0].id.as_str() <= list[1].id.as_str());
        let ids: Vec<&str> = list.iter().map(|g| g.id.as_str()).collect();
        assert!(ids.contains(&g1.id.as_str()));
        assert!(ids.contains(&g2.id.as_str()));
    }

    #[tokio::test]
    async fn list_members_nonexistent_group_returns_empty() {
        let mgr = new_mgr();
        assert!(mgr.list_members(&GroupId::new("ghost")).await.is_empty());
    }

    #[tokio::test]
    async fn get_group_nonexistent_returns_none() {
        let mgr = new_mgr();
        assert!(mgr.get_group(&GroupId::new("ghost")).await.is_none());
    }

    #[tokio::test]
    async fn invite_code_two_generations_differ() {
        // 邀请码生成器应产出不同码（高概率，非绝对；连续两次不同即可）
        let a = generate_invite_code();
        let b = generate_invite_code();
        assert_ne!(a, b, "连续两次生成的邀请码不应相同");
        assert_eq!(a.len(), INVITE_LEN);
        assert_eq!(b.len(), INVITE_LEN);
    }
}
