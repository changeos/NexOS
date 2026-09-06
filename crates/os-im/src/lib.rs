//! os-im —— 核心 IM + 多 Agent 协作中枢（接口契约）
//!
//! 定位（规划文档 §3.7）：
//! - 对话即操作入口（用户自然语言 → LLM → Tool 调用 → agent 执行）
//! - AI agent 宿主 + 多 agent 协作运行时
//! - 协作原语：能力发现 / 任务委派（无环）/ 上下文黑板 / 确认与投票 / 结果聚合
//!
//! 本 crate 仅定义契约（trait + 数据结构 + Error），实现由 owner agent 后续填充。
//! Tool 可调用各执行组件，但 trait 层不硬依赖具体 crate——通过 `Box<dyn Tool>` 注入。
//!
//! 契约规范：默认原生 `async fn in trait`；凡需 `Box<dyn XxxTrait>` 运行期多态的
//! async trait 改用 `#[async_trait]`（dyn 兼容性修正，如 `Agent`）；
//! lib 顶部统一 `#![allow(async_fn_in_trait)]`；自定义 `ImError`，
//! 并实现 `From<ImError> for os_common::ApiError` 以统一对外错误。

#![allow(async_fn_in_trait)]

pub mod agent;
pub mod blackboard;
pub mod confirmation;
pub mod conversation;
pub mod error;
pub mod federation;
pub mod federation_impl;
pub mod group;
pub mod group_impl;
pub mod impls;
pub mod llm;
pub mod orchestrator;
pub mod tool;
pub mod transport;
pub mod transport_impl;

/// Mock 实现（feature gate `mock`，供下游 api/client 测试）。
#[cfg(feature = "mock")]
pub mod mock;
#[cfg(feature = "mock")]
pub use mock::{
    MockAgent, MockAgentOrchestrator, MockConfirmationGate, MockConversationStore, MockLlmBackend,
    MockSharedContext, MockTool,
};

pub use agent::{Agent, AgentCapability, AgentId, AgentTask, AgentTaskResult};
pub use blackboard::{BlackboardEntry, SharedContext};
pub use confirmation::{
    ConfirmationGate, ConfirmationRequest, ConfirmationStatus, RiskLevel, VoteRecord,
};
pub use conversation::{ConversationId, ConversationStore, Message, MessageRole};
pub use error::{ImError, ImResult};
pub use federation::{FederationHandshake, FederationManager, FederationNode, NodeCapabilities};
pub use federation_impl::{LocalFederationManager, LocalNodeIdentity};
pub use group::{Group, GroupId, GroupManager, GroupMember, MemberType};
pub use group_impl::InMemoryGroupManager;
pub use llm::{LlmBackend, LlmBackendType, LlmRequest, LlmResponse, TokenUsage};
pub use orchestrator::{AgentOrchestrator, OrchestrationStatus};
pub use tool::{Tool, ToolCall, ToolCategory, ToolDescriptor, ToolResult};
pub use transport::{
    defaults as transport_defaults, MessageHandler, P2pTransport, PeerNode, TransportMessage,
    TransportMsgType,
};
pub use transport_impl::TcpP2pTransport;
