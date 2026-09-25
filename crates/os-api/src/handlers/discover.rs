//! `DiscoverRouteHandler` —— 把节点发现 HTTP 请求适配到内存节点列表
//! （规划文档 §3.6 / §3.14 / §9.1#10）。
//!
//! 定位：
//! - 实现 [`RouteHandler`]（`#[async_trait]`），声明 `/discover/nodes`、
//!   `/api/v1/nodes`、`/api/v1/nodes/:id` 三条 GET 路由。
//! - os CLI 的 `discover` 命令（`GET /discover/nodes`）经此 handler 返回
//!   已知节点列表 JSON。
//!
//! # 为什么用内存节点态而非真实 mDNS
//!
//! 真实 LAN 节点发现（`os_discover::MdnsDiscovery`）依赖 mdns-sd 组播 +
//! 后台守护进程 + beacon 签名/验签链路，且 `discover_peers` 是带超时的
//! 阻塞扫描——直接在 HTTP handler 里跑会拖慢请求且需网络组播权限。
//! 本 handler 先持有一份**内存节点列表**（默认含本机节点 + 基本属性：
//! hostname/role/version/arch），满足 os CLI `discover` 的可观测需求。
//!
//! 后续接通真实 `Discovery` 时，只需把 `nodes` 字段换成从 `Discovery::discover_peers`
//! 取回的 `Vec<PeerNode>`（trait 签名不变，红线：不改 trait）。
//!
//! # 路径参数
//!
//! 网关 dispatch 当前不向 handler 传递 `PathParams`，故 `handle` 从 `req.path`
//! 字符串解析 `/api/v1/nodes/:id` 的 id 段（参考 `PlaceholderHandler` 的
//! `split('?')` 模式）。
//!
//! # 错误转换
//!
//! 节点未找到时返回 `Ok(ApiResponse{ status: 404, .. })`（非 `Err`）——
//! dispatch 把 `Err` 统一渲染成 500，故 404 必须在 handle 内显式返回。

use std::sync::Arc;

use async_trait::async_trait;
use os_core::NodeId;
use os_discover::{NodeCapabilities, PeerNode};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// Handler 主体
// ----------------------------------------------------------------------------

/// 节点发现路由处理器——HTTP 边界适配到内存节点列表。
///
/// 持有 `Arc<Vec<PeerNode>>`（默认构造时填入本机节点）。生产构造：
/// `DiscoverRouteHandler::new_local()`（自动探测本机 hostname/arch/version）；
/// 测试构造：`DiscoverRouteHandler::with_nodes(vec![...])` 注入任意节点列表。
///
/// 后续接通真实 mDNS 时，可把构造换成持有 `Arc<dyn Discovery>` 并在 handle 里
/// 调 `discover_peers`（trait 签名不变）。
pub struct DiscoverRouteHandler {
    nodes: Arc<Vec<PeerNode>>,
}

impl DiscoverRouteHandler {
    /// 用一个已知的节点列表构造 handler。
    ///
    /// 生产/测试通用入口：调用方负责构造节点列表（本机节点或 fixture）。
    #[must_use]
    pub fn with_nodes(nodes: Vec<PeerNode>) -> Self {
        Self {
            nodes: Arc::new(nodes),
        }
    }

    /// 构造一个默认只含本机节点的 handler（同步——启动装配期一次性探测）。
    ///
    /// 本机节点信息：
    /// - `node_id`：取主机名（`hostname` 命令；失败回退 `"local"`）；
    /// - `endpoints`：`["0.0.0.0:8080"]`（网关默认监听地址）；
    /// - `version`：os-api crate 版本（`CARGO_PKG_VERSION`）；
    /// - `arch`：`std::env::consts::ARCH`；
    /// - `capabilities`：本机真实能力探测（KVM/ZFS/网卡/存储容量），
    ///   跑 `zpool list -H` / 读 `/dev/kvm` / 读 `/sys/class/net/<if>/speed`。
    ///
    /// **同步探测的理由**：本构造在启动装配期（`main.rs` 的 `#[tokio::main]`
    /// 内、注册路由之前）执行一次，探测是快速命令 + 文件读（合计 <50ms），
    /// 一次性同步开销可接受且语义清晰（无需在同步签名里桥接 async）。
    /// 运行期每次 `GET /discover/nodes` 只读内存 `Arc<Vec<PeerNode>>`，零阻塞。
    #[must_use]
    pub fn new_local() -> Self {
        Self::with_nodes(vec![local_node_sync()])
    }
}

