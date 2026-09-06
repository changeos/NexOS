//! `llm_external` —— 模型管理「接入外部 API」子模块（`llm` 组件内挂载）。
//!
//! 场景：本机（如 113 节点）的模型管理里，把**别的节点/服务商**提供的
//! OpenAI 兼容端点（如 106 节点网关的 qwen3.5-9b：`http://192.0.2.106:8000/v1`）
//! 登记为可对话/可测试的模型来源——不本地拉 vLLM，纯转发。
//!
//! # 与 API 网关渠道（channels）的边界
//!
//! 网关渠道（`/api/v1/gateway/channels`，见 docs/EXTERNAL_LLM_CHANNELS.md）面向
//! 「**我要卖**我的模型」：多渠道优先级/加权/故障转移 + sk-os- 令牌计费。本表
//! `llm_external_apis` 面向「**我要用**别家的模型」：模型管理页里直接登记一个
//! base_url + key，连通测试拿真实模型清单，直通对话。两套表不耦合；将来若要把
//! 某 external API 升格为网关渠道（对外卖），可做单向导入（生成一条 channel），
//! 本期不做（详见 docs/LLM_EXTERNAL_APIS.md）。
//!
//! # 数据表（llm.db 同库，llm.rs `create_schema` 幂等建表）
//!
//! `llm_external_apis(id, name, base_url, api_key, models, status, last_check_at,
//! notes, created_at, via_node)`——`models` 列存 JSON 数组字符串；DB 路径链沿用
//! llm.rs（env `NEXOS_LLM_DB` → `/tank/os-data/llm.db` → `/var/lib/os/llm.db` →
//! `./llm.db`）。**首次开库即空表**（真实数据铁律：不 seed 演示条目）；
//! `status` 只有 `unknown`（新建未测）/ `ok`（连通测试成功）/ `error`（最近
//! 一次测试失败）三态，全部由真实探测翻转。`via_node`（2026-09-02 跨网中继）
//! = 联邦大厅导入条目的来源 NodeID——非空时 chat/test 经 os-p2p overlay 定向
//! 源节点代发（见下「via_node 中继」），空 = 直连语义不变。
//!
//! # 端点契约（6 条；GET 公开读，写/test/chat 需 admin）
//!
//! | method | path | 动作 |
//! |--------|------|------|
//! | GET    | `/api/v1/llm/external-apis` | 列表（api_key 脱敏，绝不出明文）|
//! | POST   | `/api/v1/llm/external-apis` | 创建（base_url 须非空 http(s)，admin）|
//! | PUT    | `/api/v1/llm/external-apis/:id` | 编辑（部分更新：未提供字段保留原值，admin）|
//! | DELETE | `/api/v1/llm/external-apis/:id` | 删除（admin）|
//! | POST   | `/api/v1/llm/external-apis/:id/test` | 连通测试：真实 GET `<base_url>/models`，返回 `{ok, models, latency_ms, error?}`（admin）|
//! | POST   | `/api/v1/llm/external-apis/:id/chat` | 对话直通：转发 `<base_url>/chat/completions`（admin；`stream:true` 由 http.rs 特挂路由做 SSE 逐块透传，非流式走本模块整包转发）|
//!
//! # 流式实现（http.rs `build_router` 特挂，api_gateway SSE 同款手法）
//!
//! `POST /:id/chat` 的 `stream:true` 请求由 [`chat_stream_handler`]（axum 特挂
//! 路由）处理：鉴权（复用 `extract_principal` + `AuthMiddleware::authorize`，
//! 与 dispatch 同口径）→ 查行 → reqwest `bytes_stream()` 逐块透传
//! `text/event-stream`。**首字节前**失败（连接失败/非 2xx/首块超时 120s）回
//! JSON 错误；**首字节后**不再回退，上游中断只断流并末尾补一条 `: llm-ext:`
//! SSE 注释帧。usage 块原样透传（不解析不估算）。
//!
//! # 真实数据铁律
//!
//! - test 端点**真实 GET** 上游 `/models`（带 `Authorization: Bearer`），模型
//!   清单从响应 `data[].id` 解析，延迟为真实计时——上游不可达就返回
//!   `ok:false + error`，绝不编造模型名。
//! - chat 非流式把上游响应**原样透传**（含 usage），不重写不估算 token。
//! - 测试不连外网：本地 `TcpListener` mock 真 TCP（llm.rs
//!   `spawn_fake_v1_models_server` 手法）。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use once_cell::sync::Lazy;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteSpec};

/// 进程级共享 `reqwest::Client`（与 llm.rs 同款：默认 30s 兜底，调用处覆盖）。
static HTTP: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("构建 llm-external reqwest Client 失败")
});

/// 连通测试（GET /models）超时。
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

/// 非流式对话整包超时（生成整段回复可比连接慢得多，给足余量）。
const CHAT_TIMEOUT: Duration = Duration::from_secs(120);

/// 流式请求超时上限（长对话兜底；首字节另有独立 120s 窗口）。
const CHAT_STREAM_TIMEOUT: Duration = Duration::from_secs(600);

/// 流式首字节超时（思考模型 TTFT 可达数十秒，与网关 SSE 同款口径）。
const STREAM_FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(120);

/// 流式请求体大小上限（网关 SSE 特挂 64MB 的同款防御，对话体远用不到）。
const STREAM_MAX_BODY: usize = 4 * 1024 * 1024;

/// api_key 脱敏保留长度（前 4 + 后 4，中间 ***）。
const KEY_MASK_KEEP: usize = 4;

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 一条外部 API 登记（DB 行结构；`api_key` 只在服务端内存/库中出现，
/// 对外响应一律经 [`ExternalApi::masked`] 脱敏）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalApi {
    pub id: String,
    /// 显示名（用户起，如「106 网关 qwen3.5-9b」）。
    pub name: String,
    /// OpenAI 兼容根地址（含 /v1，如 `http://192.0.2.106:8000/v1`）。
    pub base_url: String,
    /// 上游 API key（空串 = 无鉴权端点）。
    #[serde(default)]
    pub api_key: String,
    /// 可用模型 id 列表（可留空，由 test 回填）。
    #[serde(default)]
    pub models: Vec<String>,
    /// `unknown` / `ok` / `error`（最近一次真实连通测试结果）。
    pub status: String,
    /// 最近一次连通测试时间（ISO 本地时区）。
    #[serde(default)]
    pub last_check_at: Option<String>,
    /// 备注（可选）。
    #[serde(default)]
    pub notes: Option<String>,
    /// 来源 NodeID（`0x`+66hex，2026-09-02 跨网中继）：联邦大厅一键导入时
    /// 写入 api_market 条目的 `source_node_id`。非空 → chat/test 经 os-p2p
    /// overlay 定向该源节点代发（源端白名单裁决）；空 → 直连语义不变。
    /// 存量行空 = 直连。
    #[serde(default)]
    pub via_node: String,
    pub created_at: String,
}

impl ExternalApi {
    /// 对外安全形态：去掉明文 key，只给脱敏串与是否有 key。
    #[must_use]
    pub fn masked(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "name": self.name,
            "base_url": self.base_url,
            "api_key_masked": mask_key(&self.api_key),
            "has_api_key": !self.api_key.is_empty(),
            "models": self.models,
            "status": self.status,
            "last_check_at": self.last_check_at,
            "notes": self.notes,
            "via_node": self.via_node,
            "created_at": self.created_at,
        })
    }
}

/// 脱敏：空串→空串；短 key（≤2*KEEP）→ `***`；长 key → 前 4 + `***` + 后 4。
fn mask_key(key: &str) -> String {
    if key.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= KEY_MASK_KEEP * 2 {
        return "***".into();
    }
    let head: String = chars[..KEY_MASK_KEEP].iter().collect();
    let tail: String = chars[chars.len() - KEY_MASK_KEEP..].iter().collect();
    format!("{head}***{tail}")
}

/// base_url 校验：非空且 http/https（其余 scheme 一律 400）。
fn valid_base_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// 拼上游端点 URL：`<base_url 去尾斜杠>/<suffix>`。
fn join_url(base_url: &str, suffix: &str) -> String {
    format!("{}/{}", base_url.trim_end_matches('/'), suffix)
}

/// 带 key 时的 Authorization 头（无 key 不带头）。
fn auth_bearer(key: &str) -> Option<String> {
    if key.is_empty() {
        None
    } else {
        Some(format!("Bearer {key}"))
    }
}

fn now_iso() -> String {
    use chrono::Local;
    Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

// ----------------------------------------------------------------------------
// 持久化（llm.db 同库）
// ----------------------------------------------------------------------------

/// 建表（幂等；由 llm.rs `create_schema` 与本模块构造时同连接调用）+
/// 老库迁移补列（2026-09-02 `via_node`——CREATE IF NOT EXISTS 不补列，
/// llm.rs `ALTER ... ADD COLUMN` 忽略 duplicate 的幂等惯例）。
pub fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS llm_external_apis (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            base_url TEXT NOT NULL,
            api_key TEXT NOT NULL DEFAULT '',
            models TEXT NOT NULL DEFAULT '[]',
            status TEXT NOT NULL DEFAULT 'unknown',
            last_check_at TEXT,
            notes TEXT,
            created_at TEXT,
            via_node TEXT NOT NULL DEFAULT ''
        );",
    )?;
    let _ = conn.execute(
        "ALTER TABLE llm_external_apis ADD COLUMN via_node TEXT NOT NULL DEFAULT ''",
        [],
    );
    Ok(())
}

fn persist_row(conn: &Connection, a: &ExternalApi) -> rusqlite::Result<()> {
    let models_json = serde_json::to_string(&a.models).unwrap_or_else(|_| "[]".into());
    conn.execute(
        "INSERT OR REPLACE INTO llm_external_apis
         (id,name,base_url,api_key,models,status,last_check_at,notes,created_at,via_node)
         VALUES (?,?,?,?,?,?,?,?,?,?)",
        params![
            a.id,
            a.name,
            a.base_url,
            a.api_key,
            models_json,
            a.status,
            a.last_check_at,
            a.notes,
            a.created_at,
            a.via_node,
        ],
    )?;
    Ok(())
}

fn delete_row(conn: &Connection, id: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM llm_external_apis WHERE id=?", params![id])?;
    Ok(())
}

