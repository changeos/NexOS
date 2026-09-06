//! Mock 实现（feature gate `mock`）——供下游 agent（provision/client/guest/update）测试用。
//!
//! 约定（_conventions.md §5）：
//! - 实现完整 trait（不 panic 的默认返回）
//! - 提供构造器设置预期返回值（builder 风格）
//! - 纯内存、确定性
//!
//! 3 个 Mock：[`MockDiscovery`] / [`MockPeerAuthenticator`] / [`MockFederationPolicy`]。
//! 行为与 [`crate::impls`] 的默认实现保持一致（内存后端 + 可注入预期），并叠加
//! "可控失败注入"能力，便于下游测试异常路径。

#![cfg(feature = "mock")]

use std::collections::HashMap;
use std::sync::Mutex;

use os_core::{DateTime, NodeId, Utc};

use crate::auth::{PairingToken, PeerAuthenticator, PeerSession, PeerSessionId};
use crate::beacon;
use crate::discovery::{Discovery, PeerCallback, PeerNode};
use crate::federation::{
    FederationAction, FederationChoice, FederationPolicy, HaEligibility, HaRequirement,
};
use crate::DiscoverError;

// ----------------------------------------------------------------------------
// MockDiscovery
// ----------------------------------------------------------------------------

/// Mock `Discovery`——内存后端，可注入预期 peer 列表与可控 beacon 校验。
///
/// 默认行为：`discover_peers` 返回注入的 peer 列表（经 beacon 结构校验过滤）；
/// `start_advertising` / `stop_advertising` 仅记录状态。
pub struct MockDiscovery {
    self_info: Mutex<Option<PeerNode>>,
    peers: Mutex<Vec<PeerNode>>,
    callback: Mutex<Option<Box<dyn PeerCallback>>>,
    /// 是否在 discover_peers 时丢弃无签名 peer（默认 true，对齐防伪红线）
    require_valid_beacon: Mutex<bool>,
}

impl Default for MockDiscovery {
    fn default() -> Self {
        Self {
            self_info: Mutex::new(None),
            peers: Mutex::new(Vec::new()),
            callback: Mutex::new(None),
            require_valid_beacon: Mutex::new(true),
        }
    }
}

impl MockDiscovery {
    /// 创建空 mock。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注入预期 peer 列表（覆盖）。
    pub fn with_peers(self, peers: Vec<PeerNode>) -> Self {
        *self.peers.lock().expect("lock poisoned") = peers;
        self
    }

    /// 追加单个 peer。
    pub fn add_peer(&self, peer: PeerNode) {
        self.peers.lock().expect("lock poisoned").push(peer);
    }

    /// 控制 beacon 校验开关（默认 true）。设 false 可测试"接受无签名 peer"路径。
    pub fn set_require_valid_beacon(&self, require: bool) {
        *self.require_valid_beacon.lock().expect("lock poisoned") = require;
    }

    /// 当前是否在广播。
    pub fn is_advertising(&self) -> bool {
        self.self_info.lock().expect("lock poisoned").is_some()
    }
}

impl Discovery for MockDiscovery {
    async fn start_advertising(&self, self_info: PeerNode) -> Result<(), DiscoverError> {
        *self.self_info.lock().expect("lock poisoned") = Some(self_info);
        Ok(())
    }

    async fn stop_advertising(&self) -> Result<(), DiscoverError> {
        *self.self_info.lock().expect("lock poisoned") = None;
        Ok(())
    }

    async fn discover_peers(&self, _timeout_ms: u32) -> Result<Vec<PeerNode>, DiscoverError> {
        let require = *self.require_valid_beacon.lock().expect("lock poisoned");
        let now = Utc::now();
        let peers = self.peers.lock().expect("lock poisoned");
        let mut out = Vec::with_capacity(peers.len());
        for peer in peers.iter() {
            if require {
                // 复用 beacon 校验（与 MdnsDiscovery 一致；mock 无公钥，传 None 走结构校验回退）
                let payload = beacon::BeaconPayload {
                    node_id: peer.node_id.clone(),
                    endpoints: peer.endpoints.clone(),
                    valid_until: now + chrono::Duration::seconds(60),
                    nonce: 0,
                };
                if !matches!(
                    beacon::verify_beacon_signature(peer, &payload, now, None),
                    beacon::BeaconVerifyOutcome::Ok
                ) {
                    continue; // 防伪红线：丢弃
                }
            }
            out.push(peer.clone());
        }
        Ok(out)
    }

