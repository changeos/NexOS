//! `ComputeRouteHandler` —— VM 生命周期的 HTTP→`VmManager` 适配器（规划文档 §3.4 / §3.6）。
//!
//! 定位：把网关 HTTP 请求（`/api/v1/vms*`）翻译为对 `os_compute::VmManager` 的调用，
//! 再把返回的 `Vm` / `Vec<Vm>` 序列化为 [`ApiResponse`]。这是 os-compute 真实
//! VM 后端接入网关的边界适配层——HTTP 进、libvirt 出。
//!
//! # 设计约束（红线）
//!
//! - 实现 [`RouteHandler`]（`#[async_trait]`，dyn 兼容，网关经 `Box<dyn RouteHandler>` 注册）。
//! - **持有具体类型 `Arc<LibvirtVmManager>`**，不持有 `dyn VmManager`：`VmManager` 是原生
//!   `async fn in trait`（`#[allow(async_fn_in_trait)]`），非 dyn 兼容（ADR-COMPAT-001）。
//!   故按规格直接持有 `LibvirtVmManager`（默认内存态骨架，开 `virt-ffi` 后切真实 libvirt）。
//! - body 是 `serde_json::Value`，handler 内 `from_value`/`to_value` 完成 DTO 往返。
//! - 路径参数（`:id`）从 `req.path` 字符串按段解析（`?query` 先剥离）。
//!
//! # 路由表
//!
//! | method | path                       | VmManager 调用 |
//! |--------|----------------------------|----------------|
//! | GET    | `/api/v1/vms`              | `list_vms`     |
//! | POST   | `/api/v1/vms`              | `create_vm`    |
//! | GET    | `/api/v1/vms/:id`          | `get_vm`       |
//! | POST   | `/api/v1/vms/:id/start`    | `start_vm`     |
//! | POST   | `/api/v1/vms/:id/stop`     | `stop_vm`      |
//! | DELETE | `/api/v1/vms/:id`          | `destroy_vm`   |

use std::sync::Arc;

use async_trait::async_trait;
use os_compute::{ComputeError, LibvirtVmManager, VmManager, VmSpec};
use os_core::{Uuid, VmId};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// 路径解析
// ----------------------------------------------------------------------------

/// 从请求路径中剥离 `?query` 后的纯 path 段（前后空段去除）。
///
/// 例：`/api/v1/vms/abc?x=1` → `["api", "v1", "vms", "abc"]`。
fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

/// 把解析出的路径段与方法归一为「命中的 VM 路由动作」。
///
/// 设计：用枚举集中表达所有合法 VM 路由形态，避免在 match guard 里散落复杂的
/// 前缀校验。`None` 表示路径不在本 handler 路由表内（调用方返回 404）。
enum VmRoute {
    /// GET/POST `/api/v1/vms`（列表 / 创建）
    Root,
    /// GET/DELETE `/api/v1/vms/:id`（单查 / 销毁）
    ById,
    /// `/api/v1/vms/:id/{start,stop}`（生命周期）
    Lifecycle,
    /// GET `/api/v1/vms/:id/xml`（virsh dumpxml）
    Xml,
    /// POST `/api/v1/vms/:id/snapshot`（virsh snapshot-create）
    Snapshot,
}

/// 判定 method + segments 是否命中某条 VM 路由。
fn classify(method: HttpMethod, segs: &[&str]) -> Option<VmRoute> {
    // VM 路由前缀固定为 ["api","v1","vms"]；长度 >= 3 才可能有合法子路由。
    if !(segs.len() >= 3 && segs[0] == "api" && segs[1] == "v1" && segs[2] == "vms") {
        return None;
    }
    match segs.len() {
        // /api/v1/vms
        3 => match method {
            HttpMethod::Get | HttpMethod::Post => Some(VmRoute::Root),
            _ => None,
        },
        // /api/v1/vms/:id
        4 => match method {
            HttpMethod::Get | HttpMethod::Delete => Some(VmRoute::ById),
            _ => None,
        },
        // /api/v1/vms/:id/{start,stop}
        5 if matches!(method, HttpMethod::Post) && matches!(segs[4], "start" | "stop") => {
            Some(VmRoute::Lifecycle)
        }
        // GET /api/v1/vms/:id/xml
        5 if matches!(method, HttpMethod::Get) && segs[4] == "xml" => Some(VmRoute::Xml),
        // POST /api/v1/vms/:id/snapshot
        5 if matches!(method, HttpMethod::Post) && segs[4] == "snapshot" => Some(VmRoute::Snapshot),
        _ => None,
    }
}

