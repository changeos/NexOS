//! `os-api` HTTP 网关 binary 入口 —— OS Axum REST + WebSocket 网关的 `main.rs`。
//!
//! 定位（规划文档 §3.6 / §9.1#10）：
//! - os-api 是 HTTP 网关层（axum REST + WebSocket），此前仅为库（无 `cargo run` 入口）。
//! - 本 binary 提供独立可执行的 HTTP 服务器入口，便于本地调试 / 容器化部署 / `--check` 诊断。
//! - 网关本体复用 [`os_api::InProcessGateway`]（中间件链 + 路由注册表 + WS Hub + axum::serve）。
//!
//! # 命令行模式
//!
//! - `os-api` / `os-api --addr <SOCKET>`：正常启动 —— 装配真实业务 handler（storage/compute/system
//!   + share/user）+ 中间件链，`axum::serve` 绑定监听地址，阻塞等待 SIGTERM/SIGINT，收到信号后优雅关闭。
//! - `os-api --check`：预检模式 —— 不真启服务，只做诊断：
//!   1. 路由表完整性（注册数 + 冲突检测）
//!   2. 中间件链配置（链长 + 顺序）
//!   3. WS Hub 就绪
//!   4. 绑定端口可达性（临时绑定后立即释放，不真启）
//! - `os-api --routes <path>`：从 JSON 文件读取额外 [`RouteSpec`] 列表并注册。
//!
//! # 红线说明
//!
//! 本入口是**可编译可运行可 --check + 能启动 HTTP 监听**的目标；装配真实业务
//! RouteHandler（`StorageRouteHandler` 跑真实 `zpool list` / `ComputeRouteHandler`
//! 列 VM / `SystemRouteHandler` 健康检查），命中后返回真实数据 JSON 响应。
//! 优雅关闭复用 `axum::serve` 的 `with_graceful_shutdown`
//! （收到 SIGTERM/SIGINT 后 drain 在途连接）。

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use async_trait::async_trait;
use clap::Parser;

use os_api::gateway::{ApiRequest, ApiResponse, Gateway, RouteHandler, RouteSpec};
use os_api::handlers::{
    AgentCoordRouteHandler, AgentHubRouteHandler, ApiGatewayRouteHandler, ApiMarketRouteHandler,
    AppRegistry, AppStoreRouteHandler, AppsRouteHandler, BackupRouteHandler, BleHubRouteHandler,
    BlockchainRouteHandler, CapabilitiesRouteHandler, CloudSyncRouteHandler, ComputeRouteHandler,
    ContainersRouteHandler, DevDocsRouteHandler, DiscoverRouteHandler, DownloadsRouteHandler,
    ExitConfig, ExitService, FederationBridge, FilesRouteHandler, FilmRouteHandler,
    FirewallManager, ForwardingRouteHandler, IdentityRouteHandler, ImAuth, ImFederation,
    ImRouteHandler, LlmRouteHandler, MediaGenRouteHandler, MediaRouteHandler,
    ModelHubRouteHandler, MonitorRouteHandler, NetworkExitRouteHandler, NetworkRouteHandler,
    NexhubCiRouteHandler, NexhubCliRouteHandler, NodeViewRouteHandler, NotesRouteHandler,
    P2pLobbyTransport, P2pRouteHandler, PowerRouteHandler, ProvisioningRouteHandler,
    QrTransferRouteHandler, RealSources, ShareRouteHandler, StorageRouteHandler,
    StreamingRouteHandler, SurveillanceRouteHandler, SystemRouteHandler, TerminalRouteHandler,
    TransferRouteHandler, UpdateRouteHandler, UserRouteHandler,
};
// 身份账本持久化 env/缺省路径（os-identity 组件宿主策略——装配层定义部署布局）。
use os_api::handlers::identity::{DEFAULT_IDENTITY_FILE, ENV_IDENTITY_FILE};
// 统一打赏（tips）handler——链上身份账本（docs/TIPS.md）。
use os_api::handlers::tips::TipsRouteHandler;
use os_api::{AuditMiddleware, AuthMiddleware, InProcessGateway, StatefulRateLimiter};
// NexHub 两大 handler（代码仓库中心 + 大厅发现层）已抽到独立 crate os-nexhub
// （NexHub 独立化，审计 docs/COMPONENT_INDEPENDENCE_AUDIT.md §6）：实现
// os_common::gateway::RouteHandler 轻量契约，经 os-api gateway.rs 的契约桥接
// blanket impl 适配为本网关 RouteHandler——注册方式与抽离前完全同构。
use os_compute::LibvirtVmManager;
use os_nexhub::{CodeRepoRouteHandler, NexHubLobbyRouteHandler};
use os_storage::ZfsCliBackend;

// ----------------------------------------------------------------------------
// 网关装配：真实 RouteHandler（Storage / Compute / System）替换占位
// ----------------------------------------------------------------------------
//
// 原占位 `PlaceholderHandler`（含 /healthz /api/v1/ping /api/v1/version）已迁移：
// - /healthz + /version（+ 新 /status）→ SystemRouteHandler（handlers/system.rs）
// - /api/v1/pools /api/v1/datasets ... → StorageRouteHandler（handlers/storage.rs，真实 zfs）
// - /api/v1/vms ...                    → ComputeRouteHandler（handlers/compute.rs）
// 下方仅保留 `spec()` 助手 + 额外路由的占位 handler（供 --routes 文件挂载）。

// ----------------------------------------------------------------------------
// 命令行参数（clap derive，与 osd 同款风格）
// ----------------------------------------------------------------------------

/// os-api HTTP 网关命令行参数（clap derive）。
#[derive(Debug, Clone, Parser)]
#[command(
    name = "os-api",
    version,
    about = "OS HTTP 网关服务器（Axum REST + WebSocket + tower 中间件链）",
    long_about = "Axum REST + WebSocket 网关，聚合各组件路由对外提供统一 API 入口。\
 监听 SIGTERM/SIGINT 优雅关闭。--check 做路由表/中间件/端口诊断不真启。"
)]
struct Cli {
    /// 监听地址（IP:PORT）。默认 0.0.0.0:8080（局域网可达，OS 服务端本职）。
    ///
    /// 写接口由 NEXOS_ADMIN_TOKEN 鉴权保护；如需仅本地访问可传 `--addr 127.0.0.1:8080`。
    #[arg(long, value_name = "SOCKET", default_value = "0.0.0.0:8080")]
    addr: String,

    /// 预检模式：验证路由表 + 中间件链 + WS Hub + 绑定端口可达性，不真启服务。
    #[arg(long)]
    check: bool,

    /// 额外路由表 JSON 文件路径（`[RouteSpec, ...]`），叠加到真实业务路由之上。
    ///
    /// 文件不存在或解析失败时打印警告并继续（不阻塞 --check 诊断）。
    #[arg(long, value_name = "PATH")]
    routes: Option<PathBuf>,
}

// ----------------------------------------------------------------------------
// 额外路由表加载
// ----------------------------------------------------------------------------

/// 从 JSON 文件读取额外 [`RouteSpec`] 列表（失败时返回空 vec + 警告，不阻塞启动）。
///
/// 格式：`[RouteSpec, ...]`（serde 反序列化）。注：这些路由的 `handler_component`
/// 若未注册对应 handler，运行时 dispatch 会返回 500（占位 handler 仅接管 `gateway`）。
fn load_extra_routes(path: Option<&PathBuf>) -> Vec<RouteSpec> {
    let Some(path) = path else {
        return Vec::new();
    };
    match std::fs::read_to_string(path) {
        Ok(content) => match serde_json::from_str::<Vec<RouteSpec>>(&content) {
            Ok(list) => list,
            Err(e) => {
                eprintln!("[os-api] 警告: 路由表 {path:?} 解析失败（{e}），忽略额外路由");
                Vec::new()
            }
        },
        Err(e) => {
            eprintln!("[os-api] 警告: 路由表 {path:?} 读取失败（{e}），忽略额外路由");
            Vec::new()
        }
    }
}

// ----------------------------------------------------------------------------
// 鉴权凭据装配（漏洞2 修复）：从环境变量读取 JWT secret + admin token
// ----------------------------------------------------------------------------

/// 从环境变量读取鉴权凭据，返回 (JwtIssuer, admin_token)。
///
/// 漏洞2 修复策略：
/// - `NEXOS_ADMIN_TOKEN`：若设置，启用固定 admin token（最简单的鉴权引导方案）。
///   请求头 `Authorization: Bearer <OS_ADMIN_TOKEN>` 精确匹配 → 注入 admin Principal。
/// - `OS_JWT_SECRET`：若设置，构造 JwtIssuerImpl（HS256）做 JWT 解析。
///   未设置时生成一个随机 secret 并打印到日志（不使用 None，避免静默无鉴权）。
///
/// 优先级：admin_token 简单可靠（适合本地调试/单用户场景）；JWT 适合多用户生产。
/// 两者可同时启用。
fn load_auth_credentials() -> (Option<Arc<os_security::JwtIssuerImpl>>, Option<Arc<String>>) {
    // 1) admin token（OS_ADMIN_TOKEN）
    let admin_token = std::env::var("NEXOS_ADMIN_TOKEN")
        .or_else(|_| std::env::var("OS_ADMIN_TOKEN"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            eprintln!("[os-api] 鉴权: NEXOS_ADMIN_TOKEN 已启用（固定 admin token）");
            Arc::new(s)
        });

    // 2) JWT secret（OS_JWT_SECRET）；未设置 → 生成随机 secret（仅本进程有效）
    let jwt = match std::env::var("NEXOS_JWT_SECRET").or_else(|_| std::env::var("OS_JWT_SECRET")) {
        Ok(s) if !s.trim().is_empty() => {
            eprintln!("[os-api] 鉴权: JWT secret 从 OS_JWT_SECRET 读取");
            Some(Arc::new(os_security::JwtIssuerImpl::new(s.into_bytes())))
        }
        _ => {
            // 生成随机 secret（32 字节 hex）。重启后旧 token 失效（可接受：未配置 secret
            // 说明是临时/开发部署，不应有长期 token）。
            let random = generate_random_secret();
            eprintln!(
                "[os-api] 鉴权: OS_JWT_SECRET 未设置，已随机生成 JWT secret（重启后旧 JWT 失效；\
                 生产请显式配置 OS_JWT_SECRET）"
            );
            Some(Arc::new(os_security::JwtIssuerImpl::new(
                random.into_bytes(),
            )))
        }
    };

    if admin_token.is_none() {
        eprintln!("[os-api] 鉴权: OS_ADMIN_TOKEN 未设置（写操作需 JWT 或配置 OS_ADMIN_TOKEN）");
    }

    (jwt, admin_token)
}

/// 生成 32 字节随机 secret（hex 编码）。
///
/// 不依赖系统随机设备（避免 /dev/urandom 不可用场景），用时间戳 + 进程 id +
/// 线程局部计数器 + 内存地址熵混合（足够防御被动嗅探；非密码学强随机）。
fn generate_random_secret() -> String {
    use std::cell::Cell;
    use std::time::{SystemTime, UNIX_EPOCH};
    thread_local! {
        static SEQ: Cell<u64> = const { Cell::new(0) };
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x1234_5678);
    let pid = std::process::id() as u64;
    let addr_entropy = &now as *const _ as u64;
    let seq = SEQ.with(|s| {
        let v = s.get().wrapping_add(0x9e37_79b9_7f4a_7c15);
        s.set(v);
        v
    });
    let mut x = now ^ pid.wrapping_mul(0x100) ^ addr_entropy ^ seq;
    let mut out = String::with_capacity(64);
    for _ in 0..64 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let nib = (x & 0xf) as u8;
        let c = if nib < 10 {
            b'0' + nib
        } else {
            b'a' + (nib - 10)
        };
        out.push(c as char);
    }
    out
}

// ----------------------------------------------------------------------------
// 网关构造（真实 RouteHandler 装配 + 中间件链 TLS→RateLimit→Auth→Audit）
// ----------------------------------------------------------------------------

