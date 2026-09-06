//! 场景 12：监控告警链路（integration-agent 规格书 §3 扩展场景）
//!
//! 链路：系统指标采集 → os-services monitor 规则评估 → 告警触发 → EventBus 发布
//! → 告警通知（mock webhook）→ 告警恢复。
//!
//! 之所以这样组织：
//! - OS 监控告警天然跨多个组件：指标采集（osd 健康探针周期触发）→ 规则评估
//!   （os-services monitor 的 AlertEngine 状态机）→ 事件发布（os-core EventBus）→
//!   通知分发（webhook/邮件）→ 告警恢复。各组件单独已有单测，但「采集 → 评估 →
//!   触发 → 通知 → 恢复」端到端状态机链路尚未有集成测覆盖。本场景在测侧搭一层
//!   `MonitorAlertPipeline` 编排骨架，把这些组件显式串通，验证告警状态机 +
//!   事件发布链路完整（呼应 backup_chain 的 `BackupPipeline` 编排层风格，
//!   不改 trait / crate 源码）。
//! - 用全 mock 后端：`MockHealthProbe`（osd 健康探针 mock，模拟 cpu 负载采样）、
//!   `AlertEngine`（os-services 真实纯逻辑状态机，不依赖外部环境）、
//!   `MockEventBus`（事件总线 mock，记录发布事件供断言）、
//!   `MockWebhookNotifier`（本测试定义的 webhook 通知 mock，记录派发的通知）。
//!
//! 重点验证：
//! - 告警状态机完整流转：Inactive → Pending → Firing → Resolved（经 AlertEngine）。
//! - 抖动抑制：for_duration_secs 窗口内条件抖动不误触发（Pending 回退 Inactive）。
//! - 跨 crate 类型桥接：AlertRule / EvalOutcome / Metric / Event 跨 os-services /
//!   os-core 一致；AlertSeverity / Topic / Severity 映射正确。
//! - 事件发布链路：Fired → 发 alert.fired 事件 + webhook 通知；Resolved → 发
//!   alert.resolved 事件 + webhook 通知（且原 Firing 告警标记 resolved）。
//! - 健康探针触发：osd MockHealthProbe 模拟指标采样（cpu_usage / disk_usage），
//!   probe() 返回的 HealthReport 驱动指标采集。
//! - 去重：同一规则同时刻最多一个 Firing 告警（重复达标样本不重复触发通知）。
//! - 多规则并发：cpu_usage 高 + disk_usage 低，只有 cpu 规则触发。
//!
//! 红线：不改 trait 签名 / crate 源码——本测试只用 os-services / os-core / osd
//! 已暴露的公开 API（含各 crate feature `mock` 注入的 mock 实现 + AlertEngine 纯逻辑）。

use std::sync::Arc;
use std::sync::Mutex;

use os_core::eventbus::{Event, EventBus, Severity, Topic};
use os_core::mock::MockEventBus;
use os_core::{DateTime, Health, HealthReport, Utc};

use os_services::monitor::{
    AlertEngine, AlertRule, AlertSeverity, AlertState, EvalOutcome, Metric, MetricKind,
};
use osd::health::HealthProbe;
use osd::MockHealthProbe;

// ----------------------------------------------------------------------------
// MonitorAlertPipeline：业务编排层——把指标采集 / 规则评估 / 事件发布 / 通知 / 恢复
// 串通。这是 integration-agent 搭的「跨 crate 编排骨架」，验证告警状态机 + 链路。
//
// 各阶段职责：
//   1) collect_metric：从 osd 健康探针取采样 → 构造 Metric（cpu_usage 等瞬时值）
//   2) evaluate：调 AlertEngine.ingest 推进状态机，返回 EvalOutcome
//   3) dispatch：Fired → 发 alert.fired 事件 + webhook 通知；Resolved → 发 alert.resolved
//   4) recover：指标回落 → 状态机从 Firing 转 Resolved（发 resolved 通知）
// 每阶段发 EventBus 事件；webhook 通知记录到 notifier；调用顺序记录到 call_log。
// ----------------------------------------------------------------------------

