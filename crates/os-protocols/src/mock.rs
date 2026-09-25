//! Mock 实现（feature gate `mock`）
//!
//! 供下游 agent（api / service / meta / provision 等）在单测/集成测中注入确定性、
//! 纯内存的协议层依赖，避免依赖真实 Samba/nfs-ganesha/dav-server/libunftp/russh。
//!
//! 用法（下游 `[dev-dependencies]`）：
//! ```toml
//! os-protocols = { workspace = true, features = ["mock"] }
//! ```
//!
//! 设计（见 `_conventions.md §5` / 规格 §5.2）：
//! - 实现完整 trait（`SmbManager`/`NfsManager`/`WebDavManager`/`FtpManager`/`SftpManager`
//!   及父 `FileProtocol`），默认返回安全值；
//! - 提供 builder 风格构造器预置共享/会话/错误，记录调用以供断言；
//! - 纯内存、无外部状态、确定性。
//!
//! 注：trait 用原生 `async fn in trait`（非 dyn 兼容，见 ADR-COMPAT-001）。
//! mock 作为具体类型实现，下游以具体类型或泛型注入即可。若需 `Box<dyn>`，须经 ADR
//! 把对应 trait 切换为 `#[async_trait]`。

#![cfg(feature = "mock")]

use std::sync::Mutex;

use os_core::ShareId;

use crate::common::{FileProtocol, Session, Share, ShareOptions};
use crate::error::{ProtocolError, ProtocolResult};
use crate::state::ShareStore;

// ============================================================================
// 通用 mock 状态：共享/会话存储 + 可选强制错误 + 调用记录
// ============================================================================

/// mock 共享状态——5 个 mock 共用的内部状态。
pub struct MockShareState {
    pub(crate) store: ShareStore,
    /// 强制下一次方法返回此错误（一次性，触发后清除）。
    pub(crate) forced_error: Option<ProtocolError>,
    /// 记录的调用序列（如 "create_share:s1"），供下游断言。
    pub(crate) calls: Vec<String>,
}

impl MockShareState {
    pub(crate) fn new() -> Self {
        Self {
            store: ShareStore::new(),
            forced_error: None,
            calls: Vec::new(),
        }
    }

    pub(crate) fn check_forced(&mut self) -> ProtocolResult<()> {
        if let Some(e) = self.forced_error.take() {
            return Err(e);
        }
        Ok(())
    }
}

impl Default for MockShareState {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// MockSmbManager
// ============================================================================

/// 内存版 [`crate::smb::SmbManager`]——预置共享/会话，记录调用，可注入错误。
pub struct MockSmbManager {
    inner: Mutex<MockShareState>,
}

impl Default for MockSmbManager {
    fn default() -> Self {
        Self::new()
    }
}

impl MockSmbManager {
    /// 构造空 mock。
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(MockShareState::new()),
        }
    }

    /// 预置一个共享（随后 `list_shares`/冲突检测可见）。
    #[must_use]
    pub fn with_share(self, share: Share) -> Self {
        {
            let st = self.inner.lock().expect("mock poisoned");
            let _ = st.store.put_share(share);
        }
        self
    }

    /// 强制下一次方法返回指定错误（一次性）。
    #[must_use]
    pub fn with_error(self, err: ProtocolError) -> Self {
        self.inner.lock().expect("mock poisoned").forced_error = Some(err);
        self
    }

    /// 取已记录的调用序列（断言用）。
    pub fn recorded_calls(&self) -> Vec<String> {
        self.inner.lock().expect("mock poisoned").calls.clone()
    }

    /// 当前共享数（断言用）。
    pub fn share_count(&self) -> usize {
        self.inner
            .lock()
            .expect("mock poisoned")
            .store
            .share_count()
    }
}

#[allow(async_fn_in_trait)]
impl FileProtocol for MockSmbManager {
    async fn create_share(&self, share: Share, _options: ShareOptions) -> ProtocolResult<Share> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.calls.push(format!("create_share:{}", share.id));
        st.store.put_share(share.clone())?;
        Ok(share)
    }

    async fn update_share(&self, id: &ShareId, _options: ShareOptions) -> ProtocolResult<Share> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.calls.push(format!("update_share:{id}"));
        st.store.get_share(id)
    }

    async fn delete_share(&self, id: &ShareId) -> ProtocolResult<()> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.calls.push(format!("delete_share:{id}"));
        st.store.remove_share(id)
    }

    async fn list_shares(&self) -> ProtocolResult<Vec<Share>> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.list_shares()
    }

    async fn get_share(&self, id: &ShareId) -> ProtocolResult<Share> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.get_share(id)
    }

    async fn list_sessions(&self) -> ProtocolResult<Vec<Session>> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.list_sessions()
    }

    async fn close_session(&self, session_id: &str) -> ProtocolResult<()> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.calls.push(format!("close_session:{session_id}"));
        st.store.close_session(session_id)
    }
}

