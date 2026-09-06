# `update-agent` 规格书

> 显示名：`Update Agent`
> 拥有 crate：`os-update`
> 启动批次：`3`

## 1. 身份

| 字段 | 值 |
|------|-----|
| agent_id | `update-agent` |
| 显示名 | Update Agent |
| 拥有的 crate | os-update |
| Git 长期分支 | `agent/update-agent` |
| 上游依赖 agent | `core-agent`（`TaskId`/`NodeId`/`DateTime`/`HealthReport`）、`iso-agent`（ISO 作为更新源）、`meta-agent`（leader 选举用于滚动升级） |
| 下游被依赖 agent | `api-agent`（OTA/回滚/CVE/滚动管理路由） |
| 启动批次 | `3`，同批可与 discover-agent / guest-agent / provision-agent / backup-agent / monitor-agent / media-agent / files-agent / devtools-agent / power-agent 并行（独占 os-update crate，无同 crate 冲突） |

## 2. 使命陈述

**一句话职责**：为 OS 系统提供运行时升级能力——A/B 双槽位 OTA 更新（签名校验）+ watchdog 自动回滚 + CVE 安全公告监听 + HA 集群滚动升级（follower-first）。

**边界**：
- ✅ 做：实现 `os-update` 的 `UpdateEngine`（check/download/verify/write_to_inactive_slot/activate_slot/status）、`RollbackManager`（list_snapshots/rollback_to/verify_current_health/auto_rollback_if_unhealthy）、`CveMonitor`/`CveCallback`（check_advisories/subscribe）、`RollingUpgrade`（plan/execute/status）；为下游提供 mock。
- ❌ 不做：不实现 ISO 构建/裸机安装（归 iso-agent，批 2，本 agent 消费其产物）；不修改 trait 签名（须走 ADR）；不实现运行时 PXE 自举/迁移（归 provision-agent）；不绕过签名校验激活更新（安全红线）；不耦合 bootloader 私有协议（一律经 A/B 槽抽象）。

## 3. 拥有的契约

> 本 agent 从原 `provision-agent` 拆分而来（§2.1 拆分理由：OTA 是运行时升级，独立阶段）。拥有 os-update crate 全部五 trait。

| crate | trait | 契约路径（相对 `OS_System/`） | 实现优先级 |
|-------|-------|-------------------------------|-----------|
| os-update | `UpdateEngine` | `crates/os-update/src/update.rs` | P0（A/B 双槽，核心） |
| os-update | `RollbackManager` | `crates/os-update/src/rollback.rs` | P0（watchdog 自动回滚，安全关键） |
| os-update | `CveMonitor` | `crates/os-update/src/cve.rs` | P1 |
| os-update | `CveCallback` | `crates/os-update/src/cve.rs` | P2（回调 trait，下游实现） |
| os-update | `RollingUpgrade` | `crates/os-update/src/rolling.rs` | P1（HA 集群滚动） |

**关键数据结构**（本 agent 需实现并对外暴露的 struct/enum）：

| 类型 | 路径 | 说明 |
|------|------|------|
| `UpdateManifest`（version/release_notes/size_bytes/sha256/signature/min_current_version/components） | `os-update/src/update.rs` | 更新清单（含 ed25519 签名 + sha256） |
| `ComponentUpdate`（name/version/restart_required） | `os-update/src/update.rs` | 单组件更新条目 |
| `UpdateSlot`（A/B） | `os-update/src/update.rs` | A/B 双槽位标识 |
| `UpdateStatus`（Downloading{progress}/Verifying/Writing/Activating/Completed/Failed{reason}） | `os-update/src/update.rs` | OTA 更新状态机 |
| `RollbackPoint`（slot/version/created_at/healthy） | `os-update/src/rollback.rs` | 回滚点（历史健康槽快照） |
| `CveAdvisory`（cve_id/affected_component/severity/fixed_version/published_at） | `os-update/src/cve.rs` | CVE 公告 |
| `CveSeverity`（Low/Medium/High/Critical） | `os-update/src/cve.rs` | CVE 严重级别 |
| `RollingStrategy`（FollowersFirst/OneAtATime/AllAtOnce） | `os-update/src/rolling.rs` | 滚动升级策略 |
| `RollingPlan`（order/strategy/per_node_verify） | `os-update/src/rolling.rs` | 滚动升级计划 |
| `RollingStatus`（Pending/Upgrading{current_node,completed}/Completed/Failed{failed_node,reason}） | `os-update/src/rolling.rs` | 滚动升级状态机 |
| `UpdateError`/`UpdateResult` | `os-update/src/error.rs` | 错误枚举（`NoUpdates`/`DownloadFailed`/`VerificationFailed`/`WriteFailed`/`SlotConflict`/`RollbackFailed`/`CveCheckFailed`/`HealthCheckFailed`） |