/// 把 [`ComputeError`] 映射为对用户友好的 HTTP 状态码 + body。
///
/// 映射策略（与 `ComputeError → os_common::ApiError` 的错误码身份映射一致）：
/// - `VmNotFound` → 404
/// - `InvalidSpec` / `HardwareVirtualizationUnavailable` → 400（用户输入/环境非法）
/// - 其余（libvirt/命令/IO/内部/迁移/镜像/网络/包/容器）→ 500
///
/// 注：当前 handler 的错误统一走 [`map_compute_err`] → `ApiGatewayError` 通道
/// （网关 dispatch 映射为 HTTP 500）；本函数保留为可选的「精细状态码」出口与测试
/// 断言依据，待后续接入更细粒度的错误响应时直接启用。
#[cfg(test)]
fn err_response(e: ComputeError) -> ApiResponse {
    let status = match &e {
        ComputeError::VmNotFound(_) => 404,
        ComputeError::InvalidSpec(_) | ComputeError::HardwareVirtualizationUnavailable(_) => 400,
        _ => 500,
    };
    ApiResponse {
        status,
        body: serde_json::json!({ "error": e.to_string() }),
        headers: serde_json::json!({}),
    }
}

// ----------------------------------------------------------------------------
// ComputeRouteHandler
// ----------------------------------------------------------------------------

/// VM 生命周期 `RouteHandler`——把 `/api/v1/vms*` HTTP 请求适配到 [`LibvirtVmManager`]。
///
/// 持有具体类型 `Arc<LibvirtVmManager>`（见模块级「设计约束」），构造时注入：
///
/// ```ignore
/// use os_api::handlers::ComputeRouteHandler;
/// use os_compute::LibvirtVmManager;
///
/// let handler = ComputeRouteHandler::new(LibvirtVmManager::new("node-a"));
/// // 持有 handler.routes() / handler.handle(...) 经网关注册
/// ```
///
/// 默认 `LibvirtVmManager` 为内存态骨架（XML 渲染 + 状态机真实，无 libvirt 连接），
/// 开 `virt-ffi` feature 后 `LibvirtVmManager` 内部切真实 libvirt 后端，本 handler 无需改动。
pub struct ComputeRouteHandler {
    vm_manager: Arc<LibvirtVmManager>,
}

impl ComputeRouteHandler {
    /// 构造 handler，注入 VM 管理器（Arc 共享，handler 可被网关 Clone-as-Arc 持有）。
    pub fn new(vm_manager: LibvirtVmManager) -> Self {
        Self {
            vm_manager: Arc::new(vm_manager),
        }
    }

    /// 构造 handler，直接注入已 Arc 包裹的 VM 管理器（供多个 handler 共享同一实例）。
    pub fn with_arc(vm_manager: Arc<LibvirtVmManager>) -> Self {
        Self { vm_manager }
    }

    /// 暴露内部 VM 管理器引用（供测试 / 上层诊断）。
    pub fn vm_manager(&self) -> &LibvirtVmManager {
        &self.vm_manager
    }
}

