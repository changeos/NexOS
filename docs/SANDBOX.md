# 真实环境测沙箱方案（SANDBOX）

> 目的：为 `osd`（cgroup/systemd/NTP）/`os-storage`（ZFS）/`os-network`+`os-guest`
> （nftables）/`os-compute`（libvirt/KVM）等**需 root + 特定内核能力**的操作提供
> "可真实跑、可重复、不污染宿主"的测试沙箱方案。
>
> 现状：这些 crate 的逻辑正确性已由 **fixture / mock / 内存后端**覆盖（见各 agent
> `PROGRESS.md` 的"阻塞项"段）；本文件补的是**真实环境集成测**这最后一层。
>
> 关联：
> - 规划文档 §8「工程与质量保障」、§4「关键技术风险与权衡」（网络组件 root 风险行）。
> - `docs/agents/devops-agent.md` §9 🟡「发布打包涉及 root/系统操作须沙箱运行」。
> - `docs/HANDOVER.md`「真实 root 环境测」段（FFI + cgroup + systemd + nftables）。
> - 各 agent `PROGRESS.md` 的「阻塞项」段（标 ⛔ 的 root/沙箱 TODO）。
>
> **红线**：本方案**不修改任何源码**——只加 `docs/` + `scripts/sandbox/`。
> 被测代码已就绪（feature 门控 / 内存后端注入点已埋），沙箱只是"提供运行环境"。

---

## 0. TL;DR

| 方案 | 适合跑 | 启动开销 | 宿主风险 | 推荐场景 |
|------|--------|----------|----------|----------|
| **A. Docker（privileged + systemd + cgroup v2）** | osd cgroup/NTP、os-guest/os-network nftables（单 netns）、os-storage ZFS-on-loop、os-compute libvirt `test:///default` | 低（秒级） | 中（privileged） | **首选**：日常 PR 沙箱 CI、本地复现 |
| **B. QEMU/KVM 嵌套虚拟化** | os-compute 真实 KVM 域、os-iso 裸机 install、os-provision PXE/分区/建池、A/B 槽首启 | 高（分钟级） | 低（完全隔离） | 发版前完整回归、裸机路径 |
| **C. systemd-nspawn** | osd systemd unit 真实拉起、cgroup v2、nftables（容器内独立 netns） | 低（秒级） | 低-中 | systemd PID1 行为复现、轻量于 Docker-systemd |

> **优先级**：先落 **方案 A**（覆盖 80% root 阻塞项），**方案 B** 补裸机/嵌套虚拟化
> 路径，**方案 C** 作为 systemd 真实行为的轻量替代（可选）。

---

## 1. 为什么需要沙箱

下列操作在单元测/CI 环境**无法真跑**（非 root、无内核设施、或会污染宿主），
当前**全部用 fixture / mock / 内存后端**覆盖逻辑，真实环境集成测缺失：

| Crate | 真实操作 | 权限/设施需求 | 当前替代 |
|-------|----------|---------------|----------|
| `osd` (`cgroup.rs`) | 真写 `/sys/fs/cgroup/os/<id>` 的 `cpu.max`/`memory.max` | root + CAP_SYS_ADMIN + cgroup v2 unified 挂载 | `InMemoryCgroupBackend`（HashMap 往返） |
| `osd` (`impl_orchestrator.rs`) | 真实 systemd unit 生成 + 进程监管 + 退避重启 | root + systemd PID1 + CAP_SYS_ADMIN | `TODO(集成阶段)` 占位（状态机可用） |
| `osd` (`ntp.rs`) | 编排 chrony 同步系统时钟 | root + CAP_SYS_TIME + chrony 守护 | `NtpManager` trait + 模型就绪，`ChronyNtp` 未实现 |
| `os-storage` (`backend_impl.rs`) | `zpool create/destroy`、`zfs create/destroy/snapshot`、send-recv 复制 | root（写操作）+ ZFS 内核模块 + loop 设备 | fixture `CommandRunner`（注入假 zpool/zfs stdout） |
| `os-storage` (`block_impl.rs`) | iSCSI target（LIO/configfs）、NVMe-oF（nvmet/configfs） | root + configfs 挂载 | 占位 |
| `os-network` (`backend.rs`) | `ip link set` / 路由 / VLAN / 桥（rtnetlink） | root + CAP_NET_ADMIN | 命令构造层就绪，执行层 `TODO(netlink-exec)` |
| `os-network` / `os-guest` | nftables 规则事务（`nftnl-ffi` feature） | root + CAP_NET_ADMIN + `libnftnl-dev`/`libmnl-dev` | `nftnl_apply_statements` 占位返回 `Err` |
| `os-compute` (`impl_vm.rs`) | 真实 libvirt/KVM 域生命周期 | root 或 libvirt 组 + `libvirt-dev` + libvirtd | `virt-ffi` feature 门控；`test:///default` fixture |
| `os-compute` (`oci.rs`/`cni.rs`) | youki 容器 + CNI 网络 | root + youki + CNI 插件 | OCI spec 构造真实，运行时骨架 |
| `os-iso` (`installer.rs`) | 真实裸机分区/建池/首启钩子 | root + 块设备 + 嵌套虚拟化 | 命令构造骨架，执行 `TODO` |
| `os-provision` | PXE 自举、阶段化迁移、块设备分区 | root + 网络 + 嵌套虚拟化 | mock 实现 |
| `os-update` (`impls.rs`) | A/B 槽 activate（写 bootloader/ostree）+ 首启 | root + bootloader + 重启 | `SlotManager` 决策纯逻辑已测，`activate_slot` 占位 |

