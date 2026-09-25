//! 统一任务框架 v1（审查高价值 #3，v0.1.54）—— 四套任务面的一处聚合。
//!
//! # 定位：轻旁路登记表，不动四套既有结构
//!
//! NexOS 里长任务散在四个组件、四套任务结构（**全部保留不动**，本模块
//! 只做旁路双写）：
//!
//! | kind | 组件 | 既有任务面 | 持久化（不变） | 接入点 |
//! |------|------|-----------|---------------|--------|
//! | `apps` | apps_handler | `AppInstallTask` 内存 Vec | apps.db `apps` 表（应用清单） | [`crate::handlers::apps_handler::AppRegistry`] `record_task` |
//! | `iso` | provisioning | `IsoTask` 内存 Vec | 无（重启即丢） | POST `/iso/tasks` + build 收尾 |
//! | `deploy` | provisioning | `DeployTask` 内存 Vec | 无（重启即丢） | POST `/ssh/deploy` + `finish_deploy` |
//! | `ci` | nexhub_ci | `CiRun` SQLite `ci_runs` | ci.db（已持久） | `CiCore::enqueue` / `run_one` |
//! | `llm_env` | llm_envs | `EnvTask` 内存 HashMap | llm.db `llm_environments`（环境行） | `register_task` / `task_finish` |
//!
//! 每处任务创建/终态时**双写**一行进本模块的 `unified_tasks` 表（SQLite
//! tasks.db）；登记失败仅 `eprintln!` 日志（**轻旁路**：聚合面绝不拖垮业务
//! 主路径——与 gw logs 裁剪「失败仅日志」同款铁律）。
//!
//! # 共享任务模型 [`UnifiedTask`]
//!
//! `{id, kind, status, label, stage, log_tail, output, created_at, finished_at}`。
//! status 五态归一：`queued | running | done | error | skipped`（四套各自
//! 状态串的映射见 [`UnifiedStatus::from_source`]）。`log_tail` 存环形日志
//! 尾（写侧裁到 [`LOG_TAIL_LINES`] 行）。
//!
//! # 聚合端点（组件 `tasks`）
//!
//! `GET /api/v1/tasks?kind=&status=&limit=50`（公开读）——跨四套统一列表，
//! 按登记序倒排（新任务在前）。`kind` 任意前缀过滤（开集合——将来新组件
//! 接入零改动）；`status` 校验五态（非法 400 如实报）；`limit` 缺省 50、
//! 上限 [`LIST_LIMIT_MAX`]。
//!
//! # 重启恢复语义（照 shotgen / blockchain_nodes 先例）
//!
//! - **历史可查**：done/error/skipped 行重启后仍在（`unified_tasks` 是磁盘
//!   SQLite），前端 `GET /api/v1/tasks` 重启前后行为一致。
//! - **悬挂行归谬**：进程重启后 `queued`/`running` 行必已死——构造时一律置
//!   `error`（error_msg「进程重启，任务中断」+ finished_at 补记），绝不留
//!   永假 running（照 llm.rs「服务重启后运行态一律重置」同口径）。
//! - **保留清理**：终态行保留 `NEXOS_TASKS_RETAIN_DAYS` 天（缺省 30，0=关
//!   闭；照 gw logs `NEXOS_GW_LOG_RETAIN_DAYS` 先例），**惰性触发**：登记
//!   与查询路径都调 [`UnifiedTaskStore::prune_if_due`]，同一节流窗
//!   （[`PRUNE_MIN_INTERVAL`]，1h）内只执行一轮 DELETE。
//!
//! # env 清单（`NEXOS_` 前缀）
//!
//! - `NEXOS_TASKS_DB`：SQLite 路径（缺省 `/tank/os-data/tasks.db`）。
//! - `NEXOS_TASKS_RETAIN_DAYS`：终态行保留天数（缺省 30，0=关闭）。
//!
//! # 局限（v1 明确不做）
//!
//! 不强行合并四套结构、不做跨组件取消/重试、不提供逐任务日志流（各组件
//! 自有详情端点）；`unified_tasks` 只做**登记 + 聚合视图**。

use std::path::Path;
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

/// unified_tasks SQLite 路径 env（缺省 `/tank/os-data/tasks.db`）。
pub const ENV_TASKS_DB: &str = "NEXOS_TASKS_DB";

/// 终态行保留天数 env（缺省 30，0=关闭；照 gw logs 先例）。
pub const ENV_TASKS_RETAIN_DAYS: &str = "NEXOS_TASKS_RETAIN_DAYS";

/// unified_tasks 缺省库路径（与 apps.db 同目录——os-data 是系统数据根）。
pub const DEFAULT_TASKS_DB: &str = "/tank/os-data/tasks.db";

/// 保留天数缺省值（30 天）。
pub const RETAIN_DAYS_DEFAULT: u64 = 30;

/// log_tail 保留行数上限（环形尾部快照；完整日志在各自详情端点）。
pub const LOG_TAIL_LINES: usize = 50;

/// 聚合端点缺省 limit。
pub const LIST_LIMIT_DEFAULT: usize = 50;

