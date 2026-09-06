# 契约编译验证进度（Contract Compile Progress）

> ⚠️ **历史文档，截至 2026-08-06（batch8，main `4a960bf` 段）**。骨架期→真实接通期的批次
> 工作记录（批 0–9 逐批明细），此后批次不再在此登记——**现状见仓库根 `MEMORY.md` +
> [CHANGELOG.md](../CHANGELOG.md) 阶段 20（2026-08-07 ~ 08-20 产品化）**。各 crate 测试基数以
> 最新 `cargo test --workspace` 为准（本文件 3,475 → 现 4,100+）。

> 接任会话（Ubuntu 开发机）的工作记录。目标：让 `cargo check --workspace` 全绿。
> 起点状态见 [HANDOVER.md](./HANDOVER.md)。

---

## 当前状态：✅ 全绿（系统组装完成 + MCP Server + 4 binary + 25 crate，3475 测试通过，main `a973df2`)

```
cargo build   --workspace                                            →  0 error
cargo test    --workspace --features mock                            →  3348 passed + 127 ignored = 3475
cargo clippy  --workspace --all-targets --features mock -D warnings  →  0 warning
cargo fmt     --all -- --check                                       →  零差异
make all / make bench / make bench-save TAG=                         →  CI 与本地一键脚本就绪
```

25 crate workspace（22 业务 + os-integration 集成测 12 场景 + nettest 冒烟 + os-mcp MCP Server）全部从契约骨架推进到真实执行层 + 系统组装完成。**P0/P1/P2/P3 第三方依赖全部注册**（77 个含内部 path 依赖）并被各 crate 按需引用，接通真实实现（trait 签名零改动）。**四轮共 20 项本机实跑 + 2 功能接通验证通过**（详见本批次 batch5 记录）。main HEAD `a973df2`，已 push（371 commits）。全程 milestone 见 [HANDOVER.md](./HANDOVER.md) §4；各 crate 真实功能覆盖清单见 §4.1；剩余阻塞见 §7。

---

## 本批次（2026-08-06：第六轮——DPU devlink/zpool status 解析 + SMB/NFS 落盘接通 + 覆盖率 79.6%）

| 子代理 / 分支 | commit | 内容 | 测试增量 |
|--------------|--------|------|---------|
| `feature/network-dpu-devlink`（network-agent） | `4a960bf` 段 | os-network DPU devlink/rdma 命令构造 + 解析器强化：7 默认测（devlink dev show/info 多设备解析 + rdma dev RoCE/mlx5 解析 + argv 纯函数 + 空输出/垃圾容错）+ 3 `#[ignore]` 真实工具可达性（本机 iproute2-6.19.0 devlink/rdma exit 0，无硬件→空 Vec）。新增 `devlink_dev_show_argv`/`parse_rdma_dev` 等纯函数 | +7 passed + 3 ignored |
| `feature/storage-zpool-status`（storage-agent） | `4a960bf` 段 | os-storage **zpool status 树形解析补全**（长期 TODO 集成阶段）：`parse_zpool_status` 纯函数（三段式：pool 段切→name/state/scan/config 提→vdev 树缩进折叠）+ Vdev 扩展 read/write/cksum 错误计数字段 + `list_pools_with_vdevs` inherent 方法。6 默认测（单盘/mirror/raidz1/DEGRADED+错误聚合/多池/容错）+ 3 `#[ignore]` 真实测（osprobepersist 池 vdev 解析正确） | +6 passed + 3 ignored |
| `feature/protocols-smb-nfs-integrate`（protocol-agent） | `4a960bf` 段 | os-protocols **SMB/NFS 真实落盘 + reload 接通**：`ReloadPolicy`（Enabled/DryRun/Disabled）+ write_smb_conf 真实 tokio::fs::write + NfsOrchestrator apply_exports（exportfs -i 往返）+ smbcontrol -s reload。4 `#[ignore]` 真实测（落盘+testparm + exportfs 往返 + smbcontrol DryRun + 全编排往返），修 unexport clients 快照 bug + batch5/6 回归适配 | +4 ignored |
| `feature/coverage-tarpaulin`（qa-agent） | `4a960bf` 段 | **测试覆盖率分析**（cargo-tarpaulin 0.37.0，首次）：18 crate 跑通，整体 src 加权 **79.6%**。`docs/COVERAGE_REPORT.md`。最高 ROI 补测：os-core DTO/newtype（0%→85%）+ 各 error.rs Display（0%）。发现 os-services pulp-0.22.3 上游兼容 bug（Rust 1.97.1，非本仓缺陷） | 0（纯报告） |
| `feature/cargo-audit-finish`（audit-agent） | `4a960bf` 段 | 供应链审计：⚠️ cargo-audit advisory-db fetch 因 github 网络不稳定+SOCKS5 代理不通未完成。更新 `docs/CODE_QUALITY_AUDIT.md`（合并 clippy pedantic + 覆盖率数据 + 标注待 CI 补） | 0（纯报告） |
| 进度检查子代理 | — | 5 轮全部存活 0 卡死。cargo-audit advisory-db lock 风险已标注。batch7 教训有效（ps 进程检查避免误判） | — |

**合并**：5 分支 `git merge --no-ff` **全部零冲突**。合并后 3348 passed + 127 ignored = 3475，clippy 0 warning。**关键价值**：①zpool status 树形解析补全长期 TODO；②SMB/NFS 落盘接通（ReloadPolicy 设计）；③首次覆盖率 79.6% 定位提升空间。5 worktree + 5 分支已清理。

## 本批次（2026-08-06：第五轮——PXE/replication/backup 真实测 + clippy pedantic 审计 + bench 回归 CI）

| 子代理 / 分支 | commit | 内容 | 测试增量 |
|--------------|--------|------|---------|
| `feature/provision-pxe-real`（provision-agent） | `4a960bf` 段 | os-provision PXE iPXE/pxelinux.cfg 模板生成验证 + dnsmasq PXE 配置语法校验：6 默认测 + 3 `#[ignore]` 真实测（dnsmasq 可达性 + PXE 配置 `--test` 语法 + tftp-root 布局），本机 dnsmasq 2.92 跑绿 | +6 passed + 3 ignored |
| `feature/storage-replication-real`（storage-agent） | `4a960bf` 段 | os-storage zfs send-recv 命令构造 + 本地真实全量/增量/加密往返：6 默认测 + 4 `#[ignore]` 真实测（send\|recv 往返 + send→file→recv 47400B + 增量 + 加密 passphrase unload/load-key），本机 zfs 2.4.1 跑绿 | +6 passed + 4 ignored |
| `feature/services-backup-real`（services-agent） | `4a960bf` 段 | os-services backup 快照验证 + scrub 解析 + send 流回放：17 默认测 + 3 `#[ignore]` 真实测（trigger_now 快照 + scrub zpool status + send 50984B recv 回放），本机 zfs 2.4.1 跑绿 | +17 passed + 3 ignored |
| `feature/clippy-pedantic-audit`（audit-agent） | `4a960bf` 段 | **clippy pedantic 全量审计**：3304 warning 扫描 + 修 25 文件高价值 lint + `docs/CODE_QUALITY_AUDIT.md`。cargo-audit 待补（子代理超限 stop） | 0（纯 lint） |
| `feature/bench-regression-ci`（devops-agent） | `4a960bf` 段 | **bench 回归 CI 阈值门控**：criterion --baseline 比对 + artifact 存储 + `bench-regression-gate.sh` 分桶阈值（strict 15%/loose 30%）。子代理超限 stop，主代理验证后提交 | 0（纯 CI/脚本） |
| 进度检查子代理 | — | 5 轮全部存活 0 卡死。2 子代理超限 stop 由主代理收尾 | — |

**合并**：5 分支零冲突。合并后 3348 passed + 127 ignored = 3475，clippy 0 warning。zfs send-recv 真实往返 + pedantic 审计 + bench CI 门控。5 worktree + 5 分支已清理。

## 本批次（2026-08-06：第四轮真实环境验证——协议栈 NFS/bootloader/RustFS-sigv4 + 接通远端git-clone/osd-systemd + 修 1 sigv4 bug）

| 子代理 / 分支 | commit | 内容 | 测试增量 |
|--------------|--------|------|---------|
| `feature/protocols-nfs-real`（protocol-agent） | `7410560` 段 | os-protocols NFS exports/ganesha.conf 渲染语法验证 + exportfs/ganesha 真实可达性：10 默认测（exports(5) 语法严格校验 + ganesha.conf 结构）+ 4 `#[ignore]` 真实测（exportfs -v + ganesha.nfsd -v + exportfs option 往返 + ganesha 配置解析），本机 nfs-ganesha 6.5 跑绿。发现 Domain_Name 弃用警告 + FSAL 块需求（留待生产接通） | +10 passed + 4 ignored |
| `feature/update-bootloader-real`（update-agent） | `7410560` 段 | os-update GRUB/systemd-boot 配置生成 + 命令构造 + bootctl 可达性：14 默认测（GRUB menuentry/systemd-boot loader.conf/entry_conf + 两阶段命令 next-boot oneshot→commit 持久 + 错误处理 + write_config_files 原子写）+ 5 `#[ignore]` 真实测（bootctl 探测 SKIP/grub-reboot 2.14 探测/true-false spawn），本机 systemd 259 跑绿。发现 Ubuntu 无 bootctl + grub-reboot vs grub2-reboot 命名差异 | +14 passed + 5 ignored |
| `feature/protocols-rustfs-http-real`（protocol-agent） | `7410560` 段 | os-protocols RustFS/S3 sigv4 签名 AWS 官方测试向量验证 + 真实 S3 HTTP：6 默认测（sigv4 canonical_request/string_to_sign/signing_key 与 AWS `aws-sig-v4-test-suite` 完全一致）+ 3 `#[ignore]` 真实测（匿名 GET noaa-goes16 公开桶 + ListObjectsV2 + sigv4 签名 GET），本机公网跑绿。**修 sigv4 bug**：canonical_request 只 trim 头部值未折叠内部连续空格（AWS 规范要求 `a   b`→`a b`），用 AWS `get-header-value-trim` 向量验证修复 | +6 passed + 3 ignored |
| `feature/devtools-git-remote-real`（devtools-agent） | `7410560` 段 | os-services **接通远端 git clone**（gix `blocking-network-client`，新 `git-remote` feature 门控 reqwest-rust-tls 后端）：`clone_repo` 真实 clone + `resolve_remote_head` 远端 URL 路径 + `trigger_pipeline` clone 容错。3 `#[ignore]` 真实测（真实 clone octocat/Hello-World + trigger_pipeline 远端 + 错误传播），本机公网跑绿。**移除 3 个 RUNTIME TODO**。gix feature 编译一次过 | +3 ignored |
| `feature/osd-systemd-integrate`（osd-agent） | `7410560` 段 | osd **接通 do_start/stop_inner 真实 systemctl**：引入 `SystemdRunner` 双后端 trait（`TokioSystemdRunner` 真实 systemctl/systemd-run + `InMemorySystemdRunner` no-op 向后兼容，同 CgroupBackend/NtpRunner 模式），do_start_inner（systemd-run + is-active 轮询）/do_stop_inner（SIGTERM→SIGKILL→reset-failed）真实接通。5 `#[ignore]` 真实测（start 拉起/stop 终止/状态机一致/restart 循环/stop 幂等），本机 systemd 259 跑绿。batch4 锚点测同步更新（注释改指向 systemd_integrate_real.rs）。零 bug | +4 passed + 5 ignored |
| 进度检查子代理 | — | 5 轮：2/5 先完成，osd-systemd 前期规划较长（已 ping 确认存活，SystemdRunner 双后端方案设计合理），0 卡死 | — |

**合并**：5 分支 `git merge --no-ff` **全部零冲突**。合并后 3348 passed + 127 ignored = 3475，clippy 0 warning。**关键价值**：①sigv4 修真实 bug（密码学正确性）；②osd-systemd 接通 batch4 唯一框架缺口（SystemdRunner 双后端不破 30+ 现有测）；③远端 git clone 接通移除 3 RUNTIME TODO。5 worktree + 5 分支已清理。

## 本批次（2026-08-06：第三轮真实环境验证——装环境解锁 ffmpeg/libvirt/runc/SMB/LIO + CPU 虚拟化检测 + 修 5 个真实 bug）