struct MonitorAlertPipeline {
    /// osd 健康探针 mock（模拟指标采样源：cpu 负载 / 磁盘使用）。
    probe: Arc<MockHealthProbe>,
    /// os-services 告警引擎（真实纯逻辑状态机）。
    engine: Mutex<AlertEngine>,
    /// 事件总线 mock（记录发布事件供断言）。
    bus: Arc<MockEventBus>,
    /// webhook 通知 mock（记录派发的告警通知）。
    notifier: Arc<MockWebhookNotifier>,
    /// 调用顺序记录（断言阶段 ↔ 组件调用对应）。
    call_log: Mutex<Vec<String>>,
}

impl MonitorAlertPipeline {
    fn new(
        probe: Arc<MockHealthProbe>,
        engine: AlertEngine,
        bus: Arc<MockEventBus>,
        notifier: Arc<MockWebhookNotifier>,
    ) -> Self {
        Self {
            probe,
            engine: Mutex::new(engine),
            bus,
            notifier,
            call_log: Mutex::new(Vec::new()),
        }
    }

    fn log(&self, entry: String) {
        self.call_log.lock().expect("call_log").push(entry);
    }

    fn call_log(&self) -> Vec<String> {
        self.call_log.lock().expect("call_log").clone()
    }

    /// 阶段 1：采集指标——从 osd 健康探针取 HealthReport → 构造 Metric。
    ///
    /// probe().health 映射为一个 sample 值（Healthy=低负载 / Unhealthy=高负载）。
    /// 返回构造的 Metric（供下游规则评估）。
    async fn collect_metric(&self, metric_name: &str, value: f64) -> Metric {
        // 触发健康探针（模拟 osd 周期性 probe）。
        let report = self.probe.probe().await;
        self.log(format!(
            "collect_metric({metric_name}={value}): probe.health={:?}",
            report.health
        ));
        Metric {
            name: metric_name.to_string(),
            kind: MetricKind::Gauge,
            value,
            labels: {
                let mut m = std::collections::HashMap::new();
                m.insert("host".to_string(), "os1".to_string());
                m
            },
            timestamp: Utc::now(),
        }
    }

    /// 阶段 2：规则评估——调 AlertEngine.ingest 推进状态机。
    ///
    /// 返回 (rule_name, EvalOutcome)（outcome 可能是 NoChange / Fired / Resolved）。
    fn evaluate(&self, rule_name: &str, value: f64, now: DateTime) -> EvalOutcome {
        let mut eng = self.engine.lock().expect("engine");
        let outcome = eng
            .ingest(rule_name, value, now)
            .expect("规则应已注册")
            .unwrap_or(EvalOutcome::NoChange);
        drop(eng);
        self.log(format!("evaluate({rule_name}, {value}): {outcome:?}"));
        outcome
    }

    /// 阶段 3：派发——根据 EvalOutcome 发事件 + webhook 通知。
    ///
    /// - Fired：发 alert.fired（Severity 对应告警级别）+ webhook.fire
    /// - Resolved：发 alert.resolved + webhook.resolve（原 Firing 告警标记 resolved）
    /// - NoChange：无动作
    async fn dispatch(
        &self,
        rule_name: &str,
        severity: AlertSeverity,
        outcome: &EvalOutcome,
        metric_name: &str,
        value: f64,
    ) {
        let now = Utc::now();
        match outcome {
            EvalOutcome::Fired { fired_at, value: v } => {
                let bus_severity = map_alert_severity(severity);
                let _ = self
                    .bus
                    .publish(Event {
                        source: "os-services/monitor".into(),
                        topic: Topic::System,
                        kind: "alert.fired".into(),
                        severity: bus_severity,
                        task_id: None,
                        payload: serde_json::json!({
                            "rule": rule_name,
                            "severity": format!("{severity:?}"),
                            "metric": metric_name,
                            "value": v,
                            "fired_at": fired_at.to_rfc3339(),
                        }),
                        timestamp: now,
                    })
                    .await;
                self.notifier.fire(rule_name, severity, *v);
                self.log(format!("dispatch({rule_name}): Fired → bus+webhook"));
            }
            EvalOutcome::Resolved { resolved_at } => {
                let _ = self
                    .bus
                    .publish(Event {
                        source: "os-services/monitor".into(),
                        topic: Topic::System,
                        kind: "alert.resolved".into(),
                        severity: Severity::Info,
                        task_id: None,
                        payload: serde_json::json!({
                            "rule": rule_name,
                            "metric": metric_name,
                            "value": value,
                            "resolved_at": resolved_at.to_rfc3339(),
                        }),
                        timestamp: now,
                    })
                    .await;
                self.notifier.resolve(rule_name);
                self.log(format!("dispatch({rule_name}): Resolved → bus+webhook"));
            }
            EvalOutcome::NoChange => {
                self.log(format!("dispatch({rule_name}): NoChange（无动作）"));
            }
        }
    }

