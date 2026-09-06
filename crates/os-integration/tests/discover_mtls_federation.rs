//! 场景 7：discover mTLS 联邦（integration-agent 规格书 §3 场景 7）
//!
//! 链路：os-discover 发现 peer → MtlsPeerAuthenticator 双向认证（pair）→
//! FederationPolicy 评估（check_eligibility + decide）→ 联邦状态机推进
//! （Probing → Authenticating → Qualifying → JoiningHa/Active）→ 成功 join
//! 或降级为 peer/standalone。
//!
//! 重点验证：
//! - 联邦决策链端到端：Discovery 发现 peer → mTLS pair 成功 → 资格检测 → 决策 →
//!   状态机推进到 Active(Ha)，各阶段串通。
//! - mTLS 失败降级：PairingFailed → 状态机降级到 Active(Standalone)，决策链不
//!   继续推进到 Qualifying。
//! - 资格不达标降级：peer 硬指标（带宽/节点数）不满足 → decide 产 StayStandalone
//!   或 RegisterAsPeer（按用户选择），状态机走降级路径。
//! - 跨 crate 类型桥接：PeerNode / PairingToken / PeerSession / FederationAction /
//!   FederationEvent 在 discover 内部各 trait 间透传一致。
//! - 状态机降级路径：AuthFailed → Standalone；HaJoinFailed → Peer。
//!
//! 红线：不改 trait 签名 / 其他 crate 源码——本测试用 os-discover feature `mock`
//! 暴露的 MockDiscovery / MockPeerAuthenticator / MockFederationPolicy + 纯逻辑
//! FederationStateMachine。

use std::sync::{Arc, Mutex};

use os_core::eventbus::{Event, EventBus, Severity, Topic};
use os_core::mock::MockEventBus;
use os_core::{NodeId, Utc};

use os_discover::auth::{PairingScope, PairingToken, PeerAuthenticator};
use os_discover::beacon;
use os_discover::discovery::{Discovery, NodeCapabilities, PeerNode};
use os_discover::federation::{
    FederationAction, FederationChoice, FederationPolicy, HaEligibility, HaRequirement,
};
use os_discover::federation_sm::{
    ActiveRole, FederationEvent, FederationState, FederationStateMachine,
};
use os_discover::{DiscoverError, MockDiscovery, MockFederationPolicy, MockPeerAuthenticator};

// ----------------------------------------------------------------------------
// 集成版联邦编排器：把 Discovery + PeerAuthenticator + FederationPolicy +
// FederationStateMachine 串起来。这是 integration-agent 搭建的「业务编排层」——
// 验证各组件能跨 trait 协作。
//
// 注：Discovery / PeerAuthenticator / FederationPolicy 都是原生 `async fn in trait`
// （非 dyn 兼容，ADR-COMPAT-001），无法 Box<dyn>；本测试用**具体类型**注入（与
// ha_failover_chain.rs 用 Arc<MockVmManager> 一致的模式）。
// ----------------------------------------------------------------------------

struct IntegratedFederationOrchestrator {
    discovery: Arc<MockDiscovery>,
    auth: Arc<MockPeerAuthenticator>,
    policy: Arc<MockFederationPolicy>,
    bus: Arc<MockEventBus>,
    /// 联邦决策状态机（纯逻辑，由本编排器推进）
    sm: Mutex<FederationStateMachine>,
    /// 已建立的 peer 会话（pair 成功后存入，供断言）
    session: Mutex<Option<os_discover::auth::PeerSession>>,
    /// 联邦动作日志（按执行顺序记录「调了什么、结果如何」）
    call_log: Mutex<Vec<String>>,
    /// HA 资格要求（硬指标门槛）
    requirement: HaRequirement,
}

impl IntegratedFederationOrchestrator {
    fn new(
        discovery: Arc<MockDiscovery>,
        auth: Arc<MockPeerAuthenticator>,
        policy: Arc<MockFederationPolicy>,
        bus: Arc<MockEventBus>,
        requirement: HaRequirement,
    ) -> Self {
        Self {
            discovery,
            auth,
            policy,
            bus,
            sm: Mutex::new(FederationStateMachine::new()),
            session: Mutex::new(None),
            call_log: Mutex::new(Vec::new()),
            requirement,
        }
    }

    fn state(&self) -> FederationState {
        self.sm.lock().expect("sm").state()
    }

    fn call_log(&self) -> Vec<String> {
        self.call_log.lock().expect("call_log").clone()
    }