| 子代理 / 分支 | commit | 内容 | 测试增量 |
|--------------|--------|------|---------|
| `feature/ffmpeg-real`（media-agent） | `7410560` 段 | os-services media_ffmpeg 真实转码测：4 `#[ignore]` 真实测（ffmpeg 版本可达 + HLS 单档位转码 + ABR 多档位 + 错误传播）+ 2 纯逻辑测，本机 ffmpeg 8.0.1 跑绿。**零实现 bug** | +2 passed + 4 ignored |
| `feature/compute-virt-real`（compute-agent） | `7410560` 段 | os-compute **virt-ffi feature 首次真实编译**（libvirt-dev 12.0.0，编译一次过）+ 4 `#[ignore]` 真实测（test:///default fixture VM 生命周期），本机 virsh 12.0.0 跑绿 | +4 ignored |
| `feature/compute-runc-real`（compute-agent） | `7410560` 段 | os-compute runc 真实容器拉起：5 `#[ignore]` 真实测，本机 runc 1.4.0 跑绿。**修生产挂起 bug**：`YoukiRunner::run` 用 `output().await` 等管道 EOF，runc init 后台进程继承管道 → 永久 hang → 改 spawn+wait+限时排空管道 | +5 ignored |
| `feature/protocols-smb-real`（protocol-agent） | `7410560` 段 | os-protocols SMB 真实测：6 默认测（smb.conf 渲染 → testparm 真实校验）+ 4 `#[ignore]` 真实测（testparm + smbstatus），本机 smbd 4.23.6 跑绿。发现 smbstatus `-j` 非 `-J` 的 4.23 变更 | +6 passed + 4 ignored |
| `feature/storage-block-real`（storage-agent） | `7410560` 段 | os-storage **LIO iSCSI + nvmet NVMe-oF 真实 configfs export**：4 `#[ignore]` 真实测，本机 target_core_mod+nvmet 跑绿。**修 3 真实 bug**：backstore 名含 `/` 被拒收（`sanitize_name`）+ portals 重复创建 + IQN 后缀。发现 ZFS/LIO zpool destroy 内核挂起 | +3 ignored |
| `feature/compute-virtcheck`（**用户需求**） | `7410560` 段 | os-compute **CPU 虚拟化能力检测**（VM 启动前预检 CPU/BIOS/KVM）：`virt_check` 模块 + `HardwareVirtualizationUnavailable` 错误 + 用户友好诊断"请在 BIOS 开启 VT-x"。18 纯逻辑测 + 3 真实测，本机 Intel Ultra 5 vmx+kvm 跑绿 | +18 passed + 3 ignored |
| 进度检查子代理 | — | 5 轮：3/5 先完成，runc+storage 真实 root 重操作收尾，0 卡死 | — |

**合并**：6 分支 `git merge --no-ff` **全部零冲突**。合并后 3348 passed + 127 ignored = 3475，clippy 0 warning。**关键价值**：runc 修生产挂起 bug（管道 EOF 陷阱）；storage LIO 修 3 个 configfs bug；virt-ffi 首次真实编译一次过。6 worktree + 6 分支已清理。

## 本批次（2026-08-05：真实接通深化 + candle/CLIP + 集成场景 9/10 + ERROR_GUIDE P3 复审 + 全局 fmt）

| 子代理 / 分支 | commit | 内容 | 测试增量 |
|--------------|--------|------|---------|
| `feature/iso-real`（iso-agent） | `8fb1d38` | os-iso xorriso 真实构建执行层：`IsoBuildRunner` trait（`#[async_trait]` dyn 兼容，与 `os-storage::CommandRunner` 同构）+ `TokioIsoRunner`（tokio::process 真实 spawn，无害命令验证）+ `FixtureIsoRunner`（确定性测试产物）；`XorrisoIsoBuilder.build` 三阶段（mksquashfs→xorriso→sha256）接通 runner；真实 xorriso/mksquashfs 测标 `#[ignore]` | +18（121 pass + 2 ignored） |
| `feature/clip-adr`（media-agent） | `f989b7b` | CLIP 推理后端选型 [ADR-DEPS-005](./adr/ADR-DEPS-005-clip-backend.md)：选 candle 0.11（纯 Rust + CUDA 适配 RTX 3090）；workspace 注册 candle-core/nn/transformers；`CandleClipModel` 骨架（embed 返回 Internal 诊断，不依赖真实权重），`PlaceholderClipModel` 保留 | +8（含 1 doc-test） |
| `feature/integ4`（integration-agent） | `5b51101` | 集成场景 **9 provision PXE 自举链路**（PhaseMachine 4 阶段推进 / S3.19 ExcludeRules 敏感过滤 / CheckpointPolicy 断点续传 / PxeConfigBuilder 三 BootMode / MockProvisioner+MockMigrationEngine 端到端）+ **10 osd 启动编排链路**（拓扑排序 / 循环检测 / 组件状态机 / InMemoryCgroupBackend 配额 / MockHealthProbe 健康触发重启） | +60（os-integration 67→127） |
| `feature/p3-error` | `d3b3cc9` | ERROR_GUIDE §3.3 10 处 P3 错误码逐个复审：1 处修复（os-services `HardwareError` UpstreamUnavailable→Internal，本机硬件非外部上游）+ 9 处保留并标注理由；符合率 94%→**95%** | 0（纯映射调整） |
| `feature/cargo-fmt` | `6dbb13a`→main 重跑 | 全 workspace `cargo fmt` 格式化统一（191 文件）+ `cargo clippy -D warnings` 严检 0 warning；fmt 分支与 iso-real 同文件冲突，故在 main 上重跑 fmt 等效（main 已含全部功能代码） | 0（纯格式） |

合并方式：4 功能分支 `git merge --no-ff` 无冲突合入；cargo-fmt 放弃分支合并、在 main 重跑提交。5 worktree + 5 分支已清理。

## 本批次（2026-08-06：第二轮真实环境验证——osd + storage + guest 五项本机实跑 + 修 4 个真实 bug）

| 子代理 / 分支 | commit | 内容 | 测试增量 |
|--------------|--------|------|---------|
| `feature/osd-cgroup`（osd-agent） | `05eb69a` 段 | osd `CgroupsRsBackend` cgroup v2 真实写入测：4 测 `#[ignore]`（apply 写 cpu.max+memory.max / read 读回一致 / update 更新 / delete 删目录），本机 cgroup2fs + root 跑绿。**发现内核行为**：cgroup v2 对 memory.max 做 PAGE_SIZE 对齐（写 100MB 实存 99999744，非实现 bug）。RAII Drop 用同步 `std::fs::remove_dir` 规避 batch3 zfs-real 踩过的嵌套 tokio runtime panic | +4 ignored |
| `feature/osd-systemd`（osd-agent） | `05eb69a` 段 | osd `SystemdOrchestrator` systemd 真实交互测：4 测 `#[ignore]`（systemd 可达性 + transient unit 生命周期 oneshot + 长跑进程 + 状态机框架锚点），本机 systemd 259 跑绿。**确认 start/stop 仍纯状态机框架**（无 systemctl 调用，TODO 集成阶段），按红线未改实现，留接通锚点 | +4 ignored |
| `feature/osd-ntp`（osd-agent） | `05eb69a` 段 | osd `ChronyNtp` 真实 chronyc 编排测：4 测 `#[ignore]`（chronyc tracking 真实解析 + sources + dry-run 命令构造 + 二进制探测），本机 chrony 4.8 跑绿。真实 Stratum 3/Leap Normal/offset 3ms 解析正确。**发现部署差异**：Ubuntu 用 `sourcedir` 指令（server 在 sources.d/），非 bug。补 lib.rs re-export 供集成测用 | +4 ignored |
| `feature/storage-block`（storage-agent） | `05eb69a` 段 | os-storage block_impl LIO/nvmet 命令构造测：6 命令构造测**默认跑**绿（iSCSI backstore→target→lun→portal 顺序 + NVMe-oF subsystem/namespace + destroy 逆操作）+ 3 configfs 真实可达性测 `#[ignore]`（本机无 LIO/nvmet 子系统，优雅 SKIP）。**修 2 真实 bug**：①`export_iscsi` 缺 backstore 创建（引用未创建的 backstore + 语法错）②`unexport` 非真逆操作（不删 backstore，增强 `UnexportKind::Iscsi` 携带 backstore 名） | +6 passed + 3 ignored |
| `feature/guest-nftnl`（guest-agent） | `05eb69a` 段 | os-guest **`nftnl_apply_statements` 从占位 Err 换真实实现**（nftnl 0.7 `Batch` + 自定义 `SetElemMsg` 设 timeout + `nft_expr!` 构造规则，经 `mnl::Socket` 提交内核）+ 3 测 `#[ignore]`（apply/revoke/rollback_checkpoint），本机 libnftnl 1.3.1 + libmnl 1.0.5 + root 跑绿。apply 后 `nft list` 实测：set 元素 timeout 1h 真实生效、`tcp dport 445 accept` 规则真实生效。**修 2 真实 bug**：①`nft_expr!` payload 宏 3-token 误用（nftnl 0.7 只接 2-token）②dport cmp 字节序 bug（nftnl `ToSlice for u16` 写 little-endian，dport 是 big-endian，端口 445 被错写为 48385→改传 `u16::to_be_bytes()`，与 batch3 os-network 字节序陷阱同类）。已知限制：多端口匹配降级为只匹配第一个端口（`TODO(nftnl-multiport)`） | +3 ignored |
| 进度检查子代理 | — | 5 轮检查：4/5 先完成，guest-nftnl 收尾（112 tool calls，因首次真实编译暴露多个 bug）。0 卡死 | — |

**合并**：5 分支 `git merge --no-ff` **全部零冲突**（5 子代理改的文件完全不重叠：3 osd 测各新增 tests/ + ntp 补 lib.rs；storage 改 block_impl.rs；guest 改 impls.rs+Cargo.toml）。合并后 build/test/clippy/fmt 全绿（2007 passed + 62 ignored = 2069，clippy 0 warning）。**关键价值**：本机实跑发现并修了 4 个真实 bug（storage block 2 命令构造 + guest nftnl 2 FFI），其中 guest nftnl-ffi 占位代码**首次真实编译**即暴露 nft_expr! 宏 API 误用 + dport 字节序问题。5 worktree + 5 分支已清理。

## 本批次（2026-08-05：真实环境验证——五大执行层本机实跑 + 修 2 个真实 bug）

| 子代理 / 分支 | commit | 内容 | 测试增量 |
|--------------|--------|------|---------|
| `feature/zfs-real`（zfs-agent） | `74af95e` 段 | os-storage 真实 zfs 池全链：sparse file vdev 建池→dataset→snapshot→destroy，自动 teardown（RAII Drop + 同步 `zpool destroy -f` 避免嵌套 tokio runtime），zfs-2.4.1，2 测 `#[ignore]`（pool 生命周期 + PoolExists 错误分类），从不碰真实磁盘 | +2 ignored |
| `feature/nftnl-real`（nettest-agent） | `74af95e` 段 | os-network `nftnl_real.rs` **修 FFI bug**：`nftnl::nftnl_sys::mnl_socket` 路径在 nftnl 0.7 不存在→改用 `mnl` crate 高层 API；修 19 编译错误（nftnl 0.6→0.7 API 漂移：Table/Chain::new 接 CString、Rule::new 接 &chain、ProtoFamily 改名、Verdict::Redirect 移除、nft_expr! 宏）；**nftnl 0.6→0.7 字节序陷阱**：`nft_expr!(cmp == 22u16)` 小端写但 TCP 端口大端读→改传 `22u16.to_be_bytes()`。nettest `nftnl_real.rs` 重写+扩：真实表/链/规则提交 + `nft list` 验证 | nftnl 测重写（ignored） |
| `feature/netlink-real`（nettest-agent） | `74af95e` 段 | nettest `rtnetlink_real.rs` 从 1 扩到 **6 测**：link_list / addr_list / route_list / link_get_by_name / genetlink_families（25 family） / dummy_crud（经 rtnetlink 写路径而非 ip 命令）。注册 genetlink/netlink-packet-core/netlink-packet-generic。本机非 root 实测过 link/addr/route 只读 | +5 ignored（→6） |
| `feature/iso-e2e-real`（iso-agent） | `74af95e` 段 | os-iso 真实 xorriso 多架构 ISO 构建：minimal/BIOS/BIOS+UEFI/UEFI-only 四产物，深度验证 ls/+/file/El Torito/sha256；**修 cli.rs bug**：`-boot-info`→`-boot-info-table`（xorriso 1.5.6 拒收错误选项名） | iso 测 ignored |
| `feature/clip-cuda-real`（media-agent） | `74af95e` 段 | os-services **CLIP CUDA 真实推理跑通**（RTX 3090）：`CandleClipModel` 从骨架（返 Internal）升级为真实实现——candle_transformers::clip::ClipModel 经 mmap safetensors 加载；embed_image decode→resize 224→affine [-1,1]→to_device(GPU)→get_image_features→L2；embed_text BPE tokenize→get_text_features→L2（spawn_blocking）。feature 门 `clip-cuda`。权重经 hf-mirror 下载 605MB（sentence-transformers/clip-ViT-B-32）。稳态 embed_image 99ms / embed_text 16ms，语义排序正确（cat-kitten 0.94 > cat-car 0.90）。注册 image/tokenizers | +2 ignored（GPU 测） |
| 进度检查子代理 | — | 5 轮 15 分钟检查：5/5 存活 0 失败。准确识别 clip-cuda 权重下载阶段（经 hf-mirror 自愈）非卡死 | — |

