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

## 全局/工程文档（非单功能）

| 文档 | 内容 |
|------|------|
| [ARCHITECTURE.md](ARCHITECTURE.md) | crate 全景与分层 + **§2.1 全系统拓扑总图（PPT 素材）+ §8 量化数字与里程碑时间线** |
| [DEPLOYMENT.md](DEPLOYMENT.md) | 部署/运维（§1–§8 蓝图；**§9 实机现状：os-api.service + /etc/default/os-api 全量 env 表 + 8080 端口语义**） |
| [FEATURE_SURVEY.md](FEATURE_SURVEY.md) / [TODO_AUDIT.md](TODO_AUDIT.md) | 功能完成度与 TODO 普查 |
| [HANDOVER.md](HANDOVER.md) / [PROGRESS.md](PROGRESS.md) / MEMORY.md（仓库根） | 交接与进度账（前两者已标历史状态头，现状以 MEMORY.md 为准） |
| [SANDBOX.md](SANDBOX.md) / [REVIEW.md](REVIEW.md) / [CODE_QUALITY_AUDIT.md](CODE_QUALITY_AUDIT.md) | 沙箱/审查/质量审计 |
| adr/ agents/ | 架构决策记录 / agent 协作规范 |

> env 未在本索引展开的功能，以各功能文档内"环境变量"小节为准；**全量 env 汇总表**见
> [DEPLOYMENT.md](DEPLOYMENT.md) §9.2。
> **PPT 制作提示**：拓扑图取 ARCHITECTURE §2.1（mermaid 可直接粘贴），量化数字取 §8，
> 各功能拓扑取功能文档"组件拓扑与数据流"小节。
