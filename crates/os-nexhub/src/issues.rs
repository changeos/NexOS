//! `IssuesService` —— 项目级 Issues + Pull Requests 协作层（2026-08-24 定稿）。
//!
//! 定位：给 NexHub 的**每个代码仓库**（`/api/v1/coderepo/repos/:name/*`）加上
//! GitHub 式 Issues / Pull Requests 交互——**没有更改权限的 agent 也能在项目上
//! 交流**：用自己的链上身份开 Issue、评论、提 PR；而 merge（=更改仓库内容）
//! 仍仅 admin / 仓库所有者可执行。文档 `docs/NEXHUB_ISSUES_PR.md`。
//!
//! # 与既有联邦大厅 PR（`nexhub_lobby::hub_pull_requests`）的关系
//!
//! **独立表、独立状态机，互不影响**：
//!
//! | 维度 | 大厅 PR（hub_pull_requests） | 本模块（hub_repo_issues/hub_repo_pulls） |
//! |------|------------------------------|------------------------------------------|
//! | 定位 | 联邦大厅条目的审核流（发布前把关） | 仓库维度的日常协作（issue 跟踪 + 代码合入） |
//! | 标识 | 全局 `pr-<nanos>` id | 每仓库自增 `number`（issues/pulls 各自独立序列） |
//! | 状态 | open/merged/rejected/closed | issue: open/closed；pull: open/merged/closed |
//! | 评论 | 无 | 有（hub_repo_comments，issue/pull 共用一张表按 kind 区分） |
//! | 分支 | base 定格为仓库默认分支 | to_branch 显式指定（缺省=仓库实际默认分支） |
//!
//! 复用（不复制）：merge 执行 = [`crate::nexhub_lobby::merge_pr_blocking`]
//! （裸仓 merge-tree 3-way + commit-tree 双 parent + update-ref，冲突 409）；
//! diff 摘要与分支名校验同源复用；仓库 owner 判定与大厅 PR 审核同一权威——
//! **大厅发布索引 `hub_lobby.publisher`**（publisher 为 pubkey 且同 pubkey 才是
//! owner；未发布/平台托管条目 → 仅 admin 可 merge）。
//!
//! # 身份与权限模型（同大厅 publish 契约，docs/MEDIA_GEN_AND_CHAIN_AUTH.md §C）
//!
//! 身份解析顺序（全部写端点，服务端反查、body 自报一律忽略）：
//! ① nexhub 链上 token（`/api/v1/nexhub/auth/challenge|verify` 三步签发，24h）
//! → ② 系统 admin token（`NEXOS_ADMIN_TOKEN`/`OS_ADMIN_TOKEN`）→ ③ 皆非 401。
//! 响应恒带 `owner_kind`（pubkey/admin）标记作者身份类别。
//!
//! | 操作 | 链上身份（pubkey） | admin |
//! |------|--------------------|-------|
//! | 开 Issue / 评论 / 提 PR | ✅（author=pubkey 归因） | ✅（author="admin"） |
//! | 关闭/重开 Issue、关闭 PR | 仅 author 同 pubkey | ✅ |
//! | merge PR | 仅仓库 owner（大厅 publisher 同 pubkey） | ✅ |
//! | 读（列表/详情/评论） | 公开 | 公开 |
//!
//! # 路由表（12 条，挂在 code_repo 组件名下，前缀 /api/v1/coderepo/repos/:name）
//!
//! | method | path | 动作 | 权限 |
//! |--------|------|------|------|
//! | GET    | `/issues` | Issue 列表（`?state=open|closed|all`，默认 open）| 公开 |
//! | POST   | `/issues` | 建 Issue `{title, body?, labels?}`（number 自动分配）| 身份 |
//! | GET    | `/issues/:num` | 详情（含评论流 + comment_count）| 公开 |
//! | POST   | `/issues/:num/comments` | 评论 `{body}` | 身份 |
//! | POST   | `/issues/:num/close` | 关闭（仅 author/admin）| 身份 |
//! | POST   | `/issues/:num/open` | 重开（仅 author/admin）| 身份 |
//! | GET    | `/pulls` | PR 列表（`?state=open|merged|closed|all`，默认 open）| 公开 |
//! | POST   | `/pulls` | 建 PR `{title, body?, from_branch, to_branch?}`（from 分支须已 push 到裸仓）| 身份 |
//! | GET    | `/pulls/:num` | 详情（含评论流 + `git diff to..from --stat`）| 公开 |
//! | POST   | `/pulls/:num/comments` | PR 评论 `{body}` | 身份 |
//! | POST   | `/pulls/:num/merge` | 合并（仅 admin/仓库 owner；merge-tree 落地）| 身份 |
//! | POST   | `/pulls/:num/close` | 关闭（仅 author/admin）| 身份 |
//!
//! 全部 `requires_auth=false`（handler 内自验链上 token / admin 回落——同
//! nexhub-lobby 模式，网关中间件不拦链上身份调用方）。
//!
//! # 链上身份共享（token 与大厅互通）
//!
//! `/api/v1/nexhub/auth/*` 签发的 token 必须在本模块可验——装配层（os-api
//! main.rs）经 `NexHubLobbyRouteHandler::with_chain_auth` 注入共享 `Arc<ChainAuth>`
//! 时，lobby 顺手把它注册进本模块的进程级共享槽（[`register_shared_chain_auth`]）；
//! 本模块请求时经 [`resolve_chain_auth`] 取用。槽未注册（独立部署/单测）时回落
//! 进程内惰性默认实例（token 域独立，需另行签发——测试经
//! [`IssuesService::with_chain_auth`] 显式注入绕开）。

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use os_common::chain_auth::{self, ChainAuth};
use os_common::gateway::{ApiResponse, HandlerError, HttpMethod, RouteSpec};

use crate::code_repo::{
    branch_exists_sync, repos_dir, resolve_default_branch_sync, validate_repo_name,
};
use crate::nexhub_lobby::{
    default_db_path as lobby_db_path_default, merge_pr_blocking, pr_diff_stat_blocking,
    validate_branch_name,
};

// ----------------------------------------------------------------------------
// 常量：路由路径（code_repo 组件名下）
// ----------------------------------------------------------------------------

const COMPONENT: &str = "code_repo";
const PREFIX: &str = "/api/v1/coderepo/repos/:name";

/// 内容长度上限（agent 生成内容护栏：标题一行、正文/评论一篇）。
const MAX_TITLE_CHARS: usize = 500;
const MAX_BODY_CHARS: usize = 20_000;
/// 标签上限（每 Issue）与单标签长度上限。
const MAX_LABELS: usize = 10;
const MAX_LABEL_CHARS: usize = 60;

// ----------------------------------------------------------------------------
// 共享链上身份槽（lobby 装配时注册 → 本模块请求时解析）
// ----------------------------------------------------------------------------

/// 进程级共享 ChainAuth 槽（os-api main.rs 装配 `with_chain_auth` 时注册）。
static SHARED_CHAIN_AUTH: Mutex<Option<Arc<ChainAuth>>> = Mutex::new(None);

/// 槽未注册时的进程内惰性默认实例（token 域独立；生产装配总会先注册）。
static FALLBACK_CHAIN_AUTH: OnceLock<Arc<ChainAuth>> = OnceLock::new();

/// 注册共享链上身份存储（lobby `with_chain_auth` 装配路径调用；重复注册后者胜）。
pub fn register_shared_chain_auth(auth: Arc<ChainAuth>) {
    *SHARED_CHAIN_AUTH.lock().expect("shared auth poisoned") = Some(auth);
}

/// 解析当前生效的链上身份存储：已注册槽 → 槽内实例；否则惰性默认实例。
fn resolve_chain_auth() -> Arc<ChainAuth> {
    if let Some(a) = SHARED_CHAIN_AUTH
        .lock()
        .expect("shared auth poisoned")
        .clone()
    {
        return a;
    }
    FALLBACK_CHAIN_AUTH
        .get_or_init(|| Arc::new(ChainAuth::new()))
        .clone()
}

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 单条 Issue（hub_repo_issues 行 + comment_count 投影）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoIssue {
    /// 仓库名（裸仓 `<repo>.git`）。
    pub repo: String,
    /// Issue 编号（**每仓库独立自增**，1 起）。
    pub number: u64,
    /// 标题。
    pub title: String,
    /// 正文（可空）。
    #[serde(default)]
    pub body: String,
    /// 作者（链上 pubkey 或 `"admin"`；服务端 token 反查，自报忽略）。
    pub author: String,
    /// 作者展示名（pubkey 派生 EVM 地址；admin 为 `"admin"`）。
    #[serde(default)]
    pub author_display: String,
    /// 作者身份类别：`pubkey`（链上身份）/ `admin`（系统 admin）。
    #[serde(default)]
    pub owner_kind: String,
    /// 状态：open / closed。
    #[serde(default = "default_open")]
    pub state: String,
    /// 标签（存储为逗号串，API 以数组交互）。
    #[serde(default)]
    pub labels: Vec<String>,
    /// 评论数（详情/列表均带，列表 UI 用）。
    #[serde(default)]
    pub comment_count: u64,
    /// 创建时间（RFC3339）。
    pub created_at: String,
    /// 更新时间（评论/关闭/重开都会刷新）。
    pub updated_at: String,
}

/// 单条评论（hub_repo_comments 行；issue 与 pull 共用一张表，kind 区分）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoComment {
    /// 仓库名。
    pub repo: String,
    /// 评论类别：`issue` / `pull`。
    pub kind: String,
    /// 父对象编号（Issue 或 PR 的 number）。
    pub parent_number: u64,
    /// 评论编号（每 (仓库,类别,父) 内自增，1 起）。
    pub number: u64,
    /// 作者（pubkey 或 `"admin"`）。
    pub author: String,
    /// 作者展示名。
    #[serde(default)]
    pub author_display: String,
    /// 作者身份类别：pubkey / admin。
    #[serde(default)]
    pub owner_kind: String,
    /// 评论正文。
    pub body: String,
    /// 创建时间（RFC3339）。
    pub created_at: String,
}

