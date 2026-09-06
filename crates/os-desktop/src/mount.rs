//! 一键挂载为网络驱动器（规划文档 §3.15 桌面独有）
//!
//! 职责：
//! - 列举远端 OS 上可挂载的共享（SMB / WebDAV）
//! - 挂载到本地（Windows 用 `net use`；Linux 用 davfs2 / 原生内核挂载）
//! - 卸载 / 列举已挂载 / 设为开机自动挂载（persistent）

use std::path::PathBuf;

use os_core::{Deserialize, Serialize};

use crate::DesktopError;

// ----------------------------------------------------------------------------
// 挂载协议 / 目标 / 信息 / 远端共享
// ----------------------------------------------------------------------------

/// 挂载协议
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountProtocol {
    /// SMB / CIFS（Windows 共享）
    Smb,
    /// WebDAV
    Webdav,
}

/// 挂载目标（描述「把谁挂到哪、用什么协议」）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountTarget {
    /// OS 端点
    pub endpoint: String,
    /// 远端共享路径（如 `"photos"` / `"backup"`）
    pub share_path: String,
    /// 挂载协议
    pub protocol: MountProtocol,
    /// Windows 盘符（如 `"Z:"`；None = 自动分配 / 非 Windows）
    pub drive_letter: Option<String>,
    /// 跨平台挂载点（Linux/macOS 路径；None = 由实现侧决定）
    pub mount_point: Option<PathBuf>,
}

/// 挂载信息（描述一个已挂载/可挂载的项）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountInfo {
    /// 挂载目标
    pub target: MountTarget,
    /// 是否已挂载
    pub mounted: bool,
    /// 实际挂载路径（盘符或挂载点；未挂载时为 None）
    pub mount_path: Option<String>,
    /// 是否设为开机自动挂载
    pub persistent: bool,
}

/// 远端可挂载的共享（list_available_shares 返回项）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteShare {
    /// 共享名
    pub name: String,
    /// 协议
    pub protocol: MountProtocol,
    /// 描述（None = 无描述）
    pub description: Option<String>,
}

// ----------------------------------------------------------------------------
// MountManager trait（async）
// ----------------------------------------------------------------------------

/// 挂载管理器——列举、挂载、卸载、持久化网络驱动器。
#[allow(async_fn_in_trait)]
pub trait MountManager: Send + Sync {
    /// 列举指定 OS 端点上可挂载的共享。
    async fn list_available_shares(&self, endpoint: &str)
        -> Result<Vec<RemoteShare>, DesktopError>;

    /// 挂载（Windows 用 `net use`；WebDAV 用 davfs2 / 原生）。
    async fn mount(&self, target: MountTarget) -> Result<MountInfo, DesktopError>;

    /// 卸载指定挂载。
    async fn unmount(&self, mount_id: &str) -> Result<(), DesktopError>;

    /// 列举所有已挂载项。
    async fn list_mounts(&self) -> Result<Vec<MountInfo>, DesktopError>;

    /// 将挂载设为开机自动挂载（写入注册表 / fstab）。
    async fn make_persistent(&self, mount_id: &str) -> Result<(), DesktopError>;
}

