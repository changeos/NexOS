//! 业务组件 RouteHandler 适配器（规划文档 §3.6 / §9.1#10）。
//!
//! 定位：每个业务组件（os-storage / os-compute / system / share / user / discover）实现一个
//! [`crate::RouteHandler`]，把自己的路由注册进网关并由 `handle` 转发到真实后端
//! （`ZfsCliBackend` / `LibvirtVmManager` 等）。binary 入口（`main.rs`）把这些适配器
//! 装配进 `InProcessGateway`，组装完整 HTTP 网关。
//!
//! # 模块
//!
//! - [`storage`]：`StorageRouteHandler` —— 持有 `Arc<ZfsCliBackend>`，提供 pool/dataset 只读路由。
//! - [`compute`]：`ComputeRouteHandler` —— 持有 `Arc<LibvirtVmManager>`，提供 vm 只读路由。
//! - [`system`]：`SystemRouteHandler` —— 提供 `/status` / `/healthz` / `/version` 等系统聚合路由。
//! - [`share`]：`ShareRouteHandler` —— 提供 `/shares` / `/api/v1/exports` 共享管理路由（内存态）。
//! - [`user`]：`UserRouteHandler` —— 提供 `/api/v1/users` 用户管理路由（内存态）。
//! - [`devdocs`]：`DevDocsRouteHandler` —— 「开发者中心」桌面应用 REST 入口：
//!   仓库 `docs/`（文档唯一事实源，git push 即更新）的只读索引与原文服务。
//!   `GET /api/v1/devdocs/index`（扫描根 + 一级子目录 `*.md`，提取标题/
//!   分类/大小/mtime，缓存 30s）+ `GET /api/v1/devdocs/doc/*path`（markdown
//!   原文；仅 .md、canonicalize 防穿越；`?lang=en|zh-TW` 走本地 LLM AI 翻译
//!   管线：译文缓存 → 异步任务 202 → `GET /devdocs/translate/tasks/:id` 轮询
//!   → 无可用模型 503 诚实降级）。文档根 env `NEXOS_DEVDOCS_DIR`（缺省
//!   `/home/oem/NexOS/docs`，回退二进制旁 `./docs`）；不存在时降级空清单 +
//!   提示，或 `NEXOS_DEVDOCS_FALLBACK_URL` 联邦回退透传。开发期公开读。
//! - [`discover`]：`DiscoverRouteHandler` —— 持有内存节点列表，提供 `/discover/nodes`
//!   / `/api/v1/nodes` / `/api/v1/nodes/:id` 节点发现路由（os CLI `discover` 命令对应）。
//! - [`im`]：`ImRouteHandler` —— 持有内存态对话/群组/节点列表，提供
//!   `/api/v1/im/conversations*` / `/api/v1/im/groups*` / `/api/v1/im/peers`
//!   / `/api/v1/im/status` IM 路由（对话/群组/Federation 暴露为 REST）。
//! - [`network`]：`NetworkRouteHandler` —— 网络管理 REST API：
//!   `/api/v1/network/interfaces`（真实探测 `ip -j -br addr`）、
//!   `/api/v1/network/routes`（真实探测默认网关）、`/api/v1/network/status`（概要）、
//!   `/api/v1/network/firewall` / `POST /vlan` / `POST /bridge`（内存态占位）。
//! - [`p2p`]：`P2pRouteHandler` —— P2P 组网层（os-p2p）REST 入口（P2b 接入）：
//!   `GET /api/v1/p2p/status|peers|buckets|ladder`（读公开，网络页拓扑 UI 数据源）
//!   与 `POST /api/v1/p2p/send|connect`（admin）。未启用（`NEXOS_P2P_ENABLE` 未设）
//!   时全部端点 503 + 引导文案。P3 起本模块另承载联邦桥（`FederationBridge` /
//!   `fed_broadcast` / `P2pLobbyTransport`——IM 大厅消息与 NexHub 大厅条目经
//!   os-p2p 跨节点互通，main.rs 装配）。
//! - [`node_view`]：`NodeViewRouteHandler` —— 节点发现页聚合视图（os-p2p 真实
//!   数据，替代旧 `/api/v1/nodes` 内存假数据）：`GET /api/v1/nodes/combined`
//!   一次返回 `{lan, p2p, ladder, self}`——lan=underlay 私网地址的直连 peer、
//!   p2p=公网/NAT peer + Kademlia 桶非直连节点、self=本机 NodeID/昵称/角色。
//!   静态路由优先于 discover 的 `/api/v1/nodes/:id` 参数路由。
//! - （已迁出）`code_repo` / `nexhub_lobby` —— NexHub 两大 handler（代码仓库中心
//!   `/api/v1/coderepo/*` + 大厅发现层 `/api/v1/nexhub/lobby/*`）已抽到独立 crate
//!   `os-nexhub`（NexHub 独立化，审计 docs/COMPONENT_INDEPENDENCE_AUDIT.md §6）：
//!   经 `os_common::gateway::RouteHandler` 轻量契约对接，由 binary 入口（main.rs）
//!   直接从 `os_nexhub` 引入注册；`/git/*` CGI 的仓库根回退用 `os_nexhub::repos_dir`
//!   （http.rs）。路由/环境变量/DB 路径运行期契约零变化。
//! - [`provisioning`]：`ProvisioningRouteHandler` —— 系统自举桌面应用 REST 入口，
//!   持有内存态 PXE 配置/启动条目/服务运行态 + ISO 构建任务 + SSH 目标/部署任务，
//!   提供 `/api/v1/provisioning/pxe/*` / `/api/v1/provisioning/iso/*`
//!   / `/api/v1/provisioning/ssh/*` / `/api/v1/provisioning/stats`
//!   系统自举三件套（PXE 网络启动 / ISO 镜像生成 / SSH 远程部署）管理路由
//!   （原 PXE 子项搬自 `pxe.rs`，ISO 与 SSH 为新增；SSH test 真实调系统 ssh
//!   子进程密钥认证，ISO/deploy 本期纯任务记录）。
//! - [`power`]：`PowerRouteHandler` —— 系统自举「电源控制层」REST 入口
//!   （PXE 装机流水线第一环：先唤醒/上电，再 PXE 引导，最后 SSH 部署），
//!   提供 `/api/v1/provisioning/power/*`：本机 BMC in-band（ipmitool
//!   chassis/sel/mc/sensor，工具缺失或无 /dev/ipmi0 时明确降级非 500）
//!   + 远程 IPMI 2.0 设备（lanplus RMCP+，密码落 state 文件但响应脱敏）
//!   + 网段扫描（纯 Rust RMCP Presence Ping，UDP 623 免凭据发现，/24 上限）
//!   + LAN 魔术唤醒 WoL（魔术包 UDP 广播 ×3，含 SecureOn 扩展；不依赖
//!     ipmitool）。状态持久化 env `NEXOS_POWER_STATE`（原子写）。
//! - [`media`]：`MediaRouteHandler` —— 持有内存态媒体库（影院/音乐/相册三类
//!   `MediaItem`），提供 `/api/v1/media/library` / `/api/v1/media/stats`
//!   / `/api/v1/media/item/:id` / `POST /api/v1/media/scan` 媒体库管理路由
//!   （OS"媒体三件套"桌面应用的后端 REST 入口）。
//! - [`media_gen`]：`MediaGenRouteHandler` —— 媒体生成（图片真实 sd-turbo
//!   spawn python 管线 + 显存探测互斥 + 视频任务框架可插后端），提供
//!   `POST /api/v1/media/image` / `GET /api/v1/media/image/recent`
//!   / `POST /api/v1/media/video` / `GET /api/v1/media/video/tasks`
//!   / `GET /api/v1/media/video/tasks/:id` / `POST /api/v1/media/auth/challenge|verify`
//!   （生图写操作三路身份：链上 token / admin / sk-os- 网关令牌计费）。
//! - [`files`]：`FilesRouteHandler` —— 文件管理器桌面应用 REST 入口，真实文件
//!   系统浏览（spawn_blocking 读目录），提供 `/api/v1/files/list` / `/stat`
//!   / `POST /mkdir` / `/delete` / `/rename`（写操作需 admin，禁 `..` 路径穿越）。
//! - [`downloads`]：`DownloadsRouteHandler` —— 下载中心桌面应用 REST 入口，真实
//!   aria2（JSON-RPC :6800）下载任务管理，提供 `/api/v1/downloads/tasks` CRUD
//!   + pause/resume/cancel / `/stats`（写操作需 admin）。
//! - [`transfer`]：`TransferRouteHandler` —— P2P 传输组件（component=transfer，
//!   2026-08-25）REST 入口：迅雷式多源下载管理 + 网状分发。本地文件发布为
//!   可传输清单（sha256 内容寻址）→ 其他节点凭 sha256/transfer_id 经 os-p2p
//!   叠加层（打洞/中继——**不依赖公网 IP**）query 源、分块拉取（逐块 sha256
//!   校验 / ≤4 在途背压 / 断点续传 / 下载完成自动做种）。与 downloads 分工：
//!   公网 HTTP/BT 走 aria2，节点间走 transfer。提供 `/api/v1/transfer/publish`
//!   / `manifests[/:id]` / `fetch` / `tasks[/:id/{pause|resume|cancel}]` / `stats`
//!   （读公开 / 写 admin；`NEXOS_P2P_ENABLE=1` 才启用，否则 503）。
//! - [`containers`]：`ContainersRouteHandler` —— 容器管理桌面应用 REST 入口，真实
//!   Docker（经 `sg docker -c` 子进程）容器+镜像管理，提供 `/api/v1/containers/list`
//!   / `/create` / `/:id/start|stop|restart` / `DELETE /:id` / `/images` / `/stats`
//!   （写操作需 admin）。
//! - [`streaming`]：`StreamingRouteHandler` —— 流媒体中心桌面应用 REST 入口，内存态
//!   拉流源/转码任务/推流目标/节目输出管理，FFmpeg NVENC 转码子进程 spawn（调度
//!   框架，MediaMTX/ffmpeg 不在线时降级为"已记录意图"，不 panic），提供
//!   `/api/v1/streaming/sources*` / `program*` / `transcode*` / `outputs*`
//!   / `/stats`（写操作需 admin）。
//! - [`forwarding`]：`ForwardingRouteHandler` —— 远程转发工具 REST 入口：SSH 隧道
//!   （spawn 系统 `ssh` 子进程，local/remote/dynamic 三模式，密钥认证无密码字段）
//!   与 Windows RDP 转发（纯 Rust TCP 代理 + `.rdp` 客户端配置文件生成），定义
//!   SQLite 持久化，提供 `/api/v1/forwarding/ssh*` / `/api/v1/forwarding/rdp*`
//!   / `/api/v1/forwarding/stats`（写操作需 admin）。
//! - [`agent_coord`]：`AgentCoordRouteHandler` —— agent 协调组件（设计来自
//!   nexos-test README §2）：`POST /api/v1/agents/register`（幂等注册，触发
//!   协议声明）+ `GET /api/v1/agents`（online 派生/callback 脱敏）+
//!   `GET /api/v1/agents/protocol`（协作协议自举）+ `GET /:name/inbox?after=`
//!   （@ 定向投递收件箱增量）+ `POST /:name/ack` + `DELETE /:name`。IM 群消息
//!   @ 命中注册 agent 即定向投递（在线 WS / 离线收件箱+webhook）；经进程级
//!   钩子 `agent_coord::on_im_message` 与 im.rs 消息路径一行挂钩，
//!   声明消息经 `ImCoordBridge` 桥直插 im_messages。
//! - [`agenthub`]：`AgentHubRouteHandler` —— 「Agent 集合」桌面应用 REST 入口：
//!   常用 AI coding agent（OpenCode / OpenClaw / Claude Code / Codex / Gemini
//!   CLI / Qwen Code / Aider / Goose / Crush）目录 + 一键安装/卸载后台任务
//!   （npm/script/uv/cargo 真实 spawn，npm 前缀不可写自动 sudo，退出码回写
//!   任务 + log_tail）+ `command -v` 已装探测 + 工具链可用性（node/npm/uv/
//!   cargo/curl）+ 自定义 agent 发布（JSON 原子持久化 `NEXOS_AGENTHUB_FILE`）
//!   + 工具链手动安装（[`agenthub_toolchain`] 子模块：node/uv/cargo 用户态
//!     安装器，202 异步任务 + 环形日志轮询，中国镜像优先）。
//!     提供 `/api/v1/agenthub/agents[/:id]` / `installed` / `toolchains` /
//!     `POST install|uninstall` / `tasks[/:id]` / `POST publish` /
//!     `DELETE published/:id` / `stats` / `POST toolchain/install` /
//!     `GET toolchain/install/tasks/:id` / `POST web/:agentId/start|stop` +
//!     `GET web/:agentId/status`（web 描述符标注的 agent 一键开 Web 界面，
//!     读公开 / 写 admin）。
//! - [`api_market`]：`ApiMarketRouteHandler` —— API 大厅（推理服务市场）REST 入口：
//!   把推理服务端点挂牌成商品（消费者查价格/服务器配置/实时负载），提供
//!   `POST /api/v1/api-market/publish`（链上 token，本地硬件探测 + body 覆盖，
//!   无 admin 回落）/ `GET /api/v1/api-market[?q=&sort=recent|price]`（公开，
//!   价格升序免费垫底）/ `GET|DELETE /api/v1/api-market/:id`（详情公开；下架
//!   仅 owner pubkey）/ `POST /:id/heartbeat`（owner 自报负载）/
//!   `GET /:id/metrics`（心跳优先 → metrics_url 代拉 → 降级 unreachable）。

