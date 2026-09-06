//! 接口管理：物理网卡 / VLAN / 桥 / 绑定
//!
//! 决策依据：规划文档 §3.9 —— NetworkManager 统一管理接口生命周期。
//! 这里仅定义 `Interface` 数据模型与 `NetworkManager` trait（async，数据路径）。

use serde::{Deserialize, Serialize};
use std::net::IpAddr;

// ----------------------------------------------------------------------------
// 标识与基础类型
// ----------------------------------------------------------------------------

/// 接口 ID（newtype，如 "eth0" / "br0" / "vlan100"）
///
/// 复用 newtype 风格（与 os-core::ids 一致）：独立类型，编译期防混淆。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InterfaceId(pub String);

impl InterfaceId {
    /// 从任意字符串构造
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    /// 取字符串切片
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for InterfaceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for InterfaceId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// 接口类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterfaceType {
    /// 物理网卡
    Physical,
    /// VLAN 子接口
    Vlan,
    /// 软件桥（bridge）
    Bridge,
    /// 链路聚合（bond）
    Bond,
    /// 回环
    Loopback,
}

/// 接口运行状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IfState {
    /// 已启用（up）
    Up,
    /// 已禁用（down）
    Down,
}

/// 绑定模式（bond mode）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BondMode {
    /// 主备（active-backup）
    ActiveBackup,
    /// 轮询（balance-rr，mode 0）
    BalanceRr,
    /// LACP（802.3ad，mode 4）
    Lacp,
    /// 广播（broadcast，mode 3）
    Broadcast,
}

// ----------------------------------------------------------------------------
// IP 地址 / CIDR
// ----------------------------------------------------------------------------

/// IPv4/IPv6 CIDR 地址（地址 + 前缀长度）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpCidr {
    /// 地址
    pub addr: IpAddr,
    /// 前缀长度（如 24 / 64）
    pub prefix: u8,
}

impl IpCidr {
    /// 构造一个 CIDR（不做合法性校验，保留与原契约兼容的快速构造路径）。
    pub fn new(addr: IpAddr, prefix: u8) -> Self {
        Self { addr, prefix }
    }

    /// 解析 CIDR 字符串（如 `"192.168.1.0/24"`、`"fd00::1/64"`）。
    ///
    /// 规则：
    /// - 必须含且仅含一个 `/` 分隔符；
    /// - 地址部分须为合法 IPv4/IPv6；
    /// - 前缀长度须为十进制整数且在地址族合法范围内（IPv4: 0..=32，IPv6: 0..=128）；
    /// - 前缀位之后的主机位允许非零（不强制网络化，交由 `network()` 处理）。
    ///
    /// 失败返回 `NetworkError::Internal`（携带原因），不 panic。
    pub fn parse(s: &str) -> Result<Self, crate::NetworkError> {
        let (addr_part, prefix_part) = s
            .split_once('/')
            .ok_or_else(|| crate::NetworkError::Internal(format!("非法 CIDR（缺 `/`）: {s}")))?;
        let addr: IpAddr = addr_part
            .parse()
            .map_err(|e| crate::NetworkError::Internal(format!("非法 IP `{addr_part}`: {e}")))?;
        let prefix: u8 = prefix_part
            .parse::<u32>()
            .map_err(|e| crate::NetworkError::Internal(format!("非法前缀 `{prefix_part}`: {e}")))?
            as u8;
        let cidr = Self::new(addr, prefix);
        cidr.validate()?;
        Ok(cidr)
    }

