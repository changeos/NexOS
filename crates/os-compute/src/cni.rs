//! CNI 网络配置（`.conflist`）生成 + **真实落盘**。
//!
//! 定位：youki 自身不含网络管理（见规格书 §2），网络由 CNI 插件链提供。
//! 本模块把 OS 容器网络描述（[`crate::container_net::NetworkDriver`] + 子网 +
//! 端口映射）翻译成 [CNI Network Configuration List][cni-list]，并写入 CNI 配置目录
//! （默认 `/etc/cni/net.d/`），交给 CNI runtime（实现层 `CniContainerNetwork`，
//! 批 3 引入 CNI 客户端后）消费。
//!
//! 分两步：
//! - [`build_conflist`]：纯函数，`NetworkDriver + 子网 → CniConfigList`（内存结构）；
//! - [`write_conflist`] / [`write_network`]：把 conflist 序列化为 `<name>.conflist` 并
//!   落盘到指定目录（CNI runtime 按 `/etc/cni/net.d/*.conflist` 字典序加载首个）。
//!
//! **为什么不引第三方 `libcni` crate**：该 crate 未在 workspace 注册（见 §9 红线）。
//! 本模块以最小自洽 serde 结构覆盖标准 CNI 1.0.0 conflist 字段。
//!
//! [cni-list]: https://github.com/containernetworking/cni/blob/main/SPEC.md#network-configuration-list
//!
//! 插件链（Bridge 驱动）：
//! 1. **bridge** —— 创建 Linux bridge，给容器 netns 分配接口；
//! 2. **portmap** —— 处理 hostPort→containerPort NAT（读 OCI annotations 的端口映射）；
//! 3. **firewall** —— 注入 iptables 规则隔离（默认接受同 bridge 流量）。
//!
//! Host/None 驱动不需要 conflist（容器直接用 host netns 或仅 lo）。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::container::PortMapping;
use crate::container_net::NetworkDriver;
use crate::error::{ComputeError, ComputeResult};

// ----------------------------------------------------------------------------
// CNI Network Configuration List（标准 1.0.0）
// ----------------------------------------------------------------------------

/// CNI 配置列表（`.conflist` 文件顶层结构）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CniConfigList {
    /// CNI 版本（固定 `1.0.0`）
    #[serde(rename = "cniVersion")]
    pub cni_version: String,
    /// 配置列表名（= 网络名）
    pub name: String,
    /// 时间分配插件
    #[serde(rename = "ipMasq", default, skip_serializing_if = "Option::is_none")]
    pub ip_masq: Option<bool>,
    /// 插件链（按顺序执行 ADD/DEL/CHECK）
    pub plugins: Vec<CniPlugin>,
}

/// CNI 插件配置（tagged by `type`）。
///
/// 用 `#[serde(tag = "type")]` 让 `type` 字段由 variant 决定，序列化时自动加。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum CniPlugin {
    /// bridge 插件——创建 Linux bridge + veth 对。
    Bridge(BridgePlugin),
    /// portmap 插件——hostPort→containerPort DNAT。
    Portmap(PortmapPlugin),
    /// firewall 插件——iptables 规则注入。
    Firewall(FirewallPlugin),
}

/// bridge 插件配置
///
/// 注：`type` 字段由外层 [`CniPlugin`] 的 `#[serde(tag = "type")]` 注入，
/// 本结构体不重复声明（避免序列化时出现 duplicate field `type`）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BridgePlugin {
    /// bridge 设备名（如 `osbr0`）
    #[serde(rename = "bridge")]
    pub bridge_name: String,
    /// 是否启用 IP masquerade（出口 NAT）
    #[serde(rename = "ipMasq", default)]
    pub ip_masq: bool,
    /// 默认 gateway（None = 不下发）
    #[serde(rename = "isGateway", default)]
    pub is_gateway: bool,
    /// 是否为容器接口启用 IP forwarding
    #[serde(rename = "isDefaultGateway", default)]
    pub is_default_gateway: bool,
    /// IPAM 配置
    pub ipam: IpamConfig,
    /// MTU
    #[serde(rename = "mtu", default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u16>,
}

