//! `CodeRepoRouteHandler` —— 代码仓库中心（**原生 git 管理**，零依赖，立即可用）。
//!
//! 本模块原长在 os-api `handlers/code_repo.rs`，NexHub 独立化（审计
//! docs/COMPONENT_INDEPENDENCE_AUDIT.md §6）后随 crate 迁入 os-nexhub，经
//! `os_common::gateway::RouteHandler` 轻量契约与网关对接（os-api 装配层桥接）。
//!
//! 定位：把网关 HTTP 请求（`/api/v1/coderepo/*`）翻译为**系统 git 子进程**调用，
//! 直接管理 `/tank/git-repos/` 下的裸仓库（`<name>.git`）。不再依赖 Gitea/Docker。
//!
//! # 设计
//!
//! - **仓库存储**：所有裸仓库放在 `repos_dir()`（默认 `/tank/git-repos`，可用环境变量
//!   `OS_GIT_REPOS_DIR` 覆盖，便于测试隔离）。
//! - **git 操作**：用 `tokio::process::Command`（async）或 `std::process::Command`
//!   （spawn_blocking 内）真实 spawn 系统 `git`。**git 失败降级不 panic**。
//! - **clone URL**：`ssh://oem@<host>:/tank/git-repos/<name>.git`（host 用
//!   [`advertise_host`] 地址优先链：`NEXOS_GIT_ADVERTISE_HOST` 显式覆盖 → 本机
//!   非回环 IPv4 → hostname 保底；用户用 `OS_GIT_USER` 覆盖，默认 `oem`）。
//! - **HTTP clone URL**：`http://<host>:<port>/git/<name>.git`（Smart Git，见
//!   [`build_clone_url_http`]；读匿名/写 token；端口用 `NEXOS_HTTP_PORT` 覆盖，
//!   默认 `8080`）。
//!
//! # 路由表（24 条，component="code_repo"；12 条仓库中心原生路由 +
//! 12 条 Issues/PR 协作路由（见 [`crate::issues`]，2026-08-24 增量））
//!
//! | method | path                                       | 动作 |
//! |--------|--------------------------------------------|------|
//! | GET    | `/api/v1/coderepo/repos`                  | 列仓库（扫描 `*.git`）|
//! | POST   | `/api/v1/coderepo/repos`                  | 创建裸仓库（admin）→ `git init --bare` + HEAD→main |
//! | DELETE | `/api/v1/coderepo/repos/:name`            | 删仓库（admin）→ `rm -rf` |
//! | GET    | `/api/v1/coderepo/repos/:name/contents`   | 文件树（`git ls-tree -r -t HEAD`）+ 分支 |
//! | GET    | `/api/v1/coderepo/repos/:name/file`       | 文件内容（`git show HEAD:<path>`）|
//! | GET    | `/api/v1/coderepo/repos/:name/commits`    | 提交历史（`git log`）|
//! | POST   | `/api/v1/coderepo/repos/:name/clone-url`  | 获取 clone URL（ssh + http，admin）|
//! | POST   | `/api/v1/coderepo/repos/:name/import`     | 导入目录为仓库（admin）|
//! | GET    | `/api/v1/coderepo/sessions`               | 列 AI 会话记录 |
//! | POST   | `/api/v1/coderepo/sessions`               | 创建会话记录（admin）|
//! | POST   | `/api/v1/coderepo/sessions/:id/end`       | 结束会话（admin）|
//! | GET    | `/api/v1/coderepo/stats`                  | {repo_count, total_size, session_count, total_commits} |
//! | GET/POST | `/api/v1/coderepo/repos/:name/issues`（及 `/:num`、`/comments`、`/close`、`/open`）| Issues 协作（[`crate::issues`]）|
//! | GET/POST | `/api/v1/coderepo/repos/:name/pulls`（及 `/:num`、`/comments`、`/merge`、`/close`）| Pull Requests 协作（[`crate::issues`]）|

use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use os_common::gateway::{
    ApiRequest, ApiResponse, HandlerError, HttpMethod, RouteHandler, RouteSpec,
};

use crate::issues::IssuesService;

// ----------------------------------------------------------------------------
// 配置（纯函数，env 可覆盖）
// ----------------------------------------------------------------------------

/// 仓库根目录（裸仓库 `<name>.git` 的父目录）。默认 `/tank/git-repos`，
/// 可用 `OS_GIT_REPOS_DIR` 覆盖（测试隔离）。
#[must_use]
pub fn repos_dir() -> String {
    std::env::var("NEXOS_GIT_REPOS_DIR")
        .or_else(|_| std::env::var("OS_GIT_REPOS_DIR"))
        .unwrap_or_else(|_| "/tank/git-repos".to_string())
}

/// git 访问用户名（clone URL 中的 user）。默认 `oem`，可用 `OS_GIT_USER` 覆盖。
fn git_user() -> String {
    std::env::var("NEXOS_GIT_USER")
        .or_else(|_| std::env::var("OS_GIT_USER"))
        .unwrap_or_else(|_| "oem".to_string())
}

/// 本机 hostname（缓存；可用 `OS_GIT_HOST` 覆盖，默认 `localhost`）。
///
/// 注意：hostname（如 `ub2604`）只有本机/本地 DNS 能解析，**跨节点不可达**——
/// 对外广播的 clone URL 主机应走 [`advertise_host`] 地址优先链。
fn cached_hostname() -> String {
    use std::sync::OnceLock;
    static HOST: OnceLock<String> = OnceLock::new();
    HOST.get_or_init(|| {
        std::env::var("NEXOS_GIT_HOST")
            .or_else(|_| std::env::var("OS_GIT_HOST"))
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                std::process::Command::new("hostname")
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "localhost".to_string())
            })
    })
    .clone()
}

/// 探测本机非回环 IPv4：UDP connect `8.8.8.8:80` 选路手法（仅选路不发包，
/// 不需要真实网络连通——内核按默认路由挑出口地址后即返回；与 os-api
/// handlers/node_view.rs、os-p2p api.rs 测试同款）。
///
/// 返回 `None` = 无可用非回环 IPv4（离线/无默认路由），调用方回退 hostname。
fn local_non_loopback_ipv4() -> Option<String> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    match s.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(v4) if !v4.is_loopback() => Some(v4.to_string()),
        _ => None,
    }
}

/// 地址优先链（纯函数，便于单测三态）：显式覆盖 → 选路 IP → hostname 保底。
///
/// 优先级：
/// 1. `env_override`（`NEXOS_GIT_ADVERTISE_HOST`）：显式覆盖，最高优先——
///    运维指定对外广播地址（多网卡/反代/NAT 场景）；
/// 2. `probed_ip`（[`local_non_loopback_ipv4`]）：默认路由出口 IP，跨节点
///    直接可达（联邦其他节点解析不了本机 hostname）；
/// 3. `fallback_hostname`（[`cached_hostname`]）：保底——本地网络若配了
///    主机名解析（DNS/mDNS）仍可用。
#[must_use]
pub fn resolve_advertise_host_with(
    env_override: Option<&str>,
    probed_ip: Option<&str>,
    fallback_hostname: &str,
) -> String {
    if let Some(h) = env_override.map(str::trim).filter(|s| !s.is_empty()) {
        return h.to_string();
    }
    if let Some(ip) = probed_ip.map(str::trim).filter(|s| !s.is_empty()) {
        return ip.to_string();
    }
    fallback_hostname.to_string()
}

/// 对外广播的 clone URL 主机（地址优先链，见 [`resolve_advertise_host_with`]）。
///
/// env `NEXOS_GIT_ADVERTISE_HOST` 不缓存（显式覆盖点，改 env 即时生效——
/// publish/详情响应里的 clone_url 每次构造都会重新取值，重 publish 即刷新）。
fn advertise_host() -> String {
    resolve_advertise_host_with(
        std::env::var("NEXOS_GIT_ADVERTISE_HOST").ok().as_deref(),
        local_non_loopback_ipv4().as_deref(),
        &cached_hostname(),
    )
}

