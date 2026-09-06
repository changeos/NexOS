# `api-agent` 规格书

> 显示名：`API Agent`
> 拥有 crate：`os-api`, `os-cli`
> 启动批次：`4`

## 1. 身份

| 字段 | 值 |
|------|-----|
| agent_id | `api-agent` |
| 显示名 | API Agent |
| 拥有的 crate | os-api, os-cli |
| Git 长期分支 | `agent/api-agent` |
| 上游依赖 agent | 全体（网关聚合）：各业务组件经 `RouteHandler` 注册路由；security（Principal/JwtIssuer）、core（Event/TaskId/SubscriptionId/Severity） |
| 下游被依赖 agent | `client-agent`（移动/桌面客户端消费 API）、`devops-agent`（CI 守护 cargo check --workspace） |
| 启动批次 | `4`，同批可与 im-agent / client-agent 并行（api 是批 4 网关聚合收尾） |

## 2. 使命陈述

**一句话职责**：实现 OS 系统的内嵌 API 网关（§9.1#10：不独立成层）与管理 CLI——Axum REST + WebSocket 网关（各业务组件自注册路由，网关聚合对外；tower 中间件链 TLS/限流/认证/审计；WS 推事件/进度/通知对接 EventBus），树形管理 CLI（连接远端或本地直调 osd）。

**边界**：
- ✅ 做：实现 `Gateway`（register_component/list_routes/start/stop，Axum 聚合）、`RouteHandler`（routes/handle，各组件适配器）、`Middleware`（before/after，含 AuthMiddleware/RateLimitMiddleware/TlsMiddleware/AuditMiddleware）、`WebSocketHub`（broadcast/send_to/subscribe/unsubscribe，对接 EventBus）、`Command`（name/description/subcommands/execute，树形 CLI）、`OutputFormatter`（format，Text/Json/Yaml）；为下游 client 提供 mock。
- ❌ 不做：不实现其他 agent 的 crate（各业务组件自行实现 `RouteHandler` 注册路由，本 agent 仅提供网关与聚合）；不修改 trait 签名（破坏性变更须经 ADR）；**不把网关独立成层/服务**（§9.1#10：内嵌于 osd，须 ADR 才能改）；不实现具体业务逻辑（路由落到组件后由组件处理）；不下沉 EventBus 本身（归 os-core，WS Hub 对接它）；不实现 CLI 各业务命令的具体逻辑（各业务 Command 由各领域提供，本 agent 提供命令框架与格式化器）。

## 3. 拥有的契约

| crate | trait | 契约路径（相对 `OS_System/`） | 实现优先级 |
|-------|-------|-------------------------------|-----------|
| os-api | `Gateway` | `crates/os-api/src/gateway.rs` | P0（网关聚合核心） |
| os-api | `RouteHandler` | `crates/os-api/src/gateway.rs` | P0（每组件实现它注册路由） |
| os-api | `Middleware` | `crates/os-api/src/middleware.rs` | P1（中间件链，Auth/RateLimit/TLS/Audit） |
| os-api | `WebSocketHub` | `crates/os-api/src/websocket.rs` | P1（WS 推送） |
| os-cli | `Command` | `crates/os-cli/src/command.rs` | P1（树形 CLI） |
| os-cli | `OutputFormatter` | `crates/os-cli/src/format.rs` | P2（Text/Json/Yaml 格式化） |

**关键数据结构**（本 agent 需实现并对外暴露的 struct/enum）：