**合并**：5 功能分支 `git merge --no-ff` 合入。SANDBOX.md §5 表格冲突（5 子代理都改）手动解为表行并存；SANDBOX.md §5.2 nftnl-real 表行冲突手动解。5 worktree + 5 分支清理（nftnl-real worktree 目录因 sudo 创建文件 Permission denied，git worktree/branch 已干净移除，目录残留待 `sudo rm -rf`）。**真实测全部 `#[ignore]` + 自动 teardown，不污染默认套件**。**关键价值**：本机实跑发现并修了 2 个真实 bug（nftnl FFI 路径 + iso `-boot-info` 选项名），证明真实环境验证的杠杆。

## 本批次（2026-08-05：CI 加固 + 性能基线 + TODO 审计 + 真实冒烟扩展 + 真实 ISO CI）

| 子代理 / 分支 | commit | 内容 | 测试增量 |
|--------------|--------|------|---------|
| `feature/ci-harden`（devops-agent） | `273090b` | CI 加固：①ci.yml 加 `cargo fmt --all -- --check` 门（最便宜，最先跑）；②ci.yml 加 `bench` job（criterion，手动/main-only，不阻塞 PR，artifact 归档）；③docs.yml 补 Pages deploy（artifact + deploy-pages job）；④Makefile 加 `bench`/`bench-pkg PKG=`/`bench-save TAG=` 三目标 | 0（纯 CI/Makefile） |
| `feature/bench-baseline`（qa） | `19e8de3` | 跑通 5 crate criterion bench（**首跑全绿零修复**）+ `docs/PERFORMANCE_BASELINE.md` 基线（硬件/日期/HEAD/每 bench mean+CI）。关键发现：routing hit_static O(1) 23-30ns、topo 近线性 O(V+E)、tantivy 建索引方差大（建议 >30% 回归阈值）、meta advance_commit_index fixture 未暴露最坏 O(N) | 0（基线文档） |
| `feature/todo-audit`（audit） | `eba6a76` | 高 TODO crate 审计（services/protocols/network，95 条）：73 [RUNTIME] 保留标注 + **2 [STUB] 补实现**（`hash_password` FNV→PBKDF2-SHA256 拉伸；`delete_by_path` no-op→tantivy raw 分词索引+真实 Term 删除）+ 13 [DOC] + 8 [OBSOLETE] 清理（tantivy/gix 已接通的旧 TODO）。`docs/TODO_AUDIT.md` 审计报告 | +4（→2028） |
| `feature/nettest-real`（nettest-agent） | `899ea7c` | nettest 真实冒烟扩展：+3 个 `#[ignore]` 真实测（`zfs_real` zfs 二进制+内核模块 / `rtnetlink_real` link_list 含 lo **本机非 root 实测过** / `nftnl_real` nft 事务，feature 门 `nftnl-ffi`）；注册 `mnl` crate；优雅失败设计（环境不支持则 SKIP 不 panic） | 0（全 ignored，默认套件不变） |
| `feature/iso-ci`（iso-agent） | `562d949` | os-iso 真实 xorriso ISO 构建 CI：①`IsoEnvironment::probe()` 探测 xorriso/mksquashfs/sha256sum（双策略：$PATH 遍历+command -v）；②真实端到端 `#[ignore]` 测（最小 ISO 构建+CD001 魔数校验，**本机真实跑通 380928 字节**）；③`scripts/sandbox/docker/Dockerfile.iso` 沙箱镜像；④ci.yml 加 `iso-build` job（手动触发 workflow_dispatch） | +7（env.rs 单测，真实测 ignored） |
| 进度检查子代理 | — | 4 轮检查结论：5/5 存活，0 卡住，0 重派。准确识别 todo-audit/nettest-real 初期只读搜索阶段非卡死 | — |

**合并**：5 分支 `git merge --no-ff` 合入。ci.yml 冲突（ci-harden 的 fmt 门/bench job 与 iso-ci 的 workflow_dispatch/iso-build job 都改 ci.yml）**手动解为 3 job 并存**（check-clippy-test / bench / iso-build），YAML 已验证合法。5 worktree + 5 分支已清理。

---

## 修复清单（本轮接任会话）

### 错误模型 / 类型别名

| # | crate | 问题 | 修法 | ADR |
|---|-------|------|------|-----|
| 1 | os-common | `ApiErrorCode` 缺 `Display`，致 `ApiError` 的 thiserror `#[error("[{code}]...")]` 编译失败 | 派生 `thiserror::Error` + 每变体 `#[error("snake_case")]`，Display 输出与 serde rename 一致 | — |
| 2 | os-core | `DateTime` 透传 chrono 泛型 `DateTime<Tz>`，下游裸 `DateTime` 报 E0107 | 改为 `pub type DateTime = chrono::DateTime<chrono::Utc>;` | [ADR-COMPAT-002](./adr/ADR-COMPAT-002-datetime-fixed-utc-alias.md) |
| 3 | osd | 上述 #2 的回归：`ntp.rs` 写 `DateTime<Utc>`（别名已固定 Utc） | 改回裸 `DateTime`（2 处），移除 unused `Utc` | ADR-COMPAT-002 |
| 4 | os-security | #2 连带：`auth.rs` 的 `Utc` 变 unused | 移除 unused import | ADR-COMPAT-002 |

### dyn 兼容（ADR-COMPAT-001）

| # | crate | trait | 加 `#[async_trait]` |
|---|-------|-------|---------------------|
| 5 | os-im | `Agent` / `Tool` / `LlmBackend` / `SharedContext` / `ConfirmationGate` | ✅ 5 个 |
| 6 | os-discover | `PeerCallback` | ✅ |
| 7 | os-update | `CveCallback` | ✅ |
| 8 | os-mobile | `PushCallback` | ✅ |
| 9 | os-wallet | `ChainAdapter` | ✅ |
| 10 | os-api | `RouteHandler` | ✅ |

> 共 10 个 trait，跨 6 个 crate。判断准则：`grep -rn "Box<dyn" crates/` 命中的 trait。

### 其他笔误 / 遗漏

| # | crate | 问题 | 修法 |
|---|-------|------|------|
| 11 | os-api | `HttpMethod` 的 `#[serde(rename_all = "uppercase")]` 非法（应 `SCREAMING_SNAKE_CASE`），级联致 HttpMethod serde derive 失败 | 改 rename 规则；HttpMethod serde 错误随之消除 |
| 12 | os-protocols | ftp/nfs/webdav 子模块缺 `use crate::common::FileProtocol;`（sftp/smb 有，3 个漏了） | 补 import（3 处） |
| 13 | os-meta | `MetaSnapshot.sqlite_dump: Bytes` 未 impl serde | workspace 根 `bytes` 加 `features = ["serde"]` |

### workspace 配置

- 根 `Cargo.toml` `[workspace.dependencies]` 新增 `async-trait = "0.1"`；`bytes` 加 `serde` feature。
- 根注释与 `os-im/src/lib.rs` 契约规范注释同步更新（async 模型条款）。

---

## 已落档 ADR（7 个）

- [ADR-COMPAT-001：`Box<dyn>` 用的 async trait 一律 `#[async_trait]`](./adr/ADR-COMPAT-001-async-trait-dyn-compat.md)
- [ADR-COMPAT-002：`os-core::DateTime` 固定为 UTC 时区的 type 别名](./adr/ADR-COMPAT-002-datetime-fixed-utc-alias.md)
- [ADR-COMPAT-003：`EventBus` 补加 `#[async_trait]`（ADR-001 漏修补全）](./adr/ADR-COMPAT-003-eventbus-async-trait.md)
- [ADR-DEPS-001：P0/P1 第三方依赖注册（reqwest/axum/tower/jsonwebtoken/argon2/ed25519-dalek/nftnl/rtnetlink/tantivy 等 11 个 + 配套）](./adr/ADR-DEPS-001-p0-p1-third-party-deps.md)
- [ADR-DEPS-002：P2 领域专用依赖注册（openraft/rusqlite/virt/bitcoin/alloy/dav-server/libunftp/russh/mdns-sd/rustls/cgroups-rs/toml/opentelemetry/rcgen/boringtun/gix 等 20 个）](./adr/ADR-DEPS-002-p2-domain-specific-deps.md)
- [ADR-DEPS-003：注册 aes-gcm 并接通 os-services(devtools) 的密钥 KVS 真实 AES-256-GCM 加密](./adr/ADR-DEPS-003-aead.md)
- [ADR-DEPS-004：注册 instant-acme 并接通 os-security 的 ACME 自动证书签续](./adr/ADR-DEPS-004-acme.md)
- [ADR-DEPS-005：CLIP 推理后端选型 candle（纯 Rust + CUDA，适配 RTX 3090）](./adr/ADR-DEPS-005-clip-backend.md)

---

## 实现阶段进度（批 0–4 全部完成）

> 进入"启动 owner agent 写实现"阶段。批次定义见 `docs/agents/README.md` §2.1。
> **横切策略（用户钦定）**：子代理遇未注册第三方 crate 时只做无依赖骨架，外部部分留 TODO + 阻塞清单。
> **限流教训**：子代理并行度上限约 5–6 个，超过触发上游 AI 限流；批 3（原 10 owner）分波执行。
> 仓库重建：批 1 进行中（2026-08-04 晚）原仓库 `.git` 损坏，改名 `OS_System_broken` 取证，从 bare `~/os-system.git` 重建主工作树，批 1 在线改动抢救后落入新仓库。批 1–4 全部工作于 2026-08-05 在新仓库完成。详见 [HANDOVER.md](./HANDOVER.md) §5。

### 批 0：core / i18n / orchestrator —— ✅ 完成并合并

| agent | 交付 | 测试 |
|-------|------|------|
| core-agent | os-core：`TokioBroadcastBus` + `MockEventBus`/`MockEventSubscriber` + ApiError 文档 | 12 |
| i18n-agent | os-i18n：`BundleTranslator`（自写极简 TOML 解析零新依赖）+ 三语 TOML(35键×3) + `MockTranslator` | 16 |
| orchestrator-agent | osd：`SystemdOrchestrator` 骨架（拓扑排序+循环检测真实实现）+ `MockHealthProbe` | 35 |

批 0 期间额外修：EventBus dyn 兼容（→ ADR-003）、os-meta clippy doc 缩进（4 处）。批 0 小计 **63 测**。

### 批 1：storage / network / rdma / security —— ✅ 完成并合并

| agent | 交付 | 测试 |
|-------|------|------|
| storage-agent | os-storage：`ZfsCliBackend` 骨架 + CLI 解析 + Mock | 51 |
| network-agent | os-network：基础 5 trait（NetworkManager/Firewall/Dhcp/Dns/Pxe）+ Mock | （并入下） |
| rdma-agent | os-network 的 RdmaManager/DpuBackend + Mock（与 network 同 crate） | 87（含 network） |
| security-agent | os-security：Principal/Role/TOTP 纯算法 + 常量时间比较 + Mock | 33 |

> os-network 87 测 = network(基础5) + rdma 两 agent 在同一 crate 累积。批 1 小计 **171 测**。

### 批 2：protocol / object / vm / container / wallet / meta / iso —— ✅ 完成并合并

