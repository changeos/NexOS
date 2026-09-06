//! Mock 实现（feature gate `mock`）——供下游 agent（api/client）测试用。
//!
//! 约定（_conventions.md §5）：
//! - 实现完整 trait（不 panic 的默认返回）
//! - 提供 builder 构造器设置预期返回值
//! - 纯内存、确定性
//!
//! 7 个 Mock：`MockLlmBackend` / `MockAgent` / `MockConversationStore` /
//! `MockSharedContext` / `MockConfirmationGate` / `MockTool` / `MockAgentOrchestrator`。

#![cfg(feature = "mock")]

use std::sync::Mutex;

use async_trait::async_trait;

use crate::agent::{Agent, AgentCapability, AgentId, AgentTask, AgentTaskResult};
use crate::blackboard::{BlackboardEntry, SharedContext};
use crate::confirmation::{ConfirmationGate, ConfirmationRequest, ConfirmationStatus, VoteRecord};
use crate::conversation::{ConversationId, ConversationStore, Message};
use crate::llm::{LlmBackend, LlmBackendType, LlmRequest, LlmResponse, TokenUsage};
use crate::orchestrator::{AgentOrchestrator, OrchestrationStatus};
use crate::tool::{Tool, ToolCall, ToolCategory, ToolDescriptor, ToolResult};
use crate::ImError;
use os_core::{HealthReport, TaskId};

// ============================================================
// MockLlmBackend
// ============================================================

/// Mock `LlmBackend`——返回预设响应。
pub struct MockLlmBackend {
    backend_type: LlmBackendType,
    response: Mutex<Option<LlmResponse>>,
    models: Mutex<Vec<String>>,
}

impl MockLlmBackend {
    /// 创建默认 mock（Cloud，空 tool_calls 回复，模型列表含一个占位）。
    pub fn new() -> Self {
        Self {
            backend_type: LlmBackendType::Cloud,
            response: Mutex::new(Some(LlmResponse {
                content: "(mock reply)".to_string(),
                tool_calls: Vec::new(),
                usage: TokenUsage {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    total_tokens: 2,
                },
                finish_reason: "stop".to_string(),
            })),
            models: Mutex::new(vec!["mock-model".to_string()]),
        }
    }

    /// 注入预期 chat 响应。
    pub fn with_response(self, resp: LlmResponse) -> Self {
        *self.response.lock().unwrap() = Some(resp);
        self
    }

    /// 注入模型列表。
    pub fn with_models(self, models: Vec<String>) -> Self {
        *self.models.lock().unwrap() = models;
        self
    }

    /// 设置后端类型。
    pub fn with_backend_type(mut self, t: LlmBackendType) -> Self {
        self.backend_type = t;
        self
    }
}

impl Default for MockLlmBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LlmBackend for MockLlmBackend {
    async fn chat(&self, _req: LlmRequest) -> Result<LlmResponse, ImError> {
        self.response
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| ImError::LlmError("mock 未配置响应".to_string()))
    }

    async fn list_models(&self) -> Result<Vec<String>, ImError> {
        Ok(self.models.lock().unwrap().clone())
    }

    async fn backend_type(&self) -> LlmBackendType {
        self.backend_type.clone()
    }
}

// ============================================================
// MockTool
// ============================================================

/// Mock `Tool`——可配置 descriptor / 可用性 / 返回结果。
pub struct MockTool {
    descriptor: ToolDescriptor,
    available: Mutex<bool>,
    /// invoke 调用次数（测试断言用）
    invoke_count: Mutex<u32>,
    result: Mutex<ToolResult>,
}

impl MockTool {
    /// 用 name 构造一个最小 mock（Storage 分类，非常驻）。
    pub fn new(name: impl Into<String>) -> Self {
        let descriptor = ToolDescriptor {
            name: name.into(),
            description: "mock tool".to_string(),
            parameters_schema: serde_json::json!({}),
            category: ToolCategory::Storage,
            requires_confirmation: false,
            conditionally_activated: None,
        };
        Self {
            descriptor,
            available: Mutex::new(true),
            invoke_count: Mutex::new(0),
            result: Mutex::new(ToolResult {
                call_id: String::new(),
                success: true,
                output: serde_json::json!({}),
                error: None,
            }),
        }
    }

    /// 设置可用性（条件激活型测试）。
    pub fn set_available(&self, v: bool) {
        *self.available.lock().unwrap() = v;
    }

    /// 设置 descriptor（含分类/确认标记等）。
    pub fn with_descriptor(self, d: ToolDescriptor) -> Self {
        Self {
            descriptor: d,
            ..self
        }
    }