| 类型 | 路径 | 说明 |
|------|------|------|
| `HttpMethod` / `RouteSpec` / `ApiRequest` / `ApiResponse` / `TlsConfig` | `os-api/src/gateway.rs` | HTTP 方法（Get/Post/Put/Delete/Patch）/ 路由规格（method/path/handler_component/requires_auth/required_roles）/ 请求（method/path/headers/body/auth）/ 响应（status/body/headers）/ TLS 配置（cert_path/key_path） |
| `MiddlewareDecision` / `AuthMiddleware` / `RateLimitMiddleware` / `TlsMiddleware` / `AuditMiddleware` | `os-api/src/middleware.rs` | before 决策（Continue/Reject{status,body}/RateLimited）/ 4 个中间件 struct |
| `WsMessage` | `os-api/src/websocket.rs` | WS 推送消息（Event{event}/Progress{task_id,progress,step}/Notification{message,severity}/Error{code,message}） |
| `OutputFormat` / `CommandSpec` / `ArgSpec` / `CommandContext` / `CommandOutput` | `os-cli/src/command.rs` | 输出格式（Text/Json/Yaml）/ 子命令规格 / 参数规格 / 执行上下文（api_endpoint/token/format）/ 输出（success/data/message） |
| `TextFormatter` / `JsonFormatter` / `YamlFormatter` | `os-cli/src/format.rs` | 3 个格式化器 struct |
| `ApiGatewayError` / `ApiGatewayResult` / `CliError` / `CliResult` | 各 crate error.rs | 错误（`From<ApiGatewayError> for ApiError`、`From<CliError> for ApiError` 须实现） |

**关键实现**：
- `AxumGateway`：基于 Axum + tower 中间件链；`register_component` 接收 `Box<dyn RouteHandler>` 聚合路由表；`list_routes` 聚合全部组件路由；`start` 启动监听（可选 TLS via `TlsConfig`）；认证身份复用 `os_security::Principal`（§3.6）。
- 各组件 `RouteHandler` 适配器：由各业务组件实现（如 `StorageRouteHandler`/`ComputeRouteHandler`），声明 `routes()`（RouteSpec 列表）并 `handle(ApiRequest)`。
- 4 个中间件：`AuthMiddleware`（解析 JWT/Session 填充 `ApiRequest.auth`）、`RateLimitMiddleware`（令牌桶按 IP/用户限流，超限 429）、`TlsMiddleware`（TLS 终止/卸载）、`AuditMiddleware`（记录请求/响应到审计日志，§3.16）。
- `AxumWsHub`：对接 Axum WS 与 os-core EventBus；`subscribe` 返回 `SubscriptionId`，`broadcast`/`send_to` 推 `WsMessage`（Event/Progress/Notification/Error）。
- `Command` 树形：各业务命令（如 `StorageCommand`/`NetworkCommand`）实现 `subcommands()` 构成命令树（如 `os storage dataset list`）；`execute` 在 `CommandContext`（含 `api_endpoint` None=本地直调模式与 osd 同进程）下执行。
- `TextFormatter`/`JsonFormatter`/`YamlFormatter`：把 `CommandOutput` 渲染为字符串。
- 多个 mock：feature `mock` 下提供 `MockGateway`/`MockRouteHandler`/`MockWebSocketHub`/`MockCommand`/`MockOutputFormatter`，供下游 client 测试。

## 4. 输入契约

> 本 agent 依赖的上游 trait，**必须经 `Box<dyn Trait>` 或泛型注入**，不得耦合具体实现。

| 上游 trait | 来源 crate | 来源 agent | mock 位置 | 用途 |
|-----------|-----------|-----------|----------|------|
| `Principal`（认证身份） | os-security | security-agent | `crates/os-security/src/mock.rs` | AuthMiddleware 解析 JWT 填充 ApiRequest.auth |
| `Event` / `TaskId` / `SubscriptionId` / `Severity`（数据类型） | os-core | core-agent | — | WS 推送事件/进度/通知 |
| `EventBus`（事件总线） | os-core | core-agent | `crates/os-core/src/mock.rs` | AxumWsHub 对接 EventBus 推送 |
| `ApiErrorCode`（数据类型） | os-common | core-agent | — | WsMessage::Error 错误码 |
| 各业务 `RouteHandler` 实现（`Box<dyn RouteHandler>`） | 各业务 crate | 各业务 agent | 各 crate 的 mock.rs | 网关聚合各组件路由 |

