# `orchestrator-agent` 规格书

> 显示名：`Orchestrator Agent`
> 拥有 crate：`osd`
> 启动批次：`0`

## 1. 身份

| 字段 | 值 |
|------|-----|
| agent_id | `orchestrator-agent` |
| 显示名 | Orchestrator Agent |
| 拥有的 crate | osd |
| Git 长期分支 | `agent/orchestrator-agent` |
| 上游依赖 agent | core-agent（用 os-core 的 `Health`/`HealthReport`/`Capacity`/`ResourceQuota`/`NodeInfo`/`EventBus`） |
| 下游被依赖 agent | 全体 owner agent（osd 管所有业务组件进程的生命周期与健康探测） |
| 启动批次 | 0，同批可与 core-agent / i18n-agent 并行 |

## 2. 使命陈述

**一句话职责**：实现 OS 系统的 PID1 后编排守护进程——业务组件进程的启停/重启/状态/配额、cgroup v2 资源隔离、内嵌 NTP 时间同步（HA 集群一致性前置依赖）。

**边界**：
- ✅ 做：实现 `Orchestrator`（async：`start`/`stop`/`restart`/`status`/`list_components`/`set_quota`/`get_quota`）；实现 `HealthProbe`（async：`probe`→`HealthReport`）；实现 `NtpManager`（async：`sync_now`/`status`/`set_servers`）；systemd unit 生成 + tokio 监管；cgroup v2 资源配额（cgroups-rs）；chrony/内嵌 NTP 编排；为各业务组件提供 `MockHealthProbe`。
- ❌ 不做：不实现其他 agent 的业务 crate（storage/network/meta...）；不修改 trait 签名（须走 ADR）；不直接调 ZFS/nftables 等业务子系统（那是各 owner agent 的职责，osd 只管进程生命周期）；不兼任主代理（OrchestratorAgent 调度会话）写 crate 代码（见 `_conventions.md` §7 红线——本 agent 是实现 osd 的 owner，与主代理角色不同）。

## 3. 拥有的契约

| crate | trait | 契约路径（相对 `OS_System/`） | 实现优先级 |
|-------|-------|-------------------------------|-----------|
| osd | `Orchestrator` | `crates/osd/src/orchestrator.rs` | P0 |
| osd | `HealthProbe` | `crates/osd/src/health.rs` | P1 |
| osd | `NtpManager` | `crates/osd/src/ntp.rs` | P0（HA 共识前置依赖，须最早启动） |

**关键数据结构**（本 agent 需实现并对外暴露的 struct/enum）：

| 类型 | 路径 | 说明 |
|------|------|------|
| `ComponentId` / `ComponentStatus` / `ComponentDescriptor` / `HealthProbeConfig` | `osd/src/component.rs` | 编排对象模型（组件声明式注册表） |
| `NtpStatus` | `osd/src/ntp.rs` | NTP 同步状态快照 |
| `OrchestratorError` / `OrchestratorResult` | `osd/src/error.rs` | 编排错误（`ComponentNotFound`/`StartFailed`/`StopFailed`/`DependencyCycle`/`NtpSyncFailed`/`QuotaFailed`/`Io`） |

**关键实现**：
- `SystemdOrchestrator`：基于 systemd unit 生成 + `tokio::process` 监管；按 `ComponentDescriptor.dependencies` 拓扑排序拉起；同组件操作串行化（内部 `HashMap<ComponentId, Mutex<()>>`）；退避重启（指数退避，连续失败超阈值标 `Failed`）。
- `CgroupQuota`：用 `cgroups-rs` 写 cgroup v2（CPU/内存/IO），`set_quota` 在线调整无需重启。
- `ChronyNtp` / 内嵌 NTP：基于 chrony 编排或自实现 NTP 客户端（§9.1#8 决策：NTP 由 osd 统管，不依赖外部 ntpd 避免双源冲突）；`sync_now` 触发一次同步，`status` 返回 `NtpStatus`（偏移/最近同步/上游列表）。
- `MockHealthProbe`：feature `mock` 下提供，供各业务组件的 osd 侧集成测注入（构造器 `MockHealthProbe::new().with_report(report)`）。

## 4. 输入契约

| 上游 trait | 来源 crate | 来源 agent | mock 位置 | 用途 |
|-----------|-----------|-----------|----------|------|
| `Health` / `HealthReport` | os-core | core-agent | `crates/os-core/src/mock.rs`（core 提供） | `HealthProbe::probe` 返回类型 |
| `ResourceQuota` / `Capacity` / `NodeInfo` | os-core（types.rs，非 trait） | core-agent | —（纯数据结构，无需 mock） | cgroup 配额模型 |
| `EventBus` | os-core | core-agent | `crates/os-core/src/mock.rs`（core 提供 `MockEventBus`） | 组件启停事件上报（`Topic::System`） |

