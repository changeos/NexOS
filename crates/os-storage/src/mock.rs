//! `MockStorageBackend` —— 纯内存 [`crate::StorageBackend`] 实现，供下游测试注入。
//!
//! 仅在 `mock` feature 下编译。下游（protocol/compute/meta/service/provision）在
//! `[dev-dependencies]` 加 `os-storage = { workspace = true, features = ["mock"] }`。
//!
//! 设计（见 `_conventions.md §5`）：
//! - 实现完整 `StorageBackend` trait，**不依赖外部状态**（无 CLI/无文件）。
//! - 提供构造器预置返回值：`MockStorageBackend::new().with_pool(p).with_dataset(ds)`。
//! - 写操作（create/destroy/snapshot/set_quota）更新内部状态，使后续读操作反映变更——
//!   便于下游测试「创建后列出」「销毁后不存在」等场景。
//! - 错误注入：`with_error` / `with_error_fn` 让下游测试错误路径。
//!
//! 注：当前只 mock 了 `StorageBackend`（下游最常用）。`Replication`/`BlockExport`/
//! `CryptoManager` 的 mock 按需补充（下游提出需求时再加，避免过度设计）。

#![cfg(feature = "mock")]

use crate::backend::StorageBackend;
use crate::error::StorageError;
use crate::model::{Dataset, Pool, Quota, Snapshot, Vdev, VdevKind, VdevSpec};
use crate::options::DatasetOptions;
use os_core::{DatasetId, PoolId, SnapshotId};
use std::collections::HashMap;
use std::sync::Mutex;

/// Mock 存储后端——纯内存、确定性。
///
/// 内部状态：pools / datasets / snapshots / quotas 四张 HashMap（按 ID 索引），
/// 加可选的强制错误（注入测试错误路径）。所有方法永不 spawn 子进程、永不 panic
/// （除非锁中毒）。
pub struct MockStorageBackend {
    inner: Mutex<MockState>,
}

struct MockState {
    pools: HashMap<String, Pool>,
    datasets: HashMap<String, Dataset>,
    snapshots: HashMap<String, Snapshot>,
    /// dataset → quota（缺省返回 Quota::default 即全 None）
    quotas: HashMap<String, Quota>,
    /// 强制错误：下次匹配方法返回此错误（None = 正常）
    forced_error: Option<StorageError>,
}

impl MockState {
    fn new() -> Self {
        Self {
            pools: HashMap::new(),
            datasets: HashMap::new(),
            snapshots: HashMap::new(),
            quotas: HashMap::new(),
            forced_error: None,
        }
    }

    fn check_forced(&mut self) -> Result<(), StorageError> {
        if let Some(e) = self.forced_error.take() {
            return Err(e);
        }
        Ok(())
    }
}

impl MockStorageBackend {
    /// 构造空 mock（无 pool/dataset）。
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(MockState::new()),
        }
    }

    /// 预置一个 pool（随后 `list_pools`/`create_pool` 冲突检测可见）。
    #[must_use]
    pub fn with_pool(self, pool: Pool) -> Self {
        {
            let mut st = self.inner.lock().expect("mock poisoned");
            st.pools.insert(pool.id.as_str().to_string(), pool);
        }
        self
    }

    /// 预置一个 dataset。
    #[must_use]
    pub fn with_dataset(self, ds: Dataset) -> Self {
        {
            let mut st = self.inner.lock().expect("mock poisoned");
            st.datasets.insert(ds.id.as_str().to_string(), ds);
        }
        self
    }

    /// 预置一个 snapshot。
    #[must_use]
    pub fn with_snapshot(self, snap: Snapshot) -> Self {
        {
            let mut st = self.inner.lock().expect("mock poisoned");
            st.snapshots.insert(snap.id.as_str().to_string(), snap);
        }
        self
    }

    /// 强制下次（任一）方法返回指定错误。一次性——只触发一次后清除。
    #[must_use]
    pub fn with_error(self, err: StorageError) -> Self {
        {
            let mut st = self.inner.lock().expect("mock poisoned");
            st.forced_error = Some(err);
        }
        self
    }

    /// 当前 pool 数量（断言用）。
    pub fn pool_count(&self) -> usize {
        self.inner.lock().expect("mock poisoned").pools.len()
    }

    /// 当前 dataset 数量（断言用）。
    pub fn dataset_count(&self) -> usize {
        self.inner.lock().expect("mock poisoned").datasets.len()
    }

    /// 当前快照数量。
    pub fn snapshot_count(&self) -> usize {
        self.inner.lock().expect("mock poisoned").snapshots.len()
    }
}

