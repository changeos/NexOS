//! 场景 3：HA 故障转移链路（规划文档 §3.5 / integration-agent 规格书 §3 I2）
//!
//! 链路：os-meta FailoverOrchestrator 检测节点失联 → 状态机推进
//! （Triggered → MigratingVm → SwitchingVip → PromotingReplica → Done）
//! → 每阶段调相应组件：
//!   - MigratingVm：调 os-compute VmManager.migrate_vm（VM 迁移）
//!   - SwitchingVip：调 os-meta VipManager.assign（VIP 漂移）
//!   - PromotingReplica：调 os-storage Replication.recv（副本提升为新主）
//!
//! 重点验证：
//! - 跨 crate 类型桥接：`FailoverTask` 状态机（os-meta 内部）+ 各组件 mock 协作。
//! - 状态机推进顺序与前置条件（record_* 后才能 advance）。
//! - 各组件调用顺序与状态机阶段一一对应。
//! - 错误传播：compute 迁移失败 → mark_failed，状态机不进入 VIP/Replica 阶段。
//! - detect_failure 返回 Some(reason) 触发 trigger_failover。
//!
//! 注：`HaFailoverOrchestrator` 默认实现是骨架（不真调 compute/storage/vip）；
//! 本测试构造一个**集成版** FailoverOrchestrator，把骨架 + 各组件串起来，
//! 验证状态机 + 各组件调用顺序的端到端正确性。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use os_compute::vm::{VmManager, VmState};
use os_compute::MockVmManager;
use os_core::eventbus::{Event, EventBus, Severity, Topic};
use os_core::mock::MockEventBus;
use os_core::{NodeId, TaskId, VmId};
use os_meta::failover::{FailoverOrchestrator, FailoverStatus};
use os_meta::failover_sm::{FailoverPhase, FailoverTask};
use os_meta::vip::{VipConfig, VipManager};
use os_network::interface::IpCidr;
use os_storage::backend::StorageBackend;
use os_storage::mock::MockStorageBackend;
use os_storage::replication::Replication;
use os_storage::StorageError;
use std::collections::HashMap;
use std::net::IpAddr;

// ----------------------------------------------------------------------------
// 集成版 FailoverOrchestrator：把骨架 + compute/storage/vip 串起来。
// 这是 integration-agent 搭建的「业务编排层」——验证各组件能跨 crate 协作。
// ----------------------------------------------------------------------------

struct IntegratedFailoverOrchestrator {
    compute: Arc<MockVmManager>,
    storage: Arc<MockStorageBackend>,
    vip: Arc<dyn VipManager>,
    bus: Arc<MockEventBus>,
    /// 已知的「失效节点上跑着的 VM 列表」（leader 维护的元数据，本测试预置）。
    node_vms: HashMap<NodeId, Vec<VmId>>,
    /// 探活结果注入：node → Some(reason) 视为失联；None 视为存活。
    probe_results: Mutex<HashMap<NodeId, Option<String>>>,
    /// 任务状态机表（task_id → FailoverTask）。
    tasks: Mutex<HashMap<TaskId, FailoverTask>>,
    /// 调用顺序记录（断言阶段 ↔ 组件调用对应）。
    call_log: Mutex<Vec<String>>,
}

impl IntegratedFailoverOrchestrator {
    fn new(
        compute: Arc<MockVmManager>,
        storage: Arc<MockStorageBackend>,
        vip: Arc<dyn VipManager>,
        bus: Arc<MockEventBus>,
    ) -> Self {
        Self {
            compute,
            storage,
            vip,
            bus,
            node_vms: HashMap::new(),
            probe_results: Mutex::new(HashMap::new()),
            tasks: Mutex::new(HashMap::new()),
            call_log: Mutex::new(Vec::new()),
        }
    }

    /// 注入：某节点上跑着的 VM 列表。
    fn with_node_vm(mut self, node: NodeId, vms: Vec<VmId>) -> Self {
        self.node_vms.insert(node, vms);
        self
    }

    /// 注入：探活结果（Some(reason) = 判失效，None = 存活）。
    fn set_probe(&self, node: &NodeId, result: Option<String>) {
        self.probe_results
            .lock()
            .expect("probe")
            .insert(node.clone(), result);
    }

    fn call_log(&self) -> Vec<String> {
        self.call_log.lock().expect("call_log").clone()
    }

