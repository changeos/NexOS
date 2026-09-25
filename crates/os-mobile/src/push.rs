//! 推送通知（规划文档 §3.15）
//!
//! 模型：`PushSubscriber` 持有一个 `PushCallback`；OS 端有事件（告警/任务完成/分享）
//! 时通过 FCM/APNs/长连接下发，桥接到 `on_notification`。

use async_trait::async_trait;
use os_core::{Deserialize, Serialize};

use crate::MobileError;

// ----------------------------------------------------------------------------
// 通知载荷 / 严重程度
// ----------------------------------------------------------------------------

/// 推送通知严重程度
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PushSeverity {
    Info,
    Warning,
    Critical,
}

/// 推送通知
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushNotification {
    /// 标题
    pub title: String,
    /// 正文
    pub body: String,
    /// 严重程度
    pub severity: PushSeverity,
    /// 点击跳转的 App 内 URL（None = 不跳转）
    pub action_url: Option<String>,
    /// 附加数据（开放结构，如告警详情/任务 ID）
    pub data: serde_json::Value,
}

// ----------------------------------------------------------------------------
// 订阅状态 + 通知队列（纯逻辑，可测）
// ----------------------------------------------------------------------------

/// 推送订阅状态机。
///
/// 初始为 `Unsubscribed`；`subscribe` 成功后转入 `Subscribed`（持有当前注册的回调标识，
/// 便于日志/调试；回调本身为 `Box<dyn PushCallback>`，存在订阅者侧而非状态枚举里，
/// 避免把 trait object 塞进 `Clone` 派生）。
///
/// 状态转换：`Unsubscribed → Subscribed`（subscribe）→ `Unsubscribed`（unsubscribe）。
/// 重复 subscribe / unsubscribe 视为幂等错误（见 [`PushSubscriptionState::subscribe`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PushSubscriptionState {
    /// 未订阅（初始 / unsubscribe 后）
    #[default]
    Unsubscribed,
    /// 已订阅
    Subscribed,
}

impl PushSubscriptionState {
    /// 是否处于已订阅状态。
    #[must_use]
    pub fn is_subscribed(self) -> bool {
        matches!(self, Self::Subscribed)
    }

    /// 转入「已订阅」。若已是 Subscribed，返回 `Err`（重复订阅）——幂等保护。
    pub fn subscribe(self) -> Result<Self, MobileError> {
        match self {
            Self::Unsubscribed => Ok(Self::Subscribed),
            Self::Subscribed => Err(MobileError::PushFailed("已处于订阅状态".into())),
        }
    }

    /// 转入「未订阅」。若已是 Unsubscribed，返回 `Err`（重复取消）——幂等保护。
    pub fn unsubscribe(self) -> Result<Self, MobileError> {
        match self {
            Self::Subscribed => Ok(Self::Unsubscribed),
            Self::Unsubscribed => Err(MobileError::PushFailed("未处于订阅状态".into())),
        }
    }
}

/// 推送通知队列——在订阅前到达或回调处理慢时缓存通知。
///
/// 设计：FCM/APNs/长连接推送可能在 `subscribe` 完成前到达，或回调处理速度跟不上
/// 下发速度。队列在「订阅前」缓存（订阅后 flush），在「已订阅」时也可作为有界缓冲，
/// 防止慢回调拖垮推送通道。
///
/// 容量策略：有界（默认 [`NotificationQueue::DEFAULT_CAPACITY`]）；溢出时**丢弃最旧**
/// 通知（FIFO 溢出），保留最新——告警类推送「最新优先」语义优于「最早优先」。
#[derive(Debug, Clone)]
pub struct NotificationQueue {
    capacity: usize,
    buf: std::collections::VecDeque<PushNotification>,
}

impl NotificationQueue {
    /// 默认容量（512 条）。
    pub const DEFAULT_CAPACITY: usize = 512;

