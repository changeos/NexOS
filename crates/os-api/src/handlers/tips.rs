//! `TipsRouteHandler` —— 统一打赏原语（链上身份账本，docs/TIPS.md）。
//!
//! 定位：打赏 = 一条**真实账本记录**（from 链上身份 pubkey → to 目标所有者
//! pubkey），挂到具体目标（IM 消息 / 大厅条目 / 节点）。四个大厅面
//! （IM 聊天、NexHub 代码大厅、模型大厅、API 大厅）共用同一原语。
//!
//! # 服务端不虚构链上转账
//!
//! `amount` 为站内积分记账；可选 `txid` 字段登记用户**自报**的真实链上凭证
//! （不验真——与 NexHub purchase 同已知限制，登记入安全隐患台账
//! docs/FEATURE_SURVEY_2026-08-20.md §5.3，按项目惯例「只记录不处理」）。
//!
//! # to_pubkey 服务端解析（防自报伪造）
//!
//! 请求体**不含**收款方——`to_pubkey` 由服务端按 target 反查目标所有者：
//!
//! | target_kind | target_ref | 解析路径 |
//! |-------------|------------|----------|
//! | `im_message` | IM 消息 id | im.db `im_messages.sender_id`（`fed:<node>:<pubkey>` 取末段）|
//! | `lobby_entry` | `nexhub:<repo_name>` | hub_lobby.db `hub_lobby.publisher`（须为合法 pubkey）|
//! | `lobby_entry` | `model:<name>@<sharer>` | model_lobby.db `model_lobby.sharer`（id 直查）|
//! | `lobby_entry` | `apimarket:<id>` | api_market.db `api_market.publisher_pubkey` |
//! | `node` | NodeID（`0x`+66hex） | NodeID 本身即节点身份公钥（node-meta/peers 同源）|
//!
//! 解析不到（目标不存在 / 所有者非链上身份——如平台托管条目）→ 400 带原因。
//!
//! # from 身份（链上 token 优先，Principal 回落）
//!
//! 与 im.rs 同款取法：`Authorization: Bearer <链上 token>` → 依次过 IM 的
//! [`ChainAuth`] 与 nexhub/api-market 共享的 [`ChainAuth`]（token 桶互不相通，
//! 同一密钥对可两侧分别认证）；验出 → from = token 反查 pubkey。无 token /
//! 两边验不过 → 回落网关 Principal（[`crate::http::extract_principal`] 现有语义：
//! 测试期默认注入 admin），admin 无 pubkey → 保留字 `"admin"`。
//!
//! # 持久化（tips.db，llm.rs 同款惯例）
//!
//! SQLite `tips` 表（WAL + 幂等建表 + CHECK(amount>0) + (target_kind,target_ref)
//! 索引）。DB 路径 env `NEXOS_TIPS_DB` 覆盖 → `/tank/os-data/tips.db` →
//! `/var/lib/os/tips.db` → `./tips.db`（保底，llm.rs 三级链同款写法）。
//!
//! # 路由表（3 条，component="tips"；链上 token 一律 handler 内自验，
//! requires_auth=false——网关系统中间件不认识链上 token，挂 true 会全拦，
//! 与 api-market 同理）
//!
//! | method | path                              | 动作 |
//! |--------|-----------------------------------|------|
//! | POST   | `/api/v1/tips`                    | 打赏入账 → 202（身份：链上 token / Principal 回落）|
//! | GET    | `/api/v1/tips/target/:kind/:ref`  | 目标聚合 {total,count,recent≤20}（公开读）|
//! | GET    | `/api/v1/tips/me`                 | 我收到/给出的聚合（按身份）|
//!
//! # 大厅接入方式（前端并行拉取，后端零侵入）
//!
//! 各大厅 handler **不深改**：前端条目卡片/消息操作区并行调
//! `GET /tips/target/:kind/:ref` 取真实累计数（权衡：多一跳请求，换取
//! 大厅 handler 零侵入——见 docs/TIPS.md §拓扑）。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};
use os_common::chain_auth::{self, ChainAuth};

// ----------------------------------------------------------------------------
// 常量
// ----------------------------------------------------------------------------

/// target_kind：IM 消息。
const KIND_IM_MESSAGE: &str = "im_message";
/// target_kind：大厅条目（nexhub / model / apimarket 按 ref 前缀分流）。
const KIND_LOBBY_ENTRY: &str = "lobby_entry";
/// target_kind：节点（NodeID 即身份公钥）。
const KIND_NODE: &str = "node";
/// 全部合法 target_kind。
const KINDS: [&str; 3] = [KIND_IM_MESSAGE, KIND_LOBBY_ENTRY, KIND_NODE];

/// lobby_entry ref 前缀：NexHub 代码大厅条目（`nexhub:<repo_name>`）。
const REF_PREFIX_NEXHUB: &str = "nexhub";
/// lobby_entry ref 前缀：模型大厅条目（`model:<name>@<sharer>`）。
const REF_PREFIX_MODEL: &str = "model";
/// lobby_entry ref 前缀：API 大厅条目（`apimarket:<id>`）。
const REF_PREFIX_API_MARKET: &str = "apimarket";

/// recent 列表上限（GET /tips/target）。
const RECENT_LIMIT: usize = 20;
/// /me 两列 recent 上限。
const ME_RECENT_LIMIT: usize = 10;
/// 留言长度上限（字符）。
const MESSAGE_MAX_CHARS: usize = 500;
/// 自报 txid 长度上限（字符）。
const TXID_MAX_CHARS: usize = 128;
/// target_ref 长度上限（字符）。
const REF_MAX_CHARS: usize = 512;
/// 脱敏前缀长度（from/to/txid 展示前 N 字符 + "…"）。
const MASK_PREFIX_CHARS: usize = 10;

// ----------------------------------------------------------------------------
// 各来源 DB 路径（to_pubkey 反查用，逐库与各家 handler 的默认链一致）
// ----------------------------------------------------------------------------

