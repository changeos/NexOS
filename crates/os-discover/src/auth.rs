//! 凭证配对互联——mTLS 双向认证建立 peer 会话
//!
//! 决策依据：规划文档 §3.14 —— 发现到 peer 后，需用配对凭证（PairingToken）
//! 完成 mTLS 双向认证，建立受信任的 peer 会话，后续同步/管理流量走该 mTLS 通道。

use os_core::{DateTime, Deserialize, NodeId, Serialize, Utc};

// ----------------------------------------------------------------------------
// PairingToken / PairingScope
// ----------------------------------------------------------------------------

/// 配对凭证作用域
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingScope {
    /// 加入 HA 集群（成为法定成员）
    JoinCluster,
    /// 仅 peer 同步（不进法定，如 ZFS mirror 对端）
    PeerSync,
    /// 客户端访问（手机/桌面客户端接入）
    ClientAccess,
}

/// 配对凭证（一次性/短期，由已信任方签发给待加入方）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingToken {
    /// 凭证字符串
    pub token: String,
    /// 过期时间
    pub expires_at: DateTime,
    /// 签发方节点 ID
    pub issued_by: NodeId,
    /// 作用域
    pub scope: PairingScope,
}

// ----------------------------------------------------------------------------
// PeerSessionId / PeerSession
// ----------------------------------------------------------------------------

/// peer 会话 ID（newtype String）
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PeerSessionId(pub String);

impl PeerSessionId {
    /// 从任意字符串构造
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    /// 取字符串切片
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PeerSessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for PeerSessionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// 已建立的 peer 会话（mTLS 双向认证完成）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerSession {
    /// 会话 ID
    pub id: PeerSessionId,
    /// 对端节点 ID
    pub peer: NodeId,
    /// 建立时间
    pub established_at: DateTime,
    /// 对端 mTLS 证书指纹（SHA-256）
    pub mtls_cert_fingerprint: String,
}

impl PeerSession {
    /// 构造一个当前时间戳的会话
    pub fn new(id: PeerSessionId, peer: NodeId, mtls_cert_fingerprint: impl Into<String>) -> Self {
        Self {
            id,
            peer,
            established_at: Utc::now(),
            mtls_cert_fingerprint: mtls_cert_fingerprint.into(),
        }
    }
}

// ----------------------------------------------------------------------------
// PeerAuthenticator trait（async，mTLS）
// ----------------------------------------------------------------------------

/// peer 认证器——基于配对凭证完成 mTLS 双向认证，建立/管理 peer 会话。
///
/// 实现者：`MtlsPeerAuthenticator`（默认，基于 rustls + 本地受信证书库）；
/// 与 os-security CertManager 协同（证书签发/校验）。
#[allow(async_fn_in_trait)]
pub trait PeerAuthenticator: Send + Sync {
    /// 配对——用凭证与对端完成 mTLS 双向认证，建立 peer 会话。
    ///
    /// - `peer_endpoint`：对端接入地址（如 "10.0.0.5:8443"）
    /// - `token`：配对凭证
    async fn pair(
        &self,
        peer_endpoint: &str,
        token: &PairingToken,
    ) -> Result<PeerSession, crate::DiscoverError>;

    /// 解除配对（断开并移除受信关系）。
    async fn unpair(&self, session: &PeerSessionId) -> Result<(), crate::DiscoverError>;

    /// 列出当前受信任的 peer 节点。
    async fn list_trusted_peers(&self) -> Vec<NodeId>;
}