fn row_to_api(row: &rusqlite::Row) -> rusqlite::Result<ExternalApi> {
    let models_json: String = row.get(4)?;
    Ok(ExternalApi {
        id: row.get(0)?,
        name: row.get(1)?,
        base_url: row.get(2)?,
        api_key: row.get(3)?,
        models: serde_json::from_str(&models_json).unwrap_or_default(),
        status: row.get(5)?,
        last_check_at: row.get(6)?,
        notes: row.get(7)?,
        created_at: row.get::<_, Option<String>>(8)?.unwrap_or_default(),
        // 空串/缺列 → 空（直连语义；存量行兼容）。
        via_node: row
            .get::<_, Option<String>>(9)?
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_default(),
    })
}

const SELECT_COLS: &str =
    "id,name,base_url,api_key,models,status,last_check_at,notes,created_at,via_node";

fn load_all(conn: &Connection) -> rusqlite::Result<Vec<ExternalApi>> {
    let sql = format!(
        "SELECT {SELECT_COLS} FROM llm_external_apis ORDER BY created_at, id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let iter = stmt.query_map([], row_to_api)?;
    let mut out = Vec::new();
    for r in iter {
        out.push(r?);
    }
    Ok(out)
}

fn get_row(conn: &Connection, id: &str) -> Option<ExternalApi> {
    let sql = format!("SELECT {SELECT_COLS} FROM llm_external_apis WHERE id=?");
    let mut stmt = conn.prepare(&sql).ok()?;
    stmt.query_row(params![id], row_to_api).ok()
}

// ----------------------------------------------------------------------------
// 状态（与 llm.rs 共享同一 Mutex<Connection>）
// ----------------------------------------------------------------------------

/// 外部 API 登记表状态：持 llm.db 连接 Arc（与 LlmRouteHandler 的 db 同源，
/// 建/查/改走同一条连接，无跨连接竞态）+ 可注入的 overlay 中继端点（via_node
/// 条目的执行通道——main.rs 在 api_market 联邦端点建好后 `set_relay` 注入；
/// 测试注 fake 互连端点，照 live.rs 的 Executor 注入模式）。
pub struct LlmExternalState {
    db: Arc<Mutex<Connection>>,
    /// id 自增计数器（构造时越过表内最大数字后缀）。
    counter: Mutex<u64>,
    /// overlay 中继端点（None = 未装配——via_node 条目报「中继失败：通道未装配」）。
    relay: Mutex<Option<crate::handlers::api_market::ApiMarketFedEndpoint>>,
}

impl LlmExternalState {
    /// 用既有连接构造（生产入口：llm.rs 各构造器传入自己的 db Arc）。
    /// 建表幂等 + 计数器越过存量最大 `xapi-<N>` 后缀（新 id 不与恢复行相撞）。
    #[must_use]
    pub fn new(db: Arc<Mutex<Connection>>) -> Self {
        let counter = db
            .lock()
            .ok()
            .and_then(|conn| {
                let _ = create_schema(&conn);
                load_all(&conn).ok()
            })
            .map(|rows| {
                rows.iter()
                    .filter_map(|a| a.id.strip_prefix("xapi-"))
                    .filter_map(|s| s.parse::<u64>().ok())
                    .max()
                    .unwrap_or(100)
            })
            .unwrap_or(100);
        Self {
            db,
            counter: Mutex::new(counter),
            relay: Mutex::new(None),
        }
    }

    /// 内存库构造（测试注入：零行，建表幂等）。
    #[must_use]
    pub fn with_memory() -> Self {
        let conn = Connection::open_in_memory().expect("内存库必成功");
        create_schema(&conn).expect("建表必成功");
        Self::new(Arc::new(Mutex::new(conn)))
    }

    /// 注入 overlay 中继端点（main.rs 装配：api_market_fed 在 Box 进网关前
    /// 取出后传入——与 CRUD 同一生存期；Clone 共享内核，p2p spawn 后 set_p2p
    /// 即生效）。测试注入 fake 互连端点（`set_full_transport` 互投）。
    pub fn set_relay(&self, relay: Option<crate::handlers::api_market::ApiMarketFedEndpoint>) {
        *self.relay.lock().expect("llm-external relay poisoned") = relay;
    }

    /// 中继端点快照（via_node 执行分支取用）。
    fn relay_endpoint(&self) -> Option<crate::handlers::api_market::ApiMarketFedEndpoint> {
        self.relay
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    fn next_id(&self) -> String {
        let mut c = self.counter.lock().expect("llm-external counter poisoned");
        *c += 1;
        format!("xapi-{}", *c)
    }

    /// 全量列表（按创建时间）。
    #[must_use]
    pub fn list(&self) -> Vec<ExternalApi> {
        self.db
            .lock()
            .ok()
            .and_then(|conn| load_all(&conn).ok())
            .unwrap_or_default()
    }

    /// 按 id 取行（对话/测试转发用，需要明文 key——只在服务端内）。
    #[must_use]
    pub fn get(&self, id: &str) -> Option<ExternalApi> {
        self.db.lock().ok().and_then(|conn| get_row(&conn, id))
    }

    /// 落表（INSERT OR REPLACE，写失败吞错——只影响重启恢复，不影响当次请求）。
    fn persist(&self, api: &ExternalApi) {
        if let Ok(conn) = self.db.lock() {
            let _ = persist_row(&conn, api);
        }
    }

    /// 删行（同上容错）。
    fn remove(&self, id: &str) {
        if let Ok(conn) = self.db.lock() {
            let _ = delete_row(&conn, id);
        }
    }
}

// ----------------------------------------------------------------------------
// REST 端点（路由 specs + 处理；由 llm.rs 的 routes()/handle() 挂载）
// ----------------------------------------------------------------------------

/// 外部 API 路由 specs（handler_component=llm；GET 公开读，写/test/chat admin）。
#[must_use]
pub fn route_specs() -> Vec<RouteSpec> {
    fn spec(method: HttpMethod, path: &str, requires_auth: bool) -> RouteSpec {
        RouteSpec {
            method,
            path: path.to_string(),
            handler_component: "llm".to_string(),
            requires_auth,
            required_roles: if requires_auth {
                vec!["admin".into()]
            } else {
                vec![]
            },
        }
    }
    vec![
        spec(HttpMethod::Get, "/api/v1/llm/external-apis", false),
        spec(HttpMethod::Post, "/api/v1/llm/external-apis", true),
        spec(HttpMethod::Put, "/api/v1/llm/external-apis/:id", true),
        spec(HttpMethod::Delete, "/api/v1/llm/external-apis/:id", true),
        spec(
            HttpMethod::Post,
            "/api/v1/llm/external-apis/:id/test",
            true,
        ),
        spec(
            HttpMethod::Post,
            "/api/v1/llm/external-apis/:id/chat",
            true,
        ),
    ]
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

/// 处理 `/api/v1/llm/external-apis*`（`segs` 为 `external-apis` 之后的段）。
///
/// 注：`POST /:id/chat` 的流式分支不走这里（http.rs 特挂
/// [`chat_stream_handler`] 逐块透传）；只有非流式（或特挂未装配时的回落）
/// 到达本函数——转发时**强制 `stream:false`**，防止上游按 SSE 推流而整包
/// 等待超时。
pub async fn handle(
    state: &LlmExternalState,
    method: HttpMethod,
    segs: &[&str],
    body: serde_json::Value,
) -> Result<ApiResponse, ApiGatewayError> {
    match (method, segs) {
        // —— GET /external-apis —— 列表（公开读；key 脱敏）
        (HttpMethod::Get, []) => Ok(ok_json(serde_json::json!({
            "apis": state.list().iter().map(ExternalApi::masked).collect::<Vec<_>>(),
        }))),

        // —— POST /external-apis —— 创建（admin）
        (HttpMethod::Post, []) => {
            let body: CreateBody = serde_json::from_value(body).map_err(|e| {
                ApiGatewayError::Internal(format!("解析创建外部 API 请求体失败: {e}"))
            })?;
            let name = body.name.trim().to_string();
            if name.is_empty() {
                return Ok(error_response(400, "name 不可为空"));
            }
            let base_url = body.base_url.trim().trim_end_matches('/').to_string();
            if base_url.is_empty() {
                return Ok(error_response(400, "base_url 不可为空"));
            }
            if !valid_base_url(&base_url) {
                return Ok(error_response(
                    400,
                    "base_url 必须以 http:// 或 https:// 开头",
                ));
            }
            let models = body
                .models
                .unwrap_or_default()
                .into_iter()
                .map(|m| m.trim().to_string())
                .filter(|m| !m.is_empty())
                .collect::<Vec<_>>();
            // via_node（联邦大厅一键导入写入）：非空须是合法 NodeID（0x+66 hex）
            // ——错误值在此挡下，不留到执行层才炸。
            let via_node = body
                .via_node
                .unwrap_or_default()
                .trim()
                .to_string();
            if !via_node.is_empty() && os_p2p::NodeId::parse(&via_node).is_none() {
                return Ok(error_response(
                    400,
                    "via_node 非法（应为 0x+66 hex NodeID——联邦大厅导入时自动写入）",
                ));
            }
            let api = ExternalApi {
                id: state.next_id(),
                name,
                base_url,
                api_key: body.api_key.unwrap_or_default().trim().to_string(),
                models,
                status: "unknown".into(),
                last_check_at: None,
                notes: body
                    .notes
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                via_node,
                created_at: now_iso(),
            };
            state.persist(&api);
            eprintln!("[llm-ext] 登记外部 API {}（{}）", api.id, api.base_url);
            Ok(ApiResponse {
                status: 201,
                body: api.masked(),
                headers: serde_json::json!({}),
            })
        }

        // —— PUT /external-apis/:id —— 编辑（部分更新：未提供字段保留原值）
        (HttpMethod::Put, [id]) => {
            let body: UpdateBody = serde_json::from_value(body).map_err(|e| {
                ApiGatewayError::Internal(format!("解析编辑外部 API 请求体失败: {e}"))
            })?;
            let Some(mut api) = state.get(id) else {
                return Ok(error_response(404, &format!("外部 API 不存在: {id}")));
            };
            // 校验同 POST：name 非空、base_url 非空 http(s)（仅对提供了的字段生效）
            if let Some(name) = &body.name {
                let name = name.trim().to_string();
                if name.is_empty() {
                    return Ok(error_response(400, "name 不可为空"));
                }
                api.name = name;
            }
            if let Some(base_url) = &body.base_url {
                let base_url = base_url.trim().trim_end_matches('/').to_string();
                if base_url.is_empty() {
                    return Ok(error_response(400, "base_url 不可为空"));
                }
                if !valid_base_url(&base_url) {
                    return Ok(error_response(
                        400,
                        "base_url 必须以 http:// 或 https:// 开头",
                    ));
                }
                api.base_url = base_url;
            }
            // api_key 提供则覆盖（空串 = 清除鉴权；未提供 = 保留原 key）
            if let Some(key) = &body.api_key {
                api.api_key = key.trim().to_string();
            }
            if let Some(models) = body.models {
                api.models = models
                    .into_iter()
                    .map(|m| m.trim().to_string())
                    .filter(|m| !m.is_empty())
                    .collect();
            }
            if let Some(notes) = body.notes {
                let notes = notes.trim().to_string();
                api.notes = (!notes.is_empty()).then_some(notes);
            }
            // via_node 提供即覆盖（空串 = 清除——回直连语义）；未提供保留原值。
            if let Some(via) = body.via_node {
                let via = via.trim().to_string();
                if !via.is_empty() && os_p2p::NodeId::parse(&via).is_none() {
                    return Ok(error_response(
                        400,
                        "via_node 非法（应为 0x+66 hex NodeID——联邦大厅导入时自动写入）",
                    ));
                }
                api.via_node = via;
            }
            state.persist(&api);
            eprintln!("[llm-ext] 编辑外部 API {id}（{}）", api.base_url);
            Ok(ok_json(api.masked()))
        }

        // —— DELETE /external-apis/:id —— 删行（admin）
        (HttpMethod::Delete, [id]) => {
            if state.get(id).is_none() {
                return Ok(error_response(404, &format!("外部 API 不存在: {id}")));
            }
            state.remove(id);
            eprintln!("[llm-ext] 删除外部 API {id}");
            Ok(ok_json(
                serde_json::json!({"ok": true, "id": id, "action": "delete"}),
            ))
        }

        // —— POST /external-apis/:id/test —— 连通测试（真实 GET /models；
        //    via_node 非空经 overlay 中继源节点代发，空则直连）
        (HttpMethod::Post, [id, "test"]) => {
            let Some(api) = state.get(id) else {
                return Ok(error_response(404, &format!("外部 API 不存在: {id}")));
            };
            let outcome = if api.via_node.is_empty() {
                test_connectivity(&api).await
            } else {
                relay_test_connectivity(state, &api).await
            };
            // 真实结果落行：status/last_check_at 翻转；models 空时回填上游清单
            let mut updated = api.clone();
            updated.status = if outcome.ok { "ok" } else { "error" }.into();
            updated.last_check_at = Some(now_iso());
            if outcome.ok && updated.models.is_empty() {
                updated.models = outcome.models.clone();
            }
            state.persist(&updated);
            eprintln!(
                "[llm-ext] 连通测试 {id} → {}（{}，{}ms，{} 模型）",
                if outcome.ok { "ok" } else { "error" },
                api.base_url,
                outcome.latency_ms,
                outcome.models.len()
            );
            Ok(ok_json(serde_json::json!({
                "id": id,
                "ok": outcome.ok,
                "models": outcome.models,
                "latency_ms": outcome.latency_ms,
                "error": outcome.error,
            })))
        }

        // —— POST /external-apis/:id/chat —— 对话直通（非流式整包转发）
        (HttpMethod::Post, [id, "chat"]) => {
            let chat_body: ChatBody =
                serde_json::from_value(body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析对话请求体失败: {e}"))
                })?;
            let model = chat_body.model.trim().to_string();
            if model.is_empty() {
                return Ok(error_response(400, "model 不可为空"));
            }
            if chat_body.messages.is_empty() {
                return Ok(error_response(400, "messages 不可为空"));
            }
            if chat_body
                .messages
                .iter()
                .any(|m| m.role.trim().is_empty() || m.content.trim().is_empty())
            {
                return Ok(error_response(400, "每条 message 的 role/content 不可为空"));
            }
            let Some(api) = state.get(id) else {
                return Ok(error_response(404, &format!("外部 API 不存在: {id}")));
            };
            let forwarded = if api.via_node.is_empty() {
                chat_forward_once(&api, &chat_body).await
            } else {
                relay_chat_forward_once(state, &api, &chat_body).await
            };
            match forwarded {
                Ok(upstream) => Ok(ok_json(upstream)),
                Err((status, e)) => {
                    eprintln!("[llm-ext] 对话转发失败（{}）: {e}", api.base_url);
                    Ok(error_response(status, &e))
                }
            }
        }

        _ => Ok(error_response(404, "llm-ext: 未匹配的路由")),
    }
}

/// `POST /api/v1/llm/external-apis` 请求体。
#[derive(Debug, Deserialize)]
struct CreateBody {
    name: String,
    base_url: String,
    #[serde(default)]
    api_key: Option<String>,
    /// 初始模型清单（可留空，由 test 回填）。
    #[serde(default)]
    models: Option<Vec<String>>,
    #[serde(default)]
    notes: Option<String>,
    /// 来源 NodeID（联邦大厅一键导入自动写入；非空走 overlay 中继）。
    #[serde(default)]
    via_node: Option<String>,
}

/// `PUT /api/v1/llm/external-apis/:id` 请求体（部分更新语义：
/// 全字段可选——`None`（未提供）保留原值；`api_key`/`via_node` 提供即覆盖
/// （空串=清除））。
#[derive(Debug, Deserialize)]
struct UpdateBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    models: Option<Vec<String>>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    via_node: Option<String>,
}

/// `POST /:id/chat` 请求体（OpenAI 兼容子集；`stream` 仅特挂路由消费）。
/// `model`/`messages` 缺省容忍（空值走 400 而不是解析 500）。
#[derive(Debug, Deserialize)]
struct ChatBody {
    #[serde(default)]
    model: String,
    #[serde(default)]
    messages: Vec<ChatMessage>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    temperature: Option<f32>,
    /// 组件内非流式路径忽略该字段（强制 false 转发）；true 走 http.rs 特挂。
    /// 字段本身仅为"接受该入参并显式忽略"的文档化占位。
    #[serde(default)]
    #[allow(dead_code)]
    stream: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatMessage {
    role: String,
    content: String,
}

/// 连通测试结果（真实探测产物，零编造）。
struct TestOutcome {
    ok: bool,
    models: Vec<String>,
    latency_ms: u64,
    error: Option<String>,
}

/// 真实 GET `<base_url>/models`（带鉴权头，10s 超时），解析 `data[].id`。
async fn test_connectivity(api: &ExternalApi) -> TestOutcome {
    let url = join_url(&api.base_url, "models");
    let started = std::time::Instant::now();
    let mut req = HTTP.get(&url).timeout(TEST_TIMEOUT);
    if let Some(auth) = auth_bearer(&api.api_key) {
        req = req.header("Authorization", auth);
    }
    match req.send().await {
        Ok(resp) => {
            let latency_ms = started.elapsed().as_millis() as u64;
            let status = resp.status();
            if !status.is_success() {
                return TestOutcome {
                    ok: false,
                    models: vec![],
                    latency_ms,
                    error: Some(format!("HTTP {}", status.as_u16())),
                };
            }
            match resp.json::<serde_json::Value>().await {
                Ok(v) => TestOutcome {
                    ok: true,
                    models: parse_model_ids(&v),
                    latency_ms,
                    error: None,
                },
                Err(e) => TestOutcome {
                    ok: false,
                    models: vec![],
                    latency_ms,
                    error: Some(format!("响应非 JSON: {e}")),
                },
            }
        }
        Err(e) => TestOutcome {
            ok: false,
            models: vec![],
            latency_ms: started.elapsed().as_millis() as u64,
            error: Some(format!("连接失败: {e}")),
        },
    }
}

/// 从 OpenAI 兼容 `/models` 响应解析模型 id 列表（`data[].id`；兼容裸数组）。
pub fn parse_model_ids(v: &serde_json::Value) -> Vec<String> {
    let arr = v
        .get("data")
        .and_then(|d| d.as_array())
        .or_else(|| v.as_array());
    arr.map(|a| {
        a.iter()
            .filter_map(|m| m.get("id").and_then(|i| i.as_str()))
            .map(String::from)
            .collect()
    })
    .unwrap_or_default()
}

/// 非流式对话整包转发：POST `<base_url>/chat/completions`（强制 stream:false），
/// 2xx 时上游 JSON **原样透传**（含 usage），失败映射 502。
async fn chat_forward_once(
    api: &ExternalApi,
    body: &ChatBody,
) -> Result<serde_json::Value, (u16, String)> {
    let url = join_url(&api.base_url, "chat/completions");
    let payload = serde_json::json!({
        "model": body.model,
        "messages": body.messages,
        "max_tokens": body.max_tokens.unwrap_or(1024),
        "temperature": body.temperature.unwrap_or(0.7),
        "stream": false,
    });
    let mut req = HTTP.post(&url).timeout(CHAT_TIMEOUT);
    if let Some(auth) = auth_bearer(&api.api_key) {
        req = req.header("Authorization", auth);
    }
    let resp = req
        .json(&payload)
        .send()
        .await
        .map_err(|e| (502u16, format!("上游请求失败: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        let detail = resp.text().await.unwrap_or_default();
        let detail: String = detail.chars().take(300).collect();
        return Err((
            502,
            format!("上游返回 HTTP {}: {}", status.as_u16(), detail),
        ));
    }
    resp.json::<serde_json::Value>()
        .await
        .map_err(|e| (502, format!("上游响应非 JSON: {e}")))
}

// ----------------------------------------------------------------------------
// via_node 中继执行（overlay 定向源节点代发——与直连可区分的错误前缀）
// ----------------------------------------------------------------------------

/// 中继失败错误（502 + 「经 <节点短式> 中继失败：<原因>」——与直连失败文案
/// 明确可区分，前端透传展示）。
fn relay_fail(api: &ExternalApi, reason: String) -> (u16, String) {
    (
        502u16,
        format!(
            "经 {} 中继失败: {reason}",
            crate::handlers::api_market::short_node_label(&api.via_node)
        ),
    )
}

/// 组装一次中继请求（鉴权头与直连同规则——Bearer，无 key 不带）。
fn relay_request_of(
    api: &ExternalApi,
    method: &str,
    url: String,
    body: Option<Vec<u8>>,
    stream: bool,
) -> crate::handlers::api_market::ApiRelayRequest {
    let headers = auth_bearer(&api.api_key)
        .map(|auth| vec![("Authorization".to_string(), auth)])
        .unwrap_or_default();
    crate::handlers::api_market::ApiRelayRequest {
        method: method.to_string(),
        url,
        headers,
        body,
        stream,
    }
}

/// 中继连通测试（via_node 非空路径）：经源节点 GET `<base_url>/models`，
/// 语义/产物与直连 [`test_connectivity`] 同构（真实延迟/真实清单/错误翻转
/// error 态——零编造）。
async fn relay_test_connectivity(
    state: &LlmExternalState,
    api: &ExternalApi,
) -> TestOutcome {
    let started = std::time::Instant::now();
    let outcome = async {
        let Some(ep) = state.relay_endpoint() else {
            return Err("P2P 通道未装配（NEXOS_P2P_ENABLE=1 且对端组网后可用）".to_string());
        };
        let req = relay_request_of(api, "GET", join_url(&api.base_url, "models"), None, false);
        ep.relay_roundtrip(&api.via_node, req, TEST_TIMEOUT).await
    }
    .await;
    let latency_ms = started.elapsed().as_millis() as u64;
    match outcome {
        Ok(done) => {
            if !(200..300).contains(&done.status) {
                return TestOutcome {
                    ok: false,
                    models: vec![],
                    latency_ms,
                    error: Some(format!("HTTP {}", done.status)),
                };
            }
            match serde_json::from_slice::<serde_json::Value>(&done.body) {
                Ok(v) => TestOutcome {
                    ok: true,
                    models: parse_model_ids(&v),
                    latency_ms,
                    error: None,
                },
                Err(e) => TestOutcome {
                    ok: false,
                    models: vec![],
                    latency_ms,
                    error: Some(format!("响应非 JSON: {e}")),
                },
            }
        }
        Err(e) => TestOutcome {
            ok: false,
            models: vec![],
            latency_ms,
            error: Some(format!(
                "经 {} 中继失败: {e}",
                crate::handlers::api_market::short_node_label(&api.via_node)
            )),
        },
    }
}

/// 中继非流式对话（via_node 非空路径）：经源节点 POST
/// `<base_url>/chat/completions`（强制 stream:false，与直连同规则），上游
/// JSON 原样透传（含 usage）。
async fn relay_chat_forward_once(
    state: &LlmExternalState,
    api: &ExternalApi,
    body: &ChatBody,
) -> Result<serde_json::Value, (u16, String)> {
    let payload = serde_json::json!({
        "model": body.model,
        "messages": body.messages,
        "max_tokens": body.max_tokens.unwrap_or(1024),
        "temperature": body.temperature.unwrap_or(0.7),
        "stream": false,
    });
    let Some(ep) = state.relay_endpoint() else {
        return Err(relay_fail(
            api,
            "P2P 通道未装配（NEXOS_P2P_ENABLE=1 且对端组网后可用）".into(),
        ));
    };
    let req = relay_request_of(
        api,
        "POST",
        join_url(&api.base_url, "chat/completions"),
        Some(serde_json::to_vec(&payload).map_err(|e| (500u16, format!("请求体序列化失败: {e}")))?),
        false,
    );
    let done = ep
        .relay_roundtrip(&api.via_node, req, CHAT_TIMEOUT)
        .await
        .map_err(|e| relay_fail(api, e))?;
    if !(200..300).contains(&done.status) {
        let detail: String = String::from_utf8_lossy(&done.body).chars().take(300).collect();
        return Err((
            502,
            format!(
                "经 {} 中继上游返回 HTTP {}: {detail}",
                crate::handlers::api_market::short_node_label(&api.via_node),
                done.status
            ),
        ));
    }
    serde_json::from_slice(&done.body)
        .map_err(|e| relay_fail(api, format!("上游响应非 JSON: {e}")))
}

/// 中继流式对话（via_node 非空路径）：经源节点流式拉取（源端 SSE 逐块透传
/// 成 api_relay_resp 帧），本地重组回 axum 响应流——`sse_passthrough_stream`
/// 的上游中断注释帧语义照旧。首块窗口沿用直连口径（120s——思考模型 TTFT
/// 可达数十秒；中继协议缺省 15s 是 pending 清理口径，不放大到本层）。
async fn relay_chat_stream_response(
    state: &LlmExternalState,
    api: &ExternalApi,
    body_json: &serde_json::Value,
    model: &str,
) -> axum::response::Response {
    let node_label = crate::handlers::api_market::short_node_label(&api.via_node);
    let Some(ep) = state.relay_endpoint() else {
        return json_error_response(
            502,
            &format!("经 {node_label} 中继失败: P2P 通道未装配（NEXOS_P2P_ENABLE=1 且对端组网后可用）"),
        );
    };
    // 请求体原样透传（model 覆盖为非空 trim 值——与直连同规则）。
    let mut fwd = body_json.clone();
    if let serde_json::Value::Object(ref mut map) = fwd {
        map.insert("model".into(), serde_json::Value::String(model.to_string()));
    }
    let payload = match serde_json::to_vec(&fwd) {
        Ok(b) => b,
        Err(e) => return json_error_response(500, &format!("请求体序列化失败: {e}")),
    };
    let req = relay_request_of(
        api,
        "POST",
        join_url(&api.base_url, "chat/completions"),
        Some(payload),
        true,
    );
    let mut stream = match ep
        .relay_open_stream(&api.via_node, req, STREAM_FIRST_BYTE_TIMEOUT)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[llm-ext] 流式中继失败（{}）: {e}", api.base_url);
            return json_error_response(502, &format!("经 {node_label} 中继失败: {e}"));
        }
    };
    // 上游非 2xx：源端把错误体单帧收尾——聚合读完回 JSON 错误（未对客户端吐字节）。
    if !(200..300).contains(&stream.status) {
        let code = stream.status;
        let mut detail = Vec::new();
        while let Some(chunk) = stream.next_chunk().await {
            match chunk {
                Ok(b) => detail.extend_from_slice(&b),
                Err(e) => {
                    detail.extend_from_slice(e.as_bytes());
                    break;
                }
            }
        }
        let detail: String = String::from_utf8_lossy(&detail).chars().take(300).collect();
        eprintln!("[llm-ext] 流式中继上游错误 HTTP {code} {detail}");
        return json_error_response(
            502,
            &format!("经 {node_label} 中继上游返回 HTTP {code} {detail}"),
        );
    }
    let content_type = stream
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| "text/event-stream".to_string());
    // 首数据块（Head 帧不带数据；首块窗口与直连同口径 120s）。
    let first = match tokio::time::timeout(STREAM_FIRST_BYTE_TIMEOUT, stream.next_chunk()).await {
        Ok(Some(Ok(b))) => Bytes::from(b),
        Ok(Some(Err(e))) => {
            return json_error_response(502, &format!("经 {node_label} 中继上游流建立失败: {e}"))
        }
        Ok(None) => {
            return json_error_response(502, "中继上游流在首字节前结束（空响应体）")
        }
        Err(_) => {
            return json_error_response(504, "中继上游首字节超时（120s 无数据）")
        }
    };
    eprintln!(
        "[llm-ext] 流式中继直通 {} → {}（model={model}，via {}）",
        api.id, api.base_url, node_label
    );
    let body_stream = sse_passthrough_stream(first, relay_chunk_stream(stream));
    axum::http::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, content_type)
        .header(axum::http::header::CACHE_CONTROL, "no-cache")
        .body(axum::body::Body::from_stream(body_stream))
        .unwrap_or_else(|_| json_error_response(500, "internal error"))
}

