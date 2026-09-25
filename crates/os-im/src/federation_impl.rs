//! Federation 默认实现——内存参考实现（规划文档 §3.7 扩展）。
//!
//! [`LocalFederationManager`] 提供：
//! - 节点注册表（内存）+ 发现（空，未接 mDNS）
//! - 信任管理：未信任节点只能 `Hello`/`Welcome`，信任后才能走完握手
//! - 四步握手状态机：`Hello → Welcome(+challenge) → Auth(签名) → Ready(session_token)`
//! - 简化版认证：预共享密钥（PSK）做挑战 HMAC；后续可升级 Ed25519 签名
//!
//! 同一 manager 既可作客户端（`handshake`：向某 endpoint 发起 Hello），
//! 也可作服务端（`handle_handshake`：处理远端发来的消息并回下一步）。
//! 测试中用两个 manager 互发消息即可走完完整握手。
//!
//! 所有方法返回 `ImResult<T>`，纯内存、`Send + Sync`（内部用 `Mutex`）。

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::error::ImError;
use crate::federation::{FederationHandshake, FederationManager, FederationNode, NodeCapabilities};

// ============================================================
// 内部状态
// ============================================================

/// 服务端侧每个握手会话的进行中状态。
#[derive(Debug, Clone)]
struct HandshakeState {
    /// 对端 node_id（来自 Hello）。
    node_id: String,
    /// 对端 endpoint（记录用，当前协议未直接消费）。
    #[allow(dead_code)]
    endpoint: String,
    /// 已下发的挑战值（Auth 步骤需对它签名）。
    challenge: String,
    /// 已颁发但未消费的 session_token（Ready 步骤交出）。
    session_token: Option<String>,
}

/// 本节点身份（用于 Hello 自我介绍）。
#[derive(Debug, Clone)]
pub struct LocalNodeIdentity {
    pub node_id: String,
    pub display_name: String,
    pub capabilities: NodeCapabilities,
}

impl LocalNodeIdentity {
    /// 创建本节点身份（默认能力：仅 IM）。
    pub fn new(node_id: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self {
            node_id: node_id.into(),
            display_name: display_name.into(),
            capabilities: NodeCapabilities::im_only(env!("CARGO_PKG_VERSION")),
        }
    }

    /// 自定义能力集。
    pub fn with_capabilities(mut self, caps: NodeCapabilities) -> Self {
        self.capabilities = caps;
        self
    }
}

// ============================================================
// LocalFederationManager
// ============================================================

/// 内存 Federation 管理器——参考实现。
///
/// - `nodes`：已知节点注册表（node_id → [`FederationNode`]）。
/// - `sessions`：服务端侧进行中的握手状态（challenge → state）。
/// - `psk`：预共享密钥（认证用；简化版，后续可换 Ed25519 公钥集）。
/// - `identity`：本节点自我介绍信息。
///
/// 生产实现应替换为带持久化 + 真实网络/mDNS 的版本；本实现聚焦协议正确性。
pub struct LocalFederationManager {
    nodes: Mutex<HashMap<String, FederationNode>>,
    sessions: Mutex<HashMap<String, HandshakeState>>,
    psk: String,
    identity: LocalNodeIdentity,
}

impl LocalFederationManager {
    /// 创建管理器：给定本节点身份 + 预共享密钥。
    pub fn new(identity: LocalNodeIdentity, psk: impl Into<String>) -> Self {
        Self {
            nodes: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            psk: psk.into(),
            identity,
        }
    }

    /// 便捷构造：用 node_id + display_name + 默认 PSK（测试/演示用）。
    pub fn with_defaults(node_id: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self::new(LocalNodeIdentity::new(node_id, display_name), DEFAULT_PSK)
    }

    /// 取本节点身份。
    pub fn identity(&self) -> &LocalNodeIdentity {
        &self.identity
    }

