//! os-compute 全部 Mock 实现的统一落脚点——纯内存，供下游测试注入。
//!
//! 仅在 `mock` feature 下编译。下游（api/guest/service 等）在
//! `[dev-dependencies]` 加 `os-compute = { workspace = true, features = ["mock"] }`。
//!
//! 本模块聚合两套相互独立的 mock（review2 P-R2-2 / R1 P5 归并）：
//! - **容器/网络/包**（container-agent owner）：[`MockContainerRuntime`] /
//!   [`MockContainerNetwork`] / [`MockPackageManager`]
//! - **VM**（vm-agent owner）：[`MockVmManager`]
//!
//! 两套 mock 符号零重叠（VM 与 container 走不同的 id/newtype/状态机），归并到
//! 单文件后下游只需 `use os_compute::mock::*` 即可一次取齐，也消除了此前
//! 「`lib.rs` 同时 re-export 两个 mock 模块」的混淆面（详见 docs/REVIEW.md
//! §R2.3 P-R2-2）。
//!
//! 设计（见 `_conventions.md §5`）：
//! - 实现完整 trait（[`ContainerRuntime`] / [`ContainerNetwork`] / [`PackageManager`]
//!   / [`VmManager`]）；
//! - **不依赖外部状态**（无 youki/CNI/dpkg/libvirt 子进程）；
//! - 提供构造器预置返回值：`MockContainerRuntime::new().with_container(c)`、
//!   `MockVmManager::new("node").with_vm(v)`；
//! - 写操作更新内部状态，使后续读操作反映变更——便于下游测
//!   「创建后列出 / 销毁后不存在」等场景；
//! - 错误注入：`with_error`（持续）/ `fail_with`（消费一次）覆盖错误路径。

#![cfg(feature = "mock")]

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use os_core::{ContainerId, NodeId, TaskId, VmId};
use os_network::IpCidr;

use crate::container::{
    can_transition, Container, ContainerRuntime, ContainerSpec, ContainerState, ImageInfo,
};
use crate::container_net::{ContainerNetwork, NetworkDriver, NetworkInfo};
use crate::error::{ComputeError, ComputeResult};
use crate::pkg::{PackageId, PackageInfo, PackageManager};
use crate::vm::{Vm, VmManager, VmSpec, VmState};

// ============================================================================
// MockContainerRuntime
// ============================================================================

/// Mock 容器运行时——纯内存、确定性。
///
/// 状态：containers（id→Container）/ images（digest→ImageInfo）/ 强制错误。
/// 容器生命周期严格走 [`can_transition`] 校验，让下游测试能验证非法迁移被拒。
pub struct MockContainerRuntime {
    inner: Mutex<MockRtState>,
}

struct MockRtState {
    containers: HashMap<String, Container>,
    images: HashMap<String, ImageInfo>,
    /// 摘要计数器（用于 pull_image 返回递增 digest）
    pull_counter: u64,
    /// 强制错误（注入测试错误路径；None = 正常返回）
    forced_error: Option<ComputeError>,
}

impl Default for MockContainerRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl MockContainerRuntime {
    /// 构造空 mock。
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(MockRtState {
                containers: HashMap::new(),
                images: HashMap::new(),
                pull_counter: 0,
                forced_error: None,
            }),
        }
    }

    /// 预置容器（"已存在"场景）。
    pub fn with_container(self, c: Container) -> Self {
        self.inner
            .lock()
            .unwrap()
            .containers
            .insert(c.id.to_string(), c);
        self
    }

    /// 预置本地镜像。
    pub fn with_image(self, img: ImageInfo) -> Self {
        self.inner
            .lock()
            .unwrap()
            .images
            .insert(img.digest.clone(), img);
        self
    }

    /// 注入强制错误：所有后续方法返回该错误。
    pub fn with_error(self, e: ComputeError) -> Self {
        self.inner.lock().unwrap().forced_error = Some(e);
        self
    }

    fn check_err(state: &MockRtState) -> ComputeResult<()> {
        match &state.forced_error {
            Some(e) => Err(clone_err(e)),
            None => Ok(()),
        }
    }
}

