# NexOS — 独立个体的操作系统 · 连接 OS

> AI 时代，每一个人都是一个非常强的个体。NexOS 打破信息孤岛，连接每一个超级个体。

详细理念见 [PHILOSOPHY.md](PHILOSOPHY.md)

---

# OS 系统（纯 Rust）

一套用纯 Rust 从零构建的 **Network Attached Storage（网络附加存储）操作系统**——以 26 个 Rust crate 覆盖一台 OS 设备的**全栈**：KVM 虚拟机、容器、ZFS 存储、网络与防火墙、文件/块协议栈、媒体转码与语义搜索、去中心化共识与链上凭证、A/B 无缝更新，以及中/英/日多语言与跨端 SDK；网关层内嵌 **Vue3 Web 桌面（30 个应用）**，并提供 NexHub 代码大厅、IM 链上身份、媒体生成（sd-turbo）、AI 网关计费等上层能力。

目标是**单一 Rust 代码库替代传统 OS 的多层异构组件**（Linux 发行版 + shell 脚本 + C 守护进程 + Web 后端），以类型安全、零运行时 GC、强错误模型统一从内核接口到 API 网关的每一层。

---

## 核心特性

- **ZFS 存储管理**：真实 `zfs send/recv` 跨池复制、原生加密（`load-key`/`unload-key`）、快照/克隆、`zpool status` 树形解析（vdev 明细 + 错误计数）。
- **KVM 虚拟机**：libvirt FFI（`virt` crate）真实管理 VM 全生命周期；**启动前 CPU 虚拟化预检**（vmx/svm flags + `/dev/kvm` + kvm 模块，缺失时给出"BIOS 开启 VT-x"诊断）。
- **容器**：OCI/runc 真实拉起（create→start→delete 往返）+ youki 运行时编排层 + CNI 网络 + Docker 管理 UI。
- **文件协议**：SMB（smb.conf 渲染 + `testparm` 校验 + reload，运维手册见 docs/STORAGE_SHARING.md）、NFS（ganesha.conf + exportfs）、FTP（libunftp）、SFTP（russh 纯 Rust SSH）、WebDAV（dav-server）。
- **块存储**：iSCSI（LIO targetcli + configfs）+ NVMe-oF（nvmet configfs export）真实往返。
- **网络管理**：nftables 防火墙（nftnl/mnl FFI 真实事务）+ rtnetlink/genetlink 接口与路由配置 + DHCP/DNS + WireGuard VPN + mTLS 设备联邦。
- **媒体**：FFmpeg 真实转码（HLS 单/多档位 ABR）+ **CLIP 语义搜索**（candle 纯 Rust 推理，CUDA GPU 加速）+ **媒体生成**（本地 sd-turbo 文生图，显存互斥保护；视频生成任务框架）。
- **A/B 无缝更新**：双槽位 bootloader 管理（GRUB/systemd-boot `bootctl`/`grub2-reboot`）+ 签名包验证（ed25519）+ 回滚。
- **去中心化共识**：真实 Raft（openraft）+ SQLite 元数据存储，支持 HA 故障转移。
- **链上凭证**：BIP-322 验签（legacy + simple）、BTC（rust-bitcoin）、EVM（alloy）多链钱包与签名；**IM/大厅链上身份**（secp256k1 公钥即身份，挑战-签名三步认证，os-common chain_auth 共享内核）。
- **NexHub（代码仓库中心 + 大厅）**：自托管 git（SSH + HTTP Smart Git 双通道）、项目发布/搜索/一键克隆、付费门禁与悬赏 bounty（docs/NEXHUB_LOBBY_DESIGN.md）。
- **AI 网关与变现**：LLM 渠道聚合 + token 四计费模式（free/per_token/per_image/credits）+ USDT/BTC/EVM 充值订单（docs/GATEWAY_MONETIZATION.md）+ vLLM 实例监控（docs/LLM_MONITORING.md）。
- **远程转发**：SSH 隧道（-L/-R/-D）spawn + 纯 Rust RDP TCP 代理 + .rdp 文件生成（docs/FORWARDING.md）。
- **30 个桌面应用**：存储/文件/备份/监控/下载（aria2）/容器/笔记/云同步（rclone）/监控摄像头/流媒体/QR 传输/应用中心/区块链/模型管理/聊天/远程转发等（docs/FEATURE_SURVEY.md、docs/APPS_REFERENCE.md）。
- **多语言**：中 / 英 / 日，自实现 TOML 翻译器 + 运行时切换。
- **跨端 SDK**：`os-cli`（运维命令树，clap）+ `os-mobile`（HTTP 客户端）+ `os-desktop` + `os-mcp`（MCP Server，AI 助手可直接管理本系统）。

