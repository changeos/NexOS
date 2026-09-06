//! 场景10：osd 启动编排链 —— 集成测试
//!
//! 覆盖：
//! - SystemdOrchestrator 正确拓扑排序（Kahn's algorithm BFS）
//! - 循环依赖检测 -> DependencyCycle 错误
//! - 组件状态机：Stopped -> Starting -> Running（通过 Orchestrator trait）
//! - 禁用组件不可启动
//! - start 幂等性（Running/Starting 再次 start 不报错）
//! - cgroup 配额设置/读取（InMemoryCgroupBackend 注入）
//! - MockHealthProbe 健康探针模拟（Unhealthy 触发 restart -> 恢复）
//! - 全链路启动：拓扑排序 -> 逐个 start -> 全部 Running

use os_core::{Health, HealthReport, ResourceQuota};
use osd::component::HealthProbeConfig;
use osd::{
    CgroupBackend, ComponentDescriptor, ComponentId, ComponentRegistry, ComponentStatus,
    HealthProbe, InMemoryCgroupBackend, MockHealthProbe, Orchestrator, OrchestratorError,
    SystemdOrchestrator,
};

// ============================================================================
// 辅助构造（本地定义，因为 osd 内部 desc_with 未导出）
// ============================================================================

fn make_quota(cpu: f32) -> ResourceQuota {
    ResourceQuota {
        cpu_cores: Some(cpu),
        memory_bytes: None,
        io_bps_limit: None,
    }
}

fn make_desc(id: &str, deps: &[&str]) -> ComponentDescriptor {
    make_desc_with(id, deps, true)
}

fn make_desc_with(id: &str, deps: &[&str], enabled: bool) -> ComponentDescriptor {
    ComponentDescriptor {
        id: ComponentId::new(id),
        dependencies: deps.iter().map(|&s| ComponentId::new(s)).collect(),
        quota: make_quota(1.0),
        health_probe: HealthProbeConfig {
            kind: "tcp".into(),
            target: "127.0.0.1:8080/health".into(),
            interval_secs: 5,
            timeout_secs: 2,
            failure_threshold: 3,
        },
        command: Some("/usr/bin/os-component".into()),
        enabled,
    }
}

fn make_full_quota() -> ResourceQuota {
    ResourceQuota {
        cpu_cores: Some(4.0),
        memory_bytes: Some(2 * 1024 * 1024 * 1024),
        io_bps_limit: Some(100_000_000),
    }
}

/// 构造注入内存 cgroup 后端的编排器
fn build_orchestrator(descs: &[ComponentDescriptor]) -> SystemdOrchestrator {
    let registry = ComponentRegistry::from_descriptors(descs.to_vec());
    SystemdOrchestrator::with_cgroup_backend(
        registry,
        "test-os",
        Box::new(InMemoryCgroupBackend::new()),
    )
}

// ============================================================================
// 1. 拓扑排序正确性
// ============================================================================

#[tokio::test]
async fn topological_sort_linear_chain() {
    // A -> B -> C (B depends on A, C depends on B)
    let orch = build_orchestrator(&[
        make_desc("os-api", &["os-meta"]),
        make_desc("os-meta", &["os-storage"]),
        make_desc("os-storage", &[]),
    ]);

    let order = orch
        .startup_order()
        .expect("Linear chain should be sortable");
    assert_eq!(order.len(), 3);

    let pos = |id: &str| -> usize {
        order
            .iter()
            .position(|x| x.as_str() == id)
            .unwrap_or_else(|| panic!("{} not in order", id))
    };

    // os-storage has no deps -> should come first
    assert!(pos("os-storage") < pos("os-meta"));
    // os-meta depends on os-storage -> should come after
    assert!(pos("os-meta") < pos("os-api"));
    // os-api depends on os-meta -> should come last
    assert!(pos("os-api") > pos("os-meta"));
}

