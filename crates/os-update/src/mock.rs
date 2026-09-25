//! `Mock*` 实现（feature gate `mock`）——供下游 api-agent 测试注入。
//!
//! 约定（_conventions.md §5）：
//! - 实现完整 trait（不 panic 的默认返回，纯内存、确定性）。
//! - 提供构造器预置返回值（builder 风格）。
//! - 不依赖外部状态（无 bootloader / NVD / 网络），下游测试可确定性运行。
//!
//! 四个 Mock：[`MockUpdateEngine`] / [`MockRollbackManager`] /
//! [`MockCveMonitor`] / [`MockRollingUpgrade`]。
//!
//! dyn 兼容性（ADR-COMPAT-001）：本 crate 的 `UpdateEngine`/`RollbackManager`/
//! `CveMonitor`/`RollingUpgrade` 保持原生 `async fn in trait`（单实现为主），
//! 故**不能** `Box<dyn UpdateEngine>`。下游（api-agent）应以**具体类型或泛型**注入
//! mock。仅 `CveCallback` 用 `#[async_trait]` 可 dyn。

#![cfg(feature = "mock")]

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use os_core::{HealthReport, NodeId, NodeInfo, TaskId};

use crate::rollback::{
    should_rollback, RollbackContext, RollbackDecision, RollbackManager, RollbackPoint,
    RollbackPolicy,
};
use crate::rolling::{
    decide_upgrade_order, RollingPlan, RollingStatus, RollingStrategy, RollingUpgrade,
};
use crate::slot::SlotManager;
use crate::update::{UpdateEngine, UpdateManifest, UpdateSlot, UpdateStatus};
use crate::{CveAdvisory, CveCallback, CveMonitor};

// ============================================================================
// MockUpdateEngine
// ============================================================================

/// Mock `UpdateEngine`——纯内存、确定性。
///
/// - `check_updates`：返回构造器预置清单（默认空 → `NoUpdates`）。
/// - `download`：注册任务并立即推进到 `Completed`。
/// - `verify`：返回构造器预置结果（默认 true）。
/// - `write_to_inactive_slot`：基于内置 [`SlotManager`] 决定可写槽，标记写入。
/// - `activate_slot`：推进槽位状态机。
/// - `status`：返回任务表状态。
///
/// 错误注入：`with_check_error` / `with_download_error` / `with_verify_result(false)`。
pub struct MockUpdateEngine {
    updates: Mutex<Vec<UpdateManifest>>,
    tasks: Mutex<HashMap<TaskId, UpdateStatus>>,
    slot: Mutex<SlotManager>,
    verify_result: Mutex<bool>,
    check_error: Mutex<Option<crate::UpdateError>>,
    download_error: Mutex<Option<crate::UpdateError>>,
}

impl Default for MockUpdateEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl MockUpdateEngine {
    /// 构造空 mock（无更新、verify=true、A 槽 active v1.0.0）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            updates: Mutex::new(Vec::new()),
            tasks: Mutex::new(HashMap::new()),
            slot: Mutex::new(SlotManager::new(UpdateSlot::A, "1.0.0", chrono::Utc::now())),
            verify_result: Mutex::new(true),
            check_error: Mutex::new(None),
            download_error: Mutex::new(None),
        }
    }

    /// 预置可用更新清单（`check_updates` 返回此清单）。
    #[must_use]
    pub fn with_updates(self, updates: Vec<UpdateManifest>) -> Self {
        *self.updates.lock().expect("mock poisoned") = updates;
        self
    }

    /// 设置 `verify` 的返回值。
    #[must_use]
    pub fn with_verify_result(self, ok: bool) -> Self {
        *self.verify_result.lock().expect("mock poisoned") = ok;
        self
    }

    /// 注入 `check_updates` 错误（抛出后清空）。
    #[must_use]
    pub fn with_check_error(self, err: crate::UpdateError) -> Self {
        *self.check_error.lock().expect("mock poisoned") = Some(err);
        self
    }

    /// 注入 `download` 错误（抛出后清空）。
    #[must_use]
    pub fn with_download_error(self, err: crate::UpdateError) -> Self {
        *self.download_error.lock().expect("mock poisoned") = Some(err);
        self
    }

    /// 取槽位状态机快照（供断言）。
    pub fn slot_manager(&self) -> SlotManager {
        self.slot.lock().expect("mock poisoned").clone()
    }
}

