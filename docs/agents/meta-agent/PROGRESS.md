# Meta Agent 进度（os-meta）

> 分支：`agent/meta-agent`（本工作在 worktree `os-wt-p2-meta`，分支 `p2/meta-agent`）
> 最新提交见 `git log --oneline -5`

## 当前状态（2026-08-05，P2 接通）

**P2 接通完成**：在原骨架基础上接通**真实 openraft 共识** + **真实 rusqlite MetaStore**
（ADR-DEPS-002）。两处原"骨架/内存"实现替换为真实后端，55 测全保留并新增 10 测。

### 本轮交付（P2 接通：openraft + rusqlite）

| 模块 | 状态 | 说明 |
|------|------|------|
| `raft_backend.rs` | ✅ 新增 | openraft 0.9 真实后端适配：`MetaRaftConfig`（类型集）+ `MemoryRaftStore`（最小内存 LogStore/StateMachine）+ `NullNetwork`（单节点空网络）+ `spawn_single_node()`（启动单节点集群 + initialize → 当选 leader） |
| `impls.rs::OpenraftConsensus` | ✅ 真实 | 双模式：`new()`/`with_state()`（轻量，兼容 MockConsensus）+ `start_single_node(id)`（真实 openraft 单节点）。真实模式下 `status`/`get_leader`/`get_members` 查询 Raft metrics（真实选主结果） |
| `impls.rs::SqliteMetaStore` | ✅ 真实 | 真实 rusqlite 后端：统一表 `meta_kv(table_name, pk, value)`，`apply_log` 事务 UPSERT/DELETE，`snapshot`/`restore` 用 JSON 行集（SQLite 逻辑 dump），`query` 真实参数化 SQL（兼容旧 `SELECT * FROM <table>` 自动重写） |
| `OpenraftKv` / `HaFailoverOrchestrator` / `NetlinkVipManager` | ⏩ 不变 | 保留原内存实现（CAS 状态机 / FailoverTask 驱动 / VIP owner 态），非本轮范围 |

**依赖**：`Cargo.toml` 加 `openraft.workspace = true`（启用 `serde`/`storage-v2`/`single-term-leader`）
+ `rusqlite.workspace = true`（workspace 已声明 `bundled`）。

**测试**：55 → **65** 测全绿（mock 特性下）；默认特性 58 测。新增测覆盖：
- openraft 单节点：自选 leader / join 返回角色 / leave 清空状态
- SQLite：put→query / delete / 多表隔离 / 真实参数化 SQL / snapshot→restore 往返（fresh + 覆盖）/ 数字键 vs 字符串键

### 关键设计决策

1. **openraft 双模式**：`OpenraftConsensus` 持 `Mutex<Option<Raft>>`。`new()`/`with_state()`
   走轻量路径（不启动 Raft）以兼容 `MockConsensus`（测试替身无需 Raft 开销）；
   `start_single_node(id)` 启动真实 Raft。所有方法在真实模式下查询 Raft metrics，
   轻量模式下回落到内存态。
2. **`#[add_async_trait]` 仅用于 trait 定义，不用于 impl**：openraft 0.9 的宏只改 trait 定义
   （加 Send bound），impl 块直接用原生 `async fn in trait`（Rust 1.75+ 稳定）。
3. **SQLite 统一表 + 逻辑 dump**：所有业务命令作用到单表 `meta_kv`，pk/value 用 JSON 字符串
   规范化（与 `InMemoryMetaState` 一致），snapshot 序列化全表为 JSON 行集（跨节点传输友好）。
4. **`query` 双语义**：`SELECT * FROM <table>` 自动重写为 meta_kv 查询并解析 value（兼容旧契约）；
   其他 SQL 原样执行（每行以列名为键的 JSON 对象，TEXT 列自动解析为 JSON）。
5. **openraft feature 必须显式启用**：`storage-v2`（`Sealed` impl 仅在该 feature 下展开，
   否则无法为自定义 store 实现 trait）+ `serde`（AppData 的 OptionalSerde 要求）+
   `single-term-leader`（简化 Vote/LeaderId）。

## 历史状态（2026-08-05 早些，骨架交付）

批次 2 启动交付：os-meta 5 个 trait 的**纯算法 + 内存骨架 + Mock** 实现（不含真实
openraft / SQLite / netlink 接入，按规格书 §6 硬阻塞项暂缓）。本轮由修复型子代理
在原 meta-agent 工作基础上**修掉全部编译错误与警告**，使 DoD 全绿并提交。

### 本轮交付（修复编译 + dyn 兼容）

