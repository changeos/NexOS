//! 快照调度器 —— Cron 表达式解析与下次触发时间计算（纯函数，无外部依赖）。
//!
//! 为什么不引入 `cron` crate：workspace 未注册该依赖（规格书 §9 红线"不虚构未发布的依赖"），
//! 且 5 段标准 cron 的解析与 next-run 计算是确定性纯算法，自行实现更可控、可测。
//!
//! 支持的语法（POSIX 5 段：`分 时 日 月 周`）：
//! - `*` 任意值
//! - 单值，如 `5`
//! - 列表，如 `1,5,10`
//! - 范围，如 `1-5`
//! - 步长，如 `*/15`、`1-30/5`
//!
//! 每段域的合法范围：分 `0..=59`、时 `0..=23`、日 `1..=31`、月 `1..=12`、周 `0..=6`（0/7 均为周日）。
//!
//! 算法思路（next_run）：从 `after` 的下一分钟开始，逐分钟递增地扫描，命中第一个满足
//! (分 ∧ 时 ∧ 日 ∧ 月 ∧ 周) 的时刻即返回。逐分钟扫描在「最近一次触发的下一分钟」起算，
//! 最多扫描到下一年（防止死循环），实测远快于真实际触发间隔。该实现侧重正确性与可读性，
//! 而非性能优化（对分钟级调度足够）。

use crate::backup::CronExpr;
use crate::ServiceError;
use os_core::DateTime;
use std::fmt;

use chrono::Timelike;

// ----------------------------------------------------------------------------
// CronField：单段的取值集合
// ----------------------------------------------------------------------------

/// 单个 cron 段解析后的取值集合。
///
/// 用 64 位位图表示某域允许的所有取值——位 i 置 1 表示允许值 i。
/// 各域位数上限：分 60、时 24、日 31、月 12、周 7，均 ≤ 64，一个 u64 足够。
#[derive(Debug, Clone, Copy, Default)]
struct CronField(u64);

impl CronField {
    /// 标记值 `v` 为允许。
    fn set(&mut self, v: u32) {
        self.0 |= 1 << v;
    }
    /// `v` 是否被允许。
    fn allows(&self, v: u32) -> bool {
        (self.0 >> v) & 1 == 1
    }
    /// 是否允许任意值（位图非零）。
    fn any(&self) -> bool {
        self.0 != 0
    }
}

// ----------------------------------------------------------------------------
// CronSchedule：解析后的调度规则
// ----------------------------------------------------------------------------

/// 解析后的 cron 调度规则（5 段位图 + 原始字符串）。
///
/// 由 [`CronSchedule::parse`] 从 [`CronExpr`] 解析得到，[`CronSchedule::next_run`]
/// 计算给定时刻之后的下一次触发时间。
#[derive(Debug, Clone)]
pub struct CronSchedule {
    minute: CronField, // 0..=59
    hour: CronField,   // 0..=23
    day: CronField,    // 1..=31
    month: CronField,  // 1..=12
    dow: CronField,    // 0..=6（周日 = 0）
    /// 「日」段是否字面 `*`（决定 dom/dow 的 OR/AND 语义，见 [`Self::day_matches`]）。
    day_restricted: bool,
    /// 「周」段是否字面 `*`。
    dow_restricted: bool,
    source: String,
}

impl CronSchedule {
    /// 原始表达式字符串。
    pub fn source(&self) -> &str {
        &self.source
    }