impl UpdateEngine for MockUpdateEngine {
    async fn check_updates(&self) -> Result<Vec<UpdateManifest>, crate::UpdateError> {
        if let Some(err) = self.check_error.lock().expect("mock poisoned").take() {
            return Err(err);
        }
        let g = self.updates.lock().expect("mock poisoned");
        if g.is_empty() {
            return Err(crate::UpdateError::NoUpdates);
        }
        Ok(g.clone())
    }

    async fn download(&self, manifest: &UpdateManifest) -> Result<TaskId, crate::UpdateError> {
        if let Some(err) = self.download_error.lock().expect("mock poisoned").take() {
            return Err(err);
        }
        let task = TaskId::new();
        self.tasks
            .lock()
            .expect("mock poisoned")
            .insert(task, UpdateStatus::Completed);
        // 同时把目标版本记入槽位（模拟下载完成即可写入）
        let _ = manifest;
        Ok(task)
    }

    async fn verify(
        &self,
        _manifest: &UpdateManifest,
        _downloaded_path: &Path,
    ) -> Result<bool, crate::UpdateError> {
        Ok(*self.verify_result.lock().expect("mock poisoned"))
    }

    async fn write_to_inactive_slot(
        &self,
        manifest: &UpdateManifest,
    ) -> Result<UpdateSlot, crate::UpdateError> {
        let mut slot = self.slot.lock().expect("mock poisoned");
        let target = slot.writable_slot()?;
        slot.begin_write(target)?;
        slot.finish_write(target, &manifest.version, chrono::Utc::now())?;
        Ok(target)
    }

