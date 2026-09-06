//! os-services Mock 实现（仅 `mock` feature 下编译）。
//!
//! **共享 crate 边界**：本文件被多个 service-agent 共享（backup/monitor/media/files/devtools/power）。
//! 每个 agent **只追加自己 owner 的 Mock**，不删/改其他 agent 的实现。
//! 文件内用 agent 分区注释隔离，避免合并冲突。
//!
//! 整合策略（orchestrator）：
//! - `MockPowerManager`：本文件内定义（power-agent）。
//! - `MockBackupManager`：本文件内定义（backup-agent）。
//! - `MockDevTools`：本文件内定义（devtools-agent）。
//! - `MockMediaManager`：本文件内定义（media-agent）。
//! - `MockFileManager`：定义在 `crate::impl_files`（files-agent），这里 re-export。
//! - `MockMonitor`：定义在 [`crate::monitor::mock`]（monitor-agent，内嵌在 monitor.rs），
//!   这里 re-export，使 `crate::mock::*` 成为下游统一入口。
//!
//! feature gate：`#[cfg(feature = "mock")]`，
//! 下游 `[dev-dependencies] os-services = { workspace = true, features = ["mock"] }`。

#![cfg(feature = "mock")]
#![allow(async_fn_in_trait)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use os_core::{DatasetId, DateTime, PageRequest, PageResponse, PoolId, SnapshotId, TaskId};

use crate::backup::{BackupJob, BackupManager, BackupPolicy, BackupStatus, ScrubReport};
use crate::devtools::{CiPipeline, CiRun, CiStatus, DevTools};
use crate::media::{Album, MediaAsset, MediaManager, TranscodeProfile};
use crate::power::{FanReading, PowerManager, PowerSchedule, SmartReport, TempReading, UpsStatus};
use crate::ServiceError;

// Re-export：其他 agent 在自己实现文件中定义的 Mock，统一聚合到 `crate::mock::*`。
pub use crate::impl_files::MockFileManager;
pub use crate::monitor::mock::MockMonitor;

// =============================================================================
// [power-agent] MockPowerManager
// =============================================================================

/// Mock `PowerManager` —— 纯内存、确定性，供下游测试注入。
///
/// 内部状态用 `Mutex` 包裹以实现 `Sync`；写操作（schedule_power / force_shutdown）
/// 更新内部状态，使后续读反映变更。错误注入经 `with_error`。
pub struct MockPowerManager {
    inner: Mutex<PowerMockState>,
}

struct PowerMockState {
    ups: UpsStatus,
    temps: Vec<TempReading>,
    fans: Vec<FanReading>,
    smart: SmartReport,
    schedule: Option<PowerSchedule>,
    shutdown_calls: Vec<String>,
    forced_error: Option<ServiceError>,
}

impl PowerMockState {
    fn default_state() -> Self {
        Self {
            ups: UpsStatus {
                online: true,
                battery_level: Some(100),
                estimated_minutes: Some(60),
                model: "MockUPS".into(),
            },
            temps: vec![TempReading {
                label: "cpu".into(),
                celsius: 45.0,
            }],
            fans: vec![FanReading {
                label: "cpu_fan".into(),
                rpm: 1500,
            }],
            smart: SmartReport {
                disk: "/dev/mock".into(),
                passed: true,
                temperature: 35.0,
                reallocated_sectors: 0,
                power_on_hours: 100,
            },
            schedule: None,
            shutdown_calls: Vec::new(),
            forced_error: None,
        }
    }

    fn check_forced(&mut self) -> Result<(), ServiceError> {
        if let Some(e) = self.forced_error.take() {
            return Err(e);
        }
        Ok(())
    }
}

impl Default for MockPowerManager {
    fn default() -> Self {
        Self::new()
    }
}