**风险**：fixture 测只证明"逻辑对"，不证明"在真内核上跑得通"。例如：
- `cpu.max` 写入格式错误 → 真实 cgroup 拒收，fixture 察觉不到。
- nft 规则集事务顺序错 → 真实 netlink 回滚，fixture 察觉不到。
- ZFS 命令 stderr 关键词漏识别 → 真实 zpool 失败被误判成功。

沙箱的目标：**把上述"真内核 / 真守护"行为纳入回归**，同时**不污染开发者宿主**。

---

## 2. 方案 A：Docker 容器（首选）

### 2.1 思路

用一个 **Ubuntu 26.04 + systemd + cgroup v2 + privileged** 的镜像，在容器内：
- 以 **root** 跑（容器内 root ↔ 宿主 uid 映射；privileged 容器直接拥有内核设施）。
- 挂载宿主 `/sys/fs/cgroup`（cgroup v2 unified）或容器内 systemd 自建 cgroup 树。
- 装 `zfsutils-linux` / `libvirt-clients`+`libvirt-dev` / `libnftnl-dev`+`libmnl-dev`。
- 起 systemd 作为 PID1（复用 [`jrei/systemd-debian`]/[`geerlingguy/docker-ubuntu26.04-ansible`]
  类镜像思路，或基于 `ubuntu:26.04` 自建——本方案用自建 Dockerfile）。

### 2.2 优劣

**优点**
- 启动秒级，可重复（CI 友好）。
- 镜像可版本锁定（Ubuntu 26.04 + 工具链版本固定）。
- privileged + cgroup v2 挂载即可覆盖 osd/os-storage/os-network/os-guest 真实路径。
- 与 GitHub Actions `services` 或自建 runner 兼容（见 §6 集成 CI）。

**缺点 / 边界**
- **privileged 容器对宿主有风险**（容器内 root ≈ 宿主 root）——只能在受控 CI runner /
  开发者隔离机跑，**不得在共享开发机直接跑**（推荐用 disposable VM 或 dev loop 机）。
- **KVM 嵌套受限**：容器内跑真实 KVM 域需 `--device /dev/kvm` + 宿主支持嵌套虚拟化；
  libvirt `test:///default` 驱动（无真实 KVM）可跑，但不算"真实 VM"。**完整 KVM 走方案 B**。
- **systemd-in-Docker** 需特殊启动（`--privileged` + `/sbin/init` + cgroup 挂载）；
  部分 CI runner（如 GitHub-hosted）的 Docker daemon 限制 systemd 启动，需用自建 runner。
- **ZFS 在容器内**：`zpool create` 需 ZFS 内核模块已加载（宿主加载即可，容器用）+ loop 设备
  权限；`zfs` 模块在 Ubuntu 26.04 宿主默认有。loop 设备需 `--cap-add SYS_ADMIN` 或 privileged。

### 2.3 适用 crate / 测试

| Crate | 能在方案 A 跑 | 备注 |
|-------|---------------|------|
| `osd` cgroup (`CgroupsRsBackend`) | ✅ | privileged + cgroup v2 挂载即可真写 |
| `osd` systemd unit | ✅ | 容器内 systemd 作 PID1 |
| `osd` NTP (`ChronyNtp`，待实现) | ⚠️ | 需容器内 chrony + `CAP_SYS_TIME`；改系统时可能影响容器外（privileged），谨慎 |
| `os-storage` ZFS-on-loop | ✅ | privileged + 宿主加载 zfs 模块 |
| `os-storage` LIO/nvmet configfs | ⚠️ | configfs 挂载 + 内核 target 模块；CI 可能不稳，建议方案 B |
| `os-network` rtnetlink | ✅ | privileged + CAP_NET_ADMIN，独立 netns |
| `os-guest` nftables (`nftnl-ffi`) | ✅ | 装 `libnftnl-dev`/`libmnl-dev` + CAP_NET_ADMIN |
| `os-compute` libvirt `test:///default` | ✅ | 装 `libvirt-dev`，无 KVM 也能跑 test 驱动 |
| `os-compute` 真实 KVM 域 | ❌ | 走方案 B（嵌套虚拟化） |
| `nettest` 真实 mDNS/TLS/HTTP | ✅ | 容器内有 loopback + 可选公网 |

### 2.4 所需环境配置（宿主）

```sh
# 1. 宿主加载 ZFS 模块（ZFS 测需要）
sudo modprobe zfs || true   # Ubuntu 26.04 通常已加载

# 2. 确认 cgroup v2 unified（多数现代 Linux 默认）
mount | grep cgroup2        # 应见 /sys/fs/cgroup type cgroup2

# 3. Docker（或 podman --privileged 等价）
docker version
```

### 2.5 镜像构建与运行（详见 `scripts/sandbox/docker/`）

