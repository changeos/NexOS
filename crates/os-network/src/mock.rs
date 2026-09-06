//! Mock 实现（feature gate `mock`）
//!
//! 供下游 agent（compute / security / guest / provision / meta 等）在单测/集成测中
//! 注入确定性、纯内存的网络层依赖，避免依赖真实 netlink/nft/DHCP/DNS/PXE 环境。
//!
//! 用法（下游 `[dev-dependencies]`）：
//! ```toml
//! os-network = { workspace = true, features = ["mock"] }
//! ```
//!
//! 设计（见 `_conventions.md` §5）：
//! - 实现完整 trait，默认返回安全值（空列表 / Ok(())）；
//! - 提供 builder 风格构造器预置返回值 / 记录调用以供断言；
//! - 纯内存、无外部状态、确定性。
//!
//! 注：trait 用原生 `async fn in trait`（非 dyn 兼容，见 ADR-COMPAT-001）。
//! mock 作为具体类型实现，下游以具体类型或泛型注入即可。若需 `Box<dyn>`，须经 ADR
//! 把对应 trait 切换为 `#[async_trait]`。

use crate::firewall::{FirewallRule, NatRule};
use crate::interface::{BondMode, Interface, InterfaceId, IpCidr};
use crate::services::{DhcpLease, DnsRecord, PxeState, PxeStatus};
use std::net::IpAddr;
use std::sync::Mutex;

// ============================================================================
// MockNetworkManager
// ============================================================================

/// 内存版 `NetworkManager`：预置接口列表，记录操作。
#[derive(Debug, Default)]
pub struct MockNetworkManager {
    inner: Mutex<MockNetworkState>,
}

#[derive(Debug, Default)]
struct MockNetworkState {
    interfaces: Vec<Interface>,
    /// 记录 up/down/delete 等调用（按顺序），供断言。
    ops: Vec<String>,
}

impl MockNetworkManager {
    /// 构造空 mock。
    pub fn new() -> Self {
        Self::default()
    }

    /// 预置一个接口（不触发校验，直接插入）。
    pub fn with_interface(mut self, iface: Interface) -> Self {
        self.inner.get_mut().unwrap().interfaces.push(iface);
        self
    }

    /// 预置多个接口。
    pub fn with_interfaces(mut self, ifaces: impl IntoIterator<Item = Interface>) -> Self {
        self.inner.get_mut().unwrap().interfaces.extend(ifaces);
        self
    }

    /// 取已记录的操作序列（供测试断言）。
    pub fn recorded_ops(&self) -> Vec<String> {
        self.inner.lock().unwrap().ops.clone()
    }
}

#[allow(async_fn_in_trait)]
impl crate::interface::NetworkManager for MockNetworkManager {
    async fn list_interfaces(&self) -> Result<Vec<Interface>, crate::NetworkError> {
        Ok(self.inner.lock().unwrap().interfaces.clone())
    }

    async fn get_interface(&self, id: &InterfaceId) -> Result<Interface, crate::NetworkError> {
        self.inner
            .lock()
            .unwrap()
            .interfaces
            .iter()
            .find(|i| i.id == *id)
            .cloned()
            .ok_or_else(|| crate::NetworkError::InterfaceNotFound(id.to_string()))
    }

    async fn create_vlan(
        &self,
        parent: &InterfaceId,
        vid: u16,
        name: InterfaceId,
    ) -> Result<Interface, crate::NetworkError> {
        crate::interface::validate_vlan_id(vid)?;
        let iface =
            Interface::new(name, crate::interface::InterfaceType::Vlan).with_parent(parent.clone());
        self.inner.lock().unwrap().interfaces.push(iface.clone());
        Ok(iface)
    }

    async fn create_bridge(&self, name: InterfaceId) -> Result<Interface, crate::NetworkError> {
        Interface::validate_name(name.as_str())?;
        let iface = Interface::new(name, crate::interface::InterfaceType::Bridge);
        self.inner.lock().unwrap().interfaces.push(iface.clone());
        Ok(iface)
    }

    async fn create_bond(
        &self,
        name: InterfaceId,
        _mode: BondMode,
        slaves: Vec<InterfaceId>,
    ) -> Result<Interface, crate::NetworkError> {
        Interface::validate_name(name.as_str())?;
        let mut iface = Interface::new(name, crate::interface::InterfaceType::Bond);
        if let Some(first) = slaves.into_iter().next() {
            iface = iface.with_parent(first);
        }
        self.inner.lock().unwrap().interfaces.push(iface.clone());
        Ok(iface)
    }

    async fn set_address(
        &self,
        id: &InterfaceId,
        addrs: Vec<IpCidr>,
    ) -> Result<(), crate::NetworkError> {
        let mut st = self.inner.lock().unwrap();
        let iface = st
            .interfaces
            .iter_mut()
            .find(|i| i.id == *id)
            .ok_or_else(|| crate::NetworkError::InterfaceNotFound(id.to_string()))?;
        iface.addrs = addrs;
        Ok(())
    }

