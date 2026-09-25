//! 更新包版本模型与 semver 比较算法（规划文档 §3.12）
//!
//! 本模块是**纯逻辑**（无外部依赖）：
//! - [`Version`]：轻量 semver（major.minor.patch + 可选预发布标识），自带解析/比较。
//! - [`UpdatePackage`]：更新包模型（版本 + 签名 + 是否增量），围绕版本组织。
//! - [`compare_versions`]：纯函数比较两个版本字符串。
//! - [`upgrade_decision`]：给定当前版本 + 目标版本（+ 可选最小要求），
//!   判定是否可直接升级 / 需要中间步骤 / 不可升级（降级或版本非法）。
//!
//! 设计原则：
//! - 不引入第三方 semver crate（避免虚构依赖，工作区未注册）。
//!   实现一个严格子集：`MAJOR.MINOR.PATCH[-PRERELEASE]`，足够覆盖 OS 系统升级场景。
//! - 预发布比较遵循 semver 规范：有预发布 < 无预发布（同 major.minor.patch）；
//!   预发布段按字母序比较（仅支持单段 alnum，简化）。
//! - 版本非法（解析失败）一律视为不可升级，调用方应人工确认。

use serde::{Deserialize, Serialize};

// ----------------------------------------------------------------------------
// Version —— 轻量 semver
// ----------------------------------------------------------------------------

/// 轻量 semver 版本（major.minor.patch + 可选预发布标识）。
///
/// 支持格式：`1.2.3` / `1.2.3-rc1` / `1.2.3-beta`（不支持 build metadata `+xxx`，
/// 简化处理）。解析失败返回 Err。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    /// 主版本号
    pub major: u64,
    /// 次版本号
    pub minor: u64,
    /// 修订号
    pub patch: u64,
    /// 预发布标识（None = 正式版；Some("rc1") = 1.0.0-rc1）
    pub pre: Option<String>,
}

impl Version {
    /// 解析版本字符串（`"1.2.3"` / `"1.2.3-rc1"`）。
    ///
    /// 错误：格式不合法（非数字段 / 段数 ≠ 3 / 空字符串）。
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("版本号为空".to_string());
        }
        // 分离预发布
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p.to_string())),
            None => (s, None),
        };
        if let Some(p) = &pre {
            if p.is_empty() {
                return Err("预发布标识为空（'-' 后无内容）".to_string());
            }
            // 预发布允许多段（semver 规范）：点分段，每段 alnum 或连字符
            // （如 `nightly.20260826` / `alpha-1.2`）——支撑时间戳化版本号
            for seg in p.split('.') {
                if seg.is_empty() || !seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                    return Err(format!("预发布标识含非法字符：{p}"));
                }
            }
        }
        let mut parts = core.split('.');
        let major = parts.next().ok_or("缺少 major 段")?;
        let minor = parts.next().ok_or("缺少 minor 段")?;
        let patch = parts.next().ok_or("缺少 patch 段")?;
        if parts.next().is_some() {
            return Err(format!("版本段数过多（应为 major.minor.patch）：{core}"));
        }
        let major = major
            .parse::<u64>()
            .map_err(|_| format!("major 非法：{major}"))?;
        let minor = minor
            .parse::<u64>()
            .map_err(|_| format!("minor 非法：{minor}"))?;
        let patch = patch
            .parse::<u64>()
            .map_err(|_| format!("patch 非法：{patch}"))?;
        Ok(Self {
            major,
            minor,
            patch,
            pre,
        })
    }

    /// 是否为正式版（无预发布标识）。
    #[must_use]
    pub fn is_release(&self) -> bool {
        self.pre.is_none()
    }

    /// 格式化回字符串。
    #[must_use]
    pub fn as_string(&self) -> String {
        match &self.pre {
            Some(p) => format!("{}.{}.{}-{p}", self.major, self.minor, self.patch),
            None => format!("{}.{}.{}", self.major, self.minor, self.patch),
        }
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_string())
    }
}

impl PartialOrd for Version {
    /// semver 比较：
    /// - 先比 major/minor.patch（数值）；
    /// - 同 major.minor.patch 下：无 pre > 有 pre；有 pre 时按字母序比 pre。
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // 数值主次修订
        let by_num = self
            .major
            .cmp(&other.major)
            .then(self.minor.cmp(&other.minor))
            .then(self.patch.cmp(&other.patch));
        if by_num != std::cmp::Ordering::Equal {
            return by_num;
        }
        // 预发布：None > Some
        match (&self.pre, &other.pre) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
            (Some(a), Some(b)) => a.cmp(b),
        }
    }
}

// ----------------------------------------------------------------------------
// 纯函数：版本比较
// ----------------------------------------------------------------------------

