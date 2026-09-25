# integration-agent 进度日志

## 当前状态
- 阶段：集成测骨架已就位（8 场景全绿：1-5 在 main/integ2，6-8 在 feature/integ3 worktree）
- 最后更新：2026-08-05
- 分支：`feature/integ3`（worktree `os-wt-integ3`，承接场景 6-8）
- 基线 commit：`7ff1c1a`（最新 main）

## 已完成（场景 6-8，feature/integ3 worktree）
- [x] **场景 6：API 路由聚合**（`tests/api_route_aggregation.rs`，8 测）
  - 链路：os-api Gateway 注册多个 RouteHandler（storage/network/compute 三组件）→ 路由匹配（method+path）→ 分发（dispatch）→ 响应聚合
  - 测：三组件路由注册聚合（8 条路由 + component_count=3 + handler_component 字段一致）/ dispatch 跨三组件命中（含调用计数断言）/ 未匹配路径返回 404 / method 不匹配返回 404 / 跨组件重复路由 RouteConflict / 响应聚合 body+headers 透传（echo）/ handler Err 转 5xx / RouteSpec 全字段聚合保留
- [x] **场景 7：discover mTLS 联邦**（`tests/discover_mtls_federation.rs`，9 测）
  - 链路：os-discover 发现 peer → MtlsPeerAuthenticator 双向认证（pair）→ FederationPolicy 评估（check_eligibility + decide）→ FederationStateMachine 推进（Probing→Authenticating→Qualifying→JoiningHa→Active）
  - 测：联邦决策链端到端（资格达标+Auto→Active(Ha)，含调用顺序断言 discover<mtls<elig<join）/ mTLS pair 失败降级 Standalone（不进资格检测）/ 资格不达标+Auto 降级 Standalone / ManualPeer→Active(Peer) / 无 peer NoPeerFound→Standalone / 状态机终态仅 Reset 可离开 / HaJoinFailed 降级 Peer（action_hint=RegisterAsPeer）/ PairingToken+HaEligibility+FederationAction 跨 trait serde round-trip / 联邦终态发 Cluster 事件（spawn 异步发布弱断言）
- [x] **场景 8：update 回滚**（`tests/update_rollback.rs`，12 测）
  - 链路：os-update A/B 双槽位（SlotManager）→ UpdateEngine 写入非活动槽+激活 → 模拟新槽启动失败（探活 Unhealthy）→ watchdog（should_rollback 判定 + SlotManager.on_boot_failed）→ 切回旧槽 + 标记新槽 Failed
  - 测：完整更新成功路径（check→download→verify→write→activate→probe→commit，含调用顺序断言）/ 新槽 boot 失败触发回滚（on_boot_failed 产 Rollback，旧槽恢复 Active 新槽 Failed）/ watchdog 决策链 Automatic+Unhealthy+有目标→RollbackNow / Watchdog 未达阈值不回滚 + 达阈值回滚 / Manual 策略 ManualConfirmationRequired / 首启无目标保护（auto_rollback 返回 false）/ RollbackManager.verify_current_health 返回预置报告 / list_snapshots 返回回滚点 / 连续双升级 A→B→A 槽位交替 / UpdateManifest+UpdateSlot+SlotStatus+HealthReport 跨 trait serde / should_rollback 纯函数决策矩阵（Healthy/Degraded/Unknown/Automatic/Watchdog/无目标）/ 端到端混合路径（第一次成功+第二次回滚）
- [x] `cargo test -p os-integration` 全绿（38 原 + 29 新 = 67 测）
- [x] `cargo check --workspace --features mock` 全绿（不破坏现有）
- [x] `cargo clippy -p os-integration --tests -- -D warnings` 0 warning

### 场景 6-8 暴露的跨 crate 问题（最小修复 / 记录）

#### 问题 7（已修，最小修复）：`os-discover` / `os-update` 的 dev-dependency 未启用 `mock` feature
- **现象**：场景 7/8 需要用 `MockDiscovery` / `MockPeerAuthenticator` / `MockUpdateEngine` / `MockRollbackManager`，但 os-integration 的 dev-dependency `os-discover.workspace = true` / `os-update.workspace = true` 未启用 `mock` feature，导致 mock 类型不可见 + `beacon::pseudo_signature`（gated by `#[cfg(any(test, feature = "mock"))]`）也不可见。
- **修复**：在 `crates/os-integration/Cargo.toml` 把这两行改为 `os-discover = { workspace = true, features = ["mock"] }` / `os-update = { workspace = true, features = ["mock"] }`。无源码改动（仅 os-integration 自己的 Cargo.toml）。
- **性质**：与问题 4（os-services 监控）同模式——os-integration 自己的 dev-dependency 配置，非红线。

