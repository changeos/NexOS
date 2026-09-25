//! WebSocket——推事件/进度/通知（规划文档 §3.6 / §9.1#9）
//!
//! 客户端订阅后，网关把 EventBus 的事件、长任务进度、通知实时推送。
//! 复用 `os_core::{Event, SubscriptionId, Severity}`。

use async_trait::async_trait;
use os_common::ApiErrorCode;
use os_core::{Event, Severity, SubscriptionId, TaskId};
use serde::{Deserialize, Serialize};

// ----------------------------------------------------------------------------
// WS 消息
// ----------------------------------------------------------------------------

/// WebSocket 推送消息
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsMessage {
    /// 事件（来自 EventBus 的原始事件）
    Event {
        /// 事件
        event: Event,
    },
    /// 长任务进度
    Progress {
        /// 关联任务
        task_id: TaskId,
        /// 进度 0.0 ~ 1.0
        progress: f32,
        /// 当前步骤描述
        step: String,
    },
    /// 通知（人类可读）
    Notification {
        /// 通知文本
        message: String,
        /// 严重级别
        severity: Severity,
    },
    /// 错误推送
    Error {
        /// 错误码
        code: ApiErrorCode,
        /// 错误消息
        message: String,
    },
    /// IM 实时消息推送（新消息到达时广播）。
    ///
    /// 序列化形如 `{"type":"im_message","conversation_id":"...","message":{...}}`，
    /// 前端按 `data.type === 'im_message'` 分发，取 `data.message` 追加到当前对话。
    ImMessage {
        /// 所属对话/群组 id
        conversation_id: String,
        /// 消息体（完整 Message DTO 的 JSON Value）
        message: serde_json::Value,
    },
    /// IM 大厅实时消息推送（大厅公共频道新消息/系统广播到达时全员广播）。
    ///
    /// 序列化形如 `{"type":"im_lobby_message","lobby_id":"lobby","message":{...}}`，
    /// 前端按 `data.type === 'im_lobby_message'` 分发，取 `data.message` 追加到大厅。
    ImLobbyMessage {
        /// 大厅 id（当前恒为 "lobby"）
        lobby_id: String,
        /// 消息体（完整 Message DTO 的 JSON Value）
        message: serde_json::Value,
    },
    /// IM 联邦大厅实时消息推送（跨节点共享频道 `fed-lobby` 新消息到达时全员
    /// 广播——本地发言与联邦接收共用同一帧型；与 [`WsMessage::ImLobbyMessage`]
    /// 的大厅频道完全隔离，2026-08-23 会话模型）。
    ///
    /// 序列化形如 `{"type":"im_fed_lobby_message","lobby_id":"fed-lobby","message":{...}}`，
    /// 前端按 `data.type === 'im_fed_lobby_message'` 分发到联邦大厅会话。
    ImFedLobbyMessage {
        /// 联邦大厅 id（恒为 "fed-lobby"）
        lobby_id: String,
        /// 消息体（完整 Message DTO 的 JSON Value）
        message: serde_json::Value,
    },
}

// ----------------------------------------------------------------------------
// WebSocketHub trait（async）
// ----------------------------------------------------------------------------

/// WebSocket 中心——管理订阅与推送。
///
/// 实现者：`AxumWsHub`（默认，对接 Axum WS 与 os-core EventBus）。
/// 方法经 `#[async_trait]` 包装以与实现块的 async fn 一致并保证 dyn 兼容（ADR-COMPAT-001）。
#[async_trait]
pub trait WebSocketHub: Send + Sync {
    /// 全员广播。
    async fn broadcast(&self, msg: WsMessage) -> Result<(), crate::ApiGatewayError>;

    /// 定向推送给某用户。
    async fn send_to(&self, user: &str, msg: WsMessage) -> Result<(), crate::ApiGatewayError>;

    /// 订阅（用户建立连接后调用），返回订阅 ID。
    async fn subscribe(&self, user: &str) -> Result<SubscriptionId, crate::ApiGatewayError>;

    /// 取消订阅。
    async fn unsubscribe(&self, id: SubscriptionId);
}