/// RelayStream → futures 流（空闲超时在 next_chunk 内部执行；Err 即断流，
/// 由 `sse_passthrough_stream` 的注释帧收尾语义兜底）。
fn relay_chunk_stream(
    rs: crate::handlers::api_market::RelayStream,
) -> impl futures::Stream<Item = Result<Bytes, String>> {
    futures::stream::unfold(Some(rs), |mut st| async move {
        let stream = st.as_mut()?;
        match stream.next_chunk().await {
            Some(Ok(b)) => Some((Ok(Bytes::from(b)), st)),
            Some(Err(e)) => {
                eprintln!("[llm-ext] 中继上游流中断: {e}");
                st = None; // 断流收尾（Err 项由 sse 注释帧语义收底后即结束）
                Some((Err(e), st))
            }
            None => None, // 正常收尾
        }
    })
}

// ----------------------------------------------------------------------------
// SSE 流式特挂（http.rs build_router 挂载；api_gateway SSE 同款手法）
// ----------------------------------------------------------------------------

/// `POST /api/v1/llm/external-apis/{id}/chat` 特挂 handler。
///
/// - `stream:true` → 真流式：鉴权 → 查行 → reqwest `bytes_stream()` 逐块透传 SSE；
/// - 其余（非流式 / body 非 JSON / 状态未装配）→ 原样重建请求交
///   [`crate::http::dispatch_handler`]（走组件整包路径，鉴权/行为与直接
///   POST 完全一致）。
///
/// 鉴权与 dispatch 同口径：`extract_principal` + `AuthMiddleware::authorize`
/// （requires_auth + admin，与路由表里该 spec 一致）——特挂路由不经 registry
/// 匹配，鉴权必须自带。
pub async fn chat_stream_handler(
    axum::extract::State(state): axum::extract::State<crate::http::GatewayState>,
    req: axum::extract::Request,
) -> axum::response::Response {
    use futures::StreamExt;

    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, STREAM_MAX_BODY).await {
        Ok(b) => b,
        Err(e) => return json_error_response(400, &format!("读取请求体失败: {e}")),
    };
    // stream 判定：仅认 JSON 顶层布尔 true（与网关 SSE 特挂同款口径）
    let parsed = serde_json::from_slice::<serde_json::Value>(&bytes).ok();
    let wants_stream = parsed
        .as_ref()
        .and_then(|v| v.get("stream"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let fallback = |state, parts, bytes| {
        crate::http::dispatch_handler(
            axum::extract::State(state),
            axum::extract::Request::from_parts(parts, axum::body::Body::from(bytes)),
        )
    };
    // 状态未装配（纯测试 Router 等）→ 回落整包路径（行为同未特挂）
    let Some(ext) = state.llm_external.clone() else {
        return fallback(state, parts, bytes).await;
    };
    let Some(body_json) = parsed.filter(|v| v.is_object()) else {
        return fallback(state, parts, bytes).await;
    };
    if !wants_stream {
        return fallback(state, parts, bytes).await;
    }

    // —— 鉴权（与 dispatch 的 3.5 步同口径：admin 写路由）——
    let mut headers_map = serde_json::Map::new();
    for (name, value) in &parts.headers {
        let key = name.as_str().to_lowercase();
        if let Ok(s) = value.to_str() {
            headers_map.insert(key, serde_json::Value::String(s.to_string()));
        }
    }
    let headers = serde_json::Value::Object(headers_map);
    let principal = crate::http::extract_principal(
        &headers,
        state.jwt.as_ref(),
        state.admin_token.as_ref(),
    )
    .await;
    let path = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| parts.uri.path().to_string());
    let api_req = ApiRequest {
        method: HttpMethod::Post,
        path,
        headers,
        body: body_json.clone(),
        auth: principal,
    };
    let route = RouteSpec {
        method: HttpMethod::Post,
        path: "/api/v1/llm/external-apis/:id/chat".into(),
        handler_component: "llm".into(),
        requires_auth: true,
        required_roles: vec!["admin".into()],
    };
    if let crate::middleware::MiddlewareDecision::Reject { status, body } =
        crate::middleware::AuthMiddleware::authorize(&api_req, &route)
    {
        return json_error_response(status, body["error"].as_str().unwrap_or("未授权"));
    }

    // —— 查行 + 参数校验（与组件路径同文案）——
    let id = parts
        .uri
        .path()
        .rsplit('/')
        .nth(1)
        .unwrap_or_default()
        .to_string();
    let Some(api) = ext.get(&id) else {
        return json_error_response(404, &format!("外部 API 不存在: {id}"));
    };
    let model = body_json
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();
    if model.is_empty() {
        return json_error_response(400, "model 不可为空");
    }
    let messages_empty = body_json
        .get("messages")
        .and_then(|m| m.as_array())
        .map(|a| a.is_empty())
        .unwrap_or(true);
    if messages_empty {
        return json_error_response(400, "messages 不可为空");
    }

    // —— via_node 分流：联邦导入条目（via_node 非空）经 overlay 中继源节点
    //    代发（不直连——跨网节点够不着发布者内网 endpoint）；空 = 直连现状。
    if !api.via_node.is_empty() {
        return relay_chat_stream_response(&ext, &api, &body_json, &model).await;
    }

    // —— 上游请求（请求体原样透传，model 覆盖为非空 trim 值）——
    let url = join_url(&api.base_url, "chat/completions");
    let mut fwd = body_json.clone();
    if let serde_json::Value::Object(ref mut map) = fwd {
        map.insert("model".into(), serde_json::Value::String(model.clone()));
    }
    let mut req = HTTP.post(&url).timeout(CHAT_STREAM_TIMEOUT);
    if let Some(auth) = auth_bearer(&api.api_key) {
        req = req.header("Authorization", auth);
    }
    let resp = match req.json(&fwd).send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[llm-ext] 流式转发失败（{}）: {e}", api.base_url);
            return json_error_response(502, &format!("上游请求失败: {e}"));
        }
    };
    if !resp.status().is_success() {
        let code = resp.status().as_u16();
        let detail = match tokio::time::timeout(Duration::from_secs(5), resp.text()).await {
            Ok(Ok(t)) => t,
            _ => String::new(),
        };
        let detail: String = detail.chars().take(300).collect();
        eprintln!("[llm-ext] 流式上游错误 HTTP {code} {detail}");
        return json_error_response(502, &format!("上游返回 HTTP {code} {detail}"));
    }
    let content_type = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("text/event-stream")
        .to_string();
    let mut stream = resp.bytes_stream();
    // 首字节前失败/超时 → 还能回 JSON 错误（未对客户端吐任何字节）
    let first = match tokio::time::timeout(STREAM_FIRST_BYTE_TIMEOUT, stream.next()).await {
        Ok(Some(Ok(b))) => b,
        Ok(Some(Err(e))) => {
            return json_error_response(502, &format!("上游流建立失败: {e}"))
        }
        Ok(None) => return json_error_response(502, "上游流在首字节前结束（空响应体）"),
        Err(_) => return json_error_response(504, "上游首字节超时（120s 无数据）"),
    };
    eprintln!("[llm-ext] 流式直通 {id} → {}（model={model}）", api.base_url);
    let body_stream = sse_passthrough_stream(first, stream);
    axum::http::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, content_type)
        .header(axum::http::header::CACHE_CONTROL, "no-cache")
        .body(axum::body::Body::from_stream(body_stream))
        .unwrap_or_else(|_| json_error_response(500, "internal error"))
}