| 模块 | 状态 | 说明 |
|------|------|------|
| `raft.rs` | ✅ 完成 | Raft 纯算法（majority / 选举判定 / commitIndex 推进 / 脑裂防护），18 测全绿 |
| `meta_apply.rs` | ✅ 完成 | apply_log 命令分发模型（MetaCommand / MetaTable / InMemoryMetaState），10 测全绿 |
| `failover_sm.rs` | ✅ 完成 | 故障转移纯状态机（Triggered→MigratingVm→SwitchingVip→PromotingReplica→Done/Failed/Aborted），10 测全绿 |
| `impls.rs` | ✅ 骨架 | 5 个 trait 的内存默认实现（OpenraftConsensus / OpenraftKv / SqliteMetaStore / HaFailoverOrchestrator / NetlinkVipManager），CAS 完整 |
| `mock.rs` | ✅ 完成 | 5 个 Mock（feature gate `mock`），builder 风格，含 `Box<dyn>` dyn 兼容断言 |
| `consensus/kv/meta_store/failover/vip.rs` | ✅ 契约 | trait 定义（加 `#[async_trait]`，见下） |

**测试**：55 测全绿（mock 特性下）/ 48 测（默认特性下）。

### 修复的编译问题

1. **E0277（`impls.rs`）**：`ConsensusInner` 原 `#[derive(Default)]`，但其字段 `state: ClusterState`
   未实现 `Default`（契约枚举不擅自扩 derive，按红线）。改用手写 `impl Default for ConsensusInner`
   （默认角色 `Standalone`，与 `OpenraftConsensus::new` 一致）。
2. **E0424（`mock.rs:64`）**：`MockConsensus::with_state` 是关联函数（无 `self`），却用 `..self`
   展开语法。改为 builder 方法 `pub fn with_state(self, ...) -> Self`，保留链式调用语义
   （测试 `mock_consensus_members_injected` 已验证链式 `.with_state(...).with_members(...)`）。
3. **E0038（5 trait dyn 不兼容）**：`Consensus` / `DistributedKv` / `MetaStore` /
   `FailoverOrchestrator` / `VipManager` 均在 `mock.rs::_assert_dyn_compatible`
   经 `Box<dyn Trait>` 运行期多态（编译期断言对象安全），但 5 个 trait 用原生
   `async fn in trait`，方法不进 vtable → E0038。**遵循 ADR-COMPAT-001**：给 5 个 trait
   及其所有 impl 块（impls.rs + mock.rs）加 `#[async_trait]`，Cargo.toml 加
   `async-trait.workspace = true`。**trait 方法签名未改**（仅加宏属性，ADR-001 允许，不算签名变更）。
4. **类型不匹配（`raft.rs` 测试）**：`has_quorum_set(&self, granted: &[NodeId])` 但测试误传
   `&["a","b"]`（`&[&str]`），改用 `node(...)` helper 转换为 `NodeId`。
5. **clippy `field_reassign_with_default`（`impls.rs:with_state`）**：改用结构体字面量初始化。
6. **dead_code（`ConsensusInner.self_id`）**：骨架阶段仅注入不读取（真实 openraft 用），
   加 `#[allow(dead_code)]` + 注释说明，留待 openraft 注册后启用。
7. **unused import**：移除 `mock.rs` 顶层 `Utc` / `ClusterConfig`（仅测试用，下沉到 test 模块）。
8. **doc 链接（rustdoc）**：转义 `log[N]` / `log[index]` / `[rusqlite]` / `[mock]` 等方括号。

### 契约层变更记录（ADR 合规）

- **给 5 个 async trait 加 `#[async_trait]`**：遵循 **ADR-COMPAT-001**（凡 `Box<dyn>` 用的
  async trait 一律 `#[async_trait]`）。os-meta 的 5 个 trait 均经 `Box<dyn>` 多态
  （见 `mock.rs::_assert_dyn_compatible`），符合判定准则。**未新增 ADR**（ADR-001 已覆盖），
  **未改 trait 方法签名**（仅加宏属性，按红线允许）。

## DoD 自检（本轮）

- [x] `cargo check -p os-meta --features mock` → 0 error 0 warning
- [x] `cargo clippy -p os-meta --all-targets --features mock -- -D warnings` → 0 warning
- [x] `cargo test -p os-meta --features mock` → 55 passed
- [x] `cargo doc -p os-meta --features mock --no-deps` → 0 warning
- [x] commit（含原 meta-agent 工作 + 本轮修复）

## 后续阻塞项（真实实现，待依赖就绪）

按规格书 §6，真实实现阻塞项（非本轮范围）：

| 阻塞项 | 类型 | 影响实现 | 当前替代 |
|--------|------|---------|---------|
| openraft 注册到 workspace | 硬阻塞 | `OpenraftConsensus` / `OpenraftKv` 真实 Raft 共识 | 内存骨架（单节点） |
| rusqlite / sqlite 注册 | 硬阻塞 | `SqliteMetaStore` 真实 SQLite 后端 | `InMemoryMetaState` |
| netlink 绑定（系统级） | 待定 | `NetlinkVipManager` 真实 VIP 漂移 + ARP | 内存 owner 态 |
| os-compute `VmManager` mock | 软依赖 | `HaFailoverOrchestrator` 真实 VM 迁移执行 | stub（状态机记录意图） |

## 未发现的 trait 方法 bug

本轮审查 5 个 trait + impl + mock 未发现 trait 方法本身存在逻辑 bug（仅修编译/dyn 兼容/警告）。
若后续真实实现阶段发现 trait 方法签名或语义问题，将按红线走 ADR + 会签，不在本修复轮擅改。
