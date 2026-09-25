//! 3-2-1 备份保留策略 —— 过期快照筛选算法（纯函数，无外部依赖）。
//!
//! 3-2-1 原则：3 份数据、2 种介质、1 份异地。本模块聚焦「保留多少份」的纯逻辑：
//! 给定一组带时间戳的快照 + 保留规则 → 计算应删除哪些（即不在任何保留桶内的快照）。
//!
//! 保留桶（GFS，Grandfather-Father-Son）：
//! - `keep_last`：最近 N 份无条件保留
//! - `keep_hourly`/`keep_daily`/`keep_weekly`/`keep_monthly`：按桶分组的最近 N 份
//! - `keep_days`：最近 N 天内的全部保留（与上面桶并集）
//!
//! 筛选算法（[`select_expired`]）：
//! 1. 对每个桶，按时间倒序取前 N 份（同桶内取该桶边界对齐后的第一份）。
//! 2. 所有桶的保留集取并集。
//! 3. `keep_days` 作为额外的时间窗口白名单。
//! 4. 不在任何保留集内的快照 → 过期（应删除）。
//!
//! 该算法是确定性的纯函数，便于单元测试与离线 dry-run（灾备演练复用）。

use os_core::DateTime;

// ----------------------------------------------------------------------------
// RetentionRule：增强版保留规则（GFS 分桶）
// ----------------------------------------------------------------------------

/// 3-2-1 / GFS 保留规则（Grandfather-Father-Son 分桶保留）。
///
/// 与 [`crate::backup::RetentionPolicy`] 的关系：后者是契约层最简模型（keep_last + keep_days），
/// 本规则是增强版，支持 hourly/daily/weekly/monthly 分桶。两者可互转（见 [`RetentionRule::to_simple`]）。
///
/// 各字段为 0 表示「该桶不保留」（即该桶所有快照都可能过期）。
/// 全为 0 时所有快照都过期（无意义配置，调用方应阻止）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionRule {
    /// 最近 N 份无条件保留（不参与分桶，最高优先级）。
    pub keep_last: u32,
    /// 保留最近 N 天内的全部快照（时间窗口白名单）。
    pub keep_days: u32,
    /// 最近 N 个「整点」快照（按小时桶）。
    pub keep_hourly: u32,
    /// 最近 N 个「每天」快照（按天桶）。
    pub keep_daily: u32,
    /// 最近 N 个「每周」快照（按 ISO 周桶）。
    pub keep_weekly: u32,
    /// 最近 N 个「每月」快照（按自然月桶）。
    pub keep_monthly: u32,
}

impl RetentionRule {
    /// 构造一个常见的 3-2-1 备份保留规则（7 天每天 / 4 周每周 / 12 月每月）。
    pub fn default_321() -> Self {
        Self {
            keep_last: 7,
            keep_days: 7,
            keep_hourly: 0,
            keep_daily: 7,
            keep_weekly: 4,
            keep_monthly: 12,
        }
    }

    /// 转为最简 [`crate::backup::RetentionPolicy`]（丢弃分桶信息，仅保留 keep_last/keep_days）。
    pub fn to_simple(&self) -> crate::backup::RetentionPolicy {
        crate::backup::RetentionPolicy {
            keep_last: self.keep_last,
            keep_days: self.keep_days,
        }
    }
}

impl Default for RetentionRule {
    fn default() -> Self {
        Self::default_321()
    }
}

// ----------------------------------------------------------------------------
// 带时间戳的快照（算法输入抽象，避免耦合 os-storage::Snapshot）
// ----------------------------------------------------------------------------

/// 供保留算法使用的快照抽象。
///
/// 算法只关心「快照标识 + 创建时间」，不关心快照内容/大小。用泛型 ID 避免耦合具体类型——
/// 调用方传 `&str`/`SnapshotId`/`String` 均可（只要可克隆比较）。
#[derive(Debug, Clone)]
pub struct TimedSnapshot<I: Clone + PartialEq> {
    /// 快照标识（如 SnapshotId 的 as_str）。
    pub id: I,
    /// 创建时间（UTC）。
    pub created: DateTime,
}

impl<I: Clone + PartialEq> TimedSnapshot<I> {
    /// 构造。
    pub fn new(id: I, created: DateTime) -> Self {
        Self { id, created }
    }
}

