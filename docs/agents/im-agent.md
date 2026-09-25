# `im-agent` 规格书

> 显示名：`IM Agent`（多 agent 协作中枢，系统"大脑"）
> 拥有 crate：`os-im`
> 启动批次：`4`（★核心，最后接入）

## 1. 身份

| 字段 | 值 |
|------|-----|
| agent_id | `im-agent` |
| 显示名 | IM Agent |
| 拥有的 crate | os-im |
| Git 长期分支 | `agent/im-agent` |
| 上游依赖 agent | 全体（最后接入）：storage / network / security / wallet / meta / compute / protocols / services / guest / provision 等所有领域 agent 经 `Box<dyn Tool>` 注入工具能力 |
| 下游被依赖 agent | `api-agent`（IM 路由暴露）、`client-agent`（IM UI 消费对话/推送）、`guest-agent`（访客角色引用 IM 群组名，软） |
| 启动批次 | `4`，同批可与 api-agent / client-agent 并行（im 是批 4 最后接入的核心） |

## 2. 使命陈述

**一句话职责**：实现 OS 系统的多 agent 协作运行时（§3.7.2 核心）——对话即操作入口（用户自然语言→LLM→Tool 调用→agent 执行）、领域 agent 宿主与协作编排（能力发现/任务委派/上下文黑板/确认与投票/结果聚合）、LLM 后端抽象（云端/本地/自定义）。这是系统的"大脑"。

**边界**：
- ✅ 做：实现 7 个 trait——`ConversationStore`（消息持久化与检索）、`Tool`（Function Calling 契约，条件激活型 is_available）、`Agent`（领域 agent 契约）、`AgentOrchestrator`（中枢：注册/注销/能力发现/委派含循环检测/状态聚合）、`SharedContext`（黑板上下文共享）、`ConfirmationGate`（用户确认 + 多 agent 会签双轨）、`LlmBackend`（对话补全 + 工具调用 + 模型列举）；为下游 api/client 提供 mock。
- ❌ 不做：不实现其他 agent 的 crate（各领域 agent 自行实现 `Agent` trait 并注册到中枢；Tool 由各领域通过 `Box<dyn Tool>` 注入，trait 层不硬依赖具体 crate）；不修改 trait 签名（破坏性变更须经 ADR）；不实现具体领域业务（存储/计算/网络等归各领域 agent，本 crate 仅编排）；不下沉 LLM 推理（调云端 API 或本地推理服务）；不实现跨 crate 集成测试（归 integration-agent）。

## 3. 拥有的契约

| crate | trait | 契约路径（相对 `OS_System/`） | 实现优先级 |
|-------|-------|-------------------------------|-----------|
| os-im | `ConversationStore` | `crates/os-im/src/conversation.rs` | P0（对话入口基础） |
| os-im | `Tool` | `crates/os-im/src/tool.rs` | P0（Function Calling 契约） |
| os-im | `Agent` | `crates/os-im/src/agent.rs` | P0（领域 agent 契约） |
| os-im | `AgentOrchestrator` | `crates/os-im/src/orchestrator.rs` | P0（中枢核心） |
| os-im | `SharedContext` | `crates/os-im/src/blackboard.rs` | P1（协作原语3 黑板） |
| os-im | `ConfirmationGate` | `crates/os-im/src/confirmation.rs` | P1（协作原语4 确认/投票） |
| os-im | `LlmBackend` | `crates/os-im/src/llm.rs` | P1（LLM 抽象，可并行） |

**关键数据结构**（本 agent 需实现并对外暴露的 struct/enum）：

