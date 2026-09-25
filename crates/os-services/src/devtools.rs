//! 运维 / 开发者工具（规划文档 §3.16 devtools 组件）
//!
//! 职责：
//! - CI 流水线触发与状态查询（拉取仓库 → 跑 steps → 上报日志）
//! - 加密 KVS 密钥存储（store/get/rotate，密文落盘）
//! - 日志聚合（CI 运行日志的过滤 / 搜索，纯逻辑算法）
//! - Git 服务模型（仓库/分支/提交元数据，**gix 真实集成已在 `impl_devtools`
//!   落地**——本地 init/commit/log/branch 真实可跑；远端 clone 留 TODO \[RUNTIME\]）
//!
//! 本文件归 `devtools-agent` 维护；其它 service-agent 拥有的文件（backup.rs /
//! monitor.rs / media.rs / files.rs / power.rs）不得改动。

use os_core::{DateTime, Deserialize, Serialize, TaskId};

use crate::ServiceError;

// ----------------------------------------------------------------------------
// CI 流水线
// ----------------------------------------------------------------------------

/// CI 流水线定义
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CiPipeline {
    /// 流水线 ID
    pub id: String,
    /// 流水线名
    pub name: String,
    /// 仓库地址（git url）
    pub repo_url: String,
    /// 触发分支（如 `"main"`）
    pub branch: String,
    /// 步骤列表（按顺序执行的命令/脚本名）
    pub steps: Vec<String>,
}

/// CI 运行状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiStatus {
    /// 排队中
    Pending,
    /// 运行中
    Running,
    /// 成功
    Success,
    /// 失败
    Failed,
    /// 已取消
    Canceled,
}

/// 一次 CI 运行
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CiRun {
    /// 关联流水线 ID
    pub pipeline_id: String,
    /// 本次运行 ID
    pub run_id: String,
    /// 当前状态
    pub status: CiStatus,
    /// 开始时间
    pub started_at: DateTime,
    /// 日志地址（运行中可能为 None）
    pub logs_url: Option<String>,
}

// ----------------------------------------------------------------------------
// 加密密钥
// ----------------------------------------------------------------------------

/// 加密密钥条目（值以加密形式存储）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretEntry {
    /// 密钥名（如 `"s3_access_key"`）
    pub key: String,
    /// 加密后的值
    pub value_encrypted: Vec<u8>,
    /// 上次更新时间
    pub updated_at: DateTime,
    /// 轮换周期（天；None = 不自动轮换）
    pub rotation_days: Option<u32>,
}

// ----------------------------------------------------------------------------
// DevTools trait（async）
// ----------------------------------------------------------------------------

/// 开发者工具——CI 流水线 + 加密 KVS 密钥。
#[allow(async_fn_in_trait)]
pub trait DevTools: Send + Sync {
    /// 触发一个流水线，返回追踪用的任务 ID。
    async fn trigger_pipeline(&self, pipeline_id: &str) -> Result<TaskId, ServiceError>;

    /// 查询某次任务对应的 CI 运行状态。
    async fn pipeline_status(&self, task: &TaskId) -> Result<CiRun, ServiceError>;

    /// 存储密钥（加密后落盘）。
    async fn store_secret(&self, key: &str, value: &[u8]) -> Result<(), ServiceError>;

    /// 读取密钥（解密后返回明文）。
    async fn get_secret(&self, key: &str) -> Result<Vec<u8>, ServiceError>;

    /// 立即轮换密钥（生成新值并更新）。
    async fn rotate_secret(&self, key: &str) -> Result<(), ServiceError>;

    /// 列出所有流水线定义。
    async fn list_pipelines(&self) -> Result<Vec<CiPipeline>, ServiceError>;
}

// ============================================================================
// 日志聚合模型（CI 运行日志的过滤 / 搜索，纯逻辑算法）
// ============================================================================
//
// 设计：CI 运行产生的日志条目（`DevLogEntry`）独立于 monitor 组件的 `LogEntry`
// （后者面向全系统日志，归 monitor-agent；此处只服务 devtools 自身的 CI 日志聚合）。
// 过滤 / 搜索是纯函数，无 IO 依赖，可独立单测。