    /// 驱动一次完整故障转移（同步串行推进状态机各阶段）。
    /// 这是 leader 的核心编排逻辑——集成测把它显式拉出来跑。
    async fn drive_failover(
        &self,
        failed: &NodeId,
        target: &NodeId,
    ) -> Result<FailoverTask, String> {
        let mut task = FailoverTask::new(failed.clone());
        let tid = task.task_id;

        // 阶段记录。
        self.tasks.lock().expect("tasks").insert(tid, task.clone());

        // === Triggered → MigratingVm ===
        task = task.advance().expect("Triggered→MigratingVm");

        // 调 compute 迁移该节点上所有 VM 到 target。
        let vms_to_migrate = self.node_vms.get(failed).cloned().unwrap_or_default();
        let mut migrated = Vec::new();
        for vm_id in &vms_to_migrate {
            match self.compute.migrate_vm(vm_id, target).await {
                Ok(_task) => {
                    migrated.push(vm_id.clone());
                    self.call_log
                        .lock()
                        .expect("call_log")
                        .push(format!("migrate_vm({vm_id} → {target}): Ok"));
                }
                Err(e) => {
                    self.call_log
                        .lock()
                        .expect("call_log")
                        .push(format!("migrate_vm({vm_id}): Err({e})"));
                    // 错误传播：迁移失败 → mark_failed，状态机停在此处。
                    let failed_task = task.mark_failed(format!("VM 迁移失败: {e}"));
                    let _ = self.bus_failure_event(failed, &failed_task).await;
                    self.tasks
                        .lock()
                        .expect("tasks")
                        .insert(tid, failed_task.clone());
                    return Err(format!("VM 迁移失败: {e}"));
                }
            }
        }
        task = task.record_migrated_vms(migrated);

        // === MigratingVm → SwitchingVip ===
        task = task.advance().expect("MigratingVm→SwitchingVip");

        // 调 vip 把 VIP 漂移到 target。
        match self.vip.assign(target).await {
            Ok(()) => {
                self.call_log
                    .lock()
                    .expect("call_log")
                    .push(format!("vip.assign({target}): Ok"));
                task = task.record_vip_moved(true);
            }
            Err(e) => {
                self.call_log
                    .lock()
                    .expect("call_log")
                    .push(format!("vip.assign({target}): Err({e})"));
                let failed_task = task.mark_failed(format!("VIP 切换失败: {e}"));
                let _ = self.bus_failure_event(failed, &failed_task).await;
                self.tasks
                    .lock()
                    .expect("tasks")
                    .insert(tid, failed_task.clone());
                return Err(format!("VIP 切换失败: {e}"));
            }
        }

        // === SwitchingVip → PromotingReplica ===
        task = task.advance().expect("SwitchingVip→PromotingReplica");

        // 调 storage「提升副本」——本测试用 Replication trait 的 send 占位
        // （真实提升是把目标节点的副本升级为主，mock 仅验证调用链通）。
        // 由于 MockStorageBackend 未实现 Replication，这里用 list_snapshots
        // 作为「副本提升后的元数据可见性」占位调用。
        match self.storage.list_snapshots(None).await {
            Ok(_) => {
                self.call_log
                    .lock()
                    .expect("call_log")
                    .push("storage.list_snapshots(promote check): Ok".to_string());
                task = task.record_replica_promoted(true);
            }
            Err(e) => {
                self.call_log
                    .lock()
                    .expect("call_log")
                    .push(format!("storage promote: Err({e})"));
                let failed_task = task.mark_failed(format!("副本提升失败: {e}"));
                let _ = self.bus_failure_event(failed, &failed_task).await;
                self.tasks
                    .lock()
                    .expect("tasks")
                    .insert(tid, failed_task.clone());
                return Err(format!("副本提升失败: {e}"));
            }
        }

        // === PromotingReplica → Done ===
        task = task.advance().expect("PromotingReplica→Done");

        // 发完成事件。
        let ev = Event {
            source: "os-meta".into(),
            topic: Topic::Cluster,
            kind: "failover.completed".into(),
            severity: Severity::Info,
            task_id: Some(tid),
            payload: serde_json::json!({
                "failed_node": failed.as_str(),
                "target_node": target.as_str(),
                "migrated_vms": vms_to_migrate.iter().map(|v| v.as_str()).collect::<Vec<_>>(),
                "phase": format!("{:?}", task.phase),
            }),
            timestamp: os_core::Utc::now(),
        };
        let _ = self.bus.publish(ev).await;

        self.tasks.lock().expect("tasks").insert(tid, task.clone());
        Ok(task)
    }

