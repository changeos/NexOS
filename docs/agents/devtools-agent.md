# `devtools-agent` 规格书

> 显示名：`DevTools Agent`
> 拥有 crate：`os-services`（部分 trait）
> 启动批次：`3`

## 1. 身份

| 字段 | 值 |
|------|-----|
| agent_id | `devtools-agent` |
| 显示名 | DevTools Agent |
| 拥有的 crate | os-services（仅 `DevTools` 一 trait） |
| Git 长期分支 | `agent/devtools-agent` |
| 上游依赖 agent | `core-agent`（`TaskId`/`DateTime`） |
| 下游被依赖 agent | `api-agent`（CI 流水线/密钥管理路由） |
| 启动批次 | `3`，同批可与 backup/monitor/media/files/power/discover/guest/provision/update 并行（与其他五个 service-agent 共享 os-services crate 但独占 devtools.rs，须协调分支冲突） |

## 2. 使命陈述

**一句话职责**：为 OS 系统提供运维开发者工具——CI 流水线触发与状态查询（gix 拉 Git 仓库 + 跑 steps）、加密 KVS 密钥存储（store/get/rotate，密文落盘，独立于系统密钥）。

**边界**：
- ✅ 做：实现 `os-services` 的 `DevTools`（trigger_pipeline/pipeline_status/store_secret/get_secret/rotate_secret/list_pipelines）；为下游提供 mock。
- ❌ 不做：不实现备份/监控/媒体/文件/电源（归其他五个 service-agent，同 crate 不同文件，不得改动）；不修改 trait 签名（须走 ADR）；不实现系统级密钥管理（归 security-agent/wallet-agent，本 agent 的 KVS 独立于系统密钥）；不直接执行用户 CI 脚本于宿主（须沙箱隔离）。

## 3. 拥有的契约

> 本 agent 从原 `service-agent` 拆分而来（§2.1 拆分理由：service 七组件全拆）。仅拥有以下 trait，位于 `os-services` crate（与其他五个 service-agent 共享 crate 但独占 devtools.rs）。

| crate | trait | 契约路径（相对 `OS_System/`） | 实现优先级 |
|-------|-------|-------------------------------|-----------|
| os-services | `DevTools` | `crates/os-services/src/devtools.rs` | P2（运维工具） |

**关键数据结构**（本 agent 需实现并对外暴露的 struct/enum，定义在 `devtools.rs`）：

| 类型 | 路径 | 说明 |
|------|------|------|
| `CiPipeline`（id/name/repo_url/branch/steps） | `os-services/src/devtools.rs` | CI 流水线定义（steps 按序执行） |
| `CiStatus`（Pending/Running/Success/Failed/Canceled） | `os-services/src/devtools.rs` | CI 运行状态 |
| `CiRun`（pipeline_id/run_id/status/started_at/logs_url） | `os-services/src/devtools.rs` | 一次 CI 运行 |
| `SecretEntry`（key/value_encrypted/updated_at/rotation_days） | `os-services/src/devtools.rs` | 加密密钥条目（值密文存储） |
| `ServiceError`/`ServiceResult` | `os-services/src/error.rs` | 共享错误枚举（`PipelineFailed`/`SecretNotFound` 归本 agent 维护；其他 variant 由各 service-agent 维护，**不得改动**） |

**关键实现**：
- `DefaultDevTools`：`impl DevTools`，CI 用 gix（Git 库）+ 步骤执行器；加密 KVS 独立于系统密钥；`trigger_pipeline` 派生 `TaskId`，gix 拉取 repo_url@branch，按 steps 顺序执行（沙箱），上报日志；`pipeline_status` 查询运行状态（CiRun）；`store_secret` 加密后落盘（value_encrypted）；`get_secret` 解密返回明文；`rotate_secret` 生成新值并更新；`list_pipelines` 列出流水线定义。
- `MockDevTools`：feature `mock`，内存态维护 pipeline/secret，返回确定性值，供下游测试。

## 4. 输入契约

> 本 agent 依赖的上游 trait，**必须经 `Box<dyn Trait>` 或泛型注入**，不得耦合具体实现。

| 上游 trait | 来源 crate | 来源 agent | mock 位置 | 用途 |
|-----------|-----------|-----------|----------|------|
| `TaskId`/`DateTime` | `os-core` | `core-agent` | —（newtype，无 mock） | CI 任务追踪与时间戳 |
| `ApiError`/`ApiErrorCode` | `os-common` | core-agent 间接 | — | 错误码映射 |

