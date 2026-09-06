//! `LiveRouteHandler` —— 「直播」（流媒体中心的直播 Tab）REST + WS 适配器。
//!
//! 定位：浏览器采集（getUserMedia / getDisplayMedia + MediaRecorder）→ WS 上行
//! 二进制 webm chunk → 服务端**内存扇出** → 观众 WS 下行 → MSE（MediaSource +
//! SourceBuffer）播放。纯 Web 技术栈，零原生依赖（不引 ffmpeg/WebRTC/mediaserver）。
//!
//! # 联邦（本地大厅 + 联邦大厅 + 跨节点中继，见下方「联邦协议」节）
//!
//! - 房间创建/结束/变更经 os-p2p overlay 广播 `live_lobby` 宣告，各节点按
//!   `fed_room_id`（`<节点短前缀>:<room_id>`）幂等合并进联邦房间表（内存 +
//!   TTL 90s 无刷新剔除——房间是短暂状态，不同于 NexHub 条目持久）；
//! - 观众节点观看远端房间：向源节点定向发 `live_relay_sub` → 源节点把 publish
//!   帧流分块（`live_relay_frame`，base64，1 MiB/块——沿 transfer.rs 分块先例）
//!   定向回传 → 本节点 LiveHub 注入**影子房间**扇出（viewer WS 路径完全复用，
//!   前端无感知差异）；退订 `live_relay_unsub` / 房间结束 / 心跳超时即停。
//!
//! # 真实状态（无 DB 无 seed 无演示房间）
//!
//! 房间表是进程内 `Mutex<HashMap<room_id, RoomState>>`：**重启即清空**，无 SQLite、
//! 无预置演示数据。`viewer_count` / `bytes_in` / `bytes_out` / `dropped_frames`
//! / `rejected_frames` 全部为真实计数（连接增减、字节累加、丢帧累加），不伪造。
//!
//! # 路由表（REST 3 条 + WS 2 条）
//!
//! | method | path                       | 动作 |
//! |--------|----------------------------|------|
//! | POST   | `/api/v1/live/rooms`       | 创建房间（admin；返回 room + publish token）|
//! | GET    | `/api/v1/live/rooms`       | 房间两段式大厅 `{local:[...], federated:[...]}`（公开读）|
//! | DELETE | `/api/v1/live/rooms/:id`   | 结束直播（admin；踢断全部连接，房间出表）|
//! | WS     | `/ws/live/:id/publish?token=` | 主播上行（二进制 webm chunk + 文本控制帧）|
//! | WS     | `/ws/live/:id/view`        | 观众下行（连上即重放 header，再实时转发）|
//!
//! WS 挂载见 `http.rs::build_router`（`/ws/live/{room_id}/{action}`），鉴权模式
//! 同终端 WS（升级前校验拒绝，客户端拿 HTTP 状态码而非 WS 空转）：
//! - publish：`?token=` 必须与创建房间时返回的 publish token 精确一致（401），
//!   房间不存在 404；同房间新主播接入顶号（旧连接被踢断）。
//! - view：公开（房间不存在 404）。
//!
//! # 扇出与背压
//!
//! - 主播首个二进制 chunk 缓存为 header（webm init segment），中途加入的观众
//!   连上即先收 header 再收实时帧（MSE 顺序 append 的前提）。
//! - 每观众一条有界 mpsc（容量 [`VIEWER_CHANNEL_CAPACITY`]）；满时 `try_send`
//!   失败即丢帧（保实时，慢消费者不阻塞主播），记 `dropped_frames`。
//! - 上行帧 > [`max_frame_bytes`] 拒收（不扇出不计 bytes_in，记 `rejected_frames`，
//!   回文本错误帧提示主播）。
//! - 每房间订阅上限 [`max_viewers_per_room`]（超出拒绝接入，记日志）。
//! - 主播断开（WS 关闭 / `{"kind":"stop"}` 控制帧 / 被顶号）：status → ended，
//!   通知观众 ended 帧；房间无主播且观众清零即回收出表。
//!
//! # env（均可选，缺省即下方默认值；非法值回默认并告警）
//!
//! - `NEXOS_LIVE_MAX_FRAME_BYTES`：单帧上限（字节，默认 2 MiB）
//! - `NEXOS_LIVE_MAX_VIEWERS`：每房间观众上限（默认 200）
//!
//! 契约细节与环境变量说明见 docs/LIVE_STREAMING.md。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::extract::ws::{Message as AxumWsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// 常量与 env 覆盖
// ----------------------------------------------------------------------------

/// 单个上行帧默认上限（2 MiB）——MediaRecorder timeslice 1s 的 webm chunk
/// 远小于此，超过即为异常客户端，拒收防内存打爆。
pub const DEFAULT_MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;

/// 每房间默认观众（订阅端）上限。
pub const DEFAULT_MAX_VIEWERS: usize = 200;

/// 每观众下行有界通道容量（帧数）——满即丢帧保实时。
const VIEWER_CHANNEL_CAPACITY: usize = 64;

// ----------------------------------------------------------------------------
// 联邦常量（本地大厅 + 联邦大厅 + 跨节点中继）
// ----------------------------------------------------------------------------

/// 联邦房间表 TTL：宣告后该时长无刷新即剔除（房间是短暂状态，不同于
/// NexHub 大厅条目的持久语义）。源节点每 [`FED_ANNOUNCE_INTERVAL`] 重宣告，
/// 90s 容忍一次丢失。
pub const FED_ROOM_TTL: Duration = Duration::from_secs(90);

/// 宣告/巡检周期（< TTL 的一半）：重广播本地房间（刷新各节点 TTL +
/// viewer_count 漂移）+ 剔除过期联邦条目 + 剪除心跳超时的中继订阅。
pub const FED_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(30);

/// 观端心跳周期：影子房间存在期间重发 `live_relay_sub` 刷新源端 last_seen。
const RELAY_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// 源端中继订阅心跳超时（> 该时长未刷新即剪除，停中继）。
pub const RELAY_SUB_TIMEOUT: Duration = Duration::from_secs(90);

/// 中继分块大小（1 MiB，沿 os-p2p transfer.rs 的裁决：overlay 单帧上限
/// 4 MiB，1 MiB 原始块经 base64 ≈ 1.4 MiB 留足信封余量）。
pub const RELAY_CHUNK_BYTES: usize = 1024 * 1024;

/// 每远端订阅的中继帧通道容量（满即丢帧保实时——与本地观众同语义）。
const RELAY_CHANNEL_CAPACITY: usize = 64;

/// 观端分块重组待完成 seq 上限（防病态源把重组缓冲撑爆）。
const RELAY_MAX_PENDING_SEQS: usize = 8;

/// 从 env 读正整数（缺省/非法回 `default` 并告警，日志带 [live] 前缀）。
fn env_usize(key: &str, default: usize) -> usize {
    match std::env::var(key) {
        Ok(v) => match v.trim().parse::<usize>() {
            Ok(n) if n > 0 => n,
            _ => {
                eprintln!("[live] env {key} 非法（{v}），回默认 {default}");
                default
            }
        },
        Err(_) => default,
    }
}

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 直播房间（对外视图；列表/创建响应共用，字段全为真实计数）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveRoom {
    pub id: String,
    pub title: String,
    /// `screen` / `camera`。
    pub source_kind: String,
    pub created_at: String,
    /// 创建者身份（admin Principal 的用户名；匿名默认注入态记 `local-admin`）。
    pub publisher_identity: String,
    /// 真实连接数（观众 WS 订阅端计数，连上 +1 / 断开 -1）。
    pub viewer_count: usize,
    /// `live` / `ended`。
    pub status: String,
    /// 主播上行累计字节。
    pub bytes_in: u64,
    /// 观众下行累计字节（只计成功投递）。
    pub bytes_out: u64,
    /// 慢消费者丢帧数（下行通道满，try_send 失败即丢）。
    pub dropped_frames: u64,
    /// 上行超限拒收帧数（> max_frame_bytes）。
    pub rejected_frames: u64,
    /// 主播 WS 是否在线（真实连接态，非 status 派生）。
    pub publisher_online: bool,
    /// 是否已缓存 init segment（观众中途加入可重放）。
    pub header_cached: bool,
}

/// 创建房间请求体（POST /api/v1/live/rooms）。
#[derive(Debug, Clone, Deserialize)]
pub struct CreateRoomBody {
    pub title: String,
    pub source_kind: String,
}

/// 联邦房间条目（联邦大厅列表元素）：远端节点 `live_lobby` 宣告按
/// `id`（fed_room_id）幂等合并的产物。与 [`LiveRoom`] 分离——联邦条目只有
/// 宣告携带的展示字段 + 来源节点，无本节点真实计数（字节/丢帧在源节点）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederatedLiveRoom {
    /// 联邦形态房间 id：`<源节点短前缀>:<本地 room_id>`（防跨节点撞名）。
    pub id: String,
    pub title: String,
    /// `screen` / `camera`。
    pub source_kind: String,
    /// `live` / `ended`（ended 即出表，不等 TTL）。
    pub status: String,
    /// 源节点 NodeID（`0x` + 66 hex，中继订阅的定向目标）。
    pub node_id: String,
    /// 源节点名（展示用）。
    pub node_name: String,
    /// 源节点本地观众数（宣告快照；中继观众计入各自节点影子房间）。
    pub viewer_count: usize,
    /// 源节点主播是否在线（宣告快照）。
    pub publisher_online: bool,
    /// 宣告时间（源节点时钟的 ISO 串）。
    pub updated_at: String,
}

// ----------------------------------------------------------------------------
// 联邦协议（fed kind × 载荷；发送端纯函数构造，接收端与测试共用）
// ----------------------------------------------------------------------------
//
// | kind                | 方向       | 载荷要素 | 语义 |
// |---------------------|-----------|----------|------|
// | `live_lobby`        | 广播       | node/node_id/room{room_id,fed_room_id,title,source_kind,status,viewer_count,publisher_online,updated_at} | 房间宣告：按 fed_room_id 幂等合并进联邦表，TTL 90s；status=ended 即出表 |
// | `live_relay_sub`    | 观众→源    | node/node_id/room_id(fed 形态) | 中继订阅/心跳：源端为该节点建中继订阅（新订阅先重放 header），30s 重发刷新 |
// | `live_relay_frame`  | 源→观众    | node/room_id(fed)/seq/ci/cn/bytes(base64)/ended | 中继帧：帧按 RELAY_CHUNK_BYTES 分块（ci/cn），ended=true 收尾 |
// | `live_relay_unsub`  | 观众→源    | node/room_id(fed) | 退订：源端剪除中继订阅即停帧 |

/// 联邦载荷类型标记：房间宣告（广播）。
pub const FED_KIND_LIVE_LOBBY: &str = "live_lobby";
/// 联邦载荷类型标记：中继订阅/心跳（观众节点 → 源节点，定向）。
pub const FED_KIND_LIVE_RELAY_SUB: &str = "live_relay_sub";
/// 联邦载荷类型标记：中继帧（源节点 → 观众节点，定向，分块 base64）。
pub const FED_KIND_LIVE_RELAY_FRAME: &str = "live_relay_frame";
/// 联邦载荷类型标记：中继退订（观众节点 → 源节点，定向）。
pub const FED_KIND_LIVE_RELAY_UNSUB: &str = "live_relay_unsub";

/// fed_room_id（纯函数）：`<源节点短前缀 8 hex>:<本地 room_id>`。
/// 短前缀取 NodeID hex（`0x` + 66）去掉 `0x` 后前 8 字符——跨节点防撞。
#[must_use]
pub fn fed_room_id(node_hex: &str, room_id: &str) -> String {
    let short: String = node_hex
        .trim_start_matches("0x")
        .chars()
        .take(8)
        .collect();
    format!("{short}:{room_id}")
}

/// 从 fed_room_id 还原本节点房间 id（纯函数）：前缀匹配本节点才返回 Some
/// （源端收到中继订阅时判定「这是我的房间」）。
#[must_use]
pub fn local_room_id(node_hex: &str, fed_id: &str) -> Option<String> {
    let prefix = format!(
        "{}:",
        node_hex.trim_start_matches("0x").chars().take(8).collect::<String>()
    );
    fed_id.strip_prefix(&prefix).map(str::to_string)
}

/// 节点名净化：空/超长（>64 字符）回退 "peer"（与 im.rs 同款限幅）。
#[must_use]
fn sanitize_fed_node_live(node: &str) -> String {
    let n = node.trim();
    if n.is_empty() || n.chars().count() > 64 {
        "peer".to_string()
    } else {
        n.to_string()
    }
}

/// 构造房间宣告载荷（纯函数，发送端与测试共用）。
#[must_use]
pub fn build_live_lobby_payload(node_hex: &str, node_name: &str, room: &LiveRoom) -> serde_json::Value {
    serde_json::json!({
        "fed": FED_KIND_LIVE_LOBBY,
        "node": sanitize_fed_node_live(node_name),
        "node_id": node_hex,
        "room": {
            "room_id": room.id,
            "fed_room_id": fed_room_id(node_hex, &room.id),
            "title": room.title,
            "source_kind": room.source_kind,
            "status": room.status,
            "viewer_count": room.viewer_count,
            "publisher_online": room.publisher_online,
            "updated_at": now_iso(),
        },
    })
}

/// 构造中继订阅/心跳载荷（纯函数）。
#[must_use]
pub fn build_relay_sub_payload(node_hex: &str, node_name: &str, fed_id: &str) -> serde_json::Value {
    serde_json::json!({
        "fed": FED_KIND_LIVE_RELAY_SUB,
        "node": sanitize_fed_node_live(node_name),
        "node_id": node_hex,
        "room_id": fed_id,
    })
}

/// 构造中继退订载荷（纯函数）。
#[must_use]
pub fn build_relay_unsub_payload(node_hex: &str, node_name: &str, fed_id: &str) -> serde_json::Value {
    serde_json::json!({
        "fed": FED_KIND_LIVE_RELAY_UNSUB,
        "node": sanitize_fed_node_live(node_name),
        "node_id": node_hex,
        "room_id": fed_id,
    })
}

/// 构造中继帧载荷（纯函数；`ended=true` 为收尾控制帧，无 bytes）。
#[must_use]
pub fn build_relay_frame_payload(
    node_name: &str,
    fed_id: &str,
    seq: u64,
    chunk_index: u32,
    chunk_count: u32,
    bytes: &[u8],
    ended: bool,
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "fed": FED_KIND_LIVE_RELAY_FRAME,
        "node": sanitize_fed_node_live(node_name),
        "room_id": fed_id,
        "seq": seq,
        "ci": chunk_index,
        "cn": chunk_count,
        "ended": ended,
    });
    if !ended {
        payload["bytes"] = serde_json::Value::String(base64::engine::general_purpose::STANDARD.encode(bytes));
    }
    payload
}

// ----------------------------------------------------------------------------
// 扇出引擎（LiveHub）：房间表 + 订阅端注册表，纯内存
// ----------------------------------------------------------------------------

/// 观众下行事件：媒体帧（共享只读）或房间结束通知。
#[derive(Debug, PartialEq)]
enum ViewerEvent {
    /// 媒体帧（header 或后续 chunk，Arc 共享零拷贝扇出）。
    Media(Arc<Vec<u8>>),
    /// 房间结束（主播断开 / DELETE /rooms/:id），观众收尾后关连接。
    Ended,
}

/// 主播帧事件探针（hub → 联邦端点消费者任务）：主播上行帧与房间终止
/// 经无界通道转给 [`LiveFedEndpoint`]，由它分块中继给远端订阅节点。
/// 只有**本节点主播**的 ingest 会 tap——中继注入（relay_inject）不 tap，
/// 杜绝 A→B→A 的中继回环。
#[derive(Debug)]
enum LiveFrameTap {
    /// 主播上行帧（含首个 chunk = header）。
    Ingest { room_id: String, frame: Arc<Vec<u8>> },
    /// 房间终止（主播断开 / DELETE / 回收）——中继订阅收尾。
    Ended { room_id: String },
}