**mock 策略**：本 agent 批 4 接入，上游 security/core 的 mock 应已就绪；各业务 RouteHandler 由各领域实现并注入，接入前用 stub RouteHandler 跑通聚合；trait 层不硬依赖具体业务 crate。

## 5. 输出要求

### 5.1 实现规范
- **命名**：实现 struct 命名为 `AxumGateway`（`Gateway`）、`AxumWsHub`（`WebSocketHub`）、`AuthMiddleware`/`RateLimitMiddleware`/`TlsMiddleware`/`AuditMiddleware`（`Middleware`，struct 已在契约层声明）、`TextFormatter`/`JsonFormatter`/`YamlFormatter`（`OutputFormatter`，struct 已声明）；各业务 `Command`/`RouteHandler` 由各领域实现。
- **错误**：`Gateway`/`RouteHandler`/`Middleware`/`WebSocketHub` 方法返回 `ApiGatewayResult<T>`；`Command` 方法返回 `CliResult<T>`；须实现 `From<ApiGatewayError> for ApiError` 与 `From<CliError> for ApiError`。
- **测试**：`AxumGateway` 的路由聚合与请求分发有集成测（注入 mock RouteHandler）；中间件链（Auth 解析/RateLimit 限流/TLS/Audit）各 Middleware 有单测；`AxumWsHub` 的订阅/广播/定向推送有测；`Command` 树形解析与 `OutputFormatter` 三种格式渲染有单测。
- **文档**：每个 pub 项有 `///` 中文文档；中间件链顺序、内嵌网关设计（§9.1#10）、WS 与 EventBus 对接、CLI 本地直调模式补 `//` 注释说明"为什么"。

### 5.2 DoD（Definition of Done，验收清单）
- [ ] 6 个 trait 有具体实现（非 `todo!()`）
- [ ] `cargo check -p os-api` 与 `cargo check -p os-cli` 通过
- [ ] `cargo test -p os-api` 与 `cargo test -p os-cli` 通过
- [ ] `cargo clippy -p os-api -p os-cli -- -D warnings` 无警告
- [ ] 为下游提供 mock（`crates/os-api/src/mock.rs`、`crates/os-cli/src/mock.rs`，feature gate `mock`）
- [ ] 更新 `PROGRESS.md`

## 6. 依赖前置

| 依赖 | 类型 | 说明 |
|------|------|------|
| `core-agent` 交付 os-core/os-common 数据类型 + EventBus mock | **软依赖** | 契约层 + WS 对接 EventBus |
| `security-agent` 交付 `Principal` 类型 + `JwtIssuer` mock | **软依赖** | AuthMiddleware 解析身份；可用 stub Principal 跑通 |
| 各业务 agent 交付 `RouteHandler` 实现 | **软依赖** | 网关聚合各组件；trait 层零硬依赖，可用 stub 跑通 |
| Axum / tower / tokio-tungstenite 在 workspace 注册 | **硬阻塞** | 第三方依赖须已注册（虚构依赖违反红线） |

**可立即启动的部分**：
- 数据结构（gateway.rs/middleware.rs/websocket.rs/command.rs/format.rs 已在契约层）
- `AxumGateway` 聚合骨架（注入 stub RouteHandler）
- 4 个中间件 struct 的实现（Auth/RateLimit/TLS/Audit 逻辑独立）
- `OutputFormatter` 三种格式渲染（纯函数）
- 多个 mock——**第一个 PR**，解锁下游 client 并行

## 7. 并行性分析

