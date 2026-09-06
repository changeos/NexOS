//! 对上层服务的接入面——设计 §3「api」+ 引擎装配。
//!
//! [`P2pNode::spawn`] 起一组 tokio 任务（见下），返回 [`Handle`] 作为唯一控制面：
//!
//! ```text
//!  ┌────────────── 上层服务（联邦大厅 / IM Federation / p2p-node CLI）───────┐
//!  │  Handle::send(to,payload)   on_msg() 订阅   peers()   connect(node_id) │
//!  │  dial(addr)——按地址直拨（手动添加节点，bootstrap 拨号同款路径）        │
//!  └───────────┬──────────────────┬─────────────────┬──────────┬────────────┘
//!              ▼                  ▼                 ▼          ▼
//!  ┌ cmd_loop ─┴────────────┐  ┌ broadcast::P2pMsg ┐  ┌ State 快照 ┐ ┌连接阶梯┐
//!  │ route_frame（SEND 路由）│  └───────────────────┘  └────────────┘ └────────┘
//!  └───┬────────────────────┘
//!      ▼  next_hop: 直连优先 → 我是其 relay（信箱）→ 经 relay 转发 → lookup 重试
//!  ┌ Shared（Mutex<State>：conns + KBuckets + RelayState + EndpointBook
//!  │        + pending_out + 打洞会话 + 连接阶梯统计）                    │
//!  └───┬──────────────┬─────────────────┬──────────────────┬───────────┘
//!      ▼              ▼                 ▼                  ▼
//!  accept_loop    conn reader/writer  maintenance_loop   bootstrap_task
//!  (入站握手)      (ECDH 会话密钥上的  (ping 剔除/桶刷新/  (mDNS 种子 + env
//!                  加密帧收发)         重拨/端点 TTL)      引导 + 保活重拨)
//! ```
//!
//! 所有任务经 [`spawn_tracked`] 登记 AbortHandle，[`Handle::shutdown`] 一次叫停。
//! State 用 std Mutex 短临界区保护——**持锁绝不 await**（无死锁面）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::crypto::SessionCipher;
use crate::endpoints::{EndpointBook, EndpointEntry, EndpointGossip, ENDPOINTS_GOSSIP_LIMIT};
use crate::identity::{NodeId, NodeIdentity, OverlayAddr};
use crate::kad::{BucketStat, KBuckets, NodeInfo, K};
use crate::punch::{ConnectError, ConnectPath, LadderStats};
use crate::relay::{self, RelayState};
use crate::transport::{read_frame_enc, write_frame_enc, Frame, FrameKind, HelloCtx, PeerHello};
use os_identity::IdentityConflict;

/// SEND 待路由缓冲：每目标上限（超出丢最旧）。
pub const PENDING_OUT_LIMIT: usize = 128;
/// 应用消息广播通道容量。
const MSG_CHANNEL_CAPACITY: usize = 1024;
/// 入站/出站握手整体超时上限。
const HANDSHAKE_CEILING: Duration = Duration::from_secs(5);

// ============================================================================
// 配置
// ============================================================================

/// 组网节奏参数（生产默认；测试用 [`Timing::testing`] 压缩到亚秒级）。
#[derive(Debug, Clone)]
pub struct Timing {
    /// 空闲多久后发起 PING 存活探测。
    pub ping_interval: Duration,
    /// PING 等待 PONG 的超时（超时记一次失败）。
    pub ping_timeout: Duration,
    /// 连续失败几次后节点除名（桶剔除 + 路由级联清理）。
    pub max_failures: u32,
    /// 桶刷新间隔（对随机 target 的定期 walk）。
    pub refresh_interval: Duration,
    /// 中继注册 TTL（被中继节点这么久不重连即连同信箱清理）。
    pub relay_ttl: Duration,
    /// 桶满替换策略中 LRS 条目的陈旧判定线。
    pub stale_after: Duration,
    /// 单次 FINDNODE 查询超时。
    pub query_timeout: Duration,
    /// 引导连接保活重拨退避。
    pub reconnect_backoff: Duration,
    /// 观测端点 TTL（地址簿条目这么久未续期即清理——死映射不滞留）。
    pub endpoint_ttl: Duration,
    /// TCP 打洞每轮尝试间隔（同时打开重试节奏）。
    pub punch_retry_interval: Duration,
    /// 打洞端点交换（PUNCH1→PUNCH2）整体超时。
    pub punch_setup_timeout: Duration,
    /// mDNS 首轮收集窗口（发现邻居优先于 env 引导拨号）。
    pub mdns_first_pass: Duration,
    /// 元数据心跳引擎 tick（meta 组件探测节奏的基单位：分数 ≥80 每 6 tick /
    /// 50-79 每 3 tick / <50 每 tick；元数据交互每 6 tick 一次）。
    pub meta_tick: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            ping_interval: Duration::from_secs(5),
            ping_timeout: Duration::from_secs(3),
            max_failures: 2,
            refresh_interval: Duration::from_secs(30),
            relay_ttl: Duration::from_secs(600),
            stale_after: Duration::from_secs(60),
            query_timeout: Duration::from_secs(2),
            reconnect_backoff: Duration::from_secs(1),
            endpoint_ttl: Duration::from_secs(600),
            punch_retry_interval: Duration::from_millis(800),
            punch_setup_timeout: Duration::from_secs(3),
            mdns_first_pass: Duration::from_secs(1),
            meta_tick: Duration::from_secs(5),
        }
    }
}

impl Timing {
    /// 测试节奏（亚秒级：剔除/刷新/收敛/打洞都在数秒内可观测）。
    #[must_use]
    pub fn testing() -> Self {
        Self {
            ping_interval: Duration::from_millis(300),
            ping_timeout: Duration::from_millis(250),
            max_failures: 2,
            refresh_interval: Duration::from_millis(700),
            relay_ttl: Duration::from_secs(5),
            stale_after: Duration::from_millis(300),
            query_timeout: Duration::from_millis(800),
            reconnect_backoff: Duration::from_millis(150),
            endpoint_ttl: Duration::from_secs(3),
            punch_retry_interval: Duration::from_millis(120),
            punch_setup_timeout: Duration::from_millis(600),
            mdns_first_pass: Duration::from_millis(120),
            meta_tick: Duration::from_millis(150),
        }
    }
}

/// 节点启动配置（[`P2pNode::spawn`] 输入；env 便捷解析见 bootstrap::config_from_env）。
#[derive(Clone)]
pub struct P2pConfig {
    /// 监听地址（`:7070` 风格请经 bootstrap::parse_listen；测试用 `127.0.0.1:0`）。
    pub listen: SocketAddr,
    /// 冷启动引导节点列表（`NEXOS_P2P_BOOTSTRAP`）。
    pub bootstrap: Vec<SocketAddr>,
    /// 公网服务节点声明（`NEXOS_P2P_PUBLIC=1`）：通告 underlay + 承担 bootstrap/relay。
    pub public: bool,
    /// 显式通告地址（`NEXOS_P2P_ADVERTISE=ip:port`）：NAT 后的服务节点监听
    /// `0.0.0.0` 而对外另有公网 IP（如云主机）——覆盖"public → 通告监听地址"
    /// 的默认；设置即隐含 public=1（能被拨到才有资格承担 bootstrap/relay）。
    pub advertise: Option<SocketAddr>,
    /// 身份（None = CSPRNG 生成；测试复现身份用 [`NodeIdentity::from_seed`]）。
    pub identity: Option<NodeIdentity>,
    /// 组网节奏（测试注入 [`Timing::testing`]）。
    pub timings: Timing,
    /// 出站拨号绑定监听端口（SO_REUSEADDR 映射复用）——TCP 打洞的"稳定 NAT
    /// 映射"前提：交换所观测到的端口即真实可拨入的端口。默认关（普通 NAT 节点
    /// 出站用临时端口，观测端点仅供诊断）。
    pub dial_from_listen_port: bool,
    /// mDNS LAN 种子（`_nexos-p2p._tcp` 广播/发现；不可用静默降级 env 引导）。
    pub mdns_enabled: bool,
    /// 节点元数据注册表持久化文件（meta 组件；None = 纯内存——测试用。
    /// env 便捷解析见 bootstrap::config_from_env：`NEXOS_P2P_META_FILE` 或
    /// key_file 同目录 `node-meta.json`）。
    pub meta_file: Option<std::path::PathBuf>,
    /// 身份账本注入（os-identity 组件——**共享实例形态**：os-api 装配层建好
    /// 持久化账本（`/tank/os-data/identity-ledger.json`，env `NEXOS_IDENTITY_FILE`）
    /// 注入本节点，同时自留一份暴露 REST 观察面）。
    ///
    /// **内嵌 vs 注入的权衡**（设计裁决，详见 docs/IDENTITY_COMPONENT.md）：
    /// 曾考虑纯事件回调（`identity_sink: Arc<dyn Fn(IdentityEvent)>`）——但
    /// 传输层的指纹判定（`owns_addr` 地址归属对比）需要**查询反馈**而非单向
    /// 通知，sink 形态下 p2p 仍需自建一份影子账本才能判定，等于两份账本；
    /// 故选共享实例：None 时本节点自建**本地内存账本**（p2p-node CLI 独立跑、
    /// 单测默认路径——行为同现在，最小记账仍要做），Some 时与装配方共享
    /// 同一实例（唯一权威源，写读一致）。
    pub identity_ledger: Option<os_identity::SharedLedger>,
    /// 本节点是否声明可作**网络出口**（network-exit，2026-08-30）：digest 自
    /// 广播首条携带 `exit_offered` 位，其他节点经 gossip 学到「谁是出口」。
    /// env `NEXOS_P2P_EXIT_OFFER=1`（bootstrap::config_from_env）；运行期经
    /// [`Handle::set_exit_offered`] 切换（权威源在 network-exit 组件状态文件，
    /// 启动时推送过来）。默认 false。
    pub exit_offered: bool,
}

impl Default for P2pConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], crate::bootstrap::P2P_PORT_DEFAULT)),
            bootstrap: Vec::new(),
            public: false,
            advertise: None,
            identity: None,
            timings: Timing::default(),
            dial_from_listen_port: false,
            mdns_enabled: false,
            meta_file: None,
            identity_ledger: None,
            exit_offered: false,
        }
    }
}

// ============================================================================
// 对外消息与观察面
// ============================================================================

/// 交付给上层的应用消息（`Handle::on_msg` 订阅）。
#[derive(Debug, Clone)]
pub struct P2pMsg {
    /// 发送者 NodeID（其连接已经握手签名验证——链路内不可伪造）。
    pub from: NodeId,
    /// 应用载荷（send 帧的 payload 原文）。
    pub payload: serde_json::Value,
    /// 剩余 ttl（收方可见——经中继路径会 < 16）。
    pub ttl: u8,
    /// 已穿越中继跳数（0 = 直连送达）。
    pub hops: u8,
}

/// peers() 单条：路由表条目 + 连接状态。
#[derive(Debug, Clone, Serialize)]
pub struct PeerInfo {
    /// 节点身份。
    pub id: NodeId,
    /// 可拨 underlay（None = NAT，只能经中继）。
    pub underlay: Option<SocketAddr>,
    /// 公网服务节点。
    pub public: bool,
    /// NODES 学到的中继者。
    pub relay: Option<NodeId>,
    /// 当前是否有已认证直连。
    pub connected: bool,
    /// 是否经我中继（我是它的 relay）。
    pub relayed_by_me: bool,
    /// 我掌握的可达性路由 `{该节点 → 经谁}`。
    pub route_via: Option<NodeId>,
    /// 直连 socket 的对端地址（握手验证过的第一手信号——分类/诊断的最高
    /// 优先地址依据；仅 connected 时有值，断连/未直连为 None）。与 `underlay`
    /// （对端**自报**通告）和端点簿观测（gossip 汇聚、可能被公网锚点覆盖）
    /// 不同，这是本机 socket 的 `peer_addr`，不可伪造。
    pub observed_addr: Option<SocketAddr>,
}

/// 身份冲突观测条目（仅提示不阻断——身份=密钥是设计特性，多 OS 共用同一
/// 私钥时权限共享；此观测面只让本机用户**知情**）。
///
/// 触发条件：对端握手自报 NodeID == 本机 NodeID（同一公钥从另一地址进入）。
/// 记账在 os-identity 账本（`IdentityLedger::record_conflict`，随账本持久化
/// ——重启不再清零），按观测地址分条累计。结构定义已迁至 os-identity
/// （形状不变），本 crate 经 lib.rs 转发导出。
/// 当前 unix 秒（观测时间戳；SystemTime 倒拨钳制为 0）。
pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ============================================================================
// Handle（控制面）
// ============================================================================

/// 节点控制句柄（Clone 共享；`shutdown` 消费式优雅停机）。
#[derive(Clone)]
pub struct Handle {
    self_id: NodeId,
    listen_addr: SocketAddr,
    cmd: mpsc::UnboundedSender<Cmd>,
    msg_tx: broadcast::Sender<P2pMsg>,
}