    async fn on_peer_discovered(&self, callback: Box<dyn PeerCallback>) {
        *self.callback.lock().expect("lock poisoned") = Some(callback);
    }
}

// ----------------------------------------------------------------------------
// MockPeerAuthenticator
// ----------------------------------------------------------------------------

/// Mock `PeerAuthenticator`——内存维护受信 peer 与会话，可注入可控失败。
///
/// 默认行为：`pair` 用 token 建立会话（记录指纹 = token 的稳定哈希投影）；
/// `list_trusted_peers` 返回已配对节点；`unpair` 移除会话与受信关系。
/// 失败注入：`set_pairing_fails(true)` 让 `pair` 返回 `PairingFailed`。
pub struct MockPeerAuthenticator {
    sessions: Mutex<HashMap<PeerSessionId, PeerSession>>,
    /// node_id → 是否受信（已配对）
    trusted: Mutex<Vec<NodeId>>,
    pairing_fails: Mutex<bool>,
}

impl Default for MockPeerAuthenticator {
    fn default() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            trusted: Mutex::new(Vec::new()),
            pairing_fails: Mutex::new(false),
        }
    }
}

impl MockPeerAuthenticator {
    /// 创建空 mock。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注入已受信节点列表（list_trusted_peers 返回）。
    pub fn with_trusted(self, peers: Vec<NodeId>) -> Self {
        *self.trusted.lock().expect("lock poisoned") = peers;
        self
    }

    /// 控制 `pair` 是否失败（测试 PairingFailed 路径）。
    pub fn set_pairing_fails(&self, fails: bool) {
        *self.pairing_fails.lock().expect("lock poisoned") = fails;
    }

    /// 当前活跃会话数（测试用）。
    pub fn session_count(&self) -> usize {
        self.sessions.lock().expect("lock poisoned").len()
    }

    /// 由 token 字符串派生一个稳定的"证书指纹"（非密码学用途，仅 mock 一致性）。
    fn fingerprint(token: &str) -> String {
        // 简单投影：对字节求和 → hex（确定性，便于断言）
        let sum: u64 = token.bytes().map(|b| b as u64).sum();
        format!("mock-fp-{sum:016x}")
    }
}

impl PeerAuthenticator for MockPeerAuthenticator {
    async fn pair(
        &self,
        peer_endpoint: &str,
        token: &PairingToken,
    ) -> Result<PeerSession, DiscoverError> {
        if *self.pairing_fails.lock().expect("lock poisoned") {
            return Err(DiscoverError::PairingFailed(
                "mock: 注入的配对失败".to_string(),
            ));
        }
        // token 过期检查
        if token.expires_at < Utc::now() {
            return Err(DiscoverError::PairingFailed(
                "mock: 配对凭证已过期".to_string(),
            ));
        }
        let fingerprint = Self::fingerprint(&token.token);
        // 用 peer_endpoint 作为 node_id 占位（mock 不解析真实 endpoint）
        let peer = NodeId::new(peer_endpoint);
        let session = PeerSession::new(
            PeerSessionId::new(format!("sess-{}", peer)),
            peer.clone(),
            fingerprint,
        );
        self.sessions
            .lock()
            .expect("lock poisoned")
            .insert(session.id.clone(), session.clone());
        self.trusted.lock().expect("lock poisoned").push(peer);
        Ok(session)
    }

    async fn unpair(&self, session: &PeerSessionId) -> Result<(), DiscoverError> {
        let removed = self.sessions.lock().expect("lock poisoned").remove(session);
        if removed.is_none() {
            return Err(DiscoverError::PairingFailed(format!(
                "mock: 会话不存在 {session}"
            )));
        }
        Ok(())
    }

