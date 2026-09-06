//! 场景 2：访客链上验证链路（规划文档 §3.18.1 / integration-agent 规格书 §3 I1+I5）
//!
//! 链路：os-guest CaptivePortal 收访客 → ChainOrchestrator 调
//! os-wallet WalletConnector（建 session）→ ChainAdapter（verify_signature +
//! query_balance/query_credential）→ RpcRegistry（判链可用）→ os-security
//! JwtIssuer 签 JWT → os-im ConversationStore 推通知（系统消息）。
//!
//! 重点验证：
//! - 跨 crate 类型桥接：guest `DefaultChainOrchestrator` 用泛型参数注入 wallet
//!   三件套 + security JwtIssuer（因为上游 trait 是原生 async fn in trait，非 dyn 兼容）。
//! - 编排全链通过：链可用 → 建 session → 签名 → 验签 → 因子满足 → 签 JWT → Completed。
//! - 错误降级（§3.18.1 三档隐私）：链不可用 + Mandatory = Failed；
//!   链不可用 + Optional = Completed（空 address_hash，降级为常规访客）。
//! - 验签失败：verify_signature 返回 false → Failed。
//! - 余额不足因子：BalanceThreshold 未满足 → Failed。
//! - 通知尾：JWT 签发后向 os-im 写一条 System 消息（链上验证完成通知）。

use std::sync::Arc;

use os_core::{GuestId, Uuid};
use os_guest::chain::{
    ChainOrchestrator, ChainVerificationConfig, ChainVerificationStatus, PrivacyMode,
};
use os_guest::impls::DefaultChainOrchestrator;
use os_im::conversation::{ConversationId, ConversationStore, Message, MessageRole};
use os_im::MockConversationStore;
use os_security::jwt::JwtIssuer;
use os_security::{JwtClaims, MockJwtIssuer, TokenType};
use os_wallet::connector::WalletConnector;
use os_wallet::mock::{MockChainAdapter, MockRpcRegistry, MockWalletConnector};
use os_wallet::registry::RpcRegistry;
use os_wallet::{ChainKind, VerificationFactor};

// ----------------------------------------------------------------------------
// 编排包装：在 ChainOrchestrator 之上接 os-im 通知尾。
// ChainOrchestrator 只到签 JWT；本测试再串一步——把结果写入对话消息（系统通知）。
// ----------------------------------------------------------------------------

struct GuestVerificationFlow {
    orch: Arc<DefaultChainOrchestrator>,
    store: Arc<MockConversationStore>,
}

impl GuestVerificationFlow {
    fn new(orch: DefaultChainOrchestrator, store: MockConversationStore) -> Self {
        Self {
            orch: Arc::new(orch),
            store: Arc::new(store),
        }
    }

    /// 端到端跑一次访客链上验证，并把结果写一条系统消息进对话（模拟 IM 通知尾）。
    async fn run(
        &self,
        guest: &GuestId,
        config: &ChainVerificationConfig,
        user: &str,
    ) -> Result<(ChainVerificationStatus, ConversationId), os_im::ImError> {
        // 1. 起一个对话（访客首次进入 Captive Portal 后建立）。
        let conv = self.store.create_conversation(user).await?;

        // 2. 启动链上验证（同步执行编排：建 session → 签名 → 验签 → 签 JWT）。
        let task = self
            .orch
            .start_verification(guest, config)
            .await
            .expect("start_verification 不应返回 Err");

        // 3. 取最终状态。
        let status = self
            .orch
            .verification_status(&task)
            .await
            .expect("verification_status 不应返回 Err");

        // 4. 通知尾：把结果写一条系统消息进对话。
        let text = match &status {
            ChainVerificationStatus::Completed { address_hash } => {
                if address_hash.is_empty() {
                    "链上验证降级：链不可用，已按 Optional 模式放行".to_string()
                } else {
                    format!("链上验证通过：address_hash={}", address_hash)
                }
            }
            ChainVerificationStatus::Failed { reason } => {
                format!("链上验证失败：{reason}")
            }
            other => format!("链上验证未完成：{other:?}"),
        };
        let msg = Message {
            id: format!("msg-{}", Uuid::new_v4()),
            conversation: conv.clone(),
            role: MessageRole::System,
            content: text,
            tool_calls: vec![],
            timestamp: os_core::Utc::now(),
        };
        self.store.add_message(msg).await?;

        Ok((status, conv))
    }
}

// ----------------------------------------------------------------------------
// 辅助构造
// ----------------------------------------------------------------------------