#[allow(async_fn_in_trait)]
impl crate::smb::SmbManager for MockSmbManager {
    async fn write_smb_conf(&self) -> ProtocolResult<std::path::PathBuf> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.calls.push("write_smb_conf".into());
        Ok(std::path::PathBuf::from("/tmp/mock-smb.conf"))
    }

    async fn reload_smbd(&self) -> ProtocolResult<()> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.calls.push("reload_smbd".into());
        Ok(())
    }

    async fn enable_time_machine(
        &self,
        share: &ShareId,
        _size_limit_gb: Option<u64>,
    ) -> ProtocolResult<Share> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.calls.push(format!("enable_time_machine:{share}"));
        st.store.get_share(share)
    }

    async fn list_smb_sessions(&self) -> ProtocolResult<Vec<Session>> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.list_sessions()
    }
}

// ============================================================================
// MockNfsManager —— 复用同一份内部状态
// ============================================================================

/// 内存版 [`crate::nfs::NfsManager`]。
pub struct MockNfsManager {
    inner: Mutex<MockShareState>,
}

impl Default for MockNfsManager {
    fn default() -> Self {
        Self::new()
    }
}

impl MockNfsManager {
    /// 构造空 mock。
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(MockShareState::new()),
        }
    }

    /// 预置一个共享。
    #[must_use]
    pub fn with_share(self, share: Share) -> Self {
        {
            let st = self.inner.lock().expect("mock poisoned");
            let _ = st.store.put_share(share);
        }
        self
    }

    /// 强制错误（一次性）。
    #[must_use]
    pub fn with_error(self, err: ProtocolError) -> Self {
        self.inner.lock().expect("mock poisoned").forced_error = Some(err);
        self
    }

    /// 共享数（断言用）。
    pub fn share_count(&self) -> usize {
        self.inner
            .lock()
            .expect("mock poisoned")
            .store
            .share_count()
    }
}

#[allow(async_fn_in_trait)]
impl FileProtocol for MockNfsManager {
    async fn create_share(&self, share: Share, _options: ShareOptions) -> ProtocolResult<Share> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.put_share(share.clone())?;
        Ok(share)
    }

    async fn update_share(&self, id: &ShareId, _options: ShareOptions) -> ProtocolResult<Share> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.get_share(id)
    }

    async fn delete_share(&self, id: &ShareId) -> ProtocolResult<()> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.remove_share(id)
    }

    async fn list_shares(&self) -> ProtocolResult<Vec<Share>> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.list_shares()
    }

    async fn get_share(&self, id: &ShareId) -> ProtocolResult<Share> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.get_share(id)
    }

    async fn list_sessions(&self) -> ProtocolResult<Vec<Session>> {
        Ok(Vec::new())
    }

    async fn close_session(&self, session_id: &str) -> ProtocolResult<()> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.close_session(session_id)
    }
}

#[allow(async_fn_in_trait)]
impl crate::nfs::NfsManager for MockNfsManager {
    async fn add_export(
        &self,
        share: &ShareId,
        _clients: Vec<String>,
        _options: crate::nfs::NfsExportOptions,
    ) -> ProtocolResult<Share> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.get_share(share)
    }

    async fn remove_export(&self, share: &ShareId) -> ProtocolResult<()> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        if st.store.get_share(share).is_err() {
            return Err(ProtocolError::ShareNotFound(share.as_str().to_string()));
        }
        Ok(())
    }
}

// ============================================================================
// MockWebDavManager
// ============================================================================

/// 内存版 [`crate::webdav::WebDavManager`]。
pub struct MockWebDavManager {
    inner: Mutex<MockShareState>,
}

impl Default for MockWebDavManager {
    fn default() -> Self {
        Self::new()
    }
}

impl MockWebDavManager {
    /// 构造空 mock。
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(MockShareState::new()),
        }
    }

    /// 预置一个共享。
    #[must_use]
    pub fn with_share(self, share: Share) -> Self {
        {
            let st = self.inner.lock().expect("mock poisoned");
            let _ = st.store.put_share(share);
        }
        self
    }

    /// 共享数（断言用）。
    pub fn share_count(&self) -> usize {
        self.inner
            .lock()
            .expect("mock poisoned")
            .store
            .share_count()
    }
}