/// 组装透传流：首块 + 上游余流。中途错误时末尾补一条 `: llm-ext:` SSE 注释帧
/// （SSE 规范忽略 `:` 开头的注释行，不污染客户端数据帧）后收尾断流——此时已
/// 对客户端开吐，无法回退。
fn sse_passthrough_stream<S, E>(
    first: Bytes,
    rest: S,
) -> impl futures::Stream<Item = Result<Bytes, E>>
where
    S: futures::Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::fmt::Display,
{
    use futures::StreamExt;

    struct St<E> {
        first: Option<Bytes>,
        inner: std::pin::Pin<Box<dyn futures::Stream<Item = Result<Bytes, E>> + Send>>,
        errored: bool,
    }
    let st = St {
        first: Some(first),
        inner: Box::pin(rest),
        errored: false,
    };
    futures::stream::unfold(st, |mut st| async move {
        if let Some(b) = st.first.take() {
            return Some((Ok(b), st));
        }
        if st.errored {
            return None;
        }
        match st.inner.next().await {
            Some(Ok(chunk)) => Some((Ok(chunk), st)),
            Some(Err(e)) => {
                eprintln!("[llm-ext] 上游流中断: {e}");
                st.errored = true;
                let comment = Bytes::from(format!(": llm-ext: upstream stream aborted ({e})\n\n"));
                Some((Ok(comment), st))
            }
            None => None,
        }
    })
}