    async fn bus_failure_event(&self, failed: &NodeId, task: &FailoverTask) {
        let ev = Event {
            source: "os-meta".into(),
            topic: Topic::Cluster,
            kind: "failover.failed".into(),
            severity: Severity::Error,
            task_id: Some(task.task_id),
            payload: serde_json::json!({
                "failed_node": failed.as_str(),
                "reason": task.failure_reason,
            }),
            timestamp: os_core::Utc::now(),
        };
        let _ = self.bus.publish(ev).await;
    }
}

#[async_trait]
impl FailoverOrchestrator for IntegratedFailoverOrchestrator {
    async fn detect_failure(&self, node: &NodeId) -> Result<Option<String>, os_meta::MetaError> {
        let reason = self
            .probe_results
            .lock()
            .expect("probe")
            .get(node)
            .cloned()
            .flatten();
        Ok(reason)
    }

    async fn trigger_failover(&self, failed: &NodeId) -> Result<TaskId, os_meta::MetaError> {
        // 选一个非失效节点作为迁移目标（本测试用固定 "node-b"）。
        let target = NodeId::new("node-b");
        let task = self
            .drive_failover(failed, &target)
            .await
            .map_err(os_meta::MetaError::Internal)?;
        Ok(task.task_id)
    }

    async fn failover_status(&self, task: &TaskId) -> FailoverStatus {
        self.tasks
            .lock()
            .expect("tasks")
            .get(task)
            .map(|t| t.to_status())
            .unwrap_or(FailoverStatus::Aborted)
    }
}

// ----------------------------------------------------------------------------
// 内存版 VipManager（mock）：记录 assign/release 调用。
// 注：os-meta 未提供 MockVipManager（仅 mock trait 其他四个），本测试自建。
// ----------------------------------------------------------------------------

struct InMemoryVipManager {
    config: VipConfig,
    owner: Mutex<Option<NodeId>>,
    assign_count: Mutex<u32>,
}

impl InMemoryVipManager {
    fn new() -> Self {
        let ip = IpCidr::new(IpAddr::from([192, 168, 1, 100]), 24);
        Self {
            config: VipConfig {
                ip,
                interface: "br0".into(),
                current_owner: None,
            },
            owner: Mutex::new(None),
            assign_count: Mutex::new(0),
        }
    }

    fn assign_count(&self) -> u32 {
        *self.assign_count.lock().expect("assign_count")
    }

    fn current_owner(&self) -> Option<NodeId> {
        self.owner.lock().expect("owner").clone()
    }
}

#[async_trait]
impl VipManager for InMemoryVipManager {
    async fn assign(&self, node: &NodeId) -> Result<(), os_meta::MetaError> {
        *self.assign_count.lock().expect("assign_count") += 1;
        *self.owner.lock().expect("owner") = Some(node.clone());
        Ok(())
    }
    async fn release(&self) -> Result<(), os_meta::MetaError> {
        *self.owner.lock().expect("owner") = None;
        Ok(())
    }
    async fn current_owner(&self) -> Option<NodeId> {
        self.owner.lock().expect("owner").clone()
    }
}

// 让编译器忽略未用字段（config 仅用于构造，不真发 netlink）。
#[allow(dead_code)]
impl InMemoryVipManager {
    fn _config_ref(&self) -> &VipConfig {
        &self.config
    }
}

// ----------------------------------------------------------------------------
// 辅助：构造一个 Running VM 注入 compute mock（迁移前置——VM 须存在）。
// ----------------------------------------------------------------------------

fn running_vm(id: &str, node: &str) -> os_compute::vm::Vm {
    let mut vm = os_compute::vm::Vm::new_defined(
        VmId::new(id),
        id,
        os_compute::vm::VmSpec {
            cpus: os_compute::vm::CpuTopology::new(2),
            memory_mb: 1024,
            disk_vol_id: os_core::VolumeId::new("tank/vm/x"),
            nics: vec![os_compute::vm::VmNic::virtio("br0")],
            firmware: os_compute::vm::VmFirmware::Bios,
        },
    );
    vm.state = VmState::Running;
    vm.node_id = Some(NodeId::new(node));
    vm
}

// ----------------------------------------------------------------------------
// 集成测
// ----------------------------------------------------------------------------

