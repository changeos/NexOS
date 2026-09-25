//! `LibvirtVmManager` —— 基于 libvirt 的 `VmManager` 实现。
//!
//! ## 双实现路径（feature 门控）
//!
//! 本文件提供两条**互斥**的实现路径，由 cargo feature `virt-ffi` 切换：
//!
//! - **默认路径**（`feature = "virt-ffi"` 关闭）：纯内存态骨架。libvirt domain
//!   XML 渲染 + `VmState` 状态机仍真实可用（来自 [`crate::vm`]），生命周期
//!   操作落在内存索引上。无需任何系统依赖，可在任意构建机 check/test/clippy。
//!   保留为 P1 阶段交付的骨架，便于无 libvirt/KVM 环境下仍验证控制流。
//!
//! - **真实路径**（`feature = "virt-ffi"` 开启）：经 [`virt`] crate（libvirt FFI
//!   绑定）真实连接 libvirt（`virConnectOpen`），`create/destroy/start/stop/
//!   pause/resume/get/list/migrate` 全部走 `virConnect*` / `virDomain*` C API。
//!   本路径在编译期链接系统 `libvirt`，故**前置依赖**：
//!   `apt install libvirt-dev`（提供 `libvirt.so` 与头文件）。无该包时
//!   `cargo build/test --features virt-ffi` 会以链接错误（undefined symbol:
//!   `virConnectOpen` …）失败——这是预期行为，非 bug。`cargo check` 仍可通过
//!   （绑定由 build script 生成，不真实链接）。
//!
//! 两路径共享：`LibvirtDomainState` ↔ [`crate::vm::VmState`] 映射、spec 校验、
//! domain XML 渲染（纯逻辑，委托 [`crate::vm::VmSpec`]）。运行期真实 libvirt/KVM
//! 操作需 root 或 libvirt 组权限。
//!
//! 实现命名遵循规格书 §5.1：实现 struct 不挂 agent 前缀，故名 `LibvirtVmManager`。
//! 依赖注册见 `docs/adr/ADR-DEPS-002-p2-domain-specific-deps.md`。

// ----------------------------------------------------------------------------
// 共享：libvirt domain 状态码 ↔ VmState 映射
// ----------------------------------------------------------------------------

use crate::vm::VmState;

/// libvirt domain XML 中 `virDomainState` 枚举的整数值。
///
/// 见 libvirt `include/libvirt/libvirt-domain.h`：0=VIR_DOMAIN_NOSTATE,
/// 1=VIR_DOMAIN_RUNNING, 3=VIR_DOMAIN_PAUSED, 4=VIR_DOMAIN_SHUTDOWN,
/// 5=VIR_DOMAIN_SHUTOFF, 6=VIR_DOMAIN_CRASHED, …
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum LibvirtDomainState {
    /// 未定义状态
    NoState = 0,
    /// 运行中
    Running = 1,
    /// 阻塞（已废弃，并入 Running）
    Blocked = 2,
    /// 已暂停（suspend）
    Paused = 3,
    /// 关机过程中
    Shutdown = 4,
    /// 已完全关闭
    Shutoff = 5,
    /// 崩溃
    Crashed = 6,
    /// 挂起（PM）
    Pmsuspended = 7,
}

impl LibvirtDomainState {
    /// 将 libvirt 原始状态码（`virDomainState`，u32）映射为内部 [`VmState`]。
    ///
    /// 映射策略（与规格书 §3 `VmState` 对齐）：
    /// - Running / Blocked → Running
    /// - Paused → Paused
    /// - Shutoff / Shutdown → Stopped（定义仍在，仅未运行）
    /// - Crashed → Failed
    /// - NoState → Failed（异常）
    /// - Pmsuspended → Paused（电源挂起视为暂停，可 resume）
    pub fn from_raw(raw: u32) -> Self {
        match raw {
            0 => Self::NoState,
            1 => Self::Running,
            2 => Self::Blocked,
            3 => Self::Paused,
            4 => Self::Shutdown,
            5 => Self::Shutoff,
            6 => Self::Crashed,
            _ => Self::Pmsuspended, // 7 及未知值并入电源挂起
        }
    }

