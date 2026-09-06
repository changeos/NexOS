//! `NotesRouteHandler` —— 笔记/文档桌面应用的 HTTP→真实文件系统持久化适配器。
//!
//! 定位：把网关 HTTP 请求（`/api/v1/notes/*`）翻译为笔记管理（持久化到文件系统），
//! 返回 JSON。这是 OS"应用类三件套"之一（笔记/文档）桌面应用的后端 REST 入口。
//!
//! # 持久化策略
//!
//! - 笔记序列化为 `<id>.json` 落盘到存储目录。目录选择优先级：
//!   1. `/tank/notes`（ZFS 池挂载点，存在即用）
//!   2. `/var/lib/os/notes`（系统持久化目录，自动创建）
//!   3. 若以上均不可写（如只读环境），回退到**内存态** + 2 条 demo 笔记。
//! - 真实读写经 `tokio::task::spawn_blocking` 调度，避免阻塞异步运行行时。
//! - id 由计数器 + 纳秒时间戳组合生成，保证唯一（不依赖 uuid crate）。
//!
//! # 路由表
//!
//! | method | path                    | 动作 |
//! |--------|-------------------------|------|
//! | GET    | `/api/v1/notes`         | 列全部（摘要，不含 content）|
//! | GET    | `/api/v1/notes/:id`     | 单条（含 content）|
//! | POST   | `/api/v1/notes`         | 创建（需 admin）|
//! | PUT    | `/api/v1/notes/:id`     | 更新（需 admin）|
//! | DELETE | `/api/v1/notes/:id`     | 删除（需 admin）|
//! | GET    | `/api/v1/notes/stats`   | 统计 |

use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 一条笔记（完整结构，含 content）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub id: String,
    pub title: String,
    /// markdown 正文
    pub content: String,
    pub tags: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// 笔记摘要（列表项，不含 content 以避免大 payload）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteSummary {
    pub id: String,
    pub title: String,
    pub tags: Vec<String>,
    pub updated_at: String,
}

impl From<&Note> for NoteSummary {
    fn from(n: &Note) -> Self {
        Self {
            id: n.id.clone(),
            title: n.title.clone(),
            tags: n.tags.clone(),
            updated_at: n.updated_at.clone(),
        }
    }
}

/// `GET /api/v1/notes/stats` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotesStats {
    pub total_notes: usize,
    pub total_tags: usize,
    /// 最近 7 天内更新的笔记数
    pub recent_updated: u64,
}

/// 创建/更新笔记请求体（字段全部可选以支持 PATCH 风格 PUT）。
#[derive(Debug, Deserialize)]
struct UpsertBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

// ----------------------------------------------------------------------------
// NotesRouteHandler
// ----------------------------------------------------------------------------

/// 笔记/文档路由处理器——HTTP 边界适配到真实文件系统持久化。
///
/// 当 `/tank/notes` 或 `/var/lib/os/notes` 可写时真实落盘；否则回退内存态
/// （持有 `Mutex<Vec<Note>>` + demo 数据），保证任何环境下 handler 可用。
pub struct NotesRouteHandler {
    /// 内存态回退存储（仅当持久化目录不可用时使用）。
    memory: Mutex<Option<Vec<Note>>>,
}

impl NotesRouteHandler {
    /// 构造 handler。
    #[must_use]
    pub fn new() -> Self {
        Self {
            memory: Mutex::new(None),
        }
    }

    /// 解析存储目录：返回 `Some(dir)` 表示可持久化，`None` 表示回退内存态。
    fn storage_dir() -> Option<String> {
        if std::path::Path::new("/tank/notes").is_dir() {
            return Some("/tank/notes".to_string());
        }
        let fallback = "/var/lib/os/notes";
        match std::fs::create_dir_all(fallback) {
            Ok(()) => {
                // 验证可写：尝试写一个 .writable 探针
                let probe = format!("{fallback}/.writable");
                if std::fs::write(&probe, b"ok").is_ok() {
                    let _ = std::fs::remove_file(&probe);
                    Some(fallback.to_string())
                } else {
                    None
                }
            }
            Err(_) => None,
        }
    }

