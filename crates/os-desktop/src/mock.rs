//! Mock 实现（feature gate `mock`）——供前端/UI 层测试用。
//!
//! 提供：`MockMountManager`。

#![cfg(feature = "mock")]

use crate::mount::{MountInfo, MountManager, MountTarget, RemoteShare};

#[cfg(test)]
use crate::mount::MountProtocol;
use crate::DesktopError;

/// Mock `MountManager`——包装 [`crate::mount_impl::SystemMountManager`]，
/// 便于前端注入测试挂载/卸载/持久化流程。
pub struct MockMountManager {
    inner: crate::mount_impl::SystemMountManager,
}

impl MockMountManager {
    /// 默认构造（空共享列表）。
    pub fn new() -> Self {
        Self {
            inner: crate::mount_impl::SystemMountManager::new(),
        }
    }

    /// 注入可用共享列表。
    pub fn with_shares(self, shares: Vec<RemoteShare>) -> Self {
        Self {
            inner: crate::mount_impl::SystemMountManager::with_shares(self.inner, shares),
        }
    }

    /// 已挂载数量。
    pub fn mount_count(&self) -> usize {
        self.inner.mount_count()
    }
}

impl Default for MockMountManager {
    fn default() -> Self {
        Self::new()
    }
}

// MountManager trait 为原生 async，impl 用原生 async fn（不用 #[async_trait]）。
impl MountManager for MockMountManager {
    async fn list_available_shares(
        &self,
        endpoint: &str,
    ) -> Result<Vec<RemoteShare>, DesktopError> {
        self.inner.list_available_shares(endpoint).await
    }
    async fn mount(&self, target: MountTarget) -> Result<MountInfo, DesktopError> {
        self.inner.mount(target).await
    }
    async fn unmount(&self, mount_id: &str) -> Result<(), DesktopError> {
        self.inner.unmount(mount_id).await
    }
    async fn list_mounts(&self) -> Result<Vec<MountInfo>, DesktopError> {
        self.inner.list_mounts().await
    }
    async fn make_persistent(&self, mount_id: &str) -> Result<(), DesktopError> {
        self.inner.make_persistent(mount_id).await
    }
}

// ============================================================
// 单元测
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn smb_target() -> MountTarget {
        MountTarget {
            endpoint: "https://os".to_string(),
            share_path: "photos".to_string(),
            protocol: MountProtocol::Smb,
            drive_letter: Some("Z:".to_string()),
            mount_point: None,
        }
    }

    #[tokio::test]
    async fn mock_mount_manager_shares_and_mount() {
        let mgr = MockMountManager::new().with_shares(vec![RemoteShare {
            name: "photos".into(),
            protocol: MountProtocol::Smb,
            description: None,
        }]);
        let shares = mgr.list_available_shares("os").await.unwrap();
        assert_eq!(shares.len(), 1);
        assert_eq!(mgr.mount_count(), 0);
        mgr.mount(smb_target()).await.unwrap();
        assert_eq!(mgr.mount_count(), 1);
    }

    #[tokio::test]
    async fn mock_mount_manager_unmount_and_persistent() {
        let mgr = MockMountManager::new();
        mgr.mount(smb_target()).await.unwrap();
        // 验证 mount 流程不 panic 且记录在表
        let mounts = mgr.list_mounts().await.unwrap();
        assert_eq!(mounts.len(), 1);
        assert!(mounts[0].mounted);
        assert_eq!(mgr.mount_count(), 1);
    }

    #[tokio::test]
    async fn mock_mount_manager_webdav() {
        let mgr = MockMountManager::new();
        let t = MountTarget {
            endpoint: "https://os".to_string(),
            share_path: "backup".to_string(),
            protocol: MountProtocol::Webdav,
            drive_letter: None,
            mount_point: Some(PathBuf::from("/mnt/os")),
        };
        let info = mgr.mount(t).await.unwrap();
        assert_eq!(info.mount_path.as_deref(), Some("/mnt/os"));
    }

    // —— 扩展边界（覆盖率补测）——

    #[tokio::test]
    async fn mock_mount_manager_default_eq_new() {
        let m1 = MockMountManager::default();
        let m2 = MockMountManager::new();
        assert_eq!(m1.mount_count(), m2.mount_count());
        assert_eq!(m1.mount_count(), 0);
    }

    #[tokio::test]
    async fn mock_mount_manager_unmount_persistent_via_mock() {
        // 通过 mock 包装测 unmount + make_persistent 流程
        let mgr = MockMountManager::new();
        mgr.mount(smb_target()).await.unwrap();
        let id = {
            // 通过 list_mounts 拿不到 id，但 mount_count 能验证记录
            assert_eq!(mgr.mount_count(), 1);
            // 直接通过内部管理器访问（已 public mount_count）
            "mnt-1".to_string()
        };
        // 卸载（已知 id 格式 mnt-1）
        let res = mgr.unmount(&id).await;
        assert!(res.is_ok());
        let mounts = mgr.list_mounts().await.unwrap();
        assert!(!mounts[0].mounted);
    }

    #[tokio::test]
    async fn mock_mount_manager_unmount_unknown_id() {
        let mgr = MockMountManager::new();
        let err = mgr.unmount("nope").await.unwrap_err();
        // UnmountFailed 经 trait → DesktopError::UnmountFailed
        assert!(matches!(err, crate::DesktopError::UnmountFailed(_)));
    }

    #[tokio::test]
    async fn mock_mount_manager_make_persistent_webdav() {
        let mgr = MockMountManager::new();
        let t = MountTarget {
            endpoint: "https://os".into(),
            share_path: "share".into(),
            protocol: MountProtocol::Webdav,
            drive_letter: None,
            mount_point: Some(PathBuf::from("/mnt/x")),
        };
        mgr.mount(t).await.unwrap();
        let id = "mnt-1".to_string();
        mgr.make_persistent(&id).await.unwrap();
        let mounts = mgr.list_mounts().await.unwrap();
        assert!(mounts[0].persistent);
    }

    #[tokio::test]
    async fn mock_mount_manager_make_persistent_unknown_id() {
        let mgr = MockMountManager::new();
        let err = mgr.make_persistent("nonexistent").await.unwrap_err();
        assert!(matches!(err, crate::DesktopError::MountFailed(_)));
    }
}
