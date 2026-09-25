# NexOS 技术路线（Rust 系统核心）

> **本文作用**：对外技术路线宣示 + 对内架构决策存档。面向零上下文读者——读完应能回答：
> NexOS 是什么、系统怎么切分、关键决策为什么这么定、接下来往哪走。
>
> **版本口径**：v0.1.51，2026-09-25。文中全部数字为当日实测（口径见 §8 量化数字表）；
> 未实现的能力一律标注「规划」。
>
> **文档历史**：本文为 v2 全面重写，替代 2026-07 立项期规划版（22 crate 规格 / 工期与
> 人力估算 / 多 agent 开发方法论 / 31 agent 集群规划——这些内容已完成历史使命，原文可在
> git 历史查看）。立项期的调研结论（SMB 无纯 Rust 实现故编排 Samba、openraft 共识、
> youki/runc 容器路径等）均已落地，其沉淀收录于本文 §5 决策记录。

---

## 1. 定位与理念

**NexOS 是每个独立个体（超级个体）的操作系统，不是 NAS 系统。**（2026-08-20 定位纠偏）

AI 时代，一个人加 AI agent 可以完成过去一个团队的事——但强大的个体各自为战，形成
信息孤岛。NexOS 的使命是**连接**：每个 NexOS 是对等的独立节点，经 P2P 组网互联成个体
网络，打破五类孤岛：

| 孤岛 | NexOS 的解法 |
|------|-------------|
| 设备孤岛 | 蓝牙 mesh 中继，设备间无网也能通信 |
| 数据孤岛 | 统一存储（ZFS）+ 文件共享 + 云同步 |
| AI 孤岛 | 模型管理 + API 网关 + 联邦大厅，AI 能力互通 |
| 身份孤岛 | 链上身份（secp256k1 公钥即身份）+ IM 联邦，跨节点身份漫游 |
| 代码孤岛 | NexHub 代码枢纽（SSH + HTTP Smart Git 双通道 + CI + webhook） |

存储（ZFS/共享/备份）是**附带能力**而非定位——它是连接的数据底座。完整理念见
[PHILOSOPHY.md](PHILOSOPHY.md)；P2P 组网设计见
[docs/NEXOS_P2P_NETWORK_DESIGN.md](docs/NEXOS_P2P_NETWORK_DESIGN.md)。

**技术栈立场**：系统核心（管理面、编排、协议、P2P、网关、前端框架）用 Rust 从零构建；
不可重造的 C 事实标准（ZFS/KVM/Samba/ffmpeg 等）与成熟外部部件（Blender/vLLM 等）以
编排与探测式集成协作——「纯 Rust」指控制面收敛到 Rust，而非排斥生态（见 §5 ADR-02）。

---

## 2. 当前架构总览

一套 **28 crate 的 Rust workspace**（26 业务 crate + 2 辅助），单进程网关 `os-api`
聚合 46 个 RouteHandler 组件对外，内嵌 Vue3 Web 桌面；节点间经 `os-p2p` 加密 overlay
组网；媒体应用以独立产品线形态在应用生态中分发（os-api 只保留系统职责）。