/// portmap 插件配置
///
/// 注：`type` 字段由外层 [`CniPlugin`] 的 `#[serde(tag = "type")]` 注入。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct PortmapPlugin {
    /// 端口映射能力声明（容器运行期由 CNI runtime 从 args 注入实际映射）
    #[serde(default)]
    pub capabilities: PortmapCapabilities,
    /// SNAT 模式（默认 `masquerade`）
    #[serde(rename = "snat", default)]
    pub snat: bool,
}

/// portmap capabilities
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct PortmapCapabilities {
    /// 支持运行期注入端口映射
    #[serde(rename = "portMappings", default)]
    pub port_mappings: bool,
}

/// firewall 插件配置
///
/// 注：`type` 字段由外层 [`CniPlugin`] 的 `#[serde(tag = "type")]` 注入。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FirewallPlugin {
    /// backend（iptables / nftables）
    pub backend: String,
    /// 默认策略（accept = 同 bridge 流量允许，isolate = 拒绝跨容器）
    #[serde(
        rename = "iptablesAdminChainName",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub iptables_admin_chain_name: Option<String>,
}

/// IPAM 配置（host-local 类型，按子网分配）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IpamConfig {
    /// IPAM 类型（host-local / static / dhcp）
    #[serde(rename = "type")]
    pub kind: String,
    /// 子网（CIDR，如 `192.168.100.0/24`）
    pub subnet: String,
    /// 网关（None = 子网 .1）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
    /// 排除的 IP 范围（保留段）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// 数据目录（host-local 落盘的 IP 分配记录）
    #[serde(rename = "dataDir", default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<String>,
}

// ----------------------------------------------------------------------------
// 默认值常量
// ----------------------------------------------------------------------------

/// CNI 标准版本（spec 1.0.0）。
pub const CNI_VERSION: &str = "1.0.0";
/// 默认 host-local IPAM 数据目录。
pub const DEFAULT_IPAM_DATA_DIR: &str = "/var/lib/cni/networks";
/// 默认 bridge MTU（1500，匹配标准以太网）。
pub const DEFAULT_MTU: u16 = 1500;
/// CNI runtime 默认网络配置目录（kubelet/CRI-O/youki 均从此处加载 `*.conflist`）。
pub const DEFAULT_CNI_NET_DIR: &str = "/etc/cni/net.d";
/// conflist 文件后缀。
pub const CONFLIST_EXTENSION: &str = ".conflist";

// ----------------------------------------------------------------------------
// 构造
// ----------------------------------------------------------------------------

/// 把 CIDR 转成字符串（用于 CNI subnet 字段）。
///
/// 单独抽出便于测试，不依赖 os_network 的 Display（IpCidr 未实现 Display，
/// 此处手写以避免引入新 trait 依赖）。
pub fn cidr_to_string(cidr: os_network::IpCidr) -> String {
    format!("{}/{}", cidr.addr, cidr.prefix)
}

/// 从子网推断默认网关（取网络地址 +1，即 .1）。
///
/// 仅对 IPv4 有意义；IPv6 用 RA/SLAAC，此处返回 None（实现层按需扩展）。
/// 注：这里只做"取主机位 = 1"的纯计算，不做网络化（IpCidr 已保证前缀合法）。
pub fn default_gateway(cidr: os_network::IpCidr) -> Option<String> {
    use std::net::IpAddr;
    match cidr.addr {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            // 计算网络地址：把主机位置零
            let host_bits = 32u8.saturating_sub(cidr.prefix);
            let net = network_addr_v4(u32::from_be_bytes(octets), host_bits);
            // +1 = 默认网关
            let gw = net + 1;
            Some(IpAddr::V4(std::net::Ipv4Addr::from(gw.to_be_bytes())).to_string())
        }
        IpAddr::V6(_) => None,
    }
}

/// IPv4 网络地址（主机位置零）。host_bits 范围 0..=32（超出按 32 处理）。
fn network_addr_v4(addr: u32, host_bits: u8) -> u32 {
    let hb = host_bits.min(32) as u32;
    if hb == 32 {
        return 0;
    }
    let mask: u32 = !((1u32 << hb) - 1);
    addr & mask
}