#[tokio::test]
async fn topological_sort_diamond_dependency() {
    //     A
    //    / \
    //   B   C
    //    \ /
    //     D
    let orch = build_orchestrator(&[
        make_desc("d", &["b", "c"]),
        make_desc("b", &["a"]),
        make_desc("c", &["a"]),
        make_desc("a", &[]),
    ]);

    let order = orch.startup_order().expect("Diamond should be sortable");
    assert_eq!(order.len(), 4);

    let pos = |id: &str| -> usize { order.iter().position(|x| x.as_str() == id).unwrap() };

    // A must come before B and C
    assert!(pos("a") < pos("b"));
    assert!(pos("a") < pos("c"));
    // B and C must come before D
    assert!(pos("b") < pos("d"));
    assert!(pos("c") < pos("d"));
}

#[tokio::test]
async fn topological_sort_single_component() {
    let orch = build_orchestrator(&[make_desc("standalone", &[])]);
    let order = orch.startup_order().unwrap();
    assert_eq!(order.len(), 1);
    assert_eq!(order[0].as_str(), "standalone");
}

#[tokio::test]
async fn topological_sort_independent_components() {
    // No dependencies between A, B, C -> all valid orderings
    let orch = build_orchestrator(&[
        make_desc("alpha", &[]),
        make_desc("beta", &[]),
        make_desc("gamma", &[]),
    ]);

    let order = orch.startup_order().unwrap();
    assert_eq!(order.len(), 3);
    let names: Vec<&str> = order.iter().map(|x| x.as_str()).collect();
    assert!(names.contains(&"alpha"));
    assert!(names.contains(&"beta"));
    assert!(names.contains(&"gamma"));
}

// ============================================================================
// 2. 循环依赖检测
// ============================================================================

#[tokio::test]
async fn cycle_detection_two_node_cycle() {
    // A -> B -> A (mutual dependency)
    let orch = build_orchestrator(&[make_desc("a", &["b"]), make_desc("b", &["a"])]);

    let err = orch.startup_order().expect_err("Cycle should be detected");
    match &err {
        OrchestratorError::DependencyCycle { cycle } => {
            assert!(!cycle.is_empty(), "Cycle string should not be empty");
        }
        other => panic!("Expected DependencyCycle, got {:?}", other),
    }
}

#[tokio::test]
async fn cycle_detection_three_node_cycle() {
    // A -> B -> C -> A
    let orch = build_orchestrator(&[
        make_desc("a", &["c"]),
        make_desc("b", &["a"]),
        make_desc("c", &["b"]),
    ]);

    let err = orch
        .startup_order()
        .expect_err("3-node cycle should be detected");
    assert!(matches!(err, OrchestratorError::DependencyCycle { .. }));
}

#[tokio::test]
async fn cycle_detection_self_dependency() {
    // A depends on itself
    let orch = build_orchestrator(&[make_desc("self-loop", &["self-loop"])]);

    let err = orch
        .startup_order()
        .expect_err("Self-dependency should be detected");
    assert!(matches!(err, OrchestratorError::DependencyCycle { .. }));
}

#[tokio::test]
async fn cycle_error_contains_node_names() {
    let orch = build_orchestrator(&[make_desc("x", &["y"]), make_desc("y", &["x"])]);

    let err = orch.startup_order().unwrap_err();
    match &err {
        OrchestratorError::DependencyCycle { cycle } => {
            assert!(cycle.contains('x'), "Cycle message should mention node x");
            assert!(cycle.contains('y'), "Cycle message should mention node y");
        }
        other => panic!("Expected DependencyCycle, got {:?}", other),
    }
}

// ============================================================================
// 3. 组件状态机
// ============================================================================

#[tokio::test]
async fn start_transitions_stopped_to_running() {
    let orch = build_orchestrator(&[make_desc("svc", &[])]);

    // Before start: status defaults to Stopped
    assert_eq!(
        orch.status(&ComponentId::new("svc")).await.unwrap(),
        ComponentStatus::Stopped
    );

    // Start
    orch.start(&ComponentId::new("svc")).await.unwrap();

    // After start: Running
    assert_eq!(
        orch.status(&ComponentId::new("svc")).await.unwrap(),
        ComponentStatus::Running
    );
}

