//! LAN 节点发现——mDNS / 组播 beacon
//!
//! 决策依据：规划文档 §3.14 —— 节点在局域网广播自身（含能力声明 + 防伪签名），
//! 并扫描发现其他节点。`beacon_signature` 防伪：beacon 由节点私钥签名，
//! 发现方用预置公钥/凭证校验，避免伪造节点混入。

use async_trait::async_trait;
use os_core::{Deserialize, NodeId, Serialize};

// ----------------------------------------------------------------------------
// PeerNode / NodeCapabilities
// ----------------------------------------------------------------------------

/// 节点能力声明（HA 资格检测的硬指标输入）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeCapabilities {
    /// 是否支持 HA（具备共识/故障转移能力）
    pub supports_ha: bool,
    /// 存储容量（GB）
    pub storage_capacity_gb: u64,
    /// 网络带宽（Gbps）
    pub network_gbps: f32,
    /// 是否具备 ZFS（HA 存储复制依赖）
    pub has_zfs: bool,
    /// 是否具备 KVM（VM 迁移依赖）
    pub has_kvm: bool,
    /// 是否具备 RDMA（IB-RoCE）
    pub rdma: bool,
    /// 是否具备 DPU（带内/带外加速）
    pub dpu: bool,
}

/// 发现到的对端节点
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerNode {
    /// 节点 ID
    pub node_id: NodeId,
    /// 可用接入端点（含端口，如 ["10.0.0.5:8443"]）
    pub endpoints: Vec<String>,
    /// 软件版本
    pub version: String,
    /// 系统架构（如 "x86_64" / "aarch64"）
    pub arch: String,
    /// 能力声明
    pub capabilities: NodeCapabilities,
    /// beacon 防伪签名（节点私钥签名 beacon 载荷；None = 未签名）
    pub beacon_signature: Option<String>,
}

// ----------------------------------------------------------------------------
// Discovery trait + PeerCallback（async）
// ----------------------------------------------------------------------------

/// LAN 节点发现——基于 mDNS / 组播 beacon 广播与扫描。
///
/// 实现者：`MdnsDiscovery`（默认，基于 mdns/组播）；其他实现可替换。
/// 生命周期：`start_advertising` 后持续广播；`discover_peers` 为一次性扫描。
#[allow(async_fn_in_trait)]
pub trait Discovery: Send + Sync {
    /// 开始广播自身（周期性发送 beacon，含 `self_info` 与防伪签名）。
    async fn start_advertising(&self, self_info: PeerNode) -> Result<(), crate::DiscoverError>;

    /// 停止广播自身。
    async fn stop_advertising(&self) -> Result<(), crate::DiscoverError>;

    /// 主动扫描局域网，返回在 `timeout_ms` 内应答的 peer 列表。
    async fn discover_peers(&self, timeout_ms: u32) -> Result<Vec<PeerNode>, crate::DiscoverError>;

    /// 注册 peer 发现/失联回调（持续扫描模式下推送事件）。
    async fn on_peer_discovered(&self, callback: Box<dyn PeerCallback>);
}

/// peer 事件回调（发现新 peer / peer 失联）。
///
/// 经 `Box<dyn PeerCallback>` 注册到 Discovery，故用 `#[async_trait]` 保证 dyn 兼容（ADR-COMPAT-001）。
#[async_trait]
pub trait PeerCallback: Send + Sync {
    /// 发现新 peer（或已有 peer 更新信息）时调用。
    async fn on_found(&self, peer: &PeerNode);

    /// peer 失联（多次探测无应答）时调用。
    async fn on_lost(&self, node_id: &NodeId);
}