```
┌─ 客户端 ────────────────────────────────────────────────────────────────────┐
│ 浏览器（Vue3 Web 桌面：28 内置应用 + 运行时可装应用包）                        │
│ os CLI · os-mobile / os-desktop SDK · os-mcp（MCP Server，AI 助手直管系统）    │
│ AI agent（链上身份 + WS + IM 大厅，@ 定向投递）                                │
└──────────────┬──────────────────────────────────────────────────────────────┘
               ▼ HTTP/WS（生产 8558 端口，单端口四流量：Web/API/git/metrics）
┌─ 系统核心：os-api 单进程网关（systemd 服务）─────────────────────────────────┐
│  中间件链：限流 → 认证（admin token / JWT / 链上身份 token）→ 审计             │
│  46 个 RouteHandler 组件（+1 条件 extra），静态路由 377 条（§8 口径）          │
│  特挂路由：/git（Smart HTTP CGI）· /dist（流式直传 Range 206）·               │
│           /s/:appId（独立网页应用）· /ws（IM/终端/直播/事件）· /healthz         │
│  存储面：storage/files/share/backup/cloudsync                                  │
│  计算面：compute/containers                                                    │
│  媒体面：media/media-gen/live（P2P 联邦直播，常开）                             │
│  协作面：im/agent-coord/agenthub/llm/model_hub/api_gateway/api-market/tips     │
│  系统面：system/network/monitor/downloads/notes/power/terminal/devdocs/update  │
│  连接面：p2p/node_view/identity/transfer/network-exit/discover/provisioning    │
│  生态面：apps/app_store/code_repo/nexhub_cli/nexhub_ci/nexhub-lobby            │
│  链上面：blockchain（节点运行 geth/bitcoind 子进程管理 + 支付验真）             │
│  其他：user/qr_transfer/ble_hub/forwarding/capabilities                        │
└──────┬───────────────────────────────────────────────────────────────────────┘
       ▼（按需调用领域 crate / 编排外部系统）
┌─ 领域 crate 层（26 业务 crate，§3 详解）──────────────────────────────────────┐
│ os-core/os-common/os-i18n（核心）· osd/os-im（编排）· os-api/os-nexhub（网关）  │
│ os-storage（ZFS）· os-network（nftables/netlink）· os-security（JWT/ACME/VPN） │
│ os-protocols（SMB/NFS/FTP/SFTP/WebDAV）· os-compute（KVM/OCI）                 │
│ os-meta（openraft HA）· os-discover（mDNS/mTLS）· os-wallet（多链签名验签）     │
│ os-p2p（Kademlia overlay）· os-identity（指纹账本）· os-guest（访客接入）       │
│ os-provision/os-iso/os-update（部署三件套）· os-services（六功能组件）          │
│ os-cli/os-mobile/os-desktop/os-mcp（客户端四件套）                              │
└──────┬───────────────────────────────────────────────────────────────────────┘
       ▼（spawn 子进程 / FFI(feature 门控) / HTTP / SQL）
┌─ 外部件协作层（探测式集成，ADR-08）──────────────────────────────────────────┐
│ ZFS(zfs/zpool CLI) · Samba · libvirt/QEMU(KVM) · runc/Docker · aria2 · rclone  │
│ ffmpeg（转码/合成） · Blender 4.4（headless 场景渲染） · vLLM（本地推理，        │
│ OpenAI API 兼容） · sd-turbo（本地生图） · vraminfo（C 工具：显存结温 MMIO      │
│ 直读，nvidia-smi 在 Linux 不提供） · geth/bitcoind（链节点） · git（子进程+CGI） │
│ SQLite（分库自治：gateway/im/media/monitor/apps/tips/ci/hub_lobby/chain-nodes） │
└───────────────────────────────────────────────────────────────────────────────┘
════════════════ P2P 加密 overlay（os-p2p，节点间） ═════════════════════════════
 Kademlia DHT（Swarm 同款全分布式）· ECDH+AESGCM 链路 · NAT 中继/TCP 打洞 ·
 mDNS LAN 种子 · 节点元数据（唯一存活源）· 联邦大厅/IM/DM/直播/传输/出口共享
═════════════════════════════════════════════════════════════════════════════════
┌─ 独立产品线（各自 NexHub 仓独立迭代，不在 os-api 内）────────────────────────┐
│ FilmStudio（AI 影片制作，Vue 前端 + 独立 axum 服务）                            │
│ StreamingStudio（流媒体中心/监控转码，同模式）                                  │
│ 经应用中心安装；系统内 live 联邦直播保留（§7）                                   │
└───────────────────────────────────────────────────────────────────────────────┘
```

多节点实况：跨 LAN/NAT/公网的多节点集群长期在跑（x86 主开发节点、公网云节点、
aarch64 边缘节点 DGX Spark、独立云锚点 p2p-node 等），覆盖同网段直连、跨 NAT 中继、
公网通告（`NEXOS_P2P_ADVERTISE`）三种拓扑；aarch64 双架构分发件 + 一键安装脚本
（`curl | bash`）支持新节点一条命令入集群。

