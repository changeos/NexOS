# provision-agent 进度日志

## 当前状态
- 阶段：实现中（批次 3 第一轮交付完成）
- 最后更新：2026-08-05

## 已完成
- [x] **纯逻辑实现**（commit: 待提交，本轮）
  - `crates/os-provision/src/exclude.rs`：§3.19 敏感项排除清单——纯路径匹配算法
    - `ExcludePattern`（Exact/Prefix/Glob）+ `ExcludeCategory`（8 类覆盖 §3.19 全部）
    - `default_excludes()`：系统口令/TLS 私钥/SSH 私钥/SMB 凭证/DB 密码/JWT-TOTP/
      钱包密钥/集群密钥 共 20 条默认规则
    - `ExcludeRules::evaluate/partition`：过滤→(应传输, 应排除)
    - glob 引擎：token 化（`**/` 段 / `**` 尾 / `*` 单层 / 字面）+ DP 匹配，
      正确处理 `**` 跨目录、`/etc/ssl/private/**/*.key` 等模式
    - 14 个单测
  - `crates/os-provision/src/phase.rs`：迁移阶段状态机
    - `MigrationPhase`（SystemInit/FileTransfer/ExcludeSensitive/FirstBoot）
      + Ord/Display/next/all
    - `PhaseMachine`：顺序推进 + 安全防御（**ExcludeSensitive 不可跳过**）
    - `PhaseTransition`（Advance/Pending/Failed）+ `resume_from` 断点恢复
    - 7 个单测
  - `crates/os-provision/src/checkpoint.rs`：断点续传模型
    - `MigrationCheckpoint`（plan_id/completed_phases/current_phase/transferred/
      exclude_outcome/updated_at）——**只存校验和/路径/大小，绝不存密钥本体**
    - `TransferredFile`（path/size/checksum）、`ExcludeOutcome`（仅计数/类别，无密钥）
    - `CheckpointPolicy::decide_resume`：纯决策算法
      （Finished / ResumeFromPhase{phase, remaining_datasets, remaining_excludes}）
    - 10 个单测
  - `crates/os-provision/src/package.rs`：迁移包格式
    - `PackageManifest` + `PackageEntry` + `MigrationPackage`
    - `pack_to_bytes`/`unpack_from_bytes`（JSON + 内容指纹回填/校验）
    - `audit()`：安全自检——发现命中排除清单的条目即拒绝（最后一道防线）
    - 6 个单测
- [x] **Mock 交付**（feature `mock`）
  - `crates/os-provision/src/mock.rs`：`MockProvisioner` + `MockMigrationEngine`
    - builder 风格（with_ready_node/with_boot_failure/with_init_failure/
      with_plan_failure/with_execute_failure）
    - 内存状态机，覆盖各成功/失败返回路径
    - 11 个 mock 自测

## 进行中
- 无

## 阻塞
- 无

## DoD 自检（本轮）
- [x] 敏感项排除过滤（exclude.rs）+ 断点续传决策（checkpoint.rs）+ 迁移状态机
      （phase.rs）完整 + 测试
- [x] trait 骨架（Provisioner/MigrationEngine 契约未改签名；Mock 实现 trait）
- [x] `cargo check -p os-provision --features mock` 0 error
- [x] `cargo test -p os-provision --features mock` 通过（49 测）
      `cargo test -p os-provision`（无 mock）通过（39 测）
- [x] `cargo clippy -p os-provision --features mock -- -D warnings` 0 warning
- [x] `cargo doc -p os-provision --features mock --no-deps` 无警告
- [x] Mock 已提交（feature gate `mock`，`#![cfg(feature = "mock")]`）

## 安全红线遵守
- 未在迁移包/checkpoint 中存任何密钥/密码明文（`ExcludeOutcome` 仅存计数+类别）
- `root_password_hash` 仅哈希占位（契约层 `ProvisionConfig` 已注释首启重设）
- 未引入新第三方依赖（无 sha2/tar/zstd；指纹用 std::hash::DefaultHasher）
- 未修改 trait 签名、未改其他 agent crate

## 下一步（待主代理/上游就绪后）
1. 上游 mock 就绪后：把 `MockProvisioner`/`MockMigrationEngine` 的编排逻辑
   从纯内存切换为注入 `os-network::PxeServer` / `os-storage::Replication` /
   `os-meta::MetaStore`（实现真正的 `PxeProvisioner` / `ZfsMigrationEngine`）
2. 集成 `ExcludeRules` 到 `MigrationEngine::execute` 的打包前过滤
3. 集成 `CheckpointPolicy` 到 `MigrationEngine::resume` 的断点续传决策
4. 跨 agent 集成测（与 storage/network/meta 联调）
