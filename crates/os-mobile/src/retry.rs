//! 重试 / 超时策略——纯决策算法，无副作用。
//!
//! 设计：reqwest 未引入（红线），真实 HTTP 重试循环由
//! [`crate::HttpOsClient`] 编排；本模块抽出「给定错误 + 当前已重试次数
//! → 是否重试 + 延迟多久」的**纯决策**，使其可确定性单测。
//!
//! 策略（与常见 HTTP 客户端一致）：
//! - 指数退避：第 N 次重试延迟 = `base_delay * multiplier^N`，封顶 `max_delay`。
//! - 最多重试 `max_attempts` 次（含首次请求：`max_attempts=3` = 1 次首试 + 2 次重试）。
//! - 仅对**可重试错误**重试：连接超时 / DNS 失败 / 5xx 服务端错误 / 429 限流；
//!   4xx（非 429）客户端错误（如 401/403/404）**不重试**，立即返回。
//!
//! 抖动（jitter）：决策算法本身**不**注入随机抖动（保证单测确定性）；真实重试循环
//! 在拿到 `delay` 后可加 ±10% 抖动，避免惊群。本模块只产出「建议延迟」。

use std::time::Duration;

// ----------------------------------------------------------------------------
// 重试错误分类
// ----------------------------------------------------------------------------

/// 触发重试决策的错误分类——抽象自 reqwest::Error / HTTP 状态码。
///
/// 把具体的 HTTP 客户端错误归一成这几类，使决策算法与具体客户端实现解耦：
/// 真实 reqwest 实现把 `reqwest::Error` 映射进来；mock 直接构造即可。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryableError {
    /// 连接错误（端点不可达 / 连接被拒 / TLS 失败）
    Connect,
    /// 请求/响应超时
    Timeout,
    /// DNS 解析失败
    Dns,
    /// HTTP 5xx 服务端错误（携带具体状态码）
    ServerStatus(u16),
    /// HTTP 429 限流
    RateLimited,
    /// HTTP 4xx 客户端错误（非 429；携带具体状态码，如 401/403/404）
    ClientStatus(u16),
}

impl RetryableError {
    /// 是否为「可重试」错误。
    ///
    /// 可重试：`Connect` / `Timeout` / `Dns` / `ServerStatus(5xx)` / `RateLimited`。
    /// 不可重试：`ClientStatus(4xx 非 429)`（客户端错误，重试无意义）。
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            RetryableError::Connect
            | RetryableError::Timeout
            | RetryableError::Dns
            | RetryableError::RateLimited => true,
            RetryableError::ServerStatus(code) => (500..600).contains(code),
            RetryableError::ClientStatus(_) => false,
        }
    }

    /// 由 HTTP 状态码构造 [`RetryableError`]。
    ///
    /// - 429 → `RateLimited`
    /// - 5xx → `ServerStatus`
    /// - 其余 4xx → `ClientStatus`
    /// - < 400 或 >= 600 → 视作不应到达的成功/异常，归 `ClientStatus`（保守不可重试）
    #[must_use]
    pub fn from_status(code: u16) -> Self {
        match code {
            429 => RetryableError::RateLimited,
            c if (500..600).contains(&c) => RetryableError::ServerStatus(c),
            c => RetryableError::ClientStatus(c),
        }
    }
}

// ----------------------------------------------------------------------------
// RetryPolicy
// ----------------------------------------------------------------------------

/// 重试策略参数（指数退避 + 最大次数）。
///
/// 默认值（[`RetryPolicy::default`]）：最多 3 次尝试，base 200ms，倍率 2，封顶 5s。
/// 这是常见的「内网 API 网关」推荐值——快速重试瞬态故障，避免长卡顿。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// 最大尝试次数（含首次请求；>=1）。
    pub max_attempts: u32,
    /// 基础延迟（第 1 次重试的延迟；首次请求失败后）。
    pub base_delay: Duration,
    /// 退避倍率（每次重试延迟 × multiplier）。
    pub multiplier: u32,
    /// 单次延迟上限（封顶）。
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(200),
            multiplier: 2,
            max_delay: Duration::from_secs(5),
        }
    }
}

impl RetryPolicy {
    /// 构造一个「不重试」的策略（`max_attempts = 1`）。
    ///
    /// 用于已知的不可重试场景（如配对绑定——重复提交可能创建多个会话），或测试中
    /// 想禁用重试以快速失败。
    #[must_use]
    pub fn no_retry() -> Self {
        Self {
            max_attempts: 1,
            base_delay: Duration::ZERO,
            multiplier: 1,
            max_delay: Duration::ZERO,
        }
    }

