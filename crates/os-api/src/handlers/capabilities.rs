//! 能力快照端点（`GET /api/v1/capabilities`，读公开）——@nexos/app-sdk 的
//! 服务端数据面（v0.1.28 批次，docs/APPS.md「应用 SDK」章）。
//!
//! # 设计红线
//!
//! - **秒回、不主动探测联邦**：全部字段聚合既有 handler 的内存态 / SQLite
//!   缓存（`instances_snapshot` / `channels_snapshot` / `listings_snapshot` /
//!   `AppRegistry::installed_apps` / p2p `Handle::peers` 本地路由表），零新
//!   出站网络请求——应用启动时的能力探测绝不拖慢宿主节点。
//! - **诚实口径**：没有缓存就如实给 0 / null / false（如 lobby 无联邦条目时
//!   `reachable=false`、`last_sync_at=null`），不编造可达性。
//!
//! # 响应形状（sdk_version='0.1' 冻结面；字段增删走版本号）
//!
//! ```json
//! {
//!   "sdk_version": "0.1",
//!   "generated_at": "2026-09-04T12:00:00+00:00",
//!   "llm":     { "instances": 2, "running": ["llm-5"] },
//!   "gateway": { "channels": 3, "enabled": 2, "relay_channels": 1 },
//!   "lobby":   { "entries": 4, "last_sync_at": "…|null", "reachable": true },
//!   "media":   { "ffmpeg_available": true },
//!   "p2p":     { "enabled": true, "peers_connected": 3 },
//!   "apps": ["film"]
//! }
//! ```
//!
//! - `llm.running`：status==running 的实例 id 列表（内存态真值，重启清零）；
//! - `gateway.relay_channels`：via_node 非空的渠道数（🌐 联邦中继渠道）；
//! - `lobby.last_sync_at`：既有条目心跳缓存里最新的一条（市场/联邦心跳缓存，
//!   无则 null——**不是**发起联邦同步的时刻）；`reachable`：任一条目心跳新鲜
//!   （api_market `heartbeat_fresh`，|年龄|≤60s）即 true；
//! - `p2p.enabled=false`（`NEXOS_P2P_ENABLE` 未开，部署缺省）时
//!   `peers_connected=0`；
//! - `apps`：已装应用包 id 列表（apps.db）。
//!
//! # 可测性
//!
//! 数据源经 [`CapabilitySources`] trait 抽象：生产装配 [`RealSources`]（薄读
//! 各 handler 既有快照方法），单测注入 [`MockSources`]（零 spawn / 零网络 /
//! 零 git）。纯聚合逻辑在 [`build_capabilities_value`]——给定输入拼接响应，
//! 与数据源完全解耦。P2P（async `Handle::peers`）不在 trait 内：handler 持
//! `Option<Handle>`，handle() 内联取数，测试用 None（disabled 态）+ 纯函数
//! 测试覆盖非零对端数。

use std::sync::Arc;

use async_trait::async_trait;

use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};
use crate::handlers::api_gateway::ApiGatewayRouteHandler;
use crate::handlers::api_market::{heartbeat_fresh, ApiMarketRouteHandler};
use crate::handlers::apps_handler::AppRegistry;
use crate::handlers::llm::LlmRouteHandler;
use crate::ApiGatewayError;

/// 本 handler 注册时的组件名。
const COMPONENT: &str = "capabilities";

/// `GET /api/v1/capabilities` —— 能力快照（读公开：应用 SDK / 桌面均消费）。
const PATH_CAPABILITIES: &str = "/api/v1/capabilities";

/// SDK 协议版本（与前端 `sdk/` 模块的 `SDK_VERSION` 同步；桥上
/// `__NEXOS_HOST__.sdk.version` 同源）。破坏性字段变更必须 bump。
pub const SDK_VERSION: &str = "0.1";

// ----------------------------------------------------------------------------
// 快照 DTO（serde 形状即线格式）
// ----------------------------------------------------------------------------

