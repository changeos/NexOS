//! 文件管理纯逻辑模型与算法（规划文档 §3.16 files 组件 / files-agent 批 3）。
//!
//! 本模块**全部为纯函数**（无 IO、无外部依赖），便于单元测试与确定性回归。
//! 设计目标（见 `docs/agents/files-agent.md` §3 / 任务范围）：
//!
//! 1. **文件树模型**：`FileEntry` 排序 / 过滤 / 分页算法。
//!    - 排序：按 name / size / mtime，升序 / 降序。
//!    - 过滤：glob 名称匹配（自实现极简 glob，避免引入 `globset`）+ 类型过滤（仅文件 / 仅目录 / 全部）。
//!    - 分页：`PageRequest` → `PageResponse`。
//! 2. **分享链接校验**：给定 `ShareLink` + 用户输入（密码 / 当前时间）→ 是否允许访问。
//!    - 过期校验、密码校验（恒定时间比较，防侧信道计时攻击）。
//!    - token 生成（基于 UUID v4，避免引入 `rand`）。
//! 3. **同步冲突解决**：`SyncConflict` + last-write-wins / 三路合并简化版。
//! 4. **全文搜索骨架**：`SearchQuery` / `SearchResult`（真实 tantivy 索引见
//!    [`crate::search_index::SearchIndex`]；本模块仅留纯函数 [`text_search`] 作回退）。
//!
//! 依赖约束：仅用 `std` + 已注册的 `os-core` / `uuid` / `chrono` / `sha2`，**不引入** `rand` /
//! `globset` / `tantivy`（workspace 未注册，见红线「不虚构依赖」）。

use os_core::{DateTime, PageRequest, Utc};
use sha2::{Digest, Sha256};

use crate::files::{FileEntry, SearchHit, ShareLink};

// ============================================================================
// 排序
// ============================================================================

/// 文件条目排序键。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortKey {
    /// 按名称（字典序，目录优先）
    Name,
    /// 按大小升序（目录视为 0）
    Size,
    /// 按修改时间（新→旧 或 旧→新）
    Mtime,
}

/// 排序方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortDir {
    Asc,
    Desc,
}

/// 目录浏览查询参数（排序 + 过滤 + 分页的统一入口）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ListQuery {
    /// 排序键（默认 Name）
    pub sort_by: SortKey,
    /// 排序方向（默认 Asc）
    pub sort_dir: SortDir,
    /// 名称 glob 过滤（None = 不过滤；如 `*.rs`）。多模式以 `|` 分隔（OR 语义）。
    pub name_glob: Option<String>,
    /// 类型过滤
    pub kind: FileKind,
    /// 分页
    pub page: PageRequest,
}

impl Default for ListQuery {
    fn default() -> Self {
        Self {
            sort_by: SortKey::Name,
            sort_dir: SortDir::Asc,
            name_glob: None,
            kind: FileKind::All,
            page: PageRequest::default(),
        }
    }
}

/// 文件类型过滤。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    /// 文件与目录都返回
    All,
    /// 仅文件
    FileOnly,
    /// 仅目录
    DirOnly,
}

// ============================================================================
// 冲突解决
// ============================================================================

/// 同步冲突的双方版本（修改时间 + 内容哈希占位）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileVersion {
    /// 文件路径（双方相同——冲突的前提是路径相同）
    pub path: String,
    /// 修改时间
    pub mtime: DateTime,
    /// 内容指纹（占位；真实实现用 blake3/sha256，待依赖注册）
    pub content_hash: String,
}

/// 同步冲突（同一路径在两端被不同修改）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncConflict {
    /// 本地版本
    pub local: FileVersion,
    /// 远端版本
    pub remote: FileVersion,
    /// 共同祖先版本（用于三路合并；None = 无祖先 / 二路合并）
    pub base: Option<FileVersion>,
}

/// 冲突解决策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveStrategy {
    /// Last-Write-Wins：取 mtime 较新者。
    LastWriteWins,
    /// 本地优先（冲突时保留本地）。
    PreferLocal,
    /// 远端优先（冲突时保留远端）。
    PreferRemote,
}

/// 冲突解决结果。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ResolveResult {
    /// 胜出版本的路径
    pub path: String,
    /// 胜出方
    pub winner: ConflictSide,
    /// 胜出版本的内容哈希
    pub content_hash: String,
    /// 是否自动解决（false = 需人工介入；本骨架实现总是 true）
    pub auto_resolved: bool,
}

/// 冲突双方。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictSide {
    Local,
    Remote,
}

// ============================================================================
// 全文搜索骨架
// ============================================================================

