//! `P2pRouteHandler` —— 把 os-p2p 组网层（`crates/os-p2p`）的观察面与控制面
//! 适配为 REST（设计 docs/NEXOS_P2P_NETWORK_DESIGN.md，P2b 接入）。
//!
//! # 定位
//!
//! os-api 启动时若 `NEXOS_P2P_ENABLE=1`（默认关——不影响无 P2P 需求的部署），
//! main.rs 在 tokio runtime 内 `P2pNode::spawn(config_from_env())` 内嵌一个
//! 组网节点（env 透传 `NEXOS_P2P_BOOTSTRAP/LISTEN/PUBLIC/KEY_FILE/NAME/MDNS`；
//! 私钥持久化 → 重启 NodeID 稳定），Handle 存入本 handler：
//!
//! - **读公开**（`requires_auth=false`）：status / peers / buckets / ladder
//!   ——网络页拓扑 UI 的 5s 轮询数据源；
//! - **写 admin**（`required_roles=["admin"]`）：send（发消息）/ connect
//!   （主动走连接阶梯：直连 → 打洞 → 中继）。
//!
//! 未启用（spawn 未发生）时全部端点返回 **503** +
//! `{"error":"P2P 未启用（NEXOS_P2P_ENABLE=1）"}`——语义化引导而非 404，
//! 前端据此展示开启指引文案。
//!
//! # 端点契约
//!
//! | 方法 | 路径 | 鉴权 | 语义 |
//! |---|---|---|---|
//! | GET | `/api/v1/p2p/status` | 公开 | 自身 NodeID/OverlayAddr/昵称/角色 + listen + 启用态 |
//! | GET | `/api/v1/p2p/peers` | 公开 | 路由表摘要（NodeID/underlay/public/连接态/中继路由） |
//! | GET | `/api/v1/p2p/buckets` | 公开 | 非空 k-bucket 摘要（po/count/entries）+ 端点簿 |
//! | GET | `/api/v1/p2p/ladder` | 公开 | 连接阶梯统计（direct/punched/relayed/punch_failed） |
//! | GET | `/api/v1/p2p/identity-conflicts` | 公开 | 身份冲突观测（同公钥多地址进入；仅提示不阻断） |
//! | POST | `/api/v1/p2p/send` | admin | `{node_id, text}` 经组网层发应用消息 |
//! | POST | `/api/v1/p2p/connect` | admin | `{node_id}` 主动连接阶梯，返回实际路径 |
//! | POST | `/api/v1/p2p/add-peer` | 公开（开发期） | `{addr: "ip:port"}` 按地址直拨（手动添加节点；无端口默认 7070） |
//! | GET | `/api/v1/p2p/node-meta` | 公开 | 节点元数据注册表快照（meta 组件：id/addrs/first_seen/last_seen/state/source，按健康分降序、Inactive 殿后） |
//! | POST | `/api/v1/p2p/node-meta/:id/reactivate` | 公开（开发期） | 手动触发元数据心跳（`:id` = 0x+66 hex）——Inactive→Active{score:30} 并立即探测一次，返回 `{ok, probed}` |
//!
//! # 观察面字段来源
//!
//! - `status.self.node_id / overlay_addr` ← `Handle::self_id()`（secp256k1 公钥
//!   / EVM 同源 20 字节，与 chain_auth 身份体系同源）；
//! - `peers[]` ← `Handle::peers()`（`PeerInfo` 直接可序列化）；
//! - `buckets.buckets[]` ← `Handle::buckets_summary()`（`BucketStat`）；
//!   `buckets.known_endpoints[]` ← `Handle::known_endpoints()`——`EndpointEntry`
//!   含 `Instant` 不可序列化，此处转 `{id, addr}` DTO；
//! - `ladder` ← `Handle::ladder_stats()`（`LadderStats`）；
//! - `node-meta` ← `Handle::node_meta()`（meta 组件注册表快照，条目
//!   `NodeMetaEntry` 直接可序列化——节点存活判定的唯一账本，其他组件从这里取）。
//!
//! # 请求体解析
//!
//! 网关 dispatch 不做 body schema 校验，本 handler 自行解析（缺字段 400）：
//! `node_id` 必须是 `0x`+66 hex 合法压缩公钥（`NodeId::parse`），`text` 必填。
//!
//! # P3 联邦桥（2026-08-22，设计文档 §8）
//!
//! 本模块另承载 os-p2p 之上的第一批消费者（IM 跨节点大厅消息 + NexHub 跨节点
//! 项目发现）：
//!
//! - [`fed_broadcast`]：把联邦载荷 fan-out 给全部已连接 peer（发送端共用）；
//! - [`P2pLobbyTransport`]：`os_nexhub::LobbyFedTransport` 的 os-p2p 实现
//!   （依赖反转——os-nexhub 不依赖 os-p2p）；
//! - [`FederationBridge`]：入站消息按 `payload.fed` 分发给 im/nexhub 接收端
//!   （main.rs 在 p2p spawn 后装配进观测 task，替代 P2b 的纯日志面）；
//! - [`spawn_conn_watcher`]：连接建立观测 task（os-p2p 无连接事件面，1s 轮询
//!   diff 的最小回调注入）——市场联邦 on-connect 补推的挂点（2026-09-03
//!   覆盖缺口修复，见 api_market 模块文档与 docs/API_MARKET.md §9）。
//!
//! # 身份冲突检测（2026-08-23，仅本地警告——不阻断、不踢人）
//!
//! 身份 = 密钥是设计特性：多个 OS 用同一私钥进入时权限相同、文件不同步。
//! 两层本地提示（均不拦截任何消息/连接）：
//!
//! - **握手层**（os-p2p `register_conn`）：对端自报 NodeID == 本机 NodeID →
//!   `eprintln` 警告 + 记账，`GET /api/v1/p2p/identity-conflicts` 观测
//!   （记账在 os-identity 身份账本——2026-08-25 组件抽离后随账本持久化；
//!   规范查询路径见 `/api/v1/identity/conflicts`）；
//! - **联邦消息层**（[`FederationBridge::dispatch`] → `ImFederation::
//!   warn_if_identity_conflict`）：入站联邦消息发送者 NodeID == 本机
//!   NodeID → 本地大厅写一条系统警告消息（同源 5 分钟去重），消息照常处理。

use async_trait::async_trait;
use os_p2p::{BucketStat, Handle, NodeId};
use serde::Serialize;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// Handler 主体
// ----------------------------------------------------------------------------

/// P2P 组网路由处理器——HTTP 边界适配到 os-p2p [`Handle`]。
///
/// - `Some(handle)`：os-api 启动时 spawn 的内嵌组网节点（`NEXOS_P2P_ENABLE=1`）；
/// - `None`：未启用——全部端点 503 + 引导文案（默认构造即此态）。
pub struct P2pRouteHandler {
    handle: Option<Handle>,
    /// 节点昵称（`NEXOS_P2P_NAME`；status 展示）。
    name: String,
    /// 公网服务节点声明（`NEXOS_P2P_PUBLIC=1`；status 角色展示）。
    public: bool,
}

impl P2pRouteHandler {
    /// 未启用构造（默认部署：`NEXOS_P2P_ENABLE` 未设/为 0）。
    ///
    /// 全部端点返回 503 + `{"error":"P2P 未启用（NEXOS_P2P_ENABLE=1）"}`。
    #[must_use]
    pub fn new_disabled() -> Self {
        Self {
            handle: None,
            name: String::new(),
            public: false,
        }
    }

    /// 已启用构造（main.rs 装配：spawn 成功后传入 Handle + env 元数据）。
    #[must_use]
    pub fn new(handle: Handle, name: String, public: bool) -> Self {
        Self {
            handle: Some(handle),
            name,
            public,
        }
    }

    /// 是否已启用（--check 诊断 / 测试用）。
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.handle.is_some()
    }

    /// 未启用统一 503 语义（前端凭 error 文案展示开启指引）。
    fn disabled_response() -> ApiResponse {
        ApiResponse {
            status: 503,
            body: serde_json::json!({"error": DISABLED_MSG}),
            headers: serde_json::json!({}),
        }
    }
}

impl Default for P2pRouteHandler {
    fn default() -> Self {
        Self::new_disabled()
    }
}

#[async_trait]
impl RouteHandler for P2pRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec_read(HttpMethod::Get, PATH_STATUS),
            spec_read(HttpMethod::Get, PATH_PEERS),
            spec_read(HttpMethod::Get, PATH_BUCKETS),
            spec_read(HttpMethod::Get, PATH_LADDER),
            spec_read(HttpMethod::Get, PATH_IDENTITY_CONFLICTS),
            spec_admin(HttpMethod::Post, PATH_SEND),
            spec_admin(HttpMethod::Post, PATH_CONNECT),
            spec_read(HttpMethod::Post, PATH_ADD_PEER), // 开发期公开（用户指示：先不搞复杂认证）
            spec_read(HttpMethod::Get, PATH_NODE_META),
            // 开发期公开（同 add-peer——手动心跳是运维动作，非越权面）
            spec_read(HttpMethod::Post, PATH_NODE_META_REACTIVATE),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let path = req.path.split('?').next().unwrap_or("");
        // —— 动态路由（:id 参数）：POST node-meta/:id/reactivate ——
        // NodeID（0x+66 hex）不含 '/'，前缀+后缀剥离无歧义（storage.rs 同款模式）。
        if req.method == HttpMethod::Post {
            if let Some(id) = path
                .strip_prefix(PATH_NODE_META_PREFIX)
                .and_then(|r| r.strip_suffix(PATH_REACTIVATE_SUFFIX))
            {
                return match &self.handle {
                    Some(h) => self.handle_meta_reactivate(h, id).await,
                    None => Ok(Self::disabled_response()),
                };
            }
        }
        match (req.method, path) {
            (HttpMethod::Get, PATH_STATUS) => Ok(match &self.handle {
                Some(h) => self.status_response(h).await,
                None => Self::disabled_response(),
            }),
            (HttpMethod::Get, PATH_PEERS) => Ok(match &self.handle {
                Some(h) => {
                    let peers = h.peers().await;
                    ok_json(to_value(&peers)?)
                }
                None => Self::disabled_response(),
            }),
            (HttpMethod::Get, PATH_BUCKETS) => Ok(match &self.handle {
                Some(h) => {
                    let buckets = h.buckets_summary().await;
                    // EndpointEntry 含 Instant（不可序列化）→ 转 {id, addr} DTO
                    let endpoints: Vec<EndpointDto> = h
                        .known_endpoints()
                        .await
                        .into_iter()
                        .map(|e| EndpointDto {
                            id: e.id.to_hex(),
                            addr: e.addr.to_string(),
                        })
                        .collect();
                    ok_json(to_value(&BucketsResp {
                        buckets,
                        known_endpoints: endpoints,
                    })?)
                }
                None => Self::disabled_response(),
            }),
            (HttpMethod::Get, PATH_LADDER) => Ok(match &self.handle {
                Some(h) => {
                    let ladder = h.ladder_stats().await;
                    ok_json(to_value(&ladder)?)
                }
                None => Self::disabled_response(),
            }),
            // 身份冲突观测（仅提示不阻断——同公钥多地址进入的本地知情面）
            (HttpMethod::Get, PATH_IDENTITY_CONFLICTS) => Ok(match &self.handle {
                Some(h) => {
                    let conflicts = h.identity_conflicts().await;
                    ok_json(to_value(&conflicts)?)
                }
                None => Self::disabled_response(),
            }),
            // 节点元数据注册表快照（meta 组件观察面——节点存活判定的唯一账本）
            (HttpMethod::Get, PATH_NODE_META) => Ok(match &self.handle {
                Some(h) => {
                    let meta = h.node_meta().await;
                    ok_json(to_value(&meta)?)
                }
                None => Self::disabled_response(),
            }),
            (HttpMethod::Post, PATH_SEND) => match &self.handle {
                Some(h) => self.handle_send(h, &req.body).await,
                None => Ok(Self::disabled_response()),
            },
            (HttpMethod::Post, PATH_CONNECT) => match &self.handle {
                Some(h) => self.handle_connect(h, &req.body).await,
                None => Ok(Self::disabled_response()),
            },
            (HttpMethod::Post, PATH_ADD_PEER) => match &self.handle {
                Some(h) => self.handle_add_peer(h, &req.body).await,
                None => Ok(Self::disabled_response()),
            },
            // —— 未覆盖的路由 —— 兜底 404（Ok，非 Err，与其它 handler 同款）
            _ => Ok(error_response(404, "p2p: 未匹配的路由")),
        }
    }
}

