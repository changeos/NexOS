//! `WebhookService` —— NexHub 事件回调（Webhooks，v0.1.50 批 2/2 top1）。
//!
//! 设计（docs/research/NEXHUB_FEATURES.md §2 实现草案 1）：对标 GitHub/Gitea 的
//! webhook 语义——仓库（或 `*` 全局）维度的 HTTP 回调订阅，事件发生时后台
//! tokio 任务 POST JSON payload + **HMAC-SHA256 签名头**到订阅方 URL。
//!
//! # 事件矩阵（订阅事件 × 触发点）
//!
//! | event | action | 触发点（一行挂钩） |
//! |-------|--------|--------------------|
//! | `push` | `pushed` | os-api `http.rs` git-http push 成功路径（照 CI `push_hook` 先例，`fire_push`） |
//! | `issues` | `created` / `commented` / `closed` / `reopened` | [`crate::issues`] 写路径（建 Issue / 评论 / 关闭·重开） |
//! | `pr` | `created` / `merged` | [`crate::issues`] 写路径（建 PR / merge） |
//! | `release` | `published` | [`crate::nexhub_lobby`] 发版成功路径（`fire_release`） |
//!
//! # 投递语义
//!
//! - **异步**：事件点只做「查匹配钩子 + tokio::spawn」，绝不阻塞业务响应
//!   （同 CI push 旁路哲学：投递失败只记日志/状态，不影响主流程）。
//! - **超时**：单次请求 10s（reqwest per-request timeout，覆盖连接→读全程）。
//! - **重试**：失败（网络错误或非 2xx）立即重试 1 次，共 ≤2 次尝试。
//! - **签名**：`X-NexHub-Signature: sha256=<hex>` = HMAC-SHA256(secret, body)
//!   （GitHub X-Hub-Signature-256 同构；secret 为空仍签名，验方以约定 secret 校验）。
//!   附 `X-NexHub-Event`（事件名）+ `X-NexHub-Delivery`（投递 id）头。
//! - **记录**：每钩子 `last_delivery`/`last_status` 列 + 最近 20 条投递环形日志
//!   （`hub_webhook_log`，插入后裁剪仅留最新 20 行）。
//!
//! # 管理路由（8 条，component="code_repo"，全部网关 admin——钩子含 secret 不公开）
//!
//! | method | path | 动作 |
//! |--------|------|------|
//! | GET    | `/api/v1/coderepo/repos/:name/webhooks` | 列仓库钩子（含全局 `*` + 各自最近投递日志）|
//! | POST   | `/api/v1/coderepo/repos/:name/webhooks` | 建钩子 `{url, secret?, events}`（admin）|
//! | DELETE | `/api/v1/coderepo/repos/:name/webhooks/:id` | 删钩子（admin）|
//! | POST   | `/api/v1/coderepo/repos/:name/webhooks/:id/toggle` | 启停翻转（admin）|
//! | GET    | `/api/v1/coderepo/webhooks` | 全局钩子面（repo=`*`）|
//! | POST   | `/api/v1/coderepo/webhooks` | 建全局钩子（admin）|
//! | DELETE | `/api/v1/coderepo/webhooks/:id` | 删全局钩子（admin）|
//! | POST   | `/api/v1/coderepo/webhooks/:id/toggle` | 全局钩子启停（admin）|
//!
//! # 装配
//!
//! `CodeRepoRouteHandler::new()` 构造本服务并安装进程级全局槽（照
//! `nexhub_ci::install_global_core` 先例）：事件点（os-api http.rs 的 push /
//! issues.rs / nexhub_lobby.rs 的 release）经 [`fire_push`] 等 free function
//! 读槽投递，槽空（服务未装配）时静默 no-op——独立部署 lobby 不受影响。
//!
//! # 依赖说明（零 Cargo.toml 变更）
//!
//! HMAC-SHA256 所需 SHA-256 为**本模块内置实现**（`sha256_raw`，FIPS 180-4 标准
//! 结构 ~60 行），不引入新依赖——workspace 已有 `sha2` 但未在 os-nexhub 主依赖
//! 面，测试用 dev-dependencies 的 sha2/k256 交叉验证正确性（已知向量 + 随机
//! 输入对照）。

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use os_common::gateway::{ApiResponse, HandlerError, HttpMethod, RouteSpec};

use crate::code_repo::{repos_dir, resolve_default_branch_sync, validate_repo_name};
use crate::issues::{RepoComment, RepoIssue, RepoPull};
use crate::nexhub_lobby::Release;

// ----------------------------------------------------------------------------
// 内置 SHA-256 + HMAC-SHA256（零新依赖；测试与 dev-dep sha2 交叉验证）
// ----------------------------------------------------------------------------

/// SHA-256 轮常数（FIPS 180-4 §4.2.2）。
const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// SHA-256 摘要（内置实现，FIPS 180-4；输入任意长度 → 32 字节）。
///
/// 单测与 dev-dep `sha2` crate 对照（已知向量 + 随机长度输入），语义一致后才
/// 用于 HMAC。块内变量名 h0..h7 用 a..hh 展开避免下标来回读写。
fn sha256_raw(msg: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    // padding：0x80 + 0 填充至 ≡56 (mod 64) + 8 字节大端 bit 长度
    let bit_len = (msg.len() as u64).wrapping_mul(8);
    let mut data = Vec::with_capacity(msg.len() + 72);
    data.extend_from_slice(msg);
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in data.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            let b = &chunk[i * 4..i * 4 + 4];
            *word = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d) = (h[0], h[1], h[2], h[3]);
        let (mut e, mut f, mut g, mut hh) = (h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// HMAC-SHA256（RFC 2104：ipad/opad 双轮结构；key > 64 字节先哈希）。
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&sha256_raw(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Vec::with_capacity(64 + msg.len());
    inner.extend_from_slice(&ipad);
    inner.extend_from_slice(msg);
    let inner_hash = sha256_raw(&inner);
    let mut outer = Vec::with_capacity(96);
    outer.extend_from_slice(&opad);
    outer.extend_from_slice(&inner_hash);
    sha256_raw(&outer)
}

/// 生成签名头值：`sha256=<hex(HMAC-SHA256(secret, body))>`。
///
/// 投递方用 body 字节签名（序列化后的 UTF-8 JSON），验方以同 secret 对收到的
/// 原始 body 字节重算比对（恒定时间比较由验方实现——本仓只产签名）。
#[must_use]
pub fn sign_payload(secret: &str, body: &[u8]) -> String {
    format!("sha256={}", hex::encode(hmac_sha256(secret.as_bytes(), body)))
}

// ----------------------------------------------------------------------------
// 事件模型与 payload 构造（纯函数，可快照测试）
// ----------------------------------------------------------------------------

/// 事件类别（订阅面）：push / issues / pr / release。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebhookEvent {
    Push,
    Issues,
    Pr,
    Release,
}

impl WebhookEvent {
    /// 事件名（`events` 列与 `X-NexHub-Event` 头的取值域）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            WebhookEvent::Push => "push",
            WebhookEvent::Issues => "issues",
            WebhookEvent::Pr => "pr",
            WebhookEvent::Release => "release",
        }
    }

    /// 全部合法事件名（建钩子校验用）。
    #[must_use]
    pub fn all_names() -> &'static [&'static str] {
        &["push", "issues", "pr", "release"]
    }
}

/// 订阅事件串（逗号分隔）是否包含指定事件。
#[must_use]
pub fn events_match(events_csv: &str, event: &str) -> bool {
    events_csv
        .split(',')
        .any(|e| e.trim() == event)
}

/// push payload：`{event, action, repo, ref, sender, delivery, push:{after, message}}`。
#[must_use]
pub fn build_push_payload(
    repo: &str,
    git_ref: &str,
    after: &str,
    message: &str,
    pusher: &str,
    delivery: &str,
) -> Json {
    serde_json::json!({
        "event": "push",
        "action": "pushed",
        "repo": repo,
        "ref": git_ref,
        "sender": pusher,
        "delivery": delivery,
        "push": { "after": after, "message": message },
    })
}

/// issue payload：`{event, action, repo, sender, delivery, issue:{...}}`
/// （`commented` 额外带 `comment` 对象与 `issue.number` 摘要）。
#[must_use]
pub fn build_issue_payload(action: &str, issue: &Json, sender: &str, delivery: &str) -> Json {
    serde_json::json!({
        "event": "issues",
        "action": action,
        "repo": issue.get("repo").cloned().unwrap_or(Json::Null),
        "sender": sender,
        "delivery": delivery,
        "issue": issue,
    })
}