/// CI 运行日志级别（与 monitor.LogLevel 同义，但独立定义以避免跨 agent 改动 monitor.rs）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DevLogLevel {
    /// 调试
    Trace,
    /// 详细
    Debug,
    /// 信息
    Info,
    /// 警告
    Warn,
    /// 错误
    Error,
}

impl DevLogLevel {
    /// 级别对应的数字权重（用于 `>=` 阈值过滤；越大越严重）
    #[must_use]
    pub fn weight(self) -> u8 {
        match self {
            Self::Trace => 0,
            Self::Debug => 1,
            Self::Info => 2,
            Self::Warn => 3,
            Self::Error => 4,
        }
    }
}

/// 一条 CI 运行日志（聚合模型）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevLogEntry {
    /// 关联流水线 ID
    pub pipeline_id: String,
    /// 关联运行 ID
    pub run_id: String,
    /// 步骤序号（从 0 起；标识该日志来自第几个 step）
    pub step_index: usize,
    /// 级别
    pub level: DevLogLevel,
    /// 日志来源（step 名 / 命令名）
    pub source: String,
    /// 日志消息体
    pub message: String,
    /// 时间戳（UTC）
    pub timestamp: DateTime,
}

/// 日志查询条件（过滤 + 关键词搜索）
///
/// 所有字段均为 `Option`，`None` 表示该维度不限制；多维度间为「逻辑与」。
/// `min_level` 为 `Some(l)` 时仅保留 `level.weight() >= l.weight()` 的条目。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LogQuery {
    /// 流水线 ID 过滤
    pub pipeline_id: Option<String>,
    /// 运行 ID 过滤
    pub run_id: Option<String>,
    /// 步骤序号过滤
    pub step_index: Option<usize>,
    /// 最低日志级别（含），None = 不过滤
    pub min_level: Option<DevLogLevel>,
    /// 来源前缀 / 子串匹配（None = 不过滤）
    pub source: Option<String>,
    /// 关键词（大小写不敏感子串匹配 message；None = 不过滤）
    pub keyword: Option<String>,
    /// 起始时间（含），None = 不限
    pub since: Option<DateTime>,
    /// 截止时间（含），None = 不限
    pub until: Option<DateTime>,
    /// 最多返回条数（None = 不限）。结果按 timestamp 升序后再截断。
    pub limit: Option<usize>,
}

impl LogQuery {
    /// 便捷构造：仅按关键词搜索。
    #[must_use]
    pub fn keyword(kw: impl Into<String>) -> Self {
        Self {
            keyword: Some(kw.into()),
            ..Self::default()
        }
    }

    /// 单条日志是否匹配本查询（不考虑 limit）。
    ///
    /// 纯函数：无 IO、无 panic；用于 `filter_logs` 与单测。
    #[must_use]
    pub fn matches(&self, e: &DevLogEntry) -> bool {
        if let Some(ref p) = self.pipeline_id {
            if &e.pipeline_id != p {
                return false;
            }
        }
        if let Some(ref r) = self.run_id {
            if &e.run_id != r {
                return false;
            }
        }
        if let Some(s) = self.step_index {
            if e.step_index != s {
                return false;
            }
        }
        if let Some(l) = self.min_level {
            if e.level.weight() < l.weight() {
                return false;
            }
        }
        if let Some(ref src) = self.source {
            if !e.source.contains(src.as_str()) {
                return false;
            }
        }
        if let Some(ref kw) = self.keyword {
            // 大小写不敏感子串匹配
            if !e.message.to_lowercase().contains(&kw.to_lowercase()) {
                return false;
            }
        }
        if let Some(t) = self.since {
            if e.timestamp < t {
                return false;
            }
        }
        if let Some(t) = self.until {
            if e.timestamp > t {
                return false;
            }
        }
        true
    }
}