/// 房间运行态（对外视图 + 扇出内部状态）。
struct RoomState {
    room: LiveRoom,
    /// 主播上行 token（创建时签发；WS publish 握手精确匹配）。
    publish_token: String,
    /// 缓存的 init segment（主播首个 chunk；新观众连上即重放）。
    header: Option<Arc<Vec<u8>>>,
    /// 主播代际（同房间重连/顶号时递增；detach 只清自己的代）。
    publisher_gen: u64,
    /// 主播踢线信号通道（DROP 即断：DELETE / 被顶号 / 房间回收）。
    publisher_kick: Option<tokio::sync::mpsc::Sender<()>>,
    /// 订阅端注册表：viewer_id → 下行通道（viewer_count 即 len）。
    subscribers: HashMap<u64, tokio::sync::mpsc::Sender<ViewerEvent>>,
    /// 联邦影子房间（远端房间的本地扇出壳）：不出现在本地列表、不可 publish，
    /// 本地观众清零即出表并通知源节点退订中继。
    remote: bool,
}

/// 直播枢纽：全部房间状态（进程内单例供 REST handler 与 WS 升基层共享）。
pub struct LiveHub {
    rooms: Mutex<HashMap<String, RoomState>>,
    next_room: AtomicU64,
    next_viewer_id: AtomicU64,
    /// 单帧上限（env NEXOS_LIVE_MAX_FRAME_BYTES 覆盖）。
    max_frame_bytes: usize,
    /// 每房间订阅上限（env NEXOS_LIVE_MAX_VIEWERS 覆盖）。
    max_viewers: usize,
    /// 联邦端点钩子（set_p2p 装配时回填）：影子房间创建 / 宣告 / 中继退订。
    fed: Mutex<Option<LiveFedEndpoint>>,
    /// 主播帧事件探针（联邦端点装配时回填）。
    frame_tap: Mutex<Option<tokio::sync::mpsc::UnboundedSender<LiveFrameTap>>>,
}

/// 进程级共享实例（REST 经 [`LiveRouteHandler::new`]，WS 经
/// `http.rs` 挂载的 `live_ws_handler`——同一实例才能同源读写）。
static SHARED_HUB: Lazy<Arc<LiveHub>> = Lazy::new(|| {
    Arc::new(LiveHub {
        rooms: Mutex::new(HashMap::new()),
        next_room: AtomicU64::new(1),
        next_viewer_id: AtomicU64::new(1),
        max_frame_bytes: env_usize("NEXOS_LIVE_MAX_FRAME_BYTES", DEFAULT_MAX_FRAME_BYTES),
        max_viewers: env_usize("NEXOS_LIVE_MAX_VIEWERS", DEFAULT_MAX_VIEWERS),
        fed: Mutex::new(None),
        frame_tap: Mutex::new(None),
    })
});

/// 主播接入失败原因（升级前转 HTTP 状态码）。
#[derive(Debug, PartialEq, Eq)]
enum AttachError {
    RoomGone,
    BadToken,
}

/// ingest 结果（主播 WS 循环据此决定是否断开 / 回错误帧）。
#[derive(Debug, PartialEq, Eq)]
enum IngestOutcome {
    /// 正常扇出：n = 成功投递的订阅端数。
    Delivered { delivered: usize },
    /// 房间不存在（已被 DELETE / 回收）——主播应断开。
    RoomGone,
    /// 帧超限拒收（不扇出不计 bytes_in）。
    Oversized { size: usize, limit: usize },
}

/// subscribe 失败原因（升级前转 HTTP 状态码）。
#[derive(Debug, PartialEq, Eq)]
enum SubscribeError {
    RoomGone,
    Full,
}

impl LiveHub {
    /// 独立实例（测试隔离用；生产走 [`LiveHub::shared`]）。
    pub fn new() -> Self {
        Self {
            rooms: Mutex::new(HashMap::new()),
            next_room: AtomicU64::new(1),
            next_viewer_id: AtomicU64::new(1),
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            max_viewers: DEFAULT_MAX_VIEWERS,
            fed: Mutex::new(None),
            frame_tap: Mutex::new(None),
        }
    }

    /// 进程级共享实例（REST handler 与 WS 升基层同一状态源）。
    pub fn shared() -> Arc<LiveHub> {
        SHARED_HUB.clone()
    }

    /// 创建房间：返回（对外视图, publish token）。状态 `live`（等待主播接入）。
    fn create_room(
        &self,
        title: String,
        source_kind: String,
        publisher_identity: String,
    ) -> (LiveRoom, String) {
        let n = self.next_room.fetch_add(1, Ordering::Relaxed);
        let id = format!("live-{n}");
        let token = mint_publish_token(&id, n);
        let room = LiveRoom {
            id: id.clone(),
            title,
            source_kind,
            created_at: now_iso(),
            publisher_identity,
            viewer_count: 0,
            status: "live".into(),
            bytes_in: 0,
            bytes_out: 0,
            dropped_frames: 0,
            rejected_frames: 0,
            publisher_online: false,
            header_cached: false,
        };
        let view = room.clone();
        let state = RoomState {
            room,
            publish_token: token.clone(),
            header: None,
            publisher_gen: 0,
            publisher_kick: None,
            subscribers: HashMap::new(),
            remote: false,
        };
        self.rooms
            .lock()
            .expect("live: rooms lock")
            .insert(id.clone(), state);
        eprintln!("[live] 创建房间 id={id} title={}", view.title);
        (view, token)
    }

    /// 房间列表（按创建序；视图字段全为实时真实值）。联邦影子房间
    /// （remote）不在此列——远端房间见联邦大厅（fed 表）。
    fn list_rooms(&self) -> Vec<LiveRoom> {
        let rooms = self.rooms.lock().expect("live: rooms lock");
        let mut list: Vec<LiveRoom> = rooms
            .values()
            .filter(|s| !s.remote)
            .map(|s| s.room.clone())
            .collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    /// 单房间视图快照（联邦宣告用；影子房间/不存在返回 None）。
    fn room_view(&self, id: &str) -> Option<LiveRoom> {
        self.rooms
            .lock()
            .expect("live: rooms lock")
            .get(id)
            .filter(|s| !s.remote)
            .map(|s| s.room.clone())
    }

    /// 结束房间（DELETE /rooms/:id）：status → ended、踢断主播与全部观众、出表。
    /// 返回结束时刻的视图快照；房间不存在返回 None。
    fn end_room(&self, id: &str) -> Option<LiveRoom> {
        let mut rooms = self.rooms.lock().expect("live: rooms lock");
        let mut state = rooms.remove(id)?;
        state.room.status = "ended".into();
        let was_local = !state.remote;
        // 踢主播（drop 信号通道 → 主播循环 recv None 即断）+ 全部观众收尾帧。
        state.publisher_kick = None;
        let mut kicked = 0usize;
        for (_, tx) in state.subscribers.drain() {
            let _ = tx.try_send(ViewerEvent::Ended);
            kicked += 1;
        }
        state.room.viewer_count = 0;
        eprintln!("[live] 结束房间 id={id} 踢断观众 {kicked} 人");
        // 本地房间终止：通知联邦端点停中继（影子房间结束无中继可停）。
        if was_local {
            self.tap_ended(id);
        }
        Some(state.room)
    }

    /// 主播接入（WS publish 升级前校验）：token 精确匹配才放行；返回代际 +
    /// 踢线信号接收端。同房间已有主播时顶号（旧 kick 通道 drop → 旧连接自断）。
    /// 影子房间（远端房间的本地扇出壳）拒绝本地 publish。
    fn attach_publisher(
        &self,
        id: &str,
        token: &str,
    ) -> Result<(u64, tokio::sync::mpsc::Receiver<()>), AttachError> {
        let mut rooms = self.rooms.lock().expect("live: rooms lock");
        let state = rooms.get_mut(id).ok_or(AttachError::RoomGone)?;
        if state.remote {
            return Err(AttachError::RoomGone); // 远端房间只能经中继观看，不可本地开播
        }
        if state.publish_token != token {
            return Err(AttachError::BadToken);
        }
        // 顶号：drop 旧信号通道（旧主播循环 recv None 自断），换新代。
        state.publisher_gen += 1;
        state.publisher_kick = None;
        let (kick_tx, kick_rx) = tokio::sync::mpsc::channel::<()>(1);
        state.publisher_kick = Some(kick_tx);
        // 新流 = 新 init segment：清旧 header，status 回 live。
        state.header = None;
        state.room.status = "live".into();
        state.room.publisher_online = true;
        state.room.header_cached = false;
        let gen = state.publisher_gen;
        let view = state.room.clone();
        drop(rooms);
        eprintln!("[live] 主播接入 room={id} gen={gen}");
        // 主播上线 → 联邦宣告（status=live；锁外发送防死锁）。
        if let Some(fed) = self.fed_endpoint() {
            fed.announce_room(&view);
        }
        Ok((gen, kick_rx))
    }

    /// 主播断开（WS 循环退出后）：只清自己代际的信号通道，status → ended，
    /// 通知观众 ended；观众清零则房间回收出表。
    fn detach_publisher(&self, id: &str, gen: u64) {
        let snapshot = {
            let mut rooms = self.rooms.lock().expect("live: rooms lock");
            let Some(state) = rooms.get_mut(id) else {
                return;
            };
            if state.publisher_gen != gen {
                return; // 已被新主播顶号，不动新主播的状态
            }
            state.publisher_kick = None;
            state.room.publisher_online = false;
            state.room.status = "ended".into();
            state.header = None;
            state.room.header_cached = false;
            for tx in state.subscribers.values() {
                let _ = tx.try_send(ViewerEvent::Ended);
            }
            eprintln!("[live] 主播断开 room={id}");
            let snapshot = state.room.clone();
            if state.subscribers.is_empty() {
                rooms.remove(id);
                eprintln!("[live] 回收房间 room={id}（无主播且观众清零）");
            }
            snapshot
        };
        // 主播下播 → 停中继 + 联邦宣告 ended（锁外防死锁）。
        self.tap_ended(id);
        if let Some(fed) = self.fed_endpoint() {
            fed.announce_room(&snapshot);
        }
    }

    /// 主播上行帧入口：超限拒收；首个 chunk 缓存 header；扇出到全部订阅端。
    fn ingest(&self, id: &str, frame: Vec<u8>) -> IngestOutcome {
        let frame = if frame.len() > self.max_frame_bytes {
            let mut rooms = self.rooms.lock().expect("live: rooms lock");
            if let Some(state) = rooms.get_mut(id) {
                state.room.rejected_frames += 1;
            }
            eprintln!(
                "[live] 拒收超限帧 room={id} size={} limit={}",
                frame.len(),
                self.max_frame_bytes
            );
            return IngestOutcome::Oversized {
                size: frame.len(),
                limit: self.max_frame_bytes,
            };
        } else {
            frame
        };
        let frame = Arc::new(frame);
        let mut rooms = self.rooms.lock().expect("live: rooms lock");
        let Some(state) = rooms.get_mut(id) else {
            return IngestOutcome::RoomGone;
        };
        let delivered = fanout_locked(state, id, &frame);
        // 主播帧 → 联邦探针（中继给远端订阅节点；锁内无界 send 不阻塞）。
        if let Some(tap) = self
            .frame_tap
            .lock()
            .expect("live: tap lock")
            .as_ref()
            .cloned()
        {
            let _ = tap.send(LiveFrameTap::Ingest {
                room_id: id.to_string(),
                frame,
            });
        }
        IngestOutcome::Delivered { delivered }
    }

    /// 中继帧注入（观端：远端房间的帧进入本地影子房间扇出）。与 ingest 的
    /// 差异：不 tap（防中继回环）、只进影子房间；帧尺寸上限沿用源端同款
    /// 校验（重组超限的病态载荷丢弃并计数）。
    fn relay_inject(&self, fed_id: &str, frame: Vec<u8>) -> IngestOutcome {
        if frame.len() > self.max_frame_bytes {
            let mut rooms = self.rooms.lock().expect("live: rooms lock");
            if let Some(state) = rooms.get_mut(fed_id) {
                state.room.rejected_frames += 1;
            }
            eprintln!(
                "[live-fed] 重组帧超限丢弃 room={fed_id} size={} limit={}",
                frame.len(),
                self.max_frame_bytes
            );
            return IngestOutcome::Oversized {
                size: frame.len(),
                limit: self.max_frame_bytes,
            };
        }
        let frame = Arc::new(frame);
        let mut rooms = self.rooms.lock().expect("live: rooms lock");
        let Some(state) = rooms.get_mut(fed_id) else {
            return IngestOutcome::RoomGone;
        };
        if !state.remote {
            return IngestOutcome::RoomGone; // 中继只进影子房间
        }
        let delivered = fanout_locked(state, fed_id, &frame);
        IngestOutcome::Delivered { delivered }
    }

    /// 观众接入（WS view 升级前校验）：注册订阅端；若已缓存 header 则先入队
    /// （保证观众先收 init segment 再收实时帧，MSE 顺序 append 前提）。
    /// 本地房间未命中时回退联邦表：是已知远端房间 → 建影子房间 + 发起中继
    /// （观众 WS 路径对本地/远端房间无感知差异）。
    fn subscribe(
        &self,
        id: &str,
    ) -> Result<(u64, tokio::sync::mpsc::Receiver<ViewerEvent>), SubscribeError> {
        // 快路径：本地房间（含已存在的影子房间）。
        {
            let mut rooms = self.rooms.lock().expect("live: rooms lock");
            if rooms.contains_key(id) {
                return self.subscribe_locked(&mut rooms, id);
            }
        }
        // 慢路径：联邦影子房间（锁外发起——ensure_shadow_room 会再拿 rooms 锁）。
        if let Some(fed) = self.fed_endpoint() {
            if fed.ensure_shadow_room(id) {
                let mut rooms = self.rooms.lock().expect("live: rooms lock");
                if rooms.contains_key(id) {
                    return self.subscribe_locked(&mut rooms, id);
                }
            }
        }
        Err(SubscribeError::RoomGone)
    }

    /// 观众断开：移除订阅端（真实减 viewer_count）；房间无主播且观众清零则回收。
    /// 影子房间观众清零 → 出表 + 通知源节点退订中继（停帧）。房间已出表
    /// （中继 ended 收尾先行）也补发退订——幂等，源端可能已剪除。
    fn unsubscribe(&self, id: &str, viewer_id: u64) {
        let stop_relay = {
            let mut rooms = self.rooms.lock().expect("live: rooms lock");
            match rooms.get_mut(id) {
                // 房间已不在表（中继 ended 收尾先行）：仍补发退订（幂等）——
                // 本地房间 id 在联邦表查不到 → no-op。
                None => true,
                Some(state) => {
                    state.subscribers.remove(&viewer_id);
                    state.room.viewer_count = state.subscribers.len();
                    eprintln!(
                        "[live] 观众离开 room={id} viewer={viewer_id} 剩余={}",
                        state.room.viewer_count
                    );
                    if state.remote && state.subscribers.is_empty() {
                        rooms.remove(id);
                        eprintln!("[live-fed] 影子房间观众清零出表 room={id}");
                        true
                    } else if state.publisher_kick.is_none() && state.subscribers.is_empty() {
                        rooms.remove(id);
                        eprintln!("[live] 回收房间 room={id}（无主播且观众清零）");
                        false
                    } else {
                        false
                    }
                }
            }
        };
        if stop_relay {
            if let Some(fed) = self.fed_endpoint() {
                fed.stop_shadow_relay(id);
            }
        }
    }

    // ------------------------------------------------------------------
    // 联邦钩子（LiveFedEndpoint 装配时回填；生产经 set_p2p，测试注 fake overlay）
    // ------------------------------------------------------------------

    /// 回填联邦端点（install 时调用；重复注入覆盖）。
    fn set_fed(&self, fed: LiveFedEndpoint) {
        *self.fed.lock().expect("live: fed lock") = Some(fed);
    }

    /// 回填帧事件探针（install 时调用）。
    fn set_frame_tap(&self, tap: tokio::sync::mpsc::UnboundedSender<LiveFrameTap>) {
        *self.frame_tap.lock().expect("live: tap lock") = Some(tap);
    }

    /// 联邦端点克隆（未装配返回 None）。
    fn fed_endpoint(&self) -> Option<LiveFedEndpoint> {
        self.fed.lock().expect("live: fed lock").clone()
    }

    /// 房间终止探针（未装配为 no-op）。
    fn tap_ended(&self, id: &str) {
        if let Some(tap) = self
            .frame_tap
            .lock()
            .expect("live: tap lock")
            .as_ref()
            .cloned()
        {
            let _ = tap.send(LiveFrameTap::Ended {
                room_id: id.to_string(),
            });
        }
    }

    /// 创建影子房间（幂等：已存在不动；观端 ensure_shadow_room 调用）。
    fn create_shadow_room(&self, fed_id: &str, entry: &FederatedLiveRoom) {
        let mut rooms = self.rooms.lock().expect("live: rooms lock");
        if rooms.contains_key(fed_id) {
            return;
        }
        let room = LiveRoom {
            id: fed_id.to_string(),
            title: entry.title.clone(),
            source_kind: entry.source_kind.clone(),
            created_at: now_iso(),
            publisher_identity: format!("fed:{}", entry.node_name),
            viewer_count: 0,
            status: "live".into(),
            bytes_in: 0,
            bytes_out: 0,
            dropped_frames: 0,
            rejected_frames: 0,
            publisher_online: entry.publisher_online,
            header_cached: false,
        };
        // publish_token 随机签发且不下发——影子房间不可本地 publish。
        let state = RoomState {
            room,
            publish_token: mint_publish_token(fed_id, 0),
            header: None,
            publisher_gen: 0,
            publisher_kick: None,
            subscribers: HashMap::new(),
            remote: true,
        };
        rooms.insert(fed_id.to_string(), state);
        eprintln!("[live-fed] 建影子房间 id={fed_id} 源节点={}", entry.node_name);
    }

    /// 影子房间是否仍在本表（心跳任务据此自终止）。
    fn shadow_room_alive(&self, fed_id: &str) -> bool {
        self.rooms
            .lock()
            .expect("live: rooms lock")
            .get(fed_id)
            .is_some_and(|s| s.remote)
    }    /// 源房间缓存的 header（新中继订阅先重放 init segment）。
    fn cached_header(&self, id: &str) -> Option<Arc<Vec<u8>>> {
        self.rooms
            .lock()
            .expect("live: rooms lock")
            .get(id)
            .and_then(|s| s.header.clone())
    }

    /// 中继投递计字节（帧 × 成功投递的远端订阅数——真实下行流量）。
    fn add_relay_bytes(&self, id: &str, bytes: u64) {
        if let Some(state) = self
            .rooms
            .lock()
            .expect("live: rooms lock")
            .get_mut(id)
        {
            state.room.bytes_out += bytes;
        }
    }

    /// 锁内订阅注册（subscribe 快/慢路径共用）：viewer 上限校验 + header 先入队。
    fn subscribe_locked(
        &self,
        rooms: &mut HashMap<String, RoomState>,
        id: &str,
    ) -> Result<(u64, tokio::sync::mpsc::Receiver<ViewerEvent>), SubscribeError> {
        let state = rooms.get_mut(id).ok_or(SubscribeError::RoomGone)?;
        if state.subscribers.len() >= self.max_viewers {
            eprintln!(
                "[live] 观众接入被拒 room={id}（订阅上限 {}）",
                self.max_viewers
            );
            return Err(SubscribeError::Full);
        }
        let vid = self.next_viewer_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = tokio::sync::mpsc::channel(VIEWER_CHANNEL_CAPACITY);
        // header 先入队（try_send 刚建队列必成功；失败仅理论上可能，忽略重放）。
        if let Some(h) = state.header.clone() {
            let _ = tx.try_send(ViewerEvent::Media(h));
        }
        state.subscribers.insert(vid, tx);
        state.room.viewer_count = state.subscribers.len();
        eprintln!(
            "[live] 观众接入 room={id} viewer={vid} 总数={}",
            state.room.viewer_count
        );
        Ok((vid, rx))
    }
}

/// 扇出内核（ingest / relay_inject 共用）：首个 chunk 缓存 header、
/// 计数真实投递/丢帧、清死端。返回成功投递的订阅端数。
fn fanout_locked(state: &mut RoomState, id: &str, frame: &Arc<Vec<u8>>) -> usize {
    let len = frame.len() as u64;
    // 首个 chunk = webm init segment（EBML header + track），缓存供中途加入观众重放。
    if state.header.is_none() {
        state.header = Some(frame.clone());
        state.room.header_cached = true;
    }
    state.room.bytes_in += len;
    let mut delivered = 0usize;
    let mut dropped: u64 = 0;
    let mut dead: Vec<u64> = Vec::new();
    for (vid, tx) in state.subscribers.iter() {
        match tx.try_send(ViewerEvent::Media(frame.clone())) {
            Ok(()) => {
                delivered += 1;
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                // 慢消费者：丢帧保实时（主播侧零阻塞），记 dropped。
                dropped += 1;
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => dead.push(*vid),
        }
    }
    for vid in dead {
        state.subscribers.remove(&vid);
    }
    state.room.viewer_count = state.subscribers.len();
    state.room.bytes_out += len * delivered as u64;
    state.room.dropped_frames += dropped;
    if dropped > 0 {
        eprintln!("[live] 慢消费者丢帧 room={id} dropped={dropped}");
    }
    delivered
}

impl Default for LiveHub {
    fn default() -> Self {
        Self::new()
    }
}

// ----------------------------------------------------------------------------
// 联邦端点（LiveFedEndpoint）：大厅宣告 + 跨节点中继（镜像 ImFederation 装配）
// ----------------------------------------------------------------------------

/// 定向发送闭包（观众节点 ↔ 源节点；生产包 os_p2p Handle::send，测试注 fake）。
type FedSendFn = Arc<dyn Fn(&os_p2p::NodeId, serde_json::Value) + Send + Sync>;
/// 广播闭包（全部已连接 peer；生产经 handlers::p2p::fed_broadcast spawn）。
type FedBroadcastFn = Arc<dyn Fn(serde_json::Value) + Send + Sync>;

/// 已装配的 overlay 通道（本节点身份 + 两个发送面）。
struct LiveFedTransport {
    send_to: FedSendFn,
    broadcast: FedBroadcastFn,
    /// 本节点 NodeID（`0x` + 66 hex）。
    node_hex: String,
    node_name: String,
}

/// 联邦房间表条目：宣告快照 + 最后刷新时刻（TTL 剔除依据）。
struct FedRoomEntry {
    room: FederatedLiveRoom,
    last_seen: Instant,
}

/// 源端一条中继订阅（远端节点订阅本节点某房间）：有界帧通道 + 心跳时刻。
#[derive(Clone)]
struct RelaySub {
    /// 远端节点（中继帧定向目标）。
    node: os_p2p::NodeId,
    /// 帧通道（容量 [`RELAY_CHANNEL_CAPACITY`]，满即丢帧保实时）。
    tx: tokio::sync::mpsc::Sender<RelayFrameEvent>,
    last_seen: Instant,
}

/// 中继发送侧事件（源端消费者任务 → 每订阅一条通道）。
enum RelayFrameEvent {
    Frame(Arc<Vec<u8>>),
    Ended,
}

/// 观端分块重组缓冲（一个进行中的 seq）。
struct ChunkParts {
    parts: Vec<Option<Vec<u8>>>,
    received: u32,
    total: usize,
}

/// 直播联邦端点——`Arc<LiveFedInner>` 的薄封装（Clone 共享同一内核）。
///
/// main.rs 装配：`LiveRouteHandler::federation()` 在 Box 进网关**之前**取出，
/// p2p spawn 成功后 `set_p2p` 注入 Handle（未装配则联邦静默停用）；入站载荷经
/// `handlers/p2p.rs` 的 FederationBridge 调 [`LiveFedEndpoint::dispatch`] 分发。
#[derive(Clone)]
pub struct LiveFedEndpoint {
    inner: Arc<LiveFedInner>,
}

struct LiveFedInner {
    hub: Arc<LiveHub>,
    transport: Mutex<Option<LiveFedTransport>>,
    /// 联邦房间表：fed_room_id → 条目（TTL 90s 无刷新剔除，无 seed）。
    fed_rooms: Mutex<HashMap<String, FedRoomEntry>>,
    /// 源端中继订阅表：本地 room_id → 远端节点 hex → 订阅。
    relay_subs: Mutex<HashMap<String, HashMap<String, RelaySub>>>,
    /// 观端分块重组缓冲：fed_room_id → seq → 分块。
    reassembly: Mutex<HashMap<String, HashMap<u64, ChunkParts>>>,
    /// 心跳任务防重（同一影子房间只跑一条心跳）。
    heartbeat_rooms: Mutex<HashMap<String, ()>>,
    /// 心跳周期（默认 [`RELAY_HEARTBEAT_INTERVAL`]；cfg(test) 可按实例缩短，
    /// 测心跳收养/退场语义时快进 tick——避免全局可变静态污染并行测试）。
    heartbeat_interval: Mutex<Duration>,
    /// 装配一次性标记（防 set_p2p 重复装任务）。
    installed: AtomicBool,
}

/// 进程级共享联邦端点（与 SHARED_HUB 绑定同一 hub 实例）。
static SHARED_FED: Lazy<LiveFedEndpoint> =
    Lazy::new(|| LiveFedEndpoint::new(LiveHub::shared()));

/// [`LiveFedEndpoint::merge_announcement`] 的处置结果（观测面，测试/诊断用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FedMerge {
    /// 新条目合并进联邦表。
    Inserted,
    /// 已有条目刷新（幂等合并，字段覆盖，不重复）。
    Refreshed,
    /// status=ended（或 TTL 过期后被 ended 覆盖）：条目立即出表。
    Removed,
    /// 非法载荷 / 自回路 / 非 live_lobby：忽略。
    Ignored,
}

/// [`LiveFedEndpoint::handle_relay_sub`] 的处置结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelaySubResult {
    /// 新订阅建立（header 已入队重放）。
    Subscribed,
    /// 心跳刷新（last_seen 更新）。
    Refreshed,
    /// 房间不存在/已结束——已回 ended 收尾帧。
    RoomGone,
    /// 载荷非法 / 非本节点房间：忽略。
    Ignored,
}

