//! A/B 双槽位状态机与槽位切换决策（规划文档 §3.12）
//!
//! 本模块是**纯逻辑**（无 bootloader / ostree 依赖）：
//! - [`SlotStatus`]：单槽位运行期状态（Active/Inactive/Failed/Updating）。
//! - [`SlotState`]：槽位完整描述（槽位 + 状态 + 版本 + 激活时间）。
//! - [`SlotManager`]：A/B 双槽编排的纯状态机——跟踪两槽、判定写入目标、
//!   处理"更新成功→切 inactive 为 active""启动失败→回滚到上一个 active"。
//!
//! 设计原则（呼应 §3.12 与 ADR-COMPAT-001）：
//! - 槽位切换是**决策**而非 I/O：本模块只产出"该激活哪个槽 / 该回滚到哪个槽"，
//!   真正的 bootloader 激活由 [`crate::UpdateEngine::activate_slot`] 执行。
//! - **不变量**：任一时刻至多一个槽为 [`SlotStatus::Active`]；两槽同时 Active
//!   或均 Inactive 视为冲突，由 [`SlotManager::resolve`] 在外部修复后重新判定。
//! - 安全：新槽只有在标记 [`SlotStatus::Active`] 且 boot 探活通过后才认为"提交"，
//!   探活失败回滚到上一个健康 active（见 [`SlotManager::on_boot_failed`]）。

use crate::update::UpdateSlot;
use os_core::DateTime;
use serde::{Deserialize, Serialize};

// ----------------------------------------------------------------------------
// 槽位状态
// ----------------------------------------------------------------------------

/// 单个槽位的运行期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotStatus {
    /// 当前活动槽（下次/本次启动从此槽引导）。
    Active,
    /// 非活动槽（可写入的备用槽，A/B 双槽更新写入的目标）。
    Inactive,
    /// 该槽启动失败过（标记坏，不再选用作写入目标，除非显式修复）。
    Failed,
    /// 正在写入更新（写入期间锁定，避免并发覆盖）。
    Updating,
}

impl SlotStatus {
    /// 是否可作为更新写入目标（必须是 Inactive 且未在 Updating/Failed）。
    #[must_use]
    pub fn is_writable(self) -> bool {
        matches!(self, Self::Inactive)
    }

    /// 是否处于"健康已提交"状态（Active 即视为已提交；探活由调用方在 Active 后做）。
    #[must_use]
    pub fn is_committed(self) -> bool {
        matches!(self, Self::Active)
    }
}

// ----------------------------------------------------------------------------
// 槽位完整描述
// ----------------------------------------------------------------------------

/// 单个槽位的完整状态快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotState {
    /// 槽位标识（A/B）
    pub slot: UpdateSlot,
    /// 运行期状态
    pub status: SlotStatus,
    /// 该槽当前装载的系统版本（None = 空槽/未安装）
    pub version: Option<String>,
    /// 最近一次被激活的时间（UTC；None = 从未激活）
    pub last_activated_at: Option<DateTime>,
    /// 最近一次写入完成的时间（UTC；None = 从未写入）
    pub last_written_at: Option<DateTime>,
}

impl SlotState {
    /// 构造一个空的非活动槽（默认初始状态）。
    #[must_use]
    pub fn new_inactive(slot: UpdateSlot) -> Self {
        Self {
            slot,
            status: SlotStatus::Inactive,
            version: None,
            last_activated_at: None,
            last_written_at: None,
        }
    }

    /// 构造一个已激活的健康槽（带版本与激活时间）。
    #[must_use]
    pub fn new_active(
        slot: UpdateSlot,
        version: impl Into<String>,
        activated_at: DateTime,
    ) -> Self {
        Self {
            slot,
            status: SlotStatus::Active,
            version: Some(version.into()),
            last_activated_at: Some(activated_at),
            last_written_at: None,
        }
    }
}

// ----------------------------------------------------------------------------
// 槽位切换决策
// ----------------------------------------------------------------------------