/// 打赏账本默认路径：env `NEXOS_TIPS_DB` 覆盖 → `/tank/os-data/tips.db` →
/// `/var/lib/os/tips.db` → `./tips.db`（llm.rs 三级链同款写法）。
fn default_tips_db_path() -> String {
    if let Some(p) = env_non_empty("NEXOS_TIPS_DB") {
        return p;
    }
    for p in &["/tank/os-data/tips.db", "/var/lib/os/tips.db"] {
        if std::path::Path::new(p)
            .parent()
            .is_some_and(|d| d.exists() || std::fs::create_dir_all(d).is_ok())
        {
            return (*p).to_string();
        }
    }
    "./tips.db".to_string()
}

/// 通用默认链（与 im.rs / nexhub_lobby.rs / model_hub.rs / api_market.rs 各自
/// 的 default_db_path 同款：`/tank/os-data/<file>` → `/var/lib/os/<file>` →
/// `./<file>`）。
fn lobby_db_default(file: &str) -> String {
    for p in &[
        format!("/tank/os-data/{file}"),
        format!("/var/lib/os/{file}"),
    ] {
        if std::path::Path::new(p)
            .parent()
            .is_some_and(|d| d.exists() || std::fs::create_dir_all(d).is_ok())
        {
            return p.clone();
        }
    }
    format!("./{file}")
}

/// env 非空取值（llm.rs 同款）。
fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// 各来源 DB 路径集合（to_pubkey 反查；默认与各家 handler 同库同路径）。
#[derive(Debug, Clone)]
pub struct TipsSources {
    /// IM 库（im_messages.sender_id）。
    pub im_db: String,
    /// NexHub 大厅库（hub_lobby.publisher）。
    pub nexhub_db: String,
    /// 模型大厅库（model_lobby.sharer）。
    pub model_db: String,
    /// API 大厅库（api_market.publisher_pubkey）。
    pub api_market_db: String,
}

impl Default for TipsSources {
    fn default() -> Self {
        Self {
            im_db: lobby_db_default("im.db"),
            nexhub_db: lobby_db_default("hub_lobby.db"),
            model_db: lobby_db_default("model_lobby.db"),
            api_market_db: lobby_db_default("api_market.db"),
        }
    }
}

// ----------------------------------------------------------------------------
// SQLite 账本
// ----------------------------------------------------------------------------

/// 打开打赏账本（WAL + 幂等建表）。
fn open_ledger(path: &str) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    create_ledger_schema(&conn)?;
    Ok(conn)
}

/// 建表（IF NOT EXISTS）：CHECK(amount>0) + 目标索引。
fn create_ledger_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS tips (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            from_pubkey TEXT NOT NULL,
            to_pubkey   TEXT NOT NULL,
            target_kind TEXT NOT NULL,
            target_ref  TEXT NOT NULL,
            amount      INTEGER NOT NULL,
            message     TEXT,
            txid        TEXT,
            created_at  INTEGER NOT NULL,
            CHECK(amount > 0)
        );
        CREATE INDEX IF NOT EXISTS idx_tips_target ON tips(target_kind, target_ref);
        CREATE INDEX IF NOT EXISTS idx_tips_to ON tips(to_pubkey);
        CREATE INDEX IF NOT EXISTS idx_tips_from ON tips(from_pubkey);
        ",
    )?;
    Ok(())
}

/// 账本行（聚合/列表输出用；from/to 视场景脱敏）。
#[derive(Debug, Clone, Serialize)]
struct TipRow {
    id: i64,
    from_pubkey: String,
    to_pubkey: String,
    target_kind: String,
    target_ref: String,
    amount: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    txid: Option<String>,
    created_at: i64,
}