    async fn activate_slot(&self, slot: UpdateSlot) -> Result<(), crate::UpdateError> {
        let mut sm = self.slot.lock().expect("mock poisoned");
        match sm.plan_activation(slot) {
            crate::slot::SlotSwitchDecision::Activate { target, previous } => {
                sm.apply_activation(target, previous, chrono::Utc::now())?;
                let _ = sm.on_boot_succeeded();
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn status(&self, task: &TaskId) -> UpdateStatus {
        let tasks = self.tasks.lock().expect("mock poisoned");
        tasks.get(task).cloned().unwrap_or(UpdateStatus::Failed {
            reason: format!("任务不存在: {task}"),
        })
    }
}

// ============================================================================
// MockRollbackManager
// ============================================================================

/// Mock `RollbackManager`——纯内存、确定性。
///
/// 内置 [`SlotManager`] + 可注入的探活结果（健康/不健康）+ 策略。
/// `list_snapshots` 返回槽位中所有非 Failed 版本；`verify_current_health`
/// 返回构造器预置报告；`auto_rollback_if_unhealthy` 用 [`should_rollback`] 判定。
pub struct MockRollbackManager {
    slot: Mutex<SlotManager>,
    health: Mutex<HealthReport>,
    policy: RollbackPolicy,
    consecutive_failures: Mutex<u32>,
}

impl Default for MockRollbackManager {
    fn default() -> Self {
        Self::new()
    }
}

impl MockRollbackManager {
    /// 构造：A 槽 active v1.0.0，健康，Automatic 策略。
    #[must_use]
    pub fn new() -> Self {
        let report = HealthReport {
            health: os_core::Health::Healthy,
            message: None,
            timestamp: chrono::Utc::now(),
        };
        Self {
            slot: Mutex::new(SlotManager::new(UpdateSlot::A, "1.0.0", chrono::Utc::now())),
            health: Mutex::new(report),
            policy: RollbackPolicy::Automatic,
            consecutive_failures: Mutex::new(0),
        }
    }

    /// 设置探活结果（`verify_current_health` 返回此报告）。
    #[must_use]
    pub fn with_health(self, report: HealthReport) -> Self {
        *self.health.lock().expect("mock poisoned") = report;
        self
    }

    /// 设置回滚策略。
    #[must_use]
    pub fn with_policy(self, policy: RollbackPolicy) -> Self {
        Self { policy, ..self }
    }

    /// 取槽位状态机快照（供断言）。
    pub fn slot_manager(&self) -> SlotManager {
        self.slot.lock().expect("mock poisoned").clone()
    }
}

impl RollbackManager for MockRollbackManager {
    async fn list_snapshots(&self) -> Vec<RollbackPoint> {
        let sm = self.slot.lock().expect("mock poisoned");
        let mut points = Vec::new();
        for s in [sm.slot(UpdateSlot::A), sm.slot(UpdateSlot::B)] {
            if let Some(ver) = &s.version {
                if s.status != crate::slot::SlotStatus::Failed {
                    points.push(RollbackPoint {
                        slot: s.slot,
                        version: ver.clone(),
                        created_at: s.last_activated_at.unwrap_or_else(chrono::Utc::now),
                        healthy: s.status == crate::slot::SlotStatus::Active,
                    });
                }
            }
        }
        points
    }

    async fn rollback_to(&self, point: &RollbackPoint) -> Result<(), crate::UpdateError> {
        let mut sm = self.slot.lock().expect("mock poisoned");
        if let Some(current) = sm.active_slot() {
            if current != point.slot {
                sm.slot_mut(current).status = crate::slot::SlotStatus::Failed;
            }
        }
        sm.slot_mut(point.slot).status = crate::slot::SlotStatus::Active;
        Ok(())
    }

    async fn verify_current_health(&self) -> Result<HealthReport, crate::UpdateError> {
        Ok(self.health.lock().expect("mock poisoned").clone())
    }

    async fn auto_rollback_if_unhealthy(&self) -> Result<bool, crate::UpdateError> {
        let health = self.health.lock().expect("mock poisoned").health;
        let sm = self.slot.lock().expect("mock poisoned");
        let has_target = sm.previous_active_slot().is_some();
        let failures = *self.consecutive_failures.lock().expect("failures poisoned");
        let ctx = RollbackContext::new(health, self.policy, failures, has_target);
        match should_rollback(&ctx) {
            RollbackDecision::RollbackNow { .. } => {
                let mut sm = self.slot.lock().expect("mock poisoned");
                let decision = sm.on_boot_failed();
                Ok(matches!(
                    decision,
                    crate::slot::SlotSwitchDecision::Rollback { .. }
                ))
            }
            _ => Ok(false),
        }
    }
}

// ============================================================================
// MockCveMonitor
// ============================================================================

/// Mock `CveMonitor`——纯内存、确定性。
///
/// `check_advisories` 返回构造器预置公告；`subscribe` 链式注册回调。
/// 错误注入：`with_check_error`。
pub struct MockCveMonitor {
    advisories: Mutex<Vec<CveAdvisory>>,
    callbacks: Mutex<Vec<Box<dyn CveCallback>>>,
    check_error: Mutex<Option<crate::UpdateError>>,
}

impl Default for MockCveMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl MockCveMonitor {
    /// 构造空 mock（无公告）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            advisories: Mutex::new(Vec::new()),
            callbacks: Mutex::new(Vec::new()),
            check_error: Mutex::new(None),
        }
    }

    /// 预置公告清单。
    #[must_use]
    pub fn with_advisories(self, advisories: Vec<CveAdvisory>) -> Self {
        *self.advisories.lock().expect("mock poisoned") = advisories;
        self
    }

    /// 注入 `check_advisories` 错误。
    #[must_use]
    pub fn with_check_error(self, err: crate::UpdateError) -> Self {
        *self.check_error.lock().expect("mock poisoned") = Some(err);
        self
    }

    /// 已注册回调数量（供断言）。
    pub fn callback_count(&self) -> usize {
        self.callbacks.lock().expect("mock poisoned").len()
    }
}

impl CveMonitor for MockCveMonitor {
    async fn check_advisories(&self) -> Result<Vec<CveAdvisory>, crate::UpdateError> {
        if let Some(err) = self.check_error.lock().expect("mock poisoned").take() {
            return Err(err);
        }
        Ok(self.advisories.lock().expect("mock poisoned").clone())
    }

