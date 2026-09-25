# 基于 Rust 的 OS 系统技术路线规划（Ubuntu 26.04 · 含 HA / 虚拟机 / 容器）

> ⚠️ **状态提示（2026-08-20 加注）**：本文件是立项期的**规划 SSOT**（§3 的 22 crate 规格、§13 方法论、
> §15 契约索引）。实现已超出规划口径：workspace 现 **26 crate**（+os-nexhub/os-mcp 等）、
> 网关层 30 个 RouteHandler + Vue3 Web 桌面（30 应用）+ NexHub/链上身份/媒体生成等上层能力。
> **当前架构实况见 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)，进度现状见仓库根 MEMORY.md**；
> 本文件正文章节（§0–§16）保持原样未改动。

> 本文为**调研分析 + 技术路线规划文档**，不含任何实现代码。
> 适用范围：单集群可横向扩展、支持 HA 故障转移、可运行虚拟机与容器、Web 管理面为 macOS 风格、纯 Rust 应用栈、最低硬件以 Ubuntu 26.04 推荐配置为门槛。

---

## 0. 决策基线（已确认）

| # | 决策点 | 结论 |
|---|--------|------|
| 1 | 扩展性 / HA / 虚拟机 | 需要**可扩展 HA**，支持**虚拟机（KVM）** |
| 2 | 技术栈纯度 | **纯 Rust 应用栈**，允许长期迭代（新协议 crate 可早期采用） |
| 3 | 前端框架 | **Vue 3** |
| 4 | 容器 | **需要**，作为应用生态载体 |
| 5 | 最低硬件 | 以 **Ubuntu 26.04 推荐配置**为最低门槛 |
| 6 | UI 风格 | **类 macOS（苹果电脑）风格** |

> **关于"纯 Rust 栈"的务实定义**：我们的全部管理、编排、协议、集群代码均用 Rust 编写；但底层内核模块（ZFS、KVM）与系统守护进程（libvirt）是 C 实现、不可重造，由 Rust 通过进程/API 编排调用。这是系统级产品"纯 Rust 栈"的合理边界，下文据此选型。

---

## 1. 调研结论：生态现状（含 HA / VM / 容器）

### 1.1 运行环境（已核实）
- **Ubuntu 26.04 LTS「Resolute Raccoon」**（2026-04-23 发布，Linux 7.0 内核，ZFS 完善支持）[1]。
- **官方最低硬件已上调**：2 GHz 双核、≥6 GB RAM、25 GB 磁盘（自 2019 年来首次提高）[4]。本产品以**推荐配置（见 §6）为最低门槛**。

### 1.2 网络文件共享协议（纯 Rust）
| 协议 | Rust 实现 | 成熟度 | 路线 |
|------|-----------|--------|------|
| **SMB2/3** | 🔴 **无成熟纯 Rust Server**（`smb-server` 无法验证；现有 crate 均为 Client） | 🔴 致命 | **P1 编排 Samba**（同 KVM/libvirt 务实边界）；长期视生态决定是否自研（见 §11.3.1） |
| **NFS v3** | `nfsserve`（HuggingFace，用户态，作者称 incomplete but functional） | 🟡 可用，v3 | **采用**；企业级 NFSv4 短期编排 `nfs-ganesha`（C），自研列远期（见 §11.3.2） |
| **WebDAV** | `dav-server` | 🟢 成熟 | **采用** |
| **FTP(S)** | `libunftp` | 🟢 成熟 | **采用** |
| **SFTP** | `russh` | 🟢 成熟 | **采用** |
| **S3/对象** | `RustFS`（纯 Rust，S3 兼容，性能对标 MinIO） | 🟢 活跃 | **采用**，独立组件 `os-object`（类 MinIO） |

### 1.3 集群 / HA / 容器 / 虚拟机（纯 Rust 优先）
| 能力 | 选型 | 说明 |
|------|------|------|
| **共识 / HA 编排** | `openraft`（Rust Raft，Databend meta 同款） | 成熟，纯 Rust 集群一致性引擎[2] |
| **容器运行时** | `youki`（Rust OCI runtime，过 containerd e2e） | 生产可用，纯 Rust[3] |
| **镜像拉取** | `oci-distribution`（Rust 镜像仓库客户端） | 配 youki 完成拉取+运行 |
| **虚拟机** | KVM/QEMU + `libvirt`（Rust 绑定或直控 QEMU） | hypervisor 为 C，管理面纯 Rust |
| **共享/复制存储** | ZFS `send/recv` 复制（含 mirror 拓扑）→ 后期 Ceph | HA 存储底座；同级 peer 无上限 ZFS mirror 同步 |

### 1.4 参考架构
- **TrueOS SCALE**：Linux + ZFS + Samba + 内核 NFS + 容器 + 中间件 + Web UI，分层编排思想可借鉴[参考]。
- **Databend / youki / openraft**：证明 Rust 已能在存储与基础设施层承担生产级职责。

---

## 2. 总体架构（集群化，分层）

```
                    ┌──────────────── 客户端 ────────────────┐
     Windows/macOS  │  Linux/ESXi  │  浏览器(Vue)  │ 备份/CI  │
                    └──┬─────┬─────┬──────────┬─────┬────────┘
            SMB │ NFS  │WebDAV│ FTP/SFTP  │ S3  │ 管理UI(REST/WS)
                ▼     ▼     ▼          ▼      ▼      ▼
┌─────────────── 协议层（全部 Rust 常驻服务）─────────────────┐
│ os-smb │ os-nfs │ os-webdav │ os-ftp │ os-sftp │ RustFS │
└───────────────────────────┬───────────────────────────────┘
            ▲ 挂载点/数据集      │ 块设备
┌───────────┴──────── 计算层（VM + 容器，Rust 编排）──────────┐
│ os-vm(KVM/libvirt,Rust) │ os-app(youki+oci-distribution)  │
└───────────────────────────┬───────────────────────────────┘
            ▲ 磁盘/镜像        │
┌───────────┴──────── 网络层（os-network，可自定义接口）─────┐
│ 接口/VLAN/桥/绑定/NAT │ 防火墙 │ 按接口挂载 DHCP/PXE/DNS 服务 │
└───────────────────────────┬───────────────────────────────┘
            ▲ 绑定网卡        │
┌───────────┴──────── 存储层（Rust 编排 ZFS）────────────────┐
│ os-storage：池/数据集/快照/配额/压缩/LUKS/复制(send|recv)  │
└───────────────────────────┬───────────────────────────────┘
            ▲                 │
┌───────────┴── 集群控制面（纯 Rust，openraft 共识）─────────┐
│ os-meta：节点成员/选主/分布式KV/故障检测/故障转移编排/VIP  │
└───────────────────────────┬───────────────────────────────┘
            ▲                 │
┌───────────┴── 节点发现与联邦层（os-discover，独立组件）────┐
│ LAN发现(mDNS)→凭证认证互联→HA资格检测→建HA集群/同级+同步     │
└───────────────────────────┬───────────────────────────────┘
            ▲                 │
┌───────────┴── 系统分发/迁移层（os-provision，类手机换机）─┐
│ PXE自举(依赖os-network)→系统初始化→分阶段传输(除密码)      │
│ （发现/认证/联邦复用 os-discover）                         │
└───────────────────────────┬───────────────────────────────┘
            ▲                 │
┌───────────┴── 功能服务层（七独立组件，可单独发布）────────┐
│ os-backup │ os-monitor │ os-media │ os-security │       │
│ os-files  │ os-devtools│ os-power                          │
└───────────────────────────┬───────────────────────────────┘
            ▲                 │
┌───────────┴── 访客接入管理层（os-guest，可选）────────────┐
│ Captive Portal · 访客身份(随机ID/JWT/公钥) · RBAC策略引擎    │
│ nftables 规则编排 · 生命周期管理 · 与 os-im/os-network 协同│
└───────────────────────────┬───────────────────────────────┘
            ▲                 │
┌───────────┴──── 管理面（Rust Axum：REST + WebSocket）──────┐
│ 配置中心│用户权限│任务调度│事件总线│审计│Web UI 后端          │
│ os-im：核心 IM + AI agent 中枢（对话即操作，SDK 编排各组件）│
│ 持久化：SQLite(redb/rusqlite) 存元数据与配置                 │
└───────────────────────────┬───────────────────────────────┘
            ▲ 客户端/接入      │
┌───────────┴──── 客户端/接入层 ─────────────────────────────┐
│ os-mobile(手机App:发现/手动IP/状态) │ os-desktop(桌面:Win优先,挂载)│
└──────────────────────────────────────────────────────────┘
        底座：Ubuntu 26.04 LTS + systemd + OpenZFS + KVM
```

---

## 3. 技术选型决策（纯 Rust 栈）

> **组件清单的单一来源（SSOT）见 §3.0 组件总表**；本节各小节为各组件的展开说明，与总表一一对应。章节按层级与依赖顺序编排（编号已重整，无字母后缀）。

### 3.0 组件总表（SSOT，单一来源）

> 本表为全系统组件的**唯一权威清单**。§8 crate workspace 清单、§11.4.3 工期表均引用此表，不再各自重复定义。"状态"列：🟢 初版纳入 / 🟡 v2+ / 🔴 远期研究。

