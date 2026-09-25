# core-agent 进度日志

## 当前状态
- 阶段：实现完成 / 待 OrchestratorAgent review 合并
- 最后更新：2026-08-04

## 已完成
- [x] **TokioBroadcastBus**（commit 3490bf4）：基于 `tokio::sync::broadcast` 的 `EventBus` 默认实现
  - 每个 `subscribe` spawn 常驻任务，从独立 `broadcast::Receiver` 收事件，按 `Topic` 过滤后 `await` 订阅者的 `handle` boxed future（匹配 ADR-COMPAT-001 签名）
  - `SubscriptionHandle` drop 即 `abort` 任务 = 订阅句柄 drop 即取消语义；`unsubscribe(id)` 提供显式取消
  - 容量 `DEFAULT_CAPACITY = 1024`；慢消费者 `Lagged(n)` 静默继续（背压策略，容量调整需 ADR）
  - 7 个单元测全过：匹配派发 / `Topic::All` 通配 / unsubscribe 停投递 / 多订阅隔离 / 并发 publish(20 任务) / 无订阅 publish / 自定义容量
- [x] **MockEventBus + MockEventSubscriber**（commit 3490bf4，feature `mock`）：解锁下游全部 agent 测试
  - 纯内存、确定性；`published()`/`published_count_for()`/`subscribers_for()`/`subscriber_count()` 供断言
  - `MockEventSubscriber` 记录收到的每个 event，`Clone` 共享记录器
  - 5 个单元测全过
  - feature gate 正确：`#[cfg(feature = "mock")]` 守护，`Cargo.toml` 加 `[features] mock = []`
- [x] **os-common ApiError 构造器文档补全**（commit f446069）：未改任何签名
- [x] DoD 全部勾选通过（见下「DoD 验证记录」）

## DoD 验证记录（真实输出，2026-08-04）
基线（开工前）：`cargo check/test/clippy -p os-core -p os-common` 全绿（0 测试）。
实现后：

| 命令 | 结果 |
|------|------|
| `cargo check -p os-core -p os-common`（默认） | Finished，0 error 0 warning |
| `cargo check -p os-core --features mock` | Finished，0 error 0 warning |
| `cargo test -p os-core -p os-common`（默认） | os-core lib 7 passed；doctest 1 passed；os-common 0 test |
| `cargo test -p os-core --features mock` | lib 12 passed（bus 7 + mock 5）；doctest 1 passed |
| `cargo clippy -p os-core -p os-common --all-targets -- -D warnings`（默认） | Finished，无警告 |
| `cargo clippy -p os-core --features mock --all-targets -- -D warnings` | Finished，无警告 |
| `cargo doc -p os-core -p os-common --no-deps --features mock` | Generated，无警告 |

## 进行中
- 无

## 阻塞
- 无

## 下一步
1. 等 OrchestratorAgent review + 合并到 main（或集成）
2. 视下游反馈：若下游测试发现 MockEventBus 缺某断言 API（如「最近一条 event」「按 kind 过滤」），再补——目前 API 覆盖常见用例
3. 若有 agent 提出 EventBus trait 签名问题，走 ADR 流程（见下「发现但未改的契约问题」）

## commit 列表（本批次）
| sha | message |
|-----|---------|
| 3490bf4 | `[core-agent] feat(os-core): 实现 TokioBroadcastBus + MockEventBus` |
| f446069 | `[core-agent] docs(os-common): 补全 ApiError 构造器中文文档` |

## 发现但未改的契约问题（供 OrchestratorAgent 决定是否走 ADR）
> 以下均为观察记录，**未改任何 trait 签名**（遵守红线 §9）。多数是设计权衡而非缺陷。

1. **`EventBus::subscribe` 取消语义与"drop 即取消"不完全等价**
   trait 签名 `subscribe(...) -> Result<SubscriptionId>` 只返回 ID，不返回 drop guard。
   "订阅句柄 drop 即取消"的语义在当前 trait 下只能由实现自行维持（TokioBroadcastBus 用内部 `SubscriptionHandle` 存注册表实现）。
   若要 trait 层强保证，需把返回类型改为 `Result<(SubscriptionId, SubscriptionGuard)>`——属破坏性签名变更，须 ADR + 全体会签。
   **当前实现已满足规格书语义要求，无需立即改。**

2. **`EventSubscriber::handle` 返回 `Output = ()`，无错误传播**
   规格书 §5.1 说"EventBus 方法返回 `Result<T, CoreError>`；内部错误映射到 `CoreError::EventBus(String)`"，
   但 `handle` 的 future 输出是 `()`，订阅者处理失败时无法上报给 bus（bus 也无法重试/记录）。
   eventbus.rs 文档注释提到"返回 Err 表示处理失败（由 EventBus 决定是否重试/记录）"与实际 `Output=()` 不一致。
   若需错误传播，`handle` 应返回 `Pin<Box<dyn Future<Output = Result<(), CoreError>> + Send + '_>>`——破坏性，须 ADR + 全体会签。
   **建议落 ADR 讨论此不一致；当前不改。**

3. **`TokioBroadcastBus::publish` 在无订阅者时静默丢弃 event**
   `broadcast::send` 在无 receiver 时返回 `Err(SendError)`，本实现 `let _ = ...` 视为正常。
   这符合 pub/sub 语义（发布者不关心谁订阅），但若业务期望"至少一处消费"需上层自行保证。已写进模块文档。无需改 trait。

4. **`Topic` 未实现 `Copy`**
   实现过程中发现 `Topic` 仅 `Clone`（含 `Hash`/`Eq`），导致 `topic_matches` 必须按引用传参。
   给 `Topic` 加 `Copy` 是非破坏性的（它是简单 enum，无数据），但属 pub 类型派生变更，按红线 §9「不增删改 pub 项」精神，未自加。
   **可选优化：未来可加 `#[derive(Copy)]`（非破坏），但建议走轻量 ADR 备案。**

## 影响的下游 agent
- **全体 owner agent**：均依赖 `os-core::EventBus`/`Event`/`Topic`；本次新增 `TokioBroadcastBus`（可直接 `use`）与 `MockEventBus`（dev-dependencies 加 `features = ["mock"]`）
- 下游测试注入示例（待写各自 crate 测试时参考）：
  ```toml
  [dev-dependencies]
  os-core = { workspace = true, features = ["mock"] }
  ```
  ```rust
  use os_core::{MockEventBus, EventBus, MockEventSubscriber, Topic};
  ```