/// [`LiveFedEndpoint::ingest_relay_frame`] 的处置结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayFrameOutcome {
    /// 重组完成并注入本地扇出。
    Injected,
    /// 分块收讫但未齐（等待其余块）。
    Pending,
    /// ended 控制帧：影子房间结束（本地观众收尾）。
    Ended,
    /// 重组超限丢弃。
    Oversized,
    /// 影子房间不存在（尚无本地观众订阅）。
    RoomGone,
    /// 载荷非法：忽略。
    Ignored,
}

impl LiveFedEndpoint {
    /// 独立实例（测试隔离；生产走 [`LiveFedEndpoint::shared`]）。
    fn new(hub: Arc<LiveHub>) -> Self {
        Self {
            inner: Arc::new(LiveFedInner {
                hub,
                transport: Mutex::new(None),
                fed_rooms: Mutex::new(HashMap::new()),
                relay_subs: Mutex::new(HashMap::new()),
                reassembly: Mutex::new(HashMap::new()),
                heartbeat_rooms: Mutex::new(HashMap::new()),
                heartbeat_interval: Mutex::new(RELAY_HEARTBEAT_INTERVAL),
                installed: AtomicBool::new(false),
            }),
        }
    }

    /// 进程级共享实例（与 `http.rs` WS 层用的 SHARED_HUB 同一 hub）。
    pub fn shared() -> Self {
        SHARED_FED.clone()
    }

    /// 注入组网 Handle + 本节点名（main.rs 装配；重复调用忽略——一次性装配）。
    pub fn set_p2p(&self, handle: os_p2p::Handle, node: String) {
        let node_hex = handle.self_id().to_hex();
        let h_direct = handle.clone();
        let send_to: FedSendFn = Arc::new(move |to, payload| h_direct.send(to, payload));
        let h_bcast = handle.clone();
        let broadcast: FedBroadcastFn = Arc::new(move |payload| {
            // fire-and-forget：广播绝不阻塞调用方（与 im 联邦同款语义）
            let h = h_bcast.clone();
            tokio::spawn(async move {
                crate::handlers::p2p::fed_broadcast(&h, payload).await;
            });
        });
        self.install(send_to, broadcast, node_hex, node);
    }

    /// 装配内核（生产 set_p2p / 测试 fake overlay 共用）：写通道 + 回填 hub
    /// 钩子 + 起消费者与巡检任务。
    fn install(&self, send_to: FedSendFn, broadcast: FedBroadcastFn, node_hex: String, node_name: String) {
        if self
            .inner
            .installed
            .swap(true, Ordering::SeqCst)
        {
            eprintln!("[live-fed] 重复装配被忽略（一次性）");
            return;
        }
        let node_name = sanitize_fed_node_live(&node_name);
        *self.inner.transport.lock().expect("live-fed: transport lock") = Some(LiveFedTransport {
            send_to,
            broadcast,
            node_hex,
            node_name,
        });
        let (tap_tx, tap_rx) = tokio::sync::mpsc::unbounded_channel::<LiveFrameTap>();
        self.inner.hub.set_fed(self.clone());
        self.inner.hub.set_frame_tap(tap_tx);
        Self::spawn_fed_tasks(self.inner.clone(), tap_rx);
        eprintln!(
            "[live-fed] 联邦端点已装配 node={} —— live_lobby 宣告 + live_relay_* 中继",
            self.node_name()
        );
    }

    /// 消费者 + 巡检任务：主播帧 → 中继分发给远端订阅；30s 一巡（TTL 剔除/
    /// 重宣告/心跳超时剪除）。
    fn spawn_fed_tasks(inner: Arc<LiveFedInner>, mut tap_rx: tokio::sync::mpsc::UnboundedReceiver<LiveFrameTap>) {
        tokio::spawn(async move {
            let mut sweep = tokio::time::interval(FED_ANNOUNCE_INTERVAL);
            loop {
                tokio::select! {
                    ev = tap_rx.recv() => match ev {
                        None => break,
                        Some(LiveFrameTap::Ingest { room_id, frame }) => {
                            inner.forward_to_relays(&room_id, frame);
                        }
                        Some(LiveFrameTap::Ended { room_id }) => {
                            inner.relays_ended(&room_id);
                        }
                    },
                    _ = sweep.tick() => {
                        inner.sweep_once(std::time::Instant::now());
                    }
                }
            }
        });
    }

    /// 是否已装配（未装配 = P2P 未启用，联邦静默停用）。
    #[must_use]
    pub fn is_federated(&self) -> bool {
        self.inner
            .transport
            .lock()
            .expect("live-fed: transport lock")
            .is_some()
    }

    /// 本节点名（未装配回退 "peer"）。
    #[must_use]
    pub fn node_name(&self) -> String {
        self.inner
            .transport
            .lock()
            .expect("live-fed: transport lock")
            .as_ref()
            .map(|t| t.node_name.clone())
            .unwrap_or_else(|| "peer".into())
    }

    /// 本节点 NodeID hex（未装配回退空串）。
    #[must_use]
    fn node_hex(&self) -> String {
        self.inner
            .transport
            .lock()
            .expect("live-fed: transport lock")
            .as_ref()
            .map(|t| t.node_hex.clone())
            .unwrap_or_default()
    }

    /// 定向发送（未装配 no-op）。
    fn send_to(&self, to: &os_p2p::NodeId, payload: serde_json::Value) {
        if let Some(t) = self
            .inner
            .transport
            .lock()
            .expect("live-fed: transport lock")
            .as_ref()
        {
            (t.send_to)(to, payload);
        }
    }

    /// 房间宣告（房间创建/状态变更/巡检刷新；未装配 no-op）。
    pub fn announce_room(&self, room: &LiveRoom) {
        if let Some(t) = self
            .inner
            .transport
            .lock()
            .expect("live-fed: transport lock")
            .as_ref()
        {
            let payload = build_live_lobby_payload(&t.node_hex, &t.node_name, room);
            (t.broadcast)(payload);
        }
    }

    /// 联邦大厅列表（先剔除过期条目再快照；按 id 排序）。
    pub fn federated_rooms(&self) -> Vec<FederatedLiveRoom> {
        self.inner.sweep_fed_rooms(std::time::Instant::now());
        let rooms = self.inner.fed_rooms.lock().expect("live-fed: fed_rooms lock");
        let mut list: Vec<FederatedLiveRoom> = rooms.values().map(|e| e.room.clone()).collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }    // ------------------------------------------------------------------
    // 入口：FederationBridge 分发（fed kind 路由）
    // ------------------------------------------------------------------

    /// 网络入口：按 `payload.fed` 分发给对应接收端（非 live_* 载荷忽略）。
    pub fn dispatch(&self, from: &os_p2p::NodeId, payload: &serde_json::Value) {
        match payload.get("fed").and_then(|v| v.as_str()) {
            Some(FED_KIND_LIVE_LOBBY) => {
                self.merge_announcement(from, payload);
            }
            Some(FED_KIND_LIVE_RELAY_SUB) => {
                self.handle_relay_sub(from, payload);
            }
            Some(FED_KIND_LIVE_RELAY_FRAME) => {
                self.ingest_relay_frame(payload);
            }
            Some(FED_KIND_LIVE_RELAY_UNSUB) => {
                self.handle_relay_unsub(from, payload);
            }
            _ => {}
        }
    }

