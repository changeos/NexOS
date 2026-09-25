//! NexHub 大厅·发布域（2026-09-25 大文件拆分批，纯搬运零行为变化）：
//! 联邦广播载荷（FED_KIND_* / sanitize_fed_node / build_*_fed_payload）
//! + PR 审核流数据层（hub_pull_requests CRUD + 分支/标签名校验）
//! + 发版数据层（hub_releases CRUD）
//! + 裸仓合并执行（merge-tree 三策略 merge/squash/rebase + tag 落库）。
//!
//! 对外面经 lobby/mod.rs 重导出（crate::nexhub_lobby::* 路径零变化）。

use super::*;

/// 联邦载荷类型标记（`payload.fed == "nexhub_lobby"`）。
pub const FED_KIND_NEXHUB_LOBBY: &str = "nexhub_lobby";

/// 联邦载荷类型标记（`payload.fed == "nexhub_release"`，2026-08-23 发版广播）。
pub const FED_KIND_NEXHUB_RELEASE: &str = "nexhub_release";

/// 联邦节点名净化：空/超长（>64 字符）回退 `"peer"`——payload 的 `node` 字段
/// 来自对端自报，写库前限幅防病态值。
#[must_use]
pub fn sanitize_fed_node(node: &str) -> String {
    let n = node.trim();
    if n.is_empty() || n.chars().count() > 64 {
        "peer".to_string()
    } else {
        n.to_string()
    }
}

/// 构造 NexHub 联邦广播载荷（纯函数，发送端与测试共用）：
/// `{"fed":"nexhub_lobby","node":<发布节点>,"entry":{...完整 LobbyEntry JSON...}}`。
#[must_use]
pub fn build_nexhub_lobby_fed_payload(node: &str, entry: &LobbyEntry) -> serde_json::Value {
    serde_json::json!({
        "fed": FED_KIND_NEXHUB_LOBBY,
        "node": sanitize_fed_node(node),
        "entry": entry,
    })
}

/// 构造发版联邦广播载荷（纯函数，发送端与测试共用）：
/// `{"fed":"nexhub_release","node":<发版节点>,"release":{...完整 Release JSON...}}`。
#[must_use]
pub fn build_nexhub_release_fed_payload(node: &str, release: &Release) -> serde_json::Value {
    serde_json::json!({
        "fed": FED_KIND_NEXHUB_RELEASE,
        "node": sanitize_fed_node(node),
        "release": release,
    })
}

/// 合法 PR 状态集合。
pub(super) const PR_STATUSES: &[&str] = &["open", "merged", "rejected", "closed"];

/// 单条 PR（hub_pull_requests 行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequest {
    /// PR id（服务端生成，`pr-<纳秒 hex>`）。
    pub id: String,
    /// 目标仓库名（裸仓 `<repo>.git`）。
    pub repo_name: String,
    /// 标题。
    pub title: String,
    /// 描述（可选）。
    #[serde(default)]
    pub description: String,
    /// 提交者分支名（须已 push 到裸仓）。
    pub source_branch: String,
    /// 提交者节点：本机 PR 恒 `"local"`；联邦 PR（后续期）= 来源节点名。
    #[serde(default = "default_source_node")]
    pub source_node: String,
    /// 提交者链上身份（pubkey；admin 代建为 `"admin"`）。
    pub author_pubkey: String,
    /// EVM 展示名（0x…40hex；admin 代建为 `"admin"`）。
    #[serde(default)]
    pub author_display: String,
    /// 状态：open / merged / rejected / closed。
    #[serde(default = "default_pr_status")]
    pub status: String,
    /// 目标分支（创建时定格为仓库实际默认分支，main→master 回退同快照逻辑）。
    #[serde(default = "default_pr_base")]
    pub base_branch: String,
    /// 审核者（merge/reject 执行者 pubkey/admin；未审核为空）。
    #[serde(default)]
    pub reviewed_by: String,
    /// 审核时间（未审核为空）。
    #[serde(default)]
    pub reviewed_at: String,
    /// 创建时间（RFC3339）。
    pub created_at: String,
    /// 更新时间（RFC3339）。
    pub updated_at: String,
}

