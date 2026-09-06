//! 限流算法（纯逻辑，无网络/时钟外部依赖）。
//!
//! 提供两种实现：
//! - [`TokenBucket`]：令牌桶（容量 + 每秒补充速率），允许短时突发。
//! - [`SlidingWindow`]：滑动窗口（按时间切片计数），严格上限。
//!
//! 时间由调用方传入（`now` 为 unix 秒，f64 便于亚秒），便于确定性单测。
//! 生产环境由 `RateLimitMiddleware` 注入 `chrono::Utc::now().timestamp() as f64`。

use std::collections::VecDeque;

// ----------------------------------------------------------------------------
// 令牌桶
// ----------------------------------------------------------------------------

/// 令牌桶限流器。
///
/// 容量 `capacity`，每秒补充 `rate` 个令牌；每次 `try_consume` 消耗 1 个。
#[derive(Debug, Clone)]
pub struct TokenBucket {
    capacity: f64,
    rate: f64,
    tokens: f64,
    last_refill: Option<f64>,
}

impl TokenBucket {
    /// 构造：初始即满。`last_refill` 设为 `None`，首次 `try_consume` 时定锚到 now。
    pub fn new(capacity: f64, rate: f64) -> Self {
        Self {
            capacity,
            rate,
            tokens: capacity,
            last_refill: None,
        }
    }

    /// 按 now 补充令牌（首次调用仅定锚时间，不补令牌，避免初始双计）。
    fn refill(&mut self, now: f64) {
        match self.last_refill {
            None => {
                // 首次：定锚时间，不补充（初始已满）
                self.last_refill = Some(now);
            }
            Some(prev) => {
                if now > prev {
                    let elapsed = now - prev;
                    self.tokens = (self.tokens + elapsed * self.rate).min(self.capacity);
                    self.last_refill = Some(now);
                }
            }
        }
    }

    /// 尝试消耗 1 个令牌；成功返回 true。
    pub fn try_consume(&mut self, now: f64) -> bool {
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// 当前可用令牌数（仅供观测）。
    pub fn available(&self) -> f64 {
        self.tokens
    }
}

// ----------------------------------------------------------------------------
// 滑动窗口
// ----------------------------------------------------------------------------

/// 滑动窗口限流器。
///
/// 在最近 `window_secs` 秒内最多 `max_requests` 次；超出即拒绝。
#[derive(Debug, Clone)]
pub struct SlidingWindow {
    max_requests: usize,
    window_secs: f64,
    timestamps: VecDeque<f64>,
}

impl SlidingWindow {
    /// 构造。
    pub fn new(max_requests: usize, window_secs: f64) -> Self {
        Self {
            max_requests,
            window_secs,
            timestamps: VecDeque::new(),
        }
    }

    /// 尝试记录一次请求；成功返回 true。
    pub fn try_record(&mut self, now: f64) -> bool {
        let cutoff = now - self.window_secs;
        while let Some(&t) = self.timestamps.front() {
            if t <= cutoff {
                self.timestamps.pop_front();
            } else {
                break;
            }
        }
        if self.timestamps.len() < self.max_requests {
            self.timestamps.push_back(now);
            true
        } else {
            false
        }
    }

    /// 当前窗口内已记录次数。
    pub fn current(&self) -> usize {
        self.timestamps.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_bucket_burst_then_refill() {
        let mut b = TokenBucket::new(3.0, 1.0);
        // 初始满
        assert!(b.try_consume(0.0));
        assert!(b.try_consume(0.0));
        assert!(b.try_consume(0.0));
        // 用尽
        assert!(!b.try_consume(0.0));
        // 2 秒后补 2 个
        assert!(b.try_consume(2.0));
        assert!(b.try_consume(2.0));
        assert!(!b.try_consume(2.0));
    }

    #[test]
    fn sliding_window_basic() {
        let mut w = SlidingWindow::new(3, 10.0);
        assert!(w.try_record(1.0));
        assert!(w.try_record(2.0));
        assert!(w.try_record(3.0));
        assert!(!w.try_record(4.0));
        // 窗口外过期后可再记录
        assert!(w.try_record(12.0));
    }

    #[test]
    fn sliding_window_evicts_old() {
        let mut w = SlidingWindow::new(2, 5.0);
        assert!(w.try_record(0.0));
        assert!(w.try_record(1.0));
        assert!(!w.try_record(1.0));
        assert!(w.try_record(10.0)); // 旧的全过期
    }

    #[test]
    fn token_bucket_available_reports_tokens() {
        let mut b = TokenBucket::new(3.0, 1.0);
        // 初始满
        assert_eq!(b.available(), 3.0);
        assert!(b.try_consume(0.0));
        assert_eq!(b.available(), 2.0);
        assert!(b.try_consume(0.0));
        assert_eq!(b.available(), 1.0);
        // 用尽后 available < 1
        assert!(b.try_consume(0.0));
        assert_eq!(b.available(), 0.0);
        // 用尽后无法再消费
        assert!(!b.try_consume(0.0));
        // 补充后回升（不超 capacity=3）
        assert!(b.try_consume(5.0));
        assert!(b.available() <= 3.0, "available 不应超过 capacity");
        assert!(b.available() > 0.0);
    }

    #[test]
    fn token_bucket_refill_noop_on_first_call() {
        // 首次 refill 仅定锚时间不补令牌（避免初始双计）
        let mut b = TokenBucket::new(2.0, 1.0);
        // 构造后立刻在 t=0 调用：定锚，available 仍为 2
        assert!(b.try_consume(0.0));
        assert_eq!(b.available(), 1.0);
    }

    #[test]
    fn token_bucket_refill_ignores_past_time() {
        // now <= prev 时不补充（防御性，不回退）
        let mut b = TokenBucket::new(2.0, 1.0);
        assert!(b.try_consume(10.0)); // 定锚 t=10，消耗 1
        assert_eq!(b.available(), 1.0);
        // 再传更早的 now（<= prev）：不应补充，也不应消耗
        // try_consume 会先 refill（无补充），再尝试消耗（有 1 个令牌，可消耗）
        assert!(b.try_consume(5.0));
        assert_eq!(b.available(), 0.0);
    }

    #[test]
    fn sliding_window_current_reports_count() {
        let mut w = SlidingWindow::new(3, 10.0);
        assert_eq!(w.current(), 0);
        assert!(w.try_record(1.0));
        assert_eq!(w.current(), 1);
        assert!(w.try_record(2.0));
        assert_eq!(w.current(), 2);
        // 第 3 次（= max）成功，第 4 次拒绝但 current 不增
        assert!(w.try_record(3.0));
        assert_eq!(w.current(), 3);
        assert!(!w.try_record(4.0));
        assert_eq!(w.current(), 3);
        // 过期后清除旧的
        assert!(w.try_record(15.0));
        // 旧的 3 条全部 >cutoff 过期，仅留新 1 条
        assert_eq!(w.current(), 1);
    }

    #[test]
    fn sliding_window_construction_default_state() {
        let w = SlidingWindow::new(5, 30.0);
        assert_eq!(w.current(), 0);
    }
}