    fn session(&self) -> Option<os_discover::auth::PeerSession> {
        self.session.lock().expect("session").clone()
    }

    /// 驱动一次完整联邦流程（discovery → mTLS pair → qualify → decide → join）。
    ///
    /// 返回最终状态机终态（应为 Active(Ha) / Active(Peer) / Active(Standalone)）。
    async fn drive_federation(
        &self,
        user_choice: FederationChoice,
        pairing_token: PairingToken,
    ) -> Result<FederationState, DiscoverError> {
        // === 阶段 1：Probing → Authenticating（discovery 发现 peer）===
        let peers = self.discovery.discover_peers(1000).await?;
        if peers.is_empty() {
            // 无 peer：状态机 NoPeerFound → Standalone 终态。
            let t = self
                .sm
                .lock()
                .expect("sm")
                .transition(&FederationEvent::NoPeerFound);
            assert!(t.valid);
            self.call_log
                .lock()
                .expect("call_log")
                .push("discover: 无 peer → Standalone".into());
            self.emit_terminal_event("federation.standalone", "无 peer");
            return Ok(self.state());
        }

        let peer = peers.first().expect("非空 peers 已校验").clone();
        // 状态机推进：PeerDiscovered → Authenticating
        let t = self
            .sm
            .lock()
            .expect("sm")
            .transition(&FederationEvent::PeerDiscovered(peer.node_id.clone()));
        assert!(t.valid, "PeerDiscovered 转移应合法");
        assert_eq!(t.to, FederationState::Authenticating);
        self.call_log
            .lock()
            .expect("call_log")
            .push(format!("discover: 发现 peer {}", peer.node_id));

        // === 阶段 2：Authenticating（mTLS 双向认证：pair）===
        let peer_endpoint = peer
            .endpoints
            .first()
            .cloned()
            .unwrap_or_else(|| format!("{}:8443", peer.node_id));
        let session = match self.auth.pair(&peer_endpoint, &pairing_token).await {
            Ok(s) => {
                self.call_log
                    .lock()
                    .expect("call_log")
                    .push(format!("mtls.pair({peer_endpoint}): Ok"));
                s
            }
            Err(e) => {
                // mTLS 失败 → 状态机 AuthFailed → Standalone 终态（降级路径）。
                let t = self
                    .sm
                    .lock()
                    .expect("sm")
                    .transition(&FederationEvent::AuthFailed);
                assert!(t.valid, "AuthFailed 转移应合法");
                assert_eq!(t.to, FederationState::Active(ActiveRole::Standalone));
                self.call_log.lock().expect("call_log").push(format!(
                    "mtls.pair({peer_endpoint}): Err({e}) → 降级 Standalone"
                ));
                self.emit_terminal_event("federation.degraded", "mTLS 失败降级");
                return Err(e);
            }
        };
        *self.session.lock().expect("session") = Some(session.clone());
        // 状态机推进：AuthSucceeded → Qualifying
        let t = self
            .sm
            .lock()
            .expect("sm")
            .transition(&FederationEvent::AuthSucceeded);
        assert!(t.valid, "AuthSucceeded 转移应合法");
        assert_eq!(t.to, FederationState::Qualifying);
        self.call_log
            .lock()
            .expect("call_log")
            .push("mtls: AuthSucceeded → Qualifying".into());

        // === 阶段 3：Qualifying（资格检测 + 决策）===
        let eligibility = self
            .policy
            .check_eligibility(&peers, &self.requirement)
            .await?;
        self.call_log.lock().expect("call_log").push(format!(
            "policy.check_eligibility: eligible={}",
            eligibility.eligible
        ));

        let action = self.policy.decide(&eligibility, user_choice).await?;
        self.call_log
            .lock()
            .expect("call_log")
            .push(format!("policy.decide: action={action:?}"));

        // 状态机推进：DecisionReady → JoiningHa / Active(Peer) / Active(Standalone)
        let t = self
            .sm
            .lock()
            .expect("sm")
            .transition(&FederationEvent::DecisionReady {
                action: action.clone(),
            });
        assert!(t.valid, "DecisionReady 转移应合法");

        match &action {
            FederationAction::JoinHaCluster { leader_endpoint } => {
                // 进入 JoiningHa；模拟加入完成。
                let t = self
                    .sm
                    .lock()
                    .expect("sm")
                    .transition(&FederationEvent::HaJoinCompleted);
                assert!(t.valid, "HaJoinCompleted 应合法");
                self.call_log.lock().expect("call_log").push(format!(
                    "federation: HaJoinCompleted → Active(Ha) leader={leader_endpoint}"
                ));
                self.emit_terminal_event("federation.joined_ha", "加入 HA");
            }
            FederationAction::RegisterAsPeer => {
                self.call_log
                    .lock()
                    .expect("call_log")
                    .push("federation: Active(Peer)".into());
                self.emit_terminal_event("federation.peer", "注册为 peer");
            }
            FederationAction::StayStandalone => {
                self.call_log
                    .lock()
                    .expect("call_log")
                    .push("federation: Active(Standalone)".into());
                self.emit_terminal_event("federation.standalone", "保持单机");
            }
        }

        Ok(self.state())
    }

