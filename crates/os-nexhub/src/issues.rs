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
//! # 路由表（16 条，挂在 code_repo 组件名下，前缀 /api/v1/coderepo/repos/:name）
//!
//! | method | path | 动作 | 权限 |
//! |--------|------|------|------|
//! | GET    | `/issues` | Issue 列表（`?state=open|closed|all`，默认 open）| 公开 |
//! | POST   | `/issues` | 建 Issue `{title, body?, labels?}`（number 自动分配）| 身份 |
//! | GET    | `/issues/:num` | 详情（含评论流 + comment_count + 引用徽章数据）| 公开 |
//! | POST   | `/issues/:num/comments` | 评论 `{body}` | 身份 |
//! | POST   | `/issues/:num/close` | 关闭（仅 author/admin）| 身份 |
//! | POST   | `/issues/:num/open` | 重开（仅 author/admin）| 身份 |
//! | GET    | `/pulls` | PR 列表（`?state=open|merged|closed|all`，默认 open）| 公开 |
//! | POST   | `/pulls` | 建 PR `{title, body?, from_branch, to_branch?}`（from 分支须已 push 到裸仓）| 身份 |
//! | GET    | `/pulls/:num` | 详情（含评论流 + `git diff to..from --stat`）| 公开 |
//! | POST   | `/pulls/:num/comments` | PR 评论 `{body}` | 身份 |
//! | POST   | `/pulls/:num/merge` | 合并 `{merge_strategy?, message?}`（仅 admin/仓库 owner）| 身份 |
//! | POST   | `/pulls/:num/close` | 关闭（仅 author/admin）| 身份 |
//! | GET    | `/releases/:tag/assets` | release 附件清单 | 公开 |
//! | POST   | `/releases/:tag/assets` | 上传附件 `{name, content_base64}`（≤100MB）| owner/admin |
//! | GET    | `/releases/:tag/assets/:aid` | 下载附件（octet-stream 直传）| 公开 |
//! | DELETE | `/releases/:tag/assets/:aid` | 删除附件 | owner/admin |
//!
//! # 三件增量（v0.1.50 批 1/2，方案 docs/research/NEXHUB_FEATURES.md）
//!
//! - **Merge 策略**（§top4）：`merge` body `merge_strategy: merge|squash|rebase`
//!   （缺省 merge=现行为）；squash 压单提交（缺省信息=PR 标题 `(#编号)`），rebase
//!   可快进则快进、否则逐 commit 变基重放（作者原样保留）。执行在
//!   [`merge_with_strategy_blocking`] 单点分叉。
//! - **交叉引用**（§top5）：PR body/评论里 `(fix|fixes|fixed|close|closes|closed|
//!   resolve|resolves|resolved) #n`（大小写不敏感）→ `hub_issue_refs` 表；merge
//!   成功时自动关闭被引用的 open issue + 在其时间线评论 `closed via PR !m`；
//!   列表/详情互相携带引用徽章数据（issue.`referencing_pulls` / pull.`referenced_issues`）。
//! - **Release 二进制附件**（§top3）：`hub_release_assets` 表 + 上传/下载/删除/
//!   清单四端点（release 行以大厅 `hub_releases` 为权威，跨库只读校验存在性）；
//!   文件落 `<repos_root>/.assets/<repo>/<tag>/`；下载经 os-api 网关
//!   `application/octet-stream` 直传（b64 信封，同 apps 静态资源先例）。
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
    default_db_path as lobby_db_path_default, merge_with_strategy_blocking, pr_diff_stat_blocking,
    validate_branch_name, validate_tag_name, MergeStrategy,
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
/// 单个 release 附件解码后大小上限（100MB；b64 信封约为其 4/3）。
const MAX_ASSET_BYTES: usize = 100 * 1024 * 1024;
/// 附件名长度上限。
const MAX_ASSET_NAME_CHARS: usize = 200;

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
    /// 引用本 Issue 的 PR 编号列表（交叉引用徽章数据，2026-09-24 §top5）。
    #[serde(default)]
    pub referencing_pulls: Vec<u64>,
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
    /// 本 PR 引用的 Issue 编号列表（交叉引用徽章数据，2026-09-24 §top5；
    /// merge 时这些 open issue 被自动关闭）。
    #[serde(default)]
    pub referenced_issues: Vec<u64>,
    /// 创建时间（RFC3339）。
    pub created_at: String,
    /// 更新时间。
    pub updated_at: String,
}

fn default_open() -> String {
    "open".to_string()
}

/// 单个 release 附件（hub_release_assets 行，2026-09-24 方案 §top3）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseAsset {
    /// 附件 id（服务端生成，`ast-<纳秒 hex>`；下载/删除定位键）。
    pub id: String,
    /// 仓库名。
    pub repo: String,
    /// 所属 release 的 git tag（release 行以大厅 hub_releases 为权威）。
    pub release_tag: String,
    /// 附件名（同 release 内唯一；即下载文件名）。
    pub name: String,
    /// 字节数（解码后）。
    pub size: u64,
    /// 内容 SHA-256（hex，小写；下载侧可校验完整性）。
    pub sha256: String,
    /// 上传时间（RFC3339）。
    pub created_at: String,
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

/// POST /pulls/:num/merge 请求体（全字段可选——空对象即缺省 merge 现行为）。
#[derive(Debug, Default, Deserialize)]
struct MergePullBody {
    /// 合并策略：`merge`（缺省，双 parent 合并提交）/ `squash`（压单提交）/
    /// `rebase`（线性变基）。未知值 400。
    #[serde(default)]
    merge_strategy: Option<String>,
    /// 自定义合并提交信息（merge/squash 生效；缺省 merge=`Merge PR #n: 标题`、
    /// squash=`标题 (#n)`）。
    #[serde(default)]
    message: Option<String>,
}