#[tokio::test]
async fn start_unknown_component_returns_error() {
    let orch = build_orchestrator(&[make_desc("svc", &[])]);
    let err = orch
        .start(&ComponentId::new("nonexistent"))
        .await
        .expect_err("Starting unknown component should fail");
    assert!(matches!(err, OrchestratorError::ComponentNotFound(_)));
}

#[tokio::test]
async fn stop_transitions_running_to_stopped() {
    let orch = build_orchestrator(&[make_desc("svc", &[])]);
    orch.start(&ComponentId::new("svc")).await.unwrap();
    assert_eq!(
        orch.status(&ComponentId::new("svc")).await.unwrap(),
        ComponentStatus::Running
    );

    orch.stop(&ComponentId::new("svc")).await.unwrap();
    assert_eq!(
        orch.status(&ComponentId::new("svc")).await.unwrap(),
        ComponentStatus::Stopped
    );
}

#[tokio::test]
async fn stop_unknown_component_returns_error() {
    let orch = build_orchestrator(&[make_desc("svc", &[])]);
    let err = orch
        .stop(&ComponentId::new("nonexistent"))
        .await
        .expect_err("Stopping unknown component should fail");
    assert!(matches!(err, OrchestratorError::ComponentNotFound(_)));
}

#[tokio::test]
async fn restart_running_returns_to_running() {
    let orch = build_orchestrator(&[make_desc("svc", &[])]);
    orch.start(&ComponentId::new("svc")).await.unwrap();
    orch.restart(&ComponentId::new("svc")).await.unwrap();
    assert_eq!(
        orch.status(&ComponentId::new("svc")).await.unwrap(),
        ComponentStatus::Running
    );
}

// ============================================================================
// 4. 禁用组件
// ============================================================================

#[tokio::test]
async fn disabled_component_cannot_be_started() {
    let orch = build_orchestrator(&[make_desc_with("offline-svc", &[], false)]);

    let err = orch
        .start(&ComponentId::new("offline-svc"))
        .await
        .expect_err("Disabled component should not start");
    match &err {
        OrchestratorError::StartFailed { component, reason } => {
            assert_eq!(component.as_str(), "offline-svc");
            assert!(
                reason.contains("禁用") || reason.contains("disabled"),
                "Error should mention disabled"
            );
        }
        other => panic!("Expected StartFailed, got {:?}", other),
    }

    // Status should reflect Disabled
    assert_eq!(
        orch.status(&ComponentId::new("offline-svc")).await.unwrap(),
        ComponentStatus::Disabled
    );
}

#[tokio::test]
async fn start_idempotent_when_already_running() {
    let orch = build_orchestrator(&[make_desc("svc", &[])]);
    orch.start(&ComponentId::new("svc")).await.unwrap();
    // Second start: should be idempotent, not error
    orch.start(&ComponentId::new("svc")).await.unwrap();
    assert_eq!(
        orch.status(&ComponentId::new("svc")).await.unwrap(),
        ComponentStatus::Running
    );
}

#[tokio::test]
async fn stop_idempotent_when_already_stopped() {
    let orch = build_orchestrator(&[make_desc("svc", &[])]);
    // Stop before ever starting: should be idempotent
    orch.stop(&ComponentId::new("svc")).await.unwrap();
    assert_eq!(
        orch.status(&ComponentId::new("svc")).await.unwrap(),
        ComponentStatus::Stopped
    );
}

// ============================================================================
// 5. cgroup 配额设置/读取
// ============================================================================

#[tokio::test]
async fn set_and_get_quota_roundtrip() {
    let orch = build_orchestrator(&[make_desc("svc", &[])]);
    let new_quota = make_full_quota();

    orch.set_quota(&ComponentId::new("svc"), new_quota.clone())
        .await
        .unwrap();

    let got = orch.get_quota(&ComponentId::new("svc")).await.unwrap();
    assert_eq!(got.cpu_cores, Some(4.0));
    assert_eq!(got.memory_bytes, Some(2 * 1024 * 1024 * 1024));
    assert_eq!(got.io_bps_limit, Some(100_000_000));
}