    /// 用指定容量构造空队列。
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            buf: std::collections::VecDeque::with_capacity(capacity.max(1)),
        }
    }

    /// 当前队列长度。
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// 入队一条通知；若已满，丢弃最旧的一条（返回被丢弃项，供上层日志）。
    ///
    /// 返回 `Some(dropped)` 表示发生了溢出丢弃；`None` 表示正常入队未溢出。
    pub fn push(&mut self, n: PushNotification) -> Option<PushNotification> {
        let dropped = if self.buf.len() >= self.capacity {
            self.buf.pop_front()
        } else {
            None
        };
        self.buf.push_back(n);
        dropped
    }

    /// 出队最旧的一条（FIFO）。
    #[must_use]
    pub fn pop(&mut self) -> Option<PushNotification> {
        self.buf.pop_front()
    }

    /// 取出队列头部（最旧）的引用，不移除。
    #[must_use]
    pub fn peek(&self) -> Option<&PushNotification> {
        self.buf.front()
    }

    /// 清空队列，返回所有已缓存通知（按 FIFO 顺序）——供 `subscribe` 成功后 flush。
    pub fn drain(&mut self) -> Vec<PushNotification> {
        self.buf.drain(..).collect()
    }
}

impl Default for NotificationQueue {
    fn default() -> Self {
        Self::new(Self::DEFAULT_CAPACITY)
    }
}

// ----------------------------------------------------------------------------
// PushSubscriber / PushCallback（async）
// ----------------------------------------------------------------------------

/// 推送订阅者——注册/注销回调。
#[allow(async_fn_in_trait)]
pub trait PushSubscriber: Send + Sync {
    /// 订阅推送，注册回调。
    async fn subscribe(&self, callback: Box<dyn PushCallback>) -> Result<(), MobileError>;

    /// 取消订阅。
    async fn unsubscribe(&self) -> Result<(), MobileError>;
}

/// 推送回调——收到通知时调用。
///
/// 经 `Box<dyn PushCallback>` 注册到 PushSubscriber，故用 `#[async_trait]` 保证 dyn 兼容（ADR-COMPAT-001）。
#[async_trait]
pub trait PushCallback: Send + Sync {
    /// 收到一条推送通知。
    async fn on_notification(&self, n: &PushNotification);
}

// ----------------------------------------------------------------------------
// InMemoryPushSubscriber（默认实现，桥接 FCM/APNs/长连接 → PushCallback）
// ----------------------------------------------------------------------------

/// 内存推送订阅者——持有当前回调与状态，桥接平台推送通道到 `PushCallback`。
///
/// 设计：FCM（Android）/ APNs（iOS）/ 长连接（桌面）在平台层收到推送后，调
/// `deliver` 把通知投递给本订阅者；本订阅者再调已注册的 `PushCallback::on_notification`。
/// 订阅前到达的通知进 `NotificationQueue` 缓存，`subscribe` 成功后 flush 给回调。
///
/// 线程安全：内部用 `Mutex`；callback 以 `Arc<dyn PushCallback>` 持有，使 `deliver`
/// 可在**不持锁**的情况下跨 await 调用回调（避免 MutexGuard 非 Send 跨 await）。
pub struct InMemoryPushSubscriber {
    inner: std::sync::Mutex<SubscriberInner>,
}

struct SubscriberInner {
    state: PushSubscriptionState,
    callback: Option<std::sync::Arc<dyn PushCallback>>,
    queue: NotificationQueue,
}

/// deliver 的内部决策结果（锁外执行，避免持锁跨 await）。
enum DeliverAction {
    /// 已订阅：flush 缓存 + 投递本次 n
    Deliver {
        cb: Option<std::sync::Arc<dyn PushCallback>>,
        queued: Vec<PushNotification>,
        n: PushNotification,
    },
    /// 未订阅：已入队缓存
    Queued,
}

impl InMemoryPushSubscriber {
    /// 构造（默认队列容量 512）。
    pub fn new() -> Self {
        Self::with_queue_capacity(NotificationQueue::DEFAULT_CAPACITY)
    }

    /// 用指定队列容量构造。
    pub fn with_queue_capacity(capacity: usize) -> Self {
        Self {
            inner: std::sync::Mutex::new(SubscriberInner {
                state: PushSubscriptionState::default(),
                callback: None,
                queue: NotificationQueue::new(capacity),
            }),
        }
    }

    /// 当前订阅状态。
    pub fn state(&self) -> PushSubscriptionState {
        self.inner.lock().unwrap().state
    }

    /// 当前队列长度（订阅前缓存的通知数）。
    pub fn queued_count(&self) -> usize {
        self.inner.lock().unwrap().queue.len()
    }