**关键实现**：
- `AbUpdateEngine`：`impl UpdateEngine`，A/B 双槽位编排；流程：`check_updates` 拉清单 → `download` 下载（返回 `TaskId`）→ `verify`（ed25519 签名 + sha256）→ `write_to_inactive_slot`（写非活动槽）→ `activate_slot`（切换下次启动项，配合 bootloader）；**所有更新须签名校验通过方可激活**。
- `AbRollbackManager`：`impl RollbackManager`，配合 bootloader 双槽引导；`verify_current_health` 启动后探活；`auto_rollback_if_unhealthy` watchdog 自动回滚（不健康则切回上一个健康槽，返回 true 表示已回滚）。
- `NvdCveMonitor`：`impl CveMonitor`，对接 NVD/OSV 数据源轮询；`subscribe` 链式注册 `Box<dyn CveCallback>`（可联动 IM 通知/自动触发更新）。
- `HaRollingUpgrade`：`impl RollingUpgrade`，配合 os-meta leader 选举；`plan` 按策略排定节点顺序（FollowersFirst 默认，leader 最后）；`execute` 逐节点升级（follower 先升 → 单节点验证通过 → 再升 leader），保证 HA 不中断。
- `MockUpdateEngine`/`MockRollbackManager`/`MockCveMonitor`/`MockRollingUpgrade`：feature `mock`，返回确定性状态，供下游测试。

## 4. 输入契约

> 本 agent 依赖的上游 trait，**必须经 `Box<dyn Trait>` 或泛型注入**，不得耦合具体实现。

| 上游 trait | 来源 crate | 来源 agent | mock 位置 | 用途 |
|-----------|-----------|-----------|----------|------|
| `IsoBuilder`（ISO 更新源） | `os-iso` | `iso-agent` | `crates/os-iso/src/mock.rs`（上游提供） | 更新包来源（ISO 作为离线更新源） |
| `Consensus`/`FailoverOrchestrator`（leader 选举） | `os-meta` | `meta-agent` | `crates/os-meta/src/mock.rs`（上游提供） | RollingUpgrade 的 leader/follower 角色判定 |
| `TaskId`/`NodeId`/`DateTime`/`HealthReport` | `os-core` | `core-agent` | —（newtype/数据结构，无 mock） | 任务追踪/节点标识/健康报告 |
| `ApiError`/`ApiErrorCode` | `os-common` | core-agent 间接 | — | 错误码映射 |

**mock 策略**：`TaskId`/`NodeId` 等是纯类型，core-agent `cargo check` 通过即可消费；iso-agent/meta-agent mock 就绪前用本地临时 stub 跑通；无真实 A/B bootloader/NVD 数据源时用各 Mock 覆盖逻辑分支。

## 5. 输出要求

### 5.1 实现规范
- **命名**：实现 struct 命名为 `AbUpdateEngine`、`AbRollbackManager`、`NvdCveMonitor`、`HaRollingUpgrade`，不挂 agent 前缀。
- **错误**：实现方法返回 `Result<T, UpdateError>`；映射 `NoUpdates`/`DownloadFailed(String)`/`VerificationFailed(String)`/`WriteFailed(String)`/`SlotConflict(String)`/`RollbackFailed(String)`/`CveCheckFailed(String)`/`HealthCheckFailed(String)`。
- **测试**：每个公开方法有单元测试；签名校验（ed25519）与 sha256 比对有专门测；watchdog 自动回滚逻辑（健康/不健康分支）有专门测；滚动升级节点顺序（FollowersFirst）有专门测；各 Mock 覆盖返回路径。
- **文档**：每个 pub 项有 `///` 中文文档；A/B 槽切换、签名校验、滚动编排补 `//` 内联注释说明"为什么"。