    /// 房间宣告合并（幂等：按 fed_room_id upsert；ended 即出表；自回路忽略）。
    pub fn merge_announcement(&self, from: &os_p2p::NodeId, payload: &serde_json::Value) -> FedMerge {
        if payload.get("fed").and_then(|v| v.as_str()) != Some(FED_KIND_LIVE_LOBBY) {
            return FedMerge::Ignored;
        }
        // node_id 必须是合法 NodeID（中继定向目标；伪造无效）。
        let Some(node_hex) = payload.get("node_id").and_then(|v| v.as_str()) else {
            return FedMerge::Ignored;
        };
        if os_p2p::NodeId::parse(node_hex).is_none() {
            return FedMerge::Ignored;
        }
        // 自回路（同私钥多实例 / 本机指纹）：本地房间表已覆盖，不入联邦表。
        if node_hex == self.node_hex() || from.to_hex() == self.node_hex() {
            return FedMerge::Ignored;
        }
        let node_name = payload
            .get("node")
            .and_then(|v| v.as_str())
            .map(sanitize_fed_node_live)
            .unwrap_or_else(|| "peer".into());
        let Some(room) = payload.get("room") else {
            return FedMerge::Ignored;
        };
        let Some(room_id) = room.get("room_id").and_then(|v| v.as_str()) else {
            return FedMerge::Ignored;
        };
        // 本地 room_id 形态校验（防病态自报值进联邦表）。
        let valid_id = (4..=64).contains(&room_id.len())
            && room_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !valid_id {
            return FedMerge::Ignored;
        }
        let title: String = room
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .chars()
            .take(200)
            .collect();
        if title.is_empty() {
            return FedMerge::Ignored;
        }
        let source_kind = match room.get("source_kind").and_then(|v| v.as_str()) {
            Some("screen") | Some("camera") => room
                .get("source_kind")
                .and_then(|v| v.as_str())
                .unwrap_or("screen")
                .to_string(),
            _ => return FedMerge::Ignored,
        };
        let status = match room.get("status").and_then(|v| v.as_str()) {
            Some("ended") => "ended",
            _ => "live",
        };
        let viewer_count = room
            .get("viewer_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(1_000_000) as usize;
        let publisher_online = room
            .get("publisher_online")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let updated_at = room
            .get("updated_at")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .chars()
            .take(40)
            .collect::<String>();
        let fed_id = fed_room_id(node_hex, room_id);
        let mut rooms = self.inner.fed_rooms.lock().expect("live-fed: fed_rooms lock");
        if status == "ended" {
            let removed = rooms.remove(&fed_id).is_some();
            if removed {
                eprintln!("[live-fed] 联邦房间出表（ended）id={fed_id}");
            }
            return FedMerge::Removed;
        }
        let entry = FederatedLiveRoom {
            id: fed_id.clone(),
            title,
            source_kind,
            status: "live".into(),
            node_id: node_hex.to_string(),
            node_name,
            viewer_count,
            publisher_online,
            updated_at,
        };
        let existed = rooms.contains_key(&fed_id);
        rooms.insert(
            fed_id.clone(),
            FedRoomEntry {
                room: entry,
                last_seen: std::time::Instant::now(),
            },
        );
        if existed {
            FedMerge::Refreshed
        } else {
            eprintln!("[live-fed] 联邦房间入表 id={fed_id}");
            FedMerge::Inserted
        }
    }

    // ------------------------------------------------------------------
    // 源端：中继订阅（live_relay_sub / live_relay_unsub / 帧转发）
    // ------------------------------------------------------------------

    /// 收到中继订阅/心跳：房间在且为本节点房间 → 建订阅（新订阅先重放
    /// header）；房间不在 → 回 ended 收尾；载荷非法/非本节点房间 → 忽略。
    pub fn handle_relay_sub(&self, from: &os_p2p::NodeId, payload: &serde_json::Value) -> RelaySubResult {
        if payload.get("fed").and_then(|v| v.as_str()) != Some(FED_KIND_LIVE_RELAY_SUB) {
            return RelaySubResult::Ignored;
        }
        // 自回路：本机指纹订阅自己（同私钥多实例）——本地直连即可，不经中继。
        if from.to_hex() == self.node_hex() {
            return RelaySubResult::Ignored;
        }
        let Some(fed_id) = payload.get("room_id").and_then(|v| v.as_str()) else {
            return RelaySubResult::Ignored;
        };
        let Some(local_id) = local_room_id(&self.node_hex(), fed_id) else {
            return RelaySubResult::Ignored; // 不是本节点的房间
        };
        let view = self.inner.hub.room_view(&local_id);
        let Some(view) = view else {
            // 房间不存在/已结束：回 ended 让观众端影子房间立即收尾。
            self.send_to(
                from,
                build_relay_frame_payload(&self.node_name(), fed_id, 0, 0, 0, &[], true),
            );
            eprintln!("[live-fed] 中继订阅房间不存在，回 ended room={fed_id}");
            return RelaySubResult::RoomGone;
        };
        let from_hex = from.to_hex();
        let new_channel = {
            let mut subs = self.inner.relay_subs.lock().expect("live-fed: relay_subs lock");
            let entry = subs.entry(local_id.clone()).or_default();
            if let Some(sub) = entry.get_mut(&from_hex) {
                sub.last_seen = std::time::Instant::now(); // 心跳刷新
                None
            } else {
                let (tx, rx) = tokio::sync::mpsc::channel(RELAY_CHANNEL_CAPACITY);
                entry.insert(
                    from_hex.clone(),
                    RelaySub {
                        node: from.clone(),
                        tx: tx.clone(),
                        last_seen: std::time::Instant::now(),
                    },
                );
                Some((tx, rx))
            }
        };
        if let Some((tx, rx)) = new_channel {
            // 新订阅：先重放缓存 header（观众端 MSE 顺序 append 前提）。
            if let Some(h) = self.inner.hub.cached_header(&local_id) {
                let _ = tx.try_send(RelayFrameEvent::Frame(h));
            }
            Self::spawn_relay_sender(
                self.inner.clone(),
                local_id.clone(),
                fed_id.to_string(),
                from.clone(),
                rx,
            );
            eprintln!(
                "[live-fed] 中继订阅建立 room={local_id} to={from_hex}（title={}）",
                view.title
            );
            RelaySubResult::Subscribed
        } else {
            RelaySubResult::Refreshed
        }
    }

    /// 收到退订：剪除该节点的中继订阅（drop 通道 → 发送任务退出即停帧）。
    pub fn handle_relay_unsub(&self, from: &os_p2p::NodeId, payload: &serde_json::Value) -> bool {
        if payload.get("fed").and_then(|v| v.as_str()) != Some(FED_KIND_LIVE_RELAY_UNSUB) {
            return false;
        }
        let Some(fed_id) = payload.get("room_id").and_then(|v| v.as_str()) else {
            return false;
        };
        let Some(local_id) = local_room_id(&self.node_hex(), fed_id) else {
            return false;
        };
        let from_hex = from.to_hex();
        let removed = {
            let mut subs = self.inner.relay_subs.lock().expect("live-fed: relay_subs lock");
            subs.get_mut(&local_id)
                .is_some_and(|entry| entry.remove(&from_hex).is_some())
        };
        if removed {
            eprintln!("[live-fed] 中继退订 room={local_id} from={from_hex}");
        }
        removed
    }

    /// 中继发送任务（每订阅一条）：帧 → 分块 base64 定向回传；Ended → 收尾帧。
    fn spawn_relay_sender(
        inner: Arc<LiveFedInner>,
        local_id: String,
        fed_id: String,
        target: os_p2p::NodeId,
        mut rx: tokio::sync::mpsc::Receiver<RelayFrameEvent>,
    ) {
        tokio::spawn(async move {
            let mut seq: u64 = 0;
            while let Some(ev) = rx.recv().await {
                let (node_name, send_to) = {
                    let t = inner
                        .transport
                        .lock()
                        .expect("live-fed: transport lock");
                    let Some(t) = t.as_ref() else {
                        break;
                    };
                    (t.node_name.clone(), t.send_to.clone())
                };
                match ev {
                    RelayFrameEvent::Frame(frame) => {
                        seq += 1;
                        let total = frame.len();
                        let n = total.div_ceil(RELAY_CHUNK_BYTES).max(1);
                        for i in 0..n {
                            let start = i * RELAY_CHUNK_BYTES;
                            let end = usize::min(total, start + RELAY_CHUNK_BYTES);
                            let payload = build_relay_frame_payload(
                                &node_name,
                                &fed_id,
                                seq,
                                i as u32,
                                n as u32,
                                &frame[start..end],
                                false,
                            );
                            (send_to)(&target, payload);
                        }
                    }
                    RelayFrameEvent::Ended => {
                        let payload =
                            build_relay_frame_payload(&node_name, &fed_id, seq, 0, 0, &[], true);
                        (send_to)(&target, payload);
                        break;
                    }
                }
            }
            eprintln!("[live-fed] 中继发送任务结束 room={local_id} fed_id={fed_id}");
        });
    }

    // ------------------------------------------------------------------
    // 观端：中继帧接收 + 影子房间
    // ------------------------------------------------------------------

    /// 收到中继帧：分块重组 → 注入影子房间扇出；ended → 影子房间收尾。
    pub fn ingest_relay_frame(&self, payload: &serde_json::Value) -> RelayFrameOutcome {
        if payload.get("fed").and_then(|v| v.as_str()) != Some(FED_KIND_LIVE_RELAY_FRAME) {
            return RelayFrameOutcome::Ignored;
        }
        let Some(fed_id) = payload.get("room_id").and_then(|v| v.as_str()) else {
            return RelayFrameOutcome::Ignored;
        };
        if payload.get("ended").and_then(|v| v.as_bool()) == Some(true) {
            self.inner.hub.end_room(fed_id);
            eprintln!("[live-fed] 中继流结束，影子房间收尾 room={fed_id}");
            return RelayFrameOutcome::Ended;
        }
        let (seq, ci, cn) = match (
            payload.get("seq").and_then(|v| v.as_u64()),
            payload.get("ci").and_then(|v| v.as_u64()),
            payload.get("cn").and_then(|v| v.as_u64()),
        ) {
            (Some(s), Some(i), Some(n)) => (s, i, n),
            _ => return RelayFrameOutcome::Ignored,
        };
        if cn == 0 || cn > 512 || ci >= cn {
            return RelayFrameOutcome::Ignored;
        }
        let Some(b64) = payload.get("bytes").and_then(|v| v.as_str()) else {
            return RelayFrameOutcome::Ignored;
        };
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) else {
            return RelayFrameOutcome::Ignored;
        };
        if bytes.len() > RELAY_CHUNK_BYTES {
            return RelayFrameOutcome::Ignored; // 单块超限：病态载荷
        }
        let Some(frame) = self.inner.push_chunk(fed_id, seq, ci as usize, cn as usize, bytes) else {
            return RelayFrameOutcome::Pending;
        };
        match self.inner.hub.relay_inject(fed_id, frame) {
            IngestOutcome::Delivered { .. } => RelayFrameOutcome::Injected,
            IngestOutcome::RoomGone => RelayFrameOutcome::RoomGone,
            IngestOutcome::Oversized { .. } => RelayFrameOutcome::Oversized,
        }
    }

    /// 观端确保影子房间（subscribe 慢路径回调）：联邦表命中且 live → 建影子
    /// 房间 + 定向发中继订阅 + 起心跳任务。返回 false = 不是已知联邦房间。
    fn ensure_shadow_room(&self, fed_id: &str) -> bool {
        let entry = {
            let rooms = self.inner.fed_rooms.lock().expect("live-fed: fed_rooms lock");
            rooms
                .get(fed_id)
                .filter(|e| e.room.status == "live")
                .map(|e| e.room.clone())
        };
        let Some(entry) = entry else {
            return false;
        };
        self.inner.hub.create_shadow_room(fed_id, &entry);
        // 定向订阅源节点（回程路由用宣告携带的 node_id 真值）。
        if let Some(node) = os_p2p::NodeId::parse(&entry.node_id) {
            let (node_hex, node_name) = {
                let t = self
                    .inner
                    .transport
                    .lock()
                    .expect("live-fed: transport lock");
                let Some(t) = t.as_ref() else {
                    return true; // 未装配 overlay：影子房间建了但无帧（防御态）
                };
                (t.node_hex.clone(), t.node_name.clone())
            };
            if node.to_hex() != node_hex {
                let payload = build_relay_sub_payload(&node_hex, &node_name, fed_id);
                self.send_to(&node, payload);
            }
            Self::spawn_heartbeat(self.inner.clone(), fed_id.to_string(), node);
        }
        true
    }

    /// 观端心跳任务：影子房间存续期间每 30s 重发 sub（源端 90s 无刷新剪除）。
    fn spawn_heartbeat(inner: Arc<LiveFedInner>, fed_id: String, node: os_p2p::NodeId) {
        {
            let mut beats = inner
                .heartbeat_rooms
                .lock()
                .expect("live-fed: heartbeat lock");
            if beats.contains_key(&fed_id) {
                return; // 该影子房间已有心跳任务
            }
            beats.insert(fed_id.clone(), ());
        }
        let inner2 = inner.clone();
        let fed_id2 = fed_id.clone();
        tokio::spawn(async move {
            loop {
                let interval = *inner2
                    .heartbeat_interval
                    .lock()
                    .expect("live-fed: heartbeat interval");
                tokio::time::sleep(interval).await;
                if !inner2.hub.shadow_room_alive(&fed_id2) {
                    // 持锁双检（真机联调发现，106/113 跨节点联调代码审查）：
                    // 「判定死亡」到「防重表摘牌」之间若影子房间被新观众重建
                    // （观众快速退出重进），spawn_heartbeat 会被防重表里这条
                    // 尚未摘除的旧任务挡住而起不了新任务——新影子房间从此
                    // 无心跳刷新，源端 90s 后按 RELAY_SUB_TIMEOUT 剪除中继，
                    // 在看观众无感断流。本任务在持锁下二次确认房间确已消亡
                    // 才摘牌退场；被重建则直接收养继续跳。
                    // 锁序说明：全局仅此处持 heartbeat_rooms 锁内再取 rooms
                    // 锁（shadow_room_alive），无反向嵌套路径，无死锁环。
                    let mut beats = inner2
                        .heartbeat_rooms
                        .lock()
                        .expect("live-fed: heartbeat lock");
                    if inner2.hub.shadow_room_alive(&fed_id2) {
                        continue; // 收养重建的影子房间（本轮不发 sub，下轮补）
                    }
                    beats.remove(&fed_id2);
                    break;
                }
                let transport = inner2
                    .transport
                    .lock()
                    .expect("live-fed: transport lock")
                    .as_ref()
                    .map(|t| (t.send_to.clone(), t.node_hex.clone(), t.node_name.clone()));
                let Some((send_to, node_hex, node_name)) = transport else {
                    break;
                };
                let payload = build_relay_sub_payload(&node_hex, &node_name, &fed_id2);
                send_to(&node, payload);
            }
            inner2
                .heartbeat_rooms
                .lock()
                .expect("live-fed: heartbeat lock")
                .remove(&fed_id2);
        });
    }

    /// 测试注入：缩短本实例心跳周期（F7 回归测快进 tick 用；生产恒为
    /// [`RELAY_HEARTBEAT_INTERVAL`]，无 env 无配置面）。
    #[cfg(test)]
    fn set_heartbeat_interval_for_test(&self, d: Duration) {
        *self
            .inner
            .heartbeat_interval
            .lock()
            .expect("live-fed: heartbeat interval") = d;
    }

    /// 影子房间观众清零（hub.unsubscribe 回调）：退订中继（停帧）。
    fn stop_shadow_relay(&self, fed_id: &str) {
        let node = {
            let rooms = self.inner.fed_rooms.lock().expect("live-fed: fed_rooms lock");
            rooms
                .get(fed_id)
                .and_then(|e| os_p2p::NodeId::parse(&e.room.node_id))
        };
        if let Some(node) = node {
            let payload = build_relay_unsub_payload(&self.node_hex(), &self.node_name(), fed_id);
            self.send_to(&node, payload);
        }
    }
}

