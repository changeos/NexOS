# CHANGELOG

> OS 系统（Rust workspace）变更记录。
>
> 项目采用契约驱动、分批合并的开发模型，**尚未正式发版**，因此本文件按**开发阶段（批次/里程碑）**而非语义化版本号组织。各阶段按时间**倒序**排列（最新在最上）。
>
> 遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 格式。变更归类：
>
> - **Added** —— 新功能 / 新模块 / 新真实接通
> - **Changed** —— 重大改动 / 重构 / 架构调整
> - **Fixed** —— bug 修复（本仓库累计 12 个由真实环境验证发现并修复的 bug）
>
> 里程碑详细记录见 [`docs/HANDOVER.md`](./docs/HANDOVER.md) §4（全程里程碑表）与 [`docs/PROGRESS.md`](./docs/PROGRESS.md)（各批次详细记录）。所有真实环境测均标 `#[ignore]` 且带自动 teardown，默认不污染测试套件。

**目录（按阶段倒序）**

- [\[Unreleased\]](#unreleased)
- [阶段 20 — 产品化：IM 分布式 + Vue3 桌面 + NexHub 大厅 + 网关变现 + 链上身份（2026-08-07 ~ 08-20）](#阶段-20--产品化im-分布式--vue3-桌面--nexhub-大厅--网关变现--链上身份2026-08-07--08-20)
- [阶段 19 — 第六轮：DPU / zpool 解析 + SMB-NFS 落盘接通 + 覆盖率基线（batch8）](#阶段-19--第六轮dpu--zpool-解析--smb-nfs-落盘接通--覆盖率基线batch8)
- [阶段 18 — 第五轮：PXE/replication/backup 真实测 + clippy pedantic + bench CI（batch7）](#阶段-18--第五轮pxereplicationbackup-真实测--clippy-pedantic--bench-cibatch7)
- [阶段 17 — 第四轮：NFS/bootloader/RustFS-sigv4 + 接通远端 git-clone / osd-systemd（batch6）](#阶段-17--第四轮nfsbootloaderrustfs-sigv4--接通远端-git-clone--osd-systemdbatch6)
- [阶段 16 — 第三轮：装环境解锁 ffmpeg/libvirt VM/runc/SMB/LIO + CPU 虚拟化检测（batch5）](#阶段-16--第三轮装环境解锁-ffmpeglibvirt-vmruncsmblio--cpu-虚拟化检测batch5)
- [阶段 15 — 第二轮：osd + storage block + guest nftnl-ffi 五项（batch4）](#阶段-15--第二轮osd--storage-block--guest-nftnl-ffi-五项batch4)
- [阶段 14 — 第一轮：五大执行层本机实跑（zfs/nftnl/netlink/iso/CLIP-CUDA）（batch3）](#阶段-14--第一轮五大执行层本机实跑zfsnftnlnetlinkisoclip-cudabatch3)
- [阶段 13 — 工程化加固：CI / 性能基线 / TODO 审计 / 真实 ISO CI](#阶段-13--工程化加固ci--性能基线--todo-审计--真实-iso-ci)
- [阶段 12 — 真实接通深化：iso xorriso + CLIP ADR + 集成场景 9/10 + fmt](#阶段-12--真实接通深化iso-xorriso--clip-adr--集成场景-910--fmt)
- [阶段 11 — review2 全闭环 + 文档收尾（5 合并）](#阶段-11--review2-全闭环--文档收尾5-合并)
- [阶段 10 — 收尾 5 批：osd NTP / provision PXE / update bootloader / cli / CommandOutput](#阶段-10--收尾-5-批osd-ntp--provision-pxe--update-bootloader--cli--commandoutput)
- [阶段 9 — 真实执行层整合：storage ZFS / compute pkg / network FFI / services FFmpeg-CLIP](#阶段-9--真实执行层整合storage-zfs--compute-pkg--network-ffi--services-ffmpeg-clip)
- [阶段 8 — 沙箱方案 + 文档：SANDBOX / DEPLOYMENT / REVIEW 第二轮](#阶段-8--沙箱方案--文档sandbox--deployment--review-第二轮)
- [阶段 7 — 跨 crate 集成测 + 网络冒烟（os-integration / nettest）](#阶段-7--跨-crate-集成测--网络冒烟os-integration--nettest)
- [阶段 6 — 基准 + BIP-322 + dyn 兼容 + radix 路由优化](#阶段-6--基准--bip-322--dyn-兼容--radix-路由优化)
- [阶段 5 — P3 真实集成：AEAD（AES-256-GCM）+ ACME 自动证书](#阶段-5--p3-真实集成aeadaes-256-gcm--acme-自动证书)
- [阶段 4 — P2 真实集成 wave2：i18n / virt / cgroups / gix / mdns+mTLS](#阶段-4--p2-真实集成-wave2i18n--virt--cgroups--gix--mdnsmtls)
- [阶段 3 — P2 真实集成 wave1：openraft / rusqlite / dav-server / libunftp / russh 等](#阶段-3--p2-真实集成-wave1openraft--rusqlite--dav-server--libunftp--russh-等)
- [阶段 2 — P0/P1 接通：reqwest/axum/tower/jwt/argon2/ed25519/nftnl/rtnetlink/tantivy](#阶段-2--p0p1-接通reqwestaxumtowerjwtargon2ed25519nftnlrtnetlinktantivy)
- [阶段 1 — 批 0–4 骨架：27 owner agent 写实现骨架（数据结构/状态机/Mock）](#阶段-1--批-04-骨架27-owner-agent-写实现骨架数据结构状态机mock)
- [阶段 0 — 契约编译：22 crate cargo check 全绿](#阶段-0--契约编译22-crate-cargo-check-全绿)

---

## \[Unreleased\]

原 2026-08-06 批次（os-mcp + batch9 覆盖率补测）的增量记录；**2026-08-07 之后的产品化增量已归档为下方「阶段 20」**。

### Added

- **os-mcp crate：OS MCP Server**（feature/os-mcp-server）：把 os-api HTTP 网关暴露为 MCP（Model Context Protocol）tools，让支持 MCP 的 AI 助手（Claude / ChatGPT 等）经 stdio JSON-RPC 2.0 管理 OS。10 个无参只读 GET tool（`os_pool_list` / `os_dataset_list` / `os_snapshot_list` / `os_vm_list` / `os_share_list` / `os_user_list` / `os_node_list` / `os_status` / `os_virt_check` / `os_health`），表驱动注册；`tools/call` 内部用 reqwest GET 对应 os-api 路由，返回真实数据 JSON。支持 `initialize` / `tools/list` / `tools/call` / `ping` 方法 + 标准 JSON-RPC error code（-32700/-32600/-32601/-32602/-32603）。手写最小 JSON-RPC over stdio（协议层与 IO 层解耦，纯函数可单测，30 测全过）；rmcp 官方 SDK 注册到 workspace deps 作可选 transport（`rmcp-transport` feature）。端到端验证：`echo tools/call os_pool_list | os-mcp` → 真实 zfs 池 `osprobepersist`。
- **测试覆盖率补测 +226 测**（batch9）：os-core DTO/newtype（0%→~100%）、各 crate `error.rs` Display（22 crate/23 测/166 断言，0%→全覆盖）、os-protocols（sigv4 边界 + FTP/SFTP 后端 + orchestrator 错误路径，+88 测）、os-network（DHCP/DNS/防火墙/DPU/RDMA 分支，+65 测）、os-core（types/ids/error Display，+34 测）。整体覆盖率由 **79.6% → ~85%**。
- **cargo-deny 供应链策略落档**（batch9）：`deny.toml` licenses 检查通过；bans 重复依赖警告登记到 `docs/CODE_QUALITY_AUDIT.md` §5/§6；udeps（dead code）检测因 nightly 工具链损坏待补。
- **cargo-audit 供应链漏洞审计真实结果**（batch8-supplement）：4 漏洞 + 5 警告，**无高危**——rsa Marvin Attack / mnl segfault / nftnl heap overflow / rkyv OOB；清除损坏的 advisory-db 解决了网络问题。

### Changed

- **13 处 `ErrorKind::Other` → `Error::other`**（batch9）：clippy lint 驱动，零行为变更。

### 统计

- commits：~13（`d0241c1..3dad4a2`）
- crate：25（+1 os-mcp）
- 测试：默认套件稳定增长，覆盖率 ~85%

---

## 阶段 20 — 产品化：IM 分布式 + Vue3 桌面 + NexHub 大厅 + 网关变现 + 链上身份（2026-08-07 ~ 08-20）

> 2026-08-07 ~ 08-20。项目从"crate 骨架全绿"转入**可日用产品**冲刺：25→26 crate、Web 桌面成型
> （30 应用）、NexHub 代码大厅上线、AI 网关变现通道、链上身份贯通。补录自 git log（583 commits 总量）。

### Added（2026-08-07 ~ 08-14 摘要）

- **IM 分布式子系统**：P2P TCP 传输 + 群组管理 + Federation 跨节点互联（握手状态机/信任模型）+ REST API + Chat.vue（SQLite 持久化 + WebSocket 实时推送）。
- **Vue3 Web 桌面**：Synology DSM 风桌面首页 + 存储页（创建池/数据集/快照/健康徽章）+ 磁盘检测 API（`GET /api/v1/disks`，lsblk 过滤系统盘）+ 创建池向导（数据/元数据/日志/缓存四区互斥）+ SPA 路由 fallback 修复。
- **ISO 安装包**：`make iso` + Rust 安装器。
- **NexHub 双通道 git**：SSH + HTTP Smart Git（git-http-backend CGI，Bearer/Basic token，路径穿越防护）；NexHub 大厅（发布/浏览/搜索/一键克隆 + GUI，0b866e4）。
- **技术栈 Rust 化**：curl→reqwest（once_cell 共享 Client）、QR 纯 Rust（qrcode/rqrr）、钱包 k256+tiny-keccak（修 EVM 派生 0x04 前缀 bug）。
- **AI 壁纸**：sd-turbo 生成 4 张壁纸 + useWallpaper.ts 图片壁纸支持。
- **GitHub 镜像**：changeos/NexOS（私有）+ CI 四轮攻坚全绿；bench 改仅手动触发（用户决策）。

### Added（2026-08-15 ~ 08-20，本阶段重点）

- **NexHub 货币化 + 悬赏 bounty**（c46d503 / 8a24c14 / d5b31e2，08-15）：免费/虚拟货币 BTC 付费门禁 + bounty 生命周期 `open→claimed→submitted→paid`（出资求活）；审查修复旧库列迁移、clippy 清零、悬赏认领竞态原子化（9a14388）；中文查询参数损坏修复（url_decode 按字节累积，8676a09）。
- **os-nexhub 抽离独立 crate**（b4647f1，08-15）：code_repo + nexhub_lobby 两大 RouteHandler 从 os-api 抽离（审计 COMPONENT_INDEPENDENCE_AUDIT §6），26/26 crate 发布元数据补齐；仓库路径迁移 NAS_System → `/home/oem/NexOS`。
- **网关 AI 变现 Phase 1**（2aa6bce + fbbff08，08-17）：token 四计费模式（free/per_token/per_image/credits）+ USDT/BTC/EVM 充值订单（价目常量/契约表/env，docs/GATEWAY_MONETIZATION.md）+ 前端计费模式选择与充值订单页。
- **远程转发全栈**（cb7f7c9 + ce7b18c，08-17）：SSH 隧道 -L/-R/-D 三模式（spawn 系统 ssh，密钥认证禁密码交互）+ RDP 纯 Rust TCP 代理（copy_bidirectional）+ .rdp 文件生成；定义 SQLite 持久化 + autostart 启动恢复；前端 SSH/RDP 双 Tab + 桌面图标（docs/FORWARDING.md）。
- **IM 区块链认证**（844bbcc，08-18）：身份 = secp256k1 压缩公钥，挑战-签名三步认证，WS 握手强制验 token，自报身份全删（用户决策 2026-08-17）；前端 @noble 密钥卡（docs/IM_BLOCKCHAIN_AUTH_DESIGN.md）。
- **存储/文件链路（迅雷场景）**（e14f211 + 235600e，08-18）：Files 目录浏览（面包屑/用量统计/三态体验）+ FileBrowser 组件化（Files/Storage 两页复用）+ SMB 共享 nexos-downloads 运维链路（smb.conf/avahi 品牌统一 NexOS，docs/STORAGE_SHARING.md）。
- **媒体生成 API**（b96affd + 096bc07，08-18）：POST /media/image 本地 sd-turbo 真实生图（默认 768×432/4 步；显存 <6G 时 503 提示先停推理实例；宽高须 64 倍数 256-1024）+ /media/video 任务框架（可插 local/external 后端，未配明确报错）+ 模型管理页「生成」Tab（docs/MEDIA_GEN_AND_CHAIN_AUTH.md）。
- **NexHub 链上身份与权限**（b96affd + 096bc07，08-18）：`os-common::chain_auth` 共享内核（IM 同款三步，密钥对全平台通用）；大厅 publish owner=pubkey、重发布/下架仅本人、bounty poster/hunter 服务端反查（修身份自报漏洞）、purchase 改 owner 公钥匹配；前端 useChainIdentity.ts。
- **NexHub 外部 agent 接入闭环**（e6114d4 + 4107900，08-18）：默认分支坑修复（建仓 HEAD→main + 快照回退探测 main→master）；接入指南 docs/NEXHUB_ONBOARDING.md（finalshell-rs 三步上架实测）。
- **vLLM 轻量监控**（0db05da，08-20）：按需抓 vLLM /metrics（5s 缓存/3s 超时），Counter 差值算速率，不可达 200+null；`NEXOS_LLM_METRICS_SIMULATE` 模拟模式（默认关）；模型管理监控 Tab（docs/LLM_MONITORING.md）。

### Fixed

- CI 双失败修复（df4adbf）：补前端构建 + 全仓 fmt + 33 处 rustdoc；runner 磁盘耗尽与 Pages 限制（96ebca5）；bench workflow 重复 env 键被 GitHub 严格解析拒绝（5dca45f）；files list_root 受限环境（无 /tank 且 /var/lib/os 不可写）跳过前置（a8941dd）。
- UI 体验三项（6cb16a0）：模型对话并入模型管理 / 充值占位警示 / 远程转发窗口化。

### 统计

- crate：25 → **26**（+os-nexhub；另有 os-web 旧前端存档非 crate）
- 测试：3,624（08-07）→ 3,949 → **4,100+**（08-18 起）
- 前端：31 个 view / 30 个桌面应用 / 约 330 条 API 路由（08-20 口径）
- commits：约 +210（`3dad4a2..5775275` 段，总 583）

---

## 阶段 19 — 第六轮：DPU / zpool 解析 + SMB-NFS 落盘接通 + 覆盖率基线（batch8）

> 2026-08-06。main `4a960bf`，**2229 测试通过**（2110 passed + 119 ignored）。里程碑对应 HANDOVER §4 第 19 项。

### Added

- **DPU devlink/rdma 命令构造 + 解析器强化**（os-network）：`devlink_dev_show_argv` / `parse_rdma_dev` 等纯函数；7 默认测 + 3 `#[ignore]` 真实工具可达性测（本机 iproute2-6.19.0）。
- **zpool status 树形解析补全**（os-storage）：`parse_zpool_status` 三段式解析 + Vdev 扩展 read/write/cksum 错误计数字段 + `list_pools_with_vdevs` inherent 方法。6 默认测（单盘/mirror/raidz1/DEGRADED+错误聚合/多池/容错）+ 3 真实测（`osprobepersist` 池 vdev 解析正确）。
- **SMB/NFS 真实落盘 + reload 接通**（os-protocols）：`ReloadPolicy`（Enabled/DryRun/Disabled）+ `write_smb_conf` 真实 tokio::fs::write + `exportfs -i` 往返 + `smbcontrol -s reload`。4 `#[ignore]` 真实测。
- **测试覆盖率分析 79.6%**（首次）：cargo-tarpaulin 0.37.0，18 crate 跑通，结果落档 `docs/COVERAGE_REPORT.md`。

### Fixed

- **SMB unexport clients 快照 bug**：导出回滚未保留 clients 字段；同步适配 batch5/6 回归。

### 统计

- commits：~11（`1c65557..d0241c1`）
- crate：24
- 测试：2205 → **2229**（2110 passed + 119 ignored）

---

## 阶段 18 — 第五轮：PXE/replication/backup 真实测 + clippy pedantic + bench CI（batch7）

> 2026-08-06。main `7157d23`，**2205 测试通过**。里程碑对应 HANDOVER §4 第 18 项。

### Added

- **PXE iPXE/pxelinux.cfg 模板生成验证**（os-provision）：+ dnsmasq PXE 配置 `--test` 语法校验；6 默认测 + 3 `#[ignore]` 真实测（本机 dnsmasq 2.92）。
- **zfs send-recv 命令构造 + 本地真实全量/增量/加密往返**（os-storage）：6 默认测 + 4 `#[ignore]` 真实测（send|recv 往返 / send→file→recv 47400B / 增量 / 加密 passphrase unload/load-key，本机 zfs 2.4.1）。
- **backup 本地快照验证 + scrub 查询解析 + send 流回放**（os-services）：17 默认测 + 3 `#[ignore]` 真实测（trigger_now 快照 / scrub `zpool status` / send 50984B recv 回放）。
- **clippy pedantic 全量审计**：扫描 3304 warning，修 25 文件高价值 lint（explicit_iter_loop / single_char_add_str / redundant_closure），审计报告 `docs/CODE_QUALITY_AUDIT.md`。
- **bench 回归 CI 阈值门控**：criterion `--baseline` 比对 + artifact 存储 + `scripts/ci/bench-regression-gate.sh` 分桶阈值（strict 15% / loose 30%）。

### Changed

- 默认 clippy 保持 **0 warning**，测试零回归。

### 统计

- commits：~11（`356f3fa..1c65557`）
- crate：24
- 测试：2166 → **2205**

---

## 阶段 17 — 第四轮：NFS/bootloader/RustFS-sigv4 + 接通远端 git-clone / osd-systemd（batch6）

> 2026-08-06。main `7410560`，**2166 测试通过**。里程碑对应 HANDOVER §4 第 17 项。

### Added

- **NFS exports/ganesha.conf 渲染语法验证**（os-protocols）：10 默认测 + 4 `#[ignore]` 真实可达性测（exportfs / ganesha 6.5）。
- **GRUB/systemd-boot 配置生成 + 命令构造**（os-update）：14 默认测 + 5 `#[ignore]` 真实可达性测（本机 grub-reboot 2.14 / systemd 259）。
- **RustFS/S3 sigv4 签名 AWS 官方测试向量验证 + 真实 S3 HTTP**（os-protocols）：6 默认测（canonical_request/string_to_sign/signing_key 与 AWS `aws-sig-v4-test-suite` 完全一致）+ 3 `#[ignore]` 真实测（匿名 GET noaa-goes16 + ListObjectsV2 + sigv4 签名 GET）。
- **接通远端 git clone**（os-services devtools）：gix `blocking-network-client`，新 `git-remote` feature 门控 reqwest-rust-tls 后端；真实 clone octocat/Hello-World `#[ignore]` 跑通；移除 **3 个 RUNTIME TODO**。
- **接通 osd do_start/stop_inner 真实 systemctl**（osd）：引入 `SystemdRunner` 双后端 trait（`TokioSystemdRunner` 真实 systemctl + `InMemorySystemdRunner` no-op 向后兼容）；5 `#[ignore]` 真实测跑绿，batch4 锚点测同步更新。**零 bug**，不破坏 30+ 现有测。

### Fixed

- **sigv4 头部值内部空格折叠 bug**（密码学正确性）：`canonical_request` 只 trim 头部值未折叠内部连续空格，AWS sigv4 规范要求 `a   b` → `a b`。用 AWS `get-header-value-trim` 测试向量验证修复。该 bug 会导致 S3 请求因签名错误被拒。

### 统计

- commits：~11（`c9d3e13..356f3fa`）
- crate：24
- 测试：2115 → **2166**（2067 passed + 99 ignored）

---

## 阶段 16 — 第三轮：装环境解锁 ffmpeg/libvirt VM/runc/SMB/LIO + CPU 虚拟化检测（batch5）

> 2026-08-06。main `62e8b23`，**2115 测试通过**。里程碑对应 HANDOVER §4 第 16 项。前置：装 ffmpeg 8.0.1 / libvirt-dev 12.0.0 / samba 4.23.6 / nfs-ganesha 6.5 / targetcli / runc 1.4.0 + 加载 nvmet。

### Added

- **ffmpeg 真实转码**（os-services）：HLS 单/多档位 ABR，4 `#[ignore]` 真实测 + 2 纯逻辑测，本机 ffmpeg 8.0.1。零实现 bug。
- **virt-ffi feature 首次真实编译**（os-compute）：libvirt 12.0.0，编译一次过；4 `#[ignore]` 真实测（`test:///default` fixture VM define/create/suspend/resume/destroy 生命周期）。
- **runc 真实容器拉起**（os-compute）：OCI bundle create→start→delete 往返 5 `#[ignore]` 测，本机 runc 1.4.0。
- **SMB smb.conf 渲染语法验证**（os-protocols）：6 默认测（testparm 真实校验）+ 4 `#[ignore]` 真实测，本机 smbd 4.23.6。
- **LIO iSCSI + nvmet NVMe-oF 真实 configfs export**（os-storage）：4 `#[ignore]` 真实测，本机 target_core_mod + nvmet 跑通。
- **CPU 虚拟化能力检测模块**（os-compute，**用户需求**）：VM 启动前预检 CPU vmx/svm flags + /dev/kvm + kvm 模块；`HardwareVirtualizationUnavailable` 错误 + 用户友好诊断（"请在 BIOS 开启 VT-x，执行 `sudo modprobe kvm_intel`"）+ `preflight_virt_check`。18 纯逻辑测 + 3 真实测，本机 Intel Ultra 5 vmx+kvm 跑通。

### Fixed

- **runc 容器创建生产挂起 bug**：`YoukiRunner::run` 用 `output().await` 等管道 EOF，runc init 后台进程继承管道写端长驻 → EOF 永不到达 → 容器创建永久 hang。改为 `spawn` + `wait` + 限时排空管道。（只有真实拉起容器才能发现，runc 特有行为。）
- **LIO backstore 名含 `/` 被 targetcli 拒收**：新增 `sanitize_name`。
- **LIO portals 重复创建**：去重判断。
- **LIO IQN 后缀转义**：后缀处理修正。（storage LIO 子代理修 3 个真测才暴露的 bug。）

### 统计

- commits：~8
- crate：24
- 测试：2069 → **2115**（2033 passed + 82 ignored）

---

## 阶段 15 — 第二轮：osd + storage block + guest nftnl-ffi 五项（batch4）

> 2026-08-06。main `05eb69a`，**2069 测试通过**。里程碑对应 HANDOVER §4 第 15 项。

### Added

- **osd cgroup v2 真实写入**（`CgroupsRsBackend`）：4 `#[ignore]` 测（apply/read/update/delete），本机 cgroup2fs + root 跑绿。
- **osd systemd 可达性 + transient unit**：4 `#[ignore]` 测，本机 systemd 259 跑绿（确认当时 start/stop 仍纯状态机框架，留接通锚点）。
- **osd chrony 真实编排**（`ChronyNtp`）：4 `#[ignore]` 测（chronyc tracking/sources 真实解析 / dry-run / 探测），本机 chrony 4.8 跑绿。
- **storage block LIO/nvmet 命令构造**（默认跑绿）+ 3 configfs 可达性 `#[ignore]` 测（本机无 LIO/nvmet，优雅 SKIP）。
- **os-guest nftnl-ffi 真实落地**：`nftnl_apply_statements` 从占位 `Err` 换真实实现（nftnl 0.7 `Batch` + 自定义 `SetElemMsg` + `nft_expr!` + `mnl::Socket` 提交内核）；3 `#[ignore]` 测，本机 libnftnl 1.3.1 + libmnl 1.0.5 跑绿。

### Fixed

- **storage `export_iscsi` 缺 backstore 创建**：引用未创建的 backstore + 语法错。
- **storage `unexport` 非真逆操作**：不删 backstore，增强 `UnexportKind::Iscsi` 携带 backstore 名。
- **guest `nft_expr!` payload 宏 3-token 误用**：nftnl 0.7 只接 2-token。
- **guest dport cmp 字节序 bug**：nftnl `ToSlice for u16` 写 little-endian，dport 是 big-endian，端口 445 被错写为 48385 → 改传 `u16::to_be_bytes()`。（与 batch3 os-network 字节序陷阱同类。）

### 统计

- commits：~7
- crate：24
- 测试：2048 → **2069**（2007 passed + 62 ignored）

---

## 阶段 14 — 第一轮：五大执行层本机实跑（zfs/nftnl/netlink/iso/CLIP-CUDA）（batch3）

> 2026-08-05。main `74af95e`，**2048 测试通过**。里程碑对应 HANDOVER §4 第 14 项。

### Added

- **真实 zfs 池全链**（os-storage）：sparse file vdev 建池 → dataset → snapshot → destroy，自动 teardown（RAII Drop + 同步 `zpool destroy -f` 避免嵌套 tokio runtime panic），zfs-2.4.1。2 `#[ignore]` 测。
- **nettest rtnetlink/genl 真实网络**：从 1 扩到 6 测（link/addr/route dump / link_get_by_name / genetlink 25 family / dummy CRUD 经 rtnetlink 而非 ip 命令）。
- **nettest nftnl 真实 nft 事务**：表/链/规则提交 + `nft list` 验证。
- **ISO 多架构真实构建**（os-iso）：minimal/BIOS/BIOS+UEFI/UEFI-only 四产物，深度验证 ls/+/file/El Torito/sha256。
- **CLIP CUDA 真实推理跑通**（os-services，RTX 3090）：`CandleClipModel` 从骨架升级为真实实现（candle_transformers::clip::ClipModel 经 mmap safetensors 加载），feature 门 `clip-cuda`。权重经 hf-mirror 下载 605MB；稳态 embed_image 99ms / embed_text 16ms，语义排序正确（cat-kitten 0.94 > cat-car 0.90）。

### Fixed

- **nftnl FFI 路径 bug**（os-network `nftnl_real.rs`）：`nftnl::nftnl_sys::mnl_socket` 路径在 nftnl 0.7 不存在 → 改用 `mnl` crate 高层 API；修 19 编译错误（nftnl 0.6→0.7 API 漂移）。
- **nftnl 0.6→0.7 字节序陷阱**：`nft_expr!(cmp == 22u16)` 小端写但 TCP 端口大端读 → 改传 `22u16.to_be_bytes()`。
- **iso cli.rs 选项名 bug**：`-boot-info` → `-boot-info-table`（xorriso 1.5.6 拒收错误选项名）。

### 统计

- commits：~5
- crate：24
- 测试：2040 → **2048**

---

## 阶段 13 — 工程化加固：CI / 性能基线 / TODO 审计 / 真实 ISO CI

> 2026-08-05。main `245abfe`，**2040 测试通过**。里程碑对应 HANDOVER §4 第 13 项。

### Added

- **CI 加固**（`.github/workflows/`）：ci.yml 加 `cargo fmt --all -- --check` 门（最便宜、最先跑）+ `bench` job（criterion，手动/main-only 不阻塞 PR）+ `iso-build` job（workflow_dispatch）；docs.yml 补 Pages deploy。Makefile 加 `bench` / `bench-pkg PKG=` / `bench-save TAG=` 三目标。
- **5 crate criterion bench 跑通 + 性能基线**：`docs/PERFORMANCE_BASELINE.md`（硬件/日期/HEAD/每 bench mean+CI）。关键发现：routing `hit_static` O(1) 23-30ns、topo 近线性 O(V+E)、tantivy 建索引方差大（建议 >30% 回归阈值）。
- **高 TODO crate 审计**（services/protocols/network，95 条）：73 RUNTIME 保留 + 13 DOC + 8 OBSOLETE 清理（旧 tantivy/gix 已接通 TODO），审计报告 `docs/TODO_AUDIT.md`。
- **nettest 真实冒烟扩展**：+3 `#[ignore]` 真实测（zfs 二进制+内核模块 / rtnetlink link_list 本机实测过 / nftnl 事务，feature 门 `nftnl-ffi`）；注册 `mnl` crate。
- **os-iso 真实 xorriso ISO 构建 CI**：`IsoEnvironment::probe()` 探测 xorriso/mksquashfs/sha256sum；真实端到端 `#[ignore]` 测本机跑通 380928 字节 + CD001 魔数校验；`scripts/sandbox/docker/Dockerfile.iso`。

### Changed

- **2 处 STUB 补真实实现**：`hash_password` FNV → PBKDF2-SHA256 拉伸；`delete_by_path` no-op → tantivy raw 分词索引 + 真实 Term 删除。

### 统计

- commits：~5
- crate：24
- 测试：2024 → **2040**

---

## 阶段 12 — 真实接通深化：iso xorriso + CLIP ADR + 集成场景 9/10 + fmt

> 2026-08-05。main `ed165a9`，**2024 测试通过**。里程碑对应 HANDOVER §4 第 12 项。

### Added

- **os-iso xorriso 真实构建执行层**：`IsoBuildRunner` trait（`#[async_trait]` dyn 兼容，与 `os-storage::CommandRunner` 同构）+ `TokioIsoRunner`（tokio::process 真实 spawn）+ `FixtureIsoRunner`（确定性测试产物）；`XorrisoIsoBuilder.build` 三阶段（mksquashfs → xorriso → sha256）接通 runner。+18 测。
- **CLIP 推理后端选型 ADR-DEPS-005**：选 candle 0.11（纯 Rust + CUDA，适配 RTX 3090）；workspace 注册 candle-core/nn/transformers；`CandleClipModel` 骨架 + `PlaceholderClipModel`（无 GPU fallback）。+8 测。
- **集成场景 9/10**（os-integration）：9 provision PXE 自举链路（PhaseMachine 4 阶段 / S3.19 ExcludeRules / CheckpointPolicy / PxeConfigBuilder 三 BootMode）；10 osd 启动编排链路（拓扑排序 / 循环检测 / InMemoryCgroupBackend 配额）。+60 测。
- **全局 `cargo fmt`**（191 文件）+ clippy `-D warnings` 0 warning。
- **ERROR_GUIDE P3 错误码复审**：10 处逐个复审，1 修 + 9 标注保留，符合率 94% → **95%**。

### 统计

- crate：24
- 测试：1935 → **2024**（1994 passed + 30 ignored）

---

## 阶段 11 — review2 全闭环 + 文档收尾（5 合并）

> 2026-08-05。main `4eb29cb`，**1935 测试通过**。里程碑对应 HANDOVER §4 第 11 项。

### Added

- **ERROR_GUIDE 错误码归类指引**（`docs/ERROR_GUIDE.md`，P-R2-4 闭环）。
- **os-common 补 13 测** + **os-compute mock 归并**（删 `mock_vm.rs`，P-R2-3 + P-R2-2 闭环）。
- **os-compute youki 容器运行时编排层骨架**（`runtime.rs`，trait + 命令构造）。
- **os-integration +3 场景**（API 聚合 / discover mTLS 联邦 / update 回滚，5→8 场景）。
- **os-services tracing 日志桥接**（`LogBridgeLayer`，tracing-subscriber，P-R2 review 遗留 TODO 闭环）。

### 统计

- crate：24
- 测试：1838 → **1935**

---

## 阶段 10 — 收尾 5 批：osd NTP / provision PXE / update bootloader / cli / CommandOutput

> 2026-08-05。main `c004231`，**1838 测试通过**。里程碑对应 HANDOVER §4 第 10 项。

### Added

- **osd chrony NTP 真实编排**（`ntp_impl.rs` 996 行）。
- **os-provision PXE 引导配置 + 初始化脚本编排**（`pxe.rs` / `init_script.rs` / `transfer.rs`）。
- **os-update bootloader A/B 槽位真实激活**（GRUB/systemd-boot，`bootloader.rs` 986 行）。
- **os-cli clap 接入 + 运维子命令骨架**（`cli.rs` 893 行）。

### Changed

- **os-core 统一 `CommandOutput` 类型**：消除 compute/storage/services 3 处重复，纯重构。

### 统计

- crate：24
- 测试：→ **1838**

---

## 阶段 9 — 真实执行层整合：storage ZFS / compute pkg / network FFI / services FFmpeg-CLIP

> 2026-08-05。里程碑对应 HANDOVER §4 第 9 项。

### Added

- **storage ZFS CLI 真实执行层**（`TokioCommandRunner`，`backend_impl.rs` 488 行真实解析）。
- **compute apt 真实执行 + OCI/CNI 落盘 + desktop**。
- **network rtnetlink/nftnl 真实执行层**（`backend.rs` 697 行 + `nftnl_real.rs` 322 行，FFI 门控）。
- **services FFmpeg 转码编排 + CLIP 接口骨架**。

### 统计

- crate：24
- 测试：+real exec

---

## 阶段 8 — 沙箱方案 + 文档：SANDBOX / DEPLOYMENT / REVIEW 第二轮

> 里程碑对应 HANDOVER §4 第 8 项。

### Added

- **沙箱方案**：`docs/SANDBOX.md` + `scripts/sandbox/`（Docker / QEMU / nspawn 三套可跑测试沙箱）。
- **部署全流程**：`docs/DEPLOYMENT.md`（850 行）。
- **第二轮评审**：`docs/REVIEW.md` 第二轮审计。

### 统计

- 纯文档/脚本批次

---

## 阶段 7 — 跨 crate 集成测 + 网络冒烟（os-integration / nettest）

> 里程碑对应 HANDOVER §4 第 7 项。

### Added

- **`os-integration` crate**：首 5 场景端到端链路测（VM 创建链 / 访客验证链 / HA 故障转移 / 备份链 / IM 对话即操作）。
- **`nettest` crate**：真实网络连通冒烟（reqwest/axum/mdns/rustls，`#[ignore]` 默认不跑）。

### 统计

- crate：22 → **24**（+ os-integration + nettest）

---

## 阶段 6 — 基准 + BIP-322 + dyn 兼容 + radix 路由优化

> 里程碑对应 HANDOVER §4 第 6 项。

### Added

- **5 crate criterion micro-benchmark 骨架**（`feature/bench`）。
- **os-wallet 完整 BIP-322 验签**（legacy + simple，`feature/bip322`）。

### Changed

- **dyn 兼容补全**（`fix/dyn-compat`）：Replication / WalletConnector / RpcRegistry / JwtIssuer。
- **os-api 路由 register/match O(n²)→O(1) 优化**（`fix/radix-route`）：method 分桶 + 静态 HashMap。

### 统计

- crate：22
- 测试：+bip322

---

## 阶段 5 — P3 真实集成：AEAD（AES-256-GCM）+ ACME 自动证书

> 2026-08-05。ADR-DEPS-003 / ADR-DEPS-004。里程碑对应 HANDOVER §4 第 5 项。

### Added

- **os-services(devtools) AES-256-GCM 真实加密**：`ENC:` 占位 → 真实 AEAD 密钥 KVS。
- **os-security instant-acme 自动证书签续**：`acme_request` 占位 → 真实 RFC 8555。

### 统计

- crate：22
- 测试：+AEAD/ACME

---

## 阶段 4 — P2 真实集成 wave2：i18n / virt / cgroups / gix / mdns+mTLS

> 2026-08-05。main `d7200c2`。ADR-DEPS-002。里程碑对应 HANDOVER §4 第 4 项。

### Added

- **wave2 真实实现**：i18n（toml）/ vm（virt-ffi）/ osd（cgroups-rs）/ devtools（gix）/ discover（mdns-sd + mTLS）。

### Fixed

- **mTLS CryptoProvider panic bug**（本会话发现，`d7200c2`）：rustls 握手 panic → 显式 ring provider。

### 统计

- crate：22
- 测试：→ **1491**（`d7200c2`）

---

## 阶段 3 — P2 真实集成 wave1：openraft / rusqlite / dav-server / libunftp / russh 等

> 2026-08-05。ADR-DEPS-002。里程碑对应 HANDOVER §4 第 3 项。

### Added

- **20 个 P2 领域依赖注册**；wave1 真实实现：meta（openraft + rusqlite MetaStore）/ protocol（dav-server / libunftp / russh）/ wallet（bitcoin / alloy）/ security（argon2 / jwt / totp / rcgen / boringtun）/ monitor（opentelemetry）。

### 统计

- crate：22
- 测试：+wave1

---

## 阶段 2 — P0/P1 接通：reqwest/axum/tower/jwt/argon2/ed25519/nftnl/rtnetlink/tantivy

> ADR-DEPS-001。里程碑对应 HANDOVER §4 第 2 项。

### Added

- **11 个 P0/P1 第三方依赖注册**并被各 crate 引用真实实现：reqwest / axum / tower / jwt / argon2 / ed25519-dalek / nftnl / rtnetlink / tantivy。

### 统计

- crate：22
- 测试：+net

---

## 阶段 1 — 批 0–4 骨架：27 owner agent 写实现骨架（数据结构/状态机/Mock）

> 2026-08-04 ~ 05。里程碑对应 HANDOVER §4 第 1 项。仓库重建（原 `.git` 损坏，从 bare 重建主工作树）见 HANDOVER §5。

### Added

- **22 crate 全部有骨架 + 单元测**：批 0（core / i18n / orchestrator）、批 1（storage / network / rdma / security）、批 2（protocol / object / vm / container / wallet / meta / iso）、批 3（discover / guest / provision / update / backup / monitor / media / files / devtools / power）、批 4（im / api / client）。
- **27 owner agent 全部完成**，27/27。

### 统计

- crate：22
- commits：批 0–4 小计
- 测试：→ **1207**（批 0：63 / 批 1：171 / 批 2：367 / 批 3：446 / 批 4：160）

---

## 阶段 0 — 契约编译：22 crate cargo check 全绿

> 起点。里程碑对应 HANDOVER §4 第 0 项。

### Added

- **OS 系统接口契约**：22 crate / 86 trait + 31 agent 规格 + 主文档（commit `277a36b`）。
- **契约编译全绿**：`cargo check --workspace` 0 error 0 warning（commit `ff51221`）。

### Changed

- 3 个 COMPAT ADR 落档（async trait dyn 兼容 / DateTime 固定 UTC 别名 / EventBus `#[async_trait]`）。

### Fixed

- 错误模型 / 类型别名 / DateTime 别名修复。

### 统计

- crate：22
- commits：起点（`277a36b` 初始化 → `ff51221` 契约编译全绿）

---

## 累计统计（截至 2026-08-20，main `5775275`）

| 项 | 数 |
|----|----|
| crate 总数 | **26**（24 业务 + `os-integration` + `nettest`；另有 `crates/os-web/` 旧前端存档非 crate） |
| commit 总数 | **583**（main，2026-08-20） |
| 测试总数 | **4,100+**（`cargo test --workspace --features mock`，2026-08-18 起；含 `#[ignore]` 真实环境测） |
| 测试覆盖率 | **79.6% → ~85%**（cargo-tarpaulin，2026-08-06 快照，见 docs/COVERAGE_REPORT.md） |
| 前端 | 31 个 Vue view / 30 个桌面应用 / 约 330 条 API 路由（08-20 grep 口径；08-15 审计 304，docs/FEATURE_SURVEY.md） |
| owner agent | 27/27 + integration-agent + docs/review/devops 3 辅助 agent |
| 真实环境验证项 | 六轮共 20 项本机实跑 + 2 功能接通 + 产品化期（sd-turbo 生图 / aria2 / docker / rclone / SMB 迅雷链路 / SSH 隧道 / RDP 代理）实测 |
| 真实 bug 修复 | **12+ 个**（sigv4 空格折叠 / runc 管道 EOF / storage backstore+IQN+portals 3 个 / guest nft_expr + dport 字节序 2 个 / nftnl FFI 路径 + 字节序 2 个 / iso boot-info / storage export/unexport 2 个 / EVM 派生 0x04 前缀 / url_decode 中文参数损坏 / 悬赏认领竞态） |
| clippy pedantic | 3304 warning 扫描，修 25 文件，默认 clippy **0 warning** |
| 已落档 ADR | 8 个（COMPAT ×3 + DEPS ×5） |

## 链接

- [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)
- 全程里程碑表：[`docs/HANDOVER.md`](./docs/HANDOVER.md) §4
- 各批次详细记录：[`docs/PROGRESS.md`](./docs/PROGRESS.md)
- 性能基线：[`docs/PERFORMANCE_BASELINE.md`](./docs/PERFORMANCE_BASELINE.md)
- 覆盖率报告：[`docs/COVERAGE_REPORT.md`](./docs/COVERAGE_REPORT.md)
- 代码质量审计：[`docs/CODE_QUALITY_AUDIT.md`](./docs/CODE_QUALITY_AUDIT.md)
- TODO 审计：[`docs/TODO_AUDIT.md`](./docs/TODO_AUDIT.md)
- 错误码指引：[`docs/ERROR_GUIDE.md`](./docs/ERROR_GUIDE.md)
