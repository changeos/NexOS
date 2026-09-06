//! `NodeViewRouteHandler` —— 节点发现页聚合视图（LAN / P2P / 非活跃三段 + 本机信息）。
//!
//! # 背景
//!
//! 节点发现页（web Nodes.vue）此前从 `/api/v1/nodes` 拉取 DiscoverRouteHandler
//! 的**内存假数据**（os-discover mock，仅本机一条），既看不到 P2P 组网层
//! （os-p2p）学到的真实邻居，也没有 LAN / P2P 分区。本 handler 把 os-p2p
//! 观察面（`Handle::peers/buckets_summary/ladder_stats/self_id/node_meta`）
//! 聚合成一个面向 UI 的组合端点，一次轮询拿全页数据。
//!
//! # 元数据接入（2026-08-23，os-p2p meta 组件第一批消费者）
//!
//! 架构原则：**节点存活检测只由元数据组件做**——本 handler 不做任何连接性
//! 探测（combined 只读 meta 快照），每个 lan/p2p 条目富化元数据字段：
//!
//! - `status`：`"active"` / `"inactive"`（meta 状态；无条目的直连 peer 默认
//!   active——存活判定未覆盖 ≠ 死亡）；
//! - `score` / `last_seen`：健康分与最近确认存活时刻（无条目时 0）；
//! - `meta_source`：`"direct"`（本机直连观测）/ `"gossip"`（他节点转述）/
//!   `null`（无条目）。
//!
//! **非活跃分组**：meta 判定 `Inactive`（五振出局）的节点单独列在响应的
//! `inactive` 数组（含 addrs/score/last_seen/source/since），**不再出现在
//! lan/p2p**（"移到非活跃节点"语义）；若某 Inactive 节点当前恰好有活连接
//! （理论少见——连接是最强活性证据，meta 引擎下一轮即复活它），以 lan/p2p
//! 为准并在其条目 status 标 active。
//!
//! # 端点契约
//!
//! | 方法 | 路径 | 鉴权 | 语义 |
//! |---|---|---|---|
//! | GET | `/api/v1/nodes/combined` | 公开 | `{lan:[], p2p:[], inactive:[], ladder:{}, self:{}}` 聚合视图 |
//!
//! 与 DiscoverRouteHandler 的 `/api/v1/nodes/:id`（参数路由）不冲突——网关
//! 路由匹配静态段优先于参数段（`routing.rs` specificity 规则），静态路径
//! `/api/v1/nodes/combined` 恒命中本 handler。
//!
//! # im_public 标注（2026-08-23，节点发现页「进入 IM」联动）
//!
//! combined 的每个 lan/p2p 节点行带 `im_public`：对方节点 IM 大厅开放开关
//! （`GET/POST /api/v1/im/lobby/access`，开发期缺省 true）经 P2P 探针
//! （`ImFederation` 内嵌 `ImLobbyProbe`，30s 缓存限频）查询所得——
//! `true` = 允许浏览（按钮可点，跳 `/chat?node=<id>` 远程大厅 Tab）；
//! `false` = 对方未开放（按钮灰）；`null` = 查询在途 / Kademlia 桶短 ID
//! 无从寻址 / P2P 未注入探针。
//!
//! # 分区规则
//!
//! 地址类别决定卡片（分类逻辑不因元数据接入而改变——元数据只富化显示信息），
//! 地址信号取三级（按可信度降序）：
//! 1. **直连观测地址**（`PeerInfo.observed_addr`——当前活跃连接 socket 的
//!    对端 `peer_addr`，握手验证过的第一手信号；仅 connected 时有值）。
//!    **最高优先**：直连私网 → LAN 卡片、直连公网 → P2P 卡片（分类用它裁决）。
//!    端点簿观测可能被公网锚点 gossip 覆盖成 NAT 映射地址，而直连 socket 的
//!    对端地址不可伪造——106↔113 这类同网段直连即便端点簿被污染也据此归 LAN。
//!    **展示地址端口规整（2026-08-25）**：入站观测的 IP 对、端口却是临时源
//!    端口（113 拨入 106 的 `192.0.2.113:49730`）——LAN 卡片展示
//!    `ip:7070`（underlay 优先，兜底 [`os_p2p::P2P_PORT_DEFAULT`]），
//!    见 [`lan_display_addr`]；P2P 卡片不受影响（公网锚点 observed 本就是
//!    监听口 7070）；
//! 2. **通告 underlay**（`PeerInfo.underlay`——仅 public/hub 节点通告可拨地址）；
//! 3. **端点簿观测地址**（`Handle::known_endpoints`——gossip 汇聚的观测地址；
//!    LAN 邻居在此现身：非 public 节点不通告 underlay，但 106↔113 这类
//!    同网段互联的观测地址必是 192.168.x/10.x 私网段。可能滞后/被覆盖，作兜底）。
//!
//! **回环地址一律不输出（2026-08-25 用户定调：「127.0.0.1 无论怎么产生的，
//! 都应该屏蔽」——取代早前「直连回环归 LAN / 非直连回环归 P2P」的分级语义）**：
//! 条目的任一地址信号（直连观测 / 通告 underlay / 端点簿观测）为回环，或其
//! meta 条目地址历史含回环 → lan/p2p/inactive 三组均不出现该条目。这是展示层
//! 双保险——meta 侧源头已封死回环（record_conn 跳过 / push 拒绝 / 加载剔除），
//! 此处兜底：即使注册表漏进回环，发现页也看不到。同机多实例的观测由 os-p2p
//! `register_conn` 的 identity_conflicts 记账承担。
//!
//! - **lan**：直连观测为私网（非回环），或（无直连信号时）上述 2/3 级
//!   任一信号为**局域网地址且非回环**（RFC1918 私网 10/8、172.16/12、
//!   192.168/16 + 链路本地）的对端——含桶内已知地址但未直连的同网段邻居
//!   （灰点）；
//! - **p2p**：直连观测为公网，或公网/无地址信号（NAT 打洞/中继）的
//!   peers + **Kademlia 桶中的非直连远端节点**（"知道存在、尚未直连"）；
//! - **inactive**：meta 注册表判定 Inactive 且无活连接的节点（单独分组，
//!   手动心跳入口见 `POST /api/v1/p2p/node-meta/:id/reactivate`）；
//! - **self**：本机 NodeID/OverlayAddr/昵称/角色（hub=公网服务节点 /
//!   edge=普通节点）/ 监听地址；未启用 P2P 时 `enabled=false` + 字段置空，
//!   仅保留 hostname（本机探测，与 discover.rs 同源逻辑）；
//! - **ladder**：连接阶梯统计（Direct/Punched/Relayed/PunchFailed，P2P 卡片
//!   底部小字数据源）。
//!
//! 桶条目是短 NodeID（`0x1234…cdef`，os-p2p `short_hex` 展示式），与 peers
//! 全量 ID 的去重按"同款短式"比较（见 [`short_hex`]）。

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};

use async_trait::async_trait;
use os_p2p::{
    BucketStat, Handle, LadderStats, MetaSource, MetaState, NodeMetaEntry, PeerInfo,
    P2P_PORT_DEFAULT,
};
use serde::Serialize;

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// Handler 主体
// ----------------------------------------------------------------------------

/// 节点聚合视图路由处理器——节点发现页双卡片（LAN + P2P/WAN）数据源。
///
/// - `Some(handle)`：os-api 启动时 spawn 的内嵌组网节点（main.rs 与
///   `P2pRouteHandler` 共享同一 `Handle` clone）；
/// - `None`：未启用——`lan`/`p2p` 空数组 + `self.enabled=false`（**不是 503**：
///   页面仍可展示本机 hostname 与空态引导文案）。
pub struct NodeViewRouteHandler {
    /// 组网节点句柄（未启用为 None）。
    handle: Option<Handle>,
    /// 节点昵称（`NEXOS_P2P_NAME`；self 展示）。
    name: String,
    /// 公网服务节点声明（`NEXOS_P2P_PUBLIC=1`；self 角色展示）。
    public: bool,
    /// 本机主机名（构造期一次性探测；P2P 未启用时 self 的兜底展示）。
    hostname: String,
    /// IM 联邦端点（大厅开放状态探针——combined 各节点行标注 `im_public`；
    /// 与 ImRouteHandler 共享同一内核，main.rs 装配时传入）。
    im_fed: Option<crate::handlers::im::ImFederation>,
}

impl NodeViewRouteHandler {
    /// 未启用构造（默认部署：`NEXOS_P2P_ENABLE` 未设/为 0）。
    #[must_use]
    pub fn new_disabled() -> Self {
        Self {
            handle: None,
            name: String::new(),
            public: false,
            hostname: crate::handlers::discover::detect_hostname(),
            im_fed: None,
        }
    }

    /// 已启用构造（main.rs 装配：与 P2pRouteHandler 共享同一 Handle + env 元数据；
    /// `im_fed` 与 ImRouteHandler 共享同一 `Arc<ImShared>`——其内嵌的
    /// `ImLobbyProbe` 缓存供 combined 标注各节点大厅开放状态）。
    #[must_use]
    pub fn new(
        handle: Handle,
        name: String,
        public: bool,
        im_fed: crate::handlers::im::ImFederation,
    ) -> Self {
        Self {
            handle: Some(handle),
            name,
            public,
            hostname: crate::handlers::discover::detect_hostname(),
            im_fed: Some(im_fed),
        }
    }

    /// 是否已启用（诊断/测试用）。
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.handle.is_some()
    }

    /// GET combined：一次拉全页数据（peers + buckets + ladder + endpoints +
    /// node_meta 并发取——本 handler 只读 meta 快照，不做任何连接性探测）。
    async fn combined_response(&self) -> ApiResponse {
        let Some(handle) = &self.handle else {
            return ok_json(to_value(&CombinedResp::disabled(&self.hostname)).unwrap_or_default());
        };
        let (peers, buckets, ladder, endpoints, meta) = tokio::join!(
            handle.peers(),
            handle.buckets_summary(),
            handle.ladder_stats(),
            handle.known_endpoints(),
            handle.node_meta()
        );
        let self_id_str = handle.self_id().to_hex();
        let meta_index = MetaIndex::build(&meta);
        let (mut lan, mut p2p) = partition(&peers, &buckets, &endpoints, &meta_index, &self_id_str);
        // 非活跃分组：meta 判定 Inactive 且未留在 lan/p2p 的节点单独列出
        let inactive = inactive_group(&meta, &lan, &p2p);
        // 大厅开放状态标注（节点发现页「进入 IM」按钮状态数据源）：
        // 探针缓存命中 Some(true/false)——首次查询在途/短 ID/未启用为 null。
        if let Some(fed) = &self.im_fed {
            for n in &mut lan {
                n.im_public = fed.lobby_status(&n.node_id);
            }
            for n in &mut p2p {
                n.im_public = fed.lobby_status(&n.node_id);
            }
        }
        let resp = CombinedResp {
            lan,
            p2p,
            inactive,
            ladder,
            self_info: SelfDto::enabled(
                handle.self_id().to_hex(),
                handle.self_id().overlay().to_hex(),
                &self.name,
                self.public,
                handle.listen_addr().to_string(),
                &self.hostname,
            ),
        };
        ok_json(to_value(&resp).unwrap_or_default())
    }
}

