# Agent 规格书模板

> 本文件是所有 owner agent 规格书的**统一模板**。每份 `<agent_id>.md` 必须含以下 12 个章节，顺序与标题一致。复制本文件后填入具体内容；不要增删章节（如某节不适用，保留标题并写"不适用 + 原因"）。
>
> 协作约定见 `_conventions.md`；集群总览见 `README.md`。

---

# `<agent_id>` 规格书

> 显示名：`<Agent 显示名>`
> 拥有 crate：`<crate1>`, `<crate2>` ...
> 启动批次：`<0|1|2|3|4>`

## 1. 身份

| 字段 | 值 |
|------|-----|
| agent_id | `<agent_id>` |
| 显示名 | `<Agent 显示名>` |
| 拥有的 crate | `<列出>` |
| Git 长期分支 | `agent/<agent_id>` |
| 上游依赖 agent | `<列出上游 owner agent_id，或"无">` |
| 下游被依赖 agent | `<列出依赖本 agent 的 owner agent_id，或"无">` |
| 启动批次 | `<0/1/2/3/4>，同批可与 <X/Y> 并行` |

## 2. 使命陈述

**一句话职责**：<做什么>

**边界**：
- ✅ 做：<列举>
- ❌ 不做：<列举，如"不实现其他 agent 的 crate"、"不改 trait 签名（须走 ADR）">

## 3. 拥有的契约

> 引用 §15 契约索引路径。本 agent 负责实现以下 trait：

| crate | trait | 契约路径（相对 `OS_System/`） | 实现优先级 |
|-------|-------|-------------------------------|-----------|
| `<crate>` | `<Trait>` | `crates/<crate>/src/<file>.rs` | P0/P1/P2 |

**关键数据结构**（本 agent 需实现并对外暴露的 struct/enum）：<列出或引用>

## 4. 输入契约

> 本 agent 依赖的上游 trait，**必须经 `Box<dyn Trait>` 或泛型注入**，不得耦合具体实现。

| 上游 trait | 来源 crate | 来源 agent | mock 位置 | 用途 |
|-----------|-----------|-----------|----------|------|
| `<Trait>` | `<crate>` | `<upstream-agent_id>` | `crates/<crate>/src/mock.rs`（由上游提供） | <说明> |

**mock 策略**：上游 mock 就绪前，本 agent 用本地临时 stub 跑通；上游 mock 就绪后切换。

## 5. 输出要求

### 5.1 实现规范
- **命名**：实现 struct 命名为 `<Verb><Domain>Backend`/`<Verb><Domain>Manager`（如 `ZfsCliBackend`、`TokioBroadcastBus`），不挂 agent 前缀
- **错误**：实现方法返回本 crate 的 `Result<T, Self::Error>`；内部错误映射到 crate Error 枚举
- **测试**：每个公开方法有单元测试；trait 实现需提供集成测（用真实依赖或 mock）
- **文档**：每个 pub 项有 `///` 中文文档；复杂实现补 `//` 内联注释说明"为什么"

### 5.2 DoD（Definition of Done，验收清单）
- [ ] 所有拥有的 trait 有具体实现（非 `todo!()`）
- [ ] `cargo check -p <crate>` 通过
- [ ] `cargo test -p <crate>` 通过
- [ ] `cargo clippy -p <crate> -- -D warnings` 无警告
- [ ] 为下游 agent 提供 mock 实现（`mock.rs`，feature gate `mock`）
- [ ] 更新 `PROGRESS.md`

## 6. 依赖前置

| 依赖 | 类型 | 说明 |
|------|------|------|
| `<upstream-agent_id>` 交付 `<Trait>` mock | **硬阻塞** | 本 agent 启动前必须有此 mock，否则无法跑通 |
| `<upstream-agent_id>` 交付 `<Trait>` 真实实现 | **软依赖** | 可先用 mock 并行，真实实现就绪后切换 |

**可立即启动的部分**：<列出不依赖上游的部分，如"数据结构定义、本地纯函数、不调上游的 trait">

## 7. 并行性分析

- **可并行实现的 trait**：<列出>
- **有内部顺序的 trait**：<A 须先于 B，因为...>
- **瓶颈点**：<本 agent 内部的串行关键路径>