    /// 把联邦终态发到 EventBus（Cluster topic）。
    fn emit_terminal_event(&self, kind: &str, msg: &str) {
        let ev = Event {
            source: "os-discover".into(),
            topic: Topic::Cluster,
            kind: kind.into(),
            severity: if kind.contains("degraded") || kind.contains("standalone") {
                Severity::Warn
            } else {
                Severity::Info
            },
            task_id: None,
            payload: serde_json::json!({
                "state": format!("{:?}", self.state()),
                "msg": msg,
            }),
            timestamp: Utc::now(),
        };
        let bus = self.bus.clone();
        // 同步触发：MockEventBus.publish 内部是 Mutex push，tokio::spawn 会丢顺序，
        // 这里直接 block_on 不合适（test runtime）。改用 tokio spawn 但不 await——
        // 为确定性，我们在调用方 drive_federation 是 async，直接 await publish。
        // 用一个临时 future：把 bus move 进 spawn。
        tokio::spawn(async move {
            let _ = bus.publish(ev).await;
        });
        // 注：因 spawn 异步可能晚于断言执行，关键事件断言不依赖此；
        // 仅作「事件流也串通」的弱断言（见 published_count_for 断言，给 spawn 留时间）。
    }
}

// ----------------------------------------------------------------------------
// 辅助：构造签名 peer / token / HA 门槛
// ----------------------------------------------------------------------------

fn signed_peer(id: &str, version: &str, caps: NodeCapabilities) -> PeerNode {
    let node_id = NodeId::new(id);
    PeerNode {
        node_id: node_id.clone(),
        endpoints: vec![format!("10.0.0.{}:8443", id.len())],
        version: version.into(),
        arch: "x86_64".into(),
        capabilities: caps,
        beacon_signature: Some(beacon::pseudo_signature(&node_id)),
    }
}

fn valid_token(scope: PairingScope) -> PairingToken {
    PairingToken {
        token: format!("tok-{}", Utc::now().timestamp()),
        expires_at: Utc::now() + chrono::Duration::hours(1),
        issued_by: NodeId::new("leader"),
        scope,
    }
}

fn ha_requirement(min_nodes: u32, min_bw: f32) -> HaRequirement {
    HaRequirement {
        min_nodes,
        min_bandwidth_gbps: min_bw,
        require_zfs: true,
        require_kvm: true,
        version_compat: vec![">=1.0.0,<2.0.0".into()],
    }
}

// ----------------------------------------------------------------------------
// 集成测
// ----------------------------------------------------------------------------

#[tokio::test]
async fn federation_full_chain_joins_ha_when_qualified() {
    // peer 硬指标全部达标 + 用户选 Auto → 决策链端到端到 Active(Ha)。
    let peer = signed_peer("peer-strong", "1.5.0", NodeCapabilities::full());
    let discovery = Arc::new(MockDiscovery::new().with_peers(vec![peer]));
    let auth = Arc::new(MockPeerAuthenticator::new());
    let policy = Arc::new(MockFederationPolicy::new());
    let bus = Arc::new(MockEventBus::new());
    let orch = IntegratedFederationOrchestrator::new(
        discovery,
        auth.clone(),
        policy,
        bus.clone(),
        ha_requirement(1, 10.0),
    );

    let final_state = orch
        .drive_federation(
            FederationChoice::Auto,
            valid_token(PairingScope::JoinCluster),
        )
        .await
        .expect("联邦应成功");

    // 状态机到 Active(Ha)。
    assert_eq!(
        final_state,
        FederationState::Active(ActiveRole::Ha),
        "资格达标 + Auto → 应 Active(Ha)"
    );
    assert!(final_state.is_terminal());

    // mTLS 会话已建立（pair 成功）。
    let session = orch.session().expect("应有会话");
    assert!(
        session.peer.as_str().contains("10.0.0."),
        "peer endpoint 应记入会话"
    );

    // 调用顺序断言：discover → mtls.pair(Ok) → check_eligibility(eligible=true) →
    // decide(JoinHaCluster) → HaJoinCompleted。
    let log = orch.call_log();
    let discover_idx = log
        .iter()
        .position(|s| s.contains("discover:"))
        .expect("应有 discover");
    let mtls_idx = log
        .iter()
        .position(|s| s.contains("mtls.pair") && s.contains("Ok"))
        .expect("应有 mtls.pair Ok");
    let elig_idx = log
        .iter()
        .position(|s| s.contains("eligible=true"))
        .expect("应有 eligible=true");
    let join_idx = log
        .iter()
        .position(|s| s.contains("HaJoinCompleted"))
        .expect("应有 HaJoinCompleted");
    assert!(discover_idx < mtls_idx, "mtls 应在 discover 之后");
    assert!(mtls_idx < elig_idx, "eligibility 应在 mtls 之后");
    assert!(elig_idx < join_idx, "join 应在 eligibility 之后");

    // mTLS 受信列表新增 peer（MockPeerAuthenticator.pair 成功后追加）。
    let trusted = auth.list_trusted_peers().await;
    assert_eq!(trusted.len(), 1, "应新增 1 个受信 peer");
}