```sh
# 构建沙箱镜像（首次 ~5-10 min，后续命中缓存秒级）
docker build -t os-sandbox:26.04 -f scripts/sandbox/docker/Dockerfile.test scripts/sandbox/docker

# 把仓库挂进容器 + privileged + systemd 作 PID1 + cgroup 挂载
docker run --rm -it \
  --privileged \
  --cgroupns=host \
  -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
  -v "$PWD:/workspace:rw" \
  -w /workspace \
  os-sandbox:26.04

# 容器内（systemd 已起）：
/workspace/scripts/sandbox/docker/run-tests.sh
```

`run-tests.sh` 会：
1. 跑常规三道门（`cargo check/clippy/test --workspace --features mock`）。
2. 跑 **真实环境集成测**：`cargo test --workspace --features mock -- --ignored`（被
   `#[ignore]` 标的真实环境测，见 §5）。
3. 针对性跑 root 路径：`cargo test -p osd --features mock cgroup::tests::real_backend`
   等（待源码侧加 `#[ignore]` 集成测后启用；本任务不动源码）。

---

## 3. 方案 B：QEMU 嵌套虚拟化（完整环境）

### 3.1 思路

在支持嵌套虚拟化的宿主（Intel VT-x / AMD-V 嵌套开）上，用 `qemu-system-x86_64`
启一个完整 Ubuntu 26.04 VM，VM 内**就是真实裸机环境**——有真 KVM（嵌套）、真 systemd、
真 ZFS 模块、真 cgroup、真 bootloader。CI 用 cloud-init 注入测试脚本。

### 3.2 优劣

**优点**
- **完全隔离**（VM 失败不影响宿主），可跑 `os-iso` 裸机 install、`os-provision` PXE
  自举、`os-update` A/B 槽首启等**会动块设备/bootloader**的高危路径。
- **真 KVM**：`os-compute` 真实域生命周期（`virConnectOpen("qemu:///system")` →
  `virDomainCreateXML` → 真实 KVM 跑）可在 VM 内真跑。
- 行为最接近生产（真 PID1 systemd、真引导）。

**缺点**
- **启动慢**（分钟级），CI 时间成本高 → 适合**夜间回归 / 发版前**，而非每 PR。
- **嵌套虚拟化并非所有 CI runner 支持**（GitHub-hosted runner 不支持嵌套 KVM；
  需自建 bare-metal runner 或用支持嵌套的云机型如 GCP nested-VPC、AWS `.metal`）。
- cloud-init / 镜像构建链复杂（维护成本高）。

### 3.3 适用 crate / 测试

| Crate | 在方案 B 跑 | 备注 |
|-------|-------------|------|
| `os-compute` 真实 KVM（`virt-ffi` 全路径） | ✅ | 唯一能真跑 KVM 域的方案 |
| `os-iso` 真实 install（分区/建池/首启钩子） | ✅ | 给 VM 挂空块设备，install 写它 |
| `os-provision` PXE 自举 + 阶段化迁移 | ✅ | VM 间 PXE/VLAN 网络 |
| `os-update` A/B 槽 activate + 首启 | ✅ | 真实 bootloader + 重启 VM 验证槽位切换 |
| `osd` / `os-storage` / `os-network` 全部 | ✅ | VM 即真机，所有方案 A 项都能跑 |

### 3.4 所需环境配置

详见 `scripts/sandbox/qemu/README.md`。要点：
- 宿主 BIOS 开 VT-x/AMD-V + 嵌套虚拟化（`cat /sys/module/kvm_intel/parameters/nested` = `Y`）。
- 宿主装 `qemu-system-x86_64` + `ovmf` + `cloud-image-utils`（`cloud-localds`）。
- 拉 Ubuntu 26.04 cloud image（`qcow2`），用 cloud-init 注入 SSH key + 测试脚本。

---

## 4. 方案 C：systemd-nspawn（轻量容器 + cgroup）

### 4.1 思路

`systemd-nspawn` 比 Docker 更接近"轻量 VM"——容器**以 systemd 作 PID1 启动**，
有完整 cgroup 树、可独立 netns、可用 `--bind` 挂目录、`--capability=CAP_NET_ADMIN`
加权限。适合"想跑真实 systemd 行为但不想跑完整 VM"的场景。

### 4.2 优劣

**优点**
- 启动秒级（比 QEMU 快）、隔离比 Docker-systemd 更"原生"（nspawn 就是为 systemd 设计）。
- systemd PID1 行为最真实（unit 生成/journal/cgroup 绑定），适合 osd systemd 路径。
- 可 `--private-network` 起独立 netns（nftables 误配不会断宿主网）。

**缺点**
- **不是通用 CI 标准**（Docker 是 CI 事实标准；nspawn 在 GitHub Actions 不原生）。
- 仍需 root（启动 nspawn 容器）+ 宿主 cgroup 协作；不如 Docker 镜像可移植。
- **不支持嵌套 KVM**（同 Docker）——真实 KVM 仍走方案 B。

### 4.3 适用 crate / 测试

适合**单独验证 systemd 行为**：osd `SystemdOrchestrator` 真实 unit 生成 + 进程监管 +
退避重启（在 Docker-systemd 行为可疑时用 nspawn 复核）。其余能力 ≈ 方案 A。

### 4.4 所需环境配置