- **可并行实现的 trait**：`Gateway` / `Middleware`（4 个 struct 独立）/ `WebSocketHub` / `Command` / `OutputFormatter` 五组相互独立，可多任务并行；os-api 与 os-cli 两 crate 也可并行。
- **有内部顺序的 trait**：`Gateway`（聚合）须消费 `RouteHandler`（各组件适配器）与 `Middleware`（链）——但实现上各 trait 独立，集成时串联中间件链与路由表。
- **瓶颈点**：`AxumGateway` 的路由聚合与中间件链顺序（TLS→RateLimit→Auth→Audit）是关键路径；`AxumWsHub` 与 EventBus 对接的实时性；`Command` 树形解析的参数路由正确性。

## 8. 验收标准

| 维度 | 标准 |
|------|------|
| 编译 | `cargo check -p os-api -p os-cli` 通过 |
| 测试 | `cargo test -p os-api -p os-cli` 通过；覆盖率 ≥ 80%（路由聚合、中间件链、WS 推送、命令树解析、格式渲染是关键路径） |
| 契约 | 未修改 trait 签名（除非有 ADR）；`cargo doc -p os-api -p os-cli` 无警告 |
| mock | 下游可用的 mock 已提交 |
| 文档 | pub 项有中文 `///`；PROGRESS.md 已更新 |
| 错误映射 | `From<ApiGatewayError> for ApiError` 与 `From<CliError> for ApiError` 完整 |
| 设计 | 网关内嵌于 osd（§9.1#10），不独立成层 |

## 9. 风险红线

🔴 **严禁**：
- 修改其他 agent 的 crate（仅可改 os-api / os-cli）
- 修改 trait 签名（6 个 trait 方法增删改须经 ADR + 受影响 agent 会签）
- **把网关独立成层/服务**（§9.1#10 红线：内嵌于 osd，须 ADR 才能改架构）
- 虚构未发布的依赖（Axum/tower/tokio-tungstenite 须在 workspace 已注册）
- trait 层硬依赖具体业务 crate（必须经 `Box<dyn RouteHandler>`/`Box<dyn Middleware>` 注入）
- 中间件链顺序错乱（TLS→RateLimit→Auth→Audit，认证须在限流后、审计最外层）
- 删除或重命名既有 pub 项（同上，走 ADR）
- 跳过测试直接提 PR

🟡 **谨慎**：
- 改中间件链顺序（影响安全与性能，须 ADR + 会签 security-agent）
- 改 WS 推送协议（WsMessage 结构变更影响 client，须 ADR + 会签 client-agent）
- CLI 本地直调模式（api_endpoint=None 与 osd 同进程零网络）须与 osd 集成验证
- TLS 证书加载（cert_path/key_path）须安全处理，私钥不记日志
- 引入新第三方 crate 须经 ReviewAgent 评估维护性/安全

## 10. 示例工作流

> 典型任务：实现 `AxumGateway`（路由聚合 + 中间件链 + 启停）。

1. **开工**：读 `docs/agents/api-agent/PROGRESS.md` + `TASKS.md` + 本规格书 §3/§4。
2. **读契约**：读 `crates/os-api/src/gateway.rs`（`Gateway`/`RouteHandler`/`RouteSpec`/`ApiRequest`/`ApiResponse`）、`middleware.rs`（`Middleware`/4 struct）、`error.rs`；读 `crates/os-security/src/`（Principal）；读 §3.6 / §9.1#10 内嵌网关 ADR。
3. **切分支**：`git checkout agent/api-agent`；建子分支 `agent/api-agent/axum-gateway`。
4. **实现**：在 `crates/os-api/src/` 新建 `impl_gateway.rs`（或扩展），定义 `AxumGateway`（持有组件路由表 + 中间件链），`impl Gateway for AxumGateway`；`register_component` 聚合 `RouteHandler.routes()`；`start` 构建 Axum Router（按 RouteSpec 挂载，中间件链 TLS→RateLimit→Auth→Audit），可选 TLS 监听；`list_routes` 聚合返回。
5. **测试**：集成测（注入 mock RouteHandler 验证路由聚合与分发、中间件链各环节、TLS/非 TLS 启停）；`cargo test -p os-api`。
6. **提 PR**：`[api-agent] axum-gateway`，描述含 DoD 勾选 + 内嵌网关说明（§9.1#10）+ 中间件链顺序 + 影响下游（client）。
7. **响应评审**：按 ReviewAgent 意见修订；契约变更触发 ADR + 会签（security/client）。
8. **更新进度**：合并后更新 `PROGRESS.md` + `TASKS.md`。

