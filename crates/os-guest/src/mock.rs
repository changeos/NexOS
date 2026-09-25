//! Mock 实现（仅 `mock` feature 下编译）。
//!
//! 提供 5 个 Mock（[`MockCaptivePortal`] / [`MockIdentityEngine`] /
//! [`MockPolicyEngine`] / [`MockNftRuleOrchestrator`] / [`MockChainOrchestrator`]），
//! 供下游 agent（api-agent）单元测/集成测注入。
//!
//! 行为：纯内存、确定性；构造器 `MockXxx::new().with_*()` 设置预期返回值，
//! 未配置时返回安全默认值（不 panic）。

use std::sync::Mutex;

use os_core::{GuestId, PageRequest, PageResponse, TaskId};

use crate::chain::{ChainVerificationConfig, ChainVerificationStatus};
use crate::identity::GuestFilter;
use crate::model::{GuestIdentity, GuestIdentityType, GuestStatus};
use crate::nft::{DryRunResult, NftGuestRule};
use crate::policy::{GuestAction, GuestContext, PolicyDecision, PolicyRule};
use crate::portal::{PortalConfig, PortalResponse, ProbeRequest};

// ============================================================================
// MockCaptivePortal
// ============================================================================

/// `CaptivePortal` 的 mock 实现。
///
/// 默认：start/stop 成功；handle_detection 返回 Pass（已放行）。
/// 可用 `with_landing` 设置返回 Landing HTML。
pub struct MockCaptivePortal {
    landing_html: Mutex<Option<String>>,
    started: Mutex<bool>,
}

impl MockCaptivePortal {
    /// 构造默认 mock（handle_detection 返回 Pass）。
    pub fn new() -> Self {
        Self {
            landing_html: Mutex::new(None),
            started: Mutex::new(false),
        }
    }

    /// 设置未认证时返回的落地页 HTML（设后 handle_detection 返回 Landing）。
    pub fn with_landing(self, html: impl Into<String>) -> Self {
        *self.landing_html.lock().expect("mock landing") = Some(html.into());
        self
    }

    /// 是否已启动。
    pub fn is_started(&self) -> bool {
        *self.started.lock().expect("mock started")
    }
}

impl Default for MockCaptivePortal {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::portal::CaptivePortal for MockCaptivePortal {
    async fn start(&self, _config: PortalConfig) -> Result<(), crate::GuestError> {
        *self.started.lock().expect("mock started") = true;
        Ok(())
    }

    async fn stop(&self) -> Result<(), crate::GuestError> {
        *self.started.lock().expect("mock started") = false;
        Ok(())
    }

    async fn handle_detection(
        &self,
        _request: ProbeRequest,
    ) -> Result<PortalResponse, crate::GuestError> {
        let html = self.landing_html.lock().expect("mock landing").clone();
        Ok(match html {
            Some(h) => PortalResponse::Landing { html: h },
            None => PortalResponse::Pass,
        })
    }
}

// ============================================================================
// MockIdentityEngine
// ============================================================================

/// `IdentityEngine` 的 mock 实现。
///
/// 默认：create_guest 生成一个固定的内存访客（Pending）；authenticate 切到 Authed；
/// revoke 切到 Revoked；list 返回全部。可用 `with_guest` 预置访客。
pub struct MockIdentityEngine {
    guests: Mutex<std::collections::HashMap<GuestId, GuestIdentity>>,
    create_fails: Mutex<bool>,
}

impl MockIdentityEngine {
    /// 构造默认 mock。
    pub fn new() -> Self {
        Self {
            guests: Mutex::new(std::collections::HashMap::new()),
            create_fails: Mutex::new(false),
        }
    }

    /// 预置一个访客（测试用）。
    pub fn with_guest(self, guest: GuestIdentity) -> Self {
        self.guests
            .lock()
            .expect("mock guests")
            .insert(guest.id.clone(), guest);
        self
    }