/// LLM 本地推理能力面。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LlmCaps {
    /// 实例总数（全部状态）。
    pub instances: usize,
    /// status==running 的实例 id 列表。
    pub running: Vec<String>,
}

/// 网关渠道能力面。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GatewayCaps {
    /// 渠道总数。
    pub channels: usize,
    /// 启用中的渠道数。
    pub enabled: usize,
    /// 🌐 联邦中继渠道数（via_node 非空）。
    pub relay_channels: usize,
}

/// 联邦大厅（API 市场）能力面——纯缓存口径，零主动探测。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LobbyCaps {
    /// 大厅条目数（本地 + 联邦接收缓存）。
    pub entries: usize,
    /// 条目心跳缓存里最新的一条（RFC3339；无任何心跳 → null）。
    pub last_sync_at: Option<String>,
    /// 按缓存判定：任一条目心跳新鲜（≤60s）→ true；无条目/心跳全过期 → false。
    pub reachable: bool,
}

/// P2P 组网能力面。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct P2pCaps {
    /// 组网是否启用（`NEXOS_P2P_ENABLE`，部署缺省关）。
    pub enabled: bool,
    /// 已认证直连对端数。
    pub peers_connected: usize,
}

/// 一次能力快照的全部输入（聚合纯函数 [`build_capabilities_value`] 的入参）。
#[derive(Debug, Clone)]
pub struct CapInputs {
    pub llm: LlmCaps,
    pub gateway: GatewayCaps,
    pub lobby: LobbyCaps,
    pub p2p: P2pCaps,
    pub media_ffmpeg_available: bool,
    /// 已装应用包 id 列表。
    pub apps: Vec<String>,
}

// ----------------------------------------------------------------------------
// 数据源抽象（生产真实 handler / 测试 mock）
// ----------------------------------------------------------------------------

/// 能力快照数据源（同步薄读——四个源都是既有 handler 的 Mutex/SQLite 快照）。
pub trait CapabilitySources: Send + Sync {
    fn llm_caps(&self) -> LlmCaps;
    fn gateway_caps(&self) -> GatewayCaps;
    fn lobby_caps(&self) -> LobbyCaps;
    fn apps_installed(&self) -> Vec<String>;
    /// ffmpeg 可用性（生产走 film.rs `detect_ffmpeg()` 的 env→PATH→常规路径链）。
    fn ffmpeg_available(&self) -> bool;
}

/// 生产数据源：持有四个真实 handler 的 Arc（main.rs 装配注入——与
/// SharedLlmHandler / SharedApiGatewayHandler 同款共享模式）。
pub struct RealSources {
    llm: Arc<LlmRouteHandler>,
    gateway: Arc<ApiGatewayRouteHandler>,
    market: Arc<ApiMarketRouteHandler>,
    apps: Arc<AppRegistry>,
}

impl RealSources {
    pub fn new(
        llm: Arc<LlmRouteHandler>,
        gateway: Arc<ApiGatewayRouteHandler>,
        market: Arc<ApiMarketRouteHandler>,
        apps: Arc<AppRegistry>,
    ) -> Self {
        Self {
            llm,
            gateway,
            market,
            apps,
        }
    }
}

impl CapabilitySources for RealSources {
    fn llm_caps(&self) -> LlmCaps {
        let list = self.llm.instances_snapshot();
        LlmCaps {
            instances: list.len(),
            running: list
                .iter()
                .filter(|i| i.status == "running")
                .map(|i| i.id.clone())
                .collect(),
        }
    }

    fn gateway_caps(&self) -> GatewayCaps {
        let channels = self.gateway.channels_snapshot();
        GatewayCaps {
            channels: channels.len(),
            enabled: channels.iter().filter(|c| c.enabled).count(),
            relay_channels: channels.iter().filter(|c| !c.via_node.is_empty()).count(),
        }
    }

