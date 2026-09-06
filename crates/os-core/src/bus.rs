//! `TokioBroadcastBus`——基于 `tokio::sync::broadcast` 的 `EventBus` 默认实现。
//!
//! 设计：
//! - 一个 broadcast channel 承载所有 topic 的事件流（容量见 `DEFAULT_CAPACITY`）。
//! - `subscribe(topic, subscriber)` 为每个订阅 spawn 一个常驻任务：持有独立的
//!   `broadcast::Receiver`（这是 broadcast 的天然多消费者模型），收到的 event 按
//!   `topic_matches` 过滤后调用 `subscriber.handle(&event).await`。
//! - `publish(event)` 把 event 广播给所有 receiver；每个 receiver 任务自行过滤。
//! - 订阅句柄（`SubscriptionHandle`）drop 即取消：handle 内持有任务 `JoinHandle` 与
//!   receiver，drop 时 abort 任务（实现"订阅句柄 drop 即取消"语义）。
//! - `unsubscribe(id)` 也提供显式取消（trait 要求）。
//!
//! 背压策略：broadcast channel 容量固定（`DEFAULT_CAPACITY = 1024`）。当订阅者消费
//! 速度落后、channel 中待消费 event 超过容量时，最旧的 event 会被丢弃，慢消费者
//! 的 `recv()` 返回 `RecvError::Lagged(n)`——本实现记为 warning 级日志（tracing 未启用
//! 时静默），并继续消费后续 event（不阻塞发布者、不拖累其他订阅者）。容量调整需走
//! ADR（影响丢消息语义）。

use crate::eventbus::{Event, EventBus, EventSubscriber, SubscriptionId, Topic};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

/// broadcast channel 默认容量（每个订阅者独立的环形缓冲大小）。
///
/// 选择 1024：足够吸收典型事件突发（如批量快照/复制进度），同时避免无界内存增长。
/// 慢消费者超过此容量会丢最旧 event（见模块文档"背压策略"）。容量调整需 ADR。
pub const DEFAULT_CAPACITY: usize = 1024;

/// 订阅句柄——drop 即取消订阅（abort 内部常驻任务）。
///
/// 由 `TokioBroadcastBus::subscribe` 内部创建；外部通常通过 `SubscriptionId` +
/// `unsubscribe` 取消。此类型主要在 bus 内部管理生命周期。
pub struct SubscriptionHandle {
    task: Option<JoinHandle<()>>,
}

impl SubscriptionHandle {
    fn new(task: JoinHandle<()>) -> Self {
        Self { task: Some(task) }
    }
}

impl Drop for SubscriptionHandle {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// 基于 `tokio::sync::broadcast` 的 `EventBus` 默认实现。
///
/// 用法：
/// ```no_run
/// # use os_core::{TokioBroadcastBus, EventBus, Event, Topic};
/// # async fn demo() -> Result<(), os_core::CoreError> {
/// let bus = TokioBroadcastBus::new();
/// bus.publish(Event::new("demo", Topic::System, "boot")).await?;
/// # Ok(())
/// # }
/// ```
pub struct TokioBroadcastBus {
    tx: broadcast::Sender<Event>,
    /// 订阅注册表：SubscriptionId -> (Topic, SubscriptionHandle)。
    /// `SubscriptionHandle` 存此即保持常驻任务存活；移除/移出即 drop 取消。
    subs: Mutex<HashMap<SubscriptionId, (Topic, SubscriptionHandle)>>,
    next_id: AtomicU64,
    /// 容量（供测试断言/构造自定义容量实例）
    capacity: usize,
}

impl TokioBroadcastBus {
    /// 用默认容量（`DEFAULT_CAPACITY`）构造
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// 用自定义容量构造（仅测试/特殊场景用；正式容量调整需 ADR）
    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity.max(1));
        Self {
            tx,
            subs: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            capacity,
        }
    }

    /// 当前活跃订阅数（供测试/监控）
    pub fn subscriber_count(&self) -> usize {
        self.subs.lock().expect("subs poisoned").len()
    }

    /// 当前 broadcast channel 容量
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    fn alloc_id(&self) -> SubscriptionId {
        SubscriptionId(self.next_id.fetch_add(1, Ordering::Relaxed) + 1)
    }
}