impl P2pRouteHandler {
    /// GET status：自身身份 + 监听 + 启用态 + 观察面摘要。
    async fn status_response(&self, handle: &Handle) -> ApiResponse {
        let (peers, buckets) = tokio::join!(handle.peers(), handle.buckets_summary());
        let connected = peers.iter().filter(|p| p.connected).count();
        let known: usize = buckets.iter().map(|b| b.count).sum();
        ok_json(serde_json::json!({
            "enabled": true,
            "self": {
                // NodeID = secp256k1 压缩公钥（0x+66 hex）；OverlayAddr = EVM 同源 20 字节
                "node_id": handle.self_id().to_hex(),
                "overlay_addr": handle.self_id().overlay().to_hex(),
                "name": self.name,
                "public": self.public,
            },
            "listen": handle.listen_addr().to_string(),
            "peers_known": known,
            "peers_connected": connected,
        }))
    }

    /// POST send：`{node_id, text}` → `Handle::send`（fire-and-forget，
    /// 无路由时入 pending_out 并触发 lookup——送达效果在对端 on_msg 体现）。
    async fn handle_send(
        &self,
        handle: &Handle,
        body: &serde_json::Value,
    ) -> Result<ApiResponse, ApiGatewayError> {
        let Some(to) = parse_node_id_field(body) else {
            return Ok(error_response(
                400,
                "body 需要 {node_id, text}：node_id 缺失或非法（0x+66 hex 压缩公钥）",
            ));
        };
        let Some(text) = body.get("text").and_then(|v| v.as_str()) else {
            return Ok(error_response(
                400,
                "body 需要 {node_id, text}：text 缺失或非字符串",
            ));
        };
        if text.is_empty() {
            return Ok(error_response(400, "text 不能为空"));
        }
        handle.send(&to, serde_json::json!({ "text": text }));
        Ok(ok_json(serde_json::json!({
            "ok": true,
            "to": to.to_hex(),
            "note": "fire-and-forget：无路由时暂存并触发查找（pending_out），送达在对端体现",
        })))
    }

    /// POST connect：`{node_id}` → 连接阶梯（已直连短路 → underlay 直拨 →
    /// 观测端点打洞 → 中继兜底）。打洞可耗时数秒（3 轮 × 800ms），前端超时
    /// 应放宽；失败（全阶梯未建立）返回 502 + 原因。
    async fn handle_connect(
        &self,
        handle: &Handle,
        body: &serde_json::Value,
    ) -> Result<ApiResponse, ApiGatewayError> {
        let Some(target) = parse_node_id_field(body) else {
            return Ok(error_response(
                400,
                "body 需要 {node_id}：缺失或非法（0x+66 hex 压缩公钥）",
            ));
        };
        match handle.connect(&target).await {
            Ok(path) => Ok(ok_json(serde_json::json!({
                "ok": true,
                "node_id": target.to_hex(),
                "path": path,
            }))),
            Err(e) => Ok(error_response(
                502,
                &format!("连接阶梯失败（直连/打洞/中继全未建立）: {e}"),
            )),
        }
    }

    /// POST node-meta/:id/reactivate：手动触发元数据心跳（复活的路径之一）——
    /// `Handle::meta_reactivate`：Inactive → Active{score:30} 并**立即探测一次**
    /// （活连接即成功；否则纯 TCP connect）。返回 `{ok, probed}`：`probed` =
    /// 本次探测结果（true = 探活成功，条目已复活；false = 不可达或注册表无
    /// 此节点）。body 无（`:id` 路径参数，0x+66 hex 压缩公钥）。
    async fn handle_meta_reactivate(
        &self,
        handle: &Handle,
        raw_id: &str,
    ) -> Result<ApiResponse, ApiGatewayError> {
        let Some(id) = NodeId::parse(raw_id) else {
            return Ok(error_response(
                400,
                "路径 :id 非法（应为 0x+66 hex 压缩公钥）",
            ));
        };
        // 探测可耗时数秒（逐地址 TCP connect × ping 超时），前端超时应放宽
        let probed = handle.meta_reactivate(&id).await;
        Ok(ok_json(serde_json::json!({
            "ok": true,
            "node_id": id.to_hex(),
            "probed": probed,
            "note": if probed {
                "探活成功——条目已复活（Active{score:30}，心跳引擎恢复跟踪）"
            } else {
                "探测未通过（不可达或注册表无此节点）——条目保持/回到非活跃"
            },
        })))
    }

    /// POST add-peer：`{addr: "ip:port"}` → `Handle::dial` 按地址直拨
    /// （bootstrap 拨号同款路径：TCP connect + 挑战-签名 + ECDH + 注册入桶）。
    ///
    /// - addr 无端口时默认补 [`os_p2p::P2P_PORT_DEFAULT`]（7070）；
    /// - 已直连该地址（underlay 或观测端点簿命中）→ 短路成功——重复拨号会被
    ///   `register_conn` 去重拒绝，不应把已建立的连接误报为失败；
    /// - 拨号+握手可耗时数秒（connect 超时 + 握手上限 5s），前端超时应放宽；
    ///   失败（不可达 / 握手失败 / 版本不兼容）返回 502 + 原因。
    async fn handle_add_peer(
        &self,
        handle: &Handle,
        body: &serde_json::Value,
    ) -> Result<ApiResponse, ApiGatewayError> {
        let Some(raw) = body
            .get("addr")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return Ok(error_response(
                400,
                "body 需要 {addr: \"ip:port\"}：addr 缺失或为空（示例 192.0.2.113:7070）",
            ));
        };
        let Some(addr) = parse_addr_field(raw).await else {
            return Ok(error_response(
                400,
                &format!("addr 非法（应为 ip:port，无端口默认 7070）: {raw}"),
            ));
        };
        // 已直连该地址 → 短路成功（防重复拨号被 register_conn 去重误报失败）
        let (peers, endpoints) = tokio::join!(handle.peers(), handle.known_endpoints());
        let connected: HashSet<&NodeId> = peers
            .iter()
            .filter(|p| p.connected)
            .map(|p| &p.id)
            .collect();
        let already: Option<&NodeId> = peers
            .iter()
            .find(|p| p.connected && p.underlay.as_ref() == Some(&addr))
            .map(|p| &p.id)
            .or_else(|| {
                endpoints
                    .iter()
                    .find(|e| e.addr == addr && connected.contains(&e.id))
                    .map(|e| &e.id)
            });
        if let Some(id) = already {
            return Ok(ok_json(serde_json::json!({
                "ok": true,
                "node_id": id.to_hex(),
                "addr": addr.to_string(),
                "note": "already-connected",
            })));
        }
        match handle.dial(addr).await {
            Ok(id) => {
                // 回读路由表行（成功即入桶并计入 peers——前端据此直接展示）
                let peers = handle.peers().await;
                let peer = peers.iter().find(|p| p.id == id);
                Ok(ok_json(serde_json::json!({
                    "ok": true,
                    "node_id": id.to_hex(),
                    "addr": addr.to_string(),
                    "peer": peer.map(|p| serde_json::json!({
                        "id": p.id.to_hex(),
                        "underlay": p.underlay.map(|a| a.to_string()),
                        "public": p.public,
                        "connected": p.connected,
                    })),
                })))
            }
            Err(e) => Ok(error_response(502, &format!("拨号 {addr} 失败: {e}"))),
        }
    }
}

// ----------------------------------------------------------------------------
// DTO（对齐 os-p2p 观察面；Instant 等不可序列化字段在此转纯数据）
// ----------------------------------------------------------------------------

/// buckets 响应：非空桶摘要 + 观测端点簿（地址交换所）。
#[derive(Serialize)]
struct BucketsResp {
    /// 非空 k-bucket 摘要（po 越大越近；每桶 k=16）。
    buckets: Vec<BucketStat>,
    /// 观测端点簿 `{NodeID → 网络观测 ip:port}`（NAT 映射口）。
    known_endpoints: Vec<EndpointDto>,
}

/// 端点簿条目 DTO（`EndpointEntry.last_seen: Instant` 不可序列化，剥除）。
#[derive(Serialize)]
struct EndpointDto {
    id: String,
    addr: String,
}

// ----------------------------------------------------------------------------
// 内部辅助（与其它 handler 同款）
// ----------------------------------------------------------------------------

/// `GET /api/v1/p2p/status`——自身身份/监听/启用态。
const PATH_STATUS: &str = "/api/v1/p2p/status";
/// `GET /api/v1/p2p/peers`——路由表摘要。
const PATH_PEERS: &str = "/api/v1/p2p/peers";
/// `GET /api/v1/p2p/buckets`——k-bucket 摘要 + 端点簿。
const PATH_BUCKETS: &str = "/api/v1/p2p/buckets";
/// `GET /api/v1/p2p/ladder`——连接阶梯统计。
const PATH_LADDER: &str = "/api/v1/p2p/ladder";
/// `GET /api/v1/p2p/identity-conflicts`——身份冲突观测（同公钥多地址进入）。
const PATH_IDENTITY_CONFLICTS: &str = "/api/v1/p2p/identity-conflicts";
/// `POST /api/v1/p2p/send`——发应用消息（admin）。
const PATH_SEND: &str = "/api/v1/p2p/send";
/// `POST /api/v1/p2p/connect`——主动连接阶梯（admin）。
const PATH_CONNECT: &str = "/api/v1/p2p/connect";
/// `POST /api/v1/p2p/add-peer`——按地址直拨手动添加节点（公开，开发期免认证）。
const PATH_ADD_PEER: &str = "/api/v1/p2p/add-peer";
/// `GET /api/v1/p2p/node-meta`——节点元数据注册表快照（meta 组件观察面）。
const PATH_NODE_META: &str = "/api/v1/p2p/node-meta";
/// `POST /api/v1/p2p/node-meta/:id/reactivate`——手动触发元数据心跳（公开，开发期）。
const PATH_NODE_META_REACTIVATE: &str = "/api/v1/p2p/node-meta/:id/reactivate";
/// reactivate 动态路由的 `:id` 剥离前缀（spec 声明用上面的参数式路径）。
const PATH_NODE_META_PREFIX: &str = "/api/v1/p2p/node-meta/";
/// reactivate 动态路由的 `:id` 剥离后缀。
const PATH_REACTIVATE_SUFFIX: &str = "/reactivate";

/// 本 handler 注册时的组件名（`RouteSpec::handler_component`）。
const COMPONENT: &str = "p2p";

/// 未启用统一文案（503 body 的 error 字段——前端凭此展示指引）。
const DISABLED_MSG: &str = "P2P 未启用（NEXOS_P2P_ENABLE=1）";

/// 构造一条只读路由规格（公开——组网拓扑观察面不涉敏感数据）。
fn spec_read(method: HttpMethod, path: &str) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: COMPONENT.to_string(),
        requires_auth: false,
        required_roles: Vec::new(),
    }
}

/// 构造一条写路由规格（admin——发消息/主动连接是改变组网状态的操作）。
fn spec_admin(method: HttpMethod, path: &str) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: COMPONENT.to_string(),
        requires_auth: true,
        required_roles: vec!["admin".to_string()],
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

/// 从请求体解析 `node_id` 字段（`0x`+66 hex 压缩公钥，`NodeId::parse` 契约）。
fn parse_node_id_field(body: &serde_json::Value) -> Option<NodeId> {
    body.get("node_id")
        .and_then(|v| v.as_str())
        .and_then(NodeId::parse)
}