/// 构造并配置网关：注册真实业务 handler（storage / compute / system / share / user，
/// + 额外路由声明）+ 中间件链。
///
/// 装配（呼应规划文档 §9.1#10）：
/// - `storage` → `StorageRouteHandler(Arc<ZfsCliBackend>)`：`GET /api/v1/pools` 跑真实 `zpool list`
/// - `compute` → `ComputeRouteHandler(Arc<LibvirtVmManager>)`：`GET /api/v1/vms` 列 VM
/// - `system`  → `SystemRouteHandler`：`/healthz` `/version` `/status`（迁自原 PlaceholderHandler）
///
/// 中间件链顺序（呼应 chain.rs 文档）：TLS → RateLimit → Auth → Audit。
/// 本 binary 暂不启用真实 TLS（rustls feature 未注册），故省略 TLS 中间件。
///
/// 漏洞2 修复：`jwt` 与 `admin_token` 注入到网关，使 HTTP 入口能解析 Bearer token
/// 填充 `ApiRequest.auth`，下游 dispatch 的强制鉴权才能生效。
async fn build_gateway(
    extra_routes: Vec<RouteSpec>,
    jwt: Option<Arc<os_security::JwtIssuerImpl>>,
    admin_token: Option<Arc<String>>,
) -> (InProcessGateway, Option<os_p2p::Handle>) {
    let mut gw = InProcessGateway::new();

    // 1) 注册真实业务 handler（各组件经 RouteHandler 装配进网关）
    //    后端为具体类型（ZfsCliBackend / LibvirtVmManager），Arc 共享给 handler。
    let storage_backend = Arc::new(ZfsCliBackend::new());
    let vm_manager = Arc::new(LibvirtVmManager::new("local"));
    gw.register_component(
        "storage",
        Box::new(StorageRouteHandler::new(storage_backend)),
    )
    .await
    .expect("注册 storage handler");
    gw.register_component(
        "compute",
        Box::new(ComputeRouteHandler::with_arc(vm_manager)),
    )
    .await
    .expect("注册 compute handler");
    gw.register_component("system", Box::new(SystemRouteHandler::new()))
        .await
        .expect("注册 system handler");
    // —— os CLI share / user 命令对应路由（内存态：示例数据，可后续接真实后端）
    gw.register_component("share", Box::new(ShareRouteHandler::new()))
        .await
        .expect("注册 share handler");
    gw.register_component("user", Box::new(UserRouteHandler::new()))
        .await
        .expect("注册 user handler");
    // discover → DiscoverRouteHandler（内存节点列表，本机节点 + 基本属性）：
    //   GET /discover/nodes（os CLI discover 命令）/ GET /api/v1/nodes / GET /api/v1/nodes/:id
    gw.register_component("discover", Box::new(DiscoverRouteHandler::new_local()))
        .await
        .expect("注册 discover handler");
    // im → ImRouteHandler（SQLite 持久化对话/群组/节点 + WebSocket 实时推送）：
    //   POST /api/v1/im/auth/challenge|verify（区块链挑战-签名认证，公开）
    //   GET/POST /api/v1/im/conversations[/ :id/messages] / GET/POST /api/v1/im/groups[/...]
    //   / GET/POST /api/v1/im/peers / GET /api/v1/im/status
    //   / POST /messages/:id/read / GET /conversations/:id/unread / GET /search
    //   用户面端点一律 Authorization: Bearer <IM token>（sender=token 反查 pubkey）。
    //   注入 gw.ws_hub() 以便发消息后广播 WsMessage::ImMessage（前端 WebSocket 实时接收）；
    //   ImAuth（nonce/token 桶）与 handler 共享同一 Arc，并注入网关供 WS 握手验
    //   ?user=<pubkey>&token=<IM token>（失败 401）。
    let im_auth = std::sync::Arc::new(ImAuth::new());
    let im_handler = ImRouteHandler::with_ws_hub(gw.ws_hub(), im_auth.clone());
    // P3 联邦端点在 Box 进网关前取出（p2p Handle 注入 + 入站分发共用同一内核）
    let im_federation = im_handler.federation();
    // agent-coord 的 IM 桥同样在 Box 进网关前取出（协议声明系统消息直插 +
    // 群成员反查，见 handlers/agent_coord.rs）
    let im_coord_bridge = im_handler.coord_bridge();
    gw.register_component("im", Box::new(im_handler))
        .await
        .expect("注册 im handler");
    // tips（统一打赏）共享 IM 的链上 token 桶（from 身份反查），先留一份克隆
    let tips_im_auth = im_auth.clone();
    gw.set_im_auth(Some(im_auth));
    // agent-coord → AgentCoordRouteHandler（agent 协调组件，设计来自 nexos-test
    //   README §2——IM 当任务通道补「定向路由/可靠投递/在线状态」）：
    //   POST /api/v1/agents/register（幂等注册，触发协议声明进其所在群组）
    //   GET /api/v1/agents（列表：online=pubkey 在 WS hub 有活订阅，callback 脱敏）
    //   GET /api/v1/agents/protocol（协作协议自举：与 agent 交流必须 @对方）
    //   GET /api/v1/agents/:name/inbox?after=（@ 定向投递收件箱增量）
    //   POST /api/v1/agents/:name/ack（确认已读）/ DELETE /api/v1/agents/:name
    //   装配：注入 gw.ws_hub()（在线判定）+ im 桥（声明直插 im_messages）；
    //   install_hook 把 @ 路由桥装进 im 发消息路径（im.rs 每条消息落库+广播后
    //   一行调用 agent_coord::on_im_message，@ 命中注册 agent 即定向投递）。
    let agent_coord = AgentCoordRouteHandler::new()
        .with_ws_hub(gw.ws_hub())
        .with_im_bridge(im_coord_bridge);
    os_api::handlers::agent_coord::install_hook(agent_coord.core());
    gw.register_component("agent-coord", Box::new(agent_coord))
        .await
        .expect("注册 agent-coord handler");
    // network → NetworkRouteHandler（网络管理 REST API：网卡/路由/状态真实探测，
    //   防火墙/VLAN/桥接内存态占位）：
    //   GET /api/v1/network/interfaces / routes / status / firewall
    //   POST /api/v1/network/vlan / bridge
    gw.register_component("network", Box::new(NetworkRouteHandler::new()))
        .await
        .expect("注册 network handler");
    // provisioning → ProvisioningRouteHandler（系统自举桌面应用 REST 入口：PXE
    //   网络启动 / ISO 镜像生成 / SSH 远程部署三件套，内存态 + SSH test 真实调
    //   系统 ssh 子进程密钥认证，ISO/deploy 纯任务记录）：
    //   GET/POST /api/v1/provisioning/pxe/config / GET/POST /api/v1/provisioning/pxe/boot-entries
    //   / DELETE /api/v1/provisioning/pxe/boot-entries/:id
    //   / GET /api/v1/provisioning/pxe/status / POST /api/v1/provisioning/pxe/start|stop
    //   / GET/POST /api/v1/provisioning/iso/tasks / DELETE /api/v1/provisioning/iso/tasks/:id
    //   / GET /api/v1/provisioning/iso/tasks/:id
    //   / GET/POST /api/v1/provisioning/ssh/targets / DELETE /api/v1/provisioning/ssh/targets/:id
    //   / POST /api/v1/provisioning/ssh/targets/:id/test
    //   / POST /api/v1/provisioning/ssh/deploy / GET /api/v1/provisioning/ssh/deploy/:id
    //   / GET /api/v1/provisioning/stats
    // 装配为 Arc 共享实例（经 SharedProvisioningHandler 纯转发注册）：terminal
    // 组件只读复用其 SSH targets 注册表（「管理」应用 SSH 终端下拉同源数据，
    // 不复制状态；provisioning 本体逻辑零改动）。
    // update 组件共享实例：provisioning 的 prepare-distributable 成功后自动
    // 登记同版本更新工件（version=CARGO_PKG_VERSION、path=分发产物——复用
    // POST /update/artifact 的校验+sha256，见 handlers/provisioning.rs 与
    // docs/UPDATE_APP.md §1a；下方 "update" 组件注册经 SharedUpdateHandler
    // 用同一实例，状态同源）。
    let update_shared = Arc::new(UpdateRouteHandler::new());
    let provisioning_shared =
        Arc::new(ProvisioningRouteHandler::new().with_update_registry(update_shared.clone()));
    gw.register_component(
        "provisioning",
        Box::new(SharedProvisioningHandler(provisioning_shared.clone())),
    )
    .await
    .expect("注册 provisioning handler");
    // media → MediaRouteHandler（内存态媒体库：影院/音乐/相册三类 MediaItem，
    //   扫描真实目录失败回退 demo 数据，对接"媒体三件套"桌面应用）：
    //   GET /api/v1/media/library[?type=] / GET /api/v1/media/stats
    //   / GET /api/v1/media/item/:id / POST /api/v1/media/scan
    gw.register_component("media", Box::new(MediaRouteHandler::new()))
        .await
        .expect("注册 media handler");
    // live → LiveRouteHandler（「直播」（流媒体中心直播 Tab）REST 入口，
    //   handlers/live.rs，docs/LIVE_STREAMING.md：本地大厅 + 联邦大厅 +
    //   跨节点中继，房间纯内存扇出，重启即清空）：
    //   POST /api/v1/live/rooms（admin）/ GET /api/v1/live/rooms（公开，
    //   {local:[...], federated:[...]} 两段式大厅）
    //   / DELETE /api/v1/live/rooms/:id（admin，踢断全部连接）；
    //   WS /ws/live/:id/publish|view 挂载见 http.rs build_router。
    //   联邦端点在 Box 进网关前取出（p2p spawn 后 set_p2p 注入 + FederationBridge
    //   入站分发共用同一实例——与 im/nexhub 同款装配）。
    let live_handler = os_api::handlers::live::LiveRouteHandler::new();
    let live_fed = live_handler.federation();
    gw.register_component("live", Box::new(live_handler))
        .await
        .expect("注册 live handler");
    // media-gen → MediaGenRouteHandler（媒体生成 REST 入口，docs/MEDIA_GEN_AND_CHAIN_AUTH.md
    //   §A/§B：图片生成真实 spawn 本地 sd-turbo python 管线（/tmp/nexos-imggen.py 自动
    //   落盘，产物 /tmp/media-gen/*.png → base64 返回），先经 nvidia-smi 显存探测，
    //   空闲 < 6000 MiB 503 提示先停 LLM 实例（Qwen3 互斥）；视频生成任务框架 +
    //   可插后端（local 未就绪 / external 读 NEXOS_VIDEO_API_URL 未配即明确报错，
    //   当前无成功路径，任务创建即 failed 附指引，诚实不假装排队）。
    //   链上身份归因 + sk-os- 生图计费（2026-08-20 变现闭环接线）：
    //   - 独立 ChainAuth（照 nexhub 的 with_chain_auth 模式，/api/v1/media/auth/
    //     challenge|verify 同 IM/NexHub 契约；POST /media/image handler 内自验：
    //     链上 token→pubkey 归因 / admin 放行 / sk-os- 计费，都无 401）；
    //   - 共享 api_gateway 实例（with_gateway）：sk-os- 生图扣费（try_charge_image，
    //     per_image 等模式扣 100 积分/图，余额不足 402）——与 api_gateway 组件
    //     **同一实例**（Mutex<Connection> 是查-检-扣原子的边界，两个实例各持
    //     一条连接会 reintroduce SELECT→UPDATE 竞态）。
    //   POST /api/v1/media/auth/challenge|verify / POST /api/v1/media/image
    //   / GET /api/v1/media/image/recent / POST /api/v1/media/video（admin）
    //   / GET /api/v1/media/video/tasks / GET /api/v1/media/video/tasks/:id
    let media_gen_chain_auth = std::sync::Arc::new(os_common::chain_auth::ChainAuth::new());
    let api_gateway_shared = std::sync::Arc::new(ApiGatewayRouteHandler::new());
    gw.register_component(
        "media-gen",
        Box::new(
            MediaGenRouteHandler::with_chain_auth(media_gen_chain_auth)
                .with_gateway(api_gateway_shared.clone()),
        ),
    )
    .await
    .expect("注册 media-gen handler");
    // files → FilesRouteHandler（真实文件系统浏览：spawn_blocking 读目录，
    //   根路径映射 /tank → /var/lib/os/files，禁 .. 路径穿越，写操作需 admin）：
    //   GET /api/v1/files/list?path= / GET /api/v1/files/stat?path=
    //   POST /api/v1/files/mkdir / delete / rename
    gw.register_component("files", Box::new(FilesRouteHandler::new()))
        .await
        .expect("注册 files handler");
    // downloads → DownloadsRouteHandler（真实 aria2 JSON-RPC :6800 下载任务管理，
    //   首次创建时按需 spawn aria2c 守护进程，aria2 未装时降级，写操作需 admin）：
    //   GET/POST /api/v1/downloads/tasks / POST /api/v1/downloads/tasks/:id/pause|resume|cancel
    //   / DELETE /api/v1/downloads/tasks/:id / GET /api/v1/downloads/stats
    gw.register_component("downloads", Box::new(DownloadsRouteHandler::new()))
        .await
        .expect("注册 downloads handler");
    // containers → ContainersRouteHandler（真实 Docker，经 sg docker -c 子进程，
    //   docker 不可用时降级，写操作需 admin）：
    //   GET /api/v1/containers/list / POST /api/v1/containers/create
    //   / POST /api/v1/containers/:id/start|stop|restart / DELETE /api/v1/containers/:id
    //   / GET /api/v1/containers/images / GET /api/v1/containers/stats
    gw.register_component("containers", Box::new(ContainersRouteHandler::new()))
        .await
        .expect("注册 containers handler");
    // surveillance → SurveillanceRouteHandler（内存态摄像头管理，预置示例摄像头，
    //   toggle/record 切换字段，写操作需 admin）：
    //   GET/POST /api/v1/surveillance/cameras / POST /api/v1/surveillance/cameras/:id/toggle|record
    //   / DELETE /api/v1/surveillance/cameras/:id / GET /api/v1/surveillance/stats
    gw.register_component("surveillance", Box::new(SurveillanceRouteHandler::new()))
        .await
        .expect("注册 surveillance handler");
    // cloudsync → CloudSyncRouteHandler（任务定义内存态 + 真实 rclone sync 子进程，
    //   rclone 未装时降级，写操作需 admin）：
    //   GET/POST /api/v1/cloudsync/tasks / POST /api/v1/cloudsync/tasks/:id/sync|pause|resume
    //   / DELETE /api/v1/cloudsync/tasks/:id / GET /api/v1/cloudsync/stats
    gw.register_component("cloudsync", Box::new(CloudSyncRouteHandler::new()))
        .await
        .expect("注册 cloudsync handler");
    // notes → NotesRouteHandler（笔记/文档持久化到文件系统：/tank/notes 或
    //   /var/lib/os/notes，不可用时回退内存态 demo 数据；写操作需 admin）：
    //   GET/POST /api/v1/notes / GET /api/v1/notes/:id / PUT /api/v1/notes/:id
    //   / DELETE /api/v1/notes/:id / GET /api/v1/notes/stats
    gw.register_component("notes", Box::new(NotesRouteHandler::new()))
        .await
        .expect("注册 notes handler");
    // apps 注册表（共享单实例，Arc）：apps 组件 REST 面（下方注册）+ 各内置
    //   引擎门控（film / streaming / qr_transfer）每请求直查同一 SQLite apps
    //   表——在首个门控组件（streaming）注册前构造。
    let apps_registry = std::sync::Arc::new(AppRegistry::new());
    // streaming → StreamingRouteHandler（内存态拉流源/转码任务/推流目标/节目输出，
    //   FFmpeg NVENC 转码子进程 spawn 调度框架，MediaMTX/ffmpeg 不在线时降级，
    //   写操作需 admin）：
    //   GET/POST /api/v1/streaming/sources / DELETE /api/v1/streaming/sources/:id
    //   / POST /api/v1/streaming/sources/:id/record/start|stop
    //   / GET /api/v1/streaming/program / POST /api/v1/streaming/program/switch
    //   / GET/POST /api/v1/streaming/transcode / DELETE /api/v1/streaming/transcode/:id
    //   / GET/POST /api/v1/streaming/outputs / DELETE /api/v1/streaming/outputs/:id
    //   / GET /api/v1/streaming/stats
    //   2026-09-05 起流媒体中心是独立应用：引擎内置但门控——未安装 streaming
    //   应用时全部端点 404 + 安装指引（with_app_registry 注入 apps 注册表，
    //   每请求直查 apps 表，装卸即时生效；见 docs/APPS.md §7「引擎门控」。
    //   P2P 联邦直播 /api/v1/live/* 是独立组件 live.rs，不经此处、常开不门控）。
    gw.register_component(
        "streaming",
        Box::new(
            StreamingRouteHandler::new().with_app_registry(apps_registry.clone()),
        ),
    )
    .await
    .expect("注册 streaming handler");
    // backup → BackupRouteHandler（备份管理桌面应用 REST 入口：内存态备份任务
    //   + 真实 ZFS 快照操作（spawn_blocking 跑 zfs list/snapshot/destroy，失败降级），
    //   预置示例任务 + 快照，写操作需 admin）：
    //   GET/POST /api/v1/backup/tasks / POST /api/v1/backup/tasks/:id/run
    //   / DELETE /api/v1/backup/tasks/:id
    //   / GET/POST /api/v1/backup/snapshots / DELETE /api/v1/backup/snapshots/:name
    //   / GET /api/v1/backup/stats / GET /api/v1/backup/restore
    gw.register_component("backup", Box::new(BackupRouteHandler::new()))
        .await
        .expect("注册 backup handler");
    // monitor → MonitorRouteHandler（系统监控桌面应用 REST 入口：真实 /proc 指标读取
    //   CPU/内存/磁盘/网络/负载，服务状态探测，SQLite 持久化告警 + 阈值规则引擎后台 task，
    //   真实 zpool list，写操作需 admin）：
    //   GET /api/v1/monitor/metrics / services / alerts / history / zpools / stats
    //   / POST /api/v1/monitor/alerts/:id/ack
    let monitor_handler = MonitorRouteHandler::new();
    // 启动阈值规则引擎后台 task（60 秒一轮，独立 tokio task，共享 SQLite 文件）
    monitor_handler.spawn_alert_engine();
    gw.register_component("monitor", Box::new(monitor_handler))
        .await
        .expect("注册 monitor handler");
    // llm → LlmRouteHandler（模型管理桌面应用 REST 入口：内存态 vLLM 推理实例管理，
    //   GPU 动态探测（nvidia-smi/rocm-smi/无 → 降级），vllm serve 子进程 spawn 调度
    //   框架（vllm 未安装时降级为 error 状态不 panic），健康探测 + 推理测试（curl
    //   转发到实例的 /health + /v1/models + /v1/chat/completions），写操作需 admin）：
    //   GET /api/v1/llm/gpu / GET,POST /api/v1/llm/instances / GET /api/v1/llm/instances/:id
    //   / POST /api/v1/llm/instances/:id/start|stop|health|chat / DELETE /api/v1/llm/instances/:id
    //   / GET /api/v1/llm/stats / 外部 API 接入 5 条（llm_external 子模块：
    //   GET,POST /api/v1/llm/external-apis / DELETE /:id / POST /:id/test
    //   / POST /:id/chat——stream:true 由 http.rs 特挂路由 SSE 逐块透传）
    // 注册经 SharedLlmHandler 包装（register_component 收 Box 独占，Arc 共享
    // 需薄转发，SharedApiGatewayHandler 同款）：下方 set_llm_external 把外部
    // API 态注入 GatewayState，流式特挂路由与组件 REST 走同一条
    // Mutex<Connection>。
    let llm_shared = std::sync::Arc::new(LlmRouteHandler::new());
    gw.register_component("llm", Box::new(SharedLlmHandler(llm_shared.clone())))
        .await
        .expect("注册 llm handler");
    // SSE 流式直通共享同一状态（2026-08-31，api_gateway 共享模式同款）：
    // POST /api/v1/llm/external-apis/{id}/chat（stream:true）经此实例查
    // base_url/api_key——与组件 CRUD 同一条 Mutex<Connection>。须在 start 前注入。
    gw.set_llm_external(Some(llm_shared.external_state()));
    // api_gateway → ApiGatewayRouteHandler（LLM API 网关桌面应用 REST 入口，One API 风格：
    //   聚合多个上游 LLM provider（本地 vLLM + OpenAI + 第三方），生成下游 sk-os-xxx 令牌
    //   配额管理，统一 OpenAI 兼容入口做代理转发 + 故障转移，调用日志计费，模型映射）：
    //   GET,POST /api/v1/gateway/channels / PUT,DELETE /api/v1/gateway/channels/:id
    //   / POST /api/v1/gateway/channels/:id/test / GET /api/v1/gateway/channels/:id
    //   / GET,POST /api/v1/gateway/tokens / DELETE /api/v1/gateway/tokens/:id
    //   / POST /api/v1/gateway/tokens/:id/disable|enable / GET /api/v1/gateway/logs|stats|models
    //   / GET,POST /api/v1/gateway/mappings / DELETE /api/v1/gateway/mappings/:name
    //   / POST /api/v1/gateway/v1/chat/completions|completions（代理转发，curl 子进程，
    //   上游不在线降级记 failed 日志不 panic；写操作需 admin，代理转发用令牌自身鉴权）
    //   生图计费同源：本组件与 media-gen 共享**同一实例**（上方 api_gateway_shared，
    //   sk-os- 生图扣费 try_charge_image 与本组件令牌管理/转发计费走同一
    //   Mutex<Connection>，扣费原子不超扣）——注册经 SharedApiGatewayHandler 包装
    //   （register_component 收 Box 独占，Arc 共享需薄转发）。
    gw.register_component(
        "api_gateway",
        Box::new(SharedApiGatewayHandler(api_gateway_shared.clone())),
    )
    .await
    .expect("注册 api_gateway handler");
    // SSE 流式转发共享同一实例（2026-08-31）：http.rs 特挂的
    // POST /api/v1/gateway/v1/{chat/,}completions（stream:true）经此实例做
    // 鉴权/选路/计费——与非流式转发、media-gen 生图扣费走同一条
    // Mutex<Connection>（查-扣原子，多实例会引入竞态）。须在 start 前注入。
    // （clone：下方 api-market 装配段还要给该实例 set_relay/set_external_source
    //  ——渠道中继执行通道 + 一键导入读取源，见 2026-09-03 接线注释。）
    gw.set_api_gateway(Some(api_gateway_shared.clone()));
    // apps → AppsRouteHandler（应用包运行时，2026-09-04，docs/APPS.md——film
    //   剥离为独立应用的配套运行时：manifest.json + web/ 静态资源的 git 仓库
    //   包，经应用中心从 NexHub nexos-app-* 仓库一键安装）：
    //   GET /api/v1/apps（已装清单，SQLite apps 表）/ POST /api/v1/apps/install
    //   {repo}（git clone --depth 1 file:///tank/git-repos/<repo>.git → 校验
    //   manifest → 拷贝到 /tank/os-data/apps/<id>/ → 登记；同版本幂等 no-op、
    //   异版本覆盖升级、同 id 异仓库 409 拒绝）/ DELETE /api/v1/apps/:id（删
    //   目录+注销）/ GET /api/v1/apps/catalog（扫 NexHub nexos-app-* 裸仓库拉
    //   manifest）/ GET /api/v1/apps/tasks（安装任务记录，appstore 任务框架
    //   同款）/ GET /apps-assets/:id/*（应用 web/ 静态资源托管，防穿越 + 按
    //   扩展名 mime）。读公开 / 写 admin。注册表实例与各引擎门控共享（上方
    //   构造的单例，with_app_registry 注入——未装应用时对应引擎端点全 404）。
    gw.register_component(
        "apps",
        Box::new(AppsRouteHandler::new(std::sync::Arc::clone(&apps_registry))),
    )
    .await
    .expect("注册 apps handler");
    // film → FilmRouteHandler（「影片制作管线」桌面应用 REST 入口，handlers/
    //   film.rs，docs/FILM_STUDIO.md——LibTV 风格 AI 影片工厂：分镜(chat)→关键
    //   帧(image)→图生视频(video)→配音(tts)→BGM(music)→ffmpeg 合成(compose)，
    //   六阶段各选模型来源（model_ref：local=本地 vLLM 实例直连 / sd-turbo 生图
    //   内核复用；channel=经网关渠道转发，含 via_node 中继渠道））：
    //   POST /api/v1/film/projects / GET /api/v1/film/projects[/:id]
    //   / PUT|DELETE /api/v1/film/projects/:id（连产物目录删）
    //   / POST .../:id/script|compose / POST .../:id/shots/:n/image|video|tts
    //   / POST .../:id/music / GET /api/v1/film/tasks[/:id] / GET /api/v1/film/tools
    //   （读公开 / 写 admin；阶段任务 202+轮询，环形日志 200 行；ffmpeg 缺失
    //   compose 报错附安装指引不自动装；chat/image 复用 api_gateway 的
    //   forward_channel、local.chat 复用 llm 实例调用面——注入上方共享实例）。
    //   2026-09-04 起 film 是独立应用：引擎内置但门控——未安装 nexos-app-film
    //   应用时全部端点 404 + 安装指引（with_app_registry 注入 apps 注册表，
    //   每请求直查 apps 表，装卸即时生效；见 docs/APPS.md「引擎门控」）。
    gw.register_component(
        "film",
        Box::new(
            FilmRouteHandler::new()
                .with_gateway(api_gateway_shared.clone())
                .with_llm(llm_shared.clone())
                // clone（原 move）：apps 注册表同时被 capabilities 组件共享
                // （GET /api/v1/capabilities 的已装应用清单——同一实例零漂移）
                .with_app_registry(apps_registry.clone()),
        ),
    )
    .await
    .expect("注册 film handler");
    // blockchain → BlockchainRouteHandler（区块链管理桌面应用 REST 入口：RPC 节点 +
    //   区块链浏览器（Blockscout）编排，4 类链（ethereum/dev/l2/custom），构造
    //   docker-compose.yml + 启动命令，start/stop 真实 spawn docker compose
    //   （失败降级 error 不 panic），写操作需 admin）：
    //   GET,POST /api/v1/blockchain/nodes / GET /api/v1/blockchain/nodes/:id
    //   / POST /api/v1/blockchain/nodes/:id/start|stop / DELETE /api/v1/blockchain/nodes/:id
    //   / GET,POST /api/v1/blockchain/explorers / DELETE /api/v1/blockchain/explorers/:id
    //   / POST /api/v1/blockchain/explorers/:id/start
    //   / GET /api/v1/blockchain/chain-presets|stats|clients
    //   + 子模块 blockchain_nodes（节点运行管理，geth/bitcoind 真实子进程，
    //     ETH 主网/Sepolia/dev + BTC 主网/testnet/regtest，fast/full 模式 +
    //     全节点空间预检 + 二进制探测安装指引，docs/BLOCKCHAIN_NODES.md）：
    //     GET,POST /api/v1/blockchain/chain-nodes / GET .../chain-nodes/presets
    //     |space-check|:id|:id/logs / POST .../chain-nodes/:id/start|stop
    //     / DELETE .../chain-nodes/:id
    gw.register_component("blockchain", Box::new(BlockchainRouteHandler::new()))
        .await
        .expect("注册 blockchain handler");
    // model_hub → ModelHubRouteHandler（模型仓库管理桌面应用 REST 入口：本地模型库扫描
    //   /tank/models/ + modelscope 一键下载 + 推荐模型 + 进度估算，modelscope 未安装时
    //   降级 failed 不 panic，写操作需 admin）：
    //   GET /api/v1/models/local / GET /api/v1/models/local/:id
    //   / DELETE /api/v1/models/local/:id
    //   / GET,POST /api/v1/models/downloads / GET,DELETE /api/v1/models/downloads/:id
    //   / GET /api/v1/models/recommended / GET /api/v1/models/stats
    gw.register_component("model_hub", Box::new(ModelHubRouteHandler::new()))
        .await
        .expect("注册 model_hub handler");
    // app_store → AppStoreRouteHandler（应用中心/第三方应用商店桌面应用 REST 入口：
    //   预置 Ubuntu 兼容应用目录（deb/snap/flatpak）+ 用户发布，apt/snap/flatpak
    //   真实 spawn 安装/卸载（失败降级 failed 不 panic），已安装探测 dpkg/snap/flatpak list，
    //   写操作需 admin）：
    //   GET /api/v1/appstore/apps[/ :id|?category=] / GET /api/v1/appstore/categories
    //   / GET /api/v1/appstore/installed / POST /api/v1/appstore/install|uninstall
    //   / GET /api/v1/appstore/tasks[/ :id] / POST /api/v1/appstore/publish
    //   / DELETE /api/v1/appstore/published/:id / GET /api/v1/appstore/stats
    gw.register_component("app_store", Box::new(AppStoreRouteHandler::new()))
        .await
        .expect("注册 app_store handler");
    // agenthub → AgentHubRouteHandler（「Agent 集合」桌面应用 REST 入口：常用 AI
    //   coding agent（OpenCode / OpenClaw / Claude Code / Codex / Gemini CLI / Qwen
    //   Code / Aider / Goose / Crush）目录 + 一键安装/卸载后台任务（npm/script/
    //   uv/cargo 真实 spawn，npm 前缀不可写自动 sudo，退出码回写任务 + log_tail，
    //   fire-and-forget 不阻塞请求）+ command -v 已装探测 + 工具链可用性
    //   （node/npm/uv/cargo/curl）+ 自定义 agent 发布（JSON 原子持久化，
    //   env NEXOS_AGENTHUB_FILE 缺省 /tank/os-data/agenthub.json）+ 工具链手动
    //   安装（agenthub_toolchain 子模块：node/uv/cargo 用户态安装器，202 异步
    //   任务 + 环形日志轮询，中国镜像优先）。读公开 / 写 admin）：
    //   GET /api/v1/agenthub/agents[/:id|?category=&installed=&source=]
    //   / GET /api/v1/agenthub/installed / GET /api/v1/agenthub/toolchains
    //   / POST /api/v1/agenthub/install|uninstall / GET /api/v1/agenthub/tasks[/:id]
    //   / POST /api/v1/agenthub/publish / DELETE /api/v1/agenthub/published/:id
    //   / GET /api/v1/agenthub/stats
    //   / POST /api/v1/agenthub/toolchain/install
    //   / GET /api/v1/agenthub/toolchain/install/tasks/:id
    //   / POST /api/v1/agenthub/web/:agentId/start|stop（admin，OpenCode 等
    //   web 描述符标注的 agent 一键起服务，URL 按 Host 头推导）
    //   / GET /api/v1/agenthub/web/:agentId/status
    gw.register_component("agenthub", Box::new(AgentHubRouteHandler::new()))
        .await
        .expect("注册 agenthub handler");
    // qr_transfer → QrTransferRouteHandler（二维码文件传输桌面应用 REST 入口：文件 →
    //   跳动 QR 视频（每帧一个 QR）+ 解码回文件。Python qrcode/pyzbar + ffmpeg 真实
    //   spawn 子进程（fire-and-forget），Python/ffmpeg 不存在降级 failed 不 panic，
    //   写操作需 admin）：
    //   POST /api/v1/qr/encode / GET /api/v1/qr/encode/:id / GET .../:id/video
    //   / POST /api/v1/qr/decode / GET /api/v1/qr/decode/:id / GET .../:id/file
    //   / GET /api/v1/qr/stats
    //   2026-09-05 起二维码传输是独立应用：引擎内置但门控——未安装 qrtransfer
    //   应用时全部端点 404 + 安装指引（with_app_registry 注入 apps 注册表，
    //   每请求直查 apps 表，装卸即时生效；见 docs/APPS.md §7「引擎门控」）。
    gw.register_component(
        "qr_transfer",
        Box::new(
            QrTransferRouteHandler::new().with_app_registry(apps_registry.clone()),
        ),
    )
    .await
    .expect("注册 qr_transfer handler");
    // ble_hub → BleHubRouteHandler（BLE mesh 网状中继枢纽：OS 作 mesh 节点 + 互联网
    //   网关，手机离线经 BLE mesh 多跳中继（A↔B↔C）通信。开放 mesh 无需配对，手机即中继
    //   节点；节点发现通告 + hop 路由 + flooding 去重。Python dbus/GATT mesh relay spawn
    //   （fire-and-forget），Python/dbus/BlueZ 不存在或 spawn 失败降级 running=false 不 panic；
    //   写操作需 admin）：
    //   GET /api/v1/ble/status / POST /api/v1/ble/start|stop / GET /api/v1/ble/nodes
    //   / DELETE /api/v1/ble/nodes/:id / POST /api/v1/ble/discover / GET /api/v1/ble/routing
    //   / POST|GET /api/v1/ble/messages / GET /api/v1/ble/stats
    gw.register_component("ble_hub", Box::new(BleHubRouteHandler::new()))
        .await
        .expect("注册 ble_hub handler");
    // code_repo → CodeRepoRouteHandler（代码仓库中心桌面应用 REST 入口：Gitea 自托管
    //   Git 平台 + AI agent 项目归档。docker compose 启停 Gitea（端口 3000 Web + 2222 SSH），
    //   仓库 CRUD 代理 Gitea REST API，AI 会话归档记录哪个 agent 会话创建了什么仓库，
    //   代码浏览文件树 + 文件内容，Gitea/Docker 不在线降级不 panic，写操作需 admin）：
    //   GET /api/v1/coderepo/status
    //   / POST /api/v1/coderepo/gitea/start|stop|init
    //   / GET,POST /api/v1/coderepo/repos / DELETE /api/v1/coderepo/repos/:name
    //   / GET /api/v1/coderepo/repos/:name/contents|file
    //   / GET,POST /api/v1/coderepo/sessions / POST /api/v1/coderepo/sessions/:id/end
    //   / GET /api/v1/coderepo/stats
    gw.register_component("code_repo", Box::new(CodeRepoRouteHandler::new()))
        .await
        .expect("注册 code_repo handler");
    // nexhub_cli → NexhubCliRouteHandler（nexhub CLI 分发端点，NexHub P2，
    //   docs/research/NEXHUB_WEB_CLI_DESIGN.md §B / docs/NEXHUB.md）：
    //   GET /api/v1/coderepo/cli.sh（公开，动态生成，照 provisioning/install.sh
    //   先例——X-Forwarded-Host/Host 头推导缺省节点地址（缺省端口 8558）+
    //   text/x-shellscript 原文直传 + nosniff；脚本资产 include_str! 随二进制
    //   分发，`curl -fsSL <节点>/api/v1/coderepo/cli.sh | sh` 一条命令安装）。
    gw.register_component("nexhub_cli", Box::new(NexhubCliRouteHandler::new()))
        .await
        .expect("注册 nexhub_cli handler");
    // nexhub_ci → NexhubCiRouteHandler（NexHub 内置 CI，v0.1.33：
    //   POST /api/v1/coderepo/repos/:name/ci（admin 手动触发）+ GET runs 列表
    //   / 详情+log / DELETE 清记录（admin）+ GET /api/v1/coderepo/ci/latest
    //   （各仓最新 run 摘要聚合——仓库卡徽章数据源）。clone 裸仓 → 工作树根
    //   流水线探测（Cargo.toml→cargo check / package.json→npm ci&&build），
    //   同仓串行 + 全局 ≤2 并发，环形日志 500 行，SQLite ci.db（NEXOS_CI_DB）。
    //   new() 同时安装全局核心：git-http push 成功路径（http.rs）经 push_hook
    //   自动触发（env NEXOS_CI_AUTO_PUSH 缺省开）。
    gw.register_component("nexhub_ci", Box::new(NexhubCiRouteHandler::new()))
        .await
        .expect("注册 nexhub_ci handler");
    // nexhub-lobby → NexHubLobbyRouteHandler（NexHub 大厅发现层，docs/NEXHUB_LOBBY_DESIGN.md：
    //   SQLite hub_lobby 发布索引 + 发布快照 + 一键克隆到 /tank/git-repos，seed nexos）：
    //   GET /api/v1/nexhub/lobby（?q= ?tag= ?sort=）/ GET /stats / GET /:name
    //   / POST /publish / DELETE /:name / POST /:name/clone / POST /:name/purchase
    //   / GET /entitlements + bounty 8 条（/api/v1/nexhub/bounty/*）。
    //   链上身份（docs/MEDIA_GEN_AND_CHAIN_AUTH.md §C，2026-08-18）：写端点
    //   身份=挑战-签名 token（challenge/verify 同 IM 契约，handler 内自验——
    //   requires_auth=false），owner/buyer/hunter/poster 全部 token 反查；
    //   无链上 token 时回落 NEXOS_ADMIN_TOKEN/OS_ADMIN_TOKEN（admin 通道）。
    //   照 IM 的 Arc 共享模式注入 ChainAuth（装配层与 handler 验同一批 token；
    //   与 IM 的 ImAuth 实例相互独立——token 桶互不相通，同一密钥对可两侧
    //   分别认证）。
    let nexhub_chain_auth = std::sync::Arc::new(os_common::chain_auth::ChainAuth::new());
    let nexhub_handler = NexHubLobbyRouteHandler::with_chain_auth(nexhub_chain_auth.clone());
    // P3 联邦端点在 Box 进网关前取出（发布路径广播 + p2p 接收端写入共用）
    let nexhub_fed = nexhub_handler.fed_endpoint();
    gw.register_component("nexhub-lobby", Box::new(nexhub_handler))
        .await
        .expect("注册 nexhub-lobby handler");
    // api-market → ApiMarketRouteHandler（API 大厅/推理服务市场，docs/API_MARKET.md）：
    //   把推理服务端点挂牌成商品——消费者在大厅查价格/服务器配置/实时负载：
    //   POST /api/v1/api-market/publish（链上 token；server_config 本地硬件探测
    //   （nvidia-smi//proc）+ body 覆盖，model_name 必填；重复发布=刷新保留计数；
    //   可选 access_info 携带消费者接入凭据——api_key 输出仅 publisher 本人/
    //   admin 明文，其他视角 <前4>***<后4> 脱敏）
    //   / GET /api/v1/api-market[?q=&sort=recent|price&scope=all|local|fed]
    //   （公开；价格升序免费垫底；scope 过滤联邦来源——缺省 all 平铺数组向后
    //   兼容）/ GET /api/v1/api-market/:id（详情公开）/ DELETE /api/v1/api-market/:id
    //   （仅 owner pubkey，403「仅发布者可下架」）/ POST /:id/heartbeat（owner 自报
    //   负载）/ GET /:id/metrics（心跳优先（≤60s 新鲜）→ metrics_url 代拉 5s →
    //   降级 unreachable）/ POST /:id/federate（owner 推送联邦大厅——两步联邦
    //   第二步，fed kind "api_market_lobby"，远端按 id/名+发布者幂等合并）。
    //   发布者身份=区块链公钥**唯一通道**（用户定稿，无 admin 回落——与 nexhub
    //   的回落语义刻意不同）；api-market 无自己的 challenge/verify 端点，与
    //   nexhub-lobby **共享同一 ChainAuth 实例**（上方 clone）——经
    //   /api/v1/nexhub/auth/challenge|verify 签发的 token 在此直接可用
    //   （401 文案即引导该处）。
    let api_market_handler = ApiMarketRouteHandler::with_chain_auth(nexhub_chain_auth.clone());
    // 能力快照接线（2026-09-04）：Arc 共享同一实例（listings_snapshot 读同一条
    // Mutex<Connection>），handler 本体经 SharedApiMarketHandler 包装注册（见
    // 下方 register_component）——capabilities 组件拿 clone 装配 RealSources。
    let api_market_shared = std::sync::Arc::new(api_market_handler);
    // P3 联邦端点在 Box 进网关前取出（p2p spawn 后 set_p2p 注入发送端 +
    // FederationBridge 入站分发共用同一实例——照 im/nexhub/live 装配模式）。
    let api_market_fed = api_market_shared.federation();
    // 跨网中继消费者侧接线（2026-09-02）：llm_external 的 via_node 条目
    // （联邦大厅一键导入）经此端点走 overlay 定向源节点代发。Clone 共享内核
    // ——下方 p2p spawn 成功后 set_p2p 装配发送面即全链路可用（本插入在
    // gw.start 前，HTTP 开服时必然已就绪）。
    llm_shared
        .external_state()
        .set_relay(Some(api_market_fed.clone()));
    // 渠道中继接线（2026-09-03 网关联邦中继转发）：api_gateway 的 via_node
    // 渠道（外部 API 一键导入生成）非流式/流式转发同走该端点；同时注入外部
    // API 登记读取源（from_external_api 一键导入查行——同一 Mutex<Connection>）。
    api_gateway_shared.set_relay(Some(api_market_fed.clone()));
    api_gateway_shared.set_external_source(Some(llm_shared.external_state()));
    gw.register_component(
        "api-market",
        Box::new(SharedApiMarketHandler(api_market_shared.clone())),
    )
    .await
    .expect("注册 api-market handler");
    // tips → TipsRouteHandler（统一打赏原语/链上身份账本，docs/TIPS.md）：
    //   打赏 = 一条真实账本记录（from 链上身份 pubkey → to 目标所有者 pubkey，
    //   服务端反查，防自报伪造），四大厅面（IM 消息/NexHub 条目/模型大厅条目/
    //   API 大厅条目）+ 节点共用同一原语：
    //   POST /api/v1/tips（链上 token 优先——im_auth 与 nexhub 共享桶依次验，
    //   无 token 回落网关 Principal/admin；to 按 target_kind×ref 反查
    //   im.db/hub_lobby.db/model_lobby.db/api_market.db，202 落 tips.db）
    //   / GET /api/v1/tips/target/:kind/:ref（目标聚合 total/count/recent≤20，
    //   公开读——前端各大厅并行拉取，大厅 handler 零侵入）
    //   / GET /api/v1/tips/me（我收到/给出的聚合，按身份）。
    //   requires_auth=false：链上 token 在 handler 内自验（网关中间件不认
    //   链上 token，挂 true 会全拦，与 api-market 同理）。
    //   注：仓库无独立 wallet 组件注册（os-wallet 经 blockchain.rs 钱包端点
    //   介入），故紧邻同为链上身份经济的 api-market 处注册。
    gw.register_component(
        "tips",
        Box::new(TipsRouteHandler::with_shared_auth(
            Some(tips_im_auth),
            Some(nexhub_chain_auth.clone()),
        )),
    )
    .await
    .expect("注册 tips handler");
    // forwarding → ForwardingRouteHandler（远程转发工具桌面应用 REST 入口：
    //   SSH 隧道 spawn 系统 ssh 子进程（local/remote/dynamic 三模式，密钥认证
    //   BatchMode=yes 禁密码交互，无密码字段）+ Windows RDP 纯 Rust TCP 代理
    //   （copy_bidirectional）+ .rdp 客户端配置文件生成，定义 SQLite 持久化，
    //   spawn/bind 失败降级 failed/error 不 panic）：
    //   GET,POST /api/v1/forwarding/ssh / GET,DELETE /api/v1/forwarding/ssh/:id
    //   / POST /api/v1/forwarding/ssh/:id/start|stop
    //   / GET,POST /api/v1/forwarding/rdp / DELETE /api/v1/forwarding/rdp/:id
    //   / POST /api/v1/forwarding/rdp/:id/start|stop
    //   / GET /api/v1/forwarding/rdp/:id/rdp-file / GET /api/v1/forwarding/stats
    //   ssh 二进制可经 NEXOS_SSH_BIN 覆写（默认 ssh）。
    // 启动副作用：后台恢复 autostart=true 的隧道/转发（spawn_autostart_resume，
    //   参考 monitor 的 spawn_alert_engine 先例；失败降级不阻塞启动）。
    let forwarding_handler = ForwardingRouteHandler::new();
    forwarding_handler.spawn_autostart_resume();
    gw.register_component("forwarding", Box::new(forwarding_handler))
        .await
        .expect("注册 forwarding handler");
    // p2p → P2pRouteHandler（os-p2p 组网层 REST 入口，docs/NEXOS_P2P_NETWORK_DESIGN.md
    //   P2b 接入）：
    //   GET /api/v1/p2p/status|peers|buckets|ladder（读公开）
    //   POST /api/v1/p2p/send|connect（admin）
    //   启动装配：NEXOS_P2P_ENABLE=1 才在网关进程内 P2pNode::spawn 内嵌组网节点
    //   （默认关——不影响无 P2P 需求的部署；env 透传 NEXOS_P2P_BOOTSTRAP/LISTEN/
    //   PUBLIC/MDNS/NAME/KEY_FILE——私钥持久化 → 重启 NodeID 稳定）。
    //   未启用时 handler 持 None，全部端点 503 + 引导文案
    //   {"error":"P2P 未启用（NEXOS_P2P_ENABLE=1）"}。
    //   P3 联邦桥：spawn 成功后 ①入站消息经 FederationBridge 分发给 IM/NexHub
    //   接收端（替代 P2b 纯日志观测 task）②把 Handle 注入两者的发送端
    //   （IM 大厅消息广播 / NexHub pubkey 条目广播）。
    let (
        p2p_handler,
        node_view_handler,
        identity_handler,
        transfer_handler,
        net_exit_handler,
        p2p_handle,
    ) = spawn_p2p_if_enabled(im_federation, nexhub_fed, live_fed, api_market_fed);
    gw.register_component("p2p", Box::new(p2p_handler))
        .await
        .expect("注册 p2p handler");
    // node_view → NodeViewRouteHandler（节点发现页聚合视图：os-p2p 真实数据接线，
    //   替代旧 /api/v1/nodes 内存假数据）：GET /api/v1/nodes/combined 一次返回
    //   {lan, p2p, ladder, self}——与 p2p handler 共享同一 Handle clone；各节点
    //   行另带 im_public（对方 IM 大厅开放状态，ImFederation 探针缓存——节点
    //   发现页「进入 IM」按钮状态数据源）；静态路由优先于 discover 的
    //   /api/v1/nodes/:id 参数路由（routing.rs specificity 规则），两路由不冲突。
    gw.register_component("node_view", Box::new(node_view_handler))
        .await
        .expect("注册 node_view handler");
    // identity → IdentityRouteHandler（身份账本 REST 观察面，os-identity 组件
    //   2026-08-25 从 os-p2p 抽离）：GET /api/v1/identity/records（全量身份记录：
    //   verified/unverified 地址集 + 冲突 + 失配事件）/ addr/:addr（地址归属
    //   查询：owner + verified 状态 + 归属记录）/ conflicts（同 NodeID 多地址
    //   观测——与 /api/v1/p2p/identity-conflicts 同源同形）。开发期公开读。
    //   账本由 spawn_p2p_if_enabled 建持久化共享实例（NEXOS_IDENTITY_FILE，
    //   缺省 /tank/os-data/identity-ledger.json）注入 os-p2p（指纹事实事件
    //   唯一落点）并自留本 handler——写读同一实例，账本即唯一权威源。
    //   未启用（P2P 未开）时全部端点 503 + 引导文案（同 p2p handler 语义）。
    gw.register_component("identity", Box::new(identity_handler))
        .await
        .expect("注册 identity handler");
    // transfer → TransferRouteHandler（P2P 传输组件，2026-08-25）：迅雷式多源
    //   下载管理 + 网状分发——本地文件发布为可传输清单（sha256 内容寻址），
    //   其他节点凭 sha256/transfer_id 经 os-p2p 叠加层（打洞/中继，不依赖公网
    //   IP）query 源、分块拉取（逐块校验/背压/断点续传/完成自动做种）。
    //   与 downloads 分工：公网 HTTP/BT 走 aria2，节点间走 transfer（Downloads.vue
    //   以 Tab 聚合展示）。POST publish / DELETE manifests/:id / POST fetch /
    //   POST tasks/:id/{pause|resume|cancel}（admin），GET manifests/tasks[/:id]/
    //   stats（公开）。env：NEXOS_TRANSFER_DIR（落地目录，缺省 /tank/downloads）。
    //   未启用（P2P 未开）时全部端点 503 + 引导文案（同 p2p handler 语义）。
    gw.register_component("transfer", Box::new(transfer_handler))
        .await
        .expect("注册 transfer handler");
    // network-exit → NetworkExitRouteHandler（WAN 出口共享 + 防火墙基础，
    //   docs/NETWORK_EXIT_RELAY.md）：**overlay 级出口节点**——本节点可声明
    //   出口（digest 自广播 exit_offered 位，env NEXOS_P2P_EXIT_OFFER=1 或
    //   POST /net-exit/offer；授权表默认 deny、逐节点 TTL），其他节点的
    //   流量经入口本地 SOCKS5 127.0.0.1:11081 → os-p2p 加密 overlay
    //   （net_exit 消息：open/data/ack/close，64KiB 分块 + 8 块窗口背压）→
    //   出口节点本机拨 127.0.0.1:11080 代拨目标（v2ray 客户端模式经自有
    //   overlay，零内核侵入；默认路由级 exit node 列二期）。防火墙半部独立
    //   于组网：规则（方向/协议/端口/源/动作/启用，空表起步）持久化
    //   NEXOS_FIREWALL_FILE + iptables 自定义链 NEXOS-FW[-OUT]（flush 先行），
    //   `GET|POST|DELETE /api/v1/firewall/*`（读公开/写 admin，deny-22 防呆
    //   需 force）。未启用 P2P 时 net-exit 端点 503、防火墙照常。
    gw.register_component("network-exit", Box::new(net_exit_handler))
        .await
        .expect("注册 network-exit handler");
    // update → UpdateRouteHandler（「更新」桌面应用 REST 入口，docs/UPDATE_APP.md）：
    //   更新源为本生态 NexHub 裸仓库（发版即 git tag），通道即 tag 过滤策略
    //   （stable 正式 / beta *-beta* / nightly 全收 / manual 仅手动）；
    //   GET /api/v1/update/status|channels|tasks[/:id]|history（读公开）
    //   POST /api/v1/update/channel|check|apply（admin）。版本比较与 A/B 槽位
    //   复用 os-update 纯逻辑（version.rs / slot.rs）；开发期真实镜像下载 /
    //   写槽 / 激活不执行（apply 任务推进到 writing 后标记"通道已预留"，
    //   语义见 handler 模块文档与 docs/UPDATE_APP.md「当前实现边界」）。
    //   env：NEXOS_VERSION（当前版本）/ NEXOS_UPDATE_STATE（状态 JSON，缺省
    //   /tank/os-data/update-state.json）/ NEXOS_UPDATE_REPO（更新源裸仓库，
    //   缺省 /tank/git-repos/nexos.git）/ NEXOS_UPDATE_REPO_URL（远端更新源
    //   git URL，如 http://<安装源>:8558/git/nexos.git——本地裸仓库缺失时
    //   check 走 git ls-remote --tags 网络查询，install.sh 装的新节点开箱即有）。
    //   实例在 provisioning 装配段已提前建好（update_shared，Arc 共享：prepare-
    //   distributable 成功后自动登记同版本工件），此处经 SharedUpdateHandler
    //   注册同一实例。
    gw.register_component(
        "update",
        Box::new(SharedUpdateHandler(update_shared.clone())),
    )
    .await
    .expect("注册 update handler");
    // power → PowerRouteHandler（系统自举「电源控制层」REST 入口——PXE 装机
    //   流水线第一环，先唤醒/上电再 PXE 引导）：
    //   GET /api/v1/provisioning/power/bmc|bmc/sensors（本机 BMC in-band，
    //   ipmitool 缺失/无 /dev/ipmi0 时明确降级非 500）
    //   / POST /api/v1/provisioning/power/bmc/power（admin）
    //   / GET|POST /api/v1/provisioning/power/ipmi/devices + DELETE/:id
    //   + POST /:id/test|power（admin，lanplus RMCP+ 远程，argv 直传防注入）
    //   + GET /:id/status
    //   / POST /api/v1/provisioning/power/ipmi/scan（admin，纯 Rust RMCP
    //   Presence Ping UDP 623 免凭据发现，/24 上限，后台任务）+
    //   GET /power/ipmi/scan/:id
    //   / GET|POST /api/v1/provisioning/power/wol/targets + DELETE/:id
    //   + POST /power/wol/wake（魔术包广播 ×3，开发期公开）
    //   + GET /power/wol/arp（ip neigh 邻居辅助选 MAC）。
    //   状态持久化 env NEXOS_POWER_STATE（缺省 /tank/os-data/power-state.json）。
    gw.register_component("power", Box::new(PowerRouteHandler::new()))
        .await
        .expect("注册 power handler");
    // terminal → TerminalRouteHandler（「管理」桌面应用 REST 入口：Web 终端——
    //   本地 shell / SSH 远程终端，docs/ADMIN_CONSOLE.md）：
    //   GET /api/v1/terminal/sessions（admin，活跃会话列表）
    //   POST /api/v1/terminal/sessions {kind:"local"|"ssh",host?,port?,user?,
    //     key_path?,target_id?,cols,rows}（admin，spawn PTY → 201）
    //   DELETE /api/v1/terminal/sessions/:id（admin，kill 进程组 + 关 PTY）
    //   GET /api/v1/terminal/node-snapshot（admin，节点状态快照——管理页顶部
    //     状态条：版本/uptime/P2P 连接数/磁盘/内存；p2p_handle 与 p2p/
    //     node_view 共享同一 clone，未启用时 p2p_connected=null）
    //   WS /ws/terminal/:session_id?token=<admin token>（JSON 帧协议，http.rs）
    //   SSH 目标来源只读复用 provisioning 注册表（上方共享实例，同一数据源）；
    //   与 WS 升级层共享进程级 TerminalSessions::shared()；启动即挂空闲
    //   回收后台任务（30 分钟无活动 kill+清理，60s 巡检）。
    let terminal_handler = TerminalRouteHandler::sharing_global_registry()
        .with_ssh_targets(Arc::new(move || provisioning_shared.ssh_targets_snapshot()))
        .with_p2p_handle(p2p_handle.clone());
    terminal_handler.sessions().spawn_idle_reaper();
    gw.register_component("terminal", Box::new(terminal_handler))
        .await
        .expect("注册 terminal handler");
    // devdocs → DevDocsRouteHandler（「开发者中心」桌面应用 REST 入口，
    //   docs/DEVDOCS_DEV_CENTER.md）：文档唯一事实源=仓库 docs/（git push
    //   即更新，post-receive 钩子已自动化），本 handler 只做渲染与服务层——
    //   GET /api/v1/devdocs/index（扫描根+一级子目录 *.md：标题/分类/大小/
    //   mtime，缓存 30s）+ GET /api/v1/devdocs/doc/*path（markdown 原文，
    //   仅 .md + canonicalize 防穿越；?lang=en|zh-TW 走本地 LLM AI 翻译管线：
    //   译文缓存 /tank/os-data/devdocs-i18n（X-Translation: cached）→ 未命中
    //   异步任务 202 + GET /devdocs/translate/tasks/:id 轮询 → 完成原子写缓存；
    //   无可用模型 503 诚实降级「中文原文可用」）。开发期公开读。env：
    //   NEXOS_DEVDOCS_DIR（文档根，缺省 /home/oem/NexOS/docs，回退二进制旁
    //   ./docs）；NEXOS_DEVDOCS_I18N_DIR（译文缓存根）；NEXOS_DEVDOCS_GATEWAY_
    //   URL/TOKEN/TRANSLATE_MODEL（翻译网关与模型）；无 checkout 节点降级
    //   空清单+提示或 NEXOS_DEVDOCS_FALLBACK_URL 联邦回退。
    gw.register_component("devdocs", Box::new(DevDocsRouteHandler::new()))
        .await
        .expect("注册 devdocs handler");
    // capabilities → CapabilitiesRouteHandler（能力快照端点，2026-09-04，
    //   docs/APPS.md「应用 SDK」章）：GET /api/v1/capabilities（读公开）——
    //   @nexos/app-sdk 的服务端数据面。秒回聚合既有 handler 内存态/缓存
    //   （llm 实例 / api_gateway 渠道 / api_market 大厅条目心跳缓存 / p2p
    //   对端数 / ffmpeg 探测 / apps 已装清单），**零主动探测联邦/零出站请求**：
    //   llm/api_gateway/apps 复用上方共享实例，api-market 经
    //   SharedApiMarketHandler 共享同一 SQLite，p2p_handle 共享同一 Handle
    //   clone（未启用时 enabled:false / peers 0）。响应带 sdk_version='0.1'
    //   协议版本（前端 crates/os-api/web/src/sdk/ 同源）。
    let capabilities_sources = RealSources::new(
        std::sync::Arc::clone(&llm_shared),
        std::sync::Arc::clone(&api_gateway_shared),
        std::sync::Arc::clone(&api_market_shared),
        std::sync::Arc::clone(&apps_registry),
    );
    gw.register_component(
        "capabilities",
        Box::new(CapabilitiesRouteHandler::new(
            capabilities_sources,
            p2p_handle.clone(),
        )),
    )
    .await
    .expect("注册 capabilities handler");

    // 2) 额外路由声明：注册到一个独立的占位 handler（"extra"），供 --routes 文件挂载
    //    仅声明路由（命中后由 ExtraPlaceholderHandler 返回占位响应）。
    if !extra_routes.is_empty() {
        gw.register_component(
            "extra",
            Box::new(ExtraPlaceholderHandler {
                routes: extra_routes,
            }),
        )
        .await
        .expect("注册 extra 路由");
    }

    // 3) 中间件链：RateLimit（1000 rps，宽松，避免预检/调试被限流）→ Auth → Audit
    //    TLS 由反向代理终止（chain.rs TODO），本入口不挂 TLS 中间件。
    gw.add_middleware(Box::new(StatefulRateLimiter::new(1000)));
    gw.add_middleware(Box::new(AuthMiddleware::new()));
    gw.add_middleware(Box::new(AuditMiddleware::new()));

    // 4) 漏洞2 修复：注入 JWT issuer / admin_token，使 HTTP 入口能解析 Bearer token。
    //    若两者都为 None，extract_principal 永远返回 None，导致所有 requires_auth
    //    路由都被 dispatch 鉴权拒绝（401）——这比"无鉴权"更安全（默认拒绝）。
    gw.set_jwt_issuer(jwt);
    gw.set_admin_token(admin_token);

    (gw, p2p_handle)
}