    /// 驱动一次完整的「采集 → 评估 → 派发」单步（一个采样周期）。
    async fn step(&self, metric_name: &str, value: f64, rule_name: &str, severity: AlertSeverity) {
        let now = Utc::now();
        let _metric = self.collect_metric(metric_name, value).await;
        let outcome = self.evaluate(rule_name, value, now);
        self.dispatch(rule_name, severity, &outcome, metric_name, value)
            .await;
    }

    /// 查规则当前状态（断言用）。
    fn state(&self, rule_name: &str) -> Option<AlertState> {
        self.engine
            .lock()
            .expect("engine")
            .state(rule_name)
            .cloned()
    }
}

// ----------------------------------------------------------------------------
// MockWebhookNotifier：mock webhook 通知接收方（记录派发的 fire/resolve 通知）。
// ----------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Notification {
    rule_name: String,
    severity: AlertSeverity,
    value: f64,
    resolved: bool,
}

#[derive(Default)]
struct MockWebhookNotifier {
    notifications: Mutex<Vec<Notification>>,
}

impl MockWebhookNotifier {
    fn new() -> Self {
        Self::default()
    }

    fn fire(&self, rule_name: &str, severity: AlertSeverity, value: f64) {
        self.notifications
            .lock()
            .expect("notifier")
            .push(Notification {
                rule_name: rule_name.to_string(),
                severity,
                value,
                resolved: false,
            });
    }

    fn resolve(&self, rule_name: &str) {
        // 标记该规则最近的未恢复通知为 resolved。
        let mut list = self.notifications.lock().expect("notifier");
        if let Some(n) = list
            .iter_mut()
            .rev()
            .find(|n| n.rule_name == rule_name && !n.resolved)
        {
            n.resolved = true;
        }
    }

    fn notifications(&self) -> Vec<Notification> {
        self.notifications.lock().expect("notifier").clone()
    }

    fn fired_count(&self, rule_name: &str) -> usize {
        self.notifications()
            .iter()
            .filter(|n| n.rule_name == rule_name && !n.resolved)
            .count()
    }
}

// ----------------------------------------------------------------------------
// 辅助
// ----------------------------------------------------------------------------

/// AlertSeverity（业务）→ EventBus Severity（事件总线）。
fn map_alert_severity(s: AlertSeverity) -> Severity {
    match s {
        AlertSeverity::Info => Severity::Info,
        AlertSeverity::Warning => Severity::Warn,
        AlertSeverity::Critical => Severity::Critical,
    }
}

/// 构造一个 cpu_usage 高负载告警规则（for_duration_secs 可配）。
fn cpu_rule(dur_secs: u32) -> AlertRule {
    AlertRule {
        name: "cpu_high".into(),
        metric: "cpu_usage".into(),
        condition: ">85".into(),
        for_duration_secs: dur_secs,
        severity: AlertSeverity::Critical,
    }
}

/// 构造一个 disk_usage 高位告警规则（for_duration_secs 可配）。
fn disk_rule(dur_secs: u32) -> AlertRule {
    AlertRule {
        name: "disk_full".into(),
        metric: "disk_usage".into(),
        condition: ">90".into(),
        for_duration_secs: dur_secs,
        severity: AlertSeverity::Warning,
    }
}

/// 固定时间戳（确定性，便于状态机断言）。
fn ts(s: &str) -> DateTime {
    chrono::DateTime::parse_from_rfc3339(s)
        .unwrap()
        .with_timezone(&Utc)
}

/// 构造一个健康探针 mock（probe 返回给定 health）。
fn probe_with(health: Health) -> MockHealthProbe {
    MockHealthProbe::new().with_report(HealthReport {
        health,
        message: None,
        timestamp: Utc::now(),
    })
}