impl Default for DiscoverRouteHandler {
    fn default() -> Self {
        // Default 必须同步——直接走同步探测路径（真实 KVM/ZFS/网卡探测）。
        Self::with_nodes(vec![local_node_sync()])
    }
}

#[async_trait]
impl RouteHandler for DiscoverRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec(HttpMethod::Get, PATH_DISCOVER_NODES),
            spec(HttpMethod::Get, PATH_API_NODES),
            spec(HttpMethod::Get, PATH_API_NODE_BY_ID),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        // 去掉 query 串，只按路径分发（参考 PlaceholderHandler 的 split('?') 模式）
        let path = req.path.split('?').next().unwrap_or("");
        match (req.method, path) {
            // —— GET /discover/nodes —— os CLI discover 命令对应入口
            (HttpMethod::Get, PATH_DISCOVER_NODES) => Ok(list_nodes_response(&self.nodes)),
            // —— GET /api/v1/nodes —— RESTful 别名（与 /discover/nodes 等价）
            (HttpMethod::Get, PATH_API_NODES) => Ok(list_nodes_response(&self.nodes)),
            // —— GET /api/v1/nodes/:id —— 单节点详情
            (HttpMethod::Get, p) if p.starts_with(PATH_API_NODES_PREFIX) => {
                let id = extract_node_id(p);
                match id {
                    Some(id) => match find_node(&self.nodes, &id) {
                        Some(node) => Ok(ok_json(to_value(node)?)),
                        None => Ok(error_response(404, &format!("节点不存在: {id}"))),
                    },
                    None => Ok(error_response(404, "节点 ID 缺失")),
                }
            }
            // —— 未覆盖的路由 —— 兜底 404（Ok，非 Err，便于网关聚合层定位）
            _ => Ok(error_response(404, "discover: 未匹配的路由")),
        }
    }
}

// ----------------------------------------------------------------------------
// 内部辅助
// ----------------------------------------------------------------------------

/// `GET /discover/nodes` —— os CLI discover 命令入口。
const PATH_DISCOVER_NODES: &str = "/discover/nodes";
/// `GET /api/v1/nodes` —— RESTful 别名。
const PATH_API_NODES: &str = "/api/v1/nodes";
/// `GET /api/v1/nodes/:id` —— 单节点详情。
const PATH_API_NODE_BY_ID: &str = "/api/v1/nodes/:id";
/// `/api/v1/nodes/` 前缀（运行时从 path 解析 `:id` 段用）。
const PATH_API_NODES_PREFIX: &str = "/api/v1/nodes/";

/// 本 handler 注册时的组件名（`RouteSpec::handler_component`）。
const COMPONENT: &str = "discover";

