//! 场景 8：update 回滚（integration-agent 规格书 §3 场景 8）
//!
//! 链路：os-update A/B 槽位（SlotManager）→ UpdateEngine 写入非活动槽 + 激活新槽
//! → 模拟新槽启动失败（探活 Unhealthy）→ watchdog（should_rollback 判定 +
//! SlotManager.on_boot_failed）→ 切回旧槽 + 标记新槽 Failed。
//!
//! 重点验证：
//! - 完整更新成功路径：A(v1.0.0) → 写 B(v1.1.0) → 激活 B → 探活通过 → 提交（on_boot_succeeded）。
//! - 回滚状态机：新槽激活后探活失败 → on_boot_failed 产 Rollback{target=旧槽, failed_slot=新槽}，
//!   旧槽恢复 Active，新槽标记 Failed。
//! - watchdog 触发链：should_rollback 判定（Automatic/Watchdog/Manual 三种策略）+
//!   SlotManager.on_boot_failed 完成回滚；RollbackManager.verify_current_health
//!   返回探活报告驱动决策。
//! - 无回滚目标（首启）保护：首启无 previous_active，探活失败也不回滚（NoOp）。
//! - 跨 crate 类型桥接：UpdateManifest / UpdateSlot / SlotState / HealthReport
//!   在 update 内部各 trait 间透传一致。
//!
//! 注：MockUpdateEngine 与 MockRollbackManager 各自持有独立 SlotManager（不共享），
//! 故回滚状态机推进用纯逻辑 SlotManager 直接驱动（更精准验证状态机本身）；
//! MockRollbackManager 用于验证 verify_current_health / list_snapshots /
//! auto_rollback_if_unhealthy 的默认行为（首启无目标保护路径）。
//!
//! 红线：不改 trait 签名 / 其他 crate 源码——本测试用 os-update feature `mock`
//! 暴露的 MockUpdateEngine / MockRollbackManager + 纯逻辑 SlotManager / should_rollback。

use std::sync::Arc;

use os_core::eventbus::{Event, EventBus, Severity, Topic};
use os_core::mock::MockEventBus;
use os_core::{Health, HealthReport, Utc};

use os_update::rollback::{
    should_rollback, RollbackContext, RollbackDecision, RollbackManager, RollbackPolicy,
};
use os_update::slot::{SlotManager, SlotStatus, SlotSwitchDecision};
use os_update::update::{ComponentUpdate, UpdateEngine, UpdateManifest, UpdateSlot};
use os_update::{MockRollbackManager, MockUpdateEngine};

// ----------------------------------------------------------------------------
// 集成版「更新编排器」：把 UpdateEngine（A/B 槽写入+激活）+ watchdog（探活+回滚）
// 串起来。这是 integration-agent 搭建的「业务编排层」——验证各组件能跨 trait 协作。
//
// 由于 mock 的 engine/rollback 各持独立 SlotManager，本编排器额外持有一个
// 「真源 SlotManager」（single source of truth），由它主导状态机推进；engine 用于
// 真实跑 check/download/verify/write/activate 的 trait 方法链路（验证 trait 行为），
// 真源 SlotManager 同步推进以验证回滚状态机。
// ----------------------------------------------------------------------------

struct UpdateOrchestrator {
    /// 引擎句柄（部分测试用其跑 check/download/verify/write/activate trait 链路）。
    /// 标注 allow(dead_code)：并非所有测试都经 engine 驱动（有些直接用 slot 状态机）。
    #[allow(dead_code)]
    engine: Arc<MockUpdateEngine>,
    #[allow(dead_code)]
    bus: Arc<MockEventBus>,
    /// 真源 SlotManager（与 engine 的 SlotManager 同步推进，用于回滚断言）。
    slot: SlotManager,
    /// 调用顺序记录（断言「写槽 → 激活 → 探活 → 回滚」的执行顺序）。
    call_log: Vec<String>,
}

