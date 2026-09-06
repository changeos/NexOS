//! P2P 传输层——节点间 TCP 连接 + 消息路由（规划文档 §3.7 入口/分布式）
//!
//! 多个 OS 节点的 IM 通过本层互联：每个节点同时是 server（`listen`）与
//! client（`connect`），消息以 [`TransportMessage`] 信封在节点间路由，
//! 载荷复用 [`crate::conversation::Message`]（或其 serde_json 子集）。
//!
//! 设计要点：
//! - 仅定义契约（`P2pTransport` trait + 数据结构），实现见 `transport_impl.rs`
//! - 本 trait 需 `Box<dyn P2pTransport>` 运行期多态，故用 `#[async_trait]`
//!   （呼应 lib 顶部 dyn 兼容性修正约定）
//! - 线长定帧协议见 [`TransportMsgType`]（4 字节长度头 + JSON body）

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use os_core::DateTime;
use serde::{Deserialize, Serialize};

use crate::error::ImError;

/// 当前 UTC 时间（与 crate 内其他模块一致，见 impls.rs）。
fn now() -> DateTime {
    chrono::Utc::now()
}

// ----------------------------------------------------------------------------
// 节点信息
// ----------------------------------------------------------------------------

/// P2P 节点信息——一个对端 OS 实例的网络身份与连接状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerNode {
    /// 节点唯一 ID（握手时由 NodeHello 交换，全局唯一）
    pub node_id: String,
    /// 监听地址（`IP:端口`），用于重连 / 广播发现
    pub addr: String,
    /// 显示名（用户可读，如 "客厅-OS"）
    pub display_name: String,
    /// 当前是否已连接（含活跃 TCP）
    pub connected: bool,
    /// 最近一次心跳 / 消息到达时间（UTC）
    pub last_seen: DateTime,
}

impl PeerNode {
    /// 构造一个已连接的 PeerNode（`last_seen` 取当前 UTC）。
    pub fn new(
        node_id: impl Into<String>,
        addr: impl Into<String>,
        display_name: impl Into<String>,
    ) -> Self {
        Self {
            node_id: node_id.into(),
            addr: addr.into(),
            display_name: display_name.into(),
            connected: true,
            last_seen: now(),
        }
    }

    /// 标记为已断开。
    pub fn mark_disconnected(&mut self) {
        self.connected = false;
    }

    /// 刷新 `last_seen` 为当前 UTC（收到任何消息/心跳时调用）。
    pub fn touch(&mut self) {
        self.last_seen = now();
    }
}

impl fmt::Display for PeerNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = if self.connected { "online" } else { "offline" };
        write!(
            f,
            "PeerNode({} @ {}, name={}, {})",
            self.node_id, self.addr, self.display_name, state
        )
    }
}

// ----------------------------------------------------------------------------
// 消息类型 / 信封
// ----------------------------------------------------------------------------

/// P2P 传输消息类型（信封的 `message_type`，决定 payload schema）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportMsgType {
    /// 聊天消息（payload = crate::conversation::Message 的 JSON）
    Chat,
    /// 建群通知
    GroupCreate,
    /// 加入群
    GroupJoin,
    /// 退出群
    GroupLeave,
    /// 节点握手（payload 含本节点身份：node_id / display_name / capabilities）
    NodeHello,
    /// 心跳（payload 可空，每 30s 发送一次）
    NodeHeartbeat,
    /// 主动断开（优雅关闭前发送）
    NodeBye,
}

impl TransportMsgType {
    /// 全部变体（便于单测遍历 / 文档枚举）。
    pub const ALL: &[TransportMsgType] = &[
        TransportMsgType::Chat,
        TransportMsgType::GroupCreate,
        TransportMsgType::GroupJoin,
        TransportMsgType::GroupLeave,
        TransportMsgType::NodeHello,
        TransportMsgType::NodeHeartbeat,
        TransportMsgType::NodeBye,
    ];

    /// 是否为节点控制类消息（Hello/Heartbeat/Bye）。
    pub fn is_node_control(self) -> bool {
        matches!(
            self,
            TransportMsgType::NodeHello
                | TransportMsgType::NodeHeartbeat
                | TransportMsgType::NodeBye
        )
    }
}

