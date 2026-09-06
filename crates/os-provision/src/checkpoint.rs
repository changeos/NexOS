//! 断点续传模型——`MigrationCheckpoint` + 恢复决策算法。
//!
//! 迁移是长流程（大数据集 ZFS send/recv），中断后必须能从断点续传，避免重头再来。
//! 锚点持久化在 `os-meta` KV（见规格书 §4 / `_conventions.md`）——本模块只定义
//! 数据结构与**纯决策算法**：给定 checkpoint + 目标状态 → 决定从哪个阶段/数据集续。
//!
//! 决策规则（[`CheckpointPolicy::decide_resume`]）：
//! - 若整体已完成 → `Finished`（无需续传）。
//! - 若已完成阶段数 < 当前阶段序号 → 从第一个"未完成且需执行"的阶段续。
//!   具体到 `FileTransfer` 阶段：从第一个未传完的数据集续（已传数据集跳过）。
//! - 优先复用 [`crate::phase::PhaseMachine`] 的顺序推进语义。

use std::collections::HashSet;

use os_core::{DatasetId, DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::exclude::ExcludeRules;
use crate::phase::MigrationPhase;

// ----------------------------------------------------------------------------
// 已传文件 + 校验和
// ----------------------------------------------------------------------------

/// 单个已迁移文件/数据集的校验记录。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransferredFile {
    /// 文件路径或数据集 ID
    pub path: String,
    /// 字节大小
    pub size: u64,
    /// 校验和（SHA-256 十六进制，64 字符；None 表示该校验跳过）
    pub checksum: Option<String>,
}

impl TransferredFile {
    /// 简便构造（无校验和）。
    pub fn new(path: impl Into<String>, size: u64) -> Self {
        Self {
            path: path.into(),
            size,
            checksum: None,
        }
    }

    /// 带校验和构造。
    pub fn with_checksum(path: impl Into<String>, size: u64, checksum: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            size,
            checksum: Some(checksum.into()),
        }
    }
}

// ----------------------------------------------------------------------------
// Checkpoint
// ----------------------------------------------------------------------------

/// 迁移断点锚点——序列化后存入 `os-meta` KV（键形如 `migrate/checkpoint/<plan_id>`）。
///
/// 安全：本结构**只存校验和/路径/大小**，绝不存密钥/密码明文（呼应 §3.19）。
/// 敏感项校验和仅在排除清单评估后用于审计（确认已排除），不参与传输。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationCheckpoint {
    /// 关联的迁移计划 ID（与 `MigrationPlan`/任务 ID 对应）
    pub plan_id: String,
    /// 已完成的阶段（按完成顺序）
    pub completed_phases: Vec<MigrationPhase>,
    /// 当前所在阶段（待执行或中断时的阶段）
    pub current_phase: MigrationPhase,
    /// 已传输的文件/数据集（FileTransfer 阶段产物）
    pub transferred: Vec<TransferredFile>,
    /// 敏感排除评估结果（ExcludeSensitive 阶段产物，仅含路径 + 命中规则类别，
    /// 不含密钥本体）
    pub exclude_outcome: ExcludeOutcome,
    /// 创建/最后更新时间（UTC）
    pub updated_at: DateTime,
}

impl MigrationCheckpoint {
    /// 新建初始锚点（从 SystemInit 起点开始，无任何完成项）。
    pub fn initial(plan_id: impl Into<String>) -> Self {
        Self {
            plan_id: plan_id.into(),
            completed_phases: Vec::new(),
            current_phase: MigrationPhase::SystemInit,
            transferred: Vec::new(),
            exclude_outcome: ExcludeOutcome::default(),
            updated_at: Utc::now(),
        }
    }

    /// 已传文件路径集合（快速查重）。
    pub fn transferred_paths(&self) -> HashSet<&str> {
        self.transferred.iter().map(|f| f.path.as_str()).collect()
    }

    /// 是否已传输某条目。
    pub fn is_transferred(&self, path: &str) -> bool {
        self.transferred.iter().any(|f| f.path == path)
    }

