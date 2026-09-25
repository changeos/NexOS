//! Mock 实现——供下游 crate 单元测/集成测注入。
//!
//! 仅在 `mock` feature 下编译（`#[cfg(feature = "mock")]`）。
//! 用法见 `_conventions.md §5`：下游在 `[dev-dependencies]` 加
//! `os-core = { workspace = true, features = ["mock"] }`，然后注入 `MockEventBus`。
//!
//! 设计：
//! - `MockEventBus`：纯内存实现 `EventBus`，记录所有 publish 的 event 供断言；
//!   `subscribe` 把订阅者存起来，测试可通过 `dispatch` 主动派发或断言注册情况。
//! - `MockEventSubscriber`：实现 `EventSubscriber`，记录收到的每个 event 供断言。

#![cfg(feature = "mock")]

use crate::eventbus::{Event, EventBus, EventSubscriber, SubscriptionId, Topic};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};

/// 注册表项：(订阅 ID, 订阅 Topic, 订阅者句柄)
type SubscriptionEntry = (SubscriptionId, Topic, Arc<dyn EventSubscriber>);

// ---------------------------------------------------------------------------
// MockEventBus
// ---------------------------------------------------------------------------

/// Mock 事件总线——纯内存、确定性，供下游测试注入。
///
/// - `publish` 不真正派发，仅把 event 记录到内部日志（`published()` 取出断言）。
/// - `subscribe` 把订阅者存起来，返回递增的 `SubscriptionId`；测试可用
///   `subscribers_for` / `subscriber_count` 断言订阅情况，或用 `dispatch_to` 主动派发。
/// - `unsubscribe` 从内部映射移除订阅者。
///
/// 内部状态用 `Mutex` 保护，故 `publish`/`subscribe`/`unsubscribe` 均为 `async fn`
/// 但实际是同步的（trait 要求 async）；这些方法永不 panic、永不阻塞。
#[derive(Default)]
pub struct MockEventBus {
    /// 所有已发布事件（按发布顺序）
    published: Mutex<Vec<Event>>,
    /// 订阅者列表：(订阅 ID, 订阅 Topic, 订阅者句柄)
    subscribers: Mutex<Vec<SubscriptionEntry>>,
    /// 下一个 SubscriptionId 的源（单调递增）
    next_id: Mutex<u64>,
}

impl MockEventBus {
    /// 构造空 mock
    pub fn new() -> Self {
        Self::default()
    }

    /// 取出已发布事件的快照（按发布顺序）
    pub fn published(&self) -> Vec<Event> {
        self.published.lock().expect("mock poisoned").clone()
    }

    /// 已发布事件中匹配给定 topic 的数量（`Topic::All` 视为匹配所有）
    pub fn published_count_for(&self, topic: Topic) -> usize {
        self.published()
            .iter()
            .filter(|e| topic_matches(&topic, &e.topic))
            .count()
    }

    /// 已发布事件总数
    pub fn published_count(&self) -> usize {
        self.published.lock().expect("mock poisoned").len()
    }

    /// 当前活跃订阅数量
    pub fn subscriber_count(&self) -> usize {
        self.subscribers.lock().expect("mock poisoned").len()
    }

    /// 取精确订阅某 topic 的所有 SubscriptionId（按注册时的 topic 精确匹配，
    /// 不做 `Topic::All` 通配——查询"谁订阅了 All"应只返回真正订阅 All 的）。
    pub fn subscribers_for(&self, topic: Topic) -> Vec<SubscriptionId> {
        self.subscribers
            .lock()
            .expect("mock poisoned")
            .iter()
            .filter(|(_, t, _)| *t == topic)
            .map(|(id, _, _)| *id)
            .collect()
    }

    /// 按 SubscriptionId 取订阅句柄（供测试主动派发）
    pub fn subscriber(&self, id: SubscriptionId) -> Option<Arc<dyn EventSubscriber>> {
        self.subscribers
            .lock()
            .expect("mock poisoned")
            .iter()
            .find(|(sid, _, _)| *sid == id)
            .map(|(_, _, s)| Arc::clone(s))
    }

    /// 清空已发布事件记录（在多阶段测试间重置断言基线）
    pub fn clear_published(&self) {
        self.published.lock().expect("mock poisoned").clear();
    }

    fn alloc_id(&self) -> SubscriptionId {
        let mut guard = self.next_id.lock().expect("mock poisoned");
        *guard += 1;
        SubscriptionId(*guard)
    }
}

#[async_trait]
impl EventBus for MockEventBus {
    async fn publish(&self, event: Event) -> Result<(), crate::CoreError> {
        self.published.lock().expect("mock poisoned").push(event);
        Ok(())
    }

    async fn subscribe(
        &self,
        topic: Topic,
        subscriber: Box<dyn EventSubscriber>,
    ) -> Result<SubscriptionId, crate::CoreError> {
        let id = self.alloc_id();
        self.subscribers
            .lock()
            .expect("mock poisoned")
            .push((id, topic, Arc::from(subscriber)));
        Ok(id)
    }

