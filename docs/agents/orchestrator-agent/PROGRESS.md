# orchestrator-agent 进度日志

## 当前状态
- 阶段：进行中（cgroup v2 配额已接通真实 cgroups-rs；systemd unit + NTP 仍留 TODO）
- 最后更新：2026-08-05

## 本次任务范围（P2 接通 cgroups-rs）

按 orchestrator-agent 主代理 P2 任务：把 osd 的 cgroup 配额从"骨架"推进到
真实 cgroups-rs cgroup v2 实现。**红线**：不修改 trait 签名 / 其他 agent crate /
不真写 cgroup（需 root，测试用 fixture）。

## 已完成
- [x] **`crates/osd/Cargo.toml` 加 `cgroups-rs.workspace = true`**
  （ADR-DEPS-002 已在 workspace 注册 `cgroups-rs = "0.5"`）
- [x] **新建 `crates/osd/src/cgroup.rs`**（真实 cgroup v2 配额实现）：
  - **`CgroupBackend` trait**：抽象 cgroup v2 的"写配额/读配额"操作，
    便于单元测试用内存后端替身，避免真写 `/sys/fs/cgroup`（需 root，规格 §9 红线）。
  - **`CgroupsRsBackend`**（生产后端，需 root + cgroup v2）：
    - `apply_quota`：把 `ResourceQuota` 翻译成 `cgroups_rs::fs::Resources`，
      经 `Cgroup::apply` 写 `cpu.max`（CFS quota=核数×100000us，period=100000us）/
      `memory.max`（字节硬上限）。
    - `read_quota`：从 `CpuController::cfs_quota/period` +
      `MemController::get_mem().max` 反推 `ResourceQuota`。
    - `is_v2()`：探测 cgroup v2 unified 模式（`OnceLock` 缓存）；非 v2 返回
      `QuotaFailed` 结构化错误（不 panic）。
    - `quota_to_resources` / `resources_to_quota`：纯函数翻译，单测往返覆盖。
  - **`InMemoryCgroupBackend`**（测试后端，零 root 依赖）：
    - `HashMap<(base, id), ResourceQuota>`，`apply`/`read` 原样存读；base 隔离。
  - **`CgroupQuota`**：组件 ID → cgroup 路径 + 后端委派 + 快照缓存。
    - `new(base)`：用真实 `CgroupsRsBackend`（生产）。
    - `with_backend(base, backend)`：注入自定义后端（测试）。
    - `set_quota`：写后端 + 缓存快照（成功后才更新）。
    - `get_quota`：优先读后端；None 则回退快照。
- [x] **`SystemdOrchestrator` 接通 `CgroupQuota`**（`impl_orchestrator.rs`）：
  - 新增字段 `cgroup: CgroupQuota`（替代旧的 `quotas: RwLock<HashMap>`）。
  - 构造器：`new(registry)`（默认 base="os" + 真实后端）、
    `with_cgroup_base(registry, base)`、`with_cgroup_backend(registry, base, backend)`（测试注入）。
  - `set_quota` 委派 `self.cgroup.set_quota(id, &quota)` → 真实 cgroup v2 写入。
  - `get_quota` 委派 `self.cgroup.get_quota(id)`，None 回退描述符默认配额。
  - 模块文档权限标注表更新：set_quota/get_quota 标"**真实接通**"。
- [x] **lib.rs 导出** `CgroupBackend` / `CgroupQuota` / `CgroupsRsBackend` /
  `InMemoryCgroupBackend`；注册 `pub mod cgroup;`。
- [x] **新增测试**（共 +24 测）：
  - `cgroup.rs`：17 测（quota↔resources 翻译往返、内存后端 apply/read、
    CgroupQuota set/get、真实后端在非 v2/非 root 环境降级不 panic）。
  - `impl_orchestrator.rs`：+7 测（set_quota 写穿后端、完整三字段往返、
    unlimited 往返、覆盖写、custom base、默认 base="os"、组件隔离）。
  - 旧测的 `build()` helper 改为注入 `InMemoryCgroupBackend`（避免非 root 真写失败）。

## DoD 状态（本次 P2 任务）
- [x] cgroups-rs 真实 cgroup v2 配额（fixture 测，不真写）—— ✅ `InMemoryCgroupBackend`
      注入测试；真实 `CgroupsRsBackend` 已实现，仅在生产 root 环境真写。
- [x] `cargo check -p osd --features mock` 通过
- [x] `cargo test -p osd --features mock` 通过（**59 测**，比原 35 多 24）
- [x] `cargo test -p osd`（默认）**55 测**通过（比原 28 多 27）
- [x] `cargo clippy -p osd --all-targets --features mock -- -D warnings` 无警告
      （默认 + mock 均清；MSRV 1.75 兼容——`Result::inspect` 1.76+ 不可用，已改写）
- [x] `cargo doc -p osd --no-deps --features mock` 无警告
- [x] `cargo check --workspace` 通过（不破坏其他 crate）
- [x] 测试数 ≥ 原有 + 新增（59 ≥ 35 + 24）

