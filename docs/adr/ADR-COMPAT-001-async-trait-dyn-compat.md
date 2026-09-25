# ADR-COMPAT-001：`Box<dyn>` 用的 async trait 一律 `#[async_trait]`

- **状态**：已采纳（Accepted）
- **日期**：2026-08-04
- **背景决策来源**：HANDOVER §4.2（原会话口述决策，本 ADR 落档）
- **影响范围**：全 workspace 含 async fn 且需 `Box<dyn XxxTrait>` 运行期多态的 trait

## 背景

原契约规范（主文档 §15.1）约定："数据路径 trait 用原生 `async fn in trait`（无 `#[async_trait]`）"，
理由是避免 `#[async_trait]` 的 `Pin<Box<dyn Future>>` 重写带来的每次调用堆分配开销。

但原生 `async fn in trait` 的方法**不能进 vtable**——编译器把它们编译为返回 `impl Future` 的关联类型，
而关联类型不对象安全。凡是需要 `Box<dyn XxxTrait>`（运行期注册不同实现、编译期不知具体类型）的 trait，
原生 async fn 直接导致 `E0038: trait is not dyn compatible`。

第一次 `cargo check --workspace` 在多个 crate 集中暴露此问题：
os-core `EventSubscriber`（原会话已修，改手写 `Pin<Box<dyn Future>>`）、
os-im `Agent`/`Tool`/`LlmBackend`/`SharedContext`/`ConfirmationGate`、
os-discover `PeerCallback`、os-update `CveCallback`、
os-mobile `PushCallback`、os-wallet `ChainAdapter`、os-api `RouteHandler`。

## 决策

**凡是出现在 `Box<dyn XxxTrait>` 里的 async trait，加 `#[async_trait]`**（宏自动把 async fn
转成 `Pin<Box<dyn Future + Send>>`，恢复对象安全）。**纯泛型/单实现、不被 `Box<dyn>` 的 async trait，
保持原生 `async fn in trait`**（零开销）。

### 判断准则

```bash
grep -rn "Box<dyn" crates/
```
命中的 trait → 加 `#[async_trait]`；同时该 crate 的 `Cargo.toml` 加 `async-trait.workspace = true`，
文件顶部 `use async_trait::async_trait;`，trait 上把 `#[allow(async_fn_in_trait)]` 换成 `#[async_trait]`。

### 已应用的 trait 清单（本轮）

| crate | trait | 用途 |
|-------|-------|------|
| os-im | `Agent` | `AgentOrchestrator::register_agent(Box<dyn Agent>)` |
| os-im | `Tool` | 各执行组件 `Box<dyn Tool>` 注入 |
| os-im | `LlmBackend` | IM `Box<dyn LlmBackend>` 注入 |
| os-im | `SharedContext` | 黑板 `Box<dyn SharedContext>` 注入 |
| os-im | `ConfirmationGate` | `Box<dyn ConfirmationGate>` 注入 |
| os-discover | `PeerCallback` | `Discovery::on_peer_discovered(Box<dyn PeerCallback>)` |
| os-update | `CveCallback` | `CveMonitor::subscribe(Box<dyn CveCallback>)` |
| os-mobile | `PushCallback` | `PushSubscriber::subscribe(Box<dyn PushCallback>)` |
| os-wallet | `ChainAdapter` | `RpcRegistry::register_adapter(Box<dyn ChainAdapter>)` |
| os-api | `RouteHandler` | `Gateway::register_component(Box<dyn RouteHandler>)` |

> 注：os-core `EventSubscriber` 由原会话用手写 `Pin<Box<dyn Future>>` 修复（早于本 ADR 落档），
> 思路一致（都是把 async fn 转成 Boxed Future 以恢复对象安全），不再回改。

## 备选方案与否定理由

1. **拆 trait（同步 trait + AsyncXxx trait）**——API 变丑：每个含 async 方法的 trait 拆成两个，
   调用方得同时 bound 两个，污染所有下游签名。否定。
2. **改泛型（`<A: Agent>` 替代 `Box<dyn Agent>`）**——丢运行期多态。而 `AgentOrchestrator::register_agent`
   必须运行期注册不同领域 agent（编译期不知具体类型）。否定。

## 代价

每次 async 方法调用堆分配一次 Future。对 agent 调度 / 事件回调 / 推送等**低频**路径完全可接受；
高频数据路径若被 `Box<dyn>`，需另评（目前契约层无此情形）。

## 对既有约定的影响

打破原 §15.1"无 `#[async_trait]`"的表述。已同步更新：
- workspace 根 `Cargo.toml` 注释（异步模型条款）
- `crates/os-im/src/lib.rs` 顶部契约规范注释

主文档 §15.1 属既有章节，按 HANDOVER §8 红线"不改既有 §0–§16 内容"，仅以本 ADR 增补覆盖，
不回改主文档原文。