    /// 第 `attempt` 次尝试（从 0 起算：0 = 首次请求）失败后，下一次重试的建议延迟。
    ///
    /// 计算式（attempt 为已完成的尝试次数，从 0 起）：
    /// - `delay = base_delay * multiplier^attempt`
    /// - 封顶 `max_delay`
    /// - 溢出（Duration 乘法溢出）→ 返回 `max_delay`
    ///
    /// 注意：本函数**不**校验「是否还有重试机会」（那是 [`decide_retry`] 的职责），
    /// 仅算「如果重试，等多久」。调用方应先 `decide_retry` 再用此值。
    #[must_use]
    pub fn delay_for(&self, attempt: u32) -> Duration {
        // multiplier^attempt，u32 溢出按 max 封顶
        let factor = self.multiplier.saturating_pow(attempt);
        // base_delay * factor（毫秒级运算，避免 Duration::* (u32) 溢出 panic）
        let base_ms = self.base_delay.as_millis();
        let ms = base_ms.saturating_mul(u128::from(factor));
        let capped = ms.min(self.max_delay.as_millis());
        Duration::from_millis(u64::try_from(capped).unwrap_or(u64::MAX))
    }
}

// ----------------------------------------------------------------------------
// RetryDecision
// ----------------------------------------------------------------------------

/// 重试决策结果——「是否重试 + 若重试则延迟多久」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    /// 重试，等待 `Duration` 后再次发起。
    Retry(Duration),
    /// 不再重试（已达 max_attempts，或错误不可重试），向上抛错。
    GiveUp,
}