/// 构造 SSH clone URL：`ssh://<user>@<host>:<repos_dir>/<name>.git`。
///
/// host 用 [`advertise_host`] 地址优先链（跨节点可达）。
#[must_use]
pub fn build_clone_url(name: &str) -> String {
    build_clone_url_with(name, &git_user(), &advertise_host(), &repos_dir())
}

/// 纯函数版 clone URL 构造（不读 env / 不缓存，便于单测）。
#[must_use]
pub fn build_clone_url_with(name: &str, user: &str, host: &str, repos_dir: &str) -> String {
    format!("ssh://{user}@{host}:{repos_dir}/{name}.git")
}

/// HTTP 服务端口（HTTP clone URL 用）。读 `NEXOS_HTTP_PORT`（回退 `OS_HTTP_PORT`），
/// 默认 `8080`（与 `main.rs` 的 `--addr` 默认值一致）。
fn http_port() -> String {
    std::env::var("NEXOS_HTTP_PORT")
        .or_else(|_| std::env::var("OS_HTTP_PORT"))
        .unwrap_or_else(|_| "8080".to_string())
}

/// 构造 HTTP Smart Git clone URL：`http://<host>:<port>/git/<name>.git`。
///
/// 对应 os-api 网关装配的 axum 路由 `/git/{*path}`（git CGI 留在 os-api http.rs，
/// 读匿名/写 token 认证——URL 字符串契约而非代码契约，见审计 §6.3 方案甲），
/// host 用 [`advertise_host`] 地址优先链（env `NEXOS_GIT_ADVERTISE_HOST` →
/// 本机非回环 IPv4 → hostname 保底——hostname 如 `ub2604` 跨节点解析不了，
/// 广播地址必须是可达 IP/显式覆盖），端口见 `http_port`。
///
/// # 鉴权用法（clone 免凭据；仅 push 需要 token）
///
/// 返回的 URL **不含凭据**。2026-08-25 起 git HTTP 读（upload-pack：clone/fetch）
/// **匿名放行**——直接 `git clone <本 URL>` 即可；仅写（receive-pack：push）
/// 要求 token（用户名任意非空、密码为 `NEXOS_ADMIN_TOKEN`）：
///
/// ```text
/// git clone http://host:8080/git/<name>.git                    # 匿名读，免凭据
/// git push http://用户名:TOKEN@host:8080/git/<name>.git main   # 写需 token
/// ```
#[must_use]
pub fn build_clone_url_http(name: &str) -> String {
    build_clone_url_http_with(name, &advertise_host(), &http_port())
}

/// 纯函数版 HTTP clone URL 构造（不读 env / 不缓存，便于单测）。
#[must_use]
pub fn build_clone_url_http_with(name: &str, host: &str, port: &str) -> String {
    format!("http://{host}:{port}/git/{name}.git")
}

/// 校验仓库名：非空、不含 `/` 与 `.`/`..` 段、不以 `-` 开头（避免 git 参数注入与路径穿越）。
/// 返回 `Err(msg)` 表示非法。
pub(crate) fn validate_repo_name(name: &str) -> Result<(), String> {
    let n = name.trim();
    if n.is_empty() {
        return Err("name 不可为空".into());
    }
    if n.starts_with('-') {
        return Err("name 不可以 '-' 开头".into());
    }
    // 仓库名不可含 '/'（否则会创建子目录 / 路径穿越）
    if n.contains('/') {
        return Err("name 不可包含 '/'".into());
    }
    if n == ".." || n == "." {
        return Err("name 不可为 '.' 或 '..'".into());
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 仓库信息（扫描裸仓库目录 + git 元数据投影）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repo {
    /// 仓库名（不含 `.git`）。
    pub name: String,
    /// 描述（读自裸仓库 `description` 文件；默认文本视为空）。
    pub description: String,
    /// 仓库占用字节（递归求和）。
    pub size_bytes: u64,
    /// 最近一次提交摘要（`<short-hash> - <subject>`；空仓库为 None）。
    pub last_commit: Option<String>,
    /// 最近一次提交日期（ISO；空仓库为 None）。
    pub last_commit_date: Option<String>,
    /// 分支数。
    pub branch_count: u32,
    /// 提交数（所有分支）。
    pub commit_count: u32,
    /// SSH clone URL。
    pub clone_url_ssh: String,
    /// HTTP clone URL（Smart Git `/git/{name}.git`，读匿名/写 token；见
    /// [`build_clone_url_http`]——地址不含 token，用法见其文档）。
    pub clone_url_http: Option<String>,
}

/// 文件树节点。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileTreeNode {
    /// 名称。
    pub name: String,
    /// 路径（相对仓库根）。
    pub path: String,
    /// 是否目录。
    pub is_dir: bool,
    /// 大小（字节，目录为 None）。
    pub size: Option<u64>,
}

/// 提交信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitInfo {
    /// 完整 hash。
    pub hash: String,
    /// 作者。
    pub author: String,
    /// 提交信息。
    pub message: String,
    /// 日期（ISO）。
    pub date: String,
}

/// AI 会话归档记录（哪个 agent 会话创建了什么仓库，形成项目时间线）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    /// 会话 id。
    pub id: String,
    /// agent 名称（`claude-code` / `codex` / `zcode` 等）。
    pub agent_name: String,
    /// 关联的仓库名。
    pub repo_name: String,
    /// 会话摘要（做了什么）。
    pub session_summary: String,
    /// 变更文件数。
    pub files_changed: u32,
    /// 提交数。
    pub commits: u32,
    /// 开始时间。
    pub started_at: String,
    /// 结束时间（None = 进行中）。
    pub ended_at: Option<String>,
}

/// 创建仓库请求体。
#[derive(Debug, Deserialize)]
struct CreateRepoBody {
    name: String,
    #[serde(default)]
    description: Option<String>,
}

/// 导入目录请求体。
#[derive(Debug, Deserialize)]
struct ImportBody {
    source_dir: String,
}

/// 创建会话请求体。
#[derive(Debug, Deserialize)]
struct CreateSessionBody {
    agent_name: String,
    repo_name: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    files_changed: Option<u32>,
    #[serde(default)]
    commits: Option<u32>,
}

// ----------------------------------------------------------------------------
// 纯函数：git 输出解析（可单测）
// ----------------------------------------------------------------------------

/// 解析 `git ls-tree -r -t HEAD` 标准输出为文件树节点列表。
///
/// 标准行格式：`<mode> SP <type> SP <hash> TAB <path>`，例如：
/// ```text
/// 100644 blob a1b2c3d4e5...\tsrc/main.rs
/// 040000 tree f9e8d7c6...\tsrc
/// ```
/// 空串/异常行静默跳过（降级）。
#[must_use]
pub fn parse_git_ls_tree(output: &str) -> Vec<FileTreeNode> {
    let mut out = Vec::new();
    for line in output.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        // 以最后一个 TAB 分隔 path
        let Some((meta, path)) = line.rsplit_once('\t') else {
            continue;
        };
        // meta = "<mode> <type> <hash>"
        let mut it = meta.split_whitespace();
        let _mode = it.next();
        let typ = it.next().unwrap_or("");
        let name = std::path::Path::new(path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());
        let is_dir = typ == "tree";
        out.push(FileTreeNode {
            name,
            path: path.to_string(),
            is_dir,
            size: None,
        });
    }
    out
}

/// 解析 `git log --format=%H%x1f%an%x1f%s%x1f%ai` 输出为提交列表。
///
/// 字段以 `\x1f`（Unit Separator）分隔，每行一条提交。空串 → 空 vec。
#[must_use]
pub fn parse_git_log(output: &str) -> Vec<CommitInfo> {
    let mut out = Vec::new();
    for line in output.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\x1f').collect();
        if parts.len() < 4 {
            continue;
        }
        out.push(CommitInfo {
            hash: parts[0].to_string(),
            author: parts[1].to_string(),
            message: parts[2].to_string(),
            date: parts[3].to_string(),
        });
    }
    out
}

// ----------------------------------------------------------------------------
// 纯函数：git 命令构造器（可单测，不执行）
// ----------------------------------------------------------------------------