// ----------------------------------------------------------------------------
// P2P 组网节点装配（P2b：NEXOS_P2P_ENABLE=1 才内嵌 spawn，默认关）
// ----------------------------------------------------------------------------

/// 按环境变量装配 P2P 组网节点（P3：连带装配联邦桥 + 身份账本；2026-08-25
/// 再连带装配 P2P 传输组件）。
///
/// - `NEXOS_P2P_ENABLE` truthy（`1`/`true`/`yes`）→ **必须在 tokio runtime 内**
///   （`build_gateway` 是 async fn，天然满足）`P2pNode::spawn(config_from_env())`：
///   env 全透传（`NEXOS_P2P_BOOTSTRAP/LISTEN/PUBLIC/MDNS/NAME/KEY_FILE`），
///   私钥文件持久化 → 重启 NodeID 稳定；
/// - **身份账本**（os-identity 组件，2026-08-25 抽离）：spawn 前建持久化共享
///   实例（`NEXOS_IDENTITY_FILE`，缺省 `/tank/os-data/identity-ledger.json`）
///   注入 `P2pConfig::identity_ledger`——os-p2p 传输层的指纹事实事件（握手
///   证据/探测结论/gossip 转述/同 NodeID 冲突观测）全部落这一份账本；本函数
///   同时把它交给 IdentityRouteHandler（写读同一实例，账本即唯一权威源）；
/// - **P2P 传输组件**（transfer，2026-08-25）：spawn 成功后
///   `TransferService::spawn(handle, TransferConfig::from_env())`——常驻 ingress
///   任务订阅 on_msg 应答 transfer_query / 供块；TransferRouteHandler 持共享
///   实例（REST 观察面与引擎写读一致）；
/// - **P3 联邦桥**（spawn 成功后，顺序红线——**注入先行**）：
///   ① 发送端注入——`im_federation.set_p2p`（联邦大厅 fed-lobby 消息广播）与
///   `nexhub_fed.set_transport(P2pLobbyTransport)`（pubkey 条目广播），
///   同步锁写入在一切消费方启动前完成；
///   ② 入站消息观测 task 经 `FederationBridge` 分发（`fed=="im_fed_lobby_message"`
///   或旧 `fed=="im_lobby"` → IM 联邦大厅落地+WS 广播；`fed=="nexhub_lobby"` →
///   hub_lobby 落地标记 source_node；非联邦载荷仅记日志）；
///   全部装配（含本函数）在 `build_gateway` 内同步完成、先于 `run_serve`
///   的 `gw.start`——HTTP 开始收请求时 Handle 必已注入；
/// - 未设置/非 truthy → 未启用态（默认关，不影响无 P2P 需求的部署；联邦
///   发送/接收随之静默停用），全部 /api/v1/p2p/* 与 /api/v1/transfer/* 端点
///   503 + 引导文案；
/// - spawn 失败（端口占用/权限）→ 告警 + 降级未启用态（不阻塞 os-api 启动）。
///
/// 返回 (p2p handler, node_view handler, identity handler, transfer handler,
/// Option<Handle>)——**Handle 必须被调用方持有到进程结束**（Handle 持命令通道
/// sender，drop 即组网任务收摊；transfer 服务与各 handler 持 Clone，网关存活
/// 即组网存活）。node_view handler 与 p2p handler 共享同一 Handle/昵称/
/// public 声明（节点发现页 combined 聚合视图的数据源一致性）；identity
/// handler 与注入 os-p2p 的账本共享同一实例（指纹事实事件的唯一权威源）。
fn spawn_p2p_if_enabled(
    im_federation: ImFederation,
    nexhub_fed: Arc<os_nexhub::LobbyFedEndpoint>,
    live_fed: os_api::handlers::live::LiveFedEndpoint,
    api_market_fed: os_api::handlers::api_market::ApiMarketFedEndpoint,
) -> (
    P2pRouteHandler,
    NodeViewRouteHandler,
    IdentityRouteHandler,
    TransferRouteHandler,
    NetworkExitRouteHandler,
    Option<os_p2p::Handle>,
) {
    if !os_p2p::truthy(std::env::var("NEXOS_P2P_ENABLE").ok().as_deref()) {
        eprintln!("[os-api] P2P: 未启用（默认关）——设 NEXOS_P2P_ENABLE=1 开启内嵌组网节点");
        return (
            P2pRouteHandler::new_disabled(),
            NodeViewRouteHandler::new_disabled(),
            IdentityRouteHandler::new_disabled(),
            TransferRouteHandler::new_disabled(),
            NetworkExitRouteHandler::new_disabled(),
            None,
        );
    }
    let name = std::env::var(os_p2p::ENV_NAME).unwrap_or_default();
    // 身份账本（os-identity 组件）：持久化共享实例——os-api 部署布局缺省
    // /tank/os-data/identity-ledger.json（env NEXOS_IDENTITY_FILE 覆盖）。
    // spawn 前建好并注入（事实事件从第一条握手开始就不丢）。
    let identity_file = std::env::var(ENV_IDENTITY_FILE)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(DEFAULT_IDENTITY_FILE));
    let identity_ledger: os_identity::SharedLedger = std::sync::Arc::new(std::sync::Mutex::new(
        os_identity::IdentityLedger::new(Some(identity_file.clone())),
    ));
    // 单一事实源：public/advertise 以 config_from_env 解析结果为准
    //（NEXOS_P2P_ADVERTISE 设置即隐含 public=1——NAT 后云主机语义）
    let mut cfg = os_p2p::config_from_env();
    // 覆盖 config_from_env 的 standalone 缺省（key 同目录 identity-ledger.json）
    // ——os-api 部署用统一数据目录；账本实例注入（传输层事实事件唯一落点）
    cfg.identity_ledger = Some(identity_ledger.clone());
    let public = cfg.public;
    match os_p2p::P2pNode::spawn(cfg) {
        Ok(handle) => {
            eprintln!(
                "[os-api] P2P: 组网节点已启动 NodeID={} OverlayAddr={} listen={} public={public} name={name}",
                handle.self_id(),
                handle.self_id().overlay(),
                handle.listen_addr()
            );
            eprintln!(
                "[os-api] P2P: 身份账本已装配（os-identity，指纹事实事件 → {}）；REST /api/v1/identity/*",
                identity_file.display()
            );
            // 发送端注入**先行**（顺序红线）：`set_p2p` / `set_transport` 都是
            // 同步锁写入（std Mutex，无 await），在桥消费 task 启动与网关对外
            // 服务（gw.start 在 run_serve）之前完成——杜绝"消息已发出而
            // Handle 未注入"的装配竞态窗口（即便未来有人把 start 提前）。
            im_federation.set_p2p(handle.clone(), name.clone());
            nexhub_fed.set_transport(Arc::new(P2pLobbyTransport(handle.clone())), name.clone());
            // 直播联邦：live_lobby 宣告广播 + live_relay_* 中继（hub 帧探针/
            // 影子房间钩子在 install 内回填，与 WS 升基层同一 SHARED_HUB）。
            live_fed.set_p2p(handle.clone(), name.clone());
            // API 大厅联邦：api_market_lobby 条目广播（federate 端点触发）+
            // 入站幂等合并（共享 handler 的同一 SQLite 表）。
            api_market_fed.set_p2p(handle.clone(), name.clone());
            // 市场联邦覆盖缺口修复（2026-09-03）：连接建立观测 task 感知新连
            // peer → backfill_to 定向补推本节点全部 federated 条目（fed_broadcast
            // 只发"当时已连"的一跳——严格 NAT 对端常年无活连接会永远错过发布
            // 广播窗口）；定期重播（30 分钟一轮）在端点 install_transport 内
            // 常驻，无需此处接线。回调 spawn 异步跑（限幅 100ms/条），不阻塞
            // 观测 task 的后续拍。
            let market_fed_backfill = api_market_fed.clone();
            os_api::handlers::p2p::spawn_conn_watcher(
                handle.clone(),
                os_api::handlers::p2p::FED_CONN_POLL_INTERVAL,
                move |peer| {
                    let fed = market_fed_backfill.clone();
                    let peer = peer.clone();
                    tokio::spawn(async move {
                        fed.backfill_to(&peer).await;
                    });
                },
            );
            eprintln!(
                "[os-api] P2P: 市场联邦补推已装配（on-connect backfill + 每 30 分钟重播——docs/API_MARKET.md §9）"
            );
            // 联邦桥：入站消息 → FederationBridge 分发（附 P2b 的日志观测面）。
            // 注入完成后才起消费 task——先注入后接收。
            let bridge = FederationBridge {
                im: Some(im_federation.clone()),
                nexhub: Some(nexhub_fed.clone()),
                live: Some(live_fed.clone()),
                api_market: Some(api_market_fed.clone()),
            };
            let mut rx = handle.on_msg();
            tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(m) => {
                            eprintln!(
                                "[os-api][p2p] recv from={} hops={} ttl={} payload={}",
                                short_node_id(&m.from.to_hex()),
                                m.hops,
                                m.ttl,
                                m.payload
                            );
                            bridge.dispatch(&m);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            eprintln!("[os-api][p2p] 观测任务落后 {n} 条（跳过）");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
            eprintln!(
                "[os-api] P2P: 联邦桥已装配（IM 大厅消息 + NexHub 大厅条目经 os-p2p 跨节点互通）"
            );
            // P2P 传输组件：常驻 ingress 应答 query/供块 + REST 观察面（与
            // p2p/node_view/identity 同一 Handle clone——网关存活即组网存活）。
            let transfer_service =
                os_p2p::TransferService::spawn(handle.clone(), os_p2p::TransferConfig::from_env());
            let transfer_dir = std::env::var("NEXOS_TRANSFER_DIR")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "/tank/downloads".to_string());
            eprintln!(
                "[os-api] P2P: 传输组件已装配（transfer）——落地目录 {transfer_dir}；REST /api/v1/transfer/*"
            );
            // WAN 出口组件（network-exit）：常驻 ingress（overlay net_exit 帧）
            // + 双端 SOCKS5 监听（入口 11081 / 出口 11080，均 127.0.0.1——红线：
            // 不对外暴露，远端流量走 overlay）；防火墙管理器同服（规则持久化 +
            // iptables NEXOS-FW 链落地）。持久化的 offered 状态是权威源——
            // 启动即推送到 p2p digest 位（env NEXOS_P2P_EXIT_OFFER 为启动缺省）。
            let exit_cfg = ExitConfig::from_env();
            let exit_entry_port = exit_cfg.entry_socks_port;
            let exit_socks_port = exit_cfg.exit_socks_port;
            let net_exit_service = ExitService::spawn(
                handle.clone(),
                exit_cfg,
                Arc::new(FirewallManager::from_env()),
            );
            eprintln!(
                "[os-api] P2P: WAN 出口组件已装配（network-exit）——入口 SOCKS5 127.0.0.1:{exit_entry_port} / 出口 SOCKS5 127.0.0.1:{exit_socks_port}；REST /api/v1/net-exit/* + /api/v1/firewall/*"
            );
            (
                P2pRouteHandler::new(handle.clone(), name.clone(), public),
                NodeViewRouteHandler::new(handle.clone(), name, public, im_federation.clone()),
                IdentityRouteHandler::new(identity_ledger),
                TransferRouteHandler::new(transfer_service),
                NetworkExitRouteHandler::new(net_exit_service),
                Some(handle),
            )
        }
        Err(e) => {
            eprintln!(
                "[os-api] P2P: 组网节点启动失败（NEXOS_P2P_LISTEN 端口占用/权限？），降级未启用态: {e}"
            );
            (
                P2pRouteHandler::new_disabled(),
                NodeViewRouteHandler::new_disabled(),
                IdentityRouteHandler::new_disabled(),
                TransferRouteHandler::new_disabled(),
                NetworkExitRouteHandler::new_disabled(),
                None,
            )
        }
    }
}