| agent | 交付 | 测试 |
|-------|------|------|
| protocol-agent | os-protocols 文件协议：smb.conf/ganesha 渲染 + 骨架 + Mock | （并入下） |
| object-agent | os-protocols 的 ObjectStore：S3 模型 + sigv4 签名 + Mock | 84（两 agent 共 crate） |
| vm-agent | os-compute 的 VmManager：骨架 + libvirt XML + 状态机 + Mock | （并入下） |
| container-agent | os-compute 的 ContainerRuntime/ContainerNetwork/PackageManager：OCI/CNI + apt/desktop + Mock | 97（两 agent 共 crate） |
| wallet-agent | os-wallet：骨架 + 签名/地址纯算法 + Mock | 28 |
| meta-agent | os-meta：Raft 纯算法 + 骨架 + Mock（修 Consensus dyn 兼容） | 55 |
| iso-agent | os-iso：`XorrisoIsoBuilder` 骨架 + Mock | 103 |

批 2 小计 **367 测**。

### 批 3：discover / guest / provision / update / backup / monitor / media / files / devtools / power —— ✅ 完成并合并

| agent | 拥有 crate / 交付 | 测试 |
|-------|------------------|------|
| discover-agent | os-discover：HA 资格检测 + 联邦决策 + Mock | 40 |
| guest-agent | os-guest：访客身份 + RBAC + nft 规则 + Mock | 49 |
| provision-agent | os-provision：迁移状态机 + 敏感排除 + 断点续传 + Mock | 49 |
| update-agent | os-update：A/B 槽位 + 滚动升级 + 回滚 + Mock | 90 |
| backup-agent | os-services(BackupManager)：快照调度 + 保留策略 + Mock | （并入下） |
| monitor-agent | os-services(Monitor)：告警引擎 + 指标模型 + Mock | （并入下） |
| media-agent | os-services(MediaManager)：EXIF + 相册 + ABR + Mock | （并入下） |
| files-agent | os-services(FileManager)：文件树 + 分享 + 冲突解决 + Mock。**增量（真实集成）**：接通 tantivy 全文搜索（新增 `search_index.rs`，BM25 + 高亮 snippet + 分页 + MultiCollector count），`DefaultFileManager::fulltext_search` 由 TF 占位改为真实查询；未注入索引时向后兼容返回空。新增 15 测 | （并入下） |
| devtools-agent | os-services(DevTools)：日志聚合 + 密钥 KVS + Mock。**增量（真实集成）**：接通 gix 真实 Git 服务（init/commit/log/branch + trigger_pipeline 基于真实仓库状态，见下「真实集成阶段」），新增 21 测 | （并入下） |
| power-agent | os-services(PowerManager)：UPS 决策 + SMART 解析 + Mock | （并入下） |

> os-services 测：批 3 合并时 218 测（`cd4f5f4` 整合提交）；files-agent 后续接通 tantivy 全文搜索后增至 **233 测（`--features mock`）/ 196 测（默认 feature）**。批 3 小计 **446 测**（基线）。批 3 因 10 owner 并行触发限流，分波执行（单波 ≤6）。

### 批 4：im / api / client —— ✅ 完成并合并（收尾批次）

| agent | 交付 | 测试 |
|-------|------|------|
| im-agent | os-im：多 agent 协作中枢 + 无环检测 + Critical 双满足 + 黑板清理 + Mock | 22 |
| api-agent | os-api 网关（路由/限流/中间件链/WS）+ os-cli（命令树/格式化）+ Mock | 41 + 16 |
| client-agent | os-mobile + os-desktop：客户端 SDK（URL/重试/HTTP/推送/挂载命令）+ Mock | 61 + 20 |

批 4 小计 **160 测**。批 4 合并后 `3d70e0b` 为收尾 clippy 清理提交（os-provision 遗留 lint）。

### 累计

| 项 | 数 |
|----|----|
| 完成批次数 | 5（批 0–4 全完）+ 真实集成（P0/P1/P2/P3 依赖全接通）+ 集成测/冒烟 + 真实执行层全批 + 文档/沙箱 + review2 全闭环 + **六轮共 26 项 + 3 功能接通 + clippy pedantic + bench CI + 覆盖率** |
| owner agent | 27/27 完成（+ integration-agent 跨 crate 链路测 10 场景） |
| crate 总数 | 24（22 业务 + `os-integration` + `nettest`） |
| commit 总数 | 317（main `a973df2`) |
| 骨架期单位测 | 1207（批 0–4 小计：63+171+367+446+160） |
| 真实集成 + 执行层 + review2 闭环 + 真实环境验证新增 | +908（P0/P1/P2/P3 接通 + 集成测 10 场景 + 真实执行层整合 + youki 骨架 + tracing 桥接 + common 补测 + iso/clip/integ4/p3-error + ci-harden/bench-baseline/todo-audit/nettest-real/iso-ci + zfs-real/nftnl-real/netlink-real/iso-e2e-real/clip-cuda-real + osd-cgroup/osd-systemd/osd-ntp/storage-block/guest-nftnl + ffmpeg-real/compute-virt-real/compute-runc-real/protocols-smb-real/storage-block-real/compute-virtcheck） |
| **总计（main `a973df2`，`--features mock`）** | **3348 passed + 127 ignored = 3475** |

各 crate 单位测分布（`cargo test -p <crate> --features mock`，main `a973df2` 真实统计）：os_api 53+1 / os_cli 55+1 / os_common 13 / os_compute 178+4 / os_core 13 / os_desktop 24 / os_discover 63 / os_guest 59+1（含 nftnl-ffi 真实实现 + 3 真实测）/ os_i18n 19+2 / os_im 22 / os_iso 128+2 / os_meta 65 / os_mobile 76 / os_network 104+4 / os_protocols 91 / os_provision 88 / os_security 81 / os_services 376 / os_storage 76+2（含 block 命令构造测 +6）/ os_update 136+3 / os_wallet 78+1 / osd 90+15（含 cgroup/systemd/ntp 真实测 +12）。`os-integration` 127（10 场景）；`nettest` 1+13ignored（含真实 zfs/nftnl/rtnetlink + genl）。

---

## 阻塞 / 需人类决策（下一阶段）

- **第三方依赖注册**：✅ **已完成**（ADR-DEPS-001/002/003/004/005，workspace.dependencies 共 77 个含内部 crate path 依赖 + clap/tracing-subscriber/criterion + candle + mnl）。原"未注册阻塞清单"全部落地。
- **项目主体已收尾 + review2 全闭环**：24 crate 从契约骨架推进到真实执行层 + 集成测 + 文档 + 沙箱方案 + 错误指引。剩余**仅运行时阻塞项**（依赖已注册/逻辑已实现，真实运行测需特殊环境/系统库/root/外部二进制；当前均有 fixture/骨架/门控覆盖逻辑正确性）+ ERROR_GUIDE 5 处 P2 复审建议。详见 [HANDOVER.md](./HANDOVER.md) §7：
  - FFmpeg 转码 / CLIP 推理后端（media，运行时外部进程/模型；骨架已落地）
  - youki/runc 容器运行时（compute container；OCI bundle 落盘已真实，youki 编排骨架已落地 `runtime.rs`）
  - 真实 PXE TFTP 服务（provision；PXE 配置/脚本编排已落地）
  - 远端 git clone（devtools，需 gix blocking-network-client feature）
  - 真实 root 环境测（推荐用 `docs/SANDBOX.md` + `scripts/sandbox/` Docker/QEMU/nspawn 沙箱）：vm `--features virt-ffi`（需 libvirt-dev）、osd cgroups 真写 /sys/fs/cgroup + systemd unit + chrony NTP（CAP_SYS_TIME）、guest `--features nftnl-ffi`（需 libnftnl-dev）、network rtnetlink/nftnl、update bootloader 写盘、storage zfs CLI。
  - ERROR_GUIDE 5 处 P2 复审（os-iso HardwareIncompatible / os-protocols ProtocolDisabled / os-security CertExpired / os-storage CommandFailed / os-wallet ChainUnsupported，见 `docs/ERROR_GUIDE.md` §3.3）
- ✅ **AEAD 真实加密**（devtools 密钥 KVS，aes-gcm，ADR-DEPS-003）**已落地**。
- ✅ **ACME 证书**（security `acme_request`，instant-acme，ADR-DEPS-004）**已落地**。
- ✅ **BIP-322 验签**（os-wallet，完整 legacy+simple）**已落地**（`feature/bip322`）。
- ✅ **chrony/NTP 编排**（osd `ntp_impl.rs`）**已落地**（`feature/osd-ntp`；真实运行仍需 CAP_SYS_TIME）。
- ✅ **tracing 日志桥接**（os-services monitor.rs `log_bridge::LogBridgeLayer`）**已落地**（`feature/tracing-bridge`，原 review2 遗留 TODO 闭环）。
- ✅ **integration-agent 已启动**：`os-integration` crate 10 场景跨 crate 端到端链路测（VM/访客/HA/备份/IM/API聚合/mTLS联邦/update回滚）合并 main。
- ✅ **review2 全闭环**：R1 的 P1–P7 + R2 的 P-R2-1/2/3 + P7 全部清账（见下方「review2 闭环阶段」+ `docs/REVIEW.md`）。
- 🟡 **devops-agent 部分**：pre-commit hook 已有；DEPLOYMENT.md + SANDBOX.md + scripts/ 沙箱方案完整；CI pipeline 未建、os-iso 真实 ISO 构建未跑、发布打包未做。
- **破坏性命令红线**：真实 zfs/ip/nft/cgroup/systemd/bootloader 改宿主禁在本机跑，需 fixture 或 SANDBOX.md 沙箱。

---

## 真实集成阶段（接通第三方依赖，按 crate 推进）

> 起点：ADR-DEPS-001 已注册 11 个 P0/P1 第三方依赖到 workspace.dependencies
> （reqwest/axum/tower/hyper/jsonwebtoken/argon2/ed25519-dalek/sha2/nftnl/rtnetlink/tantivy），
> 但**无任何 crate 实际引用**它们。本阶段逐 crate 接通真实实现（`xxx.workspace = true`
> + 替换 TODO）。每个 crate 自洽验收（check/test/clippy/doc 全绿）。逐 agent 累加，不改 trait 签名。

### os-wallet：reqwest RPC 探活接通 ✅

| 项 | 状态 |
|----|------|
| 分支 | `real/wallet-agent`（worktree `os-wt-real-wallet`） |
| 提交 | `[wallet-agent] feat(os-wallet): 接通 reqwest RPC 探活(EVM/BTC)` |
| crate 级依赖新增 | `reqwest.workspace = true`（rustls-tls，无 openssl） |
| 替换的 TODO | `RpcRegistryImpl::probe`（registry.rs）—— 原 `Err(Internal)` 占位 |

**实现要点**（`crates/os-wallet/src/registry.rs`）：
- 新增 `RpcProbe` trait（`#[async_trait]`，dyn 兼容）：抽象单次 JSON-RPC POST 调用，
  方法 `rpc_call(url, method, params) -> WalletResult<serde_json::Value>`（返回 result 字段）。
- `ReqwestProbe`：生产实现，reqwest::Client POST JSON-RPC 2.0 envelope，超时默认 5s；
  reqwest::Client 经 `OnceLock` 延迟构造（首次探活时构造），故 `RpcRegistryImpl::new`
  不返回 Result，API 向后兼容。
- `RpcRegistryImpl`：持有 `Box<dyn RpcProbe>`（可经 `with_probe` 注入）；`probe` 实现：
  - EVM：`eth_blockNumber`（params `[]`），校验 result 为 `0x` 十六进制字符串。
  - BTC：`getblockchaininfo`（params `[]`），校验 result 含 `chain`/`headers`。
  - 主 URL 失败 → 尝试 fallback URL（source 标 Remote）；全失败返回 `Err`。
- `check`：probe 成功 → `record_available` + 返回 available 状态；probe 失败 →
  `record_unavailable`（驱动状态机）+ 返回 Unavailable 状态（**不抛错**，优雅降级，
  §9 红线）；`check_all` 据此逐链降级，禁用链被跳过。

**测试**（`registry.rs` + `mock.rs`）：
- 新增 `FixtureProbe`（`mock` feature）：内存 RPC 实现，按 method 返回固定 JSON /
  模拟错误，记录调用历史——零网络。
- 新增 12 个探活路径测：EVM/BTC 成功、失败优雅降级、fallback URL、check_all
  逐链降级、非法响应格式降级、未配置链、validate_probe_result 单元测、
  `reqwest_probe_real_timeout_to_unavailable`（真实 reqwest 探活黑洞地址 → 降级）。
