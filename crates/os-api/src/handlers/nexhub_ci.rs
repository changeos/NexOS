//! `NexhubCiRouteHandler` —— NexHub 内置 CI（v0.1.34，组件 `nexhub_ci`）。
//!
//! 定位：NexHub 裸仓库的**零外部依赖内置 CI**——clone 本机裸仓库 → 应用目录
//! 探测流水线（Cargo.toml / package.json）→ tokio spawn 执行，环形日志（500 行）
//! 攒批实时入库。给仓库详情页「CI」Tab 与仓库卡徽章供数（各仓最新 run 摘要
//! 聚合端点一次拉全）。
//!
//! # 流水线探测（clone 后应用目录，两标志独立判定后串联）
//!
//! | Cargo.toml | package.json | 步骤 |
//! |------------|--------------|------|
//! | ✓ | ✗ | `cargo check --workspace --all-targets` |
//! | ✗ | ✓ | [骨架注入] → `npm ci` → `npm run build` |
//! | ✓ | ✓ | [骨架注入] + 两段串联（先 cargo 后 npm） |
//! | ✗ | ✗ | 无 —— run 记 `skipped`（诚实状态，不假装通过） |
//!
//! # monorepo 骨架注入（v0.1.34，package.json 流水线专用）
//!
//! 应用仓（nexos-app-*）的 tsconfig paths / vite 别名以 `../../` 相对路径引用
//! 主仓 SDK（`crates/os-api/web/src/sdk/index.ts`）——裸 clone 单仓必然断链。
//! 设计前提（用户定调）：应用仓只是软件独立迭代，**环境不独立**，SDK/构建环境
//! 由 NexOS 提供，应用仓不为裸 clone 自包含。因此 package.json 流水线在
//! vue-tsc/vite 前**前置骨架步骤**：
//!
//! 1. clone 目标仓到 `<work>/apps/<repo>/`（不再是 work 根——`../../` 锚点）；
//! 2. 把主仓 SDK 整目录拷到 `<work>/crates/os-api/web/src/sdk/`（源为主仓
//!    `<NEXOS_CI_MONOREPO>/crates/os-api/web/src/sdk`，env 缺省 `/home/oem/NexOS`）；
//! 3. 主仓或 SDK 不存在 → run 如实记 `failed`，日志附指引
//!    「本机无 monorepo，应用构建需 NexOS 环境」（诚实不静默跳过）。
//!
//! Cargo.toml 流水线不受影响（主仓自己的 CI 仍裸跑；应用仓无 Cargo.toml）；
//! npm 依赖由 package-lock.json 自带给 node_modules，不归骨架管。
//!
//! 每步超时 1800s（超时 kill，记 failed + exit_code 124）；clone 超时 300s
//! （失败记 failed——空仓库也如实 failed，不静默）。
//!
//! # 状态机
//!
//! ```text
//! queued ──▶ running ──▶ passed（全部步骤 exit 0）
//!                     └─▶ failed（任一步非零 / spawn 错 / 超时 / clone 失败）
//! queued ──▶ skipped（工作树根探测不到任何流水线，不执行步骤）
//! ```
//!
//! # 并发控制
//!
//! - **同仓串行**：每仓一条 FIFO 队列 + 至多一个 worker 任务顺序消费（后到排队；
//!   「查空 + 摘牌」与「入队 + 起工」各自单临界区，杜绝丢唤起）；
//! - **全局并发 ≤ 2**：`tokio::sync::Semaphore` 许可跨仓共享。
//!
//! # push 自动触发
//!
//! git-http push 成功路径（`http.rs` `git_http_handler`，CGI 200）经 [`push_hook`]
//! 入队 trigger=push；env `NEXOS_CI_AUTO_PUSH`（缺省开，`0`/`false` 关）。CI 对
//! 裸仓默认分支（HEAD）跑——push 非默认分支同样跑 HEAD（v0.1.33 口径：内置 CI
//! 面向「仓库健康」而非逐分支矩阵）。
//!
//! # 持久化（SQLite `ci.db`，env `NEXOS_CI_DB`）
//!
//! `ci_runs(id, repo_name, trigger, status, pipeline, log, exit_code, created_ms,
//! started_ms, finished_ms)`——时间一律 epoch 毫秒（排序精确），对外以 RFC3339
//! 字符串输出。
//!
//! # 路由表（5 条，component="nexhub_ci"；读公开 / 写 admin）
//!
//! | method | path | 动作 |
//! |--------|------|------|
//! | POST   | `/api/v1/coderepo/repos/:name/ci` | 手动触发（admin）→ 202 入队 |
//! | GET    | `/api/v1/coderepo/repos/:name/ci` | 该仓 runs（最新 20，不含 log）|
//! | GET    | `/api/v1/coderepo/repos/:name/ci/:run_id` | run 详情 + 环形日志全文 |
//! | DELETE | `/api/v1/coderepo/repos/:name/ci/:run_id` | 删记录（admin；queued/running 409）|
//! | GET    | `/api/v1/coderepo/ci/latest` | 聚合：各仓最新 run 摘要（徽章数据源）|

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rusqlite::{params, Connection};
use serde::Serialize;

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// 常量与 env
// ----------------------------------------------------------------------------

/// DB 文件路径覆盖 env。
pub const ENV_DB: &str = "NEXOS_CI_DB";

/// push 自动触发开关 env（缺省开；`0`/`false` 关）。
pub const ENV_AUTO_PUSH: &str = "NEXOS_CI_AUTO_PUSH";

/// CI 工作目录根覆盖 env（缺省 `/tmp/nexhub-ci`；生产/部署调参用）。
pub const ENV_WORK_ROOT: &str = "NEXOS_CI_WORK_ROOT";

/// 主仓（monorepo）位置覆盖 env（骨架注入的 SDK 源；缺省 `/home/oem/NexOS`）。
pub const ENV_MONOREPO: &str = "NEXOS_CI_MONOREPO";

/// 主仓 SDK 在主仓内的固定子路径（应用仓 `../../` 相对引用的落点）。
pub const MONOREPO_SDK_REL: &str = "crates/os-api/web/src/sdk";

/// 主仓缺省位置（开发机本体；部署他机须设 [`ENV_MONOREPO`]）。
const DEFAULT_MONOREPO: &str = "/home/oem/NexOS";

/// 流水线程序覆盖 env 前缀（`NEXOS_CI_BIN_CARGO` / `NEXOS_CI_BIN_NPM` → 绝对
/// 路径；service PATH 缺 cargo/npm 时的部署逃生口，与 blockchain 节点二进制
/// 覆盖同款机制）。
pub const ENV_BIN_PREFIX: &str = "NEXOS_CI_BIN_";

/// 单步骤超时（秒）——任务书口径 1800s。
pub const STEP_TIMEOUT_SECS: u64 = 1800;

/// clone 超时（秒）——file:// 本机克隆亚秒级，留足网络直连余量（apps 同款）。
const CLONE_TIMEOUT_SECS: u64 = 300;

/// 日志环形缓冲行数（超行丢最旧）。
pub const LOG_RING_LINES: usize = 500;

/// 全局最大并发 run 数（跨仓共享）。
pub const MAX_CONCURRENT_RUNS: usize = 2;

/// runs 列表返回条数（最新 20）。
pub const LIST_LIMIT: i64 = 20;

/// 日志攒批落库阈值（行）——每行写 DB 太密，攒 20 行或状态切换时整环刷盘。
const LOG_FLUSH_EVERY: usize = 20;

/// 超时终止步骤的约定退出码（GNU timeout 口径）。
pub const EXIT_TIMEOUT: i32 = 124;

/// env 非空取值（llm.rs / tips.rs 同款）。
fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// ci.db 默认路径链：env → `/tank/os-data/ci.db` → `/var/lib/os/ci.db` →
/// `./ci.db`（lobby_db_default 同款三级链）。
#[must_use]
fn default_db_path() -> String {
    if let Some(p) = env_non_empty(ENV_DB) {
        return p;
    }
    for p in ["/tank/os-data/ci.db", "/var/lib/os/ci.db"] {
        if Path::new(p)
            .parent()
            .is_some_and(|d| d.exists() || std::fs::create_dir_all(d).is_ok())
        {
            return (*p).to_string();
        }
    }
    "./ci.db".to_string()
}

/// CI 工作目录根：env → `/tmp/nexhub-ci`。
#[must_use]
fn default_work_root() -> PathBuf {
    env_non_empty(ENV_WORK_ROOT)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/nexhub-ci"))
}

/// 骨架注入的主仓（monorepo）根：env [`ENV_MONOREPO`] → `/home/oem/NexOS`。
#[must_use]
fn monorepo_root() -> PathBuf {
    env_non_empty(ENV_MONOREPO)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_MONOREPO))
}