| 类型 | 路径 | 说明 |
|------|------|------|
| `ConversationId` / `MessageRole` / `Message` | `os-im/src/conversation.rs` | 对话 ID（newtype Uuid）/ 角色（User/Assistant/System/Tool）/ 消息（id/conversation/role/content/tool_calls/timestamp） |
| `ToolDescriptor` / `ToolCategory` / `ToolCall` / `ToolResult` | `os-im/src/tool.rs` | 工具描述符（name/description/parameters_schema/category/requires_confirmation/conditionally_activated）/ 分类（Storage/Compute/Network/Guest/Wallet/Security/Meta/Service/Provision/Query）/ 调用 / 结果 |
| `AgentId` / `AgentCapability` / `AgentTask` / `AgentTaskResult` | `os-im/src/agent.rs` | agent ID（newtype String）/ 能力（agent_id/tools/domain）/ 委派任务（id/assigned_to/description/context/deadline）/ 结果（task_id/success/output/side_effects） |
| `OrchestrationStatus` | `os-im/src/orchestrator.rs` | 编排状态（Pending/Delegated{to}/AwaitingConfirmation{reason}/Completed{result}/Failed{reason}） |
| `BlackboardEntry` | `os-im/src/blackboard.rs` | 黑板条目（key/value/written_by/timestamp） |
| `RiskLevel` / `ConfirmationRequest` / `VoteRecord` / `ConfirmationStatus` | `os-im/src/confirmation.rs` | 风险级别（Low/Medium/High/Critical）/ 确认请求（id/task_id/description/risk_level/requested_by/created_at）/ 投票（agent/approve/reason）/ 状态（Pending/UserApproved/UserRejected/QuorumReached{approved}/Expired） |
| `LlmRequest` / `LlmResponse` / `TokenUsage` / `LlmBackendType` | `os-im/src/llm.rs` | LLM 请求（messages/model/temperature/max_tokens/tools）/ 响应（content/tool_calls/usage/finish_reason）/ token 用量 / 后端类型（Cloud/Local/Custom） |
| `ImError` / `ImResult` | `os-im/src/error.rs` | 错误（ConversationNotFound/AgentNotFound/ToolNotFound/TaskCycle/ConfirmationDenied/LlmError/Timeout/Internal；`From<ImError> for ApiError` 已定义，含 `ConfirmationRequired`/`Conflict` 错误码） |

**关键实现**：
- `SqliteConversationStore`：随 meta SQLite 复制 HA；`create_conversation`/`add_message`/`history`/`list_conversations`。
- `Tool`（条件激活型）：`is_available` 返回 false 则不注册到 LLM（呼应 `conditionally_activated` 标识，如 `Some("rpc_available:bitcoin")` 仅当条件满足时注册）。
- 各领域 `Agent`：由各领域 agent 自行实现（如 `StorageAgent`/`VmAgent`/`WalletAgent`），实现 `id`/`capabilities`/`handle_task`/`health`。
- `CentralOrchestrator`（中枢）：`register_agent`/`unregister_agent`/`list_agents`（能力发现，按 tools/domain 路由）；`delegate` 委派任务——**任务依赖图必须无环**，delegate 时做拓扑检查，命中环返回 `TaskCycle`（§3.7.2 约束）；`task_status` 聚合状态。
- `InMemoryBlackboard`（默认）/ `DistributedBlackboard`（基于 os-meta KV）：`put`/`get`/`list`/`clear_for_task`（按 `task.<id>.*` 前缀清理）。
- `DefaultConfirmationGate`：`request`（发起确认请求，含 risk_level）/`user_confirm`（用户在 IM 内确认/拒绝）/`agent_vote`（多 agent 会签）/`status`；**Critical 级需用户确认 + 会签双满足**。
- `OpenAiBackend` / `LocalCandleBackend`（candle/Phi-3-mini/Ollama）/ `CustomBackend`：`chat`（对话补全 + function calling）/`list_models`/`backend_type`。
- 7 个 mock：feature `mock` 下提供，供下游 api/client 测试。

## 4. 输入契约

> 本 agent 依赖的上游 trait，**必须经 `Box<dyn Trait>` 或泛型注入**，不得耦合具体实现。trait 层不硬依赖具体 crate——通过 `Box<dyn Tool>`/`Box<dyn Agent>` 注入，保持开放。

| 上游 trait | 来源 crate | 来源 agent | mock 位置 | 用途 |
|-----------|-----------|-----------|----------|------|
| `DateTime` / `Uuid` / `Event` / `HealthReport` / `TaskId` / `SubscriptionId`（数据类型） | os-core | core-agent | — | 时间戳/事件/健康/任务 ID |
| 各领域 `Tool` 实现（`Box<dyn Tool>`） | 各领域 crate | 各领域 agent | 各 crate 的 mock.rs | LLM function calling 能力（条件激活型） |
| 各领域 `Agent` 实现（`Box<dyn Agent>`） | 各领域 crate | 各领域 agent | 各 crate 的 mock.rs | 中枢委派的目标领域 agent |
| `MetaStore`（KV，黑板与对话存储 HA 复制） | os-meta | meta-agent | `crates/os-meta/src/mock.rs` | DistributedBlackboard / SqliteConversationStore HA 复制 |

