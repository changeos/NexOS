//! PXE 系统自举（规划文档 §3.10 阶段1）
//!
//! 流程：PXE 启动裸机目标节点 → 分区/装基础系统 → 建 ZFS 池 → 拉起 osd 空壳。
//! PXE 能力由 os-network::PxeServer 提供，本 trait 在其上编排自举阶段。

use os_core::TaskId;
use serde::{Deserialize, Serialize};

// ----------------------------------------------------------------------------
// 目标节点与配置
// ----------------------------------------------------------------------------

/// 自举目标节点（裸机，待装系统）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvisionTarget {
    /// 目标网卡 MAC（PXE 引导标识）
    pub mac: String,
    /// 指定 IP（可选；None 则走 DHCP）
    pub ip: Option<String>,
    /// CPU 架构（`x86_64` / `aarch64`）
    pub arch: String,
    /// 管理端点（安装完成后 osd 监听地址，含端口）
    pub endpoint: String,
}

/// 自举配置（基础镜像 + 池 + 网络）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvisionConfig {
    /// 基础系统镜像路径/标识（squashfs/rootfs）
    pub base_image: String,
    /// root 密码哈希（首启强制重设——见 §3.19 安全清单：绝不预置明文）
    pub root_password_hash: String,
    /// ZFS 池成员盘列表（设备路径，如 `/dev/sda`）
    pub zfs_pool_disks: Vec<String>,
    /// 网络配置（IP/网关/VLAN/绑定等，结构由 os-network 定义，此处开放）
    pub network_config: serde_json::Value,
}

// ----------------------------------------------------------------------------
// 自举状态
// ----------------------------------------------------------------------------

/// 自举阶段状态机
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum ProvisionStatus {
    /// PXE 启动中
    Booting,
    /// 安装基础系统
    Installing,
    /// 组建 ZFS 池
    FormingPool,
    /// 就绪（产出新节点 ID）
    Ready {
        /// 新装配节点的 ID
        node_id: os_core::NodeId,
    },
    /// 失败（附原因）
    Failed {
        /// 失败原因
        reason: String,
    },
}

// ----------------------------------------------------------------------------
// Provisioner trait（async）
// ----------------------------------------------------------------------------

/// 系统自举器——PXE 引导裸机并完成阶段化安装。
///
/// 实现者：编排 os-network::PxeServer + os-storage 建池 + osd 空壳拉起。
/// 安全：`root_password_hash` 仅为临时占位，首启强制用户重设（呼应 §3.19）。
#[allow(async_fn_in_trait)]
pub trait Provisioner: Send + Sync {
    /// 通过 PXE 启动目标节点（裸机加电后从网络引导）。
    async fn boot_via_pxe(&self, target: &ProvisionTarget)
        -> Result<TaskId, crate::ProvisionError>;

    /// 阶段1 初始化系统：分区/装基础系统/建 ZFS 池/拉起 osd 空壳。
    async fn init_system(
        &self,
        target: &ProvisionTarget,
        config: &ProvisionConfig,
    ) -> Result<TaskId, crate::ProvisionError>;

    /// 查询自举任务状态。
    async fn status(&self, task: &TaskId) -> ProvisionStatus;
}

// ----------------------------------------------------------------------------
// 单元测——ProvisionTarget / ProvisionConfig / ProvisionStatus serde 往返
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provision_target_serde_roundtrip() {
        let t = ProvisionTarget {
            mac: "aa:bb:cc:dd:ee:ff".into(),
            ip: Some("10.0.0.5".into()),
            arch: "x86_64".into(),
            endpoint: "10.0.0.5:8443".into(),
        };
        let json = serde_json::to_string(&t).unwrap();
        let back: ProvisionTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(back.mac, "aa:bb:cc:dd:ee:ff");
        assert_eq!(back.ip.as_deref(), Some("10.0.0.5"));
        assert_eq!(back.arch, "x86_64");
        assert_eq!(back.endpoint, "10.0.0.5:8443");
    }

    #[test]
    fn provision_target_dhcp_no_ip_roundtrip() {
        let t = ProvisionTarget {
            mac: "aa:bb".into(),
            ip: None,
            arch: "aarch64".into(),
            endpoint: "[::1]:8443".into(),
        };
        let json = serde_json::to_string(&t).unwrap();
        let back: ProvisionTarget = serde_json::from_str(&json).unwrap();
        assert!(back.ip.is_none());
        assert_eq!(back.arch, "aarch64");
    }

    #[test]
    fn provision_config_serde_roundtrip() {
        let c = ProvisionConfig {
            base_image: "/img/base.squashfs".into(),
            root_password_hash: "$6$rounds=5000$abc".into(),
            zfs_pool_disks: vec!["/dev/sda".into(), "/dev/sdb".into()],
            network_config: serde_json::json!({"ip": "10.0.0.5", "gw": "10.0.0.1"}),
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: ProvisionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.base_image, "/img/base.squashfs");
        assert_eq!(back.root_password_hash, "$6$rounds=5000$abc");
        assert_eq!(back.zfs_pool_disks.len(), 2);
        assert_eq!(back.network_config["ip"], "10.0.0.5");
    }

    #[test]
    fn provision_status_booting_roundtrip() {
        let s = ProvisionStatus::Booting;
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"phase\":\"booting\""));
        let back: ProvisionStatus = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, ProvisionStatus::Booting));
    }

    #[test]
    fn provision_status_installing_roundtrip() {
        let s = ProvisionStatus::Installing;
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"phase\":\"installing\""));
        let back: ProvisionStatus = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, ProvisionStatus::Installing));
    }

    #[test]
    fn provision_status_forming_pool_roundtrip() {
        let s = ProvisionStatus::FormingPool;
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"phase\":\"forming_pool\""));
        let back: ProvisionStatus = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, ProvisionStatus::FormingPool));
    }

    #[test]
    fn provision_status_ready_roundtrip() {
        let s = ProvisionStatus::Ready {
            node_id: os_core::NodeId::new("node-7"),
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"phase\":\"ready\""));
        let back: ProvisionStatus = serde_json::from_str(&json).unwrap();
        match back {
            ProvisionStatus::Ready { node_id } => assert_eq!(node_id.as_str(), "node-7"),
            _ => panic!("应反序列化为 Ready"),
        }
    }

    #[test]
    fn provision_status_failed_roundtrip() {
        let s = ProvisionStatus::Failed {
            reason: "network unreachable".into(),
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"phase\":\"failed\""));
        let back: ProvisionStatus = serde_json::from_str(&json).unwrap();
        match back {
            ProvisionStatus::Failed { reason } => assert_eq!(reason, "network unreachable"),
            _ => panic!("应反序列化为 Failed"),
        }
    }
}