//! - [`update`]：`UpdateRouteHandler` —— 「更新」桌面应用 REST 入口：
//!   当前版本/通道/A-B 槽位视图（os-update `SlotManager` 内存态）+ 更新源
//!   检查（NexHub 裸仓库 `/tank/git-repos/nexos.git` 的 git tag → semver
//!   比较 → 通道过滤）+ 更新任务状态机（pending→downloading→verifying→
//!   writing→reboot_pending→done；开发期真实镜像下载/写槽不执行，预留
//!   通道语义见 docs/UPDATE_APP.md）。提供 `/api/v1/update/status` /
//!   `channels` / `POST channel|check|apply` / `tasks[/:id]` / `history`
//!   （读公开 / 写 admin；通道与任务 JSON 原子持久化）。
//!
//! - [`identity`]：`IdentityRouteHandler` —— 身份账本（os-identity 组件，
//!   2026-08-25 从 os-p2p 抽离）REST 观察面：`GET /api/v1/identity/records`
//!   （全量身份记录：verified/unverified 地址集 + 冲突 + 失配事件）、
//!   `GET /api/v1/identity/addr/:addr`（地址归属查询：owner、verified 状态、
//!   归属记录）、`GET /api/v1/identity/conflicts`（同 NodeID 多地址观测，
//!   与 /api/v1/p2p/identity-conflicts 同源同形）。账本由 main.rs 建持久化
//!   共享实例（`NEXOS_IDENTITY_FILE`，缺省 `/tank/os-data/identity-ledger.json`）
//!   注入 os-p2p（指纹事实事件唯一落点）并自留本 handler（写读同一实例）。
//! - [`network_exit`]：`NetworkExitRouteHandler` —— WAN 出口共享 + 防火墙基础
//!   （component=network-exit，docs/NETWORK_EXIT_RELAY.md）：overlay 级出口节点
//!   （出口声明 digest `exit_offered` 位 + 逐节点 TTL 授权默认 deny + 入口本地
//!   SOCKS5 11081 → overlay net_exit 消息 → 出口本机拨 11080 代拨，v2ray 客户端
//!   模式经自有加密 overlay）；防火墙规则模型持久化 + iptables 自定义链
//!   `NEXOS-FW`/`NEXOS-FW-OUT` 落地（flush 先行），端点 /api/v1/net-exit/* 与
//!   /api/v1/firewall/*（读公开/写 admin，deny-22 防呆需 force；net-exit 未启
//!   用 P2P 时 503，防火墙照常）。
//! - [`terminal`]：`TerminalRouteHandler` —— 「管理」桌面应用（Web 终端）
//!   REST 入口：本地 shell（`$SHELL` 于 PTY）/ SSH 远程终端（`ssh -tt` 于
//!   PTY，密码提示透传，provisioning SSH targets 只读复用）会话生命周期，
//!   `/api/v1/terminal/sessions*` 三端点全 admin + `GET /api/v1/terminal/
//!   node-snapshot` 节点状态快照（admin，管理页顶部状态条聚合：版本/
//!   uptime/P2P 连接数/磁盘/内存）；WS `/ws/terminal/:id`
//!   JSON 帧协议（input/output/resize/exit/error）见 http.rs 与
//!   docs/ADMIN_CONSOLE.md；输出 50ms 聚合节流、会话上限 8（429）、
//!   空闲 30 分钟自动回收。
pub mod agent_coord;
pub mod agenthub;
/// 工具链手动安装（node/uv/cargo 用户态安装器）：agenthub.rs 的子模块（路由
/// 挂进 AgentHubRouteHandler 的 routes()，202 异步任务 + 环形日志轮询），见
/// `agenthub_toolchain.rs` 模块头与 docs/AGENT_HUB.md。
pub mod agenthub_toolchain;
pub mod api_gateway;
pub mod api_market;
/// 应用包运行时（manifest.json + web/ 静态资源的 git 仓库包）：安装/卸载/
/// 已装清单/catalog 扫描 NexHub `nexos-app-*` 裸仓库 + `/apps-assets/:id/*`
/// 静态托管（SQLite apps 表持久化）；film 等引擎门控共享 `AppRegistry`
/// 实例（未装应用 → 引擎业务端点 404），docs/APPS.md。
pub mod apps_handler;
pub mod app_store;
pub mod backup;
/// 能力快照端点（`GET /api/v1/capabilities`，读公开）：@nexos/app-sdk 的
/// 服务端数据面——秒回聚合 llm/api_gateway/api_market/p2p/apps 的既有
/// 内存态与缓存（零主动探测），见 `capabilities.rs` 模块头与
/// docs/APPS.md「应用 SDK」章。
pub mod capabilities;
pub mod ble_hub;
pub mod blockchain;
/// 节点运行管理（geth/bitcoind 真实子进程生命周期 + 空间预检）：blockchain.rs
/// 的子模块（路由挂进 BlockchainRouteHandler 的 routes()，SQLite 持久化 +
/// 进程表 + 状态修正），见 `blockchain_nodes.rs` 模块头与
/// docs/BLOCKCHAIN_NODES.md。
pub mod blockchain_nodes;
pub mod cloudsync;
pub mod compute;
pub mod containers;
pub mod devdocs;
pub mod discover;
pub mod downloads;
pub mod files;
/// 影片制作管线（LibTV 风格 AI 影片工厂）：项目 CRUD + 六阶段任务（分镜/关键
/// 帧/图生视频/配音/BGM/ffmpeg 合成），每阶段独立选模型来源（本地 vLLM/
/// sd-turbo 或经网关渠道转发含 via_node 中继），docs/FILM_STUDIO.md。
pub mod film;
/// FilmHub 流程引擎（2026-09-06，film.rs 超 5000 行的拆分模块——共享
/// FilmCtx / FilmRouteHandler / 任务框架；story→storyboard→定妆→BGM→生成主线）。
pub mod film_hub;
pub mod forwarding;
pub mod identity;
pub mod im;
pub mod llm;
/// 推理环境（vLLM Python venv）管理：llm.rs 的子模块（路由挂进 LlmRouteHandler
/// 的 routes()，共享 llm.db 连接），见 `llm_envs.rs` 模块头与 docs/LLM_ENVIRONMENTS.md。
pub mod llm_envs;
/// 外部 API 接入（OpenAI 兼容端点登记/连通测试/对话直通）：llm.rs 的子模块
/// （路由挂进 LlmRouteHandler 的 routes()，共享 llm.db 连接；流式直通由
/// http.rs 特挂路由消费同一状态），见 `llm_external.rs` 模块头与
/// docs/LLM_EXTERNAL_APIS.md。
pub mod llm_external;
pub mod media;
pub mod media_gen;
pub mod model_hub;
pub mod monitor;
pub mod network;
pub mod network_exit;
pub mod nexhub_ci;
pub mod nexhub_cli;
pub mod node_view;
pub mod notes;
pub mod p2p;
pub mod power;
pub mod provisioning;
pub mod qr_transfer;
pub mod share;
pub mod storage;
pub mod streaming;
pub mod surveillance;
pub mod system;
pub mod terminal;
pub mod transfer;
pub mod update;
pub mod user;