---

## 3. 分层职责

### 3.1 系统核心（workspace 内，随 os-api 编译）

| 层 | crate | 职责要点 |
|----|-------|---------|
| 核心 | os-core / os-common / os-i18n | newtype ID、EventBus、ApiError；**chain_auth 链上身份内核**（挑战-签名三步，IM/NexHub/agent 共用）；**gateway 网关契约**（RouteSpec/RouteHandler，组件抽离的关键下沉）；后端三语（简中/繁中/英） |
| 编排 | osd / os-im | systemd 编排、cgroup v2、chrony NTP；IM+多 agent 协作中枢（Tool 注入、任务委派、黑板、确认门） |
| 网关 | os-api / os-nexhub | 单进程网关 + Web 桌面 + 全部 RouteHandler；NexHub（代码仓/大厅/Issues-PR/CI/webhook/CLI）独立 crate 经桥接注册 |
| 存储 | os-storage | ZFS 全链：建池/数据集/快照/配额/native 加载密钥/send-recv 复制/可导入池探测与一键导入/删池 export·destroy 双模式；iSCSI/NVMe-oF 块 export |
| 网络 | os-network | 接口/VLAN/桥、nftables 事务（FFI feature 门控）、DHCP/DNS/PXE；iptables 链持久化防火墙 |
| 安全 | os-security | Argon2id、JWT、TOTP、内部 CA + ACME、WireGuard VPN |
| 协议 | os-protocols | SMB（编排 Samba，Time Machine）/NFS（ganesha 编排）/WebDAV/FTP/SFTP（纯 Rust 协议栈） |
| 计算 | os-compute | KVM（libvirt，含 CPU 虚拟化预检诊断）/OCI 容器（runc 往返 + CNI）/apt 第三方包 |
| 共识 | os-meta | openraft HA、SQLite 快照复制、VIP、故障转移 |
| 发现 | os-discover / os-identity / os-guest | mDNS+mTLS 联邦决策；指纹账本（NodeID↔地址证据、冲突记账）；Captive Portal 访客四类身份 + RBAC |
| 组网 | os-p2p | 全分布式 Kademlia、加密 overlay、NAT 中继信箱、节点元数据（唯一存活源）、P2P 网状分发（transfer 五帧协议）、WAN 出口共享（net-exit：SOCKS5 over overlay） |
| 支撑 | os-wallet | BTC（BIP-322/Schnorr）+ EVM（EIP-191/712）签名验签、多链凭证查询、RPC 条件激活 |
| 部署 | os-provision / os-iso / os-update | PXE/一键安装/iPXE 网络装机；ISO 打包 + Rust 安装器；A/B 双槽位 OTA + watchdog 回滚 + 版本感知升级 |
| 服务 | os-services | backup/monitor/media/files/devtools/power 六组件 |
| 客户端 | os-cli / os-mobile / os-desktop / os-mcp | 运维 CLI；移动/桌面 SDK；MCP Server（AI 助手经 JSON-RPC 管理本系统） |
| 辅助 | os-integration / nettest | 跨 crate 集成测聚合点；网络栈真机冒烟（全 #[ignore]） |

### 3.2 独立产品线（剥离终态，2026-09 v0.1.51）

媒体应用**全部独立产品线**，各自 NexHub 仓独立迭代发版：

- **FilmStudio**：AI 影片制作全流程（导入→清理→分章→向量化→分镜→定妆→音频→合成），
  自带 Vue 前端与独立服务，项目即 git 仓（内置 Issues/PR/分支/回滚）。
- **StreamingStudio**：流媒体中心与监控转码。

os-api **仅保留系统职责**：引擎代码已删除出主程序，历史应用包安装时 400 拦截并引导
改用独立版（`STRIPPED_ENGINES`）。**例外**：`live` P2P 联邦直播是联邦基础能力，
作为独立组件常开保留。演进史见 §7。

### 3.3 外部件协作（不重造轮子）