    /// 简化版 HMAC（不引入 hmac crate）：challenge ‖ psk 的 FNV-1a 64 摘要的 hex。
    /// 仅用于演示挑战-响应；生产应替换为 Ed25519 签名。
    fn sign_challenge(&self, challenge: &str) -> String {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for b in challenge
            .as_bytes()
            .iter()
            .chain(self.psk.as_bytes().iter())
        {
            hash ^= u64::from(*b);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        format!("{:016x}", hash)
    }

    /// 生成一个挑战值（确定性：node_id + 版本，便于测试断言）。
    fn make_challenge(node_id: &str) -> String {
        format!("challenge:{node_id}:v1")
    }

    /// 生成 session_token。
    fn make_session_token(node_id: &str) -> String {
        format!("sess:{node_id}:{:x}", chrono::Utc::now().timestamp())
    }
}

/// 默认预共享密钥（仅测试/演示；生产必须显式注入强密钥）。
pub const DEFAULT_PSK: &str = "os-federation-default-psk-CHANGE-ME";

#[async_trait]
impl FederationManager for LocalFederationManager {
    async fn discover_nodes(&self) -> Vec<FederationNode> {
        // 内存实现未接 mDNS：仅返回已注册节点。
        self.nodes.lock().unwrap().values().cloned().collect()
    }

    async fn add_node(&self, endpoint: &str) -> Result<FederationNode, ImError> {
        // 校验 endpoint 形如 IP:port（最简：含 ':' 且端口段为数字）。
        let Some((host, port)) = endpoint.rsplit_once(':') else {
            return Err(ImError::HandshakeFailed(format!(
                "非法 endpoint（应为 IP:port）: {endpoint}"
            )));
        };
        if host.is_empty() || port.parse::<u16>().is_err() {
            return Err(ImError::HandshakeFailed(format!(
                "非法 endpoint（应为 IP:port）: {endpoint}"
            )));
        }

        let mut nodes = self.nodes.lock().unwrap();

        // 重复添加（按 endpoint）→ NodeAlreadyExists。
        if nodes.values().any(|n| n.endpoint == endpoint) {
            return Err(ImError::NodeAlreadyExists(format!(
                "endpoint {endpoint} 已存在"
            )));
        }

        // 派生 node_id（host:port），trusted=false，仅 IM 能力。
        let node_id = format!("fed-{}:{}", host, port);
        let node = FederationNode {
            node_id: node_id.clone(),
            endpoint: endpoint.to_string(),
            display_name: format!("FedNode {endpoint}"),
            public_key: None,
            trusted: false,
            capabilities: NodeCapabilities::im_only(env!("CARGO_PKG_VERSION")),
        };
        nodes.insert(node_id, node.clone());
        Ok(node)
    }

    async fn trust_node(&self, node_id: &str) -> Result<(), ImError> {
        let mut nodes = self.nodes.lock().unwrap();
        let node = nodes
            .get_mut(node_id)
            .ok_or_else(|| ImError::HandshakeFailed(format!("未知节点: {node_id}")))?;
        // 信任即"交换/核对公钥"——简化版：注入占位公钥指纹。
        if node.public_key.is_none() {
            node.public_key = Some(format!("pk:{node_id}"));
        }
        node.trusted = true;
        Ok(())
    }

    async fn revoke_node(&self, node_id: &str) -> Result<(), ImError> {
        let mut nodes = self.nodes.lock().unwrap();
        let node = nodes
            .get_mut(node_id)
            .ok_or_else(|| ImError::HandshakeFailed(format!("未知节点: {node_id}")))?;
        node.trusted = false;
        // 吊销会话令牌：清理该节点进行中的握手状态。
        self.sessions
            .lock()
            .unwrap()
            .retain(|_, st| st.node_id != node_id);
        Ok(())
    }

    async fn list_nodes(&self) -> Vec<FederationNode> {
        self.nodes.lock().unwrap().values().cloned().collect()
    }