impl LiveFedInner {
    /// 主播帧 → 中继分发：逐订阅 try_send 有界通道（满即丢帧保实时），
    /// 成功投递数计房间 bytes_out（真实下行流量）。
    fn forward_to_relays(&self, room_id: &str, frame: Arc<Vec<u8>>) {
        let subs: Vec<RelaySub> = {
            let table = self.relay_subs.lock().expect("live-fed: relay_subs lock");
            table.get(room_id).map(|m| m.values().cloned().collect()).unwrap_or_default()
        };
        if subs.is_empty() {
            return;
        }
        let len = frame.len() as u64;
        let mut delivered = 0u64;
        let mut dead = 0usize;
        for sub in &subs {
            match sub.tx.try_send(RelayFrameEvent::Frame(frame.clone())) {
                Ok(()) => delivered += 1,
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    eprintln!(
                        "[live-fed] 中继背压丢帧 room={room_id} to={}",
                        sub.node.to_hex()
                    );
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => dead += 1,
            }
        }
        if dead > 0 {
            // 清死端（订阅方任务已退）
            if let Some(m) = self
                .relay_subs
                .lock()
                .expect("live-fed: relay_subs lock")
                .get_mut(room_id)
            {
                m.retain(|_, s| !s.tx.is_closed());
            }
        }
        if delivered > 0 {
            self.hub.add_relay_bytes(room_id, len * delivered);
        }
    }

    /// 房间终止 → 全部中继订阅收尾（Ended 帧后发送任务退出）。
    fn relays_ended(&self, room_id: &str) {
        let subs = self
            .relay_subs
            .lock()
            .expect("live-fed: relay_subs lock")
            .remove(room_id);
        if let Some(subs) = subs {
            let n = subs.len();
            for (_, sub) in subs {
                let _ = sub.tx.try_send(RelayFrameEvent::Ended);
            }
            eprintln!("[live-fed] 房间终止，中继订阅收尾 room={room_id}（{n} 条）");
        }
    }

    /// 观端分块入缓冲；集齐返回完整帧（同时清缓冲），否则 None。
    fn push_chunk(
        &self,
        fed_id: &str,
        seq: u64,
        ci: usize,
        cn: usize,
        bytes: Vec<u8>,
    ) -> Option<Vec<u8>> {
        let mut table = self.reassembly.lock().expect("live-fed: reassembly lock");
        let bufs = table.entry(fed_id.to_string()).or_default();
        // 待完成 seq 上限：超出丢最旧（防病态源撑爆重组缓冲）。
        if !bufs.contains_key(&seq) && bufs.len() >= RELAY_MAX_PENDING_SEQS {
            if let Some(oldest) = bufs.keys().copied().min() {
                bufs.remove(&oldest);
                eprintln!("[live-fed] 重组缓冲溢出，丢最旧 seq={oldest} room={fed_id}");
            }
        }
        let entry = bufs.entry(seq).or_insert_with(|| ChunkParts {
            parts: vec![None; cn],
            received: 0,
            total: 0,
        });
        if entry.parts.len() != cn {
            // 块数与先前不一致（源重发/病态）：重置该 seq 缓冲
            *entry = ChunkParts {
                parts: vec![None; cn],
                received: 0,
                total: 0,
            };
        }
        if entry.parts[ci].is_none() {
            entry.parts[ci] = Some(bytes.clone());
            entry.received += 1;
            entry.total += bytes.len();
        }
        if entry.received as usize == cn {
            let mut frame = Vec::with_capacity(entry.total);
            for p in entry.parts.drain(..).flatten() {
                frame.extend_from_slice(&p);
            }
            bufs.remove(&seq);
            if bufs.is_empty() {
                table.remove(fed_id);
            }
            Some(frame)
        } else {
            None
        }
    }

    /// 联邦表 TTL 剔除（巡检/读取共用）。
    fn sweep_fed_rooms(&self, now: std::time::Instant) {
        self.fed_rooms
            .lock()
            .expect("live-fed: fed_rooms lock")
            .retain(|id, e| {
                let fresh = now
                    .checked_duration_since(e.last_seen)
                    .is_some_and(|age| age < FED_ROOM_TTL);
                if !fresh {
                    eprintln!("[live-fed] 联邦房间 TTL 过期出表 id={id}");
                }
                fresh
            });
    }

    /// 巡检（30s 一轮）：TTL 剔除 + 心跳超时剪除中继订阅 + 本地房间重宣告。
    fn sweep_once(&self, now: std::time::Instant) {
        self.sweep_fed_rooms(now);
        // 源端中继订阅心跳超时剪除（drop tx → 发送任务退出即停帧）
        self.relay_subs
            .lock()
            .expect("live-fed: relay_subs lock")
            .retain(|room, subs| {
                subs.retain(|node, s| {
                    let fresh = now
                        .checked_duration_since(s.last_seen)
                        .is_some_and(|age| age < RELAY_SUB_TIMEOUT);
                    if !fresh {
                        eprintln!("[live-fed] 中继订阅心跳超时剪除 room={room} node={node}");
                    }
                    fresh
                });
                !subs.is_empty()
            });
        // 本地房间重宣告（刷新联邦各节点 TTL + viewer_count 漂移）
        let transport = self
            .transport
            .lock()
            .expect("live-fed: transport lock")
            .as_ref()
            .map(|t| (t.send_to.clone(), t.broadcast.clone(), t.node_hex.clone(), t.node_name.clone()));
        if let Some((_, broadcast, node_hex, node_name)) = transport {
            for room in self.hub.list_rooms() {
                let payload = build_live_lobby_payload(&node_hex, &node_name, &room);
                broadcast(payload);
            }
        }
    }
}

/// 签发 publish token（`lt-` + 32 hex）：sha256(房间号|pid|纳秒|随机计数)。
/// 不引 rand crate——熵源足够 token 不可猜（32 hex = 128 bit）。
fn mint_publish_token(room_id: &str, n: u64) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let digest = Sha256::digest(format!("{room_id}|{n}|{}|{nanos}|{c}", std::process::id()));
    format!("lt-{}", hex::encode(digest))
}

/// 创建者身份标签：admin Principal 用户名；匿名（测试期默认注入态）记 local-admin。
fn identity_label(auth: &Option<os_security::Principal>) -> String {
    auth.as_ref()
        .map(|p| {
            if p.user.name.trim().is_empty() {
                "local-admin".to_string()
            } else {
                p.user.name.clone()
            }
        })
        .unwrap_or_else(|| "local-admin".to_string())
}

// ----------------------------------------------------------------------------
// REST RouteHandler
// ----------------------------------------------------------------------------

/// 「直播」（流媒体中心直播 Tab）REST 入口（3 条，读写权限见路由表注释）。
pub struct LiveRouteHandler {
    hub: Arc<LiveHub>,
    fed: LiveFedEndpoint,
}

impl LiveRouteHandler {
    /// 生产构造：绑定进程级共享 [`LiveHub`] 与 [`LiveFedEndpoint`]（与 WS
    /// 升基层同源——影子房间/联邦表读写同一实例）。
    pub fn new() -> Self {
        Self {
            hub: LiveHub::shared(),
            fed: LiveFedEndpoint::shared(),
        }
    }

    /// 测试构造：注入独立 hub（与共享单例隔离，避免并行测试互染）。
    pub fn with_hub(hub: Arc<LiveHub>) -> Self {
        Self {
            hub: hub.clone(),
            fed: LiveFedEndpoint::new(hub),
        }
    }

    /// 联邦端点（main.rs 装配：Box 进网关前取出，p2p spawn 后 set_p2p 注入；
    /// FederationBridge 入站分发共用同一实例）。
    pub fn federation(&self) -> LiveFedEndpoint {
        self.fed.clone()
    }

    /// 房间列表便捷读（测试/诊断：不经 HTTP 分发直接读 hub 状态）。
    pub fn hub_list(&self) -> Vec<LiveRoom> {
        self.hub.list_rooms()
    }
}

impl Default for LiveRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

fn spec(
    method: HttpMethod,
    path: &str,
    requires_auth: bool,
    required_roles: Vec<String>,
) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: "live".to_string(),
        requires_auth,
        required_roles,
    }
}

fn ok_json(body: serde_json::Value) -> ApiResponse {
    ApiResponse {
        status: 200,
        body,
        headers: serde_json::json!({}),
    }
}

fn error_response(status: u16, msg: &str) -> ApiResponse {
    ApiResponse {
        status,
        body: serde_json::json!({"error": msg}),
        headers: serde_json::json!({}),
    }
}

fn to_value<T: Serialize>(v: &T) -> Result<serde_json::Value, ApiGatewayError> {
    serde_json::to_value(v).map_err(|e| ApiGatewayError::Internal(format!("响应序列化失败: {e}")))
}

fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

fn now_iso() -> String {
    use chrono::Local;
    Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

#[async_trait]
impl RouteHandler for LiveRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec(
                HttpMethod::Post,
                "/api/v1/live/rooms",
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Get, "/api/v1/live/rooms", false, vec![]),
            spec(
                HttpMethod::Delete,
                "/api/v1/live/rooms/:id",
                true,
                vec!["admin".into()],
            ),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            // —— POST /api/v1/live/rooms —— 创建房间（admin）
            (HttpMethod::Post, ["api", "v1", "live", "rooms"]) => {
                let body: CreateRoomBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析创建房间请求体失败: {e}"))
                })?;
                let title = body.title.trim().to_string();
                if title.is_empty() {
                    return Ok(error_response(400, "title 不可为空"));
                }
                if body.source_kind != "screen" && body.source_kind != "camera" {
                    return Ok(error_response(400, "source_kind 必须是 screen 或 camera"));
                }
                let (room, token) =
                    self.hub
                        .create_room(title, body.source_kind, identity_label(&req.auth));
                // 房间创建 → 联邦大厅宣告（广播 live_lobby；未装配 overlay 为 no-op）
                self.fed.announce_room(&room);
                let mut view = to_value(&room)?;
                view["publish_token"] = serde_json::json!(token);
                Ok(ApiResponse {
                    status: 201,
                    body: view,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/live/rooms —— 两段式大厅（公开读）：
            //    local = 本节点房间（可开播）；federated = 联邦宣告合并的远端房间。
            //    v1 返回数组，v1.1 起改为对象（调用方仅前端 liveListRooms 与本文件
            //    测试，均已随本变更迁移——见 docs/LIVE_STREAMING.md §3.1 兼容说明）。
            (HttpMethod::Get, ["api", "v1", "live", "rooms"]) => Ok(ok_json(
                serde_json::json!({
                    "local": to_value(&self.hub.list_rooms())?,
                    "federated": to_value(&self.fed.federated_rooms())?,
                }),
            )),

            // —— DELETE /api/v1/live/rooms/:id —— 结束直播（admin；踢断全部连接）
            (HttpMethod::Delete, ["api", "v1", "live", "rooms", id]) => {
                match self.hub.end_room(id) {
                    Some(room) => {
                        // 结束 → 联邦宣告 ended（各节点条目立即出表）
                        self.fed.announce_room(&room);
                        Ok(ok_json(to_value(&room)?))
                    }
                    None => Ok(error_response(404, &format!("live: 房间不存在: {id}"))),
                }
            }

            // —— 未覆盖路由 —— 兜底 404
            _ => Ok(error_response(404, "live: 未匹配的路由")),
        }
    }
}

// ----------------------------------------------------------------------------
// WS 升基层（axum handler）：/ws/live/{room_id}/{publish|view}
// ----------------------------------------------------------------------------

/// `/ws/live/*` 握手 query（仅 publish 用 token；view 公开）。
#[derive(serde::Deserialize, Default)]
pub struct LiveWsQuery {
    token: Option<String>,
}

/// 直播 WS 升级 handler（`http.rs::build_router` 以 `/ws/live/{room_id}/{action}`
/// 挂载）：升级前完成全部校验（客户端拿 HTTP 状态而非 WS 空转——同终端 WS 模式）。
///
/// - `publish`：token 与创建响应精确一致（401）；房间不存在 404；
///   通过后进入 [`run_publish_ws`]（二进制 webm chunk 上行）。
/// - `view`：房间存在即放行（404 兜底）；通过后进入 [`run_view_ws`]
///   （先重放 header 再实时转发）。
pub async fn live_ws_handler(
    ws: WebSocketUpgrade,
    Path((room_id, action)): Path<(String, String)>,
    Query(params): Query<LiveWsQuery>,
) -> Response {
    let hub = LiveHub::shared();
    match action.as_str() {
        "publish" => {
            let token = params.token.unwrap_or_default();
            match hub.attach_publisher(&room_id, &token) {
                Ok((gen, kick_rx)) => {
                    ws.on_upgrade(move |socket| run_publish_ws(socket, hub, room_id, gen, kick_rx))
                }
                Err(AttachError::BadToken) => (
                    axum::http::StatusCode::UNAUTHORIZED,
                    "live: WS publish 握手需要 ?token=<创建房间返回的 publish token>",
                )
                    .into_response(),
                Err(AttachError::RoomGone) => (
                    axum::http::StatusCode::NOT_FOUND,
                    format!("live: 房间不存在: {room_id}"),
                )
                    .into_response(),
            }
        }
        "view" => match hub.subscribe(&room_id) {
            Ok((viewer_id, rx)) => {
                ws.on_upgrade(move |socket| run_view_ws(socket, hub, room_id, viewer_id, rx))
            }
            Err(SubscribeError::RoomGone) => (
                axum::http::StatusCode::NOT_FOUND,
                format!("live: 房间不存在: {room_id}"),
            )
                .into_response(),
            Err(SubscribeError::Full) => (
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                "live: 本房间观众数已达上限",
            )
                .into_response(),
        },
        other => (
            axum::http::StatusCode::NOT_FOUND,
            format!("live: 未知 WS 动作 {other}（需 publish / view）"),
        )
            .into_response(),
    }
}

/// 房间是否存在（状态探针）：生产代码经 REST/WS 路径判定，本探针仅测试用。
#[cfg(test)]
impl LiveHub {
    fn room_exists(&self, id: &str) -> bool {
        self.rooms
            .lock()
            .expect("live: rooms lock")
            .contains_key(id)
    }
}

/// 主播上行循环：二进制帧 → [`LiveHub::ingest`] 扇出；文本控制帧
/// `{"kind":"stop"}` 主动结束；被踢（kick 通道断）/连接断开即清理。
async fn run_publish_ws(
    mut socket: WebSocket,
    hub: Arc<LiveHub>,
    room_id: String,
    gen: u64,
    mut kick_rx: tokio::sync::mpsc::Receiver<()>,
) {
    loop {
        tokio::select! {
            // 被踢（DELETE /rooms/:id 或新主播顶号：通道 drop → recv None）→ 立即断
            kicked = kick_rx.recv() => {
                eprintln!("[live] 主播被踢断开 room={room_id} gen={gen} signal={kicked:?}");
                break;
            }
            msg = socket.recv() => {
                match msg {
                    None | Some(Err(_)) | Some(Ok(AxumWsMessage::Close(_))) => break,
                    // 二进制帧 = MediaRecorder webm chunk → 扇出引擎
                    Some(Ok(AxumWsMessage::Binary(bytes))) => {
                        match hub.ingest(&room_id, bytes.to_vec()) {
                            IngestOutcome::Delivered { .. } => {}
                            IngestOutcome::RoomGone => {
                                eprintln!("[live] 房间已不存在，主播断开 room={room_id}");
                                break;
                            }
                            IngestOutcome::Oversized { size, limit } => {
                                // 拒收不扇出：回文本错误帧提示（保持连接，主播可降码率）
                                let text = serde_json::json!({
                                    "kind": "error",
                                    "msg": format!("帧超限拒收（{size} > {limit} 字节）"),
                                })
                                .to_string();
                                if socket.send(AxumWsMessage::Text(text.into())).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    // 文本控制帧：{"kind":"stop"} 主动结束
                    Some(Ok(AxumWsMessage::Text(text))) => {
                        let stop = serde_json::from_str::<serde_json::Value>(&text)
                            .ok()
                            .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(|k| k == "stop"))
                            .unwrap_or(false);
                        if stop {
                            eprintln!("[live] 主播发送 stop 控制帧 room={room_id}");
                            break;
                        }
                    }
                    Some(Ok(_)) => {} // 协议层 Ping/Pong 由 axum 自动应答
                }
            }
        }
    }
    hub.detach_publisher(&room_id, gen);
}

/// 观众下行循环：订阅通道 → 二进制帧顺序下发（header 已在 subscribe 时先入队）；
/// 房间结束/通道断 → 回 `{"kind":"ended"}` 文本帧收尾后关连接。
async fn run_view_ws(
    mut socket: WebSocket,
    hub: Arc<LiveHub>,
    room_id: String,
    viewer_id: u64,
    mut rx: tokio::sync::mpsc::Receiver<ViewerEvent>,
) {
    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Some(ViewerEvent::Media(bytes)) => {
                        let data: Vec<u8> = (*bytes).clone();
                        if socket.send(AxumWsMessage::Binary(data.into())).await.is_err() {
                            break;
                        }
                    }
                    Some(ViewerEvent::Ended) | None => {
                        let text = serde_json::json!({"kind": "ended"}).to_string();
                        let _ = socket.send(AxumWsMessage::Text(text.into())).await;
                        break;
                    }
                }
            }
            msg = socket.recv() => {
                match msg {
                    None | Some(Err(_)) | Some(Ok(AxumWsMessage::Close(_))) => break,
                    Some(Ok(_)) => {} // 观众只收不发；Ping/Pong 由 axum 自动应答
                }
            }
        }
    }
    hub.unsubscribe(&room_id, viewer_id);
}