enum Cmd {
    Send {
        to: NodeId,
        payload: serde_json::Value,
    },
    Peers {
        resp: oneshot::Sender<Vec<PeerInfo>>,
    },
    Buckets {
        resp: oneshot::Sender<Vec<BucketStat>>,
    },
    Route {
        id: NodeId,
        resp: oneshot::Sender<Option<NodeId>>,
    },
    /// 连接阶梯（直连 → 打洞 → 中继）；打洞可耗时数秒——cmd 循环只转发到
    /// 专用任务，不阻塞其他命令。
    Connect {
        target: NodeId,
        resp: oneshot::Sender<Result<ConnectPath, ConnectError>>,
    },
    /// 按地址直拨（手动添加节点）：TCP connect + 握手 + 注册（bootstrap 拨号
    /// 同款路径）；拨号+握手可耗时数秒——cmd 循环只转发到专用任务，不阻塞。
    Dial {
        addr: SocketAddr,
        resp: oneshot::Sender<Result<NodeId, String>>,
    },
    KnownEndpoints {
        resp: oneshot::Sender<Vec<EndpointEntry>>,
    },
    LookupEndpoint {
        id: NodeId,
        resp: oneshot::Sender<Option<SocketAddr>>,
    },
    Ladder {
        resp: oneshot::Sender<LadderStats>,
    },
    /// 身份冲突观测快照（NodeID 冲突连接记账；os-identity 账本——注入实例
    /// 随账本文件持久化，重启不再清零）。
    IdentityConflicts {
        resp: oneshot::Sender<Vec<IdentityConflict>>,
    },
    /// 节点元数据注册表快照（meta 组件——健康排名观察面）。
    NodeMeta {
        resp: oneshot::Sender<Vec<crate::meta::NodeMetaEntry>>,
    },
    /// 手动触发元数据心跳 / 复活（Inactive → Active 并立即探测一次）。
    MetaReactivate {
        id: NodeId,
        resp: oneshot::Sender<bool>,
    },
    /// 切换本节点网络出口声明（network-exit：digest 自广播的 exit_offered 位；
    /// 下一轮 gossip（≤6 tick）生效）。
    SetExitOffered {
        offered: bool,
        resp: oneshot::Sender<bool>,
    },
    Shutdown {
        done: oneshot::Sender<()>,
    },
}

impl Handle {
    /// 本节点 NodeID。
    #[must_use]
    pub fn self_id(&self) -> &NodeId {
        &self.self_id
    }

    /// 指纹判断：目标 NodeID 是否"本地"（== 本机 NodeID，含同私钥多 OS 实例
    /// 场景——身份=密钥，同指纹即同权限域，消息已本地落库，无需经 P2P 自回路）。
    ///
    /// 发送侧（联邦广播 / 定向应答）用它跳过本地指纹目标：`send` 到本机
    /// NodeID 会走本地回环交付（自回路），只造成重复入库。同步纯比较、
    /// 无锁无 await。
    #[must_use]
    pub fn is_local_target(&self, id: &NodeId) -> bool {
        id == &self.self_id
    }

    /// 实际绑定监听地址（`:0` 随机端口后可查询）。
    #[must_use]
    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    /// 发送应用消息（fire-and-forget：无路由时入 pending_out 并触发 lookup，
    /// 送达效果经接收方 on_msg 体现）。
    pub fn send(&self, to: &NodeId, payload: serde_json::Value) {
        let _ = self.cmd.send(Cmd::Send {
            to: to.clone(),
            payload,
        });
    }

    /// 订阅应用消息（broadcast——多订阅者各自独立；错过即失，不做回放）。
    #[must_use]
    pub fn on_msg(&self) -> broadcast::Receiver<P2pMsg> {
        self.msg_tx.subscribe()
    }

