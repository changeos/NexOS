//! 容器网络（CNI，补 youki 短板）
//!
//! 实现说明（规划文档 §3.4）：youki 自身不含网络管理，本 trait 编排 CNI 插件
//! （veth + bridge），提供容器网络的创建/删除/接入/断开。

use os_core::{ContainerId, Deserialize, Serialize};
use os_network::IpCidr;

use crate::ComputeResult;

/// 网络驱动
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkDriver {
    /// 桥接（创建 Linux bridge，容器通过 veth 接入）
    Bridge,
    /// host 模式（共享宿主网络栈）
    Host,
    /// 无网络（仅 lo）
    None,
}

/// 容器网络信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkInfo {
    /// 网络名
    pub name: String,
    /// 子网（CIDR）
    pub subnet: IpCidr,
    /// 驱动
    pub driver: NetworkDriver,
    /// 已接入的容器数
    pub container_count: u32,
}

/// 容器网络管理器——编排 CNI 插件。
#[allow(async_fn_in_trait)]
pub trait ContainerNetwork: Send + Sync {
    /// 创建容器网络（veth + bridge）。
    async fn create_network(&self, name: &str, subnet: IpCidr) -> ComputeResult<NetworkInfo>;

    /// 删除容器网络（须无容器接入）。
    async fn delete_network(&self, name: &str) -> ComputeResult<()>;

    /// 将容器接入网络（创建 veth 对，挂到 bridge）。
    async fn connect(&self, container: &ContainerId, network: &str) -> ComputeResult<()>;

    /// 将容器从网络断开。
    async fn disconnect(&self, container: &ContainerId, network: &str) -> ComputeResult<()>;

    /// 列出所有容器网络。
    async fn list_networks(&self) -> ComputeResult<Vec<NetworkInfo>>;
}

// ----------------------------------------------------------------------------
// 测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn sample_cidr() -> IpCidr {
        IpCidr::new(IpAddr::V4(std::net::Ipv4Addr::new(10, 88, 0, 0)), 24)
    }

    // ---------------- NetworkDriver ----------------

    #[test]
    fn network_driver_variants_eq() {
        assert_eq!(NetworkDriver::Bridge, NetworkDriver::Bridge);
        assert_eq!(NetworkDriver::Host, NetworkDriver::Host);
        assert_eq!(NetworkDriver::None, NetworkDriver::None);
        assert_ne!(NetworkDriver::Bridge, NetworkDriver::Host);
        assert_ne!(NetworkDriver::Host, NetworkDriver::None);
        assert_ne!(NetworkDriver::Bridge, NetworkDriver::None);
    }

    #[test]
    fn network_driver_copy_clone() {
        let a = NetworkDriver::Bridge;
        let b = a; // Copy
        let c = a; // 仍可用
        assert_eq!(a, b);
        assert_eq!(a, c);
        // NetworkDriver 实现 Copy（无需 clone 即可重复使用）
        assert_eq!(a, NetworkDriver::Bridge);
    }

    #[test]
    fn network_driver_serde_lowercase() {
        // serde rename_all = "lowercase"
        let cases = [
            (NetworkDriver::Bridge, "\"bridge\""),
            (NetworkDriver::Host, "\"host\""),
            (NetworkDriver::None, "\"none\""),
        ];
        for (driver, expected) in cases {
            let json = serde_json::to_string(&driver).unwrap();
            assert_eq!(json, expected, "序列化 {driver:?} 应为 {expected}");
            let back: NetworkDriver = serde_json::from_str(&json).unwrap();
            assert_eq!(back, driver, "反序列化应还原");
        }
    }

    #[test]
    fn network_driver_serde_unknown_variant_errors() {
        // 非法 driver 字符串 → 反序列化失败
        assert!(serde_json::from_str::<NetworkDriver>(r#""macvlan""#).is_err());
        assert!(serde_json::from_str::<NetworkDriver>(r#""Bridge""#).is_err());
        assert!(serde_json::from_str::<NetworkDriver>(r#""""#).is_err());
    }

    #[test]
    fn network_driver_debug_format() {
        // Debug 应含变体名
        assert!(format!("{:?}", NetworkDriver::Bridge).contains("Bridge"));
        assert!(format!("{:?}", NetworkDriver::Host).contains("Host"));
        assert!(format!("{:?}", NetworkDriver::None).contains("None"));
    }

    // ---------------- NetworkInfo ----------------

    #[test]
    fn network_info_construction_and_fields() {
        let info = NetworkInfo {
            name: "osnet".to_string(),
            subnet: sample_cidr(),
            driver: NetworkDriver::Bridge,
            container_count: 3,
        };
        assert_eq!(info.name, "osnet");
        assert_eq!(info.driver, NetworkDriver::Bridge);
        assert_eq!(info.container_count, 3);
        assert_eq!(info.subnet.prefix, 24);
    }

    #[test]
    fn network_info_zero_container_count() {
        // 空网络：container_count = 0 合法（删除前的检查场景）
        let info = NetworkInfo {
            name: "empty".to_string(),
            subnet: sample_cidr(),
            driver: NetworkDriver::Bridge,
            container_count: 0,
        };
        assert_eq!(info.container_count, 0);
    }

    #[test]
    fn network_info_clone_preserves_fields() {
        let info = NetworkInfo {
            name: "n".to_string(),
            subnet: sample_cidr(),
            driver: NetworkDriver::Host,
            container_count: 5,
        };
        let cloned = info.clone();
        assert_eq!(cloned.name, info.name);
        assert_eq!(cloned.subnet.prefix, info.subnet.prefix);
        assert_eq!(cloned.driver, info.driver);
        assert_eq!(cloned.container_count, info.container_count);
    }

    #[test]
    fn network_info_serde_roundtrip_preserves_all_fields() {
        let info = NetworkInfo {
            name: "lan0".to_string(),
            subnet: sample_cidr(),
            driver: NetworkDriver::None,
            container_count: 42,
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: NetworkInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, info.name);
        assert_eq!(back.subnet.prefix, info.subnet.prefix);
        assert_eq!(back.driver, info.driver);
        assert_eq!(back.container_count, info.container_count);
    }

    #[test]
    fn network_info_ipv6_subnet() {
        let cidr = IpCidr::new(IpAddr::V6("fd00::1".parse().unwrap()), 64);
        let info = NetworkInfo {
            name: "v6net".to_string(),
            subnet: cidr,
            driver: NetworkDriver::Bridge,
            container_count: 1,
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: NetworkInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.subnet.prefix, 64);
        assert_eq!(back.driver, NetworkDriver::Bridge);
    }

    #[test]
    fn network_info_debug_format_works() {
        // 确认 Debug trait 派生可用
        let info = NetworkInfo {
            name: "dbg".to_string(),
            subnet: sample_cidr(),
            driver: NetworkDriver::Bridge,
            container_count: 0,
        };
        let s = format!("{info:?}");
        assert!(s.contains("NetworkInfo"));
        assert!(s.contains("dbg"));
    }

    #[test]
    fn network_driver_all_variants_in_a_vec() {
        let drivers = vec![
            NetworkDriver::Bridge,
            NetworkDriver::Host,
            NetworkDriver::None,
        ];
        // 序列化整组（验证 serde 流式遍历变体）
        let json = serde_json::to_string(&drivers).unwrap();
        assert!(json.contains("\"bridge\""));
        assert!(json.contains("\"host\""));
        assert!(json.contains("\"none\""));
        let back: Vec<NetworkDriver> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, drivers);
    }
}
