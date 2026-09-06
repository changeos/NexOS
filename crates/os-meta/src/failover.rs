//! 故障转移——HA 失效节点恢复
//!
//! 决策依据：规划文档 §3.5 —— 节点故障时由 leader 编排异步故障转移任务：
//! 迁移该节点上的 VM（复用 os-compute）、切 VIP（复用本 crate vip 模块）、
//! 提升副本（复用 os-storage Replication）。整个流程作为 Task 异步执行，
//! 上层轮询 `failover_status` 获取进度。

use async_trait::async_trait;
use os_core::{DateTime, Deserialize, NodeId, Serialize, TaskId, Utc, VmId};
// 注：VmId 本身定义于 os-core::ids；本 crate 经 os-compute 间接复用（见 Cargo.toml），
//   迁移 VM 的实际执行方为 os-compute（Hypervisor），此处仅用其领域 ID 类型。

// ----------------------------------------------------------------------------
// FailoverEvent / FailoverStatus
// ----------------------------------------------------------------------------

/// 故障转移事件（一次节点失效触发的完整恢复记录）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailoverEvent {
    /// 失效节点
    pub failed_node: NodeId,
    /// 失效原因（探活超时 / 进程崩溃 / 存储异常 等）
    pub reason: String,
    /// 触发时间
    pub timestamp: DateTime,
    /// 已迁移的 VM 列表
    pub migrated_vms: Vec<VmId>,
    /// 是否已切换 VIP（true = 已漂移到新 owner）
    pub moved_vip: bool,
}

/// 故障转移任务状态
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum FailoverStatus {
    /// 已入队待执行
    Pending,
    /// 执行中（progress 为 0.0..=1.0）
    Running {
        /// 整体进度（0.0 = 刚开始，1.0 = 即将完成）
        progress: f32,
    },
    /// 已完成（所有 VM 迁移 + VIP 切换 + 副本提升成功）
    Completed,
    /// 失败（含失败原因）
    Failed {
        /// 失败原因
        reason: String,
    },
    /// 已中止（如节点恢复或人工干预）
    Aborted,
}

impl FailoverStatus {
    /// 构造一个初始 Pending 状态
    pub fn pending() -> Self {
        Self::Pending
    }

    /// 构造一个当前时间戳的初始事件壳（迁移结果待填充）
    pub fn new_event(node: NodeId, reason: impl Into<String>) -> FailoverEvent {
        FailoverEvent {
            failed_node: node,
            reason: reason.into(),
            timestamp: Utc::now(),
            migrated_vms: Vec::new(),
            moved_vip: false,
        }
    }
}

// ----------------------------------------------------------------------------
// FailoverOrchestrator trait（async）
// ----------------------------------------------------------------------------

/// 故障转移编排器——检测失效节点并编排异步恢复任务。
///
/// 实现者：`HaFailoverOrchestrator`（默认，由 leader 节点运行）；
/// 协调 os-compute（VM 迁移）、本 crate vip 模块（VIP 切换）、os-storage（副本提升）。
///
/// 注：按 ADR-COMPAT-001，本 trait 经 `Box<dyn FailoverOrchestrator>` 运行期多态
/// （见 mock.rs `_assert_dyn_compatible`），故用 `#[async_trait]`；方法签名未变。
#[async_trait]
pub trait FailoverOrchestrator: Send + Sync {
    /// 探活——对指定节点做活性探测。
    /// 返回 `Ok(Some(reason))` 表示判定失效（含原因）；`Ok(None)` 表示存活。
    async fn detect_failure(&self, node: &NodeId) -> Result<Option<String>, crate::MetaError>;

    /// 触发故障转移（异步任务：迁移 VM + 切 VIP + 提升副本）。
    /// 返回关联 TaskId，上层据此轮询 `failover_status`。
    async fn trigger_failover(&self, failed: &NodeId) -> Result<TaskId, crate::MetaError>;

    /// 查询故障转移任务进度。
    async fn failover_status(&self, task: &TaskId) -> FailoverStatus;
}

