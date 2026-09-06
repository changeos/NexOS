//! 联邦决策状态机（规划文档 §3.14）
//!
//! 把"发现 peer → 判资格 → 决策 → 配对 → 就绪"沉淀为显式状态机，便于：
//! - 单测覆盖每条转移路径（含降级路径）
//! - 上层（orchestrator/provision）据此驱动联邦流程，避免散乱条件分支
//!
//! 状态流转（主线 + 降级）：
//! ```text
//! Probing ──(发现 peer)──> Authenticating ──(mTLS ok)──> Qualifying
//!    │                          │                          │
//!    │                          │                          ├─(资格达标+HA)─> JoiningHa ─> Active(Ha)
//!    │                          │                          ├─(资格达标+Peer)────────────> Active(Peer)
//!    │                          │                          └─(不达标/Decline)──────────> Active(Standalone)
//!    │                          │
//!    │                          └─(mTLS fail)──> Active(Standalone)   // 降级：配对失败保持单机
//!    │
//!    └─(超时无 peer)──────────────────────────────────────────> Active(Standalone)
//! ```
//!
//! 本模块为纯逻辑状态机（不持有网络/IO），状态转移由显式事件驱动；上层 trait 实现
//! 调用 `on_*` 方法推进状态。状态机的核心价值是让降级路径可测、可审计。

use std::fmt;

use os_core::NodeId;

use crate::federation::{FederationAction, FederationChoice};

// ----------------------------------------------------------------------------
// 状态枚举
// ----------------------------------------------------------------------------

/// 联邦决策状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FederationState {
    /// 探测中（扫描 LAN，尚未确认任何 peer）
    Probing,
    /// 认证中（发现 peer，正在做 mTLS 双向认证）
    Authenticating,
    /// 资格判定中（mTLS 已通过，正在检测 HA 硬指标）
    Qualifying,
    /// 正在加入 HA 集群（已决策 JoinHaCluster，配对/握手进行中）
    JoiningHa,
    /// 已就绪（终态之一，含三种角色）
    Active(ActiveRole),
    /// 失败终态（不可恢复，需人工介入或重试）
    Failed,
}

/// 就绪后的角色（终态细分）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveRole {
    /// HA 集群成员
    Ha,
    /// 仅 peer 同步
    Peer,
    /// 独立单机（含降级到单机）
    Standalone,
}

impl fmt::Display for FederationState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Probing => f.write_str("Probing"),
            Self::Authenticating => f.write_str("Authenticating"),
            Self::Qualifying => f.write_str("Qualifying"),
            Self::JoiningHa => f.write_str("JoiningHa"),
            Self::Active(r) => write!(f, "Active({r:?})"),
            Self::Failed => f.write_str("Failed"),
        }
    }
}

impl FederationState {
    /// 是否为终态（Active / Failed）
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Active(_) | Self::Failed)
    }
}

// ----------------------------------------------------------------------------
// 驱动事件
// ----------------------------------------------------------------------------

/// 状态机驱动事件（上层 trait 实现产出后灌入状态机）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FederationEvent {
    /// 发现到 peer（携带对端 node_id）
    PeerDiscovered(NodeId),
    /// 未发现任何 peer（扫描超时）
    NoPeerFound,
    /// mTLS 认证成功
    AuthSucceeded,
    /// mTLS 认证失败（凭证无效/握手失败）—— 触发降级到单机
    AuthFailed,
    /// 资格检测结果就绪 + 用户选择（决策已完成）
    DecisionReady { action: FederationAction },
    /// HA 加入流程完成（JoiningHa → Active(Ha)）
    HaJoinCompleted,
    /// HA 加入流程失败 —— 降级为 peer 或单机
    HaJoinFailed { fallback_role: ActiveRole },
    /// 用户主动重置（回到 Probing）
    Reset,
}

// ----------------------------------------------------------------------------
// 状态机
// ----------------------------------------------------------------------------

/// 联邦决策状态机（纯逻辑，无 IO）
///
/// 用法：上层先 `state()` 取当前态；按业务进展调 `transition(event)` 得新态 +
/// 副作用指令（如"执行某 FederationAction"），由上层执行。
#[derive(Debug, Clone)]
pub struct FederationStateMachine {
    state: FederationState,
    /// 最近一次转移原因（便于审计/排障）
    last_reason: Option<String>,
}

/// 状态转移结果（新状态 + 可选的副作用提示）
#[derive(Debug, Clone, PartialEq)]
pub struct Transition {
    /// 转移后的新状态
    pub to: FederationState,
    /// 副作用：上层应执行的联邦动作（如触发 JoinHaCluster/StayStandalone）
    pub action_hint: Option<FederationAction>,
    /// 转移是否合法（非法事件在当前态下被忽略，状态不变）
    pub valid: bool,
}

