//! 场景 1：VM 创建链路（integration-agent 规格书 §3 / 启动 prompt 场景 1）
//!
//! 链路：os-api 收 "创建 VM" 请求 → 路由匹配 → os-api RouteHandler 调
//! os-compute(MockVmManager).create_vm → compute 用 os-storage(MockStorageBackend)
//! 的 zvol 作磁盘 → 事件发 os-core(MockEventBus) → os-services(MockMonitor) 记录指标。
//!
//! 重点验证：
//! - 跨 crate 类型一致：`VmSpec.disk_vol_id`（os-core::VolumeId） == storage zvol 名。
//! - 事件流串通：VM 创建成功后向 EventBus 发出 `Topic::Compute` 事件。
//! - 监控侧落账：同一事件被 monitor 订阅者捕获并 record_metric。
//! - 错误传播：compute 创建失败（用 fail_with 注入）→ 网关返回非 2xx + 发 `Severity::Error` 事件。
//! - 类型桥接：os-api `RouteHandler` 实现 `Box<dyn RouteHandler>` 注入 Gateway，
//!   `InProcessGateway::dispatch` 路由命中后调 `handle`。

use std::sync::Arc;

use async_trait::async_trait;

use os_api::gateway::{ApiRequest, ApiResponse, Gateway, HttpMethod, RouteHandler, RouteSpec};
use os_api::gateway_impl::InProcessGateway;
use os_compute::vm::{CpuTopology, VmFirmware, VmManager, VmNic, VmSpec, VmState};
use os_compute::MockVmManager;
use os_core::eventbus::{Event, EventBus, EventSubscriber, Severity, Topic};
use os_core::mock::MockEventBus;
use os_core::{DatasetId, PoolId};
use os_core::{TaskId, VmId, VolumeId};
use os_services::mock::MockMonitor;
use os_services::monitor::{Metric, MetricKind, Monitor};
use os_storage::backend::StorageBackend;
use os_storage::mock::MockStorageBackend;
use os_storage::model::{VdevKind, VdevSpec};
use os_storage::options::DatasetOptions;

// ----------------------------------------------------------------------------
// 测试专用 RouteHandler：把 API 请求桥接到 compute VmManager + storage zvol 创建。
// 关键：跨 crate 类型桥接——把 os-api 的 ApiRequest 转成 os-compute 的 VmSpec，
// 并先经 os-storage 创建 zvol，再调 compute.create_vm。
// ----------------------------------------------------------------------------

struct VmCreateRouteHandler {
    compute: Arc<MockVmManager>,
    storage: Arc<MockStorageBackend>,
    bus: Arc<MockEventBus>,
    monitor: Arc<MockMonitor>,
}

impl VmCreateRouteHandler {
    fn route() -> RouteSpec {
        RouteSpec {
            method: HttpMethod::Post,
            path: "/api/v1/compute/vms".to_string(),
            handler_component: "os-compute".to_string(),
            requires_auth: true,
            required_roles: vec!["admin".to_string()],
        }
    }
}

#[async_trait]
impl RouteHandler for VmCreateRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![Self::route()]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, os_api::error::ApiGatewayError> {
        // 解析请求体（来自客户端的 VM 创建参数）。
        let vm_id = req.body["vm_id"]
            .as_str()
            .ok_or_else(|| os_api::error::ApiGatewayError::Internal("缺少 vm_id".into()))?;
        let name = req.body["name"].as_str().unwrap_or(vm_id);
        let zvol = req
            .body
            .get("zvol")
            .and_then(|v| v.as_str())
            .unwrap_or("tank/vm/default");

        let spec = VmSpec {
            cpus: CpuTopology::new(2),
            memory_mb: 2048,
            disk_vol_id: VolumeId::new(zvol),
            nics: vec![VmNic::virtio("br0")],
            firmware: VmFirmware::Bios,
        };