**mock 策略**：本 agent 最后接入（批 4），上游各领域 agent 的 mock 应已就绪；接入前用本地 stub（内存 Tool/Agent 返回预设结果）跑通编排；trait 层零硬依赖，任何 `Box<dyn Tool>`/`Box<dyn Agent>` 均可注入。

## 5. 输出要求

### 5.1 实现规范
- **命名**：实现 struct 命名为 `SqliteConversationStore`（`ConversationStore`）、`CentralOrchestrator`（`AgentOrchestrator`）、`InMemoryBlackboard`/`DistributedBlackboard`（`SharedContext`）、`DefaultConfirmationGate`（`ConfirmationGate`）、`OpenAiBackend`/`LocalCandleBackend`/`CustomBackend`（`LlmBackend`）；`Tool`/`Agent` 由各领域实现，不挂 im 前缀。
- **错误**：trait 方法返回 `ImResult<T>`；LLM 调用失败映射 `LlmError`；委派循环映射 `TaskCycle`；确认被拒映射 `ConfirmationDenied`。
- **测试**：每 trait 有单元测；`AgentOrchestrator.delegate` 的循环检测（构造环状依赖图验证返回 TaskCycle）与拓扑路由有专门测；`ConfirmationGate` 的 Critical 双满足逻辑（用户确认 + 会签法定）有测；`LlmBackend` 用 mock HTTP 测 chat/tool_calls 解析。
- **文档**：每个 pub 项有 `///` 中文文档；循环检测算法、黑板 key 命名空间约定（`task.<id>.*`）、Critical 双满足、条件激活（is_available）补 `//` 注释说明"为什么"。

### 5.2 DoD（Definition of Done，验收清单）
- [ ] 7 个 trait 有具体实现（非 `todo!()`）
- [ ] `cargo check -p os-im` 通过
- [ ] `cargo test -p os-im` 通过
- [ ] `cargo clippy -p os-im -- -D warnings` 无警告
- [ ] 为下游提供 7 个 mock（`crates/os-im/src/mock.rs`，feature gate `mock`）
- [ ] 更新 `PROGRESS.md`

## 6. 依赖前置

| 依赖 | 类型 | 说明 |
|------|------|------|
| `core-agent` 交付 os-core 数据类型 | **软依赖** | 契约层，`cargo check` 通过即可 |
| 各领域 agent 交付 `Tool`/`Agent` 实现 | **软依赖** | 本 agent 最后接入；trait 层零硬依赖，可用 stub 跑通，各领域实现就绪后注入 |
| `meta-agent` 交付 `MetaStore` mock | **软依赖** | DistributedBlackboard/ConversationStore 可先用内存 stub |
| 云端 LLM API key / 本地推理服务 | **运行时软依赖** | LlmBackend 可用任一后端；无 key 时 LocalBackend 可用 |

**可立即启动的部分**：
- 数据结构（conversation.rs/tool.rs/agent.rs/orchestrator.rs/blackboard.rs/confirmation.rs/llm.rs 已在契约层）
- `CentralOrchestrator` 委派 + 循环检测（拓扑检查纯算法，不依赖上游）
- `DefaultConfirmationGate` 的 Critical 双满足逻辑（纯状态机）
- `InMemoryBlackboard`（纯内存 KV）
- 7 个 mock——**第一个 PR**，解锁下游 api/client 并行

## 7. 并行性分析

- **可并行实现的 trait**：`ConversationStore` / `SharedContext`（黑板）/ `LlmBackend` 三者相互独立，可多任务并行；`Tool`/`Agent` 是契约由各领域实现，本 agent 不实现具体领域 Tool/Agent（仅提供中枢消费它们）。
- **有内部顺序的 trait**：`AgentOrchestrator`（中枢）依赖 `Agent`（委派目标）/`SharedContext`（黑板）/`ConfirmationGate`（高危确认）——中枢实现须在三者契约稳定后，但实现上可并行开发，集成时串联。
- **瓶颈点**：`CentralOrchestrator.delegate` 的循环检测（拓扑检查正确性）是关键路径；`LlmBackend` 的 function calling 解析（tool_calls 正确解析）是"对话即操作"的核心；`ConfirmationGate` 的 Critical 双满足是安全关键。