impl Default for FederationStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl FederationStateMachine {
    /// 创建初始状态机（Probing）
    pub fn new() -> Self {
        Self {
            state: FederationState::Probing,
            last_reason: None,
        }
    }

    /// 当前状态
    pub fn state(&self) -> FederationState {
        self.state
    }

    /// 最近一次转移原因
    pub fn last_reason(&self) -> Option<&str> {
        self.last_reason.as_deref()
    }

    /// 应用一个事件，返回转移结果
    ///
    /// 合法性规则（非法事件被忽略，`valid=false`）：
    /// - 终态（Active/Failed）只接受 `Reset`
    /// - 每个非终态只接受其语义内的事件
    pub fn transition(&mut self, event: &FederationEvent) -> Transition {
        use FederationEvent as E;
        use FederationState as S;

        // 终态处理：仅 Reset 可离开终态
        if self.state.is_terminal() {
            if matches!(event, E::Reset) {
                self.set(S::Probing, "用户重置");
                return Transition {
                    to: S::Probing,
                    action_hint: None,
                    valid: true,
                };
            }
            return Transition {
                to: self.state,
                action_hint: None,
                valid: false,
            };
        }

        let (next, hint, reason, valid) = match (self.state, event) {
            (S::Probing, E::PeerDiscovered(_)) => (
                S::Authenticating,
                None,
                "发现 peer，进入认证".to_string(),
                true,
            ),
            (S::Probing, E::NoPeerFound) => (
                S::Active(ActiveRole::Standalone),
                Some(FederationAction::StayStandalone),
                "无 peer，保持单机".to_string(),
                true,
            ),
            (S::Authenticating, E::AuthSucceeded) => {
                (S::Qualifying, None, "mTLS 通过，判资格".to_string(), true)
            }
            // 降级路径：认证失败 → 单机
            (S::Authenticating, E::AuthFailed) => (
                S::Active(ActiveRole::Standalone),
                Some(FederationAction::StayStandalone),
                "mTLS 失败，降级单机".to_string(),
                true,
            ),
            (S::Qualifying, E::DecisionReady { action }) => {
                let (s, reason) = match action {
                    FederationAction::JoinHaCluster { .. } => {
                        (S::JoiningHa, "决策加入 HA".to_string())
                    }
                    FederationAction::RegisterAsPeer => {
                        (S::Active(ActiveRole::Peer), "决策为 peer".to_string())
                    }
                    FederationAction::StayStandalone => (
                        S::Active(ActiveRole::Standalone),
                        "决策保持单机".to_string(),
                    ),
                };
                (s, Some(action.clone()), reason, true)
            }
            (S::JoiningHa, E::HaJoinCompleted) => (
                S::Active(ActiveRole::Ha),
                None,
                "HA 加入完成".to_string(),
                true,
            ),
            // 降级路径：HA 加入失败 → fallback 角色（peer 或单机）
            (S::JoiningHa, E::HaJoinFailed { fallback_role }) => {
                let hint = match fallback_role {
                    ActiveRole::Peer => FederationAction::RegisterAsPeer,
                    _ => FederationAction::StayStandalone,
                };
                (
                    S::Active(*fallback_role),
                    Some(hint),
                    format!("HA 加入失败，降级为 {fallback_role:?}"),
                    true,
                )
            }
            // 其余事件在当前态非法 → 忽略
            _ => (self.state, None, String::new(), false),
        };

        if valid {
            self.set(next, &reason);
        }
        Transition {
            to: if valid { next } else { self.state },
            action_hint: hint,
            valid,
        }
    }

    fn set(&mut self, state: FederationState, reason: &str) {
        self.state = state;
        self.last_reason = if reason.is_empty() {
            None
        } else {
            Some(reason.to_string())
        };
    }
}

// ----------------------------------------------------------------------------
// 决策辅助：把 (eligibility, choice) 映射为 action（供 DefaultFederationPolicy.decide 用）
// ----------------------------------------------------------------------------

