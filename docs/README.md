# docs 索引 —— 功能文档速览（供 AI agent 协作）

> 协作铁律：**每个功能的新增能力和全部环境变量必须在该功能的 MD 里说明**。
> 所有 env 均从源码 grep 核实（名称/默认值/作用），统一 `| 变量 | 默认 | 作用 |` 表格。

## 功能文档索引

| 功能 | 文档 | 一句话 | 关键 env 速览 |
|------|------|--------|----------------|
| 远程转发（SSH 隧道 + RDP） | [FORWARDING.md](FORWARDING.md) | spawn ssh 做 -L/-R/-D 三种隧道 + 纯 Rust TCP 代理转发 RDP 并生成 .rdp 文件 | `NEXOS_SSH_BIN`（ssh）、`NEXOS_FORWARDING_HOST`/`OS_FORWARDING_HOST`（hostname 回退） |
| 存储与共享（SMB 链路） | [STORAGE_SHARING.md](STORAGE_SHARING.md) | nexos-downloads SMB 共享运维手册：smb.conf/avahi 品牌统一/迅雷接入坐标/Files·Storage 页面能力 | 无专属 env（`share.rs` 的 `NEXOS_APPLY_SYSTEM`/`OS_APPLY_SYSTEM` 门禁仅在文档提及） |
| 媒体生成 + 链上身份 | [MEDIA_GEN_AND_CHAIN_AUTH.md](MEDIA_GEN_AND_CHAIN_AUTH.md) | sd-turbo 本地生图（显存互斥 503）+ 视频任务框架 + NexHub 链上身份 | `NEXOS_IMGGEN_BIN/SCRIPT/TIMEOUT_SECS`、`NEXOS_SMI_BIN`、`NEXOS_SD_MODEL`、`NEXOS_VIDEO_API_URL/KEY` |
| 网关变现（计费+充值） | [GATEWAY_MONETIZATION.md](GATEWAY_MONETIZATION.md) | billing_mode 四模式计费 + USDT/BTC/EVM 充值订单（价目常量/契约表/env） | `NEXOS_PAY_USDT_ADDR`/`NEXOS_PAY_BTC_ADDR`/`NEXOS_PAY_EVM_ADDR`（当前为占位值，前端警示） |
| vLLM 实例监控 | [LLM_MONITORING.md](LLM_MONITORING.md) | 按需抓 vLLM /metrics（5s 缓存/3s 超时），Counter 差值算速率，不可达 200+null | `NEXOS_LLM_METRICS_SIMULATE`（默认关；开则端口不通时回 sin 波合成数据） |
| IM 区块链认证 | [IM_BLOCKCHAIN_AUTH_DESIGN.md](IM_BLOCKCHAIN_AUTH_DESIGN.md) | 身份=secp256k1 公钥，挑战-签名三步认证（§6 平台身份通用性：chain_auth 共享内核） | 无专属 env（admin 回落走 `NEXOS_ADMIN_TOKEN`/`OS_ADMIN_TOKEN`） |
| **agent 协调组件（agent-coord）** | [AGENT_COORDINATION.md](AGENT_COORDINATION.md) | IM 群消息 @ 定向投递（在线 WS / 离线收件箱+webhook）+ agent 注册表 + 收件箱 ack + 协作协议声明（设计来自 nexos-test README §2；含拓扑图/端点契约） | `NEXOS_AGENTS_FILE`（注册表+收件箱 JSON，缺省 `/tank/os-data/agents.json`） |
| NexHub 大厅 | [NEXHUB_LOBBY_DESIGN.md](NEXHUB_LOBBY_DESIGN.md) | 代码大厅发布/克隆/悬赏/付费门禁 + 链上身份权限矩阵（§12 端点契约 / §13 env 全量） | `NEXOS_GIT_REPOS_DIR`/`NEXOS_GIT_USER`/`NEXOS_GIT_HOST`/`NEXOS_HTTP_PORT`/`NEXOS_LOBBY_NO_AUTO_PUBLISH`/`NEXOS_ADMIN_TOKEN` |
| NexHub 外部 agent 接入 | [NEXHUB_ONBOARDING.md](NEXHUB_ONBOARDING.md) | 外部 agent 三步上架手册（建仓/发布/克隆坐标） | — |
| **NexHub CLI（nexhub）** | [NEXHUB.md](NEXHUB.md) | 单文件 CLI 分发端点（GET /api/v1/coderepo/cli.sh 公开动态生成，Host 头推导节点地址 + text 直传）：login/whoami/ping/repo/clone/apps deploy/self-update；token 经 curl -H @file 注入不进 argv | 无新增节点 env（客户端 `NEXHUB_NODE`/`NEXHUB_TOKEN`；端点侧复用 `NEXOS_GIT_ADVERTISE_HOST`） |
| 外部 LLM 渠道 | [EXTERNAL_LLM_CHANNELS.md](EXTERNAL_LLM_CHANNELS.md) | 免费渠道聚合与路由策略 | — |
| **应用中心（AppStore）** | [APPSTORE.md](APPSTORE.md) | apt/deb/snap/flatpak 四通道安装任务流 + 用户发布（含拓扑图；内存态限制） | 无专属 env（依赖宿主包管理器 + 免密 sudo） |
| **Agent 集合（AgentHub）** | [AGENT_HUB.md](AGENT_HUB.md) | 常用 AI coding agent（OpenCode/OpenClaw/Claude Code/Codex/Gemini CLI/Aider/Goose…）一键安装：npm/script/uv/cargo 四渠道后台任务 + command -v 已装探测 + 工具链可用性 + 自定义 agent 发布（含拓扑图/路由表） | `NEXOS_AGENTHUB_FILE`（自定义 agent JSON，缺省 `/tank/os-data/agenthub.json`）、`NEXOS_AGENTHUB_NPM_SUDO`（npm 渠道 sudo 策略，默认自动探测） |
| **P2P 组网（os-p2p）** | [NEXOS_P2P_NETWORK_DESIGN.md](NEXOS_P2P_NETWORK_DESIGN.md) | 全分布式 Kademlia + ECDH 链路加密 + 观测端点八卦 + TCP 打洞连接阶梯 + mDNS 种子 + os-api/网络页接入（P1+P2a+P2b；含部署拓扑图/env 全表/端点契约） | `NEXOS_P2P_ENABLE`（默认关）、`NEXOS_P2P_BOOTSTRAP`、`NEXOS_P2P_LISTEN`（`:7070`）、`NEXOS_P2P_PUBLIC`、`NEXOS_P2P_MDNS`、`NEXOS_P2P_NAME`、`NEXOS_P2P_KEY_FILE`（私钥持久化，重启同 NodeID） |
| **区块链管理（Blockchain）** | [BLOCKCHAIN.md](BLOCKCHAIN.md) | docker compose 编排链节点/Blockscout + k256 钱包（含拓扑图；⚠️私钥明文落盘风险标注） | 无专属 env（依赖 docker / python3 eth-account 可选） |
| **系统监控（Monitor）** | [MONITOR.md](MONITOR.md) | /proc 真实指标 + SQLite 告警 + 60s 阈值引擎（含磁贴数据源对照表/拓扑图） | 无专属 env（DB 路径三段式探测） |
| **适配器应用速查** | [APPS_REFERENCE.md](APPS_REFERENCE.md) | QR 传输/下载中心(aria2)/容器/笔记/云同步(rclone)/BLE 中继六应用：路由表+存储+拓扑速查 | 均无专属 env（依赖 ffmpeg/aria2/docker/rclone/BlueZ） |
| **开发者中心（devdocs）** | [DEVDOCS_DEV_CENTER.md](DEVDOCS_DEV_CENTER.md) | 文档门户：仓库 docs/ 唯一事实源的只读索引+原文服务 + Markdown 渲染桌面应用（含 docs/dev/ 八篇开发者指南：应用开发/安装应用/区块链 SDK/IM agent/NexHub/handler 开发/多节点部署） | `NEXOS_DEVDOCS_DIR`（文档根，缺省 `/home/oem/NexOS/docs`，回退二进制旁 `./docs`；无 checkout 节点降级空清单+提示） |
| **更新应用（update）** | [UPDATE_APP.md](UPDATE_APP.md) | os-api 自身远程升级闭环：版本发现（NexHub tag 两级解析链）→ 工件登记 → apply 真实安装管线（staged/备份/rename/自重启）+ A/B 槽位视图 + 四通道 | `NEXOS_UPDATE_REPO_URL` 等（§7 全表） |
| **网络装机（iPXE）** | [NET_BOOT.md](NET_BOOT.md) | U 盘 iPXE → 输 IP → 全自动装 Ubuntu + 入集群：双架构 ISO 仓 sha256 对账 + 纯 Rust ISO9660 提取器 + ISO 流式直传（Range 206）+ latest 滚动策略 + autoinstall 种子 | `NEXOS_NETBOOT_*`（§内全表） |
| **直播（live，系统保留）** | [LIVE_STREAMING.md](LIVE_STREAMING.md) | P2P 联邦直播：纯 Web 采集（MediaRecorder 切片→WS）→内存扇出→MSE 观看；跨节点中继 1MiB 分块重组、帧 md5 对拍（v0.1.51 剥离批核定保留） | `NEXOS_LIVE_MAX_FRAME_BYTES`/`NEXOS_LIVE_MAX_VIEWERS` |
| **传输组件（transfer）** | [TRANSFER_COMPONENT.md](TRANSFER_COMPONENT.md) | 迅雷式统一下载管理 + P2P 网状分发：五帧协议走加密 overlay，逐块 sha256/坏块重试/断点续传/完成自动做种（不经公网 IP 分发） | `NEXOS_TRANSFER_*`（§内全表） |
| **WAN 出口共享（network-exit）** | [NETWORK_EXIT_RELAY.md](NETWORK_EXIT_RELAY.md) | 出口节点 offer 声明（digest 全网可学）→ 授权（TTL 默认拒绝）→ 使用方 SOCKS5 over overlay 出网 + iptables 防火墙链（NEXOS-FW） | —（overlay 级，无内核侵入） |
| **打赏（tips）** | [TIPS.md](TIPS.md) | 统一打赏原语：链上身份账本，五类目标服务端反查防伪造（IM 消息/大厅条目/节点） | —（tips.db） |
| **链节点运行（blockchain-nodes）** | [BLOCKCHAIN_NODES.md](BLOCKCHAIN_NODES.md) | geth/bitcoind 真实子进程生命周期 + 全节点空间预检（内置体积表）+ 二进制探测 + 按实例日志 tail | `NEXOS_CHAIN_NODE_BIN_*`/`NEXOS_CHAIN_NODE_DATA_ROOT`/`NEXOS_CHAIN_NODE_SIZE_HINTS` |
| **LLM 推理环境** | [LLM_ENVIRONMENTS.md](LLM_ENVIRONMENTS.md) | vLLM Python venv 做成可管理资源：uv 多环境并存/默认环境/异步创建更新任务（环形日志） | `NEXOS_LLM_ENVS_ROOT`/`NEXOS_LLM_UV_BIN` 等 |
| **LLM 实例管理** | [LLM_INSTANCES.md](LLM_INSTANCES.md) | 实例生命周期四块：自动选口真实试绑/拉起日志/AddrInUse 监视换口重试/接入说明面板 | —（llm.db） |
| **外部 API 接入** | [LLM_EXTERNAL_APIS.md](LLM_EXTERNAL_APIS.md) | 外部 OpenAI 兼容端点接入模型管理：CRUD/真实 /models 连通测试/SSE 对话直通/PUT 编辑保留语义 | — |
| **模型大厅（model_hub）** | [MODELHUB_LOBBY.md](MODELHUB_LOBBY.md) | 权重明细/删除/符号链接导入/发布多源合并 + 魔搭/HF 镜像在线下载（闭区间 Range 分块续传） | `NEXOS_MODELSCOPE_BASE/TOKEN`、`NEXOS_HF_BASE/TOKEN` |
| **API 大厅（api-market）** | [API_MARKET.md](API_MARKET.md) | 推理服务市场：发布（链上身份唯一通道）/心跳+metrics 代拉三态负载/价格排序/联邦一键导入 | —（access_info 三视角脱敏） |
| **指纹账本（os-identity）** | [IDENTITY_COMPONENT.md](IDENTITY_COMPONENT.md) | NodeID↔地址证据登记/owns_addr 四态判定/冲突记账/回环拒收（2026-08-25 从 os-p2p 抽离，传输层回归纯传输） | `NEXOS_IDENTITY_FILE` |
| **NexHub Issues/PR** | [NEXHUB_ISSUES_PR.md](NEXHUB_ISSUES_PR.md) | 项目级 Issues/PR 协作层：三态流转+评论环形/PR diff-merge（--no-ff+issue 自动关）/权限（链上身份开评，owner merge） | — |
| **管理终端（terminal）** | [ADMIN_CONSOLE.md](ADMIN_CONSOLE.md) | Web 终端：xterm.js ↔ WS ↔ PTY（bash/ssh -tt 密码透传）+ 17 条快捷命令面板 + 节点状态条 | —（admin 鉴权，会话上限 8） |
| **系统自举（provisioning）** | [PROVISIONING.md](PROVISIONING.md) | PXE/ISO 生成/SSH 远程部署 + 电源控制层（本机 BMC/远程 IPMI 2.0/RMCP+ 免凭据扫描/WoL 魔术包） | `NEXOS_PROVISION_*`（§内全表） |
| **应用开发指南（APPS）** | [APPS.md](APPS.md) | 应用包运行时全指南（12 节）：manifest 规范/宿主桥/@nexos/app-sdk/引擎门控→剥离终态/publish-app 发布流/第二个应用 Checklist | `NEXOS_APPS_DB` 等 |
| **IM agent 接入** | [IM_AGENTS_AND_FILES.md](IM_AGENTS_AND_FILES.md) | 多 AI agent 接入与文档传输：链上身份三步/WS 订阅/@ 定向投递三态/附件字节级往返/断线补拉/通知 webhook | — |
| **一键安装引导** | [BOOTSTRAP_INSTALL.md](BOOTSTRAP_INSTALL.md) | NAT 后 Ubuntu 一条 curl 命令完成 os-api 安装+systemd 服务化+自动入集群（install.sh 动态生成，版本感知升级） | `NEXOS_ADMIN_TOKEN`（装完必换）等 |