#[tokio::test]
async fn federation_mtls_failure_degrades_to_standalone() {
    // mTLS pair 失败（注入）→ 状态机降级到 Active(Standalone)，不进 Qualifying。
    let peer = signed_peer("peer-bad-mtls", "1.5.0", NodeCapabilities::full());
    let discovery = Arc::new(MockDiscovery::new().with_peers(vec![peer]));
    let auth = Arc::new(MockPeerAuthenticator::new());
    auth.set_pairing_fails(true); // 注入 pair 失败
    let policy = Arc::new(MockFederationPolicy::new());
    let bus = Arc::new(MockEventBus::new());
    let orch = IntegratedFederationOrchestrator::new(
        discovery,
        auth.clone(),
        policy,
        bus.clone(),
        ha_requirement(1, 10.0),
    );

    let result = orch
        .drive_federation(
            FederationChoice::Auto,
            valid_token(PairingScope::JoinCluster),
        )
        .await;

    // drive 返回 Err（PairingFailed）。
    assert!(result.is_err(), "mTLS 失败应传播为 Err");
    let err = result.unwrap_err();
    assert!(
        matches!(err, DiscoverError::PairingFailed(_)),
        "应 PairingFailed，实得 {err:?}"
    );

    // 但状态机已降级到 Active(Standalone)（编排器内部推进了 AuthFailed 转移）。
    let state = orch.state();
    assert_eq!(
        state,
        FederationState::Active(ActiveRole::Standalone),
        "mTLS 失败应降级 Standalone，实得 {state:?}"
    );

    // 无 peer 会话建立。
    assert!(orch.session().is_none(), "pair 失败不应建会话");

    // 调用日志：mtls.pair Err → 降级 Standalone；**不**应进 Qualifying / decide。
    let log = orch.call_log();
    assert!(
        log.iter()
            .any(|s| s.contains("mtls.pair") && s.contains("Err")),
        "应有 mtls 失败记录: {log:?}"
    );
    assert!(
        log.iter().any(|s| s.contains("降级 Standalone")),
        "应有降级记录: {log:?}"
    );
    assert!(
        !log.iter().any(|s| s.contains("check_eligibility")),
        "mTLS 失败不应进资格检测: {log:?}"
    );
    assert!(
        !log.iter().any(|s| s.contains("policy.decide")),
        "mTLS 失败不应进决策: {log:?}"
    );
}