```sh
# 宿主：debootstrap 一个最小 rootfs（或解压 Ubuntu 26.04 cloud rootfs）
sudo debootstrap noble /var/lib/machines/os-sandbox http://archive.ubuntu.com/ubuntu/
# 装 ZFS / libvirt-dev / libnftnl-dev 等同方案 A
# 启容器
sudo systemd-nspawn -D /var/lib/machines/os-sandbox \
  --capability=CAP_NET_ADMIN,CAP_SYS_ADMIN \
  --bind="$PWD:/workspace" \
  /workspace/scripts/sandbox/docker/run-tests.sh
```

---

## 5. 应入沙箱的 `#[ignore]` / root 路径测试清单

> 原则：**`#[ignore]` 标的真实环境测 + 当前 fixture 覆盖的 root 路径**，都应在沙箱跑。
> 列名"现状"=本任务调研时（main `4dffb46`）的状态；"建议方案"=入哪个沙箱。

### 5.1 已有 `#[ignore]` 测试（直接进沙箱 `--ignored` 跑）

| 测试 | 文件 | 验证 | 建议 |
|------|------|------|------|
| `reqwest_real_get` | `crates/nettest/tests/reqwest_real.rs` | 真实公网 HTTPS GET | A（需公网出口）/ B |
| `axum_real_listen_and_get` | `crates/nettest/tests/axum_real.rs` | 真实 axum 监听 + reqwest 请求（loopback） | A（无公网也行） |
| `mdns_real_broadcast` | `crates/nettest/tests/mdns_real.rs` | 真实 mDNS 组播 + browse | A（需组播） |
| `rustls_real_tls_handshake` | `crates/nettest/tests/rustls_real.rs` | rcgen 自签 + rustls TLS 握手（loopback） | A |
| `zfs_real_smoke` | `crates/nettest/tests/zfs_real.rs` | 真实 `zfs --version` + `zpool list`（二进制 + 内核模块可用性） | A |
| `real_sparse_file_pool_dataset_snapshot_lifecycle` / `real_pool_exists_error_classification` | `crates/os-storage/tests/real_zfs_ops.rs` | ZfsCliBackend 真实 spawn zpool/zfs：sparse file vdev 建池 → dataset → snapshot → destroy 全链 + PoolExists 错误分类 | A — ✅ **本机实跑验证通过**（sparse file vdev，2025-08）；`sudo cargo test -p os-storage --features mock --test real_zfs_ops -- --ignored` |
| `real_xorriso_minimal_iso_build` | `crates/os-iso/tests/real_xorriso_build.rs` | 真实 mksquashfs + xorriso 产最小数据 ISO + ISO9660 魔数 + sha256 | A（无特权；xorriso/squashfs-tools） |
| `real_xorriso_clone_style_iso_build` | `crates/os-iso/tests/real_xorriso_build.rs` | 克隆变体风格 rootfs（含 JSON 快照）打包成 ISO | A |
| `real_xorriso_bios_only_boot_iso` | `crates/os-iso/tests/real_xorriso_build.rs` | **BIOS-only** El Torito 真实构建（`-b ... -boot-info-table -boot-load-size 4 -no-emul-boot`）+ 深度验证 | A |
| `real_xorriso_bios_uefi_dual_boot_iso` | `crates/os-iso/tests/real_xorriso_build.rs` | **BIOS+UEFI 双启**（默认 BootConfig，含 `-eltorito-alt-boot -e efi.img`）+ 深度验证 | A |
| `real_xorriso_uefi_only_boot_iso` | `crates/os-iso/tests/real_xorriso_build.rs` | **UEFI-only**（仅 `-eltorito-alt-boot -e efi.img`，覆盖 aarch64 场景）+ 深度验证 | A |
| `real_xorriso_builder_e2e_real_spawn` | `crates/os-iso/tests/real_xorriso_build.rs` | `XorrisoIsoBuilder::build` 真实 TokioIsoRunner spawn mksquashfs（验证命令派生 + 真实工具链集成） | A |

