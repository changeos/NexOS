//! `DefaultFileManager` 与 `MockFileManager` —— [`crate::files::FileManager`] 实现。
//!
//! 设计（见 `docs/agents/files-agent.md` §3 / §5）：
//! - **`DefaultFileManager`**：
//!   - `list_dir`：基于 `std::fs` 枚举目录（`StorageBackend` 当前不含文件系统列举接口，
//!     见 §6 软依赖；故直接走 `std::fs`，避免虚构接口）。
//!   - `create_share_link` / `revoke_share_link`：内存 `DashMap` 风格状态（用 `std::sync::Mutex`
//!     + `HashMap`，无新依赖），分配 id / token。
//!   - `fulltext_search`：**tantivy 真实实现**（ADR-DEPS-001 已注册）。经 [`with_search_index`]
//!     注入 [`SearchIndex`] 后走真实 BM25 + 高亮 snippet；未注入则返回空结果（向后兼容）。
//!   - `sync_config`：返回默认 `SyncConfig`（真实配置存储待 client-agent 对接）。
//! - **`MockFileManager`**（feature `mock`）：纯内存、确定性，供下游测试注入。
//!
//! 权限 / 安全：分享链接密码仅存哈希（[`crate::files_model::hash_password`]，FNV 占位）。
//!
//! [`with_search_index`]: DefaultFileManager::with_search_index
//! [`SearchIndex`]: crate::search_index::SearchIndex

// 整合说明（orchestrator）：原 files-agent 用 `#![cfg(feature = "files")]` 把整个文件
// 挂在按组件的 `files` feature 上。现在统一用单一 `mock` feature，故去掉文件级 gate；
// `DefaultFileManager` 始终编译，`MockFileManager` 仍由各 item 的 `#[cfg(feature = "mock")]`
// 单独门控。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

use os_core::{DateTime, PageRequest, PageResponse, Utc};

use crate::files::{FileEntry, FileManager, SearchHit, ShareLink, SyncConfig};
use crate::files_model;
use crate::search_index::SearchIndex;
use crate::ServiceError;

// ============================================================================
// DefaultFileManager
// ============================================================================

/// 默认文件管理器——基于 `std::fs` + 内存分享链接表 + tantivy 全文搜索。
///
/// 状态：
/// - `shares`：分享链接表（id → ShareLink），内存态（生产应持久化到 DB，待 db 层对接）。
/// - `index`：可选的 [`SearchIndex`]（`Arc`，便于多 owner 共享）。未注入（`None`）时
///   `fulltext_search` 返回空结果——保留旧占位行为，便于未配置索引的调用方平滑降级。
///   注入后由 `DefaultFileManager::index_dir` 主动建索引（或调用方预先建好）。
pub struct DefaultFileManager {
    shares: Mutex<HashMap<String, ShareLink>>,
    /// 默认同步配置（构造时指定；`sync_config` 返回此值的拷贝）
    default_sync: SyncConfig,
    /// tantivy 全文索引句柄（`Arc` 允许与 indexer 后台任务共享；`None` = 未启用）。
    index: Option<Arc<SearchIndex>>,
}

impl DefaultFileManager {
    /// 构造，使用默认同步配置（enabled=false, interval=300s, 无 excludes），无全文索引。
    #[must_use]
    pub fn new() -> Self {
        Self {
            shares: Mutex::new(HashMap::new()),
            default_sync: SyncConfig {
                enabled: false,
                interval_secs: 300,
                excludes: vec![],
            },
            index: None,
        }
    }

    /// 指定默认同步配置。
    #[must_use]
    pub fn with_sync_config(mut self, cfg: SyncConfig) -> Self {
        self.default_sync = cfg;
        self
    }

    /// 注入 tantivy 全文索引（通常由 [`SearchIndex::create_in_dir`] 构造）。
    /// 注入后 [`FileManager::fulltext_search`] 将走真实 tantivy 查询；
    /// 未注入则返回空结果（向后兼容）。
    #[must_use]
    pub fn with_search_index(mut self, index: Arc<SearchIndex>) -> Self {
        self.index = Some(index);
        self
    }

    /// 当前分享链接总数（测试 / 监控用）。
    pub fn share_count(&self) -> usize {
        self.shares.lock().expect("shares poisoned").len()
    }

