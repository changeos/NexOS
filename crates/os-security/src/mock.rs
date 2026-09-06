//! os-security mock 实现——供下游 agent（wallet/guest/im/api）测试。
//!
//! Feature gate：`mock`。编译条件 `#[cfg(feature = "mock")]`。
//! 行为：纯内存、确定性、不 panic；构造器可预设返回值。
//! 见协作约定 §5（Mock 约定）。

#![cfg(feature = "mock")]

use crate::auth::{Principal, Role, User, UserId};
use crate::cert::Certificate;
use crate::jwt::JwtClaims;
use crate::twofactor::TwoFactorSecret;
use crate::vpn::{VpnPeer, VpnStatus};
use crate::SecurityError;
use os_core::DateTime;
use std::collections::HashMap;
use std::sync::Mutex;

// ============================================================================
// MockAuthProvider
// ============================================================================

/// `AuthProvider` 的 mock 实现。
///
/// 默认行为：`create_user` 成功生成内存用户；`authenticate` 按 `with_credential`
/// 预设的密码校验；可用 `with_auth_result` 覆盖 `authenticate` 强制返回错误。
pub struct MockAuthProvider {
    users: Mutex<HashMap<UserId, User>>,
    creds: Mutex<HashMap<UserId, String>>,
    auth_fail: Mutex<bool>,
}

impl MockAuthProvider {
    pub fn new() -> Self {
        Self {
            users: Mutex::new(HashMap::new()),
            creds: Mutex::new(HashMap::new()),
            auth_fail: Mutex::new(false),
        }
    }

    /// 预置一个用户 + 明文密码（仅 mock 用，内部走常量时间比对）。
    pub fn with_credential(self, name: &str, roles: Vec<Role>, password: &str) -> Self {
        let id = UserId::new(format!("mock-{name}"));
        let user =
            User::new(id.clone(), name, roles, chrono::Utc::now()).expect("mock 用户构造不应失败");
        self.users.lock().unwrap().insert(id.clone(), user);
        self.creds.lock().unwrap().insert(id, password.to_string());
        self
    }

    /// 强制 `authenticate` 返回 `AuthFailed`（模拟认证失败场景）。
    pub fn with_auth_fail(self) -> Self {
        *self.auth_fail.lock().unwrap() = true;
        self
    }

    fn now() -> DateTime {
        chrono::Utc::now()
    }
}

impl Default for MockAuthProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(async_fn_in_trait)]
impl crate::auth::AuthProvider for MockAuthProvider {
    async fn create_user(&self, name: &str, roles: Vec<Role>) -> Result<User, SecurityError> {
        crate::auth::validate_roles(&roles)?;
        let id = UserId::new(uuid::Uuid::new_v4().to_string());
        let user = User::new(id.clone(), name, roles, Self::now())?;
        self.users.lock().unwrap().insert(id, user.clone());
        Ok(user)
    }

    async fn delete_user(&self, user: &UserId) -> Result<(), SecurityError> {
        self.users
            .lock()
            .unwrap()
            .remove(user)
            .map(|_| ())
            .ok_or_else(|| SecurityError::UserNotFound(user.to_string()))
    }

    async fn list_users(&self) -> Result<Vec<User>, SecurityError> {
        Ok(self.users.lock().unwrap().values().cloned().collect())
    }

    async fn set_password(&self, user: &UserId, password: &str) -> Result<(), SecurityError> {
        if !self.users.lock().unwrap().contains_key(user) {
            return Err(SecurityError::UserNotFound(user.to_string()));
        }
        // mock 路径：直接存（仅测试用，不走 argon2）
        self.creds
            .lock()
            .unwrap()
            .insert(user.clone(), password.to_string());
        Ok(())
    }

    async fn authenticate(&self, name: &str, password: &str) -> Result<Principal, SecurityError> {
        if *self.auth_fail.lock().unwrap() {
            return Err(SecurityError::AuthFailed);
        }
        let users = self.users.lock().unwrap();
        let user = users
            .values()
            .find(|u| u.name == name)
            .cloned()
            .ok_or(SecurityError::AuthFailed)?;
        if !user.enabled {
            return Err(SecurityError::AuthFailed);
        }
        let stored = self
            .creds
            .lock()
            .unwrap()
            .get(&user.id)
            .cloned()
            .ok_or(SecurityError::AuthFailed)?;
        if !crate::password::verify_password(password, &stored) {
            return Err(SecurityError::AuthFailed);
        }
        Principal::new(user.clone(), user.roles.clone(), Self::now())
    }