系统级 C 事实标准（ZFS/KVM/libvirt/Samba/nftables 内核设施）走**编排**：Rust 生成
配置、管理生命周期、解析机器可读输出。应用级外部部件（ffmpeg/Blender/vLLM/sd-turbo/
vraminfo/geth）走**探测式集成**（ADR-08）：装了就用、没装诚实降级并给安装指引，
Cargo 零强依赖。vraminfo 是典型样本：MIT 单文件 C 工具，补 nvidia-smi 在 Linux 上的
空白（GDDR6X/7 显存结温 MMIO 直读 + NVAPI 颗粒厂商/类型），由 NexHub 自管仓
`v1.0.0-nexos` 分发，os-api 侧 sudo 自适应提权采集（ADR-09）。

---

## 4. 关键链路（数据怎么流）

**带鉴权写请求**：浏览器 → `POST /api/v1/...`（Bearer token）→ 限流 → 认证
（注入 Principal）→ 审计 → method 分桶路由匹配（O(1)）→ RouteHandler.handle() →
领域 crate / 外部件 → ApiResponse JSON。

**跨网消费联邦模型**（典型联邦链路）：节点 B 的应用 → SDK `capabilities()` 发现 →
经网关渠道 `via_node` 中继 → overlay 定向到节点 A 代发 HTTP（api_relay 分块协议，
流式逐块回传；白名单封闭集防开放代理）→ 节点 A 的 vLLM → SSE 逐块回传 B。

**网络装机**（v0.1.48）：U 盘 iPXE → bootstrap.ipxe（输 IP/自动菜单，latest 滚动
策略）→ autoinstall 种子 → late-commands 拉安装脚本 → 装完**自动入集群闭环**；
ISO 从源节点 `/dist` 流式直传（Range 206 断点）；ISO 仓 sha256 官方对账，提取
casper 用**纯 Rust ISO9660 提取器**（免外部工具）。

---

## 5. 关键技术决策记录（ADR 式存档）

> 已有 8 个正式 ADR（COMPAT×3 + DEPS×5，见 [docs/adr/](docs/adr/)）。以下为
> 实施期沉淀的 15 条决策，每条含取舍；多为踩坑后定型，供后续演进与外部读者参考。