impl Default for NodeViewRouteHandler {
    fn default() -> Self {
        Self::new_disabled()
    }
}

#[async_trait]
impl RouteHandler for NodeViewRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![spec_read(HttpMethod::Get, PATH_COMBINED)]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let path = req.path.split('?').next().unwrap_or("");
        match (req.method, path) {
            (HttpMethod::Get, PATH_COMBINED) => Ok(self.combined_response().await),
            // —— 未覆盖的路由 —— 兜底 404（Ok，非 Err，与其它 handler 同款）
            _ => Ok(error_response(404, "node_view: 未匹配的路由")),
        }
    }
}

// ----------------------------------------------------------------------------
// 聚合 DTO
// ----------------------------------------------------------------------------

/// combined 响应：LAN 邻居 + P2P 远端 + 非活跃节点 + 连接阶梯 + 本机信息。
#[derive(Serialize)]
struct CombinedResp {
    /// 局域网邻居（通告 underlay 或端点簿观测地址为私网段）。
    lan: Vec<LanNodeDto>,
    /// P2P/WAN 远端（公网/NAT peer + 桶内非直连节点）。
    p2p: Vec<P2pNodeDto>,
    /// 非活跃节点（meta 组件判定 Inactive——五振出局，单独分组不混入 lan/p2p）。
    inactive: Vec<InactiveNodeDto>,
    /// 连接阶梯统计（P2P 卡片底部小字）。
    ladder: LadderStats,
    /// 本机信息。
    #[serde(rename = "self")]
    self_info: SelfDto,
}

impl CombinedResp {
    /// 未启用态：空 lan/p2p/inactive + 零阶梯 + 仅 hostname 的 self。
    fn disabled(hostname: &str) -> Self {
        Self {
            lan: Vec::new(),
            p2p: Vec::new(),
            inactive: Vec::new(),
            ladder: LadderStats::default(),
            self_info: SelfDto::disabled(hostname),
        }
    }
}

/// LAN 邻居条目（卡片 1 行数据）。
#[derive(Serialize)]
struct LanNodeDto {
    /// 0x + 66 hex 压缩公钥。
    node_id: String,
    /// `ip:port`（通告 underlay 或端点簿观测地址，直连可拨/已观测）。
    addr: String,
    /// 当前是否有已认证直连（绿点/灰点）。
    connected: bool,
    /// 公网服务节点标志。
    public: bool,
    /// 角色（public=true → hub，否则 edge）。
    role: String,
    /// 存活状态（meta 组件账本）："active"（心跳周期内）/ "inactive"（本分组
    /// 内恒 active——Inactive 且无活连接的对端已移入 `inactive` 分组）。
    status: String,
    /// 健康分（0-100；无元数据条目时 0）。
    score: u8,
    /// 最近一次确认存活（unix 秒；无条目时 0）。
    last_seen: u64,
    /// 元数据来源："direct"（本机直连观测）/ "gossip"（他节点转述）/ None（无条目）。
    meta_source: Option<String>,
    /// 对方 IM 大厅开放状态（P2P 探针缓存）：Some(true) = 允许浏览（「进入 IM」
    /// 可点）/ Some(false) = 未开放（按钮灰）/ None = 查询在途或短 ID 不可查。
    im_public: Option<bool>,
}

/// P2P 远端条目（卡片 2 行数据）。
#[derive(Serialize)]
struct P2pNodeDto {
    /// 0x + 66 hex 压缩公钥（bucket 来源为短式 `0x1234…cdef`）。
    node_id: String,
    /// 可拨 underlay（None = NAT 后节点，只能打洞/中继）。
    addr: Option<String>,
    /// 当前是否有已认证直连。
    connected: bool,
    /// 公网服务节点标志（public 徽标）。
    public: bool,
    /// NODES 学到的中继者。
    relay: Option<String>,
    /// 可达性路由 {该节点 → 经谁}（"来源节点"展示）。
    route_via: Option<String>,
    /// 来源："peer"（路由表直连集）/ "bucket"（Kademlia 桶非直连）。
    source: String,
    /// 存活状态（meta 组件账本；语义同 [`LanNodeDto::status`]）。
    status: String,
    /// 健康分（0-100；无元数据条目时 0）。
    score: u8,
    /// 最近一次确认存活（unix 秒；无条目时 0）。
    last_seen: u64,
    /// 元数据来源："direct" / "gossip" / None（无条目；bucket 短 ID 无从匹配
    /// 时也恒 None）。
    meta_source: Option<String>,
    /// 对方 IM 大厅开放状态（语义同 [`LanNodeDto::im_public`]；bucket 短 ID
    /// 恒 None——无从寻址查询）。
    im_public: Option<bool>,
}

/// 非活跃节点条目（inactive 分组行数据——meta 组件判定 Inactive 的节点，
/// 五振出局不再心跳；复活路径：手动心跳 `POST /api/v1/p2p/node-meta/:id/reactivate`
/// 或他节点交互报告其存活）。
#[derive(Serialize)]
struct InactiveNodeDto {
    /// 0x + 66 hex 压缩公钥（来自元数据注册表——全量 ID，可直接寻址手动心跳）。
    node_id: String,
    /// 观测地址历史（去重，最新在前，上限 8 条）。
    addrs: Vec<String>,
    /// 健康分（Inactive 出局即不再携带分数——展示 0）。
    score: u8,
    /// 最近一次确认存活（unix 秒）。
    last_seen: u64,
    /// 元数据来源："direct"（本机直连观测）/ "gossip"（他节点转述）。
    meta_source: String,
    /// 出局时刻（unix 秒——"非活跃多久"的起点）。
    since: u64,
}

/// 本机信息（顶部自机条）。
#[derive(Serialize)]
struct SelfDto {
    /// P2P 组网是否启用。
    enabled: bool,
    /// 本机 NodeID（未启用为 None）。
    node_id: Option<String>,
    /// EVM 同源 OverlayAddr（未启用为 None）。
    overlay_addr: Option<String>,
    /// 节点昵称（`NEXOS_P2P_NAME`；未设置/未启用为 None）。
    name: Option<String>,
    /// 本机主机名（始终可见——未启用时的兜底展示）。
    hostname: String,
    /// 公网服务节点声明。
    public: bool,
    /// 角色："hub"（公网服务节点）/ "edge"（普通节点）；未启用为 None。
    role: Option<String>,
    /// 组网监听地址（未启用为 None）。
    listen: Option<String>,
}

impl SelfDto {
    /// 未启用态（仅 hostname）。
    fn disabled(hostname: &str) -> Self {
        Self {
            enabled: false,
            node_id: None,
            overlay_addr: None,
            name: None,
            hostname: hostname.to_string(),
            public: false,
            role: None,
            listen: None,
        }
    }

    /// 已启用态（NodeID/OverlayAddr/昵称/角色/监听全量）。
    fn enabled(
        node_id: String,
        overlay_addr: String,
        name: &str,
        public: bool,
        listen: String,
        hostname: &str,
    ) -> Self {
        Self {
            enabled: true,
            node_id: Some(node_id),
            overlay_addr: Some(overlay_addr),
            name: (!name.is_empty()).then(|| name.to_string()),
            hostname: hostname.to_string(),
            public,
            role: Some(role_of(public).to_string()),
            listen: Some(listen),
        }
    }
}

// ----------------------------------------------------------------------------
// 纯逻辑：地址分类 + peers/buckets 分区 + 元数据富化
// ----------------------------------------------------------------------------