**mock 策略**：core-agent 的 `MockEventBus` 就绪前，本 agent 用本地临时 stub `EventBus` 跑通；就绪后切换。`Health`/`ResourceQuota` 是数据结构，core `cargo check` 通过即可用。

## 5. 输出要求

### 5.1 实现规范
- **命名**：实现 struct 命名为 `SystemdOrchestrator`（`Orchestrator`）、`CgroupQuota`（配额辅助）、`ChronyNtp`（`NtpManager`）；不挂 agent 前缀。
- **错误**：trait 方法返回 `OrchestratorResult<T>`；子进程/cgroup/ntp 失败映射到对应 `OrchestratorError` 变体。
- **测试**：`Orchestrator`（start/stop/restart/status/list/set_quota）每个方法有测试（用 mock 进程或 `echo`/`sleep` 替身）；拓扑排序与循环检测（`DependencyCycle`）有专门测；`NtpManager.sync_now` 在沙箱用本地 ntp server 测；`HealthProbe` 由各业务组件 impl，本 agent 提供 mock 供 osd 侧测。
- **文档**：每个 pub 项有 `///` 中文文档；`SystemdOrchestrator` 内部用 `//` 注释说明拓扑排序、退避重启策略、cgroup v2 路径。

### 5.2 DoD（Definition of Done，验收清单）
- [ ] `Orchestrator`/`HealthProbe`/`NtpManager` 有具体实现（非 `todo!()`）
- [ ] `cargo check -p osd` 通过
- [ ] `cargo test -p osd` 通过
- [ ] `cargo clippy -p osd -- -D warnings` 无警告
- [ ] 为各业务组件提供 `MockHealthProbe`（`crates/osd/src/mock.rs`，feature gate `mock`）
- [ ] 更新 `PROGRESS.md`

## 6. 依赖前置

| 依赖 | 类型 | 说明 |
|------|------|------|
| core-agent 交付 `Health`/`ResourceQuota`/`NodeInfo` 类型可用 | **软依赖** | core 已是契约层，`cargo check` 通过即可 |
| core-agent 交付 `MockEventBus` | **软依赖** | 组件启停事件上报用；就绪前用本地 stub |
| root / `CAP_SYS_TIME` / `CAP_SYS_ADMIN` 权限 | **运行时硬阻塞** | cgroup v2 写入与 NTP 同步需 root；测试在沙箱（loop/容器）进行 |

**可立即启动的部分**：
- `SystemdOrchestrator` 骨架（拓扑排序、ComponentRegistry）——不调上游
- `MockHealthProbe`——可先交付解锁各业务组件 osd 侧测
- `NtpStatus`/`ComponentDescriptor` 模型补全

## 7. 并行性分析

- **可并行实现的 trait**：`Orchestrator` / `HealthProbe` / `NtpManager` 三者相互独立，可分配给不同子任务并行。
- **有内部顺序的 trait**：`NtpManager` 须最早可用（HA 集群共识/证书时序依赖时钟一致，§9.1#8），优先级 P0；`Orchestrator` 次之（拉起业务组件）；`HealthProbe` 最后（业务组件起来才有探测对象）。
- **瓶颈点**：cgroup v2 + systemd 操作需 root，CI/沙箱测试是关键路径；建议沙箱用容器（已挂 cgroup v2）跑集成测。

## 8. 验收标准

| 维度 | 标准 |
|------|------|
| 编译 | `cargo check -p osd` 通过 |
| 测试 | `cargo test -p osd` 通过；覆盖率 ≥ 80%（拓扑排序、退避重启、配额写入、NTP 状态机） |
| 契约 | 未修改 trait 签名（除非有 ADR）；`cargo doc -p osd` 无警告 |
| mock | `MockHealthProbe` 已提交并 feature gate 正确 |
| 文档 | pub 项有中文 `///`；PROGRESS.md 已更新 |
| 权限 | 文档明确标注哪些操作需 root / `CAP_SYS_TIME`（cgroup 写、NTP 同步） |

## 9. 风险红线

🔴 **严禁**：
- 修改其他 agent 的 crate（仅可改 osd）
- 修改 trait 签名（`Orchestrator`/`HealthProbe`/`NtpManager` 方法增删改须经 ADR + 受影响 agent 会签——全员依赖 osd 管生命周期）
- 虚构未发布的依赖（`cgroups-rs`/chrony 绑定须经 ReviewAgent 评估并注册 workspace）
- 在非沙箱环境直接测 cgroup/systemd/NTP（可能影响宿主进程或时钟）
- 跳过测试直接提 PR

🟡 **谨慎**：
- 改退避重启策略（影响业务可用性，须 ADR）
- 改 NTP 上游默认服务器（影响时钟源，须文档 + 通知）
- cgroup v2 → cgroup v1 兼容（架构性变更，须 ADR）
- 进程监管从 systemd 切到纯 tokio spawn（语义变化，须 ADR）