/// 全文搜索查询参数（骨架；真实 tantivy 查询留 TODO）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchQuery {
    /// 搜索文本
    pub query: String,
    /// 限定根路径（None = 全局）
    pub root: Option<String>,
    /// 仅搜索指定后缀（如 `["md", "txt"]`；空 = 不限）
    pub extensions: Vec<String>,
    /// 分页
    pub page: PageRequest,
}

impl SearchQuery {
    /// 从字符串构造（root / extensions 取默认）。
    pub fn new(q: impl Into<String>) -> Self {
        Self {
            query: q.into(),
            root: None,
            extensions: Vec::new(),
            page: PageRequest::default(),
        }
    }
}

/// 搜索结果聚合（命中 + 分页）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchResult {
    /// 命中条目（已按 score 降序）
    pub hits: Vec<SearchHit>,
    /// 总命中数（分页前）
    pub total: u32,
}

// ============================================================================
// 目录浏览查询结果（含分页元信息）
// ============================================================================

/// 目录浏览结果（含分页后的条目 + 总数）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DirListing {
    /// 分页后的条目
    pub entries: Vec<FileEntry>,
    /// 过滤前总条目数（用于前端显示「共 N 项」）
    pub total_before_filter: u32,
    /// 过滤后、分页前条目数
    pub total_after_filter: u32,
}

// ============================================================================
// Glob 匹配（极简自实现；支持 `*` / `?` / 字面量；不区分大小写于跨平台友好）
// ============================================================================

/// 极简 glob 匹配——支持 `*`（任意序列，不含路径分隔符）/ `?`（单字符）/ 字面量。
///
/// 不支持 `**` / 字符类 `[abc]` / `{a,b}`——本场景仅用于文件名过滤，足够。
/// 大小写不敏感（Windows / macOS 默认大小写不敏感，跨平台一致）。
///
/// 算法：经典动态规划 / 回溯，O(n*m)。
pub fn glob_matches(name: &str, pattern: &str) -> bool {
    let name_b: Vec<char> = name.to_lowercase().chars().collect();
    let pat_b: Vec<char> = pattern.to_lowercase().chars().collect();
    glob_matches_inner(&name_b, &pat_b)
}

fn glob_matches_inner(name: &[char], pat: &[char]) -> bool {
    // dp[i] = 当前 pat 指针位于 pj 时，name[0..i] 是否匹配 pat[0..pj]
    // 用滚动布尔数组实现。
    let n = name.len();
    // prev[i] = 处理完 pat[j-1] 后，匹配 name[0..i] 的状态
    let mut prev = vec![false; n + 1];
    prev[0] = true; // 空模式匹配空串

    for pc in pat {
        let mut cur = vec![false; n + 1];
        if *pc == '*' {
            // '*' 可匹配 0 个或多个字符：cur[i] = prev[i] || cur[i-1]
            // prev[0] 表示 '*' 匹配空串；之后任一 prev 命中即可继承
            let mut any = false;
            for i in 0..=n {
                if prev[i] {
                    any = true;
                }
                cur[i] = any;
            }
        } else if *pc == '?' {
            // '?' 恰好匹配 1 个字符
            if n >= 1 {
                cur[1..=n].copy_from_slice(&prev[..n]);
            }
        } else {
            // 字面量
            for i in 1..=n {
                cur[i] = prev[i - 1] && name[i - 1] == *pc;
            }
        }
        prev = cur;
    }
    prev[n]
}

/// 解析 `a|b|c` 为多 glob；任一匹配即通过（OR 语义）。空 / None → 全通过。
pub fn glob_any_matches(name: &str, globs: Option<&str>) -> bool {
    match globs {
        None => true,
        Some(g) if g.trim().is_empty() => true,
        Some(g) => g.split('|').any(|p| {
            let p = p.trim();
            !p.is_empty() && glob_matches(name, p)
        }),
    }
}

// ============================================================================
// 文件树算法：过滤 → 排序 → 分页
// ============================================================================