---

## 快速开始

### 环境要求

| 项 | 要求 |
|----|------|
| OS | Linux（开发/运行底座为 Ubuntu 26.04 LTS） |
| Rust | **1.97+**（stable；需 `async fn in trait` 稳定线，MSRV 标注 1.75） |
| 构建工具 | `build-essential`、`pkg-config`、`libclang-dev`（nftnl/rtnetlink FFI 绑定） |
| SQLite | 无需系统安装（`rusqlite` 用 `bundled` feature 内嵌编译） |
| 可选（功能门控） | `libvirt-dev`（KVM 真实 FFI）、`libnftnl-dev` + `libmnl-dev`（防火墙 FFI）、`ffmpeg`（转码）、CUDA toolkit（CLIP GPU 加速）、`zfsutils-linux`（ZFS） |
| Docker（可选） | 跳过本地工具链，一键体验见下方「Docker 快速开始」 |

### Docker 快速开始

无需本地 Rust 工具链——用项目根 `Dockerfile` 多阶段构建一个可 `docker run` 的完整 OS 镜像（入口 = `osd --serve-api 0.0.0.0:8080`，内嵌 HTTP 网关）。

```bash
# 1. 构建镜像（builder=rust:1.97，runtime=debian:bookworm-slim，含 osd/os-api/os 三个 binary）
docker build -t os-system .

# 2. 运行（ZFS / nftables / cgroup 真实操作需特权 + 主机内核设施）
docker run --rm -it --privileged --cgroupns=host \
  -p 8080:8080 -v os-data:/data os-system

# 3. 另开终端，验证网关已起（SystemRouteHandler 健康检查端点）
curl http://localhost:8080/healthz          # → {"status":"ok"}

# 4. 体验 API（内存态业务路由，非特权亦可工作）
curl http://localhost:8080/api/v1/vms       # 虚拟机列表
curl http://localhost:8080/api/v1/nodes     # 节点发现
curl http://localhost:8080/api/v1/users     # 用户管理

# 5. 容器内用 os CLI 调本机网关
docker exec -it <container> os --server http://localhost:8080 discover
```

或用 `docker compose`（含健康探针 + 持久卷，见 `docker-compose.yml`）：

```bash
docker compose up --build         # 构建并启动
curl http://localhost:8080/healthz
docker compose down               # 停止（-v 同时删数据卷）
```

> 说明：FFI 依赖（libvirt / nftnl / mnl）在源码中是 **optional + 非默认 feature**（`virt-ffi` / `nftnl-ffi`），默认 `cargo build --workspace`（也是镜像构建用的命令）不编译 FFI 路径，故镜像 builder 阶段无需 `-dev` 头；运行期才装 `zfsutils-linux`（ZFS CLI）等。完整 ZFS/KVM/nftables 真实操作仍依赖宿主内核模块与特权。

### 构建与测试

```bash
# 1. 编译整个 workspace（26 crate）
cargo build --workspace

# 2. 运行测试套件（mock feature 开启内存/fixture 后端，无需 root 与真实硬件）
cargo test --workspace --features mock

# 3. 运行真实环境集成测（需对应系统能力 + root，默认 #[ignore] 不跑）
cargo test --workspace --features mock -- --ignored

# 4.（改前端时）重新构建 Web UI 并重嵌二进制
cd crates/os-api/web && npm install && npm run build && cd ../../..
cargo clean -p os-api && cargo build -p os-api   # rust-embed 增量不重嵌，须 clean
```

> 说明：workspace 产出 4 个可执行二进制——`osd`（系统编排守护进程 + 内嵌 API 网关）、`os-api`（独立 HTTP 网关 + Web UI，生产上以 systemd 服务运行，见 docs/DEPLOYMENT.md §9）、`os`（运维 CLI）、`os-mcp`（MCP Server）。直接运行：`cargo run --release -p osd -- --serve-api 0.0.0.0:8080`、`cargo run --release -p os-api -- --addr 0.0.0.0:8080`、`cargo run --release -p os-cli -- <subcommand>`。无需 `cargo run` 的体验方式见上方「Docker 快速开始」。