### 5.2 DoD（Definition of Done，验收清单）
- [ ] `UpdateEngine`/`RollbackManager`/`CveMonitor`/`CveCallback`/`RollingUpgrade` 有具体实现（非 `todo!()`）
- [ ] `cargo check -p os-update` 通过
- [ ] `cargo test -p os-update` 通过
- [ ] `cargo clippy -p os-update -- -D warnings` 无警告
- [ ] 为下游提供 `MockUpdateEngine`/`MockRollbackManager`/`MockCveMonitor`/`MockRollingUpgrade`（`crates/os-update/src/mock.rs`，feature gate `mock`）
- [ ] 更新 `PROGRESS.md`

## 6. 依赖前置

| 依赖 | 类型 | 说明 |
|------|------|------|
| `iso-agent` 交付 `IsoBuilder` mock | **硬阻塞** | 更新源依赖；mock 就绪方可跑通更新流程 |
| `meta-agent` 交付 `Consensus`/`FailoverOrchestrator` mock | **软依赖** | 滚动升级 leader 判定；可先用 stub 并行 |
| core-agent 交付 os-core 类型可用 | **软依赖** | core 已是契约层 |
| A/B bootloader（grub-bls/systemd-boot） | **运行时硬阻塞** | 槽位激活需 bootloader 支持；测试用 mock bootloader |
| root 权限（写槽/激活） | **运行时硬阻塞** | 写系统槽需 root；沙箱测试 |

**可立即启动的部分**：`UpdateManifest`/`RollbackPoint`/`CveAdvisory` 等数据结构已存在；ed25519 签名校验逻辑（纯函数）；`RollingPlan` 节点排序（纯函数）；各 Mock 内存态实现。

## 7. 并行性分析

- **可并行实现的 trait**：`UpdateEngine`（单节点 OTA）、`RollbackManager`（回滚）、`CveMonitor`（CVE 监听）、`RollingUpgrade`（集群滚动）四者领域独立，可四子任务并行。
- **有内部顺序的 trait**：`UpdateEngine.activate_slot` 依赖 `write_to_inactive_slot` 已完成且 `verify` 通过；`RollbackManager.auto_rollback_if_unhealthy` 依赖 `verify_current_health` 结果；`RollingUpgrade.execute` 依赖 `plan` 已生成。
- **瓶颈点**：签名校验正确性是安全关键阻塞点（绕过即安全漏洞）；watchdog 自动回滚的 bootloader 集成是串行关键路径。

## 8. 验收标准

| 维度 | 标准 |
|------|------|
| 编译 | `cargo check -p os-update` 通过 |
| 测试 | `cargo test -p os-update` 通过；关键路径（签名校验、A/B 槽切换、watchdog 回滚、滚动排序、CVE 轮询、mock 返回）覆盖率 ≥ 80% |
| 契约 | 未修改 trait 签名（除非有 ADR）；`cargo doc` 无警告 |
| mock | 四个 Mock 已提交（下游可用） |
| 文档 | pub 项有中文 `///`；PROGRESS.md 已更新 |
| 安全 | 所有更新须 ed25519 签名校验通过方可激活；watchdog 自动回滚验证通过 |

## 9. 风险红线

🔴 **严禁**：
- 修改其他 agent 的 crate（仅可改 os-update）
- 修改 trait 签名（五 trait 方法增删改须经 ADR + 受影响下游 agent 会签）
- 虚构未发布的依赖（ed25519 库/NVD 客户端须在 workspace 已注册）
- **绕过签名校验激活更新**（安全红线，任何"跳过 verify 直接 activate"的代码禁止）
- 在非沙箱环境直接 activate_slot（可能 brick 系统，须 mock bootloader）
- 跳过测试直接提 PR

🟡 **谨慎**：
- 改 A/B 槽策略（grub-bls ↔ systemd-boot ↔ 其他，架构性变更，须 ADR）
- 改 watchdog 回滚阈值（影响系统可用性，须 ReviewAgent 评审）
- 改滚动升级默认策略（FollowersFirst ↔ 其他，影响 HA，须 ADR + 会签 meta-agent）
- 引入新第三方 crate（如签名库/NVD 库）须经 ReviewAgent 评估维护性/安全
- CVE 数据源切换（NVD ↔ OSV ↔ 厂商源，影响公告覆盖，须 ADR）

## 10. 示例工作流

> 以"实现 `UpdateEngine.verify`（ed25519 签名 + sha256 校验）"为例：

