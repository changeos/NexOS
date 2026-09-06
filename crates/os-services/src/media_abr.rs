//! 自适应码率（ABR）档位选择算法（纯逻辑）。
//!
//! 给定客户端可用带宽（bps）与资源原始分辨率，从 [`TranscodeProfile`] 变体中
//! 选出"在带宽允许下、尽量贴近但不超原分辨率"的最优档位。
//!
//! 策略（参考 HLS ABR）：
//! 1. 过滤掉目标码率 > 带宽的档位（避免卡顿）。
//! 2. 若全部超出带宽，回退到最低档（Hls480p）。
//! 3. 在剩余档位中，选目标分辨率 ≤ 原始资源高度的最高档（不下采样放大）。
//! 4. 客户端带宽未知（None）→ 选默认档（Hls720p）。

use crate::media::TranscodeProfile;

/// ABR 选择配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbrConfig {
    /// 默认档位（带宽未知或全部超限时回退），默认 Hls720p。
    pub default_profile: TranscodeProfile,
    /// 码率安全余量（百分比 0..=100），实际可用带宽按 `(100 - margin)%` 计算。
    /// 例如 margin=15 → 仅使用 85% 带宽，留余量给开销。默认 15。
    pub safety_margin_pct: u8,
}

impl Default for AbrConfig {
    fn default() -> Self {
        Self {
            default_profile: TranscodeProfile::Hls720p,
            safety_margin_pct: 15,
        }
    }
}

/// 按客户端带宽选择档位（资源原始高度未知）。
///
/// 等价于 `select_profile(bandwidth_bps, None, &AbrConfig::default())`。
pub fn select_profile_for_bitrate(bandwidth_bps: Option<u64>) -> TranscodeProfile {
    select_profile(bandwidth_bps, None, &AbrConfig::default())
}

/// 按 ABR 算法选择档位。
///
/// - `bandwidth_bps`：客户端可用带宽（bps）；None = 未知。
/// - `source_height`：资源原始高度（像素）；None = 不约束放大。
/// - `cfg`：ABR 配置。
pub fn select_profile(
    bandwidth_bps: Option<u64>,
    source_height: Option<u32>,
    cfg: &AbrConfig,
) -> TranscodeProfile {
    // 带宽未知 → 默认档
    let bw = match bandwidth_bps {
        Some(b) => b,
        None => return cfg.default_profile,
    };

    // 安全余量折算
    let usable = (bw as f64) * (100.0 - cfg.safety_margin_pct as f64).max(0.0) / 100.0;

    // variants() 已按高度从高到低排序
    let mut candidates: Vec<&TranscodeProfile> = TranscodeProfile::variants()
        .iter()
        .filter(|p| (p.target_bitrate_bps() as f64) <= usable)
        .collect();

    // 全部超带宽 → 回退最低档
    if candidates.is_empty() {
        // 最低档为 variants 最后一个（Hls480p）
        return *TranscodeProfile::variants()
            .last()
            .unwrap_or(&cfg.default_profile);
    }

    // 约束：不超过源分辨率（不下采样放大）
    if let Some(h) = source_height {
        candidates.retain(|p| p.target_height() <= h);
        if candidates.is_empty() {
            // 源分辨率极低 → 用源能容纳的最高档（即第一个不超的——但全超说明源 < 480p，
            // 此场景仍给最低档以免放大失真）
            return *TranscodeProfile::variants().last().unwrap();
        }
    }

    // candidates 仍是从高到低（filter 保留顺序），取第一个（最高）
    *candidates.first().copied().unwrap_or(&cfg.default_profile)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::TranscodeProfile::*;

    #[test]
    fn unknown_bandwidth_returns_default() {
        assert_eq!(select_profile(None, None, &AbrConfig::default()), Hls720p);
    }

    #[test]
    fn high_bandwidth_picks_highest() {
        // 10 Mbps（安全余量后 8.5M）→ Hls1080p（5M）
        assert_eq!(
            select_profile(Some(10_000_000), None, &AbrConfig::default()),
            Hls1080p
        );
    }

    #[test]
    fn mid_bandwidth_picks_720() {
        // 4 Mbps → 余量 3.4M → Hls720p(2.8M)（Hls1080p 5M 超限）
        assert_eq!(
            select_profile(Some(4_000_000), None, &AbrConfig::default()),
            Hls720p
        );
    }

    #[test]
    fn low_bandwidth_picks_480() {
        // 1.5 Mbps → 余量 ~1.275M → 仅 Hls480p(1.4M)？ 1.275 < 1.4 → 全超 → 回退 480
        assert_eq!(
            select_profile(Some(1_500_000), None, &AbrConfig::default()),
            Hls480p
        );
    }

    #[test]
    fn very_low_bandwidth_falls_back_to_lowest() {
        // 100kbps：全部超限 → 回退最低档 480
        assert_eq!(
            select_profile(Some(100_000), None, &AbrConfig::default()),
            Hls480p
        );
    }

    #[test]
    fn source_height_caps_resolution() {
        // 源仅 480p 高，即使带宽充足也不选 720/1080
        assert_eq!(
            select_profile(Some(10_000_000), Some(480), &AbrConfig::default()),
            Hls480p
        );
        // 源 720p 高 → 选 720
        assert_eq!(
            select_profile(Some(10_000_000), Some(720), &AbrConfig::default()),
            Hls720p
        );
    }

    #[test]
    fn custom_safety_margin() {
        let cfg = AbrConfig {
            default_profile: Hls720p,
            safety_margin_pct: 0,
        };
        // 余量 0 → 全带宽；4M 仍 < 5M(1080) → 720
        assert_eq!(select_profile(Some(4_000_000), None, &cfg), Hls720p);
        // 5M 余量0 → =1080 码率 → 选 1080
        assert_eq!(select_profile(Some(5_000_000), None, &cfg), Hls1080p);
    }

    #[test]
    fn convenience_fn_works() {
        assert_eq!(select_profile_for_bitrate(None), Hls720p);
        assert_eq!(select_profile_for_bitrate(Some(10_000_000)), Hls1080p);
    }
}