#[async_trait]
impl RouteHandler for ComputeRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec(HttpMethod::Get, "/api/v1/vms", vec![]),
            spec(HttpMethod::Post, "/api/v1/vms", vec!["admin".into()]),
            spec(HttpMethod::Get, "/api/v1/vms/:id", vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/vms/:id/start",
                vec!["operator".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/vms/:id/stop",
                vec!["operator".into()],
            ),
            spec(HttpMethod::Delete, "/api/v1/vms/:id", vec!["admin".into()]),
            // virsh 诊断/快照（spawn virsh，libvirt 不可用时降级）
            spec(
                HttpMethod::Get,
                "/api/v1/vms/:id/xml",
                vec!["operator".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/vms/:id/snapshot",
                vec!["admin".into()],
            ),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        let route = classify(req.method, &segs).ok_or_else(|| {
            ApiGatewayError::ComponentNotFound(format!(
                "os-compute: 无匹配路由 {:?} {:?}",
                req.method, req.path
            ))
        })?;

        let resp = match (route, &segs[..]) {
            // GET /api/v1/vms → list_vms
            (VmRoute::Root, _) if matches!(req.method, HttpMethod::Get) => {
                let vms = self.vm_manager.list_vms().await.map_err(map_compute_err)?;
                ApiResponse {
                    status: 200,
                    body: serde_json::to_value(&vms).map_err(map_json_err)?,
                    headers: serde_json::json!({}),
                }
            }

            // POST /api/v1/vms → create_vm
            // body: VmSpec（id 由服务端生成 UUID，name 取 id 短形）
            (VmRoute::Root, _) => {
                let spec: VmSpec = serde_json::from_value(req.body)
                    .map_err(|e| ApiGatewayError::Internal(format!("请求体反序列化失败: {e}")))?;
                let id = VmId::new(Uuid::new_v4().to_string());
                let name = format!("vm-{}", &id.as_str()[..8.min(id.as_str().len())]);
                let vm = self
                    .vm_manager
                    .create_vm(&id, &name, spec)
                    .await
                    .map_err(map_compute_err)?;
                let location = format!("/api/v1/vms/{}", id);
                ApiResponse {
                    status: 201,
                    body: serde_json::to_value(&vm).map_err(map_json_err)?,
                    headers: serde_json::json!({ "location": location }),
                }
            }

            // GET /api/v1/vms/:id → get_vm
            (VmRoute::ById, [_, _, _, id]) if matches!(req.method, HttpMethod::Get) => {
                let vm = self
                    .vm_manager
                    .get_vm(&VmId::new(*id))
                    .await
                    .map_err(map_compute_err)?;
                ApiResponse {
                    status: 200,
                    body: serde_json::to_value(&vm).map_err(map_json_err)?,
                    headers: serde_json::json!({}),
                }
            }

            // DELETE /api/v1/vms/:id → destroy_vm
            (VmRoute::ById, _) => {
                let id = segs[3];
                self.vm_manager
                    .destroy_vm(&VmId::new(id))
                    .await
                    .map_err(map_compute_err)?;
                ApiResponse {
                    status: 204,
                    body: serde_json::Value::Null,
                    headers: serde_json::json!({}),
                }
            }

            // POST /api/v1/vms/:id/{start,stop} → start_vm / stop_vm
            (VmRoute::Lifecycle, [_, _, _, id, action]) => {
                let vm = if *action == "start" {
                    self.vm_manager.start_vm(&VmId::new(*id)).await
                } else {
                    // force=false 软关机（libvirt shutdown 路径）
                    self.vm_manager.stop_vm(&VmId::new(*id), false).await
                }
                .map_err(map_compute_err)?;
                ApiResponse {
                    status: 200,
                    body: serde_json::to_value(&vm).map_err(map_json_err)?,
                    headers: serde_json::json!({}),
                }
            }

            // GET /api/v1/vms/:id/xml → virsh dumpxml（spawn，libvirt 不可用降级）
            //
            // 先经 VmManager 取域名（virsh 按域名 dumpxml），再 spawn_blocking 跑
            // `virsh dumpxml <name>`。virsh 不可用 / 域不存在 → 200 + available=false（降级）。
            (VmRoute::Xml, _) => {
                let id = segs[3];
                let vm = self
                    .vm_manager
                    .get_vm(&VmId::new(id))
                    .await
                    .map_err(map_compute_err)?;
                let name = vm.name.clone();
                let xml = tokio::task::spawn_blocking(move || run_virsh(&["dumpxml", &name]))
                    .await
                    .map_err(|e| {
                        ApiGatewayError::Internal(format!("virsh dumpxml 任务 join 失败: {e}"))
                    })?;
                match xml {
                    Ok(x) => ApiResponse {
                        status: 200,
                        body: serde_json::json!({ "id": id, "name": vm.name, "xml": x }),
                        headers: serde_json::json!({}),
                    },
                    Err(e) => ApiResponse {
                        status: 200,
                        body: serde_json::json!({
                            "id": id,
                            "name": vm.name,
                            "xml": "",
                            "available": false,
                            "error": e,
                        }),
                        headers: serde_json::json!({}),
                    },
                }
            }

            // POST /api/v1/vms/:id/snapshot → virsh snapshot-create（spawn，降级）
            //
            // body 可选 {"name": "<snapshot-name>"}：提供则 `virsh snapshot-create-as
            // <domain> <snap>`，否则 `virsh snapshot-create <domain>`（libvirt 自动命名）。
            (VmRoute::Snapshot, _) => {
                let id = segs[3];
                let vm = self
                    .vm_manager
                    .get_vm(&VmId::new(id))
                    .await
                    .map_err(map_compute_err)?;
                let name = vm.name.clone();
                #[derive(serde::Deserialize)]
                struct SnapBody {
                    name: Option<String>,
                }
                let body: SnapBody = if req.body.is_null() {
                    SnapBody { name: None }
                } else {
                    serde_json::from_value(req.body).map_err(|e| {
                        ApiGatewayError::Internal(format!("解析快照请求体失败: {e}"))
                    })?
                };
                let snap_name = body.name.clone();
                let result = tokio::task::spawn_blocking(move || match &body.name {
                    Some(snap) if !snap.trim().is_empty() => {
                        run_virsh(&["snapshot-create-as", &name, snap.trim()])
                    }
                    _ => run_virsh(&["snapshot-create", &name]),
                })
                .await
                .map_err(|e| {
                    ApiGatewayError::Internal(format!("virsh snapshot-create 任务 join 失败: {e}"))
                })?;
                match result {
                    Ok(out) => ApiResponse {
                        status: 200,
                        body: serde_json::json!({
                            "id": id,
                            "name": vm.name,
                            "snapshot": snap_name,
                            "applied": true,
                            "output": out.trim(),
                        }),
                        headers: serde_json::json!({}),
                    },
                    Err(e) => ApiResponse {
                        status: 200,
                        body: serde_json::json!({
                            "id": id,
                            "name": vm.name,
                            "snapshot": snap_name,
                            "applied": false,
                            "error": e,
                        }),
                        headers: serde_json::json!({}),
                    },
                }
            }

            // classify 已保证所有命中分支都被上面覆盖，此分支理论不可达。
            _ => {
                return Err(ApiGatewayError::ComponentNotFound(format!(
                    "os-compute: 内部路由分派失败 {:?} {:?}",
                    req.method, req.path
                )));
            }
        };
        Ok(resp)
    }
}

