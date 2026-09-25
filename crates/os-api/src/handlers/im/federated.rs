//! IM 联邦域（2026-09-25 大文件拆分批，纯搬运零行为变化）：联邦大厅
//! （fed-lobby）广播载荷/节流器（FedThrottle）/延迟队列（FedBroadcastJob）
//! + 远程大厅查询探针（ImLobbyProbe）
//! + 联邦接收端 ImFederation（ingest 去重/身份改写/WS 广播）。
//!
//! 对外面经 im/mod.rs 重导出。

use super::*;

/// 联邦载荷类型标记（`payload.fed == "im_lobby"`）——旧版联邦大厅广播
/// （发送端已随旧 federate_lobby_message 删除；接收端仍兼容落 fed-lobby）。
pub const FED_KIND_IM_LOBBY: &str = "im_lobby";

/// 联邦载荷类型标记（`payload.fed == "im_fed_lobby_message"`）——联邦大厅
/// （fed-lobby 会话）发言的现行广播载荷。
pub const FED_KIND_IM_FED_LOBBY: &str = "im_fed_lobby_message";

/// 联邦载荷类型标记（`payload.fed == "im_dm"`）——跨节点**直通消息**（DM）的
/// 定向载荷（经 [`ImFederation::send_fed_to`] 发给目标节点，**非广播**；
/// 接收端 [`ImFederation::ingest_dm`] 只投递给 to_pubkey 对应的本节点身份）。
pub const FED_KIND_IM_DM: &str = "im_dm";

/// 远程消息 sender_id 前缀：入站远程消息改写为 `fed:<节点>:<原 pubkey>`——
/// 前端据此显示 🌐 远程徽章 + 来源节点（「来自 node-106」），同时与本地
/// pubkey 身份空间天然不碰撞（pubkey 恒以 `0x` 开头）。
pub const FED_SENDER_PREFIX: &str = "fed:";

/// 内存去重缓存容量（最近 1000 条远程消息 id，超出丢最旧；DB id 查重兜底）。
pub(super) const FED_SEEN_LIMIT: usize = 1000;

/// 身份冲突提示去重窗口（同一冲突源 5 分钟内只写一条系统警告——防大厅刷屏）。
pub(super) const IDENTITY_WARN_DEDUPE: Duration = Duration::from_secs(5 * 60);

// ----------------------------------------------------------------------------
// 联邦大厅发言时延节流（2026-08-24 用户需求：联邦大厅可一直发言、不限次数、
// 永不拒绝——但联邦广播带时延：常态每条 10s；同一发送者 60s 内第二次发言起
// 升为 1 分钟；安静满 60s 不发言后回落 10s。只影响联邦广播的**时刻**，本地
// 落库/WS 广播照常即时，消息永不丢弃）
// ----------------------------------------------------------------------------

/// 节流常态时延：每条联邦广播延迟 10s 发出。
pub const FED_THROTTLE_SHORT: Duration = Duration::from_secs(10);
/// 节流升级时延：同一发送者 60s 计数窗口内第 2 条起延迟 60s 发出。
pub const FED_THROTTLE_LONG: Duration = Duration::from_secs(60);
/// 节流计数窗口：发言时刻距今 ≥ 该窗口的旧时间戳不再计入次数（顺手清理）。
pub(super) const FED_THROTTLE_WINDOW: Duration = Duration::from_secs(60);

/// 联邦大厅发言节流器（进程内存态状态机，纯逻辑、时间可注入——单测无需真等）。
///
/// 状态 = `HashMap<sender_id, Vec<Instant>>`（各发送者近 [`FED_THROTTLE_WINDOW`]
/// 内的发言时刻）。语义（`delay_for`）：
///
/// - 首条（窗口内 0 条历史）→ [`FED_THROTTLE_SHORT`]（10s）；
/// - 窗口内已有 ≥1 条（含本次 ≥2）→ [`FED_THROTTLE_LONG`]（60s）；
/// - 安静满 60s 后旧时刻滑出窗口 → 回落 10s；
/// - 不同发送者互不影响（per-sender 计数）。
///
/// 时延以**入队时刻**一次性计算（简单确定——广播到期不随窗口滑动重算）；
/// 多实例各自独立计数、重启丢状态均**可接受**（节流只改变联邦广播时刻，
/// 不限次、不拒绝、不丢消息）。
#[derive(Debug)]
pub(super) struct FedThrottle {
    /// 常态时延（生产恒 [`FED_THROTTLE_SHORT`]；测试可注入 1ms 级短时延）。
    pub(super) short_delay: Duration,
    /// 升级时延（生产恒 [`FED_THROTTLE_LONG`]；测试可注入）。
    pub(super) long_delay: Duration,
    /// sender_id → 近 60s 窗口内发言时刻（入队时 push，超窗顺手清理）。
    history: HashMap<String, Vec<Instant>>,
}

impl FedThrottle {
    /// 生产构造：10s/60s 双时延（[`with_fed_throttle_delays`] 测试注入覆盖）。
    pub(super) fn new(short_delay: Duration, long_delay: Duration) -> Self {
        Self {
            short_delay,
            long_delay,
            history: HashMap::new(),
        }
    }

    /// 计算并记录 sender 本次发言的联邦广播时延（`now` 可注入——纯逻辑）：
    /// 清理 60s 窗口外的旧时刻后，窗口内历史 ≥1 条（含本次 ≥2）→ 升级时延，
    /// 否则常态时延；随后把 `now` 记入该 sender 的历史。返回值即延迟队列的
    /// 时延（due_at = now + delay 由调用方换算 tokio 时钟）。
    pub(super) fn delay_for(&mut self, sender: &str, now: Instant) -> Duration {
        let times = self.history.entry(sender.to_string()).or_default();
        // checked：防御非单调注入（未来时刻视为窗口外丢弃），绝不 panic
        times.retain(|t| {
            now.checked_duration_since(*t)
                .is_some_and(|age| age < FED_THROTTLE_WINDOW)
        });
        let delay = if times.is_empty() {
            self.short_delay
        } else {
            self.long_delay
        };
        times.push(now);
        delay
    }
}

/// 延迟队列的一条待广播任务：消息 + 到期时刻（tokio 时钟，入队时定格）。
pub(super) struct FedBroadcastJob {
    msg: Message,
    due_at: tokio::time::Instant,
}

