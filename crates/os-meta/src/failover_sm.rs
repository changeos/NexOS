//! 故障转移状态机——纯状态转换（无 IO / 无外部 crate 依赖）。
//!
//! 规格（启动 prompt）：`FailoverTask` 状态机
//! `Triggered → MigratingVm → SwitchingVip → PromotingReplica → Done/Failed`。
//!
//! 本模块提供该状态机的纯函数式实现，可独立测试。`HaFailoverOrchestrator`
//! 在异步任务中驱动这个状态机：每个阶段执行完毕后调用 `advance` 推进。
//! - 阶段失败 → `mark_failed` 进入 `Failed`（终态）
//! - 节点恢复或人工干预 → `abort` 进入 `Aborted`（终态）
//!
//! 与契约 `FailoverStatus` 的映射（见 failover.rs）：
//! - `Triggered`/`MigratingVm`/`SwitchingVip`/`PromotingReplica` → `FailoverStatus::Running { progress }`
//! - `Done` → `FailoverStatus::Completed`
//! - `Failed` → `FailoverStatus::Failed { reason }`
//! - `Aborted` → `FailoverStatus::Aborted`

use os_core::{Deserialize, NodeId, Serialize, TaskId, Utc, VmId};

use crate::failover::FailoverStatus;

// ----------------------------------------------------------------------------
// FailoverPhase（状态机阶段）
// ----------------------------------------------------------------------------

/// 故障转移阶段（线性推进，每阶段不可回退）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailoverPhase {
    /// 已触发，尚未开始执行
    Triggered,
    /// 迁移失效节点上的 VM（编排 os-compute VmManager）
    MigratingVm,
    /// 切换 VIP 到新 owner（编排本 crate VipManager）
    SwitchingVip,
    /// 提升副本为新主（编排 os-storage Replication）
    PromotingReplica,
    /// 完成（终态）
    Done,
    /// 失败（终态）
    Failed,
    /// 已中止（终态）
    Aborted,
}

impl FailoverPhase {
    /// 是否终态（Done/Failed/Aborted）。
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Aborted)
    }

    /// 该阶段映射到的整体进度（0.0..=1.0），用于 `FailoverStatus::Running.progress`。
    pub fn progress(self) -> f32 {
        match self {
            Self::Triggered => 0.0,
            Self::MigratingVm => 0.25,
            Self::SwitchingVip => 0.55,
            Self::PromotingReplica => 0.85,
            Self::Done => 1.0,
            Self::Failed | Self::Aborted => 1.0,
        }
    }

    /// 下一阶段（仅非终态有）；终态返回自身。
    pub fn next(self) -> Self {
        match self {
            Self::Triggered => Self::MigratingVm,
            Self::MigratingVm => Self::SwitchingVip,
            Self::SwitchingVip => Self::PromotingReplica,
            Self::PromotingReplica => Self::Done,
            Self::Done | Self::Failed | Self::Aborted => self,
        }
    }
}

// ----------------------------------------------------------------------------
// FailoverTask（状态机实例）
// ----------------------------------------------------------------------------

/// 故障转移任务（状态机实例 + 上下文）。
///
/// 不可变快照语义：每次推进返回新的 `FailoverTask`，便于无锁快照查询。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailoverTask {
    /// 任务 ID（用于上层轮询）
    pub task_id: TaskId,
    /// 失效节点
    pub failed_node: NodeId,
    /// 当前阶段
    pub phase: FailoverPhase,
    /// 已迁移的 VM
    pub migrated_vms: Vec<VmId>,
    /// 是否已切 VIP
    pub moved_vip: bool,
    /// 已提升副本（PromotingReplica 完成后置 true）
    pub promoted_replica: bool,
    /// 失败原因（仅 Failed 阶段有意义）
    pub failure_reason: Option<String>,
    /// 触发时间
    pub started_at: os_core::DateTime,
}

impl FailoverTask {
    /// 创建初始任务（Triggered 阶段）。
    pub fn new(failed_node: NodeId) -> Self {
        Self {
            task_id: TaskId::new(),
            failed_node,
            phase: FailoverPhase::Triggered,
            migrated_vms: Vec::new(),
            moved_vip: false,
            promoted_replica: false,
            failure_reason: None,
            started_at: Utc::now(),
        }
    }

    /// 推进到下一阶段。
    ///
    /// - 终态调用返回 `Err`（不可推进）；
    /// - `MigratingVm` 推进前须先经 `record_migrated_vms` 设置迁移结果（至少 1 个或显式确认空）；
    /// - `SwitchingVip` 推进前须 `record_vip_moved(true)`；
    /// - `PromotingReplica` 推进前须 `record_replica_promoted(true)`。
    pub fn advance(self) -> Result<Self, FailoverTransitionError> {
        if self.phase.is_terminal() {
            return Err(FailoverTransitionError::TerminalReached(self.phase));
        }
        // 前置条件校验（保证状态机一致性）
        self.ensure_preconditions()?;
        Ok(FailoverTask {
            phase: self.phase.next(),
            ..self
        })
    }