// ----------------------------------------------------------------------------
// 单元测试——协议枚举 + 各 model serde 往返 + 边界
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample_smb_target() -> MountTarget {
        MountTarget {
            endpoint: "https://os:8443".into(),
            share_path: "photos".into(),
            protocol: MountProtocol::Smb,
            drive_letter: Some("Z:".into()),
            mount_point: None,
        }
    }

    fn sample_webdav_target() -> MountTarget {
        MountTarget {
            endpoint: "https://os:8443".into(),
            share_path: "backup".into(),
            protocol: MountProtocol::Webdav,
            drive_letter: None,
            mount_point: Some(PathBuf::from("/mnt/os")),
        }
    }

    // —— MountProtocol serde + 派生 ——

    #[test]
    fn mount_protocol_serde_snake_case_roundtrip() {
        for (p, snake) in [
            (MountProtocol::Smb, "smb"),
            (MountProtocol::Webdav, "webdav"),
        ] {
            let s = serde_json::to_string(&p).unwrap();
            assert_eq!(s, format!("\"{snake}\""));
            let back: MountProtocol = serde_json::from_str(&s).unwrap();
            assert_eq!(back, p);
        }
    }

    #[test]
    fn mount_protocol_serde_invalid_errors() {
        let r: Result<MountProtocol, _> = serde_json::from_str("\"ftp\"");
        assert!(r.is_err());
    }

    #[test]
    fn mount_protocol_equality_copy_debug() {
        // Copy + PartialEq + Eq + Debug
        let p1 = MountProtocol::Smb;
        let p2 = p1;
        assert_eq!(p1, p2);
        assert_ne!(MountProtocol::Smb, MountProtocol::Webdav);
        let _dbg = format!("{:?}", p1);
    }

    // —— MountTarget serde 往返 ——

    #[test]
    fn mount_target_serde_roundtrip_smb() {
        let t = sample_smb_target();
        let json = serde_json::to_string(&t).unwrap();
        let back: MountTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(back.endpoint, t.endpoint);
        assert_eq!(back.share_path, t.share_path);
        assert_eq!(back.protocol, t.protocol);
        assert_eq!(back.drive_letter, t.drive_letter);
        assert_eq!(back.mount_point, t.mount_point);
    }

    #[test]
    fn mount_target_serde_roundtrip_webdav() {
        let t = sample_webdav_target();
        let json = serde_json::to_string(&t).unwrap();
        let back: MountTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(back.endpoint, t.endpoint);
        assert_eq!(back.mount_point, t.mount_point);
        assert_eq!(back.protocol, MountProtocol::Webdav);
    }

    #[test]
    fn mount_target_serde_drive_letter_none_is_null() {
        let t = sample_webdav_target();
        let json = serde_json::to_string(&t).unwrap();
        assert!(json.contains("\"drive_letter\":null"));
    }

    #[test]
    fn mount_target_serde_mount_point_none_is_null() {
        let t = sample_smb_target();
        let json = serde_json::to_string(&t).unwrap();
        assert!(json.contains("\"mount_point\":null"));
    }

    #[test]
    fn mount_target_serde_missing_protocol_errors() {
        let r: Result<MountTarget, _> = serde_json::from_str(
            r#"{"endpoint":"e","share_path":"s","drive_letter":null,"mount_point":null}"#,
        );
        assert!(r.is_err());
    }

    #[test]
    fn mount_target_clone_debug() {
        let t = sample_smb_target();
        let t2 = t.clone();
        assert_eq!(t.endpoint, t2.endpoint);
        let _dbg = format!("{:?}", t);
    }

    // —— MountInfo serde 往返 ——

    #[test]
    fn mount_info_serde_roundtrip() {
        let info = MountInfo {
            target: sample_smb_target(),
            mounted: true,
            mount_path: Some("Z:".into()),
            persistent: false,
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: MountInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.target.endpoint, info.target.endpoint);
        assert!(back.mounted);
        assert_eq!(back.mount_path.as_deref(), Some("Z:"));
        assert!(!back.persistent);
    }

    #[test]
    fn mount_info_serde_unmounted_state() {
        let info = MountInfo {
            target: sample_webdav_target(),
            mounted: false,
            mount_path: None,
            persistent: false,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"mounted\":false"));
        assert!(json.contains("\"mount_path\":null"));
        assert!(json.contains("\"persistent\":false"));
    }

    #[test]
    fn mount_info_serde_persistent_state() {
        let info = MountInfo {
            target: sample_smb_target(),
            mounted: true,
            mount_path: Some("Z:".into()),
            persistent: true,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"persistent\":true"));
    }

    // —— RemoteShare serde 往返 ——

    #[test]
    fn remote_share_serde_roundtrip() {
        let s = RemoteShare {
            name: "photos".into(),
            protocol: MountProtocol::Smb,
            description: Some("相册".into()),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: RemoteShare = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "photos");
        assert_eq!(back.protocol, MountProtocol::Smb);
        assert_eq!(back.description.as_deref(), Some("相册"));
    }

    #[test]
    fn remote_share_serde_no_description() {
        let s = RemoteShare {
            name: "backup".into(),
            protocol: MountProtocol::Webdav,
            description: None,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"description\":null"));
        let back: RemoteShare = serde_json::from_str(&json).unwrap();
        assert!(back.description.is_none());
    }

    #[test]
    fn remote_share_serde_missing_protocol_errors() {
        let r: Result<RemoteShare, _> = serde_json::from_str(r#"{"name":"x","description":null}"#);
        assert!(r.is_err());
    }

    #[test]
    fn remote_share_clone_debug() {
        let s = RemoteShare {
            name: "x".into(),
            protocol: MountProtocol::Smb,
            description: None,
        };
        let s2 = s.clone();
        assert_eq!(s.name, s2.name);
        let _dbg = format!("{:?}", s);
    }
}