/// 聚合端点 limit 上限（防一次拉全表）。
pub const LIST_LIMIT_MAX: usize = 500;

/// 保留裁剪节流窗（同一窗内只跑一轮 DELETE；测试注 `Duration::ZERO` 强制）。
pub const PRUNE_MIN_INTERVAL: Duration = Duration::from_secs(3600);

// ----------------------------------------------------------------------------
// 状态模型：五态归一
// ----------------------------------------------------------------------------

/// 统一任务状态（四套状态串的归一五态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UnifiedStatus {
    Queued,
    Running,
    Done,
    Error,
    Skipped,
}

impl UnifiedStatus {
    /// 归一映射（四套状态串 → 五态；未知串归 Error——登记面只吃已知串，
    /// 未知即数据异常，Error 如实呈现）。
    #[must_use]
    pub fn from_source(kind: &str, raw: &str) -> Self {
        let r = raw.trim();
        match (kind, r) {
            // apps：completed | failed
            ("apps", "completed") => Self::Done,
            ("apps", "failed") => Self::Error,
            // iso：pending | building | completed | failed
            ("iso", "pending") => Self::Queued,
            ("iso", "building") => Self::Running,
            ("iso", "completed") => Self::Done,
            ("iso", "failed") => Self::Error,
            // deploy：pending | transferring | running | completed | failed
            ("deploy", "pending") => Self::Queued,
            ("deploy", "transferring") | ("deploy", "running") => Self::Running,
            ("deploy", "completed") => Self::Done,
            ("deploy", "failed") => Self::Error,
            // ci：queued | running | passed | failed | skipped
            ("ci", "queued") => Self::Queued,
            ("ci", "running") => Self::Running,
            ("ci", "passed") => Self::Done,
            ("ci", "failed") => Self::Error,
            ("ci", "skipped") => Self::Skipped,
            // llm_env：running | done | error
            ("llm_env", "running") => Self::Running,
            ("llm_env", "done") => Self::Done,
            ("llm_env", "error") => Self::Error,
            // 泛化兜底（新组件未登记专属映射时也能吃标准五态）
            (_, "queued") => Self::Queued,
            (_, "running") => Self::Running,
            (_, "done") => Self::Done,
            (_, "error") => Self::Error,
            (_, "skipped") => Self::Skipped,
            _ => Self::Error,
        }
    }

    /// 五态字符串（DB 存储与 API 输出同款）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Done => "done",
            Self::Error => "error",
            Self::Skipped => "skipped",
        }
    }

    /// 是否终态（保留清理只删终态行；非终态行重启时由悬挂归谬处理）。
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Error | Self::Skipped)
    }
}

// ----------------------------------------------------------------------------
// 共享任务模型
// ----------------------------------------------------------------------------