| # | 决策 | 取舍与理由 |
|---|------|-----------|
| 01 | **定位=连接 OS，存储是附带能力**（2026-08-20 纠偏） | 早期按「NAS/私有云数据中心」叙事立项目名与界面；纠偏后 NexOS=独立个体操作系统，核心方向 P2P 组网。影响全部界面文案（界面不得再出现 NAS）、文档口径与功能优先级。原立项的 NAS 全栈能力（ZFS/共享/HA）保留为底座。 |
| 02 | **「纯 Rust」的务实边界** | 管理面/编排/协议/P2P/网关纯 Rust；C 事实标准（ZFS/KVM/Samba/ffmpeg/chrony）编排不替代——重写数十年协议正确性沉淀不现实；应用级外部件（Blender/vLLM/vraminfo）探测式集成。备选「全自研」被否定：SMB Server 至今无成熟纯 Rust 实现即是例证。 |
| 03 | **Contract-First + mock-first + 真实测隔离** | trait+DTO+Error 先行（可独立 cargo check）；每 crate `mock` feature 内存实现供注入；真实 I/O 测试一律 `#[ignore]`+自动 teardown，默认套件零环境依赖。代价：双实现维护；收益：46 组件可并行开发、任何环境全绿。 |
| 04 | **单进程内嵌网关** | os-api 聚合全部 RouteHandler，不独立网关层（立项期 §9.1#10 决策的延续）。中间件链内建（限流/认证/审计），method 分桶+静态 HashMap O(1) 匹配。取舍：单进程故障域大——以 systemd 拉起+多节点联邦兜底，换取零内部 RPC、部署面极简（单端口四流量）。 |
| 05 | **网关契约下沉 os-common** | `HttpMethod/RouteSpec/RouteHandler` 下沉后，组件 crate（如 os-nexhub）**不绑 axum/网关实现**，经 os-api 装配层 blanket impl 桥接注册。收益：组件可独立抽离复用（os-nexhub 2026-08-15 实证）；代价：契约变更需双向同步。 |
| 06 | **git 子进程封装，不引 git2/gix 库绑定** | NexHub Smart HTTP = git-http-backend CGI 特挂；仓化/CI/克隆/大厅全走 git CLI 子进程（tokio::process）。理由：libgit2/gix 的 FFI/对象模型绑定对「包装既有 git 生态」收益低、维护重；CLI 面即 git 全能力。代价：解析文本输出——以稳定 plumbing 子命令规避。 |
| 07 | **传输双轨：b64 信封 vs 流式直传** | 网关恒 JSON，multipart 不可行——小文件（截图/凭证/工件）走 **base64 信封**（统一 JSON 契约）；大件（ISO/分发件/镜像）走 http.rs **特挂流式直传**（tokio unfold 256KiB 块 + Range 206 断点续传）。取舍：b64 有 4/3 膨胀，仅用于小件；「全部流式化」列入路线图。 |
| 08 | **探测式外部工具集成契约** | ffmpeg/Blender/vraminfo/geth/bitcoind 同款三态探测：env 覆写（`NEXOS_*_BIN`）→ PATH 扫描 → 常规落点；缺失返回 200 降级体附安装指引，不报错不告警。收益：可选能力零强依赖、离线/精简部署可跑；诚实降级优于假故障。 |
| 09 | **sudo 提权自适应 + 最小授权** | 需 root 的采集（vraminfo 读 /dev/mem）：root 跑则直接 exec；非 root 走 `sudo -n`（NOPASSWD **单命令白名单** `/etc/sudoers.d/`，仅授权该工具本体，不放大权限）；sudo 层失败（未装/未授权/要密码）回落直接 exec 并如实保留降级 note。关键判定：**sudo 前缀报错≠工具回执**——`no NVIDIA GPU found` 是真实结果，不回落。 |
| 10 | **SQLite 分库自治 + WAL + busy_timeout** | 每组件一库（rusqlite bundled 内嵌，无系统依赖），库文件不进 git；并发写统一 WAL + `busy_timeout` 惯例。代价：库数量膨胀（8+），**DB 收敛**已列入路线图。备选 PostgreSQL 被否定：单机自托管场景 SQLite 零运维。 |
| 11 | **引擎门控 → 剥离终态** | 演进三段：应用包运行时（引擎随 os-api 编译、应用按装启用，未装 404）→ 双应用剥离复制 → v0.1.51 三引擎删除（历史包装 400 拦截更诚实：装了也没有内置后端）。门控每请求直查 SQLite 零缓存（微秒级、装卸即时生效），表损坏 fail-closed。详见 §7。 |
| 12 | **应用包宿主桥：应用不打包 Vue/SDK** | 实测否决「打进包」方案：双 Vue 实例响应式失活 + i18n 抛错。改为 `window.__NEXOS_HOST__` 宿主桥（vue/vueI18n/api/sdk），构建期 vite 插件重写 import，应用开发者无感；`@nexos/app-sdk` 同款零打包。standalone 形态自带完整宿主（vue 打进宿主层，应用本体仍共享）。 |
| 13 | **存活检测单一来源** | 节点存活只由 os-p2p 节点元数据组件判定（心跳 5s/指纹验证/五振出局），其他组件一律从它取信息，不得自行探测。元数据交互必须携带 NodeID 指纹并经心跳验证才采信地址——防 gossip 谎报制造假 LAN 条目；127.0.0.1 回环无论怎么产生一律拒收（曾因 mDNS 回环广播引发注册表污染）。 |
| 14 | **可观测性纪律：拒收分支必须落日志** | 联邦 ingest 曾静默丢弃帧（「帧到≠消费」），三层排障后才闭环。定型为纪律：所有拒收/降级分支必须落日志；**中继拓扑端到端测试**（A/B 互不直连经锚点中继）成为联邦类功能的标准测试形态。 |
| 15 | **版本与发布纪律** | 版本只用纯三段 semver（旧节点解析器拒四段/预发布点段）；**bump 必须在 build 前**（CARGO_PKG_VERSION 是编译期常量，增量构建嵌旧值）；对外分发 release 必须从 `git archive <tag>` 纯净树构建（并行开发期工作树常含在途 WIP）；发版后分发件（双架构+web）必刷；环境变量一律 `NEXOS_` 前缀（`OS_` 后备兼容）。 |

