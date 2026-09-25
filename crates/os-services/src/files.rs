//! 文件管理 / 全文搜索 / 分享 / 同步（规划文档 §3.16 files 组件）
//!
//! 职责：
//! - 目录浏览（list_dir）
//! - 创建 / 撤销分享链接（带过期、密码、限速）
//! - 全文搜索（分页 + 命中片段 + 相关度评分）
//! - 同步配置查询（客户端文件同步）

use os_core::{DateTime, Deserialize, PageRequest, PageResponse, Serialize};

use crate::ServiceError;

// ----------------------------------------------------------------------------
// 分享链接
// ----------------------------------------------------------------------------

/// 文件分享链接
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareLink {
    /// 链接 ID
    pub id: String,
    /// 被分享的目标路径（文件 / 目录）
    pub target_path: String,
    /// 访问令牌（URL 凭证）
    pub token: String,
    /// 过期时间（None = 永不过期）
    pub expires_at: Option<DateTime>,
    /// 密码哈希（None = 无密码）
    pub password_hash: Option<String>,
    /// 下载限速（KB/s；None = 不限速）
    pub rate_limit_kbps: Option<u32>,
    /// 创建者（用户 ID）
    pub created_by: String,
}

// ----------------------------------------------------------------------------
// 文件条目 / 搜索命中 / 同步配置
// ----------------------------------------------------------------------------

/// 目录条目（文件或子目录）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// 名称
    pub name: String,
    /// 是否为目录
    pub is_dir: bool,
    /// 大小（字节；目录为 0）
    pub size: u64,
    /// 修改时间
    pub modified: DateTime,
    /// 权限（如 `"rwxr-xr-x"`）
    pub permissions: String,
}

/// 全文搜索命中
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    /// 命中文件路径
    pub path: String,
    /// 命中片段（高亮上下文）
    pub snippet: String,
    /// 相关度评分（越大越相关）
    pub score: f32,
}

/// 文件同步配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConfig {
    /// 是否启用同步
    pub enabled: bool,
    /// 同步间隔（秒）
    pub interval_secs: u32,
    /// 排除规则（glob 模式，如 `["*.tmp", ".Trash-*"]`）
    pub excludes: Vec<String>,
}

// ----------------------------------------------------------------------------
// FileManager trait（async）
// ----------------------------------------------------------------------------

/// 文件管理器——目录浏览、分享、搜索、同步配置。
#[allow(async_fn_in_trait)]
pub trait FileManager: Send + Sync {
    /// 列出指定目录下的条目。
    async fn list_dir(&self, path: &str) -> Result<Vec<FileEntry>, ServiceError>;

    /// 创建分享链接（返回持久化后的完整链接，含分配的 id/token）。
    async fn create_share_link(&self, link: ShareLink) -> Result<ShareLink, ServiceError>;

    /// 撤销分享链接。
    async fn revoke_share_link(&self, id: &str) -> Result<(), ServiceError>;

    /// 全文搜索，分页返回命中。
    async fn fulltext_search(
        &self,
        query: &str,
        page: PageRequest,
    ) -> Result<PageResponse<SearchHit>, ServiceError>;

    /// 查询指定路径的同步配置。
    async fn sync_config(&self, path: &str) -> Result<SyncConfig, ServiceError>;
}