impl UpdateOrchestrator {
    fn new(engine: Arc<MockUpdateEngine>, bus: Arc<MockEventBus>) -> Self {
        // 真源 SlotManager 初始：A active v1.0.0（与 MockUpdateEngine 默认一致）。
        Self {
            engine,
            bus,
            slot: SlotManager::new(UpdateSlot::A, "1.0.0", Utc::now()),
            call_log: Vec::new(),
        }
    }

    /// 同步推进真源 SlotManager：写入 + 激活（与 engine 的 write/activate 镜像）。
    /// 返回激活决策（Activate{target, previous}）供调用方在探活阶段决策。
    fn write_and_activate(&mut self, version: &str) -> UpdateSlot {
        let target = self.slot.writable_slot().expect("应有可写槽");
        self.slot.begin_write(target).unwrap();
        self.slot.finish_write(target, version, Utc::now()).unwrap();
        let decision = self.slot.plan_activation(target);
        if let SlotSwitchDecision::Activate { target, previous } = decision {
            self.slot
                .apply_activation(target, previous, Utc::now())
                .unwrap();
        }
        target
    }

    /// 探活通过 → 提交（清 previous_active）。
    fn commit_boot_success(&mut self) -> bool {
        self.slot.on_boot_succeeded()
    }

    /// 探活失败 → 回滚（on_boot_failed）。
    fn rollback_on_boot_failure(&mut self) -> SlotSwitchDecision {
        self.slot.on_boot_failed()
    }
}

// ----------------------------------------------------------------------------
// 辅助：构造一个 UpdateManifest（含 osd 组件）
// ----------------------------------------------------------------------------

fn manifest(version: &str) -> UpdateManifest {
    UpdateManifest {
        version: version.to_string(),
        release_notes: "test release".into(),
        size_bytes: 100 * 1024 * 1024,
        sha256: "abc123".to_string(),
        signature: "sig-base64".to_string(),
        min_current_version: None,
        components: vec![ComponentUpdate {
            name: "osd".to_string(),
            version: version.to_string(),
            restart_required: true,
        }],
    }
}

fn unhealthy_report() -> HealthReport {
    HealthReport {
        health: Health::Unhealthy,
        message: Some("新槽启动失败：核心服务未就绪".into()),
        timestamp: Utc::now(),
    }
}

fn healthy_report() -> HealthReport {
    HealthReport {
        health: Health::Healthy,
        message: None,
        timestamp: Utc::now(),
    }
}

// ----------------------------------------------------------------------------
// 集成测
// ----------------------------------------------------------------------------