> **iso 项本机多架构实跑验证通过**（2026-08，xorriso 1.5.6 + mksquashfs 4.5.1，Ubuntu 24.04 开发机）：
> 三种启动模式均产出可启动 ISO，每架构产物大小（深度验证通过：`xorriso -ls /` 列出
> `casper/filesystem.squashfs`、`file(1)` 识别 `ISO 9660 CD-ROM filesystem data ... (bootable)`、
> El Torito 引导记录存在、sha256 非空 64-hex）：
> - **minimal**（无 El Torito）：380 928 B
> - **BIOS-only**：1 908 736 B（El Torito BIOS boot img `/boot/grub/i386-pc/eltorito.img`，boot-info-table）
> - **BIOS+UEFI 双启**：1 908 736 B（双引导 img：BIOS + UEFI `/boot/efi.img`）
> - **UEFI-only**：1 908 736 B（单 UEFI 引导 img `/boot/efi.img`）
>
> 重跑：`cargo test -p os-iso --features mock --test real_xorriso_build -- --ignored --nocapture`
> （测前 `IsoEnvironment::probe()` 探测，缺工具则清晰 panic 跳过；产物落 `/tmp`，测后清。）
>
> **已知限制（不在本批修）**：`XorrisoIsoBuilder::build` 的 `source_dir = output_root/<task_id>/tree`，
> `task_id` 在 build 内部 `TaskId::new()` 派生（UUID v4），调用方无法预填——故 `build` 真实
> 跑会在 mksquashfs 阶段失败。本批用 `real_xorriso_builder_e2e_real_spawn` 验证真实 spawn 行为
> （失败传播含 "mksquashfs"），完整产物构建由上方三个 multi-boot 测覆盖（命令行与
> `cli.rs::xorriso_build_args` 一致）。后续 PR 可让 source_dir 由调用方指定以解锁完整 build E2E。
| `cgroup_real_*`（apply/read/update/delete） ✅ **本机实跑验证通过（2026-08-06）** | `crates/osd/tests/cgroup_real.rs` | 真实 cgroup v2 写 `/sys/fs/cgroup/osd_test_*/cpu.max`+`memory.max`，读回一致，更新，删除 | A（root + cgroup v2） |
| `ntp_real_*`（sync_status/sources/dry_run/probe） ✅ **本机实跑验证通过（2026-08-06）** | `crates/osd/tests/ntp_real.rs` | 真实 chronyc tracking/sources 解析 + dry-run 命令构造 + 二进制探测 | A（只读非 root 可跑，sudo 兜底） |
| `systemd_real_*`（reachable/transient_lifecycle/transient_long_running/state_machine） ✅ **本机实跑验证通过（2026-08-06）** | `crates/osd/tests/systemd_real.rs` | systemd 可达性 + transient unit 生命周期（oneshot + 长跑进程）+ 状态机框架锚点 | A（transient unit 需 root） |
| `block_real_*`（命令构造，默认跑）+ configfs 可达性（`#[ignore]`） ✅ **命令构造测跑通（2026-08-06）** | `crates/os-storage/tests/block_real.rs` | iSCSI/NVMe-oF targetcli/nvmetcli 命令构造正确性（默认跑）+ configfs 真实可达性（本机无 LIO/nvmet，SKIP） | A（命令构造无特权；真实 export 需 B+configfs） |
| `nftnl_real_*`（apply/revoke/rollback_checkpoint） ✅ **本机实跑验证通过（2026-08-06）** | `crates/os-guest/tests/nftnl_real.rs`（`nftnl-ffi` feature） | os-guest `NftRuleOrchestratorImpl` 真实 nft 事务：set 元素 + timeout + `tcp dport 445 accept` 规则，apply/revoke/rollback 全链 | A（root + CAP_NET_ADMIN + libnftnl-dev + libmnl-dev） |
| `rtnetlink_real_*`（link/addr/route/get_by_name/genetlink） | `crates/nettest/tests/rtnetlink_real.rs` | 真实 rtnetlink 只读 dump + genetlink family 枚举 | **A**（非 root 多数可读，sudo 兜底） |
| `rtnetlink_real_dummy_crud` | `crates/nettest/tests/rtnetlink_real.rs` | 真实 rtnetlink 写路径：dummy 网卡创建/验证/删除 | **A**（需 root + CAP_NET_ADMIN，sudo 跑） |
| `nftnl_real_smoke` ✅ **本机实跑验证通过（2026-08-05）** | `crates/nettest/tests/nftnl_real.rs`（`nftnl-ffi` feature） | 真实 nftables netlink 事务：NEW 表 `inet nettest_real` + input 链（hook input, policy accept）+ 规则 `iif lo accept` → ACK → 删表清理 | A（root + CAP_NET_ADMIN + libnftnl-dev + libmnl-dev） |
| `nftnl_real_table_chain_rules` ✅ **本机实跑验证通过（2026-08-05）** | `crates/nettest/tests/nftnl_real.rs`（`nftnl-ffi` feature） | 真实表/链/规则事务：NEW 表 `inet osnettest` + 链 + `iif lo accept` + `tcp dport 22 accept` → `nft list table` 回读断言 → 删表清理 | A（同上） |
| `nftnl_add_rule_real` ✅ **本机实跑验证通过（2026-08-05）** | `crates/os-network/src/nftnl_real.rs`（`nftnl-ffi` feature） | os-network `NftnlFirewallBackend::add_rule` 真实提交路径 + teardown 删 default 表 `inet filter` | A（同上） |

> **本机验证命令**（Ubuntu 26.04，dev 包经 pool 直装：`libnftnl-dev 1.3.1-1` + `libmnl-dev 1.0.5-3build1`）：
> ```bash
> sudo apt-get install -y libnftnl-dev libmnl-dev   # 或 pool 直装（见 §5.3）
> sudo cargo test -p nettest --features nftnl-ffi --test nftnl_real -- --ignored --nocapture
> sudo cargo test -p os-network --features nftnl-ffi --lib nftnl_ -- --ignored --nocapture
> ```
> 测试均自动 teardown（删表），`nft list tables` 无残留。

### 5.2 当前无 `#[ignore]` 但**应补真实环境测**的 root 路径（本任务不动源码，仅登记）

> 这些是各 `PROGRESS.md` 标 ⛔ 的"留待沙箱"项。后续由对应 owner agent 在源码侧加
> `#[ignore]` 集成测（**不在本任务范围**——本任务只搭沙箱骨架），届时直接进沙箱跑。

