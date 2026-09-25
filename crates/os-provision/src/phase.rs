//! 迁移阶段模型——状态机（SystemInit → FileTransfer → ExcludeSensitive → FirstBoot）。
//!
//! 该状态机细化 [`crate::migration::MigrationStatus`] 的"阶段2 迁移"内部流程，
//! 把"手机换机式"迁移拆成 4 个有序子阶段，支持断点续传（见 [`crate::checkpoint`]）。
//!
//! 阶段顺序（`SystemInit` < `FileTransfer` < `ExcludeSensitive` < `FirstBoot`）：
//! 1. **SystemInit**：目标节点已就绪（由 [`crate::Provisioner`] 完成 PXE 自举 + 建池），
//!    进入迁移流程的起点。
//! 2. **FileTransfer**：迁移数据集（ZFS send/recv）+ 配置/共享/用户定义迁移包。
//! 3. **ExcludeSensitive**：在迁移包导入前，按 §3.19 排除清单过滤掉密钥/密码
//!    （见 [`crate::exclude`]）。**安全红线：此阶段不可跳过**。
//! 4. **FirstBoot**：目标节点首启：拉起 osd、强制重设 root 密码、重生成密钥、
//!    重新加入集群（由 leader 下发集群密钥）。
//!
//! 终态：`Completed`（成功）/ `Failed`（附原因，可 `resume` 续传）。

use serde::{Deserialize, Serialize};

// ----------------------------------------------------------------------------
// 阶段枚举
// ----------------------------------------------------------------------------

/// 迁移子阶段（细化 §3.10 阶段2 的内部状态）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MigrationPhase {
    /// ① 系统初始化已完成（起点）——目标节点已被自举起来，等待导入数据/配置。
    SystemInit,
    /// ② 文件/数据集传输中——ZFS send/recv + 迁移包导出。
    FileTransfer,
    /// ③ 敏感项排除过滤（§3.19）——导入前剥除密钥/密码。
    ExcludeSensitive,
    /// ④ 首启——拉起 osd、强制重设密码、重生成密钥、加入集群。
    FirstBoot,
}

impl MigrationPhase {
    /// 阶段序号（用于比较/排序，0-based）。
    pub fn ordinal(self) -> u8 {
        match self {
            MigrationPhase::SystemInit => 0,
            MigrationPhase::FileTransfer => 1,
            MigrationPhase::ExcludeSensitive => 2,
            MigrationPhase::FirstBoot => 3,
        }
    }

    /// 下一个阶段（None 表示已是末阶段 FirstBoot）。
    pub fn next(self) -> Option<MigrationPhase> {
        match self {
            MigrationPhase::SystemInit => Some(MigrationPhase::FileTransfer),
            MigrationPhase::FileTransfer => Some(MigrationPhase::ExcludeSensitive),
            MigrationPhase::ExcludeSensitive => Some(MigrationPhase::FirstBoot),
            MigrationPhase::FirstBoot => None,
        }
    }

    /// 全部阶段，按顺序。
    pub fn all() -> &'static [MigrationPhase] {
        &[
            MigrationPhase::SystemInit,
            MigrationPhase::FileTransfer,
            MigrationPhase::ExcludeSensitive,
            MigrationPhase::FirstBoot,
        ]
    }

    /// 是否安全敏感阶段（影响安全红线的过滤阶段）。
    pub fn is_security_sensitive(self) -> bool {
        matches!(self, MigrationPhase::ExcludeSensitive)
    }
}

impl std::fmt::Display for MigrationPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            MigrationPhase::SystemInit => "system_init",
            MigrationPhase::FileTransfer => "file_transfer",
            MigrationPhase::ExcludeSensitive => "exclude_sensitive",
            MigrationPhase::FirstBoot => "first_boot",
        };
        f.write_str(s)
    }
}

impl PartialOrd for MigrationPhase {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MigrationPhase {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.ordinal().cmp(&other.ordinal())
    }
}