| # | 组件 | 层级 | 职责（一句话） | 关键依赖 crate | 依赖的其他组件 | 阶段 | 初版工期(人天) | 状态 |
|---|------|------|----------------|---------------|---------------|------|--------------|------|
| 1 | `os-core` | 基础 | 领域模型/领域类型/**EventBus trait**（节点内事件总线，见 §9.1#9） | tokio(broadcast) | - | P0 | 含于#20 | 🟢 |
| 2 | `os-common` | 基础 | API schema/契约/共享类型 | - | os-core | P0 | 含于#20 | 🟢 |
| 3 | `os-i18n` | 基础 | 国际化 SSOT（简中/繁中/英） | fluent/rust-i18n | - | P0 | 含于#20 | 🟢 |
| 4 | `osd` | 系统编排 | PID1 后编排器，进程管理+cgroup v2/**NTP(chrony 编排)**（见 §9.1#8） | cgroups-rs/tokio/chrony | 全体组件 | P0 | 56(+4) | 🟢 |
| 5 | `os-storage` | 存储 | ZFS 池/数据集/快照/配额/加密/send-recv/**块存储 export(iSCSI·NVMe-oF)**（见 §9.1#11） | tokio::process/tgt/nvmet | os-core | P1 | 108(+8) | 🟢 |
| 6 | `os-protocols` | 协议（聚合） | smb/nfs/webdav/ftp/sftp/object 子模块 | samba编排/nfsserve/dav-server/libunftp/russh/RustFS | os-storage | P1 | 130 | 🟢/🟡 |
| 7 | `os-compute` | 计算（聚合） | vm/app/container-net/pkg 子模块 | libvirt/youki/oci-distribution/rtnetlink/nftnl | os-storage/os-network | P3 | 194 | 🟢 |
| 8 | `os-meta` | 集群 | openraft 共识/选主/KV/VIP/故障转移 | openraft | os-storage/osd | P2 | 120 | 🟢 |
| 9 | `os-discover` | 发现/联邦 | mDNS 发现+mTLS 互联+HA 资格检测 | mdns-sd/rustls | os-meta/os-network | P5/P6 | 40 | 🟢 |
| 10 | `os-provision` | 分发/迁移 | PXE 自举+阶段化迁移（类手机换机） | tokio/reqwest | os-network/os-discover/os-storage/os-meta | P5 | 40 | 🟢 |
| 11 | `os-iso` | 封装 | 可安装 ISO 打包（标准+克隆） | xorriso/squashfs-tools | 全体（构建期） | P8 | 30 | 🟢 |
| 12 | `os-update` | 系统更新 | OTA A/B 分区+回滚+CVE 监听 | bootloader/ostree(可选) | osd/os-iso | P8 | 76 | 🟢 |
| 13 | `os-network` | 网络 | 接口/VLAN/桥/防火墙/DHCP/PXE/DNS/IB-RoCE/DPU | rtnetlink/nftnl/dora/hickory-dns/async-rdma | os-core | P1/P4 | 80(+RDMA/DPU 远期) | 🟢 |
| 14 | `os-api` | 管理面 | Axum REST+WebSocket 网关+tower 中间件（**内嵌 API 网关**：TLS 终止/限流/认证，不独立成层，见 §9.1#10） | axum/tokio/tower | 全体（聚合） | P1 | 80(含 cli) | 🟢 |
| 15 | `web-ui` | 管理面 | Vue3+Vite macOS 风设计系统 | vue/vite/pinia/axios/vue-i18n | os-api/os-i18n | P1 | 120 | 🟢 |
| 16 | `os-im` | 管理面/中枢 | 核心 IM+AI agent 中枢+多 agent 协作+SDK | axum/openai-sdk/candle(可选) | 全体（agent 调度） | P7 | 40(基础) | 🟢 |
| 17 | `os-wallet` | 身份/凭证（新增） | 多链钱包连接+签名+凭证查询（BTC+EVM+WalletConnect）；**EVM 直查 RPC、Ordinals 自托管 ord index+外部 fallback**（见 §9.1#12）；**WC Relay 默认公共可切自托管**（见 §9.1#13） | rust-bitcoin/alloy/walletconnect-relay/ord index | os-security/os-guest(被调)/os-im(被调) | P3.5 | 61(+6) | 🟢 |
| 18 | `os-guest` | 访客（可选） | Captive Portal+访客身份+RBAC+IM 集成 | axum/jsonwebtoken/nftnl/ed25519-dalek | os-network/os-security/os-im/os-wallet/os-api | P3.5 | 170 | 🟢 |
| 19 | `os-cli` | 管理面 | 命令行工具 | clap | os-api | P1 | 含于#14 | 🟢 |
| 20 | 基础三件套 | 基础 | os-core+os-common+os-i18n 合计 | - | - | P0 | 60 | 🟢 |
| 21 | `os-services`(七子) | 功能服务 | backup/monitor/media/security/files/devtools/power | opentelemetry/tantivy/oauth2/totp/rcgen/boringtun/gix 等 | 各自依赖（见 §3.16） | P9 | 90(初版三子) | 🟢/🟡 |
| 22 | `os-mobile` | 客户端 | iOS/Android App（发现/状态/配对） | Capacitor/Vue | os-api/os-discover | P6 | 含于客户端 | 🟡 |
| 23 | `os-desktop` | 客户端 | Tauri 桌面（Windows 优先，挂载） | Tauri/Vue | os-api/os-discover | P6 | 含于客户端 | 🟡 |
| 24 | 客户端合计 | 客户端 | os-mobile+os-desktop | - | - | P6 | (v2 估算) | 🟡 |
| 25 | 测试/CI/文档 | 工程 | 集成测+多节点仿真+文档 | - | 全体 | 全程 | 100 | 🟢 |

> **去重说明**：原 §8 workspace 清单（20 crate）与本表差异——本表新增 `os-wallet`（#17），并明确 `os-protocols`/`os-compute` 为聚合 crate（内部子模块对应原 smb/nfs/container-net 等）。聚合 crate 的子模块工期已拆分计入对应行。

---

### 3.1 底座
- Ubuntu 26.04 LTS Server（最小化）；`rustup` stable；systemd 管服务；OpenZFS；开启 KVM（`/dev/kvm`）。
- 节点间：≥1 GbE 管理网 + 建议 10 GbE 存储/迁移网（用于 ZFS 复制与 VM 迁移）。
- **国际化（os-i18n，基础层）**：初始支持**简中/繁中/英文**三语，架构可扩展其他语种；翻译资源（TOML/JSON）+ 运行时切换；作为基础层组件供所有用户可见界面共用——web-ui、手机 App、桌面软件、安装器、CLI 帮助文本、IM/agent 回复均经此取本地化文案。技术：`fluent`（Mozilla，Rust）或 `rust-i18n` crate；翻译键统一命名规范，新语种仅需新增资源文件。

### 3.2 存储层（Rust 编排 ZFS）
- 通过 `tokio::process` 调用 `zpool`/`zfs`/`cryptsetup`，输出以 `-p -H` 机器可读格式解析建模。
- `StorageBackend` trait 抽象，HA 下提供 **active-passive 复制**（ZFS `send/recv`）+ 故障切换；active-active（Ceph）列为后期。
- **同级节点同步（无上限）**：同级 peer 间 ZFS 复制采用 **1 主 N 从只读副本**模式（单向 send/recv，非多主写入）；同级节点数量**无上限**，不进 openraft 法定；初期不支持多主 mirror（ZFS 原生不支持多主，工程复杂度极高），多主同步列为远期研究课题（参考 CRDTs/Syncthing 协议思路，见 §11.2.3）。**（主表述见 §3.5，此处为存储层视角的引用）**
- **块存储 export 子模块**（见 §9.1#11 决策）：iSCSI Target（编排 `tgt`/`LIO`，C 内核 target）+ NVMe-oF Target（编排 `nvmet`，内核 target）——把 ZFS 卷（zvol）以**块设备**形式 export 给 ESXi/Windows/VM。块协议归存储层（非协议层），因 export 的是 zvol 而非文件/对象；Rust 侧做配置管理与 LUN 映射。

### 3.3 协议层（全部 Rust，SMB 除外）
- **SMB（编排 Samba）**：🔴 经核实无成熟纯 Rust SMB Server；Rust 负责 smb.conf 生成、用户映射、共享管理、会话监控，smbd 协议栈由 C 实现——纳入"纯 Rust 栈"务实边界（同 KVM/libvirt）。支持 Time Machine（vfs_fruit 扩展）。
- `nfsserve`（NFSv3）、`dav-server`、`libunftp`、`russh` 均为 Rust 常驻服务，统一认证与日志。企业级 NFSv4 短期编排 `nfs-ganesha`（C），自研列远期。
- **对象存储（os-object）**：基于 `RustFS`（纯 Rust、S3 兼容、性能对标 MinIO），提供桶/对象 CRUD、生命周期、版本控制、分片上传、访问策略；后端落 ZFS 数据集；HA 下可分布式部署水平扩展——即"类似 MinIO 的文件存储"。
- ~~**iSCSI Target**~~ → **已迁移到存储层**：块协议（iSCSI/NVMe-oF）归 `os-storage` 的 block-export 子模块（见 §3.2 / §9.1#11）；本协议层仅含文件/对象协议。
- 认证：自建用户库 → 映射 Linux UID/GID；可选 PAM（`pam-sys`）；S3 用 Access Key/Secret。

### 3.4 计算层
- **虚拟机**：Rust 通过 `libvirt` Rust 绑定（或直控 QEMU 进程）管理生命周期；磁盘后端用 ZFS 卷（zvol）。
- **容器**：`youki` 作 OCI 运行时，`oci-distribution` 拉镜像，Rust 编排应用生命周期（类"应用市场"）。
- **第三方软件包（os-pkg）**：允许安装第三方 Ubuntu 26.04 的 `.deb`；Rust 编排 `apt`/`dpkg` 完成安装/卸载/升级；解析包内 `.desktop` 文件，凡带图标的第三方应用**统一归入"未知来源"分组**展示，与容器应用市场明确区分；记录安装清单便于卸载与迁移。需 root，建议沙箱/依赖隔离；HA 下第三方包默认仅装单节点、不参与复制。
- **容器网络（os-container-net）**：youki 是低层运行时（≈runc）不含网络；本组件补全——CNI 插件、veth/bridge/NAT/端口映射、容器间 DNS 解析、ZFS 卷→容器 bind mount、网络隔离；与 `os-network` 协同但独立维护。

### 3.5 集群控制面（HA 核心，纯 Rust）
- `openraft` 实现节点共识：成员管理、Leader 选举、分布式 KV（集群状态）。
- 故障检测 + 转移：Leader 在检测到节点失联后，将 VIP（浮动 IP，Rust 编排 `ip`/网络命名空间）与 VM/容器调度到健康节点。
- 存储 HA：复制模式先上；共享存储（Ceph）后置。
- **HA 集群 vs 同级 peer**：openraft 只管 HA 集群成员（法定数有限，如 3/5/7 节点）；同级 peer 节点**无上限**、不进法定、不走共识，仅经 `os-discover` 注册并由 `os-storage` 做 ZFS mirror 同步。两者分层：HA 管故障转移，同级管数据冗余。

### 3.6 管理面（Web API + UI + 持久化）
- 后端 `axum` + `tokio` + `tower`；REST 管配置，WebSocket 推事件/进度。
- 持久化 `rusqlite`/`sqlx`（SQLite）或 `redb`（KV）；**文件数据不入库**。
- **前端 Vue 3 + Vite**，macOS 风格设计系统（见 §7）；国际化经 `os-i18n`（见 §3.1）。
- API 从 Day 1 版本化（`/api/v1/`），WebSocket 消息带 schema 版本字段（见 §12.3）。
- **内嵌 API 网关**（见 §9.1#10 决策）：`os-api` 承担统一入口职责——TLS 终止（rustls）、限流、认证中间件、路由聚合各组件 REST 接口；**不独立成网关层**（避免"为分层而分层"）。需负载均衡时 os-api 自身多副本，由 os-meta VIP 调度。
- 本节仅描述"管理面基础设施"；**核心 IM + AI agent 中枢 + 多 agent 协作架构**已提升为独立子章节 §3.7。

### 3.7 核心 IM 与多 Agent 协作中枢（os-im）

> **定位**：系统的对话即操作入口 + AI agent 宿主 + 多 agent 协作运行时。深度集成全部执行组件，自然语言下指令即完成运维操作。本节既描述 os-im 组件本身，也描述**多 agent 运行时协作架构**（产品架构层）。

#### 3.7.1 os-im 组件职责
- **对话即操作**：用户自然语言下指令（建 VM、起容器、同步任务、存储操作、开通访客等），IM 解析后调度 agent 执行。
- **AI agent 宿主**：承载 agent 运行；agent 经 **SDK**（Tool/Function Calling 接口）调用 `os-vm`/`os-app`/`os-storage`/`os-object`/`os-wallet`/`os-guest` 等执行组件。
- **SDK 开放**：对外提供 Rust SDK，定义 Tool 接口、权限范围、确认回调；第三方/自建 agent 可注册接入。部分 Tool 为**条件激活型**——仅当依赖能力可用时才注册暴露（如 `wallet.sign`/`guest.auth.chain` 仅在 `os-wallet` 的 RpcRegistry 探测到链 RPC 可用时生效，见 §3.17）。
- **通知与事件**：系统事件（HA 切换、快照完成、磁盘故障）经 IM 推送；agent 也可主动汇报。
- **权限与确认**：agent 受 RBAC 限制；高危操作需用户在 IM 内确认；全量审计。
- **LLM 接入**：支持云端（OpenAI 等）与本地模型；离线/隐私场景走本地推理（本地推理为可选，硬件要求见 §11.2.5）。

#### 3.7.2 多 Agent 运行时协作架构（产品架构）

系统部署后，多个 AI agent 在 os-im 中枢内分工协作。采用**中枢-领域（Orchestrator-Specialist）拓扑**：

- **中枢 Agent（Orchestrator）**：运行在 os-im，负责意图解析、任务分解、调度、汇总、对外对话。本身不直接操作系统资源，只委派。
- **领域 Agent（Specialist）**：各管一域，注册各自 Tool 到中枢。初版规划如下领域 agent：

| 领域 Agent | 拥有的执行组件 | 典型职责 |
|-----------|---------------|---------|
| StorageAgent | os-storage / os-protocols | 建池/快照/共享/复制 |
| ComputeAgent | os-compute(vm/app/container-net/pkg) | 起容器/建 VM/装包 |
| MetaAgent | os-meta / os-discover | 集群成员/故障转移/节点发现 |
| NetworkAgent | os-network | 接口/防火墙/VLAN/RDMA |
| SecurityAgent | os-security | 签 JWT/2FA/证书/VPN |
| GuestAgent | os-guest | 访客身份/Portal/RBAC |
| WalletAgent | os-wallet | 钱包连接/签名/凭证查询 |
| ServiceAgent | os-services(七子) | 备份/监控/媒体等 |
| ProvisionAgent | os-provision / os-iso / os-update | 部署/迁移/升级 |

- **协作原语**（5 个）：
  1. **任务委派（Task Delegation）**：中枢把子任务分发给领域 agent；带任务 ID、上下文、超时、回执。
  2. **能力发现（Capability Discovery）**：领域 agent 启动时向中枢注册 Tool 清单与权限边界；中枢维护能力目录（含条件激活 Tool 的可用性状态）。
  3. **上下文共享（Shared Context Blackboard）**：多 agent 协作同一目标时共享黑板（任务背景、已收集信息、中间结果），避免重复询问用户。
  4. **确认与投票（Confirmation & Quorum）**：高危操作（删池/重置/开源/转账类）需用户 IM 内确认；跨域高危变更可配多 agent 会签（如 Storage+Security 双签）。
  5. **结果聚合（Result Aggregation）**：中枢收集各领域 agent 回执，聚合为对用户的单一回复。
- **通信机制**：节点内 tokio broadcast/MPSC 事件总线（呼应 §10.2#8 事件总线）；跨节点走 openraft log（os-meta）。Agent 间消息带 trace_id，全链路可观测（呼应 §3.16 os-monitor）。
- **示例编排**："给这个访客开通 BTC 验证" → Orchestrator 解析意图 → 委派 GuestAgent（建访客身份）+ WalletAgent（建钱包连接/验签）+ SecurityAgent（签链上凭证 JWT）→ 三 agent 经黑板协同 → 聚合结果 → 回报用户。
- **约束与红线**：每个 agent 受 RBAC 限制、Tool 权限边界明确；agent 间不得越权直接操作系统资源（必须经所属执行组件的 API）；高危操作 IM 内确认；全量审计；防委派死循环（任务图必须无环，中枢检测循环并拒绝）。
- **开发期方法论**：本架构如何用多个 AI 编码 agent 协作开发，见 §13（工程方法论，独立成章）。

### 3.8 安全
- TLS：`rustls`；审计：`tracing` 结构化日志；最小权限（Capability/polkit 仅对 ZFS/systemd 提权）。

### 3.9 网络层（os-network，可自定义接口功能）
- **接口管理**：用 `rtnetlink`/`neli`（netavark 同款生产级方案）配置接口、VLAN、桥、绑定；防火墙/NAT 用 `nftnl`（nftables 绑定）。
- **高性能互联（IB / RoCE，RDMA）**：
  - **InfiniBand**：编排 `ibverbs`/`rdma-core`（OFED 栈，`ib_*`/`rdma` CLI 与内核模块 `ib_core`/`ib_ipoib`）；支持 IPoIB（给 IB 接口配 IP）与原生 RDMA 传输。
  - **RoCE v2**：在以太网卡上跑 RDMA，依赖网卡驱动（mlx5 等）+ `rdma-core`；通过 DCB/PFC 配置无损以太网。
  - **用途**：ZFS 复制/迁移、集群心跳、VM 迁移走 RDMA 通道，显著降低延迟与 CPU 占用；与 `os-meta`（HA）、`os-provision`（迁移）、`os-storage`（复制）协同。
  - **Rust 路径**：`async-rdma`（纯 Rust RDMA/verbs 封装）或 FFI 绑 `rdma-core`；能力探测（是否有 IB/RoCE 设备）经 `rdma link`/`ibv_devinfo` CLI。
- **DPU 支持（带内 / 带外，如 NVIDIA BlueField）**：
  - **带内（in-band）**：主机经 PCIe 访问 DPU 卸载能力——将存储（NVMe-oF）、网络（OVS offload/SmartNIC）、压缩/加密卸载到 DPU，降低主机 CPU 占用；Rust 经 `rtnetlink`/`devlink` 探测 SF（subfunction）/VF 并编排卸载规则。
  - **带外（out-of-band）**：DPU 自带管理口（独立 ARM SoC + 独立 OS，如 BlueField 上的 DPU OS），主机宕机时仍可管理；Rust 经 DPU 的 REST/Redfish/IPMI/SSH（`reqwest`/`russh`）做带外管控——电源、固件、监控、隔离恢复。
  - **用途**：存储路径卸载（NVMe-oF target 在 DPU）、网络加速（OVS/VxLAN offload）、HA 带外恢复（主机故障时 DPU 仍能上报与接管触发）；与 `os-storage`、`os-meta`、`os-network` 协同。
  - **Rust 路径**：带内走 `devlink`/`rtnetlink` CLI 编排 + DPU 厂商 SDK 的 Rust 绑定（如 DOCA Rust 绑定，可选）；带外走标准带外协议（Redfish/IPMI）经 `reqwest`/纯 Rust 客户端。
- **按接口挂载可插拔网络服务**：
  - **DHCP**：对外分配 IP/网关/DNS，支持固定租约——用 `dora`（Rust 服务端）或基于 `dhcprs` 自建。
  - **PXE**：网络启动 = DHCP 选项 66/67（next-server + bootfile）+ TFTP 提供 iPXE 引导；TFTP 可参考 `pxe-np` 自建最小化服务。
  - **DNS**：局域网解析用 `hickory-dns`（纯 Rust）。
- 设计要点：网络服务与接口解耦，管理面可按网卡��选启用；IB/RoCE 作为"高速互联"可选能力，UI 显示 RDMA 设备状态与带宽；该组件需 root/Capability，独立测试与发版。

### 3.10 系统分发/迁移层（os-provision，类手机换机）
- **定位**：一套**依赖其他组件**的系统自举能力——PXE 拉起新节点并分阶段迁移（类手机换机）。**网络内节点发现、凭证认证互联、HA 资格检测与联邦编排已抽离为独立组件 `os-discover`（见 §3.14），本组件复用之**。
- **阶段1 系统初始化**：目标节点 PXE 启动（依赖 `os-network`）→ 分区 → 装基础系统 → 建 ZFS 池 → 拉起 `osd` 空壳；先成为"可用的空 OS"。
- **阶段2 传输文件**：经 `os-discover` 完成互联后，分批迁移配置、共享、用户定义（不含密码）、VM/容器定义、数据；数据走 ZFS `send/recv`，配置走迁移包。
- **排除项**：迁移排除 `/etc/shadow`、TLS/SSH 私钥、SMB/应用凭证；目标端重设密码、重生成密钥。**（统一的"密钥/数据排除清单"见 §3.19）**
- **依赖**：`os-network`（PXE）、`os-discover`（发现/认证/联邦）、`os-storage`（send/recv）、`os-meta`（入集群）；传输需断点续传 + 进度展示。

### 3.11 封装/打包层（os-iso，独立组件）
- **定位**：把整套系统（所有组件二进制 + Ubuntu 26.04 底座 + 配置）打包成**可安装 ISO 镜像**，作为离线/冷启动分发手段——与 `os-provision`（PXE 在线自举）互补。
- **打包内容**：全部 Rust 组件二进制（osd 及各服务）、Ubuntu 26.04 最小底座、ZFS/OpenZFS/KVM/libvirt 等系统依赖、web-ui 静态资源、默认配置骨架、安装器（含分区/建池/建用户引导）。
- **排除项**：用户密码与凭证、所有用户数据、集群密钥——ISO 只含"系统本体"，安装后是空白 OS，首启需重设密码/重生成密钥。**（统一排除清单见 §3.19）**
- **变体**：可生成"标准安装 ISO"（空系统）与"克隆 ISO"（含当前配置快照，仍排除密码与数据，用于批量部署同构节点）。
- **Rust 路径**：编排 `xorriso`/`squashfs-tools`/`initramfs-tools` 生成 ISO；安装器用 Rust 编写（基于 `ncurses`/TUI 或复用 Vue 的轻量 Web 引导）；构建脚本集成进 CI。
- **依赖**：无运行时依赖（构建期组件），但产出的 ISO 安装后会拉起全部组件；与 `os-provision` 共用排除清单与首启流程。

### 3.12 系统更新层（os-update，独立组件）
- **定位**：已部署节点的**在线升级/回滚机制**——与 `os-iso`（离线安装）互补，覆盖运行中系统的 OTA 更新。
- **更新模式**：A/B 分区（双系统槽位，升级写另一槽，启动失败自动回滚）或 ostree 式原子更新（文件系统树级切换）。
- **组件级滚动升级**：HA 下逐节点更新（先升级 follower，验证后再升级 leader），避免服务中断。
- **CVE 监听**：监听 Samba/QEMU/rdma-core 等 C 依赖的 CVE 公告，推送安全更新建议。
- **Rust 路径**：Rust 更新引擎（下载/校验/写入/切换启动项）；A/B 分区用 `bootloader` 配置切换；可选 `ostree` 集成。

### 3.13 系统编排层（osd，独立组件）
- **定位**：PID 1 之后的"OS 编排器"，统管 20+ Rust 组件 + Samba + QEMU + youki 的进程生命周期。
- **职责**：组件健康检查、自动重启（退避策略）、依赖排序启动、资源配额（cgroup v2 的 CPU/内存/IO 限制）、优雅停机。
- **NTP 时间同步**（见 §9.1#8 决策）：osd 编排 `chrony`（非纯 Rust，纳入务实边界，同 Samba/KVM）保证节点时钟一致——这是 openraft 共识、ZFS 快照时间戳、证书验证的**前置依赖**，须最早启动；需 root/CAP_SYS_TIME。
- **与 systemd 关系**：每个组件为独立 systemd service，osd 作为上层编排器补充 systemd 不擅长的跨组件依赖编排与高级健康策略。
- **HA**：HA 下每个节点各跑一个 osd 实例，由 `os-meta` 协调（如 leader 节点的 osd 负责主服务，follower 的 osd 维持 standby）。

### 3.14 节点发现与联邦层（os-discover，独立组件）
- **定位**：被 `os-provision`、集群与各类客户端（手机/桌面）复用的**节点发现与联邦引擎**，独立维护。
- **节点发现（LAN）**：监听/广播局域网 beacon（UDP 组播 / mDNS-DNS-SD，`mdns-sd`），发现同网段 OS 节点及能力（版本、架构、是否 HA 成员、存储/网络规格）；beacon 签名防 spoof。
- **凭证认证互联**：引导令牌 / 管理员凭证 / 配对码做双向认证（mTLS，`rustls`），通过才互联，类似设备配对。
- **HA 资格检测**：硬指标探测——节点数≥法定、硬件/内核/ZFS 特性兼容、带宽达标（HA 建议 10GbE）、复制能力可用、版本兼容。
- **分支决策**：
  - 符合 HA 且需要 → 调 `os-meta` 加入 openraft 集群，配 VIP + ZFS 故障转移。
  - 不符合（或用户不选）→ 作同级 peer，对选定数据集 ZFS mirror 同步（冗余而非自动故障转移）；**同级 peer 无数量上限**，不进 HA 法定。
- **对外接口**：暴露发现/互联 API，供 `os-provision`、手机 App、桌面软件共同调用（它们都要"发现局域网 OS / 手动 IP 连接"）。

### 3.15 客户端 / 接入层（os-mobile、os-desktop）
- **os-mobile（手机 App，iOS/Android）**：发现局域网 OS（复用 `os-discover` 的 mDNS/组播协议）、手动输入 IP 连接、查看容量/健康、凭证登录与配对、触发迁移/同步、接收告警推送。技术：跨平台优先复用 macOS 风 Vue UI（Capacitor/Ionic 或 Flutter），亦可原生（Swift/Kotlin）；注意 iOS 本地网络权限与后台限制。
- **os-desktop（桌面软件，首期 Windows）**：发现局域网 OS、手动输入 IP、一键挂载为网络驱动器（SMB/WebDAV）、状态与文件管理、配对。技术：**Tauri（Rust 后端 + 嵌入 Vue macOS 风 UI）**——复用 `web-ui` 设计系统，与纯 Rust 栈一致；Windows 挂载用 `net use`/WebDAV，需处理安装包签名与权限。
- 二者均通过 `os-api`（REST/WebSocket）与 `os-discover` 的发现协议接入；移动端偏"管理与陪伴"，桌面端可"挂载使用"。

### 3.16 功能服务层（七独立组件）
- **os-backup（备份/灾备/快照策略）**：ZFS 快照调度（定时/事件触发）、远程复制、3-2-1 备份、灾备演练（验证可恢复性）、RPO/RTO 配置；依赖 `os-storage` 的快照与 send/recv。
- **os-monitor（监控/告警/可观测性）**：统一指标(metrics)+日志(logs)+链路(traces)，基于 `opentelemetry`+`prometheus`+`tracing`；告警规则引擎，告警经 `os-im`/邮件/Webhook 推送；HA 下聚合多节点指标。
- **os-media（媒体/相册/流媒体）**：照片视频管理、人脸/CLIP 识别、转码(HLS)、DLNA 投屏；Rust 媒体处理 + `tantivy` 元数据搜索 + FFmpeg(CLI) 转码；参考 Immich 架构用 Rust 重写；数据落 ZFS。
- **os-security（安全增强）**：2FA(`totp-rs`)、OAuth2/OIDC SSO(`oauth2`/`openidconnect`)、证书管理(CA `rcgen`/ACME 自动签续)、VPN(`boringtun` 纯 Rust WireGuard 用户态)；2FA/OAuth2 统一接入各组件，VPN 供远程接入。
- **os-files（文件管理/搜索/同步/分享）**：Web 文件管理器、全文搜索(`tantivy`)、分享链接(限时/密码/限速)、客户端同步；搜索索引落 ZFS；同步与 `os-backup` 协同。
- **os-devtools（运维/开发者工具）**：日志聚合、CI 流水线、Git 服务(`gix` 纯 Rust)、密钥/凭证管理(加密 KVS)；面向开发者，与 `os-monitor` 日志互补；密钥加密存储独立于系统密钥。
- **os-power（电源管理/UPS/硬件监控）**：UPS(NUT 协议，断电自动关机保护 ZFS)、定时开关机(RTC 唤醒)、CPU/磁盘温度/风扇/SMART 健康(`smartctl`/`lm-sensors`)；硬件异常经 `os-monitor` 告警。
- 设计要点：七组件均可独立发布/回滚；通过 `os-api` 暴露能力，经 `os-im` 可被 agent/自然语言调用；`os-monitor` 为其余组件提供可观测性埋点。

### 3.18 访客接入管理层（os-guest，独立组件，可选）

- **定位**：访客网络接入与身份管理中枢——跨网络层（流量拦截/放行）、安全层（身份签发）、IM 层（群组集成）三大领域的独立业务域。让局域网内的访客设备通过 Captive Portal 完成身份注册，获得管理员授权的受限网络访问与 IM 群接入能力。
- **典型场景**：局域网内有路由器（公开访客 WiFi）+ OS。手机连访客 WiFi 后不能联网也不能接入 OS，被 Captive Portal 拦截后在页面生成访客 ID（随机 ID / 公钥身份 / 链上凭证身份），凭此 ID 获得网络接入权限并加入指定访客 IM 群，权限受管理员分配控制。
- **内部模块**：

| 模块 | 职责 | 关键 Rust 实现 |
|------|------|---------------|
| **portal-server** | Captive Portal HTTP 服务（axum :8081/:8082），拦截未认证流量、重定向到落地页、兼容多 OS 探测 | `axum` + `tower` 中间件 |
| **identity-engine** | 访客 ID 生成（随机 ID + JWT 签发/验证 + ed25519 公钥升级）；链上验证的**签名/连接下沉到 `os-wallet`**（见 §3.17），本模块仅做业务编排 | `jsonwebtoken` + `rand` + `ed25519-dalek` |
| **lifecycle-manager** | 访客会话状态机（pending → authed → expired/revoked）+ 定时清理过期会话 | `tokio` 定时任务 + `rusqlite` 状态存储 |
| **policy-engine** | 模块级 RBAC 策略评估（IM 群/文件共享/带宽/时间限额/验证因子条件） | `serde` 策略模型 + 自定义规则引擎 |
| **nft-rule-orchestrator** | nftables guest 链/nft set 管理（认证/放行/过期/撤销），与 os-network 协同 | `nftnl`（nftables netlink 绑定） |
| **guest-api** | 内部 REST 接口，供 os-api 网关聚合 | `axum` 路由 |
| **chain-orchestrator**（可选） | 链上验证**业务编排**：验证因子组合、JWT 签发、策略评估、Portal 流程；钱包连接与验签**调用 `os-wallet`** | 调用 os-wallet SDK |

- **Captive Portal 流量拦截机制**：
  - 访客设备连入访客 VLAN（如 VLAN 100，10.0.100.0/24）
  - os-network 的 DHCP（`dora`）分配 IP，DNS（`hickory-dns`）仅解析内部域名
  - 未认证流量：HTTP/HTTPS 被 nftables DNAT 重定向到 Portal 服务（:8081/:8082），其他流量 DROP
  - 认证后：向 nft set `authenticated_guests` 添加 IP（带 timeout 自动过期），放行到管理员授权的指定端口
  - nft set + timeout 实现内核级自动过期，无需应用层定时扫描
- **Captive Portal 探测兼容**：兼容 iOS（`captive.apple.com`）、Android（`connectivitycheck.gstatic.com`）、Windows（`msftconnecttest.com`）、macOS 四大平台的自动探测，返回 HTTP 302 重定向触发系统弹窗
- **访客身份体系**：

| 身份类型 | 身份有效期 | JWT 有效期 | nft 放行 timeout | 适用场景 |
|----------|-----------|-----------|-----------------|---------|
| 随机 ID（临时访客） | 24 小时 | 2 小时（自动刷新） | 8 小时 | 临时接入、会议访客 |
| 随机 ID（管理员延长） | 1-7 天可配 | 2 小时 | 8 小时 | 短期合作、实习 |
| 公钥身份（长期访客） | 30 天（可续期） | 24 小时（自动刷新） | 24 小时 | 长期可信访客、外部团队 |
| **链上凭证身份（BTC 验证，可选）** | 按策略模板配（默认 30 天可续期） | 24 小时（自动刷新） | 按角色配 | 持币/Web3 社区访客、NFT 持有者、链上信誉准入 |

- **随机 ID 格式**：`GUEST-XXXXXX`（6 位大写字母+数字，排除 O/0/I/1 易混字符，约 10 亿组合空间）
- **公钥升级流程**：管理员授权 → 访客 App 生成 ed25519 密钥对（私钥存设备 Keychain/Keystore）→ 提交公钥+签名 → os-guest 验证 → 签发长期 JWT，不再依赖 IP/MAC 绑定
- **RBAC 扩展**：现有用户体系（admin/user）新增 `guest` 角色；访客权限模型包含 `im_groups`（可加入的 IM 群）、`file_shares`（共享目录+只读/读写）、`bandwidth_limit_kbps`、`daily_time_limit_mins`、`allowed_services`；os-api 中间件统一识别 user JWT / guest JWT
- **IM 群集成**：os-guest 认证成功后调 os-im API 自动将访客加入指定 IM 群（Guest 成员类型）；访客过期/撤销时通知 os-im 移除；Guest 成员只能查看/发送被授权群的消息，不能建群/邀请
- **Portal 页面流程**：落地页（显示 OS 名称/公告）→ 注册页（生成访客 ID + 二维码）→ 成功页（权限摘要 + 引导下载 App）
- **管理员配置面**（web-ui）：访客概览（在线数/统计）、访客列表（ID/类型/状态/操作）、策略模板（默认访客/会议访客/长期访客/链上凭证访客/自定义）、Portal 设置（VLAN/落地页自定义/AP 桥接）、审计日志
- **设计要点**：os-guest 为可选特性，部署时可开启/关闭；nftables 规则变更支持 dry-run + 5 分钟自动回滚；Portal TLS 证书由 os-security 的 CA（`rcgen`）签发；nftnl 如成熟度不足可 fallback 到 nft CLI 编排（同 ZFS CLI 模式）
- **依赖**：`os-network`（VLAN/DHCP/DNS/nftables 基础设施）、`os-security`（JWT 密钥/CA 证书）、`os-im`（Guest 成员/自动入群）、`os-wallet`（链上验证的签名/连接/凭证查询，见 §3.17）、`os-api`（网关聚合）、`os-core`（领域类型）、`os-common`（API schema）

#### 3.18.1 链上凭证身份验证（可选，多链：BTC + EVM）

> **定位**：os-guest 的第四种身份路径（继随机 ID / 公钥身份之后），由 `chain-orchestrator` 子模块做**业务编排**（验证因子组合、JWT 签发、策略评估、Portal 流程）；**钱包连接、签名验证、凭证查询下沉到独立组件 `os-wallet`（§3.17）**。**仅在 `os-wallet` 的 RpcRegistry 报告链 RPC 可用时生效**，不可用时优雅降级回既有身份路径。本能力同时作为 `os-im` 的条件激活 Tool 对外暴露（见 §3.7.2），供 agent/自然语言调用。

- **决策基线（已确认，已升级为多链）**：

| # | 子决策 | 结论 |
|---|--------|------|
| 1 | **验证强度** | **三因子可单选可叠加**：签名挑战 / 余额门槛 / 链上凭证（NFT/Ordinal/ERC-721/1155）；策略模板按链类型启用任一组合。各因子在 BTC/EVM 的实现见 §3.17.2 |
| 2 | **RPC 来源** | **混合优先本地**：优先本地全节点，fallback 远程公共/自有 endpoint；多链各自配 `*_rpc_url` + `*_rpc_fallback_url`（由 os-wallet RpcRegistry 统一管理） |
| 3 | **权限定位** | **三种角色可配**：等同长期访客 / 独立"链上凭证访客"角色（权限单独配，默认 ≤ 长期访客）/ 仅作 2FA 叠加；默认采用独立角色 |
| 4 | **隐私** | **三档可配**：强制告知 / 可选同意 / 不处理；默认强制告知 + 可选同意（Portal 落地页明示"地址↔访客"映射并征求同意） |

> **关键边界（红线）**：链上验证仅证明"持私钥/持币/持凭证"，**不证明真人或可信**（被盗地址、混币池地址均可过验）。因此默认角色权限 ≤ 长期可信访客，高危操作仍需 IM 内确认。

- **条件激活机制**：与全系统优雅降级范式一致（同 IB/RoCE、nftnl，见 §4 风险表）。`os-wallet` 的 RpcRegistry 周期性探活各链 RPC，可用性状态驱动三处联动：
  - **Portal 层**：链上验证入口显隐（按可用链动态显示 BTC/EVM 选项）
  - **策略层**：`policy-engine` 的 `chain_verified` 条件是否可评估
  - **SDK 层**：`os-im` 的 `guest.auth.chain` / `wallet.sign` Tool 是否注册
- **验证流程**（以签名挑战为例）：访客选链+输入地址 → `chain-orchestrator` 生成随机 nonce → 调 `os-wallet` 发起签名请求（WalletConnect/扫码/深链）→ 访客钱包签名 → `os-wallet` 验签 → 可选叠加余额/NFT 门槛（亦调 os-wallet 查询）→ `identity-engine` 签发链上凭证 JWT（claim 含链类型、验证因子、地址哈希、过期）。
- **隐私处理**：默认**地址哈希存储**（非明文），提供清除入口；落地页强制告知映射关系并征求同意；同意记录入审计日志。
- **依赖关系**：`os-wallet`（签名/连接/凭证查询，核心）、`os-security`（JWT 密钥/CA）、`os-im`（SDK Tool 注册/agent 调用）、`os-api`（网关聚合）。

### 3.17 多链钱包与签名中枢（os-wallet，独立组件，新增）

> **定位**：多链钱包连接、消息签名、凭证查询的**基础层中枢**，独立于 os-guest。把"连接钱包 → 签名 → 验签 → 查链上状态"这条链路抽象出来，供 os-guest（链上访客验证）、os-im（agent 调用）、以及未来的支付/授权/签名场景复用。**支持 BTC + EVM 主流链，经 WalletConnect 统一签名入口**。

#### 3.17.1 内部结构

```
os-wallet
  ├─ WalletConnector trait        ← "连接钱包 → 请求签名" 抽象
  │     ├─ WalletConnectV2          (Relay + DeepLink，移动钱包主流)
  │     ├─ InjectedProvider         (浏览器注入钱包，MetaMask 等)
  │     └─ QrCodeScan               (纯二维码扫码签名，离线钱包)
  ├─ ChainAdapter trait           ← 链适配抽象（验签/查余额/查凭证）
  │     ├─ BitcoinAdapter           (BIP-322/Schnorr 验签, rust-bitcoin + secp256k1)
  │     │     ├─ 签名验证: BIP-322 优先, 兼容 Schnorr/ECDSA
  │     │     ├─ 余额门槛: UTXO/余额查询 (bitcoind JSON-RPC / Electrum)
  │     │     └─ 凭证查询: Ordinals/Inscription 持有 (Ord indexer / 自托管)
  │     └─ EvmAdapter               (EIP-191/EIP-712 验签, alloy/ethers-rs)
  │           ├─ 签名验证: EIP-191 personal_sign / EIP-712 typed_data
  │           ├─ 余额门槛: ERC-20 balanceOf (eth_call, 多 L2 支持)
  │           └─ 凭证查询: ERC-721/1155 ownerOf/balanceOf (指定 contract+tokenId)
  └─ RpcRegistry                  ← 条件激活核心（多链版）
        ├─ 注册各链 RPC: 本地全节点优先, fallback 远程 endpoint
        ├─ 健康检查: 周期探活 (BTC: getblockchaininfo; EVM: eth_blockNumber)
        ├─ 可用 → 注册该链的 ChainAdapter, 暴露对应 SDK Tool
        └─ 不可用 → 隐藏该链选项 (不报错, 调用方 fallback 其他身份路径)
```

#### 3.17.2 多链验证因子矩阵（决策表#1 的展开）

| 因子 | BTC 实现 | EVM 实现 |
|------|---------|---------|
| **签名挑战** | BIP-322 / Schnorr | EIP-191 `personal_sign` / EIP-712 `typed_data` |
| **余额门槛** | UTXO 或余额 ≥ N 聪 | ERC-20 `balanceOf` ≥ N |
| **链上凭证** | 持有指定 Ordinal / Inscription | 持有指定 ERC-721/1155（contract+tokenId） |

三因子**可单选可叠加**，由调用方（如 os-guest 的策略模板）按链类型组合配置。

#### 3.17.3 设计要点
- **条件激活**：RpcRegistry 驱动；某条链 RPC 不可用时，该链的 ChainAdapter 不注册，对应 SDK Tool（`wallet.sign.btc`/`wallet.sign.evm`/`wallet.credential.*`）也不暴露——与全系统优雅降级范式一致。
- **WalletConnect v2 集成与 Relay 部署**（见 §9.1#13 决策）：**默认用官方公共 Relay**（零运维、开箱即用；公共 Relay 仅转发加密消息，看不到内容，隐私风险可控）；**企业/隐私场景可选自托管 Relay**（配置项 `wc_relay_url`）。session 管理（建立/恢复/过期）；支持移动钱包深链拉起（MetaMask/Trust/Rainbow/OKX 等）。
- **NFT/Ordinals 凭证数据源**（见 §9.1#12 决策）：**分层查询**——EVM NFT(ERC-721/1155) 走链上 RPC 直查（alloy `eth_call` ownerOf/balanceOf，零外部依赖、最可靠）；**Ordinals/Inscription 走自托管 ord index**（os-wallet 内嵌，作为 SSOT 数据源，复用 osd cgroup 限额）**+ 可配外部 endpoint**（ordiscan/Gamma）作 fallback（Ordinals 无单次链上 RPC 等价物，需索引）。
- **多钱包兼容矩阵**：BTC（Unisat/Sparrow/BlueWallet/OKX）；EVM（MetaMask/Trust Wallet/Rainbow/Coinbase Wallet）；签名标准优先统一抽象（BIP-322 / EIP-191/712）。
- **隐私**：地址哈希存储非明文；查询/验签结果可配缓存 TTL；提供清除入口。
- **纯 Rust 边界**：`rust-bitcoin`/`secp256k1`/`alloy`(或 ethers-rs) 均纯 Rust；自托管全节点（bitcoind/geth）为 C 实现，归入 §0 务实边界（同 KVM/libvirt/Samba）。
- **依赖**：`os-security`（密钥管理）、`os-core`（领域类型）、`os-common`（API schema）。被 `os-guest`、`os-im` 调用。
- **可扩展性**：ChainAdapter trait 为未来 Solana/Cosmos 等链预留插件位（初版不实现，标 🔴 远期）。

### 3.19 密钥/数据统一排除清单（SSOT）

> 以下为全系统**统一的敏感数据排除清单**。`os-provision`（迁移）、`os-iso`（打包）、`os-clone`（克隆 ISO）均引用本清单，不再各自重复定义。

| 类别 | 排除项 | 处理方式 |
|------|--------|---------|
| 系统密码 | `/etc/shadow` 及所有用户密码哈希 | 目标端/首启**强制重设密码** |
| TLS 私钥 | 所有 `*.key`、Let's Encrypt 私钥 | 目标端**重生成** + 经 os-security 重新签发 |
| SSH 私钥 | `/etc/ssh/ssh_host_*`、用户 `~/.ssh/id_*` | 目标端**重生成** host key；用户 key 提示重新上传 |
| SMB/应用凭证 | Samba passdb / 各服务凭据 / Access Key·Secret | 目标端**重新配置** |
| 集群密钥 | openraft 集群密钥、mTLS CA 私钥、JWT 签名密钥 | 目标端**重新引导生成**（首启建新集群或重新 join） |
| 用户数据 | 所有 ZFS 数据集内容、数据库内容 | ISO/克隆**完全不含**；迁移按选定数据集走 ZFS send/recv（用户显式选择） |
| 链上凭证 | os-wallet 的地址映射、签名 session | **不迁移**；目标端重新建立（地址哈希可由用户显式导出/导入） |

**通用原则**：任何分发/迁移产物（ISO/克隆/迁移包）只含"系统本体 + 配置骨架"，**绝不**含上述敏感项；传输包签名 + 断点续传 + ZFS send/recv 自带校验保证完整性。

---

## 4. 关键技术风险与权衡

| 风险 | 缓解 |
|------|------|
| `smb-server` / `nfsserve` 较新 | 纯 Rust 迭代策略：先单节点跑通，灰度启用；保留兼容测试矩阵 |
| HA 一致性复杂度（脑裂/数据一致性） | `openraft` 强一致；复制存储用 ZFS 快照点对齐，避免半同步丢数据 |
| KVM/libvirt 为 C，非纯 Rust | 仅管理面 Rust 化；隔离故障域，libvirt 调用加超时与回滚 |
| VM 实时迁移需共享存储 | 先 active-passive（磁盘复制+重启），实时迁移/共享存储后置 |
| 镜像生态（纯 Rust 拉取） | `oci-distribution` + `youki`；必要时桥接 `skopeo` 作过渡 |
| ZFS 内存（ARC） | 限 ARC；按 §6 硬件门槛保证 ECC/容量 |
| 网络组件需 root/内核权限（rtnetlink/nftables） | 沙箱化、能力最小化（CAP_NET_ADMIN），独立集成测试防误配断网 |
| 分发/迁移的"密钥排除"与完整性 | 显式密钥清单白名单排除；ZFS send/recv 自带校验；传输包签名+断点续传；目标强制重设密码 |
| 节点发现的伪造 / 误判 HA 资格 | beacon 签名防 spoof；资格检测用硬指标（版本/架构/带宽/法定数）；HA 创建需显式确认 |
| 多端（手机/桌面）一致性与平台限制 | 共用 `os-discover` 发现协议与 `os-api` 契约；iOS 局域网/后台受限，移动端以管理为主、桌面端以挂载为主 |
| 第三方 .deb 破坏系统/依赖冲突 | 仅装单节点不复制；记录安装清单与依赖树便于回滚；沙箱/隔离；"未知来源"分组明确风险提示 |
| IB/RoCE 硬件依赖与驱动复杂 | 能力探测优雅降级（无 RDMA 则走 TCP）；FFI 绑 rdma-core 隔离 panic；DCB/PFC 配置需网卡支持，提供兼容性检测 |
| DPU 厂商绑定与带外可达性 | 抽象 `DpuBackend` trait 适配多厂商；带外走标准 Redfish/IPMI 兼容；主机宕机时带外仍可达，作为 HA 兜底上报通道 |
| 对象存储一致性/分布式扩展（类 MinIO） | RustFS 原生支持分布式；HA 下经 os-meta 协调节点；桶策略与 Access Key 统一纳管；版本控制防误删 |
| AI agent 误操作/越权（IM 中枢） | agent 受 RBAC 限制；高危操作 IM 内确认；全量审计；SDK 明确 Tool 权限边界；本地模型离线可用 |
| ISO 含敏感信息/数据泄露 | 构建期显式排除密码/密钥/数据（白名单）；克隆 ISO 仅含配置不含数据；签名+校验和防篡改；首启强制重设密码 |
| 同级 peer 无上限的同步风暴/冲突 | 🔴 降级为 1 主 N 从只读副本（非多主 mirror）；ZFS 原生不支持多主；多主列为远期研究（CRDTs/Syncthing 思路） |
| 功能服务层组件膨胀/资源争抢 | 各组件独立进程可单独启停；资源配额(CPU/内存上限)；os-media 转码等重任务可卸载到 DPU 或限速 |
| 密钥/凭证分散管理风险 | os-security 统一 2FA/OAuth2；os-devtools 的 secrets 加密 KVS 独立于系统密钥；定期轮换 |
| 多端多语翻译不同步/漏译 | os-i18n 统一翻译资源为单一来源（SSOT）；CI 校验键完整性；新语种仅加资源文件；前端用 vue-i18n 对接同一套键 |
| **元数据 HA 复制（SQLite 单节点）** | ✅ 已决（§9.1#7）：openraft 状态机内嵌 SQLite 快照（强一致+快照恢复）；初版可先单节点 SQLite，HA 时升级为内嵌复制 |
| **NTP 时钟漂移致共识/证书异常** | ✅ 已决（§9.1#8）：osd 编排 chrony 保证时钟一致；NTP 为 openraft 前置依赖，P0 最早启动；需 CAP_SYS_TIME |
| **AI/LLM 本地推理硬件需求** | 本地推理列为可选（需 GPU/NPU）；默认走云端 API；本地仅轻量 NLU 或量化小模型（candle + Phi-3-mini） |
| **访客 nftables 误配断网** | 规则变更加 dry-run + 5 分钟自动回滚；nft set 操作隔离在 guest 链内，不影响主网络；集成测试覆盖 |
| **Captive Portal 探测不兼容** | 覆盖 iOS/Android/Windows/macOS 四平台探测 URL；nftnl 成熟度不足时 fallback 到 nft CLI 编排（同 ZFS CLI 模式） |
| **访客 JWT 密钥泄露** | 密钥统一由 os-security 管理 + 定期轮换 + 短期 JWT(2h)；公钥升级后不依赖 IP/MAC 绑定 |
| **链上验证 ≠ 真人/可信**（被盗地址、混币池地址可过验） | 默认采用独立"链上凭证访客"角色，权限 ≤ 长期访客；高危操作仍需 IM 内确认；可选叠加签名挑战提升门槛 |
| **远程 RPC 挂了/限流致验证不可用** | os-wallet 的 RpcRegistry 健康检查 + 自动 fallback 本地全节点；不可用时隐藏该链选项（不报错、不卡死访客），降级回随机 ID/ed25519 |
| **链下建立"地址↔访客"映射的隐私/合规风险** | 默认强制告知 + 可选同意（Portal 明示）；地址哈希存储非明文；提供清除入口；同意记录入审计 |
| **签名标准碎片化（BIP-322/Schnorr/旧 ECDSA）** | 优先 BIP-322 统一抽象；多钱包测试矩阵覆盖（Unisat/Sparrow/BlueWallet/OKX） |
| **自托管全节点资源占用**（若本地 RPC） | pruned 模式 + 复用 osd cgroup 限额；或仅配远程 RPC 不自托管 |
| **EVM 链重组（reorg）致凭证/NFT 持有判定翻转** | 凭证查询等待确认数（confirmations）阈值；重组后重评；短 JWT 有效期限制影响窗口 |
| **WalletConnect Relay 可用性依赖**（公共 Relay 挂了签名链路断） | 支持自托管 Relay；Relay 不可用时 fallback 扫码/深链本地签名；session 可恢复 |
| **多链 RPC 数据源信任差异**（公共 RPC 可篡改/限流） | 优先本地全节点；公共 RPC 仅作 fallback；高危凭证校验要求本地节点或可信私有 endpoint |
| **多 agent 任务委派死循环/权限放大**（agent A 委派 B 委派 A；越权放大） | 任务图必须无环，中枢检测循环即拒；agent 间不得越权（必须经执行组件 API）；委派权限 ≤ 委托方权限（不可放大） |
| **开发期 agent 破坏一致性/幻觉虚构 API** | SSOT + 契约先行 + 会签 + CI 编译验证；禁止依赖未发布/虚构 crate；ReviewAgent 静态校验（见 §13） |
| **Solana/Cosmos 等未来链接入复杂度** | ChainAdapter trait 插件化预留；初版只 BTC+EVM，其他链标 🔴 远期，不阻塞初版 |

---

## 5. 分阶段路线图（仅规划）

**P0 工具链与单节点骨架**
- Ubuntu 26.04 最小化 + ZFS + KVM；Rust workspace 分层 crate；CI（clippy/fmt/test）；Axum 骨架 + SQLite。
- `osd` 守护进程骨架（组件进程管理/cgroup v2 资源配额）。

**P1 单节点 MVP（纯 Rust 协议 + 存储 + UI）**
- `os-storage` 建池/挂载；`dav-server`+`libunftp`+`russh` 暴露；Vue macOS 风 UI 看池/容量；SMB/NFS 接入 `smb-server`/`nfsserve`（灰度）。
- `os-network` 基础接口/VLAN/桥配置（rtnetlink），UI 可查看与配置网卡。

**P2 HA 集群控制面（openraft）**
- `os-meta` 多节点共识、选主、分布式 KV；ZFS 复制 + 故障转移 + VIP；管理面支持多节点视图。

**P3 计算层：容器 + 虚拟机 + 第三方软件**
- `youki`+`oci-distribution` 应用市场；`os-container-net` 容器网络(CNI/veth/bridge/NAT/卷挂载)；`os-vm` 管理 KVM 生命周期；VM 随节点故障在副本节点重启。
- `os-pkg` 第三方 .deb 安装/卸载/升级，`.desktop` 图标统一归入"未知来源"分组。

**P3.5 访客接入 + 多链钱包（os-guest + os-wallet，可选，M5-M6）**
- `os-wallet` 多链钱包与签名中枢（BTC+EVM，WalletConnect v2，ChainAdapter/RpcRegistry 条件激活）。
- `os-guest` Captive Portal 引擎 + 访客身份（随机 ID + JWT + 公钥升级 + 链上凭证身份）+ 模块级 RBAC + nftables 规则编排；链上验证的签名/连接/凭证查询调 `os-wallet`（见 §3.18.1）。
- 扩展 os-network（访客 VLAN）、os-security（guest/wallet JWT 密钥）、os-im（Guest 成员 + 自动入群）、os-api（guest 中间件）、os-files（权限校验）、web-ui（访客/钱包管理界面）。

**P4 高级与打磨**
- 实时迁移/共享存储（Ceph）；S3 网关（RustFS）；`os-network` 完整网络服务（DHCP/PXE/DNS/防火墙）+ IB/RoCE(RDMA) 高速互联 + **DPU 带内卸载/带外管控**；快照策略/远程复制/告警；性能与压测；安全审计闭环。

**P5 系统分发 / 节点发现 / 联邦（os-provision）**
- 网络内节点发现（mDNS/组播 beacon）+ 凭证双向认证互联；HA 资格检测（版本/架构/带宽/法定数）。
- 符合 HA → 按需创建 HA 集群（join `os-meta`，VIP + ZFS 复制/故障转移）；不符合 → 作为同级节点，支持数据集同步（ZFS send/recv）。
- PXE 自举 + 阶段化迁移（类手机换机）：阶段1 系统初始化；阶段2 传输文件（除密码/密钥，统一排除清单见 §3.19）；断点续传 + 进度；目标重设密码。

**P6 客户端：手机 App + 桌面软件**
- `os-discover` 抽离为独立发现/联邦组件；`os-mobile`（发现/手动IP/状态/配对/触发迁移同步）、`os-desktop`（Windows 优先，发现/手动IP/一键挂载 SMB·WebDAV），均复用 Vue macOS 风 UI 与发现/API 契约。

**P7 核心 IM 与多 Agent 协作中枢（os-im）**
- 对话即操作（建 VM/起容器/同步任务/存储操作/开通访客）；承载 agent 运行；提供 Rust SDK（Tool/Function Calling）让 agent 调用各执行组件。
- **多 agent 协作架构**（见 §3.7.2）：中枢-领域拓扑、任务委派、能力发现、上下文黑板、确认与投票、结果聚合；事件总线 + 全链路审计。
- 系统事件经 IM 推送；RBAC + 高危确认 + 审计；LLM 支持云端与本地（本地可选，见 §11.2.5）。

**P8 封装/打包（os-iso）+ 系统更新（os-update）**
- 把全部组件 + Ubuntu 26.04 底座打包成可安装 ISO（排除密码与数据，统一清单见 §3.19）；支持标准安装 ISO 与克隆 ISO；Rust 安装器 + CI 集成构建。
- `os-update` OTA 在线升级：A/B 分区或 ostree 原子更新 + 自动回滚；组件级滚动升级（HA 逐节点）；CVE 监听与安全更新推送。

**P9 功能服务层（七组件）**
- `os-backup` 快照策略/灾备演练；`os-monitor` 可观测性+告警；`os-power` UPS/硬件监控；`os-files` 文件管理+全文搜索+分享；`os-security` 2FA/OAuth2/证书/VPN；`os-media` 相册/流媒体；`os-devtools` 日志/CI/Git/密钥。七组件独立发布，经 os-api 暴露、可被 os-im agent 调用。

**P10 远期研究（自研/前沿）**
- 自研 SMB Server / NFSv4（纯 Rust）；多主同步（CRDTs/Syncthing 思路，见 §11.2.3）；NVMe-oF target；Solana/Cosmos 等新链接入 os-wallet（ChainAdapter 插件）；本地大模型推理（需 GPU/NPU）；Ceph 共享存储。

---

## 6. 硬件规格（Ubuntu 26.04 推荐为最低门槛）

| 档位 | CPU | 内存 | 存储 | 网络 | 说明 |
|------|-----|------|------|------|------|
| **最低门槛**（=Ubuntu 26.04 推荐基线） | ≥4 核 | ≥8 GB（ECC 优先） | 系统盘 ≥64 GB SSD + ≥2× 数据盘 | 1× GbE | 满足"推荐配置为最低"；仅能跑基础 OS |
| **推荐（单节点带 VM/容器）** | 8–16 核 | 32 GB ECC | 2× SSD（系统+缓存）+ 多盘位 HDD（ZFS） | 2× GbE / 10 GbE | 同时跑 VM + 容器 |
| **集群（HA，每节点）** | 同上 | 同上 | 同上 + 复制/共享存储 | **10 GbE 互联；可选 IB/RoCE(RDMA) 加速** | ≥2 节点；迁移/复制需要高带宽，IB/RoCE 显著降延迟 |

> 注：Ubuntu 26.04 官方**最低**为 2 GHz 双核 / 6 GB RAM / 25 GB 磁盘[4]；本表"最低门槛"已按你的决策抬到**推荐基线**，实际功能型部署请用"推荐"档。

---

## 7. 前端：Vue 3 + macOS 风格设计系统

- **技术**：Vue 3 + Vite + TypeScript；状态 Pinia；请求 Axios；实时 WebSocket；国际化 `vue-i18n`（对接 `os-i18n` 翻译资源，简中/繁中/英文）。
- **macOS 风格要点**（设计 token）：
  - 左侧**半透明毛玻璃侧边栏**（`backdrop-blur`）+ 右侧内容区；
  - 窗口化卡片、**圆角（rounded-2xl）**、柔和阴影、留白充足；
  - **红黄绿"交通灯"**窗口控件作为装饰/交互元素；
  - 字体栈 `-apple-system, "SF Pro", "PingFang SC"` 优先；
  - 图标用 **SF Symbols 风格**线性图标集（如 `lucide`/`iconoir`）。
- **参考实现**：`MacOS-Web-UI`（Vue3+ElementUI）[5]、`MacWeb` 模板可作视觉基线；建议自建设计系统（基于 Element Plus / Naive UI 覆写主题），保证可控与长期迭代。

---

## 8. 工程与质量保障
- **分层 crate workspace**（与 §3.0 SSOT 总表一致）：`os-core` / `os-i18n`（国际化） / `os-common` / **`osd`（系统编排守护进程）** / `os-network`（接口/VLAN/桥/NAT + DHCP/PXE/DNS + IB/RoCE + DPU） / `os-storage` / `os-protocols`（smb,nfs,webdav,ftp,sftp,object） / `os-compute`（vm,app,**container-net**,pkg） / `os-meta`（openraft） / `os-discover`（发现/认证/联邦） / `os-provision`（PXE 自举 + 阶段化迁移） / `os-iso`（系统封装/ISO 打包） / **`os-update`（OTA 在线升级/原子回滚）** / `os-services`（backup,monitor,media,security,files,devtools,power） / **`os-guest`（访客接入管理：Captive Portal + 身份引擎 + 策略引擎）** / **`os-wallet`（多链钱包与签名中枢：BTC+EVM+WalletConnect）** / `os-api` / `os-im`（核心 IM + 多 agent 协作中枢 + SDK） / `os-cli` / `os-mobile`（手机客户端） / `os-desktop`（桌面客户端，Windows 优先）。共 **21 个顶层 crate**（聚合 crate 内部含子模块，见 §3.0）。
- **测试**：单元测领域逻辑；集成测 ZFS 编排（loop 设备临时池）；协议用真实客户端冒烟；HA 用多节点仿真测故障转移。
- **配置即代码**：系统服务配置全部由 Rust 生成并版本化，避免手改 `/etc`。
- **可观测**：`tracing` 全链路日志 + 指标端点，WebSocket 推前端。

---

## 9. 仍需细化的子决策（可选）
1. HA 存储模式：先 **active-passive（ZFS 复制）** 还是直接上 **共享存储（Ceph）**？建议前者起步。
2. 用户体系：自建用户库 vs 接入现有目录（LDAP/AD）？
3. ~~集群规模上限~~ **已明确**：HA 集群走 openraft 法定数（3/5/7）；**同级 peer 无上限**，不进共识，仅 1 主 N 从只读副本（ZFS send/recv 单向复制）。
4. 容器镜像来源：公共 registry 为主，还是需要私有 registry？
5. ~~ZFS mirror 同步冲突策略~~ **已调整**：降级为单向主从只读副本，无多主冲突；多主列为远期研究（见 §11.2.3）。
6. ~~访客链上凭证身份验证~~ **已决并升级为多链**（详见 §3.17 os-wallet + §3.18.1）：(a) 验证强度三因子可单选可叠加——签名挑战/余额门槛/链上凭证，覆盖 **BTC（BIP-322/Ordinals）+ EVM（EIP-191·712/ERC-20·721·1155）**；(b) RPC 来源混合优先本地，fallback 远程（RpcRegistry 统一管理）；(c) 权限定位三种角色可配，默认独立"链上凭证访客"角色（权限 ≤ 长期访客）；(d) 隐私三档可配，默认强制告知 + 可选同意。签名/连接/凭证查询下沉到独立组件 **os-wallet**（WalletConnect v2）；**仅在链 RPC 可用时生效**，不可用降级回随机 ID/ed25519。

### 9.1 待决项推荐方案（已逐项评估）

> 以下 7 项为首轮分析合并的待决项，已逐项给出**推荐方案 + 理由 + 备选 + 影响面**。状态 ✅=已有推荐方案（待评审确认后落地）。

| # | 待决项 | 推荐方案 | 理由 | 备选 | 影响面 |
|---|--------|---------|------|------|--------|
| 7 | **元数据 HA 复制**（SQLite 一致性） | **openraft 状态机内嵌 SQLite 快照**（类 Databend/litestream）| 与 §3.5 openraft 同源、不引入新依赖；强一致 + 快照恢复，契合 HA 语义 | rqlite（独立 Raft 集群，与 os-meta 重复）；litestream（WAL 异步流式，有丢数据窗口）| os-meta 实现；初版可先单节点 SQLite，HA 时升级为内嵌复制 |
| 8 | **NTP 时间同步**归属 | **归 `osd`，编排 chrony**（非纯 Rust，纳入务实边界）| NTP 是系统级守护（同 systemd），osd 是 PID1 后编排器；NTP 是共识的*前置依赖*（时钟漂移致 Raft 选主异常），须最早启动 | 纯 Rust NTP server（生态不如 chrony 成熟）| osd 增 NTP 子模块；P0 启动；需 root/CAP_SYS_TIME |
| 9 | **事件总线**归属 | **节点内：`os-core` 提供 EventBus trait + tokio broadcast 实现；跨节点：走 os-meta 的 openraft log** | 事件总线被所有组件依赖，属基础层（同 os-common）；trait 在 os-core 定义、各组件注入使用，避免放 osd 把业务通信耦合进进程管理器 | 放 osd（耦合进程管理）；独立 os-bus 组件（过度拆分）| os-core 增 EventBus trait；§3.7.2 多 agent 协作依赖之；呼应 §10.2#8 |
| 10 | **API 网关**是否独立 | **不独立，作为 os-api 内嵌层**（Axum 路由聚合 + tower 中间件：TLS 终止/限流/认证）| 20+ 组件经独立网关是"为分层而分层"；os-api 本就是 Axum+tower，统一入口天然由它承担。需负载均衡时 os-api 多副本 + os-meta VIP 调度即可 | 独立 os-gateway 组件（仅超大规模才需要）| 无新组件；os-api 职责明确化（网关+聚合）；§10.2#9 关闭 |
| 11 | **iSCSI / NVMe-oF target** 归属 | **归 `os-storage`（块存储 export 子模块）**，编排 tgt/LIO(iSCSI) + nvmet(NVMe-oF) | 二者本质是"把存储以块形式 export 给外部"，是 storage 的 export 能力（同 zvol）；文件协议归 §3.3 协议层，块协议归存储层，语义清晰 | 放协议层 os-protocols（语义不通：协议层是文件/对象协议）| os-storage 增 block-export 子模块；§3.3 iSCSI 描述迁移至此；§10.2#15 关闭 |
| 12 | **NFT/Ordinals 凭证数据源**（os-wallet） | **分层：EVM NFT 走链上 RPC 直查（alloy eth_call，零外部依赖）；Ordinals 走自托管 ord index（os-wallet 内嵌）+ 可配外部 endpoint（ordiscan/Gamma）作 fallback** | EVM ownerOf/balanceOf 是标准 RPC，直查最可靠；Ordinals 无单次链上 RPC 等价物（需索引），自托管是 SSOT 路径 | 全外部 API（依赖第三方可用性/限流/隐私）；全自托管 EVM 节点（成本高，EVM 直查已够）| os-wallet ChainAdapter 实现差异；需 ord index 运维（复用 osd cgroup） |
| 13 | **WalletConnect Relay 部署形态** | **默认公共 Relay（零运维，开箱即用）；企业/隐私场景可选自托管**（配置项 `wc_relay_url`）| WC v2 公共 Relay 只转发加密消息（看不到内容），隐私风险可控；自托管增加运维（高可用/TLS/跨地域延迟）。默认官方、可切自托管，符合全系统"可配/优雅降级"范式 | 强制自托管（默认场景运维负担过重）；强制公共（隐私敏感场景不可接受）| os-wallet 配置项；UI 暴露切换；自托管文档化 |

> **落地建议**：以上 7 项评审确认后，#8/#9/#11 涉及组件归属调整（已同步 §3.0 SSOT 总表与 §3.2/§3.6/§3.13/§3.17），#7/#10/#12/#13 为实现层细节（已在对应组件章节补段）。工期影响已计入 §11.4.3：os-core +EventBus(+5)、osd +NTP(+4)、os-storage +block-export(+8)、os-wallet +ord index(+6)，合计 **+23 人天**（合计 1597→1620 人天，团队规模结论不变：7–8 人）。

---

## 10. 遗漏组件 / 能力清单（可行性调研补充）

> 经外部调研发现的文档中**未涉及或仅一笔带过**但对完整 OS 产品至关重要的组件。

### 10.1 高优先级遗漏（影响核心功能）

| # | 遗漏组件/能力 | 说明 | 建议 | 状态 |
|---|---------------|------|------|------|
| 1 | **容器网络层** | youki 是低层运行时（类比 runc），不含网络。需 CNI 插件或自建 veth/bridge/NAT 方案。否则容器无法联网/互访。 | 新增 `os-container-net` 子组件，基于 veth + bridge + nftables NAT | 🟢 已纳入（§3.4/#7） |
| 2 | **容器镜像管理 / 私有 Registry** | 文档仅提"拉取"，未涉及本地镜像缓存、私有仓库、镜像 GC、离线导入导出。 | 基于 `container-registry` crate 或 `oci-distribution` 自建轻量 registry | 🟡 v2+ |
| 3 | **Time Machine 备份支持** | macOS 用户核心需求。走 SMB + vfs_fruit 扩展（Samba 编排则天然支持）。 | 纳入 SMB 方案（Samba vfs_fruit） | 🟢 已纳入（§3.3） |
| 4 | **iSCSI Target** | OS 提供块存储给 ESXi/Windows/VM 的标准协议。无 Rust target 实现。 | 短期编排 `tgt`/`LIO`（C）；长期关注 Rust 实现 | ✅ 已决：归 `os-storage` block-export 子模块（§3.2 / §9.1#11，从协议层迁入） |
| 5 | **系统自更新 / OTA 机制** | 有 os-iso（初始安装）和 os-provision（PXE 部署），但**缺少已部署节点的在线升级/回滚机制**。 | 新增 `os-update`：A/B 分区或 ostree 式原子更新 + 回滚 | 🟢 已纳入（§3.12/#12） |
| 6 | **NTP 时间同步** | 集群共识（Raft）、ZFS 快照时间戳、证书验证均依赖精确时间。 | 集成 `chrony` 编排或纯 Rust NTP | ✅ 已决（§9.1#8）：归 osd，编排 chrony |
| 7 | **元数据 HA 复制方案** | SQLite 为单文件数据库，HA 多节点下如何保证元数据一致？ | openraft log + 本地 SQLite 快照（类 Databend 模式）或 litestream | ✅ 已决（§9.1#7）：openraft 内嵌 SQLite 快照 |

### 10.2 中优先级遗漏（影响产品完整度）

| # | 遗漏组件/能力 | 说明 | 建议 | 状态 |
|---|---------------|------|------|------|
| 8 | **服务间通信 / 事件总线实现** | 架构图提到"事件总线"但未指定技术。20+ 组件间需统一 pub/sub。 | tokio broadcast 节点内总线；跨节点走 openraft log | ✅ 已决（§9.1#9）：节点内归 os-core(EventBus trait)，跨节点走 os-meta openraft log |
| 9 | **API 网关 / 反向代理** | 多组件暴露多端口，需统一入口、TLS 终止、限流、认证中间件。 | Axum 层统一路由 + tower 中间件 | ✅ 已决（§9.1#10）：os-api 内嵌网关，不独立成层 |
| 10 | **日志轮转 / 磁盘空间保护** | `tracing` 输出日志但无轮转策略；OS 磁盘满会破坏 ZFS。 | `tracing-appender` 轮转 + 日志配额 + 告警 | 🟡 v2+（os-monitor 范畴） |
| 11 | **ZFS Scrub / 数据巡检调度** | ZFS 需定期 scrub 发现静默数据损坏（bit rot）。 | os-backup 中加入 scrub 调度 + 结果告警 | 🟢 已纳入（os-backup，§3.16） |
| 12 | **Web Terminal / 远程 Shell** | 管理面提供浏览器内 SSH 终端（类 TrueOS/Proxmox）。 | xterm.js + WebSocket + russh 后端 | 🟡 v2+ |
| 13 | **LDAP / AD 目录集成** | §9 列为"待细化"，但企业用户刚需。影响全部认证。 | 纳入 P2/P3；`ldap3` crate 可用 | 🟡 v2+（§9#2 待决） |
| 14 | **配额管理 UI 与执行** | 文档提到 ZFS 配额但未涉及用户/组级配额 UI、超限通知。 | os-files 或 os-storage 中细化 | 🟢 已纳入（os-storage/os-files） |
| 15 | **NVMe-oF Target（主机侧）** | DPU 章节提到 NVMe-oF target 在 DPU，主机侧也需要。 | 编排 `nvmet`（内核 target）或 `spdk` | ✅ 已决（§9.1#11）：归 os-storage block-export 子模块（同 iSCSI），主机侧编排 nvmet |
| 16 | **存储分层 / 缓存策略** | ZFS 支持 special vdev（SSD 加速 metadata/小文件），文档未涉及分层配置 UI。 | os-storage 增加 special vdev / L2ARC 管理 | 🟡 v2+ |
| 17 | **硬件兼容性列表（HCL）/ 驱动检测** | 网卡/RAID 卡/HBA/DPU 兼容性检测与告警。 | os-power 或独立 `os-hcl` 检测模块 | 🟡 v2+ |

### 10.3 低优先级遗漏（锦上添花 / 远期）

| # | 遗漏组件/能力 | 说明 | 状态 |
|---|---------------|------|------|
| 18 | **邮件 / 通知网关** | 告警推送除 IM/Webhook 外，SMTP 邮件（`lettre` crate） | 🟡 v2+ |
| 19 | **SNMP 支持** | 企业监控集成（`snmp-rs`），供外部 Nagios/Zabbix 拉取 | 🔴 远期 |
| 20 | **rsync / 远程同步服务** | 与第三方 OS/服务器同步 | 🟡 v2+ |
| 21 | **防病毒 / 恶意软件扫描** | 企业 OS 常见需求（ClamAV 编排） | 🔴 远期 |
| 22 | **目录服务 / 回收站** | 文件误删恢复（ZFS 快照 + 用户态回收站目录） | 🟡 v2+ |
| 23 | **多租户 / 组织隔离** | 若面向中小企业，需组织/项目级资源隔离 | 🔴 远期 |
| 24 | **合规 / 数据驻留策略** | GDPR 等合规要求的数据保留/删除策略引擎 | 🔴 远期 |
| 25 | **访客网络接入（Captive Portal）** | 局域网访客通过强制门户注册身份、获取受限网络访问与 IM 群接入。涉及网络拦截(VLAN/nftables)、身份签发(JWT/公钥/链上凭证)、RBAC 策略、IM 集成 | 🟢 已纳入：`os-guest`（§3.18）+ `os-wallet`（§3.17，多链签名/凭证） |

---

## 11. 可行性分析与评估

> 以下评估基于 2026-07 对 crates.io / docs.rs / GitHub 等公开源的独立调研。

### 11.1 总体可行性评估

| 维度 | 评级 | 说明 |
|------|------|------|
| 整体架构设计 | ✅ 合理 | 分层清晰、组件解耦、职责明确，参考 TrueOS SCALE 思路正确 |
| 纯 Rust 应用栈 | ⚠️ 部分高风险 | 协议层 SMB Server 无成熟 Rust 实现，为最大瓶颈 |
| HA / 集群 | ✅ 可行 | openraft 已在 Databend 生产验证，ZFS 复制方案成熟 |
| 容器 / 虚拟机 | ✅ 可行 | youki + KVM/libvirt 路径清晰，但容器编排层需补全 |
| 存储层 | ✅ 可行 | ZFS CLI 编排方案务实，libzetta-rs 可参考 |
| 网络层 | ⚠️ 中高复杂度 | rtnetlink/nftnl 生产可用，但 RDMA/DPU 为前沿领域 |
| 前端 / 客户端 | ✅ 可行 | Vue 3 + Tauri + Capacitor 生态成熟 |
| 功能服务层 + 多 agent 协作 | ⚠️ 工作量巨大 | 七个独立组件 + IM/多 agent 中枢 + os-wallet，人力需求极高 |
| 工程总量 | 🔴 极大 | 保守估计 24–36 人·年（不含打磨），需分阶段长期迭代；可借 §13 多 agent 协作开发放大人力效率 |

**总结**：架构设计合理、技术方向正确，但存在 **1 个致命风险**（SMB Server）、**3 个高风险点**（NFSv4 缺失、容器编排层空白、同级 peer 多主同步）以及 **多处组件遗漏**（已补 os-wallet/osd/os-update/容器网络等，见 §10 状态列）。建议按 §13 多 agent 协作方法论推进，分阶段长期迭代。

### 11.2 关键技术风险深度评估

#### 11.2.1 🔴 致命风险：SMB Server 无纯 Rust 实现
- **现状**：搜索 crates.io、docs.rs、GitHub，截至 2026-07 **不存在**可用的纯 Rust SMB2/3 Server 库。现有 crate（`smb`、`smb2-rs`、`pavao`）均为 Client 库。
- **影响**：SMB 是 Windows/macOS/Linux 桌面文件共享的**事实标准**，OS 无 SMB ≈ 不可用。
- **建议**：将 SMB 纳入"务实定义"边界（同 KVM/libvirt），P1 编排 Samba（smbd）；Rust 负责配置生成、用户映射、共享管理、会话监控；长期关注 Rust SMB Server 生态或自研作为远期目标。

#### 11.2.2 🔴 高风险：NFSv4 无 Rust 实现
- **现状**：`nfsserve` 仅 v3；NFSv4（含 4.1/4.2、pNFS）为有状态 compound 协议，复杂度极高，全球仅 Linux 内核 + Ganesha（C）有完整实现。
- **建议**：P1–P3 用 NFSv3 覆盖基础场景；企业级 NFSv4 短期编排 `nfs-ganesha`；自研列极远期目标。

#### 11.2.3 🔴 高风险：同级 Peer 无上限多主同步
- **问题**：ZFS 原生 `send/recv` 为**单向点对点**复制，不支持多主写入。"无上限同级 peer + mirror 拓扑 + 冲突按时间戳解决"在工程上极为复杂。
- **建议**（已采纳）：降级为**单向主从复制**（1 primary → N 只读副本），明确无多主写入；多主同步作为远期研究课题（参考 CRDTs/Syncthing 协议思路）。

#### 11.2.4 🟡 中等风险：容器编排层空白
- **问题**：youki ≈ runc（低层运行时），缺少容器网络、镜像管理、生命周期编排、存储卷挂载、日志收集。
- **建议**（已采纳）：由 `os-compute` 聚合 crate 承担编排层（含 `os-container-net` 子模块补全网络，见 §3.0/#7 与 §3.4），基于 youki + CNI + ZFS 卷；不再单设 `os-container-engine`（避免与既有命名重复）。

#### 11.2.5 🟡 中等风险：AI/LLM 本地推理硬件需求
- **问题**：本地 LLM（7B–13B）需 8–24 GB VRAM 或 32 GB+ RAM；CPU 推理极慢（~5–15 token/s）；与 OS 存储/VM 争抢资源。
- **建议**：本地推理列为可选功能（需 GPU/NPU）；默认走云端 API；本地仅轻量 NLU（意图识别）或 `candle` + 量化小模型（Phi-3-mini）。

### 11.3 人力与工期估算（粗略）

| 模块 | 估算人·年 | 说明 |
|------|-----------|------|
| 存储层 + ZFS 编排 | 1.5–2 | 含 HA 复制、加密、配额 |
| 协议层（WebDAV/FTP/SFTP/NFSv3） | 1–1.5 | 基于成熟 crate 集成 |
| SMB（Samba 编排） | 0.5–1 | 配置/用户/共享管理 |
| 集群控制面（openraft） | 2–3 | 含 VIP、故障转移、元数据复制 |
| 容器编排（youki + 网络 + 镜像） | 2–3 | 含应用市场 UI |
| 虚拟机管理 | 1–1.5 | libvirt/QEMU 编排 |
| 网络层（含 DHCP/PXE/DNS/防火墙） | 2–3 | RDMA/DPU 额外 +1–2 |
| 管理面 API + Web UI | 2–3 | Vue 3 macOS 风格 |
| 客户端（桌面 + 移动） | 1.5–2 | Tauri + Capacitor |
| 功能服务层（七组件） | 4–6 | 每组件 0.5–1 人·年 |
| IM + 多 agent 协作中枢 | 2–2.5 | 含 SDK、RBAC、LLM 集成、中枢-领域拓扑、任务委派（较原 +0.5） |
| 多链钱包（os-wallet，BTC+EVM） | 0.3–0.5 | WalletConnect + ChainAdapter + RpcRegistry（新增） |
| 系统分发/发现/ISO | 1.5–2 | PXE + mDNS + ISO 构建 |
| 测试 / CI / 文档 | 2–3 | 集成测试、多节点仿真 |
| **合计** | **~24.3–36 人·年** | 不含自研 SMB/NFSv4/多主同步；含 os-wallet + 多 agent 协作 |

> 若 5 人全职团队，约需 **4.5–6.5 年**完成全部功能（不含远期自研协议）。

### 11.4 一年初版倒推估算（含 osd / os-container-net / os-update）

> 目标：**1 年（约 240 工作日）内完成可用初版**。以下按"初版范围裁剪 + 逐组件人天拆解 + 并行排期"三步倒推。

#### 11.4.1 初版范围裁剪（1 年内可达）

初版 = **单节点可用 OS + 基础 HA + 容器/VM + Web 管理 + OTA**，砍掉远期/高级项：

| 纳入初版 | 推迟到 v2+ |
|----------|-----------|
| ZFS 建池/数据集/快照/配额 | ZFS 同级 peer 同步、Ceph |
| Samba 编排（SMB）+ NFSv3 + WebDAV + SFTP | NFSv4、自研 SMB、FTP |
| youki 容器 + **os-container-net** 基础网络 | 应用市场 UI、镜像 GC、私有 Registry |
| KVM 虚拟机基础生命周期 | 实时迁移、故障重启 |
| **osd** 守护进程（进程管理+cgroup） | 高级健康策略、HA osd 协调 |
| openraft 单集群 3 节点 HA + VIP + ZFS 主从 | 同级 peer 无上限、多主 |
| Axum REST + Vue macOS 风 UI（核心页面） | 全部功能服务层页面 |
| **os-update** A/B 分区 OTA + 回滚 | ostree、CVE 自动推送、滚动升级 |
| os-discover 基础发现 + mTLS 互联 | 联邦/同级同步 |
| os-i18n 三语骨架 | 完整翻译覆盖 |
| 功能服务层仅 os-monitor + os-backup + os-security(2FA) | media/files/devtools/power 完整版 |
| os-im 基础对话（云端 LLM）+ **多 agent 协作架构基础**（中枢-领域拓扑、任务委派） | 本地推理、agent SDK 完整版、跨节点 agent 协作 |
| **os-wallet**（BTC+EVM 基础签名/凭证，WalletConnect v2） | Solana/Cosmos 等其他链、支付/授权高级场景 |
| **os-guest**（可选，含链上凭证身份，调 os-wallet） | 公钥升级高级模板、多链叠加策略 |

#### 11.4.2 三个新组件初版人/天拆解

##### osd（守护进程）

| 子任务 | 人天 | 说明 |
|--------|------|------|
| 组件注册表 + 依赖图建模 | 8 | 组件元数据、启动依赖拓扑 |
| 进程生命周期（启动/停止/重启） | 10 | systemd unit 生成 + tokio 监管 |
| 健康检查（HTTP/TCP/自定义） | 6 | 探活策略 + 退避重启 |
| cgroup v2 资源配额 | 8 | CPU/内存/IO 限制 via `cgroups-rs` |
| 优雅停机 + 信号处理 | 4 | SIGTERM 级联 |
| 配置热加载 | 4 | 运行时调整配额/依赖 |
| 集成测试（多组件仿真启停） | 8 | loop 设备 + 临时池 |
| 文档 + CI 集成 | 4 | |
| **小计** | **52 人天** | ≈ 2.6 人月 |

##### os-container-net（容器网络）

| 子任务 | 人天 | 说明 |
|--------|------|------|
| CNI 插件框架集成 | 8 | 调用 CNI 插件生命周期 |
| veth/bridge 创建与配置 | 10 | `rtnetlink` 编排 |
| NAT + 端口映射 | 8 | `nftnl` 规则生成 |
| 容器间 DNS 解析 | 6 | `hickory-dns` 嵌入式 |
| ZFS 卷→容器 bind mount | 6 | zvol + mount 编排 |
| 网络隔离（多网络/命名空间） | 6 | network namespace 隔离 |
| 与 youki 集成 + 容器创建链路 | 8 | 端到端跑通 |
| 集成测试（多容器互通/隔离） | 8 | |
| 文档 + CI | 4 | |
| **小计** | **64 人天** | ≈ 3.2 人月 |

##### os-update（OTA 在线升级）

| 子任务 | 人天 | 说明 |
|--------|------|------|
| A/B 分区方案设计 + bootloader 配置 | 10 | GRUB/systemd-boot 双槽切换 |
| 更新包格式 + 签名/校验 | 8 | 增量包 + ed25519 签名 |
| 下载 + 校验 + 写入 inactive 槽 | 10 | 断点续传 + 完整性校验 |
| 启动切换 + 健康验证 | 8 | 启动后探活，失败回滚 |
| 回滚机制（自动 + 手动） | 8 | watchdog + 手动触发 |
| 组件级更新（非整机，单组件热更） | 10 | 基于 osd 重启单个组件 |
| HA 滚动升级（逐节点）基础版 | 8 | follower→leader 顺序 |
| 集成测试（升级/回滚/断电恢复） | 10 | |
| 文档 + CI | 4 | |
| **小计** | **76 人天** | ≈ 3.8 人月 |

> **三组件合计：192 人天 ≈ 9.6 人月**

#### 11.4.3 初版全量人/天估算（含三组件）

| 模块 | 人天 | 人月 | 初版范围说明 |
|------|------|------|-------------|
| os-core + os-common + os-i18n | 65 | 3.3 | 领域模型+契约+三语骨架+**EventBus trait**（§9.1#9） |
| **osd** | 56 | 2.8 | 进程管理+cgroup+**NTP(chrony 编排)**（§9.1#8） |
| os-storage | 108 | 5.4 | ZFS 建池/数据集/快照/配额/主从复制+**块 export(iSCSI·NVMe-oF)**（§9.1#11） |
| os-smb（Samba 编排） | 40 | 2.0 | smb.conf+用户+共享+Time Machine |
| os-nfs + os-webdav + os-sftp | 50 | 2.5 | nfsserve+dav-server+russh 集成 |
| os-object（RustFS） | 40 | 2.0 | S3 基础功能 |
| os-meta（openraft HA） | 120 | 6.0 | 3 节点共识+VIP+故障转移+元数据复制 |
| os-discover | 40 | 2.0 | mDNS 发现+mTLS 互联 |
| os-app（youki 容器） | 60 | 3.0 | youki+镜像拉取+基础生命周期 |
| **os-container-net** | 64 | 3.2 | CNI+veth+NAT+DNS+卷挂载 |
| os-vm（KVM） | 50 | 2.5 | libvirt 编排+基础生命周期 |
| os-pkg（第三方 .deb） | 20 | 1.0 | apt 编排+.desktop 解析 |
| os-network（基础） | 80 | 4.0 | 接口/VLAN/桥/防火墙（不含 RDMA/DPU） |
| os-api + os-cli | 80 | 4.0 | REST+WebSocket+CLI |
| web-ui（Vue macOS 风） | 120 | 6.0 | 核心页面（池/共享/容器/VM/网络/监控） |
| os-backup + os-monitor + os-security | 90 | 4.5 | 快照调度+指标告警+2FA（其余 v2） |
| os-im（基础对话+多 agent 协作） | 60 | 3.0 | 云端 LLM+指令解析+中枢-领域拓扑+任务委派+事件总线 |
| os-provision（PXE 自举） | 40 | 2.0 | PXE+阶段化迁移基础版 |
| os-iso | 30 | 1.5 | 标准 ISO 构建+安装器 |
| **os-update** | 76 | 3.8 | A/B OTA+回滚 |
| **os-wallet**（新增） | 61 | 3.1 | WalletConnect v2+ChainAdapter(BTC/EVM)+RpcRegistry+条件激活+**ord index 集成**（§9.1#12） |
| **os-guest**（可选） | 170 | 8.5 | Captive Portal+访客身份+RBAC+IM集成+链上凭证编排（调 os-wallet）；AI 辅助后约 110 人天 |
| 测试 / CI / 文档 | 100 | 5.0 | 集成测试+多节点仿真+文档 |
| **合计（含 os-guest/os-wallet）** | **1620 人天** | **≈ 81.0 人月** | 约 **6.8 人·年**（不含可选 os-guest/os-wallet 为 1450 人天/6.1 人·年） |

#### 11.4.4 1 年内完成的团队配置建议

> 基准：不含可选 os-guest/os-wallet 的初版工作量 **1450 人天**（含则 1620 人天）。

| 团队规模 | 可用人天/年（扣除假期/缓冲） | 是否达 1450 人天 | 说明 |
|----------|---------------------------|-----------------|------|
| 5 人 | 5 × 220 = 1100 | ❌ 差 350 人天 | 需大幅砍范围或延期至 16 个月 |
| 6 人 | 6 × 220 = 1320 | ❌ 差 130 人天 | 需砍范围（如推迟 os-wallet/os-guest）或延期至 13 个月 |
| **7 人** | 7 × 220 = 1540 | ✅ 余 90 人天 | 达标，缓冲小，需严格控制范围 |
| **8 人** | 8 × 220 = 1760 | ✅ 余 310 人天 | **推荐**，有缓冲，可纳入 os-wallet/os-guest 可选项 |

> **结论**：1 年完成初版（不含可选访客/钱包）需 **7–8 人全职团队**（含前端 1–2、后端 Rust 4–5、测试/DevOps 1）。7 人为达标下限（缓冲小），8 人为推荐（可含可选组件）。若借助 §13 多 agent 协作开发放大效率，人力可适度折减（AI 辅助系数已在 os-guest 等行体现）。

#### 11.4.5 关键组件在 1 年排期中的位置

| 组件 | 建议启动月 | 建议完成月 | 并行依赖 |
|------|-----------|-----------|----------|
| **osd** | M1 | M3 | 最先启动，其他组件依赖其进程管理 |
| **os-container-net** | M4 | M7 | 依赖 os-app(youki) M3 完成 |
| **os-update** | M8 | M11 | 依赖系统基本稳定（M7）+ os-iso(M9) 提供包格式 |
| **os-guest**（可选） | M6 | M8 | 依赖 os-network(M3)+os-im(M4)+os-security(M4)+**os-wallet(M5)**；链上验证调 os-wallet；AI 辅助约 110 人天，2 人并行；多链 RPC 不可用时链上验证降级隐藏 |
| **os-wallet**（可选） | M5 | M7 | 依赖 os-core(M1)+os-security(M4)；WalletConnect+BTC/EVM；os-guest 的前置；AI 辅助约 35 人天 |

---

## 12. 架构层面补充建议

### 12.1 补充"系统守护进程管理器（osd）"
20+ Rust 组件 + Samba + QEMU + youki 需统一进程管理：
- 每个组件为独立 systemd service；
- 新增 `osd`（init 守护）负责：组件健康检查、自动重启、依赖排序、资源配额（cgroup v2）；
- `osd` 为 Rust 实现，作为 PID 1 之后的"OS 编排器"。

### 12.2 明确"配置即代码"的版本化与回滚
- 配置存储格式（TOML/JSON）与版本化（Git-like 或 openraft log）；
- 配置变更的原子性（事务）与回滚能力；
- 多节点配置同步（经 openraft 复制）。

### 12.3 API 版本化与向后兼容
- REST API 从 Day 1 引入版本前缀（`/api/v1/`）；
- WebSocket 消息需 schema 版本字段；
- 客户端（手机/桌面）兼容多版本 API（渐进升级）。

### 12.4 安全加固补充

| 遗漏 | 建议 |
|------|------|
| 密钥轮换自动化 | TLS 证书 / SMB 密码 / SSH key 定期轮换策略 |
| 安全更新推送 | 监听 CVE 公告，自动/手动更新 Samba/QEMU 等 C 依赖 |
| 容器镜像签名 | cosign / Notary v2 验证（`sigstore` Rust 绑定） |
| 审计日志防篡改 | 审计日志 append-only + 哈希链或远程归档 |

### 12.5 路线图调整建议

| 阶段 | 调整 |
|------|------|
| **P0** | 补充 CI 中 SMB/NFS 集成测试环境搭建 |
| **P1** | SMB 改为**编排 Samba**；NFS 用 v3；补充容器网络设计 |
| **P2** | 明确 SQLite HA 方案；补充 NTP 同步；同级 peer 改为**单向主从** |
| **P3** | 补充容器编排层（网络/卷/镜像管理）；补充 iSCSI target（编排 LIO） |
| **P4** | DPU/RDMA 明确为"实验性"标签 |
| **P5** | 补充 OTA 在线更新机制 |
| **P7** | 本地 LLM 改为可选；明确硬件要求；补**多 agent 协作架构**（中枢-领域拓扑，见 §3.7.2） |
| **P9** | 补充 scrub 调度、日志轮转、邮件通知 |
| **新增 P3.5** | 访客接入管理（os-guest）+ **多链钱包（os-wallet）**：Captive Portal + 访客身份 + 链上凭证 + RBAC + IM 群集成 + WalletConnect；M5-M8 可选纳入初版 |
| **新增 P10** | 远期：自研 SMB Server / NFSv4 / 多主同步 / NVMe-oF / Solana·Cosmos 链接入 / 本地大模型推理 |

---

## 13. 多 Agent 协作开发方案（工程方法论）

> **定位**：本章描述**开发期**如何用多个 AI 编码 agent 分工并行构建本系统，与 §3.7（运行时产品层的多 agent 协作架构）互补——前者是"用 agent 团队造系统"，后者是"系统内 agent 协作运行"。两者共用 agent 协作思想，但作用域不同。
>
> **背景**：本系统 22+ 组件、估算 22–33 人·年（§11.3），单靠人力难以在 1 年内交付初版。引入多 AI 编码 agent 协作可显著放大人力效率（§11.4 已按"AI 辅助"估算工期）。本章规范这套协作开发方法，确保 agent 协作**可控、可审、不破坏一致性**。

### 13.1 组件 → Agent 映射（Owner 制）

每个 crate 分配一个 **owner agent**（领域专家 agent），对该 crate 的实现负全责。映射关系与 §3.0 SSOT 总表一一对应，与 §3.7.2 运行时领域 agent 命名对齐（便于"开发期 agent"与"运行时 agent"心智模型统一）：

| Owner Agent（开发期） | 拥有的 crate | 依赖前置 |
|----------------------|-------------|---------|
| CoreAgent | os-core / os-common / os-i18n | 最先（无依赖） |
| OrchestratorAgent | osd | CoreAgent |
| StorageAgent | os-storage | CoreAgent |
| ProtocolAgent | os-protocols（含 smb/nfs/webdav/ftp/sftp/object） | StorageAgent |
| ComputeAgent | os-compute（vm/app/container-net/pkg） | StorageAgent / NetworkAgent |
| MetaAgent | os-meta | StorageAgent / OrchestratorAgent |
| DiscoverAgent | os-discover | MetaAgent / NetworkAgent |
| ProvisionAgent | os-provision / os-iso / os-update | NetworkAgent / DiscoverAgent / StorageAgent |
| NetworkAgent | os-network | CoreAgent |
| WalletAgent | os-wallet | CoreAgent / SecurityAgent |
| GuestAgent | os-guest | NetworkAgent / SecurityAgent / IMAgent / WalletAgent |
| SecurityAgent | os-security | CoreAgent |
| ServiceAgent | os-services（七子，可再拆子 owner） | 各自依赖 |
| IMAgent | os-im | 全体（agent 调度，最后接入） |
| ApiAgent | os-api / os-cli | 全体（网关聚合） |
| FrontendAgent | web-ui | ApiAgent / CoreAgent(i18n) |
| ClientAgent | os-mobile / os-desktop | ApiAgent / DiscoverAgent |

> **原则**：一个 crate 只有一个 owner agent（避免职责重叠）；一个 agent 可拥有多个强相关 crate（如 ProvisionAgent 拥 provision/iso/update 三件）。允许**临时子 agent**（如 ServiceAgent 下挂 backup/media 子 owner）。

### 13.2 协作契约（Contract-First）

agent 间通过**接口契约**解耦，而非通过共享实现：

- **契约定义**：所有跨 crate 调用经 `os-core`（领域类型）+ `os-common`（API schema/DTO）的 trait 定义。契约即代码，纳入版本控制。
- **接口先行**：依赖方的 trait 必须先于实现定义；owner agent 先提交 trait + mock 实现 + 测试桩，下游 agent 即可并行开发（不阻塞等待真实实现）。
- **契约变更协议**：trait 变更需提 ADR（架构决策记录），经**受影响 agent 会签**（如同运行时的多 agent 会签原语，见 §3.7.2）后合并；破坏性变更需提供迁移期。
- **示例**：WalletAgent 先定义 `ChainAdapter` trait + mock（返回假签名），GuestAgent 即可并行开发 chain-orchestrator 业务编排，不必等真实 EVM/BTC 实现。

### 13.3 集成协议（CI 门禁）

agent 并行开发的产物必须经集成协议保证一致：

- **CI 门禁**：每个 PR 自动跑 clippy/fmt/单元测；跨 crate 的集成测在 merge 前必跑（呼应 §8 测试策略）。
- **模块 owner 自测**：owner agent 提交前自测本 crate；集成测失败由触发方 agent 负责 fix 或回滚。
- **跨 agent 集成测**：依赖图上的关键路径（如 guest→wallet→security 签 JWT 链路）有端到端测，由 IMAgent 或专职 IntegrationAgent 维护。
- **环境**：集成测用 loop 设备临时 ZFS 池、多节点仿真（§8），不依赖真实硬件。

### 13.4 Agent 评审链（Review Pipeline）

仿运行时的"确认与投票"原语（§3.7.2），开发期设评审链：

| 变更类型 | 评审要求 |
|---------|---------|
| 单 crate 内、低风险 | owner agent 自审 + 自动 CI |
| 跨 crate、改 trait | owner agent + 受影响 agent **会签** |
| 高危/架构性（改 SSOT 总表、改章节编号、安全相关） | 多 agent 会签 + **人类复核**（必须） |
| 新增组件/章节 | 人类决策（参考本规划文档）+ owner agent 落地 |

- **ReviewAgent**：可设专职评审 agent（独立于实现 agent，避免自审盲区），对 PR 做静态分析、依赖检查、契约一致性校验。
- **会签即 ADR**：所有会签记录归档为 ADR，作为系统演进的可追溯依据。

### 13.5 共享知识库（Agent 上下文）

多 agent 协作需要共享上下文，避免各自为政：

| 知识源 | 内容 | 维护者 |
|--------|------|--------|
| **本规划文档（SSOT）** | §3.0 组件总表、§3.19 排除清单、各章节决策 | 人类 + 各 owner agent（提 PR 改） |
| **ADR 库** | 架构决策记录（为什么这么做、权衡、替代方案） | 各 agent 提，ReviewAgent 审 |
| **契约库** | os-core/os-common 的 trait + DTO 定义 | 各 owner agent |
| **依赖图** | crate 间依赖拓扑（动态生成自 Cargo workspace） | CI 自动 |
| **术语表** | 领域术语统一（如"同级 peer""链上凭证访客"） | CoreAgent |

> agent 每次开工前**必须读取相关 ADR + 契约 + 本文档对应章节**，确保决策一致；不得凭记忆假设。

### 13.6 并行度与瓶颈分析

依赖图（§13.1）决定可并行度：

- **可高度并行**：os-storage / os-network / os-security / os-wallet（依赖仅 CoreAgent，M1 后即可并行）
- **串行瓶颈**：
  1. **CoreAgent（os-core/os-common）**——所有人依赖，必须 M1 先行完成 trait 骨架
  2. **OrchestratorAgent（osd）**——进程管理被多数组件依赖，M1–M3 优先
  3. **契约定义**——任何 trait 变更都是全局同步点；用"接口先行 + mock"缓解
  4. **集成测试**——端到端链路必须在多数组件就绪后才能跑，是后期瓶颈；建议 IMAgent 早期就介入搭测试骨架
- **关键路径**：CoreAgent → OrchestratorAgent/StorageAgent → MetaAgent → IMAgent → 集成。压缩关键路径是提速核心。

### 13.7 与运行时多 agent 架构的关系

| 维度 | 开发期（本章 §13） | 运行时（§3.7.2） |
|------|------------------|-----------------|
| 主体 | AI 编码 agent（造系统） | 系统 AI agent（操作系统） |
| 协作对象 | crate / 代码 / PR | 执行组件 / Tool / 任务 |
| 拓扑 | Owner 制 + 评审链 | 中枢-领域（Orchestrator-Specialist） |
| 共享原语 | 会签、契约、ADR | 任务委派、黑板、投票 |
| 生命周期 | 开发阶段（一次性 + 迭代） | 系统运行期（常驻） |

> 命名对齐（开发期 WalletAgent ↔ 运行时 WalletAgent）便于心智模型统一，但二者是不同实体，勿混淆。

### 13.8 风险与红线

| 风险 | 缓解 |
|------|------|
| agent 各自为政、破坏一致性 | SSOT + 契约先行 + 会签 + CI 门禁（本章核心机制） |
| trait 频繁变更致下游阻塞 | 接口先行 + mock；变更走 ADR + 迁移期 |
| agent 生成不安全/错误代码 | ReviewAgent + 人类复核（高危必经）；安全相关测试强制 |
| agent "幻觉"虚构 API/依赖 | 契约库为准；CI 编译验证；禁止依赖未发布/虚构 crate |
| 人类失去对架构掌控 | SSOT/ADR/章节变更必须人类决策；agent 只落地不决策架构 |

---

## 14. 参考来源
[1] Ubuntu 26.04 LTS 发布说明 — https://documentation.com/release-notes/26.04/
[2] openraft（Rust Raft 共识）— https://github.com/databendlabs/openraft
[3] youki（Rust OCI 运行时）— https://github.com/youki-dev/youki
[4] Ubuntu 26.04 系统要求上调（6GB RAM 等）— https://www.omgubuntu.co.uk/2026/04/ubuntu-2604-system-requriments
[5] macOS 风 Vue 参考：MacOS-Web-UI — https://github.com/HammCn/MacOS-Web-UI ；MacWeb — https://github.com/iAJue/MacWeb
[6] 协议 crate：nfsserve(https://github.com/huggingface/nfsserve) / dav-server(https://github.com/messense/dav-server-rs) / libunftp(https://github.com/bolcom/libunftp) / russh(https://github.com/Eugeny/russh) / RustFS(https://github.com/RustFS/RustFS) ；SMB 编排 Samba(https://www.samba.org) ；NFSv4 编排 nfs-ganesha(https://github.com/nfs-ganesha/nfs-ganesha)
[7] 镜像分发：oci-distribution — https://github.com/containers/oci-distribution-rs
[8] 网络组件：netavark(Rust 网络栈) — https://github.com/containers/netavark ；dora(DHCP) — https://github.com/bluecatengineering/dora ；dhcprs — https://docs.rs/dhcprs ；hickory-dns — https://github.com/hickory-dns/hickory-dns ；pxe-np — https://github.com/leruetkins/pxe-np
[9] PXE 引导：iPXE — https://ipxe.org （os-network 的 PXE 基于 iPXE 提供引导，os-provision 复用其自举能力）
[10] 节点发现：mdns-sd（Rust mDNS/DNS-SD）— https://github.com/keepsimple1/mdns-sd （os-discover 的局域网发现基于此；认证用 rustls 做 mTLS）
[11] 客户端技术：Tauri（Rust 桌面，嵌入 Web UI）— https://tauri.app ；Capacitor（Vue 跨端到 iOS/Android）— https://capacitorjs.com ；移动端 mDNS 需本地网络权限
[12] RDMA/IB/RoCE：rdma-core（OFED 用户态栈）— https://github.com/linux-rdma/rdma-core ；async-rdma（Rust RDMA 封装）— https://github.com/datenlord/async-rdma
[13] DPU：NVIDIA BlueField/DOCA — https://developer.nvidia.com/networking/doca ；带外标准 Redfish — https://www.dmtf.org/standards/redfish ；devlink（内核 SR-IOV/SF 管理）— https://docs.kernel.org/networking/devlink/
[14] ISO 打包：xorriso — https://www.gnu.org/software/xorriso/ ；squashfs-tools — https://github.com/plougher/squashfs-tools ；参考 live-build（Debian/Ubuntu ISO 构建）— https://salsa.debian.org/live-team/live-build
[15] 功能服务层：opentelemetry-rust — https://github.com/open-telemetry/opentelemetry-rust ；tantivy(全文搜索) — https://github.com/quickwit-oss/tantivy ；oauth2-rs — https://github.com/ramosbugs/oauth2-rs ；totp-rs — https://crates.io/crates/totp-rs ；rcgen(CA) — https://crates.io/crates/rcgen ；boringtun(WireGuard) — https://github.com/cloudflare/boringtun ；gix(Rust Git) — https://github.com/Byron/gitoxide ；Immich(媒体架构参考) — https://github.com/immich-app/immich
[16] 国际化：fluent-rs（Mozilla，Rust）— https://github.com/projectfluent/fluent-rs ；rust-i18n — https://github.com/longbridge/rust-i18n ；前端 vue-i18n — https://vue-i18n.intlify.dev/
[17] 多链钱包/签名（os-wallet，新增）：rust-bitcoin(BTC 纯 Rust) — https://github.com/rust-bitcoin/rust-bitcoin ；secp256k1 — https://github.com/rust-bitcoin/rust-secp256k1 ；alloy(EVM 纯 Rust) — https://github.com/alloy-rs/alloy ；BIP-322(签名验证标准) — https://github.com/bitcoin/bips/blob/master/bip-0322.mediawiki ；EIP-191/EIP-712(EVM 签名) — https://eips.ethereum.org/EIPS/eip-191 / https://eips.ethereum.org/EIPS/eip-712 ；Ordinals spec — https://github.com/ordinals/ord ；ERC-721/ERC-1155(NFT) — https://eips.ethereum.org/EIPS/eip-721 / https://eips.ethereum.org/EIPS/eip-1155
[18] WalletConnect（移动钱包签名主流协议）：WalletConnect v2 规范 — https://specs.walletconnect.com/ ；Relay 协议 — https://docs.walletconnect.com/ ；自托管 Relay — https://docs.walletconnect.com/relay-server/overview
[19] 多 agent 协作（§13 工程方法论参考）：ADR（架构决策记录）— https://adr.github.io/ ；contract-first 开发 — 以 os-core/os-common trait 为契约；monorepo + workspace 依赖图（Cargo）— https://doc.rust-lang.org/cargo/reference/workspaces.html

---

## 15. 接口契约索引（Contract Index）

> **SSOT 说明**：全部组件的接口契约以 `OS_System/` 工程目录下的 Rust 源文件为**唯一权威来源**（可 `cargo check` 验证）。本章节为索引与规范摘要，不重复罗列 trait 全文。
>
> 本章节呼应 §13.2 Contract-First：接口先行，owner agent 先提交 trait + mock，下游 agent 即可并行开发。AI agent 集群开发的起点即此处的契约。

### 15.1 全局契约规范（所有 crate 一致）

| 维度 | 规范 | 说明 |
|------|------|------|
| **异步模型** | 数据路径 trait 用原生 `async fn in trait`（Rust 1.75+，无 `#[async_trait]`）；管理/配置 trait 用同步 fn | lib.rs 顶部统一 `#![allow(async_fn_in_trait)]`；异步 trait 有 `Send` bound 支持 trait object |
| **错误模型** | 每 crate 自定义 `Error` 枚举（`#[derive(thiserror::Error)]`）+ `Result<T, Self::Error>` 别名 | 跨 crate 边界用各自 Result；统一经 `impl From<XxxError> for os_common::ApiError` 转换为对外 API 错误码 |
| **ID 规范** | 领域 ID 全部用 newtype（集中 `os-core::ids`） | `PoolId`/`DatasetId`/`VmId`/`ContainerId`/`GuestId`/`NodeId`/`TaskId`/`WalletSessionId`/`ChainId`/`AddressId`/`ShareId`/`VolumeId` 等，编译期防互赋 |
| **API 版本** | 对外 DTO 带 `api_version: u16`（`os-common::Versioned`/`VersionedEnvelope`） | 呼应 §12.3；客户端据此做版本兼容降级 |
| **命名** | trait 名能力动词（`StorageBackend`/`WalletConnector`）；方法名动词开头（`create_pool`/`sign_message`） | 所有 pub 项带中文 `///` 文档注释，说明用途/并发性/错误条件 |
| **依赖** | 全 workspace 共用 `[workspace.dependencies]`，crate 间用 `.workspace = true` + `os-xxx = { path = "crates/os-xxx" }` | 单一版本源，避免依赖碎片化 |
| **实现留空** | 每个 crate 的 `src/*.rs` 只放 trait + 数据结构 + Error，**不含实现** | 实现由 owner agent 后续填充（`impl XxxTrait for ...`）；trait 定义本身可独立 `cargo check` |

### 15.2 组件契约索引表

> 按依赖层级排列（呼应 §13.1 Owner 制依赖图）。"路径"列相对 `OS_System/`。

#### 第 0 层 — 基础（无依赖，最先）

| # | crate | 关键 trait | 路径 | owner agent |
|---|-------|-----------|------|------------|
| 1 | `os-core` | `EventBus`、`EventSubscriber` | `crates/os-core/src/{lib,ids,types,eventbus}.rs` | CoreAgent |
| 2 | `os-common` | `Versioned` | `crates/os-common/src/{lib,error,versioned}.rs` | CoreAgent |

#### 第 1 层 — 基础服务

| # | crate | 关键 trait | 路径 | owner agent |
|---|-------|-----------|------|------------|
| 3 | `os-i18n` | `Translator`、`Localizable` | `crates/os-i18n/src/*.rs` | CoreAgent |
| 4 | `osd` | `Orchestrator`、`HealthProbe`、`NtpManager` | `crates/osd/src/*.rs` | OrchestratorAgent |

#### 第 2 层 — 存储/网络/安全（可并行）

| # | crate | 关键 trait | 路径 | owner agent |
|---|-------|-----------|------|------------|
| 5 | `os-storage` | `StorageBackend`、`Replication`、`BlockExport`、`CryptoManager` | `crates/os-storage/src/*.rs` | StorageAgent |
| 6 | `os-network` | `NetworkManager`、`Firewall`、`DhcpServer`、`DnsServer`、`PxeServer`、`RdmaManager`、`DpuBackend` | `crates/os-network/src/*.rs` | NetworkAgent |
| 7 | `os-security` | `AuthProvider`、`JwtIssuer`、`CertManager`、`TwoFactor`、`VpnManager` | `crates/os-security/src/*.rs` | SecurityAgent |

#### 第 3 层 — 协议/计算/钱包

| # | crate | 关键 trait | 路径 | owner agent |
|---|-------|-----------|------|------------|
| 8 | `os-protocols` | `FileProtocol`（父）→ `SmbManager`/`NfsManager`/`WebDavManager`/`FtpManager`/`SftpManager`（子）；`ObjectStore` | `crates/os-protocols/src/*.rs` | ProtocolAgent |
| 9 | `os-compute` | `VmManager`、`ContainerRuntime`、`ContainerNetwork`、`PackageManager` | `crates/os-compute/src/*.rs` | ComputeAgent |
| 10 | `os-wallet` | `WalletConnector`、`ChainAdapter`、`RpcRegistry` | `crates/os-wallet/src/*.rs` | WalletAgent |

#### 第 4 层 — 集群/发现/访客

| # | crate | 关键 trait | 路径 | owner agent |
|---|-------|-----------|------|------------|
| 11 | `os-meta` | `Consensus`、`DistributedKv`、`FailoverOrchestrator`、`VipManager`、`MetaStore` | `crates/os-meta/src/*.rs` | MetaAgent |
| 12 | `os-discover` | `Discovery`、`PeerCallback`、`PeerAuthenticator`、`FederationPolicy` | `crates/os-discover/src/*.rs` | DiscoverAgent |
| 13 | `os-guest` | `CaptivePortal`、`IdentityEngine`、`PolicyEngine`、`NftRuleOrchestrator`、`ChainOrchestrator` | `crates/os-guest/src/*.rs` | GuestAgent |

#### 第 5 层 — 编排/分发/更新/IM/API

| # | crate | 关键 trait | 路径 | owner agent |
|---|-------|-----------|------|------------|
| 14 | `os-provision` | `Provisioner`、`MigrationEngine` | `crates/os-provision/src/*.rs` | ProvisionAgent |
| 15 | `os-iso` | `IsoBuilder`、`Installer` | `crates/os-iso/src/*.rs` | ProvisionAgent |
| 16 | `os-update` | `UpdateEngine`、`RollbackManager`、`CveMonitor`/`CveCallback`、`RollingUpgrade` | `crates/os-update/src/*.rs` | ProvisionAgent |
| 17 | `os-im` ★ | `ConversationStore`、`Tool`、`Agent`、`AgentOrchestrator`、`SharedContext`、`ConfirmationGate`、`LlmBackend` | `crates/os-im/src/*.rs` | IMAgent |
| 18 | `os-api` | `Gateway`、`RouteHandler`、`Middleware`、`WebSocketHub` | `crates/os-api/src/*.rs` | ApiAgent |

#### 第 6 层 — 功能服务/客户端/CLI

| # | crate | 关键 trait | 路径 | owner agent |
|---|-------|-----------|------|------------|
| 19 | `os-services` | `BackupManager`、`Monitor`、`MediaManager`、`FileManager`、`DevTools`、`PowerManager` | `crates/os-services/src/*.rs` | ServiceAgent |
| 20 | `os-cli` | `Command`、`OutputFormatter` | `crates/os-cli/src/*.rs` | ApiAgent |
| 21 | `os-mobile` | `OsClient`、`PushSubscriber`/`PushCallback` | `crates/os-mobile/src/*.rs` | ClientAgent |
| 22 | `os-desktop` | （复用 `os-mobile::OsClient`）、`MountManager` | `crates/os-desktop/src/*.rs` | ClientAgent |

> **trait 统计**：22 crate / 约 89 个 trait（含 trait 继承的父子关系与回调 trait）。

### 15.3 契约间关键依赖关系（非循环）

```
os-core ◀── 全员依赖（ID/EventBus/Health/领域模型）
os-common ◀── 全员依赖（ApiError/Versioned）
   │
   ├── os-i18n / osd（第1层）
   ├── os-storage / os-network / os-security（第2层，可并行）
   │       └── os-security → os-network（vpn 复用 IpCidr）
   ├── os-protocols（→storage）/ os-compute（→storage,network）/ os-wallet（→security）（第3层）
   ├── os-meta（→network）/ os-discover / os-guest（→network,security,wallet）（第4层）
   ├── os-provision（→storage,network,discover,meta）/ os-iso / os-update / os-im / os-api（→security）（第5层）
   └── os-services（→storage）/ os-cli（→api）/ os-mobile（→discover）/ os-desktop（→mobile）（第6层）
```

依赖图**必须无环**（os-im 的 `AgentOrchestrator.delegate` 运行期也强制任务图无环，见 §3.7.2）。Trait 注入方向：上层（如 osd）持有下层 trait object（`Box<dyn StorageBackend>`），下层不反向依赖上层。

### 15.4 AI agent 使用说明

**对 owner agent（实现 trait）**：
1. 读取本契约（`crates/<your-crate>/src/*.rs`）+ 相关 ADR + 本文档对应 §3.x 小节
2. 创建实现 struct，`impl XxxTrait for YourImpl`；方法体可先 `todo!()` 再逐步填充
3. 提交前 `cargo check -p <your-crate>` 确保契约未被破坏
4. 提供 mock 实现（`MockXxx`）供下游 agent 在真实实现就绪前并行开发

**对下游 agent（依赖某 trait）**：
1. 仅依赖 trait 定义，**不依赖具体实现**——通过 `Box<dyn XxxTrait>` 或泛型注入
2. trait 变更须提 ADR + 受影响 agent 会签（§13.2 / §13.4）
3. 在真实实现就绪前，用上游提供的 mock 跑通自己的链路

**对 ReviewAgent（评审）**：
1. 校验 PR 是否破坏既有 trait 签名（破坏性变更需 ADR + 迁移期）
2. 校验跨 crate 类型复用（不得重复定义 os-core/os-common 已有类型）
3. 校验错误码映射完整性（每 crate 的 Error 须实现 `From for ApiError`）

### 15.5 编译验证

工程根 `OS_System/` 具备 Rust 工具链时：
```bash
cd OS_System
cargo check --workspace          # 全部 crate 类型检查（无实现亦可通过）
cargo check -p os-storage       # 单 crate 检查
cargo doc --workspace --no-deps  # 生成全部 trait 文档
```

> 契约文件不含实现，`cargo check` 仅验证 trait/数据结构语法与跨 crate 类型解析；真正功能验证在 owner agent 填充实现后的单元/集成测试阶段（呼应 §8 测试策略）。

---

## 16. 子代理集群规划（AI Agent Swarm）

> **SSOT 说明**：子代理集群的完整规格以 `OS_System/docs/agents/` 目录为**唯一权威来源**。本章节为索引与编排模型摘要。
>
> 本章节深化 §13（开发期工程方法论）的"组件→Agent 映射"为**可直接执行的 agent 规格书体系**，为未来 AI agent 集群启动做准备。本次只规划，不启动任何 agent、不写 crate 实现。

### 16.1 编排模型（多会话 + Git 协作）

- **多会话 agent + Git 协作**：每 agent 独立 AI 会话（独立上下文窗口），经 Git 分支（`agent/<agent_id>`）+ PR 协作。
- **主代理 = OrchestratorAgent**（人类操作的调度会话）：分派任务、仲裁会签、升级人类决策。**不写 crate 代码，不兼任 owner**。
- **27 个 owner agent**（执行方）：各自拥有一组 crate/trait 的实现权。
- **4 个辅助 agent**（横切支持）：review / integration / devops / docs。

详细协作约定（Git 分支/PR/ADR/会签/进度日志/mock/可恢复性）见 `OS_System/docs/agents/_conventions.md`。

### 16.2 agent 拓扑（31 个 = 27 owner + 4 辅助）

> 完整清单见 `OS_System/docs/agents/README.md` §2。本表为摘要。

**owner agent（27，按启动批次）**：

| 批次 | agent | 数 | 说明 |
|------|-------|---|------|
| 0 | core / i18n / orchestrator | 3 | 全员基础，最先 |
| 1 | storage / network(基础5) / rdma / security | 4 | core mock 后 |
| 2 | protocol(文件) / object / vm / container / wallet / meta / iso | 7 | 高度并行 |
| 3 | discover / guest / provision / update / backup / monitor / media / files / devtools / power | 10 | 并行度最高（service 七组件全拆） |
| 4 | im / api / client | 3 | 最后接入（依赖全体） |

**辅助 agent（4）**：review（PR 评审，批0）/ integration（跨 crate 集成测，批2）/ devops（CI/CD，批0）/ docs（文档，批1）

> **智能拆分原则**：高内聚低耦合。领域差异大的拆为独立 agent（service 七组件、compute 的 VM/容器、protocol 的文件/对象、network 的基础/RDMA、provision 的三阶段）；内聚的不拆（storage 围绕 ZFS、im 围绕协作、meta 围绕共识）。拆分使批 3 并行度从 4→10。

### 16.3 规格书体系

每份规格书（`docs/agents/<agent_id>.md`）含统一 **12 章节**：
1. 身份 / 2. 使命陈述 / 3. 拥有的契约 / 4. 输入契约 / 5. 输出要求(DoD) / 6. 依赖前置 / 7. 并行性分析 / 8. 验收标准 / 9. 风险红线 / 10. 示例工作流 / 11. 启动 prompt 模板 / 12. 恢复协议

模板见 `docs/agents/_template.md`。每份规格书可独立启动——复制 §11 的 prompt 到新 AI 会话即可。

### 16.4 关键设计原则

| 原则 | 落地 |
|------|------|
| **接口先行**（§13.2） | trait 契约已定（§15），agent 实现 trait，不经口头约定 |
| **mock 解锁并行**（§13.2） | 上游先交付 mock，下游即可并行，不必等真实实现 |
| **可恢复** | 任何 agent 会话重启后能仅凭文件（规格书+约定+PROGRESS+契约）恢复；不依赖会话内记忆 |
| **主代理不写码** | 调度与实现分离，避免裁判兼运动员 |
| **ADR 治变更** | 破坏性变更（改 trait 签名）必须 ADR + 受影响 agent 会签，禁止擅改契约 |
| **任务交接文件化** | PROGRESS.md（进度）+ TASKS.md（任务队列）+ ADR（决策）落盘，防上下文丢失 |

### 16.5 启动入口

完整的启动指南、批次编排、单 agent 启动步骤见 `OS_System/docs/agents/README.md` §3。核心流程：
1. 主代理按批次分派任务（写 `docs/agents/<agent_id>/TASKS.md`）
2. 复制该 agent 规格书 §11 的启动 prompt 到新 AI 会话
3. agent 读规格书 + 约定 + 进度 + 契约后开工
4. 经 PR 协作；review-agent 评审；integration-agent 集成测
5. 主代理仲裁会签冲突、升级人类决策

> **状态**：规格书体系就绪（待最终校验）。启动时机由人类决策；启动后按批次推进，token 充裕支撑 31 agent 并行。