    /// 校验前缀长度是否在地址族合法范围内。
    ///
    /// - IPv4: 0..=32；IPv6: 0..=128。
    ///
    /// 越界返回 `NetworkError::RuleInvalid`（复用"非法输入"语义）。
    pub fn validate(&self) -> Result<(), crate::NetworkError> {
        let max = match self.addr {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        if self.prefix > max {
            return Err(crate::NetworkError::RuleInvalid(format!(
                "前缀长度 {} 超出地址族上限 {}（{}）",
                self.prefix, max, self.addr
            )));
        }
        Ok(())
    }

    /// 返回地址族允许的最大前缀长度（IPv4→32，IPv6→128）。
    pub const fn max_prefix(addr: IpAddr) -> u8 {
        match addr {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        }
    }

    /// 判断是否为 IPv4 CIDR。
    pub const fn is_ipv4(&self) -> bool {
        matches!(self.addr, IpAddr::V4(_))
    }

    /// 判断是否为 IPv6 CIDR。
    pub const fn is_ipv6(&self) -> bool {
        matches!(self.addr, IpAddr::V6(_))
    }
}

impl std::str::FromStr for IpCidr {
    type Err = crate::NetworkError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl std::fmt::Display for IpCidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

// ----------------------------------------------------------------------------
// VLAN 校验
// ----------------------------------------------------------------------------

/// VLAN ID 合法范围（IEEE 802.1Q：0..=4095，其中 0/4095 保留，可用 1..=4094）。
pub const VLAN_ID_MIN: u16 = 1;
/// VLAN ID 合法上限（含）。
pub const VLAN_ID_MAX: u16 = 4094;

/// 校验 VLAN ID 是否落在合法范围（1..=4094）。
///
/// 越界返回 `NetworkError::RuleInvalid`。供 `NetworkManager::create_vlan`
/// 实现及调用方在落 netlink 前做前置校验。
pub fn validate_vlan_id(vid: u16) -> Result<(), crate::NetworkError> {
    if !(VLAN_ID_MIN..=VLAN_ID_MAX).contains(&vid) {
        return Err(crate::NetworkError::RuleInvalid(format!(
            "VLAN ID {vid} 越界（合法范围 {}..={}）",
            VLAN_ID_MIN, VLAN_ID_MAX
        )));
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// 接口模型
// ----------------------------------------------------------------------------

/// 网络接口（统一表示物理/VLAN/桥/绑定）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Interface {
    /// 接口 ID（如 "eth0"）
    pub id: InterfaceId,
    /// 接口类型
    pub ty: InterfaceType,
    /// MTU
    pub mtu: u16,
    /// MAC 地址（字符串形式，如 "aa:bb:cc:dd:ee:ff"）
    pub mac: Option<String>,
    /// 配置的地址列表（CIDR）
    pub addrs: Vec<IpCidr>,
    /// 运行状态
    pub state: IfState,
    /// 父接口（VLAN 的物理父接口 / bond 的主接口；物理接口为 None）
    pub parent: Option<InterfaceId>,
}

/// MTU 合法范围（以太网最小 68 字节 IPv4 最小 MTU；上限取常见 jumbo frame 上界）。
pub const MTU_MIN: u16 = 68;
/// MTU 上限（jumbo frame 友好上界；硬件可支持更大但保守取值）。
pub const MTU_MAX: u16 = 9216;

impl Interface {
    /// 构造一个处于 `Down` 状态、无地址的空接口（便于实现填充）。
    pub fn new(id: InterfaceId, ty: InterfaceType) -> Self {
        Self {
            id,
            ty,
            mtu: 1500,
            mac: None,
            addrs: Vec::new(),
            state: IfState::Down,
            parent: None,
        }
    }

    /// 链式设置 MTU。
    pub fn with_mtu(mut self, mtu: u16) -> Self {
        self.mtu = mtu;
        self
    }

    /// 链式设置 MAC。
    pub fn with_mac(mut self, mac: impl Into<String>) -> Self {
        self.mac = Some(mac.into());
        self
    }

    /// 链式追加地址。
    pub fn with_addr(mut self, addr: IpCidr) -> Self {
        self.addrs.push(addr);
        self
    }

    /// 链式设置运行状态。
    pub fn with_state(mut self, state: IfState) -> Self {
        self.state = state;
        self
    }

    /// 链式设置父接口。
    pub fn with_parent(mut self, parent: InterfaceId) -> Self {
        self.parent = Some(parent);
        self
    }

    /// 校验 MTU 是否在合法范围（68..=9216）。
    pub fn validate_mtu(mtu: u16) -> Result<(), crate::NetworkError> {
        if !(MTU_MIN..=MTU_MAX).contains(&mtu) {
            return Err(crate::NetworkError::RuleInvalid(format!(
                "MTU {mtu} 越界（合法范围 {}..={}）",
                MTU_MIN, MTU_MAX
            )));
        }
        Ok(())
    }

    /// 校验接口名长度（Linux IFNAMSIZ=16，含结尾 `\0`，故可用名 ≤15 字节）。
    pub fn validate_name(name: &str) -> Result<(), crate::NetworkError> {
        if name.is_empty() || name.len() > 15 {
            return Err(crate::NetworkError::RuleInvalid(format!(
                "接口名 `{name}` 非法（长度须 1..=15）"
            )));
        }
        Ok(())
    }
}

// ----------------------------------------------------------------------------
// NetworkManager trait（async，数据路径）
// ----------------------------------------------------------------------------

/// 接口管理器——统一管理物理/VLAN/桥/绑定接口的生命周期。
///
/// 实现者：`NetlinkManager`（默认，基于 netlink/rtnetlink）；其他实现可替换。
/// 异步模型：数据路径操作（创建/删除/启停）走原生 async fn in trait。
#[allow(async_fn_in_trait)]
pub trait NetworkManager: Send + Sync {
    /// 列出所有接口。
    async fn list_interfaces(&self) -> Result<Vec<Interface>, crate::NetworkError>;

    /// 查询指定接口；不存在返回 `InterfaceNotFound`。
    async fn get_interface(&self, id: &InterfaceId) -> Result<Interface, crate::NetworkError>;

    /// 创建 VLAN 子接口。
    ///
    /// - `parent`：物理父接口（如 "eth0"）
    /// - `vid`：VLAN ID（1..=4094）
    /// - `name`：新接口名（如 "vlan100"）
    async fn create_vlan(
        &self,
        parent: &InterfaceId,
        vid: u16,
        name: InterfaceId,
    ) -> Result<Interface, crate::NetworkError>;

    /// 创建软件桥（如 "br0"）。
    async fn create_bridge(&self, name: InterfaceId) -> Result<Interface, crate::NetworkError>;

    /// 创建链路聚合（bond）。
    ///
    /// - `mode`：绑定模式
    /// - `slaves`：成员接口列表
    async fn create_bond(
        &self,
        name: InterfaceId,
        mode: BondMode,
        slaves: Vec<InterfaceId>,
    ) -> Result<Interface, crate::NetworkError>;

    /// 设置接口地址（覆盖原地址列表）。
    async fn set_address(
        &self,
        id: &InterfaceId,
        addrs: Vec<IpCidr>,
    ) -> Result<(), crate::NetworkError>;

    /// 启用接口（up）。
    async fn up(&self, id: &InterfaceId) -> Result<(), crate::NetworkError>;

    /// 禁用接口（down）。
    async fn down(&self, id: &InterfaceId) -> Result<(), crate::NetworkError>;

    /// 删除接口（仅可删 VLAN/桥/绑定等虚拟接口；物理接口拒绝删除）。
    async fn delete_interface(&self, id: &InterfaceId) -> Result<(), crate::NetworkError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    // —— IpCidr 解析与校验 ——

    #[test]
    fn ipcidr_parse_ipv4_valid() {
        let c = IpCidr::parse("192.168.1.0/24").unwrap();
        assert_eq!(c.addr, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 0)));
        assert_eq!(c.prefix, 24);
        assert!(c.is_ipv4());
        assert!(!c.is_ipv6());
    }