/// LAN 地址判定：RFC1918 私网段（10/8、172.16/12、192.168/16）+ loopback
/// （127/8——同主机部署语义上属"本地"）+ 链路本地（169.254/16、fe80::/10）。
/// 其余（公网单播）视为 WAN，归 P2P 分区。
///
/// 注：IPv6 链路本地用段位与判断（fe80::/10 = `segments()[0] & 0xffc0 ==
/// 0xfe80`）——`Ipv6Addr::is_unicast_link_local` 稳定于 1.84，超 crate MSRV。
fn is_lan_ip(ip: IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        std::net::IpAddr::V6(v6) => v6.is_loopback() || (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// 非回环 LAN 判定：私网段且非 loopback。回环条目已在 [`partition`] 入口
/// 整条剔除（2026-08-25 定调「回环一律不输出」），此处仅作分级信号的再兜底
/// ——端点簿里的 `127.0.0.1:port` 是**对端**本机多实例的观测视角，不是本机
/// 的 LAN 邻居（曾据此误入 LAN 卡片成离线条目——2026-08-23 修复，2026-08-25
/// 收紧为整条不输出）。
fn is_lan_non_loop(ip: IpAddr) -> bool {
    is_lan_ip(ip) && !ip.is_loopback()
}

/// LAN 卡片展示地址规整：观测地址带**入站临时端口**时换成对端监听口。
///
/// 背景问题（2026-08-25 修复）：113 拨入 106 时，106 侧 socket 的对端地址
/// （observed）是 `192.0.2.113:49730`——49730 是 113 **出站连接的临时源
/// 端口**（OS 分配，每次重连都变），不是它的监听口 7070。LAN 卡片照旧展示
/// 会误导（照着拨必失败），且端口随重连抖动。分类仍用观测地址的 IP（第一手
/// 信号），仅**展示层**规整端口。
///
/// 取值优先级：
/// 1. `listen_hint`：对端通告的 underlay（自报监听口——唯一可靠信号；NAT 后
///    的 edge 节点没有，None）；
/// 2. 观测端口已是 [`P2P_PORT_DEFAULT`]（对端监听默认口，或本机出站拨的就
///    是其监听口）→ 原样；
/// 3. 其余（端口 ≠ 7070 的观测地址——典型即入站 socket 的临时源端口）→
///    IP 保留、端口规整为 7070。
///
/// **假设与局限**：本集群所有节点默认监听 7070，入站观测的 IP 部分是对的、
/// 换 7070 即真实可拨；真监听非默认端口的节点会显示错（优先取 underlay 正是
/// 为此——underlay 通告将来覆盖非 public 节点后可去掉该兜底）。
///
/// 回环分支为防御性死代码：回环条目已在 [`partition`] 入口整条剔除（2026-08-25
/// 定调），正常不会走到；保留是为兜底未来回归——若漏进来，宁展示真实 socket
/// 端口也不误导成 7070（127.0.0.1 多实例共享同一 IP，7070 多半指向另一实例）。
fn lan_display_addr(listen_hint: Option<SocketAddr>, observed: SocketAddr) -> SocketAddr {
    if let Some(listen) = listen_hint {
        return listen;
    }
    let ip = observed.ip();
    if ip.is_loopback() || observed.port() == P2P_PORT_DEFAULT {
        return observed;
    }
    SocketAddr::new(ip, P2P_PORT_DEFAULT)
}

/// 角色：公网服务节点（bootstrap 锚点/中继志愿者）为 hub，其余 edge。
fn role_of(public: bool) -> &'static str {
    if public {
        "hub"
    } else {
        "edge"
    }
}

/// 短 NodeID 展示式（与 os-p2p `short_hex` 同款：`0x1234…cdef`）——
/// bucket 摘要条目即此格式，去重时按同款短式比较。
fn short_hex(s: &str) -> String {
    let n = s.len();
    if n <= 10 {
        s.to_string()
    } else {
        format!("{}…{}", &s[..6], &s[n - 4..])
    }
}

// —— 元数据富化（os-p2p meta 组件——节点存活判定的唯一账本）——

/// 元数据注册表索引：全量 NodeID hex → 条目（peers 匹配）+ 短式 → 条目
/// （bucket 摘要条目匹配——桶条目本身即短式）。
struct MetaIndex {
    by_id: HashMap<String, NodeMetaEntry>,
    by_short: HashMap<String, NodeMetaEntry>,
}

impl MetaIndex {
    /// 从 `Handle::node_meta` 快照建索引（条目克隆——快照本身另有他用）。
    /// **含回环地址的条目跳过**（展示双保险）：meta 源头已封死回环
    /// （record_conn 跳过 / push 拒绝 / 加载剔除），此处兜底——即使注册表
    /// 漏进回环条目，也不富化、不门控 lan/p2p 行，更进不了 inactive 分组。
    fn build(meta: &[NodeMetaEntry]) -> Self {
        let mut idx = Self {
            by_id: HashMap::new(),
            by_short: HashMap::new(),
        };
        for e in meta {
            if e.addrs.iter().any(|ma| ma.addr.ip().is_loopback()) {
                continue;
            }
            let full = e.id.to_hex();
            idx.by_short.insert(short_hex(&full), e.clone());
            idx.by_id.insert(full, e.clone());
        }
        idx
    }

    /// 按全量 NodeID hex 查（peers 条目）。
    fn get(&self, full_hex: &str) -> Option<&NodeMetaEntry> {
        self.by_id.get(full_hex)
    }

    /// 按短式查（bucket 摘要条目）。
    fn get_short(&self, short: &str) -> Option<&NodeMetaEntry> {
        self.by_short.get(short)
    }
}

/// 单条目的展示富化字段。
struct MetaFields {
    /// "active" / "inactive"（meta 状态机）。
    status: &'static str,
    /// 健康分（Inactive 出局即 0——状态机不再携带分数）。
    score: u8,
    /// 最近一次确认存活（unix 秒）。
    last_seen: u64,
    /// "direct" / "gossip"。
    source: Option<&'static str>,
}

/// 从注册表条目取展示字段（Active 带分数；Inactive 分数记 0）。
fn meta_fields(e: &NodeMetaEntry) -> MetaFields {
    let (status, score) = match e.state {
        MetaState::Active { score, .. } => ("active", score),
        MetaState::Inactive { .. } => ("inactive", 0),
    };
    MetaFields {
        status,
        score,
        last_seen: e.last_seen,
        source: match e.source {
            MetaSource::Direct => Some("direct"),
            MetaSource::Gossip => Some("gossip"),
        },
    }
}

/// 无注册表条目的富化兜底：直连/已知节点默认 active（存活判定未覆盖 ≠
/// 死亡——meta 引擎可能尚未建档），score/last_seen 记 0、来源 null。
fn meta_fields_default() -> MetaFields {
    MetaFields {
        status: "active",
        score: 0,
        last_seen: 0,
        source: None,
    }
}

/// 取富化字段；有活连接时 status 强制 "active"（连接是最强活性证据——
/// meta 判定 Inactive 但连接尚存的窗口期以直连为准）。
fn meta_fields_of(index: &MetaIndex, full_hex: &str, connected: bool) -> MetaFields {
    let mut f = index
        .get(full_hex)
        .map(meta_fields)
        .unwrap_or_else(meta_fields_default);
    if connected {
        f.status = "active";
    }
    f
}

/// 分区：地址类别决定卡片——peers 按地址信号判 LAN/WAN；buckets 中不在
/// peers 集的短 NodeID 作为"非直连节点"按同款规则归 lan（mDNS 发现的同网段
/// 邻居）/ p2p（source="bucket"）。每个条目富化元数据字段（status/score/
/// last_seen/meta_source——存活判定来自 meta 组件，本函数不做任何探测）；
/// meta 判定 Inactive 且无活连接的节点**不进 lan/p2p**（由 [`inactive_group`]
/// 单独列出）。
///
/// **回环条目一律不输出**（2026-08-25 用户定调：「127.0.0.1 无论怎么产生的，
/// 都应该屏蔽」）：peers 的任一地址信号（直连观测 / 通告 underlay / 端点簿
/// 观测）、bucket 条目的端点簿观测为回环 → 该条目 lan/p2p 均不出现（早前
/// 「直连回环归 LAN」语义废弃——同机多实例观测由 identity_conflicts 承担）。
/// meta 侧的回环由 [`MetaIndex::build`] / [`inactive_group`] 同步过滤（双保险）。
///
/// 地址信号优先级（按可信度降序）：
/// 1. **直连观测地址**（`PeerInfo.observed_addr`——活连接 socket 的对端
///    `peer_addr`，握手验证过的第一手信号）：私网（非回环）→ LAN 卡片、
///    公网 → P2P 卡片，**分类**均用它。最高优先——端点簿观测可能被公网锚点
///    gossip 覆盖成 NAT 映射，直连 socket 的落点不可伪造（106↔113 LAN 互连
///    被误归 WAN 的修复点：直连 `192.0.2.113` 压过端点簿里的公网
///    `198.51.100.57`）。**展示地址**经 [`lan_display_addr`] 规整：入站观测的
///    临时源端口换成对端监听口（underlay 优先，兜底 7070）；
/// 2. **通告 underlay**（仅 public 节点有——hub 锚点地址，私网**非回环**才计
///    LAN）；
/// 3. **端点簿观测地址**（`known_endpoints`：gossip 汇聚的观测地址——LAN
///    邻居的兜底现身处）。观测来源的展示地址同样规整端口（gossip 观测的
///    也可能是入站临时口）。
///
/// 无直连观测时走 2/3 级：两者皆私网（非回环）→ LAN 卡片；公网/缺失 → P2P。
fn partition(
    peers: &[PeerInfo],
    buckets: &[BucketStat],
    endpoints: &[os_p2p::EndpointEntry],
    meta: &MetaIndex,
    self_node_id: &str,
) -> (Vec<LanNodeDto>, Vec<P2pNodeDto>) {
    // 端点簿索引：全量 NodeID → 观测地址（peers 匹配）+ 短式 → 观测地址
    //（bucket 摘要条目匹配——桶条目本身即短式）
    let mut observed: HashMap<String, SocketAddr> = HashMap::new();
    let mut observed_short: HashMap<String, SocketAddr> = HashMap::new();
    for e in endpoints {
        let full = e.id.to_hex();
        observed_short.insert(short_hex(&full), e.addr);
        observed.insert(full, e.addr);
    }
    let lan_addr_of =
        |underlay: Option<SocketAddr>, obs: Option<SocketAddr>| -> Option<SocketAddr> {
            // 非直连来源的 LAN 信号必须私网且非回环（回环条目已在上方入口
            // 整条剔除，此处再兜底）
            underlay
                .filter(|u| is_lan_non_loop(u.ip()))
                .or_else(|| obs.filter(|u| is_lan_non_loop(u.ip())))
        };
    let mut lan = Vec::new();
    let mut p2p = Vec::new();
    // peers 全量 ID 的短式集合（bucket 去重基准）
    let mut peer_shorts = HashSet::new();
    for p in peers {
        let full = p.id.to_hex();
        if full == self_node_id {
            continue;
        } // 不显示本机自身
        peer_shorts.insert(short_hex(&full));
        let obs = observed.get(&full).copied();
        // 回环条目直接不输出：任一地址信号（直连观测 / 通告 underlay / 端点簿
        // 观测）为回环 → lan/p2p 均不出现（短式仍入 peer_shorts——桶侧同节点
        // 一并隐藏，不重复输出）
        if p.observed_addr.is_some_and(|a| a.ip().is_loopback())
            || p.underlay.is_some_and(|u| u.ip().is_loopback())
            || obs.is_some_and(|a| a.ip().is_loopback())
        {
            continue;
        }
        // meta 判定 Inactive 且无活连接 → 移出 lan/p2p（inactive 分组单独列出；
        // 有活连接则照常进卡——status 以直连为准标 active）
        if meta
            .get(&full)
            .is_some_and(|e| matches!(e.state, MetaState::Inactive { .. }))
            && !p.connected
        {
            continue;
        }
        let f = meta_fields_of(meta, &full, p.connected);
        // 第 1 级——直连观测地址（握手验证过的第一手信号）最高优先，直接
        // 裁决分类并作为展示地址；即便端点簿被公网锚点 gossip 污染也不受影响。
        // （回环观测已在上方整条剔除——不进 LAN。）
        if let Some(direct) = p.observed_addr {
            if is_lan_ip(direct.ip()) {
                // 直连私网 → LAN 卡片（分类用直连观测——真实链路落点；展示
                // 地址规整入站临时端口为对端监听口，见 [`lan_display_addr`]）
                lan.push(LanNodeDto {
                    node_id: full,
                    addr: lan_display_addr(p.underlay, direct).to_string(),
                    connected: p.connected,
                    public: p.public,
                    role: role_of(p.public).to_string(),
                    status: f.status.to_string(),
                    score: f.score,
                    last_seen: f.last_seen,
                    meta_source: f.source.map(str::to_string),
                    im_public: None,
                });
            } else {
                // 直连公网 → P2P 卡片（addr 用直连观测——NAT 映射/公网对端）
                p2p.push(P2pNodeDto {
                    node_id: full,
                    addr: Some(direct.to_string()),
                    connected: p.connected,
                    public: p.public,
                    relay: p.relay.as_ref().map(|r| r.to_hex()),
                    route_via: p.route_via.as_ref().map(|r| r.to_hex()),
                    source: "peer".to_string(),
                    status: f.status.to_string(),
                    score: f.score,
                    last_seen: f.last_seen,
                    meta_source: f.source.map(str::to_string),
                    im_public: None,
                });
            }
            continue;
        }
        match lan_addr_of(p.underlay, obs) {
            // 局域网地址（通告或观测）→ LAN 卡片（underlay 已在此优先选用；
            // 观测兜底来源的临时端口规整为 7070）
            Some(addr) => lan.push(LanNodeDto {
                node_id: full,
                addr: lan_display_addr(None, addr).to_string(),
                connected: p.connected,
                public: p.public,
                role: role_of(p.public).to_string(),
                status: f.status.to_string(),
                score: f.score,
                last_seen: f.last_seen,
                meta_source: f.source.map(str::to_string),
                im_public: None,
            }),
            None if p.connected
                && !p.public
                && obs.map(|a| is_lan_non_loop(a.ip())).unwrap_or(true) =>
            {
                // 直连的非 public 节点：端点簿可能为空（gossip 依赖 NODES 消息
                // 才填充），但直连说明可达——典型场景即同网段 LAN 邻居。
                // 排除：观测地址明确是公网的（NAT 映射）仍归 P2P。
                lan.push(LanNodeDto {
                    node_id: full,
                    addr: obs
                        .map(|a| lan_display_addr(None, a).to_string())
                        .unwrap_or_else(|| "直连".into()),
                    connected: true,
                    public: false,
                    role: "edge".into(),
                    status: f.status.to_string(),
                    score: f.score,
                    last_seen: f.last_seen,
                    meta_source: f.source.map(str::to_string),
                    im_public: None,
                })
            }
            // 公网地址 / 无地址信号（NAT）/ 回环观测 → P2P 卡片
            None => p2p.push(P2pNodeDto {
                node_id: full,
                addr: p
                    .underlay
                    .map(|u| u.to_string())
                    .or_else(|| obs.map(|a| a.to_string())),
                connected: p.connected,
                public: p.public,
                relay: p.relay.as_ref().map(|r| r.to_hex()),
                route_via: p.route_via.as_ref().map(|r| r.to_hex()),
                source: "peer".to_string(),
                status: f.status.to_string(),
                score: f.score,
                last_seen: f.last_seen,
                meta_source: f.source.map(str::to_string),
                im_public: None,
            }),
        }
    }
    // Kademlia 桶非直连节点（短 ID 不在 peers 集）→ 按观测地址归 lan/p2p
    for b in buckets {
        for entry in &b.entries {
            if peer_shorts.contains(entry) {
                continue;
            }
            let obs = observed_short.get(entry).copied();
            // 回环观测条目直接不输出（同 peers 侧规则——桶条目仅端点簿一个
            // 地址信号，观测为回环即整条隐藏）
            if obs.is_some_and(|a| a.ip().is_loopback()) {
                continue;
            }
            // meta 判定 Inactive → 移出 lan/p2p（桶条目天然非直连，inactive
            // 分组以 meta 全量 ID 列出——可直接寻址手动心跳）
            if meta
                .get_short(entry)
                .is_some_and(|e| matches!(e.state, MetaState::Inactive { .. }))
            {
                continue;
            }
            let f = meta
                .get_short(entry)
                .map(meta_fields)
                .unwrap_or_else(meta_fields_default);
            match obs.filter(|u| is_lan_non_loop(u.ip())) {
                // 同网段邻居（mDNS/直连学过地址，尚未建路由）→ LAN 卡片（灰点；
                // 观测地址的临时端口同样规整为 7070——桶短 ID 无 underlay 可查）
                Some(addr) => lan.push(LanNodeDto {
                    node_id: entry.clone(),
                    addr: lan_display_addr(None, addr).to_string(),
                    connected: false,
                    public: false,
                    role: role_of(false).to_string(),
                    status: f.status.to_string(),
                    score: f.score,
                    last_seen: f.last_seen,
                    meta_source: f.source.map(str::to_string),
                    im_public: None,
                }),
                // 无地址 / 公网观测 → P2P 卡片（回环观测已在上方整条剔除）
                None => p2p.push(P2pNodeDto {
                    node_id: entry.clone(),
                    addr: obs.map(|a| a.to_string()),
                    connected: false,
                    public: false,
                    relay: None,
                    route_via: None,
                    source: "bucket".to_string(),
                    status: f.status.to_string(),
                    score: f.score,
                    last_seen: f.last_seen,
                    meta_source: f.source.map(str::to_string),
                    im_public: None,
                }),
            }
        }
    }
    (lan, p2p)
}

/// 非活跃分组：meta 注册表中判定 Inactive 且未出现在 lan/p2p 的节点单独列出
/// （含 addrs/score/last_seen/meta_source/since——五振出局，不再心跳；复活靠
/// 手动心跳或他节点交互报告）。
///
/// **回环条目不输出**（展示双保险，与 [`partition`] 同规则）：地址历史含回环
/// 的 meta 条目整条跳过——meta 源头已封死回环，此处兜底防漏。
///
/// 保留 meta 快照次序（Inactive 段内按出局时刻降序——最近掉线的在前）。
/// 已留在 lan/p2p 的 Inactive 节点（当前恰有活连接的窗口期，条目 status 已
/// 标 active）不重复列出。
fn inactive_group(
    meta: &[NodeMetaEntry],
    lan: &[LanNodeDto],
    p2p: &[P2pNodeDto],
) -> Vec<InactiveNodeDto> {
    // lan/p2p 内的全量 NodeID 集（bucket 短式不可能与全量 hex 相等，无需排除）
    let present: HashSet<&str> = lan
        .iter()
        .map(|n| n.node_id.as_str())
        .chain(p2p.iter().map(|n| n.node_id.as_str()))
        .collect();
    meta.iter()
        .filter(|e| matches!(e.state, MetaState::Inactive { .. }))
        // 回环条目整条不输出（含回环地址即跳过——公网部分也不展示）
        .filter(|e| e.addrs.iter().all(|ma| !ma.addr.ip().is_loopback()))
        .filter(|e| !present.contains(e.id.to_hex().as_str()))
        .map(|e| InactiveNodeDto {
            node_id: e.id.to_hex(),
            // addrs 结构升级 {addr, verified}（os-p2p 指纹验证批）：此处仅取
            // 地址字符串，verified 展示语义由后续批接入
            addrs: e.addrs.iter().map(|a| a.addr.to_string()).collect(),
            // Inactive 状态机不携带分数——展示 0（与 lan/p2p 条目的缺省一致）
            score: 0,
            last_seen: e.last_seen,
            meta_source: match e.source {
                MetaSource::Direct => "direct",
                MetaSource::Gossip => "gossip",
            }
            .to_string(),
            since: match e.state {
                MetaState::Inactive { since } => since,
                MetaState::Active { .. } => 0,
            },
        })
        .collect()
}

// ----------------------------------------------------------------------------
// 内部辅助（与其它 handler 同款）
// ----------------------------------------------------------------------------

/// `GET /api/v1/nodes/combined`——节点发现页聚合视图。
const PATH_COMBINED: &str = "/api/v1/nodes/combined";

/// 本 handler 注册时的组件名（`RouteSpec::handler_component`）。
const COMPONENT: &str = "node_view";

/// 构造一条只读路由规格（公开——聚合观察面不涉敏感数据）。
fn spec_read(method: HttpMethod, path: &str) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: COMPONENT.to_string(),
        requires_auth: false,
        required_roles: Vec::new(),
    }
}