// ----------------------------------------------------------------------------
// 阶段执行结果
// ----------------------------------------------------------------------------

/// 单个阶段执行后的状态推进结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhaseTransition {
    /// 阶段已完成，推进到下一阶段（None 表示已是末阶段，迁移整体完成）。
    Advance {
        /// 完成的阶段
        completed: MigrationPhase,
        /// 下一阶段（None = FirstBoot 后整体完成）
        next: Option<MigrationPhase>,
    },
    /// 阶段仍在进行（未达完成条件），状态不变。
    Pending(MigrationPhase),
    /// 阶段失败（可断点续传重试）。
    Failed {
        /// 失败的阶段
        phase: MigrationPhase,
        /// 失败原因
        reason: String,
    },
}

// ----------------------------------------------------------------------------
// 状态机驱动
// ----------------------------------------------------------------------------

/// 迁移阶段状态机。
///
/// 纯内存模型，记录"当前阶段 + 已完成阶段集合"。被 `MigrationEngine` 驱动，
/// 每完成一个阶段调用 [`PhaseMachine::advance`] 推进。安全保证：
/// `ExcludeSensitive` 不可跳过（`advance` 越过它会被拒）。
#[derive(Debug, Clone)]
pub struct PhaseMachine {
    current: MigrationPhase,
    completed: Vec<MigrationPhase>,
}

impl PhaseMachine {
    /// 新建——从起点 `SystemInit` 开始。
    pub fn new() -> Self {
        Self {
            current: MigrationPhase::SystemInit,
            completed: Vec::new(),
        }
    }

    /// 从指定阶段开始（断点续传用：从 checkpoint 恢复）。
    /// `already_completed` 为已完成的阶段（按顺序），`current` 为待执行的阶段。
    pub fn resume_from(already_completed: Vec<MigrationPhase>, current: MigrationPhase) -> Self {
        Self {
            current,
            completed: already_completed,
        }
    }

    /// 当前阶段。
    pub fn current(&self) -> MigrationPhase {
        self.current
    }

    /// 已完成阶段快照。
    pub fn completed(&self) -> &[MigrationPhase] {
        &self.completed
    }

    /// 是否所有阶段都已完成（FirstBoot 也完成）。
    pub fn is_finished(&self) -> bool {
        self.completed.len() == MigrationPhase::all().len()
    }

    /// 标记当前阶段完成，推进到下一阶段。
    ///
    /// 返回 [`PhaseTransition::Advance`]；若已是末阶段（FirstBoot），返回
    /// `Advance { completed: FirstBoot, next: None }` 并把 FirstBoot 记入完成。
    ///
    /// 安全：若 `ExcludeSensitive` 尚未完成而当前已越过它（理论上不可能，因为
    /// 顺序推进），返回 [`PhaseTransition::Failed`] 拒绝推进。这是防御性断言。
    pub fn advance(&mut self) -> PhaseTransition {
        let done = self.current;
        self.completed.push(done);

        // 安全防御：ExcludeSensitive 必须经历（不可被跳过）
        if done > MigrationPhase::ExcludeSensitive
            && !self.completed.contains(&MigrationPhase::ExcludeSensitive)
        {
            // 回滚刚才的 push，避免污染状态
            self.completed.pop();
            return PhaseTransition::Failed {
                phase: done,
                reason: "安全违规：试图跳过 ExcludeSensitive 阶段（§3.19）".into(),
            };
        }

        match done.next() {
            Some(n) => {
                self.current = n;
                PhaseTransition::Advance {
                    completed: done,
                    next: Some(n),
                }
            }
            None => PhaseTransition::Advance {
                completed: done,
                next: None,
            },
        }
    }

    /// 标记当前阶段失败（不改变 `completed`，便于 resume 重试同一阶段）。
    pub fn fail(&self, reason: impl Into<String>) -> PhaseTransition {
        PhaseTransition::Failed {
            phase: self.current,
            reason: reason.into(),
        }
    }
}