| 路径 | crate / 文件 | 真实测要点 | 建议 |
|------|--------------|------------|------|
| `CgroupsRsBackend` 真写 | `osd/src/cgroup.rs` | 写 `/sys/fs/cgroup/os/<id>` 的 `cpu.max`/`memory.max`，回读一致 | **A**（privileged + cgroup v2） — ✅ **本机实跑验证通过（2026-08-06）**：`crates/osd/tests/cgroup_real.rs` 4 测 `#[ignore]`（apply 写/读回/更新/删除），本机 cgroup2fs + root 跑绿。发现 cgroup v2 内核对 `memory.max` 做 PAGE_SIZE 对齐（写 100MB 实存 99999744，非实现 bug）。RAII Drop 用同步 remove_dir 规避嵌套 runtime panic。 |
| `SystemdOrchestrator` 真实 unit + 监管 + 退避 | `osd/src/impl_orchestrator.rs`（`do_start_inner`/`do_stop_inner`，标 `TODO(集成阶段)`） | 生成 unit、`systemctl start`、kill 后退避重启 | A 或 **C**（nspawn 复核 systemd 行为） — ✅ **可达性 + transient unit 测本机实跑通过（2026-08-06）**：`crates/osd/tests/systemd_real.rs` 4 测 `#[ignore]`（systemd 可达性 + transient unit 生命周期 oneshot + 长跑进程 + 状态机框架验证），本机 systemd 259 跑绿。**确认 start/stop 仍是纯状态机框架**（无 systemctl 调用，TODO 集成阶段），未改实现，留接通锚点。 |
| `ChronyNtp`（待实现） | `osd/src/ntp.rs` | 编排 chrony 同步、读 tracking 状态 | A（CAP_SYS_TIME 谨慎）/ B — ✅ **只读编排测本机实跑通过（2026-08-06）**：`crates/osd/tests/ntp_real.rs` 4 测 `#[ignore]`（chronyc tracking 真实解析 + sources + dry-run 命令构造 + 二进制探测），本机 chrony 4.8 跑绿。真实 Stratum 3/Leap Normal/offset 3ms 解析正确。发现 Ubuntu 用 `sourcedir` 指令（server 在 sources.d/），非 bug。 |
| `ZfsCliBackend` 真实 zpool/zfs | `os-storage/src/backend_impl.rs` + `crates/os-storage/tests/real_zfs_ops.rs` | loop/sparse-file 建池、create/destroy/snapshot/send-recv | **A**（ZFS-on-loop） — ✅ **本机实跑验证通过**（sparse file vdev）：`truncate -s 1G /tmp/x.img` + `zpool create` 建池 → `zfs create ds` → `zfs snapshot ds@snap1` → list/destroy 全链，断言 `Pool/Dataset/Snapshot::from_list_line` 解析 + `PoolExists` 错误分类均正确。测自建自毁临时池（`osprobe_*`，RAII guard teardown），`#[ignore]`。跑法：`sudo cargo test -p os-storage --features mock --test real_zfs_ops -- --ignored --nocapture`（OpenZFS 2.4.1 / Ubuntu 26.04 `resolute`，2025-08 验证）。注：复用同一 sparse vdev 报 `invalid vdev`（→ `InvalidVdev`），触发 `PoolExists` 需同池名 + 不同 vdev。 |
| `block_impl` LIO/nvmet configfs | `os-storage/src/block_impl.rs` | iSCSI target / NVMe-oF namespace export | **B**（configfs 模块依赖，CI 不稳） — ✅ **命令构造测本机跑通（2026-08-06）**：`crates/os-storage/tests/block_real.rs` 6 命令构造测默认跑绿（iSCSI backstore→target→lun→portal 顺序 + NVMe-oF subsystem/namespace + destroy 逆操作）+ 3 configfs 真实可达性测 `#[ignore]`（本机无 LIO/nvmet 子系统，优雅 SKIP）。**修 2 bug**：①`export_iscsi` 缺 backstore 创建（引用未创建的 backstore + 语法错）②`unexport` 非真逆操作（不删 backstore）。真实 export 待 configfs+target_core_mod 环境。 |
| rtnetlink 真实接口 up/down/VLAN/桥 | `os-network/src/backend.rs`（`TODO(netlink-exec)`） | `ip link set` 真实生效 | **A**（独立 netns） |

> **✅ 本机实跑验证通过（2026-08，feature/netlink-real worktree）**：
> `crates/nettest/tests/rtnetlink_real.rs` 已扩到 link/addr/route/get_by_name 只读 + dummy
> 网卡 CRUD 写路径 + genetlink family 枚举，共 6 个 `#[ignore]` 测。`sudo cargo test -p nettest
> --test rtnetlink_real -- --ignored` 全绿（6 passed）。真实输出：link dump 3 接口
> （lo/enp131s0/wlp132s0）、addr dump 含 lo 127.0.0.1/8、route dump 11 条含 main 表 default
> via 192.0.2.1、get_by_name("lo")→ifindex=1、dummy CRUD（创建 osdummy→验证存在→
> 删除→验证消失，测后无残留）、genetlink family dump 25 个含 nlctrl + nl80211（有 wifi）。
> 写路径（dummy CRUD）需 root，用 sudo 跑；只读测非 root 也能跑。
| `NftRuleOrchestratorImpl` 真实 nft 事务 | `os-guest`（`nftnl-ffi` feature）+ `os-network` | `nftnl::Batch` + `mnl` crate 真提交 + dry-run 回滚。✅ **os-network `NftnlFirewallBackend` + os-guest `NftRuleOrchestratorImpl` 双双本机实跑验证通过（2026-08-06）** | **A**（装 libnftnl-dev） |

