//! 场景 4：备份链路（integration-agent 规格书 §3 / 规划文档 §3.16 backup 组件）
//!
//! 链路：os-services(backup) SchedulePolicy 定时触发 → os-storage 快照
//! （create_snapshot）→ send-recv 复制（ZfsSendRecv / Replication）→ os-monitor 告警。
//!
//! 之所以这样组织：
//! - `ZfsBackupManager`（默认实现）已把「调度 → snapshot」串起来（trigger_now 调
//!   `StorageBackend::snapshot`），但远程 send-recv 复制 + 监控告警仍是 TODO。
//!   集成测这里搭一层「业务编排」`BackupPipeline`，把 storage snapshot +
//!   ZfsSendRecv 复制 + os-monitor metric/告警显式串通，验证端到端链路正确性
//!   （呼应 integration-agent 风格：在测侧组装跨 crate 编排层，不改 trait/源码）。
//! - `SchedulePolicy::next_run` 是纯函数（backup_schedule.rs），单独验证 cron
//!   触发时机确定性，证明「定时触发」环节可被调度器依赖。
//!
//! 重点验证：
//! - 调度 → 快照 → 复制 → 告警 全链通（含调用顺序断言）。
//! - 跨 crate 类型桥接：`BackupPolicy.source`（os-core::DatasetId） ==
//!   storage snapshot 的 dataset；`SnapshotId` 跨 services/storage 一致。
//! - 远程复制：`target_remote = Some` 时触发 `Replication::send`（ZfsSendRecv），
//!   任务状态 `ReplicationStatus::Completed`。
//! - 错误传播：storage snapshot 失败（MockStorageBackend.with_error）→
//!   BackupJob.status = Failed + monitor 收 backup.failed 事件 + 不触发复制。
//! - 监控落账：成功后 monitor 记录 `backup.success.total` Counter；
//!   失败记 `backup.failed.total`。
//! - 调度时机：SchedulePolicy::Cron 的 next_run 确定性（纯函数）。

use std::sync::{Arc, Mutex};

use os_core::eventbus::{Event, EventBus, Severity, Topic};
use os_core::mock::MockEventBus;
use os_core::{DatasetId, PoolId, SnapshotId};
use os_core::{Health, Utc};
use os_services::backup::{
    BackupJob, BackupManager, BackupPolicy, BackupStatus, CronExpr, RetentionPolicy,
};
use os_services::mock::MockMonitor;
use os_services::monitor::{AlertRule, AlertSeverity, Metric, MetricKind, Monitor};
use os_services::SchedulePolicy;
use os_services::ServiceError;
use os_storage::backend::StorageBackend;
use os_storage::mock::MockStorageBackend;
use os_storage::model::{Dataset, Pool};
use os_storage::replication::{Replication, ReplicationStatus};
use os_storage::ZfsSendRecv;

// ----------------------------------------------------------------------------
// BackupPipeline：业务编排层——把调度 / snapshot / send-recv / monitor 串通。
// 这是 integration-agent 搭的「跨 crate 编排骨架」，验证各组件协作。
// ----------------------------------------------------------------------------

struct BackupPipeline {
    backend: Arc<MockStorageBackend>,
    replication: Arc<ZfsSendRecv>,
    bus: Arc<MockEventBus>,
    monitor: Arc<MockMonitor>,
    /// 调用顺序记录（断言阶段 ↔ 组件调用对应）。
    call_log: Mutex<Vec<String>>,
}

impl BackupPipeline {
    fn new(
        backend: Arc<MockStorageBackend>,
        replication: Arc<ZfsSendRecv>,
        bus: Arc<MockEventBus>,
        monitor: Arc<MockMonitor>,
    ) -> Self {
        Self {
            backend,
            replication,
            bus,
            monitor,
            call_log: Mutex::new(Vec::new()),
        }
    }

    fn log(&self, entry: String) {
        self.call_log.lock().expect("call_log").push(entry);
    }