/// 按查询条件过滤并排序日志（纯逻辑，无 IO）。
///
/// 步骤：过滤 → 按 timestamp 升序稳定排序 → 应用 `limit` 截断。
/// 输入 `logs` 不会被修改（消费后返回新 `Vec`）。
///
/// # 示例
/// ```
/// use os_services::devtools::{DevLogEntry, DevLogLevel, LogQuery, filter_logs};
/// let logs = vec![
///     DevLogEntry { pipeline_id: "p".into(), run_id: "r".into(), step_index: 0,
///         level: DevLogLevel::Info, source: "build".into(),
///         message: "started".into(), timestamp: chrono::Utc::now() },
/// ];
/// let q = LogQuery::keyword("start");
/// assert_eq!(filter_logs(logs, &q).len(), 1);
/// ```
pub fn filter_logs(mut logs: Vec<DevLogEntry>, q: &LogQuery) -> Vec<DevLogEntry> {
    logs.retain(|e| q.matches(e));
    // 稳定排序：保留等时间戳条目的原始相对顺序（step 内顺序）
    logs.sort_by_key(|e| e.timestamp);
    if let Some(n) = q.limit {
        logs.truncate(n);
    }
    logs
}

// ============================================================================
// 密钥管理 KVS（元数据 + 访问审计日志，纯逻辑）
// ============================================================================
//
// 红线：密钥值绝不存明文。`SecretEntry.value_encrypted` 必须是密文；
// 本 agent 的 KVS 加密独立于系统密钥（security/wallet），**真实 AEAD 已在
// [`crate::impl_devtools::DefaultDevTools`] 接通**（AES-256-GCM，ADR-DEPS-003）。
// 本模块的纯逻辑模型（`MemKvs` 测试辅助）仅用于单测——其 `ENC:` 占位是**测试桩**，
// 非生产路径；真实加密走 [`DefaultDevTools::store_secret`]。
//
// 审计分类：以下 devtools TODO 均属 [DOC]（说明性）/ [STUB-but-test-only]（测试桩）。

/// 密钥 ID（逻辑标识；与 `SecretEntry.key` 同义，作为强类型别名用于 KVS API）
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SecretId(pub String);

impl SecretId {
    /// 构造密钥 ID
    #[must_use]
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    /// 取内部字符串引用
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for SecretId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl std::fmt::Display for SecretId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 密钥元数据（不含密文，便于审计/列举时不泄露值）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretMeta {
    /// 密钥 ID
    pub id: SecretId,
    /// 描述（人类可读，可含用途说明；不含密文）
    pub description: Option<String>,
    /// 上次更新时间
    pub updated_at: DateTime,
    /// 轮换周期（天；None = 不自动轮换）
    pub rotation_days: Option<u32>,
}

/// 密钥访问操作类型（审计用）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretAction {
    /// 存储 / 更新
    Store,
    /// 读取
    Get,
    /// 轮换
    Rotate,
    /// 删除
    Delete,
}

/// 一条密钥访问审计记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretAuditEntry {
    /// 被访问的密钥 ID
    pub id: SecretId,
    /// 操作类型
    pub action: SecretAction,
    /// 调用者标识（用户/服务名；由上层注入）
    pub actor: String,
    /// 访问时间
    pub at: DateTime,
    /// 是否成功
    pub success: bool,
    /// 失败原因（success=false 时填）
    pub error: Option<String>,
}

/// 内存态密钥访问审计日志（追加写，可查询）。
///
/// 设计：纯逻辑容器，不持久化（持久化由 `DefaultDevTools` 落盘层负责）。
/// `record` 追加；`for_secret` 按 ID 过滤；`all` 返回全部（按时间升序）。
#[derive(Debug, Default, Clone)]
pub struct SecretAuditLog {
    entries: Vec<SecretAuditEntry>,
}

impl SecretAuditLog {
    /// 构造空审计日志。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一条审计记录，返回其索引。
    pub fn record(&mut self, entry: SecretAuditEntry) -> usize {
        self.entries.push(entry);
        self.entries.len() - 1
    }

    /// 查询某密钥的全部审计记录（按时间升序的稳定顺序）。
    #[must_use]
    pub fn for_secret(&self, id: &SecretId) -> Vec<&SecretAuditEntry> {
        self.entries.iter().filter(|e| &e.id == id).collect()
    }

    /// 全部审计记录（不可变切片引用，按追加顺序）。
    #[must_use]
    pub fn all(&self) -> &[SecretAuditEntry] {
        &self.entries
    }

