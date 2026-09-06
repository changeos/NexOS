# os-im

> 核心 IM + 多 Agent 协作中枢 · 契约 + 内存/骨架实现 · owner：im-agent（规划 §3.7）

OS 的对话与 AI agent 中枢：「对话即操作入口」（用户自然语言 → LLM → Tool 调用 →
agent 执行）+ AI agent 宿主 + 多 agent 协作运行时。本 crate 定义契约（trait +
数据结构 + Error）并附带内存实现，Tool 经 `Box<dyn Tool>` 注入而不硬依赖执行 crate。

## 核心能力

- **对话与消息**（`conversation`）：`ConversationStore` trait + `Message` /
  `MessageRole` / `ConversationId`。
- **LLM 抽象**（`llm`）：`LlmBackend` trait（`LlmRequest` / `LlmResponse` /
  `TokenUsage` / `LlmBackendType`）——后端可换，测试用 mock。
- **agent 与编排**（`agent` / `orchestrator`）：`Agent` trait（`#[async_trait]`
  dyn 兼容，能力发现 `AgentCapability` + 任务委派 `AgentTask`/`AgentTaskResult`）
  与 `AgentOrchestrator` 多 agent 协作运行时（任务无环委派）。
- **工具系统**（`tool`）：`Tool` trait（`ToolDescriptor` / `ToolCall` /
  `ToolResult` / `ToolCategory`）——Tool 可调各执行组件，trait 层零耦合。
- **协作原语**：上下文黑板 `SharedContext`（`BlackboardEntry`）+ 确认与投票
  `ConfirmationGate`（`RiskLevel` / `VoteRecord`）。
- **群组 / 联邦 / 传输**（`group*` / `federation*` / `transport*`）：
  `GroupManager`（默认 `InMemoryGroupManager`）、`FederationManager`
  （默认 `LocalFederationManager`）、`P2pTransport`（默认 `TcpP2pTransport`）。

## 架构位置

**依赖**（上游）：`os-core`、`os-common`（`From<ImError> for ApiError`）；
无其他内部依赖——Tool 注入式设计保证不反向依赖执行层 crate。

**被用**（下游）：当前 workspace 内无编译依赖方（os-api 经其自身路由层对接），
为 IM 领域唯一契约源。

## 独立使用

- **仓库外引用**：`os-im = { git = "http://ub2604:8080/git/nexos.git" }`。
- **契约规范**：默认原生 async fn in trait；需 `Box<dyn>` 运行期多态的
  （`Agent`）用 `#[async_trait]`（ADR-COMPAT-001）；自定义 `ImError`。
- **关键接口**：`ConversationStore` / `LlmBackend` / `Agent` /
  `AgentOrchestrator` / `Tool` / `SharedContext` / `ConfirmationGate` /
  `P2pTransport` / `FederationManager`。
- **feature**：`mock`（默认关）——`MockLlmBackend` / `MockAgent` /
  `MockConversationStore` / `MockSharedContext` / `MockConfirmationGate` /
  `MockTool` / `MockAgentOrchestrator` 七个测试桩。

## 测试

```bash
cargo test -p os-im
```

契约单测 + 内存实现（群组/联邦/TCP 传输）测试，纯内存无外部依赖。
