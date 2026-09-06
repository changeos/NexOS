//! `TransferRouteHandler` —— P2P 传输组件（component=transfer）的 REST 入口。
//!
//! 定位（2026-08-25 新增，用户定调「传输组件，类似迅雷或其他的 CDN，传输
//! 不能限于公网 ip，不要用公网 ip 做分发点」）：把 [`os_p2p::TransferService`]
//! （os-p2p transfer.rs——清单发布 / query-offer / 分块拉取引擎）适配为
//! `/api/v1/transfer/*`。分发走 **os-p2p 叠加层消息通道**（直连 / TCP 打洞 /
//! 中继信箱），NAT 后节点互传不依赖任何公网 IP——这是与 downloads（aria2
//! 公网 HTTP/BT）的分工边界：公网资源走 downloads，节点间网状分发走 transfer。
//! 架构与协议帧表见 docs/TRANSFER_COMPONENT.md。
//!
//! # 与 downloads.rs 的关系（红线：不改 downloads.rs）
//!
//! 独立组件独立路由：aria2 任务照旧在 `/api/v1/downloads/*`，P2P 任务在
//! `/api/v1/transfer/tasks`；前端 Downloads.vue 以 Tab 聚合展示两类任务
//! （来源徽章 🌐 HTTP/BT 与 🔗 P2P）。
//!
//! # 路由表
//!
//! | method | path | 鉴权 | 动作 |
//! |--------|------|------|------|
//! | POST   | `/api/v1/transfer/publish` | admin | `{path, name?}` 本地文件发布为可传输（生成清单），返回 transfer_id/sha256 |
//! | GET    | `/api/v1/transfer/manifests` | 公开 | 本机已发布清单（含本地路径） |
//! | DELETE | `/api/v1/transfer/manifests/:id` | admin | 下架（:id = transfer_id 或 sha256） |
//! | POST   | `/api/v1/transfer/fetch` | admin | `{sha256 或 transfer_id, name?}` 发起 P2P 拉取 → 任务 |
//! | GET    | `/api/v1/transfer/tasks` | 公开 | 任务列表（进度=块位图、速度、源节点短 ID） |
//! | GET    | `/api/v1/transfer/tasks/:id` | 公开 | 单任务详情 |
//! | POST   | `/api/v1/transfer/tasks/:id/pause` | admin | 暂停（保留进度） |
//! | POST   | `/api/v1/transfer/tasks/:id/resume` | admin | 继续（断点续传） |
//! | POST   | `/api/v1/transfer/tasks/:id/cancel` | admin | 取消（保留进度文件，重新 fetch 续传） |
//! | GET    | `/api/v1/transfer/stats` | 公开 | 统计（清单数/任务数/做种贡献） |
//!
//! # 启用条件
//!
//! 依赖 os-p2p 组网节点（`NEXOS_P2P_ENABLE=1` 时 main.rs spawn 后装配本
//! handler——`TransferService::spawn(handle, TransferConfig::from_env())`）。
//! 未启用时全部端点 **503** + 引导文案（同 p2p handler 语义）。
//! env：`NEXOS_TRANSFER_DIR`（落地目录，缺省 /tank/downloads）/
//! `NEXOS_TRANSFER_REGISTRY`（注册表持久化，缺省 `<dir>/.transfer-registry.json`）。

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};
use os_p2p::TransferService;

/// 本 handler 注册时的组件名（`RouteSpec::handler_component`）。
const COMPONENT: &str = "transfer";
/// 未启用统一文案（503 body 的 error 字段——前端凭此展示开启指引）。
const DISABLED_MSG: &str = "P2P 传输未启用（需 NEXOS_P2P_ENABLE=1 启动组网节点）";

/// P2P 传输路由处理器——HTTP 边界适配到 [`TransferService`]。
///
/// - `Some(service)`：main.rs 在 os-p2p spawn 成功后装配（`spawn_p2p_if_enabled`）；
/// - `None`：P2P 未启用——全部端点 503 + 引导文案（默认构造即此态）。
pub struct TransferRouteHandler {
    service: Option<Arc<TransferService>>,
}

impl TransferRouteHandler {
    /// 未启用构造（默认部署：`NEXOS_P2P_ENABLE` 未设/为 0）。
    #[must_use]
    pub fn new_disabled() -> Self {
        Self { service: None }
    }