    /// 记录数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ============================================================================
// Git 服务模型（仓库/分支/提交元数据；gix 真实集成留 TODO）
// ============================================================================

/// Git 仓库规格（devtools 视角的仓库元数据）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoSpec {
    /// 仓库逻辑名（如 `"os-core"`）
    pub name: String,
    /// 远端 URL（git url；https / ssh / file）
    pub url: String,
    /// 默认分支（如 `"main"`）
    pub default_branch: String,
    /// 凭据引用（指向 KVS 中的 SecretId；None = 匿名/公开仓库）
    pub credential: Option<SecretId>,
}

/// 分支引用
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Branch {
    /// 分支名
    pub name: String,
    /// 指向的最新提交 SHA（hex）
    pub head: String,
    /// 上游远端跟踪分支（如 `"origin/main"`），None = 本地分支
    pub upstream: Option<String>,
}

/// 提交元数据（轻量，不含 diff）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Commit {
    /// 提交 SHA（hex，完整 40 字符或缩写）
    pub sha: String,
    /// 作者
    pub author: String,
    /// 作者邮箱
    pub author_email: String,
    /// 提交消息（首行）
    pub message: String,
    /// 提交时间
    pub committed_at: DateTime,
}

/// 仓库快照（一次拉取后的元数据视图）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoSnapshot {
    /// 关联仓库规格
    pub spec: RepoSpec,
    /// 拉取到的分支列表
    pub branches: Vec<Branch>,
    /// 默认分支的最新提交
    pub head: Option<Commit>,
}

