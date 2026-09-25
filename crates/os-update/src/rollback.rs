//! 回滚管理（规划文档 §3.12）
//!
//! A/B 槽位 + watchdog：新槽激活后启动探活，失败自动回滚到上一个健康槽。

use os_core::{DateTime, HealthReport};
use serde::{Deserialize, Serialize};

use crate::update::UpdateSlot;

// ----------------------------------------------------------------------------
// 回滚点
// ----------------------------------------------------------------------------

/// 回滚点（历史健康槽快照）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackPoint {
    /// 槽位
    pub slot: UpdateSlot,
    /// 该槽的系统版本
    pub version: String,
    /// 创建时间（UTC）
    pub created_at: DateTime,
    /// 是否标记为健康
    pub healthy: bool,
}

// ----------------------------------------------------------------------------
// RollbackManager trait（async）
// ----------------------------------------------------------------------------

/// 回滚管理器——列出/选择/校验回滚点，并支持 watchdog 自动回滚。
///
/// 实现者：`AbRollbackManager`（默认）。配合 bootloader 双槽引导。
#[allow(async_fn_in_trait)]
pub trait RollbackManager: Send + Sync {
    /// 列出所有可回滚点。
    async fn list_snapshots(&self) -> Vec<RollbackPoint>;

    /// 回滚到指定点（切回旧槽）。
    async fn rollback_to(&self, point: &RollbackPoint) -> Result<(), crate::UpdateError>;

    /// 探活当前槽位健康（启动后调用，失败触发自动回滚）。
    async fn verify_current_health(&self) -> Result<HealthReport, crate::UpdateError>;

    /// watchdog 自动回滚——若当前不健康则切回上一个健康槽；
    /// 返回 `true` 表示已发生回滚，`false` 表示无需回滚。
    async fn auto_rollback_if_unhealthy(&self) -> Result<bool, crate::UpdateError>;
}

// ============================================================================
// 纯逻辑：回滚策略 + 触发条件判定（无 bootloader/watchdog 依赖）
// ============================================================================

use os_core::{Health, NodeRole};

// ----------------------------------------------------------------------------
// 回滚策略
// ----------------------------------------------------------------------------

/// 回滚触发策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollbackPolicy {
    /// 完全自动：任何启动探活失败立即回滚（生产默认）。
    Automatic,
    /// 仅手动：探活失败仅告警，不自动回滚（维护窗口用，需运维确认）。
    Manual,
    /// watchdog 触发：探活超时/连续失败 N 次后由 systemd-watchdog 触发回滚。
    /// `max_failures`：连续失败次数阈值（≥1）。
    Watchdog {
        /// 连续失败次数阈值
        max_failures: u32,
    },
}

impl Default for RollbackPolicy {
    /// 默认 Automatic（安全优先：新槽不健康立即回退旧槽）。
    fn default() -> Self {
        Self::Automatic
    }
}

// ----------------------------------------------------------------------------
// 回滚触发判定（纯函数）
// ----------------------------------------------------------------------------

/// 回滚触发判定所需的上下文。
#[derive(Debug, Clone)]
pub struct RollbackContext<'a> {
    /// 当前槽位健康状态（探活结果）
    pub health: Health,
    /// 当前策略
    pub policy: RollbackPolicy,
    /// 已连续探活失败次数（Watchdog 策略用）
    pub consecutive_failures: u32,
    /// 是否存在可回滚的上一健康槽（None = 无回滚目标，首启）
    pub has_rollback_target: bool,
    /// 当前节点角色（leader 回滚需特别谨慎，可能触发 failover）
    pub node_role: Option<NodeRole>,
    _a: std::marker::PhantomData<&'a ()>,
}

impl<'a> RollbackContext<'a> {
    /// 构造回滚判定上下文。
    #[must_use]
    pub fn new(
        health: Health,
        policy: RollbackPolicy,
        consecutive_failures: u32,
        has_rollback_target: bool,
    ) -> Self {
        Self {
            health,
            policy,
            consecutive_failures,
            has_rollback_target,
            node_role: None,
            _a: std::marker::PhantomData,
        }
    }