    /// 设置 create_guest 是否失败。
    pub fn with_create_failing(self, fails: bool) -> Self {
        *self.create_fails.lock().expect("mock create_fails") = fails;
        self
    }
}

impl Default for MockIdentityEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::identity::IdentityEngine for MockIdentityEngine {
    async fn create_guest(
        &self,
        id_type: GuestIdentityType,
    ) -> Result<GuestIdentity, crate::GuestError> {
        if *self.create_fails.lock().expect("mock create_fails") {
            return Err(crate::GuestError::Internal(
                "mock create 失败开关已打开".into(),
            ));
        }
        let now = os_core::Utc::now();
        let id = GuestId::new(format!(
            "GUEST-MOCK{:03}",
            self.guests.lock().expect("mock guests").len()
        ));
        let guest = GuestIdentity {
            id: id.clone(),
            identity_type: id_type,
            created_at: now,
            expires_at: now + chrono::Duration::hours(1),
            jwt_expiry: now + chrono::Duration::minutes(30),
            nft_timeout_secs: 1800,
            status: GuestStatus::Pending,
            metadata: serde_json::Value::Null,
        };
        self.guests
            .lock()
            .expect("mock guests")
            .insert(id.clone(), guest.clone());
        Ok(guest)
    }

    async fn authenticate_guest(&self, id: &GuestId) -> Result<GuestIdentity, crate::GuestError> {
        let mut guests = self.guests.lock().expect("mock guests");
        let g = guests
            .get_mut(id)
            .ok_or_else(|| crate::GuestError::GuestNotFound(id.to_string()))?;
        g.status = GuestStatus::Authed;
        Ok(g.clone())
    }

    async fn extend_guest(
        &self,
        id: &GuestId,
        duration: chrono::Duration,
    ) -> Result<GuestIdentity, crate::GuestError> {
        let mut guests = self.guests.lock().expect("mock guests");
        let g = guests
            .get_mut(id)
            .ok_or_else(|| crate::GuestError::GuestNotFound(id.to_string()))?;
        g.expires_at += duration;
        Ok(g.clone())
    }

    async fn revoke_guest(&self, id: &GuestId) -> Result<(), crate::GuestError> {
        let mut guests = self.guests.lock().expect("mock guests");
        let g = guests
            .get_mut(id)
            .ok_or_else(|| crate::GuestError::GuestNotFound(id.to_string()))?;
        g.status = GuestStatus::Revoked;
        Ok(())
    }

    async fn list_guests(
        &self,
        filter: GuestFilter,
        page: PageRequest,
    ) -> Result<PageResponse<GuestIdentity>, crate::GuestError> {
        let guests = self.guests.lock().expect("mock guests");
        let mut all: Vec<GuestIdentity> = guests
            .values()
            .filter(|g| filter.status.map(|s| g.status == s).unwrap_or(true))
            .filter(|g| filter.id_type.map(|t| g.identity_type == t).unwrap_or(true))
            .cloned()
            .collect();
        all.sort_by_key(|g| std::cmp::Reverse(g.created_at));
        let total = all.len() as u32;
        let offset = page.offset.min(total) as usize;
        let items: Vec<GuestIdentity> = all
            .into_iter()
            .skip(offset)
            .take(page.limit as usize)
            .collect();
        Ok(PageResponse {
            items,
            total,
            offset: page.offset,
            limit: page.limit,
        })
    }
}

// ============================================================================
// MockPolicyEngine
// ============================================================================

/// `PolicyEngine` 的 mock 实现。
///
/// 默认：evaluate 返回 Allow；可用 `with_decision` 覆盖返回。
pub struct MockPolicyEngine {
    decision: Mutex<PolicyDecision>,
    rules: Mutex<Vec<PolicyRule>>,
}

impl MockPolicyEngine {
    /// 构造默认 mock（evaluate 返回 Allow）。
    pub fn new() -> Self {
        Self {
            decision: Mutex::new(PolicyDecision {
                allowed: true,
                matched_rule: Some("mock-allow".into()),
                reason: "mock 默认放行".into(),
            }),
            rules: Mutex::new(Vec::new()),
        }
    }

    /// 设置 evaluate 固定返回的决策。
    pub fn with_decision(self, d: PolicyDecision) -> Self {
        *self.decision.lock().expect("mock decision") = d;
        self
    }
}

impl Default for MockPolicyEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::policy::PolicyEngine for MockPolicyEngine {
    async fn evaluate(
        &self,
        _guest: &GuestIdentity,
        _action: &GuestAction,
        _context: &GuestContext,
    ) -> Result<PolicyDecision, crate::GuestError> {
        Ok(self.decision.lock().expect("mock decision").clone())
    }

    async fn add_rule(&self, rule: PolicyRule) -> Result<String, crate::GuestError> {
        let id = format!("mock-rule-{}", self.rules.lock().expect("mock rules").len());
        self.rules.lock().expect("mock rules").push(rule);
        Ok(id)
    }