impl MockPowerManager {
    /// 新建默认 mock（健康 UPS、正常温风、passed SMART）。
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(PowerMockState::default_state()),
        }
    }

    /// 预置 `ups_status` 返回值。
    #[must_use]
    pub fn with_ups(self, ups: UpsStatus) -> Self {
        self.inner.lock().expect("mock lock").ups = ups;
        self
    }

    /// 预置 `read_temps` 返回值。
    #[must_use]
    pub fn with_temps(self, temps: Vec<TempReading>) -> Self {
        self.inner.lock().expect("mock lock").temps = temps;
        self
    }

    /// 预置 `read_fans` 返回值。
    #[must_use]
    pub fn with_fans(self, fans: Vec<FanReading>) -> Self {
        self.inner.lock().expect("mock lock").fans = fans;
        self
    }

    /// 预置 `smart_check` 返回值。
    #[must_use]
    pub fn with_smart(self, smart: SmartReport) -> Self {
        self.inner.lock().expect("mock lock").smart = smart;
        self
    }

    /// 注入强制错误：下一次任意方法返回此错误（一次性，之后清除）。
    #[must_use]
    pub fn with_error(self, err: ServiceError) -> Self {
        self.inner.lock().expect("mock lock").forced_error = Some(err);
        self
    }

    /// 取已记录的 `force_shutdown` 调用原因列表（测试断言）。
    pub fn shutdown_calls(&self) -> Vec<String> {
        self.inner.lock().expect("mock lock").shutdown_calls.clone()
    }

    /// 取当前持久化的调度配置（测试断言）。
    pub fn current_schedule(&self) -> Option<PowerSchedule> {
        self.inner.lock().expect("mock lock").schedule.clone()
    }
}

impl PowerManager for MockPowerManager {
    async fn ups_status(&self) -> Result<UpsStatus, ServiceError> {
        let mut g = self.inner.lock().expect("mock lock");
        g.check_forced()?;
        Ok(g.ups.clone())
    }

    async fn read_temps(&self) -> Result<Vec<TempReading>, ServiceError> {
        let mut g = self.inner.lock().expect("mock lock");
        g.check_forced()?;
        Ok(g.temps.clone())
    }

    async fn read_fans(&self) -> Result<Vec<FanReading>, ServiceError> {
        let mut g = self.inner.lock().expect("mock lock");
        g.check_forced()?;
        Ok(g.fans.clone())
    }

    async fn smart_check(&self, _disk: &str) -> Result<SmartReport, ServiceError> {
        let mut g = self.inner.lock().expect("mock lock");
        g.check_forced()?;
        Ok(g.smart.clone())
    }

    async fn schedule_power(&self, sched: PowerSchedule) -> Result<(), ServiceError> {
        let mut g = self.inner.lock().expect("mock lock");
        g.check_forced()?;
        g.schedule = Some(sched);
        Ok(())
    }

    async fn force_shutdown(&self, reason: &str) -> Result<(), ServiceError> {
        let mut g = self.inner.lock().expect("mock lock");
        g.check_forced()?;
        g.shutdown_calls.push(reason.to_string());
        Ok(())
    }
}

// =============================================================================
// [backup-agent] MockBackupManager
// =============================================================================

/// Mock 备份管理器——纯内存、确定性，供下游（api-agent 等）测试注入。
///
/// 行为：
/// - `schedule`：存入内部 job 表，返回自增 id。
/// - `unschedule`：删除 job；不存在返回 `JobNotFound`。
/// - `list_jobs`：返回内部 job 表克隆。
/// - `trigger_now`：标记 job 为 Success（不真正创建快照），返回 `TaskId::new()`。
/// - `scrub_status`：返回预置报告（默认空报告，可用 [`MockBackupManager::with_scrub`] 预置）。
/// - `restore`：返回 `TaskId::new()`。
pub struct MockBackupManager {
    inner: Mutex<BackupMockState>,
}

struct BackupMockState {
    jobs: HashMap<String, BackupJob>,
    /// 下一个 job id 的序号。
    next_seq: u64,
    /// 预置的 scrub 报告（scrub_status 返回此克隆）。
    scrub: ScrubReport,
    /// 强制错误（下次任一方法返回；一次性）。
    forced_error: Option<ServiceError>,
}

impl BackupMockState {
    fn new() -> Self {
        Self {
            jobs: HashMap::new(),
            next_seq: 1,
            scrub: ScrubReport {
                errors: 0,
                repaired: 0,
                last_finished: None,
                duration_secs: 0,
            },
            forced_error: None,
        }
    }

    fn check_forced(&mut self) -> Result<(), ServiceError> {
        if let Some(e) = self.forced_error.take() {
            return Err(e);
        }
        Ok(())
    }
}

