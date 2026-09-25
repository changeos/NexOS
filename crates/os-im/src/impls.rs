//! os-im 默认实现——协作中枢运行时（规划文档 §3.7.2）。
//!
//! 本文件提供 7 个 trait 的默认/参考实现：
//! - [`InMemoryConversationStore`]：内存对话存储（CRUD）。
//! - [`InMemoryBlackboard`]：内存黑板（key-value + task 前缀清理）。
//! - [`DefaultConfirmationGate`]：高危确认状态机（用户确认 + 多 agent 会签法定数）。
//! - [`CentralOrchestrator`]：中枢编排（注册 / 能力发现 / 委派含**任务图无环检测** / 状态聚合）。
//!
//! `Tool` / `Agent` / `LlmBackend` 三 trait 由各领域实现并经 `Box<dyn>` 注入，
//! 本 crate 不提供领域实现（仅 mock.rs 提供测试用 Mock）。
//!
//! 所有实现方法返回 `ImResult<T>`，纯内存、`Send + Sync`（内部用 `Mutex`）。

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;

use crate::agent::{Agent, AgentCapability, AgentId, AgentTask, AgentTaskResult};
use crate::blackboard::{BlackboardEntry, SharedContext};
use crate::confirmation::{
    ConfirmationGate, ConfirmationRequest, ConfirmationStatus, RiskLevel, VoteRecord,
};
use crate::conversation::{ConversationId, ConversationStore, Message, MessageRole};
use crate::error::ImError;
use os_core::{DateTime, TaskId};

// ============================================================
// 工具函数
// ============================================================

/// 黑板 key 前缀：`task.<id>`（约定见 im-agent 规格 §3）。
///
/// `clear_for_task` 按此前缀清理（匹配以 `task.<id>.` 开头的 key）。
fn task_prefix(task: &TaskId) -> String {
    format!("task.{}.", task)
}

/// 当前 UTC 时间。
fn now() -> DateTime {
    chrono::Utc::now()
}

// ============================================================
// InMemoryConversationStore
// ============================================================

/// 内存对话存储——纯内存实现，用于测试 / 单节点轻量场景。
///
/// 生产实现：`SqliteConversationStore`（随 meta SQLite 复制 HA，见 im-agent 规格 §3）。
pub struct InMemoryConversationStore {
    /// 对话 ID → 所属用户
    conversations: Mutex<HashMap<ConversationId, String>>,
    /// 对话 ID → 消息列表（按时间升序）
    messages: Mutex<HashMap<ConversationId, Vec<Message>>>,
}

