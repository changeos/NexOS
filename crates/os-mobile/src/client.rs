//! OS 客户端——发现/连接/状态/配对（客户端侧契约，复用 os-discover 协议）
//!
//! 该 trait 同时被 os-mobile（手机）与 os-desktop（桌面）复用——os-desktop 通过
//! `pub use os_mobile::client::{OsClient, ClientSession, SystemStatus};` 引入，避免重复定义。

use os_core::{Capacity, DateTime, Deserialize, Health, Serialize};
use os_discover::PeerNode;

use crate::MobileError;

// ----------------------------------------------------------------------------
// 客户端会话 / 系统状态
// ----------------------------------------------------------------------------

/// 客户端会话（connect / pair 成功后建立）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientSession {
    /// OS 端点
    pub endpoint: String,
    /// 认证 token（后续请求携带）
    pub token: String,
    /// 登录用户
    pub user: String,
    /// 会话过期时间
    pub expires_at: DateTime,
}

/// OS 系统状态（客户端展示用，聚合自网关 `/status`）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemStatus {
    /// 主机名
    pub hostname: String,
    /// 软件版本
    pub version: String,
    /// 存储容量
    pub capacity: Capacity,
    /// 整体健康度
    pub health: Health,
    /// 集群节点数（1 = 单机）
    pub node_count: u32,
}

// ----------------------------------------------------------------------------
// OsClient trait（async）
// ----------------------------------------------------------------------------

/// OS 客户端——发现、连接、查询状态、配对。
#[allow(async_fn_in_trait)]
pub trait OsClient: Send + Sync {
    /// 连接到 OS 端点；`token` 为 None 时进入匿名/未认证会话。
    async fn connect(
        &self,
        endpoint: &str,
        token: Option<&str>,
    ) -> Result<ClientSession, MobileError>;

    /// 断开当前会话。
    async fn disconnect(&self) -> Result<(), MobileError>;

    /// 查询远端 OS 系统状态。
    async fn get_system_status(&self) -> Result<SystemStatus, MobileError>;

    /// 调用 os-discover 协议，发现局域网内的节点。
    async fn discover_nodes(&self) -> Result<Vec<PeerNode>, MobileError>;

    /// 用配对码与 OS 建立配对会话（首次绑定时使用）。
    async fn pair(&self, endpoint: &str, pairing_code: &str) -> Result<ClientSession, MobileError>;
}

// ----------------------------------------------------------------------------
// 单元测试——ClientSession / SystemStatus 构造 + serde 往返
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use os_core::{Capacity, Health};

    fn sample_session() -> ClientSession {
        ClientSession {
            endpoint: "https://os:8443".into(),
            token: "tok-xyz".into(),
            user: "admin".into(),
            expires_at: chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        }
    }

    fn sample_status() -> SystemStatus {
        SystemStatus {
            hostname: "os-01".into(),
            version: "1.2.3".into(),
            capacity: Capacity {
                used_bytes: 100,
                total_bytes: 1000,
            },
            health: Health::Healthy,
            node_count: 3,
        }
    }

    #[test]
    fn client_session_serde_roundtrip() {
        let s = sample_session();
        let json = serde_json::to_string(&s).unwrap();
        let back: ClientSession = serde_json::from_str(&json).unwrap();
        assert_eq!(back.endpoint, s.endpoint);
        assert_eq!(back.token, s.token);
        assert_eq!(back.user, s.user);
        assert_eq!(back.expires_at, s.expires_at);
    }

    #[test]
    fn client_session_serde_minimal() {
        let s = ClientSession {
            endpoint: "".into(),
            token: "".into(),
            user: "".into(),
            expires_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&s).unwrap();
        // 字段名检查
        assert!(json.contains("\"endpoint\""));
        assert!(json.contains("\"token\""));
        assert!(json.contains("\"user\""));
        assert!(json.contains("\"expires_at\""));
        let back: ClientSession = serde_json::from_str(&json).unwrap();
        assert_eq!(back.endpoint, "");
    }

    #[test]
    fn client_session_serde_missing_field_errors() {
        // 缺 token 字段
        let r: Result<ClientSession, _> = serde_json::from_str(
            r#"{"endpoint":"e","user":"u","expires_at":"2026-01-01T00:00:00Z"}"#,
        );
        assert!(r.is_err());
    }

    #[test]
    fn system_status_serde_roundtrip() {
        let s = sample_status();
        let json = serde_json::to_string(&s).unwrap();
        let back: SystemStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back.hostname, s.hostname);
        assert_eq!(back.version, s.version);
        assert_eq!(back.capacity.used_bytes, s.capacity.used_bytes);
        assert_eq!(back.capacity.total_bytes, s.capacity.total_bytes);
        assert_eq!(back.health, s.health);
        assert_eq!(back.node_count, s.node_count);
    }

    #[test]
    fn system_status_serde_all_health_variants() {
        for h in [
            Health::Healthy,
            Health::Degraded,
            Health::Unhealthy,
            Health::Unknown,
        ] {
            let s = SystemStatus {
                hostname: "h".into(),
                version: "v".into(),
                capacity: Capacity {
                    used_bytes: 0,
                    total_bytes: 0,
                },
                health: h,
                node_count: 0,
            };
            let json = serde_json::to_string(&s).unwrap();
            let back: SystemStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back.health, h, "health {h:?} 往返失败");
        }
    }

    #[test]
    fn system_status_serde_health_snake_case() {
        // Health 序列化为 snake_case
        let s = SystemStatus {
            hostname: "h".into(),
            version: "v".into(),
            capacity: Capacity {
                used_bytes: 0,
                total_bytes: 0,
            },
            health: Health::Degraded,
            node_count: 0,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"health\":\"degraded\""));
    }

    #[test]
    fn system_status_serde_invalid_health_errors() {
        let r: Result<SystemStatus, _> = serde_json::from_str(
            r#"{"hostname":"h","version":"v","capacity":{"used_bytes":0,"total_bytes":0},"health":"not_a_health","node_count":0}"#,
        );
        assert!(r.is_err());
    }

    #[test]
    fn client_session_clone_eq_debug() {
        // Clone + Debug 派生间接覆盖
        let s = sample_session();
        let s2 = s.clone();
        assert_eq!(s.endpoint, s2.endpoint);
        assert_eq!(s.token, s2.token);
        let _dbg = format!("{:?}", s);
    }

    #[test]
    fn system_status_clone_eq_debug() {
        let s = sample_status();
        let s2 = s.clone();
        assert_eq!(s.hostname, s2.hostname);
        let _dbg = format!("{:?}", s);
    }
}