    /// 已启用构造（main.rs 装配：p2p spawn 成功后传入共享服务实例）。
    #[must_use]
    pub fn new(service: Arc<TransferService>) -> Self {
        Self {
            service: Some(service),
        }
    }

    /// 是否已启用（--check 诊断 / 测试用）。
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.service.is_some()
    }

    /// 未启用统一 503 语义。
    fn disabled_response() -> ApiResponse {
        ApiResponse {
            status: 503,
            body: serde_json::json!({"error": DISABLED_MSG}),
            headers: serde_json::json!({}),
        }
    }
}

impl Default for TransferRouteHandler {
    fn default() -> Self {
        Self::new_disabled()
    }
}

#[async_trait]
impl RouteHandler for TransferRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec_admin(HttpMethod::Post, PATH_PUBLISH),
            spec_read(HttpMethod::Get, PATH_MANIFESTS),
            spec_admin(HttpMethod::Delete, "/api/v1/transfer/manifests/:id"),
            spec_admin(HttpMethod::Post, PATH_FETCH),
            spec_read(HttpMethod::Get, PATH_TASKS),
            spec_read(HttpMethod::Get, "/api/v1/transfer/tasks/:id"),
            spec_admin(HttpMethod::Post, "/api/v1/transfer/tasks/:id/pause"),
            spec_admin(HttpMethod::Post, "/api/v1/transfer/tasks/:id/resume"),
            spec_admin(HttpMethod::Post, "/api/v1/transfer/tasks/:id/cancel"),
            spec_read(HttpMethod::Get, PATH_STATS),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            // —— POST /api/v1/transfer/publish —— 发布本地文件为可传输
            (HttpMethod::Post, ["api", "v1", "transfer", "publish"]) => match &self.service {
                Some(s) => self.handle_publish(s, &req.body).await,
                None => Ok(Self::disabled_response()),
            },

            // —— GET /api/v1/transfer/manifests —— 本机已发布清单
            (HttpMethod::Get, ["api", "v1", "transfer", "manifests"]) => match &self.service {
                Some(s) => {
                    let list: Vec<serde_json::Value> = s
                        .manifests()
                        .into_iter()
                        .map(|e| {
                            serde_json::json!({
                                "manifest": e.manifest,
                                "path": e.path.display().to_string(),
                            })
                        })
                        .collect();
                    Ok(ok_json(serde_json::json!({ "manifests": list })))
                }
                None => Ok(Self::disabled_response()),
            },

            // —— DELETE /api/v1/transfer/manifests/:id —— 下架
            (HttpMethod::Delete, ["api", "v1", "transfer", "manifests", id]) => {
                match &self.service {
                    Some(s) => {
                        if s.unpublish(id) {
                            Ok(ok_json(serde_json::json!({"ok": true, "id": id})))
                        } else {
                            Ok(error_response(404, &format!("清单不存在: {id}")))
                        }
                    }
                    None => Ok(Self::disabled_response()),
                }
            }

            // —— POST /api/v1/transfer/fetch —— 发起 P2P 拉取
            (HttpMethod::Post, ["api", "v1", "transfer", "fetch"]) => match &self.service {
                Some(s) => self.handle_fetch(s, &req.body).await,
                None => Ok(Self::disabled_response()),
            },

            // —— GET /api/v1/transfer/tasks —— 任务列表
            (HttpMethod::Get, ["api", "v1", "transfer", "tasks"]) => match &self.service {
                Some(s) => Ok(ok_json(to_value(&s.tasks())?)),
                None => Ok(Self::disabled_response()),
            },

            // —— GET /api/v1/transfer/tasks/:id —— 单任务
            (HttpMethod::Get, ["api", "v1", "transfer", "tasks", id]) => match &self.service {
                Some(s) => match s.task(id) {
                    Some(v) => Ok(ok_json(to_value(&v)?)),
                    None => Ok(error_response(404, &format!("任务不存在: {id}"))),
                },
                None => Ok(Self::disabled_response()),
            },

            // —— POST /api/v1/transfer/tasks/:id/{pause|resume|cancel} —— 任务控制
            (HttpMethod::Post, ["api", "v1", "transfer", "tasks", id, action])
                if matches!(*action, "pause" | "resume" | "cancel") =>
            {
                match &self.service {
                    Some(s) => {
                        let ok = match *action {
                            "pause" => s.pause_task(id).await,
                            "resume" => s.resume_task(id).await,
                            "cancel" => s.cancel_task(id),
                            _ => unreachable!(),
                        };
                        if ok {
                            Ok(ok_json(serde_json::json!({
                                "ok": true, "id": id, "action": *action,
                            })))
                        } else {
                            Ok(error_response(
                                409,
                                &format!("{action} 失败：任务不存在或已处终态（{id}）"),
                            ))
                        }
                    }
                    None => Ok(Self::disabled_response()),
                }
            }

            // —— GET /api/v1/transfer/stats —— 统计
            (HttpMethod::Get, ["api", "v1", "transfer", "stats"]) => match &self.service {
                Some(s) => Ok(ok_json(to_value(&s.stats())?)),
                None => Ok(Self::disabled_response()),
            },

            // —— 未覆盖路由 —— 兜底 404
            _ => Ok(error_response(404, "transfer: 未匹配的路由")),
        }
    }
}