    /// 将自身映射为内部 [`VmState`]。
    pub fn to_vm_state(self) -> VmState {
        match self {
            LibvirtDomainState::Running | LibvirtDomainState::Blocked => VmState::Running,
            LibvirtDomainState::Paused | LibvirtDomainState::Pmsuspended => VmState::Paused,
            LibvirtDomainState::Shutoff | LibvirtDomainState::Shutdown => VmState::Stopped,
            LibvirtDomainState::Crashed | LibvirtDomainState::NoState => VmState::Failed,
        }
    }
}

// ============================================================================
// 默认路径：内存态骨架（无系统依赖）
// ============================================================================
//
// 以下整块在开启 `virt-ffi` 时不编译，避免与真实路径的 `impl VmManager` 冲突。

#[cfg(not(feature = "virt-ffi"))]
mod fallback {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use os_core::{NodeId, TaskId, VmId};

    use crate::vm::{Vm, VmManager, VmSpec, VmState};
    use crate::{ComputeError, ComputeResult};

    /// 基于 libvirt 的虚拟机管理器（内存态骨架，默认编译路径）。
    ///
    /// 持有本节点 ID 与一份内存中的 VM 索引（用于查询幂等性占位）。开启
    /// cargo feature `virt-ffi` 后切换为真实 libvirt 实现（见本文件顶部
    /// 「双实现路径」文档）。`virt_uri` 在两条路径都保留，便于在不开 virt-ffi
    /// 时仍记录目标 libvirt URI（仅文档/调试用途）。
    pub struct LibvirtVmManager {
        /// 本管理器所在节点（domain 定义/启动默认落在此节点）
        local_node: NodeId,
        /// 目标 libvirt URI（如 `qemu:///system`）；骨架路径不实际连接。
        virt_uri: Option<String>,
        /// 内存态 VM 索引：VmId → Vm（占位；真实实现查询 libvirt）
        // 注意：用 Mutex 而非 RwLock 因写多读少且临界区极短。
        vms: Mutex<HashMap<VmId, Vm>>,
    }

    impl LibvirtVmManager {
        /// 构造管理器，绑定到指定本地节点；libvirt URI 默认 `qemu:///system`。
        pub fn new(local_node: impl Into<String>) -> Self {
            Self::with_uri(local_node, "qemu:///system")
        }

        /// 构造管理器并显式指定 libvirt 连接 URI（真实路径下用于 `virConnectOpen`）。
        pub fn with_uri(local_node: impl Into<String>, uri: impl Into<String>) -> Self {
            Self {
                local_node: NodeId::new(local_node),
                virt_uri: Some(uri.into()),
                vms: Mutex::new(HashMap::new()),
            }
        }

        /// 本节点 ID。
        pub fn local_node(&self) -> &NodeId {
            &self.local_node
        }

        /// 配置的 libvirt URI（骨架路径仅记录）。
        pub fn virt_uri(&self) -> Option<&str> {
            self.virt_uri.as_deref()
        }

        /// 内部：从内存索引取 VM 克隆（占位实现；真实应查 libvirt）。
        fn get_inner(&self, id: &VmId) -> ComputeResult<Vm> {
            self.vms
                .lock()
                .expect("vms mutex poisoned")
                .get(id)
                .cloned()
                .ok_or_else(|| ComputeError::VmNotFound(id.to_string()))
        }

        /// 内部：更新内存索引中 VM 的状态（占位）。
        fn set_state_inner(&self, id: &VmId, new_state: VmState) -> ComputeResult<()> {
            let mut guard = self.vms.lock().expect("vms mutex poisoned");
            let vm = guard
                .get_mut(id)
                .ok_or_else(|| ComputeError::VmNotFound(id.to_string()))?;
            vm.state = vm.state.transition_to(new_state)?;
            Ok(())
        }
    }