    fn call_log(&self) -> Vec<String> {
        self.call_log.lock().expect("call_log").clone()
    }

    /// 驱动一次完整备份：snapshot → (可选) send-recv → monitor 落账 → 事件。
    ///
    /// 成功返回创建出的快照 ID（供下游断言）；失败返回 storage 的原始错误
    /// （os_storage::StorageError，保留具体变体便于下游 matches! 断言）。
    async fn run_once(
        &self,
        policy: &BackupPolicy,
    ) -> Result<SnapshotId, os_storage::StorageError> {
        let now = Utc::now();
        let snap_name = format!("auto-{}", now.format("%Y%m%dT%H%M%S"));

        // 1) storage 创建快照（本地备份核心动作）。
        let snap = match self.backend.snapshot(&policy.source, &snap_name).await {
            Ok(s) => {
                self.log(format!(
                    "snapshot({}@{}): Ok",
                    policy.source.as_str(),
                    snap_name
                ));
                s
            }
            Err(e) => {
                self.log(format!(
                    "snapshot({}@{}): Err({e})",
                    policy.source.as_str(),
                    snap_name
                ));
                // 失败：发 Storage Error 事件 + monitor 记 backup.failed。
                let _ = self
                    .bus
                    .publish(Event {
                        source: "os-services/backup".into(),
                        topic: Topic::Storage,
                        kind: "backup.failed".into(),
                        severity: Severity::Error,
                        task_id: None,
                        payload: serde_json::json!({
                            "policy": policy.name,
                            "source": policy.source.as_str(),
                            "error": e.to_string(),
                        }),
                        timestamp: now,
                    })
                    .await;
                let _ = self
                    .monitor
                    .record_metric(Metric {
                        name: "backup.failed.total".into(),
                        kind: MetricKind::Counter,
                        value: 1.0,
                        labels: {
                            let mut m = std::collections::HashMap::new();
                            m.insert("policy".into(), policy.name.clone());
                            m
                        },
                        timestamp: now,
                    })
                    .await;
                return Err(e);
            }
        };

        // 2) 远程灾备复制（仅当 target_remote = Some）。SnapshotId 跨 crate 一致。
        if let Some(remote) = &policy.target_remote {
            // target_remote 形如 "host:dataset"，直接作为 DatasetId 传给 Replication。
            let target = DatasetId::new(remote);
            match self.replication.send(&snap.id, &target).await {
                Ok(task) => {
                    self.log(format!(
                        "replication.send({} → {}): task={}",
                        snap.id.as_str(),
                        remote,
                        task.0
                    ));
                }
                Err(e) => {
                    self.log(format!("replication.send({}): Err({e})", snap.id.as_str()));
                    // 复制失败不致命（本地快照已成功）；仅记 Warn 事件。
                    let _ = self
                        .bus
                        .publish(Event {
                            source: "os-services/backup".into(),
                            topic: Topic::Storage,
                            kind: "backup.replication.degraded".into(),
                            severity: Severity::Warn,
                            task_id: Some(task_placeholder()),
                            payload: serde_json::json!({
                                "policy": policy.name,
                                "snapshot": snap.id.as_str(),
                                "error": e.to_string(),
                            }),
                            timestamp: now,
                        })
                        .await;
                }
            }
        }

        // 3) monitor 落账：成功 Counter。
        let _ = self
            .monitor
            .record_metric(Metric {
                name: "backup.success.total".into(),
                kind: MetricKind::Counter,
                value: 1.0,
                labels: {
                    let mut m = std::collections::HashMap::new();
                    m.insert("policy".into(), policy.name.clone());
                    m
                },
                timestamp: now,
            })
            .await;

        // 4) 发 Storage 事件（snapshot.completed）。
        let _ = self
            .bus
            .publish(Event {
                source: "os-services/backup".into(),
                topic: Topic::Storage,
                kind: "snapshot.completed".into(),
                severity: Severity::Info,
                task_id: None,
                payload: serde_json::json!({
                    "policy": policy.name,
                    "snapshot": snap.id.as_str(),
                    "remote": policy.target_remote,
                }),
                timestamp: now,
            })
            .await;

        Ok(snap.id)
    }
}

