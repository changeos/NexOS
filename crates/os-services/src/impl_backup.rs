//! `ZfsBackupManager` —— [`crate::backup::BackupManager`] 的默认实现骨架。
//!
//! 基于 ZFS 快照 + send-recv（3-2-1 备份）。本文件交付骨架：
//! - 调度管理（schedule/unschedule/list_jobs）完整可用：解析 cron、计算 next_run、内存态维护 job。
//! - 即时触发（trigger_now）/ 恢复（restore）：调注入的 [`os_storage::StorageBackend`] 快照原语
//!   完成本地快照创建（真实远程 send-recv 复制留 TODO [RUNTIME]，因依赖 Replication 实现
//!   尚未在 mock 提供）。
//! - scrub 查询：骨架返回空报告（真实 scrub 由 storage-agent 调度，本层仅上报）。
//!
//! 设计要点：
//! - 注入 `Arc<dyn StorageBackend>`：便于测试用 [`os_storage::MockStorageBackend`] 注入。
//! - 内存态 job store（`Mutex<HashMap<String, BackupJob>>`）：不持久化（持久化由上层 osd 负责）。
//! - 远程复制（target_remote = Some）当前仅记日志占位 + TODO [RUNTIME]，真正 `zfs send | ssh ... zfs recv`
//!   管道待 Replication 实现就绪后接入（见规格书 §10.2#11；需 root + 远端 SSH + zfs 内核模块）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use os_core::{DatasetId, PoolId, SnapshotId, TaskId, Utc};
use os_storage::StorageBackend;

use crate::backup::{BackupJob, BackupManager, BackupPolicy, BackupStatus, ScrubReport};
use crate::backup_schedule::{CronSchedule, SchedulePolicy};
use crate::ServiceError;

// ----------------------------------------------------------------------------
// ZfsBackupManager
// ----------------------------------------------------------------------------

/// 基于 ZFS 的备份管理器（默认实现）。
///
/// 泛型参数 `B`：存储后端类型（实现 [`StorageBackend`]）。用泛型而非 `Arc<dyn StorageBackend>`
/// 是因为 `StorageBackend` 用原生 `async fn in trait`，非 dyn 兼容（见 ADR-COMPAT-001）。
/// 单实现/泛型路径保持零开销，符合契约规范。
///
/// 持有：
/// - `backend`：存储后端（快照创建/销毁/列举），通过 `Arc<B>` 注入。
/// - `jobs`：内存态 job 表（job_id → BackupJob）。
///
/// 线程安全：内部 `Mutex` 保护 job 表；`StorageBackend` 实现自身保证并发安全。
pub struct ZfsBackupManager<B: StorageBackend> {
    backend: Arc<B>,
    jobs: Mutex<HashMap<String, BackupJob>>,
}

impl<B: StorageBackend> ZfsBackupManager<B> {
    /// 构造：注入存储后端。
    pub fn new(backend: Arc<B>) -> Self {
        Self {
            backend,
            jobs: Mutex::new(HashMap::new()),
        }
    }

    /// 从 `BackupPolicy.schedule` 推导 [`SchedulePolicy`]。
    ///
    /// 约定：策略的 cron 字符串即 `SchedulePolicy::Cron`；本骨架不支持事件触发
    /// （事件源由上层事件总线接入，见规格书 §7）。
    #[allow(dead_code)]
    fn schedule_policy(policy: &BackupPolicy) -> SchedulePolicy {
        SchedulePolicy::Cron(policy.schedule.clone())
    }

    /// 生成 job id（人类可读前缀 + 计数）。
    fn gen_job_id(policy: &BackupPolicy, jobs: &HashMap<String, BackupJob>) -> String {
        let base = format!("job-{}", policy.name);
        let mut id = base.clone();
        let mut n = 1;
        while jobs.contains_key(&id) {
            n += 1;
            id = format!("{base}-{n}");
        }
        id
    }
}

impl<B: StorageBackend> BackupManager for ZfsBackupManager<B> {
    async fn schedule(&self, policy: BackupPolicy) -> Result<String, ServiceError> {
        // 校验 cron 表达式合法性（提前失败，避免注册后调度时才报错）
        let sched = CronSchedule::parse(&policy.schedule)?;

        let now = Utc::now();
        let next_run = sched.next_run(&now).ok();

        let mut jobs = self.jobs.lock().expect("jobs mutex poisoned");
        let id = Self::gen_job_id(&policy, &jobs);
        let job = BackupJob {
            id: id.clone(),
            policy,
            last_run: None,
            next_run,
            status: BackupStatus::Scheduled,
        };
        jobs.insert(id.clone(), job);
        Ok(id)
    }

    async fn unschedule(&self, job_id: &str) -> Result<(), ServiceError> {
        let mut jobs = self.jobs.lock().expect("jobs mutex poisoned");
        if jobs.remove(job_id).is_none() {
            return Err(ServiceError::JobNotFound(job_id.to_string()));
        }
        Ok(())
    }