/// 仓库名规约（与 apps_handler::valid_repo_name 同款：本机裸仓库名，防穿越）。
#[must_use]
pub fn valid_ci_repo_name(repo: &str) -> bool {
    let r = repo.trim();
    let r = r.strip_suffix(".git").unwrap_or(r);
    !r.is_empty()
        && r.len() <= 100
        && r.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && r.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// 当前时间（epoch 毫秒）。
fn now_ms() -> i64 {
    chrono::Local::now().timestamp_millis()
}

/// epoch 毫秒 → RFC3339（本地时区；None 透传——未开始/未完成字段）。
#[must_use]
fn ms_to_iso(ms: Option<i64>) -> Option<String> {
    use chrono::TimeZone;
    ms.and_then(|m| chrono::Local.timestamp_millis_opt(m).single())
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// CI run（对外 JSON）。`log` 仅详情端点填充（列表为空串 → None 不出字段）。
#[derive(Debug, Clone, Serialize)]
pub struct CiRun {
    pub id: String,
    pub repo_name: String,
    /// push | manual。
    pub trigger: String,
    /// queued | running | passed | failed | skipped。
    pub status: String,
    /// 流水线命令描述（如 `cargo check --workspace --all-targets`；skipped 为 NULL）。
    pub pipeline: Option<String>,
    pub exit_code: Option<i32>,
    /// 创建时间（RFC3339）。
    pub created_at: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    /// 运行耗时毫秒（finished-started；未完成为 None）。
    pub duration_ms: Option<i64>,
    /// 环形日志全文（详情端点才填充）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
}

/// DB 行内态（epoch 毫秒原文，查询映射中间层）。
#[derive(Debug, Clone)]
struct CiRunRow {
    id: String,
    repo_name: String,
    trigger: String,
    status: String,
    pipeline: Option<String>,
    log: String,
    exit_code: Option<i32>,
    created_ms: i64,
    started_ms: Option<i64>,
    finished_ms: Option<i64>,
}

/// 行 → DTO（时间转 RFC3339 + 耗时计算；空 log 归一为 None）。
fn row_to_run(row: CiRunRow) -> Option<CiRun> {
    Some(CiRun {
        duration_ms: match (row.started_ms, row.finished_ms) {
            (Some(s), Some(f)) => Some(f.saturating_sub(s)),
            _ => None,
        },
        id: row.id,
        repo_name: row.repo_name,
        trigger: row.trigger,
        status: row.status,
        pipeline: row.pipeline.filter(|p| !p.is_empty()),
        exit_code: row.exit_code,
        created_at: ms_to_iso(Some(row.created_ms)),
        started_at: ms_to_iso(row.started_ms),
        finished_at: ms_to_iso(row.finished_ms),
        log: (!row.log.is_empty()).then_some(row.log),
    })
}

/// 查询行列映射（列序见各 SELECT；列表查询 log 列取 NULL）。
fn map_run_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<CiRunRow> {
    Ok(CiRunRow {
        id: r.get(0)?,
        repo_name: r.get(1)?,
        trigger: r.get(2)?,
        status: r.get(3)?,
        pipeline: r.get(4)?,
        exit_code: r.get(5)?,
        created_ms: r.get(6)?,
        started_ms: r.get(7)?,
        finished_ms: r.get(8)?,
        log: r.get::<_, Option<String>>(9)?.unwrap_or_default(),
    })
}

/// 打开 ci.db（WAL + 幂等建表；open_ledger 同款）。
fn open_ci_db(path: &str) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS ci_runs (
            id          TEXT PRIMARY KEY,
            repo_name   TEXT NOT NULL,
            trigger     TEXT NOT NULL,
            status      TEXT NOT NULL,
            pipeline    TEXT,
            log         TEXT NOT NULL DEFAULT '',
            exit_code   INTEGER,
            created_ms  INTEGER NOT NULL,
            started_ms  INTEGER,
            finished_ms INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_ci_runs_repo ON ci_runs(repo_name, created_ms);
        CREATE INDEX IF NOT EXISTS idx_ci_runs_created ON ci_runs(created_ms);
        ",
    )?;
    Ok(conn)
}

// ----------------------------------------------------------------------------
// 日志环形缓冲（500 行，超行丢最旧）
// ----------------------------------------------------------------------------

/// 环形日志：`push` 超 [`LOG_RING_LINES`] 丢最旧行；`render` 按行拼接。
#[derive(Debug, Clone)]
pub struct LogRing {
    buf: VecDeque<String>,
    cap: usize,
    /// 攒批计数（自上次刷盘后新增行数；调度器刷盘策略用）。
    pub dirty: usize,
}

impl LogRing {
    /// 定容环形缓冲。
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(cap.min(1024)),
            cap,
            dirty: 0,
        }
    }

    /// 追加一行（超容丢最旧）。
    pub fn push(&mut self, line: &str) {
        if self.buf.len() == self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back(line.to_string());
        self.dirty += 1;
    }

    /// 全文（按行拼接；空环为空串）。
    #[must_use]
    pub fn render(&self) -> String {
        self.buf
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 当前行数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

// ----------------------------------------------------------------------------
// 流水线探测（纯函数，clone 后应用目录 <work>/apps/<repo>；三态易单测）
// ----------------------------------------------------------------------------

/// 单步骤 argv。
type Step = Vec<String>;

/// 探测应用目录的流水线步骤：
/// - `Cargo.toml` → `cargo check --workspace --all-targets`
/// - `package.json` → `npm ci` + `npm run build`
/// - 两者皆有 → 两段串联；皆无 → `None`（run 记 skipped）。
#[must_use]
pub fn detect_pipeline(worktree: &Path) -> Option<Vec<Step>> {
    let mut steps: Vec<Step> = Vec::new();
    if worktree.join("Cargo.toml").is_file() {
        steps.push(vec![
            "cargo".into(),
            "check".into(),
            "--workspace".into(),
            "--all-targets".into(),
        ]);
    }
    if worktree.join("package.json").is_file() {
        steps.push(vec!["npm".into(), "ci".into()]);
        steps.push(vec!["npm".into(), "run".into(), "build".into()]);
    }
    (!steps.is_empty()).then_some(steps)
}

/// 步骤序列的人读描述（`&&` 串联；写 pipeline 字段与日志步骤头）。
#[must_use]
pub fn describe_pipeline(steps: &[Step]) -> String {
    steps
        .iter()
        .map(|s| s.join(" "))
        .collect::<Vec<_>>()
        .join(" && ")
}

// ----------------------------------------------------------------------------
// monorepo 骨架注入（v0.1.34，package.json 流水线前置步骤）
// ----------------------------------------------------------------------------

/// 应用仓 clone 落点（`<work>/apps/<repo>`——仓内 `../../` 相对引用的锚点，
/// 亦是流水线探测与步骤执行的工作目录）。
#[must_use]
pub fn app_clone_dir(work_dir: &Path, repo: &str) -> PathBuf {
    work_dir.join("apps").join(repo)
}

/// 骨架注入布局（应用仓 CI 的 monorepo 镜像路径拼装，纯函数易单测）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkeletonLayout {
    /// 应用仓 clone 落点 `<work>/apps/<repo>`。
    pub app_dir: PathBuf,
    /// 主仓 SDK 源 `<monorepo>/crates/os-api/web/src/sdk`。
    pub sdk_src: PathBuf,
    /// SDK 注入落点 `<work>/crates/os-api/web/src/sdk`（与主仓内布局一致）。
    pub sdk_dest: PathBuf,
}

/// 拼装骨架布局：clone 在 `<work>/apps/<repo>`、SDK 镜像在
/// `<work>/crates/os-api/web/src/sdk`——应用仓内 `../../crates/.../sdk/index.ts`
/// 相对引用由此在 CI 工作目录内可达（tsconfig paths 与 vite 别名同源同解）。
#[must_use]
pub fn skeleton_layout(work_dir: &Path, repo: &str, monorepo: &Path) -> SkeletonLayout {
    SkeletonLayout {
        app_dir: app_clone_dir(work_dir, repo),
        sdk_src: monorepo.join(MONOREPO_SDK_REL),
        sdk_dest: work_dir.join(MONOREPO_SDK_REL),
    }
}

/// 骨架缺失指引文案（failed 日志与状态共用一份话术，诚实不静默）。
#[must_use]
fn skeleton_missing_msg(what: &str, path: &Path) -> String {
    format!(
        "{what}: {} —— 本机无 monorepo，应用构建需 NexOS 环境（env {} 可指定主仓位置）",
        path.display(),
        ENV_MONOREPO
    )
}

/// 递归拷目录（`cp -r` 语义；普通文件保留权限位，符号链接等跳过并如实只计文件）。
fn copy_dir_recursive(src: &Path, dst: &Path, files: &mut usize) -> Result<(), String> {
    let entries =
        std::fs::read_dir(src).map_err(|e| format!("读目录 {} 失败: {e}", src.display()))?;
    std::fs::create_dir_all(dst).map_err(|e| format!("建目录 {} 失败: {e}", dst.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("遍历 {} 失败: {e}", src.display()))?;
        let ft = entry.file_type().map_err(|e| e.to_string())?;
        let to = dst.join(entry.file_name());
        if ft.is_dir() {
            copy_dir_recursive(&entry.path(), &to, files)?;
        } else if ft.is_file() {
            std::fs::copy(entry.path(), &to).map_err(|e| {
                format!("拷 {} → {} 失败: {e}", entry.path().display(), to.display())
            })?;
            *files += 1;
        }
    }
    Ok(())
}

/// 执行骨架注入（package.json 流水线前置步骤，vue-tsc/vite 之前）：
/// 校验主仓与 SDK 存在 → 整目录拷到工作目录镜像位。
/// 成功与失败都写环形日志（`[骨架]` 前缀）；`Err(msg)` 供调用方记 failed。
fn inject_monorepo_skeleton(
    work_dir: &Path,
    repo: &str,
    ring: &mut LogRing,
) -> Result<(), String> {
    let monorepo = monorepo_root();
    let layout = skeleton_layout(work_dir, repo, &monorepo);
    if !monorepo.is_dir() {
        let msg = skeleton_missing_msg("主仓 monorepo 不存在", &monorepo);
        ring.push(&format!("[骨架] {msg}"));
        return Err(msg);
    }
    if !layout.sdk_src.is_dir() {
        let msg = skeleton_missing_msg("主仓 SDK 不存在", &layout.sdk_src);
        ring.push(&format!("[骨架] {msg}"));
        return Err(msg);
    }
    let mut files = 0usize;
    if let Err(e) = copy_dir_recursive(&layout.sdk_src, &layout.sdk_dest, &mut files) {
        ring.push(&format!("[骨架] SDK 拷贝失败: {e}"));
        return Err(format!("SDK 拷贝失败: {e}"));
    }
    ring.push(&format!(
        "[骨架] 注入 SDK: {} → {}（{files} 文件；monorepo={}）",
        layout.sdk_src.display(),
        layout.sdk_dest.display(),
        monorepo.display()
    ));
    Ok(())
}

// ----------------------------------------------------------------------------
// 程序解析（env 覆盖 → 注入覆盖 → PATH → 常规目录；blockchain_nodes 先例）
// ----------------------------------------------------------------------------