// ----------------------------------------------------------------------------
// 单元测试：FailoverEvent / FailoverStatus 构造 + serde 往返
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use os_core::{NodeId, VmId};

    // ---- FailoverStatus ----

    #[test]
    fn failover_status_pending_constructor() {
        assert!(matches!(FailoverStatus::pending(), FailoverStatus::Pending));
    }

    #[test]
    fn failover_status_all_variants_serde_roundtrip() {
        let cases = vec![
            serde_json::json!({"phase": "pending"}),
            serde_json::json!({"phase": "running", "progress": 0.5}),
            serde_json::json!({"phase": "completed"}),
            serde_json::json!({"phase": "failed", "reason": "vm 迁移超时"}),
            serde_json::json!({"phase": "aborted"}),
        ];
        for (i, json) in cases.into_iter().enumerate() {
            let s: FailoverStatus =
                serde_json::from_value(json).unwrap_or_else(|e| panic!("#{i}: {e}"));
            let back = serde_json::to_string(&s).unwrap();
            let _: FailoverStatus = serde_json::from_str(&back).unwrap();
        }
    }

    #[test]
    fn failover_status_running_serializes_with_progress() {
        let s = FailoverStatus::Running { progress: 0.75 };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"progress\":0.75"));
        assert!(json.contains("\"phase\":\"running\""));
    }

    #[test]
    fn failover_status_failed_serializes_with_reason() {
        let s = FailoverStatus::Failed {
            reason: "vip 冲突".into(),
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"reason\":\"vip 冲突\""));
        assert!(json.contains("\"phase\":\"failed\""));
    }

    #[test]
    fn failover_status_tag_is_phase_renamed_snake() {
        // serde(tag = "phase", rename_all = "snake_case")
        assert!(serde_json::to_string(&FailoverStatus::Pending)
            .unwrap()
            .contains("\"phase\":\"pending\""));
        assert!(serde_json::to_string(&FailoverStatus::Completed)
            .unwrap()
            .contains("\"phase\":\"completed\""));
    }

    #[test]
    fn failover_status_clone_preserves_variant() {
        let s = FailoverStatus::Running { progress: 0.3 };
        let c = s.clone();
        match c {
            FailoverStatus::Running { progress } => assert!((progress - 0.3).abs() < f32::EPSILON),
            _ => panic!("clone 应保持 Running"),
        }
    }

    // ---- FailoverEvent ----

    #[test]
    fn failover_new_event_initializes_empty_results() {
        let node = NodeId::new("n1");
        let e = FailoverStatus::new_event(node.clone(), "心跳超时");
        assert_eq!(e.failed_node, node);
        assert_eq!(e.reason, "心跳超时");
        assert!(e.migrated_vms.is_empty(), "初始事件不应有迁移结果");
        assert!(!e.moved_vip, "初始事件 moved_vip 应为 false");
        // timestamp 应被填充
        let _ = e.timestamp;
    }

    #[test]
    fn failover_new_event_accepts_string_reason() {
        let e = FailoverStatus::new_event(NodeId::new("n2"), String::from("进程崩溃"));
        assert_eq!(e.reason, "进程崩溃");
    }

    #[test]
    fn failover_event_serde_roundtrip_with_migrations() {
        let e = FailoverEvent {
            failed_node: NodeId::new("n1"),
            reason: "节点失效".into(),
            timestamp: Utc::now(),
            migrated_vms: vec![VmId::new("vm-1"), VmId::new("vm-2")],
            moved_vip: true,
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: FailoverEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.failed_node, e.failed_node);
        assert_eq!(back.reason, e.reason);
        assert_eq!(back.migrated_vms.len(), 2);
        assert!(back.moved_vip);
    }

    #[test]
    fn failover_event_serde_roundtrip_empty_migrations() {
        let e = FailoverEvent {
            failed_node: NodeId::new("n3"),
            reason: "维护关机".into(),
            timestamp: Utc::now(),
            migrated_vms: vec![],
            moved_vip: false,
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: FailoverEvent = serde_json::from_str(&json).unwrap();
        assert!(back.migrated_vms.is_empty());
        assert!(!back.moved_vip);
    }

    #[test]
    fn failover_event_clone_preserves_vms() {
        let e = FailoverEvent {
            failed_node: NodeId::new("n1"),
            reason: "x".into(),
            timestamp: Utc::now(),
            migrated_vms: vec![VmId::new("v1")],
            moved_vip: true,
        };
        let c = e.clone();
        assert_eq!(c.migrated_vms.len(), 1);
        assert!(c.moved_vip);
    }

    #[test]
    fn failover_status_pending_then_debug_format() {
        let s = FailoverStatus::pending();
        let dbg = format!("{s:?}");
        assert!(dbg.contains("Pending"));
    }
}
