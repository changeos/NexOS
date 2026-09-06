//! 灾备演练（Disaster Recovery Drill）—— 计划/结果模型 + 状态机（纯函数）。
//!
//! 灾备演练目的：周期性验证备份可恢复性——从某快照在隔离环境做 restore，校验数据完整性，
//! 不影响生产。本模块定义 `DrillPlan`（演练计划）与 `DrillResult`（演练结果）及状态机，
//! 纯数据 + 转移函数，无副作用，便于离线测试与 dry-run。
//!
//! 状态机：
//! ```text
//! Pending → Running ─┬─→ Succeeded
//!                    ├─→ Failed(reason)
//!                    └─→ Cancelled
//! ```
//! 非法转移（如 `Succeeded → Running`）返回 `Err`。

use os_core::{DatasetId, DateTime, Deserialize, Serialize, SnapshotId};

// ----------------------------------------------------------------------------
// DrillPlan：演练计划
// ----------------------------------------------------------------------------

/// 灾备演练计划——描述「从哪个快照恢复到哪个隔离目标 + 校验什么」。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrillPlan {
    /// 演练计划名（人类可读，如 `"weekly-restore-check"`）。
    pub name: String,
    /// 源快照（演练恢复的起点）。
    pub source: SnapshotId,
    /// 隔离恢复目标数据集（绝不能是生产数据集，须独立命名空间如 `drill/<plan>`）。
    pub target: DatasetId,
    /// 数据完整性校验项（如 `["checksum", "row_count"]`）。
    pub checks: Vec<String>,
    /// 超时（秒）；超时判 Failed。
    pub timeout_secs: u64,
}

// ----------------------------------------------------------------------------
// DrillStatus：状态机
// ----------------------------------------------------------------------------

/// 演练运行状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrillStatus {
    /// 已创建、未启动。
    Pending,
    /// 运行中。
    Running,
    /// 成功完成。
    Succeeded,
    /// 失败（含原因）。
    Failed,
    /// 已取消。
    Cancelled,
}

impl DrillStatus {
    /// 是否为终态（不可再转移）。
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            DrillStatus::Succeeded | DrillStatus::Failed | DrillStatus::Cancelled
        )
    }

    /// 合法转移校验：返回从 `self` 到 `next` 是否合法。
    ///
    /// 合法转移：
    /// - `Pending → Running | Cancelled`
    /// - `Running → Succeeded | Failed | Cancelled`
    /// - 终态不可转移。
    pub fn can_transition(self, next: DrillStatus) -> bool {
        use DrillStatus::*;
        matches!(
            (self, next),
            (Pending, Running | Cancelled) | (Running, Succeeded | Failed | Cancelled)
        )
    }
}

// ----------------------------------------------------------------------------
// DrillResult：演练结果
// ----------------------------------------------------------------------------

/// 灾备演练结果（含状态、时间戳、校验明细）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrillResult {
    /// 关联的演练计划。
    pub plan: DrillPlan,
    /// 当前状态。
    pub status: DrillStatus,
    /// 失败/取消原因（Succeeded 时为 None）。
    pub reason: Option<String>,
    /// 校验明细（每项 check 的通过情况；key = check 名，value = true 通过 / false 失败）。
    pub check_results: Vec<(String, bool)>,
    /// 开始时间。
    pub started_at: Option<DateTime>,
    /// 结束时间（终态时填）。
    pub finished_at: Option<DateTime>,
    /// 耗时（秒）。
    pub duration_secs: u64,
}

impl DrillResult {
    /// 创建一个 Pending 状态的初始结果。
    pub fn new(plan: DrillPlan) -> Self {
        Self {
            plan,
            status: DrillStatus::Pending,
            reason: None,
            check_results: Vec::new(),
            started_at: None,
            finished_at: None,
            duration_secs: 0,
        }
    }

    /// 状态转移：启动演练（Pending → Running）。
    ///
    /// 记录 `started_at = now`。非法转移返回 `Err`。
    pub fn start(&mut self, now: DateTime) -> Result<(), String> {
        if !self.status.can_transition(DrillStatus::Running) {
            return Err(format!(
                "非法状态转移：{:?} → Running（仅 Pending 可启动）",
                self.status
            ));
        }
        self.status = DrillStatus::Running;
        self.started_at = Some(now);
        Ok(())
    }

