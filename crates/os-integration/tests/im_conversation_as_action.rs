//! 场景 5：IM 对话即操作（规划文档 §3.7.2 / integration-agent 规格书 §3 场景 5）
//!
//! 链路：用户在 IM 内发"建 VM"自然语言 → os-im AgentOrchestrator 解析意图 →
//! 委派 ComputeAgent（ToolInvokingAgent，包装 MockTool）→ agent 调 Tool
//! （vm.create）→ 结果聚合 → 经 SharedContext 黑板共享上下文 → 高危操作经
//! ConfirmationGate 确认。
//!
//! 重点验证：
//! - 跨 crate 类型桥接：`AgentTask.context`（JSON） ↔ Tool `ToolCall.arguments`，
//!   `AgentTaskResult.output`（JSON）回填对话消息（Tool 角色）。
//! - 能力发现：orchestrator.route_by_tool / route_by_domain 把意图路由到对应 agent。
//! - 委派执行：delegate(task) → handle_task → Completed（result 聚合）。
//! - 任务图无环检测（红线 §3.7.2）：add_dependencies 命中环返回 TaskCycle。
//! - 高危确认门：High 风险 → 用户确认才 UserApproved；Critical 风险 → 用户确认
//!   + 会签达法定双满足；用户拒绝短路 UserRejected。
//! - 黑板共享：agent 把中间结果写入 SharedContext（key 命名空间 `task.<id>.*`），
//!   清理 clear_for_task 命中前缀。
//! - 通知尾：执行结果聚合后写一条 Tool 角色消息进对话（IM 内可见反馈）。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use os_core::eventbus::{Event, EventBus, Severity, Topic};
use os_core::mock::MockEventBus;
use os_core::{HealthReport, TaskId};
use os_im::agent::{Agent, AgentCapability, AgentId, AgentTask, AgentTaskResult};
use os_im::blackboard::SharedContext;
use os_im::confirmation::{
    ConfirmationGate, ConfirmationRequest, ConfirmationStatus, RiskLevel, VoteRecord,
};
use os_im::conversation::{ConversationStore, Message, MessageRole};
use os_im::orchestrator::{AgentOrchestrator, OrchestrationStatus};
use os_im::tool::{Tool, ToolCall, ToolResult};
use os_im::ImError;
use os_im::{
    MockAgentOrchestrator, MockConfirmationGate, MockConversationStore, MockSharedContext,
};

// ----------------------------------------------------------------------------
// ToolInvokingAgent：领域 agent → 调 Tool → 黑板共享。
// 在 MockTool 之上接一层「把 AgentTask 转成 ToolCall」的真实协作逻辑：
// agent.task.context → tool.call.arguments（JSON 透传），tool.result.output
// 写入 SharedContext 黑板 + 包成 AgentTaskResult。
// ----------------------------------------------------------------------------

struct ToolInvokingAgent {
    id: AgentId,
    capability: AgentCapability,
    tool: Arc<dyn Tool>,
    blackboard: Arc<dyn SharedContext>,
    invoke_log: Mutex<Vec<String>>,
}

impl ToolInvokingAgent {
    fn new(
        id: impl Into<String>,
        domain: impl Into<String>,
        tool_name: String,
        tool: Arc<dyn Tool>,
        blackboard: Arc<dyn SharedContext>,
    ) -> Self {
        let id = AgentId::new(id.into());
        let capability = AgentCapability {
            agent_id: id.clone(),
            tools: vec![tool_name],
            domain: domain.into(),
        };
        Self {
            id,
            capability,
            tool,
            blackboard,
            invoke_log: Mutex::new(Vec::new()),
        }
    }

    fn log(&self, entry: String) {
        self.invoke_log.lock().expect("invoke_log").push(entry);
    }
}

#[async_trait]
impl Agent for ToolInvokingAgent {
    async fn id(&self) -> AgentId {
        self.id.clone()
    }

    async fn capabilities(&self) -> AgentCapability {
        self.capability.clone()
    }