// ============================================================================
// 单元测试（纯逻辑：日志过滤/搜索 + KVS CRUD + 审计）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use chrono::Utc;
    use std::collections::HashMap;

    fn ts(minute: i64) -> DateTime {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, minute.clamp(0, 59) as u32, 0)
            .unwrap()
    }

    fn entry(
        run: &str,
        step: usize,
        level: DevLogLevel,
        source: &str,
        msg: &str,
        t: DateTime,
    ) -> DevLogEntry {
        DevLogEntry {
            pipeline_id: "p1".into(),
            run_id: run.into(),
            step_index: step,
            level,
            source: source.into(),
            message: msg.into(),
            timestamp: t,
        }
    }

    // ---- 日志过滤 / 搜索 ----

    #[test]
    fn log_level_weight_ordering() {
        assert!(DevLogLevel::Trace.weight() < DevLogLevel::Debug.weight());
        assert!(DevLogLevel::Debug.weight() < DevLogLevel::Info.weight());
        assert!(DevLogLevel::Info.weight() < DevLogLevel::Warn.weight());
        assert!(DevLogLevel::Warn.weight() < DevLogLevel::Error.weight());
    }

    #[test]
    fn filter_by_run_id_and_step() {
        let logs = vec![
            entry("r1", 0, DevLogLevel::Info, "build", "ok", ts(1)),
            entry("r1", 1, DevLogLevel::Info, "test", "ok", ts(2)),
            entry("r2", 0, DevLogLevel::Info, "build", "ok", ts(3)),
        ];
        let q = LogQuery {
            run_id: Some("r1".into()),
            step_index: Some(1),
            ..LogQuery::default()
        };
        let out = filter_logs(logs, &q);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source, "test");
    }

    #[test]
    fn filter_by_min_level_keeps_warn_and_above() {
        let logs = vec![
            entry("r1", 0, DevLogLevel::Debug, "s", "d", ts(1)),
            entry("r1", 0, DevLogLevel::Info, "s", "i", ts(2)),
            entry("r1", 0, DevLogLevel::Warn, "s", "w", ts(3)),
            entry("r1", 0, DevLogLevel::Error, "s", "e", ts(4)),
        ];
        let q = LogQuery {
            min_level: Some(DevLogLevel::Warn),
            ..LogQuery::default()
        };
        let out = filter_logs(logs, &q);
        assert_eq!(out.len(), 2);
        assert!(out
            .iter()
            .all(|e| e.level.weight() >= DevLogLevel::Warn.weight()));
    }

    #[test]
    fn filter_by_keyword_case_insensitive() {
        let logs = vec![
            entry("r1", 0, DevLogLevel::Info, "s", "Build started", ts(1)),
            entry("r1", 0, DevLogLevel::Info, "s", "tests passed", ts(2)),
        ];
        let out = filter_logs(logs, &LogQuery::keyword("BUILD"));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message, "Build started");
    }

    #[test]
    fn filter_by_source_substring() {
        let logs = vec![
            entry("r1", 0, DevLogLevel::Info, "build:docker", "x", ts(1)),
            entry("r1", 0, DevLogLevel::Info, "test:unit", "y", ts(2)),
        ];
        let q = LogQuery {
            source: Some("build".into()),
            ..LogQuery::default()
        };
        let out = filter_logs(logs, &q);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source, "build:docker");
    }

    #[test]
    fn filter_by_time_range() {
        let logs = vec![
            entry("r1", 0, DevLogLevel::Info, "s", "a", ts(1)),
            entry("r1", 0, DevLogLevel::Info, "s", "b", ts(5)),
            entry("r1", 0, DevLogLevel::Info, "s", "c", ts(10)),
        ];
        let q = LogQuery {
            since: Some(ts(2)),
            until: Some(ts(8)),
            ..LogQuery::default()
        };
        let out = filter_logs(logs, &q);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message, "b");
    }

    #[test]
    fn filter_sorts_by_timestamp_then_applies_limit() {
        // 故意乱序输入，验证输出按时间升序
        let logs = vec![
            entry("r1", 0, DevLogLevel::Info, "s", "c", ts(3)),
            entry("r1", 0, DevLogLevel::Info, "s", "a", ts(1)),
            entry("r1", 0, DevLogLevel::Info, "s", "b", ts(2)),
        ];
        let q = LogQuery {
            limit: Some(2),
            ..LogQuery::default()
        };
        let out = filter_logs(logs, &q);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].message, "a");
        assert_eq!(out[1].message, "b");
    }

    #[test]
    fn query_default_matches_all() {
        let q = LogQuery::default();
        let e = entry("r", 0, DevLogLevel::Trace, "s", "m", ts(0));
        assert!(q.matches(&e));
    }

    // ---- SecretId / SecretMeta ----

    #[test]
    fn secret_id_construct_and_display() {
        let id = SecretId::new("s3_key");
        assert_eq!(id.as_str(), "s3_key");
        assert_eq!(format!("{id}"), "s3_key");
        let id2: SecretId = "tok".to_string().into();
        assert_eq!(id2.as_str(), "tok");
        assert_eq!(SecretId::new("a"), SecretId::new("a"));
    }

    // ---- SecretAuditLog CRUD 风格 ----

    fn audit(id: &str, action: SecretAction, ok: bool, t: DateTime) -> SecretAuditEntry {
        SecretAuditEntry {
            id: SecretId::new(id),
            action,
            actor: "tester".into(),
            at: t,
            success: ok,
            error: if ok { None } else { Some("missing".into()) },
        }
    }

    #[test]
    fn audit_log_record_and_filter() {
        let mut log = SecretAuditLog::new();
        assert!(log.is_empty());
        log.record(audit("k1", SecretAction::Store, true, ts(1)));
        log.record(audit("k1", SecretAction::Get, true, ts(2)));
        log.record(audit("k2", SecretAction::Get, false, ts(3)));
        log.record(audit("k1", SecretAction::Rotate, true, ts(4)));
        assert_eq!(log.len(), 4);

        let k1 = log.for_secret(&SecretId::new("k1"));
        assert_eq!(k1.len(), 3);
        // 顺序保持追加顺序（升序）
        assert_eq!(k1[0].action, SecretAction::Store);
        assert_eq!(k1[2].action, SecretAction::Rotate);

        let k2 = log.for_secret(&SecretId::new("k2"));
        assert_eq!(k2.len(), 1);
        assert!(!k2[0].success);
        assert_eq!(k2[0].error.as_deref(), Some("missing"));

        assert_eq!(log.all().len(), 4);
    }

    #[test]
    fn audit_log_records_failed_get_for_missing() {
        // 模拟 get_secret 命中不存在密钥 → 记录失败审计
        let mut log = SecretAuditLog::new();
        let missing = SecretId::new("nope");
        log.record(SecretAuditEntry {
            id: missing.clone(),
            action: SecretAction::Get,
            actor: "svc".into(),
            at: ts(1),
            success: false,
            error: Some("not found".into()),
        });
        let recs = log.for_secret(&missing);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].action, SecretAction::Get);
        assert!(!recs[0].success);
    }

    // ---- KVS CRUD 一致性（内存模型，模拟 store/get/rotate/delete + 审计）----

    /// 内存 KVS：模拟 DefaultDevTools 的密钥存储逻辑（**测试桩**——真实加密走
    /// `DefaultDevTools` 的 AES-256-GCM；此处用 `ENC:` 占位仅验证 CRUD/审计逻辑）。
    #[derive(Default)]
    struct MemKvs {
        entries: HashMap<SecretId, SecretEntry>,
        audit: SecretAuditLog,
    }

    impl MemKvs {
        fn store(&mut self, id: SecretId, plaintext: &[u8], at: DateTime) {
            // [DOC/STUB-test-only] 用 `ENC:` 前缀 + 明文作为密文占位——仅用于本
            // 测试模块的 CRUD/审计逻辑验证；真实加密在 DefaultDevTools（AES-256-GCM）。
            let mut cipher = Vec::with_capacity(plaintext.len() + 4);
            cipher.extend_from_slice(b"ENC:");
            cipher.extend_from_slice(plaintext);
            let prev_rotation = self.entries.get(&id).and_then(|e| e.rotation_days);
            self.entries.insert(
                id.clone(),
                SecretEntry {
                    key: id.as_str().to_string(),
                    value_encrypted: cipher,
                    updated_at: at,
                    rotation_days: prev_rotation,
                },
            );
            self.audit.record(SecretAuditEntry {
                id,
                action: SecretAction::Store,
                actor: "test".into(),
                at,
                success: true,
                error: None,
            });
        }

        fn get(&mut self, id: &SecretId, at: DateTime) -> Result<Vec<u8>, ServiceError> {
            let res = self.entries.get(id).map(|e| {
                // [DOC/STUB-test-only] 逆占位：剥 `ENC:` 前缀还原明文（测试桩）。
                e.value_encrypted
                    .strip_prefix(b"ENC:")
                    .map(|v| v.to_vec())
                    .unwrap_or_default()
            });
            match res {
                Some(v) => {
                    self.audit.record(SecretAuditEntry {
                        id: id.clone(),
                        action: SecretAction::Get,
                        actor: "test".into(),
                        at,
                        success: true,
                        error: None,
                    });
                    Ok(v)
                }
                None => {
                    self.audit.record(SecretAuditEntry {
                        id: id.clone(),
                        action: SecretAction::Get,
                        actor: "test".into(),
                        at,
                        success: false,
                        error: Some("not found".into()),
                    });
                    Err(ServiceError::SecretNotFound(id.to_string()))
                }
            }
        }

        fn rotate(
            &mut self,
            id: &SecretId,
            new_plain: &[u8],
            at: DateTime,
        ) -> Result<(), ServiceError> {
            if !self.entries.contains_key(id) {
                self.audit.record(SecretAuditEntry {
                    id: id.clone(),
                    action: SecretAction::Rotate,
                    actor: "test".into(),
                    at,
                    success: false,
                    error: Some("not found".into()),
                });
                return Err(ServiceError::SecretNotFound(id.to_string()));
            }
            // 重新「加密」并更新时间
            let mut cipher = Vec::with_capacity(new_plain.len() + 4);
            cipher.extend_from_slice(b"ENC:");
            cipher.extend_from_slice(new_plain);
            let entry = self.entries.get_mut(id).expect("checked above");
            entry.value_encrypted = cipher;
            entry.updated_at = at;
            self.audit.record(SecretAuditEntry {
                id: id.clone(),
                action: SecretAction::Rotate,
                actor: "test".into(),
                at,
                success: true,
                error: None,
            });
            Ok(())
        }

        fn delete(&mut self, id: &SecretId, at: DateTime) -> Result<(), ServiceError> {
            let ok = self.entries.remove(id).is_some();
            self.audit.record(SecretAuditEntry {
                id: id.clone(),
                action: SecretAction::Delete,
                actor: "test".into(),
                at,
                success: ok,
                error: if ok { None } else { Some("not found".into()) },
            });
            // 删除不存在的密钥视为幂等成功（仍记录审计）
            Ok(())
        }
    }

    #[test]
    fn kvs_store_get_roundtrip() {
        let mut k = MemKvs::default();
        let id = SecretId::new("s3_access_key");
        k.store(id.clone(), b"hunter2", ts(1));
        let v = k.get(&id, ts(2)).expect("stored");
        assert_eq!(v, b"hunter2");
        // store + get 两条审计
        assert_eq!(k.audit.for_secret(&id).len(), 2);
    }

    #[test]
    fn kvs_get_missing_returns_secret_not_found_and_audit() {
        let mut k = MemKvs::default();
        let id = SecretId::new("missing");
        let err = k.get(&id, ts(1)).unwrap_err();
        assert!(matches!(err, ServiceError::SecretNotFound(_)));
        let recs = k.audit.for_secret(&id);
        assert_eq!(recs.len(), 1);
        assert!(!recs[0].success);
    }

    #[test]
    fn kvs_rotate_changes_value_and_updates_timestamp() {
        let mut k = MemKvs::default();
        let id = SecretId::new("token");
        k.store(id.clone(), b"old", ts(1));
        k.rotate(&id, b"new", ts(5)).expect("rotate");
        let v = k.get(&id, ts(6)).expect("get");
        assert_eq!(v, b"new");
        // entry.updated_at 应推进到 ts(5)
        assert_eq!(k.entries.get(&id).unwrap().updated_at, ts(5));
        // store + rotate + get = 3 条审计
        assert_eq!(k.audit.for_secret(&id).len(), 3);
    }

    #[test]
    fn kvs_rotate_missing_returns_error() {
        let mut k = MemKvs::default();
        let id = SecretId::new("nope");
        let err = k.rotate(&id, b"x", ts(1)).unwrap_err();
        assert!(matches!(err, ServiceError::SecretNotFound(_)));
    }

    #[test]
    fn kvs_delete_idempotent_and_audited() {
        let mut k = MemKvs::default();
        let id = SecretId::new("tok");
        k.store(id.clone(), b"v", ts(1));
        k.delete(&id, ts(2)).unwrap();
        // 再次删除幂等成功，但审计记录 success=false
        k.delete(&id, ts(3)).unwrap();
        let recs = k.audit.for_secret(&id);
        assert_eq!(recs.len(), 3); // store + delete(ok) + delete(notfound)
        assert!(recs[1].success);
        assert!(!recs[2].success);
        assert!(!k.entries.contains_key(&id));
    }

    // ---- Git 模型序列化往返 ----

    #[test]
    fn repo_spec_roundtrip_serde() {
        let spec = RepoSpec {
            name: "os-core".into(),
            url: "https://example.com/os-core".into(),
            default_branch: "main".into(),
            credential: Some(SecretId::new("deploy_key")),
        };
        let json = serde_json::to_string(&spec).unwrap();
        let back: RepoSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, spec.name);
        assert_eq!(back.default_branch, "main");
        assert_eq!(
            back.credential.as_ref().map(|c| c.as_str()),
            Some("deploy_key")
        );
    }

    #[test]
    fn commit_and_branch_serde() {
        let c = Commit {
            sha: "abc123".into(),
            author: "Alice".into(),
            author_email: "a@example.com".into(),
            message: "fix".into(),
            committed_at: ts(3),
        };
        let b = Branch {
            name: "main".into(),
            head: "abc123".into(),
            upstream: Some("origin/main".into()),
        };
        let cj = serde_json::to_string(&c).unwrap();
        let bj = serde_json::to_string(&b).unwrap();
        let cb: Commit = serde_json::from_str(&cj).unwrap();
        let bb: Branch = serde_json::from_str(&bj).unwrap();
        assert_eq!(cb.sha, "abc123");
        assert_eq!(bb.upstream.as_deref(), Some("origin/main"));
    }
}
