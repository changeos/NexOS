# NexOS 全功能调研报告（FEATURE SURVEY）

> 调研日期：2026-08-15 · 方法：静态代码扫描（只读）
> 范围：`crates/os-api/web/src/views/`（31 个 view，26,150 行）× `crates/os-api/src/handlers/`（28 个 handler，37,940 行）× 各 crate `lib.rs` 模块文档 + 关键实现抽查。
> 说明：os-nexhub 与 `docs/NEXHUB_LOBBY_DESIGN.md` 由另一代理负责，本报告对其只引用结论、不给改动建议。

---

## 1. 执行摘要

### 1.1 三大最值得投入的优化方向

**① 信任模型闭环（登录页 + JWT 发放 + 角色分级）——零件全在，只缺组装。**
现状是"单一共享 admin token"：`Settings.vue` 让用户把 `NEXOS_ADMIN_TOKEN` 填进 localStorage，之后该浏览器对所有 `requires_auth+admin` 路由全权放行（`http.rs` `extract_principal` 精确匹配即注入 admin Principal）。与此同时：(a) `os-security` 已有完整的 `JwtIssuer`/`JwtClaims`/`Principal`/`Role`（含 guest/high_privilege 分级）；(b) `gateway_impl.rs:243` 已在 dispatch 层强制执行 `requires_auth`/`required_roles`（"漏洞1"已修复）；(c) `Users.vue` 能真实 `useradd` 建系统用户——但建出来的用户**登不进 Web UI**（无 `/api/v1/auth/login` 端点，全仓库无 login 路由）。更深的结构问题：契约桥接（`gateway.rs:80-108`）在把请求传给 os-nexhub 等契约 handler 前**剥离 auth 字段**，契约 handler 永远拿不到 Principal，操作无法归因到人。补一个登录端点 + Users↔凭证绑定 + 契约 `ApiRequest` 增加 `auth` 透传，即可把"28 个应用共享一把万能钥匙"升级为真实多用户系统，且这是后续审计/配额/联邦信任的全部前置。

**② 最小联邦闭环（把 os-discover 接进 Nodes 页与 IM peers）。**
`os-discover`（3,661 行 / 64 测试）已经实现了联邦的**全部底层零件**：mdns-sd 真实组播的 `MdnsDiscovery`、ed25519 beacon 签名/验签、`MtlsPeerAuthenticator`（rustls 0.23 双向认证）、HA 资格纯算法、联邦决策状态机——但**零消费**：`DiscoverRouteHandler`（handlers/discover.rs）明确注释"先持有一份内存节点列表（默认含本机节点）"，`/discover/nodes`、`/api/v1/nodes` 返回的是内存假数据；Chat 的 `POST /api/v1/im/peers` 也只是"记录 IP:端口"不建立连接。前端 Nodes.vue（189 行）每 10 秒轮询的是这份假列表。这是仓库里投入产出比最高的一笔"从演示到真用"跃迁：handler 换数据源 + 后台扫描缓存（文档自己写了迁移路径："只需把 nodes 字段换成 discover_peers 取回的 Vec<PeerNode>"），1 周内 LAN 内多台 NexOS 就能互相看见，IM peers 顺势接 mTLS 会话。

**③ 数据面"最后一公里"三小件：Files 传输、Backup 真执行、CloudSync 持久化。**
三个高频刚需都停在 90% 处：(a) Files handler 只有 list/stat/mkdir/delete/rename，**没有上传/下载/复制/移动**——文件管理器传不了文件；(b) Backup 的"立即执行"仅把 `status` 翻成 `running`（handler 自述"标记执行，不真跑备份线程"），任务表纯内存重启即丢，且**没有任何恢复路径**，而 `os-storage` 的 `ZfsSendRecv`（zfs send|recv 管线）其实已经写好；(c) CloudSync 的 rclone 真跑但**任务定义在内存**——重启后所有同步任务消失（同仓库 share.rs 已有现成的 JSON 落盘模式可复制）。三件合计 1.5~2 周，直接决定"这能不能当 NAS 用"。

### 1.2 三大最该砍 / 降级的方向