impl MockBackupManager {
    /// 构造空 mock（无 job、空 scrub 报告）。
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(BackupMockState::new()),
        }
    }

    /// 预置 scrub 报告（`scrub_status` 返回此克隆）。
    #[must_use]
    pub fn with_scrub(self, report: ScrubReport) -> Self {
        {
            let mut st = self.inner.lock().expect("mock poisoned");
            st.scrub = report;
        }
        self
    }

    /// 预置一个已存在的 job（便于 `trigger_now`/`unschedule` 测试）。
    #[must_use]
    pub fn with_job(self, job: BackupJob) -> Self {
        {
            let mut st = self.inner.lock().expect("mock poisoned");
            st.jobs.insert(job.id.clone(), job);
        }
        self
    }

    /// 强制下次方法返回错误（一次性）。
    #[must_use]
    pub fn with_error(self, err: ServiceError) -> Self {
        {
            let mut st = self.inner.lock().expect("mock poisoned");
            st.forced_error = Some(err);
        }
        self
    }

    /// 当前 job 数量（断言用）。
    pub fn job_count(&self) -> usize {
        self.inner.lock().expect("mock poisoned").jobs.len()
    }
}

impl Default for MockBackupManager {
    fn default() -> Self {
        Self::new()
    }
}

impl BackupManager for MockBackupManager {
    async fn schedule(&self, policy: BackupPolicy) -> Result<String, ServiceError> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        let id = format!("mock-job-{}", st.next_seq);
        st.next_seq += 1;
        let job = BackupJob {
            id: id.clone(),
            policy,
            last_run: None,
            next_run: None, // mock 不算 next_run
            status: BackupStatus::Scheduled,
        };
        st.jobs.insert(id.clone(), job);
        Ok(id)
    }

    async fn unschedule(&self, job_id: &str) -> Result<(), ServiceError> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        if st.jobs.remove(job_id).is_none() {
            return Err(ServiceError::JobNotFound(job_id.to_string()));
        }
        Ok(())
    }

    async fn list_jobs(&self) -> Result<Vec<BackupJob>, ServiceError> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        let mut list: Vec<BackupJob> = st.jobs.values().cloned().collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(list)
    }

    async fn trigger_now(&self, job_id: &str) -> Result<TaskId, ServiceError> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        let job = st
            .jobs
            .get_mut(job_id)
            .ok_or_else(|| ServiceError::JobNotFound(job_id.to_string()))?;
        job.status = BackupStatus::Success;
        job.last_run = Some(os_core::Utc::now());
        Ok(TaskId::new())
    }

    async fn scrub_status(&self, _pool: &PoolId) -> Result<ScrubReport, ServiceError> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        Ok(st.scrub.clone())
    }

    async fn restore(
        &self,
        _snapshot: &SnapshotId,
        _target: &DatasetId,
    ) -> Result<TaskId, ServiceError> {
        let mut st = self.inner.lock().expect("mock poisoned");
        st.check_forced()?;
        Ok(TaskId::new())
    }
}

// =============================================================================
// [devtools-agent] MockDevTools
// =============================================================================

/// Mock DevTools——纯内存、确定性，供下游（api-agent 等）测试注入。
///
/// - 预置 pipeline：`with_pipelines`；`trigger_pipeline` 校验存在性并返回确定性 TaskId。
/// - 预置密钥：`with_secret`；`get_secret` 命中返回预置值，未命中返回 `SecretNotFound`。
/// - 错误注入：`with_error` 让所有方法返回强制错误。
pub struct MockDevTools {
    inner: Mutex<DevToolsMockState>,
}

struct DevToolsMockState {
    pipelines: HashMap<String, CiPipeline>,
    /// 密钥 KVS：明文占位（mock 不加密，仅供测试逻辑）
    secrets: HashMap<String, Vec<u8>>,
    /// 强制错误（注入测试错误路径）
    forced_error: Option<String>,
}

impl DevToolsMockState {
    fn new() -> Self {
        Self {
            pipelines: HashMap::new(),
            secrets: HashMap::new(),
            forced_error: None,
        }
    }
}