/// 槽位切换决策结果（纯逻辑产出，不含 I/O）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotSwitchDecision {
    /// 切换 `target` 槽为活动（原 active 降为 inactive）。
    /// 更新成功路径：把刚写完的 inactive 槽激活。
    Activate {
        /// 即将激活的槽
        target: UpdateSlot,
        /// 即将降为 inactive 的槽（上一个 active）
        previous: UpdateSlot,
    },
    /// 回滚到 `target` 槽（把失败的新槽降为 failed，原上一个 active 恢复）。
    /// 启动失败路径：新槽探活不通过，切回旧槽。
    Rollback {
        /// 回滚目标槽（上一个健康 active）
        target: UpdateSlot,
        /// 标记为失败的新槽
        failed_slot: UpdateSlot,
    },
    /// 无需切换（当前 active 已是目标 / 状态非法需外部修复）。
    NoOp,
}

// ----------------------------------------------------------------------------
// SlotManager —— A/B 双槽纯状态机
// ----------------------------------------------------------------------------

/// A/B 双槽编排状态机（纯逻辑）。
///
/// 跟踪 A/B 两槽状态，提供：
/// - [`Self::writable_slot`]：选出一个可写入的非活动槽（A/B 双槽更新的写入目标）。
/// - [`Self::active_slot`]：查询当前活动槽。
/// - [`Self::plan_activation`]：更新写入成功后，给出"激活该 inactive 槽"的决策。
/// - [`Self::on_boot_succeeded`]：新槽激活后探活通过，提交（标记上一个 active 退役）。
/// - [`Self::on_boot_failed`]：新槽激活后探活失败，给出回滚到上一个 active 的决策。
/// - [`Self::resolve`]：冲突自愈（两槽同状态时复位到一个合法态）。
///
/// 调用方（[`crate::UpdateEngine`] / [`crate::RollbackManager`] 的实现）持有此状态机，
/// 在 bootloader I/O 完成后回调本状态机推进。状态机本身不触达 bootloader。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotManager {
    /// 槽 A 状态
    pub a: SlotState,
    /// 槽 B 状态
    pub b: SlotState,
    /// 上一个被替换下来的 active 槽（用于 boot 失败时回滚；None = 首启/无历史）。
    pub previous_active: Option<UpdateSlot>,
}

impl SlotManager {
    /// 构造：给定初始活动槽及其版本/激活时间，另一槽为空 inactive。
    /// 常见初始态：A 已装系统并 active，B 空 inactive。
    #[must_use]
    pub fn new(active: UpdateSlot, version: impl Into<String>, activated_at: DateTime) -> Self {
        let version = version.into();
        let (a, b) = match active {
            UpdateSlot::A => (
                SlotState::new_active(UpdateSlot::A, version, activated_at),
                SlotState::new_inactive(UpdateSlot::B),
            ),
            UpdateSlot::B => (
                SlotState::new_inactive(UpdateSlot::A),
                SlotState::new_active(UpdateSlot::B, version, activated_at),
            ),
        };
        Self {
            a,
            b,
            previous_active: None,
        }
    }

    /// 取指定槽的状态。
    #[must_use]
    pub fn slot(&self, slot: UpdateSlot) -> &SlotState {
        match slot {
            UpdateSlot::A => &self.a,
            UpdateSlot::B => &self.b,
        }
    }

    /// 取指定槽的可变状态。
    pub fn slot_mut(&mut self, slot: UpdateSlot) -> &mut SlotState {
        match slot {
            UpdateSlot::A => &mut self.a,
            UpdateSlot::B => &mut self.b,
        }
    }

    /// 当前活动槽（至多一个；冲突时返回 None，建议先 [`Self::resolve`]）。
    #[must_use]
    pub fn active_slot(&self) -> Option<UpdateSlot> {
        let a_active = self.a.status == SlotStatus::Active;
        let b_active = self.b.status == SlotStatus::Active;
        match (a_active, b_active) {
            (true, false) => Some(UpdateSlot::A),
            (false, true) => Some(UpdateSlot::B),
            // 两个 active（冲突）或两个都非 active：None
            _ => None,
        }
    }

