//! VPN（WireGuard via boringtun）
//!
//! 决策依据：规划文档 §3.16 —— 远程接入用 WireGuard（用户态 boringtun，免内核模块依赖）。

use crate::auth::UserId;
use os_network::IpCidr;
use serde::{Deserialize, Serialize};

// ----------------------------------------------------------------------------
// VPN peer 与状态
// ----------------------------------------------------------------------------

/// VPN 对端（WireGuard peer）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VpnPeer {
    /// 对端公钥（base64）
    pub public_key: String,
    /// 允许的源地址/网段（AllowedIPs）
    pub allowed_ips: Vec<IpCidr>,
    /// 对端端点（host:port；None = 等待对端连入）
    pub endpoint: Option<String>,
    /// 关联用户（可选，便于审计）
    pub user: Option<UserId>,
}

/// VPN 运行状态
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VpnStatus {
    /// 是否在运行
    pub running: bool,
    /// 监听端口
    pub listen_port: u16,
    /// 在线 peer 数
    pub peer_count: u32,
    /// 累计接收字节
    pub bytes_rx: u64,
    /// 累计发送字节
    pub bytes_tx: u64,
}

// ----------------------------------------------------------------------------
// VpnManager trait（async）
// ----------------------------------------------------------------------------

/// VPN 管理器——WireGuard peer 增删查与状态。
///
/// 实现者：`BoringtunVpnManager`（基于 boringtun 用户态实现）。
#[allow(async_fn_in_trait)]
pub trait VpnManager: Send + Sync {
    /// 新增 peer。
    async fn add_peer(&self, peer: VpnPeer) -> Result<(), crate::SecurityError>;

    /// 移除 peer（按公钥）。
    async fn remove_peer(&self, pub_key: &str) -> Result<(), crate::SecurityError>;

    /// 列出所有 peer。
    async fn list_peers(&self) -> Result<Vec<VpnPeer>, crate::SecurityError>;

    /// 查询 VPN 运行状态。
    async fn status(&self) -> Result<VpnStatus, crate::SecurityError>;
}