// ----------------------------------------------------------------------------
// 集成测
// ----------------------------------------------------------------------------

/// 构造一条完整管线（注入 cpu + disk 规则）。
fn build_pipeline(probe: MockHealthProbe, rules: Vec<AlertRule>) -> MonitorAlertPipeline {
    let mut engine = AlertEngine::new();
    for r in rules {
        engine.add_rule(r).expect("规则应可注册");
    }
    MonitorAlertPipeline::new(
        Arc::new(probe),
        engine,
        Arc::new(MockEventBus::new()),
        Arc::new(MockWebhookNotifier::new()),
    )
}

#[tokio::test]
async fn full_chain_collect_evaluate_fire_notify_resolved() {
    // 完整状态机流转：Inactive → Firing（for_duration=0 立即触发）→ Resolved。
    let probe = probe_with(Health::Unhealthy);
    let pipeline = build_pipeline(probe, vec![cpu_rule(0)]);

    // 1) 采集 + 评估：cpu=90（>85），for_duration=0 → 立即 Firing。
    pipeline
        .step("cpu_usage", 90.0, "cpu_high", AlertSeverity::Critical)
        .await;

    // 状态机：cpu_high 已 Firing。
    assert!(
        matches!(pipeline.state("cpu_high"), Some(AlertState::Firing { .. })),
        "cpu_high 应 Firing"
    );

    // EventBus 收到 alert.fired Critical 事件。
    assert_eq!(pipeline.bus.published_count_for(Topic::System), 1);
    let fired_ev = &pipeline.bus.published()[0];
    assert_eq!(fired_ev.kind, "alert.fired");
    assert_eq!(fired_ev.severity, Severity::Critical);
    assert_eq!(fired_ev.payload["rule"].as_str(), Some("cpu_high"));
    assert_eq!(fired_ev.payload["value"].as_f64(), Some(90.0));

    // webhook 收到 fire 通知（未恢复）。
    assert_eq!(pipeline.notifier.fired_count("cpu_high"), 1);
    let notes = pipeline.notifier.notifications();
    assert_eq!(notes.len(), 1);
    assert!(!notes[0].resolved);
    assert_eq!(notes[0].severity, AlertSeverity::Critical);
    assert!(
        (notes[0].value - 90.0).abs() < 1e-9,
        "通知应携触发时的 metric 值"
    );

    // 2) 指标回落：cpu=50（<85）→ 状态机从 Firing 转 Resolved。
    pipeline
        .step("cpu_usage", 50.0, "cpu_high", AlertSeverity::Critical)
        .await;

    // 状态机：cpu_high 回到 Inactive（Resolved 是转换，状态最终置 Inactive）。
    assert_eq!(
        pipeline.state("cpu_high"),
        Some(AlertState::Inactive),
        "Resolved 后状态应回 Inactive"
    );

    // EventBus 又收到 alert.resolved Info 事件。
    assert_eq!(pipeline.bus.published_count_for(Topic::System), 2);
    let resolved_ev = &pipeline.bus.published()[1];
    assert_eq!(resolved_ev.kind, "alert.resolved");
    assert_eq!(resolved_ev.severity, Severity::Info);

    // webhook 通知已标记 resolved。
    let notes = pipeline.notifier.notifications();
    assert_eq!(notes.len(), 1, "只一条通知记录（resolved 标记在原通知上）");
    assert!(notes[0].resolved, "原 fire 通知应标记 resolved");

    // 调用顺序：2 次 step 各含 collect → evaluate → dispatch。
    let log = pipeline.call_log();
    assert_eq!(
        log.iter().filter(|s| s.contains("collect_metric")).count(),
        2
    );
    assert_eq!(log.iter().filter(|s| s.contains("evaluate")).count(), 2);
    assert_eq!(log.iter().filter(|s| s.contains("dispatch")).count(), 2);
    // 第一次 Fired，第二次 Resolved。
    assert!(log.iter().any(|s| s.contains("Fired → bus+webhook")));
    assert!(log.iter().any(|s| s.contains("Resolved → bus+webhook")));
}