/// 根据 HA 资格与用户选择产出 FederationAction（纯函数，无 IO）
///
/// 决策矩阵（规格 §3.14）：
/// | choice       | eligible | action           |
/// |--------------|----------|------------------|
/// | Auto         | true     | JoinHaCluster    |
/// | Auto         | false    | StayStandalone   |
/// | ManualHa     | true     | JoinHaCluster    |
/// | ManualHa     | false    | StayStandalone（不可 HA 时降级）|
/// | ManualPeer   | *        | RegisterAsPeer   |
/// | Decline      | *        | StayStandalone   |
///
/// `leader_endpoint`：仅在产出 JoinHaCluster 时需要（由上层根据 peer 列表选择）。
pub fn decide_action(
    eligible: bool,
    choice: FederationChoice,
    leader_endpoint: Option<&str>,
) -> FederationAction {
    match choice {
        FederationChoice::Auto => {
            if eligible {
                FederationAction::JoinHaCluster {
                    leader_endpoint: leader_endpoint.unwrap_or("").to_string(),
                }
            } else {
                FederationAction::StayStandalone
            }
        }
        FederationChoice::ManualHa => {
            if eligible {
                FederationAction::JoinHaCluster {
                    leader_endpoint: leader_endpoint.unwrap_or("").to_string(),
                }
            } else {
                // 不满足 HA 硬指标 → 降级为单机（安全策略，不强行组集群）
                FederationAction::StayStandalone
            }
        }
        FederationChoice::ManualPeer => FederationAction::RegisterAsPeer,
        FederationChoice::Decline => FederationAction::StayStandalone,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use os_core::NodeId;

    fn discover() -> FederationEvent {
        FederationEvent::PeerDiscovered(NodeId::new("peer-1"))
    }

    #[test]
    fn main_path_to_ha() {
        let mut sm = FederationStateMachine::new();
        assert_eq!(sm.state(), FederationState::Probing);

        let t = sm.transition(&discover());
        assert!(t.valid);
        assert_eq!(t.to, FederationState::Authenticating);

        let t = sm.transition(&FederationEvent::AuthSucceeded);
        assert!(t.valid);
        assert_eq!(t.to, FederationState::Qualifying);

        let action = FederationAction::JoinHaCluster {
            leader_endpoint: "10.0.0.1:8443".into(),
        };
        let t = sm.transition(&FederationEvent::DecisionReady {
            action: action.clone(),
        });
        assert!(t.valid);
        assert_eq!(t.to, FederationState::JoiningHa);
        assert_eq!(t.action_hint, Some(action));

        let t = sm.transition(&FederationEvent::HaJoinCompleted);
        assert!(t.valid);
        assert_eq!(t.to, FederationState::Active(ActiveRole::Ha));
        assert!(t.to.is_terminal());
    }

    #[test]
    fn degraded_auth_fail_to_standalone() {
        let mut sm = FederationStateMachine::new();
        sm.transition(&discover());
        let t = sm.transition(&FederationEvent::AuthFailed);
        assert_eq!(t.to, FederationState::Active(ActiveRole::Standalone));
        assert!(matches!(
            t.action_hint,
            Some(FederationAction::StayStandalone)
        ));
    }

    #[test]
    fn degraded_no_peer_to_standalone() {
        let mut sm = FederationStateMachine::new();
        let t = sm.transition(&FederationEvent::NoPeerFound);
        assert_eq!(t.to, FederationState::Active(ActiveRole::Standalone));
    }

    #[test]
    fn degraded_ha_join_fail_to_peer() {
        let mut sm = FederationStateMachine::new();
        sm.transition(&discover());
        sm.transition(&FederationEvent::AuthSucceeded);
        sm.transition(&FederationEvent::DecisionReady {
            action: FederationAction::JoinHaCluster {
                leader_endpoint: "x".into(),
            },
        });
        let t = sm.transition(&FederationEvent::HaJoinFailed {
            fallback_role: ActiveRole::Peer,
        });
        assert_eq!(t.to, FederationState::Active(ActiveRole::Peer));
        assert!(matches!(
            t.action_hint,
            Some(FederationAction::RegisterAsPeer)
        ));
    }

    #[test]
    fn terminal_ignores_events_except_reset() {
        let mut sm = FederationStateMachine::new();
        sm.transition(&FederationEvent::NoPeerFound); // → Standalone 终态
        let t = sm.transition(&discover()); // 非法
        assert!(!t.valid);
        assert!(t.to.is_terminal());

        let t = sm.transition(&FederationEvent::Reset);
        assert!(t.valid);
        assert_eq!(t.to, FederationState::Probing);
    }

    #[test]
    fn decide_matrix() {
        use FederationAction::*;
        use FederationChoice::*;

        assert!(matches!(
            decide_action(true, Auto, Some("le:8443")),
            JoinHaCluster { .. }
        ));
        assert!(matches!(decide_action(false, Auto, None), StayStandalone));
        assert!(matches!(
            decide_action(true, ManualHa, Some("le")),
            JoinHaCluster { .. }
        ));
        // 不达标 ManualHa → 降级单机
        assert!(matches!(
            decide_action(false, ManualHa, None),
            StayStandalone
        ));
        assert!(matches!(
            decide_action(true, ManualPeer, None),
            RegisterAsPeer
        ));
        assert!(matches!(
            decide_action(false, ManualPeer, None),
            RegisterAsPeer
        ));
        assert!(matches!(decide_action(true, Decline, None), StayStandalone));
    }

    #[test]
    fn last_reason_recorded() {
        let mut sm = FederationStateMachine::new();
        sm.transition(&discover());
        assert!(sm.last_reason().unwrap().contains("认证"));
    }
}