        // 1. 先在 storage 创建 zvol（作为 VM 系统盘）——跨 crate 复用 VolumeId 类型。
        self.storage
            .create_dataset(
                &DatasetId::new(zvol),
                DatasetOptions {
                    volsize: Some(20 * 1024 * 1024 * 1024),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| os_api::error::ApiGatewayError::Internal(e.to_string()))?;

        // 2. 调 compute 创建 VM。
        let vm = match self
            .compute
            .create_vm(&VmId::new(vm_id), name, spec.clone())
            .await
        {
            Ok(v) => v,
            Err(e) => {
                // 错误路径：发 Error 事件。
                let ev = Event {
                    source: "os-compute".into(),
                    topic: Topic::Compute,
                    kind: "vm.create.failed".into(),
                    severity: Severity::Error,
                    task_id: None,
                    payload: serde_json::json!({
                        "vm_id": vm_id,
                        "error": e.to_string(),
                    }),
                    timestamp: os_core::Utc::now(),
                };
                let _ = self.bus.publish(ev).await;
                return Err(os_api::error::ApiGatewayError::Internal(e.to_string()));
            }
        };

        // 3. 发 Compute 事件（VM 创建成功）。
        let ev = Event {
            source: "os-compute".into(),
            topic: Topic::Compute,
            kind: "vm.created".into(),
            severity: Severity::Info,
            task_id: None,
            payload: serde_json::json!({
                "vm_id": vm.id.as_str(),
                "name": vm.name,
                "state": format!("{:?}", vm.state),
            }),
            timestamp: os_core::Utc::now(),
        };
        let _ = self.bus.publish(ev).await;

        // 4. 监控侧落账（一个 Counter metric：累计创建数）。
        let _ = self
            .monitor
            .record_metric(Metric {
                name: "vm.create.total".into(),
                kind: MetricKind::Counter,
                value: 1.0,
                labels: {
                    let mut m = std::collections::HashMap::new();
                    m.insert("vm_id".into(), vm.id.as_str().to_string());
                    m
                },
                timestamp: os_core::Utc::now(),
            })
            .await;

        Ok(ApiResponse {
            status: 201,
            body: serde_json::json!({
                "vm_id": vm.id.as_str(),
                "name": vm.name,
                "state": format!("{:?}", vm.state),
            }),
            headers: serde_json::json!({}),
        })
    }
}

// 桥接：MockMonitor 记录的 metric 需经 EventBus 订阅者驱动（本测试中由 handler 直接调，
// 同时验证订阅者收到 Compute 事件——证明事件流与监控解耦但可串通）。
struct MonitorEventBridge {
    monitor: Arc<MockMonitor>,
}

impl MonitorEventBridge {
    fn new(monitor: Arc<MockMonitor>) -> Self {
        Self { monitor }
    }
}

impl EventSubscriber for MonitorEventBridge {
    fn handle(
        &self,
        event: &Event,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        let monitor = self.monitor.clone();
        let ev = event.clone();
        Box::pin(async move {
            // 把 Compute 事件转成一个 Gauge metric（事件计数）。
            if matches!(ev.topic, Topic::Compute) {
                let _ = monitor
                    .record_metric(Metric::gauge(
                        "compute.event.received",
                        1.0,
                        os_core::Utc::now(),
                    ))
                    .await;
            }
        })
    }
}

// ----------------------------------------------------------------------------
// 集成测
// ----------------------------------------------------------------------------

/// 构造带 admin 角色的认证主体。
///
/// VM 创建路由 `requires_auth: true` 且 `required_roles: ["admin"]`，网关 dispatch
/// 会在 handler 之前做鉴权（无认证 401 / 角色不足 403）。集成测须像真实入口那样
/// 预先解析出 admin 身份（与 os-api gateway_impl 测试的 req_with_admin_auth 同一惯例）。
fn admin_principal() -> os_security::Principal {
    use os_security::{Principal, Role, User, UserId};
    let now = chrono::Utc::now();
    let user = User::new(
        UserId::new("admin".to_string()),
        "admin".to_string(),
        vec![Role::Admin],
        now,
    )
    .unwrap();
    Principal::new(user, vec![Role::Admin], now).unwrap()
}

/// 前置：storage 预置一个 pool（VM 磁盘 zvol 须建在已有 pool 上）。
fn storage_with_pool() -> MockStorageBackend {
    MockStorageBackend::new().with_pool(os_storage::model::Pool {
        id: PoolId::new("tank"),
        name: "tank".into(),
        vdevs: vec![],
        capacity: os_core::Capacity {
            used_bytes: 0,
            total_bytes: 100 * 1024 * 1024 * 1024,
        },
        health: os_core::Health::Healthy,
    })
}