/// 统一任务行（unified_tasks 表一行 / 聚合端点元素）。
#[derive(Debug, Clone, Serialize)]
pub struct UnifiedTask {
    /// 源任务 id（各套原样：`app-task-3` / `iso-101` / `deploy-102` /
    /// `r1695…-000001-456` / `envtask-2`）。与 kind 联合主键（不同套 id
    /// 规则不同，不假设跨套唯一）。
    pub id: String,
    /// 任务族：`apps` / `iso` / `deploy` / `ci` / `llm_env`（开集合）。
    pub kind: String,
    /// 归一状态（queued | running | done | error | skipped）。
    pub status: UnifiedStatus,
    /// 人读标签（如「安装应用 demo-app」/ 仓库名 / ISO 任务名）。
    pub label: String,
    /// 当前阶段（running 时有意义：`building` / `transferring` / `create`…；
    /// 终态为 None 不出字段）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    /// 日志尾快照（环形裁剪到 [`LOG_TAIL_LINES`] 行；完整日志走各自详情端点）。
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub log_tail: String,
    /// 终态输出摘要（iso_path / action / exit_code 等一行事实）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// 创建时间（ISO 8601，本地时区——apps_handler now_iso 同款）。
    pub created_at: String,
    /// 结束时间（终态补记；未完成为 None 不出字段）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// 失败原因（error 时；其他态为 None 不出字段）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 当前 epoch 毫秒（保留清理 cutoff 用；ci_runs now_ms 同款）。
fn now_ms() -> i64 {
    chrono::Local::now().timestamp_millis()
}

/// 当前本地时区 ISO 8601（apps_handler now_iso 同款格式）。
fn now_iso() -> String {
    chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

/// 日志环形尾部：只留最后 [`LOG_TAIL_LINES`] 行（登记侧裁剪，DB 不存全量）。
#[must_use]
pub fn clip_log_tail(log: &str) -> String {
    let lines: Vec<&str> = log.lines().collect();
    if lines.len() <= LOG_TAIL_LINES {
        return log.trim_end_matches('\n').to_string();
    }
    lines[lines.len() - LOG_TAIL_LINES..].join("\n")
}

// ----------------------------------------------------------------------------
// UnifiedTaskStore：SQLite 登记表（双写目标 + 聚合查询源）
// ----------------------------------------------------------------------------

/// 统一任务登记表（`Arc` 共享：main.rs 构造一次，注入四套任务生产者 +
/// `tasks` 聚合 handler）。
///
/// 全部写方法**轻旁路**：任何 SQLite 失败仅 `eprintln!` 并返回，绝不向上
/// 传播错误（业务主路径零感知）。
pub struct UnifiedTaskStore {
    db: Mutex<Connection>,
    /// 上轮保留裁剪时刻（节流窗内跳过；gw logs `log_pruned_at` 同款）。
    pruned_at: Mutex<Option<std::time::Instant>>,
}

impl UnifiedTaskStore {
    /// 生产构造（env 路径；打开失败降级内存库不挡启动——apps_handler 同款）。
    #[must_use]
    pub fn new() -> Self {
        let db_path = std::env::var(ENV_TASKS_DB)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_TASKS_DB.to_string());
        Self::with_path(&db_path)
    }

    /// 指定路径构造（测试注入临时目录隔离）。
    ///
    /// 构造即完成两件重启语义（见模块头）：悬挂行归谬 + 建表幂等。
    /// 保留裁剪不在此跑（惰性——首个登记/查询触发）。
    #[must_use]
    pub fn with_path(db_path: &str) -> Self {
        if let Some(parent) = Path::new(db_path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = match Connection::open(db_path) {
            Ok(c) => {
                if let Err(e) = Self::create_schema(&c) {
                    eprintln!("[tasks] 建表失败（{db_path}）: {e}");
                }
                c
            }
            Err(e) => {
                eprintln!(
                    "[tasks] 打开 SQLite {db_path} 失败（{e}），降级到内存库（登记重启即丢）"
                );
                let c = Connection::open_in_memory().expect("内存库必成功");
                let _ = Self::create_schema(&c);
                c
            }
        };
        // 防 SQLITE_BUSY 立败（审计 E#6 同款）
        let _ = conn.busy_timeout(Duration::from_millis(3000));
        let store = Self {
            db: Mutex::new(conn),
            pruned_at: Mutex::new(None),
        };
        store.recover_dangling();
        store
    }

    /// 建表（幂等）。created_ms/finished_ms 为 epoch 毫秒（保留清理 cutoff
    /// 数学用）；created_at/finished_at 为 ISO 展示串（对外契约）。
    fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS unified_tasks (
                id          TEXT NOT NULL,
                kind        TEXT NOT NULL,
                status      TEXT NOT NULL,
                label       TEXT NOT NULL DEFAULT '',
                stage       TEXT,
                log_tail    TEXT NOT NULL DEFAULT '',
                output      TEXT,
                error       TEXT,
                created_at  TEXT NOT NULL,
                finished_at TEXT,
                created_ms  INTEGER NOT NULL,
                finished_ms INTEGER,
                PRIMARY KEY (kind, id)
            );
            CREATE INDEX IF NOT EXISTS idx_unified_tasks_kind_ms
                ON unified_tasks(kind, created_ms);
            CREATE INDEX IF NOT EXISTS idx_unified_tasks_ms
                ON unified_tasks(created_ms);
            ",
        )
    }

    /// 重启悬挂归谬：queued/running 行 → error（「进程重启，任务中断」+
    /// finished 补记）。构造时一次；失败仅日志（不动任何行也比 panic 强）。
    fn recover_dangling(&self) {
        let Ok(conn) = self.db.lock() else {
            return;
        };
        match conn.execute(
            "UPDATE unified_tasks SET status='error',
                error='进程重启，任务中断', finished_at=?1, finished_ms=?2
             WHERE status IN ('queued','running')",
            params![now_iso(), now_ms()],
        ) {
            Ok(n) if n > 0 => {
                eprintln!("[tasks] 重启悬挂归谬：{n} 行 queued/running 置 error（进程重启）");
            }
            Ok(_) => {}
            Err(e) => eprintln!("[tasks] 重启悬挂归谬失败（忽略）: {e}"),
        }
    }

    /// 登记/覆盖一行（INSERT OR REPLACE——双写入口：创建时整行 upsert，
    /// 幂等可重放）。轻旁路：失败仅日志。
    pub fn register(&self, task: &UnifiedTask) {
        let created_ms = now_ms();
        let created_at = if task.created_at.is_empty() {
            now_iso()
        } else {
            task.created_at.clone()
        };
        let Ok(conn) = self.db.lock() else {
            eprintln!("[tasks] 登记锁失败（kind={} id={}）", task.kind, task.id);
            return;
        };
        if let Err(e) = conn.execute(
            "INSERT OR REPLACE INTO unified_tasks
                (id, kind, status, label, stage, log_tail, output, error,
                 created_at, finished_at, created_ms, finished_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                task.id,
                task.kind,
                task.status.as_str(),
                task.label,
                task.stage,
                clip_log_tail(&task.log_tail),
                task.output,
                task.error,
                created_at,
                task.finished_at,
                created_ms,
                task.finished_at.as_ref().map(|_| now_ms()),
            ],
        ) {
            eprintln!(
                "[tasks] 登记失败（忽略；kind={} id={}）: {e}",
                task.kind, task.id
            );
            return;
        }
        drop(conn);
        self.prune_if_due();
    }