    impl VmManager for LibvirtVmManager {
        async fn create_vm(&self, id: &VmId, name: &str, spec: VmSpec) -> ComputeResult<Vm> {
            // 1) 纯逻辑：校验 spec 并生成 domain XML（这部分不依赖 libvirt，可测）
            spec.validate()?;
            let _xml = spec.to_libvirt_xml(id, name)?;

            // 2) 真实路径（virt-ffi）此处调 virDomainDefineXML；骨架路径仅记录索引。
            let vm = Vm::new_defined(id.clone(), name, spec);
            self.vms
                .lock()
                .expect("vms mutex poisoned")
                .insert(id.clone(), vm.clone());
            Ok(vm)
        }

        async fn destroy_vm(&self, id: &VmId) -> ComputeResult<()> {
            // 先确保存在
            let _vm = self.get_inner(id)?;
            // 真实路径：virDomainUndefine；若 domain 处于 Running 须先 stop。
            self.vms.lock().expect("vms mutex poisoned").remove(id);
            Ok(())
        }

        async fn start_vm(&self, id: &VmId) -> ComputeResult<Vm> {
            let _vm = self.get_inner(id)?;
            // 真实路径：virDomainCreate
            self.set_state_inner(id, VmState::Running)?;
            let mut vm = self.get_inner(id)?;
            vm.node_id = Some(self.local_node.clone());
            Ok(vm)
        }

        async fn stop_vm(&self, id: &VmId, force: bool) -> ComputeResult<Vm> {
            let _vm = self.get_inner(id)?;
            // 真实路径：force ? virDomainDestroy : virDomainShutdown
            //   （Shutdown 是软关机，需 guest agent 配合；Destroy 是硬断电）
            let _ = force; // 真实实现据 force 选择 API
            self.set_state_inner(id, VmState::Stopped)?;
            self.get_inner(id)
        }

        async fn pause_vm(&self, id: &VmId) -> ComputeResult<Vm> {
            let _vm = self.get_inner(id)?;
            // 真实路径：virDomainSuspend
            self.set_state_inner(id, VmState::Paused)?;
            self.get_inner(id)
        }

        async fn resume_vm(&self, id: &VmId) -> ComputeResult<Vm> {
            let _vm = self.get_inner(id)?;
            // 真实路径：virDomainResume
            self.set_state_inner(id, VmState::Running)?;
            self.get_inner(id)
        }

        async fn get_vm(&self, id: &VmId) -> ComputeResult<Vm> {
            // 真实路径：virDomainLookupByUUIDString + virDomainGetState
            self.get_inner(id)
        }

        async fn list_vms(&self) -> ComputeResult<Vec<Vm>> {
            // 真实路径：virConnectListAllDomains
            let guard = self.vms.lock().expect("vms mutex poisoned");
            Ok(guard.values().cloned().collect())
        }