/// 构造 bridge 插件配置。
pub fn build_bridge_plugin(
    bridge_name: &str,
    subnet: os_network::IpCidr,
    mtu: Option<u16>,
) -> BridgePlugin {
    let ipam = IpamConfig {
        kind: "host-local".to_string(),
        subnet: cidr_to_string(subnet),
        gateway: default_gateway(subnet),
        exclude: Vec::new(),
        data_dir: Some(DEFAULT_IPAM_DATA_DIR.to_string()),
    };
    BridgePlugin {
        bridge_name: bridge_name.to_string(),
        ip_masq: true,
        is_gateway: true,
        is_default_gateway: true,
        ipam,
        mtu: Some(mtu.unwrap_or(DEFAULT_MTU)),
    }
}

/// 构造 portmap 插件配置（声明支持 portMappings capability）。
pub fn build_portmap_plugin() -> PortmapPlugin {
    PortmapPlugin {
        capabilities: PortmapCapabilities {
            port_mappings: true,
        },
        snat: true,
    }
}

/// 构造 firewall 插件配置（nftables backend）。
pub fn build_firewall_plugin() -> FirewallPlugin {
    FirewallPlugin {
        backend: "nftables".to_string(),
        iptables_admin_chain_name: None,
    }
}

/// 从端口映射列表构造 CNI runtime args（ADD 阶段透传）。
///
/// 返回 `Vec<HashMap>`，每项 `{"hostPort":H,"containerPort":C,"protocol":"tcp"}`。
/// 实现层把它塞进 CNI cmd args 的 `portMappings` 字段。
pub fn port_mappings_args(ports: &[PortMapping]) -> Vec<HashMap<String, String>> {
    ports
        .iter()
        .map(|p| {
            let mut m = HashMap::new();
            m.insert("hostPort".to_string(), p.host_port.to_string());
            m.insert("containerPort".to_string(), p.container_port.to_string());
            m.insert("protocol".to_string(), protocol_str(p.protocol).to_string());
            m
        })
        .collect()
}

fn protocol_str(p: os_network::Protocol) -> &'static str {
    use os_network::Protocol;
    match p {
        Protocol::Tcp => "tcp",
        Protocol::Udp => "udp",
        Protocol::Any => "",
    }
}

/// 从网络名推导 bridge 设备名（前缀 `os` + 名字，截断到 15 字节内满足 Linux IFNAMSIZ）。
///
/// 如 `osnet` → `ososnet`；超长截断。设计：统一 `os` 前缀避免与系统 bridge 冲突。
pub fn bridge_device_name(network: &str) -> String {
    let base = format!("os{}", network);
    // IFNAMSIZ = 16，含结尾 NUL，故有效长度 15
    base.chars().take(15).collect()
}

/// 构造完整的 CNI conflist（仅 Bridge 驱动）。
///
/// Host/None 驱动返回错误——它们不需要 conflist（容器用 host netns 或 lo-only）。
/// 实现层（CniContainerNetwork）应在调本函数前按 driver 分流。
pub fn build_conflist(
    name: &str,
    driver: NetworkDriver,
    subnet: os_network::IpCidr,
) -> ComputeResult<CniConfigList> {
    if name.trim().is_empty() {
        return Err(ComputeError::InvalidSpec("CNI 网络名不能为空".to_string()));
    }
    match driver {
        NetworkDriver::Bridge => {
            let bridge = build_bridge_plugin(&bridge_device_name(name), subnet, None);
            let portmap = build_portmap_plugin();
            let firewall = build_firewall_plugin();
            Ok(CniConfigList {
                cni_version: CNI_VERSION.to_string(),
                name: name.to_string(),
                ip_masq: Some(true),
                plugins: vec![
                    CniPlugin::Bridge(bridge),
                    CniPlugin::Portmap(portmap),
                    CniPlugin::Firewall(firewall),
                ],
            })
        }
        NetworkDriver::Host | NetworkDriver::None => Err(ComputeError::InvalidSpec(format!(
            "{:?} 驱动不需要 CNI conflist（容器直接复用 host netns 或仅 lo）",
            driver
        ))),
    }
}