/// 克隆 ComputeError（多数 variant 内含 String 可克隆；Io 包装 std::io::Error 不可克隆，
/// 这里仅重建常见 variant——Mock 不依赖 Io 错误注入，故安全）。
fn clone_err(e: &ComputeError) -> ComputeError {
    match e {
        ComputeError::VmNotFound(m) => ComputeError::VmNotFound(m.clone()),
        ComputeError::ContainerNotFound(m) => ComputeError::ContainerNotFound(m.clone()),
        ComputeError::ImagePullFailed(m) => ComputeError::ImagePullFailed(m.clone()),
        ComputeError::MigrationFailed(m) => ComputeError::MigrationFailed(m.clone()),
        ComputeError::NetworkNotFound(m) => ComputeError::NetworkNotFound(m.clone()),
        ComputeError::PackageNotFound(m) => ComputeError::PackageNotFound(m.clone()),
        ComputeError::InstallFailed(m) => ComputeError::InstallFailed(m.clone()),
        ComputeError::InvalidSpec(m) => ComputeError::InvalidSpec(m.clone()),
        ComputeError::HardwareVirtualizationUnavailable(m) => {
            ComputeError::HardwareVirtualizationUnavailable(m.clone())
        }
        ComputeError::LibvirtError(m) => ComputeError::LibvirtError(m.clone()),
        ComputeError::CommandFailed(m) => ComputeError::CommandFailed(m.clone()),
        ComputeError::Internal(m) => ComputeError::Internal(m.clone()),
        // Io 不可克隆：降级为 Internal，标注原消息
        ComputeError::Io(io) => ComputeError::Internal(format!("IO: {io}")),
    }
}

impl ContainerRuntime for MockContainerRuntime {
    async fn create_container(
        &self,
        id: &ContainerId,
        name: &str,
        spec: ContainerSpec,
    ) -> ComputeResult<Container> {
        let mut st = self.inner.lock().unwrap();
        Self::check_err(&st)?;
        spec.validate()?;
        if st.containers.contains_key(&id.to_string()) {
            return Err(ComputeError::InvalidSpec(format!("容器已存在: {id}")));
        }
        let c = Container::new(id.clone(), name.to_string(), spec);
        st.containers.insert(id.to_string(), c.clone());
        Ok(c)
    }

    async fn start_container(&self, id: &ContainerId) -> ComputeResult<Container> {
        let mut st = self.inner.lock().unwrap();
        Self::check_err(&st)?;
        let c = st
            .containers
            .get_mut(&id.to_string())
            .ok_or_else(|| ComputeError::ContainerNotFound(id.to_string()))?;
        let next = ContainerState::Running;
        if !can_transition(c.state, next) {
            return Err(ComputeError::InvalidSpec(format!(
                "容器 {id} 当前 {:?} 无法 start",
                c.state
            )));
        }
        c.state = next;
        Ok(c.clone())
    }

    async fn stop_container(&self, id: &ContainerId, _force: bool) -> ComputeResult<Container> {
        let mut st = self.inner.lock().unwrap();
        Self::check_err(&st)?;
        let c = st
            .containers
            .get_mut(&id.to_string())
            .ok_or_else(|| ComputeError::ContainerNotFound(id.to_string()))?;
        let next = ContainerState::Stopped;
        if !can_transition(c.state, next) {
            return Err(ComputeError::InvalidSpec(format!(
                "容器 {id} 当前 {:?} 无法 stop",
                c.state
            )));
        }
        c.state = next;
        Ok(c.clone())
    }

    async fn remove_container(&self, id: &ContainerId) -> ComputeResult<()> {
        let mut st = self.inner.lock().unwrap();
        Self::check_err(&st)?;
        let c = st
            .containers
            .get(&id.to_string())
            .ok_or_else(|| ComputeError::ContainerNotFound(id.to_string()))?;
        if c.state == ContainerState::Running {
            return Err(ComputeError::InvalidSpec(format!(
                "容器 {id} 运行中，须先停止再删除"
            )));
        }
        st.containers.remove(&id.to_string());
        Ok(())
    }