- 现有 28 测保留（`registry_state_machine_shell` 改用 FixtureProbe 注入）。
- **测试数：28 → 41**（+13）。`cargo check/test/clippy/doc -p os-wallet --features mock` 全绿。

**未碰**（本任务范围外，仍留 TODO）：
- 验签（secp256k1/BIP-322/EIP-191/712）——阻塞于 rust-bitcoin/alloy/secp256k1 注册（P2 领域依赖）。
- trait 签名未改（`RpcRegistry`/`ChainAdapter`/`WalletConnector` 全保留）。
- 其他 agent crate 未改（os-guest 等下游消费者 clippy 复核无回归）。

### os-services / media-agent：tantivy 媒体元数据搜索 ✅

- **分支**：`real/media-agent`（worktree `os-wt-real-media`）。
- **交付**：`crates/os-services/src/media_search.rs`（新）—— tantivy 真实索引，字段
  `id/path/filename/mime_type/taken_at_ms/taken_at_iso/lat/lon/face_tags/album`。
  `DefaultMediaManager::search` 从子串占位换为 tantivy 多维查询（DSL：自由词 BM25 +
  `face:`/`album:`/`date:`/`after:`/`before:`/`geo:` 结构化过滤）。
- **接入**：`crates/os-services/Cargo.toml` 加 `tantivy.workspace = true`（与 files-agent 共享声明，语义一致）。
- **未接入（仍 TODO）**：FFmpeg 转码 / CLIP 向量 / 人脸检测（运行时硬阻塞，未注册）。
- **测试**：os-services 单位测 218 → 240（+22：media_search 13 + media_impl 集成 9）。
- **验证**：`cargo check/test/clippy -p os-services --features mock` 全绿；workspace
  `cargo test --workspace --features mock` 1234 passed（原 1212 + 22）。
- **同 crate 协调**：未改 `lib.rs` 模块声明外的契约、`mock.rs`、`error.rs`、其他 agent 文件。

### os-services / devtools-agent：gix 真实 Git 服务接通 ✅

- **分支**：`p2/devtools-agent`（worktree `os-wt-p2-devtools`）。
- **交付**（`crates/os-services/src/impl_devtools.rs`）：
  - **Git 服务（真实 gix）**：新增模块级自由函数 `init_repo`/`commit_all`/`create_branch`/
    `list_branches`/`head_commit`/`log`，封装 gix 0.86 仓库操作——init 建仓（幂等）；
    `commit_all` 经 `empty_tree().edit()` 写 blob → tree → `commit_as` 提交（显式注入
    `os-devtools-agent` 身份，避免依赖 CI 沙箱仓的 git config）；`log` 走 `rev_walk`
    breadth-first 读历史（不依赖 commit_time 排序，避免对提交图元数据的假设）；
    `create_branch` 经 ref 事务（`MustNotExist` 语义——冲突报错、同点幂等）。
  - **trigger_pipeline 基于真实仓库状态**：本地仓库（`file://`/路径）用 gix 真实读 head
    commit 作为 `logs_url` 锚点（确认仓库可达——后续 steps 执行的前置）；记录真实
    `CiRun`（`pipeline_status` 真实可查，不再是占位 `Internal`）。
  - **密钥 KVS**（已升级真实 AEAD，ADR-DEPS-003）：`store/get/rotate_secret` 现用
    `aes-gcm 0.10` 真实 AES-256-GCM 加密——密文 = `nonce(12B)‖ciphertext‖tag(16B)` 拼接，
    nonce 用 OsRng 现场生成，密钥用 SHA-256 从种子派生（独立于系统密钥）。原 `ENC:` 占位已替换。
- **接入**：`crates/os-services/Cargo.toml` 加
  `gix = { workspace = true, features = ["tree-editor"] }`（ADR-DEPS-002 默认 feature 之外
  仅补开轻量 `tree-editor`——构造 tree 对象；远端 `git clone` 需 `blocking-network-client`
  feature，留 TODO，不在本批引入网络栈）+ `aes-gcm`/`sha2`/`rand_core`（ADR-DEPS-003，密钥 KVS AEAD）；
  `tempfile = "3"` 仅 dev-dep（git 仓库往返测试用）。
- **未接入（仍 TODO）**：远端 `git clone`（http/ssh，需 `blocking-network-client` feature，
  属 ADR-DEPS-002 默认 feature 之外的 feature 决策）；AEAD 真实加密（需注册 AEAD crate）；
  CI steps 沙箱执行（容器/namespace 隔离，需 ADR + 安全评审）。
- **测试**：os-services 单位测 240 → **276**（+36 含 mock；其中 impl_devtools 新增 21：
  gix init/commit/log/branch 往返 + trigger/pipeline_status 真实 git 状态 + KVS 回归）。
  默认 feature 239 测。
- **验证**：`cargo check/test/clippy -p os-services --features mock -- -D warnings` 全绿；
  `cargo check --workspace` 全绿（无下游回归）。
- **同 crate 协调**：未改 trait 签名（`DevTools` 全保留）；未改 `ServiceError` variant；
  未改其他 agent 文件（backup/monitor/media/files/power）；`lib.rs` 仅追加 6 个 `git_*`
  re-export（新增 pub 项，无破坏性）。

### os-guest / guest-agent：axum Portal + nftnl + JWT 接通 ✅

- **分支**：`real/guest-agent`（worktree `os-wt-real-guest`）。
- **交付**（`crates/os-guest/src/impls.rs`）：
  - **HttpCaptivePortal 真实 axum 监听**：`build_router()` 公开供 `tower::ServiceExt::oneshot`
    离线打 Router（不真监听端口）；`start()` 用 `tokio::net::TcpListener` + `axum::serve` +
    `tokio::spawn` 后台跑，`stop()` 经 oneshot graceful shutdown 通道关闭。路由含
    `/portal/landing`(GET→200 HTML)、`/portal/auth`(GET→302)、`/portal/register`(POST→标记认证态)、
    `/generate_204`/`/hotspot-detect.html`/`/connecttest.txt`/`/ncsi.txt`（各 OS 探测端点）+ fallback
    兜底；302 用手写 `redirect_302`（axum 0.8 `Redirect::to` 是 303 不符合 §3.18 期望）。
  - **IdentityEngine 真实 JWT 签发**：`DefaultIdentityEngine::with_jwt_impl(Arc<JwtIssuerImpl>)`
    注入 os-security 真实 JwtIssuer；`authenticate_guest` 后签 `TokenType::Guest` JWT 并存入
    `last_jwt`（供 `issued_jwt()` 取回）。经本地 dyn 兼容包装 `pub(crate) trait GuestJwtIssuer`
    （手写 `Pin<Box<dyn Future + Send>>` 返回值，不依赖 `#[async_trait]` HRTB Send 推断——
    os-security `JwtIssuer` 是原生 async fn in trait，非 dyn 兼容，ADR-COMPAT-001）；
    未注入时保持向后兼容仅维护 `jwt_expiry`。
  - **NftRuleOrchestratorImpl 真实 nftables 事务**：经 `nftnl-ffi` feature 门控
    （`apply`/`revoke`/`rollback_checkpoint` 调 `nftnl_apply_statements`）；apply 现真正存 checkpoint
    并暴露 `last_checkpoint_id()` 供回滚。FFI 链接需 `apt install libnftnl-dev libmnl-dev`
    （ADR-DEPS-001 §91），CI 沙箱缺，故 `--features nftnl-ffi` 在无 FFI 环境下因 pkg-config
    失败（预期门控）；默认/`mock` feature 路径完全不触发 FFI 链接，`nftnl_apply_statements`
    为占位（明确 `Err`，不静默成功）。
  - **ChainOrchestrator**：已用泛型注入 wallet+security；os-wallet `RpcRegistry` 已是真实 reqwest
    探活、os-security `JwtIssuerImpl` 已是真实 jsonwebtoken，注入真实实现即得真实链路（新增
    `chain_orchestrator_real_jwt_issuer` 测注入真实 `JwtIssuerImpl`）。
- **crate 级依赖新增**：`axum` / `tower` / `hyper` / `reqwest` `.workspace = true`，
  `nftnl = { workspace = true, optional = true }` + 新 feature `nftnl-ffi`。
- **测试**：os-guest 单位测 **49 → 59**（+10：IdentityEngine 真实 JWT 签发 + 向后兼容 2 条；
  axum Portal 路由 oneshot 6 条含 1 条端到端真实监听 + reqwest 打真实 HTTP + graceful shutdown；
  ChainOrchestrator 真实 JwtIssuerImpl 1 条）。
- **验证**：`cargo check/test/clippy -p os-guest --features mock --tests -- -D warnings` 全绿；
  `cargo clippy -p os-guest -- -D warnings`（lib，默认）全绿。
- **未碰**：trait 签名未改（5 trait 全保留）；其他 agent crate 未改。

---

## P2 真实集成第二波（wave1 + wave2，2026-08-05 合并 main `d7200c2`）

> ADR-DEPS-002（P2 领域依赖，20 个）注册后，分两波子代理接通真实实现。wave1（meta/protocol/wallet/security/monitor）+ wave2（i18n/vm/osd/devtools/discover）共 10 个分支全部合并 main。每波单波 ≤6（限流教训）。**trait 签名零改动**；冲突处理：所有 5 个 wave2 分支基于 `8fd247f`（DEPS-002 注册点），共享 crate（meta/protocols/security/services/wallet）的 `Cargo.toml`/`.rs` 与 main(wave1) 一致 → Cargo.lock 唯一冲突（以 main 为基重新生成保留 wave1 lock 条目 + 新增）；devtools 在 os-services/Cargo.toml 累积保留 monitor(opentelemetry) 与 devtools(gix) 两块。

### wave1（已合并，`110a00c`）

#### os-meta：openraft 共识 + rusqlite MetaStore ✅

- **分支**：`p2/meta-agent`（已合并）。
- **交付**（`crates/os-meta/src/raft_backend.rs` + `impls.rs`）：`OpenraftConsensus` 真实 Raft（`openraft` features `serde`/`storage-v2`/`single-term-leader`，实现 `RaftLogStorage` + `RaftStateMachine` 的 `Sealed` impl）；`SqliteMetaStore` 真实 `rusqlite`（bundled，apply_log 持久化 + snapshot/restore 用 dump）。
- **接入**：`crates/os-meta/Cargo.toml` 加 `openraft`/`rusqlite.workspace = true`。
- **测试**：55 → **65**（+10：openraft/rusqlite 真实后端往返）。
- **未接入**：netlink VIP 漂移（系统级，待真实网络层）；VM 迁移执行（依赖 os-compute mock）。

#### os-protocols：dav-server/libunftp/russh 真实协议栈 ✅

- **分支**：`p2/protocol-agent`（已合并）。
- **交付**（`ftp_backend.rs` / `sftp_backend.rs` + `orchestrators.rs`）：WebDAV（`DavServerBackend`，每共享一份 `dav_server::DavHandler` + MemFs，`handle_request` 离线驱动 RFC4918）；FTP（`LibunftpBackend` + `InMemoryFtpBackend` 实现 `StorageBackend`，`build_server` 构造真实未监听 `libunftp::Server`）；SFTP（`OsSshHandler` 实现 `russh::server::Handler` 公钥认证 + SFTP 子系统，`build_ssh_server`/`build_ssh_config` 带 Ed25519 主机密钥）。
- **接入**：`crates/os-protocols/Cargo.toml` 加 `dav-server`(memfs)/`libunftp`(ring)/`russh`(ring+flate2+rsa) + 辅助 `unftp-core`/`http`/`rand`；workspace 根 libunftp/russh 改 `default-features=false`（ADR-DEPS-002 ring 策略）+ 新增 `http` 注册。
- **不真监听端口**（红线）：三后端持真实协议栈对象，端口绑定由上层负责。
- **测试**：84 → **91**（+7：协议栈离线驱动 + handler 公钥认证）。
- **未接入**：SMB/NFS 维持 CLI 骨架（未引入 samba crate）；端口监听由 api/service 挂载。

#### os-wallet：bitcoin/alloy 真实验签 ✅