    /// 设置 invoke 返回结果（不含 call_id，运行期回填调用 ID）。
    pub fn with_result(self, success: bool, output: serde_json::Value) -> Self {
        *self.result.lock().unwrap() = ToolResult {
            call_id: String::new(),
            success,
            output,
            error: if success {
                None
            } else {
                Some("mock failure".to_string())
            },
        };
        self
    }

    /// 取 invoke 调用次数。
    pub fn invoke_count(&self) -> u32 {
        *self.invoke_count.lock().unwrap()
    }
}

#[async_trait]
impl Tool for MockTool {
    async fn descriptor(&self) -> ToolDescriptor {
        self.descriptor.clone()
    }

    async fn invoke(&self, call: &ToolCall) -> Result<ToolResult, ImError> {
        *self.invoke_count.lock().unwrap() += 1;
        let mut r = self.result.lock().unwrap().clone();
        r.call_id = call.call_id.clone();
        Ok(r)
    }

    async fn is_available(&self) -> bool {
        *self.available.lock().unwrap()
    }
}

// ============================================================
// MockAgent
// ============================================================

/// Mock `Agent`——返回预设能力与任务结果。
pub struct MockAgent {
    id: AgentId,
    capability: AgentCapability,
    result_success: bool,
    result_output: serde_json::Value,
    healthy: bool,
}

impl MockAgent {
    /// 用 id + domain + tools 构造 mock agent（默认健康、成功返回）。
    pub fn new(id: impl Into<String>, domain: impl Into<String>, tools: Vec<String>) -> Self {
        let id = AgentId::new(id.into());
        let capability = AgentCapability {
            agent_id: id.clone(),
            tools,
            domain: domain.into(),
        };
        Self {
            id,
            capability,
            result_success: true,
            result_output: serde_json::json!({"mock": true}),
            healthy: true,
        }
    }

    /// 设置 handle_task 成功与否。
    pub fn with_result(mut self, success: bool, output: serde_json::Value) -> Self {
        self.result_success = success;
        self.result_output = output;
        self
    }

    /// 设置健康状态。
    pub fn with_health(mut self, healthy: bool) -> Self {
        self.healthy = healthy;
        self
    }
}

#[async_trait]
impl Agent for MockAgent {
    async fn id(&self) -> AgentId {
        self.id.clone()
    }

    async fn capabilities(&self) -> AgentCapability {
        self.capability.clone()
    }

    async fn handle_task(&self, task: &AgentTask) -> Result<AgentTaskResult, ImError> {
        Ok(AgentTaskResult {
            task_id: task.id,
            success: self.result_success,
            output: self.result_output.clone(),
            side_effects: Vec::new(),
        })
    }

    async fn health(&self) -> HealthReport {
        use os_core::Health;
        HealthReport {
            health: if self.healthy {
                Health::Healthy
            } else {
                Health::Unhealthy
            },
            message: if self.healthy {
                None
            } else {
                Some("mock unhealthy".to_string())
            },
            timestamp: chrono::Utc::now(),
        }
    }
}

// ============================================================
// MockConversationStore
// ============================================================

/// Mock `ConversationStore`——与 [`crate::impls::InMemoryConversationStore`] 等价的内存实现，
/// 单独提供以便下游替换注入。
pub struct MockConversationStore {
    inner: crate::impls::InMemoryConversationStore,
}

impl MockConversationStore {
    /// 创建空存储。
    pub fn new() -> Self {
        Self {
            inner: crate::impls::InMemoryConversationStore::new(),
        }
    }
}

impl Default for MockConversationStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ConversationStore for MockConversationStore {
    async fn create_conversation(&self, user: &str) -> Result<ConversationId, ImError> {
        self.inner.create_conversation(user).await
    }
    async fn add_message(&self, msg: Message) -> Result<(), ImError> {
        self.inner.add_message(msg).await
    }
    async fn history(&self, conv: &ConversationId, limit: u32) -> Result<Vec<Message>, ImError> {
        self.inner.history(conv, limit).await
    }
    async fn list_conversations(&self, user: &str) -> Result<Vec<ConversationId>, ImError> {
        self.inner.list_conversations(user).await
    }
}

// ============================================================
// MockSharedContext
// ============================================================

/// Mock `SharedContext`——与 [`crate::impls::InMemoryBlackboard`] 等价的内存黑板。
pub struct MockSharedContext {
    inner: crate::impls::InMemoryBlackboard,
}