/// 构造一条 GET 路由规格（统一 `discover` 组件名 + 免认证——发现列表不涉敏感数据）。
fn spec(method: HttpMethod, path: &str) -> RouteSpec {
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

/// 把可序列化结果转成 `serde_json::Value`，序列化失败统一映射为 `ApiGatewayError::Internal`。
///
/// `?Sized` 放宽以便传入 slice 引用（如 `&[PeerNode]`）。
fn to_value<T: serde::Serialize + ?Sized>(v: &T) -> Result<serde_json::Value, ApiGatewayError> {
    serde_json::to_value(v).map_err(|e| ApiGatewayError::Internal(format!("响应序列化失败: {e}")))
}

/// 构造一个最小 JSON 错误响应（status 由调用方指定）。
fn error_response(status: u16, msg: &str) -> ApiResponse {
    ApiResponse {
        status,
        body: serde_json::json!({"error": msg}),
        headers: serde_json::json!({}),
    }
}

/// 构造"列出全部节点"的响应：200 + 节点数组 JSON。
fn list_nodes_response(nodes: &[PeerNode]) -> ApiResponse {
    let body = to_value(nodes).unwrap_or_else(|e| {
        serde_json::json!({
            "error": format!("节点列表序列化失败: {e}"),
            "nodes": [],
        })
    });
    ok_json(body)
}

/// 从 `/api/v1/nodes/<id>` 路径解析出 `<id>` 段（去掉 query 后取末段）。
///
/// 返回 None 表示路径无 id 段（如 `/api/v1/nodes/`，空 id）。
fn extract_node_id(path: &str) -> Option<String> {
    // path 已是去掉 query 的纯路径（调用方 split('?') 处理过）
    let rest = path.strip_prefix(PATH_API_NODES_PREFIX)?;
    let id = rest.split('/').next()?;
    if id.is_empty() {
        return None;
    }
    Some(id.to_string())
}

/// 在节点列表中按 node_id 找单节点（精确匹配）。
fn find_node<'a>(nodes: &'a [PeerNode], id: &str) -> Option<&'a PeerNode> {
    nodes.iter().find(|n| n.node_id.as_str() == id)
}

/// 探测本机主机名（`hostname` 命令；失败回退 `"local"`）。
///
/// 用 `tokio::task::spawn_blocking` 调 `hostname` 会引入 async 复杂度，而
/// 构造期同步执行 `hostname` 命令开销极小（<5ms），故直接同步调用。
/// 若 `hostname` 命令不存在或失败，回退到 `"local"`。
///
/// `pub(crate)`：node_view.rs 的自机信息兜底展示复用同一探测逻辑。
pub(crate) fn detect_hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "local".to_string())
}

/// 构造本机节点（hostname + 版本 + arch + **真实探测能力**）。
///
/// 同步实现：跑 `hostname` / `zpool list -H` / 读 `/dev/kvm` / 读网卡 speed。
/// 适合在 `spawn_blocking` 池里跑（`new_local()`）或同步构造（`Default`）。
///
/// 能力探测见 [`detect_capabilities`]——单项失败时该字段回退保守值（不抛错），
/// 保证节点发现始终可用（探测是"尽力而为"，参考 `system.rs` 的尽力聚合语义）。
fn local_node_sync() -> PeerNode {
    PeerNode {
        node_id: NodeId::new(detect_hostname()),
        endpoints: vec!["0.0.0.0:8080".to_string()],
        version: env!("CARGO_PKG_VERSION").to_string(),
        arch: std::env::consts::ARCH.to_string(),
        capabilities: detect_capabilities(),
        beacon_signature: None,
    }
}

/// 探测本机真实能力（KVM / ZFS / 存储容量 / 网络带宽）。
///
/// 全部走同步命令/文件读，调用方应在 `spawn_blocking` 池里调（见 `new_local`）。
/// 单项探测失败不影响整体——失败字段回退保守值（false/0/默认 1Gbps），
/// 与 `system.rs` 的"尽力聚合、单项失败不拉垮整机探针"语义一致。
fn detect_capabilities() -> NodeCapabilities {
    let has_kvm = detect_kvm();
    let (has_zfs, storage_capacity_gb) = detect_zfs();
    let network_gbps = detect_network_gbps();
    NodeCapabilities {
        supports_ha: false, // 单机无法判断，需多节点（保持 false）
        storage_capacity_gb,
        network_gbps,
        has_zfs,
        has_kvm,
        rdma: false, // 无 RDMA 硬件，保持 false
        dpu: false,  // 无 DPU 硬件，保持 false
    }
}