    fn lobby_caps(&self) -> LobbyCaps {
        let list = self.market.listings_snapshot();
        // 最新心跳 = 解析 RFC3339 后取时间最大值（条目心跳可能混有时区后缀
        // +08:00/+00:00——字符串字典序会错序，必须按时间轴比较），归一 UTC。
        let last_sync_at = list
            .iter()
            .filter_map(|e| e.heartbeat_at.as_deref())
            .filter_map(|h| chrono::DateTime::parse_from_rfc3339(h).ok())
            .max()
            .map(|t| t.to_rfc3339());
        LobbyCaps {
            entries: list.len(),
            reachable: list
                .iter()
                .any(|e| e.heartbeat_at.as_deref().is_some_and(heartbeat_fresh)),
            last_sync_at,
        }
    }

    fn apps_installed(&self) -> Vec<String> {
        self.apps.installed_apps().into_iter().map(|a| a.id).collect()
    }

    fn ffmpeg_available(&self) -> bool {
        crate::handlers::film::detect_ffmpeg().is_some()
    }
}

// ----------------------------------------------------------------------------
// Handler
// ----------------------------------------------------------------------------

/// 能力快照 RouteHandler（component=capabilities，读公开）。
pub struct CapabilitiesRouteHandler {
    sources: Box<dyn CapabilitySources>,
    /// P2P Handle（main.rs 装配；None=未启用 → enabled:false / peers 0）。
    p2p: Option<os_p2p::Handle>,
}

impl CapabilitiesRouteHandler {
    /// 生产构造（main.rs：llm/api_gateway/api_market/apps 共享实例 + p2p handle）。
    #[must_use]
    pub fn new(
        sources: RealSources,
        p2p: Option<os_p2p::Handle>,
    ) -> Self {
        Self {
            sources: Box::new(sources),
            p2p,
        }
    }

    /// 测试构造（注入 mock 数据源；P2P 视 disabled）。
    #[cfg(test)]
    fn with_sources(sources: Box<dyn CapabilitySources>) -> Self {
        Self {
            sources,
            p2p: None,
        }
    }

    /// 聚合一次快照响应体（handle 唯一路径；测试复用）。
    async fn snapshot_value(&self) -> serde_json::Value {
        // P2P 需要异步取对端列表——与其他源分离（trait 保持同步薄读）。
        let p2p = match &self.p2p {
            Some(h) => {
                let peers = h.peers().await;
                P2pCaps {
                    enabled: true,
                    peers_connected: peers.iter().filter(|p| p.connected).count(),
                }
            }
            None => P2pCaps {
                enabled: false,
                peers_connected: 0,
            },
        };
        let inputs = CapInputs {
            llm: self.sources.llm_caps(),
            gateway: self.sources.gateway_caps(),
            lobby: self.sources.lobby_caps(),
            p2p,
            media_ffmpeg_available: self.sources.ffmpeg_available(),
            apps: self.sources.apps_installed(),
        };
        build_capabilities_value(inputs)
    }
}

#[async_trait]
impl RouteHandler for CapabilitiesRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![RouteSpec {
            method: HttpMethod::Get,
            path: PATH_CAPABILITIES.to_string(),
            handler_component: COMPONENT.to_string(),
            requires_auth: false,
            required_roles: Vec::new(),
        }]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let path = req.path.split('?').next().unwrap_or("");
        if req.method == HttpMethod::Get && path == PATH_CAPABILITIES {
            return Ok(ApiResponse {
                status: 200,
                body: self.snapshot_value().await,
                headers: serde_json::json!({}),
            });
        }
        Ok(ApiResponse {
            status: 404,
            body: serde_json::json!({"error": "capabilities: 未匹配的路由"}),
            headers: serde_json::json!({}),
        })
    }
}

// ----------------------------------------------------------------------------
// 纯聚合（单测主战场：给定输入 → 线格式，无 IO）
// ----------------------------------------------------------------------------

/// 拼能力快照响应体（纯函数——字段增删只动这里 + 本文件模块注释）。
pub fn build_capabilities_value(inputs: CapInputs) -> serde_json::Value {
    serde_json::json!({
        "sdk_version": SDK_VERSION,
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "llm": inputs.llm,
        "gateway": inputs.gateway,
        "lobby": inputs.lobby,
        "media": { "ffmpeg_available": inputs.media_ffmpeg_available },
        "p2p": inputs.p2p,
        "apps": inputs.apps,
    })
}

