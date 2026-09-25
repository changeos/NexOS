//! `AgentCoordRouteHandler` —— agent 协调组件（组件名 `agent-coord`）：
//! agent 注册表 + IM 群消息 @ 定向投递 + 收件箱/确认 + 协作协议声明。
//!
//! 设计来源：NexHub nexos-test 仓库 README §2（测试 agent 诊断 IM 当任务通道
//! 缺「定向路由 / 可靠投递 / 在线状态 / 任务状态机」四项能力——本组件补齐
//! 前三项对应的最小闭环，任务状态机与 webhook 重试留待下期）。
//!
//! 用户需求原话：「做 agent 协调组件，当 agent 被 at 时给对应的 agent 通讯，
//! 并申明与 agent 交流时要 at 对方」。拆成两条规矩：
//!
//! 1. **@ 即定向投递**：群里 `@<agent 名字>` 不只靠群广播——协调层为每个
//!    被命中的注册 agent 生成一条投递记录：agent 的 WS 在线（其 pubkey 在
//!    WS hub 有**新鲜**订阅——最后客户端活动距今 ≤ 阈值，见
//!    [`DEFAULT_ONLINE_STALE_SECS`]；半开/僵死连接的订阅残留不再误判在线）
//!    → 标 `delivered=ws`（WS 广播本身已送达）；
//!    离线 → 可选 webhook 回调（成功升级 `webhook`）；
//! 2. **双写收件箱（不丢 @）**：无论在线离线，投递记录一律进收件箱
//!    （`inbox`）——在线标记 `delivered=ws`、离线标记 `delivered=inbox`。
//!    WS 在线只是「即时送达」的旁证不是凭据：新鲜度窗口内强杀客户端
//!    （kill -9 / 断电，无 close 帧）订阅残留仍判在线，纯 ws 投递无人消费
//!    即丢（nexos-test BUG-agentcoord ④）——收件箱同步留痕后，重启经
//!    `GET inbox` 仍可回看。ack 语义不变：消费者处理完应 ack（置
//!    `acked_at`），默认拉取只回未读；`include_acked=1` 显式开历史；
//! 3. **协议声明**：与 agent 交流必须 @ 对方，未 @ 的消息 agent 不认领——
//!    协议文本经 `GET /api/v1/agents/protocol` 供 agent 自举读取；agent
//!    注册时自动向它加入的所有群组发一条系统声明消息（同群只发一次）。
//!
//! # 与 im 组件的耦合（最小侵入 + 注入式）
//!
//! - **出向**（im → agent-coord）：im.rs 在消息落库 + WS 广播后**一行**调用
//!   [`on_im_message`]（本模块进程级钩子，main.rs 装配时 [`install_hook`]
//!   注入，未装配时 no-op）；
//! - **入向**（agent-coord → im）：声明消息直插 `im_messages` + WS 广播、
//!   按 pubkey 查群组成员，经 [`crate::handlers::im::ImCoordBridge`] 句柄
//!   注入（im.rs 提供的轻量桥，handler 间不直接持引用）。
//!
//! # 持久化
//!
//! JSON 文件（env `NEXOS_AGENTS_FILE`，缺省 `/tank/os-data/agents.json`），
//! 目录不存在自动创建，**原子写**（先写 `.tmp` 再 rename，update.rs 同款）。
//! 内容：agent 注册表（name 唯一键 + pubkey + callback_url + 已声明群组）+
//! 投递收件箱（递增 seq）。读取缺失/损坏 → 空表降级（不阻塞启动）。
//!
//! # 鉴权
//!
//! **开发期全部公开**（requires_auth=false，跟随开发期 add-peer 惯例）——
//! 发版前收紧：register/delete 收 admin，inbox/ack 收 agent 侧 token。
//!
//! # 路由表（6 条，component="agent-coord"）
//!
//! | method | path                                | 动作 |
//! |--------|-------------------------------------|------|
//! | POST   | `/api/v1/agents/register`           | 注册/更新 agent（三级语义：admin 覆盖 / 同 pubkey 幂等 / 异 pubkey 409；触发协议声明）|
//! | GET    | `/api/v1/agents`                    | 列表（带 online / callback 脱敏 / 注册时间）|
//! | GET    | `/api/v1/agents/protocol`           | 协作协议文本（agent 自举读取）|
//! | GET    | `/api/v1/agents/:name/inbox?after=` | 收件箱增量拉取（升序，新在后；默认只回未读，`include_acked=1` 开历史）|
//! | POST   | `/api/v1/agents/:name/ack`          | 确认已读（历史保留）|
//! | DELETE | `/api/v1/agents/:name`              | 注销（连带收件箱）|

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};
use crate::handlers::im::{self, ImCoordBridge, Message};
use crate::ws_impl::WsHub;

// ----------------------------------------------------------------------------
// 常量
// ----------------------------------------------------------------------------

/// 组件名（路由注册用）。
const COMPONENT: &str = "agent-coord";
/// 注册表/收件箱持久化缺省路径（env `NEXOS_AGENTS_FILE` 覆盖）。
pub const DEFAULT_AGENTS_FILE: &str = "/tank/os-data/agents.json";
/// agent 名字规则：`[a-z0-9-]`，1..=32 字符（im @mention 名字字符集的
/// ASCII 子集——保证 `@<name>` 一定能被 im.rs 的 parse_mentions 解析出）。
const NAME_MAX_CHARS: usize = 32;
/// webhook 单次投递超时（超时/失败仅记日志不重试——重试下期）。
const WEBHOOK_HTTP_TIMEOUT: Duration = Duration::from_secs(3);
/// 投递事件名（webhook body 的 `type` 字段 + `X-NexOS-Event` 头）。
const EVENT_AGENT_MENTION: &str = "agent_mention";
/// 收件箱 content 摘要字符上限（完整消息 JSON 走 webhook，收件箱只留摘要）。
const INBOX_CONTENT_MAX_CHARS: usize = 120;
/// 投递状态：WS 在线（广播本身已送达，记录留痕）。
const DELIVERED_WS: &str = "ws";
/// 投递状态：离线 + webhook 回调成功。
const DELIVERED_WEBHOOK: &str = "webhook";
/// 投递状态：离线仅收件箱（无 callback_url，或回调未成/未试）。
const DELIVERED_INBOX: &str = "inbox";
/// online 判定新鲜度阈值缺省（秒）：订阅最后客户端活动距今超过阈值视为
/// 过期（online=false）——半开/僵死 WS 连接的订阅残留兜底（dev-standby
/// 静默失联期间仍显示在线的遗留 bug，见 BUG-dev-standby-ws-silent-drop.md
/// 「关联」段）。120 = 客户端 ping_interval 25s 的 ~5 倍余量。
const DEFAULT_ONLINE_STALE_SECS: u64 = 120;
/// online 判定新鲜度阈值 env 名（u64 秒；缺失/非法/0 → 回落缺省，见
/// [`parse_online_stale_secs`]；docs/AGENT_COORDINATION.md §5）。
const ENV_ONLINE_STALE_SECS: &str = "NEXOS_AGENTS_ONLINE_STALE_SECS";

/// 解析新鲜度阈值 env（纯函数）：u64 秒且 ≥1（0 会把一切订阅即时判过期，
/// 视为非法）；缺失/解析失败/0 → [`DEFAULT_ONLINE_STALE_SECS`]。
#[must_use]
pub fn parse_online_stale_secs(raw: Option<&str>) -> u64 {
    raw.and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|v| *v >= 1)
        .unwrap_or(DEFAULT_ONLINE_STALE_SECS)
}

/// 协作协议全文（`GET /api/v1/agents/protocol` 的 `protocol` 字段；
/// 群声明消息与文档同源，改协议只改这一处）。
pub const PROTOCOL_TEXT: &str = "NexOS agent 协作协议：与 agent 交流必须 @对方（@<name>）。\
未 @ 的消息 agent 不认领。@ 即定向投递：在线 WS 即时送达（收件箱同步留痕，\
消费后应及时 ack——默认拉取只回未读），离线进收件箱\
（GET /api/v1/agents/<name>/inbox?after=）+ 可选 webhook。执行完成回帖群内并引用原任务。";

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 注册表条目（agents.json 的 `agents[]` 行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEntry {
    /// 唯一键：`[a-z0-9-]`，≤32 字符。
    pub name: String,
    /// 链上身份（`0x` + 66 hex 压缩 secp256k1 公钥；None = 未绑定——无法判
    /// 在线/发声明，仅收件箱投递）。
    #[serde(default)]
    pub pubkey: Option<String>,
    /// 可选 webhook 接收端点（http/https；离线 @ 的定向回调目标）。
    #[serde(default)]
    pub callback_url: Option<String>,
    /// 已发过协议声明的群组 id（同 agent 同群只声明一次）。
    #[serde(default)]
    pub declared_groups: Vec<String>,
    /// 注册时间（RFC3339）。
    pub created_at: String,
    /// 最近更新时间（重复注册覆盖时刷新）。
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// `GET /api/v1/agents` 列表项（callback_url 脱敏 + 派生 online）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentListItem {
    pub name: String,
    #[serde(default)]
    pub pubkey: Option<String>,
    /// 该 agent 的 pubkey 在 WS hub 有**新鲜**订阅即 true（最后客户端活动
    /// ≤ 阈值内，env `NEXOS_AGENTS_ONLINE_STALE_SECS`，缺省 120s）；
    /// 无 pubkey 恒 false。
    pub online: bool,
    /// 脱敏后的 callback_url（只留 scheme+host，路径/查询打码）。
    #[serde(default)]
    pub callback_url: Option<String>,
    /// 已声明群组数。
    pub declared_groups: usize,
    pub created_at: String,
}

/// 一条定向投递记录（收件箱行；`GET /:name/inbox` 的 `records[]` 项）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryRecord {
    /// 递增序号（收件箱游标：`?after=<seq>` 增量拉取 / ack 确认用）。
    pub seq: u64,
    /// 命中的 agent 名字。
    pub agent_name: String,
    /// 触发消息 id（im_messages.id）。
    pub message_id: String,
    /// 触发消息所在会话/群组 id（im_messages.conversation_id）。
    pub group_id: String,
    /// 消息正文摘要（≤120 字符；完整消息 JSON 走 webhook body）。
    pub content: String,
    /// `ws` | `webhook` | `inbox`（见模块注释投递三态）。
    pub delivered: String,
    /// 确认已读时间（`POST /:name/ack` 置位；None = 未读）。
    #[serde(default)]
    pub acked_at: Option<String>,
    pub created_at: String,
}