/// 存在且是普通文件且可执行位（任一 x 位）。
fn is_exec_file(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// 步骤程序解析顺序：
/// 1. 程序名含 `/`（绝对/相对路径）→ 直通（存在性检查）；
/// 2. env `NEXOS_CI_BIN_<大写名>`（部署逃生口）；
/// 3. core 注入覆盖（测试 stub）；
/// 4. `PATH` 扫描；
/// 5. 常规路径兜底（~/.cargo/bin、/usr/local/bin、/usr/bin、/snap/bin、
///    ~/.local/bin——systemd service PATH 常缺 ~/.cargo/bin）。
///
/// 全未命中返回 None（步骤记 failed + env 指引）。
fn resolve_program(core: &CiCore, name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        return is_exec_file(&p).then_some(p);
    }
    if let Ok(v) = std::env::var(format!("{ENV_BIN_PREFIX}{}", name.to_uppercase())) {
        let p = PathBuf::from(v.trim());
        if is_exec_file(&p) {
            return Some(p);
        }
    }
    if let Some(p) = core.bin_override(name) {
        return Some(p);
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in path_var.split(':').filter(|d| !d.is_empty()) {
            let p = Path::new(dir).join(name);
            if is_exec_file(&p) {
                return Some(p);
            }
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let dirs = [
        format!("{home}/.cargo/bin"),
        "/usr/local/bin".to_string(),
        "/usr/bin".to_string(),
        "/snap/bin".to_string(),
        format!("{home}/.local/bin"),
    ];
    for dir in dirs {
        let p = Path::new(&dir).join(name);
        if is_exec_file(&p) {
            return Some(p);
        }
    }
    None
}

// ----------------------------------------------------------------------------
// 核心：CiCore（DB + 每仓队列 + 全局信号量）
// ----------------------------------------------------------------------------

/// 排队中的 run（worker 消费单位）。
#[derive(Debug, Clone)]
struct PendingRun {
    run_id: String,
    repo: String,
}

/// 调度内态（短锁快放，不跨 .await 持锁）。
#[derive(Debug, Default)]
struct CoreInner {
    /// 每仓 FIFO（同仓串行的保证）。
    queues: HashMap<String, VecDeque<PendingRun>>,
    /// 有 worker 存活（或排队中）的仓库。
    active: HashSet<String>,
}

/// CI 核心共享态：SQLite + 每仓队列 + 全局信号量。
pub struct CiCore {
    db: Mutex<Connection>,
    /// NexHub 裸仓库根（clone 源）。
    repos_dir: String,
    /// CI 工作目录根（每 run 一个子目录，用后清）。
    work_root: PathBuf,
    /// 全局并发许可（≤ [`MAX_CONCURRENT_RUNS`]）。
    sem: Arc<tokio::sync::Semaphore>,
    inner: Mutex<CoreInner>,
    /// run id 单调计数（同毫秒防撞）。
    seq: AtomicU64,
    /// 步骤超时（生产 1800s；测试注短值验超时路径）。
    step_timeout: Duration,
    /// 程序覆盖（测试 stub 注入；键为步骤程序名如 "cargo"）。
    bin_overrides: Mutex<HashMap<String, String>>,
}

impl CiCore {
    /// 生产构造（默认 DB / 仓库根 / 工作根 / 1800s 步骤超时）。
    ///
    /// # Panics
    /// ci.db 打不开时 panic（持久层缺失进程不可用——组件装配同口径）。
    pub fn open_default() -> Arc<Self> {
        Arc::new(Self::with_paths(
            &default_db_path(),
            &os_nexhub::repos_dir(),
            &default_work_root(),
        ))
    }

    /// 注入路径构造（测试隔离用）。
    #[must_use]
    pub fn with_paths(db_path: &str, repos_dir: &str, work_root: &Path) -> Self {
        let conn = open_ci_db(db_path)
            .unwrap_or_else(|e| panic!("打开 ci.db（{db_path}）失败: {e}"));
        let _ = std::fs::create_dir_all(work_root);
        Self {
            db: Mutex::new(conn),
            repos_dir: repos_dir.to_string(),
            work_root: work_root.to_path_buf(),
            sem: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_RUNS)),
            inner: Mutex::new(CoreInner::default()),
            seq: AtomicU64::new(0),
            step_timeout: Duration::from_secs(STEP_TIMEOUT_SECS),
            bin_overrides: Mutex::new(HashMap::new()),
        }
    }

    /// 注短步骤超时（测试超时路径；builder 风格）。
    #[must_use]
    pub fn with_step_timeout(mut self, d: Duration) -> Self {
        self.step_timeout = d;
        self
    }

    /// 注入程序覆盖（测试 stub；键为步骤程序名）。
    pub fn set_bin_override(&self, program: &str, path: &str) {
        self.bin_overrides
            .lock()
            .expect("bin overrides lock")
            .insert(program.to_string(), path.to_string());
    }

    fn bin_override(&self, program: &str) -> Option<PathBuf> {
        self.bin_overrides
            .lock()
            .expect("bin overrides lock")
            .get(program)
            .map(PathBuf::from)
            .filter(|p| is_exec_file(p))
    }

    fn next_run_id(&self) -> String {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        // seq 补零到 6 位：保证同毫秒内 id 的字典序 == 数值序（ORDER BY id DESC
        // 的「同毫秒取最新」依赖这一点；25 万条/毫秒封顶，足够 CI 场景）。
        format!("r{}-{:06}-{}", now_ms(), n, std::process::id())
    }

    // ---- DB 原子操作（短锁）----

    fn db_insert(&self, run: &CiRunRow) -> rusqlite::Result<()> {
        let conn = self.db.lock().expect("ci db lock");
        conn.execute(
            "INSERT INTO ci_runs (id, repo_name, trigger, status, pipeline, log, exit_code, \
             created_ms, started_ms, finished_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                run.id,
                run.repo_name,
                run.trigger,
                run.status,
                run.pipeline,
                run.log,
                run.exit_code,
                run.created_ms,
                run.started_ms,
                run.finished_ms,
            ],
        )?;
        Ok(())
    }

    /// 状态推进（pipeline/exit_code 只进不退；started_ms 首次进入 running 时落）。
    fn db_update_status(
        &self,
        id: &str,
        status: &str,
        pipeline: Option<&str>,
        exit_code: Option<i32>,
        finished: bool,
    ) {
        let conn = self.db.lock().expect("ci db lock");
        let _ = conn.execute(
            "UPDATE ci_runs SET status=?2, pipeline=COALESCE(?3, pipeline), \
             exit_code=COALESCE(?4, exit_code), started_ms=COALESCE(started_ms, ?5), \
             finished_ms=?6 WHERE id=?1",
            params![id, status, pipeline, exit_code, now_ms(), finished.then(now_ms)],
        );
    }

    /// 环形日志整环刷盘（攒批满与状态切换时调用）。
    fn db_flush_log(&self, id: &str, ring: &mut LogRing) {
        ring.dirty = 0;
        let text = ring.render();
        let conn = self.db.lock().expect("ci db lock");
        let _ = conn.execute("UPDATE ci_runs SET log=?2 WHERE id=?1", params![id, text]);
    }

    /// 单 run 全字段（含 log；详情端点 / 内部断言用）。
    fn db_get(&self, id: &str) -> Option<CiRun> {
        let conn = self.db.lock().expect("ci db lock");
        let row = conn
            .query_row(
                "SELECT id, repo_name, trigger, status, pipeline, exit_code, created_ms, \
                 started_ms, finished_ms, log FROM ci_runs WHERE id=?1",
                params![id],
                map_run_row,
            )
            .ok()?;
        row_to_run(row)
    }

    fn db_delete(&self, id: &str) -> usize {
        let conn = self.db.lock().expect("ci db lock");
        conn.execute("DELETE FROM ci_runs WHERE id=?1", params![id])
            .unwrap_or(0)
    }

    // ---- 查询 ----

    /// 某仓 runs（最新 [`LIST_LIMIT`] 条，不含 log）。
    #[must_use]
    pub fn list_runs(&self, repo: &str) -> Vec<CiRun> {
        let conn = self.db.lock().expect("ci db lock");
        let mut stmt = match conn.prepare(
            "SELECT id, repo_name, trigger, status, pipeline, exit_code, created_ms, \
             started_ms, finished_ms, NULL FROM ci_runs WHERE repo_name=?1 \
             ORDER BY created_ms DESC, id DESC LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        stmt.query_map(params![repo, LIST_LIMIT], map_run_row)
            .map(|it| {
                it.filter_map(Result::ok)
                    .filter_map(row_to_run)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 聚合：各仓最新 run 摘要（仓库卡徽章一次拉全）。
    #[must_use]
    pub fn latest_per_repo(&self) -> Vec<CiRun> {
        let conn = self.db.lock().expect("ci db lock");
        let mut stmt = match conn.prepare(
            "SELECT id, repo_name, trigger, status, pipeline, exit_code, created_ms, \
             started_ms, finished_ms, NULL FROM ci_runs r WHERE created_ms = \
             (SELECT MAX(c.created_ms) FROM ci_runs c WHERE c.repo_name = r.repo_name) \
             ORDER BY created_ms DESC, id DESC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt
            .query_map([], map_run_row)
            .map(|it| it.filter_map(Result::ok).collect::<Vec<_>>())
            .unwrap_or_default();
        // 同毫秒并列（批量灌数据）按 repo 去重——ORDER BY id DESC 下扫描序首个
        // 即该仓同毫秒最大 id。
        let mut seen: HashSet<String> = HashSet::new();
        let mut out = Vec::new();
        for row in rows {
            if seen.insert(row.repo_name.clone()) {
                if let Some(run) = row_to_run(row) {
                    out.push(run);
                }
            }
        }
        out
    }

    // ---- 入队与 worker ----

    /// 入队一条 run（手动 / push 同一道口）。仓库必须存在（本机裸仓库）。
    /// 成功返回 run id；`Err((status, msg))` 直接映射 HTTP 错误。
    pub fn enqueue(self: &Arc<Self>, repo: &str, trigger: &str) -> Result<String, (u16, String)> {
        let repo = repo.trim().trim_end_matches(".git").to_string();
        if !valid_ci_repo_name(&repo) {
            return Err((400, format!("仓库名非法: {repo}")));
        }
        let bare = Path::new(&self.repos_dir).join(format!("{repo}.git"));
        if !bare.is_dir() {
            return Err((404, format!("仓库不存在: {}", bare.display())));
        }
        let run_id = self.next_run_id();
        let row = CiRunRow {
            id: run_id.clone(),
            repo_name: repo.clone(),
            trigger: trigger.to_string(),
            status: "queued".to_string(),
            pipeline: None,
            log: String::new(),
            exit_code: None,
            created_ms: now_ms(),
            started_ms: None,
            finished_ms: None,
        };
        self.db_insert(&row)
            .map_err(|e| (500u16, format!("写 ci_runs 失败: {e}")))?;
        eprintln!("[ci] run {run_id} 入队（{repo}, trigger={trigger}）");

        // 同仓串行核心：「入队 + 必要时起工」单临界区——与 worker 的
        // 「查空 + 摘牌」临界区互斥，杜绝 worker 退出与入队交错导致的丢唤起。
        let mut inner = self.inner.lock().expect("ci inner lock");
        let need_worker = !inner.active.contains(&repo);
        if need_worker {
            inner.active.insert(repo.clone());
        }
        inner
            .queues
            .entry(repo.clone())
            .or_default()
            .push_back(PendingRun {
                run_id: run_id.clone(),
                repo: repo.clone(),
            });        drop(inner);
        if need_worker {
            tokio::spawn(worker(Arc::clone(self), repo));
        }
        Ok(run_id)
    }
}

/// 每仓 worker：只消化本仓 FIFO（同仓串行的保证）；每条 run 执行前取全局
/// 信号量许可（跨仓并发 ≤ [`MAX_CONCURRENT_RUNS`]）。队列空则摘牌退出。
async fn worker(core: Arc<CiCore>, repo: String) {
    loop {
        // 1) 取队头；空则摘牌退出（「查空 + 摘牌」单临界区，见 enqueue 注释）
        let next = {
            let mut inner = core.inner.lock().expect("ci inner lock");
            let head = inner
                .queues
                .get_mut(&repo)
                .and_then(|q| q.pop_front());
            if head.is_none() {
                inner.active.remove(&repo);
                return;
            }
            head
        };
        let Some(run) = next else { unreachable!("空队列已在上方 return") };
        // 2) 全局并发许可（≤2；同仓在本 worker 内天然串行）
        let permit = core.sem.acquire().await.expect("semaphore closed?");
        run_one(&core, &run).await;
        drop(permit);
    }
}

/// 执行单条 run：queued → running →（passed | failed | skipped）。
/// 工作目录 `<work_root>/<run_id>`，用后清（best-effort）。
async fn run_one(core: &CiCore, run: &PendingRun) {
    let run_id = run.run_id.as_str();
    let work_dir = core.work_root.join(run_id);
    let mut ring = LogRing::new(LOG_RING_LINES);

    core.db_update_status(run_id, "running", None, None, false);
    eprintln!("[ci] run {run_id} 开始（{}）", run.repo);

    let (status, pipeline, exit_code) = ci_run_inner(core, run, &work_dir, &mut ring).await;
    core.db_update_status(run_id, &status, pipeline.as_deref(), exit_code, true);
    core.db_flush_log(run_id, &mut ring);
    let _ = std::fs::remove_dir_all(&work_dir);
    eprintln!("[ci] run {run_id} 结束：{status}（exit={exit_code:?}）");
}

/// clone + 骨架注入 + 探测 + 串联执行（`run_one` 主体；返回 (终态, pipeline,
/// exit_code)）。clone 落点 `<work>/apps/<repo>`（应用仓 `../../` 相对引用的
/// 锚点）；package.json 流水线在 vue-tsc/vite 前注入 monorepo 骨架。
async fn ci_run_inner(
    core: &CiCore,
    run: &PendingRun,
    work_dir: &Path,
    ring: &mut LogRing,
) -> (String, Option<String>, Option<i32>) {
    let run_id = run.run_id.as_str();

    // 1) clone 本机裸仓库（默认分支 HEAD；--depth 1）到 <work>/apps/<repo>/
    let app_dir = app_clone_dir(work_dir, &run.repo);
    let bare = Path::new(&core.repos_dir).join(format!("{}.git", run.repo));
    let clone_url = format!("file://{}", bare.display());
    ring.push(&format!(
        "$ git clone --depth 1 {clone_url} {}",
        app_dir.display()
    ));
    let clone = tokio::time::timeout(
        Duration::from_secs(CLONE_TIMEOUT_SECS),
        tokio::process::Command::new("git")
            .args(["clone", "--depth", "1", &clone_url])
            .arg(&app_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output(),
    )
    .await;
    match clone {
        Err(_) => {
            ring.push(&format!("[clone 超时（>{CLONE_TIMEOUT_SECS}s）]"));
            return ("failed".into(), None, Some(EXIT_TIMEOUT));
        }
        Ok(Err(e)) => {
            ring.push(&format!("[git 进程启动失败: {e}]"));
            return ("failed".into(), None, None);
        }
        Ok(Ok(out)) if !out.status.success() => {
            let tail: Vec<String> = String::from_utf8_lossy(&out.stderr)
                .lines()
                .rev()
                .take(5)
                .map(str::to_string)
                .collect();
            ring.push(&format!(
                "[clone 失败 exit={:?}] {}",
                out.status.code(),
                tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
            ));
            return ("failed".into(), None, out.status.code());
        }
        Ok(Ok(_)) => {
            ring.push("[clone ok]");
            core.db_flush_log(run_id, ring);
        }
    }

    // 2) 流水线探测（clone 后应用目录；皆无 → skipped 诚实状态）
    let Some(steps) = detect_pipeline(&app_dir) else {
        ring.push("[无可用流水线：应用目录未探测到 Cargo.toml / package.json]");
        return ("skipped".into(), None, None);
    };
    let desc = describe_pipeline(&steps);
    core.db_update_status(run_id, "running", Some(&desc), None, false);

    // 2.5) package.json 流水线 → monorepo 骨架注入（vue-tsc/vite 前；主仓或
    //      SDK 缺失如实 failed 附指引——诚实不静默跳过）
    if app_dir.join("package.json").is_file() {
        if inject_monorepo_skeleton(work_dir, &run.repo, ring).is_err() {
            return ("failed".into(), None, None);
        }
        core.db_flush_log(run_id, ring);
    }

    // 3) 串联执行（应用目录为 cwd；任一步失败/超时即 failed 短路）
    for step in &steps {
        ring.push(&format!("$ {}", step.join(" ")));
        let program = match resolve_program(core, step[0].as_str()) {
            Some(p) => p,
            None => {
                ring.push(&format!(
                    "[程序未找到: {}（env {}{} 可指定绝对路径）]",
                    step[0],
                    ENV_BIN_PREFIX,
                    step[0].to_uppercase()
                ));
                return ("failed".into(), Some(desc), None);
            }
        };
        match run_step_streaming(core, run_id, ring, &app_dir, &program, step, core.step_timeout)
            .await
        {
            StepOutcome::Exit(0) => ring.push("[exit 0]"),
            StepOutcome::Exit(code) => {
                ring.push(&format!("[exit {code}]"));
                return ("failed".into(), Some(desc), Some(code));
            }
            StepOutcome::Timeout => {
                ring.push(&format!(
                    "[步骤超时（>{}s），已终止]",
                    core.step_timeout.as_secs()
                ));
                return ("failed".into(), Some(desc), Some(EXIT_TIMEOUT));
            }
            StepOutcome::Spawn(e) => {
                ring.push(&format!("[spawn 失败: {e}]"));
                return ("failed".into(), Some(desc), None);
            }
        }
        if ring.dirty >= LOG_FLUSH_EVERY {
            core.db_flush_log(run_id, ring);
        }
    }
    core.db_flush_log(run_id, ring);
    ("passed".into(), Some(desc), Some(0))
}

/// 步骤执行结果。
enum StepOutcome {
    /// 正常退出（含非零）。
    Exit(i32),
    /// 超时被杀。
    Timeout,
    /// spawn 失败（程序缺失等）。
    Spawn(String),
}

/// 子进程输出泵：逐行读 `stream` 送 `tx`（stdout/stderr 通用）。
fn spawn_line_pump<R>(stream: R, tx: tokio::sync::mpsc::UnboundedSender<String>) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send(line).is_err() {
                break;
            }
        }
    })
}

/// 执行单步骤：stdout/stderr 逐行实时入环（攒批刷盘），整步硬超时 kill。
async fn run_step_streaming(
    core: &CiCore,
    run_id: &str,
    ring: &mut LogRing,
    cwd: &Path,
    program: &Path,
    step: &[String],
    timeout: Duration,
) -> StepOutcome {
    let mut child = match tokio::process::Command::new(program)
        .args(&step[1..])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return StepOutcome::Spawn(e.to_string()),
    };
    // 双管道泵：逐行送 mpsc（环写入只在本任务，免锁）
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let mut pumps = Vec::new();
    if let Some(out) = child.stdout.take() {
        pumps.push(spawn_line_pump(out, tx.clone()));
    }
    if let Some(err) = child.stderr.take() {
        pumps.push(spawn_line_pump(err, tx.clone()));
    }
    drop(tx);

    // 实时收行（攒批落库），整步硬超时
    let deadline = tokio::time::Instant::now() + timeout;
    let mut timed_out = false;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(line)) => {
                ring.push(&line);
                if ring.dirty >= LOG_FLUSH_EVERY {
                    core.db_flush_log(run_id, ring);
                }
            }
            Ok(None) => break, // 双管道 EOF（进程退出）
            Err(_) => {
                timed_out = true;
                break;
            }
        }
    }
    if timed_out {
        let _ = child.kill().await;
    }
    for p in pumps {
        p.abort();
    }
    // 收残量（kill 后管道余行）
    while let Ok(line) = rx.try_recv() {
        ring.push(&line);
    }
    match child.wait().await {
        Ok(_) if timed_out => StepOutcome::Timeout,
        Ok(st) => StepOutcome::Exit(st.code().unwrap_or(-1)),
        Err(e) => StepOutcome::Spawn(e.to_string()),
    }
}

