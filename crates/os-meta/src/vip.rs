//! 浮动 IP（VIP）管理——HA 对外稳定入口
//!
//! 决策依据：规划文档 §3.5 —— 集群对外暴露 VIP，leader 持有 VIP；
//! 故障转移时 VIP 漂移到新 leader，客户端无感切换。
//! VIP 的接口/地址语义复用 os_network::IpCidr。

use async_trait::async_trait;
use os_core::{Deserialize, NodeId, Serialize};
use os_network::IpCidr;

// ----------------------------------------------------------------------------
// VipConfig
// ----------------------------------------------------------------------------

/// VIP 配置（一个集群可配置一个对外 VIP）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VipConfig {
    /// VIP 地址（CIDR 形式，复用 os_network::IpCidr）
    pub ip: IpCidr,
    /// 绑定的网络接口名（如 "br0"）
    pub interface: String,
    /// 当前持有节点（None = 未绑定到任何节点，通常出现在选举间隙）
    pub current_owner: Option<NodeId>,
}

// ----------------------------------------------------------------------------
// VipManager trait（async）
// ----------------------------------------------------------------------------

/// VIP 管理器——把浮动 IP 绑定到指定节点 / 释放 / 查询归属。
///
/// 实现者：`NetlinkVipManager`（默认，基于 netlink 配合 ARP 广播通告漂移）；
/// 与 os-network NetworkManager 协同（地址设置走同一接口抽象）。
///
/// 注：按 ADR-COMPAT-001，本 trait 经 `Box<dyn VipManager>` 运行期多态（见 mock.rs
/// `_assert_dyn_compatible`），故用 `#[async_trait]`；方法签名未变。
#[async_trait]
pub trait VipManager: Send + Sync {
    /// 把 VIP 绑定到指定节点（leader 当选或故障转移时调用）。
    /// 若 VIP 当前已被其他节点持有，返回 `VipConflict`。
    async fn assign(&self, node: &NodeId) -> Result<(), crate::MetaError>;

    /// 释放 VIP（当前 owner 解绑，进入未绑定状态）。
    async fn release(&self) -> Result<(), crate::MetaError>;

    /// 查询当前 VIP 持有节点；未绑定返回 None。
    async fn current_owner(&self) -> Option<NodeId>;
}

// ----------------------------------------------------------------------------
// 单元测试：VipConfig serde 往返
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn cidr(addr: &str, prefix: u8) -> IpCidr {
        IpCidr::new(addr.parse().unwrap(), prefix)
    }

    #[test]
    fn vip_config_serde_roundtrip_no_owner() {
        let cfg = VipConfig {
            ip: cidr("10.0.0.5", 24),
            interface: "br0".into(),
            current_owner: None,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: VipConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.interface, cfg.interface);
        assert!(back.current_owner.is_none());
    }

    #[test]
    fn vip_config_serde_roundtrip_with_owner() {
        let cfg = VipConfig {
            ip: cidr("192.168.1.100", 24),
            interface: "eth0".into(),
            current_owner: Some(NodeId::new("leader-1")),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: VipConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.interface, "eth0");
        assert_eq!(
            back.current_owner.as_ref().map(|n| n.as_str()),
            Some("leader-1")
        );
    }

    #[test]
    fn vip_config_clone_preserves_fields() {
        let cfg = VipConfig {
            ip: cidr("10.0.0.1", 16),
            interface: "bond0".into(),
            current_owner: Some(NodeId::new("n1")),
        };
        let c = cfg.clone();
        assert_eq!(c.interface, cfg.interface);
        assert_eq!(c.current_owner, cfg.current_owner);
    }

    #[test]
    fn vip_config_debug_format_contains_interface() {
        let cfg = VipConfig {
            ip: cidr("10.0.0.1", 24),
            interface: "dbg-iface".into(),
            current_owner: None,
        };
        let s = format!("{cfg:?}");
        assert!(s.contains("dbg-iface"));
    }

    #[test]
    fn vip_config_ipv6_cidr_supported() {
        // IPv6 CIDR 也应能往返（复用 os_network::IpCidr）
        let cfg = VipConfig {
            ip: IpCidr::new("fd00::1".parse().unwrap(), 64),
            interface: "eth1".into(),
            current_owner: None,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: VipConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.interface, "eth1");
    }
}