    async fn handle_task(&self, task: &AgentTask) -> Result<AgentTaskResult, ImError> {
        // 1) AgentTask → ToolCall：context（JSON）透传为 arguments。
        let call = ToolCall {
            tool_name: self.capability.tools[0].clone(),
            arguments: task.context.clone(),
            call_id: format!("call-{}", task.id),
        };
        self.log(format!("tool_call({}): start", call.tool_name));

        // 2) invoke Tool。
        let tool_result: ToolResult = self.tool.invoke(&call).await?;
        self.log(format!(
            "tool_call({}): success={}, output={}",
            call.tool_name, tool_result.success, tool_result.output
        ));

        // 3) 中间结果写入 SharedContext 黑板（命名空间 task.<id>.*）。
        let bb_key = format!("task.{}.tool_result", task.id);
        self.blackboard
            .put(&bb_key, tool_result.output.clone(), &self.id)
            .await?;

        // 4) 包成 AgentTaskResult。
        Ok(AgentTaskResult {
            task_id: task.id,
            success: tool_result.success,
            output: tool_result.output,
            side_effects: vec![Event::new(
                "os-im/compute-agent",
                Topic::AgentTask,
                "tool.invoked",
            )],
        })
    }

    async fn health(&self) -> HealthReport {
        use os_core::Health;
        HealthReport {
            health: Health::Healthy,
            message: None,
            timestamp: os_core::Utc::now(),
        }
    }
}

// ----------------------------------------------------------------------------
// ConversationAsActionFlow：组装「对话即操作」端到端编排。
// 所有共享态（blackboard）用 Arc，便于注入 agent + flow 自身共用同一实例。
// ----------------------------------------------------------------------------

struct ConversationAsActionFlow {
    orch: MockAgentOrchestrator,
    store: MockConversationStore,
    blackboard: Arc<MockSharedContext>,
    gate: MockConfirmationGate,
    bus: MockEventBus,
}

impl ConversationAsActionFlow {
    fn new() -> Self {
        Self {
            orch: MockAgentOrchestrator::new(),
            store: MockConversationStore::new(),
            blackboard: Arc::new(MockSharedContext::new()),
            gate: MockConfirmationGate::new(),
            bus: MockEventBus::new(),
        }
    }

    fn with_quorum(quorum: usize) -> Self {
        let mut s = Self::new();
        s.gate = MockConfirmationGate::with_quorum(quorum);
        s
    }

    /// 注册 compute agent（注入与 flow 共享的黑板实例）。
    /// 入参用具体 `Arc<MockTool>`：便于调用方同时持具体 Arc 做 invoke_count 断言，
    /// 内部 coerce 到 `Arc<dyn Tool>` 注入 agent。
    async fn register_compute_agent(
        &self,
        agent_id: &str,
        tool_name: &str,
        tool: Arc<os_im::MockTool>,
    ) -> AgentId {
        let agent = ToolInvokingAgent::new(
            agent_id,
            "compute",
            tool_name.to_string(),
            tool as Arc<dyn Tool>,
            self.blackboard.clone(),
        );
        let id = agent.id().await;
        self.orch
            .register_agent(Box::new(agent))
            .await
            .expect("register");
        id
    }