    /// 平台推送通道调此方法投递一条通知。
    ///
    /// - 已订阅：立即调 `callback.on_notification`（并 flush 任何此前缓存的）。
    /// - 未订阅：入队列缓存，等 `subscribe` 后 flush。
    pub async fn deliver(&self, n: PushNotification) {
        // 锁内：决定走「已订阅」还是「入队」。已订阅分支取 callback + flush 缓存；
        // 未订阅分支把 n 入队后返回（不调回调）。
        let action = {
            let mut inner = self.inner.lock().unwrap();
            if inner.state.is_subscribed() {
                let cb = inner.callback.clone();
                let queued = inner.queue.drain();
                DeliverAction::Deliver { cb, queued, n }
            } else {
                inner.queue.push(n);
                DeliverAction::Queued
            }
        };
        match action {
            DeliverAction::Deliver { cb, mut queued, n } => {
                if let Some(cb) = cb {
                    // 不持锁：先 flush 缓存，再投递本次 n
                    for item in queued.drain(..) {
                        cb.on_notification(&item).await;
                    }
                    cb.on_notification(&n).await;
                }
            }
            DeliverAction::Queued => {}
        }
    }
}

impl Default for InMemoryPushSubscriber {
    fn default() -> Self {
        Self::new()
    }
}

// PushSubscriber trait 为原生 async（非 #[async_trait]），故 impl 用原生 async fn。
impl PushSubscriber for InMemoryPushSubscriber {
    async fn subscribe(&self, callback: Box<dyn PushCallback>) -> Result<(), MobileError> {
        let mut inner = self.inner.lock().unwrap();
        inner.state = inner.state.subscribe()?;
        // Box → Arc：使 deliver 可克隆引用、不持锁跨 await 调用
        inner.callback = Some(std::sync::Arc::from(callback));
        Ok(())
    }

    async fn unsubscribe(&self) -> Result<(), MobileError> {
        let mut inner = self.inner.lock().unwrap();
        inner.state = inner.state.unsubscribe()?;
        inner.callback = None;
        Ok(())
    }
}