#[tokio::test]
async fn get_quota_returns_descriptor_default_when_unset() {
    let orch = build_orchestrator(&[make_desc("svc", &[])]);
    // Descriptor has cpu_cores: Some(1.0)
    let got = orch.get_quota(&ComponentId::new("svc")).await.unwrap();
    assert_eq!(got.cpu_cores, Some(1.0));
    assert!(got.memory_bytes.is_none());
    assert!(got.io_bps_limit.is_none());
}

#[tokio::test]
async fn set_quota_overwrites_previous() {
    let orch = build_orchestrator(&[make_desc("svc", &[])]);

    orch.set_quota(&ComponentId::new("svc"), make_quota(1.0))
        .await
        .unwrap();
    orch.set_quota(&ComponentId::new("svc"), make_quota(3.5))
        .await
        .unwrap();

    let got = orch.get_quota(&ComponentId::new("svc")).await.unwrap();
    assert_eq!(got.cpu_cores, Some(3.5));
}

#[tokio::test]
async fn set_quota_isolated_per_component() {
    let orch = build_orchestrator(&[make_desc("svc-a", &[]), make_desc("svc-b", &[])]);

    orch.set_quota(&ComponentId::new("svc-a"), make_quota(1.0))
        .await
        .unwrap();
    orch.set_quota(&ComponentId::new("svc-b"), make_quota(2.5))
        .await
        .unwrap();

    assert_eq!(
        orch.get_quota(&ComponentId::new("svc-a"))
            .await
            .unwrap()
            .cpu_cores,
        Some(1.0)
    );
    assert_eq!(
        orch.get_quota(&ComponentId::new("svc-b"))
            .await
            .unwrap()
            .cpu_cores,
        Some(2.5)
    );
}

#[tokio::test]
async fn set_quota_unknown_component_errors() {
    let orch = build_orchestrator(&[make_desc("svc", &[])]);
    let err = orch
        .set_quota(&ComponentId::new("nonexistent"), make_quota(1.0))
        .await
        .expect_err("Set quota on unknown component should fail");
    assert!(matches!(err, OrchestratorError::ComponentNotFound(_)));
}

#[tokio::test]
async fn set_quota_unlimited_fields() {
    let orch = build_orchestrator(&[make_desc("svc", &[])]);
    let unlimited = ResourceQuota {
        cpu_cores: None,
        memory_bytes: None,
        io_bps_limit: None,
    };
    orch.set_quota(&ComponentId::new("svc"), unlimited)
        .await
        .unwrap();
    let got = orch.get_quota(&ComponentId::new("svc")).await.unwrap();
    assert!(got.cpu_cores.is_none());
    assert!(got.memory_bytes.is_none());
    assert!(got.io_bps_limit.is_none());
}

// ============================================================================
// 6. MockHealthProbe 健康探针模拟
// ============================================================================

#[tokio::test]
async fn health_probe_returns_configured_report() {
    let probe = MockHealthProbe::new().with_report(HealthReport {
        health: Health::Healthy,
        message: Some("all good".into()),
        timestamp: chrono::Utc::now(),
    });
    let report = probe.probe().await;
    assert_eq!(report.health, Health::Healthy);
    assert_eq!(report.message.as_deref(), Some("all good"));
}

#[tokio::test]
async fn health_probe_runtime_state_transition() {
    let probe = MockHealthProbe::new().with_report(HealthReport {
        health: Health::Healthy,
        message: None,
        timestamp: chrono::Utc::now(),
    });
    assert_eq!(probe.probe().await.health, Health::Healthy);

    // Simulate degradation
    probe.set_report(HealthReport {
        health: Health::Unhealthy,
        message: Some("connection refused".into()),
        timestamp: chrono::Utc::now(),
    });
    assert_eq!(probe.probe().await.health, Health::Unhealthy);

    // Simulate recovery
    probe.set_report(HealthReport {
        health: Health::Healthy,
        message: Some("recovered".into()),
        timestamp: chrono::Utc::now(),
    });
    assert_eq!(probe.probe().await.health, Health::Healthy);
}