**① Blockchain（前端 1,152 行 + 后端 2,350 行 + 20 条路由）→ 移出默认 Dock，砍钱包。**
完成度其实不低（docker compose 真起停），但使用价值是纯演示性的：家庭/小机房场景没有"自己管一条链"的高频需求；更严重的是安全隐患——钱包**私钥明文**存 `/tank/os-data/wallets.json`（blockchain.rs:517 `WALLETS_FILE` 常量，含 `private_key` 字段）。这块投入的 27 个测试维护成本与其价值完全不成比例。建议降级为可选模块，钱包功能在加密落地前直接移除。

**② StreamingCenter（前端 1,688 行）→ 与 Surveillance/Video 合并。**
后端自述"调度框架"：MediaMTX 不在线时"降级为已记录意图"——拉流/推流/节目切换的编排状态在内存里，真实动作只有 VOD 转码 spawn ffmpeg。它与 Surveillance（RTSP→HLS 录像，真实现）和 Video（媒体库+HLS 播放）功能高度重叠，三者各自维护一套 HLS 输出目录约定。合并后可共享 ffmpeg 进程管理、HLS 路径与播放器组件，估计净删 2,000+ 行。

**③ Provisioning 的 ISO 生成与 SSH deploy 两个半成品子项 → 对接 os-iso 或裁掉。**
handler 文档明说"ISO/deploy 本期纯任务记录"（不调 xorriso、不真传输），1,269 行前端对应的是两张任务记录表；而 `os-iso` crate（4,820 行 / 171 测试）才是真构建能力且 Makefile 已有 `make iso`。要么把 iso_tasks 真接到 os-iso 构建管线（$ 高价值：Web 一键出安装镜像），要么砍掉这两个 tab 只留 PXE（PXE 本身也是内存态）。当前状态是最差的：占着一级导航，给用户"能装系统"的假预期。

---

## 2. 功能全景表（30 行 = 29 条路由应用 + Dashboard）

完成度：✅ 真实现 / 🟡 半实现（含部分占位、依赖外部环境、状态不持久）/ ❌ 占位·演示。
价值：**核心** = 高频刚需；**中频**；**演示** = 技术展示性质。