// ----------------------------------------------------------------------------
// 单元测（mock 只进 cfg(test)：channel 注入 fake 订阅端，不起 WS）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn get_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    fn post_req(path: &str, body: serde_json::Value) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Post,
            path: path.into(),
            headers: serde_json::json!({}),
            body,
            auth: None,
        }
    }

    fn del_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Delete,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    /// 建独立 hub + handler（测试互不共享房间表）。
    fn fresh() -> (Arc<LiveHub>, LiveRouteHandler) {
        let hub = Arc::new(LiveHub::new());
        (hub.clone(), LiveRouteHandler::with_hub(hub))
    }

    /// 创建房间（走 REST 路径，返回 (room_id, token)）。
    async fn create_room(h: &LiveRouteHandler, title: &str, source_kind: &str) -> (String, String) {
        let resp = h
            .handle(post_req(
                "/api/v1/live/rooms",
                serde_json::json!({"title": title, "source_kind": source_kind}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "create body: {resp:?}");
        (
            resp.body["id"].as_str().unwrap().to_string(),
            resp.body["publish_token"].as_str().unwrap().to_string(),
        )
    }

    // ---- 路由声明 ----

    #[tokio::test]
    async fn routes_declares_all_live_endpoints() {
        let h = LiveRouteHandler::with_hub(Arc::new(LiveHub::new()));
        let routes = h.routes().await;
        assert_eq!(routes.len(), 3, "应有 3 条路由: {routes:?}");
        assert!(routes.iter().all(|r| r.handler_component == "live"));
        // POST / DELETE 需 admin；GET 公开
        for m in [HttpMethod::Post, HttpMethod::Delete] {
            let r = routes.iter().find(|r| r.method == m).unwrap();
            assert!(r.requires_auth, "{m:?} 需 auth: {r:?}");
            assert_eq!(r.required_roles, vec!["admin".to_string()]);
        }
        let g = routes.iter().find(|r| r.method == HttpMethod::Get).unwrap();
        assert_eq!(g.path, "/api/v1/live/rooms");
        assert!(!g.requires_auth, "GET 公开读");
    }

    // ---- 房间生命周期 ----

    #[tokio::test]
    async fn room_lifecycle_create_list_end() {
        let (_, h) = fresh();
        let (id, token) = create_room(&h, "测试直播", "screen").await;
        assert!(id.starts_with("live-"), "房间 id 形如 live-N: {id}");
        assert!(
            token.starts_with("lt-") && token.len() > 16,
            "token 形如 lt-<hex>"
        );

        // 创建响应字段（含真实初始计数 + token）
        let resp = h
            .handle(post_req(
                "/api/v1/live/rooms",
                serde_json::json!({"title": "第二间", "source_kind": "camera"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["viewer_count"], 0);
        assert_eq!(resp.body["status"], "live");
        assert_eq!(resp.body["bytes_in"], 0);
        assert_eq!(resp.body["publisher_identity"], "local-admin");
        assert!(resp.body["publish_token"].is_string());

        // 列表公开读（两段式大厅）：两个本地房间、字段实时；联邦表无 seed 恒空
        let resp = h.handle(get_req("/api/v1/live/rooms")).await.unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body["local"].as_array().unwrap();
        assert_eq!(arr.len(), 2, "无 seed 无演示房间，恰为本次创建的 2 间");
        assert_eq!(arr[0]["title"], "测试直播");
        assert_eq!(arr[0]["source_kind"], "screen");
        assert!(
            resp.body["federated"].as_array().unwrap().is_empty(),
            "联邦大厅无 seed：{federated}",
            federated = resp.body["federated"]
        );

        // 结束（admin 路由声明 + 真实踢断语义在 hub 层测）：出表
        let resp = h
            .handle(del_req(&format!("/api/v1/live/rooms/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["status"], "ended");
        let resp = h.handle(get_req("/api/v1/live/rooms")).await.unwrap();
        assert_eq!(resp.body["local"].as_array().unwrap().len(), 1, "结束后出表");

        // 不存在的房间 404
        let resp = h
            .handle(del_req("/api/v1/live/rooms/live-999"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);

        // 未匹配路由 404
        let resp = h.handle(get_req("/api/v1/live/nope")).await.unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn create_validates_title_and_source_kind() {
        let (_, h) = fresh();
        let resp = h
            .handle(post_req(
                "/api/v1/live/rooms",
                serde_json::json!({"title": "  ", "source_kind": "screen"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "空白 title 拒绝");
        let resp = h
            .handle(post_req(
                "/api/v1/live/rooms",
                serde_json::json!({"title": "x", "source_kind": "mic"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "非法 source_kind 拒绝");
    }

    // ---- 扇出逻辑（注入 fake 订阅端 channel，不起 WS）----

    #[tokio::test]
    async fn fanout_header_replay_then_live_frames_in_order() {
        let (hub, h) = fresh();
        let (id, token) = create_room(&h, "扇出", "screen").await;
        let (_gen, _kick) = hub.attach_publisher(&id, &token).expect("token 正确接入");

        // 两个 fake 订阅端（真实 subscribe 路径，无 WS）
        let (_v1, mut rx1) = hub.subscribe(&id).expect("观众1接入");
        let (_v2, mut rx2) = hub.subscribe(&id).expect("观众2接入");

        // 中途加入的观众没有 header：主播尚未发帧
        // → 首帧（init segment）后新观众重放
        let header = b"EBML-INIT".to_vec();
        let out = hub.ingest(&id, header.clone());
        assert_eq!(out, IngestOutcome::Delivered { delivered: 2 });
        // 先入场的两人直接收到
        assert_eq!(
            rx1.recv().await,
            Some(ViewerEvent::Media(Arc::new(header.clone())))
        );
        assert_eq!(
            rx2.recv().await,
            Some(ViewerEvent::Media(Arc::new(header.clone())))
        );

        // 中途加入的第 3 人：连上即先收缓存 header，再收后续帧
        let (_v3, mut rx3) = hub.subscribe(&id).expect("观众3中途接入");
        assert_eq!(rx3.recv().await, Some(ViewerEvent::Media(Arc::new(header))));
        hub.ingest(&id, b"CLUSTER-1".to_vec());
        assert_eq!(
            rx1.recv().await,
            Some(ViewerEvent::Media(Arc::new(b"CLUSTER-1".to_vec())))
        );
        assert_eq!(
            rx3.recv().await,
            Some(ViewerEvent::Media(Arc::new(b"CLUSTER-1".to_vec())))
        );

        // 计数为真实值：3 订阅端 × 2 轮（首帧 2 人 + 第 2 帧 3 人）× 9 字节
        let rooms = hub.list_rooms();
        assert_eq!(rooms[0].viewer_count, 3);
        assert_eq!(rooms[0].bytes_in, 9 + 9);
        assert_eq!(rooms[0].bytes_out, 9 * 2 + 9 * 3);
        assert!(rooms[0].header_cached);
        assert!(rooms[0].publisher_online);
    }

    #[tokio::test]
    async fn slow_consumer_drops_frames_fast_viewer_keeps_all() {
        let (hub, h) = fresh();
        let (id, token) = create_room(&h, "慢消费者", "camera").await;
        let _ = hub.attach_publisher(&id, &token).unwrap();

        // 慢消费者：订阅后不收帧（通道容量 64）；快消费者：每帧都收。
        // 绑定 _slow_rx（下划线前缀）保持接收端存活到作用域尾 = 通道持续满。
        let (_slow_id, _slow_rx) = hub.subscribe(&id).expect("慢消费者接入");
        let (_fast_id, mut fast_rx) = hub.subscribe(&id).expect("快消费者接入");

        // 首帧（header）：快消费者立即收走（保持队列不满，后续帧零丢失）
        hub.ingest(&id, b"H".to_vec());
        assert_eq!(
            fast_rx.recv().await,
            Some(ViewerEvent::Media(Arc::new(b"H".to_vec())))
        );
        // 灌 84 帧（> 慢消费者剩余容量 63）：慢的溢出丢帧，快的逐帧全收
        for i in 0..(VIEWER_CHANNEL_CAPACITY as u64 + 20) {
            hub.ingest(&id, vec![i as u8; 4]);
            assert_eq!(
                fast_rx.recv().await,
                Some(ViewerEvent::Media(Arc::new(vec![i as u8; 4]))),
                "快消费者第 {i} 帧不丢"
            );
        }

        // 慢消费者丢帧已计数（> 容量 - 已入队数），快消费者 0 丢
        let rooms = hub.list_rooms();
        let dropped = rooms[0].dropped_frames;
        assert!(dropped >= 20, "慢消费者丢帧应 >= 溢出量 20: {dropped}");
        assert_eq!(rooms[0].viewer_count, 2, "丢帧不踢人（保连接）");
    }

    #[tokio::test]
    async fn viewer_count_increments_decrements_and_room_recycles() {
        let (hub, h) = fresh();
        let (id, token) = create_room(&h, "计数", "screen").await;
        let _ = hub.attach_publisher(&id, &token).unwrap();

        let (v1, _rx1) = hub.subscribe(&id).unwrap();
        let (v2, rx2) = hub.subscribe(&id).unwrap();
        assert_eq!(hub.list_rooms()[0].viewer_count, 2, "连上 +1");

        hub.unsubscribe(&id, v1);
        assert_eq!(hub.list_rooms()[0].viewer_count, 1, "断开 -1");

        // 主播在：观众清零不回收
        hub.unsubscribe(&id, v2);
        drop(rx2);
        assert_eq!(hub.list_rooms().len(), 1, "主播在线，房间保留");

        // 主播断开（观众已清零）→ 回收出表
        hub.detach_publisher(&id, 1);
        assert!(hub.list_rooms().is_empty(), "无主播且观众清零 → 回收");
    }

    #[tokio::test]
    async fn publisher_detach_marks_ended_and_notifies_viewers() {
        let (hub, h) = fresh();
        let (id, token) = create_room(&h, "断开通知", "screen").await;
        let (gen, _kick) = hub.attach_publisher(&id, &token).unwrap();
        hub.ingest(&id, b"HDR".to_vec());
        let (_v, mut rx) = hub.subscribe(&id).unwrap();
        // 先取走 header
        assert_eq!(
            rx.recv().await,
            Some(ViewerEvent::Media(Arc::new(b"HDR".to_vec())))
        );

        hub.detach_publisher(&id, gen);
        let rooms = hub.list_rooms();
        assert_eq!(rooms[0].status, "ended");
        assert!(!rooms[0].publisher_online);
        // 观众收到 ended 收尾帧
        assert_eq!(rx.recv().await, Some(ViewerEvent::Ended));
    }

    #[tokio::test]
    async fn publisher_replaces_old_connection_by_generation() {
        let (hub, h) = fresh();
        let (id, token) = create_room(&h, "顶号", "screen").await;
        let (gen1, mut kick1) = hub.attach_publisher(&id, &token).unwrap();
        let (gen2, kick2) = hub.attach_publisher(&id, &token).unwrap();
        assert_eq!(gen2, gen1 + 1, "代际递增");
        // 旧主播的踢线通道断（recv None = 旧 WS 循环退出）
        assert_eq!(kick1.recv().await, None);
        drop(kick2);

        // 旧代 detach 不影响新主播
        hub.detach_publisher(&id, gen1);
        assert!(
            hub.room_exists(&id) && hub.list_rooms()[0].publisher_online,
            "旧代 detach 不清新主播"
        );
        // 新代 detach 正常清理（先留一名观众，房间不因无观众被回收，可查状态）
        let (_v, _rx) = hub.subscribe(&id).expect("占位观众");
        hub.detach_publisher(&id, gen2);
        let rooms = hub.list_rooms();
        assert_eq!(rooms.len(), 1);
        assert!(!rooms[0].publisher_online);
        assert_eq!(rooms[0].status, "ended");
    }

    #[tokio::test]
    async fn wrong_token_cannot_publish() {
        let (hub, h) = fresh();
        let (id, _token) = create_room(&h, "鉴权", "screen").await;
        assert!(hub.room_exists(&id), "创建后房间在表");
        assert!(
            hub.attach_publisher(&id, "lt-wrong").is_err(),
            "错 token 拒绝"
        );
        assert!(hub.attach_publisher(&id, "").is_err(), "空 token 拒绝");
        assert!(!hub.room_exists("live-999"), "不存在的房间不在表");
        assert!(
            hub.attach_publisher("live-999", "any").is_err(),
            "房间不存在拒绝"
        );
    }

    // ---- 上行限流 ----

    #[tokio::test]
    async fn oversize_frame_rejected_not_fanned_out() {
        let (hub, h) = fresh();
        let (id, token) = create_room(&h, "限流", "screen").await;
        let _ = hub.attach_publisher(&id, &token).unwrap();
        let (_v, mut rx) = hub.subscribe(&id).unwrap();

        let big = vec![0u8; DEFAULT_MAX_FRAME_BYTES + 1];
        let out = hub.ingest(&id, big);
        assert_eq!(
            out,
            IngestOutcome::Oversized {
                size: DEFAULT_MAX_FRAME_BYTES + 1,
                limit: DEFAULT_MAX_FRAME_BYTES,
            }
        );
        // 拒收：不计 bytes_in、不扇出（观众无帧可收——用短超时探测）
        let rooms = hub.list_rooms();
        assert_eq!(rooms[0].bytes_in, 0);
        assert_eq!(rooms[0].rejected_frames, 1);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err(),
            "超限帧不得下发观众"
        );

        // 边界值恰好等于上限：放行
        let edge = vec![0u8; DEFAULT_MAX_FRAME_BYTES];
        assert_eq!(
            hub.ingest(&id, edge),
            IngestOutcome::Delivered { delivered: 1 }
        );
        assert_eq!(hub.list_rooms()[0].rejected_frames, 1, "边界帧不拒");
    }

    #[tokio::test]
    async fn subscriber_cap_rejects_beyond_limit() {
        let hub = LiveHub {
            rooms: Mutex::new(HashMap::new()),
            next_room: AtomicU64::new(1),
            next_viewer_id: AtomicU64::new(1),
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            max_viewers: 2, // 测试用小上限
            fed: Mutex::new(None),
            frame_tap: Mutex::new(None),
        };
        let hub = Arc::new(hub);
        let (id, token) = {
            let h = LiveRouteHandler::with_hub(hub.clone());
            create_room(&h, "上限", "screen").await
        };
        let _ = hub.attach_publisher(&id, &token).unwrap();
        hub.subscribe(&id).unwrap();
        let (v2, _rx2) = hub.subscribe(&id).unwrap();
        assert!(matches!(hub.subscribe(&id), Err(SubscribeError::Full),));
        // 断开一人后有空位，可再入
        hub.unsubscribe(&id, v2);
        assert!(hub.subscribe(&id).is_ok(), "断开后应有空位");
    }

    #[test]
    fn env_usize_falls_back_on_invalid() {
        // 非法值回默认（不 panic；env 操作只在单测内自洽）
        std::env::set_var("NEXOS_LIVE_TEST_X", "not-a-number");
        assert_eq!(env_usize("NEXOS_LIVE_TEST_X", 42), 42);
        std::env::set_var("NEXOS_LIVE_TEST_X", "7");
        assert_eq!(env_usize("NEXOS_LIVE_TEST_X", 42), 7);
        assert_eq!(env_usize("NEXOS_LIVE_TEST_UNSET_KEY", 42), 42);
        std::env::remove_var("NEXOS_LIVE_TEST_X");
    }

    #[test]
    fn mint_token_shape_and_uniqueness() {
        let a = mint_publish_token("live-1", 1);
        let b = mint_publish_token("live-1", 1);
        assert!(
            a.starts_with("lt-") && a.len() == 3 + 64,
            "lt- + 64 hex: {a}"
        );
        assert_ne!(a, b, "同房间两次签发不重复（纳秒+计数熵）");
    }

    // ---- 联邦（本地大厅 + 联邦大厅 + 跨节点中继）----

    /// 测试用合法 NodeID（secp256k1 生成点 G 与 2G——NodeId::parse 要求合法点）。
    const NODE_A_HEX: &str = "0x0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
    const NODE_B_HEX: &str = "0x02c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5";

    fn test_node(hex: &str) -> os_p2p::NodeId {
        os_p2p::NodeId::parse(hex).expect("测试 NodeID 须为合法压缩公钥")
    }

    /// 房间视图 fixture（联邦宣告源材料）。
    fn room_view_fixture(id: &str, title: &str, viewer_count: usize) -> LiveRoom {
        LiveRoom {
            id: id.into(),
            title: title.into(),
            source_kind: "screen".into(),
            created_at: "2026-08-31T10:00:00+08:00".into(),
            publisher_identity: "remote-admin".into(),
            viewer_count,
            status: "live".into(),
            bytes_in: 0,
            bytes_out: 0,
            dropped_frames: 0,
            rejected_frames: 0,
            publisher_online: true,
            header_cached: false,
        }
    }

    /// fake 互连 overlay：A/B 两端点定向互投（send_to → 对端 dispatch）、
    /// 广播也投对端（模拟 fed_broadcast 一跳可达）；全部外发载荷副本进捕获
    /// 日志。参考 handlers/p2p.rs 测试的 fake 注入手法（不起真 socket）。
    fn interconnect(
        a: &LiveFedEndpoint,
        b: &LiveFedEndpoint,
        log: Arc<Mutex<Vec<serde_json::Value>>>,
    ) {
        let a_id = test_node(NODE_A_HEX);
        let b_id = test_node(NODE_B_HEX);
        // A → B
        let b_for_send = b.clone();
        let log_send_a = log.clone();
        let send_a: FedSendFn = Arc::new(move |to, p| {
            log_send_a.lock().expect("log").push(p.clone());
            if *to == b_id {
                b_for_send.dispatch(&test_node(NODE_A_HEX), &p);
            }
        });
        let b_for_bcast = b.clone();
        let log_a = log.clone();
        let broadcast_a: FedBroadcastFn = Arc::new(move |p| {
            log_a.lock().expect("log").push(p.clone());
            b_for_bcast.dispatch(&test_node(NODE_A_HEX), &p);
        });
        a.install(send_a, broadcast_a, NODE_A_HEX.into(), "node-a".into());
        // B → A
        let a_for_send = a.clone();
        let log_send_b = log.clone();
        let send_b: FedSendFn = Arc::new(move |to, p| {
            log_send_b.lock().expect("log").push(p.clone());
            if *to == a_id {
                a_for_send.dispatch(&test_node(NODE_B_HEX), &p);
            }
        });
        let a_for_bcast = a.clone();
        let log_b = log.clone();
        let broadcast_b: FedBroadcastFn = Arc::new(move |p| {
            log_b.lock().expect("log").push(p.clone());
            a_for_bcast.dispatch(&test_node(NODE_B_HEX), &p);
        });
        b.install(send_b, broadcast_b, NODE_B_HEX.into(), "node-b".into());
    }

    /// A 侧中继订阅总数（观测面）。
    fn relay_sub_total(fed: &LiveFedEndpoint) -> usize {
        fed.inner
            .relay_subs
            .lock()
            .expect("relay_subs")
            .values()
            .map(|m| m.len())
            .sum()
    }

    /// F1. 联邦纯函数：fed_room_id 前缀防撞 + local_room_id 回程解析。
    #[test]
    fn fed_room_id_prefix_and_roundtrip() {
        let fed = fed_room_id(NODE_A_HEX, "live-1");
        assert_eq!(fed, "0279be66:live-1", "前缀 = NodeID hex 去 0x 前 8 字符");
        assert_ne!(fed, fed_room_id(NODE_B_HEX, "live-1"), "跨节点同号房间不撞");
        assert_eq!(
            local_room_id(NODE_A_HEX, &fed).as_deref(),
            Some("live-1"),
            "源端能还原本地房间号"
        );
        assert_eq!(
            local_room_id(NODE_B_HEX, &fed),
            None,
            "非本节点前缀还原失败（中继订阅路由判定）"
        );
    }

    /// F2. 宣告载荷序列化：kind/node/node_id/room 全要素 + fed_room_id 派生；
    ///     中继帧分块字段 + base64 + ended 控制帧无 bytes。
    #[test]
    fn fed_announcement_payload_shape() {
        let room = room_view_fixture("live-3", "NexOS 周会", 7);
        let payload = build_live_lobby_payload(NODE_A_HEX, "node-a", &room);
        assert_eq!(payload["fed"], FED_KIND_LIVE_LOBBY);
        assert_eq!(payload["node"], "node-a");
        assert_eq!(payload["node_id"], NODE_A_HEX);
        assert_eq!(payload["room"]["room_id"], "live-3");
        assert_eq!(payload["room"]["fed_room_id"], "0279be66:live-3");
        assert_eq!(payload["room"]["title"], "NexOS 周会");
        assert_eq!(payload["room"]["status"], "live");
        assert_eq!(payload["room"]["viewer_count"], 7);
        assert!(payload["room"]["updated_at"]
            .as_str()
            .is_some_and(|s| !s.is_empty()));
        // 节点名净化：空回 "peer"
        let dirty = build_live_lobby_payload(NODE_A_HEX, "   ", &room);
        assert_eq!(dirty["node"], "peer");
        // 中继帧载荷：分块字段 + base64；ended 控制帧无 bytes
        let frame =
            build_relay_frame_payload("node-a", "0279be66:live-3", 9, 1, 2, b"abc", false);
        assert_eq!(frame["fed"], FED_KIND_LIVE_RELAY_FRAME);
        assert_eq!(frame["seq"], 9);
        assert_eq!(frame["ci"], 1);
        assert_eq!(frame["cn"], 2);
        assert_eq!(
            frame["bytes"],
            base64::engine::general_purpose::STANDARD.encode(b"abc")
        );
        let ended = build_relay_frame_payload("node-a", "0279be66:live-3", 9, 0, 0, &[], true);
        assert_eq!(ended["ended"], true);
        assert!(ended.get("bytes").is_none(), "ended 控制帧无 bytes");
    }

    /// F3. 联邦房间表：幂等合并（同 id 刷新不重复）/ ended 立即出表 /
    ///     TTL 过期剔除 / 非法载荷与自回路忽略。
    #[tokio::test]
    async fn fed_merge_idempotent_ttl_and_ended() {
        let (_, h) = fresh();
        let fed = h.federation();
        let from = test_node(NODE_A_HEX);
        let mk = |viewers: usize, status: &str| {
            let mut room = room_view_fixture("live-5", "远端房间", viewers);
            room.status = status.into();
            build_live_lobby_payload(NODE_A_HEX, "node-a", &room)
        };

        // 首次 → Inserted；重复（viewer_count 变化）→ Refreshed（条目仍 1 条）
        assert_eq!(
            fed.merge_announcement(&from, &mk(3, "live")),
            FedMerge::Inserted
        );
        assert_eq!(
            fed.merge_announcement(&from, &mk(5, "live")),
            FedMerge::Refreshed
        );
        let list = fed.federated_rooms();
        assert_eq!(list.len(), 1, "幂等合并不产生重复条目");
        assert_eq!(list[0].id, "0279be66:live-5");
        assert_eq!(list[0].viewer_count, 5, "刷新覆盖字段");
        assert_eq!(list[0].node_name, "node-a");
        assert_eq!(list[0].node_id, NODE_A_HEX);

        // 另一节点的房间 → 独立条目
        let other = build_live_lobby_payload(
            NODE_B_HEX,
            "node-b",
            &room_view_fixture("live-5", "同号房间", 1),
        );
        assert_eq!(
            fed.merge_announcement(&test_node(NODE_B_HEX), &other),
            FedMerge::Inserted
        );
        assert_eq!(fed.federated_rooms().len(), 2, "不同节点前缀不合并");

        // 非法载荷：缺 node_id / 病态 room_id
        assert_eq!(
            fed.merge_announcement(&from, &serde_json::json!({"fed": FED_KIND_LIVE_LOBBY})),
            FedMerge::Ignored
        );
        let bad_id = build_live_lobby_payload(
            NODE_A_HEX,
            "node-a",
            &room_view_fixture("bad id!", "x", 1),
        );
        assert_eq!(fed.merge_announcement(&from, &bad_id), FedMerge::Ignored);

        // 自回路：装上本端身份（node-b）后，node_id==本机的宣告不入联邦表
        fed.install(
            Arc::new(|_to: &os_p2p::NodeId, _p: serde_json::Value| {}),
            Arc::new(|_p: serde_json::Value| {}),
            NODE_B_HEX.into(),
            "node-b".into(),
        );
        let self_loop =
            build_live_lobby_payload(NODE_B_HEX, "self", &room_view_fixture("live-9", "x", 0));
        assert_eq!(
            fed.merge_announcement(&test_node(NODE_B_HEX), &self_loop),
            FedMerge::Ignored,
            "本机指纹自回路不入联邦表"
        );

        // TTL：91s 无刷新剔除（合成 now，不真等）
        let later = std::time::Instant::now() + std::time::Duration::from_secs(91);
        fed.inner.sweep_once(later);
        assert!(
            fed.federated_rooms().is_empty(),
            "TTL 过期全部出表（房间是短暂状态）"
        );

        // ended：立即出表（不等 TTL）
        assert_eq!(
            fed.merge_announcement(&from, &mk(1, "live")),
            FedMerge::Inserted
        );
        assert_eq!(
            fed.merge_announcement(&from, &mk(1, "ended")),
            FedMerge::Removed
        );
        assert!(fed.federated_rooms().is_empty());
    }

    /// F4. 两段式大厅端点契约（向后兼容迁移断言）：GET /rooms 顶层对象
    ///     {local, federated}——本地数组 + 联邦数组（旧数组形态的唯一调用方
    ///     前端 liveListRooms 与本文件测试已随本变更迁移，见
    ///     docs/LIVE_STREAMING.md §3.1）。
    #[tokio::test]
    async fn rooms_endpoint_two_lobby_shape() {
        let (_, h) = fresh();
        let fed = h.federation();
        let (id, _token) = create_room(&h, "本地房间", "screen").await;
        // 合并一条远端宣告
        let payload = build_live_lobby_payload(
            NODE_A_HEX,
            "node-a",
            &room_view_fixture("live-2", "远端房间", 4),
        );
        assert_eq!(
            fed.merge_announcement(&test_node(NODE_A_HEX), &payload),
            FedMerge::Inserted
        );

        let resp = h.handle(get_req("/api/v1/live/rooms")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert!(!resp.body.is_array(), "v1.1 起顶层为对象（不再是无标记数组）");
        let local = resp.body["local"].as_array().expect("local 数组");
        let federated = resp.body["federated"].as_array().expect("federated 数组");
        assert_eq!(local.len(), 1);
        assert_eq!(local[0]["id"], serde_json::json!(id));
        assert_eq!(federated.len(), 1);
        assert_eq!(federated[0]["id"], "0279be66:live-2");
        assert_eq!(federated[0]["node_name"], "node-a");
        assert_eq!(federated[0]["viewer_count"], 4);
    }

    /// F5. 中继闭环（两 LiveHub 实例 + fake 互连 overlay）：A 播（header +
    ///     1.5 MiB 双块帧）→ B 订（影子房间 + live_relay_sub）→ B 观众收
    ///     header + 重组帧 → 退订剪除 → 重订 → A 下播 → B 观众收 Ended 收尾
    ///     + 联邦条目出表。中继帧计入 A 房间 bytes_out。
    #[tokio::test]
    async fn relay_sub_chunk_reassembly_unsub_ended_loop() {
        let (hub_a, h_a) = fresh();
        let (hub_b, h_b) = fresh();
        let fed_a = h_a.federation();
        let fed_b = h_b.federation();
        let log: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        interconnect(&fed_a, &fed_b, log.clone());
        assert!(fed_a.is_federated() && fed_b.is_federated());

        // A 创建房间 → 宣告经 fake overlay 到 B（联邦表入条目）
        let (room_id, token) = create_room(&h_a, "跨节点直播", "screen").await;
        let fed_id = fed_room_id(NODE_A_HEX, &room_id);
        let list = fed_b.federated_rooms();
        assert_eq!(list.len(), 1, "宣告已合并: {list:?}");
        assert_eq!(list[0].id, fed_id);

        // A 主播接入（二次宣告 publisher_online=true）
        let (gen, _kick) = hub_a.attach_publisher(&room_id, &token).expect("A 主播接入");
        assert!(
            fed_b.federated_rooms()[0].publisher_online,
            "attach 宣告刷新到 B"
        );

        // B 观众订阅联邦形态 id → 影子房间 + 中继订阅（A 侧建立）
        let (viewer, mut rx) = hub_b.subscribe(&fed_id).expect("B 观众接入影子房间");
        // 影子房间不出现在 B 的本地大厅
        assert!(h_b.hub_list().is_empty(), "影子房间不进本地列表");
        // 影子房间不可本地 publish（远端房间只读）
        assert!(hub_b.attach_publisher(&fed_id, "").is_err());
        assert_eq!(relay_sub_total(&fed_a), 1, "A 侧一条中继订阅");

        // A 推 header + 双块大帧（1.5 MiB > RELAY_CHUNK_BYTES 1 MiB → 2 块）
        assert_eq!(
            hub_a.ingest(&room_id, b"INIT-SEG".to_vec()),
            IngestOutcome::Delivered { delivered: 0 }
        );
        let big: Vec<u8> = (0..RELAY_CHUNK_BYTES + 512 * 1024)
            .map(|i| (i % 251) as u8)
            .collect();
        assert_eq!(
            hub_a.ingest(&room_id, big.clone()),
            IngestOutcome::Delivered { delivered: 0 }
        );

        // B 观众先收 header 重放，再收重组完整大帧（尺寸一致）
        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("等 header")
            .expect("通道未关");
        assert_eq!(ev, ViewerEvent::Media(Arc::new(b"INIT-SEG".to_vec())));
        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("等重组大帧")
            .expect("通道未关");
        match ev {
            ViewerEvent::Media(frame) => {
                assert_eq!(frame.len(), big.len(), "重组尺寸一致");
                assert_eq!(&frame[..big.len()], &big[..], "重组字节级一致");
            }
            other => panic!("应为媒体帧: {other:?}"),
        }
        // 不应再有积压帧
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err(),
            "帧队列应已取空"
        );

        // 中继帧计入 A 房间 bytes_out（真实下行流量：8 字节 header + 大帧字节数）
        let out = hub_a.list_rooms()[0].bytes_out;
        assert!(out >= (8 + big.len()) as u64, "中继计 bytes_out: {out}");

        // B 观众退订 → 影子房间出表 + A 侧收到 live_relay_unsub 剪除订阅
        hub_b.unsubscribe(&fed_id, viewer);
        assert!(!hub_b.room_exists(&fed_id), "影子房间观众清零出表");
        assert_eq!(relay_sub_total(&fed_a), 0, "退订后 A 侧订阅剪除");
        assert!(
            log.lock()
                .expect("log")
                .iter()
                .any(|p| p["fed"] == FED_KIND_LIVE_RELAY_UNSUB && p["room_id"] == fed_id),
            "live_relay_unsub 已发出"
        );

        // 重订 + A 下播 → B 新观众先收 header 重放（新中继订阅的 init segment）
        // 再收实时帧；A 下播 → B 观众收 Ended 收尾；ended 宣告 → 联邦条目出表
        let (_viewer2, mut rx2) = hub_b.subscribe(&fed_id).expect("B 观众重订");
        hub_a.ingest(&room_id, b"CLUSTER-9".to_vec());
        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), rx2.recv())
            .await
            .expect("等重订后 header 重放")
            .expect("通道未关");
        assert_eq!(ev, ViewerEvent::Media(Arc::new(b"INIT-SEG".to_vec())));
        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), rx2.recv())
            .await
            .expect("等重订后实时帧")
            .expect("通道未关");
        assert_eq!(ev, ViewerEvent::Media(Arc::new(b"CLUSTER-9".to_vec())));
        hub_a.detach_publisher(&room_id, gen);
        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), rx2.recv())
            .await
            .expect("等 Ended 收尾")
            .expect("通道未关");
        assert_eq!(ev, ViewerEvent::Ended, "源端下播 → 中继 ended → B 收尾");
        // detach 的 ended 宣告让 B 联邦表出表
        for _ in 0..20 {
            if fed_b.federated_rooms().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(fed_b.federated_rooms().is_empty(), "ended 宣告 → 联邦条目出表");
    }

    /// F6. 中继订阅心跳超时剪除（源端 90s 无刷新 → sweep 剪除即停帧）。
    #[tokio::test]
    async fn relay_sub_heartbeat_timeout_prunes() {
        let (hub_a, h_a) = fresh();
        let (hub_b, h_b) = fresh();
        let fed_a = h_a.federation();
        let fed_b = h_b.federation();
        interconnect(&fed_a, &fed_b, Arc::new(Mutex::new(Vec::new())));

        let (room_id, token) = create_room(&h_a, "心跳", "camera").await;
        let _ = hub_a.attach_publisher(&room_id, &token).unwrap();
        let fed_id = fed_room_id(NODE_A_HEX, &room_id);
        let _ = hub_b.subscribe(&fed_id).expect("B 订阅");
        assert_eq!(relay_sub_total(&fed_a), 1);
        // 91s 后巡检（合成 now）：心跳超时剪除
        fed_a
            .inner
            .sweep_once(std::time::Instant::now() + std::time::Duration::from_secs(91));
        assert_eq!(relay_sub_total(&fed_a), 0, "心跳超时即剪除（节点失联停中继）");
    }

    /// F7.（真机联调发现，106/113 跨节点联调）心跳任务的收养/退场/重起语义：
    ///
    /// 修复的竞态是「心跳任务判死 → 防重表摘牌」窗口内影子房间被重建时，
    /// 新订阅拿不到心跳任务（spawn_heartbeat 被旧条目挡住）→ 源端 90s 剪除
    /// 中继 → 在看观众断流。该窗口在任务内为无 await 的同步段，黑盒编排
    /// 无法插入复现（检查与摘牌之间不可调度），故本测试锁定修复后的可观测
    /// 语义防回归：① 退订→回订（任务睡眠期重建）心跳继续刷新源端；② 彻底
    /// 无人观看后心跳任务退场不再发 sub；③ 摘牌后再次回订能起新心跳。
    /// 心跳周期按实例缩至 30ms（真时间快进，无需 tokio test-util）。
    #[tokio::test]
    async fn relay_heartbeat_task_adopts_recreated_shadow_room() {
        let (hub_a, h_a) = fresh();
        let (hub_b, h_b) = fresh();
        let fed_a = h_a.federation();
        let fed_b = h_b.federation();
        let tick = Duration::from_millis(30);
        fed_b.set_heartbeat_interval_for_test(tick);
        let settle = Duration::from_millis(200);
        let log: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        interconnect(&fed_a, &fed_b, log.clone());

        let (room_id, token) = create_room(&h_a, "心跳收养", "screen").await;
        let _ = hub_a.attach_publisher(&room_id, &token).unwrap();
        let fed_id = fed_room_id(NODE_A_HEX, &room_id);
        let sub_count = || {
            log.lock()
                .expect("log")
                .iter()
                .filter(|p| p["fed"] == FED_KIND_LIVE_RELAY_SUB && p["room_id"] == fed_id)
                .count()
        };

        // ① 初订：影子房间 + 心跳任务；走多个心跳 tick
        let (v1, _rx1) = hub_b.subscribe(&fed_id).expect("B 初订");
        tokio::time::sleep(settle).await;
        let base = sub_count();
        assert!(base >= 2, "初订 1 次 + 至少 1 次心跳刷新: {base}");
        assert_eq!(relay_sub_total(&fed_a), 1, "源端订阅在表");

        // ② 退订后立即回订（心跳任务尚在睡眠）：醒来须收养新影子房间继续刷新
        hub_b.unsubscribe(&fed_id, v1);
        let (v2, _rx2) = hub_b.subscribe(&fed_id).expect("B 回订（重建影子房间）");
        tokio::time::sleep(settle).await;
        assert!(
            sub_count() > base,
            "心跳任务收养重建的影子房间并继续刷新（{base} → {}）",
            sub_count()
        );
        assert_eq!(relay_sub_total(&fed_a), 1, "回订后源端订阅仍在（未被剪除）");

        // ③ 彻底退订：心跳任务在下一 tick 判死（双检）→ 摘牌退场，不再发 sub
        hub_b.unsubscribe(&fed_id, v2);
        tokio::time::sleep(settle).await;
        let stopped = sub_count();
        tokio::time::sleep(settle).await;
        assert_eq!(
            sub_count(),
            stopped,
            "心跳任务已退场（防重表已摘牌），不再刷新"
        );

        // ④ 摘牌后再次回订：能起新心跳任务（防重表已清，不被旧条目挡住）
        let (_v3, _rx3) = hub_b.subscribe(&fed_id).expect("B 再订");
        tokio::time::sleep(settle).await;
        assert!(
            sub_count() > stopped,
            "新心跳任务已起并刷新（{stopped} → {}）",
            sub_count()
        );
    }

    // ---- WS e2e（真实 axum serve + tungstenite 握手，同 terminal WS 手法）----

    use crate::gateway::Gateway as _;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    /// 对 url 发起 WS 握手，返回服务端 HTTP 状态（101 成功；拒绝路径由
    /// tungstenite 以 Error::Http(resp) 携带状态码）。
    async fn ws_handshake_status(url: &str) -> u16 {
        let req = url.to_string().into_client_request().unwrap();
        match tokio_tungstenite::connect_async(req).await {
            Ok((_stream, resp)) => resp.status().as_u16(),
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => resp.status().as_u16(),
            Err(other) => panic!("非预期握手错误: {other:?}"),
        }
    }

    /// 在共享 hub 建房（REST 层直调 handler，绕过 HTTP 鉴权中间件；WS 升基层
    /// 消费的是同一 `LiveHub::shared()` 实例）。
    async fn shared_room(title: &str) -> (String, String) {
        let h = LiveRouteHandler::new();
        create_room(&h, title, "screen").await
    }

    /// 轮询共享 hub 列表直至谓词命中（2s 超时，验 rooms 异步收敛）。
    async fn wait_rooms(pred: impl Fn(&[LiveRoom]) -> bool) -> Vec<LiveRoom> {
        for _ in 0..40 {
            let rooms = LiveRouteHandler::new().hub_list();
            if pred(&rooms) {
                return rooms;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("房间状态 2s 内未收敛");
    }

    /// 主播错误 token / 不存在房间：升级前 401 / 404（客户端拿 HTTP 状态）。
    #[tokio::test]
    async fn live_ws_publish_rejects_bad_token_and_missing_room() {
        let (id, _token) = shared_room("WS 鉴权").await;
        let gw = crate::InProcessGateway::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        gw.start(&format!("127.0.0.1:{}", addr.port()), None)
            .await
            .expect("start");

        let base = format!("ws://127.0.0.1:{}/ws/live", addr.port());
        assert_eq!(
            ws_handshake_status(&format!("{base}/{id}/publish?token=lt-wrong")).await,
            401,
            "错 publish token 必须 401"
        );
        assert_eq!(
            ws_handshake_status(&format!("{base}/live-424242/publish?token=x")).await,
            404,
            "房间不存在 404"
        );
        assert_eq!(
            ws_handshake_status(&format!("{base}/live-424242/view")).await,
            404,
            "观众接入不存在房间 404"
        );
        assert_eq!(
            ws_handshake_status(&format!("{base}/{id}/dance")).await,
            404,
            "未知动作 404"
        );
    }

    /// 全链路：主播 token 握手 → 二进制帧扇出 → 中途观众收 header 重放 + 实时帧
    /// → stop 控制帧 → 观众收 ended 收尾。
    #[tokio::test]
    async fn live_ws_e2e_publish_view_roundtrip() {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message as WsMessage;

        let (id, token) = shared_room("WS e2e").await;
        let gw = crate::InProcessGateway::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        gw.start(&format!("127.0.0.1:{}", addr.port()), None)
            .await
            .expect("start");
        let base = format!("ws://127.0.0.1:{}/ws/live/{id}", addr.port());

        // 主播握手（token 精确匹配 → 101）并发送首个 chunk（init segment）
        let req = format!("{base}/publish?token={token}")
            .into_client_request()
            .unwrap();
        let (mut pub_ws, resp) = tokio_tungstenite::connect_async(req)
            .await
            .expect("主播握手");
        assert_eq!(resp.status().as_u16(), 101);
        pub_ws
            .send(WsMessage::Binary(b"INIT-SEG".to_vec().into()))
            .await
            .unwrap();

        // 等扇出引擎缓存 header（中途观众重放的前提）
        wait_rooms(|rs| rs.iter().any(|r| r.id == id && r.header_cached)).await;

        // 观众中途接入：先收缓存的 header，再收后续实时帧
        let req = format!("{base}/view").into_client_request().unwrap();
        let (mut view_ws, resp) = tokio_tungstenite::connect_async(req)
            .await
            .expect("观众握手");
        assert_eq!(resp.status().as_u16(), 101);
        let m = tokio::time::timeout(std::time::Duration::from_secs(2), view_ws.next())
            .await
            .expect("等 header 重放")
            .expect("连接未断")
            .expect("帧合法");
        assert_eq!(m.into_data(), b"INIT-SEG".to_vec(), "中途观众先收 header");

        pub_ws
            .send(WsMessage::Binary(b"CLUSTER-1".to_vec().into()))
            .await
            .unwrap();
        let m = tokio::time::timeout(std::time::Duration::from_secs(2), view_ws.next())
            .await
            .expect("等实时帧")
            .expect("连接未断")
            .expect("帧合法");
        assert_eq!(m.into_data(), b"CLUSTER-1".to_vec(), "实时帧顺序下发");

        // 真实计数：1 观众在线、上行字节全计、下行只计成功投递
        let rooms = wait_rooms(|rs| rs.iter().any(|r| r.id == id && r.viewer_count == 1)).await;
        let room = rooms.iter().find(|r| r.id == id).unwrap();
        assert!(
            room.bytes_in >= (8 + 9) as u64,
            "bytes_in: {}",
            room.bytes_in
        );
        assert!(room.bytes_out >= 9, "bytes_out: {}", room.bytes_out);

        // stop 控制帧 → 主播断开 → 观众收 {"kind":"ended"}
        pub_ws
            .send(WsMessage::Text(r#"{"kind":"stop"}"#.into()))
            .await
            .unwrap();
        let m = tokio::time::timeout(std::time::Duration::from_secs(2), view_ws.next())
            .await
            .expect("等 ended 帧")
            .expect("连接未断")
            .expect("帧合法");
        let text = m.into_text().expect("应为文本帧").as_str().to_string();
        assert!(text.contains(r#""kind":"ended""#), "ended 控制帧: {text}");

        // 观众断开 → 房间无主播且观众清零 → 回收出表
        drop(view_ws);
        wait_rooms(|rs| rs.iter().all(|r| r.id != id)).await;
    }

    /// 全链路（联邦形态）：远端房间宣告合并进联邦表 → 真 WS 观众连联邦形态
    /// id（影子房间 + live_relay_sub 发出）→ 手动驱动中继帧 dispatch → 观众收
    /// header + 实时帧 → ended 控制帧 → 观众收 {"kind":"ended"} → 断开发
    /// live_relay_unsub。共享 hub/联邦端点（与 http.rs WS 挂载同一实例）。
    #[tokio::test]
    async fn live_ws_e2e_federated_shadow_room_view() {
        use futures::StreamExt;

        // 共享联邦端点装 fake overlay：全部外发载荷进捕获日志（本测试手动
        // 驱动 dispatch 模拟源节点回传）。
        let inbox: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let log1 = inbox.clone();
        let send_to: FedSendFn = Arc::new(move |_to, p| {
            log1.lock().expect("log").push(p);
        });
        let log2 = inbox.clone();
        let broadcast: FedBroadcastFn = Arc::new(move |p| {
            log2.lock().expect("log").push(p);
        });
        let fed = LiveFedEndpoint::shared();
        fed.install(send_to, broadcast, NODE_B_HEX.into(), "node-b".into());

        // 合并 node-a 的房间宣告（真实 merge 路径）→ 联邦大厅可 see
        let a_node = test_node(NODE_A_HEX);
        let announce =
            build_live_lobby_payload(NODE_A_HEX, "node-a", &room_view_fixture("live-777", "远端直播", 2));
        fed.dispatch(&a_node, &announce);
        let fed_id = fed_room_id(NODE_A_HEX, "live-777");
        assert_eq!(fed.federated_rooms()[0].id, fed_id);

        // 真 WS：观众 view 连联邦形态 id（影子房间自动建立 + 中继订阅发出）
        let gw = crate::InProcessGateway::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        gw.start(&format!("127.0.0.1:{}", addr.port()), None)
            .await
            .expect("start");
        let url = format!("ws://127.0.0.1:{}/ws/live/{fed_id}/view", addr.port());
        let req = url.into_client_request().unwrap();
        let (mut view_ws, resp) = tokio_tungstenite::connect_async(req)
            .await
            .expect("联邦房间观众握手（影子房间）");
        assert_eq!(resp.status().as_u16(), 101);
        // 中继订阅已定向发出（载荷完整）
        assert!(
            inbox
                .lock()
                .expect("log")
                .iter()
                .any(|p| p["fed"] == FED_KIND_LIVE_RELAY_SUB
                    && p["room_id"] == fed_id
                    && p["node_id"] == NODE_B_HEX),
            "live_relay_sub 已发出"
        );

        // 模拟源节点回传中继帧（经真实 dispatch → 重组 → 注入扇出）
        fed.dispatch(
            &a_node,
            &build_relay_frame_payload("node-a", &fed_id, 1, 0, 1, b"FED-INIT", false),
        );
        let m = tokio::time::timeout(std::time::Duration::from_secs(2), view_ws.next())
            .await
            .expect("等联邦 header")
            .expect("连接未断")
            .expect("帧合法");
        assert_eq!(m.into_data(), b"FED-INIT".to_vec(), "联邦观众先收 header");

        // 双块帧（重组后完整下发）
        let half = RELAY_CHUNK_BYTES / 2;
        let big: Vec<u8> = (0..half * 2).map(|i| (i % 249) as u8).collect();
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut p1 = build_relay_frame_payload("node-a", &fed_id, 2, 0, 2, &big[..half], false);
        p1["bytes"] = serde_json::Value::String(b64.encode(&big[..half]));
        fed.dispatch(&a_node, &p1);
        // 未集齐：无帧可收
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), view_ws.next())
                .await
                .is_err(),
            "分块未齐不下发"
        );
        let p2 = build_relay_frame_payload("node-a", &fed_id, 2, 1, 2, &big[half..], false);
        fed.dispatch(&a_node, &p2);
        let m = tokio::time::timeout(std::time::Duration::from_secs(2), view_ws.next())
            .await
            .expect("等重组帧")
            .expect("连接未断")
            .expect("帧合法");
        assert_eq!(m.into_data(), big, "双块重组后完整下发");

        // 源端结束 → ended 控制帧 → 观众收 {"kind":"ended"} 收尾
        fed.dispatch(
            &a_node,
            &build_relay_frame_payload("node-a", &fed_id, 2, 0, 0, &[], true),
        );
        let m = tokio::time::timeout(std::time::Duration::from_secs(2), view_ws.next())
            .await
            .expect("等 ended 帧")
            .expect("连接未断")
            .expect("帧合法");
        let text = m.into_text().expect("应为文本帧").as_str().to_string();
        assert!(text.contains(r#""kind":"ended""#), "ended 控制帧: {text}");

        // 观众断开 → 影子房间出表 + live_relay_unsub 发出（退订即停帧）
        view_ws.close(None).await.ok();
        for _ in 0..20 {
            if inbox
                .lock()
                .expect("log")
                .iter()
                .any(|p| p["fed"] == FED_KIND_LIVE_RELAY_UNSUB && p["room_id"] == fed_id)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            inbox
                .lock()
                .expect("log")
                .iter()
                .any(|p| p["fed"] == FED_KIND_LIVE_RELAY_UNSUB && p["room_id"] == fed_id),
            "观众断开 → live_relay_unsub"
        );
    }
}