#[tokio::test]
async fn health_probe_cloned_instances_share_state() {
    let probe = MockHealthProbe::new().with_report(HealthReport {
        health: Health::Degraded,
        message: None,
        timestamp: chrono::Utc::now(),
    });
    let probe_clone = probe.clone();

    probe_clone.set_report(HealthReport {
        health: Health::Unhealthy,
        message: None,
        timestamp: chrono::Utc::now(),
    });

    // Original should see the change via Arc
    assert_eq!(probe.probe().await.health, Health::Unhealthy);
}

// ============================================================================
// 7. InMemoryCgroupBackend 直接测试
// ============================================================================

#[test]
fn in_memory_cgroup_backend_apply_and_read() {
    let backend = InMemoryCgroupBackend::new();
    let cid = ComponentId::new("test-comp");
    let quota = ResourceQuota {
        cpu_cores: Some(2.0),
        memory_bytes: Some(1024 * 1024 * 1024),
        io_bps_limit: None,
    };

    // Apply quota
    backend
        .apply_quota("test-os", &cid, &quota)
        .expect("apply should succeed");

    // Read back
    let read = backend
        .read_quota("test-os", &cid)
        .expect("read should succeed")
        .expect("quota should exist");
    assert_eq!(read.cpu_cores, Some(2.0));
    assert_eq!(read.memory_bytes, Some(1024 * 1024 * 1024));
}

#[test]
fn in_memory_cgroup_backend_missing_returns_none() {
    let backend = InMemoryCgroupBackend::new();
    let cid = ComponentId::new("nonexistent");
    let result = backend.read_quota("test-os", &cid).unwrap();
    assert!(result.is_none(), "Missing quota should return None");
}

// ============================================================================
// 8. 全链路启动：拓扑排序 -> 逐个 start -> 全部 Running
// ============================================================================

#[tokio::test]
async fn full_startup_chain_topo_sort_then_start_all() {
    // Define a realistic component graph:
    //   os-storage (no deps)
    //   os-meta (depends on os-storage)
    //   os-api (depends on os-meta)
    //   os-monitor (depends on os-storage)
    let orch = build_orchestrator(&[
        make_desc("os-storage", &[]),
        make_desc("os-meta", &["os-storage"]),
        make_desc("os-api", &["os-meta"]),
        make_desc("os-monitor", &["os-storage"]),
    ]);

    // Step 1: Compute startup order via topological sort
    let order = orch
        .startup_order()
        .expect("Should compute valid startup order");
    assert_eq!(order.len(), 4);

    // Step 2: Start each component in topological order
    for component_id in &order {
        orch.start(component_id).await.unwrap_or_else(|e| {
            panic!(
                "Starting {} should succeed, got: {:?}",
                component_id.as_str(),
                e
            )
        });
    }

    // Step 3: Verify all components are Running
    for desc in orch.list_components().await.unwrap() {
        let status = orch.status(&desc.id).await.unwrap();
        assert_eq!(
            status,
            ComponentStatus::Running,
            "Component {} should be Running after startup chain",
            desc.id.as_str()
        );
    }
}