    async fn disable_user(&self, user: &UserId) -> Result<(), SecurityError> {
        let mut users = self.users.lock().unwrap();
        let u = users
            .get_mut(user)
            .ok_or_else(|| SecurityError::UserNotFound(user.to_string()))?;
        u.enabled = false;
        Ok(())
    }
}

// ============================================================================
// MockJwtIssuer
// ============================================================================

/// `JwtIssuer` 的 mock 实现。
///
/// `issue`：把 claims 序列化为字符串作为"token"（非真实 JWT，仅供测试流转）。
/// `verify`：反序列化回 claims；可用 `with_verify_fail` 强制失败。
pub struct MockJwtIssuer {
    verify_fail: Mutex<bool>,
}

impl MockJwtIssuer {
    pub fn new() -> Self {
        Self {
            verify_fail: Mutex::new(false),
        }
    }

    /// 强制 `verify` 返回 `JwtInvalid`。
    pub fn with_verify_fail(self) -> Self {
        *self.verify_fail.lock().unwrap() = true;
        self
    }
}

impl Default for MockJwtIssuer {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl crate::jwt::JwtIssuer for MockJwtIssuer {
    async fn issue(&self, claims: JwtClaims) -> Result<String, SecurityError> {
        serde_json::to_string(&claims)
            .map_err(|e| SecurityError::JwtInvalid(format!("mock 序列化失败: {e}")))
    }

    async fn verify(&self, token: &str) -> Result<JwtClaims, SecurityError> {
        if *self.verify_fail.lock().unwrap() {
            return Err(SecurityError::JwtInvalid("mock 强制失败".into()));
        }
        serde_json::from_str::<JwtClaims>(token)
            .map_err(|e| SecurityError::JwtInvalid(format!("mock 反序列化失败: {e}")))
    }

    async fn rotate_keys(&self) -> Result<(), SecurityError> {
        Ok(())
    }
}

// ============================================================================
// MockCertManager
// ============================================================================

/// `CertManager` 的 mock 实现——返回内存构造的占位证书。
pub struct MockCertManager;

impl MockCertManager {
    pub fn new() -> Self {
        Self
    }

    fn fake_cert(cn: &str) -> Certificate {
        Certificate {
            id: format!("mock-cert-{cn}"),
            common_name: cn.to_string(),
            not_before: chrono::Utc::now(),
            not_after: chrono::Utc::now() + chrono::Duration::days(365),
            issuer: "mock-ca".into(),
            serial: uuid::Uuid::new_v4().to_string(),
            auto_renew: true,
        }
    }
}

impl Default for MockCertManager {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(async_fn_in_trait)]
impl crate::cert::CertManager for MockCertManager {
    async fn init_ca(&self, common_name: &str) -> Result<Certificate, SecurityError> {
        Ok(Self::fake_cert(common_name))
    }
    async fn sign(&self, _csr: &[u8], _days: u32) -> Result<Vec<u8>, SecurityError> {
        Ok(b"mock-cert-bytes".to_vec())
    }
    async fn list_certs(&self) -> Result<Vec<Certificate>, SecurityError> {
        Ok(vec![Self::fake_cert("mock")])
    }
    async fn renew(&self, _id: &str) -> Result<(), SecurityError> {
        Ok(())
    }
    async fn acme_request(&self, domain: &str) -> Result<Certificate, SecurityError> {
        Ok(Self::fake_cert(domain))
    }
}

// ============================================================================
// MockTwoFactor
// ============================================================================

/// `TwoFactor` 的 mock 实现。
///
/// `verify`：可用 `with_verify_ok`/`with_verify_fail` 预设结果；默认接受 code "000000"。
pub struct MockTwoFactor {
    verify_ok: Mutex<Option<bool>>,
}

impl MockTwoFactor {
    pub fn new() -> Self {
        Self {
            verify_ok: Mutex::new(None),
        }
    }
    pub fn with_verify_ok(self) -> Self {
        *self.verify_ok.lock().unwrap() = Some(true);
        self
    }
    pub fn with_verify_fail(self) -> Self {
        *self.verify_ok.lock().unwrap() = Some(false);
        self
    }
}

impl Default for MockTwoFactor {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(async_fn_in_trait)]
impl crate::twofactor::TwoFactor for MockTwoFactor {
    async fn enable(&self, user: &UserId) -> Result<TwoFactorSecret, SecurityError> {
        Ok(TwoFactorSecret {
            user_id: user.clone(),
            encrypted: "mock-encrypted".into(),
            otpauth_uri: Some(format!("otpauth://totp/mock:{}?secret=MOCK", user)),
        })
    }
    async fn verify(&self, _user: &UserId, code: &str) -> Result<bool, SecurityError> {
        Ok(self
            .verify_ok
            .lock()
            .unwrap()
            .unwrap_or_else(|| code == "000000"))
    }
    async fn disable(&self, _user: &UserId) -> Result<(), SecurityError> {
        Ok(())
    }
}

// ============================================================================
// MockVpnManager
// ============================================================================

/// `VpnManager` 的 mock 实现——内存 peer 表 + 静态状态。
pub struct MockVpnManager {
    peers: Mutex<Vec<VpnPeer>>,
}

impl MockVpnManager {
    pub fn new() -> Self {
        Self {
            peers: Mutex::new(Vec::new()),
        }
    }
}

impl Default for MockVpnManager {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(async_fn_in_trait)]
impl crate::vpn::VpnManager for MockVpnManager {
    async fn add_peer(&self, peer: VpnPeer) -> Result<(), SecurityError> {
        self.peers.lock().unwrap().push(peer);
        Ok(())
    }
    async fn remove_peer(&self, pub_key: &str) -> Result<(), SecurityError> {
        let mut peers = self.peers.lock().unwrap();
        let before = peers.len();
        peers.retain(|p| p.public_key != pub_key);
        if peers.len() == before {
            return Err(SecurityError::VpnError(format!("peer 不存在: {pub_key}")));
        }
        Ok(())
    }
    async fn list_peers(&self) -> Result<Vec<VpnPeer>, SecurityError> {
        Ok(self.peers.lock().unwrap().clone())
    }
    async fn status(&self) -> Result<VpnStatus, SecurityError> {
        let count = self.peers.lock().unwrap().len() as u32;
        Ok(VpnStatus {
            running: true,
            listen_port: 51820,
            peer_count: count,
            bytes_rx: 0,
            bytes_tx: 0,
        })
    }
}

// 让未使用的 import 在 mock feature 下不报警（DateTime 用于 now() 辅助）。

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthProvider;
    use crate::cert::CertManager;
    use crate::jwt::{JwtIssuer, TokenType};
    use crate::twofactor::TwoFactor;
    use crate::vpn::VpnManager;