#[tokio::test]
async fn ha_failover_full_state_machine_drives_all_components() {
    // compute 预置 node-a 上的 2 个 Running VM。
    let compute = Arc::new(
        MockVmManager::new("node-a")
            .with_vm(running_vm("vm-1", "node-a"))
            .with_vm(running_vm("vm-2", "node-a")),
    );
    let storage = Arc::new(MockStorageBackend::new());
    let vip = Arc::new(InMemoryVipManager::new());
    let bus = Arc::new(MockEventBus::new());

    let orch = IntegratedFailoverOrchestrator::new(
        compute.clone(),
        storage.clone(),
        vip.clone(),
        bus.clone(),
    )
    .with_node_vm(
        NodeId::new("node-a"),
        vec![VmId::new("vm-1"), VmId::new("vm-2")],
    );
    // 探活判定 node-a 失联。
    orch.set_probe(&NodeId::new("node-a"), Some("心跳超时".into()));

    // 探活。
    let reason = orch.detect_failure(&NodeId::new("node-a")).await.unwrap();
    assert_eq!(reason.as_deref(), Some("心跳超时"));

    // 触发故障转移（drive 完整状态机）。
    let tid = orch
        .trigger_failover(&NodeId::new("node-a"))
        .await
        .expect("failover 应成功");

    // 状态机到 Done。
    let status = orch.failover_status(&tid).await;
    assert!(
        matches!(status, FailoverStatus::Completed),
        "应 Completed，实得 {status:?}"
    );

    // 调用顺序断言：migrate(vm-1) → migrate(vm-2) → vip.assign → storage promote。
    let log = orch.call_log();
    assert!(
        log.iter().any(|s| s.contains("migrate_vm(vm-1")),
        "应迁移 vm-1: {log:?}"
    );
    assert!(
        log.iter().any(|s| s.contains("migrate_vm(vm-2")),
        "应迁移 vm-2: {log:?}"
    );
    assert!(
        log.iter().any(|s| s.contains("vip.assign")),
        "应切 VIP: {log:?}"
    );
    assert!(
        log.iter().any(|s| s.contains("storage.list_snapshots")),
        "应副本提升: {log:?}"
    );
    // 顺序：vip.assign 必须在所有 migrate 之后。
    let last_migrate = log.iter().rposition(|s| s.contains("migrate_vm")).unwrap();
    let vip_idx = log.iter().position(|s| s.contains("vip.assign")).unwrap();
    assert!(
        last_migrate < vip_idx,
        "VIP 切换应在所有 VM 迁移之后（顺序错乱: {log:?}）"
    );
    // promote 必须在 vip.assign 之后。
    let promote_idx = log
        .iter()
        .position(|s| s.contains("storage.list_snapshots"))
        .unwrap();
    assert!(vip_idx < promote_idx, "副本提升应在 VIP 切换之后");

    // compute 上两个 VM 都进了 Migrating 状态。
    assert_eq!(
        compute.get_vm(&VmId::new("vm-1")).await.unwrap().state,
        VmState::Migrating
    );
    assert_eq!(
        compute.get_vm(&VmId::new("vm-2")).await.unwrap().state,
        VmState::Migrating
    );
    // VIP 漂移到 node-b。
    assert_eq!(vip.assign_count(), 1);
    assert_eq!(
        vip.current_owner().as_ref().map(|n| n.as_str()),
        Some("node-b")
    );

    // EventBus 收到完成事件。
    assert_eq!(bus.published_count_for(Topic::Cluster), 1);
    assert_eq!(bus.published()[0].kind, "failover.completed");
    assert_eq!(bus.published()[0].severity, Severity::Info);
}