/// 探测 KVM：`/dev/kvm` 是否存在（存在即表示内核 KVM 模块可用）。
fn detect_kvm() -> bool {
    std::path::Path::new("/dev/kvm").exists()
}

/// 探测 ZFS + 存储总容量：跑 `zpool list -H`，解析输出。
///
/// `zpool list -H` 输出形如（tab 分隔）：
/// ```text
/// tank   928G   612K   928G   -   -   0%   0%   1.00x   ONLINE   -
/// ```
/// - 第 1 列：pool 名；第 2 列：SIZE（如 `928G`）。
/// - 有任意非空行 = 有 ZFS 池（`has_zfs = true`）。
/// - `storage_capacity_gb` 取第一行 SIZE 解析（G/T/P → GB；无单位视为 GB）。
/// - 命令不存在/无输出/解析失败 → `(false, 0)`。
fn detect_zfs() -> (bool, u64) {
    let output = match std::process::Command::new("zpool")
        .args(["list", "-H"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return (false, 0),
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let first = match text.lines().find(|l| !l.trim().is_empty()) {
        Some(l) => l,
        None => return (false, 0),
    };
    let size_field = first.split_whitespace().nth(1).unwrap_or("");
    let capacity_gb = parse_zpool_size(size_field).unwrap_or(0);
    (true, capacity_gb)
}

/// 解析 zpool SIZE 字段为 GB（`928G` → 928，`2T` → 2048，`1P` → 1_048_576）。
///
/// 无单位或未知单位按 GB 处理（保守）；解析失败返回 None。
fn parse_zpool_size(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // 分离数字部分与单位后缀
    let (num_part, suffix) = s
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '_')
        .map(|i| s.split_at(i))
        .unwrap_or((s, ""));
    let n: f64 = num_part.replace('_', "").parse().ok()?;
    let multiplier: f64 = match suffix {
        "T" | "TiB" | "TIB" => 1024.0,
        "P" | "PiB" | "PIB" => 1024.0 * 1024.0,
        "M" | "MiB" | "MIB" => 1.0 / 1024.0,
        "K" | "KiB" | "KIB" => 1.0 / (1024.0 * 1024.0),
        "G" | "GiB" | "GIB" | "" => 1.0,
        _ => 1.0, // 未知单位保守按 GB
    };
    Some((n * multiplier) as u64)
}

/// 探测网络带宽（Gbps）：遍历 `/sys/class/net/<iface>/speed`，取活跃接口最大速率。
///
/// - speed 文件值为 Mbps（如 `1000` = 1Gbps），转 Gbps = `value / 1000`。
/// - 跳过 `lo` 回环；speed 读不到（如 wifi 无线/网卡 down）跳过该接口。
/// - 全部读不到 → 默认 `1.0`（保守，假设百兆以上以太网）。
fn detect_network_gbps() -> f32 {
    let entries = match std::fs::read_dir("/sys/class/net") {
        Ok(e) => e,
        Err(_) => return 1.0,
    };
    let mut max_mbps: u64 = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "lo" {
            continue;
        }
        let speed_path = format!("/sys/class/net/{name}/speed");
        if let Ok(content) = std::fs::read_to_string(&speed_path) {
            if let Ok(mbps) = content.trim().parse::<u64>() {
                if mbps > max_mbps {
                    max_mbps = mbps;
                }
            }
        }
    }
    if max_mbps > 0 {
        (max_mbps as f32) / 1000.0
    } else {
        1.0
    }
}

