# api-agent 进度日志

## 当前状态
- 阶段：完成（接通真实实现，待主代理合并）
- 最后更新：2026-08-05（real/api-agent 批次：axum/tower/hyper 接入）

## real/api-agent 批次：接通真实实现（本次）
任务：用 axum/tower/hyper 替换 os-api 的 Gateway/WebSocketHub TODO 骨架；os-cli 接通真实解析。

### 已完成
- [x] **os-api/Cargo.toml**：加 `axum.workspace` / `tower.workspace` / `hyper.workspace`（已在 ADR-DEPS-001 注册）。
  - dev-deps：`tower`（util）、`reqwest`（HTTP 客户端集成测）、`tokio-tungstenite 0.29`（WS 客户端集成测，与 axum 0.8 ws 版本对齐）。
- [x] **InProcessGateway**（`gateway_impl.rs` + 新 `http.rs`）：
  - `start` 真实 `tokio::net::TcpListener::bind` + `axum::serve(listener, router)` + `with_graceful_shutdown`；`stop` 触发 shutdown 信号 + join serve task（1s 超时）。
  - 新模块 `http.rs`：`build_router` 把 `RouteRegistry` 映射为 axum Router（`:id` → `{id}`、`*` → `{*wildcard}` 适配 axum 0.8 路径语法），每条路由共享 `dispatch_handler`（axum Request ↔ ApiRequest 转换 + 复用 `dispatch` 分发算法，保持 57 测不变）。
  - 状态共享：`InProcessGateway` 改 `Clone`（Arc 包装 components/registry/listening/ws_hub/jwt/ws_path/serve_handle），`GatewayState { gateway: Arc<InProcessGateway>, jwt }` 供 axum handler 持有。
  - dispatch 改用 `Arc<dyn RouteHandler>`（不再 take-out/put-back），消除跨 await 锁风险。
- [x] **WebSocketHub 真实推送**（`http.rs::ws_handler` + `run_ws`）：
  - `axum::extract::ws::WebSocketUpgrade` 握手 → `WsHub::subscribe_raw` → `tokio::select!` 循环把 broadcast 推送的 `WsMessage` 序列化为 Text 帧写回客户端；断开时 `unsubscribe_raw`。
  - `WsHub` 改 `#[derive(Clone)]`（Arc 包装内部状态），`InProcessGateway::ws_hub()` 返回 clone 供 axum handler。
  - 默认 WS 路径 `/ws`，可用 `set_ws_path` 配置或关闭。
- [x] **JWT 认证中间件真实**（`http.rs::extract_principal` + `InProcessGateway::set_jwt_issuer`）：
  - HTTP 入口从 `Authorization: Bearer <token>` 头解析 JWT（经 `os_security::JwtIssuerImpl` 真实验签），构造 `Principal` 填充 `ApiRequest.auth`，下游 `AuthMiddleware`/路由 `requires_auth` 直接消费。
  - 注：`os_security::JwtIssuer` trait 用原生 `async fn`（非 dyn 兼容），按红线不改 trait，改用具体类型 `Arc<JwtIssuerImpl>` 注入（仅 `JwtIssuerImpl` 实现 trait，编译器建议直接使用）。
- [x] **os-cli 接通真实解析**（新 `parse.rs`）：
  - 自实现 `parse_args`（workspace 未注册 clap，按无依赖骨架策略）：`--name=value`（推荐）/ `--flag`（布尔）/ `-x`（短选项标志）/ `--`（分隔）/ 位置参数。
  - 无 schema 时的确定性策略：无 `=` 的 `--name` 视为布尔标志（POSIX 惯例）；需带值必须用 `--name=value`。
  - `ParsedArgs { opts, flags, positional }` 结构化输出，供各业务 Command `execute` 消费。
- [x] **TLS 留 TODO**：`start` 仅校验 `TlsConfig` 路径非空，真实 rustls/axum-server TLS 监听阻塞于 feature 未启用（按约定留 TODO，不虚构依赖）。

### 测试
- os-api：**53 测**（41 原有 + 12 新增：http.rs 9 + gateway_impl 真实监听集成测 1 + ws_impl 真实 WS 端到端 1 + ws_impl Clone 共享 1）
  - `start_serves_real_http_and_dispatches`：reqwest 真实 HTTP 请求验证 axum::serve 端到端（200 + 404）。
  - `real_ws_endpoint_pushes_messages`：tokio-tungstenite 真实 WS 握手 + broadcast 推送 + 客户端收到序列化 WsMessage。
  - `jwt_principal_extracted_from_bearer`：JwtIssuerImpl 签发 → HTTP 入口解析 → handler 见到 Principal。
- os-cli：**29 测**（16 原有 + 13 新增：parse.rs 12 + command_tree 端到端 1）
- 合计 **82 测**（≥ 57 + 25 新增）。

### DoD 勾选
- [x] axum Router 真实构建 + 路由注册（可测，`build_router` + 9 单测）
- [x] WS 真实推送（axum WebSocket，可测，`real_ws_endpoint_pushes_messages` 端到端）
- [x] JWT 认证中间件真实（os-security JwtIssuerImpl）
- [x] `cargo check/test/clippy -p os-api -p os-cli --features mock -- -D warnings` 全绿
- [x] 测试数 82 ≥ 57 + 新增