impl MockDevTools {
    /// 构造空 mock。
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(DevToolsMockState::new()),
        }
    }

    /// 预置流水线定义。
    #[must_use]
    pub fn with_pipelines(self, pipelines: Vec<CiPipeline>) -> Self {
        {
            let mut st = self.inner.lock().expect("mock poisoned");
            for p in pipelines {
                st.pipelines.insert(p.id.clone(), p);
            }
        }
        self
    }

    /// 预置一个密钥（明文，mock 不加密）。
    #[must_use]
    pub fn with_secret(self, key: &str, value: &[u8]) -> Self {
        self.inner
            .lock()
            .expect("mock poisoned")
            .secrets
            .insert(key.to_string(), value.to_vec());
        self
    }

    /// 注入强制错误：下次起的方法返回 `Internal(msg)`。
    #[must_use]
    pub fn with_error(self, msg: impl Into<String>) -> Self {
        self.inner.lock().expect("mock poisoned").forced_error = Some(msg.into());
        self
    }

    fn now() -> DateTime {
        os_core::Utc::now()
    }
}

impl Default for MockDevTools {
    fn default() -> Self {
        Self::new()
    }
}

impl DevTools for MockDevTools {
    async fn trigger_pipeline(&self, pipeline_id: &str) -> Result<TaskId, ServiceError> {
        let mut st = self.inner.lock().expect("mock poisoned");
        if let Some(msg) = st.forced_error.take() {
            return Err(ServiceError::Internal(msg));
        }
        if !st.pipelines.contains_key(pipeline_id) {
            return Err(ServiceError::PipelineFailed(format!(
                "流水线不存在: {pipeline_id}"
            )));
        }
        Ok(TaskId::new())
    }

    async fn pipeline_status(&self, _task: &TaskId) -> Result<CiRun, ServiceError> {
        let mut st = self.inner.lock().expect("mock poisoned");
        if let Some(msg) = st.forced_error.take() {
            return Err(ServiceError::Internal(msg));
        }
        // 确定性返回一个 Success 运行
        Ok(CiRun {
            pipeline_id: "mock".into(),
            run_id: "mock-run".into(),
            status: CiStatus::Success,
            started_at: Self::now(),
            logs_url: Some("mock://logs".into()),
        })
    }

    async fn store_secret(&self, key: &str, value: &[u8]) -> Result<(), ServiceError> {
        let mut st = self.inner.lock().expect("mock poisoned");
        if let Some(msg) = st.forced_error.take() {
            return Err(ServiceError::Internal(msg));
        }
        st.secrets.insert(key.to_string(), value.to_vec());
        Ok(())
    }

    async fn get_secret(&self, key: &str) -> Result<Vec<u8>, ServiceError> {
        let mut st = self.inner.lock().expect("mock poisoned");
        if let Some(msg) = st.forced_error.take() {
            return Err(ServiceError::Internal(msg));
        }
        st.secrets
            .get(key)
            .cloned()
            .ok_or_else(|| ServiceError::SecretNotFound(key.to_string()))
    }

    async fn rotate_secret(&self, key: &str) -> Result<(), ServiceError> {
        let mut st = self.inner.lock().expect("mock poisoned");
        if let Some(msg) = st.forced_error.take() {
            return Err(ServiceError::Internal(msg));
        }
        if st.secrets.contains_key(key) {
            Ok(())
        } else {
            Err(ServiceError::SecretNotFound(key.to_string()))
        }
    }

    async fn list_pipelines(&self) -> Result<Vec<CiPipeline>, ServiceError> {
        let mut st = self.inner.lock().expect("mock poisoned");
        if let Some(msg) = st.forced_error.take() {
            return Err(ServiceError::Internal(msg));
        }
        Ok(st.pipelines.values().cloned().collect())
    }
}

// =============================================================================
// [media-agent] MockMediaManager
// =============================================================================

/// 媒体管理器 Mock。
///
/// 行为：
/// - `ingest`：若配置了 `with_asset` 则返回预置 asset；否则构造一个确定性 asset。
/// - `search`：从内存态按子串过滤，分页返回。
/// - `transcode`：返回固定 `TaskId`（由 `with_task_id` 设，默认 nil UUID）。
/// - `stream_playlist`：返回固定 URL 串。
/// - `list_albums`：返回预置相册列表（默认空）。
///
/// 构造：`MockMediaManager::new().with_asset(...).with_albums(...)`。
#[derive(Debug, Default, Clone)]
pub struct MockMediaManager {
    inner: Arc<Mutex<MediaMockState>>,
}