/// issue/pr 评论 payload：`{event:"issues", action:"commented", repo, sender,
/// delivery, issue:{number}, comment:{...}}`（kind=pull 时 issue 字段为 PR 摘要）。
#[must_use]
pub fn build_comment_payload(
    repo: &str,
    kind: &str,
    parent_number: u64,
    comment: &Json,
    sender: &str,
    delivery: &str,
) -> Json {
    serde_json::json!({
        "event": if kind == "pull" { "pr" } else { "issues" },
        "action": "commented",
        "repo": repo,
        "sender": sender,
        "delivery": delivery,
        "issue": { "number": parent_number },
        "comment": comment,
    })
}

/// PR payload：`{event:"pr", action, repo, sender, delivery, pull:{...}}`
/// （`merged` 额外带 `merged_sha`）。
#[must_use]
pub fn build_pull_payload(
    action: &str,
    pull: &Json,
    sender: &str,
    delivery: &str,
    merged_sha: Option<&str>,
) -> Json {
    let mut v = serde_json::json!({
        "event": "pr",
        "action": action,
        "repo": pull.get("repo").cloned().unwrap_or(Json::Null),
        "sender": sender,
        "delivery": delivery,
        "pull": pull,
    });
    if let Some(sha) = merged_sha {
        v["merged_sha"] = serde_json::json!(sha);
    }
    v
}

/// release payload：`{event:"release", action:"published", repo, sender, delivery,
/// release:{...}}`（repo 取 release.repo_name）。
#[must_use]
pub fn build_release_payload(release: &Json, sender: &str, delivery: &str) -> Json {
    serde_json::json!({
        "event": "release",
        "action": "published",
        "repo": release.get("repo_name").cloned().unwrap_or(Json::Null),
        "sender": sender,
        "delivery": delivery,
        "release": release,
    })
}

// ----------------------------------------------------------------------------
// DTO（持久化行投影）
// ----------------------------------------------------------------------------

/// 单条 webhook（`hub_webhooks` 行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Webhook {
    /// 钩子 id（`wh-<nanos>`）。
    pub id: String,
    /// 仓库名；`*` = 全局钩子（所有仓库事件都投递）。
    pub repo: String,
    /// 回调 URL（http/https）。
    pub url: String,
    /// 签名 secret（可空——空 secret 仍签名，验方按空串约定校验）。
    pub secret: String,
    /// 订阅事件（push/issues/pr/release）。
    pub events: Vec<String>,
    /// 是否启用（停用后不投递，保留配置）。
    pub enabled: bool,
    /// 创建时间（RFC3339）。
    pub created_at: String,
    /// 最近一次投递时间（未投递为 None）。
    pub last_delivery: Option<String>,
    /// 最近一次投递结果（"200" / "err: <原因>"；未投递为 None）。
    pub last_status: Option<String>,
}

/// 单条投递日志（`hub_webhook_log` 行，每钩子环形保留最近 20 条）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookDelivery {
    /// 日志行 id（自增）。
    pub id: i64,
    /// 事件名（push/issues/pr/release）。
    pub event: String,
    /// 目标 URL。
    pub url: String,
    /// 是否成功（2xx）。
    pub ok: bool,
    /// 结果描述（HTTP 状态码或错误原因）。
    pub status: String,
    /// 尝试次数（1 或 2——失败重试 1 次）。
    pub attempts: u32,
    /// 投递时间（RFC3339）。
    pub delivered_at: String,
}

// ----------------------------------------------------------------------------
// SQLite 持久化（Mutex<Connection> 短锁，同 issues/lobby 模式）
// ----------------------------------------------------------------------------

/// 环形日志保留条数（每钩子）。
const LOG_RING_SIZE: usize = 20;
/// 单次投递超时（覆盖连接→读完整响应全程）。
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);
/// 失败重试次数（总尝试 ≤ 2）。
const DELIVERY_RETRIES: u32 = 1;

fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS hub_webhooks (
            id             TEXT PRIMARY KEY,
            repo           TEXT NOT NULL,
            url            TEXT NOT NULL,
            secret         TEXT DEFAULT '',
            events         TEXT NOT NULL,
            enabled        INTEGER NOT NULL DEFAULT 1,
            created_at     TEXT NOT NULL,
            last_delivery  TEXT,
            last_status    TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_hub_webhooks_repo ON hub_webhooks(repo);
        CREATE TABLE IF NOT EXISTS hub_webhook_log (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            hook_id      TEXT NOT NULL,
            event        TEXT NOT NULL,
            url          TEXT NOT NULL,
            ok           INTEGER NOT NULL,
            status       TEXT NOT NULL,
            attempts     INTEGER NOT NULL,
            delivered_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_hub_webhook_log_hook ON hub_webhook_log(hook_id, id);",
    )
}

fn webhook_from_row(row: &rusqlite::Row) -> rusqlite::Result<Webhook> {
    Ok(Webhook {
        id: row.get(0)?,
        repo: row.get(1)?,
        url: row.get(2)?,
        secret: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
        events: row
            .get::<_, String>(4)?
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        enabled: row.get::<_, i64>(5)? != 0,
        created_at: row.get(6)?,
        last_delivery: row.get(7)?,
        last_status: row.get(8)?,
    })
}

fn delivery_from_row(row: &rusqlite::Row) -> rusqlite::Result<WebhookDelivery> {
    Ok(WebhookDelivery {
        id: row.get(0)?,
        event: row.get(2)?,
        url: row.get(3)?,
        ok: row.get::<_, i64>(4)? != 0,
        status: row.get(5)?,
        attempts: row.get::<_, i64>(6).map(|n| n.max(0) as u32)?,
        delivered_at: row.get(7)?,
    })
}

// ----------------------------------------------------------------------------
// WebhookService
// ----------------------------------------------------------------------------

/// Webhook 服务——CRUD（SQLite `hub_webhooks`）+ 匹配投递（异步 tokio spawn +
/// HMAC 签名 + 超时重试 + 环形日志）。
///
/// 装配：`CodeRepoRouteHandler::new()` 构造并安装全局槽；事件点经 [`fire_push`]
/// 等 free function 投递（槽空 no-op）。
pub struct WebhookService {
    /// 钩子配置 + 投递日志（`webhooks.db`，三级回退路径见 [`Self::default_db_path`]）。
    db: Arc<Mutex<Connection>>,
    /// 仓库根目录（仓库面建/列钩子的存在性校验；构造定格，测试注入绕 env 竞态）。
    repos_root: String,
}

impl WebhookService {
    /// 生产构造：默认 DB 路径（`/tank/os-data/webhooks.db` → `/var/lib/os/webhooks.db`
    /// → `./webhooks.db`，同 issues 三级回退）；文件库打开失败降级内存库不 panic。
    #[must_use]
    pub fn new() -> Self {
        let path = Self::default_db_path();
        let conn = Connection::open(&path).and_then(|c| {
            let _ = c.busy_timeout(Duration::from_millis(3000)); // 防 SQLITE_BUSY 立败（审计 E#6）
            let _ = c.pragma_update(None, "journal_mode", "WAL");
            create_schema(&c).map(|_| c)
        });
        let db = match conn {
            Ok(c) => Arc::new(Mutex::new(c)),
            Err(e) => {
                eprintln!("webhooks: 打开 SQLite {path} 失败（{e}），降级到内存库");
                let c = Connection::open_in_memory().expect("内存库必成功");
                create_schema(&c).expect("建表必成功");
                Arc::new(Mutex::new(c))
            }
        };
        Self {
            db,
            repos_root: repos_dir(),
        }
    }

    /// 测试构造：指定 DB 文件路径（隔离到 tempdir）。
    #[must_use]
    pub fn with_db_path(path: &str) -> Self {
        Self::with_paths(path, &repos_dir())
    }

