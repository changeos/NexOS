//! 协议层共享类型 —— 协议枚举 / 共享 / 共享选项 / 会话
//!
//! `ShareId` 直接复用 os-core 的 newtype（不在此重定义）。
//! `FileProtocol` 是各协议子 trait 的父 trait，提供统一的共享生命周期与会话管理接口。

use std::path::PathBuf;

use os_core::{Deserialize, Serialize};

// 重导出 ShareId（复用 os-core 的 newtype），便于下游 `use os_protocols::ShareId`
pub use os_core::ShareId;

use crate::ProtocolResult;

// ----------------------------------------------------------------------------
// 协议枚举
// ----------------------------------------------------------------------------

/// 文件共享协议
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// SMB / CIFS（编排 Samba）
    Smb,
    /// NFS（v3 nfsserve / v4 nfs-ganesha）
    Nfs,
    /// WebDAV
    Webdav,
    /// FTP
    Ftp,
    /// SFTP（SSH 文件传输）
    Sftp,
    /// S3 兼容对象存储（RustFS）
    S3,
}

// ----------------------------------------------------------------------------
// 共享 / 共享选项 / 会话
// ----------------------------------------------------------------------------

/// 文件共享（一个对外暴露的目录路径，绑定一种协议）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Share {
    /// 共享 ID（复用 os-core::ShareId）
    pub id: ShareId,
    /// 共享名（对外展示，如 SMB 的 share name）
    pub name: String,
    /// 协议
    pub protocol: Protocol,
    /// 共享的数据集路径（如 `/tank/media`）
    pub path: PathBuf,
    /// 是否只读
    pub read_only: bool,
    /// 允许访问的主机列表（CIDR / 主机名；空表示不限）
    pub hosts_allow: Vec<String>,
    /// 是否启用
    pub enabled: bool,
    /// 创建时间
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// 共享创建/更新时的协议无关选项
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShareOptions {
    /// 备注/描述
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// 是否在网络邻居中可见（SMB browseable）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browseable: Option<bool>,
    /// 是否允许 guest（匿名）访问
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guest_ok: Option<bool>,
    /// 最大并发连接数（None = 不限）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_connections: Option<u32>,
    /// 允许的用户列表（SMB valid users / SFTP allowed users）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub valid_users: Vec<String>,
}

/// 协议会话（已连接的客户端）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// 会话 ID（协议侧分配，如 SMB PID/UID 复合键、SFTP session id）
    pub id: String,
    /// 协议
    pub protocol: Protocol,
    /// 已认证用户（guest 时为 "guest"/"anonymous"）
    pub user: String,
    /// 客户端 IP
    pub client_ip: String,
    /// 建立连接时间
    pub connected_at: chrono::DateTime<chrono::Utc>,
    /// 关联的共享 ID
    pub share_id: ShareId,
}

// ----------------------------------------------------------------------------
// FileProtocol 父 trait（async）—— 各协议子 trait 继承它
// ----------------------------------------------------------------------------

/// 文件协议统一抽象——所有协议共享的共享生命周期与会话管理接口。
///
/// 各协议子 trait（`SmbManager`/`NfsManager`/...）通过
/// `pub trait XxxManager: FileProtocol` 继承本 trait，并补充协议特有方法。
#[allow(async_fn_in_trait)]
pub trait FileProtocol: Send + Sync {
    /// 创建共享（落盘协议配置后返回最终 Share）。
    async fn create_share(&self, share: Share, options: ShareOptions) -> ProtocolResult<Share>;

    /// 更新共享（修改 comment/valid_users 等选项）。
    async fn update_share(&self, id: &ShareId, options: ShareOptions) -> ProtocolResult<Share>;

    /// 删除共享（同时从协议配置中移除）。
    async fn delete_share(&self, id: &ShareId) -> ProtocolResult<()>;

    /// 列出所有共享。
    async fn list_shares(&self) -> ProtocolResult<Vec<Share>>;

    /// 查询单个共享。
    async fn get_share(&self, id: &ShareId) -> ProtocolResult<Share>;

    /// 列出当前活跃会话。
    async fn list_sessions(&self) -> ProtocolResult<Vec<Session>>;

    /// 关闭指定会话（强制踢出客户端）。
    async fn close_session(&self, session_id: &str) -> ProtocolResult<()>;
}