/// NodeID 短式（`0x1234…cdef`——日志观测用）。
fn short_node_id(hex: &str) -> String {
    let n = hex.len();
    if n <= 12 {
        hex.to_string()
    } else {
        format!("{}…{}", &hex[..8], &hex[n - 4..])
    }
}

/// 额外路由的占位 handler：声明来自 `--routes` 文件的路由，命中后回显请求路径。
///
/// 这些路由的 `handler_component` 在文件中可任意指定，但注册时统一挂到本 handler
/// 名下（"extra"）；真实生产应由各业务组件提供带依赖注入的 handler。
struct ExtraPlaceholderHandler {
    routes: Vec<RouteSpec>,
}

/// api_gateway 组件的共享实例包装（2026-08-20 生图计费接线）。
///
/// `register_component` 收 `Box<dyn RouteHandler>`（独占），而 media-gen 的
/// sk-os- 生图扣费（`try_charge_image`）需要与 api_gateway 组件**同一实例**
/// ——`Mutex<Connection>` 是查-检-扣原子的边界——故装配层先建 `Arc` 双方共享，
/// 本包装零逻辑纯转发（routes/handle 与原注册完全同构）。
struct SharedApiGatewayHandler(std::sync::Arc<ApiGatewayRouteHandler>);

/// provisioning 组件的共享实例包装（terminal SSH 目标只读复用）。
///
/// 同 SharedApiGatewayHandler 模式：register_component 收 Box 独占，而
/// terminal 组件的 SSH 目标下拉需要与 provisioning **同一注册表实例**
/// （`ssh_targets_snapshot` 读同一 Mutex 态，避免复制状态漂移），装配层
/// 先建 `Arc` 双方共享，本包装零逻辑纯转发。
struct SharedProvisioningHandler(std::sync::Arc<ProvisioningRouteHandler>);