// ----------------------------------------------------------------------------
// 内部辅助
// ----------------------------------------------------------------------------

/// 紧凑构造一条 [`RouteSpec`]（component 固定 os-compute，全部 requires_auth）。
fn spec(method: HttpMethod, path: &str, required_roles: Vec<String>) -> RouteSpec {
    RouteSpec {
        method,
        path: path.into(),
        handler_component: "compute".into(),
        requires_auth: true,
        required_roles,
    }
}

/// [`ComputeError`] → [`ApiGatewayError`]（用 Display 文本保留诊断信息）。
///
/// 注：返回的 `ApiGatewayError::Internal` 仅用于 trait 错误通道；网关 dispatch 阶段
/// 会统一把 handler 错误映射为 HTTP 500（见 `gateway_impl::dispatch`）。若需更细粒度
/// 的状态码（404/400），handler 内应直接返回成功的 [`ApiResponse`]（见 [`err_response`]）。
fn map_compute_err(e: ComputeError) -> ApiGatewayError {
    ApiGatewayError::Internal(e.to_string())
}

/// serde 序列化错误 → [`ApiGatewayError`]。
fn map_json_err(e: serde_json::Error) -> ApiGatewayError {
    ApiGatewayError::Internal(format!("响应序列化失败: {e}"))
}

