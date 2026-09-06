//! 钱包连接抽象（WalletConnect v2 / 注入 / 二维码）
//!
//! 实现说明（规划文档 §3.17 / §9.1#13）：
//! - WalletConnect Relay 默认用公共 relay，可切换自托管（配置项 `wc_relay_url`）
//! - `request_signature` 触发用户钱包侧弹窗，用户确认后返回签名

use async_trait::async_trait;
use os_core::{AddressId, Deserialize, Serialize, WalletSessionId};

use crate::model::{ChainKind, SignatureAlgorithm};
use crate::WalletResult;

// ----------------------------------------------------------------------------
// 连接器 / 会话
// ----------------------------------------------------------------------------

/// 钱包连接方式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorKind {
    /// WalletConnect v2（relay 中转，跨设备）
    WalletConnectV2,
    /// 注入式（浏览器扩展钱包，同源）
    Injected,
    /// 二维码（手机钱包扫码连接）
    QrCode,
}

/// 钱包会话（已建立的连接）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletSession {
    /// 会话 ID（复用 os-core::WalletSessionId）
    pub id: WalletSessionId,
    /// 链大类
    pub chain: ChainKind,
    /// 已授权地址
    pub address: AddressId,
    /// 连接方式
    pub connector: ConnectorKind,
    /// 建立时间
    pub established_at: chrono::DateTime<chrono::Utc>,
    /// 过期时间
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

// ----------------------------------------------------------------------------
// 签名请求 / 响应
// ----------------------------------------------------------------------------

/// 签名请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignRequest {
    /// 关联会话 ID
    pub session_id: WalletSessionId,
    /// 待签名消息（原始或结构化编码后字符串）
    pub message: String,
    /// 签名算法
    pub algorithm: SignatureAlgorithm,
}

/// 签名响应
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignResponse {
    /// 签名结果（原始字节，编码由调用方决定）
    pub signature: Vec<u8>,
    /// 签名地址
    pub address: AddressId,
}

// ----------------------------------------------------------------------------
// WalletConnector trait（async）
// ----------------------------------------------------------------------------

/// 钱包连接器——发起连接、断开、请求签名。
///
/// 实现说明：WalletConnect v2 走 relay（默认公共可切自托管，配置 `wc_relay_url`）。
#[async_trait]
pub trait WalletConnector: Send + Sync {
    /// 发起连接（含 WC v2 relay 配对流程），返回建立的会话。
    async fn connect(&self, chain: ChainKind, kind: ConnectorKind) -> WalletResult<WalletSession>;

    /// 断开会话。
    async fn disconnect(&self, session: &WalletSessionId) -> WalletResult<()>;

    /// 请求用户钱包签名（触发钱包侧确认弹窗）。
    async fn request_signature(&self, req: SignRequest) -> WalletResult<SignResponse>;

    /// 列出当前活跃会话。
    async fn list_sessions(&self) -> WalletResult<Vec<WalletSession>>;
}

// ============================================================================
// 会话生命周期辅助（纯逻辑，可单测）
// ============================================================================

impl WalletSession {
    /// 构造一个会话，自动生成 session id 与默认有效期（默认 1 小时）。
    pub fn new(
        chain: ChainKind,
        address: AddressId,
        connector: ConnectorKind,
        established_at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        let id = WalletSessionId::new(os_core::Uuid::new_v4().to_string());
        let expires_at = established_at + chrono::Duration::hours(1);
        Self {
            id,
            chain,
            address,
            connector,
            established_at,
            expires_at,
        }
    }

    /// 设置自定义过期时间（链式）。
    pub fn with_expiry(mut self, expires_at: chrono::DateTime<chrono::Utc>) -> Self {
        self.expires_at = expires_at;
        self
    }

    /// 是否在 `now` 时点仍有效（未过期）。
    pub fn is_active_at(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        now < self.expires_at
    }
}

impl SignRequest {
    /// 构造签名请求。
    pub fn new(
        session_id: WalletSessionId,
        message: impl Into<String>,
        algo: SignatureAlgorithm,
    ) -> Self {
        Self {
            session_id,
            message: message.into(),
            algorithm: algo,
        }
    }
}

// ============================================================================
// WalletConnector 骨架实现（WC v2 / 注入 / 二维码）
// ============================================================================
//
// 阻塞说明：真实的 WalletConnect v2（relay 配对、会话协商）依赖
// `walletconnect-relay` 等 crate，尚未在 workspace 注册。当前三个
// Connector 仅提供"会话内存态管理 + 签名请求路由"的骨架壳：connect 触发
// 配对流程占位（返回 Internal 错误），disconnect/list_sessions 可用。
// 真实 WC v2 协议接入留 TODO（见 BLOCKERS）。