    /// 对指定根目录增量建索引（委派 [`SearchIndex::index_dir`]）。
    /// 未注入索引时返回 `Internal` 错误，提示调用方先 `with_search_index`。
    pub fn index_dir(&self, root: &str, extensions: &[String]) -> Result<usize, ServiceError> {
        match &self.index {
            Some(idx) => idx.index_dir(root, extensions),
            None => Err(ServiceError::Internal(
                "未配置全文索引：先调用 with_search_index".into(),
            )),
        }
    }

    /// 当前索引文档数（未注入索引返回 0）。
    pub fn index_num_docs(&self) -> u64 {
        self.index
            .as_ref()
            .map(|i| i.num_docs().unwrap_or(0))
            .unwrap_or(0)
    }
}

impl Default for DefaultFileManager {
    fn default() -> Self {
        Self::new()
    }
}

impl FileManager for DefaultFileManager {
    async fn list_dir(&self, path: &str) -> Result<Vec<FileEntry>, ServiceError> {
        // 在独立线程做阻塞 IO，避免阻塞异步运行时（tokio 的 block_in_place 需多线程运行时，
        // 此处为骨架实现直接读；生产应换 tokio::fs 或 StorageBackend 扩展接口）。
        let path = path.to_string();
        let entries = read_dir_sync(&path)?;
        Ok(entries)
    }

    async fn create_share_link(&self, link: ShareLink) -> Result<ShareLink, ServiceError> {
        let mut stored = link;
        // 分配 id（若调用方未给）与 token（始终新生成，保证唯一）
        if stored.id.is_empty() {
            stored.id = uuid::Uuid::new_v4().to_string();
        }
        stored.token = files_model::generate_share_token();
        // 校验：target_path 不应为空
        if stored.target_path.trim().is_empty() {
            return Err(ServiceError::Internal("target_path 不能为空".into()));
        }
        // 密码必须已哈希化（调用方负责）；此处仅断言形式（非空字符串）。
        // 注：本骨架不强制——若调用方传明文，由其负责。Mock 同理。
        let mut shares = self.shares.lock().expect("shares poisoned");
        shares.insert(stored.id.clone(), stored.clone());
        Ok(stored)
    }

    async fn revoke_share_link(&self, id: &str) -> Result<(), ServiceError> {
        let mut shares = self.shares.lock().expect("shares poisoned");
        if shares.remove(id).is_none() {
            return Err(ServiceError::LinkNotFound(id.to_string()));
        }
        Ok(())
    }

    async fn fulltext_search(
        &self,
        query: &str,
        page: PageRequest,
    ) -> Result<PageResponse<SearchHit>, ServiceError> {
        // 接通真实 tantivy 索引（files-agent 批 3 完成）。未注入索引时返回空结果
        // （向后兼容：调用方未配置索引不应报错，仅查询无结果）。
        match &self.index {
            Some(idx) => idx.search(query, page),
            None => Ok(PageResponse {
                items: vec![],
                total: 0,
                offset: page.offset,
                limit: page.limit,
            }),
        }
    }

    async fn sync_config(&self, _path: &str) -> Result<SyncConfig, ServiceError> {
        Ok(self.default_sync.clone())
    }
}

/// 同步读取目录条目（阻塞 IO 的纯函数封装，便于复用与测试）。
fn read_dir_sync(path: &str) -> Result<Vec<FileEntry>, ServiceError> {
    let p = Path::new(path);
    if !p.is_dir() {
        return Err(ServiceError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("不是目录或不存在: {path}"),
        )));
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(p)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        let modified: DateTime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| {
                DateTime::from_timestamp(d.as_secs() as i64, d.subsec_nanos())
                    .unwrap_or_else(Utc::now)
            })
            .unwrap_or_else(Utc::now);
        let permissions = format_permissions(&meta);
        out.push(FileEntry {
            name: entry.file_name().to_string_lossy().to_string(),
            is_dir: meta.is_dir(),
            size: if meta.is_dir() { 0 } else { meta.len() },
            modified,
            permissions,
        });
    }
    Ok(out)
}

/// 简易权限字符串（Unix mode → rwx）。
fn format_permissions(meta: &std::fs::Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        let mut s = String::with_capacity(9);
        for shift in [6, 3, 0] {
            let bits = (mode >> shift) & 0o7;
            s.push(if bits & 4 != 0 { 'r' } else { '-' });
            s.push(if bits & 2 != 0 { 'w' } else { '-' });
            s.push(if bits & 1 != 0 { 'x' } else { '-' });
        }
        s
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        "rw-r--r--".to_string()
    }
}

// ============================================================================
// MockFileManager（feature `mock`）
// ============================================================================