    async fn get_container(&self, id: &ContainerId) -> ComputeResult<Container> {
        let st = self.inner.lock().unwrap();
        Self::check_err(&st)?;
        st.containers
            .get(&id.to_string())
            .cloned()
            .ok_or_else(|| ComputeError::ContainerNotFound(id.to_string()))
    }

    async fn list_containers(&self) -> ComputeResult<Vec<Container>> {
        let st = self.inner.lock().unwrap();
        Self::check_err(&st)?;
        Ok(st.containers.values().cloned().collect())
    }

    async fn pull_image(&self, image: &str) -> ComputeResult<String> {
        let mut st = self.inner.lock().unwrap();
        Self::check_err(&st)?;
        if image.trim().is_empty() {
            return Err(ComputeError::InvalidSpec("镜像名不能为空".into()));
        }
        st.pull_counter += 1;
        let digest = format!("sha256:{:064x}", st.pull_counter);
        let img = ImageInfo {
            digest: digest.clone(),
            name: image.to_string(),
            size: 1024 * 1024, // 占位 1MB
            pulled_at: chrono::Utc::now(),
        };
        st.images.insert(digest.clone(), img);
        Ok(digest)
    }

    async fn list_images(&self) -> ComputeResult<Vec<ImageInfo>> {
        let st = self.inner.lock().unwrap();
        Self::check_err(&st)?;
        Ok(st.images.values().cloned().collect())
    }

    async fn remove_image(&self, digest: &str) -> ComputeResult<()> {
        let mut st = self.inner.lock().unwrap();
        Self::check_err(&st)?;
        if st.images.remove(digest).is_none() {
            return Err(ComputeError::ImagePullFailed(format!(
                "镜像不存在: {digest}"
            )));
        }
        Ok(())
    }
}

// ============================================================================
// MockContainerNetwork
// ============================================================================

/// Mock 容器网络——纯内存。
pub struct MockContainerNetwork {
    inner: Mutex<MockNetState>,
}

struct MockNetState {
    networks: HashMap<String, NetworkInfo>,
    /// 接入关系：(container_id, network_name) → true
    connections: HashMap<(String, String), ()>,
    forced_error: Option<ComputeError>,
}

impl Default for MockContainerNetwork {
    fn default() -> Self {
        Self::new()
    }
}

impl MockContainerNetwork {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(MockNetState {
                networks: HashMap::new(),
                connections: HashMap::new(),
                forced_error: None,
            }),
        }
    }

    /// 预置网络。
    pub fn with_network(self, n: NetworkInfo) -> Self {
        self.inner
            .lock()
            .unwrap()
            .networks
            .insert(n.name.clone(), n);
        self
    }

    /// 注入强制错误。
    pub fn with_error(self, e: ComputeError) -> Self {
        self.inner.lock().unwrap().forced_error = Some(e);
        self
    }

    fn check_err(state: &MockNetState) -> ComputeResult<()> {
        match &state.forced_error {
            Some(e) => Err(clone_err(e)),
            None => Ok(()),
        }
    }
}

impl ContainerNetwork for MockContainerNetwork {
    async fn create_network(&self, name: &str, subnet: IpCidr) -> ComputeResult<NetworkInfo> {
        let mut st = self.inner.lock().unwrap();
        Self::check_err(&st)?;
        if name.trim().is_empty() {
            return Err(ComputeError::InvalidSpec("网络名不能为空".into()));
        }
        if st.networks.contains_key(name) {
            return Err(ComputeError::InvalidSpec(format!("网络已存在: {name}")));
        }
        let info = NetworkInfo {
            name: name.to_string(),
            subnet,
            driver: NetworkDriver::Bridge,
            container_count: 0,
        };
        st.networks.insert(name.to_string(), info.clone());
        Ok(info)
    }