#### 问题 8（已规避，记录）：`MockUpdateEngine` 与 `MockRollbackManager` 各持独立 SlotManager（不共享）
- **现象**：场景 8 期望「engine 写入+激活」与「rollback 探活+回滚」共享同一 bootloader 槽位状态，但两个 mock 各持独立 `Mutex<SlotManager>`，状态不同步。
- **根因**：mock 设计为「单 trait 注入」，未考虑跨 trait 状态共享（真实 osd 中 SlotManager 是 bootloader 层共享状态）。
- **影响**：场景 8 的回滚状态机推进不能直接靠 engine+rollback 两个 mock 协作完成。
- **规避**：集成测自建 `UpdateOrchestrator`，额外持一个「真源 SlotManager」（single source of truth）由它主导回滚状态机推进；engine 用于真实跑 check/download/verify/write/activate 的 trait 方法链路（验证 trait 行为），rollback 用于 verify_current_health / list_snapshots / 首启保护路径。**这是 integration-agent 搭建的「业务编排层」的合理用法**——与 ha_failover 场景的 `IntegratedFailoverOrchestrator` 同模式。
- **阻塞**：无。**后续切真时**：真实 SlotManager 是 bootloader 层共享状态，无需 mock 协作。

#### 问题 9（已规避，记录）：`MockRollbackManager` 内部 SlotManager 字段私有，无法外部注入「新槽已激活」状态
- **现象**：想测试「新槽激活后探活失败」的 auto_rollback 路径，需把 MockRollbackManager 内置 SlotManager 设为「B active + A inactive + previous_active=A」，但 `slot: Mutex<SlotManager>` 字段私有，外部无法注入。
- **根因**：mock 未提供「预设槽位状态」的 builder（仅有 `with_health` / `with_policy`）。
- **规避**：用纯逻辑 `SlotManager` + `should_rollback` 直接驱动回滚状态机（场景 8 大部分测用此模式）；MockRollbackManager 仅用于验证其默认行为（首启保护 / verify_current_health / list_snapshots）。
- **阻塞**：无（纯逻辑路径完全可测）。**后续**：若需用 MockRollbackManager 测完整回滚链，可给 mock 加一个 `with_slot_state(SlotManager)` builder（mock-agent 范畴，非红线）。

## 验证矩阵（更新）

| 场景 | 测数 | 跨越 crate | 结果 |
|------|------|-----------|------|
| 1 VM 创建 | 6 | api/compute/storage/core/services | ✅ |
| 2 访客链上验证 | 7 | guest/wallet/security/im | ✅ |
| 3 HA 故障转移 | 7 | meta/compute/storage/network | ✅ |
| 4 备份链路 | 8 | services/storage/core | ✅ |
| 5 IM 对话即操作 | 10 | im/core | ✅ |
| 6 API 路由聚合 | 8 | api | ✅ |
| 7 discover mTLS 联邦 | 9 | discover/core | ✅ |
| 8 update 回滚 | 12 | update/core | ✅ |
| 合计 | **67** | 涉及 16 crate | ✅ |

## 已完成（场景 4-5，feature/integ2 worktree）
- [x] **场景 4：备份链路**（`tests/backup_chain.rs`，8 测）
  - 链路：os-services(backup) SchedulePolicy 定时触发 → os-storage snapshot（create_snapshot）→ send-recv 复制（ZfsSendRecv/Replication）→ os-monitor 告警
  - 测：本地快照全链成功（snapshot→monitor→事件）/ 远程 send-recv 复制链路（含调用顺序断言）/ snapshot 失败中止 + 发 backup.failed 事件 + monitor 记 failed Counter / monitor 预置 alert 规则触发告警 / 源数据集缺失传播 DatasetNotFound / ZfsBackupManager 默认实现 schedule→trigger_now 链路 / SchedulePolicy cron next_run 确定性（纯函数）/ SnapshotId 跨 services-storage 类型一致
- [x] **场景 5：IM 对话即操作**（`tests/im_conversation_as_action.rs`，10 测）
  - 链路：用户"建 VM" → os-im AgentOrchestrator 能力发现 → 委派 ComputeAgent（ToolInvokingAgent）→ agent 调 Tool（MockTool）→ 结果聚合 → SharedContext 黑板共享 + ConfirmationGate 高危确认
  - 测：全链成功（route→delegate→tool→黑板→事件→IM 反馈尾）/ 用户拒绝短路 / Critical 双满足（用户确认+会签法定）/ 任务图无环检测（环→TaskCycle）/ DAG 允许委派 / 黑板命名空间清理 / 用户拒绝优先于投票 / AgentTask.context↔ToolCall.arguments 透传 / Low 风险用户确认即可 / delegate 未知 agent → AgentNotFound+Failed