1. **开工**：读 `PROGRESS.md`（恢复上下文）+ `TASKS.md`（取任务）+ 本规格书 §3/§4
2. **读契约**：读 `crates/os-update/src/update.rs`（`UpdateEngine` trait + `UpdateManifest`/`UpdateStatus`）+ `crates/os-update/src/error.rs`（`UpdateError::VerificationFailed`）+ 相关 ADR（签名校验策略）
3. **切分支**：`git checkout agent/update-agent`；建子分支 `agent/update-agent/verify-update`
4. **实现**：新建 `impl_update.rs`，定义 `AbUpdateEngine`，`impl UpdateEngine for AbUpdateEngine`；`verify` 先算下载文件 sha256 比对 `manifest.sha256`，再用预置公钥 ed25519 验签 `manifest.signature`（Base64 解码后验签）；任一失败映射 `VerificationFailed`，全过返回 true。
5. **测试**：单元测（合法签名 → true；篡改 sha256 → VerificationFailed；篡改签名 → VerificationFailed；构造测试密钥对验签）；`cargo test -p os-update`
6. **提 PR**：推到远程，PR 标题 `[update-agent] verify-update`，描述含 DoD 勾选状态 + 安全说明（签名校验不可绕过）+ 所需权限
7. **响应评审**：按 ReviewAgent 意见修订；安全相关变更须 ReviewAgent + 安全评审
8. **更新进度**：合并后更新 `PROGRESS.md` + `TASKS.md`

## 11. 启动 prompt 模板

> 复制以下 prompt 启动本 agent 会话（替换 `<...>`）：

```
你是 OS 系统的 Update Agent（agent_id: update-agent）。
你的规格书在 OS_System/docs/agents/update-agent.md——请先完整读取它。
协作约定在 OS_System/docs/agents/_conventions.md——也请读取。
你拥有的契约在 OS_System/crates/os-update/src/update.rs、rollback.rs、cve.rs、rolling.rs（五 trait 归你）。

本次任务：<具体任务描述，或"按 TASKS.md 取下一个任务；优先交付四个 Mock 解锁下游">

开工前必读：
1. OS_System/docs/agents/update-agent.md（你的规格）
2. OS_System/docs/agents/_conventions.md（协作约定）
3. OS_System/docs/agents/update-agent/PROGRESS.md（你的进度，恢复上下文）
4. OS_System/docs/agents/update-agent/TASKS.md（你的任务队列）
5. 你拥有的 crate 的 src/*.rs（契约：update.rs/rollback.rs/cve.rs/rolling.rs/error.rs）
6. 相关 ADR（OS_System/docs/adr/），特别是签名校验策略、A/B 槽决策

特别注意：所有更新须 ed25519 签名校验通过方可激活（安全红线，不可绕过）；
watchdog 自动回滚配合 bootloader 双槽引导；滚动升级默认 FollowersFirst（leader 最后）；
activate_slot 需 bootloader 支持，沙箱用 mock bootloader 测试。
不得修改其他 agent 的 crate；不得破坏 trait 签名（走 ADR）。
完成后：更新 PROGRESS.md + TASKS.md，按 DoD 自检，提 PR。
```

## 12. 恢复协议

> 会话重启后（上下文丢失），按以下顺序恢复：

1. 读 `OS_System/docs/agents/update-agent.md`（本规格书，重识身份）
2. 读 `OS_System/docs/agents/_conventions.md`（重识协作规则）
3. 读 `OS_System/docs/agents/update-agent/PROGRESS.md`（**最关键**——你之前做到哪了）
4. 读 `OS_System/docs/agents/update-agent/TASKS.md`（下一个任务）
5. `git log agent/update-agent --oneline -20`（看最近提交，了解工作状态）
6. `cargo check -p os-update`（确认当前代码状态）
7. 继续未完成任务；若阻塞，在 PROGRESS.md 记录并在 TASKS.md 标 `blocked`

> **若 PROGRESS.md 不存在**（首次启动或丢失）：从规格书 §3 取契约清单（`UpdateEngine`/`RollbackManager`/`CveMonitor`/`CveCallback`/`RollingUpgrade` 五 trait），从 `git log` 推断进度，重建 PROGRESS.md。优先确认签名校验逻辑（`verify`）是否已实现并不可绕过（安全关键，系统升级准入依赖）。