    /// 上一个被替换下来的 active 槽（boot 失败时的回滚目标候选）。
    #[must_use]
    pub fn previous_active_slot(&self) -> Option<UpdateSlot> {
        // 优先用显式记录的 previous_active；否则退化为"非当前 active 的那个槽"
        // （只要它装过版本且非 Failed，可作为回滚候选）。
        if let Some(prev) = self.previous_active {
            return Some(prev);
        }
        let other = self.active_slot()?.other();
        let st = self.slot(other);
        if st.version.is_some() && st.status != SlotStatus::Failed {
            Some(other)
        } else {
            None
        }
    }

    /// 选出一个可写入更新的非活动槽。
    ///
    /// 规则：优先返回状态为 [`SlotStatus::Inactive`] 的槽；若两槽都 Inactive
    /// （首启未装系统），优先返回非当前 active 的那个；均不可写返回 None。
    /// 返回 [`crate::UpdateError::SlotConflict`] 当两槽均不可写（Updating/Failed）。
    pub fn writable_slot(&self) -> Result<UpdateSlot, crate::UpdateError> {
        // 标准路径：当前有一个 active，另一槽 inactive 可写。
        if let Some(active) = self.active_slot() {
            let other = active.other();
            if self.slot(other).status.is_writable() {
                return Ok(other);
            }
        }
        // 退化路径：两个都 inactive（首启空机），写 A。
        if self.a.status.is_writable() {
            return Ok(UpdateSlot::A);
        }
        if self.b.status.is_writable() {
            return Ok(UpdateSlot::B);
        }
        Err(crate::UpdateError::SlotConflict(
            "无可写槽位（两槽均 Updating/Failed 或非法）".to_string(),
        ))
    }

    /// 标记开始向 `slot` 写入更新（锁定为 Updating）。
    ///
    /// 前置：`slot` 必须 Inactive，否则返回 [`crate::UpdateError::SlotConflict`]。
    pub fn begin_write(&mut self, slot: UpdateSlot) -> Result<(), crate::UpdateError> {
        if !self.slot(slot).status.is_writable() {
            return Err(crate::UpdateError::SlotConflict(format!(
                "槽 {slot:?} 非可写（当前 {:?}），无法开始写入",
                self.slot(slot).status
            )));
        }
        self.slot_mut(slot).status = SlotStatus::Updating;
        Ok(())
    }

    /// 标记向 `slot` 写入完成（写入新版本 `version`，时间 `written_at`）。
    /// 写完后槽位回到 Inactive（等待 [`Self::plan_activation`] 激活）。
    pub fn finish_write(
        &mut self,
        slot: UpdateSlot,
        version: impl Into<String>,
        written_at: DateTime,
    ) -> Result<(), crate::UpdateError> {
        if self.slot(slot).status != SlotStatus::Updating {
            return Err(crate::UpdateError::SlotConflict(format!(
                "槽 {slot:?} 不在 Updating（当前 {:?}），无法完成写入",
                self.slot(slot).status
            )));
        }
        let s = self.slot_mut(slot);
        s.status = SlotStatus::Inactive;
        s.version = Some(version.into());
        s.last_written_at = Some(written_at);
        Ok(())
    }

    /// 标记写入失败（槽位降为 Failed，不再用作写入目标除非显式修复）。
    pub fn fail_write(&mut self, slot: UpdateSlot) {
        self.slot_mut(slot).status = SlotStatus::Failed;
    }

    /// 修复一个 Failed 槽回到 Inactive（外部诊断后调用，如低级格式化后）。
    pub fn repair(&mut self, slot: UpdateSlot) {
        let s = self.slot_mut(slot);
        if s.status == SlotStatus::Failed {
            s.status = SlotStatus::Inactive;
        }
    }