// ----------------------------------------------------------------------------
// 桶键计算
// ----------------------------------------------------------------------------

use chrono::{Datelike, Timelike};

/// 小时桶键（年-月-日-时）。
fn hour_key(t: &DateTime) -> (i32, u32, u32, u32) {
    (t.year(), t.month(), t.day(), t.hour())
}
/// 日桶键（年-月-日）。
fn day_key(t: &DateTime) -> (i32, u32, u32) {
    (t.year(), t.month(), t.day())
}
/// 周桶键（年 + ISO 周）。用 chrono 的 iso_week。
fn week_key(t: &DateTime) -> (i32, u32) {
    let iw = t.iso_week();
    (iw.year(), iw.week())
}
/// 月桶键（年-月）。
fn month_key(t: &DateTime) -> (i32, u32) {
    (t.year(), t.month())
}

// ----------------------------------------------------------------------------
// 过期筛选算法
// ----------------------------------------------------------------------------

/// 计算应过期的快照标识列表。
///
/// 输入：一组快照（任意顺序）+ 保留规则 + 当前参考时间 `now`（用于 `keep_days` 时间窗）。
/// 输出：不在任何保留桶内的快照（即应删除的）。
///
/// 算法：
/// 1. 按创建时间降序排序。
/// 2. `keep_last`：取前 N 份加入保留集。
/// 3. `keep_days`：created >= (now - keep_days 天) 的加入保留集。
/// 4. 对 hourly/daily/weekly/monthly 各桶：遍历（已降序），每个新桶键取第一份，累计到 N 份止。
/// 5. 不在保留集内的 → 过期。
///
/// 返回的过期列表按时间升序（最老的先删，便于按序 `zfs destroy`）。
pub fn select_expired<I: Clone + PartialEq>(
    snapshots: &[TimedSnapshot<I>],
    rule: &RetentionRule,
    now: &DateTime,
) -> Vec<I> {
    if snapshots.is_empty() {
        return Vec::new();
    }

    // 降序（最新在前）
    let mut sorted: Vec<&TimedSnapshot<I>> = snapshots.iter().collect();
    sorted.sort_by_key(|s| std::cmp::Reverse(s.created));

    let mut kept: Vec<&TimedSnapshot<I>> = Vec::new();

    // 1. keep_last（最高优先级）
    for s in sorted.iter().take(rule.keep_last as usize) {
        push_unique(&mut kept, s);
    }

    // 2. keep_days 时间窗
    if rule.keep_days > 0 {
        let cutoff = *now - checked_days(rule.keep_days as i64);
        for s in &sorted {
            if s.created >= cutoff {
                push_unique(&mut kept, s);
            }
        }
    }

    // 3. 分桶保留
    pick_bucket(&sorted, rule.keep_hourly as usize, hour_key, &mut kept);
    pick_bucket(&sorted, rule.keep_daily as usize, day_key, &mut kept);
    pick_bucket(&sorted, rule.keep_weekly as usize, week_key, &mut kept);
    pick_bucket(&sorted, rule.keep_monthly as usize, month_key, &mut kept);

    // 4. 不在保留集 → 过期；输出按时间升序（老的先删）
    let mut expired: Vec<&TimedSnapshot<I>> = sorted
        .iter()
        .copied()
        .filter(|s| !kept.iter().any(|k| k.id == s.id))
        .collect();
    expired.sort_by_key(|s| s.created);
    expired.into_iter().map(|s| s.id.clone()).collect()
}

/// 去重推入：仅当 id 未在 kept 中时推入。
fn push_unique<'a, I: Clone + PartialEq>(
    kept: &mut Vec<&'a TimedSnapshot<I>>,
    s: &'a TimedSnapshot<I>,
) {
    if !kept.iter().any(|k| k.id == s.id) {
        kept.push(s);
    }
}

/// 分桶挑选：遍历降序快照，每个新桶键取第一份，累计到 `count` 份止。
fn pick_bucket<'a, I: Clone + PartialEq, K: Eq>(
    sorted: &[&'a TimedSnapshot<I>],
    count: usize,
    key_fn: fn(&DateTime) -> K,
    kept: &mut Vec<&'a TimedSnapshot<I>>,
) {
    if count == 0 {
        return;
    }
    let mut picked = 0usize;
    let mut last_key: Option<K> = None;
    for s in sorted {
        let k = key_fn(&s.created);
        if last_key.as_ref() == Some(&k) {
            continue; // 同桶跳过（已取该桶第一份）
        }
        last_key = Some(k);
        // 该桶的第一份（最新）→ 保留
        if !kept.iter().any(|kk| kk.id == s.id) {
            kept.push(*s);
        }
        picked += 1;
        if picked >= count {
            break;
        }
    }
}