/// 一条消息是否参与联邦（发送与接收共用同一裁决，纯函数）：
/// - 助手回复（`sender_kind == "agent"`，含 `agent:nexos-assistant`）**不联邦**
///   ——每个节点的 AI 只回本地，避免联邦网内重复 AI 回答；
/// - 系统消息（`sender_id == "system"` / `msg_type == "system"`，含入廊欢迎）
///   **不联邦**——入廊是本地事件，远程节点无需重复播报；
/// - 其余（人类大厅消息，无论来源本地还是远程）联邦。
#[must_use]
pub fn lobby_message_federable(msg: &Message) -> bool {
    msg.sender_kind != "agent" && msg.sender_id != "system" && msg.msg_type != "system"
}

/// 联邦节点名净化：空/超长（>64 字符）回退 `None`（调用方按非法载荷丢弃）——
/// `node` 来自对端自报，落库 sender_id 前限幅防病态值。
#[must_use]
pub(super) fn sanitize_fed_node_im(node: &str) -> Option<String> {
    let n = node.trim();
    if n.is_empty() || n.chars().count() > 64 {
        None
    } else {
        Some(n.to_string())
    }
}

/// 构造旧版 IM 大厅联邦广播载荷（纯函数，接收端兼容测试共用）：
/// `{"fed":"im_lobby","node":<发布节点>,"message":{...完整 Message JSON...}}`。
/// 发送端已删除（我的大厅不再联邦广播）——保留供旧节点载荷的接收语义测试。
#[must_use]
pub fn build_im_lobby_fed_payload(node: &str, msg: &Message) -> serde_json::Value {
    build_im_fed_lobby_payload_with_kind(FED_KIND_IM_LOBBY, node, msg)
}

/// 构造联邦大厅（fed-lobby 会话）联邦广播载荷（纯函数，发送端与测试共用）：
/// `{"fed":"im_fed_lobby_message","node":<发布节点>,"message":{...完整 Message...}}`。
#[must_use]
pub fn build_im_fed_lobby_payload(node: &str, msg: &Message) -> serde_json::Value {
    build_im_fed_lobby_payload_with_kind(FED_KIND_IM_FED_LOBBY, node, msg)
}

/// 载荷构造内核（两种 fed kind 共用形状，仅标记不同）。
pub(super) fn build_im_fed_lobby_payload_with_kind(
    kind: &str,
    node: &str,
    msg: &Message,
) -> serde_json::Value {
    serde_json::json!({
        "fed": kind,
        "node": sanitize_fed_node_im(node).unwrap_or_else(|| "peer".into()),
        "message": msg,
    })
}

// ----------------------------------------------------------------------------
// 大厅开放开关 + 远程大厅查询通道（2026-08-23，节点发现页「进入 IM」联动）
// ----------------------------------------------------------------------------

/// 联邦载荷类型标记（`payload.fed == "im_lobby_query"`）——请求浏览对方大厅。
pub const FED_KIND_IM_LOBBY_QUERY: &str = "im_lobby_query";
/// 联邦载荷类型标记（`payload.fed == "im_lobby_reply"`）——im_lobby_query 的应答。
pub const FED_KIND_IM_LOBBY_REPLY: &str = "im_lobby_reply";
/// 联邦载荷类型标记（`payload.fed == "im_lobby_post"`）——向对方大厅远程发言。
pub const FED_KIND_IM_LOBBY_POST: &str = "im_lobby_post";

/// 远程大厅镜像返回的消息条数上限（最近 20 条）。
pub const LOBBY_VIEW_LIMIT: usize = 20;

/// 查询应答缓存时长 + 同节点重查限频（30s——combined 每 10s 轮询也不致频繁查询）。
pub const LOBBY_QUERY_TTL: Duration = Duration::from_secs(30);

/// 远程大厅镜像消息（**脱敏 DTO**）：只含文本与元数据——**不含**
/// attachment/file_url/read_by/mentions（文件内容与读回执不出本机）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LobbyViewMessage {
    pub id: String,
    pub sender_id: String,
    pub sender_name: Option<String>,
    pub content: String,
    pub msg_type: String,
    pub created_at: String,
    pub sender_kind: String,
}

/// 消息 → 脱敏镜像（纯函数）：剥离附件/文件 URL/已读名单，仅保留展示必需字段。
#[must_use]
pub(super) fn sanitize_lobby_message(m: &Message) -> LobbyViewMessage {
    LobbyViewMessage {
        id: m.id.clone(),
        sender_id: m.sender_id.clone(),
        sender_name: m.sender_name.clone(),
        content: m.content.clone(),
        msg_type: m.msg_type.clone(),
        created_at: m.created_at.clone(),
        sender_kind: m.sender_kind.clone(),
    }
}

/// 远程大厅查询应答视图（探针侧解析 im_lobby_reply 的产物）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteLobbyView {
    /// 对方大厅开放开关（false 应答通常带 error="denied"）。
    pub public: bool,
    /// 应答错误标记（"denied" 等；开放成功为 None）。
    #[serde(default)]
    pub error: Option<String>,
    /// 开放时的脱敏消息镜像（≤ [`LOBBY_VIEW_LIMIT`] 条，时间正序）。
    #[serde(default)]
    pub messages: Vec<LobbyViewMessage>,
}

/// 从 im_lobby_reply 载荷解析应答视图（纯函数；结构非法返回 None）。
#[must_use]
pub(super) fn parse_lobby_reply(payload: &serde_json::Value) -> Option<RemoteLobbyView> {
    if payload.get("fed").and_then(|v| v.as_str()) != Some(FED_KIND_IM_LOBBY_REPLY) {
        return None;
    }
    let public = payload.get("public")?.as_bool()?;
    let messages = payload
        .get("messages")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| serde_json::from_value::<LobbyViewMessage>(v.clone()).ok())
                .take(LOBBY_VIEW_LIMIT)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Some(RemoteLobbyView {
        public,
        error: payload
            .get("error")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        messages,
    })
}

/// 远程发言 sender_id 净化：非空且 ≤80 字符（防病态自报值落库）。
#[must_use]
pub(super) fn sanitize_fed_sender(id: &str) -> Option<String> {
    let s = id.trim();
    if s.is_empty() || s.chars().count() > 80 {
        None
    } else {
        Some(s.to_string())
    }
}

