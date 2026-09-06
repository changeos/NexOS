//! EventBus trait —— 节点内事件总线（pub/sub）
//!
//! 决策依据：规划文档 §9.1#9 —— 节点内事件总线归 os-core 提供（trait + 默认 tokio broadcast
//! 实现），跨节点走 os-meta 的 openraft log。
//!
//! 设计：
//! - trait 用 async（发布/订阅属运行时数据路径）
//! - 默认实现为 `TokioBroadcastBus`（基于 tokio::sync::broadcast）
//! - 跨组件解耦：发布者不关心谁订阅；订阅者按 Topic 过滤

use crate::ids::TaskId;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// 事件主题（按业务域分类，订阅时过滤）
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Topic {
    /// 存储事件（池创建/快照完成/复制进度/磁盘故障）
    Storage,
    /// 计算事件（VM/容器启停/迁移）
    Compute,
    /// 集群事件（HA 切换/成员变更）
    Cluster,
    /// 网络事件（接口 up/down）
    Network,
    /// 访客事件（认证/过期/撤销）
    Guest,
    /// 钱包事件（连接建立/签名完成）
    Wallet,
    /// 安全事件（证书过期/2FA/入侵）
    Security,
    /// agent 任务事件（委派/完成/失败，见 §3.7.2）
    AgentTask,
    /// 系统事件（osd 组件启停/更新）
    System,
    /// 全部（通配，订阅时用）
    All,
}

/// 事件严重级别
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warn,
    Error,
    Critical,
}

/// 系统事件（跨组件传递的结构化消息）
///
/// 所有跨组件异步通知经 EventBus 传递；payload 用 serde_json::Value 保持开放
/// （各域自定义 payload 结构，消费方按 source + kind 解析）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// 来源组件（如 "os-storage" / "os-meta"）
    pub source: String,
    /// 主题
    pub topic: Topic,
    /// 事件类型（如 "snapshot.completed" / "failover.triggered"）
    pub kind: String,
    pub severity: Severity,
    /// 关联任务（如有，便于链路追踪）
    pub task_id: Option<TaskId>,
    /// 负载（开放结构，消费方按 kind 解析）
    pub payload: serde_json::Value,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl Event {
    pub fn new(source: impl Into<String>, topic: Topic, kind: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            topic,
            kind: kind.into(),
            severity: Severity::Info,
            task_id: None,
            payload: serde_json::Value::Null,
            timestamp: chrono::Utc::now(),
        }
    }
}

/// 订阅 ID（取消订阅时用）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriptionId(pub u64);

/// 事件订阅者（收到事件后的回调）
///
/// 实现者通常是各组件的事件处理闭包。为支持 trait object，用 async fn in trait
/// 时需 `Send` bound；这里用 boxed Future 以支持动态分发。
pub trait EventSubscriber: Send + Sync {
    /// 处理事件；返回 Err 表示处理失败（由 EventBus 决定是否重试/记录）
    fn handle(
        &self,
        event: &Event,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>;
}

/// 事件总线 trait（节点内 pub/sub）
///
/// 实现者：`TokioBroadcastBus`（默认，os-core 提供）；其他实现可替换。
/// 并发性：publish 可被多任务并发调用；subscribe 返回的订阅在 drop 时自动取消。
///
/// 经 `Box<dyn EventBus>` 注入到各业务组件（如 osd 编排器、os-api WS 网关），
/// 故用 `#[async_trait]` 保证 dyn 兼容（ADR-COMPAT-001 / ADR-COMPAT-003）。
#[async_trait]
pub trait EventBus: Send + Sync {
    /// 发布事件；订阅了匹配 Topic 的订阅者会收到回调。
    async fn publish(&self, event: Event) -> Result<(), crate::CoreError>;

    /// 订阅指定主题；返回订阅 ID，用 unsubscribe 取消。
    async fn subscribe(
        &self,
        topic: Topic,
        subscriber: Box<dyn EventSubscriber>,
    ) -> Result<SubscriptionId, crate::CoreError>;

    /// 取消订阅
    async fn unsubscribe(&self, id: SubscriptionId) -> Result<(), crate::CoreError>;
}