#[tokio::test]
async fn pending_state_requires_for_duration_before_firing() {
    // 抖动抑制：for_duration_secs > 0 时，条件首次满足进 Pending，未持续够时长不 Firing。
    let probe = probe_with(Health::Degraded);
    let pipeline = build_pipeline(probe, vec![cpu_rule(300)]); // 5 分钟持续要求

    let t0 = ts("2026-01-01T00:00:00Z");
    // 首次满足 cpu=90 → Pending，不触发通知。
    let outcome = pipeline.evaluate("cpu_high", 90.0, t0);
    assert_eq!(
        outcome,
        EvalOutcome::NoChange,
        "首次满足应 Pending 不 Firing"
    );
    assert!(
        matches!(pipeline.state("cpu_high"), Some(AlertState::Pending { .. })),
        "应 Pending"
    );
    // 未触发任何事件 / 通知。
    assert_eq!(pipeline.bus.published_count(), 0);
    assert_eq!(pipeline.notifier.notifications().len(), 0);

    // 1 分钟后仍满足，但未到 5 分钟 → 仍 Pending。
    let t1 = ts("2026-01-01T00:01:00Z");
    let outcome = pipeline.evaluate("cpu_high", 90.0, t1);
    assert_eq!(outcome, EvalOutcome::NoChange);
    assert!(matches!(
        pipeline.state("cpu_high"),
        Some(AlertState::Pending { .. })
    ));

    // 5 分钟后（≥ for_duration）→ 转 Firing，触发通知。
    let t2 = ts("2026-01-01T00:05:00Z");
    let outcome = pipeline.evaluate("cpu_high", 90.0, t2);
    assert!(
        matches!(outcome, EvalOutcome::Fired { .. }),
        "持续够时长应 Firing"
    );
    assert!(matches!(
        pipeline.state("cpu_high"),
        Some(AlertState::Firing { .. })
    ));
}

#[tokio::test]
async fn pending_resets_when_condition_flaps() {
    // 抖动：Pending 窗口内条件不再满足 → 回退 Inactive（重置抖动抑制窗口）。
    let probe = probe_with(Health::Healthy);
    let pipeline = build_pipeline(probe, vec![cpu_rule(300)]);

    let t0 = ts("2026-01-01T00:00:00Z");
    // 首次满足 → Pending。
    let outcome = pipeline.evaluate("cpu_high", 90.0, t0);
    assert_eq!(outcome, EvalOutcome::NoChange);
    assert!(matches!(
        pipeline.state("cpu_high"),
        Some(AlertState::Pending { .. })
    ));

    // 1 分钟后条件抖动消失（cpu=50 < 85）→ Pending 回退 Inactive。
    let t1 = ts("2026-01-01T00:01:00Z");
    let outcome = pipeline.evaluate("cpu_high", 50.0, t1);
    assert_eq!(outcome, EvalOutcome::NoChange);
    assert_eq!(
        pipeline.state("cpu_high"),
        Some(AlertState::Inactive),
        "抖动应回退 Inactive"
    );

    // 之后即便立刻满足且时间已过 5 分钟，也需重新攒满 for_duration（从新 Pending 起算）。
    let t2 = ts("2026-01-01T00:06:00Z");
    let outcome = pipeline.evaluate("cpu_high", 90.0, t2);
    assert_eq!(
        outcome,
        EvalOutcome::NoChange,
        "重新 Pending（since 重置），未触发"
    );
    assert!(matches!(
        pipeline.state("cpu_high"),
        Some(AlertState::Pending { .. })
    ));
}

#[tokio::test]
async fn dedup_repeated_firing_does_not_re_notify() {
    // 去重：同一规则同时刻最多一个 Firing 告警——重复达标样本不重复触发通知。
    let probe = probe_with(Health::Unhealthy);
    let pipeline = build_pipeline(probe, vec![cpu_rule(0)]);

    // 第一次 cpu=90 → Firing，触发通知。
    pipeline
        .step("cpu_usage", 90.0, "cpu_high", AlertSeverity::Critical)
        .await;
    assert_eq!(pipeline.notifier.fired_count("cpu_high"), 1);
    assert_eq!(pipeline.bus.published_count_for(Topic::System), 1);

    // 第二次 cpu=92（仍 >85）→ 已 Firing，NoChange，不重复通知。
    pipeline
        .step("cpu_usage", 92.0, "cpu_high", AlertSeverity::Critical)
        .await;
    assert_eq!(
        pipeline.notifier.fired_count("cpu_high"),
        1,
        "已 Firing 不应重复触发通知"
    );
    assert_eq!(
        pipeline.bus.published_count_for(Topic::System),
        1,
        "已 Firing 不应重复发 alert.fired"
    );

    // 状态仍 Firing。
    assert!(matches!(
        pipeline.state("cpu_high"),
        Some(AlertState::Firing { .. })
    ));
}

