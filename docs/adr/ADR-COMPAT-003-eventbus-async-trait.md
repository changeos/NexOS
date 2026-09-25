# ADR-COMPAT-003：`EventBus` 补加 `#[async_trait]`（ADR-COMPAT-001 漏修补全）

- **状态**：已采纳（Accepted）
- **日期**：2026-08-04
- **背景**：批 0 orchestrator-agent 实现 osd 骨架时发现，并报告
- **影响范围**：os-core `EventBus` trait 及其实现（仅 os-core 内部）

## 背景

ADR-COMPAT-001 确立了横切规则："凡是出现在 `Box<dyn XxxTrait>` 里的 async trait，加 `#[async_trait]`"。
落档该 ADR 时，os-core 的 dyn 兼容修复只覆盖了 `EventSubscriber`（手写 `Pin<Box<dyn Future>>`），
**遗漏了 `EventBus` 本身**——`EventBus` 的三个方法（`publish`/`subscribe`/`unsubscribe`）仍是原生
`async fn in trait`，导致 `Box<dyn EventBus>` 触发 `E0038: trait is not dyn compatible`。

批 0 期间，orchestrator-agent 实现 `SystemdOrchestrator` 时发现：规格书 §4 期望编排器以
`Box<dyn EventBus>` 注入用于事件上报，但当前 `EventBus` 非 dyn 兼容，无法实现。core-agent 独立
实现 `TokioBroadcastBus` 时也印证了同一问题。

## 决策

**给 `EventBus` 补加 `#[async_trait]`**，与 ADR-COMPAT-001 规则一致。本次为该 ADR 的漏修补全，
非新决策。`EventSubscriber` 保持手写 `Pin<Box<dyn Future>>` 不动（已 dyn 兼容，且 core-agent 的
实现已匹配该签名，无回改必要）。

## 影响评估（极小）

- 当前 workspace 仅 2 处 `impl EventBus for`：`TokioBroadcastBus` 与 `MockEventBus`，
  **均在 os-core 内部**（core-agent 同批实现）。两处 impl 块同步加 `#[async_trait]`。
- 下游（os-im/os-api 等）目前无 `impl EventBus for` 或 `Box<dyn EventBus>` 代码
  （实现尚未开始），**零外部破坏**。
- 未来下游若要注入 EventBus，现可直接 `Box<dyn EventBus>`。

## 应用清单

| 文件 | 改动 |
|------|------|
| `crates/os-core/Cargo.toml` | 加 `async-trait.workspace = true` |
| `crates/os-core/src/eventbus.rs` | `EventBus` trait 加 `#[async_trait]` + `use`；移除 `#[allow(async_fn_in_trait)]`；补 doc 说明 |
| `crates/os-core/src/bus.rs` | `impl EventBus for TokioBroadcastBus` 加 `#[async_trait]` + `use` |
| `crates/os-core/src/mock.rs` | `impl EventBus for MockEventBus` 加 `#[async_trait]` + `use` |

## 验证

- `cargo check -p os-core --features mock`：通过
- `cargo test -p os-core --features mock`：12 passed
- 临时编译期断言 `fn _assert(b: Box<dyn EventBus>)` 编译通过，证明已 dyn 兼容