/// 比较两个版本字符串。
///
/// 返回：
/// - `Ok(Ordering)`：两版本均合法；
/// - `Err(msg)`：任一版本解析失败。
pub fn compare_versions(a: &str, b: &str) -> Result<std::cmp::Ordering, String> {
    let va = Version::parse(a)?;
    let vb = Version::parse(b)?;
    Ok(va.cmp(&vb))
}

// ----------------------------------------------------------------------------
// UpdatePackage —— 更新包模型
// ----------------------------------------------------------------------------

/// 更新包模型（围绕版本 + 签名 + 是否增量组织）。
///
/// 注：`delta_from` 在增量包（`Some(from)`）时填充实际基准版本字符串；
/// 全量包为 `None`（可从任意版本升级）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdatePackage {
    /// 目标版本
    pub version: String,
    /// 是否增量包（None = 全量；Some(from) = 仅可从 from 升级）
    pub delta_from: Option<String>,
    /// ed25519 签名（Base64；具体验签在 UpdateEngine::verify）
    pub signature: String,
    /// SHA256 校验和
    pub sha256: String,
    /// 包大小（字节）
    pub size_bytes: u64,
}

impl UpdatePackage {
    /// 是否增量包。
    #[must_use]
    pub fn is_delta(&self) -> bool {
        self.delta_from.is_some()
    }

    /// 构造全量包。
    #[must_use]
    pub fn full(
        version: impl Into<String>,
        signature: impl Into<String>,
        sha256: impl Into<String>,
        size_bytes: u64,
    ) -> Self {
        Self {
            version: version.into(),
            delta_from: None,
            signature: signature.into(),
            sha256: sha256.into(),
            size_bytes,
        }
    }

    /// 构造增量包（仅可从 `from` 版本应用）。
    #[must_use]
    pub fn delta(
        version: impl Into<String>,
        from: impl Into<String>,
        signature: impl Into<String>,
        sha256: impl Into<String>,
        size_bytes: u64,
    ) -> Self {
        Self {
            version: version.into(),
            delta_from: Some(from.into()),
            signature: signature.into(),
            sha256: sha256.into(),
            size_bytes,
        }
    }
}

// ----------------------------------------------------------------------------
// 升级决策
// ----------------------------------------------------------------------------

/// 升级决策结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeDecision {
    /// 可直接升级到 `target`。
    Upgradable {
        /// 目标版本
        target: String,
    },
    /// 需先升级到中间版本 `via`，再到 `target`。
    /// 触发：增量包的基准版本高于当前（当前 < from < target），
    /// 需先用全量/其他包升到 from，再应用此增量。
    NeedsIntermediate {
        /// 中间必须先到的版本（增量包的基准）
        via: String,
        /// 最终目标
        target: String,
    },
    /// 不可升级（降级 / 版本非法 / 当前低于最小要求）。
    NotUpgradable {
        /// 原因
        reason: String,
    },
    /// 无需升级（当前已是目标版本或更高）。
    AlreadyUpToDate,
}

