//! NexHub 大厅·自动跟随域（2026-09-25 大文件拆分批，纯搬运零行为变化）：
//! nexos 本地 bare 副本 auto-pull（联邦刷新 → 后台 fetch/clone，10 分钟节流防抖）
//! + 服务端 git clone（本机 10s / 跨节点 HTTP 120s 超时）
//! + 启动 ensure 链（nexos 条目补发布 + post-receive 同步钩子幂等补装）。
//!
//! LobbyFedEndpoint 的 auto-pull 两方法随域走（impl 块分置，同一类型）。

use super::*;

/// nexos 本地 bare 副本自动跟随的节流窗口：同一仓库两次触发之间最少间隔
/// 10 分钟——快照风暴（对端短时间多次 push → 多次重广播）只兑现最近一次，
/// 防抖不追帧（下一个窗口总会再同步到最新）。
pub(super) const AUTO_PULL_THROTTLE: std::time::Duration = std::time::Duration::from_secs(600);

/// nexos 本地副本自动跟随的总开关 env 名（`=0` 关闭，缺省/其他值开启）。
pub(super) const AUTO_PULL_ENV: &str = "NEXOS_LOBBY_AUTO_PULL";

/// 自动跟随是否启用（读 env [`AUTO_PULL_ENV`]，每次调度即时读取——运维
/// 改环境变量重启即生效；默认开）。
pub(super) fn auto_pull_enabled() -> bool {
    std::env::var(AUTO_PULL_ENV).as_deref() != Ok("0")
}

/// 极简日志（os-nexhub 无 tracing 依赖——eprintln 与本模块其余降级日志同款）。
pub(super) fn tracing_like_log(msg: &str) {
    eprintln!("[os-nexhub] {msg}");
}

// ----------------------------------------------------------------------------
// 服务端 git clone（async，本机 10s / 联邦 HTTP 120s 超时）
// ----------------------------------------------------------------------------

/// 本机克隆超时（秒）：本地路径 clone 与本机条目自报的 http/ssh 远端（设计
/// 文档 §5/§6 一期内置兜底）。
pub(super) const CLONE_TIMEOUT_SECS: u64 = 10;

/// 联邦 HTTP 克隆超时（秒）：消费节点经 `/git/*` Smart HTTP 从源节点跨网络
/// 拉取（大仓/慢链路），比本机 10s 宽——10s 掐死的恰恰是跨节点拉取的主路径。
pub(super) const FED_CLONE_TIMEOUT_SECS: u64 = 120;

/// spawn `git clone --bare <source> <target>`（`timeout_secs` 超时；kill_on_drop
/// 保证超时后子进程被回收；GIT_TERMINAL_PROMPT=0 防凭据交互挂起）。
pub(super) async fn spawn_git_clone_bare(
    source: &str,
    target: &str,
    timeout_secs: u64,
) -> Result<(), String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("clone")
        .arg("--bare")
        .arg(source)
        .arg(target)
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null());
    let child = cmd
        .spawn()
        .map_err(|e| format!("`git` 调用失败（未安装？）: {e}"))?;
    match tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        child.wait_with_output(),
    )
    .await
    {
        Err(_) => Err(format!(
            "git clone 超时（{timeout_secs}s），已终止: {source}"
        )),
        Ok(Err(e)) => Err(format!("git clone 等待失败: {e}")),
        Ok(Ok(out)) => {
            if out.status.success() {
                Ok(())
            } else {
                Err(format!(
                    "git clone 失败: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ))
            }
        }
    }
}

// ----------------------------------------------------------------------------
// nexos 本地 bare 副本自动跟随（联邦刷新 → 后台 fetch/clone，2026-08-27）
// ----------------------------------------------------------------------------

/// 自动跟随的拉取源解析结果：URL + 配套超时（本机路径 10s / 跨节点 HTTP 120s，
/// 与一键克隆 [`NexHubLobbyRouteHandler::clone_entry_async`] 同档）。
pub(super) struct AutoPullSource {
    pub(super) url: String,
    pub(super) timeout_secs: u64,
}

/// 从快照解析副本跟随的拉取源——**只用快照自带信号**（与 `select_clone_source`
/// 同构，但从不 fallback 空串）：
///
/// - `source_url` 非空且本机存在该路径 → 本地直拉（同布局跨节点 / 源节点自身
///   场景），10s 超时；
/// - 否则 `clone_url_http` 非空 → 联邦 HTTP 拉（消费节点主路径），120s 超时；
/// - 两者皆无 → None（调度端直接跳过，不 spawn）。
pub(super) fn resolve_auto_pull_source(entry: &LobbyEntry) -> Option<AutoPullSource> {
    if !entry.source_url.is_empty() && Path::new(&entry.source_url).exists() {
        return Some(AutoPullSource {
            url: entry.source_url.clone(),
            timeout_secs: CLONE_TIMEOUT_SECS,
        });
    }
    let http = entry.clone_url_http.trim();
    if !http.is_empty() {
        return Some(AutoPullSource {
            url: http.to_string(),
            timeout_secs: FED_CLONE_TIMEOUT_SECS,
        });
    }
    None
}

/// 副本跟随的后台执行结果（[`run_auto_pull_job`] 产物，测试/日志观测面）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AutoPullOutcome {
    /// 本地 HEAD 已等于快照 short_hash → 跳过 fetch（省流量）。
    HeadMatchSkipped,
    /// 对既有 bare 副本 `git fetch --prune` 更新分支引用。
    Fetched,
    /// 本地无副本 → 完整 `git clone --bare` 落地。
    Cloned,
}