/// POST /releases/:tag/assets 请求体（b64-JSON 信封，同 downloads/torrent 先例
/// ——网关契约无 multipart）。
#[derive(Debug, Deserialize)]
struct AssetUploadBody {
    /// 附件名（同 release 内唯一；不可含路径分隔符）。
    name: String,
    /// 附件内容 base64（标准字母表；解码后 ≤100MB）。
    content_base64: String,
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
/// 附件行字段序（hub_release_assets INSERT/SELECT 共用）。
const ASSET_COLUMNS: &str = "id,repo_name,release_tag,name,size,sha256,created_at";

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
        );
        CREATE TABLE IF NOT EXISTS hub_issue_refs (
            repo_name      TEXT NOT NULL,
            issue_number   INTEGER NOT NULL,
            pull_number    INTEGER NOT NULL,
            created_at     TEXT NOT NULL,
            PRIMARY KEY (repo_name, issue_number, pull_number)
        );
        CREATE TABLE IF NOT EXISTS hub_release_assets (
            id             TEXT NOT NULL PRIMARY KEY,
            repo_name      TEXT NOT NULL,
            release_tag    TEXT NOT NULL,
            name           TEXT NOT NULL,
            size           INTEGER NOT NULL DEFAULT 0,
            sha256         TEXT NOT NULL DEFAULT '',
            created_at     TEXT NOT NULL,
            UNIQUE (repo_name, release_tag, name)
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
        referencing_pulls: Vec::new(),
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
        referenced_issues: Vec::new(),
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
            let _ = c.busy_timeout(std::time::Duration::from_millis(3000)); // 防 SQLITE_BUSY 立败（审计 E#6）
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
            let _ = c.busy_timeout(std::time::Duration::from_millis(3000)); // 防 SQLITE_BUSY 立败（审计 E#6）
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
            *guard = Connection::open(&self.lobby_db_path)
                .and_then(|c| {
                    c.busy_timeout(std::time::Duration::from_millis(3000))?; // 防 SQLITE_BUSY 立败（审计 E#6）
                    Ok(c)
                })
                .ok();
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

/// 本模块路由是否认领该路径段（`repos/:name/issues|pulls|releases/...` 命名空间）。
fn owns_namespace(segs: &[&str]) -> bool {
    segs.len() >= 6
        && segs[0] == "api"
        && segs[1] == "v1"
        && segs[2] == "coderepo"
        && segs[3] == "repos"
        && (segs[5] == "issues" || segs[5] == "pulls" || segs[5] == "releases")
}

/// 16 条路由 spec（component="code_repo"；读公开、写 handler 内自验身份）。
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
        // Release 二进制附件（2026-09-24 方案 §top3；release 行权威在大厅
        // hub_releases，此处按 (repo, tag) 挂附件——仓库域命名空间，不动 lobby）
        (HttpMethod::Get, "/releases/:tag/assets"),
        (HttpMethod::Post, "/releases/:tag/assets"),
        (HttpMethod::Get, "/releases/:tag/assets/:aid"),
        (HttpMethod::Delete, "/releases/:tag/assets/:aid"),
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
                self.with_num_async(num, |n| self.merge_pull(repo, n, headers, body))
                    .await
            }
            (HttpMethod::Post, ["api", "v1", "coderepo", "repos", repo, "pulls", num, "close"]) => {
                self.with_num(num, |n| Ok(self.close_pull(repo, n, headers)))
            }

            // ============ Release 二进制附件（§top3） ============
            (
                HttpMethod::Get,
                ["api", "v1", "coderepo", "repos", repo, "releases", tag, "assets"],
            ) => Ok(self.list_release_assets(repo, tag)),
            (
                HttpMethod::Post,
                ["api", "v1", "coderepo", "repos", repo, "releases", tag, "assets"],
            ) => Ok(self.upload_release_asset(repo, tag, headers, body)),
            (
                HttpMethod::Get,
                ["api", "v1", "coderepo", "repos", repo, "releases", tag, "assets", aid],
            ) => Ok(self.download_release_asset(repo, tag, aid)),
            (
                HttpMethod::Delete,
                ["api", "v1", "coderepo", "repos", repo, "releases", tag, "assets", aid],
            ) => Ok(self.delete_release_asset(repo, tag, aid, headers)),

            // 命名空间内未覆盖组合 → 404
            _ => Ok(error_response(
                404,
                "code_repo: 未匹配的 issues/pulls/releases 路由",
            )),
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
            referencing_pulls: Vec::new(),
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
        crate::webhooks::fire_issue(repo, "created", &issue, caller.actor());
        ApiResponse {
            status: 201,
            body: serde_json::json!({ "ok": true, "issue": issue }),
            headers: serde_json::json!({}),
        }
    }

    /// GET /issues/:num（公开）：详情 + 评论流 + comment_count + 引用徽章数据。
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
            let refs = refs_of_pull_issue(&conn, repo, num).unwrap_or_default();
            (
                RepoIssue {
                    referencing_pulls: refs,
                    ..issue
                },
                comments,
            )
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
            body: text.clone(),
            created_at: now_iso(),
        };
        if let Err(e) = insert_comment(&conn, &comment) {
            return error_response(500, &format!("写入评论失败: {e}"));
        }
        // PR 评论里的 fixes #n 同步进交叉引用表（issue 评论无此语义）
        if kind == "pull" {
            record_pull_refs(&conn, repo, parent, &[&text]);
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
        crate::webhooks::fire_comment(repo, kind, parent, &comment);
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
        // webhook 事件（issues：closed / reopened——任务矩阵含关闭，重开顺带同报）
        crate::webhooks::fire_issue(
            repo,
            if open { "reopened" } else { "closed" },
            &issue,
            caller.actor(),
        );
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
            referenced_issues: Vec::new(),
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
                    // PR body 的 fixes #n → 交叉引用表（merge 联动关闭 + 徽章数据）
                    record_pull_refs(&conn, repo, n, &[&pull.body]);
                    RepoPull {
                        referenced_issues: cross_refs(&pull.body),
                        ..pull
                    }
                }
                Err(e) => return Ok(error_response(500, &format!("分配编号失败: {e}"))),
            }
        };
        crate::webhooks::fire_pull(repo, "created", &saved, caller.actor(), None);
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
            let refs = refs_of_pull(&conn, repo, num).unwrap_or_default();
            (
                RepoPull {
                    referenced_issues: refs,
                    ..pull
                },
                comments,
            )
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
    /// 更改仓库内容，没有更改权限的 agent 不能执行。执行复用 lobby 的裸仓合并
    /// 内核（[`merge_with_strategy_blocking`]，2026-09-24 §top4 三策略单点分叉）：
    /// body `{merge_strategy?: merge|squash|rebase, message?}`（缺省 merge=现行为）；
    /// squash 缺省信息=`标题 (#编号)`，rebase 逐 commit 变基线性（作者原样保留）；
    /// 冲突 409。merge 成功后联动关闭 body/评论里 `fixes #n` 引用的 open issue
    /// （§top5），并在其时间线自动评论 `closed via PR !m`。
    async fn merge_pull(
        &self,
        repo: &str,
        num: u64,
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
        // 策略与信息（空 body/空对象 = 缺省 merge 现行为）
        let body: MergePullBody = if body.is_null() {
            MergePullBody::default()
        } else {
            match serde_json::from_value(body.clone()) {
                Ok(b) => b,
                Err(e) => return Ok(error_response(400, &format!("解析合并请求体失败: {e}"))),
            }
        };
        let strategy = match MergeStrategy::parse(body.merge_strategy.as_deref()) {
            Some(s) => s,
            None => {
                return Ok(error_response(
                    400,
                    "非法 merge_strategy（可选 merge/squash/rebase，缺省 merge）",
                ))
            }
        };
        let custom_msg = body
            .message
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        if let Some(m) = &custom_msg {
            if m.chars().count() > MAX_BODY_CHARS {
                return Ok(error_response(
                    400,
                    &format!("合并信息过长（≤{MAX_BODY_CHARS} 字符）"),
                ));
            }
        }
        let message = match (&strategy, &custom_msg) {
            // merge 缺省：现行为（Merge PR #n: 标题）
            (MergeStrategy::Merge, None) => format!("Merge PR #{}: {}", pull.number, pull.title),
            // squash 缺省：PR 标题 (#编号)（GitHub 同款缺省）
            (MergeStrategy::Squash, None) => format!("{} (#{})", pull.title, pull.number),
            (_, Some(m)) => m.clone(),
            // rebase 不产生合并提交（逐 commit 保留原信息）——占位空串
            (MergeStrategy::Rebase, None) => String::new(),
        };
        let (m_bare, m_to, m_from, m_msg) = (
            bare.clone(),
            pull.to_branch.clone(),
            pull.from_branch.clone(),
            message,
        );
        let merged = tokio::task::spawn_blocking(move || {
            merge_with_strategy_blocking(&m_bare, &m_to, &m_from, &m_msg, &strategy)
        })
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
        pull.updated_at = now.clone();
        let closed_issues = {
            let conn = self.db.lock().expect("db poisoned");
            if let Err(e) = save_pull(&conn, &pull) {
                return Ok(error_response(500, &format!("写入 PR 失败: {e}")));
            }
            // 交叉引用联动（§top5）：关闭被引用的 open issue + 时间线自动评论
            self.close_referenced_issues(&conn, repo, num, &caller)
        };
        // webhook 事件（pr：merged，含合并提交 sha）
        crate::webhooks::fire_pull(repo, "merged", &pull, caller.actor(), Some(&merged_sha));
        Ok(ok_json(serde_json::json!({
            "ok": true,
            "repo": repo,
            "number": num,
            "state": "merged",
            "merge_strategy": strategy.as_str(),
            "merged_by": pull.merged_by,
            "merged_at": pull.merged_at,
            "merged_sha": merged_sha,
            "closed_issues": closed_issues,
        })))
    }

    /// merge 成功后的交叉引用联动（调用方须已持 db 锁）：`fixes #n` 引用的
    /// open issue → closed + 时间线评论 `closed via PR !m`（作者=合并执行者）。
    /// 返回实际关闭的编号列表（已 closed/不存在的引用静默跳过）。
    fn close_referenced_issues(
        &self,
        conn: &Connection,
        repo: &str,
        pull: u64,
        caller: &Caller,
    ) -> Vec<u64> {
        let refs = refs_of_pull(conn, repo, pull).unwrap_or_default();
        let mut closed = Vec::new();
        let now = now_iso();
        for issue_num in refs {
            let Ok(Some(mut issue)) = find_issue(conn, repo, issue_num) else {
                continue; // 引用了不存在的编号——徽章可显示，不联动
            };
            if issue.state != "open" {
                continue; // 已关闭/无状态可流转
            }
            issue.state = "closed".to_string();
            issue.updated_at = now.clone();
            if save_issue(conn, &issue).is_err() {
                continue;
            }
            // 时间线自动评论（编号与既有评论流共享序列；作者=合并执行者）
            let number = next_comment_number(conn, repo, "issue", issue_num).unwrap_or(1);
            let auto = RepoComment {
                repo: repo.to_string(),
                kind: "issue".to_string(),
                parent_number: issue_num,
                number,
                author: caller.actor().to_string(),
                author_display: caller.display().to_string(),
                owner_kind: caller.owner_kind().to_string(),
                body: format!("closed via PR !{pull}"),
                created_at: now.clone(),
            };
            let _ = insert_comment(conn, &auto);
            closed.push(issue_num);
        }
        closed
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

    // ------------------------------------------------------------------------
    // Release 二进制附件（2026-09-24 方案 §top3：hub_release_assets + 四端点）
    // ------------------------------------------------------------------------

    /// 附件文件落盘路径：`<repos_root>/.assets/<repo>/<tag>/<asset-id>.bin`
    /// （tag 已过 validate_tag_name——无 `/`、无 `..`，路径安全）。
    fn asset_file_path(&self, repo: &str, tag: &str, id: &str) -> String {
        format!("{}/.assets/{repo}/{tag}/{id}.bin", self.repos_root)
    }

    /// release 行存在性（权威=大厅 hub_lobby.db 的 hub_releases 表；本服务对
    /// 该库只读——复用 owner 判定连接）。查不到/库不可开 → None（404）。
    fn release_exists(&self, repo: &str, tag: &str) -> bool {
        let mut guard = self.lobby_conn.lock().expect("lobby conn poisoned");
        if guard.is_none() {
            *guard = Connection::open(&self.lobby_db_path)
                .and_then(|c| {
                    c.busy_timeout(std::time::Duration::from_millis(3000))?;
                    Ok(c)
                })
                .ok();
        }
        let Some(conn) = guard.as_ref() else {
            return false;
        };
        conn.query_row(
            "SELECT 1 FROM hub_releases WHERE repo_name=? AND tag=?",
            params![repo, tag],
            |_| Ok(()),
        )
        .is_ok()
    }

    /// 校验仓库存在 + tag 合法 + release 行存在 → Err(响应) 400/404。
    fn require_release(&self, repo: &str, tag: &str) -> Result<(), ApiResponse> {
        self.require_repo(repo)?;
        if let Err(msg) = validate_tag_name(tag) {
            return Err(error_response(400, &msg));
        }
        if !self.release_exists(repo, tag) {
            return Err(error_response(
                404,
                &format!("release 不存在: {repo}/{tag}"),
            ));
        }
        Ok(())
    }

    /// 附件上传/删除权限（同 merge：admin 或仓库 owner——发版物即仓库内容）。
    fn caller_can_manage_assets(&self, caller: &Caller, repo: &str) -> bool {
        match caller.pubkey() {
            Some(pk) => self.repo_owner_pubkey(repo).as_deref() == Some(pk),
            None => true,
        }
    }

    /// GET /releases/:tag/assets（公开）：release 附件清单。
    fn list_release_assets(&self, repo: &str, tag: &str) -> ApiResponse {
        if let Err(resp) = self.require_release(repo, tag) {
            return resp;
        }
        let conn = self.db.lock().expect("db poisoned");
        match load_assets(&conn, repo, tag) {
            Ok(assets) => ok_json(serde_json::json!({
                "repo": repo,
                "tag": tag,
                "assets": assets,
            })),
            Err(e) => error_response(500, &format!("数据库错误: {e}")),
        }
    }

    /// POST /releases/:tag/assets（owner/admin）：b64-JSON 信封 `{name,
    /// content_base64}`，解码后 ≤100MB；同名附件 409；落盘 + 落库（sha256/size）。
    fn upload_release_asset(
        &self,
        repo: &str,
        tag: &str,
        headers: &Json,
        body: &Json,
    ) -> ApiResponse {
        let Some(caller) = self.caller(headers) else {
            return auth_required();
        };
        if let Err(resp) = self.require_release(repo, tag) {
            return resp;
        }
        if !self.caller_can_manage_assets(&caller, repo) {
            return error_response(
                403,
                "仅 admin 或仓库所有者可管理 release 附件（附件即发版物——先把仓库发布到大厅并归因你的链上身份，或联系管理员）",
            );
        }
        let body: AssetUploadBody = match serde_json::from_value(body.clone()) {
            Ok(b) => b,
            Err(e) => return error_response(400, &format!("解析上传请求体失败: {e}")),
        };
        let name = body.name.trim().to_string();
        if name.is_empty() {
            return error_response(400, "附件名不可为空");
        }
        if name.chars().count() > MAX_ASSET_NAME_CHARS {
            return error_response(400, &format!("附件名过长（≤{MAX_ASSET_NAME_CHARS} 字符）"));
        }
        if name.contains(['/', '\\']) || name == ".." || name == "." || name.starts_with('.') {
            return error_response(400, "附件名不可为路径（含 / 或以 . 开头）");
        }
        if name.chars().any(|c| c.is_control()) {
            return error_response(400, "附件名不可含控制字符");
        }
        let content = match b64_decode(body.content_base64.trim()) {
            Some(c) => c,
            None => return error_response(400, "content_base64 非法（标准字母表 base64）"),
        };
        if content.is_empty() {
            return error_response(400, "附件内容不可为空");
        }
        if content.len() > MAX_ASSET_BYTES {
            return error_response(413, &format!("附件过大（解码后 ≤{MAX_ASSET_BYTES} 字节）"));
        }
        // 同名冲突（release 内附件名唯一）→ 409
        {
            let conn = self.db.lock().expect("db poisoned");
            match find_asset_by_name(&conn, repo, tag, &name) {
                Ok(Some(_)) => {
                    return error_response(409, &format!("附件已存在: {repo}/{tag}/{name}"))
                }
                Ok(None) => {}
                Err(e) => return error_response(500, &format!("数据库错误: {e}")),
            }
        }
        let id = format!(
            "ast-{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let path = self.asset_file_path(repo, tag, &id);
        if let Some(dir) = std::path::Path::new(&path).parent() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                return error_response(500, &format!("创建附件目录失败: {e}"));
            }
        }
        if let Err(e) = std::fs::write(&path, &content) {
            return error_response(500, &format!("附件落盘失败: {e}"));
        }
        let asset = ReleaseAsset {
            id: id.clone(),
            repo: repo.to_string(),
            release_tag: tag.to_string(),
            name,
            size: content.len() as u64,
            sha256: sha256_hex(&content),
            created_at: now_iso(),
        };
        {
            let conn = self.db.lock().expect("db poisoned");
            if let Err(e) = insert_asset(&conn, &asset) {
                let _ = std::fs::remove_file(&path);
                let conflict = matches!(
                    &e,
                    rusqlite::Error::SqliteFailure(f, _)
                        if f.code == rusqlite::ErrorCode::ConstraintViolation
                );
                if conflict {
                    return error_response(
                        409,
                        &format!("附件已存在: {repo}/{tag}/{}", asset.name),
                    );
                }
                return error_response(500, &format!("写入附件记录失败: {e}"));
            }
        }
        ApiResponse {
            status: 201,
            body: serde_json::json!({ "ok": true, "asset": asset }),
            headers: serde_json::json!({}),
        }
    }

    /// GET /releases/:tag/assets/:aid（公开）：附件内容直传——b64 信封 +
    /// `application/octet-stream` + `Content-Disposition`（os-api 网关按
    /// direct-passthrough 先例解码回原始字节，浏览器直接下载）。
    fn download_release_asset(&self, repo: &str, tag: &str, aid: &str) -> ApiResponse {
        if let Err(resp) = self.require_release(repo, tag) {
            return resp;
        }
        let (asset, path) = {
            let conn = self.db.lock().expect("db poisoned");
            match find_asset(&conn, repo, tag, aid) {
                Ok(Some(a)) => {
                    let p = self.asset_file_path(repo, tag, &a.id);
                    (a, p)
                }
                Ok(None) => {
                    return error_response(404, &format!("附件不存在: {aid}"));
                }
                Err(e) => return error_response(500, &format!("数据库错误: {e}")),
            }
        };
        let content = match std::fs::read(&path) {
            Ok(c) => c,
            Err(_) => return error_response(404, &format!("附件文件缺失: {aid}")),
        };
        ApiResponse {
            status: 200,
            body: serde_json::Value::String(b64_encode(&content)),
            headers: serde_json::json!({
                "content-type": "application/octet-stream",
                "content-disposition": format!(
                    "attachment; filename=\"{}\"",
                    asset.name.replace(['"', '\r', '\n'], "")
                ),
            }),
        }
    }

    /// DELETE /releases/:tag/assets/:aid（owner/admin）：删库行 + 尽力删文件。
    fn delete_release_asset(
        &self,
        repo: &str,
        tag: &str,
        aid: &str,
        headers: &Json,
    ) -> ApiResponse {
        let Some(caller) = self.caller(headers) else {
            return auth_required();
        };
        if let Err(resp) = self.require_release(repo, tag) {
            return resp;
        }
        if !self.caller_can_manage_assets(&caller, repo) {
            return error_response(403, "仅 admin 或仓库所有者可管理 release 附件");
        }
        let (asset, path) = {
            let conn = self.db.lock().expect("db poisoned");
            match find_asset(&conn, repo, tag, aid) {
                Ok(Some(a)) => {
                    let p = self.asset_file_path(repo, tag, &a.id);
                    (a, p)
                }
                Ok(None) => return error_response(404, &format!("附件不存在: {aid}")),
                Err(e) => return error_response(500, &format!("数据库错误: {e}")),
            }
        };
        {
            let conn = self.db.lock().expect("db poisoned");
            if let Err(e) = delete_asset(&conn, repo, tag, aid) {
                return error_response(500, &format!("删除附件记录失败: {e}"));
            }
        }
        let _ = std::fs::remove_file(&path);
        ok_json(serde_json::json!({
            "ok": true,
            "repo": repo,
            "tag": tag,
            "id": asset.id,
            "action": "asset_delete",
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
    let refs = "(SELECT GROUP_CONCAT(pull_number) FROM (SELECT pull_number FROM hub_issue_refs r \
         WHERE r.repo_name=i.repo_name AND r.issue_number=i.number ORDER BY pull_number))";
    let mut sql = format!(
        "SELECT {ISSUE_COLUMNS}, {comment_count} AS comment_count, {refs} AS refs \
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
        issue.referencing_pulls = csv_numbers(row.get::<_, Option<String>>(11)?.as_deref());
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
    let refs =
        "(SELECT GROUP_CONCAT(issue_number) FROM (SELECT issue_number FROM hub_issue_refs r \
         WHERE r.repo_name=p.repo_name AND r.pull_number=p.number ORDER BY issue_number))";
    let mut sql = format!(
        "SELECT {PULL_COLUMNS}, {comment_count} AS comment_count, {refs} AS refs \
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
        pull.referenced_issues = csv_numbers(row.get::<_, Option<String>>(14)?.as_deref());
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

// ----------------------------------------------------------------------------
// Release 附件持久化（hub_release_assets；release 行权威在大厅 hub_releases）
// ----------------------------------------------------------------------------

fn asset_from_row(row: &rusqlite::Row) -> rusqlite::Result<ReleaseAsset> {
    Ok(ReleaseAsset {
        id: row.get(0)?,
        repo: row.get(1)?,
        release_tag: row.get(2)?,
        name: row.get(3)?,
        size: row.get::<_, i64>(4)?.max(0) as u64,
        sha256: row.get(5)?,
        created_at: row.get(6)?,
    })
}

/// 某 release 的附件清单（上传时间升序）。
fn load_assets(conn: &Connection, repo: &str, tag: &str) -> rusqlite::Result<Vec<ReleaseAsset>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ASSET_COLUMNS} FROM hub_release_assets \
         WHERE repo_name=? AND release_tag=? ORDER BY created_at ASC, id ASC"
    ))?;
    let iter = stmt.query_map(params![repo, tag], asset_from_row)?;
    let mut out = Vec::new();
    for a in iter {
        out.push(a?);
    }
    Ok(out)
}

/// 按 id 查附件（repo+tag 双冗余定位——跨仓同 id 不可达，仓库隔离的查询面）。
fn find_asset(
    conn: &Connection,
    repo: &str,
    tag: &str,
    id: &str,
) -> rusqlite::Result<Option<ReleaseAsset>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ASSET_COLUMNS} FROM hub_release_assets \
         WHERE repo_name=? AND release_tag=? AND id=?"
    ))?;
    stmt.query_row(params![repo, tag, id], asset_from_row)
        .optional()
}