/// PR 默认状态（open）。
pub(super) fn default_pr_status() -> String {
    "open".to_string()
}

/// PR 默认目标分支（main；创建时按仓库实际默认分支覆盖）。
pub(super) fn default_pr_base() -> String {
    "main".to_string()
}

/// 生成 PR id（时间戳纳秒 hex，足够唯一；前缀 `pr-` 契约）。
pub(super) fn new_pr_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("pr-{nanos:x}")
}

/// 分支名校验（防 git 参数注入）：非空、不以 `-` 开头、无空白、不含 `..`、
/// 不含 ref 非法字符（`~^:?*[\`）。
/// （pub(crate)：issues.rs 的 PR 分支名校验复用同一套规则。）
pub(crate) fn validate_branch_name(branch: &str) -> Result<(), String> {
    let b = branch.trim();
    if b.is_empty() {
        return Err("分支名不可为空".into());
    }
    if b.starts_with('-') {
        return Err("分支名不可以 '-' 开头".into());
    }
    if b != branch || b.chars().any(|c| c.is_whitespace()) {
        return Err("分支名不可包含空白".into());
    }
    if b.contains("..") || b.contains(['~', '^', ':', '?', '*', '[', '\\']) {
        return Err(format!("分支名含非法字符: {b}"));
    }
    Ok(())
}

/// tag 名校验（同分支名校验 + 不可 `.` 开头 / `.lock` 结尾——git ref 规则）。
/// （pub(crate)：issues.rs 的 release assets 端点复用同一校验。）
pub(crate) fn validate_tag_name(tag: &str) -> Result<(), String> {
    validate_branch_name(tag)?;
    let t = tag.trim();
    if t.starts_with('.') || t.starts_with('/') {
        return Err("tag 名不可以 '.' 或 '/' 开头".into());
    }
    if t.ends_with('/') || t.ends_with(".lock") {
        return Err("tag 名不可以 '/' 或 '.lock' 结尾".into());
    }
    if t.len() > 128 {
        return Err("tag 名过长（≤128 字符）".into());
    }
    Ok(())
}

/// 列字段序（hub_pull_requests INSERT/SELECT 共用）。
pub(super) const PR_COLUMNS: &str = "id,repo_name,title,description,source_branch,source_node,\
     author_pubkey,author_display,status,base_branch,reviewed_by,reviewed_at,\
     created_at,updated_at";

pub(super) fn insert_pr(conn: &Connection, p: &PullRequest) -> rusqlite::Result<()> {
    conn.execute(
        &format!(
            "INSERT OR REPLACE INTO hub_pull_requests ({PR_COLUMNS}) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)"
        ),
        params![
            p.id,
            p.repo_name,
            p.title,
            p.description,
            p.source_branch,
            p.source_node,
            p.author_pubkey,
            p.author_display,
            p.status,
            p.base_branch,
            p.reviewed_by,
            p.reviewed_at,
            p.created_at,
            p.updated_at,
        ],
    )?;
    Ok(())
}

pub(super) fn pr_from_row(row: &rusqlite::Row) -> rusqlite::Result<PullRequest> {
    Ok(PullRequest {
        id: row.get(0)?,
        repo_name: row.get(1)?,
        title: row.get(2)?,
        description: row.get(3)?,
        source_branch: row.get(4)?,
        source_node: row
            .get::<_, Option<String>>(5)?
            .unwrap_or_else(default_source_node),
        author_pubkey: row.get(6)?,
        author_display: row.get::<_, Option<String>>(7)?.unwrap_or_default(),
        status: row
            .get::<_, Option<String>>(8)?
            .unwrap_or_else(default_pr_status),
        base_branch: row
            .get::<_, Option<String>>(9)?
            .unwrap_or_else(default_pr_base),
        reviewed_by: row.get(10)?,
        reviewed_at: row.get(11)?,
        created_at: row.get::<_, Option<String>>(12)?.unwrap_or_default(),
        updated_at: row.get::<_, Option<String>>(13)?.unwrap_or_default(),
    })
}

