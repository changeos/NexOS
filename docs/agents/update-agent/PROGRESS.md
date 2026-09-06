# update-agent 进度日志

## 当前状态
- 阶段：完成（批 3 纯逻辑骨架 + Mock 交付）+ 接通真实实现（reqwest/ed25519/sha2）
- 最后更新：2026-08-05

## 已完成
- [x] A/B 双槽位状态机 + 槽位切换决策（`slot.rs`：SlotManager/SlotStatus/SlotState/SlotSwitchDecision）——纯逻辑，无 bootloader 依赖（commit: 待提交）
- [x] 更新包模型 + semver 版本比较 + 升级路径决策（`version.rs`：Version/UpdatePackage/compare_versions/upgrade_decision）——纯逻辑
- [x] 滚动升级节点顺序决策 + 状态机推进（`rolling.rs` 扩展：decide_upgrade_order/RollingStateMachine）——纯逻辑
- [x] 回滚策略 + 触发条件判定（`rollback.rs` 扩展：RollbackPolicy/should_rollback/RollbackContext/RollbackDecision）——纯逻辑
- [x] 四 trait 默认实现骨架（`impls.rs`：AbUpdateEngine/AbRollbackManager/NvdCveMonitor/HaRollingUpgrade）——决策路径可用
- [x] 四个 Mock（`mock.rs`：MockUpdateEngine/MockRollbackManager/MockCveMonitor/MockRollingUpgrade，feature `mock`）——已提交
- [x] **接通真实 I/O（本批）**：
  - `real.rs`：ed25519 验签 + sha256 摘要 + reqwest 下载 + OSV 解析（纯函数 + 单测）
  - `AbUpdateEngine`：`check_updates`/`download`/`verify` 接通 reqwest + ed25519 + sha256（替换原 todo!()，安全红线已落地）
  - `NvdCveMonitor`：`check_advisories` 接通 reqwest POST OSV `/query` 批量轮询 + 解析过滤
  - Cargo.toml：注册 `reqwest`/`ed25519-dalek`/`sha2`/`base64`/`hex` + 测试 `tempfile`

## 进行中
- 无

## 阻塞
- 无

## DoD 自检（接通真实实现批）
- [x] 更新包 reqwest 下载真实（`real::download_to_file` + `AbUpdateEngine::download`）
- [x] ed25519 签名校验真实（`real::verify_package`，不再 `todo!()`；fail-closed）
- [x] CVE reqwest 轮询真实（`NvdCveMonitor::check_advisories`，fixture 测）
- [x] `cargo check -p os-update --features mock` 0 error
- [x] `cargo test -p os-update --features mock` 通过（**112 passed** = 90 旧 + 22 新）
- [x] clippy 0 warning（`cargo clippy -p os-update --features mock --all-targets -- -D warnings`）
- [x] `cargo doc -p os-update --features mock --no-deps` 0 warning
- [x] trait 签名未改；其他 agent crate 未改
- [x] verify 不可绕过签名（fail-closed：sha256 不匹配 / 签名无效 / I/O 失败 均映射 VerificationFailed）

## 测试覆盖（112 项，按模块）
- `slot::tests`：19 项（初始态/可写槽/begin-finish-fail 写入/激活规划-应用/boot 成功-失败回滚/冲突自愈/端到端升级循环）
- `version::tests`：22 项（parse/比较/预发布排序/全量-增量包/升级决策各分支）
- `rolling::tests`：14 项（FollowersFirst leader 最后/OneAtATime/AllAtOnce/多 leader 报错/空成员报错/peer-standalone 排序/状态机推进-失败-完成）
- `rollback::tests`：13 项（健康-Degraded-Unknown 分支/Automatic/Manual/Watchdog 阈值/无目标优先）
- `mock::tests`：22 项（四个 Mock 的默认返回/预置/错误注入/槽位切换/回滚/CVE 订阅/滚动执行完成）
- `real::tests`：**+13 项新增**（sha256 已知向量/文件摘要/验签往返/篡改内容失败/错误密钥失败/缺文件 fail-closed/非法 Base64/OSV 过滤/无 CVE alias 回退 id/非法 JSON/空响应/缺 fixed_version/非法 published 回退）
- `impls::tests`：**+9 项新增**（下载+验签往返/404 失败/篡改拒签/清单解析/空清单 NoUpdates/CVE 轮询过滤/CVE 空响应/CVE 上游 500/CVE 订阅计数；均走真实 reqwest + 本地 TcpListener fixture）

## 关键设计决策
1. **纯逻辑前置**：bootloader/ostree/ed25519/NVD 依赖未注册前，先把所有"决策"逻辑（槽位切换/版本比较/节点顺序/回滚判定）做成纯函数+纯状态机，可独立测试；本批接通真实 I/O 后保留这些纯逻辑复用。
2. **SlotManager**：封装 A/B 双槽内存状态机，UpdateEngine/RollbackManager/Mock 均复用其决策（writable_slot/begin-finish_write/plan_activation/apply_activation/on_boot_succeeded/on_boot_failed/resolve）。
3. **NodeId 比较**：os-core 的 `NodeId` newtype 未派生 Ord（仅 PartialEq/Eq/Hash），节点排序用 `.as_str().cmp()`。
4. **安全红线（已落地）**：`verify` 一律经 `real::verify_package` 真实验签——先比 sha256（大小写不敏感 hex），再 ed25519 验签（签名覆盖文件 sha256 的 32 字节原始摘要）。任一失败或 I/O 错误均映射 `VerificationFailed`（fail-closed，无绕过路径）。公钥由构造器注入（系统构建期烧录可信根公钥）。
5. **dyn 兼容性**：四 trait 保持原生 `async fn in trait`（单实现为主），不能 `Box<dyn>`；仅 `CveCallback` 用 `#[async_trait]` 可 dyn（ADR-COMPAT-001）。
6. **CVE 数据源选型**：选 OSV（聚合 NVD/PyPI/RustSec/GHSA）而非直连 NVD——OSV schema 统一、匿名可用、无 API key 配额门槛，更适合开源 OS。精确 CVSS→severity 映射留 TODO（当前按 summary 文本保守推断）。
7. **依赖注入**：`AbUpdateEngine`/`NvdCveMonitor` 提供 `with_staging_dir`/`with_api_url`/`with_client` builder，便于 fixture 测试把请求指向本地 TcpListener 极简 HTTP 服务器（零依赖，避免引入 wiremock/httpmock）。

## 下一步（后续批次，依赖就绪后）
1. `download` 断点续传：支持 Range 请求 + 已下载字节续传（当前 `real::download_to_file` 留 TODO，基础下载已真实）。
2. `activate_slot`：对接 bootloader（grub-bls/systemd-boot）+ ostree 写槽。
3. `execute`（滚动）：对接 os-meta leader 选举（Consensus::get_members/get_leader）+ 单节点 OTA 编排。
4. `verify_current_health`：对接健康探针（systemd is-system-running / RPC）。
5. OSV severity：精确 CVSS→级别映射（替换 summary 文本推断）。