| 应用（路由） | 前端行数 | 完成度 | 价值 | 关键证据（技术债） | 优化方向 |
|---|---|---|---|---|---|
| Dashboard（/） | 2,047 | ✅ | 核心 | 单文件 2,047 行；apps 数组与 appRegistry.ts 双份维护 | 拆 WindowFrame/Widgets 组件，apps 收敛到 registry 单源 |
| 存储管理（/storage） | 2,011 | ✅ | 核心 | 真 ZFS CLI（ZfsCliBackend，36 测试）；写操作需 sudo/zfs 环境 | RAID-Z 向导；容量趋势图（接 Monitor 采样） |
| 虚拟机（/vms） | 747 | 🟡 | 中频 | 后端默认**内存态骨架**（`virt-ffi` feature 默认关，libvirt FFI 需 libvirt-dev）；XML/状态机部分真实 | 开 virt-ffi 的 CI/发布通道，或 UI 明示"演示模式" |
| 文件共享（/shares） | 336 | 🟡 | 核心(NAS本职) | smb.conf 真写但靠 `OS_APPLY_SYSTEM` 门禁（默认关=不生效）；NFS 仅只读列表 | NFS 写导出；门禁状态在 UI 可见 |
| 用户（/users） | 242 | 🟡 | 核心 | 真实 `useradd/userdel`，但建的 Linux 用户**无法登录 Web**（信任模型断裂）；users.json 落盘 | 与登录/JWT 闭环（见 §1.1①） |
| 聊天（/chat） | 1,636 | ✅ | 核心(差异化) | SQLite + WebSocket 推送 + 大厅心跳均为真；身份取 localStorage `os.user.id` 缺省 'me'；peers 仅记录不连接 | 身份接登录；peers 接 mTLS（见 §1.1②） |
| 模型对话（/modelchat） | 445 | ✅ | 核心(AI叙事) | 直连 vLLM SSE 流式真实；历史仅 localStorage | 会话落库；多实例路由/降级 |
| 节点（/nodes） | 189 | ❌ | 核心(联邦入口) | `/api/v1/nodes` 返回**内存假列表**（handler 自述）；真 mDNS 在 os-discover 未接线 | **联邦 MVP 首选切入点**（§1.1②） |
| 网络（/network） | 895 | 🟡 | 中频 | 网卡/路由真探测（`ip -j`）；防火墙/VLAN/桥接为**内存占位卡**（前端注明"功能开发中"） | 接 os-network nftables 后端，或砍占位卡 |
| 系统自举（/provisioning） | 1,269 | ❌ | 低频 | PXE/ISO/SSH 六把 Mutex 内存态；ISO 与 deploy"纯任务记录"；SSH test 是唯一真实网络操作 | 对接 os-iso 真构建 or 裁剪（§1.2③） |
| 影院（/video） | 934 | ✅ | 中频 | 真盘扫描 `/tank/media/video` + TMDB 刮削（TMDB_API_KEY）；0 文件时回退 demo 数据（标 `demo:true` 不混入） | 刮削失败重试；字幕/播放进度 |
| 音乐（/music） | 433 | 🟡 | 中频 | 真扫描，但艺术家/专辑元数据为占位 | 音频标签解析（artist/album/cover） |
| 相册（/photo） | 1,090 | 🟡 | 中频 | 真扫描；缩略图为**确定性纯色占位**（无真缩略图生成） | 后台缩略图生成 + 时间线/人脸分组 |
| 备份（/backup） | 587 | 🟡 | 核心(数据安全) | 前端齐全；快照真（zfs snapshot/list/destroy + 60s 调度 + 保留策略）；但任务 `run` **只翻状态不执行**、任务表不持久、**无恢复路径** | 接 `ZfsSendRecv` 真执行 + 任务落 SQLite（§1.1③） |
| 监控（/monitor） | 503 | ✅ | 核心 | /proc 真指标 + SQLite 告警 + 阈值引擎（60s 轮）真实；唯 `/history` 为**占位示例数据** | 历史采样落库（alerts 表模式现成，1 天） |
| 文件管理（/files） | 490 | 🟡 | 核心 | 真实 FS 操作 + `..` 穿越防护；**无上传/下载/复制/移动** | 补 range 下载 + 分片上传（§1.1③） |
| 下载中心（/downloads） | 457 | ✅ | 中频 | 真 aria2 JSON-RPC + 自动拉起 aria2c；目录硬编码 `/tank/downloads` | magnet/BT；目录入配置 |
| 容器管理（/containers) | 397 | ✅ | 中频 | 真实 docker（经 `sg docker -c` 组会话 workaround） | 直连 docker socket API 更稳 |
| 监控摄像头（/surveillance） | 660 | ✅ | 中频 | 真 ffmpeg RTSP→HLS/MP4 + probe；配置 cameras.json 落盘；重启丢运行态（设计使然） | 录像回放日历；在线心跳探测 |
| 云同步（/cloudsync） | 484 | 🟡 | 中频 | rclone 真跑；**任务定义内存态——重启全丢**（与 share.rs 已有落盘模式不一致） | 任务 JSON 落盘（半天，复制 share 模式） |
| 笔记（/notes） | 342 | ✅ | 中频 | 真实 FS 持久化（/tank/notes→/var/lib/os/notes→内存 三级回退） | markdown 渲染 + 全文搜索 |
| 流媒体中心（/streaming） | 1,688 | 🟡 | 演示 | 内存编排 + VOD 转码真 spawn；MediaMTX"已记录意图"；NVENC 假设 GPU 存在（`-hwaccel auto` 已缓解） | 与 Surveillance/Video 合并（§1.2②） |
| 模型管理（/llm） | 911 | ✅ | 核心(AI OS) | GPU 动态探测（nvidia-smi/rocm-smi 回退）+ vLLM 实例真 spawn（fire-and-forget） | GPU 显存预算/池化；实例健康自愈 |
| API 网关（/gateway） | 1,243 | ✅ | 中频 | 全仓库最成熟 handler（3,661 行/58 测试/27 路由，SQLite：渠道/令牌/日志/统计） | 渠道自动发现本地 vLLM 实例并注册 |
| 区块链（/blockchain） | 1,152 | 🟡 | 演示 | docker compose 真起停；**钱包私钥明文**存 /tank/os-data/wallets.json | 降级出 Dock；钱包加密 or 移除（§1.2①） |
| 模型仓库（/modelhub） | 721 | ✅ | 中频 | 真扫 /tank/models + modelscope 真下载（进度=重扫目录估算） | 支持 HuggingFace；下载校验和 |
| 应用中心（/appstore） | 927 | 🟡 | 中频 | install 真 spawn apt/dpkg/snap（sudo，无密码 echo）；**13 个预置应用硬编码目录**，无远程源 | 远程应用源（可复用 NexHub 大厅思路） |
| 二维码传输（/qrtransfer） | 949 | ✅ | 演示(但真实) | 纯 Rust QR 编解码（qrcode/rqrr）+ ffmpeg 合成视频，roundtrip 可测 | 保留——离线传输叙事的真实组件 |
| NexHub（/codehub） | 1,529 | ✅ | 中频 | 真 git 裸仓库 + Smart HTTP CGI + SQLite 大厅（另一代理负责中，此处仅登记） | 见 NEXHUB_LOBBY_DESIGN.md |
| 设置（/settings） | 400 | 🟡 | 核心 | 系统信息/壁纸/令牌/虚拟化检测 4 节；**无登录页**；语言切换仅影响 shell | 变登录+会话管理入口（§1.1①） |