/// 查询单条 PR（按 id + repo 双重定位——PR id 全局唯一，repo 是路由冗余校验）。
pub(super) fn find_pr(
    conn: &Connection,
    repo: &str,
    id: &str,
) -> rusqlite::Result<Option<PullRequest>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {PR_COLUMNS} FROM hub_pull_requests WHERE id=? AND repo_name=?"
    ))?;
    stmt.query_row(params![id, repo], pr_from_row).optional()
}

/// PR 列表：`repo` 维度 + `status` 可选过滤（须为合法状态），创建时间降序。
pub(super) fn load_prs(
    conn: &Connection,
    repo: &str,
    status: Option<&str>,
) -> rusqlite::Result<Vec<PullRequest>> {
    let mut sql = format!("SELECT {PR_COLUMNS} FROM hub_pull_requests WHERE repo_name=?");
    let mut bind: Vec<String> = vec![repo.to_string()];
    if let Some(s) = status {
        sql.push_str(" AND status=?");
        bind.push(s.to_string());
    }
    sql.push_str(" ORDER BY created_at DESC");
    let mut stmt = conn.prepare(&sql)?;
    let iter = stmt.query_map(params_from_iter(bind.iter()), pr_from_row)?;
    let mut out = Vec::new();
    for p in iter {
        out.push(p?);
    }
    Ok(out)
}

// ----------------------------------------------------------------------------
// 发版（release）持久化层（2026-08-23 定稿：git tag + SQLite hub_releases）
// ----------------------------------------------------------------------------

/// 单条 release（hub_releases 行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    /// release id（服务端生成，`rel-<纳秒 hex>`；联邦落地保留原 id）。
    pub id: String,
    /// 仓库名。
    pub repo_name: String,
    /// git tag 名（创建时已 `git tag` 到仓库默认分支头）。
    pub tag: String,
    /// 标题。
    #[serde(default)]
    pub title: String,
    /// 发版说明。
    #[serde(default)]
    pub notes: String,
    /// 发版人（恒 `"admin"`——发版是平台级权限；联邦落地保留原值）。
    #[serde(default)]
    pub created_by: String,
    /// 发版时间（RFC3339）。
    pub created_at: String,
}

/// 生成 release id（时间戳纳秒 hex）。
pub(super) fn new_release_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("rel-{nanos:x}")
}

/// 列字段序（hub_releases INSERT/SELECT 共用）。
pub(super) const RELEASE_COLUMNS: &str = "id,repo_name,tag,title,notes,created_by,created_at";

pub(super) fn insert_release(conn: &Connection, r: &Release) -> rusqlite::Result<()> {
    conn.execute(
        &format!(
            "INSERT OR REPLACE INTO hub_releases ({RELEASE_COLUMNS}) \
             VALUES (?,?,?,?,?,?,?)"
        ),
        params![
            r.id,
            r.repo_name,
            r.tag,
            r.title,
            r.notes,
            r.created_by,
            r.created_at,
        ],
    )?;
    Ok(())
}

pub(super) fn release_from_row(row: &rusqlite::Row) -> rusqlite::Result<Release> {
    Ok(Release {
        id: row.get(0)?,
        repo_name: row.get(1)?,
        tag: row.get(2)?,
        title: row.get(3)?,
        notes: row.get(4)?,
        created_by: row.get(5)?,
        created_at: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
    })
}

/// 查询某仓库某 tag 的 release（唯一性键 repo+tag）。
pub(super) fn find_release(
    conn: &Connection,
    repo: &str,
    tag: &str,
) -> rusqlite::Result<Option<Release>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {RELEASE_COLUMNS} FROM hub_releases WHERE repo_name=? AND tag=?"
    ))?;
    stmt.query_row(params![repo, tag], release_from_row)
        .optional()
}

/// release 列表（按仓库，发版时间降序）。
pub(super) fn list_releases(conn: &Connection, repo: &str) -> rusqlite::Result<Vec<Release>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {RELEASE_COLUMNS} FROM hub_releases WHERE repo_name=? ORDER BY created_at DESC"
    ))?;
    let iter = stmt.query_map(params![repo], release_from_row)?;
    let mut out = Vec::new();
    for r in iter {
        out.push(r?);
    }
    Ok(out)
}