/// 单条项目级 PR（hub_repo_pulls 行 + comment_count 投影）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoPull {
    /// 仓库名。
    pub repo: String,
    /// PR 编号（**每仓库独立自增**，与 issue 序列互不影响）。
    pub number: u64,
    /// 标题。
    pub title: String,
    /// 描述（可空）。
    #[serde(default)]
    pub body: String,
    /// 来源分支（须已 push 到裸仓，创建时校验）。
    pub from_branch: String,
    /// 目标分支（缺省=仓库实际默认分支，main→master 回退同快照逻辑）。
    pub to_branch: String,
    /// 作者（pubkey 或 `"admin"`）。
    pub author: String,
    /// 作者展示名。
    #[serde(default)]
    pub author_display: String,
    /// 作者身份类别：pubkey / admin。
    #[serde(default)]
    pub owner_kind: String,
    /// 状态：open / merged / closed。
    #[serde(default = "default_open")]
    pub state: String,
    /// 合并执行者（未合并为空；pubkey 或 "admin"）。
    #[serde(default)]
    pub merged_by: String,
    /// 合并时间（未合并为空）。
    #[serde(default)]
    pub merged_at: String,
    /// 评论数。
    #[serde(default)]
    pub comment_count: u64,
    /// 创建时间（RFC3339）。
    pub created_at: String,
    /// 更新时间。
    pub updated_at: String,
}

fn default_open() -> String {
    "open".to_string()
}

// ----------------------------------------------------------------------------
// 身份（Caller）：token 反查 pubkey / admin 回落（同 lobby 契约）
// ----------------------------------------------------------------------------

/// 已认证的项目协作调用方（`Authorization: Bearer` 解析结果）。
enum Caller {
    /// 链上身份：issue/PR author 归因到该 pubkey。
    Pubkey {
        pubkey: String,
        /// 展示名（pubkey 派生 EVM 地址）。
        display_name: String,
    },
    /// 系统 admin（平台管理通道）。
    Admin,
}

impl Caller {
    /// 归因标识（写库的 author 值）：pubkey 身份 → pubkey；admin → `"admin"`。
    fn actor(&self) -> &str {
        match self {
            Caller::Pubkey { pubkey, .. } => pubkey,
            Caller::Admin => "admin",
        }
    }

    /// 是否为链上 pubkey 身份（非 admin）。
    fn pubkey(&self) -> Option<&str> {
        match self {
            Caller::Pubkey { pubkey, .. } => Some(pubkey),
            Caller::Admin => None,
        }
    }

    /// 展示名。
    fn display(&self) -> &str {
        match self {
            Caller::Pubkey { display_name, .. } => display_name,
            Caller::Admin => "admin",
        }
    }

    /// 身份类别标记（响应 owner_kind）。
    fn owner_kind(&self) -> &'static str {
        match self {
            Caller::Pubkey { .. } => "pubkey",
            Caller::Admin => "admin",
        }
    }
}

// ----------------------------------------------------------------------------
// 请求体
// ----------------------------------------------------------------------------

/// 标签入参：数组 `["bug","ui"]` 或逗号串 `"bug,ui"` 均可（agent 直发 curl 友好）。
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LabelsInput {
    List(Vec<String>),
    Plain(String),
}

impl LabelsInput {
    /// 规范化为标签数组：trim、去空、限量（10 个 × 60 字符）。
    fn normalize(&self) -> Vec<String> {
        let raw: Vec<String> = match self {
            LabelsInput::List(v) => v.clone(),
            LabelsInput::Plain(s) => s.split(['，', ',']).map(String::from).collect(),
        };
        raw.into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .take(MAX_LABELS)
            .map(|s| s.chars().take(MAX_LABEL_CHARS).collect())
            .collect()
    }
}

/// POST /issues 请求体。
#[derive(Debug, Deserialize)]
struct CreateIssueBody {
    title: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    labels: Option<LabelsInput>,
}

/// POST /pulls 请求体。
#[derive(Debug, Deserialize)]
struct CreatePullBody {
    title: String,
    #[serde(default)]
    body: Option<String>,
    from_branch: String,
    #[serde(default)]
    to_branch: Option<String>,
}

/// 评论请求体（issue / pull 共用）。
#[derive(Debug, Deserialize)]
struct CommentBody {
    body: String,
}

// ----------------------------------------------------------------------------
// SQLite 持久化层（Mutex<Connection> 短锁快查快放，同 lobby 模式）
// ----------------------------------------------------------------------------

/// Issue 行字段序（INSERT/SELECT 共用；comment_count 由子查询拼出）。
const ISSUE_COLUMNS: &str =
    "repo_name,number,title,body,author,author_display,state,labels,created_at,updated_at";
/// Pull 行字段序。
const PULL_COLUMNS: &str = "repo_name,number,title,body,from_branch,to_branch,author,\
     author_display,state,merged_by,merged_at,created_at,updated_at";
/// 评论行字段序。
const COMMENT_COLUMNS: &str =
    "repo_name,kind,parent_number,number,author,author_display,body,created_at";

fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS hub_repo_issues (
            repo_name      TEXT NOT NULL,
            number         INTEGER NOT NULL,
            title          TEXT NOT NULL,
            body           TEXT DEFAULT '',
            author         TEXT NOT NULL,
            author_display TEXT DEFAULT '',
            state          TEXT DEFAULT 'open',
            labels         TEXT DEFAULT '',
            created_at     TEXT NOT NULL,
            updated_at     TEXT NOT NULL,
            PRIMARY KEY (repo_name, number)
        );
        CREATE INDEX IF NOT EXISTS idx_repo_issues_state ON hub_repo_issues(repo_name, state);
        CREATE TABLE IF NOT EXISTS hub_repo_pulls (
            repo_name      TEXT NOT NULL,
            number         INTEGER NOT NULL,
            title          TEXT NOT NULL,
            body           TEXT DEFAULT '',
            from_branch    TEXT NOT NULL,
            to_branch      TEXT NOT NULL,
            author         TEXT NOT NULL,
            author_display TEXT DEFAULT '',
            state          TEXT DEFAULT 'open',
            merged_by      TEXT DEFAULT '',
            merged_at      TEXT DEFAULT '',
            created_at     TEXT NOT NULL,
            updated_at     TEXT NOT NULL,
            PRIMARY KEY (repo_name, number)
        );
        CREATE INDEX IF NOT EXISTS idx_repo_pulls_state ON hub_repo_pulls(repo_name, state);
        CREATE TABLE IF NOT EXISTS hub_repo_comments (
            repo_name      TEXT NOT NULL,
            kind           TEXT NOT NULL,
            parent_number  INTEGER NOT NULL,
            number         INTEGER NOT NULL,
            author         TEXT NOT NULL,
            author_display TEXT DEFAULT '',
            body           TEXT NOT NULL,
            created_at     TEXT NOT NULL,
            PRIMARY KEY (repo_name, kind, parent_number, number)
        );",
    )
}