// ----------------------------------------------------------------------------
// 单元测——WsMessage 各变体 serde 往返
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use os_common::ApiErrorCode;
    use os_core::{Event, Severity, TaskId, Topic};

    #[test]
    fn ws_message_event_roundtrip() {
        let m = WsMessage::Event {
            event: Event::new("src", Topic::System, "evt-1"),
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"type\":\"event\""));
        let back: WsMessage = serde_json::from_str(&json).unwrap();
        match back {
            WsMessage::Event { event } => {
                assert_eq!(event.kind.as_str(), "evt-1");
                assert_eq!(event.source.as_str(), "src");
            }
            _ => panic!("应反序列化为 Event"),
        }
    }

    #[test]
    fn ws_message_progress_roundtrip() {
        let m = WsMessage::Progress {
            task_id: TaskId::new(),
            progress: 0.5,
            step: "transferring".into(),
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"type\":\"progress\""));
        let back: WsMessage = serde_json::from_str(&json).unwrap();
        match back {
            WsMessage::Progress { progress, step, .. } => {
                assert!((progress - 0.5).abs() < 1e-6);
                assert_eq!(step, "transferring");
            }
            _ => panic!("应反序列化为 Progress"),
        }
    }

    #[test]
    fn ws_message_notification_roundtrip() {
        let m = WsMessage::Notification {
            message: "hello".into(),
            severity: Severity::Info,
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"type\":\"notification\""));
        let back: WsMessage = serde_json::from_str(&json).unwrap();
        match back {
            WsMessage::Notification { message, severity } => {
                assert_eq!(message, "hello");
                assert_eq!(severity, Severity::Info);
            }
            _ => panic!("应反序列化为 Notification"),
        }
    }

    #[test]
    fn ws_message_error_roundtrip() {
        let m = WsMessage::Error {
            code: ApiErrorCode::NotFound,
            message: "missing".into(),
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"type\":\"error\""));
        let back: WsMessage = serde_json::from_str(&json).unwrap();
        match back {
            WsMessage::Error { code, message } => {
                assert_eq!(code, ApiErrorCode::NotFound);
                assert_eq!(message, "missing");
            }
            _ => panic!("应反序列化为 Error"),
        }
    }

    #[test]
    fn ws_message_im_lobby_roundtrip() {
        let m = WsMessage::ImLobbyMessage {
            lobby_id: "lobby".into(),
            message: serde_json::json!({"content": "欢迎来到 NexOS 大厅"}),
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"type\":\"im_lobby_message\""));
        assert!(json.contains("\"lobby_id\":\"lobby\""));
        let back: WsMessage = serde_json::from_str(&json).unwrap();
        match back {
            WsMessage::ImLobbyMessage { lobby_id, message } => {
                assert_eq!(lobby_id, "lobby");
                assert_eq!(message["content"], "欢迎来到 NexOS 大厅");
            }
            _ => panic!("应反序列化为 ImLobbyMessage"),
        }
    }

    #[test]
    fn ws_message_im_fed_lobby_roundtrip() {
        let m = WsMessage::ImFedLobbyMessage {
            lobby_id: "fed-lobby".into(),
            message: serde_json::json!({"content": "跨节点共享频道"}),
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"type\":\"im_fed_lobby_message\""), "{json}");
        assert!(json.contains("\"lobby_id\":\"fed-lobby\""), "{json}");
        let back: WsMessage = serde_json::from_str(&json).unwrap();
        match back {
            WsMessage::ImFedLobbyMessage { lobby_id, message } => {
                assert_eq!(lobby_id, "fed-lobby");
                assert_eq!(message["content"], "跨节点共享频道");
            }
            _ => panic!("应反序列化为 ImFedLobbyMessage"),
        }
    }

    #[test]
    fn ws_message_clone_and_debug() {
        // Clone + Debug derive：确保可用
        let m = WsMessage::Notification {
            message: "x".into(),
            severity: Severity::Info,
        };
        let cloned = m.clone();
        let dbg = format!("{:?}", m);
        assert!(dbg.contains("Notification"));
        match cloned {
            WsMessage::Notification { message, .. } => assert_eq!(message, "x"),
            _ => panic!(),
        }
    }
}