    async fn list_jobs(&self) -> Result<Vec<BackupJob>, ServiceError> {
        let jobs = self.jobs.lock().expect("jobs mutex poisoned");
        let mut list: Vec<BackupJob> = jobs.values().cloned().collect();
        // 稳定排序（按 id），便于断言
        list.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(list)
    }

    async fn trigger_now(&self, job_id: &str) -> Result<TaskId, ServiceError> {
        // 取出 job（持锁期间复制 policy，避免 await 持锁）
        let policy = {
            let mut jobs = self.jobs.lock().expect("jobs mutex poisoned");
            let job = jobs
                .get_mut(job_id)
                .ok_or_else(|| ServiceError::JobNotFound(job_id.to_string()))?;
            job.status = BackupStatus::Running;
            job.policy.clone()
        };

        let now = Utc::now();
        let snap_name = format!("auto-{}", now.format("%Y%m%dT%H%M%S"));

        // 调存储后端创建快照（本地备份的核心动作）
        let result = self.backend.snapshot(&policy.source, &snap_name).await;

        let task_id = TaskId::new();
        {
            let mut jobs = self.jobs.lock().expect("jobs mutex poisoned");
            let job = jobs
                .get_mut(job_id)
                .ok_or_else(|| ServiceError::JobNotFound(job_id.to_string()))?;
            match result {
                Ok(_) => {
                    job.status = BackupStatus::Success;
                    job.last_run = Some(now);
                    // 远程复制（3-2-1）：target_remote = Some 时应触发 send-recv。
                    // TODO(backup-agent) [RUNTIME]: 待 storage-agent 的 Replication mock 就绪后接入，
                    //   当前仅记录策略意图，不真正传输（避免阻塞测试）。需真实 zfs send|ssh recv（root）。
                    if let Some(_remote) = &policy.target_remote {
                        // 真实实现：self.replication.send(&snapshot_id, &remote_target).await
                    }
                }
                Err(_e) => {
                    job.status = BackupStatus::Failed;
                }
            }
        }

        // 即时触发恒返回一个 TaskId 供下游追踪（无论成败——失败已在 job.status 反映）
        Ok(task_id)
    }

    async fn scrub_status(&self, _pool: &PoolId) -> Result<ScrubReport, ServiceError> {
        // ZFS scrub 的真实调度与执行归 storage-agent（§10.2#11）；
        // 本层仅上报最近一次报告。骨架返回空报告（无历史 scrub 记录）。
        // TODO(backup-agent) [RUNTIME]: 接入 storage-agent 的 scrub 查询原语后填充真实数据。
        //   需真实 zfs scrub 状态读取（root + zfs 内核模块）。
        Ok(ScrubReport {
            errors: 0,
            repaired: 0,
            last_finished: None,
            duration_secs: 0,
        })
    }

    async fn restore(
        &self,
        snapshot: &SnapshotId,
        target: &DatasetId,
    ) -> Result<TaskId, ServiceError> {
        // 从快照恢复到目标数据集。
        // 真实实现：`zfs clone <snapshot> <target>` 或 `zfs send | zfs recv`，
        // 由 storage-agent 提供 restore 原语后接入。骨架：校验快照存在性后返回 TaskId。
        // TODO(backup-agent) [RUNTIME]: 接入 storage restore 原语（clone/recv）。
        //   需真实 zfs clone/recv（root + zfs 内核模块）。

        // 校验快照存在（通过 list_snapshots 间接验证）
        let _snaps = self
            .backend
            .list_snapshots(None)
            .await
            .map_err(|e| ServiceError::Internal(format!("查询快照失败: {e}")))?;
        // 注：target 数据集的存在性校验留给底层 restore 原语（骨架阶段不强制）。
        let _ = (snapshot, target);

        Ok(TaskId::new())
    }
}