/// 共享的会话内存表（被各 Connector 持有）。
#[derive(Debug, Default)]
pub struct SessionStore {
    sessions: std::sync::Mutex<std::collections::HashMap<WalletSessionId, WalletSession>>,
}

impl SessionStore {
    /// 构造空表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 插入/覆盖会话。
    pub fn put(&self, session: WalletSession) {
        self.sessions
            .lock()
            .expect("sessions poisoned")
            .insert(session.id.clone(), session);
    }

    /// 取会话。
    pub fn get(&self, id: &WalletSessionId) -> Option<WalletSession> {
        self.sessions
            .lock()
            .expect("sessions poisoned")
            .get(id)
            .cloned()
    }

    /// 移除会话，返回是否曾存在。
    pub fn remove(&self, id: &WalletSessionId) -> bool {
        self.sessions
            .lock()
            .expect("sessions poisoned")
            .remove(id)
            .is_some()
    }

    /// 列出所有会话（拷贝）。
    pub fn list(&self) -> Vec<WalletSession> {
        self.sessions
            .lock()
            .expect("sessions poisoned")
            .values()
            .cloned()
            .collect()
    }
}

/// WalletConnect v2 连接器骨架（relay 中转，跨设备配对）。
///
/// 真实 WC v2 协议（Relay 配对 + 会话协商 + DeepLink/二维码）依赖
/// `walletconnect-relay`，待注册后接入（TODO）。`wc_relay_url` 可切自托管。
pub struct WalletConnectV2Connector {
    sessions: SessionStore,
    /// relay URL（默认公共，可切自托管；规划文档 §9.1#13）。
    relay_url: String,
}

impl WalletConnectV2Connector {
    /// 构造，使用默认公共 relay。
    pub fn new() -> Self {
        Self::with_relay_url("wss://relay.walletconnect.com")
    }

    /// 指定自托管 relay URL 构造。
    pub fn with_relay_url(relay_url: impl Into<String>) -> Self {
        Self {
            sessions: SessionStore::new(),
            relay_url: relay_url.into(),
        }
    }

    /// 当前 relay URL。
    pub fn relay_url(&self) -> &str {
        &self.relay_url
    }
}

impl Default for WalletConnectV2Connector {
    fn default() -> Self {
        Self::new()
    }
}

// WalletConnector 是 async trait（ADR-COMPAT-001，被 Box<dyn> 用故加 #[async_trait]）。
#[async_trait]
impl WalletConnector for WalletConnectV2Connector {
    async fn connect(&self, chain: ChainKind, _kind: ConnectorKind) -> WalletResult<WalletSession> {
        // TODO(wallet-agent): 接入 WalletConnect v2 relay 配对流程
        // （依赖 walletconnect-relay，未注册）。当前返回 Internal 错误，
        // 真实配对成功后会构造 WalletSession 并 put 到 sessions。
        Err(crate::WalletError::Internal(format!(
            "WalletConnect v2 配对未接入（relay={}, 链 {}）；待 walletconnect-relay 注册",
            self.relay_url,
            chain.display_name()
        )))
    }

    async fn disconnect(&self, session: &WalletSessionId) -> WalletResult<()> {
        if self.sessions.remove(session) {
            Ok(())
        } else {
            Err(crate::WalletError::SessionNotFound(session.to_string()))
        }
    }

    async fn request_signature(&self, _req: SignRequest) -> WalletResult<SignResponse> {
        // TODO(wallet-agent): WC v2 签名请求需经 relay 转发到用户钱包。
        Err(crate::WalletError::Internal(
            "WalletConnect v2 签名请求未接入（待 walletconnect-relay 注册）".to_string(),
        ))
    }

    async fn list_sessions(&self) -> WalletResult<Vec<WalletSession>> {
        Ok(self.sessions.list())
    }
}

/// 注入式连接器骨架（浏览器扩展钱包，同源）。
///
/// 真实注入由前端 SDK 完成；本骨架在服务侧维护会话表，签名请求路由到
/// 桥接层（TODO：前端桥接协议接入）。
pub struct InjectedConnector {
    sessions: SessionStore,
}

impl InjectedConnector {
    /// 构造。
    pub fn new() -> Self {
        Self {
            sessions: SessionStore::new(),
        }
    }
}

