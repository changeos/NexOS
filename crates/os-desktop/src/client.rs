//! 客户端契约——复用 os-mobile（桌面/手机两端共享同一客户端 trait）
//!
//! 为避免重复定义，本模块仅做 `pub use` 重导出 os-mobile 的客户端契约。
//! 桌面特有的「挂载」能力见 `mount` 模块。

pub use os_mobile::client::{ClientSession, OsClient, SystemStatus};

// ----------------------------------------------------------------------------
// 单元测试——验证重导出与客户端 model serde 往返（桌面端视角）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use os_core::{Capacity, Health};

    #[test]
    fn reexports_are_usable() {
        // 重导出的类型在桌面端可见且可构造
        let s = SystemStatus {
            hostname: "desktop-os".into(),
            version: "1.0.0".into(),
            capacity: Capacity {
                used_bytes: 0,
                total_bytes: 1,
            },
            health: Health::Healthy,
            node_count: 1,
        };
        assert_eq!(s.hostname, "desktop-os");
        assert_eq!(s.node_count, 1);
    }

    #[test]
    fn client_session_serde_roundtrip_via_reexport() {
        let s = ClientSession {
            endpoint: "https://os:8443".into(),
            token: "tok".into(),
            user: "admin".into(),
            expires_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: ClientSession = serde_json::from_str(&json).unwrap();
        assert_eq!(back.endpoint, s.endpoint);
        assert_eq!(back.token, s.token);
    }

    #[test]
    fn system_status_serde_roundtrip_via_reexport() {
        let s = SystemStatus {
            hostname: "h".into(),
            version: "v".into(),
            capacity: Capacity {
                used_bytes: 50,
                total_bytes: 100,
            },
            health: Health::Degraded,
            node_count: 2,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: SystemStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back.hostname, "h");
        assert_eq!(back.capacity.used_bytes, 50);
        assert_eq!(back.health, Health::Degraded);
        assert_eq!(back.node_count, 2);
    }
}
