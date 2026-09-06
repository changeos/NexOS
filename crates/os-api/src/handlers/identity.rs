//! `IdentityRouteHandler` —— 身份账本（os-identity 组件）REST 观察面。
//!
//! # 背景（2026-08-25 组件抽离）
//!
//! 指纹账本与对比从 os-p2p 抽成独立 crate `os-identity`（用户定调：「指纹
//! 对比单独做一个组件，不要集成在 p2p 里面」）。os-api 装配层是账本的
//! **唯一持久化宿主**：main.rs 在 `NEXOS_P2P_ENABLE=1` 时建好带持久化的共享
//! 实例（`NEXOS_IDENTITY_FILE`，缺省 `/tank/os-data/identity-ledger.json`）
//! 注入 os-p2p（`P2pConfig::identity_ledger`，传输层事实事件的唯一落点），
//! 同时自留一份给本 handler 暴露 REST——写读同一实例，账本即唯一权威源。
//!
//! # 端点契约（开发期公开读——如实口径：透出的是**全网拓扑情报**（身份-
//! 地址归属、活跃时间线），对内网侦察有价值，生产前必须收紧鉴权）
//!
//! | 方法 | 路径 | 鉴权 | 语义 |
//! |---|---|---|---|
//! | GET | `/api/v1/identity/records` | 公开 | 全量身份记录（verified/unverified 地址集 + first/last_seen + 冲突 + 失配事件，按 last_seen 降序） |
//! | GET | `/api/v1/identity/addr/:addr` | 公开 | 地址归属查询：owner node_id + verified 状态 + 归属记录（含冲突/失配）；无主 → owner=null |
//! | GET | `/api/v1/identity/conflicts` | 公开 | 同 NodeID 多地址观测（与 `/api/v1/p2p/identity-conflicts` 同源同形） |
//!
//! 未启用（P2P 未开，账本无数据源）→ 503 + 引导文案（与 p2p handler 同款语义）。
//!
//! # 与既有观察面的关系
//!
//! - `GET /api/v1/p2p/identity-conflicts`（p2p handler）保留：数据同源（同一
//!   账本实例），本 handler 是组件化后的规范路径；
//! - `node_view`（节点发现页 combined）**不改**：meta 注册表的 verified 位继续
//!   由 os-p2p 维护（同证据双记账，输出形状不变——消费方无感）。
//!
//! `:addr` 解析：`ip:port` 全格式（IPv4 `1.2.3.4:7070` / IPv6 `[::1]:7070`）、
//! 裸 IPv4（自动补 [`os_p2p::P2P_PORT_DEFAULT`] 7070——与 p2p add-peer 同款
//! 约定）、裸 IPv6 多冒号（`::1` → `[::1]:7070`）与 `[v6]` 括号裸地址（补端口）
//! ——IPv6 不括号直接拼端口是非法串，分支处理见 [`parse_addr_param`]。账本按
//! SocketAddr 键控，裸 IP 不补端口无从查起。不支持主机名（账本键是 socket
//! 地址，不做 DNS）。

use async_trait::async_trait;
use std::net::SocketAddr;
use std::sync::Mutex;

use os_identity::{AddrOwnership, IdentityLedger, SharedLedger};
use serde::Serialize;

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// Handler 主体
// ----------------------------------------------------------------------------

/// 身份账本路由处理器——os-identity 共享账本的 REST 观察面。
///
/// - `Some(ledger)`：与 os-p2p 内嵌节点共享的账本实例（main.rs 装配注入）；
/// - `None`：未启用（P2P 未开）——全部端点 503 + 引导文案。
pub struct IdentityRouteHandler {
    ledger: Option<SharedLedger>,
}

impl IdentityRouteHandler {
    /// 未启用构造（默认部署：`NEXOS_P2P_ENABLE` 未设/为 0；账本无数据源）。
    #[must_use]
    pub fn new_disabled() -> Self {
        Self { ledger: None }
    }