/// 删除 release 行（repo+tag 定位），返回影响行数。
pub(super) fn delete_release(conn: &Connection, repo: &str, tag: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM hub_releases WHERE repo_name=? AND tag=?",
        params![repo, tag],
    )
}

// ----------------------------------------------------------------------------
// PR / release 的 git 操作（blocking，spawn_blocking 内执行）
// ----------------------------------------------------------------------------

/// 分支是否真实存在（全 ref 形式杜绝选项注入；同 code_repo::branch_exists_sync）。
pub(super) fn pr_branch_exists(bare: &str, branch: &str) -> bool {
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

/// PR diff 摘要：`git diff <base>..<source> --stat`（spec 契约；全 ref 形式防注入）。
/// 失败（分支被删/仓库移除）降级空串——详情仍可看，不 500。
/// （pub(crate)：issues.rs 的项目级 PR 详情摘要复用同一实现。）
pub(crate) fn pr_diff_stat_blocking(bare: &str, base: &str, source: &str) -> String {
    let (ok, out) = run_git_sync(
        bare,
        &[
            "diff",
            &format!("refs/heads/{base}..refs/heads/{source}"),
            "--stat",
        ],
    );
    if ok {
        out.trim_end().to_string()
    } else {
        String::new()
    }
}

/// PR 合并策略（issues.rs 项目级 PR 的 `merge_strategy` 入参，2026-09-24 方案
/// §top4；大厅 PR 恒为 [`MergeStrategy::Merge`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MergeStrategy {
    /// 缺省：3-way 合并 + 双 parent 合并提交（GitHub merge commit）。
    Merge,
    /// 压单提交：merge-tree 合成树 + 单 parent commit-tree（GitHub squash）。
    Squash,
    /// 线性变基：可快进则快进；否则逐 commit 变基重放（GitHub rebase）。
    Rebase,
}

impl MergeStrategy {
    /// 解析策略名（大小写不敏感；空/缺省 → Merge）。未知值 → None（调用方 400）。
    pub(crate) fn parse(raw: Option<&str>) -> Option<Self> {
        let s = raw.unwrap_or_default().trim().to_ascii_lowercase();
        match s.as_str() {
            "" | "merge" => Some(MergeStrategy::Merge),
            "squash" => Some(MergeStrategy::Squash),
            "rebase" => Some(MergeStrategy::Rebase),
            _ => None,
        }
    }

    /// 策略名（响应回显 / 日志）。
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            MergeStrategy::Merge => "merge",
            MergeStrategy::Squash => "squash",
            MergeStrategy::Rebase => "rebase",
        }
    }
}

/// 裸仓合并 PR（blocking）：`merge-tree --write-tree`（git ≥2.38，无工作区 3-way
/// 合并）→ `commit-tree`（双 parent 合并提交，内置身份不依赖全局配置）→
/// `update-ref` 推进 base 分支。冲突（merge-tree 退出码 1）返回 `Err`（调用方
/// 转 409）。成功返回新 base 分支头 sha。
/// （pub(crate)：issues.rs 的项目级 PR merge 复用同一实现——两处 PR 语义不同的
/// 是状态机与权限，合并的 git 执行完全同源，不复制代码。）
pub(crate) fn merge_pr_blocking(
    bare: &str,
    base: &str,
    source: &str,
    message: &str,
) -> Result<String, String> {
    merge_with_strategy_blocking(bare, base, source, message, &MergeStrategy::Merge)
}