fn evm_config(factors: Vec<VerificationFactor>, privacy: PrivacyMode) -> ChainVerificationConfig {
    ChainVerificationConfig {
        required_factors: factors,
        chain: ChainKind::Evm,
        role_on_success: Some("guest".into()),
        privacy_mode: privacy,
    }
}

fn make_orch(
    connector: MockWalletConnector,
    adapter: MockChainAdapter,
    registry: MockRpcRegistry,
    jwt: MockJwtIssuer,
) -> DefaultChainOrchestrator {
    DefaultChainOrchestrator::new(
        Box::new(connector),
        Box::new(adapter),
        Box::new(registry),
        Box::new(jwt),
    )
}

// ----------------------------------------------------------------------------
// 集成测
// ----------------------------------------------------------------------------

#[tokio::test]
async fn guest_chain_verification_full_success() {
    // wallet 三件套：链可用 + 验签通过 + 余额足够。
    let connector = MockWalletConnector::new().with_sign_address("0xabc");
    let adapter = MockChainAdapter::new(ChainKind::Evm)
        .with_verify_result(true)
        .with_balance(1_000_000);
    let registry = MockRpcRegistry::new(); // Evm 默认 available
    let jwt = MockJwtIssuer::new();

    let orch = make_orch(connector, adapter, registry, jwt);
    let store = MockConversationStore::new();
    let flow = GuestVerificationFlow::new(orch, store);

    let guest = GuestId::new("GUEST-123456");
    let config = evm_config(
        vec![
            VerificationFactor::SignatureChallenge,
            VerificationFactor::BalanceThreshold {
                min_amount: 500_000,
            },
        ],
        PrivacyMode::Mandatory,
    );

    let (status, conv) = flow.run(&guest, &config, "alice").await.unwrap();

    // 链路全通：Completed + address_hash 非空。
    match &status {
        ChainVerificationStatus::Completed { address_hash } => {
            assert!(
                !address_hash.is_empty(),
                "成功路径应填 address_hash: {status:?}"
            );
        }
        other => panic!("应 Completed，实得 {other:?}"),
    }

    // 通知尾：对话里有一条 System 消息（含 address_hash）。
    let history = flow.store.history(&conv, 10).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].role, MessageRole::System);
    assert!(history[0].content.contains("链上验证通过"));
    assert!(history[0].content.contains("address_hash="));
}

#[tokio::test]
async fn guest_chain_verification_mandatory_chain_down_fails() {
    // 链不可用 + Mandatory → 直接 Failed（红线 §3.18.1）。
    let connector = MockWalletConnector::new();
    let adapter = MockChainAdapter::new(ChainKind::Evm).with_verify_result(true);
    let registry = MockRpcRegistry::new();
    registry.set_available(ChainKind::Evm, false);
    let jwt = MockJwtIssuer::new();

    let orch = make_orch(connector, adapter, registry, jwt);
    let store = MockConversationStore::new();
    let flow = GuestVerificationFlow::new(orch, store);

    let guest = GuestId::new("GUEST-down");
    let config = evm_config(
        vec![VerificationFactor::SignatureChallenge],
        PrivacyMode::Mandatory,
    );

    let (status, conv) = flow.run(&guest, &config, "bob").await.unwrap();

    match status {
        ChainVerificationStatus::Failed { reason } => {
            assert!(
                reason.contains("不可用") && reason.contains("Mandatory"),
                "失败原因应说明 Mandatory 拒绝，实得: {reason}"
            );
        }
        other => panic!("链不可用 + Mandatory 应 Failed，实得 {other:?}"),
    }

    // 通知尾仍写入失败原因。
    let history = flow.store.history(&conv, 10).await.unwrap();
    assert_eq!(history.len(), 1);
    assert!(history[0].content.contains("链上验证失败"));
}

#[tokio::test]
async fn guest_chain_verification_optional_chain_down_degrades() {
    // 链不可用 + Optional → Completed（空 address_hash，降级常规访客）。
    let connector = MockWalletConnector::new();
    let adapter = MockChainAdapter::new(ChainKind::Bitcoin).with_verify_result(true);
    let registry = MockRpcRegistry::new();
    registry.set_available(ChainKind::Bitcoin, false);
    let jwt = MockJwtIssuer::new();

    let orch = make_orch(connector, adapter, registry, jwt);
    let store = MockConversationStore::new();
    let flow = GuestVerificationFlow::new(orch, store);

    let guest = GuestId::new("GUEST-opt");
    let config = ChainVerificationConfig {
        required_factors: vec![VerificationFactor::SignatureChallenge],
        chain: ChainKind::Bitcoin,
        role_on_success: Some("guest".into()),
        privacy_mode: PrivacyMode::Optional,
    };

    let (status, _conv) = flow.run(&guest, &config, "carol").await.unwrap();

    match status {
        ChainVerificationStatus::Completed { address_hash } => {
            assert!(
                address_hash.is_empty(),
                "降级路径 address_hash 应为空（未走链上），实得: {address_hash}"
            );
        }
        other => panic!("Optional + 链不可用 应降级为 Completed，实得 {other:?}"),
    }
}