    /// 已启用构造（main.rs 装配：与注入 os-p2p 的同一共享实例）。
    #[must_use]
    pub fn new(ledger: SharedLedger) -> Self {
        Self {
            ledger: Some(ledger),
        }
    }

    /// 是否已启用（诊断/测试用）。
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.ledger.is_some()
    }

    /// 未启用统一 503 语义（与 p2p handler 同款——前端凭 error 文案展示指引）。
    fn disabled_response() -> ApiResponse {
        ApiResponse {
            status: 503,
            body: serde_json::json!({"error": DISABLED_MSG}),
            headers: serde_json::json!({}),
        }
    }

    /// GET records：全量身份记录（账本快照直接可序列化——`IdentityRecord`
    /// 即 DTO；按 last_seen 降序由账本排序保证）。
    fn records_response(ledger: &Mutex<IdentityLedger>) -> ApiResponse {
        let snapshot = ledger.lock().expect("identity ledger poisoned").snapshot();
        ok_json(to_value(&snapshot).unwrap_or_default())
    }

    /// GET addr/:addr：地址归属查询（owner + verified 状态 + 归属记录）。
    ///
    /// 单锁内取齐三份数据（A4 修复）：原实现 `snapshot()` 全表克隆再线性找
    /// 记录 + `owns_addr` 二次加锁——账本键控（node_id），直接
    /// [`IdentityLedger::get_record`] 按键取。
    fn addr_response(ledger: &Mutex<IdentityLedger>, addr: SocketAddr) -> ApiResponse {
        let resp = {
            let ledger = ledger.lock().expect("identity ledger poisoned");
            match ledger.owner_of(addr) {
                Some((owner, verified)) => {
                    // owns_addr 结论为输出准绳（与 owner_of 矛盾时以它为准）
                    let ownership = match ledger.owns_addr(addr, &owner) {
                        AddrOwnership::Verified => "verified",
                        AddrOwnership::Unverified => "unverified",
                        AddrOwnership::Foreign { .. } => "foreign",
                        AddrOwnership::Unknown => "unknown",
                    };
                    AddrOwnerResp {
                        addr: addr.to_string(),
                        owner: Some(owner.clone()),
                        verified,
                        ownership: ownership.to_string(),
                        record: ledger.get_record(&owner),
                    }
                }
                None => AddrOwnerResp {
                    addr: addr.to_string(),
                    owner: None,
                    verified: false,
                    ownership: "unknown".to_string(),
                    record: None,
                },
            }
        };
        ok_json(to_value(&resp).unwrap_or_default())
    }
}

impl Default for IdentityRouteHandler {
    fn default() -> Self {
        Self::new_disabled()
    }
}

#[async_trait]
impl RouteHandler for IdentityRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec_read(HttpMethod::Get, PATH_RECORDS),
            spec_read(HttpMethod::Get, PATH_ADDR),
            spec_read(HttpMethod::Get, PATH_CONFLICTS),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let path = req.path.split('?').next().unwrap_or("");
        // —— 动态路由（:addr 参数）：GET /api/v1/identity/addr/:addr ——
        // SocketAddr（ip:port）不含 '/'，前缀剥离无歧义（p2p.rs :id 同款模式）。
        if req.method == HttpMethod::Get {
            if let Some(raw) = path.strip_prefix(PATH_ADDR_PREFIX) {
                return match &self.ledger {
                    Some(ledger) => match parse_addr_param(raw) {
                        Some(addr) => Ok(Self::addr_response(ledger, addr)),
                        None => Ok(error_response(
                            400,
                            "路径 :addr 非法（支持 ip:port / [v6]:port，裸 IPv4 或 IPv6 自动补 7070；不支持主机名）",
                        )),
                    },
                    None => Ok(Self::disabled_response()),
                };
            }
        }
        match (req.method, path) {
            (HttpMethod::Get, PATH_RECORDS) => Ok(match &self.ledger {
                Some(ledger) => Self::records_response(ledger),
                None => Self::disabled_response(),
            }),
            (HttpMethod::Get, PATH_CONFLICTS) => Ok(match &self.ledger {
                Some(ledger) => {
                    let conflicts = ledger.lock().expect("identity ledger poisoned").conflicts();
                    ok_json(to_value(&conflicts).unwrap_or_default())
                }
                None => Self::disabled_response(),
            }),
            // —— 未覆盖的路由 —— 兜底 404（Ok，非 Err，与其它 handler 同款）
            _ => Ok(error_response(404, "identity: 未匹配的路由")),
        }
    }
}

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 地址归属查询响应。
#[derive(Serialize)]
struct AddrOwnerResp {
    /// 查询地址（规整后的 ip:port）。
    addr: String,
    /// 归属身份（`0x`+66 hex；无主为 None）。
    owner: Option<String>,
    /// 是否 verified（本机/报告方实证过）。
    verified: bool,
    /// owns_addr 判定标签（"verified" / "unverified" / "foreign" / "unknown"）。
    ownership: String,
    /// 归属记录全量（无主为 None——含 verified/unverified 地址集、冲突、失配）。
    record: Option<os_identity::IdentityRecord>,
}

