//! os-nexhub —— NexHub：代码仓库中心 + 大厅发现层（从 os-api 抽离的独立 crate）。
//!
//! 定位（审计 docs/COMPONENT_INDEPENDENCE_AUDIT.md §6「NexHub 独立化专项」）：
//! NexHub 两大 RouteHandler 原生长在 os-api 的 `handlers/` 目录，现抽成独立 crate，
//! 经 [`os_common::gateway::RouteHandler`] 轻量契约与网关对接；os-api 仍是组合根
//! （网关装配 / 中间件 / `/git/*` Smart HTTP CGI），在装配层把本 crate 的 handler
//! 桥接为网关版 `RouteHandler`（错误 `HandlerError → ApiGatewayError` 身份映射）。
//!
//! # 模块
//!
//! - [`code_repo`]：`CodeRepoRouteHandler` —— 代码仓库中心（**原生系统 git** 裸仓库
//!   CRUD + 文件树/提交历史浏览 + 目录导入 + AI 会话归档），路由前缀
//!   `/api/v1/coderepo/*`（12 条，component="code_repo"）。
//! - [`issues`]：`IssuesService` —— 项目级 Issues + Pull Requests 协作层
//!   （2026-08-24）：挂在 code_repo 名下（`/api/v1/coderepo/repos/:name/issues|pulls*`，
//!   12 条）——没有更改权限的 agent 用链上身份开 Issue/评论/提 PR，merge 仅
//!   admin/仓库 owner（大厅 publisher）；SQLite `hub_repo_*` 表 + git merge-tree
//!   （复用大厅实现），文档 docs/NEXHUB_ISSUES_PR.md。
//! - [`nexhub_lobby`]：`NexHubLobbyRouteHandler` —— NexHub 大厅发现层（SQLite
//!   `hub_lobby` 发布索引 + 元数据快照 + 一键克隆 + 悬赏 + 链上身份认证 + PR
//!   审核流（`hub_pull_requests`）+ 发版权限控制（`hub_releases`）+ nexos 自动
//!   联邦），路由前缀 `/api/v1/nexhub/{lobby,bounty,auth}/*`（28 条，
//!   component="nexhub-lobby"；设计文档 docs/NEXHUB_LOBBY_DESIGN.md 与
//!   docs/MEDIA_GEN_AND_CHAIN_AUTH.md §C——身份=secp256k1 公钥，
//!   `challenge|verify` 挑战-签名 + 写端点 owner/buyer/hunter/poster 全部
//!   token 反查，body 自报身份一律忽略）。
//! - [`lobby_sync_hook`]：post-receive 自动同步钩子（2026-08-25，设计文档 §15）——
//!   git push nexos.git → 后台 curl publish（刷新 latest_commit/pushed_at 快照）
//!   与 federate（联邦重广播）；启动 ensure 流程幂等补装（缺则写、漂移则覆盖、
//!   用户自管不动），大厅条目随仓库最新提交自动更新。
//!
//! # 运行期契约（NexHub 独立化保持；链上身份为 2026-08-18 增量）
//!
//! - 路由：`/api/v1/coderepo/*`（含项目级协作 `/repos/:name/issues|pulls*`）、
//!   `/api/v1/nexhub/{lobby,bounty,auth}/*`、`/git/*`
//!   （Smart HTTP 由 os-api 网关装配，clone URL 仍指向
//!   `http://<host>:<port>/git/<name>.git`）。
//! - 环境变量：`NEXOS_GIT_REPOS_DIR`/`OS_GIT_REPOS_DIR`（仓库根，默认
//!   `/tank/git-repos`）、`NEXOS_GIT_USER`/`OS_GIT_USER`、`NEXOS_GIT_HOST`/`OS_GIT_HOST`、
//!   `NEXOS_HTTP_PORT`/`OS_HTTP_PORT`（HTTP clone URL 端口，默认 8080）、
//!   `NEXOS_ADMIN_TOKEN`/`OS_ADMIN_TOKEN`（链上 token 之外的 admin 回落通道，
//!   与 os-api 网关同一变量；未设置则仅链上身份可写）。
//! - DB 路径：大厅 `hub_lobby` 与项目协作 `repo_issues` 各自独立——
//!   `/tank/os-data/{hub_lobby,repo_issues}.db` → `/var/lib/os/{...}.db` →
//!   `./{...}.db` 三级回退（构造注入 `with_db_path` / `with_paths` 可覆盖）。
//! - 外部依赖：PATH 上需有系统 `git` 二进制（git 失败降级不 panic）。
//!
//! # 独立维护
//!
//! - 依赖面：仅 os-common（契约）+ tokio/rusqlite/serde/chrono，**不依赖 os-api**。
//! - os-api 经 `os-nexhub.workspace = true` 引入并在 main.rs 注册两个 handler；
//!   `/git/*` CGI 的仓库根回退用 [`repos_dir`]。
//! - 测试自足：`cargo test -p os-nexhub`（tempdir/内存库隔离，含 issues.rs
//!   协作层 9 测 + lobby/code_repo 既有全量）。

pub mod chain_verify;
pub mod code_repo;
pub mod issues;
pub mod lobby_sync_hook;
pub mod nexhub_lobby;

pub use code_repo::{
    build_clone_url, build_clone_url_http, build_clone_url_http_with, build_clone_url_with,
    build_create_repo_cmd, build_import_script, parse_git_log, parse_git_ls_tree, repos_dir,
    CodeRepoRouteHandler,
};
pub use issues::{IssuesService, RepoComment, RepoIssue, RepoPull};
pub use lobby_sync_hook::{build_post_receive_hook_script, ensure_post_receive_hook, HOOK_MARKER};
pub use nexhub_lobby::{
    build_nexhub_lobby_fed_payload, build_nexhub_release_fed_payload, excerpt_of, normalize_sort,
    sanitize_fed_node, LatestCommit, LobbyEntry, LobbyFedEndpoint, LobbyFedIngest,
    LobbyFedTransport, NexHubLobbyRouteHandler, PullRequest, Release, FED_KIND_NEXHUB_LOBBY,
    FED_KIND_NEXHUB_RELEASE,
};