#[tokio::test]
async fn guest_chain_verification_signature_fails() {
    // 验签失败 → Failed。
    let connector = MockWalletConnector::new().with_sign_address("0xbad");
    let adapter = MockChainAdapter::new(ChainKind::Evm).with_verify_result(false); // 验签不过
    let registry = MockRpcRegistry::new();
    let jwt = MockJwtIssuer::new();

    let orch = make_orch(connector, adapter, registry, jwt);
    let store = MockConversationStore::new();
    let flow = GuestVerificationFlow::new(orch, store);

    let guest = GuestId::new("GUEST-bad-sig");
    let config = evm_config(
        vec![VerificationFactor::SignatureChallenge],
        PrivacyMode::Mandatory,
    );

    let (status, conv) = flow.run(&guest, &config, "dave").await.unwrap();

    match status {
        ChainVerificationStatus::Failed { reason } => {
            assert!(
                reason.contains("签名验证未通过"),
                "应报告签名验证未通过，实得: {reason}"
            );
        }
        other => panic!("验签失败应 Failed，实得 {other:?}"),
    }
    // 通知尾写入失败。
    let history = flow.store.history(&conv, 10).await.unwrap();
    assert!(history[0].content.contains("链上验证失败"));
}

#[tokio::test]
async fn guest_chain_verification_balance_below_threshold() {
    // 余额因子未满足 → Failed。
    let connector = MockWalletConnector::new().with_sign_address("0xpoor");
    let adapter = MockChainAdapter::new(ChainKind::Evm)
        .with_verify_result(true)
        .with_balance(100); // 远低于阈值 500_000
    let registry = MockRpcRegistry::new();
    let jwt = MockJwtIssuer::new();

    let orch = make_orch(connector, adapter, registry, jwt);
    let store = MockConversationStore::new();
    let flow = GuestVerificationFlow::new(orch, store);

    let guest = GuestId::new("GUEST-poor");
    let config = evm_config(
        vec![
            VerificationFactor::SignatureChallenge,
            VerificationFactor::BalanceThreshold {
                min_amount: 500_000,
            },
        ],
        PrivacyMode::Mandatory,
    );

    let (status, _conv) = flow.run(&guest, &config, "eve").await.unwrap();

    match status {
        ChainVerificationStatus::Failed { reason } => {
            assert!(
                reason.contains("余额") || reason.contains("因子"),
                "应报告余额/因子未满足，实得: {reason}"
            );
        }
        other => panic!("余额不足应 Failed，实得 {other:?}"),
    }
}

#[tokio::test]
async fn jwt_round_trip_after_verification() {
    // 验证 MockJwtIssuer 签发/校验链路本身（编排器签 JWT 后能用同 issuer 验回）。
    let jwt = MockJwtIssuer::new();
    let claims = JwtClaims {
        sub: os_security::auth::UserId("GUEST-1".into()),
        roles: vec![],
        exp: (os_core::Utc::now() + chrono::Duration::hours(1)).timestamp(),
        iat: os_core::Utc::now().timestamp(),
        token_type: TokenType::ChainCredential,
        custom: serde_json::json!({ "chain": "evm", "address_hash": "addr-deadbeef" }),
    };
    let token = jwt.issue(claims.clone()).await.unwrap();
    let back = jwt.verify(&token).await.unwrap();
    assert_eq!(back.sub.0, "GUEST-1");
    assert_eq!(back.token_type, TokenType::ChainCredential);
    assert_eq!(back.custom["address_hash"].as_str(), Some("addr-deadbeef"));
}

#[tokio::test]
async fn connector_session_round_trips_with_registry_availability() {
    // 单独验证：connector 建立会话 + registry 探活——证明 wallet mock 行为自洽。
    let connector = MockWalletConnector::new().with_sign_address("0xsess");
    let session = connector
        .connect(ChainKind::Evm, os_wallet::ConnectorKind::WalletConnectV2)
        .await
        .unwrap();
    assert_eq!(session.address.as_str(), "0xsess");

    let registry = MockRpcRegistry::new();
    assert!(registry.is_available(ChainKind::Evm).await.unwrap());
    registry.set_available(ChainKind::Evm, false);
    assert!(!registry.is_available(ChainKind::Evm).await.unwrap());
}