#[derive(Debug, Default)]
struct MediaMockState {
    assets: HashMap<String, MediaAsset>,
    albums: Vec<Album>,
    task_id: Option<TaskId>,
    /// ingest 默认返回（未预置时）
    ingest_default: Option<MediaAsset>,
    /// ingest 失败路径集合（命中即返回 Internal 错误）
    ingest_fail_paths: Vec<String>,
}

impl MockMediaManager {
    /// 构造空 Mock。
    pub fn new() -> Self {
        Self::default()
    }

    /// 预置一个 asset（search/list_albums 可见）。
    #[must_use]
    pub fn with_asset(self, asset: MediaAsset) -> Self {
        {
            let mut st = self.inner.lock().expect("mock lock");
            st.assets.insert(asset.id.clone(), asset);
        }
        self
    }

    /// 预置相册列表。
    #[must_use]
    pub fn with_albums(self, albums: Vec<Album>) -> Self {
        {
            let mut st = self.inner.lock().expect("mock lock");
            st.albums = albums;
        }
        self
    }

    /// 设 transcode 返回的固定 TaskId。
    #[must_use]
    pub fn with_task_id(self, task_id: TaskId) -> Self {
        {
            let mut st = self.inner.lock().expect("mock lock");
            st.task_id = Some(task_id);
        }
        self
    }

    /// 设 ingest 的默认返回（路径未命中预置时）。
    #[must_use]
    pub fn with_ingest_default(self, asset: MediaAsset) -> Self {
        {
            let mut st = self.inner.lock().expect("mock lock");
            st.ingest_default = Some(asset);
        }
        self
    }

    /// 让 ingest 对给定路径返回 `Internal` 错误（模拟失败）。
    #[must_use]
    pub fn with_ingest_failure(self, path: impl Into<String>) -> Self {
        {
            let mut st = self.inner.lock().expect("mock lock");
            st.ingest_fail_paths.push(path.into());
        }
        self
    }
}

impl MediaManager for MockMediaManager {
    async fn ingest(&self, path: &Path) -> Result<MediaAsset, ServiceError> {
        let path_str = path.to_string_lossy().to_string();
        let st = self.inner.lock().expect("mock lock");
        if st.ingest_fail_paths.iter().any(|p| p == &path_str) {
            return Err(ServiceError::Internal(format!(
                "mock ingest 失败: {path_str}"
            )));
        }
        // 若预置默认则返回之（拷贝并改 path）
        if let Some(a) = &st.ingest_default {
            let mut a = a.clone();
            a.path = path_str;
            return Ok(a);
        }
        // 否则构造确定性 asset
        let id = format!("mock:{}", path_str);
        Ok(MediaAsset {
            id,
            path: path_str,
            mime_type: "image/jpeg".to_string(),
            size_bytes: 0,
            width: Some(640),
            height: Some(480),
            taken_at: None,
            faces: Vec::new(),
            clip_embedding: None,
        })
    }

    async fn search(
        &self,
        query: &str,
        page: PageRequest,
    ) -> Result<PageResponse<MediaAsset>, ServiceError> {
        let st = self.inner.lock().expect("mock lock");
        let q = query.to_lowercase();
        let mut hits: Vec<MediaAsset> = st
            .assets
            .values()
            .filter(|a| {
                a.path.to_lowercase().contains(&q)
                    || a.mime_type.to_lowercase().contains(&q)
                    || a.faces.iter().any(|f| {
                        f.name
                            .as_deref()
                            .map(|n| n.to_lowercase().contains(&q))
                            .unwrap_or(false)
                    })
            })
            .cloned()
            .collect();
        hits.sort_by(|a, b| a.id.cmp(&b.id));
        let total = hits.len() as u32;
        let items = hits
            .into_iter()
            .skip(page.offset as usize)
            .take(page.limit as usize)
            .collect();
        Ok(PageResponse {
            items,
            total,
            offset: page.offset,
            limit: page.limit,
        })
    }