fn labels_from_row(raw: Option<String>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn owner_kind_of(author: &str) -> String {
    if chain_auth::parse_pubkey(author).is_some() {
        "pubkey".to_string()
    } else {
        "admin".to_string()
    }
}

fn issue_from_row(row: &rusqlite::Row) -> rusqlite::Result<RepoIssue> {
    let author: String = row.get(4)?;
    Ok(RepoIssue {
        repo: row.get(0)?,
        number: row.get(1)?,
        title: row.get(2)?,
        body: row.get(3)?,
        author_display: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
        state: row.get(6)?,
        labels: labels_from_row(row.get(7)?),
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
        owner_kind: owner_kind_of(&author),
        author,
        comment_count: 0,
    })
}

fn pull_from_row(row: &rusqlite::Row) -> rusqlite::Result<RepoPull> {
    let author: String = row.get(6)?;
    Ok(RepoPull {
        repo: row.get(0)?,
        number: row.get(1)?,
        title: row.get(2)?,
        body: row.get(3)?,
        from_branch: row.get(4)?,
        to_branch: row.get(5)?,
        author_display: row.get::<_, Option<String>>(7)?.unwrap_or_default(),
        state: row.get(8)?,
        merged_by: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
        merged_at: row.get::<_, Option<String>>(10)?.unwrap_or_default(),
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
        owner_kind: owner_kind_of(&author),
        author,
        comment_count: 0,
    })
}

fn comment_from_row(row: &rusqlite::Row) -> rusqlite::Result<RepoComment> {
    let author: String = row.get(4)?;
    Ok(RepoComment {
        repo: row.get(0)?,
        kind: row.get(1)?,
        parent_number: row.get(2)?,
        number: row.get(3)?,
        author_display: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
        body: row.get(6)?,
        created_at: row.get(7)?,
        owner_kind: owner_kind_of(&author),
        author,
    })
}

/// 分配下一个编号（每仓库维度；调用方须已持 db 锁——Mutex 保证进程内串行）。
fn next_number(conn: &Connection, table: &str, repo: &str) -> rusqlite::Result<u64> {
    let sql = format!("SELECT COALESCE(MAX(number), 0) + 1 FROM {table} WHERE repo_name=?");
    conn.query_row(&sql, params![repo], |r| r.get::<_, i64>(0))
        .map(|n| n.max(1) as u64)
}

fn save_issue(conn: &Connection, i: &RepoIssue) -> rusqlite::Result<()> {
    conn.execute(
        &format!(
            "INSERT OR REPLACE INTO hub_repo_issues ({ISSUE_COLUMNS}) \
             VALUES (?,?,?,?,?,?,?,?,?,?)"
        ),
        params![
            i.repo,
            i.number as i64,
            i.title,
            i.body,
            i.author,
            i.author_display,
            i.state,
            i.labels.join(","),
            i.created_at,
            i.updated_at,
        ],
    )?;
    Ok(())
}

fn save_pull(conn: &Connection, p: &RepoPull) -> rusqlite::Result<()> {
    conn.execute(
        &format!(
            "INSERT OR REPLACE INTO hub_repo_pulls ({PULL_COLUMNS}) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)"
        ),
        params![
            p.repo,
            p.number as i64,
            p.title,
            p.body,
            p.from_branch,
            p.to_branch,
            p.author,
            p.author_display,
            p.state,
            p.merged_by,
            p.merged_at,
            p.created_at,
            p.updated_at,
        ],
    )?;
    Ok(())
}

// ----------------------------------------------------------------------------
// IssuesService
// ----------------------------------------------------------------------------

/// 项目级 Issues + Pull Requests 协作服务——SQLite 状态机（issue/PR/评论）+
/// 系统 git（分支存在性 / diff 摘要 / merge-tree 合并，全部复用 lobby 实现）。
///
/// 挂在 `CodeRepoRouteHandler` 名下（component="code_repo"），经
/// [`IssuesService::try_handle`] 参与路由分发；构造时定格仓库根目录与
/// hub_lobby.db 路径（owner 判定的权威数据源），测试经 [`IssuesService::with_paths`]
/// 注入临时路径隔离（不读 env，规避并行测试竞态）。
pub struct IssuesService {
    /// 协作数据（hub_repo_issues / hub_repo_pulls / hub_repo_comments）。
    db: Arc<Mutex<Connection>>,
    /// 仓库根目录（裸仓 `<repo>.git` 的父目录，构造定格）。
    repos_root: String,
    /// hub_lobby.db 路径（仓库 owner 判定：hub_lobby.publisher 为 pubkey 才是 owner）。
    lobby_db_path: String,
    /// hub_lobby 只读连接（惰性打开；打开/查询失败降级 None → admin-only merge）。
    lobby_conn: Mutex<Option<Connection>>,
    /// 系统 admin token（`NEXOS_ADMIN_TOKEN`/`OS_ADMIN_TOKEN`，构造定格）。
    admin_token: Option<String>,
    /// 测试注入的链上身份存储（缺省请求时走共享槽 → 惰性默认实例）。
    pinned_auth: Option<Arc<ChainAuth>>,
}

impl IssuesService {
    /// 生产构造：默认 DB 路径（三级回退，见 [`Self::default_db_path`]）+
    /// `code_repo::repos_dir()` + lobby 默认 DB 路径 + env admin token。
    /// 文件库打开失败降级内存库（eprintln 提示，不 panic——同 lobby 模式）。
    #[must_use]
    pub fn new() -> Self {
        let path = Self::default_db_path();
        let conn = Connection::open(&path).and_then(|c| {
            let _ = c.pragma_update(None, "journal_mode", "WAL");
            create_schema(&c).map(|_| c)
        });
        let db = match conn {
            Ok(c) => Arc::new(Mutex::new(c)),
            Err(e) => {
                eprintln!("coderepo-issues: 打开 SQLite {path} 失败（{e}），降级到内存库");
                let c = Connection::open_in_memory().expect("内存库必成功");
                create_schema(&c).expect("建表必成功");
                Arc::new(Mutex::new(c))
            }
        };
        Self {
            db,
            repos_root: repos_dir(),
            lobby_db_path: lobby_db_path_default(),
            lobby_conn: Mutex::new(None),
            admin_token: Self::admin_token_from_env(),
            pinned_auth: None,
        }
    }

    /// 测试构造：指定协作 DB / lobby DB / 仓库根（全注入，不读 env）。
    #[must_use]
    pub fn with_paths(issues_db: &str, lobby_db: &str, repos_root: &str) -> Self {
        let conn = Connection::open(issues_db).and_then(|c| {
            let _ = c.pragma_update(None, "journal_mode", "WAL");
            create_schema(&c).map(|_| c)
        });
        let db = match conn {
            Ok(c) => Arc::new(Mutex::new(c)),
            Err(e) => {
                eprintln!("coderepo-issues: 打开 SQLite {issues_db} 失败（{e}），降级到内存库");
                let c = Connection::open_in_memory().expect("内存库必成功");
                create_schema(&c).expect("建表必成功");
                Arc::new(Mutex::new(c))
            }
        };
        Self {
            db,
            repos_root: repos_root.to_string(),
            lobby_db_path: lobby_db.to_string(),
            lobby_conn: Mutex::new(None),
            admin_token: Self::admin_token_from_env(),
            pinned_auth: None,
        }
    }

    /// 内存库构造（`CodeRepoRouteHandler::with_empty` 旧测试路径：零文件副作用）。
    #[must_use]
    pub fn in_memory() -> Self {
        let conn = Connection::open_in_memory().expect("内存库必成功");
        create_schema(&conn).expect("建表必成功");
        Self {
            db: Arc::new(Mutex::new(conn)),
            repos_root: repos_dir(),
            lobby_db_path: lobby_db_path_default(),
            lobby_conn: Mutex::new(None),
            admin_token: Self::admin_token_from_env(),
            pinned_auth: None,
        }
    }

    /// 注入系统 admin token（链式构造器，测试绕 env 竞态）。
    #[must_use]
    pub fn with_admin_token(mut self, token: &str) -> Self {
        self.admin_token = Some(token.to_string());
        self
    }

    /// 注入链上身份存储（链式构造器，测试定格 token 域；生产走共享槽）。
    #[must_use]
    pub fn with_chain_auth(mut self, auth: Arc<ChainAuth>) -> Self {
        self.pinned_auth = Some(auth);
        self
    }

    /// 默认 DB 路径：优先 `/tank/os-data/repo_issues.db`，再 `/var/lib/os/repo_issues.db`，
    /// 最后 `./repo_issues.db`（与 lobby 的 default_db_path 同模式；独立文件——
    /// 协作数据与大厅发布索引互不干扰，锁域分离）。
    fn default_db_path() -> String {
        for p in &["/tank/os-data/repo_issues.db", "/var/lib/os/repo_issues.db"] {
            if Path::new(p)
                .parent()
                .is_some_and(|d| d.exists() || std::fs::create_dir_all(d).is_ok())
            {
                return (*p).to_string();
            }
        }
        "./repo_issues.db".to_string()
    }

    /// 系统 admin token（env）：`NEXOS_ADMIN_TOKEN` 优先回退 `OS_ADMIN_TOKEN`，
    /// 构造时定格（同 lobby 语义；测试经 [`Self::with_admin_token`] 注入）。
    fn admin_token_from_env() -> Option<String> {
        std::env::var("NEXOS_ADMIN_TOKEN")
            .or_else(|_| std::env::var("OS_ADMIN_TOKEN"))
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
    }

    /// 裸仓库路径（`<repos_root>/<repo>.git`）。
    fn bare_of(&self, repo: &str) -> String {
        format!("{}/{repo}.git", self.repos_root)
    }

    /// 解析调用方身份：链上 token（pinned → 共享槽 → 惰性默认）→ admin 回落。
    fn caller(&self, headers: &Json) -> Option<Caller> {
        let token = chain_auth::bearer_token(headers)?;
        let auth = self.pinned_auth.clone().unwrap_or_else(resolve_chain_auth);
        if let Some(pubkey) = auth.verify_token(token) {
            let vk = chain_auth::parse_pubkey(&pubkey)?;
            return Some(Caller::Pubkey {
                pubkey,
                display_name: chain_auth::derive_display_name(&vk),
            });
        }
        if self.admin_token.as_deref() == Some(token) {
            return Some(Caller::Admin);
        }
        None
    }

    /// 仓库 owner pubkey（merge 权限判定的数据源）：读 hub_lobby 发布索引，
    /// `publisher` 为合法压缩公钥 → Some(pubkey)；无条目/平台托管条目/读库失败
    /// → None（此时仅 admin 可 merge——安全默认降级）。连接惰性打开复用。
    fn repo_owner_pubkey(&self, repo: &str) -> Option<String> {
        let mut guard = self.lobby_conn.lock().expect("lobby conn poisoned");
        if guard.is_none() {
            // 打不开（文件不存在等）→ 永久降级 admin-only（本进程内不再重试；
            // lobby 与本服务同进程同用户，正常运行时文件必然可开）
            *guard = Connection::open(&self.lobby_db_path).ok();
        }
        let conn = guard.as_ref()?;
        let publisher: Option<String> = conn
            .query_row(
                "SELECT publisher FROM hub_lobby WHERE repo_name=?",
                params![repo],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten();
        let p = publisher?;
        chain_auth::parse_pubkey(&p).is_some().then_some(p)
    }

    /// 校验仓库名并确认裸仓存在 → Ok(bare 路径)；Err(响应) 400/404。
    fn require_repo(&self, repo: &str) -> Result<String, ApiResponse> {
        if let Err(msg) = validate_repo_name(repo) {
            return Err(error_response(400, &msg));
        }
        let bare = self.bare_of(repo);
        if !Path::new(&bare).is_dir() {
            return Err(error_response(404, &format!("仓库不存在: {repo}")));
        }
        Ok(bare)
    }
}

impl Default for IssuesService {
    fn default() -> Self {
        Self::new()
    }
}

// ----------------------------------------------------------------------------
// 路由声明与分发（挂在 code_repo 名下；全部 requires_auth=false，handler 内自验）
// ----------------------------------------------------------------------------

/// 本模块路由是否认领该路径段（`repos/:name/issues|pulls/...` 命名空间）。
fn owns_namespace(segs: &[&str]) -> bool {
    segs.len() >= 6
        && segs[0] == "api"
        && segs[1] == "v1"
        && segs[2] == "coderepo"
        && segs[3] == "repos"
        && (segs[5] == "issues" || segs[5] == "pulls")
}

/// 12 条路由 spec（component="code_repo"；读公开、写 handler 内自验身份）。
pub fn route_specs() -> Vec<RouteSpec> {
    let mut out = Vec::new();
    for (method, suffix) in [
        (HttpMethod::Get, "/issues"),
        (HttpMethod::Post, "/issues"),
        (HttpMethod::Get, "/issues/:num"),
        (HttpMethod::Post, "/issues/:num/comments"),
        (HttpMethod::Post, "/issues/:num/close"),
        (HttpMethod::Post, "/issues/:num/open"),
        (HttpMethod::Get, "/pulls"),
        (HttpMethod::Post, "/pulls"),
        (HttpMethod::Get, "/pulls/:num"),
        (HttpMethod::Post, "/pulls/:num/comments"),
        (HttpMethod::Post, "/pulls/:num/merge"),
        (HttpMethod::Post, "/pulls/:num/close"),
    ] {
        out.push(RouteSpec {
            method,
            path: format!("{PREFIX}{suffix}"),
            handler_component: COMPONENT.to_string(),
            requires_auth: false,
            required_roles: vec![],
        });
    }
    out
}

impl IssuesService {
    /// 路由分发入口：认领 issues/pulls 命名空间则处理并返回 `Some(响应)`；
    /// 否则 `None`（`CodeRepoRouteHandler::handle` 继续自己的 match）。
    pub(crate) async fn try_handle(
        &self,
        method: HttpMethod,
        path: &str,
        headers: &Json,
        body: &Json,
    ) -> Option<Result<ApiResponse, HandlerError>> {
        let segs = path_segments(path);
        if !owns_namespace(&segs) {
            return None;
        }
        let query = query_params(path);
        Some(self.dispatch(method, &segs, &query, headers, body).await)
    }

    /// 命名空间内分发（owns_namespace 已保证 segs 形状合法，未知组合兜底 404）。
    async fn dispatch(
        &self,
        method: HttpMethod,
        segs: &[&str],
        query: &std::collections::HashMap<String, String>,
        headers: &Json,
        body: &Json,
    ) -> Result<ApiResponse, HandlerError> {
        match (method, segs) {
            // ============ Issues ============
            (HttpMethod::Get, ["api", "v1", "coderepo", "repos", repo, "issues"]) => {
                Ok(self.list_issues(repo, query))
            }
            (HttpMethod::Post, ["api", "v1", "coderepo", "repos", repo, "issues"]) => {
                Ok(self.create_issue(repo, headers, body))
            }
            (HttpMethod::Get, ["api", "v1", "coderepo", "repos", repo, "issues", num]) => {
                self.with_num(num, |n| Ok(self.issue_detail(repo, n)))
            }
            (
                HttpMethod::Post,
                ["api", "v1", "coderepo", "repos", repo, "issues", num, "comments"],
            ) => self.with_num(num, |n| {
                Ok(self.add_comment("issue", repo, n, headers, body))
            }),
            (
                HttpMethod::Post,
                ["api", "v1", "coderepo", "repos", repo, "issues", num, "close"],
            ) => self.with_num(num, |n| Ok(self.set_issue_state(repo, n, false, headers))),
            (HttpMethod::Post, ["api", "v1", "coderepo", "repos", repo, "issues", num, "open"]) => {
                self.with_num(num, |n| Ok(self.set_issue_state(repo, n, true, headers)))
            }

            // ============ Pull Requests ============
            (HttpMethod::Get, ["api", "v1", "coderepo", "repos", repo, "pulls"]) => {
                Ok(self.list_pulls(repo, query))
            }
            (HttpMethod::Post, ["api", "v1", "coderepo", "repos", repo, "pulls"]) => {
                self.create_pull(repo, headers, body).await
            }
            (HttpMethod::Get, ["api", "v1", "coderepo", "repos", repo, "pulls", num]) => {
                self.with_num_async(num, |n| self.pull_detail(repo, n))
                    .await
            }
            (
                HttpMethod::Post,
                ["api", "v1", "coderepo", "repos", repo, "pulls", num, "comments"],
            ) => self.with_num(
                num,
                |n| Ok(self.add_comment("pull", repo, n, headers, body)),
            ),
            (HttpMethod::Post, ["api", "v1", "coderepo", "repos", repo, "pulls", num, "merge"]) => {
                self.with_num_async(num, |n| self.merge_pull(repo, n, headers))
                    .await
            }
            (HttpMethod::Post, ["api", "v1", "coderepo", "repos", repo, "pulls", num, "close"]) => {
                self.with_num(num, |n| Ok(self.close_pull(repo, n, headers)))
            }

            // 命名空间内未覆盖组合 → 404
            _ => Ok(error_response(404, "code_repo: 未匹配的 issues/pulls 路由")),
        }
    }

    /// `:num` 段解析（同步处理器包装）：非数字 → 400。
    fn with_num(
        &self,
        num: &str,
        f: impl FnOnce(u64) -> Result<ApiResponse, HandlerError>,
    ) -> Result<ApiResponse, HandlerError> {
        match num.parse::<u64>() {
            Ok(n) if n > 0 => f(n),
            _ => Ok(error_response(400, &format!("编号非法（正整数）: {num}"))),
        }
    }

    /// `:num` 段解析（异步处理器包装）：非正整数 → 400。
    async fn with_num_async<F, Fut>(&self, num: &str, f: F) -> Result<ApiResponse, HandlerError>
    where
        F: FnOnce(u64) -> Fut,
        Fut: std::future::Future<Output = Result<ApiResponse, HandlerError>>,
    {
        match num.parse::<u64>() {
            Ok(n) if n > 0 => f(n).await,
            _ => Ok(error_response(400, &format!("编号非法（正整数）: {num}"))),
        }
    }

    // ------------------------------------------------------------------------
    // Issues
    // ------------------------------------------------------------------------

    /// GET /issues（公开）：`?state=open|closed|all`，默认 open；创建序倒排。
    fn list_issues(
        &self,
        repo: &str,
        query: &std::collections::HashMap<String, String>,
    ) -> ApiResponse {
        if let Err(resp) = self.require_repo(repo) {
            return resp;
        }
        let state = normalize_state(query.get("state").map(String::as_str), &["open", "closed"]);
        let Some(state) = state else {
            return error_response(400, "非法 state（可选 open/closed/all，默认 open）");
        };
        let conn = self.db.lock().expect("db poisoned");
        let issues = load_issues(&conn, repo, &state).unwrap_or_default();
        ok_json(serde_json::json!({ "repo": repo, "state": state, "issues": issues }))
    }

    /// POST /issues（需身份）：title 必填（≤500），body/labels 可选；number 自动分配。
    fn create_issue(&self, repo: &str, headers: &Json, body: &Json) -> ApiResponse {
        let Some(caller) = self.caller(headers) else {
            return auth_required();
        };
        if let Err(resp) = self.require_repo(repo) {
            return resp;
        }
        let body: CreateIssueBody = match serde_json::from_value(body.clone()) {
            Ok(b) => b,
            Err(e) => return error_response(400, &format!("解析创建 Issue 请求体失败: {e}")),
        };
        let title = body.title.trim().to_string();
        if title.is_empty() {
            return error_response(400, "Issue 标题不得为空");
        }
        if title.chars().count() > MAX_TITLE_CHARS {
            return error_response(400, &format!("标题过长（≤{MAX_TITLE_CHARS} 字符）"));
        }
        let text = body.body.unwrap_or_default().trim().to_string();
        if text.chars().count() > MAX_BODY_CHARS {
            return error_response(400, &format!("正文过长（≤{MAX_BODY_CHARS} 字符）"));
        }
        let labels = body.labels.map(|l| l.normalize()).unwrap_or_default();
        let now = now_iso();
        let issue = RepoIssue {
            repo: repo.to_string(),
            number: 0,
            title,
            body: text,
            author: caller.actor().to_string(),
            author_display: caller.display().to_string(),
            owner_kind: caller.owner_kind().to_string(),
            state: "open".to_string(),
            labels,
            comment_count: 0,
            created_at: now.clone(),
            updated_at: now,
        };
        let conn = self.db.lock().expect("db poisoned");
        let number = match next_number(&conn, "hub_repo_issues", repo) {
            Ok(n) => n,
            Err(e) => return error_response(500, &format!("分配编号失败: {e}")),
        };
        let issue = RepoIssue { number, ..issue };
        if let Err(e) = save_issue(&conn, &issue) {
            return error_response(500, &format!("写入 Issue 失败: {e}"));
        }
        ApiResponse {
            status: 201,
            body: serde_json::json!({ "ok": true, "issue": issue }),
            headers: serde_json::json!({}),
        }
    }

    /// GET /issues/:num（公开）：详情 + 评论流 + comment_count。
    fn issue_detail(&self, repo: &str, num: u64) -> ApiResponse {
        if let Err(resp) = self.require_repo(repo) {
            return resp;
        }
        let (issue, comments) = {
            let conn = self.db.lock().expect("db poisoned");
            let issue = match find_issue(&conn, repo, num) {
                Ok(Some(i)) => i,
                Ok(None) => return error_response(404, &format!("Issue 不存在: #{num}")),
                Err(e) => return error_response(500, &format!("数据库错误: {e}")),
            };
            let comments = load_comments(&conn, repo, "issue", num).unwrap_or_default();
            (issue, comments)
        };
        let count = comments.len() as u64;
        ok_json(serde_json::json!({
            "issue": RepoIssue { comment_count: count, ..issue },
            "comments": comments,
        }))
    }

    /// POST /issues/:num/comments（需身份）：正文必填；评论同时刷新父对象 updated_at。
    fn add_comment(
        &self,
        kind: &str,
        repo: &str,
        parent: u64,
        headers: &Json,
        body: &Json,
    ) -> ApiResponse {
        let Some(caller) = self.caller(headers) else {
            return auth_required();
        };
        if let Err(resp) = self.require_repo(repo) {
            return resp;
        }
        let body: CommentBody = match serde_json::from_value(body.clone()) {
            Ok(b) => b,
            Err(e) => return error_response(400, &format!("解析评论请求体失败: {e}")),
        };
        let text = body.body.trim().to_string();
        if text.is_empty() {
            return error_response(400, "评论正文不得为空");
        }
        if text.chars().count() > MAX_BODY_CHARS {
            return error_response(400, &format!("评论过长（≤{MAX_BODY_CHARS} 字符）"));
        }
        let conn = self.db.lock().expect("db poisoned");
        // 父对象存在性（404）
        let parent_exists = match kind {
            "issue" => find_issue(&conn, repo, parent).map(|o| o.is_some()),
            _ => find_pull(&conn, repo, parent).map(|o| o.is_some()),
        };
        match parent_exists {
            Ok(true) => {}
            Ok(false) => {
                let what = if kind == "issue" { "Issue" } else { "PR" };
                return error_response(404, &format!("{what} 不存在: #{parent}"));
            }
            Err(e) => return error_response(500, &format!("数据库错误: {e}")),
        }
        let number = match next_comment_number(&conn, repo, kind, parent) {
            Ok(n) => n,
            Err(e) => return error_response(500, &format!("分配编号失败: {e}")),
        };
        let comment = RepoComment {
            repo: repo.to_string(),
            kind: kind.to_string(),
            parent_number: parent,
            number,
            author: caller.actor().to_string(),
            author_display: caller.display().to_string(),
            owner_kind: caller.owner_kind().to_string(),
            body: text,
            created_at: now_iso(),
        };
        if let Err(e) = insert_comment(&conn, &comment) {
            return error_response(500, &format!("写入评论失败: {e}"));
        }
        // 评论刷新父对象 updated_at（列表「最近活跃」排序的基础数据）
        let touch = if kind == "issue" {
            conn.execute(
                "UPDATE hub_repo_issues SET updated_at=? WHERE repo_name=? AND number=?",
                params![comment.created_at, repo, parent as i64],
            )
        } else {
            conn.execute(
                "UPDATE hub_repo_pulls SET updated_at=? WHERE repo_name=? AND number=?",
                params![comment.created_at, repo, parent as i64],
            )
        };
        if let Err(e) = touch {
            return error_response(500, &format!("刷新更新时间失败: {e}"));
        }
        ApiResponse {
            status: 201,
            body: serde_json::json!({ "ok": true, "comment": comment }),
            headers: serde_json::json!({}),
        }
    }

    /// POST /issues/:num/close | /open（需身份）：仅 author 本人或 admin；状态机
    /// 校验（open 才能 close，closed 才能 open），非法流转 409。
    fn set_issue_state(&self, repo: &str, num: u64, open: bool, headers: &Json) -> ApiResponse {
        let Some(caller) = self.caller(headers) else {
            return auth_required();
        };
        if let Err(resp) = self.require_repo(repo) {
            return resp;
        }
        let mut issue = {
            let conn = self.db.lock().expect("db poisoned");
            match find_issue(&conn, repo, num) {
                Ok(Some(i)) => i,
                Ok(None) => return error_response(404, &format!("Issue 不存在: #{num}")),
                Err(e) => return error_response(500, &format!("数据库错误: {e}")),
            }
        };
        // 权限：admin 恒可；链上身份须与 author 同 pubkey（admin 建的 Issue 对
        // 链上身份关闭 → 403，与大厅 PR close 同语义）
        let allowed = match caller.pubkey() {
            Some(pk) => issue.author == pk,
            None => true,
        };
        if !allowed {
            return error_response(403, "仅 Issue 作者或 admin 可关闭/重开该 Issue");
        }
        let target = if open { "open" } else { "closed" };
        if issue.state == target {
            return error_response(409, &format!("Issue 已是 {target} 状态"));
        }
        issue.state = target.to_string();
        issue.updated_at = now_iso();
        {
            let conn = self.db.lock().expect("db poisoned");
            if let Err(e) = save_issue(&conn, &issue) {
                return error_response(500, &format!("写入 Issue 失败: {e}"));
            }
        }
        ok_json(serde_json::json!({
            "ok": true,
            "repo": repo,
            "number": num,
            "state": target,
            "by": caller.actor(),
        }))
    }

    // ------------------------------------------------------------------------
    // Pull Requests
    // ------------------------------------------------------------------------

    /// GET /pulls（公开）：`?state=open|merged|closed|all`，默认 open。
    fn list_pulls(
        &self,
        repo: &str,
        query: &std::collections::HashMap<String, String>,
    ) -> ApiResponse {
        if let Err(resp) = self.require_repo(repo) {
            return resp;
        }
        let state = normalize_state(
            query.get("state").map(String::as_str),
            &["open", "merged", "closed"],
        );
        let Some(state) = state else {
            return error_response(400, "非法 state（可选 open/merged/closed/all，默认 open）");
        };
        let conn = self.db.lock().expect("db poisoned");
        let pulls = load_pulls(&conn, repo, &state).unwrap_or_default();
        ok_json(serde_json::json!({ "repo": repo, "state": state, "pulls": pulls }))
    }

    /// POST /pulls（需身份）：from_branch 必须已 push 到裸仓（git rev-parse 校验）；
    /// to_branch 缺省=仓库实际默认分支（main→master 回退）；from≠to；两端都须存在。
    async fn create_pull(
        &self,
        repo: &str,
        headers: &Json,
        body: &Json,
    ) -> Result<ApiResponse, HandlerError> {
        let Some(caller) = self.caller(headers) else {
            return Ok(auth_required());
        };
        let bare = match self.require_repo(repo) {
            Ok(b) => b,
            Err(resp) => return Ok(resp),
        };
        let body: CreatePullBody = match serde_json::from_value(body.clone()) {
            Ok(b) => b,
            Err(e) => return Ok(error_response(400, &format!("解析创建 PR 请求体失败: {e}"))),
        };
        let title = body.title.trim().to_string();
        if title.is_empty() {
            return Ok(error_response(400, "PR 标题不得为空"));
        }
        if title.chars().count() > MAX_TITLE_CHARS {
            return Ok(error_response(
                400,
                &format!("标题过长（≤{MAX_TITLE_CHARS} 字符）"),
            ));
        }
        let from_branch = body.from_branch.trim().to_string();
        if let Err(msg) = validate_branch_name(&from_branch) {
            return Ok(error_response(400, &msg));
        }
        let to_branch = match body
            .to_branch
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(b) => b.to_string(),
            // 缺省=仓库实际默认分支（建仓即 main；存量 master 仓回退 master，
            // 避免目标分支不存在的坑——比硬编码 main 更贴合存量仓库）
            None => {
                let b = bare.clone();
                tokio::task::spawn_blocking(move || resolve_default_branch_sync(&b))
                    .await
                    .map_err(|e| HandlerError::Internal(format!("默认分支探测 join 失败: {e}")))?
            }
        };
        if let Err(msg) = validate_branch_name(&to_branch) {
            return Ok(error_response(400, &msg));
        }
        if from_branch == to_branch {
            return Ok(error_response(400, "from_branch 与 to_branch 不能相同"));
        }
        // 分支存在性（一次 blocking 任务查两端）
        let (from_ok, to_ok) = {
            let (b, f, t) = (bare.clone(), from_branch.clone(), to_branch.clone());
            tokio::task::spawn_blocking(move || {
                (branch_exists_sync(&b, &f), branch_exists_sync(&b, &t))
            })
            .await
            .map_err(|e| HandlerError::Internal(format!("分支校验 join 失败: {e}")))?
        };
        if !from_ok {
            return Ok(error_response(
                400,
                &format!("from_branch 在仓库中不存在（先 git push 到裸仓）: {from_branch}"),
            ));
        }
        if !to_ok {
            return Ok(error_response(
                400,
                &format!("to_branch 在仓库中不存在: {to_branch}"),
            ));
        }
        let text = body.body.unwrap_or_default().trim().to_string();
        if text.chars().count() > MAX_BODY_CHARS {
            return Ok(error_response(
                400,
                &format!("描述过长（≤{MAX_BODY_CHARS} 字符）"),
            ));
        }
        let now = now_iso();
        let pull = RepoPull {
            repo: repo.to_string(),
            number: 0,
            title,
            body: text,
            from_branch,
            to_branch,
            author: caller.actor().to_string(),
            author_display: caller.display().to_string(),
            owner_kind: caller.owner_kind().to_string(),
            state: "open".to_string(),
            merged_by: String::new(),
            merged_at: String::new(),
            comment_count: 0,
            created_at: now.clone(),
            updated_at: now,
        };
        let saved = {
            let conn = self.db.lock().expect("db poisoned");
            match next_number(&conn, "hub_repo_pulls", repo) {
                Ok(n) => {
                    let pull = RepoPull { number: n, ..pull };
                    if let Err(e) = save_pull(&conn, &pull) {
                        return Ok(error_response(500, &format!("写入 PR 失败: {e}")));
                    }
                    pull
                }
                Err(e) => return Ok(error_response(500, &format!("分配编号失败: {e}"))),
            }
        };
        Ok(ApiResponse {
            status: 201,
            body: serde_json::json!({ "ok": true, "pull": saved }),
            headers: serde_json::json!({}),
        })
    }

    /// GET /pulls/:num（公开）：详情 + 评论流 + diff 摘要（分支被删/仓库移除 →
    /// 空串降级，详情仍可看，同 lobby 契约）。
    async fn pull_detail(&self, repo: &str, num: u64) -> Result<ApiResponse, HandlerError> {
        if let Err(resp) = self.require_repo(repo) {
            return Ok(resp);
        }
        let (pull, comments) = {
            let conn = self.db.lock().expect("db poisoned");
            let pull = match find_pull(&conn, repo, num) {
                Ok(Some(p)) => p,
                Ok(None) => return Ok(error_response(404, &format!("PR 不存在: #{num}"))),
                Err(e) => return Ok(error_response(500, &format!("数据库错误: {e}"))),
            };
            let comments = load_comments(&conn, repo, "pull", num).unwrap_or_default();
            (pull, comments)
        };
        let count = comments.len() as u64;
        // diff 摘要（分支被删/仓库移除 → 空串降级，详情仍可看，同 lobby 契约）
        let bare = self.bare_of(repo);
        let stat = if Path::new(&bare).is_dir() {
            let (b, t, f) = (bare, pull.to_branch.clone(), pull.from_branch.clone());
            tokio::task::spawn_blocking(move || pr_diff_stat_blocking(&b, &t, &f))
                .await
                .map_err(|e| HandlerError::Internal(format!("diff 任务 join 失败: {e}")))?
        } else {
            String::new()
        };
        Ok(ok_json(serde_json::json!({
            "pull": RepoPull { comment_count: count, ..pull },
            "comments": comments,
            "diff_stat": stat,
        })))
    }

    /// POST /pulls/:num/merge（需身份）：**仅 admin 或仓库 owner**（owner 判定以
    /// 大厅发布索引为权威：hub_lobby.publisher=pubkey 且同 pubkey）——merge 即
    /// 更改仓库内容，没有更改权限的 agent 不能执行。执行复用 lobby 的裸仓
    /// merge-tree（3-way + commit-tree 双 parent + update-ref）；冲突 409。
    async fn merge_pull(
        &self,
        repo: &str,
        num: u64,
        headers: &Json,
    ) -> Result<ApiResponse, HandlerError> {
        let Some(caller) = self.caller(headers) else {
            return Ok(auth_required());
        };
        let bare = match self.require_repo(repo) {
            Ok(b) => b,
            Err(resp) => return Ok(resp),
        };
        let mut pull = {
            let conn = self.db.lock().expect("db poisoned");
            match find_pull(&conn, repo, num) {
                Ok(Some(p)) => p,
                Ok(None) => return Ok(error_response(404, &format!("PR 不存在: #{num}"))),
                Err(e) => return Ok(error_response(500, &format!("数据库错误: {e}"))),
            }
        };
        // 权限：admin 恒可；链上身份须为仓库 owner（大厅 publisher 同 pubkey）
        let allowed = match caller.pubkey() {
            Some(pk) => self.repo_owner_pubkey(repo).as_deref() == Some(pk),
            None => true,
        };
        if !allowed {
            return Ok(error_response(
                403,
                "仅 admin 或仓库所有者可合并该 PR（merge 即更改权限——先把仓库发布到大厅并归因你的链上身份，或联系管理员）",
            ));
        }
        if pull.state != "open" {
            return Ok(error_response(
                409,
                &format!("仅 open 状态可合并（当前 {}）", pull.state),
            ));
        }
        let message = format!("Merge PR #{}: {}", pull.number, pull.title);
        let (m_bare, m_to, m_from, m_msg) = (
            bare.clone(),
            pull.to_branch.clone(),
            pull.from_branch.clone(),
            message,
        );
        let merged =
            tokio::task::spawn_blocking(move || merge_pr_blocking(&m_bare, &m_to, &m_from, &m_msg))
                .await
                .map_err(|e| HandlerError::Internal(format!("合并任务 join 失败: {e}")))?;
        let merged_sha = match merged {
            Ok(sha) => sha,
            Err(e) => {
                return Ok(if e.starts_with("合并冲突") {
                    error_response(409, &e)
                } else {
                    error_response(502, &e)
                })
            }
        };
        let now = now_iso();
        pull.state = "merged".to_string();
        pull.merged_by = caller.actor().to_string();
        pull.merged_at = now.clone();
        pull.updated_at = now;
        {
            let conn = self.db.lock().expect("db poisoned");
            if let Err(e) = save_pull(&conn, &pull) {
                return Ok(error_response(500, &format!("写入 PR 失败: {e}")));
            }
        }
        Ok(ok_json(serde_json::json!({
            "ok": true,
            "repo": repo,
            "number": num,
            "state": "merged",
            "merged_by": pull.merged_by,
            "merged_at": pull.merged_at,
            "merged_sha": merged_sha,
        })))
    }

    /// POST /pulls/:num/close（需身份）：仅 author 本人或 admin；仅 open 可关闭。
    fn close_pull(&self, repo: &str, num: u64, headers: &Json) -> ApiResponse {
        let Some(caller) = self.caller(headers) else {
            return auth_required();
        };
        if let Err(resp) = self.require_repo(repo) {
            return resp;
        }
        let mut pull = {
            let conn = self.db.lock().expect("db poisoned");
            match find_pull(&conn, repo, num) {
                Ok(Some(p)) => p,
                Ok(None) => return error_response(404, &format!("PR 不存在: #{num}")),
                Err(e) => return error_response(500, &format!("数据库错误: {e}")),
            }
        };
        let allowed = match caller.pubkey() {
            Some(pk) => pull.author == pk,
            None => true,
        };
        if !allowed {
            return error_response(403, "仅 PR 作者或 admin 可关闭该 PR");
        }
        if pull.state != "open" {
            return error_response(409, &format!("仅 open 状态可关闭（当前 {}）", pull.state));
        }
        pull.state = "closed".to_string();
        pull.updated_at = now_iso();
        {
            let conn = self.db.lock().expect("db poisoned");
            if let Err(e) = save_pull(&conn, &pull) {
                return error_response(500, &format!("写入 PR 失败: {e}"));
            }
        }
        ok_json(serde_json::json!({
            "ok": true,
            "repo": repo,
            "number": num,
            "state": "closed",
            "closed_by": caller.actor(),
        }))
    }
}

// ----------------------------------------------------------------------------
// 查询辅助（纯 DB 操作，短锁内执行）
// ----------------------------------------------------------------------------

/// 状态过滤规范化：缺省/空 → `all` 之外的首状态（默认 open）；`all` → `all`；
/// 非法 → None（调用方 400）。
fn normalize_state(raw: Option<&str>, allowed: &[&str]) -> Option<String> {
    let s = raw.unwrap_or("open").trim();
    if s.is_empty() {
        return Some("open".to_string());
    }
    if s == "all" {
        return Some("all".to_string());
    }
    allowed.contains(&s).then(|| s.to_string())
}

fn load_issues(conn: &Connection, repo: &str, state: &str) -> rusqlite::Result<Vec<RepoIssue>> {
    let comment_count = "(SELECT COUNT(*) FROM hub_repo_comments c \
         WHERE c.repo_name=i.repo_name AND c.kind='issue' AND c.parent_number=i.number)";
    let mut sql = format!(
        "SELECT {ISSUE_COLUMNS}, {comment_count} AS comment_count \
         FROM hub_repo_issues i WHERE repo_name=?"
    );
    let mut bind: Vec<String> = vec![repo.to_string()];
    if state != "all" {
        sql.push_str(" AND state=?");
        bind.push(state.to_string());
    }
    sql.push_str(" ORDER BY number DESC");
    let mut stmt = conn.prepare(&sql)?;
    let iter = stmt.query_map(params_from_iter(bind.iter()), |row| {
        let mut issue = issue_from_row(row)?;
        issue.comment_count = row.get(10)?;
        Ok(issue)
    })?;
    let mut out = Vec::new();
    for i in iter {
        out.push(i?);
    }
    Ok(out)
}

fn find_issue(conn: &Connection, repo: &str, num: u64) -> rusqlite::Result<Option<RepoIssue>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ISSUE_COLUMNS} FROM hub_repo_issues WHERE repo_name=? AND number=?"
    ))?;
    stmt.query_row(params![repo, num as i64], issue_from_row)
        .optional()
}

