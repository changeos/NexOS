# `monitor-agent` 规格书

> 显示名：`Monitor Agent`
> 拥有 crate：`os-services`（部分 trait）
> 启动批次：`3`

## 1. 身份

| 字段 | 值 |
|------|-----|
| agent_id | `monitor-agent` |
| 显示名 | Monitor Agent |
| 拥有的 crate | os-services（仅 `Monitor` 一 trait） |
| Git 长期分支 | `agent/monitor-agent` |
| 上游依赖 agent | `core-agent`（`DateTime`） |
| 下游被依赖 agent | `api-agent`（监控/告警/日志查询路由）、`power-agent`（硬件告警，软）、`update-agent`（健康探活，软） |
| 启动批次 | `3`，同批可与 backup/media/files/devtools/power/discover/guest/provision/update 并行（与其他五个 service-agent 共享 os-services crate 但独占 monitor.rs，须协调分支冲突） |

## 2. 使命陈述

**一句话职责**：为 OS 系统提供可观测性能力——metric 采集与查询（Counter/Gauge/Histogram）、日志收集与过滤 tail、告警规则引擎（条件 + 持续时长阈值）。

**边界**：
- ✅ 做：实现 `os-services` 的 `Monitor`（record_metric/query_metrics/add_alert_rule/list_alerts/tail_logs）；为下游提供 mock。
- ❌ 不做：不实现备份/媒体/文件/开发工具/电源（归其他五个 service-agent，同 crate 不同文件，不得改动）；不修改 trait 签名（须走 ADR）；不直接读硬件传感器（归 power-agent，仅接收其上报的 metric）；不实现分布式追踪后端（仅采集与查询）。

## 3. 拥有的契约

> 本 agent 从原 `service-agent` 拆分而来（§2.1 拆分理由：service 七组件全拆）。仅拥有以下 trait，位于 `os-services` crate（与其他五个 service-agent 共享 crate 但独占 monitor.rs）。

| crate | trait | 契约路径（相对 `OS_System/`） | 实现优先级 |
|-------|-------|-------------------------------|-----------|
| os-services | `Monitor` | `crates/os-services/src/monitor.rs` | P1（批 3 核心能力） |

**关键数据结构**（本 agent 需实现并对外暴露的 struct/enum，定义在 `monitor.rs`）：

| 类型 | 路径 | 说明 |
|------|------|------|
| `MetricKind`（Counter/Gauge/Histogram） | `os-services/src/monitor.rs` | metric 类型 |
| `Metric`（name/kind/value/labels/timestamp） | `os-services/src/monitor.rs` | 单个 metric 数据点（labels 多维属性） |
| `LogLevel`（Trace/Debug/Info/Warn/Error） | `os-services/src/monitor.rs` | 日志级别 |
| `LogEntry`（level/target/message/timestamp/fields） | `os-services/src/monitor.rs` | 单条日志（target = 模块/组件名） |
| `AlertSeverity`（Info/Warning/Critical） | `os-services/src/monitor.rs` | 告警严重程度 |
| `AlertRule`（name/metric/condition/for_duration_secs/severity） | `os-services/src/monitor.rs` | 告警规则（condition 如 `">0.9"`，for_duration_secs 避免抖动） |
| `Alert`（rule_name/severity/fired_at/resolved/message） | `os-services/src/monitor.rs` | 已触发的告警 |
| `LogFilter`（level/target/since） | `os-services/src/monitor.rs` | 日志过滤条件 |
| `ServiceError`/`ServiceResult` | `os-services/src/error.rs` | 共享错误枚举（本 agent 主要用 `Internal`/`Io`；variant 归属见 error.rs，**不得改动其他 variant**） |

**关键实现**：
- `OtelMonitor`：`impl Monitor`，基于 opentelemetry + prometheus + tracing；`record_metric` 写入 metric 时序库（prometheus 风格）；`query_metrics` 按时间范围查询；`add_alert_rule` 注册规则到告警引擎；`list_alerts` 返回已触发未恢复 + 近期已恢复；`tail_logs` 按 LogFilter 过滤日志（级别/目标/时间）。
- `MockMonitor`：feature `mock`，内存态维护 metric/log/alert，返回确定性值，供下游测试。

## 4. 输入契约

> 本 agent 依赖的上游 trait，**必须经 `Box<dyn Trait>` 或泛型注入**，不得耦合具体实现。