    async fn delete_rule(&self, id: &str) -> Result<(), crate::GuestError> {
        let mut rules = self.rules.lock().expect("mock rules");
        if let Some(pos) = rules.iter().position(|r| r.priority.to_string() == id) {
            rules.remove(pos);
        }
        Ok(())
    }

    async fn list_rules(&self) -> Vec<PolicyRule> {
        self.rules.lock().expect("mock rules").clone()
    }
}

// ============================================================================
// MockNftRuleOrchestrator
// ============================================================================

/// `NftRuleOrchestrator` 的 mock 实现。
///
/// 默认：dry_run 返回空冲突 + 构造的 would_change；apply/revoke 成功；
/// rollback_checkpoint 成功。可用 `with_apply_failing` 让 apply 失败。
pub struct MockNftRuleOrchestrator {
    apply_fails: Mutex<bool>,
    applied: Mutex<Vec<NftGuestRule>>,
}

impl MockNftRuleOrchestrator {
    /// 构造默认 mock。
    pub fn new() -> Self {
        Self {
            apply_fails: Mutex::new(false),
            applied: Mutex::new(Vec::new()),
        }
    }

    /// 设置 apply 是否失败。
    pub fn with_apply_failing(self, fails: bool) -> Self {
        *self.apply_fails.lock().expect("mock apply_fails") = fails;
        self
    }

    /// 已 apply 的规则（测试断言用）。
    pub fn applied_rules(&self) -> Vec<NftGuestRule> {
        self.applied.lock().expect("mock applied").clone()
    }
}

impl Default for MockNftRuleOrchestrator {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::nft::NftRuleOrchestrator for MockNftRuleOrchestrator {
    async fn dry_run(&self, rule: &NftGuestRule) -> Result<DryRunResult, crate::GuestError> {
        let would_change = crate::nft::statements_for_rule(rule)?;
        Ok(DryRunResult {
            would_change,
            conflicts: Vec::new(),
        })
    }

    async fn apply(&self, rule: NftGuestRule) -> Result<(), crate::GuestError> {
        if *self.apply_fails.lock().expect("mock apply_fails") {
            return Err(crate::GuestError::NftRuleFailed(
                "mock apply 失败开关已打开".into(),
            ));
        }
        self.applied.lock().expect("mock applied").push(rule);
        Ok(())
    }

    async fn revoke(&self, _guest_ip: &str) -> Result<(), crate::GuestError> {
        Ok(())
    }

    async fn rollback_checkpoint(&self, _checkpoint_id: &str) -> Result<(), crate::GuestError> {
        Ok(())
    }
}

// ============================================================================
// MockChainOrchestrator
// ============================================================================

/// `ChainOrchestrator` 的 mock 实现。
///
/// 默认：start_verification 返回固定 TaskId 并把 status 置为 Completed（address_hash=mock）；
/// verification_status 返回该状态。可用 `with_status` 覆盖。
pub struct MockChainOrchestrator {
    status: Mutex<ChainVerificationStatus>,
    next_task: Mutex<u64>,
}

impl MockChainOrchestrator {
    /// 构造默认 mock（验证直接成功）。
    pub fn new() -> Self {
        Self {
            status: Mutex::new(ChainVerificationStatus::Completed {
                address_hash: "addr-mock-0000000000000000".into(),
            }),
            next_task: Mutex::new(0),
        }
    }

    /// 设置 verification_status 固定返回的状态。
    pub fn with_status(self, status: ChainVerificationStatus) -> Self {
        *self.status.lock().expect("mock status") = status;
        self
    }
}

impl Default for MockChainOrchestrator {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::chain::ChainOrchestrator for MockChainOrchestrator {
    async fn start_verification(
        &self,
        _guest: &GuestId,
        _config: &ChainVerificationConfig,
    ) -> Result<TaskId, crate::GuestError> {
        let mut n = self.next_task.lock().expect("mock next_task");
        *n += 1;
        // TaskId 内部为 Uuid；mock 用固定 Uuid 派生保证可重复。
        let id = os_core::Uuid::new_v4();
        Ok(TaskId(id))
    }