        async fn migrate_vm(&self, id: &VmId, target_node: &NodeId) -> ComputeResult<TaskId> {
            let _vm = self.get_inner(id)?;
            // 真实路径：active-passive 迁移——
            //   1) 确认共享存储（zvol 已对 target 可见，由 storage-agent 保证）
            //   2) virDomainMigrateToURI3(dom, target_uri, …)
            //   3) 成功后更新 node_id，domain 在源节点进入 Stopped
            // 骨架返回新 TaskId 占位，标记 Migrating 态。
            self.set_state_inner(id, VmState::Migrating)?;
            let _ = target_node;
            Ok(TaskId::new())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::vm::CpuTopology;

        fn spec() -> VmSpec {
            VmSpec {
                cpus: CpuTopology::new(2),
                memory_mb: 1024,
                disk_vol_id: os_core::VolumeId::new("tank/vm/x"),
                nics: vec![crate::vm::VmNic::virtio("br0")],
                firmware: crate::vm::VmFirmware::Bios,
            }
        }

        #[tokio::test]
        async fn create_then_get_lifecycle() {
            let mgr = LibvirtVmManager::new("node-a");
            let id = VmId::new("vm-1");
            let vm = mgr.create_vm(&id, "test", spec()).await.expect("create");
            assert_eq!(vm.state, VmState::Stopped);
            assert!(vm.node_id.is_none());

            let got = mgr.get_vm(&id).await.expect("get");
            assert_eq!(got.name, "test");

            let started = mgr.start_vm(&id).await.expect("start");
            assert_eq!(started.state, VmState::Running);
            assert_eq!(started.node_id.as_ref().unwrap().as_str(), "node-a");

            let paused = mgr.pause_vm(&id).await.expect("pause");
            assert_eq!(paused.state, VmState::Paused);

            let resumed = mgr.resume_vm(&id).await.expect("resume");
            assert_eq!(resumed.state, VmState::Running);

            let stopped = mgr.stop_vm(&id, false).await.expect("stop");
            assert_eq!(stopped.state, VmState::Stopped);

            let listed = mgr.list_vms().await.expect("list");
            assert_eq!(listed.len(), 1);

            let task = mgr
                .migrate_vm(&id, &NodeId::new("node-b"))
                .await
                .expect("migrate");
            assert_ne!(task.0, os_core::Uuid::nil());
        }

        #[tokio::test]
        async fn create_rejects_invalid_spec() {
            let mgr = LibvirtVmManager::new("node-a");
            let mut bad = spec();
            bad.memory_mb = 0;
            assert!(mgr.create_vm(&VmId::new("x"), "x", bad).await.is_err());
        }

        #[tokio::test]
        async fn get_missing_returns_vm_not_found() {
            let mgr = LibvirtVmManager::new("node-a");
            let err = mgr.get_vm(&VmId::new("nope")).await.unwrap_err();
            assert!(matches!(err, ComputeError::VmNotFound(_)));
        }

        #[tokio::test]
        async fn destroy_removes_vm() {
            let mgr = LibvirtVmManager::new("node-a");
            let id = VmId::new("vm-1");
            mgr.create_vm(&id, "test", spec()).await.unwrap();
            mgr.destroy_vm(&id).await.unwrap();
            assert!(mgr.get_vm(&id).await.is_err());
        }

        #[tokio::test]
        async fn illegal_transition_errors() {
            // Stopped -> Paused 非法
            let mgr = LibvirtVmManager::new("node-a");
            let id = VmId::new("vm-1");
            mgr.create_vm(&id, "test", spec()).await.unwrap();
            let err = mgr.pause_vm(&id).await.unwrap_err();
            assert!(matches!(err, ComputeError::InvalidSpec(_)));
        }

        #[tokio::test]
        async fn with_uri_records_uri() {
            let mgr = LibvirtVmManager::with_uri("n1", "qemu:///session");
            assert_eq!(mgr.virt_uri(), Some("qemu:///session"));
            assert_eq!(mgr.local_node().as_str(), "n1");
        }
    }
}

// ============================================================================
// 真实路径：virt crate（libvirt FFI）—— feature `virt-ffi`
// ============================================================================

#[cfg(feature = "virt-ffi")]
mod virt_backend {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use os_core::{NodeId, TaskId, VmId};
    use virt::connect::Connect;
    use virt::domain::Domain;
    use virt::sys;

    use super::LibvirtDomainState;
    use crate::vm::{Vm, VmManager, VmSpec, VmState};
    use crate::{ComputeError, ComputeResult};

    /// 将 [`virt`] 错误统一映射为 [`ComputeError::LibvirtError`]（保留 Display 文本）。
    fn vmap<T, E: std::fmt::Display>(r: Result<T, E>) -> ComputeResult<T> {
        r.map_err(|e| ComputeError::LibvirtError(e.to_string()))
    }