// ----------------------------------------------------------------------------
// push 自动触发（http.rs git_http_handler 成功路径调用）
// ----------------------------------------------------------------------------

/// 进程内单例槽（`NexhubCiRouteHandler::new()` 装配时安装；push 钩子读取）。
static CORE_SLOT: Mutex<Option<Arc<CiCore>>> = Mutex::new(None);

/// 安装全局核心（装配时调用一次；重复安装保留首个，返回本次是否生效）。
pub fn install_global_core(core: Arc<CiCore>) -> bool {
    let mut slot = CORE_SLOT.lock().expect("core slot lock");
    if slot.is_some() {
        return false;
    }
    *slot = Some(core);
    true
}

/// 取全局核心（push 钩子用；未装配返回 None——CI 未装配时 push 不受影响）。
#[must_use]
pub fn global_core() -> Option<Arc<CiCore>> {
    CORE_SLOT.lock().expect("core slot lock").clone()
}

/// push 成功钩子：env 开关（缺省开）+ 仓库名校验 + 全局核心入队（trigger=push）。
/// 任何失败都只记 `[ci]` 日志，绝不影响 push 响应（CI 是旁路）。
pub fn push_hook(repo: &str) {
    push_hook_with(global_core(), repo);
}

/// [`push_hook`] 的可注入形态（测试用）。
pub fn push_hook_with(core: Option<Arc<CiCore>>, repo: &str) {
    if auto_push_disabled() {
        return;
    }
    if !valid_ci_repo_name(repo) {
        eprintln!("[ci] push 钩子跳过（仓库名非法）: {repo}");
        return;
    }
    let Some(core) = core else {
        return;
    };
    let repo = repo.trim().trim_end_matches(".git").to_string();
    match core.enqueue(&repo, "push") {
        Ok(id) => eprintln!("[ci] push 自动触发 run {id}（{repo}）"),
        Err((code, msg)) => eprintln!("[ci] push 自动触发失败（{code}）: {msg}"),
    }
}

/// env `NEXOS_CI_AUTO_PUSH` 显式关闭（`0` / `false`，大小写不敏感；缺省开）。
fn auto_push_disabled() -> bool {
    std::env::var(ENV_AUTO_PUSH)
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            v == "0" || v == "false"
        })
        .unwrap_or(false)
}