**统计：✅ 15 个 · 🟡 13 个 · ❌ 2 个**（另有 BleHub.vue 436 行未路由，作为 Chat 的 BLE tab 内嵌，属实验性 mesh 中继，spawn Python GATT 脚本，不计入）。

> 关于"Backup.vue 是占位页"的订正：Backup.vue 前端本身是齐全的真实现（587 行，调 5 组真实端点）；**占位的是后端任务语义**——"立即执行"只改状态不执行任何备份动作。取舍建议见 §4-O3。

---

## 3. 横向缺口清单（跨功能一致性问题）

1. **信任模型断裂（最高优先）**：无登录页、无 `/api/v1/auth/login`；`NEXOS_ADMIN_TOKEN` 一个 token = 永久全权 admin，存浏览器 localStorage；Users 建的系统用户登不进 Web；`Role`（admin/operator/auditor/guest…）与 `is_high_privilege/is_guest` 在 os-security 已定义却只有 "admin" 一个角色被路由表使用（全部 `required_roles=["admin"]`）；契约桥接剥离 `auth` 字段 → os-nexhub 等契约 handler 无操作归因。
2. **错误处理三档不一**：`friendlyError`（404/405→"后端尚未实现"）只在 17/30 视图存在；21 个视图用原生 `window.confirm` 而非统一 Dialog（仓库已有 Toast 体系 `useToast` 却没推广到确认场景）；22 处 `catch { }` 静默吞错。
3. **i18n 假覆盖**：三语言 locale（en/ja/zh）各 35 个 key，唯一消费者是 `LanguageSwitcher.vue`（shell 层）；26,150 行业务视图硬编码中文——语言切换开关的实际效果与用户预期严重不符。
4. **移动端/离线能力缺失**：无 PWA manifest / service worker / 图标集（public/ 只有 favicon+壁纸）；仅 16 个文件含 `@media`；os-mobile（3,745 行 Rust SDK）与 Web 前端零集成；无任何离线降级（网关断开=白屏级失败）。
5. **实时通道利用率低**：WsHub（broadcast + send_to）已可用且 Chat 已接，但 Monitor（2 处）、Nodes（10s）、AppStore、ModelHub、QrTransfer 等 7 个视图仍 setInterval 轮询；Monitor 的阈值引擎命中告警也不经 WS 推送到前端。
6. **持久化三档并存且分布随机**：SQLite（im/monitor/media/api_gateway/nexhub——重启全保）> JSON 落盘（share/user/notes/surveillance/ble——保）> **纯内存（backup 任务、cloudsync 任务、network 全部写操作、provisioning 全部、streaming 编排、vms、blockchain 部分——重启全丢）**。同为"任务列表"，backup/cloudsync 丢、downloads 不丢（aria2 自持），用户无法建立一致预期。
7. **`/tank` 硬编码 + env 双命名**：`"/tank..."` 字面量出现在 20+ 个文件（media.rs 21 处、streaming.rs 15 处、os-protocols/nfs.rs 18 处…），每个 handler 自带一条"tank→/var/lib/os→./xxx"回退链，互不一致；`OS_ADMIN_TOKEN/NEXOS_ADMIN_TOKEN`、`OS_APPLY_SYSTEM/NEXOS_APPLY_SYSTEM`、`OS_JWT_SECRET/NEXOS_JWT_SECRET` 双命名并存（12 vs 7 处引用）。
8. **`OS_APPLY_SYSTEM` 门禁的"半真半假"态**：share/user 的真实系统变更（写 smb.conf、useradd）默认**不生效**（env 未设时仅落 JSON），但 UI 仍报"创建成功"——用户看到的成功与系统实际状态可能不符，且 UI 无处显示门禁开关状态。
9. **路径参数手工解析**：网关 dispatch 不传 PathParams，每个 handler 自行 `split('/')` 解析 `:id`（mod.rs 承认此债）——重复代码 + 潜在解析不一致。
10. **Demo 数据边界不统一**：media"真盘优先、0 文件才回退 demo 且标 `demo:true`"是好范式；但 share/user"缺失/空 → 预置示例数据"没有 demo 标记，monitor alerts seed 2 条示例告警同样无标记——用户无法区分真实数据与预置演示。