impl Default for InjectedConnector {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl WalletConnector for InjectedConnector {
    async fn connect(
        &self,
        _chain: ChainKind,
        _kind: ConnectorKind,
    ) -> WalletResult<WalletSession> {
        // TODO(wallet-agent): 注入式连接需前端桥接上报地址；占位错误。
        Err(crate::WalletError::Internal(
            "注入式连接未接入（待前端桥接协议接入）".to_string(),
        ))
    }

    async fn disconnect(&self, session: &WalletSessionId) -> WalletResult<()> {
        if self.sessions.remove(session) {
            Ok(())
        } else {
            Err(crate::WalletError::SessionNotFound(session.to_string()))
        }
    }

    async fn request_signature(&self, _req: SignRequest) -> WalletResult<SignResponse> {
        Err(crate::WalletError::Internal(
            "注入式签名未接入（待前端桥接协议接入）".to_string(),
        ))
    }

    async fn list_sessions(&self) -> WalletResult<Vec<WalletSession>> {
        Ok(self.sessions.list())
    }
}

/// 二维码连接器骨架（手机钱包扫码连接）。
///
/// 生成配对 URI（TODO）+ 轮询钱包确认（TODO）。
pub struct QrCodeConnector {
    sessions: SessionStore,
}

impl QrCodeConnector {
    /// 构造。
    pub fn new() -> Self {
        Self {
            sessions: SessionStore::new(),
        }
    }
}

impl Default for QrCodeConnector {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl WalletConnector for QrCodeConnector {
    async fn connect(
        &self,
        _chain: ChainKind,
        _kind: ConnectorKind,
    ) -> WalletResult<WalletSession> {
        // TODO(wallet-agent): 二维码配对 URI 生成 + 钱包确认轮询。
        Err(crate::WalletError::Internal(
            "二维码连接未接入（待配对 URI 生成 + 确认轮询）".to_string(),
        ))
    }

    async fn disconnect(&self, session: &WalletSessionId) -> WalletResult<()> {
        if self.sessions.remove(session) {
            Ok(())
        } else {
            Err(crate::WalletError::SessionNotFound(session.to_string()))
        }
    }

    async fn request_signature(&self, _req: SignRequest) -> WalletResult<SignResponse> {
        Err(crate::WalletError::Internal(
            "二维码签名未接入（待配对 URI 生成 + 确认轮询）".to_string(),
        ))
    }