impl TransferRouteHandler {
    /// POST publish：`{path, name?}` → [`TransferService::publish`]（spawn_blocking
    /// 建清单）。返回 201 + `{transfer_id, sha256, name, size, chunks, chunk_size}`
    /// ——调用方把 sha256/transfer_id 发给其他节点用户即可在其节点 fetch。
    async fn handle_publish(
        &self,
        s: &Arc<TransferService>,
        body: &serde_json::Value,
    ) -> Result<ApiResponse, ApiGatewayError> {
        let Some(path) = body
            .get("path")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|p| !p.is_empty())
        else {
            return Ok(error_response(
                400,
                "body 需要 {path, name?}：path 缺失或为空",
            ));
        };
        if path.contains("..") {
            return Ok(error_response(400, "path 不允许包含 ..（防路径穿越）"));
        }
        let name = body
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|n| !n.is_empty());
        match s.publish(std::path::Path::new(path), name).await {
            Ok(m) => Ok(ApiResponse {
                status: 201,
                body: serde_json::json!({
                    "transfer_id": m.transfer_id,
                    "sha256": m.sha256,
                    "name": m.name,
                    "size": m.size,
                    "chunk_size": m.chunk_size,
                    "chunks": m.chunks.len(),
                    "mime": m.mime,
                    "published_at": m.published_at,
                    "note": "把 sha256 或 transfer_id 发给其他节点的用户即可在其节点 fetch",
                }),
                headers: serde_json::json!({}),
            }),
            Err(e) => Ok(error_response(
                400,
                &format!("发布失败（路径不可读 / 是目录 / 读盘中途变化？）: {e}"),
            )),
        }
    }

    /// POST fetch：`{sha256 | transfer_id, name?}` → [`TransferService::fetch`]
    /// （立即返回任务 ID，进度经 GET tasks 轮询）。返回 202 + 任务视图。
    async fn handle_fetch(
        &self,
        s: &Arc<TransferService>,
        body: &serde_json::Value,
    ) -> Result<ApiResponse, ApiGatewayError> {
        let key = ["sha256", "transfer_id"]
            .iter()
            .find_map(|k| {
                body.get(*k)
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
            })
            .unwrap_or_default();
        if key.is_empty() {
            return Ok(error_response(
                400,
                "body 需要 {sha256 | transfer_id, name?}：键缺失或为空",
            ));
        }
        // 键合法性粗校验：64 hex 或 tr_ 前缀（提前拒绝明显误粘贴，避免空转一轮 query）
        let valid = key.starts_with("tr_")
            || (key.len() == 64 && key.chars().all(|c| c.is_ascii_hexdigit()));
        if !valid {
            return Ok(error_response(
                400,
                "sha256 须为 64 位 hex，或 transfer_id 须为 tr_ 前缀",
            ));
        }
        let name = body
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|n| !n.is_empty());
        let id = s.fetch(key, name).await;
        let view = s
            .task(&id)
            .ok_or_else(|| ApiGatewayError::Internal("任务建后即失".into()))?;
        Ok(ApiResponse {
            status: 202,
            body: to_value(&view)?,
            headers: serde_json::json!({}),
        })
    }
}

