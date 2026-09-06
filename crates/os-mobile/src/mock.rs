//! Mock 实现（feature gate `mock`）——供前端/UI 层测试用。
//!
//! 约定（_conventions.md §5）：实现完整 trait（不 panic 的默认返回），
//! 提供 builder 构造器，纯内存、确定性。
//!
//! 提供：`MockOsClient` / `MockPushSubscriber` / `MockPushCallback`。

#![cfg(feature = "mock")]

use std::sync::Mutex;

use os_core::Health;
use os_discover::PeerNode;

use crate::client::{ClientSession, OsClient, SystemStatus};
use crate::push::{PushCallback, PushNotification, PushSubscriber};
use crate::MobileError;

// ============================================================
// MockOsClient
// ============================================================

/// Mock `OsClient`——可配置系统状态与发现的节点，会话状态机完整。
pub struct MockOsClient {
    session: Mutex<Option<ClientSession>>,
    status: Mutex<SystemStatus>,
    nodes: Mutex<Vec<PeerNode>>,
    connect_fail: Mutex<bool>,
}

impl MockOsClient {
    /// 默认构造（已连接状态占位、空节点列表）。
    pub fn new() -> Self {
        Self {
            session: Mutex::new(None),
            status: Mutex::new(SystemStatus {
                hostname: "mock-os".to_string(),
                version: "0.0.0".to_string(),
                capacity: os_core::Capacity {
                    used_bytes: 0,
                    total_bytes: 0,
                },
                health: Health::Healthy,
                node_count: 1,
            }),
            nodes: Mutex::new(Vec::new()),
            connect_fail: Mutex::new(false),
        }
    }

    /// 注入 get_system_status 返回值。
    pub fn with_status(self, status: SystemStatus) -> Self {
        *self.status.lock().unwrap() = status;
        self
    }

    /// 注入 discover_nodes 返回值。
    pub fn with_nodes(self, nodes: Vec<PeerNode>) -> Self {
        *self.nodes.lock().unwrap() = nodes;
        self
    }

    /// 设置 connect 是否失败（测试错误路径）。
    pub fn set_connect_fail(&self, fail: bool) {
        *self.connect_fail.lock().unwrap() = fail;
    }

    /// 是否已连接。
    pub fn is_connected(&self) -> bool {
        self.session.lock().unwrap().is_some()
    }
}

impl Default for MockOsClient {
    fn default() -> Self {
        Self::new()
    }
}

impl OsClient for MockOsClient {
    async fn connect(
        &self,
        endpoint: &str,
        token: Option<&str>,
    ) -> Result<ClientSession, MobileError> {
        if *self.connect_fail.lock().unwrap() {
            return Err(MobileError::EndpointUnreachable(
                "mock connect 失败".to_string(),
            ));
        }
        let session = ClientSession {
            endpoint: endpoint.to_string(),
            token: token.unwrap_or("anonymous").to_string(),
            user: token
                .map(|_| "authed".to_string())
                .unwrap_or_else(|| "anonymous".to_string()),
            expires_at: chrono::Utc::now() + chrono::Duration::hours(24),
        };
        *self.session.lock().unwrap() = Some(session.clone());
        Ok(session)
    }

    async fn disconnect(&self) -> Result<(), MobileError> {
        let mut slot = self.session.lock().unwrap();
        if slot.is_none() {
            return Err(MobileError::NotConnected);
        }
        *slot = None;
        Ok(())
    }

    async fn get_system_status(&self) -> Result<SystemStatus, MobileError> {
        if self.session.lock().unwrap().is_none() {
            return Err(MobileError::NotConnected);
        }
        Ok(self.status.lock().unwrap().clone())
    }

    async fn discover_nodes(&self) -> Result<Vec<PeerNode>, MobileError> {
        if self.session.lock().unwrap().is_none() {
            return Err(MobileError::NotConnected);
        }
        Ok(self.nodes.lock().unwrap().clone())
    }

    async fn pair(&self, endpoint: &str, pairing_code: &str) -> Result<ClientSession, MobileError> {
        let session = ClientSession {
            endpoint: endpoint.to_string(),
            token: format!("mock-paired:{}", pairing_code),
            user: "paired".to_string(),
            expires_at: chrono::Utc::now() + chrono::Duration::hours(24),
        };
        *self.session.lock().unwrap() = Some(session.clone());
        Ok(session)
    }
}

