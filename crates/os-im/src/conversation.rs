//! 对话与消息（规划文档 §3.7 入口）
//!
//! "对话即操作"：用户在 IM 内自然语言交互，LLM 解析为 Tool 调用，由 agent 执行。

use os_core::{DateTime, Uuid};
use serde::{Deserialize, Serialize};

use crate::tool::ToolCall;

// ----------------------------------------------------------------------------
// 对话 ID 与角色
// ----------------------------------------------------------------------------

/// 对话 ID（newtype Uuid）
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConversationId(pub Uuid);

impl ConversationId {
    /// 生成新对话 ID
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}
impl Default for ConversationId {
    fn default() -> Self {
        Self::new()
    }
}
impl std::fmt::Display for ConversationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 消息角色
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    /// 用户
    User,
    /// 助手（LLM）
    Assistant,
    /// 系统提示
    System,
    /// 工具调用结果
    Tool,
}

// ----------------------------------------------------------------------------
// 消息
// ----------------------------------------------------------------------------

/// 单条对话消息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// 消息 ID（客户端生成或后端生成）
    pub id: String,
    /// 所属对话
    pub conversation: ConversationId,
    /// 角色
    pub role: MessageRole,
    /// 文本内容
    pub content: String,
    /// LLM 发起的工具调用（仅 Assistant 角色可能携带）
    pub tool_calls: Vec<ToolCall>,
    /// 时间戳（UTC）
    pub timestamp: DateTime,
}

// ----------------------------------------------------------------------------
// ConversationStore trait（async）
// ----------------------------------------------------------------------------

/// 对话存储——消息持久化与检索。
///
/// 实现者：`SqliteConversationStore`（默认，随 meta SQLite 复制 HA）。
#[allow(async_fn_in_trait)]
pub trait ConversationStore: Send + Sync {
    /// 创建新对话，返回对话 ID。
    async fn create_conversation(&self, user: &str) -> Result<ConversationId, crate::ImError>;

    /// 追加一条消息。
    async fn add_message(&self, msg: Message) -> Result<(), crate::ImError>;

    /// 取最近 `limit` 条历史消息（按时间升序）。
    async fn history(
        &self,
        conv: &ConversationId,
        limit: u32,
    ) -> Result<Vec<Message>, crate::ImError>;

    /// 列出某用户参与的全部对话。
    async fn list_conversations(&self, user: &str) -> Result<Vec<ConversationId>, crate::ImError>;
}
