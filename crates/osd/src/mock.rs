//! Mock 实现（feature gate `mock`）
//!
//! 当前提供：
//! - [`MockHealthProbe`]：各业务组件 osd 侧测试用的健康探针替身（规格书 §3 关键实现 / §5.1）
//!
//! 后续可扩展：`MockOrchestrator` / `MockNtpManager`（按需）。
//!
//! ## 用法
//! ```ignore
//! use osd::{HealthProbe, mock::MockHealthProbe};
//! use os_core::{Health, HealthReport};
//!
//! let report = HealthReport {
//!     health: Health::Healthy,
//!     message: None,
//!     timestamp: chrono::Utc::now(),
//! };
//! let probe = MockHealthProbe::new().with_report(report.clone());
//! // probe.probe().await == report
//! ```

#![cfg(feature = "mock")]

use std::sync::{Arc, Mutex};

use crate::health::HealthProbe;
use os_core::{Health, HealthReport};

/// 健康探针的内存替身（仅测试用）
///
/// 行为：`probe()` 返回构造时配置的固定 `HealthReport`。
/// 不依赖外部状态，下游测试可确定性运行（规格书 §5.2 mock 行为约定）。
///
/// 默认返回 `Health::Unknown`（无 panic 的默认返回，§5.2）。
#[derive(Debug, Clone)]
pub struct MockHealthProbe {
    report: Arc<MockState>,
}

#[derive(Debug, Default)]
struct MockState {
    report: Mutex<Option<HealthReport>>,
}

impl MockHealthProbe {
    /// 构造默认 mock（probe 返回 `Health::Unknown`）
    pub fn new() -> Self {
        Self {
            report: Arc::new(MockState::default()),
        }
    }

    /// 设置 probe 返回的报告（链式）
    pub fn with_report(self, report: HealthReport) -> Self {
        *self
            .report
            .report
            .lock()
            .expect("mock report lock poisoned") = Some(report);
        self
    }

    /// 运行时替换 probe 返回的报告（便于测"先健康后故障"的状态转换）
    pub fn set_report(&self, report: HealthReport) {
        *self
            .report
            .report
            .lock()
            .expect("mock report lock poisoned") = Some(report);
    }

    /// 取当前配置的报告（测试辅助）
    pub fn current_report(&self) -> HealthReport {
        self.report
            .report
            .lock()
            .expect("mock report lock poisoned")
            .clone()
            .unwrap_or_else(default_unknown_report)
    }
}

impl Default for MockHealthProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthProbe for MockHealthProbe {
    async fn probe(&self) -> HealthReport {
        self.current_report()
    }
}

fn default_unknown_report() -> HealthReport {
    HealthReport {
        health: Health::Unknown,
        message: Some("MockHealthProbe 未配置报告，默认 Unknown".into()),
        timestamp: chrono::Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(h: Health) -> HealthReport {
        HealthReport {
            health: h,
            message: None,
            timestamp: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn default_probe_returns_unknown() {
        let probe = MockHealthProbe::new();
        let r = probe.probe().await;
        assert_eq!(r.health, Health::Unknown);
    }

    #[tokio::test]
    async fn with_report_returns_configured() {
        let probe = MockHealthProbe::new().with_report(report(Health::Healthy));
        assert_eq!(probe.probe().await.health, Health::Healthy);
    }

    #[tokio::test]
    async fn set_report_runtime_swap() {
        let probe = MockHealthProbe::new().with_report(report(Health::Healthy));
        assert_eq!(probe.probe().await.health, Health::Healthy);
        probe.set_report(report(Health::Unhealthy));
        assert_eq!(probe.probe().await.health, Health::Unhealthy);
    }

    #[tokio::test]
    async fn cloned_instances_share_state() {
        // Clone 共享内部状态（Arc），便于跨任务注入同一探针
        let probe = MockHealthProbe::new().with_report(report(Health::Healthy));
        let probe2 = probe.clone();
        probe2.set_report(report(Health::Degraded));
        assert_eq!(probe.probe().await.health, Health::Degraded);
    }
}