## 8. 验收标准

| 维度 | 标准 |
|------|------|
| 编译 | `cargo check -p os-im` 通过 |
| 测试 | `cargo test -p os-im` 通过；覆盖率 ≥ 80%（循环检测、拓扑路由、Critical 双满足、LLM tool_calls 解析、黑板 key 前缀清理是关键路径） |
| 契约 | 未修改 trait 签名（除非有 ADR）；`cargo doc -p os-im` 无警告 |
| mock | 下游可用的 7 个 mock 已提交 |
| 文档 | pub 项有中文 `///`；PROGRESS.md 已更新 |
| 错误映射 | `From<ImError> for ApiError` 完整（含 `ConfirmationRequired`/`Conflict`/`TaskCycle` 错误码） |
| 安全 | 高危操作（requires_confirmation=true）必经确认门；Critical 必用户确认 + 会签双满足 |

## 9. 风险红线

🔴 **严禁**：
- 修改其他 agent 的 crate（仅可改 os-im）
- 修改 trait 签名（7 个 trait 方法增删改须经 ADR + 受影响 agent 会签——影响全体领域 agent）
- **delegate 任务图出现环**（§3.7.2 无环约束红线：delegate 时必须拓扑检查，命中环返回 TaskCycle，禁止放行）
- **高危操作跳过确认门**（requires_confirmation=true 的 Tool 执行前必经 ConfirmationGate；Critical 必用户确认 + 会签双满足）
- trait 层硬依赖具体 crate（必须经 `Box<dyn Tool>`/`Box<dyn Agent>` 注入，保持开放）
- 虚构未发布的依赖（LLM SDK/candle/SQLite 须在 workspace 已注册）
- 删除或重命名既有 pub 项（同上，走 ADR）
- 跳过测试直接提 PR

🟡 **谨慎**：
- **条件激活型 Tool**（is_available）：返回 false 时不注册到 LLM，条件变化时动态注册/注销，须测试覆盖
- **黑板 key 命名空间**（`task.<id>.*`）：clear_for_task 按前缀清理，key 设计须避免误清
- **LLM 后端切换**（Cloud ↔ Local）：降级策略（云端不可用切本地），须 ADR 定降级规则
- **会签法定人数**：Critical 级法定人数阈值须可配置，默认值须 ReviewAgent 评审
- **token 用量计费**：TokenUsage 须准确记录，影响成本核算
- 引入新第三方 crate 须经 ReviewAgent 评估维护性/安全

## 10. 示例工作流

> 典型任务：实现 `AgentOrchestrator.delegate`（中枢委派 + 循环检测）。

1. **开工**：读 `docs/agents/im-agent/PROGRESS.md` + `TASKS.md` + 本规格书 §3/§4。
2. **读契约**：读 `crates/os-im/src/orchestrator.rs`（`AgentOrchestrator`/`OrchestrationStatus`）、`agent.rs`（`Agent`/`AgentTask`/`AgentCapability`）、`error.rs`（`TaskCycle`）；读 §3.7.2 多 agent 协作 ADR。
3. **切分支**：`git checkout agent/im-agent`；建子分支 `agent/im-agent/orchestrator-delegate`。
4. **实现**：在 `crates/os-im/src/` 新建 `impl_orchestrator.rs`（或扩展），定义 `CentralOrchestrator`（持有已注册 agent 表 + 任务依赖图），`impl AgentOrchestrator for CentralOrchestrator`；`delegate` 先做拓扑检查（任务依赖图加入新任务边后判环，命中返回 `TaskCycle`），再按 `AgentCapability.tools`/`domain` 路由到目标 agent，调 `agent.handle_task`，推进 `OrchestrationStatus`（Delegated→Completed/Failed）；高危任务转 `AwaitingConfirmation` 经 ConfirmationGate。
5. **测试**：单元测（构造环状依赖图验证 TaskCycle、拓扑路由正确性、各状态流转、高危转确认门）；`cargo test -p os-im`。
6. **提 PR**：`[im-agent] orchestrator-delegate`，描述含 DoD 勾选 + 循环检测算法说明 + 影响下游（api/client）+ 影响上游（全体领域 agent 经 Box<dyn Agent> 注入）。
7. **响应评审**：按 ReviewAgent 意见修订；契约变更触发 ADR + 会签（影响全体领域 agent）。
8. **更新进度**：合并后更新 `PROGRESS.md` + `TASKS.md`。