impl fmt::Display for TransportMsgType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportMsgType::Chat => write!(f, "chat"),
            TransportMsgType::GroupCreate => write!(f, "group_create"),
            TransportMsgType::GroupJoin => write!(f, "group_join"),
            TransportMsgType::GroupLeave => write!(f, "group_leave"),
            TransportMsgType::NodeHello => write!(f, "node_hello"),
            TransportMsgType::NodeHeartbeat => write!(f, "node_heartbeat"),
            TransportMsgType::NodeBye => write!(f, "node_bye"),
        }
    }
}

/// P2P 传输消息——节点间传递的信封（payload 为任意 JSON，由 `message_type` 决定 schema）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportMessage {
    /// 发送方节点 ID
    pub from_node: String,
    /// 目标节点 ID（`None` = 广播给所有已连接节点）
    pub to_node: Option<String>,
    /// 所属群 ID（群消息路由线索，私聊为 `None`）
    pub group_id: Option<String>,
    /// 消息类型（决定 payload 解释方式）
    pub message_type: TransportMsgType,
    /// 消息载荷（任意 JSON；Chat 场景为 `crate::conversation::Message`）
    pub payload: serde_json::Value,
    /// 时间戳（UTC）
    pub timestamp: DateTime,
    /// 消息签名（可选；HMAC / Ed25519，防伪造——校验由实现层负责）
    pub signature: Option<String>,
}

impl TransportMessage {
    /// 构造一条点对点消息（to_node = Some）。
    pub fn direct(
        from_node: impl Into<String>,
        to_node: impl Into<String>,
        message_type: TransportMsgType,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            from_node: from_node.into(),
            to_node: Some(to_node.into()),
            group_id: None,
            message_type,
            payload,
            timestamp: now(),
            signature: None,
        }
    }

    /// 构造一条广播消息（to_node = None）。
    pub fn broadcast(
        from_node: impl Into<String>,
        message_type: TransportMsgType,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            from_node: from_node.into(),
            to_node: None,
            group_id: None,
            message_type,
            payload,
            timestamp: now(),
            signature: None,
        }
    }

    /// 是否为广播消息。
    pub fn is_broadcast(&self) -> bool {
        self.to_node.is_none()
    }

    /// 链式设置 group_id。
    #[must_use]
    pub fn with_group(mut self, group_id: impl Into<String>) -> Self {
        self.group_id = Some(group_id.into());
        self
    }

    /// 链式设置 signature。
    #[must_use]
    pub fn with_signature(mut self, signature: impl Into<String>) -> Self {
        self.signature = Some(signature.into());
        self
    }
}

// ----------------------------------------------------------------------------
// 接收回调 / 配置
// ----------------------------------------------------------------------------

/// 消息接收回调：实现层 spawn 的读 task 收到一帧后调用此函数。
///
/// 注：`Arc<dyn Fn>` 形式以便多线程共享；回调内禁止阻塞过久（建议派发到 channel）。
pub type MessageHandler = std::sync::Arc<dyn Fn(TransportMessage) + Send + Sync>;

/// P2P 传输层默认配置常量。
pub mod defaults {
    use super::Duration;

    /// 心跳间隔（30 秒）。
    pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

    /// 单帧最大字节数（16 MiB，防止恶意节点打爆内存）。
    pub const MAX_FRAME_SIZE: u64 = 16 * 1024 * 1024;

    /// 长度头字节数（4 字节大端无符号 u32）。
    pub const LENGTH_HEADER_BYTES: usize = 4;
}

// ----------------------------------------------------------------------------
// P2pTransport trait
// ----------------------------------------------------------------------------

/// P2P 传输层契约——节点间 TCP 连接管理与消息路由。
///
/// 实现者：`TcpP2pTransport`（`transport_impl.rs`，基于 tokio TCP +
/// length-delimited JSON 帧）。
///
/// 生命周期：
/// - `listen`：作为 server，监听 `addr`，接收其他节点主动连入；
/// - `connect`：作为 client，主动拨号到远端 `addr`，返回握手后的 [`PeerNode`]；
/// - `send` / `broadcast`：路由单播 / 广播；
/// - `peers`：列出当前已连接节点（快照）；
/// - `disconnect`：优雅断开（先发 NodeBye 再关 socket）。
#[async_trait]
pub trait P2pTransport: Send + Sync {
    /// 启动监听（作为 server，接收其他节点连接）。
    async fn listen(&self, addr: &str) -> Result<(), ImError>;

