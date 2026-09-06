//! `ImRouteHandler` —— 把 os-im 对话/群组/Federation 功能暴露为 HTTP REST API
//! （规划文档 §3.6 / §3.7 / §9.1#10）。
//!
//! 定位：让 Web UI / CLI / MCP 能经 HTTP 调用 IM 能力——对话/消息、群组、
//! Federation 节点（peers）、IM 服务状态，并经 WebSocket 实时推送新消息。
//!
//! # 当前实现策略：SQLite 持久化 + WebSocket 实时推送
//!
//! 对话/消息/群组/节点全部落 SQLite（`Mutex<Connection>` 短锁快查快放，
//! 参考 [`crate::handlers::api_gateway`] 的模式）。发消息成功后经
//! [`crate::ws_impl::WsHub`] 广播 `WsMessage::ImMessage`，前端 WebSocket
//! 客户端收到后追加到当前对话——取代 3s 轮询。
//!
//! # 区块链认证（设计 docs/IM_BLOCKCHAIN_AUTH_DESIGN.md，2026-08-17 决策）
//!
//! IM 用户身份 = secp256k1 公钥（压缩格式 `0x` + 66 hex），唯一认证方式是
//! 挑战-签名（私钥永不出客户端），展示名 = 公钥派生 EVM 地址：
//!
//! 1. `POST /api/v1/im/auth/challenge {pubkey}` → `{nonce}`（60s 单次有效）
//! 2. 客户端用私钥对 nonce 的 UTF-8 字节做 ECDSA 签名（65 字节 r||s||v hex）
//! 3. `POST /api/v1/im/auth/verify {pubkey, nonce, signature}` → `{token}`（24h）
//! 4. REST：`Authorization: Bearer <token>`；WS：`?user=<pubkey>&token=<token>`
//!    （握手即验，失败 401——旧裸 `?user=` 一律拒绝，一次性破坏性变更）
//!
//! 所有用户身份端点的 sender/user 由服务端从 token 反查 pubkey 填充，
//! 请求体/查询参数里的自报身份一律忽略。nonce 桶与 token 桶在内存
//! （[`ImAuth`]，重启失效可接受，客户端自动重认证）；单点登录：新 verify
//! 顶掉旧 token。系统级 `NEXOS_ADMIN_TOKEN` 仅用于管理端点（POST /peers），
//! 与 IM 用户身份正交。
//!
//! # 离线消息补拉（2026-08-20 无人值守韧性批次）
//!
//! WebSocket 断线重连期间错过的大厅/会话消息，经 HTTP 增量端点补齐：
//! `GET /api/v1/im/messages?conversation_id=<cid>&after_id=<本地最后一条消息 id>`
//! ——返回按插入序（rowid）**升序**、**严格晚于** after_id 的消息，
//! `limit` 默认 50、上限 200（钳制）。大厅同语义走既有
//! `GET /api/v1/im/lobby/messages?after_id=`。after_id 是消息 id（字符串，
//! 服务端映射为插入序 rowid 比较），未知/缺省 after_id → 从头升序取
//! limit 条。客户端拿到缺口后按 id 去重追加即可。
//!
//! # 多 AI agent 接入 + 文档传输（2026-08-21，设计 docs/IM_AGENTS_AND_FILES.md）
//!
//! 在链上身份之上叠加三类能力（全部向后兼容——新字段 serde default，存量
//! 消息/客户端零迁移）：
//!
//! 1. **agent 可见性（sender_kind）**：消息加 `sender_kind: "human"|"agent"`
//!    （默认 human）。外部 agent（Windows 演示机 / PPT agent 等）用与人类
//!    完全相同的链上身份三步认证 + REST/WS 通道发消息，body 自带
//!    `sender_kind:"agent"` 即可在前端渲染为 AI 身份。**信任边界**：该字段
//!    是展示层自声明语义——任何持有效 token 的调用方都可声明 agent（服务端
//!    只白名单归一 human/agent，不做强校验）；消息归因仍以 token 反查的
//!    pubkey 为准。WS 广播帧（ImMessage/ImLobbyMessage 的 message 体）与
//!    补拉/历史端点原样透传该字段。
//! 2. **@mention + 内置助手**：发消息时服务端解析 `@<名字>`（名字规则
//!    `[一-龥A-Za-z0-9_-]{1,42}`）落到 `mentions` 列。`@NexOS助手` 触发
//!    内置 agent：spawn 异步任务 → 剥掉 @ 的正文 POST 到本地推理
//!    （`NEXOS_IM_AGENT_LLM_URL` 覆盖，默认
//!    `http://127.0.0.1:8000/v1/chat/completions`；模型
//!    `NEXOS_IM_AGENT_MODEL` 默认 `qwen3.5-9b`；不可达回固定话术）→ 以
//!    sender_kind=agent 回同会话一条消息（≤800 字截断 + "（AI 生成）"后缀）。
//!    防风暴：同会话 3s 窗口内多条 @ 只响应最后一条（代次去抖）；agent
//!    消息不触发（防自激）。
//! 3. **文档传输（附件）**：`POST /api/v1/im/files` 上传（JSON 通道
//!    base64，≤64MiB，落 `/tank/im-files/<YYYYMM>/<uuid>-<净化名>`）；
//!    `GET /api/v1/im/files/:file_id?token=` 下载（IM token 头/查询或
//!    admin token，base64 信封 + Content-Disposition）。发消息可带
//!    `attachment:{file_id,...}`——服务端按 file_id 核对存在性并用**落盘
//!    真值覆盖 size/filename**（伪造无效）。
//!
//! # 消息推送通知 webhook（2026-08-22，消除外部 agent 轮询）
//!
//! 「IM 一有消息，自动通知所有参与的 AI agent」——agent 注册一个 HTTP
//! 接收端点（`POST /api/v1/im/notify/register`，链上 token 身份，
//! owner=pubkey），消息成功写入后服务端对所有匹配的 webhook
//! `tokio::spawn` 异步 POST（**完全不阻塞消息路径**，与内置助手同款）：
//!
//! - body = 完整 Message JSON（含 sender_kind/mentions/attachment；
//!   **不含任何 token**——接收方拿不到敏感凭证）；
//! - Header `X-NexOS-Event: lobby_message | conversation_message`；
//! - 事件过滤 `events:["lobby","conversation"]`（缺省双开）；
//!   `conversation_id` 可选绑定单个会话（缺省=全部会话）；
//! - 超时 5s；失败计连败（`fail_count`），连败 ≥5 次自动注销
//!   （`status=disabled` + `last_error` 记录原因，重新注册即可恢复）；
//! - 管理：`GET /im/notify/list`（只看自己的）/ `DELETE /im/notify/:id`
//!   （仅 owner 可注销）。
//!
//! # 联邦接收开关（2026-08-23）
//!
//! 用户可开/关 IM 联邦的**接收**：关闭后本节点不再落地其他节点的联邦大厅
//! 消息（[`ImFederation::ingest`] 入口短路返回 `Paused`，不写库、不 WS 广播），
//! **本地消息与联邦发送完全不受影响**（`federate_fed_lobby_message` 照常广播）；
//! 重新打开即恢复接收。开关是进程内原子布尔（默认开，重启回默认），经
//! `GET/POST /api/v1/im/federation` 读写（详见路由表与端点注释）。
//!
//! # 联邦大厅＝独立可写会话（2026-08-23 用户纠正批次）
//!
//! 联邦大厅**不是**只读聚合流，是与「我的大厅」**互相隔离**的独立会话
//! （conversation_id 恒为 [`FED_LOBBY_ID`] = "fed-lobby"）：
//!
//! - **我的大厅（lobby）**＝本节点的房间：本地用户 + 远程用户（经节点发现
//!   `im_lobby_post` 直接进入）说话，消息只留本节点（**不再**自动联邦广播）；
//! - **联邦大厅（fed-lobby）**＝跨节点共享频道：`POST /api/v1/im/fed-lobby/
//!   messages` 本地落库 + P2P 广播全部已连接 peer（新 fed 载荷
//!   `im_fed_lobby_message`）；其他节点收到后落本地 fed-lobby 会话
//!   （sender_id=`fed:<node>:<pubkey>`，sender_name 带 🌐 来源标注）；
//! - **远程节点大厅**＝对方节点的「我的大厅」（经 P2P 进入，不变）。
//!
//! # 联邦大厅发言时延节流（2026-08-24）
//!
//! 联邦大厅可**一直发言、不限次数、永不拒绝**，但联邦广播带时延（仅延后
//! 广播时刻，消息永不丢弃）：本地落库 + 本节点 WS 广播**即时**；P2P 联邦
//! 广播经延迟队列发出——常态每条 10s（[`FED_THROTTLE_SHORT`]），同一发送者
//! 60s 计数窗口内第二次发言起升为 60s（[`FED_THROTTLE_LONG`]），安静满 60s
//! 回落 10s。状态机 [`FedThrottle`]（进程内存态，多实例/重启各自独立，可
//! 接受）；发送响应透出 `federate_delay_secs`（10/60，非联邦消息 0）与
//! `note` 说明。
//!
//! 兼容迁移：旧版把 `fed == "im_lobby"` 广播落进 lobby——现 [`ImFederation::ingest`]
//! 同时接受旧 `im_lobby` 与新 `im_fed_lobby_message` 载荷，**一律落 fed-lobby**
//! （不再污染我的大厅）；旧版 POST /lobby/messages 的自动联邦广播已删除。
//!
//! # 大厅开放开关 + 远程大厅浏览/发言（2026-08-23，节点发现页联动）
//!
//! 每个节点可决定是否**允许其他 NexOS 节点浏览本机 IM 大厅**——开发前期
//! 缺省**允许**（[`ImShared::lobby_public`]，进程内原子布尔，经
//! `GET/POST /api/v1/im/lobby/access` 读写；发版前再评估默认值）。在此之上
//! 叠加两个 P2P 查询通道载荷（走 os-p2p 加密链路）：
//!
//! - `{"fed":"im_lobby_query","node":…,"req_id":…}`——对方发起浏览请求；本机
//!   开关开 → 回 `im_lobby_reply`（最近 [`LOBBY_VIEW_LIMIT`] 条**脱敏**消息，
//!   只含文本与元数据，**不含 attachment/file_url/read_by**）；关 → 回
//!   `{"public":false,"error":"denied"}`。查询端（[`ImLobbyProbe`]）缓存应答
//!   30s 限频，节点发现页 combined 端点据此标注 `im_public`，IM 页据此渲染
//!   远程大厅只读镜像；
//! - `{"fed":"im_lobby_post","node":…,"sender_id":…,"content":…}`——远程发言
//!   （IM 页远程大厅 Tab 的输入框）：对方开关开（且联邦接收未暂停）才落地
//!   本机大厅（`fed:<节点>:<pubkey>` 前缀，不承载附件），否则静默丢弃。
//!
//! HTTP 面：`GET /api/v1/im/lobby/remote/:node_id`（阻塞查询对方开放状态 +
//! 消息镜像）与 `POST /api/v1/im/lobby/remote/:node_id/messages`（先查状态，
//! denied → 403；超时 → 504；开放 → 经 P2P 发言）。
//!
//! # 点对点直通消息 DM（2026-08-30，dm-* 会话）
//!
//! 大厅保持现状（各人的大厅不动）之外，新增**直通消息通道**：A 可直接向
//! 某个链上身份 B 发私信——不经大厅广播，只有双方可见：
//!
//! - **开关**：[`ImShared::dm_open`]（进程内原子布尔，开发期缺省 **true**，
//!   `GET/POST /api/v1/im/dm/access` 读写，语义同 lobby access 端点）。false =
//!   其他身份发来的 DM 一律不收（本地 POST /im/dm 403；跨节点 ingest 丢弃）；
//!   自己发出的 DM 不受影响；
//! - **会话确定性**：双方共用同一会话 id [`dm_conversation_id`]
//!   （`dm-` + sha256(排序后双方 pubkey) 前 8 字节 hex——与发起方向无关，
//!   双端各自落库天然同 id）；成员表 `im_dm_members`（双方各一行）；
//! - **本节点投递**：对方身份在本节点（大厅在场或 WS 在线订阅，见
//!   [`ImShared::identity_local`]）→ 落库 + **定向 WS 推送**（`send_to_n`
//!   按 pubkey，只有收发双方收到——区别于会话消息的全员广播）；
//! - **跨节点定向路由**：对方不在本节点 → 经 P2P overlay **定向发送**到对方
//!   节点（fed kind [`FED_KIND_IM_DM`]，载荷 `{from_pubkey, from_name,
//!   to_pubkey, content, node, ts, msg_id}`——非广播，只有目标节点收到）；
//!   对方节点 ingest（[`ImFederation::ingest_dm`]）→ dm_open 检查 → 落库
//!   （同确定性 id）→ 定向 WS 推给收件人。**回程路由**：ingest 顺带把
//!   发送方 pubkey → 发送方 NodeID 登记 `im_dm_peers`（P2P 层验签真值），
//!   收件人回复时 POST /im/dm 自动按登记路由回原节点（无需带 to_node）；
//! - **去重**：跨节点消息 id = 载荷 hash（[`dm_message_id`]）——同一条消息
//!   重投/回环双端只落一份（fed_seen 内存缓存 + DB 查重双兜底）；
//! - **可见性**：`GET /im/conversations` 只列自己是成员的 dm-* 会话（members
//!   感知）；dm 历史读取（补拉/搜索/`GET /conversations/:id/messages`）与
//!   发言统一走成员校验（非成员 403；发言端点对 dm 会话禁用——DM 发送唯一
//!   入口是 POST /im/dm，dm_open 开关不旁路）。
//!
//! # agent 协调组件挂钩（2026-08-24，agent-coord）
//!
//! 会话/群发消息与大厅发消息在**落库 + WS 广播后**各加**一行**
//! `crate::handlers::agent_coord::on_im_message(&msg)`——@ 定向投递给命中的
//! 注册 agent（在线 WS 留痕 / 离线收件箱+webhook）。钩子是进程级单例，
//! main.rs 装配 agent-coord 时注入，未装配时 no-op（本模块零额外状态）。
//! 反向（协议声明系统消息直插）经 [`ImCoordBridge`] 桥注入，见
//! docs/AGENT_COORDINATION.md。
//!
//! # 路由表（36 条，component="im"）
//!
//! | method | path                                         | 动作 |
//! |--------|----------------------------------------------|------|
//! | POST   | `/api/v1/im/auth/challenge`                   | 签发 nonce（公开）|
//! | POST   | `/api/v1/im/auth/verify`                      | 验签发 token（公开）|
//! | GET    | `/api/v1/im/conversations`                   | 列出对话（IM token）|
//! | POST   | `/api/v1/im/conversations`                   | 创建对话（created_by=pubkey）|
//! | GET    | `/api/v1/im/conversations/:id/messages`      | 对话消息历史（IM token）|
//! | POST   | `/api/v1/im/conversations/:id/messages`      | 发送消息（sender=pubkey，广播 WS）|
//! | GET    | `/api/v1/im/messages`                        | 离线补拉：`?conversation_id=&after_id=&limit=`（IM token；群组/大厅非成员 403）|
//! | GET    | `/api/v1/im/groups`                          | 列出群组（IM token）|
//! | POST   | `/api/v1/im/groups`                          | 创建群组（owner=pubkey）|
//! | POST   | `/api/v1/im/groups/:id/join`                 | 加入群组（member=pubkey）|
//! | POST   | `/api/v1/im/groups/:id/leave`                | 退出群组（member=pubkey）|
//! | GET    | `/api/v1/im/groups/:id/members`              | 群组成员（IM token）|
//! | GET    | `/api/v1/im/peers`                           | 已连接 Federation 节点（公开）|
//! | POST   | `/api/v1/im/peers`                           | 添加节点（系统级认证）|
//! | GET    | `/api/v1/im/status`                          | IM 服务状态（公开）|
//! | POST   | `/api/v1/im/messages/:id/read`               | 标记消息已读（user=pubkey）|
//! | GET    | `/api/v1/im/conversations/:id/unread`        | 对话未读数（user=pubkey）|
//! | GET    | `/api/v1/im/search`                          | 搜索消息 `?q=&conversation_id=<可选>&limit=`（缺省搜大厅；IM token）|
//! | GET    | `/api/v1/im/lobby`                            | 大厅信息 + 心跳（IM token）|
//! | GET    | `/api/v1/im/lobby/messages`                   | 大厅最近 50 条 + 心跳；`?after_id=` 增量同语义（IM token）|
//! | POST   | `/api/v1/im/lobby/messages`                   | 发大厅消息（sender=pubkey，广播 WS）|
//! | GET    | `/api/v1/im/lobby/members`                    | 大厅成员（IM token）|
//! | POST   | `/api/v1/im/files`                            | 上传 IM 附件（IM token；base64-JSON ≤64MiB）|
//! | GET    | `/api/v1/im/files/:file_id`                   | 下载附件（IM token 头/`?token=` 或 admin token）|
//! | POST   | `/api/v1/im/notify/register`                  | 注册推送 webhook（IM token；owner=pubkey）|
//! | GET    | `/api/v1/im/notify/list`                      | 列出自己的 webhook（IM token；owner 过滤）|
//! | DELETE | `/api/v1/im/notify/:id`                       | 注销 webhook（IM token；仅 owner）|
//! | GET    | `/api/v1/im/federation`                       | 联邦接收开关状态（IM token）|
//! | POST   | `/api/v1/im/federation`                       | 切换联邦接收开关（admin 或 IM token）|
//! | GET    | `/api/v1/im/lobby/access`                     | 大厅开放开关状态（admin 或 IM token）|
//! | POST   | `/api/v1/im/lobby/access`                     | 切换大厅开放开关（admin 或 IM token）|
//! | GET    | `/api/v1/im/lobby/remote/:node_id`            | 远程大厅镜像：开放状态 + 最近 20 条脱敏消息（IM token；`?timeout_ms=` 300..=8000 默认 4000）|
//! | POST   | `/api/v1/im/lobby/remote/:node_id/messages`   | 远程大厅发言（IM token；对方未开放 403 / 无应答 504）|
//! | GET    | `/api/v1/im/fed-lobby`                        | 联邦大厅信息 + 心跳加入（IM token）|
//! | GET    | `/api/v1/im/fed-lobby/messages`               | 联邦大厅最近 50 条 / `?after_id=` 增量（IM token）|
//! | POST   | `/api/v1/im/fed-lobby/messages`               | 联邦大厅发言（IM token；本地落库 + P2P 广播全部 peer）|
//! | GET    | `/api/v1/im/dm/access`                        | 直通消息开放开关状态（admin 或 IM token）|
//! | POST   | `/api/v1/im/dm/access`                        | 切换直通消息开放开关（admin 或 IM token）|
//! | POST   | `/api/v1/im/dm`                               | 发起点对点直通消息 {to_pubkey, content, to_node?}（IM token）|

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use once_cell::sync::Lazy;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};
use crate::websocket::WsMessage;
use crate::ws_impl::WsHub;

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 对话（im_conversations 行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub name: String,
    /// 0=单聊，1=群聊。
    #[serde(default)]
    pub is_group: bool,
    /// 创建者 id。
    #[serde(default)]
    pub created_by: Option<String>,
    pub created_at: String,
    /// DM（dm-* 会话）成员 = 双方 pubkey（[`im_dm_members`] 表；群组/普通
    /// 对话恒空——群组成员模型在 im_group_members，前端据 members 判定
    /// 「对方是谁」与私聊路由）。
    #[serde(default)]
    pub members: Vec<String>,
}

/// 单条消息（im_messages 行）。
///
/// 字段命名与前端 `ImMessage`（`web/src/api/types.ts`）对齐：
/// `sender_id` / `sender_name` / `conversation_id` / `created_at` / `content`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub conversation_id: String,
    pub sender_id: String,
    /// 发送者显示名（None 时前端回退到 sender_id）。
    #[serde(default)]
    pub sender_name: Option<String>,
    pub content: String,
    /// `text` / `file` / `image` / `system`。
    #[serde(default = "default_msg_type_text")]
    pub msg_type: String,
    #[serde(default)]
    pub file_url: Option<String>,
    /// 被回复消息 id。
    #[serde(default)]
    pub reply_to: Option<String>,
    pub created_at: String,
    /// 已读用户 id 列表（JSON 数组持久化）。
    #[serde(default)]
    pub read_by: Vec<String>,
    /// 发送者类别：`human`（默认）| `agent`（AI 代理）| `system`（本地系统
    /// 消息，如身份冲突警告——仅服务端自产）。
    ///
    /// **展示层自声明语义**（2026-08-21 agent 批次）：发消息 body 可带，
    /// 服务端只做白名单归一（非 `agent`/`system` 一律存 `human`），不校验
    /// 声明者是否真是 agent——归因仍以 token 反查 pubkey 为准（信任边界见
    /// 模块注释）。存量消息缺该列 → serde default `human`。
    #[serde(default = "default_sender_kind_human")]
    pub sender_kind: String,
    /// @ 提及的名字列表（服务端从 content 解析 `@<名字>`，去重保序；
    /// JSON 列持久化）。触发内置助手看 [`NEXOS_ASSISTANT`]。
    #[serde(default)]
    pub mentions: Vec<String>,
    /// 附件元数据（发消息时服务端按 file_id 核对落盘真值后落库；无附件 None）。
    #[serde(default)]
    pub attachment: Option<Attachment>,
}

/// IM 消息附件（服务端核对后的真值——`size_bytes`/`filename` 以
/// [`crate::handlers::im`] 上传落盘记录为准，客户端自报值被覆盖）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub file_id: String,
    pub filename: String,
    pub size_bytes: u64,
    /// MIME（上传时按扩展名猜测，发消息可覆盖）。
    #[serde(default)]
    pub mime: Option<String>,
}

/// 发消息 body 里的自报附件（仅 file_id 必填——filename/size 以服务端为准）。
#[derive(Debug, Clone, Deserialize)]
struct AttachmentReq {
    file_id: String,
    #[serde(default)]
    #[allow(dead_code)] // 自报值一律被服务端真值覆盖（防伪造），字段仅作兼容解析
    filename: Option<String>,
    #[serde(default)]
    #[allow(dead_code)] // 同上：伪造 size 无效
    size_bytes: Option<u64>,
    #[serde(default)]
    mime: Option<String>,
}

/// IM 附件落盘记录（im_files 行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImFileRecord {
    pub file_id: String,
    /// 净化后的原始文件名（展示/下载 Content-Disposition 用）。
    pub filename: String,
    pub size_bytes: u64,
    pub mime: Option<String>,
    /// 上传者 pubkey（链上身份）。
    pub uploader: Option<String>,
    /// 落盘绝对路径。
    pub path: String,
    pub created_at: String,
}

/// 附件下载信封（`GET /api/v1/im/files/:file_id` 响应体，与 files.rs
/// download 的 base64 JSON 信封同款先例——网关响应恒 JSON，无法回裸流）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImFileDownload {
    pub file_id: String,
    pub filename: String,
    pub size_bytes: u64,
    pub mime_type: String,
    /// 恒为 "base64"。
    pub encoding: String,
    pub content_base64: String,
}

/// 消息推送 webhook（im_webhooks 行；docs/IM_AGENTS_AND_FILES.md §7）。
///
/// owner = 注册时的 token 反查 pubkey（链上身份）；匹配的消息成功写入后
/// 服务端异步 POST 完整 Message JSON 到 `url`（Header `X-NexOS-Event`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImWebhook {
    pub id: String,
    /// agent 的 HTTP 接收端点（POST 目标，http/https）。
    pub url: String,
    /// 注册者 pubkey（token 反查，自报值一律忽略）。
    pub owner_pubkey: String,
    /// 订阅事件：`lobby`（大厅新消息）/ `conversation`（会话新消息），
    /// JSON 列持久化。缺省（注册时不传）= 双开。
    #[serde(default = "default_webhook_events_all")]
    pub events: Vec<String>,
    /// 绑定单个会话（仅 conversation 事件生效；None=全部会话）。
    #[serde(default)]
    pub conversation_id: Option<String>,
    /// `active` / `disabled`（连败 ≥5 次自动注销）。
    #[serde(default = "default_webhook_active")]
    pub status: String,
    /// 连败计数（成功投递清零）。
    #[serde(default)]
    pub fail_count: u32,
    /// 最近一次投递时刻（RFC3339，可空）。
    #[serde(default)]
    pub last_fired_at: Option<String>,
    /// 最近一次投递错误（含自动注销原因；成功投递清空）。
    #[serde(default)]
    pub last_error: Option<String>,
    pub created_at: String,
}

/// 群组成员（im_group_members 行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupMember {
    pub group_id: String,
    pub user_id: String,
    /// `owner` / `admin` / `member`。
    #[serde(default = "default_member_role")]
    pub role: String,
    pub joined_at: String,
}

/// 群组的对外表示（聚合 members + last_activity，前端 `ImGroup` 兼容）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub owner: Option<String>,
    /// 前端用：`group` / `direct`。群组恒为 `group`。
    #[serde(default = "default_kind_group")]
    pub kind: String,
    #[serde(default)]
    pub members: Vec<String>,
    /// 最后一条消息时间（RFC3339，可空）。
    #[serde(default)]
    pub last_activity: Option<String>,
    pub created_at: String,
}

/// 已连接的 Federation 节点（im_peers 行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    /// 形如 `tcp://ip:port`（Federation）/ `ip:port`（兼容前端 `addr`）。
    pub endpoint: String,
    /// `online` / `offline` / `connecting`（前端兼容）。
    #[serde(default = "default_peer_online")]
    pub status: String,
    #[serde(default)]
    pub last_seen: Option<String>,
}

/// IM 服务状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImStatus {
    pub ready: bool,
    pub conversations: usize,
    pub groups: usize,
    pub peers: usize,
    pub messages: usize,
}

/// 大厅成员（im_lobby_members 行 + 派生 online 字段）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LobbyMember {
    pub user_id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    /// 最近一次心跳（RFC3339；60s 内活跃 = 在线）。
    #[serde(default)]
    pub last_seen: Option<String>,
    #[serde(default)]
    pub joined_at: Option<String>,
    /// 派生字段：last_seen 距今 < 60s。
    #[serde(default)]
    pub online: bool,
}

/// 大厅信息（GET /api/v1/im/lobby 响应体）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LobbyInfo {
    /// 恒为 "lobby"。
    pub id: String,
    pub name: String,
    /// 成员总数。
    pub member_count: usize,
    /// 在线成员数（60s 心跳窗口）。
    pub online_count: usize,
    /// 最近一条消息（可空）。
    #[serde(default)]
    pub last_message: Option<Message>,
}

// ----------------------------------------------------------------------------
// 区块链认证：共享内核 os_common::chain_auth（设计 §2；泛化见
// docs/MEDIA_GEN_AND_CHAIN_AUTH.md §C——NexHub 同款挑战-签名模式）
// ----------------------------------------------------------------------------

/// nonce 有效期（秒）：challenge 签发后 60s 内须完成 verify（共享内核常量）。
pub const IM_NONCE_TTL_SECS: i64 = os_common::chain_auth::NONCE_TTL_SECS;
/// token 有效期（秒）：24h（单点登录——同 pubkey 新 verify 顶掉旧 token）。
pub const IM_TOKEN_TTL_SECS: i64 = os_common::chain_auth::TOKEN_TTL_SECS;

/// IM 认证存储——[`os_common::chain_auth::ChainAuth`] 的薄适配（类型别名）：
/// nonce 桶（pubkey→nonce，60s TTL 单次使用）+ token 桶（token→(pubkey, 过期)，
/// pubkey→token 反查索引）。
///
/// 内存 HashMap + Mutex（重启失效可接受：客户端 401 后自动重走挑战-签名）。
/// 与 `ImRouteHandler` 共享（`Arc`），WS 握手层（http.rs）经网关
/// `InProcessGateway::im_auth()` 取同一实例验 token。抽取共享后对外 API
/// （new/create_nonce/take_nonce/issue_token/verify_token/verify_ws）与端点
/// 契约零变化——IM 挂独立实例，与 NexHub 的 token 桶互不相通。
pub type ImAuth = os_common::chain_auth::ChainAuth;

/// 校验 IM 用户名（=身份）格式并解析公钥：`0x` + 66 hex（33 字节压缩 secp256k1，
/// `k256::VerifyingKey::from_sec1` 必须解析成功）。共享内核同名实现。
pub use os_common::chain_auth::parse_pubkey as parse_im_pubkey;

/// 展示名派生（纯函数）：EVM 地址 `0x` + 40 hex =
/// keccak256(未压缩公钥[1..])[12..]（与 os-api blockchain 钱包同规则；
/// os-wallet 的派生走 alloy 栈，跨 crate 复用会引入重依赖，故共享内核本地实现）。
pub use os_common::chain_auth::derive_display_name;

/// ECDSA 验签（共享内核）：签名 = 65 字节 `r||s||v`（v 为恢复位，校验时忽略），
/// 对 nonce 的 UTF-8 字节签（ecdsa crate 的 `verify` 内部做 SHA-256 摘要，
/// 与前端 @noble/secp256k1 `sign(sha256(nonce))` 逐字节兼容）。
use os_common::chain_auth::verify_nonce_signature;

/// 已认证的 IM 调用方（token 反查出的真实身份）。
struct ImCaller {
    pubkey: String,
    display_name: String,
}

/// 从请求头解析 `Authorization: Bearer <IM token>`（大小写宽容，共享内核实现）。
fn bearer_token(req: &ApiRequest) -> Option<&str> {
    os_common::chain_auth::bearer_token(&req.headers)
}

// ----------------------------------------------------------------------------
// ImRouteHandler
// ----------------------------------------------------------------------------

/// IM 路由处理器——HTTP 边界适配到 SQLite 持久化模型 + WebSocket 实时推送。
///
/// 持有共享内核 [`ImShared`]（`Arc`——内置助手的异步回复任务需要跨
/// `tokio::spawn` 拿到 db/WS Hub/防风暴状态）+ [`ImAuth`]（挑战-签名认证的
/// nonce/token 桶；`Arc` 共享给 WS 握手层）。所有 DB 访问短锁快放（同步执行，
/// 不跨 `.await` 持锁）。
pub struct ImRouteHandler {
    shared: Arc<ImShared>,
    auth: Arc<ImAuth>,
    /// 可注入配置（助手 LLM 端点/模型/防风暴窗口、附件根目录、admin token）：
    /// 生产走 env/默认值，测试链式覆盖（绕开 env 并行竞态，model_hub 同款）。
    config: ImConfig,
}

/// 共享内核：db + WS Hub + 内置助手防风暴代次表。
///
/// 独立成 `Arc` 的原因：`tokio::spawn` 的助手回复任务是 `'static`——需要
/// 一份可在 handler 借用期之外存活的 db/Hub 句柄（rusqlite `Connection`
/// 是 `Send` 非 `Sync`，经 `Mutex` 共享单连接；WAL 下与 REST 写路径互斥
/// 串行，安全）。
struct ImShared {
    db: Mutex<Connection>,
    ws_hub: Option<WsHub>,
    /// 内置助手防风暴：会话 id →（最新触发代次，最近触发时刻）。
    /// 新触发代次 +1；旧任务在提交回复前发现代次被超越即放弃——
    /// 3s 窗口内多条 @ 只有最后一条得到响应。条目按 1h TTL 顺手清理。
    assistant_gen: Mutex<HashMap<String, (u64, Instant)>>,
    /// P3 联邦：os-p2p 组网 Handle + 本节点名（None = P2P 未启用——联邦
    /// 发送/接收静默跳过，单机部署零开销）。装配时 main.rs 经
    /// [`ImFederation::set_p2p`] 注入。
    fed_p2p: Mutex<Option<(os_p2p::Handle, String)>>,
    /// P3 联邦：近期已收远程消息 id 内存缓存（去重快路径，容量
    /// [`FED_SEEN_LIMIT`]；DB 的 id 查重兜底——重启后缓存为空仍不重复写）。
    fed_seen: Mutex<VecDeque<String>>,
    /// 联邦**接收**开关（2026-08-23）：false = 暂停接收远程大厅消息——
    /// [`ImFederation::ingest`] 入口短路返回 `Paused`（不写库、不广播）；
    /// 本地消息与联邦发送路径不受影响。默认 true；经
    /// `GET/POST /api/v1/im/federation` 读写；重启回默认（进程内态，不落库）。
    fed_enabled: AtomicBool,
    /// 大厅**开放**开关（2026-08-23）：false = 不允许其他节点浏览本机大厅
    /// （`im_lobby_query` 应答 denied / `im_lobby_post` 静默丢弃）。开发前期
    /// 缺省 **true**（缺省开放，便于联调；发版前再评估默认值）；经
    /// `GET/POST /api/v1/im/lobby/access` 读写；重启回默认（进程内态，不落库）。
    lobby_public: AtomicBool,
    /// 直通消息（DM）**开放**开关（2026-08-30）：false = 不接收其他身份发给
    /// 本节点身份的直通消息（本地 POST /im/dm 403；跨节点 `im_dm` ingest
    /// 丢弃）——自己发出的 DM 不受影响。开发阶段缺省 **true**（默认允许，
    /// `GET/POST /api/v1/im/dm/access` 读写；发版前再评估默认值）；重启回
    /// 默认（进程内态，不落库）。
    dm_open: AtomicBool,
    /// 身份冲突提示去重（2026-08-23）：冲突源 NodeID hex → 上次提示时刻
    /// （[`IDENTITY_WARN_DEDUPE`] 窗口内同源只提示一次）。进程内态不落库。
    identity_warn_last: Mutex<HashMap<String, Instant>>,
    /// 联邦大厅发言时延节流器（2026-08-24）：sender → 近 60s 发言时刻的
    /// 状态机（[`FedThrottle`]），时延参数经 [`ImConfig`] 构造注入（测试
    /// 1ms 级覆盖）。进程内态——重启/多实例各自计数，可接受。
    fed_throttle: Mutex<FedThrottle>,
    /// 联邦广播**延迟队列**的发送端（惰性创建：首次入队时 spawn 单 worker
    /// 串行发送，见 [`ImFederation::enqueue_fed_lobby_broadcast`]）。
    /// unbounded——消息永不因背压丢弃；进程内态，重启丢队列可接受。
    fed_broadcast_tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<FedBroadcastJob>>>,
    /// 远程大厅查询探针（查询端缓存 + 在途关联；[`ImFederation::set_p2p`]
    /// 注入 Handle 时创建，P2P 未启用保持 None——远程浏览端点 503）。
    lobby_probe: Mutex<Option<Arc<ImLobbyProbe>>>,
}

/// 可注入配置（全部 `None` = 生产默认：env 覆盖 → 内置常量）。
#[derive(Clone, Default)]
struct ImConfig {
    /// 助手推理端点覆盖（env `NEXOS_IM_AGENT_LLM_URL`）。
    agent_llm_url: Option<String>,
    /// 助手模型名覆盖（env `NEXOS_IM_AGENT_MODEL`）。
    agent_model: Option<String>,
    /// 防风暴窗口覆盖（默认 [`ASSISTANT_STORM_WINDOW`]）。
    agent_storm_window: Option<Duration>,
    /// 附件根目录覆盖（默认 [`im_files_root_default`]）。
    files_root: Option<PathBuf>,
    /// 系统 admin token（构造时定格 env `NEXOS_ADMIN_TOKEN`/`OS_ADMIN_TOKEN`；
    /// 附件下载 `?token=` 直链场景；测试经 [`ImRouteHandler::with_admin_token`]
    /// 注入绕开 env 并行竞态）。
    admin_token: Option<String>,
    /// 联邦大厅节流常态时延覆盖（默认 [`FED_THROTTLE_SHORT`]；测试 1ms 级）。
    fed_delay_short: Option<Duration>,
    /// 联邦大厅节流升级时延覆盖（默认 [`FED_THROTTLE_LONG`]；测试注入）。
    fed_delay_long: Option<Duration>,
}

impl ImRouteHandler {
    /// 构造 handler，打开默认 DB 路径 + 建表 + seed demo 数据，无 WS Hub
    /// （独立 ImAuth——不经网关共享时 WS 握手验不到该实例的 token）。
    #[must_use]
    pub fn new() -> Self {
        Self::open(&default_db_path(), None, Arc::new(ImAuth::default()))
    }

    /// 构造 handler，打开默认 DB 路径 + 建表 + seed，注入 WebSocket Hub 与
    /// 共享认证存储。
    ///
    /// main.rs 注册时传 `ImRouteHandler::with_ws_hub(gw.ws_hub(), auth.clone())`
    /// 并 `gw.set_im_auth(Some(auth))`——REST 与 WS 握手验同一批 token。
    #[must_use]
    pub fn with_ws_hub(hub: WsHub, auth: Arc<ImAuth>) -> Self {
        Self::open(&default_db_path(), Some(hub), auth)
    }

    /// 用指定 DB 路径构造（无 WS Hub，测试/诊断注入；独立 ImAuth）。
    #[must_use]
    pub fn with_db_path(path: &str) -> Self {
        Self::open(path, None, Arc::new(ImAuth::default()))
    }

    /// 用临时内存库构造（测试注入：数据隔离，进程结束即丢，无 seed）。
    #[must_use]
    pub fn with_empty() -> Self {
        let conn = Connection::open_in_memory().expect("内存库必成功");
        create_schema(&conn).expect("建表必成功");
        Self::from_parts(conn, None, Arc::new(ImAuth::default()))
    }

    /// 用临时内存库 + WS Hub + 共享认证存储构造（WS 端到端测试注入）。
    #[must_use]
    pub fn with_empty_ws(hub: WsHub, auth: Arc<ImAuth>) -> Self {
        let conn = Connection::open_in_memory().expect("内存库必成功");
        create_schema(&conn).expect("建表必成功");
        Self::from_parts(conn, Some(hub), auth)
    }

    /// 用临时内存库构造并 seed demo 数据（测试注入：每个实例独立隔离，
    /// 避免 `new()` 的共享文件库在并行测试下互相干扰）。
    #[must_use]
    pub fn with_demo_data() -> Self {
        let conn = Connection::open_in_memory().expect("内存库必成功");
        create_schema(&conn).expect("建表必成功");
        seed_if_empty(&conn).expect("seed 必成功");
        Self::from_parts(conn, None, Arc::new(ImAuth::default()))
    }

    /// 用既有连接 + 可选 Hub + 认证存储组装（建表/seed 由调用方负责——
    /// [`ImRouteHandler::with_empty`] 等测试入口与 `open` 共用）。
    fn from_parts(conn: Connection, ws_hub: Option<WsHub>, auth: Arc<ImAuth>) -> Self {
        let config = ImConfig::default();
        let shared = ImShared {
            db: Mutex::new(conn),
            ws_hub,
            assistant_gen: Mutex::new(HashMap::new()),
            fed_p2p: Mutex::new(None),
            fed_seen: Mutex::new(VecDeque::new()),
            fed_enabled: AtomicBool::new(true),
            // 开发前期缺省开放大厅（lobby_public=true）：便于多节点联调互见；
            // 发版前再评估是否回落默认私密（false）。
            lobby_public: AtomicBool::new(true),
            // 开发阶段缺省允许直通消息（dm_open=true，用户裁决「当前开发
            // 阶段默认允许」）；发版前再评估默认值。
            dm_open: AtomicBool::new(true),
            identity_warn_last: Mutex::new(HashMap::new()),
            lobby_probe: Mutex::new(None),
            fed_throttle: Mutex::new(FedThrottle::new(
                config.fed_delay_short.unwrap_or(FED_THROTTLE_SHORT),
                config.fed_delay_long.unwrap_or(FED_THROTTLE_LONG),
            )),
            fed_broadcast_tx: Mutex::new(None),
        };
        Self {
            shared: Arc::new(shared),
            auth,
            config,
        }
    }

    fn open(path: &str, ws_hub: Option<WsHub>, auth: Arc<ImAuth>) -> Self {
        let conn = open_db(path).unwrap_or_else(|e| {
            eprintln!("im: 打开 SQLite {path} 失败（{e}），降级到内存库");
            Connection::open_in_memory().expect("内存库必成功")
        });
        // admin token 构造时定格 env（model_hub 同款；测试用链式覆盖）
        let config = ImConfig {
            admin_token: admin_token_from_env(),
            ..ImConfig::default()
        };
        let shared = ImShared {
            db: Mutex::new(conn),
            ws_hub,
            assistant_gen: Mutex::new(HashMap::new()),
            fed_p2p: Mutex::new(None),
            fed_seen: Mutex::new(VecDeque::new()),
            fed_enabled: AtomicBool::new(true),
            // 开发前期缺省开放大厅（lobby_public=true）：便于多节点联调互见；
            // 发版前再评估是否回落默认私密（false）。
            lobby_public: AtomicBool::new(true),
            // 开发阶段缺省允许直通消息（dm_open=true，用户裁决「当前开发
            // 阶段默认允许」）；发版前再评估默认值。
            dm_open: AtomicBool::new(true),
            identity_warn_last: Mutex::new(HashMap::new()),
            lobby_probe: Mutex::new(None),
            fed_throttle: Mutex::new(FedThrottle::new(
                config.fed_delay_short.unwrap_or(FED_THROTTLE_SHORT),
                config.fed_delay_long.unwrap_or(FED_THROTTLE_LONG),
            )),
            fed_broadcast_tx: Mutex::new(None),
        };
        Self {
            shared: Arc::new(shared),
            auth,
            config,
        }
    }

    /// 链式注入助手推理端点（测试用：绕开 env 并行竞态）。
    #[must_use]
    pub fn with_agent_llm_url(mut self, url: &str) -> Self {
        self.config.agent_llm_url = Some(url.to_string());
        self
    }

    /// 链式注入助手模型名（测试用）。
    #[must_use]
    pub fn with_agent_model(mut self, model: &str) -> Self {
        self.config.agent_model = Some(model.to_string());
        self
    }

    /// 链式注入防风暴窗口（测试用：默认 3s 太慢）。
    #[must_use]
    pub fn with_agent_storm_window(mut self, window: Duration) -> Self {
        self.config.agent_storm_window = Some(window);
        self
    }

    /// 链式注入附件根目录（测试用：临时目录隔离）。
    #[must_use]
    pub fn with_files_root(mut self, root: &str) -> Self {
        self.config.files_root = Some(PathBuf::from(root));
        self
    }

    /// 链式注入系统 admin token（测试用：绕开 env 并行竞态）。
    #[must_use]
    pub fn with_admin_token(mut self, token: &str) -> Self {
        self.config.admin_token = Some(token.to_string());
        self
    }

    /// 链式注入联邦大厅节流时延（测试用：默认 10s/60s 太慢——双节点端到端
    /// 与延迟队列测试注入 1ms 级；仅改节流参数，语义不变）。同步覆写已构造
    /// shared 内的节流器，故须在 `federation()` 分发之前调用。
    #[must_use]
    pub fn with_fed_throttle_delays(mut self, short: Duration, long: Duration) -> Self {
        self.config.fed_delay_short = Some(short);
        self.config.fed_delay_long = Some(long);
        {
            let mut throttle = self
                .shared
                .fed_throttle
                .lock()
                .expect("fed_throttle poisoned");
            throttle.short_delay = short;
            throttle.long_delay = long;
        }
        self
    }

    /// 助手推理端点：测试覆盖 > env `NEXOS_IM_AGENT_LLM_URL` > 动态探测（见 spawn 内）。
    fn agent_llm_url_configured(&self) -> Option<String> {
        self.config
            .agent_llm_url
            .clone()
            .or_else(|| env_non_empty("NEXOS_IM_AGENT_LLM_URL"))
    }

    /// 动态探测本机活跃 vLLM：**8123 优先**（新实例端口约定），再扫 8000..=8010
    /// （旧实例递增区）。逐端口 GET /v1/models（200ms 超时，AGENT_HTTP 共享客户端），
    /// 全不通回落默认常量——根治"端口漂移导致助手连不上 LLM"（2026-08-21 实测踩坑，
    /// 用户裁决：新实例默认端口迁 8123）。
    async fn probe_live_llm_url() -> String {
        for port in [8123u16].into_iter().chain(8000..=8010) {
            let url = format!("http://127.0.0.1:{port}/v1/models");
            let ok = AGENT_HTTP
                .get(&url)
                .timeout(std::time::Duration::from_millis(200))
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            if ok {
                return format!("http://127.0.0.1:{port}/v1/chat/completions");
            }
        }
        ASSISTANT_LLM_URL_DEFAULT.to_string()
    }

    /// 助手模型名解析：测试覆盖 > env `NEXOS_IM_AGENT_MODEL` > 默认。
    fn agent_model(&self) -> String {
        self.config
            .agent_model
            .clone()
            .or_else(|| env_non_empty("NEXOS_IM_AGENT_MODEL"))
            .unwrap_or_else(|| ASSISTANT_LLM_MODEL_DEFAULT.to_string())
    }

    /// 防风暴窗口解析：测试覆盖 > 默认 3s。
    fn agent_storm_window(&self) -> Duration {
        self.config
            .agent_storm_window
            .unwrap_or(ASSISTANT_STORM_WINDOW)
    }

    /// 附件根目录解析：测试覆盖 > env `NEXOS_IM_FILES_ROOT` > 默认。
    fn files_root(&self) -> PathBuf {
        self.config
            .files_root
            .clone()
            .or_else(|| env_non_empty("NEXOS_IM_FILES_ROOT").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(im_files_root_default()))
    }

    /// 认证存储引用（main.rs 装配 / 网关 WS 握手共享）。
    #[must_use]
    pub fn auth(&self) -> Arc<ImAuth> {
        self.auth.clone()
    }

    /// 联邦端点（P3）：main.rs 装配时在 handler Box 进网关**之前**取出——
    /// p2p Handle 注入（发送端）与 FederationBridge 的入站分发（接收端）
    /// 共用同一份 `Arc<ImShared>`。
    #[must_use]
    pub fn federation(&self) -> ImFederation {
        ImFederation {
            shared: self.shared.clone(),
        }
    }

    /// 从 `Authorization: Bearer <IM token>` 反查调用方真实身份
    /// （pubkey + 派生展示名）。无/无效 token → None（调用方回 401）。
    fn caller(&self, req: &ApiRequest) -> Option<ImCaller> {
        let token = bearer_token(req)?;
        let pubkey = self.auth.verify_token(token)?;
        let display_name = derive_display_name(&parse_im_pubkey(&pubkey)?);
        Some(ImCaller {
            pubkey,
            display_name,
        })
    }

    /// 系统 admin token 校验（Bearer 同格式；未配置 admin token 恒 false）——
    /// 联邦/大厅开关类管理端点与 IM token 二选一的鉴权前半段。
    fn admin_ok(&self, req: &ApiRequest) -> bool {
        bearer_token(req).is_some_and(|t| {
            self.config
                .admin_token
                .as_deref()
                .is_some_and(|expected| expected == t)
        })
    }

    /// 当前全量对话快照（从 DB 查）。
    #[must_use]
    pub fn conversations_snapshot(&self) -> Vec<Conversation> {
        let conn = self.shared.db.lock().expect("db poisoned");
        load_all_conversations(&conn).unwrap_or_default()
    }

    /// 当前全量消息快照（从 DB 查）。
    #[must_use]
    pub fn messages_snapshot(&self) -> Vec<Message> {
        let conn = self.shared.db.lock().expect("db poisoned");
        load_all_messages(&conn).unwrap_or_default()
    }

    /// 当前全量群组快照（从 DB 查）。
    #[must_use]
    pub fn groups_snapshot(&self) -> Vec<Group> {
        let conn = self.shared.db.lock().expect("db poisoned");
        load_all_groups(&conn).unwrap_or_default()
    }

    /// 当前全量 peers 快照（从 DB 查）。
    #[must_use]
    pub fn peers_snapshot(&self) -> Vec<Peer> {
        let conn = self.shared.db.lock().expect("db poisoned");
        load_all_peers(&conn).unwrap_or_default()
    }

    /// agent 协调组件的 IM 桥句柄（声明系统消息直插 + 群成员查询；
    /// main.rs 装配 agent-coord 时注入，见 [`ImCoordBridge`]）。
    #[must_use]
    pub fn coord_bridge(&self) -> ImCoordBridge {
        ImCoordBridge {
            shared: self.shared.clone(),
        }
    }

    /// 把一条大厅消息全员广播到 WebSocket（`type: "im_lobby_message"`）。
    fn broadcast_lobby(hub: &Option<WsHub>, msg: &Message) {
        if let Some(hub) = hub {
            let msg_val = serde_json::to_value(msg).unwrap_or(serde_json::Value::Null);
            hub.broadcast_n(WsMessage::ImLobbyMessage {
                lobby_id: LOBBY_ID.to_string(),
                message: msg_val,
            });
        }
    }

    /// 把一条联邦大厅消息全员广播到 WebSocket（`type: "im_fed_lobby_message"`，
    /// lobby_id 恒为 [`FED_LOBBY_ID`]）——本地发言与联邦接收共用同一帧型，
    /// 前端据此路由到联邦大厅会话（与大厅帧完全隔离）。
    fn broadcast_fed_lobby(hub: &Option<WsHub>, msg: &Message) {
        if let Some(hub) = hub {
            let msg_val = serde_json::to_value(msg).unwrap_or(serde_json::Value::Null);
            hub.broadcast_n(WsMessage::ImFedLobbyMessage {
                lobby_id: FED_LOBBY_ID.to_string(),
                message: msg_val,
            });
        }
    }

    /// 把一条会话消息广播到 WebSocket（`type: "im_message"`）。
    fn broadcast_conversation(hub: &Option<WsHub>, cid: &str, msg: &Message) {
        if let Some(hub) = hub {
            let msg_val = serde_json::to_value(msg).unwrap_or(serde_json::Value::Null);
            hub.broadcast_n(WsMessage::ImMessage {
                conversation_id: cid.to_string(),
                message: msg_val,
            });
        }
    }

    /// 校验 conversation_id 是否存在（im_conversations 或 im_groups 任一命中即可，
    /// 因群组 id 即可作为 conversation_id 收发消息）。
    fn conversation_exists(conn: &Connection, id: &str) -> bool {
        let in_conv: bool = conn
            .query_row(
                "SELECT 1 FROM im_conversations WHERE id=?",
                params![id],
                |_| Ok(true),
            )
            .optional()
            .unwrap_or(Some(false))
            .unwrap_or(false);
        if in_conv {
            return true;
        }
        conn.query_row("SELECT 1 FROM im_groups WHERE id=?", params![id], |_| {
            Ok(true)
        })
        .optional()
        .unwrap_or(None)
        .is_some()
    }

    /// 会话可读性判定（离线补拉端点 `GET /api/v1/im/messages` 专用）：
    /// - `None`：会话不存在（im_conversations / im_groups / lobby 均未命中）→ 404；
    /// - `Some(false)`：越权 → 403。大厅须已加入（im_lobby_members），
    ///   群组须在 im_group_members（owner/admin/member 皆可），
    ///   DM（dm-*）须在 im_dm_members（只有收发双方可读）；
    /// - `Some(true)`：可读。普通直接对话（im_conversations）沿用现状——
    ///   对话无成员表，任何有效 IM token 都能读（与既有
    ///   GET /conversations/:id/messages 同语义，保持兼容）；
    ///   联邦大厅（fed-lobby）是跨节点公共频道——任何有效 IM token 可读。
    fn conversation_readable(conn: &Connection, cid: &str, pubkey: &str) -> Option<bool> {
        let hit = |sql: &str| {
            conn.query_row(sql, params![cid], |_| Ok(true))
                .optional()
                .unwrap_or(None)
                .is_some()
        };
        if cid == LOBBY_ID {
            return Some(lobby_is_member(conn, pubkey));
        }
        if cid == FED_LOBBY_ID {
            return Some(true);
        }
        if hit("SELECT 1 FROM im_groups WHERE id=?") {
            let member = conn
                .query_row(
                    "SELECT 1 FROM im_group_members WHERE group_id=? AND user_id=?",
                    params![cid, pubkey],
                    |_| Ok(true),
                )
                .optional()
                .unwrap_or(None)
                .is_some();
            return Some(member);
        }
        if hit("SELECT 1 FROM im_conversations WHERE id=?") {
            // DM 会话按成员表收口（只有收发双方可读）；普通对话无成员模型，
            // 沿用全员可读的现状（兼容既有客户端）。
            if is_dm_conversation(cid) {
                return Some(dm_is_member(conn, cid, pubkey));
            }
            return Some(true);
        }
        None
    }
}

impl Default for ImRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

// ----------------------------------------------------------------------------
// ImCoordBridge：agent 协调组件（agent_coord.rs）的 IM 桥接句柄
// —— handler 间不直接持引用：agent-coord 经 main.rs 装配时注入本句柄，
//    获得「直插系统消息 + 广播」与「按 pubkey 查群成员」两个最小能力。
// ----------------------------------------------------------------------------

/// agent 协调组件的 IM 桥（`Arc<ImShared>` 轻量句柄，`federation()` 同款手法）。
///
/// 能力（刻意最小面）：
/// - [`ImCoordBridge::post_system`]：服务端直插一条 sender_kind="system" 的
///   系统消息进 `im_messages` + WS 广播（协议声明用）；
/// - [`ImCoordBridge::groups_of_member`]：查某 pubkey 加入的全部群组 id
///   （声明定向用）。
pub struct ImCoordBridge {
    shared: Arc<ImShared>,
}

impl ImCoordBridge {
    /// 直插系统消息 + 按会话类型选 WS 广播通道；返回是否落库成功。
    pub fn post_system(&self, conversation_id: &str, content: &str) -> bool {
        let msg = Message {
            id: new_uuid(),
            conversation_id: conversation_id.to_string(),
            sender_id: "system".to_string(),
            sender_name: Some("NexOS".to_string()),
            content: content.to_string(),
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
                return false; // 写失败按未声明处理（协调层保留 declared_groups 不记录）
            }
        }
        if conversation_id == LOBBY_ID {
            ImRouteHandler::broadcast_lobby(&self.shared.ws_hub, &msg);
        } else {
            ImRouteHandler::broadcast_conversation(&self.shared.ws_hub, conversation_id, &msg);
        }
        true
    }

    /// 查某用户（pubkey）加入的全部群组 id（im_group_members 反查）。
    pub fn groups_of_member(&self, user_id: &str) -> Vec<String> {
        let conn = self.shared.db.lock().expect("db poisoned");
        let mut stmt = match conn.prepare("SELECT group_id FROM im_group_members WHERE user_id=?") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        stmt.query_map(params![user_id], |row| row.get::<_, String>(0))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }
}

#[async_trait]
impl RouteHandler for ImRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            // —— 认证（公开挑战-签名，无 IM token / 无系统认证）——
            spec(HttpMethod::Post, PATH_AUTH_CHALLENGE, false, vec![]),
            spec(HttpMethod::Post, PATH_AUTH_VERIFY, false, vec![]),
            // —— 对话（IM 用户面：IM token 在 handler 内验，不走系统中间件）——
            spec(HttpMethod::Get, PATH_CONV_LIST, false, vec![]),
            spec(HttpMethod::Post, PATH_CONV_LIST, false, vec![]),
            spec(HttpMethod::Get, PATH_CONV_MESSAGES, false, vec![]),
            spec(HttpMethod::Post, PATH_CONV_MESSAGES, false, vec![]),
            // —— 离线补拉（IM token 在 handler 内验；member 语义见端点注释）——
            spec(HttpMethod::Get, PATH_MESSAGES_CATCHUP, false, vec![]),
            // —— 群组（同上，IM token 在 handler 内验）——
            spec(HttpMethod::Get, PATH_GROUPS, false, vec![]),
            spec(HttpMethod::Post, PATH_GROUPS, false, vec![]),
            spec(HttpMethod::Post, PATH_GROUP_JOIN, false, vec![]),
            spec(HttpMethod::Post, PATH_GROUP_LEAVE, false, vec![]),
            spec(HttpMethod::Get, PATH_GROUP_MEMBERS, false, vec![]),
            // —— Federation peers（管理面：系统级 Principal，与 IM 身份正交）——
            spec(HttpMethod::Get, PATH_PEERS, false, vec![]),
            spec(HttpMethod::Post, PATH_PEERS, true, vec![]),
            // —— 状态（公开健康检查）——
            spec(HttpMethod::Get, PATH_STATUS, false, vec![]),
            // —— 已读 / 未读 / 搜索（IM token 在 handler 内验）——
            spec(HttpMethod::Post, PATH_MSG_READ, false, vec![]),
            spec(HttpMethod::Get, PATH_CONV_UNREAD, false, vec![]),
            spec(HttpMethod::Get, PATH_SEARCH, false, vec![]),
            // —— 大厅（公共频道，全员自动加入；IM token 在 handler 内验）——
            spec(HttpMethod::Get, PATH_LOBBY, false, vec![]),
            spec(HttpMethod::Get, PATH_LOBBY_MESSAGES, false, vec![]),
            spec(HttpMethod::Post, PATH_LOBBY_MESSAGES, false, vec![]),
            spec(HttpMethod::Get, PATH_LOBBY_MEMBERS, false, vec![]),
            // —— 附件（文档传输：IM token 在 handler 内验；下载另收 ?token=）——
            spec(HttpMethod::Post, PATH_FILES, false, vec![]),
            spec(HttpMethod::Get, PATH_FILE_DOWNLOAD, false, vec![]),
            // —— 推送通知 webhook（IM token 在 handler 内验；owner=pubkey）——
            spec(HttpMethod::Post, PATH_NOTIFY_REGISTER, false, vec![]),
            spec(HttpMethod::Get, PATH_NOTIFY_LIST, false, vec![]),
            spec(HttpMethod::Delete, PATH_NOTIFY_UNREGISTER, false, vec![]),
            // —— 联邦接收开关（GET 读状态需 IM token；POST 切换 admin 或 IM
            //    token——均在 handler 内验，不走系统中间件，与用户面惯例一致）——
            spec(HttpMethod::Get, PATH_FEDERATION, false, vec![]),
            spec(HttpMethod::Post, PATH_FEDERATION, false, vec![]),
            // —— 大厅开放开关（是否允许其他节点浏览本机大厅；默认 false。
            //    GET 读状态 / POST 切换均收 admin 或 IM token，handler 内验）——
            spec(HttpMethod::Get, PATH_LOBBY_ACCESS, false, vec![]),
            spec(HttpMethod::Post, PATH_LOBBY_ACCESS, false, vec![]),
            // —— 远程大厅互联（节点发现页「进入 IM」跳转目的地；IM token
            //    在 handler 内验）：GET 拉对方开放状态 + 脱敏消息镜像；POST
            //    经 P2P 向对方大厅发言（对方开关开才落地）——
            spec(HttpMethod::Get, PATH_LOBBY_REMOTE, false, vec![]),
            spec(HttpMethod::Post, PATH_LOBBY_REMOTE_MESSAGES, false, vec![]),
            // —— 联邦大厅（跨节点共享频道，与我的大厅完全隔离的可写会话；
            //    IM token 在 handler 内验）：GET 心跳加入；GET messages 历史/
            //    增量；POST 发言（本地落库 + P2P 广播全部 peer）——
            spec(HttpMethod::Get, PATH_FED_LOBBY, false, vec![]),
            spec(HttpMethod::Get, PATH_FED_LOBBY_MESSAGES, false, vec![]),
            spec(HttpMethod::Post, PATH_FED_LOBBY_MESSAGES, false, vec![]),
            // —— 直通消息 DM（点对点私信，只有双方可见；IM token 在 handler
            //    内验）：POST 发起（本地投递或 P2P 定向路由）；GET/POST access
            //    为开放开关（admin 或 IM token）——
            spec(HttpMethod::Post, PATH_DM, false, vec![]),
            spec(HttpMethod::Get, PATH_DM_ACCESS, false, vec![]),
            spec(HttpMethod::Post, PATH_DM_ACCESS, false, vec![]),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        let query = req.path.split('?').nth(1).unwrap_or("");
        match (req.method, segs.as_slice()) {
            // —— POST /api/v1/im/auth/challenge —— 签发挑战 nonce（公开）
            //    body: {pubkey} → {nonce, expires_in, display_name}
            (HttpMethod::Post, ["api", "v1", "im", "auth", "challenge"]) => {
                #[derive(serde::Deserialize)]
                struct ChallengeReq {
                    pubkey: String,
                }
                let body: ChallengeReq = match serde_json::from_value(req.body) {
                    Ok(b) => b,
                    Err(e) => return Ok(error_response(400, &format!("解析挑战请求体失败: {e}"))),
                };
                let vk = match parse_im_pubkey(&body.pubkey) {
                    Some(v) => v,
                    None => {
                        return Ok(error_response(
                            400,
                            "pubkey 非法：应为 0x + 66 hex（33 字节压缩 secp256k1）",
                        ))
                    }
                };
                let nonce = self.auth.create_nonce(&body.pubkey);
                Ok(ok_json(serde_json::json!({
                    "nonce": nonce,
                    "expires_in": IM_NONCE_TTL_SECS,
                    "display_name": derive_display_name(&vk),
                })))
            }

            // —— POST /api/v1/im/auth/verify —— 验签 + 签发 token（公开）
            //    body: {pubkey, nonce, signature(0x+130 hex, 65 字节 r||s||v)}
            //    → {token, expires_in, pubkey, display_name}
            (HttpMethod::Post, ["api", "v1", "im", "auth", "verify"]) => {
                #[derive(serde::Deserialize)]
                struct VerifyReq {
                    pubkey: String,
                    nonce: String,
                    signature: String,
                }
                let body: VerifyReq = match serde_json::from_value(req.body) {
                    Ok(b) => b,
                    Err(e) => return Ok(error_response(400, &format!("解析验签请求体失败: {e}"))),
                };
                let vk = match parse_im_pubkey(&body.pubkey) {
                    Some(v) => v,
                    None => {
                        return Ok(error_response(
                            400,
                            "pubkey 非法：应为 0x + 66 hex（33 字节压缩 secp256k1）",
                        ))
                    }
                };
                let sig_hex = body.signature.trim().trim_start_matches("0x");
                let sig = match hex::decode(sig_hex) {
                    Ok(s) if s.len() == 65 => s,
                    _ => {
                        return Ok(error_response(
                            400,
                            "signature 非法：应为 65 字节 r||s||v 的 hex（可带 0x 前缀）",
                        ))
                    }
                };
                // nonce 用后即焚（签名失败同样烧掉，防暴力尝试）
                if !self.auth.take_nonce(&body.pubkey, &body.nonce) {
                    return Ok(error_response(401, "nonce 无效、已用或已过期（60s）"));
                }
                if !verify_nonce_signature(&vk, &body.nonce, &sig) {
                    return Ok(error_response(401, "签名验证失败"));
                }
                let (token, expires_in) = self.auth.issue_token(&body.pubkey);
                Ok(ok_json(serde_json::json!({
                    "token": token,
                    "expires_in": expires_in,
                    "pubkey": body.pubkey,
                    "display_name": derive_display_name(&vk),
                })))
            }

            // —— GET /api/v1/im/conversations —— 列出对话
            (HttpMethod::Get, ["api", "v1", "im", "conversations"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let list = {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    load_all_conversations(&conn)
                        .unwrap_or_default()
                        .into_iter()
                        // DM 会话 members 感知：只列自己是成员的（对方发起的
                        // 私信也可见——判定依据是 im_dm_members 而非 created_by，
                        // 2026-08-30 DM 批次）；普通对话沿用全员可见现状。
                        .filter(|c| {
                            !is_dm_conversation(&c.id) || dm_is_member(&conn, &c.id, &caller.pubkey)
                        })
                        .collect::<Vec<_>>()
                };
                Ok(ok_json(to_value(&list)?))
            }

            // —— POST /api/v1/im/conversations —— 创建对话 body:{ name }
            //    created_by = token 反查 pubkey（自报值一律忽略）
            (HttpMethod::Post, ["api", "v1", "im", "conversations"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                #[derive(serde::Deserialize)]
                struct CreateConvReq {
                    name: String,
                }
                let body: CreateConvReq = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析创建对话请求体失败: {e}"))
                })?;
                let conv = Conversation {
                    id: new_uuid(),
                    name: body.name,
                    is_group: false,
                    created_by: Some(caller.pubkey),
                    created_at: now_iso(),
                    members: Vec::new(),
                };
                {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    insert_conversation(&conn, &conv)?;
                }
                Ok(ApiResponse {
                    status: 201,
                    body: to_value(&conv)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/im/conversations/:id/messages —— 对话消息历史
            //    DM（dm-*）会话按成员收口：非收发双方 403（私信只有双方可见）。
            (HttpMethod::Get, ["api", "v1", "im", "conversations", id, "messages"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let cid = (*id).to_string();
                let list = {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    if is_dm_conversation(&cid) && !dm_is_member(&conn, &cid, &caller.pubkey) {
                        return Ok(error_response(403, "非直通消息参与者，无权读取该会话"));
                    }
                    load_messages_by_conversation(&conn, &cid).unwrap_or_default()
                };
                Ok(ok_json(to_value(&list)?))
            }

            // —— POST /api/v1/im/conversations/:id/messages —— 发送消息
            //    body: { content, msg_type?, file_url?, reply_to?,
            //            sender_kind?("human"|"agent" 展示层自声明), attachment? }
            //    sender = token 反查 pubkey（自报 sender_id/sender_name 一律忽略）；
            //    mentions 服务端从 content 解析；attachment 按 file_id 核对真值；
            //    @NexOS助手 触发内置助手异步回复（见模块注释）。
            (HttpMethod::Post, ["api", "v1", "im", "conversations", id, "messages"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let cid = (*id).to_string();
                let exists = {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    Self::conversation_exists(&conn, &cid)
                };
                if !exists {
                    return Ok(error_response(404, &format!("对话不存在: {cid}")));
                }
                // DM 会话不走通用发送端点：统一走 POST /api/v1/im/dm（dm_open
                // 开关与成员收口都在该端点收口，不旁路；2026-08-30 DM 批次）。
                if is_dm_conversation(&cid) {
                    return Ok(error_response(
                        400,
                        "直通消息会话请走 POST /api/v1/im/dm（to_pubkey=对方公钥）",
                    ));
                }
                #[derive(serde::Deserialize)]
                struct SendMsgReq {
                    content: String,
                    #[serde(default = "default_msg_type_text")]
                    msg_type: String,
                    #[serde(default)]
                    file_url: Option<String>,
                    #[serde(default)]
                    reply_to: Option<String>,
                    /// 展示层自声明（非 "agent" 一律归一 "human"；见 Message 字段注释）。
                    #[serde(default)]
                    sender_kind: Option<String>,
                    /// 附件（服务端按 file_id 核对存在性并覆盖 size/filename）。
                    #[serde(default)]
                    attachment: Option<AttachmentReq>,
                }
                let body: SendMsgReq = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析发送消息请求体失败: {e}"))
                })?;
                let attachment = match self.verify_attachment(body.attachment.as_ref()) {
                    Ok(a) => a,
                    Err(resp) => return Ok(resp),
                };
                let mentions = parse_mentions(&body.content);
                let msg = Message {
                    id: new_uuid(),
                    conversation_id: cid.clone(),
                    sender_id: caller.pubkey.clone(),
                    sender_name: Some(caller.display_name.clone()),
                    content: body.content,
                    msg_type: body.msg_type,
                    file_url: body.file_url,
                    reply_to: body.reply_to,
                    created_at: now_iso(),
                    read_by: vec![caller.pubkey.clone()],
                    sender_kind: normalize_sender_kind(body.sender_kind.as_deref()),
                    mentions,
                    attachment,
                };
                {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    insert_message(&conn, &msg)?;
                }
                // 广播到 WebSocket（实时推送；message 体携带全部新字段）
                Self::broadcast_conversation(&self.shared.ws_hub, &cid, &msg);
                // agent 协调：@ 定向投递（agent-coord 组件钩子，未装配时 no-op）
                crate::handlers::agent_coord::on_im_message(&msg);
                // @NexOS助手 → 内置助手异步回复（防风暴去抖，agent 消息不触发）
                self.maybe_spawn_assistant(&msg);
                // 推送通知：匹配的注册 webhook 异步 POST（不阻塞本响应）
                self.shared.dispatch_webhooks(&msg);
                Ok(ApiResponse {
                    status: 201,
                    body: to_value(&msg)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/im/messages?conversation_id=&after_id=&limit= ——
            //    离线补拉（WS 断线重连后的缺口增量；也可当通用升序分页用）：
            //    - 鉴权：IM token（401）；
            //    - member 语义：大厅/群组须是成员（403，同 POST /lobby/messages
            //      的门）；直接对话（im_conversations）沿用现状——任何有效
            //      IM token 可读（无成员表）；未知会话 404；
            //    - after_id：本地最后一条消息 id，服务端映射 rowid 严格大于；
            //      缺省/未知 → 从头升序取；
            //    - limit：默认 50，钳制到 1..=200；
            //    - 返回：Message[]，按插入序（rowid）升序。
            (HttpMethod::Get, ["api", "v1", "im", "messages"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let Some(cid) =
                    parse_query_str(query, "conversation_id").filter(|s| !s.trim().is_empty())
                else {
                    return Ok(error_response(
                        400,
                        "缺少 conversation_id 查询参数（?conversation_id=<会话 id>）",
                    ));
                };
                let after_id = parse_query_str(query, "after_id").filter(|s| !s.trim().is_empty());
                let limit = parse_query_str(query, "limit")
                    .and_then(|s| s.trim().parse::<i64>().ok())
                    .unwrap_or(50)
                    .clamp(1, 200);
                let access = {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    Self::conversation_readable(&conn, &cid, &caller.pubkey)
                };
                let list = match access {
                    None => return Ok(error_response(404, &format!("会话不存在: {cid}"))),
                    Some(false) => {
                        return Ok(error_response(
                            403,
                            &format!(
                                "无权访问会话 {cid}（非成员；群组先 join，大厅先 GET /lobby）"
                            ),
                        ));
                    }
                    Some(true) => {
                        let conn = self.shared.db.lock().expect("db poisoned");
                        load_messages_after(&conn, &cid, after_id.as_deref(), limit)
                            .unwrap_or_default()
                    }
                };
                Ok(ok_json(to_value(&list)?))
            }

            // —— GET /api/v1/im/groups —— 列出群组（聚合 members + last_activity）
            (HttpMethod::Get, ["api", "v1", "im", "groups"]) => {
                let Some(_caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let list = {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    load_all_groups(&conn).unwrap_or_default()
                };
                Ok(ok_json(to_value(&list)?))
            }

            // —— POST /api/v1/im/groups —— 创建群组 body:{ name, members? }
            //    owner = token 反查 pubkey（自报 owner 一律忽略）
            (HttpMethod::Post, ["api", "v1", "im", "groups"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                #[derive(serde::Deserialize)]
                struct CreateGroupReq {
                    name: String,
                    #[serde(default)]
                    members: Option<Vec<String>>,
                }
                let body: CreateGroupReq = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析创建群组请求体失败: {e}"))
                })?;
                let owner = caller.pubkey;
                let mut members = body.members.unwrap_or_default();
                if !members.contains(&owner) {
                    members.insert(0, owner.clone());
                }
                let now = now_iso();
                let gid = new_uuid();
                let group = Group {
                    id: gid.clone(),
                    name: body.name,
                    owner: Some(owner.clone()),
                    kind: "group".to_string(),
                    members: members.clone(),
                    last_activity: None,
                    created_at: now.clone(),
                };
                {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    insert_group(&conn, &group)?;
                    for uid in &members {
                        let role = if *uid == owner { "owner" } else { "member" };
                        let _ = insert_group_member(&conn, &gid, uid, role, &now);
                    }
                }
                Ok(ApiResponse {
                    status: 201,
                    body: to_value(&group)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— POST /api/v1/im/groups/:id/join —— 加入群组（member = token pubkey）
            (HttpMethod::Post, ["api", "v1", "im", "groups", id, "join"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let mut group = {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    match find_group(&conn, id)? {
                        Some(g) => g,
                        None => return Ok(error_response(404, &format!("群组不存在: {id}"))),
                    }
                };
                {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    if !group.members.contains(&caller.pubkey) {
                        let _ =
                            insert_group_member(&conn, id, &caller.pubkey, "member", &now_iso());
                        group.members.push(caller.pubkey.clone());
                    }
                }
                Ok(ok_json(to_value(&group)?))
            }

            // —— POST /api/v1/im/groups/:id/leave —— 退出群组（member = token pubkey）
            (HttpMethod::Post, ["api", "v1", "im", "groups", id, "leave"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let mut group = {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    match find_group(&conn, id)? {
                        Some(g) => g,
                        None => return Ok(error_response(404, &format!("群组不存在: {id}"))),
                    }
                };
                {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    let _ = remove_group_member(&conn, id, &caller.pubkey);
                    group.members.retain(|m| *m != caller.pubkey);
                }
                Ok(ok_json(to_value(&group)?))
            }

            // —— GET /api/v1/im/groups/:id/members —— 群组成员
            (HttpMethod::Get, ["api", "v1", "im", "groups", id, "members"]) => {
                let Some(_caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let (name, members) = {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    match find_group(&conn, id)? {
                        Some(g) => (g.name, g.members),
                        None => return Ok(error_response(404, &format!("群组不存在: {id}"))),
                    }
                };
                Ok(ok_json(serde_json::json!({
                    "group_id": id,
                    "name": name,
                    "members": members,
                })))
            }

            // —— GET /api/v1/im/peers —— 已连接的 Federation 节点
            (HttpMethod::Get, ["api", "v1", "im", "peers"]) => {
                let list = {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    load_all_peers(&conn).unwrap_or_default()
                };
                Ok(ok_json(to_value(&list)?))
            }

            // —— POST /api/v1/im/peers —— 添加节点 body:{ addr | endpoint, id?, name? }
            (HttpMethod::Post, ["api", "v1", "im", "peers"]) => {
                #[derive(serde::Deserialize)]
                struct AddPeerReq {
                    #[serde(default)]
                    addr: Option<String>,
                    #[serde(default)]
                    endpoint: Option<String>,
                    #[serde(default)]
                    id: Option<String>,
                    #[serde(default)]
                    name: Option<String>,
                }
                let body: AddPeerReq = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析添加节点请求体失败: {e}"))
                })?;
                let endpoint = body
                    .endpoint
                    .filter(|s| !s.is_empty())
                    .or(body.addr.filter(|s| !s.is_empty()))
                    .ok_or_else(|| {
                        ApiGatewayError::Internal("添加节点缺少 addr/endpoint".to_string())
                    })?;
                let peer = Peer {
                    id: body.id.unwrap_or_else(|| format!("peer-{}", short_uuid())),
                    name: body.name,
                    endpoint,
                    status: "online".to_string(),
                    last_seen: Some(now_iso()),
                };
                {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    insert_peer(&conn, &peer)?;
                }
                Ok(ApiResponse {
                    status: 201,
                    body: to_value(&peer)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/im/status —— IM 服务状态
            (HttpMethod::Get, ["api", "v1", "im", "status"]) => {
                let status = {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    let conversations = count_rows(&conn, "im_conversations");
                    let groups = count_rows(&conn, "im_groups");
                    let peers = count_rows(&conn, "im_peers");
                    let messages = count_rows(&conn, "im_messages");
                    ImStatus {
                        ready: true,
                        conversations,
                        groups,
                        peers,
                        messages,
                    }
                };
                Ok(ok_json(to_value(&status)?))
            }

            // —— POST /api/v1/im/messages/:id/read —— 标记消息已读
            //    已读人 = token 反查 pubkey（自报 user_id 一律忽略）
            (HttpMethod::Post, ["api", "v1", "im", "messages", id, "read"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let conn = self.shared.db.lock().expect("db poisoned");
                let mut msg = match find_message(&conn, id)? {
                    Some(m) => m,
                    None => return Ok(error_response(404, &format!("消息不存在: {id}"))),
                };
                if !msg.read_by.contains(&caller.pubkey) {
                    msg.read_by.push(caller.pubkey.clone());
                    update_message_read_by(&conn, &msg)?;
                }
                Ok(ok_json(to_value(&msg)?))
            }

            // —— GET /api/v1/im/conversations/:id/unread —— 对话未读消息数
            //    user = token 反查 pubkey（查询参数 ?user= 一律忽略）
            (HttpMethod::Get, ["api", "v1", "im", "conversations", id, "unread"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let unread = {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    count_unread(&conn, id, &caller.pubkey)
                };
                Ok(ok_json(serde_json::json!({
                    "conversation_id": id,
                    "user": caller.pubkey,
                    "unread": unread,
                })))
            }

            // —— GET /api/v1/im/search?q=<关键词>&conversation_id=<可选>&limit= ——
            //    全文搜索消息（content LIKE，通配符按字面匹配）：
            //    - 鉴权：IM token（401）；
            //    - q：必填非空白（缺省/空白 → 400），值经 URL 解码（%XX +
            //      `+`→空格；前端 URLSearchParams/encodeURIComponent 产物可
            //      直用，CJK/空格/% 正常）；
            //    - conversation_id：缺省 = 搜大厅（lobby）；指定 = 搜该会话，
            //      member 门与补拉 `GET /messages` 同款（未知 404；大厅未加入/
            //      群组非成员 403；直接对话全员可读）；
            //    - limit：默认 50，钳制到 1..=200；
            //    - 返回：{q, conversation_id, count, results}，按 created_at 倒序
            //      （最新在前）；q 原样回显供前端高亮。
            (HttpMethod::Get, ["api", "v1", "im", "search"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let q = parse_query_str(query, "q")
                    .map(|s| url_decode_query(&s))
                    .unwrap_or_default();
                if q.trim().is_empty() {
                    return Ok(error_response(400, "缺少搜索词（?q=<关键词>）"));
                }
                let cid = parse_query_str(query, "conversation_id")
                    .map(|s| url_decode_query(&s))
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| LOBBY_ID.to_string());
                let limit = parse_query_str(query, "limit")
                    .and_then(|s| s.trim().parse::<i64>().ok())
                    .unwrap_or(50)
                    .clamp(1, 200);
                let access = {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    Self::conversation_readable(&conn, &cid, &caller.pubkey)
                };
                let results = match access {
                    None => return Ok(error_response(404, &format!("会话不存在: {cid}"))),
                    Some(false) => {
                        return Ok(error_response(
                            403,
                            &format!(
                                "无权搜索会话 {cid}（非成员；群组先 join，大厅先 GET /lobby）"
                            ),
                        ));
                    }
                    Some(true) => {
                        let conn = self.shared.db.lock().expect("db poisoned");
                        search_messages(&conn, &cid, &q, limit).unwrap_or_default()
                    }
                };
                Ok(ok_json(serde_json::json!({
                    "q": q,
                    "query": q,
                    "conversation_id": cid,
                    "count": results.len(),
                    "results": to_value(&results)?,
                })))
            }

            // —— GET /api/v1/im/lobby —— 大厅信息（Bearer token 心跳 + 自动加入/欢迎）
            (HttpMethod::Get, ["api", "v1", "im", "lobby"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let info = {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    // 心跳 touch；首次进入自动加入 + 系统欢迎广播
                    if upsert_lobby_member(&conn, &caller.pubkey, &caller.display_name) {
                        let welcome = build_welcome_message(&caller.display_name);
                        let _ = insert_message(&conn, &welcome);
                        Self::broadcast_lobby(&self.shared.ws_hub, &welcome);
                    }
                    lobby_info(&conn)
                };
                Ok(ok_json(to_value(&info)?))
            }

            // —— GET /api/v1/im/lobby/messages[?after_id=] —— 大厅最近 50 条消息
            //    （Bearer 心跳）；带 after_id 时为增量补拉：返回严格晚于该消息
            //    的大厅消息（插入序升序，同 GET /api/v1/im/messages 语义）
            (HttpMethod::Get, ["api", "v1", "im", "lobby", "messages"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let after_id = parse_query_str(query, "after_id").filter(|s| !s.trim().is_empty());
                let list = {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    // 心跳 touch（新用户静默加入，欢迎消息仅由 GET /lobby 触发）
                    let _ = upsert_lobby_member(&conn, &caller.pubkey, &caller.display_name);
                    load_recent_lobby_messages(&conn, LOBBY_RECENT_LIMIT, after_id.as_deref())
                        .unwrap_or_default()
                };
                Ok(ok_json(to_value(&list)?))
            }

            // —— POST /api/v1/im/lobby/messages —— 发大厅消息（我的大厅＝本节点
            //    房间，消息只留本节点，**不**联邦广播——跨节点走 fed-lobby 端点）
            //    body: { content, sender_kind?, attachment? }
            //    sender = token 反查 pubkey（自报 user_id/sender_name 一律忽略）；
            //    须已是大厅成员（GET /lobby 自动加入）；mentions 服务端解析；
            //    @NexOS助手 触发内置助手异步回复（回大厅）。
            (HttpMethod::Post, ["api", "v1", "im", "lobby", "messages"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                #[derive(serde::Deserialize)]
                struct LobbySendReq {
                    content: String,
                    /// 展示层自声明（非 "agent" 一律归一 "human"）。
                    #[serde(default)]
                    sender_kind: Option<String>,
                    /// 附件（服务端按 file_id 核对存在性并覆盖 size/filename）。
                    #[serde(default)]
                    attachment: Option<AttachmentReq>,
                }
                let body: LobbySendReq = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析大厅消息请求体失败: {e}"))
                })?;
                if body.content.trim().is_empty() {
                    return Ok(error_response(400, "大厅消息内容不能为空"));
                }
                let is_member = {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    lobby_is_member(&conn, &caller.pubkey)
                };
                if !is_member {
                    return Ok(error_response(
                        403,
                        "尚未加入大厅（先 GET /api/v1/im/lobby 自动加入）",
                    ));
                }
                let attachment = match self.verify_attachment(body.attachment.as_ref()) {
                    Ok(a) => a,
                    Err(resp) => return Ok(resp),
                };
                let mentions = parse_mentions(&body.content);
                let msg = Message {
                    id: new_uuid(),
                    conversation_id: LOBBY_ID.to_string(),
                    sender_id: caller.pubkey.clone(),
                    sender_name: Some(caller.display_name.clone()),
                    content: body.content,
                    msg_type: "text".to_string(),
                    file_url: None,
                    reply_to: None,
                    created_at: now_iso(),
                    read_by: vec![caller.pubkey],
                    sender_kind: normalize_sender_kind(body.sender_kind.as_deref()),
                    mentions,
                    attachment,
                };
                {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    insert_message(&conn, &msg)?;
                }
                Self::broadcast_lobby(&self.shared.ws_hub, &msg);
                // 我的大厅与联邦完全隔离（2026-08-23 用户纠正）：消息只留本节点，
                // 不再自动联邦广播——跨节点发言走 POST /api/v1/im/fed-lobby/messages
                // agent 协调：@ 定向投递（agent-coord 组件钩子，未装配时 no-op）
                crate::handlers::agent_coord::on_im_message(&msg);
                // @NexOS助手 → 内置助手异步回复（防风暴去抖，agent 消息不触发）
                self.maybe_spawn_assistant(&msg);
                // 推送通知：匹配的注册 webhook 异步 POST（不阻塞本响应）
                self.shared.dispatch_webhooks(&msg);
                Ok(ApiResponse {
                    status: 201,
                    body: to_value(&msg)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/im/lobby/members —— 大厅成员列表（区分在线/离线）
            (HttpMethod::Get, ["api", "v1", "im", "lobby", "members"]) => {
                let Some(_caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let members = {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    load_lobby_members(&conn).unwrap_or_default()
                };
                let online_count = members.iter().filter(|m| m.online).count();
                Ok(ok_json(serde_json::json!({
                    "lobby_id": LOBBY_ID,
                    "member_count": members.len(),
                    "online_count": online_count,
                    "members": to_value(&members)?,
                })))
            }

            // —— POST /api/v1/im/files —— 上传 IM 附件（IM token）
            //    body: { filename, content_base64 }（multipart 无法穿过网关 JSON
            //    通道——与 files.rs upload 同款 base64-JSON 先例）。
            //    校验 ≤64MiB（base64 长度前置估算 + 解码后复检）；净化文件名；
            //    落 /tank/im-files/<YYYYMM>/<uuid>-<净化名>（目录自动建 +
            //    tmp+rename 原子写）；im_files 表记元数据。返回
            //    {file_id, url, filename, size_bytes, mime}——url 为相对直链
            //    （含上传者 IM token query，仅自用/可信转发）。
            (HttpMethod::Post, ["api", "v1", "im", "files"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                #[derive(serde::Deserialize)]
                struct FileUploadReq {
                    #[serde(default)]
                    filename: Option<String>,
                    #[serde(default)]
                    content_base64: Option<String>,
                }
                // 先留直链 token（req.body 稍后被 move 进解析）
                let link_token = bearer_token(&req).unwrap_or_default().to_string();
                let body: FileUploadReq = match serde_json::from_value(req.body) {
                    Ok(b) => b,
                    Err(_) => {
                        return Ok(error_response(
                            400,
                            "上传请求体须为 JSON 对象 {filename, content_base64}",
                        ))
                    }
                };
                let raw_name = body.filename.unwrap_or_default().trim().to_string();
                let b64 = body.content_base64.unwrap_or_default().trim().to_string();
                if raw_name.is_empty() || b64.is_empty() {
                    return Ok(error_response(
                        400,
                        "缺少必填字段 filename / content_base64（JSON 通道，见模块注释）",
                    ));
                }
                // 超限前置检查：按 base64 长度估算解码后大小（len*3/4），
                // 避免先把 >64MiB 的字符串解码进内存再拒绝。
                if b64.len() / 4 * 3 > IM_FILE_MAX_BYTES {
                    return Ok(error_response(413, "附件超限：单文件最大 64 MiB"));
                }
                let bytes = match base64::engine::general_purpose::STANDARD.decode(&b64) {
                    Ok(b) => b,
                    Err(e) => return Ok(error_response(400, &format!("content_base64 非法: {e}"))),
                };
                if bytes.len() > IM_FILE_MAX_BYTES {
                    return Ok(error_response(413, "附件超限：单文件最大 64 MiB"));
                }
                let size_bytes = bytes.len() as u64;
                let filename = sanitize_im_filename(&raw_name);
                let file_id = new_uuid();
                let month = chrono::Local::now().format("%Y%m").to_string();
                let dir = self.files_root().join(&month);
                let stored_name = format!("{file_id}-{filename}");
                let joined =
                    tokio::task::spawn_blocking(move || store_im_file(&dir, &stored_name, &bytes))
                        .await
                        .map_err(|e| {
                            ApiGatewayError::Internal(format!("附件落盘任务 join 失败: {e}"))
                        })?;
                let path = match joined {
                    Ok(p) => p,
                    Err((status, msg)) => return Ok(error_response(status, &msg)),
                };
                let record = ImFileRecord {
                    file_id: file_id.clone(),
                    filename: filename.clone(),
                    size_bytes,
                    mime: Some(guess_mime_im(&filename)),
                    uploader: Some(caller.pubkey.clone()),
                    path: path.to_string_lossy().into_owned(),
                    created_at: now_iso(),
                };
                {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    insert_file_record(&conn, &record)?;
                }
                // url 带上传者自身 IM token（?token= 直链场景：<img>/浏览器
                // 无法带 Bearer 头；token 24h 有效，泄露面=自己转发给谁）。
                Ok(ApiResponse {
                    status: 201,
                    body: serde_json::json!({
                        "file_id": file_id,
                        "url": im_file_url(&file_id, &link_token),
                        "filename": filename,
                        "size_bytes": record.size_bytes,
                        "mime": record.mime,
                    }),
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/im/files/:file_id?token= —— 下载 IM 附件
            //    鉴权（任一）：Authorization: Bearer <IM token> /
            //    ?token=<IM token> / ?token=<系统 admin token>（URL 直链场景）。
            //    回传与 files.rs download 同款 base64 JSON 信封 +
            //    Content-Disposition（RFC 5987）。
            (HttpMethod::Get, ["api", "v1", "im", "files", file_id]) => {
                let record = {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    find_file_record(&conn, file_id)?
                };
                let Some(record) = record else {
                    return Ok(error_response(404, &format!("附件不存在: {file_id}")));
                };
                // 鉴权：Bearer IM token > ?token= IM token > ?token= admin token
                let authorized = if self.caller(&req).is_some() {
                    true
                } else {
                    parse_query_str(query, "token")
                        .map(|t| {
                            self.auth.verify_token(&t).is_some()
                                || self
                                    .config
                                    .admin_token
                                    .as_deref()
                                    .is_some_and(|expected| expected == t)
                        })
                        .unwrap_or(false)
                };
                if !authorized {
                    return Ok(error_response(
                        401,
                        "需要 IM token（Bearer 头或 ?token=）或系统 admin token（?token=）",
                    ));
                }
                let joined = tokio::task::spawn_blocking(move || {
                    read_im_file(&record, IM_FILE_MAX_BYTES as u64)
                })
                .await
                .map_err(|e| ApiGatewayError::Internal(format!("附件读取任务 join 失败: {e}")))?;
                match joined {
                    Ok(dl) => Ok(ApiResponse {
                        status: 200,
                        body: to_value(&dl)?,
                        headers: serde_json::json!({
                            "content-disposition": content_disposition_im(&dl.filename),
                        }),
                    }),
                    Err((status, msg)) => Ok(error_response(status, &msg)),
                }
            }

            // —— POST /api/v1/im/notify/register —— 注册推送 webhook（IM token）
            //    body: { url, events?=["lobby","conversation"], conversation_id? }
            //    owner = token 反查 pubkey（自报 owner 一律忽略）；
            //    events 白名单 ["lobby","conversation"]（非法/空 → 400）；
            //    conversation_id 可选绑定（须存在且可读——群组须成员，404/403）；
            //    → 201 {id, url, owner_pubkey, events, conversation_id, status,
            //           fail_count, last_fired_at, last_error, created_at}
            (HttpMethod::Post, ["api", "v1", "im", "notify", "register"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                #[derive(serde::Deserialize)]
                struct NotifyRegisterReq {
                    url: String,
                    #[serde(default)]
                    events: Option<Vec<String>>,
                    #[serde(default)]
                    conversation_id: Option<String>,
                }
                let body: NotifyRegisterReq = match serde_json::from_value(req.body) {
                    Ok(b) => b,
                    Err(e) => return Ok(error_response(400, &format!("解析注册请求体失败: {e}"))),
                };
                let url = body.url.trim().to_string();
                if !is_valid_webhook_url(&url) {
                    return Ok(error_response(
                        400,
                        "url 非法：须为 http:// 或 https:// 开头（≤2048 字符）",
                    ));
                }
                let events = match body.events {
                    None => default_webhook_events_all(),
                    Some(ref ev) => match normalize_webhook_events(ev) {
                        Some(v) => v,
                        None => {
                            return Ok(error_response(
                                400,
                                "events 非法：仅支持 [\"lobby\",\"conversation\"] 的非空子集",
                            ))
                        }
                    },
                };
                if let Some(cid) = body
                    .conversation_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    let access = {
                        let conn = self.shared.db.lock().expect("db poisoned");
                        Self::conversation_readable(&conn, cid, &caller.pubkey)
                    };
                    match access {
                        None => {
                            return Ok(error_response(404, &format!("会话不存在: {cid}")));
                        }
                        Some(false) => {
                            return Ok(error_response(
                                403,
                                &format!(
                                    "无权访问会话 {cid}（非成员；群组先 join，大厅先 GET /lobby）"
                                ),
                            ));
                        }
                        Some(true) => {}
                    }
                } else if body.conversation_id.is_some() {
                    return Ok(error_response(
                        400,
                        "conversation_id 不能为空串（缺省=全部会话）",
                    ));
                }
                let hook = ImWebhook {
                    id: new_uuid(),
                    url,
                    owner_pubkey: caller.pubkey,
                    events,
                    conversation_id: body
                        .conversation_id
                        .map(|c| c.trim().to_string())
                        .filter(|c| !c.is_empty()),
                    status: WEBHOOK_STATUS_ACTIVE.to_string(),
                    fail_count: 0,
                    last_fired_at: None,
                    last_error: None,
                    created_at: now_iso(),
                };
                {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    insert_webhook(&conn, &hook)?;
                }
                Ok(ApiResponse {
                    status: 201,
                    body: to_value(&hook)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/im/notify/list —— 列出自己的 webhook（IM token）
            //    owner 身份过滤（看不到别人的，别人也看不到你的）
            (HttpMethod::Get, ["api", "v1", "im", "notify", "list"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let list = {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    load_webhooks_by_owner(&conn, &caller.pubkey).unwrap_or_default()
                };
                Ok(ok_json(to_value(&list)?))
            }

            // —— DELETE /api/v1/im/notify/:id —— 注销 webhook（IM token，仅 owner）
            //    他人注销 → 403（注册表不动）；未知 id → 404
            (HttpMethod::Delete, ["api", "v1", "im", "notify", id]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let hook = {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    find_webhook(&conn, id)?
                };
                let Some(hook) = hook else {
                    return Ok(error_response(404, &format!("webhook 不存在: {id}")));
                };
                if hook.owner_pubkey != caller.pubkey {
                    return Ok(error_response(403, "仅 owner 可注销该 webhook"));
                }
                {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    delete_webhook(&conn, id)?;
                }
                Ok(ok_json(
                    serde_json::json!({"ok": true, "id": id, "deleted": true}),
                ))
            }

            // —— GET /api/v1/im/federation —— 联邦接收开关状态（IM token）
            //    → {enabled, note}：关闭仅暂停"接收"其他节点的联邦大厅消息；
            //    本地消息与联邦发送不受影响（note 说明当前语义）。
            (HttpMethod::Get, ["api", "v1", "im", "federation"]) => {
                let Some(_caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let enabled = self.federation().fed_enabled();
                Ok(ok_json(serde_json::json!({
                    "enabled": enabled,
                    "note": fed_toggle_note(enabled),
                })))
            }

            // —— POST /api/v1/im/federation —— 切换联邦接收开关
            //    鉴权（任一）：链上 IM token / 系统 admin token（Bearer）；
            //    body: {enabled: bool}；关闭 = ingest 入口短路（远程联邦大厅
            //    消息不落地不广播），本地消息/发送照常；打开恢复接收。
            (HttpMethod::Post, ["api", "v1", "im", "federation"]) => {
                #[derive(serde::Deserialize)]
                struct FedToggleReq {
                    enabled: bool,
                }
                if !self.admin_ok(&req) && self.caller(&req).is_none() {
                    return Ok(error_response(
                        401,
                        "需要 IM token 或系统 admin token（Authorization: Bearer）",
                    ));
                }
                let body: FedToggleReq = match serde_json::from_value(req.body) {
                    Ok(b) => b,
                    Err(_) => {
                        return Ok(error_response(400, "body 须为 JSON 对象 {enabled: bool}"))
                    }
                };
                let enabled = self.federation().set_fed_enabled(body.enabled);
                eprintln!("[fed] 联邦接收开关 → {enabled}");
                Ok(ok_json(serde_json::json!({
                    "enabled": enabled,
                    "note": fed_toggle_note(enabled),
                })))
            }

            // —— GET /api/v1/im/lobby/access —— 大厅开放开关状态
            //    鉴权（任一）：链上 IM token / 系统 admin token（Bearer）。
            //    → {lobby_public, note}：true（开发期缺省开放）= 允许只读浏览
            //    （+ 远程发言落地）；false = 其他节点在节点发现页/IM 页看不到
            //    本机大厅。
            (HttpMethod::Get, ["api", "v1", "im", "lobby", "access"]) => {
                if !self.admin_ok(&req) && self.caller(&req).is_none() {
                    return Ok(auth_required());
                }
                let lobby_public = self.federation().lobby_public();
                Ok(ok_json(serde_json::json!({
                    "lobby_public": lobby_public,
                    "note": lobby_access_note(lobby_public),
                })))
            }

            // —— POST /api/v1/im/lobby/access —— 切换大厅开放开关
            //    鉴权（任一）：admin token / IM token；body: {lobby_public: bool}。
            (HttpMethod::Post, ["api", "v1", "im", "lobby", "access"]) => {
                #[derive(serde::Deserialize)]
                struct LobbyAccessReq {
                    lobby_public: bool,
                }
                if !self.admin_ok(&req) && self.caller(&req).is_none() {
                    return Ok(error_response(
                        401,
                        "需要 IM token 或系统 admin token（Authorization: Bearer）",
                    ));
                }
                let body: LobbyAccessReq = match serde_json::from_value(req.body) {
                    Ok(b) => b,
                    Err(_) => {
                        return Ok(error_response(
                            400,
                            "body 须为 JSON 对象 {lobby_public: bool}",
                        ))
                    }
                };
                let lobby_public = self.federation().set_lobby_public(body.lobby_public);
                eprintln!("[fed] 大厅开放开关 → {lobby_public}");
                Ok(ok_json(serde_json::json!({
                    "lobby_public": lobby_public,
                    "note": lobby_access_note(lobby_public),
                })))
            }

            // —— GET /api/v1/im/lobby/remote/:node_id —— 远程大厅镜像
            //    （节点发现页「进入 IM」跳转后的远程 Tab 数据源）：经 P2P 向
            //    对方发 im_lobby_query 并限时等待应答（?timeout_ms= 300..=8000，
            //    默认 4000）——200 {node_id, public, messages?}：
            //    public=true → messages=最近 20 条脱敏消息（只读镜像）；
            //    public=false → error="denied"（对方未开放）；
            //    public=null → error="timeout"（对方无应答/不可达）。
            //    P2P 未启用 503；node_id 非 0x+66hex 400。
            (HttpMethod::Get, ["api", "v1", "im", "lobby", "remote", node_id]) => {
                let Some(_caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let Some(node) = os_p2p::NodeId::parse(node_id) else {
                    return Ok(error_response(
                        400,
                        "node_id 非法：应为 0x + 66 hex（33 字节压缩 secp256k1）",
                    ));
                };
                let timeout = lobby_query_timeout(parse_query_str(query, "timeout_ms").as_deref());
                match self.federation().remote_lobby(&node, timeout).await {
                    Some(view) if view.public => Ok(ok_json(serde_json::json!({
                        "node_id": node_id,
                        "public": true,
                        "messages": view.messages,
                    }))),
                    Some(view) => Ok(ok_json(serde_json::json!({
                        "node_id": node_id,
                        "public": false,
                        "error": view.error.as_deref().unwrap_or("denied"),
                    }))),
                    None => Ok(ok_json(serde_json::json!({
                        "node_id": node_id,
                        "public": serde_json::Value::Null,
                        "error": "timeout",
                    }))),
                }
            }

            // —— POST /api/v1/im/lobby/remote/:node_id/messages —— 远程大厅发言
            //    body: {content}；sender=本机 IM token 反查 pubkey。先阻塞查询
            //    对方开放状态：denied → 403；超时 → 504；开放 → 经 P2P 发
            //    im_lobby_post（fire-and-forget，落地与否在对端开关/接收开关
            //    体现——刷新镜像即可见）。远程通道不承载附件。
            (HttpMethod::Post, ["api", "v1", "im", "lobby", "remote", node_id, "messages"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let Some(node) = os_p2p::NodeId::parse(node_id) else {
                    return Ok(error_response(
                        400,
                        "node_id 非法：应为 0x + 66 hex（33 字节压缩 secp256k1）",
                    ));
                };
                #[derive(serde::Deserialize)]
                struct RemoteSendReq {
                    content: String,
                }
                let body: RemoteSendReq = match serde_json::from_value(req.body) {
                    Ok(b) => b,
                    Err(_) => {
                        return Ok(error_response(400, "body 须为 JSON 对象 {content: string}"))
                    }
                };
                if body.content.trim().is_empty() {
                    return Ok(error_response(400, "远程大厅消息内容不能为空"));
                }
                let timeout = lobby_query_timeout(parse_query_str(query, "timeout_ms").as_deref());
                let fed = self.federation();
                match fed.remote_lobby(&node, timeout).await {
                    None => Ok(error_response(504, "对方节点无应答（超时）")),
                    Some(v) if !v.public => Ok(error_response(403, "对方未开放 IM 大厅")),
                    Some(_) => {
                        if !fed.send_fed_to(
                            &node,
                            build_lobby_post_payload(
                                &fed.node_name(),
                                &caller.pubkey,
                                &caller.display_name,
                                &body.content,
                            ),
                        ) {
                            return Ok(error_response(503, "P2P 未启用（NEXOS_P2P_ENABLE=1）"));
                        }
                        Ok(ok_json(serde_json::json!({
                            "ok": true,
                            "node_id": node_id,
                            "note": "已发送到对方大厅（刷新镜像可见；落地以对方开放/接收开关为准）",
                        })))
                    }
                }
            }

            // —— GET /api/v1/im/fed-lobby —— 联邦大厅信息（Bearer 心跳 + 加入）
            //    联邦大厅＝跨节点共享频道（conversation_id 恒为 FED_LOBBY_ID），
            //    与我的大厅完全隔离；加入复用本节点在场表（不触发欢迎广播）。
            (HttpMethod::Get, ["api", "v1", "im", "fed-lobby"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let info = {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    // 心跳 touch（加入联邦大厅＝加入本节点 IM 在场；联邦大厅
                    // 无欢迎系统消息——它是跨节点频道，不属于任何单节点事件）
                    let _ = upsert_lobby_member(&conn, &caller.pubkey, &caller.display_name);
                    fed_lobby_info(&conn)
                };
                Ok(ok_json(to_value(&info)?))
            }

            // —— GET /api/v1/im/fed-lobby/messages[?after_id=] —— 联邦大厅最近
            //    50 条消息（Bearer 心跳）；带 after_id 时为增量补拉：返回严格
            //    晚于该消息的 fed-lobby 会话消息（插入序升序，同 GET
            //    /api/v1/im/messages 语义）。
            (HttpMethod::Get, ["api", "v1", "im", "fed-lobby", "messages"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let after_id = parse_query_str(query, "after_id").filter(|s| !s.trim().is_empty());
                let list = {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    // 心跳 touch（与 GET /fed-lobby 同语义：加入即在场）
                    let _ = upsert_lobby_member(&conn, &caller.pubkey, &caller.display_name);
                    load_recent_conversation_messages(
                        &conn,
                        FED_LOBBY_ID,
                        LOBBY_RECENT_LIMIT,
                        after_id.as_deref(),
                    )
                    .unwrap_or_default()
                };
                Ok(ok_json(to_value(&list)?))
            }

            // —— POST /api/v1/im/fed-lobby/messages —— 联邦大厅发言（可写会话）
            //    body: { content, sender_kind? }；sender = token 反查 pubkey
            //    （自报身份一律忽略）。路由（与我的大厅完全隔离）：
            //    ① 本地写入 im_messages（conversation_id=fed-lobby）+ WS 广播
            //       **即时**（im_fed_lobby_message 帧，本节点体验不变）；
            //    ② P2P 联邦广播经**延迟队列**发出（fed=im_fed_lobby_message；
            //       常态 10s、同一 sender 60s 内多次发言升至 60s——2026-08-24
            //       时延节流：不限次/不拒绝/不丢消息，只延后广播时刻；对端
            //       收到后落其 fed-lobby 会话，sender_id=fed:<node>:<pubkey>）；
            //    ③ 响应透出 federate_delay_secs + note。
            //    须已加入（GET /fed-lobby 自动加入）；联邦通道不承载附件；
            //    @NexOS助手不触发（助手是本节点 AI，不跨节点回答）。
            (HttpMethod::Post, ["api", "v1", "im", "fed-lobby", "messages"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                #[derive(serde::Deserialize)]
                struct FedLobbySendReq {
                    content: String,
                    /// 展示层自声明（非 "agent" 一律归一 "human"）。
                    #[serde(default)]
                    sender_kind: Option<String>,
                }
                let body: FedLobbySendReq = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析联邦大厅消息请求体失败: {e}"))
                })?;
                if body.content.trim().is_empty() {
                    return Ok(error_response(400, "联邦大厅消息内容不能为空"));
                }
                let is_member = {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    lobby_is_member(&conn, &caller.pubkey)
                };
                if !is_member {
                    return Ok(error_response(
                        403,
                        "尚未加入联邦大厅（先 GET /api/v1/im/fed-lobby 自动加入）",
                    ));
                }
                let mentions = parse_mentions(&body.content);
                let msg = Message {
                    id: new_uuid(),
                    conversation_id: FED_LOBBY_ID.to_string(),
                    sender_id: caller.pubkey.clone(),
                    sender_name: Some(caller.display_name.clone()),
                    content: body.content,
                    msg_type: "text".to_string(),
                    file_url: None,
                    reply_to: None,
                    created_at: now_iso(),
                    read_by: vec![caller.pubkey],
                    sender_kind: normalize_sender_kind(body.sender_kind.as_deref()),
                    mentions,
                    attachment: None, // 联邦通道不承载附件（文件不出本节点）
                };
                {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    insert_message(&conn, &msg)?;
                }
                Self::broadcast_fed_lobby(&self.shared.ws_hub, &msg);
                // P2P 联邦广播（2026-08-24 起带时延节流）：本地落库 + WS 广播
                // 上方已**即时**完成（本节点体验不变）；联邦广播经延迟队列
                // 到期发出（常态 10s，同一 sender 60s 内多次发言升至 60s——
                // 不限次、不拒绝、不丢消息，仅延后广播时刻）
                let fed_delay = self.federation().enqueue_fed_lobby_broadcast(&msg);
                // 推送通知：匹配的注册 webhook 异步 POST（不阻塞本响应）
                self.shared.dispatch_webhooks(&msg);
                // 响应透出联邦时延（前端可据此提示；本期前端不改）
                let mut body = to_value(&msg)?;
                let delay_secs = fed_delay.as_secs();
                let note = if delay_secs == 0 {
                    "本条不参与联邦广播（agent/系统消息仅本节点可见）".to_string()
                } else {
                    format!(
                        "联邦广播将于 {delay_secs} 秒后发出（联邦大厅节流：常驻 10s，\
                         分钟内多次发言升至 60s）"
                    )
                };
                if let serde_json::Value::Object(map) = &mut body {
                    map.insert("federate_delay_secs".into(), serde_json::json!(delay_secs));
                    map.insert("note".into(), serde_json::Value::String(note));
                }
                Ok(ApiResponse {
                    status: 201,
                    body,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/im/dm/access —— 直通消息开放开关状态
            //    鉴权（任一）：链上 IM token / 系统 admin token（Bearer）。
            //    → {dm_open, note}：true（开发期缺省允许）= 其他身份可向本节点
            //    身份发直通消息；false = 一律不收（403 / 跨节点丢弃）。
            (HttpMethod::Get, ["api", "v1", "im", "dm", "access"]) => {
                if !self.admin_ok(&req) && self.caller(&req).is_none() {
                    return Ok(auth_required());
                }
                let dm_open = self.federation().dm_open();
                Ok(ok_json(serde_json::json!({
                    "dm_open": dm_open,
                    "note": dm_access_note(dm_open),
                })))
            }

            // —— POST /api/v1/im/dm/access —— 切换直通消息开放开关
            //    鉴权（任一）：admin token / IM token；body: {dm_open: bool}。
            (HttpMethod::Post, ["api", "v1", "im", "dm", "access"]) => {
                #[derive(serde::Deserialize)]
                struct DmAccessReq {
                    dm_open: bool,
                }
                if !self.admin_ok(&req) && self.caller(&req).is_none() {
                    return Ok(error_response(
                        401,
                        "需要 IM token 或系统 admin token（Authorization: Bearer）",
                    ));
                }
                let body: DmAccessReq = match serde_json::from_value(req.body) {
                    Ok(b) => b,
                    Err(_) => {
                        return Ok(error_response(400, "body 须为 JSON 对象 {dm_open: bool}"))
                    }
                };
                let dm_open = self.federation().set_dm_open(body.dm_open);
                eprintln!("[dm] 直通消息开放开关 → {dm_open}");
                Ok(ok_json(serde_json::json!({
                    "dm_open": dm_open,
                    "note": dm_access_note(dm_open),
                })))
            }

            // —— POST /api/v1/im/dm —— 发起点对点直通消息（DM，仅双方可见）
            //    body: {to_pubkey, content, sender_kind?, to_node?}；发起者 =
            //    token 反查 pubkey（自报一律忽略）。路由（按序判定）：
            //    ① 对方身份在本节点（大厅在场或 WS 在线）→ 本地投递：dm_open
            //       检查（关 → 403「对方未开放直通消息」）→ 确定性 dm-* 会话
            //       落库 + WS 定向推给收发双方（send_to_n，非全员广播）；
            //    ② 对方不在本节点 → P2P overlay **定向发送**到对方节点（fed
            //       kind `im_dm`，非广播）：目标节点 = body.to_node（显式指定）
            //       或 im_dm_peers 登记（收过对方跨节点 DM 的回程路由）；无路由
            //       → 404。发送侧本地同留一份（同确定性会话/消息 id，双端一致），
            //       落地与否以对方节点 dm_open 为准（fire-and-forget）。
            //    不触发内置助手/agent 钩子/webhook——DM 是严格双端通道。
            (HttpMethod::Post, ["api", "v1", "im", "dm"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                #[derive(serde::Deserialize)]
                struct DmSendReq {
                    to_pubkey: String,
                    content: String,
                    /// 展示层自声明（非 "agent" 一律归一 "human"）。
                    #[serde(default)]
                    sender_kind: Option<String>,
                    /// 跨节点路由的目标 NodeID（0x+66hex；缺省按 im_dm_peers
                    /// 登记回程路由，再缺省 404）。
                    #[serde(default)]
                    to_node: Option<String>,
                }
                let body: DmSendReq = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析直通消息请求体失败: {e}"))
                })?;
                let to = body.to_pubkey.trim().to_string();
                if parse_im_pubkey(&to).is_none() {
                    return Ok(error_response(
                        400,
                        "to_pubkey 非法：应为 0x + 66 hex（33 字节压缩 secp256k1）",
                    ));
                }
                if to == caller.pubkey {
                    return Ok(error_response(400, "不能给自己发直通消息"));
                }
                let content = body.content.trim().to_string();
                if content.is_empty() || content.chars().count() > 4000 {
                    return Ok(error_response(
                        400,
                        "直通消息内容不能为空且不超过 4000 字符",
                    ));
                }
                let mentions = parse_mentions(&content);
                let cid = dm_conversation_id(&caller.pubkey, &to);
                // —— 路由判定：对方身份是否在本节点 ——
                let local = {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    self.shared.identity_local(&conn, &to)
                };
                if local {
                    if !self.federation().dm_open() {
                        return Ok(error_response(403, "对方未开放直通消息"));
                    }
                    let msg = Message {
                        id: new_uuid(),
                        conversation_id: cid.clone(),
                        sender_id: caller.pubkey.clone(),
                        sender_name: Some(caller.display_name.clone()),
                        content,
                        msg_type: "text".to_string(),
                        file_url: None,
                        reply_to: None,
                        created_at: now_iso(),
                        read_by: vec![caller.pubkey.clone()],
                        sender_kind: normalize_sender_kind(body.sender_kind.as_deref()),
                        mentions,
                        attachment: None, // DM 通道不承载附件（文件不出双端）
                    };
                    {
                        let conn = self.shared.db.lock().expect("db poisoned");
                        self.shared
                            .ensure_dm_conversation(&conn, &cid, &caller.pubkey, &to);
                        insert_message(&conn, &msg)?;
                    }
                    // 定向 WS：收发双方各收到一份（其他订阅者不可见）
                    self.shared
                        .push_dm_ws(&cid, &msg, &[caller.pubkey.as_str(), to.as_str()]);
                    return Ok(ApiResponse {
                        status: 201,
                        body: serde_json::json!({
                            "message": msg,
                            "conversation_id": cid,
                            "route": "local",
                        }),
                        headers: serde_json::json!({}),
                    });
                }
                // —— 跨节点：解析定向目标节点（显式 to_node > im_dm_peers 登记）——
                let target: Option<os_p2p::NodeId> = match body
                    .to_node
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    Some(n) => match os_p2p::NodeId::parse(n) {
                        Some(id) => Some(id),
                        None => {
                            return Ok(error_response(
                                400,
                                "to_node 非法：应为 0x + 66 hex（33 字节压缩 secp256k1）",
                            ));
                        }
                    },
                    None => {
                        let conn = self.shared.db.lock().expect("db poisoned");
                        lookup_dm_peer_node(&conn, &to).and_then(|n| os_p2p::NodeId::parse(&n))
                    }
                };
                let Some(node) = target else {
                    return Ok(error_response(
                        404,
                        "对方不在本节点，且未知对方节点（对方先经联邦大厅/远程大厅联系你，\
                         或显式提供 to_node）",
                    ));
                };
                // 载荷：消息 id = 载荷 hash（对端 ingest 去重 + 双端同 id）
                let ts = now_iso();
                let msg_id = dm_message_id(&caller.pubkey, &to, &content, &ts);
                let payload = serde_json::json!({
                    "fed": FED_KIND_IM_DM,
                    "msg_id": msg_id,
                    "from_pubkey": caller.pubkey,
                    "from_name": caller.display_name,
                    "to_pubkey": to,
                    "content": content,
                    "node": self.federation().node_name(),
                    "ts": ts,
                });
                if !self.federation().send_fed_to(&node, payload) {
                    return Ok(error_response(503, "P2P 未启用（NEXOS_P2P_ENABLE=1）"));
                }
                // 发送侧本地留档（同确定性会话/消息 id——双端各自落库天然对齐；
                // 对端回环重投时按 msg_id 去重）
                let msg = Message {
                    id: msg_id,
                    conversation_id: cid.clone(),
                    sender_id: caller.pubkey.clone(),
                    sender_name: Some(caller.display_name.clone()),
                    content,
                    msg_type: "text".to_string(),
                    file_url: None,
                    reply_to: None,
                    created_at: ts,
                    read_by: vec![caller.pubkey.clone()],
                    sender_kind: normalize_sender_kind(body.sender_kind.as_deref()),
                    mentions: Vec::new(),
                    attachment: None,
                };
                {
                    let conn = self.shared.db.lock().expect("db poisoned");
                    self.shared
                        .ensure_dm_conversation(&conn, &cid, &caller.pubkey, &to);
                    let _ = insert_message(&conn, &msg); // 已存在（回环）按已发处理
                }
                self.shared
                    .push_dm_ws(&cid, &msg, &[caller.pubkey.as_str()]);
                Ok(ApiResponse {
                    status: 201,
                    body: serde_json::json!({
                        "message": msg,
                        "conversation_id": cid,
                        "route": "p2p",
                        "note": "已定向发送到对方节点（落地以对方直通消息开关为准）",
                    }),
                    headers: serde_json::json!({}),
                })
            }

            // —— 未覆盖路由 —— 兜底 404（Ok，非 Err，便于上层定位）
            _ => Ok(error_response(404, "im: 未匹配的路由")),
        }
    }
}

// ----------------------------------------------------------------------------
// 内部辅助
// ----------------------------------------------------------------------------

/// `POST /api/v1/im/auth/challenge`（公开：签发挑战 nonce）
const PATH_AUTH_CHALLENGE: &str = "/api/v1/im/auth/challenge";
/// `POST /api/v1/im/auth/verify`（公开：验签 + 签发 IM token）
const PATH_AUTH_VERIFY: &str = "/api/v1/im/auth/verify";
/// `GET/POST /api/v1/im/conversations`
const PATH_CONV_LIST: &str = "/api/v1/im/conversations";
/// `GET/POST /api/v1/im/conversations/:id/messages`
const PATH_CONV_MESSAGES: &str = "/api/v1/im/conversations/:id/messages";
/// `GET /api/v1/im/messages?conversation_id=&after_id=&limit=`（离线补拉）
const PATH_MESSAGES_CATCHUP: &str = "/api/v1/im/messages";
/// `GET/POST /api/v1/im/groups`
const PATH_GROUPS: &str = "/api/v1/im/groups";
/// `POST /api/v1/im/groups/:id/join`
const PATH_GROUP_JOIN: &str = "/api/v1/im/groups/:id/join";
/// `POST /api/v1/im/groups/:id/leave`
const PATH_GROUP_LEAVE: &str = "/api/v1/im/groups/:id/leave";
/// `GET /api/v1/im/groups/:id/members`
const PATH_GROUP_MEMBERS: &str = "/api/v1/im/groups/:id/members";
/// `GET/POST /api/v1/im/peers`
const PATH_PEERS: &str = "/api/v1/im/peers";
/// `GET /api/v1/im/status`
const PATH_STATUS: &str = "/api/v1/im/status";
/// `POST /api/v1/im/messages/:id/read`
const PATH_MSG_READ: &str = "/api/v1/im/messages/:id/read";
/// `GET /api/v1/im/conversations/:id/unread`
const PATH_CONV_UNREAD: &str = "/api/v1/im/conversations/:id/unread";
/// `GET /api/v1/im/search`
const PATH_SEARCH: &str = "/api/v1/im/search";
/// `GET /api/v1/im/lobby`
const PATH_LOBBY: &str = "/api/v1/im/lobby";
/// `GET/POST /api/v1/im/lobby/messages`
const PATH_LOBBY_MESSAGES: &str = "/api/v1/im/lobby/messages";
/// `GET /api/v1/im/lobby/members`
const PATH_LOBBY_MEMBERS: &str = "/api/v1/im/lobby/members";
/// `POST /api/v1/im/files`（上传附件，IM token）
const PATH_FILES: &str = "/api/v1/im/files";
/// `GET /api/v1/im/files/:file_id`（下载附件，IM token 头/`?token=` 或 admin）
const PATH_FILE_DOWNLOAD: &str = "/api/v1/im/files/:file_id";
/// `POST /api/v1/im/notify/register`（注册推送 webhook，IM token）
const PATH_NOTIFY_REGISTER: &str = "/api/v1/im/notify/register";
/// `GET /api/v1/im/notify/list`（列出自己的 webhook，IM token）
const PATH_NOTIFY_LIST: &str = "/api/v1/im/notify/list";
/// `DELETE /api/v1/im/notify/:id`（注销 webhook，IM token，仅 owner）
const PATH_NOTIFY_UNREGISTER: &str = "/api/v1/im/notify/:id";
/// `GET/POST /api/v1/im/federation`（联邦接收开关：GET 读状态 IM token；
/// POST 切换 admin 或 IM token，handler 内验）
const PATH_FEDERATION: &str = "/api/v1/im/federation";
/// `GET/POST /api/v1/im/lobby/access`（大厅开放开关：是否允许其他节点浏览
/// 本机大厅，默认 false；admin 或 IM token，handler 内验）
const PATH_LOBBY_ACCESS: &str = "/api/v1/im/lobby/access";
/// `GET /api/v1/im/lobby/remote/:node_id`（远程大厅镜像：开放状态 + 最近 20
/// 条脱敏消息；IM token；`?timeout_ms=` 300..=8000 默认 4000）
const PATH_LOBBY_REMOTE: &str = "/api/v1/im/lobby/remote/:node_id";
/// `POST /api/v1/im/lobby/remote/:node_id/messages`（远程大厅发言；IM token；
/// 对方未开放 403 / 无应答 504）
const PATH_LOBBY_REMOTE_MESSAGES: &str = "/api/v1/im/lobby/remote/:node_id/messages";
/// `GET /api/v1/im/fed-lobby`（联邦大厅信息 + 心跳加入，IM token）
const PATH_FED_LOBBY: &str = "/api/v1/im/fed-lobby";
/// `GET/POST /api/v1/im/fed-lobby/messages`（联邦大厅历史/增量与发言，IM token）
const PATH_FED_LOBBY_MESSAGES: &str = "/api/v1/im/fed-lobby/messages";
/// `POST /api/v1/im/dm`（发起点对点直通消息 {to_pubkey, content, to_node?}；
/// IM token——本地投递或经 P2P 定向路由到对方节点）
const PATH_DM: &str = "/api/v1/im/dm";
/// `GET/POST /api/v1/im/dm/access`（直通消息开放开关：是否允许其他身份发给
/// 本节点身份私信，开发期默认 true；admin 或 IM token，handler 内验）
const PATH_DM_ACCESS: &str = "/api/v1/im/dm/access";

/// 本 handler 注册时的组件名（`RouteSpec::handler_component`）。
const COMPONENT: &str = "im";

/// 大厅固定 id（im_lobby.id / 大厅消息 conversation_id 恒为该值）。
pub const LOBBY_ID: &str = "lobby";
/// 联邦大厅固定 id（跨节点共享频道的 conversation_id）——与「我的大厅」
/// （[`LOBBY_ID`]）**完全隔离**的独立会话：发言广播全部已连接节点，接收其他
/// 节点的联邦大厅消息（2026-08-23 用户纠正：联邦大厅是可写会话，非只读聚合）。
pub const FED_LOBBY_ID: &str = "fed-lobby";
/// 在线心跳窗口：last_seen 距今 < 60s 判定在线。
pub const ONLINE_WINDOW_SECS: i64 = 60;
/// 大厅消息拉取上限（最近 50 条）。
pub const LOBBY_RECENT_LIMIT: usize = 50;

// ----------------------------------------------------------------------------
// IM 联邦（P3，docs/NEXOS_P2P_NETWORK_DESIGN.md §8）：联邦大厅（fed-lobby，
// 跨节点共享频道）消息经 os-p2p 广播给已连接 peer + 接收远程联邦消息落地本地
// fed-lobby 会话 + WS 广播（2026-08-23 起「我的大厅」lobby 与联邦完全隔离，
// 不再自动广播）
// ----------------------------------------------------------------------------

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
const FED_SEEN_LIMIT: usize = 1000;

/// 身份冲突提示去重窗口（同一冲突源 5 分钟内只写一条系统警告——防大厅刷屏）。
const IDENTITY_WARN_DEDUPE: Duration = Duration::from_secs(5 * 60);

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
const FED_THROTTLE_WINDOW: Duration = Duration::from_secs(60);

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
struct FedThrottle {
    /// 常态时延（生产恒 [`FED_THROTTLE_SHORT`]；测试可注入 1ms 级短时延）。
    short_delay: Duration,
    /// 升级时延（生产恒 [`FED_THROTTLE_LONG`]；测试可注入）。
    long_delay: Duration,
    /// sender_id → 近 60s 窗口内发言时刻（入队时 push，超窗顺手清理）。
    history: HashMap<String, Vec<Instant>>,
}

impl FedThrottle {
    /// 生产构造：10s/60s 双时延（[`with_fed_throttle_delays`] 测试注入覆盖）。
    fn new(short_delay: Duration, long_delay: Duration) -> Self {
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
    fn delay_for(&mut self, sender: &str, now: Instant) -> Duration {
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
struct FedBroadcastJob {
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
fn sanitize_fed_node_im(node: &str) -> Option<String> {
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
fn build_im_fed_lobby_payload_with_kind(
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
fn sanitize_lobby_message(m: &Message) -> LobbyViewMessage {
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
fn parse_lobby_reply(payload: &serde_json::Value) -> Option<RemoteLobbyView> {
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
fn sanitize_fed_sender(id: &str) -> Option<String> {
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
fn lobby_query_timeout(raw: Option<&str>) -> Duration {
    let ms = raw
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(4000)
        .clamp(300, 8000);
    Duration::from_millis(ms)
}

/// 大厅开放开关状态说明文案（GET/POST /api/v1/im/lobby/access 响应 `note`）。
fn lobby_access_note(lobby_public: bool) -> &'static str {
    if lobby_public {
        "大厅已开放（开发期缺省）：同网络的 NexOS 节点可在节点发现页/IM 页只读浏览本机大厅（最近 20 条，不含附件），并可通过远程通道发言"
    } else {
        "大厅未开放：其他节点无法浏览本机 IM 大厅；开启后同网络的 NexOS 节点可只读浏览（不含附件内容）"
    }
}

/// 直通消息开放开关状态说明文案（GET/POST /api/v1/im/dm/access 响应 `note`）。
fn dm_access_note(dm_open: bool) -> &'static str {
    if dm_open {
        "直通消息已开放（开发阶段缺省允许）：其他链上身份可直接向你发私信（只有双方可见）；关闭后对方发送将被拒绝"
    } else {
        "直通消息已关闭：其他身份发给你的直通消息将被拒绝（403/跨节点丢弃）；你自己发出的私信不受影响"
    }
}

// ----------------------------------------------------------------------------
// DM（点对点直通消息）——确定性会话 id / 消息 id / 成员与对端登记
// ----------------------------------------------------------------------------

/// DM（直通消息）会话 id 前缀（`im_conversations.id` 以此开头的会话即 DM）。
pub const DM_CONV_PREFIX: &str = "dm-";

/// 会话 id 是否是 DM（直通消息会话）。
fn is_dm_conversation(cid: &str) -> bool {
    cid.starts_with(DM_CONV_PREFIX)
}

/// DM 确定性会话 id（纯函数）：`dm-` + sha256(`<小 pubkey>\n<大 pubkey>`)
/// 前 8 字节 hex——**与发起方向无关**（先排序再散列），双方节点各自落库
/// 天然得到同一会话 id。
#[must_use]
pub fn dm_conversation_id(a: &str, b: &str) -> String {
    use sha2::Digest;
    let (x, y) = if a <= b { (a, b) } else { (b, a) };
    let digest = sha2::Sha256::digest(format!("{x}\n{y}").as_bytes());
    format!("{DM_CONV_PREFIX}{}", hex::encode(&digest[..8]))
}

/// DM 跨节点消息 id（纯函数）：`dm-msg-` + sha256(载荷要素) 前 12 字节 hex
/// ——发送端与接收端对同一条消息算出同一 id，天然去重（重投/回环只落一份）。
#[must_use]
pub fn dm_message_id(from: &str, to: &str, content: &str, ts: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(format!("{from}\n{to}\n{content}\n{ts}").as_bytes());
    format!("{DM_CONV_PREFIX}msg-{}", hex::encode(&digest[..12]))
}

/// DM 会话展示名（纯函数，对称确定性）：`直通 <a 短显> · <b 短显>`——
/// 双端一致；前端侧栏按 members 里的「对方」覆盖展示为对方名字。
fn dm_conversation_name(a: &str, b: &str) -> String {
    let (x, y) = if a <= b { (a, b) } else { (b, a) };
    format!("直通 {} · {}", short_pubkey_label(x), short_pubkey_label(y))
}

/// pubkey 短显标签：`0x1234…cdef`（前 6 后 4 字符；短串原样——UTF-8 边界安全）。
fn short_pubkey_label(pubkey: &str) -> String {
    let chars: Vec<char> = pubkey.chars().collect();
    if chars.len() > 14 {
        let head: String = chars[..6].iter().collect();
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("{head}…{tail}")
    } else {
        pubkey.to_string()
    }
}

/// 某 DM 会话的成员列表（im_dm_members，按加入序；空 = 非成员表会话）。
fn load_dm_members(conn: &Connection, cid: &str) -> Vec<String> {
    let Ok(mut stmt) = conn
        .prepare("SELECT user_id FROM im_dm_members WHERE conversation_id=? ORDER BY joined_at")
    else {
        return Vec::new();
    };
    let Ok(iter) = stmt.query_map(params![cid], |r| r.get::<_, String>(0)) else {
        return Vec::new();
    };
    iter.filter_map(Result::ok).collect()
}

/// 某身份是否是某 DM 会话成员（收发双方之一）。
fn dm_is_member(conn: &Connection, cid: &str, pubkey: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM im_dm_members WHERE conversation_id=? AND user_id=?",
        params![cid, pubkey],
        |_| Ok(true),
    )
    .optional()
    .unwrap_or(Some(false))
    .unwrap_or(false)
}

/// 登记跨节点 DM 对端（ingest 回程路由）：pubkey → 发送方 NodeID（P2P 层
/// 验签真值，非载荷自报）+ 展示名。回复时 POST /im/dm 无需带 to_node。
fn upsert_dm_peer(conn: &Connection, pubkey: &str, node_id_hex: &str, display_name: &str) {
    let name: Option<String> = if display_name.trim().is_empty() {
        None
    } else {
        Some(display_name.chars().take(64).collect())
    };
    let _ = conn.execute(
        "INSERT INTO im_dm_peers (pubkey,node,display_name,last_seen) VALUES (?,?,?,?)
         ON CONFLICT(pubkey) DO UPDATE SET node=excluded.node,
            display_name=COALESCE(excluded.display_name, im_dm_peers.display_name),
            last_seen=excluded.last_seen",
        params![pubkey, node_id_hex, name, now_iso()],
    );
}

/// 查某身份的跨节点 DM 路由（None = 未登记，本节点不曾收过该身份的 DM）。
fn lookup_dm_peer_node(conn: &Connection, pubkey: &str) -> Option<String> {
    conn.query_row(
        "SELECT node FROM im_dm_peers WHERE pubkey=?",
        params![pubkey],
        |r| r.get::<_, Option<String>>(0),
    )
    .optional()
    .ok()
    .flatten()
    .flatten()
}

impl ImShared {
    /// 某链上身份是否「在本节点」：大厅在场（GET /lobby 的 Bearer 心跳自动
    /// 加入 im_lobby_members）或当前有 WS 在线订阅（by_user 表）——两者任一
    /// 命中即视为本地身份，DM 走本地投递；否则需要跨节点定向路由。
    fn identity_local(&self, conn: &Connection, pubkey: &str) -> bool {
        if lobby_is_member(conn, pubkey) {
            return true;
        }
        self.ws_hub
            .as_ref()
            .is_some_and(|hub| hub.subscriber_count_for(pubkey) > 0)
    }

    /// 确保 DM 会话行 + 双方成员存在（幂等，**调用方持 db 锁**）：已存在 →
    /// 原样返回（保留首次创建的 created_by/时间）；不存在 → 插入确定性
    /// `dm-` 会话（name 对称确定性）+ 双方成员各一行。双端各自调用得到同一行。
    fn ensure_dm_conversation(
        &self,
        conn: &Connection,
        cid: &str,
        a: &str,
        b: &str,
    ) -> Conversation {
        let existing: Option<(String, Option<String>, String)> = conn
            .query_row(
                "SELECT name,created_by,created_at FROM im_conversations WHERE id=?",
                params![cid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .ok()
            .flatten();
        let conv = match existing {
            Some((name, created_by, created_at)) => Conversation {
                id: cid.to_string(),
                name,
                is_group: false,
                created_by,
                created_at,
                members: load_dm_members(conn, cid),
            },
            None => {
                let conv = Conversation {
                    id: cid.to_string(),
                    name: dm_conversation_name(a, b),
                    is_group: false,
                    created_by: Some(a.to_string()),
                    created_at: now_iso(),
                    members: vec![a.to_string(), b.to_string()],
                };
                // INSERT OR IGNORE：双端并发创建同一确定性 id 时只落一行
                let _ = conn.execute(
                    "INSERT OR IGNORE INTO im_conversations (id,name,is_group,created_by,created_at) VALUES (?,?,?,?,?)",
                    params![
                        conv.id,
                        conv.name,
                        conv.is_group as i64,
                        conv.created_by.as_deref(),
                        conv.created_at
                    ],
                );
                conv
            }
        };
        let now = now_iso();
        for uid in [a, b] {
            let _ = conn.execute(
                "INSERT OR IGNORE INTO im_dm_members (conversation_id,user_id,joined_at) VALUES (?,?,?)",
                params![cid, uid, now],
            );
        }
        conv
    }

    /// DM 定向 WS 推送（`im_message` 帧 + `send_to_n` 按 pubkey）：只有收件
    /// 列表里的订阅者收到——区别于 [`ImRouteHandler::broadcast_conversation`]
    /// 的全员广播（DM 绝不广播）。收件人离线则空投递（落库已保证回看）。
    fn push_dm_ws(&self, cid: &str, msg: &Message, recipients: &[&str]) {
        let Some(hub) = &self.ws_hub else {
            return;
        };
        let frame = WsMessage::ImMessage {
            conversation_id: cid.to_string(),
            message: serde_json::to_value(msg).unwrap_or(serde_json::Value::Null),
        };
        for user in recipients {
            hub.send_to_n(user, frame.clone());
        }
    }
}

// ----------------------------------------------------------------------------
// ImLobbyProbe —— 查询端探针：发 im_lobby_query / 收 im_lobby_reply / 30s 缓存
// ----------------------------------------------------------------------------

/// 探针缓存条目：最近一次应答标志 + 限频水位（消息全量只在阻塞查询的
/// oneshot 通道流转，缓存不重复存——combined 只消费 public 布尔）。
#[derive(Debug, Clone)]
struct ProbeEntry {
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
    shared: Arc<ImShared>,
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

/// 在线判定（纯函数）：`last_seen`（RFC3339）距 `now_secs`（unix 秒）< 60s 即在线。
///
/// 解析失败 / 缺失时间一律离线（宁可少算在线，不可误报）。
#[must_use]
pub fn is_online(last_seen: &str, now_secs: i64) -> bool {
    match chrono::DateTime::parse_from_rfc3339(last_seen) {
        Ok(t) => (now_secs - t.timestamp()).abs() < ONLINE_WINDOW_SECS,
        Err(_) => false,
    }
}

/// 构造"欢迎加入大厅"系统消息（纯函数，conversation_id 恒为 lobby）。
#[must_use]
pub fn build_welcome_message(user: &str) -> Message {
    Message {
        id: new_uuid(),
        conversation_id: LOBBY_ID.to_string(),
        sender_id: "system".to_string(),
        sender_name: Some("NexOS".to_string()),
        content: format!("欢迎 {user} 加入 NexOS 大厅"),
        msg_type: "system".to_string(),
        file_url: None,
        reply_to: None,
        created_at: now_iso(),
        read_by: Vec::new(),
        sender_kind: "human".to_string(),
        mentions: Vec::new(),
        attachment: None,
    }
}

fn default_sender_kind_human() -> String {
    "human".to_string()
}

/// 归一 sender_kind（纯函数）：`agent` / `system` 白名单放行，其余（缺失/
/// 垃圾值）一律 `human`——展示层自声明语义的兜底（见 Message 字段注释；
/// `system` 为 2026-08-23 身份冲突警告等本地系统消息保留）。
fn normalize_sender_kind(kind: Option<&str>) -> String {
    match kind.map(str::trim) {
        Some("agent") => "agent".to_string(),
        Some("system") => "system".to_string(),
        _ => "human".to_string(),
    }
}

// ----------------------------------------------------------------------------
// @mention 解析 + 内置助手 NexOS助手（2026-08-21 agent 批次）
// ----------------------------------------------------------------------------

/// 内置 agent 名字：@提及该名字触发内置助手（常量对外——前端高亮/外部
/// agent 避免撞名用）。
pub const NEXOS_ASSISTANT: &str = "NexOS助手";
/// 助手合成 sender_id（非链上身份——无私钥，仅服务端代发；归因恒可信）。
const ASSISTANT_SENDER_ID: &str = "agent:nexos-assistant";
/// 助手回复正文字符上限（"（AI 生成）"后缀不计入）。
const ASSISTANT_REPLY_MAX_CHARS: usize = 800;
/// 助手回复固定后缀（AI 生成标识，前端/审计用）。
const ASSISTANT_SUFFIX: &str = "（AI 生成）";
/// LLM 不可达/出错时的固定降级话术。
const ASSISTANT_FALLBACK_TEXT: &str = "抱歉，本地推理服务暂时不可用，请稍后再试。";
/// 默认防风暴去抖窗口：同会话该窗口内多条 @ 只响应最后一条。
const ASSISTANT_STORM_WINDOW: Duration = Duration::from_secs(3);
/// 默认推理端点（OpenAI 兼容 chat/completions；env `NEXOS_IM_AGENT_LLM_URL` 覆盖）。
const ASSISTANT_LLM_URL_DEFAULT: &str = "http://127.0.0.1:8000/v1/chat/completions";
/// 默认模型名（env `NEXOS_IM_AGENT_MODEL` 覆盖）。
const ASSISTANT_LLM_MODEL_DEFAULT: &str = "qwen3.5-9b";
/// 助手推理请求超时（与 llm.rs chat 通道同款 60s）。
const ASSISTANT_LLM_TIMEOUT: Duration = Duration::from_secs(60);
/// 防风暴代次表条目 TTL（顺手清理，防无界增长）。
const ASSISTANT_GEN_TTL: Duration = Duration::from_secs(3600);
/// @ 名字字符集：CJK 基本区（一-龥 = U+4E00..=U+9FA5）+ ASCII 字母数字 + `_`/`-`。
fn is_mention_char(c: char) -> bool {
    ('\u{4E00}'..='\u{9FA5}').contains(&c) || c.is_ascii_alphanumeric() || c == '_' || c == '-'
}
/// @ 名字长度上限（字符）。
const MENTION_MAX_CHARS: usize = 42;

/// 解析 content 中的 @ 提及（纯函数）：每个 `@` 后跟 1..=42 个合法名字字符的
/// 连续段即一次提及（超 42 字符截断到 42；`@` 后跟非法字符不算——如邮箱
/// 前缀后的 `@example.com` 会截出 `example`，属既定语义）。去重保序。
///
/// 中文/英文/多 @ 均可：`"你好 @NexOS助手 请看 @alice 的稿"` →
/// `["NexOS助手", "alice"]`。
#[must_use]
pub fn parse_mentions(content: &str) -> Vec<String> {
    let chars: Vec<char> = content.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '@' {
            i += 1;
            continue;
        }
        let mut name = String::new();
        let mut j = i + 1;
        while j < chars.len()
            && is_mention_char(chars[j])
            && name.chars().count() < MENTION_MAX_CHARS
        {
            name.push(chars[j]);
            j += 1;
        }
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
        i = if j > i + 1 { j } else { i + 1 };
    }
    out
}

/// 剥掉 content 中已解析的 `@名字` 片段（纯函数，助手 prompt 用——"除 @ 外文本"）。
/// 每个名字只剥首个匹配；结果 trim。全部剥完为空（用户只发了 @）时由调用方
/// 回退原文。
#[must_use]
pub fn strip_mentions(content: &str, names: &[String]) -> String {
    let mut out = content.to_string();
    for n in names {
        out = out.replacen(&format!("@{n}"), "", 1);
    }
    out.trim().to_string()
}

/// 按**字符**安全截断（UTF-8 边界安全）：超过 max 字符取前 max 个。
#[must_use]
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// 助手推理用共享 HTTP 客户端（进程级连接池复用，llm.rs 同款）。
static AGENT_HTTP: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .build()
        .expect("构建助手推理 reqwest Client 失败")
});

/// 调本地推理（OpenAI 兼容 `POST /v1/chat/completions`，llm.rs chat 通道同款）。
///
/// 失败（连接拒绝/HTTP 错误/响应缺字段）一律 `Err`——调用方降级到固定话术，
/// 绝不 panic、绝不阻塞发消息请求（本函数只在 spawn 的回复任务里调用）。
async fn assistant_chat_complete(url: &str, model: &str, prompt: &str) -> Result<String, String> {
    let payload = serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": "你是 NexOS IM 内置的「NexOS助手」，用简洁中文回答用户问题。"},
            {"role": "user", "content": prompt},
        ],
        "max_tokens": 4096,  // 思考模型（qwen3.5 reasoning）推理段耗 token，512 会 finish=length 致 content=null（演示agent实测 F3）
        "temperature": 0.7,
    });
    // vLLM 实例启用 --api-key 时（NEXOS_VLLM_API_KEY 透传），助手直连同样要带
    let mut req = AGENT_HTTP.post(url).timeout(ASSISTANT_LLM_TIMEOUT);
    if let Ok(k) = std::env::var("NEXOS_VLLM_API_KEY") {
        if !k.trim().is_empty() {
            req = req.bearer_auth(k);
        }
    }
    let resp = req
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("推理请求发送失败（本地 LLM 未运行？）: {e}"))?;
    let resp = resp
        .error_for_status()
        .map_err(|e| format!("推理请求失败（HTTP 错误）: {e}"))?;
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("解析推理响应失败: {e}"))?;
    v.get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(str::to_string)
        .ok_or_else(|| "推理响应缺少 choices[0].message.content".to_string())
}

/// 该会话的最新触发代次是否仍是 `gen`（防风暴提交闸门）。
fn assistant_is_latest(shared: &ImShared, cid: &str, gen: u64) -> bool {
    shared
        .assistant_gen
        .lock()
        .expect("assistant_gen poisoned")
        .get(cid)
        .is_some_and(|(g, _)| *g == gen)
}

impl ImRouteHandler {
    /// @NexOS助手 触发判定 + spawn 异步回复任务。
    ///
    /// 触发条件（缺一不可）：非 agent 消息（防自激——助手回复不触发新回复）
    /// 且 `mentions` 含 [`NEXOS_ASSISTANT`]。
    ///
    /// 防风暴（去抖）：先登记代次（同会话每触发 +1），任务先睡满
    /// [`ImRouteHandler::agent_storm_window`]（默认 3s）——睡眠期间更新的 @ 会
    /// 顶掉代次；醒来与提交前各核一次代次，被超越即静默放弃——**3s 窗口内
    /// 多条 @ 只有最后一条得到回复**。
    fn maybe_spawn_assistant(&self, trigger: &Message) {
        if trigger.sender_kind == "agent" {
            return; // 助手/外部 agent 消息不再触发（防风暴第二道闸）
        }
        if !trigger.mentions.iter().any(|m| m == NEXOS_ASSISTANT) {
            return;
        }
        let shared = self.shared.clone();
        let url_configured = self.agent_llm_url_configured();
        let model = self.agent_model();
        let window = self.agent_storm_window();
        let cid = trigger.conversation_id.clone();
        let reply_to = trigger.id.clone();
        // prompt = 除 @ 外文本；全剥空（用户只发 @）回退原文
        let prompt = {
            let stripped = strip_mentions(&trigger.content, &trigger.mentions);
            if stripped.is_empty() {
                trigger.content.trim().to_string()
            } else {
                stripped
            }
        };
        // 登记代次（顺手按 TTL 清理旧条目，防表无界增长）
        let gen = {
            let mut gens = shared.assistant_gen.lock().expect("assistant_gen poisoned");
            gens.retain(|_, (_, at)| at.elapsed() < ASSISTANT_GEN_TTL);
            let entry = gens.entry(cid.clone()).or_insert((0, Instant::now()));
            entry.0 += 1;
            entry.1 = Instant::now();
            entry.0
        };
        tokio::spawn(async move {
            // 睡满去抖窗口：让同窗口内的后续 @ 有机会顶掉本任务
            tokio::time::sleep(window).await;
            if !assistant_is_latest(&shared, &cid, gen) {
                return;
            }
            // env/测试未覆盖时动态探测活跃 LLM 端口（8123 优先）
            let url = match url_configured.clone() {
                Some(u) => u,
                None => Self::probe_live_llm_url().await,
            };
            let body = assistant_chat_complete(&url, &model, &prompt)
                .await
                .unwrap_or_else(|_| ASSISTANT_FALLBACK_TEXT.to_string());
            // LLM 往返期间可能又有新 @——提交前再核一次代次
            if !assistant_is_latest(&shared, &cid, gen) {
                return;
            }
            let mut content = truncate_chars(body.trim(), ASSISTANT_REPLY_MAX_CHARS);
            content.push_str(ASSISTANT_SUFFIX);
            let reply = Message {
                id: new_uuid(),
                conversation_id: cid.clone(),
                sender_id: ASSISTANT_SENDER_ID.to_string(),
                sender_name: Some(NEXOS_ASSISTANT.to_string()),
                content,
                msg_type: "text".to_string(),
                file_url: None,
                reply_to: Some(reply_to),
                created_at: now_iso(),
                read_by: Vec::new(),
                sender_kind: "agent".to_string(),
                mentions: Vec::new(),
                attachment: None,
            };
            {
                let conn = shared.db.lock().expect("db poisoned");
                let _ = insert_message(&conn, &reply);
            }
            if cid == LOBBY_ID {
                Self::broadcast_lobby(&shared.ws_hub, &reply);
            } else {
                Self::broadcast_conversation(&shared.ws_hub, &cid, &reply);
            }
            // 助手回复也是一条新消息——同样触发匹配的 webhook（参与的
            // agent 对 @ 的回答也能收到推送，无需轮询）
            shared.dispatch_webhooks(&reply);
        });
    }

    /// 发消息 body 的自报附件核对（纯服务端视角）：按 file_id 查 im_files——
    /// 不存在 → `Err(400)`；存在 → **落盘真值覆盖** filename/size_bytes
    /// （伪造自报无效），mime 取自报（可精化）回落存储值。
    fn verify_attachment(
        &self,
        req: Option<&AttachmentReq>,
    ) -> Result<Option<Attachment>, ApiResponse> {
        let Some(a) = req else {
            return Ok(None);
        };
        let record = {
            let conn = self.shared.db.lock().expect("db poisoned");
            find_file_record(&conn, &a.file_id).unwrap_or(None)
        };
        let Some(rec) = record else {
            return Err(error_response(
                400,
                &format!(
                    "attachment.file_id 不存在: {}（先 POST /api/v1/im/files）",
                    a.file_id
                ),
            ));
        };
        Ok(Some(Attachment {
            file_id: rec.file_id,
            filename: rec.filename,
            size_bytes: rec.size_bytes,
            mime: a.mime.clone().filter(|m| !m.trim().is_empty()).or(rec.mime),
        }))
    }
}

// ----------------------------------------------------------------------------
// IM 附件落盘（文档传输，2026-08-21；files.rs 同款阻塞 FS + spawn_blocking 惯例）
// ----------------------------------------------------------------------------

/// 附件单文件上限：64 MiB。
const IM_FILE_MAX_BYTES: usize = 64 * 1024 * 1024;
/// 净化后文件名长度上限（字符；uuid 前缀另计）。
const IM_FILENAME_MAX_CHARS: usize = 120;

/// 附件根目录（env `NEXOS_IM_FILES_ROOT` 覆盖）：`/tank/im-files`（可建）→
/// `/var/lib/os/im-files` → `./im-files`（与 default_db_path 同款回退链）。
fn im_files_root_default() -> String {
    for p in ["/tank/im-files", "/var/lib/os/im-files"] {
        let path = std::path::Path::new(p);
        if path
            .parent()
            .is_some_and(|d| d.exists() || std::fs::create_dir_all(d).is_ok())
        {
            return p.to_string();
        }
    }
    "./im-files".to_string()
}

/// env 读取辅助：trim 后非空才算配置。
fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// 读系统 admin token env（与 model_hub/media_gen 同款语义）：
/// `NEXOS_ADMIN_TOKEN` 优先，回落 `OS_ADMIN_TOKEN`；trim 后非空才算启用。
fn admin_token_from_env() -> Option<String> {
    std::env::var("NEXOS_ADMIN_TOKEN")
        .or_else(|_| std::env::var("OS_ADMIN_TOKEN"))
        .ok()
        .and_then(|t| {
            let t = t.trim().to_string();
            (!t.is_empty()).then_some(t)
        })
}

/// 净化上传文件名（纯函数）：先按白名单逐字符映射——ASCII 字母数字、CJK
/// （一-龥）、`.`、`-`、`_`、`(`、`)`、空格保留，**其余（含 `/` `\` 与控制
/// 字符——路径穿越/不可文名）一律 `_`**；再截到 [`IM_FILENAME_MAX_CHARS`]
/// 字符；全空回退 `file`。净化后是安全的单段名（uuid 前缀 + 该名落盘）。
#[must_use]
pub fn sanitize_im_filename(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| {
            let keep = ('\u{4E00}'..='\u{9FA5}').contains(&c)
                || c.is_ascii_alphanumeric()
                || matches!(c, '.' | '-' | '_' | '(' | ')' | ' ');
            if keep {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        return "file".to_string();
    }
    truncate_chars(&cleaned, IM_FILENAME_MAX_CHARS)
}

/// 附件直链 url（纯函数）：`/api/v1/im/files/<file_id>?token=<token>`
/// （相对路径，客户端自行拼 scheme://host:port）。
#[must_use]
pub fn im_file_url(file_id: &str, token: &str) -> String {
    if token.is_empty() {
        format!("/api/v1/im/files/{file_id}")
    } else {
        format!("/api/v1/im/files/{file_id}?token={token}")
    }
}

/// 附件落盘（阻塞调用，handler 经 spawn_blocking 调）：目录自动建 →
/// tmp+rename 原子写（files.rs store_upload 同款）。返回最终路径。
fn store_im_file(
    dir: &std::path::Path,
    stored_name: &str,
    bytes: &[u8],
) -> Result<std::path::PathBuf, (u16, String)> {
    if bytes.len() > IM_FILE_MAX_BYTES {
        return Err((413, "附件超限：单文件最大 64 MiB".to_string()));
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        return Err((500, format!("附件目录自动创建失败: {e}")));
    }
    let final_path = dir.join(stored_name);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(".imfile-{}-{nanos}.tmp", std::process::id()));
    if let Err(e) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err((500, format!("写入临时文件失败: {e}")));
    }
    match std::fs::rename(&tmp, &final_path) {
        Ok(()) => Ok(final_path),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err((500, format!("附件落盘失败: {e}")))
        }
    }
}

/// 读取附件并构造 base64 信封（阻塞调用；files.rs read_download 同款）：
/// 不存在 → 404；超 [`IM_FILE_MAX_BYTES`] → 413；IO 错误 → 500。
fn read_im_file(rec: &ImFileRecord, max_bytes: u64) -> Result<ImFileDownload, (u16, String)> {
    let meta = std::fs::metadata(&rec.path).map_err(|e| (404, format!("附件文件不存在: {e}")))?;
    if meta.len() > max_bytes {
        return Err((413, "附件超限：单文件最大 64 MiB".to_string()));
    }
    let bytes = std::fs::read(&rec.path).map_err(|e| (500, format!("读取附件失败: {e}")))?;
    Ok(ImFileDownload {
        file_id: rec.file_id.clone(),
        filename: rec.filename.clone(),
        size_bytes: bytes.len() as u64,
        mime_type: rec
            .mime
            .clone()
            .unwrap_or_else(|| guess_mime_im(&rec.filename)),
        content_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        encoding: "base64".to_string(),
    })
}

/// 按扩展名猜 MIME（极简映射，files.rs guess_mime 的 IM 精简版——文档传输
/// 场景优先覆盖 Office/PDF/图片）。
fn guess_mime_im(name: &str) -> String {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "json" => "application/json",
        "pdf" => "application/pdf",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "mp4" => "video/mp4",
        "mp3" => "audio/mpeg",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// `Content-Disposition: attachment` 头值（RFC 5987；files.rs 同款双 filename）。
fn content_disposition_im(name: &str) -> String {
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let ascii = if ascii.is_empty() { "download" } else { &ascii };
    let mut pct = String::with_capacity(name.len());
    for b in name.bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            );
        if keep {
            pct.push(b as char);
        } else {
            pct.push_str(&format!("%{b:02X}"));
        }
    }
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{pct}")
}

// ----------------------------------------------------------------------------
// 消息推送通知 webhook（2026-08-22，docs/IM_AGENTS_AND_FILES.md §7）
// —— 注册/管理端点见 handle()，派发在 ImShared::dispatch_webhooks
// ----------------------------------------------------------------------------

/// 订阅事件名：大厅新消息（conversation_id == lobby 的消息）。
pub const NOTIFY_EVENT_LOBBY: &str = "lobby";
/// 订阅事件名：会话新消息（大厅以外的全部会话/群组消息）。
pub const NOTIFY_EVENT_CONVERSATION: &str = "conversation";
/// webhook 注册/管理路径前缀（错误消息提示用）。
/// 投递时的自定义事件头：`lobby_message` / `conversation_message`。
const WEBHOOK_EVENT_HEADER: &str = "X-NexOS-Event";
/// 单次投递超时（超时计一次失败）。
const WEBHOOK_HTTP_TIMEOUT: Duration = Duration::from_secs(5);
/// 连败 ≥ 该值自动注销（status=disabled + last_error 记录原因）。
const WEBHOOK_MAX_CONSECUTIVE_FAILURES: u32 = 5;
/// webhook url 长度上限（字符；防把超大串塞进 DB/每次投递）。
const WEBHOOK_URL_MAX_CHARS: usize = 2048;
/// 注册表 status 取值：活跃（参与派发）。
const WEBHOOK_STATUS_ACTIVE: &str = "active";
/// 注册表 status 取值：连败自动注销（不参与派发；重新注册即恢复）。
const WEBHOOK_STATUS_DISABLED: &str = "disabled";

fn default_webhook_events_all() -> Vec<String> {
    vec![
        NOTIFY_EVENT_LOBBY.to_string(),
        NOTIFY_EVENT_CONVERSATION.to_string(),
    ]
}

fn default_webhook_active() -> String {
    WEBHOOK_STATUS_ACTIVE.to_string()
}

/// 校验 webhook url（纯函数）：`http://`/`https://` scheme + 非空主机段 +
/// 无空白字符 + ≤2048 字符（防把超大串/畸形串塞进 DB 和每次投递）。
#[must_use]
pub fn is_valid_webhook_url(url: &str) -> bool {
    let u = url.trim();
    let Some((scheme, rest)) = u.split_once("://") else {
        return false;
    };
    if scheme != "http" && scheme != "https" {
        return false;
    }
    !rest.is_empty()
        && !rest.chars().any(char::is_whitespace)
        && u.chars().count() <= WEBHOOK_URL_MAX_CHARS
}

/// 归一注册 events（纯函数）：白名单过滤 + 去重保序。
/// 返回 None = 传入里有非法值或全空（调用方回 400）；Some(空) 不会出现。
fn normalize_webhook_events(events: &[String]) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for e in events {
        let t = e.trim();
        if t != NOTIFY_EVENT_LOBBY && t != NOTIFY_EVENT_CONVERSATION {
            return None;
        }
        if !out.iter().any(|x| x == t) {
            out.push(t.to_string());
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(out)
}

/// 派发事件名（纯函数）：大厅消息 → `lobby_message`，其余 → `conversation_message`。
#[must_use]
pub fn webhook_event_name(conversation_id: &str) -> &'static str {
    if conversation_id == LOBBY_ID {
        "lobby_message"
    } else {
        "conversation_message"
    }
}

/// webhook 是否匹配该消息（纯函数，派发过滤 + 测试共用）：
/// - status 须 active（自动注销的不再派发）；
/// - 事件过滤：大厅消息须订阅 `lobby`，会话消息须订阅 `conversation`；
/// - conversation 事件可绑定单个会话（None=全部会话）；lobby 事件不与
///   conversation_id 绑定（大厅是单一公共频道，绑了也无意义）。
#[must_use]
pub fn webhook_matches(hook: &ImWebhook, msg: &Message) -> bool {
    if hook.status != WEBHOOK_STATUS_ACTIVE {
        return false;
    }
    let is_lobby = msg.conversation_id == LOBBY_ID;
    let event_hit = if is_lobby {
        hook.events.iter().any(|e| e == NOTIFY_EVENT_LOBBY)
    } else {
        hook.events.iter().any(|e| e == NOTIFY_EVENT_CONVERSATION)
    };
    if !event_hit {
        return false;
    }
    if is_lobby {
        return true;
    }
    hook.conversation_id
        .as_deref()
        .map_or(true, |cid| cid == msg.conversation_id)
}

impl ImShared {
    /// 消息成功写入后：对所有匹配的注册 webhook spawn 异步 POST。
    ///
    /// **完全不阻塞消息路径**——同步段只做一次短锁查表，逐 webhook
    /// `tokio::spawn`（单 hook 失败/超时互不影响，也不影响 HTTP 响应）。
    /// body = 完整 Message JSON（不含任何 token），Header
    /// `X-NexOS-Event: lobby_message|conversation_message`，超时 5s；
    /// 成功清零连败并记 last_fired_at，失败连败 +1，连败 ≥5 自动注销。
    fn dispatch_webhooks(self: &Arc<Self>, msg: &Message) {
        let hooks: Vec<ImWebhook> = {
            let conn = self.db.lock().expect("db poisoned");
            load_all_webhooks(&conn)
                .unwrap_or_default()
                .into_iter()
                .filter(|hook| webhook_matches(hook, msg))
                .collect()
        };
        if hooks.is_empty() {
            return;
        }
        let event = webhook_event_name(&msg.conversation_id);
        let payload = serde_json::to_value(msg).unwrap_or(serde_json::Value::Null);
        for hook in hooks {
            let shared = self.clone();
            let payload = payload.clone();
            tokio::spawn(async move {
                let outcome: Result<(), String> = async {
                    let resp = AGENT_HTTP
                        .post(&hook.url)
                        .timeout(WEBHOOK_HTTP_TIMEOUT)
                        .header(WEBHOOK_EVENT_HEADER, event)
                        .json(&payload)
                        .send()
                        .await
                        .map_err(|e| format!("投递失败（超时/不可达）: {e}"))?;
                    if resp.status().is_success() {
                        Ok(())
                    } else {
                        Err(format!("接收端返回 HTTP {}", resp.status().as_u16()))
                    }
                }
                .await;
                let conn = shared.db.lock().expect("db poisoned");
                match outcome {
                    Ok(()) => {
                        let _ = webhook_record_success(&conn, &hook.id);
                    }
                    Err(err) => {
                        let _ = webhook_record_failure(
                            &conn,
                            &hook.id,
                            &err,
                            WEBHOOK_MAX_CONSECUTIVE_FAILURES,
                        );
                    }
                }
            });
        }
    }
}

/// 统一 401：IM 用户面端点缺/无效 Bearer token（客户端应重走挑战-签名）。
fn auth_required() -> ApiResponse {
    error_response(
        401,
        "需要 Authorization: Bearer <IM token>（先 POST /api/v1/im/auth/challenge + /auth/verify）",
    )
}

/// 联邦接收开关状态说明文案（GET/POST /api/v1/im/federation 响应 `note`）。
fn fed_toggle_note(enabled: bool) -> &'static str {
    if enabled {
        "联邦接收已开启：接收其他节点发到联邦大厅的消息（本开关只管接收，发送不受影响）"
    } else {
        "联邦接收已暂停：不再接收其他节点发到联邦大厅的消息（本地消息与联邦发送不受影响）"
    }
}

fn default_msg_type_text() -> String {
    "text".to_string()
}
fn default_member_role() -> String {
    "member".to_string()
}
fn default_kind_group() -> String {
    "group".to_string()
}
fn default_peer_online() -> String {
    "online".to_string()
}

/// 构造一条 [`RouteSpec`]（component 固定 `im`）。
///
/// requires_auth 语义：IM 用户面端点恒 false——它们的认证是 IM token
/// （handler 内验，不走系统 Principal 中间件，见设计 §3「与系统级
/// admin token 正交」）；仅管理端点（POST /peers）为 true（系统级认证）。
fn spec(
    method: HttpMethod,
    path: &str,
    requires_auth: bool,
    required_roles: Vec<String>,
) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: COMPONENT.to_string(),
        requires_auth,
        required_roles,
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

fn to_value<T: Serialize>(v: &T) -> Result<serde_json::Value, ApiGatewayError> {
    serde_json::to_value(v).map_err(|e| ApiGatewayError::Internal(format!("响应序列化失败: {e}")))
}

/// 从请求路径中剥离 `?query` 后的纯 path 段（前后空段去除）。
fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

/// 从 query string 解析字符串参数。
fn parse_query_str(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut it = pair.splitn(2, '=');
        if it.next() == Some(key) {
            return it.next().map(|s| s.to_string());
        }
    }
    None
}

/// 搜索词 URL 解码（`%XX` + `+`→空格，media.rs `url_decode` 同款表单语义）：
/// 前端 `URLSearchParams`/`encodeURIComponent` 产物均可正确还原（空格走
/// `%20` 或 `+`，字面加号走 `%2B`）。`parse_query_str` 返回原始编码串
/// （补拉/未读等 ASCII id 端点无感），搜索词是自由文本（CJK / `%` / 空格
/// 都会被编码），须解码后再匹配。
fn url_decode_query(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            _ => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 当前本地时间（RFC3339 / ISO8601 带时区）。
fn now_iso() -> String {
    chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

/// 生成一个新的 UUID v4 字符串（os_core::Uuid 与 os-im 同源）。
fn new_uuid() -> String {
    os_core::Uuid::new_v4().to_string()
}

/// 生成一个短 UUID（取前 8 字符）——peer id 默认填充用。
fn short_uuid() -> String {
    new_uuid().chars().take(8).collect()
}

// ----------------------------------------------------------------------------
// SQLite 持久化层
// ----------------------------------------------------------------------------

/// 默认 DB 路径：优先 `/tank/os-data/im.db`，再 `/var/lib/os/im.db`，最后 `./im.db`。
fn default_db_path() -> String {
    for p in &["/tank/os-data/im.db", "/var/lib/os/im.db"] {
        if std::path::Path::new(p)
            .parent()
            .is_some_and(|d| d.exists() || std::fs::create_dir_all(d).is_ok())
        {
            return (*p).to_string();
        }
    }
    "./im.db".to_string()
}

/// 打开 SQLite 文件，建表，首次空表时 seed demo 数据。
fn open_db(path: &str) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    create_schema(&conn)?;
    seed_if_empty(&conn)?;
    Ok(conn)
}

/// 建表（IF NOT EXISTS）+ 消息按时间排序索引 + 存量库迁移（ALTER 补列）。
fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS im_conversations (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            is_group INTEGER DEFAULT 0,
            created_by TEXT,
            created_at TEXT
        );
        CREATE TABLE IF NOT EXISTS im_messages (
            id TEXT PRIMARY KEY,
            conversation_id TEXT NOT NULL,
            sender_id TEXT NOT NULL,
            sender_name TEXT,
            content TEXT NOT NULL,
            msg_type TEXT DEFAULT 'text',
            file_url TEXT,
            reply_to TEXT,
            created_at TEXT,
            read_by TEXT DEFAULT '[]',
            sender_kind TEXT DEFAULT 'human',
            mentions TEXT DEFAULT '[]',
            attachment TEXT
        );
        CREATE TABLE IF NOT EXISTS im_groups (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            owner TEXT,
            created_at TEXT
        );
        CREATE TABLE IF NOT EXISTS im_group_members (
            group_id TEXT NOT NULL,
            user_id TEXT NOT NULL,
            role TEXT DEFAULT 'member',
            joined_at TEXT,
            PRIMARY KEY (group_id, user_id)
        );
        CREATE TABLE IF NOT EXISTS im_dm_members (
            conversation_id TEXT NOT NULL,
            user_id TEXT NOT NULL,
            joined_at TEXT,
            PRIMARY KEY (conversation_id, user_id)
        );
        CREATE TABLE IF NOT EXISTS im_dm_peers (
            pubkey TEXT PRIMARY KEY,
            node TEXT,
            display_name TEXT,
            last_seen TEXT
        );
        CREATE TABLE IF NOT EXISTS im_peers (
            id TEXT PRIMARY KEY,
            name TEXT,
            endpoint TEXT,
            status TEXT DEFAULT 'offline',
            last_seen TEXT
        );
        CREATE TABLE IF NOT EXISTS im_lobby (
            id TEXT PRIMARY KEY,
            name TEXT DEFAULT '大厅',
            created_at TEXT
        );
        CREATE TABLE IF NOT EXISTS im_lobby_members (
            user_id TEXT PRIMARY KEY,
            display_name TEXT,
            last_seen TEXT,
            joined_at TEXT
        );
        CREATE TABLE IF NOT EXISTS im_files (
            file_id TEXT PRIMARY KEY,
            filename TEXT NOT NULL,
            size_bytes INTEGER NOT NULL,
            mime TEXT,
            uploader TEXT,
            path TEXT NOT NULL,
            created_at TEXT
        );
        CREATE TABLE IF NOT EXISTS im_webhooks (
            id TEXT PRIMARY KEY,
            url TEXT NOT NULL,
            owner_pubkey TEXT NOT NULL,
            events TEXT DEFAULT '[\"lobby\",\"conversation\"]',
            conversation_id TEXT,
            status TEXT DEFAULT 'active',
            fail_count INTEGER DEFAULT 0,
            last_fired_at TEXT,
            last_error TEXT,
            created_at TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_im_messages_conv ON im_messages(conversation_id, created_at);
        ",
    )?;
    // 迁移：2026-08-21 之前的 im_messages 表缺 sender_kind/mentions/attachment
    // 三列（CREATE IF NOT EXISTS 不会给已存在的表补列）。列已存在时 ALTER 报
    // "duplicate column" —— 忽略即可（幂等，forwarding.rs 同款惯例）。
    for ddl in [
        "ALTER TABLE im_messages ADD COLUMN sender_kind TEXT DEFAULT 'human'",
        "ALTER TABLE im_messages ADD COLUMN mentions TEXT DEFAULT '[]'",
        "ALTER TABLE im_messages ADD COLUMN attachment TEXT",
    ] {
        let _ = conn.execute(ddl, []);
    }
    // 大厅表首次为空则创建大厅 + 欢迎系统消息（幂等：已有大厅则跳过）
    seed_lobby_if_empty(conn)?;
    Ok(())
}

/// 大厅 seed：im_lobby 为空时插入固定大厅行 + 1 条系统欢迎消息。
///
/// 欢迎消息落在 im_messages（conversation_id='lobby'），复用现有消息读写路径。
fn seed_lobby_if_empty(conn: &Connection) -> rusqlite::Result<()> {
    let lobby_count: i64 = conn.query_row("SELECT COUNT(*) FROM im_lobby", [], |r| r.get(0))?;
    if lobby_count == 0 {
        conn.execute(
            "INSERT OR REPLACE INTO im_lobby (id,name,created_at) VALUES (?,?,?)",
            params![LOBBY_ID, "大厅", now_iso()],
        )?;
        let welcome = Message {
            id: "msg-lobby-seed".to_string(),
            conversation_id: LOBBY_ID.to_string(),
            sender_id: "system".to_string(),
            sender_name: Some("NexOS".to_string()),
            content: "欢迎来到 NexOS 大厅 — 连接每一个超级个体".to_string(),
            msg_type: "system".to_string(),
            file_url: None,
            reply_to: None,
            created_at: now_iso(),
            read_by: Vec::new(),
            sender_kind: "human".to_string(),
            mentions: Vec::new(),
            attachment: None,
        };
        insert_message(conn, &welcome)?;
    }
    Ok(())
}

/// 首次空表时 seed demo 数据（2 对话 + 5 消息 + 1 群组）。已存在数据则跳过。
///
/// 注意：大厅欢迎消息在 [`create_schema`] 里先落库（也写 im_messages），
/// 故这里不能以 im_messages 计数判空——只看对话/群组是否为空。
fn seed_if_empty(conn: &Connection) -> rusqlite::Result<()> {
    let conv_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM im_conversations", [], |r| r.get(0))?;
    let group_count: i64 = conn.query_row("SELECT COUNT(*) FROM im_groups", [], |r| r.get(0))?;
    if conv_count == 0 && group_count == 0 {
        seed_demo(conn)?;
    }
    Ok(())
}

/// seed 具体数据：2 对话 + 1 群组（3 成员）+ 5 条消息。
fn seed_demo(conn: &Connection) -> rusqlite::Result<()> {
    let now = now_iso();
    // 2 对话
    let c1 = Conversation {
        id: "conv-general".into(),
        name: "通用聊天".into(),
        is_group: false,
        created_by: Some("alice".into()),
        created_at: now.clone(),
        members: Vec::new(),
    };
    let c2 = Conversation {
        id: "conv-dev".into(),
        name: "开发讨论".into(),
        is_group: false,
        created_by: Some("bob".into()),
        created_at: now.clone(),
        members: Vec::new(),
    };
    insert_conversation(conn, &c1)?;
    insert_conversation(conn, &c2)?;
    // 1 群组（owner alice + 成员 bob/carol）
    let g = Group {
        id: "group-dev-team".into(),
        name: "Dev Team".into(),
        owner: Some("alice".into()),
        kind: "group".into(),
        members: vec!["alice".into(), "bob".into(), "carol".into()],
        last_activity: None,
        created_at: now.clone(),
    };
    insert_group(conn, &g)?;
    for (uid, role) in [("alice", "owner"), ("bob", "member"), ("carol", "member")] {
        insert_group_member(conn, &g.id, uid, role, &now)?;
    }
    // 5 条消息（conv-general×2，conv-dev×1，group-dev-team×2）
    let msgs = [
        Message {
            id: "msg-1".into(),
            conversation_id: "conv-general".into(),
            sender_id: "alice".into(),
            sender_name: Some("Alice".into()),
            content: "大家好！".into(),
            msg_type: "text".into(),
            file_url: None,
            reply_to: None,
            created_at: "2026-01-01T09:00:00+08:00".into(),
            read_by: vec!["alice".into()],
            sender_kind: "human".into(),
            mentions: Vec::new(),
            attachment: None,
        },
        Message {
            id: "msg-2".into(),
            conversation_id: "conv-general".into(),
            sender_id: "bob".into(),
            sender_name: Some("Bob".into()),
            content: "hi Alice".into(),
            msg_type: "text".into(),
            file_url: None,
            reply_to: Some("msg-1".into()),
            created_at: "2026-01-01T09:01:00+08:00".into(),
            read_by: vec!["bob".into()],
            sender_kind: "human".into(),
            mentions: Vec::new(),
            attachment: None,
        },
        Message {
            id: "msg-3".into(),
            conversation_id: "conv-dev".into(),
            sender_id: "carol".into(),
            sender_name: Some("Carol".into()),
            content: "看看这份设计文档".into(),
            msg_type: "file".into(),
            file_url: Some("/tank/docs/design.pdf".into()),
            reply_to: None,
            created_at: "2026-01-01T10:00:00+08:00".into(),
            read_by: vec!["carol".into()],
            sender_kind: "human".into(),
            mentions: Vec::new(),
            attachment: None,
        },
        Message {
            id: "msg-4".into(),
            conversation_id: "group-dev-team".into(),
            sender_id: "alice".into(),
            sender_name: Some("Alice".into()),
            content: "今晚发版".into(),
            msg_type: "text".into(),
            file_url: None,
            reply_to: None,
            created_at: "2026-01-01T11:00:00+08:00".into(),
            read_by: vec!["alice".into()],
            sender_kind: "human".into(),
            mentions: Vec::new(),
            attachment: None,
        },
        Message {
            id: "msg-5".into(),
            conversation_id: "group-dev-team".into(),
            sender_id: "system".into(),
            sender_name: Some("System".into()),
            content: "Carol 加入了群组".into(),
            msg_type: "system".into(),
            file_url: None,
            reply_to: None,
            created_at: "2026-01-01T11:05:00+08:00".into(),
            read_by: Vec::new(),
            sender_kind: "human".into(),
            mentions: Vec::new(),
            attachment: None,
        },
    ];
    for m in &msgs {
        insert_message(conn, m)?;
    }
    Ok(())
}

// ---- 表行数统计 ----

fn count_rows(conn: &Connection, table: &str) -> usize {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    conn.query_row(&sql, [], |r| r.get::<_, i64>(0))
        .unwrap_or(0) as usize
}

// ---- conversations CRUD ----

fn insert_conversation(conn: &Connection, c: &Conversation) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO im_conversations (id,name,is_group,created_by,created_at) VALUES (?,?,?,?,?)",
        params![c.id, c.name, c.is_group as i64, c.created_by.as_deref(), c.created_at],
    )?;
    Ok(())
}

fn load_all_conversations(conn: &Connection) -> rusqlite::Result<Vec<Conversation>> {
    let mut stmt = conn.prepare(
        "SELECT id,name,is_group,created_by,created_at FROM im_conversations ORDER BY created_at",
    )?;
    let iter = stmt.query_map([], conversation_from_row)?;
    let mut out = Vec::new();
    for c in iter {
        let mut conv = c?;
        // DM 会话附带成员（双方 pubkey，前端据此识别「对方」并路由私信）
        if is_dm_conversation(&conv.id) {
            conv.members = load_dm_members(conn, &conv.id);
        }
        out.push(conv);
    }
    Ok(out)
}

fn conversation_from_row(row: &rusqlite::Row) -> rusqlite::Result<Conversation> {
    Ok(Conversation {
        id: row.get(0)?,
        name: row.get(1)?,
        is_group: row.get::<_, i64>(2)? != 0,
        created_by: row.get(3)?,
        created_at: row.get(4)?,
        members: Vec::new(), // DM 成员由 load_all_conversations 按需回填
    })
}

// ---- messages CRUD ----

/// im_messages 查询列（与 [`message_from_row`] 的索引一一对应；2026-08-21
/// 起含 sender_kind/mentions/attachment 三列——存量库缺列由 create_schema
/// 的幂等 ALTER 补齐，故 SELECT 恒可带全列）。
const MSG_COLS: &str =
    "id,conversation_id,sender_id,sender_name,content,msg_type,file_url,reply_to,created_at,read_by,sender_kind,mentions,attachment";

fn insert_message(conn: &Connection, m: &Message) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO im_messages
         (id,conversation_id,sender_id,sender_name,content,msg_type,file_url,reply_to,created_at,read_by,sender_kind,mentions,attachment)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
        params![
            m.id,
            m.conversation_id,
            m.sender_id,
            m.sender_name.as_deref(),
            m.content,
            m.msg_type,
            m.file_url.as_deref(),
            m.reply_to.as_deref(),
            m.created_at,
            serde_json::to_string(&m.read_by).unwrap_or_else(|_| "[]".into()),
            normalize_sender_kind(Some(&m.sender_kind)),
            serde_json::to_string(&m.mentions).unwrap_or_else(|_| "[]".into()),
            m.attachment
                .as_ref()
                .map(|a| serde_json::to_string(a).unwrap_or_default()),
        ],
    )?;
    Ok(())
}

fn load_messages_by_conversation(conn: &Connection, cid: &str) -> rusqlite::Result<Vec<Message>> {
    let sql =
        format!("SELECT {MSG_COLS} FROM im_messages WHERE conversation_id=? ORDER BY created_at");
    let mut stmt = conn.prepare(&sql)?;
    let iter = stmt.query_map(params![cid], message_from_row)?;
    let mut out = Vec::new();
    for m in iter {
        out.push(m?);
    }
    Ok(out)
}

fn load_all_messages(conn: &Connection) -> rusqlite::Result<Vec<Message>> {
    let sql = format!("SELECT {MSG_COLS} FROM im_messages ORDER BY created_at");
    let mut stmt = conn.prepare(&sql)?;
    let iter = stmt.query_map([], message_from_row)?;
    let mut out = Vec::new();
    for m in iter {
        out.push(m?);
    }
    Ok(out)
}

fn find_message(conn: &Connection, id: &str) -> rusqlite::Result<Option<Message>> {
    let sql = format!("SELECT {MSG_COLS} FROM im_messages WHERE id=?");
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_row(params![id], message_from_row).optional()
}

fn update_message_read_by(conn: &Connection, m: &Message) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE im_messages SET read_by=? WHERE id=?",
        params![
            serde_json::to_string(&m.read_by).unwrap_or_else(|_| "[]".into()),
            m.id
        ],
    )?;
    Ok(())
}

fn message_from_row(row: &rusqlite::Row) -> rusqlite::Result<Message> {
    let read_by_json: String = row.get(9)?;
    let read_by: Vec<String> = serde_json::from_str(&read_by_json).unwrap_or_default();
    let mentions_json: String = row
        .get::<_, Option<String>>(11)?
        .unwrap_or_else(|| "[]".into());
    let mentions: Vec<String> = serde_json::from_str(&mentions_json).unwrap_or_default();
    let attachment = row
        .get::<_, Option<String>>(12)?
        .and_then(|s| serde_json::from_str(&s).ok());
    Ok(Message {
        id: row.get(0)?,
        conversation_id: row.get(1)?,
        sender_id: row.get(2)?,
        sender_name: row.get(3)?,
        content: row.get(4)?,
        msg_type: row
            .get::<_, Option<String>>(5)?
            .unwrap_or_else(|| "text".into()),
        file_url: row.get(6)?,
        reply_to: row.get(7)?,
        created_at: row.get::<_, Option<String>>(8)?.unwrap_or_default(),
        read_by,
        sender_kind: row
            .get::<_, Option<String>>(10)?
            .unwrap_or_else(|| "human".into()),
        mentions,
        attachment,
    })
}

/// 统计某对话中某用户未读消息数（read_by 不含该用户）。
fn count_unread(conn: &Connection, cid: &str, user: &str) -> usize {
    let all = load_messages_by_conversation(conn, cid).unwrap_or_default();
    all.iter()
        .filter(|m| !m.read_by.contains(&user.to_string()))
        .count()
}

/// LIKE 通配符转义（`\` / `%` / `_` 前插 `\`，配合 SQL 的 `ESCAPE '\'`）：
/// 用户输入的 `%`/`_` 按**字面字符**匹配，不再当通配符（搜 "100%" 不会命中
/// "100" 开头的一切）。返回值已含两侧 `%` 包裹。
fn like_pattern_literal(q: &str) -> String {
    let mut escaped = String::with_capacity(q.len() + 2);
    for c in q.chars() {
        if c == '%' || c == '_' || c == '\\' {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    format!("%{escaped}%")
}

/// 搜索某会话消息（content LIKE %q%，通配符字面转义），created_at 倒序
/// （最新在前），至多 limit 条。
fn search_messages(
    conn: &Connection,
    cid: &str,
    q: &str,
    limit: i64,
) -> rusqlite::Result<Vec<Message>> {
    let like = like_pattern_literal(q);
    let sql = format!(
        "SELECT {MSG_COLS} FROM im_messages
         WHERE conversation_id=?1 AND content LIKE ?2 ESCAPE '\\'
         ORDER BY created_at DESC
         LIMIT ?3"
    );
    let mut stmt = conn.prepare(&sql)?;
    let iter = stmt.query_map(params![cid, like, limit], message_from_row)?;
    let mut out = Vec::new();
    for m in iter {
        out.push(m?);
    }
    Ok(out)
}

// ---- groups CRUD ----

fn insert_group(conn: &Connection, g: &Group) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO im_groups (id,name,owner,created_at) VALUES (?,?,?,?)",
        params![g.id, g.name, g.owner.as_deref(), g.created_at],
    )?;
    Ok(())
}

fn find_group(conn: &Connection, id: &str) -> rusqlite::Result<Option<Group>> {
    let g_row: Option<(String, String, Option<String>, String)> = conn
        .query_row(
            "SELECT id,name,owner,created_at FROM im_groups WHERE id=?",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((id, name, owner, created_at)) = g_row else {
        return Ok(None);
    };
    let members = load_group_members(conn, &id);
    let last_activity = last_message_time(conn, &id);
    Ok(Some(Group {
        id,
        name,
        owner,
        kind: "group".to_string(),
        members,
        last_activity,
        created_at,
    }))
}

fn load_all_groups(conn: &Connection) -> rusqlite::Result<Vec<Group>> {
    // 先收集全部行（避免在迭代中嵌套 prepare/查询同一连接引发借用冲突）。
    let rows: Vec<(String, String, Option<String>, String)> = {
        let mut stmt =
            conn.prepare("SELECT id,name,owner,created_at FROM im_groups ORDER BY created_at")?;
        let iter = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in iter {
            out.push(r?);
        }
        out
    };
    let mut out = Vec::new();
    for (id, name, owner, created_at) in rows {
        let members = load_group_members(conn, &id);
        let last_activity = last_message_time(conn, &id);
        out.push(Group {
            id,
            name,
            owner,
            kind: "group".to_string(),
            members,
            last_activity,
            created_at,
        });
    }
    Ok(out)
}

/// 某 conversation/group 的最后一条消息时间（None=无消息）。
fn last_message_time(conn: &Connection, cid: &str) -> Option<String> {
    conn.query_row(
        "SELECT created_at FROM im_messages WHERE conversation_id=? ORDER BY created_at DESC LIMIT 1",
        params![cid],
        |r| r.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
}

// ---- group_members CRUD ----

fn insert_group_member(
    conn: &Connection,
    gid: &str,
    uid: &str,
    role: &str,
    joined_at: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO im_group_members (group_id,user_id,role,joined_at) VALUES (?,?,?,?)",
        params![gid, uid, role, joined_at],
    )?;
    Ok(())
}

fn remove_group_member(conn: &Connection, gid: &str, uid: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM im_group_members WHERE group_id=? AND user_id=?",
        params![gid, uid],
    )
}

fn load_group_members(conn: &Connection, gid: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT user_id FROM im_group_members WHERE group_id=? ORDER BY joined_at")
        .ok();
    let Some(ref mut s) = stmt else {
        return Vec::new();
    };
    let rows = s.query_map(params![gid], |r| r.get::<_, String>(0));
    let Ok(iter) = rows else {
        return Vec::new();
    };
    let mut out = Vec::new();
    out.extend(iter.filter_map(Result::ok));
    out
}

// ---- peers CRUD ----

fn insert_peer(conn: &Connection, p: &Peer) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO im_peers (id,name,endpoint,status,last_seen) VALUES (?,?,?,?,?)",
        params![
            p.id,
            p.name.as_deref(),
            p.endpoint,
            p.status,
            p.last_seen.as_deref(),
        ],
    )?;
    Ok(())
}

fn load_all_peers(conn: &Connection) -> rusqlite::Result<Vec<Peer>> {
    let mut stmt =
        conn.prepare("SELECT id,name,endpoint,status,last_seen FROM im_peers ORDER BY id")?;
    let iter = stmt.query_map([], peer_from_row)?;
    let mut out = Vec::new();
    for p in iter {
        out.push(p?);
    }
    Ok(out)
}

fn peer_from_row(row: &rusqlite::Row) -> rusqlite::Result<Peer> {
    Ok(Peer {
        id: row.get(0)?,
        name: row.get(1)?,
        endpoint: row.get(2)?,
        status: row
            .get::<_, Option<String>>(3)?
            .unwrap_or_else(|| "offline".into()),
        last_seen: row.get(4)?,
    })
}

// ---- im_files CRUD（文档传输，2026-08-21）----

fn insert_file_record(conn: &Connection, r: &ImFileRecord) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO im_files (file_id,filename,size_bytes,mime,uploader,path,created_at) VALUES (?,?,?,?,?,?,?)",
        params![
            r.file_id,
            r.filename,
            r.size_bytes as i64,
            r.mime.as_deref(),
            r.uploader.as_deref(),
            r.path,
            r.created_at
        ],
    )?;
    Ok(())
}

fn find_file_record(conn: &Connection, file_id: &str) -> rusqlite::Result<Option<ImFileRecord>> {
    conn.query_row(
        "SELECT file_id,filename,size_bytes,mime,uploader,path,created_at FROM im_files WHERE file_id=?",
        params![file_id],
        |row| {
            Ok(ImFileRecord {
                file_id: row.get(0)?,
                filename: row.get(1)?,
                size_bytes: row.get::<_, i64>(2)? as u64,
                mime: row.get(3)?,
                uploader: row.get(4)?,
                path: row.get(5)?,
                created_at: row
                    .get::<_, Option<String>>(6)?
                    .unwrap_or_default(),
            })
        },
    )
    .optional()
}

// ---- im_webhooks CRUD（消息推送通知，2026-08-22）----

/// im_webhooks 查询列（与 [`webhook_from_row`] 的索引一一对应）。
const WEBHOOK_COLS: &str =
    "id,url,owner_pubkey,events,conversation_id,status,fail_count,last_fired_at,last_error,created_at";

fn insert_webhook(conn: &Connection, w: &ImWebhook) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO im_webhooks
         (id,url,owner_pubkey,events,conversation_id,status,fail_count,last_fired_at,last_error,created_at)
         VALUES (?,?,?,?,?,?,?,?,?,?)",
        params![
            w.id,
            w.url,
            w.owner_pubkey,
            serde_json::to_string(&w.events).unwrap_or_else(|_| "[]".into()),
            w.conversation_id.as_deref(),
            w.status,
            w.fail_count as i64,
            w.last_fired_at.as_deref(),
            w.last_error.as_deref(),
            w.created_at
        ],
    )?;
    Ok(())
}

fn load_all_webhooks(conn: &Connection) -> rusqlite::Result<Vec<ImWebhook>> {
    let sql = format!("SELECT {WEBHOOK_COLS} FROM im_webhooks ORDER BY created_at");
    let mut stmt = conn.prepare(&sql)?;
    let iter = stmt.query_map([], webhook_from_row)?;
    let mut out = Vec::new();
    for w in iter {
        out.push(w?);
    }
    Ok(out)
}

/// 某 owner 的全部 webhook（list 端点 owner 过滤）。
fn load_webhooks_by_owner(conn: &Connection, owner: &str) -> rusqlite::Result<Vec<ImWebhook>> {
    let sql =
        format!("SELECT {WEBHOOK_COLS} FROM im_webhooks WHERE owner_pubkey=? ORDER BY created_at");
    let mut stmt = conn.prepare(&sql)?;
    let iter = stmt.query_map(params![owner], webhook_from_row)?;
    let mut out = Vec::new();
    for w in iter {
        out.push(w?);
    }
    Ok(out)
}

fn find_webhook(conn: &Connection, id: &str) -> rusqlite::Result<Option<ImWebhook>> {
    let sql = format!("SELECT {WEBHOOK_COLS} FROM im_webhooks WHERE id=?");
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_row(params![id], webhook_from_row).optional()
}

fn delete_webhook(conn: &Connection, id: &str) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM im_webhooks WHERE id=?", params![id])
}

/// 投递成功：连败清零 + 记 last_fired_at + 清 last_error。
fn webhook_record_success(conn: &Connection, id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE im_webhooks SET fail_count=0, last_fired_at=?, last_error=NULL WHERE id=?",
        params![now_iso(), id],
    )?;
    Ok(())
}

/// 投递失败：连败 +1 + 记 last_error；连败 ≥ max_fails 自动注销
/// （status=disabled，last_error 换成注销原因——注册表保留行供 owner 审计，
/// 重新注册同 url 即恢复）。
fn webhook_record_failure(
    conn: &Connection,
    id: &str,
    err: &str,
    max_fails: u32,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE im_webhooks SET fail_count=fail_count+1, last_error=?1, last_fired_at=?2 WHERE id=?3",
        params![err, now_iso(), id],
    )?;
    let fails: Option<i64> = conn
        .query_row(
            "SELECT fail_count FROM im_webhooks WHERE id=?",
            params![id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    if fails.is_some_and(|f| f >= max_fails as i64) {
        conn.execute(
            "UPDATE im_webhooks SET status=?1, last_error=?2 WHERE id=?3",
            params![
                WEBHOOK_STATUS_DISABLED,
                format!("连败 {max_fails} 次自动注销（最近错误: {err}）"),
                id
            ],
        )?;
    }
    Ok(())
}

fn webhook_from_row(row: &rusqlite::Row) -> rusqlite::Result<ImWebhook> {
    let events_json: String = row
        .get::<_, Option<String>>(3)?
        .unwrap_or_else(|| "[]".into());
    let events: Vec<String> = serde_json::from_str(&events_json).unwrap_or_default();
    Ok(ImWebhook {
        id: row.get(0)?,
        url: row.get(1)?,
        owner_pubkey: row.get(2)?,
        events,
        conversation_id: row.get(4)?,
        status: row
            .get::<_, Option<String>>(5)?
            .unwrap_or_else(|| WEBHOOK_STATUS_ACTIVE.into()),
        fail_count: row.get::<_, i64>(6)?.max(0) as u32,
        last_fired_at: row.get(7)?,
        last_error: row.get(8)?,
        created_at: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
    })
}

// ---- lobby（大厅公共频道）----

/// upsert 大厅成员：新用户插入（joined_at/last_seen=now）返回 true；老用户仅刷新
/// last_seen/display_name（心跳）返回 false。全员不可退出大厅——无删除路径。
fn upsert_lobby_member(conn: &Connection, user_id: &str, display_name: &str) -> bool {
    let now = now_iso();
    let existing: bool = conn
        .query_row(
            "SELECT 1 FROM im_lobby_members WHERE user_id=?",
            params![user_id],
            |_| Ok(true),
        )
        .optional()
        .unwrap_or(Some(false))
        .unwrap_or(false);
    let res = if existing {
        conn.execute(
            "UPDATE im_lobby_members SET last_seen=?, display_name=? WHERE user_id=?",
            params![now, display_name, user_id],
        )
    } else {
        conn.execute(
            "INSERT INTO im_lobby_members (user_id,display_name,last_seen,joined_at) VALUES (?,?,?,?)",
            params![user_id, display_name, now, now],
        )
    };
    if res.is_err() {
        return false;
    }
    !existing
}

/// 某用户是否已是大厅成员。
fn lobby_is_member(conn: &Connection, user_id: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM im_lobby_members WHERE user_id=?",
        params![user_id],
        |_| Ok(true),
    )
    .optional()
    .unwrap_or(Some(false))
    .unwrap_or(false)
}

/// 全量大厅成员（按加入时间排序；online 由 is_online 派生）。
fn load_lobby_members(conn: &Connection) -> rusqlite::Result<Vec<LobbyMember>> {
    let mut stmt = conn.prepare(
        "SELECT user_id,display_name,last_seen,joined_at FROM im_lobby_members ORDER BY joined_at",
    )?;
    let now = chrono::Local::now().timestamp();
    let iter = stmt.query_map([], |row| {
        let last_seen: Option<String> = row.get(2)?;
        Ok(LobbyMember {
            user_id: row.get(0)?,
            display_name: row.get(1)?,
            online: last_seen
                .as_deref()
                .map(|t| is_online(t, now))
                .unwrap_or(false),
            last_seen,
            joined_at: row.get(3)?,
        })
    })?;
    let mut out = Vec::new();
    for m in iter {
        out.push(m?);
    }
    Ok(out)
}

/// 按 after_id 增量拉会话消息（离线补拉核心查询）：
/// 返回 conversation 下 **插入序（rowid）严格大于 after_id** 的消息，升序，
/// 至多 limit 条。after_id 为 None / 空串 / 该会话中不存在的 id → 从头升序取
/// （COALESCE 回退 rowid 0，自然全量）。消息 id 是随机 uuid，不可字符串排序，
/// 故以 rowid（= 写入顺序，单调递增）作为全序基准。
fn load_messages_after(
    conn: &Connection,
    cid: &str,
    after_id: Option<&str>,
    limit: i64,
) -> rusqlite::Result<Vec<Message>> {
    let sql = format!(
        "SELECT {MSG_COLS} FROM im_messages
         WHERE conversation_id=?1
           AND rowid > COALESCE((SELECT rowid FROM im_messages WHERE id=?2 AND conversation_id=?1), 0)
         ORDER BY rowid ASC
         LIMIT ?3"
    );
    let mut stmt = conn.prepare(&sql)?;
    let iter = stmt.query_map(
        params![cid, after_id.unwrap_or(""), limit],
        message_from_row,
    )?;
    let mut out = Vec::new();
    for m in iter {
        out.push(m?);
    }
    Ok(out)
}

/// 大厅/联邦大厅最近 limit 条消息（时间正序返回，conversation_id 由调用方给定）。
///
/// 带 after_id 时切换为增量语义：严格晚于该消息的该会话消息升序取 limit 条
/// （复用 [`load_messages_after`]，与 `/api/v1/im/messages` 端点同语义）；
/// 不带 after_id 维持旧行为（DESC 取最近 limit 条再反转为正序）。
fn load_recent_conversation_messages(
    conn: &Connection,
    cid: &str,
    limit: usize,
    after_id: Option<&str>,
) -> rusqlite::Result<Vec<Message>> {
    if after_id.is_some_and(|s| !s.is_empty()) {
        return load_messages_after(conn, cid, after_id, limit as i64);
    }
    let sql = format!(
        "SELECT {MSG_COLS} FROM im_messages WHERE conversation_id=? ORDER BY created_at DESC LIMIT ?"
    );
    let mut stmt = conn.prepare(&sql)?;
    let iter = stmt.query_map(params![cid, limit as i64], message_from_row)?;
    let mut out = Vec::new();
    for m in iter {
        out.push(m?);
    }
    out.reverse(); // DESC 取最近 N 条 → 反转为时间正序
    Ok(out)
}

/// 我的大厅最近 limit 条消息（[`load_recent_conversation_messages`] 的 lobby 特化）。
fn load_recent_lobby_messages(
    conn: &Connection,
    limit: usize,
    after_id: Option<&str>,
) -> rusqlite::Result<Vec<Message>> {
    load_recent_conversation_messages(conn, LOBBY_ID, limit, after_id)
}

/// 大厅最后一条消息（无消息返回 None）。
fn last_lobby_message(conn: &Connection) -> Option<Message> {
    load_recent_lobby_messages(conn, 1, None)
        .ok()?
        .into_iter()
        .next()
}

/// 大厅信息聚合（成员数/在线数/最近消息）。
fn lobby_info(conn: &Connection) -> LobbyInfo {
    let members = load_lobby_members(conn).unwrap_or_default();
    let online_count = members.iter().filter(|m| m.online).count();
    let name: String = conn
        .query_row(
            "SELECT name FROM im_lobby WHERE id=?",
            params![LOBBY_ID],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "大厅".to_string());
    LobbyInfo {
        id: LOBBY_ID.to_string(),
        name,
        member_count: members.len(),
        online_count,
        last_message: last_lobby_message(conn),
    }
}

/// 联邦大厅信息聚合（GET /api/v1/im/fed-lobby 响应内核）。
///
/// 跨节点共享频道没有独立成员表——在场/在线沿用本节点大厅成员表（每个
/// 节点的本地用户即该节点在联邦频道的参与者）；最近消息取 fed-lobby 会话。
fn fed_lobby_info(conn: &Connection) -> LobbyInfo {
    let members = load_lobby_members(conn).unwrap_or_default();
    let online_count = members.iter().filter(|m| m.online).count();
    LobbyInfo {
        id: FED_LOBBY_ID.to_string(),
        name: "联邦大厅".to_string(),
        member_count: members.len(),
        online_count,
        last_message: load_recent_conversation_messages(conn, FED_LOBBY_ID, 1, None)
            .ok()
            .and_then(|mut v| v.pop()),
    }
}

// ----------------------------------------------------------------------------
// 单元测
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_handler() -> ImRouteHandler {
        ImRouteHandler::with_empty()
    }

    /// 快速联邦节流版 handler（1ms 级双时延注入）：双节点端到端与延迟队列
    /// 测试用——默认 10s/60s 太慢，语义不变仅缩短时钟。
    fn fast_fed_handler() -> ImRouteHandler {
        ImRouteHandler::with_empty()
            .with_fed_throttle_delays(Duration::from_millis(1), Duration::from_millis(1))
    }

    fn demo_handler() -> ImRouteHandler {
        ImRouteHandler::with_demo_data()
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

    fn post_req(path: &str, body: serde_json::Value) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Post,
            path: path.into(),
            headers: serde_json::json!({}),
            body,
            auth: None,
        }
    }

    // —— 区块链认证测试辅助（真密钥对，k256 与生产同栈）——

    /// 生成真 secp256k1 密钥对（CSPRNG）。
    fn new_key() -> k256::ecdsa::SigningKey {
        use k256::elliptic_curve::rand_core::OsRng;
        k256::ecdsa::SigningKey::random(&mut OsRng)
    }

    /// 私钥 → IM 用户名（0x + 66 hex 压缩公钥）。
    fn pubkey_hex(sk: &k256::ecdsa::SigningKey) -> String {
        format!(
            "0x{}",
            hex::encode(sk.verifying_key().to_encoded_point(true).as_bytes())
        )
    }

    /// 客户端签名：SHA-256(nonce UTF-8) → RFC6979 ECDSA（65 字节 r||s||v，
    /// v 为真实恢复位——与前端 @noble/secp256k1 sign(sha256(nonce)) 同构）。
    fn sign_nonce(sk: &k256::ecdsa::SigningKey, nonce: &str) -> [u8; 65] {
        use sha2::Digest;
        let digest = sha2::Sha256::new_with_prefix(nonce.as_bytes());
        let (sig, recid) = sk.sign_digest_recoverable(digest).expect("签名必成功");
        let mut out = [0u8; 65];
        out[..64].copy_from_slice(&sig.to_bytes());
        out[64] = u8::from(recid);
        out
    }

    /// 带 IM token 的 GET。
    fn authed_get(path: &str, token: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.into(),
            headers: serde_json::json!({"authorization": format!("Bearer {token}")}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    /// 带 IM token 的 POST。
    fn authed_post(path: &str, token: &str, body: serde_json::Value) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Post,
            path: path.into(),
            headers: serde_json::json!({"authorization": format!("Bearer {token}")}),
            body,
            auth: None,
        }
    }

    /// 真密钥对全流程登录：challenge → sign → verify → `(pubkey, token)`。
    async fn login(h: &ImRouteHandler, sk: &k256::ecdsa::SigningKey) -> (String, String) {
        let pubkey = pubkey_hex(sk);
        let resp = h
            .handle(post_req(
                PATH_AUTH_CHALLENGE,
                serde_json::json!({ "pubkey": pubkey }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "challenge 应成功: {}", resp.body);
        let nonce = resp.body["nonce"].as_str().unwrap().to_string();
        let sig = sign_nonce(sk, &nonce);
        let resp = h
            .handle(post_req(
                PATH_AUTH_VERIFY,
                serde_json::json!({
                    "pubkey": pubkey,
                    "nonce": nonce,
                    "signature": format!("0x{}", hex::encode(sig)),
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "verify 应成功: {}", resp.body);
        (pubkey, resp.body["token"].as_str().unwrap().to_string())
    }

    // 1. routes 数量（19 原有 + 2 认证 + 1 离线补拉 + 2 附件 + 3 推送通知 + 2 联邦开关
    //    + 2 大厅开放开关 + 2 远程大厅互联 + 3 联邦大厅 + 3 直通消息 = 39）
    #[tokio::test]
    async fn routes_declares_all_im_endpoints() {
        let h = empty_handler();
        let routes = h.routes().await;
        assert_eq!(
            routes.len(),
            39,
            "应声明 39 条路由（19 原有 + 2 认证 + 1 补拉 + 2 附件 + 3 推送通知 + 2 联邦开关 + 2 大厅开放 + 2 远程大厅 + 3 联邦大厅 + 3 直通消息）"
        );
        assert!(routes.iter().all(|r| r.handler_component == COMPONENT));
        let pairs: Vec<(HttpMethod, &str)> =
            routes.iter().map(|r| (r.method, r.path.as_str())).collect();
        // 认证 2 条（公开）
        assert!(pairs.contains(&(HttpMethod::Post, PATH_AUTH_CHALLENGE)));
        assert!(pairs.contains(&(HttpMethod::Post, PATH_AUTH_VERIFY)));
        // 原有 3 条扩展端点
        assert!(pairs.contains(&(HttpMethod::Post, PATH_MSG_READ)));
        assert!(pairs.contains(&(HttpMethod::Get, PATH_CONV_UNREAD)));
        assert!(pairs.contains(&(HttpMethod::Get, PATH_SEARCH)));
        // 离线补拉 1 条（IM token 在 handler 内验，不走系统中间件）
        assert!(pairs.contains(&(HttpMethod::Get, PATH_MESSAGES_CATCHUP)));
        // 大厅 4 条
        assert!(pairs.contains(&(HttpMethod::Get, PATH_LOBBY)));
        assert!(pairs.contains(&(HttpMethod::Get, PATH_LOBBY_MESSAGES)));
        assert!(pairs.contains(&(HttpMethod::Post, PATH_LOBBY_MESSAGES)));
        assert!(pairs.contains(&(HttpMethod::Get, PATH_LOBBY_MEMBERS)));
        // 附件 2 条（IM token 在 handler 内验）
        assert!(pairs.contains(&(HttpMethod::Post, PATH_FILES)));
        assert!(pairs.contains(&(HttpMethod::Get, PATH_FILE_DOWNLOAD)));
        // 推送通知 webhook 3 条（IM token 在 handler 内验，owner=pubkey）
        assert!(pairs.contains(&(HttpMethod::Post, PATH_NOTIFY_REGISTER)));
        assert!(pairs.contains(&(HttpMethod::Get, PATH_NOTIFY_LIST)));
        assert!(pairs.contains(&(HttpMethod::Delete, PATH_NOTIFY_UNREGISTER)));
        // 联邦接收开关 2 条（IM token / admin 在 handler 内验）
        assert!(pairs.contains(&(HttpMethod::Get, PATH_FEDERATION)));
        assert!(pairs.contains(&(HttpMethod::Post, PATH_FEDERATION)));
        // 大厅开放开关 2 条（admin 或 IM token 在 handler 内验）
        assert!(pairs.contains(&(HttpMethod::Get, PATH_LOBBY_ACCESS)));
        assert!(pairs.contains(&(HttpMethod::Post, PATH_LOBBY_ACCESS)));
        // 远程大厅互联 2 条（IM token 在 handler 内验）
        assert!(pairs.contains(&(HttpMethod::Get, PATH_LOBBY_REMOTE)));
        assert!(pairs.contains(&(HttpMethod::Post, PATH_LOBBY_REMOTE_MESSAGES)));
        // 联邦大厅 3 条（跨节点共享频道，IM token 在 handler 内验）
        assert!(pairs.contains(&(HttpMethod::Get, PATH_FED_LOBBY)));
        assert!(pairs.contains(&(HttpMethod::Get, PATH_FED_LOBBY_MESSAGES)));
        assert!(pairs.contains(&(HttpMethod::Post, PATH_FED_LOBBY_MESSAGES)));
        // 直通消息 DM 3 条（POST 发起 + GET/POST access 开关；IM token 在
        // handler 内验）
        assert!(pairs.contains(&(HttpMethod::Post, PATH_DM)));
        assert!(pairs.contains(&(HttpMethod::Get, PATH_DM_ACCESS)));
        assert!(pairs.contains(&(HttpMethod::Post, PATH_DM_ACCESS)));
        // 用户面端点不走系统中间件（IM token 在 handler 内验）；
        // 仅 POST /peers（管理面）保留系统级 requires_auth。
        for r in &routes {
            let expected = r.path == PATH_PEERS && r.method == HttpMethod::Post;
            assert_eq!(
                r.requires_auth, expected,
                "{:?} {} 的 requires_auth 应为 {expected}",
                r.method, r.path
            );
        }
    }

    // 2. seed 数据验证（2 对话 + 5 demo 消息 + 1 大厅欢迎消息 + 1 群组）
    #[tokio::test]
    async fn seed_data_validation() {
        let h = demo_handler();
        let resp = h.handle(get_req(PATH_STATUS)).await.unwrap();
        assert_eq!(resp.body["conversations"], 2);
        assert_eq!(resp.body["messages"], 6, "5 条 demo + 1 条大厅欢迎系统消息");
        assert_eq!(resp.body["groups"], 1);
    }

    // 3. SQLite conversations roundtrip（认证后 created_by = token pubkey）
    #[tokio::test]
    async fn conversations_roundtrip() {
        let h = empty_handler();
        let sk = new_key();
        let (pubkey, token) = login(&h, &sk).await;
        let resp = h
            .handle(authed_post(
                PATH_CONV_LIST,
                &token,
                serde_json::json!({ "name": "测试对话", "created_by": "forged-attacker" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        let id = resp.body["id"].as_str().unwrap().to_string();
        assert_eq!(resp.body["name"], "测试对话");
        assert_eq!(
            resp.body["created_by"], pubkey,
            "created_by 应为 token pubkey"
        );
        // 列表含新对话
        let list = h.handle(authed_get(PATH_CONV_LIST, &token)).await.unwrap();
        assert_eq!(list.body.as_array().unwrap().len(), 1);
        // snapshot 也能查到（DB 真实写入）
        let snap = h.conversations_snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].id, id);
    }

    // 4. SQLite messages roundtrip（含增强字段；sender 一律 = token pubkey）
    #[tokio::test]
    async fn messages_roundtrip_enhanced_fields() {
        let h = empty_handler();
        let sk = new_key();
        let (pubkey, token) = login(&h, &sk).await;
        let display_name = derive_display_name(&parse_im_pubkey(&pubkey).unwrap());
        // 先建对话
        let c = h
            .handle(authed_post(
                PATH_CONV_LIST,
                &token,
                serde_json::json!({ "name": "c1" }),
            ))
            .await
            .unwrap();
        let cid = c.body["id"].as_str().unwrap().to_string();
        // 发一条带 file_url 的消息（自报 sender 字段应被忽略/覆盖）
        let resp = h
            .handle(authed_post(
                &format!("/api/v1/im/conversations/{cid}/messages"),
                &token,
                serde_json::json!({
                    "content": "见附件",
                    "sender_id": "alice",
                    "sender_name": "Alice",
                    "msg_type": "file",
                    "file_url": "/tank/a.pdf"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["msg_type"], "file");
        assert_eq!(resp.body["file_url"], "/tank/a.pdf");
        assert_eq!(resp.body["sender_id"], pubkey, "sender_id 应被服务端覆盖");
        assert_eq!(resp.body["sender_name"], display_name);
        // 历史可查回
        let hist = h
            .handle(authed_get(
                &format!("/api/v1/im/conversations/{cid}/messages"),
                &token,
            ))
            .await
            .unwrap();
        let arr = hist.body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["content"], "见附件");
        assert_eq!(arr[0]["file_url"], "/tank/a.pdf");
    }

    // 5. SQLite groups + members roundtrip（owner/joiner = token pubkey）
    #[tokio::test]
    async fn groups_and_members_roundtrip() {
        let h = empty_handler();
        let (pubkey1, token1) = login(&h, &new_key()).await;
        let (pubkey2, token2) = login(&h, &new_key()).await;
        let resp = h
            .handle(authed_post(
                PATH_GROUPS,
                &token1,
                serde_json::json!({ "name": "team", "owner": "forged-attacker" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["owner"], pubkey1, "owner 应为 token pubkey");
        let gid = resp.body["id"].as_str().unwrap().to_string();
        // 列表含新群组，含 members
        let list = h.handle(authed_get(PATH_GROUPS, &token1)).await.unwrap();
        let arr = list.body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["kind"], "group");
        let members = arr[0]["members"].as_array().unwrap();
        assert!(members.contains(&serde_json::json!(pubkey1)));
        // 第二个用户 join（body member 自报值被忽略）
        h.handle(authed_post(
            &format!("/api/v1/im/groups/{gid}/join"),
            &token2,
            serde_json::json!({ "member": "forged-attacker" }),
        ))
        .await
        .unwrap();
        let members_resp = h
            .handle(authed_get(
                &format!("/api/v1/im/groups/{gid}/members"),
                &token2,
            ))
            .await
            .unwrap();
        let m = members_resp.body["members"].as_array().unwrap();
        assert!(
            m.contains(&serde_json::json!(pubkey2)),
            "join 应以 token pubkey 入组"
        );
        assert!(!m.contains(&serde_json::json!("forged-attacker")));
    }

    // 6. 消息已读标记（已读人 = token pubkey）
    #[tokio::test]
    async fn mark_message_read() {
        let h = demo_handler();
        let (pubkey, token) = login(&h, &new_key()).await;
        // msg-1（conv-general, sender alice）初始 read_by=["alice"]
        let before = h
            .handle(authed_post(
                "/api/v1/im/messages/msg-1/read",
                &token,
                serde_json::json!({ "user_id": "forged-attacker" }),
            ))
            .await
            .unwrap();
        assert_eq!(before.status, 200);
        let read_by = before.body["read_by"].as_array().unwrap();
        assert!(
            read_by.contains(&serde_json::json!(pubkey)),
            "已读人应为 token pubkey"
        );
        assert!(read_by.contains(&serde_json::json!("alice")));
        assert!(!read_by.contains(&serde_json::json!("forged-attacker")));
        // 不存在消息 → 404
        let miss = h
            .handle(authed_post(
                "/api/v1/im/messages/nope/read",
                &token,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(miss.status, 404);
    }

    // 7. 未读计数（user = token pubkey，查询参数 ?user= 被忽略）
    #[tokio::test]
    async fn unread_count() {
        let h = demo_handler();
        let (pubkey, token) = login(&h, &new_key()).await;
        // conv-general 2 条 demo 消息对新身份（pubkey）全未读；?user=alice 自报应被忽略
        let resp = h
            .handle(authed_get(
                "/api/v1/im/conversations/conv-general/unread?user=alice",
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["unread"], 2);
        assert_eq!(resp.body["user"], pubkey);
        // 标记 msg-1 已读后再查 → 1
        h.handle(authed_post(
            "/api/v1/im/messages/msg-1/read",
            &token,
            serde_json::json!({}),
        ))
        .await
        .unwrap();
        let resp2 = h
            .handle(authed_get(
                "/api/v1/im/conversations/conv-general/unread",
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(resp2.body["unread"], 1);
    }

    // 8. 搜索面（2026-08-22 迭代：会话范围 + member 门 + limit 钳制 + LIKE 转义 + 空 q 400）
    // 8a. 会话搜索（指定 conversation_id）：直接对话可读 → 命中 demo msg-3
    #[tokio::test]
    async fn search_scoped_conversation() {
        let h = demo_handler();
        let (_, token) = login(&h, &new_key()).await;
        let resp = h
            .handle(authed_get(
                "/api/v1/im/search?q=设计文档&conversation_id=conv-dev",
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "直接对话搜索应 200: {}", resp.body);
        assert_eq!(resp.body["count"], 1);
        let results = resp.body["results"].as_array().unwrap();
        assert_eq!(results[0]["content"], "看看这份设计文档");
        // 回显：q 原文（前端高亮用）+ 实际搜索的会话 id
        assert_eq!(resp.body["q"], "设计文档");
        assert_eq!(resp.body["conversation_id"], "conv-dev");
        // 范围隔离：搜 conv-general 不命中 conv-dev 的消息
        let miss = h
            .handle(authed_get(
                "/api/v1/im/search?q=设计文档&conversation_id=conv-general",
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(miss.body["count"], 0);
    }

    // 8b. 大厅搜索（conversation_id 缺省 = lobby）：先 GET /lobby 自动加入，
    //     欢迎消息含 "大厅"；新发消息按 created_at 倒序排最前
    #[tokio::test]
    async fn search_lobby_default_scope() {
        let h = demo_handler();
        let (_, token) = login(&h, &new_key()).await;
        // 未加入大厅 → 403（member 门同补拉）
        let denied = h
            .handle(authed_get("/api/v1/im/search?q=大厅", &token))
            .await
            .unwrap();
        assert_eq!(denied.status, 403, "未加入大厅搜索应 403");
        // GET /lobby 自动加入（落欢迎消息）
        let join = h
            .handle(authed_get("/api/v1/im/lobby", &token))
            .await
            .unwrap();
        assert_eq!(join.status, 200);
        // 再发一条含关键词的消息 → 倒序第一条
        let sent = h
            .handle(authed_post(
                "/api/v1/im/lobby/messages",
                &token,
                serde_json::json!({ "content": "这条也提到大厅测试" }),
            ))
            .await
            .unwrap();
        assert_eq!(sent.status, 201);
        let resp = h
            .handle(authed_get("/api/v1/im/search?q=大厅", &token))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "加入后默认搜大厅应 200: {}", resp.body);
        assert_eq!(resp.body["conversation_id"], "lobby");
        let results = resp.body["results"].as_array().unwrap();
        assert!(results.len() >= 2, "欢迎消息 + 新消息都应命中");
        assert_eq!(results[0]["content"], "这条也提到大厅测试", "最新在前");
    }

    // 8c. 会话搜索权限：群组非成员 403（join 后 200）；未知会话 404
    #[tokio::test]
    async fn search_conversation_permission() {
        let h = demo_handler();
        let (_, token) = login(&h, &new_key()).await;
        // 群组 demo 数据成员是 alice/bob/carol，新身份非成员 → 403
        let denied = h
            .handle(authed_get(
                "/api/v1/im/search?q=群组&conversation_id=group-dev-team",
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(denied.status, 403, "非群组成员搜索应 403");
        // join 后 → 200，"群组" 命中 msg-5（Carol 加入了群组）
        let join = h
            .handle(authed_post(
                "/api/v1/im/groups/group-dev-team/join",
                &token,
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(join.status, 200);
        let resp = h
            .handle(authed_get(
                "/api/v1/im/search?q=群组&conversation_id=group-dev-team",
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["count"], 1);
        // 未知会话 → 404
        let missing = h
            .handle(authed_get(
                "/api/v1/im/search?q=hi&conversation_id=nope",
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(missing.status, 404);
    }

    // 8d. LIKE 特殊字符按字面匹配（% / _ / \ 转义，ESCAPE '\'）
    #[tokio::test]
    async fn search_like_wildcard_escaped() {
        let h = demo_handler();
        let (_, token) = login(&h, &new_key()).await;
        h.handle(authed_get("/api/v1/im/lobby", &token))
            .await
            .unwrap();
        for content in ["折扣 100% off", "打五折不含百分号", "a_b 下划线"] {
            let r = h
                .handle(authed_post(
                    "/api/v1/im/lobby/messages",
                    &token,
                    serde_json::json!({ "content": content }),
                ))
                .await
                .unwrap();
            assert_eq!(r.status, 201);
        }
        // q="100%" 只命中含字面 % 的那条（未转义时 % 是通配符，会命中 "100" 开头的一切）
        let pct = h
            .handle(authed_get("/api/v1/im/search?q=100%25", &token))
            .await
            .unwrap();
        assert_eq!(pct.status, 200);
        let results = pct.body["results"].as_array().unwrap();
        assert_eq!(results.len(), 1, "百分号按字面匹配: {}", pct.body);
        assert_eq!(results[0]["content"], "折扣 100% off");
        // q="%" 不命中无百分号的消息（未转义的 LIKE "%%%" 匹配一切）
        let only_pct = h
            .handle(authed_get("/api/v1/im/search?q=%25", &token))
            .await
            .unwrap();
        assert_eq!(
            only_pct.body["results"].as_array().unwrap().len(),
            1,
            "裸 % 只匹配字面百分号: {}",
            only_pct.body
        );
        // q="_" 只命中含字面下划线的消息（未转义的 "%_%" 匹配任意非空）
        let und = h
            .handle(authed_get("/api/v1/im/search?q=_", &token))
            .await
            .unwrap();
        let und_results = und.body["results"].as_array().unwrap();
        assert_eq!(und_results.len(), 1, "下划线按字面匹配: {}", und.body);
        assert_eq!(und_results[0]["content"], "a_b 下划线");
    }

    // 8e. limit 钳制（默认 50，钳到 1..=200）
    #[tokio::test]
    async fn search_limit_clamped() {
        let h = demo_handler();
        let (_, token) = login(&h, &new_key()).await;
        h.handle(authed_get("/api/v1/im/lobby", &token))
            .await
            .unwrap();
        for i in 1..=3 {
            let r = h
                .handle(authed_post(
                    "/api/v1/im/lobby/messages",
                    &token,
                    serde_json::json!({ "content": format!("限额测试条目{i}") }),
                ))
                .await
                .unwrap();
            assert_eq!(r.status, 201);
        }
        // limit=2 → 2 条
        let two = h
            .handle(authed_get("/api/v1/im/search?q=限额测试&limit=2", &token))
            .await
            .unwrap();
        assert_eq!(two.body["count"], 2);
        // limit=0 → 钳到 1
        let one = h
            .handle(authed_get("/api/v1/im/search?q=限额测试&limit=0", &token))
            .await
            .unwrap();
        assert_eq!(one.body["count"], 1, "limit=0 应钳到 1: {}", one.body);
        // limit=99999 → 钳到 200（3 条全返回，不报错）
        let big = h
            .handle(authed_get(
                "/api/v1/im/search?q=限额测试&limit=99999",
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(big.status, 200);
        assert_eq!(big.body["count"], 3);
        // 非法 limit → 回退默认 50
        let bad = h
            .handle(authed_get("/api/v1/im/search?q=限额测试&limit=abc", &token))
            .await
            .unwrap();
        assert_eq!(bad.body["count"], 3);
    }

    // 8f. 空 q → 400（缺省 / 空串 / 纯空白）
    #[tokio::test]
    async fn search_empty_q_rejected() {
        let h = demo_handler();
        let (_, token) = login(&h, &new_key()).await;
        h.handle(authed_get("/api/v1/im/lobby", &token))
            .await
            .unwrap();
        for path in [
            "/api/v1/im/search",
            "/api/v1/im/search?q=",
            "/api/v1/im/search?q=%20%20",
        ] {
            let resp = h.handle(authed_get(path, &token)).await.unwrap();
            assert_eq!(resp.status, 400, "{path} 空 q 应 400");
        }
        // 无 token → 401（语义同其它 IM 用户面端点）
        let anon = h.handle(get_req("/api/v1/im/search?q=hi")).await.unwrap();
        assert_eq!(anon.status, 401);
    }

    // 9. 消息按时间排序
    #[tokio::test]
    async fn messages_ordered_by_time() {
        let h = demo_handler();
        let (_, token) = login(&h, &new_key()).await;
        let resp = h
            .handle(authed_get(
                "/api/v1/im/conversations/conv-general/messages",
                &token,
            ))
            .await
            .unwrap();
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        // msg-1 (09:00) 在 msg-2 (09:01) 之前
        assert_eq!(arr[0]["id"], "msg-1");
        assert_eq!(arr[1]["id"], "msg-2");
    }

    // 10. file_url 消息类型（msg-3 为 file）
    #[tokio::test]
    async fn file_message_type() {
        let h = demo_handler();
        let (_, token) = login(&h, &new_key()).await;
        let resp = h
            .handle(authed_get(
                "/api/v1/im/conversations/conv-dev/messages",
                &token,
            ))
            .await
            .unwrap();
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["msg_type"], "file");
        assert_eq!(arr[0]["file_url"], "/tank/docs/design.pdf");
    }

    // 11. 系统消息类型（msg-5 为 system）
    #[tokio::test]
    async fn system_message_type() {
        let h = demo_handler();
        let (_, token) = login(&h, &new_key()).await;
        let resp = h
            .handle(authed_get(
                "/api/v1/im/conversations/group-dev-team/messages",
                &token,
            ))
            .await
            .unwrap();
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[1]["msg_type"], "system");
    }

    // 12. peers 增查（兼容前端 addr 字段）
    #[tokio::test]
    async fn peers_add_and_list() {
        let h = empty_handler();
        let resp = h
            .handle(post_req(
                PATH_PEERS,
                serde_json::json!({ "addr": "10.0.0.5:8443", "id": "node-5", "name": "nodeA" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["id"], "node-5");
        assert_eq!(resp.body["endpoint"], "10.0.0.5:8443");
        assert_eq!(resp.body["status"], "online");
        assert_eq!(h.peers_snapshot().len(), 1);
    }

    // 13. 向群组发消息（group id 可作为 conversation_id）+ WS 广播
    #[tokio::test]
    async fn send_message_to_group() {
        let hub = WsHub::default();
        let (_id, _rx) = hub.subscribe_raw("probe");
        // 注入 ws_hub + ImAuth 的 handler（内存库 + seed）
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        seed_demo(&conn).unwrap();
        let h = ImRouteHandler::from_parts(conn, Some(hub.clone()), Arc::new(ImAuth::default()));
        let (pubkey, token) = login(&h, &new_key()).await;
        let resp = h
            .handle(authed_post(
                "/api/v1/im/conversations/group-dev-team/messages",
                &token,
                serde_json::json!({ "content": "新消息", "sender_id": "forged-attacker" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["conversation_id"], "group-dev-team");
        assert_eq!(resp.body["sender_id"], pubkey);
        // hub 有 1 个订阅 → 广播应送达 1 个
        assert_eq!(hub.subscriber_count(), 1);
    }

    // 14. send_message 不存在的对话 → 404（无 token 先 401）
    #[tokio::test]
    async fn send_message_unknown_conversation_404() {
        let h = empty_handler();
        let (_, token) = login(&h, &new_key()).await;
        let resp = h
            .handle(authed_post(
                "/api/v1/im/conversations/nope/messages",
                &token,
                serde_json::json!({ "content": "hi" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // 15. 兜底未匹配 → 404
    #[tokio::test]
    async fn unmatched_route_404() {
        let h = empty_handler();
        let resp = h.handle(get_req("/api/v1/im/unknown")).await.unwrap();
        assert_eq!(resp.status, 404);
        assert!(resp.body["error"].as_str().unwrap().contains("未匹配"));
    }

    // =========================================================================
    // 离线补拉（GET /api/v1/im/messages?conversation_id=&after_id=&limit=）
    // 单元测——WS 断线重连后的缺口增量语义
    // =========================================================================

    // C1. after_id 语义：缺省 → 全量升序；严格大于（不含 after_id 本身）；
    //     未知 after_id → 从头升序；结果按插入序（rowid）升序
    #[tokio::test]
    async fn catchup_after_id_semantics() {
        let h = demo_handler();
        let (_, token) = login(&h, &new_key()).await;
        // conv-general 有 msg-1（09:00）/ msg-2（09:01）
        let base = "/api/v1/im/messages?conversation_id=conv-general";
        // 缺省 after_id → 全量升序
        let resp = h.handle(authed_get(base, &token)).await.unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["id"], "msg-1", "升序：msg-1 在前");
        assert_eq!(arr[1]["id"], "msg-2");
        // after_id=msg-1 → 严格大于 → 只剩 msg-2（不含 after_id 本身）
        let resp = h
            .handle(authed_get(&format!("{base}&after_id=msg-1"), &token))
            .await
            .unwrap();
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 1, "严格晚于 msg-1 的只有 msg-2");
        assert_eq!(arr[0]["id"], "msg-2");
        // after_id 指向别的会话的消息 → 本会话无此 id → 从头升序
        let resp = h
            .handle(authed_get(&format!("{base}&after_id=msg-3"), &token))
            .await
            .unwrap();
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 2, "未知 after_id 应回退到从头取");
        assert_eq!(arr[0]["id"], "msg-1");
    }

    // C2. limit：缺省 50；上限 200 钳制；非法值回退默认；下限 1 钳制
    #[tokio::test]
    async fn catchup_limit_clamped() {
        let h = empty_handler();
        let (_, token) = login(&h, &new_key()).await;
        let c = h
            .handle(authed_post(
                PATH_CONV_LIST,
                &token,
                serde_json::json!({ "name": "灌水群" }),
            ))
            .await
            .unwrap();
        let cid = c.body["id"].as_str().unwrap().to_string();
        // 直接落库 205 条（经 insert_message 保持 rowid = 写入序）
        {
            let conn = h.shared.db.lock().expect("db poisoned");
            for i in 1..=205 {
                let m = Message {
                    id: format!("flood-{i}"),
                    conversation_id: cid.clone(),
                    sender_id: "bob".into(),
                    sender_name: Some("Bob".into()),
                    content: format!("第 {i} 条"),
                    msg_type: "text".into(),
                    file_url: None,
                    reply_to: None,
                    created_at: format!("2026-01-01T00:{:02}:{:02}+08:00", i / 60, i % 60),
                    read_by: Vec::new(),
                    sender_kind: "human".into(),
                    mentions: Vec::new(),
                    attachment: None,
                };
                insert_message(&conn, &m).unwrap();
            }
        }
        let base = format!("/api/v1/im/messages?conversation_id={cid}");
        // 缺省 → 50（从最早一条开始升序）
        let resp = h.handle(authed_get(&base, &token)).await.unwrap();
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 50, "默认 limit=50");
        assert_eq!(arr[0]["id"], "flood-1", "升序从头取");
        assert_eq!(arr[49]["id"], "flood-50");
        // 上限钳制：limit=10000 → 200
        let resp = h
            .handle(authed_get(&format!("{base}&limit=10000"), &token))
            .await
            .unwrap();
        assert_eq!(resp.body.as_array().unwrap().len(), 200, "上限 200 钳制");
        // 超上限但不足全量：limit=201+ 且总量更少时按 200
        let resp = h
            .handle(authed_get(&format!("{base}&limit=99999"), &token))
            .await
            .unwrap();
        assert_eq!(resp.body.as_array().unwrap().len(), 200);
        // 小 limit
        let resp = h
            .handle(authed_get(&format!("{base}&limit=3"), &token))
            .await
            .unwrap();
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[2]["id"], "flood-3");
        // 非法 limit → 默认 50；limit=0 → 钳到 1
        let resp = h
            .handle(authed_get(&format!("{base}&limit=abc"), &token))
            .await
            .unwrap();
        assert_eq!(
            resp.body.as_array().unwrap().len(),
            50,
            "非法 limit 回退 50"
        );
        let resp = h
            .handle(authed_get(&format!("{base}&limit=0"), &token))
            .await
            .unwrap();
        assert_eq!(resp.body.as_array().unwrap().len(), 1, "limit=0 钳到 1");
        // after_id + limit 组合：从 flood-200 之后只剩 5 条
        let resp = h
            .handle(authed_get(
                &format!("{base}&after_id=flood-200&limit=200"),
                &token,
            ))
            .await
            .unwrap();
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 5);
        assert_eq!(arr[0]["id"], "flood-201");
        assert_eq!(arr[4]["id"], "flood-205");
    }

    // C3. 越权与存在性：群组非成员 403（join 后 200）；直接对话沿用全员可读；
    //     未知会话 404；缺 conversation_id 400；无 token 401
    #[tokio::test]
    async fn catchup_membership_and_existence_gates() {
        let h = empty_handler();
        let (pubkey1, token1) = login(&h, &new_key()).await;
        let (_pubkey2, token2) = login(&h, &new_key()).await;
        // 用户 1 建群（自己是 owner）+ 建直接对话
        let g = h
            .handle(authed_post(
                PATH_GROUPS,
                &token1,
                serde_json::json!({ "name": "私密群" }),
            ))
            .await
            .unwrap();
        let gid = g.body["id"].as_str().unwrap().to_string();
        let c = h
            .handle(authed_post(
                PATH_CONV_LIST,
                &token1,
                serde_json::json!({ "name": "直接对话" }),
            ))
            .await
            .unwrap();
        let cid = c.body["id"].as_str().unwrap().to_string();
        // 用户 2 非群成员 → 403
        let denied = h
            .handle(authed_get(
                &format!("/api/v1/im/messages?conversation_id={gid}"),
                &token2,
            ))
            .await
            .unwrap();
        assert_eq!(denied.status, 403, "非群组成员补拉应 403");
        // join 后 → 200
        h.handle(authed_post(
            &format!("/api/v1/im/groups/{gid}/join"),
            &token2,
            serde_json::json!({}),
        ))
        .await
        .unwrap();
        let allowed = h
            .handle(authed_get(
                &format!("/api/v1/im/messages?conversation_id={gid}"),
                &token2,
            ))
            .await
            .unwrap();
        assert_eq!(allowed.status, 200, "join 后应可补拉");
        assert_eq!(allowed.body.as_array().unwrap().len(), 0);
        // 直接对话：非创建者也可读（沿用现状，保持兼容）
        let conv = h
            .handle(authed_get(
                &format!("/api/v1/im/messages?conversation_id={cid}"),
                &token2,
            ))
            .await
            .unwrap();
        assert_eq!(conv.status, 200, "直接对话沿用全员可读");
        // 大厅：未加入 → 403；GET /lobby 自动加入后 → 200
        let lobby_denied = h
            .handle(authed_get(
                "/api/v1/im/messages?conversation_id=lobby",
                &token2,
            ))
            .await
            .unwrap();
        assert_eq!(lobby_denied.status, 403, "未加入大厅补拉应 403");
        let _ = h.handle(authed_get(PATH_LOBBY, &token2)).await.unwrap();
        let lobby_ok = h
            .handle(authed_get(
                "/api/v1/im/messages?conversation_id=lobby",
                &token2,
            ))
            .await
            .unwrap();
        assert_eq!(lobby_ok.status, 200);
        // 未知会话 → 404
        let miss = h
            .handle(authed_get(
                "/api/v1/im/messages?conversation_id=no-such",
                &token2,
            ))
            .await
            .unwrap();
        assert_eq!(miss.status, 404);
        // 缺 conversation_id → 400；无 token → 401
        let bad = h
            .handle(authed_get("/api/v1/im/messages", &token2))
            .await
            .unwrap();
        assert_eq!(bad.status, 400);
        let anon = h
            .handle(get_req("/api/v1/im/messages?conversation_id=x"))
            .await
            .unwrap();
        assert_eq!(anon.status, 401);
        // 自查：用户 1（owner）读自己的群没问题
        let own = h
            .handle(authed_get(
                &format!("/api/v1/im/messages?conversation_id={gid}"),
                &token1,
            ))
            .await
            .unwrap();
        assert_eq!(own.status, 200);
        assert_eq!(own.body.as_array().unwrap().len(), 0);
        assert!(!pubkey1.is_empty());
    }

    // C4. 空结果：after_id 已是最新一条 → 返回空数组（重连补拉常见的"无缺口"）
    #[tokio::test]
    async fn catchup_empty_result_when_after_latest() {
        let h = demo_handler();
        let (_, token) = login(&h, &new_key()).await;
        let resp = h
            .handle(authed_get(
                "/api/v1/im/messages?conversation_id=conv-general&after_id=msg-2",
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(
            resp.body.as_array().unwrap().len(),
            0,
            "after 最新一条应返回空数组"
        );
        // 空会话（新建对话无消息）→ 也是空数组而非错误
        let c = h
            .handle(authed_post(
                PATH_CONV_LIST,
                &token,
                serde_json::json!({ "name": "空对话" }),
            ))
            .await
            .unwrap();
        let cid = c.body["id"].as_str().unwrap().to_string();
        let resp = h
            .handle(authed_get(
                &format!("/api/v1/im/messages?conversation_id={cid}&after_id=whatever"),
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body.as_array().unwrap().len(), 0);
    }

    // C5. 大厅同语义：GET /lobby/messages?after_id= 严格晚于、升序、
    //     after 最新 → 空；未知 after_id → 从头（与补拉端点一致）
    #[tokio::test]
    async fn lobby_after_id_same_semantics() {
        let h = empty_handler();
        let (_, token) = login(&h, &new_key()).await;
        // 进大厅（自动加入 + 欢迎消息）：消息流 = seed 欢迎 + 我的欢迎
        let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
        // 再发 2 条
        for content in ["第一条", "第二条"] {
            let resp = h
                .handle(authed_post(
                    PATH_LOBBY_MESSAGES,
                    &token,
                    serde_json::json!({ "content": content }),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 201, "{content} 应发成功");
        }
        // 全量（旧行为：最近 50 条正序）拿 id 基准
        let full = h
            .handle(authed_get(PATH_LOBBY_MESSAGES, &token))
            .await
            .unwrap();
        let arr = full.body.as_array().unwrap();
        assert_eq!(arr.len(), 4, "seed 欢迎 + 用户欢迎 + 2 条发言");
        let ids: Vec<&str> = arr.iter().map(|m| m["id"].as_str().unwrap()).collect();
        // after_id = 第 2 条（用户欢迎）→ 只剩 2 条发言，且顺序升序
        let resp = h
            .handle(authed_get(
                &format!("{}?after_id={}", PATH_LOBBY_MESSAGES, ids[1]),
                &token,
            ))
            .await
            .unwrap();
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 2, "严格晚于第 2 条的是后 2 条发言");
        assert_eq!(arr[0]["content"], "第一条");
        assert_eq!(arr[1]["content"], "第二条");
        // after 最新 → 空
        let resp = h
            .handle(authed_get(
                &format!("{}?after_id={}", PATH_LOBBY_MESSAGES, ids[3]),
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(resp.body.as_array().unwrap().len(), 0);
        // 未知 after_id → 从头（4 条全量，≤50）
        let resp = h
            .handle(authed_get(
                &format!("{}?after_id=nonexistent", PATH_LOBBY_MESSAGES),
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(resp.body.as_array().unwrap().len(), 4);
        // 无 token → 401
        let anon = h
            .handle(get_req(&format!("{}?after_id=x", PATH_LOBBY_MESSAGES)))
            .await
            .unwrap();
        assert_eq!(anon.status, 401);
    }

    // 辅助函数自测
    #[test]
    fn path_segments_parses_correctly() {
        assert_eq!(
            path_segments("/api/v1/im/groups"),
            vec!["api", "v1", "im", "groups"]
        );
        assert_eq!(
            path_segments("/api/v1/im/search?q=hi"),
            vec!["api", "v1", "im", "search"]
        );
    }

    #[test]
    fn parse_query_str_works() {
        assert_eq!(parse_query_str("q=hi&x=1", "q"), Some("hi".into()));
        assert_eq!(parse_query_str("a=1&q=hello", "q"), Some("hello".into()));
        assert_eq!(parse_query_str("", "q"), None);
    }

    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<ImRouteHandler>();
    }

    #[test]
    fn dto_round_trips_serde() {
        let m = Message {
            id: "x".into(),
            conversation_id: "c1".into(),
            sender_id: "alice".into(),
            sender_name: Some("Alice".into()),
            content: "hi".into(),
            msg_type: "text".into(),
            file_url: None,
            reply_to: None,
            created_at: "2026-01-01T00:00:00+08:00".into(),
            read_by: vec!["alice".into()],
            sender_kind: "human".into(),
            mentions: Vec::new(),
            attachment: None,
        };
        let v = serde_json::to_value(&m).unwrap();
        let back: Message = serde_json::from_value(v).unwrap();
        assert_eq!(back.id, "x");
        assert_eq!(back.read_by.len(), 1);
    }

    // =========================================================================
    // 大厅（Lobby）单元测
    // =========================================================================

    /// 辅助：生成距 now 偏移 offset_secs 秒的 RFC3339 时间串。
    fn iso_offset_secs(offset_secs: i64) -> String {
        (chrono::Local::now() + chrono::Duration::seconds(offset_secs))
            .format("%Y-%m-%dT%H:%M:%S%:z")
            .to_string()
    }

    // L1. 大厅 seed：空库建表即有大厅 + 欢迎系统消息（无 token → 401）
    #[tokio::test]
    async fn lobby_seed_creates_hall_and_welcome() {
        let h = empty_handler();
        let sk = new_key();
        let (_, token) = login(&h, &sk).await;
        // 大厅端点一律要求 IM token
        let anon = h.handle(get_req(PATH_LOBBY)).await.unwrap();
        assert_eq!(anon.status, 401, "无 token 的大厅访问应 401");
        let resp = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["id"], "lobby");
        assert_eq!(resp.body["name"], "大厅");
        assert_eq!(resp.body["member_count"], 1, "首次 GET 即自动加入");
        // seed 欢迎消息（im_messages，conversation_id=lobby）仍可见
        // （seed 1 条 + 本次 GET /lobby 自动加入的欢迎 1 条）
        let msgs = h
            .handle(authed_get(PATH_LOBBY_MESSAGES, &token))
            .await
            .unwrap();
        let arr = msgs.body.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["msg_type"], "system");
        assert_eq!(
            arr[0]["content"],
            "欢迎来到 NexOS 大厅 — 连接每一个超级个体"
        );
        // info.last_message = 本次新用户的欢迎系统消息（最新的那条）
        assert_eq!(
            resp.body["last_message"]["content"],
            format!(
                "欢迎 {} 加入 NexOS 大厅",
                derive_display_name(sk.verifying_key())
            )
        );
    }

    // L2. 新用户首次 GET /lobby（Bearer）：自动加入 + 欢迎系统消息（展示名）
    #[tokio::test]
    async fn lobby_first_visit_joins_and_welcomes() {
        let h = empty_handler();
        let sk = new_key();
        let (pubkey, token) = login(&h, &sk).await;
        let display_name = derive_display_name(&parse_im_pubkey(&pubkey).unwrap());
        let resp = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
        assert_eq!(resp.body["member_count"], 1);
        assert_eq!(resp.body["online_count"], 1, "刚心跳过应在线");
        // 欢迎消息已入大厅消息流（seed 1 条 + 欢迎 1 条），内容用展示名
        let msgs = h
            .handle(authed_get(PATH_LOBBY_MESSAGES, &token))
            .await
            .unwrap();
        let arr = msgs.body.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(
            arr[1]["content"],
            format!("欢迎 {display_name} 加入 NexOS 大厅")
        );
        assert_eq!(arr[1]["msg_type"], "system");
        // 重复 GET 不再重复欢迎
        let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
        let msgs2 = h
            .handle(authed_get(PATH_LOBBY_MESSAGES, &token))
            .await
            .unwrap();
        assert_eq!(
            msgs2.body.as_array().unwrap().len(),
            2,
            "重复进入不再发欢迎"
        );
    }

    // L3. 发大厅消息：非成员 403；加入后 201 + 消息可查（sender = token pubkey）
    #[tokio::test]
    async fn lobby_send_message_member_gating() {
        let h = empty_handler();
        let sk = new_key();
        let (pubkey, token) = login(&h, &sk).await;
        // 未加入（从未 GET /lobby）→ 403
        let denied = h
            .handle(authed_post(
                PATH_LOBBY_MESSAGES,
                &token,
                serde_json::json!({ "user_id": "forged-attacker", "content": "hi" }),
            ))
            .await
            .unwrap();
        assert_eq!(denied.status, 403);
        // 先进大厅（自动加入）→ 发言成功
        let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
        let sent = h
            .handle(authed_post(
                PATH_LOBBY_MESSAGES,
                &token,
                serde_json::json!({ "user_id": "forged-attacker", "content": "大家好！" }),
            ))
            .await
            .unwrap();
        assert_eq!(sent.status, 201);
        assert_eq!(sent.body["conversation_id"], "lobby");
        assert_eq!(sent.body["sender_id"], pubkey, "sender 应为 token pubkey");
        assert_eq!(sent.body["msg_type"], "text");
        // 大厅消息流可见（seed + 欢迎系统消息 + 发言）
        let msgs = h
            .handle(authed_get(PATH_LOBBY_MESSAGES, &token))
            .await
            .unwrap();
        let contents: Vec<&str> = msgs
            .body
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["content"].as_str().unwrap())
            .collect();
        assert!(contents.contains(&"大家好！"));
        // 空内容 → 400
        let empty = h
            .handle(authed_post(
                PATH_LOBBY_MESSAGES,
                &token,
                serde_json::json!({ "content": "  " }),
            ))
            .await
            .unwrap();
        assert_eq!(empty.status, 400);
    }

    // L4. 大厅成员列表：在线/离线区分（成员以 pubkey 记录）
    #[tokio::test]
    async fn lobby_members_online_offline() {
        let h = empty_handler();
        let (pubkey, token) = login(&h, &new_key()).await;
        let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
        // 把该成员的 last_seen 拨回 1 小时前 → 离线
        {
            let conn = h.shared.db.lock().expect("db poisoned");
            conn.execute(
                "UPDATE im_lobby_members SET last_seen=? WHERE user_id=?",
                params![iso_offset_secs(-3600), pubkey],
            )
            .unwrap();
        }
        let resp = h
            .handle(authed_get(PATH_LOBBY_MEMBERS, &token))
            .await
            .unwrap();
        assert_eq!(resp.body["member_count"], 1);
        assert_eq!(resp.body["online_count"], 0);
        let members = resp.body["members"].as_array().unwrap();
        assert_eq!(members[0]["user_id"], pubkey);
        assert_eq!(members[0]["online"], false);
    }

    // L5. 心跳：GET /lobby/messages（Bearer）刷新 last_seen（离线 → 在线）
    #[tokio::test]
    async fn lobby_heartbeat_touches_last_seen() {
        let h = empty_handler();
        let (pubkey, token) = login(&h, &new_key()).await;
        let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
        // 拨回 5 分钟前 → 离线
        {
            let conn = h.shared.db.lock().expect("db poisoned");
            conn.execute(
                "UPDATE im_lobby_members SET last_seen=? WHERE user_id=?",
                params![iso_offset_secs(-300), pubkey],
            )
            .unwrap();
        }
        let before = h
            .handle(authed_get(PATH_LOBBY_MEMBERS, &token))
            .await
            .unwrap();
        assert_eq!(before.body["online_count"], 0);
        // GET /lobby/messages（Bearer 心跳）→ 重新在线
        let _ = h
            .handle(authed_get(PATH_LOBBY_MESSAGES, &token))
            .await
            .unwrap();
        let after = h
            .handle(authed_get(PATH_LOBBY_MEMBERS, &token))
            .await
            .unwrap();
        assert_eq!(after.body["online_count"], 1, "心跳后应在线");
    }

    // L6. is_online 纯函数：60s 窗口 / 解析失败
    #[test]
    fn is_online_window_semantics() {
        let now = chrono::Local::now().timestamp();
        // RFC3339 串
        let just_now = iso_offset_secs(-5);
        let stale = iso_offset_secs(-120);
        assert!(is_online(&just_now, now), "5s 前在线");
        assert!(!is_online(&stale, now), "120s 前离线");
        assert!(is_online(&iso_offset_secs(3), now), "轻微未来时间容忍在线");
        // 解析失败 / 空串 → 离线
        assert!(!is_online("not-a-time", now));
        assert!(!is_online("", now));
    }

    // L7. build_welcome_message 纯函数形状
    #[test]
    fn welcome_message_shape() {
        let m = build_welcome_message("dave");
        assert_eq!(m.conversation_id, "lobby");
        assert_eq!(m.sender_id, "system");
        assert_eq!(m.msg_type, "system");
        assert_eq!(m.content, "欢迎 dave 加入 NexOS 大厅");
        assert!(m.read_by.is_empty());
        assert!(!m.id.is_empty(), "id 为生成的 uuid");
    }

    // =========================================================================
    // 区块链认证（IM_BLOCKCHAIN_AUTH_DESIGN §2）单元测——真密钥对全流程
    // =========================================================================

    // A1. challenge：合法公钥 → 256-bit nonce + TTL + 展示名
    #[tokio::test]
    async fn auth_challenge_valid_pubkey() {
        let h = empty_handler();
        let sk = new_key();
        let pubkey = pubkey_hex(&sk);
        let resp = h
            .handle(post_req(
                PATH_AUTH_CHALLENGE,
                serde_json::json!({ "pubkey": pubkey }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let nonce = resp.body["nonce"].as_str().unwrap();
        assert_eq!(nonce.len(), 64, "256-bit hex");
        assert!(nonce.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(resp.body["expires_in"], IM_NONCE_TTL_SECS);
        let display = resp.body["display_name"].as_str().unwrap();
        assert!(
            display.starts_with("0x") && display.len() == 42,
            "EVM 地址 0x+40hex"
        );
    }

    // A2. challenge：公钥格式非法 → 400（缺 0x / 长度错 / 非 hex / 非法 sec1 点）
    #[tokio::test]
    async fn auth_challenge_rejects_invalid_pubkey() {
        let h = empty_handler();
        let valid = pubkey_hex(&new_key());
        for bad in [
            valid[2..].to_string(),             // 缺 0x 前缀
            format!("0x{}", &valid[2..66]),     // 长度不足（64 hex）
            format!("0x{}zz", &valid[2..66]),   // 非 hex 字符
            format!("0x04{}", "ab".repeat(32)), // 0x04 未压缩标签 + 33 字节 → sec1 解析失败
            "0x".to_string(),
            String::new(),
        ] {
            let resp = h
                .handle(post_req(
                    PATH_AUTH_CHALLENGE,
                    serde_json::json!({ "pubkey": bad }),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 400, "非法 pubkey 应 400: {bad}");
        }
    }

    // A3. verify：真密钥对全流程 challenge→sign→verify→token（24h + 展示名）
    #[tokio::test]
    async fn auth_verify_full_flow_issues_token() {
        let h = empty_handler();
        let sk = new_key();
        let (pubkey, token) = login(&h, &sk).await;
        assert_eq!(token.len(), 64, "256-bit hex token");
        // verify 响应带 expires_in=86400 + pubkey + 展示名（再走一次校验响应字段）
        let resp = h
            .handle(post_req(
                PATH_AUTH_CHALLENGE,
                serde_json::json!({ "pubkey": pubkey }),
            ))
            .await
            .unwrap();
        let nonce = resp.body["nonce"].as_str().unwrap().to_string();
        let sig = sign_nonce(&sk, &nonce);
        let resp = h
            .handle(post_req(
                PATH_AUTH_VERIFY,
                serde_json::json!({
                    "pubkey": pubkey,
                    "nonce": nonce,
                    "signature": hex::encode(sig),
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["expires_in"], IM_TOKEN_TTL_SECS);
        assert_eq!(resp.body["pubkey"], pubkey);
        assert!(resp.body["display_name"]
            .as_str()
            .unwrap()
            .starts_with("0x"));
    }

    // A4. nonce 重放拒绝（单次使用：verify 成功后同一 nonce 再验 → 401）
    #[tokio::test]
    async fn auth_verify_nonce_replay_rejected() {
        let h = empty_handler();
        let sk = new_key();
        let pubkey = pubkey_hex(&sk);
        let resp = h
            .handle(post_req(
                PATH_AUTH_CHALLENGE,
                serde_json::json!({ "pubkey": pubkey }),
            ))
            .await
            .unwrap();
        let nonce = resp.body["nonce"].as_str().unwrap().to_string();
        let sig = hex::encode(sign_nonce(&sk, &nonce));
        let first = h
            .handle(post_req(
                PATH_AUTH_VERIFY,
                serde_json::json!({ "pubkey": pubkey, "nonce": nonce, "signature": sig }),
            ))
            .await
            .unwrap();
        assert_eq!(first.status, 200, "首次 verify 应成功");
        let replay = h
            .handle(post_req(
                PATH_AUTH_VERIFY,
                serde_json::json!({ "pubkey": pubkey, "nonce": nonce, "signature": sig }),
            ))
            .await
            .unwrap();
        assert_eq!(replay.status, 401, "nonce 重放应 401（用后即焚）");
    }

    // A5. 未签发/不匹配的 nonce → 401
    #[tokio::test]
    async fn auth_verify_wrong_nonce_rejected() {
        let h = empty_handler();
        let sk = new_key();
        let pubkey = pubkey_hex(&sk);
        let sig = hex::encode(sign_nonce(&sk, "0".repeat(64).as_str()));
        let resp = h
            .handle(post_req(
                PATH_AUTH_VERIFY,
                serde_json::json!({
                    "pubkey": pubkey,
                    "nonce": "deadbeef",
                    "signature": sig,
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 401, "未知 nonce 应 401");
    }

    // A6. 伪造签名（另一把私钥签）→ 401
    #[tokio::test]
    async fn auth_verify_forged_signature_rejected() {
        let h = empty_handler();
        let sk = new_key();
        let attacker = new_key();
        let pubkey = pubkey_hex(&sk);
        let resp = h
            .handle(post_req(
                PATH_AUTH_CHALLENGE,
                serde_json::json!({ "pubkey": pubkey }),
            ))
            .await
            .unwrap();
        let nonce = resp.body["nonce"].as_str().unwrap().to_string();
        let forged = hex::encode(sign_nonce(&attacker, &nonce));
        let resp = h
            .handle(post_req(
                PATH_AUTH_VERIFY,
                serde_json::json!({ "pubkey": pubkey, "nonce": nonce, "signature": forged }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 401, "伪造签名应 401");
    }

    // A7. 签名格式非法（非 hex / 非 65 字节）→ 400
    #[tokio::test]
    async fn auth_verify_malformed_signature_rejected() {
        let h = empty_handler();
        let sk = new_key();
        let pubkey = pubkey_hex(&sk);
        let resp = h
            .handle(post_req(
                PATH_AUTH_CHALLENGE,
                serde_json::json!({ "pubkey": pubkey }),
            ))
            .await
            .unwrap();
        let nonce = resp.body["nonce"].as_str().unwrap().to_string();
        for bad_sig in [
            "zzzz".to_string(),
            hex::encode([0u8; 64]),
            hex::encode([0u8; 66]),
        ] {
            let resp = h
                .handle(post_req(
                    PATH_AUTH_VERIFY,
                    serde_json::json!({
                        "pubkey": pubkey,
                        "nonce": nonce,
                        "signature": bad_sig,
                    }),
                ))
                .await
                .unwrap();
            assert_eq!(
                resp.status,
                400,
                "签名格式非法应 400（len={}）",
                bad_sig.len()
            );
        }
    }

    // A8. 过期 token → 401
    #[tokio::test]
    async fn auth_expired_token_rejected() {
        let h = empty_handler();
        let (_, token) = login(&h, &new_key()).await;
        h.auth.expire_token_for_test(&token);
        let resp = h.handle(authed_get(PATH_CONV_LIST, &token)).await.unwrap();
        assert_eq!(resp.status, 401, "过期 token 应 401");
    }

    // A9. 单点登录：同 pubkey 二次 verify 顶掉旧 token
    #[tokio::test]
    async fn auth_single_login_replaces_old_token() {
        let h = empty_handler();
        let sk = new_key();
        let (_, old_token) = login(&h, &sk).await;
        // 旧 token 先确认可用
        let ok = h
            .handle(authed_get(PATH_CONV_LIST, &old_token))
            .await
            .unwrap();
        assert_eq!(ok.status, 200);
        // 同一密钥再登录 → 旧 token 失效、新 token 可用
        let (_, new_token) = login(&h, &sk).await;
        assert_ne!(old_token, new_token);
        let stale = h
            .handle(authed_get(PATH_CONV_LIST, &old_token))
            .await
            .unwrap();
        assert_eq!(stale.status, 401, "旧 token 应被顶掉（401）");
        let fresh = h
            .handle(authed_get(PATH_CONV_LIST, &new_token))
            .await
            .unwrap();
        assert_eq!(fresh.status, 200, "新 token 应可用");
    }

    // A10. REST 全端点强制 token：缺失 / 伪值 → 401（GET /status 与 POST /peers 除外）
    #[tokio::test]
    async fn auth_rest_endpoints_require_token() {
        let h = empty_handler();
        for (desc, req) in [
            ("GET /conversations", get_req(PATH_CONV_LIST)),
            ("GET /search", get_req(PATH_SEARCH)),
            ("GET /lobby", get_req(PATH_LOBBY)),
            (
                "POST /lobby/messages",
                post_req(PATH_LOBBY_MESSAGES, serde_json::json!({ "content": "hi" })),
            ),
            (
                "POST /conversations",
                post_req(PATH_CONV_LIST, serde_json::json!({ "name": "x" })),
            ),
            ("GET unread", get_req("/api/v1/im/conversations/c/unread")),
        ] {
            let resp = h.handle(req).await.unwrap();
            assert_eq!(resp.status, 401, "无 token 的 {desc} 应 401");
        }
        // 伪造 token 同样 401
        let forged = h
            .handle(authed_get(PATH_CONV_LIST, "0".repeat(64).as_str()))
            .await
            .unwrap();
        assert_eq!(forged.status, 401);
        // 公开端点不受影响：GET /status
        let status = h.handle(get_req(PATH_STATUS)).await.unwrap();
        assert_eq!(status.status, 200);
    }

    // A11. 请求体/查询参数伪造身份一律被服务端覆盖（join member / read user_id）
    #[tokio::test]
    async fn auth_identity_overrides_self_reported_fields() {
        let h = empty_handler();
        let (pubkey1, token1) = login(&h, &new_key()).await;
        let (pubkey2, token2) = login(&h, &new_key()).await;
        // 群组：body.member 自报 "admin" 应被忽略，实际入组的是 token pubkey
        let g = h
            .handle(authed_post(
                PATH_GROUPS,
                &token1,
                serde_json::json!({ "name": "g" }),
            ))
            .await
            .unwrap();
        let gid = g.body["id"].as_str().unwrap().to_string();
        let joined = h
            .handle(authed_post(
                &format!("/api/v1/im/groups/{gid}/join"),
                &token2,
                serde_json::json!({ "member": "admin" }),
            ))
            .await
            .unwrap();
        let members = joined.body["members"].as_array().unwrap();
        assert!(members.contains(&serde_json::json!(pubkey2)));
        assert!(!members.contains(&serde_json::json!("admin")));
        // leave：member = token pubkey（不是 body 自报）
        let left = h
            .handle(authed_post(
                &format!("/api/v1/im/groups/{gid}/leave"),
                &token2,
                serde_json::json!({ "member": pubkey1 }),
            ))
            .await
            .unwrap();
        let members = left.body["members"].as_array().unwrap();
        assert!(
            !members.contains(&serde_json::json!(pubkey2)),
            "token 本人应已退组"
        );
        assert!(
            members.contains(&serde_json::json!(pubkey1)),
            "自报他人不应被退组"
        );
    }

    // A12. REST 发消息：sender = token 反查 pubkey（全链路：建对话→发消息→历史）
    #[tokio::test]
    async fn auth_rest_send_message_sender_from_token() {
        let h = empty_handler();
        let (pubkey, token) = login(&h, &new_key()).await;
        let c = h
            .handle(authed_post(
                PATH_CONV_LIST,
                &token,
                serde_json::json!({ "name": "c" }),
            ))
            .await
            .unwrap();
        let cid = c.body["id"].as_str().unwrap().to_string();
        let sent = h
            .handle(authed_post(
                &format!("/api/v1/im/conversations/{cid}/messages"),
                &token,
                serde_json::json!({
                    "content": "来自链上身份",
                    "sender_id": "victim",
                    "sender_name": "受害者的名字",
                    "role": "admin",
                }),
            ))
            .await
            .unwrap();
        assert_eq!(sent.status, 201);
        assert_eq!(sent.body["sender_id"], pubkey);
        assert_eq!(
            sent.body["sender_name"],
            derive_display_name(&parse_im_pubkey(&pubkey).unwrap())
        );
        // 历史里的 sender 也归因到 pubkey
        let hist = h
            .handle(authed_get(
                &format!("/api/v1/im/conversations/{cid}/messages"),
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(hist.body[0]["sender_id"], pubkey);
        assert_eq!(
            hist.body[0]["sender_name"],
            derive_display_name(&parse_im_pubkey(&pubkey).unwrap())
        );
    }

    // A13. 展示名派生：公开测试向量（secp256k1 生成元 ↔ EVM 地址，私钥=1）
    #[test]
    fn auth_display_name_known_vector() {
        // 私钥 1 的公钥即生成元（压缩 0x0279be…）；其 EVM 地址是著名常量
        let vk =
            parse_im_pubkey("0x0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798")
                .expect("生成元公钥应可解析");
        assert_eq!(
            derive_display_name(&vk),
            "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf"
        );
        // 随机密钥往返：parse(derive 用的公钥) 恒成功、两次派生一致
        let sk = new_key();
        let vk2 = parse_im_pubkey(&pubkey_hex(&sk)).unwrap();
        assert_eq!(derive_display_name(&vk2), derive_display_name(&vk2));
    }

    // A14. ImAuth 桶语义：nonce 覆盖 / token 反查 / WS 匹配
    #[test]
    fn auth_store_bucket_semantics() {
        let auth = ImAuth::default();
        let pk = pubkey_hex(&new_key());
        // nonce 新 challenge 覆盖旧值
        let n1 = auth.create_nonce(&pk);
        let n2 = auth.create_nonce(&pk);
        assert_ne!(n1, n2);
        assert!(!auth.take_nonce(&pk, &n1), "旧 nonce 已被覆盖");
        assert!(auth.take_nonce(&pk, &n2), "最新 nonce 可用");
        assert!(!auth.take_nonce(&pk, &n2), "nonce 单次使用");
        // token 反查 + WS 匹配
        let (token, _) = auth.issue_token(&pk);
        assert_eq!(auth.verify_token(&token), Some(pk.clone()));
        assert_eq!(auth.verify_token("bogus"), None);
        assert!(auth.verify_ws(&pk, &token), "user 与 token 匹配");
        assert!(
            !auth.verify_ws(&pubkey_hex(&new_key()), &token),
            "user 不匹配应拒绝"
        );
    }

    // —— WebSocket 握手强制（真服务器 + tokio-tungstenite）——

    /// 启动带 IM 认证的网关（内存 im handler + 共享 ImAuth），返回
    /// `(网关, 绑定地址, 认证存储)`。端口取临时空闲端口。
    async fn start_im_gateway() -> (
        crate::gateway_impl::InProcessGateway,
        std::net::SocketAddr,
        Arc<ImAuth>,
    ) {
        use crate::gateway::Gateway;
        let gw = crate::gateway_impl::InProcessGateway::new();
        let auth = Arc::new(ImAuth::default());
        gw.register_component(
            "im",
            Box::new(ImRouteHandler::with_empty_ws(gw.ws_hub(), auth.clone())),
        )
        .await
        .expect("注册 im handler");
        gw.set_im_auth(Some(auth.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        gw.start(&format!("127.0.0.1:{}", addr.port()), None)
            .await
            .expect("start");
        (gw, addr, auth)
    }

    // A15. WS 握手：?user=<pubkey>&token=<token> 成功 + 广播可达
    //     （token 经真挑战-签名流程获取——challenge/verify 只碰共享 ImAuth，
    //      用同 auth 的探针 handler 走完整链路）
    #[tokio::test]
    async fn auth_ws_handshake_with_token_succeeds() {
        use futures::StreamExt;
        let (gw, addr, auth) = start_im_gateway().await;
        let probe = ImRouteHandler::with_empty_ws(gw.ws_hub(), auth.clone());
        let (pubkey, token) = login(&probe, &new_key()).await;

        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let url = format!(
            "ws://127.0.0.1:{}/ws?user={pubkey}&token={token}",
            addr.port()
        );
        let req = url.as_str().into_client_request().unwrap();
        let (mut ws_stream, _resp) = tokio_tungstenite::connect_async(req)
            .await
            .expect("带合法 token 的 WS 握手应成功");
        // 握手后该连接应已订阅（以 pubkey 身份），能收到广播
        let pushed = WsMessage::Event {
            event: os_core::Event::new("test", os_core::Topic::System, "im.auth.ws"),
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(gw.ws_hub().broadcast_n(pushed), 1, "应有 1 个订阅");
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), ws_stream.next())
            .await
            .expect("WS 收到不应超时")
            .expect("stream 不应结束")
            .expect("帧应无错");
        let text = frame.into_text().expect("应为文本帧");
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["type"], "event");
    }

    // A16. WS 握手：错误 token / user 不匹配 → 握手被拒（HTTP 401）
    #[tokio::test]
    async fn auth_ws_handshake_wrong_token_rejected() {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let (_gw, addr, auth) = start_im_gateway().await;
        let sk = new_key();
        let pubkey = pubkey_hex(&sk);
        let (token, _) = auth.issue_token(&pubkey);
        let base = format!("ws://127.0.0.1:{}/ws", addr.port());
        // 错误 token
        let bad = format!("{base}?user={pubkey}&token={}", "0".repeat(64));
        let req = bad.as_str().into_client_request().unwrap();
        assert!(
            tokio_tungstenite::connect_async(req).await.is_err(),
            "错误 token 握手应被拒"
        );
        // token 有效但 user 不匹配（冒充他人）
        let other = pubkey_hex(&new_key());
        let mismatch = format!("{base}?user={other}&token={token}");
        let req = mismatch.as_str().into_client_request().unwrap();
        assert!(
            tokio_tungstenite::connect_async(req).await.is_err(),
            "user 与 token 不匹配应被拒"
        );
    }

    // A17. WS 握手：裸 ?user= 无 token → 拒绝（一次性破坏性变更，无兼容通道）
    #[tokio::test]
    async fn auth_ws_handshake_no_token_rejected() {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let (_gw, addr, _auth) = start_im_gateway().await;
        let pubkey = pubkey_hex(&new_key());
        let url = format!("ws://127.0.0.1:{}/ws?user={pubkey}", addr.port());
        let req = url.as_str().into_client_request().unwrap();
        assert!(
            tokio_tungstenite::connect_async(req).await.is_err(),
            "裸 ?user= 不带 token 应被拒"
        );
        // 完全无参数同样拒绝
        let bare = format!("ws://127.0.0.1:{}/ws", addr.port());
        let req = bare.as_str().into_client_request().unwrap();
        assert!(
            tokio_tungstenite::connect_async(req).await.is_err(),
            "无 user/token 应被拒"
        );
    }

    // =========================================================================
    // 多 AI agent 接入 + 文档传输（2026-08-21）单元测
    // —— G 面：mentions/sender_kind/助手闭环；F 面：附件上传下载/核对
    // =========================================================================

    use std::sync::Mutex as StdMutex;

    /// 测试用防风暴窗口（默认 3s 太慢；3 条连发 POST ≪ 200ms，去抖语义稳定）。
    const TEST_STORM_WINDOW: Duration = Duration::from_millis(200);

    /// 假 LLM 服务器（本地 TcpListener 手写 HTTP/1.1）：echo 模式回
    /// `ECHO:<user prompt>`（同时证明请求形状与"除 @ 外文本"剥取）；
    /// 捕获原始请求体到 `seen` 供断言（模型名/prompt）。返回完整 endpoint url。
    async fn spawn_fake_llm(
        seen: Arc<StdMutex<Vec<String>>>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            for _ in 0..4 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut acc: Vec<u8> = Vec::new();
                let mut buf = [0u8; 16384];
                // 读全：按 Content-Length 判断请求体收完
                loop {
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    acc.extend_from_slice(&buf[..n]);
                    let head = String::from_utf8_lossy(&acc).into_owned();
                    if let Some(pos) = head.find("\r\n\r\n") {
                        let cl = head[..pos]
                            .lines()
                            .find(|l| l.to_ascii_lowercase().starts_with("content-length"))
                            .and_then(|l| l.split(':').nth(1))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if acc.len() >= pos + 4 + cl {
                            break;
                        }
                    }
                }
                let req_text = String::from_utf8_lossy(&acc).into_owned();
                // echo：取 messages[1].content（user 消息）
                let user_prompt = serde_json::from_str::<serde_json::Value>(
                    req_text.split("\r\n\r\n").nth(1).unwrap_or(""),
                )
                .ok()
                .and_then(|v| v["messages"][1]["content"].as_str().map(str::to_string))
                .unwrap_or_default();
                seen.lock().unwrap().push(req_text);
                let body = serde_json::json!({
                    "choices": [{"message": {"content": format!("ECHO:{user_prompt}")}}]
                });
                let body = body.to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        (format!("http://{addr}/v1/chat/completions"), task)
    }

    /// 必然连接失败的推理端点（127.0.0.1:1 无服务 → 秒级 ECONNREFUSED，
    /// 走"LLM 不可达降级"路径）。
    const DEAD_LLM_URL: &str = "http://127.0.0.1:1/v1/chat/completions";

    /// 等待条件成立（25ms 轮询；超时返回最后一次求值）。
    async fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if f() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        f()
    }

    /// 某会话里的助手回复（按合成 sender_id 判定——外部 agent 自声明
    /// sender_kind=agent 的用户消息不算助手回复）。
    fn agent_replies(h: &ImRouteHandler, cid: &str) -> Vec<Message> {
        h.messages_snapshot()
            .into_iter()
            .filter(|m| m.conversation_id == cid && m.sender_id == ASSISTANT_SENDER_ID)
            .collect()
    }

    /// 独立临时附件根目录（测试隔离；返回 (路径字符串, PathBuf)）。
    fn temp_files_root(tag: &str) -> (String, PathBuf) {
        let dir = std::env::temp_dir().join(format!("nexos-im-test-{tag}-{}", new_uuid()));
        (dir.to_string_lossy().into_owned(), dir)
    }

    // G1. mentions 解析：中文/英文/多 @/混合
    #[test]
    fn mentions_parse_chinese_english_multi() {
        assert_eq!(
            parse_mentions("你好 @NexOS助手 请看 @alice 的稿"),
            vec!["NexOS助手", "alice"]
        );
        assert_eq!(parse_mentions("@bob-dev_1 收到没"), vec!["bob-dev_1"]);
        assert_eq!(parse_mentions("@甲 @乙 @甲"), vec!["甲", "乙"], "去重保序");
        assert_eq!(parse_mentions("没有提及"), Vec::<String>::new());
        assert_eq!(parse_mentions(""), Vec::<String>::new());
    }

    // G2. mentions 边界：裸 @/@@/邮箱式/超长截断/标点截断
    #[test]
    fn mentions_parse_edges() {
        assert_eq!(parse_mentions("@"), Vec::<String>::new(), "裸 @ 不算");
        assert_eq!(
            parse_mentions("@ 你好"),
            Vec::<String>::new(),
            "@ 后空格不算"
        );
        assert_eq!(
            parse_mentions("@@NexOS助手"),
            vec!["NexOS助手"],
            "首个 @ 后跟 @ 非法 → 第二个 @ 起解析"
        );
        // 邮箱式：名字到非法字符（.）为止——既定语义
        assert_eq!(parse_mentions("mail me a@b.example.com"), vec!["b"]);
        // 超过 42 字符截断到 42（EVM 地址 0x+40hex 需完整容纳）
        let long = "字".repeat(50);
        let parsed = parse_mentions(&format!("@{long}"));
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].chars().count(), 42, "名字截断到 42 字符");
        // EVM 地址（0x+40hex = 42 字符）完整解析不截断
        let evm = "0xe4461efaca05117631277fbb3c7f7e40e01179fc";
        assert_eq!(parse_mentions(&format!("@{evm}")), vec![evm]);
        // 标点截断：中文顿号后停止
        assert_eq!(parse_mentions("@张三、@李四"), vec!["张三", "李四"]);
        // strip_mentions：剥 @ 片段、全剥空
        assert_eq!(
            strip_mentions("@NexOS助手 帮我总结这份文档", &["NexOS助手".to_string()]),
            "帮我总结这份文档"
        );
        assert_eq!(strip_mentions("@NexOS助手", &["NexOS助手".to_string()]), "");
    }

    // G3. sender_kind 兼容：缺省 human；agent 放行；垃圾值归一 human
    #[tokio::test]
    async fn sender_kind_default_agent_and_garbage_normalized() {
        let h = empty_handler();
        let (_, token) = login(&h, &new_key()).await;
        let c = h
            .handle(authed_post(
                PATH_CONV_LIST,
                &token,
                serde_json::json!({ "name": "c" }),
            ))
            .await
            .unwrap();
        let cid = c.body["id"].as_str().unwrap().to_string();
        let base = format!("/api/v1/im/conversations/{cid}/messages");
        // 缺省 → human（存量客户端零迁移）
        let r1 = h
            .handle(authed_post(
                &base,
                &token,
                serde_json::json!({ "content": "hi" }),
            ))
            .await
            .unwrap();
        assert_eq!(r1.body["sender_kind"], "human", "缺省应为 human");
        // agent 放行（外部 agent 自声明）
        let r2 = h
            .handle(authed_post(
                &base,
                &token,
                serde_json::json!({ "content": "agent 说", "sender_kind": "agent" }),
            ))
            .await
            .unwrap();
        assert_eq!(r2.body["sender_kind"], "agent");
        // 垃圾值归一 human（白名单）
        let r3 = h
            .handle(authed_post(
                &base,
                &token,
                serde_json::json!({ "content": "x", "sender_kind": "robot" }),
            ))
            .await
            .unwrap();
        assert_eq!(r3.body["sender_kind"], "human", "白名单外归一 human");
    }

    // G4. mentions + sender_kind 全链路往返：响应/历史/补拉三处一致
    #[tokio::test]
    async fn sender_kind_mentions_roundtrip_history_and_catchup() {
        let h = empty_handler();
        let (_, token) = login(&h, &new_key()).await;
        let c = h
            .handle(authed_post(
                PATH_CONV_LIST,
                &token,
                serde_json::json!({ "name": "c" }),
            ))
            .await
            .unwrap();
        let cid = c.body["id"].as_str().unwrap().to_string();
        let sent = h
            .handle(authed_post(
                &format!("/api/v1/im/conversations/{cid}/messages"),
                &token,
                serde_json::json!({
                    "content": "@NexOS助手 @alice 帮我把这页做成 PPT",
                    "sender_kind": "agent"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(sent.status, 201);
        let m = &sent.body;
        assert_eq!(m["sender_kind"], "agent");
        assert_eq!(
            m["mentions"],
            serde_json::json!(["NexOS助手", "alice"]),
            "服务端应解析 mentions"
        );
        assert_eq!(m["attachment"], serde_json::Value::Null);
        // 历史
        let hist = h
            .handle(authed_get(
                &format!("/api/v1/im/conversations/{cid}/messages"),
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(hist.body[0]["sender_kind"], "agent");
        assert_eq!(hist.body[0]["mentions"][0], "NexOS助手");
        // 补拉
        let catchup = h
            .handle(authed_get(
                &format!("/api/v1/im/messages?conversation_id={cid}"),
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(catchup.body[0]["sender_kind"], "agent");
        assert_eq!(catchup.body[0]["mentions"][1], "alice");
        // 大厅同语义
        let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
        let lobby = h
            .handle(authed_post(
                PATH_LOBBY_MESSAGES,
                &token,
                serde_json::json!({ "content": "大厅 @张三 到了吗" }),
            ))
            .await
            .unwrap();
        assert_eq!(lobby.body["sender_kind"], "human");
        assert_eq!(lobby.body["mentions"], serde_json::json!(["张三"]));
    }

    // G5. 存量行兼容：老列集插入（缺三新列）读回默认 human/[]
    #[tokio::test]
    async fn legacy_rows_default_to_human() {
        let h = demo_handler();
        {
            let conn = h.shared.db.lock().expect("db poisoned");
            conn.execute(
                "INSERT INTO im_messages (id,conversation_id,sender_id,sender_name,content,msg_type,created_at,read_by)
                 VALUES ('legacy-1','conv-general','alice','Alice','旧消息','text','2026-01-02T09:00:00+08:00','[]')",
                [],
            )
            .unwrap();
        }
        let (_, token) = login(&h, &new_key()).await;
        let resp = h
            .handle(authed_get(
                "/api/v1/im/conversations/conv-general/messages",
                &token,
            ))
            .await
            .unwrap();
        let legacy = resp
            .body
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == "legacy-1")
            .expect("legacy 行应可见");
        assert_eq!(legacy["sender_kind"], "human", "旧行默认 human");
        assert_eq!(legacy["mentions"], serde_json::json!([]));
        assert_eq!(legacy["attachment"], serde_json::Value::Null);
    }

    // G6. 助手触发（大厅，假 LLM echo）：回复字段全形状 + prompt 剥 @ + 模型名
    #[tokio::test]
    async fn assistant_lobby_reply_full_shape_with_fake_llm() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let (url, _srv) = spawn_fake_llm(seen.clone()).await;
        let h = empty_handler()
            .with_agent_llm_url(&url)
            .with_agent_model("test-model")
            .with_agent_storm_window(TEST_STORM_WINDOW);
        let (_, token) = login(&h, &new_key()).await;
        let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
        let sent = h
            .handle(authed_post(
                PATH_LOBBY_MESSAGES,
                &token,
                serde_json::json!({ "content": "@NexOS助手 帮我总结这份季度报告" }),
            ))
            .await
            .unwrap();
        assert_eq!(sent.status, 201);
        let trigger_id = sent.body["id"].as_str().unwrap().to_string();
        // 等助手回复落库（窗口 200ms + 本地 echo 秒回）
        let ok = wait_until(Duration::from_secs(5), || {
            !agent_replies(&h, LOBBY_ID).is_empty()
        })
        .await;
        assert!(ok, "助手应在窗口后回复大厅");
        let replies = agent_replies(&h, LOBBY_ID);
        assert_eq!(replies.len(), 1, "单次 @ 应恰有一条回复");
        let r = &replies[0];
        assert_eq!(r.sender_kind, "agent");
        assert_eq!(r.sender_name.as_deref(), Some("NexOS助手"));
        assert_eq!(r.sender_id, "agent:nexos-assistant");
        assert_eq!(r.reply_to.as_deref(), Some(trigger_id.as_str()));
        assert_eq!(
            r.content, "ECHO:帮我总结这份季度报告（AI 生成）",
            "prompt 应剥掉 @，回显带后缀"
        );
        // 请求形状：模型名 + system/user 双消息
        let reqs = seen.lock().unwrap();
        assert_eq!(reqs.len(), 1, "只有最后一次触发真调了 LLM");
        assert!(
            reqs[0].contains("\"model\":\"test-model\""),
            "模型名应透传: {}",
            reqs[0]
        );
        assert!(reqs[0].contains("帮我总结这份季度报告"));
        assert!(!reqs[0].contains("@NexOS助手"), "prompt 不应含 @ 片段");
    }

    // G7. 会话内 @ 同样生效（LLM 不可达 → 固定降级话术）
    #[tokio::test]
    async fn assistant_conversation_trigger_llm_unreachable_fallback() {
        let h = empty_handler()
            .with_agent_llm_url(DEAD_LLM_URL)
            .with_agent_storm_window(TEST_STORM_WINDOW);
        let (_, token) = login(&h, &new_key()).await;
        let c = h
            .handle(authed_post(
                PATH_CONV_LIST,
                &token,
                serde_json::json!({ "name": "带助手" }),
            ))
            .await
            .unwrap();
        let cid = c.body["id"].as_str().unwrap().to_string();
        let sent = h
            .handle(authed_post(
                &format!("/api/v1/im/conversations/{cid}/messages"),
                &token,
                serde_json::json!({ "content": "@NexOS助手 列三个要点" }),
            ))
            .await
            .unwrap();
        assert_eq!(sent.status, 201);
        let ok = wait_until(Duration::from_secs(5), || {
            !agent_replies(&h, &cid).is_empty()
        })
        .await;
        assert!(ok, "LLM 不可达也应降级回复");
        let replies = agent_replies(&h, &cid);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].conversation_id, cid, "回复落原会话");
        assert_eq!(
            replies[0].content,
            format!("{ASSISTANT_FALLBACK_TEXT}{ASSISTANT_SUFFIX}"),
            "不可达 → 固定话术 + 后缀"
        );
    }

    // G8. 触发/不触发矩阵：无 @、@他人、agent 消息 → 一律不回复
    #[tokio::test]
    async fn assistant_no_trigger_matrix() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let (url, _srv) = spawn_fake_llm(seen.clone()).await;
        let h = empty_handler()
            .with_agent_llm_url(&url)
            .with_agent_storm_window(TEST_STORM_WINDOW);
        let (_, token) = login(&h, &new_key()).await;
        let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
        for (content, kind) in [
            ("普通消息，谁也不@", None),
            ("@alice 帮我个忙", None),                    // @ 他人不触发
            ("@NexOS助手 agent 不应触发", Some("agent")), // agent 消息跳过（防自激）
        ] {
            let mut body = serde_json::json!({ "content": content });
            if let Some(k) = kind {
                body["sender_kind"] = serde_json::json!(k);
            }
            let r = h
                .handle(authed_post(PATH_LOBBY_MESSAGES, &token, body))
                .await
                .unwrap();
            assert_eq!(r.status, 201);
        }
        // 等满 3× 窗口 + 余量 → 仍应零回复、零 LLM 请求
        tokio::time::sleep(TEST_STORM_WINDOW * 4).await;
        assert!(
            agent_replies(&h, LOBBY_ID).is_empty(),
            "无 @ / @他人 / agent 消息都不应触发助手"
        );
        assert!(seen.lock().unwrap().is_empty(), "不应发生任何 LLM 请求");
    }

    // G9. 防风暴：3s（测试 200ms）窗口内 3 条 @ 只响应最后一条
    #[tokio::test]
    async fn assistant_storm_window_last_trigger_wins() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let (url, _srv) = spawn_fake_llm(seen.clone()).await;
        let h = empty_handler()
            .with_agent_llm_url(&url)
            .with_agent_storm_window(TEST_STORM_WINDOW);
        let (_, token) = login(&h, &new_key()).await;
        let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
        for i in 1..=3 {
            let r = h
                .handle(authed_post(
                    PATH_LOBBY_MESSAGES,
                    &token,
                    serde_json::json!({ "content": format!("@NexOS助手 问{i}") }),
                ))
                .await
                .unwrap();
            assert_eq!(r.status, 201);
        }
        // 恰一条回复，且回复内容对应最后一条（问3）
        let ok = wait_until(Duration::from_secs(5), || {
            agent_replies(&h, LOBBY_ID).len() == 1
        })
        .await;
        assert!(ok, "防风暴后应恰有一条回复");
        tokio::time::sleep(TEST_STORM_WINDOW * 2).await;
        let replies = agent_replies(&h, LOBBY_ID);
        assert_eq!(replies.len(), 1, "窗口内多条 @ 只响应最后一条");
        assert_eq!(replies[0].content, "ECHO:问3（AI 生成）");
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "旧代次任务应被去抖放弃，不调 LLM"
        );
    }

    // G10. 超长回复截断（≤800 字 + 后缀），UTF-8 边界安全
    #[tokio::test]
    async fn assistant_reply_truncated_to_800_chars() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let (url, _srv) = spawn_fake_llm(seen.clone()).await;
        let h = empty_handler()
            .with_agent_llm_url(&url)
            .with_agent_storm_window(TEST_STORM_WINDOW);
        let (_, token) = login(&h, &new_key()).await;
        let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
        // echo 服务器回 5 + 900 = 905 字符 → 截到 800 + 后缀 7 = 807
        let r = h
            .handle(authed_post(
                PATH_LOBBY_MESSAGES,
                &token,
                serde_json::json!({ "content": format!("@NexOS助手 {}", "长".repeat(900)) }),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201);
        let ok = wait_until(Duration::from_secs(5), || {
            !agent_replies(&h, LOBBY_ID).is_empty()
        })
        .await;
        assert!(ok);
        let reply = &agent_replies(&h, LOBBY_ID)[0];
        assert!(reply.content.starts_with("ECHO:长长"));
        assert!(reply.content.ends_with(ASSISTANT_SUFFIX));
        assert_eq!(
            reply.content.chars().count(),
            ASSISTANT_REPLY_MAX_CHARS + ASSISTANT_SUFFIX.chars().count(),
            "正文截到 800 字 + 后缀"
        );
        assert!(!seen.lock().unwrap().is_empty());
    }

    // G11. 纯函数：truncate_chars UTF-8 边界安全 + normalize_sender_kind
    #[test]
    fn truncate_and_normalize_pure() {
        assert_eq!(truncate_chars("abcd", 3), "abc");
        assert_eq!(truncate_chars("中文安全", 2), "中文");
        assert_eq!(truncate_chars("短", 10), "短");
        assert_eq!(truncate_chars("😀😀😀", 2), "😀😀", "emoji 按字符截断");
        assert_eq!(normalize_sender_kind(None), "human");
        assert_eq!(normalize_sender_kind(Some("human")), "human");
        assert_eq!(normalize_sender_kind(Some("agent")), "agent");
        assert_eq!(normalize_sender_kind(Some(" agent ")), "agent", "trim 宽容");
        assert_eq!(
            normalize_sender_kind(Some("ADMIN")),
            "human",
            "白名单外归一"
        );
    }

    // —— F 面：文档传输 ——（上传落盘/超限/净化/下载鉴权/attachment 核对/往返）

    /// 上传小工具：登录 → POST /im/files → (file_id, token)。
    async fn upload(
        h: &ImRouteHandler,
        token: &str,
        filename: &str,
        bytes: &[u8],
    ) -> serde_json::Value {
        let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        h.handle(authed_post(
            PATH_FILES,
            token,
            serde_json::json!({ "filename": filename, "content_base64": b64 }),
        ))
        .await
        .unwrap()
        .body
    }

    // F1. 上传 → 落盘路径形状（月目录 + uuid 前缀 + 净化名）→ 下载往返
    #[tokio::test]
    async fn imfile_upload_roundtrip_disk_shape_and_download() {
        let (root_str, root) = temp_files_root("rt");
        let h = empty_handler().with_files_root(&root_str);
        let (_, token) = login(&h, &new_key()).await;
        let payload = b"nexos im file payload \xe4\xb8\xad\xe6\x96\x87".to_vec();
        let up = upload(&h, &token, "季度报告.pptx", &payload).await;
        assert_eq!(up["size_bytes"], payload.len() as u64);
        assert_eq!(
            up["mime"],
            "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        );
        let file_id = up["file_id"].as_str().unwrap().to_string();
        // 落盘形状：<root>/<YYYYMM>/<uuid>-季度报告.pptx
        let month = chrono::Local::now().format("%Y%m").to_string();
        let dir = root.join(&month);
        let entries: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        assert_eq!(entries.len(), 1);
        let stored = entries[0].to_string_lossy().into_owned();
        assert!(
            stored.contains(&format!("{file_id}-季度报告.pptx")),
            "落盘名 = uuid-净化名: {stored}"
        );
        assert_eq!(
            std::fs::metadata(&entries[0]).unwrap().len(),
            payload.len() as u64
        );
        // url 形状：/api/v1/im/files/<id>?token=<token>
        assert_eq!(
            up["url"],
            format!("/api/v1/im/files/{file_id}?token={token}")
        );
        // Bearer 下载往返：base64 信封 + content-disposition
        let dl = h
            .handle(authed_get(&format!("/api/v1/im/files/{file_id}"), &token))
            .await
            .unwrap();
        assert_eq!(dl.status, 200);
        assert_eq!(dl.body["filename"], "季度报告.pptx");
        assert_eq!(dl.body["encoding"], "base64");
        assert_eq!(dl.body["size_bytes"], payload.len() as u64);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(dl.body["content_base64"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, payload, "下载内容逐字节一致");
        let cd = dl.headers["content-disposition"].as_str().unwrap();
        assert!(cd.starts_with("attachment; filename=\""));
        assert!(cd.contains("filename*=UTF-8''"), "RFC 5987: {cd}");
        let _ = std::fs::remove_dir_all(&root);
    }

    // F2. 超限 413（base64 长度前置估算，不实际解码）+ store_im_file 解码后闸门
    #[tokio::test]
    async fn imfile_upload_rejects_oversize() {
        let (root_str, root) = temp_files_root("over");
        let h = empty_handler().with_files_root(&root_str);
        let (_, token) = login(&h, &new_key()).await;
        // 长度使 len/4*3 恰超 64MiB（约 86MiB 字符串——只测闸门不解码）
        let huge_b64 = "A".repeat(IM_FILE_MAX_BYTES / 3 * 4 + 16);
        let resp = h
            .handle(authed_post(
                PATH_FILES,
                &token,
                serde_json::json!({ "filename": "huge.bin", "content_base64": huge_b64 }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 413, "前置估算即拒");
        // 解码后闸门（spawn_blocking 路径的兜底；API 面先被前置拦截）
        let big = vec![b'x'; IM_FILE_MAX_BYTES + 1];
        let err = store_im_file(&root, "x.bin", &big).unwrap_err();
        assert_eq!(err.0, 413);
        // 缺字段 / 坏 base64 → 400
        let miss = h
            .handle(authed_post(
                PATH_FILES,
                &token,
                serde_json::json!({ "filename": "a" }),
            ))
            .await
            .unwrap();
        assert_eq!(miss.status, 400);
        let bad = h
            .handle(authed_post(
                PATH_FILES,
                &token,
                serde_json::json!({ "filename": "a.txt", "content_base64": "@@@not-base64@@@" }),
            ))
            .await
            .unwrap();
        assert_eq!(bad.status, 400);
        // 无 token → 401
        let anon = h
            .handle(post_req(
                PATH_FILES,
                serde_json::json!({ "filename": "a", "content_base64": "YQ==" }),
            ))
            .await
            .unwrap();
        assert_eq!(anon.status, 401);
        let _ = std::fs::remove_dir_all(&root);
    }

    // F3. 文件名净化：路径分隔/穿越/控制字符 → `_`；全非法回退 file
    #[tokio::test]
    async fn imfile_filename_sanitization() {
        assert_eq!(sanitize_im_filename("../evil/x.sh"), ".._evil_x.sh");
        assert_eq!(sanitize_im_filename("a\\b\\c.txt"), "a_b_c.txt");
        assert_eq!(
            sanitize_im_filename("报告 最终版(1).pptx"),
            "报告 最终版(1).pptx"
        );
        assert_eq!(sanitize_im_filename("  \n\t  "), "file", "全非法/空白回退");
        assert_eq!(sanitize_im_filename(""), "file");
        let long = "a".repeat(300);
        assert_eq!(
            sanitize_im_filename(&long).chars().count(),
            120,
            "截到 120 字符"
        );
        // 端到端：恶意名落盘后无路径穿越
        let (root_str, root) = temp_files_root("san");
        let h = empty_handler().with_files_root(&root_str);
        let (_, token) = login(&h, &new_key()).await;
        let up = upload(&h, &token, "../../etc/passwd", b"x").await;
        let file_id = up["file_id"].as_str().unwrap().to_string();
        assert_eq!(up["filename"], ".._.._etc_passwd", "净化后展示名");
        let month = chrono::Local::now().format("%Y%m").to_string();
        let dir = root.join(&month);
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names
                .iter()
                .any(|n| n.contains(&file_id) && n.contains(".._.._etc_passwd")),
            "落盘单段名: {names:?}"
        );
        // root 外无泄漏
        assert!(!root.parent().unwrap().join("etc").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    // F4. 下载鉴权矩阵：无/坏 token 401；query IM token / Bearer / admin token 200；
    //     未知 id 404
    #[tokio::test]
    async fn imfile_download_auth_matrix() {
        let (root_str, root) = temp_files_root("auth");
        let h = empty_handler()
            .with_files_root(&root_str)
            .with_admin_token("adm-tk-1");
        let (_, token) = login(&h, &new_key()).await;
        let up = upload(&h, &token, "a.txt", b"hello").await;
        let file_id = up["file_id"].as_str().unwrap().to_string();
        // 无 token → 401
        let anon = h
            .handle(get_req(&format!("/api/v1/im/files/{file_id}")))
            .await
            .unwrap();
        assert_eq!(anon.status, 401);
        // 坏 token → 401
        let bad = h
            .handle(get_req(&format!(
                "/api/v1/im/files/{file_id}?token={}",
                "0".repeat(64)
            )))
            .await
            .unwrap();
        assert_eq!(bad.status, 401);
        // query IM token（直链场景）→ 200
        let q = h
            .handle(get_req(&format!(
                "/api/v1/im/files/{file_id}?token={token}"
            )))
            .await
            .unwrap();
        assert_eq!(q.status, 200);
        assert_eq!(q.body["filename"], "a.txt");
        // Bearer 头 → 200
        let b = h
            .handle(authed_get(&format!("/api/v1/im/files/{file_id}"), &token))
            .await
            .unwrap();
        assert_eq!(b.status, 200);
        // admin token → 200
        let adm = h
            .handle(get_req(&format!(
                "/api/v1/im/files/{file_id}?token=adm-tk-1"
            )))
            .await
            .unwrap();
        assert_eq!(adm.status, 200);
        // 未知 id → 404
        let miss = h
            .handle(get_req("/api/v1/im/files/no-such-id"))
            .await
            .unwrap();
        assert_eq!(miss.status, 404);
        let _ = std::fs::remove_dir_all(&root);
    }

    // F5. attachment 核对：伪造 size/filename 被服务端真值覆盖；未知 file_id 400
    #[tokio::test]
    async fn attachment_verified_against_server_truth() {
        let (root_str, root) = temp_files_root("att");
        let h = empty_handler().with_files_root(&root_str);
        let (_, token) = login(&h, &new_key()).await;
        let payload = b"0123456789".to_vec(); // 10 字节
        let up = upload(&h, &token, "真名.docx", &payload).await;
        let file_id = up["file_id"].as_str().unwrap().to_string();
        let c = h
            .handle(authed_post(
                PATH_CONV_LIST,
                &token,
                serde_json::json!({ "name": "c" }),
            ))
            .await
            .unwrap();
        let cid = c.body["id"].as_str().unwrap().to_string();
        // 伪造 size_bytes=1 / filename="forged" → 服务端覆盖为真值
        let sent = h
            .handle(authed_post(
                &format!("/api/v1/im/conversations/{cid}/messages"),
                &token,
                serde_json::json!({
                    "content": "见附件",
                    "attachment": {
                        "file_id": file_id,
                        "filename": "forged.exe",
                        "size_bytes": 1
                    }
                }),
            ))
            .await
            .unwrap();
        assert_eq!(sent.status, 201);
        assert_eq!(sent.body["attachment"]["file_id"], file_id);
        assert_eq!(
            sent.body["attachment"]["filename"], "真名.docx",
            "伪造文件名被覆盖"
        );
        assert_eq!(
            sent.body["attachment"]["size_bytes"], 10,
            "伪造 size 被覆盖"
        );
        assert_eq!(
            sent.body["attachment"]["mime"],
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        );
        // 历史与补拉同真值
        let hist = h
            .handle(authed_get(
                &format!("/api/v1/im/conversations/{cid}/messages"),
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(hist.body[0]["attachment"]["size_bytes"], 10);
        let catchup = h
            .handle(authed_get(
                &format!("/api/v1/im/messages?conversation_id={cid}"),
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(catchup.body[0]["attachment"]["filename"], "真名.docx");
        // 未知 file_id → 400
        let bad = h
            .handle(authed_post(
                &format!("/api/v1/im/conversations/{cid}/messages"),
                &token,
                serde_json::json!({
                    "content": "坏附件",
                    "attachment": { "file_id": "no-such", "size_bytes": 99 }
                }),
            ))
            .await
            .unwrap();
        assert_eq!(bad.status, 400, "未知附件应 400");
        assert!(bad.body["error"].as_str().unwrap().contains("不存在"));
        let _ = std::fs::remove_dir_all(&root);
    }

    // F6. WS 广播帧携带新字段：sender_kind / mentions / attachment（大厅帧）
    #[tokio::test]
    async fn ws_frame_carries_agent_fields_and_attachment() {
        let (root_str, root) = temp_files_root("ws");
        let hub = WsHub::default();
        let h = ImRouteHandler::with_empty_ws(hub.clone(), Arc::new(ImAuth::default()))
            .with_files_root(&root_str);
        let (_, token) = login(&h, &new_key()).await;
        let up = upload(&h, &token, "slides.pptx", b"pptx-bytes").await;
        let file_id = up["file_id"].as_str().unwrap().to_string();
        let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
        // GET /lobby 已触发欢迎帧广播——之后才订阅，首帧即我们的消息
        let (_sub, mut rx) = hub.subscribe_raw("probe");
        // 大厅消息（避开 @NexOS助手 免触发助手；@alice 仅验证 mentions 透传）
        let sent = h
            .handle(authed_post(
                PATH_LOBBY_MESSAGES,
                &token,
                serde_json::json!({
                    "content": "@alice 请看附件",
                    "sender_kind": "agent",
                    "attachment": { "file_id": file_id, "size_bytes": 999 }
                }),
            ))
            .await
            .unwrap();
        assert_eq!(sent.status, 201);
        let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("WS 帧不应超时")
            .expect("通道不应关闭");
        let v = serde_json::to_value(&frame).unwrap();
        assert_eq!(v["type"], "im_lobby_message");
        assert_eq!(v["lobby_id"], "lobby");
        assert_eq!(v["message"]["sender_kind"], "agent", "帧透传 sender_kind");
        assert_eq!(v["message"]["mentions"], serde_json::json!(["alice"]));
        assert_eq!(
            v["message"]["attachment"]["size_bytes"], 10,
            "帧内附件为服务端真值（伪造 999 被覆盖）"
        );
        assert_eq!(v["message"]["attachment"]["file_id"], file_id);
        assert_eq!(v["message"]["attachment"]["filename"], "slides.pptx");
        let _ = std::fs::remove_dir_all(&root);
    }

    // F7. mime 猜测 + Content-Disposition 纯函数
    #[test]
    fn imfile_mime_and_disposition_pure() {
        assert_eq!(
            guess_mime_im("a.pptx"),
            "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        );
        assert_eq!(guess_mime_im("a.pdf"), "application/pdf");
        assert_eq!(guess_mime_im("noext"), "application/octet-stream");
        let cd = content_disposition_im("中文 名.pptx");
        assert!(cd.contains("filename=\"______.pptx\"") || cd.contains("filename=\""));
        assert!(cd.contains("filename*=UTF-8''"), "RFC 5987 编码段: {cd}");
        assert!(cd.contains("%E4%B8%AD"), "中文按 UTF-8 百分号编码: {cd}");
    }

    // =========================================================================
    // 消息推送通知 webhook（2026-08-22）单元测 —— N 面
    // —— 注册归因/owner 过滤/注销权限/大厅与会话触发/事件过滤/超时不阻塞/
    //    连败自动注销/无 token 泄漏/纯函数
    // =========================================================================

    /// 假 webhook 接收端应答模式：Ok（回 200）/ Hang（收下不回——客户端等满
    /// 5s 超时，用于验证消息路径不被阻塞）。
    #[derive(Clone, Copy)]
    enum FakeHookMode {
        Ok,
        Hang,
    }

    /// 假 webhook 接收端（本地 TcpListener 手写 HTTP/1.1，spawn_fake_llm 同款
    /// 手法）：逐请求把**原始请求文本**（请求行+headers+body）记进 seen，
    /// 按 mode 应答；至多服务 8 个连接。返回完整接收端点 url。
    async fn spawn_webhook_receiver(
        seen: Arc<StdMutex<Vec<String>>>,
        mode: FakeHookMode,
    ) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..8 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut acc: Vec<u8> = Vec::new();
                let mut buf = [0u8; 16384];
                // 按 Content-Length 判断请求体收完
                loop {
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    acc.extend_from_slice(&buf[..n]);
                    let head = String::from_utf8_lossy(&acc).into_owned();
                    if let Some(pos) = head.find("\r\n\r\n") {
                        let cl = head[..pos]
                            .lines()
                            .find(|l| l.to_ascii_lowercase().starts_with("content-length"))
                            .and_then(|l| l.split(':').nth(1))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if acc.len() >= pos + 4 + cl {
                            break;
                        }
                    }
                }
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&acc).into_owned());
                match mode {
                    FakeHookMode::Ok => {
                        let resp =
                            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
                        let _ = sock.write_all(resp.as_bytes()).await;
                    }
                    FakeHookMode::Hang => {
                        // 收下不回：让客户端等满 5s 超时（测试只关心发消息端
                        // 不被阻塞；测试结束 runtime 回收本任务）
                        tokio::time::sleep(Duration::from_secs(600)).await;
                    }
                }
            }
        });
        format!("http://{addr}/agent-hook")
    }

    /// 带 IM token 的 DELETE。
    fn authed_delete(path: &str, token: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Delete,
            path: path.into(),
            headers: serde_json::json!({"authorization": format!("Bearer {token}")}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    /// 注册小工具：POST /notify/register，返回响应。
    async fn notify_register(
        h: &ImRouteHandler,
        token: &str,
        url: &str,
        events: Option<Vec<&str>>,
        conversation_id: Option<&str>,
    ) -> ApiResponse {
        let mut body = serde_json::json!({ "url": url });
        if let Some(ev) = events {
            body["events"] = serde_json::json!(ev);
        }
        if let Some(cid) = conversation_id {
            body["conversation_id"] = serde_json::json!(cid);
        }
        h.handle(authed_post(PATH_NOTIFY_REGISTER, token, body))
            .await
            .unwrap()
    }

    /// 直查 im_webhooks 行（测试轮询派发结果用）。
    fn webhook_row(h: &ImRouteHandler, id: &str) -> Option<ImWebhook> {
        let conn = h.shared.db.lock().expect("db poisoned");
        find_webhook(&conn, id).unwrap_or(None)
    }

    /// 必然投递失败的接收端点（127.0.0.1:1 无服务 → 秒级 ECONNREFUSED）。
    const DEAD_HOOK_URL: &str = "http://127.0.0.1:1/agent-hook";

    // N1. 注册归因：owner = token pubkey；缺省 events 双开；非法 url/events 400；
    //     conversation_id 未知 404 / 非成员群组 403 / 空串 400；无 token 401
    #[tokio::test]
    async fn notify_register_attribution_and_validation() {
        let h = empty_handler();
        let (pubkey, token) = login(&h, &new_key()).await;
        // 无 token → 401
        let anon = h
            .handle(post_req(
                PATH_NOTIFY_REGISTER,
                serde_json::json!({ "url": "http://127.0.0.1:9/x" }),
            ))
            .await
            .unwrap();
        assert_eq!(anon.status, 401);
        // 正常注册：owner 归因 + 缺省 events 双开
        let resp = notify_register(&h, &token, "http://127.0.0.1:9900/hook", None, None).await;
        assert_eq!(resp.status, 201, "注册应 201: {}", resp.body);
        assert!(!resp.body["id"].as_str().unwrap().is_empty());
        assert_eq!(resp.body["owner_pubkey"], pubkey, "owner 应为 token pubkey");
        assert_eq!(
            resp.body["events"],
            serde_json::json!(["lobby", "conversation"]),
            "缺省 events = 双开"
        );
        assert_eq!(resp.body["status"], "active");
        assert_eq!(resp.body["fail_count"], 0);
        assert_eq!(resp.body["last_fired_at"], serde_json::Value::Null);
        assert_eq!(resp.body["conversation_id"], serde_json::Value::Null);
        // events 部分订阅合法；非法值/空数组 → 400
        let only_lobby = notify_register(
            &h,
            &token,
            "http://127.0.0.1:9900/hook2",
            Some(vec!["lobby"]),
            None,
        )
        .await;
        assert_eq!(only_lobby.status, 201);
        assert_eq!(only_lobby.body["events"], serde_json::json!(["lobby"]));
        for bad_events in [vec!["nope"], vec!["lobby", "bogus"], Vec::<&str>::new()] {
            let r = notify_register(
                &h,
                &token,
                "http://127.0.0.1:9900/hook3",
                Some(bad_events.clone()),
                None,
            )
            .await;
            assert_eq!(r.status, 400, "events={bad_events:?} 应 400");
        }
        // 非法 url → 400
        for bad_url in ["ftp://x/y", "", "not-a-url", "http://"] {
            let r = notify_register(&h, &token, bad_url, None, None).await;
            assert_eq!(r.status, 400, "url={bad_url} 应 400");
        }
        // conversation_id：未知会话 404；空串 400
        let miss =
            notify_register(&h, &token, "http://127.0.0.1:9900/h", None, Some("no-such")).await;
        assert_eq!(miss.status, 404);
        let empty_cid =
            notify_register(&h, &token, "http://127.0.0.1:9900/h", None, Some("")).await;
        assert_eq!(empty_cid.status, 400);
        // 非成员群组 → 403（与离线补拉同款 member 门）
        let (_, token2) = login(&h, &new_key()).await;
        let g = h
            .handle(authed_post(
                PATH_GROUPS,
                &token2,
                serde_json::json!({ "name": "私密群" }),
            ))
            .await
            .unwrap();
        let gid = g.body["id"].as_str().unwrap().to_string();
        let denied = notify_register(&h, &token, "http://127.0.0.1:9900/h", None, Some(&gid)).await;
        assert_eq!(denied.status, 403, "非群组成员注册该会话 webhook 应 403");
    }

    // N2. list owner 过滤：各自只见自己的；无 token 401
    #[tokio::test]
    async fn notify_list_owner_filter() {
        let h = empty_handler();
        let (pubkey1, token1) = login(&h, &new_key()).await;
        let (pubkey2, token2) = login(&h, &new_key()).await;
        let r1 = notify_register(&h, &token1, "http://127.0.0.1:9901/a", None, None).await;
        let r2 = notify_register(&h, &token1, "http://127.0.0.1:9901/b", None, None).await;
        let r3 = notify_register(&h, &token2, "http://127.0.0.1:9901/c", None, None).await;
        assert_eq!(r1.status, 201);
        assert_eq!(r2.status, 201);
        assert_eq!(r3.status, 201);
        // 用户 1：恰 2 条，全部归因自己
        let l1 = h
            .handle(authed_get(PATH_NOTIFY_LIST, &token1))
            .await
            .unwrap();
        let arr = l1.body.as_array().unwrap();
        assert_eq!(arr.len(), 2, "用户 1 只看到自己的 2 条");
        assert!(arr.iter().all(|w| w["owner_pubkey"] == pubkey1));
        // 用户 2：恰 1 条
        let l2 = h
            .handle(authed_get(PATH_NOTIFY_LIST, &token2))
            .await
            .unwrap();
        let arr2 = l2.body.as_array().unwrap();
        assert_eq!(arr2.len(), 1);
        assert_eq!(arr2[0]["owner_pubkey"], pubkey2);
        assert_eq!(arr2[0]["url"], "http://127.0.0.1:9901/c");
        // 无 token → 401
        let anon = h.handle(get_req(PATH_NOTIFY_LIST)).await.unwrap();
        assert_eq!(anon.status, 401);
    }

    // N3. 注销权限矩阵：非 owner 403（注册表不动）；owner 200 后消失；
    //     再删 404；未知 id 404；无 token 401
    #[tokio::test]
    async fn notify_unregister_owner_only() {
        let h = empty_handler();
        let (_, token1) = login(&h, &new_key()).await;
        let (_, token2) = login(&h, &new_key()).await;
        let r = notify_register(&h, &token1, "http://127.0.0.1:9902/a", None, None).await;
        let wid = r.body["id"].as_str().unwrap().to_string();
        // 无 token → 401
        let anon = h
            .handle(ApiRequest {
                method: HttpMethod::Delete,
                path: format!("/api/v1/im/notify/{wid}"),
                headers: serde_json::json!({}),
                body: serde_json::Value::Null,
                auth: None,
            })
            .await
            .unwrap();
        assert_eq!(anon.status, 401);
        // 他人注销 → 403，注册表不动
        let denied = h
            .handle(authed_delete(&format!("/api/v1/im/notify/{wid}"), &token2))
            .await
            .unwrap();
        assert_eq!(denied.status, 403, "非 owner 注销应 403");
        assert!(webhook_row(&h, &wid).is_some(), "403 后行应保留");
        // owner 注销 → 200；列表清空
        let ok = h
            .handle(authed_delete(&format!("/api/v1/im/notify/{wid}"), &token1))
            .await
            .unwrap();
        assert_eq!(ok.status, 200);
        assert_eq!(ok.body["deleted"], true);
        assert!(webhook_row(&h, &wid).is_none(), "行应已删除");
        let list = h
            .handle(authed_get(PATH_NOTIFY_LIST, &token1))
            .await
            .unwrap();
        assert_eq!(list.body.as_array().unwrap().len(), 0);
        // 重复注销 / 未知 id → 404
        let again = h
            .handle(authed_delete(&format!("/api/v1/im/notify/{wid}"), &token1))
            .await
            .unwrap();
        assert_eq!(again.status, 404);
        let miss = h
            .handle(authed_delete("/api/v1/im/notify/no-such", &token1))
            .await
            .unwrap();
        assert_eq!(miss.status, 404);
    }

    // N4. 大厅消息触发 webhook：假接收端收到完整 Message JSON
    //     （sender_kind/mentions/attachment 真值）+ X-NexOS-Event 头；
    //     投递成功后 fail_count=0 + last_fired_at 落位
    #[tokio::test]
    async fn notify_lobby_message_dispatches_full_payload() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let url = spawn_webhook_receiver(seen.clone(), FakeHookMode::Ok).await;
        let (root_str, root) = temp_files_root("notify");
        let h = empty_handler().with_files_root(&root_str);
        let (pubkey, token) = login(&h, &new_key()).await;
        let reg = notify_register(&h, &token, &url, None, None).await;
        assert_eq!(reg.status, 201);
        let wid = reg.body["id"].as_str().unwrap().to_string();
        // 进大厅 + 带附件 + @ + agent 自声明发一条
        let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
        let up = upload(&h, &token, "路演.pptx", b"pptx").await;
        let file_id = up["file_id"].as_str().unwrap().to_string();
        let sent = h
            .handle(authed_post(
                PATH_LOBBY_MESSAGES,
                &token,
                serde_json::json!({
                    "content": "@alice 请看附件",
                    "sender_kind": "agent",
                    "attachment": { "file_id": file_id, "size_bytes": 999 }
                }),
            ))
            .await
            .unwrap();
        assert_eq!(sent.status, 201);
        let msg_id = sent.body["id"].as_str().unwrap().to_string();
        // 等投递到达（异步 spawn）
        let ok = wait_until(Duration::from_secs(5), || !seen.lock().unwrap().is_empty()).await;
        assert!(ok, "webhook 应收到大厅消息推送");
        let raw = seen.lock().unwrap()[0].clone();
        assert!(
            raw.starts_with("POST /agent-hook HTTP/1.1"),
            "应为 POST 到注册端点: {raw}"
        );
        assert!(
            raw.to_ascii_lowercase()
                .contains(&"x-nexos-event: lobby_message".to_ascii_lowercase()),
            "事件头应为 lobby_message: {raw}"
        );
        assert!(
            !raw.contains("authorization"),
            "不应携带任何 Authorization 头"
        );
        // body = 完整 Message JSON（附件真值覆盖 + mentions + sender_kind）
        let body_json: serde_json::Value =
            serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap_or(""))
                .expect("请求体应为合法 JSON");
        assert_eq!(body_json["id"], msg_id);
        assert_eq!(body_json["conversation_id"], "lobby");
        assert_eq!(body_json["sender_id"], pubkey);
        assert_eq!(body_json["sender_kind"], "agent");
        assert_eq!(body_json["mentions"], serde_json::json!(["alice"]));
        assert_eq!(body_json["attachment"]["file_id"], file_id);
        assert_eq!(body_json["attachment"]["filename"], "路演.pptx");
        assert_eq!(
            body_json["attachment"]["size_bytes"], 4,
            "附件 size 为落盘真值"
        );
        // 投递成功：fail_count=0 + last_fired_at 落位
        let ok = wait_until(Duration::from_secs(5), || {
            webhook_row(&h, &wid)
                .as_ref()
                .is_some_and(|w| w.last_fired_at.is_some())
        })
        .await;
        assert!(ok, "投递成功应记 last_fired_at");
        let w = webhook_row(&h, &wid).unwrap();
        assert_eq!(w.fail_count, 0);
        assert!(w.last_error.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    // N5. conversation 过滤：绑定 conv-1 的 webhook 只收 conv-1 的消息
    #[tokio::test]
    async fn notify_conversation_filter_pinned_only() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let url = spawn_webhook_receiver(seen.clone(), FakeHookMode::Ok).await;
        let h = empty_handler();
        let (_, token) = login(&h, &new_key()).await;
        let c1 = h
            .handle(authed_post(
                PATH_CONV_LIST,
                &token,
                serde_json::json!({ "name": "c1" }),
            ))
            .await
            .unwrap();
        let cid1 = c1.body["id"].as_str().unwrap().to_string();
        let c2 = h
            .handle(authed_post(
                PATH_CONV_LIST,
                &token,
                serde_json::json!({ "name": "c2" }),
            ))
            .await
            .unwrap();
        let cid2 = c2.body["id"].as_str().unwrap().to_string();
        // 只订阅 conversation 事件 + 绑定 cid1
        let reg = notify_register(&h, &token, &url, Some(vec!["conversation"]), Some(&cid1)).await;
        assert_eq!(reg.status, 201, "{}", reg.body);
        // cid1 消息 → 推送
        let to1 = h
            .handle(authed_post(
                &format!("/api/v1/im/conversations/{cid1}/messages"),
                &token,
                serde_json::json!({ "content": "给 conv1" }),
            ))
            .await
            .unwrap();
        assert_eq!(to1.status, 201);
        let ok = wait_until(Duration::from_secs(5), || !seen.lock().unwrap().is_empty()).await;
        assert!(ok, "绑定的会话消息应推送");
        assert!(seen.lock().unwrap()[0].contains("给 conv1"));
        assert!(seen.lock().unwrap()[0].contains("x-nexos-event: conversation_message"));
        // cid2 消息 → 不推送（负向断言：留足派发窗口后仍只有 1 条）
        let to2 = h
            .handle(authed_post(
                &format!("/api/v1/im/conversations/{cid2}/messages"),
                &token,
                serde_json::json!({ "content": "给 conv2" }),
            ))
            .await
            .unwrap();
        assert_eq!(to2.status, 201);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(seen.lock().unwrap().len(), 1, "非绑定会话的消息不应推送");
    }

    // N6. 事件过滤：lobby-only 不收会话消息；conversation-only 不收大厅消息
    #[tokio::test]
    async fn notify_event_filter_lobby_vs_conversation() {
        let seen_lobby = Arc::new(StdMutex::new(Vec::new()));
        let seen_conv = Arc::new(StdMutex::new(Vec::new()));
        let url_lobby = spawn_webhook_receiver(seen_lobby.clone(), FakeHookMode::Ok).await;
        let url_conv = spawn_webhook_receiver(seen_conv.clone(), FakeHookMode::Ok).await;
        let h = empty_handler();
        let (_, token) = login(&h, &new_key()).await;
        let r1 = notify_register(&h, &token, &url_lobby, Some(vec!["lobby"]), None).await;
        let r2 = notify_register(&h, &token, &url_conv, Some(vec!["conversation"]), None).await;
        assert_eq!(r1.status, 201);
        assert_eq!(r2.status, 201);
        // 会话消息：只有 conversation-only 收到
        let c = h
            .handle(authed_post(
                PATH_CONV_LIST,
                &token,
                serde_json::json!({ "name": "c" }),
            ))
            .await
            .unwrap();
        let cid = c.body["id"].as_str().unwrap().to_string();
        let conv_msg = h
            .handle(authed_post(
                &format!("/api/v1/im/conversations/{cid}/messages"),
                &token,
                serde_json::json!({ "content": "会话消息" }),
            ))
            .await
            .unwrap();
        assert_eq!(conv_msg.status, 201);
        let ok = wait_until(Duration::from_secs(5), || {
            !seen_conv.lock().unwrap().is_empty()
        })
        .await;
        assert!(ok, "conversation-only webhook 应收到会话消息");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            seen_lobby.lock().unwrap().is_empty(),
            "lobby-only webhook 不应收会话消息"
        );
        // 大厅消息：只有 lobby-only 收到
        let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
        let lobby_msg = h
            .handle(authed_post(
                PATH_LOBBY_MESSAGES,
                &token,
                serde_json::json!({ "content": "大厅消息" }),
            ))
            .await
            .unwrap();
        assert_eq!(lobby_msg.status, 201);
        let ok = wait_until(Duration::from_secs(5), || {
            !seen_lobby.lock().unwrap().is_empty()
        })
        .await;
        assert!(ok, "lobby-only webhook 应收到大厅消息");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            seen_conv.lock().unwrap().len(),
            1,
            "conversation-only webhook 不应收大厅消息"
        );
    }

    // N7. 超时不阻塞消息路径：接收端收下不回（触发 5s 超时），发消息的
    //     201 仍秒回（远小于 5s）
    #[tokio::test]
    async fn notify_timeout_does_not_block_message_path() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let url = spawn_webhook_receiver(seen.clone(), FakeHookMode::Hang).await;
        let h = empty_handler();
        let (_, token) = login(&h, &new_key()).await;
        let reg = notify_register(&h, &token, &url, None, None).await;
        assert_eq!(reg.status, 201);
        let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
        // 计时发消息：201 必须在 webhook 5s 超时之前返回
        let started = Instant::now();
        let sent = h
            .handle(authed_post(
                PATH_LOBBY_MESSAGES,
                &token,
                serde_json::json!({ "content": "不阻塞我" }),
            ))
            .await
            .unwrap();
        let elapsed = started.elapsed();
        assert_eq!(sent.status, 201);
        assert!(
            elapsed < Duration::from_secs(3),
            "消息响应应秒回（实际 {elapsed:?}，webhook 超时 5s）"
        );
        // 接收端确实收到了请求（挂起中）——证明派发真实发生只是不等它
        let ok = wait_until(Duration::from_secs(5), || !seen.lock().unwrap().is_empty()).await;
        assert!(ok, "挂起接收端应已收到投递请求");
    }

    // N8. 连败 5 次自动注销：死端口注册 → 5 条消息 5 连败 → status=disabled
    //     + last_error 记录；第 6 条消息不再尝试（fail_count 停在 5）
    #[tokio::test]
    async fn notify_auto_deregister_after_consecutive_failures() {
        let h = empty_handler();
        let (_, token) = login(&h, &new_key()).await;
        let reg = notify_register(&h, &token, DEAD_HOOK_URL, None, None).await;
        assert_eq!(reg.status, 201);
        let wid = reg.body["id"].as_str().unwrap().to_string();
        let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
        // 5 条消息 → 5 次秒败（ECONNREFUSED）→ 自动注销
        for i in 1..=5 {
            let r = h
                .handle(authed_post(
                    PATH_LOBBY_MESSAGES,
                    &token,
                    serde_json::json!({ "content": format!("第{i}条") }),
                ))
                .await
                .unwrap();
            assert_eq!(r.status, 201);
        }
        let ok = wait_until(Duration::from_secs(5), || {
            webhook_row(&h, &wid)
                .as_ref()
                .is_some_and(|w| w.status == "disabled")
        })
        .await;
        assert!(ok, "连败 5 次应自动注销（status=disabled）");
        let w = webhook_row(&h, &wid).unwrap();
        assert_eq!(w.fail_count, 5, "连败计数恰为 5");
        assert!(
            w.last_error.as_deref().unwrap_or("").contains("自动注销"),
            "last_error 应记录注销原因: {:?}",
            w.last_error
        );
        // 注销后不再尝试：第 6 条消息不推进 fail_count
        let r = h
            .handle(authed_post(
                PATH_LOBBY_MESSAGES,
                &token,
                serde_json::json!({ "content": "第六条" }),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201);
        tokio::time::sleep(Duration::from_millis(400)).await;
        let w = webhook_row(&h, &wid).unwrap();
        assert_eq!(w.fail_count, 5, "注销后不再派发，连败计数应停在 5");
        // 注册表仍在（owner 可见注销原因），重新注册同 url 即恢复
        let list = h
            .handle(authed_get(PATH_NOTIFY_LIST, &token))
            .await
            .unwrap();
        assert_eq!(list.body.as_array().unwrap().len(), 1);
        assert_eq!(list.body[0]["status"], "disabled");
    }

    // N9. 推送 body 不含敏感 token：请求原文（头+体）找不到发送者的
    //     IM token / admin token 字样；body 键集合 ⊆ Message 字段
    #[tokio::test]
    async fn notify_payload_carries_no_sensitive_token() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let url = spawn_webhook_receiver(seen.clone(), FakeHookMode::Ok).await;
        let h = empty_handler().with_admin_token("adm-secret-1");
        let (_, token) = login(&h, &new_key()).await;
        let reg = notify_register(&h, &token, &url, None, None).await;
        assert_eq!(reg.status, 201);
        let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
        let sent = h
            .handle(authed_post(
                PATH_LOBBY_MESSAGES,
                &token,
                serde_json::json!({ "content": "机密测试" }),
            ))
            .await
            .unwrap();
        assert_eq!(sent.status, 201);
        let ok = wait_until(Duration::from_secs(5), || !seen.lock().unwrap().is_empty()).await;
        assert!(ok);
        let raw = seen.lock().unwrap()[0].clone();
        assert!(!raw.contains(&token), "推送不得泄露发送者 IM token: {raw}");
        assert!(!raw.contains("adm-secret-1"), "推送不得泄露 admin token");
        assert!(!raw.contains("authorization"), "不应带 Authorization 头");
        // body 键集合 = Message DTO 字段（无任何凭证类键）
        let body: serde_json::Value =
            serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap_or("")).unwrap();
        let keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        for k in &keys {
            assert!(
                matches!(
                    *k,
                    "id" | "conversation_id"
                        | "sender_id"
                        | "sender_name"
                        | "content"
                        | "msg_type"
                        | "file_url"
                        | "reply_to"
                        | "created_at"
                        | "read_by"
                        | "sender_kind"
                        | "mentions"
                        | "attachment"
                ),
                "payload 出现非 Message 字段: {k}"
            );
        }
    }

    // N10. 纯函数矩阵：url 校验 / 事件名 / 匹配判定 / events 归一
    #[test]
    fn notify_pure_functions_matrix() {
        // url 校验
        assert!(is_valid_webhook_url("http://127.0.0.1:9000/hook"));
        assert!(is_valid_webhook_url("https://agent.example.com/im"));
        assert!(!is_valid_webhook_url("ftp://x/y"));
        assert!(!is_valid_webhook_url(""));
        assert!(!is_valid_webhook_url(&format!(
            "http://x/{}",
            "a".repeat(2100)
        )));
        // 事件名
        assert_eq!(webhook_event_name(LOBBY_ID), "lobby_message");
        assert_eq!(webhook_event_name("conv-1"), "conversation_message");
        // events 归一：合法去重 / 非法 None / 空 None
        assert_eq!(
            normalize_webhook_events(&["lobby".into(), "lobby".into(), "conversation".into()]),
            Some(vec!["lobby".to_string(), "conversation".to_string()])
        );
        assert_eq!(normalize_webhook_events(&["nope".into()]), None);
        assert_eq!(normalize_webhook_events(&[]), None);
        // 匹配矩阵
        let mk = |events: &[&str], cid: Option<&str>, status: &str| ImWebhook {
            id: "w1".into(),
            url: "http://127.0.0.1:9/h".into(),
            owner_pubkey: "pk".into(),
            events: events.iter().map(|s| s.to_string()).collect(),
            conversation_id: cid.map(str::to_string),
            status: status.into(),
            fail_count: 0,
            last_fired_at: None,
            last_error: None,
            created_at: "t".into(),
        };
        let lobby_msg: Message = serde_json::from_value(serde_json::json!({
            "id": "m1", "conversation_id": LOBBY_ID, "sender_id": "a",
            "content": "hi", "created_at": "t"
        }))
        .unwrap();
        let conv_msg: Message = serde_json::from_value(serde_json::json!({
            "id": "m2", "conversation_id": "conv-1", "sender_id": "a",
            "content": "hi", "created_at": "t"
        }))
        .unwrap();
        // disabled 永不匹配
        assert!(!webhook_matches(
            &mk(&["lobby", "conversation"], None, "disabled"),
            &lobby_msg
        ));
        // 大厅消息：订阅 lobby 才收；conversation_id 绑定不影响大厅
        assert!(webhook_matches(&mk(&["lobby"], None, "active"), &lobby_msg));
        assert!(webhook_matches(
            &mk(&["lobby"], Some("conv-1"), "active"),
            &lobby_msg
        ));
        assert!(!webhook_matches(
            &mk(&["conversation"], None, "active"),
            &lobby_msg
        ));
        // 会话消息：订阅 conversation 才收；绑定须一致
        assert!(webhook_matches(
            &mk(&["conversation"], None, "active"),
            &conv_msg
        ));
        assert!(webhook_matches(
            &mk(&["conversation"], Some("conv-1"), "active"),
            &conv_msg
        ));
        assert!(!webhook_matches(
            &mk(&["conversation"], Some("conv-2"), "active"),
            &conv_msg
        ));
        assert!(!webhook_matches(&mk(&["lobby"], None, "active"), &conv_msg));
        // 双开全收
        assert!(webhook_matches(
            &mk(&["lobby", "conversation"], None, "active"),
            &conv_msg
        ));
    }

    // N11. 会话消息触发：pinned webhook 收到 conversation_message 头 + 完整 body
    //      （覆盖 N5 未验的头部/会话 id 断言）
    #[tokio::test]
    async fn notify_conversation_message_event_header() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let url = spawn_webhook_receiver(seen.clone(), FakeHookMode::Ok).await;
        let h = empty_handler();
        let (pubkey, token) = login(&h, &new_key()).await;
        let c = h
            .handle(authed_post(
                PATH_CONV_LIST,
                &token,
                serde_json::json!({ "name": "c" }),
            ))
            .await
            .unwrap();
        let cid = c.body["id"].as_str().unwrap().to_string();
        let reg = notify_register(&h, &token, &url, Some(vec!["conversation"]), Some(&cid)).await;
        assert_eq!(reg.status, 201);
        let sent = h
            .handle(authed_post(
                &format!("/api/v1/im/conversations/{cid}/messages"),
                &token,
                serde_json::json!({ "content": "会话推送", "sender_kind": "agent" }),
            ))
            .await
            .unwrap();
        assert_eq!(sent.status, 201);
        let msg_id = sent.body["id"].as_str().unwrap().to_string();
        let ok = wait_until(Duration::from_secs(5), || !seen.lock().unwrap().is_empty()).await;
        assert!(ok);
        let raw = seen.lock().unwrap()[0].clone();
        assert!(raw.contains("x-nexos-event: conversation_message"), "{raw}");
        let body: serde_json::Value =
            serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap_or("")).unwrap();
        assert_eq!(body["id"], msg_id);
        assert_eq!(body["conversation_id"], cid);
        assert_eq!(body["sender_id"], pubkey);
        assert_eq!(body["sender_kind"], "agent");
    }

    // ---- P3 联邦大厅（docs/NEXOS_P2P_NETWORK_DESIGN.md §8）----

    /// 人类大厅消息 fixture（pubkey 发送者）。
    fn human_lobby_msg(id: &str, sender: &str, content: &str) -> Message {
        Message {
            id: id.to_string(),
            conversation_id: LOBBY_ID.to_string(),
            sender_id: sender.to_string(),
            sender_name: Some("远程用户".to_string()),
            content: content.to_string(),
            msg_type: "text".to_string(),
            file_url: None,
            reply_to: None,
            created_at: "2026-08-22T10:00:00+08:00".to_string(),
            read_by: vec![sender.to_string()],
            sender_kind: "human".to_string(),
            mentions: Vec::new(),
            attachment: None,
        }
    }

    // 1. 联邦纯函数：载荷形状 + federable 裁决（agent/系统不联邦，human 联邦）
    #[test]
    fn fed_payload_shape_and_federable_rules() {
        let msg = human_lobby_msg("m-1", "0xabc", "hello fed");
        assert!(lobby_message_federable(&msg), "人类消息应联邦");
        let payload = build_im_lobby_fed_payload("node-106", &msg);
        assert_eq!(payload["fed"], FED_KIND_IM_LOBBY);
        assert_eq!(payload["node"], "node-106");
        assert_eq!(payload["message"]["id"], "m-1");
        assert_eq!(payload["message"]["content"], "hello fed");
        assert_eq!(payload["message"]["sender_id"], "0xabc");
        // agent 消息（助手回复）不联邦——联邦网内不重复 AI 回答
        let mut agent = msg.clone();
        agent.sender_kind = "agent".to_string();
        agent.sender_id = "agent:nexos-assistant".to_string();
        assert!(!lobby_message_federable(&agent), "agent 消息不联邦");
        // 系统欢迎消息不联邦（入廊是本地事件）
        let welcome = build_welcome_message("alice");
        assert!(!lobby_message_federable(&welcome), "欢迎消息不联邦");
        let mut sys = msg.clone();
        sys.msg_type = "system".to_string();
        assert!(!lobby_message_federable(&sys), "system 类型不联邦");
    }

    // 2. P2P 未启用：POST /lobby/messages 与 POST /fed-lobby/messages 均照常
    //    201（本地写入不受联邦影响；federate 显式调用静默跳过返回 false）
    #[tokio::test]
    async fn fed_post_lobby_without_p2p_silently_skips() {
        let h = empty_handler();
        assert!(!h.federation().is_federated(), "未注入 P2P");
        let (_pubkey, token) = login(&h, &new_key()).await;
        // 自动加入大厅（两个大厅端点共用在场表）
        let resp = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
        assert_eq!(resp.status, 200);
        let resp = h
            .handle(authed_post(
                PATH_LOBBY_MESSAGES,
                &token,
                serde_json::json!({ "content": "无 P2P 也能发" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "未启用 P2P 时发消息照常 201");
        // 联邦大厅发言（GET /fed-lobby 自动加入在场表）
        let resp = h.handle(authed_get(PATH_FED_LOBBY, &token)).await.unwrap();
        assert_eq!(resp.status, 200);
        let resp = h
            .handle(authed_post(
                PATH_FED_LOBBY_MESSAGES,
                &token,
                serde_json::json!({ "content": "联邦大厅无 P2P 也能发" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "未启用 P2P 时联邦大厅发言照常 201");
        // federate 显式调用也返回 false（静默跳过）
        let msg: Message = serde_json::from_value(resp.body).unwrap();
        assert!(
            !h.federation().federate_fed_lobby_message(&msg).await,
            "无 Handle 时广播跳过"
        );
    }

    // 3. 联邦接收：新旧两种 fed 载荷 → Written，恒落 fed-lobby 会话
    //    （sender_id 前缀 + sender_name 🌐 来源标注；与我的大厅隔离）
    #[tokio::test]
    async fn fed_ingest_writes_message_with_fed_sender_prefix() {
        let h = empty_handler();
        let fed = h.federation();
        // 新 kind（现行 fed-lobby 发言广播）
        let msg = human_lobby_msg("fed-m-1", "0xdeadbeef", "来自远程的问候");
        let payload = build_im_fed_lobby_payload("node-106", &msg);
        assert_eq!(payload["fed"], FED_KIND_IM_FED_LOBBY);
        assert_eq!(fed.ingest(&payload), ImFedIngest::Written);
        let saved = h
            .messages_snapshot()
            .into_iter()
            .find(|m| m.id == "fed-m-1")
            .expect("应写入本地 im_messages");
        assert_eq!(
            saved.conversation_id, FED_LOBBY_ID,
            "远程联邦消息恒落联邦大厅（与我的大厅隔离）"
        );
        assert_eq!(
            saved.sender_id,
            format!("{FED_SENDER_PREFIX}node-106:0xdeadbeef"),
            "sender_id 加来源前缀"
        );
        assert_eq!(
            saved.sender_name.as_deref(),
            Some("🌐 远程用户（node-106）"),
            "sender_name 加 🌐 来源标注"
        );
        assert_eq!(saved.content, "来自远程的问候");
        assert_eq!(saved.sender_kind, "human");
        // 旧 kind（旧版节点 im_lobby 广播）兼容接收——同样落 fed-lobby
        let legacy = human_lobby_msg("fed-m-legacy", "0xcafe", "旧版节点的广播");
        assert_eq!(
            fed.ingest(&build_im_lobby_fed_payload("node-old", &legacy)),
            ImFedIngest::Written
        );
        let saved_legacy = h
            .messages_snapshot()
            .into_iter()
            .find(|m| m.id == "fed-m-legacy")
            .expect("旧载荷兼容落地");
        assert_eq!(saved_legacy.conversation_id, FED_LOBBY_ID);
        assert_eq!(saved_legacy.sender_id, "fed:node-old:0xcafe");
        assert!(
            !h.messages_snapshot().iter().any(
                |m| m.conversation_id == LOBBY_ID && m.sender_id.starts_with(FED_SENDER_PREFIX)
            ),
            "我的大厅（lobby）不再出现联邦消息"
        );
    }

    // 4. 联邦接收去重：同 id 二次收不重写（内存缓存 + DB 双重判定）
    #[tokio::test]
    async fn fed_ingest_dedups_same_message_id() {
        let h = empty_handler();
        let fed = h.federation();
        let msg = human_lobby_msg("fed-dup", "0x1", "只写一次");
        let payload = build_im_lobby_fed_payload("node-a", &msg);
        assert_eq!(fed.ingest(&payload), ImFedIngest::Written);
        assert_eq!(fed.ingest(&payload), ImFedIngest::Duplicate, "缓存命中");
        // 重启语义：新端点（缓存为空）仍靠 DB 兜底不重写
        let fresh = ImRouteHandler::with_empty();
        // 同一 DB 需共享：直接用同一 handler 的另一端点视角——ImFederation
        // 是 Arc<ImShared> 封装，缓存属端点私有；DB 兜底用同 handler 验证：
        // （清缓存不可行，故 DB 兜底路径由 nexhub/bridge 测试与下面 5 覆盖）
        assert_eq!(
            fed.ingest(&payload),
            ImFedIngest::Duplicate,
            "三次收仍不重写"
        );
        assert_eq!(
            h.messages_snapshot()
                .iter()
                .filter(|m| m.id == "fed-dup")
                .count(),
            1,
            "库中仅一条"
        );
        drop(fresh);
    }

    // 5. 联邦接收：agent/系统/非 im_lobby/缺字段载荷一律 Ignored 零写入
    #[tokio::test]
    async fn fed_ingest_ignores_agent_system_and_foreign_payloads() {
        let h = empty_handler();
        let fed = h.federation();
        // agent 消息（远端助手回复不落本地）
        let mut agent = human_lobby_msg("fed-agent", "0x2", "AI 回复");
        agent.sender_kind = "agent".to_string();
        assert_eq!(
            fed.ingest(&build_im_lobby_fed_payload("n", &agent)),
            ImFedIngest::Ignored
        );
        // 系统消息
        let sys = build_welcome_message("bob");
        assert_eq!(
            fed.ingest(&build_im_lobby_fed_payload("n", &sys)),
            ImFedIngest::Ignored
        );
        // 非 im_lobby（NexHub 大厅条目等他类载荷）
        assert_eq!(
            fed.ingest(&serde_json::json!({"fed": "nexhub_lobby", "node": "n", "entry": {}})),
            ImFedIngest::Ignored
        );
        // 无 fed 标记（P2b 调试消息 {text}）
        assert_eq!(
            fed.ingest(&serde_json::json!({"text": "hi"})),
            ImFedIngest::Ignored
        );
        // 缺 node / 缺 message / message 非法
        let m = human_lobby_msg("x", "0x3", "c");
        assert_eq!(
            fed.ingest(&serde_json::json!({"fed": FED_KIND_IM_LOBBY, "message": m})),
            ImFedIngest::Ignored
        );
        assert_eq!(
            fed.ingest(&serde_json::json!({"fed": FED_KIND_IM_LOBBY, "node": "n"})),
            ImFedIngest::Ignored
        );
        assert_eq!(
            fed.ingest(&serde_json::json!({"fed": FED_KIND_IM_LOBBY, "node": "n", "message": 42})),
            ImFedIngest::Ignored
        );
        assert!(
            !h.messages_snapshot()
                .iter()
                .any(|m| m.sender_id.starts_with(FED_SENDER_PREFIX)),
            "全部非法/不可联邦载荷零写入（库中仅有 schema 的欢迎消息）"
        );
    }

    // 6. 联邦接收触发 WS 广播（本地在线用户实时看到远程联邦大厅消息——
    //    帧型 im_fed_lobby_message，路由到联邦大厅会话而非我的大厅）
    #[tokio::test]
    async fn fed_ingest_broadcasts_ws_to_local_users() {
        let hub = WsHub::new(8);
        let h = ImRouteHandler::with_empty_ws(hub, std::sync::Arc::new(ImAuth::new()));
        let (_sid, mut rx) = {
            let hub2 = match &h.shared.ws_hub {
                Some(h) => h,
                None => panic!("应持有 Hub"),
            };
            hub2.subscribe_raw("ws-user")
        };
        let fed = h.federation();
        let msg = human_lobby_msg("fed-ws-1", "0x4", "远程 WS 推送");
        assert_eq!(
            fed.ingest(&build_im_fed_lobby_payload("node-106", &msg)),
            ImFedIngest::Written
        );
        let ws = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("WS 广播应即时")
            .expect("订阅存活");
        match ws {
            WsMessage::ImFedLobbyMessage { lobby_id, message } => {
                assert_eq!(lobby_id, FED_LOBBY_ID);
                assert_eq!(message["id"], "fed-ws-1");
                assert_eq!(message["conversation_id"], FED_LOBBY_ID);
                assert_eq!(
                    message["sender_id"],
                    format!("{FED_SENDER_PREFIX}node-106:0x4")
                );
            }
            other => panic!("应为 ImFedLobbyMessage，实际 {other:?}"),
        }
    }

    // ---- 联邦接收开关（2026-08-23：GET/POST /api/v1/im/federation）----

    // 7. 开关端点鉴权矩阵 + 状态读写：GET 默认开（匿名 401）→ POST（IM token）
    //    关闭 → GET 反映 → POST（admin token）重开；匿名 POST 401 / 缺字段 400
    #[tokio::test]
    async fn fed_toggle_endpoints_auth_and_state() {
        let h = empty_handler().with_admin_token("adm-fed-1");
        let (_pubkey, token) = login(&h, &new_key()).await;
        // 匿名 GET / POST 一律 401
        assert_eq!(
            h.handle(get_req(PATH_FEDERATION)).await.unwrap().status,
            401
        );
        assert_eq!(
            h.handle(post_req(
                PATH_FEDERATION,
                serde_json::json!({"enabled": false})
            ))
            .await
            .unwrap()
            .status,
            401
        );
        // 默认开 + note 文案
        let resp = h.handle(authed_get(PATH_FEDERATION, &token)).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["enabled"], true, "默认开");
        assert!(
            resp.body["note"].as_str().unwrap().contains("开启"),
            "note 应说明开启状态: {}",
            resp.body["note"]
        );
        // body 缺 enabled → 400
        let resp = h
            .handle(authed_post(
                PATH_FEDERATION,
                &token,
                serde_json::json!({"foo": 1}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // IM token 关闭 → enabled=false + note 说明暂停
        let resp = h
            .handle(authed_post(
                PATH_FEDERATION,
                &token,
                serde_json::json!({"enabled": false}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["enabled"], false);
        assert!(
            resp.body["note"].as_str().unwrap().contains("暂停"),
            "note 应说明暂停状态: {}",
            resp.body["note"]
        );
        // GET 反映新状态
        let resp = h.handle(authed_get(PATH_FEDERATION, &token)).await.unwrap();
        assert_eq!(resp.body["enabled"], false);
        // admin token（无 IM token）可重开——Bearer 头同格式
        let resp = h
            .handle(authed_post(
                PATH_FEDERATION,
                "adm-fed-1",
                serde_json::json!({"enabled": true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "admin token 应可切换");
        assert_eq!(resp.body["enabled"], true);
        assert!(h.federation().fed_enabled(), "内核状态同步");
    }

    // 8. 关闭后 ingest 入口短路：Paused 零写入（合法载荷也不落地）
    #[tokio::test]
    async fn fed_ingest_paused_when_disabled_zero_write() {
        let h = empty_handler();
        let fed = h.federation();
        assert!(fed.fed_enabled(), "默认开");
        // 经端点关闭（端点→内核同一条路）
        let (_pk, token) = login(&h, &new_key()).await;
        let resp = h
            .handle(authed_post(
                PATH_FEDERATION,
                &token,
                serde_json::json!({"enabled": false}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.body["enabled"], false);
        // 合法 im_lobby 载荷也短路 Paused
        let msg = human_lobby_msg("fed-paused-1", "0x9", "暂停期间的远程消息");
        let payload = build_im_lobby_fed_payload("node-x", &msg);
        assert_eq!(fed.ingest(&payload), ImFedIngest::Paused);
        // 他类载荷同样在入口短路（开关优先于载荷解析）
        assert_eq!(
            fed.ingest(&serde_json::json!({"fed": "nexhub_lobby", "entry": {}})),
            ImFedIngest::Paused
        );
        assert!(
            !h.messages_snapshot().iter().any(|m| m.id == "fed-paused-1"),
            "暂停期间零写入"
        );
    }

    // 9. 重新打开即恢复：同载荷正常落地（Written）
    #[tokio::test]
    async fn fed_ingest_resumes_after_reenable() {
        let h = empty_handler();
        let fed = h.federation();
        fed.set_fed_enabled(false);
        let msg = human_lobby_msg("fed-resume-1", "0xa", "恢复后的远程消息");
        let payload = build_im_lobby_fed_payload("node-y", &msg);
        assert_eq!(fed.ingest(&payload), ImFedIngest::Paused);
        // 经端点重开（POST enabled=true）
        let (_pk, token) = login(&h, &new_key()).await;
        let resp = h
            .handle(authed_post(
                PATH_FEDERATION,
                &token,
                serde_json::json!({"enabled": true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.body["enabled"], true);
        // 同一载荷正常写入（未被暂停期间的丢弃污染去重缓存）
        assert_eq!(fed.ingest(&payload), ImFedIngest::Written);
        assert!(
            h.messages_snapshot()
                .iter()
                .any(|m| m.id == "fed-resume-1" && m.sender_id == "fed:node-y:0xa"),
            "恢复后正常写入（带来源前缀）"
        );
    }

    // 10. 接收开关不影响发送：关闭接收后 POST /fed-lobby/messages 照常 201，
    //     且内部 federate 照常广播——对端节点收到 im_fed_lobby_message 载荷
    //     （双节点端到端）
    #[tokio::test]
    async fn fed_receive_toggle_does_not_affect_send() {
        use os_p2p::{P2pConfig, P2pNode, Timing};
        // 双节点 mesh（handlers/p2p.rs 测试同款：A 公网锚点 + B 引导到 A）
        let a = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            public: true,
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .expect("A 随机端口绑定必成功");
        let b = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            bootstrap: vec![a.listen_addr()],
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .expect("B 随机端口绑定必成功");
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let peers = a.peers().await;
            if peers.iter().any(|p| p.id == *b.self_id() && p.connected) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // 1ms 级节流时延注入：联邦广播走延迟队列（默认 10s 太慢）
        let h = fast_fed_handler();
        let fed = h.federation();
        fed.set_p2p(a.clone(), "node-a".into());
        let (_pk, token) = login(&h, &new_key()).await;
        // 先加入联邦大厅（POST /fed-lobby/messages 须成员）
        let resp = h.handle(authed_get(PATH_FED_LOBBY, &token)).await.unwrap();
        assert_eq!(resp.status, 200);
        // 关闭接收 → 本地发送 + 广播均不受影响
        fed.set_fed_enabled(false);
        let mut brx = b.on_msg();
        let resp = h
            .handle(authed_post(
                PATH_FED_LOBBY_MESSAGES,
                &token,
                serde_json::json!({"content": "关了接收也能发"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "本地发消息不受接收开关影响");
        let msg_id = resp.body["id"].as_str().unwrap().to_string();
        // 对端收到联邦广播（POST 内部的 federate_fed_lobby_message 照常执行——
        // 现经延迟队列，测试注入 1ms 时延快速到期）
        let got = tokio::time::timeout(Duration::from_secs(3), brx.recv())
            .await
            .expect("对端应收到广播（发送不受接收开关影响）")
            .expect("broadcast 存活");
        assert_eq!(got.payload["fed"], FED_KIND_IM_FED_LOBBY);
        assert_eq!(got.payload["node"], "node-a");
        assert_eq!(got.payload["message"]["id"], msg_id.as_str());
        assert_eq!(got.payload["message"]["conversation_id"], FED_LOBBY_ID);
        a.shutdown().await;
        b.shutdown().await;
    }

    // ---- 联邦大厅独立会话（fed-lobby，2026-08-23 用户纠正：可写、与我的大厅隔离）----

    // 11. GET /fed-lobby：心跳 + 加入（在场表记录成员）+ 信息聚合
    //     （id 恒为 fed-lobby；无欢迎系统消息——联邦大厅是跨节点频道）
    #[tokio::test]
    async fn fed_lobby_join_info_and_heartbeat() {
        let h = empty_handler();
        let (pubkey, token) = login(&h, &new_key()).await;
        // 匿名 401
        assert_eq!(h.handle(get_req(PATH_FED_LOBBY)).await.unwrap().status, 401);
        let resp = h.handle(authed_get(PATH_FED_LOBBY, &token)).await.unwrap();
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert_eq!(resp.body["id"], FED_LOBBY_ID);
        assert_eq!(resp.body["name"], "联邦大厅");
        assert_eq!(resp.body["member_count"], 1, "加入后在场表 +1");
        assert_eq!(resp.body["online_count"], 1, "心跳即时在线");
        assert_eq!(
            resp.body["last_message"],
            serde_json::Value::Null,
            "暂无消息"
        );
        // 加入记录在在场表（与我的大厅共用本节点 IM 在场）
        let members = {
            let conn = h.shared.db.lock().expect("db poisoned");
            load_lobby_members(&conn).unwrap_or_default()
        };
        assert!(members.iter().any(|m| m.user_id == pubkey));
        // 无欢迎系统消息落 fed-lobby
        assert!(
            !h.messages_snapshot()
                .iter()
                .any(|m| m.conversation_id == FED_LOBBY_ID),
            "加入联邦大厅不产生系统消息"
        );
    }

    // 12. POST /fed-lobby/messages：写入 fed-lobby 会话 + 列表/增量补拉
    #[tokio::test]
    async fn fed_lobby_post_writes_lists_and_incremental() {
        let h = empty_handler();
        let (pubkey, token) = login(&h, &new_key()).await;
        let resp = h.handle(authed_get(PATH_FED_LOBBY, &token)).await.unwrap();
        assert_eq!(resp.status, 200);
        let first = h
            .handle(authed_post(
                PATH_FED_LOBBY_MESSAGES,
                &token,
                serde_json::json!({"content": "第一条 @NexOS助手", "sender_kind": "agent"}),
            ))
            .await
            .unwrap();
        assert_eq!(first.status, 201, "{}", first.body);
        assert_eq!(first.body["conversation_id"], FED_LOBBY_ID);
        assert_eq!(
            first.body["sender_id"], pubkey,
            "sender = token 反查 pubkey"
        );
        assert_eq!(
            first.body["sender_kind"], "agent",
            "sender_kind 展示层自声明（agent 声明保留；联邦广播按 federable 规则跳过）"
        );
        assert_eq!(
            first.body["mentions"],
            serde_json::json!(["NexOS助手"]),
            "mentions 服务端解析"
        );
        assert!(first.body["attachment"].is_null(), "联邦通道不承载附件");
        let second = h
            .handle(authed_post(
                PATH_FED_LOBBY_MESSAGES,
                &token,
                serde_json::json!({"content": "第二条"}),
            ))
            .await
            .unwrap();
        assert_eq!(second.status, 201);
        // 全量列表（最近 50 条，时间正序）
        let list = h
            .handle(authed_get(PATH_FED_LOBBY_MESSAGES, &token))
            .await
            .unwrap();
        let arr = list.body.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["content"], "第一条 @NexOS助手");
        assert_eq!(arr[1]["content"], "第二条");
        // 增量补拉：after_id=第一条 → 只剩第二条
        let first_id = first.body["id"].as_str().unwrap();
        let gap = h
            .handle(authed_get(
                &format!("{PATH_FED_LOBBY_MESSAGES}?after_id={first_id}"),
                &token,
            ))
            .await
            .unwrap();
        let gap_arr = gap.body.as_array().unwrap();
        assert_eq!(gap_arr.len(), 1, "增量只补缺口");
        assert_eq!(gap_arr[0]["content"], "第二条");
        // 通用补拉端点 conversation_id=fed-lobby 也可读（跨节点公共频道）
        let catchup = h
            .handle(authed_get(
                &format!("/api/v1/im/messages?conversation_id={FED_LOBBY_ID}&after_id={first_id}"),
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(catchup.status, 200, "{}", catchup.body);
        assert_eq!(catchup.body.as_array().unwrap().len(), 1);
        // 信息聚合的最近消息跟随
        let info = h.handle(authed_get(PATH_FED_LOBBY, &token)).await.unwrap();
        assert_eq!(info.body["last_message"]["content"], "第二条");
        // sender_kind 非 agent 白名单值 → 归一 human（展示层自声明兜底）
        let junk = h
            .handle(authed_post(
                PATH_FED_LOBBY_MESSAGES,
                &token,
                serde_json::json!({"content": "垃圾声明", "sender_kind": "robot"}),
            ))
            .await
            .unwrap();
        assert_eq!(junk.status, 201);
        assert_eq!(junk.body["sender_kind"], "human", "非 agent 归一 human");
    }

    // 13. fed-lobby 端点鉴权矩阵：匿名 401 ×3 / 未加入 403 / 空正文 400 /
    //     加入后 201（GET 心跳即加入）
    #[tokio::test]
    async fn fed_lobby_endpoints_auth_matrix() {
        let h = empty_handler();
        let (_pubkey, token) = login(&h, &new_key()).await;
        // 匿名三端点一律 401
        assert_eq!(h.handle(get_req(PATH_FED_LOBBY)).await.unwrap().status, 401);
        assert_eq!(
            h.handle(get_req(PATH_FED_LOBBY_MESSAGES))
                .await
                .unwrap()
                .status,
            401
        );
        assert_eq!(
            h.handle(post_req(
                PATH_FED_LOBBY_MESSAGES,
                serde_json::json!({"content": "x"})
            ))
            .await
            .unwrap()
            .status,
            401
        );
        // 未加入（GET /fed-lobby 尚未调用）直接发言 → 403
        let resp = h
            .handle(authed_post(
                PATH_FED_LOBBY_MESSAGES,
                &token,
                serde_json::json!({"content": "还没加入"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 403, "{}", resp.body);
        // GET /fed-lobby/messages 心跳同样自动加入
        let resp = h
            .handle(authed_get(PATH_FED_LOBBY_MESSAGES, &token))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        // 空白正文 → 400
        let resp = h
            .handle(authed_post(
                PATH_FED_LOBBY_MESSAGES,
                &token,
                serde_json::json!({"content": "   "}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // 加入后发言 → 201
        let resp = h
            .handle(authed_post(
                PATH_FED_LOBBY_MESSAGES,
                &token,
                serde_json::json!({"content": "加入了"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
    }

    // 14. fed-lobby 与我的大厅完全隔离：互不串消息（本地发言/远程联邦消息各归各会话）
    #[tokio::test]
    async fn fed_lobby_and_my_lobby_fully_isolated() {
        let h = empty_handler();
        let fed = h.federation();
        let (pubkey, token) = login(&h, &new_key()).await;
        let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap(); // 加入我的大厅
        let _ = h.handle(authed_get(PATH_FED_LOBBY, &token)).await.unwrap(); // 加入联邦大厅
                                                                             // 两边各发一条
        let _ = h
            .handle(authed_post(
                PATH_LOBBY_MESSAGES,
                &token,
                serde_json::json!({"content": "我的大厅消息"}),
            ))
            .await
            .unwrap();
        let _ = h
            .handle(authed_post(
                PATH_FED_LOBBY_MESSAGES,
                &token,
                serde_json::json!({"content": "联邦大厅消息"}),
            ))
            .await
            .unwrap();
        // 远程联邦消息（ingest）只落 fed-lobby
        let remote = human_lobby_msg("iso-remote", "0x77", "来自对端的联邦消息");
        assert_eq!(
            fed.ingest(&build_im_fed_lobby_payload("node-b", &remote)),
            ImFedIngest::Written
        );
        // 列表互不含对方的消息
        let lobby_list = h
            .handle(authed_get(PATH_LOBBY_MESSAGES, &token))
            .await
            .unwrap();
        for m in lobby_list.body.as_array().unwrap() {
            assert_ne!(m["content"], "联邦大厅消息", "我的大厅不含 fed-lobby 发言");
            assert_ne!(m["content"], "来自对端的联邦消息");
        }
        let fed_list = h
            .handle(authed_get(PATH_FED_LOBBY_MESSAGES, &token))
            .await
            .unwrap();
        let fed_arr = fed_list.body.as_array().unwrap();
        assert_eq!(fed_arr.len(), 2, "本地发言 + 远程联邦消息");
        for m in fed_arr {
            assert_ne!(m["content"], "我的大厅消息", "联邦大厅不含我的大厅发言");
        }
        // 快照按 conversation_id 严格分离
        let snap = h.messages_snapshot();
        assert_eq!(
            snap.iter()
                .filter(|m| m.conversation_id == LOBBY_ID && m.sender_id == pubkey)
                .count(),
            1
        );
        assert_eq!(
            snap.iter()
                .filter(|m| m.conversation_id == FED_LOBBY_ID && m.sender_id == pubkey)
                .count(),
            1
        );
        assert_eq!(
            snap.iter()
                .filter(|m| m.conversation_id == FED_LOBBY_ID && m.sender_id == "fed:node-b:0x77")
                .count(),
            1
        );
    }

    // 15. POST /fed-lobby/messages P2P 广播（双节点端到端：对端收到
    //     im_fed_lobby_message 载荷，message.conversation_id=fed-lobby）——
    //     2026-08-24 起经延迟队列到期广播（测试注入 1ms 时延）
    #[tokio::test]
    async fn fed_lobby_post_p2p_broadcast_two_nodes() {
        use os_p2p::{P2pConfig, P2pNode, Timing};
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
        while Instant::now() < deadline {
            let peers = a.peers().await;
            if peers.iter().any(|p| p.id == *b.self_id() && p.connected) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // 1ms 级节流时延注入：联邦广播走延迟队列（默认 10s 太慢）
        let h = fast_fed_handler();
        h.federation().set_p2p(a.clone(), "node-a".into());
        let (pubkey, token) = login(&h, &new_key()).await;
        let _ = h.handle(authed_get(PATH_FED_LOBBY, &token)).await.unwrap();
        let mut brx = b.on_msg();
        let resp = h
            .handle(authed_post(
                PATH_FED_LOBBY_MESSAGES,
                &token,
                serde_json::json!({"content": "跨节点你好"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        let got = tokio::time::timeout(Duration::from_secs(3), brx.recv())
            .await
            .expect("对端应收到联邦大厅广播")
            .expect("broadcast 存活");
        assert_eq!(got.payload["fed"], FED_KIND_IM_FED_LOBBY);
        assert_eq!(got.payload["node"], "node-a");
        assert_eq!(got.payload["message"]["conversation_id"], FED_LOBBY_ID);
        assert_eq!(got.payload["message"]["sender_id"], pubkey);
        assert_eq!(got.payload["message"]["content"], "跨节点你好");
        a.shutdown().await;
        b.shutdown().await;
    }

    // ---- 联邦大厅发言时延节流（2026-08-24：不限次、不拒绝、不丢消息——
    //      仅延后联邦广播时刻；本地即时不变）----

    // 15a. 状态机纯逻辑：首条发言 → 常态时延 10s
    #[test]
    fn fed_throttle_first_message_is_short_delay() {
        let mut t = FedThrottle::new(FED_THROTTLE_SHORT, FED_THROTTLE_LONG);
        let t0 = Instant::now();
        assert_eq!(
            t.delay_for("0xa1", t0),
            FED_THROTTLE_SHORT,
            "首条（窗口内零历史）→ 10s"
        );
    }

    // 15b. 状态机纯逻辑：60s 窗口内第二条、第三条 → 升级时延 60s
    #[test]
    fn fed_throttle_second_and_third_within_window_are_long() {
        let mut t = FedThrottle::new(FED_THROTTLE_SHORT, FED_THROTTLE_LONG);
        let t0 = Instant::now();
        assert_eq!(t.delay_for("0xa1", t0), FED_THROTTLE_SHORT);
        assert_eq!(
            t.delay_for("0xa1", t0 + Duration::from_secs(5)),
            FED_THROTTLE_LONG,
            "60s 内第二条 → 60s"
        );
        assert_eq!(
            t.delay_for("0xa1", t0 + Duration::from_secs(20)),
            FED_THROTTLE_LONG,
            "60s 内第三条仍 60s（不限次，只升时延）"
        );
    }

    // 15c. 状态机纯逻辑：安静 61s 后窗口滑空 → 回落 10s
    #[test]
    fn fed_throttle_falls_back_after_quiet_61s() {
        let mut t = FedThrottle::new(FED_THROTTLE_SHORT, FED_THROTTLE_LONG);
        let t0 = Instant::now();
        assert_eq!(t.delay_for("0xa1", t0), FED_THROTTLE_SHORT);
        assert_eq!(
            t.delay_for("0xa1", t0 + Duration::from_secs(1)),
            FED_THROTTLE_LONG
        );
        // 距最后一条（t0+1）61s → 全部滑出 60s 窗口，按首条对待
        assert_eq!(
            t.delay_for("0xa1", t0 + Duration::from_secs(62)),
            FED_THROTTLE_SHORT,
            "安静 61s 后回落 10s"
        );
    }

    // 15d. 状态机纯逻辑：窗口滑动边界 + 发送者隔离
    //      （30s 前一条 + 现在一条 = 2 → 60s；60.5s 前的不再计入 → 10s；
    //        另一 sender 零历史不受他人影响 → 10s）
    #[test]
    fn fed_throttle_sliding_window_and_sender_isolation() {
        let mut t = FedThrottle::new(FED_THROTTLE_SHORT, FED_THROTTLE_LONG);
        let t0 = Instant::now();
        assert_eq!(t.delay_for("0xa1", t0), FED_THROTTLE_SHORT);
        assert_eq!(
            t.delay_for("0xa1", t0 + Duration::from_secs(30)),
            FED_THROTTLE_LONG,
            "30s 前一条在窗口内 → 含本次 2 条 → 60s"
        );
        // 上一条在 t0+30，距今 60.5s → 滑出窗口
        assert_eq!(
            t.delay_for(
                "0xa1",
                t0 + Duration::from_secs(30) + Duration::from_millis(60_500)
            ),
            FED_THROTTLE_SHORT,
            "60.5s 前的发言不再计入 → 回落 10s"
        );
        // 发送者隔离：0xb2 的首条不受 0xa1 的密集发言影响
        assert_eq!(
            t.delay_for("0xb2", t0),
            FED_THROTTLE_SHORT,
            "per-sender 计数，互不影响"
        );
    }

    // 15e. HTTP 路径透出：首条 federate_delay_secs=10 + note；60s 内第二条
    //      =60；agent 自声明消息不参与联邦（0 + 专属 note，且不占节流计数）
    #[tokio::test]
    async fn fed_lobby_post_response_exposes_federate_delay() {
        let h = empty_handler(); // 默认 10s/60s（无 P2P，只验证响应字段）
        let (pubkey, token) = login(&h, &new_key()).await;
        let _ = h.handle(authed_get(PATH_FED_LOBBY, &token)).await.unwrap();
        let first = h
            .handle(authed_post(
                PATH_FED_LOBBY_MESSAGES,
                &token,
                serde_json::json!({"content": "首条"}),
            ))
            .await
            .unwrap();
        assert_eq!(first.status, 201, "{}", first.body);
        assert_eq!(first.body["federate_delay_secs"], 10, "首条常态 10s");
        assert_eq!(first.body["sender_id"], pubkey);
        assert!(
            first.body["note"]
                .as_str()
                .is_some_and(|n| n.contains("联邦广播将于 10 秒后发出")),
            "note 说明时延: {}",
            first.body["note"]
        );
        let second = h
            .handle(authed_post(
                PATH_FED_LOBBY_MESSAGES,
                &token,
                serde_json::json!({"content": "二条"}),
            ))
            .await
            .unwrap();
        assert_eq!(second.status, 201);
        assert_eq!(
            second.body["federate_delay_secs"], 60,
            "同一 sender 60s 内第二条升至 60s"
        );
        assert!(
            second.body["note"]
                .as_str()
                .is_some_and(|n| n.contains("60 秒")),
            "note 升级说明: {}",
            second.body["note"]
        );
        // agent 自声明：不联邦（0 + 专属 note），且不改变他人计数
        let agent = h
            .handle(authed_post(
                PATH_FED_LOBBY_MESSAGES,
                &token,
                serde_json::json!({"content": "agent 不出本节点", "sender_kind": "agent"}),
            ))
            .await
            .unwrap();
        assert_eq!(agent.status, 201, "{}", agent.body);
        assert_eq!(agent.body["federate_delay_secs"], 0, "agent 消息不联邦");
        assert!(
            agent.body["note"]
                .as_str()
                .is_some_and(|n| n.contains("不参与联邦广播")),
            "agent note: {}",
            agent.body["note"]
        );
    }

    // 15f. 延迟队列端到端（双节点 + 1ms 级时延注入）：本地即时落库，联邦
    //      广播到期发出且**按入队序**送达对端（消息永不丢弃、不限次）
    #[tokio::test]
    async fn fed_throttle_delay_queue_end_to_end_two_nodes() {
        use os_p2p::{P2pConfig, P2pNode, Timing};
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
        while Instant::now() < deadline {
            let peers = a.peers().await;
            if peers.iter().any(|p| p.id == *b.self_id() && p.connected) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let h = fast_fed_handler();
        h.federation().set_p2p(a.clone(), "node-a".into());
        let (_pk, token) = login(&h, &new_key()).await;
        let _ = h.handle(authed_get(PATH_FED_LOBBY, &token)).await.unwrap();
        // 连发两条（本地应立即全在库——联邦延迟不影响本地体验）
        let r1 = h
            .handle(authed_post(
                PATH_FED_LOBBY_MESSAGES,
                &token,
                serde_json::json!({"content": "队列一号"}),
            ))
            .await
            .unwrap();
        let r2 = h
            .handle(authed_post(
                PATH_FED_LOBBY_MESSAGES,
                &token,
                serde_json::json!({"content": "队列二号"}),
            ))
            .await
            .unwrap();
        assert_eq!(r1.status, 201);
        assert_eq!(r2.status, 201, "第二条不被拒绝——不限次，仅升时延");
        assert!(
            r1.body["federate_delay_secs"].is_u64() && r2.body["federate_delay_secs"].is_u64(),
            "响应透出 federate_delay_secs 字段（1ms 注入下取整为 0）"
        );
        assert_eq!(
            h.messages_snapshot()
                .iter()
                .filter(|m| m.conversation_id == FED_LOBBY_ID)
                .count(),
            2,
            "本地两条均已即时落库（联邦延迟不动本地）"
        );
        // 对端按入队序收到两条联邦广播（经延迟队列到期发出）
        let mut brx = b.on_msg();
        let got1 = tokio::time::timeout(Duration::from_secs(3), brx.recv())
            .await
            .expect("对端应收到第一条联邦广播")
            .expect("broadcast 存活");
        let got2 = tokio::time::timeout(Duration::from_secs(3), brx.recv())
            .await
            .expect("对端应收到第二条联邦广播（不限次不丢消息）")
            .expect("broadcast 存活");
        assert_eq!(got1.payload["fed"], FED_KIND_IM_FED_LOBBY);
        assert_eq!(got1.payload["message"]["content"], "队列一号");
        assert_eq!(
            got2.payload["message"]["content"], "队列二号",
            "按入队序送达"
        );
        a.shutdown().await;
        b.shutdown().await;
    }

    // 16. 我的大厅不再自动联邦广播（双节点：POST /lobby/messages 后对端收不到
    //     任何载荷——完全隔离；联邦大厅发言才有广播）
    #[tokio::test]
    async fn my_lobby_no_longer_federates_two_nodes() {
        use os_p2p::{P2pConfig, P2pNode, Timing};
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
        while Instant::now() < deadline {
            let peers = a.peers().await;
            if peers.iter().any(|p| p.id == *b.self_id() && p.connected) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let h = empty_handler();
        h.federation().set_p2p(a.clone(), "node-a".into());
        let (_pk, token) = login(&h, &new_key()).await;
        let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
        let mut brx = b.on_msg();
        let resp = h
            .handle(authed_post(
                PATH_LOBBY_MESSAGES,
                &token,
                serde_json::json!({"content": "只留本节点"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        assert!(
            tokio::time::timeout(Duration::from_millis(800), brx.recv())
                .await
                .is_err(),
            "我的大厅发言不再联邦广播（对端收不到任何载荷）"
        );
        a.shutdown().await;
        b.shutdown().await;
    }

    // 17. WS 广播正确路由：lobby 发言 → im_lobby_message 帧；fed-lobby 发言 →
    //     im_fed_lobby_message 帧（同一 Hub 两帧型互不混淆）
    #[tokio::test]
    async fn ws_broadcast_routes_lobby_vs_fed_lobby() {
        let hub = WsHub::new(8);
        let h = ImRouteHandler::with_empty_ws(hub, std::sync::Arc::new(ImAuth::new()));
        let (pubkey, token) = login(&h, &new_key()).await;
        let _ = h.handle(authed_get(PATH_LOBBY, &token)).await.unwrap();
        let _ = h.handle(authed_get(PATH_FED_LOBBY, &token)).await.unwrap();
        // 订阅放在两次 GET 之后——避开 GET /lobby 的欢迎系统消息广播
        let (_sid, mut rx) = match &h.shared.ws_hub {
            Some(hub2) => hub2.subscribe_raw("ws-user"),
            None => panic!("应持有 Hub"),
        };
        // fed-lobby 发言 → ImFedLobbyMessage
        let resp = h
            .handle(authed_post(
                PATH_FED_LOBBY_MESSAGES,
                &token,
                serde_json::json!({"content": "联邦频道帧"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        let ws1 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("联邦大厅 WS 广播应即时")
            .expect("订阅存活");
        match ws1 {
            WsMessage::ImFedLobbyMessage { lobby_id, message } => {
                assert_eq!(lobby_id, FED_LOBBY_ID);
                assert_eq!(message["conversation_id"], FED_LOBBY_ID);
                assert_eq!(message["content"], "联邦频道帧");
            }
            other => panic!("fed-lobby 发言应为 ImFedLobbyMessage，实际 {other:?}"),
        }
        // lobby 发言 → ImLobbyMessage（不受 fed-lobby 影响）
        let resp = h
            .handle(authed_post(
                PATH_LOBBY_MESSAGES,
                &token,
                serde_json::json!({"content": "本节点频道帧"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        let ws2 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("大厅 WS 广播应即时")
            .expect("订阅存活");
        match ws2 {
            WsMessage::ImLobbyMessage { lobby_id, message } => {
                assert_eq!(lobby_id, LOBBY_ID);
                assert_eq!(message["conversation_id"], LOBBY_ID);
                assert_eq!(message["sender_id"], pubkey);
            }
            other => panic!("lobby 发言应为 ImLobbyMessage，实际 {other:?}"),
        }
    }

    // 18. 联邦接收暂停对 fed-lobby 同样生效：关闭后 ingest 短路 Paused
    //     （暂停期间远程联邦消息不落 fed-lobby），恢复即写入
    #[tokio::test]
    async fn fed_lobby_ingest_paused_and_resume() {
        let h = empty_handler();
        let fed = h.federation();
        fed.set_fed_enabled(false);
        let remote = human_lobby_msg("fed-pause-1", "0x51", "暂停期间的联邦消息");
        assert_eq!(
            fed.ingest(&build_im_fed_lobby_payload("node-z", &remote)),
            ImFedIngest::Paused
        );
        assert!(
            !h.messages_snapshot().iter().any(|m| m.id == "fed-pause-1"),
            "暂停期间零写入"
        );
        fed.set_fed_enabled(true);
        assert_eq!(
            fed.ingest(&build_im_fed_lobby_payload("node-z", &remote)),
            ImFedIngest::Written
        );
        let saved = h
            .messages_snapshot()
            .into_iter()
            .find(|m| m.id == "fed-pause-1")
            .unwrap();
        assert_eq!(saved.conversation_id, FED_LOBBY_ID);
    }

    // ---- 大厅开放开关 + 远程大厅浏览/发言（2026-08-23，节点发现页联动）----

    /// 直接向 handler 的内存库插一条大厅消息（绕过 REST——测试种子数据）。
    fn seed_lobby_msg(h: &ImRouteHandler, msg: &Message) {
        let conn = h.shared.db.lock().expect("db poisoned");
        insert_message(&conn, msg).expect("种子消息写入必成功");
    }

    // 11. 开关端点：开发期默认 true + 鉴权矩阵 + IM/admin token 读写 + note 文案
    #[tokio::test]
    async fn lobby_access_endpoints_auth_default_and_toggle() {
        let h = empty_handler().with_admin_token("adm-lobby-1");
        let (_pubkey, token) = login(&h, &new_key()).await;
        // 匿名 GET/POST 一律 401
        assert_eq!(
            h.handle(get_req(PATH_LOBBY_ACCESS)).await.unwrap().status,
            401
        );
        assert_eq!(
            h.handle(post_req(
                PATH_LOBBY_ACCESS,
                serde_json::json!({"lobby_public": false})
            ))
            .await
            .unwrap()
            .status,
            401
        );
        // 开发期默认 true（缺省开放，允许其他节点浏览）+ note 说明
        let resp = h
            .handle(authed_get(PATH_LOBBY_ACCESS, &token))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["lobby_public"], true, "开发期缺省开放");
        assert!(
            resp.body["note"].as_str().unwrap().contains("开放"),
            "note 应说明开放状态: {}",
            resp.body["note"]
        );
        assert!(h.federation().lobby_public(), "内核状态同步默认 true");
        // body 缺字段 → 400
        let resp = h
            .handle(authed_post(
                PATH_LOBBY_ACCESS,
                &token,
                serde_json::json!({"foo": 1}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // IM token 关闭 → false + note 说明未开放语义
        let resp = h
            .handle(authed_post(
                PATH_LOBBY_ACCESS,
                &token,
                serde_json::json!({"lobby_public": false}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["lobby_public"], false);
        assert!(resp.body["note"].as_str().unwrap().contains("未开放"));
        // GET 反映新状态；admin token（无 IM token）可重新打开——Bearer 同格式
        assert_eq!(
            h.handle(authed_get(PATH_LOBBY_ACCESS, &token))
                .await
                .unwrap()
                .body["lobby_public"],
            false
        );
        let resp = h
            .handle(authed_post(
                PATH_LOBBY_ACCESS,
                "adm-lobby-1",
                serde_json::json!({"lobby_public": true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "admin token 应可切换");
        assert_eq!(resp.body["lobby_public"], true);
        assert!(h.federation().lobby_public(), "内核状态同步开启");
    }

    // 12. 查询应答 denied 路径：开关关闭（lobby_public=false）回 denied、不读库不泄消息
    #[test]
    fn lobby_query_reply_denied_by_default() {
        let h = demo_handler();
        let fed = h.federation();
        assert!(fed.lobby_public(), "开发期默认开放");
        fed.set_lobby_public(false);
        assert!(!fed.lobby_public(), "手动关闭后为 false");
        let payload = fed.lobby_query_reply_payload("req-1");
        assert_eq!(payload["fed"], FED_KIND_IM_LOBBY_REPLY);
        assert_eq!(payload["req_id"], "req-1");
        assert_eq!(payload["public"], false);
        assert_eq!(payload["error"], "denied");
        assert!(
            payload.get("messages").is_none(),
            "denied 应答不带任何消息：{payload}"
        );
    }

    // 13. 开放路径 + 脱敏 + 上限：25 条种子（带附件/文件 URL）→ 恰好最近 20 条、
    //     无 attachment/file_url/read_by/mentions 字段
    #[test]
    fn lobby_query_reply_open_sanitized_and_capped_at_20() {
        let h = empty_handler();
        // 25 条消息，created_at 递增（m-00 最旧 … m-24 最新），全部带附件元数据
        for i in 0..25 {
            let mut m = human_lobby_msg(&format!("m-{i:02}"), "0xabc", &format!("msg {i}"));
            // 时间取 2026-12-31（晚于 with_empty 预置的 msg-lobby-seed 当前时刻）
            m.created_at = format!("2026-12-31T10:{i:02}:00+08:00");
            m.attachment = Some(Attachment {
                file_id: format!("f-{i}"),
                filename: "secret.pdf".into(),
                size_bytes: 1024,
                mime: Some("application/pdf".into()),
            });
            m.file_url = Some(format!("/api/v1/im/files/f-{i}"));
            seed_lobby_msg(&h, &m);
        }
        let fed = h.federation();
        fed.set_lobby_public(true);
        let payload = fed.lobby_query_reply_payload("req-2");
        assert_eq!(payload["fed"], FED_KIND_IM_LOBBY_REPLY);
        assert_eq!(payload["public"], true);
        let msgs = payload["messages"].as_array().expect("开放应带消息数组");
        assert_eq!(msgs.len(), LOBBY_VIEW_LIMIT, "恰好 20 条（上限）");
        // 时间正序 + 是最近的 20 条（m-05..m-24，丢弃最旧 5 条）
        assert_eq!(msgs[0]["id"], "m-05");
        assert_eq!(msgs[19]["id"], "m-24");
        // 脱敏：无 attachment / file_url / read_by / mentions 字段（文件内容不出本机）
        for m in msgs {
            assert!(m.get("attachment").is_none(), "镜像不含附件: {m}");
            assert!(m.get("file_url").is_none(), "镜像不含文件 URL: {m}");
            assert!(m.get("read_by").is_none(), "镜像不含已读名单: {m}");
            assert!(m.get("mentions").is_none(), "镜像不含提及列表: {m}");
            // 展示必需字段齐备
            for field in ["id", "sender_id", "sender_name", "content", "created_at"] {
                assert!(m.get(field).is_some(), "镜像缺 {field}: {m}");
            }
        }
    }

    // 14. 远程发言（im_lobby_post）：开关关闭丢弃 → 开放后落地（fed: 前缀 + 无附件）
    //     → 联邦接收暂停 Paused → 非法载荷 Ignored
    #[tokio::test]
    async fn fed_lobby_post_gated_until_public() {
        let h = empty_handler();
        let fed = h.federation();
        let payload = build_lobby_post_payload("node-113", "0xab", "0xC0FFEE", "远程问候");
        // 开发期默认开放 → 远程发言直接落地
        assert_eq!(fed.ingest_lobby_post(&payload), ImFedIngest::Written);
        assert!(
            h.messages_snapshot()
                .iter()
                .any(|m| m.content == "远程问候"),
            "缺省开放期间应落地"
        );
        // 手动关闭 → 静默丢弃（零新增写入）
        fed.set_lobby_public(false);
        assert_eq!(fed.ingest_lobby_post(&payload), ImFedIngest::Ignored);
        assert_eq!(
            h.messages_snapshot()
                .iter()
                .filter(|m| m.content == "远程问候")
                .count(),
            1,
            "关闭期间零新增写入"
        );
        // 重新开放 → 落地（sender_id = 远端原 pubkey——直接发到本机大厅的消息
        // 不加 fed: 前缀（fed: 前缀归属联邦大厅）；不承载附件）
        fed.set_lobby_public(true);
        assert_eq!(fed.ingest_lobby_post(&payload), ImFedIngest::Written);
        let got = h
            .messages_snapshot()
            .into_iter()
            .find(|m| m.content == "远程问候")
            .expect("开放后应落地");
        assert_eq!(
            got.sender_id, "0xab",
            "直接进入本机大厅：sender_id 不加前缀"
        );
        assert_eq!(
            got.sender_name.as_deref(),
            Some("🌐 0xC0FFEE（node-113）"),
            "sender_name 标注远端来源"
        );
        assert_eq!(got.conversation_id, LOBBY_ID);
        assert!(got.attachment.is_none(), "远程通道不承载附件");
        // 联邦接收暂停 → Paused（与 ingest 同一道闸门）
        fed.set_fed_enabled(false);
        assert_eq!(
            fed.ingest_lobby_post(&payload),
            ImFedIngest::Paused,
            "暂停接收时远程发言同样丢弃"
        );
        fed.set_fed_enabled(true);
        // 非法载荷：空正文 / 缺 sender / 他类 fed → Ignored
        assert_eq!(
            fed.ingest_lobby_post(&build_lobby_post_payload("node-113", "0xab", "n", "   ")),
            ImFedIngest::Ignored
        );
        assert_eq!(
            fed.ingest_lobby_post(&serde_json::json!({
                "fed": FED_KIND_IM_LOBBY_POST, "node": "node-113", "content": "缺 sender"
            })),
            ImFedIngest::Ignored
        );
        assert_eq!(
            fed.ingest_lobby_post(&serde_json::json!({"fed": "im_lobby"})),
            ImFedIngest::Ignored
        );
    }

    // 15. 远程大厅 REST 端点：鉴权/参数校验矩阵 + 无应答超时路径
    //     （真实 p2p 节点但无应答端 → public=null error=timeout；?timeout_ms=300 快速）
    #[tokio::test]
    async fn remote_lobby_rest_auth_validation_and_timeout() {
        use os_p2p::{P2pConfig, P2pNode, Timing};
        let h = empty_handler();
        let (_pk, token) = login(&h, &new_key()).await;
        // 匿名 → 401；非法 node_id → 400
        assert_eq!(
            h.handle(get_req(PATH_LOBBY_REMOTE)).await.unwrap().status,
            401
        );
        let bad = "/api/v1/im/lobby/remote/0x00";
        let resp = h.handle(authed_get(bad, &token)).await.unwrap();
        assert_eq!(resp.status, 400, "非 66hex node_id 应 400");
        let resp = h
            .handle(authed_post(
                "/api/v1/im/lobby/remote/0x00/messages",
                &token,
                serde_json::json!({"content": "hi"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // 空正文 → 400
        let node = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        let hex = node.self_id().to_hex();
        // 注入 Handle（探针就绪）但对端无应答桥 → timeout（?timeout_ms=300 钳制下限）
        h.federation().set_p2p(node.clone(), "node-a".into());
        let resp = h
            .handle(authed_get(
                &format!("/api/v1/im/lobby/remote/{hex}?timeout_ms=300"),
                &token,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "超时是数据态（200 + error 字段）非 5xx");
        assert_eq!(resp.body["node_id"], hex);
        assert_eq!(resp.body["public"], serde_json::Value::Null);
        assert_eq!(resp.body["error"], "timeout");
        // POST：空正文 400；无应答 → 504
        let resp = h
            .handle(authed_post(
                &format!("/api/v1/im/lobby/remote/{hex}/messages?timeout_ms=300"),
                &token,
                serde_json::json!({"content": "   "}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "空正文 400");
        let resp = h
            .handle(authed_post(
                &format!("/api/v1/im/lobby/remote/{hex}/messages?timeout_ms=300"),
                &token,
                serde_json::json!({"content": "有人在吗"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 504, "对方无应答 → 504");
        node.shutdown().await;
    }

    // 16. 双节点端到端：GET 镜像 denied → 对端开放后带脱敏消息 → POST 远程发言
    //     落地对端大厅（fed: 前缀）——节点发现页「进入 IM」的完整数据流
    #[tokio::test]
    async fn remote_lobby_two_nodes_end_to_end() {
        use crate::handlers::p2p::FederationBridge;
        use os_p2p::{P2pConfig, P2pNode, Timing};
        // B（应答端，公网锚点）+ A（查询端，引导到 B）
        let b_node = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
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
        let h_a = empty_handler();
        let h_b = empty_handler();
        let fed_a = h_a.federation();
        let fed_b = h_b.federation();
        fed_a.set_p2p(a_node.clone(), "node-a".into());
        fed_b.set_p2p(b_node.clone(), "node-b".into());
        // B 侧入站桥（answer_lobby_query / ingest_lobby_post 在此触发）
        let bridge = FederationBridge {
            im: Some(fed_b.clone()),
            nexhub: None,
            live: None,
            api_market: None,
        };
        let mut brx = b_node.on_msg();
        tokio::spawn(async move {
            while let Ok(m) = brx.recv().await {
                bridge.dispatch(&m);
            }
        });
        // 等 A↔B 直连建立
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let peers = a_node.peers().await;
            if peers
                .iter()
                .any(|p| p.id == *b_node.self_id() && p.connected)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // —— 3) A 经 REST 远程发言 → 落地 B 的大厅（sender=远端原 pubkey，
        //     不加 fed: 前缀——直接进入对方大厅的消息在"我的大厅"显示）——
        let (pk, token) = login(&h_a, &new_key()).await;
        let b_hex = b_node.self_id().to_hex();
        let remote = format!("/api/v1/im/lobby/remote/{b_hex}?timeout_ms=8000");

        // —— 1) B 手动关闭（开发期缺省开放，显式关）→ denied ——
        fed_b.set_lobby_public(false);
        let resp = h_a.handle(authed_get(&remote, &token)).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["public"], false, "关闭后 denied");
        assert_eq!(resp.body["error"], "denied");

        // —— 2) B 开放 + 种子消息 → 镜像可见（脱敏）——
        fed_b.set_lobby_public(true);
        let mut seeded = human_lobby_msg("b-msg-1", "0xbb", "B 节点的消息");
        seeded.created_at = "2026-08-23T11:00:00+08:00".into();
        seeded.attachment = Some(Attachment {
            file_id: "f-b1".into(),
            filename: "b-secret.pdf".into(),
            size_bytes: 9,
            mime: None,
        });
        seed_lobby_msg(&h_b, &seeded);
        let resp = h_a.handle(authed_get(&remote, &token)).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["public"], true, "开放后允许浏览");
        let msgs = resp.body["messages"].as_array().expect("镜像消息数组");
        assert!(msgs.iter().any(|m| m["id"] == "b-msg-1"), "含 B 的种子消息");
        let mirror = msgs.iter().find(|m| m["id"] == "b-msg-1").unwrap();
        assert!(
            mirror.get("attachment").is_none(),
            "镜像脱敏：不带附件（含 B 侧附件）"
        );

        // —— 3) A 经 REST 远程发言 → 落地 B 的大厅（fed:node-a:<pubkey>）——
        let resp = h_a
            .handle(authed_post(
                &format!("/api/v1/im/lobby/remote/{b_hex}/messages?timeout_ms=8000"),
                &token,
                serde_json::json!({"content": "来自 A 的远程发言"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "开放后远程发言放行");
        assert_eq!(resp.body["ok"], true);
        // 轮询 B 落地（fire-and-forget，毫秒级但异步）
        let deadline = Instant::now() + Duration::from_secs(5);
        let landed = loop {
            let hit = h_b
                .messages_snapshot()
                .into_iter()
                .any(|m| m.content == "来自 A 的远程发言");
            if hit || Instant::now() > deadline {
                break hit;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert!(landed, "远程发言应落地 B 大厅");
        let got = h_b
            .messages_snapshot()
            .into_iter()
            .find(|m| m.content == "来自 A 的远程发言")
            .unwrap();
        assert_eq!(
            got.sender_id, pk,
            "直接进入对方大厅：sender_id 为远端原 pubkey（不加 fed: 前缀）"
        );
        assert!(
            got.sender_name
                .as_deref()
                .is_some_and(|n| n.starts_with("🌐 ") && n.contains("node-a")),
            "sender_name 标注远端来源: {:?}",
            got.sender_name
        );

        // —— 4) B 关闭开关 → GET 回 denied、POST 回 403（开关切换联动）——
        fed_b.set_lobby_public(false);
        let resp = h_a.handle(authed_get(&remote, &token)).await.unwrap();
        assert_eq!(resp.body["public"], false, "关闭后回 denied");
        let resp = h_a
            .handle(authed_post(
                &format!("/api/v1/im/lobby/remote/{b_hex}/messages?timeout_ms=8000"),
                &token,
                serde_json::json!({"content": "关闭后发言"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 403, "关闭后远程发言被拒");
        assert!(
            !h_b.messages_snapshot()
                .iter()
                .any(|m| m.content == "关闭后发言"),
            "关闭期间零写入"
        );
        a_node.shutdown().await;
        b_node.shutdown().await;
    }

    // 17. 本地指纹跳过 P2P 自回路（2026-08-23）：A 与 B 共用同一私钥（同
    //     NodeID 的另一 OS 实例——身份=密钥，同指纹即同权限域）→ 联邦广播
    //     （federate_fed_lobby_message → fed_broadcast）与大厅查询应答
    //     （answer_lobby_query）对指纹==本机 NodeID 的目标都不经 P2P：消息
    //     已在本地落库，发给同指纹节点只会自回路重复入库。
    #[tokio::test]
    async fn federation_skips_local_fingerprint_targets() {
        use os_p2p::{NodeIdentity, P2pConfig, P2pNode, Timing};
        let identity = NodeIdentity::generate();
        let a = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            public: true,
            identity: Some(identity.clone()),
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        let b = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            bootstrap: vec![a.listen_addr()],
            identity: Some(identity), // 同一私钥 → 同 NodeID 的对端
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        // 等同指纹对端连入（A 侧 identity_conflicts 记账即连接凭证）
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && a.identity_conflicts().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            !a.identity_conflicts().await.is_empty(),
            "同私钥实例应已连入（冲突记账为凭证）"
        );
        let h = empty_handler();
        let fed = h.federation();
        fed.set_p2p(a.clone(), "node-a".into());
        let mut rx = a.on_msg();
        // —— 1) 联邦大厅广播：唯一对端是本机指纹 → 0 目标，返回 false ——
        let msg = human_lobby_msg("m-local-fp", "0xabc", "同指纹广播");
        assert!(
            !fed.federate_fed_lobby_message(&msg).await,
            "指纹==本机的目标被跳过 → 广播 0 peer，返回 false"
        );
        // —— 2) 大厅查询应答：来自本机指纹的 im_lobby_query 跳过（本地自回路）——
        fed.answer_lobby_query(
            a.self_id(),
            &serde_json::json!({"fed": FED_KIND_IM_LOBBY_QUERY, "req_id": "r-local-fp"}),
        );
        // 两处均不得产生本地回声（send 到本机 NodeID 会本地回环交付到 on_msg）
        let echoed = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
        assert!(
            echoed.is_err(),
            "本地指纹目标不得经 P2P 自回路回声: {:?}",
            echoed.ok()
        );
        a.shutdown().await;
        b.shutdown().await;
    }

    // =========================================================================
    // 点对点直通消息 DM（2026-08-30）单元测 —— DM 面
    // —— 开关读写/默认 true、403 关闭拒发、确定性会话 id（双向同 id）、
    //    落库+定向推送形状、跨节点 ingest（双 handler，A 发 B 收）、
    //    ingest 对方关闭丢弃、members 感知列表、去重、回程路由
    // =========================================================================

    /// DM 测试前置：登录 a/b 两身份，b 经 GET /lobby 心跳成为本节点在场身份
    /// （identity_local 命中大厅成员路径）。
    async fn dm_login_pair(h: &ImRouteHandler) -> ((String, String), (String, String)) {
        let a = login(h, &new_key()).await;
        let b = login(h, &new_key()).await;
        let _ = h.handle(authed_get(PATH_LOBBY, &b.1)).await.unwrap();
        (a, b)
    }

    // DM1. 开关端点：鉴权矩阵 / 开发阶段默认 true / 读写往返 / note 语义
    #[tokio::test]
    async fn dm_access_endpoints_auth_default_and_toggle() {
        let h = empty_handler().with_admin_token("adm-dm-1");
        let (_pubkey, token) = login(&h, &new_key()).await;
        // 匿名 GET/POST 一律 401
        assert_eq!(h.handle(get_req(PATH_DM_ACCESS)).await.unwrap().status, 401);
        assert_eq!(
            h.handle(post_req(
                PATH_DM_ACCESS,
                serde_json::json!({"dm_open": false})
            ))
            .await
            .unwrap()
            .status,
            401
        );
        // 开发阶段默认 true（用户裁决「当前开发阶段默认允许」）+ note 说明
        let resp = h.handle(authed_get(PATH_DM_ACCESS, &token)).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["dm_open"], true, "开发阶段缺省允许直通消息");
        assert!(
            resp.body["note"].as_str().unwrap().contains("开放"),
            "note 应说明开放状态: {}",
            resp.body["note"]
        );
        assert!(h.federation().dm_open(), "内核状态同步默认 true");
        // body 缺字段 → 400
        let resp = h
            .handle(authed_post(
                PATH_DM_ACCESS,
                &token,
                serde_json::json!({"foo": 1}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // IM token 关闭 → false + note 说明关闭语义（对方发送被拒）
        let resp = h
            .handle(authed_post(
                PATH_DM_ACCESS,
                &token,
                serde_json::json!({"dm_open": false}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["dm_open"], false);
        assert!(resp.body["note"].as_str().unwrap().contains("关闭"));
        // GET 反映新状态；admin token（无 IM token）可重新打开
        assert_eq!(
            h.handle(authed_get(PATH_DM_ACCESS, &token))
                .await
                .unwrap()
                .body["dm_open"],
            false
        );
        let resp = h
            .handle(authed_post(
                PATH_DM_ACCESS,
                "adm-dm-1",
                serde_json::json!({"dm_open": true}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "admin token 应可切换");
        assert_eq!(resp.body["dm_open"], true);
        assert!(h.federation().dm_open(), "内核状态同步重开");
    }

    // DM2. 确定性会话 id（纯函数）：双向同 id、dm- 前缀、不同对不碰撞
    #[test]
    fn dm_conversation_id_symmetric_and_prefixed() {
        let a = "0x1111111111111111111111111111111111111111111111111111111111111111";
        let b = "0x2222222222222222222222222222222222222222222222222222222222222222";
        let c = "0x3333333333333333333333333333333333333333333333333333333333333333";
        let ab = dm_conversation_id(a, b);
        let ba = dm_conversation_id(b, a);
        assert_eq!(ab, ba, "发起方向无关——双方看到同一个会话 id");
        assert!(ab.starts_with("dm-"), "dm- 前缀: {ab}");
        assert_eq!(ab.len(), "dm-".len() + 16, "短 hash（8 字节 hex）");
        assert_ne!(ab, dm_conversation_id(a, c), "不同对不碰撞");
        assert_ne!(
            dm_message_id(a, b, "hi", "t1"),
            dm_message_id(a, b, "hi", "t2"),
            "消息 id 含时间戳要素"
        );
    }

    // DM3. 本地投递全链：route=local、确定性 id、双方会话列表可见（对方发起
    //      的 DM 也可见——members 感知而非 created_by）、历史可读
    #[tokio::test]
    async fn dm_send_local_delivery_deterministic_conversation() {
        let h = empty_handler();
        let ((pa, ta), (pb, tb)) = dm_login_pair(&h).await;
        let resp = h
            .handle(authed_post(
                PATH_DM,
                &ta,
                serde_json::json!({"to_pubkey": pb, "content": "在吗？直接说"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "{}", resp.body);
        assert_eq!(resp.body["route"], "local");
        let cid = resp.body["conversation_id"].as_str().unwrap().to_string();
        assert_eq!(cid, dm_conversation_id(&pa, &pb), "确定性会话 id");
        assert_eq!(resp.body["message"]["sender_id"], pa, "sender=token 反查");
        assert_eq!(resp.body["message"]["content"], "在吗？直接说");
        assert_eq!(
            resp.body["message"]["read_by"],
            serde_json::json!([pa]),
            "发送者自己已读"
        );
        // 双方会话列表都含该 dm 会话（B 看得到 A 发起的——成员感知）
        for tok in [&ta, &tb] {
            let list = h.handle(authed_get(PATH_CONV_LIST, tok)).await.unwrap();
            let hit = list
                .body
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["id"] == serde_json::json!(cid))
                .unwrap_or_else(|| panic!("dm 会话应出现在列表"));
            let members = hit["members"].as_array().unwrap();
            assert!(members.contains(&serde_json::json!(pa)));
            assert!(members.contains(&serde_json::json!(pb)));
        }
        // 历史可读（B 视角 1 条）
        let hist = h
            .handle(authed_get(
                &format!("/api/v1/im/conversations/{cid}/messages"),
                &tb,
            ))
            .await
            .unwrap();
        assert_eq!(hist.status, 200);
        assert_eq!(hist.body.as_array().unwrap().len(), 1);
        // 再发一条 → 复用同一会话（不重复建行）
        let again = h
            .handle(authed_post(
                PATH_DM,
                &ta,
                serde_json::json!({"to_pubkey": pb, "content": "第二条"}),
            ))
            .await
            .unwrap();
        assert_eq!(again.status, 201);
        assert_eq!(
            again.body["conversation_id"].as_str().unwrap(),
            cid,
            "同对私信复用同一确定性会话"
        );
        let list = h.handle(authed_get(PATH_CONV_LIST, &ta)).await.unwrap();
        assert_eq!(
            list.body
                .as_array()
                .unwrap()
                .iter()
                .filter(|c| c["id"] == serde_json::json!(cid))
                .count(),
            1,
            "不重复建会话行"
        );
    }

    // DM4. 参数与开关闸门：dm_open=false → 403「对方未开放直通消息」；
    //      非法 pubkey / 给自己发 / 空正文 / 未知对方节点（无路由）→ 4xx
    #[tokio::test]
    async fn dm_send_gates_closed_switch_and_bad_requests() {
        let h = empty_handler();
        let ((pa, ta), (pb, tb)) = dm_login_pair(&h).await;
        let _ = tb;
        // 非法 to_pubkey
        let bad = h
            .handle(authed_post(
                PATH_DM,
                &ta,
                serde_json::json!({"to_pubkey": "not-a-pubkey", "content": "x"}),
            ))
            .await
            .unwrap();
        assert_eq!(bad.status, 400);
        // 给自己发
        let self_dm = h
            .handle(authed_post(
                PATH_DM,
                &ta,
                serde_json::json!({"to_pubkey": pa, "content": "x"}),
            ))
            .await
            .unwrap();
        assert_eq!(self_dm.status, 400);
        // 空正文
        let empty = h
            .handle(authed_post(
                PATH_DM,
                &ta,
                serde_json::json!({"to_pubkey": pb, "content": "   "}),
            ))
            .await
            .unwrap();
        assert_eq!(empty.status, 400);
        // 对方不在本节点且无路由（全新身份，从未在本节点出现）→ 404
        let fresh = pubkey_hex(&new_key());
        let no_route = h
            .handle(authed_post(
                PATH_DM,
                &ta,
                serde_json::json!({"to_pubkey": fresh, "content": "跨节点私信"}),
            ))
            .await
            .unwrap();
        assert_eq!(no_route.status, 404);
        assert!(
            no_route.body["error"]
                .as_str()
                .unwrap()
                .contains("不在本节点"),
            "404 应说明无路由: {}",
            no_route.body["error"]
        );
        // 关开关 → 本地投递被拒（对方=本节点身份，闸门即本节点 dm_open）
        h.federation().set_dm_open(false);
        let denied = h
            .handle(authed_post(
                PATH_DM,
                &ta,
                serde_json::json!({"to_pubkey": pb, "content": "在吗"}),
            ))
            .await
            .unwrap();
        assert_eq!(denied.status, 403);
        assert!(
            denied.body["error"]
                .as_str()
                .unwrap()
                .contains("未开放直通消息"),
            "403 文案: {}",
            denied.body["error"]
        );
        // 开关关不影响自己发往**已登记对端**……本用例无登记，重开后恢复 201
        h.federation().set_dm_open(true);
        let ok = h
            .handle(authed_post(
                PATH_DM,
                &ta,
                serde_json::json!({"to_pubkey": pb, "content": "在吗"}),
            ))
            .await
            .unwrap();
        assert_eq!(ok.status, 201, "重开后恢复本地投递");
    }

    // DM5. 定向推送形状：收发双方各收 1 帧 im_message（conversation_id=dm-*），
    //      无关订阅者收不到（绝不全员广播）
    #[tokio::test]
    async fn dm_local_push_is_targeted_not_broadcast() {
        let hub = WsHub::default();
        let h = ImRouteHandler::with_empty_ws(hub.clone(), Arc::new(ImAuth::default()));
        let ((pa, ta), (pb, _tb)) = dm_login_pair(&h).await;
        let (_pc, tc) = login(&h, &new_key()).await;
        let _ = h.handle(authed_get(PATH_LOBBY, &tc)).await.unwrap();
        // 心跳/欢迎广播之后再订阅（避免欢迎帧混入）
        let (_sa, mut rx_a) = hub.subscribe_raw(&pa);
        let (_sb, mut rx_b) = hub.subscribe_raw(&pb);
        let (_sc, mut rx_c) = hub.subscribe_raw(&_pc);
        let resp = h
            .handle(authed_post(
                PATH_DM,
                &ta,
                serde_json::json!({"to_pubkey": pb, "content": "定向私信"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        let cid = resp.body["conversation_id"].as_str().unwrap().to_string();
        for rx in [&mut rx_a, &mut rx_b] {
            let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("收发双方应各收到一帧")
                .expect("通道不应关闭");
            let v = serde_json::to_value(&frame).unwrap();
            assert_eq!(
                v["type"], "im_message",
                "复用 im_message 帧（前端零改动路由）"
            );
            assert_eq!(v["conversation_id"], serde_json::json!(cid));
            assert_eq!(v["message"]["content"], "定向私信");
        }
        let leaked = tokio::time::timeout(Duration::from_millis(300), rx_c.recv()).await;
        assert!(
            leaked.is_err(),
            "无关订阅者不得收到 DM（不广播）: {:?}",
            leaked.ok()
        );
    }

    // DM6. 可见性收口：非成员的会话列表看不到 dm-*；历史/补拉 403；
    //      dm 会话禁用通用发送端点（唯一入口 POST /im/dm，开关不旁路）
    #[tokio::test]
    async fn dm_visibility_members_only_and_endpoint_gated() {
        let h = empty_handler();
        let ((_pa, ta), (pb, _tb)) = dm_login_pair(&h).await;
        let (_pc, tc) = login(&h, &new_key()).await;
        let resp = h
            .handle(authed_post(
                PATH_DM,
                &ta,
                serde_json::json!({"to_pubkey": pb, "content": "私聊内容"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        let cid = resp.body["conversation_id"].as_str().unwrap().to_string();
        // C 的会话列表不含该 dm 会话（members 感知过滤）
        let list = h.handle(authed_get(PATH_CONV_LIST, &tc)).await.unwrap();
        assert!(
            !list
                .body
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["id"] == serde_json::json!(cid)),
            "非成员看不到 dm 会话"
        );
        // C 读历史 → 403；离线补拉 → 403
        let hist = h
            .handle(authed_get(
                &format!("/api/v1/im/conversations/{cid}/messages"),
                &tc,
            ))
            .await
            .unwrap();
        assert_eq!(hist.status, 403, "非参与者无权读取私信");
        let catchup = h
            .handle(authed_get(
                &format!("/api/v1/im/messages?conversation_id={cid}"),
                &tc,
            ))
            .await
            .unwrap();
        assert_eq!(catchup.status, 403, "补拉同款成员门");
        // 通用发送端点对 dm 会话禁用（dm_open/成员收口不旁路）
        let direct = h
            .handle(authed_post(
                &format!("/api/v1/im/conversations/{cid}/messages"),
                &ta,
                serde_json::json!({"content": "绕过开关"}),
            ))
            .await
            .unwrap();
        assert_eq!(direct.status, 400);
        assert!(
            direct.body["error"].as_str().unwrap().contains("/im/dm"),
            "应指引走 /im/dm: {}",
            direct.body["error"]
        );
    }

    // DM7. 跨节点端到端（双 handler + 双 P2P 节点 + 真 FederationBridge 分发）：
    //      A 节点 a 发给 B 节点 b（to_node 定向路由，非广播）→ B 侧 ingest 落
    //      dm-* 会话 + b 可见 + 消息 id=载荷 hash；
    //      随后 b 无 to_node 回信 → 按 im_dm_peers 登记自动路由回 A 节点
    #[tokio::test]
    async fn dm_cross_node_end_to_end_and_reply_routing() {
        use crate::handlers::p2p::FederationBridge;
        use os_p2p::{P2pConfig, P2pNode, Timing};
        let node_a = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            public: true,
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        let node_b = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            bootstrap: vec![node_a.listen_addr()],
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let peers = node_a.peers().await;
            if peers
                .iter()
                .any(|p| p.id == *node_b.self_id() && p.connected)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let h_a = empty_handler();
        h_a.federation().set_p2p(node_a.clone(), "node-a".into());
        let h_b = empty_handler();
        h_b.federation().set_p2p(node_b.clone(), "node-b".into());
        // a 只在 A 登录；b 只在 B 登录 + 大厅心跳（B 侧本地身份）
        let (pa, ta) = login(&h_a, &new_key()).await;
        let (pb, tb) = login(&h_b, &new_key()).await;
        let _ = h_b.handle(authed_get(PATH_LOBBY, &tb)).await.unwrap();
        let bridge_b = FederationBridge {
            im: Some(h_b.federation()),
            nexhub: None,
            live: None,
            api_market: None,
        };
        let mut rx_b_side = node_b.on_msg(); // 收 a→b 定向载荷（入站观测）
        let mut rx_a_side = node_a.on_msg(); // 收 b→a 回信载荷
                                             // a → b：显式 to_node 定向路由
        let resp = h_a
            .handle(authed_post(
                PATH_DM,
                &ta,
                serde_json::json!({
                    "to_pubkey": pb,
                    "content": "跨节点直通",
                    "to_node": node_b.self_id().to_hex(),
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "{}", resp.body);
        assert_eq!(resp.body["route"], "p2p");
        let cid_a = resp.body["conversation_id"].as_str().unwrap().to_string();
        assert_eq!(cid_a, dm_conversation_id(&pa, &pb), "发送侧同确定性 id");
        // B 节点收到 im_dm 定向载荷 → 经真 FederationBridge 分发进 ingest
        let got = tokio::time::timeout(Duration::from_secs(3), rx_b_side.recv())
            .await
            .expect("B 节点应收到 im_dm 载荷")
            .expect("通道存活");
        assert_eq!(got.payload["fed"], FED_KIND_IM_DM);
        assert_eq!(got.payload["from_pubkey"], serde_json::json!(pa));
        assert_eq!(got.payload["to_pubkey"], serde_json::json!(pb));
        bridge_b.dispatch(&got);
        // B 侧：b 的会话列表出现 dm- 会话（对端发起也可见）+ 消息落库
        let list_b = h_b.handle(authed_get(PATH_CONV_LIST, &tb)).await.unwrap();
        let hit = list_b
            .body
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == serde_json::json!(cid_a))
            .expect("B 侧应出现同 id 的 dm 会话");
        let members = hit["members"].as_array().unwrap();
        assert!(members.contains(&serde_json::json!(pa)));
        assert!(members.contains(&serde_json::json!(pb)));
        let hist_b = h_b
            .handle(authed_get(
                &format!("/api/v1/im/conversations/{cid_a}/messages"),
                &tb,
            ))
            .await
            .unwrap();
        assert_eq!(hist_b.status, 200);
        let arr = hist_b.body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["content"], "跨节点直通");
        assert_eq!(
            arr[0]["sender_id"],
            serde_json::json!(pa),
            "保留原始 pubkey（收件人可据此回信）"
        );
        assert!(
            arr[0]["sender_name"].as_str().unwrap().contains("node-a"),
            "来源标注: {}",
            arr[0]["sender_name"]
        );
        // —— b 回信：不带 to_node —— 按 ingest 登记的回程路由自动定向回 A 节点
        let reply = h_b
            .handle(authed_post(
                PATH_DM,
                &tb,
                serde_json::json!({"to_pubkey": pa, "content": "收到，回你"}),
            ))
            .await
            .unwrap();
        assert_eq!(reply.status, 201, "{}", reply.body);
        assert_eq!(reply.body["route"], "p2p", "按 im_dm_peers 登记自动路由");
        assert_eq!(
            reply.body["conversation_id"].as_str().unwrap(),
            cid_a,
            "回信同确定性会话"
        );
        let back = tokio::time::timeout(Duration::from_secs(3), rx_a_side.recv())
            .await
            .expect("A 节点应收到回信载荷")
            .expect("通道存活");
        assert_eq!(back.payload["fed"], FED_KIND_IM_DM);
        assert_eq!(back.payload["from_pubkey"], serde_json::json!(pb));
        assert_eq!(back.payload["to_pubkey"], serde_json::json!(pa));
        node_a.shutdown().await;
        node_b.shutdown().await;
    }

    // DM8. ingest 闸门与去重：对方 dm_open=false 丢弃；收件人不在本节点丢弃
    //      （错投）；同 msg_id 重投只落一份（Duplicate）
    #[tokio::test]
    async fn dm_ingest_gates_and_dedupe() {
        let h = empty_handler();
        let fed = h.federation();
        // NodeId = 33 字节压缩 secp256k1（0x+66hex）——用真密钥生成
        let from_node =
            os_p2p::NodeId::parse(&pubkey_hex(&new_key())).expect("真公钥应可解析为 NodeId");
        let sender = pubkey_hex(&new_key());
        let ((_, _ta), (pb, _tb)) = dm_login_pair(&h).await;
        let payload = |to: &str, msg_id: &str| {
            serde_json::json!({
                "fed": FED_KIND_IM_DM,
                "msg_id": msg_id,
                "from_pubkey": sender,
                "to_pubkey": to,
                "content": "跨节点私信",
                "node": "node-x",
                "ts": "2026-08-30T10:00:00Z",
            })
        };
        // 非法载荷（缺字段 / 非 im_dm kind）→ Ignored
        assert_eq!(
            fed.ingest_dm(&from_node, &serde_json::json!({"fed": "im_lobby"})),
            ImFedIngest::Ignored
        );
        assert_eq!(
            fed.ingest_dm(
                &from_node,
                &serde_json::json!({"fed": FED_KIND_IM_DM, "to_pubkey": pb})
            ),
            ImFedIngest::Ignored,
            "缺 from_pubkey → Ignored"
        );
        // 收件人不在本节点（错投）→ Ignored
        let fresh = pubkey_hex(&new_key());
        assert_eq!(
            fed.ingest_dm(&from_node, &payload(&fresh, "dm-msg-miss")),
            ImFedIngest::Ignored,
            "错投（收件人非本节点身份）丢弃"
        );
        // 正常落地 → Written
        assert_eq!(
            fed.ingest_dm(&from_node, &payload(&pb, "dm-msg-1")),
            ImFedIngest::Written
        );
        // 同 msg_id 重投 → Duplicate（内存缓存路径）
        assert_eq!(
            fed.ingest_dm(&from_node, &payload(&pb, "dm-msg-1")),
            ImFedIngest::Duplicate
        );
        // DB 兜底路径：绕过缓存直接插同 id → 仍 Duplicate（清空缓存后重投）
        {
            let mut seen = h.shared.fed_seen.lock().unwrap();
            seen.clear();
        }
        assert_eq!(
            fed.ingest_dm(&from_node, &payload(&pb, "dm-msg-1")),
            ImFedIngest::Duplicate,
            "DB 查重兜底（重启后缓存为空）"
        );
        // 关开关 → 丢弃（Ignored，不落库不推送）
        fed.set_dm_open(false);
        assert_eq!(
            fed.ingest_dm(&from_node, &payload(&pb, "dm-msg-2")),
            ImFedIngest::Ignored,
            "对方未开放直通消息 → 丢弃"
        );
        let msgs = h
            .messages_snapshot()
            .into_iter()
            .filter(|m| is_dm_conversation(&m.conversation_id))
            .collect::<Vec<_>>();
        assert_eq!(msgs.len(), 1, "只落了第一条（去重 + 关开关不落）");
        assert_eq!(msgs[0].id, "dm-msg-1");
    }

    // DM9. 联邦大厅发送方 DM 路由登记（register_fed_sender_route）：登记后
    //      对远端身份发起 DM 无需 to_node（跨节点私信从联邦大厅即可发起）
    #[tokio::test]
    async fn dm_fed_lobby_sender_route_registration() {
        let h = empty_handler();
        let fed = h.federation();
        let from_node =
            os_p2p::NodeId::parse(&pubkey_hex(&new_key())).expect("真公钥应可解析为 NodeID");
        let sender = pubkey_hex(&new_key());
        // 联邦大厅载荷（发送方=远端身份）→ 登记路由
        fed.register_fed_sender_route(
            &from_node,
            &serde_json::json!({
                "fed": FED_KIND_IM_FED_LOBBY,
                "node": "node-x",
                "message": {"sender_id": sender, "sender_name": "远端同学", "content": "hi"},
            }),
        );
        // 非 0x 身份（系统/agent）不登记
        fed.register_fed_sender_route(
            &from_node,
            &serde_json::json!({
                "fed": FED_KIND_IM_FED_LOBBY,
                "node": "node-x",
                "message": {"sender_id": "system", "content": "欢迎"},
            }),
        );
        // 本节点身份发起 DM：对方不在本节点，但已登记路由 → 走 p2p 定向
        //（P2P 未装配 → 503，恰好证明路由解析成功而非 404 无路由）
        let (_me, token) = login(&h, &new_key()).await;
        let resp = h
            .handle(authed_post(
                PATH_DM,
                &token,
                serde_json::json!({"to_pubkey": sender, "content": "从联邦大厅私聊你"}),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status, 503,
            "登记路由应被解析（503=P2P 未启用，非 404 无路由）"
        );
        let fresh = pubkey_hex(&new_key());
        let resp = h
            .handle(authed_post(
                PATH_DM,
                &token,
                serde_json::json!({"to_pubkey": fresh, "content": "无路由"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404, "未登记的远端身份仍无路由");
    }
}