- [x] `cargo test -p os-integration` 全绿（20 原 + 18 新 = 38 测）
- [x] `cargo check --workspace --features mock` 全绿（不破坏现有）
- [x] `cargo clippy -p os-integration --tests -- -D warnings` 0 warning

## 已完成（首批 1-3 场景，integration/main）
- [x] 新建 `crates/os-integration`（独立集成测 crate，dev-dependencies 引用全部 22 crate，全启用 `mock` feature）
- [x] 加入 workspace members（根 `Cargo.toml`）

## 已完成
- [x] 新建 `crates/os-integration`（独立集成测 crate，dev-dependencies 引用全部 22 crate，全启用 `mock` feature）
- [x] 加入 workspace members（根 `Cargo.toml`）
- [x] **场景 1：VM 创建链路**（`tests/vm_creation_chain.rs`，6 测）
  - 链路：os-api RouteHandler → os-compute VmManager.create_vm → os-storage zvol（create_dataset）→ os-core EventBus → os-services Monitor
  - 测：全链成功 / compute 失败发 Error 事件 / storage 池缺失传播 / EventBus→Monitor 订阅桥接 / VolumeId 跨 crate 类型一致 / VdevSpec 跨 crate 可用
- [x] **场景 2：访客链上验证链路**（`tests/guest_chain_verification.rs`，7 测）
  - 链路：os-guest ChainOrchestrator（DefaultChainOrchestrator 泛型注入）→ os-wallet（WalletConnector/ChainAdapter/RpcRegistry）→ os-security JwtIssuer → os-im ConversationStore（通知尾）
  - 测：全链成功（Completed+address_hash）/ Mandatory+链不可用=Failed / Optional+链不可用=降级 Completed（空 address_hash）/ 验签失败=Failed / 余额不足=Failed / JWT round-trip / wallet mock 自洽
- [x] **场景 3：HA 故障转移链路**（`tests/ha_failover_chain.rs`，7 测）
  - 链路：os-meta FailoverTask 状态机（Triggered→MigratingVm→SwitchingVip→PromotingReplica→Done）→ os-compute migrate_vm + os-meta VipManager.assign + os-storage list_snapshots（副本提升占位）
  - 测：完整状态机驱动全组件（含调用顺序断言）/ migrate 失败 mark_failed 且不切 VIP / VIP 冲突 mark_failed / 无 VM 仍完成 / 状态机前置条件 / 终态不可推进 / VIP 幂等漂移
- [x] 首批基线：`cargo test -p os-integration` 全绿（20 测）/ `cargo check --workspace --features mock` 全绿 / `cargo test --workspace --features mock` 全绿（1491 + 20 = 1511 测）/ `cargo clippy -p os-integration --tests -- -D warnings` 0 warning

## 集成测暴露的跨 crate 问题（最重要）+ 修复

### 问题 1（已修，最小修复）：`Mutex` 临时值跨 await 自死锁
- **现象**：HA 故障转移测 `ha_failover_migrate_failure_*` / `ha_failover_vip_conflict_*` 死锁挂起。
- **根因**：测试代码用 `orch.failover_status(&orch.tasks.lock()...keys().next().copied().unwrap()).await` —— 内层 `orch.tasks.lock()` 返回的 `MutexGuard` 作为临时值，其生命周期被延长到 `.await` 结束；而 `failover_status` 内部又取同一把锁 → **自死锁**。
- **修复**：先 `let tid_for_status = orch.tasks.lock()...keys().next().copied().unwrap();`（语句结束即 drop guard），再 `orch.failover_status(&tid_for_status).await`。
- **性质**：测试代码 bug（非 trait/源码 bug），最小修复，不改任何 crate 源码。**这是真实可复用的教训**——后续所有跨 `Mutex` + `await` 的集成测都须遵守「先 drop 锁再 await」。

### 问题 2（已规避，记录）：`os-storage::Replication` trait 非 dyn 兼容
- **现象**：尝试用 `dyn Replication` 注入副本提升侧失败：`the trait Replication is not dyn compatible`。
- **根因**：`Replication` 是**原生 `async fn in trait`**（ADR-COMPAT-001），未加 `#[async_trait]`，故不可 `Box<dyn>`。
- **影响**：HA 故障转移的「PromotingReplica 副本提升」阶段无法用 `dyn Replication` 注入；本批用 `StorageBackend::list_snapshots` 占位调用（验证调用链通），真实副本提升（`ZfsSendRecv`）需 storage-agent 在 os-storage 内部接通，或为 `Replication` 补 `#[async_trait]`（破坏性变更，须走 ADR）。
- **阻塞**：无（集成测已用占位绕过）。**后续切真时**：要么 storage-agent 提供一个 dyn 兼容的包装 trait，要么走 ADR 给 `Replication` 加 `#[async_trait]`。