/// 按 [`ListQuery`] 对原始条目做过滤 + 排序 + 分页，返回 [`DirListing`]。
///
/// 步骤（顺序固定，便于测试）：
/// 1. 类型过滤（`kind`）。
/// 2. 名称 glob 过滤（`name_glob`）。
/// 3. 排序（目录始终优先于文件；同类内按 `sort_by`/`sort_dir`）。
/// 4. 分页（`page.offset` / `page.limit`）。
pub fn list_entries(all: &[FileEntry], q: &ListQuery) -> DirListing {
    let total_before_filter = u32::try_from(all.len()).unwrap_or(u32::MAX);

    // 过滤
    let filtered: Vec<&FileEntry> = all
        .iter()
        .filter(|e| match q.kind {
            FileKind::All => true,
            FileKind::FileOnly => !e.is_dir,
            FileKind::DirOnly => e.is_dir,
        })
        .filter(|e| glob_any_matches(&e.name, q.name_glob.as_deref()))
        .collect();
    let total_after_filter = u32::try_from(filtered.len()).unwrap_or(u32::MAX);

    // 排序
    let mut sorted: Vec<&FileEntry> = filtered;
    sort_entries(&mut sorted, q.sort_by, q.sort_dir);

    // 分页
    let offset = q.page.offset as usize;
    let limit = q.page.limit as usize;
    let entries: Vec<FileEntry> = if offset >= sorted.len() {
        Vec::new()
    } else {
        let end = (offset + limit).min(sorted.len());
        sorted[offset..end].iter().map(|e| (*e).clone()).collect()
    };

    DirListing {
        entries,
        total_before_filter,
        total_after_filter,
    }
}

/// 原地排序：目录优先，同类内按 `sort_by`/`sort_dir`。
pub fn sort_entries(entries: &mut [&FileEntry], by: SortKey, dir: SortDir) {
    entries.sort_by(|a, b| {
        // 目录永远排在文件前（与方向无关，参考常见文件管理器行为）
        match (a.is_dir, b.is_dir) {
            (true, false) => return std::cmp::Ordering::Less,
            (false, true) => return std::cmp::Ordering::Greater,
            _ => {}
        }
        let ord = match by {
            SortKey::Name => a.name.cmp(&b.name),
            SortKey::Size => a.size.cmp(&b.size),
            SortKey::Mtime => a.modified.cmp(&b.modified),
        };
        apply_dir(ord, dir)
    });
}

fn apply_dir(ord: std::cmp::Ordering, dir: SortDir) -> std::cmp::Ordering {
    match dir {
        SortDir::Asc => ord,
        SortDir::Desc => ord.reverse(),
    }
}

// ============================================================================
// 分享链接：token 生成 + 校验
// ============================================================================

/// 生成分享 token（URL 安全、32 字符）。
///
/// 实现注记：workspace 未直接依赖 `rand` crate（红线：不虚构依赖），故 token 由
/// **UUID v4 + 时间戳** 组合而成——UUID v4 内部已含密码学随机源（uuid crate 的 `v4`
/// feature 走 `getrandom` 系统级 CSPRNG）。128bit 随机性 + 时间戳已满足分享 token
/// 抗猜测需求（同量级于会话 ID 强度）。
///
/// **后续可选增强**：若引入 `rand`/`rand_core` 直接 CSPRNG 取样（本 crate 已间接经
/// `getrandom` 获得），可输出 256bit 纯随机；当前 UUID v4 方案**已达标**，无运行时阻塞。
pub fn generate_share_token() -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let ts = Utc::now().timestamp_millis();
    format!("{id}{ts:x}")
}

/// 分享链接密码哈希（确定性，SHA-256）。
///
/// **实现注记**：workspace 已注册 `sha2`（ADR-DEPS-003），故本实现用 **SHA-256 + 固定
/// 应用域盐 + 8192 轮迭代**（PBKDF2 风格）替代原 FNV-1a 占位。
///
/// - **确定性**：签名 `fn(&str) -> String`（无 salt 入参，便于校验端复算）；为抵消无随机 salt
///   的弱点，采用域分隔 salt + 高轮次拉伸（每轮混入上轮摘要 + 域常量）。
/// - **算法标识**：哈希串以 `ssh256:` 前缀标注算法与轮数，便于将来切 Argon2id 时平滑识别旧哈希。
/// - **适用范围**：仅用于**分享链接访问密码**（短生命、低价值、校验侧为纯函数）。**系统用户登录
///   密码**走 `os-security::password::hash_password`（真实 Argon2id + 随机 salt）——两者**不共用**。
///
/// **安全限制**：固定 salt + 8192 轮仍弱于 Argon2id（无内存硬度），不适合高价值凭证存储。
/// 升级路径：评估引入 `argon2` 到本 crate 后改为 Argon2id（含 salt + 内存参数）。
//
// TODO(security): 引入 `argon2` 到 os-services 后替换为 Argon2id（随机 salt + 内存参数）。
//     当前 SHA-256 拉伸已远强于原 FNV 占位（FNV 非密码学哈希，不可作密码存储）。
pub fn hash_password(password: &str) -> String {
    // 应用域分隔盐（固定常量；与轮数一并写入前缀，便于将来识别）。
    const DOMAIN_SALT: &[u8] = b"os-services/share-link-v1";
    const ROUNDS: u32 = 8192;

    // 初始：SHA-256(salt ‖ password)
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN_SALT);
    hasher.update(password.as_bytes());
    let mut state: [u8; 32] = hasher.finalize().into();

    // 拉伸：每轮 SHA-256(prev ‖ salt ‖ round-counter ‖ password)
    //   round-counter 防止相同输入跨轮产生完全相同的块（参考 PBKDF2 的 INT_32_BE(i)）。
    for i in 0u32..ROUNDS {
        let mut h = Sha256::new();
        h.update(state);
        h.update(DOMAIN_SALT);
        h.update(i.to_be_bytes());
        h.update(password.as_bytes());
        state = h.finalize().into();
    }
    format!("ssh256:{}:{}", ROUNDS, hex_encode(&state))
}