#[allow(async_fn_in_trait)]
impl FileProtocol for MockWebDavManager {
    async fn create_share(&self, share: Share, _options: ShareOptions) -> ProtocolResult<Share> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.put_share(share.clone())?;
        Ok(share)
    }

    async fn update_share(&self, id: &ShareId, _options: ShareOptions) -> ProtocolResult<Share> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.get_share(id)
    }

    async fn delete_share(&self, id: &ShareId) -> ProtocolResult<()> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.remove_share(id)
    }

    async fn list_shares(&self) -> ProtocolResult<Vec<Share>> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.list_shares()
    }

    async fn get_share(&self, id: &ShareId) -> ProtocolResult<Share> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.get_share(id)
    }

    async fn list_sessions(&self) -> ProtocolResult<Vec<Session>> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.list_sessions()
    }

    async fn close_session(&self, session_id: &str) -> ProtocolResult<()> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.close_session(session_id)
    }
}

#[allow(async_fn_in_trait)]
impl crate::webdav::WebDavManager for MockWebDavManager {}

// ============================================================================
// MockFtpManager
// ============================================================================

/// 内存版 [`crate::ftp::FtpManager`]。
pub struct MockFtpManager {
    inner: Mutex<MockShareState>,
}

impl Default for MockFtpManager {
    fn default() -> Self {
        Self::new()
    }
}

impl MockFtpManager {
    /// 构造空 mock。
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(MockShareState::new()),
        }
    }

    /// 预置一个共享。
    #[must_use]
    pub fn with_share(self, share: Share) -> Self {
        {
            let st = self.inner.lock().expect("mock poisoned");
            let _ = st.store.put_share(share);
        }
        self
    }

    /// 共享数（断言用）。
    pub fn share_count(&self) -> usize {
        self.inner
            .lock()
            .expect("mock poisoned")
            .store
            .share_count()
    }
}

#[allow(async_fn_in_trait)]
impl FileProtocol for MockFtpManager {
    async fn create_share(&self, share: Share, _options: ShareOptions) -> ProtocolResult<Share> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.put_share(share.clone())?;
        Ok(share)
    }

    async fn update_share(&self, id: &ShareId, _options: ShareOptions) -> ProtocolResult<Share> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.get_share(id)
    }

    async fn delete_share(&self, id: &ShareId) -> ProtocolResult<()> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.remove_share(id)
    }

    async fn list_shares(&self) -> ProtocolResult<Vec<Share>> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.list_shares()
    }

    async fn get_share(&self, id: &ShareId) -> ProtocolResult<Share> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.get_share(id)
    }

    async fn list_sessions(&self) -> ProtocolResult<Vec<Session>> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.list_sessions()
    }

    async fn close_session(&self, session_id: &str) -> ProtocolResult<()> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.close_session(session_id)
    }
}

#[allow(async_fn_in_trait)]
impl crate::ftp::FtpManager for MockFtpManager {}

// ============================================================================
// MockSftpManager
// ============================================================================

/// 内存版 [`crate::sftp::SftpManager`]——记录 authorized_keys 调用。
pub struct MockSftpManager {
    inner: Mutex<MockShareState>,
    /// 用户 → 公钥列表（authorize_key 记录）
    pub authorized_keys: Mutex<std::collections::HashMap<String, Vec<String>>>,
}

impl Default for MockSftpManager {
    fn default() -> Self {
        Self::new()
    }
}

impl MockSftpManager {
    /// 构造空 mock。
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(MockShareState::new()),
            authorized_keys: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// 预置一个共享。
    #[must_use]
    pub fn with_share(self, share: Share) -> Self {
        {
            let st = self.inner.lock().expect("mock poisoned");
            let _ = st.store.put_share(share);
        }
        self
    }

    /// 共享数（断言用）。
    pub fn share_count(&self) -> usize {
        self.inner
            .lock()
            .expect("mock poisoned")
            .store
            .share_count()
    }
}