impl Default for PhaseMachine {
    fn default() -> Self {
        Self::new()
    }
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_ordering() {
        assert!(MigrationPhase::SystemInit < MigrationPhase::FileTransfer);
        assert!(MigrationPhase::FileTransfer < MigrationPhase::ExcludeSensitive);
        assert!(MigrationPhase::ExcludeSensitive < MigrationPhase::FirstBoot);
    }

    #[test]
    fn phase_next() {
        assert_eq!(
            MigrationPhase::SystemInit.next(),
            Some(MigrationPhase::FileTransfer)
        );
        assert_eq!(
            MigrationPhase::FileTransfer.next(),
            Some(MigrationPhase::ExcludeSensitive)
        );
        assert_eq!(
            MigrationPhase::ExcludeSensitive.next(),
            Some(MigrationPhase::FirstBoot)
        );
        assert_eq!(MigrationPhase::FirstBoot.next(), None);
    }

    #[test]
    fn full_lifecycle() {
        let mut m = PhaseMachine::new();
        assert_eq!(m.current(), MigrationPhase::SystemInit);
        assert!(!m.is_finished());

        assert!(matches!(
            m.advance(),
            PhaseTransition::Advance {
                completed: MigrationPhase::SystemInit,
                next: Some(MigrationPhase::FileTransfer)
            }
        ));
        assert!(matches!(
            m.advance(),
            PhaseTransition::Advance {
                completed: MigrationPhase::FileTransfer,
                next: Some(MigrationPhase::ExcludeSensitive)
            }
        ));
        assert!(matches!(
            m.advance(),
            PhaseTransition::Advance {
                completed: MigrationPhase::ExcludeSensitive,
                next: Some(MigrationPhase::FirstBoot)
            }
        ));
        assert!(matches!(
            m.advance(),
            PhaseTransition::Advance {
                completed: MigrationPhase::FirstBoot,
                next: None
            }
        ));
        assert!(m.is_finished());
        assert_eq!(m.completed().len(), 4);
    }

    #[test]
    fn exclude_sensitive_cannot_be_skipped() {
        // 构造一个非法状态：已绕过 ExcludeSensitive 但未完成它（防御性测试）
        // 正常 PhaseMachine 不允许直接构造此状态，这里模拟"已完成 FirstBoot 但缺 ExcludeSensitive"
        let mut m = PhaseMachine {
            current: MigrationPhase::FirstBoot,
            completed: vec![
                MigrationPhase::SystemInit,
                MigrationPhase::FileTransfer,
                // 故意缺 ExcludeSensitive
            ],
        };
        let t = m.advance();
        match t {
            PhaseTransition::Failed { phase, reason } => {
                assert_eq!(phase, MigrationPhase::FirstBoot);
                assert!(reason.contains("ExcludeSensitive"));
            }
            _ => panic!("应拒绝跳过敏感排除阶段"),
        }
        // completed 未被污染（pop 回滚）
        assert_eq!(m.completed().len(), 2);
    }

    #[test]
    fn resume_from_checkpoint() {
        let m = PhaseMachine::resume_from(
            vec![MigrationPhase::SystemInit, MigrationPhase::FileTransfer],
            MigrationPhase::ExcludeSensitive,
        );
        assert_eq!(m.current(), MigrationPhase::ExcludeSensitive);
        assert_eq!(m.completed().len(), 2);
        assert!(!m.is_finished());
    }

    #[test]
    fn fail_does_not_mutate_completed() {
        let mut m = PhaseMachine::new();
        let _ = m.advance(); // SystemInit done
        let t = m.fail("网络中断");
        match t {
            PhaseTransition::Failed { phase, reason } => {
                assert_eq!(phase, MigrationPhase::FileTransfer);
                assert_eq!(reason, "网络中断");
            }
            _ => panic!(),
        }
        assert_eq!(m.completed().len(), 1); // 仅 SystemInit
    }