---

## 6. 联邦与分布式设计

P2P 组网是 NexOS 的核心方向（对齐 §1 定位），设计全文见
[docs/NEXOS_P2P_NETWORK_DESIGN.md](docs/NEXOS_P2P_NETWORK_DESIGN.md)。

- **组网层（os-p2p）**：全分布式 Kademlia（Swarm 同款拓扑），不设中心注册表——公网
  锚点是「服务节点」不是中心；ECDH+AESGCM 加密链路；NAT 中继信箱 + TCP 打洞；
  mDNS LAN 种子冷启动。身份复用链上身份的 secp256k1 密钥（NodeID 即公钥指纹）。
- **身份层（os-identity）**：指纹账本独立组件——NodeID↔地址证据登记、四态归属判定、
  失配/冲突记账；传输层只发事实事件，账本与策略外移（架构
  [docs/IDENTITY_COMPONENT.md](docs/IDENTITY_COMPONENT.md)）。
- **节点自述协议（v0.1.45）**：node-meta gossip 捎带自述四字段
  （agent_version/hostname/platform/gpu），serde 兼容旧线格式零变化；联邦节点页显示
  版本徽章与待升级提示——集群侧版本感知的基石。
- **联邦大厅**：NexHub 项目大厅 / 模型大厅 / API 大厅三厅跨节点同步合并（publish→
  federate 两步语义，post-receive 钩自动联邦）；联邦条目内网 IP 遮蔽（admin 可展开）。
- **IM 联邦**：本地大厅 / 联邦大厅 / 远程节点大厅三会话；DM 点对点定向（非广播）；
  消息去重 + 断线补拉 + 在线新鲜度判定（订阅入向帧 touch，半开连接不误判在线）。
- **直播联邦（live，系统内保留）**：纯 Web 采集（MediaRecorder 切片→WS 上行→内存
  扇出→MSE 观看）；跨节点中继 1MiB 分块重组，帧字节级 md5 对拍验证；中途加入
  header 重放零缺帧、TTL 精确回收。
- **P2P 网状分发（transfer）**：五帧协议（query/offer/chunk/chunk_data/error）走
  加密 overlay，1MiB 分块逐块 sha256、坏块重试、块位图断点续传、背压控制、完成自动
  做种——「不经公网 IP 也能分发」。
- **WAN 出口共享（net-exit）**：出口节点声明 offer（digest 全网可学）→ 授权（TTL，
  默认拒绝）→ 使用方本地 SOCKS5 经 overlay 到出口节点出网；overlay 级实现不引内核
  侵入（调研 v2ray/Tailscale 后的自研轻量路径）。
- **跨网 API 中继**：联邦模型经 `via_node` 渠道中继消费（api_relay 分块协议，白名单
  封闭集 `{models,chat/completions}` 防开放代理）——NAT 后节点也能用上其他节点的 AI。
- **治理模型**：联邦大厅管理员=开发者（全局空间），本地大厅管理员=节点管理员；管理
  功能开发期暂缓（2026-08-23 定稿）。

---

## 7. 应用生态架构（运行时 → 门控 → 剥离）

应用生态是「os-api 保持系统职责纯粹」的关键机制，四阶段演进：