    #[tokio::test]
    async fn mock_auth_ok() {
        let p = MockAuthProvider::new().with_credential("carol", vec![Role::User], "pw");
        let prin = p.authenticate("carol", "pw").await.expect("auth ok");
        assert_eq!(prin.user.name, "carol");
    }

    #[tokio::test]
    async fn mock_auth_fail() {
        let p = MockAuthProvider::new().with_credential("d", vec![Role::User], "pw");
        assert!(p.authenticate("d", "wrong").await.is_err());
        let p2 = MockAuthProvider::new()
            .with_credential("d", vec![Role::User], "pw")
            .with_auth_fail();
        assert!(p2.authenticate("d", "pw").await.is_err());
    }

    #[tokio::test]
    async fn mock_jwt_roundtrip() {
        let j = MockJwtIssuer::new();
        let claims = JwtClaims {
            sub: UserId::new("s"),
            roles: vec![Role::User],
            exp: 100,
            iat: 0,
            token_type: TokenType::Access,
            custom: serde_json::Value::Null,
        };
        let tok = j.issue(claims.clone()).await.unwrap();
        let back = j.verify(&tok).await.unwrap();
        assert_eq!(back.sub, claims.sub);
    }

    #[tokio::test]
    async fn mock_jwt_verify_fail() {
        let j = MockJwtIssuer::new().with_verify_fail();
        assert!(j.verify("x").await.is_err());
    }

    #[tokio::test]
    async fn mock_cert() {
        let c = MockCertManager::new();
        assert!(c.init_ca("ca").await.is_ok());
        assert!(!c.list_certs().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn mock_twofactor() {
        let t = MockTwoFactor::new();
        assert!(t.verify(&UserId::new("u"), "000000").await.unwrap());
        assert!(!t.verify(&UserId::new("u"), "111111").await.unwrap());
        let tf = MockTwoFactor::new().with_verify_ok();
        assert!(tf.verify(&UserId::new("u"), "anything").await.unwrap());
    }

    #[tokio::test]
    async fn mock_vpn() {
        let v = MockVpnManager::new();
        let peer = VpnPeer {
            public_key: "mk".into(),
            allowed_ips: vec![],
            endpoint: None,
            user: None,
        };
        v.add_peer(peer).await.unwrap();
        assert_eq!(v.list_peers().await.unwrap().len(), 1);
        let st = v.status().await.unwrap();
        assert!(st.running);
        assert_eq!(st.peer_count, 1);
    }
}