    async fn unsubscribe(&self, id: SubscriptionId) -> Result<(), crate::CoreError> {
        let mut guard = self.subscribers.lock().expect("mock poisoned");
        guard.retain(|(sid, _, _)| *sid != id);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// MockEventSubscriber
// ---------------------------------------------------------------------------

/// Mock 订阅者——记录收到的每个 event，供测试断言。
///
/// 实现了 `EventSubscriber::handle`（空操作 future，仅记录），内部用 `Mutex<Vec<Event>>`
/// 收集事件。`Clone` 廉价（仅 `Arc` 引用计数变化），可在多处共享同一记录器。
#[derive(Debug, Default, Clone)]
pub struct MockEventSubscriber {
    received: Arc<Mutex<Vec<Event>>>,
}

impl MockEventSubscriber {
    pub fn new() -> Self {
        Self::default()
    }

    /// 取已收到的事件快照（按到达顺序）
    pub fn received(&self) -> Vec<Event> {
        self.received.lock().expect("mock poisoned").clone()
    }

    /// 已收到事件数
    pub fn received_count(&self) -> usize {
        self.received.lock().expect("mock poisoned").len()
    }

    /// 清空已收到事件
    pub fn clear(&self) {
        self.received.lock().expect("mock poisoned").clear();
    }
}

impl EventSubscriber for MockEventSubscriber {
    fn handle(
        &self,
        event: &Event,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        self.received
            .lock()
            .expect("mock poisoned")
            .push(event.clone());
        Box::pin(async {})
    }
}

// ---------------------------------------------------------------------------
// 辅助：topic 匹配（与 TokioBroadcastBus 保持一致）
// ---------------------------------------------------------------------------

/// 判断订阅 topic 是否匹配事件 topic（`Topic::All` 匹配所有）
pub(crate) fn topic_matches(subscribed: &Topic, event_topic: &Topic) -> bool {
    *subscribed == Topic::All || subscribed == event_topic
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(topic: Topic) -> Event {
        Event::new("test-src", topic, "test.kind")
    }

    #[tokio::test]
    async fn mock_bus_records_published_events() {
        let bus = MockEventBus::new();
        bus.publish(ev(Topic::Storage)).await.unwrap();
        bus.publish(ev(Topic::Compute)).await.unwrap();
        bus.publish(ev(Topic::Storage)).await.unwrap();

        assert_eq!(bus.published_count(), 3);
        assert_eq!(bus.published_count_for(Topic::Storage), 2);
        assert_eq!(bus.published_count_for(Topic::Compute), 1);
        assert_eq!(bus.published_count_for(Topic::Network), 0);
    }

    #[tokio::test]
    async fn mock_bus_clear_published() {
        let bus = MockEventBus::new();
        bus.publish(ev(Topic::Storage)).await.unwrap();
        assert_eq!(bus.published_count(), 1);
        bus.clear_published();
        assert_eq!(bus.published_count(), 0);
    }

    #[tokio::test]
    async fn mock_bus_subscribe_unsubscribe() {
        let bus = MockEventBus::new();
        let id1 = bus
            .subscribe(Topic::Storage, Box::new(MockEventSubscriber::new()))
            .await
            .unwrap();
        let id2 = bus
            .subscribe(Topic::All, Box::new(MockEventSubscriber::new()))
            .await
            .unwrap();

        assert_eq!(bus.subscriber_count(), 2);
        assert_eq!(bus.subscribers_for(Topic::Storage), vec![id1]);
        // Topic::All 订阅者用 Topic::All 查询命中
        assert_eq!(bus.subscribers_for(Topic::All).len(), 1);

        bus.unsubscribe(id1).await.unwrap();
        assert_eq!(bus.subscriber_count(), 1);
        assert!(bus.subscribers_for(Topic::Storage).is_empty());

        bus.unsubscribe(id2).await.unwrap();
        assert_eq!(bus.subscriber_count(), 0);
        // 重复 unsubscribe 不 panic
        bus.unsubscribe(id2).await.unwrap();
    }

    #[tokio::test]
    async fn mock_subscriber_handles_dispatch() {
        let sub = MockEventSubscriber::new();
        assert_eq!(sub.received_count(), 0);

        let _ = sub.handle(&ev(Topic::Storage)).await;
        let _ = sub.handle(&ev(Topic::Compute)).await;

        let received = sub.received();
        assert_eq!(received.len(), 2);
        assert_eq!(received[0].topic, Topic::Storage);
        assert_eq!(received[1].topic, Topic::Compute);
    }

    #[tokio::test]
    async fn mock_subscriber_clone_shares_state() {
        let sub = MockEventSubscriber::new();
        let sub2 = sub.clone();
        let _ = sub2.handle(&ev(Topic::Storage)).await;
        // clone 共享内部记录
        assert_eq!(sub.received_count(), 1);
    }
}