/// 分策略合并（blocking，2026-09-24 方案 §top4——merge/squash/rebase 单点分叉）：
///
/// - **merge**：现行为（merge-tree 3-way + commit-tree 双 parent + update-ref）；
/// - **squash**：merge-tree 合成树 + commit-tree **单 parent**（`message` 缺省由
///   调用方传 PR 标题+#编号）+ update-ref——来源分支的多个提交压成一个；
/// - **rebase**：`merge-base == base` 时直接快进（update-ref base ← source 头，
///   原 sha 全保留）；否则把 `merge-base..source` 的提交**逐个**用
///   `merge-tree --write-tree --merge-base=<原 parent>` 变基重放到 base 之上
///   （作者名/邮箱/日期与提交信息原样保留，committer=NexHub），全程线性单 parent。
///
/// 任一步冲突返回 `Err("合并冲突…")`（调用方统一转 409）。
pub(crate) fn merge_with_strategy_blocking(
    bare: &str,
    base: &str,
    source: &str,
    message: &str,
    strategy: &MergeStrategy,
) -> Result<String, String> {
    if *strategy == MergeStrategy::Merge {
        return merge_commit_blocking(bare, base, source, message);
    }
    let base_ref = format!("refs/heads/{base}");
    let src_ref = format!("refs/heads/{source}");
    let (bok, bout) = run_git_sync(bare, &["rev-parse", &base_ref]);
    if !bok {
        return Err(format!("目标分支不存在: {base}"));
    }
    let base_sha = bout.trim().to_string();
    let (sok, sout) = run_git_sync(bare, &["rev-parse", &src_ref]);
    if !sok {
        return Err(format!("来源分支不存在: {source}"));
    }
    let src_sha = sout.trim().to_string();

    if *strategy == MergeStrategy::Squash {
        // 1. 3-way 合成树（复用 merge 的合成语义——冲突同报）
        let tree = merge_tree_blocking(bare, &base_ref, &src_ref)?;
        // 2. 单 parent 提交（squash：来源提交历史不入 base，仅内容压成一个提交）
        let commit = commit_tree_blocking(bare, &tree, &[&base_sha], message, &[])?;
        // 3. 推进 base 分支
        update_ref_blocking(bare, &base_ref, &commit)?;
        return Ok(commit);
    }

    // —— rebase ——
    // 1. merge-base：相等即可快进（线性保留原 sha）；无共同祖先不可变基
    let (mok, mout) = run_git_sync(bare, &["merge-base", &base_ref, &src_ref]);
    if !mok {
        return Err(format!("无法变基：{base} 与 {source} 无共同祖先"));
    }
    let merge_base = mout.trim().to_string();
    if merge_base == base_sha {
        let (uok, _) = run_git_sync(bare, &["update-ref", &base_ref, &src_sha]);
        if !uok {
            return Err(format!("git update-ref {base} 失败"));
        }
        return Ok(src_sha);
    }
    // 2. 逐 commit 变基重放（reverse = 旧→新顺序；作者信息原样保留）
    let (rok, rout) = run_git_sync(
        bare,
        &["rev-list", "--reverse", &format!("{merge_base}..{src_ref}")],
    );
    if !rok {
        return Err(format!("git rev-list {source} 失败"));
    }
    let commits: Vec<String> = rout
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    if commits.is_empty() {
        // merge-base..source 为空却非快进（source 已含 base）——退化为快进
        let (uok, _) = run_git_sync(bare, &["update-ref", &base_ref, &src_sha]);
        if !uok {
            return Err(format!("git update-ref {base} 失败"));
        }
        return Ok(src_sha);
    }
    let mut tip = base_sha;
    for c in &commits {
        // 该提交的原 parent（作为 3-way 的 merge-base）
        let (pok, pout) = run_git_sync(bare, &["rev-parse", &format!("{c}^")]);
        let parent = if pok {
            pout.trim().to_string()
        } else {
            String::new()
        };
        // 变基该提交：merge-tree(tip × c, base=c 的原 parent) → 单 parent commit-tree
        let tree = if parent.is_empty() {
            merge_tree_with_base_blocking(bare, &tip, c, "")?
        } else {
            merge_tree_with_base_blocking(bare, &tip, c, &parent)?
        };
        let (author, msg) = commit_meta_blocking(bare, c);
        let commit = commit_tree_blocking(bare, &tree, &[&tip], &msg, &author)?;
        tip = commit;
    }
    // 3. 推进 base 分支到重放后的头
    let (uok, _) = run_git_sync(bare, &["update-ref", &base_ref, &tip]);
    if !uok {
        return Err(format!("git update-ref {base} 失败"));
    }
    Ok(tip)
}