impl MockSharedContext {
    /// 创建空黑板。
    pub fn new() -> Self {
        Self {
            inner: crate::impls::InMemoryBlackboard::new(),
        }
    }
}

impl Default for MockSharedContext {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SharedContext for MockSharedContext {
    async fn put(
        &self,
        key: &str,
        value: serde_json::Value,
        writer: &AgentId,
    ) -> Result<(), ImError> {
        self.inner.put(key, value, writer).await
    }
    async fn get(&self, key: &str) -> Option<BlackboardEntry> {
        self.inner.get(key).await
    }
    async fn list(&self) -> Vec<BlackboardEntry> {
        self.inner.list().await
    }
    async fn clear_for_task(&self, task: &TaskId) {
        self.inner.clear_for_task(task).await
    }
}

// ============================================================
// MockConfirmationGate
// ============================================================

/// Mock `ConfirmationGate`——包装 [`crate::impls::DefaultConfirmationGate`]，暴露 quorum 配置。
pub struct MockConfirmationGate {
    inner: crate::impls::DefaultConfirmationGate,
}

impl MockConfirmationGate {
    /// 默认（Critical 法定 2）。
    pub fn new() -> Self {
        Self {
            inner: crate::impls::DefaultConfirmationGate::new(),
        }
    }

    /// 自定义法定人数。
    pub fn with_quorum(quorum: usize) -> Self {
        Self {
            inner: crate::impls::DefaultConfirmationGate::with_quorum(quorum),
        }
    }

    /// 取法定人数。
    pub fn quorum(&self) -> usize {
        self.inner.quorum()
    }
}

impl Default for MockConfirmationGate {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ConfirmationGate for MockConfirmationGate {
    async fn request(&self, req: ConfirmationRequest) -> Result<String, ImError> {
        self.inner.request(req).await
    }
    async fn user_confirm(&self, req_id: &str, approved: bool) -> Result<(), ImError> {
        self.inner.user_confirm(req_id, approved).await
    }
    async fn agent_vote(&self, req_id: &str, vote: VoteRecord) -> Result<(), ImError> {
        self.inner.agent_vote(req_id, vote).await
    }
    async fn status(&self, req_id: &str) -> ConfirmationStatus {
        self.inner.status(req_id).await
    }
}

// ============================================================
// MockAgentOrchestrator
// ============================================================

/// Mock `AgentOrchestrator`——包装 [`crate::impls::CentralOrchestrator`]，
/// 提供下游注入与能力发现测试入口。
pub struct MockAgentOrchestrator {
    inner: crate::impls::CentralOrchestrator,
}

impl MockAgentOrchestrator {
    /// 创建空中枢。
    pub fn new() -> Self {
        Self {
            inner: crate::impls::CentralOrchestrator::new(),
        }
    }

    /// 按 Tool name 路由（能力发现）。
    pub async fn route_by_tool(&self, tool_name: &str) -> Option<AgentId> {
        self.inner.route_by_tool(tool_name).await
    }

    /// 按 domain 路由。
    pub async fn route_by_domain(&self, domain: &str) -> Option<AgentId> {
        self.inner.route_by_domain(domain).await
    }

    /// 注册依赖边并做无环检测（红线：环 → TaskCycle）。
    pub fn add_dependencies(
        &self,
        task: TaskId,
        deps: impl IntoIterator<Item = TaskId>,
    ) -> Result<(), ImError> {
        self.inner.add_dependencies(task, deps)
    }

    /// 取任务依赖。
    pub fn dependencies_of(&self, task: &TaskId) -> std::collections::HashSet<TaskId> {
        self.inner.dependencies_of(task)
    }
}

impl Default for MockAgentOrchestrator {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentOrchestrator for MockAgentOrchestrator {
    async fn register_agent(&self, agent: Box<dyn Agent>) -> Result<(), ImError> {
        self.inner.register_agent(agent).await
    }
    async fn unregister_agent(&self, id: &AgentId) {
        self.inner.unregister_agent(id).await
    }
    async fn list_agents(&self) -> Vec<AgentCapability> {
        self.inner.list_agents().await
    }
    async fn delegate(&self, task: AgentTask) -> Result<TaskId, ImError> {
        self.inner.delegate(task).await
    }
    async fn task_status(&self, task: &TaskId) -> OrchestrationStatus {
        self.inner.task_status(task).await
    }
}

// ============================================================
// 单元测——Mock builder 行为（feature gate mock）
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentTask;
    use crate::tool::ToolCall;
    use os_core::TaskId;