    /// 测试构造：DB 文件路径 + 仓库根全注入（不读 env，规避并行测试竞态——
    /// 同 [`crate::issues::IssuesService::with_paths`] 模式）。
    #[must_use]
    pub fn with_paths(path: &str, repos_root: &str) -> Self {
        let conn = Connection::open(path).and_then(|c| {
            let _ = c.busy_timeout(Duration::from_millis(3000));
            let _ = c.pragma_update(None, "journal_mode", "WAL");
            create_schema(&c).map(|_| c)
        });
        let db = match conn {
            Ok(c) => Arc::new(Mutex::new(c)),
            Err(e) => {
                eprintln!("webhooks: 打开 SQLite {path} 失败（{e}），降级到内存库");
                let c = Connection::open_in_memory().expect("内存库必成功");
                create_schema(&c).expect("建表必成功");
                Arc::new(Mutex::new(c))
            }
        };
        Self {
            db,
            repos_root: repos_root.to_string(),
        }
    }

    /// 内存库构造（测试零文件副作用）。
    #[must_use]
    pub fn in_memory() -> Self {
        let conn = Connection::open_in_memory().expect("内存库必成功");
        create_schema(&conn).expect("建表必成功");
        Self {
            db: Arc::new(Mutex::new(conn)),
            repos_root: repos_dir(),
        }
    }

    fn default_db_path() -> String {
        for p in &["/tank/os-data/webhooks.db", "/var/lib/os/webhooks.db"] {
            if Path::new(p)
                .parent()
                .is_some_and(|d| d.exists() || std::fs::create_dir_all(d).is_ok())
            {
                return (*p).to_string();
            }
        }
        "./webhooks.db".to_string()
    }

    /// 安装进程级全局槽（装配时调用一次；重复安装保留首个——同 CI 先例）。
    pub fn install_global(self: Arc<Self>) -> bool {
        let mut slot = SERVICE_SLOT.lock().expect("webhook slot poisoned");
        if slot.is_some() {
            return false;
        }
        *slot = Some(self);
        true
    }

    // ------------------------------------------------------------------
    // CRUD（handler 分发用；返回 ApiResponse 直接复用）
    // ------------------------------------------------------------------

    /// 列钩子：`repo` 为具体仓库名时返回「该仓库专属 + 全局 `*`」两类，
    /// `repo=="*"`（全局面）仅返回全局钩子。每钩子附最近投递日志。
    pub fn list(&self, repo: &str) -> Vec<(Webhook, Vec<WebhookDelivery>)> {
        let conn = self.db.lock().expect("db poisoned");
        let hooks = match repo {
            "*" => load_hooks(&conn, "*"),
            other => load_hooks_matching(&conn, other),
        };
        hooks
            .into_iter()
            .map(|h| {
                let logs = load_logs(&conn, &h.id).unwrap_or_default();
                (h, logs)
            })
            .collect()
    }

    /// 建钩子：`repo` 为仓库名或 `*`；events 校验（合法事件、非空、去重）。
    pub fn create(&self, repo: &str, url: &str, secret: &str, events: &[String]) -> Result<Webhook, (u16, String)> {
        let repo = repo.trim();
        if repo == "*" {
            // 全局钩子：无仓库实体要求
        } else if let Err(msg) = validate_repo_name(repo) {
            return Err((400, msg));
        }
        let url = url.trim();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err((400, "url 须以 http:// 或 https:// 开头".to_string()));
        }
        if url.len() > 2000 {
            return Err((400, "url 过长（≤2000 字符）".to_string()));
        }
        let secret = secret.trim();
        if secret.chars().count() > 200 {
            return Err((400, "secret 过长（≤200 字符）".to_string()));
        }
        // 事件规范化：trim、去空、去重、限量保序（全部合法事件）
        let mut normalized: Vec<String> = Vec::new();
        for e in events {
            let e = e.trim().to_ascii_lowercase();
            if e.is_empty() {
                continue;
            }
            if !WebhookEvent::all_names().contains(&e.as_str()) {
                return Err((400, format!("非法事件 {e:?}（可选 push/issues/pr/release）")));
            }
            if !normalized.contains(&e) {
                normalized.push(e);
            }
        }
        if normalized.is_empty() {
            return Err((
                400,
                "events 不可为空（至少订阅 push/issues/pr/release 之一）".to_string(),
            ));
        }
        let hook = Webhook {
            id: format!("wh-{}", now_nanos()),
            repo: repo.to_string(),
            url: url.to_string(),
            secret: secret.to_string(),
            events: normalized,
            enabled: true,
            created_at: now_iso(),
            last_delivery: None,
            last_status: None,
        };
        let conn = self.db.lock().expect("db poisoned");
        if let Err(e) = conn.execute(
            // 字面量 1 落在 enabled（第 6 位）；created_at 走第 7 位占位符——
            // 此前 (?,?,?,?,?,?,1) 顺序错位：created_at 写进 enabled、1 写进
            // created_at，行映射恒败（list 恒空）
            "INSERT INTO hub_webhooks (id, repo, url, secret, events, enabled, created_at) \
             VALUES (?,?,?,?,?,1,?)",
            params![
                hook.id,
                hook.repo,
                hook.url,
                hook.secret,
                hook.events.join(","),
                hook.created_at,
            ],
        ) {
            return Err((500, format!("写入 webhook 失败: {e}")));
        }
        Ok(hook)
    }

    /// 删钩子（scope 限定：repo 面内匹配钩子 repo 为该仓或 `*`；日志级联删除）。
    pub fn delete(&self, scope: &str, id: &str) -> Result<(), (u16, String)> {
        let conn = self.db.lock().expect("db poisoned");
        let removed = conn
            .execute(
                "DELETE FROM hub_webhooks WHERE id=? AND (repo=? OR (?='*' AND repo='*'))",
                params![id, scope, scope],
            )
            .map_err(|e| (500, format!("删除 webhook 失败: {e}")))?;
        if removed == 0 {
            return Err((404, format!("webhook 不存在: {id}")));
        }
        let _ = conn.execute("DELETE FROM hub_webhook_log WHERE hook_id=?", params![id]);
        Ok(())
    }

    /// 启停翻转（停用后不投递，配置保留）。返回翻转后的状态。
    pub fn toggle(&self, scope: &str, id: &str) -> Result<bool, (u16, String)> {
        let conn = self.db.lock().expect("db poisoned");
        let current: Option<i64> = conn
            .query_row(
                "SELECT enabled FROM hub_webhooks WHERE id=? AND (repo=? OR (?='*' AND repo='*'))",
                params![id, scope, scope],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| (500, format!("查询 webhook 失败: {e}")))?;
        let Some(current) = current else {
            return Err((404, format!("webhook 不存在: {id}")));
        };
        let next = current == 0; // 1 → 停用，0 → 启用
        conn.execute(
            "UPDATE hub_webhooks SET enabled=? WHERE id=?",
            params![next as i64, id],
        )
        .map_err(|e| (500, format!("更新 webhook 失败: {e}")))?;
        Ok(next)
    }

    // ------------------------------------------------------------------
    // 投递
    // ------------------------------------------------------------------

    /// 事件分发入口：查匹配钩子（enabled + repo 匹配 + 事件订阅）→ 每钩子
    /// tokio::spawn 异步投递（绝不阻塞调用方；槽由 fire_* 包装）。
    pub async fn dispatch_event(&self, event: WebhookEvent, repo: &str, payload: Json) {
        let hooks = {
            let conn = self.db.lock().expect("db poisoned");
            load_hooks_matching(&conn, repo)
        };
        let name = event.as_str();
        for hook in hooks {
            if !hook.enabled || !hook.events.iter().any(|e| e == name) {
                continue;
            }
            let db = Arc::clone(&self.db);
            let payload = payload.clone();
            tokio::spawn(async move {
                Self::deliver_with(&db, &hook, name, &payload).await;
            });
        }
    }

    /// 单钩子投递：POST JSON + 签名头，10s 超时，失败重试 1 次，写投递记录。
    ///
    /// `pub` 供集成测试直接 await（确定性验证）；生产路径经 [`Self::dispatch_event`]
    /// 的 tokio::spawn 走同一函数。
    pub async fn deliver_one(&self, hook: &Webhook, event: &str, payload: &Json) {
        Self::deliver_with(&self.db, hook, event, payload).await;
    }

    /// [`Self::deliver_one`] 的 db 直通形态（spawn 'static 用）。
    async fn deliver_with(db: &Arc<Mutex<Connection>>, hook: &Webhook, event: &str, payload: &Json) {
        let delivery_id = format!("dl-{}", now_nanos());
        let body = match serde_json::to_vec(payload) {
            Ok(b) => b,
            Err(e) => {
                // payload 构造侧的 bug——记失败日志即返回（不影响其他钩子）
                record_delivery(db, hook, event, false, &format!("err: 序列化失败 {e}"), 0);
                return;
            }
        };
        let signature = sign_payload(&hook.secret, &body);
        let client = reqwest::Client::new();
        let mut attempts = 0u32;
        let mut status_text = String::new();
        let mut ok = false;
        while attempts <= DELIVERY_RETRIES {
            attempts += 1;
            let resp = client
                .post(&hook.url)
                .timeout(DELIVERY_TIMEOUT)
                .header("content-type", "application/json")
                .header("x-nexhub-event", event)
                .header("x-nexhub-delivery", &delivery_id)
                .header("x-nexhub-signature", &signature)
                .body(body.clone())
                .send()
                .await;
            match resp {
                Ok(r) if r.status().is_success() => {
                    status_text = r.status().as_u16().to_string();
                    ok = true;
                    break;
                }
                Ok(r) => {
                    status_text = format!("http {}", r.status().as_u16());
                }
                Err(e) => {
                    status_text = format!("err: {e}");
                }
            }
        }
        if !ok {
            eprintln!(
                "[webhook] 投递失败（{attempts} 次）: {} {} → {status_text}",
                hook.url, event
            );
        }
        record_delivery(db, hook, event, ok, &status_text, attempts);
    }
}