## 验证输出
```
cargo check  -p osd                  → Finished, 0 error 0 warning
cargo check  -p osd --features mock  → Finished, 0 error 0 warning
cargo test   -p osd                  → 55 passed; 0 failed
cargo test   -p osd --features mock  → 59 passed; 0 failed (1 doctest ignored)
cargo clippy -p osd --all-targets --features mock -- -D warnings → Finished, 0 warning
cargo clippy -p osd --all-targets            -- -D warnings → Finished, 0 warning
cargo doc    -p osd --no-deps --features mock → Finished, 无警告
cargo check  --workspace              → Finished（cgroups-rs 0.5.1 编译通过）
```

## 设计决策
1. **`CgroupBackend` trait 抽象**：把"cgroup 写/读"抽象成 trait，生产用 `CgroupsRsBackend`
   （真实 root 路径），测试用 `InMemoryCgroupBackend`（内存哈希）。这是规避"真写 cgroup
   需 root"红线（规格 §9）的关键——`SystemdOrchestrator` 持有 `Box<dyn CgroupBackend>`，
   测试构造时注入内存后端，生产构造时用真实后端。**未改任何 trait 签名**（Orchestrator
   trait 的 set_quota/get_quota 签名不变）。
2. **CFS 配额转换**：`cpu_cores = c` → `cpu.max = "<c×100000> 100000"`（100ms 周期）。
   c=2 → 200000us/100000us = 2 核。`None`/`0.0` → `max`（不限）。
3. **IO 限制暂只记录快照**：cgroup v2 `io.max` 需 `<major>:<minor> rbps=<bytes>`，
   但 `ResourceQuota.io_bps_limit` 不携带设备号。本次 IO 字段经 `InMemoryCgroupBackend`
   完整往返（测试覆盖），真实后端的 IO 写入留待 `ResourceQuota` 加 `device` 字段后补
   （走 ADR trait 签名修订流程）。CPU/内存已完整真实写入。
4. **MSRV 兼容**：workspace MSRV=1.75.0，`Result::inspect`（1.76+）会触发
   `clippy::incompatible_msrv`，改用 `?` + 显式 insert。

## 阻塞项（⛔ 留待后续阶段，本次按红线不做）
- ⛔ **真实 systemd unit 生成 + 进程监管**：需 root + systemd + CAP_SYS_ADMIN +
  集成测环境（沙箱容器）。`impl_orchestrator.rs` 内 `do_start_inner`/`do_stop_inner`
  相关位置标 `TODO(集成阶段)`。
- ⛔ **真实 cgroup 后端在沙箱的集成测**：`CgroupsRsBackend` 已实现，但单元测环境非 root，
  无法真写 `/sys/fs/cgroup`。集成测需 root + cgroup v2 挂载的沙箱容器
  （规格 §7 瓶颈点）。当前用 `InMemoryCgroupBackend` 覆盖逻辑正确性。
- ⛔ **`ChronyNtp`（`NtpManager` 的真实实现）**：依赖 chrony 绑定（未注册）
  + CAP_SYS_TIME（root）。当前 `NtpManager` trait 与 `NtpStatus` 模型已就绪，
  实现待 chrony 绑定注册 + root 环境后做。
- ⛔ **退避重启（指数退避，连续失败超阈值标 `Failed`）**：依赖真实进程监管，
  待 systemd 集成阶段一并实现（规格 §3 关键实现，标在 TODO）。
- ⛔ **IO 限速真实写入（`io.max`）**：`ResourceQuota` 缺设备号字段，待 ADR 扩展后补。

## 契约问题（发现的，已规避，未擅改 trait）
- **`os-core::EventBus` 非 dyn 兼容**：该 trait 用原生 `async fn in trait`
  （`eventbus.rs` 顶部 `#[allow(async_fn_in_trait)]`），方法不能进 vtable，
  故 `Box<dyn EventBus>` 触发 `E0038: trait is not dyn compatible`
  （ADR-COMPAT-001 已记录此模式）。规格书 §4 期望编排器注入 `Box<dyn EventBus>` 上报事件。
  **本次规避**：`SystemdOrchestrator` 不强持有 `Box<dyn EventBus>`；事件上报留待
  core-agent 就绪后通过 dyn 兼容封装（如 `EventEmitter` trait，需新 ADR）或泛型参数接入。
  **升级建议**：若需 dyn 派发 EventBus，须由 core-agent 对 `EventBus` 加 `#[async_trait]`
  （走 ADR + 受影响 agent 会签），或新建 dyn 兼容的窄接口。

## 下一步（建议）
1. 等待 core-agent 交付 `TokioBroadcastBus`（EventBus 实现）+ 确认 EventBus dyn 兼容策略（ADR）
2. 准备 root + systemd + cgroup v2 沙箱容器 → 跑 `CgroupsRsBackend` 集成测（真写 cpu.max/memory.max）
3. 在沙箱实现真实 systemd unit 生成 + 进程监管 + 退避重启
4. 注册 chrony 绑定（ReviewAgent 评估）→ 实现 `ChronyNtp`（NtpManager）
5. （可选）扩展 `ResourceQuota` 加 `device` 字段 → 实现 `io.max` 真实写入（走 ADR）