/// Mock 文件管理器——纯内存、确定性，供下游测试注入（api-agent / client-agent）。
///
/// 用法：
/// ```ignore
/// use os_services::mock::MockFileManager;
/// let fm = MockFileManager::new()
///     .with_entry("/dir", FileEntry { ... })
///     .with_share(share_link);
/// ```
#[cfg(feature = "mock")]
pub struct MockFileManager {
    inner: Mutex<MockState>,
}

#[cfg(feature = "mock")]
struct MockState {
    /// path → 目录条目列表
    dirs: HashMap<String, Vec<FileEntry>>,
    /// id → ShareLink
    shares: HashMap<String, ShareLink>,
    /// search 命中（固定返回，便于断言）
    search_hits: Vec<SearchHit>,
    /// sync 配置
    sync: SyncConfig,
    /// 强制错误（一次性）
    forced_error: Option<ServiceError>,
}

#[cfg(feature = "mock")]
impl Default for MockState {
    fn default() -> Self {
        Self {
            dirs: HashMap::new(),
            shares: HashMap::new(),
            search_hits: Vec::new(),
            sync: SyncConfig {
                enabled: false,
                interval_secs: 300,
                excludes: vec![],
            },
            forced_error: None,
        }
    }
}

#[cfg(feature = "mock")]
impl MockFileManager {
    /// 构造空 mock。
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(MockState::default()),
        }
    }

    /// 预置某目录的条目（`list_dir(path)` 将返回此列表的拷贝）。
    #[must_use]
    pub fn with_entry(self, path: impl Into<String>, entry: FileEntry) -> Self {
        self.with_entries(path, vec![entry])
    }

    /// 预置某目录的多个条目。
    #[must_use]
    pub fn with_entries(self, path: impl Into<String>, entries: Vec<FileEntry>) -> Self {
        self.inner
            .lock()
            .expect("mock poisoned")
            .dirs
            .insert(path.into(), entries);
        self
    }

    /// 预置一个分享链接（`create_share_link` 会再分配 token；预置的用于 `revoke` 校验）。
    #[must_use]
    pub fn with_share(self, link: ShareLink) -> Self {
        self.inner
            .lock()
            .expect("mock poisoned")
            .shares
            .insert(link.id.clone(), link);
        self
    }

    /// 预置搜索命中（`fulltext_search` 返回此列表分页）。
    #[must_use]
    pub fn with_search_hits(self, hits: Vec<SearchHit>) -> Self {
        self.inner.lock().expect("mock poisoned").search_hits = hits;
        self
    }

    /// 预置同步配置。
    #[must_use]
    pub fn with_sync_config(self, cfg: SyncConfig) -> Self {
        self.inner.lock().expect("mock poisoned").sync = cfg;
        self
    }

    /// 强制下一次方法调用返回指定错误（一次性）。
    #[must_use]
    pub fn with_error(self, err: ServiceError) -> Self {
        self.inner.lock().expect("mock poisoned").forced_error = Some(err);
        self
    }

    fn check_forced(&self) -> Result<(), ServiceError> {
        if let Some(e) = self
            .inner
            .lock()
            .expect("mock poisoned")
            .forced_error
            .take()
        {
            return Err(e);
        }
        Ok(())
    }

    /// 当前分享链接数（断言用）。
    pub fn share_count(&self) -> usize {
        self.inner.lock().expect("mock poisoned").shares.len()
    }
}