    async fn handshake(&self, endpoint: &str) -> Result<FederationHandshake, ImError> {
        // 客户端侧：向 endpoint 发起 Hello。endpoint 仅做格式校验，
        // 不强制要求预注册（实际连接由生产实现的传输层负责）。
        if endpoint
            .rsplit_once(':')
            .map(|(_, p)| p.parse::<u16>().is_ok())
            != Some(true)
        {
            return Err(ImError::HandshakeFailed(format!(
                "非法 endpoint（应为 IP:port）: {endpoint}"
            )));
        }
        Ok(FederationHandshake::Hello {
            node_id: self.identity.node_id.clone(),
            display_name: self.identity.display_name.clone(),
            capabilities: self.identity.capabilities.clone(),
        })
    }

    async fn handle_handshake(
        &self,
        msg: FederationHandshake,
    ) -> Result<FederationHandshake, ImError> {
        match msg {
            // —— 第 1 步：收到 Hello ——
            FederationHandshake::Hello {
                node_id,
                display_name,
                capabilities,
            } => {
                // 注册/更新对端节点（未信任）。
                {
                    let mut nodes = self.nodes.lock().unwrap();
                    let entry = nodes
                        .entry(node_id.clone())
                        .or_insert_with(|| FederationNode {
                            node_id: node_id.clone(),
                            endpoint: String::new(),
                            display_name: display_name.clone(),
                            public_key: None,
                            trusted: false,
                            capabilities: capabilities.clone(),
                        });
                    entry.display_name = display_name;
                    entry.capabilities = capabilities;
                }
                // 生成挑战并记录会话状态。
                let challenge = Self::make_challenge(&node_id);
                self.sessions.lock().unwrap().insert(
                    challenge.clone(),
                    HandshakeState {
                        node_id: node_id.clone(),
                        endpoint: String::new(),
                        challenge: challenge.clone(),
                        session_token: None,
                    },
                );
                Ok(FederationHandshake::Welcome {
                    accepted: true,
                    challenge: Some(challenge),
                    reason: None,
                })
            }
            // —— 第 2 步：收到 Welcome ——
            FederationHandshake::Welcome {
                accepted,
                challenge,
                reason,
            } => {
                if !accepted {
                    return Err(ImError::HandshakeFailed(
                        reason.unwrap_or_else(|| "对端拒绝握手".to_string()),
                    ));
                }
                let Some(challenge) = challenge else {
                    return Err(ImError::HandshakeFailed("Welcome 未携带挑战值".to_string()));
                };
                // 客户端用 PSK 对挑战签名。
                let response = self.sign_challenge(&challenge);
                Ok(FederationHandshake::Auth { response })
            }
            // —— 第 3 步：收到 Auth ——
            FederationHandshake::Auth { response } => {
                // 校验：找到其挑战签名与 response 匹配的会话。
                let mut sessions = self.sessions.lock().unwrap();
                let key = sessions
                    .iter()
                    .find(|(_, st)| self.sign_challenge(&st.challenge) == response)
                    .map(|(k, _)| k.clone())
                    .ok_or_else(|| ImError::AuthFailed("挑战响应不匹配".to_string()))?;

                let st = sessions.get_mut(&key).unwrap();
                // 校验节点是否已信任——未信任不颁发 token。
                let trusted = self
                    .nodes
                    .lock()
                    .unwrap()
                    .get(&st.node_id)
                    .map(|n| n.trusted)
                    .unwrap_or(false);
                if !trusted {
                    return Err(ImError::NodeNotTrusted(format!(
                        "节点 {} 未信任，拒绝 Federation",
                        st.node_id
                    )));
                }
                let token = Self::make_session_token(&st.node_id);
                st.session_token = Some(token.clone());
                let token_for_ready = token;
                drop(sessions);
                // 消费该会话状态。
                self.sessions.lock().unwrap().remove(&key);
                Ok(FederationHandshake::Ready {
                    session_token: token_for_ready,
                })
            }
            // —— 第 4 步：收到 Ready ——
            FederationHandshake::Ready { session_token } => {
                if session_token.is_empty() {
                    return Err(ImError::AuthFailed("空 session_token".to_string()));
                }
                Ok(FederationHandshake::Ready { session_token })
            }
        }
    }
}

// ============================================================
// 单元测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 工具：用两个 manager 走完整握手（client=发起方, server=接收方）。
    /// 返回最终的 Ready session_token。
    ///
    /// 关键点：client 与 server 用同一 PSK，签名才能匹配；server 必须信任 client 节点。
    async fn run_full_handshake(
        client: &LocalFederationManager,
        server: &LocalFederationManager,
    ) -> String {
        let server_endpoint = "10.0.0.2:8443";

        // 1. client 发起 Hello。
        let hello = client.handshake(server_endpoint).await.unwrap();
        // 2. server 处理 Hello → Welcome(+challenge)。同时把 client 注册为节点（未信任）。
        let welcome = server.handle_handshake(hello).await.unwrap();
        match &welcome {
            FederationHandshake::Welcome {
                accepted,
                challenge,
                ..
            } => {
                assert!(*accepted, "server 应接受 Hello");
                assert!(challenge.is_some(), "Welcome 须携带挑战");
            }
            _ => panic!("期望 Welcome，得到 {welcome:?}"),
        }

        // 3. server 信任 client 节点（用 client identity 的 node_id）。
        let client_node_id = client.identity().node_id.clone();
        server.trust_node(&client_node_id).await.unwrap();

        // 4. client 处理 Welcome → Auth（用 client PSK 签名；两端 PSK 一致）。
        let auth = client.handle_handshake(welcome).await.unwrap();
        let FederationHandshake::Auth { response } = &auth else {
            panic!("期望 Auth，得到 {auth:?}");
        };
        assert!(!response.is_empty());

        // 5. server 校验 Auth → Ready（因已信任 client 节点）。
        let ready = server.handle_handshake(auth).await.unwrap();
        let FederationHandshake::Ready { session_token } = &ready else {
            panic!("期望 Ready，得到 {ready:?}");
        };
        assert!(!session_token.is_empty());
        session_token.clone()
    }