### 真实环境沙箱

需 root / 内核能力的操作（cgroup、systemd、ZFS、nftables、libvirt）配有可重复、不污染宿主的沙箱方案：Docker / QEMU / nspawn 三套，脚本在 `scripts/sandbox/`，详见 [`docs/SANDBOX.md`](docs/SANDBOX.md)。

---

## 架构概览

26 个 crate 按职责分层（自底向上），每层只依赖其下层：

```
┌──────────────────────────────────────────────────────────────┐
│ 客户端层    │ os-cli · os-mobile · os-desktop · os-mcp     │  ← 运维 CLI / 移动端 / 桌面端 SDK / MCP Server
├──────────────────────────────────────────────────────────────┤
│ 部署更新层  │ os-provision · os-iso · os-update             │  ← PXE 引导 / ISO 构建 / A/B 更新
│             │ os-im · os-api · os-nexhub                     │  ← IM agent / HTTP-WS 网关+Web UI / NexHub 大厅
├──────────────────────────────────────────────────────────────┤
│ 服务编排层  │ osd · os-services · os-meta · os-discover    │  ← 守护进程 / 监控+媒体 / Raft 共识 / 设备发现
│             │ os-guest                                        │  ← 访客网络认证
├──────────────────────────────────────────────────────────────┤
│ 协议计算层  │ os-protocols · os-compute · os-wallet         │  ← 文件/块协议 / VM+容器 / 链上钱包
├──────────────────────────────────────────────────────────────┤
│ 存储网络层  │ os-storage · os-network · os-security         │  ← ZFS / 网络防火墙 / 鉴权+证书
├──────────────────────────────────────────────────────────────┤
│ 核心层      │ os-core · os-common · os-i18n                 │  ← 领域类型/事件总线 / 公共 DTO+chain_auth / 多语言
└──────────────────────────────────────────────────────────────┘
        （另：os-integration = 跨 crate 端到端集成测；nettest = 网络连通冒烟，全 #[ignore]；
              crates/os-web/ = 旧前端残留存档，非 crate）
```

设计要点：
- **契约优先（Contract-First）**：trait + 数据结构 + Error 先行，实现后填；签名稳定，便于多 agent 并行。
- **真实与模拟分离**：每个外部 I/O（ZFS/libvirt/nftables/...）都有 `mock`/`fixture` 后端（默认测）与真实后端（`#[ignore]` 真实测），默认套件零环境依赖。
- **dyn 兼容与 async trait**：见 [`docs/adr/ADR-COMPAT-001`](docs/adr/ADR-COMPAT-001-async-trait-dyn-compat.md)。

---

## 文档索引

| 文档 | 说明 |
|------|------|
| [docs/README.md](docs/README.md) | **docs 总索引**——功能文档速览（路由表/env 一页可查），AI agent 协作入口 |
| [OS_系统_Rust技术路线规划.md](OS_系统_Rust技术路线规划.md) | 项目主规划（架构 / crate SSOT / 方法论 / 契约索引） |
| [docs/HANDOVER.md](docs/HANDOVER.md) | 交接文档——恢复全部上下文的唯一入口（现状 / 里程碑 / 决策） |
| [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) | 部署全流程（构建 / osd 启动 / 配置 / systemd / ISO / HA / 升级 / 监控） |
| [docs/DEPENDENCIES.md](docs/DEPENDENCIES.md) | 第三方依赖选型与注册说明（历史归档 + ADR 索引） |
| [docs/ERROR_GUIDE.md](docs/ERROR_GUIDE.md) | 错误码归类指引（跨 crate `From → ApiError` 审计） |
| [docs/SANDBOX.md](docs/SANDBOX.md) | 真实环境测沙箱方案（Docker / QEMU / nspawn） |
| [docs/PERFORMANCE_BASELINE.md](docs/PERFORMANCE_BASELINE.md) | criterion micro-benchmark 性能回归基线 |
| [docs/COVERAGE_REPORT.md](docs/COVERAGE_REPORT.md) | cargo-tarpaulin 测试覆盖率报告 |
| [docs/CODE_QUALITY_AUDIT.md](docs/CODE_QUALITY_AUDIT.md) | clippy pedantic 审计 + 重复依赖 / 未用依赖分析 |
| [docs/REVIEW.md](docs/REVIEW.md) | 两轮契约一致性审计（R1 + R2 全闭环） |
| [docs/TODO_AUDIT.md](docs/TODO_AUDIT.md) | 高 TODO crate 审计（保留 / 已补实现 / 废弃归类） |
| [docs/adr/](docs/adr/) | 架构决策记录（8 个 ADR，见下） |

