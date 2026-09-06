# os-update

> 系统更新层 · A/B 双槽位 OTA + 回滚 + CVE 监听 + HA 滚动升级 · 规划文档 §3.12

OS 的更新 crate：A/B 双槽位 OTA（ed25519 签名校验）、watchdog 自动回滚、
C 依赖 CVE 监听（Samba/QEMU/rdma-core 等）与 HA 集群滚动升级（follower 先、
leader 最后）。契约层 + 大量纯逻辑骨架（状态机/决策）+ 已接通的 bootloader 编排。

## 核心能力

- **更新引擎**（`update` / `impls`）：`UpdateEngine` trait（`UpdateManifest` /
  `UpdateSlot` / `UpdateStatus`）；默认实现 `AbUpdateEngine`（含 ed25519
  签名校验）。
- **A/B 槽位状态机**（`slot`）：`SlotManager` / `SlotState` / `SlotStatus` /
  `SlotSwitchDecision`——槽位切换决策纯逻辑，无 bootloader 依赖。
- **回滚**（`rollback` / `impls`）：`RollbackManager` trait + `AbRollbackManager`；
  `should_rollback`（策略 + 触发条件判定纯函数，watchdog 启动探活失败回退旧槽）。
- **CVE 监听**（`cve` / `impls`）：`CveMonitor` trait + `NvdCveMonitor`
  （`CveAdvisory` / `CveSeverity`；`CveCallback` 用 `#[async_trait]` 支持
  `Box<dyn>` 多态）。
- **HA 滚动升级**（`rolling` / `version`）：`RollingUpgrade` trait +
  `HaRollingUpgrade`；`decide_upgrade_order` 节点顺序决策 + 滚动状态机推进器；
  `version` 内 semver 比较（`compare_versions`）与升级路径决策
  （`upgrade_decision`）。
- **bootloader 编排**（`bootloader`）：GRUB / systemd-boot 槽位激活——
  `BootloaderConfig` 配置生成 + `BootloaderRunner` 执行抽象（默认
  `TokioBootloaderRunner`），`activate_slot` 已接真实编排。

## 架构位置

**依赖**（上游）：`os-core`、`os-common`（`From<UpdateError> for ApiError`）；
签名校验依赖 ed25519-dalek（workspace 注册）。

**被用**（下游）：当前 workspace 内无编译依赖方；os-api 的更新路由为其预期
消费场景（`mock` 桩注释标注「供下游 api-agent 测试注入」）。

## 独立使用

- **仓库外引用**：`os-update = { git = "http://ub2604:8080/git/nexos.git" }`。
- **关键接口**：`UpdateEngine` / `RollbackManager` / `CveMonitor` /
  `RollingUpgrade` 四 trait + `SlotManager` 槽位状态机 + 纯逻辑决策函数
  （`should_rollback` / `decide_upgrade_order` / `upgrade_decision`，
  可独立复用于任意 A/B 系统）。
- **feature**：`mock`（默认关）——`MockUpdateEngine` / `MockRollbackManager` /
  `MockCveMonitor` / `MockRollingUpgrade`（供下游 api-agent 测试注入）。

## 测试

```bash
cargo test -p os-update
```

纯逻辑（槽位状态机/semver/回滚判定/滚动顺序）单测默认跑；bootloader 真实
执行测在 `tests/bootloader_real.rs` 中以 `#[ignore]` 标记，需 root。