> **✅ os-guest nftnl-ffi 真实落地本机实跑验证通过（2026-08-06，feature/guest-nftnl worktree）**：
> `crates/os-guest/src/impls.rs::nftnl_apply_statements` 从占位 `Err` 换为真实实现
> （nftnl 0.7 `Batch` + 自定义 `SetElemMsg` 设 timeout + `nft_expr!` 构造规则，经 `mnl::Socket` 提交内核）。
> `crates/os-guest/tests/nftnl_real.rs` 3 测 `#[ignore]`（apply 真实提交 + revoke + rollback_checkpoint），
> 本机 libnftnl 1.3.1 + libmnl 1.0.5 + root 跑绿。apply 后 `nft list` 实测：set 元素 timeout 1h 真实生效、
> `tcp dport 445 accept` 规则真实生效。**修 2 真实 bug**：①`nft_expr!` payload 宏 3-token 误用
> （nftnl 0.7 只接 2-token）②dport cmp 字节序 bug（nftnl `ToSlice for u16` 写 little-endian，dport 是
> big-endian，端口 445 被错写为 48385→改传 `u16::to_be_bytes()`）。RAII Drop 测后自动删表，宿主无残留。
> 已知限制：多端口匹配降级为只匹配第一个端口（`TODO(nftnl-multiport)`）。
| `LibvirtVmManager` 真实 KVM | `os-compute/src/impl_vm.rs`（`virt-ffi` feature） | `virConnectOpen("qemu:///system")` → 域生命周期 | **B**（唯一真 KVM）；`test:///default` fixture 在 A 即可 |
| `YoukiRuntime` / `CniContainerNetwork`（待批 3） | `os-compute/src/{oci,cni}.rs` | youki 跑容器 + CNI 配网 | A（privileged 嵌套 youki） |
| `IsoBuilder`/`Installer` 真实 install | `os-iso/src/installer.rs`（`TODO`） | 真实分区/建池/首启钩子 | **B**（动块设备） |

> 注：`IsoBuilder` 的 **ISO 构建** 侧（xorriso + mksquashfs 产出可启动 ISO）已**本机多架构实跑
> 验证通过**（BIOS-only / BIOS+UEFI / UEFI-only 三模式，见 §5.1 iso 行）。**仅 install 侧**（裸机
> 分区/建池/首启）仍待沙箱 B 补真实测。
| `os-provision` PXE/分区 | `os-provision/src/*.rs` | PXE 自举、阶段化迁移 | **B** |
| `AbUpdateEngine::activate_slot` | `os-update/src/impls.rs`（`todo!()`） | 写 bootloader/ostree + 首启槽位切换 | **B**（动 bootloader + 重启） |

### 5.3 编译期 FFI 门控（沙箱需装系统库）

下列 feature 在沙箱**必须装系统 `-dev` 包**才能编译（`Dockerfile.test` 已覆盖）：

| Feature | 系统依赖 |
|---------|----------|
| `os-compute/virt-ffi` | `libvirt-dev`（提供 `libvirt.so` + 头文件） |
| `os-network/nftnl-ffi`、`os-guest/nftnl-ffi`、`nettest/nftnl-ffi` | `libnftnl-dev` + `libmnl-dev`（pkg-config 找 `.pc`） |

> 当前 CI/开发机只有运行时 `.so`（缺 `-dev` 头），故 `cargo check --features virt-ffi`
> / `--features nftnl-ffi` 失败属**预期门控行为**；沙箱镜像装 `-dev` 后即可开。
>
> **装 `-dev` 包**：标准 `sudo apt-get install -y libnftnl-dev libmnl-dev`。若镜像源
> 暂未索引 `-dev`（如 Ubuntu 26.04/resolute 早期仅含 `libnftnl11`/`libmnl0` 运行时），
> 从 Ubuntu pool 直装对应版本 .deb（与已装运行时版本严格匹配，避免 ABI 漂移）：
> ```bash
> # 已装：libnftnl11 1.3.1-1 / libmnl0 1.0.5-3build1 → 装同版本 -dev
> curl -o /tmp/libnftnl-dev.deb http://archive.ubuntu.com/ubuntu/pool/main/libn/libnftnl/libnftnl-dev_1.3.1-1_amd64.deb
> curl -o /tmp/libmnl-dev.deb   http://archive.ubuntu.com/ubuntu/pool/main/libm/libmnl/libmnl-dev_1.0.5-3build1_amd64.deb
> sudo dpkg -i /tmp/libnftnl-dev.deb /tmp/libmnl-dev.deb
> ```

### 5.4 nftnl FFI 路径修正（2026-08-05 实跑时发现并修复）

实跑 `--features nftnl-ffi` 时发现 `crates/os-network/src/nftnl_real.rs::send_batch`
误用 `nftnl::nftnl_sys::mnl_socket::*`——**该路径在 nftnl 0.7 不存在**（nftnl 的 re-export
仅含 `nftnl_sys` 的 libnftnl FFI 绑定，**不 re-export mnl**），导致 `cargo build
--features nftnl-ffi` 失败。同期还发现一批 nftnl 0.6→0.7 API 漂移（`Table::new`/
`Chain::new` 取 `CString`、`Rule::new` 只接 chain、`ProtoFamily::Inet6/Netdev`→
`Ipv6/NetDev`、`Expression::Meta(...)` 把 trait 当 enum 用、`Verdict::Redirect` 不存在）。