## 10. 示例工作流

> 典型任务：实现 `SystemdOrchestrator`。

1. **开工**：读 `docs/agents/orchestrator-agent/PROGRESS.md` + `TASKS.md` + 本规格书 §3/§4。
2. **读契约**：读 `crates/osd/src/orchestrator.rs`（`Orchestrator`）、`component.rs`（`ComponentDescriptor`/`ComponentStatus`）、`error.rs`；读 `crates/os-core/src/types.rs`（`ResourceQuota`/`Health`）。
3. **切分支**：`git checkout agent/orchestrator-agent`；建子分支 `agent/orchestrator-agent/systemd-orchestrator`。
4. **实现**：在 `crates/osd/src/` 新建 `impl_orchestrator.rs`，定义 `SystemdOrchestrator`，`impl Orchestrator for SystemdOrchestrator`；内部：`ComponentRegistry`（HashMap）、依赖拓扑排序（Kahn 算法，检测环抛 `DependencyCycle`）、`tokio::process::Command` 拉起、退避重启任务。
5. **cgroup**：用 `cgroups-rs` 写 v2（`cpu.max`/`memory.max`/`io.max`）；`set_quota` 在线写。
6. **测试**：单元测（拓扑排序、环检测、状态机）；集成测（沙箱容器内 start/stop 一个 `sleep 100` 替身进程）；`cargo test -p osd`。
7. **提 PR**：`[orchestrator-agent] systemd-orchestrator`，描述含 DoD 勾选 + 影响下游（全体）+ 所需权限（root/CAP_SYS_ADMIN）。
8. **响应评审**：按 ReviewAgent 意见修订。
9. **更新进度**：合并后更新 `PROGRESS.md` + `TASKS.md`。

## 11. 启动 prompt 模板

> 复制以下 prompt 启动本 agent 会话（替换 `<...>`）：

```
你是 OS 系统的 Orchestrator Agent（agent_id: orchestrator-agent）。
你的规格书在 OS_System/docs/agents/orchestrator-agent.md——请先完整读取它。
协作约定在 OS_System/docs/agents/_conventions.md——也请读取。
你拥有的契约在 OS_System/crates/osd/src/*.rs（orchestrator.rs / health.rs / ntp.rs / component.rs / error.rs）。

本次任务：<具体任务描述，或"按 TASKS.md 取下一个任务；优先交付 MockHealthProbe 与 NtpManager（HA 共识前置）">

开工前必读：
1. OS_System/docs/agents/orchestrator-agent.md（你的规格）
2. OS_System/docs/agents/_conventions.md（协作约定）
3. OS_System/docs/agents/orchestrator-agent/PROGRESS.md（你的进度，恢复上下文）
4. OS_System/docs/agents/orchestrator-agent/TASKS.md（你的任务队列）
5. 你拥有的 crate 的 src/*.rs（契约）
6. crates/os-core/src/types.rs（Health/ResourceQuota/NodeInfo 模型）
7. 相关 ADR（OS_System/docs/adr/）

特别注意：osd 是 PID1 后编排器，管所有业务组件进程；NTP 是 HA 集群共识前置依赖须最早启动；
cgroup v2/systemd/NTP 操作需 root / CAP_SYS_TIME / CAP_SYS_ADMIN，测试必须在沙箱进行。
注意区分：本 agent（实现 osd 的 owner）与 OrchestratorAgent（主代理调度会话，不写码）是不同角色。
不得修改其他 agent 的 crate；不得破坏 trait 签名（走 ADR）。
完成后：更新 PROGRESS.md + TASKS.md，按 DoD 自检，提 PR。
```

## 12. 恢复协议

> 会话重启后（上下文丢失），按以下顺序恢复：

1. 读 `OS_System/docs/agents/orchestrator-agent.md`（本规格书，重识身份）
2. 读 `OS_System/docs/agents/_conventions.md`（重识协作规则）
3. 读 `OS_System/docs/agents/orchestrator-agent/PROGRESS.md`（**最关键**——你之前做到哪了）
4. 读 `OS_System/docs/agents/orchestrator-agent/TASKS.md`（下一个任务）
5. `git log agent/orchestrator-agent --oneline -20`（看最近提交，了解工作状态）
6. `cargo check -p osd`（确认当前代码状态）
7. 继续未完成任务；若阻塞，在 PROGRESS.md 记录并在 TASKS.md 标 `blocked`

> **若 PROGRESS.md 不存在**（首次启动或丢失）：从规格书 §3 取契约清单（`Orchestrator`/`HealthProbe`/`NtpManager`），从 `git log` 推断进度，重建 PROGRESS.md。优先确认 `MockHealthProbe` 与 `NtpManager` 是否已交付（HA 共识前置 + 各组件 osd 侧测依赖）。