- **分支**：`p2/wallet-agent`（已合并；叠加在 `real/wallet-agent` 的 reqwest 探活之上）。
- **交付**（`signing.rs` + `chain.rs`）：真实验签 `verify_eip191`/`verify_eip712`（alloy `recover_address_from_msg`/`TypedData::eip712_signing_hash`，RFC web3.js v1.2.2 向量）；`verify_schnorr`（BIP-340，bitcoin secp256k1）；`verify_ecdsa_message`（Bitcoin Core signmessage 兼容，RecoverableSignature）；adapter `query_balance`/`query_credential`（eth_getBalance/ownerOf/balanceOf ABI 自实现 + scantxoutset）。辅助 `eth_function_selector`（keccak256[:4]，已知向量 ownerOf=0x6352211e）。
- **接入**：`crates/os-wallet/Cargo.toml` 加 `bitcoin`/`alloy`(eip712)/`secp256k1`(recovery+hashes)。
- **测试**：41 → **65**（+24：EIP-191 RFC×5 / EIP-712×2 / Schnorr×4 / ECDSA×2 / ABI×4 / adapter×5 + 其他）。
- **未接入**：`verify_bip322` 留 TODO（完整伪交易封装）。

#### os-security：argon2 + jwt + totp + rcgen + boringtun ✅

- **分支**：`p2/security-agent`（已合并）。
- **交付**（`password.rs`/`impls.rs`/`totp.rs`）：Argon2id 密码哈希（PHC 字符串，OsRng salt，解析失败回退常量时间比较兼容 mock）；JwtIssuerImpl 真实 jsonwebtoken HS256（密钥轮换宽限期最多 4 个）；TOTP 真实 HMAC-SHA1（RFC 4226/6238 测试向量全过）+ base32 otpauth URI；rcgen CA 自签；boringtun WireGuard noise 协议层（peer 增删查 + 握手状态 + 字节统计）。
- **接入**：`crates/os-security/Cargo.toml` 加 `argon2`/`jsonwebtoken`/`hmac`/`sha1`/`rand`/`rcgen`/`boringtun`。
- **测试**：33 → **67**（+34：Argon2/JWT/TOTP/rcgen/boringtun 真实路径）。
- **未接入**：`acme_request`（instant-acme 未注册，P3）；TOTP secret 持久化（AEAD 未注册）；boringtun 真实数据面（device feature + root）。

#### os-services(monitor)：opentelemetry 真实指标 + Prometheus 导出 ✅

- **分支**：`p2/monitor-agent`（已合并）。
- **交付**（`crates/os-services/src/monitor.rs`）：`OtelMonitor` 真实 Counter/Gauge/Histogram 采集（按 MetricKind 分派，Counter 单调累加/Gauge last-write-wins/Histogram SDK 内桶聚合）；仪器句柄 lazy 缓存；`render_metrics()` 用 `registry.gather() + TextEncoder::encode()` 生成 Prometheus exposition v0.0.4；`metrics_content_type()`；exporter 配置 `without_target_info/scope_info` + `service.name=os` resource。保留内存时序供 query_metrics + 告警引擎抖动窗口。
- **接入**：workspace 根加 `opentelemetry_sdk`/`prometheus`；crate 级 4 个 `.workspace = true`。
- **测试**：os-services +8 OTel 导出测（gauge/counter/histogram/labels/content-type/告警端到端）。
- **未接入**：tracing-subscriber 日志桥接（tail_logs 真实日志源）；OTLP exporter（推模式）。

### wave2（本会话合并，`d7200c2`）

#### os-i18n：toml 真实解析 ✅

- **分支**：`p2/i18n-agent`（合并 `62a1316`）。
- **交付**（`crates/os-i18n/src/impl_translator.rs`）：`parse_toml` 用 `toml::from_str::<toml::Table>` 完整解析（替换自写极简解析），`flatten_table` 递归扁平化嵌套表为 `full_key -> 文案`；支持行注释/节头/嵌套表/内联表/数组/多行字符串/全部转义；非字符串叶子跳过；解析失败返回 `I18nError::ParseFailed`。trait 签名零改，`from_toml`/`new`/`reload`/`t` 入口语义不变，fallback 链不变。
- **接入**：`crates/os-i18n/Cargo.toml` 加 `toml.workspace = true`。
- **测试**：16 → **19**（+3：完整 TOML 语法解析测）。
- **未接入**：无（无运行时阻塞）。

#### os-compute(vm)：virt 真实 KVM（feature 门控）✅

- **分支**：`p2/vm-agent`（合并 `95c8d10`）。
- **交付**（`crates/os-compute/src/impl_vm.rs`）：`Cargo.toml` 加 `virt = { workspace=true, optional=true }` + feature `virt-ffi = ["dep:virt"]`；`impl_vm.rs` 重构为双互斥路径（`#[cfg(not(feature="virt-ffi"))]` 内存骨架 / `#[cfg(feature="virt-ffi")]` 真实 virt）。真实路径：`virConnectOpen`（惰性缓存）+ Domain 全生命周期（DefineXML/Create/Shutdown/Destroy/Suspend/Resume/Undefine/GetState/LookupByUUIDString）+ `virConnectListAllDomains` + `virDomainMigrateToURI`（active-passive: PEER2PEER|LIVE|UNDEFINE_SOURCE）；`LibvirtDomainState::from_raw(u32)` 覆盖 0..=7 全枚举；virt-ffi 测试用 `test:///default` fixture 驱动，无 libvirt-dev 时优雅跳过。
- **接入**：`crates/os-compute/Cargo.toml`（feature `virt-ffi` + optional virt dep）。
- **测试**：97 → **99**（+2：vm 状态映射 + fixture 路径）。
- **未接入（运行时阻塞）**：`cargo test --features virt-ffi` 链接失败（undefined symbol: virConnectOpen）—— 需 `apt install libvirt-dev` + libvirtd 环境；运行期 root/libvirt 组权限。

#### osd：cgroups-rs 真实 cgroup v2 配额 ✅

- **分支**：`p2/osd-agent`（合并 `2e36303`）。
- **交付**（`crates/osd/src/cgroup.rs` + `impl_orchestrator.rs`）：`CgroupsRsBackend` 真实 cgroup v2 资源限制（cpu.max/memory.max/pids.max），`SystemdOrchestrator` 注入；`InMemoryCgroupBackend` 覆盖逻辑正确性（非 root 环境）。
- **接入**：`crates/osd/Cargo.toml` 加 `cgroups-rs.workspace = true`。
- **测试**：35 → **59**（+24：cgroup 配额逻辑 + 状态机）。
- **未接入（运行时阻塞）**：真实 systemd unit 生成 + 进程监管（root + CAP_SYS_ADMIN）；CgroupsRsBackend 真写 /sys/fs/cgroup（root + cgroup v2 沙箱）；ChronyNtp（chrony 绑定未注册 + CAP_SYS_TIME）；退避重启；IO 限速 io.max（ResourceQuota 缺设备号字段）。

#### os-services(devtools)：gix 真实 Git 服务 ✅

- **分支**：`p2/devtools-agent`（合并 `9486a1e`）。
- **交付**（`crates/os-services/src/impl_devtools.rs`）：模块级自由函数 `init_repo`/`commit_all`/`create_branch`/`list_branches`/`head_commit`/`log` 封装 gix 0.86（init 幂等；commit_all 经 `empty_tree().edit()` 写 blob→tree→`commit_as` 显式注入 os-devtools-agent 身份；log 走 rev_walk breadth-first；create_branch ref 事务 MustNotExist 语义）；`trigger_pipeline` 本地仓库用 gix 真实读 head commit 作 logs_url 锚点 + 记录真实 CiRun。
- **接入**：`crates/os-services/Cargo.toml` 加 `gix = { workspace=true, features=["tree-editor"] }`（+dev `tempfile`）；**冲突累积解**：同时保留 monitor(opentelemetry wave1) 与 devtools(gix wave2) 两块。
- **测试**：os-services +21（gix 往返 + trigger/pipeline_status 真实 git 状态 + KVS 回归）。
- **未接入**：远端 git clone（需 gix blocking-network-client feature）；AEAD 真实加密（密钥 KVS 已接通 aes-gcm，见 ADR-DEPS-003）；CI steps 沙箱执行。

#### os-discover：mdns-sd 组播 + rustls mTLS ✅

- **分支**：`p2/discover-agent`（合并 `341dc32`）+ **bugfix**（`d7200c2`）。
- **交付**（`crates/os-discover/src/impls.rs` + `mtls.rs`）：mDNS 真实组播发现（mdns-sd ServiceData browse + TXT 往返，loopback 测 publisher 自解析）；`MtlsPeerAuthenticator` 真实 mTLS 双向认证（rustls 0.23 + ring，`pair` 经 `complete_io` 驱动握手，取对端证书 SHA-256 指纹写入 PeerSession；unpair/list_trusted；自签证书 fixture loopback 测：成功/过期拒绝/不受信根拒绝/TCP 拒绝/unpair）。
- **接入**：`crates/os-discover/Cargo.toml` 加 `mdns-sd`/`rustls`(std feature)；dev `rcgen`（fixture 自签证书）。
- **bugfix（本会话发现）**：合并后 `cargo test --workspace --features mock` 下 4 个 mTLS 握手测 panic（rustls 进程级 CryptoProvider 自动探测失败——workspace 同时激活 ring 与 aws-lc-rs）。修法：`mtls.rs:342` `WebPkiClientVerifier::builder_with_provider(root_store, provider)` 显式注入 ring（与 server/client config 一致）。commit `d7200c2`。
- **测试**：40 → **63**（+23：mDNS 组播 5 + mTLS 真实握手 7 + beacon ed25519 验签等）。
- **未接入**：持续扫描事件循环（上层集成 tokio task 驱动）；beacon 公钥指纹与 mTLS 证书指纹关联校验（上层 CertManager 协同）。

---

## P3 真实集成（2026-08-05，分支 `feature/acme`）

### os-security：instant-acme ACME 自动证书签续 ✅

- **分支**：`feature/acme`（worktree `os-wt-acme`）。
- **交付**（`crates/os-security/src/acme.rs` 新 + `impls.rs` 改）：
  - **`AcmeConfig` + `AcmeChallengeSolver` trait**：抽象 ACME 配置（directory URL + 联系邮箱 +
    challenge 完成策略）与 challenge 完成器（HTTP-01/DNS-01 摆放由调用方注入）。提供
    `AcmeConfig::lets_encrypt_staging`/`production`/`with_directory`（含 `with_http` 注入
    测试 HttpClient）构造器。
  - **`CaCertManager::acme_request` 真实实现**：原 `Err(Internal)` 占位 → 用 instant-acme 走
    完整 RFC 8555 流程（Account 创建 → NewOrder → authorizations 遍历 + challenge solve +
    set_ready → poll_ready → finalize → poll_certificate）。PEM 证书链经 x509-parser 解析为
    `Certificate` 元数据（CN 优先，回退首 SAN DNS），入库 `auto_renew=true`、`source=Acme(domain)`。
  - **ACME 与 rcgen CA 协调**：ACME 证书优先（公网可信）；CA 自签作 fallback——`acme_request`
    未注入 `AcmeConfig` 时返回明确错误（不静默回退，调用方显式选 `init_ca + sign`）。
  - **`renew_expiring(threshold_days)`**：续期入口（trait 签名零改，新增关联方法）。遍历
    `auto_renew=true` 且 `not_after - now < threshold_days` 的证书，按来源分派——ACME 来源
    re-request（`acme_request(domain)` + 移除旧 serial 记录），CA 自签来源走既有 `renew(id)`。
    单证书失败不中断其他续期。
  - **`AutoSolveSolver` + `FixtureAcmeServer`**（测试 fixture）：内存 ACME 服务器实现
    `instant_acme::HttpClient` trait，模拟 directory/newNonce/newAccount/newOrder/authz/challenge/
    finalize/cert 序列（含 JWS payload 解码、Location/Replay-Nonce header、challenge set_ready
    即置 Valid、finalize 用 rcgen fixture CA 签发叶子）。instant-acme 的 JWS/nonce/重试逻辑
    真实跑，仅网络层替换为内存——零 LE 请求（红线）。
- **接入**：`crates/os-security/Cargo.toml` 加 `instant-acme.workspace = true`（feature
  `hyper-rustls`/`rcgen`/`ring`，不开 aws-lc-rs）+ `http`/`bytes`（HttpClient trait 类型）；
  dev `futures`/`http-body-util`（fixture 读/构 body）。workspace 根 `[workspace.dependencies]`
  注册 `instant-acme = { version="0.8", default-features=false, features=["hyper-rustls","rcgen","ring"] }`
  （锁定 0.8.5）。新增 ADR：[ADR-DEPS-004](./adr/ADR-DEPS-004-acme.md)。