// ----------------------------------------------------------------------------
// 单元测试（订阅状态机 + 通知队列纯逻辑）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn note(title: &str) -> PushNotification {
        PushNotification {
            title: title.into(),
            body: "body".into(),
            severity: PushSeverity::Info,
            action_url: None,
            data: serde_json::Value::Null,
        }
    }

    // —— PushSubscriptionState ——

    #[test]
    fn subscription_state_default_is_unsubscribed() {
        assert_eq!(
            PushSubscriptionState::default(),
            PushSubscriptionState::Unsubscribed
        );
        assert!(!PushSubscriptionState::Unsubscribed.is_subscribed());
        assert!(PushSubscriptionState::Subscribed.is_subscribed());
    }

    #[test]
    fn subscription_state_transitions() {
        let s = PushSubscriptionState::Unsubscribed;
        let s = s.subscribe().unwrap();
        assert_eq!(s, PushSubscriptionState::Subscribed);
        let s = s.unsubscribe().unwrap();
        assert_eq!(s, PushSubscriptionState::Unsubscribed);
    }

    #[test]
    fn subscription_state_double_subscribe_is_error() {
        let s = PushSubscriptionState::Subscribed;
        assert!(s.subscribe().is_err());
    }

    #[test]
    fn subscription_state_double_unsubscribe_is_error() {
        let s = PushSubscriptionState::Unsubscribed;
        assert!(s.unsubscribe().is_err());
    }

    // —— NotificationQueue ——

    #[test]
    fn queue_default_capacity() {
        assert_eq!(NotificationQueue::DEFAULT_CAPACITY, 512);
        let q = NotificationQueue::default();
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn queue_push_pop_fifo() {
        let mut q = NotificationQueue::new(3);
        q.push(note("a"));
        q.push(note("b"));
        q.push(note("c"));
        assert_eq!(q.len(), 3);
        // FIFO：pop 出最旧
        assert_eq!(q.pop().unwrap().title, "a");
        assert_eq!(q.pop().unwrap().title, "b");
        assert_eq!(q.pop().unwrap().title, "c");
        assert!(q.pop().is_none());
    }

    #[test]
    fn queue_peek_does_not_remove() {
        let mut q = NotificationQueue::new(3);
        q.push(note("a"));
        assert_eq!(q.peek().unwrap().title, "a");
        assert_eq!(q.len(), 1); // 仍在
    }

    #[test]
    fn queue_overflow_drops_oldest() {
        let mut q = NotificationQueue::new(2);
        q.push(note("a"));
        q.push(note("b"));
        // 满：入 c，丢弃最旧的 a
        let dropped = q.push(note("c"));
        assert_eq!(dropped.unwrap().title, "a");
        assert_eq!(q.len(), 2);
        // 队列现在是 b, c
        assert_eq!(q.pop().unwrap().title, "b");
        assert_eq!(q.pop().unwrap().title, "c");
    }

    #[test]
    fn queue_drain_clears_and_returns_fifo() {
        let mut q = NotificationQueue::new(5);
        q.push(note("a"));
        q.push(note("b"));
        let drained = q.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].title, "a");
        assert_eq!(drained[1].title, "b");
        assert!(q.is_empty());
    }

    #[test]
    fn queue_capacity_min_one() {
        // 容量 0 被钳制为 1
        let mut q = NotificationQueue::new(0);
        assert_eq!(q.len(), 0);
        q.push(note("a"));
        // 满了（cap=1）：入 b 丢弃 a
        let dropped = q.push(note("b"));
        assert_eq!(dropped.unwrap().title, "a");
        assert_eq!(q.pop().unwrap().title, "b");
    }

    #[test]
    fn queue_clone_is_independent() {
        let mut q = NotificationQueue::new(3);
        q.push(note("a"));
        let mut q2 = q.clone();
        q2.push(note("b"));
        assert_eq!(q.len(), 1);
        assert_eq!(q2.len(), 2);
    }

    // —— InMemoryPushSubscriber（订阅状态机 + 队列桥接）——

    /// 记录回调：把收到的通知标题收集到 Mutex<Vec>。
    struct RecordCallback {
        received: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl PushCallback for RecordCallback {
        async fn on_notification(&self, n: &PushNotification) {
            self.received.lock().unwrap().push(n.title.clone());
        }
    }

    #[tokio::test]
    async fn deliver_before_subscribe_is_queued() {
        let sub = InMemoryPushSubscriber::new();
        assert_eq!(sub.state(), PushSubscriptionState::Unsubscribed);
        // 未订阅：投递 2 条进队列
        sub.deliver(note("a")).await;
        sub.deliver(note("b")).await;
        assert_eq!(sub.queued_count(), 2);
    }

    #[tokio::test]
    async fn subscribe_then_deliver_invokes_callback() {
        let sub = InMemoryPushSubscriber::new();
        let cb = std::sync::Arc::new(RecordCallback {
            received: std::sync::Mutex::new(Vec::new()),
        });
        sub.subscribe(Box::new(RecordCallbackProxy(cb.clone())))
            .await
            .unwrap();
        assert_eq!(sub.state(), PushSubscriptionState::Subscribed);
        // 已订阅：投递立即回调
        sub.deliver(note("x")).await;
        assert_eq!(cb.received.lock().unwrap().clone(), vec!["x".to_string()]);
    }

    #[tokio::test]
    async fn queued_flushed_on_next_deliver_after_subscribe() {
        let sub = InMemoryPushSubscriber::new();
        // 订阅前缓存 2 条
        sub.deliver(note("a")).await;
        sub.deliver(note("b")).await;
        let cb = std::sync::Arc::new(RecordCallback {
            received: std::sync::Mutex::new(Vec::new()),
        });
        sub.subscribe(Box::new(RecordCallbackProxy(cb.clone())))
            .await
            .unwrap();
        // 订阅后投递 1 条：应 flush 缓存(a,b) + 本次(c)
        sub.deliver(note("c")).await;
        let got = cb.received.lock().unwrap().clone();
        assert_eq!(got, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
    }

    #[tokio::test]
    async fn unsubscribe_clears_callback() {
        let sub = InMemoryPushSubscriber::new();
        let cb = std::sync::Arc::new(RecordCallback {
            received: std::sync::Mutex::new(Vec::new()),
        });
        sub.subscribe(Box::new(RecordCallbackProxy(cb.clone())))
            .await
            .unwrap();
        sub.unsubscribe().await.unwrap();
        assert_eq!(sub.state(), PushSubscriptionState::Unsubscribed);
        // 取消订阅后投递进队列，不回调
        sub.deliver(note("z")).await;
        assert!(cb.received.lock().unwrap().is_empty());
        assert_eq!(sub.queued_count(), 1);
    }

    /// 代理：把 Arc<RecordCallback> 包成 PushCallback（Box<dyn> 需要 'static + Send）。
    struct RecordCallbackProxy(std::sync::Arc<RecordCallback>);

    #[async_trait]
    impl PushCallback for RecordCallbackProxy {
        async fn on_notification(&self, n: &PushNotification) {
            self.0.on_notification(n).await;
        }
    }

    // —— 扩展边界（覆盖率补测：model + serde 往返）——

    #[test]
    fn push_severity_serde_roundtrip() {
        // 全变体 serde 往返：snake_case 序列化 + 反序列化
        for (sev, snake) in [
            (PushSeverity::Info, "info"),
            (PushSeverity::Warning, "warning"),
            (PushSeverity::Critical, "critical"),
        ] {
            let s = serde_json::to_string(&sev).unwrap();
            assert_eq!(s, format!("\"{snake}\""));
            let back: PushSeverity = serde_json::from_str(&s).unwrap();
            assert_eq!(back, sev);
        }
    }

    #[test]
    fn push_severity_serde_rejects_invalid() {
        let r: Result<PushSeverity, _> = serde_json::from_str("\"not_a_severity\"");
        assert!(r.is_err());
    }

    #[test]
    fn push_severity_equality_and_copy() {
        // Copy + PartialEq + Eq + Debug 派生
        let s1 = PushSeverity::Critical;
        let s2 = s1; // Copy
        assert_eq!(s1, s2);
        assert_ne!(PushSeverity::Info, PushSeverity::Warning);
        let _dbg = format!("{:?}", s1);
    }

    #[test]
    fn push_notification_serde_full_roundtrip() {
        let n = PushNotification {
            title: "告警".into(),
            body: "磁盘满".into(),
            severity: PushSeverity::Critical,
            action_url: Some("os://alert/1".into()),
            data: serde_json::json!({"level": 3, "tags": ["disk", "full"]}),
        };
        let s = serde_json::to_string(&n).unwrap();
        let back: PushNotification = serde_json::from_str(&s).unwrap();
        assert_eq!(back.title, n.title);
        assert_eq!(back.body, n.body);
        assert_eq!(back.severity, n.severity);
        assert_eq!(back.action_url, n.action_url);
        assert_eq!(back.data, n.data);
    }

    #[test]
    fn push_notification_serde_with_null_fields() {
        // action_url=None / data=Value::Null 的最小通知
        let n = note("hi");
        let s = serde_json::to_string(&n).unwrap();
        assert!(s.contains("\"action_url\":null"));
        assert!(s.contains("\"data\":null"));
        let back: PushNotification = serde_json::from_str(&s).unwrap();
        assert_eq!(back.title, "hi");
        assert!(back.action_url.is_none());
        assert_eq!(back.data, serde_json::Value::Null);
    }

    #[test]
    fn push_notification_serde_missing_severity_errors() {
        // severity 字段缺失 → 反序列化失败
        let r: Result<PushNotification, _> = serde_json::from_str(r#"{"title":"t","body":"b"}"#);
        assert!(r.is_err());
    }

    #[test]
    fn push_notification_with_complex_data() {
        // data 为复杂对象（数组嵌对象）
        let n = PushNotification {
            title: "task".into(),
            body: "done".into(),
            severity: PushSeverity::Info,
            action_url: None,
            data: serde_json::json!({
                "items": [{"id": 1}, {"id": 2}],
                "count": 2,
            }),
        };
        let s = serde_json::to_string(&n).unwrap();
        let back: PushNotification = serde_json::from_str(&s).unwrap();
        assert_eq!(back.data["count"], 2);
        assert_eq!(back.data["items"][0]["id"], 1);
    }

    #[test]
    fn subscription_state_equality_and_debug() {
        // PartialEq/Eq/Debug/Default/Copy 派生
        assert_eq!(
            PushSubscriptionState::default(),
            PushSubscriptionState::Unsubscribed
        );
        assert_ne!(
            PushSubscriptionState::Subscribed,
            PushSubscriptionState::Unsubscribed
        );
        let s = PushSubscriptionState::Subscribed;
        let _copy = s;
        assert_eq!(s, _copy);
        let _dbg = format!("{:?}", s);
    }

    #[test]
    fn queue_with_capacity_one_roundtrip() {
        // 容量 1：每次入队都丢弃上一个
        let mut q = NotificationQueue::new(1);
        assert!(q.is_empty());
        assert!(q.push(note("a")).is_none());
        let dropped = q.push(note("b")).unwrap();
        assert_eq!(dropped.title, "a");
        assert_eq!(q.len(), 1);
        assert_eq!(q.peek().unwrap().title, "b");
    }

    #[test]
    fn queue_peek_empty_is_none() {
        let q = NotificationQueue::new(5);
        assert!(q.peek().is_none());
    }

    #[test]
    fn queue_drain_empty_returns_empty() {
        let mut q = NotificationQueue::new(5);
        assert!(q.drain().is_empty());
        assert!(q.is_empty());
    }

    #[test]
    fn queue_pop_empty_is_none() {
        let mut q = NotificationQueue::new(5);
        assert!(q.pop().is_none());
    }

    #[test]
    fn queue_default_is_default_capacity() {
        let q = NotificationQueue::default();
        // 不直接断言内部 capacity，但默认容量 512 足够 push 几条而不溢出
        let _cloned = q.clone(); // 覆盖 Clone 派生
        assert!(q.is_empty());
    }

    #[tokio::test]
    async fn inmemory_subscriber_default_eq_new() {
        let s1 = InMemoryPushSubscriber::default();
        let s2 = InMemoryPushSubscriber::new();
        // 两者初始状态一致
        assert_eq!(s1.state(), s2.state());
        assert_eq!(s1.queued_count(), 0);
        assert_eq!(s2.queued_count(), 0);
    }

    #[tokio::test]
    async fn inmemory_subscriber_with_capacity_queues_many() {
        let sub = InMemoryPushSubscriber::with_queue_capacity(3);
        sub.deliver(note("a")).await;
        sub.deliver(note("b")).await;
        sub.deliver(note("c")).await;
        assert_eq!(sub.queued_count(), 3);
    }

    #[tokio::test]
    async fn inmemory_subscriber_unsubscribe_when_not_subscribed_errors() {
        let sub = InMemoryPushSubscriber::new();
        let err = sub.unsubscribe().await.unwrap_err();
        assert!(matches!(err, MobileError::PushFailed(_)));
    }

    #[tokio::test]
    async fn inmemory_subscriber_subscribe_when_already_subscribed_errors() {
        let sub = InMemoryPushSubscriber::new();
        let cb = std::sync::Arc::new(RecordCallback {
            received: std::sync::Mutex::new(Vec::new()),
        });
        sub.subscribe(Box::new(RecordCallbackProxy(cb.clone())))
            .await
            .unwrap();
        // 重复订阅 → 错误
        let err = sub
            .subscribe(Box::new(RecordCallbackProxy(cb.clone())))
            .await
            .unwrap_err();
        assert!(matches!(err, MobileError::PushFailed(_)));
    }

    #[tokio::test]
    async fn inmemory_subscriber_resubscribe_after_unsubscribe() {
        // unsubscribe 后可重新 subscribe
        let sub = InMemoryPushSubscriber::new();
        let cb = std::sync::Arc::new(RecordCallback {
            received: std::sync::Mutex::new(Vec::new()),
        });
        sub.subscribe(Box::new(RecordCallbackProxy(cb.clone())))
            .await
            .unwrap();
        sub.unsubscribe().await.unwrap();
        // 再次订阅成功
        sub.subscribe(Box::new(RecordCallbackProxy(cb.clone())))
            .await
            .unwrap();
        assert_eq!(sub.state(), PushSubscriptionState::Subscribed);
    }

    #[tokio::test]
    async fn inmemory_subscriber_deliver_after_unsubscribe_then_resubscribe_flushes() {
        // unsubscribe 后投递进队列；重新 subscribe 后再投递会 flush
        let sub = InMemoryPushSubscriber::new();
        let cb = std::sync::Arc::new(RecordCallback {
            received: std::sync::Mutex::new(Vec::new()),
        });
        sub.subscribe(Box::new(RecordCallbackProxy(cb.clone())))
            .await
            .unwrap();
        sub.unsubscribe().await.unwrap();
        // 取消订阅后投递 1 条进队列
        sub.deliver(note("queued")).await;
        assert_eq!(sub.queued_count(), 1);
        // 重新订阅
        sub.subscribe(Box::new(RecordCallbackProxy(cb.clone())))
            .await
            .unwrap();
        // 投递 1 条 → flush 缓存 + 本次
        sub.deliver(note("new")).await;
        let got = cb.received.lock().unwrap().clone();
        assert_eq!(got, vec!["queued".to_string(), "new".to_string()]);
    }
}