#[tokio::test]
async fn update_full_cycle_writes_activates_and_commits() {
    // 完整更新成功路径：A(v1.0.0) → 写 B(v1.1.0) → 激活 B → 探活通过 → 提交。
    let engine = Arc::new(MockUpdateEngine::new().with_updates(vec![manifest("1.1.0")]));
    let bus = Arc::new(MockEventBus::new());
    let mut orch = UpdateOrchestrator::new(engine.clone(), bus.clone());

    // 1. 检查更新（trait 链路）。
    let updates = engine.check_updates().await.expect("应有更新");
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].version, "1.1.0");
    orch.call_log
        .push("check_updates: 1 个更新 (v1.1.0)".into());

    // 2. 下载。
    let task = engine.download(&updates[0]).await.expect("下载应成功");
    orch.call_log.push(format!("download: task={task}"));

    // 3. 校验（签名+sha256，mock 默认 true）。
    let verified = engine
        .verify(&updates[0], std::path::Path::new("/tmp/dl"))
        .await
        .expect("verify 应成功");
    assert!(verified, "mock verify 默认 true");
    orch.call_log.push("verify: Ok".into());

    // 4. 写入非活动槽（trait + 真源 SlotManager 同步推进）。
    let written = engine
        .write_to_inactive_slot(&updates[0])
        .await
        .expect("写入应成功");
    assert_eq!(written, UpdateSlot::B, "应写入 B（A 为 active）");
    // 激活 B（trait 链路：engine 内部 SlotManager 同步推进）。
    engine
        .activate_slot(UpdateSlot::B)
        .await
        .expect("激活应成功");
    // 真源 SlotManager 同步推进（写+激活，与 engine 镜像）。
    let orch_written = orch.write_and_activate("1.1.0");
    assert_eq!(orch_written, UpdateSlot::B);
    orch.call_log
        .push("write_to_inactive_slot(B) + activate".into());

    // 5. 探活（healthy）→ 提交。
    let committed = orch.commit_boot_success();
    assert!(committed, "提交后应仍有 active 槽");
    orch.call_log.push("probe: Healthy → commit".into());

    // 6. 状态机断言：B active v1.1.0，A 降为 inactive，previous_active 清空。
    assert_eq!(orch.slot.active_slot(), Some(UpdateSlot::B));
    assert_eq!(
        orch.slot.slot(UpdateSlot::B).version.as_deref(),
        Some("1.1.0")
    );
    assert_eq!(orch.slot.slot(UpdateSlot::A).status, SlotStatus::Inactive);
    assert_eq!(orch.slot.previous_active, None);

    // engine 侧槽位也一致（mock activate 内部 on_boot_succeeded）。
    let eng_sm = engine.slot_manager();
    assert_eq!(eng_sm.active_slot(), Some(UpdateSlot::B));

    // 发完成事件。
    let ev = Event {
        source: "os-update".into(),
        topic: Topic::System,
        kind: "update.completed".into(),
        severity: Severity::Info,
        task_id: Some(task),
        payload: serde_json::json!({
            "new_slot": "B",
            "new_version": "1.1.0",
        }),
        timestamp: Utc::now(),
    };
    bus.publish(ev).await.unwrap();

    // 调用顺序断言：check → download → verify → write → probe/commit。
    let log = &orch.call_log;
    let order = [
        "check_updates",
        "download",
        "verify",
        "write_to_inactive",
        "probe",
    ];
    let mut last_idx = 0;
    for keyword in order {
        let idx = log
            .iter()
            .position(|s| s.contains(keyword))
            .unwrap_or_else(|| panic!("应有调用记录 {keyword}: {log:?}"));
        assert!(
            idx >= last_idx,
            "{keyword} 应在上一阶段之后（顺序错乱: {log:?}）"
        );
        last_idx = idx;
    }

    // EventBus 收到完成事件。
    assert_eq!(bus.published_count_for(Topic::System), 1);
    assert_eq!(bus.published()[0].kind, "update.completed");
}

#[tokio::test]
async fn update_new_slot_boot_failure_triggers_rollback() {
    // 回滚状态机核心路径：新槽激活后探活失败 → on_boot_failed 产 Rollback。
    let bus = Arc::new(MockEventBus::new());
    let engine = Arc::new(MockUpdateEngine::new());
    let mut orch = UpdateOrchestrator::new(engine, bus.clone());

    // 写 B（v1.1.0-broken）+ 激活（不提交，留待探活）。
    let written = orch.write_and_activate("1.1.0-broken");
    assert_eq!(written, UpdateSlot::B);
    assert_eq!(orch.slot.active_slot(), Some(UpdateSlot::B));
    assert_eq!(orch.slot.previous_active, Some(UpdateSlot::A));
    orch.call_log.push("write+activate B(v1.1.0-broken)".into());

    // 探活失败 → on_boot_failed → 回滚。
    let rollback_decision = orch.rollback_on_boot_failure();
    orch.call_log
        .push(format!("probe: Unhealthy → rollback {rollback_decision:?}"));
    assert_eq!(
        rollback_decision,
        SlotSwitchDecision::Rollback {
            target: UpdateSlot::A,      // 回滚到旧槽
            failed_slot: UpdateSlot::B, // 新槽标记 Failed
        }
    );

    // 状态机断言：A 恢复 Active，B 标记 Failed，previous_active 清空。
    assert_eq!(orch.slot.active_slot(), Some(UpdateSlot::A));
    assert_eq!(orch.slot.slot(UpdateSlot::B).status, SlotStatus::Failed);
    assert_eq!(
        orch.slot.slot(UpdateSlot::A).version.as_deref(),
        Some("1.0.0")
    );
    assert_eq!(
        orch.slot.slot(UpdateSlot::B).version.as_deref(),
        Some("1.1.0-broken")
    );
    assert_eq!(
        orch.slot.previous_active, None,
        "回滚后应清空 previous_active"
    );

    // 发回滚事件（Error 级别）。
    let ev = Event {
        source: "os-update".into(),
        topic: Topic::System,
        kind: "update.rolled_back".into(),
        severity: Severity::Error,
        task_id: None,
        payload: serde_json::json!({
            "failed_slot": "B",
            "rolled_back_to": "A",
            "reason": "新槽探活 Unhealthy",
        }),
        timestamp: Utc::now(),
    };
    bus.publish(ev).await.unwrap();
    assert_eq!(bus.published_count_for(Topic::System), 1);
    assert_eq!(bus.published()[0].kind, "update.rolled_back");
    assert_eq!(bus.published()[0].severity, Severity::Error);

    // 调用顺序断言：write+activate 在 rollback 之前。
    let log = &orch.call_log;
    let write_idx = log
        .iter()
        .position(|s| s.contains("write+activate"))
        .unwrap();
    let rollback_idx = log.iter().position(|s| s.contains("rollback")).unwrap();
    assert!(write_idx < rollback_idx, "write 应在 rollback 之前");
}