/// 落盘状态（agents.json 全量）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PersistState {
    /// agent 注册表（name 唯一）。
    #[serde(default)]
    agents: Vec<AgentEntry>,
    /// 全部投递记录（全 agent 共用递增 seq）。
    #[serde(default)]
    inbox: Vec<DeliveryRecord>,
    /// 下一个投递 seq（从 1 起）。
    #[serde(default)]
    next_seq: u64,
}

// ----------------------------------------------------------------------------
// 校验 / 派生（纯函数）
// ----------------------------------------------------------------------------

/// 校验 agent 名字（纯函数）：非空、≤32 字符、字符集 `[a-z0-9-]`。
#[must_use]
pub fn is_valid_agent_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= NAME_MAX_CHARS
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// 校验 pubkey（纯函数）：`0x` + 恰好 66 个 hex 字符（33 字节压缩公钥的
/// 格式校验；不做 secp256k1 点校验——协调层只拿它对 WS 订阅键精确匹配）。
#[must_use]
pub fn is_valid_pubkey(pk: &str) -> bool {
    match pk.strip_prefix("0x") {
        Some(hex) => hex.len() == 66 && hex.chars().all(|c| c.is_ascii_hexdigit()),
        None => false,
    }
}

/// callback_url 校验复用 im webhook 规则（http/https + 无空白 + ≤2048）。
#[must_use]
pub fn is_valid_callback_url(url: &str) -> bool {
    im::is_valid_webhook_url(url)
}

/// 脱敏 callback_url（纯函数）：保留 scheme://host，路径/查询一律 `***`
/// （列表展示用——完整 URL 只有注册者自己与投递路径知道）。
#[must_use]
pub fn mask_callback_url(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let host_end = rest.find('/').unwrap_or(rest.len());
            let (host, tail) = rest.split_at(host_end);
            if tail.is_empty() {
                format!("{scheme}://{host}")
            } else {
                format!("{scheme}://{host}/***")
            }
        }
        None => "***".to_string(),
    }
}

/// 构造协议声明消息文案（纯函数；注册时直插 agent 所在群组）。
#[must_use]
pub fn declaration_text(name: &str) -> String {
    format!(
        "📢【agent 协作协议】{name} 已注册。与它交流请 @{name}——\
被 @ 才会定向投递（在线 WS / 离线收件箱+webhook），未 @ 的消息 agent 不认领。"
    )
}

// ----------------------------------------------------------------------------
// CoordCore：注册表 + 投递内核（handler 与 im 钩子共用一份 Arc）
// ----------------------------------------------------------------------------

/// 协调内核：持久化状态 + 注入的 WS hub / IM 桥。
///
/// 独立成 `Arc` 的原因：im.rs 发消息路径经进程级钩子 [`on_im_message`]
/// 触达同一份状态（handler 实例被 `Box` 进网关后钩子仍持有内核句柄）。
pub struct CoordCore {
    /// 全量状态（短锁快放，不跨 `.await` 持锁）。
    state: Mutex<PersistState>,
    /// 持久化路径（None = 纯内存态，测试用）。
    state_path: Option<String>,
    /// 注入的 WS hub（在线判定：pubkey 有新鲜订阅即在线；None = 恒离线）。
    ws_hub: Option<WsHub>,
    /// 注入的 IM 桥（声明系统消息直插 + 群成员查询；None = 声明跳过）。
    im_bridge: Option<ImCoordBridge>,
    /// online 判定新鲜度阈值（构造时经 [`parse_online_stale_secs`] 从
    /// env `NEXOS_AGENTS_ONLINE_STALE_SECS` 读定；改动需重启进程）。
    online_stale: Duration,
}

impl CoordCore {
    /// 判定 agent 是否在线：pubkey 在 WS hub 的 by_user 表有订阅，**且**
    /// 订阅新鲜（最后客户端活动距今 ≤ `online_stale`——半开/僵死连接的
    /// 订阅残留不误判在线）。过期订阅不删除（重连/断开路径自会清理），
    /// 只影响本判定。
    fn online(&self, pubkey: &str) -> bool {
        self.ws_hub
            .as_ref()
            .is_some_and(|hub| hub.fresh_subscriber_count_for(pubkey, self.online_stale) > 0)
    }

    /// 持久化当前状态（路径 None 时空操作；失败打日志不阻塞请求）。
    fn persist(&self, st: &PersistState) {
        let Some(path) = &self.state_path else { return };
        if let Err(e) = persist_state_to(path, st) {
            eprintln!("[agent-coord] 状态落盘失败 {path}: {e}");
        }
    }

    /// 注册/更新 agent（幂等：同 name 同 pubkey 覆盖 pubkey/callback_url，
    /// **保留 declared_groups**——重复注册不重发声明）。返回
    /// `(更新后的条目, 是否新建, 本次声明群组数)`。
    ///
    /// 同名三级语义（nexos-test BUG-agentcoord ① 的安全版）：
    ///
    /// - **同 pubkey**（或原条目未绑 pubkey）→ 幂等覆盖（200）；
    /// - **异 pubkey 且无 admin** → 409 拒绝（防重名劫持，先到先得不退）；
    /// - **异 pubkey + admin**（`Authorization: Bearer <NEXOS_ADMIN_TOKEN>`）
    ///   → 覆盖换绑：pubkey/callback 换新，**declared_groups 与收件箱保留**、
    ///   已声明群组不重发（运维找回通道——私钥丢失/换机时管理员代为换绑）。
    ///
    /// 注册成功即触发协议声明（§协议声明）：有 pubkey 且注入了 IM 桥时，
    /// 对该 pubkey 加入的所有群组发一条系统声明消息，同群只发一次。
    fn register(
        &self,
        name: &str,
        pubkey: Option<String>,
        callback_url: Option<String>,
        admin_override: bool,
    ) -> Result<(AgentEntry, bool, usize), String> {
        let now = now_iso();
        // —— 第一段：upsert 注册表 + 落盘 ——
        let (mut entry, is_new, snapshot) = {
            let mut st = self.state.lock().expect("agent-coord state poisoned");
            match st.agents.iter_mut().find(|a| a.name == name) {
                Some(a) => {
                    // 防重名劫持：name 已被别的 pubkey 占用 → 拒绝（先到先得；
                    // None 表示从未绑定身份，可被首个带 pubkey 的注册补全）。
                    // admin 例外：覆盖换绑（保 declared_groups/收件箱，不重发声明）
                    if let (Some(existing), Some(incoming)) =
                        (a.pubkey.as_deref(), pubkey.as_deref())
                    {
                        if existing != incoming && !admin_override {
                            return Err(format!(
                                "agent name {name:?} 已被 pubkey {existing} 占用（异 pubkey 抢名拒绝；admin token 可覆盖换绑）"
                            ));
                        }
                    }
                    a.pubkey = pubkey.clone();
                    a.callback_url = callback_url.clone();
                    a.updated_at = Some(now.clone());
                    (a.clone(), false, st.clone())
                }
                None => {
                    let a = AgentEntry {
                        name: name.to_string(),
                        pubkey: pubkey.clone(),
                        callback_url: callback_url.clone(),
                        declared_groups: Vec::new(),
                        created_at: now.clone(),
                        updated_at: None,
                    };
                    st.agents.push(a.clone());
                    (a, true, st.clone())
                }
            }
        };
        self.persist(&snapshot);
        // —— 第二段：协议声明（向未声明过的群组逐个发系统消息，同步短操作）——
        let mut declared_now = 0usize;
        if let (Some(pk), Some(bridge)) = (pubkey.as_deref(), self.im_bridge.as_ref()) {
            for gid in bridge.groups_of_member(pk) {
                if entry.declared_groups.contains(&gid) {
                    continue;
                }
                if bridge.post_system(&gid, &declaration_text(name)) {
                    entry.declared_groups.push(gid);
                    declared_now += 1;
                }
            }
            if declared_now > 0 {
                let snapshot = {
                    let mut st = self.state.lock().expect("agent-coord state poisoned");
                    if let Some(a) = st.agents.iter_mut().find(|a| a.name == name) {
                        a.declared_groups = entry.declared_groups.clone();
                    }
                    st.clone()
                };
                self.persist(&snapshot);
            }
        }
        Ok((entry, is_new, declared_now))
    }