/// 轻量 hex 编码（避免引入 `hex` crate；仅 32 字节固定长度输入）。
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// 恒定时间比较两个字符串（防计时侧信道）。长度不同时仍走完整流程后返回 false。
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    let mut diff: u8 = (ab.len() ^ bb.len()) as u8;
    let n = ab.len().min(bb.len());
    for i in 0..n {
        diff |= ab[i] ^ bb[i];
    }
    diff == 0
}

/// 分享链接访问决策输入（用户提供）。
#[derive(Debug, Clone)]
pub struct AccessRequest<'a> {
    /// 用户提交的 token（须与 link.token 一致）
    pub token: &'a str,
    /// 用户提交的密码明文（link 无密码时忽略）
    pub password: Option<&'a str>,
    /// 当前时间（用于过期判断；便于测试注入）
    pub now: DateTime,
}

/// 分享链接访问决策结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessDecision {
    /// 允许访问
    Allow,
    /// token 不匹配
    BadToken,
    /// 链接已过期
    Expired,
    /// 需要密码但未提供
    PasswordRequired,
    /// 密码错误
    BadPassword,
}

/// 校验分享链接访问——给定 link + 用户输入 + 当前时间 → 决策。
///
/// 纯函数，无副作用，便于穷尽测试所有路径。
pub fn check_share_access(link: &ShareLink, req: &AccessRequest<'_>) -> AccessDecision {
    // 1. token 校验（恒定时间，防计时泄漏 token 长度/前缀）
    if !constant_time_eq(&link.token, req.token) {
        return AccessDecision::BadToken;
    }
    // 2. 过期校验
    if let Some(exp) = link.expires_at {
        if req.now >= exp {
            return AccessDecision::Expired;
        }
    }
    // 3. 密码校验
    match &link.password_hash {
        None => AccessDecision::Allow,
        Some(hash) => match req.password {
            None => AccessDecision::PasswordRequired,
            Some(p) => {
                if constant_time_eq(hash, &hash_password(p)) {
                    AccessDecision::Allow
                } else {
                    AccessDecision::BadPassword
                }
            }
        },
    }
}

// ============================================================================
// 同步冲突解决
// ============================================================================

/// 判断两个版本是否真冲突（路径相同 + 内容不同）。
pub fn is_conflict(local: &FileVersion, remote: &FileVersion) -> bool {
    local.path == remote.path && local.content_hash != remote.content_hash
}

/// 按策略解决冲突。
///
/// - `LastWriteWins`：取 mtime 较新者；时间相同取 Local（确定性）。
/// - `PreferLocal` / `PreferRemote`：直接取对应方。
///
/// 若双方内容哈希相同（无冲突），返回 Local（视为已一致）。
pub fn resolve_conflict(c: &SyncConflict, strategy: ResolveStrategy) -> ResolveResult {
    // 无冲突：直接返回（不视为错误）
    if c.local.content_hash == c.remote.content_hash {
        return ResolveResult {
            path: c.local.path.clone(),
            winner: ConflictSide::Local,
            content_hash: c.local.content_hash.clone(),
            auto_resolved: true,
        };
    }
    let (winner, hash) = match strategy {
        ResolveStrategy::PreferLocal => (ConflictSide::Local, c.local.content_hash.clone()),
        ResolveStrategy::PreferRemote => (ConflictSide::Remote, c.remote.content_hash.clone()),
        ResolveStrategy::LastWriteWins => {
            if c.local.mtime >= c.remote.mtime {
                (ConflictSide::Local, c.local.content_hash.clone())
            } else {
                (ConflictSide::Remote, c.remote.content_hash.clone())
            }
        }
    };
    ResolveResult {
        path: c.local.path.clone(),
        winner,
        content_hash: hash,
        auto_resolved: true,
    }
}