/// JSON 错误响应（`{"error": ...}`，与 ApiResponse 错误形态同构）。
fn json_error_response(status: u16, msg: &str) -> axum::response::Response {
    let body = serde_json::json!({"error": msg}).to_string();
    axum::http::Response::builder()
        .status(
            axum::http::StatusCode::from_u16(status)
                .unwrap_or(axum::http::StatusCode::BAD_GATEWAY),
        )
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|_| axum::http::Response::new(axum::body::Body::from("internal error")))
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn segs_after(path: &str) -> Vec<&str> {
        path.split('?')
            .next()
            .unwrap_or(path)
            .split('/')
            .filter(|s| !s.is_empty())
            .skip(4) // api/v1/llm/external-apis
            .collect()
    }

    async fn call(state: &LlmExternalState, method: HttpMethod, path: &str, body: serde_json::Value) -> ApiResponse {
        handle(state, method, &segs_after(path), body)
            .await
            .unwrap()
    }

    async fn create(state: &LlmExternalState, name: &str, base_url: &str, key: &str) -> String {
        let resp = call(
            state,
            HttpMethod::Post,
            "/api/v1/llm/external-apis",
            serde_json::json!({"name": name, "base_url": base_url, "api_key": key}),
        )
        .await;
        assert_eq!(resp.status, 201, "create body: {resp:?}");
        resp.body["id"].as_str().unwrap().to_string()
    }

    // ---- mask_key / URL 拼接 / 解析 ----

    #[test]
    fn mask_key_variants() {
        assert_eq!(mask_key(""), "");
        assert_eq!(mask_key("sk"), "***");
        assert_eq!(mask_key("sk-1234"), "***");
        assert_eq!(mask_key("sk-abcdefgh"), "sk-a***efgh");
    }

    #[test]
    fn join_url_trims_trailing_slash() {
        assert_eq!(join_url("http://h:1/v1/", "models"), "http://h:1/v1/models");
        assert_eq!(
            join_url("http://h:1/v1", "chat/completions"),
            "http://h:1/v1/chat/completions"
        );
    }

    #[test]
    fn parse_model_ids_openai_shape_and_bare_array() {
        let v = serde_json::json!({"object":"list","data":[{"id":"a"},{"id":"b"}]});
        assert_eq!(parse_model_ids(&v), vec!["a", "b"]);
        let bare = serde_json::json!([{"id":"x"}]);
        assert_eq!(parse_model_ids(&bare), vec!["x"]);
        let junk = serde_json::json!({"foo": 1});
        assert!(parse_model_ids(&junk).is_empty());
    }

    // ---- CRUD + 脱敏 ----

    #[tokio::test]
    async fn crud_create_list_delete_and_key_masking() {
        let state = LlmExternalState::with_memory();
        let id = create(&state, "106 网关", "http://192.0.2.106:8000/v1", "sk-secret-123456").await;

        // 列表：key 脱敏、无明文
        let resp = call(
            &state,
            HttpMethod::Get,
            "/api/v1/llm/external-apis",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(resp.status, 200);
        let arr = resp.body["apis"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], id);
        assert_eq!(arr[0]["api_key_masked"], "sk-s***3456");
        assert_eq!(arr[0]["has_api_key"], true);
        assert_eq!(arr[0]["status"], "unknown");
        // 明文 key 绝不出现在响应里
        let raw = resp.body.to_string();
        assert!(!raw.contains("sk-secret-123456"), "响应泄漏明文 key: {raw}");

        // 删除 → 列表空；再删 404
        let resp = call(
            &state,
            HttpMethod::Delete,
            &format!("/api/v1/llm/external-apis/{id}"),
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["ok"], true);
        let resp = call(
            &state,
            HttpMethod::Get,
            "/api/v1/llm/external-apis",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(resp.body["apis"].as_array().unwrap().len(), 0);
        let resp = call(
            &state,
            HttpMethod::Delete,
            &format!("/api/v1/llm/external-apis/{id}"),
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn create_validates_name_and_base_url() {
        let state = LlmExternalState::with_memory();
        // 空 name
        let resp = call(
            &state,
            HttpMethod::Post,
            "/api/v1/llm/external-apis",
            serde_json::json!({"name":"", "base_url":"http://x/v1"}),
        )
        .await;
        assert_eq!(resp.status, 400);
        // 空 base_url
        let resp = call(
            &state,
            HttpMethod::Post,
            "/api/v1/llm/external-apis",
            serde_json::json!({"name":"x", "base_url":" "}),
        )
        .await;
        assert_eq!(resp.status, 400);
        // 非 http(s)
        let resp = call(
            &state,
            HttpMethod::Post,
            "/api/v1/llm/external-apis",
            serde_json::json!({"name":"x", "base_url":"ftp://x/v1"}),
        )
        .await;
        assert_eq!(resp.status, 400);
        assert!(resp.body["error"].as_str().unwrap().contains("http"));
        // 合法 https 201 + 尾斜杠收敛
        let resp = call(
            &state,
            HttpMethod::Post,
            "/api/v1/llm/external-apis",
            serde_json::json!({"name":"y", "base_url":"https://api.example.com/v1/"}),
        )
        .await;
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["base_url"], "https://api.example.com/v1");
    }

    #[test]
    fn route_specs_shape() {
        let specs = route_specs();
        assert_eq!(specs.len(), 6, "6 条路由");
        assert!(specs.iter().all(|s| s.handler_component == "llm"));
        // GET 公开、其余 admin
        assert!(!specs[0].requires_auth);
        assert!(
            specs[1..]
                .iter()
                .all(|s| s.requires_auth && s.required_roles == ["admin"])
        );
    }

    // ---- PUT 编辑（部分更新语义）----

    #[tokio::test]
    async fn edit_updates_each_field_and_preserves_unprovided() {
        let state = LlmExternalState::with_memory();
        let id = create(
            &state,
            "106 网关",
            "http://192.0.2.106:8000/v1",
            "sk-keep-secret",
        )
        .await;

        // 逐字段改 name / base_url / api_key / models / notes
        let resp = call(
            &state,
            HttpMethod::Put,
            &format!("/api/v1/llm/external-apis/{id}"),
            serde_json::json!({
                "name": "106 网关（新名）",
                "base_url": "http://192.0.2.107:8000/v1/",
                "api_key": "sk-new-key-987654",
                "models": [" m1 ", "", "m2"],
                "notes": "换到了 107"
            }),
        )
        .await;
        assert_eq!(resp.status, 200, "body: {resp:?}");
        assert_eq!(resp.body["name"], "106 网关（新名）");
        assert_eq!(resp.body["base_url"], "http://192.0.2.107:8000/v1", "尾斜杠收敛");
        assert_eq!(resp.body["api_key_masked"], "sk-n***7654");
        assert_eq!(resp.body["models"], serde_json::json!(["m1", "m2"]), "trim + 空项过滤");
        assert_eq!(resp.body["notes"], "换到了 107");
        // 服务端行内明文 key 已被覆盖（仅内存/库侧可见）
        let row = state.get(&id).unwrap();
        assert_eq!(row.api_key, "sk-new-key-987654");

        // 未提供的字段保留原值（空对象 = 全部保留）
        let resp = call(
            &state,
            HttpMethod::Put,
            &format!("/api/v1/llm/external-apis/{id}"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(resp.status, 200, "body: {resp:?}");
        assert_eq!(resp.body["name"], "106 网关（新名）");
        assert_eq!(resp.body["base_url"], "http://192.0.2.107:8000/v1");
        assert_eq!(resp.body["models"], serde_json::json!(["m1", "m2"]));
        assert_eq!(resp.body["notes"], "换到了 107");
        let row = state.get(&id).unwrap();
        assert_eq!(row.api_key, "sk-new-key-987654", "api_key 未提供必须保留原值");

        // api_key 提供空串 = 清除鉴权（has_api_key 翻 false）
        let resp = call(
            &state,
            HttpMethod::Put,
            &format!("/api/v1/llm/external-apis/{id}"),
            serde_json::json!({"api_key": "  "}),
        )
        .await;
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["has_api_key"], false);
        assert_eq!(state.get(&id).unwrap().api_key, "");

        // notes 提供空串 = 清除备注（None 而非空串）
        let resp = call(
            &state,
            HttpMethod::Put,
            &format!("/api/v1/llm/external-apis/{id}"),
            serde_json::json!({"notes": "   "}),
        )
        .await;
        assert_eq!(resp.status, 200);
        assert!(resp.body["notes"].is_null(), "空 notes 应清除: {resp:?}");
    }

    #[tokio::test]
    async fn edit_validates_and_404() {
        let state = LlmExternalState::with_memory();
        // 不存在 → 404（body 合法）
        let resp = call(
            &state,
            HttpMethod::Put,
            "/api/v1/llm/external-apis/xapi-999",
            serde_json::json!({"name": "x"}),
        )
        .await;
        assert_eq!(resp.status, 404);
        // 空 name → 400
        let id = create(&state, "106", "http://192.0.2.106:8000/v1", "").await;
        for bad in [
            serde_json::json!({"name": "  "}),
            serde_json::json!({"base_url": " "}),
            serde_json::json!({"base_url": "ftp://x/v1"}),
        ] {
            let resp = call(
                &state,
                HttpMethod::Put,
                &format!("/api/v1/llm/external-apis/{id}"),
                bad,
            )
            .await;
            assert_eq!(resp.status, 400, "非法字段应 400: {resp:?}");
        }
        // 校验失败不改行（原值保留）
        let row = state.get(&id).unwrap();
        assert_eq!(row.name, "106");
        assert_eq!(row.base_url, "http://192.0.2.106:8000/v1");
    }

    #[test]
    fn edit_persists_across_state_reopen() {
        let dir = std::env::temp_dir().join(format!(
            "nexos-llm-ext-edit-{}",
            os_core::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("llm.db");
        let id = {
            let state = LlmExternalState::new(Arc::new(Mutex::new(
                Connection::open(&db).unwrap(),
            )));
            let api = ExternalApi {
                id: "xapi-1".into(),
                name: "旧名".into(),
                base_url: "http://old:8000/v1".into(),
                api_key: "sk-old".into(),
                models: vec![],
                status: "ok".into(),
                last_check_at: None,
                notes: None,
                via_node: String::new(),
                created_at: "t0".into(),
            };
            state.persist(&api);
            // 直接落一条编辑（persist 是 INSERT OR REPLACE）
            let mut edited = api.clone();
            edited.name = "新名".into();
            edited.base_url = "http://new:9000/v1".into();
            edited.api_key = "sk-new".into();
            state.persist(&edited);
            edited.id
        };
        let state = LlmExternalState::new(Arc::new(Mutex::new(
            Connection::open(&db).unwrap(),
        )));
        let row = state.get(&id).unwrap();
        assert_eq!(row.name, "新名");
        assert_eq!(row.base_url, "http://new:9000/v1");
        assert_eq!(row.api_key, "sk-new", "编辑后的 key 持久化");
        let _ = std::fs::remove_file(&db);
    }

    // ---- mock 上游（真 TCP，llm.rs spawn_fake_v1_models_server 手法）----

    /// 单连接 mock：读请求 → `script(请求文本)` 产响应体 → 回 200 JSON。
    /// 返回 (port, 收到的原始请求文本)。
    fn spawn_mock_upstream<F>(script: F) -> (u16, std::sync::Arc<Mutex<String>>)
    where
        F: FnOnce(&str) -> String + Send + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 失败");
        let port = listener.local_addr().expect("local_addr").port();
        let seen = std::sync::Arc::new(Mutex::new(String::new()));
        let seen2 = std::sync::Arc::clone(&seen);
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buf = vec![0u8; 64 * 1024];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req_text = String::from_utf8_lossy(&buf[..n]).to_string();
            *seen2.lock().unwrap() = req_text.clone();
            let body = script(&req_text);
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        });
        (port, seen)
    }

    fn models_body(ids: &[&str]) -> String {
        serde_json::json!({
            "object": "list",
            "data": ids.iter().map(|id| serde_json::json!({"id": id, "object": "model"})).collect::<Vec<_>>(),
        })
        .to_string()
    }

    // ---- 连通测试（真实 GET /models，mock 真 TCP）----

    #[tokio::test]
    async fn test_endpoint_success_parses_models_and_sends_auth() {
        let (port, seen) = spawn_mock_upstream(|_| models_body(&["qwen3.5-9b", "qwen3-8b"]));
        let state = LlmExternalState::with_memory();
        let id = create(
            &state,
            "106",
            &format!("http://127.0.0.1:{port}/v1"),
            "sk-upstream",
        )
        .await;

        let resp = call(
            &state,
            HttpMethod::Post,
            &format!("/api/v1/llm/external-apis/{id}/test"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(resp.status, 200, "body: {resp:?}");
        assert_eq!(resp.body["ok"], true);
        assert_eq!(
            resp.body["models"],
            serde_json::json!(["qwen3.5-9b", "qwen3-8b"]),
            "模型清单应来自真实 /models 响应"
        );
        assert!(resp.body["latency_ms"].as_u64().is_some(), "真实延迟");
        assert!(resp.body["error"].is_null());
        // 请求侧：真实 GET /models + Bearer 头（hyper 统一小写头名，大小写不敏感比对）
        let seen = seen.lock().unwrap().clone();
        assert!(
            seen.starts_with("GET /v1/models "),
            "应 GET <base>/models: {seen}"
        );
        assert!(
            seen.to_ascii_lowercase()
                .contains("authorization: bearer sk-upstream"),
            "缺鉴权头: {seen}"
        );

        // 状态翻转 + 空 models 回填
        let list = call(
            &state,
            HttpMethod::Get,
            "/api/v1/llm/external-apis",
            serde_json::Value::Null,
        )
        .await;
        let row = &list.body["apis"][0];
        assert_eq!(row["status"], "ok");
        assert!(row["last_check_at"].as_str().is_some());
        assert_eq!(
            row["models"],
            serde_json::json!(["qwen3.5-9b", "qwen3-8b"]),
            "test 应回填空 models"
        );
    }

    #[tokio::test]
    async fn test_endpoint_failure_marks_error() {
        // 死端口（bind 后立即 drop → 连接拒绝）
        let dead = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let state = LlmExternalState::with_memory();
        let id = create(&state, "死上游", &format!("http://127.0.0.1:{dead}/v1"), "").await;

        let resp = call(
            &state,
            HttpMethod::Post,
            &format!("/api/v1/llm/external-apis/{id}/test"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(resp.status, 200, "探测失败不是 HTTP 错误");
        assert_eq!(resp.body["ok"], false);
        assert_eq!(resp.body["models"], serde_json::json!([]));
        let err = resp.body["error"].as_str().unwrap();
        assert!(!err.is_empty(), "失败必须带原因");
        // 状态翻转 error
        let list = call(
            &state,
            HttpMethod::Get,
            "/api/v1/llm/external-apis",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(list.body["apis"][0]["status"], "error");
        // 不存在 404
        let resp = call(
            &state,
            HttpMethod::Post,
            "/api/v1/llm/external-apis/xapi-999/test",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(resp.status, 404);
    }

    // ---- 对话直通（非流式，mock）----

    fn chat_ok_body() -> String {
        serde_json::json!({
            "id": "chatcmpl-1",
            "model": "qwen3.5-9b",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "你好，我是 qwen3.5-9b。"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 7, "completion_tokens": 9, "total_tokens": 16},
        })
        .to_string()
    }

    #[tokio::test]
    async fn chat_nonstream_passes_through_upstream_body_and_usage() {
        let (port, seen) = spawn_mock_upstream(|req| {
            assert!(req.starts_with("POST /v1/chat/completions "), "路径错: {req}");
            chat_ok_body()
        });
        let state = LlmExternalState::with_memory();
        let id = create(&state, "106", &format!("http://127.0.0.1:{port}/v1"), "sk-up").await;

        let resp = call(
            &state,
            HttpMethod::Post,
            &format!("/api/v1/llm/external-apis/{id}/chat"),
            serde_json::json!({
                "model": "qwen3.5-9b",
                "messages": [{"role": "user", "content": "自我介绍"}],
                "stream": true, // 组件路径应强制非流式转发
            }),
        )
        .await;
        assert_eq!(resp.status, 200, "body: {resp:?}");
        // 上游响应原样透传（含 usage，不估算不重写）
        assert_eq!(
            resp.body["choices"][0]["message"]["content"],
            "你好，我是 qwen3.5-9b。"
        );
        assert_eq!(resp.body["usage"]["total_tokens"], 16);
        assert_eq!(resp.body["model"], "qwen3.5-9b");
        // 请求侧：model 透传 + stream 强制 false + Bearer
        let seen = seen.lock().unwrap().clone();
        assert!(seen.contains("\"model\":\"qwen3.5-9b\""), "model 未透传: {seen}");
        assert!(
            seen.contains("\"stream\":false"),
            "组件路径必须强制非流式: {seen}"
        );
        assert!(
            seen.to_ascii_lowercase().contains("authorization: bearer sk-up"),
            "缺鉴权头: {seen}"
        );
    }

    #[tokio::test]
    async fn chat_validates_body_and_missing_row() {
        let state = LlmExternalState::with_memory();
        // 缺 model
        let resp = call(
            &state,
            HttpMethod::Post,
            "/api/v1/llm/external-apis/xapi-101/chat",
            serde_json::json!({"messages": [{"role":"user","content":"hi"}]}),
        )
        .await;
        assert_eq!(resp.status, 400);
        // 缺 messages
        let resp = call(
            &state,
            HttpMethod::Post,
            "/api/v1/llm/external-apis/xapi-101/chat",
            serde_json::json!({"model": "m"}),
        )
        .await;
        assert_eq!(resp.status, 400);
        // 空消息 content
        let resp = call(
            &state,
            HttpMethod::Post,
            "/api/v1/llm/external-apis/xapi-101/chat",
            serde_json::json!({"model": "m", "messages": [{"role":"user","content":" "}]}),
        )
        .await;
        assert_eq!(resp.status, 400);
        // 行不存在（body 合法）
        let resp = call(
            &state,
            HttpMethod::Post,
            "/api/v1/llm/external-apis/xapi-999/chat",
            serde_json::json!({"model": "m", "messages": [{"role":"user","content":"hi"}]}),
        )
        .await;
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn chat_upstream_http_error_maps_502() {
        // mock 一个 500 上游
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let body = "{\"error\":\"model overloaded\"}";
            let resp = format!(
                "HTTP/1.1 500 Internal Server Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
        });
        let state = LlmExternalState::with_memory();
        let id = create(&state, "500 上游", &format!("http://127.0.0.1:{port}/v1"), "").await;
        let resp = call(
            &state,
            HttpMethod::Post,
            &format!("/api/v1/llm/external-apis/{id}/chat"),
            serde_json::json!({"model": "m", "messages": [{"role":"user","content":"hi"}]}),
        )
        .await;
        assert_eq!(resp.status, 502);
        assert!(resp.body["error"].as_str().unwrap().contains("500"));
    }

    // ---- SSE 透传流（核心函数级：先正常块，再错误块 → 注释帧收尾）----

    #[tokio::test]
    async fn sse_passthrough_stream_yields_chunks_then_error_comment() {
        use futures::StreamExt;
        let items: Vec<Result<Bytes, String>> = vec![
            Ok(Bytes::from("data: {\"a\":1}\n\n")),
            Ok(Bytes::from("data: [DONE]\n\n")),
            Err("connection reset".into()),
            // errored 后再有项也不该再出（unfold 在注释帧后收尾）
            Ok(Bytes::from("SHOULD-NOT-APPEAR")),
        ];
        let src = futures::stream::iter(items);
        let s = sse_passthrough_stream(Bytes::from("first\n"), src);
        futures::pin_mut!(s);
        let mut out = String::new();
        let mut count = 0;
        while let Some(chunk) = s.next().await {
            count += 1;
            out.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        }
        assert_eq!(count, 4, "首块 + 2 数据块 + 1 注释帧");
        assert!(out.starts_with("first\n"));
        assert!(out.contains("data: {\"a\":1}"));
        assert!(out.contains("data: [DONE]"));
        assert!(
            out.contains(": llm-ext: upstream stream aborted (connection reset)"),
            "中断应有注释帧: {out}"
        );
        assert!(!out.contains("SHOULD-NOT-APPEAR"), "注释帧后应收尾断流");
    }

    // ---- 持久化（真实文件库：重启恢复 + 计数器越过）----

    #[test]
    fn persists_rows_across_reopen() {
        let dir = std::env::temp_dir().join(format!("nexos-llm-ext-{}", os_core::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("llm.db");
        {
            let conn = Connection::open(&db).unwrap();
            create_schema(&conn).unwrap();
            let state = LlmExternalState::new(Arc::new(Mutex::new(conn)));
            state.persist(&ExternalApi {
                id: "xapi-1".into(),
                name: "106".into(),
                base_url: "http://192.0.2.106:8000/v1".into(),
                api_key: "sk-keep".into(),
                models: vec!["qwen3.5-9b".into()],
                status: "ok".into(),
                last_check_at: Some("t".into()),
                notes: None,
                via_node: String::new(),
                created_at: "t0".into(),
            });
        }
        // 重开同一文件：行恢复、key 明文在库内（服务端侧）
        let conn = Connection::open(&db).unwrap();
        let state = LlmExternalState::new(Arc::new(Mutex::new(conn)));
        let list = state.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "xapi-1");
        assert_eq!(list[0].models, vec!["qwen3.5-9b"]);
        assert_eq!(list[0].api_key, "sk-keep");
        // 计数器越过存量最大后缀 → 新 id 不撞
        assert_eq!(state.next_id(), "xapi-2");
        let _ = std::fs::remove_file(&db);
    }

    // ---- via_node（联邦导入 → overlay 中继，2026-09-02）----

    use crate::handlers::api_market::{ApiMarketFedEndpoint, FedBroadcastFn, FedSendFn};

    /// fake 互连 overlay：消费者端点 a ↔ 源端点 source 定向互投（api_market
    /// 测试 relay_pair 同款手法；源端白名单已种 endpoint_url）。
    fn wire_fake_relay(source: ApiMarketFedEndpoint) -> (ApiMarketFedEndpoint, String) {
        let a = ApiMarketFedEndpoint::test_endpoint();
        let a_id = os_p2p::NodeIdentity::generate().node_id();
        let b_id = os_p2p::NodeIdentity::generate().node_id();
        let a_hex = a_id.to_hex();
        let b_hex = b_id.to_hex();
        let b2 = source.clone();
        let b_target = b_id.clone();
        let a_from = a_id.clone();
        let send_to_b: FedSendFn = Arc::new(move |to, payload| {
            if *to == b_target {
                b2.dispatch(&a_from, &payload);
            }
        });
        let noop_broadcast: FedBroadcastFn = Arc::new(|_| {});
        a.set_full_transport(send_to_b, noop_broadcast.clone(), a_hex, "node-a".into());
        let a3 = a.clone();
        let a_target = a_id.clone();
        let b_from = b_id.clone();
        let send_to_a: FedSendFn = Arc::new(move |to, payload| {
            if *to == a_target {
                a3.dispatch(&b_from, &payload);
            }
        });
        source.set_full_transport(send_to_a, noop_broadcast, b_hex.clone(), "node-b".into());
        (a, b_hex)
    }

    /// 合法 NodeID hex（测试数据生成用）。
    fn some_node_hex() -> String {
        os_p2p::NodeIdentity::generate().node_id().to_hex()
    }

    #[tokio::test]
    async fn via_node_create_update_validation_and_masking() {
        let state = LlmExternalState::with_memory();
        // 非法 via_node（非 0x+66 hex）→ 400。
        let resp = call(
            &state,
            HttpMethod::Post,
            "/api/v1/llm/external-apis",
            serde_json::json!({
                "name": "联邦条目", "base_url": "http://192.0.2.106:8558/v1",
                "via_node": "not-a-node",
            }),
        )
        .await;
        assert_eq!(resp.status, 400);
        assert!(resp.body["error"].as_str().unwrap().contains("via_node"));
        // 合法 via_node → 201 + masked 输出带 via_node。
        let node = some_node_hex();
        let resp = call(
            &state,
            HttpMethod::Post,
            "/api/v1/llm/external-apis",
            serde_json::json!({
                "name": "联邦条目", "base_url": "http://192.0.2.106:8558/v1",
                "via_node": node,
            }),
        )
        .await;
        assert_eq!(resp.status, 201, "body: {resp:?}");
        assert_eq!(resp.body["via_node"], node);
        let id = resp.body["id"].as_str().unwrap().to_string();
        assert_eq!(state.get(&id).unwrap().via_node, node, "落库持久");
        // PUT：未提供保留；空串清除（回直连语义）。
        let resp = call(
            &state,
            HttpMethod::Put,
            &format!("/api/v1/llm/external-apis/{id}"),
            serde_json::json!({"name": "改名"}),
        )
        .await;
        assert_eq!(resp.body["via_node"], node, "未提供保留");
        let resp = call(
            &state,
            HttpMethod::Put,
            &format!("/api/v1/llm/external-apis/{id}"),
            serde_json::json!({"via_node": ""}),
        )
        .await;
        assert_eq!(resp.body["via_node"], "", "空串清除");
        assert_eq!(state.get(&id).unwrap().via_node, "");
        // 直连行（不提供 via_node）默认空——语义不变。
        let resp = call(
            &state,
            HttpMethod::Post,
            "/api/v1/llm/external-apis",
            serde_json::json!({"name": "直连", "base_url": "http://x/v1"}),
        )
        .await;
        assert_eq!(resp.body["via_node"], "");
    }

    /// 前端联邦一键导入「已登记 → 升级」路径的后端契约（2026-09-03 修复）：
    /// 0.1.16 前导入的旧行（无 via_node / 无 key / 直连语义）+ PUT 只带
    /// via_node+models（脱敏视角不带 api_key）→ 两字段升级、缺省 key 原样
    /// 保留（空即空，不被置成掩码串）。
    #[tokio::test]
    async fn edit_upgrades_legacy_direct_row_via_fed_reimport() {
        let state = LlmExternalState::with_memory();
        let node = some_node_hex();
        // 旧行：直连语义（via_node 空、无 key、无模型——0.1.16 前联邦导入形态）
        let id = create(&state, "qwen3.5-9b@ub2604", "http://192.168.5.11:8558/v1", "").await;
        // 联邦条目为脱敏视角：PUT 升级补丁只有 via_node + models
        let resp = call(
            &state,
            HttpMethod::Put,
            &format!("/api/v1/llm/external-apis/{id}"),
            serde_json::json!({"via_node": node, "models": ["qwen3.5-9b"]}),
        )
        .await;
        assert_eq!(resp.status, 200, "body: {resp:?}");
        assert_eq!(resp.body["via_node"], node, "升级为经源节点中继");
        assert_eq!(resp.body["models"], serde_json::json!(["qwen3.5-9b"]));
        assert_eq!(resp.body["has_api_key"], false, "缺省 api_key 不覆盖");
        let row = state.get(&id).unwrap();
        assert_eq!(row.via_node, node, "落库持久");
        assert_eq!(row.api_key, "", "旧行无 key 时保持空（不被掩码串污染）");
        assert_eq!(row.base_url, "http://192.168.5.11:8558/v1", "未提供字段保留");
    }

    /// 中继未装配（P2P 未启用 / 测试未注入）→ 错误明确「经 <节点> 中继失败」，
    /// 与直连失败可区分。
    #[tokio::test]
    async fn via_node_relay_not_configured_reports_clear_error() {
        let state = LlmExternalState::with_memory();
        let node = some_node_hex();
        let id = create(&state, "联邦未组网", "http://192.0.2.106:8558/v1", "").await;
        let resp = call(
            &state,
            HttpMethod::Put,
            &format!("/api/v1/llm/external-apis/{id}"),
            serde_json::json!({"via_node": node}),
        )
        .await;
        assert_eq!(resp.status, 200);
        // test：ok:false + 经 <节点> 中继失败（状态翻转 error）。
        let resp = call(
            &state,
            HttpMethod::Post,
            &format!("/api/v1/llm/external-apis/{id}/test"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["ok"], false);
        let err = resp.body["error"].as_str().unwrap();
        assert!(err.contains("中继失败"), "test 错误可区分: {err}");
        // chat：502 + 同前缀。
        let resp = call(
            &state,
            HttpMethod::Post,
            &format!("/api/v1/llm/external-apis/{id}/chat"),
            serde_json::json!({"model": "m", "messages": [{"role":"user","content":"hi"}]}),
        )
        .await;
        assert_eq!(resp.status, 502);
        let err = resp.body["error"].as_str().unwrap();
        assert!(err.contains("中继失败"), "chat 错误可区分: {err}");
        assert!(err.contains("…"), "节点以短式呈现: {err}");
    }

    /// 端到端（消费者全链路）：via_node 条目 POST test/chat → overlay 中继 →
    /// 源端白名单放行 → mock 上游；usage 原样透传、models 真实解析。
    #[tokio::test]
    async fn test_and_chat_via_fake_relay_end_to_end() {
        // mock 上游（两连接：先 /models 后 /chat/completions——spawn_mock_upstream
        // 是单连接脚本，这里直接起双连接版）。
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for _ in 0..2 {
                let Ok((mut s, _)) = listener.accept() else { return };
                let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buf = vec![0u8; 64 * 1024];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let body = if req.starts_with("GET /v1/models ") {
                    models_body(&["qwen3.5-9b"])
                } else {
                    assert!(
                        req.starts_with("POST /v1/chat/completions "),
                        "上游路径: {req}"
                    );
                    chat_ok_body()
                };
                let _ = s.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .as_bytes(),
                );
                let _ = s.flush();
            }
        });
        let base = format!("http://127.0.0.1:{port}/v1");
        // 源端点：白名单种 base（本地已发布条目）；消费者端点互连后注入 state。
        let source = ApiMarketFedEndpoint::test_endpoint_with_local_listing(&base);
        let (relay, b_hex) = wire_fake_relay(source);
        let state = LlmExternalState::with_memory();
        state.set_relay(Some(relay));
        // 登记联邦导入条目（via_node = 源节点 NodeID——前端一键导入的形态）。
        let id = create(&state, "106 联邦", &base, "sk-fed-relay").await;
        let resp = call(
            &state,
            HttpMethod::Put,
            &format!("/api/v1/llm/external-apis/{id}"),
            serde_json::json!({"via_node": b_hex}),
        )
        .await;
        assert_eq!(resp.status, 200);
        // —— test：经中继 GET /models ——
        let resp = call(
            &state,
            HttpMethod::Post,
            &format!("/api/v1/llm/external-apis/{id}/test"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(resp.status, 200, "body: {resp:?}");
        assert_eq!(resp.body["ok"], true, "经中继连通: {resp:?}");
        assert_eq!(resp.body["models"], serde_json::json!(["qwen3.5-9b"]));
        // —— chat：经中继 POST /chat/completions（上游 JSON 原样透传含 usage）——
        let resp = call(
            &state,
            HttpMethod::Post,
            &format!("/api/v1/llm/external-apis/{id}/chat"),
            serde_json::json!({
                "model": "qwen3.5-9b",
                "messages": [{"role": "user", "content": "自我介绍"}],
            }),
        )
        .await;
        assert_eq!(resp.status, 200, "body: {resp:?}");
        assert_eq!(
            resp.body["choices"][0]["message"]["content"],
            "你好，我是 qwen3.5-9b。"
        );
        assert_eq!(resp.body["usage"]["total_tokens"], 16, "usage 原样透传");
        // 状态翻转 ok（真实探测结果）。
        let list = call(
            &state,
            HttpMethod::Get,
            "/api/v1/llm/external-apis",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(list.body["apis"][0]["status"], "ok");
    }

    /// 流式（via_node 全链路）：relay_chat_stream_response → SSE 逐块透传 +
    /// content-type 透传；上游非 2xx → 首 JSON 错误（经 <节点> 中继前缀）。
    #[tokio::test]
    async fn stream_via_fake_relay_passes_chunks_and_errors() {
        // SSE 上游：三块 + DONE。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let Ok((mut s, _)) = listener.accept() else { return };
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n");
            let _ = s.flush();
            let _ = s.write_all(b"data: {\"delta\":{\"content\":\"a\"}}\n\n");
            let _ = s.flush();
            let _ = s.write_all(b"data: {\"delta\":{\"content\":\"b\"}}\n\n");
            let _ = s.write_all(b"data: [DONE]\n\n");
        });
        let base = format!("http://127.0.0.1:{port}/v1");
        let source = ApiMarketFedEndpoint::test_endpoint_with_local_listing(&base);
        let (relay, b_hex) = wire_fake_relay(source);
        let state = LlmExternalState::with_memory();
        state.set_relay(Some(relay));
        let api = ExternalApi {
            id: "xapi-9".into(),
            name: "联邦流式".into(),
            base_url: base.clone(),
            api_key: String::new(),
            models: vec![],
            status: "unknown".into(),
            last_check_at: None,
            notes: None,
            via_node: b_hex,
            created_at: "t".into(),
        };
        state.persist(&api);
        let body_json = serde_json::json!({"model": "m", "messages": [{"role":"user","content":"hi"}], "stream": true});
        let resp = relay_chat_stream_response(&state, &api, &body_json, "m").await;
        assert_eq!(resp.status(), 200);
        assert!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.contains("text/event-stream")),
            "content-type 透传"
        );
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("\"content\":\"a\""), "块序 1: {text}");
        assert!(text.contains("\"content\":\"b\""), "块序 2: {text}");
        assert!(text.contains("data: [DONE]"), "收尾: {text}");

        // 非 2xx：源端把错误体单帧收尾 → 首 JSON 错误（未吐字节）。
        let listener2 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port2 = listener2.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let Ok((mut s, _)) = listener2.accept() else { return };
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let body = "{\"error\":\"model overloaded\"}";
            let _ = s.write_all(format!(
                "HTTP/1.1 503 Service Unavailable\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            ).as_bytes());
        });
        let base2 = format!("http://127.0.0.1:{port2}/v1");
        let source2 = ApiMarketFedEndpoint::test_endpoint_with_local_listing(&base2);
        let (relay2, b_hex2) = wire_fake_relay(source2);
        let state2 = LlmExternalState::with_memory();
        state2.set_relay(Some(relay2));
        let api2 = ExternalApi {
            id: "xapi-10".into(),
            name: "联邦 503".into(),
            base_url: base2,
            api_key: String::new(),
            models: vec![],
            status: "unknown".into(),
            last_check_at: None,
            notes: None,
            via_node: b_hex2,
            created_at: "t".into(),
        };
        state2.persist(&api2);
        let resp = relay_chat_stream_response(&state2, &api2, &body_json, "m").await;
        assert_eq!(resp.status(), 502);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let err = v["error"].as_str().unwrap();
        assert!(err.contains("中继上游返回 HTTP 503"), "错误前缀可区分: {err}");
        assert!(err.contains("model overloaded"));
    }
}