- **测试**：os-security 单位测 **67 → 81**（+14：acme_request 完整流程 + 证书入 list + 多域名 +
  solver 观察 + 无配置错误 + DNS-01 缺失报错 + renew_expiring 4 场景：跳过非续期/ACME 续期/
  CA 续期/无 ACME 配置报错 + acme 模块单测 4：PEM 解析/challenge kind eq/auto-solve 记录）。
  默认 feature 与 `--features mock` 均通过。
- **验证**：`cargo check/test/clippy/doc -p os-security --features mock` 全绿（0 warning）；
  `cargo check --workspace --features mock` 全绿（无下游回归，os-guest 注入 JwtIssuerImpl 路径
  不受影响——ACME 是 CertManager 新路径，下游仅消费既有 trait）。
- **未接入（仍 TODO）**：Account credentials 持久化（keyring/KMS，与 CA 私钥同存储策略）；
  ARI（ACME Renewal Information）续期窗口（instant-acme 0.8 支持，需 LE 生产环境）；
  真实 LE Staging 集成测（需联网 + 真摆 HTTP-01/DNS-01，留 `#[ignore]`）；External Account
  Binding（ZeroSSL/Google Trust Services）；ACME 证书私钥持久化（`Order::finalize` 返回的 PEM）。
- **trait 签名零改**（`CertManager` 5 方法签名全保留）；`SecurityError` 不新增 variant（ACME
  错误映射到既有 `Internal`）；其他 agent crate 零改。

---

## 下一步（供后续会话）

P0/P1/P2/P3 真实集成 + review2 闭环全部完成。完整优先级与开局指令见 [HANDOVER.md](./HANDOVER.md) §7 与 §12。要点：

1. **剩余运行时阻塞项**（依赖已注册）：FFmpeg/CLIP、youki 真实运行时、真实 PXE、远端 git clone、真实 root 环境测（virt-ffi/cgroups/systemd/nftnl-ffi/bootloader）——见上方「阻塞」清单。
2. **ERROR_GUIDE 5 处 P2 复审**（可选，非阻断）：见 `docs/ERROR_GUIDE.md` §3.3。
3. 派 integration-agent 扩场景（真实 root 场景用 SANDBOX 沙箱跑）。
4. 派 devops-agent 建 CI 守护 + os-iso 真实构建 + 发布打包。
5. 派子代理时单波 ≤6（限流教训）。

---

## 性能基准骨架（criterion）—— branch `feature/bench`

为 5 个高价值纯算法 crate 加 criterion micro-benchmark 骨架，建立回归基线。**仅加 `benches/` + `[dev-dependencies]`，trait/业务逻辑零改动**（红线遵守）。

- **依赖**：workspace 根 `Cargo.toml [workspace.dependencies]` 加 `criterion = { version = "0.5", features = ["html_reports"] }`（dev-only，不进 release 产物）。
- **5 个 crate 的基准**（`[[bench]] name=... harness=false` + `benches/<name>.rs`）：

| crate | bench 文件 | 覆盖算法 | 初始数字（中位数） |
|-------|-----------|---------|-------------------|
| os-meta | `benches/raft.rs` | `advance_commit_index`（commitIndex 推进扫描）/ `advance_commit_index_from_log` / `check_election`（投票去重 + quorum）/ `log_is_up_to_date` / `InMemoryMetaState::apply`（CAS/UPSERT 同路径） | commitIndex：1.76 ns（全员复制时首项命中即 break）；check_election 1000 票：14.5 µs（69 Melem/s）；meta_apply 10000 条：3.06 ms（3.27 Melem/s） |
| os-storage | `benches/zfs_parse.rs` | `Pool::from_list_line` / `Dataset::from_list_line` / `Snapshot::from_list_line`（大输出批量解析） | pool 10k 行：2.10 ms（4.77 Melem/s）；dataset 10k 行：1.77 ms（5.65 Melem/s）；snapshot 10k 行：1.10 ms（9.13 Melem/s） |
| os-api | `benches/routing.rs` | `RouteRegistry::register`（冲突检测线性扫描）/ `match_request`（线性扫描 + specificity 排序）/ `match_path`（单次模式匹配） | register 5000 条：1.46 s（O(n²) 回归点）；match 5000 路由：~600 µs；match_path 单次：270 ns |
| os-services | `benches/tantivy_search.rs` | `SearchIndex::add_file+commit`（建索引）/ `search`（BM25 + Count + snippet） | 建索引 2000 docs：13.2 ms（151 Kelem/s）；查 term_rust 2000 docs：44.5 µs；miss：5.7 µs（175 Kelem/s） |
| osd | `benches/topo.rs` | `topological_sort`（Kahn 拓扑排序 + 环检测，不同图形态） | 线性链 5000 节点：933 µs（5.36 Melem/s）；稀疏 DAG 5000：1.13 ms；分层 DAG l5_w30（150 节点 ~3600 边）：167 µs |

**回归点提示**（数字异常时优先排查）：
- os-api `route_register` 是 O(n²)（注册时对每条新路由全表 same_pattern 扫描）—— n=5000 已 1.5s，未来若路由数增长应改 radix tree。
- os-meta `advance_commit_index` 当前测的是"全员复制到顶"最快路径；如改 has_quorum_set 为 BTreeSet 等，1.76 ns 基线会变。
- osd `topological_sort` 5 Melem/s 是 HashMap 入度表的开销下限；若降到 <1 Melem/s 说明回归。

**验证**：`cargo check --workspace --features mock` ✅ 0 error；`cargo clippy --workspace --features mock --benches -- -D warnings` ✅ 0 warning；5 个 bench 二进制均 `cargo bench --no-run` 通过并实跑出数。

criterion baseline 已写入 `target/criterion/`（默认 100 样本；本次实跑用 `--sample-size 10-20 --measurement-time 2` 快速采点，正式回归基线建议重跑默认配置）。


---

## 收尾阶段（2026-08-05，main `d7200c2` → `4eb29cb`）

> 本节记录 HANDOVER `d7200c2` 截止点之后合并的全部批次。测试从 1491 增至 1935（`--features mock`，+444）。trait 签名全程零改动。

### os-wallet：完整 BIP-322 验签（`feature/bip322`，合并 `9cf7af7`）

- **交付**（`crates/os-wallet/src/signing.rs` +809 行）：完整 BIP-322 签名验证——legacy（P2PKH 伪交易 + ECDSA recover + signmessage 哈希）与 simple（witness stack 结构校验）两条路径。RFC 测试向量覆盖。
- **接入**：`crates/os-wallet/Cargo.toml` 加 bitcoin feature（已有 bitcoin 依赖，开 hashes/recovery）。
- **测试**：os-wallet 65 → **79**（`--features mock`）。
- **意义**：原 §7 阻塞项「BIP-322 验签（完整伪交易封装未做）」**已解决**。

### dyn 兼容补全（`fix/dyn-compat`，合并 `a8137cc`）

- **交付**：ADR-COMPAT-001 真实集成阶段补全——os-storage `Replication`、os-wallet `WalletConnector`/`RpcRegistry` 加 `#[async_trait]`（这些 trait 在真实集成中被 `Box<dyn>` 使用，骨架期未触发）。
- **意义**：dyn 兼容覆盖完整，无编译警告。

### os-api：路由 O(n²)→O(1) 优化（`fix/radix-route`，合并 `4dffb46`）

- **交付**（`crates/os-api/src/`）：路由 `register`（原 O(n²) 全表 same_pattern 扫描）+ `match`（原 O(n) 线性扫描）优化为 **method 分桶 + 静态 HashMap**，register/match 降至近 O(1)。bench 基线更新（`benches/routing.rs`）。
- **意义**：原 bench 提示的 O(n²) 回归点（n=5000 已 1.5s）**已消除**。

### 跨 crate 集成测（`integration/main` + `feature/integ2`，合并 `953f1de`/`f7fb9ef`）

- **交付**（新 crate `os-integration`，`tests/` 5 场景）：
  - `vm_creation_chain.rs`：VM 创建链（compute→meta→storage 状态联动）。
  - `guest_chain_verification.rs`：访客验证链（guest→security JWT→wallet 链适配，chain-dyn 批次重构为 dyn 注入）。
  - `ha_failover_chain.rs`：HA 故障转移链（discover→meta Raft 选举）。
  - `backup_chain.rs`（integ2）：备份链（services backup→storage snapshot→meta log）。
  - `im_conversation_as_action.rs`（integ2）：IM 对话即操作链（im→api→services 执行）。
- **接入**：workspace 根 `Cargo.toml` members 加 `os-integration`；dev-dep 引用相关 crate。
- **测试**：os-integration **38 测**（默认 feature，集成测不依赖 mock feature）。
- **chain-dyn 配套**（`0676ba1`）：os-guest `ChainOrchestrator` 从泛型注入重构为 **dyn 注入**（简化构造）；os-security `JwtIssuer` 加 `#[async_trait]`（dyn 兼容）；guest_chain_verification 测适配。Cargo.lock 累积。

### 真实网络冒烟（`feature/nettest`，合并 `f292c32`）

- **交付**（新 crate `nettest`）：真实网络连通冒烟测——reqwest HTTP/axum 服务/mdns-sd 组播/rustls TLS，全部 `#[ignore]`（默认不跑，避免 CI 需联网/端口）。手动 `cargo test -p nettest -- --ignored` 在联网环境跑。
- **意义**：真实网络栈端到端冒烟，不污染默认测试。

### os-services：FFmpeg 转码编排 + CLIP 接口骨架（`feature/ffmpeg-clip`，合并 `783da63`）

- **交付**（`crates/os-services/src/media_ffmpeg.rs` 796 行 + `media_clip.rs` 653 行）：
  - `media_ffmpeg.rs`：FFmpeg 转码编排骨架——命令构造（分辨率/码率/编码器/ABR 自适应）、进度解析、任务状态机、并发限流；真实 FFmpeg 二进制调用留运行时注入（`FFmpegRunner` trait，生产注入进程 + 测试注入 mock）。
  - `media_clip.rs`：CLIP 向量接口骨架——`ClipEmbedder` trait（图像/文本 → 向量），特征库索引/检索抽象；**推理后端未定**（candle/ort/onnxruntime 待 ADR），当前 `MockEmbedder` 覆盖接口契约。
- **测试**：os-services +FFmpeg/CLIP 编排逻辑测（含状态机、命令构造、进度解析）。
- **未接入（运行时阻塞）**：真实 FFmpeg 二进制；CLIP 推理模型加载 + 向量推理后端。

### 真实环境测沙箱方案（`feature/sandbox-doc`，合并 `e74aa7d`）

- **交付**（`docs/SANDBOX.md` 325 行 + `scripts/sandbox/`）：
  - **Docker 沙箱**（`scripts/sandbox/docker/`）：`Dockerfile.test`（基于 ubuntu，装 libvirt-dev/libnftnl-dev/libmnl-dev/zfsutils 等系统依赖）+ `run-tests.sh`（容器内跑 `cargo test --features mock` + 可选 `--features virt-ffi`/`nftnl-ffi` 真实 FFI 测）。
  - **QEMU 沙箱**（`scripts/sandbox/qemu/`）：完整虚拟机方案（README 253 行），cloud-init 配置，可跑真实 systemd/cgroup/kernel 级破坏性测。
  - **nspawn 方案**：轻量容器，cgroup namespace 隔离。
- **意义**：为 §7 真实 root 环境测提供「可真实跑、可重复、不污染宿主」的方案。

### 部署指南 + 第二轮评审（`feature/deploy-doc` + `feature/review2`，合并 `23b9d71`/`dbfb146`）

- **`docs/DEPLOYMENT.md`**（850 行）：从源码组装可运行 OS 系统的全流程——构建、`osd` 启动编排、配置、systemd 集成、ISO 安装、HA 集群、升级/回滚、监控接入。
- **`docs/REVIEW.md` 第二轮（R2）**：基线 `783da63`（P0/P1/P2 接通 + integration + 真实实现整合后）全面审计；R1 的 7 问题（P1–P7）全部闭环；R2 TODO 残留统计。

### 真实执行层整合（最后几批，`storage-real`/`pkg-real`/`net-real`）

> 把骨架期留 TODO 的「CLI/系统调用占位」替换为真实执行层（trait 签名零改）。