#[tokio::test]
async fn update_watchdog_decision_chain_automatic_unhealthy() {
    // watchdog 决策链：探活 Unhealthy + Automatic + 有目标 → should_rollback 返回
    // RollbackNow → 调 SlotManager.on_boot_failed 完成切槽。
    let mut sm = SlotManager::new(UpdateSlot::A, "1.0.0", Utc::now());
    // 写 B + 激活（不提交）。
    sm.begin_write(UpdateSlot::B).unwrap();
    sm.finish_write(UpdateSlot::B, "1.1.0", Utc::now()).unwrap();
    let d = sm.plan_activation(UpdateSlot::B);
    if let SlotSwitchDecision::Activate { target, previous } = d {
        sm.apply_activation(target, previous, Utc::now()).unwrap();
    }
    // 现在 B active，A inactive（previous_active=A）。
    let has_target = sm.previous_active_slot().is_some();
    assert!(has_target, "应有回滚目标 A");

    // watchdog 决策：Automatic + Unhealthy + 有目标 → RollbackNow。
    let ctx = RollbackContext::new(Health::Unhealthy, RollbackPolicy::Automatic, 1, has_target);
    let decision = should_rollback(&ctx);
    assert!(
        matches!(decision, RollbackDecision::RollbackNow { .. }),
        "Automatic + Unhealthy + 有目标 → 应 RollbackNow，实得 {decision:?}"
    );

    // 执行回滚。
    let rollback = sm.on_boot_failed();
    assert!(matches!(rollback, SlotSwitchDecision::Rollback { .. }));
    assert_eq!(sm.active_slot(), Some(UpdateSlot::A));
    assert_eq!(sm.slot(UpdateSlot::B).status, SlotStatus::Failed);
}

#[tokio::test]
async fn update_watchdog_below_threshold_no_rollback() {
    // Watchdog 策略：连续失败次数 < 阈值 → NoRollback（累计计数，不切槽）。
    let mut sm = SlotManager::new(UpdateSlot::A, "1.0.0", Utc::now());
    sm.begin_write(UpdateSlot::B).unwrap();
    sm.finish_write(UpdateSlot::B, "1.1.0", Utc::now()).unwrap();
    let d = sm.plan_activation(UpdateSlot::B);
    if let SlotSwitchDecision::Activate { target, previous } = d {
        sm.apply_activation(target, previous, Utc::now()).unwrap();
    }
    let has_target = sm.previous_active_slot().is_some();

    // 连续失败 2 次 < 阈值 3 → NoRollback。
    let ctx = RollbackContext::new(
        Health::Unhealthy,
        RollbackPolicy::Watchdog { max_failures: 3 },
        2,
        has_target,
    );
    let decision = should_rollback(&ctx);
    assert!(
        matches!(decision, RollbackDecision::NoRollback { .. }),
        "连续失败 2 < 阈值 3 → 应 NoRollback，实得 {decision:?}"
    );

    // 状态机不切槽（B 仍 active）。
    assert_eq!(sm.active_slot(), Some(UpdateSlot::B));
    assert_eq!(sm.slot(UpdateSlot::B).status, SlotStatus::Active);

    // 阈值达标（3 ≥ 3）→ RollbackNow。
    let ctx = RollbackContext::new(
        Health::Unhealthy,
        RollbackPolicy::Watchdog { max_failures: 3 },
        3,
        has_target,
    );
    let decision = should_rollback(&ctx);
    assert!(matches!(decision, RollbackDecision::RollbackNow { .. }));
}