    async fn transcode(
        &self,
        asset_id: &str,
        _profile: TranscodeProfile,
    ) -> Result<TaskId, ServiceError> {
        let st = self.inner.lock().expect("mock lock");
        // 未预置 asset 视为不存在（除非设了默认 ingest，表示 mock 接受任意 asset）
        if !st.assets.contains_key(asset_id) && st.ingest_default.is_none() {
            return Err(ServiceError::AssetNotFound(asset_id.to_string()));
        }
        Ok(st.task_id.unwrap_or_else(nil_task_id))
    }

    async fn stream_playlist(
        &self,
        asset_id: &str,
        profile: TranscodeProfile,
    ) -> Result<String, ServiceError> {
        let st = self.inner.lock().expect("mock lock");
        if !st.assets.contains_key(asset_id) && st.ingest_default.is_none() {
            return Err(ServiceError::AssetNotFound(asset_id.to_string()));
        }
        Ok(format!(
            "/mock-stream/{asset_id}/{}.m3u8",
            profile.target_height()
        ))
    }

    async fn list_albums(&self) -> Result<Vec<Album>, ServiceError> {
        let st = self.inner.lock().expect("mock lock");
        Ok(st.albums.clone())
    }
}

fn nil_task_id() -> TaskId {
    // 全零 UUID 作为确定性默认（mock 不应随机）
    use os_core::Uuid;
    TaskId(Uuid::nil())
}

// =============================================================================
// Tests（各 agent 的 mock 自测）
// =============================================================================

#[cfg(test)]
mod power_tests {
    use super::*;

    #[tokio::test]
    async fn mock_defaults() {
        let m = MockPowerManager::new();
        let ups = m.ups_status().await.unwrap();
        assert!(ups.online);
        assert_eq!(m.read_temps().await.unwrap().len(), 1);
        assert_eq!(m.read_fans().await.unwrap().len(), 1);
        assert!(m.smart_check("/dev/x").await.unwrap().passed);
    }

    #[tokio::test]
    async fn mock_with_ups_override() {
        let m = MockPowerManager::new().with_ups(UpsStatus {
            online: false,
            battery_level: Some(8),
            estimated_minutes: Some(2),
            model: "LowUPS".into(),
        });
        let ups = m.ups_status().await.unwrap();
        assert!(!ups.online);
        assert_eq!(ups.battery_level, Some(8));
    }

    #[tokio::test]
    async fn mock_force_shutdown_records() {
        let m = MockPowerManager::new();
        m.force_shutdown("low battery").await.unwrap();
        assert_eq!(m.shutdown_calls(), vec!["low battery".to_string()]);
    }

    #[tokio::test]
    async fn mock_schedule_persists() {
        let m = MockPowerManager::new();
        let s = PowerSchedule {
            power_on_cron: Some("0 3 * * *".into()),
            shutdown_cron: None,
        };
        m.schedule_power(s).await.unwrap();
        let got = m.current_schedule().expect("schedule persisted");
        assert_eq!(got.power_on_cron.as_deref(), Some("0 3 * * *"));
        assert_eq!(got.shutdown_cron, None);
    }

    #[tokio::test]
    async fn mock_error_injection_one_shot() {
        let m = MockPowerManager::new().with_error(ServiceError::HardwareError("ups gone".into()));
        let err = m.ups_status().await.unwrap_err();
        assert!(matches!(err, ServiceError::HardwareError(_)));
        // 二次调用恢复正常
        assert!(m.ups_status().await.is_ok());
    }
}

#[cfg(test)]
mod backup_tests {
    use super::*;
    use crate::backup::{CronExpr, RetentionPolicy};