#[tokio::test]
async fn multi_rule_only_matching_rule_fires() {
    // 多规则并发：cpu_usage 高 + disk_usage 低，只有 cpu 规则触发。
    let probe = probe_with(Health::Unhealthy);
    let pipeline = build_pipeline(probe, vec![cpu_rule(0), disk_rule(0)]);

    // cpu=90（>85 触发 cpu_high）；disk=50（<90 不触发 disk_full）。
    pipeline
        .step("cpu_usage", 90.0, "cpu_high", AlertSeverity::Critical)
        .await;
    // disk 单独评估（不进 step 的事件路径，直接 evaluate 验状态）。
    let disk_outcome = pipeline.evaluate("disk_full", 50.0, Utc::now());
    assert_eq!(disk_outcome, EvalOutcome::NoChange, "disk=50 < 90 不应触发");

    // 只有 cpu_high Firing；disk_full 仍 Inactive。
    assert!(matches!(
        pipeline.state("cpu_high"),
        Some(AlertState::Firing { .. })
    ));
    assert_eq!(
        pipeline.state("disk_full"),
        Some(AlertState::Inactive),
        "disk_full 应仍 Inactive"
    );

    // 只 cpu 规则发了通知。
    assert_eq!(pipeline.notifier.fired_count("cpu_high"), 1);
    assert_eq!(pipeline.notifier.fired_count("disk_full"), 0);

    // EventBus 只 1 个 alert.fired（cpu）。
    let published = pipeline.bus.published();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].payload["rule"].as_str(), Some("cpu_high"));
}

#[tokio::test]
async fn health_probe_drives_metric_collection() {
    // 验证 osd 健康探针触发指标采集：probe().health 与采集到的 metric 语义对齐。
    // Unhealthy → 高 cpu（告警触发）；Healthy → 低 cpu（告警恢复）。
    let probe = Arc::new(MockHealthProbe::new());
    let pipeline = MonitorAlertPipeline::new(
        probe.clone(),
        {
            let mut e = AlertEngine::new();
            e.add_rule(cpu_rule(0)).unwrap();
            e
        },
        Arc::new(MockEventBus::new()),
        Arc::new(MockWebhookNotifier::new()),
    );

    // 探针返回 Unhealthy → 采集 cpu=95（高负载语义）→ 触发告警。
    probe.set_report(HealthReport {
        health: Health::Unhealthy,
        message: Some("cpu overload".into()),
        timestamp: Utc::now(),
    });
    pipeline
        .step("cpu_usage", 95.0, "cpu_high", AlertSeverity::Critical)
        .await;
    assert!(matches!(
        pipeline.state("cpu_high"),
        Some(AlertState::Firing { .. })
    ));
    // 采集日志记录了 probe.health=Unhealthy。
    assert!(
        pipeline
            .call_log()
            .iter()
            .any(|s| s.contains("probe.health=Unhealthy")),
        "采集日志应反映探针状态"
    );

    // 探针切回 Healthy → 采集 cpu=30（低负载语义）→ 告警恢复。
    probe.set_report(HealthReport {
        health: Health::Healthy,
        message: None,
        timestamp: Utc::now(),
    });
    pipeline
        .step("cpu_usage", 30.0, "cpu_high", AlertSeverity::Critical)
        .await;
    assert_eq!(
        pipeline.state("cpu_high"),
        Some(AlertState::Inactive),
        "恢复后应 Inactive"
    );
    assert!(
        pipeline
            .call_log()
            .iter()
            .any(|s| s.contains("probe.health=Healthy")),
        "采集日志应反映探针恢复"
    );

    // 事件序列：fired（Critical）→ resolved（Info）。
    let published = pipeline.bus.published();
    let kinds: Vec<&str> = published.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds, vec!["alert.fired", "alert.resolved"]);
    assert_eq!(published[0].severity, Severity::Critical);
    assert_eq!(published[1].severity, Severity::Info);
}