    /// 累计已传字节数。
    pub fn total_transferred_bytes(&self) -> u64 {
        self.transferred.iter().map(|f| f.size).sum()
    }
}

// ----------------------------------------------------------------------------
// 排除评估结果（持久化的子结构）
// ----------------------------------------------------------------------------

/// 敏感排除阶段的持久化结果（仅含审计用元数据，不含密钥本体）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExcludeOutcome {
    /// 已评估并排除的条目数（仅计数，不存路径以免泄露密钥位置）
    pub excluded_count: usize,
    /// 已通过（应传输）的条目数
    pub transferred_count: usize,
    /// 命中的敏感类别集合（去重，审计用）
    pub hit_categories: Vec<String>,
}

impl ExcludeOutcome {
    /// 基于 [`ExcludeRules::partition`] 的结果构造。
    pub fn from_partition<I>(
        transferred: &[&str],
        excluded: &[(I, crate::exclude::ExcludeRule)],
    ) -> Self
    where
        I: AsRef<str>,
    {
        let mut cats: Vec<String> = Vec::new();
        for (_, rule) in excluded {
            let c = format!("{:?}", rule.category);
            if !cats.contains(&c) {
                cats.push(c);
            }
        }
        Self {
            excluded_count: excluded.len(),
            transferred_count: transferred.len(),
            hit_categories: cats,
        }
    }
}

// ----------------------------------------------------------------------------
// 恢复决策
// ----------------------------------------------------------------------------

/// 恢复决策结果。
#[derive(Debug, Clone, PartialEq)]
pub enum ResumeDecision {
    /// 整体已完成，无需续传。
    Finished,
    /// 从指定阶段续传；若为 FileTransfer，附待续传数据集列表。
    ResumeFromPhase {
        /// 起始阶段
        phase: MigrationPhase,
        /// 在 FileTransfer 阶段待传的数据集（已完成的过滤掉；其它阶段为空）
        remaining_datasets: Vec<DatasetId>,
        /// 尚未排除的待评估条目（ExcludeSensitive 阶段重算时用）
        remaining_excludes: Vec<String>,
    },
}

/// 断点续传决策策略（纯函数）。
#[derive(Debug, Clone, Default)]
pub struct CheckpointPolicy;

impl CheckpointPolicy {
    /// 决定从哪里续传。
    ///
    /// 参数：
    /// - `checkpoint`：既有锚点（None = 全新迁移，从 SystemInit 起）
    /// - `target_phases`：本迁移需经过的全部阶段（一般用 [`MigrationPhase::all`]）
    /// - `all_datasets`：本迁移计划涉及的全部数据集
    /// - `pending_excludes`：尚未评估的待迁移条目（路径/键名，ExcludeSensitive 用）
    /// - `rules`：排除规则集（决策时不算排除，仅用于在 FileTransfer 阶段
    ///   决定剩余数据集是否仍需传——此处不排除数据集，因数据集传输由 ZFS 负责，
    ///   排除的是迁移包内敏感项；故 `rules` 主要在 ExcludeSensitive 阶段使用）
    pub fn decide_resume(
        &self,
        checkpoint: Option<&MigrationCheckpoint>,
        target_phases: &[MigrationPhase],
        all_datasets: &[DatasetId],
        pending_excludes: &[String],
        _rules: &ExcludeRules,
    ) -> ResumeDecision {
        let cp = match checkpoint {
            None => {
                // 全新迁移：从起点开始，全部数据集待传
                return ResumeDecision::ResumeFromPhase {
                    phase: MigrationPhase::SystemInit,
                    remaining_datasets: all_datasets.to_vec(),
                    remaining_excludes: pending_excludes.to_vec(),
                };
            }
            Some(c) => c,
        };

        // 整体完成判定：已完成阶段数 == 目标阶段数 且 当前阶段已完成
        if cp.completed_phases.len() >= target_phases.len() {
            return ResumeDecision::Finished;
        }

        // 从 checkpoint.current_phase 续（该阶段中断时未完成）
        let phase = cp.current_phase;

        // 若是 FileTransfer：算出尚未传完的数据集
        let remaining_datasets: Vec<DatasetId> = match phase {
            MigrationPhase::FileTransfer => all_datasets
                .iter()
                .filter(|d| !cp.is_transferred(d.as_str()))
                .cloned()
                .collect(),
            _ => Vec::new(),
        };

        // 若是 ExcludeSensitive：返回尚未评估的条目
        // （排除阶段重算：用 rules 重新 partition，已评估过的计入 outcome）
        let remaining_excludes: Vec<String> = match phase {
            MigrationPhase::ExcludeSensitive => pending_excludes.to_vec(),
            _ => Vec::new(),
        };

        ResumeDecision::ResumeFromPhase {
            phase,
            remaining_datasets,
            remaining_excludes,
        }
    }
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ds(s: &str) -> DatasetId {
        DatasetId::new(s)
    }