/// 按附件名查（同名冲突预检）。
fn find_asset_by_name(
    conn: &Connection,
    repo: &str,
    tag: &str,
    name: &str,
) -> rusqlite::Result<Option<ReleaseAsset>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ASSET_COLUMNS} FROM hub_release_assets \
         WHERE repo_name=? AND release_tag=? AND name=?"
    ))?;
    stmt.query_row(params![repo, tag, name], asset_from_row)
        .optional()
}

fn insert_asset(conn: &Connection, a: &ReleaseAsset) -> rusqlite::Result<()> {
    conn.execute(
        &format!("INSERT INTO hub_release_assets ({ASSET_COLUMNS}) VALUES (?,?,?,?,?,?,?)"),
        params![
            a.id,
            a.repo,
            a.release_tag,
            a.name,
            a.size as i64,
            a.sha256,
            a.created_at,
        ],
    )?;
    Ok(())
}

fn delete_asset(conn: &Connection, repo: &str, tag: &str, id: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM hub_release_assets WHERE repo_name=? AND release_tag=? AND id=?",
        params![repo, tag, id],
    )
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
// 交叉引用（2026-09-24 方案 §top5：fixes #n 解析 → hub_issue_refs → merge 联动）
// ----------------------------------------------------------------------------

/// 交叉引用关键字（小写；fix/close/resolve 三动词的原形/三单/过去式——GitHub
/// closing keywords 同族）。匹配要求动词前是非字母数字（词首边界），后跟可选
/// 空白 + `#` + 数字。
const CROSS_REF_VERBS: [&str; 9] = [
    "fix", "fixes", "fixed", "close", "closes", "closed", "resolve", "resolves", "resolved",
];