/// 解析 add-peer 的 `addr` 字段：裸 IP/主机名（无 `:`）自动补默认端口
/// [`os_p2p::P2P_PORT_DEFAULT`]（7070），再经 `lookup_host` 异步解析
/// （支持 `ip:port` 与 `hostname:port`）。
async fn parse_addr_field(raw: &str) -> Option<SocketAddr> {
    let s = raw.trim();
    if s.is_empty() || s.contains(char::is_whitespace) {
        return None;
    }
    let with_port = if s.contains(':') {
        s.to_string()
    } else {
        format!("{s}:{}", os_p2p::P2P_PORT_DEFAULT)
    };
    let mut resolved = tokio::net::lookup_host(&with_port).await.ok()?;
    resolved.next()
}

// ----------------------------------------------------------------------------
// P3 联邦桥：os-p2p 入站/出站 ⇄ IM 大厅 + NexHub 大厅
//（设计 docs/NEXOS_P2P_NETWORK_DESIGN.md §8——本节是 os-p2p 之上第一批消费者）
// ----------------------------------------------------------------------------

/// 联邦广播：把载荷逐个 `send` 给**每个已连接 peer**（fire-and-forget；
/// 无路由时 os-p2p 暂存并触发查找）。返回送达目标的 peer 数（0 = 孤网）。
///
/// 注意广播范围 = **当前已连接**的 peer（一跳）——接收方 ingest 落地后不
/// 转播，天然无环；更大的覆盖面交给 os-p2p 的 DHT 路由（send 逐对端直达）。
///
/// **本地指纹目标跳过**：指纹==本机 NodeID 的目标（同私钥多 OS 实例场景）
/// 不经 P2P——消息已在本地落库，发给同指纹节点只会自回路重复入库。IM 联邦
/// 大厅广播与 NexHub 大厅推送（`P2pLobbyTransport`）共用此过滤。
pub async fn fed_broadcast(handle: &Handle, payload: serde_json::Value) -> usize {
    let peers = handle.peers().await;
    let targets: Vec<NodeId> = peers
        .into_iter()
        .filter(|p| p.connected)
        // 指纹==本机 → 本地已落库，P2P 自回路只会造成重复入库
        .filter(|p| !handle.is_local_target(&p.id))
        .map(|p| p.id)
        .collect();
    let count = targets.len();
    for id in targets {
        handle.send(&id, payload.clone());
    }
    count
}

/// `os_nexhub::LobbyFedTransport` 的 os-p2p 实现（main.rs 装配注入 nexhub
/// 联邦端点——os-nexhub 不依赖 os-p2p，经通道抽象反转依赖方向）。
///
/// `broadcast` 是同步 trait 方法：内部 tokio::spawn 异步 fan-out（发布路径
/// 不被联邦传播阻塞；fire-and-forget 语义与 IM 侧一致）。fan-out 经
/// [`fed_broadcast`]——本地指纹目标（NodeID==本机，同私钥多 OS 实例）在该处
/// 统一跳过：本地效果已达成，P2P 自回路只会重复入库。
pub struct P2pLobbyTransport(pub Handle);

impl os_nexhub::LobbyFedTransport for P2pLobbyTransport {
    fn broadcast(&self, payload: serde_json::Value) {
        let handle = self.0.clone();
        tokio::spawn(async move {
            fed_broadcast(&handle, payload).await;
        });
    }
}

/// 连接观测轮询周期（[`spawn_conn_watcher`] 缺省拍距）：1s——一拍 `peers()`
/// 命令往返（mpsc enum + 小 Vec）成本可忽略，窗口足够抓住"活了至少一拍"的
/// 连接；更短命的闪断由定期重播兜底（api_market fed 端点 30 分钟一轮）。
pub const FED_CONN_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// 连接建立观测 task（市场联邦 **on-connect 补推**的事件面，2026-09-03 覆盖
/// 缺口修复）：轮询 `Handle::peers()`，对**新出现**的已连接 peer 逐个同步回调
/// `on_connected(&NodeId)`（main.rs 装配的回调里 spawn
/// `ApiMarketFedEndpoint::backfill_to` 补推全部 federated 条目）。
///
/// # 挂点说明（为何是轮询观测而非事件订阅）
///
/// os-p2p **没有**现成的连接建立通知面：`register_conn` 只写内部表（meta 组件
/// 经组件内直调感知、identity 走账本），对上层消费者只有 `on_msg()` 应用消息
/// 广播——连接本身无事件可订阅。故在 os-api 侧（本模块的桥/连接观测处）加
/// 这条最小回调注入：1s 一拍 diff 已连接集合，语义等价"连接建立事件"（首拍
/// 即种子——进程启动时对既有连接也触发补推，重启覆盖缺口同理修复）。
///
/// # 过滤与生命周期
///
/// - 本地指纹目标（NodeID==本机，同私钥多 OS 实例）不回调——与
///   [`fed_broadcast`] 的自回路过滤同语义（发给同指纹节点只会重复入库）；
/// - task 常驻随进程；Handle 关闭后 `peers()` 恒空，空转无害（1s 一拍的
///   no-op）。
pub fn spawn_conn_watcher<F>(
    handle: Handle,
    poll_interval: std::time::Duration,
    mut on_connected: F,
) -> tokio::task::JoinHandle<()>
where
    F: FnMut(&NodeId) + Send + 'static,
{
    tokio::spawn(async move {
        let mut seen: HashSet<NodeId> = HashSet::new();
        let mut tick = tokio::time::interval(poll_interval);
        // 慢拍防堆积（补推回调自身可能耗时——Delay 语义错过后顺延，不追赶）。
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            for p in handle.peers().await {
                if p.connected && !handle.is_local_target(&p.id) && seen.insert(p.id.clone()) {
                    eprintln!(
                        "[p2p][fed] 感知新连接 {}（触发联邦 on-connect 补推）",
                        crate::handlers::api_market::short_node_label(&p.id.to_hex())
                    );
                    on_connected(&p.id);
                }
            }
        }
    })
}

/// 联邦消息分发桥：os-p2p 入站消息（`Handle::on_msg` 观测 task）→
/// 按 `payload.fed` 类型分发给 im / nexhub / live 接收端。
///
/// main.rs 装配（p2p spawn 成功后，**注入先行**）：
///
/// ```text
/// im_federation.set_p2p(handle.clone(), name)                  // ① 发送端注入（同步锁写）
/// nexhub_fed.set_transport(Arc::new(P2pLobbyTransport(handle.clone())), name)
/// live_fed.set_p2p(handle.clone(), name)                        // （直播联邦：大厅宣告+中继）
/// FederationBridge { im, nexhub, live: Some(live_fed) }
///   └─ tokio::spawn(loop rx.recv() → bridge.dispatch(&msg))   // ② 再起入站消费 task
/// ```
///
/// 顺序保证：注入（std Mutex 同步写入，无 await）在消费 task 启动与网关
/// 对外服务之前完成——不存在"消息已发出而 Handle 未注入"的窗口。
///
/// 各消费端均可缺省（`None` → 对应类型载荷只记日志不落地）——组件独立
/// 启停/测试注入友好。
pub struct FederationBridge {
    /// IM 大厅接收端（`fed == "im_lobby"`）。
    pub im: Option<crate::handlers::im::ImFederation>,
    /// NexHub 大厅接收端（`fed == "nexhub_lobby"`）。
    pub nexhub: Option<Arc<os_nexhub::LobbyFedEndpoint>>,
    /// 直播联邦接收端（`fed == "live_lobby" | "live_relay_*"`：联邦大厅宣告
    /// 合并 + 跨节点中继订阅/帧/退订，2026-08-31）。
    pub live: Option<crate::handlers::live::LiveFedEndpoint>,
    /// API 大厅联邦接收端（`fed == "api_market_lobby"`：推理服务市场挂牌条目
    /// 幂等合并，2026-08-31；`api_relay_req|resp`：跨网 API 中继，2026-09-02）。
    pub api_market: Option<crate::handlers::api_market::ApiMarketFedEndpoint>,
}

impl FederationBridge {
    /// 分发一条入站组网消息：识别联邦载荷 → 对应接收端 ingest；非联邦
    /// 载荷（P2b 的 `{text}` 调试消息、查询端自产的 im_lobby_reply 等）静默
    /// 忽略（日志面保留在观测 task）。
    ///
    /// IM 相关载荷在分发前先过一层**身份冲突检测**（仅提示不拦截）：发送者
    /// NodeID == 本机 NodeID（同一私钥的另一节点在发言）→ `ImFederation::
    /// warn_if_identity_conflict` 本地大厅写系统警告；消息本身照常 ingest。
    pub fn dispatch(&self, msg: &os_p2p::P2pMsg) {
        let fed = msg.payload.get("fed").and_then(|v| v.as_str());
        // 身份冲突检测（IM 发言类载荷）：同 NodeID 发言 → 本地警告（不拦截）
        if matches!(
            fed,
            Some(crate::handlers::im::FED_KIND_IM_LOBBY)
                | Some(crate::handlers::im::FED_KIND_IM_FED_LOBBY)
                | Some(crate::handlers::im::FED_KIND_IM_LOBBY_POST)
        ) {
            if let Some(im) = &self.im {
                let node = msg
                    .payload
                    .get("node")
                    .and_then(|v| v.as_str())
                    .unwrap_or("peer");
                im.warn_if_identity_conflict(&msg.from, node);
            }
        }
        match fed {
            Some(crate::handlers::im::FED_KIND_IM_LOBBY) => {
                if let Some(im) = &self.im {
                    im.ingest(&msg.payload);
                }
            }
            // 联邦大厅（fed-lobby 会话）发言广播（2026-08-23）：落本地
            // fed-lobby 会话（sender_id=fed:<node>:<pubkey>）——与 im_lobby
            // 旧载荷同走 ingest（对端兼容）。
            Some(crate::handlers::im::FED_KIND_IM_FED_LOBBY) => {
                if let Some(im) = &self.im {
                    im.ingest(&msg.payload);
                    // 顺带登记发送方 DM 路由（pubkey→NodeID）：之后可从联邦
                    // 大厅直接对其发起跨节点私信（无需 to_node，2026-08-30）。
                    im.register_fed_sender_route(&msg.from, &msg.payload);
                }
            }
            // 远程大厅浏览请求（2026-08-23）：应答开放状态 + ≤20 条脱敏消息
            // （未开放回 denied；查询端探针 ImLobbyProbe 消费应答）。
            Some(crate::handlers::im::FED_KIND_IM_LOBBY_QUERY) => {
                if let Some(im) = &self.im {
                    im.answer_lobby_query(&msg.from, &msg.payload);
                }
            }
            // 远程大厅发言：对方大厅开放（lobby_public）才落地（fed: 前缀）。
            Some(crate::handlers::im::FED_KIND_IM_LOBBY_POST) => {
                if let Some(im) = &self.im {
                    im.ingest_lobby_post(&msg.payload);
                }
            }
            // 跨节点直通消息（DM，2026-08-30）：定向载荷（非广播）——接收端
            // dm_open 检查 + 收件人须是本节点身份，落确定性 dm-* 会话后只
            // 定向推给收件人（from = P2P 验签 NodeID，回程路由登记用）。
            Some(crate::handlers::im::FED_KIND_IM_DM) => {
                if let Some(im) = &self.im {
                    im.ingest_dm(&msg.from, &msg.payload);
                }
            }
            Some(os_nexhub::FED_KIND_NEXHUB_LOBBY) => {
                if let Some(nexhub) = &self.nexhub {
                    nexhub.ingest(&msg.payload);
                }
            }
            // 远程发版广播（2026-08-23）：release 元数据落地本地 hub_releases
            // （不对端执行 git tag——tag 随仓库内容同步）。
            Some(os_nexhub::FED_KIND_NEXHUB_RELEASE) => {
                if let Some(nexhub) = &self.nexhub {
                    nexhub.ingest_release(&msg.payload);
                }
            }
            // 直播联邦（2026-08-31）：live_lobby 宣告合并进联邦大厅表 +
            // live_relay_sub / live_relay_frame / live_relay_unsub 跨节点中继
            // （观众节点订阅源节点房间，帧流经 overlay 定向回传注入本地扇出）。
            Some(crate::handlers::live::FED_KIND_LIVE_LOBBY)
            | Some(crate::handlers::live::FED_KIND_LIVE_RELAY_SUB)
            | Some(crate::handlers::live::FED_KIND_LIVE_RELAY_FRAME)
            | Some(crate::handlers::live::FED_KIND_LIVE_RELAY_UNSUB) => {
                if let Some(live) = &self.live {
                    live.dispatch(&msg.from, &msg.payload);
                }
            }
            // API 大厅联邦（2026-08-31 起）：api_market_lobby 挂牌条目快照幂等
            // 合并进本地 api_market 表（同源刷新保留本地计数、异源受保护跳过）；
            // api_relay_req / api_relay_resp 跨网中继（2026-09-02）——联邦条目
            // 的内网 endpoint 经 overlay 定向源节点代发（白名单裁决 + SSE 逐块
            // 回传，消费者 llm_external 的 via_node 条目走此通道）。统一经
            // `ApiMarketFedEndpoint::dispatch`（带验签 from——来源 NodeID 记录
            // 与伪造应答校验都依赖它）。
            Some(crate::handlers::api_market::FED_KIND_API_MARKET_LOBBY)
            | Some(crate::handlers::api_market::FED_KIND_API_RELAY_REQ)
            | Some(crate::handlers::api_market::FED_KIND_API_RELAY_RESP) => {
                match &self.api_market {
                    Some(api_market) => api_market.dispatch(&msg.from, &msg.payload),
                    // 2026-09-03 真机跟进：消费端未装配时此前静默丢弃（真机
                    // 现象"帧已收到、无任何 api-market-fed 日志"的候选之一）
                    // ——现在落告警日志，装配缺位一眼可见。
                    None => eprintln!(
                        "[fed-bridge] api_market 接收端未装配，丢弃 {} 载荷（from={} hops={}）",
                        fed.unwrap_or("?"),
                        crate::handlers::api_market::short_node_label(&msg.from.to_hex()),
                        msg.hops
                    ),
                }
            }
            _ => {} // 非联邦消息：p2p send 端点（{text}）等，忽略
        }
    }
}