// ----------------------------------------------------------------------------
// Handler
// ----------------------------------------------------------------------------

/// NexHub CI 路由处理器（组件 `nexhub_ci`；生产 `new()` 安装全局核心供 push 钩子）。
pub struct NexhubCiRouteHandler {
    core: Arc<CiCore>,
}

impl NexhubCiRouteHandler {
    /// 生产构造（默认路径 + 安装全局核心）。
    #[must_use]
    pub fn new() -> Self {
        let core = CiCore::open_default();
        install_global_core(Arc::clone(&core));
        Self { core }
    }

    /// 注入核心构造（测试隔离 / 特殊装配）。
    #[must_use]
    pub fn with_core(core: Arc<CiCore>) -> Self {
        Self { core }
    }
}

impl Default for NexhubCiRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

fn spec(method: HttpMethod, path: &str, requires_auth: bool) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: "nexhub_ci".to_string(),
        requires_auth,
        required_roles: if requires_auth {
            vec!["admin".into()]
        } else {
            vec![]
        },
    }
}

#[async_trait]
impl RouteHandler for NexhubCiRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec(HttpMethod::Post, "/api/v1/coderepo/repos/:name/ci", true),
            spec(HttpMethod::Get, "/api/v1/coderepo/repos/:name/ci", false),
            spec(
                HttpMethod::Get,
                "/api/v1/coderepo/repos/:name/ci/:run_id",
                false,
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/coderepo/repos/:name/ci/:run_id",
                true,
            ),
            spec(HttpMethod::Get, "/api/v1/coderepo/ci/latest", false),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs: Vec<&str> = req
            .path
            .split('?')
            .next()
            .unwrap_or(&req.path)
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        match (req.method, segs.as_slice()) {
            // —— POST /repos/:name/ci —— 手动触发（admin 由网关路由层强制）→ 202
            (HttpMethod::Post, ["api", "v1", "coderepo", "repos", name, "ci"]) => {
                match self.core.enqueue(name, "manual") {
                    Ok(id) => {
                        let run = self.core.db_get(&id);
                        Ok(ApiResponse {
                            status: 202,
                            body: serde_json::json!({ "ok": true, "run": run }),
                            headers: serde_json::json!({}),
                        })
                    }
                    Err((code, msg)) => Ok(error_response(code, &msg)),
                }
            }

            // —— GET /repos/:name/ci —— 该仓 runs（最新 20，不含 log）
            (HttpMethod::Get, ["api", "v1", "coderepo", "repos", name, "ci"]) => Ok(ok_json(
                serde_json::json!({
                    "repo": name,
                    "runs": self.core.list_runs(name),
                }),
            )),

            // —— GET /repos/:name/ci/:run_id —— 详情 + 环形日志全文
            (HttpMethod::Get, ["api", "v1", "coderepo", "repos", name, "ci", run_id]) => {
                match self.core.db_get(run_id) {
                    Some(run) if run.repo_name == *name => {
                        Ok(ok_json(serde_json::json!({ "run": run })))
                    }
                    _ => Ok(error_response(404, "run 不存在（或与仓库不匹配）")),
                }
            }

            // —— DELETE /repos/:name/ci/:run_id —— 清记录（admin；未终态 409）
            (HttpMethod::Delete, ["api", "v1", "coderepo", "repos", name, "ci", run_id]) => {
                match self.core.db_get(run_id) {
                    None => Ok(error_response(404, "run 不存在")),
                    Some(run) if run.repo_name != *name => {
                        Ok(error_response(404, "run 与仓库不匹配"))
                    }
                    Some(run) if run.status == "queued" || run.status == "running" => {
                        Ok(error_response(409, &format!("run 进行中（{}），不可删除", run.status)))
                    }
                    Some(run) => {
                        let n = self.core.db_delete(&run.id);
                        if n == 0 {
                            return Ok(error_response(404, "run 不存在"));
                        }
                        eprintln!("[ci] run {} 记录已删除（{name}）", run.id);
                        Ok(ok_json(serde_json::json!({ "ok": true, "id": run.id })))
                    }
                }
            }

            // —— GET /ci/latest —— 各仓最新 run 摘要（仓库卡徽章一次拉全）
            (HttpMethod::Get, ["api", "v1", "coderepo", "ci", "latest"]) => {
                Ok(ok_json(serde_json::json!({ "latest": self.core.latest_per_repo() })))
            }

            _ => Ok(error_response(404, "nexhub_ci: 未匹配的路由")),
        }
    }
}

// ----------------------------------------------------------------------------
// 响应小工场（apps_handler 同款）
// ----------------------------------------------------------------------------

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
        body: serde_json::json!({ "error": msg }),
        headers: serde_json::json!({}),
    }
}