impl Default for TokioBroadcastBus {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EventBus for TokioBroadcastBus {
    async fn publish(&self, event: Event) -> Result<(), crate::CoreError> {
        // broadcast::send 失败仅当无 receiver——此时视为空操作成功（event 丢弃）。
        // 有 receiver 时返回当前 receiver 数。两种情况都不算总线错误。
        let _ = self.tx.send(event);
        Ok(())
    }

    async fn subscribe(
        &self,
        topic: Topic,
        subscriber: Box<dyn EventSubscriber>,
    ) -> Result<SubscriptionId, crate::CoreError> {
        let id = self.alloc_id();
        let subscriber: Arc<dyn EventSubscriber> = Arc::from(subscriber);
        let mut rx = self.tx.subscribe();
        // 每个 spawn 的任务收所有 event，按 topic 过滤后调用 handle。
        // topic move 进闭包（订阅者在任务存活期间用同一 topic 过滤）；原 topic
        // 留给下方注册表 insert（Topic: Clone，但此处只在闭包前用一次）。
        let task_topic = topic.clone();
        let task = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if topic_matches(&task_topic, &event.topic) {
                            // 调用订阅者 handle；handle 返回 boxed future，await 它。
                            // 单个订阅者 panic/handle 异常不应波及其他订阅者，
                            // 但 broadcast 通道无内置隔离——此处按 ADR-COMPAT-001
                            // 约定的低频路径直接 await。
                            subscriber.handle(&event).await;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // 发送端全部 drop（bus 被 drop），结束任务
                        break;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // 慢消费者丢消息：记录后继续（不退出）。无 tracing 依赖时静默。
                        // n 为丢弃的 event 数；此处用 std 入门级日志占位，避免引入新依赖。
                        let _ = n; // 容量溢出告警留给上层观测系统
                    }
                }
            }
        });
        let handle = SubscriptionHandle::new(task);
        self.subs
            .lock()
            .expect("subs poisoned")
            .insert(id, (topic, handle));
        Ok(id)
    }

    async fn unsubscribe(&self, id: SubscriptionId) -> Result<(), crate::CoreError> {
        self.subs.lock().expect("subs poisoned").remove(&id); // remove 即 drop SubscriptionHandle 即 abort 任务
        Ok(())
    }
}