// ----------------------------------------------------------------------------
// 单元测——鉴权矩阵 / 未启用 503 / 各端点字段 / 参数校验
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use os_nexhub::LobbyFedTransport;
    use os_p2p::{P2pConfig, P2pNode, Timing};

    /// 构造一个测试组网节点（随机端口 / 测试节奏 / 关 mDNS，隔离环境）。
    fn spawn_test_node() -> Handle {
        P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .expect("随机端口绑定必成功")
    }

    /// 本机非回环 IPv4（UDP connect 探测默认路由——仅选路不发包；与 os-p2p
    /// api.rs 测试同款）。2026-08-25 回环彻底屏蔽后 record_conn 对回环观测
    /// 不入册——注册表相关测试的 mesh 必须经非回环地址互拨。
    fn non_loopback_local_ipv4() -> std::net::Ipv4Addr {
        let s = std::net::UdpSocket::bind("0.0.0.0:0").expect("UDP bind（选路用）");
        s.connect("8.8.8.8:80").expect("connect（仅选路，不发包）");
        match s.local_addr().expect("local_addr").ip() {
            std::net::IpAddr::V4(v4) if !v4.is_loopback() => v4,
            other => panic!("无非回环本机 IPv4 可用（mesh 测试无从进行）: {other}"),
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

    fn post_req(path: &str, body: serde_json::Value) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Post,
            path: path.into(),
            headers: serde_json::json!({}),
            body,
            auth: None,
        }
    }

    // —— 路由声明 + 鉴权矩阵 ——

    #[tokio::test]
    async fn routes_declare_read_and_admin_matrix() {
        let h = P2pRouteHandler::new_disabled();
        let routes = h.routes().await;
        assert_eq!(routes.len(), 10, "10 条路由");
        assert!(routes.iter().all(|r| r.handler_component == COMPONENT));
        // 读公开：6 条 GET 全部 requires_auth=false 且无角色
        for path in [
            PATH_STATUS,
            PATH_PEERS,
            PATH_BUCKETS,
            PATH_LADDER,
            PATH_IDENTITY_CONFLICTS,
            PATH_NODE_META,
        ] {
            let r = routes
                .iter()
                .find(|r| r.path == path && r.method == HttpMethod::Get)
                .unwrap_or_else(|| panic!("应声明 GET {path}"));
            assert!(!r.requires_auth, "{path} 读公开");
            assert!(r.required_roles.is_empty(), "{path} 无角色要求");
        }
        // 写 admin：SEND/CONNECT requires_auth=true + roles=["admin"]
        for path in [PATH_SEND, PATH_CONNECT] {
            let r = routes
                .iter()
                .find(|r| r.path == path && r.method == HttpMethod::Post)
                .unwrap_or_else(|| panic!("应声明 POST {path}"));
            assert!(r.requires_auth, "{path} 写需认证");
            assert_eq!(
                r.required_roles,
                vec!["admin".to_string()],
                "{path} admin 角色"
            );
        }
        // add-peer 开发期公开（routes() 里 spec_read 声明——用户指示：先不搞
        // 复杂认证；此前误按 admin 断言导致矩阵测试失败）
        let add_peer = routes
            .iter()
            .find(|r| r.path == PATH_ADD_PEER && r.method == HttpMethod::Post)
            .expect("应声明 POST add-peer");
        assert!(!add_peer.requires_auth, "add-peer 开发期公开免认证");
        assert!(add_peer.required_roles.is_empty(), "add-peer 无角色要求");
        // node-meta reactivate 同款开发期公开（手动心跳是运维动作，非越权面）
        let reactivate = routes
            .iter()
            .find(|r| r.path == PATH_NODE_META_REACTIVATE && r.method == HttpMethod::Post)
            .expect("应声明 POST node-meta/:id/reactivate");
        assert!(!reactivate.requires_auth, "reactivate 开发期公开免认证");
        assert!(
            reactivate.required_roles.is_empty(),
            "reactivate 无角色要求"
        );
    }

    // —— 未启用：全部端点 503 + 统一引导文案 ——

    #[tokio::test]
    async fn disabled_returns_503_with_guide_message() {
        let h = P2pRouteHandler::new_disabled();
        assert!(!h.is_enabled());
        for req in [
            get_req(PATH_STATUS),
            get_req(PATH_PEERS),
            get_req(PATH_BUCKETS),
            get_req(PATH_LADDER),
            get_req(PATH_IDENTITY_CONFLICTS),
            post_req(
                PATH_SEND,
                serde_json::json!({"node_id": "0x00", "text": "hi"}),
            ),
            post_req(PATH_CONNECT, serde_json::json!({"node_id": "0x00"})),
            post_req(PATH_ADD_PEER, serde_json::json!({"addr": "10.0.0.1:7070"})),
        ] {
            let resp = h.handle(req).await.expect("disabled 应答 Ok");
            assert_eq!(resp.status, 503, "未启用语义 503");
            assert_eq!(
                resp.body["error"].as_str().unwrap(),
                "P2P 未启用（NEXOS_P2P_ENABLE=1）",
                "统一引导文案"
            );
        }
    }

    // —— status：自身身份/监听/启用态各字段 ——

    #[tokio::test]
    async fn status_returns_self_identity_and_listen() {
        let handle = spawn_test_node();
        let h = P2pRouteHandler::new(handle.clone(), "test-node".into(), true);
        let resp = h.handle(get_req(PATH_STATUS)).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["enabled"], true);
        // node_id 与 Handle 同源（0x + 66 hex）
        assert_eq!(resp.body["self"]["node_id"], handle.self_id().to_hex());
        assert!(resp.body["self"]["node_id"]
            .as_str()
            .unwrap()
            .starts_with("0x"));
        assert_eq!(resp.body["self"]["node_id"].as_str().unwrap().len(), 68);
        // overlay_addr = EVM 同源 20 字节（0x + 40 hex）
        assert_eq!(
            resp.body["self"]["overlay_addr"],
            handle.self_id().overlay().to_hex()
        );
        assert_eq!(
            resp.body["self"]["overlay_addr"].as_str().unwrap().len(),
            42
        );
        // 昵称 / 角色 / 监听地址 / 空态计数
        assert_eq!(resp.body["self"]["name"], "test-node");
        assert_eq!(resp.body["self"]["public"], true);
        assert_eq!(resp.body["listen"], handle.listen_addr().to_string());
        assert_eq!(resp.body["peers_known"], 0);
        assert_eq!(resp.body["peers_connected"], 0);
        handle.shutdown().await;
    }

    // —— peers / buckets / ladder 空态形状 ——

    #[tokio::test]
    async fn peers_buckets_ladder_empty_shape() {
        let handle = spawn_test_node();
        let h = P2pRouteHandler::new(handle.clone(), String::new(), false);
        // peers：空数组
        let resp = h.handle(get_req(PATH_PEERS)).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body.as_array().unwrap().len(), 0);
        // buckets：{buckets: [], known_endpoints: []}
        let resp = h.handle(get_req(PATH_BUCKETS)).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["buckets"].as_array().unwrap().len(), 0);
        assert_eq!(resp.body["known_endpoints"].as_array().unwrap().len(), 0);
        // ladder：四计数全 0
        let resp = h.handle(get_req(PATH_LADDER)).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["direct"], 0);
        assert_eq!(resp.body["punched"], 0);
        assert_eq!(resp.body["relayed"], 0);
        assert_eq!(resp.body["punch_failed"], 0);
        handle.shutdown().await;
    }

    // —— send：自环送达 + 参数校验 ——

    #[tokio::test]
    async fn send_to_self_delivers_locally() {
        let handle = spawn_test_node();
        let h = P2pRouteHandler::new(handle.clone(), String::new(), false);
        let mut rx = handle.on_msg();
        let resp = h
            .handle(post_req(
                PATH_SEND,
                serde_json::json!({"node_id": handle.self_id().to_hex(), "text": "hello p2p"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["ok"], true);
        // 本地回环即时送达（broadcast）
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("自环即时")
            .expect("broadcast 存活");
        assert_eq!(msg.payload["text"], "hello p2p");
        assert_eq!(msg.hops, 0);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn send_rejects_invalid_body() {
        let handle = spawn_test_node();
        let h = P2pRouteHandler::new(handle.clone(), String::new(), false);
        // node_id 缺失
        let resp = h
            .handle(post_req(PATH_SEND, serde_json::json!({"text": "x"})))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        assert!(resp.body["error"].as_str().unwrap().contains("node_id"));
        // node_id 非法（非公钥 hex）
        let resp = h
            .handle(post_req(
                PATH_SEND,
                serde_json::json!({"node_id": "not-a-key", "text": "x"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // text 缺失
        let resp = h
            .handle(post_req(
                PATH_SEND,
                serde_json::json!({"node_id": handle.self_id().to_hex()}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        assert!(resp.body["error"].as_str().unwrap().contains("text"));
        // text 空串
        let resp = h
            .handle(post_req(
                PATH_SEND,
                serde_json::json!({"node_id": handle.self_id().to_hex(), "text": ""}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        handle.shutdown().await;
    }

    // —— connect：非法 node_id 400；对自身连接按无路由失败（502 不 panic）——

    #[tokio::test]
    async fn connect_validates_and_fails_gracefully() {
        let handle = spawn_test_node();
        let h = P2pRouteHandler::new(handle.clone(), String::new(), false);
        // 非法 node_id
        let resp = h
            .handle(post_req(
                PATH_CONNECT,
                serde_json::json!({"node_id": "0x00"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // 自身不可连接（connect_ladder 对 self 返回 NoRoute）→ 502 错误响应（非 Err）
        let resp = h
            .handle(post_req(
                PATH_CONNECT,
                serde_json::json!({"node_id": handle.self_id().to_hex()}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 502);
        assert!(resp.body["error"]
            .as_str()
            .unwrap()
            .contains("连接阶梯失败"));
        handle.shutdown().await;
    }

    // —— add-peer：按地址直拨 + 参数校验 + 已连短路 + 不可达 502 ——

    #[tokio::test]
    async fn add_peer_dials_address_and_reports_node_id() {
        // B 为拨号目标（无引导、随机端口）；A 经 REST add-peer 直拨 B
        let b = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        let a = spawn_test_node();
        let h = P2pRouteHandler::new(a.clone(), String::new(), false);

        // addr 缺失 → 400
        let resp = h
            .handle(post_req(PATH_ADD_PEER, serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // addr 非法 → 400
        let resp = h
            .handle(post_req(
                PATH_ADD_PEER,
                serde_json::json!({"addr": "not an addr"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);

        // 正常拨号 → 200 + 对端 NodeID + peer 摘要（成功即入路由表）
        let resp = h
            .handle(post_req(
                PATH_ADD_PEER,
                serde_json::json!({"addr": b.listen_addr().to_string()}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "拨号应成功: {resp:?}");
        assert_eq!(resp.body["ok"], true);
        assert_eq!(resp.body["node_id"], b.self_id().to_hex());
        assert_eq!(resp.body["addr"], b.listen_addr().to_string());
        assert_eq!(resp.body["peer"]["id"], b.self_id().to_hex());
        assert_eq!(resp.body["peer"]["connected"], true);
        // 路由表反映连接（前端刷新列表即可见）
        let resp = h.handle(get_req(PATH_PEERS)).await.unwrap();
        assert!(resp
            .body
            .as_array()
            .unwrap()
            .iter()
            .any(|p| { p["id"] == b.self_id().to_hex() && p["connected"] == true }));

        // 重复添加同地址 → 短路成功（already-connected，不误报失败）
        let resp = h
            .handle(post_req(
                PATH_ADD_PEER,
                serde_json::json!({"addr": b.listen_addr().to_string()}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "已连短路: {resp:?}");
        assert_eq!(resp.body["node_id"], b.self_id().to_hex());
        assert_eq!(resp.body["note"], "already-connected");

        // 无端口裸 IP → 补默认 7070（拨得通/拨不通依环境，但响应中的地址
        // 必须是补全后的 127.0.0.1:7070——200 的 addr 字段或 502 的错误文案）
        let resp = h
            .handle(post_req(
                PATH_ADD_PEER,
                serde_json::json!({"addr": "127.0.0.1"}),
            ))
            .await
            .unwrap();
        let body_str = resp.body.to_string();
        assert!(
            resp.status == 200 || resp.status == 502,
            "裸 IP 应补端口后正常应答: {resp:?}"
        );
        assert!(
            body_str.contains("127.0.0.1:7070"),
            "地址应补默认端口 7070: {body_str}"
        );

        // 不可达地址 → 502 + 原因（非 panic）
        let resp = h
            .handle(post_req(
                PATH_ADD_PEER,
                serde_json::json!({"addr": "127.0.0.1:1"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 502);
        assert!(resp.body["error"].as_str().unwrap().contains("拨号"));

        a.shutdown().await;
        b.shutdown().await;
    }

    // —— 双节点：peers/buckets/端点簿反映真实组网 ——

    #[tokio::test]
    async fn two_nodes_peers_and_buckets_reflect_mesh() {
        // B 先起（公网角色），A 引导到 B → 连接建立 + 桶收录 + 端点簿记账
        let b = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
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
        let h = P2pRouteHandler::new(a.clone(), "node-a".into(), false);

        // 等 A 的引导拨号 + walk 收敛（测试节奏下数秒内）——os-p2p 的 DTO 是
        // Serialize-only，测试直接在 JSON 值上断言（与前端消费同视角）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let peers_body = loop {
            let resp = h.handle(get_req(PATH_PEERS)).await.unwrap();
            if resp.body.as_array().is_some_and(|a| !a.is_empty())
                || std::time::Instant::now() > deadline
            {
                break resp.body;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        };
        let arr = peers_body.as_array().expect("peers 应为数组");
        assert!(!arr.is_empty(), "A 应学到 B");
        let entry = arr
            .iter()
            .find(|p| p["id"] == b.self_id().to_hex())
            .expect("路由表应含 B");
        assert_eq!(entry["connected"], true, "A↔B 已连接");
        assert_eq!(entry["public"], true, "B 是公网服务节点");
        assert!(entry["underlay"].is_string(), "B 有可拨 underlay");

        // status 的 peers_known/connected 计数同步
        let resp = h.handle(get_req(PATH_STATUS)).await.unwrap();
        assert_eq!(resp.body["peers_known"], 1);
        assert_eq!(resp.body["peers_connected"], 1);

        // buckets：B 落在某个非空桶（po 0..=159，count≥1）
        let resp = h.handle(get_req(PATH_BUCKETS)).await.unwrap();
        let buckets = resp.body["buckets"].as_array().expect("buckets 数组");
        assert!(!buckets.is_empty(), "至少一个非空桶");
        assert!(buckets.iter().all(
            |x| x["count"].as_u64().unwrap_or(0) >= 1 && x["po"].as_u64().unwrap_or(999) <= 159
        ));
        // 端点簿：A 观测到 B 的连接地址（loopback）
        let endpoints = resp.body["known_endpoints"]
            .as_array()
            .expect("known_endpoints 数组");
        assert!(
            endpoints.iter().any(|e| e["id"] == b.self_id().to_hex()),
            "端点簿应有 B 的观测端点"
        );

        // connect 已直连 → 短路 Direct（serde rename_all=snake_case → "direct"）
        let resp = h
            .handle(post_req(
                PATH_CONNECT,
                serde_json::json!({"node_id": b.self_id().to_hex()}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["path"], "direct");

        // ladder：四计数齐备（已直连短路不计数——direct 只记 underlay 直拨，
        // 本场景 A 的连接来自 bootstrap 拨号，计数为 0 是正确语义）
        let resp = h.handle(get_req(PATH_LADDER)).await.unwrap();
        for field in ["direct", "punched", "relayed", "punch_failed"] {
            assert!(
                resp.body[field].is_u64(),
                "ladder 应含 u64 字段 {field}，实际 {}",
                resp.body
            );
        }
        assert_eq!(resp.body["direct"], 0);

        // A → B 发消息，B 收到（经 REST send 走完整路由）
        let mut brx = b.on_msg();
        let resp = h
            .handle(post_req(
                PATH_SEND,
                serde_json::json!({"node_id": b.self_id().to_hex(), "text": "via rest"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let msg = tokio::time::timeout(std::time::Duration::from_secs(3), brx.recv())
            .await
            .expect("直连送达即时")
            .expect("B 的 broadcast 存活");
        assert_eq!(msg.from, *a.self_id());
        assert_eq!(msg.payload["text"], "via rest");
        assert_eq!(msg.hops, 0, "直连 0 跳");

        a.shutdown().await;
        b.shutdown().await;
    }

    // —— node-meta：注册表快照透传（meta 组件观察面）+ reactivate 调用路径 ——

    // 1. GET node-meta：双节点 mesh 后 A 的注册表含 B（direct 来源），
    //    条目六字段齐备（id/addrs/first_seen/last_seen/state/source）
    #[tokio::test]
    async fn node_meta_lists_registry_entries_with_shape() {
        let (a, b) = two_node_mesh().await;
        let h = P2pRouteHandler::new(a.clone(), "node-a".into(), false);
        // 双节点 mesh：A 的注册表应含 B（register_conn 直连观测 → Direct）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let body = loop {
            let resp = h.handle(get_req(PATH_NODE_META)).await.unwrap();
            assert_eq!(resp.status, 200);
            let hit = resp
                .body
                .as_array()
                .is_some_and(|l| l.iter().any(|e| e["id"] == b.self_id().to_hex()));
            if hit || std::time::Instant::now() > deadline {
                break resp.body;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        };
        let arr = body.as_array().expect("node-meta 应为数组");
        let entry = arr
            .iter()
            .find(|e| e["id"] == b.self_id().to_hex())
            .expect("A 的注册表应含 B");
        // 条目六字段：id / addrs / first_seen / last_seen / state / source
        assert!(entry["id"].as_str().unwrap().starts_with("0x"));
        assert!(
            entry["addrs"].as_array().is_some_and(|l| !l.is_empty()),
            "地址历史非空（观测地址入档）"
        );
        // addrs 条目双格式兼容：os-p2p MetaAddr 改为 {addr, verified} 对象
        // （2026-08-23 指纹验证批次）；旧格式为裸地址字符串。
        let addr0 = entry["addrs"][0]
            .as_str()
            .map(String::from)
            .or_else(|| entry["addrs"][0]["addr"].as_str().map(String::from))
            .expect("addrs[0] 应为地址（对象 .addr 或裸字符串）");
        assert!(
            addr0.contains(&format!("{}:", non_loopback_local_ipv4())),
            "最新观测地址在前（非回环本机地址 mesh——回环观测不入册）: {addr0}"
        );
        assert!(entry["first_seen"].as_u64().unwrap_or(0) > 0);
        assert!(entry["last_seen"].as_u64().unwrap_or(0) > 0);
        // state 外部标签式 {"active": {score, consec_fail}}（serde rename_all=snake_case）
        let state = &entry["state"];
        let active = state
            .get("active")
            .or_else(|| state.get("inactive"))
            .expect("state 应为 {active|inactive: …} 结构");
        assert!(
            active.get("score").is_some() || active.get("since").is_some(),
            "状态携带分数或出局时刻: {active}"
        );
        if let Some(active) = state.get("active") {
            assert!(
                active["score"].as_u64().unwrap_or(0) >= 50,
                "直连对端建档 50 起（连接即活性证据）"
            );
        }
        assert_eq!(entry["source"], "direct", "直连观测来源为 direct");
        // 排序契约（score 降序、Inactive 殿后）由 meta 组件保证——透传不重排
        a.shutdown().await;
        b.shutdown().await;
    }

    // 2. POST node-meta/:id/reactivate：:id 非法 400；已连对端立即探测通过
    //    （probed=true）；未知节点 probed=false；未启用 503
    #[tokio::test]
    async fn node_meta_reactivate_probes_and_reports_result() {
        let (a, b) = two_node_mesh().await;
        let h = P2pRouteHandler::new(a.clone(), "node-a".into(), false);
        let reactivate_req = |id: String| ApiRequest {
            method: HttpMethod::Post,
            path: format!("{PATH_NODE_META_PREFIX}{id}{PATH_REACTIVATE_SUFFIX}"),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        };
        // :id 非法（非 0x+66 hex 公钥）→ 400
        let resp = h.handle(reactivate_req("not-a-key".into())).await.unwrap();
        assert_eq!(resp.status, 400);
        assert!(resp.body["error"].as_str().unwrap().contains("0x+66 hex"));
        // 已连接对端 → {ok:true, probed:true}（活连接即活性证据）
        let resp = h
            .handle(reactivate_req(b.self_id().to_hex()))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["ok"], true);
        assert_eq!(resp.body["probed"], true, "活连接立即探测必成功");
        assert_eq!(resp.body["node_id"], b.self_id().to_hex());
        // 未知节点（合法格式但注册表无条目）→ ok:true, probed:false（不 panic）
        let stranger = os_p2p::NodeIdentity::generate().node_id();
        let resp = h.handle(reactivate_req(stranger.to_hex())).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["ok"], true);
        assert_eq!(resp.body["probed"], false, "未知节点无条目可探测");
        a.shutdown().await;
        b.shutdown().await;
        // 未启用 → 503 统一语义（GET node-meta / POST reactivate 同）
        let d = P2pRouteHandler::new_disabled();
        let resp = d.handle(get_req(PATH_NODE_META)).await.unwrap();
        assert_eq!(resp.status, 503);
        assert_eq!(resp.body["error"].as_str().unwrap(), DISABLED_MSG);
        let resp = d.handle(reactivate_req(stranger.to_hex())).await.unwrap();
        assert_eq!(resp.status, 503);
        assert_eq!(resp.body["error"].as_str().unwrap(), DISABLED_MSG);
    }

    // —— 兜底：未声明方法/路径 404 ——

    #[tokio::test]
    async fn unmatched_route_returns_404() {
        let h = P2pRouteHandler::new_disabled();
        let resp = h
            .handle(post_req("/api/v1/p2p/status", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
        assert!(resp.body["error"].as_str().unwrap().contains("未匹配"));
    }

    #[test]
    fn default_trait_is_disabled() {
        fn assert_default<T: Default>() {}
        assert_default::<P2pRouteHandler>();
        assert!(!P2pRouteHandler::default().is_enabled());
    }

    // ---- P3 联邦桥（FederationBridge / fed_broadcast / P2pLobbyTransport）----

    /// 等待两节点建立连接（测试节奏下数秒内；超时 panic 由调用方 assert 兜底）。
    async fn wait_mesh(a: &Handle, b: &Handle) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            let peers = a.peers().await;
            if peers.iter().any(|p| p.id == *b.self_id() && p.connected) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("10s 内 A↔B 未建立连接");
    }

    /// 双节点 mesh fixture：A（公网锚点，监听**非回环本机地址**——2026-08-25
    /// 回环彻底屏蔽后 record_conn 对回环观测不入册，经 LAN IP 拨入 B 才会
    /// 出现在 A 的注册表）+ B（引导到 A），返回 (a, b) Handle。
    async fn two_node_mesh() -> (Handle, Handle) {
        let lan_ip = non_loopback_local_ipv4();
        let a = P2pNode::spawn(P2pConfig {
            listen: SocketAddr::new(std::net::IpAddr::V4(lan_ip), 0),
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
        wait_mesh(&a, &b).await;
        (a, b)
    }

    #[tokio::test]
    async fn fed_broadcast_reaches_connected_peer() {
        let (a, b) = two_node_mesh().await;
        let mut brx = b.on_msg();
        let sent = fed_broadcast(
            &a,
            serde_json::json!({"fed": "im_lobby", "node": "node-a", "message": {"id": "x"}}),
        )
        .await;
        assert_eq!(sent, 1, "应送达 1 个已连接 peer");
        let msg = tokio::time::timeout(std::time::Duration::from_secs(3), brx.recv())
            .await
            .expect("直连即时送达")
            .expect("broadcast 存活");
        assert_eq!(msg.from, *a.self_id());
        assert_eq!(msg.payload["fed"], "im_lobby");
        assert_eq!(msg.payload["node"], "node-a");
        assert_eq!(msg.hops, 0, "直连 0 跳");
        a.shutdown().await;
        b.shutdown().await;
    }

    #[tokio::test]
    async fn fed_broadcast_without_peers_returns_zero() {
        let a = spawn_test_node();
        let sent = fed_broadcast(&a, serde_json::json!({"fed": "im_lobby"})).await;
        assert_eq!(sent, 0, "孤网广播返回 0（不 panic）");
        a.shutdown().await;
    }

    #[tokio::test]
    async fn fed_broadcast_skips_local_fingerprint_target() {
        // 同私钥双实例（对端 NodeID == 本机 NodeID）：唯一"已连"对端是本地
        // 指纹 → 广播 0 目标（消息已在本地落库，发给同指纹节点=自回路重复
        // 入库）。k-bucket 按设计忽略自身 ID 条目（peers 通常不含它），本过滤
        // 是第二道防线——保证即便路由表来源变化也不自回路。
        let identity = os_p2p::NodeIdentity::generate();
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
            identity: Some(identity), // 同一私钥 → 同 NodeID
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        // 等同指纹对端连入（A 侧 identity_conflicts 记账即连接凭证）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline && a.identity_conflicts().await.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(
            !a.identity_conflicts().await.is_empty(),
            "同私钥实例应已连入"
        );
        assert!(a.is_local_target(b.self_id()), "同私钥对端指纹==本机");
        let sent = fed_broadcast(&a, serde_json::json!({"fed": "im_lobby"})).await;
        assert_eq!(sent, 0, "本地指纹目标被跳过 → 广播 0 peer");
        a.shutdown().await;
        b.shutdown().await;
    }

    #[tokio::test]
    async fn p2p_lobby_transport_broadcasts_to_peer() {
        let (a, b) = two_node_mesh().await;
        let mut brx = b.on_msg();
        // 同步 trait 方法（内部 spawn 异步 fan-out）——发布路径不阻塞
        P2pLobbyTransport(a.clone()).broadcast(
            serde_json::json!({"fed": "nexhub_lobby", "node": "node-a", "entry": {"repo_name": "p"}}),
        );
        let msg = tokio::time::timeout(std::time::Duration::from_secs(3), brx.recv())
            .await
            .expect("spawn 的广播应即时送达")
            .expect("broadcast 存活");
        assert_eq!(msg.payload["fed"], "nexhub_lobby");
        assert_eq!(msg.payload["entry"]["repo_name"], "p");
        a.shutdown().await;
        b.shutdown().await;
    }

    fn bridge_msg(payload: serde_json::Value) -> os_p2p::P2pMsg {
        os_p2p::P2pMsg {
            from: os_p2p::NodeIdentity::generate().node_id(),
            payload,
            ttl: 16,
            hops: 0,
        }
    }

    #[tokio::test]
    async fn bridge_dispatches_im_lobby_to_im_endpoint() {
        let im = crate::handlers::im::ImRouteHandler::with_empty();
        let fed = im.federation();
        let bridge = FederationBridge {
            im: Some(fed.clone()),
            nexhub: None,
            live: None,
            api_market: None,
        };
        let msg = crate::handlers::im::Message {
            id: "bridge-1".to_string(),
            conversation_id: "lobby".to_string(),
            sender_id: "0xab".to_string(),
            sender_name: None,
            content: "桥接写入".to_string(),
            msg_type: "text".to_string(),
            file_url: None,
            reply_to: None,
            created_at: "2026-08-22T10:00:00+08:00".to_string(),
            read_by: vec![],
            sender_kind: "human".to_string(),
            mentions: vec![],
            attachment: None,
        };
        bridge.dispatch(&bridge_msg(
            crate::handlers::im::build_im_lobby_fed_payload("node-9", &msg),
        ));
        assert!(
            im.messages_snapshot()
                .iter()
                .any(|m| m.id == "bridge-1" && m.sender_id == "fed:node-9:0xab"),
            "bridge 分发后消息应落地（带来源前缀）"
        );
    }

    #[tokio::test]
    async fn bridge_dispatches_nexhub_lobby_entry() {
        let nexhub = os_nexhub::NexHubLobbyRouteHandler::with_empty();
        let endpoint = nexhub.fed_endpoint();
        let bridge = FederationBridge {
            im: None,
            nexhub: Some(endpoint),
            live: None,
            api_market: None,
        };
        let entry = serde_json::json!({
            "repo_name": "bridge-proj", "description": "经桥写入", "publisher": "0xpk",
            "published_at": "2026-08-22T10:00:00+08:00",
        });
        bridge.dispatch(&bridge_msg(serde_json::json!({
            "fed": "nexhub_lobby", "node": "node-8", "entry": entry,
        })));
        let saved = nexhub
            .entries_snapshot()
            .into_iter()
            .find(|e| e.repo_name == "bridge-proj")
            .expect("条目应落地");
        assert_eq!(saved.source_node, "node-8", "source_node 标记来源");
    }

    #[tokio::test]
    async fn bridge_ignores_non_fed_payloads() {
        let im = crate::handlers::im::ImRouteHandler::with_empty();
        let nexhub = os_nexhub::NexHubLobbyRouteHandler::with_empty();
        let bridge = FederationBridge {
            im: Some(im.federation()),
            nexhub: Some(nexhub.fed_endpoint()),
            live: None,
            api_market: None,
        };
        // P2b 调试消息 / 未知 fed 类型 —— 双端零写入
        bridge.dispatch(&bridge_msg(serde_json::json!({"text": "hello"})));
        bridge.dispatch(&bridge_msg(
            serde_json::json!({"fed": "unknown_kind", "node": "n"}),
        ));
        assert!(
            !im.messages_snapshot()
                .iter()
                .any(|m| m.sender_id.starts_with("fed:")),
            "IM 零写入"
        );
        assert!(nexhub.entries_snapshot().is_empty(), "hub_lobby 零写入");
    }

    // ---- 身份冲突检测（2026-08-23，仅提示不阻断）----
    //
    //   身份 = 密钥是设计特性：多个 OS 用同一私钥进入 → 权限相同、文件不同步。
    //   两层本地警告：握手层（identity-conflicts 端点观测）+ 联邦消息层
    //   （大厅系统警告消息）；均不阻断连接、不拦截消息。

    /// 统计本地大厅里的身份冲突系统警告条数。
    fn conflict_warnings(
        im: &crate::handlers::im::ImRouteHandler,
    ) -> Vec<crate::handlers::im::Message> {
        im.messages_snapshot()
            .into_iter()
            .filter(|m| {
                m.sender_id == "system"
                    && m.sender_kind == "system"
                    && m.content.contains("身份冲突警告")
            })
            .collect()
    }

    /// 构造一条可联邦的人类大厅消息。
    fn fed_msg(id: &str, content: &str) -> crate::handlers::im::Message {
        crate::handlers::im::Message {
            id: id.to_string(),
            conversation_id: "fed-lobby".to_string(),
            sender_id: "0xab".to_string(),
            sender_name: None,
            content: content.to_string(),
            msg_type: "text".to_string(),
            file_url: None,
            reply_to: None,
            created_at: "2026-08-23T10:00:00+08:00".to_string(),
            read_by: vec![],
            sender_kind: "human".to_string(),
            mentions: vec![],
            attachment: None,
        }
    }

    // 1. 同 NodeID 连接：警告记账（= 警告日志路径已触发）+ 连接不被拒绝
    #[tokio::test]
    async fn identity_conflict_endpoint_reports_same_key_connection() {
        // 共用同一私钥（同 NodeID）的双节点：B 引导拨号到 A → A 记账冲突
        let identity = os_p2p::NodeIdentity::generate();
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
            identity: Some(identity), // 同一私钥 → 同 NodeID
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        let h = P2pRouteHandler::new(a.clone(), "node-a".into(), false);
        // 等冲突记账出现（REST 端点视角；空态为 200 + []）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let body = loop {
            let resp = h.handle(get_req(PATH_IDENTITY_CONFLICTS)).await.unwrap();
            assert_eq!(resp.status, 200);
            if resp.body.as_array().is_some_and(|a| !a.is_empty())
                || std::time::Instant::now() > deadline
            {
                break resp.body;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        };
        let arr = body.as_array().expect("conflicts 应为数组");
        assert!(!arr.is_empty(), "同 NodeID 连接应产生冲突记账: {arr:?}");
        for c in arr {
            assert_eq!(c["node_id"], a.self_id().to_hex(), "冲突 NodeID = 本机公钥");
            assert!(
                c["remote_addr"].as_str().is_some_and(|s| s.contains(':')),
                "remote_addr 为观测 ip:port"
            );
            assert!(c["first_seen"].as_u64().unwrap_or(0) > 0, "first_seen 有效");
            assert!(c["last_seen"].as_u64().unwrap_or(0) > 0, "last_seen 有效");
            assert!(
                c["warning_count"].as_u64().unwrap_or(0) >= 1,
                "至少警告一次"
            );
        }
        // 连接不被拒绝：register_conn 全程走完（端点簿收录该 NodeID 观测端点）
        let endpoints = a.known_endpoints().await;
        assert!(
            endpoints.iter().any(|e| e.id == *a.self_id()),
            "同 NodeID 连接照常注册（仅警告不阻断）"
        );
        a.shutdown().await;
        b.shutdown().await;
    }

    // 2. 不同 NodeID（正常组网）→ 零冲突 + 端点空态形状
    #[tokio::test]
    async fn identity_conflict_endpoint_empty_for_distinct_nodeids() {
        let handle = spawn_test_node();
        let h = P2pRouteHandler::new(handle.clone(), String::new(), false);
        // 空态：200 + []
        let resp = h.handle(get_req(PATH_IDENTITY_CONFLICTS)).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body.as_array().unwrap().len(), 0, "空态零冲突");
        // 正常双节点组网（不同 NodeID）→ 仍零冲突
        let (a, b) = two_node_mesh().await;
        let ha = P2pRouteHandler::new(a.clone(), "node-a".into(), false);
        let resp = ha.handle(get_req(PATH_IDENTITY_CONFLICTS)).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(
            resp.body.as_array().unwrap().len(),
            0,
            "不同 NodeID 不产生冲突"
        );
        assert!(b.identity_conflicts().await.is_empty(), "反向同样零冲突");
        handle.shutdown().await;
        a.shutdown().await;
        b.shutdown().await;
    }

    // 3. 联邦消息来自同 NodeID：系统警告写入大厅 + 原消息照常处理
    #[tokio::test]
    async fn fed_same_nodeid_message_warns_and_ingests_normally() {
        let handle = spawn_test_node();
        let im = crate::handlers::im::ImRouteHandler::with_empty();
        im.federation().set_p2p(handle.clone(), "node-a".into());
        let bridge = FederationBridge {
            im: Some(im.federation()),
            nexhub: None,
            live: None,
            api_market: None,
        };
        // 发送者 NodeID = 本机（同公钥另一节点发言）
        let msg = fed_msg("conflict-1", "我是同一个私钥的另一个 OS");
        bridge.dispatch(&os_p2p::P2pMsg {
            from: handle.self_id().clone(),
            payload: crate::handlers::im::build_im_fed_lobby_payload("evil-twin", &msg),
            ttl: 16,
            hops: 0,
        });
        // 原消息照常处理（落联邦大厅 + fed: 来源前缀）
        let got = im
            .messages_snapshot()
            .into_iter()
            .find(|m| m.id == "conflict-1")
            .expect("原联邦消息应照常落地");
        assert_eq!(got.sender_id, "fed:evil-twin:0xab");
        assert_eq!(got.conversation_id, crate::handlers::im::FED_LOBBY_ID);
        // 系统警告写入本地大厅（lobby，非联邦大厅）
        let warnings = conflict_warnings(&im);
        assert_eq!(warnings.len(), 1, "恰好一条系统警告: {warnings:?}");
        let w = &warnings[0];
        assert_eq!(w.sender_id, "system");
        assert_eq!(w.sender_kind, "system");
        assert_eq!(w.msg_type, "system");
        assert_eq!(w.conversation_id, crate::handlers::im::LOBBY_ID);
        assert!(
            w.content.contains("身份冲突警告") && w.content.contains("evil-twin"),
            "警告含来源节点: {}",
            w.content
        );
        assert!(
            w.content.contains("多个 OS 使用同一私钥时权限共享"),
            "警告含权限共享提示"
        );
        handle.shutdown().await;
    }

    // 4. 5 分钟去重：连续两条同源只提示一次；不同 NodeID 不提示
    #[tokio::test]
    async fn fed_same_nodeid_warning_deduped_in_window() {
        let handle = spawn_test_node();
        let im = crate::handlers::im::ImRouteHandler::with_empty();
        im.federation().set_p2p(handle.clone(), "node-a".into());
        let bridge = FederationBridge {
            im: Some(im.federation()),
            nexhub: None,
            live: None,
            api_market: None,
        };
        // 连续两条同 NodeID 联邦消息（间隔毫秒级 << 5 分钟窗口）
        for id in ["dup-1", "dup-2"] {
            let msg = fed_msg(id, "连续发言");
            bridge.dispatch(&os_p2p::P2pMsg {
                from: handle.self_id().clone(),
                payload: crate::handlers::im::build_im_fed_lobby_payload("evil-twin", &msg),
                ttl: 16,
                hops: 0,
            });
        }
        assert_eq!(
            conflict_warnings(&im).len(),
            1,
            "去重窗口内只提示一次（两条消息都照常落地）"
        );
        // 两条原消息均照常处理（去重只作用于提示，不拦截消息）
        for id in ["dup-1", "dup-2"] {
            assert!(
                im.messages_snapshot().iter().any(|m| m.id == id),
                "原消息 {id} 照常落地"
            );
        }
        // 不同 NodeID 的联邦消息 → 不产生新警告
        let msg = fed_msg("normal-1", "正常联邦发言");
        bridge.dispatch(&bridge_msg(
            crate::handlers::im::build_im_fed_lobby_payload("node-b", &msg),
        ));
        assert_eq!(
            conflict_warnings(&im).len(),
            1,
            "不同 NodeID 不触发身份冲突警告"
        );
        assert!(
            im.messages_snapshot().iter().any(|m| m.id == "normal-1"),
            "正常联邦消息照常落地"
        );
        handle.shutdown().await;
    }

    // —— 端到端（P3 双节点拓扑）：A 发联邦大厅消息 B 收到 + B 发大厅条目 A 收到 ——
    //
    //   [节点 A]                                  [节点 B]
    //   ImRouteHandler──POST /fed-lobby/messages  ImRouteHandler
    //     └ federation().federate ──Handle::send──▶ on_msg ─▶ FederationBridge
    //   NexHubLobbyRouteHandler                     └▶ im.ingest → im_messages + WS
    //     └ fed_endpoint ◀──FederationBridge── on_msg ◀──broadcast_entry── fed_endpoint
    //        └▶ ingest → hub_lobby(source_node)
    #[tokio::test]
    async fn fed_end_to_end_two_nodes_im_and_nexhub() {
        let (a, b) = two_node_mesh().await;
        // —— 节点 A：IM handler（REST 发消息）+ nexhub（收条目）——
        let im_a = crate::handlers::im::ImRouteHandler::with_empty();
        im_a.federation().set_p2p(a.clone(), "node-a".into());
        let nex_a = os_nexhub::NexHubLobbyRouteHandler::with_empty();
        let nex_a_fed = nex_a.fed_endpoint();
        // —— 节点 B：IM handler（收消息）+ nexhub（发条目）——
        let im_b = crate::handlers::im::ImRouteHandler::with_empty();
        im_b.federation().set_p2p(b.clone(), "node-b".into());
        let nex_b = os_nexhub::NexHubLobbyRouteHandler::with_empty();
        nex_b
            .fed_endpoint()
            .set_transport(Arc::new(P2pLobbyTransport(b.clone())), "node-b".into());
        // —— 两侧联邦桥（main.rs 同款：入站消息观测 task 分发）——
        let spawn_bridge = |h: &Handle,
                            im: crate::handlers::im::ImFederation,
                            nx: Arc<os_nexhub::LobbyFedEndpoint>| {
            let bridge = FederationBridge {
                im: Some(im),
                nexhub: Some(nx),
                live: None,
                api_market: None,
            };
            let mut rx = h.on_msg();
            tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(m) => bridge.dispatch(&m),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            });
        };
        spawn_bridge(&a, im_a.federation(), nex_a_fed.clone());
        spawn_bridge(&b, im_b.federation(), nex_b.fed_endpoint());

        // —— 1) A 经 REST 发联邦大厅消息（fed-lobby 会话）→ B 收到
        //      （fed: 前缀 + 来源 node-a；我的大厅 lobby 不再联邦广播）——
        let (pubkey, token) = im_login(&im_a).await;
        let resp = im_a
            .handle(ApiRequest {
                method: HttpMethod::Get,
                path: "/api/v1/im/fed-lobby".into(),
                headers: serde_json::json!({"authorization": format!("Bearer {token}")}),
                body: serde_json::Value::Null,
                auth: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "加入联邦大厅成功: {resp:?}");
        let resp = im_a
            .handle(ApiRequest {
                method: HttpMethod::Post,
                path: "/api/v1/im/fed-lobby/messages".into(),
                headers: serde_json::json!({"authorization": format!("Bearer {token}")}),
                body: serde_json::json!({ "content": "跨节点你好（来自 A）" }),
                auth: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "A 本地写入成功: {resp:?}");
        let msg_id = resp.body["id"].as_str().unwrap().to_string();
        // B 侧轮询落地（真实组网传输 + bridge 分发 + ingest）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let got = loop {
            let hit = im_b
                .messages_snapshot()
                .into_iter()
                .find(|m| m.id == msg_id);
            if hit.is_some() || std::time::Instant::now() > deadline {
                break hit;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        };
        let got = got.expect("B 应在 10s 内收到 A 的联邦大厅消息");
        assert_eq!(got.sender_id, format!("fed:node-a:{pubkey}"), "来源前缀");
        assert_eq!(got.content, "跨节点你好（来自 A）");
        assert_eq!(
            got.conversation_id,
            crate::handlers::im::FED_LOBBY_ID,
            "联邦消息落 B 的联邦大厅会话（与我的大厅隔离）"
        );

        // —— 2) B 广播大厅条目（publish 路径同款 broadcast_entry）→ A 收到 ——
        let entry = os_nexhub::LobbyEntry {
            repo_name: "b-shared-proj".to_string(),
            description: "B 节点分享的项目".to_string(),
            tags: vec!["fed".to_string()],
            publisher: "0xnode-b-pubkey".to_string(),
            source_url: "/tank/git-repos/b-shared-proj.git".to_string(),
            homepage_node: "local".to_string(),
            source_node: "local".to_string(),
            // 2026-08-25 联邦 HTTP 克隆地址（跨节点一键克隆用）：fixture 留空。
            clone_url_http: String::new(),
            commit_count: 5,
            size_bytes: 2048,
            default_branch: "main".to_string(),
            last_commit: Some("beef42 - cross-node".to_string()),
            last_commit_date: Some("2026-08-22T09:00:00+08:00".to_string()),
            readme_excerpt: "# shared".to_string(),
            download_count: 0,
            published_at: "2026-08-22T10:00:00+08:00".to_string(),
            price_sats: 0,
            currency: "free".to_string(),
            federated: true,
            // 2026-08-25 快照增量（§15）：字段带默认值，fixture 无需填。
            latest_commit: None,
            pushed_at: String::new(),
        };
        nex_b.fed_endpoint().broadcast_entry(&entry);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let got_entry = loop {
            let hit = nex_a
                .entries_snapshot()
                .into_iter()
                .find(|e| e.repo_name == "b-shared-proj");
            if hit.is_some() || std::time::Instant::now() > deadline {
                break hit;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        };
        let got_entry = got_entry.expect("A 应在 10s 内收到 B 的大厅条目");
        assert_eq!(got_entry.source_node, "node-b", "A 侧标记来源节点");
        assert_eq!(got_entry.description, "B 节点分享的项目");

        a.shutdown().await;
        b.shutdown().await;
    }

    /// IM 登录辅助（challenge → sign → verify，p2p 测试内独立实现避免跨测试
    /// 模块依赖；与 im.rs 测试同栈：k256 + SHA-256 摘要 ECDSA）。
    async fn im_login(h: &crate::handlers::im::ImRouteHandler) -> (String, String) {
        use k256::elliptic_curve::rand_core::OsRng;
        let sk = k256::ecdsa::SigningKey::random(&mut OsRng);
        let pubkey = format!(
            "0x{}",
            hex::encode(sk.verifying_key().to_encoded_point(true).as_bytes())
        );
        let post = |path: &str, body: serde_json::Value| ApiRequest {
            method: HttpMethod::Post,
            path: path.into(),
            headers: serde_json::json!({}),
            body,
            auth: None,
        };
        let resp = h
            .handle(post(
                "/api/v1/im/auth/challenge",
                serde_json::json!({"pubkey": pubkey}),
            ))
            .await
            .unwrap();
        let nonce = resp.body["nonce"].as_str().unwrap().to_string();
        use sha2::Digest;
        let digest = sha2::Sha256::new_with_prefix(nonce.as_bytes());
        let (sig, recid) = sk.sign_digest_recoverable(digest).unwrap();
        let mut out = [0u8; 65];
        out[..64].copy_from_slice(&sig.to_bytes());
        out[64] = u8::from(recid);
        let resp = h
            .handle(post(
                "/api/v1/im/auth/verify",
                serde_json::json!({
                    "pubkey": pubkey, "nonce": nonce,
                    "signature": format!("0x{}", hex::encode(out)),
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        (pubkey, resp.body["token"].as_str().unwrap().to_string())
    }

    // —— 连接观测 task（市场联邦 on-connect 补推的事件面，2026-09-03）——

    /// 1. 新连接恰回调一次：首拍种子（已连 B）→ 稳态无重复 → 新节点 C 拨入
    ///    再回调 C。
    #[tokio::test]
    async fn conn_watcher_reports_new_connections_once() {
        let (a, b) = two_node_mesh().await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        spawn_conn_watcher(
            a.clone(),
            std::time::Duration::from_millis(50),
            move |id| {
                let _ = tx.send(id.clone());
            },
        );
        // 首拍种子：B 已连 → 恰一次回调（进程启动对既有连接也触发补推的
        // 语义来源——重启覆盖缺口同理修复）。
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("首拍应回调已连 peer")
            .expect("channel 存活");
        assert_eq!(got, *b.self_id());
        // 稳态：多拍后已见 peer 不重复回调。
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            rx.try_recv().is_err(),
            "已见 peer 的后续拍不应重复回调"
        );
        // 新节点 C 引导拨入 A → 回调 C（连接建立事件）。
        let c = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            bootstrap: vec![a.listen_addr()],
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("新连接应回调")
            .expect("channel 存活");
        assert_eq!(got, *c.self_id());
        a.shutdown().await;
        b.shutdown().await;
        c.shutdown().await;
    }

    /// api-market 链上登录（p2p 测试内自持：challenge 内部 API + k256 签名，
    /// 与 api_market.rs 测试的 login 同栈——api-market 无自己的 REST auth 面）。
    fn market_login(
        h: &crate::handlers::api_market::ApiMarketRouteHandler,
    ) -> (String, String) {
        use k256::elliptic_curve::rand_core::OsRng;
        let sk = k256::ecdsa::SigningKey::random(&mut OsRng);
        let pubkey = format!(
            "0x{}",
            hex::encode(sk.verifying_key().to_encoded_point(true).as_bytes())
        );
        let auth = h.auth();
        let nonce = auth.create_nonce(&pubkey);
        assert!(auth.take_nonce(&pubkey, &nonce), "nonce 应匹配未过期");
        let (token, _) = auth.issue_token(&pubkey);
        (pubkey, token)
    }

    /// 2. 端到端（真机组网）：A 发布+federate 时 B 未入网（广播空投，真机
    ///    缺陷：fed_broadcast 只发"当时已连"的 peer）→ B 引导拨入 → 观测
    ///    task 感知新连接 → backfill_to 定向补推 → B 的桥 ingest 落库。
    #[tokio::test]
    async fn market_fed_backfill_end_to_end_on_real_mesh() {
        // A：公网锚点（LAN 监听——register_conn 对回环观测不入册，走非回环
        // 与 two_node_mesh 同款）；此刻 B 尚未入网。
        let lan_ip = non_loopback_local_ipv4();
        let a = P2pNode::spawn(P2pConfig {
            listen: SocketAddr::new(std::net::IpAddr::V4(lan_ip), 0),
            public: true,
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        // A 侧市场：发布 + federate（0 已连 peer——广播空投）。
        let market_a = crate::handlers::api_market::ApiMarketRouteHandler::with_empty();
        let fed_a = market_a.federation();
        fed_a.set_p2p(a.clone(), "node-a".into());
        let (pubkey, token) = market_login(&market_a);
        let publish = ApiRequest {
            method: HttpMethod::Post,
            path: "/api/v1/api-market/publish".into(),
            headers: serde_json::json!({"authorization": format!("Bearer {token}")}),
            body: serde_json::json!({
                "api_name": "backfill-e2e",
                "description": "发布窗口 B 不在线",
                "endpoint_url": "http://10.0.0.106:8558/v1",
                "pricing": { "mode": "free" },
                "server_config": { "model_name": "Qwen3.5-9B" },
            }),
            auth: None,
        };
        let resp = market_a.handle(publish).await.unwrap();
        assert_eq!(resp.status, 201, "挂牌应 201: {resp:?}");
        let id = resp.body["id"].as_str().unwrap().to_string();
        let resp = market_a
            .handle(ApiRequest {
                method: HttpMethod::Post,
                path: format!("/api/v1/api-market/{id}/federate"),
                headers: serde_json::json!({"authorization": format!("Bearer {token}")}),
                body: serde_json::Value::Null,
                auth: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "federate 应 200: {resp:?}");
        // 观测 task（main.rs 同款接线）：新连接 → spawn backfill_to。
        let fed_cb = fed_a.clone();
        spawn_conn_watcher(
            a.clone(),
            std::time::Duration::from_millis(50),
            move |peer| {
                let fed = fed_cb.clone();
                let peer = peer.clone();
                tokio::spawn(async move {
                    fed.backfill_to(&peer).await;
                });
            },
        );
        // B：此刻才入网（引导拨号 A）——连接建立即触发补推；B 侧桥消费
        // on_msg 分发给市场联邦端点（先订阅后连接，不丢帧；spawn 后的
        // 调度由下方 deadline 轮询自然覆盖）。
        let b = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            bootstrap: vec![a.listen_addr()],
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        let market_b = crate::handlers::api_market::ApiMarketRouteHandler::with_empty();
        let bridge = FederationBridge {
            im: None,
            nexhub: None,
            live: None,
            api_market: Some(market_b.federation()),
        };
        let mut brx = b.on_msg();
        tokio::spawn(async move {
            loop {
                match brx.recv().await {
                    Ok(m) => bridge.dispatch(&m),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });
        // B 侧轮询落地（真实组网 + 观测 task + 补推 + ingest 全链路，10s 预算）。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let got = loop {
            let hit = market_b
                .listings_snapshot()
                .into_iter()
                .find(|e| e.api_name == "backfill-e2e");
            if hit.is_some() || std::time::Instant::now() > deadline {
                break hit;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        };
        let got = got.expect("B 应在连接建立后经补推收到条目（覆盖缺口修复）");
        assert_eq!(got.source_node, "node-a", "来源标记发布节点");
        assert_eq!(
            got.source_node_id,
            a.self_id().to_hex(),
            "验签来源 NodeID（dispatch → ingest_from）"
        );
        assert_eq!(got.download_count, 0, "本地计数清零起步");
        assert!(got.federated, "推送标志随快照");
        assert_eq!(got.publisher_pubkey, pubkey);
        a.shutdown().await;
        b.shutdown().await;
    }

    /// 3. 端到端·中继拓扑（真机跟进 2026-09-03 第二轮：Spark 场景复现）：
    ///    A（发布者）与 B（Spark 类接收端）互不直连、都只连公网锚点 P——
    ///    B 在 A 的 node-meta 注册表 Active（经 P 的元数据八卦）但 connected
    ///    恒 false。A 的重播轮定向补播相位（生产目标集闭包）经 send_to 按需
    ///    路由（A → P 中继转发 → B，hops=1）送达 → B 的联邦桥消费落库——
    ///    实证"中继帧与直连帧同一条 on_msg 消费路径"（deliver_local 共享）。
    #[tokio::test]
    async fn market_fed_directed_replay_via_relay_topology() {
        // P：公网锚点（LAN 监听——元数据注册表对回环观测不入册，B/A 经 LAN
        // 拨入才会入 P 的注册表，进而经八卦回流到 A）。
        let lan_ip = non_loopback_local_ipv4();
        let p_node = P2pNode::spawn(P2pConfig {
            listen: SocketAddr::new(std::net::IpAddr::V4(lan_ip), 0),
            public: true,
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        // A：发布者（只引导到 P；与 B 永不直连——发送路径不拨号，只能经 P）。
        let a = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            bootstrap: vec![p_node.listen_addr()],
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        // B：Spark 类接收端（只引导到 P；非 public——对 P 注册中继可达性）。
        let b = P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            bootstrap: vec![p_node.listen_addr()],
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .unwrap();
        wait_mesh(&a, &p_node).await;
        wait_mesh(&b, &p_node).await;
        // A↔B 无直连（前置断言——本测试的全部意义在此）。
        {
            let peers = a.peers().await;
            assert!(
                !peers
                    .iter()
                    .any(|x| x.id == *b.self_id() && x.connected),
                "A 与 B 不应直连（中继拓扑前提）"
            );
        }
        // B 侧：市场 handler + 联邦桥消费 task（记录 api_market_lobby 帧的
        // hops——断言经中继路径送达）。
        let market_b = crate::handlers::api_market::ApiMarketRouteHandler::with_empty();
        let bridge = FederationBridge {
            im: None,
            nexhub: None,
            live: None,
            api_market: Some(market_b.federation()),
        };
        let hops_seen: std::sync::Arc<std::sync::Mutex<Vec<u8>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let hops_ref = hops_seen.clone();
        let mut brx = b.on_msg();
        tokio::spawn(async move {
            loop {
                match brx.recv().await {
                    Ok(m) => {
                        if m.payload.get("fed").and_then(|v| v.as_str())
                            == Some(crate::handlers::api_market::FED_KIND_API_MARKET_LOBBY)
                        {
                            hops_ref.lock().unwrap().push(m.hops);
                        }
                        bridge.dispatch(&m);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });
        // A 侧：发布 + federate（广播只到 P——P 无消费端；B 收不到）。
        let market_a = crate::handlers::api_market::ApiMarketRouteHandler::with_empty();
        let fed_a = market_a.federation();
        fed_a.set_p2p(a.clone(), "node-a".into());
        let (_, token) = market_login(&market_a);
        let resp = market_a
            .handle(ApiRequest {
                method: HttpMethod::Post,
                path: "/api/v1/api-market/publish".into(),
                headers: serde_json::json!({"authorization": format!("Bearer {token}")}),
                body: serde_json::json!({
                    "api_name": "relay-topo-api",
                    "description": "中继拓扑定向补播",
                    "endpoint_url": "http://10.0.0.106:8558/v1",
                    "pricing": { "mode": "free" },
                    "server_config": { "model_name": "Qwen3.5-9B" },
                }),
                auth: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "挂牌应 201: {resp:?}");
        let id = resp.body["id"].as_str().unwrap().to_string();
        let resp = market_a
            .handle(ApiRequest {
                method: HttpMethod::Post,
                path: format!("/api/v1/api-market/{id}/federate"),
                headers: serde_json::json!({"authorization": format!("Bearer {token}")}),
                body: serde_json::Value::Null,
                auth: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert!(
            market_b.listings_snapshot().is_empty(),
            "B 无直连——federate 广播到不了它（缺口复现）"
        );
        // 等 A 的 node-meta 经 P 的元数据八卦学到 B（Active）——生产目标集
        // 闭包的数据源；测试节奏下数秒内收敛。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let learned = a
                .node_meta()
                .await
                .iter()
                .any(|e| e.id == *b.self_id() && matches!(e.state, os_p2p::MetaState::Active { .. }));
            if learned || std::time::Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        // 重播轮：定向补播相位对 B send_to（A 无 B 路由 → 按需 lookup 经 P
        // 学到 route，P 中继转发）→ B 消费落库。
        assert_eq!(fed_a.replay_round().await, 1, "重播 1 条");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let got = loop {
            let hit = market_b
                .listings_snapshot()
                .into_iter()
                .find(|e| e.api_name == "relay-topo-api");
            if hit.is_some() || std::time::Instant::now() > deadline {
                break hit;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        };
        let got = got.expect("B 应经中继路径收到定向补播的条目");
        assert_eq!(got.source_node, "node-a");
        assert_eq!(got.source_node_id, a.self_id().to_hex(), "验签来源 NodeID");
        // 中继路径实证：B 收到的 api_market_lobby 帧 hops ≥ 1（A→P→B 一跳
        // 中继；直连才是 0）——与 IM 的 hops≥1 帧同一条消费路径。
        let hops = hops_seen.lock().unwrap().clone();
        assert!(
            hops.iter().any(|h| *h >= 1),
            "应观察到经中继转发的帧（hops≥1），实际: {hops:?}"
        );
        a.shutdown().await;
        b.shutdown().await;
        p_node.shutdown().await;
    }
}