/// 构造 `git init --bare <repo>` 命令。
///
/// 注意：init 本身不保证默认分支是 main（未配全局 `init.defaultBranch` 时落在
/// 系统默认，常见 master）——[`CodeRepoRouteHandler::create_repo_async`] 会在
/// init 后追加 `git symbolic-ref HEAD refs/heads/main` 显式定格，代码自洽
/// 不依赖环境配置。
#[must_use]
pub fn build_create_repo_cmd(repos_dir: &str, name: &str) -> Vec<String> {
    vec![
        "git".into(),
        "init".into(),
        "--bare".into(),
        format!("{repos_dir}/{name}.git"),
    ]
}

/// 构造导入目录为仓库的 shell 脚本（`sh -c` 执行）。
///
/// 流程：`cd <source> && git init && git add -A && git commit && git push <bare> HEAD:<branch>`。
/// 用 `git -c user.name=... -c user.email=...` 避免依赖全局 git 配置。
/// `git commit` 用 `{ ... || true; }` 包裹——**无变更（nothing to commit）也不中断**，
/// 确保 `git push` 总能执行（目标目录可能已是干净的 git 仓库，仅需把既有 HEAD 推到裸仓库）。
#[must_use]
pub fn build_import_script(repos_dir: &str, name: &str, source_dir: &str, branch: &str) -> String {
    let bare = format!("{repos_dir}/{name}.git");
    format!(
        "cd '{source}' && git init && git add -A && \
         {{ git -c user.name='OS' -c user.email='os@local' commit -m 'Import from {source}' || true; }} && \
         git push '{bare}' 'HEAD:{branch}'",
        source = source_dir,
        bare = bare,
        branch = branch,
    )
}

// ----------------------------------------------------------------------------
// 文件系统辅助（blocking）
// ----------------------------------------------------------------------------

/// 递归求目录总字节（用于仓库 size）。失败返回 0，不 panic。
fn dir_size_bytes(path: &str) -> u64 {
    let mut total: u64 = 0;
    let mut stack: Vec<std::path::PathBuf> = vec![std::path::PathBuf::from(path)];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                if let Ok(meta) = e.metadata() {
                    if meta.is_dir() {
                        stack.push(e.path());
                    } else {
                        total += meta.len();
                    }
                }
            }
        }
    }
    total
}

/// 读取裸仓库 `description` 文件；默认文本（"Unnamed repository..."）视为空。
fn read_description(bare: &str) -> String {
    let raw = std::fs::read_to_string(format!("{bare}/description")).unwrap_or_default();
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.starts_with("Unnamed repository") {
        String::new()
    } else {
        trimmed.to_string()
    }
}

/// 同步执行 `git --git-dir=<bare> <args>`，返回 `(success, stdout)`。失败降级 `(false, "")`。
fn run_git_sync(git_dir: &str, args: &[&str]) -> (bool, String) {
    let mut cmd = std::process::Command::new("git");
    cmd.arg(format!("--git-dir={git_dir}"));
    cmd.args(args);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.stdin(std::process::Stdio::null());
    match cmd.output() {
        Ok(out) => (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string(),
        ),
        Err(_) => (false, String::new()),
    }
}

/// 分支是否真实存在（`git rev-parse --verify --quiet refs/heads/<branch>`）。
/// 用全 ref 形式（不以 `-` 开头）杜绝分支名被解析为 git 选项的边界。
/// （pub(crate)：issues.rs 的 PR 分支存在性校验复用。）
pub(crate) fn branch_exists_sync(bare: &str, branch: &str) -> bool {
    run_git_sync(
        bare,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .0
}

/// 解析裸仓库的**有效默认分支**（blocking；`scan_repos_blocking` 与
/// nexhub-lobby 快照共用）——读 README/最后提交等内容时应以此为源，而非裸 HEAD：
///
/// 1. `git symbolic-ref --short HEAD` 读 HEAD 指向的分支名（失败/空 → `master`）；
/// 2. 该分支实际存在 → 直接命中（存量 master 仓 / 建仓即 main 的新仓）；
/// 3. 不存在（空仓，或只推了非 HEAD 分支——如 init 落 master 而用户只推 main，
///    外部 agent 接入实测踩到的坑）→ 回退探测 **main → master**，取第一个
///    实际存在的分支；
/// 4. 全都不存在（真空仓）→ 返回 HEAD 名（调用方 `git log`/`git show` 失败
///    降级为 None/空，不 panic）。
///
/// 返回短分支名；拼 `refs/heads/<name>` 全 ref 后使用。
pub fn resolve_default_branch_sync(bare: &str) -> String {
    let (_, out) = run_git_sync(bare, &["symbolic-ref", "--short", "HEAD"]);
    let head = out.trim();
    let head = if head.is_empty() { "master" } else { head };
    if branch_exists_sync(bare, head) {
        return head.to_string();
    }
    for cand in ["main", "master"] {
        if branch_exists_sync(bare, cand) {
            return cand.to_string();
        }
    }
    head.to_string()
}

/// 扫描 `repos_dir` 下所有 `<name>.git` 裸仓库，返回 [`Repo`] 列表（blocking）。
fn scan_repos_blocking(repos_dir: &str) -> Vec<Repo> {
    let mut out = Vec::new();
    let _ = std::fs::create_dir_all(repos_dir);
    let rd = match std::fs::read_dir(repos_dir) {
        Ok(rd) => rd,
        Err(_) => return out,
    };
    let mut names: Vec<String> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".git") && e.metadata().map(|m| m.is_dir()).unwrap_or(false) {
                Some(name.trim_end_matches(".git").to_string())
            } else {
                None
            }
        })
        .collect();
    names.sort();
    for name in names {
        let bare = format!("{repos_dir}/{name}.git");
        let size_bytes = dir_size_bytes(&bare);
        let description = read_description(&bare);
        // 最近一次提交：<short> \x1f <subject> \x1f <date>
        // （用有效默认分支而非裸 HEAD——存量仓 HEAD 可能指向不存在的分支，
        // 如 init 落 master 而用户只推 main，裸 HEAD 解析不到内容；见
        // resolve_default_branch_sync 的回退探测）
        let branch_ref = format!("refs/heads/{}", resolve_default_branch_sync(&bare));
        let (lok, lout) = run_git_sync(
            &bare,
            &["log", "-1", "--format=%h\x1f%s\x1f%ai", &branch_ref],
        );
        let (last_commit, last_commit_date) = if lok {
            let parts: Vec<&str> = lout.trim_end().split('\x1f').collect();
            if parts.len() >= 3 {
                (
                    Some(format!("{} - {}", parts[0], parts[1])),
                    Some(parts[2].to_string()),
                )
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };
        // 分支数
        let (bok, bout) = run_git_sync(
            &bare,
            &["for-each-ref", "--format=%(refname)", "refs/heads"],
        );
        let branch_count = if bok {
            bout.lines().filter(|l| !l.trim().is_empty()).count() as u32
        } else {
            0
        };
        // 提交数（所有分支）
        let (cok, cout) = run_git_sync(&bare, &["rev-list", "--count", "--all"]);
        let commit_count = if cok {
            cout.trim().parse::<u32>().unwrap_or(0)
        } else {
            0
        };
        let clone_url_ssh = build_clone_url(&name);
        let clone_url_http = build_clone_url_http(&name);
        out.push(Repo {
            name,
            description,
            size_bytes,
            last_commit,
            last_commit_date,
            branch_count,
            commit_count,
            clone_url_ssh,
            clone_url_http: Some(clone_url_http),
        });
    }
    out
}

// ----------------------------------------------------------------------------
// CodeRepoRouteHandler
// ----------------------------------------------------------------------------

/// 代码仓库中心路由处理器——HTTP 边界适配到**原生系统 git**（裸仓库 CRUD +
/// 文件浏览 + 提交历史 + 目录导入 + AI 会话归档）+ **项目级 Issues / Pull
/// Requests 协作层**（[`IssuesService`]，2026-08-24——没有更改权限的 agent
/// 用链上身份在项目上交流；merge 仅 admin/仓库 owner）。**git 失败降级不 panic**。
pub struct CodeRepoRouteHandler {
    /// AI 会话记录（内存态）。
    sessions: Mutex<Vec<AgentSession>>,
    /// 会话 id 计数器。
    counter: Mutex<u64>,
    /// Issues / PR 协作服务（SQLite hub_repo_* 表 + git merge-tree，见 issues.rs）。
    issues: IssuesService,
}