    /// 状态转移：标记成功（Running → Succeeded）。
    ///
    /// 校验明细全填，`finished_at = now`，计算 `duration_secs`。
    pub fn succeed(&mut self, checks: Vec<(String, bool)>, now: DateTime) -> Result<(), String> {
        if !self.status.can_transition(DrillStatus::Succeeded) {
            return Err(format!(
                "非法状态转移：{:?} → Succeeded（仅 Running 可成功）",
                self.status
            ));
        }
        self.status = DrillStatus::Succeeded;
        self.check_results = checks;
        self.finished_at = Some(now);
        self.duration_secs = self.compute_duration(now);
        Ok(())
    }

    /// 状态转移：标记失败（Running → Failed）。
    pub fn fail(&mut self, reason: impl Into<String>, now: DateTime) -> Result<(), String> {
        if !self.status.can_transition(DrillStatus::Failed) {
            return Err(format!(
                "非法状态转移：{:?} → Failed（仅 Running 可失败）",
                self.status
            ));
        }
        self.status = DrillStatus::Failed;
        self.reason = Some(reason.into());
        self.finished_at = Some(now);
        self.duration_secs = self.compute_duration(now);
        Ok(())
    }

    /// 状态转移：取消（Pending/Running → Cancelled）。
    pub fn cancel(&mut self, reason: impl Into<String>, now: DateTime) -> Result<(), String> {
        if !self.status.can_transition(DrillStatus::Cancelled) {
            return Err(format!(
                "非法状态转移：{:?} → Cancelled（终态不可取消）",
                self.status
            ));
        }
        self.status = DrillStatus::Cancelled;
        self.reason = Some(reason.into());
        self.finished_at = Some(now);
        self.duration_secs = self.compute_duration(now);
        Ok(())
    }

    /// 是否所有校验项均通过（Succeeded 时有意义；非终态返回 false）。
    pub fn all_checks_passed(&self) -> bool {
        self.status == DrillStatus::Succeeded
            && !self.check_results.is_empty()
            && self.check_results.iter().all(|(_, ok)| *ok)
    }

    /// 计算 duration（finished - started）。未结束或无 started 返回 0。
    fn compute_duration(&self, finished: DateTime) -> u64 {
        match self.started_at {
            Some(s) => (finished - s).num_seconds().max(0) as u64,
            None => 0,
        }
    }
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use chrono::Utc;

    fn plan() -> DrillPlan {
        DrillPlan {
            name: "weekly".into(),
            source: SnapshotId::new("tank/media@snap1"),
            target: DatasetId::new("drill/weekly"),
            checks: vec!["checksum".into(), "row_count".into()],
            timeout_secs: 3600,
        }
    }

    fn dt(y: i32, mo: u32, d: u32, h: u32) -> DateTime {
        Utc.with_ymd_and_hms(y, mo, d, h, 0, 0).unwrap()
    }

    // —— 状态机合法性 ——

    #[test]
    fn terminal_status_is_terminal() {
        assert!(DrillStatus::Succeeded.is_terminal());
        assert!(DrillStatus::Failed.is_terminal());
        assert!(DrillStatus::Cancelled.is_terminal());
        assert!(!DrillStatus::Pending.is_terminal());
        assert!(!DrillStatus::Running.is_terminal());
    }

    #[test]
    fn legal_transitions() {
        assert!(DrillStatus::Pending.can_transition(DrillStatus::Running));
        assert!(DrillStatus::Pending.can_transition(DrillStatus::Cancelled));
        assert!(DrillStatus::Running.can_transition(DrillStatus::Succeeded));
        assert!(DrillStatus::Running.can_transition(DrillStatus::Failed));
        assert!(DrillStatus::Running.can_transition(DrillStatus::Cancelled));
    }