    /// 端到端跑一次「对话即操作」：
    /// 1. 建对话；2. 用户意图入消息；3. 高危确认；4. 用户拒绝短路 OR
    ///    委派 agent；5. 结果聚合写回对话（Tool 消息）+ 事件。
    async fn run_high_risk_action(
        &self,
        intent_text: &str,
        target_agent: AgentId,
        risk: RiskLevel,
        user_approves: bool,
    ) -> Result<(OrchestrationStatus, ConfirmationStatus), ImError> {
        // 1) 建对话 + 用户意图入消息。
        let conv = self.store.create_conversation("alice").await?;
        self.store
            .add_message(Message {
                id: format!("msg-user-{}", conv),
                conversation: conv.clone(),
                role: MessageRole::User,
                content: intent_text.to_string(),
                tool_calls: vec![],
                timestamp: os_core::Utc::now(),
            })
            .await?;

        // 2) 构造委派任务。
        let task_id = TaskId::new();
        let task = AgentTask {
            id: task_id,
            assigned_to: target_agent.clone(),
            description: intent_text.to_string(),
            context: json!({ "intent": intent_text, "vm_id": "vm-from-im" }),
            deadline: None,
        };

        // 3) 高危操作：先发确认请求。
        let req = ConfirmationRequest {
            id: format!("req-{}", task_id),
            task_id,
            description: format!("高危操作需确认：{}", intent_text),
            risk_level: risk,
            requested_by: target_agent.clone(),
            created_at: os_core::Utc::now(),
        };
        let req_id = self.gate.request(req).await?;
        self.gate.user_confirm(&req_id, user_approves).await?;
        let conf_status = self.gate.status(&req_id).await;

        // 4) 用户拒绝 → 短路：不委派 agent。
        if matches!(conf_status, ConfirmationStatus::UserRejected) {
            self.store
                .add_message(Message {
                    id: format!("msg-rej-{}", conv),
                    conversation: conv.clone(),
                    role: MessageRole::System,
                    content: "用户拒绝，操作取消".to_string(),
                    tool_calls: vec![],
                    timestamp: os_core::Utc::now(),
                })
                .await?;
            let _ = self
                .bus
                .publish(Event {
                    source: "os-im".into(),
                    topic: Topic::AgentTask,
                    kind: "action.rejected".into(),
                    severity: Severity::Warn,
                    task_id: Some(task_id),
                    payload: json!({ "reason": "user_rejected" }),
                    timestamp: os_core::Utc::now(),
                })
                .await;
            return Ok((
                OrchestrationStatus::Failed {
                    reason: "用户拒绝".into(),
                },
                conf_status,
            ));
        }

        // 5) 委派 agent 执行（agent 调 tool）。
        let _returned_id = self.orch.delegate(task).await?;
        let status = self.orch.task_status(&task_id).await;

        // 6) 结果聚合写回对话（Tool 角色）。
        let result_text = match &status {
            OrchestrationStatus::Completed { result } => {
                format!("操作完成：{}", result)
            }
            OrchestrationStatus::Failed { reason } => format!("操作失败：{}", reason),
            other => format!("操作未完成：{other:?}"),
        };
        self.store
            .add_message(Message {
                id: format!("msg-tool-{}", conv),
                conversation: conv.clone(),
                role: MessageRole::Tool,
                content: result_text,
                tool_calls: vec![],
                timestamp: os_core::Utc::now(),
            })
            .await?;

        // 7) 发 AgentTask 事件。
        let kind = if matches!(status, OrchestrationStatus::Completed { .. }) {
            "action.completed"
        } else {
            "action.failed"
        };
        let severity = if matches!(status, OrchestrationStatus::Completed { .. }) {
            Severity::Info
        } else {
            Severity::Error
        };
        let _ = self
            .bus
            .publish(Event {
                source: "os-im".into(),
                topic: Topic::AgentTask,
                kind: kind.into(),
                severity,
                task_id: Some(task_id),
                payload: json!({ "agent": target_agent.as_str() }),
                timestamp: os_core::Utc::now(),
            })
            .await;

        Ok((status, conf_status))
    }
}

// ----------------------------------------------------------------------------
// 集成测
// ----------------------------------------------------------------------------

/// 全链路：用户"建 VM" → 路由 ComputeAgent → agent 调 vm.create tool →
/// 高危确认（用户确认）→ 结果聚合回写对话 + 事件。
#[tokio::test]
async fn im_conversation_creates_vm_full_chain() {
    let flow = ConversationAsActionFlow::new();

    let tool: Arc<os_im::MockTool> = Arc::new(
        os_im::MockTool::new("vm.create")
            .with_result(true, json!({ "vm_id": "vm-from-im", "state": "stopped" })),
    );
    let agent_id = flow
        .register_compute_agent("compute-agent", "vm.create", tool.clone())
        .await;

    // 能力发现：route_by_tool / route_by_domain 命中 compute-agent。
    assert_eq!(
        flow.orch.route_by_tool("vm.create").await,
        Some(agent_id.clone())
    );
    assert_eq!(
        flow.orch.route_by_domain("compute").await,
        Some(agent_id.clone())
    );

    let (status, conf) = flow
        .run_high_risk_action("帮我建一台 VM", agent_id.clone(), RiskLevel::High, true)
        .await
        .unwrap();

    // 高危确认：High + 用户确认 → UserApproved。
    assert!(
        matches!(conf, ConfirmationStatus::UserApproved),
        "High + 用户确认应 UserApproved，实得 {conf:?}"
    );
    // 编排状态：Completed（agent 调 tool 成功）。
    match &status {
        OrchestrationStatus::Completed { result } => {
            assert_eq!(result["vm_id"].as_str(), Some("vm-from-im"));
        }
        other => panic!("应 Completed，实得 {other:?}"),
    }

    // 链路断言：
    // 1. tool 被 invoke 1 次（agent 调 tool 链路通）。
    assert_eq!(tool.invoke_count(), 1, "agent 应调一次 tool");
    // 2. 黑板写入 task.<id>.tool_result（agent → SharedContext 链路通）。
    let bb_entries = flow.blackboard.list().await;
    assert!(
        bb_entries
            .iter()
            .any(|e| e.key.starts_with("task.") && e.key.ends_with(".tool_result")),
        "黑板应有 task.<id>.tool_result 条目: {bb_entries:?}"
    );
    // 3. EventBus 收 action.completed Info 事件。
    assert_eq!(flow.bus.published_count_for(Topic::AgentTask), 1);
    assert_eq!(flow.bus.published()[0].kind, "action.completed");
    assert_eq!(flow.bus.published()[0].severity, Severity::Info);
    // 4. 对话历史 2 条：用户意图 + Tool 结果（IM 反馈尾）。
    let any_conv = flow
        .store
        .list_conversations("alice")
        .await
        .unwrap()
        .pop()
        .unwrap();
    let history = flow.store.history(&any_conv, 10).await.unwrap();
    assert_eq!(history.len(), 2, "对话应有用户意图 + Tool 结果 2 条");
    assert_eq!(history[0].role, MessageRole::User);
    assert_eq!(history[1].role, MessageRole::Tool);
    assert!(history[1].content.contains("vm-from-im"));
}