    async fn list_sessions(&self) -> WalletResult<Vec<WalletSession>> {
        Ok(self.sessions.list())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use os_core::AddressId;

    #[test]
    fn session_lifecycle_default_expiry() {
        let addr = AddressId::new("0xabc");
        let now = chrono::Utc::now();
        let s = WalletSession::new(
            ChainKind::Evm,
            addr.clone(),
            ConnectorKind::WalletConnectV2,
            now,
        );
        assert!(s.is_active_at(now));
        assert!(!s.is_active_at(s.expires_at));
        assert_eq!(s.connector, ConnectorKind::WalletConnectV2);
        assert_eq!(s.address, addr);
    }

    #[test]
    fn session_store_crud() {
        let store = SessionStore::new();
        let addr = AddressId::new("bc1q...");
        let s = WalletSession::new(
            ChainKind::Bitcoin,
            addr,
            ConnectorKind::QrCode,
            chrono::Utc::now(),
        );
        let id = s.id.clone();
        store.put(s.clone());
        assert_eq!(store.get(&id).map(|s| s.id), Some(id.clone()));
        assert_eq!(store.list().len(), 1);
        assert!(store.remove(&id));
        assert!(!store.remove(&id));
        assert!(store.get(&id).is_none());
    }

    #[tokio::test]
    async fn injected_disconnect_missing_session_errors() {
        let c = InjectedConnector::new();
        let err = c
            .disconnect(&WalletSessionId::new("nope"))
            .await
            .unwrap_err();
        assert!(matches!(err, crate::WalletError::SessionNotFound(_)));
    }

    #[tokio::test]
    async fn wc_relay_url_configurable() {
        let c = WalletConnectV2Connector::with_relay_url("wss://self.hosted/relay");
        assert_eq!(c.relay_url(), "wss://self.hosted/relay");
        // connect 在真实 WC v2 接入前返回 Internal（不 panic）。
        let err = c
            .connect(ChainKind::Evm, ConnectorKind::WalletConnectV2)
            .await
            .unwrap_err();
        assert!(matches!(err, crate::WalletError::Internal(_)));
        // list_sessions 可用（空表）。
        assert!(c.list_sessions().await.unwrap().is_empty());
    }

    // =========================================================================
    // 覆盖率补充：构造 / serde / 各 Connector trait 路径 / Default impl
    // =========================================================================

    #[test]
    fn connector_kind_serde_snake_case() {
        let cases = [
            (ConnectorKind::WalletConnectV2, "wallet_connect_v2"),
            (ConnectorKind::Injected, "injected"),
            (ConnectorKind::QrCode, "qr_code"),
        ];
        for (k, expected) in cases {
            assert_eq!(
                serde_json::to_string(&k).unwrap(),
                format!("\"{expected}\"")
            );
            let back: ConnectorKind = serde_json::from_str(&format!("\"{expected}\"")).unwrap();
            assert_eq!(back, k);
        }
        assert!(serde_json::from_str::<ConnectorKind>("\"unknown\"").is_err());
    }

    #[test]
    fn connector_kind_equality_copy() {
        // ConnectorKind 派生 Copy + PartialEq + Eq。
        let a = ConnectorKind::WalletConnectV2;
        let b = a; // Copy
        assert_eq!(a, b);
        assert_eq!(ConnectorKind::Injected, ConnectorKind::Injected);
        assert_ne!(ConnectorKind::WalletConnectV2, ConnectorKind::Injected);
    }

    #[test]
    fn wallet_session_with_expiry_overrides_default() {
        let now = chrono::Utc::now();
        let custom_expiry = now + chrono::Duration::hours(2);
        let s = WalletSession::new(
            ChainKind::Bitcoin,
            AddressId::new("bc1q"),
            ConnectorKind::QrCode,
            now,
        )
        .with_expiry(custom_expiry);
        assert_eq!(s.expires_at, custom_expiry);
        // 在 now 时仍有效。
        assert!(s.is_active_at(now));
        // 在过期时间点已无效。
        assert!(!s.is_active_at(custom_expiry));
        // 在过期后无效。
        assert!(!s.is_active_at(custom_expiry + chrono::Duration::seconds(1)));
    }

    #[test]
    fn wallet_session_new_generates_unique_ids() {
        let now = chrono::Utc::now();
        let s1 = WalletSession::new(
            ChainKind::Evm,
            AddressId::new("0x1"),
            ConnectorKind::WalletConnectV2,
            now,
        );
        let s2 = WalletSession::new(
            ChainKind::Evm,
            AddressId::new("0x1"),
            ConnectorKind::WalletConnectV2,
            now,
        );
        // UUID v4 保证唯一性。
        assert_ne!(s1.id, s2.id);
        // 默认过期时间 = established_at + 1 小时。
        assert_eq!(s2.expires_at, now + chrono::Duration::hours(1));
    }

    #[test]
    fn wallet_session_serde_roundtrip() {
        let now = chrono::Utc::now();
        let s = WalletSession::new(
            ChainKind::Evm,
            AddressId::new("0xabc"),
            ConnectorKind::Injected,
            now,
        );
        let json = serde_json::to_string(&s).unwrap();
        let back: WalletSession = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, s.id);
        assert_eq!(back.chain, s.chain);
        assert_eq!(back.address, s.address);
        assert_eq!(back.connector, s.connector);
        assert_eq!(back.established_at, s.established_at);
        assert_eq!(back.expires_at, s.expires_at);
    }

    #[test]
    fn sign_request_new_and_serde() {
        let sid = WalletSessionId::new("sess-1");
        let req = SignRequest::new(sid.clone(), "hello world", SignatureAlgorithm::Eip191);
        assert_eq!(req.session_id, sid);
        assert_eq!(req.message, "hello world");
        assert_eq!(req.algorithm, SignatureAlgorithm::Eip191);

        let json = serde_json::to_string(&req).unwrap();
        let back: SignRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.session_id, req.session_id);
        assert_eq!(back.message, req.message);
        assert_eq!(back.algorithm, req.algorithm);
    }

    #[test]
    fn sign_request_new_accepts_string_message() {
        let sid = WalletSessionId::new("s");
        let req = SignRequest::new(
            sid,
            String::from("owned message"),
            SignatureAlgorithm::Eip712,
        );
        assert_eq!(req.message, "owned message");
    }

    #[test]
    fn sign_response_serde_roundtrip() {
        let r = SignResponse {
            signature: vec![0xde, 0xad, 0xbe, 0xef],
            address: AddressId::new("0xfeed"),
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: SignResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.signature, r.signature);
        assert_eq!(back.address, r.address);
    }