// ----------------------------------------------------------------------------
// 测试（真实 git fixture + stub 程序；全部临时目录隔离，不碰真实 /tank）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// env 触碰类测试互斥（进程全局 env，并行测试会互踩——forwarding 同款锁；
    /// tokio Mutex：骨架类测试须持锁跨 .await 等 worker 跑完，env 才不能被改）。
    static ENV_LOCK: once_cell::sync::Lazy<tokio::sync::Mutex<()>> =
        once_cell::sync::Lazy::new(|| tokio::sync::Mutex::new(()));

    // ---- 探测三态（纯函数）----

    #[test]
    fn detect_pipeline_three_states() {
        let dir = std::env::temp_dir().join(format!("ci-detect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // 1) 皆无 → None
        let none_dir = dir.join("none");
        std::fs::create_dir_all(&none_dir).unwrap();
        assert!(detect_pipeline(&none_dir).is_none(), "空目录应无流水线");
        // 2) 仅 Cargo.toml → cargo check 一段
        let cargo_dir = dir.join("cargo");
        std::fs::create_dir_all(&cargo_dir).unwrap();
        std::fs::write(cargo_dir.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        let steps = detect_pipeline(&cargo_dir).expect("cargo 仓应有流水线");
        assert_eq!(steps.len(), 1);
        assert_eq!(
            describe_pipeline(&steps),
            "cargo check --workspace --all-targets"
        );
        // 3) 仅 package.json → npm ci + npm run build 两段
        let npm_dir = dir.join("npm");
        std::fs::create_dir_all(&npm_dir).unwrap();
        std::fs::write(npm_dir.join("package.json"), "{}").unwrap();
        let steps = detect_pipeline(&npm_dir).expect("npm 仓应有流水线");
        assert_eq!(steps.len(), 2);
        assert_eq!(describe_pipeline(&steps), "npm ci && npm run build");
        // 4) 两者皆有 → 三步串联（cargo 1 + npm 2）
        std::fs::write(cargo_dir.join("package.json"), "{}").unwrap();
        let steps = detect_pipeline(&cargo_dir).expect("双标志仓应有流水线");
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0][0], "cargo");
        assert_eq!(steps[1][0], "npm");
        assert_eq!(steps[2], vec!["npm", "run", "build"]);
        // 目录（非文件）形态不误判
        std::fs::create_dir_all(none_dir.join("package.json")).unwrap();
        assert!(detect_pipeline(&none_dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 环形日志（500 行丢最旧）----

    #[test]
    fn log_ring_drops_oldest_beyond_cap() {
        let mut ring = LogRing::new(LOG_RING_LINES);
        for i in 0..600 {
            ring.push(&format!("line-{i}"));
        }
        assert_eq!(ring.len(), LOG_RING_LINES, "超容后应恒为 500 行");
        let text = ring.render();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 500);
        assert_eq!(lines.first(), Some(&"line-100"), "最旧 100 行（line-0..99）被挤出");
        assert_eq!(lines.last(), Some(&"line-599"));
        assert!(!text.contains("line-0\n"), "line-0 应被丢弃");
        // 空环
        let empty = LogRing::new(10);
        assert!(empty.is_empty());
        assert_eq!(empty.render(), "");
    }

    // ---- 仓库名规约 ----

    #[test]
    fn valid_ci_repo_name_rules() {
        assert!(valid_ci_repo_name("nexos-app-film"));
        assert!(valid_ci_repo_name("nexos-app-film.git"));
        assert!(valid_ci_repo_name("a.b_c-d"));
        assert!(!valid_ci_repo_name(""));
        assert!(!valid_ci_repo_name("../etc"));
        assert!(!valid_ci_repo_name("a/b"));
        assert!(!valid_ci_repo_name(".hidden"));
        assert!(!valid_ci_repo_name("-lead"));
        assert!(!valid_ci_repo_name("has space"));
    }

    // ---- fixture 工具 ----

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ci-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 建含初始提交的裸仓库（root_files: 工作树根文件 → 内容）。
    fn make_bare_repo(repos_dir: &Path, repo: &str, root_files: &[(&str, &str)]) {
        let bare = repos_dir.join(format!("{repo}.git"));
        assert!(
            std::process::Command::new("git")
                .args(["init", "--bare", "-b", "main", bare.to_str().unwrap()])
                .output()
                .expect("git init")
                .status
                .success(),
            "git init --bare 失败"
        );
        let work = repos_dir.join(format!(".{repo}-seed"));
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).unwrap();
        for (name, content) in root_files {
            std::fs::write(work.join(name), content).unwrap();
        }
        for args in [
            vec!["init"],
            vec!["add", "-A"],
            vec![
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "seed",
            ],
            vec!["push", bare.to_str().unwrap(), "HEAD:main"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(work.to_str().unwrap())
                    .args(&args)
                    .output()
                    .expect("git seed")
                    .status
                    .success(),
                "git seed 步骤失败: {args:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&work);
    }

    /// 写 stub 程序（POSIX sh）：打印 `lines` 行 stdout + 1 行 stderr，睡
    /// `sleep_secs`，按 `exit_code` 退出；`marks` 非空时在睡前后追加
    /// `S <ms>` / `E <ms>` 标记（并发断言用——路径烘焙进脚本，免进程级 env 竞态）。
    fn make_stub(dir: &Path, name: &str, lines: usize, sleep_secs: &str, exit_code: i32, marks: &str) -> PathBuf {
        let p = dir.join(format!("stub-{name}.sh"));
        std::fs::write(
            &p,
            format!(
                "#!/bin/sh\n\
                 for i in $(seq 1 {lines}); do echo \"stub-out-$i\"; done\n\
                 echo \"stub-err-1\" >&2\n\
                 if [ -n \"{marks}\" ]; then echo \"S $(date +%s%3N)\" >> \"{marks}\"; fi\n\
                 sleep {sleep_secs}\n\
                 if [ -n \"{marks}\" ]; then echo \"E $(date +%s%3N)\" >> \"{marks}\"; fi\n\
                 exit {exit_code}\n"
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    /// 轮询等待某仓达到终态的 run 数（超时 panic，带现场）。
    ///
    /// 必须用 `tokio::time::sleep` 让出执行权：worker 是 `tokio::spawn` 的任务，
    /// current-thread 测试 runtime 下阻塞（std::thread::sleep）会饿死它。
    async fn wait_finished(core: &CiCore, repo: &str, want: usize, timeout_ms: u64) -> Vec<CiRun> {
        let started = std::time::Instant::now();
        loop {
            let finished: Vec<CiRun> = core
                .list_runs(repo)
                .into_iter()
                .filter(|r| r.status != "queued" && r.status != "running")
                .collect();
            if finished.len() >= want {
                return finished;
            }
            if started.elapsed().as_millis() as u64 > timeout_ms {
                panic!(
                    "等待 {repo} 达到 {want} 条终态 run 超时: {:?}",
                    core.list_runs(repo)
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    fn get_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.to_string(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    fn req(method: HttpMethod, path: &str) -> ApiRequest {
        ApiRequest {
            method,
            path: path.to_string(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    /// 归一 stub 时间戳为「秒×1e9 + 9 位小数」定宽值（仅保序，不保刻度）。
    ///
    /// 本机是 uutils coreutils：`date +%s%3N` 忽略 3 位宽打**全 9 位纳秒**，
    /// 且纳秒段前导零会被数值化丢掉（ns<1e8 时标记得 18 位而非 19 位）——
    /// 裸整型比较会把 18 位排到所有 19 位之前，峰值被低估（实测 flaky）。
    /// GNU 机器 `%s%3N` 是 13 位毫秒，同式拆「前 10 位秒 + 尾段左补零到 9」
    /// 亦保序。秒恒 10 位（epoch 到 2286 年），拆点安全。
    fn norm_mark_ts(ts: i64) -> i64 {
        let s = ts.to_string();
        if s.len() <= 10 {
            return ts.saturating_mul(1_000_000_000);
        }
        let (sec, frac) = s.split_at(10);
        let sec: i64 = sec.parse().unwrap_or(0);
        let frac: i64 = format!("{frac:0>9}").parse().unwrap_or(0);
        sec.saturating_mul(1_000_000_000).saturating_add(frac)
    }

    /// 从 S/E 标记行算并发峰值（同刻并列时 E 先于 S 计——峰值只低不高，
    /// `peak <= N` 断言因此稳；`peak >= 2` 由亚秒级 sleep 间隔保证可观测）。
    fn peak_concurrency(marks_text: &str) -> (i32, usize) {
        let mut events: Vec<(i64, i32)> = marks_text
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                let kind = it.next()?;
                let ts: i64 = it.next()?.parse().ok()?;
                Some((
                    norm_mark_ts(ts),
                    match kind {
                        "S" => 1,
                        _ => -1,
                    },
                ))
            })
            .collect();
        let total = events.len();
        events.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1))); // 同刻：-1(E) 在前
        let mut cur = 0i32;
        let mut peak = 0i32;
        for (_, d) in &events {
            cur += d;
            peak = peak.max(cur);
        }
        (peak, total)
    }

    // ---- 全生命周期（stub 通过 → passed；环形日志 + 状态机字段）----

    #[tokio::test]
    async fn full_run_passes_with_streamed_log() {
        let root = temp_root("pass");
        // 490 行 stdout + 1 stderr + clone/步骤头/退出码 ≈ 495 行 < 环容 500：
        // 全程保留（丢最旧行为由 log_ring_drops_oldest_beyond_cap 专测）。
        let stub = make_stub(&root, "cargo", 490, "0", 0, "");
        let core = Arc::new(
            CiCore::with_paths(
                root.join("ci.db").to_str().unwrap(),
                root.join("repos").to_str().unwrap(),
                &root.join("work"),
            )
            .with_step_timeout(Duration::from_secs(30)),
        );
        core.set_bin_override("cargo", stub.to_str().unwrap());
        make_bare_repo(&root.join("repos"), "demo", &[("Cargo.toml", "[package]\n")]);

        let id = core.enqueue("demo", "manual").expect("入队");
        let runs = wait_finished(&core, "demo", 1, 30_000).await;
        assert_eq!(runs.len(), 1);
        let r = &runs[0];
        assert_eq!(r.id, id);
        assert_eq!(r.status, "passed", "stub exit 0 应 passed");
        assert_eq!(r.trigger, "manual");
        assert_eq!(r.exit_code, Some(0));
        assert_eq!(
            r.pipeline.as_deref(),
            Some("cargo check --workspace --all-targets")
        );
        assert!(r.duration_ms.is_some_and(|d| d >= 0));
        assert!(r.created_at.is_some() && r.started_at.is_some() && r.finished_at.is_some());
        assert!(r.log.is_none(), "列表不带 log: {r:?}");

        // 详情：环形 500 行（605 行总量丢最旧 105）
        let resp = NexhubCiRouteHandler::with_core(Arc::clone(&core))
            .handle(get_req(&format!("/api/v1/coderepo/repos/demo/ci/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let log = resp.body["run"]["log"].as_str().expect("详情应带 log");
        assert_eq!(log.lines().count(), 495);
        assert!(log.contains("stub-out-1"), "首行输出保留（未超环容）");
        assert!(log.contains("stub-out-490"), "最新输出保留");
        assert!(log.contains("$ git clone"), "含 clone 记录");
        assert!(log.contains("$ cargo check"), "含步骤头");
        assert!(log.contains("[exit 0]"), "含步骤退出码");
        assert!(log.contains("stub-err-1"), "stderr 同样入环");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn failed_run_records_exit_code() {
        let root = temp_root("fail");
        let stub = make_stub(&root, "cargo", 2, "0", 3, "");
        let core = Arc::new(
            CiCore::with_paths(
                root.join("ci.db").to_str().unwrap(),
                root.join("repos").to_str().unwrap(),
                &root.join("work"),
            )
            .with_step_timeout(Duration::from_secs(30)),
        );
        core.set_bin_override("cargo", stub.to_str().unwrap());
        make_bare_repo(&root.join("repos"), "bad", &[("Cargo.toml", "x")]);
        core.enqueue("bad", "manual").unwrap();
        let runs = wait_finished(&core, "bad", 1, 30_000).await;
        assert_eq!(runs[0].status, "failed");
        assert_eq!(runs[0].exit_code, Some(3), "非零退出码透传");
        assert!(runs[0].pipeline.is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn skipped_when_no_pipeline_detected() {
        let root = temp_root("skip");
        let core = Arc::new(CiCore::with_paths(
            root.join("ci.db").to_str().unwrap(),
            root.join("repos").to_str().unwrap(),
            &root.join("work"),
        ));
        // 仓库只有 README：探测皆无 → skipped
        make_bare_repo(&root.join("repos"), "docs", &[("README.md", "hi")]);
        let id = core.enqueue("docs", "push").unwrap();
        let runs = wait_finished(&core, "docs", 1, 30_000).await;
        assert_eq!(runs[0].status, "skipped");
        assert_eq!(runs[0].trigger, "push", "push 触发口径保持");
        assert!(runs[0].pipeline.is_none(), "skipped 无流水线描述");
        assert_eq!(runs[0].exit_code, None);
        // 详情日志写明原因，且未执行任何步骤
        let run = core.db_get(&id).unwrap();
        let log = run.log.unwrap_or_default();
        assert!(log.contains("无可用流水线"), "{log}");
        assert!(!log.contains("$ cargo"), "skipped 不执行任何步骤");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn empty_repo_clones_then_skips() {
        let root = temp_root("clonefail");
        let core = Arc::new(CiCore::with_paths(
            root.join("ci.db").to_str().unwrap(),
            root.join("repos").to_str().unwrap(),
            &root.join("work"),
        ));
        // 空裸仓库（无提交）：现代 git 对空仓 clone 仍 exit 0（附 warning）→
        // 工作树为空 → 探测皆无 → skipped（诚实：无流水线，而非假装失败）。
        let repos = root.join("repos");
        std::fs::create_dir_all(&repos).unwrap();
        let bare = repos.join("empty.git");
        assert!(
            std::process::Command::new("git")
                .args(["init", "--bare", "-b", "main", bare.to_str().unwrap()])
                .output()
                .unwrap()
                .status
                .success()
        );
        core.enqueue("empty", "manual").unwrap();
        let runs = wait_finished(&core, "empty", 1, 30_000).await;
        assert_eq!(runs[0].status, "skipped", "空仓库 clone 成功但无流水线 → skipped");
        assert!(runs[0].pipeline.is_none());
        // 真 clone 失败路径（裸仓库目录存在但损坏）→ failed
        let broken = repos.join("broken.git");
        std::fs::create_dir_all(&broken).unwrap();
        core.enqueue("broken", "manual").unwrap();
        let runs = wait_finished(&core, "broken", 1, 30_000).await;
        assert_eq!(runs[0].status, "failed", "损坏裸仓库 clone 失败 → failed");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn timeout_kills_step_and_marks_failed_124() {
        let root = temp_root("timeout");
        let stub = make_stub(&root, "cargo", 1, "5", 0, ""); // 睡 5s
        let core = Arc::new(
            CiCore::with_paths(
                root.join("ci.db").to_str().unwrap(),
                root.join("repos").to_str().unwrap(),
                &root.join("work"),
            )
            .with_step_timeout(Duration::from_millis(400)), // 步骤超时 400ms
        );
        core.set_bin_override("cargo", stub.to_str().unwrap());
        make_bare_repo(&root.join("repos"), "slow", &[("Cargo.toml", "x")]);
        core.enqueue("slow", "manual").unwrap();
        let runs = wait_finished(&core, "slow", 1, 30_000).await;
        assert_eq!(runs[0].status, "failed");
        assert_eq!(runs[0].exit_code, Some(EXIT_TIMEOUT), "超时记 124");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn program_resolution_and_override() {
        let _guard = ENV_LOCK.lock().await;
        let root = temp_root("binres");
        let core = Arc::new(CiCore::with_paths(
            root.join("ci.db").to_str().unwrap(),
            root.join("repos").to_str().unwrap(),
            &root.join("work"),
        ));
        // 带 '/' 的程序名直通语义：存在 → Some；不存在 → None
        let real_git = resolve_program(&core, "/usr/bin/git");
        assert!(real_git.is_some_and(|p| p.ends_with("git")));
        assert!(resolve_program(&core, "/nonexistent-dir-xyz/prog").is_none());
        // npm-only 仓走 stub（骨架注入用 fake monorepo，勿依赖真机 /home/oem/NexOS）
        std::env::set_var(ENV_MONOREPO, make_fake_monorepo(&root, "mono"));
        let stub = make_stub(&root, "npm", 2, "0", 0, "");
        core.set_bin_override("npm", stub.to_str().unwrap());
        make_bare_repo(&root.join("repos"), "npmonly", &[("package.json", "{}")]);
        core.enqueue("npmonly", "manual").unwrap();
        let runs = wait_finished(&core, "npmonly", 1, 30_000).await;
        assert_eq!(runs[0].status, "passed", "覆盖后走 stub: {:?}", runs[0].pipeline);
        assert_eq!(
            runs[0].pipeline.as_deref(),
            Some("npm ci && npm run build"),
            "npm-only 仓两段串联"
        );
        std::env::remove_var(ENV_MONOREPO);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- monorepo 骨架注入（v0.1.34）----

    /// 建 fake monorepo fixture：`crates/os-api/web/src/sdk/index.ts`（含标记内容，
    /// 供「注入后相对路径可达」断言）。返回根路径字符串。
    fn make_fake_monorepo(root: &Path, name: &str) -> String {
        let mono = root.join(name).join("crates/os-api/web/src/sdk");
        std::fs::create_dir_all(&mono).unwrap();
        std::fs::write(mono.join("index.ts"), "// fake sdk marker-SDK-CONTENT\n").unwrap();
        std::fs::write(mono.join("gateway.ts"), "export const g = 1\n").unwrap();
        root.join(name).to_str().unwrap().to_string()
    }

    /// 写「SDK 相对路径探针」stub：在 cwd 检查 `../../crates/os-api/web/src/sdk/
    /// index.ts`（应用目录锚点），命中打印 SDK-REACHABLE + 文件内容标记。
    fn make_sdk_probe_stub(dir: &Path) -> PathBuf {
        let p = dir.join("stub-sdk-probe.sh");
        std::fs::write(
            &p,
            "#!/bin/sh\n\
             if [ -f \"../../crates/os-api/web/src/sdk/index.ts\" ]; then\n\
               echo SDK-REACHABLE:$(head -c 40 ../../crates/os-api/web/src/sdk/index.ts)\n\
             else\n\
               echo SDK-MISSING-at-$PWD\n\
             fi\n\
             exit 0\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    #[test]
    fn skeleton_layout_path_assembly() {
        // 词法归一（解 `..`/`.`，不触盘）——验证相对锚点用
        fn lex_normalize(p: &Path) -> PathBuf {
            let mut out = PathBuf::new();
            for c in p.components() {
                match c {
                    std::path::Component::ParentDir => {
                        out.pop();
                    }
                    std::path::Component::CurDir => {}
                    other => out.push(other.as_os_str()),
                }
            }
            out
        }
        let work = Path::new("/tmp/w");
        let layout = skeleton_layout(work, "nexos-app-film", Path::new("/mono"));
        // clone 落点 <work>/apps/<repo>；SDK 源/落点与主仓内布局一致——
        // 应用目录内 `../../` 恰好落回 <work> 命中注入位。
        assert_eq!(layout.app_dir, Path::new("/tmp/w/apps/nexos-app-film"));
        assert_eq!(
            layout.sdk_src,
            Path::new("/mono/crates/os-api/web/src/sdk")
        );
        assert_eq!(
            layout.sdk_dest,
            Path::new("/tmp/w/crates/os-api/web/src/sdk")
        );
        assert!(
            lex_normalize(&layout.app_dir.join("../../crates/os-api/web/src/sdk"))
                == layout.sdk_dest,
            "相对锚点必须命中注入落点"
        );
        // 仓名带 .git 后缀不落路径（enqueue 已规约，layout 只拼原名）
        let l2 = skeleton_layout(work, "x", Path::new("/m"));
        assert_eq!(l2.app_dir, Path::new("/tmp/w/apps/x"));
    }

    #[tokio::test]
    async fn skeleton_injection_makes_relative_sdk_reachable() {
        let _guard = ENV_LOCK.lock().await;
        let root = temp_root("skelinj");
        let core = Arc::new(CiCore::with_paths(
            root.join("ci.db").to_str().unwrap(),
            root.join("repos").to_str().unwrap(),
            &root.join("work"),
        ));
        // fake monorepo（SDK 2 文件）+ npm 探针 stub（cwd 检查 ../../ 相对路径）
        std::env::set_var(ENV_MONOREPO, make_fake_monorepo(&root, "mono"));
        let probe = make_sdk_probe_stub(&root);
        core.set_bin_override("npm", probe.to_str().unwrap());
        make_bare_repo(
            &root.join("repos"),
            "nexos-app-x",
            &[("package.json", "{\"name\":\"x\"}")],
        );
        let id = core.enqueue("nexos-app-x", "push").unwrap();
        let runs = wait_finished(&core, "nexos-app-x", 1, 30_000).await;
        assert_eq!(runs[0].status, "passed", "骨架注入后 npm 步骤应通过");
        let run = core.db_get(&id).unwrap();
        let log = run.log.unwrap_or_default();
        assert!(log.contains("[骨架] 注入 SDK"), "骨架步骤日志可见: {log}");
        assert!(log.contains("2 文件"), "fake SDK 恰 2 文件: {log}");
        assert!(
            log.contains("SDK-REACHABLE:// fake sdk marker-SDK-CONTENT"),
            "步骤 cwd 内 ../../ 相对路径可达: {log}"
        );
        assert!(!log.contains("SDK-MISSING"), "{log}");
        std::env::remove_var(ENV_MONOREPO);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn skeleton_missing_monorepo_or_sdk_fails_with_guidance() {
        let _guard = ENV_LOCK.lock().await;
        let root = temp_root("skelmiss");
        let core = Arc::new(CiCore::with_paths(
            root.join("ci.db").to_str().unwrap(),
            root.join("repos").to_str().unwrap(),
            &root.join("work"),
        ));
        // 有 SDK 探针 stub 也不该被执行：骨架失败在 npm 步骤前短路
        let probe = make_sdk_probe_stub(&root);
        core.set_bin_override("npm", probe.to_str().unwrap());
        make_bare_repo(
            &root.join("repos"),
            "nexos-app-y",
            &[("package.json", "{\"name\":\"y\"}")],
        );

        // 1) 主仓根不存在 → failed + 指引
        std::env::set_var(ENV_MONOREPO, root.join("no-such-mono").to_str().unwrap());
        let mut id = core.enqueue("nexos-app-y", "manual").unwrap();
        let runs = wait_finished(&core, "nexos-app-y", 1, 30_000).await;
        assert_eq!(runs[0].status, "failed", "主仓缺失必须如实 failed");
        assert_eq!(runs[0].exit_code, None, "未执行任何步骤，无退出码");
        let run = core.db_get(&id).unwrap();
        let log = run.log.clone().unwrap_or_default();
        assert!(log.contains("主仓 monorepo 不存在"), "{log}");
        assert!(log.contains("本机无 monorepo，应用构建需 NexOS 环境"), "{log}");
        assert!(!log.contains("$ npm"), "骨架失败在 npm 前短路: {log}");

        // 2) 主仓存在但 SDK 目录缺失 → 同款 failed + 指引
        let empty_mono = root.join("empty-mono");
        std::fs::create_dir_all(&empty_mono).unwrap();
        std::env::set_var(ENV_MONOREPO, empty_mono.to_str().unwrap());
        id = core.enqueue("nexos-app-y", "manual").unwrap();
        let runs = wait_finished(&core, "nexos-app-y", 2, 30_000).await;
        assert_eq!(runs[0].status, "failed", "SDK 缺失必须如实 failed");
        let run = core.db_get(&id).unwrap();
        let log = run.log.unwrap_or_default();
        assert!(log.contains("主仓 SDK 不存在"), "{log}");
        assert!(log.contains("需 NexOS 环境"), "{log}");
        assert!(!log.contains("$ npm"), "{log}");
        std::env::remove_var(ENV_MONOREPO);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- 并发控制：同仓串行 + 全局 ≤2 ----

    #[tokio::test]
    async fn same_repo_runs_execute_serially() {
        let root = temp_root("serial");
        let marks = root.join("marks.log");
        let stub = make_stub(&root, "cargo", 1, "0.4", 0, marks.to_str().unwrap());
        let core = Arc::new(
            CiCore::with_paths(
                root.join("ci.db").to_str().unwrap(),
                root.join("repos").to_str().unwrap(),
                &root.join("work"),
            )
            .with_step_timeout(Duration::from_secs(30)),
        );
        core.set_bin_override("cargo", stub.to_str().unwrap());
        make_bare_repo(&root.join("repos"), "one", &[("Cargo.toml", "x")]);
        for _ in 0..3 {
            core.enqueue("one", "manual").unwrap();
        }
        let runs = wait_finished(&core, "one", 3, 60_000).await;
        assert!(runs.iter().all(|r| r.status == "passed"));

        let text = std::fs::read_to_string(&marks).unwrap();
        let (peak, total) = peak_concurrency(&text);
        assert_eq!(total, 6, "3 run × (S,E)");
        assert!(peak <= 1, "同仓必须串行（实测峰值 {peak}）: {text}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn global_concurrency_capped_at_two() {
        let root = temp_root("global");
        let marks = root.join("marks.log");
        let stub = make_stub(&root, "cargo", 1, "0.6", 0, marks.to_str().unwrap());
        let core = Arc::new(
            CiCore::with_paths(
                root.join("ci.db").to_str().unwrap(),
                root.join("repos").to_str().unwrap(),
                &root.join("work"),
            )
            .with_step_timeout(Duration::from_secs(30)),
        );
        core.set_bin_override("cargo", stub.to_str().unwrap());
        let repos = ["ga", "gb", "gc", "gd"];
        for r in repos {
            make_bare_repo(&root.join("repos"), r, &[("Cargo.toml", "x")]);
        }
        for r in repos {
            core.enqueue(r, "manual").unwrap();
        }
        for r in repos {
            let runs = wait_finished(&core, r, 1, 60_000).await;
            assert_eq!(runs[0].status, "passed", "{r}");
        }
        let text = std::fs::read_to_string(&marks).unwrap();
        let (peak, total) = peak_concurrency(&text);
        assert_eq!(total, 8, "4 run × (S,E)");
        assert!(
            peak <= MAX_CONCURRENT_RUNS as i32,
            "全局并发不得超 {MAX_CONCURRENT_RUNS}（实测峰值 {peak}）: {text}"
        );
        assert!(peak >= 2, "应观察到 2 并发（信号量在用）: {text}");
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- 端点：列表上限 / 详情 / 删除规则 / latest 聚合 ----

    #[tokio::test]
    async fn endpoints_list_limit_delete_and_latest() {
        let root = temp_root("endpoints");
        let core = Arc::new(CiCore::with_paths(
            root.join("ci.db").to_str().unwrap(),
            root.join("repos").to_str().unwrap(),
            &root.join("work"),
        ));
        let h = NexhubCiRouteHandler::with_core(Arc::clone(&core));
        make_bare_repo(&root.join("repos"), "ep", &[("README.md", "x")]); // skipped 快
        // 灌 25 条 finished（skipped）run → 列表恰 20（list_runs 截 20，
        // 完成判定走「最新一条 id == 最后入队 id」而非计数——列表有上限）
        let mut last_id = String::new();
        for _ in 0..25 {
            last_id = core.enqueue("ep", "manual").unwrap();
        }
        // 等最后入队的一条到达终态（bounded，防意外挂死测试进程）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let runs = core.list_runs("ep");
            let newest_done = runs
                .iter()
                .any(|r| r.id == last_id && r.status != "queued" && r.status != "running");
            if newest_done {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "等待最后一条 run（{last_id}）终态超时: {:?}",
                core.list_runs("ep")
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let resp = h
            .handle(get_req("/api/v1/coderepo/repos/ep/ci"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let runs = resp.body["runs"].as_array().unwrap();
        assert_eq!(runs.len(), LIST_LIMIT as usize, "列表截 20");
        assert!(runs[0]["log"].is_null(), "列表不带 log");
        assert!(runs[0]["created_at"].is_string());

        // 详情 404（不存在）
        let resp = h
            .handle(get_req("/api/v1/coderepo/repos/ep/ci/no-such-run"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);

        // 删除：finished 可删；重复删 404
        let some_id = runs[0]["id"].as_str().unwrap().to_string();
        let resp = h
            .handle(req(
                HttpMethod::Delete,
                &format!("/api/v1/coderepo/repos/ep/ci/{some_id}"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{resp:?}");
        let resp = h
            .handle(req(
                HttpMethod::Delete,
                &format!("/api/v1/coderepo/repos/ep/ci/{some_id}"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404, "重复删除 → 404");

        // run 与仓库不匹配 → 404
        make_bare_repo(&root.join("repos"), "other", &[("README.md", "x")]);
        let other_id = core.enqueue("other", "manual").unwrap();
        wait_finished(&core, "other", 1, 30_000).await;
        let resp = h
            .handle(req(
                HttpMethod::Delete,
                &format!("/api/v1/coderepo/repos/ep/ci/{other_id}"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404, "run 与仓库不匹配 → 404");

        // 进行中（running）删除 → 409：慢 stub 造 running
        let slow_root = temp_root("endpoints-running");
        let slow_stub = make_stub(&slow_root, "cargo", 1, "2", 0, "");
        let slow_core = Arc::new(
            CiCore::with_paths(
                slow_root.join("ci.db").to_str().unwrap(),
                slow_root.join("repos").to_str().unwrap(),
                &slow_root.join("work"),
            )
            .with_step_timeout(Duration::from_secs(30)),
        );
        slow_core.set_bin_override("cargo", slow_stub.to_str().unwrap());
        make_bare_repo(&slow_root.join("repos"), "slowrepo", &[("Cargo.toml", "x")]);
        let running_id = slow_core.enqueue("slowrepo", "manual").unwrap();
        let mut tries = 0;
        while slow_core
            .db_get(&running_id)
            .map(|r| r.status)
            .as_deref()
            != Some("running")
            && tries < 150
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            tries += 1;
        }
        let h2 = NexhubCiRouteHandler::with_core(Arc::clone(&slow_core));
        let resp = h2
            .handle(req(
                HttpMethod::Delete,
                &format!("/api/v1/coderepo/repos/slowrepo/ci/{running_id}"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 409, "进行中不可删除: {resp:?}");
        wait_finished(&slow_core, "slowrepo", 1, 30_000).await;
        let _ = std::fs::remove_dir_all(&slow_root);

        // latest 聚合：两仓各取最新一条
        let resp = h
            .handle(get_req("/api/v1/coderepo/ci/latest"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let latest = resp.body["latest"].as_array().unwrap();
        let ep_entry = latest
            .iter()
            .find(|r| r["repo_name"] == "ep")
            .expect("ep 在列");
        assert_eq!(ep_entry["status"], "skipped");
        assert!(latest.iter().any(|r| r["repo_name"] == "other"));
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- 入队校验 ----

    #[tokio::test]
    async fn enqueue_validates_repo() {
        let root = temp_root("enqueue");
        let core = Arc::new(CiCore::with_paths(
            root.join("ci.db").to_str().unwrap(),
            root.join("repos").to_str().unwrap(),
            &root.join("work"),
        ));
        assert_eq!(core.enqueue("../evil", "manual").unwrap_err().0, 400);
        assert_eq!(core.enqueue("a/b", "manual").unwrap_err().0, 400);
        assert_eq!(core.enqueue("ghost", "manual").unwrap_err().0, 404);
        make_bare_repo(&root.join("repos"), "ok", &[("README.md", "x")]);
        let id = core.enqueue("ok", "manual").unwrap();
        assert!(id.starts_with('r'));
        wait_finished(&core, "ok", 1, 30_000).await;
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- push 钩子门控 ----

    #[tokio::test]
    async fn push_hook_gating() {
        let _guard = ENV_LOCK.lock().await;
        let root = temp_root("pushhook");
        let core = Arc::new(CiCore::with_paths(
            root.join("ci.db").to_str().unwrap(),
            root.join("repos").to_str().unwrap(),
            &root.join("work"),
        ));
        make_bare_repo(&root.join("repos"), "hooked", &[("README.md", "x")]);

        // 1) 缺省开 → 入队
        std::env::remove_var(ENV_AUTO_PUSH);
        push_hook_with(Some(Arc::clone(&core)), "hooked");
        assert_eq!(core.list_runs("hooked").len(), 1, "缺省应自动触发");
        // 2) =0 → 不触发
        std::env::set_var(ENV_AUTO_PUSH, "0");
        push_hook_with(Some(Arc::clone(&core)), "hooked");
        assert_eq!(core.list_runs("hooked").len(), 1, "=0 应关闭");
        // 3) =false → 不触发
        std::env::set_var(ENV_AUTO_PUSH, "false");
        push_hook_with(Some(Arc::clone(&core)), "hooked");
        assert_eq!(core.list_runs("hooked").len(), 1);
        // 4) =1 → 恢复触发
        std::env::set_var(ENV_AUTO_PUSH, "1");
        push_hook_with(Some(Arc::clone(&core)), "hooked");
        assert_eq!(core.list_runs("hooked").len(), 2, "=1 应恢复");
        // 5) 非法仓库名忽略（不 panic 不入队）
        push_hook_with(Some(Arc::clone(&core)), "../evil");
        push_hook_with(Some(Arc::clone(&core)), "");
        assert_eq!(core.list_runs("hooked").len(), 2);
        // 6) 未装配核心 → 静默
        push_hook_with(None, "hooked");
        assert_eq!(core.list_runs("hooked").len(), 2);
        std::env::remove_var(ENV_AUTO_PUSH);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- 路由声明（权限矩阵）----

    #[tokio::test]
    async fn routes_declare_auth_matrix() {
        let root = temp_root("routes");
        let h = NexhubCiRouteHandler::with_core(Arc::new(CiCore::with_paths(
            root.join("ci.db").to_str().unwrap(),
            root.join("repos").to_str().unwrap(),
            &root.join("work"),
        )));
        let routes = h.routes().await;
        assert_eq!(routes.len(), 5, "{routes:?}");
        assert!(routes.iter().all(|r| r.handler_component == "nexhub_ci"));
        for r in &routes {
            match r.method {
                HttpMethod::Post | HttpMethod::Delete => {
                    assert!(r.requires_auth, "写操作需 admin: {:?}", r.path);
                    assert_eq!(r.required_roles, vec!["admin".to_string()]);
                }
                _ => assert!(!r.requires_auth, "读公开: {:?}", r.path),
            }
        }
        // 聚合端点（段数与 repos/:name/ci 不同，注册期不冲突）
        assert!(routes
            .iter()
            .any(|r| r.method == HttpMethod::Get && r.path == "/api/v1/coderepo/ci/latest"));
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- 端到端权限：网关 dispatch 层对非 admin 写强制拒绝 ----
    //
    // 注：HTTP 入口另有「无凭据默认注入 admin」策略（NEXOS_AUTH_DEFAULT_ADMIN，
    // 用户 2026-08-26 指示，extract_principal）——本测试直接压 dispatch 层的
    // authorize 强制点（req.auth=None → 401，不入队；admin Principal → 202），
    // 与入口策略解耦、无 env 竞态。

    #[tokio::test]
    async fn write_endpoints_enforced_by_gateway_auth() {
        use os_security::{Principal, Role, User, UserId};

        let root = temp_root("gwauth");
        let core = Arc::new(CiCore::with_paths(
            root.join("ci.db").to_str().unwrap(),
            root.join("repos").to_str().unwrap(),
            &root.join("work"),
        ));
        make_bare_repo(&root.join("repos"), "gwrepo", &[("README.md", "x")]);
        let gw = crate::InProcessGateway::new();
        crate::gateway::Gateway::register_component(
            &gw,
            "nexhub_ci",
            Box::new(NexhubCiRouteHandler::with_core(Arc::clone(&core))),
        )
        .await
        .expect("注册 nexhub_ci");

        let post = |auth: Option<Principal>| ApiRequest {
            method: HttpMethod::Post,
            path: "/api/v1/coderepo/repos/gwrepo/ci".to_string(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth,
        };

        // 1) 无凭据 → 401，不入队
        let (resp, _) = gw.dispatch(post(None)).await;
        assert_eq!(resp.status, 401, "无凭据触发 CI 应 401: {resp:?}");
        assert!(core.list_runs("gwrepo").is_empty(), "401 不得入队");

        // 2) admin Principal → 202 入队
        let now = chrono::Utc::now();
        let roles = vec![Role::Admin];
        let user = User::new(UserId::new("admin".to_string()), "admin".to_string(), roles.clone(), now)
            .expect("构造 admin 用户");
        let admin = Principal::new(user, roles, now).expect("构造 admin Principal");
        let (resp, route) = gw.dispatch(post(Some(admin))).await;
        assert_eq!(resp.status, 202, "admin 触发应 202: {resp:?}");
        assert_eq!(route.map(|r| r.handler_component).as_deref(), Some("nexhub_ci"));
        assert_eq!(core.list_runs("gwrepo").len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }
}