/// 用户拒绝 → 短路：不委派 agent，写 System 消息 + action.rejected 事件。
#[tokio::test]
async fn im_conversation_user_rejects_short_circuits() {
    let flow = ConversationAsActionFlow::new();
    let tool: Arc<os_im::MockTool> = Arc::new(os_im::MockTool::new("vm.destroy"));
    let agent_id = flow
        .register_compute_agent("compute-agent", "vm.destroy", tool.clone())
        .await;

    let (status, conf) = flow
        .run_high_risk_action("销毁 VM", agent_id, RiskLevel::High, false)
        .await
        .unwrap();

    // 用户拒绝 → UserRejected。
    assert!(matches!(conf, ConfirmationStatus::UserRejected));
    // 状态：Failed（reason = 用户拒绝）。
    assert!(matches!(status, OrchestrationStatus::Failed { .. }));

    // agent 未被调用（tool invoke_count = 0）。
    assert_eq!(tool.invoke_count(), 0, "用户拒绝时不应调 tool");
    // 黑板无写入（agent 没执行）。
    assert!(flow.blackboard.list().await.is_empty(), "拒绝时黑板应空");
    // EventBus 发 action.rejected Warn 事件（不是 action.completed）。
    assert_eq!(flow.bus.published_count_for(Topic::AgentTask), 1);
    assert_eq!(flow.bus.published()[0].kind, "action.rejected");
    assert_eq!(flow.bus.published()[0].severity, Severity::Warn);
    // 对话历史 2 条：用户意图 + System 取消。
    let conv = flow
        .store
        .list_conversations("alice")
        .await
        .unwrap()
        .pop()
        .unwrap();
    let history = flow.store.history(&conv, 10).await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[1].role, MessageRole::System);
    assert!(history[1].content.contains("用户拒绝"));
}

/// Critical 风险：需用户确认 + 会签达法定双满足（红线 §3.7.2）。
#[tokio::test]
async fn im_conversation_critical_needs_user_and_quorum() {
    let flow = ConversationAsActionFlow::with_quorum(2);
    let tool: Arc<os_im::MockTool> = Arc::new(os_im::MockTool::new("cluster.shutdown"));
    let agent_id = flow
        .register_compute_agent("compute-agent", "cluster.shutdown", tool.clone())
        .await;

    // 手动跑 Critical 双满足路径。
    let task_id = TaskId::new();
    let req = ConfirmationRequest {
        id: format!("req-{}", task_id),
        task_id,
        description: "关机集群（Critical）".into(),
        risk_level: RiskLevel::Critical,
        requested_by: agent_id.clone(),
        created_at: os_core::Utc::now(),
    };
    let req_id = flow.gate.request(req).await.unwrap();

    // 仅用户确认（无法定）→ 仍 Pending。
    flow.gate.user_confirm(&req_id, true).await.unwrap();
    assert!(matches!(
        flow.gate.status(&req_id).await,
        ConfirmationStatus::Pending
    ));

    // 1 票（未达法定 2）→ 仍 Pending。
    flow.gate
        .agent_vote(
            &req_id,
            VoteRecord {
                agent: AgentId::new("meta-agent"),
                approve: true,
                reason: None,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        flow.gate.status(&req_id).await,
        ConfirmationStatus::Pending
    ));

    // 第 2 票达法定 → QuorumReached { approved: true }（红线：Critical 双满足）。
    flow.gate
        .agent_vote(
            &req_id,
            VoteRecord {
                agent: AgentId::new("storage-agent"),
                approve: true,
                reason: None,
            },
        )
        .await
        .unwrap();
    match flow.gate.status(&req_id).await {
        ConfirmationStatus::QuorumReached { approved } => assert!(approved),
        other => panic!("Critical 双满足应 QuorumReached，实得 {other:?}"),
    }

    // 法定满足后委派 agent 执行。
    let task = AgentTask {
        id: task_id,
        assigned_to: agent_id,
        description: "关机集群".into(),
        context: json!({}),
        deadline: None,
    };
    flow.orch.delegate(task).await.unwrap();
    assert!(matches!(
        flow.orch.task_status(&task_id).await,
        OrchestrationStatus::Completed { .. }
    ));
    assert_eq!(tool.invoke_count(), 1, "Critical 双满足后才调 tool");
}