    #[test]
    fn session_store_default_is_empty() {
        let s = SessionStore::default();
        assert!(s.list().is_empty());
        assert!(s.get(&WalletSessionId::new("nope")).is_none());
        assert!(!s.remove(&WalletSessionId::new("nope")));
    }

    #[test]
    fn session_store_put_overwrites_same_id() {
        let store = SessionStore::new();
        let now = chrono::Utc::now();
        let s1 = WalletSession::new(
            ChainKind::Evm,
            AddressId::new("0x1"),
            ConnectorKind::WalletConnectV2,
            now,
        );
        let id = s1.id.clone();
        store.put(s1);
        // 同 id 不同地址覆盖。
        let s2 = WalletSession {
            id: id.clone(),
            chain: ChainKind::Evm,
            address: AddressId::new("0x2"),
            connector: ConnectorKind::WalletConnectV2,
            established_at: now,
            expires_at: now + chrono::Duration::hours(1),
        };
        store.put(s2);
        assert_eq!(store.list().len(), 1);
        assert_eq!(
            store.get(&id).map(|s| s.address.as_str().to_string()),
            Some("0x2".into())
        );
    }

    #[tokio::test]
    async fn wallet_connect_v2_default_uses_public_relay() {
        let c = WalletConnectV2Connector::default();
        assert_eq!(c.relay_url(), "wss://relay.walletconnect.com");
    }

    #[tokio::test]
    async fn wallet_connect_v2_disconnect_existing_session_succeeds() {
        // 预置一个会话到 store，验证 disconnect 成功路径。
        let c = WalletConnectV2Connector::new();
        let now = chrono::Utc::now();
        let s = WalletSession::new(
            ChainKind::Evm,
            AddressId::new("0x1"),
            ConnectorKind::WalletConnectV2,
            now,
        );
        c.sessions.put(s.clone());
        // 注：sessions 是 pub 字段（仅在 crate 内），但通过 connect 路径无法预置
        // （connect 总是返回 Err），故直接用 put 验证 disconnect 成功路径。
        c.disconnect(&s.id).await.unwrap();
        // 再次 disconnect 已不存在 -> Err。
        let err = c.disconnect(&s.id).await.unwrap_err();
        assert!(matches!(err, crate::WalletError::SessionNotFound(_)));
    }

    #[tokio::test]
    async fn wallet_connect_v2_request_signature_returns_internal() {
        let c = WalletConnectV2Connector::new();
        let err = c
            .request_signature(SignRequest::new(
                WalletSessionId::new("x"),
                "m",
                SignatureAlgorithm::Eip191,
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, crate::WalletError::Internal(_)));
    }

    #[tokio::test]
    async fn injected_connector_full_lifecycle() {
        let c = InjectedConnector::new();
        // connect 占位返回 Internal。
        let err = c
            .connect(ChainKind::Evm, ConnectorKind::Injected)
            .await
            .unwrap_err();
        assert!(matches!(err, crate::WalletError::Internal(_)));
        // request_signature 占位返回 Internal。
        let err = c
            .request_signature(SignRequest::new(
                WalletSessionId::new("x"),
                "m",
                SignatureAlgorithm::Eip191,
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, crate::WalletError::Internal(_)));
        // list_sessions 空表。
        assert!(c.list_sessions().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn injected_connector_default() {
        let c = InjectedConnector::default();
        assert!(c.list_sessions().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn qr_code_connector_full_lifecycle() {
        let c = QrCodeConnector::new();
        // connect 占位返回 Internal。
        let err = c
            .connect(ChainKind::Bitcoin, ConnectorKind::QrCode)
            .await
            .unwrap_err();
        assert!(matches!(err, crate::WalletError::Internal(_)));
        // disconnect 不存在的会话 -> Err。
        let err = c
            .disconnect(&WalletSessionId::new("nope"))
            .await
            .unwrap_err();
        assert!(matches!(err, crate::WalletError::SessionNotFound(_)));
        // request_signature 占位返回 Internal。
        let err = c
            .request_signature(SignRequest::new(
                WalletSessionId::new("x"),
                "m",
                SignatureAlgorithm::Schnorr,
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, crate::WalletError::Internal(_)));
        // list_sessions 空表。
        assert!(c.list_sessions().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn qr_code_connector_default() {
        let c = QrCodeConnector::default();
        assert!(c.list_sessions().await.unwrap().is_empty());
    }
}