    /// @ 定向投递（im.rs 每条消息落库+广播后经钩子调用）：
    ///
    /// - system 消息（声明自身）/ 无 mentions → 直接返回；
    /// - mentions 命中注册表才投递（未注册名字忽略）；agent 给自己发消息
    ///   （sender_id == 自身 pubkey）不投递（防自回环）；
    /// - **双写收件箱（BUG-agentcoord ④）**：无论在线离线都写收件箱记录
    ///   ——在线 `delivered=ws`（WS 广播已即时送达，记录是重启后的回看凭据：
    ///   新鲜度窗口内强杀客户端时纯 ws 投递无人消费即丢）、离线
    ///   `delivered=inbox` + 可选 webhook 异步回调（2xx 成功升级 `webhook`；
    ///   失败仅记日志不重试）。消费方处理完应及时 ack（默认拉取只回未读）；
    /// - **去重**：同 message_id 对同 agent 不重复插入（消息重放/挂钩重入
    ///   下收件箱不翻倍）。
    fn route_message(self: &Arc<Self>, msg: &Message) {
        if msg.mentions.is_empty() || msg.sender_kind == "system" {
            return;
        }
        // 命中的注册 agent + 在线判定快照（锁内只读快照，投递记录另行入锁）
        let hits: Vec<(AgentEntry, bool)> = {
            let st = self.state.lock().expect("agent-coord state poisoned");
            st.agents
                .iter()
                .filter(|a| {
                    msg.mentions.contains(&a.name)
                        && a.pubkey.as_deref() != Some(msg.sender_id.as_str())
                })
                .map(|a| {
                    (
                        a.clone(),
                        a.pubkey.as_deref().is_some_and(|pk| self.online(pk)),
                    )
                })
                .collect()
        };
        if hits.is_empty() {
            return;
        }
        for (agent, online) in hits {
            let seq = {
                let mut st = self.state.lock().expect("agent-coord state poisoned");
                // 同 message_id 已投递过该 agent → 跳过（不占 seq 不重发 webhook）
                if st
                    .inbox
                    .iter()
                    .any(|r| r.agent_name == agent.name && r.message_id == msg.id)
                {
                    continue;
                }
                st.next_seq += 1;
                let seq = st.next_seq;
                st.inbox.push(DeliveryRecord {
                    seq,
                    agent_name: agent.name.clone(),
                    message_id: msg.id.clone(),
                    group_id: msg.conversation_id.clone(),
                    content: im::truncate_chars(&msg.content, INBOX_CONTENT_MAX_CHARS),
                    delivered: if online {
                        DELIVERED_WS.to_string()
                    } else {
                        DELIVERED_INBOX.to_string()
                    },
                    acked_at: None,
                    created_at: now_iso(),
                });
                let snapshot = st.clone();
                drop(st);
                self.persist(&snapshot);
                seq
            };
            // 离线 + 配了 callback_url → 异步 webhook（不阻塞消息路径）；
            // 成功把该记录升级为 webhook，失败仅记日志（重试下期）。
            if !online
                && agent.callback_url.is_some()
                && tokio::runtime::Handle::try_current().is_ok()
            {
                let url = agent.callback_url.clone().unwrap_or_default();
                let payload = serde_json::json!({
                    "type": EVENT_AGENT_MENTION,
                    "agent": agent.name,
                    "message": serde_json::to_value(msg).unwrap_or(serde_json::Value::Null),
                    "inbox_url": format!(
                        "/api/v1/agents/{}/inbox?after={}",
                        agent.name,
                        seq.saturating_sub(1)
                    ),
                });
                let core = Arc::clone(self);
                let (name, seq) = (agent.name.clone(), seq);
                tokio::spawn(async move {
                    let ok = COORD_HTTP
                        .post(&url)
                        .timeout(WEBHOOK_HTTP_TIMEOUT)
                        .header("X-NexOS-Event", EVENT_AGENT_MENTION)
                        .json(&payload)
                        .send()
                        .await
                        .map(|r| r.status().is_success())
                        .unwrap_or(false);
                    if ok {
                        core.mark_delivered(&name, seq, DELIVERED_WEBHOOK);
                    } else {
                        eprintln!(
                            "[agent-coord] webhook 投递失败（不重试）: {name} seq={seq} -> {url}"
                        );
                    }
                });
            }
        }
    }

    /// webhook 成功后升级投递状态（找不到记录静默跳过——agent 已注销等）。
    fn mark_delivered(&self, agent_name: &str, seq: u64, delivered: &str) {
        let snapshot = {
            let mut st = self.state.lock().expect("agent-coord state poisoned");
            if let Some(r) = st
                .inbox
                .iter_mut()
                .find(|r| r.agent_name == agent_name && r.seq == seq)
            {
                r.delivered = delivered.to_string();
            }
            st.clone()
        };
        self.persist(&snapshot);
    }

    /// 收件箱拉取：seq 严格大于 `after` 的该 agent 记录，升序（新在后，
    /// 便于增量追加）。**默认只回未读**（`acked_at` 为空——客户端「返回即
    /// 未读」，重启不会把历史任务当新任务重跑，BUG-agentcoord ③）；
    /// `include_acked=true` 显式开历史（ack 不删，靠此开关回看）。
    /// agent 不存在 → None。
    fn inbox_of(&self, name: &str, after: u64, include_acked: bool) -> Option<Vec<DeliveryRecord>> {
        let st = self.state.lock().expect("agent-coord state poisoned");
        if !st.agents.iter().any(|a| a.name == name) {
            return None;
        }
        Some(
            st.inbox
                .iter()
                .filter(|r| {
                    r.agent_name == name && r.seq > after && (include_acked || r.acked_at.is_none())
                })
                .cloned()
                .collect(),
        )
    }

    /// 确认已读：把 seq ≤ 给定值的未读记录置 acked_at（历史保留不删）。
    /// 返回本次置位条数；agent 不存在 → None。
    fn ack(&self, name: &str, seq: u64) -> Option<usize> {
        let (n, snapshot) = {
            let mut st = self.state.lock().expect("agent-coord state poisoned");
            if !st.agents.iter().any(|a| a.name == name) {
                return None;
            }
            let now = now_iso();
            let n = st
                .inbox
                .iter_mut()
                .filter(|r| r.agent_name == name && r.seq <= seq && r.acked_at.is_none())
                .map(|r| {
                    r.acked_at = Some(now.clone());
                })
                .count();
            (n, st.clone())
        };
        self.persist(&snapshot);
        Some(n)
    }

    /// 注销：删注册条目 + 连带其收件箱记录。不存在 → false。
    fn delete(&self, name: &str) -> bool {
        let snapshot = {
            let mut st = self.state.lock().expect("agent-coord state poisoned");
            let before = st.agents.len();
            st.agents.retain(|a| a.name != name);
            if st.agents.len() == before {
                return false;
            }
            st.inbox.retain(|r| r.agent_name != name);
            st.clone()
        };
        self.persist(&snapshot);
        true
    }

    /// 列表视图（带 online 派生 + callback 脱敏）。
    fn list(&self) -> Vec<AgentListItem> {
        let st = self.state.lock().expect("agent-coord state poisoned");
        st.agents
            .iter()
            .map(|a| AgentListItem {
                name: a.name.clone(),
                pubkey: a.pubkey.clone(),
                online: a.pubkey.as_deref().is_some_and(|pk| self.online(pk)),
                callback_url: a.callback_url.as_deref().map(mask_callback_url),
                declared_groups: a.declared_groups.len(),
                created_at: a.created_at.clone(),
            })
            .collect()
    }
}

/// webhook 用共享 HTTP 客户端（进程级连接池，im.rs AGENT_HTTP 同款）。
static COORD_HTTP: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .build()
        .expect("构建 agent-coord reqwest Client 失败")
});

// ----------------------------------------------------------------------------
// 进程级钩子：im.rs 发消息路径的一行挂钩
// ----------------------------------------------------------------------------

/// im 消息钩子（进程级单例）：main.rs 装配 agent-coord 时 [`install_hook`]
/// 注入内核；im.rs 每条消息落库+WS 广播后调 [`on_im_message`]
/// （未装配时 no-op——单测/独立 im 部署零开销）。
static MENTION_HOOK: Lazy<RwLock<Option<Arc<CoordCore>>>> = Lazy::new(|| RwLock::new(None));

/// 装配时注入协调内核（main.rs：`install_hook(handler.core())`）。
pub fn install_hook(core: Arc<CoordCore>) {
    *MENTION_HOOK.write().expect("agent-coord hook poisoned") = Some(core);
}

/// 清除钩子（测试隔离用；生产不调）。
pub fn clear_hook() {
    *MENTION_HOOK.write().expect("agent-coord hook poisoned") = None;
}

/// im.rs 挂钩入口：一条新消息落库+广播后，@ 定向投递给命中的注册 agent。
pub fn on_im_message(msg: &Message) {
    let core = MENTION_HOOK
        .read()
        .expect("agent-coord hook poisoned")
        .clone();
    if let Some(core) = core {
        core.route_message(msg);
    }
}

// ----------------------------------------------------------------------------
// AgentCoordRouteHandler
// ----------------------------------------------------------------------------

/// agent 协调路由处理器——HTTP 边界（`/api/v1/agents*`）适配到
/// [`CoordCore`]（JSON 持久化注册表/收件箱 + WS 在线判定 + IM 桥声明）。
pub struct AgentCoordRouteHandler {
    core: Arc<CoordCore>,
}

impl AgentCoordRouteHandler {
    /// 生产构造：读 env `NEXOS_AGENTS_FILE`（缺省 [`DEFAULT_AGENTS_FILE`]）
    /// 并载入既有状态（缺失/损坏 → 空表）。
    #[must_use]
    pub fn new() -> Self {
        let path = std::env::var("NEXOS_AGENTS_FILE")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_AGENTS_FILE.to_string());
        Self::with_state_path(&path)
    }

    /// 指定持久化路径构造（读回既有状态；测试注入）。
    #[must_use]
    pub fn with_state_path(path: &str) -> Self {
        Self::from_parts(Some(path.to_string()))
    }

    /// 纯内存态构造（无持久化；测试隔离注入）。
    #[must_use]
    pub fn with_empty() -> Self {
        Self::from_parts(None)
    }

    fn from_parts(state_path: Option<String>) -> Self {
        let state = state_path
            .as_deref()
            .map_or_else(PersistState::default, load_state_from);
        let online_stale = Duration::from_secs(parse_online_stale_secs(
            std::env::var(ENV_ONLINE_STALE_SECS).ok().as_deref(),
        ));
        Self {
            core: Arc::new(CoordCore {
                state: Mutex::new(state),
                state_path,
                ws_hub: None,
                im_bridge: None,
                online_stale,
            }),
        }
    }

    /// 链式注入 WS hub（在线判定 + 声明广播；main.rs 传 `gw.ws_hub()`）。
    /// 须在 [`install_hook`] / 网关注册**之前**调用（内核 Arc 尚未共享）。
    #[must_use]
    pub fn with_ws_hub(mut self, hub: WsHub) -> Self {
        Arc::get_mut(&mut self.core)
            .expect("agent-coord 内核尚未共享（with_ws_hub 须在 install_hook/注册前）")
            .ws_hub = Some(hub);
        self
    }

    /// 链式注入 IM 桥（声明系统消息直插 + 群成员查询）。
    /// 须在 [`install_hook`] / 网关注册**之前**调用。
    #[must_use]
    pub fn with_im_bridge(mut self, bridge: ImCoordBridge) -> Self {
        Arc::get_mut(&mut self.core)
            .expect("agent-coord 内核尚未共享（with_im_bridge 须在 install_hook/注册前）")
            .im_bridge = Some(bridge);
        self
    }

    /// 协调内核句柄（main.rs `install_hook` 注入 im 发消息路径用）。
    #[must_use]
    pub fn core(&self) -> Arc<CoordCore> {
        self.core.clone()
    }

    /// @ 定向投递入口（测试/诊断直调；生产路径走 im 钩子 [`on_im_message`]）。
    pub fn route(&self, msg: &Message) {
        self.core.route_message(msg);
    }
}