impl InMemoryConversationStore {
    /// 创建空存储。
    pub fn new() -> Self {
        Self {
            conversations: Mutex::new(HashMap::new()),
            messages: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryConversationStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ConversationStore for InMemoryConversationStore {
    async fn create_conversation(&self, user: &str) -> Result<ConversationId, ImError> {
        let id = ConversationId::new();
        self.conversations
            .lock()
            .unwrap()
            .insert(id.clone(), user.to_string());
        self.messages.lock().unwrap().insert(id.clone(), Vec::new());
        Ok(id)
    }

    async fn add_message(&self, msg: Message) -> Result<(), ImError> {
        let mut msgs = self.messages.lock().unwrap();
        if !msgs.contains_key(&msg.conversation) {
            return Err(ImError::ConversationNotFound(format!(
                "{}",
                msg.conversation
            )));
        }
        msgs.get_mut(&msg.conversation).unwrap().push(msg);
        Ok(())
    }

    async fn history(&self, conv: &ConversationId, limit: u32) -> Result<Vec<Message>, ImError> {
        let msgs = self.messages.lock().unwrap();
        let list = match msgs.get(conv) {
            None => {
                return Err(ImError::ConversationNotFound(format!("{}", conv)));
            }
            Some(v) => v,
        };
        // 取最近 limit 条，按时间升序返回
        let n = limit as usize;
        let start = list.len().saturating_sub(n);
        Ok(list[start..].to_vec())
    }

    async fn list_conversations(&self, user: &str) -> Result<Vec<ConversationId>, ImError> {
        let convs = self.conversations.lock().unwrap();
        let mut ids: Vec<ConversationId> = convs
            .iter()
            .filter(|(_, u)| u.as_str() == user)
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort_by_key(|id| id.0.to_string());
        Ok(ids)
    }
}

// ============================================================
// InMemoryBlackboard
// ============================================================

/// 内存黑板——agent 间协作原语（key-value，按 task 前缀清理）。
///
/// 生产实现：`DistributedBlackboard`（基于 os-meta KV，HA 复制）。
pub struct InMemoryBlackboard {
    entries: Mutex<HashMap<String, BlackboardEntry>>,
}

impl InMemoryBlackboard {
    /// 创建空黑板。
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryBlackboard {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SharedContext for InMemoryBlackboard {
    async fn put(
        &self,
        key: &str,
        value: serde_json::Value,
        writer: &AgentId,
    ) -> Result<(), ImError> {
        let entry = BlackboardEntry {
            key: key.to_string(),
            value,
            written_by: writer.clone(),
            timestamp: now(),
        };
        self.entries.lock().unwrap().insert(key.to_string(), entry);
        Ok(())
    }

    async fn get(&self, key: &str) -> Option<BlackboardEntry> {
        self.entries.lock().unwrap().get(key).cloned()
    }

    async fn list(&self) -> Vec<BlackboardEntry> {
        let entries = self.entries.lock().unwrap();
        let mut v: Vec<BlackboardEntry> = entries.values().cloned().collect();
        // 按 key 排序，保证确定性（便于测试断言）
        v.sort_by(|a, b| a.key.cmp(&b.key));
        v
    }

    async fn clear_for_task(&self, task: &TaskId) {
        let prefix = task_prefix(task);
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|k, _| !k.starts_with(&prefix));
    }
}

// ============================================================
// DefaultConfirmationGate
// ============================================================

/// 内部确认请求状态。
#[derive(Debug, Clone)]
struct GateState {
    req: ConfirmationRequest,
    user_decision: Option<bool>,
    votes: Vec<VoteRecord>,
}

impl GateState {
    fn new(req: ConfirmationRequest) -> Self {
        Self {
            req,
            user_decision: None,
            votes: Vec::new(),
        }
    }
}

/// 默认确认门——高危操作双轨确认（用户 + 多 agent 会签）。
///
/// 状态流转：
/// - 任一风险级别：用户拒绝 → 立即 [`ConfirmationStatus::UserRejected`]。
/// - `Low` / `Medium`：用户确认即 [`ConfirmationStatus::UserApproved`]。
/// - `High`：需用户确认 → [`ConfirmationStatus::UserApproved`]（不强制会签）。
/// - `Critical`：**必须用户确认 + 会签达法定人数双满足**（红线，见 im-agent 规格 §9）。
///   会签法定人数默认 2（可配置），赞成票达阈值后 [`ConfirmationStatus::QuorumReached { approved: true }`]。
pub struct DefaultConfirmationGate {
    /// request id → 状态
    states: Mutex<HashMap<String, GateState>>,
    /// Critical 级所需赞成票数（默认 2）
    quorum: usize,
}

impl DefaultConfirmationGate {
    /// 创建默认确认门（Critical 法定人数 = 2）。
    pub fn new() -> Self {
        Self::with_quorum(2)
    }

    /// 创建自定义法定人数的确认门（Critical 级用）。
    ///
    /// 法定人数阈值须 ≥ 1；默认值 2 须经 ReviewAgent 评审（im-agent 规格 §9 黄线）。
    pub fn with_quorum(quorum: usize) -> Self {
        Self {
            states: Mutex::new(HashMap::new()),
            quorum: quorum.max(1),
        }
    }

    /// 取法定人数阈值。
    pub fn quorum(&self) -> usize {
        self.quorum
    }

    /// 计算当前状态（不修改）。Critical 双满足：用户确认 + 赞成票 ≥ 法定人数。
    fn compute_status(state: &GateState, quorum: usize) -> ConfirmationStatus {
        // 用户拒绝优先
        if matches!(state.user_decision, Some(false)) {
            return ConfirmationStatus::UserRejected;
        }
        match state.req.risk_level {
            RiskLevel::Low | RiskLevel::Medium => {
                if matches!(state.user_decision, Some(true)) {
                    ConfirmationStatus::UserApproved
                } else {
                    ConfirmationStatus::Pending
                }
            }
            RiskLevel::High => {
                if matches!(state.user_decision, Some(true)) {
                    ConfirmationStatus::UserApproved
                } else {
                    ConfirmationStatus::Pending
                }
            }
            RiskLevel::Critical => {
                // Critical 双满足：用户确认 + 会签达法定（红线）
                let user_ok = matches!(state.user_decision, Some(true));
                let approve_count = state.votes.iter().filter(|v| v.approve).count();
                if user_ok && approve_count >= quorum {
                    ConfirmationStatus::QuorumReached { approved: true }
                } else {
                    ConfirmationStatus::Pending
                }
            }
        }
    }
}

impl Default for DefaultConfirmationGate {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ConfirmationGate for DefaultConfirmationGate {
    async fn request(&self, req: ConfirmationRequest) -> Result<String, ImError> {
        let id = req.id.clone();
        self.states
            .lock()
            .unwrap()
            .insert(id.clone(), GateState::new(req));
        Ok(id)
    }

    async fn user_confirm(&self, req_id: &str, approved: bool) -> Result<(), ImError> {
        let mut states = self.states.lock().unwrap();
        let state = states
            .get_mut(req_id)
            .ok_or_else(|| ImError::Internal(format!("确认请求不存在: {}", req_id)))?;
        state.user_decision = Some(approved);
        Ok(())
    }

    async fn agent_vote(&self, req_id: &str, vote: VoteRecord) -> Result<(), ImError> {
        let mut states = self.states.lock().unwrap();
        let state = states
            .get_mut(req_id)
            .ok_or_else(|| ImError::Internal(format!("确认请求不存在: {}", req_id)))?;
        // 同一 agent 重复投票：覆盖其上一次投票（保留最后一次意图）
        if let Some(existing) = state.votes.iter_mut().find(|v| v.agent == vote.agent) {
            *existing = vote;
        } else {
            state.votes.push(vote);
        }
        Ok(())
    }

    async fn status(&self, req_id: &str) -> ConfirmationStatus {
        let states = self.states.lock().unwrap();
        match states.get(req_id) {
            None => ConfirmationStatus::Expired,
            Some(state) => Self::compute_status(state, self.quorum),
        }
    }
}

// ============================================================
// CentralOrchestrator（任务图无环检测 + Tool 路由）
// ============================================================

/// 已注册的领域 agent（持有 trait object）。
struct RegisteredAgent {
    agent: Box<dyn Agent>,
    capability: AgentCapability,
}

/// 中枢——多 agent 协作运行时核心（规划文档 §3.7.2）。
///
/// 职责：
/// - 注册/注销领域 agent（`Box<dyn Agent>`）。
/// - 能力发现：按 `AgentCapability.tools` / `domain` 路由任务（[`Self::route_by_tool`] / [`Self::route_by_domain`]）。
/// - 任务委派：**任务依赖图必须无环**——delegate 时做 DFS 环检测，命中返回 [`ImError::TaskCycle`]（红线）。
/// - 状态聚合：`task_status` 返回 [`crate::OrchestrationStatus`]。
///
/// 真实 LLM 调用 / 跨 agent 自动委派链不在本骨架范围（留 TODO）。
pub struct CentralOrchestrator {
    /// agent ID → 已注册 agent
    agents: Mutex<HashMap<AgentId, RegisteredAgent>>,
    /// 任务状态表
    task_status: Mutex<HashMap<TaskId, crate::OrchestrationStatus>>,
    /// 任务依赖图：任务 → 它**等待**的任务集合（被等待的任务先完成）。
    /// 无环约束：该图必须保持 DAG（DFS 检测，命中环返回 TaskCycle）。
    task_deps: Mutex<HashMap<TaskId, HashSet<TaskId>>>,
}

impl CentralOrchestrator {
    /// 创建空中枢。
    pub fn new() -> Self {
        Self {
            agents: Mutex::new(HashMap::new()),
            task_status: Mutex::new(HashMap::new()),
            task_deps: Mutex::new(HashMap::new()),
        }
    }

    /// 按 Tool name 路由到对应 agent（能力发现）。
    ///
    /// 返回首个声明该 tool 的 agent ID（注册顺序由 HashMap 决定，确定性不足；
    /// 多 agent 声明同一 tool 时建议调用方二次筛选 domain）。
    pub async fn route_by_tool(&self, tool_name: &str) -> Option<AgentId> {
        let agents = self.agents.lock().unwrap();
        for a in agents.values() {
            if a.capability.tools.iter().any(|t| t == tool_name) {
                return Some(a.capability.agent_id.clone());
            }
        }
        None
    }

    /// 按 domain 路由到对应 agent（能力发现）。
    pub async fn route_by_domain(&self, domain: &str) -> Option<AgentId> {
        let agents = self.agents.lock().unwrap();
        for a in agents.values() {
            if a.capability.domain == domain {
                return Some(a.capability.agent_id.clone());
            }
        }
        None
    }

    /// 取任务当前依赖（被等待的任务集合）——测试可见。
    pub fn dependencies_of(&self, task: &TaskId) -> HashSet<TaskId> {
        self.task_deps
            .lock()
            .unwrap()
            .get(task)
            .cloned()
            .unwrap_or_default()
    }

    /// 注册依赖边（task 等待 deps），并做**无环检测**。
    ///
    /// 算法：在加入新边后对整张依赖图做 DFS 三色标记（白/灰/黑），
    /// 遇到灰色节点即存在环（回边）。命中环则回滚并返回 [`ImError::TaskCycle`]。
    ///
    /// 为什么对整图做检测而非仅增量：增量检测需精确判断新边是否引入回边，
    /// 整图 DFS 实现简单且正确（任务图规模小，O(V+E) 可接受），优先保证正确性。
    pub fn add_dependencies(
        &self,
        task: TaskId,
        deps: impl IntoIterator<Item = TaskId>,
    ) -> Result<(), ImError> {
        let mut graph = self.task_deps.lock().unwrap();
        let entry = graph.entry(task).or_default();
        for d in deps {
            entry.insert(d);
        }
        // 整图无环检测
        if let Some(cycle) = detect_cycle(&graph) {
            // 命中环：把该 task 的依赖图恢复（清空刚加的最稳妥——这里整体回滚 task 的 deps）
            // 简化：保留原状但返回错误，调用方应据此中止委派。
            // 为避免脏数据，移除该 task 的依赖图（回到无 deps 状态）。
            graph.remove(&task);
            let path = cycle
                .into_iter()
                .map(|t| format!("{}", t))
                .collect::<Vec<_>>()
                .join(" -> ");
            return Err(ImError::TaskCycle(format!("任务依赖图出现环: {}", path)));
        }
        Ok(())
    }
}

impl Default for CentralOrchestrator {
    fn default() -> Self {
        Self::new()
    }
}

/// DFS 三色环检测。返回 Some(环路径) 表示存在环。
///
/// - 白（未访问）→ 灰（在当前递归栈）→ 黑（完成）。
/// - 遇到灰色邻居即回边，构成环。
fn detect_cycle(graph: &HashMap<TaskId, HashSet<TaskId>>) -> Option<Vec<TaskId>> {
    let mut color: HashMap<TaskId, u8> = HashMap::new(); // 0=白 1=灰 2=黑
    let mut stack: Vec<TaskId> = Vec::new();

    // 收集所有节点（含仅作为依赖出现的）
    let mut nodes: HashSet<TaskId> = HashSet::new();
    for (k, vs) in graph {
        nodes.insert(*k);
        for v in vs {
            nodes.insert(*v);
        }
    }

    for start in nodes {
        if color.get(&start).copied().unwrap_or(0) == 0 {
            if let Some(cycle) = dfs_visit(start, graph, &mut color, &mut stack) {
                return Some(cycle);
            }
        }
    }
    None
}

/// DFS 访问单个节点，返回检测到的环路径（从环起点回到自身）。
fn dfs_visit(
    node: TaskId,
    graph: &HashMap<TaskId, HashSet<TaskId>>,
    color: &mut HashMap<TaskId, u8>,
    stack: &mut Vec<TaskId>,
) -> Option<Vec<TaskId>> {
    color.insert(node, 1); // 灰
    stack.push(node);

    if let Some(neighbors) = graph.get(&node) {
        for &next in neighbors {
            let c = color.get(&next).copied().unwrap_or(0);
            if c == 1 {
                // 回边：构造环（从 stack 中 next 出现位置到当前）
                let pos = stack.iter().position(|x| *x == next)?;
                let cycle: Vec<TaskId> = stack[pos..].to_vec();
                return Some(cycle);
            }
            if c == 0 {
                if let Some(cycle) = dfs_visit(next, graph, color, stack) {
                    return Some(cycle);
                }
            }
        }
    }

    stack.pop();
    color.insert(node, 2); // 黑
    None
}

impl crate::orchestrator::AgentOrchestrator for CentralOrchestrator {
    async fn register_agent(&self, agent: Box<dyn Agent>) -> Result<(), ImError> {
        let capability = agent.capabilities().await;
        let id = capability.agent_id.clone();
        let mut agents = self.agents.lock().unwrap();
        agents.insert(id, RegisteredAgent { agent, capability });
        Ok(())
    }

    async fn unregister_agent(&self, id: &AgentId) {
        self.agents.lock().unwrap().remove(id);
    }

    async fn list_agents(&self) -> Vec<AgentCapability> {
        let agents = self.agents.lock().unwrap();
        let mut caps: Vec<AgentCapability> =
            agents.values().map(|a| a.capability.clone()).collect();
        // 按 agent_id 排序，保证确定性（AgentId 无 Ord，按内部字符串排）
        caps.sort_by(|a, b| a.agent_id.as_str().cmp(b.agent_id.as_str()));
        caps
    }

    async fn delegate(&self, task: AgentTask) -> Result<TaskId, ImError> {
        let task_id = task.id;
        let assigned_to = task.assigned_to.clone();

        // 1) 无环检测：本任务依赖（这里 task 自身被加入图，无显式 deps）
        //    delegate 单任务不引入依赖边，但做一致性检查（确保该 task 未成环）。
        {
            let graph = self.task_deps.lock().unwrap();
            if graph.contains_key(&task_id) {
                if let Some(cycle) = detect_cycle(&graph) {
                    let path = cycle
                        .into_iter()
                        .map(|t| format!("{}", t))
                        .collect::<Vec<_>>()
                        .join(" -> ");
                    return Err(ImError::TaskCycle(format!(
                        "委派失败，任务依赖图出现环: {}",
                        path
                    )));
                }
            }
        }

        // 2) 校验目标 agent 已注册
        let agent_exists = self.agents.lock().unwrap().contains_key(&assigned_to);
        if !agent_exists {
            // 标记失败状态
            self.task_status.lock().unwrap().insert(
                task_id,
                crate::OrchestrationStatus::Failed {
                    reason: format!("agent 不存在: {}", assigned_to),
                },
            );
            return Err(ImError::AgentNotFound(format!("{}", assigned_to)));
        }

        // 3) 标记已委派
        self.task_status.lock().unwrap().insert(
            task_id,
            crate::OrchestrationStatus::Delegated {
                to: assigned_to.clone(),
            },
        );

        // 4) 执行（真实执行：取 agent trait object，调 handle_task）
        let result: Result<AgentTaskResult, ImError> = {
            let agents = self.agents.lock().unwrap();
            // 这里持有锁调用 async handle_task 会违反 Send 约束（MutexGuard 非 Send），
            // 故先 clone 出 capability 校验通过后，释放锁再调用。
            // 但 Box<dyn Agent> 无法 clone——改为在锁内取引用不行（async）。
            // 解决：把 agent 从 map 临时 take 出来执行后放回（避免持锁跨 await）。
            drop(agents);
            // 重新设计：用 try take + put back 模式
            let taken = self.agents.lock().unwrap().remove(&assigned_to);
            match taken {
                Some(ra) => {
                    let res = ra.agent.handle_task(&task).await;
                    // 放回
                    self.agents.lock().unwrap().insert(assigned_to.clone(), ra);
                    res
                }
                None => {
                    return Err(ImError::AgentNotFound(format!("{}", assigned_to)));
                }
            }
        };

        // 5) 更新状态
        match result {
            Ok(r) => {
                if r.success {
                    self.task_status.lock().unwrap().insert(
                        task_id,
                        crate::OrchestrationStatus::Completed { result: r.output },
                    );
                } else {
                    self.task_status.lock().unwrap().insert(
                        task_id,
                        crate::OrchestrationStatus::Failed {
                            reason: "agent 返回失败".to_string(),
                        },
                    );
                }
            }
            Err(e) => {
                self.task_status.lock().unwrap().insert(
                    task_id,
                    crate::OrchestrationStatus::Failed {
                        reason: format!("{}", e),
                    },
                );
            }
        }
        Ok(task_id)
    }

    async fn task_status(&self, task: &TaskId) -> crate::OrchestrationStatus {
        let status = self.task_status.lock().unwrap();
        match status.get(task) {
            Some(s) => s.clone(),
            None => crate::OrchestrationStatus::Pending,
        }
    }
}

/// 构造一条对话消息的便利函数（仅字段聚合，不持久化）。
///
/// 引入此 helper 同时让 `MessageRole` 在本文件有真实引用点。
pub fn make_message(
    id: impl Into<String>,
    conversation: ConversationId,
    role: MessageRole,
    content: impl Into<String>,
) -> Message {
    Message {
        id: id.into(),
        conversation,
        role,
        content: content.into(),
        tool_calls: Vec::new(),
        timestamp: now(),
    }
}

// ============================================================
// 单元测——关键路径（im-agent 规格 §5.1/§8）
// 覆盖：任务图无环检测、Critical 双满足、黑板 key 前缀清理、
// 对话存储 CRUD、Tool 路由能力发现。
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::confirmation::ConfirmationRequest;
    use crate::conversation::ConversationStore;
    use crate::orchestrator::AgentOrchestrator;
    use os_core::TaskId;

    // —— 对话存储 ——（CRUD + history 取最近 limit 条）

    #[tokio::test]
    async fn conversation_store_crud_and_history() {
        let store = InMemoryConversationStore::new();
        let conv = store.create_conversation("alice").await.unwrap();
        // add_message 到不存在的对话 → 错误
        let bad = make_message("m0", ConversationId::new(), MessageRole::User, "x");
        assert!(store.add_message(bad).await.is_err());

        // 追加 3 条，history 取最近 2 条应返回后两条（升序）
        for i in 0..3u32 {
            store
                .add_message(make_message(
                    format!("m{}", i),
                    conv.clone(),
                    MessageRole::User,
                    format!("hello {}", i),
                ))
                .await
                .unwrap();
        }
        let hist = store.history(&conv, 2).await.unwrap();
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].content, "hello 1");
        assert_eq!(hist[1].content, "hello 2");

        // list_conversations 按用户过滤
        let _c2 = store.create_conversation("bob").await.unwrap();
        let alice = store.list_conversations("alice").await.unwrap();
        assert_eq!(alice.len(), 1);
        assert_eq!(alice[0], conv);
        assert_eq!(store.list_conversations("bob").await.unwrap().len(), 1);
        assert_eq!(store.list_conversations("nobody").await.unwrap().len(), 0);

        // 不存在的对话 history → 错误
        assert!(store.history(&ConversationId::new(), 10).await.is_err());
    }

    // —— 黑板：key 前缀清理 ——

    #[tokio::test]
    async fn blackboard_put_get_list_and_clear_for_task() {
        let bb = InMemoryBlackboard::new();
        let writer = AgentId::new("storage-agent");
        let task_a = TaskId::new();
        let task_b = TaskId::new();

        // 写入 task_a / task_b 命名空间 + 一个全局 key
        bb.put(
            &format!("task.{}.snapshot", task_a),
            serde_json::json!(1),
            &writer,
        )
        .await
        .unwrap();
        bb.put(
            &format!("task.{}.replica", task_a),
            serde_json::json!(2),
            &writer,
        )
        .await
        .unwrap();
        bb.put(
            &format!("task.{}.iso", task_b),
            serde_json::json!(3),
            &writer,
        )
        .await
        .unwrap();
        bb.put("global.config", serde_json::json!(4), &writer)
            .await
            .unwrap();

        // get 单个
        let got = bb
            .get(&format!("task.{}.snapshot", task_a))
            .await
            .expect("entry exists");
        assert_eq!(got.value, serde_json::json!(1));
        assert_eq!(got.written_by, writer);
        assert!(bb.get("missing").await.is_none());

        // list 全部 4 条
        assert_eq!(bb.list().await.len(), 4);

        // 清理 task_a（命中前缀 task.<a>.，保留 task_b 与 global）
        bb.clear_for_task(&task_a).await;
        let remaining: Vec<String> = bb.list().await.into_iter().map(|e| e.key).collect();
        assert_eq!(remaining.len(), 2);
        assert!(remaining
            .iter()
            .any(|k| k == &format!("task.{}.iso", task_b)));
        assert!(remaining.iter().any(|k| k == "global.config"));
    }

    // —— 确认门：Critical 双满足（用户确认 + 会签达法定）——

    fn make_request(risk: RiskLevel) -> ConfirmationRequest {
        ConfirmationRequest {
            id: format!(
                "req-{}",
                chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
            ),
            task_id: TaskId::new(),
            description: "高危删除".to_string(),
            risk_level: risk,
            requested_by: AgentId::new("storage-agent"),
            created_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn confirmation_gate_low_user_approves() {
        let gate = DefaultConfirmationGate::new();
        let req = make_request(RiskLevel::Low);
        let id = gate.request(req).await.unwrap();
        // 未确认前 pending
        assert!(matches!(
            gate.status(&id).await,
            ConfirmationStatus::Pending
        ));
        gate.user_confirm(&id, true).await.unwrap();
        assert!(matches!(
            gate.status(&id).await,
            ConfirmationStatus::UserApproved
        ));
    }

    #[tokio::test]
    async fn confirmation_gate_user_rejects_short_circuits() {
        let gate = DefaultConfirmationGate::new();
        let req = make_request(RiskLevel::High);
        let id = gate.request(req).await.unwrap();
        gate.user_confirm(&id, false).await.unwrap();
        // 用户拒绝优先 → UserRejected（即便后续投票也无法翻盘）
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
        assert!(matches!(
            gate.status(&id).await,
            ConfirmationStatus::UserRejected
        ));
    }

    #[tokio::test]
    async fn confirmation_gate_critical_needs_both_user_and_quorum() {
        // 法定 2 票
        let gate = DefaultConfirmationGate::with_quorum(2);
        let req = make_request(RiskLevel::Critical);
        let id = gate.request(req).await.unwrap();

        // 仅用户确认（无法定）→ 仍 pending
        gate.user_confirm(&id, true).await.unwrap();
        assert!(matches!(
            gate.status(&id).await,
            ConfirmationStatus::Pending
        ));

        // 1 票（未达法定 2）→ 仍 pending
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
        assert!(matches!(
            gate.status(&id).await,
            ConfirmationStatus::Pending
        ));

        // 第 2 票达法定 → QuorumReached { approved: true }（红线：Critical 双满足）
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
        match gate.status(&id).await {
            ConfirmationStatus::QuorumReached { approved } => assert!(approved),
            other => panic!("期望 QuorumReached，实际 {:?}", other),
        }
    }

    #[tokio::test]
    async fn confirmation_gate_critical_revoke_vote_overwrites() {
        let gate = DefaultConfirmationGate::with_quorum(1);
        let req = make_request(RiskLevel::Critical);
        let id = gate.request(req).await.unwrap();
        gate.user_confirm(&id, true).await.unwrap();
        // 同一 agent 先赞成（达法定 1）
        let voter = AgentId::new("a1");
        gate.agent_vote(
            &id,
            VoteRecord {
                agent: voter.clone(),
                approve: true,
                reason: None,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            gate.status(&id).await,
            ConfirmationStatus::QuorumReached { approved: true }
        ));
        // 同一 agent 改投反对（覆盖，赞成归零）→ pending
        gate.agent_vote(
            &id,
            VoteRecord {
                agent: voter,
                approve: false,
                reason: Some("改主意".to_string()),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            gate.status(&id).await,
            ConfirmationStatus::Pending
        ));
    }

    #[tokio::test]
    async fn confirmation_gate_unknown_request_returns_expired() {
        let gate = DefaultConfirmationGate::new();
        assert!(matches!(
            gate.status("nope").await,
            ConfirmationStatus::Expired
        ));
    }

    // —— 任务图无环检测（红线）——
    // 直接测 DFS 检测算法：构造环 → detect_cycle 命中。

    #[test]
    fn detect_cycle_finds_simple_loop() {
        let a = TaskId::new();
        let b = TaskId::new();
        // a -> b -> a
        let mut graph: HashMap<TaskId, HashSet<TaskId>> = HashMap::new();
        graph.insert(a, [b].into_iter().collect());
        graph.insert(b, [a].into_iter().collect());
        let cycle = detect_cycle(&graph).expect("应检测到环");
        assert!(cycle.len() >= 2);
        assert!(cycle.contains(&a) && cycle.contains(&b));
    }

    #[test]
    fn detect_cycle_no_loop_in_dag() {
        let a = TaskId::new();
        let b = TaskId::new();
        let c = TaskId::new();
        // a -> b -> c（DAG，无环）
        let mut graph: HashMap<TaskId, HashSet<TaskId>> = HashMap::new();
        graph.insert(a, [b].into_iter().collect());
        graph.insert(b, [c].into_iter().collect());
        assert!(detect_cycle(&graph).is_none());
    }

    #[test]
    fn add_dependencies_rejects_cycle_and_rolls_back() {
        let orch = CentralOrchestrator::new();
        let a = TaskId::new();
        let b = TaskId::new();
        // 先建 a -> b（合法 DAG）
        orch.add_dependencies(a, [b]).unwrap();
        assert!(orch.dependencies_of(&a).contains(&b));
        // 再加 b -> a，构成环 → TaskCycle，且 b 的依赖被回滚（无 deps）
        let err = orch.add_dependencies(b, [a]).unwrap_err();
        assert!(matches!(err, ImError::TaskCycle(_)), "应返回 TaskCycle");
        // b 的 deps 被回滚（add_dependencies 命中环时移除该 task 条目）
        assert!(orch.dependencies_of(&b).is_empty());
        // a 的合法依赖保留
        assert!(orch.dependencies_of(&a).contains(&b));
    }

    // —— Tool 路由（能力发现）—— 用 mock agent 注入中枢。

    #[tokio::test]
    async fn route_by_tool_and_domain_finds_registered_agent() {
        // 用 mock.rs 的 MockAgent（mock feature 下可用）
        let orch = CentralOrchestrator::new();
        let mock = crate::mock::MockAgent::new(
            "storage-agent",
            "storage",
            vec!["storage.snapshot.create".to_string()],
        );
        orch.register_agent(Box::new(mock)).await.unwrap();
        // 按 tool 路由
        assert_eq!(
            orch.route_by_tool("storage.snapshot.create").await,
            Some(AgentId::new("storage-agent"))
        );
        assert!(orch.route_by_tool("unknown.tool").await.is_none());
        // 按 domain 路由
        assert_eq!(
            orch.route_by_domain("storage").await,
            Some(AgentId::new("storage-agent"))
        );
        assert!(orch.route_by_domain("compute").await.is_none());
        // list_agents 返回能力
        let caps = orch.list_agents().await;
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].agent_id, AgentId::new("storage-agent"));
    }

    #[tokio::test]
    async fn delegate_to_unknown_agent_fails() {
        let orch = CentralOrchestrator::new();
        let task = AgentTask {
            id: TaskId::new(),
            assigned_to: AgentId::new("ghost"),
            description: "x".to_string(),
            context: serde_json::json!({}),
            deadline: None,
        };
        let err = orch.delegate(task).await.unwrap_err();
        assert!(matches!(err, ImError::AgentNotFound(_)));
    }

    #[tokio::test]
    async fn delegate_runs_registered_agent_and_completes() {
        let orch = CentralOrchestrator::new();
        let mock = crate::mock::MockAgent::new("vm-agent", "compute", Vec::new())
            .with_result(true, serde_json::json!({"vm": "running"}));
        orch.register_agent(Box::new(mock)).await.unwrap();
        let tid = TaskId::new();
        let task = AgentTask {
            id: tid,
            assigned_to: AgentId::new("vm-agent"),
            description: "start vm".to_string(),
            context: serde_json::json!({}),
            deadline: None,
        };
        let returned = orch.delegate(task).await.unwrap();
        assert_eq!(returned, tid);
        match orch.task_status(&tid).await {
            crate::OrchestrationStatus::Completed { result } => {
                assert_eq!(result, serde_json::json!({"vm": "running"}));
            }
            other => panic!("期望 Completed，实际 {:?}", other),
        }
    }

    #[tokio::test]
    async fn delegate_failed_agent_result_marks_failed() {
        let orch = CentralOrchestrator::new();
        let mock = crate::mock::MockAgent::new("failing-agent", "x", Vec::new())
            .with_result(false, serde_json::json!({}));
        orch.register_agent(Box::new(mock)).await.unwrap();
        let tid = TaskId::new();
        let task = AgentTask {
            id: tid,
            assigned_to: AgentId::new("failing-agent"),
            description: "boom".to_string(),
            context: serde_json::json!({}),
            deadline: None,
        };
        orch.delegate(task).await.unwrap();
        assert!(matches!(
            orch.task_status(&tid).await,
            crate::OrchestrationStatus::Failed { .. }
        ));
    }

    #[tokio::test]
    async fn unregister_agent_removes_capability() {
        let orch = CentralOrchestrator::new();
        let mock = crate::mock::MockAgent::new("tmp-agent", "x", Vec::new());
        orch.register_agent(Box::new(mock)).await.unwrap();
        assert_eq!(orch.list_agents().await.len(), 1);
        orch.unregister_agent(&AgentId::new("tmp-agent")).await;
        assert!(orch.list_agents().await.is_empty());
    }
}