/// 解析文本中的关闭型交叉引用：`(fix|fixes|fixed|close|closes|closed|resolve|
/// resolves|resolved) #n`，大小写不敏感；返回去重后的 issue 编号（首现顺序）。
/// 纯 std 手写扫描（无 regex 依赖；动词须为词首——`refixes #1` 不匹配，
/// 动词与 `#` 之间允许空白——`fixes #12` / `Fixes:#3` 均命中）。
fn cross_refs(text: &str) -> Vec<u64> {
    let lower = text.to_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    let mut out: Vec<u64> = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        // 找下一个 '#'
        let Some(hash) = (i..chars.len()).find(|&k| chars[k] == '#') else {
            break;
        };
        // '#<digits>'
        let mut j = hash + 1;
        while j < chars.len() && chars[j].is_ascii_digit() {
            j += 1;
        }
        if j > hash + 1 {
            // 反向跳过 '#' 前的空白，检查前缀是否以动词结尾（词首边界）
            let mut head_end = hash;
            while head_end > 0 && chars[head_end - 1].is_whitespace() {
                head_end -= 1;
            }
            let head: String = chars[..head_end].iter().collect();
            if ends_with_cross_ref_verb(&head) {
                let num: u64 = chars[hash + 1..j]
                    .iter()
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0);
                if num > 0 && !out.contains(&num) {
                    out.push(num);
                }
            }
        }
        i = j.max(hash + 1);
    }
    out
}