/// 构造一个 200 JSON 响应（空 headers）。
fn ok_json(body: serde_json::Value) -> ApiResponse {
    ApiResponse {
        status: 200,
        body,
        headers: serde_json::json!({}),
    }
}

/// 构造一个最小 JSON 错误响应（status 由调用方指定）。
fn error_response(status: u16, msg: &str) -> ApiResponse {
    ApiResponse {
        status,
        body: serde_json::json!({"error": msg}),
        headers: serde_json::json!({}),
    }
}

/// 把可序列化结果转成 `serde_json::Value`，序列化失败统一映射为 Internal。
fn to_value<T: serde::Serialize + ?Sized>(v: &T) -> Result<serde_json::Value, ApiGatewayError> {
    serde_json::to_value(v).map_err(|e| ApiGatewayError::Internal(format!("响应序列化失败: {e}")))
}

// ----------------------------------------------------------------------------
// 单元测——路由归属 / 响应结构 / LAN·P2P 过滤 / self 字段 / 真实 mesh
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use os_p2p::{NodeIdentity, P2pConfig, P2pNode, Timing};

    /// 生成一个随机 NodeId（fixture 用）。
    fn random_id() -> os_p2p::NodeId {
        NodeIdentity::generate().node_id()
    }

    /// 本机非回环 IPv4（UDP connect 探测默认路由——仅选路不发包；与 os-p2p
    /// api.rs 测试同款）。2026-08-25 回环彻底屏蔽后，真实 mesh 测试必须经
    /// 非回环地址互拨——回环观测的对端既不入 meta 注册表也不出现在的
    /// combined 视图（partition 三组零输出）。
    fn non_loopback_local_ipv4() -> std::net::Ipv4Addr {
        let s = std::net::UdpSocket::bind("0.0.0.0:0").expect("UDP bind（选路用）");
        s.connect("8.8.8.8:80").expect("connect（仅选路，不发包）");
        match s.local_addr().expect("local_addr").ip() {
            std::net::IpAddr::V4(v4) if !v4.is_loopback() => v4,
            other => panic!("无非回环本机 IPv4 可用（真实 mesh 测试无从进行）: {other}"),
        }
    }

    /// 真实 mesh 测试用的监听地址（本机 LAN IP + 随机端口——对端经它拨入，
    /// 观测地址非回环，combined / meta 才可见）。
    fn lan_listen() -> SocketAddr {
        SocketAddr::new(std::net::IpAddr::V4(non_loopback_local_ipv4()), 0)
    }

    /// 构造一条路由表 fixture（id/underlay/connected/public 可控；无直连
    /// 观测——带 observed_addr 的场景用 struct 更新语法覆盖）。
    fn peer(id: os_p2p::NodeId, underlay: Option<&str>, connected: bool, public: bool) -> PeerInfo {
        PeerInfo {
            id,
            underlay: underlay.map(|a| a.parse().unwrap()),
            public,
            relay: None,
            connected,
            relayed_by_me: false,
            route_via: None,
            observed_addr: None,
        }
    }

    /// 构造一条端点簿 fixture（NodeID + 观测地址）。
    fn endpoint(id: os_p2p::NodeId, addr: &str) -> os_p2p::EndpointEntry {
        os_p2p::EndpointEntry {
            id,
            addr: addr.parse().unwrap(),
            last_seen: std::time::Instant::now(),
        }
    }

    /// 空元数据索引（无 meta 条目的分区 fixture——富化字段走默认值）。
    fn empty_meta() -> MetaIndex {
        MetaIndex::build(&[])
    }

    /// 构造一条 meta 注册表条目 fixture（addrs 解析 ip:port；state/source 可控）。
    fn meta_entry(
        id: os_p2p::NodeId,
        addrs: &[&str],
        last_seen: u64,
        state: os_p2p::MetaState,
        source: os_p2p::MetaSource,
    ) -> NodeMetaEntry {
        NodeMetaEntry {
            id,
            addrs: addrs
                .iter()
                .map(|a| os_p2p::MetaAddr::unverified(a.parse().unwrap()))
                .collect(),
            first_seen: last_seen.saturating_sub(600),
            last_seen,
            state,
            source,
            verified: false,
            exit_offered: false,
        }
    }

    fn get_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    // —— 1) 路由归属：GET /api/v1/nodes/combined 归 node_view 组件、读公开 ——

    #[tokio::test]
    async fn routes_declares_combined_under_node_view_component() {
        let h = NodeViewRouteHandler::new_disabled();
        let routes = h.routes().await;
        assert_eq!(routes.len(), 1, "仅 1 条路由");
        let r = &routes[0];
        assert_eq!(r.method, HttpMethod::Get);
        assert_eq!(r.path, PATH_COMBINED);
        assert_eq!(r.handler_component, COMPONENT, "路由归属 node_view 组件");
        assert!(!r.requires_auth, "聚合观察面读公开");
        assert!(r.required_roles.is_empty(), "无角色要求");
    }

    // —— 2) 未启用：结构完整（lan/p2p 空数组 + self.enabled=false + 零阶梯）——

    #[tokio::test]
    async fn disabled_combined_has_shape_with_empty_partitions() {
        let h = NodeViewRouteHandler::new_disabled();
        assert!(!h.is_enabled());
        let resp = h.handle(get_req(PATH_COMBINED)).await.unwrap();
        assert_eq!(
            resp.status, 200,
            "未启用返回 200 空视图（非 503——页面仍可展示）"
        );
        // 结构四键齐备（2026-08-23 起含 inactive 非活跃分组）
        assert!(
            resp.body["lan"].as_array().unwrap().is_empty(),
            "lan 空数组"
        );
        assert!(
            resp.body["p2p"].as_array().unwrap().is_empty(),
            "p2p 空数组"
        );
        assert!(
            resp.body["inactive"].as_array().unwrap().is_empty(),
            "inactive 空数组"
        );
        assert_eq!(resp.body["ladder"]["direct"], 0);
        assert_eq!(resp.body["ladder"]["punched"], 0);
        assert_eq!(resp.body["ladder"]["relayed"], 0);
        // self：enabled=false + hostname 兜底 + 其余字段 null
        assert_eq!(resp.body["self"]["enabled"], false);
        assert_eq!(resp.body["self"]["node_id"], serde_json::Value::Null);
        assert_eq!(resp.body["self"]["role"], serde_json::Value::Null);
        assert!(
            !resp.body["self"]["hostname"].as_str().unwrap().is_empty(),
            "hostname 兜底展示"
        );
    }

    // —— 3) LAN 地址过滤：私网/loopback 归 LAN，公网与 172.16-31 边界外归 WAN ——

    #[test]
    fn is_lan_ip_classifies_private_vs_public() {
        // RFC1918 三段 + 边界值
        for ok in [
            "10.0.0.1",
            "10.255.255.255", // 10/8
            "172.16.0.1",
            "172.31.255.255", // 172.16/12（含上下界）
            "192.168.1.106",
            "192.168.0.1", // 192.168/16
            "127.0.0.1",   // loopback（同主机部署）
            "169.254.1.1", // 链路本地
            "::1",
            "fe80::1", // IPv6 loopback / 链路本地
        ] {
            let ip: IpAddr = ok.parse().unwrap();
            assert!(is_lan_ip(ip), "{ok} 应判为 LAN");
        }
        // 公网 + 172.16/12 上下界外
        for wan in [
            "8.8.8.8",
            "1.2.3.4",
            "100.64.0.1", // 公网单播
            "172.15.255.255",
            "172.32.0.1",  // 172.15 / 172.32 不在 172.16-31 内
            "2001:db8::1", // 公网 IPv6
        ] {
            let ip: IpAddr = wan.parse().unwrap();
            assert!(!is_lan_ip(ip), "{wan} 应判为 WAN");
        }
    }

    // —— 4) 分区过滤：私网 peer → lan；公网/无 underlay peer → p2p（source=peer）——

    #[test]
    fn partition_routes_peers_by_underlay_class() {
        let lan_id = random_id();
        let wan_id = random_id();
        let nat_id = random_id();
        let lan_hex = lan_id.to_hex();
        let wan_hex = wan_id.to_hex();
        let nat_hex = nat_id.to_hex();
        let peers = vec![
            peer(lan_id, Some("192.168.1.106:7070"), true, false),
            peer(wan_id, Some("8.8.8.8:7070"), false, true),
            peer(nat_id, None, false, false), // NAT 后节点（无 underlay）
        ];
        let (lan, p2p) = partition(&peers, &[], &[], &empty_meta(), "");
        // LAN 卡片：仅私网 peer，字段完整（addr/connected/role）
        assert_eq!(lan.len(), 1, "仅私网 peer 进 lan");
        assert_eq!(lan[0].node_id, lan_hex);
        assert_eq!(lan[0].addr, "192.168.1.106:7070");
        assert!(lan[0].connected);
        assert_eq!(lan[0].role, "edge");
        // P2P 卡片：公网 + NAT 两条，source 均为 peer
        assert_eq!(p2p.len(), 2, "公网与 NAT peer 进 p2p");
        let wan = p2p.iter().find(|p| p.node_id == wan_hex).unwrap();
        assert_eq!(wan.source, "peer");
        assert_eq!(wan.addr.as_deref(), Some("8.8.8.8:7070"));
        assert!(wan.public);
        let nat = p2p.iter().find(|p| p.node_id == nat_hex).unwrap();
        assert_eq!(nat.addr, None, "NAT 节点无 underlay");
        assert!(!nat.connected);
        // public peer 的角色推导（hub）在 LAN 侧体现
        let hub_lan = partition(
            &[peer(random_id(), Some("10.1.2.3:1"), true, true)],
            &[],
            &[],
            &empty_meta(),
            "",
        );
        assert_eq!(hub_lan.0[0].role, "hub");
    }

    // —— 4b) 端点簿观测地址兜底：非 public 节点不通告 underlay，LAN 邻居靠观测地址现身 ——

    #[test]
    fn partition_uses_observed_endpoint_for_lan_classification() {
        // 场景对齐真实部署：106↔113 同网段互联，非 public 节点 underlay 均为
        // None（os-p2p 仅 public 节点通告可拨地址），但端点簿记录了真实 socket。
        let lan_peer = random_id(); // 192.168 观测 → LAN
        let wan_peer = random_id(); // 公网观测 → P2P（NAT 映射口）
        let bare_peer = random_id(); // 无任何地址信号 → P2P（addr=null）
        let lan_hex = lan_peer.to_hex();
        let bare_hex = bare_peer.to_hex();
        let endpoints = vec![
            endpoint(lan_peer.clone(), "192.168.1.113:52001"),
            endpoint(wan_peer.clone(), "203.0.113.9:40022"),
        ];
        let peers = vec![
            peer(lan_peer.clone(), None, true, false),
            peer(wan_peer.clone(), None, true, false),
            peer(bare_peer.clone(), None, false, false),
        ];
        let (lan, p2p) = partition(&peers, &[], &endpoints, &empty_meta(), "");
        // 观测私网地址 → LAN 卡片（真实 106/113 部署的落点；观测端口 52001
        // 是临时口，展示规整为对端监听口 7070——见 lan_display_addr）
        assert_eq!(lan.len(), 1, "观测私网地址的 peer 进 lan");
        assert_eq!(lan[0].node_id, lan_hex);
        assert_eq!(
            lan[0].addr, "192.168.1.113:7070",
            "LAN 行展示规整后的可拨地址"
        );
        assert!(lan[0].connected);
        // 公网观测（NAT 映射）与无信号 → P2P；addr 展示观测地址 / null
        assert_eq!(p2p.len(), 2);
        let wan = p2p
            .iter()
            .find(|p| p.addr.as_deref() == Some("203.0.113.9:40022"))
            .expect("NAT 映射观测地址展示在 p2p 行");
        assert_eq!(wan.source, "peer");
        let bare = p2p.iter().find(|p| p.addr.is_none()).unwrap();
        assert_eq!(bare.node_id, bare_hex);
    }

    // —— 4c) 直连观测地址最高优先：私网直连 → LAN 卡片；公网直连 → P2P 卡片
    //        （addr 均用直连观测）；端点簿被公网锚点 gossip 污染（观测被覆盖成
    //        公网映射）也不影响直连 peers 的 LAN 分类——106↔113 LAN 互连被误
    //        归 WAN 的修复点 ——

    #[test]
    fn partition_direct_observed_overrides_polluted_endpoint_book() {
        // 场景对齐真实故障：106↔113 有直连 LAN socket（192.0.2.113），但
        // 端点簿里 113 的观测地址被公网锚点 gossip 覆盖成公网 198.51.100.57
        // ——直连观测（握手验证过的第一手信号）必须压过被污染的端点簿。
        let lan_id = random_id(); // 直连私网观测 → LAN（端点簿公网污染不改分类）
        let wan_id = random_id(); // 直连公网观测 → P2P（addr 用直连观测）
        let lan_hex = lan_id.to_hex();
        let wan_hex = wan_id.to_hex();
        let endpoints = vec![
            endpoint(lan_id.clone(), "198.51.100.57:55637"), // gossip 污染的公网观测
            endpoint(wan_id.clone(), "203.0.113.9:40022"),  // 一致的公网观测
        ];
        let peers = vec![
            PeerInfo {
                observed_addr: Some("192.0.2.113:49730".parse().unwrap()),
                ..peer(lan_id.clone(), None, true, false)
            },
            PeerInfo {
                observed_addr: Some("203.0.113.9:40022".parse().unwrap()),
                ..peer(wan_id.clone(), None, true, false)
            },
        ];
        let (lan, p2p) = partition(&peers, &[], &endpoints, &empty_meta(), "");
        // 直连私网观测压过端点簿公网污染 → LAN 卡片；49730 是 113 拨入的
        // 临时源端口，展示规整为监听口 7070（IP 保留——真实链路落点）
        assert_eq!(lan.len(), 1, "直连私网观测 → LAN（端点簿污染不改变分类）");
        assert_eq!(lan[0].node_id, lan_hex);
        assert_eq!(
            lan[0].addr, "192.0.2.113:7070",
            "LAN 行展示规整后的可拨地址"
        );
        assert!(lan[0].connected);
        // 直连公网观测 → P2P 卡片，addr 用直连观测
        assert_eq!(p2p.len(), 1, "直连公网观测 → p2p");
        assert_eq!(p2p[0].node_id, wan_hex);
        assert_eq!(p2p[0].addr.as_deref(), Some("203.0.113.9:40022"));
        assert_eq!(p2p[0].source, "peer");
    }

    // —— 4d) 入站临时端口规整（2026-08-25）：LAN 卡片 addr 展示对端**监听口**
    //        而非入站 socket 的临时源端口——underlay 优先（自报监听口），
    //        无 underlay 兜底默认口 7070；端口已 7070 原样；桶灰点邻居的观测
    //        临时口同样规整；回环观测条目整条不输出（2026-08-25 定调） ——

    #[test]
    fn partition_regularizes_inbound_ephemeral_port_to_listen_port() {
        // 场景对齐真实故障：113 拨入 106，106 侧 observed 是 113 的出站临时
        // 源端口（每次重连都变，照着拨必失败）——IP 对、端口应换监听口。
        let ephemeral = random_id(); // 直连观测 49730、无 underlay → 展示 :7070
        let underlaid = random_id(); // 有 underlay（自报监听口 8000）→ 用 underlay
        let already = random_id(); // 观测端口已是 7070 → 原样
        let local = random_id(); // 回环观测（本机多实例）→ 整条不输出
        let gray = random_id(); // 桶内未直连邻居，观测临时口 → 展示 :7070
        let ephemeral_hex = ephemeral.to_hex();
        let underlaid_hex = underlaid.to_hex();
        let already_hex = already.to_hex();
        let local_hex = local.to_hex();
        let gray_short = short_hex(&gray.to_hex());
        let peers = vec![
            PeerInfo {
                observed_addr: Some("192.0.2.113:49730".parse().unwrap()),
                ..peer(ephemeral, None, true, false)
            },
            PeerInfo {
                observed_addr: Some("192.0.2.113:49731".parse().unwrap()),
                ..peer(underlaid, Some("192.0.2.99:8000"), true, true)
            },
            PeerInfo {
                observed_addr: Some("192.168.1.50:7070".parse().unwrap()),
                ..peer(already, None, true, false)
            },
            PeerInfo {
                observed_addr: Some("127.0.0.1:41003".parse().unwrap()),
                ..peer(local, None, true, false)
            },
        ];
        let endpoints = vec![endpoint(gray, "192.168.1.113:52001")];
        let buckets = vec![BucketStat {
            po: 1,
            count: 1,
            entries: vec![gray_short.clone()],
        }];
        let (lan, p2p) = partition(&peers, &buckets, &endpoints, &empty_meta(), "");
        assert_eq!(lan.len(), 4);
        let addr_of = |id: &str| lan.iter().find(|n| n.node_id == id).unwrap().addr.clone();
        // 1) 无 underlay 的入站临时口 → 规整为 P2P 默认监听口（IP 保留）
        assert_eq!(
            addr_of(&ephemeral_hex),
            "192.0.2.113:7070",
            "入站临时端口 49730 规整为监听口 7070"
        );
        // 2) 有 underlay → 用 underlay（自报监听口，非 7070 也不规整）
        assert_eq!(
            addr_of(&underlaid_hex),
            "192.0.2.99:8000",
            "underlay 优先，端口 8000 原样展示"
        );
        // 3) 观测端口已是 7070 → 原样
        assert_eq!(addr_of(&already_hex), "192.168.1.50:7070");
        // 4) 回环观测条目整条不输出（lan/p2p 均无——即使已直连）
        assert!(
            !lan.iter().any(|n| n.node_id == local_hex)
                && !p2p.iter().any(|n| n.node_id == local_hex),
            "回环观测条目直接不输出（2026-08-25 定调：无论怎么产生都屏蔽）"
        );
        // 5) 桶内未直连邻居的观测临时口同样规整（灰点，无 underlay 可查）
        assert_eq!(
            addr_of(&gray_short),
            "192.168.1.113:7070",
            "桶灰点观测临时口 52001 规整为 7070"
        );
        assert!(p2p.is_empty());
    }

    // —— 5) 桶非直连节点：不在 peers 集的短 ID 追加进 p2p（source=bucket），在集的不重复；观测私网地址归 lan ——

    #[test]
    fn partition_appends_bucket_only_nodes_without_duplicates() {
        let connected = random_id();
        let remote_only = random_id();
        let lan_neighbor = random_id(); // 桶内已知私网地址、未直连 → LAN 灰点
        let connected_short = short_hex(&connected.to_hex());
        let remote_only_short = short_hex(&remote_only.to_hex());
        let lan_neighbor_short = short_hex(&lan_neighbor.to_hex());
        let endpoints = vec![endpoint(lan_neighbor, "192.168.1.113:7070")];
        let peers = vec![peer(connected, Some("10.0.0.5:7070"), true, false)];
        let buckets = vec![BucketStat {
            po: 3,
            count: 3,
            entries: vec![
                connected_short.clone(),    // 已在 peers 集 → 不重复
                remote_only_short.clone(),  // 桶内非直连（无地址）→ p2p
                lan_neighbor_short.clone(), // 桶内非直连（私网观测）→ lan
            ],
        }];
        // self_id_str 已在上方声明
        // LAN：直连 peer + 桶内已知私网地址的未直连邻居
        let (lan, p2p) = partition(&peers, &buckets, &endpoints, &empty_meta(), "");
        assert_eq!(lan.len(), 2, "直连 peer + 桶内私网邻居");
        let gray = lan
            .iter()
            .find(|n| n.node_id == lan_neighbor_short)
            .expect("桶内私网观测邻居应落 lan");
        assert!(!gray.connected, "未直连邻居灰点");
        assert_eq!(gray.addr, "192.168.1.113:7070");
        // p2p 仅含桶内无地址信号的非直连节点（connected 的短式被去重）
        assert_eq!(
            p2p.len(),
            1,
            "bucket-only 无地址节点追加 1 条，peers 内短式去重"
        );
        assert_eq!(p2p[0].node_id, remote_only_short);
        assert_eq!(p2p[0].source, "bucket");
        assert!(!p2p[0].connected, "桶内非直连节点 connected=false");
        assert_eq!(p2p[0].addr, None);
    }

    // —— 5b) 回环彻底屏蔽（2026-08-25 用户定调：「127.0.0.1 无论怎么产生的，
    //        都应该屏蔽」——取代 2026-08-23 的「直连回环归 LAN / 非直连回环
    //        归 P2P」分级修复）：lan/p2p/inactive 三组对回环条目**零输出**——
    //        peers 任一地址信号为回环（直连观测 / underlay / 端点簿观测）、
    //        桶条目回环观测、meta 快照条目地址历史含回环（Inactive/混合皆算，
    //        Active 含回环则不参与富化），一律整条不出现；正常条目不受影响 ——

    #[test]
    fn loopback_entries_emitted_nowhere_across_three_groups() {
        let gossip_bucket = random_id(); // 桶内非直连 + 回环观测 → 三组皆无
        let gossip_peer = random_id(); // peer + 回环端点观测 → 三组皆无
        let direct_local = random_id(); // 直连 observed 127.0.0.1（本机多实例）→ 三组皆无
        let underlay_lo = random_id(); // underlay 127.0.0.1 → 三组皆无
        let meta_enrich_lo = random_id(); // meta Active 条目含回环 → 不参与富化
        let meta_inactive_lo = random_id(); // meta Inactive 仅回环 → inactive 不列出
        let meta_mixed_lo = random_id(); // meta Inactive 回环+公网混合 → 同样不列出
        let meta_inactive_ok = random_id(); // 正常 Inactive（对照组）→ inactive 列出
        let healthy = random_id(); // 正常私网 peer（对照组）→ lan
        let healthy_hex = healthy.to_hex();
        let enrich_hex = meta_enrich_lo.to_hex();
        let gossip_bucket_short = short_hex(&gossip_bucket.to_hex());
        let endpoints = vec![
            endpoint(gossip_bucket.clone(), "127.0.0.1:41001"),
            endpoint(gossip_peer.clone(), "127.0.0.1:41002"),
            endpoint(healthy.clone(), "192.168.1.113:7070"),
        ];
        let peers = vec![
            // 无直连观测 + 回环端点观测 → 不输出
            peer(gossip_peer, None, false, false),
            // 直连观测 127.0.0.1（connected——本机 socket 对端，不可伪造也屏蔽）
            PeerInfo {
                observed_addr: Some("127.0.0.1:41003".parse().unwrap()),
                ..peer(direct_local, None, true, false)
            },
            // 通告 underlay 为回环 → 不输出
            peer(underlay_lo, Some("127.0.0.1:41005"), true, true),
            // 正常私网 peer（对照组）→ lan
            peer(healthy, Some("192.168.1.113:7070"), true, false),
            // 正常私网 peer + meta 条目地址历史含回环 → 行保留但富化不采纳
            peer(
                meta_enrich_lo.clone(),
                Some("192.168.1.120:7070"),
                true,
                false,
            ),
        ];
        let buckets = vec![BucketStat {
            po: 2,
            count: 1,
            entries: vec![gossip_bucket_short.clone()],
        }];
        let inactive_state = |since| os_p2p::MetaState::Inactive { since };
        let meta = vec![
            // Active + 回环地址历史：MetaIndex 跳过——该行富化字段走默认值
            meta_entry(
                meta_enrich_lo.clone(),
                &["127.0.0.1:41009"],
                9_000,
                os_p2p::MetaState::Active {
                    score: 90,
                    consec_fail: 0,
                },
                os_p2p::MetaSource::Direct,
            ),
            // Inactive + 仅回环：inactive 分组不列出
            meta_entry(
                meta_inactive_lo.clone(),
                &["127.0.0.1:41010"],
                1_000,
                inactive_state(1_100),
                os_p2p::MetaSource::Direct,
            ),
            // Inactive + 回环/公网混合：同样不列出（含回环即跳过）
            meta_entry(
                meta_mixed_lo.clone(),
                &["127.0.0.1:41011", "203.0.113.9:40022"],
                1_000,
                inactive_state(1_200),
                os_p2p::MetaSource::Gossip,
            ),
            // 正常 Inactive（对照组）：inactive 分组照常列出
            meta_entry(
                meta_inactive_ok.clone(),
                &["203.0.113.8:40022"],
                1_000,
                inactive_state(1_300),
                os_p2p::MetaSource::Gossip,
            ),
        ];
        let idx = MetaIndex::build(&meta);
        let (lan, p2p) = partition(&peers, &buckets, &endpoints, &idx, "");
        // lan：仅两条正常私网行（healthy + meta_enrich_lo）；一切回环信号条目缺席
        assert_eq!(lan.len(), 2, "回环条目不进 lan，正常私网条目保留");
        let h = lan.iter().find(|n| n.node_id == healthy_hex).unwrap();
        assert_eq!(h.addr, "192.168.1.113:7070");
        // meta 含回环的条目不参与富化：行保留（自身信号正常）但字段走默认值
        let e = lan.iter().find(|n| n.node_id == enrich_hex).unwrap();
        assert_eq!(e.score, 0, "含回环地址的 meta 条目不作富化来源");
        assert_eq!(e.meta_source, None, "meta 回环泄漏在展示层不可见");
        // p2p：桶内回环观测条目同样不输出
        assert!(p2p.is_empty(), "桶内回环观测条目不进 p2p（p2p 应为空）");
        // inactive：仅对照组；仅回环 / 混合回环的 Inactive 条目均不列出
        let inactive = inactive_group(&meta, &lan, &p2p);
        let ids: Vec<&str> = inactive.iter().map(|n| n.node_id.as_str()).collect();
        assert_eq!(
            ids,
            vec![meta_inactive_ok.to_hex().as_str()],
            "inactive 仅列正常条目——回环条目（含混合）零输出"
        );
        // 三组兜底扫描：任何行不得携带回环地址
        assert!(lan.iter().all(|n| !n.addr.starts_with("127.")));
        assert!(p2p
            .iter()
            .all(|n| n.addr.as_deref().map_or(true, |a| !a.starts_with("127."))));
        assert!(inactive
            .iter()
            .flat_map(|n| n.addrs.iter())
            .all(|a| !a.starts_with("127.")));
    }

    // —— 5c) 元数据富化（2026-08-23，meta 组件接入）：lan/p2p 条目带
    //        status/score/last_seen/meta_source；无条目默认 active/0/0/null ——

    #[test]
    fn partition_enriches_entries_from_meta_registry() {
        let scored = random_id(); // Active{score:85} + Direct → LAN 私网 peer
        let gossiped = random_id(); // Active{score:60} + Gossip → P2P 公网 peer
        let unknown = random_id(); // 无 meta 条目 → 默认值
        let scored_hex = scored.to_hex();
        let gossiped_hex = gossiped.to_hex();
        let unknown_hex = unknown.to_hex();
        let meta = vec![
            meta_entry(
                scored.clone(),
                &["192.168.1.113:7070"],
                1_700_000_100,
                os_p2p::MetaState::Active {
                    score: 85,
                    consec_fail: 0,
                },
                os_p2p::MetaSource::Direct,
            ),
            meta_entry(
                gossiped.clone(),
                &["203.0.113.9:40022"],
                1_700_000_050,
                os_p2p::MetaState::Active {
                    score: 60,
                    consec_fail: 1,
                },
                os_p2p::MetaSource::Gossip,
            ),
        ];
        let peers = vec![
            peer(scored, Some("192.168.1.113:7070"), true, false),
            peer(gossiped, Some("203.0.113.9:40022"), false, true),
            peer(unknown, None, false, false), // NAT 后节点（无任何信号）→ p2p
        ];
        let idx = MetaIndex::build(&meta);
        let (lan, p2p) = partition(&peers, &[], &[], &idx, "");
        // LAN 条目富化：Active{85}/Direct 全量透出
        assert_eq!(lan.len(), 1);
        let l = &lan[0];
        assert_eq!(l.node_id, scored_hex);
        assert_eq!(l.status, "active");
        assert_eq!(l.score, 85);
        assert_eq!(l.last_seen, 1_700_000_100);
        assert_eq!(l.meta_source.as_deref(), Some("direct"));
        // P2P 条目富化：Gossip 来源 + 无条目默认值
        let g = p2p.iter().find(|p| p.node_id == gossiped_hex).unwrap();
        assert_eq!(g.status, "active");
        assert_eq!(g.score, 60);
        assert_eq!(g.last_seen, 1_700_000_050);
        assert_eq!(g.meta_source.as_deref(), Some("gossip"));
        let u = p2p.iter().find(|p| p.node_id == unknown_hex).unwrap();
        assert_eq!(u.status, "active", "无 meta 条目默认 active");
        assert_eq!(u.score, 0);
        assert_eq!(u.last_seen, 0);
        assert_eq!(u.meta_source, None);
    }

    // —— 5d) 非活跃分组（2026-08-23）：meta 判定 Inactive 且无活连接的节点
    //        移出 lan/p2p、单独列入 inactive；有活连接的 Inactive 以 lan/p2p
    //        为准（status 标 active）且不重复列入；meta-only 节点（不在路由表）
    //        也列出；快照次序保留（最近出局在前）——

    #[test]
    fn partition_moves_inactive_meta_nodes_to_inactive_group() {
        let dead = random_id(); // Inactive + 未连接 peer → 仅 inactive 分组
        let half_dead = random_id(); // Inactive + 活连接 → 留 lan（status=active）
        let ghost = random_id(); // Inactive + 不在路由表（meta-only）→ inactive
        let older = random_id(); // Inactive + 更早出局 → 排在 dead 之后
        let alive = random_id(); // Active → 正常进 lan
        let dead_hex = dead.to_hex();
        let half_dead_hex = half_dead.to_hex();
        let alive_hex = alive.to_hex();
        let inactive_state = |since| os_p2p::MetaState::Inactive { since };
        let meta = vec![
            // 快照次序：Inactive 段内按出局时刻降序（since 3000 在 2000 前）
            meta_entry(
                older.clone(),
                &["10.0.0.9:7070"],
                2_900,
                inactive_state(3000),
                os_p2p::MetaSource::Gossip,
            ),
            meta_entry(
                dead.clone(),
                &["192.168.1.113:7070"],
                1_900,
                inactive_state(2000),
                os_p2p::MetaSource::Direct,
            ),
            meta_entry(
                half_dead.clone(),
                &["192.168.1.114:7070"],
                1_950,
                inactive_state(2100),
                os_p2p::MetaSource::Direct,
            ),
            meta_entry(
                ghost.clone(),
                &["203.0.113.8:40022"],
                500,
                inactive_state(1000),
                os_p2p::MetaSource::Gossip,
            ),
            meta_entry(
                alive.clone(),
                &["192.168.1.106:7070"],
                2_000,
                os_p2p::MetaState::Active {
                    score: 95,
                    consec_fail: 0,
                },
                os_p2p::MetaSource::Direct,
            ),
        ];
        let peers = vec![
            peer(dead, Some("192.168.1.113:7070"), false, false), // 未连接 → 移出
            peer(half_dead, Some("192.168.1.114:7070"), true, false), // 活连接 → 留
            peer(alive, Some("192.168.1.106:7070"), true, false),
        ];
        let idx = MetaIndex::build(&meta);
        let (lan, p2p) = partition(&peers, &[], &[], &idx, "");
        // lan：活连接的 Inactive（status 压成 active）+ Active 各 1 条；dead 不在
        assert_eq!(lan.len(), 2, "未连接的 Inactive 移出 lan");
        let hd = lan.iter().find(|n| n.node_id == half_dead_hex).unwrap();
        assert_eq!(hd.status, "active", "活连接压过 meta Inactive（直连为准）");
        assert_eq!(hd.score, 0, "Inactive 富化分数记 0");
        let av = lan.iter().find(|n| n.node_id == alive_hex).unwrap();
        assert_eq!(av.score, 95);
        assert!(p2p.is_empty());
        // inactive 分组：dead + ghost（meta-only）+ older；half_dead 不重复列出；
        // 次序保留 meta 快照序（Inactive 段内最近出局在前——older→dead→ghost）
        let inactive = inactive_group(&meta, &lan, &p2p);
        let ids: Vec<&str> = inactive.iter().map(|n| n.node_id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                older.to_hex().as_str(),
                dead_hex.as_str(),
                ghost.to_hex().as_str()
            ],
            "since 降序；活连接的 Inactive 不重复；meta-only 节点也在列"
        );
        let d = &inactive[1];
        assert_eq!(d.addrs, vec!["192.168.1.113:7070".to_string()]);
        assert_eq!(d.score, 0);
        assert_eq!(d.last_seen, 1_900);
        assert_eq!(d.meta_source, "direct");
        assert_eq!(d.since, 2000);
        // 桶内 Inactive 短 ID 同样移出 lan/p2p（meta 短式索引命中；fixture 用
        // 非回环地址——回环条目的行为由 5b 专测，此处保 Inactive 门控语义）
        let bucket_dead = random_id();
        let bucket_dead_short = short_hex(&bucket_dead.to_hex());
        let meta2 = vec![meta_entry(
            bucket_dead.clone(),
            &["203.0.113.8:40022"],
            900,
            inactive_state(1000),
            os_p2p::MetaSource::Gossip,
        )];
        let idx2 = MetaIndex::build(&meta2);
        let buckets = vec![BucketStat {
            po: 1,
            count: 1,
            entries: vec![bucket_dead_short],
        }];
        let (lan2, p2p2) = partition(&[], &buckets, &[], &idx2, "");
        assert!(
            lan2.is_empty() && p2p2.is_empty(),
            "桶内 Inactive 移出 lan/p2p"
        );
        assert_eq!(
            inactive_group(&meta2, &lan2, &p2p2)[0].node_id,
            bucket_dead.to_hex(),
            "inactive 分组以全量 ID 列出（可直接寻址手动心跳）"
        );
    }

    // —— 6) 真实 mesh：对端经本机 LAN IP 落 lan（connected=true + 真实 addr，
    //        2026-08-25 起回环 mesh 不可见——回环条目三组零输出）；self 字段全量 ——

    #[tokio::test]
    async fn combined_real_mesh_partitions_neighbor_and_self_fields() {
        // B 先起（公网角色，监听非回环本机地址——回环观测不入册也不展示），
        // A 引导到 B → 连接建立 + 桶收录
        let lan_ip = non_loopback_local_ipv4().to_string();
        let b = P2pNode::spawn(P2pConfig {
            listen: lan_listen(),
            public: true,
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        let a = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            bootstrap: vec![b.listen_addr()],
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        let h = NodeViewRouteHandler::new(
            a.clone(),
            "node-a".into(),
            false,
            crate::handlers::im::ImRouteHandler::with_empty().federation(),
        );

        // 等待 A↔B 建立并出现在 combined 视图（测试节奏下数秒内）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let body = loop {
            let resp = h.handle(get_req(PATH_COMBINED)).await.unwrap();
            assert_eq!(resp.status, 200);
            let hit = resp.body["lan"].as_array().is_some_and(|l| !l.is_empty());
            if hit || std::time::Instant::now() > deadline {
                break resp.body;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        };

        // B（LAN IP underlay）落 LAN 卡片，字段来自真实路由表
        let lan = body["lan"].as_array().expect("lan 数组");
        let entry = lan
            .iter()
            .find(|p| p["node_id"] == b.self_id().to_hex())
            .expect("A 的 LAN 视图应含 B");
        assert_eq!(entry["connected"], true, "A↔B 已连接");
        assert_eq!(entry["public"], true, "B 是公网服务节点");
        assert_eq!(entry["role"], "hub", "public 对端角色 hub");
        assert!(
            entry["addr"]
                .as_str()
                .unwrap()
                .starts_with(&format!("{lan_ip}:")),
            "LAN 行带真实 underlay 地址（非回环本机地址 mesh）"
        );
        // im_public 字段恒存在：无探针（set_p2p 未注入）时为 null（查询中语义）
        assert!(
            entry.get("im_public").is_some(),
            "节点行应携带 im_public 字段"
        );
        assert_eq!(entry["im_public"], serde_json::Value::Null);
        // 元数据富化字段（meta 组件接入）：直连对端 Active/Direct，建档 50 起
        assert_eq!(entry["status"], "active", "直连对端 meta 状态 active");
        assert!(
            entry["score"].as_u64().unwrap_or(0) >= 50,
            "连接即活性证据（建档 50 起）"
        );
        assert!(entry["last_seen"].as_u64().unwrap_or(0) > 0);
        assert_eq!(entry["meta_source"], "direct");
        // 非活跃分组恒存在（空态为数组——mesh 内无五振出局节点）
        assert!(
            body["inactive"].as_array().is_some_and(|a| a.is_empty()),
            "inactive 空数组"
        );

        // self：与 Handle 同源的身份/昵称/角色/监听
        assert_eq!(body["self"]["enabled"], true);
        assert_eq!(body["self"]["node_id"], a.self_id().to_hex());
        assert_eq!(body["self"]["overlay_addr"], a.self_id().overlay().to_hex());
        assert_eq!(body["self"]["name"], "node-a");
        assert_eq!(body["self"]["public"], false);
        assert_eq!(body["self"]["role"], "edge", "非 public 节点角色 edge");
        assert_eq!(body["self"]["listen"], a.listen_addr().to_string());
        assert!(!body["self"]["hostname"].as_str().unwrap().is_empty());

        // ladder 四计数齐备（u64）
        for field in ["direct", "punched", "relayed", "punch_failed"] {
            assert!(
                body["ladder"][field].is_u64(),
                "ladder 应含 u64 字段 {field}，实际 {}",
                body["ladder"]
            );
        }

        a.shutdown().await;
        b.shutdown().await;
    }

    // —— 7) 兜底：未声明方法/路径 404；Default 即未启用 ——

    #[tokio::test]
    async fn unmatched_route_returns_404() {
        let h = NodeViewRouteHandler::new_disabled();
        let mut req = get_req(PATH_COMBINED);
        req.method = HttpMethod::Post;
        let resp = h.handle(req).await.unwrap();
        assert_eq!(resp.status, 404);
        assert!(resp.body["error"].as_str().unwrap().contains("未匹配"));
    }

    #[test]
    fn default_trait_is_disabled() {
        fn assert_default<T: Default>() {}
        assert_default::<NodeViewRouteHandler>();
        assert!(!NodeViewRouteHandler::default().is_enabled());
    }

    // —— 8) im_public 联动（2026-08-23）：双节点 mesh + 对端应答桥——
    //    对方关闭（lobby_public=false）→ combined im_public=false；对端开开关
    //    后经阻塞查询刷新缓存 → combined im_public=true。（开发期缺省开放，
    //    故下方显式关闭后再验 denied。）

    /// 起 A（查询端：NodeView + ImFederation set_p2p）+ B（应答端：ImFederation
    /// set_p2p + FederationBridge 消费入站查询），等 A↔B 直连建立。
    async fn spawn_probe_pair() -> (
        NodeViewRouteHandler,
        crate::handlers::im::ImFederation,
        crate::handlers::im::ImFederation,
        os_p2p::Handle,
        os_p2p::Handle,
    ) {
        // B 监听非回环本机地址（回环 mesh 对 combined 不可见——2026-08-25 定调）
        let b_node = P2pNode::spawn(P2pConfig {
            listen: lan_listen(),
            public: true,
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        let a_node = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            bootstrap: vec![b_node.listen_addr()],
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        let h_a = crate::handlers::im::ImRouteHandler::with_empty();
        let h_b = crate::handlers::im::ImRouteHandler::with_empty();
        let fed_a = h_a.federation();
        let fed_b = h_b.federation();
        fed_a.set_p2p(a_node.clone(), "node-a".into());
        fed_b.set_p2p(b_node.clone(), "node-b".into());
        // B 侧入站分发桥（answer_lobby_query 在此被触发）
        let bridge = crate::handlers::p2p::FederationBridge {
            im: Some(fed_b.clone()),
            nexhub: None,
            live: None,
            api_market: None,
        };
        let mut rx = b_node.on_msg();
        tokio::spawn(async move {
            while let Ok(m) = rx.recv().await {
                bridge.dispatch(&m);
            }
        });
        // 等双向连接建立（测试节奏下数秒内）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            let peers = a_node.peers().await;
            if peers
                .iter()
                .any(|p| p.id == *b_node.self_id() && p.connected)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let view = NodeViewRouteHandler::new(a_node.clone(), "node-a".into(), false, fed_a.clone());
        (view, fed_a, fed_b, a_node, b_node)
    }

    #[tokio::test]
    async fn combined_reflects_remote_lobby_toggle_state() {
        let (view, fed_a, fed_b, a, b) = spawn_probe_pair().await;
        let b_hex = b.self_id().to_hex();
        // 1) B 关闭开关（开发期缺省开放，显式关）：阻塞查询拿 denied →
        //    缓存 Some(false) → combined false
        fed_b.set_lobby_public(false);
        let node = os_p2p::NodeId::parse(&b_hex).unwrap();
        let v = fed_a
            .remote_lobby(&node, std::time::Duration::from_secs(8))
            .await
            .expect("关闭开关下也应答（denied）");
        assert!(!v.public, "关闭后不允许浏览");
        assert_eq!(v.error.as_deref(), Some("denied"));
        let resp = view.handle(get_req(PATH_COMBINED)).await.unwrap();
        assert_eq!(resp.status, 200);
        let row = resp.body["lan"]
            .as_array()
            .expect("lan 数组")
            .iter()
            .find(|p| p["node_id"] == b_hex.as_str())
            .expect("B 应在 A 的 LAN 视图");
        assert_eq!(row["im_public"], false, "combined 应带 im_public=false");

        // 2) B 打开开关 → 再查询刷新缓存 → combined true（开关切换联动）
        fed_b.set_lobby_public(true);
        let v = fed_a
            .remote_lobby(&node, std::time::Duration::from_secs(8))
            .await
            .expect("开放后应答");
        assert!(v.public);
        let resp = view.handle(get_req(PATH_COMBINED)).await.unwrap();
        let row = resp.body["lan"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["node_id"] == b_hex.as_str())
            .unwrap();
        assert_eq!(row["im_public"], true, "开关切换后 combined 联动为 true");

        a.shutdown().await;
        b.shutdown().await;
    }

    // —— 9) 探针状态语义：未应答节点（无应答端）im_public 为 null（查询中），
    //        combined 不因查询阻塞（非阻塞 status，下一次轮询才见应答）——

    #[tokio::test]
    async fn combined_im_public_null_when_no_reply_yet() {
        // B 监听非回环本机地址（回环 mesh 对 combined 不可见——2026-08-25 定调）
        let b = P2pNode::spawn(P2pConfig {
            listen: lan_listen(),
            public: true,
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        let a = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            bootstrap: vec![b.listen_addr()],
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        let h_a = crate::handlers::im::ImRouteHandler::with_empty();
        let fed_a = h_a.federation();
        fed_a.set_p2p(a.clone(), "node-a".into());
        // B 是裸节点（无 ImFederation 应答桥）→ 查询永远无应答
        let view = NodeViewRouteHandler::new(a.clone(), "node-a".into(), false, fed_a);
        // 等连接建立后拉 combined（探针发出首查但无应答）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let body = loop {
            let resp = view.handle(get_req(PATH_COMBINED)).await.unwrap();
            let hit = resp.body["lan"]
                .as_array()
                .is_some_and(|l| l.iter().any(|p| p["node_id"] == b.self_id().to_hex()));
            if hit || std::time::Instant::now() > deadline {
                break resp.body;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        };
        let row = body["lan"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["node_id"] == b.self_id().to_hex())
            .unwrap();
        assert_eq!(
            row["im_public"],
            serde_json::Value::Null,
            "无应答节点 im_public=null（查询中）"
        );
        a.shutdown().await;
        b.shutdown().await;
    }
}