    /// 阶段推进（running 中间态：stage/label/log_tail 可更新；终态字段不动）。
    /// 不存在的行静默跳过（登记失败后的推进不该再报错——轻旁路）。
    pub fn progress(&self, kind: &str, id: &str, stage: Option<&str>, log_tail: &str) {
        let Ok(conn) = self.db.lock() else {
            return;
        };
        let _ = conn.execute(
            "UPDATE unified_tasks SET status='running', stage=?3, log_tail=?4
             WHERE kind=?1 AND id=?2",
            params![kind, id, stage, clip_log_tail(log_tail)],
        );
    }

    /// 终态落表（done/error/skipped：status + output/error + finished 补记）。
    /// 幂等（重复终态覆盖）；行不存在时补插一行（label 未知给空——登记
    /// 失败漏行时终态仍可见）。
    pub fn finish(
        &self,
        kind: &str,
        id: &str,
        status: UnifiedStatus,
        label: &str,
        output: Option<&str>,
        error: Option<&str>,
    ) {
        let now = now_iso();
        let nowm = now_ms();
        let Ok(conn) = self.db.lock() else {
            eprintln!("[tasks] 终态落表锁失败（kind={kind} id={id}）");
            return;
        };
        let updated = conn
            .execute(
                "UPDATE unified_tasks SET status=?3, output=?4, error=?5,
                    finished_at=?6, finished_ms=?7, stage=NULL
                 WHERE kind=?1 AND id=?2",
                params![kind, id, status.as_str(), output, error, now, nowm],
            )
            .unwrap_or(0);
        if updated == 0 {
            // 行不存在（登记曾失败 / 直接终态记录如 apps 同步安装）→ 补插
            if let Err(e) = conn.execute(
                "INSERT OR REPLACE INTO unified_tasks
                    (id, kind, status, label, output, error,
                     created_at, finished_at, created_ms, finished_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?9)",
                params![id, kind, status.as_str(), label, output, error, now, now, nowm],
            ) {
                eprintln!("[tasks] 终态补插失败（忽略；kind={kind} id={id}）: {e}");
                return;
            }
        }
        drop(conn);
        self.prune_if_due();
    }

    /// 聚合查询（kind/status 可选过滤；按登记序倒排，新在前）。
    pub fn list(&self, kind: Option<&str>, status: Option<&str>, limit: usize) -> Vec<UnifiedTask> {
        self.list_at(kind, status, limit)
    }