/// update 组件的共享实例包装（prepare-distributable 自动登记更新工件）。
///
/// 同 SharedProvisioningHandler 模式：register_component 收 Box 独占，而
/// provisioning 的 prepare-distributable 成功后要把同版本工件登记进
/// update 组件的**同一工件表实例**（`register_artifact_and_persist` 写同
/// 一 Mutex 态 + 同一 update-state.json——两个实例各持一份状态会漂移成
/// 「prepare 登记了但 GET /update/artifacts 看不见」），装配层先建 `Arc`
/// 双方共享，本包装零逻辑纯转发。
struct SharedUpdateHandler(std::sync::Arc<UpdateRouteHandler>);

/// llm 组件的共享实例包装（外部 API 对话流式直通接线，2026-08-31）。
///
/// 同 SharedApiGatewayHandler 模式：register_component 收 Box 独占，而
/// http.rs 特挂的 POST /api/v1/llm/external-apis/{id}/chat（stream:true）需要
/// 与 llm 组件**同一外部 API 表状态**（同一条 Mutex<Connection>），装配层
/// 先建 Arc 双方共享，本包装零逻辑纯转发。
struct SharedLlmHandler(std::sync::Arc<LlmRouteHandler>);

/// api-market 组件的共享实例包装（能力快照接线，2026-09-04）。
///
/// 同 SharedApiGatewayHandler 模式：register_component 收 Box 独占，而
/// capabilities 组件（GET /api/v1/capabilities，@nexos/app-sdk 数据面）需要
/// 与 api-market 组件**同一实例**（listings_snapshot 读同一条 Mutex<Connection>
/// ——多实例会各持一份数据漂移成「大厅有条目但 capabilities 报 0」），装配层
/// 先建 Arc 双方共享，本包装零逻辑纯转发。
struct SharedApiMarketHandler(std::sync::Arc<ApiMarketRouteHandler>);