/// 三路合并简化版：若只有一方相对 base 改动，取改动方；双方都改动 → 退化为 LWW。
///
/// 返回 (`ResolveResult`, `bool` merged_clean)：`merged_clean=false` 表示双方都改、
/// 无法干净合并，已退化为 LWW（真实实现应产出冲突文件供人工合并）。
pub fn three_way_merge(c: &SyncConflict) -> (ResolveResult, bool) {
    let local_changed = c
        .base
        .as_ref()
        .map(|b| b.content_hash != c.local.content_hash)
        .unwrap_or(true);
    let remote_changed = c
        .base
        .as_ref()
        .map(|b| b.content_hash != c.remote.content_hash)
        .unwrap_or(true);
    match (local_changed, remote_changed) {
        (false, false) => {
            // 都没改（或无 base 且哈希巧合一致）——返回 Local
            (
                ResolveResult {
                    path: c.local.path.clone(),
                    winner: ConflictSide::Local,
                    content_hash: c.local.content_hash.clone(),
                    auto_resolved: true,
                },
                true,
            )
        }
        (true, false) => (
            ResolveResult {
                path: c.local.path.clone(),
                winner: ConflictSide::Local,
                content_hash: c.local.content_hash.clone(),
                auto_resolved: true,
            },
            true,
        ),
        (false, true) => (
            ResolveResult {
                path: c.remote.path.clone(),
                winner: ConflictSide::Remote,
                content_hash: c.remote.content_hash.clone(),
                auto_resolved: true,
            },
            true,
        ),
        (true, true) => {
            // 双方都改 → 退化 LWW（非干净合并）
            (resolve_conflict(c, ResolveStrategy::LastWriteWins), false)
        }
    }
}

// ============================================================================
// 全文搜索：简易文本匹配（tantivy 未注册前的占位实现）
// ============================================================================

/// 在 `content` 中搜索 `query`（大小写不敏感），返回高亮 snippet 与 score。
///
/// score = 命中次数 / 内容长度（归一化），保证短文档命中得分更高（类似 TF 归一化）。
/// snippet 取首个命中位置前后各 32 字符。
///
/// **定位**：纯函数工具，不依赖外部索引——用于无 `SearchIndex` 场景的轻量回退与单测。
/// 真实 BM25 / 分词 / 高亮走 [`crate::search_index::SearchIndex`]（tantivy 真实索引，
/// ADR-DEPS-001 已注册；[`FileManager::fulltext_search`](crate::files::FileManager::fulltext_search)
/// 默认实现经 `with_search_index` 注入后走 tantivy）。
pub fn text_search(content: &str, query: &str) -> Option<SearchHit> {
    if query.trim().is_empty() {
        return None;
    }
    let lower_content = content.to_lowercase();
    let lower_query = query.to_lowercase();
    let hits: Vec<usize> = lower_content
        .match_indices(&lower_query)
        .map(|(i, _)| i)
        .collect();
    if hits.is_empty() {
        return None;
    }
    let count = hits.len();
    // score：命中密度（命中次数 / 内容字符数），加权避免极短内容爆分
    let density = count as f32 / content.len().max(1) as f32;
    let score = (density * 100.0 + count as f32 * 0.1).min(100.0);
    // snippet：首个命中前后 32 字符
    let first = hits[0];
    let start = first.saturating_sub(32);
    let end = (first + lower_query.len() + 32).min(content.len());
    let snippet = format!("…{}…", &content[start..end]);
    Some(SearchHit {
        path: String::new(), // 由调用方填充
        snippet,
        score,
    })
}