impl Default for AgentCoordRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RouteHandler for AgentCoordRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            // 开发期公开（add-peer 惯例）；发版前收紧：register/delete → admin，
            // inbox/ack → agent 侧 token（见模块注释「鉴权」）。
            spec(HttpMethod::Post, "/api/v1/agents/register", false),
            spec(HttpMethod::Get, "/api/v1/agents", false),
            spec(HttpMethod::Get, "/api/v1/agents/protocol", false),
            spec(HttpMethod::Get, "/api/v1/agents/:name/inbox", false),
            spec(HttpMethod::Post, "/api/v1/agents/:name/ack", false),
            spec(HttpMethod::Delete, "/api/v1/agents/:name", false),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        let query = req.path.split_once('?').map(|(_, q)| q).unwrap_or("");
        match (req.method, segs.as_slice()) {
            // —— POST /api/v1/agents/register —— 注册/更新 agent
            //    body: { name, pubkey?, callback_url? }
            //    同名三级语义（BUG-agentcoord ① 安全版）：同 pubkey 幂等覆盖
            //    （保留 declared_groups 不重发声明）/ 异 pubkey 无 admin 409 /
            //    异 pubkey + admin（Bearer NEXOS_ADMIN_TOKEN → Admin Principal）
            //    覆盖换绑。201=新建 / 200=覆盖更新；畸形 body 400（客户端错误，
            //    BUG-agentcoord ②——不再污染 5xx 监控口径）。
            (HttpMethod::Post, ["api", "v1", "agents", "register"]) => {
                #[derive(serde::Deserialize)]
                struct RegisterReq {
                    name: String,
                    #[serde(default)]
                    pubkey: Option<String>,
                    #[serde(default)]
                    callback_url: Option<String>,
                }
                let body: RegisterReq = match serde_json::from_value(req.body) {
                    Ok(b) => b,
                    Err(e) => {
                        return Ok(error_response(
                            400,
                            &format!("解析注册 agent 请求体失败（应为 JSON 对象）: {e}"),
                        ))
                    }
                };
                // admin 判定（覆盖换绑特权）：AuthMiddleware 已解析的 Principal
                // 带 Admin 角色（NEXOS_ADMIN_TOKEN Bearer 精确匹配 / admin JWT）；
                // 路由本身仍公开——无 admin 的注册走幂等/409 语义。
                let admin_override = req.auth.as_ref().is_some_and(|p| {
                    p.roles
                        .iter()
                        .any(|r| matches!(r, os_security::Role::Admin))
                });
                let name = body.name.trim().to_string();
                if !is_valid_agent_name(&name) {
                    return Ok(error_response(
                        400,
                        &format!("非法 name（须 [a-z0-9-]，1..={NAME_MAX_CHARS} 字符）: {name:?}"),
                    ));
                }
                if let Some(pk) = body.pubkey.as_deref() {
                    if !is_valid_pubkey(pk) {
                        return Ok(error_response(
                            400,
                            "非法 pubkey（须 0x + 66 hex 压缩 secp256k1 公钥）",
                        ));
                    }
                }
                if let Some(url) = body.callback_url.as_deref() {
                    if !is_valid_callback_url(url) {
                        return Ok(error_response(
                            400,
                            "非法 callback_url（须 http(s) 且无空白，≤2048 字符）",
                        ));
                    }
                }
                let (entry, is_new, declared_now) =
                    match self
                        .core
                        .register(&name, body.pubkey, body.callback_url, admin_override)
                    {
                        Ok(v) => v,
                        Err(e) => return Ok(error_response(409, &e)),
                    };
                let mut resp =
                    to_value(&entry).map_err(|e| ApiGatewayError::Internal(format!("{e}")))?;
                resp["declared_now"] = serde_json::json!(declared_now);
                resp["online"] = serde_json::json!(entry
                    .pubkey
                    .as_deref()
                    .is_some_and(|pk| self.core.online(pk)));
                Ok(ApiResponse {
                    status: if is_new { 201 } else { 200 },
                    body: resp,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/agents —— 列表（online 派生 + callback 脱敏）
            (HttpMethod::Get, ["api", "v1", "agents"]) => {
                let agents = self.core.list();
                Ok(ok_json(serde_json::json!({
                    "agents": to_value(&agents)?,
                    "count": agents.len(),
                })))
            }

            // —— GET /api/v1/agents/protocol —— 协作协议文本（agent 自举读取）
            (HttpMethod::Get, ["api", "v1", "agents", "protocol"]) => {
                Ok(ok_json(serde_json::json!({
                    "component": COMPONENT,
                    "version": 1,
                    "protocol": PROTOCOL_TEXT,
                    "endpoints": {
                        "register": "/api/v1/agents/register",
                        "list": "/api/v1/agents",
                        "protocol": "/api/v1/agents/protocol",
                        "inbox": "/api/v1/agents/<name>/inbox?after=<seq>",
                        "ack": "/api/v1/agents/<name>/ack",
                    },
                })))
            }

            // —— GET /api/v1/agents/:name/inbox?after=<seq>&include_acked=1
            //    收件箱增量：返回 seq 严格大于 after 的记录，升序（新在后）；
            //    缺省 after=0。**默认只回未读**（acked_at 为空），
            //    include_acked=1 显式开历史（BUG-agentcoord ③——客户端
            //    「返回即未读」，重启不重跑历史任务）。
            (HttpMethod::Get, ["api", "v1", "agents", name, "inbox"]) => {
                let after = parse_query_u64(query, "after").unwrap_or(0);
                let include_acked = parse_query_u64(query, "include_acked").unwrap_or(0) > 0;
                let Some(records) = self.core.inbox_of(name, after, include_acked) else {
                    return Ok(error_response(404, &format!("agent 不存在: {name}")));
                };
                Ok(ok_json(serde_json::json!({
                    "agent": name,
                    "after": after,
                    "include_acked": include_acked,
                    "count": records.len(),
                    "records": to_value(&records)?,
                })))
            }

            // —— POST /api/v1/agents/:name/ack —— 确认已读 body:{seq}
            //    seq ≤ 给定值的未读记录置 acked_at；历史保留（不删除，
            //    include_acked=1 可回看）。畸形 body 400（同 register）。
            (HttpMethod::Post, ["api", "v1", "agents", name, "ack"]) => {
                #[derive(serde::Deserialize)]
                struct AckReq {
                    seq: u64,
                }
                let body: AckReq = match serde_json::from_value(req.body) {
                    Ok(b) => b,
                    Err(e) => {
                        return Ok(error_response(
                            400,
                            &format!("解析 ack 请求体失败（应为 JSON 对象）: {e}"),
                        ))
                    }
                };
                let Some(acked) = self.core.ack(name, body.seq) else {
                    return Ok(error_response(404, &format!("agent 不存在: {name}")));
                };
                Ok(ok_json(serde_json::json!({
                    "agent": name,
                    "acked": acked,
                    "seq": body.seq,
                })))
            }

            // —— DELETE /api/v1/agents/:name —— 注销（连带收件箱）
            (HttpMethod::Delete, ["api", "v1", "agents", name]) => {
                if self.core.delete(name) {
                    Ok(ok_json(serde_json::json!({"deleted": name})))
                } else {
                    Ok(error_response(404, &format!("agent 不存在: {name}")))
                }
            }

            // —— 未覆盖路由 —— 兜底 404（Ok，非 Err，便于上层定位）
            _ => Ok(error_response(404, "agent-coord: 未匹配的路由")),
        }
    }
}

// ----------------------------------------------------------------------------
// 持久化（JSON 原子写，update.rs 同款）
// ----------------------------------------------------------------------------

/// 原子写 JSON（先写 `<path>.tmp` 再 rename；父目录不存在自动创建）。
fn persist_state_to(path: &str, st: &PersistState) -> Result<(), String> {
    let p = std::path::Path::new(path);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建状态目录失败 {dir:?}: {e}"))?;
    }
    let tmp = format!("{path}.tmp");
    let body = serde_json::to_string_pretty(st).map_err(|e| format!("状态序列化失败: {e}"))?;
    std::fs::write(&tmp, body).map_err(|e| format!("写临时状态失败 {tmp}: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("原子替换状态失败 {path}: {e}"))
}

/// 读回 JSON 状态（缺失/解析失败 → 空表，不报错：首次运行/损坏降级）。
fn load_state_from(path: &str) -> PersistState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

// ----------------------------------------------------------------------------
// HTTP 小工具（各 handler 本地惯例）
// ----------------------------------------------------------------------------

/// 构造一条 [`RouteSpec`]（component 固定 `agent-coord`；本组件无角色门）。
fn spec(method: HttpMethod, path: &str, requires_auth: bool) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: COMPONENT.to_string(),
        requires_auth,
        required_roles: vec![],
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

/// 从 query string 解析 u64 参数（缺失/非数字 → None）。
fn parse_query_u64(query: &str, key: &str) -> Option<u64> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        if k == key {
            v.parse::<u64>().ok()
        } else {
            None
        }
    })
}