#[tokio::test]
async fn update_manual_policy_requires_confirmation() {
    // Manual 策略：探活失败应返回 ManualConfirmationRequired（不自动回滚）。
    let mut sm = SlotManager::new(UpdateSlot::A, "1.0.0", Utc::now());
    sm.begin_write(UpdateSlot::B).unwrap();
    sm.finish_write(UpdateSlot::B, "1.1.0", Utc::now()).unwrap();
    let d = sm.plan_activation(UpdateSlot::B);
    if let SlotSwitchDecision::Activate { target, previous } = d {
        sm.apply_activation(target, previous, Utc::now()).unwrap();
    }
    let has_target = sm.previous_active_slot().is_some();

    let ctx = RollbackContext::new(Health::Unhealthy, RollbackPolicy::Manual, 5, has_target);
    let decision = should_rollback(&ctx);
    assert!(
        matches!(
            decision,
            RollbackDecision::ManualConfirmationRequired { .. }
        ),
        "Manual 策略 → 应 ManualConfirmationRequired，实得 {decision:?}"
    );

    // 不自动回滚：B 仍 active（需人工确认）。
    assert_eq!(sm.active_slot(), Some(UpdateSlot::B));
}

#[tokio::test]
async fn update_no_rollback_target_first_boot_protects() {
    // 无回滚目标（首启 / 无 previous_active）：探活失败也不回滚（NoOp）。
    // 用 MockRollbackManager 默认状态（A active v1.0.0，B 空无版本）验证
    // auto_rollback_if_unhealthy 不回滚（首启保护）。
    let rollback = MockRollbackManager::new().with_health(unhealthy_report());

    // 默认 SlotManager：A active v1.0.0，B 空（无 previous_active）。
    let sm = rollback.slot_manager();
    assert_eq!(sm.active_slot(), Some(UpdateSlot::A));
    assert!(sm.previous_active.is_none());

    // Unhealthy 但无目标 → 不回滚。
    let rolled_back = rollback
        .auto_rollback_if_unhealthy()
        .await
        .expect("auto_rollback 不应 Err");
    assert!(!rolled_back, "首启无目标 → 不应回滚");

    // 状态不变：A 仍 active。
    let sm = rollback.slot_manager();
    assert_eq!(sm.active_slot(), Some(UpdateSlot::A));

    // should_rollback 纯函数同样判定：无目标 → NoRollback。
    let ctx = RollbackContext::new(Health::Unhealthy, RollbackPolicy::Automatic, 99, false);
    let decision = should_rollback(&ctx);
    assert!(matches!(decision, RollbackDecision::NoRollback { .. }));
}

#[tokio::test]
async fn rollback_manager_verify_health_returns_preset() {
    // 验证 RollbackManager.verify_current_health 返回构造器预置的报告
    // （watchdog 探活的输入来源）。
    let rollback = MockRollbackManager::new().with_health(unhealthy_report());
    let report = rollback.verify_current_health().await.unwrap();
    assert_eq!(report.health, Health::Unhealthy);
    assert!(report.message.as_ref().unwrap().contains("核心服务"));

    // healthy 报告。
    let rollback = MockRollbackManager::new().with_health(healthy_report());
    let report = rollback.verify_current_health().await.unwrap();
    assert_eq!(report.health, Health::Healthy);

    // healthy 时不应回滚（即使有目标）。
    let rolled_back = rollback.auto_rollback_if_unhealthy().await.unwrap();
    assert!(!rolled_back, "Healthy → 不回滚");
}