impl CodeRepoRouteHandler {
    /// 构造 handler（空会话列表 + 协作服务落默认 DB 路径）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(vec![]),
            counter: Mutex::new(100),
            issues: IssuesService::new(),
        }
    }

    /// 用空列表构造（测试注入；协作服务用内存库——零文件副作用）。
    #[must_use]
    pub fn with_empty() -> Self {
        Self {
            sessions: Mutex::new(vec![]),
            counter: Mutex::new(100),
            issues: IssuesService::in_memory(),
        }
    }

    /// 注入协作服务（测试：临时 DB / 仓库根 / 链上身份全定格，不读 env）。
    #[must_use]
    pub fn with_issues(issues: IssuesService) -> Self {
        Self {
            sessions: Mutex::new(vec![]),
            counter: Mutex::new(100),
            issues,
        }
    }

    /// 当前会话快照。
    #[must_use]
    pub fn sessions_snapshot(&self) -> Vec<AgentSession> {
        self.sessions.lock().expect("sessions poisoned").clone()
    }

    fn next_id(&self, prefix: &str) -> String {
        let mut c = self.counter.lock().expect("counter poisoned");
        *c += 1;
        format!("{prefix}-{}", *c)
    }

    /// 通用 spawn 命令（stdout 为主，stderr 诊断），失败降级 `(false, 原因)` 不 panic。
    async fn spawn_command(cmd: &[String]) -> (bool, String) {
        if cmd.is_empty() {
            return (false, "空命令".into());
        }
        let mut c = tokio::process::Command::new(&cmd[0]);
        c.args(&cmd[1..]);
        c.stdout(std::process::Stdio::piped());
        c.stderr(std::process::Stdio::piped());
        c.stdin(std::process::Stdio::null());
        match c.output().await {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                let combined = if stdout.is_empty() {
                    stderr
                } else if stderr.is_empty() {
                    stdout
                } else {
                    format!("{stdout} | {stderr}")
                };
                (out.status.success(), combined)
            }
            Err(e) => (false, format!("`{}` 调用失败（未安装？）: {e}", cmd[0])),
        }
    }

    /// 在裸仓库内执行 git（`git --git-dir=<bare> <args>`），返回 `(success, stdout)`。
    async fn run_git_in_repo(bare: &str, args: &[&str]) -> (bool, String) {
        let mut cmd: Vec<String> = vec!["git".into(), format!("--git-dir={bare}")];
        cmd.extend(args.iter().map(|s| s.to_string()));
        Self::spawn_command(&cmd).await
    }

    /// 确保仓库根目录存在（mkdir -p）。
    fn ensure_repos_dir(repos_dir: &str) -> Result<(), String> {
        std::fs::create_dir_all(repos_dir)
            .map_err(|e| format!("创建仓库根目录 {repos_dir} 失败: {e}"))
    }

    /// 探测裸仓库的默认分支（`git symbolic-ref --short HEAD`，失败默认 master）。
    /// 返回的是 HEAD **声明**的分支（import 的推送目标），不校验其是否存在——
    /// 读内容的回退探测见 [`resolve_default_branch_sync`]。
    async fn default_branch(bare: &str) -> String {
        let (ok, out) = Self::run_git_in_repo(bare, &["symbolic-ref", "--short", "HEAD"]).await;
        if ok {
            out.trim().to_string()
        } else {
            "master".to_string()
        }
    }

    /// 创建裸仓库（git init --bare + HEAD 指向 main），并写入 description。
    ///
    /// init 后显式 `git symbolic-ref HEAD refs/heads/main` 定格默认分支：全局
    /// 未配 `init.defaultBranch` 时 init 的 HEAD 落在系统默认（常见 master），
    /// 外部 agent 直接推 main 后裸 HEAD 解析不到内容（快照/浏览读空）——显式
    /// 指向让建仓契约与主流 `main` 对齐，不依赖环境层配置。
    async fn create_repo_async(
        repos_dir: &str,
        name: &str,
        description: &str,
    ) -> Result<(), String> {
        Self::ensure_repos_dir(repos_dir)?;
        let bare = format!("{repos_dir}/{name}.git");
        let cmd = build_create_repo_cmd(repos_dir, name);
        let (ok, out) = Self::spawn_command(&cmd).await;
        if !ok {
            return Err(format!("git init --bare 失败: {out}"));
        }
        let (sok, sout) =
            Self::run_git_in_repo(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]).await;
        if !sok {
            return Err(format!(
                "git symbolic-ref HEAD refs/heads/main 失败: {sout}"
            ));
        }
        if !description.is_empty() {
            let _ = std::fs::write(format!("{bare}/description"), description);
        }
        Ok(())
    }

    /// 导入现有目录为仓库：在工作目录 git init + add + commit + push 到裸仓库。
    async fn import_dir_async(
        repos_dir: &str,
        name: &str,
        source_dir: &str,
    ) -> Result<String, String> {
        let bare = format!("{repos_dir}/{name}.git");
        // 裸仓库不存在则创建
        if !std::path::Path::new(&bare).is_dir() {
            Self::create_repo_async(repos_dir, name, "").await?;
        }
        let branch = Self::default_branch(&bare).await;
        let script = build_import_script(repos_dir, name, source_dir, &branch);
        let (ok, out) = Self::spawn_command(&["sh".into(), "-c".into(), script]).await;
        if !ok {
            return Err(format!("导入目录失败: {out}"));
        }
        Ok(branch)
    }
}