/// 运行时无关的后台任务投递：tokio 上下文内走 `Handle::spawn`；无上下文
/// （同步 ingest 调用方/单测线程）兜底起独立线程自建一次性 runtime 执行。
pub(super) fn spawn_detached_future<F>(job: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(job);
        }
        Err(_) => {
            std::thread::spawn(move || {
                if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    rt.block_on(job);
                }
            });
        }
    }
}

/// 通用 spawn `git <args>`（timeout_secs 超时 kill 兜底、GIT_TERMINAL_PROMPT=0
/// 防凭据挂起），成功返回 trim 后 stdout。与 [`spawn_git_clone_bare`] 同款
/// 子进程纪律（kill_on_drop + 全管道 + stdin null），供 fetch / rev-parse 复用。
pub(super) async fn spawn_git_run(args: &[&str], timeout_secs: u64) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        // HTTP 拉取低速熔断：30s 内 <1KiB/s 视为链路僵死提前退出（比整段
        // 超时更快止损；本地路径/file:// 不受影响）。
        .env("GIT_HTTP_LOW_SPEED_LIMIT", "1024")
        .env("GIT_HTTP_LOW_SPEED_TIME", "30")
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null());
    let child = cmd
        .spawn()
        .map_err(|e| format!("`git` 调用失败（未安装？）: {e}"))?;
    match tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        child.wait_with_output(),
    )
    .await
    {
        Err(_) => Err(format!("git {args:?} 超时（{timeout_secs}s），已终止")),
        Ok(Err(e)) => Err(format!("git {args:?} 等待失败: {e}")),
        Ok(Ok(out)) => {
            if out.status.success() {
                Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
            } else {
                Err(format!(
                    "git {args:?} 失败: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ))
            }
        }
    }
}

/// bare 副本 HEAD 短 hash（7 位，与快照 latest_commit.short_hash 同宽）；
/// 空仓库/非法仓 → None（调用方放弃判等直接 fetch，幂等无害）。
pub(super) async fn bare_head_short_hash(repo_dir: &str) -> Option<String> {
    spawn_git_run(&["-C", repo_dir, "rev-parse", "--short=7", "HEAD"], 10)
        .await
        .ok()
}

/// nexos 副本跟随后台任务（由 [`LobbyFedEndpoint::schedule_nexos_auto_pull`
/// ] 经 [`spawn_detached_future`] 投递；DB 锁已释放，可自由做文件/git 操作）：
///
/// 1. 目标 `<repos_root>/nexos.git` **已存在** → 先比对本地 HEAD 与快照
///    `latest_commit.short_hash`，相同即跳过（省流量）；不同则对副本执行
///    `git -C <dir> fetch <source> "+refs/heads/*:refs/heads/*"
///    "+refs/tags/*:refs/tags/*" --prune` ——bare 仓可直接 fetch 推进分支引用
///    （HEAD 所指分支随引用更新）；**tag refspec 为显式强制镜像**（2026-09-03
///    补：发版即 tag，heads-only refspec 下 tag 只能靠 git 的机会主义
///    auto-follow——不保证覆盖旧对象上的 tag、绝不更新已存在/被强推的 tag，
///    下游副本 refs/tags 可能永远为空 → 更新检查读 tag 失明。显式
///    `+refs/tags/*` 保证 tag 必达、`-f` 强推形态（release.sh `tag -fa` +
///    `push -f`）被强制对齐、源侧删 tag 时随 `--prune` 镜像清理）。
/// 2. 目标不存在 → 走完整 `git clone --bare`（首次联邦收件即落副本）。
///
/// 失败仅静默日志（下个快照再试），绝不 panic/影响 ingest 返回值。
pub(super) async fn run_auto_pull_job(
    repos_root: String,
    entry: LobbyEntry,
    source: AutoPullSource,
) {
    let outcome = run_auto_pull_inner(&repos_root, &entry, &source).await;
    match outcome {
        Ok(AutoPullOutcome::HeadMatchSkipped) => {
            tracing_like_log("nexhub-fed: nexos 副本已是快照提交（HEAD 判等命中），跳过 fetch")
        }
        Ok(AutoPullOutcome::Fetched) => {
            tracing_like_log("nexhub-fed: nexos 副本 fetch 完成（分支引用已推进）")
        }
        Ok(AutoPullOutcome::Cloned) => {
            tracing_like_log("nexhub-fed: nexos 副本不存在 → 完整 clone 落地")
        }
        Err(e) => tracing_like_log(&format!(
            "nexhub-fed: nexos 副本跟随失败（静默，待下个快照重试）: {e}"
        )),
    }
}

/// [`run_auto_pull_job`] 的实质逻辑（独立纯化便于测试直调拿回结果）。
pub(super) async fn run_auto_pull_inner(
    repos_root: &str,
    entry: &LobbyEntry,
    source: &AutoPullSource,
) -> Result<AutoPullOutcome, String> {
    let target = format!("{repos_root}/{}.git", entry.repo_name);
    if Path::new(&target).exists() {
        // hash 判等省流：本地 HEAD == 快照 short_hash → 无需 fetch
        if let Some(want) = entry.latest_commit.as_ref().map(|c| c.short_hash.as_str()) {
            if let Some(local) = bare_head_short_hash(&target).await {
                if local == want {
                    return Ok(AutoPullOutcome::HeadMatchSkipped);
                }
            }
        }
        spawn_git_run(
            &[
                "-C",
                &target,
                "fetch",
                &source.url,
                "+refs/heads/*:refs/heads/*",
                // tag 显式镜像（发版即 tag：显式 refspec 保证必达 + 强推对齐；
                // 见 fn 文档注释。auto-follow 机会主义语义不可依赖）
                "+refs/tags/*:refs/tags/*",
                "--prune",
            ],
            source.timeout_secs,
        )
        .await?;
        Ok(AutoPullOutcome::Fetched)
    } else {
        std::fs::create_dir_all(repos_root)
            .map_err(|e| format!("创建仓库根目录 {repos_root} 失败: {e}"))?;
        spawn_git_clone_bare(&source.url, &target, source.timeout_secs).await?;
        Ok(AutoPullOutcome::Cloned)
    }
}

// ----------------------------------------------------------------------------
// 内部辅助
// ----------------------------------------------------------------------------

/// nexos 常驻（设计文档 §5 + 2026-08-23 自动联邦）：`nexos` 主仓库**默认常驻
/// 大厅**——启动（建库路径）**无条件确保**已发布，不再「表空才 seed」：
///
/// - 条目不存在 → 自动发布为大厅第一条（publisher: NexOS，description 用仓库
///   description，空则回退占位文案）——下架后重启会回来；
/// - 条目已存在 → 刷新快照（等价重复 publish：`INSERT OR REPLACE` 语义，复用
///   [`snapshot_repo_blocking`] 重统计 commit 数/大小/last_commit/README 摘要，
///   保留 `download_count`）——推送新代码后快照不过期；
/// - **自动联邦**：常驻条目直接置 `federated=true`——nexos 一启动就在联邦
///   大厅，无需手动 `POST /:name/federate`。返回写入的条目（构造方/通道注入方
///   据此 `broadcast_entry`；P2P 未装配时广播静默跳过，标志仍置位）。
/// - **自动同步钩子**（2026-08-25，设计文档 §15）：常驻同时补装 nexos.git 的
///   post-receive 钩子——此后 106 等节点 `git push` 新提交即自动触发 publish
///   （刷新 latest_commit/pushed_at 快照）+ federate（重广播），本地与联邦大厅
///   条目不再停留在启动时的旧快照（幂等补装，见 [`crate::lobby_sync_hook`]）。
///
/// 跳过条件：`<repos_root>/nexos.git` 不存在（无从快照）；或 env
/// [`ENV_NO_AUTO_PUBLISH`] 置 `1`（逃生口：发布与联邦一并跳过）。幂等可重入：
/// 重复调用仍只此一条。
pub(super) fn ensure_nexos_published(
    conn: &Connection,
    repos_root: &str,
) -> rusqlite::Result<Option<LobbyEntry>> {
    if auto_publish_disabled() {
        return Ok(None);
    }
    let bare = format!("{repos_root}/{SEED_REPO}.git");
    if !Path::new(&bare).is_dir() {
        return Ok(None);
    }
    let snap = snapshot_repo_blocking(repos_root, SEED_REPO);
    let mut entry = LobbyEntry {
        repo_name: SEED_REPO.to_string(),
        description: if snap.description.is_empty() {
            "NexOS 主仓库（本地节点）".to_string()
        } else {
            snap.description
        },
        tags: vec!["nexos".to_string(), "official".to_string()],
        publisher: SEED_PUBLISHER.to_string(),
        source_url: bare,
        homepage_node: default_homepage_node(),
        source_node: default_source_node(),
        // 常驻即定格本节点 HTTP 克隆地址（每次启动刷新——advertise_host 变化
        // /端口调整后重启即广播新地址）；联邦消费节点一键克隆经此拉取。
        clone_url_http: build_clone_url_http(SEED_REPO),
        commit_count: snap.commit_count,
        size_bytes: snap.size_bytes,
        default_branch: snap.default_branch,
        last_commit: snap.last_commit,
        last_commit_date: snap.last_commit_date,
        readme_excerpt: snap.readme_excerpt,
        download_count: 0,
        published_at: now_iso(),
        price_sats: 0,
        currency: "free".to_string(),
        // 自动联邦：常驻即推送（无需手动 federate）——广播由构造方在 handler
        // 组装完成后执行（open_db 期 LobbyFedEndpoint 尚未建好）。
        federated: true,
        // 自动同步链快照增量：结构化最新提交 + 本次刷新时间（每次启动刷新）。
        latest_commit: snap.latest_commit,
        pushed_at: now_iso(),
    };
    // 等价重复 publish：INSERT OR REPLACE 刷新快照，保留既有 download_count
    entry.download_count = find_entry(conn, SEED_REPO)?.map_or(0, |old| old.download_count);
    insert_entry(conn, &entry)?;
    // 顺手补装 post-receive 自动同步钩子（设计文档 §15）：git push nexos.git →
    // 钩子后台 curl 本地 publish（刷新快照）+ federate（重广播）——任何部署形态
    // （systemd/docker/手动）启动即自动获得「推送即同步大厅」能力，无需人工装
    // 钩子。幂等：缺失/生成内容漂移才写（用户自管钩子带自定义内容则不动）；
    // 失败仅记日志不阻塞启动（降级 = 自动同步退化为启动时刷新）。
    ensure_nexos_sync_hook(repos_root);
    Ok(Some(entry))
}

/// nexos 裸仓库补装 post-receive 自动同步钩子（[`ensure_nexos_published`] 尾步，
/// 独立函数便于日志聚焦）。API 地址/token 从 env 推导（见
/// [`crate::lobby_sync_hook`]），repos_root 即 `NEXOS_GIT_REPOS_DIR` 注入值。
pub(super) fn ensure_nexos_sync_hook(repos_root: &str) {
    match crate::lobby_sync_hook::ensure_post_receive_hook(
        repos_root,
        SEED_REPO,
        &crate::lobby_sync_hook::lobby_sync_api_base(),
        &crate::lobby_sync_hook::lobby_sync_admin_token(),
    ) {
        Ok(true) => tracing_like_log(&format!(
            "nexhub-lobby: 已补装 {SEED_REPO} post-receive 自动同步钩子（push → publish+federate）"
        )),
        Ok(false) => {}
        Err(e) => tracing_like_log(&format!(
            "nexhub-lobby: 补装 {SEED_REPO} post-receive 钩子失败（不影响启动）: {e}"
        )),
    }
}

/// 常驻开关（env 逃生口）：[`ENV_NO_AUTO_PUBLISH`] 显式为 `1` → 禁用 nexos
/// 自动常驻（发布与刷新均跳过）；未设置或其余值 → 启用。
pub(super) fn auto_publish_disabled() -> bool {
    std::env::var(ENV_NO_AUTO_PUBLISH).is_ok_and(|v| v.trim() == "1")
}

impl LobbyFedEndpoint {
    /// nexos 本地 bare 副本自动跟随（2026-08-27，链路最后一环）：
    ///
    /// 源节点 push → post-receive → 重 publish + federate 广播新快照；消费端
    /// [`Self::ingest`] 落库后大厅条目已是最新，但**本地 `/…/git-repos/nexos.git`
    /// bare 副本仍停留旧提交**——用户从本节点 NexHub clone 到的是旧代码。本方法
    /// 在 Written/Refreshed 落地成功后由 ingest 触发：
    ///
    /// - **仅内置主仓** [`SEED_REPO`]（其他联邦仓不跟随——用户未要求全量同步）；
    /// - 拉取源**只用快照自带信号**（与一键克隆同构：source_url 本机存在走
    ///   本地直拉 10s；否则 clone_url_http 跨节点 HTTP 拉 120s；皆无 → 静默跳过，
    ///   不 spawn 无谓线程）；
    /// - **节流**：同仓库 [`AUTO_PULL_THROTTLE`]（10 分钟）内最多触发一次；
    /// - 真正的 git 操作全部投递到**无锁后台任务**（ingest 持 DB 锁期间零阻塞、
    ///   失败静默日志不影响返回值，下个快照再试）；
    /// - 总开关 env [`AUTO_PULL_ENV`]`=0` 关闭（默认开）。
    ///
    /// 后台任务内部再做两级省流判定（见 [`run_auto_pull_job`]）。
    pub(super) fn schedule_nexos_auto_pull(&self, entry: &LobbyEntry) {
        if !auto_pull_enabled() {
            return;
        }
        if entry.repo_name != SEED_REPO {
            return; // 只跟内置主仓，其他联邦仓保持手动
        }
        // 解析拉取源（只用快照信号；解析不出 → 不占用节流窗口直接跳过）
        let Some(source) = resolve_auto_pull_source(entry) else {
            tracing_like_log("nexhub-fed: nexos 快照无可达拉取源（source_url 不在本机且无 clone_url_http），跳过副本跟随");
            return;
        };
        let now = std::time::Instant::now();
        if !self.try_acquire_auto_pull_slot(&entry.repo_name, now) {
            tracing_like_log(&format!(
                "nexhub-fed: nexos 副本跟随节流中（{}s 内已触发过），本次跳过",
                AUTO_PULL_THROTTLE.as_secs()
            ));
            return;
        }
        tracing_like_log(&format!(
            "nexhub-fed: 触发 nexos 本地副本跟随拉取（目标 {}/{}.git ← {}）",
            self.repos_root, entry.repo_name, source.url
        ));
        spawn_detached_future(run_auto_pull_job(
            self.repos_root.clone(),
            entry.clone(),
            source,
        ));
    }

    /// 节流窗口占位（登记即占坑）：同仓库距上次触发不足 [`AUTO_PULL_THROTTLE`]
    /// → false（不触发）；否则登记 now 并放行。纯内存判定，供单元测试注入
    /// 人造时钟验证边界。
    pub(super) fn try_acquire_auto_pull_slot(&self, repo: &str, now: std::time::Instant) -> bool {
        let mut map = self.auto_pull_last.lock().expect("auto-pull slot poisoned");
        match map.get(repo) {
            Some(t) if now.duration_since(*t) < AUTO_PULL_THROTTLE => false,
            _ => {
                map.insert(repo.to_string(), now);
                true
            }
        }
    }
}