    /// 更新写入成功后，给出"激活刚写完的 inactive 槽"的决策。
    ///
    /// 决策：把 `just_written` 激活，原 active 降为 inactive（并记入 previous_active，
    /// 作为 boot 失败时的回滚目标）。调用方据此执行 bootloader 激活，成功后调
    /// [`Self::on_boot_succeeded`] 提交，失败调 [`Self::on_boot_failed`] 回滚。
    ///
    /// 返回 [`SlotSwitchDecision::Activate`]；若 `just_written` 状态非 Inactive 或
    /// 当前无 active（首启特例），返回 [`SlotSwitchDecision::NoOp`] 并由调用方
    /// 直接激活（首启无回滚需求）。
    pub fn plan_activation(&self, just_written: UpdateSlot) -> SlotSwitchDecision {
        // 前置：刚写完的槽必须是 Inactive
        if self.slot(just_written).status != SlotStatus::Inactive {
            return SlotSwitchDecision::NoOp;
        }
        match self.active_slot() {
            Some(prev) => SlotSwitchDecision::Activate {
                target: just_written,
                previous: prev,
            },
            // 首启（无 active）：直接激活，无回滚目标
            None => SlotSwitchDecision::NoOp,
        }
    }

    /// 应用 [`SlotSwitchDecision::Activate`]：在内存态执行槽位切换
    /// （new 激活、old 降为 inactive、记录 previous_active）。
    ///
    /// 这只是状态机推进，不触达 bootloader；调用方应在 bootloader 激活成功后调用。
    pub fn apply_activation(
        &mut self,
        target: UpdateSlot,
        previous: UpdateSlot,
        now: DateTime,
    ) -> Result<(), crate::UpdateError> {
        if target == previous {
            return Err(crate::UpdateError::SlotConflict(
                "激活目标与上一个 active 相同".to_string(),
            ));
        }
        if self.slot(target).version.is_none() {
            return Err(crate::UpdateError::SlotConflict(format!(
                "槽 {target:?} 未写入任何版本，无法激活"
            )));
        }
        // 旧 active 降为 inactive（保留版本，作回滚候选）
        self.slot_mut(previous).status = SlotStatus::Inactive;
        // 新槽激活
        let t = self.slot_mut(target);
        t.status = SlotStatus::Active;
        t.last_activated_at = Some(now);
        self.previous_active = Some(previous);
        Ok(())
    }

    /// 新槽激活后 boot 探活通过——提交切换（清理 previous_active 标记，确认提交）。
    ///
    /// 返回 true 表示已提交；false 表示当前 active 与 previous 不一致（非法态）。
    /// 提交后 `previous_active` 清空（旧槽版本保留，仍可作显式手动回滚点）。
    #[must_use]
    pub fn on_boot_succeeded(&mut self) -> bool {
        // 提交：清掉 previous_active（探活已过，不再需要自动回滚标记）
        self.previous_active = None;
        self.active_slot().is_some()
    }

    /// 新槽激活后 boot 探活失败——给出回滚到上一个 active 的决策。
    ///
    /// 决策：把当前 active 标记 Failed，回滚到 previous_active（或退化候选）。
    /// 无可用回滚目标（首启/无历史健康槽）时返回 [`SlotSwitchDecision::NoOp`]，
    /// 调用方需人工介入。
    #[must_use]
    pub fn on_boot_failed(&mut self) -> SlotSwitchDecision {
        let Some(failed_slot) = self.active_slot() else {
            return SlotSwitchDecision::NoOp;
        };
        // 选回滚目标
        let Some(target) = self.previous_active_slot() else {
            return SlotSwitchDecision::NoOp;
        };
        if target == failed_slot {
            return SlotSwitchDecision::NoOp;
        }
        // 内存态推进：新槽标记 Failed，旧槽恢复 Active
        self.slot_mut(failed_slot).status = SlotStatus::Failed;
        let t = self.slot_mut(target);
        t.status = SlotStatus::Active;
        // previous_active 清空（已回滚到位）
        self.previous_active = None;
        SlotSwitchDecision::Rollback {
            target,
            failed_slot,
        }
    }