/// 构造远程大厅发言载荷（纯函数，REST 端点与测试共用）：
/// `{"fed":"im_lobby_post","node":<本节点>,"sender_id":<发言者 pubkey>,
///    "sender_name":<展示名>,"content":<正文>}`——不含任何 token/附件。
#[must_use]
pub fn build_lobby_post_payload(
    node: &str,
    sender_id: &str,
    sender_name: &str,
    content: &str,
) -> serde_json::Value {
    serde_json::json!({
        "fed": FED_KIND_IM_LOBBY_POST,
        "node": sanitize_fed_node_im(node).unwrap_or_else(|| "peer".into()),
        "sender_id": sender_id,
        "sender_name": sender_name,
        "content": content,
    })
}

/// 远程大厅查询超时解析（`?timeout_ms=` 300..=8000 钳制，缺省 4000）。
pub(super) fn lobby_query_timeout(raw: Option<&str>) -> Duration {
    let ms = raw
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(4000)
        .clamp(300, 8000);
    Duration::from_millis(ms)
}

/// 探针缓存条目：最近一次应答标志 + 限频水位（消息全量只在阻塞查询的
/// oneshot 通道流转，缓存不重复存——combined 只消费 public 布尔）。
#[derive(Debug, Clone)]
pub(super) struct ProbeEntry {
    /// 最近一次应答的 public 标志（None = 查询中/未应答）。
    public: Option<bool>,
    /// 最近一次**发出查询**的时刻（30s 限频基准——无论是否已应答）。
    last_queried: Instant,
}

impl ProbeEntry {
    fn fresh_placeholder() -> Self {
        Self {
            public: None,
            // 占位为零：首次 status() 即视为过期并发起查询
            last_queried: Instant::now() - LOBBY_QUERY_TTL,
        }
    }
}

/// 远程大厅查询探针（查询端）：持有组网 Handle，向对端发 `im_lobby_query`
/// 并消费 `im_lobby_reply`（独立 `on_msg` 订阅者——broadcast 多订阅者互不影响
/// FederationBridge 的既有分发）。
///
/// - **缓存**：节点 hex → [`ProbeEntry`]，30s 限频（combined 10s 轮询下同节点
///   至多 30s 一查）；未应答节点返回 `None`（UI 渲染"查询中"）；
/// - **在途关联**：req_id → oneshot——REST 阻塞查询（GET /lobby/remote/:id）
///   限时等待应答；应答迟到（超时后到达）只更新缓存，oneshot send 失败即弃。
pub struct ImLobbyProbe {
    /// 组网句柄（发送查询；与 FederationBridge 共享同一底层节点）。
    handle: os_p2p::Handle,
    /// 本节点名（查询载荷 `node` 字段，对端日志归因用）。
    name: String,
    /// 节点 hex → 缓存条目（std Mutex 短锁快放，不跨 await）。
    cache: Mutex<HashMap<String, ProbeEntry>>,
    /// req_id → 在途阻塞查询的应答通道（应答任务完成即摘除）。
    pending: Mutex<HashMap<String, tokio::sync::oneshot::Sender<RemoteLobbyView>>>,
}

impl ImLobbyProbe {
    /// 创建探针并启动应答消费 task（须在 tokio runtime 内调用——
    /// [`ImFederation::set_p2p`] 在 main.rs 装配/测试内均满足）。
    fn spawn(handle: os_p2p::Handle, name: String) -> Arc<Self> {
        let probe = Arc::new(Self {
            handle,
            name,
            cache: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
        });
        let this = probe.clone();
        let mut rx = this.handle.on_msg();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(m) => this.apply_reply(&m),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("[fed] 大厅探针落后 {n} 条（跳过，30s 后重查）");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        probe
    }

    /// 处理一条入站消息：仅识别 `im_lobby_reply`——按 req_id 完成在途等待 +
    /// 按发送者 hex 更新缓存（非本类载荷静默忽略）。
    fn apply_reply(&self, msg: &os_p2p::P2pMsg) {
        let Some(view) = parse_lobby_reply(&msg.payload) else {
            return;
        };
        if let Some(rid) = msg.payload.get("req_id").and_then(|v| v.as_str()) {
            if let Some(tx) = self
                .pending
                .lock()
                .expect("probe pending poisoned")
                .remove(rid)
            {
                let _ = tx.send(view.clone()); // 接收方已超时放弃 → send 失败即弃
            }
        }
        let from = msg.from.to_hex();
        let mut cache = self.cache.lock().expect("probe cache poisoned");
        let entry = cache
            .entry(from)
            .or_insert_with(ProbeEntry::fresh_placeholder);
        entry.public = Some(view.public);
        // last_queried 不动：限频基准是"发出查询"，与应答到达无关
    }

    /// 发一条查询（不注册等待；req_id 由调用方决定是否用于 oneshot 关联）。
    fn send_query(&self, node: &os_p2p::NodeId, req_id: &str) {
        self.handle.send(
            node,
            serde_json::json!({
                "fed": FED_KIND_IM_LOBBY_QUERY,
                "node": self.name,
                "req_id": req_id,
            }),
        );
    }

    /// 非阻塞状态查询（node_view combined 每 10s 轮询用）：
    /// - 有效条目且 30s 内已查 → 返回缓存的 public（未应答为 None = 查询中）；
    /// - 无条目 / 已过期（30s）→ 发一条新查询（刷新限频水位），返回既有
    ///   public（首次为 None——下次轮询可见应答）；
    /// - 短 ID（Kademlia 桶条目 `0x1234…cdef`，非全量 66 hex）不可查询 → None。
    #[must_use]
    pub fn status(&self, node_hex: &str) -> Option<bool> {
        // 桶条目短式/非法 id：无从寻址
        let node = os_p2p::NodeId::parse(node_hex)?;
        let mut cache = self.cache.lock().expect("probe cache poisoned");
        let entry = cache
            .entry(node_hex.to_string())
            .or_insert_with(ProbeEntry::fresh_placeholder);
        let known = entry.public;
        if Instant::now().saturating_duration_since(entry.last_queried) >= LOBBY_QUERY_TTL {
            entry.last_queried = Instant::now();
            drop(cache); // 短锁纪律：send 前放锁
            self.send_query(&node, &new_uuid());
        }
        known
    }