### 架构决策记录（ADR）

| ADR | 主题 |
|-----|------|
| [ADR-COMPAT-001](docs/adr/ADR-COMPAT-001-async-trait-dyn-compat.md) | `Box<dyn>` 用的 async trait 一律 `#[async_trait]` |
| [ADR-COMPAT-002](docs/adr/ADR-COMPAT-002-datetime-fixed-utc-alias.md) | `os-core::DateTime` 固定为 UTC 时区 type 别名 |
| [ADR-COMPAT-003](docs/adr/ADR-COMPAT-003-eventbus-async-trait.md) | `EventBus` 补加 `#[async_trait]`（001 漏修补全） |
| [ADR-DEPS-001](docs/adr/ADR-DEPS-001-p0-p1-third-party-deps.md) | 注册 P0/P1 高频第三方依赖到 workspace |
| [ADR-DEPS-002](docs/adr/ADR-DEPS-002-p2-domain-specific-deps.md) | 注册 P2 领域专用依赖到 workspace |
| [ADR-DEPS-003](docs/adr/ADR-DEPS-003-aead.md) | 注册 `aes-gcm` 接通真实 AES-256-GCM 加密 |
| [ADR-DEPS-004](docs/adr/ADR-DEPS-004-acme.md) | 注册 instant-acme 接通 ACME 自动证书签续 |
| [ADR-DEPS-005](docs/adr/ADR-DEPS-005-clip-backend.md) | CLIP 推理后端选型——candle（纯 Rust + CUDA） |

---

## 开发状态

- **代码规模**：26 crate workspace（24 业务 crate + `os-integration` 跨 crate 集成测 + `nettest` 网络冒烟）；网关层 30 个 RouteHandler 组件、约 330 条 API 路由（grep spec() 统计；08-15 审计口径 304 + 转发/媒体生成等增量，见 docs/ARCHITECTURE.md §8.1）、31 个 Vue view（30 个桌面应用）。
- **测试**：`cargo test --workspace --features mock` **4,100+ 全绿**（2026-08-18 口径 4,100；ignored = 需 root / 真实硬件的真实环境测，默认不跑）。
- **覆盖率**：src 行覆盖约 **85%**（cargo-tarpaulin，18 crate；详见 [`docs/COVERAGE_REPORT.md`](docs/COVERAGE_REPORT.md)，口径为 2026-08-06 快照）。
- **代码质量**：`clippy -D warnings` 0 warning（含 pedantic 高价值 lint 已修）；`cargo fmt` 零差异；cargo-deny licenses 通过；GitHub Actions CI 全绿（fmt/clippy/test/bench/前端构建）。
- **真实环境验证**：26 项本机实跑验证（ZFS 池全链 / nftables 事务 / rtnetlink / libvirt VM 生命周期 / runc 容器 / SMB-NFS 落盘 / iSCSI+NVMe-oF configfs / FFmpeg 转码 / CLIP CUDA 推理 / cgroup v2 / systemd / chrony / bootloader / ISO 构建 / sd-turbo 生图 / aria2 / docker / rclone...），过程中发现并修复 10+ 个真实 bug。
- **生产运行**：开发机 os-api 以 systemd 服务（`os-api.service`）常驻 8080 端口（Web UI + API + git smart HTTP + /metrics），鉴权/计费/收款等 env 见 docs/DEPLOYMENT.md §9。

---

## License

`MIT OR Apache-2.0`（见 [`Cargo.toml`](Cargo.toml) `[workspace.package]`）。各 crate 沿用同一双许可证；尚无独立 `LICENSE` 文件，如需可按 SPDX `MIT OR Apache-2.0` 补全。