fn load_pulls(conn: &Connection, repo: &str, state: &str) -> rusqlite::Result<Vec<RepoPull>> {
    let comment_count = "(SELECT COUNT(*) FROM hub_repo_comments c \
         WHERE c.repo_name=p.repo_name AND c.kind='pull' AND c.parent_number=p.number)";
    let mut sql = format!(
        "SELECT {PULL_COLUMNS}, {comment_count} AS comment_count \
         FROM hub_repo_pulls p WHERE repo_name=?"
    );
    let mut bind: Vec<String> = vec![repo.to_string()];
    if state != "all" {
        sql.push_str(" AND state=?");
        bind.push(state.to_string());
    }
    sql.push_str(" ORDER BY number DESC");
    let mut stmt = conn.prepare(&sql)?;
    let iter = stmt.query_map(params_from_iter(bind.iter()), |row| {
        let mut pull = pull_from_row(row)?;
        pull.comment_count = row.get(13)?;
        Ok(pull)
    })?;
    let mut out = Vec::new();
    for p in iter {
        out.push(p?);
    }
    Ok(out)
}

fn find_pull(conn: &Connection, repo: &str, num: u64) -> rusqlite::Result<Option<RepoPull>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {PULL_COLUMNS} FROM hub_repo_pulls WHERE repo_name=? AND number=?"
    ))?;
    stmt.query_row(params![repo, num as i64], pull_from_row)
        .optional()
}