    /// 附加节点角色（用于 leader 特判日志，不影响决策本身）。
    #[must_use]
    pub fn with_role(mut self, role: NodeRole) -> Self {
        self.node_role = Some(role);
        self
    }
}

/// 回滚决策结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RollbackDecision {
    /// 应触发回滚。
    RollbackNow {
        /// 触发原因
        reason: String,
    },
    /// 不回滚（健康或策略不触发）。
    NoRollback {
        /// 不触发的原因说明
        reason: String,
    },
    /// 策略要求人工确认（Manual 策略下探活失败）。
    ManualConfirmationRequired {
        /// 告警说明
        message: String,
    },
}

/// 判定是否应触发回滚（纯函数）。
///
/// 决策逻辑（呼应 §3.12 watchdog 自动回滚）：
/// 1. 健康为 Healthy/Unknown（探测未完成）→ 不回滚。
/// 2. Degraded（降级）→ 不自动回滚（部分功能不可用但系统可用，告警即可）。
/// 3. Unhealthy：
///    - Automatic：有回滚目标 → 立即回滚；无目标 → NoRollback（无可退路）。
///    - Manual：不自动回滚，返回 ManualConfirmationRequired。
///    - Watchdog{max}：连续失败 ≥ max 且有目标 → 回滚；否则累计计数（NoRollback）。
/// 4. 无回滚目标（首启/无历史健康槽）：任何策略都不回滚（无路可退），返回 NoRollback。
#[must_use]
pub fn should_rollback(ctx: &RollbackContext<'_>) -> RollbackDecision {
    // 无回滚目标：任何不健康都不回滚（无路可退）
    if !ctx.has_rollback_target {
        return RollbackDecision::NoRollback {
            reason: "无可用回滚目标（首启或无历史健康槽），无法自动回滚".to_string(),
        };
    }
    match ctx.health {
        Health::Healthy => RollbackDecision::NoRollback {
            reason: "当前槽位健康，无需回滚".to_string(),
        },
        Health::Unknown => RollbackDecision::NoRollback {
            reason: "探活未完成/超时（Unknown），暂不回滚".to_string(),
        },
        Health::Degraded => RollbackDecision::NoRollback {
            reason: "降级（部分功能不可用）但系统可用，告警不回滚".to_string(),
        },
        Health::Unhealthy => match ctx.policy {
            RollbackPolicy::Automatic => RollbackDecision::RollbackNow {
                reason: "Automatic 策略：探活 Unhealthy，立即回滚到上一个健康槽".to_string(),
            },
            RollbackPolicy::Manual => RollbackDecision::ManualConfirmationRequired {
                message: "Manual 策略：探活 Unhealthy，需运维确认是否回滚".to_string(),
            },
            RollbackPolicy::Watchdog { max_failures } => {
                if ctx.consecutive_failures >= max_failures {
                    RollbackDecision::RollbackNow {
                        reason: format!(
                            "Watchdog 策略：连续失败 {} 次（阈值 {max_failures}），触发回滚",
                            ctx.consecutive_failures
                        ),
                    }
                } else {
                    RollbackDecision::NoRollback {
                        reason: format!(
                            "Watchdog 策略：连续失败 {} 次 < 阈值 {max_failures}，累计计数暂不回滚",
                            ctx.consecutive_failures
                        ),
                    }
                }
            }
        },
    }
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use os_core::Health;

    fn ctx(
        health: Health,
        policy: RollbackPolicy,
        failures: u32,
        has_target: bool,
    ) -> RollbackContext<'static> {
        RollbackContext::new(health, policy, failures, has_target)
    }

    // —— 健康状态分支 ——

    #[test]
    fn healthy_never_rolls_back() {
        for policy in [
            RollbackPolicy::Automatic,
            RollbackPolicy::Manual,
            RollbackPolicy::Watchdog { max_failures: 1 },
        ] {
            let d = should_rollback(&ctx(Health::Healthy, policy, 0, true));
            assert!(
                matches!(d, RollbackDecision::NoRollback { .. }),
                "{policy:?}"
            );
        }
    }

    #[test]
    fn degraded_does_not_rollback() {
        let d = should_rollback(&ctx(Health::Degraded, RollbackPolicy::Automatic, 0, true));
        assert!(matches!(d, RollbackDecision::NoRollback { .. }));
    }

    #[test]
    fn unknown_does_not_rollback() {
        let d = should_rollback(&ctx(Health::Unknown, RollbackPolicy::Automatic, 0, true));
        assert!(matches!(d, RollbackDecision::NoRollback { .. }));
    }

    // —— Automatic ——

    #[test]
    fn automatic_unhealthy_rolls_back_with_target() {
        let d = should_rollback(&ctx(Health::Unhealthy, RollbackPolicy::Automatic, 1, true));
        assert!(matches!(d, RollbackDecision::RollbackNow { .. }));
    }

    #[test]
    fn automatic_unhealthy_no_target_skips() {
        let d = should_rollback(&ctx(Health::Unhealthy, RollbackPolicy::Automatic, 1, false));
        assert!(matches!(d, RollbackDecision::NoRollback { .. }));
    }

    // —— Manual ——

    #[test]
    fn manual_unhealthy_requires_confirmation() {
        let d = should_rollback(&ctx(Health::Unhealthy, RollbackPolicy::Manual, 5, true));
        assert!(matches!(
            d,
            RollbackDecision::ManualConfirmationRequired { .. }
        ));
    }

    // —— Watchdog ——

    #[test]
    fn watchdog_below_threshold_no_rollback() {
        let d = should_rollback(&ctx(
            Health::Unhealthy,
            RollbackPolicy::Watchdog { max_failures: 3 },
            2,
            true,
        ));
        assert!(matches!(d, RollbackDecision::NoRollback { .. }));
    }

    #[test]
    fn watchdog_at_threshold_rolls_back() {
        let d = should_rollback(&ctx(
            Health::Unhealthy,
            RollbackPolicy::Watchdog { max_failures: 3 },
            3,
            true,
        ));
        assert!(matches!(d, RollbackDecision::RollbackNow { .. }));
    }

    #[test]
    fn watchdog_above_threshold_rolls_back() {
        let d = should_rollback(&ctx(
            Health::Unhealthy,
            RollbackPolicy::Watchdog { max_failures: 2 },
            5,
            true,
        ));
        assert!(matches!(d, RollbackDecision::RollbackNow { .. }));
    }

    #[test]
    fn watchdog_unhealthy_no_target_skips() {
        let d = should_rollback(&ctx(
            Health::Unhealthy,
            RollbackPolicy::Watchdog { max_failures: 1 },
            10,
            false,
        ));
        assert!(matches!(d, RollbackDecision::NoRollback { .. }));
    }

    // —— 无目标优先 ——

    #[test]
    fn no_target_overrides_all_unhealthy_policies() {
        for policy in [
            RollbackPolicy::Automatic,
            RollbackPolicy::Watchdog { max_failures: 1 },
        ] {
            let d = should_rollback(&ctx(Health::Unhealthy, policy, 99, false));
            assert!(
                matches!(d, RollbackDecision::NoRollback { .. }),
                "{policy:?}"
            );
        }
    }

    // —— 默认策略 ——

    #[test]
    fn default_policy_is_automatic() {
        assert_eq!(RollbackPolicy::default(), RollbackPolicy::Automatic);
    }

    #[test]
    fn context_with_role_attaches() {
        let c =
            ctx(Health::Healthy, RollbackPolicy::Automatic, 0, true).with_role(NodeRole::Leader);
        assert_eq!(c.node_role, Some(NodeRole::Leader));
        // 角色不影响决策
        assert!(matches!(
            should_rollback(&c),
            RollbackDecision::NoRollback { .. }
        ));
    }
}