/// 原 merge 行为（3-way + 双 parent）：`merge_pr_blocking` 的实现体。
pub(super) fn merge_commit_blocking(
    bare: &str,
    base: &str,
    source: &str,
    message: &str,
) -> Result<String, String> {
    let base_ref = format!("refs/heads/{base}");
    let src_ref = format!("refs/heads/{source}");
    // 1. 双方 sha（commit-tree 的 parent 须完整 sha）
    let (bok, bout) = run_git_sync(bare, &["rev-parse", &base_ref]);
    if !bok {
        return Err(format!("目标分支不存在: {base}"));
    }
    let base_sha = bout.trim().to_string();
    let (sok, sout) = run_git_sync(bare, &["rev-parse", &src_ref]);
    if !sok {
        return Err(format!("来源分支不存在: {source}"));
    }
    let src_sha = sout.trim().to_string();
    // 2. 3-way 合成树（冲突 → git 退出码 1，输出含冲突清单）
    let tree = merge_tree_blocking(bare, &base_ref, &src_ref)?;
    // 3. 合并提交（双 parent；identical parent 时 git 自动去重）
    let commit = commit_tree_blocking(bare, &tree, &[&base_sha, &src_sha], message, &[])?;
    // 4. 推进 base 分支（原子 ref 更新）
    update_ref_blocking(bare, &base_ref, &commit)?;
    Ok(commit)
}

/// `merge-tree --write-tree <a> <b>`（无显式 merge-base = git 自算共同祖先；
/// 冲突退出码 1 → `Err("合并冲突…")`，与既有错误前缀约定一致）。返回首行树 sha。
pub(super) fn merge_tree_blocking(bare: &str, a: &str, b: &str) -> Result<String, String> {
    let identity = || vec!["-c", "user.name=NexHub", "-c", "user.email=nexhub@local"];
    let mut cmd: Vec<String> = vec!["git".into(), format!("--git-dir={bare}")];
    cmd.extend(identity().into_iter().map(String::from));
    cmd.extend([
        "merge-tree".into(),
        "--write-tree".into(),
        a.into(),
        b.into(),
    ]);
    let mt = std::process::Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("git merge-tree 调用失败: {e}"))?;
    let stdout = String::from_utf8_lossy(&mt.stdout).to_string();
    if !mt.status.success() {
        let detail = stdout
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("存在冲突")
            .to_string();
        return Err(format!("合并冲突: {detail}"));
    }
    let tree = stdout.lines().next().unwrap_or_default().trim().to_string();
    if tree.is_empty() {
        return Err("merge-tree 未产出树对象".into());
    }
    Ok(tree)
}

/// `git merge-tree --write-tree --merge-base=<mb> <onto> <commit>`（变基重放单提交
/// 的合成树；`mb` 为空则省略 --merge-base 交由 git 自算）。冲突 → `Err("合并冲突…")`。
pub(super) fn merge_tree_with_base_blocking(
    bare: &str,
    onto: &str,
    commit: &str,
    merge_base: &str,
) -> Result<String, String> {
    let identity = || vec!["-c", "user.name=NexHub", "-c", "user.email=nexhub@local"];
    let mut cmd: Vec<String> = vec!["git".into(), format!("--git-dir={bare}")];
    cmd.extend(identity().into_iter().map(String::from));
    cmd.extend(["merge-tree".into(), "--write-tree".into()]);
    if !merge_base.is_empty() {
        cmd.push(format!("--merge-base={merge_base}"));
    }
    cmd.push(onto.into());
    cmd.push(commit.into());
    let mt = std::process::Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("git merge-tree 调用失败: {e}"))?;
    let stdout = String::from_utf8_lossy(&mt.stdout).to_string();
    if !mt.status.success() {
        let detail = stdout
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("存在冲突")
            .to_string();
        return Err(format!("合并冲突: {detail}"));
    }
    let tree = stdout.lines().next().unwrap_or_default().trim().to_string();
    if tree.is_empty() {
        return Err("merge-tree 未产出树对象".into());
    }
    Ok(tree)
}