    async fn up(&self, id: &InterfaceId) -> Result<(), crate::NetworkError> {
        let mut st = self.inner.lock().unwrap();
        let iface = st
            .interfaces
            .iter_mut()
            .find(|i| i.id == *id)
            .ok_or_else(|| crate::NetworkError::InterfaceNotFound(id.to_string()))?;
        iface.state = crate::interface::IfState::Up;
        st.ops.push(format!("up:{}", id));
        Ok(())
    }

    async fn down(&self, id: &InterfaceId) -> Result<(), crate::NetworkError> {
        let mut st = self.inner.lock().unwrap();
        let iface = st
            .interfaces
            .iter_mut()
            .find(|i| i.id == *id)
            .ok_or_else(|| crate::NetworkError::InterfaceNotFound(id.to_string()))?;
        iface.state = crate::interface::IfState::Down;
        st.ops.push(format!("down:{}", id));
        Ok(())
    }

    async fn delete_interface(&self, id: &InterfaceId) -> Result<(), crate::NetworkError> {
        let mut st = self.inner.lock().unwrap();
        let pos = st
            .interfaces
            .iter()
            .position(|i| i.id == *id)
            .ok_or_else(|| crate::NetworkError::InterfaceNotFound(id.to_string()))?;
        // 契约：物理接口拒绝删除
        if matches!(
            st.interfaces[pos].ty,
            crate::interface::InterfaceType::Physical | crate::interface::InterfaceType::Loopback
        ) {
            return Err(crate::NetworkError::RuleInvalid(format!(
                "物理/回环接口不可删除: {id}"
            )));
        }
        st.interfaces.remove(pos);
        st.ops.push(format!("delete:{}", id));
        Ok(())
    }
}

// ============================================================================
// MockFirewall
// ============================================================================

/// 内存版 `Firewall`：维护规则与 NAT 列表，记录 dry-run/add 调用。
#[derive(Debug, Default)]
pub struct MockFirewall {
    inner: Mutex<MockFirewallState>,
}

#[derive(Debug, Default)]
struct MockFirewallState {
    rules: Vec<(String, FirewallRule)>,
    nats: Vec<NatRule>,
    next_id: u64,
    /// 若 Some，dry_run 返回 RuleInvalid(该消息)（模拟规则风险）。
    /// 用消息字符串而非 NetworkError，避免依赖 Error 的 Clone。
    dry_run_error_msg: Option<String>,
}

impl MockFirewall {
    /// 构造空 mock。
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置 dry_run 的失败消息（模拟"规则有风险"，dry_run 返回 `RuleInvalid(msg)`）。
    pub fn with_dry_run_error(mut self, msg: impl Into<String>) -> Self {
        self.inner.get_mut().unwrap().dry_run_error_msg = Some(msg.into());
        self
    }

    /// 当前规则数。
    pub fn rule_count(&self) -> usize {
        self.inner.lock().unwrap().rules.len()
    }

    /// 当前 NAT 规则快照。
    pub fn nat_snapshot(&self) -> Vec<NatRule> {
        self.inner.lock().unwrap().nats.clone()
    }
}

#[allow(async_fn_in_trait)]
impl crate::firewall::Firewall for MockFirewall {
    async fn list_rules(&self) -> Result<Vec<FirewallRule>, crate::NetworkError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .rules
            .iter()
            .map(|(_, r)| r.clone())
            .collect())
    }

    async fn add_rule(&self, rule: FirewallRule) -> Result<String, crate::NetworkError> {
        // 默认先 dry_run（契约 §3.9）
        self.dry_run(&rule).await?;
        let mut st = self.inner.lock().unwrap();
        st.next_id += 1;
        let id = format!("rule-{}", st.next_id);
        st.rules.push((id.clone(), rule));
        Ok(id)
    }

    async fn delete_rule(&self, id: &str) -> Result<(), crate::NetworkError> {
        let mut st = self.inner.lock().unwrap();
        let before = st.rules.len();
        st.rules.retain(|(rid, _)| rid != id);
        if st.rules.len() == before {
            return Err(crate::NetworkError::InterfaceNotFound(format!(
                "规则不存在: {id}"
            )));
        }
        Ok(())
    }

    async fn dry_run(&self, rule: &FirewallRule) -> Result<(), crate::NetworkError> {
        rule.validate()?;
        if let Some(msg) = &self.inner.lock().unwrap().dry_run_error_msg {
            return Err(crate::NetworkError::RuleInvalid(msg.clone()));
        }
        Ok(())
    }

    async fn add_nat(&self, rule: NatRule) -> Result<(), crate::NetworkError> {
        self.inner.lock().unwrap().nats.push(rule);
        Ok(())
    }

    async fn delete_nat(&self, rule: &NatRule) -> Result<(), crate::NetworkError> {
        let mut st = self.inner.lock().unwrap();
        let before = st.nats.len();
        st.nats.retain(|n| n != rule);
        if st.nats.len() == before {
            return Err(crate::NetworkError::InterfaceNotFound(
                "NAT 规则不存在".into(),
            ));
        }
        Ok(())
    }
}

// ============================================================================
// MockDhcpServer
// ============================================================================