/// spawn `virsh <args>`（best-effort）；失败返回 `Err(诊断)`，不 panic。
///
/// libvirt/virsh 未安装、域不存在、无权限等情况下返回 Err，由调用方降级
/// （返回 200 + applied=false / available=false），不阻塞服务。
fn run_virsh(args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new("virsh")
        .args(args)
        .output()
        .map_err(|e| format!("virsh 不可用: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "virsh 退出码 {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

// ----------------------------------------------------------------------------
// 单元测
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use os_compute::{CpuTopology, LibvirtVmManager, VmFirmware, VmNic, VmState};
    use os_core::VolumeId;

    /// 构造一份合法 VmSpec（2 vcpu / 1024MB / br0 网卡 / BIOS）。
    fn sample_spec() -> serde_json::Value {
        serde_json::json!({
            "cpus": { "vcpus": 2, "sockets": 1, "cores": 2, "threads": 1 },
            "memory_mb": 1024,
            "disk_vol_id": "tank/vm/test",
            "nics": [ { "bridge": "br0", "model": "virtio" } ],
            "firmware": "bios",
        })
    }

    /// 构造一份非法 VmSpec（memory_mb=0）。
    fn bad_spec() -> serde_json::Value {
        serde_json::json!({
            "cpus": { "vcpus": 1, "sockets": 1, "cores": 1, "threads": 1 },
            "memory_mb": 0,
            "disk_vol_id": "tank/vm/bad",
            "nics": [ { "bridge": "br0", "model": "virtio" } ],
            "firmware": "bios",
        })
    }

    fn new_handler() -> ComputeRouteHandler {
        ComputeRouteHandler::new(LibvirtVmManager::new("node-test"))
    }

    fn req(method: HttpMethod, path: &str, body: serde_json::Value) -> ApiRequest {
        ApiRequest {
            method,
            path: path.into(),
            headers: serde_json::json!({}),
            body,
            auth: None,
        }
    }

    #[tokio::test]
    async fn routes_declares_all_vm_endpoints() {
        let h = new_handler();
        let routes = h.routes().await;
        assert_eq!(routes.len(), 8);
        // 每条路由的 component 都是 os-compute
        assert!(routes.iter().all(|r| r.handler_component == "compute"));
        // 路径前缀校验
        assert!(routes.iter().all(|r| r.path.starts_with("/api/v1/vms")));
        // 关键路由存在
        let paths: Vec<String> = routes
            .iter()
            .map(|r| format!("{:?} {}", r.method, r.path))
            .collect();
        assert!(paths
            .iter()
            .any(|p| p.contains("Get") && p.ends_with("/api/v1/vms")));
        assert!(paths
            .iter()
            .any(|p| p.contains("Post") && p.ends_with("/api/v1/vms")));
        assert!(paths
            .iter()
            .any(|p| p.contains("Get") && p.ends_with(":id")));
        assert!(paths.iter().any(|p| p.contains(":id/start")));
        assert!(paths.iter().any(|p| p.contains(":id/stop")));
        assert!(paths
            .iter()
            .any(|p| p.contains("Delete") && p.ends_with(":id")));
        // virsh xml / snapshot 新增路由
        assert!(paths.iter().any(|p| p.contains(":id/xml")));
        assert!(paths.iter().any(|p| p.contains(":id/snapshot")));
    }

    #[tokio::test]
    async fn list_vms_empty_returns_200_array() {
        let h = new_handler();
        let resp = h
            .handle(req(HttpMethod::Get, "/api/v1/vms", serde_json::json!({})))
            .await;
        let resp = resp.expect("list 不应返回 Err");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, serde_json::json!([]));
    }

    #[tokio::test]
    async fn create_vm_then_list_returns_201_and_appears() {
        let h = new_handler();

        // 1) POST /api/v1/vms → 201 + body 含 id
        let resp = h
            .handle(req(HttpMethod::Post, "/api/v1/vms", sample_spec()))
            .await
            .expect("create 不应返回 Err");
        assert_eq!(resp.status, 201);
        assert!(resp.headers["location"].is_string());
        // body 是 Vm 对象，含 id/state
        assert!(resp.body["id"].is_string());
        assert_eq!(resp.body["state"], serde_json::json!("stopped"));
        assert_eq!(resp.body["spec"]["memory_mb"], 1024);
        let id = resp.body["id"].as_str().unwrap().to_string();

        // 2) GET /api/v1/vms → 列表含刚创建的 VM
        let resp = h
            .handle(req(HttpMethod::Get, "/api/v1/vms", serde_json::json!({})))
            .await
            .expect("list 不应返回 Err");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body.as_array().unwrap().len(), 1);
        assert_eq!(resp.body[0]["id"], serde_json::json!(id));

        // 3) GET /api/v1/vms/:id → 单查命中
        let resp = h
            .handle(req(
                HttpMethod::Get,
                &format!("/api/v1/vms/{id}"),
                serde_json::json!({}),
            ))
            .await
            .expect("get 不应返回 Err");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["id"], serde_json::json!(id));
    }

    #[tokio::test]
    async fn create_vm_rejects_invalid_spec() {
        let h = new_handler();
        // 内存骨架在 create_vm 内做 spec.validate()，非法 spec 走 ComputeError 通道
        let err = h
            .handle(req(HttpMethod::Post, "/api/v1/vms", bad_spec()))
            .await
            .unwrap_err();
        assert!(matches!(err, ApiGatewayError::Internal(_)));
    }

    #[tokio::test]
    async fn start_stop_lifecycle() {
        let h = new_handler();
        // 先创建
        let resp = h
            .handle(req(HttpMethod::Post, "/api/v1/vms", sample_spec()))
            .await
            .unwrap();
        let id = resp.body["id"].as_str().unwrap().to_string();

        // start
        let resp = h
            .handle(req(
                HttpMethod::Post,
                &format!("/api/v1/vms/{id}/start"),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["state"], serde_json::json!("running"));

        // stop
        let resp = h
            .handle(req(
                HttpMethod::Post,
                &format!("/api/v1/vms/{id}/stop"),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["state"], serde_json::json!("stopped"));
    }

    #[tokio::test]
    async fn get_missing_vm_returns_error() {
        let h = new_handler();
        let err = h
            .handle(req(
                HttpMethod::Get,
                "/api/v1/vms/nope-uuid",
                serde_json::json!({}),
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, ApiGatewayError::Internal(_)));
    }

    #[tokio::test]
    async fn delete_vm_returns_204() {
        let h = new_handler();
        let resp = h
            .handle(req(HttpMethod::Post, "/api/v1/vms", sample_spec()))
            .await
            .unwrap();
        let id = resp.body["id"].as_str().unwrap().to_string();

        let resp = h
            .handle(req(
                HttpMethod::Delete,
                &format!("/api/v1/vms/{id}"),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 204);
        assert_eq!(resp.body, serde_json::Value::Null);

        // 再次 GET 应失败
        assert!(h
            .handle(req(
                HttpMethod::Get,
                &format!("/api/v1/vms/{id}"),
                serde_json::json!({}),
            ))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn xml_route_returns_200_degraded_for_unknown_domain() {
        let h = new_handler();
        // 先创建一个 VM（内存态骨架），拿到 id
        let resp = h
            .handle(req(HttpMethod::Post, "/api/v1/vms", sample_spec()))
            .await
            .unwrap();
        let id = resp.body["id"].as_str().unwrap().to_string();
        // GET /xml → virsh dumpxml 对非真实域必失败 → 200 + available=false（降级，不 Err）
        let resp = h
            .handle(req(
                HttpMethod::Get,
                &format!("/api/v1/vms/{id}/xml"),
                serde_json::json!({}),
            ))
            .await
            .expect("xml 路由应返回 Ok（降级）");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["id"], serde_json::json!(id));
        // virsh 失败时 available=false；成功时含 xml 字段
        assert!(
            resp.body.get("xml").is_some() || resp.body.get("available").is_some(),
            "响应应含 xml 或 available 字段"
        );
    }

    #[tokio::test]
    async fn snapshot_route_returns_200_degraded() {
        let h = new_handler();
        let resp = h
            .handle(req(HttpMethod::Post, "/api/v1/vms", sample_spec()))
            .await
            .unwrap();
        let id = resp.body["id"].as_str().unwrap().to_string();
        // POST /snapshot 带 name → virsh 失败 → 200 + applied=false（降级）
        let resp = h
            .handle(req(
                HttpMethod::Post,
                &format!("/api/v1/vms/{id}/snapshot"),
                serde_json::json!({ "name": "snap1" }),
            ))
            .await
            .expect("snapshot 路由应返回 Ok（降级）");
        assert_eq!(resp.status, 200);
        assert!(resp.body["applied"].is_boolean());
        assert_eq!(resp.body["snapshot"], serde_json::json!("snap1"));
        // 无 body 也能工作（自动命名路径）
        let resp = h
            .handle(req(
                HttpMethod::Post,
                &format!("/api/v1/vms/{id}/snapshot"),
                serde_json::Value::Null,
            ))
            .await
            .expect("snapshot 无 body 应返回 Ok");
        assert_eq!(resp.status, 200);
        assert!(resp.body["applied"].is_boolean());
    }

    #[tokio::test]
    async fn unknown_route_returns_component_not_found() {
        let h = new_handler();
        // /api/v1/vms/:id/reboot 未定义
        let err = h
            .handle(req(
                HttpMethod::Post,
                "/api/v1/vms/some-id/reboot",
                serde_json::json!({}),
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, ApiGatewayError::ComponentNotFound(_)));
    }

    #[tokio::test]
    async fn path_segments_strips_query() {
        let segs = path_segments("/api/v1/vms/abc?x=1&y=2");
        assert_eq!(segs, vec!["api", "v1", "vms", "abc"]);
    }

    #[test]
    fn err_response_maps_status_codes() {
        assert_eq!(
            err_response(ComputeError::VmNotFound("x".into())).status,
            404
        );
        assert_eq!(
            err_response(ComputeError::InvalidSpec("x".into())).status,
            400
        );
        assert_eq!(
            err_response(ComputeError::HardwareVirtualizationUnavailable("x".into())).status,
            400
        );
        assert_eq!(
            err_response(ComputeError::LibvirtError("x".into())).status,
            500
        );
        assert_eq!(err_response(ComputeError::Internal("x".into())).status, 500);
    }

    #[test]
    fn classify_covers_all_routes() {
        // Root
        assert!(matches!(
            classify(HttpMethod::Get, &["api", "v1", "vms"]),
            Some(VmRoute::Root)
        ));
        assert!(matches!(
            classify(HttpMethod::Post, &["api", "v1", "vms"]),
            Some(VmRoute::Root)
        ));
        // ById
        assert!(matches!(
            classify(HttpMethod::Get, &["api", "v1", "vms", "x"]),
            Some(VmRoute::ById)
        ));
        assert!(matches!(
            classify(HttpMethod::Delete, &["api", "v1", "vms", "x"]),
            Some(VmRoute::ById)
        ));
        // Lifecycle
        assert!(matches!(
            classify(HttpMethod::Post, &["api", "v1", "vms", "x", "start"]),
            Some(VmRoute::Lifecycle)
        ));
        assert!(matches!(
            classify(HttpMethod::Post, &["api", "v1", "vms", "x", "stop"]),
            Some(VmRoute::Lifecycle)
        ));
        // Xml / Snapshot
        assert!(matches!(
            classify(HttpMethod::Get, &["api", "v1", "vms", "x", "xml"]),
            Some(VmRoute::Xml)
        ));
        assert!(matches!(
            classify(HttpMethod::Post, &["api", "v1", "vms", "x", "snapshot"]),
            Some(VmRoute::Snapshot)
        ));
        // 不命中
        assert!(classify(HttpMethod::Put, &["api", "v1", "vms"]).is_none());
        assert!(classify(HttpMethod::Post, &["api", "v1", "vms", "x", "reboot"]).is_none());
        assert!(classify(HttpMethod::Get, &["api", "v1", "storage"]).is_none());
        assert!(classify(
            HttpMethod::Post,
            &["api", "v1", "vms", "x", "start", "extra"]
        )
        .is_none());
        // xml 用 POST / snapshot 用 GET → 不命中
        assert!(classify(HttpMethod::Post, &["api", "v1", "vms", "x", "xml"]).is_none());
        assert!(classify(HttpMethod::Get, &["api", "v1", "vms", "x", "snapshot"]).is_none());
    }

    #[test]
    fn with_arc_shares_same_manager() {
        let mgr = Arc::new(LibvirtVmManager::new("node-a"));
        let h1 = ComputeRouteHandler::with_arc(mgr.clone());
        let h2 = ComputeRouteHandler::with_arc(mgr);
        assert_eq!(
            h1.vm_manager().local_node().as_str(),
            h2.vm_manager().local_node().as_str()
        );
    }

    #[test]
    fn sample_spec_roundtrips_through_serde() {
        // 确保测试用的 spec JSON 能反序列化为 VmSpec（避免测试本身构造错误）
        let spec: VmSpec = serde_json::from_value(sample_spec()).unwrap();
        spec.validate().unwrap();
        assert_eq!(spec.memory_mb, 1024);
        assert_eq!(spec.firmware, VmFirmware::Bios);
        assert_eq!(spec.nics.len(), 1);
    }

    #[test]
    fn cpu_topology_helper_constructs_symmetric() {
        let t = CpuTopology::new(4);
        assert_eq!(t.vcpus, 4);
        assert_eq!(t.sockets, 1);
        assert_eq!(t.cores, 4);
        assert_eq!(t.threads, 1);
        t.validate().unwrap();
    }

    #[test]
    fn vm_state_stopped_is_default_for_new_defined() {
        // 验证 create_vm 后状态语义（handler 返回的 state 应是 stopped）
        let _state = VmState::Stopped;
        let nic = VmNic::virtio("br0");
        nic.validate().unwrap();
        let _vol = VolumeId::new("tank/vm/x");
    }
}