## 11. 启动 prompt 模板

> 复制以下 prompt 启动本 agent 会话（替换 `<...>`）：

```
你是 OS 系统的 IM Agent（agent_id: im-agent）——多 agent 协作中枢，系统"大脑"。
你的规格书在 OS_System/docs/agents/im-agent.md——请先完整读取它。
协作约定在 OS_System/docs/agents/_conventions.md——也请读取。
你拥有的契约在 OS_System/crates/os-im/src/*.rs（conversation.rs / tool.rs / agent.rs / orchestrator.rs / blackboard.rs / confirmation.rs / llm.rs / error.rs）。

本次任务：<具体任务描述，或"按 TASKS.md 取下一个任务；优先交付 7 个 mock 解锁下游 api/client">

开工前必读：
1. OS_System/docs/agents/im-agent.md（你的规格）
2. OS_System/docs/agents/_conventions.md（协作约定）
3. OS_System/docs/agents/im-agent/PROGRESS.md（你的进度，恢复上下文）
4. OS_System/docs/agents/im-agent/TASKS.md（你的任务队列）
5. 你拥有的 crate 的 src/*.rs（契约：7 trait + error）
6. 相关 ADR（OS_System/docs/adr/），特别是 §3.7 对话即操作、§3.7.2 多 agent 协作运行时
7. 上游：crates/os-core/src/（Event/HealthReport/TaskId）、crates/os-meta/src/（MetaStore，黑板/对话 HA 复制）

特别注意：你是系统"大脑"——多 agent 协作运行时（§3.7.2）；
delegate 任务图必须无环（拓扑检查，命中环返回 TaskCycle，红线）；
高危操作（requires_confirmation=true）必经确认门，Critical 级须用户确认 + 多 agent 会签双满足；
Tool 条件激活型（is_available 返回 false 不注册到 LLM）；
trait 层零硬依赖具体 crate，经 Box<dyn Tool>/Box<dyn Agent> 注入保持开放；
LLM 云端（OpenAI）/本地（candle/Phi-3-mini/Ollama）/自定义三档后端；
黑板 key 命名空间 task.<id>.*（clear_for_task 按前缀清理）；
你最后接入（批 4），各领域 agent 实现已就绪，你的职责是编排中枢而非实现具体领域业务。
不得修改其他 agent 的 crate；不得破坏 trait 签名（走 ADR，影响全体领域 agent）。
完成后：更新 PROGRESS.md + TASKS.md，按 DoD 自检，提 PR。
```

## 12. 恢复协议

> 会话重启后（上下文丢失），按以下顺序恢复：

1. 读 `OS_System/docs/agents/im-agent.md`（本规格书，重识身份）
2. 读 `OS_System/docs/agents/_conventions.md`（重识协作规则）
3. 读 `OS_System/docs/agents/im-agent/PROGRESS.md`（**最关键**——你之前做到哪了）
4. 读 `OS_System/docs/agents/im-agent/TASKS.md`（下一个任务）
5. `git log agent/im-agent --oneline -20`（看最近提交，了解工作状态）
6. `cargo check -p os-im`（确认当前代码状态）
7. 继续未完成任务；若阻塞，在 PROGRESS.md 记录并在 TASKS.md 标 `blocked`

> **若 PROGRESS.md 不存在**（首次启动或丢失）：从规格书 §3 取契约清单（7 trait：ConversationStore/Tool/Agent/AgentOrchestrator/SharedContext/ConfirmationGate/LlmBackend），从 `git log` 推断进度，重建 PROGRESS.md。优先确认 7 个 mock 是否已交付（未交付则阻塞下游 api/client 并行）；确认 `delegate` 循环检测是否有测试覆盖（红线：任务图必须无环）；确认高危操作是否经确认门（Critical 双满足）。