impl Default for WebhookService {
    fn default() -> Self {
        Self::new()
    }
}

// ----------------------------------------------------------------------------
// 查询辅助（短锁内执行）
// ----------------------------------------------------------------------------

/// 精确 repo 的钩子（`repo=?`）。
fn load_hooks(conn: &Connection, repo: &str) -> Vec<Webhook> {
    query_hooks(conn, "WHERE repo=? ORDER BY created_at, id", repo)
}

/// 匹配 repo 的钩子：仓库专属（repo=该仓）+ 全局（repo='*'）。
fn load_hooks_matching(conn: &Connection, repo: &str) -> Vec<Webhook> {
    query_hooks(
        conn,
        "WHERE repo=? OR repo='*' ORDER BY created_at, id",
        repo,
    )
}

/// 钩子查询公共实现（filter 片段含且仅含一个 `?` 绑定 repo；失败降级空 vec）。
fn query_hooks(conn: &Connection, filter: &str, repo: &str) -> Vec<Webhook> {
    let sql = format!(
        "SELECT id, repo, url, secret, events, enabled, created_at, last_delivery, last_status \
         FROM hub_webhooks {filter}"
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Vec::new();
    };
    let Ok(iter) = stmt.query_map(params![repo], webhook_from_row) else {
        return Vec::new();
    };
    iter.flatten().collect()
}

/// 钩子最近投递日志（最新在前，≤20 条）。
fn load_logs(conn: &Connection, hook_id: &str) -> rusqlite::Result<Vec<WebhookDelivery>> {
    let mut stmt = conn.prepare(
        "SELECT id, hook_id, event, url, ok, status, attempts, delivered_at \
         FROM hub_webhook_log WHERE hook_id=? ORDER BY id DESC LIMIT ?",
    )?;
    let iter = stmt.query_map(params![hook_id, LOG_RING_SIZE as i64], delivery_from_row)?;
    let mut out = Vec::new();
    for d in iter {
        out.push(d?);
    }
    Ok(out)
}

/// 写投递记录：更新钩子 last_delivery/last_status + 环形日志（保留最新 20 条）。
fn record_delivery(
    db: &Arc<Mutex<Connection>>,
    hook: &Webhook,
    event: &str,
    ok: bool,
    status: &str,
    attempts: u32,
) {
    let now = now_iso();
    let conn = db.lock().expect("db poisoned");
    let _ = conn.execute(
        "UPDATE hub_webhooks SET last_delivery=?, last_status=? WHERE id=?",
        params![now, status, hook.id],
    );
    let _ = conn.execute(
        "INSERT INTO hub_webhook_log (hook_id, event, url, ok, status, attempts, delivered_at) \
         VALUES (?,?,?,?,?,?,?)",
        params![hook.id, event, hook.url, ok as i64, status, attempts as i64, now],
    );
    // 环形裁剪：仅保留该钩子最新 LOG_RING_SIZE 条
    let _ = conn.execute(
        "DELETE FROM hub_webhook_log WHERE hook_id=? AND id NOT IN \
         (SELECT id FROM hub_webhook_log WHERE hook_id=? ORDER BY id DESC LIMIT ?)",
        params![hook.id, hook.id, LOG_RING_SIZE as i64],
    );
}

// ----------------------------------------------------------------------------
// 进程级全局槽 + 事件点 fire 函数（一行挂钩面）
// ----------------------------------------------------------------------------

/// 进程级共享槽（`CodeRepoRouteHandler::new()` 装配时安装；事件点读取）。
static SERVICE_SLOT: Mutex<Option<Arc<WebhookService>>> = Mutex::new(None);

/// 取全局服务（事件点用；未装配返回 None——webhook 面未启用时事件点 no-op）。
#[must_use]
pub fn global_service() -> Option<Arc<WebhookService>> {
    SERVICE_SLOT.lock().expect("webhook slot poisoned").clone()
}

/// 安装全局服务（main 装配链经由 `CodeRepoRouteHandler::new()` 调用）。
pub fn install_global_service(svc: Arc<WebhookService>) -> bool {
    svc.install_global()
}

/// push 事件（os-api git-http push 成功路径调用，一行挂钩）：
/// 后台解析默认分支 + 头提交摘要 → 构造 payload → 匹配投递。
///
/// ref 解析用 [`resolve_default_branch_sync`]（HTTP CGI 边界拿不到本次推送的
/// ref 列表——与 CI push_hook 同一信息边界；推送后默认分支即最新状态）。
pub fn fire_push(repo: &str, pusher: &str) {
    let Some(svc) = global_service() else {
        return;
    };
    let repo = repo.trim().trim_end_matches(".git").to_string();
    if validate_repo_name(&repo).is_err() {
        return;
    }
    let pusher = pusher.to_string();
    tokio::spawn(async move {
        let bare = format!("{}/{repo}.git", repos_dir());
        let resolved = tokio::task::spawn_blocking(move || {
            let branch = resolve_default_branch_sync(&bare);
            let git_ref = format!("refs/heads/{branch}");
            // 头提交摘要：`<short> <subject>`（失败降级空串，不阻塞投递）
            let out = std::process::Command::new("git")
                .arg(format!("--git-dir={bare}"))
                .args(["log", "-1", "--format=%h %s"])
                .output();
            let head = out
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();
            match head.split_once(' ') {
                Some((a, m)) => (git_ref, a.to_string(), m.to_string()),
                None => (git_ref, String::new(), String::new()),
            }
        })
        .await;
        let Ok((git_ref, after, message)) = resolved else {
            return;
        };
        let delivery = format!("dl-{}", now_nanos());
        let payload = build_push_payload(&repo, &git_ref, &after, &message, &pusher, &delivery);
        svc.dispatch_event(WebhookEvent::Push, &repo, payload).await;
    });
}

/// issue 事件（created / closed / reopened；issues.rs 写路径一行挂钩）。
pub fn fire_issue(repo: &str, action: &str, issue: &RepoIssue, sender: &str) {
    let Some(svc) = global_service() else {
        return;
    };
    // spawn 'static：全部入参先持有化（同 CI push_hook 的 fire-and-forget 模式）
    let (repo, action, sender) = (repo.to_string(), action.to_string(), sender.to_string());
    let issue = serde_json::to_value(issue).unwrap_or(Json::Null);
    tokio::spawn(async move {
        let delivery = format!("dl-{}", now_nanos());
        let payload = build_issue_payload(&action, &issue, &sender, &delivery);
        svc.dispatch_event(WebhookEvent::Issues, &repo, payload).await;
    });
}

/// issue/PR 评论事件（issues.rs 评论写路径一行挂钩；kind=issue|pull）。
pub fn fire_comment(repo: &str, kind: &str, parent_number: u64, comment: &RepoComment) {
    let Some(svc) = global_service() else {
        return;
    };
    let (repo, kind) = (repo.to_string(), kind.to_string());
    let comment = serde_json::to_value(comment).unwrap_or(Json::Null);
    tokio::spawn(async move {
        let delivery = format!("dl-{}", now_nanos());
        let sender = comment
            .get("author")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_string();
        let payload = build_comment_payload(&repo, &kind, parent_number, &comment, &sender, &delivery);
        let event = if kind == "pull" {
            WebhookEvent::Pr
        } else {
            WebhookEvent::Issues
        };
        svc.dispatch_event(event, &repo, payload).await;
    });
}