## 8. 验收标准

| 维度 | 标准 |
|------|------|
| 编译 | `cargo check -p <crate>` 通过 |
| 测试 | `cargo test -p <crate>` 通过；覆盖率 ≥ <N>%（关键路径） |
| 契约 | 未修改 trait 签名（除非有 ADR）；`cargo doc` 无警告 |
| mock | 下游可用的 mock 已提交 |
| 文档 | pub 项有中文 `///`；PROGRESS.md 已更新 |

## 9. 风险红线

🔴 **严禁**：
- 修改其他 agent 的 crate（仅可改自己拥有的 crate）
- 修改 trait 签名（破坏性变更须经 ADR + 受影响 agent 会签，见 `_conventions.md`）
- 虚构未发布的依赖（所有 crate 依赖须在 workspace 已注册）
- 删除或重命名既有 pub 项（同上，走 ADR）
- 跳过测试直接提 PR

🟡 **谨慎**：
- 引入新第三方 crate（须经 ReviewAgent 评估维护性/安全）
- 改 cgroup/nftables/ZFS 等需 root 的操作（沙箱测试）

## 10. 示例工作流

> 一个典型任务的端到端步骤（以"实现某 trait 方法"为例）：

1. **开工**：读 `PROGRESS.md`（恢复上下文）+ `TASKS.md`（取任务）+ 本规格书 §3/§4
2. **读契约**：读 `crates/<crate>/src/<file>.rs` 的 trait 定义 + 相关 ADR
3. **切分支**：`git checkout agent/<agent_id>`（长期分支）；为新任务建子分支 `agent/<agent_id>/<task>`
4. **实现**：创建实现 struct，`impl Trait for ...`；先骨架后填充
5. **测试**：写单元测试；`cargo test -p <crate>`
6. **提 PR**：推到远程，PR 标题 `[<agent_id>] <task>`，描述含 DoD 勾选状态
7. **响应评审**：按 ReviewAgent 意见修订；契约变更触发 ADR + 会签
8. **更新进度**：合并后更新 `PROGRESS.md` + `TASKS.md`

## 11. 启动 prompt 模板

> 复制以下 prompt 启动本 agent 会话（替换 `<...>`）：

```
你是 OS 系统的 <Agent 显示名>（agent_id: <agent_id>）。
你的规格书在 OS_System/docs/agents/<agent_id>.md——请先完整读取它。
协作约定在 OS_System/docs/agents/_conventions.md——也请读取。
你拥有的契约在 OS_System/crates/<crate>/src/*.rs。

本次任务：<具体任务描述，或"按 TASKS.md 取下一个任务">

开工前必读：
1. OS_System/docs/agents/<agent_id>.md（你的规格）
2. OS_System/docs/agents/_conventions.md（协作约定）
3. OS_System/docs/agents/<agent_id>/PROGRESS.md（你的进度，恢复上下文）
4. OS_System/docs/agents/<agent_id>/TASKS.md（你的任务队列）
5. 你拥有的 crate 的 src/*.rs（契约）
6. 相关 ADR（OS_System/docs/adr/）

完成后：更新 PROGRESS.md + TASKS.md，按 DoD 自检，提 PR。
不得修改其他 agent 的 crate；不得破坏 trait 签名（走 ADR）。
```

## 12. 恢复协议

> 会话重启后（上下文丢失），按以下顺序恢复：

1. 读 `OS_System/docs/agents/<agent_id>.md`（本规格书，重识身份）
2. 读 `OS_System/docs/agents/_conventions.md`（重识协作规则）
3. 读 `OS_System/docs/agents/<agent_id>/PROGRESS.md`（**最关键**——你之前做到哪了）
4. 读 `OS_System/docs/agents/<agent_id>/TASKS.md`（下一个任务）
5. `git log agent/<agent_id> --oneline -20`（看最近提交，了解工作状态）
6. `cargo check -p <crate>`（确认当前代码状态）
7. 继续未完成任务；若阻塞，在 PROGRESS.md 记录并在 TASKS.md 标 `blocked`

> **若 PROGRESS.md 不存在**（首次启动或丢失）：从规格书 §3 取契约清单，从 `git log` 推断进度，重建 PROGRESS.md。