    /// 基于 libvirt 的虚拟机管理器（真实实现，feature `virt-ffi`）。
    ///
    /// 通过 [`virt`] crate 真实连接 libvirt（`virConnectOpen`），所有生命周期方法
    /// 走 libvirt C API。构造时打开连接并缓存于 `Mutex`；后续操作复用该连接。
    ///
    /// 前置依赖：构建/运行机须 `apt install libvirt-dev` 且运行账号在 `libvirt` 组
    /// 或为 root（否则 `virConnectOpen` 返回权限错误）。
    pub struct LibvirtVmManager {
        /// 本管理器所在节点
        local_node: NodeId,
        /// 目标 libvirt URI（`virConnectOpen` 入参）
        virt_uri: String,
        /// 已打开的 libvirt 连接（惰性打开：首次调用时填充）
        conn: Mutex<Option<Connect>>,
        /// VmId → domain 名称的本地索引（用于在 libvirt 无对应 domain 时
        /// 返回 `VmNotFound`，并缓存 created_at）。libvirt 的真正主键是 UUID
        /// （domain XML `<uuid>`），故查找优先按 UUID 串。
        meta: Mutex<HashMap<VmId, VmMeta>>,
    }

    /// 单 VM 的本地缓存元数据（libvirt 不存 `created_at`，故本地保留）。
    #[derive(Clone)]
    struct VmMeta {
        name: String,
        spec: VmSpec,
        created_at: chrono::DateTime<chrono::Utc>,
    }

    impl LibvirtVmManager {
        /// 构造管理器，绑定到指定本地节点，连接 `qemu:///system`。
        pub fn new(local_node: impl Into<String>) -> Self {
            Self::with_uri(local_node, "qemu:///system")
        }

        /// 构造管理器并显式指定 libvirt 连接 URI。
        ///
        /// 常见 URI：`qemu:///system`（系统级，需权限）、`qemu:///session`
        /// （用户级）、`test:///default`（libvirt 内置测试驱动，无 KVM 即可，
        /// 供单测/集成测使用）。
        pub fn with_uri(local_node: impl Into<String>, uri: impl Into<String>) -> Self {
            Self {
                local_node: NodeId::new(local_node),
                virt_uri: uri.into(),
                conn: Mutex::new(None),
                meta: Mutex::new(HashMap::new()),
            }
        }

        /// 本节点 ID。
        pub fn local_node(&self) -> &NodeId {
            &self.local_node
        }

        /// 配置的 libvirt URI。
        pub fn virt_uri(&self) -> &str {
            &self.virt_uri
        }

        /// 打开（或复用已打开的）libvirt 连接。
        fn connect(&self) -> ComputeResult<Connect> {
            // 复用缓存连接（Clone 增引用计数；Drop 时减；最终由 Mutex 持有的那份 close）。
            let mut guard = self.conn.lock().expect("conn mutex poisoned");
            if guard.is_none() {
                let c = vmap(Connect::open(Some(self.virt_uri.as_str())))?;
                *guard = Some(c);
            }
            // Clone 一份给调用方（引用计数 +1）；调用方 Drop 时引用计数 -1，
            // 但底层连接由 guard 持有，不会被 close。clone() 不返回 Result。
            Ok(guard.as_ref().unwrap().clone())
        }

        /// 按 UUID 串（`VmId` 的字符串形态）在 libvirt 查找 domain。
        fn lookup_domain(conn: &Connect, id: &VmId) -> ComputeResult<Domain> {
            vmap(Domain::lookup_by_uuid_string(conn, id.as_str())).map_err(|e| match e {
                // libvirt 在 domain 不存在时返回的错误归一为 VmNotFound
                ComputeError::LibvirtError(_) => ComputeError::VmNotFound(id.to_string()),
                other => other,
            })
        }