pub use agent_coord::AgentCoordRouteHandler;
pub use agenthub::AgentHubRouteHandler;
pub use api_gateway::{ApiGatewayRouteHandler, ForwardPlan};
pub use api_market::ApiMarketRouteHandler;
pub use app_store::AppStoreRouteHandler;
pub use apps_handler::{AppRegistry, AppsRouteHandler};
pub use backup::BackupRouteHandler;
pub use capabilities::{CapabilitiesRouteHandler, CapabilitySources, RealSources};
pub use ble_hub::BleHubRouteHandler;
pub use blockchain::BlockchainRouteHandler;
pub use cloudsync::CloudSyncRouteHandler;
pub use compute::ComputeRouteHandler;
pub use containers::ContainersRouteHandler;
pub use devdocs::DevDocsRouteHandler;
pub use discover::DiscoverRouteHandler;
pub use downloads::DownloadsRouteHandler;
pub use files::FilesRouteHandler;
pub use film::FilmRouteHandler;
pub use forwarding::ForwardingRouteHandler;
pub use identity::IdentityRouteHandler;
pub use im::{ImAuth, ImFedIngest, ImFederation, ImRouteHandler};
pub use llm::LlmRouteHandler;
pub use media::MediaRouteHandler;
pub use media_gen::MediaGenRouteHandler;
pub use model_hub::ModelHubRouteHandler;
pub use monitor::MonitorRouteHandler;
pub use network::NetworkRouteHandler;
pub use network_exit::{
    ExitConfig, ExitService, ExitState, FirewallManager, FirewallRule, NetworkExitRouteHandler,
    ENTRY_SOCKS_PORT_DEFAULT, EXIT_SOCKS_PORT_DEFAULT, FW_CHAIN_IN, FW_CHAIN_OUT,
};
pub use nexhub_ci::NexhubCiRouteHandler;
pub use nexhub_cli::NexhubCliRouteHandler;
pub use node_view::NodeViewRouteHandler;
pub use notes::NotesRouteHandler;
pub use p2p::{fed_broadcast, FederationBridge, P2pLobbyTransport, P2pRouteHandler};
pub use power::PowerRouteHandler;
pub use provisioning::ProvisioningRouteHandler;
pub use qr_transfer::QrTransferRouteHandler;
pub use share::ShareRouteHandler;
pub use storage::StorageRouteHandler;
pub use streaming::StreamingRouteHandler;
pub use surveillance::SurveillanceRouteHandler;
pub use system::SystemRouteHandler;
pub use terminal::{
    ClientFrame, ServerFrame, TerminalRouteHandler, TerminalSessionInfo, TerminalSessions,
};
pub use transfer::TransferRouteHandler;
pub use update::UpdateRouteHandler;
pub use user::UserRouteHandler;
pub mod live;
pub mod tips;