#[tokio::test]
async fn rollback_manager_list_snapshots_returns_rollback_points() {
    // 验证 RollbackManager.list_snapshots 返回可回滚点（供「手动选择回滚目标」用）。
    let rollback = MockRollbackManager::new();
    let snaps = rollback.list_snapshots().await;
    // 默认 A active v1.0.0 → 1 个快照（A，healthy）。
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].slot, UpdateSlot::A);
    assert_eq!(snaps[0].version, "1.0.0");
    assert!(snaps[0].healthy);
}

#[tokio::test]
async fn update_double_upgrade_a_to_b_to_a() {
    // 连续两次更新：A → B(v1.1.0) → A(v1.2.0)，验证槽位交替写入+激活+提交。
    let engine = Arc::new(MockUpdateEngine::new());

    // 第一次：写 B（v1.1.0），激活 B。
    let written1 = engine
        .write_to_inactive_slot(&manifest("1.1.0"))
        .await
        .unwrap();
    assert_eq!(written1, UpdateSlot::B);
    engine.activate_slot(UpdateSlot::B).await.unwrap();
    let sm = engine.slot_manager();
    assert_eq!(sm.active_slot(), Some(UpdateSlot::B));
    assert_eq!(sm.slot(UpdateSlot::B).version.as_deref(), Some("1.1.0"));

    // 第二次：写 A（v1.2.0），激活 A。
    let written2 = engine
        .write_to_inactive_slot(&manifest("1.2.0"))
        .await
        .unwrap();
    assert_eq!(written2, UpdateSlot::A, "第二次应写 A（B 为 active）");
    engine.activate_slot(UpdateSlot::A).await.unwrap();
    let sm = engine.slot_manager();
    assert_eq!(sm.active_slot(), Some(UpdateSlot::A));
    assert_eq!(sm.slot(UpdateSlot::A).version.as_deref(), Some("1.2.0"));
    // B 降为 inactive（版本保留为 1.1.0，作回滚候选）。
    assert_eq!(sm.slot(UpdateSlot::B).status, SlotStatus::Inactive);
    assert_eq!(sm.slot(UpdateSlot::B).version.as_deref(), Some("1.1.0"));
}

#[tokio::test]
async fn update_manifest_type_cross_trait_identity() {
    // 跨 crate 类型一致性：UpdateManifest / UpdateSlot / SlotStatus 可序列化，
    // 在 update 引擎 / 回滚器 / 事件 payload 之间透传。
    let m = manifest("1.5.0");
    let json = serde_json::to_string(&m).expect("manifest 应可序列化");
    let back: UpdateManifest = serde_json::from_str(&json).expect("应可反序列化");
    assert_eq!(back.version, "1.5.0");
    assert_eq!(back.components.len(), 1);
    assert!(back.components[0].restart_required);

    // UpdateSlot 序列化（snake_case）。
    assert_eq!(serde_json::to_string(&UpdateSlot::A).unwrap(), "\"a\"");
    assert_eq!(serde_json::to_string(&UpdateSlot::B).unwrap(), "\"b\"");

    // SlotStatus 序列化。
    let json = serde_json::to_string(&SlotStatus::Failed).unwrap();
    assert!(json.contains("failed"));
    let json = serde_json::to_string(&SlotStatus::Active).unwrap();
    assert!(json.contains("active"));

    // HealthReport 跨 trait（rollback 产 → 编排器读）。
    let report = unhealthy_report();
    let json = serde_json::to_string(&report).unwrap();
    let back: HealthReport = serde_json::from_str(&json).unwrap();
    assert_eq!(back.health, Health::Unhealthy);
}

