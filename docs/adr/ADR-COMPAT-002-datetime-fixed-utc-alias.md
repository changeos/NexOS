# ADR-COMPAT-002：`os-core::DateTime` 固定为 UTC 时区的 type 别名

- **状态**：已采纳（Accepted）
- **日期**：2026-08-04
- **影响范围**：os-core 公共导出 + 全 workspace 引用 `DateTime` 的下游 crate

## 背景

原 `crates/os-core/src/lib.rs:19` 的导出为：

```rust
pub use chrono::{DateTime, Utc};
```

这是 chrono 原始的**泛型**类型 `DateTime<Tz: TimeZone>`，需要时区泛型参数。但：

1. os-core 自身内部（`types.rs:33`、`eventbus.rs:66`）一律用全路径 `chrono::DateTime<chrono::Utc>`，
   即内部语义本就是 UTC。
2. 下游 crate（os-im、os-discover、os-update、os-mobile 等多处）按"统一从 os-core 引"
   的契约规范，写裸 `DateTime`（如 `pub timestamp: DateTime`），却因 `DateTime` 是泛型类型而触发
   `E0107: missing generics for struct DateTime`。第一次 `cargo check --workspace` 在 os-im 等
   4 个 crate 集中暴露（共 12 处错误源）。

根因是 os-core 把"带泛型参数的原始类型"透传给了下游，而下游期望的是"一个能直接用的具体时间类型"。

## 决策

将 `os-core::DateTime` 改为**固定 UTC 时区的 type 别名**：

```rust
// crates/os-core/src/lib.rs
pub use chrono::Utc;
pub type DateTime = chrono::DateTime<chrono::Utc>;
```

全系统用 **UTC 作为内部时间表示**（日志 / 快照时间戳 / 事件时间 / 任务截止 / NTP 同步时间），
前端展示时再转本地时区。下游裸 `DateTime` 即 `DateTime<Utc>`，类型直接成立。

若某处确需其他时区，显式用 `chrono::DateTime<Tz>`（不走 os-core 的别名）。

## 备选方案与否定理由

1. **下游每处补 `<Utc>` 泛型**（`DateTime<Utc>`）——契约层只动 os-im，但 os-discover / os-update /
   os-mobile 等都有同类问题，逐 crate 补泛型重复且易漏；且 `DateTime<Utc>` 比裸 `DateTime` 啰嗦。
   否定（治标不治本）。
2. **改用 `time` crate**——引入新依赖，且 os-core 已用 chrono。否定。

## 影响

- os-core 公共 API 变更：`DateTime` 从"泛型 re-export"变为"固定 Utc 的 type 别名"。
  **非破坏性**：os-core 内部本就用 `DateTime<Utc>`；下游裸 `DateTime` 本就是写错（只是过去没编译验证过）。
- 连带修复 osd `ntp.rs` 的 `DateTime<Utc>`（2 处，改回裸 `DateTime`，等价）。
- 连带清理 os-security `auth.rs` 的 unused `Utc` import。

## 应用清单

| 文件 | 改动 |
|------|------|
| `crates/os-core/src/lib.rs` | `pub use chrono::{DateTime, Utc};` → `pub use chrono::Utc;` + `pub type DateTime = chrono::DateTime<chrono::Utc>;` |
| `crates/osd/src/ntp.rs` | `DateTime<Utc>` → `DateTime`（2 处），移除 unused `Utc` import |
| `crates/os-security/src/auth.rs` | 移除 unused `Utc` import |
| os-im/os-discover/os-update/os-mobile | 无需改动（裸 `DateTime` 现直接成立） |