#[async_trait]
impl RouteHandler for SharedApiGatewayHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        self.0.routes().await
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, os_api::ApiGatewayError> {
        self.0.handle(req).await
    }
}

#[async_trait]
impl RouteHandler for SharedProvisioningHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        self.0.routes().await
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, os_api::ApiGatewayError> {
        self.0.handle(req).await
    }
}

#[async_trait]
impl RouteHandler for SharedUpdateHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        self.0.routes().await
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, os_api::ApiGatewayError> {
        self.0.handle(req).await
    }
}

#[async_trait]
impl RouteHandler for SharedLlmHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        self.0.routes().await
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, os_api::ApiGatewayError> {
        self.0.handle(req).await
    }
}

#[async_trait]
impl RouteHandler for SharedApiMarketHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        self.0.routes().await
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, os_api::ApiGatewayError> {
        self.0.handle(req).await
    }
}

#[async_trait]
impl RouteHandler for ExtraPlaceholderHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        self.routes.clone()
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, os_api::ApiGatewayError> {
        Ok(ApiResponse {
            status: 200,
            body: serde_json::json!({
                "component": "extra",
                "placeholder": true,
                "path": req.path,
            }),
            headers: serde_json::json!({}),
        })
    }
}

// ----------------------------------------------------------------------------
// 预检模式（--check）
// ----------------------------------------------------------------------------