    fn policy(name: &str) -> BackupPolicy {
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

    #[tokio::test]
    async fn schedule_and_list() {
        let m = MockBackupManager::new();
        let id = m.schedule(policy("daily")).await.unwrap();
        assert!(id.starts_with("mock-job-"));
        assert_eq!(m.job_count(), 1);
        let jobs = m.list_jobs().await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].status, BackupStatus::Scheduled);
    }

    #[tokio::test]
    async fn unschedule_unknown_errors() {
        let m = MockBackupManager::new();
        let err = m.unschedule("nope").await.unwrap_err();
        assert!(matches!(err, ServiceError::JobNotFound(_)));
    }

    #[tokio::test]
    async fn trigger_now_marks_success() {
        let m = MockBackupManager::new();
        let id = m.schedule(policy("daily")).await.unwrap();
        let _task = m.trigger_now(&id).await.unwrap();
        let jobs = m.list_jobs().await.unwrap();
        assert_eq!(jobs[0].status, BackupStatus::Success);
        assert!(jobs[0].last_run.is_some());
    }

    #[tokio::test]
    async fn scrub_status_returns_preset() {
        let report = ScrubReport {
            errors: 5,
            repaired: 3,
            last_finished: None,
            duration_secs: 120,
        };
        let m = MockBackupManager::new().with_scrub(report);
        let r = m.scrub_status(&PoolId::new("tank")).await.unwrap();
        assert_eq!(r.errors, 5);
        assert_eq!(r.repaired, 3);
        assert_eq!(r.duration_secs, 120);
    }

    #[tokio::test]
    async fn restore_returns_task() {
        let m = MockBackupManager::new();
        let task = m
            .restore(&SnapshotId::new("tank/media@s1"), &DatasetId::new("tank/r"))
            .await
            .unwrap();
        let _ = task;
    }

    #[tokio::test]
    async fn forced_error_one_shot() {
        let m = MockBackupManager::new().with_error(ServiceError::Internal("boom".into()));
        let err = m.list_jobs().await.unwrap_err();
        assert!(matches!(err, ServiceError::Internal(_)));
        // 一次性：再调正常
        assert!(m.list_jobs().await.is_ok());
    }

    #[tokio::test]
    async fn multiple_jobs_unique_ids() {
        let m = MockBackupManager::new();
        let id1 = m.schedule(policy("a")).await.unwrap();
        let id2 = m.schedule(policy("b")).await.unwrap();
        assert_ne!(id1, id2);
        assert_eq!(m.job_count(), 2);
    }
}

#[cfg(test)]
mod devtools_tests {
    use super::*;

    fn p(id: &str) -> CiPipeline {
        CiPipeline {
            id: id.into(),
            name: format!("pipe-{id}"),
            repo_url: "mock://repo".into(),
            branch: "main".into(),
            steps: vec!["build".into()],
        }
    }

    #[tokio::test]
    async fn trigger_unknown_pipeline_errors() {
        let m = MockDevTools::new();
        let err = m.trigger_pipeline("nope").await.unwrap_err();
        assert!(matches!(err, ServiceError::PipelineFailed(_)));
    }

    #[tokio::test]
    async fn trigger_known_pipeline_returns_taskid() {
        let m = MockDevTools::new().with_pipelines(vec![p("p1")]);
        let tid = m.trigger_pipeline("p1").await.unwrap();
        let _ = tid; // TaskId 非零即可
    }

    #[tokio::test]
    async fn secret_store_get_roundtrip() {
        let m = MockDevTools::new();
        m.store_secret("k", b"v").await.unwrap();
        let got = m.get_secret("k").await.unwrap();
        assert_eq!(got, b"v");
    }

    #[tokio::test]
    async fn secret_get_missing_errors() {
        let m = MockDevTools::new();
        let err = m.get_secret("x").await.unwrap_err();
        assert!(matches!(err, ServiceError::SecretNotFound(_)));
    }

    #[tokio::test]
    async fn secret_rotate_missing_errors() {
        let m = MockDevTools::new();
        let err = m.rotate_secret("x").await.unwrap_err();
        assert!(matches!(err, ServiceError::SecretNotFound(_)));
    }

    #[tokio::test]
    async fn forced_error_injects() {
        let m = MockDevTools::new().with_error("boom");
        let err = m.list_pipelines().await.unwrap_err();
        assert!(matches!(err, ServiceError::Internal(ref s) if s == "boom"));
    }

    #[tokio::test]
    async fn list_pipelines_returns_preset() {
        let m = MockDevTools::new().with_pipelines(vec![p("a"), p("b")]);
        let list = m.list_pipelines().await.unwrap();
        assert_eq!(list.len(), 2);
    }

    #[tokio::test]
    async fn with_secret_preset_gettable() {
        let m = MockDevTools::new().with_secret("tok", b"abc");
        let got = m.get_secret("tok").await.unwrap();
        assert_eq!(got, b"abc");
    }
}