/// 文本是否以交叉引用动词结尾（动词首字符前是文本头或非字母数字——词首边界；
/// `refixes`/`hotfix #1` 的 `fix` 不算，`fixes`/`will fix` 算）。
fn ends_with_cross_ref_verb(head: &str) -> bool {
    for verb in CROSS_REF_VERBS {
        if let Some(at) = head.len().checked_sub(verb.len()) {
            if head.is_char_boundary(at)
                && &head[at..] == verb
                && (at == 0
                    || !head[..at]
                        .chars()
                        .next_back()
                        .is_some_and(|c| c.is_alphanumeric()))
            {
                return true;
            }
        }
    }
    false
}

/// GROUP_CONCAT 产物（`1,3,7` / NULL）→ 升序编号数组（徽章数据装配）。
fn csv_numbers(raw: Option<&str>) -> Vec<u64> {
    raw.unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .filter(|&n| n > 0)
        .collect()
}

/// 记录 PR 的交叉引用（body + 评论文本合并解析；INSERT OR IGNORE 去重）。
/// 调用方须已持 db 锁。
fn record_pull_refs(conn: &Connection, repo: &str, pull: u64, texts: &[&str]) {
    let now = now_iso();
    for text in texts {
        for issue in cross_refs(text) {
            let _ = conn.execute(
                "INSERT OR IGNORE INTO hub_issue_refs \
                 (repo_name, issue_number, pull_number, created_at) VALUES (?,?,?,?)",
                params![repo, issue as i64, pull as i64, now],
            );
        }
    }
}

/// 查询 PR 引用的 issue 编号（升序；merge 联动关闭 + 徽章数据共用）。
fn refs_of_pull(conn: &Connection, repo: &str, pull: u64) -> rusqlite::Result<Vec<u64>> {
    let mut stmt = conn.prepare(
        "SELECT issue_number FROM hub_issue_refs \
         WHERE repo_name=? AND pull_number=? ORDER BY issue_number ASC",
    )?;
    let iter = stmt.query_map(params![repo, pull as i64], |r| {
        r.get::<_, i64>(0).map(|n| n.max(0) as u64)
    })?;
    let mut out = Vec::new();
    for n in iter {
        out.push(n?);
    }
    Ok(out)
}

/// 查询引用某 issue 的 PR 编号（升序；issue 详情徽章数据）。
fn refs_of_pull_issue(conn: &Connection, repo: &str, issue: u64) -> rusqlite::Result<Vec<u64>> {
    let mut stmt = conn.prepare(
        "SELECT pull_number FROM hub_issue_refs \
         WHERE repo_name=? AND issue_number=? ORDER BY pull_number ASC",
    )?;
    let iter = stmt.query_map(params![repo, issue as i64], |r| {
        r.get::<_, i64>(0).map(|n| n.max(0) as u64)
    })?;
    let mut out = Vec::new();
    for n in iter {
        out.push(n?);
    }
    Ok(out)
}

// ----------------------------------------------------------------------------
// base64 / SHA-256（纯 std 实现——os-nexhub 主依赖无 base64/sha2 crate，Cargo.toml
// 冻结不新增依赖；测试用 dev-dep sha2 交叉验证正确性）
// ----------------------------------------------------------------------------

/// 标准 base64 解码（字母表 A-Za-z0-9+/，`=` 填充可选，容忍空白；其它字符 → None）。
fn b64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let cleaned: Vec<u8> = s
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
        .collect();
    if cleaned.is_empty() {
        return Some(Vec::new()); // 空串（或纯填充）→ 空字节
    }
    if cleaned.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(cleaned.len() * 3 / 4);
    for chunk in cleaned.chunks(4) {
        let mut buf: u32 = 0;
        for (i, &c) in chunk.iter().enumerate() {
            buf |= val(c)? << (18 - 6 * i);
        }
        let bytes = [(buf >> 16) as u8, (buf >> 8) as u8, buf as u8];
        out.extend_from_slice(&bytes[..chunk.len() - 1]);
    }
    Some(out)
}