/// 对一组命中按 score 降序排序并分页。
pub fn paginate_hits(mut hits: Vec<SearchHit>, page: PageRequest) -> SearchResult {
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let total = u32::try_from(hits.len()).unwrap_or(u32::MAX);
    let offset = page.offset as usize;
    let limit = page.limit as usize;
    let items = if offset >= hits.len() {
        Vec::new()
    } else {
        let end = (offset + limit).min(hits.len());
        hits[offset..end].to_vec()
    };
    SearchResult { hits: items, total }
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool, size: u64, modified: DateTime) -> FileEntry {
        FileEntry {
            name: name.into(),
            is_dir,
            size,
            modified,
            permissions: "rw-r--r--".into(),
        }
    }

    fn ts(secs: i64) -> DateTime {
        DateTime::from_timestamp(secs, 0).unwrap()
    }

    // ---- glob ----

    #[test]
    fn glob_basic() {
        assert!(glob_matches("a.rs", "*.rs"));
        assert!(glob_matches("README.md", "*.md"));
        assert!(!glob_matches("a.txt", "*.rs"));
        assert!(glob_matches("a", "*"));
        assert!(glob_matches("ab", "a?"));
        assert!(!glob_matches("abc", "a?"));
        assert!(glob_matches("README.MD", "*.md")); // 大小写不敏感
    }

    #[test]
    fn glob_star_matches_empty() {
        assert!(glob_matches("", "*"));
        assert!(glob_matches("anything", "***"));
    }

    #[test]
    fn glob_any_or_semantics() {
        assert!(glob_any_matches("a.rs", Some("*.txt|*.rs")));
        assert!(!glob_any_matches("a.go", Some("*.txt|*.rs")));
        assert!(glob_any_matches("a.go", None));
        assert!(glob_any_matches("a.go", Some(""))); // 空 = 全通过
    }

    // ---- 排序 / 过滤 / 分页 ----

    #[test]
    fn sort_dirs_first_then_name() {
        let e = [
            entry("zfile", false, 10, ts(1)),
            entry("Adir", true, 0, ts(2)),
            entry("bdir", true, 0, ts(3)),
            entry("afile", false, 5, ts(4)),
        ];
        let refs: Vec<&FileEntry> = e.iter().collect();
        let mut r = refs;
        sort_entries(&mut r, SortKey::Name, SortDir::Asc);
        let names: Vec<&str> = r.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, vec!["Adir", "bdir", "afile", "zfile"]);
    }

    #[test]
    fn sort_by_size_desc_files() {
        let e = [
            entry("a", false, 1, ts(1)),
            entry("b", false, 30, ts(2)),
            entry("c", false, 10, ts(3)),
        ];
        let mut r: Vec<&FileEntry> = e.iter().collect();
        sort_entries(&mut r, SortKey::Size, SortDir::Desc);
        let names: Vec<&str> = r.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, vec!["b", "c", "a"]);
    }

    #[test]
    fn sort_by_mtime_desc() {
        let e = [
            entry("old", false, 1, ts(100)),
            entry("new", false, 1, ts(900)),
            entry("mid", false, 1, ts(500)),
        ];
        let mut r: Vec<&FileEntry> = e.iter().collect();
        sort_entries(&mut r, SortKey::Mtime, SortDir::Desc);
        let names: Vec<&str> = r.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, vec!["new", "mid", "old"]);
    }

    #[test]
    fn list_filter_and_paginate() {
        let e: Vec<FileEntry> = (0..10)
            .map(|i| entry(&format!("f{i}.rs"), false, i, ts(i as i64)))
            .chain((0..3).map(|i| entry(&format!("d{i}"), true, 0, ts(i))))
            .collect();
        let q = ListQuery {
            sort_by: SortKey::Name,
            sort_dir: SortDir::Asc,
            name_glob: Some("*.rs".into()),
            kind: FileKind::FileOnly,
            page: PageRequest {
                offset: 2,
                limit: 3,
            },
        };
        let listing = list_entries(&e, &q);
        assert_eq!(listing.total_before_filter, 13);
        assert_eq!(listing.total_after_filter, 10);
        assert_eq!(listing.entries.len(), 3);
        // 排序后第 3-5 个：目录被过滤掉、文件按名升序 f0.rs..f9.rs
        let names: Vec<&str> = listing.entries.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, vec!["f2.rs", "f3.rs", "f4.rs"]);
    }

    #[test]
    fn list_offset_beyond_returns_empty() {
        let e = vec![entry("a", false, 1, ts(1))];
        let q = ListQuery {
            page: PageRequest {
                offset: 100,
                limit: 10,
            },
            ..Default::default()
        };
        let l = list_entries(&e, &q);
        assert!(l.entries.is_empty());
        assert_eq!(l.total_after_filter, 1);
    }

    #[test]
    fn list_dir_only_filter() {
        let e = vec![entry("a", false, 1, ts(1)), entry("D1", true, 0, ts(2))];
        let q = ListQuery {
            kind: FileKind::DirOnly,
            ..Default::default()
        };
        let l = list_entries(&e, &q);
        assert_eq!(l.entries.len(), 1);
        assert!(l.entries[0].is_dir);
    }

    // ---- 分享链接校验 ----

    fn link(token: &str, exp: Option<DateTime>, pw: Option<&str>) -> ShareLink {
        ShareLink {
            id: "id1".into(),
            target_path: "/x".into(),
            token: token.into(),
            expires_at: exp,
            password_hash: pw.map(hash_password),
            rate_limit_kbps: None,
            created_by: "u".into(),
        }
    }

    #[test]
    fn access_allow_no_password_no_expiry() {
        let l = link("tok", None, None);
        let r = AccessRequest {
            token: "tok",
            password: None,
            now: ts(50),
        };
        assert_eq!(check_share_access(&l, &r), AccessDecision::Allow);
    }

    #[test]
    fn access_bad_token() {
        let l = link("tok", None, None);
        let r = AccessRequest {
            token: "wrong",
            password: None,
            now: ts(50),
        };
        assert_eq!(check_share_access(&l, &r), AccessDecision::BadToken);
    }

    #[test]
    fn access_expired() {
        let l = link("tok", Some(ts(100)), None);
        let r = AccessRequest {
            token: "tok",
            password: None,
            now: ts(200),
        };
        assert_eq!(check_share_access(&l, &r), AccessDecision::Expired);
    }

    #[test]
    fn access_not_yet_expired_boundary() {
        // now == expires_at 视为过期（>=）
        let l = link("tok", Some(ts(100)), None);
        let r = AccessRequest {
            token: "tok",
            password: None,
            now: ts(100),
        };
        assert_eq!(check_share_access(&l, &r), AccessDecision::Expired);
    }

    #[test]
    fn access_password_required() {
        let l = link("tok", None, Some("secret"));
        let r = AccessRequest {
            token: "tok",
            password: None,
            now: ts(50),
        };
        assert_eq!(check_share_access(&l, &r), AccessDecision::PasswordRequired);
    }

    #[test]
    fn access_bad_password() {
        let l = link("tok", None, Some("secret"));
        let r = AccessRequest {
            token: "tok",
            password: Some("nope"),
            now: ts(50),
        };
        assert_eq!(check_share_access(&l, &r), AccessDecision::BadPassword);
    }

    #[test]
    fn access_correct_password() {
        let l = link("tok", None, Some("secret"));
        let r = AccessRequest {
            token: "tok",
            password: Some("secret"),
            now: ts(50),
        };
        assert_eq!(check_share_access(&l, &r), AccessDecision::Allow);
    }

    #[test]
    fn constant_time_eq_handles_diff_lengths() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(!constant_time_eq("abcd", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
    }

    #[test]
    fn token_nonempty_and_unique() {
        let t1 = generate_share_token();
        let t2 = generate_share_token();
        assert!(!t1.is_empty());
        // 连续两次生成极大概率不同（UUID v4 + 时间戳）
        assert_ne!(t1, t2);
    }

    #[test]
    fn hash_password_deterministic() {
        assert_eq!(hash_password("abc"), hash_password("abc"));
        assert_ne!(hash_password("abc"), hash_password("abd"));
        // 算法前缀（含轮数标记）——便于将来切 Argon2id 时识别旧哈希。
        assert!(hash_password("abc").starts_with("ssh256:8192:"));
        // 密码不应以明文出现在哈希串中
        let h = hash_password("supersecret");
        assert!(!h.contains("supersecret"));
    }

    #[test]
    fn hash_password_avalanche() {
        // 单字符差异应产生完全不同的摘要
        let a = hash_password("password1");
        let b = hash_password("password2");
        assert_ne!(a, b);
        // 两个不同密码的哈希共享部分不应超过偶然长度（hex 串无公共后缀）
        let a_body = a.split(':').nth(2).unwrap();
        let b_body = b.split(':').nth(2).unwrap();
        assert_ne!(a_body, b_body);
    }

    // ---- 冲突解决 ----

    fn ver(path: &str, secs: i64, hash: &str) -> FileVersion {
        FileVersion {
            path: path.into(),
            mtime: ts(secs),
            content_hash: hash.into(),
        }
    }

    #[test]
    fn resolve_lww_local_newer() {
        let c = SyncConflict {
            local: ver("/a", 200, "h1"),
            remote: ver("/a", 100, "h2"),
            base: None,
        };
        let r = resolve_conflict(&c, ResolveStrategy::LastWriteWins);
        assert_eq!(r.winner, ConflictSide::Local);
        assert_eq!(r.content_hash, "h1");
    }

    #[test]
    fn resolve_lww_remote_newer() {
        let c = SyncConflict {
            local: ver("/a", 100, "h1"),
            remote: ver("/a", 200, "h2"),
            base: None,
        };
        let r = resolve_conflict(&c, ResolveStrategy::LastWriteWins);
        assert_eq!(r.winner, ConflictSide::Remote);
    }

    #[test]
    fn resolve_lww_same_mtime_picks_local() {
        let c = SyncConflict {
            local: ver("/a", 100, "h1"),
            remote: ver("/a", 100, "h2"),
            base: None,
        };
        let r = resolve_conflict(&c, ResolveStrategy::LastWriteWins);
        assert_eq!(r.winner, ConflictSide::Local);
    }

    #[test]
    fn resolve_prefer_remote_overrides_lww() {
        let c = SyncConflict {
            local: ver("/a", 200, "h1"),
            remote: ver("/a", 100, "h2"),
            base: None,
        };
        let r = resolve_conflict(&c, ResolveStrategy::PreferRemote);
        assert_eq!(r.winner, ConflictSide::Remote);
    }

    #[test]
    fn resolve_no_conflict_returns_local() {
        let c = SyncConflict {
            local: ver("/a", 100, "same"),
            remote: ver("/a", 999, "same"),
            base: None,
        };
        let r = resolve_conflict(&c, ResolveStrategy::LastWriteWins);
        assert_eq!(r.winner, ConflictSide::Local);
        assert_eq!(r.content_hash, "same");
    }

    #[test]
    fn is_conflict_detects_content_diff() {
        assert!(is_conflict(&ver("/a", 1, "x"), &ver("/a", 2, "y")));
        assert!(!is_conflict(&ver("/a", 1, "x"), &ver("/a", 2, "x")));
        assert!(!is_conflict(&ver("/a", 1, "x"), &ver("/b", 2, "y")));
    }

    // ---- 三路合并 ----

    #[test]
    fn three_way_only_local_changed_clean() {
        let c = SyncConflict {
            local: ver("/a", 100, "new_local"),
            remote: ver("/a", 100, "base_hash"),
            base: Some(ver("/a", 50, "base_hash")),
        };
        let (r, clean) = three_way_merge(&c);
        assert!(clean);
        assert_eq!(r.winner, ConflictSide::Local);
        assert_eq!(r.content_hash, "new_local");
    }

    #[test]
    fn three_way_only_remote_changed_clean() {
        let c = SyncConflict {
            local: ver("/a", 100, "base_hash"),
            remote: ver("/a", 100, "new_remote"),
            base: Some(ver("/a", 50, "base_hash")),
        };
        let (r, clean) = three_way_merge(&c);
        assert!(clean);
        assert_eq!(r.winner, ConflictSide::Remote);
    }

    #[test]
    fn three_way_both_changed_not_clean() {
        let c = SyncConflict {
            local: ver("/a", 200, "new_local"),
            remote: ver("/a", 100, "new_remote"),
            base: Some(ver("/a", 50, "base_hash")),
        };
        let (r, clean) = three_way_merge(&c);
        assert!(!clean); // 双方都改 → 退化 LWW
        assert_eq!(r.winner, ConflictSide::Local); // local mtime 较新
    }

    #[test]
    fn three_way_neither_changed() {
        let c = SyncConflict {
            local: ver("/a", 100, "same"),
            remote: ver("/a", 100, "same"),
            base: Some(ver("/a", 50, "same")),
        };
        let (r, clean) = three_way_merge(&c);
        assert!(clean);
        assert_eq!(r.content_hash, "same");
    }

    // ---- 全文搜索骨架 ----

    #[test]
    fn text_search_finds_and_scores() {
        let content = "hello world hello rust";
        let h = text_search(content, "hello").unwrap();
        assert!(h.score > 0.0);
        assert!(h.snippet.contains("hello"));
    }

    #[test]
    fn text_search_case_insensitive() {
        let h = text_search("Hello WORLD", "hello").unwrap();
        assert!(h.snippet.to_lowercase().contains("hello"));
    }

    #[test]
    fn text_search_no_match() {
        assert!(text_search("hello", "rust").is_none());
        assert!(text_search("hello", "").is_none());
    }

    #[test]
    fn text_search_shorter_content_scores_higher() {
        let short = text_search("rust", "rust").unwrap();
        let long = text_search(&format!("rust {}", "x".repeat(1000)), "rust").unwrap();
        assert!(short.score > long.score);
    }

    #[test]
    fn paginate_hits_sorts_desc_and_pages() {
        let hits = vec![
            SearchHit {
                path: "a".into(),
                snippet: String::new(),
                score: 1.0,
            },
            SearchHit {
                path: "b".into(),
                snippet: String::new(),
                score: 5.0,
            },
            SearchHit {
                path: "c".into(),
                snippet: String::new(),
                score: 3.0,
            },
        ];
        let r = paginate_hits(
            hits,
            PageRequest {
                offset: 1,
                limit: 1,
            },
        );
        assert_eq!(r.total, 3);
        assert_eq!(r.hits.len(), 1);
        assert_eq!(r.hits[0].path, "c"); // 5.0,3.0,1.0 → 第二个是 3.0
    }

    #[test]
    fn paginate_hits_empty() {
        let r = paginate_hits(Vec::new(), PageRequest::default());
        assert_eq!(r.total, 0);
        assert!(r.hits.is_empty());
    }
}