#[cfg(feature = "mock")]
impl Default for MockFileManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "mock")]
impl FileManager for MockFileManager {
    async fn list_dir(&self, path: &str) -> Result<Vec<FileEntry>, ServiceError> {
        self.check_forced()?;
        let st = self.inner.lock().expect("mock poisoned");
        match st.dirs.get(path) {
            Some(v) => Ok(v.clone()),
            None => Err(ServiceError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("mock 未预置目录: {path}"),
            ))),
        }
    }

    async fn create_share_link(&self, link: ShareLink) -> Result<ShareLink, ServiceError> {
        self.check_forced()?;
        let mut st = self.inner.lock().expect("mock poisoned");
        let mut stored = link;
        if stored.id.is_empty() {
            stored.id = uuid::Uuid::new_v4().to_string();
        }
        stored.token = files_model::generate_share_token();
        st.shares.insert(stored.id.clone(), stored.clone());
        Ok(stored)
    }

    async fn revoke_share_link(&self, id: &str) -> Result<(), ServiceError> {
        self.check_forced()?;
        let mut st = self.inner.lock().expect("mock poisoned");
        if st.shares.remove(id).is_none() {
            return Err(ServiceError::LinkNotFound(id.to_string()));
        }
        Ok(())
    }

    async fn fulltext_search(
        &self,
        _query: &str,
        page: PageRequest,
    ) -> Result<PageResponse<SearchHit>, ServiceError> {
        self.check_forced()?;
        let st = self.inner.lock().expect("mock poisoned");
        let total = u32::try_from(st.search_hits.len()).unwrap_or(u32::MAX);
        let offset = page.offset as usize;
        let limit = page.limit as usize;
        let items = if offset >= st.search_hits.len() {
            Vec::new()
        } else {
            let end = (offset + limit).min(st.search_hits.len());
            st.search_hits[offset..end].to_vec()
        };
        Ok(PageResponse {
            items,
            total,
            offset: page.offset,
            limit: page.limit,
        })
    }

    async fn sync_config(&self, _path: &str) -> Result<SyncConfig, ServiceError> {
        self.check_forced()?;
        Ok(self.inner.lock().expect("mock poisoned").sync.clone())
    }
}