    /// [`Self::list`] 的可注入实现（测试直接构造旧 created_ms 行验保留）。
    fn list_at(&self, kind: Option<&str>, status: Option<&str>, limit: usize) -> Vec<UnifiedTask> {
        let Ok(conn) = self.db.lock() else {
            return vec![];
        };
        let mut sql = String::from(
            "SELECT id, kind, status, label, stage, log_tail, output, error,
                    created_at, finished_at FROM unified_tasks",
        );
        let mut conds: Vec<&str> = vec![];
        if kind.is_some() {
            conds.push("kind = ?1");
        }
        if status.is_some() {
            conds.push("status = ?2");
        }
        if !conds.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conds.join(" AND "));
        }
        sql.push_str(" ORDER BY created_ms DESC, rowid DESC LIMIT ?3");
        let Ok(mut stmt) = conn.prepare(&sql) else {
            return vec![];
        };
        // 列序（与上方 SELECT 一一对应）：
        // 0 id, 1 kind, 2 status, 3 label, 4 stage, 5 log_tail, 6 output,
        // 7 error, 8 created_at, 9 finished_at
        let map = |r: &rusqlite::Row<'_>| -> rusqlite::Result<UnifiedTask> {
            Ok(UnifiedTask {
                id: r.get(0)?,
                kind: r.get(1)?,
                status: UnifiedStatus::from_source(
                    &r.get::<_, String>(1)?,
                    &r.get::<_, String>(2)?,
                ),
                label: r.get(3)?,
                stage: r.get(4)?,
                log_tail: r.get(5)?,
                output: r.get(6)?,
                error: r.get(7)?,
                created_at: r.get(8)?,
                finished_at: r.get(9)?,
            })
        };
        let rows = match (kind, status) {
            (Some(k), Some(s)) => stmt.query_map(params![k, s, limit as i64], map),
            (Some(k), None) => stmt.query_map(params![k, "", limit as i64], map),
            (None, Some(s)) => stmt.query_map(params!["", s, limit as i64], map),
            (None, None) => stmt.query_map(params!["", "", limit as i64], map),
        };
        match rows {
            Ok(it) => it.filter_map(Result::ok).collect(),
            Err(_) => vec![],
        }
    }

    /// 终态行总数（聚合端点 total 字段；轻量 COUNT）。
    pub fn total(&self) -> usize {
        let Ok(conn) = self.db.lock() else {
            return 0;
        };
        conn.query_row("SELECT COUNT(*) FROM unified_tasks", [], |r| {
            r.get::<_, i64>(0)
        })
        .map(|n| n.max(0) as usize)
        .unwrap_or(0)
    }

    /// 保留裁剪（惰性 + 节流）：删**终态**且 finished_ms 早于
    /// `now - NEXOS_TASKS_RETAIN_DAYS` 的行。0=关闭。失败仅日志。
    ///
    /// `min_interval` 供测试注入（`Duration::ZERO` 强制触发）；生产走
    /// [`PRUNE_MIN_INTERVAL`]，由 `register`/`finish`/`list` 路径调用。
    pub fn prune_if_due(&self) {
        self.prune_with_interval(PRUNE_MIN_INTERVAL);
    }

    /// [`Self::prune_if_due`] 的核心体（节流窗参数化，gw logs
    /// `prune_logs_locked` 同款结构）。
    pub fn prune_with_interval(&self, min_interval: Duration) {
        let retain = parse_retain_days(std::env::var(ENV_TASKS_RETAIN_DAYS).ok().as_deref());
        self.prune_with(retain, min_interval);
    }

    /// 保留裁剪内核（retain 参数化——测试注入显式天数，不碰进程全局 env
    /// 防并行用例互踩；`parse_retain_days` 的 env 语义单测另验）。
    fn prune_with(&self, retain: u64, min_interval: Duration) {
        {
            let mut last = self.pruned_at.lock().expect("prune state poisoned");
            if last.is_some_and(|t| t.elapsed() < min_interval) {
                return;
            }
            // 先记时刻再删：DELETE 失败也不在每次调用上紧贴重试
            *last = Some(std::time::Instant::now());
        }
        if retain == 0 {
            return; // 显式关闭（保留全部）
        }
        let cutoff_ms = now_ms() - i64::try_from(retain).unwrap_or(i64::MAX) * 86_400_000;
        let Ok(conn) = self.db.lock() else {
            return;
        };
        match conn.execute(
            "DELETE FROM unified_tasks
             WHERE status IN ('done','error','skipped') AND finished_ms IS NOT NULL
               AND finished_ms < ?1",
            params![cutoff_ms],
        ) {
            Ok(n) if n > 0 => {
                eprintln!("[tasks] 保留裁剪：删除 {n} 行过期终态任务（retain={retain}d）");
            }
            Ok(_) => {}
            Err(e) => eprintln!("[tasks] 保留裁剪失败（忽略）: {e}"),
        }
    }

    /// 手动改 finished_ms（仅测试用——造过期行验保留清理）。
    #[cfg(test)]
    fn test_backdate(&self, kind: &str, id: &str, finished_ms: i64) {
        let conn = self.db.lock().expect("tasks db lock");
        conn.execute(
            "UPDATE unified_tasks SET finished_ms=?3 WHERE kind=?1 AND id=?2",
            params![kind, id, finished_ms],
        )
        .expect("backdate");
    }
}

impl Default for UnifiedTaskStore {
    fn default() -> Self {
        Self::new()
    }
}

/// 解析保留天数 env（纯函数可单测；解析失败回落缺省 30——不猜更激进值，
/// `parse_log_retain_days` 同款）。
#[must_use]
pub fn parse_retain_days(raw: Option<&str>) -> u64 {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => RETAIN_DAYS_DEFAULT,
        Some(s) => s.parse::<u64>().unwrap_or(RETAIN_DAYS_DEFAULT),
    }
}

// ----------------------------------------------------------------------------
// UnifiedTasksRouteHandler：聚合端点
// ----------------------------------------------------------------------------

/// 统一任务聚合路由处理器（组件 `tasks`）：`GET /api/v1/tasks`。
pub struct UnifiedTasksRouteHandler {
    pub store: Arc<UnifiedTaskStore>,
}

impl UnifiedTasksRouteHandler {
    /// 生产构造（main.rs：与四套生产者共享同一 store 实例）。
    #[must_use]
    pub fn new(store: Arc<UnifiedTaskStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl RouteHandler for UnifiedTasksRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![RouteSpec {
            method: HttpMethod::Get,
            path: "/api/v1/tasks".to_string(),
            handler_component: "tasks".to_string(),
            requires_auth: false,
            required_roles: vec![],
        }]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            // —— GET /api/v1/tasks?kind=&status=&limit= —— 跨四套统一列表
            (HttpMethod::Get, ["api", "v1", "tasks"]) => {
                let query = query_params(&req.path);
                let kind = query.get("kind").map(String::as_str).filter(|k| !k.is_empty());
                let status = query
                    .get("status")
                    .map(String::as_str)
                    .filter(|s| !s.is_empty());
                if let Some(s) = status {
                    // 五态校验（非法 400 如实报——聚合契约不给空列表装成功）
                    let valid =
                        ["queued", "running", "done", "error", "skipped"].contains(&s);
                    if !valid {
                        return Ok(ApiResponse {
                            status: 400,
                            body: serde_json::json!({
                                "error": format!(
                                    "status 非法: {s}（合法值 queued|running|done|error|skipped）"
                                )
                            }),
                            headers: serde_json::json!({}),
                        });
                    }
                }
                let limit = query
                    .get("limit")
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(LIST_LIMIT_DEFAULT)
                    .clamp(1, LIST_LIMIT_MAX);
                let tasks = self.store.list(kind, status, limit);
                // 惰性裁剪挂查询路径（读也顺手触发；节流窗内零成本）
                self.store.prune_if_due();
                Ok(ApiResponse {
                    status: 200,
                    body: serde_json::json!({
                        "tasks": tasks,
                        "total": self.store.total(),
                        "limit": limit,
                    }),
                    headers: serde_json::json!({}),
                })
            }
            _ => Ok(ApiResponse {
                status: 404,
                body: serde_json::json!({"error": "tasks: 未匹配的路由"}),
                headers: serde_json::json!({}),
            }),
        }
    }
}