| 上游 trait | 来源 crate | 来源 agent | mock 位置 | 用途 |
|-----------|-----------|-----------|----------|------|
| `DateTime` | `os-core` | `core-agent` | —（重导出，无 mock） | metric/log/alert 时间戳 |
| `ApiError`/`ApiErrorCode` | `os-common` | core-agent 间接 | — | 错误码映射 |

**mock 策略**：本 agent 对 core 的依赖全部是类型/重导出，**无业务 trait 依赖**。core-agent `cargo check` 通过即可开工；告警规则条件解析与持续时长判断是纯函数，无依赖可独立测试。

## 5. 输出要求

### 5.1 实现规范
- **命名**：实现 struct 命名为 `OtelMonitor`，不挂 agent 前缀。
- **错误**：实现方法返回 `Result<T, ServiceError>`；主要映射 `Internal(String)`/`Io`；不新增/改动其他归各 service-agent 的 variant。
- **测试**：每个公开方法有单元测试；告警规则条件解析（`">0.9"` 等表达式）与持续时长判断（for_duration_secs 抖动抑制）有专门测；LogFilter 过滤逻辑有专门测；`MockMonitor` 覆盖返回路径。
- **文档**：每个 pub 项有 `///` 中文文档；告警引擎与 metric 采集补 `//` 内联注释说明"为什么"。

### 5.2 DoD（Definition of Done，验收清单）
- [ ] `Monitor` 有具体实现（非 `todo!()`）
- [ ] `cargo check -p os-services` 通过（与其他 service-agent 同 crate，须分支不冲突）
- [ ] `cargo test -p os-services` 通过
- [ ] `cargo clippy -p os-services -- -D warnings` 无警告
- [ ] 为下游提供 `MockMonitor`（`crates/os-services/src/mock.rs`，feature gate `mock`）
- [ ] 更新 `PROGRESS.md`

## 6. 依赖前置

| 依赖 | 类型 | 说明 |
|------|------|------|
| `core-agent` 交付 os-core 类型可用 | **软依赖** | core 已是契约层，`cargo check` 通过即可；本 agent 不依赖 core 业务 trait |
| 其他 service-agent 编辑同 crate 分支协调 | **协调依赖** | 共享 `os-services` crate，六 agent 分支可能冲突 mock.rs/lib.rs/error.rs；约定：lib.rs/mock.rs/error.rs 改动走 PR 互评 + 子分支命名带前缀 |

**可立即启动的部分**：`Metric`/`LogEntry`/`AlertRule` 等数据结构已存在；告警规则条件解析（纯函数）；LogFilter 过滤逻辑（纯函数）；`MockMonitor` 内存态实现。

## 7. 并行性分析

- **可并行实现的 trait**：仅一个 trait；方法内部分三组可并行：metric 采集查询（record/query）、告警引擎（add_rule/list_alerts）、日志（tail_logs）。
- **有内部顺序的 trait**：`list_alerts` 依赖规则已 `add_alert_rule` 且 metric 已 `record_metric`（告警引擎消费 metric 流）。
- **瓶颈点**：告警引擎的条件表达式解析与持续时长判断是早期阻塞点；metric 时序存储的查询性能（大数据量）。

## 8. 验收标准

| 维度 | 标准 |
|------|------|
| 编译 | `cargo check -p os-services` 通过 |
| 测试 | `cargo test -p os-services` 通过；关键路径（metric 采集/查询、告警条件解析与抖动抑制、日志过滤、mock 返回）覆盖率 ≥ 75% |
| 契约 | 未修改 trait 签名（除非有 ADR）；未改动 `ServiceError` 归其他 service-agent 的 variant；`cargo doc` 无警告 |
| mock | `MockMonitor` 已提交（下游可用） |
| 文档 | pub 项有中文 `///`；PROGRESS.md 已更新 |

## 9. 风险红线

🔴 **严禁**：
- 修改其他 service-agent 拥有的 trait（`BackupManager`/`MediaManager`/`FileManager`/`DevTools`/`PowerManager`；改动须经 ADR + 会签）
- 修改 `ServiceError` 中归其他 service-agent 的 variant（本 agent 仅用 `Internal`/`Io`，不新增 variant）
- 修改 trait 签名（破坏性变更须经 ADR + 受影响 agent 会签）
- 虚构未发布的依赖（opentelemetry/prometheus/tracing 须在 workspace 已注册）
- 跳过测试直接提 PR