    /// 标记失败（任意非终态阶段 → Failed）。
    pub fn mark_failed(self, reason: impl Into<String>) -> Self {
        FailoverTask {
            phase: FailoverPhase::Failed,
            failure_reason: Some(reason.into()),
            ..self
        }
    }

    /// 中止（任意非终态 → Aborted）。
    pub fn abort(self) -> Self {
        FailoverTask {
            phase: FailoverPhase::Aborted,
            ..self
        }
    }

    /// 记录已迁移的 VM 列表（MigratingVm 阶段调用）。
    pub fn record_migrated_vms(self, vms: Vec<VmId>) -> Self {
        FailoverTask {
            migrated_vms: vms,
            ..self
        }
    }

    /// 记录 VIP 是否已漂移（SwitchingVip 阶段调用）。
    pub fn record_vip_moved(self, moved: bool) -> Self {
        FailoverTask {
            moved_vip: moved,
            ..self
        }
    }

    /// 记录副本是否已提升（PromotingReplica 阶段调用）。
    pub fn record_replica_promoted(self, promoted: bool) -> Self {
        FailoverTask {
            promoted_replica: promoted,
            ..self
        }
    }

    /// 当前阶段是否终态。
    pub fn is_terminal(&self) -> bool {
        self.phase.is_terminal()
    }

    /// 转换为对外 `FailoverStatus`（契约枚举）。
    pub fn to_status(&self) -> FailoverStatus {
        match self.phase {
            FailoverPhase::Triggered
            | FailoverPhase::MigratingVm
            | FailoverPhase::SwitchingVip
            | FailoverPhase::PromotingReplica => FailoverStatus::Running {
                progress: self.phase.progress(),
            },
            FailoverPhase::Done => FailoverStatus::Completed,
            FailoverPhase::Failed => FailoverStatus::Failed {
                reason: self
                    .failure_reason
                    .clone()
                    .unwrap_or_else(|| "未知".to_string()),
            },
            FailoverPhase::Aborted => FailoverStatus::Aborted,
        }
    }

    // 校验阶段推进前置条件
    fn ensure_preconditions(&self) -> Result<(), FailoverTransitionError> {
        match self.phase {
            FailoverPhase::SwitchingVip if !self.moved_vip => {
                Err(FailoverTransitionError::MissingPrecondition(
                    "SwitchingVip 推进前须 record_vip_moved(true)".into(),
                ))
            }
            FailoverPhase::PromotingReplica if !self.promoted_replica => {
                Err(FailoverTransitionError::MissingPrecondition(
                    "PromotingReplica 推进前须 record_replica_promoted(true)".into(),
                ))
            }
            _ => Ok(()),
        }
    }
}

// ----------------------------------------------------------------------------
// 错误
// ----------------------------------------------------------------------------