| 阶段 | 版本 | 机制 |
|------|------|------|
| 1. 应用包运行时 | v0.1.26（2026-09-04） | `apps` 组件：manifest 规范（id/版本/entry/min_os_api）、六端点（安装/卸载/目录/资产）、应用资产三道穿越闸；**引擎随 os-api 编译、应用按装启用**——未装应用的引擎端点 404 并提示去商店安装 |
| 2. standalone + SDK | v0.1.27–0.1.28 | 应用包自包含宿主（零 CDN 内网离线可跑）；`@nexos/app-sdk` 能力面 SDK（能力快照/联邦大厅/网关/本地 LLM/通知/降级三态），宿主桥双载体（ADR-12） |
| 3. 双应用剥离 | v0.1.30（2026-09-05） | qrtransfer/streaming 复制 film 模式剥离；引擎门控 + 前端零残留迁出；跨应用 API 消费方评估（门控只管 UI 引擎面） |
| 4. 三引擎剥离终态 | v0.1.51（2026-09-25） | film/streaming/surveillance 引擎**删除出 os-api**，由独立产品线 FilmStudio/StreamingStudio（各自 NexHub 仓）承接；历史 `nexos-app-*` 仓安装 400 拦截（`STRIPPED_ENGINES`，提示改用独立版）；live 联邦直播作为联邦基础能力常开保留 |

配套基建：

- **NexHub 内置 CI（v0.1.33）**：push 自动触发（receive-pack 精确路径钩），Cargo/npm
  流水线探测、同仓 FIFO 串行、环形日志实时刷库、四态徽章。
- **一键发布**：`tools/publish-app.sh` 六步（版本/构建/发布仓 rsync/commit+tag+push/
  触发 CI/安装）——应用迭代日常=改码+一条命令。
- **Webhooks（v0.1.50）**：GitHub 同构语义——push/issues/pr/release 四事件，
  HMAC-SHA256 签名头（`X-NexHub-Signature`），异步投递不阻塞业务，重试+投递环形日志。
- **MCP 配套端点（v0.1.50）**：coderepo 文件写入（git plumbing 落 commit，author=mcp）
  与代码搜索——AI agent 经 MCP 直接读写仓库。
- **NexHub CLI**：`curl | sh` 直装的 POSIX sh CLI（login/clone/apps deploy 等），
  token 不进 URL。
- 应用开发指南：[docs/APPS.md](docs/APPS.md)（12 节，film 为贯穿示例）；
  开发者中心（DevDocs 应用）实时渲染仓库 docs/。

---

## 8. 量化数字表（2026-09-25 实测）