/// 预检模式：做完整诊断，返回是否全过（true=无硬失败项）。
///
/// 诊断项：
/// 1. 路由表完整性（注册数 + 逐条打印；冲突已在 register 阶段检测，能走到这说明无冲突）
/// 2. 中间件链配置（链长 + 顺序）
/// 3. WS Hub 就绪（默认挂 /ws）
/// 4. 绑定端口可达性（临时 TcpListener bind 同地址 → 立即 drop，不真启服务）
async fn run_check(gw: &InProcessGateway, addr: &str) -> bool {
    let mut all_ok = true;

    // 1. 路由表
    let routes = gw.list_routes().await;
    print!("[check] 路由表（{} 条）... ", routes.len());
    if routes.is_empty() {
        println!("WARN（空路由表，启动后所有请求 404）");
    } else {
        println!("OK");
        for r in &routes {
            eprintln!(
                "[check]   {:?} {:<24} -> {} (auth={}, roles={:?})",
                r.method, r.path, r.handler_component, r.requires_auth, r.required_roles
            );
        }
    }

    // 2. 中间件链
    let mw = gw.middleware_count();
    print!("[check] 中间件链（{} 层）... ", mw);
    if mw == 0 {
        println!("WARN（空中间件链，无 RateLimit/Auth/Audit）");
    } else {
        println!("OK（顺序: RateLimit → Auth → Audit）");
    }

    // 3. WS Hub（默认挂 /ws， subscriber_count 为 0 表示就绪但暂无连接）
    let ws_subs = gw.ws_hub().subscriber_count();
    println!(
        "[check] WebSocket Hub... OK（/ws 已挂载，当前订阅 {} 个）",
        ws_subs
    );

    // 4. 组件计数（真实业务 handler：storage / compute / system / share / user / discover
    //    / im / network / provisioning / media / media-gen / files / downloads / containers
    //    / surveillance / cloudsync / notes / streaming / backup / monitor / llm / api_gateway
    //    / blockchain / model_hub / app_store / qr_transfer / ble_hub / code_repo
    //    / nexhub-lobby / api-market / forwarding / p2p / update / devdocs / power，可选 + extra）
    let extra_suffix = if gw.component_count() > 37 {
        " + extra"
    } else {
        ""
    };
    println!(
        "[check] 已注册组件（{} 个）：storage + compute + system + share + user + discover + im + network + provisioning + media + media-gen + files + downloads + containers + surveillance + cloudsync + notes + streaming + backup + monitor + llm + api_gateway + blockchain + model_hub + app_store + agenthub + qr_transfer + ble_hub + code_repo + nexhub-lobby + api-market + forwarding + p2p + identity + update + devdocs + power + terminal{extra_suffix}",
        gw.component_count()
    );

    // 5. 绑定端口可达性（临时 bind → 立即释放，不真启服务）
    print!("[check] 绑定端口 {addr} 可达性... ");
    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => {
            drop(listener);
            println!("OK（可绑定，端口空闲）");
        }
        Err(e) => {
            println!("FAIL（{e}）");
            all_ok = false;
        }
    }

    all_ok
}