// ----------------------------------------------------------------------------
// 单元测——注入节点列表，测路由→JSON 全链路
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个测试用节点（id/version/endpoints 可控）。
    fn test_node(id: &str) -> PeerNode {
        PeerNode {
            node_id: NodeId::new(id),
            endpoints: vec![format!("10.0.0.{id_tail}:8443", id_tail = id)],
            version: "1.5.0".to_string(),
            arch: "x86_64".to_string(),
            capabilities: NodeCapabilities::full(),
            beacon_signature: None,
        }
    }

    /// 构造一个对指定路径的 GET 请求。
    fn get_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.to_string(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    // —— routes() 声明 ——

    #[tokio::test]
    async fn routes_declares_three_get_endpoints() {
        let h = DiscoverRouteHandler::with_nodes(vec![]);
        let routes = h.routes().await;
        assert_eq!(routes.len(), 3, "应声明 3 条路由");
        for r in &routes {
            assert_eq!(r.method, HttpMethod::Get, "全部应为 GET");
            assert_eq!(r.handler_component, COMPONENT);
            assert!(!r.requires_auth, "发现列表免认证");
            assert!(r.required_roles.is_empty(), "无角色要求");
        }
        let paths: Vec<&str> = routes.iter().map(|r| r.path.as_str()).collect();
        assert!(paths.contains(&PATH_DISCOVER_NODES));
        assert!(paths.contains(&PATH_API_NODES));
        assert!(paths.contains(&PATH_API_NODE_BY_ID));
    }

    // —— GET /discover/nodes ——

    #[tokio::test]
    async fn discover_nodes_returns_node_list() {
        let h = DiscoverRouteHandler::with_nodes(vec![test_node("n1"), test_node("n2")]);
        let resp = h
            .handle(get_req(PATH_DISCOVER_NODES))
            .await
            .expect("discover 应成功");
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().expect("body 应为数组");
        assert_eq!(arr.len(), 2);
        // NodeId newtype 序列化为裸字符串
        assert_eq!(arr[0]["node_id"], "n1");
        assert_eq!(arr[1]["node_id"], "n2");
        // 核心字段存在
        assert_eq!(arr[0]["version"], "1.5.0");
        assert_eq!(arr[0]["arch"], "x86_64");
        assert!(arr[0]["endpoints"].is_array());
    }

    #[tokio::test]
    async fn discover_nodes_empty_returns_empty_array() {
        let h = DiscoverRouteHandler::with_nodes(vec![]);
        let resp = h.handle(get_req(PATH_DISCOVER_NODES)).await.unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn discover_nodes_query_string_stripped() {
        // ?lan=10.0.0.0/24 不影响路径分发（验证 split('?') 兜底）
        let h = DiscoverRouteHandler::with_nodes(vec![test_node("n1")]);
        let resp = h
            .handle(get_req("/discover/nodes?lan=10.0.0.0/24"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body.as_array().unwrap().len(), 1);
    }

    // —— GET /api/v1/nodes（别名）——

    #[tokio::test]
    async fn api_v1_nodes_returns_same_as_discover() {
        let h = DiscoverRouteHandler::with_nodes(vec![test_node("n1")]);
        let resp = h.handle(get_req(PATH_API_NODES)).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body.as_array().unwrap().len(), 1);
        assert_eq!(resp.body[0]["node_id"], "n1");
    }

    // —— GET /api/v1/nodes/:id ——

    #[tokio::test]
    async fn get_node_by_id_returns_detail() {
        let h = DiscoverRouteHandler::with_nodes(vec![test_node("n1"), test_node("n2")]);
        let resp = h
            .handle(get_req("/api/v1/nodes/n2"))
            .await
            .expect("单节点详情应成功");
        assert_eq!(resp.status, 200);
        // body 是单节点对象（非数组）
        assert_eq!(resp.body["node_id"], "n2");
        assert_eq!(resp.body["version"], "1.5.0");
    }

    #[tokio::test]
    async fn get_node_by_id_not_found_returns_404() {
        let h = DiscoverRouteHandler::with_nodes(vec![test_node("n1")]);
        let resp = h.handle(get_req("/api/v1/nodes/nope")).await.unwrap();
        assert_eq!(resp.status, 404);
        assert!(resp.body["error"].as_str().unwrap().contains("节点不存在"));
    }

    #[tokio::test]
    async fn get_node_by_id_empty_id_returns_404() {
        // /api/v1/nodes/ → 空 id → 404
        let h = DiscoverRouteHandler::with_nodes(vec![test_node("n1")]);
        let resp = h.handle(get_req("/api/v1/nodes/")).await.unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn get_node_by_id_strips_query() {
        // /api/v1/nodes/n1?verbose=1 → query 不影响 id 解析
        let h = DiscoverRouteHandler::with_nodes(vec![test_node("n1")]);
        let resp = h
            .handle(get_req("/api/v1/nodes/n1?verbose=1"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["node_id"], "n1");
    }

    // —— 兜底 ——

    #[tokio::test]
    async fn unmatched_route_returns_404_body() {
        // POST /discover/nodes 未声明 → 兜底 404（Ok，非 Err）
        let h = DiscoverRouteHandler::with_nodes(vec![]);
        let req = ApiRequest {
            method: HttpMethod::Post,
            path: PATH_DISCOVER_NODES.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        };
        let resp = h.handle(req).await.unwrap();
        assert_eq!(resp.status, 404);
        assert!(resp.body["error"].as_str().unwrap().contains("未匹配"));
    }

    // —— 辅助函数自测 ——

    #[test]
    fn extract_node_id_parses_simple() {
        assert_eq!(extract_node_id("/api/v1/nodes/n1"), Some("n1".into()));
        assert_eq!(
            extract_node_id("/api/v1/nodes/node-abc"),
            Some("node-abc".into())
        );
        // query 会被调用方先 split('?')，此处测纯路径
        assert_eq!(extract_node_id("/api/v1/nodes/"), None);
        assert_eq!(extract_node_id("/api/v1/pools"), None);
    }

    #[test]
    fn find_node_matches_exact_id() {
        let nodes = vec![test_node("n1"), test_node("n2")];
        assert!(find_node(&nodes, "n1").is_some());
        assert!(find_node(&nodes, "nope").is_none());
    }

    // —— 默认构造（本机节点）——

    #[tokio::test]
    async fn new_local_yields_at_least_one_node() {
        // 默认构造应含本机节点（hostname 或 "local"）
        // new_local() 同步——若在 runtime 内走 spawn_blocking 探测（本测试在
        // #[tokio::test] runtime 内，会走 spawn_blocking 路径）
        let h = DiscoverRouteHandler::new_local();
        let resp = h.handle(get_req(PATH_DISCOVER_NODES)).await.unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().unwrap();
        assert!(!arr.is_empty(), "本机节点列表非空");
        // node_id 是非空字符串
        let id = arr[0]["node_id"].as_str().expect("node_id 是字符串");
        assert!(!id.is_empty());
        // 版本字段非空（取自 CARGO_PKG_VERSION）
        let v = arr[0]["version"].as_str().expect("version 是字符串");
        assert!(!v.is_empty());
        // arch 字段非空
        let a = arr[0]["arch"].as_str().expect("arch 是字符串");
        assert!(!a.is_empty());
    }

    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<DiscoverRouteHandler>();
    }

    // —— 能力探测解析单测（纯逻辑，无外部依赖）——

    #[test]
    fn parse_zpool_size_gigabytes() {
        // 928G → 928（本机 tank 池真实输出）
        assert_eq!(parse_zpool_size("928G"), Some(928));
        assert_eq!(parse_zpool_size("928GiB"), Some(928));
    }

    #[test]
    fn parse_zpool_size_terabytes() {
        // 2T → 2048 GB
        assert_eq!(parse_zpool_size("2T"), Some(2048));
        assert_eq!(parse_zpool_size("2TiB"), Some(2048));
        // 1.5T → 1536 GB
        assert_eq!(parse_zpool_size("1.5T"), Some(1536));
    }

    #[test]
    fn parse_zpool_size_petabytes() {
        // 1P → 1_048_576 GB
        assert_eq!(parse_zpool_size("1P"), Some(1_048_576));
    }

    #[test]
    fn parse_zpool_size_megabytes_and_bare_number() {
        // 512M → 0 GB（向下取整，< 1GB）
        assert_eq!(parse_zpool_size("512M"), Some(0));
        // 无单位按 GB 处理
        assert_eq!(parse_zpool_size("100"), Some(100));
    }

    #[test]
    fn parse_zpool_size_empty_and_garbage() {
        assert_eq!(parse_zpool_size(""), None);
        // 纯字母非数字 → 解析失败
        assert_eq!(parse_zpool_size("abc"), None);
        // 仅单位无数字 → 解析失败
        assert_eq!(parse_zpool_size("G"), None);
    }

    #[test]
    fn detect_kvm_returns_bool_without_panic() {
        // 本机有 /dev/kvm 时 true；无（CI/容器）时 false——两者皆合法
        let _ = detect_kvm();
    }

    #[test]
    fn detect_capabilities_includes_real_caps_without_panic() {
        // 探测不应 panic（命令缺失/文件不可读都应优雅回退）
        let caps = detect_capabilities();
        // 打印真实探测值，便于 `--nocapture` 排障（本机：KVM/ZFS/网卡）
        println!(
            "detect_capabilities → has_kvm={} has_zfs={} storage={}GB net={}Gbps",
            caps.has_kvm, caps.has_zfs, caps.storage_capacity_gb, caps.network_gbps
        );
        // network_gbps 合理范围（默认 1.0 或更大）
        assert!(caps.network_gbps > 0.0, "网络带宽应 > 0");
        // 单机 supports_ha 恒 false
        assert!(!caps.supports_ha);
        // rdma/dpu 当前硬件恒 false
        assert!(!caps.rdma);
        assert!(!caps.dpu);
    }

    /// 当本机有 /dev/kvm 时，探测应返回 has_kvm=true（环境断言，CI 无 KVM 时跳过）。
    #[test]
    fn detect_kvm_matches_dev_kvm_presence() {
        let expected = std::path::Path::new("/dev/kvm").exists();
        let actual = detect_kvm();
        assert_eq!(actual, expected, "detect_kvm 应与 /dev/kvm 存在性一致");
    }

    /// 当本机能跑 `zpool list -H` 且有输出时，探测应返回 has_zfs=true。
    #[test]
    fn detect_zfs_matches_zpool_list_when_available() {
        let zpool_ok = std::process::Command::new("zpool")
            .args(["list", "-H"])
            .output()
            .map(|o| o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().is_empty())
            .unwrap_or(false);
        let (has_zfs, storage) = detect_zfs();
        if zpool_ok {
            assert!(has_zfs, "zpool list -H 有输出时应探测 has_zfs=true");
            assert!(storage > 0, "zpool 有池时 storage_capacity_gb 应 > 0");
        }
        // zpool 不可用时无强断言（has_zfs 应为 false）
    }

    /// 验证 PeerNode JSON 结构与 os CLI / os-mobile 客户端期望一致
    /// （NodeId 序列化为裸字符串，endpoints 数组，capabilities 对象）。
    #[test]
    fn peer_node_json_shape_matches_client_contract() {
        let node = test_node("n1");
        let json = serde_json::to_value(&node).unwrap();
        // node_id 是裸字符串（非 {"": ...} 对象）
        assert_eq!(json["node_id"], "n1");
        assert!(json["node_id"].is_string(), "node_id 必须是字符串");
        // endpoints 是字符串数组
        assert!(json["endpoints"].is_array());
        // capabilities 是对象
        assert!(json["capabilities"].is_object());
        assert!(json["capabilities"]["supports_ha"].is_boolean());
    }
}