    async fn list_trusted_peers(&self) -> Vec<NodeId> {
        self.trusted.lock().expect("lock poisoned").clone()
    }
}

// ----------------------------------------------------------------------------
// MockFederationPolicy
// ----------------------------------------------------------------------------

/// Mock `FederationPolicy`——可注入资格结果与决策动作。
///
/// 默认行为：与 `DefaultFederationPolicy` 一致（真实硬指标判定 + 决策矩阵）。
/// 注入：`set_eligibility(...)` 让 `check_eligibility` 返回固定结果；
/// `set_action(...)` 让 `decide` 返回固定动作。
pub struct MockFederationPolicy {
    eligibility_override: Mutex<Option<HaEligibility>>,
    action_override: Mutex<Option<FederationAction>>,
}

impl Default for MockFederationPolicy {
    fn default() -> Self {
        Self {
            eligibility_override: Mutex::new(None),
            action_override: Mutex::new(None),
        }
    }
}

impl MockFederationPolicy {
    /// 创建 mock（默认行为同 DefaultFederationPolicy）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注入固定资格结果（None 恢复默认真实判定）。
    pub fn set_eligibility(&self, elig: Option<HaEligibility>) {
        *self.eligibility_override.lock().expect("lock poisoned") = elig;
    }

    /// 注入固定决策动作（None 恢复默认真实决策）。
    pub fn set_action(&self, action: Option<FederationAction>) {
        *self.action_override.lock().expect("lock poisoned") = action;
    }
}

impl FederationPolicy for MockFederationPolicy {
    async fn check_eligibility(
        &self,
        peers: &[PeerNode],
        requirements: &HaRequirement,
    ) -> Result<HaEligibility, DiscoverError> {
        if let Some(e) = self
            .eligibility_override
            .lock()
            .expect("lock poisoned")
            .clone()
        {
            return Ok(e);
        }
        // 默认：委托真实算法（与 DefaultFederationPolicy 一致）
        let quals: Vec<_> = peers
            .iter()
            .map(|p| crate::capabilities::qualify_peer(p, requirements))
            .collect();
        let mut reasons =
            crate::capabilities::aggregate_qualifications(&quals, requirements.min_nodes);
        let eligible = reasons.is_empty();
        if eligible {
            reasons.clear();
        }
        Ok(HaEligibility::new(eligible, reasons))
    }

    async fn decide(
        &self,
        eligibility: &HaEligibility,
        user_choice: FederationChoice,
    ) -> Result<FederationAction, DiscoverError> {
        if let Some(a) = self.action_override.lock().expect("lock poisoned").clone() {
            return Ok(a);
        }
        Ok(crate::federation_sm::decide_action(
            eligibility.eligible,
            user_choice,
            None,
        ))
    }
}