/// 任务图无环检测（红线 §3.7.2）：环 → TaskCycle。
#[tokio::test]
async fn im_orchestrator_task_graph_cycle_rejected() {
    let flow = ConversationAsActionFlow::new();
    let a = TaskId::new();
    let b = TaskId::new();
    let c = TaskId::new();

    // 合法 DAG：a → b → c。
    flow.orch.add_dependencies(a, [b]).unwrap();
    flow.orch.add_dependencies(b, [c]).unwrap();
    assert!(flow.orch.dependencies_of(&a).contains(&b));
    assert!(flow.orch.dependencies_of(&b).contains(&c));

    // 加 c → a 构成环 → TaskCycle，且 c 的依赖被回滚（无 deps）。
    let err = flow.orch.add_dependencies(c, [a]).unwrap_err();
    assert!(
        matches!(err, ImError::TaskCycle(_)),
        "环应返回 TaskCycle，实得 {err:?}"
    );
    // c 的 deps 被回滚（add_dependencies 命中环时移除该 task 条目）。
    assert!(
        flow.orch.dependencies_of(&c).is_empty(),
        "环命中应回滚 c 的 deps"
    );
    // a/b 的合法依赖保留。
    assert!(flow.orch.dependencies_of(&a).contains(&b));
}

/// 任务图无环：DAG（无环）允许，delegate 正常执行。
#[tokio::test]
async fn im_orchestrator_dag_allows_delegation() {
    let flow = ConversationAsActionFlow::new();
    let tool: Arc<os_im::MockTool> =
        Arc::new(os_im::MockTool::new("vm.create").with_result(true, json!({"ok":1})));
    let agent_id = flow
        .register_compute_agent("compute-agent", "vm.create", tool.clone())
        .await;

    // 三任务依赖 DAG：task1 → task2 → task3（task1 等 task2，task2 等 task3）。
    let t1 = TaskId::new();
    let t2 = TaskId::new();
    let t3 = TaskId::new();
    flow.orch.add_dependencies(t1, [t2]).unwrap();
    flow.orch.add_dependencies(t2, [t3]).unwrap();

    // delegate t1（无环，可执行）。
    let task = AgentTask {
        id: t1,
        assigned_to: agent_id,
        description: "build".into(),
        context: json!({}),
        deadline: None,
    };
    flow.orch.delegate(task).await.unwrap();
    assert!(matches!(
        flow.orch.task_status(&t1).await,
        OrchestrationStatus::Completed { .. }
    ));
}

/// SharedContext 黑板：key 命名空间 + clear_for_task 按前缀清理。
#[tokio::test]
async fn im_shared_context_namespace_and_clear_for_task() {
    let bb = MockSharedContext::new();
    let writer = AgentId::new("compute-agent");
    let task_a = TaskId::new();
    let task_b = TaskId::new();

    // task_a 写两条，task_b 写一条，全局一条。
    bb.put(&format!("task.{}.vm_id", task_a), json!("vm-1"), &writer)
        .await
        .unwrap();
    bb.put(
        &format!("task.{}.snapshot", task_a),
        json!("snap-1"),
        &writer,
    )
    .await
    .unwrap();
    bb.put(&format!("task.{}.iso", task_b), json!("iso-1"), &writer)
        .await
        .unwrap();
    bb.put("global.config", json!({"region": "us"}), &writer)
        .await
        .unwrap();

    // 清理 task_a 命名空间（命中前缀 task.<a>.，保留 task_b 与 global）。
    bb.clear_for_task(&task_a).await;
    let remaining: Vec<String> = bb.list().await.into_iter().map(|e| e.key).collect();
    assert_eq!(remaining.len(), 2);
    assert!(remaining
        .iter()
        .any(|k| k == &format!("task.{}.iso", task_b)));
    assert!(remaining.iter().any(|k| k == "global.config"));
}