🟡 **谨慎**：
- **同 crate 分支冲突**：与其他五个 service-agent 共享 `os-services`，lib.rs/mock.rs/error.rs 改动须互评；建议各自独立 impl 文件（`impl_monitor.rs`）减少冲突
- 改 metric 存储后端（prometheus ↔ 其他时序库，架构性变更，须 ADR）
- 改告警条件表达式语法（影响规则兼容性，须 ADR）
- 引入新第三方 crate（如时序库）须经 ReviewAgent 评估维护性/安全

## 10. 示例工作流

> 以"实现 `Monitor.add_alert_rule`（告警规则注册 + 条件解析）"为例：

1. **开工**：读 `PROGRESS.md`（恢复上下文）+ `TASKS.md`（取任务）+ 本规格书 §3/§4
2. **读契约**：读 `crates/os-services/src/monitor.rs`（`Monitor` trait + `AlertRule`/`AlertSeverity`）+ `crates/os-services/src/error.rs`（`ServiceError::Internal`）+ 相关 ADR
3. **切分支**：`git checkout agent/monitor-agent`；建子分支 `agent/monitor-agent/add-alert-rule`
4. **实现**：新建 `impl_monitor.rs`，定义 `OtelMonitor`，`impl Monitor for OtelMonitor`；`add_alert_rule` 解析 `rule.condition`（如 `">0.9"` → 算子 + 阈值），校验 metric 名存在性，注册到告警引擎（含 for_duration_secs 抖动抑制状态机）；持久化规则。
5. **测试**：单元测（条件表达式合法/非法解析、抖动抑制：短时波动不触发、持续超阈值触发）；`cargo test -p os-services`
6. **提 PR**：推到远程，PR 标题 `[monitor-agent] add-alert-rule`，描述含 DoD 勾选状态 + 同 crate 协调备注（CC 其他 service-agent）
7. **响应评审**：按 ReviewAgent 意见修订；契约变更触发 ADR + 会签
8. **更新进度**：合并后更新 `PROGRESS.md` + `TASKS.md`

## 11. 启动 prompt 模板

> 复制以下 prompt 启动本 agent 会话（替换 `<...>`）：

```
你是 OS 系统的 Monitor Agent（agent_id: monitor-agent）。
你的规格书在 OS_System/docs/agents/monitor-agent.md——请先完整读取它。
协作约定在 OS_System/docs/agents/_conventions.md——也请读取。
你拥有的契约在 OS_System/crates/os-services/src/monitor.rs（仅 Monitor trait 归你；backup.rs/media.rs/files.rs/devtools.rs/power.rs 归其他 service-agent，不得改动）。

本次任务：<具体任务描述，或"按 TASKS.md 取下一个任务；优先交付 MockMonitor 解锁下游">

开工前必读：
1. OS_System/docs/agents/monitor-agent.md（你的规格）
2. OS_System/docs/agents/_conventions.md（协作约定）
3. OS_System/docs/agents/monitor-agent/PROGRESS.md（你的进度，恢复上下文）
4. OS_System/docs/agents/monitor-agent/TASKS.md（你的任务队列）
5. 你拥有的 trait：crates/os-services/src/monitor.rs、error.rs（仅用 Internal/Io variant，不新增）
6. 相关 ADR（OS_System/docs/adr/）

完成后：更新 PROGRESS.md + TASKS.md，按 DoD 自检，提 PR。
不得修改其他 agent 的 crate；不得破坏 trait 签名（走 ADR）；不得改动 ServiceError 归其他 service-agent 的 variant。
特殊注意：与其他五个 service-agent 共享 os-services crate，分支改动须互评；告警规则条件解析与抖动抑制（for_duration_secs）是核心。
```

## 12. 恢复协议

> 会话重启后（上下文丢失），按以下顺序恢复：

1. 读 `OS_System/docs/agents/monitor-agent.md`（本规格书，重识身份）
2. 读 `OS_System/docs/agents/_conventions.md`（重识协作规则）
3. 读 `OS_System/docs/agents/monitor-agent/PROGRESS.md`（**最关键**——你之前做到哪了）
4. 读 `OS_System/docs/agents/monitor-agent/TASKS.md`（下一个任务）
5. `git log agent/monitor-agent --oneline -20`（看最近提交，了解工作状态）
6. `cargo check -p os-services`（确认当前代码状态）
7. 继续未完成任务；若阻塞，在 PROGRESS.md 记录并在 TASKS.md 标 `blocked`

> **若 PROGRESS.md 不存在**（首次启动或丢失）：从规格书 §3 取契约清单（`Monitor` 一 trait），从 `git log` 推断进度，重建 PROGRESS.md。优先确认 `MockMonitor` 是否已交付（下游 api-agent 依赖，未交付则阻塞下游并行）。