    async fn verification_status(
        &self,
        _task: &TaskId,
    ) -> Result<ChainVerificationStatus, crate::GuestError> {
        Ok(self.status.lock().expect("mock status").clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::ChainOrchestrator;
    use crate::identity::IdentityEngine;
    use crate::nft::NftRuleOrchestrator;
    use crate::policy::PolicyEngine;
    use crate::portal::CaptivePortal;

    #[tokio::test]
    async fn mock_captive_portal_default_pass_then_landing() {
        let p = MockCaptivePortal::new();
        let req = ProbeRequest {
            user_agent: "x".into(),
            host: "h".into(),
            path: "/".into(),
        };
        assert!(matches!(
            p.handle_detection(req).await.unwrap(),
            PortalResponse::Pass
        ));
        let p2 = MockCaptivePortal::new().with_landing("<h1>hi</h1>");
        let req2 = ProbeRequest {
            user_agent: "x".into(),
            host: "h".into(),
            path: "/".into(),
        };
        match p2.handle_detection(req2).await.unwrap() {
            PortalResponse::Landing { html } => assert_eq!(html, "<h1>hi</h1>"),
            other => panic!("期望 Landing，实际 {other:?}"),
        }
        // start/stop。
        p2.start(PortalConfig {
            listen_http: 80,
            listen_https: 443,
            vlan_id: None,
            landing_html: None,
            ap_bridge: false,
        })
        .await
        .unwrap();
        assert!(p2.is_started());
        p2.stop().await.unwrap();
        assert!(!p2.is_started());
    }

    #[tokio::test]
    async fn mock_identity_engine_lifecycle() {
        let e = MockIdentityEngine::new();
        let g = e.create_guest(GuestIdentityType::RandomId).await.unwrap();
        assert_eq!(g.status, GuestStatus::Pending);
        let g2 = e.authenticate_guest(&g.id).await.unwrap();
        assert_eq!(g2.status, GuestStatus::Authed);
        e.revoke_guest(&g.id).await.unwrap();

        let failing = MockIdentityEngine::new().with_create_failing(true);
        assert!(failing
            .create_guest(GuestIdentityType::RandomId)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn mock_policy_engine_default_allow_and_override() {
        let pe = MockPolicyEngine::new();
        let guest = GuestIdentity {
            id: GuestId::new("GUEST-X"),
            identity_type: GuestIdentityType::RandomId,
            created_at: os_core::Utc::now(),
            expires_at: os_core::Utc::now(),
            jwt_expiry: os_core::Utc::now(),
            nft_timeout_secs: 0,
            status: GuestStatus::Authed,
            metadata: serde_json::Value::Null,
        };
        let ctx = GuestContext::new("1.1.1.1");
        let d = pe
            .evaluate(&guest, &GuestAction::Authenticate, &ctx)
            .await
            .unwrap();
        assert!(d.allowed);

        let pe2 = MockPolicyEngine::new().with_decision(PolicyDecision {
            allowed: false,
            matched_rule: None,
            reason: "mock deny".into(),
        });
        let d2 = pe2
            .evaluate(&guest, &GuestAction::Authenticate, &ctx)
            .await
            .unwrap();
        assert!(!d2.allowed);
    }

    #[tokio::test]
    async fn mock_nft_orchestrator_apply_and_fail() {
        let o = MockNftRuleOrchestrator::new();
        let rule = NftGuestRule {
            guest_ip: "10.0.0.5".to_string(),
            action: crate::nft::NftGuestAction::Authenticate {
                allowed_ports: vec![443],
            },
            timeout_secs: 3600,
        };
        o.apply(rule.clone()).await.unwrap();
        assert_eq!(o.applied_rules().len(), 1);

        let of = MockNftRuleOrchestrator::new().with_apply_failing(true);
        assert!(of.apply(rule).await.is_err());
    }

    #[tokio::test]
    async fn mock_chain_orchestrator_default_completed() {
        let c = MockChainOrchestrator::new();
        let guest = GuestId::new("GUEST-X");
        let cfg = ChainVerificationConfig {
            required_factors: vec![],
            chain: os_wallet::ChainKind::Evm,
            role_on_success: None,
            privacy_mode: crate::chain::PrivacyMode::Optional,
        };
        let task = c.start_verification(&guest, &cfg).await.unwrap();
        let st = c.verification_status(&task).await.unwrap();
        match st {
            ChainVerificationStatus::Completed { address_hash } => {
                assert!(!address_hash.is_empty())
            }
            other => panic!("期望 Completed，实际 {other:?}"),
        }
    }
}