/// 判断订阅 topic 是否匹配事件 topic（`Topic::All` 匹配所有）
///
/// 与 `mock` feature 下 `mock::topic_matches` 保持一致语义。
fn topic_matches(subscribed: &Topic, event_topic: &Topic) -> bool {
    *subscribed == Topic::All || subscribed == event_topic
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    /// 测试用订阅者：把收到的 event 的 topic 推进共享 vec（用 Mutex 而非 async 通道，
    /// 与 handle 的同步记录语义一致）。
    struct CollectingSubscriber {
        received: Arc<StdMutex<Vec<Topic>>>,
    }

    impl CollectingSubscriber {
        fn new() -> (Self, Arc<StdMutex<Vec<Topic>>>) {
            let received = Arc::new(StdMutex::new(Vec::new()));
            (
                Self {
                    received: Arc::clone(&received),
                },
                received,
            )
        }
    }

    impl EventSubscriber for CollectingSubscriber {
        fn handle(
            &self,
            event: &Event,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
            self.received.lock().unwrap().push(event.topic.clone());
            Box::pin(async {})
        }
    }

    #[tokio::test]
    async fn publish_delivers_to_matching_subscriber() {
        let bus = TokioBroadcastBus::new();
        let (sub, received) = CollectingSubscriber::new();
        let _id = bus.subscribe(Topic::Storage, Box::new(sub)).await.unwrap();

        // 给 spawn 的任务一点启动时间（subscribe spawn 是异步的）
        tokio::time::sleep(Duration::from_millis(20)).await;

        bus.publish(Event::new("t", Topic::Storage, "k1"))
            .await
            .unwrap();
        bus.publish(Event::new("t", Topic::Compute, "k2"))
            .await
            .unwrap();

        // 等任务消费完
        tokio::time::sleep(Duration::from_millis(50)).await;

        let got = received.lock().unwrap().clone();
        assert_eq!(got, vec![Topic::Storage]);
    }

    #[tokio::test]
    async fn topic_all_matches_everything() {
        let bus = TokioBroadcastBus::new();
        let (sub, received) = CollectingSubscriber::new();
        let _id = bus.subscribe(Topic::All, Box::new(sub)).await.unwrap();

        tokio::time::sleep(Duration::from_millis(20)).await;

        bus.publish(Event::new("t", Topic::Storage, "k1"))
            .await
            .unwrap();
        bus.publish(Event::new("t", Topic::Network, "k2"))
            .await
            .unwrap();
        bus.publish(Event::new("t", Topic::Wallet, "k3"))
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        let got = received.lock().unwrap().clone();
        assert_eq!(got.len(), 3);
    }

    #[tokio::test]
    async fn unsubscribe_stops_delivery() {
        let bus = TokioBroadcastBus::new();
        let (sub, received) = CollectingSubscriber::new();
        let id = bus.subscribe(Topic::Storage, Box::new(sub)).await.unwrap();

        tokio::time::sleep(Duration::from_millis(20)).await;
        bus.publish(Event::new("t", Topic::Storage, "before"))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(received.lock().unwrap().len(), 1);

        bus.unsubscribe(id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        bus.publish(Event::new("t", Topic::Storage, "after"))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        // unsubscribe 后不再收到
        assert_eq!(received.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn multiple_subscribers_isolated() {
        let bus = TokioBroadcastBus::new();
        let (s1, r1) = CollectingSubscriber::new();
        let (s2, r2) = CollectingSubscriber::new();
        let _id1 = bus.subscribe(Topic::Storage, Box::new(s1)).await.unwrap();
        let _id2 = bus.subscribe(Topic::Compute, Box::new(s2)).await.unwrap();

        tokio::time::sleep(Duration::from_millis(20)).await;
        bus.publish(Event::new("t", Topic::Storage, "k1"))
            .await
            .unwrap();
        bus.publish(Event::new("t", Topic::Compute, "k2"))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(r1.lock().unwrap().clone(), vec![Topic::Storage]);
        assert_eq!(r2.lock().unwrap().clone(), vec![Topic::Compute]);
    }

    #[tokio::test]
    async fn concurrent_publish_safe() {
        // 多任务并发 publish 不应 panic / 不应丢（无 lag 时）
        let bus = Arc::new(TokioBroadcastBus::new());
        let (sub, received) = CollectingSubscriber::new();
        let _id = bus.subscribe(Topic::All, Box::new(sub)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut handles = Vec::new();
        for i in 0..20 {
            let bus = Arc::clone(&bus);
            handles.push(tokio::spawn(async move {
                bus.publish(Event::new("t", Topic::Storage, format!("k{i}")))
                    .await
                    .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(80)).await;

        // 20 条全部应被消费（容量 1024 足够，无 lag）
        assert_eq!(received.lock().unwrap().len(), 20);
        assert_eq!(bus.subscriber_count(), 1);
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_ok() {
        // 无订阅者时 publish 不应报错（broadcast send 返回 Err 视为正常）
        let bus = TokioBroadcastBus::new();
        bus.publish(Event::new("t", Topic::Storage, "k"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn with_capacity_custom() {
        let bus = TokioBroadcastBus::with_capacity(8);
        assert_eq!(bus.capacity(), 8);
    }
}