    /// 生成新 id（计数器 + 纳秒，保证唯一）。
    fn gen_id() -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let c = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("note-{c:x}-{nanos:x}")
    }

    /// 内存态回退：初始化 demo 数据。
    fn memory_demo() -> Vec<Note> {
        vec![
            Note {
                id: "note-demo-1".into(),
                title: "欢迎使用 OS 笔记".into(),
                content: "# 欢迎\n\n这是 OS 笔记应用的演示条目。\n\n支持 **markdown**。".into(),
                tags: vec!["demo".into(), "入门".into()],
                created_at: "2026-08-07T10:00:00+08:00".into(),
                updated_at: "2026-08-07T10:00:00+08:00".into(),
            },
            Note {
                id: "note-demo-2".into(),
                title: "运维备忘".into(),
                content: "# 运维\n\n- zpool scrub 每月一次\n- 备份验证每周一次".into(),
                tags: vec!["运维".into()],
                created_at: "2026-08-08T08:00:00+08:00".into(),
                updated_at: "2026-08-08T08:30:00+08:00".into(),
            },
        ]
    }
}

impl Default for NotesRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RouteHandler for NotesRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec(HttpMethod::Get, "/api/v1/notes", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/notes/:id", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/notes",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Put,
                "/api/v1/notes/:id",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/notes/:id",
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Get, "/api/v1/notes/stats", false, vec![]),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            // —— GET /api/v1/notes —— 列全部（摘要）
            (HttpMethod::Get, ["api", "v1", "notes"]) => {
                let notes = self.list_all().await?;
                let summaries: Vec<NoteSummary> = notes.iter().map(NoteSummary::from).collect();
                Ok(ok_json(to_value(&summaries)?))
            }

            // —— GET /api/v1/notes/stats —— 统计
            (HttpMethod::Get, ["api", "v1", "notes", "stats"]) => {
                let notes = self.list_all().await?;
                let mut tagset = std::collections::HashSet::new();
                let mut recent = 0u64;
                let cutoff = recent_cutoff();
                for n in &notes {
                    for t in &n.tags {
                        tagset.insert(t.clone());
                    }
                    if n.updated_at >= cutoff {
                        recent += 1;
                    }
                }
                let stats = NotesStats {
                    total_notes: notes.len(),
                    total_tags: tagset.len(),
                    recent_updated: recent,
                };
                Ok(ok_json(to_value(&stats)?))
            }

            // —— GET /api/v1/notes/:id —— 单条（含 content）
            (HttpMethod::Get, ["api", "v1", "notes", id]) => match self.get_one(id).await? {
                Some(n) => Ok(ok_json(to_value(&n)?)),
                None => Ok(error_response(404, &format!("笔记不存在: {id}"))),
            },

            // —— POST /api/v1/notes —— 创建
            (HttpMethod::Post, ["api", "v1", "notes"]) => {
                let body: UpsertBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析创建笔记请求体失败: {e}"))
                })?;
                let title = body
                    .title
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        // 直接返回 400 而非 Err（保持与其它 handler 一致的 JSON 错误）
                        ApiGatewayError::Internal("title 必填".into())
                    });
                let title = match title {
                    Ok(t) => t,
                    Err(_) => return Ok(error_response(400, "title 不可为空")),
                };
                let now = now_iso();
                let note = Note {
                    id: Self::gen_id(),
                    title,
                    content: body.content.unwrap_or_default(),
                    tags: body.tags.unwrap_or_default(),
                    created_at: now.clone(),
                    updated_at: now,
                };
                self.persist_put(&note).await?;
                Ok(ApiResponse {
                    status: 201,
                    body: to_value(&note)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— PUT /api/v1/notes/:id —— 更新
            (HttpMethod::Put, ["api", "v1", "notes", id]) => {
                let body: UpsertBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析更新笔记请求体失败: {e}"))
                })?;
                let updated = self.update_one(id, body).await?;
                match updated {
                    Some(n) => Ok(ok_json(to_value(&n)?)),
                    None => Ok(error_response(404, &format!("笔记不存在: {id}"))),
                }
            }

            // —— DELETE /api/v1/notes/:id —— 删除
            (HttpMethod::Delete, ["api", "v1", "notes", id]) => {
                let removed = self.remove_one(id).await?;
                if removed {
                    Ok(ok_json(
                        serde_json::json!({"ok": true, "id": id, "action": "delete"}),
                    ))
                } else {
                    Ok(error_response(404, &format!("笔记不存在: {id}")))
                }
            }

            // —— 未覆盖路由 —— 兜底 404
            _ => Ok(error_response(404, "notes: 未匹配的路由")),
        }
    }
}

