//! NTP 管理契约（规划文档 §9.1#8 决策：NTP 由 osd 统管）
//!
//! HA 集群要求所有节点时钟一致（证书有效期/raft log 时序/审计时间戳都依赖此）。
//! osd 内嵌 NTP 客户端，作为系统时钟权威来源，避免依赖外部 ntpd 造成双源冲突。

use os_core::{DateTime, Deserialize, Serialize};

/// NTP 同步状态快照
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtpStatus {
    /// 是否已同步（与上游 server 偏移在容忍范围内）
    pub synced: bool,
    /// 本地时钟与上游的偏移（毫秒；正=本地快，负=本地慢）
    pub offset_ms: i64,
    /// 最近一次成功同步时间
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_sync: Option<DateTime>,
    /// 当前配置的上游 NTP 服务器列表
    pub servers: Vec<String>,
}

/// NTP 管理器 trait（异步）
///
/// 实现者：osd 内嵌的 NTP 子系统（基于 chrono + 自实现/绑定的 NTP 客户端）。
pub trait NtpManager: Send + Sync {
    /// 立即触发一次同步，返回同步完成后的当前时间
    ///
    /// 失败原因：上游不可达 / 偏移过大无法修正，详见 [`crate::OrchestratorError::NtpSyncFailed`]
    async fn sync_now(&self) -> crate::OrchestratorResult<DateTime>;

    /// 查询当前 NTP 同步状态（不触发同步）
    async fn status(&self) -> NtpStatus;

    /// 设置上游 NTP 服务器列表（运行时热更新，无需重启）
    async fn set_servers(&self, servers: Vec<String>) -> crate::OrchestratorResult<()>;
}

// 注：`ChronyNtp`（`NtpManager` 的真实实现）**本次不做**——依赖 chrony 绑定
// （未在 workspace 注册，规格 §9 红线：严禁虚构依赖）+ `CAP_SYS_TIME`（root）。
// 待 chrony 绑定经 ReviewAgent 注册 + root 集成测环境就绪后再实现。

#[cfg(test)]
mod tests {
    use super::NtpStatus;

    #[test]
    fn ntp_status_serializes_with_servers() {
        let s = NtpStatus {
            synced: true,
            offset_ms: 12,
            last_sync: None,
            servers: vec!["pool.ntp.org".into()],
        };
        let json = serde_json::to_string(&s).expect("序列化");
        assert!(json.contains("pool.ntp.org"));
        assert!(json.contains("\"synced\":true"));
        // last_sync 为 None 时应被 skip
        assert!(!json.contains("last_sync"));
    }

    #[test]
    fn ntp_status_roundtrip() {
        let s = NtpStatus {
            synced: false,
            offset_ms: -5,
            last_sync: None,
            servers: vec![],
        };
        let json = serde_json::to_string(&s).expect("序列化");
        let back: NtpStatus = serde_json::from_str(&json).expect("反序列化");
        assert!(!back.synced);
        assert_eq!(back.offset_ms, -5);
        assert!(back.servers.is_empty());
        assert!(back.last_sync.is_none());
    }

    #[test]
    fn ntp_status_default_like_unsynced() {
        // 验证一个典型"未同步"快照的结构稳定（不依赖 DateTime 具体值）
        let s = NtpStatus {
            synced: false,
            offset_ms: 0,
            last_sync: None,
            servers: vec!["0.pool.ntp.org".into(), "1.pool.ntp.org".into()],
        };
        assert!(!s.synced);
        assert_eq!(s.servers.len(), 2);
    }
}