    #[tokio::test]
    async fn handshake_full_flow_succeeds_when_trusted() {
        // 两端用同一 PSK（DEFAULT_PSK），签名方能匹配。
        let client = LocalFederationManager::with_defaults("client-1", "Client Node");
        let server = LocalFederationManager::with_defaults("server-1", "Server Node");

        let token = run_full_handshake(&client, &server).await;
        assert!(token.starts_with("sess:client-1"), "token={token}");
    }

    #[tokio::test]
    async fn handshake_rejected_when_node_not_trusted() {
        let server = LocalFederationManager::with_defaults("server-1", "Server Node");
        // 模拟远端 client 发来 Hello（不信任）。
        let hello = FederationHandshake::Hello {
            node_id: "peer-x".to_string(),
            display_name: "Untrusted Client".to_string(),
            capabilities: NodeCapabilities::im_only("0.0.0"),
        };
        let welcome = server.handle_handshake(hello).await.unwrap();
        let challenge = match welcome {
            FederationHandshake::Welcome { challenge, .. } => challenge.unwrap(),
            _ => panic!("期望 Welcome"),
        };

        // peer 用 server 的 PSK 签名（同实例，签名一致）。
        let response = server.sign_challenge(&challenge);
        let auth = FederationHandshake::Auth { response };

        // server 校验 Auth → 因未信任 peer，拒绝（NodeNotTrusted），不颁 token。
        let err = server.handle_handshake(auth).await.unwrap_err();
        assert!(
            matches!(err, ImError::NodeNotTrusted(_)),
            "期望 NodeNotTrusted，得到 {err:?}"
        );
    }

    #[tokio::test]
    async fn trust_node_marks_trusted_and_sets_pubkey() {
        let mgr = LocalFederationManager::with_defaults("n1", "Node 1");
        let node = mgr.add_node("192.168.1.10:9000").await.unwrap();
        assert!(!node.trusted);
        assert!(node.public_key.is_none());

        mgr.trust_node(&node.node_id).await.unwrap();
        let after = mgr
            .list_nodes()
            .await
            .into_iter()
            .find(|n| n.node_id == node.node_id)
            .unwrap();
        assert!(after.trusted);
        assert!(after.public_key.is_some());
    }