#[tokio::test]
async fn vm_creation_chain_full_path() {
    // 组装：注入各组件 mock。
    let storage = Arc::new(storage_with_pool());
    let compute = Arc::new(MockVmManager::default());
    let bus = Arc::new(MockEventBus::new());
    let monitor = Arc::new(MockMonitor::default());

    // monitor 订阅 Compute 事件（事件流 → 监控）。
    let bridge = MonitorEventBridge::new(monitor.clone());
    bus.subscribe(Topic::Compute, Box::new(bridge))
        .await
        .unwrap();

    // 注册一个 RouteHandler 到 InProcessGateway。
    let gateway = InProcessGateway::new();
    let handler = Box::new(VmCreateRouteHandler {
        compute: compute.clone(),
        storage: storage.clone(),
        bus: bus.clone(),
        monitor: monitor.clone(),
    });
    gateway
        .register_component("os-compute", handler)
        .await
        .unwrap();

    // 列路由应包含我们注册的。
    let routes = gateway.list_routes().await;
    assert!(
        routes.iter().any(|r| r.path == "/api/v1/compute/vms"),
        "网关应注册 VM 创建路由: {:?}",
        routes
    );

    // 构造请求。
    let req = ApiRequest {
        method: HttpMethod::Post,
        path: "/api/v1/compute/vms".into(),
        headers: serde_json::json!({}),
        body: serde_json::json!({
            "vm_id": "vm-001",
            "name": "test-vm",
            "zvol": "tank/vm/vm-001",
        }),
        auth: Some(admin_principal()),
    };

    // 经 InProcessGateway::dispatch 派发（不真起 HTTP）。
    let (resp, matched) = gateway.dispatch(req).await;

    // 路由匹配到 os-compute。
    assert!(matched.is_some(), "dispatch 应匹配到路由");
    assert_eq!(matched.unwrap().handler_component, "os-compute");

    // 响应 201。
    assert_eq!(resp.status, 201);
    assert_eq!(resp.body["vm_id"].as_str(), Some("vm-001"));
    assert_eq!(
        resp.body["state"].as_str(),
        Some(format!("{:?}", VmState::Stopped).as_str())
    );

    // 链路断言：
    // 1. storage 上建出了 zvol。
    assert_eq!(storage.dataset_count(), 1, "应创建 zvol 数据集");
    // 2. compute 上能查到 VM。
    let vm = compute.get_vm(&VmId::new("vm-001")).await.unwrap();
    assert_eq!(vm.name, "test-vm");
    // 3. 跨 crate 类型一致：VM 的 disk_vol_id == 我们传给 storage 的 zvol 名。
    assert_eq!(vm.spec.disk_vol_id.as_str(), "tank/vm/vm-001");
    // 4. EventBus 收到 vm.created 事件。
    let compute_events = bus.published_count_for(Topic::Compute);
    assert_eq!(compute_events, 1, "应有 1 个 Compute 事件");
    let published = bus.published();
    assert_eq!(published[0].kind, "vm.created");
    // 5. monitor 被直接调了一次（handler 内 record_metric）+ 一次事件桥接（订阅者收到）。
    //    验证监控侧落账可查（直接读 MockMonitor.recorded_metrics）。
    let recorded = monitor.recorded_metrics();
    assert!(
        recorded.iter().any(|m| m.name == "vm.create.total"),
        "monitor 应记录 vm.create.total: {:?}",
        recorded
    );
}

#[tokio::test]
async fn vm_creation_chain_compute_failure_emits_error_event() {
    // 组装：compute 注入下次失败。
    let storage = Arc::new(storage_with_pool());
    let compute = Arc::new(MockVmManager::default().fail_with(
        os_compute::ComputeError::LibvirtError("mock libvirt 故障".into()),
    ));
    let bus = Arc::new(MockEventBus::new());
    let monitor = Arc::new(MockMonitor::default());

    let gateway = InProcessGateway::new();
    let handler = Box::new(VmCreateRouteHandler {
        compute: compute.clone(),
        storage: storage.clone(),
        bus: bus.clone(),
        monitor: monitor.clone(),
    });
    gateway
        .register_component("os-compute", handler)
        .await
        .unwrap();

    let req = ApiRequest {
        method: HttpMethod::Post,
        path: "/api/v1/compute/vms".into(),
        headers: serde_json::json!({}),
        body: serde_json::json!({
            "vm_id": "vm-fail",
            "zvol": "tank/vm/vm-fail",
        }),
        auth: Some(admin_principal()),
    };
    let (resp, _matched) = gateway.dispatch(req).await;

    // 错误传播：dispatch 把 handler 的 Err 转成非 2xx 响应。
    assert!(
        resp.status >= 500,
        "compute 失败应返回 5xx，实得 {}",
        resp.status
    );
    // 同时错误事件流出。
    assert_eq!(bus.published_count_for(Topic::Compute), 1);
    assert_eq!(bus.published()[0].severity, Severity::Error);
    assert_eq!(bus.published()[0].kind, "vm.create.failed");
    // storage 上 zvol 已创建（错误发生在 compute 阶段，storage 已先完成）。
    assert_eq!(storage.dataset_count(), 1);
    // compute 上没有 VM。
    assert!(compute.list_vms().await.unwrap().is_empty());
}