impl Default for MockStorageBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl StorageBackend for MockStorageBackend {
    async fn create_pool(&self, id: &PoolId, vdevs: Vec<VdevSpec>) -> Result<Pool, StorageError> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        let key = id.as_str().to_string();
        if st.pools.contains_key(&key) {
            return Err(StorageError::PoolExists(key));
        }
        // 校验 vdev：至少一个 vdev、Disk 至少 1 盘、Mirror 至少 2 盘、RaidzN 至少 N+1 盘
        if vdevs.is_empty() {
            return Err(StorageError::InvalidVdev("无 vdev".into()));
        }
        let mut vdev_instances = Vec::new();
        for spec in &vdevs {
            let min = match spec.kind {
                VdevKind::Disk => 1,
                VdevKind::Mirror => 2,
                VdevKind::Raidz1 => 3,
                VdevKind::Raidz2 => 4,
                VdevKind::Raidz3 => 5,
            };
            if spec.disks.len() < min {
                return Err(StorageError::InvalidVdev(format!(
                    "{:?} 需至少 {min} 盘，实际 {}",
                    spec.kind,
                    spec.disks.len()
                )));
            }
            vdev_instances.push(Vdev {
                kind: spec.kind.clone(),
                disks: spec.disks.clone(),
                health: os_core::Health::Healthy,
                read_errors: 0,
                write_errors: 0,
                cksum_errors: 0,
            });
        }
        let pool = Pool {
            id: id.clone(),
            name: key.clone(),
            vdevs: vdev_instances,
            capacity: os_core::Capacity {
                used_bytes: 0,
                total_bytes: 0,
            },
            health: os_core::Health::Healthy,
        };
        st.pools.insert(key, pool.clone());
        Ok(pool)
    }

    async fn destroy_pool(&self, id: &PoolId) -> Result<(), StorageError> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        let key = id.as_str().to_string();
        st.pools
            .remove(&key)
            .ok_or(StorageError::PoolNotFound(key))?;
        // 级联删除该池下的 dataset/snapshot
        st.datasets.retain(|_, ds| ds.pool.as_str() != id.as_str());
        st.snapshots
            .retain(|_, s| !s.dataset.as_str().starts_with(&format!("{}/", id)));
        Ok(())
    }

    async fn list_pools(&self) -> Result<Vec<Pool>, StorageError> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        Ok(st.pools.values().cloned().collect())
    }

    async fn create_dataset(
        &self,
        name: &DatasetId,
        options: DatasetOptions,
    ) -> Result<Dataset, StorageError> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        let key = name.as_str().to_string();
        if st.datasets.contains_key(&key) {
            return Err(StorageError::DatasetExists(key));
        }
        // 校验所属池存在
        let pool_name = key
            .split_once('/')
            .map(|(p, _)| p.to_string())
            .unwrap_or_else(|| key.clone());
        if !st.pools.contains_key(&pool_name) {
            return Err(StorageError::PoolNotFound(pool_name));
        }
        let ds = Dataset {
            id: name.clone(),
            pool: PoolId::new(pool_name),
            name: key.clone(),
            used_bytes: 0,
            avail_bytes: options.volsize.unwrap_or(0),
            mounted: options.volsize.is_none(), // 文件系统默认已挂载，zvol 未挂载
            encryption: if options.encryption.as_ref().is_some_and(|e| e.enabled) {
                crate::model::EncryptionState::Unlocked
            } else {
                crate::model::EncryptionState::Off
            },
        };
        // 应用 quota
        if let Some(q) = options.quota {
            st.quotas.insert(key.clone(), q);
        }
        st.datasets.insert(key, ds.clone());
        Ok(ds)
    }

    async fn destroy_dataset(&self, name: &DatasetId) -> Result<(), StorageError> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        let key = name.as_str().to_string();
        st.datasets
            .remove(&key)
            .ok_or(StorageError::DatasetNotFound(key))?;
        // 级联删除该数据集的快照
        st.snapshots
            .retain(|_, s| s.dataset.as_str() != name.as_str());
        st.quotas.remove(name.as_str());
        Ok(())
    }

    async fn list_datasets(&self, pool: Option<&PoolId>) -> Result<Vec<Dataset>, StorageError> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        let result: Vec<Dataset> = st
            .datasets
            .values()
            .filter(|ds| pool.map_or(true, |p| ds.pool == *p))
            .cloned()
            .collect();
        Ok(result)
    }

    async fn snapshot(&self, dataset: &DatasetId, name: &str) -> Result<Snapshot, StorageError> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        let ds_key = dataset.as_str().to_string();
        if !st.datasets.contains_key(&ds_key) {
            return Err(StorageError::DatasetNotFound(ds_key));
        }
        let full = format!("{ds_key}@{name}");
        if st.snapshots.contains_key(&full) {
            return Err(StorageError::CommandFailed(format!("快照已存在：{full}")));
        }
        let snap = Snapshot {
            id: SnapshotId::new(full.clone()),
            dataset: dataset.clone(),
            created: os_core::Utc::now(),
            used_bytes: 0,
        };
        st.snapshots.insert(full, snap.clone());
        Ok(snap)
    }

    async fn destroy_snapshot(&self, snapshot: &SnapshotId) -> Result<(), StorageError> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        let key = snapshot.as_str().to_string();
        st.snapshots
            .remove(&key)
            .ok_or(StorageError::SnapshotNotFound(key))?;
        Ok(())
    }

    async fn list_snapshots(
        &self,
        dataset: Option<&DatasetId>,
    ) -> Result<Vec<Snapshot>, StorageError> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        let result: Vec<Snapshot> = st
            .snapshots
            .values()
            .filter(|s| dataset.map_or(true, |d| s.dataset == *d))
            .cloned()
            .collect();
        Ok(result)
    }

    async fn set_quota(&self, dataset: &DatasetId, quota: Quota) -> Result<(), StorageError> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        let key = dataset.as_str().to_string();
        if !st.datasets.contains_key(&key) {
            return Err(StorageError::DatasetNotFound(key));
        }
        st.quotas.insert(key, quota);
        Ok(())
    }

    async fn get_quota(&self, dataset: &DatasetId) -> Result<Quota, StorageError> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        let key = dataset.as_str().to_string();
        if !st.datasets.contains_key(&key) {
            return Err(StorageError::DatasetNotFound(key));
        }
        Ok(st.quotas.get(&key).copied().unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(name: &str) -> Pool {
        Pool {
            id: PoolId::new(name),
            name: name.into(),
            vdevs: vec![Vdev {
                kind: VdevKind::Mirror,
                disks: vec!["/dev/sdb".into(), "/dev/sdc".into()],
                health: os_core::Health::Healthy,
                read_errors: 0,
                write_errors: 0,
                cksum_errors: 0,
            }],
            capacity: os_core::Capacity {
                used_bytes: 0,
                total_bytes: 1_000_000_000,
            },
            health: os_core::Health::Healthy,
        }
    }

    #[tokio::test]
    async fn create_pool_then_list_and_conflict() {
        let be = MockStorageBackend::new();
        let p = be
            .create_pool(
                &PoolId::new("tank"),
                vec![VdevSpec {
                    kind: VdevKind::Mirror,
                    disks: vec!["/dev/sdb".into(), "/dev/sdc".into()],
                }],
            )
            .await
            .unwrap();
        assert_eq!(p.id.as_str(), "tank");
        assert_eq!(be.pool_count(), 1);
        let pools = be.list_pools().await.unwrap();
        assert_eq!(pools.len(), 1);
        // 二次 create 报 PoolExists
        let err = be
            .create_pool(
                &PoolId::new("tank"),
                vec![VdevSpec {
                    kind: VdevKind::Mirror,
                    disks: vec!["/dev/sdd".into(), "/dev/sde".into()],
                }],
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::PoolExists(_)));
    }

    #[tokio::test]
    async fn invalid_vdev_rejected() {
        let be = MockStorageBackend::new();
        // Mirror 仅 1 盘
        let err = be
            .create_pool(
                &PoolId::new("tank"),
                vec![VdevSpec {
                    kind: VdevKind::Mirror,
                    disks: vec!["/dev/sdb".into()],
                }],
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::InvalidVdev(_)));
    }

    #[tokio::test]
    async fn dataset_lifecycle() {
        let be = MockStorageBackend::new().with_pool(pool("tank"));
        let ds = be
            .create_dataset(&DatasetId::new("tank/media"), DatasetOptions::default())
            .await
            .unwrap();
        assert_eq!(ds.pool.as_str(), "tank");
        assert_eq!(be.dataset_count(), 1);
        // 重复报 DatasetExists
        assert!(matches!(
            be.create_dataset(&DatasetId::new("tank/media"), DatasetOptions::default())
                .await
                .unwrap_err(),
            StorageError::DatasetExists(_)
        ));
        // 不存在的池
        assert!(matches!(
            be.create_dataset(&DatasetId::new("ghost/x"), DatasetOptions::default())
                .await
                .unwrap_err(),
            StorageError::PoolNotFound(_)
        ));
        // 销毁
        be.destroy_dataset(&DatasetId::new("tank/media"))
            .await
            .unwrap();
        assert_eq!(be.dataset_count(), 0);
        // 二次销毁报 NotFound
        assert!(matches!(
            be.destroy_dataset(&DatasetId::new("tank/media"))
                .await
                .unwrap_err(),
            StorageError::DatasetNotFound(_)
        ));
    }

    #[tokio::test]
    async fn snapshot_and_quota() {
        let be = MockStorageBackend::new()
            .with_pool(pool("tank"))
            .with_dataset(Dataset {
                id: DatasetId::new("tank/media"),
                pool: PoolId::new("tank"),
                name: "tank/media".into(),
                used_bytes: 0,
                avail_bytes: 0,
                mounted: true,
                encryption: crate::model::EncryptionState::Off,
            });
        be.snapshot(&DatasetId::new("tank/media"), "s1")
            .await
            .unwrap();
        assert_eq!(be.snapshot_count(), 1);
        let snaps = be
            .list_snapshots(Some(&DatasetId::new("tank/media")))
            .await
            .unwrap();
        assert_eq!(snaps.len(), 1);
        // 销毁数据集级联删快照
        be.destroy_dataset(&DatasetId::new("tank/media"))
            .await
            .unwrap();
        assert_eq!(be.snapshot_count(), 0);
    }

    #[tokio::test]
    async fn quota_get_set() {
        let be = MockStorageBackend::new()
            .with_pool(pool("tank"))
            .with_dataset(Dataset {
                id: DatasetId::new("tank/media"),
                pool: PoolId::new("tank"),
                name: "tank/media".into(),
                used_bytes: 0,
                avail_bytes: 0,
                mounted: true,
                encryption: crate::model::EncryptionState::Off,
            });
        // 缺省 quota 全 None
        let q = be.get_quota(&DatasetId::new("tank/media")).await.unwrap();
        assert_eq!(q.refquota, None);
        be.set_quota(
            &DatasetId::new("tank/media"),
            Quota {
                refquota: Some(1000),
                refreservation: Some(500),
            },
        )
        .await
        .unwrap();
        let q = be.get_quota(&DatasetId::new("tank/media")).await.unwrap();
        assert_eq!(q.refquota, Some(1000));
        assert_eq!(q.refreservation, Some(500));
    }

    #[tokio::test]
    async fn forced_error_injects() {
        let be = MockStorageBackend::new().with_error(StorageError::CommandFailed("boom".into()));
        let err = be.list_pools().await.unwrap_err();
        assert!(matches!(err, StorageError::CommandFailed(_)));
        // 一次性：再调正常
        assert!(be.list_pools().await.is_ok());
    }

    #[tokio::test]
    async fn list_datasets_scoped_to_pool() {
        let be = MockStorageBackend::new()
            .with_pool(pool("tank"))
            .with_pool(pool("backup"))
            .with_dataset(Dataset {
                id: DatasetId::new("tank/a"),
                pool: PoolId::new("tank"),
                name: "tank/a".into(),
                used_bytes: 0,
                avail_bytes: 0,
                mounted: true,
                encryption: crate::model::EncryptionState::Off,
            })
            .with_dataset(Dataset {
                id: DatasetId::new("backup/b"),
                pool: PoolId::new("backup"),
                name: "backup/b".into(),
                used_bytes: 0,
                avail_bytes: 0,
                mounted: true,
                encryption: crate::model::EncryptionState::Off,
            });
        let tank_ds = be.list_datasets(Some(&PoolId::new("tank"))).await.unwrap();
        assert_eq!(tank_ds.len(), 1);
        let all = be.list_datasets(None).await.unwrap();
        assert_eq!(all.len(), 2);
    }
}