/// PR 事件（created / merged；merged 带 merged_sha）。
pub fn fire_pull(repo: &str, action: &str, pull: &RepoPull, sender: &str, merged_sha: Option<&str>) {
    let Some(svc) = global_service() else {
        return;
    };
    let (repo, action, sender) = (repo.to_string(), action.to_string(), sender.to_string());
    let pull = serde_json::to_value(pull).unwrap_or(Json::Null);
    let merged_sha = merged_sha.map(String::from);
    tokio::spawn(async move {
        let delivery = format!("dl-{}", now_nanos());
        let payload = build_pull_payload(&action, &pull, &sender, &delivery, merged_sha.as_deref());
        svc.dispatch_event(WebhookEvent::Pr, &repo, payload).await;
    });
}

/// release 事件（nexhub_lobby 发版成功路径一行挂钩）。
pub fn fire_release(release: &Release, sender: &str) {
    let Some(svc) = global_service() else {
        return;
    };
    let sender = sender.to_string();
    let release = serde_json::to_value(release).unwrap_or(Json::Null);
    let repo = release
        .get("repo_name")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string();
    tokio::spawn(async move {
        let delivery = format!("dl-{}", now_nanos());
        let payload = build_release_payload(&release, &sender, &delivery);
        svc.dispatch_event(WebhookEvent::Release, &repo, payload).await;
    });
}

// ----------------------------------------------------------------------------
// 路由声明与分发（挂在 code_repo 名下；全部网关 admin——钩子含 secret）
// ----------------------------------------------------------------------------

const COMPONENT: &str = "code_repo";

/// 命名空间认领：`repos/:name/webhooks/...` 或 `coderepo/webhooks/...`（全局）。
fn owns_namespace(segs: &[&str]) -> bool {
    segs.len() >= 4
        && segs[0] == "api"
        && segs[1] == "v1"
        && segs[2] == "coderepo"
        && ((segs[3] == "repos" && segs.len() >= 6 && segs[5] == "webhooks")
            || segs[3] == "webhooks")
}

/// 8 条路由 spec（component="code_repo"；全部 requires_auth + admin——管理面含
/// secret，不公开；与 issues 协作面的「handler 内自验」不同，本管理面无链上
/// 身份语义，纯系统 admin）。
pub fn route_specs() -> Vec<RouteSpec> {
    let admin = vec!["admin".to_string()];
    let mut out = Vec::new();
    for (method, suffix) in [
        (HttpMethod::Get, "/repos/:name/webhooks"),
        (HttpMethod::Post, "/repos/:name/webhooks"),
        (HttpMethod::Delete, "/repos/:name/webhooks/:id"),
        (HttpMethod::Post, "/repos/:name/webhooks/:id/toggle"),
        (HttpMethod::Get, "/webhooks"),
        (HttpMethod::Post, "/webhooks"),
        (HttpMethod::Delete, "/webhooks/:id"),
        (HttpMethod::Post, "/webhooks/:id/toggle"),
    ] {
        out.push(RouteSpec {
            method,
            path: format!("/api/v1/coderepo{suffix}"),
            handler_component: COMPONENT.to_string(),
            requires_auth: true,
            required_roles: admin.clone(),
        });
    }
    out
}

/// 建钩子请求体（events 数组或逗号串均收，agent/前端双友好）。
#[derive(Debug, Deserialize)]
struct CreateWebhookBody {
    url: String,
    #[serde(default)]
    secret: Option<String>,
    events: WebhookEventsInput,
}

/// events 入参：`["push","issues"]` 或 `"push,issues"`。
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WebhookEventsInput {
    List(Vec<String>),
    Plain(String),
}

impl WebhookEventsInput {
    fn to_vec(&self) -> Vec<String> {
        match self {
            WebhookEventsInput::List(v) => v.clone(),
            WebhookEventsInput::Plain(s) => s
                .split(['，', ','])
                .map(String::from)
                .collect(),
        }
    }
}

impl WebhookService {
    /// 路由分发入口：认领 webhooks 命名空间则处理并返回 `Some(响应)`；
    /// 否则 `None`（`CodeRepoRouteHandler::handle` 继续自己的 match）。
    pub(crate) async fn try_handle(
        &self,
        method: HttpMethod,
        path: &str,
        body: &Json,
    ) -> Option<Result<ApiResponse, HandlerError>> {
        let segs: Vec<&str> = path
            .split('?')
            .next()
            .unwrap_or(path)
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        if !owns_namespace(&segs) {
            return None;
        }
        Some(self.dispatch(method, &segs, body).await)
    }

    /// 命名空间内分发（owns_namespace 已保证形状；repo 面要求仓库存在）。
    async fn dispatch(
        &self,
        method: HttpMethod,
        segs: &[&str],
        body: &Json,
    ) -> Result<ApiResponse, HandlerError> {
        match (method, segs) {
            // ============ 仓库面（/api/v1/coderepo/repos/:name/webhooks...）============

            (HttpMethod::Get, ["api", "v1", "coderepo", "repos", name, "webhooks"]) => {
                if let Err(resp) = self.require_repo(name) {
                    return Ok(resp);
                }
                Ok(self.list_response(name))
            }
            (HttpMethod::Post, ["api", "v1", "coderepo", "repos", name, "webhooks"]) => {
                if let Err(resp) = self.require_repo(name) {
                    return Ok(resp);
                }
                Ok(self.create_response(name, body))
            }
            (
                HttpMethod::Delete,
                ["api", "v1", "coderepo", "repos", name, "webhooks", id],
            ) => Ok(self.delete_response(name, id)),
            (
                HttpMethod::Post,
                ["api", "v1", "coderepo", "repos", name, "webhooks", id, "toggle"],
            ) => Ok(self.toggle_response(name, id)),

            // ============ 全局面（/api/v1/coderepo/webhooks...，repo='*'）============

            (HttpMethod::Get, ["api", "v1", "coderepo", "webhooks"]) => {
                Ok(self.list_response("*"))
            }
            (HttpMethod::Post, ["api", "v1", "coderepo", "webhooks"]) => {
                Ok(self.create_response("*", body))
            }
            (HttpMethod::Delete, ["api", "v1", "coderepo", "webhooks", id]) => {
                Ok(self.delete_response("*", id))
            }
            (HttpMethod::Post, ["api", "v1", "coderepo", "webhooks", id, "toggle"]) => {
                Ok(self.toggle_response("*", id))
            }

            _ => Ok(error_response(404, "code_repo: 未匹配的 webhooks 路由")),
        }
    }

    /// 仓库面仓库存在性（404）。
    fn require_repo(&self, repo: &str) -> Result<(), ApiResponse> {
        if let Err(msg) = validate_repo_name(repo) {
            return Err(error_response(400, &msg));
        }
        let bare = format!("{}/{repo}.git", self.repos_root);
        if !Path::new(&bare).is_dir() {
            return Err(error_response(404, &format!("仓库不存在: {repo}")));
        }
        Ok(())
    }

    /// GET 列表响应（钩子 + 各自投递日志，secret 原样返回——管理面已 admin 门禁）。
    fn list_response(&self, repo: &str) -> ApiResponse {
        let webhooks: Vec<Json> = self
            .list(repo)
            .into_iter()
            .map(|(h, logs)| {
                serde_json::json!({ "webhook": h, "deliveries": logs })
            })
            .collect();
        ok_json(serde_json::json!({
            "repo": repo,
            "webhooks": webhooks,
        }))
    }

    /// POST 建钩子响应。
    fn create_response(&self, repo: &str, body: &Json) -> ApiResponse {
        let body: CreateWebhookBody = match serde_json::from_value(body.clone()) {
            Ok(b) => b,
            Err(e) => return error_response(400, &format!("解析 webhook 请求体失败: {e}")),
        };
        match self.create(
            repo,
            &body.url,
            body.secret.as_deref().unwrap_or_default(),
            &body.events.to_vec(),
        ) {
            Ok(hook) => ApiResponse {
                status: 201,
                body: serde_json::json!({ "ok": true, "webhook": hook }),
                headers: serde_json::json!({}),
            },
            Err((code, msg)) => error_response(code, &msg),
        }
    }