## 11. 启动 prompt 模板

> 复制以下 prompt 启动本 agent 会话（替换 `<...>`）：

```
你是 OS 系统的 API Agent（agent_id: api-agent）。
你的规格书在 OS_System/docs/agents/api-agent.md——请先完整读取它。
协作约定在 OS_System/docs/agents/_conventions.md——也请读取。
你拥有的契约在 OS_System/crates/os-api/src/*.rs（gateway.rs / middleware.rs / websocket.rs / error.rs）
与 OS_System/crates/os-cli/src/*.rs（command.rs / format.rs / error.rs）。

本次任务：<具体任务描述，或"按 TASKS.md 取下一个任务；优先交付 mock 解锁下游 client">

开工前必读：
1. OS_System/docs/agents/api-agent.md（你的规格）
2. OS_System/docs/agents/_conventions.md（协作约定）
3. OS_System/docs/agents/api-agent/PROGRESS.md（你的进度，恢复上下文）
4. OS_System/docs/agents/api-agent/TASKS.md（你的任务队列）
5. 你拥有的 crate 的 src/*.rs（契约：os-api 4 trait + os-cli 2 trait）
6. 相关 ADR（OS_System/docs/adr/），特别是 §3.6 API 网关、§9.1#10 内嵌网关不独立
7. 上游：crates/os-security/src/（Principal，AuthMiddleware）、crates/os-core/src/（EventBus/Event/TaskId，WS 对接）

特别注意：内嵌网关不独立成层（§9.1#10，内嵌于 osd，须 ADR 才能改）；
各业务组件经 RouteHandler 自注册路由，网关聚合对外（不实现具体业务逻辑）；
中间件链顺序 TLS→RateLimit→Auth→Audit（认证在限流后，审计最外层）；
WebSocket 推事件/进度/通知对接 os-core EventBus；
CLI 树形命令（Command 暴露 subcommands），本地直调模式（api_endpoint=None 与 osd 同进程零网络）；
trait 层零硬依赖具体业务 crate，经 Box<dyn RouteHandler>/Box<dyn Middleware> 注入；
认证身份复用 os_security::Principal。
不得修改其他 agent 的 crate；不得破坏 trait 签名（走 ADR）。
完成后：更新 PROGRESS.md + TASKS.md，按 DoD 自检，提 PR。
```

## 12. 恢复协议

> 会话重启后（上下文丢失），按以下顺序恢复：

1. 读 `OS_System/docs/agents/api-agent.md`（本规格书，重识身份）
2. 读 `OS_System/docs/agents/_conventions.md`（重识协作规则）
3. 读 `OS_System/docs/agents/api-agent/PROGRESS.md`（**最关键**——你之前做到哪了）
4. 读 `OS_System/docs/agents/api-agent/TASKS.md`（下一个任务）
5. `git log agent/api-agent --oneline -20`（看最近提交，了解工作状态）
6. `cargo check -p os-api -p os-cli`（确认当前代码状态）
7. 继续未完成任务；若阻塞，在 PROGRESS.md 记录并在 TASKS.md 标 `blocked`

> **若 PROGRESS.md 不存在**（首次启动或丢失）：从规格书 §3 取契约清单（os-api 4 trait：Gateway/RouteHandler/Middleware/WebSocketHub + os-cli 2 trait：Command/OutputFormatter），从 `git log` 推断进度，重建 PROGRESS.md。优先确认 mock 是否已交付（未交付则阻塞下游 client 并行）；确认网关是否内嵌（§9.1#10，禁止独立成层）；确认中间件链顺序是否正确。