    async fn subscribe(&self, callback: Box<dyn CveCallback>) {
        self.callbacks.lock().expect("mock poisoned").push(callback);
    }
}

// ============================================================================
// MockRollingUpgrade
// ============================================================================

/// Mock `RollingUpgrade`——纯内存、确定性。
///
/// 内置成员快照 + 任务状态机表。`plan` 用 [`decide_upgrade_order`]；
/// `execute` 注册任务并立即推进到 `Completed`（确定性）；`status` 返回任务状态。
pub struct MockRollingUpgrade {
    members: Mutex<Vec<NodeInfo>>,
    tasks: Mutex<HashMap<TaskId, crate::rolling::RollingStateMachine>>,
    execute_error: Mutex<Option<crate::UpdateError>>,
}

impl Default for MockRollingUpgrade {
    fn default() -> Self {
        Self::new()
    }
}

impl MockRollingUpgrade {
    /// 构造空 mock（无成员）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            members: Mutex::new(Vec::new()),
            tasks: Mutex::new(HashMap::new()),
            execute_error: Mutex::new(None),
        }
    }

    /// 预置集群成员快照。
    #[must_use]
    pub fn with_members(self, members: Vec<NodeInfo>) -> Self {
        *self.members.lock().expect("mock poisoned") = members;
        self
    }

    /// 注入 `execute` 错误。
    #[must_use]
    pub fn with_execute_error(self, err: crate::UpdateError) -> Self {
        *self.execute_error.lock().expect("mock poisoned") = Some(err);
        self
    }
}

impl RollingUpgrade for MockRollingUpgrade {
    async fn plan(
        &self,
        _manifest: &UpdateManifest,
        strategy: RollingStrategy,
    ) -> Result<RollingPlan, crate::UpdateError> {
        let members = self.members.lock().expect("mock poisoned").clone();
        let order = decide_upgrade_order(&members, strategy)?;
        Ok(RollingPlan {
            order,
            strategy,
            per_node_verify: true,
        })
    }

    async fn execute(&self, plan: RollingPlan) -> Result<TaskId, crate::UpdateError> {
        if let Some(err) = self.execute_error.lock().expect("mock poisoned").take() {
            return Err(err);
        }
        let task = TaskId::new();
        let mut sm = crate::rolling::RollingStateMachine::new(plan);
        // 确定性推进：start → 逐节点成功 → Completed
        let _ = sm.start();
        while !matches!(
            sm.state,
            RollingStatus::Completed | RollingStatus::Failed { .. }
        ) {
            if sm.on_node_succeeded().is_err() {
                break;
            }
        }
        self.tasks.lock().expect("mock poisoned").insert(task, sm);
        Ok(task)
    }

    async fn status(&self, task: &TaskId) -> RollingStatus {
        let tasks = self.tasks.lock().expect("mock poisoned");
        tasks
            .get(task)
            .map(|sm| sm.state.clone())
            .unwrap_or(RollingStatus::Failed {
                failed_node: NodeId::new("unknown"),
                reason: format!("任务不存在: {task}"),
            })
    }
}

/// dyn 兼容性说明（呼应 ADR-COMPAT-001）：
///
/// 本 crate 的 `UpdateEngine`/`RollbackManager`/`CveMonitor`/`RollingUpgrade`
/// 保持原生 `async fn in trait`（单实现为主），不能 `Box<dyn>`。下游（api-agent）
/// 应以具体类型或泛型注入 mock。仅 `CveCallback` 用 `#[async_trait]` 可 dyn。
#[doc(hidden)]
pub fn _dyn_compat_note() {}

