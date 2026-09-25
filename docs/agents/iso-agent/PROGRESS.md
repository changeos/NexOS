# iso-agent 进度日志

## 当前状态
- 阶段：实现中（批 2 骨架就绪，真实工具链执行待沙箱）
- 最后更新：2026-08-05

## 已完成
- [x] **XorrisoIsoBuilder 骨架**（`crates/os-iso/src/impl_iso.rs`）
  - `impl IsoBuilder`：`build`（校验 spec → 派生命令参数 → 注册任务 → 推进 Building{squashfs}）
  - `status`（任务状态机查询：Pending/Building/Completed/Failed）
  - `verify`（sha256 比对，占位实现）
  - 命令参数构造为纯函数（`cli.rs`），真实 spawn 留 TODO
- [x] **RustInstaller 骨架**（`crates/os-iso/src/impl_installer.rs`）
  - `impl Installer`：`detect_hardware`（HCL 占位报告 + 告警生成）、`install`（InstallStep 状态机推进 + 校验）
  - KVM 检测纯函数（`detect_kvm_support_from_cpuinfo`：vmx/svm flag）
  - 真实裸机写盘/建池留 TODO
- [x] **InstallStep 状态机**（`installer.rs`）：7 步有序枚举 + `next()`/`is_terminal()`/`all_steps()`/`label()`
- [x] **IsoSpec 校验 + 克隆快照敏感项过滤**（`iso.rs`）
  - `IsoSpec::validate`（架构/版本/组件/locale 非空 + 架构白名单）
  - `IsoSpec::sanitize_clone_snapshot` + `filter_sensitive` + `is_sensitive_key` + `SENSITIVE_CONFIG_KEYS`（§3.19 排除清单：password/token/api_key/private_key/mnemonic 等大小写不敏感递归过滤）
- [x] **CLI 命令构造纯函数**（`cli.rs`）
  - `squashfs_pack_args`（mksquashfs：-noappend -comp -b）
  - `xorriso_build_args`（xorriso -as mkisofs：-V/-b/-boot-info/-no-emul-boot/[-eltorito-alt-boot -e]/-o）
  - `sha256sum_args` / `parse_sha256sum_output`（64 位 hex 解析 + 大小写归一）
  - `derive_boot_config`（卷标派生 + ISO 9660 ≤32 字节截断）
- [x] **InstallTarget 校验**（`installer.rs`）：必填字段 + RAID 级别白名单 + RAID 盘数兼容（mirror≥2/raidz1≥3/raidz2≥4/raidz3≥5）
- [x] **HCL 阈值 + 告警生成**（`installer.rs`）：`HclThresholds`（默认 mem 4/8GB、disk 32GB）+ `hcl_warnings`（低内存/无盘/小盘/无网卡/无 KVM）
- [x] **MockIsoBuilder / MockInstaller**（`crates/os-iso/src/mock.rs`，feature `mock`）
  - `MockIsoBuilder`：build 立即标 Completed（确定性产物）、status、verify、错误注入（`with_build_error`/`with_verify_result`/`with_initial_status`）
  - `MockInstaller`：detect_hardware/install 预置报告、admin_user 动态注入首启动作、错误注入（`with_install_error`/`with_hardware`/`with_install_report`/`skip_target_validation`）
- [x] **lib.rs 模块接线 + re-export**（`IsoBuilder`/`Installer`/`XorrisoIsoBuilder`/`RustInstaller`/`MockIsoBuilder`/`MockInstaller` 全公开）
- [x] **测试**：103 测全过（含 mock）
  - CLI 命令形态、sha256 解析、ISO 卷标截断
  - IsoSpec 校验各分支、敏感项过滤递归/大小写/数组
  - InstallStep 状态机全序列、InstallTarget RAID 盘数
  - HCL 告警各路径、KVM vmx/svm/多核/子串不命中
  - XorrisoIsoBuilder build/status/verify/task_command_args
  - RustInstaller detect/install/RAID 各级别
  - MockIsoBuilder/MockInstaller 全行为 + 错误注入

## 进行中
- 无

## 阻塞
- 无（骨架无外部阻塞；真实 xorriso/squashfs 子进程执行与裸机安装属"运行时硬阻塞"，按规格书 §6 留沙箱 TODO）

## DoD 自检
- [x] `IsoBuilder`/`Installer` 有骨架（命令构造 + 数据结构真实，真实 xorriso 执行留 TODO）
- [x] `cargo check -p os-iso --features mock`：0 error
- [x] `cargo test -p os-iso --features mock`：103 passed
- [x] `cargo clippy -p os-iso --all-targets --features mock -- -D warnings`：0 warning
- [x] `cargo doc -p os-iso --features mock --no-deps`：0 warning
- [x] `MockIsoBuilder`/`MockInstaller` 已提交（feature gate `mock`）

## 下一步（待主代理分派）
1. 真实 xorriso/mksquashfs/sha256sum 子进程执行（需沙箱 + 工具链；用 `assert_cmd`/fixture 录制 xorriso 输出做解析测）
2. 真实 `detect_hardware` 硬件探测（读 /proc/cpuinfo、lsblk、/proc/meminfo、/sys/class/net）
3. 真实裸机 `install`（分区/建 ZFS 池/装系统/首启钩子；需嵌套虚拟化沙箱）
4. 等待 update-agent（批 3）消费 MockIsoBuilder 反馈

## 关键决策（落档备查，无新 ADR）
- **trait 签名未改**：IsoBuilder/Installer 保持原生 `async fn in trait`（无 `#[async_trait]`），未走 ADR。下游（update-agent/api-agent）以**具体类型/泛型**注入 mock，**不能** `Box<dyn IsoBuilder>`（呼应 ADR-COMPAT-001：单实现为主，不预设 dyn 派发）。若下游确需 dyn，须走 ADR + 会签。
- **无新 crate**：xorriso/squashfs 编排用 `tokio::process::Command`（workspace 已有 tokio），无新依赖。
- **新增依赖**：仅 `tracing`（已在 workspace.dependencies），用于 build/install 流程日志。