/// 状态机转换错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FailoverTransitionError {
    /// 已到终态，不可推进
    #[error("已到终态 {0:?}，不可推进")]
    TerminalReached(FailoverPhase),
    /// 推进前置条件未满足
    #[error("推进前置条件未满足: {0}")]
    MissingPrecondition(String),
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn node(s: &str) -> NodeId {
        NodeId::new(s)
    }
    fn vm(s: &str) -> VmId {
        VmId::new(s)
    }

    #[test]
    fn phase_next_chain() {
        assert_eq!(FailoverPhase::Triggered.next(), FailoverPhase::MigratingVm);
        assert_eq!(
            FailoverPhase::MigratingVm.next(),
            FailoverPhase::SwitchingVip
        );
        assert_eq!(
            FailoverPhase::SwitchingVip.next(),
            FailoverPhase::PromotingReplica
        );
        assert_eq!(FailoverPhase::PromotingReplica.next(), FailoverPhase::Done);
        assert_eq!(FailoverPhase::Done.next(), FailoverPhase::Done);
    }

    #[test]
    fn phase_is_terminal() {
        assert!(!FailoverPhase::Triggered.is_terminal());
        assert!(!FailoverPhase::MigratingVm.is_terminal());
        assert!(FailoverPhase::Done.is_terminal());
        assert!(FailoverPhase::Failed.is_terminal());
        assert!(FailoverPhase::Aborted.is_terminal());
    }

    #[test]
    fn phase_progress_monotonic() {
        let mut prev = -1.0f32;
        for p in [
            FailoverPhase::Triggered,
            FailoverPhase::MigratingVm,
            FailoverPhase::SwitchingVip,
            FailoverPhase::PromotingReplica,
            FailoverPhase::Done,
        ] {
            assert!(p.progress() > prev, "{:?} progress not monotonic", p);
            prev = p.progress();
        }
    }

    #[test]
    fn full_happy_path() {
        let t0 = FailoverTask::new(node("n1"));
        assert_eq!(t0.phase, FailoverPhase::Triggered);

        // Triggered → MigratingVm（无需前置）
        let t1 = t0.advance().expect("triggered -> migrating");
        assert_eq!(t1.phase, FailoverPhase::MigratingVm);

        // MigratingVm → SwitchingVip（记录迁移结果，无需校验数量；空列表也合法）
        let t2 = t1
            .record_migrated_vms(vec![vm("v1"), vm("v2")])
            .advance()
            .expect("migrating -> switching");
        assert_eq!(t2.phase, FailoverPhase::SwitchingVip);
        assert_eq!(t2.migrated_vms.len(), 2);

        // SwitchingVip → PromotingReplica（须 record_vip_moved(true)）
        let t3 = t2
            .record_vip_moved(true)
            .advance()
            .expect("switching -> promoting");
        assert_eq!(t3.phase, FailoverPhase::PromotingReplica);
        assert!(t3.moved_vip);

        // PromotingReplica → Done（须 record_replica_promoted(true)）
        let t4 = t3
            .record_replica_promoted(true)
            .advance()
            .expect("promoting -> done");
        assert_eq!(t4.phase, FailoverPhase::Done);
        assert!(t4.is_terminal());
    }

    #[test]
    fn advance_blocked_when_precondition_missing() {
        // SwitchingVip 未 record_vip_moved(true) → 拒绝推进
        let t = FailoverTask {
            phase: FailoverPhase::SwitchingVip,
            moved_vip: false,
            ..FailoverTask::new(node("n1"))
        };
        assert!(matches!(
            t.advance(),
            Err(FailoverTransitionError::MissingPrecondition(_))
        ));

        // PromotingReplica 未 record_replica_promoted(true) → 拒绝推进
        let t2 = FailoverTask {
            phase: FailoverPhase::PromotingReplica,
            promoted_replica: false,
            ..FailoverTask::new(node("n1"))
        };
        assert!(matches!(
            t2.advance(),
            Err(FailoverTransitionError::MissingPrecondition(_))
        ));
    }

    #[test]
    fn advance_blocked_on_terminal() {
        for phase in [
            FailoverPhase::Done,
            FailoverPhase::Failed,
            FailoverPhase::Aborted,
        ] {
            let t = FailoverTask {
                phase,
                ..FailoverTask::new(node("n1"))
            };
            assert!(matches!(
                t.advance(),
                Err(FailoverTransitionError::TerminalReached(_))
            ));
        }
    }

    #[test]
    fn mark_failed_from_any_active() {
        let t = FailoverTask::new(node("n1"))
            .record_migrated_vms(vec![vm("v1")])
            .mark_failed("VM 迁移超时");
        assert_eq!(t.phase, FailoverPhase::Failed);
        assert_eq!(t.failure_reason.as_deref(), Some("VM 迁移超时"));
        assert!(t.is_terminal());
    }

    #[test]
    fn abort_sets_aborted() {
        let t = FailoverTask::new(node("n1")).abort();
        assert_eq!(t.phase, FailoverPhase::Aborted);
        assert!(t.is_terminal());
    }

    #[test]
    fn to_status_mapping() {
        assert!(matches!(
            FailoverTask::new(node("n1")).to_status(),
            FailoverStatus::Running { .. }
        ));
        let done = FailoverTask {
            phase: FailoverPhase::Done,
            ..FailoverTask::new(node("n1"))
        };
        assert!(matches!(done.to_status(), FailoverStatus::Completed));

        let failed = FailoverTask {
            phase: FailoverPhase::Failed,
            failure_reason: Some("err".into()),
            ..FailoverTask::new(node("n1"))
        };
        assert!(matches!(failed.to_status(), FailoverStatus::Failed { .. }));

        let aborted = FailoverTask {
            phase: FailoverPhase::Aborted,
            ..FailoverTask::new(node("n1"))
        };
        assert!(matches!(aborted.to_status(), FailoverStatus::Aborted));
    }

    #[test]
    fn running_progress_uses_phase_progress() {
        let t = FailoverTask {
            phase: FailoverPhase::SwitchingVip,
            ..FailoverTask::new(node("n1"))
        };
        match t.to_status() {
            FailoverStatus::Running { progress } => {
                assert!((progress - FailoverPhase::SwitchingVip.progress()).abs() < f32::EPSILON);
            }
            _ => panic!("expected Running"),
        }
    }
}
