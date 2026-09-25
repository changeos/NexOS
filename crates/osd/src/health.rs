//! HealthProbe trait —— 组件健康自检契约
//!
//! 各业务组件实现此 trait，osd 周期性调用它判断组件是否健康。
//! 返回 os-core 的 `HealthReport`（统一健康模型，跨组件复用）。

use os_core::HealthReport;

/// 健康探针 trait（异步）
///
/// 实现者：各业务组件（如 os-storage、os-meta）。osd 编排器持有探针引用，
/// 按 `HealthProbeConfig` 间隔调用 `probe()`；连续失败超阈值则标记组件 `Failed`。
pub trait HealthProbe: Send + Sync {
    /// 执行健康探测，返回 `HealthReport`
    ///
    /// 实现应快速返回（亚秒级）；超时由 osd 调用方控制（用 `tokio::time::timeout` 包裹）。
    async fn probe(&self) -> HealthReport;
}

// ----------------------------------------------------------------------------
// 单元测试：HealthProbe 契约（trait + 内嵌实现覆盖 trait 派发）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use os_core::{Health, Utc};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    /// 固定返回某 HealthReport 的探针（覆盖 trait 最小实现 + 派发）。
    struct StaticProbe {
        report: HealthReport,
    }

    impl HealthProbe for StaticProbe {
        async fn probe(&self) -> HealthReport {
            self.report.clone()
        }
    }

    fn report(h: Health, msg: Option<&str>) -> HealthReport {
        HealthReport {
            health: h,
            message: msg.map(|s| s.to_string()),
            timestamp: Utc::now(),
        }
    }

    #[tokio::test]
    async fn static_probe_returns_configured_report() {
        let p = StaticProbe {
            report: report(Health::Healthy, Some("ok")),
        };
        let r = p.probe().await;
        assert_eq!(r.health, Health::Healthy);
        assert_eq!(r.message.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn static_probe_unhealthy_propagates() {
        let p = StaticProbe {
            report: report(Health::Unhealthy, Some("down")),
        };
        let r = p.probe().await;
        assert_eq!(r.health, Health::Unhealthy);
    }

    #[tokio::test]
    async fn static_probe_degraded_propagates() {
        let p = StaticProbe {
            report: report(Health::Degraded, None),
        };
        let r = p.probe().await;
        assert_eq!(r.health, Health::Degraded);
        assert!(r.message.is_none());
    }

    #[tokio::test]
    async fn static_probe_via_boxed_dispatch() {
        // Box<impl HealthProbe> 仍可派发（编译期单态化；trait 非_dyn 兼容故用 Box<T>）
        let p: Box<StaticProbe> = Box::new(StaticProbe {
            report: report(Health::Healthy, None),
        });
        let r = p.probe().await;
        assert_eq!(r.health, Health::Healthy);
    }

    /// 每次调用递增计数器的探针（覆盖"被多次调用"场景）。
    struct CountingProbe {
        count: Arc<AtomicU32>,
    }

    impl HealthProbe for CountingProbe {
        async fn probe(&self) -> HealthReport {
            let n = self.count.fetch_add(1, Ordering::SeqCst);
            HealthReport {
                health: if n == 0 {
                    Health::Healthy
                } else {
                    Health::Degraded
                },
                message: Some(format!("call #{n}")),
                timestamp: Utc::now(),
            }
        }
    }

    #[tokio::test]
    async fn counting_probe_tracks_calls() {
        let count = Arc::new(AtomicU32::new(0));
        let p = CountingProbe {
            count: count.clone(),
        };
        // 第 1 次：n=0 → Healthy
        assert_eq!(p.probe().await.health, Health::Healthy);
        // 第 2 次：n=1 → Degraded
        assert_eq!(p.probe().await.health, Health::Degraded);
        // 第 3 次：n=2 → Degraded
        assert_eq!(p.probe().await.health, Health::Degraded);
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn health_probe_can_be_shared_across_tasks() {
        // Send + Sync：可跨 tokio task 共享（Arc<impl HealthProbe>）
        let probe: Arc<StaticProbe> = Arc::new(StaticProbe {
            report: report(Health::Healthy, None),
        });
        let mut handles = vec![];
        for _ in 0..5 {
            let p = probe.clone();
            handles.push(tokio::spawn(async move { p.probe().await.health }));
        }
        for h in handles {
            assert_eq!(h.await.unwrap(), Health::Healthy);
        }
    }

    #[tokio::test]
    async fn unknown_health_report_propagates() {
        let p = StaticProbe {
            report: report(Health::Unknown, Some("no data")),
        };
        assert_eq!(p.probe().await.health, Health::Unknown);
    }
}