### 问题 3（已规避，记录）：`DefaultChainOrchestrator` 用泛型而非 `dyn` 注入
- **现象**：访客链上验证的 `DefaultChainOrchestrator<C, A, R, J>` 用 4 个泛型参数注入 wallet/security，而非 `Arc<dyn Trait>`。
- **根因**：上游 `WalletConnector` / `RpcRegistry` / `os_security::JwtIssuer` 均为原生 `async fn in trait`（非 dyn 兼容，ADR-COMPAT-001），无法 `Box<dyn>`。
- **影响**：集成测中编排器构造签名冗长（需写全 4 个泛型参数），但功能正常。`ChainAdapter` 本身是 `#[async_trait]`（可 dyn），但为构造统一未用 dyn。
- **阻塞**：无（泛型路径完全可用）。**后续**：若上游 trait 统一补 `#[async_trait]`（走 ADR），编排器可简化为 `Arc<dyn>` 注入。

### 问题 4（已规避，记录）：`MockMonitor` 不是 `os-monitor` crate，而是 `os-services::mock::MockMonitor`
- **现象**：启动 prompt 提到 `os-monitor`，但实际 monitor 在 `os-services` 的 `monitor` 子模块（无独立 `os-monitor` crate）。
- **修复**：集成测 dependency 用 `os-services = { features = ["mock"] }`，引用 `os_services::mock::MockMonitor`。无源码改动。

### 问题 5（已规避，记录）：`os-api::ApiGatewayError` 无 `BadRequest`/`InternalError` 变体
- **现象**：variant 实为 `Internal(String)`（无 `BadRequest`、`InternalError`）。
- **修复**：集成测 RouteHandler 错误用 `ApiGatewayError::Internal(...)`，dispatch 把 handler Err 映射为 500。无源码改动。

### 问题 6（已规避，记录）：`os-compute::MockVmManager` 模块私有
- **现象**：`mod mock_vm;`（私有），但 struct 经 `pub use mock_vm::MockVmManager;` 在 crate 根重导出。
- **修复**：集成测用 `use os_compute::MockVmManager;`（顶层），不用 `os_compute::mock_vm::MockVmManager`。无源码改动。

## 验证矩阵

| 场景 | 测数 | 跨越 crate | 结果 |
|------|------|-----------|------|
| 1 VM 创建 | 6 | api/compute/storage/core/services | ✅ |
| 2 访客链上验证 | 7 | guest/wallet/security/im | ✅ |
| 3 HA 故障转移 | 7 | meta/compute/storage/network | ✅ |
| 4 备份链路 | 8 | services/storage/core | ✅ |
| 5 IM 对话即操作 | 10 | im/core | ✅ |
| 合计 | **38** | 涉及 14 crate | ✅ |

## 下一步（建议）
1. **切真依赖**：当 owner agent 把各 mock 切真实实现（libvirt/ZFS/网络/链节点）后，集成测逐步从 mock 切真（保留 mock 路径作 CI 快速回归）。
2. **问题 2/3 的 ADR 推进**：若需要 dyn 注入，给 `Replication`/`WalletConnector`/`RpcRegistry`/`JwtIssuer` 走 ADR 加 `#[async_trait]`。
3. **场景 4 远程复制切真**：当前 ZfsSendRecv 是骨架（立即置 Completed），真实 `zfs send | ssh ... zfs recv` 管道接通后，集成测的 `backup_pipeline_remote_replication_chain` 可加进进度/失败路径断言。
4. **场景 5 LLM 切真**：当前用 ToolInvokingAgent 包装 MockTool（绕过 LLM 意图解析）；LLM 接通后可加「自然语言→intent 解析」环节的集成测（MockLlmBackend 注入 tool_calls）。

## 阻塞
- 无硬阻塞。问题 2/3 已用占位/泛型绕过，不阻塞当前集成测骨架。

## 备注
- 本批集成测**全 Mock**，不真起 libvirt/ZFS/网络/链节点（红线遵守）。
- 未改任何 trait 签名（红线遵守）。
- 未改其他 crate 源码（红线遵守）——本批只新建 `crates/os-integration` + workspace members 一行 + 本 PROGRESS。