    /// 连接到远端节点（作为 client），握手完成后返回 [`PeerNode`]。
    async fn connect(&self, addr: &str) -> Result<PeerNode, ImError>;

    /// 发送消息到指定节点（点对点；节点未连接返回 [`ImError::Disconnected`]）。
    async fn send(&self, node_id: &str, msg: TransportMessage) -> Result<(), ImError>;

    /// 广播消息到所有已连接节点（`msg.to_node` 应为 `None`）。
    async fn broadcast(&self, msg: TransportMessage) -> Result<(), ImError>;

    /// 列出当前已连接的节点（快照，可能瞬态过时）。
    async fn peers(&self) -> Vec<PeerNode>;

    /// 断开指定节点（优雅关闭：先发 NodeBye，再关 socket）。
    async fn disconnect(&self, node_id: &str) -> Result<(), ImError>;
}

// ============================================================================
// 单测
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use os_core::DateTime;
    use serde_json::json;

    // ---- TransportMessage serde 往返 ----

    #[test]
    fn transport_message_serde_roundtrip_direct() {
        let msg = TransportMessage::direct(
            "node-A",
            "node-B",
            TransportMsgType::Chat,
            json!({"text": "hello from A"}),
        )
        .with_group("group-1")
        .with_signature("sig-abc");

        let json_str = serde_json::to_string(&msg).expect("serialize");
        let back: TransportMessage = serde_json::from_str(&json_str).expect("deserialize");

        assert_eq!(back.from_node, "node-A");
        assert_eq!(back.to_node.as_deref(), Some("node-B"));
        assert_eq!(back.group_id.as_deref(), Some("group-1"));
        assert_eq!(back.message_type, TransportMsgType::Chat);
        assert_eq!(back.payload, json!({"text": "hello from A"}));
        assert_eq!(back.signature.as_deref(), Some("sig-abc"));
        assert!(!back.is_broadcast());
        // timestamp 序列化无损（同值）
        assert_eq!(back.timestamp, msg.timestamp);
    }

    #[test]
    fn transport_message_serde_roundtrip_broadcast() {
        let msg = TransportMessage::broadcast("node-A", TransportMsgType::NodeHeartbeat, json!({}));
        let json_str = serde_json::to_string(&msg).expect("serialize");
        let back: TransportMessage = serde_json::from_str(&json_str).expect("deserialize");
        assert!(back.is_broadcast());
        assert!(back.to_node.is_none());
        assert!(back.group_id.is_none());
        assert!(back.signature.is_none());
        assert_eq!(back.message_type, TransportMsgType::NodeHeartbeat);
    }

    #[test]
    fn transport_message_to_node_none_serializes_as_null() {
        // 广播消息的 to_node 应为 null（而非被 serde skip）
        let msg = TransportMessage::broadcast("n1", TransportMsgType::NodeBye, json!(null));
        let v: serde_json::Value = serde_json::to_value(&msg).expect("to_value");
        assert!(v.get("to_node").unwrap().is_null());
        assert!(v.get("signature").unwrap().is_null());
    }

    // ---- TransportMsgType 全变体 ----

    #[test]
    fn transport_msg_type_all_variants_serde_roundtrip() {
        for &variant in TransportMsgType::ALL {
            let s = serde_json::to_string(&variant).expect("serialize variant");
            let back: TransportMsgType = serde_json::from_str(&s).expect("deserialize variant");
            assert_eq!(variant, back, "serde roundtrip failed for {:?}", variant);
        }
    }

    #[test]
    fn transport_msg_type_snake_case_serialization() {
        // 确保字符串形态是 snake_case（前端 / 跨语言协议契约）
        let cases = [
            (TransportMsgType::Chat, "\"chat\""),
            (TransportMsgType::GroupCreate, "\"group_create\""),
            (TransportMsgType::GroupJoin, "\"group_join\""),
            (TransportMsgType::GroupLeave, "\"group_leave\""),
            (TransportMsgType::NodeHello, "\"node_hello\""),
            (TransportMsgType::NodeHeartbeat, "\"node_heartbeat\""),
            (TransportMsgType::NodeBye, "\"node_bye\""),
        ];
        for (variant, expected) in cases {
            assert_eq!(
                serde_json::to_string(&variant).unwrap(),
                expected,
                "snake_case mismatch for {:?}",
                variant
            );
        }
    }

    #[test]
    fn transport_msg_type_is_node_control() {
        assert!(!TransportMsgType::Chat.is_node_control());
        assert!(TransportMsgType::NodeHello.is_node_control());
        assert!(TransportMsgType::NodeHeartbeat.is_node_control());
        assert!(TransportMsgType::NodeBye.is_node_control());
    }

    #[test]
    fn transport_msg_type_display_matches_snake_case() {
        for &variant in TransportMsgType::ALL {
            let display = variant.to_string();
            let serde_str = serde_json::to_string(&variant).unwrap();
            // serde_str 带引号，display 不带
            assert_eq!(format!("\"{}\"", display), serde_str);
        }
    }

    #[test]
    fn transport_msg_type_all_count_is_seven() {
        assert_eq!(
            TransportMsgType::ALL.len(),
            7,
            "expected exactly 7 variants"
        );
    }

    // ---- PeerNode 构造 + Display ----

    #[test]
    fn peer_node_new_defaults_connected_with_now_last_seen() {
        let before: DateTime = now();
        let peer = PeerNode::new("node-X", "1.2.3.4:5678", "LivingRoom");
        let after: DateTime = now();

        assert_eq!(peer.node_id, "node-X");
        assert_eq!(peer.addr, "1.2.3.4:5678");
        assert_eq!(peer.display_name, "LivingRoom");
        assert!(peer.connected);
        // last_seen 在构造窗口内
        assert!(peer.last_seen >= before && peer.last_seen <= after);
    }

    #[test]
    fn peer_node_mark_disconnected_flips_flag_and_touch_updates_last_seen() {
        let mut peer = PeerNode::new("node-X", "1.2.3.4:5678", "LivingRoom");
        let original = peer.last_seen;

        // 触发 touch 让 last_seen 推进（与 original 相比 >=，同 ns 不变也算正常）
        peer.touch();
        assert!(peer.last_seen >= original);

        peer.mark_disconnected();
        assert!(!peer.connected);
    }

    #[test]
    fn peer_node_display_contains_id_addr_state() {
        let peer = PeerNode::new("node-X", "1.2.3.4:5678", "LivingRoom");
        let s = format!("{}", peer);
        assert!(s.contains("node-X"), "display missing node_id: {s}");
        assert!(s.contains("1.2.3.4:5678"), "display missing addr: {s}");
        assert!(s.contains("LivingRoom"), "display missing name: {s}");
        assert!(s.contains("online"), "online state missing: {s}");

        let mut offline = peer.clone();
        offline.mark_disconnected();
        let s2 = format!("{}", offline);
        assert!(s2.contains("offline"), "offline state missing: {s2}");
    }

    #[test]
    fn peer_node_serde_roundtrip() {
        let peer = PeerNode::new("node-X", "1.2.3.4:5678", "LivingRoom");
        let s = serde_json::to_string(&peer).expect("serialize");
        let back: PeerNode = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back.node_id, peer.node_id);
        assert_eq!(back.addr, peer.addr);
        assert_eq!(back.display_name, peer.display_name);
        assert_eq!(back.connected, peer.connected);
        assert_eq!(back.last_seen, peer.last_seen);
    }

    // ---- defaults 常量 ----

    #[test]
    fn defaults_heartbeat_interval_is_30s() {
        assert_eq!(defaults::HEARTBEAT_INTERVAL, Duration::from_secs(30));
    }

    #[test]
    fn defaults_frame_size_and_header_sane() {
        assert_eq!(defaults::LENGTH_HEADER_BYTES, 4);
        // MAX_FRAME_SIZE 是编译期常量；用 const 块做编译期断言（clippy 友好）
        const _: () = assert!(defaults::MAX_FRAME_SIZE == 16 * 1024 * 1024);
    }
}