/// 把 conflist 序列化成 `.conflist` JSON 字符串（pretty）。
pub fn to_conflist_json(list: &CniConfigList) -> ComputeResult<String> {
    serde_json::to_string_pretty(list)
        .map_err(|e| ComputeError::Internal(format!("CNI conflist 序列化失败: {e}")))
}

/// 构造 conflist 文件名：`<network_name>.conflist`。
pub fn conflist_filename(network_name: &str) -> String {
    format!("{network_name}{CONFLIST_EXTENSION}")
}

/// 把 conflist 序列化后写入 `<dir>/<name>.conflist`。
///
/// `dir` 须存在（调用方负责 `create_dir_all`）；本函数仅写文件——这样调用方可控决定
/// 目录布局（[`write_network`] 封装了含建目录的便捷路径）。若文件已存在则覆盖（幂等）。
/// 返回写入的完整路径。
pub fn write_conflist(
    list: &CniConfigList,
    dir: &std::path::Path,
) -> ComputeResult<std::path::PathBuf> {
    let json = to_conflist_json(list)?;
    let target = dir.join(conflist_filename(&list.name));
    std::fs::write(&target, json)?;
    Ok(target)
}

/// 一站式：从网络描述生成 conflist、建目录、写 `<name>.conflist`。
///
/// - `name`/`driver`/`subnet`：见 [`build_conflist`]；
/// - `dir`：CNI 配置目录（生产用 [`DEFAULT_CNI_NET_DIR`]，测试用 tempdir）。
///
/// 目录已存在不报错（幂等）；返回写入的 conflist 完整路径。
pub fn write_network(
    name: &str,
    driver: NetworkDriver,
    subnet: os_network::IpCidr,
    dir: &std::path::Path,
) -> ComputeResult<std::path::PathBuf> {
    std::fs::create_dir_all(dir)?;
    let list = build_conflist(name, driver, subnet)?;
    write_conflist(&list, dir)
}

/// 从指定目录读回 `<name>.conflist` 并反序列化（往返校验/审计用）。
pub fn read_conflist(name: &str, dir: &std::path::Path) -> ComputeResult<CniConfigList> {
    let target = dir.join(conflist_filename(name));
    let content = std::fs::read_to_string(&target)?;
    serde_json::from_str(&content)
        .map_err(|e| ComputeError::Internal(format!("conflist 反序列化失败: {e}")))
}

/// 列出指定目录下所有 `*.conflist` 文件的网络名（按文件名字典序）。
///
/// 用于实现层 `list_networks` 扫描 CNI 目录。返回网络名（去掉 `.conflist` 后缀）。
pub fn list_conflist_networks(dir: &std::path::Path) -> ComputeResult<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("conflist") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                names.push(stem.to_string());
            }
        }
    }
    names.sort();
    Ok(names)
}