#[cfg(test)]
mod media_tests {
    use super::*;
    use crate::media::{Album, BBox, FaceTag, MediaAsset};
    use os_core::PageRequest;

    fn asset(id: &str, path: &str, faces: &[&str]) -> MediaAsset {
        MediaAsset {
            id: id.to_string(),
            path: path.to_string(),
            mime_type: "image/jpeg".to_string(),
            size_bytes: 1,
            width: Some(100),
            height: Some(100),
            taken_at: None,
            faces: faces
                .iter()
                .map(|n| FaceTag {
                    name: Some((*n).to_string()),
                    bbox: BBox {
                        x: 0.0,
                        y: 0.0,
                        w: 0.1,
                        h: 0.1,
                    },
                })
                .collect(),
            clip_embedding: None,
        }
    }

    #[tokio::test]
    async fn ingest_default_when_unset() {
        let m = MockMediaManager::new();
        let a = m.ingest(std::path::Path::new("/x/y.jpg")).await.unwrap();
        assert_eq!(a.id, "mock:/x/y.jpg");
        assert_eq!(a.width, Some(640));
    }

    #[tokio::test]
    async fn ingest_with_default_override() {
        let m = MockMediaManager::new().with_ingest_default(asset("fixed", "/pre", &[]));
        let a = m
            .ingest(std::path::Path::new("/real/path.jpg"))
            .await
            .unwrap();
        assert_eq!(a.id, "fixed");
        assert_eq!(a.path, "/real/path.jpg"); // path 被实际路径覆盖
    }

    #[tokio::test]
    async fn ingest_failure() {
        let m = MockMediaManager::new().with_ingest_failure("/bad.jpg");
        let err = m
            .ingest(std::path::Path::new("/bad.jpg"))
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::Internal(_)));
    }

    #[tokio::test]
    async fn search_matches() {
        let m = MockMediaManager::new()
            .with_asset(asset("a1", "/vac/x.jpg", &["alice"]))
            .with_asset(asset("a2", "/work/y.jpg", &[]));
        let r = m.search("vac", PageRequest::default()).await.unwrap();
        assert_eq!(r.total, 1);
        let r = m.search("alice", PageRequest::default()).await.unwrap();
        assert_eq!(r.total, 1);
    }

    #[tokio::test]
    async fn transcode_uses_fixed_task_id() {
        use os_core::Uuid;
        let tid = TaskId(Uuid::parse_str("12345678-1234-1234-1234-123456789012").unwrap());
        let m = MockMediaManager::new()
            .with_asset(asset("a1", "/p", &[]))
            .with_task_id(tid);
        let got = m.transcode("a1", TranscodeProfile::Hls720p).await.unwrap();
        assert_eq!(got.0.to_string(), "12345678-1234-1234-1234-123456789012");
    }

    #[tokio::test]
    async fn transcode_unknown_errors() {
        let m = MockMediaManager::new();
        let err = m
            .transcode("ghost", TranscodeProfile::Hls720p)
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::AssetNotFound(_)));
    }

    #[tokio::test]
    async fn stream_playlist_url() {
        let m = MockMediaManager::new().with_asset(asset("a1", "/p", &[]));
        let url = m
            .stream_playlist("a1", TranscodeProfile::Hls1080p)
            .await
            .unwrap();
        assert_eq!(url, "/mock-stream/a1/1080.m3u8");
    }

    #[tokio::test]
    async fn list_albums_returns_preset() {
        let albums = vec![Album {
            id: "alb1".into(),
            name: "旅行".into(),
            asset_count: 5,
        }];
        let m = MockMediaManager::new().with_albums(albums.clone());
        let got = m.list_albums().await.unwrap();
        assert_eq!(got.len(), albums.len());
        assert_eq!(got[0].id, albums[0].id);
        assert_eq!(got[0].name, albums[0].name);
        assert_eq!(got[0].asset_count, albums[0].asset_count);
    }

    #[tokio::test]
    async fn default_task_id_is_nil() {
        let m = MockMediaManager::new().with_asset(asset("a1", "/p", &[]));
        let got = m.transcode("a1", TranscodeProfile::Hls720p).await.unwrap();
        assert_eq!(got.0.to_string(), "00000000-0000-0000-0000-000000000000");
    }
}