/// 内存版 `DhcpServer`：维护租约与范围配置。
#[derive(Debug, Default)]
pub struct MockDhcpServer {
    inner: Mutex<MockDhcpState>,
}

#[derive(Debug, Default)]
struct MockDhcpState {
    leases: Vec<DhcpLease>,
    static_leases: Vec<(String, IpAddr)>,
    range: Option<(IpAddr, IpAddr, IpAddr, Vec<IpAddr>)>,
}

impl MockDhcpServer {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn lease_count(&self) -> usize {
        self.inner.lock().unwrap().leases.len()
    }
    pub fn static_leases_snapshot(&self) -> Vec<(String, IpAddr)> {
        self.inner.lock().unwrap().static_leases.clone()
    }
}

#[allow(async_fn_in_trait)]
impl crate::services::DhcpServer for MockDhcpServer {
    async fn list_leases(&self) -> Result<Vec<DhcpLease>, crate::NetworkError> {
        Ok(self.inner.lock().unwrap().leases.clone())
    }

    async fn add_static_lease(&self, mac: String, ip: IpAddr) -> Result<(), crate::NetworkError> {
        DhcpLease::validate_mac(&mac)?;
        self.inner.lock().unwrap().static_leases.push((mac, ip));
        Ok(())
    }

    async fn remove_static_lease(&self, mac: &str) -> Result<(), crate::NetworkError> {
        let mut st = self.inner.lock().unwrap();
        let before = st.static_leases.len();
        st.static_leases.retain(|(m, _)| m != mac);
        if st.static_leases.len() == before {
            return Err(crate::NetworkError::InterfaceNotFound(format!(
                "静态租约不存在: {mac}"
            )));
        }
        Ok(())
    }

    async fn set_range(
        &self,
        start: IpAddr,
        end: IpAddr,
        gateway: IpAddr,
        dns: Vec<IpAddr>,
    ) -> Result<(), crate::NetworkError> {
        self.inner.lock().unwrap().range = Some((start, end, gateway, dns));
        Ok(())
    }
}

// ============================================================================
// MockDnsServer
// ============================================================================

/// 内存版 `DnsServer`：维护 `(name, record)` 列表。
#[derive(Debug, Default)]
pub struct MockDnsServer {
    inner: Mutex<Vec<(String, DnsRecord)>>,
}