    async fn delete_network(&self, name: &str) -> ComputeResult<()> {
        let mut st = self.inner.lock().unwrap();
        Self::check_err(&st)?;
        let exists = st.networks.contains_key(name);
        if !exists {
            return Err(ComputeError::NetworkNotFound(name.to_string()));
        }
        // 检查无容器接入
        let has_conn = st.connections.keys().any(|(_, net)| net == name);
        if has_conn {
            return Err(ComputeError::InvalidSpec(format!(
                "网络 {name} 仍有容器接入，无法删除"
            )));
        }
        st.networks.remove(name);
        Ok(())
    }

    async fn connect(&self, container: &ContainerId, network: &str) -> ComputeResult<()> {
        let mut st = self.inner.lock().unwrap();
        Self::check_err(&st)?;
        let key = (container.to_string(), network.to_string());
        // 先做存在性/重复接入检查（不可变借），再分别 mutate 两张 map
        if !st.networks.contains_key(network) {
            return Err(ComputeError::NetworkNotFound(network.to_string()));
        }
        if st.connections.contains_key(&key) {
            return Err(ComputeError::InvalidSpec(format!(
                "容器 {container} 已接入网络 {network}"
            )));
        }
        st.connections.insert(key, ());
        if let Some(info) = st.networks.get_mut(network) {
            info.container_count += 1;
        }
        Ok(())
    }

    async fn disconnect(&self, container: &ContainerId, network: &str) -> ComputeResult<()> {
        let mut st = self.inner.lock().unwrap();
        Self::check_err(&st)?;
        let key = (container.to_string(), network.to_string());
        if st.connections.remove(&key).is_none() {
            return Err(ComputeError::NetworkNotFound(format!(
                "容器 {container} 未接入网络 {network}"
            )));
        }
        if let Some(info) = st.networks.get_mut(network) {
            info.container_count = info.container_count.saturating_sub(1);
        }
        Ok(())
    }

    async fn list_networks(&self) -> ComputeResult<Vec<NetworkInfo>> {
        let st = self.inner.lock().unwrap();
        Self::check_err(&st)?;
        Ok(st.networks.values().cloned().collect())
    }
}

// ============================================================================
// MockPackageManager
// ============================================================================

/// Mock 包管理器——纯内存。
pub struct MockPackageManager {
    inner: Mutex<MockPkgState>,
}

struct MockPkgState {
    installed: HashMap<String, PackageInfo>,
    /// install 时根据 deb 路径名推断的包名（确定性返回）
    forced_error: Option<ComputeError>,
}

impl Default for MockPackageManager {
    fn default() -> Self {
        Self::new()
    }
}

impl MockPackageManager {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(MockPkgState {
                installed: HashMap::new(),
                forced_error: None,
            }),
        }
    }

    /// 预置已安装包。
    pub fn with_package(self, p: PackageInfo) -> Self {
        self.inner
            .lock()
            .unwrap()
            .installed
            .insert(p.id.to_string(), p);
        self
    }

    /// 注入强制错误。
    pub fn with_error(self, e: ComputeError) -> Self {
        self.inner.lock().unwrap().forced_error = Some(e);
        self
    }

    fn check_err(state: &MockPkgState) -> ComputeResult<()> {
        match &state.forced_error {
            Some(e) => Err(clone_err(e)),
            None => Ok(()),
        }
    }
}