    /// 解析 5 段 cron 表达式。非法表达式返回 [`ServiceError::Internal`]。
    pub fn parse(expr: &CronExpr) -> Result<Self, ServiceError> {
        let raw = expr.as_str();
        let parts: Vec<&str> = raw.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(ServiceError::Internal(format!(
                "cron 表达式须 5 段（分 时 日 月 周），实际 {} 段：{raw:?}",
                parts.len()
            )));
        }
        Ok(Self {
            minute: parse_field(parts[0], 0, 59).map_err(|e| field_err("minute", parts[0], &e))?,
            hour: parse_field(parts[1], 0, 23).map_err(|e| field_err("hour", parts[1], &e))?,
            day: parse_field(parts[2], 1, 31).map_err(|e| field_err("day", parts[2], &e))?,
            month: parse_field(parts[3], 1, 12).map_err(|e| field_err("month", parts[3], &e))?,
            dow: parse_field(parts[4], 0, 6).map_err(|e| field_err("dow", parts[4], &e))?,
            day_restricted: is_restricted(parts[2]),
            dow_restricted: is_restricted(parts[4]),
            source: raw.to_string(),
        })
    }

    /// 计算严格晚于 `after` 的下一次触发时间。
    ///
    /// 从 `after` 的下一分钟（秒归零）开始逐分钟扫描，最多扫描 366 天
    /// （覆盖最长「2 月 29 日每 4 年一次」之外的全部合法月/日组合场景）。
    /// 若一年内无任何匹配（极罕见，如 `2 2 29 2 *` 在非闰年的下一闰年远超一年），
    /// 返回 `Err`——避免无限循环。
    pub fn next_run(&self, after: &DateTime) -> Result<DateTime, ServiceError> {
        // 从 after 的下一分钟、秒/纳秒归零开始（严格大于 after）。
        let mut cur = after
            .with_second(0)
            .and_then(|t| t.with_nanosecond(0))
            .ok_or_else(|| ServiceError::Internal("时间归零失败".into()))?;
        cur += chrono::Duration::minutes(1);

        // 最多扫描 4 年零 1 天（覆盖「2 月 29 日每 4 年一次」）。
        let limit = *after + chrono::Duration::days(366 * 4 + 1);
        while cur < limit {
            if self.matches(cur) {
                return Ok(cur);
            }
            cur += chrono::Duration::minutes(1);
        }
        Err(ServiceError::Internal(format!(
            "cron 表达式在 {} 后 4 年内无可触发时刻：{}",
            after, self.source
        )))
    }

    /// 判断给定时刻是否匹配本调度规则。
    fn matches<Tz: chrono::TimeZone>(&self, t: chrono::DateTime<Tz>) -> bool {
        use chrono::{Datelike, Timelike};
        self.minute.allows(t.minute())
            && self.hour.allows(t.hour())
            && self.month.allows(t.month())
            && self.day_matches(t)
    }

    /// 日 + 周的匹配。
    ///
    /// 传统 cron（Vixie cron）语义：
    /// - 日与周**都**被显式限制（非 `*`）→ 二者取并集（OR）。
    /// - 否则（至少一方为 `*`）→ 取交集（AND），即两者都须匹配。
    ///
    /// 例如 `0 0 29 2 *`（仅日段受限、周段为 `*`）只在 2 月 29 日触发；
    /// `0 0 * * 0`（仅周段受限）在每个周日触发；
    /// `0 0 1 * 0`（日 1 + 周日均受限）在每月 1 号**或**周日触发（OR）。
    fn day_matches<Tz: chrono::TimeZone>(&self, t: chrono::DateTime<Tz>) -> bool {
        use chrono::Datelike;
        let dom_ok = self.day.allows(t.day());
        // chrono Weekday: Mon=1 .. Sun=7；cron 周日=0。
        let cron_dow = t.weekday().num_days_from_sunday();
        let dow_ok = self.dow.allows(cron_dow);
        if self.day_restricted && self.dow_restricted {
            dom_ok || dow_ok
        } else {
            dom_ok && dow_ok
        }
    }
}

/// 判断 cron 段是否「显式受限」——即字面**非** `*`（允许 `*/k` 视为受限，因它限制子集）。
/// 仅当整个段恰好等于 `*` 时视为未受限。
fn is_restricted(s: &str) -> bool {
    s.trim() != "*"
}

impl fmt::Display for CronSchedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

fn field_err(field: &str, raw: &str, e: &str) -> ServiceError {
    ServiceError::Internal(format!("cron {field} 段非法 {raw:?}: {e}"))
}

/// 解析单个 cron 段。
///
/// `min`/`max` 为该域的闭区间边界。返回该域允许的取值位图。
fn parse_field(s: &str, min: u32, max: u32) -> Result<CronField, String> {
    let mut f = CronField::default();
    for item in s.split(',') {
        parse_item(item, min, max, &mut f)?;
    }
    if !f.any() {
        return Err("段为空（无任何匹配值）".to_string());
    }
    Ok(f)
}

/// 解析单个项（可能是 `*`、`n`、`a-b`、`*/k`、`a-b/k`）。
fn parse_item(item: &str, min: u32, max: u32, f: &mut CronField) -> Result<(), String> {
    // 分离步长后缀 `/k`
    let (range_part, step) = match item.split_once('/') {
        Some((rp, sp)) => {
            let step: u32 = sp.parse().map_err(|_| format!("步长非整数: {sp:?}"))?;
            if step == 0 {
                return Err("步长不能为 0".to_string());
            }
            (rp, step)
        }
        None => (item, 1),
    };

    // 确定范围 [lo, hi]
    let (lo, hi) = if range_part == "*" {
        (min, max)
    } else if let Some((a, b)) = range_part.split_once('-') {
        let a = parse_value(a, min, max)?;
        let b = parse_value(b, min, max)?;
        if a > b {
            return Err(format!("范围下界 {a} 大于上界 {b}"));
        }
        (a, b)
    } else {
        let v = parse_value(range_part, min, max)?;
        (v, v)
    };

    let mut cur = lo;
    while cur <= hi {
        f.set(cur);
        cur += step;
        // 防止 step 溢出导致 cur 跳过 hi 之后仍 <= hi 的极端情况（u32 加法环绕）
        if cur < lo {
            break;
        }
    }
    Ok(())
}