    /// 路由表观察面（全部已知节点 + 连接状态）。
    pub async fn peers(&self) -> Vec<PeerInfo> {
        let (resp, rx) = oneshot::channel();
        if self.cmd.send(Cmd::Peers { resp }).is_ok() {
            rx.await.unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    /// 非空桶摘要（邻域阶 / 数量 / 短 ID——网络页拓扑展示）。
    pub async fn buckets_summary(&self) -> Vec<BucketStat> {
        let (resp, rx) = oneshot::channel();
        if self.cmd.send(Cmd::Buckets { resp }).is_ok() {
            rx.await.unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    /// 查询我掌握的可达性路由：`Some(relay)` = 经 relay 可达（None = 直连域或未知）。
    pub async fn route(&self, id: &NodeId) -> Option<NodeId> {
        let (resp, rx) = oneshot::channel();
        if self
            .cmd
            .send(Cmd::Route {
                id: id.clone(),
                resp,
            })
            .is_ok()
        {
            rx.await.unwrap_or(None)
        } else {
            None
        }
    }

    /// 连接阶梯：已直连短路 → underlay 直拨 → 观测端点 TCP 打洞 → 中继兜底。
    /// 返回实际建立路径（[`ConnectPath`]）；打洞全败且无中继路由 →
    /// [`ConnectError::PunchFailed`]。
    pub async fn connect(&self, target: &NodeId) -> Result<ConnectPath, ConnectError> {
        let (resp, rx) = oneshot::channel();
        let fallback = || Err(ConnectError::NoRoute(Box::new(target.clone())));
        if self
            .cmd
            .send(Cmd::Connect {
                target: target.clone(),
                resp,
            })
            .is_ok()
        {
            match rx.await {
                Ok(result) => result,
                // 命令通道存活但任务被 abort（停机边缘）——按无路由上报
                Err(_) => fallback(),
            }
        } else {
            fallback()
        }
    }

    /// 观测端点地址簿快照（`{NodeID → 网络观测到的 ip:port}`，按新鲜度降序）。
    pub async fn known_endpoints(&self) -> Vec<EndpointEntry> {
        let (resp, rx) = oneshot::channel();
        if self.cmd.send(Cmd::KnownEndpoints { resp }).is_ok() {
            rx.await.unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    /// 按地址拨号（手动添加节点）：TCP connect + 双向挑战-签名 + ECDH 握手 +
    /// 注册（bootstrap 拨号同款路径；成功即入桶并计入 peers）。成功返回对端
    /// NodeID；失败（不可达 / 握手失败 / 超时）返回原因字符串。
    pub async fn dial(&self, addr: SocketAddr) -> Result<NodeId, String> {
        let (resp, rx) = oneshot::channel();
        if self.cmd.send(Cmd::Dial { addr, resp }).is_ok() {
            rx.await
                .unwrap_or_else(|_| Err("节点任务已停机".to_string()))
        } else {
            Err("节点任务已停机".to_string())
        }
    }

    /// 查某节点的观测端点（打洞目标 / NAT 映射诊断）。
    pub async fn lookup_endpoint(&self, id: &NodeId) -> Option<SocketAddr> {
        let (resp, rx) = oneshot::channel();
        if self
            .cmd
            .send(Cmd::LookupEndpoint {
                id: id.clone(),
                resp,
            })
            .is_ok()
        {
            rx.await.unwrap_or(None)
        } else {
            None
        }
    }

    /// 连接阶梯统计（direct / punched / relayed / punch_failed——CLI status）。
    pub async fn ladder_stats(&self) -> LadderStats {
        let (resp, rx) = oneshot::channel();
        if self.cmd.send(Cmd::Ladder { resp }).is_ok() {
            rx.await.unwrap_or_default()
        } else {
            LadderStats::default()
        }
    }

    /// 身份冲突观测快照（对端 NodeID == 本机 NodeID 的连接记账，按观测地址
    /// 分条；空 = 无冲突）。仅提示不阻断——「多个 OS 用同一私钥进入」时本机
    /// 用户的知情观测面。
    pub async fn identity_conflicts(&self) -> Vec<IdentityConflict> {
        let (resp, rx) = oneshot::channel();
        if self.cmd.send(Cmd::IdentityConflicts { resp }).is_ok() {
            rx.await.unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    /// 优雅停机：断开全部连接（对端立即感知 EOF）、叫停全部任务。
    pub async fn shutdown(self) {
        let (done, rx) = oneshot::channel();
        if self.cmd.send(Cmd::Shutdown { done }).is_ok() {
            let _ = rx.await;
        }
    }

    /// 节点元数据注册表快照（meta 组件）：所有连接过本节点的节点——地址历史 /
    /// first_seen / last_seen / 状态 / 分数 / 来源。按健康分降序（心跳一直正常
    /// 的靠前），Inactive 殿后。
    pub async fn node_meta(&self) -> Vec<crate::meta::NodeMetaEntry> {
        let (resp, rx) = oneshot::channel();
        if self.cmd.send(Cmd::NodeMeta { resp }).is_ok() {
            rx.await.unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    /// 手动触发元数据心跳（复活的路径之一）：Inactive → Active{score:30} 并
    /// **立即探测一次**返回结果（true = 探活成功）；Active 节点同样允许立即
    /// 探测；未知节点返回 false。另一条复活路径是他节点元数据交互报告其存活
    /// （见 meta 模块）。
    pub async fn meta_reactivate(&self, id: &NodeId) -> bool {
        let (resp, rx) = oneshot::channel();
        if self
            .cmd
            .send(Cmd::MetaReactivate {
                id: id.clone(),
                resp,
            })
            .is_ok()
        {
            rx.await.unwrap_or(false)
        } else {
            false
        }
    }

    /// 切换本节点网络出口声明（network-exit，2026-08-30）：digest 自广播
    /// `exit_offered` 位随下一轮 gossip（≤6 tick）广播出去。返回切换后的值
    /// （停机边缘返回 false 无意义，调用方可经 `node_meta` 侧观察确认）。
    pub async fn set_exit_offered(&self, offered: bool) -> bool {
        let (resp, rx) = oneshot::channel();
        if self.cmd.send(Cmd::SetExitOffered { offered, resp }).is_ok() {
            rx.await.unwrap_or(false)
        } else {
            false
        }
    }
}

// ============================================================================
// 节点入口
// ============================================================================

/// 节点装配入口（命名空间类型——无实例）。
pub struct P2pNode;

impl P2pNode {
    /// 启动节点。**必须在 tokio runtime 内调用**（内部 tokio::spawn / from_std）。
    ///
    /// 绑定 `config.listen` → 起 accept/cmd/maintenance/bootstrap 任务 → 返回 Handle。
    pub fn spawn(config: P2pConfig) -> std::io::Result<Handle> {
        Self::spawn_inner(config).map(|(_, handle)| handle)
    }

    /// [`P2pNode::spawn`] 的内部形态：额外返回引擎根 `Arc<Shared>`（crate 内
    /// 测试直连内部判定面——如 `connected_to_addr`——用；生产路径只用 Handle）。
    pub(crate) fn spawn_inner(config: P2pConfig) -> std::io::Result<(Arc<Shared>, Handle)> {
        // 同步绑定（socket2 带 SO_REUSEADDR → std listener → nonblocking → tokio
        // 注册），保持 spawn 非 async。REUSEADDR 是打洞映射复用的前提：出站
        // socket 之后可以绑定与监听相同的本地端口。
        let (std_listener, listen_addr) = bind_listener(config.listen)?;
        let listener = TcpListener::from_std(std_listener)?;
        let identity = config
            .identity
            .clone()
            .unwrap_or_else(NodeIdentity::generate);
        let self_id = identity.node_id();
        let advertise = config.advertise.or_else(|| {
            // public 回退通告守卫：监听 unspecified（`0.0.0.0:7070` 等）时该地址
            // 不可拨——通告出去只会污染对端路由表（曾出现 peers 里
            // underlay=0.0.0.0:7070 的垃圾条目）。NAT 后的服务节点必须用
            // `NEXOS_P2P_ADVERTISE` 显式通告真实可拨地址。
            config
                .public
                .then_some(listen_addr)
                .filter(|a| !a.ip().is_unspecified())
        });
        let (msg_tx, _) = broadcast::channel(MSG_CHANNEL_CAPACITY);
        let (shutdown_tx, _) = watch::channel(false);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        // 身份账本：装配方注入（os-api 共享实例）或本地自建（纯内存——CLI
        // 独立跑 / 测试默认路径；见 P2pConfig::identity_ledger 的权衡说明）。
        let identity_ledger = config
            .identity_ledger
            .clone()
            .unwrap_or_else(|| Arc::new(Mutex::new(os_identity::IdentityLedger::new(None))));

        let shared = Arc::new(Shared {
            identity,
            self_id: self_id.clone(),
            listen_addr,
            advertise,
            public: config.public,
            dial_from_listen_port: config.dial_from_listen_port,
            mdns_enabled: config.mdns_enabled,
            bootstrap_addrs: config.bootstrap.clone(),
            timing: config.timings.clone(),
            identity_ledger,
            msg_tx,
            shutdown_tx,
            tasks: Mutex::new(Vec::new()),
            me: Mutex::new(None),
            state: Mutex::new(State::new(
                self_id.clone(),
                config.timings.stale_after,
                config.meta_file.clone(),
                config.exit_offered,
            )),
        });
        // 自引用（Weak）：方法内部需要 spawn 持有 Shared 的任务（lookup 等）
        *shared.me.lock().expect("me poisoned") = Some(Arc::downgrade(&shared));

        spawn_tracked(&shared, accept_loop(shared.clone(), listener));
        spawn_tracked(&shared, cmd_loop(shared.clone(), cmd_rx));
        spawn_tracked(&shared, maintenance_loop(shared.clone()));
        spawn_tracked(&shared, crate::meta::meta_engine(shared.clone()));
        spawn_tracked(&shared, crate::bootstrap::bootstrap_task(shared.clone()));
        tracing::info!(
            self = %crate::short_hex(&self_id.to_hex()),
            %listen_addr,
            public = config.public,
            mdns = config.mdns_enabled,
            "os-p2p 节点启动（ECDH+AES-256-GCM 加密链路）"
        );
        Ok((
            shared.clone(),
            Handle {
                self_id,
                listen_addr,
                cmd: cmd_tx,
                msg_tx: shared.msg_tx.clone(),
            },
        ))
    }
}

/// 绑定监听（Unix 下带 SO_REUSEADDR + SO_REUSEPORT——打洞映射复用要求出站
/// socket 可绑定与监听**完全相同**的本地地址：REUSEADDR 只覆盖 TIME_WAIT/重叠
/// 场景，同端口精确绑定需要 REUSEPORT；入站连接仍由 listener 承接，已连接的
/// 复用 socket 不抢新连接）。
fn bind_listener(addr: SocketAddr) -> std::io::Result<(std::net::TcpListener, SocketAddr)> {
    let domain = if addr.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };
    let sock = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
    #[cfg(unix)]
    {
        sock.set_reuse_address(true)?;
        sock.set_reuse_port(true)?;
    }
    sock.bind(&addr.into())?;
    sock.listen(1024)?;
    let local = sock.local_addr()?.as_socket().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "non-inet socket")
    })?;
    sock.set_nonblocking(true)?;
    Ok((std::net::TcpListener::from(sock), local))
}

// ============================================================================
// 引擎内部
// ============================================================================

/// 已认证连接（握手后注册；读写任务各持一半 socket + 共享会话密钥）。
///
/// 注：对端**通告**的可拨地址（underlay）不挂在连接上——它属于节点档案
/// （buckets/NodeInfo），连接上只留 `observed`（本机 socket 的真实对端地址）。
/// 曾按对端通告 underlay 做地址级连接判定（connected_to_addr），NAT 对端
/// 通告 None 永远 miss 引发连接风暴（2026-08-24），已改按 observed 比对
/// 并移除本字段，杜绝维度混淆复发。
pub(crate) struct Conn {
    pub(crate) peer: NodeId,
    /// 公网服务节点（打洞中介排序：交换所角色优先）。
    pub(crate) public: bool,
    /// 会话密钥（握手 ECDH 派生——所有帧 AES-256-GCM）。
    cipher: SessionCipher,
    /// 对端观测地址（本机 socket 的 peer_addr——握手验证过的第一手信号，
    /// LAN/WAN 分类的最高优先地址依据；端点簿观测可能被 gossip 覆盖，此字段
    /// 不可能）。
    pub(crate) observed: SocketAddr,
    /// 本端 socket 地址（打洞出站映射复用的绑定来源）。
    pub(crate) local: Option<SocketAddr>,
    write_tx: mpsc::UnboundedSender<Frame>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Frame>>>,
    closed: AtomicBool,
    last_seen: Mutex<Instant>,
}

impl Conn {
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn mark_closed(&self) {
        self.closed.store(true, Ordering::Release);
    }

    fn touch(&self) {
        *self.last_seen.lock().expect("conn poisoned") = Instant::now();
    }

    fn idle_for(&self) -> Duration {
        self.last_seen.lock().expect("conn poisoned").elapsed()
    }

    /// 非阻塞发送（unbounded——拥塞交给 TCP 背压；连接已关 → false）。
    pub(crate) fn try_send(&self, frame: Frame) -> bool {
        !self.is_closed() && self.write_tx.send(frame).is_ok()
    }
}

/// 节点可变状态（std Mutex 短临界区——**持锁绝不 await**）。
pub(crate) struct State {
    pub(crate) conns: HashMap<NodeId, Arc<Conn>>,
    pub(crate) buckets: KBuckets,
    pub(crate) relay: RelayState,
    /// 观测端点地址簿（地址交换所——gossip 随 NODES 扩散）。
    pub(crate) endpoints: EndpointBook,
    /// SEND 待路由缓冲（无路径时暂存，lookup 完成/对端重连后冲刷）。
    /// 值 = (帧, 是否中继转发上下文——冲刷时保持 ttl/hops 语义)。
    pending_out: HashMap<NodeId, VecDeque<(Frame, bool)>>,
    /// 进行中的迭代查询（去重）。
    active_lookups: HashSet<OverlayAddr>,
    /// 最近一次 lookup 时刻（发送路径的触发节流）。
    last_lookup: HashMap<OverlayAddr, Instant>,
    /// 拨号去重（并发 lookup 不同时拨同一节点）。
    dialing: HashSet<NodeId>,
    /// 进行中的打洞目标（并发防抖：同目标不重复打）。
    pub(crate) punching: HashSet<NodeId>,
    /// 发起方等待 PUNCH2 的挂起会话 `{token → 端点回传通道}`。
    pub(crate) pending_punch: HashMap<String, oneshot::Sender<Vec<SocketAddr>>>,
    /// 连接阶梯统计（Handle::ladder_stats / CLI status）。
    pub(crate) ladder: LadderStats,
    /// 节点元数据注册表（meta 组件——本 crate 唯一的节点存活判定账本）。
    pub(crate) meta: crate::meta::NodeMetaStore,
    next_req_id: u64,
}

impl State {
    fn new(
        self_id: NodeId,
        stale_after: Duration,
        meta_file: Option<std::path::PathBuf>,
        exit_offered: bool,
    ) -> Self {
        Self {
            conns: HashMap::new(),
            buckets: KBuckets::new(self_id, stale_after),
            relay: RelayState::new(),
            endpoints: EndpointBook::new(),
            pending_out: HashMap::new(),
            active_lookups: HashSet::new(),
            last_lookup: HashMap::new(),
            dialing: HashSet::new(),
            punching: HashSet::new(),
            pending_punch: HashMap::new(),
            ladder: LadderStats::default(),
            meta: crate::meta::NodeMetaStore::with_exit_offer(meta_file, exit_offered),
            next_req_id: 1,
        }
    }

    fn req_id(&mut self) -> u64 {
        let id = self.next_req_id;
        self.next_req_id += 1;
        id
    }
}

/// 节点共享根（所有任务持有）。
pub(crate) struct Shared {
    identity: NodeIdentity,
    /// 自身 NodeID（bootstrap/punch 通告与去重用）。
    pub(crate) self_id: NodeId,
    /// 实际绑定监听地址（mDNS 通告 / 映射复用拨号源）。
    pub(crate) listen_addr: SocketAddr,
    /// 对外通告地址（元数据自广播首条携带——None 时自广播只带 id/alive；
    /// NAT 后显式通告 / public 节点通告监听地址，见 `P2pNode::spawn`）。
    pub(crate) advertise: Option<SocketAddr>,
    public: bool,
    dial_from_listen_port: bool,
    pub(crate) mdns_enabled: bool,
    pub(crate) bootstrap_addrs: Vec<SocketAddr>,
    pub(crate) timing: Timing,
    /// 身份账本（os-identity 组件——指纹证据登记 + 地址归属对比 + 冲突/失配
    /// 观测 + 持久化）。装配方注入共享实例或本地自建（见
    /// `P2pConfig::identity_ledger`）；std Mutex 短临界区、持锁不 await，
    /// **绝不与 state 锁嵌套**（两锁无顺序关系）。
    pub(crate) identity_ledger: os_identity::SharedLedger,
    msg_tx: broadcast::Sender<P2pMsg>,
    shutdown_tx: watch::Sender<bool>,
    tasks: Mutex<Vec<tokio::task::AbortHandle>>,
    me: Mutex<Option<Weak<Shared>>>,
    pub(crate) state: Mutex<State>,
}

/// 任务登记 spawn（shutdown 时统一 abort）。
pub(crate) fn spawn_tracked<F>(shared: &Shared, fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let handle = tokio::spawn(fut);
    shared
        .tasks
        .lock()
        .expect("tasks poisoned")
        .push(handle.abort_handle());
}

impl Shared {
    /// 自身 Overlay 地址（bootstrap walk target）。
    pub(crate) fn self_overlay(&self) -> OverlayAddr {
        self.self_id.overlay()
    }

    /// 停机观察句柄（bootstrap 保活循环等使用）。
    pub(crate) fn shutdown_watch(&self) -> watch::Receiver<bool> {
        self.shutdown_tx.subscribe()
    }

    /// 应用发送入口：本机回环直接交付；否则构造 send 帧走路由（源发：不改 ttl/hops）。
    fn route_app_send(&self, to: &NodeId, payload: serde_json::Value) {
        let frame = Frame::send(&self.self_id, to, payload);
        if to == &self.self_id {
            self.deliver_local(&frame);
        } else {
            self.route_frame(frame, false);
        }
    }

    /// SEND 帧路由（应用发送 + 中继转发的公共路径）。
    ///
    /// hops 语义 = **已穿越的中继跳数**：源节点首发不改 ttl/hops；中继转发
    /// （`is_forward`）与信箱入队（接收方必经本中继）才 `hopped()`——直连送达
    /// hops=0，经一个中继 hops=1（收方断言 ttl<16 / hops≥1 的依据）。
    fn route_frame(&self, frame: Frame, is_forward: bool) {
        let Some(dst) = frame.dst.clone() else {
            tracing::warn!("send 帧缺 dst，丢弃");
            return;
        };
        // 中继转发或信箱交付时施加 -1 ttl / +1 hops；源发直传保持原值
        let hop_frame = |f: &Frame, forward: bool| -> Option<Frame> {
            if forward {
                f.hopped()
            } else {
                Some(f.clone())
            }
        };
        enum Action {
            Wire(Arc<Conn>, Frame),
            Queue(Frame),
            Drop(&'static str),
            Defer(Frame),
        }
        let action = {
            let st = self.state.lock().expect("state poisoned");
            let conns: HashSet<NodeId> = st.conns.keys().cloned().collect();
            match relay::next_hop(
                &self.self_id,
                &dst,
                &conns,
                &st.relay.relayed_ids(),
                st.relay.routes_ref(),
            ) {
                relay::NextHop::Deliver => {
                    drop(st);
                    self.deliver_local(&frame);
                    return;
                }
                relay::NextHop::Direct => match st.conns.get(&dst) {
                    Some(conn) => match hop_frame(&frame, is_forward) {
                        Some(h) => Action::Wire(conn.clone(), h),
                        None => Action::Drop("ttl 尽"),
                    },
                    None => Action::Defer(frame.clone()),
                },
                relay::NextHop::RelayQueue => match frame.hopped() {
                    Some(h) => Action::Queue(h),
                    None => Action::Drop("ttl 尽"),
                },
                relay::NextHop::Forward(relay_id) => {
                    match (
                        st.conns.get(&relay_id).cloned(),
                        hop_frame(&frame, is_forward),
                    ) {
                        (Some(conn), Some(h)) => Action::Wire(conn, h),
                        _ => Action::Defer(frame.clone()),
                    }
                }
                relay::NextHop::Unknown => Action::Defer(frame.clone()),
            }
        };
        match action {
            Action::Wire(conn, h) => {
                if !conn.try_send(h) {
                    tracing::debug!(dst = %crate::short_hex(&dst.to_hex()), "连接已关，转入待路由");
                    self.defer_send(frame, &dst, is_forward);
                }
            }
            Action::Queue(h) => {
                let mut st = self.state.lock().expect("state poisoned");
                if !st.relay.enqueue_offline(&dst, h) {
                    tracing::warn!(dst = %crate::short_hex(&dst.to_hex()), "未注册中继却走到信箱路径，丢弃");
                }
            }
            Action::Drop(reason) => {
                tracing::warn!(dst = %crate::short_hex(&dst.to_hex()), reason, "帧丢弃");
            }
            Action::Defer(f) => self.defer_send(f, &dst, is_forward),
        }
    }

    /// 无路由暂存 + 按需触发 lookup（带节流；ttl 尽直接丢弃不缓冲）。
    fn defer_send(&self, frame: Frame, dst: &NodeId, is_forward: bool) {
        if frame.ttl == 0 {
            return;
        }
        let mut st = self.state.lock().expect("state poisoned");
        let q = st.pending_out.entry(dst.clone()).or_default();
        if q.len() >= PENDING_OUT_LIMIT {
            q.pop_front();
        }
        q.push_back((frame, is_forward));
        drop(st);
        self.maybe_spawn_lookup(&dst.overlay());
    }

    /// lookup 触发（active 去重 + 冷却窗口节流；经 Weak 自引用 spawn）。
    fn maybe_spawn_lookup(&self, target: &OverlayAddr) {
        {
            let mut st = self.state.lock().expect("state poisoned");
            if st.active_lookups.contains(target) {
                return;
            }
            let cooldown = self.timing.query_timeout * 2;
            if st
                .last_lookup
                .get(target)
                .is_some_and(|t| t.elapsed() < cooldown)
            {
                return;
            }
            st.active_lookups.insert(*target);
            st.last_lookup.insert(*target, Instant::now());
        }
        let arc = self
            .me
            .lock()
            .expect("me poisoned")
            .as_ref()
            .and_then(Weak::upgrade);
        match arc {
            Some(shared) => spawn_tracked(self, lookup(shared, *target)),
            None => {
                // 节点已在析构边缘——回滚标记
                self.state
                    .lock()
                    .expect("state poisoned")
                    .active_lookups
                    .remove(target);
            }
        }
    }

    /// 本地交付上层。
    fn deliver_local(&self, frame: &Frame) {
        let msg = P2pMsg {
            from: frame.src.clone(),
            payload: frame
                .app_payload()
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            ttl: frame.ttl,
            hops: frame.hops,
        };
        let _ = self.msg_tx.send(msg);
    }

    /// 帧分发（reader 任务调用；sync——持锁不await）。`self: &Arc<Self>`：
    /// punch 响应需要 spawn 持有 Shared 的任务。
    fn handle_frame(self: &Arc<Self>, conn: &Arc<Conn>, frame: Frame) {
        conn.touch();
        let now = Instant::now();
        // 锁内决策、锁外执行的动作（punch 转交 / 打洞响应 / 身份转述证据）
        let mut forward: Option<(Arc<Conn>, Frame)> = None;
        let mut respond_punch: Option<(Arc<Conn>, NodeId, String, Vec<SocketAddr>)> = None;
        let mut identity_gossip: Vec<crate::meta::MetaDigestEntry> = Vec::new();
        let reply: Option<Frame> = {
            let mut st = self.state.lock().expect("state poisoned");
            st.buckets.touch(&frame.src);
            st.relay.mark_alive(&frame.src, now);
            match frame.kind {
                FrameKind::Hello | FrameKind::AuthChallenge | FrameKind::AuthResponse => {
                    tracing::debug!(kind = ?frame.kind, "握手帧出现在数据阶段，忽略");
                    None
                }
                FrameKind::Ping => frame
                    .req_id()
                    .map(|rid| Frame::pong(&self.self_id, &frame.src, rid)),
                FrameKind::Pong => {
                    if let Some(rid) = frame.req_id() {
                        if let Some(tx) =
                            conn.pending.lock().expect("pending poisoned").remove(&rid)
                        {
                            let _ = tx.send(frame.clone());
                        }
                    }
                    None
                }
                FrameKind::FindNode => {
                    let target = frame
                        .field("target")
                        .and_then(|v| v.as_str())
                        .and_then(OverlayAddr::parse);
                    match (target, frame.req_id()) {
                        (Some(target), Some(rid)) => {
                            // 我知道的最近 ≤K 个（不含请求者与自身；被中继节点补 relay=我）
                            let nodes: Vec<NodeInfo> = st
                                .buckets
                                .closest(&target, K * 2)
                                .into_iter()
                                .filter(|i| i.id != frame.src && i.id != self.self_id)
                                .map(|mut i| {
                                    if st.relay.is_relayed(&i.id) {
                                        i.relay = Some(self.self_id.clone());
                                    }
                                    i
                                })
                                .take(K)
                                .collect();
                            // 地址交换所：观测端点随 NODES 一并八卦（上限 32 防膨胀；
                            // 排除自身——但保留请求者，它正要学自己的观测端点）
                            let endpoints = st
                                .endpoints
                                .gossip_sample(Some(&self.self_id), ENDPOINTS_GOSSIP_LIMIT);
                            Some(Frame::nodes(
                                &self.self_id,
                                &frame.src,
                                rid,
                                &nodes,
                                &endpoints,
                            ))
                        }
                        _ => {
                            tracing::warn!("findnode 缺 target/req_id，忽略");
                            None
                        }
                    }
                }
                FrameKind::Nodes => {
                    if let Some(rid) = frame.req_id() {
                        if let Some(tx) =
                            conn.pending.lock().expect("pending poisoned").remove(&rid)
                        {
                            let _ = tx.send(frame.clone());
                        }
                    }
                    // 学习应答节点（桶 + 可达性记录）——lookup 收敛的唯一知识入口
                    let list = frame
                        .field("nodes")
                        .cloned()
                        .and_then(|v| serde_json::from_value::<Vec<NodeInfo>>(v).ok());
                    if let Some(list) = list {
                        for info in list {
                            if info.id == self.self_id {
                                continue;
                            }
                            if let Some(relay_id) = info.relay.clone() {
                                st.relay.set_route(info.id.clone(), relay_id, &self.self_id);
                            }
                            st.buckets.upsert(info);
                        }
                    }
                    // 观测端点八卦学习（地址交换所回灌——含"我自己的观测端点"）
                    if let Some(eps) = frame
                        .field("endpoints")
                        .cloned()
                        .and_then(|v| serde_json::from_value::<Vec<EndpointGossip>>(v).ok())
                    {
                        st.endpoints.learn(&eps, now);
                    }
                    None
                }
                FrameKind::Send => match frame.dst.as_ref() {
                    Some(d) if d == &self.self_id => {
                        drop(st);
                        self.deliver_local(&frame);
                        None
                    }
                    Some(_) => {
                        drop(st);
                        self.route_frame(frame, true);
                        None
                    }
                    None => {
                        tracing::warn!("send 帧缺 dst，丢弃");
                        None
                    }
                },
                FrameKind::RelayAnnounce => {
                    if self.public {
                        st.relay.register_relayed(frame.src.clone(), now);
                        tracing::info!(
                            peer = %crate::short_hex(&frame.src.to_hex()),
                            "接受中继注册（store-and-forward 就绪）"
                        );
                    } else {
                        tracing::debug!("非公网节点拒收中继注册");
                    }
                    None
                }
                // 元数据交互：合并远端注册表摘要（学习新节点 / 新鲜度更新 /
                // 复活 Inactive——见 meta::NodeMetaStore::merge_digest）
                FrameKind::MetaGossip => {
                    if let Some(entries) = frame.meta_digest() {
                        // 复活新鲜度线 = 两个交互周期（2 × 6 tick）
                        let fresh = self.timing.meta_tick
                            * (crate::meta::META_GOSSIP_EVERY_TICKS as u32 * 2);
                        let revived =
                            st.meta
                                .merge_digest(&self.self_id, &entries, unix_now(), fresh);
                        if revived > 0 {
                            tracing::info!(
                                count = revived,
                                "元数据交互：远端报告复活 Inactive 节点（恢复心跳）"
                            );
                        }
                        // 摘要移出锁外记身份证据（identity 锁绝不与 state 锁嵌套）
                        identity_gossip = entries;
                    }
                    None
                }
                // 打洞控制帧：dst=最终对端，中介节点按直连转交
                FrameKind::Punch1 | FrameKind::Punch2 => match frame.dst.as_ref() {
                    Some(d) if d == &self.self_id => {
                        match frame.punch_payload() {
                            Some((token, eps)) if frame.kind == FrameKind::Punch1 => {
                                // 我是打洞目标：起响应方（回 PUNCH2 + 约定时刻同时打开）
                                respond_punch = Some((conn.clone(), frame.src.clone(), token, eps));
                            }
                            Some((token, eps)) => {
                                // PUNCH2 回到发起方：唤醒等待中的打洞任务
                                if let Some(tx) = st.pending_punch.remove(&token) {
                                    let _ = tx.send(eps);
                                }
                            }
                            None => {
                                tracing::debug!("punch 帧载荷非法，忽略");
                            }
                        }
                        None
                    }
                    Some(dst) => {
                        // 我是共同中介：向目标的直连转交（不知道目标即丢——
                        // 发起方会轮询其他中介）
                        match st.conns.get(dst).cloned() {
                            Some(next) => {
                                // 载荷端点对中介也是知识（转述学习）
                                if let Some((_, eps)) = frame.punch_payload() {
                                    let gossip: Vec<EndpointGossip> = eps
                                        .iter()
                                        .map(|&a| EndpointGossip::new(frame.src.clone(), a))
                                        .collect();
                                    st.endpoints.learn(&gossip, now);
                                }
                                match frame.hopped() {
                                    Some(h) => forward = Some((next, h)),
                                    None => tracing::warn!("punch 帧 ttl 尽，丢弃"),
                                }
                            }
                            None => {
                                tracing::debug!(
                                    dst = %crate::short_hex(&dst.to_hex()),
                                    "中介不知打洞目标，丢弃转交请求"
                                );
                            }
                        }
                        None
                    }
                    None => {
                        tracing::debug!("punch 帧缺 dst，丢弃");
                        None
                    }
                },
            }
        };
        if let Some(reply) = reply {
            conn.try_send(reply);
        }
        // 元数据交互的转述证据 → 身份账本（Gossip——报告方 verified 位透传，
        // 未验证地址只入 unverified 集；接收侧必须经本机指纹验证才能采信）。
        // 自身条目跳过（自广播回声）；回环/无地址报告由账本侧拒绝。
        if !identity_gossip.is_empty() {
            let now = unix_now();
            let mut ledger = self
                .identity_ledger
                .lock()
                .expect("identity ledger poisoned");
            for e in &identity_gossip {
                if e.id == self.self_id {
                    continue;
                }
                if let Some(addr) = e.addr {
                    ledger.record_evidence(
                        &e.id.to_hex(),
                        addr,
                        os_identity::EvidenceKind::Gossip {
                            verified: e.verified,
                        },
                        now,
                    );
                }
            }
        }
        if let Some((next, f)) = forward {
            tracing::debug!(
                dst = %crate::short_hex(&f.dst.as_ref().map(|d| d.to_hex()).unwrap_or_default()),
                "打洞控制帧转交"
            );
            if !next.try_send(f) {
                tracing::debug!("打洞转交目标连接已关，丢弃");
            }
        }
        if let Some((via, initiator, token, eps)) = respond_punch {
            crate::punch::spawn_punch_responder(self.clone(), via, initiator, token, eps);
        }
    }

    /// 连接断开（reader EOF/错误 或 writer 出错——读写两侧对称调用）。
    ///
    /// `Arc::ptr_eq` 保护：仅当表内该 NodeID 的当前连接**就是**本连接时才
    /// 移除——若已被后续注册顶替、或另一侧任务先行移除/引擎停机已清表，
    /// 不误删他人条目（重复调用静默，不重复记日志）。
    fn on_conn_closed(&self, conn: &Arc<Conn>) {
        conn.mark_closed();
        let removed = {
            let mut st = self.state.lock().expect("state poisoned");
            match st.conns.get(&conn.peer) {
                Some(current) if Arc::ptr_eq(current, conn) => {
                    st.conns.remove(&conn.peer);
                    true
                }
                _ => false,
            }
        };
        // 桶条目保留（Kademlia 容忍陈旧）：可拨节点交重拨探测裁决；
        // 被中继的 NAT 节点保留注册 + 信箱（store-and-forward 语义），relay TTL 清理。
        if removed {
            tracing::info!(
                peer = %crate::short_hex(&conn.peer.to_hex()),
                "连接断开（桶条目保留待探测 / 信箱保留待重连）"
            );
        }
    }

    /// 节点除名级联：桶移除 + 经其路由清理（ping 连续超时 / 重拨连续失败）。
    fn evict_node(&self, st: &mut State, id: &NodeId) {
        st.buckets.remove(id);
        st.relay.remove_routes_via(id);
        st.pending_out.remove(id);
        // 信箱与中继注册保留（TTL 兜底）——被中继节点可能只是暂时离线
    }

    /// peers 观察面快照。
    fn peers_snapshot(&self) -> Vec<PeerInfo> {
        let st = self.state.lock().expect("state poisoned");
        st.buckets
            .entries()
            .into_iter()
            .map(|info| PeerInfo {
                // 直连观测地址：活连接的 socket 对端（半关/待清理的连接不算——
                // is_closed 的连接随时会被除名，其地址不应再作为第一手信号）
                observed_addr: st
                    .conns
                    .get(&info.id)
                    .filter(|c| !c.is_closed())
                    .map(|c| c.observed),
                connected: st.conns.contains_key(&info.id),
                relayed_by_me: st.relay.is_relayed(&info.id),
                route_via: st.relay.route_for(&info.id),
                id: info.id,
                underlay: info.underlay,
                public: info.public,
                relay: info.relay,
            })
            .collect()
    }

    /// 是否与某地址存在已认证活跃连接（bootstrap 保活探测用）。
    ///
    /// 按 `observed`（本机 socket 的对端实际地址）比对，而非对端**通告**的
    /// `underlay`——NAT 节点（public=false 且未配置 advertise）握手不通告
    /// underlay（None），按 underlay 比对永远 miss，即使连接活跃也返回
    /// false → bootstrap 保活每秒重拨、对端 register_conn 按 NodeID 去重
    /// 全拒，形成「一边疯狂拨、一边疯狂丢」的连接风暴（2026-08-24，
    /// nexos-test BUG-p2p-connected_to_addr-underlay-storm）。
    /// 调用方（bootstrap 保活 / mDNS 收割，bootstrap.rs）均为出站拨号场景：
    /// observed 即拨号目标地址，比对语义等价且更准。
    pub(crate) fn connected_to_addr(&self, addr: &SocketAddr) -> bool {
        let st = self.state.lock().expect("state poisoned");
        st.conns
            .values()
            .any(|c| !c.is_closed() && c.observed == *addr)
    }

    /// 冲刷某目标的待路由缓冲（lookup 完成 / 对端重连）。
    fn flush_pending(&self, dst: &NodeId) {
        let frames: Vec<(Frame, bool)> = {
            let mut st = self.state.lock().expect("state poisoned");
            st.pending_out
                .remove(dst)
                .map(|q| q.into_iter().collect())
                .unwrap_or_default()
        };
        for (f, fwd) in frames {
            self.route_frame(f, fwd);
        }
    }

    /// lookup 完成回调：冲刷 overlay 命中的全部待路由目标。
    fn flush_pending_for_target(&self, target: &OverlayAddr) {
        let hits: Vec<NodeId> = {
            let st = self.state.lock().expect("state poisoned");
            st.pending_out
                .keys()
                .filter(|dst| dst.overlay() == *target)
                .cloned()
                .collect()
        };
        for dst in hits {
            self.flush_pending(&dst);
        }
    }

    /// 优雅停机（先应答再自停——见 cmd_loop 的 Cmd::Shutdown 分支）。
    async fn do_shutdown(&self) {
        // 元数据注册表同步刷盘一次（脏标记防抖的兜底——重启不丢）
        crate::meta::flush_now(self);
        let _ = self.shutdown_tx.send(true);
        let aborts: Vec<tokio::task::AbortHandle> = self
            .tasks
            .lock()
            .expect("tasks poisoned")
            .drain(..)
            .collect();
        self.state.lock().expect("state poisoned").conns.clear();
        for h in aborts {
            h.abort();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ============================================================================
// 任务体
// ============================================================================

/// 控制面命令循环。
async fn cmd_loop(shared: Arc<Shared>, mut rx: mpsc::UnboundedReceiver<Cmd>) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Cmd::Send { to, payload } => shared.route_app_send(&to, payload),
            Cmd::Peers { resp } => {
                let _ = resp.send(shared.peers_snapshot());
            }
            Cmd::Buckets { resp } => {
                let summary = shared
                    .state
                    .lock()
                    .expect("state poisoned")
                    .buckets
                    .summary();
                let _ = resp.send(summary);
            }
            Cmd::Route { id, resp } => {
                let route = shared
                    .state
                    .lock()
                    .expect("state poisoned")
                    .relay
                    .route_for(&id);
                let _ = resp.send(route);
            }
            Cmd::Connect { target, resp } => {
                // 打洞可耗时数秒——专用任务执行，命令循环即刻返回收下一条
                let worker = shared.clone();
                spawn_tracked(&shared, async move {
                    let result = crate::punch::connect_ladder(&worker, &target).await;
                    let _ = resp.send(result);
                });
            }
            Cmd::Dial { addr, resp } => {
                // 拨号+握手可耗时数秒（connect 超时 + 握手上限 5s）——专用任务
                // 执行，命令循环即刻返回收下一条
                let worker = shared.clone();
                spawn_tracked(&shared, async move {
                    let result = match dial_addr(&worker, addr).await {
                        Some(conn) => Ok(conn.peer.clone()),
                        None => Err(format!(
                            "拨号 {addr} 失败（不可达 / 握手失败 / 超时 / 已有同节点连接）"
                        )),
                    };
                    let _ = resp.send(result);
                });
            }
            Cmd::KnownEndpoints { resp } => {
                let entries = shared
                    .state
                    .lock()
                    .expect("state poisoned")
                    .endpoints
                    .entries();
                let _ = resp.send(entries);
            }
            Cmd::LookupEndpoint { id, resp } => {
                let addr = shared
                    .state
                    .lock()
                    .expect("state poisoned")
                    .endpoints
                    .lookup(&id);
                let _ = resp.send(addr);
            }
            Cmd::Ladder { resp } => {
                let stats = shared.state.lock().expect("state poisoned").ladder.clone();
                let _ = resp.send(stats);
            }
            Cmd::IdentityConflicts { resp } => {
                // 冲突记账已迁至 os-identity 账本（register_conn 发事实事件）；
                // 账本侧按最近发现降序（前端警告条首条 = 最活跃冲突源）
                let list = shared
                    .identity_ledger
                    .lock()
                    .expect("identity ledger poisoned")
                    .conflicts();
                let _ = resp.send(list);
            }
            Cmd::NodeMeta { resp } => {
                // 健康排名：Active 分数降序在前，Inactive 殿后（见 meta::snapshot）
                let entries = shared.state.lock().expect("state poisoned").meta.snapshot();
                let _ = resp.send(entries);
            }
            Cmd::MetaReactivate { id, resp } => {
                // 探测可耗满 TCP 超时——专用任务执行，命令循环即刻返回收下一条
                let worker = shared.clone();
                spawn_tracked(&shared, async move {
                    let ok = crate::meta::reactivate_probe(&worker, &id).await;
                    let _ = resp.send(ok);
                });
            }
            Cmd::SetExitOffered { offered, resp } => {
                let mut st = shared.state.lock().expect("state poisoned");
                st.meta.set_self_exit(offered);
                let now = st.meta.self_exit();
                drop(st);
                tracing::info!(
                    offered = now,
                    "切换网络出口声明（下一轮元数据交互自广播生效）"
                );
                let _ = resp.send(now);
            }
            Cmd::Shutdown { done } => {
                // 先应答（Handle::shutdown 返回），再统一 abort（含本任务自身）
                let _ = done.send(());
                shared.do_shutdown().await;
                break;
            }
        }
    }
}

/// 入站接受循环（每个新连接独立握手任务）。
async fn accept_loop(shared: Arc<Shared>, listener: TcpListener) {
    let mut shutdown_rx = shared.shutdown_tx.subscribe();
    loop {
        let accepted = tokio::select! {
            _ = shutdown_rx.changed() => break,
            res = listener.accept() => res,
        };
        match accepted {
            Ok((stream, addr)) => {
                let worker = shared.clone();
                spawn_tracked(&shared, async move {
                    match handshake_stream(&worker, stream).await {
                        Some(accepted) => {
                            // 入站审计：谁（源地址 addr）拨入、握手后是哪个 NodeID、
                            // 观测地址（NAT 映射口）——定位「谁在拨 7070」类问题。
                            // eprintln 而非 tracing：os-api 不装 subscriber，
                            // tracing 在网关进程里无声（journald 看不到）。
                            eprintln!(
                                "[os-p2p] 入站连接握手成功 remote={} node={} observed={}",
                                addr,
                                crate::short_hex(&accepted.hello.node_id.to_hex()),
                                accepted.observed
                            );
                            let _ = register_conn(&worker, accepted).await;
                        }
                        None => {
                            // 握手失败/超时也记源地址（测试进程拨一下就断即此类）。
                            // eprintln 而非 tracing：os-api 不装 subscriber，
                            // tracing 在网关进程里无声（journald 看不到）。
                            eprintln!("[os-p2p] 入站连接握手失败/超时 remote={addr}");
                        }
                    }
                });
            }
            Err(e) => {
                tracing::warn!("accept 失败: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// 握手产物（连接尚未注册；register_conn 消费）。
pub(crate) struct AcceptedConn {
    pub read: OwnedReadHalf,
    pub write: OwnedWriteHalf,
    pub hello: PeerHello,
    /// 会话密钥（ECDH 派生——读写任务共享）。
    pub cipher: SessionCipher,
    /// 对端观测地址（socket 对端 ip:port——NAT 映射口，入地址簿）。
    pub observed: SocketAddr,
    /// 本端 socket 地址（打洞映射复用来源）。
    pub local: Option<SocketAddr>,
}

/// 对 TCP 流完成双向挑战-签名 + ECDH 握手（失败/超时 → None，流随作用域关闭）。
pub(crate) async fn handshake_stream(shared: &Shared, stream: TcpStream) -> Option<AcceptedConn> {
    let _ = stream.set_nodelay(true);
    let observed = stream.peer_addr().ok()?;
    let local = stream.local_addr().ok();
    let (mut read, mut write) = stream.into_split();
    let ctx = HelloCtx {
        identity: &shared.identity,
        advertise: shared.advertise.map(|a| a.to_string()),
        public: shared.public,
    };
    match tokio::time::timeout(
        HANDSHAKE_CEILING,
        crate::transport::handshake(&mut read, &mut write, &ctx),
    )
    .await
    {
        Ok(Ok((hello, cipher))) => Some(AcceptedConn {
            read,
            write,
            hello,
            cipher,
            observed,
            local,
        }),
        Ok(Err(e)) => {
            tracing::debug!("握手失败: {e}");
            None
        }
        Err(_) => {
            tracing::debug!("握手超时");
            None
        }
    }
}

/// 连接注册：去重 → 入表 → 观测端点入簿 → 中继注册 → 冲信箱/待路由 →
/// 起读写任务（加密帧收发）。
pub(crate) async fn register_conn(
    shared: &Arc<Shared>,
    accepted: AcceptedConn,
) -> Result<Arc<Conn>, String> {
    // 身份事实事件 → os-identity 账本（2026-08-25 组件抽离：传输层只发事实，
    // 记谁/信谁/地址属于谁由账本判定）。两件事：
    //
    // ① 同 NodeID 冲突观测（仅提示不阻断，2026-08-23 语义迁入账本）：对端
    //   握手自报公钥 == 本机 NodeID——同一私钥被多个 OS 同时使用（身份=密钥，
    //   权限共享是设计特性，不是攻击），账本记一条警告观测让用户知情，连接
    //   照常建立；
    // ② 握手证据（非自身、非回环）：observed 地址在该 NodeID 名下升 verified
    //   ——注册前先查一次地址归属（owns_addr），地址已实证属于**其他**身份时
    //   记 warn（地址换人/复用的观测事实，同样不阻断）。
    let peer_hex = accepted.hello.node_id.to_hex();
    {
        let mut ledger = shared
            .identity_ledger
            .lock()
            .expect("identity ledger poisoned");
        if accepted.hello.node_id == shared.self_id {
            let count =
                ledger.record_conflict(&shared.self_id.to_hex(), accepted.observed, unix_now());
            eprintln!(
                "[p2p][WARN] 检测到相同公钥从 {} 连接（本机 NodeID 冲突，第 {} 次；仅提示不阻断）",
                accepted.observed, count
            );
            tracing::warn!(
                peer = %crate::short_hex(&peer_hex),
                observed = %accepted.observed,
                count,
                "NodeID 冲突：对端自报公钥与本机相同（身份=密钥，权限共享是设计特性——仅本地警告，不阻断连接；记账在 os-identity 账本）"
            );
        } else if let os_identity::AddrOwnership::Foreign { owner } =
            ledger.owns_addr(accepted.observed, &peer_hex)
        {
            tracing::warn!(
                peer = %crate::short_hex(&peer_hex),
                observed = %accepted.observed,
                owner = %crate::short_hex(&owner),
                "地址归属冲突：观测地址已实证属于其他身份（IP 换人/复用——观测事实，不阻断；账本将按最新握手证据改判归属）"
            );
        }
    }
    let underlay = accepted
        .hello
        .underlay
        .as_deref()
        .and_then(|s| s.parse().ok());
    let (write_tx, write_rx) = mpsc::unbounded_channel();
    let cipher = accepted.cipher.clone();
    let conn = Arc::new(Conn {
        peer: accepted.hello.node_id.clone(),
        public: accepted.hello.public,
        cipher: cipher.clone(),
        observed: accepted.observed,
        local: accepted.local,
        write_tx,
        pending: Mutex::new(HashMap::new()),
        closed: AtomicBool::new(false),
        last_seen: Mutex::new(Instant::now()),
    });
    {
        let mut st = shared.state.lock().expect("state poisoned");
        // 同 NodeID 去重「弃新保旧」——曾评估改「弃旧保新」（新连接顶掉旧
        // 连接，旧连接优雅关闭），经评估保留现状，理由：
        // ① punch 同时打开（punch.rs）双方同刻互拨，「先完成握手注册者胜」
        //   的竞速语义**依赖**此去重拒绝——改顶替后双方各自保留「对方刚顶掉
        //   的那条」，被顶连接的 write shutdown 产生 FIN 互传，两条 socket
        //   双双阵亡（打洞报成功即断，比拒绝更糟）；
        // ② 任意双侧同时互拨窗口（bootstrap 种子对称配置）同理抖动。
        // 「有活连接仍拨」的入口已在拨号侧封死：bootstrap 保活按 observed
        // 地址判连（connected_to_addr——2026-08-24 连接风暴根因修复），
        // redial_sweep / ensure_conn 按 NodeID+is_closed 判连；且 dial_addr
        // 对去重拒绝复用既有连接返回 Ok，调用方语义完好。
        if let Some(old) = st.conns.get(&accepted.hello.node_id) {
            if !old.is_closed() {
                tracing::debug!(
                    peer = %crate::short_hex(&accepted.hello.node_id.to_hex()),
                    "重复连接（旧连接仍在），放弃新连接"
                );
                return Err("duplicate connection".into());
            }
        }
        st.conns
            .insert(accepted.hello.node_id.clone(), conn.clone());
        st.buckets.upsert(NodeInfo {
            id: accepted.hello.node_id.clone(),
            underlay,
            public: accepted.hello.public,
            relay: None,
        });
        // 地址交换所记账：对端经此连接被我观测为 observed（NAT 后节点的公网映射）
        st.endpoints.observe(
            accepted.hello.node_id.clone(),
            accepted.observed,
            Instant::now(),
        );
        // 元数据记账（meta 组件注册表的 Direct 入口）：所有连接过本节点的节点
        // 留档——first_seen 建档 / last_seen+addrs 更新 / Inactive 复活。
        // 同私钥多 OS 的"自己连自己"除外（注册表不收录本机）。
        if accepted.hello.node_id != shared.self_id {
            st.meta
                .record_conn(&accepted.hello.node_id, accepted.observed, unix_now());
        }
    }
    // 握手证据 → 身份账本（非自身；回环由账本侧拒绝——同机多实例的观测归
    // record_conflict 冲突面，地址集不收回环）。锁外写入：identity 锁绝不与
    // state 锁嵌套（两锁无顺序关系）。
    if accepted.hello.node_id != shared.self_id {
        shared
            .identity_ledger
            .lock()
            .expect("identity ledger poisoned")
            .record_evidence(
                &peer_hex,
                accepted.observed,
                os_identity::EvidenceKind::Handshake,
                unix_now(),
            );
    }
    tracing::info!(
        peer = %crate::short_hex(&accepted.hello.node_id.to_hex()),
        underlay = ?underlay,
        observed = %accepted.observed,
        public = accepted.hello.public,
        session = ?conn.cipher,
        "连接建立（ECDH+挑战-签名双向验证，链路已加密）"
    );
    // NAT 节点 → 公网对端：注册中继（"我的可达性经你"）
    if !shared.public && accepted.hello.public {
        conn.try_send(Frame::relay_announce(
            &shared.self_id,
            &accepted.hello.node_id,
        ));
    }
    // 信箱冲刷（被中继节点重连——离线消息送达时刻）
    {
        let mut st = shared.state.lock().expect("state poisoned");
        for f in st.relay.take_mailbox(&accepted.hello.node_id) {
            conn.try_send(f);
        }
    }
    // 待路由冲刷（该对端重新可达）
    shared.flush_pending(&accepted.hello.node_id);
    // 读写任务（会话密钥上的加密帧收发）
    spawn_tracked(
        shared,
        reader_task(shared.clone(), conn.clone(), accepted.read, cipher.clone()),
    );
    spawn_tracked(
        shared,
        writer_task(
            shared.clone(),
            conn.clone(),
            write_rx,
            accepted.write,
            cipher,
        ),
    );
    Ok(conn)
}

/// 连接读循环：解密收帧并分发；EOF/错误/验签失败 → on_conn_closed。
async fn reader_task(
    shared: Arc<Shared>,
    conn: Arc<Conn>,
    mut read: OwnedReadHalf,
    cipher: SessionCipher,
) {
    loop {
        match read_frame_enc(&mut read, &cipher).await {
            Ok(frame) => shared.handle_frame(&conn, frame),
            Err(e) => {
                tracing::debug!(peer = %crate::short_hex(&conn.peer.to_hex()), "读结束: {e}");
                break;
            }
        }
    }
    shared.on_conn_closed(&conn);
}

/// 连接写循环（加密发帧）。退出与 reader_task 对称走 `on_conn_closed`
/// （mark_closed + 按身份移除）——此前只 mark_closed 不 remove，写侧先退
/// （对端 RST / 本端写错误）时连接滞留 conns 表成僵尸（is_closed=true 但
/// 仍占 NodeID 槽，阻塞重拨窗口）。`Arc::ptr_eq` 保护见 on_conn_closed。
async fn writer_task(
    shared: Arc<Shared>,
    conn: Arc<Conn>,
    mut rx: mpsc::UnboundedReceiver<Frame>,
    mut write: OwnedWriteHalf,
    cipher: SessionCipher,
) {
    let mut shutdown_rx = shared.shutdown_tx.subscribe();
    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => break,
            item = rx.recv() => match item {
                Some(frame) => {
                    if write_frame_enc(&mut write, &cipher, &frame).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
        }
    }
    shared.on_conn_closed(&conn);
    let _ = write.shutdown().await;
}

/// 维护循环：ping 剔除 / 桶刷新 walk / 重拨探测 / 中继与端点 TTL 清理。
async fn maintenance_loop(shared: Arc<Shared>) {
    let mut ping_tick = tokio::time::interval(shared.timing.ping_interval);
    let mut refresh_tick = tokio::time::interval(shared.timing.refresh_interval);
    let mut refresh_self = false;
    ping_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    refresh_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut shutdown_rx = shared.shutdown_tx.subscribe();
    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => break,
            _ = ping_tick.tick() => {
                ping_sweep(&shared);
                redial_sweep(&shared);
                relay_ttl_sweep(&shared);
                endpoints_ttl_sweep(&shared);
            }
            _ = refresh_tick.tick() => {
                // 交替 walk：自身邻域 / 随机 target（Swarm 式桶刷新）
                let target = if refresh_self {
                    refresh_self = false;
                    shared.self_id.overlay()
                } else {
                    refresh_self = true;
                    OverlayAddr::random()
                };
                shared.maybe_spawn_lookup(&target);
            }
        }
    }
}

/// PING 扫描：空闲连接探测；连续失败 → 除名 + 级联清理。
fn ping_sweep(shared: &Arc<Shared>) {
    let targets: Vec<Arc<Conn>> = {
        let st = shared.state.lock().expect("state poisoned");
        st.conns
            .values()
            .filter(|c| c.idle_for() > shared.timing.ping_interval)
            .cloned()
            .collect()
    };
    for conn in targets {
        let worker = shared.clone();
        spawn_tracked(shared, async move {
            let shared = worker;
            let (tx, rx) = oneshot::channel();
            let rid = {
                let mut st = shared.state.lock().expect("state poisoned");
                let rid = st.req_id();
                conn.pending
                    .lock()
                    .expect("pending poisoned")
                    .insert(rid, tx);
                rid
            };
            if !conn.try_send(Frame::ping(&shared.self_id, &conn.peer, rid)) {
                conn.pending.lock().expect("pending poisoned").remove(&rid);
                return;
            }
            match tokio::time::timeout(shared.timing.ping_timeout, rx).await {
                Ok(Ok(_)) => {
                    conn.touch();
                    shared
                        .state
                        .lock()
                        .expect("state poisoned")
                        .buckets
                        .touch(&conn.peer);
                }
                _ => {
                    tracing::warn!(
                        peer = %crate::short_hex(&conn.peer.to_hex()),
                        "ping 超时（记一次失败）"
                    );
                    let mut st = shared.state.lock().expect("state poisoned");
                    if st
                        .buckets
                        .record_failure(&conn.peer, shared.timing.max_failures)
                        .is_some()
                    {
                        st.conns.remove(&conn.peer);
                        st.relay.remove_routes_via(&conn.peer);
                        drop(st);
                        conn.mark_closed();
                        tracing::info!(
                            peer = %crate::short_hex(&conn.peer.to_hex()),
                            "连续 ping 失败，节点除名"
                        );
                    }
                }
            }
        });
    }
}

/// 重拨扫描：桶内可拨但无连接的条目——探测拨号，失败记 failure。
fn redial_sweep(shared: &Arc<Shared>) {
    let candidates: Vec<NodeInfo> = {
        let st = shared.state.lock().expect("state poisoned");
        st.buckets
            .entries()
            .into_iter()
            .filter(|i| i.dialable() && !st.conns.contains_key(&i.id))
            .collect()
    };
    for info in candidates {
        let worker = shared.clone();
        spawn_tracked(shared, async move {
            if ensure_conn(&worker, &info).await.is_none() {
                let mut st = worker.state.lock().expect("state poisoned");
                if st
                    .buckets
                    .record_failure(&info.id, worker.timing.max_failures)
                    .is_some()
                {
                    worker.evict_node(&mut st, &info.id);
                    tracing::info!(
                        peer = %crate::short_hex(&info.id.to_hex()),
                        "重拨连续失败，节点除名（桶剔除）"
                    );
                }
            }
        });
    }
}

/// 中继注册 TTL 清理（幽灵路由）。
fn relay_ttl_sweep(shared: &Arc<Shared>) {
    let expired = {
        let mut st = shared.state.lock().expect("state poisoned");
        st.relay
            .evict_expired(Instant::now(), shared.timing.relay_ttl)
    };
    if !expired.is_empty() {
        let mut st = shared.state.lock().expect("state poisoned");
        for id in expired {
            st.buckets.remove(&id);
        }
    }
}

/// 观测端点 TTL 清理（死 NAT 映射不滞留地址簿）。
fn endpoints_ttl_sweep(shared: &Arc<Shared>) {
    let expired = {
        let mut st = shared.state.lock().expect("state poisoned");
        st.endpoints
            .evict_expired(Instant::now(), shared.timing.endpoint_ttl)
    };
    if !expired.is_empty() {
        tracing::debug!(count = expired.len(), "观测端点 TTL 清理");
    }
}

/// 是否与目标存在已认证活跃连接（打洞竞速判定 / 阶梯短路）。
pub(crate) fn is_connected(shared: &Shared, id: &NodeId) -> bool {
    shared
        .state
        .lock()
        .expect("state poisoned")
        .conns
        .get(id)
        .is_some_and(|c| !c.is_closed())
}

/// 拨号（可选本地端口绑定——打洞映射复用）：TCP connect + 双向挑战-签名 +
/// ECDH 握手 + 注册（bootstrap / 重拨 / 打洞共用）。
pub(crate) async fn dial_addr(shared: &Arc<Shared>, addr: SocketAddr) -> Option<Arc<Conn>> {
    let stream = dial_socket(shared, addr, None).await?;
    let accepted = handshake_stream(shared, stream).await?;
    let target = accepted.hello.node_id.clone();
    match register_conn(shared, accepted).await {
        Ok(conn) => Some(conn),
        // 同节点已有活连接（register_conn 去重拒绝）：握手已验明对端身份——
        // 复用既有连接，语义为"已连接"而非"拨号失败"（手动 add-peer 对已连
        // 节点重复添加的主路径，曾误报"不可达"）。
        Err(_) => {
            let st = shared.state.lock().expect("state poisoned");
            st.conns.get(&target).filter(|c| !c.is_closed()).cloned()
        }
    }
}

/// 底层 socket 拨号：`local_bind` = Some 时绑定该本地地址再 connect
/// （SO_REUSEADDR——与监听/中介连接复用同一端口，打洞"稳定映射"语义）；
/// `dial_from_listen_port` 配置时默认绑定监听口。整体受 query_timeout×2 约束。
pub(crate) async fn dial_socket(
    shared: &Shared,
    remote: SocketAddr,
    local_bind: Option<SocketAddr>,
) -> Option<TcpStream> {
    let local = local_bind.or_else(|| {
        shared.dial_from_listen_port.then(|| {
            let ip = if shared.listen_addr.ip().is_unspecified() {
                std::net::IpAddr::from([127, 0, 0, 1])
            } else {
                shared.listen_addr.ip()
            };
            SocketAddr::new(ip, shared.listen_addr.port())
        })
    });
    let connect = async {
        match local {
            None => TcpStream::connect(remote).await,
            Some(local) => {
                let sock = if remote.is_ipv4() {
                    TcpSocket::new_v4()
                } else {
                    TcpSocket::new_v6()
                }?;
                sock.set_reuseaddr(true)?;
                #[cfg(unix)]
                sock.set_reuseport(true)?;
                sock.bind(local)?;
                sock.connect(remote).await
            }
        }
    };
    tokio::time::timeout(shared.timing.query_timeout * 2, connect)
        .await
        .ok()?
        .ok()
}

/// 确保与目标的连接：有则复用；无且可拨则拨号 + 握手 + 注册（NAT 不可拨 → None）。
pub(crate) async fn ensure_conn(shared: &Arc<Shared>, info: &NodeInfo) -> Option<Arc<Conn>> {
    if let Some(c) = shared
        .state
        .lock()
        .expect("state poisoned")
        .conns
        .get(&info.id)
    {
        if !c.is_closed() {
            return Some(c.clone());
        }
    }
    let addr = info.underlay?;
    {
        let mut st = shared.state.lock().expect("state poisoned");
        if st.dialing.contains(&info.id) {
            return None;
        }
        st.dialing.insert(info.id.clone());
    }
    let conn = dial_addr(shared, addr).await;
    shared
        .state
        .lock()
        .expect("state poisoned")
        .dialing
        .remove(&info.id);
    conn
}

/// Kademlia 迭代查询（FINDNODE 收敛）：每轮向 α 个更近的可拨节点并行查询，
/// 一轮既无更近节点也无新知识即收敛；完成后冲刷 pending_out。
pub(crate) async fn lookup(shared: Arc<Shared>, target: OverlayAddr) {
    tracing::debug!(target = %target, "lookup 开始");
    let self_id = shared.self_id.clone();
    let mut queried: HashSet<NodeId> = HashSet::new();
    for _round in 0..crate::kad::MAX_LOOKUP_ROUNDS {
        let candidates: Vec<NodeInfo> = {
            let st = shared.state.lock().expect("state poisoned");
            st.buckets
                .closest_dialable(&target, crate::kad::K)
                .into_iter()
                .filter(|i| i.id != self_id && !queried.contains(&i.id))
                .take(crate::kad::ALPHA)
                .collect()
        };
        if candidates.is_empty() {
            break;
        }
        let (best_before, known_before) = {
            let st = shared.state.lock().expect("state poisoned");
            (
                st.buckets
                    .closest(&target, 1)
                    .first()
                    .map(|i| i.overlay().xor(&target)),
                st.buckets.len(),
            )
        };
        let mut set = tokio::task::JoinSet::new();
        for info in candidates {
            queried.insert(info.id.clone());
            let shared = shared.clone();
            set.spawn(async move {
                let _ = query_node(&shared, &info, target).await;
            });
        }
        while set.join_next().await.is_some() {}
        // 收敛判定：既无更近节点也无新条目 → 无更近即收敛
        let (best_after, known_after) = {
            let st = shared.state.lock().expect("state poisoned");
            (
                st.buckets
                    .closest(&target, 1)
                    .first()
                    .map(|i| i.overlay().xor(&target)),
                st.buckets.len(),
            )
        };
        let closer = match (best_after, best_before) {
            (Some(a), Some(b)) => a < b,
            (Some(_), None) => true,
            _ => false,
        };
        if !closer && known_after == known_before {
            tracing::debug!(target = %target, "lookup 收敛（无更近且无新节点）");
            break;
        }
    }
    shared
        .state
        .lock()
        .expect("state poisoned")
        .active_lookups
        .remove(&target);
    shared.flush_pending_for_target(&target);
}

/// 单点 FINDNODE（req_id 关联 + 超时；知识学习在 handle_frame 的 Nodes 分支集中完成）。
async fn query_node(shared: &Arc<Shared>, info: &NodeInfo, target: OverlayAddr) -> Option<()> {
    let conn = ensure_conn(shared, info).await?;
    let (tx, rx) = oneshot::channel();
    let rid = {
        let mut st = shared.state.lock().expect("state poisoned");
        let rid = st.req_id();
        conn.pending
            .lock()
            .expect("pending poisoned")
            .insert(rid, tx);
        rid
    };
    if !conn.try_send(Frame::find_node(&shared.self_id, &info.id, rid, &target)) {
        conn.pending.lock().expect("pending poisoned").remove(&rid);
        return None;
    }
    match tokio::time::timeout(shared.timing.query_timeout, rx).await {
        Ok(Ok(frame)) if frame.kind == FrameKind::Nodes => Some(()),
        _ => None,
    }
}

// ============================================================================
// 单元测——配置默认值 / spawn 自环
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::{MetaSource, MetaState};

    /// 本机非回环 IPv4（元数据 gossip 出入口过滤回环地址——127.0.0.1 只在
    /// 本机可拨，转述给其他节点纯噪声；跨节点学址的集成测试需经非回环本机
    /// 地址互拨，即生产路径）。UDP connect 选路技巧：不真正发包，只让内核
    /// 按默认路由挑出口接口并返回其地址（CI/开发机必有非回环 IPv4）。
    fn non_loopback_local_ipv4() -> std::net::Ipv4Addr {
        let s = std::net::UdpSocket::bind("0.0.0.0:0").expect("UDP bind（选路用）");
        s.connect("8.8.8.8:80").expect("connect（仅选路，不发包）");
        match s.local_addr().expect("local_addr").ip() {
            std::net::IpAddr::V4(v4) if !v4.is_loopback() => v4,
            other => panic!("无非回环本机 IPv4 可用（gossip 学址测试无从进行）: {other}"),
        }
    }

    // 1. 配置默认值：监听 :7070、非公网、无引导、生产节奏；P2a 开关默认关
    #[test]
    fn config_defaults() {
        let c = P2pConfig::default();
        assert_eq!(c.listen.port(), crate::bootstrap::P2P_PORT_DEFAULT);
        assert_eq!(c.listen.ip(), std::net::IpAddr::from([0, 0, 0, 0]));
        assert!(c.bootstrap.is_empty() && !c.public && c.identity.is_none());
        assert!(!c.dial_from_listen_port && !c.mdns_enabled);
        assert!(
            c.meta_file.is_none(),
            "元数据注册表默认纯内存（显式配置才持久化）"
        );
        assert!(
            c.identity_ledger.is_none(),
            "身份账本默认不自建持久化实例（os-api 注入或本地内存——见 P2pConfig 字段说明）"
        );
        let t = Timing::default();
        assert_eq!(t.max_failures, 2);
        assert_eq!(t.ping_timeout, Duration::from_secs(3));
        assert_eq!(t.punch_retry_interval, Duration::from_millis(800));
        assert_eq!(t.endpoint_ttl, Duration::from_secs(600));
        assert_eq!(t.meta_tick, Duration::from_secs(5), "元数据 tick 默认 5s");
        // 测试节奏压缩到亚秒
        let tt = Timing::testing();
        assert!(tt.ping_interval < Duration::from_secs(1));
        assert!(tt.relay_ttl < Timing::default().relay_ttl);
        assert!(tt.punch_retry_interval < t.punch_retry_interval);
        assert!(tt.mdns_first_pass < Duration::from_secs(1));
        assert!(
            tt.meta_tick < Duration::from_secs(1),
            "测试元数据 tick 亚秒级"
        );
    }

    // 2. spawn + 自环发送 + 观察面 + 优雅停机（单节点冒烟）
    #[tokio::test]
    async fn spawn_self_send_and_shutdown() {
        let handle = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            timings: Timing::testing(),
            ..P2pConfig::default()
        })
        .expect("随机端口绑定必成功");
        let mut rx = handle.on_msg();
        handle.send(handle.self_id(), serde_json::json!({"loop": true}));
        let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("本地回环即时")
            .expect("broadcast 存活");
        assert_eq!(msg.from, *handle.self_id());
        assert_eq!(msg.hops, 0, "自环 0 跳");
        assert_eq!(msg.ttl, crate::transport::DEFAULT_TTL);
        assert_eq!(msg.payload["loop"], true);
        assert!(handle.peers().await.is_empty());
        assert!(handle.buckets_summary().await.is_empty());
        // 端点簿/阶梯观察面空态
        assert!(handle.known_endpoints().await.is_empty());
        assert!(handle.lookup_endpoint(handle.self_id()).await.is_none());
        let ladder = handle.ladder_stats().await;
        assert_eq!(ladder.direct, 0);
        let handle2 = handle.clone();
        handle.shutdown().await;
        // 停机后命令通道关闭 → 观察面返回空
        assert!(handle2.peers().await.is_empty());
    }

    // 3. 身份冲突检测（仅提示不阻断）：共用同一私钥（同 NodeID）的第二个
    //    节点拨入 → 本机记账 identity_conflicts + 连接照常建立（不拒绝）。
    #[tokio::test]
    async fn identity_conflict_recorded_without_rejecting_connection() {
        let identity = NodeIdentity::generate();
        let a = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            public: true,
            identity: Some(identity.clone()),
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .expect("随机端口绑定必成功");
        let b = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            bootstrap: vec![a.listen_addr()],
            identity: Some(identity), // 同一私钥 → 同 NodeID
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .expect("随机端口绑定必成功");
        // 等 B 引导拨号到 A → A 的 register_conn 检测到 peer NodeID == 本机
        //（B 可保活重拨多次——同公钥从多个端口进入，每条各记一笔，≥1 即可）
        let deadline = Instant::now() + Duration::from_secs(10);
        let conflicts = loop {
            let list = a.identity_conflicts().await;
            if !list.is_empty() || Instant::now() > deadline {
                break list;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert!(!conflicts.is_empty(), "应至少记录一条身份冲突");
        for c in &conflicts {
            assert_eq!(c.node_id, a.self_id().to_hex(), "冲突 NodeID = 本机公钥");
            assert!(
                c.remote_addr.contains(':'),
                "remote_addr 为观测 ip:port: {}",
                c.remote_addr
            );
            assert!(c.warning_count >= 1, "至少警告一次");
            assert!(
                c.first_seen > 0 && c.last_seen >= c.first_seen,
                "时间戳有效"
            );
        }
        // 连接不被拒绝：register_conn 全程走完（观测端点簿收录该 NodeID——
        // k-bucket 按设计忽略自身 ID，故 peers() 不含它，以端点簿为注册凭证）。
        let endpoints = a.known_endpoints().await;
        assert!(
            endpoints.iter().any(|e| e.id == *a.self_id()),
            "连接照常注册（端点簿应含同 NodeID 观测端点）: {endpoints:?}"
        );
        a.shutdown().await;
        b.shutdown().await;
    }

    // 4. 不同 NodeID（正常组网）→ 零身份冲突记账。
    #[tokio::test]
    async fn distinct_nodeids_record_no_identity_conflict() {
        let a = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            public: true,
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        let b = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            bootstrap: vec![a.listen_addr()],
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let peers = a.peers().await;
            if peers.iter().any(|p| p.id == *b.self_id() && p.connected)
                || Instant::now() > deadline
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            a.identity_conflicts().await.is_empty(),
            "不同 NodeID 不产生冲突记账"
        );
        assert!(b.identity_conflicts().await.is_empty(), "反向同样零冲突");
        a.shutdown().await;
        b.shutdown().await;
    }

    // 4b. 身份账本接线（os-identity 组件抽离，2026-08-25）：装配方注入共享
    //     账本 → register_conn 的握手证据落账（B 的观测地址在 B 名下 verified，
    //     owns_addr 判定 Verified）；停机 flush 后账本文件落盘（持久化往返）。
    //     mesh 走非回环本机地址（回环证据由账本侧拒绝——2026-08-25 定调）。
    #[tokio::test]
    async fn handshake_evidence_lands_in_injected_identity_ledger() {
        let dir = std::env::temp_dir().join(format!("p2p-identity-ledger-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("identity-ledger.json");
        let _ = std::fs::remove_file(&file);
        let ledger: os_identity::SharedLedger = Arc::new(Mutex::new(
            os_identity::IdentityLedger::new(Some(file.clone())),
        ));
        // A 注入共享账本（os-api 装配形态）；监听非回环本机地址——B 经它拨入，
        // A 侧观测地址非回环（回环证据不入账本地址集）
        let lan_ip = non_loopback_local_ipv4();
        let a = P2pNode::spawn(P2pConfig {
            listen: SocketAddr::new(std::net::IpAddr::V4(lan_ip), 0),
            public: true,
            identity_ledger: Some(ledger.clone()),
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        let b = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            bootstrap: vec![a.listen_addr()],
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        // 等 A 注册表收录 B（握手证据与 meta 记账同刻发生）
        let deadline = Instant::now() + Duration::from_secs(10);
        let b_addr = loop {
            let metas = a.node_meta().await;
            if let Some(e) = metas.iter().find(|e| e.id == *b.self_id()) {
                if let Some(ma) = e.addrs.first() {
                    break ma.addr;
                }
            }
            if Instant::now() > deadline {
                panic!("10s 内 A 的注册表未收录 B（握手证据无从产生）");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        // 握手证据落账：B 的观测地址在 B 名下 verified（owns_addr = 对比库判定）
        {
            let ledger = ledger.lock().expect("identity ledger poisoned");
            assert_eq!(
                ledger.owns_addr(b_addr, &b.self_id().to_hex()),
                os_identity::AddrOwnership::Verified,
                "握手观测地址应在 B 名下 verified"
            );
            let rec = ledger
                .snapshot()
                .into_iter()
                .find(|r| r.node_id == b.self_id().to_hex())
                .expect("账本应有 B 的记录");
            assert!(rec.verified_addrs.contains(&b_addr));
            assert!(rec.unverified_addrs.is_empty(), "直连观测不产生未验证地址");
        }
        // 停机 flush：账本文件落盘（注入形态的持久化由 p2p 引擎停机强刷承担）
        a.shutdown().await;
        assert!(file.exists(), "停机应强刷身份账本到注入的持久化文件");
        let reloaded = os_identity::IdentityLedger::new(Some(file.clone()));
        assert_eq!(
            reloaded.owns_addr(b_addr, &b.self_id().to_hex()),
            os_identity::AddrOwnership::Verified,
            "重载后判定不变（持久化往返）"
        );
        b.shutdown().await;
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&dir);
    }

    // 5. 手动重复拨号已连节点（Handle::dial 同地址二次调用）：register_conn
    //    去重拒绝 → dial_addr 应复用既有连接返回 Ok（曾误报"不可达/拨号失败"，
    //    手动添加节点控制台对已连节点重复添加即此路径）。
    #[tokio::test]
    async fn dial_already_connected_node_reuses_connection() {
        let a = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        let b = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        let id1 = b.dial(a.listen_addr()).await.expect("首次拨号应成功");
        assert_eq!(id1, *a.self_id());
        // 二次拨号同地址：握手验明同节点 → 复用既有连接，返回 Ok 而非 Err
        let id2 = b
            .dial(a.listen_addr())
            .await
            .expect("重复拨号应复用既有连接（语义=已连接），不得误报失败");
        assert_eq!(id2, *a.self_id());
        // 连接仍健康：对端视角 b 已连
        let peers = a.peers().await;
        assert!(
            peers.iter().any(|p| p.id == *b.self_id() && p.connected),
            "既有连接应保持存活: {peers:?}"
        );
        a.shutdown().await;
        b.shutdown().await;
    }

    // 6. 通告 unspecified 守卫（PUBLIC=1 + 监听 0.0.0.0 的等价单测）：public
    //    回退通告过滤不可拨的 unspecified 地址——A 以 public=true 监听
    //    0.0.0.0:0 运行（曾把 0.0.0.0:7070 通告出去污染对端路由表），B 拨入后
    //    peers 里 A 的 underlay 应为 None；真实地址经 observed_addr 观测。
    #[tokio::test]
    async fn public_node_listening_unspecified_advertises_nothing() {
        let a = P2pNode::spawn(P2pConfig {
            listen: "0.0.0.0:0".parse().unwrap(),
            public: true, // PUBLIC=1 + 监听 unspecified——回退通告应被守卫过滤
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        let b = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        // A 绑定 0.0.0.0 → 经 loopback 拨其真实端口
        let a_port = a.listen_addr().port();
        let dial = SocketAddr::new(std::net::IpAddr::from([127, 0, 0, 1]), a_port);
        let a_id = b.dial(dial).await.expect("loopback 拨入应成功");
        assert_eq!(a_id, *a.self_id());
        let peers = b.peers().await;
        let entry = peers
            .iter()
            .find(|p| p.id == *a.self_id())
            .expect("A 应出现在 B 的 peers");
        // 守卫生效：不可拨的 unspecified 监听地址不被通告（垃圾 underlay 根除）
        assert_eq!(
            entry.underlay, None,
            "public + 监听 0.0.0.0 不得通告 unspecified 地址"
        );
        // 真实地址经直连观测面暴露（端口 = A 的绑定端口，IP 为拨入路径落点）
        assert_eq!(
            entry.observed_addr,
            Some(dial),
            "observed_addr 应为 B 侧 socket 的对端地址（A 的真实地址）"
        );
        a.shutdown().await;
        b.shutdown().await;
    }

    // 7. peers_snapshot 的 observed_addr：B dial A 后，B 的 peers 里 A 条目
    //    observed_addr == A.listen_addr()（直连 socket 对端——握手验证过的
    //    第一手地址信号；非 public 节点 underlay=None，观测地址是唯一线索）。
    #[tokio::test]
    async fn peers_snapshot_exposes_direct_observed_addr() {
        let a = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        let b = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        let _ = b.dial(a.listen_addr()).await.expect("拨号应成功");
        let peers = b.peers().await;
        let entry = peers
            .iter()
            .find(|p| p.id == *a.self_id())
            .expect("A 应出现在 B 的 peers");
        assert!(entry.connected, "拨号后应已连接");
        assert_eq!(
            entry.observed_addr,
            Some(a.listen_addr()),
            "直连观测地址 = A 的监听地址（socket peer_addr）"
        );
        a.shutdown().await;
        b.shutdown().await;
    }

    // 8. is_local_target 指纹判断：self_id 与同 NodeID 节点为 true（含同私钥
    //    多 OS 实例——身份=密钥，同指纹即同权限域），不同节点为 false。
    #[tokio::test]
    async fn is_local_target_matches_own_fingerprint_only() {
        let a = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        let b = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        // 本机自身
        assert!(a.is_local_target(a.self_id()), "self_id 是本地目标");
        // 同 NodeID 的另一实例（clone 出同值——语义即同私钥对端）
        let same = a.self_id().clone();
        assert!(
            a.is_local_target(&same),
            "同 NodeID（同私钥实例）是本地目标"
        );
        // 不同节点
        assert!(!a.is_local_target(b.self_id()), "不同 NodeID 不是本地目标");
        assert!(!b.is_local_target(a.self_id()), "反向比较同样不命中");
        a.shutdown().await;
        b.shutdown().await;
    }

    // 9. 元数据心跳引擎（五振出局 + 手动复活）：B 拨入 A 建档（Direct/50）→
    //    B 停机 → A 指纹验证探测其地址（关闭端口不可达）连续 5 败 → Inactive
    //    （不再心跳）→ 手动 meta_reactivate 复活并立即探测（B 仍停机 → false，
    //    条目回 Active 后引擎再次探败出局）。未知节点手动探测 → false 且不建档。
    //    注：A 必须监听**非回环本机地址**——record_conn 对回环观测不入册
    //    （2026-08-25 回环彻底屏蔽定调），经 LAN IP 拨入才有注册表条目可测。
    #[tokio::test]
    async fn meta_heartbeat_five_strikes_then_manual_reactivate() {
        let a = P2pNode::spawn(P2pConfig {
            listen: SocketAddr::new(std::net::IpAddr::V4(non_loopback_local_ipv4()), 0),
            timings: Timing::testing(),
            mdns_enabled: false,
            meta_file: None, // 纯内存
            ..P2pConfig::default()
        })
        .unwrap();
        let b = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        // B 拨入 A → A 的注册表建档（register_conn 的 Direct 入口）
        let _ = b.dial(a.listen_addr()).await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let metas = a.node_meta().await;
            if metas.iter().any(|e| e.id == *b.self_id()) || Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let metas = a.node_meta().await;
        let e = metas
            .iter()
            .find(|e| e.id == *b.self_id())
            .expect("注册表应收录 B（所有连接过本节点的节点都记录）");
        assert!(matches!(e.state, MetaState::Active { .. }), "建档即 Active");
        assert_eq!(e.source, MetaSource::Direct, "本机直连观测");
        assert!(!e.addrs.is_empty());
        // B 停机：连接断开，A 对其观测地址指纹验证探测连续不可达失败
        //（meta_tick=150ms，分数 50 档每 3 tick 一败 → 降档后每 tick 一败 →
        // 5 败出局）
        let b_id = b.self_id().clone();
        b.shutdown().await;
        let deadline = Instant::now() + Duration::from_secs(10);
        let went_inactive = loop {
            let metas = a.node_meta().await;
            if metas
                .iter()
                .any(|e| e.id == b_id && matches!(e.state, MetaState::Inactive { .. }))
            {
                break true;
            }
            if Instant::now() > deadline {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert!(
            went_inactive,
            "连续 5 次心跳失败应移入 Inactive（不再心跳）"
        );
        // 手动复活 + 立即探测：B 仍停机 → 返回 false；复活后条目回 Active
        let ok = a.meta_reactivate(&b_id).await;
        assert!(!ok, "B 已停机，手动探测应失败");
        let deadline = Instant::now() + Duration::from_secs(5);
        let metas = loop {
            let metas = a.node_meta().await;
            // 复活后引擎恢复心跳，对停机节点会再次五振出局——只要出现过 Active
            // 即证明复活路径生效
            if metas
                .iter()
                .any(|e| e.id == b_id && matches!(e.state, MetaState::Active { .. }))
                || Instant::now() > deadline
            {
                break metas;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert!(
            metas
                .iter()
                .any(|e| e.id == b_id && matches!(e.state, MetaState::Active { .. })),
            "meta_reactivate 应复活条目（探测窗口内观察到 Active）"
        );
        // 未知节点手动探测：false 且不建档
        let stranger = NodeIdentity::generate().node_id();
        assert!(!a.meta_reactivate(&stranger).await, "未知节点无条目可复活");
        assert!(
            a.node_meta().await.iter().all(|e| e.id != stranger),
            "手动探测不新建档案（建档只走连接/交互入口）"
        );
        a.shutdown().await;
    }

    // 10. 元数据持久化往返：写盘（停机刷盘）→ 新 spawn（同 meta_file）→ 条目还在。
    //     注：A 监听非回环本机地址（同测试 9——回环观测不入册，LAN IP 拨入才建档）。
    #[tokio::test]
    async fn meta_persistence_across_spawn_restart() {
        let dir = std::env::temp_dir().join(format!("p2p-meta-restart-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("node-meta.json");
        let _ = std::fs::remove_file(&file);
        let a = P2pNode::spawn(P2pConfig {
            listen: SocketAddr::new(std::net::IpAddr::V4(non_loopback_local_ipv4()), 0),
            timings: Timing::testing(),
            mdns_enabled: false,
            meta_file: Some(file.clone()),
            ..P2pConfig::default()
        })
        .unwrap();
        let b = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        let _ = b.dial(a.listen_addr()).await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let metas = a.node_meta().await;
            if metas.iter().any(|e| e.id == *b.self_id()) || Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            a.node_meta().await.iter().any(|e| e.id == *b.self_id()),
            "停机前注册表应含 B"
        );
        // 停机 → 同步刷盘一次（防抖兜底路径）
        a.shutdown().await;
        assert!(file.exists(), "shutdown 应落盘元数据注册表");
        // "重启"：新 spawn（同 meta_file）→ 条目还在（重启不丢）
        let a2 = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            timings: Timing::testing(),
            mdns_enabled: false,
            meta_file: Some(file.clone()),
            ..P2pConfig::default()
        })
        .unwrap();
        let metas = a2.node_meta().await;
        let e = metas
            .iter()
            .find(|e| e.id == *b.self_id())
            .expect("重启后条目仍在（持久化生效）");
        assert!(!e.addrs.is_empty(), "地址历史保真");
        assert!(
            e.first_seen > 0 && e.last_seen >= e.first_seen,
            "时间戳保真"
        );
        a2.shutdown().await;
        b.shutdown().await;
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&dir);
    }

    // 11. 元数据交互 + 指纹验证探测洗白（探测侧不产生连接）：A 经 C 的交互
    //     摘要学到 B（Gossip / verified=false），心跳引擎对 B 的监听口做指纹
    //     验证探测（TCP connect + 复用握手路径比对 NodeID）——匹配成功 →
    //     分数上涨 + verified 洗白。探测不 register_conn（探测侧红线）：A 的
    //     条目来源保持 Gossip（Direct 只可能来自 register_conn）、A/B 互无
    //     已认证连接。B 侧按普通入站连接短暂登记探测者再 EOF 断开——既有
    //     入站路径的标准行为（其条目可能翻 Direct，与探测侧红线无关）。
    //     注：gossip 出入口过滤回环地址——学址经**非回环本机地址**（B 监听
    //     0.0.0.0、C 经 LAN IP 拨入观测到非回环地址，即生产路径）；若经
    //     127.0.0.1 互拨，摘要里的回环地址会在 C 的 digest 出口被剔除，A 无
    //     从学到 B（回环过滤语义见 meta.rs 单测）。
    #[tokio::test]
    async fn meta_gossip_learn_and_fingerprint_probe_verifies_without_connection() {
        let spawn_node = |listen: &str| {
            P2pNode::spawn(P2pConfig {
                listen: listen.parse().unwrap(),
                timings: Timing::testing(),
                mdns_enabled: false,
                meta_file: None,
                ..P2pConfig::default()
            })
            .unwrap()
        };
        let a = spawn_node("127.0.0.1:0");
        let b = spawn_node("0.0.0.0:0"); // 经本机 LAN IP 可拨（观测非回环）
        let c = spawn_node("127.0.0.1:0");
        let b_lan = SocketAddr::new(
            std::net::IpAddr::V4(non_loopback_local_ipv4()),
            b.listen_addr().port(),
        );
        // C → B（经 LAN IP）：C 的注册表记 B@b_lan（观测 = socket 对端 =
        // B 的监听口，verified=true——直连握手天然验证；非回环 → digest 携带）
        let _ = c.dial(b_lan).await.unwrap();
        // A → C：A 接收 C 的交互广播（每 6 tick 一次），学到 B（Gossip）
        let _ = a.dial(c.listen_addr()).await.unwrap();
        // 等 A 学到 B 且指纹验证探测洗白（verified=true + 分数 > 50——探测
        // 握手返回 B 的真实 NodeID 与条目匹配）
        let deadline = Instant::now() + Duration::from_secs(10);
        let learned = loop {
            let metas = a.node_meta().await;
            let hit = metas.iter().find(|e| e.id == *b.self_id()).and_then(|e| {
                if let MetaState::Active { score, .. } = e.state {
                    Some((score, e.verified))
                } else {
                    None
                }
            });
            if hit
                .is_some_and(|(score, verified)| verified && score > crate::meta::META_SCORE_START)
            {
                break true;
            }
            if Instant::now() > deadline {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert!(
            learned,
            "A 应经交互摘要学到 B 并经指纹验证探测洗白（无连接路径的活体+指纹证据）"
        );
        // A 的 B 条目来源保持 Gossip、verified=true——探测侧不 register_conn
        //（来源翻 Direct 只可能走 register_conn）
        let metas = a.node_meta().await;
        let e = metas
            .iter()
            .find(|e| e.id == *b.self_id())
            .expect("A 的注册表应含 B");
        assert_eq!(
            e.source,
            MetaSource::Gossip,
            "指纹验证探测不得注册连接（来源不应翻成 Direct）"
        );
        assert!(e.verified, "探测匹配后条目洗白为已验证");
        assert!(
            e.addrs.iter().any(|ma| ma.addr == b_lan && ma.verified),
            "探测命中的地址带验证标记"
        );
        // A 与 B 无 P2P 连接（B 若经 kad 八卦出现在 A 的路由表也必须未连接）
        let peers = a.peers().await;
        if let Some(p) = peers.iter().find(|p| p.id == *b.self_id()) {
            assert!(!p.connected, "元数据指纹探测不得建立 P2P 连接");
        }
        // B 侧对 A 无已认证连接（A 的探测握手对 B 是普通入站连接，EOF 即断）
        let peers = b.peers().await;
        if let Some(p) = peers.iter().find(|p| p.id == *a.self_id()) {
            assert!(!p.connected, "探测连接随握手读完即断——不留活连接");
        }
        a.shutdown().await;
        b.shutdown().await;
        c.shutdown().await;
    }

    // 12. 指纹验证探测防谎报闭环（地址易主）：A 以固定端口运行，B 经 C 的
    //     摘要学到 A 并探测洗白（verified=true）；A 停机后 D 复用同一端口——
    //     B 继续探测该地址：握手成功但 NodeID=D≠A → **指纹不匹配** → 记失败
    //     + 撤销 verified → 五振出局（Inactive）。裸 connect 会把 D 的监听
    //     误判为"A 仍存活"，指纹比对戳穿谎报/陈旧观测。全程探测不产生连接。
    #[tokio::test]
    async fn meta_fingerprint_mismatch_on_addr_takeover() {
        // 固定端口占位（A 用它监听；A 停机后 D 复用——bind_listener 带
        // SO_REUSEADDR/REUSEPORT，TIME_WAIT 不阻重绑）
        let takeover_port = {
            let l = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
            l.local_addr().unwrap().port()
        };
        // gossip 出入口过滤回环地址——学址与探测走**非回环本机地址**：A 绑定
        // LAN IP:port（127.0.0.1 观测会在 C 的 digest 出口被剔除，B 无从学到）
        let a_addr = SocketAddr::new(
            std::net::IpAddr::V4(non_loopback_local_ipv4()),
            takeover_port,
        );
        let spawn_on = |addr: SocketAddr| {
            P2pNode::spawn(P2pConfig {
                listen: addr,
                timings: Timing::testing(),
                mdns_enabled: false,
                meta_file: None,
                ..P2pConfig::default()
            })
            .unwrap()
        };
        let spawn_free = || {
            P2pNode::spawn(P2pConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                timings: Timing::testing(),
                mdns_enabled: false,
                meta_file: None,
                ..P2pConfig::default()
            })
            .unwrap()
        };
        let a = spawn_on(a_addr);
        let b = spawn_free();
        let c = spawn_free();
        // C 直连 A（C 记 A@a_addr，Direct/verified）→ B 连 C 收摘要学 A（Gossip）
        let _ = c.dial(a_addr).await.unwrap();
        let _ = b.dial(c.listen_addr()).await.unwrap();
        let a_id = a.self_id().clone();
        // 阶段一：A 在位，B 的指纹探测匹配 → verified=true 且分数上涨
        let deadline = Instant::now() + Duration::from_secs(10);
        let verified_ok = loop {
            let hit = b
                .node_meta()
                .await
                .into_iter()
                .find(|e| e.id == a_id)
                .and_then(|e| {
                    if let MetaState::Active { score, .. } = e.state {
                        Some((score, e.verified))
                    } else {
                        None
                    }
                });
            if hit
                .is_some_and(|(score, verified)| verified && score > crate::meta::META_SCORE_START)
            {
                break true;
            }
            if Instant::now() > deadline {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert!(verified_ok, "A 在位时探测应洗白 B 侧的 Gossip 条目");
        // 地址易主：A 停机 → D 复用同端口（地址背后换成另一个节点）
        a.shutdown().await;
        let d = spawn_on(a_addr);
        // 阶段二：B 继续探测 → 握手成功但指纹不匹配 → 失败累积 + verified 撤销
        // → 五振出局（Inactive；此后不再探测）
        let deadline = Instant::now() + Duration::from_secs(15);
        let mismatched = loop {
            let hit = b.node_meta().await.into_iter().find(|e| e.id == a_id);
            if hit.is_some_and(|e| !e.verified && matches!(e.state, MetaState::Inactive { .. })) {
                break true;
            }
            if Instant::now() > deadline {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert!(
            mismatched,
            "指纹不匹配应记失败并撤销 verified，连续失败出局 Inactive"
        );
        let e = b
            .node_meta()
            .await
            .into_iter()
            .find(|e| e.id == a_id)
            .expect("条目仍在注册表（出局不删档）");
        assert!(!e.verified, "地址易主后验证结论已撤销");
        assert!(
            e.addrs.iter().all(|ma| !ma.verified),
            "易主地址的验证标记一并撤销（不得再作为 A 的凭据外传）"
        );
        // 探测不产生连接：B 与 A/D 均无已认证连接
        let peers = b.peers().await;
        for p in peers
            .iter()
            .filter(|p| p.id == a_id || p.id == *d.self_id())
        {
            assert!(!p.connected, "指纹探测不得建立 P2P 连接");
        }
        b.shutdown().await;
        c.shutdown().await;
        d.shutdown().await;
    }

    // 13. NAT 对端的地址级连接判定回归（2026-08-24 连接风暴根因）：A 为 NAT
    //     节点（public=false 且未 advertise → 握手不通告 underlay），B 出站
    //     拨 A 注册成功后，connected_to_addr(A 监听地址) 必须为 true——曾按
    //     对端**通告**的 underlay 比对，NAT 对端 None 永远 miss → bootstrap
    //     保活每秒重拨、对端 register_conn 按 NodeID 去重全拒（113↔106 风暴）；
    //     A 停机断开后应回落 false。B 侧需直连引擎内部判定面，走 spawn_inner。
    #[tokio::test]
    async fn connected_to_addr_matches_nat_peer_by_observed_addr() {
        let a = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            public: false, // NAT 节点：不回退通告监听地址 → hello underlay=None
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        let (b_shared, b) = P2pNode::spawn_inner(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        let conn = dial_addr(&b_shared, a.listen_addr())
            .await
            .expect("出站拨号应成功");
        // 前置条件由配置保证：A public=false 且未 advertise → 握手不通告
        // underlay（正是修复前 connected_to_addr 永假的原因——连接上已不
        // 留 underlay 字段，地址维度只认 observed）
        assert_eq!(conn.observed, a.listen_addr(), "出站连接 observed=拨号目标");
        // 修复前：underlay=None 比对永远 miss → false → bootstrap 每秒重拨
        assert!(
            b_shared.connected_to_addr(&a.listen_addr()),
            "连接活跃时按 observed 地址应命中（NAT 对端 underlay=None）"
        );
        // A 停机 → socket 关闭 → B 侧 reader EOF → on_conn_closed 移除 → 回落
        let a_listen = a.listen_addr();
        a.shutdown().await;
        let deadline = Instant::now() + Duration::from_secs(10);
        while b_shared.connected_to_addr(&a_listen) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            !b_shared.connected_to_addr(&a_listen),
            "对端断开后应回落 false（不得滞留僵尸判定）"
        );
        let _ = b;
        b_shared.do_shutdown().await;
    }
}