/// 安全构造 `chrono::Duration::days`（i64 越界返回 None，但 days 一般小）。
fn checked_days(d: i64) -> chrono::Duration {
    chrono::Duration::days(d)
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use chrono::Utc;

    fn t(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    fn sn(id: &str, created: DateTime) -> TimedSnapshot<String> {
        TimedSnapshot::new(id.to_string(), created)
    }

    #[test]
    fn empty_input_returns_empty() {
        let rule = RetentionRule::default_321();
        let now = t(2024, 1, 31, 0, 0);
        let expired = select_expired::<String>(&[], &rule, &now);
        assert!(expired.is_empty());
    }

    #[test]
    fn keep_last_protects_newest_n() {
        // 10 份，keep_last=3 → 最早 7 份过期
        let snaps: Vec<TimedSnapshot<String>> = (1..=10)
            .map(|i| sn(&format!("s{i}"), t(2024, 1, i, 0, 0)))
            .collect();
        let rule = RetentionRule {
            keep_last: 3,
            keep_days: 0,
            keep_hourly: 0,
            keep_daily: 0,
            keep_weekly: 0,
            keep_monthly: 0,
        };
        let now = t(2024, 1, 31, 0, 0);
        let expired = select_expired(&snaps, &rule, &now);
        // s1..s7 过期（最早 7 份）
        assert_eq!(expired.len(), 7);
        assert_eq!(expired[0], "s1");
        assert_eq!(expired[6], "s7");
    }

    #[test]
    fn keep_days_time_window_protects_recent() {
        // 5 份：1/25、1/26、1/27、1/28、1/29；now=1/30；keep_days=3 → 1/27 之后保留
        let snaps = vec![
            sn("a", t(2024, 1, 25, 0, 0)),
            sn("b", t(2024, 1, 26, 0, 0)),
            sn("c", t(2024, 1, 27, 0, 0)),
            sn("d", t(2024, 1, 28, 0, 0)),
            sn("e", t(2024, 1, 29, 0, 0)),
        ];
        let rule = RetentionRule {
            keep_last: 0,
            keep_days: 3,
            keep_hourly: 0,
            keep_daily: 0,
            keep_weekly: 0,
            keep_monthly: 0,
        };
        // now=1/30 00:00，cutoff = 1/27 00:00；created >= cutoff 保留
        let now = t(2024, 1, 30, 0, 0);
        let expired = select_expired(&snaps, &rule, &now);
        // a、b 过期（c/d/e 在窗内）
        assert_eq!(expired, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn daily_bucket_keeps_one_per_day() {
        // 1/10 每小时一份（24 份）；keep_daily=3 → 最近 3 天每天各 1 份……
        // 但都在同一天，故仅 1 个桶；这里改为跨天测试
        let snaps = vec![
            sn("d1a", t(2024, 1, 1, 3, 0)),
            sn("d1b", t(2024, 1, 1, 9, 0)),
            sn("d2a", t(2024, 1, 2, 3, 0)),
            sn("d2b", t(2024, 1, 2, 9, 0)),
            sn("d3a", t(2024, 1, 3, 3, 0)),
            sn("d3b", t(2024, 1, 3, 9, 0)),
        ];
        let rule = RetentionRule {
            keep_last: 0,
            keep_days: 0,
            keep_hourly: 0,
            keep_daily: 2,
            keep_weekly: 0,
            keep_monthly: 0,
        };
        let now = t(2024, 1, 4, 0, 0);
        let expired = select_expired(&snaps, &rule, &now);
        // keep_daily=2：最近 2 天（1/3、1/2）各取最新一份 → d3b、d2b 保留
        // 其余 d1a/d1b/d2a/d3a 过期
        assert_eq!(expired.len(), 4);
        assert!(expired.contains(&"d1a".to_string()));
        assert!(expired.contains(&"d1b".to_string()));
        assert!(expired.contains(&"d2a".to_string()));
        assert!(expired.contains(&"d3a".to_string()));
    }

    #[test]
    fn union_of_buckets_and_keep_last() {
        // keep_last=1 + keep_daily=1：最新一份由 keep_last 保，daily 桶再保一份（去重后仍 1 份/天）
        let snaps = vec![
            sn("d1", t(2024, 1, 1, 12, 0)),
            sn("d2", t(2024, 1, 2, 12, 0)),
            sn("d3", t(2024, 1, 3, 12, 0)),
        ];
        let rule = RetentionRule {
            keep_last: 1,
            keep_days: 0,
            keep_hourly: 0,
            keep_daily: 1,
            keep_weekly: 0,
            keep_monthly: 0,
        };
        let now = t(2024, 1, 4, 0, 0);
        let expired = select_expired(&snaps, &rule, &now);
        // 保留：d3（keep_last）+ d3（daily 桶，去重）→ 仅 d3 保留；d1/d2 过期
        assert_eq!(expired, vec!["d1".to_string(), "d2".to_string()]);
    }

    #[test]
    fn monthly_bucket_keeps_one_per_month() {
        let snaps = vec![
            sn("m1a", t(2024, 1, 10, 0, 0)),
            sn("m1b", t(2024, 1, 20, 0, 0)),
            sn("m2a", t(2024, 2, 5, 0, 0)),
            sn("m2b", t(2024, 2, 15, 0, 0)),
            sn("m3a", t(2024, 3, 1, 0, 0)),
        ];
        let rule = RetentionRule {
            keep_last: 0,
            keep_days: 0,
            keep_hourly: 0,
            keep_daily: 0,
            keep_weekly: 0,
            keep_monthly: 2,
        };
        let now = t(2024, 4, 1, 0, 0);
        let expired = select_expired(&snaps, &rule, &now);
        // keep_monthly=2：最近 2 月（3 月、2 月）各取最新 → m3a、m2b 保留；其余过期
        assert_eq!(expired.len(), 3);
        assert!(expired.contains(&"m1a".to_string()));
        assert!(expired.contains(&"m1b".to_string()));
        assert!(expired.contains(&"m2a".to_string()));
    }

    #[test]
    fn expired_output_ascending_by_time() {
        let snaps = vec![
            sn("newest", t(2024, 1, 5, 0, 0)),
            sn("old1", t(2024, 1, 1, 0, 0)),
            sn("old2", t(2024, 1, 2, 0, 0)),
        ];
        let rule = RetentionRule {
            keep_last: 1,
            keep_days: 0,
            keep_hourly: 0,
            keep_daily: 0,
            keep_weekly: 0,
            keep_monthly: 0,
        };
        let now = t(2024, 1, 6, 0, 0);
        let expired = select_expired(&snaps, &rule, &now);
        // 升序：old1 在 old2 前
        assert_eq!(expired, vec!["old1".to_string(), "old2".to_string()]);
    }

    #[test]
    fn default_321_rule_keeps_recent() {
        // 30 天每天一份；3-2-1 默认（keep_daily=7、keep_weekly=4、keep_monthly=12）
        let snaps: Vec<TimedSnapshot<String>> = (1..=30)
            .map(|i| sn(&format!("d{i}"), t(2024, 1, i, 0, 0)))
            .collect();
        let rule = RetentionRule::default_321();
        let now = t(2024, 1, 31, 0, 0);
        let expired = select_expired(&snaps, &rule, &now);
        // keep_last=7 + keep_days=7：最近 7 天（24-30）保留；keep_daily=7 同样；
        // keep_weekly=4：本月快照都在同一 ISO 周或相邻；keep_monthly=12：本月 1 份
        // 关键断言：最近 7 天都保留，最早的若干天过期
        for i in 24..=30 {
            assert!(
                !expired.contains(&format!("d{i}")),
                "最近7天的 d{i} 不应过期"
            );
        }
        // 1 月 1 日（远早于 7 天窗）应过期
        assert!(expired.contains(&"d1".to_string()));
    }

    #[test]
    fn to_simple_drops_buckets() {
        let rule = RetentionRule {
            keep_last: 5,
            keep_days: 30,
            keep_hourly: 24,
            keep_daily: 7,
            keep_weekly: 4,
            keep_monthly: 12,
        };
        let simple = rule.to_simple();
        assert_eq!(simple.keep_last, 5);
        assert_eq!(simple.keep_days, 30);
    }
}