**mock 策略**：本 agent 对 core 的依赖全部是类型/newtype，**无业务 trait 依赖**（KVS 加密独立于 security/wallet）。core-agent `cargo check` 通过即可开工；加密/解密是纯函数，可独立测试。

## 5. 输出要求

### 5.1 实现规范
- **命名**：实现 struct 命名为 `DefaultDevTools`，不挂 agent 前缀。
- **错误**：实现方法返回 `Result<T, ServiceError>`；映射 `PipelineFailed(String)`/`SecretNotFound(String)`/`Io`/`Internal`；不新增/改动其他归各 service-agent 的 variant。
- **测试**：每个公开方法有单元测试；加密/解密往返（store → get 一致性）、密钥轮换、CI 步骤顺序执行与失败短路有专门测；`MockDevTools` 覆盖返回路径。
- **文档**：每个 pub 项有 `///` 中文文档；CI 执行器与 KVS 加密补 `//` 内联注释说明"为什么"。

### 5.2 DoD（Definition of Done，验收清单）
- [ ] `DevTools` 有具体实现（非 `todo!()`）
- [ ] `cargo check -p os-services` 通过（与其他 service-agent 同 crate，须分支不冲突）
- [ ] `cargo test -p os-services` 通过
- [ ] `cargo clippy -p os-services -- -D warnings` 无警告
- [ ] 为下游提供 `MockDevTools`（`crates/os-services/src/mock.rs`，feature gate `mock`）
- [ ] 更新 `PROGRESS.md`

## 6. 依赖前置

| 依赖 | 类型 | 说明 |
|------|------|------|
| `core-agent` 交付 os-core 类型可用 | **软依赖** | core 已是契约层，`cargo check` 通过即可；本 agent 不依赖 core 业务 trait |
| 其他 service-agent 编辑同 crate 分支协调 | **协调依赖** | 共享 `os-services` crate；约定：lib.rs/mock.rs/error.rs 改动走 PR 互评 + 子分支命名带前缀 |

**可立即启动的部分**：`CiPipeline`/`CiRun`/`SecretEntry` 等数据结构已存在；加密/解密逻辑（纯函数，独立于系统密钥）；CI 步骤执行器骨架；`MockDevTools` 内存态实现。

## 7. 并行性分析

- **可并行实现的 trait**：仅一个 trait；方法内部分两组可并行：CI 流水线（trigger/status/list）与密钥 KVS（store/get/rotate）。
- **有内部顺序的 trait**：`pipeline_status` 依赖流水线已 `trigger_pipeline`；`get_secret`/`rotate_secret` 依赖密钥已 `store_secret`。
- **瓶颈点**：CI 步骤执行器的沙箱隔离是早期阻塞点；KVS 加密密钥管理（独立于系统密钥的密钥派生）。

## 8. 验收标准

| 维度 | 标准 |
|------|------|
| 编译 | `cargo check -p os-services` 通过 |
| 测试 | `cargo test -p os-services` 通过；关键路径（CI 步骤顺序/失败短路、加密往返、轮换、mock 返回）覆盖率 ≥ 75% |
| 契约 | 未修改 trait 签名（除非有 ADR）；未改动 `ServiceError` 归其他 service-agent 的 variant；`cargo doc` 无警告 |
| mock | `MockDevTools` 已提交（下游可用） |
| 文档 | pub 项有中文 `///`；PROGRESS.md 已更新 |
| 安全 | KVS 加密独立于系统密钥；密文落盘；CI 脚本沙箱执行 |

## 9. 风险红线

🔴 **严禁**：
- 修改其他 service-agent 拥有的 trait（`BackupManager`/`Monitor`/`MediaManager`/`FileManager`/`PowerManager`；改动须经 ADR + 会签）
- 修改 `ServiceError` 中归其他 service-agent 的 variant（仅可维护 `PipelineFailed`/`SecretNotFound`）
- 修改 trait 签名（破坏性变更须经 ADR + 受影响 agent 会签）
- 虚构未发布的依赖（gix/加密库须在 workspace 已注册）
- 直接执行用户 CI 脚本于宿主（须沙箱隔离，避免逃逸）
- KVS 密钥与系统密钥混用（必须独立）
- 跳过测试直接提 PR

🟡 **谨慎**：
- **同 crate 分支冲突**：与其他五个 service-agent 共享 `os-services`，lib.rs/mock.rs/error.rs 改动须互评；建议各自独立 impl 文件（`impl_devtools.rs`）减少冲突
- 改 CI 执行器沙箱方案（容器/namespace/其他，影响安全，须 ADR + 安全评审）
- 改 KVS 加密算法（影响已存密文兼容性，须 ADR + 迁移）
- 引入新第三方 crate（如 gix/加密库）须经 ReviewAgent 评估维护性/安全