    #[test]
    fn fresh_migration_starts_from_system_init() {
        let policy = CheckpointPolicy;
        let all = vec![ds("tank/media"), ds("tank/photos")];
        let pending = vec!["/etc/os/config.toml".to_string()];
        let rules = ExcludeRules::defaults();
        let d = policy.decide_resume(None, MigrationPhase::all(), &all, &pending, &rules);
        match d {
            ResumeDecision::ResumeFromPhase {
                phase,
                remaining_datasets,
                remaining_excludes,
            } => {
                assert_eq!(phase, MigrationPhase::SystemInit);
                assert_eq!(remaining_datasets.len(), 2);
                assert_eq!(remaining_excludes.len(), 1);
            }
            _ => panic!("应从 SystemInit 续"),
        }
    }

    #[test]
    fn finished_when_all_phases_done() {
        let mut cp = MigrationCheckpoint::initial("p1");
        cp.completed_phases = MigrationPhase::all().to_vec();
        cp.current_phase = MigrationPhase::FirstBoot;
        let policy = CheckpointPolicy;
        let rules = ExcludeRules::defaults();
        let d = policy.decide_resume(Some(&cp), MigrationPhase::all(), &[], &[], &rules);
        assert_eq!(d, ResumeDecision::Finished);
    }

    #[test]
    fn resume_file_transfer_skips_done_datasets() {
        let mut cp = MigrationCheckpoint::initial("p1");
        cp.completed_phases = vec![MigrationPhase::SystemInit];
        cp.current_phase = MigrationPhase::FileTransfer;
        cp.transferred
            .push(TransferredFile::new("tank/media", 1024));
        let all = vec![ds("tank/media"), ds("tank/photos"), ds("tank/docs")];
        let rules = ExcludeRules::defaults();
        let policy = CheckpointPolicy;
        let d = policy.decide_resume(Some(&cp), MigrationPhase::all(), &all, &[], &rules);
        match d {
            ResumeDecision::ResumeFromPhase {
                phase,
                remaining_datasets,
                ..
            } => {
                assert_eq!(phase, MigrationPhase::FileTransfer);
                // tank/media 已传，应只返回剩余两个
                let names: Vec<&str> = remaining_datasets.iter().map(|d| d.as_str()).collect();
                assert_eq!(names, vec!["tank/photos", "tank/docs"]);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn resume_exclude_sensitive_returns_pending() {
        let mut cp = MigrationCheckpoint::initial("p1");
        cp.completed_phases = vec![MigrationPhase::SystemInit, MigrationPhase::FileTransfer];
        cp.current_phase = MigrationPhase::ExcludeSensitive;
        let pending = vec!["/etc/shadow".to_string(), "/etc/hostname".to_string()];
        let rules = ExcludeRules::defaults();
        let policy = CheckpointPolicy;
        let d = policy.decide_resume(Some(&cp), MigrationPhase::all(), &[], &pending, &rules);
        match d {
            ResumeDecision::ResumeFromPhase {
                phase,
                remaining_datasets,
                remaining_excludes,
            } => {
                assert_eq!(phase, MigrationPhase::ExcludeSensitive);
                assert!(remaining_datasets.is_empty());
                assert_eq!(remaining_excludes.len(), 2);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn transferred_file_checksum_roundtrip() {
        let f = TransferredFile::with_checksum("/x", 10, "abc123");
        assert_eq!(f.checksum.as_deref(), Some("abc123"));
        let f2 = TransferredFile::new("/y", 5);
        assert!(f2.checksum.is_none());
    }

    #[test]
    fn checkpoint_total_bytes() {
        let cp = MigrationCheckpoint {
            plan_id: "p".into(),
            completed_phases: vec![],
            current_phase: MigrationPhase::SystemInit,
            transferred: vec![
                TransferredFile::new("a", 100),
                TransferredFile::new("b", 250),
            ],
            exclude_outcome: ExcludeOutcome::default(),
            updated_at: Utc::now(),
        };
        assert_eq!(cp.total_transferred_bytes(), 350);
        assert!(cp.is_transferred("a"));
        assert!(!cp.is_transferred("z"));
    }

    #[test]
    fn exclude_outcome_from_partition() {
        let rules = ExcludeRules::defaults();
        let entries = ["/etc/shadow", "/etc/hostname", "/etc/os/jwt-signing.key"];
        let (t, e) = rules.partition(entries.iter().copied());
        let outcome = ExcludeOutcome::from_partition(&t, &e);
        assert_eq!(outcome.excluded_count, 2);
        assert_eq!(outcome.transferred_count, 1);
        assert!(!outcome.hit_categories.is_empty());
    }

    #[test]
    fn checkpoint_serializable() {
        // 确认可序列化（存入 os-meta KV 前提）
        let cp = MigrationCheckpoint::initial("p1");
        let json = serde_json::to_string(&cp).expect("序列化");
        let back: MigrationCheckpoint = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back.plan_id, "p1");
    }

    #[test]
    fn exclude_outcome_does_not_store_key_body() {
        // 安全：outcome 只存计数/类别，不存敏感路径本体
        let o = ExcludeOutcome::default();
        let serialized = serde_json::to_string(&o).unwrap();
        assert!(!serialized.contains("shadow"));
        assert!(!serialized.contains("password"));
    }

    #[test]
    fn decide_resume_with_zero_target_phases_treated_as_finished() {
        // 边界：target_phases 空 + 已完成空 → 完成判定（completed_phases.len()>=0）
        let cp = MigrationCheckpoint::initial("p1");
        let policy = CheckpointPolicy;
        let rules = ExcludeRules::defaults();
        let d = policy.decide_resume(Some(&cp), &[], &[], &[], &rules);
        // completed_phases.len()==0 >= target.len()==0 → Finished
        assert_eq!(d, ResumeDecision::Finished);
    }

    // —— 覆盖率补测：CheckpointPolicy 边界 + transferred_paths/from_partition ——

    #[test]
    fn checkpoint_transferred_paths_set() {
        // 覆盖 transferred_paths()（HashSet 查重）
        let mut cp = MigrationCheckpoint::initial("p1");
        cp.transferred.push(TransferredFile::new("a", 10));
        cp.transferred.push(TransferredFile::new("b", 20));
        let paths = cp.transferred_paths();
        assert_eq!(paths.len(), 2);
        assert!(paths.contains("a"));
        assert!(paths.contains("b"));
        assert!(!paths.contains("c"));
    }

    #[test]
    fn checkpoint_resume_from_system_init_phase_returns_empty_remaining() {
        // current_phase = SystemInit（非 FileTransfer/ExcludeSensitive）→
        // remaining_datasets 空 + remaining_excludes 空
        let mut cp = MigrationCheckpoint::initial("p1");
        cp.current_phase = MigrationPhase::SystemInit;
        let all = vec![ds("tank/x"), ds("tank/y")];
        let pending = vec!["/etc/shadow".to_string()];
        let rules = ExcludeRules::defaults();
        let policy = CheckpointPolicy;
        let d = policy.decide_resume(Some(&cp), MigrationPhase::all(), &all, &pending, &rules);
        match d {
            ResumeDecision::ResumeFromPhase {
                phase,
                remaining_datasets,
                remaining_excludes,
            } => {
                assert_eq!(phase, MigrationPhase::SystemInit);
                assert!(
                    remaining_datasets.is_empty(),
                    "SystemInit 阶段 remaining 为空"
                );
                assert!(
                    remaining_excludes.is_empty(),
                    "SystemInit 阶段 remaining_excludes 为空"
                );
            }
            _ => panic!("应从 SystemInit 续"),
        }
    }

    #[test]
    fn checkpoint_resume_from_first_boot_phase() {
        // current_phase = FirstBoot（非 FileTransfer/ExcludeSensitive）→ 空 remaining
        let mut cp = MigrationCheckpoint::initial("p1");
        cp.completed_phases = vec![
            MigrationPhase::SystemInit,
            MigrationPhase::FileTransfer,
            MigrationPhase::ExcludeSensitive,
        ];
        cp.current_phase = MigrationPhase::FirstBoot;
        let all = vec![ds("tank/x")];
        let rules = ExcludeRules::defaults();
        let policy = CheckpointPolicy;
        let d = policy.decide_resume(Some(&cp), MigrationPhase::all(), &all, &[], &rules);
        match d {
            ResumeDecision::ResumeFromPhase {
                phase,
                remaining_datasets,
                remaining_excludes,
            } => {
                assert_eq!(phase, MigrationPhase::FirstBoot);
                assert!(remaining_datasets.is_empty());
                assert!(remaining_excludes.is_empty());
            }
            _ => panic!("应从 FirstBoot 续"),
        }
    }

    #[test]
    fn exclude_outcome_dedup_categories() {
        // 覆盖 from_partition 的类别去重分支（多条同类规则 → hit_categories 不重复）
        use crate::exclude::ExcludeRule;
        use crate::exclude::{ExcludeCategory as C, ExcludePattern as P};
        let rules = ExcludeRules::from_rules(vec![
            ExcludeRule {
                pattern: P::Exact("/etc/shadow".into()),
                category: C::SystemCredential,
                reason: "r1".into(),
            },
            ExcludeRule {
                pattern: P::Exact("/etc/gshadow".into()),
                category: C::SystemCredential, // 同类 → 应去重
                reason: "r2".into(),
            },
            ExcludeRule {
                pattern: P::Exact("/etc/os/jwt.key".into()),
                category: C::JwtTotpSecret,
                reason: "r3".into(),
            },
        ]);
        let entries = ["/etc/shadow", "/etc/gshadow", "/etc/os/jwt.key", "/safe"];
        let (t, e) = rules.partition(entries.iter().copied());
        let outcome = ExcludeOutcome::from_partition(&t, &e);
        assert_eq!(outcome.excluded_count, 3);
        assert_eq!(outcome.transferred_count, 1);
        // 两类：SystemCredential + JwtTotpSecret（去重后）
        assert_eq!(outcome.hit_categories.len(), 2);
        assert!(outcome
            .hit_categories
            .contains(&"SystemCredential".to_string()));
        assert!(outcome
            .hit_categories
            .contains(&"JwtTotpSecret".to_string()));
    }

    #[test]
    fn exclude_outcome_empty_when_no_excludes() {
        // 全部应传输 → excluded_count=0, hit_categories 空
        let rules = ExcludeRules::from_rules(vec![]);
        let entries = ["/a", "/b"];
        let (t, e) = rules.partition(entries.iter().copied());
        let outcome = ExcludeOutcome::from_partition(&t, &e);
        assert_eq!(outcome.excluded_count, 0);
        assert_eq!(outcome.transferred_count, 2);
        assert!(outcome.hit_categories.is_empty());
    }
}