    #[tokio::test]
    async fn mock_llm_backend_returns_configured_response() {
        let backend = MockLlmBackend::new()
            .with_backend_type(LlmBackendType::Local)
            .with_response(LlmResponse {
                content: "hi".to_string(),
                tool_calls: Vec::new(),
                usage: TokenUsage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                },
                finish_reason: "stop".to_string(),
            });
        assert_eq!(backend.backend_type().await, LlmBackendType::Local);
        let resp = backend
            .chat(LlmRequest {
                messages: Vec::new(),
                model: None,
                temperature: None,
                max_tokens: None,
                tools: Vec::new(),
            })
            .await
            .unwrap();
        assert_eq!(resp.content, "hi");
        assert_eq!(resp.usage.total_tokens, 15);
        // with_models
        let b = MockLlmBackend::new().with_models(vec!["m1".into(), "m2".into()]);
        assert_eq!(b.list_models().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn mock_tool_invoke_counts_and_backfills_call_id() {
        let tool = MockTool::new("storage.snapshot.create")
            .with_result(true, serde_json::json!({"ok": 1}));
        assert!(tool.is_available().await);
        tool.set_available(false);
        assert!(!tool.is_available().await);

        let call = ToolCall {
            tool_name: "storage.snapshot.create".to_string(),
            arguments: serde_json::json!({}),
            call_id: "call-1".to_string(),
        };
        let res = tool.invoke(&call).await.unwrap();
        assert!(res.success);
        assert_eq!(res.call_id, "call-1"); // 运行期回填
        assert_eq!(res.output, serde_json::json!({"ok": 1}));
        assert_eq!(tool.invoke_count(), 1);
        // descriptor 可取
        let desc = tool.descriptor().await;
        assert_eq!(desc.name, "storage.snapshot.create");
    }

    #[tokio::test]
    async fn mock_agent_handle_task_and_health() {
        let agent = MockAgent::new("wallet-agent", "wallet", vec!["wallet.sign".into()])
            .with_result(true, serde_json::json!({"signed": true}))
            .with_health(false);
        assert_eq!(agent.id().await.as_str(), "wallet-agent");
        let cap = agent.capabilities().await;
        assert_eq!(cap.tools, vec!["wallet.sign".to_string()]);
        let task = AgentTask {
            id: TaskId::new(),
            assigned_to: AgentId::new("wallet-agent"),
            description: "sign tx".to_string(),
            context: serde_json::json!({}),
            deadline: None,
        };
        let res = agent.handle_task(&task).await.unwrap();
        assert!(res.success);
        // 健康 = Unhealthy
        use os_core::Health;
        assert_eq!(agent.health().await.health, Health::Unhealthy);
    }

    #[tokio::test]
    async fn mock_shared_context_delegates_to_inmemory() {
        let bb = MockSharedContext::new();
        let writer = AgentId::new("a");
        bb.put("k", serde_json::json!(1), &writer).await.unwrap();
        assert_eq!(bb.get("k").await.unwrap().value, serde_json::json!(1));
        assert_eq!(bb.list().await.len(), 1);
    }

    #[tokio::test]
    async fn mock_confirmation_gate_quorum_configurable() {
        let gate = MockConfirmationGate::with_quorum(3);
        assert_eq!(gate.quorum(), 3);
        let default_gate = MockConfirmationGate::new();
        assert_eq!(default_gate.quorum(), 2);
    }

    #[tokio::test]
    async fn mock_agent_orchestrator_routes_and_delegates() {
        let orch = MockAgentOrchestrator::new();
        let mock = MockAgent::new("net-agent", "network", vec!["net.iface.up".into()]);
        orch.register_agent(Box::new(mock)).await.unwrap();
        assert_eq!(
            orch.route_by_tool("net.iface.up").await,
            Some(AgentId::new("net-agent"))
        );
        let tid = TaskId::new();
        let task = AgentTask {
            id: tid,
            assigned_to: AgentId::new("net-agent"),
            description: "up".to_string(),
            context: serde_json::json!({}),
            deadline: None,
        };
        orch.delegate(task).await.unwrap();
        assert!(matches!(
            orch.task_status(&tid).await,
            OrchestrationStatus::Completed { .. }
        ));
    }

    #[test]
    fn mock_orchestrator_cycle_detection_exposed() {
        let orch = MockAgentOrchestrator::new();
        let a = TaskId::new();
        let b = TaskId::new();
        orch.add_dependencies(a, [b]).unwrap();
        let err = orch.add_dependencies(b, [a]).unwrap_err();
        assert!(matches!(err, ImError::TaskCycle(_)));
    }
}