// ============================================================================
// DefaultFileManager 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "mock")]
    fn entry(name: &str) -> FileEntry {
        FileEntry {
            name: name.into(),
            is_dir: false,
            size: 10,
            modified: Utc::now(),
            permissions: "rw-r--r--".into(),
        }
    }

    #[tokio::test]
    async fn default_list_dir_missing() {
        let fm = DefaultFileManager::new();
        let err = fm.list_dir("/no/such/path/xyz").await.unwrap_err();
        assert!(matches!(err, ServiceError::Io(_)));
    }

    #[tokio::test]
    async fn default_list_dir_temp() {
        let dir = std::env::temp_dir().join(format!("fm-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"hi").unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        let fm = DefaultFileManager::new();
        let mut entries = fm.list_dir(dir.to_str().unwrap()).await.unwrap();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"sub"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn default_create_and_revoke_share() {
        let fm = DefaultFileManager::new();
        let link = ShareLink {
            id: String::new(),
            target_path: "/x".into(),
            token: String::new(),
            expires_at: None,
            password_hash: None,
            rate_limit_kbps: Some(100),
            created_by: "u".into(),
        };
        let created = fm.create_share_link(link).await.unwrap();
        assert!(!created.id.is_empty());
        assert!(!created.token.is_empty());
        assert_eq!(fm.share_count(), 1);
        fm.revoke_share_link(&created.id).await.unwrap();
        assert_eq!(fm.share_count(), 0);
    }

    #[tokio::test]
    async fn default_revoke_missing_returns_link_not_found() {
        let fm = DefaultFileManager::new();
        let err = fm.revoke_share_link("nope").await.unwrap_err();
        assert!(matches!(err, ServiceError::LinkNotFound(_)));
    }

    #[tokio::test]
    async fn default_create_share_empty_path_rejected() {
        let fm = DefaultFileManager::new();
        let link = ShareLink {
            id: String::new(),
            target_path: "  ".into(),
            token: String::new(),
            expires_at: None,
            password_hash: None,
            rate_limit_kbps: None,
            created_by: "u".into(),
        };
        let err = fm.create_share_link(link).await.unwrap_err();
        assert!(matches!(err, ServiceError::Internal(_)));
    }

    #[tokio::test]
    async fn default_fulltext_search_empty_skeleton() {
        // 未注入索引：返回空结果（向后兼容），不报错。
        let fm = DefaultFileManager::new();
        let r = fm
            .fulltext_search(
                "anything",
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(r.total, 0);
        assert!(r.items.is_empty());
    }

    #[tokio::test]
    async fn default_fulltext_search_with_tantivy_index() {
        // 注入 RAM 索引 + 建几个文件 → fulltext_search 走真实 tantivy 查询。
        use crate::search_index::IndexedFile;
        let idx = std::sync::Arc::new(crate::search_index::SearchIndex::create_in_ram());
        idx.add_file(&IndexedFile {
            path: "/notes/tantivy.md".into(),
            name: "tantivy.md".into(),
            content: "Tantivy is a fast full text search engine written in rust".into(),
        })
        .unwrap();
        idx.add_file(&IndexedFile {
            path: "/notes/other.md".into(),
            name: "other.md".into(),
            content: "unrelated content".into(),
        })
        .unwrap();
        idx.commit().unwrap();
        let fm = DefaultFileManager::new().with_search_index(idx);

        let r = fm
            .fulltext_search(
                "tantivy",
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(r.total, 1);
        assert_eq!(r.items[0].path, "/notes/tantivy.md");
        assert!(r.items[0].snippet.to_lowercase().contains("tantivy"));
        assert!(r.items[0].score > 0.0);
    }

    #[tokio::test]
    async fn default_fulltext_search_pagination() {
        use crate::search_index::IndexedFile;
        let idx = std::sync::Arc::new(crate::search_index::SearchIndex::create_in_ram());
        for i in 0..5 {
            idx.add_file(&IndexedFile {
                path: format!("/d/{i}.md"),
                name: format!("{i}.md"),
                content: format!("rust content number {i}"),
            })
            .unwrap();
        }
        idx.commit().unwrap();
        let fm = DefaultFileManager::new().with_search_index(idx);
        let r = fm
            .fulltext_search(
                "rust",
                PageRequest {
                    offset: 2,
                    limit: 2,
                },
            )
            .await
            .unwrap();
        assert_eq!(r.total, 5);
        assert_eq!(r.items.len(), 2);
        assert_eq!(r.offset, 2);
        assert_eq!(r.limit, 2);
    }

    #[tokio::test]
    async fn default_index_dir_builds_real_index() {
        // 创建临时目录、写几个文件、调 index_dir 建索引、再查询验证。
        let root = std::env::temp_dir().join(format!("fm-idx-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.md"), "the quick brown fox rust").unwrap();
        std::fs::write(root.join("sub/b.md"), "lazy dog rust language").unwrap();
        std::fs::write(root.join("c.txt"), "rust in txt").unwrap(); // 被白名单过滤
        std::fs::write(root.join("d.md"), "no keyword").unwrap();

        let idx = std::sync::Arc::new(crate::search_index::SearchIndex::create_in_ram());
        let fm = DefaultFileManager::new().with_search_index(idx);
        let n = fm
            .index_dir(root.to_str().unwrap(), &["md".into()])
            .unwrap();
        assert_eq!(n, 3); // a/sub-b/d 共 3 个 .md
        assert!(fm.index_num_docs() >= 3);

        let r = fm
            .fulltext_search(
                "rust",
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(r.total, 2); // a / sub-b 命中
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn default_index_dir_without_index_errors() {
        let fm = DefaultFileManager::new();
        let err = fm.index_dir("/x", &[]).unwrap_err();
        assert!(matches!(err, ServiceError::Internal(_)));
    }

    #[tokio::test]
    async fn default_sync_config_default() {
        let fm = DefaultFileManager::new();
        let cfg = fm.sync_config("/x").await.unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.interval_secs, 300);
    }

    // ---- MockFileManager ----

    #[cfg(feature = "mock")]
    #[tokio::test]
    async fn mock_list_dir_returns_preset() {
        let fm = MockFileManager::new().with_entry("/d", entry("a.txt"));
        let v = fm.list_dir("/d").await.unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "a.txt");
    }

    #[cfg(feature = "mock")]
    #[tokio::test]
    async fn mock_list_dir_missing_path() {
        let fm = MockFileManager::new();
        let err = fm.list_dir("/missing").await.unwrap_err();
        assert!(matches!(err, ServiceError::Io(_)));
    }

    #[cfg(feature = "mock")]
    #[tokio::test]
    async fn mock_create_and_revoke_share() {
        let fm = MockFileManager::new();
        let link = ShareLink {
            id: String::new(),
            target_path: "/x".into(),
            token: String::new(),
            expires_at: None,
            password_hash: None,
            rate_limit_kbps: None,
            created_by: "u".into(),
        };
        let created = fm.create_share_link(link).await.unwrap();
        assert!(!created.token.is_empty());
        fm.revoke_share_link(&created.id).await.unwrap();
        let err = fm.revoke_share_link(&created.id).await.unwrap_err();
        assert!(matches!(err, ServiceError::LinkNotFound(_)));
    }

    #[cfg(feature = "mock")]
    #[tokio::test]
    async fn mock_fulltext_search_paginates_preset_hits() {
        let hits = vec![
            SearchHit {
                path: "a".into(),
                snippet: String::new(),
                score: 1.0,
            },
            SearchHit {
                path: "b".into(),
                snippet: String::new(),
                score: 2.0,
            },
            SearchHit {
                path: "c".into(),
                snippet: String::new(),
                score: 3.0,
            },
        ];
        let fm = MockFileManager::new().with_search_hits(hits);
        let r = fm
            .fulltext_search(
                "q",
                PageRequest {
                    offset: 1,
                    limit: 1,
                },
            )
            .await
            .unwrap();
        assert_eq!(r.total, 3);
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].path, "b");
    }

    #[cfg(feature = "mock")]
    #[tokio::test]
    async fn mock_forced_error_one_shot() {
        let fm = MockFileManager::new().with_error(ServiceError::Internal("boom".into()));
        let err = fm.list_dir("/d").await.unwrap_err();
        assert!(matches!(err, ServiceError::Internal(_)));
        // 第二次不再报错（已清）
        let _ = fm.list_dir("/d").await; // 返回 NotFound（未预置）
    }

    #[cfg(feature = "mock")]
    #[tokio::test]
    async fn mock_sync_config_preset() {
        let cfg = SyncConfig {
            enabled: true,
            interval_secs: 60,
            excludes: vec!["*.tmp".into()],
        };
        let fm = MockFileManager::new().with_sync_config(cfg);
        let got = fm.sync_config("/x").await.unwrap();
        assert!(got.enabled);
        assert_eq!(got.interval_secs, 60);
        assert_eq!(got.excludes, vec!["*.tmp".to_string()]);
    }

    // ---- read_dir_sync 边界 ----

    #[test]
    fn read_dir_sync_nonexistent_returns_io_error() {
        let err = read_dir_sync("/no/such/dir/xyz/abc").unwrap_err();
        assert!(matches!(err, ServiceError::Io(_)));
    }

    #[test]
    fn read_dir_sync_file_not_directory_returns_io_error() {
        // 传一个文件路径（非目录）→ NotFound
        let err = read_dir_sync("/etc/hostname").unwrap_err();
        assert!(matches!(err, ServiceError::Io(_)));
    }

    #[test]
    fn read_dir_sync_lists_entries_with_permissions() {
        // 真实读临时目录：验证条目 + permissions 非空
        let dir = std::env::temp_dir().join(format!("rds-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), b"hi").unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        let entries = read_dir_sync(dir.to_str().unwrap()).unwrap();
        assert!(entries.len() >= 2);
        // permissions 字符串应 9 字符（rwxr-xr-x 等）
        for e in &entries {
            assert_eq!(e.permissions.len(), 9);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- format_permissions ----

    #[cfg(unix)]
    #[test]
    fn format_permissions_full_access() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = std::env::temp_dir().join(format!("fp-test-{}", uuid::Uuid::new_v4()));
        std::fs::write(&tmp, b"x").unwrap();
        // 0o777 → rwxrwxrwx
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o777)).unwrap();
        let meta = std::fs::metadata(&tmp).unwrap();
        assert_eq!(format_permissions(&meta), "rwxrwxrwx");
        // 0o000 → ---------
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o000)).unwrap();
        let meta = std::fs::metadata(&tmp).unwrap();
        assert_eq!(format_permissions(&meta), "---------");
        // 0o755 → rwxr-xr-x
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        let meta = std::fs::metadata(&tmp).unwrap();
        assert_eq!(format_permissions(&meta), "rwxr-xr-x");
        // 0o644 → rw-r--r--
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
        let meta = std::fs::metadata(&tmp).unwrap();
        assert_eq!(format_permissions(&meta), "rw-r--r--");
        std::fs::remove_file(&tmp).ok();
    }

    // ---- DefaultFileManager builders ----

    #[tokio::test]
    async fn default_with_sync_config_overrides() {
        let cfg = SyncConfig {
            enabled: true,
            interval_secs: 120,
            excludes: vec!["*.bak".into()],
        };
        let fm = DefaultFileManager::new().with_sync_config(cfg);
        let got = fm.sync_config("/x").await.unwrap();
        assert!(got.enabled);
        assert_eq!(got.interval_secs, 120);
        assert_eq!(got.excludes, vec!["*.bak".to_string()]);
    }

    #[tokio::test]
    async fn default_default_sync_config() {
        let fm = DefaultFileManager::default(); // default() == new()
        let cfg = fm.sync_config("/x").await.unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.interval_secs, 300);
        assert!(cfg.excludes.is_empty());
    }

    #[tokio::test]
    async fn default_index_num_docs_zero_without_index() {
        let fm = DefaultFileManager::new();
        // 未注入索引 → num_docs 返回 0
        assert_eq!(fm.index_num_docs(), 0);
    }
}