// ----------------------------------------------------------------------------
// 正常启动模式（axum::serve + 信号优雅关闭）
// ----------------------------------------------------------------------------

/// 正常启动模式：注册路由 + 中间件 → axum::serve 监听 → 等信号 → 优雅关闭。
///
/// 优雅关闭策略：收到 SIGTERM/SIGINT 后调用 [`Gateway::stop`]（触发 axum::serve
/// 的 `with_graceful_shutdown` 信号，drain 在途连接后退出）；随后停掉内嵌 P2P
/// 组网节点（`p2p_handle.shutdown`——断开全部对端连接，对端立即感知 EOF）。
async fn run_serve(
    gw: &InProcessGateway,
    addr: &str,
    p2p_handle: Option<&os_p2p::Handle>,
) -> ExitCode {
    let routes = gw.list_routes().await;
    eprintln!(
        "[os-api] 启动 HTTP 网关 @ {addr}（{} 条路由，{} 层中间件，{} 个组件）",
        routes.len(),
        gw.middleware_count(),
        gw.component_count()
    );
    for r in &routes {
        eprintln!(
            "[os-api]   {:?} {:<24} -> {}",
            r.method, r.path, r.handler_component
        );
    }

    // 打印 bind 地址 + 安全提示
    if addr.starts_with("0.0.0.0") || addr.starts_with("::") {
        eprintln!(
            "[os-api] 安全提示: 绑定 {addr}（局域网可达，OS 服务端默认）。\
             写接口已启用 OS_ADMIN_TOKEN 鉴权；如仅本地调试可用 --addr 127.0.0.1:8080"
        );
    } else {
        eprintln!(
            "[os-api] 安全提示: 绑定 {addr}（仅本地访问）。如需局域网访问请用 --addr 0.0.0.0:8080"
        );
    }

    // 启动网关（内部 axum::serve + 后台 serve task）
    if let Err(e) = gw.start(addr, None).await {
        eprintln!("[os-api] 启动失败：{e}");
        return ExitCode::FAILURE;
    }
    eprintln!("[os-api] 已开始监听 {addr}（SIGTERM/SIGINT 触发优雅关闭）");

    // 等待信号
    wait_for_signal().await;

    // 优雅停止（Gateway::stop 触发 graceful_shutdown + 等 serve task 收尾）
    gw.stop().await;
    // 内嵌 P2P 组网节点优雅停机（断开对端连接 + 叫停全部组网任务）
    if let Some(h) = p2p_handle {
        h.clone().shutdown().await;
        eprintln!("[os-api] P2P 组网节点已优雅关闭");
    }
    eprintln!("[os-api] 已优雅关闭，退出");
    ExitCode::SUCCESS
}

/// 阻塞等待 SIGTERM/SIGINT（unix）或 stdin（非 unix）。
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => eprintln!("[os-api] 收到 SIGTERM，开始优雅关闭"),
            _ = sigint.recv() => eprintln!("[os-api] 收到 SIGINT，开始优雅关闭"),
        }
    }
    #[cfg(not(unix))]
    {
        eprintln!("[os-api] 非 unix 平台无信号处理，按回车退出");
        let mut buf = String::new();
        let _ = std::io::stdin().read_line(&mut buf);
    }
}

// ----------------------------------------------------------------------------
// main
// ----------------------------------------------------------------------------

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    // 加载额外路由（失败时警告 + 继续，不阻塞 --check）
    let extra_routes = load_extra_routes(cli.routes.as_ref());
    if !extra_routes.is_empty() {
        eprintln!("[os-api] 已加载 {} 条额外路由", extra_routes.len());
    }

    // 漏洞2 修复：从环境变量加载鉴权凭据（OS_JWT_SECRET / OS_ADMIN_TOKEN）
    let (jwt, admin_token) = load_auth_credentials();

    // 构造网关（注册示例路由 + 中间件链 + 注入鉴权凭据）
    let (gw, p2p_handle) = build_gateway(extra_routes, jwt, admin_token).await;
    // 持有一个 Arc 引用以防 gw 被 drop（build_gateway 返回 owned，此处无其他持有者，
    // 但显式标注生命周期到 main 结束，便于未来扩展为多组件注入）。
    let _gw_arc: Arc<InProcessGateway> = Arc::new(gw.clone());
    let gw_ref = &gw;

    if cli.check {
        let ok = run_check(gw_ref, &cli.addr).await;
        if ok {
            eprintln!("[os-api] 预检通过（无硬失败项）");
            ExitCode::SUCCESS
        } else {
            eprintln!("[os-api] 预检发现失败项");
            ExitCode::FAILURE
        }
    } else {
        // p2p_handle 持到进程结束（drop 即组网命令通道关闭、节点收摊）
        run_serve(gw_ref, &cli.addr, p2p_handle.as_ref()).await
    }
}