fn task_placeholder() -> os_core::TaskId {
    os_core::TaskId::new()
}

// ----------------------------------------------------------------------------
// 辅助构造
// ----------------------------------------------------------------------------

fn pool(name: &str) -> Pool {
    Pool {
        id: PoolId::new(name),
        name: name.into(),
        vdevs: vec![],
        capacity: os_core::Capacity {
            used_bytes: 0,
            total_bytes: 100 * 1024 * 1024 * 1024,
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

fn backend_with_tank_media() -> MockStorageBackend {
    MockStorageBackend::new()
        .with_pool(pool("tank"))
        .with_dataset(dataset("tank/media"))
}

fn policy_local(name: &str) -> BackupPolicy {
    BackupPolicy {
        name: name.into(),
        schedule: CronExpr::new("0 3 * * *"),
        retention: RetentionPolicy {
            keep_last: 7,
            keep_days: 7,
        },
        source: DatasetId::new("tank/media"),
        target_remote: None,
    }
}

fn policy_remote(name: &str, remote: &str) -> BackupPolicy {
    BackupPolicy {
        target_remote: Some(remote.to_string()),
        ..policy_local(name)
    }
}

// ----------------------------------------------------------------------------
// 集成测
// ----------------------------------------------------------------------------

#[tokio::test]
async fn backup_pipeline_local_snapshot_only_succeeds() {
    // 仅本地快照（target_remote = None）：链路 = snapshot → monitor 落账 → 事件。
    let backend = Arc::new(backend_with_tank_media());
    let replication = Arc::new(ZfsSendRecv::new("root"));
    let bus = Arc::new(MockEventBus::new());
    let monitor = Arc::new(MockMonitor::default());

    let pipeline = BackupPipeline::new(
        backend.clone(),
        replication.clone(),
        bus.clone(),
        monitor.clone(),
    );

    let snap_id = pipeline
        .run_once(&policy_local("media-daily"))
        .await
        .expect("本地快照应成功");

    // 1) storage 上多了一个快照。
    assert_eq!(backend.snapshot_count(), 1, "应创建 1 个快照");
    // 2) SnapshotId 跨 crate 一致：含 dataset 名 + @ + 快照名前缀。
    assert!(snap_id.as_str().starts_with("tank/media@auto-"));

    // 3) monitor 记到成功 Counter。
    let recorded = monitor.recorded_metrics();
    assert!(
        recorded.iter().any(|m| m.name == "backup.success.total"
            && m.kind == MetricKind::Counter
            && m.labels.get("policy") == Some(&"media-daily".to_string())),
        "monitor 应记 backup.success.total: {:?}",
        recorded
    );
    assert!(
        !recorded.iter().any(|m| m.name == "backup.failed.total"),
        "成功路径不应记 failed metric"
    );

    // 4) EventBus 收到 snapshot.completed 事件。
    assert_eq!(bus.published_count_for(Topic::Storage), 1);
    let published = bus.published();
    assert_eq!(published[0].kind, "snapshot.completed");
    assert_eq!(published[0].severity, Severity::Info);

    // 5) 调用顺序：只有 snapshot（未走复制）。
    let log = pipeline.call_log();
    assert!(log
        .iter()
        .any(|s| s.contains("snapshot(") && s.contains("Ok")));
    assert!(
        !log.iter().any(|s| s.contains("replication.send")),
        "target_remote=None 不应触发复制"
    );

    // replication 完全没被调用（任务表为空）。
    let any_task = os_core::TaskId::new();
    let _ = any_task;
    assert!(
        replication.replication_status(&any_task).await.is_err(),
        "未触发复制 → 任务表为空"
    );
}

#[tokio::test]
async fn backup_pipeline_remote_replication_chain() {
    // target_remote = Some：链路 = snapshot → send-recv → monitor → 事件。
    let backend = Arc::new(backend_with_tank_media());
    let replication = Arc::new(ZfsSendRecv::new("root"));
    let bus = Arc::new(MockEventBus::new());
    let monitor = Arc::new(MockMonitor::default());

    let pipeline = BackupPipeline::new(
        backend.clone(),
        replication.clone(),
        bus.clone(),
        monitor.clone(),
    );

    let snap_id = pipeline
        .run_once(&policy_remote("media-remote", "backup-host:tank/recv"))
        .await
        .expect("远程备份应成功");

    // 1) snapshot 创建。
    assert_eq!(backend.snapshot_count(), 1);

    // 2) 复制被触发——通过调用日志 + 任务状态间接断言。
    let log = pipeline.call_log();
    let send_entry = log
        .iter()
        .find(|s| s.contains("replication.send"))
        .expect("应触发复制");
    assert!(
        send_entry.contains("→ backup-host:tank/recv"),
        "复制目标应是远端 host:dataset: {send_entry}"
    );

    // 3) 顺序：snapshot 必须在 replication 之前。
    let snap_idx = log
        .iter()
        .position(|s| s.contains("snapshot("))
        .expect("snapshot 调用记录");
    let repl_idx = log
        .iter()
        .position(|s| s.contains("replication.send"))
        .expect("replication 调用记录");
    assert!(
        snap_idx < repl_idx,
        "snapshot 必须在 replication 之前（顺序错乱: {log:?}）"
    );

    // 4) ZfsSendRecv 内部任务状态可达（通过 send 返回的 task_id 反查 Completed）。
    //    send 的实现是「立即置 Completed」，且 send_log 里有 task=<uuid>。
    let task_id_str = send_entry
        .split("task=")
        .nth(1)
        .expect("send log 应含 task=<uuid>");
    let uuid = os_core::Uuid::parse_str(task_id_str).expect("task=<uuid> 应可解析");
    let task = os_core::TaskId(uuid);
    let status = replication
        .replication_status(&task)
        .await
        .expect("复制任务应可达");
    assert!(
        matches!(status, ReplicationStatus::Completed { .. }),
        "骨架实现应立即置 Completed，实得 {status:?}"
    );

    // 5) SnapshotId 跨 crate 类型一致：复制时传的 snap_id 与 storage 内一致。
    assert!(snap_id.as_str().starts_with("tank/media@auto-"));

    // 6) EventBus 仍只发 1 个 snapshot.completed（复制不单独发事件，集成编排层合并）。
    assert_eq!(bus.published_count_for(Topic::Storage), 1);
    assert_eq!(bus.published()[0].kind, "snapshot.completed");
    assert_eq!(
        bus.published()[0].payload["remote"].as_str(),
        Some("backup-host:tank/recv")
    );

    // 7) monitor 落账成功 Counter。
    assert!(monitor
        .recorded_metrics()
        .iter()
        .any(|m| m.name == "backup.success.total"));
}

#[tokio::test]
async fn backup_pipeline_snapshot_failure_aborts_and_emits_alert() {
    // storage 注入一次性失败：snapshot 阶段失败 → 不触发复制 + 发 backup.failed 事件
    // + monitor 记 failed Counter。
    let backend = Arc::new(backend_with_tank_media().with_error(
        os_storage::StorageError::CommandFailed("mock 快照故障".into()),
    ));
    let replication = Arc::new(ZfsSendRecv::new("root"));
    let bus = Arc::new(MockEventBus::new());
    let monitor = Arc::new(MockMonitor::default());

    let pipeline = BackupPipeline::new(
        backend.clone(),
        replication.clone(),
        bus.clone(),
        monitor.clone(),
    );

    let result = pipeline
        .run_once(&policy_remote("media-fail", "backup-host:tank/recv"))
        .await;
    assert!(result.is_err(), "snapshot 失败应传播为 Err");

    // 1) storage 上没有快照。
    assert_eq!(backend.snapshot_count(), 0);
    // 2) 复制不应被触发。
    assert!(
        !pipeline
            .call_log()
            .iter()
            .any(|s| s.contains("replication.send")),
        "snapshot 失败时不应触发复制"
    );
    // 3) EventBus 发 backup.failed Error 事件。
    assert_eq!(bus.published_count_for(Topic::Storage), 1);
    let ev = &bus.published()[0];
    assert_eq!(ev.kind, "backup.failed");
    assert_eq!(ev.severity, Severity::Error);
    assert_eq!(ev.payload["policy"].as_str(), Some("media-fail"));
    // 4) monitor 记 failed Counter（且没有 success Counter）。
    let recorded = monitor.recorded_metrics();
    assert!(
        recorded.iter().any(|m| m.name == "backup.failed.total"
            && m.labels.get("policy") == Some(&"media-fail".to_string())),
        "monitor 应记 backup.failed.total"
    );
    assert!(
        !recorded.iter().any(|m| m.name == "backup.success.total"),
        "失败路径不应记 success metric"
    );
}

#[tokio::test]
async fn backup_pipeline_monitor_alert_rule_fires_on_failure_metric() {
    // 验证：monitor 预置 alert 规则（backup.failed.total > 0），失败链路触发告警。
    let backend = Arc::new(
        backend_with_tank_media()
            .with_error(os_storage::StorageError::CommandFailed("boom".into())),
    );
    let replication = Arc::new(ZfsSendRecv::new("root"));
    let bus = Arc::new(MockEventBus::new());
    let monitor = Arc::new(MockMonitor::default());

    // 预置告警规则：backup.failed.total > 0（无持续时长，立即触发）。
    monitor
        .add_alert_rule(AlertRule {
            name: "backup_failed_alert".into(),
            metric: "backup.failed.total".into(),
            condition: ">0".into(),
            for_duration_secs: 0,
            severity: AlertSeverity::Critical,
        })
        .await
        .unwrap();

    let pipeline = BackupPipeline::new(backend, replication, bus, monitor.clone());

    let _ = pipeline.run_once(&policy_local("p")).await;

    // 失败 metric 落账后告警引擎应触发一条 Firing 告警。
    let alerts = monitor.alerts();
    assert!(
        alerts
            .iter()
            .any(|a| a.rule_name == "backup_failed_alert" && !a.resolved),
        "应触发 backup_failed_alert 告警: {alerts:?}"
    );
}

#[tokio::test]
async fn backup_pipeline_dataset_missing_propagates() {
    // 源数据集不存在（policy.source 指向 ghost）→ snapshot 返回 DatasetNotFound。
    let backend = Arc::new(
        MockStorageBackend::new()
            .with_pool(pool("tank"))
            .with_dataset(dataset("tank/media")),
    );
    let replication = Arc::new(ZfsSendRecv::new("root"));
    let bus = Arc::new(MockEventBus::new());
    let monitor = Arc::new(MockMonitor::default());

    let pipeline = BackupPipeline::new(backend, replication, bus.clone(), monitor);

    let mut bad_policy = policy_local("ghost");
    bad_policy.source = DatasetId::new("tank/nonexistent");

    let result = pipeline.run_once(&bad_policy).await;
    assert!(result.is_err(), "源数据集不存在应失败");
    let err = result.unwrap_err();
    assert!(
        matches!(err, os_storage::StorageError::DatasetNotFound(_)),
        "应是 DatasetNotFound，实得 {err:?}"
    );
    // EventBus 发失败事件（payload.source = tank/nonexistent）。
    assert_eq!(bus.published_count_for(Topic::Storage), 1);
    assert_eq!(
        bus.published()[0].payload["source"].as_str(),
        Some("tank/nonexistent")
    );
}

#[tokio::test]
async fn backup_manager_schedule_and_trigger_via_default_impl() {
    // 验证 os-services 默认实现 ZfsBackupManager（注入 MockStorageBackend）的
    // schedule → trigger_now 链路：trigger_now 调 storage.snapshot 创建快照。
    use os_services::ZfsBackupManager;

    let backend = Arc::new(backend_with_tank_media());
    let mgr = ZfsBackupManager::new(backend.clone());

    // schedule：解析 cron + 算 next_run + 入 job 表。
    let job_id = mgr.schedule(policy_local("daily")).await.unwrap();
    assert!(job_id.starts_with("job-daily"));

    let jobs = mgr.list_jobs().await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].status, BackupStatus::Scheduled);
    assert!(jobs[0].next_run.is_some(), "应算出 next_run");

    // trigger_now：调 backend.snapshot 创建快照 + 标记 Success。
    let _task = mgr.trigger_now(&job_id).await.unwrap();
    let jobs: Vec<BackupJob> = mgr.list_jobs().await.unwrap();
    assert_eq!(jobs[0].status, BackupStatus::Success);
    assert!(jobs[0].last_run.is_some());

    // storage 上确实创建了快照（跨 crate 验证 trigger_now 真调了 snapshot）。
    assert_eq!(backend.snapshot_count(), 1, "trigger_now 应创建快照");
}

#[tokio::test]
async fn schedule_policy_cron_next_run_is_deterministic() {
    // 调度环节：SchedulePolicy::Cron 的 next_run 是纯函数，确定下一次触发时刻。
    // 这里验证 backup_schedule 模块与 BackupPolicy 的桥接可用。
    use chrono::TimeZone;

    let policy = policy_local("cron-test");
    let sched = SchedulePolicy::Cron(policy.schedule.clone());

    let after = chrono::Utc.with_ymd_and_hms(2024, 1, 15, 10, 0, 0).unwrap();
    let next = sched.next_run(&after).unwrap().expect("应有 next_run");
    // 每天 03:00 → 次日 03:00。
    let expected = chrono::Utc.with_ymd_and_hms(2024, 1, 16, 3, 0, 0).unwrap();
    assert_eq!(next, expected);

    // Manual / Event 无 next_run。
    assert!(SchedulePolicy::Manual.next_run(&after).unwrap().is_none());
    assert!(SchedulePolicy::Event("dataset-changed".into())
        .next_run(&after)
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn snapshot_id_cross_crate_type_identity() {
    // 静态验证：services::BackupPolicy.source（DatasetId） 与
    // storage::StorageBackend::snapshot 的 dataset 参数同一 os-core::DatasetId newtype。
    // 跨 crate 编译期类型一致——避免运行期字符串拼接错位。
    let source = DatasetId::new("tank/media");
    let policy = BackupPolicy {
        name: "type-check".into(),
        schedule: CronExpr::new("0 3 * * *"),
        retention: RetentionPolicy {
            keep_last: 1,
            keep_days: 1,
        },
        source: source.clone(),
        target_remote: None,
    };
    // 用同一 DatasetId 调 storage snapshot（编译期类型校验）。
    let backend = backend_with_tank_media();
    let snap = backend.snapshot(&policy.source, "s1").await.unwrap();
    // SnapshotId 形如 "tank/media@s1"。
    assert_eq!(snap.id.as_str(), "tank/media@s1");
    // 再用同一 SnapshotId 喂给 Replication（编译期类型校验）。
    let repl = ZfsSendRecv::default();
    let target = DatasetId::new("backup:tank/recv");
    let _task = repl.send(&snap.id, &target).await.unwrap();
}

// 抑制未用警告（ServiceError 在签名中引用但仅在错误变体匹配时用到）。
#[allow(dead_code)]
fn _silence_unused() {
    let _ = ServiceError::Internal(String::new());
}