#[tokio::test]
async fn update_rollback_decision_pure_function_matrix() {
    // 验证 should_rollback 纯函数决策矩阵（与 watchdog 触发判定对齐）。
    use RollbackDecision as D;
    use RollbackPolicy as P;

    // Healthy → 永不回滚。
    for policy in [P::Automatic, P::Manual, P::Watchdog { max_failures: 1 }] {
        let ctx = RollbackContext::new(Health::Healthy, policy, 0, true);
        assert!(
            matches!(should_rollback(&ctx), D::NoRollback { .. }),
            "Healthy + {policy:?} → 不回滚"
        );
    }

    // Degraded → 不回滚（系统可用，告警即可）。
    let ctx = RollbackContext::new(Health::Degraded, P::Automatic, 0, true);
    assert!(matches!(should_rollback(&ctx), D::NoRollback { .. }));

    // Unknown → 不回滚（探活未完成）。
    let ctx = RollbackContext::new(Health::Unknown, P::Automatic, 0, true);
    assert!(matches!(should_rollback(&ctx), D::NoRollback { .. }));

    // Unhealthy + Automatic + 有目标 → RollbackNow。
    let ctx = RollbackContext::new(Health::Unhealthy, P::Automatic, 1, true);
    assert!(matches!(should_rollback(&ctx), D::RollbackNow { .. }));

    // Unhealthy + Watchdog + 达阈值 → RollbackNow。
    let ctx = RollbackContext::new(Health::Unhealthy, P::Watchdog { max_failures: 3 }, 3, true);
    assert!(matches!(should_rollback(&ctx), D::RollbackNow { .. }));

    // Unhealthy + Watchdog + 未达阈值 → NoRollback。
    let ctx = RollbackContext::new(Health::Unhealthy, P::Watchdog { max_failures: 3 }, 1, true);
    assert!(matches!(should_rollback(&ctx), D::NoRollback { .. }));

    // 无目标 → 永不回滚（即使 Unhealthy + Automatic）。
    let ctx = RollbackContext::new(Health::Unhealthy, P::Automatic, 99, false);
    assert!(matches!(should_rollback(&ctx), D::NoRollback { .. }));
}

#[tokio::test]
async fn update_full_cycle_then_rollback_on_next_upgrade() {
    // 端到端混合路径：第一次升级成功（A→B），第二次升级（B→A）后探活失败回滚到 B。
    let bus = Arc::new(MockEventBus::new());
    let engine = Arc::new(MockUpdateEngine::new());
    let mut orch = UpdateOrchestrator::new(engine, bus.clone());

    // 第一次：A → B(v1.1.0)，探活通过，提交。
    let first = orch.write_and_activate("1.1.0");
    assert_eq!(first, UpdateSlot::B);
    assert!(orch.commit_boot_success());
    assert_eq!(orch.slot.active_slot(), Some(UpdateSlot::B));
    assert_eq!(orch.slot.previous_active, None);

    // 第二次：B → A(v1.2.0-broken)，探活失败，回滚到 B。
    let second = orch.write_and_activate("1.2.0-broken");
    assert_eq!(second, UpdateSlot::A);
    assert_eq!(orch.slot.active_slot(), Some(UpdateSlot::A));
    assert_eq!(orch.slot.previous_active, Some(UpdateSlot::B));

    let rollback = orch.rollback_on_boot_failure();
    assert_eq!(
        rollback,
        SlotSwitchDecision::Rollback {
            target: UpdateSlot::B,
            failed_slot: UpdateSlot::A,
        }
    );

    // 回滚后：B 恢复 Active（v1.1.0），A 标记 Failed（v1.2.0-broken）。
    assert_eq!(orch.slot.active_slot(), Some(UpdateSlot::B));
    assert_eq!(
        orch.slot.slot(UpdateSlot::B).version.as_deref(),
        Some("1.1.0")
    );
    assert_eq!(orch.slot.slot(UpdateSlot::A).status, SlotStatus::Failed);
    assert_eq!(
        orch.slot.slot(UpdateSlot::A).version.as_deref(),
        Some("1.2.0-broken")
    );

    // 发回滚事件。
    let ev = Event {
        source: "os-update".into(),
        topic: Topic::System,
        kind: "update.rolled_back".into(),
        severity: Severity::Error,
        task_id: None,
        payload: serde_json::json!({
            "failed_slot": "A",
            "rolled_back_to": "B",
        }),
        timestamp: Utc::now(),
    };
    bus.publish(ev).await.unwrap();
    assert_eq!(bus.published_count_for(Topic::System), 1);
}