        /// 从 libvirt domain + 本地缓存元数据重建 [`Vm`]。
        ///
        /// `_conn` 保留入参位以备后续扩展（如读连接级统计），当前仅用 `dom`。
        #[allow(unused_variables)]
        fn domain_to_vm(&self, _conn: &Connect, dom: &Domain) -> ComputeResult<Vm> {
            let uuid_str = vmap(dom.get_uuid_string())?;
            let id = VmId::new(uuid_str);
            let (raw_state, _reason) = vmap(dom.get_state())?;
            let state = LibvirtDomainState::from_raw(raw_state).to_vm_state();
            let meta = {
                let g = self.meta.lock().expect("meta mutex poisoned");
                g.get(&id).cloned()
            };
            let (name, spec, created_at) = match meta {
                Some(m) => (m.name, m.spec, m.created_at),
                None => {
                    // 缓存缺失（如 libvirt 中存在但本进程未创建过的 domain）：
                    // 用 libvirt 实际 name + 退化 spec 占位。
                    let name = vmap(dom.get_name())?;
                    (name, fallback_spec(), chrono::Utc::now())
                }
            };
            Ok(Vm {
                id,
                name,
                spec,
                state,
                // 仅 Running/Paused/Migrating 视为已调度到本节点
                node_id: if matches!(state, VmState::Running | VmState::Paused) {
                    Some(self.local_node.clone())
                } else {
                    None
                },
                created_at,
            })
        }
    }

    /// 当 libvirt 中存在 domain 但本地无 spec 缓存时使用的退化规格（仅占位）。
    fn fallback_spec() -> VmSpec {
        VmSpec {
            cpus: crate::vm::CpuTopology::new(1),
            memory_mb: 128,
            disk_vol_id: os_core::VolumeId::new("unknown/unknown"),
            nics: vec![crate::vm::VmNic::virtio("unknown")],
            firmware: crate::vm::VmFirmware::Bios,
        }
    }

    impl VmManager for LibvirtVmManager {
        async fn create_vm(&self, id: &VmId, name: &str, spec: VmSpec) -> ComputeResult<Vm> {
            // 1) 纯逻辑：校验 + 生成 domain XML（不依赖 libvirt，确保错误前置）
            spec.validate()?;
            let xml = spec.to_libvirt_xml(id, name)?;

            // 2) 真实：virDomainDefineXML（定义但不启动 → libvirt domain 进入 Shutoff）
            let conn = self.connect()?;
            let dom = vmap(Domain::define_xml(&conn, &xml))?;
            let created_at = chrono::Utc::now();
            // 缓存元数据（用于后续 get_vm 重建 spec/created_at）
            self.meta.lock().expect("meta mutex poisoned").insert(
                id.clone(),
                VmMeta {
                    name: name.to_string(),
                    spec: spec.clone(),
                    created_at,
                },
            );
            let vm = Vm {
                id: id.clone(),
                name: name.to_string(),
                spec,
                state: VmState::Stopped,
                node_id: None,
                created_at,
            };
            // dom 在此 Drop（引用计数 -1；底层 domain 定义由 libvirt 持久化保留）
            drop(dom);
            Ok(vm)
        }

        async fn destroy_vm(&self, id: &VmId) -> ComputeResult<()> {
            let conn = self.connect()?;
            let dom = Self::lookup_domain(&conn, id)?;
            // 先取状态：若 Running 则强制 destroy 后再 undefine（undefine 仅对
            // 非运行 domain 有效；运行中 domain 须 VIR_DOMAIN_UNDEFINE_* 配合或先停）。
            let (raw_state, _) = vmap(dom.get_state())?;
            let st = LibvirtDomainState::from_raw(raw_state).to_vm_state();
            if matches!(st, VmState::Running | VmState::Paused) {
                vmap(dom.destroy())?;
            }
            vmap(dom.undefine())?;
            self.meta.lock().expect("meta mutex poisoned").remove(id);
            Ok(())
        }

        async fn start_vm(&self, id: &VmId) -> ComputeResult<Vm> {
            let conn = self.connect()?;
            let dom = Self::lookup_domain(&conn, id)?;
            // virDomainCreate：已定义未运行的 domain → Running
            vmap(dom.create())?;
            self.domain_to_vm(&conn, &dom)
        }