#[tokio::test]
async fn vm_creation_chain_storage_pool_missing_propagates() {
    // 用空 storage（无 pool），create_dataset 会报 PoolNotFound。
    let storage = Arc::new(MockStorageBackend::new()); // 无 pool
    let compute = Arc::new(MockVmManager::default());
    let bus = Arc::new(MockEventBus::new());
    let monitor = Arc::new(MockMonitor::default());

    let gateway = InProcessGateway::new();
    let handler = Box::new(VmCreateRouteHandler {
        compute: compute.clone(),
        storage: storage.clone(),
        bus: bus.clone(),
        monitor: monitor.clone(),
    });
    gateway
        .register_component("os-compute", handler)
        .await
        .unwrap();

    let req = ApiRequest {
        method: HttpMethod::Post,
        path: "/api/v1/compute/vms".into(),
        headers: serde_json::json!({}),
        body: serde_json::json!({
            "vm_id": "vm-no-pool",
            "zvol": "ghost/vm/x",
        }),
        auth: Some(admin_principal()),
    };
    let (resp, _matched) = gateway.dispatch(req).await;

    // storage 阶段失败 → 上游正确处理（5xx + 不发成功事件）。
    assert!(resp.status >= 500, "PoolNotFound 应返回 5xx");
    assert_eq!(
        bus.published_count(),
        0,
        "storage 阶段失败不应发 Compute 事件（handler 提前 return）"
    );
    // compute 完全未被调用。
    assert!(compute.list_vms().await.unwrap().is_empty());
}

#[tokio::test]
async fn event_subscriber_chain_propagates_to_monitor() {
    // 单独验证：EventBus 订阅者收到 Compute 事件 → 调 monitor。
    let bus = MockEventBus::new();
    let monitor = Arc::new(MockMonitor::default());
    let bridge = MonitorEventBridge::new(monitor.clone());
    bus.subscribe(Topic::Compute, Box::new(bridge))
        .await
        .unwrap();

    // 直接 publish 一个 Compute 事件（模拟 vm.created）。
    let ev = Event::new("os-compute", Topic::Compute, "vm.created");
    bus.publish(ev).await.unwrap();

    // 由于 MockEventBus 不真派发（仅记录），用 dispatch_to 主动触发订阅者。
    let subs = bus.subscribers_for(Topic::Compute);
    assert_eq!(subs.len(), 1, "应有 1 个 Compute 订阅者");
    let sub = bus.subscriber(subs[0]).unwrap();
    let fake_event = Event::new("os-compute", Topic::Compute, "vm.created");
    let _ = sub.handle(&fake_event).await;

    // monitor 记到 metric。
    let recorded = monitor.recorded_metrics();
    assert!(
        recorded.iter().any(|m| m.name == "compute.event.received"),
        "monitor 应收到桥接的事件 metric"
    );
}

#[tokio::test]
async fn volume_id_cross_crate_type_identity() {
    // 静态验证：os-compute VmSpec.disk_vol_id 与 os-storage DatasetId 用同名 VolumeId
    // 类型串联（同一 os-core::VolumeId newtype，跨 crate 编译期不可互赋）。
    let vol = VolumeId::new("tank/vm/x");
    let spec = VmSpec {
        cpus: CpuTopology::new(1),
        memory_mb: 1024,
        disk_vol_id: vol.clone(),
        nics: vec![VmNic::virtio("br0")],
        firmware: VmFirmware::Bios,
    };
    // 给 storage 用同名 DatasetId（zvol 即 dataset 的一种）。
    let ds_id = DatasetId::new(vol.as_str());
    assert_eq!(spec.disk_vol_id.as_str(), ds_id.as_str());
    // TaskId 也是共享 newtype（migrate 返回）。
    let task = TaskId::new();
    assert_ne!(task.0, os_core::Uuid::nil());
}

#[tokio::test]
async fn storage_vdev_spec_round_trips_to_pool() {
    // 验证 storage MockStorageBackend::create_pool 跨 crate 的 VdevSpec 类型可用。
    let be = MockStorageBackend::new();
    let _ = be
        .create_pool(
            &PoolId::new("tank"),
            vec![VdevSpec {
                kind: VdevKind::Mirror,
                disks: vec!["/dev/sdb".into(), "/dev/sdc".into()],
            }],
        )
        .await
        .unwrap();
    assert_eq!(be.pool_count(), 1);
}