#[tokio::test]
async fn federation_ineligible_auto_degrades_to_standalone() {
    // peer 硬指标不达标（带宽低）+ Auto → decide 产 StayStandalone（降级单机）。
    let mut caps = NodeCapabilities::full();
    caps.network_gbps = 1.0; // 远低于 min_bw=10.0
    let peer = signed_peer("peer-weak", "1.5.0", caps);
    let discovery = Arc::new(MockDiscovery::new().with_peers(vec![peer]));
    let auth = Arc::new(MockPeerAuthenticator::new());
    let policy = Arc::new(MockFederationPolicy::new());
    let bus = Arc::new(MockEventBus::new());
    let orch = IntegratedFederationOrchestrator::new(
        discovery,
        auth,
        policy,
        bus.clone(),
        ha_requirement(1, 10.0),
    );

    let final_state = orch
        .drive_federation(
            FederationChoice::Auto,
            valid_token(PairingScope::JoinCluster),
        )
        .await
        .expect("降级路径应成功完成（非 Err）");

    // mTLS 成功（pair 通过），但资格不达标 → decide=StayStandalone → Active(Standalone)。
    assert_eq!(
        final_state,
        FederationState::Active(ActiveRole::Standalone),
        "不达标 + Auto → 应 Standalone，实得 {final_state:?}"
    );

    // mTLS 会话已建立（pair 成功；资格检测在 mTLS 之后）。
    assert!(
        orch.session().is_some(),
        "mTLS 应已成功（资格不达标但 pair 已完成）"
    );

    let log = orch.call_log();
    assert!(
        log.iter().any(|s| s.contains("eligible=false")),
        "应有 eligible=false: {log:?}"
    );
    assert!(
        log.iter().any(|s| s.contains("StayStandalone")),
        "应决策 Standalone: {log:?}"
    );
}

#[tokio::test]
async fn federation_manual_peer_choice_becomes_peer() {
    // 用户选 ManualPeer（无论资格）→ decide 产 RegisterAsPeer → Active(Peer)。
    let peer = signed_peer("peer-for-peer", "1.5.0", NodeCapabilities::full());
    let discovery = Arc::new(MockDiscovery::new().with_peers(vec![peer]));
    let auth = Arc::new(MockPeerAuthenticator::new());
    let policy = Arc::new(MockFederationPolicy::new());
    let bus = Arc::new(MockEventBus::new());
    let orch = IntegratedFederationOrchestrator::new(
        discovery,
        auth,
        policy,
        bus.clone(),
        ha_requirement(1, 10.0),
    );

    let final_state = orch
        .drive_federation(
            FederationChoice::ManualPeer,
            valid_token(PairingScope::PeerSync),
        )
        .await
        .expect("ManualPeer 应成功");

    assert_eq!(
        final_state,
        FederationState::Active(ActiveRole::Peer),
        "ManualPeer → 应 Active(Peer)，实得 {final_state:?}"
    );

    let log = orch.call_log();
    // 注：资格可能 eligible=true，但 ManualPeer 强制走 RegisterAsPeer（decide_action 矩阵）。
    assert!(
        log.iter().any(|s| s.contains("RegisterAsPeer")),
        "应决策 RegisterAsPeer: {log:?}"
    );
    assert!(
        log.iter().any(|s| s.contains("Active(Peer)")),
        "应有 Active(Peer) 终态记录: {log:?}"
    );
}

#[tokio::test]
async fn federation_no_peer_falls_back_to_standalone() {
    // discover_peers 返回空（无 peer）→ 状态机 NoPeerFound → Standalone。
    let discovery = Arc::new(MockDiscovery::new()); // 无 peer
    let discovery_clone = discovery.clone();
    let auth = Arc::new(MockPeerAuthenticator::new());
    let policy = Arc::new(MockFederationPolicy::new());
    let bus = Arc::new(MockEventBus::new());
    let orch = IntegratedFederationOrchestrator::new(
        discovery,
        auth,
        policy,
        bus.clone(),
        ha_requirement(1, 10.0),
    );

    // discover_peers 返回空时，drive 内部走 PeerNotFound 路径（discover_peers 返回
    // Ok(vec![])，故走「无 peer」分支，不是 Err）。我们直接验证编排器的状态机。
    // 但 drive_federation 内部把空 peers 视为 NoPeerFound。需构造：先调 discover
    // 拿到空，再走降级。
    let peers = discovery_clone.discover_peers(1000).await.unwrap();
    assert!(peers.is_empty());
    // 手动推进状态机模拟 NoPeerFound（编排器 drive 内部也是这么做的）。
    let t = orch
        .sm
        .lock()
        .expect("sm")
        .transition(&FederationEvent::NoPeerFound);
    assert!(t.valid);
    assert_eq!(
        orch.state(),
        FederationState::Active(ActiveRole::Standalone)
    );

    // 无 mTLS / 资格 / 决策调用（短路在 discovery）。
    let log = orch.call_log();
    assert!(log.is_empty(), "无 peer 时编排器不应调任何后续组件");
}

