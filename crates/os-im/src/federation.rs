//! Federation 协议——跨 OS 节点互联 + 消息同步（规划文档 §3.7 扩展）。
//!
//! 定位：让不同 OS 节点互相发现、认证、互联，使"别的 IM 通过 IP 接入群"成为可能。
//!
//! 与 [`crate::transport`] 的分工：`transport` 提供节点间 TCP 连接与消息信封（物理层），
//! `federation` 在其上定义**信任模型 + 握手协议 + 节点注册表**（会话/应用层）。
//! 生产实现中 federation manager 通常通过 `P2pTransport` 收发 [`FederationHandshake`]。
//!
//! 协议分三层：
//! 1. **发现**：mDNS/UDP 广播或手动添加（`endpoint = IP:port`）。
//! 2. **信任**：未信任节点只能完成 `Hello`/`Welcome`，不能加入群；
//!    交换公钥（或预共享密钥）后 `trust_node` 升级为信任节点。
//! 3. **握手**：`Hello → Welcome(+challenge) → Auth(签名) → Ready(session_token)`。
//!
//! 契约规范（与 crate 其余 trait 一致）：本 trait 需 `Box<dyn FederationManager>`
//! 运行期多态，故使用 `#[async_trait]`；错误复用 [`crate::ImError]。
//!
//! 实现见 [`crate::federation_impl::LocalFederationManager`]。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::ImError;

// ----------------------------------------------------------------------------
// 节点身份与能力
// ----------------------------------------------------------------------------

/// Federation 节点身份。
///
/// 一个节点 = 一台远端 OS（`endpoint = IP:port`）。信任前 `public_key`/`trusted`
/// 可能为空/false；`trust_node` 后交换（或核对）公钥并置 `trusted = true`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederationNode {
    /// 节点唯一 ID（建议用公钥指纹或 hostname+随机后缀）
    pub node_id: String,
    /// 接入端点 `IP:port`
    pub endpoint: String,
    /// 节点展示名
    pub display_name: String,
    /// Ed25519 公钥（认证用；信任前可能为空）
    pub public_key: Option<String>,
    /// 是否已信任（信任后才能参与 Federation 群同步）
    pub trusted: bool,
    /// 支持的功能集
    pub capabilities: NodeCapabilities,
}

/// 节点能力广告——发现/握手时交换，决定该节点可参与哪些联邦功能。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeCapabilities {
    /// 是否支持 IM 联邦
    pub im: bool,
    /// 是否支持存储联邦（跨节点卷/快照同步）
    pub storage: bool,
    /// 是否支持计算联邦（跨节点 agent 委派）
    pub compute: bool,
    /// 节点协议版本
    pub version: String,
}

impl NodeCapabilities {
    /// 创建一个仅开启 IM 的最小能力集（最常见情形：联邦群聊）。
    pub fn im_only(version: impl Into<String>) -> Self {
        Self {
            im: true,
            storage: false,
            compute: false,
            version: version.into(),
        }
    }
}

// ----------------------------------------------------------------------------
// 握手协议
// ----------------------------------------------------------------------------

/// Federation 握手消息（四步握手：`Hello → Welcome → Auth → Ready`）。
///
/// - 未信任节点只能完成 `Hello`/`Welcome`（`Welcome.accepted = false` 或带挑战但
///   拒绝 `Ready`），不能获取 `session_token`。
/// - 信任节点完成全流程后拿到 `session_token`，凭此参与联邦群同步。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum FederationHandshake {
    /// 第 1 步：发起方亮明身份与能力。
    Hello {
        node_id: String,
        display_name: String,
        capabilities: NodeCapabilities,
    },
    /// 第 2 步：接收方回应——是否接受 + 挑战值（挑战用于后续 Auth 签名）。
    Welcome {
        accepted: bool,
        challenge: Option<String>,
        reason: Option<String>,
    },
    /// 第 3 步：发起方对挑战签名（简化版：预共享密钥 HMAC）。
    Auth { response: String },
    /// 第 4 步：接收方校验通过后颁发会话令牌。
    Ready { session_token: String },
}

// ----------------------------------------------------------------------------
// FederationManager trait
// ----------------------------------------------------------------------------

/// Federation 管理 trait——节点发现 / 信任管理 / 握手驱动。
///
/// 实现者：[`crate::federation_impl::LocalFederationManager`]（内存参考实现）；
/// 生产实现可替换为带 mDNS + 持久化的版本。
#[async_trait]
pub trait FederationManager: Send + Sync {
    /// 发现局域网内的 OS 节点（mDNS/UDP 广播）。
    async fn discover_nodes(&self) -> Vec<FederationNode>;

    /// 手动添加节点（通过 `IP:port`）。
    async fn add_node(&self, endpoint: &str) -> Result<FederationNode, ImError>;

    /// 信任节点（交换/核对公钥）。信任后才能参与 Federation 群同步。
    async fn trust_node(&self, node_id: &str) -> Result<(), ImError>;

    /// 拒绝/吊销节点。
    async fn revoke_node(&self, node_id: &str) -> Result<(), ImError>;

    /// 列出已知节点。
    async fn list_nodes(&self) -> Vec<FederationNode>;

    /// 发送 Federation 握手（客户端侧：向 `endpoint` 发起 Hello）。
    async fn handshake(&self, endpoint: &str) -> Result<FederationHandshake, ImError>;

    /// 接收并处理 Federation 握手（服务端侧：回应对方消息）。
    async fn handle_handshake(
        &self,
        msg: FederationHandshake,
    ) -> Result<FederationHandshake, ImError>;
}