/// 落一条打赏（INSERT；amount 已在上游校验 > 0，CHECK 约束兜底）。
fn insert_tip(conn: &Connection, r: &TipRow) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO tips
         (from_pubkey,to_pubkey,target_kind,target_ref,amount,message,txid,created_at)
         VALUES (?,?,?,?,?,?,?,?)",
        params![
            r.from_pubkey,
            r.to_pubkey,
            r.target_kind,
            r.target_ref,
            r.amount,
            r.message,
            r.txid,
            r.created_at,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 目标聚合：{total, count}（一次查询取回）。
fn aggregate_target(conn: &Connection, kind: &str, ref_: &str) -> rusqlite::Result<(i64, i64)> {
    conn.query_row(
        "SELECT COALESCE(SUM(amount),0), COUNT(*) FROM tips
         WHERE target_kind=?1 AND target_ref=?2",
        params![kind, ref_],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
}

/// 按目标的最近打赏（新在前，上限 limit）。
fn recent_for_target(
    conn: &Connection,
    kind: &str,
    ref_: &str,
    limit: usize,
) -> rusqlite::Result<Vec<TipRow>> {
    let mut stmt = conn.prepare(
        "SELECT id,from_pubkey,to_pubkey,target_kind,target_ref,amount,message,txid,created_at
         FROM tips WHERE target_kind=?1 AND target_ref=?2
         ORDER BY id DESC LIMIT ?3",
    )?;
    let iter = stmt.query_map(params![kind, ref_, limit as i64], map_tip_row)?;
    iter.collect()
}

/// 按身份聚合（收到 to_pubkey=? / 给出 from_pubkey=?，两列各一次查询）。
fn aggregate_identity(
    conn: &Connection,
    pubkey: &str,
) -> rusqlite::Result<((i64, i64), (i64, i64))> {
    let received = conn.query_row(
        "SELECT COALESCE(SUM(amount),0), COUNT(*) FROM tips WHERE to_pubkey=?1",
        params![pubkey],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let given = conn.query_row(
        "SELECT COALESCE(SUM(amount),0), COUNT(*) FROM tips WHERE from_pubkey=?1",
        params![pubkey],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok((received, given))
}

/// 我收到的最近打赏（新在前）。
fn recent_received(conn: &Connection, pubkey: &str, limit: usize) -> rusqlite::Result<Vec<TipRow>> {
    let mut stmt = conn.prepare(
        "SELECT id,from_pubkey,to_pubkey,target_kind,target_ref,amount,message,txid,created_at
         FROM tips WHERE to_pubkey=?1 ORDER BY id DESC LIMIT ?2",
    )?;
    let iter = stmt.query_map(params![pubkey, limit as i64], map_tip_row)?;
    iter.collect()
}

/// 我给出的最近打赏（新在前）。
fn recent_given(conn: &Connection, pubkey: &str, limit: usize) -> rusqlite::Result<Vec<TipRow>> {
    let mut stmt = conn.prepare(
        "SELECT id,from_pubkey,to_pubkey,target_kind,target_ref,amount,message,txid,created_at
         FROM tips WHERE from_pubkey=?1 ORDER BY id DESC LIMIT ?2",
    )?;
    let iter = stmt.query_map(params![pubkey, limit as i64], map_tip_row)?;
    iter.collect()
}

/// 行映射（列序与 SELECT 一致）。
fn map_tip_row(row: &rusqlite::Row) -> rusqlite::Result<TipRow> {
    Ok(TipRow {
        id: row.get(0)?,
        from_pubkey: row.get(1)?,
        to_pubkey: row.get(2)?,
        target_kind: row.get(3)?,
        target_ref: row.get(4)?,
        amount: row.get(5)?,
        message: row.get(6)?,
        txid: row.get(7)?,
        created_at: row.get(8)?,
    })
}

// ----------------------------------------------------------------------------
// 脱敏（纯函数）
// ----------------------------------------------------------------------------

/// 身份/凭证脱敏：前 10 字符 + "…"（admin 等短保留字原样）。
fn mask(s: &str) -> String {
    if s.chars().count() <= MASK_PREFIX_CHARS {
        return s.to_string();
    }
    let prefix: String = s.chars().take(MASK_PREFIX_CHARS).collect();
    format!("{prefix}…")
}

/// recent 条目（对外脱敏形：from/txid 前缀 + 金额/留言/时间）。
fn masked_entry(r: &TipRow) -> serde_json::Value {
    serde_json::json!({
        "from": mask(&r.from_pubkey),
        "to": mask(&r.to_pubkey),
        "target_kind": r.target_kind,
        "target_ref": r.target_ref,
        "amount": r.amount,
        "message": r.message,
        "txid": r.txid.as_deref().map(mask),
        "created_at": r.created_at,
    })
}

// ----------------------------------------------------------------------------
// to_pubkey 解析（服务端反查，防自报伪造）
// ----------------------------------------------------------------------------

/// 只读打开来源库（文件不存在/打不开 → Err 带库路径，转 400）。
fn open_source_ro(path: &str) -> Result<Connection, String> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("来源库不可读（{path}）: {e}"))
}

/// 单列查询助手（LIKE 不用；只做等值）。
fn query_text(conn: &Connection, sql: &str, key: &str) -> Result<Option<String>, rusqlite::Error> {
    use rusqlite::OptionalExtension;
    conn.query_row(sql, params![key], |row| row.get::<_, String>(0))
        .optional()
}

/// 从所有者字段解析合法 pubkey（平台托管字符串 → None）。
fn owner_to_pubkey(owner: &str) -> Option<String> {
    if chain_auth::parse_pubkey(owner).is_some() {
        return Some(owner.to_string());
    }
    None
}

/// IM 消息作者解析：`im_messages.sender_id`；联邦消息 `fed:<node>:<pubkey>`
/// 取末段；系统消息（system）与自报名（无链上身份）不可解析。
fn resolve_im_author(sender_id: &str) -> Option<String> {
    let candidate = if let Some(rest) = sender_id.strip_prefix("fed:") {
        rest.rsplit(':').next().unwrap_or(rest)
    } else {
        sender_id
    };
    owner_to_pubkey(candidate)
}

/// 解析 target → 收款方 pubkey。Err = 400 原因（目标不存在 / 无链上身份 /
/// 来源库不可读 / ref 格式非法）。
fn resolve_to_pubkey(sources: &TipsSources, kind: &str, ref_: &str) -> Result<String, String> {
    match kind {
        KIND_IM_MESSAGE => {
            let conn = open_source_ro(&sources.im_db)?;
            let sender = query_text(&conn, "SELECT sender_id FROM im_messages WHERE id=?1", ref_)
                .map_err(|e| format!("IM 库查询失败: {e}"))?;
            match sender {
                Some(s) => {
                    resolve_im_author(&s).ok_or_else(|| format!("消息作者无链上身份（sender={s}）"))
                }
                None => Err(format!("IM 消息不存在: {ref_}")),
            }
        }
        KIND_LOBBY_ENTRY => {
            let (prefix, id) = ref_.split_once(':').ok_or_else(|| {
                "lobby_entry ref 须为 <来源>:<条目id>（如 nexhub:nexos）".to_string()
            })?;
            match prefix {
                REF_PREFIX_NEXHUB => {
                    let conn = open_source_ro(&sources.nexhub_db)?;
                    let publisher = query_text(
                        &conn,
                        "SELECT publisher FROM hub_lobby WHERE repo_name=?1",
                        id,
                    )
                    .map_err(|e| format!("NexHub 库查询失败: {e}"))?;
                    match publisher {
                        Some(p) => owner_to_pubkey(&p).ok_or_else(|| {
                            format!("条目为平台托管（publisher={p}），无链上身份可打赏")
                        }),
                        None => Err(format!("NexHub 大厅条目不存在: {id}")),
                    }
                }
                REF_PREFIX_MODEL => {
                    let conn = open_source_ro(&sources.model_db)?;
                    let sharer = query_text(
                        &conn,
                        "SELECT sharer FROM model_lobby WHERE id=?1",
                        id,
                    )
                    .map_err(|e| format!("模型大厅库查询失败: {e}"))?;
                    match sharer {
                        Some(s) => owner_to_pubkey(&s).ok_or_else(|| {
                            format!("分享者非链上身份（sharer={s}），不可打赏")
                        }),
                        None => Err(format!("模型大厅条目不存在: {id}")),
                    }
                }
                REF_PREFIX_API_MARKET => {
                    let conn = open_source_ro(&sources.api_market_db)?;
                    let pubkey = query_text(
                        &conn,
                        "SELECT publisher_pubkey FROM api_market WHERE id=?1",
                        id,
                    )
                    .map_err(|e| format!("API 大厅库查询失败: {e}"))?;
                    match pubkey {
                        Some(p) => owner_to_pubkey(&p)
                            .ok_or_else(|| format!("挂牌发布者身份非法: {p}")),
                        None => Err(format!("API 大厅条目不存在: {id}")),
                    }
                }
                other => Err(format!(
                    "lobby_entry ref 前缀须为 {REF_PREFIX_NEXHUB}/{REF_PREFIX_MODEL}/{REF_PREFIX_API_MARKET}，收到 {other}"
                )),
            }
        }
        KIND_NODE => {
            // NodeID 即节点身份公钥（os-p2p identity：NodeId 恒 0x+66hex 压缩
            // secp256k1，与 chain_auth 身份字符串同构）——解析成功即收款方。
            chain_auth::parse_pubkey(ref_)
                .map(|_| ref_.to_string())
                .ok_or_else(|| {
                    format!("node 目标 ref 须为合法 NodeID（0x+66hex 公钥），收到 {ref_}")
                })
        }
        other => Err(format!(
            "target_kind 须为 {}，收到 {other}",
            KINDS.join("/")
        )),
    }
}

// ----------------------------------------------------------------------------
// 请求/响应 DTO
// ----------------------------------------------------------------------------

/// POST /api/v1/tips 请求体。
#[derive(Debug, Deserialize)]
struct CreateTipBody {
    target_kind: String,
    target_ref: String,
    amount: i64,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    txid: Option<String>,
}

// ----------------------------------------------------------------------------
// TipsRouteHandler
// ----------------------------------------------------------------------------

/// 打赏账本路由处理器——HTTP 边界适配到 SQLite `tips` 表 + to_pubkey 服务端
/// 反查（跨 im.db / hub_lobby.db / model_lobby.db / api_market.db 只读）。
///
/// 持有 `Mutex<Connection>`（短锁快放，不跨 `.await` 持锁）+ 可选的两只
/// [`ChainAuth`]（IM / nexhub+api-market，main.rs 装配时注入共享实例——
/// handler 自验链上 token 反查 from pubkey）。
pub struct TipsRouteHandler {
    db: Mutex<Connection>,
    im_auth: Option<Arc<ChainAuth>>,
    chain_auth: Option<Arc<ChainAuth>>,
    sources: TipsSources,
}

impl TipsRouteHandler {
    /// 构造 handler：默认 DB 路径 + 无共享认证（from 回落 Principal/admin；
    /// 本地诊断用，生产装配走 [`Self::with_shared_auth`]）。
    #[must_use]
    pub fn new() -> Self {
        Self::open(&default_tips_db_path(), None, None, TipsSources::default())
    }

    /// main.rs 装配构造：默认 DB 路径 + 共享链上认证（im_auth = IM 的
    /// token 桶；chain_auth = nexhub/api-market 共享桶）。
    #[must_use]
    pub fn with_shared_auth(
        im_auth: Option<Arc<ChainAuth>>,
        chain_auth: Option<Arc<ChainAuth>>,
    ) -> Self {
        Self::open(
            &default_tips_db_path(),
            im_auth,
            chain_auth,
            TipsSources::default(),
        )
    }

    /// 全量注入构造（单元测试主入口：临时账本 + 指定来源库 + 认证桶）。
    #[must_use]
    pub fn with_parts(
        tips_db: &str,
        im_auth: Option<Arc<ChainAuth>>,
        chain_auth: Option<Arc<ChainAuth>>,
        sources: TipsSources,
    ) -> Self {
        Self::open(tips_db, im_auth, chain_auth, sources)
    }

    fn open(
        path: &str,
        im_auth: Option<Arc<ChainAuth>>,
        chain_auth: Option<Arc<ChainAuth>>,
        sources: TipsSources,
    ) -> Self {
        let conn = open_ledger(path).unwrap_or_else(|e| {
            eprintln!("[tips] 打开 SQLite {path} 失败（{e}），降级到内存库");
            let mem = Connection::open_in_memory().expect("内存库必成功");
            create_ledger_schema(&mem).expect("内存建表必成功");
            mem
        });
        Self {
            db: Mutex::new(conn),
            im_auth,
            chain_auth,
            sources,
        }
    }

    /// from 身份解析（im.rs 同款取法）：
    /// 1. Bearer token → IM 桶 / nexhub 桶依次验 → pubkey；
    /// 2. 回落网关 Principal（extract_principal 现有语义——测试期默认注入
    ///    admin）；admin 无 pubkey → 保留字 "admin"；
    /// 3. 解析不到（无 token 且 Principal None）→ None → 调用方 401。
    fn resolve_from(&self, req: &ApiRequest) -> Option<String> {
        if let Some(token) = chain_auth::bearer_token(&req.headers) {
            if let Some(pk) = self
                .im_auth
                .as_ref()
                .and_then(|a| a.verify_token(token))
                .or_else(|| self.chain_auth.as_ref().and_then(|a| a.verify_token(token)))
            {
                return Some(pk);
            }
        }
        // 回落 Principal：user.name（admin 身份即 "admin"；JWT sub 亦同名字段）
        req.auth.as_ref().map(|p| p.user.name.clone())
    }

    /// POST /api/v1/tips 处理：校验 → 解析 to_pubkey → 落账 → 202。
    async fn handle_create(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let body: CreateTipBody = serde_json::from_value(req.body.clone())
            .map_err(|e| ApiGatewayError::Internal(format!("解析打赏请求体失败: {e}")))?;
        let kind = body.target_kind.trim().to_string();
        let ref_ = body.target_ref.trim().to_string();
        if !KINDS.contains(&kind.as_str()) {
            return Ok(error_response(
                400,
                &format!("target_kind 须为 {}，收到 {kind}", KINDS.join("/")),
            ));
        }
        if ref_.is_empty() || ref_.chars().count() > REF_MAX_CHARS {
            return Ok(error_response(400, "target_ref 不可为空且 ≤512 字符"));
        }
        if body.amount <= 0 {
            return Ok(error_response(400, "amount 必须为正整数（站内积分）"));
        }
        let message = body
            .message
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string);
        if message
            .as_deref()
            .is_some_and(|m| m.chars().count() > MESSAGE_MAX_CHARS)
        {
            return Ok(error_response(
                400,
                &format!("留言 ≤{MESSAGE_MAX_CHARS} 字符"),
            ));
        }
        let txid = body
            .txid
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string);
        if txid
            .as_deref()
            .is_some_and(|t| t.chars().count() > TXID_MAX_CHARS)
        {
            return Ok(error_response(400, &format!("txid ≤{TXID_MAX_CHARS} 字符")));
        }

        // from：链上 token 优先 / Principal 回落；解析不到 401（不静默归 admin）
        let from = match self.resolve_from(&req) {
            Some(f) => f,
            None => {
                return Ok(error_response(
                    401,
                    "未识别身份：请带 IM/NexHub 链上 token，或经网关 Principal 认证",
                ));
            }
        };

        // to：服务端反查（防自报伪造）
        let to = match resolve_to_pubkey(&self.sources, &kind, &ref_) {
            Ok(t) => t,
            Err(reason) => {
                eprintln!("[tips] to_pubkey 解析失败 kind={kind} ref={ref_}: {reason}");
                return Ok(error_response(400, &format!("目标不可打赏: {reason}")));
            }
        };

        let row = TipRow {
            id: 0,
            from_pubkey: from,
            to_pubkey: to,
            target_kind: kind,
            target_ref: ref_,
            amount: body.amount,
            message,
            txid,
            created_at: chrono::Utc::now().timestamp(),
        };
        let id = {
            let conn = self.db.lock().expect("tips db poisoned");
            insert_tip(&conn, &row)
                .map_err(|e| ApiGatewayError::Internal(format!("打赏落账失败: {e}")))?
        };
        eprintln!(
            "[tips] #{} {} → {} {} 积分（{} {}）",
            id,
            mask(&row.from_pubkey),
            mask(&row.to_pubkey),
            row.amount,
            row.target_kind,
            row.target_ref
        );
        Ok(ApiResponse {
            status: 202,
            body: serde_json::json!({
                "ok": true,
                "id": id,
                "from": mask(&row.from_pubkey),
                "to": mask(&row.to_pubkey),
                "target_kind": row.target_kind,
                "target_ref": row.target_ref,
                "amount": row.amount,
                "created_at": row.created_at,
            }),
            headers: serde_json::json!({}),
        })
    }

    /// GET /api/v1/tips/target/:kind/:ref 处理：目标聚合 + 最近 20 条脱敏。
    fn handle_target(&self, kind: &str, ref_: &str) -> Result<ApiResponse, ApiGatewayError> {
        if !KINDS.contains(&kind) {
            return Ok(error_response(
                400,
                &format!("target_kind 须为 {}，收到 {kind}", KINDS.join("/")),
            ));
        }
        let conn = self.db.lock().expect("tips db poisoned");
        let (total, count) = aggregate_target(&conn, kind, ref_)
            .map_err(|e| ApiGatewayError::Internal(format!("聚合查询失败: {e}")))?;
        let recent = recent_for_target(&conn, kind, ref_, RECENT_LIMIT)
            .map_err(|e| ApiGatewayError::Internal(format!("recent 查询失败: {e}")))?;
        let recent: Vec<serde_json::Value> = recent.iter().map(masked_entry).collect();
        Ok(ok_json(serde_json::json!({
            "target_kind": kind,
            "target_ref": ref_,
            "total": total,
            "count": count,
            "recent": recent,
        })))
    }

    /// GET /api/v1/tips/me 处理：我收到/给出的聚合（按解析出的身份）。
    fn handle_me(&self, identity: &str) -> Result<ApiResponse, ApiGatewayError> {
        let conn = self.db.lock().expect("tips db poisoned");
        let ((recv_total, recv_count), (give_total, give_count)) =
            aggregate_identity(&conn, identity)
                .map_err(|e| ApiGatewayError::Internal(format!("身份聚合查询失败: {e}")))?;
        let received = recent_received(&conn, identity, ME_RECENT_LIMIT)
            .map_err(|e| ApiGatewayError::Internal(format!("received 查询失败: {e}")))?;
        let given = recent_given(&conn, identity, ME_RECENT_LIMIT)
            .map_err(|e| ApiGatewayError::Internal(format!("given 查询失败: {e}")))?;
        Ok(ok_json(serde_json::json!({
            "identity": mask(identity),
            "received": {"total": recv_total, "count": recv_count},
            "given": {"total": give_total, "count": give_count},
            "recent_received": received.iter().map(masked_entry).collect::<Vec<_>>(),
            "recent_given": given.iter().map(masked_entry).collect::<Vec<_>>(),
        })))
    }
}