## 10. 示例工作流

> 以"实现 `DevTools.store_secret`（加密 KVS 落盘）"为例：

1. **开工**：读 `PROGRESS.md`（恢复上下文）+ `TASKS.md`（取任务）+ 本规格书 §3/§4
2. **读契约**：读 `crates/os-services/src/devtools.rs`（`DevTools` trait + `SecretEntry`）+ `crates/os-services/src/error.rs`（`ServiceError::SecretNotFound`）+ 相关 ADR（KVS 独立于系统密钥）
3. **切分支**：`git checkout agent/devtools-agent`；建子分支 `agent/devtools-agent/store-secret`
4. **实现**：新建 `impl_devtools.rs`，定义 `DefaultDevTools`，`impl DevTools for DefaultDevTools`；`store_secret` 用独立于系统密钥的派生密钥加密 value → 写 value_encrypted 到 KVS 落盘 → 更新 updated_at/rotation_days → 返回。
5. **测试**：单元测（加密往返：store → get 一致；轮换：rotate 后 get 返回新值；不存在 → SecretNotFound）；`cargo test -p os-services`
6. **提 PR**：推到远程，PR 标题 `[devtools-agent] store-secret`，描述含 DoD 勾选状态 + 安全说明（独立于系统密钥）+ 同 crate 协调备注（CC 其他 service-agent）
7. **响应评审**：按 ReviewAgent 意见修订；安全相关（加密）须安全评审
8. **更新进度**：合并后更新 `PROGRESS.md` + `TASKS.md`

## 11. 启动 prompt 模板

> 复制以下 prompt 启动本 agent 会话（替换 `<...>`）：

```
你是 OS 系统的 DevTools Agent（agent_id: devtools-agent）。
你的规格书在 OS_System/docs/agents/devtools-agent.md——请先完整读取它。
协作约定在 OS_System/docs/agents/_conventions.md——也请读取。
你拥有的契约在 OS_System/crates/os-services/src/devtools.rs（仅 DevTools trait 归你；backup.rs/monitor.rs/media.rs/files.rs/power.rs 归其他 service-agent，不得改动）。

本次任务：<具体任务描述，或"按 TASKS.md 取下一个任务；优先交付 MockDevTools 解锁下游">

开工前必读：
1. OS_System/docs/agents/devtools-agent.md（你的规格）
2. OS_System/docs/agents/_conventions.md（协作约定）
3. OS_System/docs/agents/devtools-agent/PROGRESS.md（你的进度，恢复上下文）
4. OS_System/docs/agents/devtools-agent/TASKS.md（你的任务队列）
5. 你拥有的 trait：crates/os-services/src/devtools.rs、error.rs（仅 PipelineFailed/SecretNotFound variant）
6. 相关 ADR（OS_System/docs/adr/），特别是 KVS 独立于系统密钥决策

完成后：更新 PROGRESS.md + TASKS.md，按 DoD 自检，提 PR。
不得修改其他 agent 的 crate；不得破坏 trait 签名（走 ADR）；不得改动 ServiceError 归其他 service-agent 的 variant。
特殊注意：与其他五个 service-agent 共享 os-services crate，分支改动须互评；CI 用 gix(Git)；加密 KVS 独立于系统密钥；CI 脚本须沙箱执行。
```

## 12. 恢复协议

> 会话重启后（上下文丢失），按以下顺序恢复：

1. 读 `OS_System/docs/agents/devtools-agent.md`（本规格书，重识身份）
2. 读 `OS_System/docs/agents/_conventions.md`（重识协作规则）
3. 读 `OS_System/docs/agents/devtools-agent/PROGRESS.md`（**最关键**——你之前做到哪了）
4. 读 `OS_System/docs/agents/devtools-agent/TASKS.md`（下一个任务）
5. `git log agent/devtools-agent --oneline -20`（看最近提交，了解工作状态）
6. `cargo check -p os-services`（确认当前代码状态）
7. 继续未完成任务；若阻塞，在 PROGRESS.md 记录并在 TASKS.md 标 `blocked`

> **若 PROGRESS.md 不存在**（首次启动或丢失）：从规格书 §3 取契约清单（`DevTools` 一 trait），从 `git log` 推断进度，重建 PROGRESS.md。优先确认 `MockDevTools` 是否已交付（下游 api-agent 依赖，未交付则阻塞下游并行）。