    /// 阻塞查询（REST GET /lobby/remote/:id 与发言前置检查用）：注册 oneshot
    /// → 发查询 → 限时等待应答。`None` = 超时/无应答（缓存保留，供 combined）。
    pub async fn query(&self, node: &os_p2p::NodeId, timeout: Duration) -> Option<RemoteLobbyView> {
        let req_id = new_uuid();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending
            .lock()
            .expect("probe pending poisoned")
            .insert(req_id.clone(), tx);
        {
            // 同步推进限频水位（紧跟其后的 combined 轮询不再重复发查询）
            let mut cache = self.cache.lock().expect("probe cache poisoned");
            let entry = cache
                .entry(node.to_hex())
                .or_insert_with(ProbeEntry::fresh_placeholder);
            entry.last_queried = Instant::now();
        }
        self.send_query(node, &req_id);
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(view)) => Some(view),
            _ => None, // 超时或发送端 drop（应答迟到已入缓存）
        }
    }
}

/// IM 联邦端点——`Arc<ImShared>` 的薄封装（Clone 共享同一内核）。
///
/// main.rs 装配：`im_handler.federation()` 在 Box 进网关**之前**取出，
/// p2p spawn 成功后 `set_p2p` 注入 Handle（P2P 未启用保持未注入——发送
/// 静默跳过）；os-api 的 `FederationBridge`（handlers/p2p.rs）持同一端点
/// 把入站 `fed == "im_lobby"` 载荷分发给 [`Self::ingest`]。
#[derive(Clone)]
pub struct ImFederation {
    pub(super) shared: Arc<ImShared>,
}

/// [`ImFederation::ingest`] 的处置结果（接收端观测面，测试/诊断用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImFedIngest {
    /// 新消息已写入本地 im_messages + WS 广播本地在线用户。
    Written,
    /// 重复（同消息 id 已存在——内存缓存或 DB 命中），未重写。
    Duplicate,
    /// 非联邦大厅载荷（im_fed_lobby_message/im_lobby 以外）/ 结构非法 /
    /// 不可联邦消息（agent/系统），忽略。
    Ignored,
    /// 联邦接收开关已关（`fed_enabled=false`，POST /api/v1/im/federation），
    /// 入口短路——不解析载荷、不写库、不广播（2026-08-23）。
    Paused,
}

impl ImFederation {
    /// 注入组网 Handle + 本节点名（main.rs 装配：p2p spawn 成功后调用；
    /// 重复注入覆盖——测试/热替换友好）。同步锁写入（std Mutex，无 await）。
    /// 注入同时创建远程大厅查询探针（[`ImLobbyProbe`]——独立 on_msg 订阅者）。
    pub fn set_p2p(&self, handle: os_p2p::Handle, node: String) {
        let node = sanitize_fed_node_im(&node).unwrap_or_else(|| "peer".into());
        eprintln!("[fed] p2p handle injected（node={node}）");
        let probe = ImLobbyProbe::spawn(handle.clone(), node.clone());
        *self.shared.fed_p2p.lock().expect("fed_p2p poisoned") = Some((handle, node));
        *self
            .shared
            .lobby_probe
            .lock()
            .expect("lobby_probe poisoned") = Some(probe);
    }

    /// 是否已装配（未装配 = P2P 未启用，联邦发送静默跳过）。
    #[must_use]
    pub fn is_federated(&self) -> bool {
        self.shared
            .fed_p2p
            .lock()
            .expect("fed_p2p poisoned")
            .is_some()
    }

    /// 本节点名（远程发言载荷 `node` 字段用；未装配回退 "peer"）。
    #[must_use]
    pub fn node_name(&self) -> String {
        self.shared
            .fed_p2p
            .lock()
            .expect("fed_p2p poisoned")
            .as_ref()
            .map(|(_, n)| n.clone())
            .unwrap_or_else(|| "peer".into())
    }

    /// 把一条联邦载荷定向发给指定节点（远程大厅发言的发送通道；未装配
    /// P2P 返回 false——调用方按 503 处理）。
    pub fn send_fed_to(&self, node: &os_p2p::NodeId, payload: serde_json::Value) -> bool {
        match self
            .shared
            .fed_p2p
            .lock()
            .expect("fed_p2p poisoned")
            .clone()
        {
            Some((handle, _)) => {
                handle.send(node, payload);
                true
            }
            None => false,
        }
    }

    /// 大厅**开放**开关当前状态（true = 允许其他节点浏览本机大厅；默认 false）。
    #[must_use]
    pub fn lobby_public(&self) -> bool {
        self.shared.lobby_public.load(Ordering::Relaxed)
    }

    /// 设置大厅开放开关（POST /api/v1/im/lobby/access 的内核；返回设置后状态）。
    pub fn set_lobby_public(&self, public: bool) -> bool {
        self.shared.lobby_public.store(public, Ordering::Relaxed);
        public
    }

    /// 非阻塞的对方开放状态（node_view combined 聚合用）：探针缓存命中返回
    /// `Some(public)`；未应答/短 ID/P2P 未启用返回 None（UI 渲染"查询中"）。
    /// 过期（30s）时顺带发起一次后台重查。
    #[must_use]
    pub fn lobby_status(&self, node_hex: &str) -> Option<bool> {
        self.shared
            .lobby_probe
            .lock()
            .expect("lobby_probe poisoned")
            .as_ref()
            .and_then(|probe| probe.status(node_hex))
    }

    /// 阻塞查询对方大厅（REST GET /lobby/remote/:id 内核）：限时等待应答；
    /// `None` = 超时/无应答/P2P 未启用。
    pub async fn remote_lobby(
        &self,
        node: &os_p2p::NodeId,
        timeout: Duration,
    ) -> Option<RemoteLobbyView> {
        let probe = self
            .shared
            .lobby_probe
            .lock()
            .expect("lobby_probe poisoned")
            .clone()?;
        probe.query(node, timeout).await
    }