## 全局/工程文档（非单功能）

| 文档 | 内容 |
|------|------|
| [ARCHITECTURE.md](ARCHITECTURE.md) | crate 全景与分层 + **§2.1 全系统拓扑总图（PPT 素材）+ §8 量化数字与里程碑时间线** |
| [DEPLOYMENT.md](DEPLOYMENT.md) | 部署/运维（§1–§8 蓝图；**§9 实机现状：os-api.service + /etc/default/os-api 全量 env 表 + 端口语义**） |
| [FEATURE_SURVEY.md](FEATURE_SURVEY.md) / [FEATURE_SURVEY_2026-08-20.md](FEATURE_SURVEY_2026-08-20.md) / [TODO_AUDIT.md](TODO_AUDIT.md) | 功能完成度与 TODO 普查（调研快照） |
| [HANDOVER.md](HANDOVER.md) / [PROGRESS.md](PROGRESS.md) / MEMORY.md（仓库根） | 交接与进度账（前两者已标历史状态头，现状以 MEMORY.md 为准） |
| [SANDBOX.md](SANDBOX.md) / [REVIEW.md](REVIEW.md) / [CODE_QUALITY_AUDIT.md](CODE_QUALITY_AUDIT.md) | 沙箱/审查/质量审计 |
| [ERROR_GUIDE.md](ERROR_GUIDE.md) / [DEPENDENCIES.md](DEPENDENCIES.md) | 错误码归类（跨 crate From→ApiError 审计）/ 依赖选型归档（ADR 索引） |
| [COMPONENT_INDEPENDENCE_AUDIT.md](COMPONENT_INDEPENDENCE_AUDIT.md) / [PERFORMANCE_BASELINE.md](PERFORMANCE_BASELINE.md) / [COVERAGE_REPORT.md](COVERAGE_REPORT.md) | 组件独立性审计 / criterion 性能基线 / 覆盖率报告 |
| adr/ agents/ research/ dev/ | 架构决策记录（8 ADR）/ agent 协作规范（历史规格书）/ 调研方案书存档 / 开发者指南八篇 |