// 给 mock 用的 now 占位（避免 unused import 警告时仍保留语义）
#[allow(dead_code)]
fn _now() -> DateTime {
    Utc::now()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::PairingScope;
    use crate::discovery::NodeCapabilities;

    fn signed_peer(id: &str) -> PeerNode {
        let node_id = NodeId::new(id);
        PeerNode {
            beacon_signature: Some(beacon::pseudo_signature(&node_id)),
            node_id,
            endpoints: vec!["10.0.0.1:8443".into()],
            version: "1.5.0".into(),
            arch: "x86_64".into(),
            capabilities: NodeCapabilities::full(),
        }
    }

    fn valid_token(scope: PairingScope) -> PairingToken {
        PairingToken {
            token: "tok-123".into(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
            issued_by: NodeId::new("leader"),
            scope,
        }
    }

    fn expired_token() -> PairingToken {
        PairingToken {
            token: "tok-exp".into(),
            expires_at: Utc::now() - chrono::Duration::hours(1),
            issued_by: NodeId::new("leader"),
            scope: PairingScope::JoinCluster,
        }
    }

    fn req() -> HaRequirement {
        HaRequirement {
            min_nodes: 1,
            min_bandwidth_gbps: 10.0,
            require_zfs: true,
            require_kvm: true,
            version_compat: vec![">=1.0.0,<2.0.0".into()],
        }
    }

    #[tokio::test]
    async fn mock_discovery_filters_invalid_beacon() {
        let mut p = signed_peer("n1");
        p.beacon_signature = None;
        let d = MockDiscovery::new().with_peers(vec![p]);
        let found = d.discover_peers(10).await.unwrap();
        assert!(found.is_empty());
    }

    #[tokio::test]
    async fn mock_discovery_can_disable_beacon_check() {
        let mut p = signed_peer("n1");
        p.beacon_signature = None;
        let d = MockDiscovery::new().with_peers(vec![p]);
        d.set_require_valid_beacon(false);
        let found = d.discover_peers(10).await.unwrap();
        assert_eq!(found.len(), 1);
    }

    #[tokio::test]
    async fn mock_discovery_advertising_toggle() {
        let d = MockDiscovery::new();
        assert!(!d.is_advertising());
        d.start_advertising(signed_peer("self")).await.unwrap();
        assert!(d.is_advertising());
        d.stop_advertising().await.unwrap();
        assert!(!d.is_advertising());
    }

    #[tokio::test]
    async fn mock_auth_pair_success() {
        let auth = MockPeerAuthenticator::new();
        let sess = auth
            .pair("peer-1:8443", &valid_token(PairingScope::JoinCluster))
            .await
            .unwrap();
        assert_eq!(sess.peer, NodeId::new("peer-1:8443"));
        assert_eq!(auth.session_count(), 1);
        let trusted = auth.list_trusted_peers().await;
        assert_eq!(trusted.len(), 1);
    }

    #[tokio::test]
    async fn mock_auth_pair_expired_token_fails() {
        let auth = MockPeerAuthenticator::new();
        let err = auth
            .pair("peer-1:8443", &expired_token())
            .await
            .unwrap_err();
        assert!(matches!(err, DiscoverError::PairingFailed(_)));
    }

    #[tokio::test]
    async fn mock_auth_pair_injected_failure() {
        let auth = MockPeerAuthenticator::new();
        auth.set_pairing_fails(true);
        let err = auth
            .pair("peer-1:8443", &valid_token(PairingScope::PeerSync))
            .await
            .unwrap_err();
        assert!(matches!(err, DiscoverError::PairingFailed(_)));
    }

    #[tokio::test]
    async fn mock_auth_unpair_removes_session() {
        let auth = MockPeerAuthenticator::new();
        let sess = auth
            .pair("peer-1:8443", &valid_token(PairingScope::ClientAccess))
            .await
            .unwrap();
        auth.unpair(&sess.id).await.unwrap();
        assert_eq!(auth.session_count(), 0);
        // 重复 unpair 报错
        assert!(auth.unpair(&sess.id).await.is_err());
    }

    #[tokio::test]
    async fn mock_federation_default_matches_real() {
        let policy = MockFederationPolicy::new();
        let elig = policy
            .check_eligibility(&[signed_peer("n1")], &req())
            .await
            .unwrap();
        assert!(elig.eligible);
        let action = policy.decide(&elig, FederationChoice::Auto).await.unwrap();
        assert!(matches!(action, FederationAction::JoinHaCluster { .. }));
    }

    #[tokio::test]
    async fn mock_federation_override_eligibility() {
        let policy = MockFederationPolicy::new();
        policy.set_eligibility(Some(HaEligibility::new(
            false,
            vec!["mock: 注入不达标".into()],
        )));
        let elig = policy
            .check_eligibility(&[signed_peer("n1")], &req())
            .await
            .unwrap();
        assert!(!elig.eligible);
    }

    #[tokio::test]
    async fn mock_federation_override_action() {
        let policy = MockFederationPolicy::new();
        policy.set_action(Some(FederationAction::StayStandalone));
        let elig = HaEligibility::new(true, vec![]);
        let action = policy.decide(&elig, FederationChoice::Auto).await.unwrap();
        assert!(matches!(action, FederationAction::StayStandalone));
    }
}