#[tokio::test]
async fn ha_failover_migrate_failure_marks_failed_and_no_vip() {
    // compute 注入 migrate 失败（fail_with 一次性）。
    let compute = Arc::new(
        MockVmManager::new("node-a")
            .fail_with(os_compute::ComputeError::LibvirtError("迁移超时".into()))
            .with_vm(running_vm("vm-x", "node-a")),
    );
    let storage = Arc::new(MockStorageBackend::new());
    let vip = Arc::new(InMemoryVipManager::new());
    let bus = Arc::new(MockEventBus::new());

    let orch = IntegratedFailoverOrchestrator::new(
        compute.clone(),
        storage.clone(),
        vip.clone(),
        bus.clone(),
    )
    .with_node_vm(NodeId::new("node-a"), vec![VmId::new("vm-x")]);

    let result = orch.trigger_failover(&NodeId::new("node-a")).await;
    assert!(result.is_err(), "迁移失败应传播为 Err");

    // 状态机停在 Failed（不是 Completed）。
    // 注：取 task_id 须先 drop 锁，再 await failover_status（后者会再取同一把锁——
    // 否则 guard 跨 await 临时延长生命周期会自死锁）。
    let tid_for_status = orch
        .tasks
        .lock()
        .expect("tasks")
        .keys()
        .next()
        .copied()
        .unwrap();
    let status = orch.failover_status(&tid_for_status).await;
    assert!(
        matches!(status, FailoverStatus::Failed { .. }),
        "应 Failed，实得 {status:?}"
    );

    // VIP 不应被切（错误在 MigratingVm 阶段，未进 SwitchingVip）。
    assert_eq!(vip.assign_count(), 0, "迁移失败时不应切 VIP");
    // 调用日志只有 migrate 失败记录，无 vip/promote。
    let log = orch.call_log();
    assert!(log.iter().any(|s| s.contains("migrate_vm(vm-x): Err")));
    assert!(
        !log.iter().any(|s| s.contains("vip.assign")),
        "迁移失败时不应调 vip.assign"
    );
    assert!(
        !log.iter().any(|s| s.contains("storage.list_snapshots")),
        "迁移失败时不应调 storage promote"
    );

    // 发了 failover.failed Error 事件。
    assert_eq!(bus.published_count_for(Topic::Cluster), 1);
    assert_eq!(bus.published()[0].kind, "failover.failed");
    assert_eq!(bus.published()[0].severity, Severity::Error);
}

#[tokio::test]
async fn ha_failover_vip_conflict_marks_failed() {
    // compute 正常，但 vip.assign 失败（用包装返回 Err 的 vip）。
    let compute = Arc::new(MockVmManager::new("node-a").with_vm(running_vm("vm-y", "node-a")));
    let storage = Arc::new(MockStorageBackend::new());
    let vip = Arc::new(ConflictVipManager);
    let bus = Arc::new(MockEventBus::new());

    let orch =
        IntegratedFailoverOrchestrator::new(compute.clone(), storage.clone(), vip, bus.clone())
            .with_node_vm(NodeId::new("node-a"), vec![VmId::new("vm-y")]);

    let result = orch.trigger_failover(&NodeId::new("node-a")).await;
    assert!(result.is_err(), "VIP 冲突应传播为 Err");

    // VM 已迁移成功（在 VIP 失败之前）。
    assert_eq!(
        compute.get_vm(&VmId::new("vm-y")).await.unwrap().state,
        VmState::Migrating
    );
    // status = Failed（先取 key 再 await，避免锁跨 await 自死锁）。
    let tid_for_status = orch
        .tasks
        .lock()
        .expect("tasks")
        .keys()
        .next()
        .copied()
        .unwrap();
    let status = orch.failover_status(&tid_for_status).await;
    assert!(matches!(status, FailoverStatus::Failed { .. }));
    // storage promote 不应被调用（VIP 阶段失败）。
    let log = orch.call_log();
    assert!(!log.iter().any(|s| s.contains("storage.list_snapshots")));
}

#[tokio::test]
async fn ha_failover_no_vms_still_completes() {
    // 失效节点上无 VM（record_migrated_vms(空) 显式确认）→ 仍能推进到 Done。
    let compute = Arc::new(MockVmManager::new("node-a"));
    let storage = Arc::new(MockStorageBackend::new());
    let vip = Arc::new(InMemoryVipManager::new());
    let bus = Arc::new(MockEventBus::new());

    let orch = IntegratedFailoverOrchestrator::new(compute, storage, vip, bus).with_node_vm(
        NodeId::new("node-a"),
        vec![], // 空
    );

    let tid = orch
        .trigger_failover(&NodeId::new("node-a"))
        .await
        .expect("无 VM 也应完成 failover");
    let status = orch.failover_status(&tid).await;
    assert!(matches!(status, FailoverStatus::Completed));
}