## 前序工作（无依赖骨架批次，已合并）



## 已完成
- [x] 接续前 owner 未完成的"无依赖骨架"工作（commit: 本批，见 `git log`）
- [x] os-api（4 trait + 实现 + Mock，57 测合计）：
  - `RouteRegistry`（路由注册表 + 匹配算法）：method+path 参数 `:id`、通配 `*`、静态优先于参数、冲突检测、修复 `best.unwrap()` 移动 bug
  - `MiddlewareChain` + 4 个中间件实现（`#[async_trait]` 按 ADR-COMPAT-001）：
    - `AuthMiddleware`：鉴权（401/403，角色集合比较）
    - `RateLimitMiddleware`/`StatefulRateLimiter`：令牌桶（按源 IP/用户，修复 `TokenBucket` last_refill 初始化 bug）
    - `SlidingWindowRateLimiter`：滑动窗口
    - `TlsMiddleware`：配置校验
    - `AuditMiddleware`：审计记录生成
  - `InProcessGateway`（Gateway 骨架）：register_component 聚合路由、中间件链分发（before→路由匹配→handle→逆序 after）、start/stop 状态、TLS 校验；真实 Axum bind 留 TODO
  - `WsHub`（WebSocketHub 内存实现）：订阅/广播/定向推送（tokio broadcast 通道），修复内部方法名遮蔽（broadcast_n/subscribe_raw）
  - 3 个 Mock：`MockGateway` / `MockRouteHandler` / `MockWebSocketHub`
  - 41 单元测（路由匹配 13 + 中间件链 8 + 限流算法 3 + WS hub 6 + 网关分发 7 + Mock 4）
- [x] os-cli（2 trait + 实现 + Mock，16 测）：
  - `TextFormatter` / `JsonFormatter` / `YamlFormatter`（OutputFormatter）：YAML 极简自实现（serde_yaml 未注册，按无依赖骨架策略）
  - `CommandTree`：树形命令注册/解析/分发
  - `format_output`：按 OutputFormat 选择格式化器
  - 2 个 Mock：`MockCommand` / `MockOutputFormatter`
  - 16 单元测（格式渲染 6 + 命令树 5 + Mock 5）
- [x] DoD 自检：`cargo check/test/clippy -p os-api -p os-cli --features mock --all-targets -- -D warnings` 全绿

## 进行中
（无）

## 阻塞（本 real 批次后剩余）
- ⛔ TLS 证书加载（rustls/axum-server feature 未注册）：`start` 仅校验路径非空，仍按明文 HTTP 监听——待 rustls feature 注册后改为真实 TLS 终止。
- ⛔ serde_yaml（YamlFormatter 极简自实现，待注册后替换）。
- ℹ️ `JwtIssuer` trait 非 dyn 兼容（原生 async fn）：当前用具体 `JwtIssuerImpl` 注入；若未来需多 issuer 多态，须 os-security 走 ADR 改 `#[async_trait]`。

前批阻塞（已解除）：~~真实 Axum/tower HTTP 监听~~（本批已接通）、~~AxumWsHub 与 axum::extract::ws 对接~~（本批已接通）。

## 契约兼容性修正（ADR-COMPAT-001 应用，非破坏性）
前 owner 的实现文件此前未接入 lib.rs（孤儿文件），接入时暴露并修正以下兼容性问题：
- `Middleware` / `Gateway` / `WebSocketHub` 三 trait 原用原生 `async fn`（`#[allow(async_fn_in_trait)]`），
  但实现以 `Box<dyn Middleware>` / `#[async_trait]` impl 使用 → 按 ADR-COMPAT-001 改 `#[async_trait]`（恢复 dyn 兼容，不改方法签名/参数/返回类型）
- `routing.rs` `match_request` 的 `best.unwrap()` 移动 bug（PathParams 非 Copy）→ 改 `as_ref` 比较
- `ws_impl.rs` 内部方法名与 trait 方法遮蔽（broadcast 返回 usize vs async）→ 重命名内部方法 broadcast_n/send_to_n/subscribe_raw/unsubscribe_raw
- `middleware_impl.rs` `TokenBucket` last_refill==0 初始化 bug → 改 `Option<f64>` 首次定锚

## 下一步
1. 主代理统一合并 `agent/api-agent` 分支
2. 待 axum/tower 在 workspace 注册后接入真实 HTTP/WS 监听
3. 待 serde_yaml 注册后替换 YamlFormatter 实现

## DoD 勾选（规格 §5.2）
- [x] 6 个 trait 有具体实现（Gateway/RouteHandler/Middleware/WebSocketHub + Command/OutputFormatter）
- [x] `cargo check -p os-api -p os-cli` 通过
- [x] `cargo test -p os-api -p os-cli` 通过（41 + 16 = 57 测）
- [x] `cargo clippy -p os-api -p os-cli -- -D warnings` 无警告
- [x] mock 已提交（os-api/src/mock.rs 3 个 + os-cli/src/mock.rs 2 个，feature gate `mock`）
- [x] PROGRESS.md 已更新