impl Default for TipsRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

// ----------------------------------------------------------------------------
// RouteHandler 实现
// ----------------------------------------------------------------------------

#[async_trait]
impl RouteHandler for TipsRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            // 链上 token handler 内自验 + Principal 回落（挂 true 会把链上
            // token 全拦 401，与 api-market 同理）
            spec(HttpMethod::Post, "/api/v1/tips", false),
            spec(HttpMethod::Get, "/api/v1/tips/target/:kind/:ref", false),
            spec(HttpMethod::Get, "/api/v1/tips/me", false),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            (HttpMethod::Post, ["api", "v1", "tips"]) => self.handle_create(req).await,

            // —— GET /api/v1/tips/target/:kind/:ref —— 目标聚合（公开读；
            // ref 允许多段（lobby_entry 前缀/模型 id 带 @ 均无 /，此处仅防御）；
            // percent 解码见 percent_decode_segment 注释
            (HttpMethod::Get, ["api", "v1", "tips", "target", kind, rest @ ..])
                if !rest.is_empty() =>
            {
                let ref_ = percent_decode_segment(&rest.join("/"));
                self.handle_target(kind, &ref_)
            }

            // —— GET /api/v1/tips/me —— 我的聚合（按 from 身份）
            (HttpMethod::Get, ["api", "v1", "tips", "me"]) => match self.resolve_from(&req) {
                Some(identity) => self.handle_me(&identity),
                None => Ok(error_response(
                    401,
                    "未识别身份：请带 IM/NexHub 链上 token，或经网关 Principal 认证",
                )),
            },

            _ => Ok(error_response(404, "tips: 未匹配的路由")),
        }
    }
}