| 维度 | 数值 | 口径/来源 |
|------|------|----------|
| workspace crate | **28**（26 业务 + os-integration + nettest） | 根 Cargo.toml `members` 逐项计数 |
| 当前版本 | **v0.1.51**（2026-09-25） | 根 Cargo.toml `workspace.package.version` |
| RouteHandler 组件 | **46 常驻 + 1 条件 extra** | `crates/os-api/src/main.rs` `register_component` 全量枚举（含 os-nexhub 桥接 2 个） |
| 静态 API 路由 | **377**（os-api 325 + os-nexhub 52） | grep `spec(` 剔除 `#[cfg(test)]` 模块后计数；另有 /git、/dist、/s/:appId、/ws/* 等网关特挂路由不在此口径 |
| 内置桌面应用 | **28**（appRegistry `builtinApps` 29 项含设置） | `crates/os-api/web/src/appRegistry.ts`；另有应用中心运行时可装应用包 |
| 测试 | os-api **1,715**（cargo test 基线）；全 workspace 测试函数 **约 5,000+** | os-api 为 2026-09-25 cargo test 运行数；workspace 为 grep `#[test]/#[tokio::test]` 属性计数（5,060） |
| Rust 代码量 | **约 31.6 万行**（含测试与注释） | `find crates -name "*.rs" | xargs wc -l` |
| 前端代码量 | **约 6.7 万行**（Vue/TS） | `crates/os-api/web/src` 下 .vue/.ts 行数 |
| 前端语言 | **4 语**（zh-CN/zh-TW/en-US/ja-JP）；后端 os-i18n 3 语（zh_cn/zh_tw/en） | `web/src/i18n/index.ts` SUPPORTED_LOCALES；`crates/os-i18n/locales/` |
| os-api 单 crate | **约 14.2 万行**（含内联测试）；最大 crate | wc 统计——「拆分」已列入路线图 |
| 运行时 SQLite | 8+ 分库（gateway/im/media/monitor/apps/tips/ci/hub_lobby/chain-nodes 等） | 各组件 env 指定路径 |
| 多节点实况 | 跨 LAN/NAT/公网多节点集群（x86/aarch64 混合 + 独立云锚点） | 集群节点注册表；一键安装支持双架构 |
| 工程质量 | clippy `-D warnings` 0 warning；CI fmt/clippy/test/前端构建全绿 | GitHub Actions（随仓发布） |

> 历史口径对照：2026-08-21 为 27 crate / 32 handler / 397 路由 / 4,375 测试
> （docs/ARCHITECTURE.md §8 快照）——此后 os-identity 加入、三引擎剥离、路由重排，
> 数字变化属正常演进，以本表为当前事实源。

---

## 9. 路线图

### 9.1 近期（下一批候选，2026-09-25 排队）

| 项 | 内容 | 动机 |
|----|------|------|
| **DB 收敛** | 8+ 分库治理：统一连接面/迁移/备份策略 | 分库自治的代价积累（ADR-10） |
| **流式化** | b64 信封（ADR-07）向流式直传推广：大文件上传/下载通道泛化 | 消除 4/3 膨胀与内存峰值；ISO 直传模式已验证 |
| **统一任务框架** | 各组件任务中心/环形日志/轮询模式泛化为统一框架 | 任务观测面重复实现（CI/推理环境/装机/影片管线各自一套） |
| **i18n 对齐** | 后端三语与前端四语对齐；应用包四语言覆盖补齐 | 日文仅前端；应用包 i18n 键×4 已成惯例 |
| **os-api 拆分** | 14 万行巨石按 os-nexhub 模式继续抽离组件 crate | 编译时长/并行协作/职责纯粹 |
| 遗留修复 | Files API 同名上传静默加 -1 副本隐患；p2p 同网段直连优先路由；transfer 二进制帧化；多源并行拉取 | 均为实测发现的具体债 |

### 9.2 中期（规划）

- **手机客户端**（Flutter，含 BLE mesh 中继接入）——调研完成未开发。
- **联邦治理功能**：大厅管理员面、PR 审核流、发版权限（治理模型已定稿，功能暂缓）。
- **安全台账清偿**：RDP 转发认证面、链上身份私钥存储加固（台账见
  docs/FEATURE_SURVEY_2026-08-20.md；部分项用户指示先记录不处理）。
- `/tank` 等硬编码路径与 env 收敛。

### 9.3 远期研究（自研/前沿，承接立项期 P10）

自研 SMB Server / NFSv4（视 Rust 生态）、多主同步（CRDT/Syncthing 思路）、Ceph 共享
存储与 VM 实时迁移、本地大模型推理常态化、更多链接入 os-wallet（ChainAdapter 插件位）。

### 9.4 暂缓项（用户明确指示，勿主动重启）

GitHub 常态同步（仅明说时推）、LLM 实例持久化（手动拉起即可）、生产强 token 更换、
ISO 完整构建追投、release 构建常态化。

---

## 10. 文档地图

| 文档 | 内容 |
|------|------|
| [PHILOSOPHY.md](PHILOSOPHY.md) | 理念（连接 OS，五类孤岛） |
| [README.md](README.md) | 项目门面：特性/快速开始/构建 |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | 架构详解：crate 分层/数据流/测试架构 |
| [docs/README.md](docs/README.md) | docs 总索引（功能文档速览，AI agent 协作入口） |
| [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) | 部署全流程 |
| [docs/APPS.md](docs/APPS.md) | 应用包开发指南（运行时/门控/剥离终态） |
| [docs/NEXOS_P2P_NETWORK_DESIGN.md](docs/NEXOS_P2P_NETWORK_DESIGN.md) | P2P 组网设计 |
| [docs/adr/](docs/adr/) | 8 个正式 ADR（COMPAT/DEPS） |
| 其余功能文档 | 见 docs/README.md 索引（每功能 MD 含端点契约/env 清单/拓扑图——功能文档同步铁律） |

---

*NexOS — Connecting the Islands.*