impl Default for CodeRepoRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RouteHandler for CodeRepoRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        let mut routes = vec![
            // 仓库管理（5 条）
            spec(HttpMethod::Get, "/api/v1/coderepo/repos", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/coderepo/repos",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/coderepo/repos/:name",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/coderepo/repos/:name/contents",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/coderepo/repos/:name/file",
                false,
                vec![],
            ),
            // 仓库操作（3 条）
            spec(
                HttpMethod::Get,
                "/api/v1/coderepo/repos/:name/commits",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/coderepo/repos/:name/clone-url",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/coderepo/repos/:name/import",
                true,
                vec!["admin".into()],
            ),
            // AI 会话归档（3 条）
            spec(HttpMethod::Get, "/api/v1/coderepo/sessions", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/coderepo/sessions",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/coderepo/sessions/:id/end",
                true,
                vec!["admin".into()],
            ),
            // 统计（1 条）
            spec(HttpMethod::Get, "/api/v1/coderepo/stats", false, vec![]),
        ];
        // 项目级 Issues / Pull Requests 协作层（12 条，2026-08-24——读公开、写
        // 在 handler 内自验链上 token / admin 回落，同 nexhub-lobby 模式）：
        //   GET/POST  /repos/:name/issues[/:num[/comments|close|open]]
        //   GET/POST  /repos/:name/pulls[/:num[/comments|merge|close]]
        routes.extend(crate::issues::route_specs());
        routes
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, HandlerError> {
        // Issues / Pull Requests 命名空间（repos/:name/issues|pulls/...）先由协作
        // 服务认领（非该命名空间返回 None 继续 match——路由扩展不改既有分发）。
        if let Some(res) = self
            .issues
            .try_handle(req.method, &req.path, &req.headers, &req.body)
            .await
        {
            return res;
        }
        let segs = path_segments(&req.path);
        let query = query_params(&req.path);
        let dir = repos_dir();
        match (req.method, segs.as_slice()) {
            // ============ 仓库管理 ============

            // —— GET /api/v1/coderepo/repos —— 列仓库（扫描 *.git）
            (HttpMethod::Get, ["api", "v1", "coderepo", "repos"]) => {
                let dir_clone = dir.clone();
                let repos = tokio::task::spawn_blocking(move || scan_repos_blocking(&dir_clone))
                    .await
                    .map_err(|e| HandlerError::Internal(format!("扫描仓库任务 join 失败: {e}")))?;
                Ok(ok_json(serde_json::json!({ "repos": to_value(&repos)? })))
            }

            // —— POST /api/v1/coderepo/repos —— 创建裸仓库（admin）
            (HttpMethod::Post, ["api", "v1", "coderepo", "repos"]) => {
                let body: CreateRepoBody = serde_json::from_value(req.body)
                    .map_err(|e| HandlerError::Internal(format!("解析创建仓库请求体失败: {e}")))?;
                if let Err(msg) = validate_repo_name(&body.name) {
                    return Ok(error_response(400, &msg));
                }
                let name = body.name.trim().to_string();
                let bare = format!("{dir}/{name}.git");
                if std::path::Path::new(&bare).exists() {
                    return Ok(error_response(409, &format!("仓库已存在: {name}")));
                }
                let desc = body.description.unwrap_or_default();
                match Self::create_repo_async(&dir, &name, &desc).await {
                    Ok(()) => Ok(ApiResponse {
                        status: 201,
                        body: serde_json::json!({
                            "ok": true,
                            "name": name,
                            "description": desc,
                            "clone_url_ssh": build_clone_url(&name),
                            "clone_url_http": build_clone_url_http(&name),
                            "path": bare,
                        }),
                        headers: serde_json::json!({}),
                    }),
                    Err(e) => Ok(error_response(502, &e)),
                }
            }

            // —— DELETE /api/v1/coderepo/repos/:name —— 删仓库（admin，rm -rf）
            (HttpMethod::Delete, ["api", "v1", "coderepo", "repos", name]) => {
                if let Err(msg) = validate_repo_name(name) {
                    return Ok(error_response(400, &msg));
                }
                let bare = format!("{dir}/{name}.git");
                if !std::path::Path::new(&bare).is_dir() {
                    return Ok(error_response(404, &format!("仓库不存在: {name}")));
                }
                let bare_clone = bare.clone();
                let res = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&bare_clone))
                    .await
                    .map_err(|e| HandlerError::Internal(format!("删除任务 join 失败: {e}")))?;
                match res {
                    Ok(()) => Ok(ok_json(serde_json::json!({
                        "ok": true,
                        "name": name,
                        "action": "delete",
                    }))),
                    Err(e) => Ok(error_response(502, &format!("删除仓库失败: {e}"))),
                }
            }

            // —— GET /api/v1/coderepo/repos/:name/contents —— 文件树 + 分支
            (HttpMethod::Get, ["api", "v1", "coderepo", "repos", name, "contents"]) => {
                if let Err(msg) = validate_repo_name(name) {
                    return Ok(error_response(400, &msg));
                }
                let bare = format!("{dir}/{name}.git");
                if !std::path::Path::new(&bare).is_dir() {
                    return Ok(error_response(404, &format!("仓库不存在: {name}")));
                }
                let default_branch = Self::default_branch(&bare).await;
                let branches = Self::list_branches(&bare).await;
                let (ok, out) =
                    Self::run_git_in_repo(&bare, &["ls-tree", "-r", "-t", "HEAD"]).await;
                let tree = if ok {
                    parse_git_ls_tree(&out)
                } else {
                    Vec::new()
                };
                Ok(ok_json(serde_json::json!({
                    "name": name,
                    "default_branch": default_branch,
                    "branches": branches,
                    "tree": to_value(&tree)?,
                })))
            }

            // —— GET /api/v1/coderepo/repos/:name/file?path=... —— 文件内容
            (HttpMethod::Get, ["api", "v1", "coderepo", "repos", name, "file"]) => {
                if let Err(msg) = validate_repo_name(name) {
                    return Ok(error_response(400, &msg));
                }
                let path = query.get("path").cloned().unwrap_or_default();
                if path.trim().is_empty() {
                    return Ok(error_response(400, "query param `path` 不可为空"));
                }
                if path.split('/').any(|s| s == "..") {
                    return Ok(error_response(400, "path 不可包含 '..'"));
                }
                let bare = format!("{dir}/{name}.git");
                if !std::path::Path::new(&bare).is_dir() {
                    return Ok(error_response(404, &format!("仓库不存在: {name}")));
                }
                let (ok, out) =
                    Self::run_git_in_repo(&bare, &["show", &format!("HEAD:{path}")]).await;
                Ok(ok_json(serde_json::json!({
                    "name": name,
                    "path": path,
                    "ok": ok,
                    "exists": ok,
                    "content": if ok { out } else { String::new() },
                })))
            }

            // ============ 仓库操作 ============

            // —— GET /api/v1/coderepo/repos/:name/commits —— 提交历史
            (HttpMethod::Get, ["api", "v1", "coderepo", "repos", name, "commits"]) => {
                if let Err(msg) = validate_repo_name(name) {
                    return Ok(error_response(400, &msg));
                }
                let bare = format!("{dir}/{name}.git");
                if !std::path::Path::new(&bare).is_dir() {
                    return Ok(error_response(404, &format!("仓库不存在: {name}")));
                }
                let fmt = "%H\x1f%an\x1f%s\x1f%ai".to_string();
                let (ok, out) =
                    Self::run_git_in_repo(&bare, &["log", "-n", "20", &format!("--format={fmt}")])
                        .await;
                let commits = if ok { parse_git_log(&out) } else { Vec::new() };
                Ok(ok_json(
                    serde_json::json!({ "name": name, "commits": to_value(&commits)? }),
                ))
            }

            // —— POST /api/v1/coderepo/repos/:name/clone-url —— 获取 clone URL
            (HttpMethod::Post, ["api", "v1", "coderepo", "repos", name, "clone-url"]) => {
                if let Err(msg) = validate_repo_name(name) {
                    return Ok(error_response(400, &msg));
                }
                Ok(ok_json(serde_json::json!({
                    "name": name,
                    "clone_url_ssh": build_clone_url(name),
                    "clone_url_http": build_clone_url_http(name),
                })))
            }

            // —— POST /api/v1/coderepo/repos/:name/import —— 导入目录为仓库（admin）
            (HttpMethod::Post, ["api", "v1", "coderepo", "repos", name, "import"]) => {
                if let Err(msg) = validate_repo_name(name) {
                    return Ok(error_response(400, &msg));
                }
                let body: ImportBody = serde_json::from_value(req.body)
                    .map_err(|e| HandlerError::Internal(format!("解析导入请求体失败: {e}")))?;
                if body.source_dir.trim().is_empty() {
                    return Ok(error_response(400, "source_dir 不可为空"));
                }
                if !std::path::Path::new(&body.source_dir).is_dir() {
                    return Ok(error_response(
                        400,
                        &format!("source_dir 不存在或非目录: {}", body.source_dir),
                    ));
                }
                match Self::import_dir_async(&dir, name, body.source_dir.trim()).await {
                    Ok(branch) => Ok(ok_json(serde_json::json!({
                        "ok": true,
                        "name": name,
                        "source_dir": body.source_dir,
                        "branch": branch,
                        "clone_url_ssh": build_clone_url(name),
                        "clone_url_http": build_clone_url_http(name),
                    }))),
                    Err(e) => Ok(error_response(502, &e)),
                }
            }

            // ============ AI 会话归档 ============

            // —— GET /api/v1/coderepo/sessions —— 列 AI 会话记录
            (HttpMethod::Get, ["api", "v1", "coderepo", "sessions"]) => {
                Ok(ok_json(to_value(&self.sessions_snapshot())?))
            }

            // —— POST /api/v1/coderepo/sessions —— 创建会话记录（admin）
            (HttpMethod::Post, ["api", "v1", "coderepo", "sessions"]) => {
                let body: CreateSessionBody = serde_json::from_value(req.body)
                    .map_err(|e| HandlerError::Internal(format!("解析创建会话请求体失败: {e}")))?;
                if body.agent_name.trim().is_empty() {
                    return Ok(error_response(400, "agent_name 不可为空"));
                }
                if body.repo_name.trim().is_empty() {
                    return Ok(error_response(400, "repo_name 不可为空"));
                }
                let session = AgentSession {
                    id: self.next_id("session"),
                    agent_name: body.agent_name.trim().to_string(),
                    repo_name: body.repo_name.trim().to_string(),
                    session_summary: body.summary.unwrap_or_default(),
                    files_changed: body.files_changed.unwrap_or(0),
                    commits: body.commits.unwrap_or(0),
                    started_at: now_iso(),
                    ended_at: None,
                };
                let resp = to_value(&session)?;
                self.sessions
                    .lock()
                    .expect("sessions poisoned")
                    .push(session);
                Ok(ApiResponse {
                    status: 201,
                    body: resp,
                    headers: serde_json::json!({}),
                })
            }

            // —— POST /api/v1/coderepo/sessions/:id/end —— 结束会话（admin）
            (HttpMethod::Post, ["api", "v1", "coderepo", "sessions", id, "end"]) => {
                let mut sessions = self.sessions.lock().expect("sessions poisoned");
                let Some(s) = sessions.iter_mut().find(|s| s.id == *id) else {
                    return Ok(error_response(404, &format!("会话不存在: {id}")));
                };
                s.ended_at = Some(now_iso());
                Ok(ok_json(serde_json::json!({
                    "ok": true,
                    "id": id,
                    "ended_at": s.ended_at,
                })))
            }

            // ============ 统计 ============

            // —— GET /api/v1/coderepo/stats —— 聚合统计
            (HttpMethod::Get, ["api", "v1", "coderepo", "stats"]) => {
                let dir_clone = dir.clone();
                let repos = tokio::task::spawn_blocking(move || scan_repos_blocking(&dir_clone))
                    .await
                    .map_err(|e| {
                        HandlerError::Internal(format!("stats 扫描任务 join 失败: {e}"))
                    })?;
                let total_size: u64 = repos.iter().map(|r| r.size_bytes).sum();
                let total_commits: u32 = repos.iter().map(|r| r.commit_count).sum::<u32>()
                    + self
                        .sessions_snapshot()
                        .iter()
                        .map(|s| s.commits)
                        .sum::<u32>();
                let session_count = self.sessions_snapshot().len();
                Ok(ok_json(serde_json::json!({
                    "repo_count": repos.len(),
                    "total_size": total_size,
                    "session_count": session_count,
                    "total_commits": total_commits,
                })))
            }

            // —— 未覆盖路由 —— 兜底 404
            _ => Ok(error_response(404, "code_repo: 未匹配的路由")),
        }
    }
}