// ----------------------------------------------------------------------------
// 内部辅助（与其它 handler 同款）
// ----------------------------------------------------------------------------

/// `POST /api/v1/transfer/publish`——发布本地文件为可传输（admin）。
const PATH_PUBLISH: &str = "/api/v1/transfer/publish";
/// `GET /api/v1/transfer/manifests`——本机已发布清单（公开）。
const PATH_MANIFESTS: &str = "/api/v1/transfer/manifests";
/// `POST /api/v1/transfer/fetch`——发起 P2P 拉取（admin）。
const PATH_FETCH: &str = "/api/v1/transfer/fetch";
/// `GET /api/v1/transfer/tasks`——任务列表（公开）。
const PATH_TASKS: &str = "/api/v1/transfer/tasks";
/// `GET /api/v1/transfer/stats`——统计（公开）。
const PATH_STATS: &str = "/api/v1/transfer/stats";

/// 构造一条只读路由规格（任务/清单/统计观察面不涉敏感数据）。
fn spec_read(method: HttpMethod, path: &str) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: COMPONENT.to_string(),
        requires_auth: false,
        required_roles: Vec::new(),
    }
}

/// 构造一条写路由规格（发布/下架/发起拉取/任务控制是改变状态的操作）。
fn spec_admin(method: HttpMethod, path: &str) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: COMPONENT.to_string(),
        requires_auth: true,
        required_roles: vec!["admin".to_string()],
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

fn to_value<T: serde::Serialize + ?Sized>(v: &T) -> Result<serde_json::Value, ApiGatewayError> {
    serde_json::to_value(v).map_err(|e| ApiGatewayError::Internal(format!("响应序列化失败: {e}")))
}

fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use os_p2p::{P2pConfig, P2pNode, Timing, TransferConfig};

    /// 测试组网节点（随机端口 / 测试节奏 / 关 mDNS）。
    fn spawn_test_node() -> os_p2p::Handle {
        P2pNode::spawn(P2pConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            timings: Timing::testing(),
            mdns_enabled: false,
            ..P2pConfig::default()
        })
        .expect("随机端口绑定必成功")
    }

    /// 测试服务配置（隔离临时目录）。
    fn test_cfg(tag: &str) -> TransferConfig {
        TransferConfig {
            dest_dir: std::env::temp_dir().join(format!(
                "osapi-transfer-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
            )),
            registry_file: None,
            chunk_size: 16 * 1024,
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

    fn delete_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Delete,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    // ---- 1. 路由声明：10 端点、component=transfer、鉴权矩阵 ----

    #[tokio::test]
    async fn routes_declare_ten_endpoints_with_auth_matrix() {
        let h = TransferRouteHandler::new_disabled();
        let routes = h.routes().await;
        assert_eq!(routes.len(), 10);
        assert!(routes.iter().all(|r| r.handler_component == "transfer"));
        for r in &routes {
            match r.method {
                HttpMethod::Post | HttpMethod::Delete => {
                    assert!(r.requires_auth, "写路由须鉴权: {r:?}");
                    assert_eq!(r.required_roles, vec!["admin".to_string()]);
                }
                _ => assert!(!r.requires_auth, "读路由公开: {r:?}"),
            }
        }
    }

    // ---- 2. 未启用：全部端点 503 + 引导文案 ----

    #[tokio::test]
    async fn disabled_returns_503_with_guidance() {
        let h = TransferRouteHandler::new_disabled();
        assert!(!h.is_enabled());
        for req in [
            get_req("/api/v1/transfer/tasks"),
            get_req("/api/v1/transfer/manifests"),
            get_req("/api/v1/transfer/stats"),
            post_req(
                "/api/v1/transfer/publish",
                serde_json::json!({"path": "/x"}),
            ),
            post_req(
                "/api/v1/transfer/fetch",
                serde_json::json!({"sha256": "a".repeat(64)}),
            ),
            post_req("/api/v1/transfer/tasks/task-1/pause", serde_json::json!({})),
            delete_req("/api/v1/transfer/manifests/tr_x"),
        ] {
            let resp = h.handle(req).await.unwrap();
            assert_eq!(resp.status, 503, "未启用一律 503");
            assert!(resp.body["error"]
                .as_str()
                .unwrap()
                .contains("NEXOS_P2P_ENABLE"));
        }
        // Default 同未启用
        assert!(!TransferRouteHandler::default().is_enabled());
    }

    // ---- 3. publish 校验：缺 path / .. 穿越 / 目录 → 400 ----

    #[tokio::test]
    async fn publish_validates_request_body() {
        let node = spawn_test_node();
        let s = TransferService::spawn(node.clone(), test_cfg("pub"));
        let h = TransferRouteHandler::new(s);
        // 缺 path
        let r = h
            .handle(post_req("/api/v1/transfer/publish", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(r.status, 400, "缺 path");
        // 路径穿越
        let r = h
            .handle(post_req(
                "/api/v1/transfer/publish",
                serde_json::json!({"path": "../../etc/passwd"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 400, "拒 .. 穿越");
        // 目录
        let r = h
            .handle(post_req(
                "/api/v1/transfer/publish",
                serde_json::json!({"path": "/tmp"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 400, "目录不可发布");
        node.shutdown().await;
    }

    // ---- 4. publish → manifests → DELETE 下架（REST 全链）----

    #[tokio::test]
    async fn publish_manifests_unpublish_roundtrip() {
        let node = spawn_test_node();
        let cfg = test_cfg("round");
        std::fs::create_dir_all(&cfg.dest_dir).unwrap();
        let f = cfg.dest_dir.join("rest-api.bin");
        std::fs::write(&f, vec![7u8; 4096]).unwrap();
        let s = TransferService::spawn(node.clone(), cfg);
        let h = TransferRouteHandler::new(s);
        // 发布 → 201 + transfer_id/sha256
        let r = h
            .handle(post_req(
                "/api/v1/transfer/publish",
                serde_json::json!({"path": f.display().to_string()}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201, "发布成功: {r:?}");
        let sha = r.body["sha256"].as_str().unwrap().to_string();
        let tid = r.body["transfer_id"].as_str().unwrap().to_string();
        assert!(tid.starts_with("tr_"));
        assert_eq!(sha.len(), 64);
        // manifests → 1 条
        let r = h
            .handle(get_req("/api/v1/transfer/manifests"))
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body["manifests"].as_array().unwrap().len(), 1);
        assert_eq!(r.body["manifests"][0]["path"], f.display().to_string());
        // DELETE 按 transfer_id 下架
        let r = h
            .handle(delete_req(&format!("/api/v1/transfer/manifests/{tid}")))
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        let r = h
            .handle(get_req("/api/v1/transfer/manifests"))
            .await
            .unwrap();
        assert_eq!(r.body["manifests"].as_array().unwrap().len(), 0);
        // 重复下架 → 404
        let r = h
            .handle(delete_req(&format!("/api/v1/transfer/manifests/{tid}")))
            .await
            .unwrap();
        assert_eq!(r.status, 404);
        node.shutdown().await;
    }

    // ---- 5. fetch 校验：缺键 / 非法键 → 400 ----

    #[tokio::test]
    async fn fetch_validates_key_format() {
        let node = spawn_test_node();
        let s = TransferService::spawn(node.clone(), test_cfg("fetch"));
        let h = TransferRouteHandler::new(s);
        // 缺键
        let r = h
            .handle(post_req("/api/v1/transfer/fetch", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(r.status, 400, "缺 sha256/transfer_id");
        // 非法键（不是 64 hex 也不是 tr_）
        let z64 = "z".repeat(64);
        for bad in ["short", z64.as_str(), "tx_1234"] {
            let r = h
                .handle(post_req(
                    "/api/v1/transfer/fetch",
                    serde_json::json!({"sha256": bad}),
                ))
                .await
                .unwrap();
            assert_eq!(r.status, 400, "非法键: {bad}");
        }
        node.shutdown().await;
    }

    // ---- 6. fetch → tasks 轮询（REST 视图字段；单节点无源 → 任务在册后失败）----

    #[tokio::test]
    async fn fetch_creates_task_visible_in_tasks_list() {
        let node = spawn_test_node();
        let s = TransferService::spawn(node.clone(), test_cfg("tasks"));
        let h = TransferRouteHandler::new(s);
        let r = h
            .handle(post_req(
                "/api/v1/transfer/fetch",
                serde_json::json!({"transfer_id": "tr_abcd1234abcd1234", "name": "远端文件.iso"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 202, "fetch 受理即 202: {r:?}");
        let task_id = r.body["id"].as_str().unwrap().to_string();
        assert!(task_id.starts_with("task-"));
        // 列表 + 单查均可见（观察面字段齐备）
        let r = h.handle(get_req("/api/v1/transfer/tasks")).await.unwrap();
        assert_eq!(r.status, 200);
        let arr = r.body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "远端文件.iso");
        assert_eq!(arr[0]["phase"], "querying");
        let r = h
            .handle(get_req(&format!("/api/v1/transfer/tasks/{task_id}")))
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        // 未知任务 404
        let r = h
            .handle(get_req("/api/v1/transfer/tasks/task-999"))
            .await
            .unwrap();
        assert_eq!(r.status, 404);
        // stats 可见
        let r = h.handle(get_req("/api/v1/transfer/stats")).await.unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body["tasks"], 1);
        node.shutdown().await;
    }

    // ---- 7. 双节点 REST 端到端：A 经 REST 发布 → B 经 REST fetch → 完成 ----

    #[tokio::test]
    async fn rest_end_to_end_two_nodes() {
        let a = spawn_test_node();
        let b = spawn_test_node();
        b.dial(a.listen_addr()).await.expect("B 拨 A");
        let cfg_a = test_cfg("rest-a");
        let cfg_b = test_cfg("rest-b");
        std::fs::create_dir_all(&cfg_a.dest_dir).unwrap();
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let f = cfg_a.dest_dir.join("iso-image.bin");
        std::fs::write(&f, &data).unwrap();
        let ha = TransferRouteHandler::new(TransferService::spawn(a.clone(), cfg_a));
        let sb = TransferService::spawn(b.clone(), cfg_b);
        let hb = TransferRouteHandler::new(sb.clone());
        // A 发布
        let r = ha
            .handle(post_req(
                "/api/v1/transfer/publish",
                serde_json::json!({"path": f.display().to_string(), "name": "ubuntu-24.04.iso"}),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 201);
        let sha = r.body["sha256"].as_str().unwrap().to_string();
        // B 拉取（B 无已连 peer 也能建任务——引擎查询窗口内失败，REST 层不受阻）
        let r = hb
            .handle(post_req(
                "/api/v1/transfer/fetch",
                serde_json::json!({ "sha256": sha }),
            ))
            .await
            .unwrap();
        assert_eq!(r.status, 202, "B fetch 受理: {r:?}");
        let task_id = r.body["id"].as_str().unwrap().to_string();
        // 轮询至完成（真实 overlay query/offer + 分块拉取 + 校验）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let view = loop {
            let r = hb
                .handle(get_req(&format!("/api/v1/transfer/tasks/{task_id}")))
                .await
                .unwrap();
            let phase = r.body["phase"].as_str().unwrap_or("");
            if phase == "completed" || phase == "failed" || std::time::Instant::now() > deadline {
                break r;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        };
        assert_eq!(view.body["phase"], "completed", "B 应完成拉取: {view:?}");
        assert_eq!(view.body["progress"], 100);
        // 落地字节一致（B 的落地目录）
        let dest = view.body["dest_path"].as_str().unwrap();
        assert_eq!(std::fs::read(dest).unwrap(), data, "REST 链路字节级一致");
        // pause/resume/cancel 对终态任务 → 409
        for action in ["pause", "resume", "cancel"] {
            let r = hb
                .handle(post_req(
                    &format!("/api/v1/transfer/tasks/{task_id}/{action}"),
                    serde_json::json!({}),
                ))
                .await
                .unwrap();
            assert_eq!(r.status, 409, "终态任务 {action} 应 409");
        }
        a.shutdown().await;
        b.shutdown().await;
    }

    // ---- 8. 兜底 404 ----

    #[tokio::test]
    async fn unmatched_route_returns_404() {
        let h = TransferRouteHandler::new_disabled();
        let r = h.handle(get_req("/api/v1/transfer/nothing")).await.unwrap();
        assert_eq!(r.status, 404);
    }
}