---

## 4. 优化方向清单（按 ROI 排序）

| # | 做什么 | 为什么（证据） | 预估工作量 | 依赖 |
|---|---|---|---|---|
| O1 | **信任模型闭环**：新增 `/api/v1/auth/login`（校验系统用户/PAM 或 users.json 凭证）→ JwtIssuer 发 token；Settings 加登录页与会话显示；Users↔登录凭证绑定；契约 `os_common::gateway::ApiRequest` 增加 `auth` 透传（可选 `Principal`），装配层不再剥离 | os-security JwtIssuer/Role 已就绪、dispatch 已强制 authorize（gateway_impl.rs:243）、前端 client.ts 已有 Bearer 注入点（一处生效）；解锁多用户、审计归因、契约 handler 归因三件事 | 1~2 周 | 无（全部零件已存在） |
| O2 | **联邦最小闭环（Federation MVP）**：`DiscoverRouteHandler` 增加 `Arc<dyn Discovery>` 注入，后台 task 周期 `discover_peers`（含 beacon 验签）写缓存，`/api/v1/nodes` 从缓存读；IM `POST /peers` 接 `MtlsPeerAuthenticator` 建会话；Nodes.vue 增加配对/信任动作 | os-discover 的 mDNS/beacon/mTLS/状态机全实现全测试（64 tests）却零消费；handler 文档写明迁移路径"只需换 nodes 字段数据源"；Nodes/Chat 前端已就位 | 1 周（可见节点）+1 周（IM 真会话） | O1（联邦信任需按用户/节点区分） |
| O3 | **Backup 补实或降级**：方案 A=任务落 SQLite + `run` 真执行（本地快照→保留策略已有；跨机走 `ZfsSendRecv`）+ 恢复（zfs recv）入口；方案 B=砍任务表，页面降级为"ZFS 快照管理" | "立即执行"只翻状态（backup.rs 自述）；任务重启即丢；无恢复路径=备份功能不成立；os-storage Replication 已写好未接线 | A：3~5 天 / B：0.5 天 | 无 |
| O4 | **Files 补内容传输**：`GET /api/v1/files/download`（Range 支持大文件）+ `POST /upload`（分片/直传）+ copy/move | 文件管理器不能传文件是最刺眼的功能洞；handler 已有路径安全框架（禁 `..`） | 2~3 天 | 无 |
| O5 | **`/tank` 与 env 收敛**：新建 `os-common` Config 模块（`tank_root()`/`data_dir()`/`media_dir()` 单源 + `NEXOS_TANK_ROOT` env 覆盖），替换 20+ 文件字面量；env 命名二选一（建议 NEXOS_*，保留 OS_* 读回退一个版本周期） | 343 处 `/tank` 字面量、回退链互不一致；env 双命名 12 vs 7 处 | 2 天（机械替换+测试） | 无 |
| O6 | **CloudSync 任务持久化 + Monitor history 落库**：cloudsync 任务复用 share.rs JSON 落盘模式；monitor `/history` 改为定时采样 INSERT（SQLite 已有） | cloudsync 重启丢任务；monitor history 现为占位示例数据（handler 自述），而采样 task 已在跑只需顺手落库 | 各 0.5~1 天 | 无 |
| O7 | **UX 基建统一**：全局 Dialog 组件替换 21 处 `window.confirm`；`friendlyError` 提升到 api/client.ts 统一；401 全局跳登录（依赖 O1）；空状态组件化（26 视图已各自实现一遍） | 错误提示/确认/空状态三件套在 30 个视图里各写各的 | 3 天 | O1（401 跳转） |
| O8 | **轮询→WS 推广**：Monitor 指标/告警、Nodes 节点上下线、AppStore 安装进度走 WsHub 订阅 | WsHub 已可用、Chat 已验证模式；7 视图轮询浪费且延迟高 | 3 天 | O2（Nodes 事件源） |
| O9 | **降级/砍除执行**：Blockchain 移出默认 Dock + 钱包私钥加密（或移除钱包）；StreamingCenter 并入 Surveillance/Video；Provisioning ISO/deploy 要么接 os-iso（Makefile `make iso` 已验证构建链）要么裁掉 | §1.2 三段论据；合计可净减 ~3,000 行维护面 + 消除一个明文私钥安全隐患 | 各 1~2 天 | 无 |
| O10 | **门禁可见性**：`OS_APPLY_SYSTEM` 状态经 `/status` 暴露，share/user 写操作响应附 `applied:false` 时前端提示"仅记录未生效（需设 NEXOS_APPLY_SYSTEM=1）" | 消除"UI 报成功、系统未变更"的半真半假态 | 1 天 | 无 |

