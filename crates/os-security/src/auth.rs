//! 用户认证——用户管理、密码校验、身份令牌（Principal）
//!
//! 决策依据：规划文档 §3.16 / §3.18 —— 用户角色含 `ChainVerifiedGuest`（链上凭证访客）。
//! 安全约束：`Credentials` 仅存 `password_hash`，绝不存明文。

use os_core::DateTime;
use serde::{Deserialize, Serialize};

// ----------------------------------------------------------------------------
// 标识与角色
// ----------------------------------------------------------------------------

/// 用户 ID（newtype String）
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UserId(pub String);

impl UserId {
    /// 从任意字符串构造
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    /// 取字符串切片
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for UserId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// 用户角色
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// 管理员（全权）
    Admin,
    /// 普通用户
    User,
    /// 访客
    Guest,
    /// 链上凭证访客（呼应 §3.18：通过链上凭证验证身份的临时访客）
    ChainVerifiedGuest,
}

impl Role {
    /// 是否为高权角色（强制 2FA 的判断依据，呼应 twofactor 模块文档）。
    ///
    /// `Admin` 视为高权；`User`/`Guest`/`ChainVerifiedGuest` 视为普通。
    pub fn is_high_privilege(self) -> bool {
        matches!(self, Role::Admin)
    }

    /// 是否为访客类角色（含普通访客与链上凭证访客）。
    pub fn is_guest(self) -> bool {
        matches!(self, Role::Guest | Role::ChainVerifiedGuest)
    }
}

/// 角色列表校验：不能为空，且不得重复。
///
/// 返回 `Err(SecurityError::Internal)` 描述具体问题；合法返回 `Ok(())`。
pub fn validate_roles(roles: &[Role]) -> Result<(), crate::SecurityError> {
    if roles.is_empty() {
        return Err(crate::SecurityError::Internal("角色列表不能为空".into()));
    }
    // Role 是 Copy + Eq，O(n²) 去重检查对极小列表足够且无外部依赖。
    for (i, r) in roles.iter().enumerate() {
        if roles[i + 1..].iter().any(|x| x == r) {
            return Err(crate::SecurityError::Internal(format!(
                "角色列表存在重复: {:?}",
                r
            )));
        }
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// 用户与凭证
// ----------------------------------------------------------------------------

/// 用户
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    /// 用户 ID
    pub id: UserId,
    /// 显示名
    pub name: String,
    /// 角色列表
    pub roles: Vec<Role>,
    /// 是否启用
    pub enabled: bool,
    /// 创建时间（UTC）
    pub created_at: DateTime,
}

impl User {
    /// 构造用户（构造时校验角色列表）。
    pub fn new(
        id: UserId,
        name: impl Into<String>,
        roles: Vec<Role>,
        created_at: DateTime,
    ) -> Result<Self, crate::SecurityError> {
        validate_roles(&roles)?;
        Ok(Self {
            id,
            name: name.into(),
            roles,
            enabled: true,
            created_at,
        })
    }

    /// 是否拥有指定角色。
    pub fn has_role(&self, role: Role) -> bool {
        self.roles.contains(&role)
    }
}

/// 凭证（仅存哈希，绝不存明文密码）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credentials {
    /// 关联用户 ID
    pub user_id: UserId,
    /// 密码哈希（Argon2id 等算法输出；本字段不存明文）
    pub password_hash: String,
    /// 最近一次更新时间
    pub updated_at: DateTime,
}

impl Credentials {
    /// 构造凭证——只接收**已哈希**的字符串，强制调用方先完成哈希。
    ///
    /// 红线：本类型绝不接受明文密码；构造器不做哈希（哈希由 `password` 模块负责）。
    /// 这里仅做最低限度防御：若传入值疑似明文（长度 < 16 或含空格），返回错误。
    pub fn new(
        user_id: UserId,
        password_hash: impl Into<String>,
        updated_at: DateTime,
    ) -> Result<Self, crate::SecurityError> {
        let h = password_hash.into();
        // 防御性校验：合法密码哈希（argon2/bcrypt/scrypt 输出）通常远长于 16 字符且不含空格。
        // 这里只是阻止明显的误用（如直接传入明文），不替代真正的哈希算法。
        if h.len() < 16 || h.contains(' ') {
            return Err(crate::SecurityError::Internal(
                "password_hash 疑似明文（长度过短或含空格），拒绝存储".into(),
            ));
        }
        Ok(Self {
            user_id,
            password_hash: h,
            updated_at,
        })
    }
}

// ----------------------------------------------------------------------------
// 认证结果：Principal（内存身份令牌）
// ----------------------------------------------------------------------------

/// 认证成功后的身份令牌（内存用，签发 JWT 的输入）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Principal {
    /// 用户
    pub user: User,
    /// 认证时刻生效的角色
    pub roles: Vec<Role>,
    /// 认证时间（UTC）
    pub auth_time: DateTime,
}

impl Principal {
    /// 构造 Principal（构造时校验角色列表）。
    pub fn new(
        user: User,
        roles: Vec<Role>,
        auth_time: DateTime,
    ) -> Result<Self, crate::SecurityError> {
        validate_roles(&roles)?;
        Ok(Self {
            user,
            roles,
            auth_time,
        })
    }
}

// ----------------------------------------------------------------------------
// AuthProvider trait（async）
// ----------------------------------------------------------------------------

/// 认证提供者——用户管理与密码校验。
///
/// 实现者：`DbAuthProvider`（基于用户库 + Argon2id）；可替换为 LDAP/OIDC 适配器。
/// 安全：`set_password` 内部完成哈希；`authenticate` 内部完成哈希比对。
#[allow(async_fn_in_trait)]
pub trait AuthProvider: Send + Sync {
    /// 创建用户。
    async fn create_user(&self, name: &str, roles: Vec<Role>)
        -> Result<User, crate::SecurityError>;

    /// 删除用户。
    async fn delete_user(&self, user: &UserId) -> Result<(), crate::SecurityError>;

    /// 列出所有用户。
    async fn list_users(&self) -> Result<Vec<User>, crate::SecurityError>;

    /// 设置用户密码（内部哈希后存储；不接收/不存明文）。
    async fn set_password(&self, user: &UserId, password: &str)
        -> Result<(), crate::SecurityError>;

    /// 认证——校验用户名/密码，成功返回 `Principal`，失败返回 `AuthFailed`。
    async fn authenticate(
        &self,
        name: &str,
        password: &str,
    ) -> Result<Principal, crate::SecurityError>;

    /// 禁用用户（标记 enabled = false）。
    async fn disable_user(&self, user: &UserId) -> Result<(), crate::SecurityError>;
}