impl PackageManager for MockPackageManager {
    async fn install(&self, deb_path: &Path) -> ComputeResult<PackageInfo> {
        let mut st = self.inner.lock().unwrap();
        Self::check_err(&st)?;
        // 用 apt::parse_deb_filename 推断包名（确定性）
        let fname = deb_path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| {
                ComputeError::InvalidSpec(format!("非法路径: {}", deb_path.display()))
            })?;
        let parsed = crate::apt::parse_deb_filename(fname)?;
        let info = PackageInfo::third_party(
            PackageId::new(parsed.package),
            parsed.version.unwrap_or_else(|| "0.0.0".to_string()),
        )
        .with_description(format!("mock 安装自 {}", deb_path.display()));
        st.installed.insert(info.id.to_string(), info.clone());
        Ok(info)
    }

    async fn uninstall(&self, id: &PackageId) -> ComputeResult<()> {
        let mut st = self.inner.lock().unwrap();
        Self::check_err(&st)?;
        if st.installed.remove(&id.to_string()).is_none() {
            return Err(ComputeError::PackageNotFound(id.to_string()));
        }
        Ok(())
    }

    async fn upgrade(&self, id: &PackageId) -> ComputeResult<PackageInfo> {
        let mut st = self.inner.lock().unwrap();
        Self::check_err(&st)?;
        let info = st
            .installed
            .get_mut(&id.to_string())
            .ok_or_else(|| ComputeError::PackageNotFound(id.to_string()))?;
        // 模拟版本递增
        info.version = format!("{}+upgraded", info.version);
        Ok(info.clone())
    }

    async fn list_installed(&self) -> ComputeResult<Vec<PackageInfo>> {
        let st = self.inner.lock().unwrap();
        Self::check_err(&st)?;
        Ok(st.installed.values().cloned().collect())
    }

    async fn search(&self, query: &str) -> ComputeResult<Vec<PackageInfo>> {
        let st = self.inner.lock().unwrap();
        Self::check_err(&st)?;
        let q = query.to_ascii_lowercase();
        Ok(st
            .installed
            .values()
            .filter(|p| {
                p.id.as_str().to_ascii_lowercase().contains(&q)
                    || p.description.to_ascii_lowercase().contains(&q)
            })
            .cloned()
            .collect())
    }
}

// ============================================================================
// MockVmManager（原 mock_vm.rs，review2 P-R2-2 归并入此）
// ============================================================================

/// VM 管理 mock。内部维护一份 VM 索引，所有方法在内存态完成。
///
/// 归并说明（review2 P-R2-2）：原位于独立文件 `mock_vm.rs`，现归并至本模块。
/// 公共 API 未变：`new` / `with_vm` / `fail_with` 与原 `MockVmManager` 完全一致，
/// 仅物理位置从 `mock_vm` 模块迁移到 `mock` 模块（trait 签名未改）。
pub struct MockVmManager {
    vms: Mutex<HashMap<VmId, Vm>>,
    /// 若设置，下一次对应操作返回此错误（用于测试错误路径）
    next_error: Mutex<Option<ComputeError>>,
    /// 创建 VM 时默认绑定的本地节点
    local_node: NodeId,
}

impl Default for MockVmManager {
    fn default() -> Self {
        Self::new("mock-node")
    }
}

impl MockVmManager {
    /// 构造空 mock，绑定到指定本地节点。
    pub fn new(local_node: impl Into<String>) -> Self {
        Self {
            vms: Mutex::new(HashMap::new()),
            next_error: Mutex::new(None),
            local_node: NodeId::new(local_node),
        }
    }

    /// 预置一个已存在的 VM（常用于 get/list/start 的初始状态）。
    pub fn with_vm(self, vm: Vm) -> Self {
        self.vms
            .lock()
            .expect("vms mutex poisoned")
            .insert(vm.id.clone(), vm);
        self
    }

    /// 让下一次操作返回指定错误（消费一次后清除），用于测试错误路径。
    pub fn fail_with(self, err: ComputeError) -> Self {
        *self.next_error.lock().expect("next_error poisoned") = Some(err);
        self
    }

    /// 取出并清除下一次的预设错误（若有）。
    fn take_error(&self) -> Option<ComputeError> {
        self.next_error.lock().expect("next_error poisoned").take()
    }

    fn get_inner(&self, id: &VmId) -> ComputeResult<Vm> {
        self.vms
            .lock()
            .expect("vms mutex poisoned")
            .get(id)
            .cloned()
            .ok_or_else(|| ComputeError::VmNotFound(id.to_string()))
    }