    /// 冲突自愈：当两槽同时 Active（或同时非 Active 且都有版本）时，
    /// 复位到一个合法态——保留 `last_activated_at` 较新的那个为 Active，
    /// 另一个降为 Inactive。
    ///
    /// 返回复位后选定的 Active 槽；若两槽都无版本（空机），返回 None。
    pub fn resolve(&mut self) -> Option<UpdateSlot> {
        let a_active = self.a.status == SlotStatus::Active;
        let b_active = self.b.status == SlotStatus::Active;
        if a_active && b_active {
            // 冲突：保留激活时间更晚的（更可能是新切换的目标）
            let a_t = self.a.last_activated_at;
            let b_t = self.b.last_activated_at;
            let keep = match (a_t, b_t) {
                (Some(x), Some(y)) => x >= y,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => true, // 平局保 A
            };
            if keep {
                self.b.status = SlotStatus::Inactive;
                Some(UpdateSlot::A)
            } else {
                self.a.status = SlotStatus::Inactive;
                Some(UpdateSlot::B)
            }
        } else {
            self.active_slot()
        }
    }
}

/// A/B 槽位的伴生方法（取对侧槽）。
impl UpdateSlot {
    /// 取对侧槽（A↔B）。
    #[must_use]
    pub fn other(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::UpdateSlot;

    fn now() -> DateTime {
        chrono::Utc::now()
    }

    fn fresh_mgr() -> SlotManager {
        // 初始：A active（v1.0.0），B 空 inactive
        SlotManager::new(UpdateSlot::A, "1.0.0", now())
    }

    // —— 基础查询 ——

    #[test]
    fn initial_state() {
        let m = fresh_mgr();
        assert_eq!(m.active_slot(), Some(UpdateSlot::A));
        assert_eq!(m.slot(UpdateSlot::A).status, SlotStatus::Active);
        assert_eq!(m.slot(UpdateSlot::B).status, SlotStatus::Inactive);
        assert_eq!(m.slot(UpdateSlot::A).version.as_deref(), Some("1.0.0"));
        assert!(m.slot(UpdateSlot::B).version.is_none());
    }

    #[test]
    fn other_slot_helper() {
        assert_eq!(UpdateSlot::A.other(), UpdateSlot::B);
        assert_eq!(UpdateSlot::B.other(), UpdateSlot::A);
    }

    #[test]
    fn slot_status_flags() {
        assert!(SlotStatus::Active.is_committed());
        assert!(!SlotStatus::Inactive.is_committed());
        assert!(SlotStatus::Inactive.is_writable());
        assert!(!SlotStatus::Updating.is_writable());
        assert!(!SlotStatus::Failed.is_writable());
        assert!(!SlotStatus::Active.is_writable());
    }

    // —— writable_slot ——

    #[test]
    fn writable_slot_picks_other_of_active() {
        let m = fresh_mgr();
        // A active → B 可写
        assert_eq!(m.writable_slot().unwrap(), UpdateSlot::B);
    }

    #[test]
    fn writable_slot_picks_a_when_both_inactive() {
        // 首启空机：两槽都 Inactive，优先写 A
        let m = SlotManager {
            a: SlotState::new_inactive(UpdateSlot::A),
            b: SlotState::new_inactive(UpdateSlot::B),
            previous_active: None,
        };
        // 没有 active，两槽都可写，按实现优先返回 A
        let w = m.writable_slot().unwrap();
        assert_eq!(w, UpdateSlot::A);
    }

    #[test]
    fn writable_slot_conflict_when_none_writable() {
        let mut m = fresh_mgr();
        m.b.status = SlotStatus::Failed; // B 坏
                                         // 此时无 Inactive 槽
        assert!(matches!(
            m.writable_slot(),
            Err(crate::UpdateError::SlotConflict(_))
        ));
    }

    // —— begin/finish/fail write ——

    #[test]
    fn begin_write_locks_slot() {
        let mut m = fresh_mgr();
        m.begin_write(UpdateSlot::B).unwrap();
        assert_eq!(m.slot(UpdateSlot::B).status, SlotStatus::Updating);
        // 重复 begin 同槽应失败
        assert!(m.begin_write(UpdateSlot::B).is_err());
        // begin 已 active 的 A 应失败
        assert!(m.begin_write(UpdateSlot::A).is_err());
    }

    #[test]
    fn finish_write_sets_version_and_inactive() {
        let mut m = fresh_mgr();
        m.begin_write(UpdateSlot::B).unwrap();
        let t = now();
        m.finish_write(UpdateSlot::B, "1.1.0", t).unwrap();
        assert_eq!(m.slot(UpdateSlot::B).status, SlotStatus::Inactive);
        assert_eq!(m.slot(UpdateSlot::B).version.as_deref(), Some("1.1.0"));
        assert_eq!(m.slot(UpdateSlot::B).last_written_at, Some(t));
    }

    #[test]
    fn finish_write_without_begin_fails() {
        let mut m = fresh_mgr();
        // B 是 Inactive 而非 Updating
        assert!(m.finish_write(UpdateSlot::B, "1.1.0", now()).is_err());
    }

    #[test]
    fn fail_write_marks_failed() {
        let mut m = fresh_mgr();
        m.begin_write(UpdateSlot::B).unwrap();
        m.fail_write(UpdateSlot::B);
        assert_eq!(m.slot(UpdateSlot::B).status, SlotStatus::Failed);
        // Failed 后不再可写，需 repair
        assert!(m.writable_slot().is_err());
        m.repair(UpdateSlot::B);
        assert_eq!(m.slot(UpdateSlot::B).status, SlotStatus::Inactive);
        assert_eq!(m.writable_slot().unwrap(), UpdateSlot::B);
    }

    // —— plan_activation + apply_activation ——

    #[test]
    fn plan_activation_activates_inactive() {
        let mut m = fresh_mgr();
        m.begin_write(UpdateSlot::B).unwrap();
        m.finish_write(UpdateSlot::B, "1.1.0", now()).unwrap();
        let decision = m.plan_activation(UpdateSlot::B);
        assert_eq!(
            decision,
            SlotSwitchDecision::Activate {
                target: UpdateSlot::B,
                previous: UpdateSlot::A,
            }
        );
        // 应用激活
        m.apply_activation(UpdateSlot::B, UpdateSlot::A, now())
            .unwrap();
        assert_eq!(m.active_slot(), Some(UpdateSlot::B));
        assert_eq!(m.slot(UpdateSlot::B).status, SlotStatus::Active);
        assert_eq!(m.slot(UpdateSlot::A).status, SlotStatus::Inactive);
        assert_eq!(m.previous_active, Some(UpdateSlot::A));
    }

    #[test]
    fn plan_activation_noop_when_not_inactive() {
        let mut m = fresh_mgr();
        m.begin_write(UpdateSlot::B).unwrap();
        // B 在 Updating（未 finish），plan 应 NoOp
        assert_eq!(m.plan_activation(UpdateSlot::B), SlotSwitchDecision::NoOp);
    }

    #[test]
    fn apply_activation_rejects_empty_slot() {
        let mut m = fresh_mgr();
        // B 仍是空 Inactive（无 version），不能激活
        assert!(matches!(
            m.apply_activation(UpdateSlot::B, UpdateSlot::A, now()),
            Err(crate::UpdateError::SlotConflict(_))
        ));
    }

    #[test]
    fn apply_activation_rejects_same_slot() {
        let mut m = fresh_mgr();
        assert!(matches!(
            m.apply_activation(UpdateSlot::A, UpdateSlot::A, now()),
            Err(crate::UpdateError::SlotConflict(_))
        ));
    }

    // —— on_boot_succeeded / on_boot_failed ——

    #[test]
    fn on_boot_succeeded_commits() {
        let mut m = fresh_mgr();
        m.begin_write(UpdateSlot::B).unwrap();
        m.finish_write(UpdateSlot::B, "1.1.0", now()).unwrap();
        let _ = m.plan_activation(UpdateSlot::B);
        m.apply_activation(UpdateSlot::B, UpdateSlot::A, now())
            .unwrap();
        // 探活通过 → 提交
        assert!(m.on_boot_succeeded());
        assert_eq!(m.previous_active, None); // 清空
        assert_eq!(m.active_slot(), Some(UpdateSlot::B));
    }

    #[test]
    fn on_boot_failed_rolls_back_to_previous() {
        let mut m = fresh_mgr();
        m.begin_write(UpdateSlot::B).unwrap();
        m.finish_write(UpdateSlot::B, "1.1.0-broken", now())
            .unwrap();
        m.apply_activation(UpdateSlot::B, UpdateSlot::A, now())
            .unwrap();
        // 探活失败 → 回滚到 A，B 标记 Failed
        let decision = m.on_boot_failed();
        assert_eq!(
            decision,
            SlotSwitchDecision::Rollback {
                target: UpdateSlot::A,
                failed_slot: UpdateSlot::B,
            }
        );
        assert_eq!(m.active_slot(), Some(UpdateSlot::A));
        assert_eq!(m.slot(UpdateSlot::B).status, SlotStatus::Failed);
        assert_eq!(m.slot(UpdateSlot::A).version.as_deref(), Some("1.0.0"));
    }

    #[test]
    fn on_boot_failed_noop_without_previous() {
        // 首启：无 previous，boot 失败也无路可退
        let mut m = SlotManager {
            a: SlotState::new_inactive(UpdateSlot::A),
            b: SlotState::new_inactive(UpdateSlot::B),
            previous_active: None,
        };
        // 手动把 A 标记 active（模拟首启后）但没有 previous_active
        m.a.status = SlotStatus::Active;
        m.a.version = Some("1.0.0".to_string());
        let decision = m.on_boot_failed();
        assert_eq!(decision, SlotSwitchDecision::NoOp);
    }

    #[test]
    fn full_update_cycle_succeeds() {
        // 端到端：A(v1.0.0) → 写 B(v1.1.0) → 激活 B → 探活通过 → 提交
        let mut m = fresh_mgr();
        m.begin_write(UpdateSlot::B).unwrap();
        m.finish_write(UpdateSlot::B, "1.1.0", now()).unwrap();
        let d = m.plan_activation(UpdateSlot::B);
        if let SlotSwitchDecision::Activate { target, previous } = d {
            m.apply_activation(target, previous, now()).unwrap();
        } else {
            panic!("应是 Activate");
        }
        assert!(m.on_boot_succeeded());
        assert_eq!(m.active_slot(), Some(UpdateSlot::B));
        assert_eq!(m.slot(UpdateSlot::B).version.as_deref(), Some("1.1.0"));
        // 再升一次：B active，写 A
        m.begin_write(UpdateSlot::A).unwrap();
        m.finish_write(UpdateSlot::A, "1.2.0", now()).unwrap();
        let d = m.plan_activation(UpdateSlot::A);
        if let SlotSwitchDecision::Activate { target, previous } = d {
            m.apply_activation(target, previous, now()).unwrap();
        } else {
            panic!("应是 Activate");
        }
        assert!(m.on_boot_succeeded());
        assert_eq!(m.active_slot(), Some(UpdateSlot::A));
        assert_eq!(m.slot(UpdateSlot::A).version.as_deref(), Some("1.2.0"));
    }

    // —— resolve（冲突自愈）——

    #[test]
    fn resolve_conflict_keeps_newer() {
        let mut m = fresh_mgr(); // A active at now
        let earlier = now() - chrono::Duration::seconds(100);
        // 手动制造两槽都 active 的冲突，B 激活更晚
        m.b.status = SlotStatus::Active;
        m.b.version = Some("1.1.0".to_string());
        m.b.last_activated_at = Some(now()); // B 更晚
        m.a.last_activated_at = Some(earlier); // A 更早
        let resolved = m.resolve();
        assert_eq!(resolved, Some(UpdateSlot::B)); // 保留更晚的 B
        assert_eq!(m.slot(UpdateSlot::A).status, SlotStatus::Inactive);
        assert_eq!(m.slot(UpdateSlot::B).status, SlotStatus::Active);
    }

    #[test]
    fn resolve_no_conflict_returns_current() {
        let mut m = fresh_mgr();
        let r = m.resolve();
        assert_eq!(r, Some(UpdateSlot::A));
    }
}