### 历史存档（已剥离引擎/已完成的修复记录，供追溯）

| 文档 | 说明 |
|------|------|
| [FILM_STUDIO.md](FILM_STUDIO.md) / [SURVEILLANCE.md](SURVEILLANCE.md) | 已剥离引擎（v0.1.51）的契约存档——现役产品线为独立版 FilmStudio / StreamingStudio（各自 NexHub 仓） |
| [NODE_META_LOOPBACK_FIX.md](NODE_META_LOOPBACK_FIX.md) | 127.0.0.1 回环噪声全链闭环修复记录（digest 出入口过滤/mDNS 隔离域/历史清理） |
| [DESIGN_PHILOSOPHY_REVIEW.md](DESIGN_PHILOSOPHY_REVIEW.md) / [NEXHUB_OPTIMIZATION_2026-08-15.md](NEXHUB_OPTIMIZATION_2026-08-15.md) / [NEXOS_UI_OPTIMIZATION_PLAN.md](NEXOS_UI_OPTIMIZATION_PLAN.md) | 设计理念复盘 / NexHub 优化过程 / UI 优化方案（点时快照） |

> env 未在本索引展开的功能，以各功能文档内"环境变量"小节为准；**全量 env 汇总表**见
> [DEPLOYMENT.md](DEPLOYMENT.md) §9.2。
> **PPT 制作提示**：拓扑图取 ARCHITECTURE §2.1（mermaid 可直接粘贴），量化数字取 §8，
> 各功能拓扑取功能文档"组件拓扑与数据流"小节。