impl MockDnsServer {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn record_count(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

#[allow(async_fn_in_trait)]
impl crate::services::DnsServer for MockDnsServer {
    async fn list_records(&self) -> Result<Vec<(String, DnsRecord)>, crate::NetworkError> {
        Ok(self.inner.lock().unwrap().clone())
    }

    async fn add_record(&self, name: String, record: DnsRecord) -> Result<(), crate::NetworkError> {
        self.inner.lock().unwrap().push((name, record));
        Ok(())
    }

    async fn delete_record(
        &self,
        name: &str,
        record: &DnsRecord,
    ) -> Result<(), crate::NetworkError> {
        let mut st = self.inner.lock().unwrap();
        let before = st.len();
        st.retain(|(n, r)| !(n == name && r == record));
        if st.len() == before {
            return Err(crate::NetworkError::InterfaceNotFound(format!(
                "DNS 记录不存在: {name}"
            )));
        }
        Ok(())
    }
}

// ============================================================================
// MockPxeServer
// ============================================================================

/// 内存版 `PxeServer`：维护 boot file 配置。
#[derive(Debug, Default)]
pub struct MockPxeServer {
    inner: Mutex<MockPxeState>,
}

#[derive(Debug, Default)]
struct MockPxeState {
    boot_file: Option<String>,
    next_server: Option<IpAddr>,
}

impl MockPxeServer {
    pub fn new() -> Self {
        Self::default()
    }
}

#[allow(async_fn_in_trait)]
impl crate::services::PxeServer for MockPxeServer {
    async fn set_boot_file(
        &self,
        filename: String,
        next_server: IpAddr,
    ) -> Result<(), crate::NetworkError> {
        let mut st = self.inner.lock().unwrap();
        st.boot_file = Some(filename);
        st.next_server = Some(next_server);
        Ok(())
    }

    async fn status(&self) -> Result<PxeStatus, crate::NetworkError> {
        let st = self.inner.lock().unwrap();
        let state = if st.boot_file.is_some() {
            PxeState::Enabled
        } else {
            PxeState::Disabled
        };
        Ok(PxeStatus {
            state,
            boot_file: st.boot_file.clone(),
            next_server: st.next_server,
        })
    }
}

// ============================================================================
// 测试（mock 自身的健全性测试）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firewall::{Firewall, FirewallAction, Protocol};
    use crate::interface::{InterfaceType, IpCidr, NetworkManager};
    use crate::services::{DhcpServer, DnsServer, PxeServer};
    use chrono::Utc;
    use std::net::Ipv4Addr;

    fn ip(a: &str) -> IpAddr {
        a.parse().unwrap()
    }

    #[tokio::test]
    async fn mock_network_crud() {
        let m = MockNetworkManager::new().with_interface(
            Interface::new(InterfaceId::new("eth0"), InterfaceType::Physical)
                .with_state(crate::interface::IfState::Up),
        );
        // list
        assert_eq!(m.list_interfaces().await.unwrap().len(), 1);
        // get
        assert!(m.get_interface(&InterfaceId::new("eth0")).await.is_ok());
        assert!(matches!(
            m.get_interface(&InterfaceId::new("nope"))
                .await
                .unwrap_err(),
            crate::NetworkError::InterfaceNotFound(_)
        ));
        // create vlan
        let v = m
            .create_vlan(&InterfaceId::new("eth0"), 100, InterfaceId::new("vlan100"))
            .await
            .unwrap();
        assert_eq!(v.ty, InterfaceType::Vlan);
        // create vlan bad vid
        assert!(m
            .create_vlan(&InterfaceId::new("eth0"), 5000, InterfaceId::new("x"))
            .await
            .is_err());
        // up recorded
        m.up(&InterfaceId::new("eth0")).await.unwrap();
        assert!(m.recorded_ops().iter().any(|o| o == "up:eth0"));
        // delete physical rejected
        assert!(m.delete_interface(&InterfaceId::new("eth0")).await.is_err());
        // delete vlan ok
        m.delete_interface(&InterfaceId::new("vlan100"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn mock_firewall_dry_run_and_add() {
        let fw = MockFirewall::new();
        let good = FirewallRule {
            action: FirewallAction::Allow,
            protocol: Protocol::Tcp,
            src_addr: None,
            src_port: None,
            dst_addr: None,
            dst_port: Some("80".into()),
            target_port: None,
            description: None,
        };
        let id = fw.add_rule(good).await.unwrap();
        assert_eq!(fw.rule_count(), 1);
        fw.delete_rule(&id).await.unwrap();
        assert_eq!(fw.rule_count(), 0);
    }

    #[tokio::test]
    async fn mock_firewall_dry_run_error_blocks_add() {
        let fw = MockFirewall::new().with_dry_run_error("风险");
        let good = FirewallRule {
            action: FirewallAction::Allow,
            protocol: Protocol::Any,
            src_addr: None,
            src_port: None,
            dst_addr: None,
            dst_port: None,
            target_port: None,
            description: None,
        };
        assert!(fw.add_rule(good).await.is_err());
        assert_eq!(fw.rule_count(), 0);
    }

    #[tokio::test]
    async fn mock_dhcp_static_lease_mac_validated() {
        let d = MockDhcpServer::new();
        assert!(d
            .add_static_lease("aa:bb:cc:dd:ee:ff".into(), ip("10.0.0.5"))
            .await
            .is_ok());
        assert_eq!(d.lease_count(), 0); // 静态租约不计入 leases 列表
        assert_eq!(d.static_leases_snapshot().len(), 1);
        // bad mac
        assert!(d
            .add_static_lease("nope".into(), ip("10.0.0.6"))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn mock_dns_records() {
        let dns = MockDnsServer::new();
        let rec = DnsRecord::a(Ipv4Addr::new(192, 168, 1, 1));
        dns.add_record("os.local".into(), rec.clone())
            .await
            .unwrap();
        assert_eq!(dns.record_count(), 1);
        dns.delete_record("os.local", &rec).await.unwrap();
        assert_eq!(dns.record_count(), 0);
    }

    #[tokio::test]
    async fn mock_pxe_status() {
        let pxe = MockPxeServer::new();
        assert_eq!(pxe.status().await.unwrap().state, PxeState::Disabled);
        pxe.set_boot_file("pxelinux.0".into(), ip("10.0.0.1"))
            .await
            .unwrap();
        let s = pxe.status().await.unwrap();
        assert_eq!(s.state, PxeState::Enabled);
        assert_eq!(s.boot_file.as_deref(), Some("pxelinux.0"));
    }

    // 覆盖 DhcpLease / IpCidr 在 mock 路径的可达性
    #[test]
    fn dhcp_lease_validate_mac_reachable() {
        assert!(DhcpLease::validate_mac("aa:bb:cc:dd:ee:ff").is_ok());
        assert!(DhcpLease::validate_mac("AA-BB-CC-DD-EE-FF").is_ok());
        assert!(DhcpLease::validate_mac("bad").is_err());
        let _ = DhcpLease {
            mac: "aa:bb:cc:dd:ee:ff".into(),
            ip: ip("10.0.0.1"),
            hostname: None,
            expiry: Utc::now(),
        };
        let _ = IpCidr::new(ip("10.0.0.1"), 24);
    }
}

// ============================================================================
// —— rdma-agent 的 Mock(合并自 rdma 分支) ——
// ============================================================================
use crate::dpu::{DpuBackend, DpuModel, FwStatus, NvmeofOffloadConfig, PowerAction};
use crate::rdma::{RdmaCapability, RdmaDevice, RdmaManager};
pub struct MockRdmaManager {
    /// `list_devices` / `detect_capability` 返回的设备列表
    devices: Mutex<Vec<RdmaDevice>>,
    /// `configure_ipoib` 是否被调用（测试可断言）
    pub ipoib_calls: Mutex<Vec<(String, IpCidr)>>,
}

impl Default for MockRdmaManager {
    fn default() -> Self {
        Self {
            devices: Mutex::new(Vec::new()),
            ipoib_calls: Mutex::new(Vec::new()),
        }
    }
}

impl MockRdmaManager {
    /// 构造一个默认 mock（available=false）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置 `list_devices` 返回的设备列表（同时驱动 `detect_capability`）。
    pub fn with_devices(self, devices: Vec<RdmaDevice>) -> Self {
        *self.devices.lock().expect("mock poisoned") = devices;
        self
    }
}

impl RdmaManager for MockRdmaManager {
    async fn list_devices(&self) -> Result<Vec<RdmaDevice>, crate::NetworkError> {
        Ok(self.devices.lock().expect("mock poisoned").clone())
    }

    async fn detect_capability(&self) -> Result<RdmaCapability, crate::NetworkError> {
        let devs = self.devices.lock().expect("mock poisoned").clone();
        let names: Vec<String> = devs.iter().map(|d| d.name.clone()).collect();
        let available = !names.is_empty();
        let ty = if available {
            let mut it = devs.iter().map(|d| d.ty);
            let first = it.next();
            first.filter(|f| it.all(|t| t == *f))
        } else {
            None
        };
        Ok(RdmaCapability {
            available,
            devices: names,
            ty,
        })
    }

    async fn configure_ipoib(&self, dev: &str, addr: IpCidr) -> Result<(), crate::NetworkError> {
        self.ipoib_calls
            .lock()
            .expect("mock poisoned")
            .push((dev.to_string(), addr));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// MockDpuBackend
// ---------------------------------------------------------------------------

/// Mock DPU 后端——纯内存、确定性，供下游测试注入。
///
/// 默认：`list_dp_us` 返回空、卸载 / 电源操作返回 `Ok(())`、固件状态返回 "unknown"。
/// 通过 `with_dp_us` / `with_fw_status` 覆盖返回值。
pub struct MockDpuBackend {
    /// `list_dp_us` 返回的 DPU 列表
    dp_us: Mutex<Vec<DpuModel>>,
    /// `redfish_firmware_status` 返回的状态
    fw_status: Mutex<FwStatus>,
    /// 是否调用过 offload（测试断言用）
    pub offload_calls: Mutex<Vec<String>>,
    /// 是否调用过 redfish_power（测试断言用）
    pub power_calls: Mutex<Vec<(String, PowerAction)>>,
}

impl Default for MockDpuBackend {
    fn default() -> Self {
        Self {
            dp_us: Mutex::new(Vec::new()),
            fw_status: Mutex::new(FwStatus {
                version: String::new(),
                health: "unknown".into(),
                update_available: false,
            }),
            offload_calls: Mutex::new(Vec::new()),
            power_calls: Mutex::new(Vec::new()),
        }
    }
}

impl MockDpuBackend {
    /// 构造一个默认 mock（空 / 降级返回）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置 `list_dp_us` 返回的 DPU 列表。
    pub fn with_dp_us(self, dp_us: Vec<DpuModel>) -> Self {
        *self.dp_us.lock().expect("mock poisoned") = dp_us;
        self
    }

    /// 设置 `redfish_firmware_status` 返回的状态。
    pub fn with_fw_status(self, status: FwStatus) -> Self {
        *self.fw_status.lock().expect("mock poisoned") = status;
        self
    }
}

impl DpuBackend for MockDpuBackend {
    async fn list_dp_us(&self) -> Result<Vec<DpuModel>, crate::NetworkError> {
        Ok(self.dp_us.lock().expect("mock poisoned").clone())
    }

    async fn offload_nvmeof(
        &self,
        dpu: &str,
        _config: NvmeofOffloadConfig,
    ) -> Result<(), crate::NetworkError> {
        self.offload_calls
            .lock()
            .expect("mock poisoned")
            .push(dpu.to_string());
        Ok(())
    }

    async fn offload_ovs(&self, dpu: &str) -> Result<(), crate::NetworkError> {
        self.offload_calls
            .lock()
            .expect("mock poisoned")
            .push(dpu.to_string());
        Ok(())
    }

    async fn redfish_power(
        &self,
        dpu: &str,
        action: PowerAction,
    ) -> Result<(), crate::NetworkError> {
        self.power_calls
            .lock()
            .expect("mock poisoned")
            .push((dpu.to_string(), action));
        Ok(())
    }

    async fn redfish_firmware_status(&self, _dpu: &str) -> Result<FwStatus, crate::NetworkError> {
        Ok(self.fw_status.lock().expect("mock poisoned").clone())
    }
}

// ---------------------------------------------------------------------------
// rdma Mock 单元测试（mock feature 下，合并自 rdma 分支）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod rdma_tests {
    use super::*;
    use crate::dpu::DpuMode;
    use crate::rdma::RdmaType;
    use std::net::IpAddr;

    #[tokio::test]
    async fn mock_rdma_default_degrades() {
        let m = MockRdmaManager::new();
        let cap = m.detect_capability().await.unwrap();
        assert!(!cap.available);
        assert!(m.list_devices().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn mock_rdma_with_devices() {
        let dev = RdmaDevice {
            name: "mlx5_0".into(),
            ty: RdmaType::InfiniBand,
            state: "PORT_ACTIVE".into(),
            ports: vec![],
        };
        let m = MockRdmaManager::new().with_devices(vec![dev]);
        let cap = m.detect_capability().await.unwrap();
        assert!(cap.available);
        assert_eq!(cap.devices, vec!["mlx5_0".to_string()]);
        assert_eq!(cap.ty, Some(RdmaType::InfiniBand));
    }

    #[tokio::test]
    async fn mock_rdma_configure_ipoib_records() {
        let m = MockRdmaManager::new();
        let cidr = IpCidr::new("192.168.1.1".parse::<IpAddr>().unwrap(), 24);
        m.configure_ipoib("ib0", cidr).await.unwrap();
        let calls = m.ipoib_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "ib0");
    }

    #[tokio::test]
    async fn mock_dpu_default_degrades() {
        let b = MockDpuBackend::new();
        assert!(b.list_dp_us().await.unwrap().is_empty());
        let fw = b.redfish_firmware_status("dpu0").await.unwrap();
        assert_eq!(fw.health, "unknown");
        b.offload_nvmeof(
            "dpu0",
            NvmeofOffloadConfig {
                nqn: "nqn".into(),
                namespaces: vec![],
                listen_addr: "0.0.0.0".parse().unwrap(),
                port: 4420,
            },
        )
        .await
        .unwrap();
        assert!(b
            .offload_calls
            .lock()
            .unwrap()
            .contains(&"dpu0".to_string()));
    }

    #[tokio::test]
    async fn mock_dpu_with_dp_us_and_fw() {
        let model = DpuModel {
            vendor: "NVIDIA".into(),
            model: "BlueField-3".into(),
            firmware: "24.31".into(),
            mgmt_addr: "192.168.1.50".parse().unwrap(),
            mode: DpuMode::OutOfBand,
        };
        let fw = FwStatus {
            version: "24.31".into(),
            health: "ok".into(),
            update_available: true,
        };
        let b = MockDpuBackend::new()
            .with_dp_us(vec![model])
            .with_fw_status(fw);
        let list = b.list_dp_us().await.unwrap();
        assert_eq!(list.len(), 1);
        let st = b.redfish_firmware_status("dpu0").await.unwrap();
        assert!(st.update_available);
    }
}

// ============================================================================
// 覆盖率补测：补全 mock 各 trait 实现的未覆盖分支（CRUD 边界 + 不存在路径）
// ============================================================================

#[cfg(test)]
mod coverage_tests {
    use super::*;
    use crate::firewall::{Firewall, FirewallAction, NatRule, Protocol};
    use crate::interface::{BondMode, IfState, InterfaceType, IpCidr, NetworkManager};
    use crate::services::{DhcpServer, DnsServer};
    use crate::RdmaType;
    use std::net::Ipv4Addr;

    fn ip(a: &str) -> IpAddr {
        a.parse().unwrap()
    }

    // —— MockNetworkManager 边界 ——

    #[tokio::test]
    async fn mock_net_with_interfaces_builder() {
        // with_interfaces（多接口构造器）+ list 回读
        let ifaces = vec![
            Interface::new(InterfaceId::new("eth0"), InterfaceType::Physical),
            Interface::new(InterfaceId::new("eth1"), InterfaceType::Physical),
        ];
        let m = MockNetworkManager::new().with_interfaces(ifaces);
        assert_eq!(m.list_interfaces().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn mock_net_create_bridge_and_bond_records() {
        let m = MockNetworkManager::new();
        let br = m
            .create_bridge(InterfaceId::new("br0"))
            .await
            .expect("create_bridge 应成功");
        assert_eq!(br.ty, InterfaceType::Bridge);
        // 非法名 → RuleInvalid
        assert!(m
            .create_bridge(InterfaceId::new("x".repeat(20)))
            .await
            .is_err());
        // bond（含 slaves，覆盖 with_parent 分支）
        let bond = m
            .create_bond(
                InterfaceId::new("bond0"),
                BondMode::ActiveBackup,
                vec![InterfaceId::new("eth0"), InterfaceId::new("eth1")],
            )
            .await
            .expect("create_bond 应成功");
        assert_eq!(bond.ty, InterfaceType::Bond);
        assert_eq!(bond.parent.as_ref().map(|p| p.as_str()), Some("eth0"));
        // 空 slaves bond（不进 with_parent 分支）
        let bond2 = m
            .create_bond(InterfaceId::new("bond1"), BondMode::BalanceRr, vec![])
            .await
            .unwrap();
        assert!(bond2.parent.is_none());
        // bond 非法名
        assert!(m
            .create_bond(InterfaceId::new(""), BondMode::Broadcast, vec![])
            .await
            .is_err());
    }

    #[tokio::test]
    async fn mock_net_up_down_and_set_address() {
        let m = MockNetworkManager::new().with_interface(
            Interface::new(InterfaceId::new("eth0"), InterfaceType::Physical)
                .with_state(IfState::Up),
        );
        // down 触发记录
        m.down(&InterfaceId::new("eth0")).await.unwrap();
        assert!(m.recorded_ops().iter().any(|o| o == "down:eth0"));
        let after = m.get_interface(&InterfaceId::new("eth0")).await.unwrap();
        assert_eq!(after.state, IfState::Down);
        // set_address 命中
        m.set_address(
            &InterfaceId::new("eth0"),
            vec![IpCidr::new(ip("10.0.0.1"), 24)],
        )
        .await
        .unwrap();
        assert_eq!(
            m.get_interface(&InterfaceId::new("eth0"))
                .await
                .unwrap()
                .addrs
                .len(),
            1
        );
        // set_address 接口不存在
        assert!(matches!(
            m.set_address(
                &InterfaceId::new("nope"),
                vec![IpCidr::new(ip("10.0.0.1"), 24)],
            )
            .await
            .unwrap_err(),
            crate::NetworkError::InterfaceNotFound(_)
        ));
        // down 不存在
        assert!(matches!(
            m.down(&InterfaceId::new("nope")).await.unwrap_err(),
            crate::NetworkError::InterfaceNotFound(_)
        ));
        // delete 虚拟接口（VLAN）成功并记录
        let m2 = MockNetworkManager::new();
        m2.create_vlan(&InterfaceId::new("eth0"), 100, InterfaceId::new("vlan100"))
            .await
            .unwrap();
        m2.delete_interface(&InterfaceId::new("vlan100"))
            .await
            .unwrap();
        assert!(m2.recorded_ops().iter().any(|o| o == "delete:vlan100"));
        // delete 不存在
        assert!(m2
            .delete_interface(&InterfaceId::new("nope"))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn mock_net_delete_bridge_and_loopback_rejected() {
        let m = MockNetworkManager::new()
            .with_interface(Interface::new(
                InterfaceId::new("br0"),
                InterfaceType::Bridge,
            ))
            .with_interface(Interface::new(
                InterfaceId::new("lo"),
                InterfaceType::Loopback,
            ));
        // bridge 可删
        m.delete_interface(&InterfaceId::new("br0")).await.unwrap();
        // loopback 不可删
        assert!(matches!(
            m.delete_interface(&InterfaceId::new("lo"))
                .await
                .unwrap_err(),
            crate::NetworkError::RuleInvalid(_)
        ));
    }

    // —— MockFirewall 边界 ——

    #[tokio::test]
    async fn mock_firewall_nat_crud_and_snapshot() {
        let fw = MockFirewall::new();
        let nat = NatRule {
            protocol: Protocol::Tcp,
            src: ip("10.0.0.1"),
            translated_addr: ip("203.0.113.1"),
            translated_port: Some(8443),
        };
        // 空快照
        assert!(fw.nat_snapshot().is_empty());
        fw.add_nat(nat.clone()).await.unwrap();
        assert_eq!(fw.nat_snapshot().len(), 1);
        // delete 命中
        fw.delete_nat(&nat).await.unwrap();
        assert!(fw.nat_snapshot().is_empty());
        // delete 不存在 → InterfaceNotFound
        assert!(matches!(
            fw.delete_nat(&nat).await.unwrap_err(),
            crate::NetworkError::InterfaceNotFound(_)
        ));
    }

    #[tokio::test]
    async fn mock_firewall_delete_unknown_rule() {
        let fw = MockFirewall::new();
        assert!(matches!(
            fw.delete_rule("nope").await.unwrap_err(),
            crate::NetworkError::InterfaceNotFound(_)
        ));
    }

    #[tokio::test]
    async fn mock_firewall_list_rules_returns_added() {
        // list_rules（含多条）回读路径
        let fw = MockFirewall::new();
        let r1 = FirewallRule {
            action: FirewallAction::Allow,
            protocol: Protocol::Tcp,
            src_addr: None,
            src_port: None,
            dst_addr: None,
            dst_port: Some("22".into()),
            target_port: None,
            description: None,
        };
        let r2 = FirewallRule {
            action: FirewallAction::Deny,
            protocol: Protocol::Udp,
            src_addr: None,
            src_port: None,
            dst_addr: None,
            dst_port: Some("53".into()),
            target_port: None,
            description: None,
        };
        assert!(fw.list_rules().await.unwrap().is_empty());
        fw.add_rule(r1).await.unwrap();
        fw.add_rule(r2).await.unwrap();
        let rules = fw.list_rules().await.unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].action, FirewallAction::Allow);
        assert_eq!(rules[1].action, FirewallAction::Deny);
    }

    #[tokio::test]
    async fn mock_firewall_dry_run_invalid_rule() {
        let fw = MockFirewall::new();
        // Redirect 无 target_port → validate 失败 → dry_run 报 RuleInvalid
        let bad = FirewallRule {
            action: FirewallAction::Redirect,
            protocol: Protocol::Tcp,
            src_addr: None,
            src_port: None,
            dst_addr: None,
            dst_port: Some("80".into()),
            target_port: None,
            description: None,
        };
        assert!(matches!(
            fw.dry_run(&bad).await.unwrap_err(),
            crate::NetworkError::RuleInvalid(_)
        ));
    }

    // —— MockDhcpServer 边界 ——

    #[tokio::test]
    async fn mock_dhcp_full_lifecycle() {
        let d = MockDhcpServer::new();
        // list_leases 空
        assert!(d.list_leases().await.unwrap().is_empty());
        // set_range
        d.set_range(
            ip("10.0.0.100"),
            ip("10.0.0.200"),
            ip("10.0.0.1"),
            vec![ip("8.8.8.8")],
        )
        .await
        .unwrap();
        // add + remove 静态租约
        d.add_static_lease("aa:bb:cc:dd:ee:ff".into(), ip("10.0.0.5"))
            .await
            .unwrap();
        d.remove_static_lease("aa:bb:cc:dd:ee:ff").await.unwrap();
        // remove 不存在
        assert!(matches!(
            d.remove_static_lease("aa:bb:cc:dd:ee:ff")
                .await
                .unwrap_err(),
            crate::NetworkError::InterfaceNotFound(_)
        ));
        // bad mac add
        assert!(d
            .add_static_lease("nope".into(), ip("10.0.0.6"))
            .await
            .is_err());
    }

    // —— MockDnsServer 边界 ——

    #[tokio::test]
    async fn mock_dns_delete_unknown_record() {
        let dns = MockDnsServer::new();
        let rec = DnsRecord::a(Ipv4Addr::new(192, 168, 1, 1));
        // list 空
        assert!(dns.list_records().await.unwrap().is_empty());
        // delete 不存在
        assert!(matches!(
            dns.delete_record("os.local", &rec).await.unwrap_err(),
            crate::NetworkError::InterfaceNotFound(_)
        ));
    }

    #[tokio::test]
    async fn mock_dns_multiple_records_mixed_types() {
        let dns = MockDnsServer::new();
        let a = DnsRecord::a(Ipv4Addr::new(10, 0, 0, 1));
        let cname = DnsRecord::cname("os.example.com");
        let mx = DnsRecord::mx("mail.example.com", 10);
        dns.add_record("host1".into(), a.clone()).await.unwrap();
        dns.add_record("alias".into(), cname.clone()).await.unwrap();
        dns.add_record("mail".into(), mx.clone()).await.unwrap();
        assert_eq!(dns.record_count(), 3);
        // delete 仅匹配的条目（精确 (name, record) 对）
        dns.delete_record("alias", &cname).await.unwrap();
        assert_eq!(dns.record_count(), 2);
        // delete 同名不同记录 → 不命中
        assert!(matches!(
            dns.delete_record("host1", &mx).await.unwrap_err(),
            crate::NetworkError::InterfaceNotFound(_)
        ));
    }

    // —— MockRdmaManager / MockDpuBackend 边界 ——

    #[tokio::test]
    async fn mock_rdma_mixed_types_yields_none_ty() {
        // 混合 IB + RoCE → ty=None（保守降级分支）
        let devs = vec![
            RdmaDevice {
                name: "mlx5_0".into(),
                ty: RdmaType::InfiniBand,
                state: "PORT_ACTIVE".into(),
                ports: vec![],
            },
            RdmaDevice {
                name: "roce0".into(),
                ty: RdmaType::RoceV2,
                state: "PORT_ACTIVE".into(),
                ports: vec![],
            },
        ];
        let m = MockRdmaManager::new().with_devices(devs);
        let cap = m.detect_capability().await.unwrap();
        assert!(cap.available);
        assert!(cap.ty.is_none(), "混合类型应返回 ty=None");
        assert_eq!(cap.devices.len(), 2);
        // list_devices 回读
        assert_eq!(m.list_devices().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn mock_dpu_offload_ovs_and_power_recorded() {
        let b = MockDpuBackend::new();
        b.offload_ovs("dpu0").await.unwrap();
        b.redfish_power("dpu0", PowerAction::On).await.unwrap();
        // offload_calls 含 dpu0（nvmeof + ovs 共用列表）
        assert!(b
            .offload_calls
            .lock()
            .unwrap()
            .contains(&"dpu0".to_string()));
        // power_calls 含 (dpu0, On)
        assert!(b
            .power_calls
            .lock()
            .unwrap()
            .contains(&("dpu0".to_string(), PowerAction::On)));
    }

    #[tokio::test]
    async fn mock_dpu_all_power_actions() {
        let b = MockDpuBackend::new();
        b.redfish_power("dpu0", PowerAction::Off).await.unwrap();
        b.redfish_power("dpu0", PowerAction::Reset).await.unwrap();
        b.redfish_power("dpu0", PowerAction::GracefulShutdown)
            .await
            .unwrap();
        let calls = b.power_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 3);
    }

    #[tokio::test]
    async fn mock_dpu_offload_nvmeof_records() {
        let b = MockDpuBackend::new();
        b.offload_nvmeof(
            "bf3-0",
            NvmeofOffloadConfig {
                nqn: "nqn.test".into(),
                namespaces: vec!["ns1".into()],
                listen_addr: "0.0.0.0".parse().unwrap(),
                port: 4420,
            },
        )
        .await
        .unwrap();
        assert!(b
            .offload_calls
            .lock()
            .unwrap()
            .contains(&"bf3-0".to_string()));
    }
}