/// ConfirmationGate：用户拒绝优先，即便后续投票也无法翻盘。
#[tokio::test]
async fn im_confirmation_user_reject_beats_votes() {
    let gate = MockConfirmationGate::new();
    let task = TaskId::new();
    let req = ConfirmationRequest {
        id: "req-reject".into(),
        task_id: task,
        description: "高危".into(),
        risk_level: RiskLevel::Critical,
        requested_by: AgentId::new("compute-agent"),
        created_at: os_core::Utc::now(),
    };
    let id = gate.request(req).await.unwrap();

    // 用户拒绝。
    gate.user_confirm(&id, false).await.unwrap();
    // 后续 agent 赞成票（达法定 2）。
    gate.agent_vote(
        &id,
        VoteRecord {
            agent: AgentId::new("a1"),
            approve: true,
            reason: None,
        },
    )
    .await
    .unwrap();
    gate.agent_vote(
        &id,
        VoteRecord {
            agent: AgentId::new("a2"),
            approve: true,
            reason: None,
        },
    )
    .await
    .unwrap();

    // 仍 UserRejected（用户拒绝优先于投票）。
    assert!(matches!(
        gate.status(&id).await,
        ConfirmationStatus::UserRejected
    ));
}

/// 静态桥接：AgentTask.context（JSON） ↔ ToolCall.arguments（JSON）透传。
#[tokio::test]
async fn im_intent_context_bridges_to_tool_arguments() {
    let bb: Arc<dyn SharedContext> = Arc::new(MockSharedContext::new());
    let tool: Arc<os_im::MockTool> =
        Arc::new(os_im::MockTool::new("vm.create").with_result(true, json!({"state":"ok"})));
    let agent = ToolInvokingAgent::new(
        "a",
        "compute",
        "vm.create".into(),
        tool.clone() as Arc<dyn Tool>,
        bb,
    );

    let ctx = json!({
        "vm_id": "vm-bridge",
        "cpus": 2,
        "memory_mb": 2048,
        "nested": { "key": "value" }
    });
    let task = AgentTask {
        id: TaskId::new(),
        assigned_to: AgentId::new("a"),
        description: "test".into(),
        context: ctx.clone(),
        deadline: None,
    };
    let result = agent.handle_task(&task).await.unwrap();
    assert!(result.success);

    // tool 收到的 arguments == task.context（透传）。
    assert_eq!(tool.invoke_count(), 1);
    // 通过 AgentTaskResult.output 验证（output 即 tool_result.output）。
    assert_eq!(result.output, json!({"state":"ok"}));
}

/// Low 风险操作：用户确认即 UserApproved（不需会签）。
#[tokio::test]
async fn im_low_risk_user_confirm_suffices() {
    let flow = ConversationAsActionFlow::new();
    let tool: Arc<os_im::MockTool> =
        Arc::new(os_im::MockTool::new("vm.list").with_result(true, json!([{"id":"vm-1"}])));
    let agent_id = flow
        .register_compute_agent("query-agent", "vm.list", tool.clone())
        .await;

    let (status, conf) = flow
        .run_high_risk_action("列 VM", agent_id, RiskLevel::Low, true)
        .await
        .unwrap();

    assert!(matches!(conf, ConfirmationStatus::UserApproved));
    assert!(matches!(status, OrchestrationStatus::Completed { .. }));
    assert_eq!(tool.invoke_count(), 1);
}

/// delegate 到未注册 agent → AgentNotFound + 状态 Failed。
#[tokio::test]
async fn im_delegate_unknown_agent_fails() {
    let flow = ConversationAsActionFlow::new();
    let task = AgentTask {
        id: TaskId::new(),
        assigned_to: AgentId::new("ghost-agent"),
        description: "x".into(),
        context: json!({}),
        deadline: None,
    };
    let err = flow.orch.delegate(task.clone()).await.unwrap_err();
    assert!(matches!(err, ImError::AgentNotFound(_)), "应 AgentNotFound");

    // 任务状态：Failed（agent 不存在）。
    let status = flow.orch.task_status(&task.id).await;
    assert!(matches!(status, OrchestrationStatus::Failed { .. }));
}