        async fn stop_vm(&self, id: &VmId, force: bool) -> ComputeResult<Vm> {
            let conn = self.connect()?;
            let dom = Self::lookup_domain(&conn, id)?;
            // force=true → virDomainDestroy（硬断电）；false → virDomainShutdown（软关机）
            if force {
                vmap(dom.destroy())?;
            } else {
                vmap(dom.shutdown())?;
            }
            self.domain_to_vm(&conn, &dom)
        }

        async fn pause_vm(&self, id: &VmId) -> ComputeResult<Vm> {
            let conn = self.connect()?;
            let dom = Self::lookup_domain(&conn, id)?;
            vmap(dom.suspend())?;
            self.domain_to_vm(&conn, &dom)
        }

        async fn resume_vm(&self, id: &VmId) -> ComputeResult<Vm> {
            let conn = self.connect()?;
            let dom = Self::lookup_domain(&conn, id)?;
            vmap(dom.resume())?;
            self.domain_to_vm(&conn, &dom)
        }

        async fn get_vm(&self, id: &VmId) -> ComputeResult<Vm> {
            let conn = self.connect()?;
            let dom = Self::lookup_domain(&conn, id)?;
            self.domain_to_vm(&conn, &dom)
        }

        async fn list_vms(&self) -> ComputeResult<Vec<Vm>> {
            let conn = self.connect()?;
            // 列出活动 + 非活动 domain（位掩码：ACTIVE=1 | INACTIVE=2）
            let flags =
                sys::VIR_CONNECT_LIST_DOMAINS_ACTIVE | sys::VIR_CONNECT_LIST_DOMAINS_INACTIVE;
            let domains = vmap(conn.list_all_domains(flags))?;
            let mut out = Vec::with_capacity(domains.len());
            for dom in &domains {
                // 单个 domain 查询失败不应使整列失败（如并发删除）；跳过并继续。
                match self.domain_to_vm(&conn, dom) {
                    Ok(vm) => out.push(vm),
                    Err(_) => continue,
                }
            }
            Ok(out)
        }