/// 读取提交元信息（变基重放保留原样）：`(GIT_AUTHOR_* 环境变量组, 完整提交信息)`。
/// 读失败降级（空 author + 空信息——commit-tree 仍会以 NexHub 身份落提交）。
pub(super) fn commit_meta_blocking(bare: &str, commit: &str) -> (Vec<(String, String)>, String) {
    let (n_ok, n_out) = run_git_sync(bare, &["show", "-s", "--format=%an", commit]);
    let (e_ok, e_out) = run_git_sync(bare, &["show", "-s", "--format=%ae", commit]);
    let (d_ok, d_out) = run_git_sync(bare, &["show", "-s", "--format=%aI", commit]);
    let (m_ok, m_out) = run_git_sync(bare, &["show", "-s", "--format=%B", commit]);
    let mut env = Vec::new();
    if n_ok && e_ok {
        let name = n_out.trim().to_string();
        let email = e_out.trim().to_string();
        if !name.is_empty() && !email.is_empty() {
            env.push(("GIT_AUTHOR_NAME".to_string(), name));
            env.push(("GIT_AUTHOR_EMAIL".to_string(), email));
            if d_ok {
                let date = d_out.trim().to_string();
                if !date.is_empty() {
                    env.push(("GIT_AUTHOR_DATE".to_string(), date));
                }
            }
        }
    }
    let msg = if m_ok {
        m_out.trim_end().to_string()
    } else {
        String::new()
    };
    (env, msg)
}

/// `commit-tree <tree> -p … -m <message>`（内置 NexHub committer 身份，不依赖
/// 全局配置；`author_env` = 变基重放保留的 GIT_AUTHOR_* 组）。返回新提交 sha。
pub(super) fn commit_tree_blocking(
    bare: &str,
    tree: &str,
    parents: &[&str],
    message: &str,
    author_env: &[(String, String)],
) -> Result<String, String> {
    let mut cmd: Vec<String> = vec!["git".into(), format!("--git-dir={bare}")];
    cmd.extend(
        ["-c", "user.name=NexHub", "-c", "user.email=nexhub@local"]
            .into_iter()
            .map(String::from),
    );
    cmd.push("commit-tree".into());
    cmd.push(tree.to_string());
    for p in parents {
        cmd.push("-p".into());
        cmd.push((*p).to_string());
    }
    cmd.push("-m".into());
    cmd.push(message.to_string());
    let mut c = std::process::Command::new(&cmd[0]);
    c.args(&cmd[1..]);
    for (k, v) in author_env {
        c.env(k, v);
    }
    let ct = c
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("git commit-tree 调用失败: {e}"))?;
    let commit = String::from_utf8_lossy(&ct.stdout).trim().to_string();
    if !ct.status.success() || commit.is_empty() {
        return Err(format!(
            "git commit-tree 失败: {}",
            String::from_utf8_lossy(&ct.stderr).trim()
        ));
    }
    Ok(commit)
}

/// `update-ref` 推进分支（原子 ref 更新）。
pub(super) fn update_ref_blocking(bare: &str, base_ref: &str, commit: &str) -> Result<(), String> {
    let (uok, _) = run_git_sync(bare, &["update-ref", base_ref, commit]);
    if !uok {
        return Err(format!("git update-ref {base_ref} 失败"));
    }
    Ok(())
}

/// 打 tag（blocking）：`git tag <tag> <默认分支>`（轻量 tag 定格在默认分支头）。
/// tag 已存在（含用户手动 `git tag` 过、DB 无行的场景）→ Err（409）。
pub(super) fn tag_release_blocking(bare: &str, tag: &str) -> Result<(), String> {
    let branch = resolve_default_branch_sync(bare);
    let target = format!("refs/heads/{branch}");
    let (ok, out) = run_git_sync_loud(bare, &["tag", tag, &target]);
    if ok {
        return Ok(());
    }
    let err = out.trim();
    if err.contains("already exists") {
        return Err(format!("tag 已存在: {tag}"));
    }
    Err(format!("git tag 失败: {err}"))
}

/// 删 tag（blocking）：`git tag -d <tag>`；tag 不在 git 对象库（如联邦落地行）→
/// 视为已删（Ok）——库行才是权威。
pub(super) fn delete_tag_blocking(bare: &str, tag: &str) {
    let _ = run_git_sync(bare, &["tag", "-d", tag]);
}

// 单元测试（参考 code_repo.rs 测试风格：纯函数 + 临时目录真实 git fixture）
// ----------------------------------------------------------------------------