    #[test]
    fn illegal_transitions() {
        // 终态不可再转移
        for term in [
            DrillStatus::Succeeded,
            DrillStatus::Failed,
            DrillStatus::Cancelled,
        ] {
            for next in [
                DrillStatus::Pending,
                DrillStatus::Running,
                DrillStatus::Succeeded,
                DrillStatus::Failed,
                DrillStatus::Cancelled,
            ] {
                assert!(!term.can_transition(next), "{term:?} → {next:?} 应非法");
            }
        }
        // Pending 不能直接到 Succeeded/Failed
        assert!(!DrillStatus::Pending.can_transition(DrillStatus::Succeeded));
        assert!(!DrillStatus::Pending.can_transition(DrillStatus::Failed));
        // Running 不能回到 Pending
        assert!(!DrillStatus::Running.can_transition(DrillStatus::Pending));
    }

    // —— DrillResult 转移 ——

    #[test]
    fn full_success_path() {
        let mut r = DrillResult::new(plan());
        assert_eq!(r.status, DrillStatus::Pending);
        let start = dt(2024, 1, 1, 10);
        r.start(start).unwrap();
        assert_eq!(r.status, DrillStatus::Running);
        assert_eq!(r.started_at, Some(start));

        let end = dt(2024, 1, 1, 11);
        r.succeed(
            vec![("checksum".into(), true), ("row_count".into(), true)],
            end,
        )
        .unwrap();
        assert_eq!(r.status, DrillStatus::Succeeded);
        assert_eq!(r.duration_secs, 3600);
        assert!(r.all_checks_passed());
    }

    #[test]
    fn fail_with_reason() {
        let mut r = DrillResult::new(plan());
        r.start(dt(2024, 1, 1, 10)).unwrap();
        r.fail("restore 超时", dt(2024, 1, 1, 11)).unwrap();
        assert_eq!(r.status, DrillStatus::Failed);
        assert_eq!(r.reason.as_deref(), Some("restore 超时"));
    }

    #[test]
    fn cancel_from_pending() {
        let mut r = DrillResult::new(plan());
        r.cancel("手动取消", dt(2024, 1, 1, 10)).unwrap();
        assert_eq!(r.status, DrillStatus::Cancelled);
        assert_eq!(r.reason.as_deref(), Some("手动取消"));
    }

    #[test]
    fn cancel_from_running() {
        let mut r = DrillResult::new(plan());
        r.start(dt(2024, 1, 1, 10)).unwrap();
        r.cancel("超时取消", dt(2024, 1, 1, 12)).unwrap();
        assert_eq!(r.status, DrillStatus::Cancelled);
    }

    #[test]
    fn illegal_start_rejected() {
        let mut r = DrillResult::new(plan());
        r.start(dt(2024, 1, 1, 10)).unwrap();
        // Running → start 非法
        assert!(r.start(dt(2024, 1, 1, 11)).is_err());
    }

    #[test]
    fn succeed_from_pending_rejected() {
        let mut r = DrillResult::new(plan());
        // 未启动直接成功 → 非法
        let err = r
            .succeed(vec![("checksum".into(), true)], dt(2024, 1, 1, 10))
            .unwrap_err();
        assert!(err.contains("非法状态转移"));
    }

    #[test]
    fn failed_terminal_rejects_further() {
        let mut r = DrillResult::new(plan());
        r.start(dt(2024, 1, 1, 10)).unwrap();
        r.fail("err", dt(2024, 1, 1, 11)).unwrap();
        // 终态再转移
        assert!(r.fail("again", dt(2024, 1, 1, 12)).is_err());
        assert!(r.cancel("x", dt(2024, 1, 1, 12)).is_err());
    }

    #[test]
    fn all_checks_passed_requires_all_true() {
        let mut r = DrillResult::new(plan());
        r.start(dt(2024, 1, 1, 10)).unwrap();
        // 一项失败 → all_checks_passed false
        r.succeed(
            vec![("checksum".into(), true), ("row_count".into(), false)],
            dt(2024, 1, 1, 11),
        )
        .unwrap();
        assert!(!r.all_checks_passed());
    }

    #[test]
    fn duration_zero_when_not_started() {
        let mut r = DrillResult::new(plan());
        // 未 start 直接 cancel（Pending → Cancelled）：duration 0
        r.cancel("x", dt(2024, 1, 1, 10)).unwrap();
        assert_eq!(r.duration_secs, 0);
    }
}