**修正**（仅限 `nftnl_real.rs` FFI/API 路径，未动 trait 签名）：
- finalized batch 投递改走独立 `mnl` crate 高层封装（`Socket::new` / `send_all` /
  `recv` + `cb_run` 函数），与 `nettest::nftnl_real::send_and_process` **同源写法**；
- `os-network` 新增 `mnl = { workspace = true, optional = true }` 依赖（与 `nftnl`
  共 `nftnl-ffi` feature 门控，同 libmnl-dev FFI 依赖）；
- 表达式组装改用 `nftnl::nft_expr!` 宏（`meta l4proto` / `cmp == <u8>` /
  `verdict accept|drop`）；Redirect action 在 add_rule 路径返回 `Err`（nftnl 0.7 的
  `Verdict` enum 无 Redirect 变体——redirect 在 nft 是 nat 表达式 Redir，非 verdict，
  完整实现留 `TODO(nftnl-nat)`，与 `add_nat` 的 TODO 一致）。

> 注：`nettest/tests/nftnl_real.rs` 原有 smoke 测同样引用了不存在的 `socket_recv`/
> `CbRunner`（mnl 0.2 实际 API 是 `Socket::recv`/`cb_run`）和裸 `nft_expr!`（应为
> `nftnl::nft_expr!`）——故该测此前**从未编译过**。本次一并对齐 mnl/nftnl 0.2/0.7 正确
> API，并补 table/chain/rules + `nft list` 回读验证测（见 §5.1）。

---

## 6. 与现有 CI 的关系（`.github/workflows/ci.yml`）

现状 CI（`ci.yml`）只跑非 privileged 三道门：`cargo check/clippy/test --workspace --features mock`，
**不跑 `--ignored`、不开 FFI feature、不挂 cgroup**——保持 PR CI 快 + 不需特权。

**集成策略**（**本任务不实施**，仅规划；待 owner 决策后另起 PR）：
1. **保留现有 `ci.yml`** 作为每 PR 的快速门（fixture 测，无特权）。
2. **新增 `sandbox-ci.yml`**（可选，自建 runner）：
   - 触发：`workflow_dispatch` 手动 + 夜间 cron + 发版分支推送（**不每 PR**）。
   - runner：自建 bare-metal/支持嵌套虚拟化的 runner（GitHub-hosted 不支持 privileged systemd-in-Docker + 嵌套 KVM）。
   - job：`docker build` + `docker run --privileged` + `run-tests.sh`（含 `--ignored`）。
   - 失败：通知相关 owner（附容器日志），不阻断 PR 合并（与 `ci.yml` 三道门解耦）。
3. **QEMU 完整回归**（方案 B）：发版前手动 / 夜间跑，覆盖 KVM/iso/provision/update 高危路径。

> 红线：**沙箱 CI 不替代三道门**。三道门（fixture 测）保证"逻辑对 + 快反馈"；
> 沙箱测补"真内核上跑得通"。两者互补，不互替。

---

## 7. 维护责任

- **`scripts/sandbox/docker/Dockerfile.test` + `run-tests.sh`**：devops-agent 维护
  （CI 基础设施范畴，`docs/agents/devops-agent.md` §3 C4 构建脚本）。
- **`scripts/sandbox/qemu/README.md`**：devops-agent 维护（环境配置指引）。
- **被测源码 / `#[ignore]` 集成测**：各 owner agent 维护（osd→orchestrator-agent、
  os-storage→storage-agent、os-network/os-guest→network/guest-agent、
  os-compute→vm/container-agent、os-iso→iso-agent、os-provision→provision-agent、
  os-update→update-agent）。
- **本文件（`docs/SANDBOX.md`）**：devops-agent 维护；新增 root 阻塞项时各 owner
  agent 在 PR 描述里 @devops-agent 更新 §5 清单。

---

## 8. 不做（边界）

- ❌ **不动源码**（不加 `#[ignore]` 测、不改 trait、不真跑沙箱）——本任务只搭骨架 + 文档。
- ❌ **不推送 / 不合并**——只本地 commit。
- ❌ **不在共享开发机跑 privileged 容器 / 嵌套虚拟化**——只能在 disposable VM / 隔离机 / CI runner 跑。
- ❌ **不让沙箱测替代三道门**——见 §6。

## 9. 参考

- 规划文档：§4 风险行（网络组件 root / 第三方 deb 沙箱）、§8 测试策略。
- `docs/agents/{orchestrator,storage,vm,container,guest,iso,provision,update,security}-agent.md`
  各自的「依赖前置」「权限」段。
- 各 `docs/agents/*-agent/PROGRESS.md` 的「阻塞项」段（⭐ 本文件 §5 的来源）。
- `docs/HANDOVER.md`「真实 root 环境测」段（FFI + cgroup + systemd + nftables 汇总）。
- `docs/DEPENDENCIES.md` §2.3（osd 阻塞）、`docs/adr/ADR-DEPS-002` §91（nftnl FFI 前置）。