// ----------------------------------------------------------------------------
// 测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::PortMapping;
    use os_network::{IpCidr, Protocol};
    use std::net::IpAddr;

    fn subnet_v4() -> IpCidr {
        IpCidr::new(IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 100, 0)), 24)
    }

    #[test]
    fn cidr_to_string_formats_addr_prefix() {
        let s = cidr_to_string(subnet_v4());
        assert_eq!(s, "192.168.100.0/24");
    }

    #[test]
    fn default_gateway_is_network_addr_plus_one() {
        let gw = default_gateway(subnet_v4()).unwrap();
        assert_eq!(gw, "192.168.100.1");
    }

    #[test]
    fn default_gateway_ipv6_is_none() {
        let cidr = IpCidr::new(IpAddr::V6("fd00::1".parse().unwrap()), 64);
        assert!(default_gateway(cidr).is_none());
    }

    #[test]
    fn network_addr_zeroes_host_bits() {
        // 192.168.100.130 / 24 -> network 192.168.100.0
        let a = u32::from_be_bytes([192, 168, 100, 130]);
        let net = network_addr_v4(a, 8);
        assert_eq!(net.to_be_bytes(), [192, 168, 100, 0]);
        // /30 -> 保留前 30 位
        let net30 = network_addr_v4(a, 2);
        assert_eq!(net30.to_be_bytes(), [192, 168, 100, 128]);
    }

    #[test]
    fn bridge_device_name_prefixes_and_truncates() {
        assert_eq!(bridge_device_name("net"), "osnet");
        // 超长截断到 15
        let long = bridge_device_name("this-is-a-very-long-network-name");
        assert!(long.len() <= 15);
        assert!(long.starts_with("os"));
    }

    #[test]
    fn build_bridge_plugin_has_host_local_ipam() {
        let p = build_bridge_plugin("osbr0", subnet_v4(), None);
        assert_eq!(p.bridge_name, "osbr0");
        assert_eq!(p.ipam.kind, "host-local");
        assert_eq!(p.ipam.subnet, "192.168.100.0/24");
        assert_eq!(p.ipam.gateway.as_deref(), Some("192.168.100.1"));
        assert!(p.is_default_gateway);
        assert_eq!(p.mtu, Some(DEFAULT_MTU));
    }

    #[test]
    fn portmap_plugin_declares_capability() {
        let p = build_portmap_plugin();
        assert!(p.capabilities.port_mappings);
    }

    #[test]
    fn port_mappings_args_serialize_each_port() {
        let ports = vec![
            PortMapping {
                host_port: 8080,
                container_port: 80,
                protocol: Protocol::Tcp,
            },
            PortMapping {
                host_port: 53,
                container_port: 53,
                protocol: Protocol::Udp,
            },
        ];
        let args = port_mappings_args(&ports);
        assert_eq!(args.len(), 2);
        assert_eq!(args[0].get("hostPort").unwrap(), "8080");
        assert_eq!(args[0].get("protocol").unwrap(), "tcp");
        assert_eq!(args[1].get("protocol").unwrap(), "udp");
    }

    #[test]
    fn build_conflist_bridge_chains_three_plugins() {
        let list = build_conflist("osnet", NetworkDriver::Bridge, subnet_v4()).unwrap();
        assert_eq!(list.name, "osnet");
        assert_eq!(list.cni_version, CNI_VERSION);
        assert_eq!(list.plugins.len(), 3);
        // 顺序：bridge -> portmap -> firewall
        assert!(matches!(list.plugins[0], CniPlugin::Bridge(_)));
        assert!(matches!(list.plugins[1], CniPlugin::Portmap(_)));
        assert!(matches!(list.plugins[2], CniPlugin::Firewall(_)));
    }

    #[test]
    fn build_conflist_rejects_empty_name() {
        let err = build_conflist("  ", NetworkDriver::Bridge, subnet_v4()).unwrap_err();
        assert!(matches!(err, ComputeError::InvalidSpec(_)));
    }

    #[test]
    fn build_conflist_rejects_host_and_none_drivers() {
        for d in [NetworkDriver::Host, NetworkDriver::None] {
            let err = build_conflist("x", d, subnet_v4()).unwrap_err();
            assert!(matches!(err, ComputeError::InvalidSpec(_)));
        }
    }

    #[test]
    fn to_conflist_json_roundtrip_parses_back() {
        let list = build_conflist("osnet", NetworkDriver::Bridge, subnet_v4()).unwrap();
        let json = to_conflist_json(&list).unwrap();
        // JSON 中应出现关键标签
        assert!(json.contains(r#""cniVersion": "1.0.0""#));
        assert!(json.contains(r#""type": "bridge""#));
        assert!(json.contains(r#""type": "portmap""#));
        assert!(json.contains(r#""type": "firewall""#));
        let back: CniConfigList = serde_json::from_str(&json).unwrap();
        assert_eq!(back, list);
    }

    #[test]
    fn bridge_plugin_custom_mtu() {
        let p = build_bridge_plugin("osbr0", subnet_v4(), Some(9000));
        assert_eq!(p.mtu, Some(9000));
    }

    // --------------------------------------------------------------------
    // 落盘测（tempdir 真实文件系统往返）
    // --------------------------------------------------------------------

    #[test]
    fn conflist_filename_appends_extension() {
        assert_eq!(conflist_filename("osnet"), "osnet.conflist");
    }

    #[test]
    fn write_conflist_creates_file_with_correct_name() {
        let tmp = tempfile::tempdir().unwrap();
        let list = build_conflist("osnet", NetworkDriver::Bridge, subnet_v4()).unwrap();
        let path = write_conflist(&list, tmp.path()).unwrap();
        assert_eq!(path.file_name().unwrap(), "osnet.conflist");
        assert!(path.is_file());

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains(r#""cniVersion": "1.0.0""#));
        assert!(content.contains(r#""name": "osnet""#));
        assert!(content.contains(r#""type": "bridge""#));
    }

    #[test]
    fn write_conflist_is_idempotent_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let list = build_conflist("osnet", NetworkDriver::Bridge, subnet_v4()).unwrap();
        write_conflist(&list, tmp.path()).unwrap();
        write_conflist(&list, tmp.path()).unwrap();
        let count = std::fs::read_dir(tmp.path()).unwrap().count();
        assert_eq!(count, 1, "conflist 应被覆盖而非新增");
    }

    #[test]
    fn write_network_creates_dir_and_writes_conflist() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("net.d");
        assert!(!dir.exists());

        let path = write_network("mynet", NetworkDriver::Bridge, subnet_v4(), &dir).unwrap();
        assert!(dir.is_dir());
        assert!(path.is_file());
        assert_eq!(path, dir.join("mynet.conflist"));

        // 读回校验
        let back = read_conflist("mynet", &dir).unwrap();
        assert_eq!(back.name, "mynet");
        assert_eq!(back.cni_version, CNI_VERSION);
        assert_eq!(back.plugins.len(), 3);
    }

    #[test]
    fn write_network_roundtrip_preserves_subnet_and_gateway() {
        let tmp = tempfile::tempdir().unwrap();
        write_network("osnet", NetworkDriver::Bridge, subnet_v4(), tmp.path()).unwrap();
        let back = read_conflist("osnet", tmp.path()).unwrap();
        let bridge = match &back.plugins[0] {
            CniPlugin::Bridge(b) => b,
            _ => panic!("首个插件应为 bridge"),
        };
        assert_eq!(bridge.ipam.subnet, "192.168.100.0/24");
        assert_eq!(bridge.ipam.gateway.as_deref(), Some("192.168.100.1"));
    }

    #[test]
    fn read_conflist_missing_file_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let err = read_conflist("nope", tmp.path()).unwrap_err();
        assert!(matches!(err, ComputeError::Io(_)));
    }

    #[test]
    fn list_conflist_networks_returns_sorted_names() {
        let tmp = tempfile::tempdir().unwrap();
        // 写三个网络（乱序写入）
        write_network("znet", NetworkDriver::Bridge, subnet_v4(), tmp.path()).unwrap();
        write_network("anet", NetworkDriver::Bridge, subnet_v4(), tmp.path()).unwrap();
        write_network("mnet", NetworkDriver::Bridge, subnet_v4(), tmp.path()).unwrap();

        let names = list_conflist_networks(tmp.path()).unwrap();
        assert_eq!(names, vec!["anet", "mnet", "znet"]);
    }

    #[test]
    fn list_conflist_networks_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let names = list_conflist_networks(tmp.path()).unwrap();
        assert!(names.is_empty());
    }

    #[test]
    fn list_conflist_networks_ignores_non_conflist_files() {
        let tmp = tempfile::tempdir().unwrap();
        write_network("real", NetworkDriver::Bridge, subnet_v4(), tmp.path()).unwrap();
        std::fs::write(tmp.path().join("README"), "not a conflist").unwrap();
        std::fs::write(tmp.path().join("x.conf"), "old CNI 0.1.0 single").unwrap();
        let names = list_conflist_networks(tmp.path()).unwrap();
        assert_eq!(names, vec!["real"]);
    }

    // --------------------------------------------------------------------
    // 补充测：CniPlugin tagged serde / portmap 边界 / firewall / 常量
    // --------------------------------------------------------------------

    #[test]
    fn cni_version_constant_is_1_0_0() {
        assert_eq!(CNI_VERSION, "1.0.0");
    }

    #[test]
    fn default_constants_have_expected_values() {
        assert_eq!(DEFAULT_IPAM_DATA_DIR, "/var/lib/cni/networks");
        assert_eq!(DEFAULT_MTU, 1500);
        assert_eq!(DEFAULT_CNI_NET_DIR, "/etc/cni/net.d");
        assert_eq!(CONFLIST_EXTENSION, ".conflist");
    }

    #[test]
    fn bridge_plugin_defaults_set_all_flags() {
        let p = build_bridge_plugin("osbr0", subnet_v4(), None);
        assert!(p.ip_masq, "ipMasq 应默认 true");
        assert!(p.is_gateway, "isGateway 应默认 true");
        assert!(p.is_default_gateway, "isDefaultGateway 应默认 true");
        assert_eq!(p.ipam.data_dir.as_deref(), Some(DEFAULT_IPAM_DATA_DIR));
        assert!(p.ipam.exclude.is_empty());
    }

    #[test]
    fn build_portmap_plugin_snat_true() {
        let p = build_portmap_plugin();
        assert!(p.snat);
        assert!(p.capabilities.port_mappings);
    }

    #[test]
    fn build_firewall_plugin_nftables_no_admin_chain() {
        let p = build_firewall_plugin();
        assert_eq!(p.backend, "nftables");
        assert!(p.iptables_admin_chain_name.is_none());
    }

    #[test]
    fn portmap_plugin_default_all_false() {
        let p = PortmapPlugin::default();
        assert!(!p.snat);
        assert!(!p.capabilities.port_mappings);
    }

    #[test]
    fn cni_plugin_bridge_tagged_serialization() {
        // CniPlugin::Bridge → JSON 含 "type":"bridge"
        let list = build_conflist("net", NetworkDriver::Bridge, subnet_v4()).unwrap();
        let json = serde_json::to_string(&list).unwrap();
        // kebab-case tag: bridge / portmap / firewall
        assert!(json.contains(r#""type":"bridge""#));
        assert!(json.contains(r#""type":"portmap""#));
        assert!(json.contains(r#""type":"firewall""#));
    }

    #[test]
    fn cni_plugin_roundtrip_keeps_tagged_variants() {
        let list = build_conflist("n", NetworkDriver::Bridge, subnet_v4()).unwrap();
        let json = serde_json::to_string(&list).unwrap();
        let back: CniConfigList = serde_json::from_str(&json).unwrap();
        assert_eq!(back.plugins.len(), 3);
        assert!(matches!(back.plugins[0], CniPlugin::Bridge(_)));
        assert!(matches!(back.plugins[1], CniPlugin::Portmap(_)));
        assert!(matches!(back.plugins[2], CniPlugin::Firewall(_)));
    }

    #[test]
    fn portmap_plugin_serialization_includes_capabilities() {
        let p = build_portmap_plugin();
        let json = serde_json::to_string(&p).unwrap();
        // portmap 内嵌结构序列化时应含 portMappings 字段
        assert!(json.contains(r#""portMappings":true"#));
        assert!(json.contains(r#""snat":true"#));
    }

    #[test]
    fn firewall_plugin_serialization_omits_admin_chain_when_none() {
        let p = build_firewall_plugin();
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains(r#""backend":"nftables""#));
        assert!(!json.contains("iptablesAdminChainName"), "None 应被跳过");
    }

    #[test]
    fn firewall_plugin_with_admin_chain() {
        let mut p = build_firewall_plugin();
        p.iptables_admin_chain_name = Some("os-admin".to_string());
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains(r#""iptablesAdminChainName":"os-admin""#));
    }

    #[test]
    fn ipam_config_default_fields() {
        let p = build_bridge_plugin("x", subnet_v4(), None);
        assert_eq!(p.ipam.kind, "host-local");
        assert!(p.ipam.exclude.is_empty());
        assert_eq!(p.ipam.data_dir.as_deref(), Some(DEFAULT_IPAM_DATA_DIR));
    }

    #[test]
    fn bridge_device_name_simple_short() {
        // 短名：保留前缀
        assert_eq!(bridge_device_name("lan"), "oslan");
        assert_eq!(bridge_device_name("a"), "osa");
    }

    #[test]
    fn bridge_device_name_empty_network() {
        // 空网络名 → 仅前缀 "os"
        assert_eq!(bridge_device_name(""), "os");
    }

    #[test]
    fn cidr_to_string_v6() {
        let cidr = IpCidr::new(IpAddr::V6("fd00::1".parse().unwrap()), 64);
        assert_eq!(cidr_to_string(cidr), "fd00::1/64");
    }

    #[test]
    fn default_gateway_v4_non_default_network() {
        // /16 子网：网关 = .0.1
        let cidr = IpCidr::new(IpAddr::V4(std::net::Ipv4Addr::new(172, 16, 5, 100)), 16);
        let gw = default_gateway(cidr).unwrap();
        assert_eq!(gw, "172.16.0.1");
    }

    #[test]
    fn default_gateway_v4_32_prefix() {
        // /32 单机：host_bits=0，net = 原 IP，gw = IP+1
        let cidr = IpCidr::new(IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 5)), 32);
        let gw = default_gateway(cidr).unwrap();
        assert_eq!(gw, "10.0.0.6");
    }

    #[test]
    fn network_addr_v4_zero_prefix() {
        // /0：host_bits=32，应返回 0
        let a = u32::from_be_bytes([192, 168, 1, 1]);
        let net = network_addr_v4(a, 32);
        assert_eq!(net, 0);
    }

    #[test]
    fn port_mappings_args_empty_returns_empty() {
        let args = port_mappings_args(&[]);
        assert!(args.is_empty());
    }

    #[test]
    fn port_mappings_args_any_protocol_empty_string() {
        let ports = vec![PortMapping {
            host_port: 80,
            container_port: 8080,
            protocol: Protocol::Any,
        }];
        let args = port_mappings_args(&ports);
        assert_eq!(args[0].get("protocol").unwrap(), "");
    }

    #[test]
    fn conflist_filename_special_chars() {
        // 含特殊字符的网络名：直接拼后缀（不做转义）
        assert_eq!(conflist_filename("net-1"), "net-1.conflist");
        assert_eq!(conflist_filename(""), ".conflist");
    }

    #[test]
    fn to_conflist_json_serialization_error_unreachable() {
        // CniConfigList 全为 String 字段，正常序列化不应失败
        let list = build_conflist("n", NetworkDriver::Bridge, subnet_v4()).unwrap();
        assert!(to_conflist_json(&list).is_ok());
    }

    #[test]
    fn list_conflist_networks_missing_dir_returns_io_error() {
        let err =
            list_conflist_networks(std::path::Path::new("/nonexistent/cni-xyz-999")).unwrap_err();
        assert!(matches!(err, ComputeError::Io(_)));
    }

    #[test]
    fn build_conflist_name_with_internal_spaces_ok() {
        // 空格在中间（非全空白）：name.trim() 非空，应成功
        let list = build_conflist("my net", NetworkDriver::Bridge, subnet_v4()).unwrap();
        assert_eq!(list.name, "my net");
    }

    #[test]
    fn read_conflist_malformed_returns_internal() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("bad.conflist"), "{not json").unwrap();
        let err = read_conflist("bad", tmp.path()).unwrap_err();
        assert!(matches!(err, ComputeError::Internal(_)));
    }

    #[test]
    fn cni_config_list_clone_eq() {
        let list = build_conflist("n", NetworkDriver::Bridge, subnet_v4()).unwrap();
        let cloned = list.clone();
        assert_eq!(cloned, list);
    }

    #[test]
    fn cni_config_list_ip_masq_some_true() {
        let list = build_conflist("n", NetworkDriver::Bridge, subnet_v4()).unwrap();
        assert_eq!(list.ip_masq, Some(true));
    }
}