/// 评论编号分配：每 (repo, kind, parent) 维度自增（与 issue/pull 主键序列独立）。
fn next_comment_number(
    conn: &Connection,
    repo: &str,
    kind: &str,
    parent: u64,
) -> rusqlite::Result<u64> {
    conn.query_row(
        "SELECT COALESCE(MAX(number), 0) + 1 FROM hub_repo_comments \
         WHERE repo_name=? AND kind=? AND parent_number=?",
        params![repo, kind, parent as i64],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n.max(1) as u64)
}

fn insert_comment(conn: &Connection, c: &RepoComment) -> rusqlite::Result<()> {
    conn.execute(
        &format!(
            "INSERT INTO hub_repo_comments ({COMMENT_COLUMNS}) \
             VALUES (?,?,?,?,?,?,?,?)"
        ),
        params![
            c.repo,
            c.kind,
            c.parent_number as i64,
            c.number as i64,
            c.author,
            c.author_display,
            c.body,
            c.created_at,
        ],
    )?;
    Ok(())
}

fn load_comments(
    conn: &Connection,
    repo: &str,
    kind: &str,
    parent: u64,
) -> rusqlite::Result<Vec<RepoComment>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COMMENT_COLUMNS} FROM hub_repo_comments \
         WHERE repo_name=? AND kind=? AND parent_number=? ORDER BY number ASC"
    ))?;
    let iter = stmt.query_map(params![repo, kind, parent as i64], comment_from_row)?;
    let mut out = Vec::new();
    for c in iter {
        out.push(c?);
    }
    Ok(out)
}