/// 标准 base64 编码（带 `=` 填充；下载直传信封用——os-api 网关按标准字母表解码）。
fn b64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let mut buf: u32 = (chunk[0] as u32) << 16;
        if chunk.len() > 1 {
            buf |= (chunk[1] as u32) << 8;
        }
        if chunk.len() > 2 {
            buf |= chunk[2] as u32;
        }
        out.push(TABLE[(buf >> 18) as usize & 63] as char);
        out.push(TABLE[(buf >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(buf >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[buf as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// SHA-256 摘要 → 小写 hex（FIPS 180-4 纯实现；附件完整性校验字段）。
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    // 填充：0x80 + 0* + 8 字节大端位长
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
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
    h.iter().map(|x| format!("{x:08x}")).collect()
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
    fn route_specs_declare_sixteen_public_auth_endpoints() {
        let specs = route_specs();
        assert_eq!(specs.len(), 16, "应有 16 条路由: {specs:?}");
        assert!(specs.iter().all(|s| s.handler_component == "code_repo"));
        // 全部 handler 内自验（requires_auth=false——网关不拦链上身份）
        assert!(
            specs
                .iter()
                .all(|s| !s.requires_auth && s.required_roles.is_empty()),
            "issues/pulls/releases 路由应由 handler 自验身份: {specs:?}"
        );
        let paths: Vec<&str> = specs.iter().map(|s| s.path.as_str()).collect();
        assert!(paths.contains(&"/api/v1/coderepo/repos/:name/issues"));
        assert!(paths.contains(&"/api/v1/coderepo/repos/:name/issues/:num/close"));
        assert!(paths.contains(&"/api/v1/coderepo/repos/:name/pulls/:num/merge"));
        // Release 附件四端点（§top3）
        assert!(paths.contains(&"/api/v1/coderepo/repos/:name/releases/:tag/assets"));
        assert!(paths.contains(&"/api/v1/coderepo/repos/:name/releases/:tag/assets/:aid"));
        // 上传=POST、下载/清单=GET、删除=DELETE 各就位
        for (m, suffix) in [
            (HttpMethod::Get, "/releases/:tag/assets"),
            (HttpMethod::Post, "/releases/:tag/assets"),
            (HttpMethod::Get, "/releases/:tag/assets/:aid"),
            (HttpMethod::Delete, "/releases/:tag/assets/:aid"),
        ] {
            assert!(
                specs
                    .iter()
                    .any(|s| s.method == m && s.path == format!("{PREFIX}{suffix}")),
                "缺资产端点 {m:?} {suffix}"
            );
        }
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

    // ---- 交叉引用解析（§top5 纯函数） ----

    #[test]
    fn cross_ref_parser_accepts_three_verbs_case_insensitive() {
        // 三动词 × 原形/三单/过去式 × 大小写
        assert_eq!(cross_refs("fixes #1"), vec![1]);
        assert_eq!(cross_refs("Fixed #2"), vec![2]);
        assert_eq!(cross_refs("this CLOSES #3"), vec![3]);
        assert_eq!(cross_refs("will resolve #4"), vec![4]);
        assert_eq!(cross_refs("RESOLVED #5"), vec![5]);
        assert_eq!(cross_refs("close #6"), vec![6]);
        // 多引用 + 去重 + 首现顺序
        assert_eq!(
            cross_refs("fixes #12 and closes #7, also fixes #12 again resolves #8"),
            vec![12, 7, 8]
        );
        // 非匹配形态：无动词 / 动词内嵌（非词首）/ 无 # / 纯 # / #后无数字 / 0 号
        assert!(cross_refs("see #9").is_empty());
        assert!(cross_refs("refixes #10").is_empty(), "refixes 非词首动词");
        assert!(cross_refs("hotfix #11").is_empty(), "hotfix 非词首动词");
        assert!(cross_refs("fixes 12").is_empty(), "缺 # 前缀");
        assert!(cross_refs("fixes #").is_empty());
        assert!(cross_refs("fixes #0").is_empty(), "0 号不合法");
        assert!(cross_refs("").is_empty());
        // '#' 紧贴动词（无空白）也应命中
        assert_eq!(cross_refs("fixes#13"), vec![13]);
    }

    // ---- b64 / sha256（纯 std 实现；sha2 dev-dep 交叉验证） ----

    #[test]
    fn b64_codec_known_vectors_roundtrip() {
        let cases: &[(&str, &str)] = &[
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (plain, enc) in cases {
            assert_eq!(b64_encode(plain.as_bytes()), *enc, "编码向量 {plain:?}");
            let dec = b64_decode(enc).expect("解码应成功");
            assert_eq!(dec, plain.as_bytes(), "解码向量 {enc:?}");
        }
        // 去填充 / 含空白也可解；非法字符 → None
        assert_eq!(b64_decode("Zm9vYg"), b64_decode("Zm9vYg=="));
        assert_eq!(b64_decode("Zm9v\nYg=="), b64_decode("Zm9vYg=="));
        assert!(b64_decode("Zm9v!g==").is_none(), "非法字符应 None");
        assert!(b64_decode("A").is_none(), "残缺四字节组应 None");
        // 任意字节往返（含 0x00/0xff）
        let blob: Vec<u8> = (0u16..600).map(|i| (i % 256) as u8).collect();
        assert_eq!(b64_decode(&b64_encode(&blob)).unwrap(), blob);
    }

    #[test]
    fn sha256_hex_matches_reference_vectors() {
        // FIPS 向量（与 sha2 dev-dep 交叉验证——主实现纯 std）
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let long = vec![b'a'; 1_000_003]; // 跨多块 + 非对齐尾
        let expect = {
            use sha2::Digest;
            let mut h = sha2::Sha256::new();
            h.update(&long);
            hex::encode(h.finalize())
        };
        assert_eq!(sha256_hex(&long), expect, "长输入应与 sha2 crate 一致");
    }

    // ---- Merge 策略（§top4；HTTP 级三策略拓扑） ----

    /// `git rev-list --parents -n 1 <branch>` → [commit, parents…]。
    fn head_parents(bare: &str, branch: &str) -> Vec<String> {
        let (ok, out) = run(&[
            "git",
            &format!("--git-dir={bare}"),
            "rev-list",
            "--parents",
            "-n",
            "1",
            &format!("refs/heads/{branch}"),
        ]);
        assert!(ok, "rev-list 失败");
        out.split_whitespace().map(String::from).collect()
    }

    #[tokio::test]
    async fn merge_strategies_produce_expected_topologies() {
        let dir = tempdir();
        for name in ["mrg", "sqz", "rbz"] {
            make_repo(&dir, name, Some("feature"));
        }
        let auth = Arc::new(ChainAuth::new());
        let svc = service(&dir).with_chain_auth(auth.clone());
        let token = login(
            &auth,
            &k256::ecdsa::SigningKey::random(&mut k256::elliptic_curve::rand_core::OsRng),
        );

        async fn open_pr(svc: &IssuesService, token: &str, repo: &str) -> u64 {
            let resp = handle(
                svc,
                req_auth(
                    post_req(
                        &format!("/api/v1/coderepo/repos/{repo}/pulls"),
                        serde_json::json!({
                            "title": "合入 feature",
                            "from_branch": "feature"
                        }),
                    ),
                    token,
                ),
            )
            .await;
            assert_eq!(resp.status, 201, "{repo} 建 PR 应 201: {}", resp.body);
            resp.body["pull"]["number"].as_u64().unwrap()
        }

        // 非法策略 → 400
        let _n = open_pr(&svc, &token, "mrg").await;
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/mrg/pulls/1/merge",
                    serde_json::json!({ "merge_strategy": "bogus" }),
                ),
                TEST_ADMIN_TOKEN,
            ),
        )
        .await;
        assert_eq!(resp.status, 400, "非法策略应 400: {}", resp.body);

        // —— merge（缺省=现行为）：双 parent 合并提交 ——
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/mrg/pulls/1/merge",
                    serde_json::json!({ "merge_strategy": "merge" }),
                ),
                TEST_ADMIN_TOKEN,
            ),
        )
        .await;
        assert_eq!(resp.status, 200, "merge 策略应 200: {}", resp.body);
        assert_eq!(resp.body["merge_strategy"], "merge");
        assert_eq!(resp.body["closed_issues"], serde_json::json!([]));
        let hp = head_parents(&format!("{dir}/mrg.git"), "main");
        assert_eq!(hp.len(), 3, "merge=双 parent: {hp:?}");
        assert_eq!(hp[0], resp.body["merged_sha"].as_str().unwrap());

        // —— squash：单 parent；缺省信息=标题 (#编号)；内容等价 ——
        let n2 = open_pr(&svc, &token, "sqz").await;
        assert_eq!(n2, 1);
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/sqz/pulls/1/merge",
                    serde_json::json!({ "merge_strategy": "squash" }),
                ),
                TEST_ADMIN_TOKEN,
            ),
        )
        .await;
        assert_eq!(resp.status, 200, "squash 应 200: {}", resp.body);
        assert_eq!(resp.body["merge_strategy"], "squash");
        let sqz_bare = format!("{dir}/sqz.git");
        let hp = head_parents(&sqz_bare, "main");
        assert_eq!(hp.len(), 2, "squash=单 parent: {hp:?}");
        let (ok, msg) = run(&[
            "git",
            &format!("--git-dir={sqz_bare}"),
            "log",
            "-1",
            "--format=%s",
            "main",
        ]);
        assert!(ok);
        assert_eq!(
            msg.trim(),
            "合入 feature (#1)",
            "squash 缺省信息=标题(#编号)"
        );
        let (ok, diff) = run(&[
            "git",
            &format!("--git-dir={sqz_bare}"),
            "diff",
            "refs/heads/main..refs/heads/feature",
        ]);
        assert!(
            ok && diff.trim().is_empty(),
            "squash 后内容应与 feature 等价"
        );

        // —— rebase（此 fixture 可快进）：main 头==feature 头，原 sha 保留 ——
        let _n3 = open_pr(&svc, &token, "rbz").await;
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/rbz/pulls/1/merge",
                    serde_json::json!({ "merge_strategy": "rebase" }),
                ),
                TEST_ADMIN_TOKEN,
            ),
        )
        .await;
        assert_eq!(resp.status, 200, "rebase 应 200: {}", resp.body);
        assert_eq!(resp.body["merge_strategy"], "rebase");
        let (ok, feat) = run(&[
            "git",
            &format!("--git-dir={dir}/rbz.git"),
            "rev-parse",
            "refs/heads/feature",
        ]);
        assert!(ok);
        assert_eq!(
            resp.body["merged_sha"].as_str().unwrap(),
            feat.trim(),
            "快进 rebase 应保留 feature 原 sha"
        );
        let hp = head_parents(&format!("{dir}/rbz.git"), "main");
        assert_eq!(hp.len(), 2, "rebase 后仍单 parent（线性）");
    }

    // ---- 交叉引用 merge 联动 + 时间线 + 徽章（§top5） ----

    #[tokio::test]
    async fn cross_reference_closes_issue_on_merge_with_timeline_and_badges() {
        let dir = tempdir();
        make_repo(&dir, "demo", Some("feature"));
        let auth = Arc::new(ChainAuth::new());
        let svc = service(&dir).with_chain_auth(auth.clone());
        let sk = k256::ecdsa::SigningKey::random(&mut k256::elliptic_curve::rand_core::OsRng);
        let token = login(&auth, &sk);

        // 两个 issue：#1 将被 fixes；#2 预先手动关闭（联动应跳过已关闭）
        for title in ["崩溃", "已处理"] {
            let resp = handle(
                &svc,
                req_auth(
                    post_req(
                        "/api/v1/coderepo/repos/demo/issues",
                        serde_json::json!({ "title": title }),
                    ),
                    &token,
                ),
            )
            .await;
            assert_eq!(resp.status, 201);
        }
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/issues/2/close",
                    serde_json::json!({}),
                ),
                &token,
            ),
        )
        .await;
        assert_eq!(resp.status, 200);

        // PR body 带 fixes #1 + fixes #99（不存在）；评论再补 resolves #2（已关闭）
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/pulls",
                    serde_json::json!({
                        "title": "修复崩溃",
                        "body": "该提交 fixes #1，另 fixes #99（不存在的编号）",
                        "from_branch": "feature"
                    }),
                ),
                &token,
            ),
        )
        .await;
        assert_eq!(resp.status, 201, "建 PR 应 201: {}", resp.body);
        assert_eq!(
            resp.body["pull"]["referenced_issues"],
            serde_json::json!([1, 99]),
            "创建响应即带引用徽章数据"
        );
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/pulls/1/comments",
                    serde_json::json!({ "body": "also resolves #2" }),
                ),
                &token,
            ),
        )
        .await;
        assert_eq!(resp.status, 201);

        // 徽章：issue 列表带 referencing_pulls；PR 列表带 referenced_issues
        let resp = handle(
            &svc,
            get_req("/api/v1/coderepo/repos/demo/issues?state=all"),
        )
        .await;
        // 倒序：issues[0]=#2（评论 resolves #2 引用）、issues[1]=#1（body fixes #1）
        assert_eq!(
            resp.body["issues"][1]["referencing_pulls"],
            serde_json::json!([1]),
            "issue #1 应带引用 PR 徽章数据"
        );
        assert_eq!(
            resp.body["issues"][0]["referencing_pulls"],
            serde_json::json!([1]),
            "issue #2（评论引用）也应带徽章数据"
        );
        let resp = handle(&svc, get_req("/api/v1/coderepo/repos/demo/pulls?state=all")).await;
        assert_eq!(
            resp.body["pulls"][0]["referenced_issues"],
            serde_json::json!([1, 2, 99]),
            "PR 徽章含 body+评论引用（升序去重）"
        );

        // merge（admin）→ issue #1 自动关闭 + 时间线评论；#2 跳过
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
        assert_eq!(resp.status, 200, "merge 应 200: {}", resp.body);
        assert_eq!(
            resp.body["closed_issues"],
            serde_json::json!([1]),
            "只关闭 open 的被引用 issue（#2 已关、#99 不存在）"
        );

        // issue #1 详情：state=closed + 时间线评论 "closed via PR !1"（作者=admin）
        let resp = handle(&svc, get_req("/api/v1/coderepo/repos/demo/issues/1")).await;
        assert_eq!(resp.body["issue"]["state"], "closed", "联动关闭应生效");
        let comments = resp.body["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 1, "时间线自动评论: {}", resp.body);
        assert_eq!(comments[0]["body"], "closed via PR !1");
        assert_eq!(comments[0]["author"], "admin");
        // issue #2 保持 closed 且无多余时间线
        let resp = handle(&svc, get_req("/api/v1/coderepo/repos/demo/issues/2")).await;
        assert_eq!(resp.body["issue"]["state"], "closed");
        assert_eq!(resp.body["comments"].as_array().unwrap().len(), 0);

        // 无引用的 PR merge → closed_issues 为空（不误伤）
        make_repo(&dir, "demo2", Some("feature"));
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo2/pulls",
                    serde_json::json!({ "title": "无引用", "from_branch": "feature" }),
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
                    "/api/v1/coderepo/repos/demo2/pulls/1/merge",
                    serde_json::json!({}),
                ),
                TEST_ADMIN_TOKEN,
            ),
        )
        .await;
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["closed_issues"], serde_json::json!([]));
    }

    // ---- Release 附件（§top3）：CRUD + 权限 + 越权仓库隔离 ----

    /// 在 hub_lobby.db 写入 release 行（附件端点的存在性权威；schema 由 lobby 构造）。
    fn seed_lobby_release(dir: &str, repo: &str, tag: &str) {
        let _lobby = crate::nexhub_lobby::NexHubLobbyRouteHandler::with_db_path(
            &format!("{dir}/hub_lobby.db"),
            dir,
        );
        let conn = Connection::open(format!("{dir}/hub_lobby.db")).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO hub_releases (id,repo_name,tag,title,notes,created_by,created_at) \
             VALUES (?, ?, ?, 't', '', 'admin', datetime('now'))",
            params![format!("rel-{repo}-{tag}"), repo, tag],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn release_asset_crud_permissions_and_repo_isolation() {
        let dir = tempdir();
        make_repo(&dir, "demo", None);
        make_repo(&dir, "other", None);
        let auth = Arc::new(ChainAuth::new());
        let svc = service(&dir).with_chain_auth(auth.clone());
        let sk = k256::ecdsa::SigningKey::random(&mut k256::elliptic_curve::rand_core::OsRng);
        let token = login(&auth, &sk);

        seed_lobby_release(&dir, "demo", "v1.0.0");
        let content = b"binary payload \x00\xff\x01".to_vec();
        let b64 = b64_encode(&content);

        // release 不存在（other 仓，admin 探测）→ 404；无 token 上传 → 401
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/other/releases/v1.0.0/assets",
                    serde_json::json!({ "name": "a.bin", "content_base64": b64 }),
                ),
                TEST_ADMIN_TOKEN,
            ),
        )
        .await;
        assert_eq!(
            resp.status, 404,
            "未发版仓库的资产端点应 404: {}",
            resp.body
        );
        let resp = handle(
            &svc,
            post_req(
                "/api/v1/coderepo/repos/demo/releases/v1.0.0/assets",
                serde_json::json!({ "name": "a.bin", "content_base64": b64 }),
            ),
        )
        .await;
        assert_eq!(resp.status, 401, "无 token 上传应 401");

        // 非普通链上身份（非 owner）上传 → 403
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/releases/v1.0.0/assets",
                    serde_json::json!({ "name": "a.bin", "content_base64": b64 }),
                ),
                &token,
            ),
        )
        .await;
        assert_eq!(resp.status, 403, "非 owner 上传应 403");

        // admin 上传 → 201（sha256/size 核对）
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/releases/v1.0.0/assets",
                    serde_json::json!({ "name": "app.tar.gz", "content_base64": b64 }),
                ),
                TEST_ADMIN_TOKEN,
            ),
        )
        .await;
        assert_eq!(resp.status, 201, "admin 上传应 201: {}", resp.body);
        assert_eq!(resp.body["asset"]["name"], "app.tar.gz");
        assert_eq!(resp.body["asset"]["size"], content.len() as u64);
        assert_eq!(
            resp.body["asset"]["sha256"],
            sha256_hex(&content),
            "sha256 应为内容摘要"
        );
        let aid = resp.body["asset"]["id"].as_str().unwrap().to_string();

        // 同名 → 409；非法 b64 → 400；空内容 → 400；路径名 → 400
        let resp = handle(
            &svc,
            req_auth(
                post_req(
                    "/api/v1/coderepo/repos/demo/releases/v1.0.0/assets",
                    serde_json::json!({ "name": "app.tar.gz", "content_base64": b64 }),
                ),
                TEST_ADMIN_TOKEN,
            ),
        )
        .await;
        assert_eq!(resp.status, 409, "同名附件应 409");
        for (name, body) in [
            (
                "bad-b64",
                serde_json::json!({ "name": "x", "content_base64": "!!!" }),
            ),
            (
                "empty",
                serde_json::json!({ "name": "x", "content_base64": "" }),
            ),
            (
                "path-name",
                serde_json::json!({ "name": "../evil", "content_base64": b64 }),
            ),
        ] {
            let resp = handle(
                &svc,
                req_auth(
                    post_req("/api/v1/coderepo/repos/demo/releases/v1.0.0/assets", body),
                    TEST_ADMIN_TOKEN,
                ),
            )
            .await;
            assert_eq!(resp.status, 400, "{name} 应 400: {}", resp.body);
        }

        // 清单（公开）
        let resp = handle(
            &svc,
            get_req("/api/v1/coderepo/repos/demo/releases/v1.0.0/assets"),
        )
        .await;
        assert_eq!(resp.status, 200);
        let assets = resp.body["assets"].as_array().unwrap();
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0]["id"], aid.as_str());

        // 下载（公开）：b64 信封 + octet-stream 直传头（os-api 网关解码回原始字节）
        let resp = handle(
            &svc,
            get_req(&format!(
                "/api/v1/coderepo/repos/demo/releases/v1.0.0/assets/{aid}"
            )),
        )
        .await;
        assert_eq!(resp.status, 200, "下载应 200: {}", resp.body);
        assert_eq!(resp.headers["content-type"], "application/octet-stream");
        assert!(resp.headers["content-disposition"]
            .as_str()
            .unwrap_or("")
            .contains("app.tar.gz"));
        assert_eq!(
            resp.body.as_str().unwrap(),
            b64,
            "body=b64 信封（网关 direct-passthrough 解码）"
        );

        // 越权仓库隔离：other 仓同 aid / 同 tag 下载 → 404（repo 维度双重定位）
        for path in [
            format!("/api/v1/coderepo/repos/other/releases/v1.0.0/assets/{aid}"),
            format!("/api/v1/coderepo/repos/demo/releases/v9.9.9/assets/{aid}"),
            "/api/v1/coderepo/repos/demo/releases/v1.0.0/assets/ast-nope".to_string(),
        ] {
            let resp = handle(&svc, get_req(&path)).await;
            assert_eq!(resp.status, 404, "{path} 应 404（隔离/不存在）");
        }

        // 删除：无 token 401 → 非普通身份 403 → admin 200 → 再下载 404
        let resp = handle(
            &svc,
            del_req(&format!(
                "/api/v1/coderepo/repos/demo/releases/v1.0.0/assets/{aid}"
            )),
        )
        .await;
        assert_eq!(resp.status, 401);
        let resp = handle(
            &svc,
            req_auth_del(
                &format!("/api/v1/coderepo/repos/demo/releases/v1.0.0/assets/{aid}"),
                &token,
            ),
        )
        .await;
        assert_eq!(resp.status, 403);
        let resp = handle(
            &svc,
            req_auth_del(
                &format!("/api/v1/coderepo/repos/demo/releases/v1.0.0/assets/{aid}"),
                TEST_ADMIN_TOKEN,
            ),
        )
        .await;
        assert_eq!(resp.status, 200, "admin 删除应 200: {}", resp.body);
        assert_eq!(resp.body["action"], "asset_delete");
        let resp = handle(
            &svc,
            get_req(&format!(
                "/api/v1/coderepo/repos/demo/releases/v1.0.0/assets/{aid}"
            )),
        )
        .await;
        assert_eq!(resp.status, 404, "删除后下载应 404");
        let resp = handle(
            &svc,
            req_auth_del(
                &format!("/api/v1/coderepo/repos/demo/releases/v1.0.0/assets/{aid}"),
                TEST_ADMIN_TOKEN,
            ),
        )
        .await;
        assert_eq!(resp.status, 404, "重复删除应 404");
    }

    /// DELETE 请求构造（测试辅助）。
    fn del_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Delete,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
        }
    }

    /// 带 Bearer 的 DELETE（path 直构）。
    fn req_auth_del(path: &str, token: &str) -> ApiRequest {
        let mut r = del_req(path);
        r.headers = serde_json::json!({ "authorization": format!("Bearer {token}") });
        r
    }
}