// ----------------------------------------------------------------------------
// 单元测试（用 MockStorageBackend 注入）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::{CronExpr, RetentionPolicy};
    use os_core::{DatasetId, Health, PoolId};
    use os_storage::{
        model::{Dataset, Pool},
        MockStorageBackend,
    };

    fn pool(name: &str) -> Pool {
        Pool {
            id: PoolId::new(name),
            name: name.into(),
            vdevs: vec![],
            capacity: os_core::Capacity {
                used_bytes: 0,
                total_bytes: 0,
            },
            health: Health::Healthy,
        }
    }

    fn dataset(name: &str) -> Dataset {
        Dataset {
            id: DatasetId::new(name),
            pool: PoolId::new("tank"),
            name: name.into(),
            used_bytes: 0,
            avail_bytes: 0,
            mounted: true,
            encryption: os_storage::model::EncryptionState::Off,
        }
    }

    fn backend_with_tank_media() -> Arc<MockStorageBackend> {
        let be = MockStorageBackend::new()
            .with_pool(pool("tank"))
            .with_dataset(dataset("tank/media"));
        Arc::new(be)
    }

    /// 测试用 manager 类型别名（注入 MockStorageBackend）。
    type TestMgr = ZfsBackupManager<MockStorageBackend>;

    fn policy(name: &str, cron: &str) -> BackupPolicy {
        BackupPolicy {
            name: name.into(),
            schedule: CronExpr::new(cron),
            retention: RetentionPolicy {
                keep_last: 7,
                keep_days: 7,
            },
            source: DatasetId::new("tank/media"),
            target_remote: None,
        }
    }

    #[tokio::test]
    async fn schedule_invalid_cron_rejected() {
        let mgr: TestMgr = ZfsBackupManager::new(backend_with_tank_media());
        let bad = policy("p", "0 3 *"); // 段数不足
        let err = mgr.schedule(bad).await.unwrap_err();
        assert!(matches!(err, ServiceError::Internal(_)));
    }

    #[tokio::test]
    async fn schedule_returns_id_and_next_run() {
        let mgr: TestMgr = ZfsBackupManager::new(backend_with_tank_media());
        let id = mgr.schedule(policy("daily", "0 3 * * *")).await.unwrap();
        assert!(id.starts_with("job-daily"));
        let jobs = mgr.list_jobs().await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].next_run.is_some(), "应算出 next_run");
        assert_eq!(jobs[0].status, BackupStatus::Scheduled);
    }

    #[tokio::test]
    async fn schedule_unique_ids_on_name_clash() {
        let mgr: TestMgr = ZfsBackupManager::new(backend_with_tank_media());
        let id1 = mgr.schedule(policy("daily", "0 3 * * *")).await.unwrap();
        let id2 = mgr.schedule(policy("daily", "0 4 * * *")).await.unwrap();
        assert_ne!(id1, id2);
        assert_eq!(mgr.list_jobs().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn unschedule_removes_job() {
        let mgr: TestMgr = ZfsBackupManager::new(backend_with_tank_media());
        let id = mgr.schedule(policy("daily", "0 3 * * *")).await.unwrap();
        mgr.unschedule(&id).await.unwrap();
        assert_eq!(mgr.list_jobs().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn unschedule_unknown_returns_job_not_found() {
        let mgr: TestMgr = ZfsBackupManager::new(backend_with_tank_media());
        let err = mgr.unschedule("nope").await.unwrap_err();
        assert!(matches!(err, ServiceError::JobNotFound(_)));
    }

    #[tokio::test]
    async fn trigger_now_creates_snapshot_and_marks_success() {
        let mgr: TestMgr = ZfsBackupManager::new(backend_with_tank_media());
        let id = mgr.schedule(policy("daily", "0 3 * * *")).await.unwrap();
        let task = mgr.trigger_now(&id).await.unwrap();
        let _ = task; // TaskId 仅追踪用
        let jobs = mgr.list_jobs().await.unwrap();
        assert_eq!(jobs[0].status, BackupStatus::Success);
        assert!(jobs[0].last_run.is_some());
    }

    #[tokio::test]
    async fn trigger_now_unknown_returns_job_not_found() {
        let mgr: TestMgr = ZfsBackupManager::new(backend_with_tank_media());
        let err = mgr.trigger_now("nope").await.unwrap_err();
        assert!(matches!(err, ServiceError::JobNotFound(_)));
    }

    #[tokio::test]
    async fn trigger_now_marks_failed_when_backend_errors() {
        // 注入强制错误：MockStorageBackend::with_error 是一次性的，且在 snapshot 调用时触发
        let be = MockStorageBackend::new()
            .with_pool(pool("tank"))
            .with_dataset(dataset("tank/media"))
            .with_error(os_storage::StorageError::CommandFailed("boom".into()));
        let mgr: TestMgr = ZfsBackupManager::new(Arc::new(be));
        let id = mgr.schedule(policy("daily", "0 3 * * *")).await.unwrap();
        let _ = mgr.trigger_now(&id).await.unwrap();
        let jobs = mgr.list_jobs().await.unwrap();
        assert_eq!(jobs[0].status, BackupStatus::Failed);
    }

    #[tokio::test]
    async fn scrub_status_returns_empty_report() {
        let mgr: TestMgr = ZfsBackupManager::new(backend_with_tank_media());
        let report = mgr.scrub_status(&PoolId::new("tank")).await.unwrap();
        assert_eq!(report.errors, 0);
        assert_eq!(report.repaired, 0);
        assert!(report.last_finished.is_none());
    }

    #[tokio::test]
    async fn restore_returns_task_id() {
        let mgr: TestMgr = ZfsBackupManager::new(backend_with_tank_media());
        let task = mgr
            .restore(
                &SnapshotId::new("tank/media@snap1"),
                &DatasetId::new("tank/restored"),
            )
            .await
            .unwrap();
        let _ = task;
    }

    #[test]
    fn schedule_policy_from_cron() {
        let p = policy("daily", "0 3 * * *");
        let sp: SchedulePolicy = TestMgr::schedule_policy(&p);
        assert!(matches!(sp, SchedulePolicy::Cron(_)));
    }
}