#[tokio::test]
async fn full_startup_chain_with_quota_assignment() {
    // Same as above, but also assign per-component quotas during startup
    let orch = build_orchestrator(&[
        {
            let mut d = make_desc("os-storage", &[]);
            d.quota = make_quota(2.0);
            d
        },
        {
            let mut d = make_desc("os-meta", &["os-storage"]);
            d.quota = ResourceQuota {
                cpu_cores: Some(1.5),
                memory_bytes: Some(512 * 1024 * 1024),
                io_bps_limit: None,
            };
            d
        },
        {
            let mut d = make_desc("os-api", &["os-meta"]);
            d.quota = ResourceQuota {
                cpu_cores: Some(0.5),
                memory_bytes: Some(256 * 1024 * 1024),
                io_bps_limit: Some(50_000_000),
            };
            d
        },
    ]);

    // Start in topological order
    let order = orch.startup_order().unwrap();
    for cid in &order {
        orch.start(cid).await.unwrap();
    }

    // Verify all Running
    for cid in &order {
        assert_eq!(orch.status(cid).await.unwrap(), ComponentStatus::Running);
    }

    // Verify quotas match descriptor defaults
    assert_eq!(
        orch.get_quota(&ComponentId::new("os-storage"))
            .await
            .unwrap()
            .cpu_cores,
        Some(2.0)
    );
    assert_eq!(
        orch.get_quota(&ComponentId::new("os-meta"))
            .await
            .unwrap()
            .cpu_cores,
        Some(1.5)
    );
    assert_eq!(
        orch.get_quota(&ComponentId::new("os-api"))
            .await
            .unwrap()
            .cpu_cores,
        Some(0.5)
    );
}

// ============================================================================
// 9. Health check 触发 restart 模式
// ============================================================================

#[tokio::test]
async fn health_probe_unhealthy_triggers_restart() {
    // Simulate: start component -> detect unhealthy via probe -> restart -> verify Running
    let orch = build_orchestrator(&[make_desc("svc", &[])]);
    let probe = MockHealthProbe::new().with_report(HealthReport {
        health: Health::Healthy,
        message: None,
        timestamp: chrono::Utc::now(),
    });

    // Start component
    orch.start(&ComponentId::new("svc")).await.unwrap();
    assert_eq!(
        orch.status(&ComponentId::new("svc")).await.unwrap(),
        ComponentStatus::Running
    );

    // Simulate health check detects Unhealthy
    probe.set_report(HealthReport {
        health: Health::Unhealthy,
        message: Some("TCP connection refused".into()),
        timestamp: chrono::Utc::now(),
    });
    let report = probe.probe().await;
    assert_eq!(report.health, Health::Unhealthy);

    // Orchestrator restarts the component (health check failure threshold exceeded)
    orch.restart(&ComponentId::new("svc")).await.unwrap();
    assert_eq!(
        orch.status(&ComponentId::new("svc")).await.unwrap(),
        ComponentStatus::Running,
        "After restart, component should be Running again"
    );

    // After restart, health should recover (simulated)
    probe.set_report(HealthReport {
        health: Health::Healthy,
        message: Some("recovered after restart".into()),
        timestamp: chrono::Utc::now(),
    });
    assert_eq!(probe.probe().await.health, Health::Healthy);
}

// ============================================================================
// 10. list_components 验证
// ============================================================================

#[tokio::test]
async fn list_components_returns_all_registered() {
    let orch = build_orchestrator(&[
        make_desc("a", &[]),
        make_desc("b", &["a"]),
        make_desc("c", &["b"]),
    ]);

    let list = orch.list_components().await.unwrap();
    assert_eq!(list.len(), 3);

    let ids: Vec<&str> = list.iter().map(|d| d.id.as_str()).collect();
    assert!(ids.contains(&"a"));
    assert!(ids.contains(&"b"));
    assert!(ids.contains(&"c"));
}

// ============================================================================
// 11. ComponentRegistry 验证
// ============================================================================

#[test]
fn registry_from_descriptors() {
    let mut registry = ComponentRegistry::new();
    registry.register(make_desc("x", &[]));
    registry.register(make_desc("y", &["x"]));

    assert_eq!(registry.len(), 2);
    assert!(registry.get(&ComponentId::new("x")).is_some());
    assert!(registry.get(&ComponentId::new("y")).is_some());
    assert!(registry.get(&ComponentId::new("missing")).is_none());

    let all = registry.all();
    assert_eq!(all.len(), 2);
}
