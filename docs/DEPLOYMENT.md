# OS 系统部署指南（DEPLOYMENT）

> **本文目标**：把 22 crate 的 Rust workspace 从源码组装成一台可运行的 OS 系统——覆盖构建、`osd` 启动编排、配置、systemd 集成、ISO 安装、HA 集群、升级/回滚、监控接入的全流程。
>
> **关联文档**：
> - `OS_系统_Rust技术路线规划.md` §2（总体架构）、§3.11（os-iso）、§3.12（os-update）、§3.13（osd）、§3.19（排除清单 SSOT）
> - `docs/HANDOVER.md` —— 当前实现状态（哪些是真实实现、哪些是骨架）
> - `docs/SANDBOX.md` —— root/内核操作的沙箱测试方案
>
> **重要现状**（截至 2026-08-05；2026-08-20 增补 §9 实机部署现状）：22 crate 全部有实现（现 26 crate），1491 测试全绿（现 4,100+）；`osd` 的 cgroup v2 配额已接通真实 `cgroups-rs`，但**进程拉起（systemd 调用）/NTP** 仍是骨架（待 root 沙箱集成）；`os-iso` 安装器的真实裸机执行（分区/建池/写盘）留 TODO；`os-update` bootloader 激活留 TODO。本指南既描述**目标终态**也标注**当前阻塞点**，便于分阶段落地。
> **当前生产实跑形态是 §9（os-api systemd 服务）**，不是 §2–§4 的 osd 全家桶编排。

---

## 0. 目录