    fn set_state(&self, id: &VmId, state: VmState) -> ComputeResult<()> {
        let mut g = self.vms.lock().expect("vms mutex poisoned");
        let vm = g
            .get_mut(id)
            .ok_or_else(|| ComputeError::VmNotFound(id.to_string()))?;
        vm.state = vm.state.transition_to(state)?;
        Ok(())
    }
}

impl VmManager for MockVmManager {
    async fn create_vm(&self, id: &VmId, name: &str, spec: VmSpec) -> ComputeResult<Vm> {
        if let Some(e) = self.take_error() {
            return Err(e);
        }
        spec.validate()?;
        let vm = Vm::new_defined(id.clone(), name, spec);
        self.vms
            .lock()
            .expect("vms mutex poisoned")
            .insert(id.clone(), vm.clone());
        Ok(vm)
    }

    async fn destroy_vm(&self, id: &VmId) -> ComputeResult<()> {
        if let Some(e) = self.take_error() {
            return Err(e);
        }
        let mut g = self.vms.lock().expect("vms mutex poisoned");
        g.remove(id)
            .map(|_| ())
            .ok_or_else(|| ComputeError::VmNotFound(id.to_string()))
    }

    async fn start_vm(&self, id: &VmId) -> ComputeResult<Vm> {
        if let Some(e) = self.take_error() {
            return Err(e);
        }
        self.get_inner(id)?;
        self.set_state(id, VmState::Running)?;
        let mut vm = self.get_inner(id)?;
        vm.node_id = Some(self.local_node.clone());
        Ok(vm)
    }

    async fn stop_vm(&self, id: &VmId, _force: bool) -> ComputeResult<Vm> {
        if let Some(e) = self.take_error() {
            return Err(e);
        }
        self.get_inner(id)?;
        self.set_state(id, VmState::Stopped)?;
        self.get_inner(id)
    }

    async fn pause_vm(&self, id: &VmId) -> ComputeResult<Vm> {
        if let Some(e) = self.take_error() {
            return Err(e);
        }
        self.get_inner(id)?;
        self.set_state(id, VmState::Paused)?;
        self.get_inner(id)
    }

    async fn resume_vm(&self, id: &VmId) -> ComputeResult<Vm> {
        if let Some(e) = self.take_error() {
            return Err(e);
        }
        self.get_inner(id)?;
        self.set_state(id, VmState::Running)?;
        self.get_inner(id)
    }

    async fn get_vm(&self, id: &VmId) -> ComputeResult<Vm> {
        if let Some(e) = self.take_error() {
            return Err(e);
        }
        self.get_inner(id)
    }

    async fn list_vms(&self) -> ComputeResult<Vec<Vm>> {
        if let Some(e) = self.take_error() {
            return Err(e);
        }
        let g = self.vms.lock().expect("vms mutex poisoned");
        Ok(g.values().cloned().collect())
    }

    async fn migrate_vm(&self, id: &VmId, _target_node: &NodeId) -> ComputeResult<TaskId> {
        if let Some(e) = self.take_error() {
            return Err(e);
        }
        self.get_inner(id)?;
        self.set_state(id, VmState::Migrating)?;
        Ok(TaskId::new())
    }
}

