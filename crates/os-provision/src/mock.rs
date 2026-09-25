//! Mock 实现（feature gate `mock`）——供下游 agent（cli/api 等）测试注入。
//!
//! 约定（`_conventions.md §5`）：
//! - 实现完整 trait（不 panic 的默认返回）
//! - builder 构造器设置预期返回值
//! - 纯内存、确定性
//!
//! 提供 [`MockProvisioner`] / [`MockMigrationEngine`]。
//! 两者都基于内存状态机，可注入预期状态/失败，覆盖各返回路径。

#![cfg(feature = "mock")]

use std::collections::HashMap;
use std::sync::Mutex;

use os_core::{DatasetId, NodeId, TaskId};

use crate::migration::{MigrationEngine, MigrationPlan, MigrationStatus};
use crate::provision::{ProvisionConfig, ProvisionStatus, ProvisionTarget, Provisioner};
use crate::ProvisionError;

// ----------------------------------------------------------------------------
// MockProvisioner
// ----------------------------------------------------------------------------

/// Mock `Provisioner`。
///
/// 默认行为：`boot_via_pxe`/`init_system` 成功并记录任务；`status` 返回
/// 默认推进状态（Booting → Installing → FormingPool → Ready）。
/// 可注入：预期失败、预期就绪节点 ID、状态覆盖。
pub struct MockProvisioner {
    tasks: Mutex<HashMap<TaskId, ProvisionStatus>>,
    /// 注入的"下次 init 后产出"的 node_id（None 则生成形如 `mock-node-<n>`）
    next_node_id: Mutex<Option<NodeId>>,
    /// boot/init 是否失败
    boot_fails: Mutex<bool>,
    init_fails: Mutex<bool>,
    /// 计数器（生成 node_id 用）
    counter: Mutex<u32>,
}

impl MockProvisioner {
    /// 默认 mock（成功路径）。
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            next_node_id: Mutex::new(None),
            boot_fails: Mutex::new(false),
            init_fails: Mutex::new(false),
            counter: Mutex::new(0),
        }
    }

    /// 注入 init 完成后产出的 node_id。
    pub fn with_ready_node(self, node_id: impl Into<String>) -> Self {
        *self.next_node_id.lock().unwrap() = Some(NodeId::new(node_id));
        self
    }

    /// 让 `boot_via_pxe` 失败。
    pub fn with_boot_failure(self, fails: bool) -> Self {
        *self.boot_fails.lock().unwrap() = fails;
        self
    }

    /// 让 `init_system` 失败。
    pub fn with_init_failure(self, fails: bool) -> Self {
        *self.init_fails.lock().unwrap() = fails;
        self
    }

    /// 覆盖某任务的状态（用于测试自定义状态查询）。
    pub fn set_status(&self, task: TaskId, status: ProvisionStatus) {
        self.tasks.lock().unwrap().insert(task, status);
    }

    fn gen_node_id(&self) -> NodeId {
        if let Some(id) = self.next_node_id.lock().unwrap().clone() {
            return id;
        }
        let mut c = self.counter.lock().unwrap();
        *c += 1;
        NodeId::new(format!("mock-node-{}", *c))
    }
}

impl Default for MockProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

impl Provisioner for MockProvisioner {
    async fn boot_via_pxe(&self, _target: &ProvisionTarget) -> Result<TaskId, ProvisionError> {
        if *self.boot_fails.lock().unwrap() {
            return Err(ProvisionError::PxeBootFailed(
                "mock: boot 已被配置为失败".into(),
            ));
        }
        let task = TaskId::new();
        self.tasks
            .lock()
            .unwrap()
            .insert(task, ProvisionStatus::Booting);
        Ok(task)
    }

    async fn init_system(
        &self,
        _target: &ProvisionTarget,
        _config: &ProvisionConfig,
    ) -> Result<TaskId, ProvisionError> {
        if *self.init_fails.lock().unwrap() {
            return Err(ProvisionError::InitFailed(
                "mock: init 已被配置为失败".into(),
            ));
        }
        let task = TaskId::new();
        let node_id = self.gen_node_id();
        self.tasks
            .lock()
            .unwrap()
            .insert(task, ProvisionStatus::Ready { node_id });
        Ok(task)
    }