/// 给定当前版本、目标版本（+ 可选最小当前版本要求），判定升级路径。
///
/// 参数：
/// - `current`：当前系统版本字符串。
/// - `target`：目标版本字符串。
/// - `min_current`：目标包要求的最小当前版本（None = 不限制）。
/// - `delta_from`：若目标包是增量包，其基准版本（None = 全量包）。
///
/// 决策逻辑：
/// 1. 任一版本非法 → `NotUpgradable`。
/// 2. current ≥ target → `AlreadyUpToDate`（含相等）。
/// 3. current < min_current（若有限制）→ `NotUpgradable`（需先升到 min_current）。
/// 4. 增量包且 current ≠ delta_from → `NeedsIntermediate { via: delta_from }`
///    （需先升到基准版本才能应用此增量）。
/// 5. 否则 → `Upgradable`。
#[allow(clippy::too_many_arguments)]
pub fn upgrade_decision(
    current: &str,
    target: &str,
    min_current: Option<&str>,
    delta_from: Option<&str>,
) -> UpgradeDecision {
    // 1. 解析校验
    let cur = match Version::parse(current) {
        Ok(v) => v,
        Err(e) => {
            return UpgradeDecision::NotUpgradable {
                reason: format!("当前版本非法（{current}）：{e}"),
            };
        }
    };
    let tgt = match Version::parse(target) {
        Ok(v) => v,
        Err(e) => {
            return UpgradeDecision::NotUpgradable {
                reason: format!("目标版本非法（{target}）：{e}"),
            };
        }
    };

    // 2. current >= target → 已是最新
    if cur >= tgt {
        return UpgradeDecision::AlreadyUpToDate;
    }

    // 3. 最小当前版本要求
    if let Some(min_s) = min_current {
        match Version::parse(min_s) {
            Ok(min_v) => {
                if cur < min_v {
                    return UpgradeDecision::NotUpgradable {
                        reason: format!("当前版本 {cur} 低于最小要求 {min_v}，需先升级到 {min_v}"),
                    };
                }
            }
            Err(e) => {
                return UpgradeDecision::NotUpgradable {
                    reason: format!("最小当前版本标识非法（{min_s}）：{e}"),
                };
            }
        }
    }

    // 4. 增量包：必须从基准版本应用
    if let Some(from_s) = delta_from {
        match Version::parse(from_s) {
            Ok(from_v) => {
                if cur != from_v {
                    // 当前不是基准：若当前 < 基准 < 目标，需先到基准；否则不可用
                    if cur < from_v && from_v < tgt {
                        return UpgradeDecision::NeedsIntermediate {
                            via: from_s.to_string(),
                            target: target.to_string(),
                        };
                    }
                    return UpgradeDecision::NotUpgradable {
                        reason: format!("增量包基准为 {from_v}，当前 {cur} 不匹配且无法构成升级链"),
                    };
                }
            }
            Err(e) => {
                return UpgradeDecision::NotUpgradable {
                    reason: format!("增量基准版本非法（{from_s}）：{e}"),
                };
            }
        }
    }

    // 5. 可直接升级
    UpgradeDecision::Upgradable {
        target: target.to_string(),
    }
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    // —— Version::parse ——

    #[test]
    fn parse_simple() {
        let v = Version::parse("1.2.3").unwrap();
        assert_eq!((v.major, v.minor, v.patch), (1, 2, 3));
        assert!(v.pre.is_none());
        assert!(v.is_release());
    }

    #[test]
    fn parse_with_prerelease() {
        let v = Version::parse("1.2.3-rc1").unwrap();
        assert_eq!(v.pre.as_deref(), Some("rc1"));
        assert!(!v.is_release());
    }

    #[test]
    fn parse_roundtrip() {
        for s in ["1.0.0", "0.1.0", "2.5.7", "1.0.0-rc1", "10.20.30-beta"] {
            let v = Version::parse(s).unwrap();
            assert_eq!(v.as_string(), s);
            assert_eq!(v.to_string(), s);
        }
    }

    #[test]
    fn parse_rejects_invalid() {
        for bad in [
            "", "1", "1.2", "1.2.3.4", "a.b.c", "1.2.x", "1.2.3-", "1.2.3-.b", "1.2.3-a.",
        ] {
            assert!(
                Version::parse(bad).is_err(),
                "应拒绝 {bad}（多段预发布与点分段已按 semver 允许——见 relax 提交）"
            );
        }
    }

    #[test]
    fn parse_trims_whitespace() {
        assert!(Version::parse("  1.2.3  ").is_ok());
    }

    // —— 比较 ——

    #[test]
    fn compare_basic() {
        assert_eq!(compare_versions("1.0.0", "1.0.0").unwrap(), Ordering::Equal);
        assert_eq!(compare_versions("1.0.0", "2.0.0").unwrap(), Ordering::Less);
        assert_eq!(
            compare_versions("2.0.0", "1.0.0").unwrap(),
            Ordering::Greater
        );
        assert_eq!(compare_versions("1.2.0", "1.3.0").unwrap(), Ordering::Less);
        assert_eq!(
            compare_versions("1.0.1", "1.0.0").unwrap(),
            Ordering::Greater
        );
    }

    #[test]
    fn compare_prerelease_lower_than_release() {
        // 1.0.0-rc1 < 1.0.0
        assert_eq!(
            compare_versions("1.0.0-rc1", "1.0.0").unwrap(),
            Ordering::Less
        );
        // 1.0.0 > 1.0.0-rc1
        assert_eq!(
            compare_versions("1.0.0", "1.0.0-rc1").unwrap(),
            Ordering::Greater
        );
    }

    #[test]
    fn compare_two_prereleases_alphabetical() {
        // rc1 < rc2（字母序）
        assert_eq!(
            compare_versions("1.0.0-rc1", "1.0.0-rc2").unwrap(),
            Ordering::Less
        );
        // beta < rc1
        assert_eq!(
            compare_versions("1.0.0-beta", "1.0.0-rc1").unwrap(),
            Ordering::Less
        );
        // 同 pre 相等
        assert_eq!(
            compare_versions("1.0.0-rc1", "1.0.0-rc1").unwrap(),
            Ordering::Equal
        );
    }

    #[test]
    fn compare_error_on_invalid() {
        assert!(compare_versions("bad", "1.0.0").is_err());
        assert!(compare_versions("1.0.0", "bad").is_err());
    }

    // —— UpdatePackage ——

    #[test]
    fn package_full_vs_delta() {
        let full = UpdatePackage::full("1.1.0", "sig", "sha", 100);
        assert!(!full.is_delta());
        assert!(full.delta_from.is_none());
        let delta = UpdatePackage::delta("1.1.0", "1.0.0", "sig", "sha", 10);
        assert!(delta.is_delta());
        assert_eq!(delta.delta_from.as_deref(), Some("1.0.0"));
    }

    // —— upgrade_decision ——

    #[test]
    fn decision_upgradable_full() {
        // 全量包，1.0.0 → 1.1.0，无最小要求
        let d = upgrade_decision("1.0.0", "1.1.0", None, None);
        assert_eq!(
            d,
            UpgradeDecision::Upgradable {
                target: "1.1.0".to_string()
            }
        );
    }

    #[test]
    fn decision_already_up_to_date() {
        // current == target
        let d = upgrade_decision("1.0.0", "1.0.0", None, None);
        assert_eq!(d, UpgradeDecision::AlreadyUpToDate);
        // current > target（不降级）
        let d = upgrade_decision("1.1.0", "1.0.0", None, None);
        assert_eq!(d, UpgradeDecision::AlreadyUpToDate);
    }

    #[test]
    fn decision_min_current_not_met() {
        // 当前 1.0.0，目标要求最小 1.0.5
        let d = upgrade_decision("1.0.0", "2.0.0", Some("1.0.5"), None);
        assert!(matches!(d, UpgradeDecision::NotUpgradable { .. }));
    }

    #[test]
    fn decision_min_current_met() {
        // 当前 1.0.5，目标要求最小 1.0.5（满足）
        let d = upgrade_decision("1.0.5", "2.0.0", Some("1.0.5"), None);
        assert_eq!(
            d,
            UpgradeDecision::Upgradable {
                target: "2.0.0".to_string()
            }
        );
    }

    #[test]
    fn decision_delta_exact_match() {
        // 增量包基准 1.0.0，当前正是 1.0.0 → 可直接应用
        let d = upgrade_decision("1.0.0", "1.1.0", None, Some("1.0.0"));
        assert_eq!(
            d,
            UpgradeDecision::Upgradable {
                target: "1.1.0".to_string()
            }
        );
    }

    #[test]
    fn decision_delta_needs_intermediate() {
        // 增量包基准 1.0.5（仅可从 1.0.5 应用），当前 1.0.0
        // 1.0.0 < 1.0.5 < 1.1.0 → 需先到 1.0.5
        let d = upgrade_decision("1.0.0", "1.1.0", None, Some("1.0.5"));
        assert_eq!(
            d,
            UpgradeDecision::NeedsIntermediate {
                via: "1.0.5".to_string(),
                target: "1.1.0".to_string()
            }
        );
    }

    #[test]
    fn decision_delta_base_too_low() {
        // 增量基准 0.9.0 < 当前 1.0.0：无法构成升级链（current > base）
        let d = upgrade_decision("1.0.0", "1.1.0", None, Some("0.9.0"));
        assert!(matches!(d, UpgradeDecision::NotUpgradable { .. }));
    }

    #[test]
    fn decision_delta_base_equals_target() {
        // 异常：增量基准 == 目标（无意义包）
        let d = upgrade_decision("1.0.0", "1.1.0", None, Some("1.1.0"));
        assert!(matches!(d, UpgradeDecision::NotUpgradable { .. }));
    }

    #[test]
    fn decision_invalid_current() {
        let d = upgrade_decision("bad", "1.1.0", None, None);
        assert!(matches!(d, UpgradeDecision::NotUpgradable { .. }));
    }

    #[test]
    fn decision_invalid_target() {
        let d = upgrade_decision("1.0.0", "bad", None, None);
        assert!(matches!(d, UpgradeDecision::NotUpgradable { .. }));
    }

    #[test]
    fn decision_prerelease_target() {
        // 正式版 1.0.0 → 预发布 1.1.0-rc1：1.1.0-rc1 > 1.0.0，可升级
        let d = upgrade_decision("1.0.0", "1.1.0-rc1", None, None);
        assert_eq!(
            d,
            UpgradeDecision::Upgradable {
                target: "1.1.0-rc1".to_string()
            }
        );
    }

    #[test]
    fn decision_prerelease_current_higher_than_release_target() {
        // 1.1.0-rc1 → 1.1.0：rc1 < 正式版，可升级（升级到正式版）
        let d = upgrade_decision("1.1.0-rc1", "1.1.0", None, None);
        assert!(matches!(d, UpgradeDecision::Upgradable { .. }));
    }
}