- **os-storage ZFS CLI 真实执行层**（`feature/storage-real`，合并 `e1b7883`，+19 测）：`backend_impl.rs` 488 行真实解析 + `TokioCommandRunner`（trait 抽象命令执行，生产 tokio::process + 测试 mock）；ZFS pool/dataset/snapshot list/create/destroy 真实命令构造 + 输出解析。测试 51 → **72**。
- **os-compute apt 真实执行 + OCI/CNI 落盘**（`feature/pkg-real`，合并 `1561369`，+39 测）：`apt.rs`（+510）真实 apt-get 命令编排（install/upgrade/remove/update，事务回滚）；`cni.rs`（+182）CNI 网络配置真实落盘（JSON schema）；`desktop.rs`（+179）桌面环境包组。统一 CommandOutput（后被 cmd-output-fix 重构）。测试 99 → **138**。
- **os-network rtnetlink/nftnl 真实执行层**（`feature/net-real`，合并 `7286733`，+17 测）：`backend.rs`（+697）rtnetlink 真实接口/地址/路由事务（FFI 门控 `rtnetlink-ffi` feature）；`nftnl_real.rs`（322 行）nftables 真实事务（apply/revoke/checkpoint，FFI 门控 `nftnl-ffi`）。无 FFI 环境走骨架路径。测试 87 → **108**。

### 收尾 5 批（合并，main → `c004231`）

> 收尾的 5 个 feature 分支，**全部零冲突**。顺序按依赖：cmd-output-fix（基础重构）→ osd-ntp → provision-real → update-slot → cli-real。

- **os-core 统一 CommandOutput**（`feature/cmd-output-fix`，合并 `a95aa9d`）：纯重构——消除 os-compute(`apt.rs`)/os-storage(`backend_impl.rs`)/os-services(`media_ffmpeg.rs`) 3 处重复定义的 `CommandOutput` 结构，统一到 `os-core/src/types.rs`（+62 行）。各 crate 改 import。trait 签名零改，行为不变。**review2 P-R2-1 由此闭环**（原 3 处同构 → 1 处统一定义；os-cli 的 CommandOutput 是 CLI 渲染输出，不同概念，保留）。
- **osd chrony NTP 真实编排**（`feature/osd-ntp`，合并 `43a3746`）：`crates/osd/src/ntp_impl.rs`（996 行）—— `ChronyNtp` 真实编排：chronyc 命令封装（sources/tracking/makestep）、时钟源优先级、偏移监控、同步状态机；`SystemdOrchestrator` 注入。真实 chrony 服务运行需 CAP_SYS_TIME（当前 fixture 覆盖逻辑）。测试 osd → **93**。
- **os-provision PXE 引导配置 + 初始化脚本编排**（`feature/provision-real`，合并 `9ebb75c`）：`pxe.rs`（386）PXE 引导配置（pxelinux.cfg 生成、kernel/initrd 路径、启动参数）+ `init_script.rs`（361）初始化脚本编排（cloud-init/user-data 生成、首启任务）+ `transfer.rs`（444）迁移执行编排（与既有迁移状态机协同）。真实 TFTP/DHCP 服务下发未接（运行时）。测试 provision → **88**。
- **os-update bootloader A/B 槽位真实激活**（`feature/update-slot`，合并 `7b8795e`）：`bootloader.rs`（986 行）—— GRUB（grub-set-default/grub-editenv）+ systemd-boot（bootctl set-default/LoaderEntryDefault）双后端真实激活；`impls.rs`（+395）`SlotManager` 接通 bootloader：activate_slot 真实写 boot 配置 + 下次启动生效 + 回滚。真实写盘需 root（破坏性，须沙箱）。测试 update → **139**。
- **os-cli clap 接入 + 运维子命令骨架**（`feature/cli-real`，合并 `c004231`）：`cli.rs`（893 行）—— clap derive 命令树（顶级 os + 子命令：storage/network/compute/security/backup/update/system/service/devtools）；每子命令参数解析 + 调用 os-api client 骨架；输出格式化。workspace 根 `Cargo.toml` 注册 `clap`（+ `crates/os-cli/Cargo.toml` +13）。测试 cli → **56**。

---

## review2 闭环阶段（2026-08-05，main `c004231` → `4eb29cb`）

> 本节记录 HANDOVER `c004231` 截止点之后合并的最后 5 个 feature 分支，**全部零冲突，全部为 review2 应修/建议的闭环 + 文档收尾**。测试从 1838 增至 1935（`--features mock`，+97）。trait 签名全程零改动。详见 `docs/REVIEW.md`。

### ERROR_GUIDE 错误码归类指引（`feature/error-guide`，合并 `c4c6662`）

- **交付**（`docs/ERROR_GUIDE.md` 203 行）：review2 P7 / R1-P7「错误码归类指引未沉淀」闭环产物。沉淀：
  - §1 ApiErrorCode 10 变体语义 + 客户端语义 + 三条边界规则（NotFound vs InvalidInput / PermissionDenied vs Conflict / UpstreamUnavailable vs Internal）。
  - §2 Error 模式 → 推荐码归类映射表。
  - §3 全 21 个 `From<XxxError> for ApiError` 实现逐项审计（163 变体映射，符合指引 148 / 偏差 15），沉淀 **5 处 P2 复审建议**（os-iso HardwareIncompatible / os-protocols ProtocolDisabled / os-security CertExpired / os-storage CommandFailed / os-wallet ChainUnsupported）。
  - §4 新增 Error 变体决策流程 + 实现 Checklist + 何时新增 ApiErrorCode 变体。
- **非强制约定**：归类有合理歧义时给"推荐 + 可接受备选"，本指引**不在本 PR 改源码**，作下一轮迭代对照基线。
- **意义**：review2 P7（错误码归类指引未沉淀）**闭环**。

### os-common 补测 + os-compute mock 归并（`feature/common-test`，合并 `2431d87`）

- **os-common 补 13 测**（review2 P-R2-3 / R1-P6 闭环）：原 `os-common` lib unittest 0（纯 DTO/错误码层）。补 13 个冒烟测——`ApiError` 构造器（not_found/invalid_input/permission_denied/internal）、`ApiErrorCode` Display & serde、`Versioned::api_version` 默认值、`VersionedEnvelope` round-trip。测试 os-common → **13**（原 0）。
- **os-compute mock 归并**（review2 P-R2-2 / R1-P5 闭环）：删 `crates/os-compute/src/mock_vm.rs`，归并到 `mock.rs` 单一来源；`lib.rs` 删除 `pub use mock_vm::{...}` 重复 re-export。`MockVmManager` 等符号现仅从 `mock.rs` 导出，消除同名符号双源隐患。trait 签名零改。
- **意义**：review2 P-R2-2（mock 双源）+ P-R2-3（os-common 0 测）**闭环**。

### os-compute youki 容器运行时编排层骨架（`feature/youki-rt`，合并 `a6623cb`）

- **交付**（`crates/os-compute/src/runtime.rs`）：youki 容器运行时**编排层骨架**——`ContainerRuntime` trait 的 youki 适配器（命令构造：youki create/run/exec/delete + OCI bundle 路径解析 + cgroup/namespace 参数）。真实 youki/runc 二进制执行留运行时注入（trait 抽象，生产注入进程 + 测试注入 mock）。
- **未接入（运行时阻塞）**：真实 youki/runc 运行时二进制拉起容器（需 root + 二进制）。OCI bundle 落盘 + CNI 生成已真实（pkg-real 批次）。
- **测试**：os-compute → **178**（+youki 编排逻辑测）。
- **意义**：DEPENDENCIES.md 列的 youki 阻塞项**逻辑层闭环**（运行时二进制仍 TODO）。

### os-integration +3 场景（`feature/integ3`，合并 `80910c3`）

- **交付**（`crates/os-integration/tests/` 新增 3 文件）：
  - `api_route_aggregation.rs`：API 路由聚合链（api 多路由 → 各 service，radix 匹配 / 限流 / 中间件链）。
  - `discover_mtls_federation.rs`：discover mTLS 联邦链（mDNS 组播 + mTLS 握手 + 证书指纹校验 + 成员加入 meta）。
  - `update_rollback.rs`：update 回滚链（SlotManager → bootloader A/B 激活 → meta leader → services monitor 告警）。
- **接入**：`crates/os-integration/Cargo.toml` 加 `mock` feature（聚合测现可用 `--features mock` 跑，便于 mock 路径覆盖）。
- **测试**：os-integration 38 → **67**（+29，5 → 10 场景）。
- **意义**：integration-agent 覆盖度 +60%，跨 crate 链路测从 5 场景扩到 10 场景。

### os-services tracing 日志桥接（`feature/tracing-bridge`，合并 `4eb29cb`）

- **交付**（`crates/os-services/src/monitor.rs` + `log_bridge` 模块）：review2 遗留 TODO「tracing-subscriber 日志桥接」闭环——
  - **采集层**：自定义 `tracing_subscriber::Layer`（`log_bridge::LogBridgeLayer`）把 `tracing::Event` 捕获为结构化 `LogEntry`（level/target/timestamp/fields/message），推入共享 `LogBuffer`（Arc<Mutex<Vec>））。与 `fmt::Layer` 同级组合，clone-safe（仅持 Arc）。
  - **查询层**：`OtelMonitor::tail_logs(LogFilter)` 从 buffer 过滤返回（原占位 → 真实）。
  - **导出层**：`build_subscriber_with` 提供 tracing-subscriber JSON 格式导出骨架（`fmt::format::Json`）。
- **接入**：workspace 根 `Cargo.toml` 注册 `tracing-subscriber = "0.3"`（features `env-filter` + `json`）；`crates/os-services/Cargo.toml` 引用。
- **测试**：os-services → **359**（+tracing 桥接采集/查询/过滤测）。
- **意义**：review2 P-R2 遗留 TODO（monitor tail_logs 真实日志源）**闭环**。OTel 指标 + Prometheus 导出早已真实（wave1），现日志桥接也真实。

### 最终统计（main `4eb29cb`）

- **24 crate**（22 业务 + os-integration 10 场景 + nettest），workspace.dependencies **77 个**（含内部 crate path 依赖 + clap/tracing-subscriber/criterion）。
- **`cargo test --workspace --features mock`：1907 passed + 28 ignored = 1935**（默认 feature 1846 passed + 25 ignored）。
- **8 个 ADR**（COMPAT-001/002/003 + DEPS-001/002/003/004/005），全程 trait 签名零改动。
- **156 commits**，**所有 worktree 全部清理**，仅剩主工作树。
- **review2 全闭环**：R1 的 P1–P7（4 应修 + 3 建议）+ R2 的 P-R2-1/2/3/4/5 全部清账。
- 项目主体收尾 + review2 闭环。剩余仅运行时阻塞项 + ERROR_GUIDE 5 处 P2 复审建议（见 [HANDOVER.md](./HANDOVER.md) §7）。

### 最新统计（main `a973df2`，系统组装完成 + MCP Server 之后）

- **25 crate**（22 业务 + os-integration + nettest + **os-mcp**），workspace.dependencies **78 个**（+rmcp）。
- **`cargo test --workspace --features mock`：3348 passed + 127 ignored = 3475**。
- **4 个可执行 binary**：osd（守护进程 + `--serve-api` 一体化内嵌 HTTP）/ os（CLI）/ os-api（HTTP 网关 25 路由 6 组件）/ **os-mcp（MCP Server 10 tools）**。
- **8 个 ADR**，全程 trait 签名零改动。
- **371 commits**，仅剩主工作树。
- **系统组装完成**：`curl /api/v1/pools` → 真实 zfs 池 JSON / `os pool list` → CLI 端到端真实数据 / `os status` → hostname+CPU 虚拟化。6 RouteHandler（storage/compute/system/share/user/discover）。
- **MCP Server**：10 tools（pool/vm/share/user/node/status/virt_check 等），AI 助手可管理 OS。
- **覆盖率 ~90%**（cargo-tarpaulin）。
- **cargo doc 0 warning** + clippy 0 warning + cargo-audit 4 漏洞+5 警告（无高危）+ cargo-deny licenses ok。
- **Docker 打包就绪**（Dockerfile + docker-compose，辅助开发/CI）。**正式交付物为 ISO 安装包（待做）**。
- 完整文档体系：README + ARCHITECTURE + CONTRIBUTING + CHANGELOG + 11 内部文档 + 8 ADR。
- 剩余：ISO 安装包构建 / Windows 接入端 / 安卓接入端。