// ----------------------------------------------------------------------------
// 内部辅助
// ----------------------------------------------------------------------------

fn spec(method: HttpMethod, path: &str, requires_auth: bool) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: "tips".to_string(),
        requires_auth,
        required_roles: vec![],
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

/// 路径切段（各 handler 同款：剥 query、滤空段）。
fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

/// 仅解码路径段里的 `%XX`（不做 `+`→空格——query 语义；http.rs 的
/// `percent_decode_path` 同款，本地复制避免跨模块私有依赖）。
/// ref 含 `:`/`@`（lobby 前缀分隔、模型 id），前端 encodeURIComponent 会
/// 编成 `%3A`/`%40`——网关按原始路径分发不解码，handler 自行还原（未编码
/// 的 ref 原样通过，两态兼容）。
fn percent_decode_segment(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::ApiRequest;
    use os_security::{Principal, Role, User};

    /// 临时 DB 路径（llm.rs 测试同款手法：进程号 + 计数防并行互踩）。
    fn tmp_db_path(tag: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::env::temp_dir().join(format!("os-api-tips-{tag}-{}-{n}.db", std::process::id()))
    }

    /// 生成随机合法 pubkey（k256 同栈）。
    fn random_pubkey() -> String {
        use k256::elliptic_curve::rand_core::OsRng;
        let sk = k256::ecdsa::SigningKey::random(&mut OsRng);
        format!(
            "0x{}",
            hex::encode(sk.verifying_key().to_encoded_point(true).as_bytes())
        )
    }

    /// admin Principal（extract_principal 测试期默认注入同形）。
    fn admin_principal() -> Option<Principal> {
        let now = chrono::Utc::now();
        let roles = vec![Role::Admin];
        let user = User::new(
            os_security::UserId::new("admin".to_string()),
            "admin",
            roles.clone(),
            now,
        )
        .ok()?;
        Principal::new(user, roles, now).ok()
    }

    /// POST 请求构造（可带头/身份）。
    fn post_tip(body: serde_json::Value, headers: serde_json::Value) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Post,
            path: "/api/v1/tips".into(),
            headers,
            body,
            auth: admin_principal(),
        }
    }

    /// GET 请求构造（无凭据 → admin Principal 注入同 extract_principal 语义）。
    fn get(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: admin_principal(),
        }
    }

    /// 建一个只含目标表最小 schema 的来源库并插入一行（owner 任意）。
    fn seed_source_db(path: &std::path::Path, ddl: &str, insert: &str) {
        let conn = Connection::open(path).expect("来源库必开");
        conn.execute_batch(ddl).expect("建表必成");
        conn.execute(insert, []).expect("插行必成");
    }

    /// 测试 handler：临时账本 + 四个空来源库路径（目标解析用）。
    fn handler_with_sources(
        tag: &str,
    ) -> (
        TipsRouteHandler,
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        let tips = tmp_db_path(tag);
        let im = tmp_db_path(&format!("{tag}-im"));
        let nexhub = tmp_db_path(&format!("{tag}-hub"));
        let model = tmp_db_path(&format!("{tag}-model"));
        let market = tmp_db_path(&format!("{tag}-market"));
        let sources = TipsSources {
            im_db: im.to_string_lossy().into_owned(),
            nexhub_db: nexhub.to_string_lossy().into_owned(),
            model_db: model.to_string_lossy().into_owned(),
            api_market_db: market.to_string_lossy().into_owned(),
        };
        (
            TipsRouteHandler::with_parts(
                tips.to_string_lossy().as_ref(),
                Some(Arc::new(ChainAuth::new())),
                Some(Arc::new(ChainAuth::new())),
                sources,
            ),
            im,
            nexhub,
            model,
            market,
        )
    }

    #[tokio::test]
    async fn routes_declares_all_tips_endpoints() {
        let h = TipsRouteHandler::with_parts(
            tmp_db_path("routes").to_string_lossy().as_ref(),
            None,
            None,
            TipsSources::default(),
        );
        let routes = h.routes().await;
        assert_eq!(routes.len(), 3, "应有 3 条路由: {routes:?}");
        assert!(routes.iter().all(|r| r.handler_component == "tips"));
        assert!(
            routes.iter().all(|r| !r.requires_auth),
            "链上 token 自验，全部 requires_auth=false"
        );
        for p in [
            "/api/v1/tips",
            "/api/v1/tips/target/:kind/:ref",
            "/api/v1/tips/me",
        ] {
            assert!(routes.iter().any(|r| r.path == p), "缺路由 {p}");
        }
    }

    #[test]
    fn ledger_rejects_non_positive_amount_via_check() {
        let path = tmp_db_path("check");
        let conn = open_ledger(path.to_string_lossy().as_ref()).expect("建库必成");
        let row = TipRow {
            id: 0,
            from_pubkey: random_pubkey(),
            to_pubkey: random_pubkey(),
            target_kind: KIND_NODE.into(),
            target_ref: random_pubkey(),
            amount: 10,
            message: None,
            txid: None,
            created_at: 1_700_000_000,
        };
        let id = insert_tip(&conn, &row).expect("合法行必落");
        assert!(id > 0);
        // CHECK(amount>0)：直接绕过 API 写 0/负数必须被库层拒绝
        for bad in [0i64, -5] {
            let mut r = row.clone();
            r.amount = bad;
            assert!(
                insert_tip(&conn, &r).is_err(),
                "amount={bad} 应被 CHECK 拒绝"
            );
        }
    }

    #[tokio::test]
    async fn post_tip_rejects_bad_kind_ref_amount() {
        let (h, ..) = handler_with_sources("validate");
        // 未知 kind
        let resp = h
            .handle(post_tip(
                serde_json::json!({"target_kind": "video", "target_ref": "x", "amount": 1}),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // 空 ref
        let resp = h
            .handle(post_tip(
                serde_json::json!({"target_kind": KIND_NODE, "target_ref": "  ", "amount": 1}),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // amount ≤ 0
        for bad in [0, -1] {
            let resp = h
                .handle(post_tip(
                    serde_json::json!({"target_kind": KIND_NODE, "target_ref": random_pubkey(), "amount": bad}),
                    serde_json::json!({}),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 400, "amount={bad} 应 400");
        }
        // 留言超长
        let resp = h
            .handle(post_tip(
                serde_json::json!({
                    "target_kind": KIND_NODE, "target_ref": random_pubkey(), "amount": 1,
                    "message": "长".repeat(MESSAGE_MAX_CHARS + 1),
                }),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    #[tokio::test]
    async fn post_tip_resolves_im_message_author() {
        let (h, im_db, ..) = handler_with_sources("im");
        let author = random_pubkey();
        let fed_author = random_pubkey();
        seed_source_db(
            &im_db,
            "CREATE TABLE im_messages (id TEXT PRIMARY KEY, sender_id TEXT NOT NULL);",
            &format!(
                "INSERT INTO im_messages (id,sender_id) VALUES ('m1','{author}'),
                 ('m2','fed:node-a:{fed_author}'), ('m3','system');"
            ),
        );
        // 普通消息 → 作者 pubkey
        let resp = h
            .handle(post_tip(
                serde_json::json!({"target_kind": KIND_IM_MESSAGE, "target_ref": "m1", "amount": 50}),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 202, "{:?}", resp.body);
        assert_eq!(resp.body["to"], mask(&author));
        // 联邦消息 → fed: 前缀剥出 pubkey
        let resp = h
            .handle(post_tip(
                serde_json::json!({"target_kind": KIND_IM_MESSAGE, "target_ref": "m2", "amount": 5}),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 202);
        assert_eq!(resp.body["to"], mask(&fed_author));
        // 系统消息 → 无链上身份 400
        let resp = h
            .handle(post_tip(
                serde_json::json!({"target_kind": KIND_IM_MESSAGE, "target_ref": "m3", "amount": 5}),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "{:?}", resp.body);
        // 消息不存在 → 400
        let resp = h
            .handle(post_tip(
                serde_json::json!({"target_kind": KIND_IM_MESSAGE, "target_ref": "nope", "amount": 5}),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    #[tokio::test]
    async fn post_tip_resolves_three_lobby_sources() {
        let (h, _, nexhub_db, model_db, market_db) = handler_with_sources("lobby");
        let owner = random_pubkey();
        let sharer = random_pubkey();
        let publisher = random_pubkey();
        seed_source_db(
            &nexhub_db,
            "CREATE TABLE hub_lobby (repo_name TEXT PRIMARY KEY, publisher TEXT);",
            &format!(
                "INSERT INTO hub_lobby (repo_name,publisher) VALUES ('nexos','{owner}'),
                 ('house','NexOS');"
            ),
        );
        seed_source_db(
            &model_db,
            "CREATE TABLE model_lobby (id TEXT PRIMARY KEY, sharer TEXT);",
            &format!(
                "INSERT INTO model_lobby (id,sharer) VALUES ('qwen@{sharer}','{sharer}'),
                 ('llama@admin','admin');"
            ),
        );
        seed_source_db(
            &market_db,
            "CREATE TABLE api_market (id TEXT PRIMARY KEY, publisher_pubkey TEXT);",
            &format!("INSERT INTO api_market (id,publisher_pubkey) VALUES ('u1','{publisher}')"),
        );
        // NexHub 链上身份条目 → 202
        let resp = h
            .handle(post_tip(
                serde_json::json!({"target_kind": KIND_LOBBY_ENTRY, "target_ref": "nexhub:nexos", "amount": 100}),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 202, "{:?}", resp.body);
        assert_eq!(resp.body["to"], mask(&owner));
        // NexHub 平台托管条目 → 400
        let resp = h
            .handle(post_tip(
                serde_json::json!({"target_kind": KIND_LOBBY_ENTRY, "target_ref": "nexhub:house", "amount": 1}),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // 模型大厅链上分享者 → 202
        let resp = h
            .handle(post_tip(
                serde_json::json!({"target_kind": KIND_LOBBY_ENTRY, "target_ref": format!("model:qwen@{sharer}"), "amount": 30}),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 202, "{:?}", resp.body);
        assert_eq!(resp.body["to"], mask(&sharer));
        // 模型大厅 admin 分享 → 400
        let resp = h
            .handle(post_tip(
                serde_json::json!({"target_kind": KIND_LOBBY_ENTRY, "target_ref": "model:llama@admin", "amount": 1}),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // API 大厅 → 202（publisher_pubkey 唯一通道）
        let resp = h
            .handle(post_tip(
                serde_json::json!({"target_kind": KIND_LOBBY_ENTRY, "target_ref": "apimarket:u1", "amount": 20}),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 202, "{:?}", resp.body);
        assert_eq!(resp.body["to"], mask(&publisher));
        // 未知前缀 → 400
        let resp = h
            .handle(post_tip(
                serde_json::json!({"target_kind": KIND_LOBBY_ENTRY, "target_ref": "appstore:x", "amount": 1}),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    #[tokio::test]
    async fn post_tip_node_target_must_be_valid_pubkey() {
        let (h, ..) = handler_with_sources("node");
        let node_id = random_pubkey();
        let resp = h
            .handle(post_tip(
                serde_json::json!({"target_kind": KIND_NODE, "target_ref": node_id, "amount": 7, "txid": "0xabc123"}),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 202, "{:?}", resp.body);
        assert_eq!(resp.body["to"], mask(&node_id), "NodeID 即身份公钥");
        let resp = h
            .handle(post_tip(
                serde_json::json!({"target_kind": KIND_NODE, "target_ref": "node-106", "amount": 7}),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "昵称式 NodeID 应拒（须 0x+66hex）");
    }

    #[tokio::test]
    async fn from_identity_prefers_chain_token_then_principal() {
        let im_auth = Arc::new(ChainAuth::new());
        let user = random_pubkey();
        // 手工签发一枚 IM token（绕过 challenge——单元测聚焦 verify 反查路径）
        let (token, _) = im_auth.issue_token(&user);
        let node_id = random_pubkey();
        let h = TipsRouteHandler::with_parts(
            tmp_db_path("from-ledger").to_string_lossy().as_ref(),
            Some(im_auth),
            None,
            TipsSources::default(),
        );
        // 链上 token → from = 反查 pubkey
        let resp = h
            .handle(post_tip(
                serde_json::json!({"target_kind": KIND_NODE, "target_ref": node_id, "amount": 3}),
                serde_json::json!({ "authorization": format!("Bearer {token}") }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 202, "{:?}", resp.body);
        assert_eq!(resp.body["from"], mask(&user));
        // 无 token → Principal（admin）→ from = "admin"
        let resp = h
            .handle(post_tip(
                serde_json::json!({"target_kind": KIND_NODE, "target_ref": &node_id, "amount": 4}),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 202);
        assert_eq!(resp.body["from"], "admin");
        // 无 token 且无 Principal → 401
        let mut req = post_tip(
            serde_json::json!({"target_kind": KIND_NODE, "target_ref": &node_id, "amount": 4}),
            serde_json::json!({}),
        );
        req.auth = None;
        let resp = h.handle(req).await.unwrap();
        assert_eq!(resp.status, 401);
    }

    #[tokio::test]
    async fn target_aggregation_and_recent_masking() {
        let (h, ..) = handler_with_sources("agg");
        let target = random_pubkey();
        // 25 条全部 from=admin（Principal 回落路径；链上 token 路径另测）
        for i in 1..=25i64 {
            let resp = h
                .handle(post_tip(
                    serde_json::json!({
                        "target_kind": KIND_NODE, "target_ref": &target,
                        "amount": i, "message": format!("第{i}条"),
                    }),
                    serde_json::json!({}),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 202, "第{i}条应落账");
        }
        // 1..=25 求和 = 325，count = 25
        let resp = h
            .handle(get(&format!("/api/v1/tips/target/node/{target}")))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["total"], 325);
        assert_eq!(resp.body["count"], 25);
        let recent = resp.body["recent"].as_array().expect("recent 数组");
        assert_eq!(recent.len(), RECENT_LIMIT, "recent 截断到 20");
        assert_eq!(recent[0]["message"], "第25条", "新在前");
        assert_eq!(recent[0]["from"], "admin", "短身份不脱敏");
        assert!(recent[0]["created_at"].as_i64().is_some(), "时间戳齐全");
        // 未知 kind → 400
        let resp = h.handle(get("/api/v1/tips/target/bogus/x")).await.unwrap();
        assert_eq!(resp.status, 400);
    }

    #[tokio::test]
    async fn me_aggregation_by_identity() {
        let (h, im_db, ..) = handler_with_sources("me");
        let node_a = random_pubkey();
        let recipient = random_pubkey();
        // admin 打出 2 条（10 + 20），并打 recipient 的 IM 消息一条（5）
        seed_source_db(
            &im_db,
            "CREATE TABLE im_messages (id TEXT PRIMARY KEY, sender_id TEXT NOT NULL);",
            &format!("INSERT INTO im_messages (id,sender_id) VALUES ('m1','{recipient}')"),
        );
        for (kind, ref_, amount) in [
            (KIND_NODE, node_a.as_str(), 10i64),
            (KIND_IM_MESSAGE, "m1", 20),
            (KIND_IM_MESSAGE, "m1", 5),
        ] {
            let resp = h
                .handle(post_tip(
                    serde_json::json!({"target_kind": kind, "target_ref": ref_, "amount": amount}),
                    serde_json::json!({}),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 202);
        }
        // admin 视角：given = 35/3
        let resp = h.handle(get("/api/v1/tips/me")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["identity"], "admin");
        assert_eq!(resp.body["given"]["total"], 35);
        assert_eq!(resp.body["given"]["count"], 3);
        assert_eq!(resp.body["received"]["total"], 0);
        assert_eq!(resp.body["recent_given"].as_array().map(Vec::len), Some(3));
        // recipient 视角（链上 token）：received = 25/2，given = 0
        let (token, _) = h
            .im_auth
            .as_ref()
            .expect("测试 handler 注入了 im_auth")
            .issue_token(&recipient);
        let resp = h
            .handle(ApiRequest {
                method: HttpMethod::Get,
                path: "/api/v1/tips/me".into(),
                headers: serde_json::json!({ "authorization": format!("Bearer {token}") }),
                body: serde_json::Value::Null,
                auth: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{:?}", resp.body);
        assert_eq!(resp.body["identity"], mask(&recipient));
        assert_eq!(resp.body["received"]["total"], 25);
        assert_eq!(resp.body["received"]["count"], 2);
        assert_eq!(resp.body["given"]["total"], 0);
        assert_eq!(
            resp.body["recent_received"].as_array().map(Vec::len),
            Some(2)
        );
    }

    #[tokio::test]
    async fn ledger_survives_reopen_same_db() {
        let path = tmp_db_path("reopen");
        let node_id = random_pubkey();
        {
            let h = TipsRouteHandler::with_parts(
                path.to_string_lossy().as_ref(),
                None,
                None,
                TipsSources::default(),
            );
            let resp = h
                .handle(post_tip(
                    serde_json::json!({"target_kind": KIND_NODE, "target_ref": &node_id, "amount": 9}),
                    serde_json::json!({}),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 202);
        }
        // 模拟重启：同库重开，聚合仍在
        let h2 = TipsRouteHandler::with_parts(
            path.to_string_lossy().as_ref(),
            None,
            None,
            TipsSources::default(),
        );
        let resp = h2
            .handle(get(&format!("/api/v1/tips/target/node/{node_id}")))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["total"], 9);
        assert_eq!(resp.body["count"], 1);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn unmatched_path_returns_404() {
        let (h, ..) = handler_with_sources("404");
        let resp = h.handle(get("/api/v1/tips/unknown")).await.unwrap();
        assert_eq!(resp.status, 404);
        // target 缺 ref 段 → 404（未匹配 target 分支）
        let resp = h.handle(get("/api/v1/tips/target/node")).await.unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn target_ref_percent_decoded() {
        // 前端 encodeURIComponent 会把 lobby ref 的 `:`/`@` 编成 %3A/%40——
        // 网关按原始路径分发，handler 须解码后再聚合（未编码路径两态兼容）
        let (h, im_db, ..) = handler_with_sources("pct");
        let recipient = random_pubkey();
        seed_source_db(
            &im_db,
            "CREATE TABLE im_messages (id TEXT PRIMARY KEY, sender_id TEXT NOT NULL);",
            &format!("INSERT INTO im_messages (id,sender_id) VALUES ('m1','{recipient}')"),
        );
        let resp = h
            .handle(post_tip(
                serde_json::json!({"target_kind": KIND_IM_MESSAGE, "target_ref": "m1", "amount": 8}),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 202);
        // 编码路径 %3A（nexhub%3Anexos）与未编码（im 消息 id 无特殊字符）都 200
        let resp = h
            .handle(get("/api/v1/tips/target/lobby_entry/nexhub%3Anexos"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{:?}", resp.body);
        assert_eq!(resp.body["target_ref"], "nexhub:nexos", "解码后聚合");
        assert_eq!(resp.body["total"], 0);
        let resp = h
            .handle(get("/api/v1/tips/target/im_message/m1"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["total"], 8);
        assert_eq!(resp.body["count"], 1);
    }
}