// ----------------------------------------------------------------------------
// 响应与解析辅助（与 code_repo / nexhub_lobby 同款小工具，模块自足）
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

/// 统一 401：写端点缺/无效身份（文案与 lobby 一致，引导三步认证）。
fn auth_required() -> ApiResponse {
    error_response(
        401,
        "需要 Authorization: Bearer <nexhub token>（先 POST /api/v1/nexhub/auth/challenge + /auth/verify）或系统 admin token",
    )
}

fn now_iso() -> String {
    chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

/// 剥离 `?query` 的路径段（前后空段去除）。
fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

/// 解析 query string 为 HashMap（%XX + `+` 解码，同 code_repo）。
fn query_params(path: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    if let Some(q) = path.split('?').nth(1) {
        for pair in q.split('&') {
            let mut it = pair.splitn(2, '=');
            if let Some(k) = it.next() {
                if k.is_empty() {
                    continue;
                }
                let v = it.next().unwrap_or("");
                out.insert(k.to_string(), url_decode(v));
            }
        }
    }
    out
}

fn url_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'+' {
            out.push(' ');
            i += 1;
        } else if b == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push(((h * 16 + l) as u8) as char);
                i += 3;
            } else {
                out.push(b as char);
                i += 1;
            }
        } else {
            out.push(b as char);
            i += 1;
        }
    }
    out
}