/// 解析单值，校验边界。周日的 `7` 规范化为 `0`（仅对 dow 域，由调用方 max 控制）。
fn parse_value(s: &str, min: u32, max: u32) -> Result<u32, String> {
    let v: u32 = s.parse().map_err(|_| format!("非整数值: {s:?}"))?;
    // 兼容 cron 周日 = 7 的写法（仅当域为 dow，即 min=0 max=6 时）
    let normalized = if min == 0 && max == 6 && v == 7 { 0 } else { v };
    if normalized < min || normalized > max {
        return Err(format!("值 {v} 越界（{min}..={max}）"));
    }
    Ok(normalized)
}

// ----------------------------------------------------------------------------
// 调度策略（事件触发 / cron 触发）
// ----------------------------------------------------------------------------

/// 快照调度策略类型。
///
/// - `Cron`：按 cron 周期触发（如每天 03:00）。
/// - `Event`：事件驱动（如"数据集变更后立即"），由上层事件源触发，不参与 cron next_run 计算。
/// - `Manual`：仅人工触发，无自动调度。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedulePolicy {
    /// 按 cron 表达式周期触发。
    Cron(CronExpr),
    /// 事件触发（不在本调度器内计算 next_run；next_run 恒为 None）。
    Event(String),
    /// 仅手动触发。
    Manual,
}

impl SchedulePolicy {
    /// 计算严格晚于 `after` 的下次触发时间。
    ///
    /// `Cron` → 解析并计算；`Event`/`Manual` → `None`（无自动调度）。
    pub fn next_run(&self, after: &DateTime) -> Result<Option<DateTime>, ServiceError> {
        match self {
            SchedulePolicy::Cron(expr) => {
                let sched = CronSchedule::parse(expr)?;
                Ok(Some(sched.next_run(after)?))
            }
            SchedulePolicy::Event(_) | SchedulePolicy::Manual => Ok(None),
        }
    }
}