#[allow(async_fn_in_trait)]
impl FileProtocol for MockSftpManager {
    async fn create_share(&self, share: Share, _options: ShareOptions) -> ProtocolResult<Share> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.put_share(share.clone())?;
        Ok(share)
    }

    async fn update_share(&self, id: &ShareId, _options: ShareOptions) -> ProtocolResult<Share> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.get_share(id)
    }

    async fn delete_share(&self, id: &ShareId) -> ProtocolResult<()> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.remove_share(id)
    }

    async fn list_shares(&self) -> ProtocolResult<Vec<Share>> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.list_shares()
    }

    async fn get_share(&self, id: &ShareId) -> ProtocolResult<Share> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.get_share(id)
    }

    async fn list_sessions(&self) -> ProtocolResult<Vec<Session>> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.list_sessions()
    }

    async fn close_session(&self, session_id: &str) -> ProtocolResult<()> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        st.store.close_session(session_id)
    }
}

#[allow(async_fn_in_trait)]
impl crate::sftp::SftpManager for MockSftpManager {
    async fn authorize_key(&self, user: &str, pubkey: &str) -> ProtocolResult<()> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        self.authorized_keys
            .lock()
            .expect("authkeys poisoned")
            .entry(user.to_string())
            .or_default()
            .push(pubkey.to_string());
        Ok(())
    }
}

// ============================================================================
// mock 自身的健全性测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::Protocol;
    use crate::nfs::NfsManager;
    use crate::sftp::SftpManager;
    use crate::smb::SmbManager;
    use chrono::Utc;
    use std::path::PathBuf;

    fn share(id: &str) -> Share {
        Share {
            id: ShareId::new(id),
            name: id.into(),
            protocol: Protocol::Smb,
            path: PathBuf::from("/tank/x"),
            read_only: false,
            hosts_allow: vec![],
            enabled: true,
            created_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn mock_smb_full_lifecycle() {
        let m = MockSmbManager::new();
        let s = m
            .create_share(share("s1"), ShareOptions::default())
            .await
            .unwrap();
        assert_eq!(s.id.as_str(), "s1");
        assert_eq!(m.share_count(), 1);
        assert_eq!(m.list_shares().await.unwrap().len(), 1);
        assert!(m.recorded_calls().iter().any(|c| c == "create_share:s1"));
        // write_smb_conf / reload_smbd / enable_time_machine
        assert_eq!(
            m.write_smb_conf().await.unwrap(),
            PathBuf::from("/tmp/mock-smb.conf")
        );
        m.reload_smbd().await.unwrap();
        let _ = m
            .enable_time_machine(&ShareId::new("s1"), None)
            .await
            .unwrap();
        // delete
        m.delete_share(&ShareId::new("s1")).await.unwrap();
        assert_eq!(m.share_count(), 0);
    }

    #[tokio::test]
    async fn mock_smb_forced_error_one_shot() {
        let m = MockSmbManager::new().with_error(ProtocolError::Internal("boom".into()));
        assert!(matches!(
            m.list_shares().await.unwrap_err(),
            ProtocolError::Internal(_)
        ));
        // 一次性：再调正常
        assert!(m.list_shares().await.is_ok());
    }

    #[tokio::test]
    async fn mock_nfs_add_remove_export() {
        let m = MockNfsManager::new().with_share(share("n1"));
        use crate::nfs::NfsExportOptions;
        let _ = m
            .add_export(
                &ShareId::new("n1"),
                vec!["10.0.0.0/24".into()],
                NfsExportOptions::default(),
            )
            .await
            .unwrap();
        m.remove_export(&ShareId::new("n1")).await.unwrap();
        // 移除不存在的共享的 export 报错
        assert!(matches!(
            m.remove_export(&ShareId::new("nope")).await.unwrap_err(),
            ProtocolError::ShareNotFound(_)
        ));
    }

    #[tokio::test]
    async fn mock_webdav_ftp_lifecycle() {
        let w = MockWebDavManager::new();
        w.create_share(share("w1"), ShareOptions::default())
            .await
            .unwrap();
        assert_eq!(w.share_count(), 1);

        let f = MockFtpManager::new();
        f.create_share(share("f1"), ShareOptions::default())
            .await
            .unwrap();
        assert_eq!(f.share_count(), 1);
    }

    #[tokio::test]
    async fn mock_sftp_authorize_records() {
        let s = MockSftpManager::new();
        s.authorize_key("alice", "ssh-rsa AAAA alice@host")
            .await
            .unwrap();
        let keys = s.authorized_keys.lock().unwrap();
        assert_eq!(
            keys.get("alice").map(Vec::as_slice).unwrap_or(&[]),
            ["ssh-rsa AAAA alice@host"]
        );
    }
}