impl CodeRepoRouteHandler {
    /// 列分支（`git for-each-ref refs/heads`）。失败返回空 vec。
    async fn list_branches(bare: &str) -> Vec<String> {
        let (ok, out) = Self::run_git_in_repo(
            bare,
            &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
        )
        .await;
        if !ok {
            return Vec::new();
        }
        out.lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    }
}

// ----------------------------------------------------------------------------
// 内部辅助
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
        handler_component: "code_repo".to_string(),
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

fn to_value<T: Serialize>(v: &T) -> Result<serde_json::Value, HandlerError> {
    serde_json::to_value(v).map_err(|e| HandlerError::Internal(format!("响应序列化失败: {e}")))
}

fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

/// 解析 query string 为 HashMap。
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

/// 简易 URL 解码（仅 %XX + + → 空格）。
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

fn now_iso() -> String {
    use chrono::Local;
    Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string()
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

    // ---- build_clone_url ----

    #[test]
    fn build_clone_url_contains_ssh_scheme() {
        // 用纯函数版避免 env/缓存带来的不确定性
        let url = build_clone_url_with("my-repo", "oem", "os-test-host", "/tank/git-repos");
        assert!(
            url.starts_with("ssh://"),
            "clone URL 应以 ssh:// 开头: {url}"
        );
        assert!(url.contains("my-repo.git"), "应含仓库名: {url}");
        assert!(url.contains("os-test-host"), "应含 host: {url}");
        assert!(url.contains("oem@"), "应含 user@: {url}");
        assert_eq!(url, "ssh://oem@os-test-host:/tank/git-repos/my-repo.git");
    }

    #[test]
    fn build_clone_url_http_contains_git_path() {
        // 用纯函数版避免 env/缓存带来的不确定性
        let url = build_clone_url_http_with("my-repo", "os-test-host", "8080");
        assert!(
            url.starts_with("http://"),
            "HTTP clone URL 应以 http:// 开头: {url}"
        );
        assert!(url.contains("/git/"), "应含 /git/ 路径前缀: {url}");
        assert!(url.contains("my-repo.git"), "应含 <name>.git: {url}");
        assert!(url.ends_with(".git"), "应以 .git 结尾: {url}");
        // token 不拼进地址（仅 push 需要，用户以 http://user:TOKEN@host 形式自行注入）
        assert!(!url.contains('@'), "URL 不应内嵌凭据: {url}");
        assert_eq!(url, "http://os-test-host:8080/git/my-repo.git");
    }

    // ---- advertise_host 地址优先链（跨节点可达性）----

    #[test]
    fn resolve_advertise_host_env_override_wins() {
        // 显式覆盖最高优先：即使能探测到选路 IP 也用 env 指定值
        assert_eq!(
            resolve_advertise_host_with(Some("192.0.2.200"), Some("192.168.1.5"), "ub2604"),
            "192.0.2.200",
            "env 显式覆盖应最高优先"
        );
        // 带空白自动 trim；纯空白视为未设置（落到选路 IP）
        assert_eq!(
            resolve_advertise_host_with(Some("  192.0.2.201 "), Some("192.168.1.5"), "ub2604"),
            "192.0.2.201"
        );
        assert_eq!(
            resolve_advertise_host_with(Some("   "), Some("192.168.1.5"), "ub2604"),
            "192.168.1.5",
            "空白 env 应视为未设置"
        );
    }

    #[test]
    fn resolve_advertise_host_prefers_ip_over_hostname() {
        // 无 env：选路 IP 次优先（hostname 跨节点解析不了，仅保底）
        assert_eq!(
            resolve_advertise_host_with(None, Some("192.168.1.5"), "ub2604"),
            "192.168.1.5",
            "选路 IP 应优先于 hostname"
        );
        // 探测失败（离线/无默认路由）→ 回退 hostname（本地网络配了主机名解析仍可用）
        assert_eq!(
            resolve_advertise_host_with(None, None, "ub2604"),
            "ub2604",
            "探测失败应回退 hostname"
        );
        assert_eq!(
            resolve_advertise_host_with(None, Some(""), "ub2604"),
            "ub2604",
            "空探测值同样回退"
        );
    }

    #[test]
    fn local_non_loopback_ipv4_is_valid_lan_ip_or_none() {
        // 契约：Some → 必是合法非回环 IPv4；None（沙箱无网络）→ 调用方回退
        if let Some(ip) = local_non_loopback_ipv4() {
            let v4: std::net::Ipv4Addr = ip.parse().expect("探测结果应为合法 IPv4 字面量");
            assert!(!v4.is_loopback(), "不应广播回环地址: {ip}");
        }
    }

    #[test]
    fn advertise_host_returns_nonempty() {
        // 组装链（env 未设时）：要么选路 IP 要么 hostname，恒非空
        let host = advertise_host();
        assert!(!host.trim().is_empty(), "广播主机不可为空");
    }

    // ---- parse_git_ls_tree ----

    #[test]
    fn parse_git_ls_tree_parses_blobs_and_trees() {
        let out = "100644 blob a1b2c3d4e5f6\tsrc/main.rs\n\
                   040000 tree f9e8d7c6b5a4\tsrc\n\
                   100644 blob 112233445566\tREADME.md\n";
        let nodes = parse_git_ls_tree(out);
        assert_eq!(nodes.len(), 3, "应解析 3 个节点");
        assert_eq!(nodes[0].path, "src/main.rs");
        assert!(!nodes[0].is_dir, "src/main.rs 是文件");
        assert_eq!(nodes[0].name, "main.rs");
        assert_eq!(nodes[1].path, "src");
        assert!(nodes[1].is_dir, "src 是目录");
        assert_eq!(nodes[1].name, "src");
        assert_eq!(nodes[2].name, "README.md");
    }

    #[test]
    fn parse_git_ls_tree_empty_returns_empty() {
        assert!(parse_git_ls_tree("").is_empty());
        assert!(parse_git_ls_tree("\n\n").is_empty());
    }

    // ---- parse_git_log ----

    #[test]
    fn parse_git_log_parses_commits() {
        let out = "abc1234fullhash\x1fZCode\x1fFix bug\x1f2026-08-13 10:00:00 +0800\n\
                   def5678fullhash\x1fAlice\x1fAdd feature\x1f2026-08-12 09:00:00 +0800\n";
        let commits = parse_git_log(out);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].hash, "abc1234fullhash");
        assert_eq!(commits[0].author, "ZCode");
        assert_eq!(commits[0].message, "Fix bug");
        assert!(commits[0].date.starts_with("2026-08-13"));
        assert_eq!(commits[1].message, "Add feature");
    }

    #[test]
    fn parse_git_log_empty_returns_empty() {
        assert!(parse_git_log("").is_empty());
    }

    // ---- repos_dir ----

    /// env 竞态防护（审计 §6.6 预警项）：下方两个改全局 `NEXOS_GIT_REPOS_DIR` 的
    /// 测试（`repos_dir_default_path` 与 `stats_aggregates_counts_without_panic`）
    /// 在并行测试线程下互相应干扰——stats 扫描期间 env 被另一测试 remove 会扫到
    /// 默认 `/tank/git-repos`（真机有真实仓库）。code_repo handler 每请求读 env
    /// （无构造注入点），故用模块级 tokio Mutex 把两个 env 依赖测试串行化（覆盖
    /// 不变；tokio Mutex 而非 std Mutex：stats 需持锁跨 `.await`，且两个测试各持
    /// 独立 runtime，std 锁跨 await 是 clippy `await_holding_lock` 禁区）。
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn repos_dir_default_path() {
        let _guard = ENV_LOCK.lock().await;
        std::env::remove_var("NEXOS_GIT_REPOS_DIR");
        std::env::remove_var("OS_GIT_REPOS_DIR");
        assert_eq!(repos_dir(), "/tank/git-repos");
        std::env::set_var("NEXOS_GIT_REPOS_DIR", "/tmp/custom-repos");
        assert_eq!(repos_dir(), "/tmp/custom-repos");
        std::env::remove_var("NEXOS_GIT_REPOS_DIR");
        std::env::remove_var("OS_GIT_REPOS_DIR");
    }

    // ---- 路由数量（24 = 原生 12 + Issues/PR 协作 12）----

    #[tokio::test]
    async fn routes_declares_endpoints_all_code_repo() {
        let h = CodeRepoRouteHandler::new();
        let routes = h.routes().await;
        assert_eq!(routes.len(), 24, "应有 24 条路由: {routes:?}");
        assert!(
            routes.iter().all(|r| r.handler_component == "code_repo"),
            "全部归属 code_repo 组件"
        );
        // 原生仓库中心写操作（POST / DELETE，除协作层）仍要求 admin
        for r in &routes {
            let is_issues = r.path.contains("/issues") || r.path.contains("/pulls");
            if matches!(r.method, HttpMethod::Post | HttpMethod::Delete) && !is_issues {
                assert!(r.requires_auth, "原生写操作需 auth: {r:?}");
                assert_eq!(r.required_roles, vec!["admin".to_string()]);
            }
        }
        // GET 全部公开
        for r in &routes {
            if r.method == HttpMethod::Get {
                assert!(!r.requires_auth, "GET 应公开: {r:?}");
            }
        }
        // 协作层（issues/pulls）全部 handler 内自验（requires_auth=false——
        // 链上 token 身份经网关直达，同 nexhub-lobby 用户面模式）
        for r in &routes {
            if r.path.contains("/issues") || r.path.contains("/pulls") {
                assert!(
                    !r.requires_auth && r.required_roles.is_empty(),
                    "协作路由 handler 自验: {r:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn routes_include_commits_clone_url_import() {
        let h = CodeRepoRouteHandler::new();
        let routes = h.routes().await;
        let paths: Vec<(HttpMethod, &str)> =
            routes.iter().map(|r| (r.method, r.path.as_str())).collect();
        assert!(paths.contains(&(HttpMethod::Get, "/api/v1/coderepo/repos/:name/commits")));
        assert!(paths.contains(&(HttpMethod::Post, "/api/v1/coderepo/repos/:name/clone-url")));
        assert!(paths.contains(&(HttpMethod::Post, "/api/v1/coderepo/repos/:name/import")));
    }

    // ---- 命令构造 ----

    #[test]
    fn build_create_repo_cmd_constructs_init_bare() {
        let cmd = build_create_repo_cmd("/tank/git-repos", "demo");
        assert_eq!(cmd[0], "git");
        assert!(cmd.contains(&"init".to_string()));
        assert!(cmd.contains(&"--bare".to_string()));
        assert!(
            cmd.contains(&"/tank/git-repos/demo.git".to_string()),
            "应含裸仓库路径: {cmd:?}"
        );
    }

    #[test]
    fn build_import_script_contains_git_workflow() {
        let script = build_import_script("/tank/git-repos", "demo", "/home/oem/project", "master");
        assert!(script.contains("git init"), "应含 git init: {script}");
        assert!(script.contains("git add -A"), "应含 git add -A: {script}");
        assert!(script.contains(" commit -m "), "应含 commit -m: {script}");
        assert!(script.contains("git push"), "应含 git push: {script}");
        assert!(
            script.contains("/tank/git-repos/demo.git"),
            "应 push 到裸仓库: {script}"
        );
        assert!(
            script.contains("HEAD:master"),
            "应 push 到 master: {script}"
        );
        assert!(
            script.contains("user.name='OS'") && script.contains("user.email="),
            "应内置 user.name/email 避免依赖全局配置: {script}"
        );
    }

    // ---- 仓库创建（真实 git init，隔离到 tempdir）----

    #[tokio::test]
    async fn create_repo_runs_git_init_bare() {
        let tmp = tempdir();
        let dir = tmp.to_string();
        let res = CodeRepoRouteHandler::create_repo_async(&dir, "demo", "a demo repo").await;
        assert!(res.is_ok(), "create_repo 应成功: {:?}", res);
        let bare = format!("{dir}/demo.git");
        assert!(std::path::Path::new(&bare).is_dir(), "裸仓库应存在: {bare}");
        assert!(
            std::path::Path::new(&format!("{bare}/HEAD")).exists(),
            "裸仓库应有 HEAD"
        );
        // description 已写入
        let desc = std::fs::read_to_string(format!("{bare}/description")).unwrap();
        assert_eq!(desc, "a demo repo");
    }

    // ---- 建仓默认分支（外部 agent 接入实测坑位）：HEAD 显式指向 main ----

    #[tokio::test]
    async fn create_repo_sets_head_to_refs_heads_main() {
        let tmp = tempdir();
        let dir = tmp.to_string();
        CodeRepoRouteHandler::create_repo_async(&dir, "headcheck", "")
            .await
            .unwrap();
        let bare = format!("{dir}/headcheck.git");
        let (ok, out) =
            CodeRepoRouteHandler::run_git_in_repo(&bare, &["symbolic-ref", "HEAD"]).await;
        assert!(ok, "symbolic-ref 应成功");
        assert_eq!(
            out.trim(),
            "refs/heads/main",
            "建仓 API 产出的裸仓 HEAD 应指向 refs/heads/main（不依赖全局 init.defaultBranch）"
        );
    }

    // ---- resolve_default_branch_sync：空仓（无任何分支）返回 HEAD 名，不 panic ----

    #[tokio::test]
    async fn resolve_default_branch_sync_empty_repo_returns_head_name() {
        let tmp = tempdir();
        let dir = tmp.to_string();
        CodeRepoRouteHandler::create_repo_async(&dir, "bare-empty", "")
            .await
            .unwrap();
        // 空仓：HEAD=main 但 main/master 都不存在 → 返回 HEAD 名（调用方 log/show 失败降级）
        assert_eq!(
            resolve_default_branch_sync(&format!("{dir}/bare-empty.git")),
            "main"
        );
    }

    // ---- scan_repos：HEAD 指向不存在的分支（init 落 master、只推 main 的存量
    //      形态）经回退探测仍能取到 last_commit ----

    #[tokio::test]
    async fn scan_repos_reads_last_commit_with_main_only_push() {
        let tmp = tempdir();
        let dir = tmp.to_string();
        // 模拟存量仓：建仓 API 建出（HEAD=main）后把 HEAD 拨回 master，再只推 main
        CodeRepoRouteHandler::create_repo_async(&dir, "legacy", "")
            .await
            .unwrap();
        let bare = format!("{dir}/legacy.git");
        assert!(
            CodeRepoRouteHandler::run_git_in_repo(
                &bare,
                &["symbolic-ref", "HEAD", "refs/heads/master"]
            )
            .await
            .0
        );
        commit_and_push_main(&dir, "legacy");
        let repos = scan_repos_blocking(&dir);
        assert_eq!(repos.len(), 1, "应扫到 1 个仓库: {repos:?}");
        assert_eq!(repos[0].name, "legacy");
        assert_eq!(repos[0].branch_count, 1, "只有 main 一个分支: {repos:?}");
        assert_eq!(repos[0].commit_count, 1);
        assert!(
            repos[0].last_commit.is_some(),
            "HEAD(master) 指向的分支不存在但推了 main → 回退 main 后应取到 last_commit: {repos:?}"
        );
    }

    /// 工作区 1 提交（README.md）推到裸仓 main 分支（默认分支回退 fixture 用）。
    fn commit_and_push_main(repos_dir: &str, name: &str) {
        let bare = format!("{repos_dir}/{name}.git");
        let work = format!("{repos_dir}/.{name}-work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(format!("{work}/README.md"), "# legacy\n只推 main").unwrap();
        let ok = |args: &[&str]| {
            matches!(
                std::process::Command::new(args[0]).args(&args[1..]).output(),
                Ok(o) if o.status.success()
            )
        };
        assert!(ok(&["git", "-c", "init.defaultBranch=main", "init", &work]));
        assert!(ok(&["git", "-C", &work, "add", "-A"]));
        assert!(ok(&[
            "git",
            "-C",
            &work,
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@t",
            "commit",
            "-m",
            "only main"
        ]));
        assert!(ok(&["git", "-C", &work, "push", &bare, "HEAD:main"]));
        let _ = std::fs::remove_dir_all(&work);
    }

    // ---- 空仓库文件树返回空 ----

    #[tokio::test]
    async fn empty_repo_contents_returns_empty_tree() {
        let tmp = tempdir();
        let dir = tmp.to_string();
        CodeRepoRouteHandler::create_repo_async(&dir, "empty", "")
            .await
            .unwrap();
        let bare = format!("{dir}/empty.git");
        let (ok, out) =
            CodeRepoRouteHandler::run_git_in_repo(&bare, &["ls-tree", "-r", "-t", "HEAD"]).await;
        // 空仓库 HEAD 不存在 → 失败
        let tree = if ok {
            parse_git_ls_tree(&out)
        } else {
            Vec::new()
        };
        assert!(tree.is_empty(), "空仓库文件树应为空");
    }

    // ---- 会话 CRUD ----

    #[tokio::test]
    async fn sessions_create_list_end() {
        let h = CodeRepoRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/coderepo/sessions",
                serde_json::json!({
                    "agent_name": "zcode",
                    "repo_name": "os-core",
                    "summary": "实现了存储模块",
                    "files_changed": 12,
                    "commits": 5,
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        let id = resp.body["id"].as_str().unwrap().to_string();
        assert!(id.starts_with("session-"));
        assert_eq!(resp.body["commits"], 5);
        assert!(resp.body["ended_at"].is_null());

        let resp = h
            .handle(get_req("/api/v1/coderepo/sessions"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], id);

        let resp = h
            .handle(post_req(
                &format!("/api/v1/coderepo/sessions/{id}/end"),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let resp = h
            .handle(get_req("/api/v1/coderepo/sessions"))
            .await
            .unwrap();
        let arr = resp.body.as_array().unwrap();
        assert!(arr[0]["ended_at"].is_string());
    }

    #[tokio::test]
    async fn sessions_create_rejects_empty_fields() {
        let h = CodeRepoRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/coderepo/sessions",
                serde_json::json!({"agent_name": "", "repo_name": "x"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        let resp = h
            .handle(post_req(
                "/api/v1/coderepo/sessions",
                serde_json::json!({"agent_name": "zcode", "repo_name": ""}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    #[tokio::test]
    async fn sessions_end_missing_returns_404() {
        let h = CodeRepoRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/coderepo/sessions/nope/end",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // ---- stats 聚合 ----

    #[tokio::test]
    async fn stats_aggregates_counts_without_panic() {
        // 持锁跨全程（见 ENV_LOCK 注释）：扫描发生在 handle().await 内，锁必须
        // 覆盖整个请求生命周期，否则 env 可能被并行测试改走。
        let _guard = ENV_LOCK.lock().await;
        let tmp = tempdir();
        std::env::set_var("NEXOS_GIT_REPOS_DIR", &tmp);
        let h = CodeRepoRouteHandler::with_empty();
        h.handle(post_req(
            "/api/v1/coderepo/sessions",
            serde_json::json!({"agent_name": "zcode", "repo_name": "a", "commits": 3}),
        ))
        .await
        .unwrap();
        h.handle(post_req(
            "/api/v1/coderepo/sessions",
            serde_json::json!({"agent_name": "codex", "repo_name": "b", "commits": 7}),
        ))
        .await
        .unwrap();
        let resp = h.handle(get_req("/api/v1/coderepo/stats")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["session_count"], 2);
        // total_commits = 会话 commits 之和（无仓库提交）= 10
        assert_eq!(resp.body["total_commits"], 10);
        assert!(resp.body["repo_count"].is_u64());
        assert!(resp.body["total_size"].is_u64());
        std::env::remove_var("NEXOS_GIT_REPOS_DIR");
        std::env::remove_var("OS_GIT_REPOS_DIR");
    }

    // ---- clone-url 端点 ----

    #[tokio::test]
    async fn clone_url_endpoint_returns_ssh_url() {
        // 仅做结构断言：host 来自 OnceLock 缓存（并行测试下不可控），
        // 故只验证 scheme/仓库名/user@/:/ 等不变结构。
        let h = CodeRepoRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/coderepo/repos/myrepo/clone-url",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let url = resp.body["clone_url_ssh"].as_str().unwrap();
        assert!(url.starts_with("ssh://"), "应以 ssh:// 开头: {url}");
        assert!(url.contains("myrepo.git"), "应含仓库名: {url}");
        assert!(url.contains("@"), "应含 user@: {url}");
        // host:path 分隔（scp 风格），且路径以仓库名结尾
        assert!(url.contains(":/"), "应含 host:path 分隔: {url}");
        assert!(url.ends_with("/myrepo.git"), "应以仓库名结尾: {url}");
        // HTTP clone URL 同步返回（Smart Git /git/<name>.git，不含 token）
        let http = resp.body["clone_url_http"]
            .as_str()
            .expect("clone_url_http 应为字符串");
        assert!(http.starts_with("http://"), "应以 http:// 开头: {http}");
        assert!(http.contains("/git/"), "应含 /git/ 路径前缀: {http}");
        assert!(http.ends_with("/myrepo.git"), "应以仓库名结尾: {http}");
        assert!(!http.contains('@'), "不应内嵌凭据: {http}");
    }

    // ---- 名称校验 ----

    #[test]
    fn validate_repo_name_rejects_bad_input() {
        assert!(validate_repo_name("").is_err());
        assert!(validate_repo_name("../x").is_err());
        assert!(validate_repo_name("a/b").is_err());
        assert!(validate_repo_name("-evil").is_err());
        assert!(validate_repo_name("good-name").is_ok());
        assert!(validate_repo_name("good_name").is_ok());
    }

    // ---- 兜底 404 ----

    #[tokio::test]
    async fn unmatched_route_returns_404() {
        let h = CodeRepoRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/coderepo/nope")).await.unwrap();
        assert_eq!(resp.status, 404);
    }

    // ---- import 缺 source_dir → 400 ----

    #[tokio::test]
    async fn import_requires_source_dir() {
        let h = CodeRepoRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/coderepo/repos/demo/import",
                serde_json::json!({"source_dir": ""}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<CodeRepoRouteHandler>();
    }

    // ---- 测试辅助：唯一临时目录 ----

    fn tempdir() -> String {
        let p = std::env::temp_dir().join(format!(
            "os-coderepo-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p.to_string_lossy().into_owned()
    }
}