// ----------------------------------------------------------------------------
// 单元测试（真 git fixture + 真密钥对，参考 nexhub_lobby 测试风格）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use os_common::gateway::ApiRequest;

    const TEST_ADMIN_TOKEN: &str = "coderepo-issues-test-admin";

    fn get_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
        }
    }

    fn post_req(path: &str, body: serde_json::Value) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Post,
            path: path.into(),
            headers: serde_json::json!({}),
            body,
        }
    }

    fn req_auth(req: ApiRequest, token: &str) -> ApiRequest {
        let mut r = req;
        r.headers = serde_json::json!({ "authorization": format!("Bearer {token}") });
        r
    }

    /// 持久化服务（文件库隔离到 tempdir；admin token 注入绕 env 竞态）。
    fn service(dir: &str) -> IssuesService {
        IssuesService::with_paths(
            &format!("{dir}/repo_issues.db"),
            &format!("{dir}/hub_lobby.db"),
            dir,
        )
        .with_admin_token(TEST_ADMIN_TOKEN)
    }

    /// 真密钥对登录：直接在注入的 ChainAuth 上签发 token（绕 HTTP 三步——
    /// 挑战-签名链路已由 lobby 覆盖，此处聚焦协作端点语义）。
    fn login(auth: &ChainAuth, sk: &k256::ecdsa::SigningKey) -> String {
        let pubkey = format!(
            "0x{}",
            hex::encode(sk.verifying_key().to_encoded_point(true).as_bytes())
        );
        auth.issue_token(&pubkey).0
    }

    fn tempdir() -> String {
        let p = std::env::temp_dir().join(format!(
            "os-coderepo-issues-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn run(cmd: &[&str]) -> (bool, String) {
        match std::process::Command::new(cmd[0]).args(&cmd[1..]).output() {
            Ok(out) => (
                out.status.success(),
                String::from_utf8_lossy(&out.stdout).to_string(),
            ),
            Err(_) => (false, String::new()),
        }
    }

    /// 造真实裸仓（main 分支 1 提交）+ 可选附加分支（在 main 基础上加一个文件）。
    fn make_repo(dir: &str, name: &str, extra_branch: Option<&str>) {
        let bare = format!("{dir}/{name}.git");
        assert!(run(&["git", "init", "--bare", &bare]).0);
        let work = format!("{dir}/.{name}-work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(format!("{work}/README.md"), "# test\n").unwrap();
        assert!(run(&["git", "-c", "init.defaultBranch=main", "init", &work]).0);
        assert!(run(&["git", "-C", &work, "add", "-A"]).0);
        assert!(
            run(&[
                "git",
                "-C",
                &work,
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "init"
            ])
            .0
        );
        assert!(run(&["git", "-C", &work, "push", &bare, "HEAD:main"]).0);
        if let Some(b) = extra_branch {
            std::fs::write(format!("{work}/feature.txt"), "feature\n").unwrap();
            assert!(run(&["git", "-C", &work, "add", "-A"]).0);
            assert!(
                run(&[
                    "git",
                    "-C",
                    &work,
                    "-c",
                    "user.name=T",
                    "-c",
                    "user.email=t@t",
                    "commit",
                    "-m",
                    "feature"
                ])
                .0
            );
            assert!(run(&["git", "-C", &work, "push", &bare, &format!("HEAD:{b}")]).0);
        }
        let _ = std::fs::remove_dir_all(&work);
    }

    /// 在 hub_lobby.db 写入 publisher 行（owner 判定 fixture；schema 由 lobby 构造）。
    fn seed_lobby_owner(dir: &str, repo: &str, publisher: &str) {
        // 用 lobby handler 起一份完整 schema（避免本模块依赖 lobby 私有建表函数）
        let _lobby = crate::nexhub_lobby::NexHubLobbyRouteHandler::with_db_path(
            &format!("{dir}/hub_lobby.db"),
            dir,
        );
        let conn = Connection::open(format!("{dir}/hub_lobby.db")).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO hub_lobby (repo_name, publisher, published_at) \
             VALUES (?, ?, datetime('now'))",
            params![repo, publisher],
        )
        .unwrap();
    }

    async fn handle(svc: &IssuesService, req: ApiRequest) -> ApiResponse {
        svc.try_handle(req.method, &req.path, &req.headers, &req.body)
            .await
            .expect("issues 命名空间应被认领")
            .unwrap()
    }

    // ---- 路由声明 ----

    #[test]
    fn route_specs_declare_twelve_public_auth_endpoints() {
        let specs = route_specs();
        assert_eq!(specs.len(), 12, "应有 12 条路由: {specs:?}");
        assert!(specs.iter().all(|s| s.handler_component == "code_repo"));
        // 全部 handler 内自验（requires_auth=false——网关不拦链上身份）
        assert!(
            specs
                .iter()
                .all(|s| !s.requires_auth && s.required_roles.is_empty()),
            "issues/pulls 路由应由 handler 自验身份: {specs:?}"
        );
        let paths: Vec<&str> = specs.iter().map(|s| s.path.as_str()).collect();
        assert!(paths.contains(&"/api/v1/coderepo/repos/:name/issues"));
        assert!(paths.contains(&"/api/v1/coderepo/repos/:name/issues/:num/close"));
        assert!(paths.contains(&"/api/v1/coderepo/repos/:name/pulls/:num/merge"));
    }

    // ---- 命名空间认领 ----

    #[tokio::test]
    async fn namespace_detection_only_claims_issues_or_pulls() {
        assert!(owns_namespace(&path_segments(
            "/api/v1/coderepo/repos/demo/issues"
        )));
        assert!(owns_namespace(&path_segments(
            "/api/v1/coderepo/repos/demo/pulls/1/merge"
        )));
        assert!(!owns_namespace(&path_segments(
            "/api/v1/coderepo/repos/demo/contents"
        )));
        assert!(!owns_namespace(&path_segments(
            "/api/v1/coderepo/repos/demo"
        )));
        assert!(!owns_namespace(&path_segments(
            "/api/v1/nexhub/lobby/demo/pulls"
        )));
        // 非本命名空间 → None（调用方继续自己的 match）
        let svc = service(&tempdir());
        assert!(svc
            .try_handle(
                HttpMethod::Get,
                "/api/v1/coderepo/repos/demo/contents",
                &serde_json::json!({}),
                &serde_json::Value::Null,
            )
            .await
            .is_none());
    }

    // ---- Issue 生命周期 ----

    #[tokio::test]
    async fn issue_lifecycle_create_comment_close_reopen() {
        let dir = tempdir();
        make_repo(&dir, "demo", None);
        let auth = Arc::new(ChainAuth::new());
        let svc = service(&dir).with_chain_auth(auth.clone());
        let sk = k256::ecdsa::SigningKey::random(&mut k256::elliptic_curve::rand_core::OsRng);
        let token = login(&auth, &sk);
        let pubkey = format!(
            "0x{}",
            hex::encode(sk.verifying_key().to_encoded_point(true).as_bytes())
        );

        // 无 token → 401
        let resp = handle(
            &svc,
            post_req(
                "/api/v1/coderepo/repos/demo/issues",
                serde_json::json!({ "title": "bug" }),
            ),
        )
        .await;
        assert_eq!(resp.status, 401, "无 token 建 Issue 应 401");

        // 链上身份建 Issue → 201 + number=1 + author=pubkey + owner_kind=pubkey
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/issues",
                    serde_json::json!({
                        "title": "构建失败",
                        "body": "cargo build 报错",
                        "labels": ["bug", "build"]
                    }),
                ),
                &token,
            ),
        )
        .await;
        assert_eq!(resp.status, 201, "建 Issue 应 201: {}", resp.body);
        assert_eq!(resp.body["issue"]["number"], 1);
        assert_eq!(resp.body["issue"]["author"], pubkey.as_str());
        assert_eq!(resp.body["issue"]["owner_kind"], "pubkey");
        assert_eq!(resp.body["issue"]["state"], "open");
        assert_eq!(
            resp.body["issue"]["labels"],
            serde_json::json!(["bug", "build"])
        );

        // 列表默认 open，公开读（无 token）
        let resp = handle(&svc, get_req("/api/v1/coderepo/repos/demo/issues")).await;
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["issues"].as_array().unwrap().len(), 1);

        // 评论（标签串形式也接受）
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/issues/1/comments",
                    serde_json::json!({ "body": "我来复现一下" }),
                ),
                &token,
            ),
        )
        .await;
        assert_eq!(resp.status, 201, "评论应 201: {}", resp.body);
        assert_eq!(resp.body["comment"]["number"], 1);

        // 详情含评论 + comment_count
        let resp = handle(&svc, get_req("/api/v1/coderepo/repos/demo/issues/1")).await;
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["issue"]["comment_count"], 1);
        assert_eq!(resp.body["comments"].as_array().unwrap().len(), 1);

        // 他人（另一密钥对）关闭 → 403
        let auth2 = Arc::new(ChainAuth::new());
        let svc2 = service(&dir).with_chain_auth(auth2.clone());
        let sk2 = k256::ecdsa::SigningKey::random(&mut k256::elliptic_curve::rand_core::OsRng);
        let token2 = login(&auth2, &sk2);
        let resp = handle(
            &svc2,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/issues/1/close",
                    serde_json::json!({}),
                ),
                &token2,
            ),
        )
        .await;
        assert_eq!(resp.status, 403, "非作者关闭应 403");

        // 作者关闭 → 200；重复关闭 → 409
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/issues/1/close",
                    serde_json::json!({}),
                ),
                &token,
            ),
        )
        .await;
        assert_eq!(resp.status, 200, "作者关闭应 200: {}", resp.body);
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/issues/1/close",
                    serde_json::json!({}),
                ),
                &token,
            ),
        )
        .await;
        assert_eq!(resp.status, 409, "重复关闭应 409");

        // 列表 ?state=closed 可见；?state=open 为空
        let resp = handle(
            &svc,
            get_req("/api/v1/coderepo/repos/demo/issues?state=closed"),
        )
        .await;
        assert_eq!(resp.body["issues"].as_array().unwrap().len(), 1);
        let resp = handle(
            &svc,
            get_req("/api/v1/coderepo/repos/demo/issues?state=open"),
        )
        .await;
        assert_eq!(resp.body["issues"].as_array().unwrap().len(), 0);
        // 非法 state → 400
        let resp = handle(
            &svc,
            get_req("/api/v1/coderepo/repos/demo/issues?state=bad"),
        )
        .await;
        assert_eq!(resp.status, 400);

        // 重开（作者）→ open
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/issues/1/open",
                    serde_json::json!({}),
                ),
                &token,
            ),
        )
        .await;
        assert_eq!(resp.status, 200, "作者重开应 200");

        // admin 也能关闭（回落通道）
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/issues/1/close",
                    serde_json::json!({}),
                ),
                TEST_ADMIN_TOKEN,
            ),
        )
        .await;
        assert_eq!(resp.status, 200, "admin 关闭应 200: {}", resp.body);
    }

    #[tokio::test]
    async fn issue_number_increments_per_repo_independently() {
        let dir = tempdir();
        make_repo(&dir, "a", None);
        make_repo(&dir, "b", None);
        let auth = Arc::new(ChainAuth::new());
        let svc = service(&dir).with_chain_auth(auth.clone());
        let sk = k256::ecdsa::SigningKey::random(&mut k256::elliptic_curve::rand_core::OsRng);
        let token = login(&auth, &sk);

        for repo in ["a", "a", "b"] {
            let resp = handle(
                &svc,
                req_auth(
                    post_req(
                        &format!("/api/v1/coderepo/repos/{repo}/issues"),
                        serde_json::json!({ "title": "t" }),
                    ),
                    &token,
                ),
            )
            .await;
            assert_eq!(resp.status, 201);
        }
        let a: Vec<_> = handle(&svc, get_req("/api/v1/coderepo/repos/a/issues?state=all"))
            .await
            .body["issues"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["number"].as_u64().unwrap())
            .collect();
        let b: Vec<_> = handle(&svc, get_req("/api/v1/coderepo/repos/b/issues?state=all"))
            .await
            .body["issues"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["number"].as_u64().unwrap())
            .collect();
        // 每仓库自增互不干扰（倒序）
        assert_eq!(a, vec![2, 1], "仓库 a 应有 #2/#1");
        assert_eq!(b, vec![1], "仓库 b 应只有 #1");
    }

    #[tokio::test]
    async fn issue_validation_and_missing_repo() {
        let dir = tempdir();
        make_repo(&dir, "demo", None);
        let auth = Arc::new(ChainAuth::new());
        let svc = service(&dir).with_chain_auth(auth.clone());
        let token = login(
            &auth,
            &k256::ecdsa::SigningKey::random(&mut k256::elliptic_curve::rand_core::OsRng),
        );

        // 空标题 → 400
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/issues",
                    serde_json::json!({ "title": "  " }),
                ),
                &token,
            ),
        )
        .await;
        assert_eq!(resp.status, 400);
        // 仓库不存在 → 404
        let resp = handle(&svc, get_req("/api/v1/coderepo/repos/nope/issues")).await;
        assert_eq!(resp.status, 404);
        // Issue 不存在 → 404；编号非法 → 400
        let resp = handle(&svc, get_req("/api/v1/coderepo/repos/demo/issues/9")).await;
        assert_eq!(resp.status, 404);
        let resp = handle(&svc, get_req("/api/v1/coderepo/repos/demo/issues/abc")).await;
        assert_eq!(resp.status, 400);
    }

    // ---- PR：创建校验 / merge 权限 / 状态流转 ----

    #[tokio::test]
    async fn pull_create_validates_branches_and_merge_permissions() {
        let dir = tempdir();
        make_repo(&dir, "demo", Some("feature"));
        let auth = Arc::new(ChainAuth::new());
        let svc = service(&dir).with_chain_auth(auth.clone());
        let sk = k256::ecdsa::SigningKey::random(&mut k256::elliptic_curve::rand_core::OsRng);
        let token = login(&auth, &sk);
        let pubkey = format!(
            "0x{}",
            hex::encode(sk.verifying_key().to_encoded_point(true).as_bytes())
        );

        // from_branch 不存在 → 400
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/pulls",
                    serde_json::json!({ "title": "feat", "from_branch": "no-such" }),
                ),
                &token,
            ),
        )
        .await;
        assert_eq!(
            resp.status, 400,
            "不存在的 from_branch 应 400: {}",
            resp.body
        );

        // 合法创建：to_branch 缺省 → 仓库默认分支 main
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/pulls",
                    serde_json::json!({
                        "title": "合入 feature",
                        "body": "功能说明",
                        "from_branch": "feature"
                    }),
                ),
                &token,
            ),
        )
        .await;
        assert_eq!(resp.status, 201, "建 PR 应 201: {}", resp.body);
        assert_eq!(resp.body["pull"]["number"], 1);
        assert_eq!(resp.body["pull"]["to_branch"], "main");
        assert_eq!(resp.body["pull"]["from_branch"], "feature");
        assert_eq!(resp.body["pull"]["author"], pubkey.as_str());
        assert_eq!(resp.body["pull"]["state"], "open");

        // from == to → 400
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/pulls",
                    serde_json::json!({ "title": "x", "from_branch": "main", "to_branch": "main" }),
                ),
                &token,
            ),
        )
        .await;
        assert_eq!(resp.status, 400);

        // 无 token merge → 401；普通链上身份（非 owner）merge → 403
        let resp = handle(
            &svc,
            post_req(
                "/api/v1/coderepo/repos/demo/pulls/1/merge",
                serde_json::json!({}),
            ),
        )
        .await;
        assert_eq!(resp.status, 401);
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/pulls/1/merge",
                    serde_json::json!({}),
                ),
                &token,
            ),
        )
        .await;
        assert_eq!(
            resp.status, 403,
            "非 owner 的链上身份 merge 应 403: {}",
            resp.body
        );

        // owner（大厅 publisher=同一 pubkey）merge → 200 + state=merged + 分支推进
        seed_lobby_owner(&dir, "demo", &pubkey);
        let before = run(&[
            "git",
            &format!("--git-dir={dir}/demo.git"),
            "rev-parse",
            "refs/heads/main",
        ])
        .1;
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/pulls/1/merge",
                    serde_json::json!({}),
                ),
                &token,
            ),
        )
        .await;
        assert_eq!(resp.status, 200, "owner merge 应 200: {}", resp.body);
        assert_eq!(resp.body["state"], "merged");
        assert_eq!(resp.body["merged_by"], pubkey.as_str());
        let after = run(&[
            "git",
            &format!("--git-dir={dir}/demo.git"),
            "rev-parse",
            "refs/heads/main",
        ])
        .1;
        assert_ne!(before.trim(), after.trim(), "merge 后 main 应推进");

        // 已 merged 再 merge → 409
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/pulls/1/merge",
                    serde_json::json!({}),
                ),
                &token,
            ),
        )
        .await;
        assert_eq!(resp.status, 409);

        // 列表按 state 过滤
        let resp = handle(
            &svc,
            get_req("/api/v1/coderepo/repos/demo/pulls?state=merged"),
        )
        .await;
        assert_eq!(resp.body["pulls"].as_array().unwrap().len(), 1);
        let resp = handle(
            &svc,
            get_req("/api/v1/coderepo/repos/demo/pulls?state=open"),
        )
        .await;
        assert_eq!(resp.body["pulls"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn pull_admin_merge_close_and_detail() {
        let dir = tempdir();
        make_repo(&dir, "demo", Some("feature"));
        let auth = Arc::new(ChainAuth::new());
        let svc = service(&dir).with_chain_auth(auth.clone());
        let sk = k256::ecdsa::SigningKey::random(&mut k256::elliptic_curve::rand_core::OsRng);
        let token = login(&auth, &sk);

        // 链上身份建 PR + 评论；admin merge（无大厅条目 → admin-only 通道）
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/pulls",
                    serde_json::json!({ "title": "feat", "from_branch": "feature" }),
                ),
                &token,
            ),
        )
        .await;
        assert_eq!(resp.status, 201);
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/pulls/1/comments",
                    serde_json::json!({ "body": "请看 diff" }),
                ),
                &token,
            ),
        )
        .await;
        assert_eq!(resp.status, 201);

        // 详情（公开）：评论 + diff_stat 非空（feature 比 main 多一个文件）
        let resp = handle(&svc, get_req("/api/v1/coderepo/repos/demo/pulls/1")).await;
        assert_eq!(resp.status, 200, "PR 详情应 200: {}", resp.body);
        assert_eq!(resp.body["pull"]["comment_count"], 1);
        assert_eq!(resp.body["comments"].as_array().unwrap().len(), 1);
        assert!(
            resp.body["diff_stat"]
                .as_str()
                .unwrap_or("")
                .contains("feature.txt"),
            "diff_stat 应含 feature.txt: {}",
            resp.body["diff_stat"]
        );

        // admin merge → 200（merged_by=admin）
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/pulls/1/merge",
                    serde_json::json!({}),
                ),
                TEST_ADMIN_TOKEN,
            ),
        )
        .await;
        assert_eq!(resp.status, 200, "admin merge 应 200: {}", resp.body);
        assert_eq!(resp.body["merged_by"], "admin");
    }

    #[tokio::test]
    async fn pull_close_author_only_and_state_flow() {
        let dir = tempdir();
        make_repo(&dir, "demo", Some("feature"));
        let auth = Arc::new(ChainAuth::new());
        let svc = service(&dir).with_chain_auth(auth.clone());
        let sk = k256::ecdsa::SigningKey::random(&mut k256::elliptic_curve::rand_core::OsRng);
        let token = login(&auth, &sk);

        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/pulls",
                    serde_json::json!({ "title": "feat", "from_branch": "feature" }),
                ),
                &token,
            ),
        )
        .await;
        assert_eq!(resp.status, 201);

        // 他人关闭 → 403
        let auth2 = Arc::new(ChainAuth::new());
        let svc2 = service(&dir).with_chain_auth(auth2.clone());
        let sk2 = k256::ecdsa::SigningKey::random(&mut k256::elliptic_curve::rand_core::OsRng);
        let resp = handle(
            &svc2,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/pulls/1/close",
                    serde_json::json!({}),
                ),
                &login(&auth2, &sk2),
            ),
        )
        .await;
        assert_eq!(resp.status, 403, "非作者关闭 PR 应 403");

        // 作者关闭 → 200；closed 的 PR 不能 merge（409）
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/pulls/1/close",
                    serde_json::json!({}),
                ),
                &token,
            ),
        )
        .await;
        assert_eq!(resp.status, 200, "作者关闭应 200: {}", resp.body);
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/pulls/1/merge",
                    serde_json::json!({}),
                ),
                TEST_ADMIN_TOKEN,
            ),
        )
        .await;
        assert_eq!(resp.status, 409, "closed PR 不能 merge");
        // 重复 close → 409
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/pulls/1/close",
                    serde_json::json!({}),
                ),
                TEST_ADMIN_TOKEN,
            ),
        )
        .await;
        assert_eq!(resp.status, 409);
    }

    // ---- admin 归因 / owner_kind ----

    #[tokio::test]
    async fn admin_authorship_marks_owner_kind_admin() {
        let dir = tempdir();
        make_repo(&dir, "demo", None);
        let svc = service(&dir);
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/issues",
                    serde_json::json!({ "title": "admin issue" }),
                ),
                TEST_ADMIN_TOKEN,
            ),
        )
        .await;
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["issue"]["author"], "admin");
        assert_eq!(resp.body["issue"]["owner_kind"], "admin");
        // 标签逗号串入参也接受
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/issues",
                    serde_json::json!({ "title": "t2", "labels": "a，b" }),
                ),
                TEST_ADMIN_TOKEN,
            ),
        )
        .await;
        assert_eq!(
            resp.body["issue"]["labels"],
            serde_json::json!(["a", "b"]),
            "中文逗号分隔的标签串也应规范化"
        );
    }
}