// ============================================================
// MockPushSubscriber
// ============================================================

/// Mock `PushSubscriber`——包装 [`crate::push::InMemoryPushSubscriber`]，
/// 便于前端注入测试。
pub struct MockPushSubscriber {
    inner: crate::push::InMemoryPushSubscriber,
}

impl MockPushSubscriber {
    /// 默认构造。
    pub fn new() -> Self {
        Self {
            inner: crate::push::InMemoryPushSubscriber::new(),
        }
    }

    /// 当前订阅状态。
    pub fn state(&self) -> crate::push::PushSubscriptionState {
        self.inner.state()
    }

    /// 缓存队列长度。
    pub fn queued_count(&self) -> usize {
        self.inner.queued_count()
    }

    /// 投递一条通知（桥接到回调）。
    pub async fn deliver(&self, n: PushNotification) {
        self.inner.deliver(n).await;
    }
}

impl Default for MockPushSubscriber {
    fn default() -> Self {
        Self::new()
    }
}

impl PushSubscriber for MockPushSubscriber {
    async fn subscribe(&self, callback: Box<dyn PushCallback>) -> Result<(), MobileError> {
        self.inner.subscribe(callback).await
    }
    async fn unsubscribe(&self) -> Result<(), MobileError> {
        self.inner.unsubscribe().await
    }
}

// ============================================================
// MockPushCallback
// ============================================================

/// Mock `PushCallback`——收集收到的通知，便于断言。
pub struct MockPushCallback {
    received: Mutex<Vec<PushNotification>>,
}

impl MockPushCallback {
    /// 构造空收集器。
    pub fn new() -> Self {
        Self {
            received: Mutex::new(Vec::new()),
        }
    }

    /// 取已收到的通知（克隆）。
    pub fn received(&self) -> Vec<PushNotification> {
        self.received.lock().unwrap().clone()
    }

    /// 已收到数量。
    pub fn count(&self) -> usize {
        self.received.lock().unwrap().len()
    }
}

impl Default for MockPushCallback {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl PushCallback for MockPushCallback {
    async fn on_notification(&self, n: &PushNotification) {
        self.received.lock().unwrap().push(n.clone());
    }
}

// ============================================================
// 单元测
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::push::PushSeverity;

    #[tokio::test]
    async fn mock_os_client_connect_status_disconnect() {
        let c = MockOsClient::new();
        assert!(!c.is_connected());
        c.connect("https://os", Some("tok")).await.unwrap();
        assert!(c.is_connected());
        let st = c.get_system_status().await.unwrap();
        assert_eq!(st.hostname, "mock-os");
        c.disconnect().await.unwrap();
        assert!(!c.is_connected());
        // 断开后查状态 → NotConnected
        assert!(matches!(
            c.get_system_status().await,
            Err(MobileError::NotConnected)
        ));
    }

    #[tokio::test]
    async fn mock_os_client_connect_fail() {
        let c = MockOsClient::new();
        c.set_connect_fail(true);
        assert!(c.connect("https://os", None).await.is_err());
    }

    #[tokio::test]
    async fn mock_os_client_pair() {
        let c = MockOsClient::new();
        let s = c.pair("https://os", "CODE").await.unwrap();
        assert!(s.token.contains("CODE"));
        assert!(c.is_connected());
    }

    #[tokio::test]
    async fn mock_push_subscriber_deliver_to_callback() {
        let sub = MockPushSubscriber::new();
        let cb = std::sync::Arc::new(MockPushCallback::new());
        // 用代理把 Arc 包成 Box<dyn PushCallback>
        struct Proxy(std::sync::Arc<MockPushCallback>);
        #[async_trait::async_trait]
        impl PushCallback for Proxy {
            async fn on_notification(&self, n: &PushNotification) {
                self.0.on_notification(n).await;
            }
        }
        sub.subscribe(Box::new(Proxy(cb.clone()))).await.unwrap();
        sub.deliver(note("hello")).await;
        assert_eq!(cb.count(), 1);
        assert_eq!(cb.received()[0].title, "hello");
    }

    fn note(title: &str) -> PushNotification {
        PushNotification {
            title: title.into(),
            body: "b".into(),
            severity: PushSeverity::Info,
            action_url: None,
            data: serde_json::Value::Null,
        }
    }
}