1. [构建流程](#1-构建流程)
2. [osd 启动编排](#2-osd-启动编排)
3. [配置](#3-配置)
4. [systemd 集成](#4-systemd-集成)
5. [ISO 安装流程](#5-iso-安装流程)
6. [HA 集群部署](#6-ha-集群部署)
7. [升级/回滚](#7-升级回滚)
8. [监控接入](#8-监控接入)
9. [实机部署现状（os-api.service，2026-08）](#9-实机部署现状os-apiservice2026-08)

---

## 1. 构建流程

### 1.1 构建环境

| 项 | 要求 |
|----|------|
| OS | Ubuntu 26.04 LTS（与运行时底座一致） |
| Rust | stable（开发机用 1.97；`rustup default stable`） |
| 系统依赖（运行时） | OpenZFS（`zfsutils-linux`）、KVM（`qemu-kvm` + `libvirt-daemon-system`）、`chrony`、`samba`、`xorriso`、`squashfs-tools` |
| 系统依赖（构建期） | `build-essential`、`pkg-config`、`libsqlite3-dev`（或用 `rusqlite/bundled`，已默认）、`libclang-dev`（nftnl rtnetlink FFI） |
| 可选（功能门控） | `libvirt-dev`（开启 `virt-ffi` 走真实 KVM；缺失则 os-compute 回退骨架） |

> **横切策略**：未注册的第三方依赖一律不擅自引入 workspace；按 `docs/agents/_conventions.md` 走 ADR 流程（ADR-DEPS-001/002 已注册 31 个依赖）。

### 1.2 编译命令

```bash
# 在仓库根目录（含 Cargo.toml workspace）
cargo build --release --workspace --features mock
```

- `--features mock`：解锁下游 Mock 注入路径，与 CI（`.github/workflows/ci.yml`）和 `make all` 保持一致。**生产构建可去掉 `mock`**（mock impl 仅 `#[cfg(feature = "mock")]` 编译，不进 release 产物）。
- 三个质量门（与 `Makefile` 一致）：
  - `make check` —— `cargo check --workspace --features mock --all-targets`
  - `make clippy` —— `cargo clippy --workspace --all-targets --features mock -- -D warnings`
  - `make test` —— `cargo test --workspace --features mock`

### 1.3 产出的二进制

`cargo build --release` 后，可执行文件落在 `target/release/`。当前 workspace 中**带 `main.rs` 的可执行 crate**包括（其余是 lib）：

| 二进制 | crate | 角色 |
|--------|-------|------|
| `osd` | `crates/osd` | PID1 后编排器，拉起全部业务组件 |
| `os-api` | `crates/os-api` | 内嵌 API 网关（axum REST + WebSocket），统一入口 |
| `os-cli` | `crates/os-cli` | 命令行管理工具（连接远端 os-api） |

> 其余 crate（`os-storage` / `os-network` / `os-meta` / `os-discover` / `os-services` / `os-security` / `os-provision` / `os-iso` / `os-update` / `os-im` / `os-protocols` / `os-compute` / `os-guest` / `os-wallet` / `os-common` / `os-core` / `os-i18n` / `os-integration` / `os-mobile` / `os-desktop` / `nettest`）当前以 **lib** 形式存在，由 `osd` 作为进程内组件或经 trait 注入编排。**终态目标**是把高频常驻服务（storage/network/meta/protocols/compute）拆为独立可执行进程，由 osd 经 `SystemdOrchestrator` 拉起（见 §2）。

### 1.4 部署目录结构

推荐的安装后目录布局（`os-iso` 安装器和 `os-provision` 迁移遵循同一骨架）：

```
/opt/os/                          # 系统本体（只读，A/B 槽位切换）
├── bin/
│   ├── osd                       # 编排器
│   ├── os-api                    # API 网关
│   ├── os-cli
│   └── (其余组件二进制，终态)
├── lib/                           # 共享 Rust lib（如有动态链接）
├── share/
│   ├── web-ui/                    # Vue 3 前端静态资源（dist）
│   ├── i18n/                      # os-i18n 翻译资源（zh-CN/zh-TW/en TOML）
│   └── defaults/                  # 默认配置骨架（系统配置模板）
│       ├── osd.toml
│       ├── components.toml
│       └── network.toml
└── versions/<semver>/             # 每版本独立目录，A/B 槽位软链到此

/etc/os/                          # 节点本地配置（首启生成 / 用户编辑）
├── osd.toml                      # 主配置（覆盖 share/defaults）
├── components.toml                # 组件注册表（依赖图 + 配额）
├── network.toml                   # 网络配置
├── os/                           # os-security 凭证（mode 0600）
│   ├── jwt-signing.key            # 首启生成（§3.19 排除项）
│   ├── ca.key + ca.crt            # 首启自签 CA（mTLS）
│   └── tls.key + tls.crt          # 首启生成 / ACME 签发
├── systemd/                       # osd 作为 PID1 时的子 unit
│   └── os-*.service
└── cluster/                       # HA 集群凭证（首启或 join 时生成）
    ├── openraft.key
    └── join-token

/var/lib/os/                      # 持久状态（数据卷外，入 root 池的本地数据集）
├── meta/                          # os-meta openraft 日志 + SQLite 状态机
├── api/                           # os-api 配置中心 SQLite
├── im/                            # os-im 对话历史 + agent 状态
└── monitor/                       # os-monitor 告警状态

/var/log/os/                      # 日志（按组件分文件，journald 也收）

/zpool/data/                       # 用户数据（ZFS 数据集，由用户建池后挂载）
```

**关键约束**：
- `/etc/os/os/` 与 `/etc/os/cluster/` 是 §3.19 排除清单项——ISO/克隆**绝不**携带，首启强制重生成（见 §3.4）。
- `/opt/os/` 与 `/etc/os/` 之外的目录由 `os-iso` 安装器建池后写入。

---

## 2. osd 启动编排

`osd` 是规划文档 §3.13 定义的 **PID1 之后的 OS 编排器**，统管 20+ Rust 组件 + Samba + QEMU + youki 的进程生命周期。本节描述它的启动流程、依赖图、cgroup 配额、健康检查机制。

### 2.1 启动顺序总览

```
PID 1 (systemd 或 osd 自身，见 §4)
  └─ osd (PID 2 量级)
       1. 解析配置：/etc/os/osd.toml + /etc/os/components.toml
       2. 载入 ComponentRegistry（HashMap<ComponentId, ComponentDescriptor>）
       3. 拓扑排序（topological_sort / Kahn 算法，crates/osd/src/topo.rs）
          ├── 检测循环依赖 → DependencyCycle 错误
          └── 产出有序启动列表
       4. 启动 NTP（chrony）—— §9.1#8 决策：最早启动，HA/ZFS/证书前置
       5. 按序拉起组件：每个组件
          ├── 创建 cgroup /sys/fs/cgroup/os/<id>（cgroups-rs，CgroupQuota）
          ├── 写入 cpu.max / memory.max / pids.max
          ├── 启动进程（systemd unit 或 exec）
          ├── 标记 ComponentStatus::Starting
          ├── 健康探针轮询（HealthProbeConfig）
          └── 首次探针通过 → ComponentStatus::Running；连续失败达阈值 → Failed + 退避重启
       6. 全部 Running → osd 进入监管循环（探针 + 自动重启 + 配额在线调整）
```

### 2.2 组件依赖图（默认）

依赖关系编码在 `ComponentDescriptor.dependencies`（`/etc/os/components.toml`）。默认依赖图（呼应 §2 总体架构分层，自底向上）：

```
chrony (NTP)              ← osd 直接编排，最早
  └─ os-storage          ← ZFS 池/数据集就绪（数据层）
       ├─ os-network     ← 接口/桥/防火墙就绪
       ├─ os-meta        ← openraft 共识（HA 时；单节点 Standalone 模式跳过）
       └─ os-security    ← CA/JWT 就绪（供下游签发）
            ├─ os-protocols   ← SMB/NFS/WebDAV/FTP/SFTP/S3
            ├─ os-compute     ← VM(KVM)/容器(youki)
            ├─ os-api         ← 内嵌网关（依赖前面全部就绪）
            ├─ os-im          ← IM + agent 中枢（依赖 os-api）
            ├─ os-services    ← backup/monitor/media/files/devtools/power（六子模块）
            ├─ os-guest       ← 访客 Portal（可选）
            └─ os-discover    ← LAN 发现（独立，无强依赖）
```

> **同组件串行化**：`SystemdOrchestrator` 保证对**同一组件**的操作（start/stop/restart/set_quota）串行；跨组件可并发（见 `impl_orchestrator.rs` 注释）。

### 2.3 cgroup v2 资源配额

每个组件的 `ResourceQuota`（`os-core`）由 osd 翻译成 cgroup v2 文件写入（`crates/osd/src/cgroup.rs`）：

| `ResourceQuota` 字段 | cgroup v2 文件 | 转换 |
|---------------------|---------------|------|
| `cpu_cores: Some(c)` | `cpu.max` | `"<c*100000> 100000"`（CFS 100ms 周期） |
| `cpu_cores: None` | `cpu.max` | `"max 100000"` |
| `memory_bytes: Some(b)` | `memory.max` | `"<b>"`（字节硬上限） |
| `memory_bytes: None` | `memory.max` | `"max"` |
| `io_bps_limit` | （仅记录快照） | 需设备主次号，留待 `ResourceQuota` 扩展 |

- **写入路径**：`/sys/fs/cgroup/os/<component_id>`（cgroup v2 unified 挂载点的子层级）。
- **权限要求**：`root` + `CAP_SYS_ADMIN` + cgroup v2 挂载（见 SANDBOX.md 方案 A/C）。
- **在线调整**：`Orchestrator::set_quota` 直接改写 cgroup 文件，**无需重启进程**。
- **测试可注入**：单元测试用 `InMemoryCgroupBackend` 替身，不真写 `/sys/fs/cgroup`（红线）。
- **后端抽象**：`CgroupBackend` trait（`apply_quota` / `read_quota`），生产 `CgroupsRsBackend`，测试 `InMemoryCgroupBackend`。

### 2.4 健康检查

`HealthProbeConfig`（每组件一份）定义 osd 如何探活：

```toml
[health_probe]
kind = "http"                          # "http" / "tcp" / "exec"
target = "127.0.0.1:8080/health"
interval_secs = 10
timeout_secs = 3
failure_threshold = 3                  # 连续失败 3 次判 Failed
```

- **状态机**（`ComponentStatus`）：`Starting` →（探针通过）→ `Running`；连续失败达阈值 → `Failed` → 退避重启；`Stopped`（手动停）；`Disabled`（配置标 `enabled=false`，osd 跳过）。
- **退避**：失败重启按指数退避（实现侧策略，避免 crash loop 风暴）。
- **HA 协同**：HA 下每节点各跑一个 osd；`os-meta` 协调——leader 节点 osd 跑主服务，follower 节点 osd 维持 standby（VIP 与 VM 调度由 os-meta failover 触发）。

### 2.5 当前实现状态（阻塞点）

| 能力 | 状态 |
|------|------|
| 拓扑排序（Kahn）+ 环检测 | ✅ 完整（`topo.rs`，含 criterion benchmark） |
| `ComponentRegistry` + 状态机 | ✅ 完整 |
| cgroup v2 配额写入（cgroups-rs） | ✅ 真实接通，需 root |
| `set_quota`/`get_quota` 在线调整 | ✅ 真实 |
| 健康探针（trait + mock） | ✅ 契约 + Mock；真实 systemd 进程监管待集成 |
| systemd 拉起进程（start/stop/restart） | 🟡 框架（状态转换可用，真实拉起待 root 沙箱集成） |
| NTP（chrony 编排） | 🟡 契约 + 框架，真实编排待集成 |

> 这些阻塞项不阻塞本文档读者理解部署流程；它们是"沙箱集成阶段"的工作，见 `docs/SANDBOX.md`。

---

## 3. 配置

### 3.1 配置文件格式

系统配置统一用 **TOML**（与 `os-i18n` 翻译资源、`cargo` 生态一致；`toml` crate 已注册 ADR-DEPS-002）。机器可读的组件描述符（`ComponentDescriptor`）经 `serde` 反序列化。

### 3.2 主配置 `/etc/os/osd.toml`

```toml
# osd 主配置（首启从 /opt/os/share/defaults/osd.toml 复制，用户可改）

[os]
data_dir = "/var/lib/os"
log_dir  = "/var/log/os"
config_dir = "/etc/os"

[ntp]
enabled = true
# chrony 配置（osd 编排 chrony，需 root + CAP_SYS_TIME）
servers = ["pool.ntp.org", "time.cloudflare.com"]
# HA 下强制：节点时钟偏差 < 50ms（openraft/Kerberos/证书前置）
max_drift_ms = 50

[orchestrator]
# osd 自身运行模式
mode = "pid1"                          # "pid1"（自管）或 "systemd"（systemd 上层编排），见 §4
cgroup_base = "os"                    # /sys/fs/cgroup/<base>/<component_id>
restart_backoff_ms = 1000              # 失败重启初始退避
restart_backoff_max_ms = 30000
health_check_concurrency = 8           # 并发探针数

[api]
listen = "0.0.0.0:8443"
tls_cert = "/etc/os/os/tls.crt"
tls_key  = "/etc/os/os/tls.key"

[meta]
# HA 模式（"standalone" / "cluster"）
mode = "standalone"
cluster_id = ""                        # cluster 模式由首节点生成或 join 时填
raft_endpoint = ""                     # cluster 模式：本节点 openraft 监听地址
```

### 3.3 组件注册表 `/etc/os/components.toml`

声明式描述每个被编排的组件。osd 启动时一次性载入，运行期只读：

```toml
# 一个 [[component]] 块对应一个 ComponentDescriptor
[[component]]
id = "os-storage"
dependencies = []                       # 启动前必须 Running 的组件
enabled = true
command = "/opt/os/bin/os-storage"

  [component.quota]                      # ResourceQuota → cgroup v2
  cpu_cores = 2.0
  memory_bytes = 2147483648              # 2 GiB
  # io_bps_limit = 104857600             # 可选，需设备号（暂仅快照）

  [component.health_probe]
  kind = "tcp"
  target = "127.0.0.1:9100"
  interval_secs = 10
  timeout_secs = 3
  failure_threshold = 3

[[component]]
id = "os-network"
dependencies = ["os-storage"]
enabled = true
command = "/opt/os/bin/os-network"

  [component.quota]
  cpu_cores = 1.0
  memory_bytes = 536870912              # 512 MiB

  [component.health_probe]
  kind = "exec"
  target = "/opt/os/bin/os-cli net check"
  interval_secs = 30
  timeout_secs = 5
  failure_threshold = 3

[[component]]
id = "os-api"
dependencies = ["os-storage", "os-network", "os-security"]
enabled = true
command = "/opt/os/bin/os-api"

  [component.quota]
  cpu_cores = 2.0
  memory_bytes = 1073741824             # 1 GiB

  [component.health_probe]
  kind = "http"
  target = "127.0.0.1:8443/health"
  interval_secs = 10
  timeout_secs = 3
  failure_threshold = 3

# 可选组件
[[component]]
id = "os-guest"
enabled = false                          # 默认禁用，用户开启访客 Portal 时改 true
dependencies = ["os-api", "os-security"]
command = "/opt/os/bin/os-guest"
# ... quota / health_probe
```

> **循环依赖**：osd 启动时拓扑排序检测到环会直接报 `OrchestratorError::DependencyCycle` 拒绝启动，配置错误必须修正。

### 3.4 环境变量

osd 与组件读取的常见环境变量（可用 systemd `Environment=` 或 `/etc/os/env` 注入）：

| 变量 | 默认 | 用途 |
|------|------|------|
| `OS_CONFIG_DIR` | `/etc/os` | 配置目录 |
| `OS_DATA_DIR` | `/var/lib/os` | 持久状态 |
| `OS_LOG_DIR` | `/var/log/os` | 日志 |
| `OS_LOG_LEVEL` | `info` | `RUST_LOG` 风格（`osd=debug,os_storage=trace`） |
| `OSD_MODE` | `pid1` | 运行模式（`pid1` / `systemd`） |
| `OS_CGROUP_BASE` | `os` | cgroup 子层级名 |
| `OS_DISABLE_ROOT_OPS` | `0` | 非 root 测试环境置 `1`，跳过 cgroup/systemd 写入（走内存后端） |

### 3.5 首次启动引导（建池 / 建用户 / 重设密码）

**呼应 §3.19 排除清单**：ISO/克隆/迁移包**只含系统本体 + 配置骨架**，绝不携带密码哈希、私钥、集群密钥、用户数据。首启必须完成下列步骤，未完成则系统标记 `uninitialized` 拒绝对外服务：

1. **建存储池**（`os-storage` 引导）：
   - 用户在安装器（§5）或首启 Web 向导里选磁盘 → `zpool create`。
   - 池名默认 `tank`，挂载 `/tank`；推荐启用 LZ4 压缩 + ashift=12。
   - 创建系统子数据集：`tank/os-meta`、`tank/os-api`、`tank/os-data`。
2. **重设 root / admin 密码**（§3.19 系统密码项）：
   - 安装器在 `InstallStep::SetupFirstBoot` 阶段标记"首启强制重设"。
   - 首启 Web 向导（或 `os-cli bootstrap set-password`）用 `os-security` 的 Argon2id 哈希写入；之前 ISO 不带任何密码哈希。
3. **重生成密钥与证书**（§3.19 私钥/集群密钥项）：
   - `os-security` 首启自签 CA（`rcgen`）→ `/etc/os/os/ca.{key,crt}`。
   - JWT 签名密钥 → `/etc/os/os/jwt-signing.key`（Ed25519 / HS256）。
   - TLS 证书 → 首启用自签，用户可在 Web 配 ACME（Let's Encrypt）后替换。
   - SSH host key → 重生成 `/etc/ssh/ssh_host_*`。
4. **集群密钥**（若启用 HA）：
   - 首节点首启 → 生成 `openraft.key` + `join-token` + mTLS CA 私钥。
   - 后续节点经 `os-discover` 配对 + join-token 加入（见 §6），**不重生成**集群密钥而是继承。
5. **建首位管理员用户**：Web 向导建第一个管理员（用户名/密码/2FA），系统进入正常服务态。

> **状态机**：`uninitialized` →（建池 + 重设密码 + 重生成密钥 + 建管理员）→ `provisioned` → `running`。`os-api` 在 `uninitialized` 状态仅暴露 `/api/v1/bootstrap/*` 端点，其余拒绝。

---

## 4. systemd 集成

osd 与 systemd 有两种集成模式（规划 §3.13）。两种模式下 osd 的编排逻辑（拓扑/cgroup/健康）完全相同，区别在"谁当 PID1"与"组件进程怎么拉起"。

### 4.1 模式 A：osd 作为 systemd service（推荐生产）

```
PID 1: systemd
  └─ osd.service (Type=notify, NotifyAccess=main)
       ├─ 编排器（拓扑/cgroup/健康）
       └─ 经 systemctl / D-Bus 拉起子组件 unit
```

- **osd 不当 PID1**，但作为上层编排器补充 systemd 不擅长的**跨组件依赖排序**与**高级健康策略**（退避、级联重启、HA 联动）。
- 每个组件一个独立 unit（`os-storage.service` / `os-network.service` / ...），osd 通过 `systemctl start/stop/restart` 编排。
- **优势**：标准 systemd 体验（`journalctl`、`systemctl status`、`machinectl`）、与 Ubuntu 底座一致、崩溃恢复由 systemd 兜底。

**`/etc/systemd/system/osd.service`**：

```ini
[Unit]
Description=OS System Orchestrator (osd)
After=network.target zfs-import-cache.service chronyd.service
Wants=chronyd.service
Requires=zfs-import.target

[Service]
Type=notify
NotifyAccess=main
EnvironmentFile=/etc/os/env
ExecStart=/opt/os/bin/osd --config /etc/os/osd.toml
Restart=on-failure
RestartSec=2s

# osd 需要的权限：拉起子 unit + 写 cgroup + 编排 chrony
# （systemd 自身已 root，子 unit 经 User= 各自降权）
LimitNOFILE=65536
TasksMax=infinity

# 通知就绪（sd_notify）后才视为激活
WatchdogSec=30s

[Install]
WantedBy=multi-user.target
```

**子组件 unit 示例 `/etc/systemd/system/os-storage.service`**（osd 编排的目标）：

```ini
[Unit]
Description=OS Storage (ZFS orchestration)
PartOf=osd.service
After=osd.service
Requires=osd.service

[Service]
Type=exec
ExecStart=/opt/os/bin/os-storage
Restart=on-failure
RestartSec=2s
User=root                      # ZFS 操作需 root；可用 systemd CapabilityBounding 限权
Slice=os.slice                # 统一在 os.slice 下，cgroup 层级清晰

[Install]
WantedBy=multi-user.target
```

### 4.2 模式 B：osd 自身作 PID1（容器 / 嵌入式 / ISO 安装的运行时）

```
PID 1: osd (容器入口 或 ISO 安装后的 initramfs 切换根)
  ├─ 信号处理（SIGCHLD 收尸、SIGTERM 优雅停机）
  ├─ 编排器（拓扑/cgroup/健康）
  └─ 直接 fork/exec 拉起子组件（不走 systemd）
```

- 用于容器镜像（`docker run` 入口）、嵌入式设备、或 ISO 安装的精简运行时。
- osd 必须实现完整的 PID1 职责：僵尸回收、`SIGTERM`→广播停止、`SIGINT`/`SIGUSR1` 触发 reload。
- **`/etc/os/osd.toml` 设 `[orchestrator] mode = "pid1"`**；osd 不调 systemctl，直接 `exec` 各组件二进制并把 PID 注册到自身监管表。
- **优势**：单进程入口、无 systemd 依赖、容器友好。
- **风险**：失去 systemd 兜底；推荐只在容器 / 临时环境用此模式。

**容器 `Dockerfile`（示意）**：

```dockerfile
FROM ubuntu:26.04
RUN apt-get update && apt-get install -y zfsutils-linux chrony && rm -rf /var/lib/apt/lists/*
COPY target/release/osd /opt/os/bin/osd
COPY target/release/os-api /opt/os/bin/os-api
COPY --from=os-ui-builder /dist /opt/os/share/web-ui
COPY share/defaults/ /opt/os/share/defaults/
ENV OSD_MODE=pid1
# privileged：cgroup v2 + ZFS loop 设备（SANDBOX.md 方案 A）
ENTRYPOINT ["/opt/os/bin/osd", "--config", "/etc/os/osd.toml"]
```

### 4.3 优雅停机

两种模式下，osd 收到停止信号后按**依赖逆序**停止组件：
- 先停上层（os-im / os-api / os-guest），再停协议层（os-protocols），最后停存储/网络/底层。
- 每组件先 `SIGTERM`，超时（默认 30s）后 `SIGKILL`。
- osd 自身在所有组件退出后才退出（保证 cgroup 清理、日志 flush）。

---

## 5. ISO 安装流程

`os-iso`（§3.11）负责把整套系统打包成可安装 ISO，是**离线/冷启动分发**手段（与 `os-provision` 的 PXE 在线自举互补）。

### 5.1 ISO 内容与变体

| 内容 | 说明 |
|------|------|
| Ubuntu 26.04 最小底座 | 精简 rootfs（squashfs 压缩） |
| Rust 组件二进制 | osd + 全部组件，放 `/opt/os/bin/` |
| 系统依赖 | ZFS / KVM / libvirt / chrony / Samba |
| web-ui 静态资源 | Vue 3 dist |
| 默认配置骨架 | `/opt/os/share/defaults/` |
| Rust 安装器 | TUI（ncurses）或轻量 Web 引导 |
| initramfs | 加载 squashfs + 切换到安装环境 |

**两种变体**（`IsoVariant`，`crates/os-iso/src/iso.rs`）：

- **`Standard`**（标准安装 ISO）：空系统，安装后是空白 OS，首启需重设密码 / 重生成密钥 / 建池 / 建管理员（§3.5）。
- **`Clone`**（克隆变体）：内嵌当前节点**配置快照**（拓扑、共享、网络、用户定义），用于批量部署同构节点。**配置快照在打包前经 `filter_sensitive()` 递归剔除 §3.19 敏感键**（password / secret / token / private_key / ssh_key / cert_key / mnemonic / seed / access_token / refresh_token……大小写不敏感），绝不携带任何可还原凭据。

### 5.2 构建命令（构建期组件）

`os-iso` 编排 `xorriso` + `squashfs-tools` + `initramfs-tools`：

```bash
# 1. 准备 ISO 工作目录（rootfs / EFI / isolinux / initramfs）
# 2. 注入全部 Rust 二进制（cargo build --release 产出）
# 3. 标准变体：空配置骨架；克隆变体：导出当前节点 config_snapshot 并 filter_sensitive()
# 4. xorriso 打包成 .iso
cargo run --release -p os-iso -- build --variant standard --out os-os-1.0.0.iso
# 或克隆变体（需当前节点运行中导出快照）
cargo run --release -p os-iso -- build --variant clone --from-current --out os-os-clone.iso
```

> 真实 xorriso/squashfs 调用由 `os-iso/src/impl_iso.rs` 完成（构建期组件，无运行时依赖）。打包脚本集成进 CI（发版流水线）。

### 5.3 安装器步骤（裸机安装状态机）

`RustInstaller`（`crates/os-iso/src/installer.rs`）按下列有序状态机推进，任一步失败终止：

```
Partition → CreatePool → ExtractRootfs → InstallComponents
  → ConfigureSystem → SetupFirstBoot → Done
```

| 步骤 | 操作 | 权限/风险 |
|------|------|----------|
| `Partition` | 写磁盘分区表（GPT，含 boot/EFI/root 分区 + 双系统槽位 A/B） | root，高危（数据擦除） |
| `CreatePool` | `zpool create` 在 root 分区建 ZFS 池 | root，高危 |
| `ExtractRootfs` | squashfs 解压到目标盘根文件系统 | root |
| `InstallComponents` | 注入 osd / os-storage / ... 二进制到 `/opt/os/bin/` | root |
| `ConfigureSystem` | 网络 / locale / fstab / 首位用户骨架（不含密码） | root |
| `SetupFirstBoot` | 设置首启动作：标记 `uninitialized` + 强制重设密码（§3.19） | root |
| `Done` | 安装完成，重启进入新系统首启（§3.5 引导） | — |

**硬件兼容性检测（HCL）**：安装器启动先跑 HCL——核对 Ubuntu 26.04 推荐配置（2 GHz 双核 / ≥6 GB RAM / 25 GB 磁盘）、ZFS/KVM 可用性、网卡存在——不达标拒绝继续。

### 5.4 首启（呼应 §3.5）

ISO 安装完成重启后，新系统处于 `uninitialized` 状态。osd 拉起 os-api 但只暴露 `/api/v1/bootstrap/*`。用户经 Web 向导或 `os-cli bootstrap` 完成：建池 → 重设 admin 密码 → （HA）生成集群密钥 / join → 建首位管理员 → 进入 `running`。

> 真实裸机执行（分区/建池/写盘）当前留 TODO（需 SANDBOX.md 方案 B：QEMU/KVM 嵌套虚拟化）；逻辑状态机已可测。

---

## 6. HA 集群部署

HA 集群由 `os-discover`（发现 + 配对 + 资格检测）+ `os-meta`（openraft 共识 + 故障转移 + VIP）+ `os-storage`（ZFS send/recv 复制）协同实现。**HA 集群 vs 同级 peer**（§3.5）必须区分：

- **HA 集群**：openraft 共识成员，法定数有限（3/5/7 节点），自动故障转移 + VIP。
- **同级 peer**：经 os-discover 注册，**无数量上限**、不进法定、不走共识，仅做 ZFS mirror 同步（1 主 N 从只读副本，单向 send/recv）。

### 6.1 前置条件（HA 资格检测）

`os-discover` 的 HA 资格检测硬指标（全部满足才允许建/加 HA 集群）：

- 节点数 ≥ 法定（≥3，奇数：3/5/7）。
- 硬件 / 内核 / ZFS 特性兼容（与既有成员同构）。
- 节点间带宽达标（**HA 建议 10 GbE**，用于 ZFS 复制 + VM 迁移）。
- ZFS send/recv 复制能力可用。
- 版本兼容（同 major.minor，或经 os-update 滚动升级到齐）。

### 6.2 单节点首启建集群（首节点）

```bash
# 在第一台节点完成 §3.5 引导后，切到 HA 模式
os-cli ha init --cluster-id mycluster --endpoint 10.0.0.11:7000
```

- `os-meta` 生成 `cluster_id` + openraft 监听端点。
- 首启生成集群密钥（`/etc/os/cluster/openraft.key`）+ mTLS CA + **join-token**（§3.19 排除项，仅本节点持有）。
- 当前节点角色 `Standalone` → `Leader`（单节点法定）。

### 6.3 多节点 join 流程

新节点经 `os-discover` 发现 + 配对 + join：

```
新节点                                  既有集群（leader 或任一成员）
  │
  │ 1. os-discover LAN beacon (mDNS-DNS-SD / UDP 组播)
  │ ──────────────────────────────────────────────────────►
  │                              发现响应（版本/架构/HA 成员/规格，beacon 签名防 spoof）
  │ ◄───────────────────────────────────────────────────────
  │
  │ 2. 凭证认证互联（引导令牌 / 管理员凭证 / 配对码）
  │    → mTLS 双向认证（rustls，CA 由首节点签发）
  │ ──────────────────────────────────────────────────────►
  │                              HA 资格检测（HCL + 带宽 + ZFS + 版本）
  │ ◄───────────────────────────────────────────────────────
  │    分支决策：
  │    ├─ 符合 HA 且用户选 HA → 调 os-meta.join_cluster（向下）
  │    └─ 否则 → 同级 peer（os-storage ZFS mirror，不进法定）
  │
  │ 3. Consensus::join_cluster(endpoint, token)
  │    （crates/os-meta/src/consensus.rs）
  │ ──────────────────────────────────────────────────────►
  │                              leader 把新节点加入 openraft 成员列表
  │                              触发日志复制追赶 + 投票加入
  │ ◄──────────────────────────────────────────────────────
  │                              返回 NodeRole::Follower
  │
  │ 4. os-storage 配 ZFS send/recv（leader 数据 → 新 follower）
  │ 5. os-meta 配 VIP（浮动 IP，由 leader 持有）
  │ 6. osd 进入 standby（follower 维持就绪，leader 跑主服务）
```

**命令示例**：

```bash
# 在新节点（已 os-discover 配对成功，拿到 join-token）
os-cli ha join --endpoint 10.0.0.11:7000 --token <join-token-from-leader>
```

`Consensus::join_cluster(endpoint, token) -> NodeRole`（`crates/os-meta/src/consensus.rs`）：
- `endpoint`：已知成员（通常 leader）的 openraft 接入地址。
- `token`：由 discovery/mTLS 配对签发的加入凭证。
- 成功返回 `Follower`（通常）；写操作经 leader 转发，非 leader 写返回 `NotLeader`。

### 6.4 故障转移

- **故障检测**：`os-meta` openraft 心跳超时 → leader 标记节点失联 → 触发选举。
- **VIP 漂移**：新 leader 由 `os-meta` 编排 `ip` 把 VIP（浮动 IP）绑到自身网卡。
- **VM/容器调度**：失联节点上的 VM/容器由 leader 重调度到健康节点（KVM `virsh migrate` / youki 重启）。
- **存储故障切换**：active-passive，原 active 失联后 passive 提升为 active（ZFS 池导入）。

### 6.5 离开集群

```bash
os-cli ha leave
```

`Consensus::leave_cluster()`：先做日志追赶，再从 openraft 成员列表移除。离开后该节点回 `Standalone`。

---

## 7. 升级/回滚

`os-update`（§3.12）覆盖运行中节点的 OTA 升级，与 `os-iso`（离线安装）互补。

### 7.1 A/B 双槽位 OTA

每个节点磁盘上有**两个系统槽位**（A / B），任一时刻至多一个为 `Active`（不变量）：

| 槽位状态（`SlotStatus`） | 含义 |
|--------------------------|------|
| `Active` | 当前活动槽（本次启动从此槽引导） |
| `Inactive` | 备用槽（A/B 更新的写入目标，可写） |
| `Updating` | 正在写入更新（锁定，防并发覆盖） |
| `Failed` | 该槽启动失败过（标记坏，不再选用作写入目标，除非显式修复） |

**升级流程**（`AbUpdateEngine`，`crates/os-update/src/impls.rs`）：

```
1. 下载更新包（UpdateManifest + 二进制）
2. ed25519 签名校验 + sha256 摘要（已接通 ed25519-dalek + sha2）
3. semver 比较（upgrade_decision）：拒绝降级 / 跨大版本跳（除非 manifest 显式允许）
4. 选写入目标：SlotManager 选 Inactive 槽（A→B 或 B→A）
   标记目标槽 Updating
5. 写入新系统到目标槽（/opt/os/versions/<new-semver>）
6. bootloader 激活目标槽为下次启动（SlotStatus::Active），原槽变 Inactive
7. 重启
8. watchdog 探活（首启健康检查）：
   ├─ 通过 → 提交（标记新槽健康 committed，旧槽保留为回滚点）
   └─ 失败 → on_boot_failed：SlotManager 自动回滚到上一个 active 槽
```

**关键安全**：新槽只有在标记 `Active` **且** boot 探活通过后才视为"提交"；探活失败自动回滚到上一个健康 active 槽（`SlotManager::on_boot_failed`）。bootloader 激活当前留 TODO（待 bootloader 依赖注册，A/B 槽状态机决策已完整可用）。

### 7.2 回滚

`AbRollbackManager`（`crates/os-update/src/rollback.rs`）：

- **触发条件**（`should_rollback`）：boot 探活失败 / 关键组件启动超时 / 健康检查连续失败。
- **回滚点**（`RollbackPoint`）：上一个健康槽位 + 版本快照。
- **策略**（`RollbackPolicy`）：自动（watchdog）/ 手动（管理员触发）。
- **手动回滚**：
  ```bash
  os-cli update rollback                    # 回到上一个 committed 槽
  os-cli update rollback --to <semver>      # 回到指定版本（需该版本槽位仍在）
  ```

### 7.3 HA 集群滚动升级

`HaRollingUpgrade`（`crates/os-update/src/rolling.rs`）逐节点升级，保证 HA 不中断：

| 策略（`RollingStrategy`） | 说明 |
|---------------------------|------|
| `FollowersFirst`（默认） | follower 先升 → 单节点验证 → 再升 leader，最大化可用性 |
| `OneAtATTime` | 一次一个节点（逐节点串行） |
| `AllAtOnce` | 全部同时（仅维护窗口用） |

**流程**（`RollingPlan` + `RollingStateMachine`）：

1. `decide_upgrade_order()` 按 strategy 排定节点升级顺序。
2. 逐节点：`os-update` 写新槽 → 切槽 → 重启该节点。
3. 该节点重入集群后，`per_node_verify` 做健康验证（openraft 心跳恢复 + VIP/VM 正常）。
4. 验证通过 → 升下一节点；失败 → 该节点自动回滚到旧槽（不影响其他节点）。
5. leader 最后升（`FollowersFirst`）：升 leader 前先触发一次 leader 切换（旧 leader 主动让位），保证升级期间始终有 leader。

### 7.4 CVE 监听

`NvdCveMonitor`（`crates/os-update/src/cve.rs`）监听 C 依赖 CVE 公告（Samba / QEMU / rdma-core / libvirt / openssl），推送安全更新建议。`CveCallback` 经 `Box<dyn>` 多态，用 `#[async_trait]`（ADR-COMPAT-001）。NVD 数据源拉取留 TODO（决策路径已可用）。

### 7.5 组件级升级（非系统级）

非系统组件（os-services 的子模块、第三方 .deb）可单独升级，不触发 A/B 切槽：
- Rust 组件：换 `/opt/os/bin/<component>` 二进制 + osd `restart <component>`。
- 第三方 .deb：`os-pkg` 编排 `apt`/`dpkg`（§3.4）。
- HA 下第三方包默认仅装单节点、不参与复制。

---

## 8. 监控接入

`os-services` 的 monitor 子模块（`crates/os-services/src/monitor.rs`，规划 §3.16）提供 metric 采集 / 日志收集 / 告警引擎，并暴露 Prometheus 兼容的 `/metrics` 端点。

### 8.1 架构

```
各 Rust 组件（Counter/Gauge/Histogram 仪器）
   │ OpenTelemetry SDK（SdkMeterProvider 聚合）
   ▼
opentelemetry-prometheus exporter
   │
   ▼
prometheus::Registry（gather）
   │
   ▼
OtelMonitor::render_metrics() —— Prometheus 文本格式（type=text, version=0.0.4）
   │
   ▼
os-api 的 /metrics 端点（axum handler 回写）
```

- `OtelMonitor`（`impl Monitor`）基于 `opentelemetry` + `opentelemetry_sdk` + `opentelemetry-prometheus` + `prometheus`（均 ADR-DEPS-002 注册并接通）。
- 指标用 OTel `Counter`/`Gauge`/`Histogram` 仪器，经 `SdkMeterProvider` 聚合；`/metrics` 端点经 `prometheus::Registry` + `TextEncoder` 输出文本格式。
- `ExporterBuilder` 关闭 `target_info` / `otel_scope_info` 噪声 metric，让 `/metrics` 输出干净。

### 8.2 /metrics 端点

- 暴露在 os-api 的 `/metrics` 路径（与业务 API `/api/v1/*` 分离）。
- 响应体 = `OtelMonitor::render_metrics()` 返回的 Prometheus 文本格式。
- 典型指标：
  - `os_storage_zpool_used_bytes{pool="tank"}`
  - `os_storage_io_ops_total{pool="tank",op="read"}`
  - `os_meta_raft_term` / `os_meta_raft_commit_index` / `os_meta_cluster_state`
  - `osd_component_status{component="os-storage"}`（0=Failed/1=Starting/2=Running/3=Stopped/4=Disabled）
  - `osd_component_restart_total{component="..."}`
  - `os_api_http_requests_total{route,method,status}`
  - `os_compute_vm_count{state="running"}`

### 8.3 Prometheus 抓取配置

`/etc/prometheus/prometheus.yml`（在监控服务器，非 OS 节点本身）：

```yaml
scrape_configs:
  - job_name: 'os'
    file_sd_configs:
      - files: ['/etc/prometheus/os-targets.yml']
    metrics_path: /metrics
    scheme: https
    tls_config:
      ca_file: /etc/prometheus/os-ca.crt
      # 若 os-api 用自签，需把 os-security 自签 CA 分发给 Prometheus
    relabel_configs:
      # HA 集群：抓 VIP（由 os-meta 漂移，Prometheus 自动跟到 leader）
      # 同级 peer：分别抓每节点
```

`/etc/prometheus/os-targets.yml`：

```yaml
- targets:
    - 'os-vip.example.com:8443'    # HA 集群 VIP（或单节点固定 IP）
  labels:
    cluster: 'mycluster'
```

### 8.4 告警规则

`os-services` 的 `AlertEngine`（纯逻辑状态机：`Pending` → `Firing` → `Resolved`）内置抖动抑制（`for_duration_secs`）与去重。Prometheus 侧可配告警规则示例（`prometheus/rules.yml`）：

```yaml
groups:
  - name: os
    rules:
      - alert: OsDiskFull
        expr: os_storage_zpool_used_bytes / os_storage_zpool_size_bytes > 0.9
        for: 5m
        labels: { severity: warning }
        annotations:
          summary: "OS 池 {{ $labels.pool }} 使用率 >90%"

      - alert: OsRaftNoLeader
        expr: os_meta_cluster_state == 3   # Offline
        for: 1m
        labels: { severity: critical }
        annotations:
          summary: "OS 集群 {{ $labels.cluster }} 无 leader"

      - alert: OsComponentFailed
        expr: osd_component_status == 0
        for: 2m
        labels: { severity: critical }
        annotations:
          summary: "OS 组件 {{ $labels.component }} Failed"
```

### 8.5 日志与可观测性

- **日志**：组件统一 `tracing` 输出，`/var/log/os/<component>.log` + journald 双写；`os-services` 的日志查询接口支持按级别/目标/时间过滤 tail。
- **分布式追踪**：多 agent 协作（§3.7.2）的 trace_id 全链路贯通；OTel tracer 导出可接 Jaeger/Tempo（tracing→OTel 桥接留 TODO）。
- **HA 联动**：`os-meta` 故障转移、`os-update` 回滚等系统事件经 `os-im` 推送给管理员；监控告警也走 IM 通道。

---

## 9. 实机部署现状（os-api.service，2026-08）

> §1–§8 描述的是目标终态蓝图。**当前开发机（ub2604）实际以单进程 os-api 常驻运行**，
> 本节为真实部署口径（2026-08-20 核对：systemd unit + 源码 env 读取点）。

### 9.1 systemd 服务

`/etc/systemd/system/os-api.service`（实机内容）：

```ini
[Unit]
Description=NexOS os-api gateway
After=network-online.target
Wants=network-online.target

[Service]
User=oem
EnvironmentFile=/etc/default/os-api
WorkingDirectory=/home/oem/NexOS
ExecStart=/home/oem/NexOS/target/debug/os-api --addr 0.0.0.0:8080
Restart=on-failure
RestartSec=3

[Install]
WantedBy=multi-user.target
```

运维：`sudo systemctl daemon-reload && sudo systemctl enable --now os-api`；日志
`journalctl -u os-api -f`（旧手动方式 `nohup` 的 /tmp/os-api.log 已被 journald 取代）。
**debug 二进制是有意的**（用户 2026-08-15 指示暂缓 release 构建：现阶段更改频繁）。

### 9.2 环境变量（/etc/default/os-api）

os-api 进程的全部 env 读取点（`crates/os-api/src/main.rs:128-167` + 各 handler）：

| 变量 | 默认 | 作用 |
|------|------|------|
| `NEXOS_ADMIN_TOKEN` | （空，回退 `OS_ADMIN_TOKEN`） | 固定 admin token：请求头 `Authorization: Bearer <token>` 精确匹配 → 注入 admin Principal（所有 requires_auth+admin 路由的全钥匙） |
| `OS_ADMIN_TOKEN` | （空） | 同上的 OS_ 前缀后备 |
| `NEXOS_JWT_SECRET` | （空，回退 `OS_JWT_SECRET`） | JWT HS256 密钥；未设置则随机生成（重启后旧 JWT 失效，生产必配） |
| `OS_JWT_SECRET` | （空） | 同上后备 |
| `NEXOS_PAY_USDT_ADDR` | （空→订单带 warning） | USDT 充值收款地址（api_gateway.rs:142；未配时订单仍创建但提示先配置再打款） |
| `NEXOS_PAY_BTC_ADDR` | （空→订单带 warning） | BTC 充值收款地址（同上） |
| `NEXOS_PAY_EVM_ADDR` | （空→订单带 warning） | EVM 链充值收款地址（同上） |
| `NEXOS_LLM_METRICS_SIMULATE` | 关（`0`） | `=1/true` 开 vLLM 监控模拟模式：端口不通时回 sin 波合成数据（llm.rs:542；默认纯真实，绝不伪造） |
| `NEXOS_HTTP_PORT` | `8080` | NexHub git clone_url_http 生成的端口（`OS_HTTP_PORT` 后备） |
| `NEXOS_GIT_REPOS_DIR` | `/tank/git-repos` | NexHub 裸仓库根目录（`OS_GIT_REPOS_DIR` 后备） |
| `NEXOS_GIT_USER` | `oem` | git SSH 通道用户名（`OS_GIT_USER` 后备） |
| `NEXOS_GIT_HOST` | `ub2604` | clone URL 主机名（`OS_GIT_HOST` 后备） |
| `NEXOS_LOBBY_NO_AUTO_PUBLISH` | 关 | `=1` 跳过启动时 nexos 主仓默认常驻大厅发布（os-nexhub nexhub_lobby.rs:1510） |
| `NEXOS_SSH_BIN` | `ssh` | 远程转发 spawn 的 ssh 二进制路径（forwarding） |
| `NEXOS_FORWARDING_HOST` | （回退 hostname） | 转发端点默认主机（`OS_FORWARDING_HOST` 后备） |
| `NEXOS_IMGGEN_BIN` / `NEXOS_IMGGEN_SCRIPT` / `NEXOS_IMGGEN_TIMEOUT_SECS` | python3 / 自动落盘脚本 / 超时 | sd-turbo 生图管线（media_gen，详见 MEDIA_GEN_AND_CHAIN_AUTH.md） |
| `NEXOS_SMI_BIN` | `nvidia-smi` | 显存探测二进制 |
| `NEXOS_SD_MODEL` | `/tank/models/sd-turbo` | 生图模型目录 |
| `NEXOS_VIDEO_API_URL` / `NEXOS_VIDEO_API_KEY` | （空→任务即 failed 附指引） | 外部视频生成后端（未配即明确报错，诚实不假装排队） |
| `NEXOS_APPLY_SYSTEM` / `OS_APPLY_SYSTEM` | 关（不生效） | SMB 共享真实写系统（smb.conf/avahi）门禁，详见 STORAGE_SHARING.md |
| `NEXOS_P2P_ENABLE` | 未设（关） | `=1/true/yes` 在 os-api 进程内 spawn os-p2p 组网节点（P2b；未启用时 `/api/v1/p2p/*` 全部 503 引导文案），详见 NEXOS_P2P_NETWORK_DESIGN.md §7 |
| `NEXOS_P2P_BOOTSTRAP` | 空（孤网等入站） | P2P 引导节点 `host:port,...`（如 cloud 锚点 `198.51.100.114:7070`） |
| `NEXOS_P2P_LISTEN` | `:7070` | P2P 组网监听地址（省 IP 形式支持） |
| `NEXOS_P2P_PUBLIC` | 未设（普通节点） | `=1` 声明公网服务节点（bootstrap 锚点 + 端点交换所 + relay 志愿者） |
| `NEXOS_P2P_MDNS` | 开 | `=0/false` 关闭 mDNS LAN 种子（`_nexos-p2p._tcp`，与 avahi `_nexos._tcp` 不串扰） |
| `NEXOS_P2P_NAME` | 空 | P2P 节点昵称（status/网络页展示） |
| `NEXOS_P2P_KEY_FILE` | 降级链 `/tank/os-data/p2p-node-key` → `/var/lib/os/p2p-node-key` → `./p2p-node-key` | P2P 节点 secp256k1 私钥（hex）：存在加载/不存在生成并原子写 0600——**重启 NodeID 稳定**（CLI p2p-node 与 os-api 共用） |
| `TMDB_API_KEY` | （空→刮削跳过） | 影院 TMDB 元数据刮削（media.rs，无 NEXOS_ 前缀，历史遗留） |

> 协作铁律：环境变量一律 **NEXOS_ 前缀**（OS_ 作后备兼容）；新增 env 必须同步登记到对应功能
> MD 的 env 表（本表是全量汇总，各功能文档为准）。

### 9.3 8080 端口语义

单端口复用四种流量（`--addr 0.0.0.0:8080`）：

| 路径 | 流量 |
|------|------|
| `/` | Vue3 Web 桌面（rust-embed 嵌入的 static-dist） |
| `/api/v1/*` | 业务 REST（30 个 RouteHandler 组件，约 304 条路由） |
| `/metrics` | 进程级 OTel Prometheus 指标（§8） |
| `/git/<repo>.git/*` | HTTP Smart Git（git-http-backend CGI；Bearer token 或 Basic 密码=token，MEMORY.md §四） |

鉴权分层：写操作（POST/PUT/DELETE）需 admin token 或 JWT；读多为公开；IM 用户面端点需
链上身份 token（IM_BLOCKCHAIN_AUTH_DESIGN.md）。仅本地调试可 `--addr 127.0.0.1:8080`。

### 9.4 前端改动发布流程

```
cd crates/os-api/web && npm run build     # 产物 → static-dist（os-web/static-dist 为旧存档）
cargo clean -p os-api && cargo build -p os-api   # rust-embed 增量不重嵌，必须 clean
sudo systemctl restart os-api
```

（rust-embed 坑：不 clean 会嵌入旧资源——MEMORY.md 避坑记录。）

---

## 附录 A：快速验证清单（部署后自检）

```bash
# 1. 服务状态
systemctl status osd                        # 或 docker ps（模式 B）
os-cli component list                       # 列出全部组件 + ComponentStatus

# 2. 健康检查
curl -sk https://localhost:8443/health       # os-api 健康
curl -sk https://localhost:8443/metrics | head

# 3. 存储
zpool status tank
os-cli storage pool list

# 4. HA（若启用）
os-cli ha status                            # ClusterStatus（角色/term/leader/commit_index）

# 5. 升级槽位
os-cli update slot status                   # A/B 槽位状态

# 6. 日志
journalctl -u osd -f
tail -f /var/log/os/osd.log
```

## 附录 B：常见问题

| 现象 | 排查 |
|------|------|
| osd 启动报 `DependencyCycle` | `components.toml` 依赖图有环，检查 `dependencies` 字段 |
| cgroup 写入 EPERM | 非 root 运行，或 cgroup v2 未挂载；设 `OS_DISABLE_ROOT_OPS=1` 走内存后端（仅测试） |
| 组件一直 `Failed` 重启 | 看健康探针配置（`health_probe.target` 是否可达）；提高 `failure_threshold` 或先手动跑 `command` 看错误 |
| HA join 失败 `NotLeader` | 写操作必须发到 leader；先用 `os-cli ha status` 找 leader 再 join |
| 升级后首启失败 | watchdog 应自动回滚到旧槽；检查 `/var/log/os/osd.log` 的 boot 探针日志 |
| `/metrics` 抓不到 | os-api TLS 用自签 CA，Prometheus `tls_config.ca_file` 需指向 os-security CA |

**文档版本**：1.1（§1–§8 为 2026-08-05 蓝图，对应 main @ `d7200c2`；§9 为 2026-08-20 实机现状增补）
**红线声明**：本文档只描述部署，不修改任何源码。