    #[test]
    fn ipcidr_parse_ipv6_valid() {
        let c = IpCidr::parse("fd00::1/64").unwrap();
        assert_eq!(c.prefix, 64);
        assert!(c.is_ipv6());
    }

    #[test]
    fn ipcidr_parse_boundary_prefix() {
        assert!(IpCidr::parse("10.0.0.1/32").is_ok());
        assert!(IpCidr::parse("10.0.0.1/0").is_ok());
        assert!(IpCidr::parse("::1/128").is_ok());
    }

    #[test]
    fn ipcidr_parse_rejects_missing_slash() {
        assert!(IpCidr::parse("192.168.1.1").is_err());
    }

    #[test]
    fn ipcidr_parse_rejects_bad_ip() {
        assert!(IpCidr::parse("999.168.1.1/24").is_err());
    }

    #[test]
    fn ipcidr_parse_rejects_bad_prefix() {
        assert!(IpCidr::parse("10.0.0.1/abc").is_err());
    }

    #[test]
    fn ipcidr_validate_rejects_ipv4_prefix_over_32() {
        let err = IpCidr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 33)
            .validate()
            .unwrap_err();
        assert!(matches!(err, crate::NetworkError::RuleInvalid(_)));
    }

    #[test]
    fn ipcidr_validate_rejects_ipv6_prefix_over_128() {
        assert!(IpCidr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 129)
            .validate()
            .is_err());
    }

    #[test]
    fn ipcidr_from_str_roundtrip() {
        let c: IpCidr = "10.0.0.1/8".parse().unwrap();
        assert_eq!(c.to_string(), "10.0.0.1/8");
    }

    #[test]
    fn ipcidr_max_prefix_correct() {
        assert_eq!(IpCidr::max_prefix(IpAddr::V4(Ipv4Addr::LOCALHOST)), 32);
        assert_eq!(IpCidr::max_prefix(IpAddr::V6(Ipv6Addr::LOCALHOST)), 128);
    }

    #[test]
    fn ipcidr_serde_roundtrip() {
        let c = IpCidr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 24);
        let j = serde_json::to_string(&c).unwrap();
        let back: IpCidr = serde_json::from_str(&j).unwrap();
        assert_eq!(c, back);
    }

    // —— VLAN ID 校验 ——

    #[test]
    fn vlan_id_boundaries() {
        assert!(validate_vlan_id(0).is_err());
        assert!(validate_vlan_id(1).is_ok());
        assert!(validate_vlan_id(4094).is_ok());
        assert!(validate_vlan_id(4095).is_err());
        assert!(validate_vlan_id(u16::MAX).is_err());
    }

    // —— Interface builder / 校验 ——

    #[test]
    fn interface_builder_chains() {
        let iface = Interface::new(InterfaceId::new("eth0"), InterfaceType::Physical)
            .with_mtu(9000)
            .with_mac("aa:bb:cc:dd:ee:ff")
            .with_addr(IpCidr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 24))
            .with_state(IfState::Up);
        assert_eq!(iface.mtu, 9000);
        assert_eq!(iface.mac.as_deref(), Some("aa:bb:cc:dd:ee:ff"));
        assert_eq!(iface.addrs.len(), 1);
        assert_eq!(iface.state, IfState::Up);
    }

    #[test]
    fn interface_mtu_validation() {
        assert!(Interface::validate_mtu(68).is_ok());
        assert!(Interface::validate_mtu(1500).is_ok());
        assert!(Interface::validate_mtu(9216).is_ok());
        assert!(Interface::validate_mtu(67).is_err());
        assert!(Interface::validate_mtu(9217).is_err());
    }

    #[test]
    fn interface_name_validation() {
        assert!(Interface::validate_name("eth0").is_ok());
        assert!(Interface::validate_name(&"x".repeat(15)).is_ok());
        assert!(Interface::validate_name("").is_err());
        assert!(Interface::validate_name(&"x".repeat(16)).is_err());
    }

    #[test]
    fn interface_id_display_from() {
        let id = InterfaceId::new("eth0");
        assert_eq!(id.to_string(), "eth0");
        let id2: InterfaceId = "br0".to_string().into();
        assert_eq!(id2.as_str(), "br0");
    }
}