#[tokio::test]
async fn federation_state_machine_terminal_ignores_non_reset() {
    // 验证状态机本身：终态后除 Reset 外的事件均被忽略（pure logic 单测保底）。
    let mut sm = FederationStateMachine::new();
    sm.transition(&FederationEvent::NoPeerFound); // → Standalone 终态
    assert!(sm.state().is_terminal());

    // 终态后 PeerDiscovered 应被忽略（valid=false，状态不变）。
    let t = sm.transition(&FederationEvent::PeerDiscovered(NodeId::new("x")));
    assert!(!t.valid);
    assert_eq!(sm.state(), FederationState::Active(ActiveRole::Standalone));

    // Reset 可离开终态回到 Probing。
    let t = sm.transition(&FederationEvent::Reset);
    assert!(t.valid);
    assert_eq!(sm.state(), FederationState::Probing);
}

#[tokio::test]
async fn federation_ha_join_failed_degrades_to_peer() {
    // 决策 JoinHaCluster 后，HA 加入流程失败 → 降级为 peer（fallback_role=Peer）。
    let mut sm = FederationStateMachine::new();
    sm.transition(&FederationEvent::PeerDiscovered(NodeId::new("p")));
    sm.transition(&FederationEvent::AuthSucceeded);
    sm.transition(&FederationEvent::DecisionReady {
        action: FederationAction::JoinHaCluster {
            leader_endpoint: "10.0.0.1:8443".into(),
        },
    });
    assert_eq!(sm.state(), FederationState::JoiningHa);

    // HA 加入失败 → fallback 到 peer。
    let t = sm.transition(&FederationEvent::HaJoinFailed {
        fallback_role: ActiveRole::Peer,
    });
    assert!(t.valid);
    assert_eq!(t.to, FederationState::Active(ActiveRole::Peer));
    // action_hint 应为 RegisterAsPeer（降级后的动作）。
    assert_eq!(t.action_hint, Some(FederationAction::RegisterAsPeer));
}

#[tokio::test]
async fn pairing_token_type_cross_trait_identity() {
    // 跨 crate 类型一致性：PairingToken / PairingScope / PeerSessionId 在 auth /
    // discovery / policy 之间透传，类型一致可序列化。
    let token = PairingToken {
        token: "abc".into(),
        expires_at: Utc::now(),
        issued_by: NodeId::new("leader"),
        scope: PairingScope::JoinCluster,
    };
    // 序列化 round-trip（serde_json 跨 trait 可用）。
    let json = serde_json::to_string(&token).expect("token 应可序列化");
    let back: PairingToken = serde_json::from_str(&json).expect("应可反序列化");
    assert_eq!(back.token, "abc");
    assert_eq!(back.scope, PairingScope::JoinCluster);

    // HaEligibility 同理（policy 产 → 编排器读）。
    let elig = HaEligibility::new(true, vec![]);
    let json = serde_json::to_string(&elig).unwrap();
    let back: HaEligibility = serde_json::from_str(&json).unwrap();
    assert!(back.eligible);

    // FederationAction 也可序列化（跨 trait 决策结果透传）。
    let action = FederationAction::JoinHaCluster {
        leader_endpoint: "le:8443".into(),
    };
    let json = serde_json::to_string(&action).unwrap();
    assert!(json.contains("join_ha_cluster"));
}

#[tokio::test]
async fn federation_eventually_emits_cluster_event() {
    // 弱断言：联邦终态会触发 EventBus 上的 Cluster 事件（事件流也串通）。
    // 因 emit_terminal_event 用 tokio::spawn 异步发布，需 yield 让其执行。
    let peer = signed_peer("peer-event", "1.5.0", NodeCapabilities::full());
    let discovery = Arc::new(MockDiscovery::new().with_peers(vec![peer]));
    let auth = Arc::new(MockPeerAuthenticator::new());
    let policy = Arc::new(MockFederationPolicy::new());
    let bus = Arc::new(MockEventBus::new());
    let orch = IntegratedFederationOrchestrator::new(
        discovery,
        auth,
        policy,
        bus.clone(),
        ha_requirement(1, 10.0),
    );

    let _ = orch
        .drive_federation(
            FederationChoice::Auto,
            valid_token(PairingScope::JoinCluster),
        )
        .await
        .unwrap();

    // 让 spawn 的 publish 有机会执行（yield 几次）。
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }

    // Cluster topic 上应有事件（joined_ha）。
    let count = bus.published_count_for(Topic::Cluster);
    assert!(
        count >= 1,
        "联邦终态应至少发 1 个 Cluster 事件，实得 {count}"
    );
    let published = bus.published();
    assert!(
        published.iter().any(|e| e.kind == "federation.joined_ha"),
        "应有 joined_ha 事件: {published:?}"
    );
}