    /// DELETE 响应。
    fn delete_response(&self, scope: &str, id: &str) -> ApiResponse {
        match self.delete(scope, id) {
            Ok(()) => ok_json(serde_json::json!({ "ok": true, "id": id })),
            Err((code, msg)) => error_response(code, &msg),
        }
    }

    /// toggle 响应。
    fn toggle_response(&self, scope: &str, id: &str) -> ApiResponse {
        match self.toggle(scope, id) {
            Ok(enabled) => ok_json(serde_json::json!({
                "ok": true, "id": id, "enabled": enabled,
            })),
            Err((code, msg)) => error_response(code, &msg),
        }
    }
}

// ----------------------------------------------------------------------------
// 小工具（与 issues/code_repo 同款）
// ----------------------------------------------------------------------------

fn ok_json(body: Json) -> ApiResponse {
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

fn now_iso() -> String {
    chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- SHA-256 / HMAC 正确性（已知向量 + dev-dep sha2 交叉验证）----

    #[test]
    fn sha256_known_vectors() {
        // FIPS 180-4 / NIST 例向量
        assert_eq!(
            hex::encode(sha256_raw(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex::encode(sha256_raw(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex::encode(sha256_raw(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // >64 字节（跨块）与 >56 字节（单块 padding 边界）
        assert_eq!(
            hex::encode(sha256_raw(&[b'a'; 56])),
            "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a"
        );
        assert_eq!(
            hex::encode(sha256_raw(&[b'a'; 64])),
            "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"
        );
    }

    #[test]
    fn sha256_matches_reference_impl_on_random_inputs() {
        // 与 dev-dep sha2 crate 全面对照：长度 0..300（跨块/边界 padding 全扫）
        use sha2::Digest;
        let mut seed = 0x1234_5678u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for len in 0..300usize {
            let data: Vec<u8> = (0..len).map(|_| (next() & 0xff) as u8).collect();
            let mut h = sha2::Sha256::new();
            h.update(&data);
            assert_eq!(
                sha256_raw(&data)[..],
                h.finalize()[..],
                "长度 {len} 不一致"
            );
        }
    }

    #[test]
    fn hmac_sha256_known_vector() {
        // 已知向量（HMAC-SHA256, key="key", msg="The quick brown fox ..."）
        assert_eq!(
            hex::encode(hmac_sha256(
                b"key",
                b"The quick brown fox jumps over the lazy dog"
            )),
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
        // RFC 4231 Test Case 2
        assert_eq!(
            hex::encode(hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn hmac_sha256_long_key_is_hashed_first() {
        // key > 64 字节：先 hash 再进 ipad/opad（RFC 2104）；与手写 ipad 结构自洽
        let long_key = vec![b'k'; 100];
        let mac = hmac_sha256(&long_key, b"msg");
        // 手工复算同结构（防实现抄错）
        let k = sha256_raw(&long_key);
        let mut ipad = [0x36u8; 64];
        let mut opad = [0x5cu8; 64];
        for i in 0..32 {
            ipad[i] ^= k[i];
            opad[i] ^= k[i];
        }
        let mut inner = ipad.to_vec();
        inner.extend_from_slice(b"msg");
        let mut outer = opad.to_vec();
        outer.extend_from_slice(&sha256_raw(&inner));
        assert_eq!(mac, sha256_raw(&outer));
    }

    #[test]
    fn sign_payload_format() {
        let sig = sign_payload("s3cret", br#"{"a":1}"#);
        assert!(sig.starts_with("sha256="));
        assert_eq!(sig.len(), "sha256=".len() + 64);
        // 相同输入确定性；不同 secret 不同签名
        assert_eq!(sig, sign_payload("s3cret", br#"{"a":1}"#));
        assert_ne!(sig, sign_payload("other", br#"{"a":1}"#));
        // 空 secret 也可签名（验方按空串约定校验）
        assert!(sign_payload("", b"x").starts_with("sha256="));
    }

    // ---- payload 快照 ----

    /// 经 serde（`#[serde(default)]` 全字段兜底）构造 fixture——对 issues.rs
    /// 结构体演进（并行批次加字段）稳健，缺省字段走默认值。
    fn sample_issue() -> RepoIssue {
        serde_json::from_value(serde_json::json!({
            "repo": "demo", "number": 7, "title": "T", "body": "B",
            "author": "admin", "author_display": "admin", "owner_kind": "admin",
            "state": "open", "labels": ["bug"], "comment_count": 0,
            "created_at": "2026-09-24T10:00:00+08:00",
            "updated_at": "2026-09-24T10:00:00+08:00",
        }))
        .expect("RepoIssue fixture")
    }

    fn sample_pull() -> RepoPull {
        serde_json::from_value(serde_json::json!({
            "repo": "demo", "number": 3, "title": "P", "body": "",
            "from_branch": "feat", "to_branch": "main",
            "author": "admin", "author_display": "admin", "owner_kind": "admin",
            "state": "merged", "merged_by": "admin",
            "merged_at": "2026-09-24T11:00:00+08:00", "comment_count": 0,
            "created_at": "2026-09-24T10:30:00+08:00",
            "updated_at": "2026-09-24T11:00:00+08:00",
        }))
        .expect("RepoPull fixture")
    }

    #[test]
    fn payload_snapshots() {
        let p = build_push_payload("demo", "refs/heads/main", "abc1234", "fix: x", "oem", "dl-1");
        assert_eq!(p["event"], "push");
        assert_eq!(p["action"], "pushed");
        assert_eq!(p["repo"], "demo");
        assert_eq!(p["ref"], "refs/heads/main");
        assert_eq!(p["sender"], "oem");
        assert_eq!(p["delivery"], "dl-1");
        assert_eq!(p["push"]["after"], "abc1234");
        assert_eq!(p["push"]["message"], "fix: x");

        let issue = serde_json::to_value(sample_issue()).unwrap();
        let p = build_issue_payload("created", &issue, "admin", "dl-2");
        assert_eq!(p["event"], "issues");
        assert_eq!(p["action"], "created");
        assert_eq!(p["repo"], "demo");
        assert_eq!(p["issue"]["number"], 7);

        let p = build_pull_payload("merged", &serde_json::to_value(sample_pull()).unwrap(), "admin", "dl-3", Some("deadbeef"));
        assert_eq!(p["event"], "pr");
        assert_eq!(p["action"], "merged");
        assert_eq!(p["pull"]["from_branch"], "feat");
        assert_eq!(p["merged_sha"], "deadbeef");
        let p2 = build_pull_payload("created", &serde_json::to_value(sample_pull()).unwrap(), "admin", "dl-4", None);
        assert!(p2.get("merged_sha").is_none());

        let release = serde_json::json!({
            "id": "rl-1", "repo_name": "demo", "tag": "v1.0",
            "title": "v1.0", "notes": "first", "created_by": "admin",
            "created_at": "2026-09-24T12:00:00+08:00",
        });
        let p = build_release_payload(&release, "admin", "dl-5");
        assert_eq!(p["event"], "release");
        assert_eq!(p["action"], "published");
        assert_eq!(p["repo"], "demo");
        assert_eq!(p["release"]["tag"], "v1.0");

        let comment = serde_json::json!({
            "repo": "demo", "kind": "issue", "parent_number": 7, "number": 1,
            "author": "admin", "body": "+1", "created_at": "2026-09-24T10:05:00+08:00",
        });
        let p = build_comment_payload("demo", "issue", 7, &comment, "admin", "dl-6");
        assert_eq!(p["event"], "issues");
        assert_eq!(p["action"], "commented");
        assert_eq!(p["issue"]["number"], 7);
        assert_eq!(p["comment"]["body"], "+1");
        // kind=pull → pr 事件
        let p = build_comment_payload("demo", "pull", 3, &comment, "admin", "dl-7");
        assert_eq!(p["event"], "pr");
    }

    // ---- 事件匹配 ----

    #[test]
    fn events_match_handles_csv() {
        assert!(events_match("push,issues", "push"));
        assert!(events_match("push, issues", "issues"));
        assert!(!events_match("push", "issues"));
        assert!(!events_match("", "push"));
    }

    // ---- CRUD ----

    fn svc() -> WebhookService {
        WebhookService::in_memory()
    }

    #[test]
    fn crud_create_list_delete_toggle() {
        let s = svc();
        let h = s
            .create("demo", "http://127.0.0.1:9/hook", "sec", &["push".into(), "issues".into()])
            .unwrap();
        assert!(h.id.starts_with("wh-"));
        assert!(h.enabled);
        assert_eq!(h.events, vec!["push", "issues"]);
        let h2 = s.create("*", "http://127.0.0.1:9/global", "", &["release".into()]).unwrap();
        assert_eq!(h2.repo, "*");

        // list("demo") = 仓库专属 + 全局
        let listed = s.list("demo");
        assert_eq!(listed.len(), 2);
        // list("*") 仅全局
        assert_eq!(s.list("*").len(), 1);

        // toggle：翻转两次回原状态
        assert!(!s.toggle("demo", &h.id).unwrap());
        assert!(s.toggle("demo", &h.id).unwrap());
        // scope 隔离：demo 面删不掉别的仓的钩子 → 404
        assert_eq!(s.delete("other", &h.id).unwrap_err().0, 404);
        // 删除
        s.delete("demo", &h.id).unwrap();
        assert_eq!(s.list("demo").len(), 1);
        assert_eq!(s.delete("demo", &h.id).unwrap_err().0, 404);
    }

    #[test]
    fn create_validates_inputs() {
        let s = svc();
        // 非法 URL scheme
        assert_eq!(s.create("demo", "ftp://x", "", &["push".into()]).unwrap_err().0, 400);
        // 空事件
        assert_eq!(
            s.create("demo", "http://a/b", "", &[]).unwrap_err().0,
            400
        );
        // 非法事件名
        assert_eq!(
            s.create("demo", "http://a/b", "", &["pushx".into()]).unwrap_err().0,
            400
        );
        // 非法仓库名
        assert_eq!(
            s.create("../evil", "http://a/b", "", &["push".into()]).unwrap_err().0,
            400
        );
        // 事件去重 + 大小写规范化
        let h = s
            .create("demo", "http://a/b", "", &["Push".into(), "push".into(), " ".into()])
            .unwrap();
        assert_eq!(h.events, vec!["push"]);
    }

    // ---- HTTP 分发（webhooks 命名空间）----

    use os_common::gateway::ApiRequest;

    fn req(method: HttpMethod, path: &str, body: Json) -> ApiRequest {
        ApiRequest {
            method,
            path: path.into(),
            headers: serde_json::json!({}),
            body,
        }
    }

    async fn handle(s: &WebhookService, r: ApiRequest) -> ApiResponse {
        s.try_handle(r.method, &r.path, &r.body)
            .await
            .expect("webhooks 命名空间应被认领")
            .unwrap()
    }

    #[test]
    fn namespace_detection() {
        assert!(owns_namespace(
            &"/api/v1/coderepo/repos/demo/webhooks"
                .split('/')
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        ));
        assert!(owns_namespace(
            &"/api/v1/coderepo/webhooks"
                .split('/')
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        ));
        assert!(!owns_namespace(
            &"/api/v1/coderepo/repos/demo/issues"
                .split('/')
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        ));
    }

    #[test]
    fn route_specs_declare_eight_admin_routes() {
        let specs = route_specs();
        assert_eq!(specs.len(), 8, "应有 8 条路由: {specs:?}");
        assert!(specs.iter().all(|s| s.handler_component == "code_repo"));
        assert!(
            specs
                .iter()
                .all(|s| s.requires_auth && s.required_roles == vec!["admin".to_string()]),
            "webhook 管理面全部 admin（含 GET——钩子带 secret）: {specs:?}"
        );
    }

    #[tokio::test]
    async fn http_crud_flow_with_tempdir_repo() {
        let tmp = tempdir();
        // 全注入（DB + 仓库根），不读 env——规避与 code_repo 模块 env 测试的并行竞态
        let s = WebhookService::with_paths(&format!("{tmp}/webhooks.db"), &tmp);
        // 仓库不存在 → 404
        let resp = handle(
            &s,
            req(
                HttpMethod::Get,
                "/api/v1/coderepo/repos/nope/webhooks",
                Json::Null,
            ),
        )
        .await;
        assert_eq!(resp.status, 404);
        // 造仓库目录
        std::fs::create_dir_all(format!("{tmp}/demo.git")).unwrap();
        // 建
        let resp = handle(
            &s,
            req(
                HttpMethod::Post,
                "/api/v1/coderepo/repos/demo/webhooks",
                serde_json::json!({"url": "http://127.0.0.1:9/hook", "secret": "sec", "events": ["push","issues"]}),
            ),
        )
        .await;
        assert_eq!(resp.status, 201);
        let id = resp.body["webhook"]["id"].as_str().unwrap().to_string();
        // 列
        let resp = handle(
            &s,
            req(HttpMethod::Get, "/api/v1/coderepo/repos/demo/webhooks", Json::Null),
        )
        .await;
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["webhooks"].as_array().unwrap().len(), 1);
        // toggle
        let resp = handle(
            &s,
            req(
                HttpMethod::Post,
                &format!("/api/v1/coderepo/repos/demo/webhooks/{id}/toggle"),
                Json::Null,
            ),
        )
        .await;
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["enabled"], false);
        // 全局面：建 + 列
        let resp = handle(
            &s,
            req(
                HttpMethod::Post,
                "/api/v1/coderepo/webhooks",
                serde_json::json!({"url": "http://127.0.0.1:9/g", "events": "release"}),
            ),
        )
        .await;
        assert_eq!(resp.status, 201);
        let gid = resp.body["webhook"]["id"].as_str().unwrap().to_string();
        let resp = handle(
            &s,
            req(HttpMethod::Get, "/api/v1/coderepo/webhooks", Json::Null),
        )
        .await;
        assert_eq!(resp.body["webhooks"].as_array().unwrap().len(), 1);
        // 删全局
        let resp = handle(
            &s,
            req(
                HttpMethod::Delete,
                &format!("/api/v1/coderepo/webhooks/{gid}"),
                Json::Null,
            ),
        )
        .await;
        assert_eq!(resp.status, 200);
        // 未匹配路由兜底 404
        let resp = handle(
            &s,
            req(
                HttpMethod::Put,
                "/api/v1/coderepo/repos/demo/webhooks",
                Json::Null,
            ),
        )
        .await;
        assert_eq!(resp.status, 404);
    }

    // ---- fake HTTP 接收端（裸 TCP 手写 HTTP/1.1，无新依赖）----

    /// 一次被记录的请求。
    #[derive(Debug, Clone)]
    struct RecordedRequest {
        method: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl RecordedRequest {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }
    }

    /// 脚本化 fake 接收端：按收到的请求顺序返回 `script` 中的状态码（耗尽后 200）。
    async fn spawn_fake_receiver(
        script: Vec<u16>,
    ) -> (String, Arc<std::sync::Mutex<Vec<RecordedRequest>>>, tokio::sync::oneshot::Sender<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let recorded = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
        let rec = Arc::clone(&recorded);
        let script = Arc::new(std::sync::Mutex::new(script));
        tokio::spawn(async move {
            loop {
                let conn = tokio::select! {
                    c = listener.accept() => c,
                    _ = &mut rx => break,
                };
                let Ok((mut sock, _)) = conn else { continue };
                let rec = Arc::clone(&rec);
                let script = Arc::clone(&script);
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    // 读到头区结束
                    let header_end = loop {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
                            break pos;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
                    let content_length = head
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                        .and_then(|l| l.split(':').nth(1))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    // 读满 body
                    while buf.len() < header_end + 4 + content_length {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let body = buf[header_end + 4..].to_vec();
                    let mut lines = head.lines();
                    let request_line = lines.next().unwrap_or_default();
                    let method = request_line
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .to_string();
                    let headers: Vec<(String, String)> = lines
                        .filter_map(|l| l.split_once(':'))
                        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                        .collect();
                    rec.lock().unwrap().push(RecordedRequest {
                        method,
                        headers,
                        body,
                    });
                    let status = {
                        let mut s = script.lock().unwrap();
                        if s.is_empty() {
                            200
                        } else {
                            s.remove(0)
                        }
                    };
                    let reason = if status == 200 { "OK" } else { "Fail" };
                    let resp = format!("HTTP/1.1 {status} {reason}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        (format!("http://{addr}"), recorded, tx)
    }

    fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|w| w == needle)
    }

    /// 轮询等待收到 ≥n 个请求（超时 fail）。
    async fn wait_for_requests(
        recorded: &Arc<std::sync::Mutex<Vec<RecordedRequest>>>,
        n: usize,
    ) -> Vec<RecordedRequest> {
        for _ in 0..200 {
            {
                let r = recorded.lock().unwrap();
                if r.len() >= n {
                    return r.clone();
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("5s 内未收到 {n} 个请求");
    }

    #[tokio::test]
    async fn delivery_posts_signed_payload_and_verifies() {
        let (url, recorded, _tx) = spawn_fake_receiver(vec![]).await;
        let s = svc();
        let h = s.create("demo", &url, "topsecret", &["issues".into()]).unwrap();
        let issue = serde_json::to_value(sample_issue()).unwrap();
        let payload = build_issue_payload("created", &issue, "admin", "dl-test");
        s.deliver_one(&h, "issues", &payload).await;

        let reqs = wait_for_requests(&recorded, 1).await;
        let r = &reqs[0];
        assert_eq!(r.method, "POST");
        // 签名头可验证：X-NexHub-Signature = HMAC(secret, body bytes)
        let sig = r.header("x-nexhub-signature").expect("缺签名头");
        assert_eq!(sig, sign_payload("topsecret", &r.body));
        // 独立重算（模拟接收端验签）
        assert!(sig.starts_with("sha256="));
        // 事件/投递 id 头 + content-type
        assert_eq!(r.header("x-nexhub-event"), Some("issues"));
        assert!(r.header("x-nexhub-delivery").is_some());
        assert_eq!(
            r.header("content-type").map(str::to_ascii_lowercase),
            Some("application/json".to_string())
        );
        // body 是合法 JSON payload
        let body: Json = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(body["event"], "issues");
        assert_eq!(body["issue"]["number"], 7);
        // 投递记录：last_status=200 + 环形日志 1 条 ok
        let listed = s.list("demo");
        let (wh, logs) = &listed[0];
        assert_eq!(wh.last_status.as_deref(), Some("200"));
        assert!(wh.last_delivery.is_some());
        assert_eq!(logs.len(), 1);
        assert!(logs[0].ok);
        assert_eq!(logs[0].attempts, 1);
        assert_eq!(logs[0].event, "issues");
    }

    #[tokio::test]
    async fn delivery_retries_once_on_failure_then_succeeds() {
        // 第一次 500（触发重试），第二次 200（成功）→ attempts=2
        let (url, recorded, _tx) = spawn_fake_receiver(vec![500]).await;
        let s = svc();
        let h = s.create("demo", &url, "", &["push".into()]).unwrap();
        let payload = build_push_payload("demo", "refs/heads/main", "abc", "m", "oem", "dl-r");
        s.deliver_one(&h, "push", &payload).await;

        let reqs = wait_for_requests(&recorded, 2).await;
        assert_eq!(reqs.len(), 2, "失败应重试 1 次（共 2 次尝试）");
        // 两次投递的签名一致（同 body + 同 secret）
        assert_eq!(reqs[0].header("x-nexhub-signature"), reqs[1].header("x-nexhub-signature"));
        let listed = s.list("demo");
        let (wh, logs) = &listed[0];
        assert_eq!(wh.last_status.as_deref(), Some("200"));
        assert_eq!(logs[0].attempts, 2);
        assert!(logs[0].ok);
    }

    #[tokio::test]
    async fn delivery_gives_up_after_one_retry() {
        // 两次都 500 → 最终失败，attempts=2，last_status=http 500
        let (url, recorded, _tx) = spawn_fake_receiver(vec![500, 500]).await;
        let s = svc();
        let h = s.create("demo", &url, "", &["release".into()]).unwrap();
        let release = serde_json::json!({"repo_name": "demo", "tag": "v1"});
        let payload = build_release_payload(&release, "admin", "dl-f");
        s.deliver_one(&h, "release", &payload).await;

        let reqs = wait_for_requests(&recorded, 2).await;
        assert_eq!(reqs.len(), 2, "重试 1 次后不再尝试");
        let listed = s.list("demo");
        let (wh, logs) = &listed[0];
        assert_eq!(wh.last_status.as_deref(), Some("http 500"));
        assert!(!logs[0].ok);
        assert_eq!(logs[0].attempts, 2);
    }

    #[tokio::test]
    async fn dispatch_event_filters_by_enabled_repo_and_event() {
        let (url, recorded, _tx) = spawn_fake_receiver(vec![]).await;
        let s = svc();
        // 四个钩子：命中 / 停用 / 别的仓库 / 未订阅该事件
        s.create("demo", &url, "", &["issues".into()]).unwrap();
        let disabled = s.create("demo", &url, "", &["issues".into()]).unwrap();
        s.toggle("demo", &disabled.id).unwrap();
        s.create("other", &url, "", &["issues".into()]).unwrap();
        s.create("demo", &url, "", &["push".into()]).unwrap();
        // 全局钩子（repo='*'）订阅 issues → 也应收到 demo 的 issue 事件
        s.create("*", &url, "", &["issues".into()]).unwrap();

        let issue = serde_json::to_value(sample_issue()).unwrap();
        let payload = build_issue_payload("created", &issue, "admin", "dl-d");
        s.dispatch_event(WebhookEvent::Issues, "demo", payload).await;
        let reqs = wait_for_requests(&recorded, 2).await;
        assert_eq!(reqs.len(), 2, "仅「仓库专属启用 + 全局启用」两个钩子命中");
    }

    #[tokio::test]
    async fn ring_log_keeps_latest_twenty() {
        let (url, _recorded, _tx) = spawn_fake_receiver(vec![]).await;
        let s = svc();
        let h = s.create("demo", &url, "", &["push".into()]).unwrap();
        let payload = build_push_payload("demo", "r", "a", "m", "p", "dl");
        for _ in 0..25 {
            s.deliver_one(&h, "push", &payload).await;
        }
        let listed = s.list("demo");
        let (_, logs) = &listed[0];
        assert_eq!(logs.len(), LOG_RING_SIZE, "环形日志仅保留最新 {LOG_RING_SIZE} 条");
        // 最新在前（自增 id 降序）
        let ids: Vec<i64> = logs.iter().map(|l| l.id).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(ids, sorted);
    }

    // ---- fire_*（全局槽 no-op + 经槽投递端到端）----

    /// 全局槽互斥：fire_* 经进程级 SERVICE_SLOT 投递，两测试对槽的存在性假设
    /// 相反——并行执行会互踩（一个装槽/清槽时另一个正 fire）。tokio Mutex：
    /// fire_issue 测试需持锁跨 `.await`（std 锁跨 await 是 clippy
    /// `await_holding_lock` 禁区，同 code_repo ENV_LOCK 先例）。
    static SLOT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn fire_functions_noop_without_global_slot() {
        let _guard = SLOT_LOCK.lock().await;
        // 确保槽为空（前序测试可能残留）：fire_* 静默 no-op（不 panic、不投递）
        *SERVICE_SLOT.lock().unwrap() = None;
        fire_push("demo", "oem");
        fire_issue("demo", "created", &sample_issue(), "admin");
        fire_pull("demo", "created", &sample_pull(), "admin", None);
        fire_release(
            &Release {
                id: "rl".into(),
                repo_name: "demo".into(),
                tag: "v1".into(),
                title: "t".into(),
                notes: String::new(),
                created_by: "admin".into(),
                created_at: now_iso(),
            },
            "admin",
        );
    }

    #[tokio::test]
    async fn fire_issue_delivers_through_global_slot() {
        let _guard = SLOT_LOCK.lock().await;
        let (url, recorded, _tx) = spawn_fake_receiver(vec![]).await;
        // 独立服务 + 安装全局槽（模拟装配；SLOT_LOCK 保证本测试独占进程内 slot）
        let s = Arc::new(WebhookService::in_memory());
        s.clone().install_global();
        s.create("demo", &url, "slotsec", &["issues".into()]).unwrap();

        fire_issue("demo", "created", &sample_issue(), "admin");
        let reqs = wait_for_requests(&recorded, 1).await;
        let r = &reqs[0];
        assert_eq!(r.header("x-nexhub-event"), Some("issues"));
        assert_eq!(
            r.header("x-nexhub-signature"),
            Some(sign_payload("slotsec", &r.body).as_str())
        );
        let body: Json = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(body["action"], "created");
        // 清理全局槽（避免污染其他测试）
        *SERVICE_SLOT.lock().unwrap() = None;
    }

    // ---- 工具 ----

    fn tempdir() -> String {
        let p = std::env::temp_dir().join(format!(
            "os-webhooks-test-{}",
            now_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p.to_string_lossy().into_owned()
    }
}