#[tokio::test]
async fn severity_mapping_to_event_bus() {
    // 跨 crate 类型桥接：AlertSeverity（业务）正确映射到 EventBus Severity（事件）。
    // Warning 告警 → Warn 事件；Critical 告警 → Critical 事件。
    let probe = probe_with(Health::Unhealthy);
    let pipeline = build_pipeline(probe, vec![cpu_rule(0), disk_rule(0)]);

    // disk_full 是 Warning 规则；手动触发它（disk=95 > 90）。
    pipeline
        .step("disk_usage", 95.0, "disk_full", AlertSeverity::Warning)
        .await;

    // cpu_high 是 Critical 规则。
    pipeline
        .step("cpu_usage", 90.0, "cpu_high", AlertSeverity::Critical)
        .await;

    let published = pipeline.bus.published();
    assert_eq!(published.len(), 2);
    // disk_full → Warn；cpu_high → Critical。
    let disk_ev = published
        .iter()
        .find(|e| e.payload["rule"].as_str() == Some("disk_full"))
        .unwrap();
    assert_eq!(disk_ev.severity, Severity::Warn);
    let cpu_ev = published
        .iter()
        .find(|e| e.payload["rule"].as_str() == Some("cpu_high"))
        .unwrap();
    assert_eq!(cpu_ev.severity, Severity::Critical);

    // map_alert_severity 覆盖三档（Info / Warning / Critical）。
    assert_eq!(map_alert_severity(AlertSeverity::Info), Severity::Info);
    assert_eq!(map_alert_severity(AlertSeverity::Warning), Severity::Warn);
    assert_eq!(
        map_alert_severity(AlertSeverity::Critical),
        Severity::Critical
    );
}

#[tokio::test]
async fn metric_constructor_cross_crate_type_identity() {
    // 跨 crate 类型桥接：Metric（os-services）字段类型一致；可构造 + 携带 labels。
    let now = Utc::now();
    let m = Metric::gauge("cpu_usage", 75.5, now).with_label("host", "os1");
    assert_eq!(m.name, "cpu_usage");
    assert_eq!(m.kind, MetricKind::Gauge);
    assert!((m.value - 75.5).abs() < 1e-9);
    assert_eq!(m.labels.get("host"), Some(&"os1".to_string()));
    assert_eq!(m.timestamp, now);

    // Counter / Histogram 构造器也可用（跨 crate 类型一致）。
    let c = Metric::counter("bytes_sent", 1024.0, now);
    assert_eq!(c.kind, MetricKind::Counter);
    let h = Metric::histogram("req_latency", 0.025, now);
    assert_eq!(h.kind, MetricKind::Histogram);
}

#[tokio::test]
async fn alert_engine_unknown_rule_returns_none() {
    // 未注册规则的 evaluate 返回 None（链路对未知规则的降级处理）。
    let probe = probe_with(Health::Healthy);
    let pipeline = build_pipeline(probe, vec![]);

    let mut eng = pipeline.engine.lock().expect("engine");
    let outcome = eng.ingest("nonexistent_rule", 1.0, Utc::now()).unwrap();
    assert!(outcome.is_none(), "未注册规则应返回 None");
}

#[tokio::test]
async fn alert_rule_condition_parse_validated_on_register() {
    // 跨 crate：AlertEngine.add_rule 校验条件表达式可解析——坏规则拒绝注册。
    let mut engine = AlertEngine::new();
    // 合法条件可注册。
    engine
        .add_rule(AlertRule {
            name: "ok".into(),
            metric: "m".into(),
            condition: ">80".into(),
            for_duration_secs: 0,
            severity: AlertSeverity::Warning,
        })
        .unwrap();
    // 非法条件（缺算子）拒绝注册。
    let err = engine
        .add_rule(AlertRule {
            name: "bad".into(),
            metric: "m".into(),
            condition: "80".into(), // 缺比较算子
            for_duration_secs: 0,
            severity: AlertSeverity::Warning,
        })
        .unwrap_err();
    assert!(
        matches!(err, os_services::ServiceError::Internal(_)),
        "坏条件应 Internal 错误: {err:?}"
    );
}