---

## 5. 数据支撑

### 5.1 前端（crates/os-api/web/src）

- **31 个 view 文件 / 26,150 行**；web src（vue+ts）合计 **32,484 行**。
- 行数 Top5：DashboardView 2,047 / Storage 2,011 / StreamingCenter 1,688 / Chat 1,636 / CodeHub 1,529。
- 行数 Bottom5：Nodes 189 / Users 242 / Shares 336 / Notes 342 / Containers 397——与"后端越假前端越薄"完全正相关（Nodes 后端假→前端最薄）。
- API client（client.ts）**249 个端点函数**；i18n 3 locale × 35 key（仅 shell 消费）；WebSocket 仅 Chat 消费；`window.confirm` 21 文件；`@media` 16 文件。

### 5.2 后端 handler（crates/os-api/src/handlers）

- **28 个 handler / 37,940 行 / 632 个单测**（该目录内）。
- 测试密度 Top3：api_gateway 58（3,661 行）、streaming 39、backup 38。
- 测试密度相对薄弱：system 8（516 行）、notes 8（682 行）、files 9（689 行）——files 涉及路径安全却测试最少之一。
- 行数 Top3：api_gateway 3,661 / streaming 2,564 / media 2,427。

### 5.3 路由

- **静态注册 RouteSpec 共 304 条**：os-api 26 个 handler 277 条 + os-nexhub 2 个 handler 27 条（用脚本解析各 `routes()` 函数体统计）。另有 axum 层动态挂载：`/ws`、`/git/{*path}`（Smart HTTP CGI）、静态资源 fallback。
- MEMORY.md 记"470+ API 路由"，与本次可验证的 304 条静态 spec 存在口径差（可能含动态路径展开/历史计数）；建议后续以 `os-api --check` 输出为准修正 MEMORY.md。

### 5.4 workspace 测试与代码量

- **`#[test]`+`#[tokio::test]` 属性共 4,172 个**（含 cfg(test) 代码；docs/PROGRESS.md 记 cargo test 运行口径 3,348 passed + 127 ignored，2026 较早时点）。测试数 Top5：os-api 757 / os-services 489 / os-compute 392 / os-protocols 238 / os-network 192。
- 代码量 Top5（src/，行）：os-api 44,751 / os-services 18,924 / os-compute 8,856 / os-protocols 7,730 / os-wallet 6,075。
- **实现度反差最大的三个 crate**：os-discover 3,661 行/64 测试（能力完整、零消费）；os-iso 4,820 行/171 测试（真构建能力、未被 Provisioning 使用）；os-mobile 3,745 行/157 测试（客户端 SDK、未接 Web/PWA）——三者都是"底层已建好、产品面未接通"，与 §4 O2/O9 直接对应。

---

## 附：本报告未覆盖项

- os-nexhub 与 NEXHUB_LOBBY_DESIGN.md 的在途改动（另一代理负责）。
- osd 守护进程编排、os-update/os-wallet/os-meta 等非桌面应用 crate 的内部质量（仅统计量）。
- 运行时行为（未启动服务实测，全部结论来自静态代码与模块自述文档）。