// ----------------------------------------------------------------------------
// 内部辅助（与其它 handler 同款）
// ----------------------------------------------------------------------------

/// `GET /api/v1/identity/records`——全量身份记录。
const PATH_RECORDS: &str = "/api/v1/identity/records";
/// `GET /api/v1/identity/addr/:addr`——地址归属查询。
const PATH_ADDR: &str = "/api/v1/identity/addr/:addr";
/// `GET /api/v1/identity/conflicts`——同 NodeID 多地址观测。
const PATH_CONFLICTS: &str = "/api/v1/identity/conflicts";
/// addr 动态路由的 `:addr` 剥离前缀（spec 声明用上面的参数式路径）。
const PATH_ADDR_PREFIX: &str = "/api/v1/identity/addr/";

/// 本 handler 注册时的组件名（`RouteSpec::handler_component`）。
const COMPONENT: &str = "identity";

/// 未启用统一文案（503 body 的 error 字段——前端凭此展示指引）。
const DISABLED_MSG: &str = "身份账本未启用（NEXOS_P2P_ENABLE=1 开启组网后自动装配）";

/// 身份账本持久化文件 env（main.rs 装配读取；os-identity 组件宿主策略）。
pub const ENV_IDENTITY_FILE: &str = "NEXOS_IDENTITY_FILE";

/// 身份账本持久化文件缺省路径（os-api 部署布局——与系统数据目录同处）。
pub const DEFAULT_IDENTITY_FILE: &str = "/tank/os-data/identity-ledger.json";

/// 构造一条只读路由规格（开发期公开读——透出拓扑情报，生产前须收紧鉴权）。
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

/// 解析 `:addr` 路径参数 → SocketAddr。四种形态（A3 修复：裸 IPv6 直接拼
/// `:7070` 会得到 `::1:7070` 这类非法串——分支处理）：
///
/// - `ip:port` 全格式（IPv4 的 `1.2.3.4:7070` 或 IPv6 的 `[::1]:7070`）原样；
/// - 裸 IPv4（无 `:` 无 `]`）→ 补默认端口 `1.2.3.4:7070`；
/// - 裸 IPv6 多 `:`（`::1` / `2001:db8::1`）→ 包括号补端口 `[..]:7070`；
/// - `[v6]` 括号裸地址（以 `]` 结尾无端口）→ 补端口 `[v6]:7070`。
///
/// 裸 IP 补 [`os_p2p::P2P_PORT_DEFAULT`]（与 p2p add-peer 同款约定）；主机名
/// 不支持（账本键是 socket 地址，无 DNS）。
fn parse_addr_param(raw: &str) -> Option<SocketAddr> {
    let s = raw.trim();
    if s.is_empty() || s.contains(char::is_whitespace) {
        return None;
    }
    // 全格式（IPv4 `1.2.3.4:7070` / IPv6 `[::1]:7070`）直接过——含端口的
    // IPv4 也带 ':'，先试整体解析再谈补端口。
    if let Ok(full) = s.parse::<SocketAddr>() {
        return Some(full);
    }
    let with_port = if s.starts_with('[') && s.ends_with(']') {
        // [v6] 裸括号地址 → 补端口
        format!("{s}:{}", os_p2p::P2P_PORT_DEFAULT)
    } else if s.contains(':') {
        // 纯 IPv6（多冒号裸地址）→ 包括号补端口
        format!("[{s}]:{}", os_p2p::P2P_PORT_DEFAULT)
    } else {
        // 裸 IPv4（残缺输入交给 parse 判死刑）
        format!("{s}:{}", os_p2p::P2P_PORT_DEFAULT)
    };
    with_port.parse().ok()
}