    /// 应答端的查询载荷计算（纯计算不联网，测试可直接断言）：
    /// - 开关关 → `{"public":false,"error":"denied"}`（不读库）；
    /// - 开关开（开发期缺省）→ `{"public":true,"messages":[≤20 条脱敏消息]}`
    ///   （时间正序）。
    #[must_use]
    pub fn lobby_query_reply_payload(&self, req_id: &str) -> serde_json::Value {
        if !self.lobby_public() {
            return serde_json::json!({
                "fed": FED_KIND_IM_LOBBY_REPLY,
                "req_id": req_id,
                "public": false,
                "error": "denied",
            });
        }
        let messages = {
            let conn = self.shared.db.lock().expect("db poisoned");
            load_recent_lobby_messages(&conn, LOBBY_VIEW_LIMIT, None).unwrap_or_default()
        };
        serde_json::json!({
            "fed": FED_KIND_IM_LOBBY_REPLY,
            "req_id": req_id,
            "public": true,
            "messages": messages.iter().map(sanitize_lobby_message).collect::<Vec<_>>(),
        })
    }

    /// 网络入口：收到 `im_lobby_query`（经 handlers/p2p.rs FederationBridge 分发）
    /// → 计算应答（开关裁决 + 脱敏镜像）→ 发回查询方。缺 req_id / Handle 未
    /// 注入（理论上桥在则必注入）仅记日志。
    pub fn answer_lobby_query(&self, from: &os_p2p::NodeId, payload: &serde_json::Value) {
        if payload.get("fed").and_then(|v| v.as_str()) != Some(FED_KIND_IM_LOBBY_QUERY) {
            return;
        }
        let Some(req_id) = payload.get("req_id").and_then(|v| v.as_str()) else {
            eprintln!("[fed] im_lobby_query 缺 req_id，忽略");
            return;
        };
        // 指纹判断：查询方==本机 NodeID（同私钥多 OS 实例的自发查询）→ 不走
        // P2P 应答——send 到本机指纹会本地回环自答，探针缓存被自己刷写毫无
        // 意义；本地查询路径（remote_lobby REST）本就直接读库，无需回声。
        if let Some((handle, _)) = self
            .shared
            .fed_p2p
            .lock()
            .expect("fed_p2p poisoned")
            .clone()
        {
            if handle.is_local_target(from) {
                eprintln!("[fed] im_lobby_query 来自本机指纹节点，跳过应答（本地自回路）");
                return;
            }
        }
        let reply = self.lobby_query_reply_payload(req_id);
        eprintln!(
            "[fed] im_lobby_query 应答 public={}（{} 条镜像）",
            self.lobby_public(),
            reply["messages"].as_array().map_or(0, Vec::len)
        );
        if !self.send_fed_to(from, reply) {
            eprintln!("[fed] im_lobby_query 到达但 p2p handle 未注入，无法应答");
        }
    }

    /// 网络入口：收到 `im_lobby_post`（远程大厅发言）→ 裁决落地。
    ///
    /// - 联邦接收暂停（fed_enabled=false）→ `Paused`（与 ingest 同一道闸门）；
    /// - 大厅未开放（lobby_public=false）→ `Ignored`（静默丢弃，日志留痕）；
    /// - 载荷非法（缺 node/sender/content 空/超限）→ `Ignored`；
    /// - 通过 → 构造 Message（id 服务端生成；sender_id 改写 `fed:<node>:<pubkey>`；
    ///   **不承载附件**）写入大厅 + WS 广播本地在线用户 → `Written`。
    pub fn ingest_lobby_post(&self, payload: &serde_json::Value) -> ImFedIngest {
        if !self.fed_enabled() {
            return ImFedIngest::Paused;
        }
        if payload.get("fed").and_then(|v| v.as_str()) != Some(FED_KIND_IM_LOBBY_POST) {
            return ImFedIngest::Ignored;
        }
        if !self.lobby_public() {
            eprintln!("[fed] 远程大厅发言被拒（大厅未开放）");
            return ImFedIngest::Ignored;
        }
        let Some(node) = payload
            .get("node")
            .and_then(|v| v.as_str())
            .and_then(sanitize_fed_node_im)
        else {
            return ImFedIngest::Ignored;
        };
        let Some(sender) = payload
            .get("sender_id")
            .and_then(|v| v.as_str())
            .and_then(sanitize_fed_sender)
        else {
            return ImFedIngest::Ignored;
        };
        let content = payload
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if content.is_empty() || content.chars().count() > 4000 {
            return ImFedIngest::Ignored; // 空正文/超长（>4000 字符）不落地
        }
        let sender_name = payload
            .get("sender_name")
            .and_then(|v| v.as_str())
            .map(|s| s.chars().take(64).collect::<String>());
        let msg = Message {
            id: new_uuid(),
            conversation_id: LOBBY_ID.to_string(),
            // 直接发到本节点大厅的消息：不加 fed: 前缀——在接收方"我的大厅"显示
            // （fed: 前缀会被前端归入"联邦大厅"，但这是对方直接发到我的大厅的，
            //   应在"我的大厅"显示，sender_name 标注远端来源）
            sender_id: sender.to_string(),
            sender_name: sender_name
                .map(|n| format!("🌐 {n}（{node}）"))
                .or_else(|| Some(format!("🌐 {node}"))),
            content,
            msg_type: "text".to_string(),
            file_url: None,
            reply_to: None,
            created_at: now_iso(),
            read_by: Vec::new(),
            sender_kind: "human".to_string(),
            mentions: Vec::new(),
            attachment: None, // 远程发言通道不承载附件
        };
        {
            let conn = self.shared.db.lock().expect("db poisoned");
            if insert_message(&conn, &msg).is_err() {
                return ImFedIngest::Ignored;
            }
        }
        ImRouteHandler::broadcast_lobby(&self.shared.ws_hub, &msg);
        ImFedIngest::Written
    }