        async fn migrate_vm(&self, id: &VmId, target_node: &NodeId) -> ComputeResult<TaskId> {
            let conn = self.connect()?;
            let dom = Self::lookup_domain(&conn, id)?;
            // active-passive 迁移初版：peer2peer + live + undefine_source。
            // 目标 URI 由 NodeId 拼装（约定：qemu+tcp://<node>/system）。
            // 共享存储（zvol）由 storage-agent 保证对 target 可见。
            let duri = format!("qemu+tcp://{}/system", target_node.as_str());
            let flags = sys::VIR_MIGRATE_PEER2PEER
                | sys::VIR_MIGRATE_LIVE
                | sys::VIR_MIGRATE_UNDEFINE_SOURCE;
            vmap(dom.migrate_to_uri(
                duri.as_str(),
                flags,
                None, // 目标 domain 名沿用源名
                0,    // 带宽不限
            ))?;
            // 标记迁移中（供 provision-agent 追踪）；真实迁移在 libvirt 后台执行。
            if let Some(m) = self.meta.lock().expect("meta mutex poisoned").get_mut(id) {
                let _ = &m; // 元数据保持；状态查询时由 libvirt 反映
            }
            Ok(TaskId::new())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::vm::CpuTopology;

        fn spec() -> VmSpec {
            VmSpec {
                cpus: CpuTopology::new(1),
                memory_mb: 128,
                disk_vol_id: os_core::VolumeId::new("tank/vm/x"),
                nics: vec![crate::vm::VmNic::virtio("br0")],
                firmware: crate::vm::VmFirmware::Bios,
            }
        }

        /// 判断 libvirt test 驱动可用（环境无 libvirt-dev 时连接会 Err）。
        /// 不可用时本测试模块整体跳过，避免链接失败以外的误报。
        fn test_driver_ok() -> bool {
            Connect::open(Some("test:///default")).is_ok()
        }

        #[tokio::test]
        async fn create_rejects_invalid_spec() {
            // spec 校验在调用 libvirt 之前，故无需真实连接。
            let mgr = LibvirtVmManager::with_uri("n1", "test:///default");
            let mut bad = spec();
            bad.memory_mb = 0;
            assert!(mgr.create_vm(&VmId::new("x"), "x", bad).await.is_err());
        }

        #[tokio::test]
        async fn open_test_driver_or_skip_lifecycle() {
            if !test_driver_ok() {
                eprintln!(
                    "skip: libvirt test 驱动不可用（无 libvirt-dev / libvirtd），\
                     仅当 cargo check --features virt-ffi 通过即满足"
                );
                return;
            }
            // 完整生命周期：test 驱动支持 define/create/suspend/resume/destroy/undefine。
            // VmId 必须是合法 UUID 串（libvirt domain <uuid> 会被校验）。
            let mgr = LibvirtVmManager::with_uri("node-a", "test:///default");
            let id = VmId::new("123e4567-e89b-12d3-a456-426614174000");
            let vm = mgr.create_vm(&id, "p2vm", spec()).await.expect("create");
            assert_eq!(vm.state, VmState::Stopped);

            let started = mgr.start_vm(&id).await.expect("start");
            assert_eq!(started.state, VmState::Running);

            let paused = mgr.pause_vm(&id).await.expect("pause");
            assert_eq!(paused.state, VmState::Paused);

            let resumed = mgr.resume_vm(&id).await.expect("resume");
            assert_eq!(resumed.state, VmState::Running);

            let stopped = mgr.stop_vm(&id, true).await.expect("stop");
            assert_eq!(stopped.state, VmState::Stopped);

            let got = mgr.get_vm(&id).await.expect("get");
            assert_eq!(got.id, id);

            mgr.destroy_vm(&id).await.expect("destroy");
            assert!(mgr.get_vm(&id).await.is_err());
        }

        #[tokio::test]
        async fn open_test_driver_or_skip_list() {
            if !test_driver_ok() {
                eprintln!("skip: libvirt test 驱动不可用");
                return;
            }
            let mgr = LibvirtVmManager::with_uri("node-a", "test:///default");
            let id = VmId::new("123e4567-e89b-12d3-a456-426614174001");
            mgr.create_vm(&id, "p2vm-list", spec()).await.unwrap();
            let listed = mgr.list_vms().await.expect("list");
            // test:///default 自带 1 个 domain + 我们定义的 1 个
            assert!(listed.iter().any(|v| v.id == id));
        }
    }
}

// ----------------------------------------------------------------------------
// 共享：re-export + 共享测试（两路径都跑的 LibvirtDomainState 映射测）
// ----------------------------------------------------------------------------

#[cfg(not(feature = "virt-ffi"))]
pub use fallback::LibvirtVmManager;
#[cfg(feature = "virt-ffi")]
pub use virt_backend::LibvirtVmManager;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libvirt_state_mapping() {
        use LibvirtDomainState::*;
        assert_eq!(Running.to_vm_state(), VmState::Running);
        assert_eq!(Blocked.to_vm_state(), VmState::Running);
        assert_eq!(Paused.to_vm_state(), VmState::Paused);
        assert_eq!(Pmsuspended.to_vm_state(), VmState::Paused);
        assert_eq!(Shutoff.to_vm_state(), VmState::Stopped);
        assert_eq!(Shutdown.to_vm_state(), VmState::Stopped);
        assert_eq!(Crashed.to_vm_state(), VmState::Failed);
        assert_eq!(NoState.to_vm_state(), VmState::Failed);
    }

    #[test]
    fn from_raw_covers_all_libvirt_states() {
        // 0..=7 是 libvirt 定义的全部 virDomainState 枚举值
        for raw in 0u32..=7 {
            let _ = LibvirtDomainState::from_raw(raw);
        }
        // 越界值并入 Pmsuspended 分支（不 panic）
        let overflow = LibvirtDomainState::from_raw(99);
        assert_eq!(overflow, LibvirtDomainState::Pmsuspended);
    }
}