// ----------------------------------------------------------------------------
// 单元测——路由矩阵 / 未启用 503 / 三端点字段 / 归属判定 / 回环/裸 IP 解析
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use os_identity::EvidenceKind;
    use std::sync::Arc;

    fn get_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    /// 构造一个预填账本：A（握手验证 addr1 + gossip addr2 + 冲突）+ B（probe 验证 addr3）。
    fn fixture_ledger() -> SharedLedger {
        let ledger = Arc::new(Mutex::new(IdentityLedger::new(None)));
        {
            let mut l = ledger.lock().unwrap();
            l.record_evidence(
                "0x01aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "203.0.113.1:41000".parse().unwrap(),
                EvidenceKind::Handshake,
                1000,
            );
            l.record_evidence(
                "0x01aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "203.0.113.2:41000".parse().unwrap(),
                EvidenceKind::Gossip { verified: false },
                1010,
            );
            l.record_evidence(
                "0x02bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "203.0.113.3:41000".parse().unwrap(),
                EvidenceKind::ProbeVerified,
                1020,
            );
            l.record_conflict(
                "0x01aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "127.0.0.1:33516".parse().unwrap(),
                1030,
            );
        }
        ledger
    }

    const A_ID: &str = "0x01aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B_ID: &str = "0x02bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    // 1. 路由矩阵：3 条 GET 全部 identity 组件、公开读、无角色
    #[tokio::test]
    async fn routes_declare_public_read_matrix() {
        let h = IdentityRouteHandler::new_disabled();
        let routes = h.routes().await;
        assert_eq!(routes.len(), 3, "3 条路由");
        for r in &routes {
            assert_eq!(r.method, HttpMethod::Get);
            assert_eq!(r.handler_component, COMPONENT);
            assert!(!r.requires_auth, "开发期公开读");
            assert!(r.required_roles.is_empty());
        }
    }

    // 2. 未启用：全部端点 503 + 统一引导文案；:addr 非法 400 优先于 503 之外
    #[tokio::test]
    async fn disabled_returns_503_with_guide_message() {
        let h = IdentityRouteHandler::new_disabled();
        assert!(!h.is_enabled());
        for req in [
            get_req(PATH_RECORDS),
            get_req("/api/v1/identity/addr/203.0.113.1:41000"),
            get_req(PATH_CONFLICTS),
        ] {
            let resp = h.handle(req).await.unwrap();
            assert_eq!(resp.status, 503, "未启用语义 503");
            assert_eq!(resp.body["error"].as_str().unwrap(), DISABLED_MSG);
        }
    }

    // 3. records：全量记录透出（verified/unverified 地址集 + 冲突内嵌 + 按
    //    last_seen 降序——B 1020 在 A 1030 之后？A 冲突更新 last_seen=1030 → A 前）
    #[tokio::test]
    async fn records_lists_identity_records_with_shape() {
        let h = IdentityRouteHandler::new(fixture_ledger());
        let resp = h.handle(get_req(PATH_RECORDS)).await.unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().expect("records 应为数组");
        assert_eq!(arr.len(), 2, "两个身份各一条");
        // last_seen 降序：A（1030，冲突记账续期）在前
        assert_eq!(arr[0]["node_id"], A_ID);
        assert_eq!(arr[1]["node_id"], B_ID);
        let a = &arr[0];
        assert_eq!(
            a["verified_addrs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["203.0.113.1:41000"],
            "握手验证地址入 verified 集"
        );
        assert_eq!(
            a["unverified_addrs"][0], "203.0.113.2:41000",
            "gossip 转述地址入 unverified 集"
        );
        assert_eq!(a["first_seen"], 1000);
        assert_eq!(a["last_seen"], 1030);
        assert_eq!(
            a["conflict_entries"].as_array().unwrap().len(),
            1,
            "冲突观测内嵌记录"
        );
        assert_eq!(a["conflict_entries"][0]["addr"], "127.0.0.1:33516");
        assert_eq!(a["mismatch_events"].as_array().unwrap().len(), 0);
    }

    // 4. addr 归属查询：verified 主 / unverified 主 / 无主；裸 IP 补 7070；
    //    非法 addr 400
    #[tokio::test]
    async fn addr_lookup_reports_owner_and_verified_state() {
        let h = IdentityRouteHandler::new(fixture_ledger());
        // verified 主
        let resp = h
            .handle(get_req("/api/v1/identity/addr/203.0.113.1:41000"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["addr"], "203.0.113.1:41000");
        assert_eq!(resp.body["owner"], A_ID);
        assert_eq!(resp.body["verified"], true);
        assert_eq!(resp.body["ownership"], "verified");
        assert_eq!(resp.body["record"]["node_id"], A_ID, "归属记录全量附带");
        // unverified 主（gossip 转述）
        let resp = h
            .handle(get_req("/api/v1/identity/addr/203.0.113.2:41000"))
            .await
            .unwrap();
        assert_eq!(resp.body["owner"], A_ID);
        assert_eq!(resp.body["verified"], false);
        assert_eq!(resp.body["ownership"], "unverified");
        // 无主
        let resp = h
            .handle(get_req("/api/v1/identity/addr/198.51.100.9:9999"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["owner"], serde_json::Value::Null);
        assert_eq!(resp.body["ownership"], "unknown");
        assert!(resp.body["record"].is_null());
        // 裸 IP 补默认端口 7070
        let resp = h
            .handle(get_req("/api/v1/identity/addr/203.0.113.1"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["addr"], "203.0.113.1:7070", "裸 IP 补 7070");
        // 非法 addr → 400
        let resp = h
            .handle(get_req("/api/v1/identity/addr/not-an-addr"))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // 未匹配路由 → 404
        let resp = h.handle(get_req("/api/v1/identity/unknown")).await.unwrap();
        assert_eq!(resp.status, 404);
    }

    // 4b. IPv6 三形态解析（A3）：裸多冒号 / [v6] 括号裸地址 / [v6]:port 全格式；
    //     裸 `::1` 直接拼 :7070 是非法串——修复后自动包括号补端口
    #[tokio::test]
    async fn addr_lookup_ipv6_three_forms() {
        let ledger = Arc::new(Mutex::new(IdentityLedger::new(None)));
        {
            let mut l = ledger.lock().unwrap();
            l.record_evidence(
                A_ID,
                "[2001:db8::1]:41000".parse().unwrap(),
                EvidenceKind::Handshake,
                1000,
            );
        }
        let h = IdentityRouteHandler::new(ledger);
        // 形态一：裸多冒号 IPv6 → [..]:7070（默认端口查不到 41000 的记录，
        // 但 addr 字段规整成功——解析不再 400）
        let resp = h
            .handle(get_req("/api/v1/identity/addr/2001:db8::1"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "裸 IPv6 不再 400: {}", resp.body);
        assert_eq!(resp.body["addr"], "[2001:db8::1]:7070", "包括号补默认端口");
        assert_eq!(resp.body["ownership"], "unknown", "端口不同非同键");
        // 形态二：[v6] 括号裸地址 → 补端口
        let resp = h
            .handle(get_req("/api/v1/identity/addr/[2001:db8::1]"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["addr"], "[2001:db8::1]:7070");
        // 形态三：[v6]:port 全格式 → 原样（命中账本记录）
        let resp = h
            .handle(get_req("/api/v1/identity/addr/[2001:db8::1]:41000"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["addr"], "[2001:db8::1]:41000");
        assert_eq!(resp.body["owner"], A_ID);
        assert_eq!(resp.body["verified"], true);
        assert_eq!(resp.body["ownership"], "verified");
        assert_eq!(resp.body["record"]["node_id"], A_ID, "A4：单锁内按键取记录");
        // 裸回环 IPv6（::1）同样规整成功（账本拒收回环地址 → 无主）
        let resp = h
            .handle(get_req("/api/v1/identity/addr/::1"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "裸 ::1 不再 400: {}", resp.body);
        assert_eq!(resp.body["addr"], "[::1]:7070");
        assert_eq!(resp.body["owner"], serde_json::Value::Null);
    }

    // 5. conflicts：同 p2p identity-conflicts 端点同源同形（node_id/
    //    remote_addr/first_seen/last_seen/warning_count）
    #[tokio::test]
    async fn conflicts_mirror_ledger_observations() {
        let h = IdentityRouteHandler::new(fixture_ledger());
        let resp = h.handle(get_req(PATH_CONFLICTS)).await.unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["node_id"], A_ID);
        assert_eq!(arr[0]["remote_addr"], "127.0.0.1:33516");
        assert_eq!(arr[0]["warning_count"], 1);
        assert_eq!(arr[0]["first_seen"], 1030);
        assert_eq!(arr[0]["last_seen"], 1030);
        // 空账本 → 空数组（200，非 503——启用态语义）
        let empty = IdentityRouteHandler::new(Arc::new(Mutex::new(IdentityLedger::new(None))));
        let resp = empty.handle(get_req(PATH_CONFLICTS)).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body.as_array().unwrap().len(), 0);
    }

    // 6. 端到端：真实双节点 mesh（共享账本注入 A）→ records 含 B 的握手验证
    //    地址 + addr 查询 owner=B verified=true（账本经 os-p2p 事实事件填充）
    #[tokio::test]
    async fn real_mesh_fills_ledger_via_p2p_events() {
        use os_p2p::{P2pConfig, P2pNode, Timing};
        // 本机非回环 IPv4（回环证据由账本拒绝——mesh 必须经非回环地址互拨）
        let lan_ip = {
            let s = std::net::UdpSocket::bind("0.0.0.0:0").expect("UDP bind");
            s.connect("8.8.8.8:80").expect("connect（仅选路）");
            match s.local_addr().expect("local_addr").ip() {
                std::net::IpAddr::V4(v4) if !v4.is_loopback() => v4,
                other => panic!("无非回环本机 IPv4: {other}"),
            }
        };
        let ledger: SharedLedger = Arc::new(Mutex::new(IdentityLedger::new(None)));
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
        // 等 A 注册表收录 B（握手证据同刻落账）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let b_addr = loop {
            let metas = a.node_meta().await;
            if let Some(e) = metas.iter().find(|e| e.id == *b.self_id()) {
                if let Some(ma) = e.addrs.first() {
                    break ma.addr;
                }
            }
            if std::time::Instant::now() > deadline {
                panic!("10s 内 A 的注册表未收录 B");
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        };
        let h = IdentityRouteHandler::new(ledger);
        // records 含 B（握手验证地址）
        let resp = h.handle(get_req(PATH_RECORDS)).await.unwrap();
        let entry = resp
            .body
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["node_id"] == b.self_id().to_hex())
            .expect("账本 records 应含 B");
        assert_eq!(entry["verified_addrs"][0], b_addr.to_string());
        // addr 查询：owner=B + verified
        let resp = h
            .handle(get_req(&format!("/api/v1/identity/addr/{b_addr}")))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["owner"], b.self_id().to_hex());
        assert_eq!(resp.body["verified"], true);
        assert_eq!(resp.body["ownership"], "verified");
        a.shutdown().await;
        b.shutdown().await;
    }
}