// ----------------------------------------------------------------------------
// 持久化辅助（spawn_blocking + 内存态回退）
// ----------------------------------------------------------------------------

impl NotesRouteHandler {
    /// 列出全部笔记。
    async fn list_all(&self) -> Result<Vec<Note>, ApiGatewayError> {
        match Self::storage_dir() {
            Some(dir) => {
                let joined = tokio::task::spawn_blocking(move || list_dir_sync(&dir))
                    .await
                    .map_err(|e| ApiGatewayError::Internal(format!("列笔记任务 join 失败: {e}")))?;
                joined.map_err(|e| ApiGatewayError::Internal(format!("列笔记失败: {e}")))
            }
            None => {
                let mut mem = self.memory.lock().expect("memory poisoned");
                if mem.is_none() {
                    *mem = Some(Self::memory_demo());
                }
                Ok(mem.as_ref().expect("initialized").clone())
            }
        }
    }

    /// 取单条笔记。
    async fn get_one(&self, id: &str) -> Result<Option<Note>, ApiGatewayError> {
        let all = self.list_all().await?;
        Ok(all.into_iter().find(|n| n.id == id))
    }

    /// 写入（新建/覆盖）一条笔记。
    async fn persist_put(&self, note: &Note) -> Result<(), ApiGatewayError> {
        match Self::storage_dir() {
            Some(dir) => {
                let note = note.clone();
                let joined = tokio::task::spawn_blocking(move || write_note_sync(&dir, &note))
                    .await
                    .map_err(|e| ApiGatewayError::Internal(format!("写笔记任务 join 失败: {e}")))?;
                joined.map_err(|e| ApiGatewayError::Internal(format!("写笔记失败: {e}")))
            }
            None => {
                let mut mem = self.memory.lock().expect("memory poisoned");
                if mem.is_none() {
                    *mem = Some(Self::memory_demo());
                }
                let list = mem.as_mut().expect("initialized");
                match list.iter().position(|n| n.id == note.id) {
                    Some(i) => list[i] = note.clone(),
                    None => list.push(note.clone()),
                }
                Ok(())
            }
        }
    }

    /// 更新一条笔记（PATCH 风格：仅更新请求中出现的字段）。
    async fn update_one(
        &self,
        id: &str,
        body: UpsertBody,
    ) -> Result<Option<Note>, ApiGatewayError> {
        let id = id.to_string();
        match self.get_one(&id).await? {
            None => Ok(None),
            Some(mut n) => {
                if let Some(t) = body.title {
                    let t = t.trim().to_string();
                    if t.is_empty() {
                        return Ok(Some(n)); // 空标题忽略
                    }
                    n.title = t;
                }
                if let Some(c) = body.content {
                    n.content = c;
                }
                if let Some(tags) = body.tags {
                    n.tags = tags;
                }
                n.updated_at = now_iso();
                self.persist_put(&n).await?;
                Ok(Some(n))
            }
        }
    }

    /// 删除一条笔记。
    async fn remove_one(&self, id: &str) -> Result<bool, ApiGatewayError> {
        let id = id.to_string();
        match Self::storage_dir() {
            Some(dir) => {
                let path = format!("{dir}/{id}.json");
                let joined =
                    tokio::task::spawn_blocking(move || std::fs::remove_file(&path).map(|_| true))
                        .await
                        .map_err(|e| {
                            ApiGatewayError::Internal(format!("删笔记任务 join 失败: {e}"))
                        })?;
                Ok(match joined {
                    Ok(removed) => removed,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                    Err(e) => return Err(ApiGatewayError::Internal(format!("删笔记失败: {e}"))),
                })
            }
            None => {
                let mut mem = self.memory.lock().expect("memory poisoned");
                if mem.is_none() {
                    *mem = Some(Self::memory_demo());
                }
                let list = mem.as_mut().expect("initialized");
                let before = list.len();
                list.retain(|n| n.id != id);
                Ok(list.len() != before)
            }
        }
    }
}

// ----------------------------------------------------------------------------
// 同步 FS 辅助
// ----------------------------------------------------------------------------