// ----------------------------------------------------------------------------
// 测试（mock 数据源矩阵 + 纯聚合 + axum 全栈）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 可编程 mock 数据源（各 handler 态的替身——零 spawn / 零网络）。
    struct MockSources {
        llm: LlmCaps,
        gateway: GatewayCaps,
        lobby: LobbyCaps,
        apps: Vec<String>,
        ffmpeg: bool,
    }

    impl CapabilitySources for MockSources {
        fn llm_caps(&self) -> LlmCaps {
            self.llm.clone()
        }
        fn gateway_caps(&self) -> GatewayCaps {
            self.gateway.clone()
        }
        fn lobby_caps(&self) -> LobbyCaps {
            self.lobby.clone()
        }
        fn apps_installed(&self) -> Vec<String> {
            self.apps.clone()
        }
        fn ffmpeg_available(&self) -> bool {
            self.ffmpeg
        }
    }

    fn mock(inputs: CapInputs) -> CapabilitiesRouteHandler {
        CapabilitiesRouteHandler::with_sources(Box::new(MockSources {
            llm: inputs.llm,
            gateway: inputs.gateway,
            lobby: inputs.lobby,
            apps: inputs.apps,
            ffmpeg: inputs.media_ffmpeg_available,
        }))
    }

    fn get(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.to_string(),
            headers: serde_json::json!({}),
            body: serde_json::json!({}),
            auth: None,
        }
    }

    fn full_inputs() -> CapInputs {
        CapInputs {
            llm: LlmCaps {
                instances: 2,
                running: vec!["llm-5".into()],
            },
            gateway: GatewayCaps {
                channels: 3,
                enabled: 2,
                relay_channels: 1,
            },
            lobby: LobbyCaps {
                entries: 4,
                last_sync_at: Some("2026-09-04T04:00:00+00:00".into()),
                reachable: true,
            },
            p2p: P2pCaps {
                enabled: true,
                peers_connected: 3,
            },
            media_ffmpeg_available: true,
            apps: vec!["film".into()],
        }
    }

    #[tokio::test]
    async fn full_snapshot_shape_and_values() {
        let h = mock(full_inputs());
        let resp = h.handle(get(PATH_CAPABILITIES)).await.unwrap();
        assert_eq!(resp.status, 200);
        let b = resp.body;
        assert_eq!(b["sdk_version"], SDK_VERSION, "协议版本冻结面");
        assert!(b["generated_at"].as_str().is_some(), "generated_at 必出");
        assert_eq!(b["llm"]["instances"], 2);
        assert_eq!(b["llm"]["running"][0], "llm-5");
        assert_eq!(b["gateway"]["channels"], 3);
        assert_eq!(b["gateway"]["enabled"], 2);
        assert_eq!(b["gateway"]["relay_channels"], 1);
        assert_eq!(b["lobby"]["entries"], 4);
        assert_eq!(b["lobby"]["reachable"], true);
        assert_eq!(b["lobby"]["last_sync_at"], "2026-09-04T04:00:00+00:00");
        assert_eq!(b["media"]["ffmpeg_available"], true);
        // P2P 走 Handle（mock 无 Handle → disabled）；非零对端形态在
        // build_capabilities_value 纯函数测试覆盖。
        assert_eq!(b["p2p"]["enabled"], false);
        assert_eq!(b["p2p"]["peers_connected"], 0);
        assert_eq!(b["apps"][0], "film");
    }

    #[test]
    fn pure_build_p2p_enabled_shape() {
        let v = build_capabilities_value(full_inputs());
        assert_eq!(v["p2p"]["enabled"], true);
        assert_eq!(v["p2p"]["peers_connected"], 3);
        assert_eq!(v["media"]["ffmpeg_available"], true);
        assert_eq!(v["apps"], serde_json::json!(["film"]));
        // 线格式冻结：顶层键集合不增不减（SDK 按此解析）。
        let mut keys: Vec<&str> = v
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "apps",
                "gateway",
                "generated_at",
                "llm",
                "lobby",
                "media",
                "p2p",
                "sdk_version"
            ]
        );
    }

    #[tokio::test]
    async fn empty_node_is_honest_zeros_and_nulls() {
        // 空节点（P2P 未开 / 无实例 / 无渠道 / 无大厅条目 / 无应用）：
        // 全部如实 0 / false / null——不编造可达性。
        let h = mock(CapInputs {
            llm: LlmCaps {
                instances: 0,
                running: vec![],
            },
            gateway: GatewayCaps {
                channels: 0,
                enabled: 0,
                relay_channels: 0,
            },
            lobby: LobbyCaps {
                entries: 0,
                last_sync_at: None,
                reachable: false,
            },
            p2p: P2pCaps {
                enabled: false,
                peers_connected: 0,
            },
            media_ffmpeg_available: false,
            apps: vec![],
        });
        let b = h.handle(get(PATH_CAPABILITIES)).await.unwrap().body;
        assert_eq!(b["llm"]["running"], serde_json::json!([]));
        assert_eq!(b["lobby"]["last_sync_at"], serde_json::Value::Null);
        assert_eq!(b["lobby"]["reachable"], false);
        assert_eq!(b["p2p"]["enabled"], false);
        assert_eq!(b["apps"], serde_json::json!([]));
        assert_eq!(b["sdk_version"], "0.1");
    }

    #[tokio::test]
    async fn routes_declare_public_get() {
        let h = mock(full_inputs());
        let routes = h.routes().await;
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].path, PATH_CAPABILITIES);
        assert_eq!(routes[0].method, HttpMethod::Get);
        assert!(!routes[0].requires_auth, "读公开（应用 SDK 探测面）");
    }

    #[tokio::test]
    async fn unmatched_route_is_404() {
        let h = mock(full_inputs());
        let resp = h.handle(get("/api/v1/capabilities/extra")).await.unwrap();
        assert_eq!(resp.status, 404);
        let resp = h
            .handle(ApiRequest {
                method: HttpMethod::Post,
                path: PATH_CAPABILITIES.to_string(),
                headers: serde_json::json!({}),
                body: serde_json::json!({}),
                auth: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn query_string_does_not_break_matching() {
        let h = mock(full_inputs());
        let resp = h
            .handle(get("/api/v1/capabilities?verbose=1"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "query 参数不影响 dispatch（与全网关同规约）");
    }

    /// 端到端：组件注册 → build_router（axum）→ oneshot——真实 HTTP 栈 200。
    /// 数据源用真实 handler 的空态（with_empty / 临时目录），验证装配路径可用。
    #[tokio::test]
    async fn served_through_full_axum_router() {
        use tower::ServiceExt;

        let tmp = std::env::temp_dir().join(format!(
            "nexos-caps-e2e-{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&tmp);
        let db = tmp.join("market.db");
        let _ = std::fs::remove_file(&db);
        let market = Arc::new(ApiMarketRouteHandler::with_db_path(
            db.to_str().unwrap(),
            Arc::new(os_common::chain_auth::ChainAuth::new()),
        ));
        let apps = Arc::new(AppRegistry::with_paths(
            tmp.join("apps.db").to_str().unwrap(),
            tmp.join("apps-root").to_str().unwrap(),
            tmp.join("repos").to_str().unwrap(),
        ));
        let sources = RealSources::new(
            Arc::new(LlmRouteHandler::with_empty()),
            Arc::new(ApiGatewayRouteHandler::with_empty()),
            market,
            apps,
        );
        let h = CapabilitiesRouteHandler::new(sources, None);
        let gw = crate::InProcessGateway::new();
        crate::gateway::Gateway::register_component(&gw, "capabilities", Box::new(h))
            .await
            .expect("注册 capabilities");
        let router = crate::http::build_router(gw.make_state(None, None), None);
        let resp = router
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/v1/capabilities")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["sdk_version"], "0.1");
        assert!(v["llm"]["instances"].is_u64());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