// ============================================================================
// 测试（Mock 自测——验证 mock 行为符合 _conventions.md §5.2）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use os_core::ContainerId;
    use os_network::IpCidr;
    use std::net::{IpAddr, Ipv4Addr};

    fn cidr() -> IpCidr {
        IpCidr::new(IpAddr::V4(Ipv4Addr::new(10, 88, 0, 0)), 24)
    }

    #[tokio::test]
    async fn container_lifecycle_create_start_stop_remove() {
        let rt = MockContainerRuntime::new();
        let id = ContainerId::new("c1");
        let c = rt
            .create_container(&id, "nginx", ContainerSpec::new("nginx:1.25"))
            .await
            .unwrap();
        assert_eq!(c.state, ContainerState::Created);

        let c = rt.start_container(&id).await.unwrap();
        assert_eq!(c.state, ContainerState::Running);

        // running 不能直接 remove
        let err = rt.remove_container(&id).await.unwrap_err();
        assert!(matches!(err, ComputeError::InvalidSpec(_)));

        let c = rt.stop_container(&id, false).await.unwrap();
        assert_eq!(c.state, ContainerState::Stopped);

        rt.remove_container(&id).await.unwrap();
        assert!(rt.get_container(&id).await.is_err());
    }

    #[tokio::test]
    async fn container_create_duplicate_errors() {
        let rt = MockContainerRuntime::new();
        let id = ContainerId::new("c1");
        rt.create_container(&id, "x", ContainerSpec::new("img"))
            .await
            .unwrap();
        let err = rt
            .create_container(&id, "x", ContainerSpec::new("img"))
            .await
            .unwrap_err();
        assert!(matches!(err, ComputeError::InvalidSpec(_)));
    }

    #[tokio::test]
    async fn container_pull_image_returns_distinct_digests() {
        let rt = MockContainerRuntime::new();
        let d1 = rt.pull_image("nginx").await.unwrap();
        let d2 = rt.pull_image("redis").await.unwrap();
        assert_ne!(d1, d2);
        assert!(d1.starts_with("sha256:"));
        assert_eq!(rt.list_images().await.unwrap().len(), 2);
        rt.remove_image(&d1).await.unwrap();
        assert_eq!(rt.list_images().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn container_forced_error_propagates() {
        let rt = MockContainerRuntime::new().with_error(ComputeError::Internal("boom".into()));
        let err = rt.list_containers().await.unwrap_err();
        assert!(matches!(err, ComputeError::Internal(_)));
    }

    #[tokio::test]
    async fn network_create_delete_and_connect() {
        let net = MockContainerNetwork::new();
        let info = net.create_network("osnet", cidr()).await.unwrap();
        assert_eq!(info.driver, NetworkDriver::Bridge);
        assert_eq!(info.container_count, 0);

        let cid = ContainerId::new("c1");
        net.connect(&cid, "osnet").await.unwrap();
        let count = net
            .list_networks()
            .await
            .unwrap()
            .into_iter()
            .find(|n| n.name == "osnet")
            .map(|n| n.container_count)
            .unwrap_or(0);
        assert_eq!(count, 1);

        net.disconnect(&cid, "osnet").await.unwrap();
        // 删除网络（无接入）
        net.delete_network("osnet").await.unwrap();
    }

    #[tokio::test]
    async fn network_delete_with_connection_errors() {
        let net = MockContainerNetwork::new();
        net.create_network("n", cidr()).await.unwrap();
        let cid = ContainerId::new("c1");
        net.connect(&cid, "n").await.unwrap();
        let err = net.delete_network("n").await.unwrap_err();
        assert!(matches!(err, ComputeError::InvalidSpec(_)));
    }

    #[tokio::test]
    async fn network_not_found_on_missing() {
        let net = MockContainerNetwork::new();
        let err = net.delete_network("nope").await.unwrap_err();
        assert!(matches!(err, ComputeError::NetworkNotFound(_)));
    }

    #[tokio::test]
    async fn pkg_install_and_search() {
        let pm = MockPackageManager::new();
        let info = pm
            .install(std::path::Path::new("/tmp/code_1.85_amd64.deb"))
            .await
            .unwrap();
        assert_eq!(info.id.as_str(), "code");
        assert_eq!(info.version, "1.85");

        let found = pm.search("code").await.unwrap();
        assert_eq!(found.len(), 1);
    }

    #[tokio::test]
    async fn pkg_uninstall_missing_errors() {
        let pm = MockPackageManager::new();
        let err = pm.uninstall(&PackageId::new("nope")).await.unwrap_err();
        assert!(matches!(err, ComputeError::PackageNotFound(_)));
    }

    #[tokio::test]
    async fn pkg_upgrade_bumps_version() {
        let pm = MockPackageManager::new()
            .with_package(PackageInfo::official(PackageId::new("redis"), "7.0"));
        let info = pm.upgrade(&PackageId::new("redis")).await.unwrap();
        assert!(info.version.contains("upgraded"));
    }
}

// ============================================================================
// MockVmManager 自测（原 mock_vm.rs 的 tests，迁移自归并前的独立模块）
// ============================================================================

#[cfg(test)]
mod vm_tests {
    use super::*;
    use crate::vm::{CpuTopology, VmFirmware, VmNic};
    use os_core::VmId;

    fn spec() -> VmSpec {
        VmSpec {
            cpus: CpuTopology::new(2),
            memory_mb: 1024,
            disk_vol_id: os_core::VolumeId::new("tank/vm/x"),
            nics: vec![VmNic::virtio("br0")],
            firmware: VmFirmware::Bios,
        }
    }

    fn running_vm(id: &str) -> Vm {
        let mut vm = Vm::new_defined(VmId::new(id), id, spec());
        vm.state = VmState::Running;
        vm.node_id = Some(NodeId::new("mock-node"));
        vm
    }

    #[tokio::test]
    async fn create_get_list_destroy() {
        let mgr = MockVmManager::default();
        let id = VmId::new("v1");
        let vm = mgr.create_vm(&id, "v1", spec()).await.unwrap();
        assert_eq!(vm.state, VmState::Stopped);

        assert!(mgr.get_vm(&id).await.is_ok());
        assert_eq!(mgr.list_vms().await.unwrap().len(), 1);

        mgr.destroy_vm(&id).await.unwrap();
        assert!(mgr.get_vm(&id).await.is_err());
        assert!(mgr.list_vms().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn lifecycle_start_pause_resume_stop() {
        let mgr = MockVmManager::default().with_vm(running_vm("v1"));
        let id = VmId::new("v1");
        // 预置为 Running
        assert_eq!(mgr.get_vm(&id).await.unwrap().state, VmState::Running);
        // pause/resume
        assert_eq!(mgr.pause_vm(&id).await.unwrap().state, VmState::Paused);
        assert_eq!(mgr.resume_vm(&id).await.unwrap().state, VmState::Running);
        // stop
        assert_eq!(
            mgr.stop_vm(&id, false).await.unwrap().state,
            VmState::Stopped
        );
    }

    #[tokio::test]
    async fn fail_with_consumes_once() {
        let mgr = MockVmManager::default().fail_with(ComputeError::LibvirtError("boom".into()));
        // 第一次 get 因 fail_with 返回错误
        let err = mgr.get_vm(&VmId::new("any")).await.unwrap_err();
        assert!(matches!(err, ComputeError::LibvirtError(_)));
        // 第二次恢复正常（NotFound）
        let err2 = mgr.get_vm(&VmId::new("any")).await.unwrap_err();
        assert!(matches!(err2, ComputeError::VmNotFound(_)));
    }

    #[tokio::test]
    async fn create_rejects_invalid_spec() {
        let mgr = MockVmManager::default();
        let mut bad = spec();
        bad.cpus = CpuTopology::new(0);
        assert!(mgr.create_vm(&VmId::new("x"), "x", bad).await.is_err());
    }

    #[tokio::test]
    async fn migrate_marks_migrating() {
        let mgr = MockVmManager::default().with_vm(running_vm("v1"));
        let id = VmId::new("v1");
        let task = mgr.migrate_vm(&id, &NodeId::new("node-b")).await.unwrap();
        assert_ne!(task.0, os_core::Uuid::nil());
        assert_eq!(mgr.get_vm(&id).await.unwrap().state, VmState::Migrating);
    }

    #[tokio::test]
    async fn get_missing_not_found() {
        let mgr = MockVmManager::default();
        let err = mgr.get_vm(&VmId::new("nope")).await.unwrap_err();
        assert!(matches!(err, ComputeError::VmNotFound(_)));
    }
}
