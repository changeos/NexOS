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

// ----------------------------------------------------------------------------
// 域子模块（2026-09-25 大文件拆分批，方法论同 film_hub v0.1.42 域子模块模式）：
// 原 ~11.9k 行单文件按域拆为 handlers/im/ 目录——本 mod.rs 保留核心（数据
// 模型/链上认证/handler 构造与共享内核/RouteHandler 路由表与 handle 分发/
// 共享管道与 SQLite 建库），各域子模块承载专属数据层与逻辑（纯搬运，零行为
// 变化）。外部引用面（crate::handlers::im::* 与 handlers/mod.rs 的 pub use）
// 经下方私有 glob use + 显式 pub use 重导出零改动；测试整体迁 im/tests.rs。
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests;

mod attachments;
mod dm_groups;
mod federated;
mod sessions;
mod stats;
mod webhooks;

use attachments::*;
use dm_groups::*;
use federated::*;
use sessions::*;
use stats::*;
use webhooks::*;

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

/// IM DB 短事务统一入口（审计 A1-1/Top1 批 1，2026-09-25，docs/research/
/// AUDIT_OPTIMIZATION_2026-09.md §F#1）：把「async 上下文里直接
/// `shared.db.lock()` + 同步 SQLite IO」挪进 `tokio::task::spawn_blocking`
/// ——IM 是最热的 WS+REST 混合面，原先历史拉取/消息写入排队期间整个
/// tokio worker 不可 poll 其他连接；现在 async 任务只 `.await` 结果。
///
/// 锁语义与原先完全一致（详见 [`ImRouteHandler::db_call`]）：同一把组件级
/// 互斥锁、同一条连接、短锁快放不跨 `.await`；闭包 panic 原样上抛。
/// 供 [`ImRouteHandler::db_call`]（REST 面）与持有 `Arc<ImShared>` 的
/// spawn 任务（内置助手回复）共用。
async fn im_db_call<T, F>(shared: Arc<ImShared>, f: F) -> T
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> T + Send + 'static,
{
    match tokio::task::spawn_blocking(move || {
        let conn = shared.db.lock().expect("db poisoned");
        f(&conn)
    })
    .await
    {
        Ok(v) => v,
        Err(join_err) => std::panic::resume_unwind(join_err.into_panic()),
    }
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

    /// IM DB 短事务统一入口（审计 A1-1/Top1 批 1，2026-09-25）：闭包在
    /// `spawn_blocking` 线程里拿锁执行——**锁等待与 SQLite 同步 IO 全部离开
    /// tokio worker**，async 任务只 `.await` 结果。
    ///
    /// 锁语义与原先「async 里直接 `shared.db.lock()`」完全一致：
    /// - 同一把组件级互斥锁、同一条连接（WAL）——读写仍互斥串行，原子性
    ///   边界不变（不引连接池、不改锁结构，rusqlite `Connection` 非 `Sync`
    ///   仍单连接共享）；
    /// - 短锁快放：闭包返回即解锁，绝不跨 `.await` 持锁；
    /// - panic 面一致：锁中毒/闭包 panic 经 `resume_unwind` 原样上抛（等价
    ///   原 `expect("db poisoned")`），不静默吞错。
    async fn db_call<T, F>(&self, f: F) -> T
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> T + Send + 'static,
    {
        im_db_call(Arc::clone(&self.shared), f).await
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

// 对外面（原 im.rs 顶层 pub 项随域搬家后在此重导出——路径/可见性零变化）。
pub use attachments::{
    im_file_url, sanitize_im_filename, Attachment, ImFileDownload, ImFileRecord,
};
pub use dm_groups::{dm_conversation_id, dm_message_id, Group, GroupMember, DM_CONV_PREFIX};
pub use federated::{
    build_im_fed_lobby_payload, build_im_lobby_fed_payload, build_lobby_post_payload,
    lobby_message_federable, ImFedIngest, ImFederation, ImLobbyProbe, LobbyViewMessage,
    RemoteLobbyView, FED_KIND_IM_DM, FED_KIND_IM_FED_LOBBY, FED_KIND_IM_LOBBY,
    FED_KIND_IM_LOBBY_POST, FED_KIND_IM_LOBBY_QUERY, FED_KIND_IM_LOBBY_REPLY, FED_SENDER_PREFIX,
    FED_THROTTLE_LONG, FED_THROTTLE_SHORT, LOBBY_QUERY_TTL, LOBBY_VIEW_LIMIT,
};
pub use sessions::{
    build_welcome_message, parse_mentions, strip_mentions, truncate_chars, LobbyInfo, LobbyMember,
    NEXOS_ASSISTANT,
};
pub use stats::{is_online, ImStatus, Peer};
pub use webhooks::{
    is_valid_webhook_url, webhook_event_name, webhook_matches, ImWebhook,
    NOTIFY_EVENT_CONVERSATION, NOTIFY_EVENT_LOBBY,
};

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
                let pubkey = caller.pubkey.clone();
                let list = self
                    .db_call(move |conn| {
                        load_all_conversations(conn)
                            .unwrap_or_default()
                            .into_iter()
                            // DM 会话 members 感知：只列自己是成员的（对方发起的
                            // 私信也可见——判定依据是 im_dm_members 而非 created_by，
                            // 2026-08-30 DM 批次）；普通对话沿用全员可见现状。
                            .filter(|c| {
                                !is_dm_conversation(&c.id) || dm_is_member(conn, &c.id, &pubkey)
                            })
                            .collect::<Vec<_>>()
                    })
                    .await;
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
                let conv_for_db = conv.clone();
                self.db_call(move |conn| insert_conversation(conn, &conv_for_db))
                    .await?;
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
                let pubkey = caller.pubkey.clone();
                let list = self
                    .db_call(move |conn| {
                        if is_dm_conversation(&cid) && !dm_is_member(conn, &cid, &pubkey) {
                            return None;
                        }
                        Some(load_messages_by_conversation(conn, &cid).unwrap_or_default())
                    })
                    .await;
                let list = match list {
                    Some(l) => l,
                    None => return Ok(error_response(403, "非直通消息参与者，无权读取该会话")),
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
                    let cid = cid.clone();
                    self.db_call(move |conn| Self::conversation_exists(conn, &cid))
                        .await
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
                let attachment = match self.verify_attachment(body.attachment.as_ref()).await {
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
                // 消息写入走 blocking 池（高频写：IM 消息路径，A1-1 热点）
                let msg_for_db = msg.clone();
                self.db_call(move |conn| insert_message(conn, &msg_for_db))
                    .await?;
                // 广播到 WebSocket（实时推送；message 体携带全部新字段）
                Self::broadcast_conversation(&self.shared.ws_hub, &cid, &msg);
                // agent 协调：@ 定向投递（agent-coord 组件钩子，未装配时 no-op）
                crate::handlers::agent_coord::on_im_message(&msg);
                // @NexOS助手 → 内置助手异步回复（防风暴去抖，agent 消息不触发）
                self.maybe_spawn_assistant(&msg);
                // 推送通知：匹配的注册 webhook 异步 POST（不阻塞本响应）
                self.shared.dispatch_webhooks(&msg).await;
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
                    let cid = cid.clone();
                    let pubkey = caller.pubkey.clone();
                    self.db_call(move |conn| Self::conversation_readable(conn, &cid, &pubkey))
                        .await
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
                        let cid = cid.clone();
                        self.db_call(move |conn| {
                            load_messages_after(conn, &cid, after_id.as_deref(), limit)
                                .unwrap_or_default()
                        })
                        .await
                    }
                };
                Ok(ok_json(to_value(&list)?))
            }

            // —— GET /api/v1/im/groups —— 列出群组（聚合 members + last_activity）
            (HttpMethod::Get, ["api", "v1", "im", "groups"]) => {
                let Some(_caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let list = self
                    .db_call(|conn| load_all_groups(conn).unwrap_or_default())
                    .await;
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
                    let group_for_db = group.clone();
                    self.db_call(move |conn| {
                        insert_group(conn, &group_for_db)?;
                        for (uid, role) in members.iter().map(|uid| {
                            let role = if *uid == owner { "owner" } else { "member" };
                            (uid, role)
                        }) {
                            let _ = insert_group_member(conn, &gid, uid, role, &now);
                        }
                        Ok::<(), rusqlite::Error>(())
                    })
                    .await?;
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
                    let gid = (*id).to_string();
                    match self.db_call(move |conn| find_group(conn, &gid)).await? {
                        Some(g) => g,
                        None => return Ok(error_response(404, &format!("群组不存在: {id}"))),
                    }
                };
                {
                    let gid = (*id).to_string();
                    let pubkey = caller.pubkey.clone();
                    let known_members = group.members.clone();
                    let joined = self
                        .db_call(move |conn| {
                            if !known_members.contains(&pubkey) {
                                let _ =
                                    insert_group_member(conn, &gid, &pubkey, "member", &now_iso());
                                return true;
                            }
                            false
                        })
                        .await;
                    if joined {
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
                    let gid = (*id).to_string();
                    match self.db_call(move |conn| find_group(conn, &gid)).await? {
                        Some(g) => g,
                        None => return Ok(error_response(404, &format!("群组不存在: {id}"))),
                    }
                };
                {
                    let gid = (*id).to_string();
                    let pubkey = caller.pubkey.clone();
                    self.db_call(move |conn| {
                        let _ = remove_group_member(conn, &gid, &pubkey);
                    })
                    .await;
                    group.members.retain(|m| *m != caller.pubkey);
                }
                Ok(ok_json(to_value(&group)?))
            }

            // —— GET /api/v1/im/groups/:id/members —— 群组成员
            (HttpMethod::Get, ["api", "v1", "im", "groups", id, "members"]) => {
                let Some(_caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let gid = (*id).to_string();
                let group = self.db_call(move |conn| find_group(conn, &gid)).await?;
                let Some(group) = group else {
                    return Ok(error_response(404, &format!("群组不存在: {id}")));
                };
                let (name, members) = (group.name, group.members);
                Ok(ok_json(serde_json::json!({
                    "group_id": id,
                    "name": name,
                    "members": members,
                })))
            }

            // —— GET /api/v1/im/peers —— 已连接的 Federation 节点
            (HttpMethod::Get, ["api", "v1", "im", "peers"]) => {
                let list = self
                    .db_call(|conn| load_all_peers(conn).unwrap_or_default())
                    .await;
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
                    let peer_for_db = peer.clone();
                    self.db_call(move |conn| insert_peer(conn, &peer_for_db))
                        .await?;
                }
                Ok(ApiResponse {
                    status: 201,
                    body: to_value(&peer)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/im/status —— IM 服务状态
            (HttpMethod::Get, ["api", "v1", "im", "status"]) => {
                // stats 聚合走 blocking 池（A1-1 点名的重查询类）
                let status = self
                    .db_call(|conn| {
                        let conversations = count_rows(conn, "im_conversations");
                        let groups = count_rows(conn, "im_groups");
                        let peers = count_rows(conn, "im_peers");
                        let messages = count_rows(conn, "im_messages");
                        ImStatus {
                            ready: true,
                            conversations,
                            groups,
                            peers,
                            messages,
                        }
                    })
                    .await;
                Ok(ok_json(to_value(&status)?))
            }

            // —— POST /api/v1/im/messages/:id/read —— 标记消息已读
            //    已读人 = token 反查 pubkey（自报 user_id 一律忽略）
            (HttpMethod::Post, ["api", "v1", "im", "messages", id, "read"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let mid = (*id).to_string();
                let pubkey = caller.pubkey.clone();
                let outcome = self
                    .db_call(move |conn| {
                        let mut msg = match find_message(conn, &mid)? {
                            Some(m) => m,
                            None => return Ok::<Option<Message>, rusqlite::Error>(None),
                        };
                        if !msg.read_by.contains(&pubkey) {
                            msg.read_by.push(pubkey.clone());
                            update_message_read_by(conn, &msg)?;
                        }
                        Ok(Some(msg))
                    })
                    .await?;
                let msg = match outcome {
                    Some(m) => m,
                    None => return Ok(error_response(404, &format!("消息不存在: {id}"))),
                };
                Ok(ok_json(to_value(&msg)?))
            }

            // —— GET /api/v1/im/conversations/:id/unread —— 对话未读消息数
            //    user = token 反查 pubkey（查询参数 ?user= 一律忽略）
            (HttpMethod::Get, ["api", "v1", "im", "conversations", id, "unread"]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let unread = {
                    let cid = (*id).to_string();
                    let pubkey = caller.pubkey.clone();
                    self.db_call(move |conn| count_unread(conn, &cid, &pubkey))
                        .await
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
                    let cid = cid.clone();
                    let pubkey = caller.pubkey.clone();
                    self.db_call(move |conn| Self::conversation_readable(conn, &cid, &pubkey))
                        .await
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
                        let cid = cid.clone();
                        let q_for_db = q.clone();
                        self.db_call(move |conn| {
                            search_messages(conn, &cid, &q_for_db, limit).unwrap_or_default()
                        })
                        .await
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
                    let pubkey = caller.pubkey.clone();
                    let display_name = caller.display_name.clone();
                    let hub = self.shared.ws_hub.clone();
                    self.db_call(move |conn| {
                        // 心跳 touch；首次进入自动加入 + 系统欢迎广播
                        if upsert_lobby_member(conn, &pubkey, &display_name) {
                            let welcome = build_welcome_message(&display_name);
                            let _ = insert_message(conn, &welcome);
                            Self::broadcast_lobby(&hub, &welcome);
                        }
                        lobby_info(conn)
                    })
                    .await
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
                    let pubkey = caller.pubkey.clone();
                    let display_name = caller.display_name.clone();
                    self.db_call(move |conn| {
                        // 心跳 touch（新用户静默加入，欢迎消息仅由 GET /lobby 触发）
                        let _ = upsert_lobby_member(conn, &pubkey, &display_name);
                        load_recent_lobby_messages(conn, LOBBY_RECENT_LIMIT, after_id.as_deref())
                            .unwrap_or_default()
                    })
                    .await
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
                    let pubkey = caller.pubkey.clone();
                    self.db_call(move |conn| lobby_is_member(conn, &pubkey))
                        .await
                };
                if !is_member {
                    return Ok(error_response(
                        403,
                        "尚未加入大厅（先 GET /api/v1/im/lobby 自动加入）",
                    ));
                }
                let attachment = match self.verify_attachment(body.attachment.as_ref()).await {
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
                    let msg_for_db = msg.clone();
                    self.db_call(move |conn| insert_message(conn, &msg_for_db))
                        .await?;
                }
                Self::broadcast_lobby(&self.shared.ws_hub, &msg);
                // 我的大厅与联邦完全隔离（2026-08-23 用户纠正）：消息只留本节点，
                // 不再自动联邦广播——跨节点发言走 POST /api/v1/im/fed-lobby/messages
                // agent 协调：@ 定向投递（agent-coord 组件钩子，未装配时 no-op）
                crate::handlers::agent_coord::on_im_message(&msg);
                // @NexOS助手 → 内置助手异步回复（防风暴去抖，agent 消息不触发）
                self.maybe_spawn_assistant(&msg);
                // 推送通知：匹配的注册 webhook 异步 POST（不阻塞本响应）
                self.shared.dispatch_webhooks(&msg).await;
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
                let members = self
                    .db_call(|conn| load_lobby_members(conn).unwrap_or_default())
                    .await;
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
                    let record_for_db = record.clone();
                    self.db_call(move |conn| insert_file_record(conn, &record_for_db))
                        .await?;
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
                let fid = (*file_id).to_string();
                let record = {
                    let fid = fid.clone();
                    self.db_call(move |conn| find_file_record(conn, &fid))
                        .await?
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
                        let cid = cid.to_string();
                        let pubkey = caller.pubkey.clone();
                        self.db_call(move |conn| Self::conversation_readable(conn, &cid, &pubkey))
                            .await
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
                    let hook_for_db = hook.clone();
                    self.db_call(move |conn| insert_webhook(conn, &hook_for_db))
                        .await?;
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
                    let pubkey = caller.pubkey.clone();
                    self.db_call(move |conn| {
                        load_webhooks_by_owner(conn, &pubkey).unwrap_or_default()
                    })
                    .await
                };
                Ok(ok_json(to_value(&list)?))
            }

            // —— DELETE /api/v1/im/notify/:id —— 注销 webhook（IM token，仅 owner）
            //    他人注销 → 403（注册表不动）；未知 id → 404
            (HttpMethod::Delete, ["api", "v1", "im", "notify", id]) => {
                let Some(caller) = self.caller(&req) else {
                    return Ok(auth_required());
                };
                let wid = (*id).to_string();
                let hook = {
                    let wid = wid.clone();
                    self.db_call(move |conn| find_webhook(conn, &wid)).await?
                };
                let Some(hook) = hook else {
                    return Ok(error_response(404, &format!("webhook 不存在: {id}")));
                };
                if hook.owner_pubkey != caller.pubkey {
                    return Ok(error_response(403, "仅 owner 可注销该 webhook"));
                }
                {
                    let wid = wid.clone();
                    self.db_call(move |conn| delete_webhook(conn, &wid)).await?;
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
                    let pubkey = caller.pubkey.clone();
                    let display_name = caller.display_name.clone();
                    self.db_call(move |conn| {
                        // 心跳 touch（加入联邦大厅＝加入本节点 IM 在场；联邦大厅
                        // 无欢迎系统消息——它是跨节点频道，不属于任何单节点事件）
                        let _ = upsert_lobby_member(conn, &pubkey, &display_name);
                        fed_lobby_info(conn)
                    })
                    .await
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
                    let pubkey = caller.pubkey.clone();
                    let display_name = caller.display_name.clone();
                    self.db_call(move |conn| {
                        // 心跳 touch（与 GET /fed-lobby 同语义：加入即在场）
                        let _ = upsert_lobby_member(conn, &pubkey, &display_name);
                        load_recent_conversation_messages(
                            conn,
                            FED_LOBBY_ID,
                            LOBBY_RECENT_LIMIT,
                            after_id.as_deref(),
                        )
                        .unwrap_or_default()
                    })
                    .await
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
                    let pubkey = caller.pubkey.clone();
                    self.db_call(move |conn| lobby_is_member(conn, &pubkey))
                        .await
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
                    let msg_for_db = msg.clone();
                    self.db_call(move |conn| insert_message(conn, &msg_for_db))
                        .await?;
                }
                Self::broadcast_fed_lobby(&self.shared.ws_hub, &msg);
                // P2P 联邦广播（2026-08-24 起带时延节流）：本地落库 + WS 广播
                // 上方已**即时**完成（本节点体验不变）；联邦广播经延迟队列
                // 到期发出（常态 10s，同一 sender 60s 内多次发言升至 60s——
                // 不限次、不拒绝、不丢消息，仅延后广播时刻）
                let fed_delay = self.federation().enqueue_fed_lobby_broadcast(&msg);
                // 推送通知：匹配的注册 webhook 异步 POST（不阻塞本响应）
                self.shared.dispatch_webhooks(&msg).await;
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
                    let shared = Arc::clone(&self.shared);
                    let to = to.clone();
                    self.db_call(move |conn| shared.identity_local(conn, &to))
                        .await
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
                        let shared = Arc::clone(&self.shared);
                        let cid = cid.clone();
                        let from = caller.pubkey.clone();
                        let to_peer = to.clone();
                        let msg_for_db = msg.clone();
                        self.db_call(move |conn| {
                            shared.ensure_dm_conversation(conn, &cid, &from, &to_peer);
                            insert_message(conn, &msg_for_db)
                        })
                        .await?;
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
                        let to = to.clone();
                        self.db_call(move |conn| lookup_dm_peer_node(conn, &to))
                            .await
                            .and_then(|n| os_p2p::NodeId::parse(&n))
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
                    let shared = Arc::clone(&self.shared);
                    let cid = cid.clone();
                    let from = caller.pubkey.clone();
                    let to_peer = to.clone();
                    let msg_for_db = msg.clone();
                    self.db_call(move |conn| {
                        shared.ensure_dm_conversation(conn, &cid, &from, &to_peer);
                        // 已存在（回环）按已发处理
                        let _ = insert_message(conn, &msg_for_db);
                    })
                    .await;
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
    let _ = conn.busy_timeout(std::time::Duration::from_millis(3000)); // 防 SQLITE_BUSY 立败（审计 E#6）
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