impl fmt::Display for SchedulePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SchedulePolicy::Cron(e) => write!(f, "cron({e})"),
            SchedulePolicy::Event(e) => write!(f, "event({e})"),
            SchedulePolicy::Manual => f.write_str("manual"),
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

    fn dt(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    fn cron(s: &str) -> CronExpr {
        CronExpr::new(s)
    }

    // —— parse_field 边界 ——

    #[test]
    fn parse_star_allows_all() {
        let f = parse_field("*", 0, 59).unwrap();
        for v in 0..=59 {
            assert!(f.allows(v), "应允许 {v}");
        }
    }

    #[test]
    fn parse_step_range() {
        // */15 → 0,15,30,45
        let f = parse_field("*/15", 0, 59).unwrap();
        assert!(f.allows(0) && f.allows(15) && f.allows(30) && f.allows(45));
        assert!(!f.allows(5));
    }

    #[test]
    fn parse_list_and_range() {
        let f = parse_field("1,5,10-12", 0, 59).unwrap();
        assert!(f.allows(1) && f.allows(5) && f.allows(10) && f.allows(11) && f.allows(12));
        assert!(!f.allows(2));
    }

    #[test]
    fn parse_range_with_step() {
        // 1-30/5 → 1,6,11,16,21,26
        let f = parse_field("1-30/5", 0, 59).unwrap();
        assert!(f.allows(1) && f.allows(6) && f.allows(26));
        assert!(!f.allows(5) && !f.allows(31));
    }

    #[test]
    fn parse_out_of_range_rejected() {
        assert!(parse_field("60", 0, 59).is_err());
        assert!(parse_field("24", 0, 23).is_err());
        assert!(parse_field("0", 1, 31).is_err()); // 日从 1 起
    }

    #[test]
    fn parse_dow_sunday_seven() {
        // 周域 0..=6，7 应等价于 0
        let f = parse_field("7", 0, 6).unwrap();
        assert!(f.allows(0));
    }

    #[test]
    fn parse_inverted_range_rejected() {
        assert!(parse_field("5-3", 0, 59).is_err());
    }

    #[test]
    fn parse_step_zero_rejected() {
        assert!(parse_field("*/0", 0, 59).is_err());
    }

    #[test]
    fn schedule_wrong_segment_count_rejected() {
        assert!(CronSchedule::parse(&cron("0 3 * *")).is_err());
        assert!(CronSchedule::parse(&cron("0 3 * * * extra")).is_err());
    }

    // —— next_run 计算 ——

    #[test]
    fn next_run_daily_at_3am() {
        // 每天 03:00
        let s = CronSchedule::parse(&cron("0 3 * * *")).unwrap();
        // 2024-01-15 10:00 → 下次 2024-01-16 03:00
        let next = s.next_run(&dt(2024, 1, 15, 10, 0)).unwrap();
        assert_eq!(next, dt(2024, 1, 16, 3, 0));
    }

    #[test]
    fn next_run_same_day_before_trigger() {
        let s = CronSchedule::parse(&cron("0 3 * * *")).unwrap();
        // 2024-01-15 02:00 → 当天 03:00
        let next = s.next_run(&dt(2024, 1, 15, 2, 0)).unwrap();
        assert_eq!(next, dt(2024, 1, 15, 3, 0));
    }

    #[test]
    fn next_run_exact_minute_not_included() {
        // 严格大于 after：03:00:30 → 当天下一分钟不在 03:00（已过），故次日
        let s = CronSchedule::parse(&cron("0 3 * * *")).unwrap();
        let after = Utc.with_ymd_and_hms(2024, 1, 15, 3, 0, 30).unwrap();
        let next = s.next_run(&after).unwrap();
        assert_eq!(next, dt(2024, 1, 16, 3, 0));
    }

    #[test]
    fn next_run_every_15_minutes() {
        let s = CronSchedule::parse(&cron("*/15 * * * *")).unwrap();
        // 10:07 → 10:15
        let after = Utc.with_ymd_and_hms(2024, 1, 15, 10, 7, 0).unwrap();
        let next = s.next_run(&after).unwrap();
        assert_eq!(next, dt(2024, 1, 15, 10, 15));
    }

    #[test]
    fn next_run_weekly_sunday() {
        // 每周日 02:00
        let s = CronSchedule::parse(&cron("0 2 * * 0")).unwrap();
        // 2024-01-15 是周一 → 下个周日 2024-01-21
        let next = s.next_run(&dt(2024, 1, 15, 10, 0)).unwrap();
        assert_eq!(next, dt(2024, 1, 21, 2, 0));
    }

    #[test]
    fn next_run_monthly_first() {
        // 每月 1 号 00:00
        let s = CronSchedule::parse(&cron("0 0 1 * *")).unwrap();
        // 2024-01-15 → 2024-02-01
        let next = s.next_run(&dt(2024, 1, 15, 10, 0)).unwrap();
        assert_eq!(next, dt(2024, 2, 1, 0, 0));
    }

    #[test]
    fn next_run_specific_month_day() {
        // 2 月 29 日（闰年）2024 是闰年
        let s = CronSchedule::parse(&cron("0 0 29 2 *")).unwrap();
        // 2024-01-15 → 2024-02-29
        let next = s.next_run(&dt(2024, 1, 15, 10, 0)).unwrap();
        assert_eq!(next, dt(2024, 2, 29, 0, 0));
    }

    #[test]
    fn next_run_february_29_non_leap_skips() {
        // 2023 非 2-29；从 2023-01-01 出发应跳到 2024-02-29（4 年内）
        let s = CronSchedule::parse(&cron("0 0 29 2 *")).unwrap();
        let next = s.next_run(&dt(2023, 1, 1, 0, 0)).unwrap();
        assert_eq!(next, dt(2024, 2, 29, 0, 0));
    }

    #[test]
    fn next_run_impossible_in_window_errors() {
        // 2 月 30 日永远不存在 → 4 年内扫描无匹配 → Err
        let s = CronSchedule::parse(&cron("0 0 30 2 *")).unwrap();
        assert!(s.next_run(&dt(2024, 1, 1, 0, 0)).is_err());
    }

    // —— SchedulePolicy ——

    #[test]
    fn schedule_policy_cron_next_run() {
        let p = SchedulePolicy::Cron(cron("0 3 * * *"));
        let next = p.next_run(&dt(2024, 1, 15, 10, 0)).unwrap();
        assert_eq!(next, Some(dt(2024, 1, 16, 3, 0)));
    }

    #[test]
    fn schedule_policy_event_no_next_run() {
        let p = SchedulePolicy::Event("dataset-changed".into());
        assert_eq!(p.next_run(&dt(2024, 1, 15, 10, 0)).unwrap(), None);
    }

    #[test]
    fn schedule_policy_manual_no_next_run() {
        let p = SchedulePolicy::Manual;
        assert_eq!(p.next_run(&dt(2024, 1, 15, 10, 0)).unwrap(), None);
    }
}