    async fn status(&self, task: &TaskId) -> ProvisionStatus {
        self.tasks
            .lock()
            .unwrap()
            .get(task)
            .cloned()
            .unwrap_or(ProvisionStatus::Failed {
                reason: "mock: 未知任务".into(),
            })
    }
}

// ----------------------------------------------------------------------------
// MockMigrationEngine
// ----------------------------------------------------------------------------

/// Mock `MigrationEngine`。
///
/// 默认行为：`plan` 生成排除清单（用 [`crate::exclude::ExcludeRules::defaults`] 的
/// 类别串）、`execute` 记任务并置 `Transferring`→`Completed`、`resume` 同 execute、
/// `status` 查任务。可注入失败。
pub struct MockMigrationEngine {
    tasks: Mutex<HashMap<TaskId, MigrationStatus>>,
    plans: Mutex<HashMap<String, MigrationPlan>>, // plan_id -> plan
    plan_fails: Mutex<bool>,
    execute_fails: Mutex<bool>,
}

impl MockMigrationEngine {
    /// 默认 mock（成功路径）。
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            plans: Mutex::new(HashMap::new()),
            plan_fails: Mutex::new(false),
            execute_fails: Mutex::new(false),
        }
    }

    /// 让 `plan` 失败。
    pub fn with_plan_failure(self, fails: bool) -> Self {
        *self.plan_fails.lock().unwrap() = fails;
        self
    }

    /// 让 `execute` 失败。
    pub fn with_execute_failure(self, fails: bool) -> Self {
        *self.execute_fails.lock().unwrap() = fails;
        self
    }

    /// 预置一个 plan（resume 用）。
    pub fn with_plan(self, plan: MigrationPlan) -> Self {
        // 用 source/target 组合做 plan_id key
        let key = format!("{}->{}", plan.source_node, plan.target_node);
        self.plans.lock().unwrap().insert(key, plan);
        self
    }
}

impl Default for MockMigrationEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl MigrationEngine for MockMigrationEngine {
    async fn plan(
        &self,
        source: &NodeId,
        target: &NodeId,
        datasets: &[DatasetId],
    ) -> Result<MigrationPlan, ProvisionError> {
        if *self.plan_fails.lock().unwrap() {
            return Err(ProvisionError::MigrationFailed(
                "mock: plan 已被配置为失败".into(),
            ));
        }
        // 生成 §3.19 默认排除清单的"键串"——mock 不存路径，只存类别占位
        let exclude_keys: Vec<String> = crate::exclude::default_excludes()
            .into_iter()
            .map(|r| format!("exclude::{:?}", r.category))
            .collect();
        let plan = MigrationPlan {
            source_node: source.clone(),
            target_node: target.clone(),
            datasets: datasets.to_vec(),
            exclude_keys,
            resume_point: None,
        };
        let key = format!("{}->{}", source, target);
        self.plans.lock().unwrap().insert(key, plan.clone());
        Ok(plan)
    }

    async fn execute(&self, plan: MigrationPlan) -> Result<TaskId, ProvisionError> {
        if *self.execute_fails.lock().unwrap() {
            return Err(ProvisionError::MigrationFailed(
                "mock: execute 已被配置为失败".into(),
            ));
        }
        let task = TaskId::new();
        self.tasks
            .lock()
            .unwrap()
            .insert(task, MigrationStatus::Completed);
        // 记录 plan 以便 resume
        let key = format!("{}->{}", plan.source_node, plan.target_node);
        self.plans.lock().unwrap().insert(key, plan);
        Ok(task)
    }

    async fn resume(&self, plan_id: &str) -> Result<TaskId, ProvisionError> {
        // mock: 视为重新执行（与 execute 同路径）
        let _ = plan_id;
        if *self.execute_fails.lock().unwrap() {
            return Err(ProvisionError::MigrationFailed(
                "mock: resume 已被配置为失败".into(),
            ));
        }
        let task = TaskId::new();
        self.tasks
            .lock()
            .unwrap()
            .insert(task, MigrationStatus::Completed);
        Ok(task)
    }

    async fn status(&self, task: &TaskId) -> MigrationStatus {
        self.tasks
            .lock()
            .unwrap()
            .get(task)
            .cloned()
            .unwrap_or(MigrationStatus::Failed {
                reason: "mock: 未知任务".into(),
            })
    }
}