/// 重试决策算法：给定「错误 + 当前已完成尝试次数（从 0 起）+ 策略」→ 决策。
///
/// 决策规则：
/// 1. 错误不可重试（`!err.is_retryable()`）→ `GiveUp`（4xx 客户端错误立即返回）。
/// 2. 已达最大尝试次数（`attempt + 1 >= max_attempts`）→ `GiveUp`（次数耗尽）。
/// 3. 否则 → `Retry(delay_for(attempt))`。
///
/// `attempt` 语义：**已完成的尝试次数**，从 0 起算（0 = 首次请求刚失败，正在决定要不要第 2 次）。
///
/// # 示例
/// ```
/// use std::time::Duration;
/// use os_mobile::retry::{decide_retry, RetryPolicy, RetryableError, RetryDecision};
/// let p = RetryPolicy { max_attempts: 3, base_delay: Duration::from_millis(100),
///     multiplier: 2, max_delay: Duration::from_secs(1) };
/// // 首次失败（attempt=0），服务端 503 → 重试，延迟 100ms
/// assert_eq!(decide_retry(&RetryableError::ServerStatus(503), 0, &p),
///     RetryDecision::Retry(Duration::from_millis(100)));
/// // 第 3 次失败（attempt=2），次数耗尽 → GiveUp
/// assert_eq!(decide_retry(&RetryableError::ServerStatus(503), 2, &p),
///     RetryDecision::GiveUp);
/// // 4xx 不可重试
/// assert_eq!(decide_retry(&RetryableError::ClientStatus(404), 0, &p),
///     RetryDecision::GiveUp);
/// ```
#[must_use]
pub fn decide_retry(err: &RetryableError, attempt: u32, policy: &RetryPolicy) -> RetryDecision {
    // 1) 不可重试错误 → 立即放弃
    if !err.is_retryable() {
        return RetryDecision::GiveUp;
    }
    // 2) 次数耗尽（attempt 从 0 起：attempt+1 == 已发起的总次数；>= max_attempts 则放弃）
    if attempt + 1 >= policy.max_attempts {
        return RetryDecision::GiveUp;
    }
    // 3) 重试
    RetryDecision::Retry(policy.delay_for(attempt))
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(100),
            multiplier: 2,
            max_delay: Duration::from_secs(1),
        }
    }

    // —— RetryableError ——

    #[test]
    fn retryable_classification() {
        assert!(RetryableError::Connect.is_retryable());
        assert!(RetryableError::Timeout.is_retryable());
        assert!(RetryableError::Dns.is_retryable());
        assert!(RetryableError::RateLimited.is_retryable());
        assert!(RetryableError::ServerStatus(500).is_retryable());
        assert!(RetryableError::ServerStatus(503).is_retryable());
        assert!(RetryableError::ServerStatus(599).is_retryable());
        // 4xx 不可重试
        assert!(!RetryableError::ClientStatus(400).is_retryable());
        assert!(!RetryableError::ClientStatus(401).is_retryable());
        assert!(!RetryableError::ClientStatus(404).is_retryable());
    }

    #[test]
    fn from_status_mapping() {
        assert_eq!(
            RetryableError::from_status(429),
            RetryableError::RateLimited
        );
        assert_eq!(
            RetryableError::from_status(500),
            RetryableError::ServerStatus(500)
        );
        assert_eq!(
            RetryableError::from_status(503),
            RetryableError::ServerStatus(503)
        );
        assert_eq!(
            RetryableError::from_status(401),
            RetryableError::ClientStatus(401)
        );
        assert_eq!(
            RetryableError::from_status(404),
            RetryableError::ClientStatus(404)
        );
        // 边界：< 400 / >= 600 归 ClientStatus（保守不可重试）
        assert_eq!(
            RetryableError::from_status(200),
            RetryableError::ClientStatus(200)
        );
        assert_eq!(
            RetryableError::from_status(600),
            RetryableError::ClientStatus(600)
        );
    }

    // —— delay_for（指数退避）——

    #[test]
    fn delay_for_exponential_backoff() {
        let p = policy();
        // attempt 0: 100 * 2^0 = 100ms
        assert_eq!(p.delay_for(0), Duration::from_millis(100));
        // attempt 1: 100 * 2^1 = 200ms
        assert_eq!(p.delay_for(1), Duration::from_millis(200));
        // attempt 2: 100 * 2^2 = 400ms
        assert_eq!(p.delay_for(2), Duration::from_millis(400));
    }

    #[test]
    fn delay_for_capped_at_max() {
        let p = policy();
        // attempt 10: 100 * 2^10 = 102400ms > 1000ms 封顶
        assert_eq!(p.delay_for(10), Duration::from_secs(1));
        assert_eq!(p.delay_for(100), Duration::from_secs(1));
    }

    #[test]
    fn delay_for_no_retry_is_zero() {
        let p = RetryPolicy::no_retry();
        assert_eq!(p.delay_for(0), Duration::ZERO);
        assert_eq!(p.max_attempts, 1);
    }

    #[test]
    fn delay_for_saturates_on_overflow() {
        // multiplier 极大 / attempt 极大 → saturating，不 panic
        let p = RetryPolicy {
            max_attempts: 100,
            base_delay: Duration::from_secs(1),
            multiplier: 10,
            max_delay: Duration::from_secs(2),
        };
        assert_eq!(p.delay_for(u32::MAX), Duration::from_secs(2));
    }

    // —— decide_retry ——

    #[test]
    fn decide_retry_on_server_error_then_retry() {
        let p = policy();
        assert_eq!(
            decide_retry(&RetryableError::ServerStatus(503), 0, &p),
            RetryDecision::Retry(Duration::from_millis(100))
        );
        assert_eq!(
            decide_retry(&RetryableError::ServerStatus(503), 1, &p),
            RetryDecision::Retry(Duration::from_millis(200))
        );
    }

    #[test]
    fn decide_retry_give_up_when_attempts_exhausted() {
        let p = policy(); // max_attempts=3 → attempt 0,1,2 三次；attempt=2 后放弃
        assert_eq!(
            decide_retry(&RetryableError::ServerStatus(503), 2, &p),
            RetryDecision::GiveUp
        );
        assert_eq!(
            decide_retry(&RetryableError::Timeout, 5, &p),
            RetryDecision::GiveUp
        );
    }

    #[test]
    fn decide_retry_client_error_immediate_give_up() {
        let p = policy();
        // 4xx 立即放弃，即使还有重试次数
        assert_eq!(
            decide_retry(&RetryableError::ClientStatus(401), 0, &p),
            RetryDecision::GiveUp
        );
        assert_eq!(
            decide_retry(&RetryableError::ClientStatus(404), 0, &p),
            RetryDecision::GiveUp
        );
    }

    #[test]
    fn decide_retry_rate_limited_is_retryable() {
        let p = policy();
        assert_eq!(
            decide_retry(&RetryableError::RateLimited, 0, &p),
            RetryDecision::Retry(Duration::from_millis(100))
        );
    }

    #[test]
    fn decide_retry_no_retry_policy_never_retries() {
        let p = RetryPolicy::no_retry();
        // 即使是可重试错误，max_attempts=1 → attempt=0 时 attempt+1>=1 → GiveUp
        assert_eq!(
            decide_retry(&RetryableError::Timeout, 0, &p),
            RetryDecision::GiveUp
        );
    }

    #[test]
    fn decide_retry_max_attempts_one() {
        // max_attempts=1：首次失败（attempt=0）即放弃
        let p = RetryPolicy {
            max_attempts: 1,
            base_delay: Duration::from_millis(100),
            multiplier: 2,
            max_delay: Duration::from_secs(1),
        };
        assert_eq!(
            decide_retry(&RetryableError::Connect, 0, &p),
            RetryDecision::GiveUp
        );
    }

    #[test]
    fn retry_decision_eq() {
        assert_eq!(
            RetryDecision::Retry(Duration::from_millis(100)),
            RetryDecision::Retry(Duration::from_millis(100))
        );
        assert_ne!(
            RetryDecision::Retry(Duration::from_millis(100)),
            RetryDecision::GiveUp
        );
        assert_eq!(RetryDecision::GiveUp, RetryDecision::GiveUp);
    }

    // —— 扩展边界（覆盖率补测）——

    #[test]
    fn retryable_error_debug_clone_eq() {
        // Debug + Clone + PartialEq + Eq 派生间接覆盖
        let e1 = RetryableError::ServerStatus(503);
        let e2 = e1.clone();
        assert_eq!(e1, e2);
        let _dbg = format!("{:?}", e1);
        assert_ne!(
            RetryableError::ServerStatus(500),
            RetryableError::ServerStatus(503)
        );
        assert_ne!(RetryableError::Connect, RetryableError::Timeout);
    }

    #[test]
    fn from_status_boundaries() {
        // 499 → ClientStatus（< 500）
        assert_eq!(
            RetryableError::from_status(499),
            RetryableError::ClientStatus(499)
        );
        // 599 → ServerStatus（< 600）
        assert!(matches!(
            RetryableError::from_status(599),
            RetryableError::ServerStatus(599)
        ));
        // 0 → ClientStatus
        assert_eq!(
            RetryableError::from_status(0),
            RetryableError::ClientStatus(0)
        );
        // 600 → ClientStatus（>= 600 边界，保守不可重试）
        assert_eq!(
            RetryableError::from_status(600),
            RetryableError::ClientStatus(600)
        );
    }

    #[test]
    fn from_status_400_not_retryable() {
        // 400 ~ 428 都是 ClientStatus，不可重试
        for code in [400, 401, 403, 404, 410, 422, 428] {
            let e = RetryableError::from_status(code);
            assert!(!e.is_retryable(), "{code} 应不可重试");
        }
        // 430 ~ 499 同理（除非 429）
        for code in [430, 450, 499] {
            let e = RetryableError::from_status(code);
            assert!(!e.is_retryable(), "{code} 应不可重试");
        }
    }

    #[test]
    fn from_status_5xx_all_retryable() {
        // 500 ~ 599 全部可重试
        for code in [500, 501, 502, 503, 504, 599] {
            let e = RetryableError::from_status(code);
            assert!(e.is_retryable(), "{code} 应可重试");
        }
    }

    #[test]
    fn server_status_below_500_not_retryable() {
        // ServerStatus(499) 走枚举的 is_retryable：500..600 之外 → false
        assert!(!RetryableError::ServerStatus(499).is_retryable());
        assert!(!RetryableError::ServerStatus(600).is_retryable());
    }

    #[test]
    fn retry_policy_default_values() {
        let p = RetryPolicy::default();
        assert_eq!(p.max_attempts, 3);
        assert_eq!(p.base_delay, Duration::from_millis(200));
        assert_eq!(p.multiplier, 2);
        assert_eq!(p.max_delay, Duration::from_secs(5));
    }

    #[test]
    fn retry_policy_no_retry_zero_delays() {
        let p = RetryPolicy::no_retry();
        assert_eq!(p.max_attempts, 1);
        assert_eq!(p.base_delay, Duration::ZERO);
        assert_eq!(p.max_delay, Duration::ZERO);
        assert_eq!(p.multiplier, 1);
        // delay_for 永远为 0
        assert_eq!(p.delay_for(0), Duration::ZERO);
        assert_eq!(p.delay_for(10), Duration::ZERO);
    }

    #[test]
    fn retry_policy_eq_and_clone() {
        // PartialEq/Eq/Clone/Copy 派生间接覆盖
        let p1 = RetryPolicy::default();
        let p2 = p1;
        assert_eq!(p1, p2);
        let _dbg = format!("{:?}", p1);
    }

    #[test]
    fn delay_for_zero_multiplier() {
        // multiplier=0：每次重试延迟 = base * 0^attempt
        // 0^0 = 1（saturating_pow 行为）→ attempt 0 时仍为 base_delay
        let p = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(100),
            multiplier: 0,
            max_delay: Duration::from_secs(10),
        };
        // 0u32.saturating_pow(0) = 1
        assert_eq!(p.delay_for(0), Duration::from_millis(100));
        // 0u32.saturating_pow(1) = 0 → 延迟 0
        assert_eq!(p.delay_for(1), Duration::from_millis(0));
        assert_eq!(p.delay_for(2), Duration::from_millis(0));
    }

    #[test]
    fn delay_for_multiplier_one_constant() {
        // multiplier=1：延迟恒为 base_delay
        let p = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(50),
            multiplier: 1,
            max_delay: Duration::from_secs(10),
        };
        for attempt in 0..10 {
            assert_eq!(p.delay_for(attempt), Duration::from_millis(50));
        }
    }

    #[test]
    fn delay_for_zero_base_delay() {
        let p = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::ZERO,
            multiplier: 2,
            max_delay: Duration::from_secs(5),
        };
        // base=0 → 任何 attempt 都是 0
        assert_eq!(p.delay_for(0), Duration::ZERO);
        assert_eq!(p.delay_for(100), Duration::ZERO);
    }

    #[test]
    fn decide_retry_connect_dns_timeout_retryable() {
        let p = policy();
        // 连接/DNS/超时类错误首次重试
        for err in [
            RetryableError::Connect,
            RetryableError::Dns,
            RetryableError::Timeout,
        ] {
            assert_eq!(
                decide_retry(&err, 0, &p),
                RetryDecision::Retry(Duration::from_millis(100)),
                "{err:?} 应在 attempt 0 重试"
            );
        }
    }

    #[test]
    fn decide_retry_server_status_5xx_all_retry() {
        let p = policy();
        for code in [500, 501, 502, 503, 504, 599] {
            let err = RetryableError::ServerStatus(code);
            assert_eq!(
                decide_retry(&err, 0, &p),
                RetryDecision::Retry(Duration::from_millis(100)),
                "5xx {code} 应重试"
            );
        }
    }

    #[test]
    fn decide_retry_4xx_variants_never_retry() {
        let p = policy();
        for code in [400, 401, 403, 404, 410, 422, 499] {
            let err = RetryableError::ClientStatus(code);
            assert_eq!(
                decide_retry(&err, 0, &p),
                RetryDecision::GiveUp,
                "4xx {code} 应立即放弃"
            );
        }
    }

    #[test]
    fn decide_retry_exhausts_at_exact_boundary() {
        // max_attempts=3：attempt 0,1 可重试；attempt 2 (attempt+1=3) 放弃
        let p = policy(); // max_attempts=3
        assert_eq!(
            decide_retry(&RetryableError::Connect, 0, &p),
            RetryDecision::Retry(Duration::from_millis(100))
        );
        assert_eq!(
            decide_retry(&RetryableError::Connect, 1, &p),
            RetryDecision::Retry(Duration::from_millis(200))
        );
        assert_eq!(
            decide_retry(&RetryableError::Connect, 2, &p),
            RetryDecision::GiveUp
        );
    }

    #[test]
    fn decide_retry_with_no_retry_policy_always_giveup() {
        let p = RetryPolicy::no_retry();
        // attempt 0 + max_attempts 1 → attempt+1 >= 1 → GiveUp
        for err in [
            RetryableError::Connect,
            RetryableError::Timeout,
            RetryableError::Dns,
            RetryableError::RateLimited,
            RetryableError::ServerStatus(503),
        ] {
            assert_eq!(
                decide_retry(&err, 0, &p),
                RetryDecision::GiveUp,
                "{err:?} 在 no_retry 下应放弃"
            );
        }
    }

    #[test]
    fn decide_retry_retry_delay_progression() {
        // 验证多次重试的延迟递增（指数退避）
        let p = policy(); // base 100, mult 2, max 1s
        let d0 = match decide_retry(&RetryableError::Connect, 0, &p) {
            RetryDecision::Retry(d) => d,
            _ => panic!(),
        };
        let d1 = match decide_retry(&RetryableError::Connect, 1, &p) {
            RetryDecision::Retry(d) => d,
            _ => panic!(),
        };
        assert!(d1 > d0);
        assert_eq!(d0, Duration::from_millis(100));
        assert_eq!(d1, Duration::from_millis(200));
    }

    #[test]
    fn retry_decision_debug_and_clone() {
        let d = RetryDecision::Retry(Duration::from_millis(50));
        let _dbg = format!("{:?}", d);
        let d2 = d;
        assert_eq!(d, d2); // Copy
        let _dbg2 = format!("{:?}", RetryDecision::GiveUp);
    }
}