// ----------------------------------------------------------------------------
// 内部辅助（app_store 同款小工具）
// ----------------------------------------------------------------------------

fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

/// 解析 query string 为 HashMap（仅 key=value，重复取最后一个）。
fn query_params(path: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    if let Some(q) = path.split('?').nth(1) {
        for pair in q.split('&') {
            let mut it = pair.splitn(2, '=');
            if let Some(k) = it.next() {
                if k.is_empty() {
                    continue;
                }
                out.insert(k.to_string(), it.next().unwrap_or("").to_string());
            }
        }
    }
    out
}

// ----------------------------------------------------------------------------
// 单元测试（全部临时目录/内存库隔离，不碰真实 /tank）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn store_at(name: &str) -> (Arc<UnifiedTaskStore>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "nexos-tasks-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let s = Arc::new(UnifiedTaskStore::with_path(
            dir.join("tasks.db").to_str().unwrap(),
        ));
        (s, dir)
    }

    fn task(kind: &str, id: &str, status: UnifiedStatus, label: &str) -> UnifiedTask {
        UnifiedTask {
            id: id.into(),
            kind: kind.into(),
            status,
            label: label.into(),
            stage: None,
            log_tail: String::new(),
            output: None,
            created_at: String::new(),
            finished_at: None,
            error: None,
        }
    }

    // ---- 状态归一映射 ----

    #[test]
    fn status_mapping_covers_four_sources() {
        // apps
        assert_eq!(UnifiedStatus::from_source("apps", "completed"), UnifiedStatus::Done);
        assert_eq!(UnifiedStatus::from_source("apps", "failed"), UnifiedStatus::Error);
        // iso
        assert_eq!(UnifiedStatus::from_source("iso", "pending"), UnifiedStatus::Queued);
        assert_eq!(UnifiedStatus::from_source("iso", "building"), UnifiedStatus::Running);
        // deploy
        assert_eq!(
            UnifiedStatus::from_source("deploy", "transferring"),
            UnifiedStatus::Running
        );
        assert_eq!(UnifiedStatus::from_source("deploy", "running"), UnifiedStatus::Running);
        // ci
        assert_eq!(UnifiedStatus::from_source("ci", "queued"), UnifiedStatus::Queued);
        assert_eq!(UnifiedStatus::from_source("ci", "passed"), UnifiedStatus::Done);
        assert_eq!(UnifiedStatus::from_source("ci", "skipped"), UnifiedStatus::Skipped);
        // llm_env
        assert_eq!(UnifiedStatus::from_source("llm_env", "done"), UnifiedStatus::Done);
        assert_eq!(UnifiedStatus::from_source("llm_env", "error"), UnifiedStatus::Error);
        // 泛化五态 + 未知归 Error
        assert_eq!(UnifiedStatus::from_source("future", "queued"), UnifiedStatus::Queued);
        assert_eq!(UnifiedStatus::from_source("ci", "???"), UnifiedStatus::Error);
        // 终态判定
        assert!(UnifiedStatus::Done.is_terminal());
        assert!(!UnifiedStatus::Running.is_terminal());
    }

    // ---- 双写一致：register → finish → list 同源可见 ----

    #[test]
    fn register_progress_finish_roundtrip() {
        let (s, _dir) = store_at("roundtrip");
        // 创建登记（ci 入队）
        s.register(&task("ci", "r1", UnifiedStatus::Queued, "nexhub 仓 CI"));
        // 推进 running
        s.progress("ci", "r1", Some("cargo check"), "$ cargo check\nrunning");
        // 终态 done
        s.finish("ci", "r1", UnifiedStatus::Done, "nexhub 仓 CI", Some("exit=0"), None);
        let rows = s.list(Some("ci"), None, 50);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, UnifiedStatus::Done);
        assert_eq!(rows[0].output.as_deref(), Some("exit=0"));
        assert!(rows[0].finished_at.is_some(), "终态补记 finished_at");
        assert!(rows[0].stage.is_none(), "终态清 stage");
    }

    #[test]
    fn finish_without_register_backfills_row() {
        // apps 同步安装直接落终态（无前置 queued/running 登记）→ 补插可见
        let (s, _dir) = store_at("backfill");
        s.finish(
            "apps",
            "app-task-1",
            UnifiedStatus::Done,
            "安装应用 demo",
            Some("install 0.1.0"),
            None,
        );
        let rows = s.list(Some("apps"), None, 50);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, UnifiedStatus::Done);
    }

    #[test]
    fn register_is_idempotent_upsert() {
        // 同 (kind,id) 重复登记 = 覆盖不报错（双写幂等可重放）
        let (s, _dir) = store_at("upsert");
        s.register(&task("iso", "iso-101", UnifiedStatus::Queued, "std"));
        s.register(&task("iso", "iso-101", UnifiedStatus::Queued, "std v2"));
        assert_eq!(s.list(Some("iso"), None, 50).len(), 1);
        assert_eq!(s.list(None, None, 50)[0].label, "std v2");
    }

    #[test]
    fn log_tail_clipped_to_ring_lines() {
        let (s, _dir) = store_at("clip");
        let long = (0..200).map(|i| format!("line-{i}")).collect::<Vec<_>>().join("\n");
        let mut t = task("ci", "r9", UnifiedStatus::Running, "x");
        t.log_tail = long;
        s.register(&t);
        let stored = s.list(Some("ci"), None, 50)[0].log_tail.clone();
        let lines = stored.lines().count();
        assert_eq!(lines, LOG_TAIL_LINES, "裁到环形上限");
        assert!(stored.contains("line-199"), "保尾部丢头部");
        assert!(!stored.contains("line-0"), "最旧行已丢");
    }

    // ---- 聚合过滤 ----

    #[test]
    fn aggregate_filters_kind_status_and_limit() {
        let (s, _dir) = store_at("filter");
        s.register(&task("apps", "a1", UnifiedStatus::Done, "app1"));
        s.register(&task("ci", "r1", UnifiedStatus::Skipped, "ci1"));
        s.register(&task("ci", "r2", UnifiedStatus::Error, "ci2"));
        s.register(&task("iso", "i1", UnifiedStatus::Queued, "iso1"));

        // kind 过滤
        let ci = s.list(Some("ci"), None, 50);
        assert_eq!(ci.len(), 2);
        assert!(ci.iter().all(|t| t.kind == "ci"));
        // status 过滤
        let errs = s.list(None, Some("error"), 50);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].id, "r2");
        // 组合过滤
        assert_eq!(s.list(Some("ci"), Some("skipped"), 50).len(), 1);
        // limit 截断（总数 4，limit 2 → 2，且是最新的 2 条：倒排）
        let top2 = s.list(None, None, 2);
        assert_eq!(top2.len(), 2);
        assert_eq!(top2[0].id, "i1", "最新在前");
        // kind 不存在 → 空
        assert!(s.list(Some("nope"), None, 50).is_empty());
    }

    #[tokio::test]
    async fn endpoint_contract_query_and_errors() {
        let (s, _dir) = store_at("endpoint");
        s.register(&task("apps", "a1", UnifiedStatus::Done, "app1"));
        s.register(&task("ci", "r1", UnifiedStatus::Error, "ci1"));

        let h = UnifiedTasksRouteHandler::new(Arc::clone(&s));
        // 无过滤
        let resp = h
            .handle(req_get("/api/v1/tasks"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["total"], 2);
        assert_eq!(resp.body["tasks"].as_array().unwrap().len(), 2);
        assert_eq!(resp.body["limit"], LIST_LIMIT_DEFAULT);
        // kind 过滤
        let resp = h
            .handle(req_get("/api/v1/tasks?kind=ci"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["tasks"].as_array().unwrap().len(), 1);
        assert_eq!(resp.body["tasks"][0]["kind"], "ci");
        // status 过滤
        let resp = h
            .handle(req_get("/api/v1/tasks?status=done"))
            .await
            .unwrap();
        assert_eq!(resp.body["tasks"].as_array().unwrap().len(), 1);
        assert_eq!(resp.body["tasks"][0]["id"], "a1");
        // 非法 status → 400 如实报
        let resp = h
            .handle(req_get("/api/v1/tasks?status=weird"))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        assert!(resp.body["error"].as_str().unwrap().contains("非法"));
        // limit=1
        let resp = h
            .handle(req_get("/api/v1/tasks?limit=1"))
            .await
            .unwrap();
        assert_eq!(resp.body["tasks"].as_array().unwrap().len(), 1);
        // 未知路由 → 404
        let resp = h
            .handle(req_get("/api/v1/tasks/other"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // ---- 重启悬挂恢复 ----

    #[test]
    fn dangling_running_rows_recovered_as_error_on_reopen() {
        let (s, dir) = store_at("recover");
        s.register(&task("iso", "i1", UnifiedStatus::Running, "building 中"));
        s.register(&task("ci", "r1", UnifiedStatus::Queued, "排队中"));
        s.register(&task("apps", "a1", UnifiedStatus::Done, "已完成"));
        s.register(&task("ci", "r2", UnifiedStatus::Error, "已失败"));
        // 模拟重启：同一 DB 文件重开（触发 recover_dangling）
        let s2 = UnifiedTaskStore::with_path(dir.join("tasks.db").to_str().unwrap());
        let rows = s2.list(None, None, 50);
        // running/queued → error + 「进程重启」；done/error 原样
        let iso = rows.iter().find(|t| t.id == "i1").unwrap();
        assert_eq!(iso.status, UnifiedStatus::Error);
        assert_eq!(iso.error.as_deref(), Some("进程重启，任务中断"));
        assert!(iso.finished_at.is_some(), "悬挂行补记 finished_at");
        let ci1 = rows.iter().find(|t| t.id == "r1").unwrap();
        assert_eq!(ci1.status, UnifiedStatus::Error);
        let a1 = rows.iter().find(|t| t.id == "a1").unwrap();
        assert_eq!(a1.status, UnifiedStatus::Done, "终态行不受重启影响");
        let r2 = rows.iter().find(|t| t.id == "r2").unwrap();
        assert_eq!(r2.status, UnifiedStatus::Error);
    }

    // ---- 保留清理 ----

    #[test]
    fn retention_prunes_old_terminal_rows_only() {
        let (s, _dir) = store_at("retain");
        // 三行终态 + 一行 running（不该被保留清理动——悬挂由重启归谬管）
        s.finish("ci", "r-old", UnifiedStatus::Done, "旧 run", None, None);
        s.finish("ci", "r-keep", UnifiedStatus::Done, "新 run", None, None);
        s.finish("apps", "a-err", UnifiedStatus::Error, "旧失败", None, None);
        s.register(&task("iso", "i-live", UnifiedStatus::Running, "在建"));
        // r-old / a-err 回拨到 40 天前（过期），r-keep 保持现在
        let old_ms = now_ms() - 40 * 86_400_000;
        s.test_backdate("ci", "r-old", old_ms);
        s.test_backdate("apps", "a-err", old_ms);

        s.prune_with(30, Duration::ZERO); // 30 天保留，强制触发（绕节流窗）

        let ids: Vec<String> = s.list(None, None, 50).iter().map(|t| t.id.clone()).collect();
        assert!(!ids.contains(&"r-old".to_string()), "过期 done 已清");
        assert!(!ids.contains(&"a-err".to_string()), "过期 error 已清");
        assert!(ids.contains(&"r-keep".to_string()), "未过期终态保留");
        assert!(ids.contains(&"i-live".to_string()), "非终态行保留清理不动");
    }

    #[test]
    fn retention_throttle_and_disable() {
        let (s, _dir) = store_at("throttle");
        s.finish("ci", "r-old", UnifiedStatus::Done, "旧", None, None);
        s.test_backdate("ci", "r-old", now_ms() - 40 * 86_400_000);
        // 0=显式关闭：一行不删
        s.prune_with(0, Duration::ZERO);
        assert_eq!(s.list(None, None, 50).len(), 1, "retain=0 全保留");
        // 恢复 30 天；强制触发 → 过期行删除
        s.prune_with(30, Duration::ZERO);
        assert_eq!(s.list(None, None, 50).len(), 0, "过期行已清");
        // 再登记一行并回拨——节流窗（1h）内的惰性 prune 不再执行
        s.finish("ci", "r2", UnifiedStatus::Done, "第二行", None, None);
        s.test_backdate("ci", "r2", now_ms() - 40 * 86_400_000);
        s.register(&task("ci", "r3", UnifiedStatus::Queued, "触发惰性路径"));
        // r2（过期终态但节流窗内未裁）+ r3（非终态）= 2 行
        assert_eq!(s.list(None, None, 50).len(), 2, "节流窗内未再裁剪");
        // 窗归零后（强制）补删生效：r2 清、r3 留
        s.prune_with(30, Duration::ZERO);
        assert_eq!(s.list(None, None, 50).len(), 1, "绕过节流窗后删除");
        assert_eq!(s.list(None, None, 50)[0].id, "r3");
    }

    #[test]
    fn parse_retain_days_variants() {
        assert_eq!(parse_retain_days(None), 30, "缺省 30");
        assert_eq!(parse_retain_days(Some("")), 30, "空串回落缺省");
        assert_eq!(parse_retain_days(Some("7")), 7);
        assert_eq!(parse_retain_days(Some("0")), 0, "0=显式关闭");
        assert_eq!(parse_retain_days(Some("abc")), 30, "解析失败回落缺省");
        assert_eq!(parse_retain_days(Some(" 14 ")), 14, "容忍空白");
    }

    #[tokio::test]
    async fn routes_declares_single_public_endpoint() {
        let (s, _dir) = store_at("routes");
        let h = UnifiedTasksRouteHandler::new(s);
        let routes = h.routes().await;
        assert_eq!(routes.len(), 1, "{routes:?}");
        assert_eq!(routes[0].path, "/api/v1/tasks");
        assert_eq!(routes[0].handler_component, "tasks");
        assert!(!routes[0].requires_auth, "聚合面公开读");
    }

    fn req_get(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }
}