/// 当前本地时间（RFC3339 带时区，im.rs 同款）。
fn now_iso() -> String {
    chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

// ----------------------------------------------------------------------------
// 单元测（25 个：路由形状/注册幂等/同名三级语义（同 pubkey 幂等、异 pubkey
// 409、admin Principal 覆盖换绑保 declared_groups+收件箱）/ 畸形 JSON 400/
// 非法 name/pubkey/callback 400/ 在线判定（含新鲜度阈值：订阅刚 touch 在线、
// 僵死订阅离线不删、env 覆盖生效、无 pubkey 恒离线）/ 投递三态（ws / webhook
// （std TcpListener mock）/ inbox）+ 双写·去重·ack 过滤 / 未注册与自提及 /
// inbox after 增量 + 默认只回未读·include_acked=1 开历史 / ack 历史保留 /
// 协议文本 / 声明一次 / 无 pubkey 跳过 / 删除 / 持久化 / im 钩子）
// ----------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    fn handler() -> AgentCoordRouteHandler {
        AgentCoordRouteHandler::with_empty()
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

    fn get_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    fn delete_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Delete,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    /// 带 IM token 的请求（im 用户面端点用）。
    fn im_authed(
        method: HttpMethod,
        path: &str,
        token: &str,
        body: serde_json::Value,
    ) -> ApiRequest {
        ApiRequest {
            method,
            path: path.into(),
            headers: serde_json::json!({"authorization": format!("Bearer {token}")}),
            body,
            auth: None,
        }
    }

    async fn register(h: &AgentCoordRouteHandler, body: serde_json::Value) -> ApiResponse {
        h.handle(post_req("/api/v1/agents/register", body))
            .await
            .unwrap()
    }

    /// 造一个格式合法的假 pubkey（0x + 66 hex；seed 派生互不相同）。
    fn fake_pubkey(seed: u8) -> String {
        let bytes: Vec<u8> = (0..33).map(|i| seed.wrapping_add(i as u8)).collect();
        format!("0x{}", hex::encode(bytes))
    }

    /// 构造认证身份（B1 覆盖换绑权限用；model_hub 同款手法）。
    fn principal(admin: bool) -> os_security::Principal {
        let now = chrono::Utc::now();
        let roles = if admin {
            vec![os_security::Role::Admin]
        } else {
            vec![os_security::Role::User]
        };
        let name = if admin { "admin" } else { "user" };
        let user = os_security::User::new(os_security::UserId::new(name), name, roles.clone(), now)
            .unwrap();
        os_security::Principal::new(user, roles, now).unwrap()
    }

    /// 带认证身份的注册请求（Bearer NEXOS_ADMIN_TOKEN 经 AuthMiddleware 解析后
    /// 即注入该 Principal——单测直接构造等价形态）。
    fn register_req_authed(
        body: serde_json::Value,
        auth: Option<os_security::Principal>,
    ) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Post,
            path: "/api/v1/agents/register".into(),
            headers: serde_json::json!({}),
            body,
            auth,
        }
    }

    /// 造一条 im 消息（mentions 由 content 解析，与 im.rs 服务端语义一致）。
    /// id 用进程内递增序号：同长度的不同消息也须不同 id——投递去重按
    /// message_id（B4），长度派生 id 会把「一/二/三」误判成同一条。
    fn msg_in(cid: &str, sender: &str, content: &str) -> Message {
        static MSG_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let seq = MSG_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Message {
            id: format!("msg-{sender}-{seq}"),
            conversation_id: cid.to_string(),
            sender_id: sender.to_string(),
            sender_name: Some(sender.to_string()),
            content: content.to_string(),
            msg_type: "text".to_string(),
            file_url: None,
            reply_to: None,
            created_at: now_iso(),
            read_by: vec![sender.to_string()],
            sender_kind: "human".to_string(),
            mentions: im::parse_mentions(content),
            attachment: None,
        }
    }

    /// 取收件箱记录（经 HTTP inbox 端点，顺带覆盖其契约；默认语义＝只回未读）。
    async fn inbox(h: &AgentCoordRouteHandler, name: &str, after: u64) -> Vec<DeliveryRecord> {
        inbox_with(h, name, after, false).await
    }

    /// 取收件箱记录（include_acked 显式控制——true = 含已 ack 历史）。
    async fn inbox_with(
        h: &AgentCoordRouteHandler,
        name: &str,
        after: u64,
        include_acked: bool,
    ) -> Vec<DeliveryRecord> {
        let resp = h
            .handle(get_req(&format!(
                "/api/v1/agents/{name}/inbox?after={after}&include_acked={}",
                if include_acked { 1 } else { 0 }
            )))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "inbox 应 200: {}", resp.body);
        serde_json::from_value(resp.body["records"].clone()).unwrap()
    }

    /// 轮询等异步条件成立（截止时间后最核一次）；cond 为同步闭包。
    async fn wait_until<F: Fn() -> bool>(timeout_ms: u64, cond: F) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if cond() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return cond();
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    // 1. 路由表：6 条、component=agent-coord、开发期全公开
    #[tokio::test]
    async fn routes_shape() {
        let routes = handler().routes().await;
        assert_eq!(routes.len(), 6, "6 条路由: {routes:?}");
        assert!(routes.iter().all(|r| r.handler_component == "agent-coord"));
        assert!(routes.iter().all(|r| !r.requires_auth), "开发期全公开");
    }

    // 2. 注册幂等：同 name 覆盖（pubkey/callback 更新，created_at 保留）
    #[tokio::test]
    async fn register_idempotent_overwrite() {
        let h = handler();
        let first = register(
            &h,
            serde_json::json!({"name": "dev-agent", "callback_url": "http://127.0.0.1:9900/hook"}),
        )
        .await;
        assert_eq!(first.status, 201, "首次注册应 201: {}", first.body);
        let created = first.body["created_at"].as_str().unwrap().to_string();
        let second = register(
            &h,
            serde_json::json!({"name": "dev-agent", "pubkey": fake_pubkey(7)}),
        )
        .await;
        assert_eq!(
            second.status, 200,
            "重复注册应 200（覆盖）: {}",
            second.body
        );
        assert_eq!(
            second.body["created_at"],
            serde_json::json!(created),
            "created_at 保留"
        );
        assert_eq!(
            second.body["callback_url"],
            serde_json::Value::Null,
            "未传即覆盖为空"
        );
        // 列表只剩 1 条
        let list = h.handle(get_req("/api/v1/agents")).await.unwrap();
        assert_eq!(list.body["count"], serde_json::json!(1));
    }

    // 2b. 重名劫持拒绝：name 已被 pubkey A 占用，异 pubkey B 抢名 → 409；
    //     同 pubkey 重复注册仍幂等 200
    #[tokio::test]
    async fn register_name_conflict_rejected() {
        let h = handler();
        let first = register(
            &h,
            serde_json::json!({"name": "claimed-agent", "pubkey": fake_pubkey(11)}),
        )
        .await;
        assert_eq!(first.status, 201, "首次注册应 201: {}", first.body);
        let conflict = register(
            &h,
            serde_json::json!({"name": "claimed-agent", "pubkey": fake_pubkey(22)}),
        )
        .await;
        assert_eq!(
            conflict.status, 409,
            "异 pubkey 抢名应 409: {}",
            conflict.body
        );
        let same = register(
            &h,
            serde_json::json!({"name": "claimed-agent", "pubkey": fake_pubkey(11)}),
        )
        .await;
        assert_eq!(same.status, 200, "同 pubkey 重复注册应 200: {}", same.body);
    }

    // 2c. admin 覆盖换绑（BUG-agentcoord ① 安全版三级语义）：异 pubkey + admin
    //     Principal → 200 覆盖（pubkey 换绑、created_at/declared_groups 保留、
    //     收件箱保留）；非 admin Principal / 匿名的异 pubkey → 维持 409
    async fn register_authed(
        h: &AgentCoordRouteHandler,
        body: serde_json::Value,
        auth: Option<os_security::Principal>,
    ) -> ApiResponse {
        h.handle(register_req_authed(body, auth)).await.unwrap()
    }

    #[tokio::test]
    async fn register_admin_overrides_foreign_pubkey() {
        let h = handler();
        let first = register(
            &h,
            serde_json::json!({"name": "ops-agent", "pubkey": fake_pubkey(51)}),
        )
        .await;
        assert_eq!(first.status, 201);
        let created = first.body["created_at"].as_str().unwrap().to_string();
        // 造一点 declared_groups 历史与收件箱存量（覆盖后应保留）
        {
            let mut st = h.core.state.lock().unwrap();
            let a = st
                .agents
                .iter_mut()
                .find(|a| a.name == "ops-agent")
                .unwrap();
            a.declared_groups.push("group-legacy".to_string());
        }
        h.route(&msg_in("g", "0xa", "@ops-agent 旧任务"));
        // 非 admin 身份（普通用户 Principal）异 pubkey → 仍 409
        let user_taken = register_authed(
            &h,
            serde_json::json!({"name": "ops-agent", "pubkey": fake_pubkey(52)}),
            Some(principal(false)),
        )
        .await;
        assert_eq!(
            user_taken.status, 409,
            "普通身份无覆盖特权: {}",
            user_taken.body
        );
        // admin 异 pubkey → 200 覆盖换绑
        let admin = register_authed(
            &h,
            serde_json::json!({"name": "ops-agent", "pubkey": fake_pubkey(53)}),
            Some(principal(true)),
        )
        .await;
        assert_eq!(admin.status, 200, "admin 覆盖换绑应 200: {}", admin.body);
        assert_eq!(admin.body["pubkey"], fake_pubkey(53), "pubkey 换绑生效");
        assert_eq!(
            admin.body["created_at"],
            serde_json::json!(created),
            "created_at 保留"
        );
        assert_eq!(
            admin.body["declared_groups"],
            serde_json::json!(["group-legacy"]),
            "declared_groups 保留（不重发声明的记账基础）"
        );
        // 收件箱随条目保留（换绑不清投递历史）
        assert_eq!(inbox(&h, "ops-agent", 0).await.len(), 1, "收件箱保留");
        // 匿名异 pubkey 再来 → 维持 409（防劫持不退）
        let anon = register(
            &h,
            serde_json::json!({"name": "ops-agent", "pubkey": fake_pubkey(54)}),
        )
        .await;
        assert_eq!(anon.status, 409, "匿名异 pubkey 维持 409: {}", anon.body);
    }

    // 2d. 畸形 JSON body 400（BUG-agentcoord ②）：register/ack 的 serde 解析
    //     失败映射 400（客户端错误），不再 500（污染 5xx 监控口径）
    #[tokio::test]
    async fn malformed_body_returns_400_not_500() {
        let h = handler();
        // 非 JSON 对象（字符串/数组/缺 name 的对象）
        for bad in [
            serde_json::json!("BAD NAME!"),
            serde_json::json!([1, 2, 3]),
            serde_json::json!({"pubkey": fake_pubkey(1)}),
        ] {
            let resp = h
                .handle(post_req("/api/v1/agents/register", bad.clone()))
                .await
                .unwrap();
            assert_eq!(resp.status, 400, "畸形 body 应 400（非 500/Err）: {bad}");
            assert!(resp.body["error"].as_str().unwrap().contains("解析注册"));
        }
        // ack 同病同修
        register(&h, serde_json::json!({"name": "bad-body-agent"})).await;
        let resp = h
            .handle(post_req(
                "/api/v1/agents/bad-body-agent/ack",
                serde_json::json!("not an object"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "ack 畸形 body 应 400: {}", resp.body);
        let resp = h
            .handle(post_req(
                "/api/v1/agents/bad-body-agent/ack",
                serde_json::json!({}), // 缺 seq
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "ack 缺 seq 应 400: {}", resp.body);
    }

    // 3. 非法 name 400：大写/下划线/中文/超长/空；合法边界放行
    #[tokio::test]
    async fn register_invalid_name_rejected() {
        let h = handler();
        for bad in [
            "Bad_Name".to_string(),
            "中文名".to_string(),
            "a".repeat(33),
            String::new(),
            "with space".to_string(),
        ] {
            let resp = register(&h, serde_json::json!({"name": bad})).await;
            assert_eq!(resp.status, 400, "name={bad:?} 应 400");
        }
        // 合法边界：1 字符 / 32 字符 / 字母数字连字符
        for ok in ["a".to_string(), "b".repeat(32), "dev-agent-1".to_string()] {
            let resp = register(&h, serde_json::json!({"name": ok})).await;
            assert_eq!(resp.status, 201, "name={ok:?} 应 201");
        }
    }

    // 4. 非法 pubkey / callback_url 400
    #[tokio::test]
    async fn register_invalid_pubkey_and_callback_rejected() {
        let h = handler();
        for bad_pk in [
            "1234".to_string(),
            "0x1234".to_string(),
            format!("0x{}", "a".repeat(67)),
            "0xzz".to_string(),
        ] {
            let resp = register(&h, serde_json::json!({"name": "x-agent", "pubkey": bad_pk})).await;
            assert_eq!(resp.status, 400, "pubkey={bad_pk:?} 应 400");
        }
        let ok = register(
            &h,
            serde_json::json!({"name": "x-agent", "pubkey": fake_pubkey(1)}),
        )
        .await;
        assert_eq!(ok.status, 201);
        let bad_url = register(
            &h,
            serde_json::json!({"name": "x-agent", "callback_url": "ftp://127.0.0.1/hook"}),
        )
        .await;
        assert_eq!(bad_url.status, 400, "非 http(s) callback 应 400");
    }

    // 5. 列表 online 判定：造 WS 订阅（pubkey 有活订阅即在线）+ callback 脱敏
    #[tokio::test]
    async fn list_online_flag_via_ws_subscription() {
        let hub = WsHub::new(16);
        let h = handler().with_ws_hub(hub.clone());
        let pk = fake_pubkey(3);
        register(
            &h,
            serde_json::json!({
                "name": "online-agent",
                "pubkey": pk,
                "callback_url": "http://127.0.0.1:9900/hook/secret?token=abc"
            }),
        )
        .await;
        let list = h.handle(get_req("/api/v1/agents")).await.unwrap();
        assert_eq!(
            list.body["agents"][0]["online"],
            serde_json::json!(false),
            "无订阅应离线"
        );
        assert_eq!(
            list.body["agents"][0]["callback_url"],
            serde_json::json!("http://127.0.0.1:9900/***"),
            "callback_url 脱敏（路径/查询打码，host[:port] 保留）"
        );
        let (_sub, _rx) = hub.subscribe_raw(&pk);
        let list = h.handle(get_req("/api/v1/agents")).await.unwrap();
        assert_eq!(
            list.body["agents"][0]["online"],
            serde_json::json!(true),
            "有活订阅应在线"
        );
        // 无 pubkey 的 agent 恒离线
        register(&h, serde_json::json!({"name": "no-pk-agent"})).await;
        let list = h.handle(get_req("/api/v1/agents")).await.unwrap();
        let no_pk = list.body["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["name"] == "no-pk-agent")
            .unwrap();
        assert_eq!(no_pk["online"], serde_json::json!(false));
    }

    // —— online 新鲜度判定（半开/僵死 WS 连接兜底，BUG-dev-standby 关联项）——

    /// env 互斥锁：改写 `NEXOS_AGENTS_ONLINE_STALE_SECS` 或依赖默认阈值语义
    /// 的用例串行（测试进程内 env 全局，防并行用例构造 handler 时读到
    /// 被覆盖的阈值）。用 tokio Mutex：env 覆盖须跨越 `.await` 持锁到
    /// 断言结束（阈值在 handler 构造时读定），std Mutex 会触发 clippy
    /// `await_holding_lock`。
    static ENV_LOCK: Lazy<tokio::sync::Mutex<()>> = Lazy::new(|| tokio::sync::Mutex::new(()));

    /// RAII env 恢复（测试结束 remove_var，防泄漏到并行用例）。
    struct EnvGuard;
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            std::env::remove_var(ENV_ONLINE_STALE_SECS);
        }
    }

    // 5a. 订阅刚 touch → online true：新订阅即新鲜；touch_raw（模拟读循环
    //     收到客户端 ping）后仍在线
    #[tokio::test]
    async fn online_fresh_subscription_is_true() {
        let hub = WsHub::new(16);
        let h = handler().with_ws_hub(hub.clone());
        let pk = fake_pubkey(41);
        register(&h, serde_json::json!({"name": "fresh-agent", "pubkey": pk})).await;
        let (sub, _rx) = hub.subscribe_raw(&pk);
        let list = h.handle(get_req("/api/v1/agents")).await.unwrap();
        assert_eq!(
            list.body["agents"][0]["online"],
            serde_json::json!(true),
            "新订阅即新鲜"
        );
        hub.touch_raw(sub);
        let list = h.handle(get_req("/api/v1/agents")).await.unwrap();
        assert_eq!(
            list.body["agents"][0]["online"],
            serde_json::json!(true),
            "touch 后仍在线"
        );
    }

    // 5b. 订阅 last_active 老于阈值 → online false（半开兜底）：订阅不删
    //     （subscriber_count_for 仍 1）；@ 投递走 inbox 而非 ws；touch 复活
    #[tokio::test]
    async fn online_stale_subscription_judged_offline() {
        let _env = ENV_LOCK.lock().await;
        let hub = WsHub::new(16);
        let h = handler().with_ws_hub(hub.clone());
        let pk = fake_pubkey(42);
        register(&h, serde_json::json!({"name": "stale-agent", "pubkey": pk})).await;
        let (sub, _rx) = hub.subscribe_raw(&pk);
        // 伪造半开：订阅残留但最后活动已 121s（> 默认阈值 120）
        hub.backdate_last_active_for_test(sub, Duration::from_secs(DEFAULT_ONLINE_STALE_SECS + 1));
        let list = h.handle(get_req("/api/v1/agents")).await.unwrap();
        assert_eq!(
            list.body["agents"][0]["online"],
            serde_json::json!(false),
            "僵死订阅应判离线"
        );
        assert_eq!(
            hub.subscriber_count_for(&pk),
            1,
            "过期不删订阅（只影响判定，清理仍归断连路径）"
        );
        // 半开期间 @ 它 → 投递记录走收件箱（不再误标 ws）
        h.route(&msg_in("g", "0xa", "@stale-agent 在吗"));
        let records = inbox(&h, "stale-agent", 0).await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].delivered, "inbox", "僵死订阅的 @ 投递应走收件箱");
        // 客户端 ping 到达（touch）→ 重新在线
        hub.touch_raw(sub);
        let list = h.handle(get_req("/api/v1/agents")).await.unwrap();
        assert_eq!(
            list.body["agents"][0]["online"],
            serde_json::json!(true),
            "touch 复活后重新在线"
        );
    }

    // 5c. 阈值 env 覆盖生效 + 解析回落：合法值放大窗口（121s 旧订阅仍在线），
    //     去掉 env 重建 handler（阈值构造时读定）→ 回落默认 120 判离线；
    //     纯函数解析：缺失/非数字/0 → 默认
    #[tokio::test]
    async fn online_stale_threshold_env_override() {
        assert_eq!(parse_online_stale_secs(None), DEFAULT_ONLINE_STALE_SECS);
        assert_eq!(
            parse_online_stale_secs(Some("abc")),
            DEFAULT_ONLINE_STALE_SECS,
            "非数字回落默认"
        );
        assert_eq!(
            parse_online_stale_secs(Some("0")),
            DEFAULT_ONLINE_STALE_SECS,
            "0（即时过期）视为非法回落默认"
        );
        assert_eq!(
            parse_online_stale_secs(Some(" 30 ")),
            30,
            "trim + 合法 u64 生效"
        );
        let _env = ENV_LOCK.lock().await;
        let _guard = EnvGuard;
        std::env::set_var(ENV_ONLINE_STALE_SECS, "999999");
        let hub = WsHub::new(16);
        let h = handler().with_ws_hub(hub.clone());
        let pk = fake_pubkey(43);
        register(&h, serde_json::json!({"name": "env-agent", "pubkey": pk})).await;
        let (sub, _rx) = hub.subscribe_raw(&pk);
        hub.backdate_last_active_for_test(sub, Duration::from_secs(DEFAULT_ONLINE_STALE_SECS + 1));
        let list = h.handle(get_req("/api/v1/agents")).await.unwrap();
        assert_eq!(
            list.body["agents"][0]["online"],
            serde_json::json!(true),
            "阈值 999999s 时 121s 旧订阅仍在线（env 生效；默认 120 会判 false）"
        );
        // 阈值构造时读定：清除 env 后须重建 handler 才回落默认
        drop(h);
        std::env::remove_var(ENV_ONLINE_STALE_SECS);
        let h2 = handler().with_ws_hub(hub.clone());
        register(&h2, serde_json::json!({"name": "env-agent", "pubkey": pk})).await;
        let list = h2.handle(get_req("/api/v1/agents")).await.unwrap();
        assert_eq!(
            list.body["agents"][0]["online"],
            serde_json::json!(false),
            "回落默认 120s 后同一订阅判离线"
        );
    }

    // 5d. 无 pubkey 恒 false：hub 里其他用户的活订阅再多也与本 agent 无关
    #[tokio::test]
    async fn online_false_without_pubkey_despite_other_subs() {
        let hub = WsHub::new(16);
        let h = handler().with_ws_hub(hub.clone());
        register(&h, serde_json::json!({"name": "anon-agent"})).await;
        let _ = hub.subscribe_raw(&fake_pubkey(44));
        let _ = hub.subscribe_raw("someone-else");
        let list = h.handle(get_req("/api/v1/agents")).await.unwrap();
        assert_eq!(
            list.body["agents"][0]["online"],
            serde_json::json!(false),
            "无 pubkey 无从匹配订阅键，恒离线"
        );
    }

    // 6. 投递态一：在线 → delivered=ws（WS 广播已送达，inbox 仅留痕；
    //    配了 callback 也不触发——在线不走 webhook）
    #[tokio::test]
    async fn route_mention_online_marks_ws() {
        let hub = WsHub::new(16);
        let h = handler().with_ws_hub(hub.clone());
        let pk = fake_pubkey(5);
        register(
            &h,
            serde_json::json!({
                "name": "ws-agent",
                "pubkey": pk,
                "callback_url": "http://127.0.0.1:1/dead"
            }),
        )
        .await;
        let (_sub, _rx) = hub.subscribe_raw(&pk);
        h.route(&msg_in("group-dev", "0xalice", "@ws-agent 在吗"));
        let records = inbox(&h, "ws-agent", 0).await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].delivered, "ws");
        assert_eq!(records[0].group_id, "group-dev");
        assert_eq!(records[0].agent_name, "ws-agent");
    }

    // 6b. 双写 + 去重 + ack 收口（BUG-agentcoord ④）：在线订阅 + @ → 收件箱
    //     有 delivered=ws 记录（强杀场景重启后 inbox 可见——@ 不丢）；ack 后
    //     默认（include_acked=0）不回、include_acked=1 可回看；同 message_id
    //     重投不重复插入
    #[tokio::test]
    async fn online_mention_double_writes_inbox_and_ack_filters() {
        let hub = WsHub::new(16);
        let h = handler().with_ws_hub(hub.clone());
        let pk = fake_pubkey(61);
        register(&h, serde_json::json!({"name": "dup-agent", "pubkey": pk})).await;
        let (_sub, _rx) = hub.subscribe_raw(&pk); // 在线（新鲜订阅）
        let msg = msg_in("g", "0xalice", "@dup-agent 强杀窗口测试");
        h.route(&msg);
        // 双写：在线也有收件箱记录（delivered=ws）
        let records = inbox(&h, "dup-agent", 0).await;
        assert_eq!(records.len(), 1, "在线 @ 也落收件箱（双写）");
        assert_eq!(records[0].delivered, "ws");
        // 去重：同 message_id 重投（挂钩重入/重放）不重复插入
        h.route(&msg);
        h.route(&msg);
        assert_eq!(
            inbox_with(&h, "dup-agent", 0, true).await.len(),
            1,
            "同 message_id 不重复插入"
        );
        // ack 收口：消费者确认后默认拉取不回（返回即未读）
        let resp = h
            .handle(post_req(
                "/api/v1/agents/dup-agent/ack",
                serde_json::json!({"seq": records[0].seq}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert_eq!(resp.body["acked"], serde_json::json!(1));
        assert!(
            inbox(&h, "dup-agent", 0).await.is_empty(),
            "ack 后默认（include_acked=0）不回"
        );
        let history = inbox_with(&h, "dup-agent", 0, true).await;
        assert_eq!(history.len(), 1, "include_acked=1 显式开历史");
        assert!(history[0].acked_at.is_some());
        // 后续新 @ 再来 → 默认拉取只回这条新的
        h.route(&msg_in("g", "0xalice", "@dup-agent 新任务"));
        let fresh = inbox(&h, "dup-agent", 0).await;
        assert_eq!(fresh.len(), 1);
        assert!(fresh[0].acked_at.is_none());
    }

    // 7. 投递态二：离线 + callback → 本地 mock HTTP listener（std TcpListener）
    //    收到 POST（body 带 agent_mention/完整消息/inbox_url），记录升级 webhook
    #[tokio::test]
    async fn route_mention_offline_webhook_fires() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let url = spawn_mock_webhook(seen.clone(), 2);
        let h = handler();
        register(
            &h,
            serde_json::json!({"name": "hook-agent", "callback_url": url}),
        )
        .await;
        let msg = msg_in("group-ops", "0xbob", "@hook-agent 请查日志");
        h.route(&msg);
        // 轮询等异步 webhook 到达（≤3s）
        let got = wait_until(3_000, || {
            seen.lock()
                .unwrap()
                .iter()
                .any(|r| r.contains("agent_mention"))
        })
        .await;
        assert!(got, "mock webhook 应收到 agent_mention POST");
        let raw = seen
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.contains("agent_mention"))
            .unwrap()
            .clone();
        assert!(raw.contains(&msg.id), "body 含完整消息（message.id）");
        assert!(
            raw.contains("/api/v1/agents/hook-agent/inbox"),
            "body 含 inbox_url: {raw}"
        );
        // 投递记录升级为 webhook
        let upgraded = wait_until(3_000, || {
            h.core
                .state
                .lock()
                .unwrap()
                .inbox
                .iter()
                .any(|r| r.agent_name == "hook-agent" && r.delivered == "webhook")
        })
        .await;
        assert!(upgraded, "投递记录应升级 delivered=webhook");
    }

    // 8. 投递态三：离线无 callback → 仅收件箱（delivered=inbox）
    #[tokio::test]
    async fn route_mention_offline_no_callback_inbox_only() {
        let h = handler();
        register(&h, serde_json::json!({"name": "inbox-agent"})).await;
        h.route(&msg_in("group-ops", "0xbob", "@inbox-agent 领取任务"));
        let records = inbox(&h, "inbox-agent", 0).await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].delivered, "inbox");
        assert_eq!(records[0].content, "@inbox-agent 领取任务");
    }

    // 9. 未注册名字不投递；agent 自提及不投递（防自回环）；system 消息不路由
    #[tokio::test]
    async fn route_ignores_unregistered_selfmention_and_system() {
        let h = handler();
        let pk = fake_pubkey(9);
        register(&h, serde_json::json!({"name": "echo-agent", "pubkey": pk})).await;
        h.route(&msg_in("g", "0xbob", "@nobody-registered 喵"));
        h.route(&msg_in("g", &pk, "@echo-agent 自己说"));
        let mut sys = msg_in("g", "system", "@echo-agent 声明");
        sys.sender_kind = "system".to_string();
        h.route(&sys);
        let records = inbox(&h, "echo-agent", 0).await;
        assert!(records.is_empty(), "三处都不应投递: {records:?}");
    }

    // 10. inbox after 增量：升序（新在后），after=seq 只取后续；默认只回未读
    //     （BUG-agentcoord ③），include_acked=1 开历史
    #[tokio::test]
    async fn inbox_after_incremental() {
        let h = handler();
        register(&h, serde_json::json!({"name": "seq-agent"})).await;
        for content in ["@seq-agent 一", "@seq-agent 二", "@seq-agent 三"] {
            h.route(&msg_in("g", "0xbob", content));
        }
        let all = inbox(&h, "seq-agent", 0).await;
        assert_eq!(all.len(), 3);
        assert!(
            all.windows(2).all(|w| w[0].seq < w[1].seq),
            "升序（新在后）"
        );
        let after_first = inbox(&h, "seq-agent", all[0].seq).await;
        assert_eq!(after_first.len(), 2);
        assert!(after_first.iter().all(|r| r.seq > all[0].seq));
        let after_last = inbox(&h, "seq-agent", all[2].seq).await;
        assert!(after_last.is_empty(), "游标打到最新应为空");
        // 默认只回未读：ack 掉前两条 → 默认拉取只剩第三条；include_acked=1 全回
        let _ = h
            .handle(post_req(
                "/api/v1/agents/seq-agent/ack",
                serde_json::json!({"seq": all[1].seq}),
            ))
            .await
            .unwrap();
        let unacked = inbox(&h, "seq-agent", 0).await;
        assert_eq!(unacked.len(), 1, "默认过滤已 ack");
        assert_eq!(unacked[0].seq, all[2].seq);
        assert_eq!(
            inbox_with(&h, "seq-agent", 0, true).await.len(),
            3,
            "include_acked=1 显式开历史"
        );
        // 未知 agent → 404
        let resp = h
            .handle(get_req("/api/v1/agents/ghost-agent/inbox"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // 11. ack：seq 之前（含）标已读，历史保留（include_acked=1 可见）
    #[tokio::test]
    async fn ack_marks_records_and_keeps_history() {
        let h = handler();
        register(&h, serde_json::json!({"name": "ack-agent"})).await;
        h.route(&msg_in("g", "0xa", "@ack-agent 一"));
        h.route(&msg_in("g", "0xa", "@ack-agent 二"));
        let all = inbox_with(&h, "ack-agent", 0, true).await;
        let resp = h
            .handle(post_req(
                "/api/v1/agents/ack-agent/ack",
                serde_json::json!({"seq": all[0].seq}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert_eq!(resp.body["acked"], serde_json::json!(1));
        // 默认拉取：已 ack 的不再回（返回即未读）
        let pending = inbox(&h, "ack-agent", 0).await;
        assert_eq!(pending.len(), 1, "默认只回未读");
        assert!(pending[0].acked_at.is_none());
        // include_acked=1：历史保留不删
        let after = inbox_with(&h, "ack-agent", 0, true).await;
        assert_eq!(after.len(), 2, "ack 后历史保留");
        assert!(after[0].acked_at.is_some(), "seq<=ack 的标已读");
        assert!(after[1].acked_at.is_none(), "seq>ack 的保持未读");
        // 未知 agent ack → 404；缺 seq → 400（客户端错误，B2）
        let ghost = h
            .handle(post_req(
                "/api/v1/agents/ghost/ack",
                serde_json::json!({"seq": 1}),
            ))
            .await
            .unwrap();
        assert_eq!(ghost.status, 404);
        assert_eq!(
            h.handle(post_req(
                "/api/v1/agents/ack-agent/ack",
                serde_json::json!({})
            ))
            .await
            .unwrap()
            .status,
            400,
            "缺 seq 应 400（非 500）"
        );
    }

    // 12. 协议文本端点：自举可读，含 @ 规矩与收件箱路径
    #[tokio::test]
    async fn protocol_endpoint_returns_text() {
        let h = handler();
        let resp = h.handle(get_req("/api/v1/agents/protocol")).await.unwrap();
        assert_eq!(resp.status, 200);
        let text = resp.body["protocol"].as_str().unwrap();
        assert!(text.contains("必须 @对方"), "协议须立 @ 规矩: {text}");
        assert!(text.contains("agent 不认领"));
        assert!(text.contains("/api/v1/agents/<name>/inbox?after="));
        assert_eq!(
            resp.body["endpoints"]["register"],
            "/api/v1/agents/register"
        );
    }

    // 13. 协议声明：注册即向所在群组发系统消息；重复注册不重发（同群一次）
    #[tokio::test]
    async fn declaration_posted_once_per_group() {
        let im_h = im::ImRouteHandler::with_empty();
        let h = handler().with_im_bridge(im_h.coord_bridge());
        // 真实链上身份三步登录拿 token（im 用户面与生产同栈）
        let (owner_pk, token) = im_login(&im_h).await;
        let agent_pk = fake_pubkey(21);
        // 建群：owner + agent pubkey 两个成员
        let group = im_h
            .handle(im_authed(
                HttpMethod::Post,
                "/api/v1/im/groups",
                &token,
                serde_json::json!({"name": "协调组", "members": [owner_pk, agent_pk]}),
            ))
            .await
            .unwrap();
        assert_eq!(group.status, 201, "{}", group.body);
        let gid = group.body["id"].as_str().unwrap().to_string();
        // 注册 → 声明进群
        let reg = register(
            &h,
            serde_json::json!({"name": "decl-agent", "pubkey": agent_pk}),
        )
        .await;
        assert_eq!(reg.status, 201);
        assert_eq!(
            reg.body["declared_now"],
            serde_json::json!(1),
            "应声明 1 个群"
        );
        assert_eq!(
            reg.body["declared_groups"],
            serde_json::json!([gid]),
            "declared_groups 落档"
        );
        let msgs = im_group_messages(&im_h, &gid, &token).await;
        let decls: Vec<_> = msgs
            .iter()
            .filter(|m| {
                m["sender_kind"] == "system"
                    && m["content"].as_str().unwrap().contains("@decl-agent")
            })
            .collect();
        assert_eq!(decls.len(), 1, "声明只发一次: {msgs:?}");
        assert!(decls[0]["content"]
            .as_str()
            .unwrap()
            .contains("未 @ 的消息 agent 不认领"));
        // 重复注册 → 不重发
        let again = register(
            &h,
            serde_json::json!({"name": "decl-agent", "pubkey": agent_pk}),
        )
        .await;
        assert_eq!(again.status, 200);
        assert_eq!(
            again.body["declared_now"],
            serde_json::json!(0),
            "重复注册不重发"
        );
        let msgs2 = im_group_messages(&im_h, &gid, &token).await;
        let decls2 = msgs2
            .iter()
            .filter(|m| {
                m["sender_kind"] == "system"
                    && m["content"].as_str().unwrap().contains("@decl-agent")
            })
            .count();
        assert_eq!(decls2, 1, "群里仍只有一条声明");
    }

    // 14. 无 pubkey（或未注入 IM 桥）→ 注册成功但跳过声明
    #[tokio::test]
    async fn declaration_skipped_without_pubkey() {
        let h = handler();
        let resp = register(&h, serde_json::json!({"name": "anon-agent"})).await;
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["declared_now"], serde_json::json!(0));
    }

    // 15. 删除：连带收件箱；重复删 404
    #[tokio::test]
    async fn delete_agent_removes_entry_and_inbox() {
        let h = handler();
        register(&h, serde_json::json!({"name": "gone-agent"})).await;
        h.route(&msg_in("g", "0xa", "@gone-agent 走了"));
        assert_eq!(inbox(&h, "gone-agent", 0).await.len(), 1);
        let resp = h
            .handle(delete_req("/api/v1/agents/gone-agent"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let list = h.handle(get_req("/api/v1/agents")).await.unwrap();
        assert_eq!(list.body["count"], serde_json::json!(0));
        let inbox_resp = h
            .handle(get_req("/api/v1/agents/gone-agent/inbox"))
            .await
            .unwrap();
        assert_eq!(inbox_resp.status, 404, "收件箱随 agent 删除");
        let again = h
            .handle(delete_req("/api/v1/agents/gone-agent"))
            .await
            .unwrap();
        assert_eq!(again.status, 404);
    }

    // 16. 持久化：注册+投递后同路径重启（新 handler 读回），原子写不留 .tmp
    #[tokio::test]
    async fn state_persists_across_restart() {
        let path = std::env::temp_dir().join(format!("agents-test-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let path = path.to_str().unwrap().to_string();
        let h = AgentCoordRouteHandler::with_state_path(&path);
        register(
            &h,
            serde_json::json!({"name": "disk-agent", "pubkey": fake_pubkey(33)}),
        )
        .await;
        h.route(&msg_in("g", "0xa", "@disk-agent 落盘"));
        drop(h);
        assert!(
            !std::path::Path::new(&format!("{path}.tmp")).exists(),
            "原子写不留 .tmp"
        );
        let h2 = AgentCoordRouteHandler::with_state_path(&path);
        let list = h2.handle(get_req("/api/v1/agents")).await.unwrap();
        assert_eq!(list.body["count"], serde_json::json!(1), "重启后注册表读回");
        let records = inbox(&h2, "disk-agent", 0).await;
        assert_eq!(records.len(), 1, "重启后收件箱读回");
        let _ = std::fs::remove_file(&path);
    }

    // 17. im 钩子端到端：install_hook 后经 im 发消息路径 @ → 收件箱命中
    //     （验证 im.rs 挂钩一行 + 本模块钩子桥的完整链路）
    #[tokio::test]
    async fn im_hook_routes_mentions_end_to_end() {
        let im_h = im::ImRouteHandler::with_empty();
        let h = handler().with_im_bridge(im_h.coord_bridge());
        install_hook(h.core());
        // 测试结束（含断言失败）自动摘钩，避免影响并行用例
        struct HookGuard;
        impl Drop for HookGuard {
            fn drop(&mut self) {
                clear_hook();
            }
        }
        let _guard = HookGuard;
        register(&h, serde_json::json!({"name": "e2e-agent"})).await;
        let (_owner_pk, token) = im_login(&im_h).await;
        let conv = im_h
            .handle(im_authed(
                HttpMethod::Post,
                "/api/v1/im/conversations",
                &token,
                serde_json::json!({"name": "e2e 会话"}),
            ))
            .await
            .unwrap();
        assert_eq!(conv.status, 201, "{}", conv.body);
        let cid = conv.body["id"].as_str().unwrap().to_string();
        let sent = im_h
            .handle(im_authed(
                HttpMethod::Post,
                &format!("/api/v1/im/conversations/{cid}/messages"),
                &token,
                serde_json::json!({"content": "@e2e-agent 请接管构建"}),
            ))
            .await
            .unwrap();
        assert_eq!(sent.status, 201, "{}", sent.body);
        let sent_id = sent.body["id"].as_str().unwrap().to_string();
        let records = inbox(&h, "e2e-agent", 0).await;
        assert_eq!(records.len(), 1, "经 im 发消息路径应命中投递: {records:?}");
        assert_eq!(records[0].group_id, cid);
        assert_eq!(records[0].delivered, "inbox");
        assert_eq!(
            records[0].message_id, sent_id,
            "记录关联 im 实际落库的消息 id"
        );
    }

    // ---- 测试辅助：im 登录（k256 真密钥对，与 im.rs 测试同栈）----

    fn new_key() -> k256::ecdsa::SigningKey {
        use k256::elliptic_curve::rand_core::OsRng;
        k256::ecdsa::SigningKey::random(&mut OsRng)
    }

    fn pubkey_hex(sk: &k256::ecdsa::SigningKey) -> String {
        format!(
            "0x{}",
            hex::encode(sk.verifying_key().to_encoded_point(true).as_bytes())
        )
    }

    fn sign_nonce(sk: &k256::ecdsa::SigningKey, nonce: &str) -> [u8; 65] {
        use sha2::Digest;
        let digest = sha2::Sha256::new_with_prefix(nonce.as_bytes());
        let (sig, recid) = sk.sign_digest_recoverable(digest).expect("签名必成功");
        let mut out = [0u8; 65];
        out[..64].copy_from_slice(&sig.to_bytes());
        out[64] = u8::from(recid);
        out
    }

    /// 真密钥对全流程登录：challenge → sign → verify → `(pubkey, token)`。
    async fn im_login(im_h: &im::ImRouteHandler) -> (String, String) {
        let sk = new_key();
        let pubkey = pubkey_hex(&sk);
        let resp = im_h
            .handle(post_req(
                "/api/v1/im/auth/challenge",
                serde_json::json!({"pubkey": pubkey}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let nonce = resp.body["nonce"].as_str().unwrap().to_string();
        let sig = hex::encode(sign_nonce(&sk, &nonce));
        let resp = im_h
            .handle(post_req(
                "/api/v1/im/auth/verify",
                serde_json::json!({
                    "pubkey": pubkey,
                    "nonce": nonce,
                    "signature": format!("0x{sig}")
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "verify 应成功: {}", resp.body);
        let token = resp.body["token"].as_str().unwrap().to_string();
        (pubkey, token)
    }

    /// 带 token 读会话消息历史（声明断言用）。
    async fn im_group_messages(
        im_h: &im::ImRouteHandler,
        gid: &str,
        token: &str,
    ) -> Vec<serde_json::Value> {
        let resp = im_h
            .handle(im_authed(
                HttpMethod::Get,
                &format!("/api/v1/im/conversations/{gid}/messages"),
                token,
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{}", resp.body);
        resp.body.as_array().unwrap().clone()
    }

    // ---- 测试辅助：mock webhook（std TcpListener 手写 HTTP/1.1，
    //      api_market spawn_fake_json_server 同款手法 + 请求体捕获）----

    /// 起一个本地 mock webhook 接收端（std::net::TcpListener + 线程）：
    /// 逐连接按 Content-Length 收完请求，把**原始请求文本**记进 `seen`，
    /// 回 200；至多服务 `max` 个连接。返回完整接收端点 url。
    fn spawn_mock_webhook(seen: Arc<std::sync::Mutex<Vec<String>>>, max: usize) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 失败");
        let addr = listener.local_addr().expect("local_addr 失败");
        std::thread::spawn(move || {
            for _ in 0..max {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut acc: Vec<u8> = Vec::new();
                let mut buf = [0u8; 16384];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
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
                    }
                }
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&acc).into_owned());
                let resp = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{addr}/agent-hook")
    }
}