    /// 登记联邦大厅消息发送方的 DM 路由（FederationBridge 在 ingest 联邦大厅
    /// 载荷时顺带调用，2026-08-30）：把发送方 pubkey → 发送方 NodeID（P2P 层
    /// 验签真值）记入 `im_dm_peers`——之后本节点身份对其发起 DM 无需带
    /// to_node，自动定向到其节点（跨节点私信从联邦大厅「私聊」即可发起）。
    /// 幂等；非 0x 链上身份（系统/agent）静默跳过。
    pub fn register_fed_sender_route(&self, from: &os_p2p::NodeId, payload: &serde_json::Value) {
        let Some(sender) = payload
            .get("message")
            .and_then(|m| m.get("sender_id"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| parse_im_pubkey(s).is_some())
        else {
            return;
        };
        let name = payload
            .get("message")
            .and_then(|m| m.get("sender_name"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let conn = self.shared.db.lock().expect("db poisoned");
        upsert_dm_peer(&conn, sender, &from.to_hex(), name);
    }

    /// 联邦**接收**开关当前状态（true = 接收远程大厅消息；默认 true）。
    #[must_use]
    pub fn fed_enabled(&self) -> bool {
        self.shared.fed_enabled.load(Ordering::Relaxed)
    }

    /// 直通消息（DM）**开放**开关当前状态（true = 允许其他身份发给本节点
    /// 身份私信；开发阶段缺省 true——用户裁决「当前开发阶段默认允许」）。
    #[must_use]
    pub fn dm_open(&self) -> bool {
        self.shared.dm_open.load(Ordering::Relaxed)
    }

    /// 设置直通消息开放开关（POST /api/v1/im/dm/access 的内核；返回设置后
    /// 状态）。false = 本地 POST /im/dm 对本节点身份 403、跨节点 `im_dm`
    /// ingest 丢弃；自己发出的 DM 不受影响。
    pub fn set_dm_open(&self, open: bool) -> bool {
        self.shared.dm_open.store(open, Ordering::Relaxed);
        open
    }

    /// 网络入口：收到跨节点直通消息 `im_dm`（经 handlers/p2p.rs
    /// FederationBridge 分发；`from` = P2P 层验签的发送方 NodeID）→ 裁决落地。
    ///
    /// - 载荷非法（缺 from/to/content 空或超限/pubkey 非法）→ `Ignored`；
    /// - 本机直通开关关（dm_open=false）→ `Ignored`（静默丢弃，日志留痕——
    ///   与远程大厅发言同语义，不回执不重投）；
    /// - 收件人不在本节点（错投/收件人从未在本节点认证）→ `Ignored`；
    /// - 去重：`msg_id` 内存缓存 + DB 查重 → `Duplicate`；
    /// - 通过 → 确定性会话（同发送端算法）+ 双方成员 + 对端登记（回程路由）
    ///   + 落库（id=msg_id）→ **定向 WS 只推收件人** → `Written`。
    ///
    /// 注意：不经联邦接收开关（fed_enabled 只管大厅类联邦消息；DM 的闸门
    /// 是自己的 dm_open）。
    pub fn ingest_dm(&self, from: &os_p2p::NodeId, payload: &serde_json::Value) -> ImFedIngest {
        if payload.get("fed").and_then(|v| v.as_str()) != Some(FED_KIND_IM_DM) {
            return ImFedIngest::Ignored;
        }
        if !self.dm_open() {
            eprintln!("[dm] 远程直通消息被拒（本机未开放直通消息）");
            return ImFedIngest::Ignored;
        }
        let valid_pubkey = |v: &serde_json::Value| {
            v.as_str()
                .map(str::trim)
                .filter(|s| parse_im_pubkey(s).is_some())
                .map(str::to_string)
        };
        let Some(from_pub) = payload.get("from_pubkey").and_then(valid_pubkey) else {
            return ImFedIngest::Ignored;
        };
        let Some(to_pub) = payload.get("to_pubkey").and_then(valid_pubkey) else {
            return ImFedIngest::Ignored;
        };
        let content = payload
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if content.is_empty() || content.chars().count() > 4000 {
            return ImFedIngest::Ignored;
        }
        let ts = payload
            .get("ts")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let msg_id = payload
            .get("msg_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| dm_message_id(&from_pub, &to_pub, &content, &ts));
        let node = payload
            .get("node")
            .and_then(|v| v.as_str())
            .and_then(sanitize_fed_node_im)
            .unwrap_or_else(|| "peer".into());
        let from_name = payload
            .get("from_name")
            .and_then(|v| v.as_str())
            .map(|s| s.chars().take(64).collect::<String>());
        let cid = dm_conversation_id(&from_pub, &to_pub);
        let sender_name = from_name
            .map(|n| format!("🌐 {n}（{node}）"))
            .or_else(|| Some(format!("🌐 {node}")));
        let msg = Message {
            id: msg_id,
            conversation_id: cid.clone(),
            // 跨节点 DM 保留原始发送者 pubkey（不加 fed: 前缀）——收件人直接
            // 以此为 to_pubkey 回信（回程路由走 im_dm_peers 登记）；来源经
            // sender_name 标注（与远程大厅发言同款）。
            sender_id: from_pub.clone(),
            sender_name,
            content,
            msg_type: "text".to_string(),
            file_url: None,
            reply_to: None,
            created_at: if ts.is_empty() { now_iso() } else { ts },
            read_by: Vec::new(),
            sender_kind: "human".to_string(),
            mentions: Vec::new(),
            attachment: None, // DM 通道不承载附件
        };
        {
            let conn = self.shared.db.lock().expect("db poisoned");
            // 错投判定：收件人不是本节点身份（大厅不在场且无 WS 订阅）→ 丢弃
            if !self.shared.identity_local(&conn, &to_pub) {
                eprintln!("[dm] 直通消息收件人 {to_pub} 不在本节点，丢弃");
                return ImFedIngest::Ignored;
            }
            {
                let mut seen = self.shared.fed_seen.lock().expect("fed_seen poisoned");
                if seen.contains(&msg.id) {
                    return ImFedIngest::Duplicate;
                }
                seen.push_back(msg.id.clone());
                while seen.len() > FED_SEEN_LIMIT {
                    seen.pop_front();
                }
            }
            if find_message(&conn, &msg.id).unwrap_or(None).is_some() {
                return ImFedIngest::Duplicate; // 重启后缓存为空——DB 兜底
            }
            self.shared
                .ensure_dm_conversation(&conn, &cid, &from_pub, &to_pub);
            // 回程路由登记：发送方 pubkey → 发送方 NodeID（P2P 验签真值）
            upsert_dm_peer(
                &conn,
                &from_pub,
                &from.to_hex(),
                payload
                    .get("from_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
            );
            if insert_message(&conn, &msg).is_err() {
                return ImFedIngest::Ignored;
            }
        }
        // 定向 WS：只推收件人（发送端自留档在其节点，本节点无其订阅）
        self.shared.push_dm_ws(&cid, &msg, &[to_pub.as_str()]);
        ImFedIngest::Written
    }

    /// 设置联邦接收开关（POST /api/v1/im/federation 的内核；返回设置后的
    /// 状态）。关闭仅影响 ingest 入口——本地消息与发送广播不受影响。
    pub fn set_fed_enabled(&self, enabled: bool) -> bool {
        self.shared.fed_enabled.store(enabled, Ordering::Relaxed);
        enabled
    }

    /// 发送端：联邦大厅（fed-lobby 会话）消息联邦广播给全部已连接 peer。
    ///
    /// - 不可联邦消息（agent/系统，[`lobby_message_federable`]）/ 未装配 P2P
    ///   → 静默跳过，返回 `false`（不阻塞本地写入语义）；
    /// - 广播 = 经 `fed_broadcast` 发载荷 `{"fed":"im_fed_lobby_message",...}`
    ///   给每个已连接 peer（fire-and-forget），返回是否发出（有 peer 即 true
    ///   ——送达在对端 ingest 体现）。**本地指纹目标**（NodeID==本机，同私钥
    ///   多 OS 实例）在 `fed_broadcast` 内跳过——消息已在本地落库，发给同
    ///   指纹节点只会自回路重复入库；
    /// - `[fed]` 日志观测面（journalctl -u os-api | grep "\[fed\]"）：
    ///   入口 `broadcasting fed-lobby message` / 未注入跳过 / 实际送达 peer 数。
    ///
    /// （旧版 `federate_lobby_message`——POST /lobby/messages 自动广播——已随
    /// 「我的大厅与联邦完全隔离」删除，2026-08-23。）
    pub async fn federate_fed_lobby_message(&self, msg: &Message) -> bool {
        if !lobby_message_federable(msg) {
            return false;
        }
        eprintln!("[fed] broadcasting fed-lobby message: {}", msg.id);
        let (handle, node) = match self
            .shared
            .fed_p2p
            .lock()
            .expect("fed_p2p poisoned")
            .clone()
        {
            Some(v) => v,
            None => {
                // P2P 未启用/未注入：静默跳过（本地写入语义不受影响），日志留痕
                eprintln!("[fed] skip broadcast（p2p handle 未注入）: {}", msg.id);
                return false;
            }
        };
        let sent =
            crate::handlers::p2p::fed_broadcast(&handle, build_im_fed_lobby_payload(&node, msg))
                .await;
        eprintln!("[fed] broadcast done: {} → {sent} peer(s)", msg.id);
        sent > 0
    }

    /// 发送端（节流入口，POST /fed-lobby/messages 专用，2026-08-24）：计算
    /// sender 的节流时延（[`FedThrottle::delay_for`]，以入队时刻定格）并把
    /// 消息推入**延迟广播队列**，立即返回时延——本地落库与 WS 广播已由路由
    /// 层先行完成（本节点体验不变，联邦广播延迟到期再发）。
    ///
    /// - 不可联邦消息（agent/系统，[`lobby_message_federable`]）不入队、不
    ///   计数，返回 `ZERO`（响应层据此提示"不参与联邦广播"）；
    /// - **不限次、不拒绝、不丢消息**：unbounded 队列 + 单 worker 串行按
    ///   入队序发送；进程内态——重启/多实例丢队列各自独立，可接受。
    pub fn enqueue_fed_lobby_broadcast(&self, msg: &Message) -> Duration {
        if !lobby_message_federable(msg) {
            return Duration::ZERO;
        }
        let delay = {
            let mut throttle = self
                .shared
                .fed_throttle
                .lock()
                .expect("fed_throttle poisoned");
            throttle.delay_for(&msg.sender_id, Instant::now())
        };
        self.push_fed_broadcast_queue(msg.clone(), delay);
        eprintln!(
            "[fed] fed-lobby message queued: {} → federate in {}s",
            msg.id,
            delay.as_secs()
        );
        delay
    }

    /// 延迟广播队列内核：惰性启动单 worker（`recv → sleep 到期 →
    /// federate_fed_lobby_message`，串行——按入队序发送，先入队者的 sleep
    /// 不被插队）；unbounded channel 消息永不因背压丢弃。worker 持本
    /// `ImFederation` 克隆（Arc 句柄），随 runtime 生命周期存活。
    fn push_fed_broadcast_queue(&self, msg: Message, delay: Duration) {
        let mut tx_slot = self
            .shared
            .fed_broadcast_tx
            .lock()
            .expect("fed_broadcast_tx poisoned");
        if tx_slot.is_none() {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<FedBroadcastJob>();
            let fed = self.clone();
            tokio::spawn(async move {
                while let Some(job) = rx.recv().await {
                    let now = tokio::time::Instant::now();
                    if job.due_at > now {
                        tokio::time::sleep_until(job.due_at).await;
                    }
                    // 到期即发（到期时刻早于当前则立即）；federate 内部自兜
                    // 不可联邦/P2P 未注入 → 静默跳过，队列继续
                    fed.federate_fed_lobby_message(&job.msg).await;
                }
            });
            *tx_slot = Some(tx);
        }
        // send 失败仅当 worker 所在 runtime 已关停（进程退出）——与重启丢队列
        // 同语义，可接受
        let _ = tx_slot
            .as_ref()
            .expect("惰性创建后必存在")
            .send(FedBroadcastJob {
                msg,
                due_at: tokio::time::Instant::now() + delay,
            });
    }

    /// 身份冲突检测（联邦消息层，2026-08-23；**仅本地提示，不拦截消息**）。
    ///
    /// 入站联邦消息的发送节点 `from`（连接握手签名验证过的 NodeID）若等于
    /// 本机 NodeID —— 同一私钥的另一节点正在联邦网络发言（身份 = 密钥：
    /// 多个 OS 用同一私钥进入时权限相同、文件不同步，这是设计特性而非攻击；
    /// 本机用户应知情）。处置：
    ///
    /// - P2P 未启用（Handle 未注入）→ 无从比对本机 NodeID，直接返回；
    /// - `from != 本机 NodeID` → 正常联邦消息，直接返回；
    /// - 命中冲突 → `eprintln` 警告 + 本地大厅（lobby）写一条**系统警告消息**
    ///   （sender_id="system" / sender_kind="system"；系统消息不参与联邦
    ///   广播，不会回灌对端）；同一冲突源 [`IDENTITY_WARN_DEDUPE`]（5 分钟）
    ///   内只提示一次（内存 HashMap 防刷屏）；
    /// - 调用方（`FederationBridge::dispatch`）检测后**照常 ingest 原消息**。
    pub fn warn_if_identity_conflict(&self, from: &os_p2p::NodeId, node: &str) {
        let Some((handle, _)) = self
            .shared
            .fed_p2p
            .lock()
            .expect("fed_p2p poisoned")
            .clone()
        else {
            return; // P2P 未启用：没有本机 NodeID 可比对
        };
        if from != handle.self_id() {
            return; // 不同 NodeID：正常联邦消息
        }
        // 去重：同一冲突源 5 分钟内只提示一次
        let key = from.to_hex();
        {
            let mut last = self
                .shared
                .identity_warn_last
                .lock()
                .expect("identity_warn_last poisoned");
            if last
                .get(&key)
                .is_some_and(|t| t.elapsed() < IDENTITY_WARN_DEDUPE)
            {
                return;
            }
            last.insert(key, Instant::now());
        }
        // 缩略 NodeID（本层无 socket 地址，以发送者公钥缩写作地址标识）
        let full = from.to_hex();
        let short = format!("{}…{}", &full[..10], &full[full.len() - 6..]);
        eprintln!("[fed][WARN] 身份冲突：相同公钥从另一节点发言（{node}/{short}）");
        let msg = Message {
            id: new_uuid(),
            conversation_id: LOBBY_ID.to_string(),
            sender_id: "system".to_string(),
            sender_name: Some("NexOS".to_string()),
            content: format!(
                "⚠️ 身份冲突警告：检测到相同公钥从另一节点发言（{node}/{short}）。\
多个 OS 使用同一私钥时权限共享，请确认是否为本人操作。"
            ),
            msg_type: "system".to_string(),
            file_url: None,
            reply_to: None,
            created_at: now_iso(),
            read_by: Vec::new(),
            sender_kind: "system".to_string(),
            mentions: Vec::new(),
            attachment: None,
        };
        {
            let conn = self.shared.db.lock().expect("db poisoned");
            if insert_message(&conn, &msg).is_err() {
                return; // 写失败仅影响提示面（尽力而为），不影响原消息处理
            }
        }
        ImRouteHandler::broadcast_lobby(&self.shared.ws_hub, &msg);
    }

    /// 接收端：解析联邦载荷 → 去重 → 写本地 im_messages + WS 广播。
    ///
    /// 载荷契约 `{"fed":"im_fed_lobby_message"|"im_lobby","node":<来源节点>,
    /// "message":{Message}}`（新 kind 为现行 fed-lobby 发言广播；旧 kind 为
    /// 旧版节点的 im_lobby 广播——兼容接收，同样落联邦大厅）：
    /// - 联邦接收开关已关（[`ImFederation::fed_enabled`] == false）→ 入口
    ///   短路 `Paused`（不解析、不写库、不广播——远程消息在该期间丢弃，
    ///   与"暂停接收"语义一致）；
    /// - 非本类载荷/缺 node/message 解析失败/id 空 → `Ignored`；
    /// - 远端 agent/系统消息（不可联邦）不落地 → `Ignored`；
    /// - `conversation_id` 强制归位 [`FED_LOBBY_ID`]（联邦消息恒落联邦大厅，
    ///   与我的大厅完全隔离——防伪造会话注入）；
    /// - `sender_id` 改写 `fed:<node>:<原值>`（来源标识 + 身份空间隔离）、
    ///   `sender_name` 加 🌐 来源标注（`🌐 <名>（<节点>）`）；
    /// - 去重：消息 id 内存缓存（1000 条）+ DB `find_message` 双重判定，
    ///   已存在 → `Duplicate`（不重写、不广播）；
    /// - 写入后走 [`ImRouteHandler::broadcast_fed_lobby`] WS 通道
    ///   （`im_fed_lobby_message` 帧）。
    pub fn ingest(&self, payload: &serde_json::Value) -> ImFedIngest {
        if !self.fed_enabled() {
            eprintln!("[fed] ingest skipped（联邦接收已暂停）");
            return ImFedIngest::Paused;
        }
        let kind = payload.get("fed").and_then(|v| v.as_str());
        if kind != Some(FED_KIND_IM_FED_LOBBY) && kind != Some(FED_KIND_IM_LOBBY) {
            return ImFedIngest::Ignored;
        }
        let Some(node) = payload
            .get("node")
            .and_then(|v| v.as_str())
            .and_then(sanitize_fed_node_im)
        else {
            return ImFedIngest::Ignored;
        };
        let Some(msg_val) = payload.get("message") else {
            return ImFedIngest::Ignored;
        };
        let Ok(mut msg) = serde_json::from_value::<Message>(msg_val.clone()) else {
            return ImFedIngest::Ignored;
        };
        if msg.id.trim().is_empty() || !lobby_message_federable(&msg) {
            return ImFedIngest::Ignored;
        }
        // 归位联邦大厅 + 来源改写（远程联邦消息恒落 fed-lobby，与我的大厅
        // 完全隔离；sender_id 前缀隔离身份空间 + sender_name 🌐 来源标注）
        msg.conversation_id = FED_LOBBY_ID.to_string();
        msg.sender_id = format!("{FED_SENDER_PREFIX}{node}:{}", msg.sender_id);
        msg.sender_name = Some(match msg.sender_name {
            Some(n) => format!("🌐 {n}（{node}）"),
            None => format!("🌐 {node}"),
        });
        {
            let mut seen = self.shared.fed_seen.lock().expect("fed_seen poisoned");
            if seen.contains(&msg.id) {
                return ImFedIngest::Duplicate;
            }
            seen.push_back(msg.id.clone());
            while seen.len() > FED_SEEN_LIMIT {
                seen.pop_front();
            }
        }
        {
            let conn = self.shared.db.lock().expect("db poisoned");
            if find_message(&conn, &msg.id).unwrap_or(None).is_some() {
                return ImFedIngest::Duplicate; // 重启后缓存为空——DB 兜底
            }
            if insert_message(&conn, &msg).is_err() {
                return ImFedIngest::Ignored; // 写失败按忽略处理（联邦尽力而为）
            }
        }
        ImRouteHandler::broadcast_fed_lobby(&self.shared.ws_hub, &msg);
        ImFedIngest::Written
    }
}