// ----------------------------------------------------------------------------
// 单元测试（仅 mock feature 下编译）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rolling::RollingStrategy;
    use crate::update::{ComponentUpdate, UpdateManifest, UpdateSlot};
    use async_trait::async_trait;
    use os_core::{Health, NodeId, NodeInfo, NodeRole};

    fn manifest(version: &str) -> UpdateManifest {
        UpdateManifest {
            version: version.to_string(),
            release_notes: String::new(),
            size_bytes: 100,
            sha256: "abc".to_string(),
            signature: "sig".to_string(),
            min_current_version: None,
            components: vec![ComponentUpdate {
                name: "osd".to_string(),
                version: version.to_string(),
                restart_required: false,
            }],
        }
    }

    fn node(id: &str, role: NodeRole) -> NodeInfo {
        NodeInfo {
            node_id: NodeId::new(id),
            role,
            version: "1.0.0".to_string(),
            arch: "x86_64".to_string(),
            endpoints: Vec::new(),
            health: Health::Healthy,
        }
    }

    // —— MockUpdateEngine ——

    #[tokio::test]
    async fn mock_check_no_updates_default() {
        let e = MockUpdateEngine::new();
        assert!(matches!(
            e.check_updates().await,
            Err(crate::UpdateError::NoUpdates)
        ));
    }

    #[tokio::test]
    async fn mock_check_returns_preset() {
        let e = MockUpdateEngine::new().with_updates(vec![manifest("1.1.0")]);
        let list = e.check_updates().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].version, "1.1.0");
    }

    #[tokio::test]
    async fn mock_check_error_injected() {
        let e = MockUpdateEngine::new()
            .with_check_error(crate::UpdateError::DownloadFailed("boom".into()));
        assert!(e.check_updates().await.is_err());
        // 第二次无错误（已消费）
        assert!(matches!(
            e.check_updates().await,
            Err(crate::UpdateError::NoUpdates)
        ));
    }

    #[tokio::test]
    async fn mock_verify_default_true() {
        let e = MockUpdateEngine::new();
        assert!(e
            .verify(&manifest("1.1.0"), std::path::Path::new("/tmp/x"))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn mock_verify_configurable_false() {
        let e = MockUpdateEngine::new().with_verify_result(false);
        assert!(!e
            .verify(&manifest("1.1.0"), std::path::Path::new("/tmp/x"))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn mock_write_and_activate_switches_slot() {
        let e = MockUpdateEngine::new();
        // A active，写入应返回 B
        let written = e.write_to_inactive_slot(&manifest("1.1.0")).await.unwrap();
        assert_eq!(written, UpdateSlot::B);
        // 激活 B → A 降级
        e.activate_slot(UpdateSlot::B).await.unwrap();
        let sm = e.slot_manager();
        assert_eq!(sm.active_slot(), Some(UpdateSlot::B));
    }

    #[tokio::test]
    async fn mock_download_error_injected() {
        let e = MockUpdateEngine::new()
            .with_download_error(crate::UpdateError::DownloadFailed("x".into()));
        assert!(e.download(&manifest("1.1.0")).await.is_err());
    }

    #[tokio::test]
    async fn mock_status_unknown_task() {
        let e = MockUpdateEngine::new();
        let s = e.status(&TaskId::new()).await;
        assert!(matches!(s, UpdateStatus::Failed { .. }));
    }

    // —— MockRollbackManager ——

    #[tokio::test]
    async fn mock_list_snapshots_default() {
        let r = MockRollbackManager::new();
        let snaps = r.list_snapshots().await;
        // 默认 A active v1.0.0 → 1 个快照
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].slot, UpdateSlot::A);
        assert_eq!(snaps[0].version, "1.0.0");
        assert!(snaps[0].healthy);
    }

    #[tokio::test]
    async fn mock_healthy_no_rollback() {
        let report = HealthReport {
            health: Health::Healthy,
            message: None,
            timestamp: chrono::Utc::now(),
        };
        let r = MockRollbackManager::new().with_health(report);
        assert!(!r.auto_rollback_if_unhealthy().await.unwrap());
    }

    #[tokio::test]
    async fn mock_unhealthy_no_target_no_rollback() {
        // 默认无 previous_active（首启）→ 即使 unhealthy 也不回滚
        let report = HealthReport {
            health: Health::Unhealthy,
            message: None,
            timestamp: chrono::Utc::now(),
        };
        let r = MockRollbackManager::new().with_health(report);
        assert!(!r.auto_rollback_if_unhealthy().await.unwrap());
    }

    #[tokio::test]
    async fn mock_rollback_to_switches() {
        let r = MockRollbackManager::new();
        // 手动把 B 标记 active（模拟双槽）
        {
            let mut sm = r.slot.lock().expect("mock poisoned");
            sm.slot_mut(UpdateSlot::B).status = crate::slot::SlotStatus::Active;
            sm.slot_mut(UpdateSlot::B).version = Some("1.1.0".to_string());
            sm.slot_mut(UpdateSlot::A).status = crate::slot::SlotStatus::Inactive;
            sm.previous_active = Some(UpdateSlot::A);
        }
        let point = RollbackPoint {
            slot: UpdateSlot::A,
            version: "1.0.0".to_string(),
            created_at: chrono::Utc::now(),
            healthy: true,
        };
        r.rollback_to(&point).await.unwrap();
        let sm = r.slot_manager();
        assert_eq!(sm.active_slot(), Some(UpdateSlot::A));
    }

    #[tokio::test]
    async fn mock_verify_health_returns_preset() {
        let report = HealthReport {
            health: Health::Degraded,
            message: Some("partial".to_string()),
            timestamp: chrono::Utc::now(),
        };
        let r = MockRollbackManager::new().with_health(report);
        let h = r.verify_current_health().await.unwrap();
        assert_eq!(h.health, Health::Degraded);
    }

    // —— MockCveMonitor ——

    #[tokio::test]
    async fn mock_cve_check_empty_default() {
        let m = MockCveMonitor::new();
        assert!(m.check_advisories().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn mock_cve_check_preset() {
        let adv = CveAdvisory {
            cve_id: "CVE-2024-1".to_string(),
            affected_component: "samba".to_string(),
            severity: crate::CveSeverity::High,
            fixed_version: "4.20".to_string(),
            published_at: chrono::Utc::now(),
        };
        let m = MockCveMonitor::new().with_advisories(vec![adv]);
        let list = m.check_advisories().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].cve_id, "CVE-2024-1");
    }

    #[tokio::test]
    async fn mock_cve_error_injected() {
        let m = MockCveMonitor::new()
            .with_check_error(crate::UpdateError::CveCheckFailed("down".into()));
        assert!(m.check_advisories().await.is_err());
    }

    #[tokio::test]
    async fn mock_cve_subscribe_counts() {
        struct Noop;
        #[async_trait]
        impl CveCallback for Noop {
            async fn on_advisory(&self, _: &CveAdvisory) {}
        }
        let m = MockCveMonitor::new();
        m.subscribe(Box::new(Noop)).await;
        m.subscribe(Box::new(Noop)).await;
        assert_eq!(m.callback_count(), 2);
    }

    // —— MockRollingUpgrade ——

    #[tokio::test]
    async fn mock_plan_followers_first() {
        let m = MockRollingUpgrade::new().with_members(vec![
            node("L", NodeRole::Leader),
            node("b", NodeRole::Follower),
            node("a", NodeRole::Follower),
        ]);
        let plan = m
            .plan(&manifest("1.1.0"), RollingStrategy::FollowersFirst)
            .await
            .unwrap();
        assert_eq!(
            plan.order,
            vec![NodeId::new("a"), NodeId::new("b"), NodeId::new("L")]
        );
    }

    #[tokio::test]
    async fn mock_execute_completes() {
        let m = MockRollingUpgrade::new().with_members(vec![
            node("L", NodeRole::Leader),
            node("a", NodeRole::Follower),
        ]);
        let plan = m
            .plan(&manifest("1.1.0"), RollingStrategy::FollowersFirst)
            .await
            .unwrap();
        let task = m.execute(plan).await.unwrap();
        let s = m.status(&task).await;
        assert!(matches!(s, RollingStatus::Completed));
    }

    #[tokio::test]
    async fn mock_execute_error_injected() {
        let m = MockRollingUpgrade::new().with_members(vec![node("L", NodeRole::Leader)]);
        let plan = m
            .plan(&manifest("1.1.0"), RollingStrategy::FollowersFirst)
            .await
            .unwrap();
        let m = m.with_execute_error(crate::UpdateError::Internal("boom".into()));
        assert!(m.execute(plan).await.is_err());
    }

    #[tokio::test]
    async fn mock_rolling_status_unknown_task() {
        let m = MockRollingUpgrade::new();
        let s = m.status(&TaskId::new()).await;
        assert!(matches!(s, RollingStatus::Failed { .. }));
    }
}