    #[tokio::test]
    async fn revoke_node_untrusts_and_clears_session() {
        let mgr = LocalFederationManager::with_defaults("n1", "Node 1");
        let node = mgr.add_node("192.168.1.11:9000").await.unwrap();
        mgr.trust_node(&node.node_id).await.unwrap();

        // 注入一个进行中会话状态（模拟握手未完成）。
        mgr.sessions.lock().unwrap().insert(
            "challenge:n1:v1".to_string(),
            HandshakeState {
                node_id: node.node_id.clone(),
                endpoint: node.endpoint.clone(),
                challenge: "challenge:n1:v1".to_string(),
                session_token: None,
            },
        );
        assert_eq!(mgr.sessions.lock().unwrap().len(), 1);

        mgr.revoke_node(&node.node_id).await.unwrap();
        let after = mgr
            .list_nodes()
            .await
            .into_iter()
            .find(|n| n.node_id == node.node_id)
            .unwrap();
        assert!(!after.trusted);
        assert_eq!(mgr.sessions.lock().unwrap().len(), 0, "revoke 应清理会话");
    }

    #[tokio::test]
    async fn add_node_rejects_duplicate_endpoint() {
        let mgr = LocalFederationManager::with_defaults("n1", "Node 1");
        mgr.add_node("10.1.2.3:8000").await.unwrap();
        let err = mgr.add_node("10.1.2.3:8000").await.unwrap_err();
        assert!(
            matches!(err, ImError::NodeAlreadyExists(_)),
            "期望 NodeAlreadyExists，得到 {err:?}"
        );
    }

    #[tokio::test]
    async fn add_node_rejects_invalid_endpoint() {
        let mgr = LocalFederationManager::with_defaults("n1", "Node 1");
        // 缺端口
        assert!(mgr.add_node("10.1.2.3").await.is_err());
        // 非数字端口
        assert!(mgr.add_node("10.1.2.3:http").await.is_err());
        // 空 host
        assert!(mgr.add_node(":8000").await.is_err());
    }

    #[tokio::test]
    async fn handle_welcome_rejected_propagates_error() {
        let mgr = LocalFederationManager::with_defaults("n1", "Node 1");
        let denied = FederationHandshake::Welcome {
            accepted: false,
            challenge: None,
            reason: Some("policy denied".to_string()),
        };
        let err = mgr.handle_handshake(denied).await.unwrap_err();
        assert!(
            matches!(err, ImError::HandshakeFailed(ref m) if m.contains("policy denied")),
            "期望 HandshakeFailed(policy denied)，得到 {err:?}"
        );
    }

    #[tokio::test]
    async fn auth_with_wrong_response_fails() {
        let mgr = LocalFederationManager::with_defaults("server", "Server");
        // 预置一个会话状态（模拟已收到 Hello）。
        mgr.sessions.lock().unwrap().insert(
            "challenge:peer:v1".to_string(),
            HandshakeState {
                node_id: "peer".to_string(),
                endpoint: String::new(),
                challenge: "challenge:peer:v1".to_string(),
                session_token: None,
            },
        );
        // 错误的响应。
        let err = mgr
            .handle_handshake(FederationHandshake::Auth {
                response: "deadbeef".to_string(),
            })
            .await
            .unwrap_err();
        assert!(
            matches!(err, ImError::AuthFailed(_)),
            "期望 AuthFailed，得到 {err:?}"
        );
    }

    #[tokio::test]
    async fn discover_nodes_returns_registered() {
        let mgr = LocalFederationManager::with_defaults("n1", "Node 1");
        assert!(mgr.discover_nodes().await.is_empty());
        mgr.add_node("10.0.0.5:7000").await.unwrap();
        mgr.add_node("10.0.0.6:7000").await.unwrap();
        assert_eq!(mgr.discover_nodes().await.len(), 2);
    }

    #[test]
    fn node_capabilities_im_only_builder() {
        let caps = NodeCapabilities::im_only("9.9.9");
        assert!(caps.im);
        assert!(!caps.storage);
        assert!(!caps.compute);
        assert_eq!(caps.version, "9.9.9");
    }
}