    #[test]
    fn is_security_sensitive_flag() {
        assert!(MigrationPhase::ExcludeSensitive.is_security_sensitive());
        assert!(!MigrationPhase::SystemInit.is_security_sensitive());
        assert!(!MigrationPhase::FileTransfer.is_security_sensitive());
        assert!(!MigrationPhase::FirstBoot.is_security_sensitive());
    }

    #[test]
    fn phase_display() {
        assert_eq!(MigrationPhase::SystemInit.to_string(), "system_init");
        assert_eq!(
            MigrationPhase::ExcludeSensitive.to_string(),
            "exclude_sensitive"
        );
    }

    // —— 覆盖率补测：Display 全分支 + ordinal + Default + 枚举语义 ——

    #[test]
    fn phase_display_all_variants() {
        // 覆盖 FileTransfer / FirstBoot 的 Display 分支
        assert_eq!(MigrationPhase::FileTransfer.to_string(), "file_transfer");
        assert_eq!(MigrationPhase::FirstBoot.to_string(), "first_boot");
    }

    #[test]
    fn phase_ordinals() {
        // 覆盖 ordinal() 全分支
        assert_eq!(MigrationPhase::SystemInit.ordinal(), 0);
        assert_eq!(MigrationPhase::FileTransfer.ordinal(), 1);
        assert_eq!(MigrationPhase::ExcludeSensitive.ordinal(), 2);
        assert_eq!(MigrationPhase::FirstBoot.ordinal(), 3);
    }

    #[test]
    fn phase_all_has_four_in_order() {
        let all = MigrationPhase::all();
        assert_eq!(all.len(), 4);
        assert_eq!(all[0], MigrationPhase::SystemInit);
        assert_eq!(all[1], MigrationPhase::FileTransfer);
        assert_eq!(all[2], MigrationPhase::ExcludeSensitive);
        assert_eq!(all[3], MigrationPhase::FirstBoot);
    }

    #[test]
    fn phase_machine_default() {
        // 覆盖 PhaseMachine::Default impl（== new）
        let m = PhaseMachine::default();
        assert_eq!(m.current(), MigrationPhase::SystemInit);
        assert!(m.completed().is_empty());
        assert!(!m.is_finished());
    }

    #[test]
    fn phase_advance_to_finished_is_finished() {
        // 推进到末态后 is_finished()=true
        let mut m = PhaseMachine::new();
        let _ = m.advance(); // SystemInit
        let _ = m.advance(); // FileTransfer
        let _ = m.advance(); // ExcludeSensitive
        assert!(!m.is_finished());
        let _ = m.advance(); // FirstBoot
        assert!(m.is_finished());
    }

    #[test]
    fn phase_serde_roundtrip() {
        // 覆盖 serde rename_all snake_case 往返
        for p in MigrationPhase::all() {
            let json = serde_json::to_string(p).unwrap();
            let back: MigrationPhase = serde_json::from_str(&json).unwrap();
            assert_eq!(*p, back);
        }
        // 校验具体 snake_case 标签
        assert_eq!(
            serde_json::to_string(&MigrationPhase::FileTransfer).unwrap(),
            "\"file_transfer\""
        );
        assert_eq!(
            serde_json::to_string(&MigrationPhase::FirstBoot).unwrap(),
            "\"first_boot\""
        );
    }

    #[test]
    fn phase_partial_ord_eq() {
        // 覆盖 Ord/PartialOrd 排序（== 与 !=）
        assert_eq!(MigrationPhase::SystemInit, MigrationPhase::SystemInit);
        assert_ne!(MigrationPhase::SystemInit, MigrationPhase::FirstBoot);
        // 全序：SystemInit < ... < FirstBoot
        let mut v: Vec<MigrationPhase> = MigrationPhase::all().to_vec();
        v.reverse();
        v.sort();
        assert_eq!(v, MigrationPhase::all());
    }
}