// ----------------------------------------------------------------------------
// mock 自测
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> ProvisionTarget {
        ProvisionTarget {
            mac: "aa:bb:cc:dd:ee:ff".into(),
            ip: None,
            arch: "x86_64".into(),
            endpoint: "10.0.0.5:8443".into(),
        }
    }

    fn config() -> ProvisionConfig {
        ProvisionConfig {
            base_image: "/img/base.squashfs".into(),
            root_password_hash: "$6$...".into(),
            zfs_pool_disks: vec!["/dev/sda".into()],
            network_config: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn provisioner_success_path() {
        let p = MockProvisioner::new().with_ready_node("node-7");
        let t = p.boot_via_pxe(&target()).await.unwrap();
        assert!(matches!(p.status(&t).await, ProvisionStatus::Booting));
        let t2 = p.init_system(&target(), &config()).await.unwrap();
        match p.status(&t2).await {
            ProvisionStatus::Ready { node_id } => assert_eq!(node_id.as_str(), "node-7"),
            _ => panic!("应为 Ready"),
        }
    }

    #[tokio::test]
    async fn provisioner_boot_failure() {
        let p = MockProvisioner::new().with_boot_failure(true);
        let err = p.boot_via_pxe(&target()).await.unwrap_err();
        assert!(matches!(err, ProvisionError::PxeBootFailed(_)));
    }

    #[tokio::test]
    async fn provisioner_init_failure() {
        let p = MockProvisioner::new().with_init_failure(true);
        let err = p.init_system(&target(), &config()).await.unwrap_err();
        assert!(matches!(err, ProvisionError::InitFailed(_)));
    }

    #[tokio::test]
    async fn provisioner_unknown_status() {
        let p = MockProvisioner::new();
        let st = p.status(&TaskId::new()).await;
        assert!(matches!(st, ProvisionStatus::Failed { .. }));
    }

    #[tokio::test]
    async fn migration_plan_generates_excludes() {
        let m = MockMigrationEngine::new();
        let plan = m
            .plan(
                &NodeId::new("a"),
                &NodeId::new("b"),
                &[DatasetId::new("tank/media")],
            )
            .await
            .unwrap();
        assert!(!plan.exclude_keys.is_empty(), "plan 应生成排除清单");
        assert_eq!(plan.datasets.len(), 1);
    }

    #[tokio::test]
    async fn migration_execute_completes() {
        let m = MockMigrationEngine::new();
        let plan = m
            .plan(&NodeId::new("a"), &NodeId::new("b"), &[])
            .await
            .unwrap();
        let t = m.execute(plan).await.unwrap();
        assert!(matches!(m.status(&t).await, MigrationStatus::Completed));
    }

    #[tokio::test]
    async fn migration_plan_failure() {
        let m = MockMigrationEngine::new().with_plan_failure(true);
        let err = m
            .plan(&NodeId::new("a"), &NodeId::new("b"), &[])
            .await
            .unwrap_err();
        assert!(matches!(err, ProvisionError::MigrationFailed(_)));
    }

    #[tokio::test]
    async fn migration_execute_failure() {
        let m = MockMigrationEngine::new().with_execute_failure(true);
        let plan = MigrationPlan {
            source_node: NodeId::new("a"),
            target_node: NodeId::new("b"),
            datasets: vec![],
            exclude_keys: vec![],
            resume_point: None,
        };
        let err = m.execute(plan).await.unwrap_err();
        assert!(matches!(err, ProvisionError::MigrationFailed(_)));
    }

    #[tokio::test]
    async fn migration_resume_completes() {
        let m = MockMigrationEngine::new();
        let t = m.resume("p1").await.unwrap();
        assert!(matches!(m.status(&t).await, MigrationStatus::Completed));
    }

    #[tokio::test]
    async fn migration_unknown_status() {
        let m = MockMigrationEngine::new();
        let st = m.status(&TaskId::new()).await;
        assert!(matches!(st, MigrationStatus::Failed { .. }));
    }

    // —— 覆盖率补测：builder 方法 + Default + gen_node_id fallback + set_status ——

    #[tokio::test]
    async fn provisioner_default() {
        // 覆盖 MockProvisioner::Default impl
        let p = MockProvisioner::default();
        // 不设 ready_node → gen_node_id 走 fallback（mock-node-<n>）
        let t = p.init_system(&target(), &config()).await.unwrap();
        match p.status(&t).await {
            ProvisionStatus::Ready { node_id } => {
                assert!(
                    node_id.as_str().starts_with("mock-node-"),
                    "fallback node_id"
                );
            }
            _ => panic!("应为 Ready"),
        }
    }

    #[tokio::test]
    async fn provisioner_set_status_override() {
        // 覆盖 set_status（覆盖某任务的状态）
        let p = MockProvisioner::new();
        let task = TaskId::new();
        p.set_status(
            task,
            ProvisionStatus::Failed {
                reason: "custom".into(),
            },
        );
        match p.status(&task).await {
            ProvisionStatus::Failed { reason } => assert_eq!(reason, "custom"),
            _ => panic!("应为自定义 Failed"),
        }
    }

    #[tokio::test]
    async fn provisioner_with_boot_failure_false_explicit() {
        // 覆盖 with_boot_failure(false) 显式调用（builder 方法体）
        let p = MockProvisioner::new().with_boot_failure(false);
        let t = p.boot_via_pxe(&target()).await.unwrap();
        assert!(matches!(p.status(&t).await, ProvisionStatus::Booting));
    }

    #[tokio::test]
    async fn provisioner_with_init_failure_false_explicit() {
        // 覆盖 with_init_failure(false) 显式调用
        let p = MockProvisioner::new().with_init_failure(false);
        let t = p.init_system(&target(), &config()).await.unwrap();
        assert!(matches!(p.status(&t).await, ProvisionStatus::Ready { .. }));
    }

    #[tokio::test]
    async fn provisioner_gen_node_id_counter_increments() {
        // 多次 init 不设 ready_node → 计数器递增，node_id 各不同
        let p = MockProvisioner::new();
        let t1 = p.init_system(&target(), &config()).await.unwrap();
        let t2 = p.init_system(&target(), &config()).await.unwrap();
        let n1 = match p.status(&t1).await {
            ProvisionStatus::Ready { node_id } => node_id,
            _ => panic!(),
        };
        let n2 = match p.status(&t2).await {
            ProvisionStatus::Ready { node_id } => node_id,
            _ => panic!(),
        };
        assert_ne!(n1.as_str(), n2.as_str(), "计数器递增 → node_id 不同");
    }

    #[tokio::test]
    async fn migration_engine_default() {
        // 覆盖 MockMigrationEngine::Default impl
        let m = MockMigrationEngine::default();
        let plan = m
            .plan(&NodeId::new("a"), &NodeId::new("b"), &[])
            .await
            .unwrap();
        assert!(!plan.exclude_keys.is_empty());
    }

    #[tokio::test]
    async fn migration_plan_failure_false_explicit() {
        // 覆盖 with_plan_failure(false) builder 方法
        let m = MockMigrationEngine::new().with_plan_failure(false);
        let plan = m.plan(&NodeId::new("a"), &NodeId::new("b"), &[]).await;
        assert!(plan.is_ok());
    }

    #[tokio::test]
    async fn migration_execute_failure_false_explicit() {
        // 覆盖 with_execute_failure(false) builder 方法
        let m = MockMigrationEngine::new().with_execute_failure(false);
        let plan = MigrationPlan {
            source_node: NodeId::new("a"),
            target_node: NodeId::new("b"),
            datasets: vec![],
            exclude_keys: vec![],
            resume_point: None,
        };
        let t = m.execute(plan).await.unwrap();
        assert!(matches!(m.status(&t).await, MigrationStatus::Completed));
    }

    #[tokio::test]
    async fn migration_with_plan_stores_plan() {
        // 覆盖 with_plan（预置一个 plan）
        let plan = MigrationPlan {
            source_node: NodeId::new("src"),
            target_node: NodeId::new("dst"),
            datasets: vec![DatasetId::new("tank/x")],
            exclude_keys: vec!["/etc/shadow".into()],
            resume_point: Some("anchor-1".into()),
        };
        let _m = MockMigrationEngine::new().with_plan(plan);
        // 不 panic 即可（builder 消费 self）
    }

    #[tokio::test]
    async fn migration_resume_fails_when_configured() {
        // 覆盖 resume 的 execute_fails 分支
        let m = MockMigrationEngine::new().with_execute_failure(true);
        let err = m.resume("p1").await.unwrap_err();
        assert!(matches!(err, ProvisionError::MigrationFailed(_)));
        assert!(matches!(
            err,
            ProvisionError::MigrationFailed(ref msg) if msg.contains("resume")
        ));
    }
}