#[tokio::test]
async fn state_machine_preconditions_enforced() {
    // 直接验证：状态机推进前置条件——SwitchingVip 不 record_vip_moved 不能 advance。
    let mut task = FailoverTask::new(NodeId::new("n"));
    // Triggered → MigratingVm（Triggered 无前置条件）。
    task = task.advance().unwrap();
    assert_eq!(task.phase, FailoverPhase::MigratingVm);
    // MigratingVm：需先 record_migrated_vms（空也算显式确认）。
    task = task.record_migrated_vms(vec![]).advance().unwrap();
    assert_eq!(task.phase, FailoverPhase::SwitchingVip);
    // 未 record_vip_moved(true) 就 advance → 报 MissingPrecondition。
    let err = task.advance();
    assert!(err.is_err());
    let msg = format!("{}", err.unwrap_err());
    assert!(
        msg.contains("SwitchingVip"),
        "应报 SwitchingVip 前置条件: {msg}"
    );
}

#[tokio::test]
async fn state_machine_terminal_advance_rejected() {
    let mut task = FailoverTask::new(NodeId::new("n"));
    // Triggered → MigratingVm（无前置条件）。
    task = task.advance().unwrap();
    // MigratingVm → SwitchingVip（需先 record_migrated_vms）。
    task = task.record_migrated_vms(vec![]).advance().unwrap();
    // SwitchingVip → PromotingReplica（需先 record_vip_moved(true)）。
    task = task.record_vip_moved(true).advance().unwrap();
    // PromotingReplica → Done（需先 record_replica_promoted(true)）。
    task = task.record_replica_promoted(true).advance().unwrap();
    assert_eq!(task.phase, FailoverPhase::Done);
    assert!(task.is_terminal());
    // 终态再 advance → TerminalReached。
    let err = task.advance();
    assert!(err.is_err());
    let msg = format!("{}", err.unwrap_err());
    assert!(
        msg.contains("终态") || msg.contains("TerminalReached"),
        "应报终态不可推进: {msg}"
    );
}

#[tokio::test]
async fn vip_assign_idempotent_after_reassign() {
    // 验证 VIP 多次 assign 覆盖 owner（模拟 leader 切换）。
    let vip = InMemoryVipManager::new();
    vip.assign(&NodeId::new("node-a")).await.unwrap();
    assert_eq!(
        vip.current_owner().as_ref().map(|n| n.as_str()),
        Some("node-a")
    );
    vip.assign(&NodeId::new("node-b")).await.unwrap();
    assert_eq!(
        vip.current_owner().as_ref().map(|n| n.as_str()),
        Some("node-b")
    );
    assert_eq!(vip.assign_count(), 2);
    vip.release().await.unwrap();
    assert!(vip.current_owner().is_none());
}

// ----------------------------------------------------------------------------
// 冲突版 VipManager（assign 恒返回 Err，模拟 VIP 被其他节点持有）
// ----------------------------------------------------------------------------

struct ConflictVipManager;

#[async_trait]
impl VipManager for ConflictVipManager {
    async fn assign(&self, _node: &NodeId) -> Result<(), os_meta::MetaError> {
        Err(os_meta::MetaError::VipConflict("VIP 被其他节点持有".into()))
    }
    async fn release(&self) -> Result<(), os_meta::MetaError> {
        Ok(())
    }
    async fn current_owner(&self) -> Option<NodeId> {
        None
    }
}

// ----------------------------------------------------------------------------
// 占位：Replication trait 在本测试用例中尚未通过 mock 注入（MockStorageBackend
// 未 impl Replication）。本测试用 list_snapshots 占位「副本提升」调用，
// 同时静态验证 Replication trait 签名可被定义（编译期检查跨 crate 兼容）。
// ----------------------------------------------------------------------------

#[allow(dead_code)]
fn _replication_trait_signature_check() {
    // 占位：证明 os-storage::StorageError 跨 crate 可构造。
    // 注：os-storage::Replication 是**原生 async fn in trait**（非 dyn 兼容，
    // ADR-COMPAT-001），无法用 #[async_trait] 桩实现，故这里只静态引用类型符号，
    // 不构造实现。真实「副本提升」侧由 storage-agent 在 os-storage 内部接通
    // （ZfsSendRecv），集成测侧只能经 StorageBackend 的 list_snapshots 占位调用
    // （见 IntegratedFailoverOrchestrator::drive_failover 的 PromotingReplica 阶段）。
    let _ = StorageError::CommandFailed("test".into());
    // 引用 trait 符号（编译期校验可见性）。
    fn _ref<T: Replication>() {}
    _ref::<os_storage::ZfsSendRecv>();
}