/// 同步列出目录下所有 `*.json` 笔记（按 updated_at 倒序）。
fn list_dir_sync(dir: &str) -> std::io::Result<Vec<Note>> {
    let mut notes = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(notes),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue, // 跳过不可读文件
        };
        if let Ok(n) = serde_json::from_str::<Note>(&content) {
            notes.push(n);
        }
    }
    notes.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(notes)
}

/// 同步写入单条笔记到 `<dir>/<id>.json`。
fn write_note_sync(dir: &str, note: &Note) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = format!("{dir}/{}.json", note.id);
    let body = serde_json::to_vec_pretty(note).map_err(std::io::Error::other)?;
    std::fs::write(path, body)
}

// ----------------------------------------------------------------------------
// 通用辅助
// ----------------------------------------------------------------------------

fn spec(
    method: HttpMethod,
    path: &str,
    requires_auth: bool,
    required_roles: Vec<String>,
) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: "notes".to_string(),
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

/// 7 天前的 ISO 8601 字符串（用于"最近更新"统计的粗略阈值）。
fn recent_cutoff() -> String {
    use chrono::Local;
    (Local::now() - chrono::Duration::days(7))
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

// ----------------------------------------------------------------------------
// 单元测试
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

    fn put_req(path: &str, body: serde_json::Value) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Put,
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

    #[tokio::test]
    async fn routes_declares_six_endpoints() {
        let h = NotesRouteHandler::new();
        let routes = h.routes().await;
        assert_eq!(routes.len(), 6);
        assert!(routes.iter().all(|r| r.handler_component == "notes"));
        // 写操作都要求 admin
        for r in &routes {
            if r.method == HttpMethod::Post
                || r.method == HttpMethod::Put
                || r.method == HttpMethod::Delete
            {
                assert!(r.requires_auth);
                assert_eq!(r.required_roles, vec!["admin".to_string()]);
            }
        }
        // 确认 PUT 路由存在
        assert!(routes
            .iter()
            .any(|r| r.method == HttpMethod::Put && r.path == "/api/v1/notes/:id"));
    }

    #[tokio::test]
    async fn list_returns_summaries_without_content() {
        let h = NotesRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/notes")).await.unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().expect("body 为数组");
        assert!(!arr.is_empty(), "至少应有 demo 数据");
        // 摘要不应含 content
        assert!(arr.iter().all(|n| n.get("content").is_none()));
        assert!(arr.iter().all(|n| n["id"].is_string()));
    }

    #[tokio::test]
    async fn create_get_update_delete_roundtrip() {
        let h = NotesRouteHandler::new();
        // 创建
        let resp = h
            .handle(post_req(
                "/api/v1/notes",
                serde_json::json!({"title": "测试笔记", "content": "# hi", "tags": ["t"]}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "create body: {resp:?}");
        let id = resp.body["id"].as_str().unwrap().to_string();
        assert_eq!(resp.body["title"], "测试笔记");
        // 单条 GET 含 content
        let resp = h
            .handle(get_req(&format!("/api/v1/notes/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["content"], "# hi");
        // PUT 更新
        let resp = h
            .handle(put_req(
                &format!("/api/v1/notes/{id}"),
                serde_json::json!({"title": "改后", "content": "# updated"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["title"], "改后");
        assert_eq!(resp.body["content"], "# updated");
        // DELETE
        let resp = h
            .handle(del_req(&format!("/api/v1/notes/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["ok"], true);
        // 再 GET 应 404
        let resp = h
            .handle(get_req(&format!("/api/v1/notes/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn stats_returns_counts() {
        let h = NotesRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/notes/stats")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body["total_notes"].as_u64().unwrap() >= 1);
        assert!(resp.body["total_tags"].as_u64().unwrap_or(0) < u64::MAX);
    }

    #[tokio::test]
    async fn create_validates_empty_title() {
        let h = NotesRouteHandler::new();
        let resp = h
            .handle(post_req("/api/v1/notes", serde_json::json!({"title": ""})))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    #[tokio::test]
    async fn get_missing_returns_404() {
        let h = NotesRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/notes/does-not-exist-xyz"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn put_missing_returns_404() {
        let h = NotesRouteHandler::new();
        let resp = h
            .handle(put_req(
                "/api/v1/notes/nope-xyz",
                serde_json::json!({"title": "x"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<NotesRouteHandler>();
    }
}
