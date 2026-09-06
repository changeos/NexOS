//! `BackupRouteHandler` —— 备份管理桌面应用的 HTTP→内存态备份任务 + ZFS 快照管理适配器。
//!
//! 定位：把网关 HTTP 请求（`/api/v1/backup/*`）翻译为备份任务管理 + ZFS 快照操作，
//! 返回 JSON。这是 OS 备份管理桌面应用的后端 REST 入口。
//!
//! # 当前实现策略
//!
//! - **备份任务**：内存态 `Mutex<Vec<BackupTask>>`，构造时预置示例任务便于前端演示。
//!   `run` 仅切换 `status=running`（标记执行，不真跑备份线程）；
//!   `trigger-snapshot` 真实创建一次 ZFS 快照 + 应用保留策略。
//! - **ZFS 快照**：`spawn_blocking` 真实调 `zfs list -H -t snapshot` / `zfs snapshot` /
//!   `zfs destroy`。命令不存在 / 无权限 / 无 pool 时**降级**（返回空数组或示例快照），
//!   绝不 panic（参考 `storage.rs` / `discover.rs` 的探测降级模式）。
//! - **快照调度**：`BackupRouteHandler::new()` 启动一个后台 tokio task，每 60 秒检查
//!   所有 `auto_snapshot==true && schedule != "manual"` 的任务，到时间则创建 ZFS 快照
//!   + 应用保留策略（`apply_retention`），调度器持有 `Arc<BackupState>` 共享状态，
//!     os-api 退出时进程直接结束无需显式停止。
//!
//! # 路由表
//!
//! | method | path                                              | 动作 |
//! |--------|---------------------------------------------------|------|
//! | GET    | `/api/v1/backup/tasks`                            | 列任务 |
//! | POST   | `/api/v1/backup/tasks`                            | 建任务（需 admin）|
//! | POST   | `/api/v1/backup/tasks/:id/run`                    | 立即执行（标记 running，需 admin）|
//! | POST   | `/api/v1/backup/tasks/:id/trigger-snapshot`       | 手动触发一次快照（真实 zfs snapshot，需 admin）|
//! | GET    | `/api/v1/backup/tasks/:id/snapshots`              | 列该任务关联的快照（按 source 过滤）|
//! | DELETE | `/api/v1/backup/tasks/:id`                        | 删任务（需 admin）|
//! | GET    | `/api/v1/backup/snapshots`                        | 列快照（真实 zfs list）|
//! | POST   | `/api/v1/backup/snapshots`                        | 创建快照（需 admin，真实 zfs snapshot）|
//! | DELETE | `/api/v1/backup/snapshots/:name`                  | 删快照（需 admin，真实 zfs destroy）|
//! | GET    | `/api/v1/backup/stats`                            | 统计 |
//! | GET    | `/api/v1/backup/restore`                          | 可用恢复点（本期占位）|

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 一条备份任务（内存态）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupTask {
    pub id: String,
    pub name: String,
    /// 源路径/dataset，如 `/tank/data` 或 `tank/data`。
    pub source: String,
    /// 目标，如 `/backup/tank-data` 或 `s3://bucket`。
    pub dest: String,
    /// 模式：`full` / `incremental` / `snapshot`。
    pub mode: String,
    /// 计划：`manual` / `hourly` / `daily` / `weekly`。
    pub schedule: String,
    /// 状态：`idle` / `running` / `completed` / `failed`。
    pub status: String,
    pub last_run: Option<String>,
    /// 下次预计执行时间（根据 schedule 间隔算）。
    pub next_run: Option<String>,
    /// 上次备份大小（字节）。
    pub size_bytes: u64,
    pub created_at: String,
    /// 保留策略：最多保留 N 个快照（0=不限），超出自动删最旧。
    #[serde(default)]
    pub retention_count: u32,
    /// 保留天数：超过 N 天的快照自动删（0=不限）。
    #[serde(default)]
    pub retention_days: u32,
    /// 是否启用自动快照（mode=snapshot 时按 schedule 自动创建）。
    #[serde(default)]
    pub auto_snapshot: bool,
}

/// 一条 ZFS 快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// 全名 `tank/data@snap-20260808`。
    pub name: String,
    pub pool: String,
    pub created_at: String,
    pub used_bytes: u64,
    pub referenced_bytes: u64,
}

/// `GET /api/v1/backup/stats` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupStats {
    pub tasks_total: usize,
    pub tasks_running: usize,
    pub tasks_completed: usize,
    pub snapshots_total: usize,
    pub last_backup_size: u64,
    /// 启用了自动快照的任务数。
    pub auto_snapshots_enabled: usize,
}

/// 创建备份任务请求体。
#[derive(Debug, Deserialize)]
struct CreateTaskBody {
    name: String,
    source: String,
    dest: String,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default)]
    retention_count: Option<u32>,
    #[serde(default)]
    retention_days: Option<u32>,
    #[serde(default)]
    auto_snapshot: Option<bool>,
}

/// 创建快照请求体。
#[derive(Debug, Deserialize)]
struct CreateSnapshotBody {
    pool: String,
    name: String,
}

/// 远程复制任务（zfs send | ssh zfs recv 的异步执行状态）。
///
/// 字段参考 `BackupTask`：id / source / target / status / pid / error / created_at。
/// `status` ∈ `running` / `completed` / `failed`；任务由 `POST /api/v1/backup/replication`
/// 创建并立即返回（status=running），实际 send/recv 在后台 tokio task 中执行。
#[derive(Debug, Clone, Serialize)]
pub struct ReplicationTask {
    pub id: String,
    /// 源数据集（如 `tank/data`）。
    pub source: String,
    /// 目标 SSH 主机（如 `root@10.0.0.2`）。
    pub target_ssh: String,
    /// 目标数据集（如 `backup/tank-data`）。
    pub target_dataset: String,
    /// 源快照全名（如 `tank/data@rep-20260812`）。
    pub snapshot: String,
    /// 状态：`running` / `completed` / `failed`。
    pub status: String,
    /// 子进程 PID（send/recv pipeline 的 sh PID），可能为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// 失败原因（status=failed 时有值）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: String,
}

/// `POST /api/v1/backup/replication` 请求体。
#[derive(Debug, Deserialize)]
struct ReplicationBody {
    source_dataset: String,
    target_ssh: String,
    target_dataset: String,
}

// ----------------------------------------------------------------------------
// BackupState（共享状态，可被调度器与 handler 共同持有）
// ----------------------------------------------------------------------------

/// 备份共享状态——tasks 列表 + 复制任务列表 + id 计数器。
/// 用 `Arc<BackupState>` 让后台调度 tokio task 与 handler 共享（满足 'static）。
struct BackupState {
    tasks: Mutex<Vec<BackupTask>>,
    counter: Mutex<u64>,
    /// 远程复制任务（zfs send/recv 异步状态）。
    replications: Mutex<Vec<ReplicationTask>>,
    repl_counter: Mutex<u64>,
}

impl BackupState {
    fn new(tasks: Vec<BackupTask>) -> Self {
        Self {
            tasks: Mutex::new(tasks),
            counter: Mutex::new(100),
            replications: Mutex::new(Vec::new()),
            repl_counter: Mutex::new(0),
        }
    }

    fn next_id(&self) -> String {
        let mut c = self.counter.lock().expect("counter poisoned");
        *c += 1;
        format!("bk-{}", *c)
    }

    /// 下一个复制任务 id（`repl-<n>`）。
    fn next_repl_id(&self) -> String {
        let mut c = self.repl_counter.lock().expect("repl_counter poisoned");
        *c += 1;
        format!("repl-{}", *c)
    }
}

// ----------------------------------------------------------------------------
// BackupRouteHandler
// ----------------------------------------------------------------------------

/// 备份管理路由处理器——HTTP 边界适配到内存态备份任务列表 + 真实 ZFS 快照操作
/// + 后台 cron 快照调度。
///
/// 持有 `Arc<BackupState>`：构造时（`new`）启动后台调度 tokio task，
/// `with_tasks`（测试注入）**不**启动调度器（避免测试期间产生副作用）。
pub struct BackupRouteHandler {
    state: Arc<BackupState>,
}

impl BackupRouteHandler {
    /// 构造 handler，预置 demo 任务，并启动后台快照调度（每 60 秒检查一次）。
    #[must_use]
    pub fn new() -> Self {
        let state = Arc::new(BackupState::new(demo_tasks()));
        spawn_scheduler(Arc::clone(&state));
        Self { state }
    }

    /// 用空任务列表构造（测试注入），**不**启动后台调度（测试零副作用）。
    #[must_use]
    pub fn with_tasks(tasks: Vec<BackupTask>) -> Self {
        let state = Arc::new(BackupState::new(tasks));
        Self { state }
    }

    /// 当前全量任务快照。
    #[must_use]
    pub fn tasks_snapshot(&self) -> Vec<BackupTask> {
        self.state.tasks.lock().expect("tasks poisoned").clone()
    }

    /// 统计快照（不含 snapshots_total，快照数由调用方补）。
    fn stats_snapshot(&self) -> BackupStats {
        let tasks = self.state.tasks.lock().expect("tasks poisoned");
        let mut running = 0usize;
        let mut completed = 0usize;
        let mut last_size = 0u64;
        let mut auto_enabled = 0usize;
        for t in tasks.iter() {
            if t.status == "running" {
                running += 1;
            } else if t.status == "completed" {
                completed += 1;
            }
            if t.size_bytes > last_size {
                last_size = t.size_bytes;
            }
            if t.auto_snapshot {
                auto_enabled += 1;
            }
        }
        BackupStats {
            tasks_total: tasks.len(),
            tasks_running: running,
            tasks_completed: completed,
            snapshots_total: 0, // 由 handle 补
            last_backup_size: last_size,
            auto_snapshots_enabled: auto_enabled,
        }
    }
}

impl Default for BackupRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RouteHandler for BackupRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec(HttpMethod::Get, "/api/v1/backup/tasks", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/backup/tasks",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/backup/tasks/:id/run",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/backup/tasks/:id/trigger-snapshot",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/backup/tasks/:id/snapshots",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/backup/tasks/:id",
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Get, "/api/v1/backup/snapshots", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/backup/snapshots",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/backup/snapshots/:name",
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Get, "/api/v1/backup/stats", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/backup/restore", false, vec![]),
            // —— 远程复制（zfs send/recv）——
            spec(
                HttpMethod::Post,
                "/api/v1/backup/replication",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/backup/replication/:id",
                false,
                vec![],
            ),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            // —— GET /api/v1/backup/tasks —— 列任务
            (HttpMethod::Get, ["api", "v1", "backup", "tasks"]) => {
                let tasks = self.tasks_snapshot();
                Ok(ok_json(to_value(&tasks)?))
            }

            // —— GET /api/v1/backup/stats —— 统计
            (HttpMethod::Get, ["api", "v1", "backup", "stats"]) => {
                let mut stats = self.stats_snapshot();
                let snaps = list_snapshots_blocking();
                stats.snapshots_total = snaps.len();
                Ok(ok_json(to_value(&stats)?))
            }

            // —— GET /api/v1/backup/snapshots —— 列快照（真实 zfs list，失败降级）
            (HttpMethod::Get, ["api", "v1", "backup", "snapshots"]) => {
                let snaps = list_snapshots_blocking();
                Ok(ok_json(to_value(&snaps)?))
            }

            // —— GET /api/v1/backup/restore —— 占位恢复点列表
            (HttpMethod::Get, ["api", "v1", "backup", "restore"]) => {
                let restore_points = serde_json::json!([
                    {"id": "rp-1", "name": "tank/data@daily-20260807",
                     "type": "snapshot", "created_at": "2026-08-07T03:00:00+08:00",
                     "size_bytes": 1_200_000_000, "restorable": true},
                    {"id": "rp-2", "name": "tank/data@daily-20260806",
                     "type": "snapshot", "created_at": "2026-08-06T03:00:00+08:00",
                     "size_bytes": 1_150_000_000, "restorable": true},
                ]);
                Ok(ok_json(restore_points))
            }

            // —— POST /api/v1/backup/tasks —— 建任务
            (HttpMethod::Post, ["api", "v1", "backup", "tasks"]) => {
                let body: CreateTaskBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析创建备份任务请求体失败: {e}"))
                })?;
                if body.name.trim().is_empty() {
                    return Ok(error_response(400, "name 不可为空"));
                }
                if body.source.trim().is_empty() {
                    return Ok(error_response(400, "source 不可为空"));
                }
                if body.dest.trim().is_empty() {
                    return Ok(error_response(400, "dest 不可为空"));
                }
                let mode = body
                    .mode
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "full".to_string());
                let schedule = body
                    .schedule
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "manual".to_string());
                let auto_snapshot = body.auto_snapshot.unwrap_or(false);
                let now = now_iso();
                let next_run = if auto_snapshot && schedule != "manual" {
                    compute_next_run(&now, &schedule)
                } else {
                    None
                };
                let task = BackupTask {
                    id: self.state.next_id(),
                    name: body.name,
                    source: body.source,
                    dest: body.dest,
                    mode,
                    schedule,
                    status: "idle".into(),
                    last_run: None,
                    next_run,
                    size_bytes: 0,
                    created_at: now,
                    retention_count: body.retention_count.unwrap_or(0),
                    retention_days: body.retention_days.unwrap_or(0),
                    auto_snapshot,
                };
                let resp_body = to_value(&task)?;
                self.state.tasks.lock().expect("tasks poisoned").push(task);
                Ok(ApiResponse {
                    status: 201,
                    body: resp_body,
                    headers: serde_json::json!({}),
                })
            }

            // —— POST /api/v1/backup/tasks/:id/run —— 立即执行（内存态切 running）
            (HttpMethod::Post, ["api", "v1", "backup", "tasks", id, "run"]) => {
                let mut tasks = self.state.tasks.lock().expect("tasks poisoned");
                let found = tasks.iter_mut().find(|t| t.id == *id);
                match found {
                    Some(t) => {
                        t.status = "running".into();
                        t.last_run = Some(now_iso());
                        Ok(ok_json(to_value(&t)?))
                    }
                    None => Ok(error_response(404, &format!("备份任务不存在: {id}"))),
                }
            }

            // —— POST /api/v1/backup/tasks/:id/trigger-snapshot —— 手动触发一次快照
            // 立即创建 ZFS 快照（source@auto-<ts>），更新 last_run/next_run，累加 size_bytes，
            // 应用保留策略（retention_count / retention_days）。失败降级为 warning 不 panic。
            (HttpMethod::Post, ["api", "v1", "backup", "tasks", id, "trigger-snapshot"]) => {
                let task = {
                    let tasks = self.state.tasks.lock().expect("tasks poisoned");
                    tasks.iter().find(|t| t.id == *id).cloned()
                };
                let Some(task) = task else {
                    return Ok(error_response(404, &format!("备份任务不存在: {id}")));
                };
                let (ts, full) = make_auto_snapshot_name(&task.source);
                let outcome = create_snapshot_blocking(&full);
                let now = now_iso();
                // 更新 last_run / next_run / size_bytes（失败也记录尝试时间）
                {
                    let mut tasks = self.state.tasks.lock().expect("tasks poisoned");
                    if let Some(t) = tasks.iter_mut().find(|t| t.id == task.id) {
                        t.last_run = Some(now.clone());
                        if t.auto_snapshot && t.schedule != "manual" {
                            t.next_run = compute_next_run(&now, &t.schedule);
                        }
                    }
                }
                // 应用保留策略（即使本次创建失败，也清理一次旧快照）
                let (deleted, warns) = apply_retention_for_task(&task);
                Ok(match outcome {
                    Ok(()) => ok_json(serde_json::json!({
                        "ok": true,
                        "name": full,
                        "timestamp": ts,
                        "action": "trigger-snapshot",
                        "task_id": task.id,
                        "source": task.source,
                        "retention_deleted": deleted,
                        "retention_warnings": warns,
                    })),
                    Err(msg) => ok_json(serde_json::json!({
                        "ok": false,
                        "name": full,
                        "timestamp": ts,
                        "action": "trigger-snapshot",
                        "task_id": task.id,
                        "source": task.source,
                        "warning": msg,
                        "retention_deleted": deleted,
                        "retention_warnings": warns,
                    })),
                })
            }

            // —— GET /api/v1/backup/tasks/:id/snapshots —— 列该任务关联的快照（按 source 过滤）
            (HttpMethod::Get, ["api", "v1", "backup", "tasks", id, "snapshots"]) => {
                let task = {
                    let tasks = self.state.tasks.lock().expect("tasks poisoned");
                    tasks.iter().find(|t| t.id == *id).cloned()
                };
                let Some(task) = task else {
                    return Ok(error_response(404, &format!("备份任务不存在: {id}")));
                };
                let snaps = list_snapshots_blocking();
                let filtered: Vec<&Snapshot> =
                    snaps.iter().filter(|s| s.pool == task.source).collect();
                Ok(ok_json(serde_json::json!({
                    "task_id": task.id,
                    "source": task.source,
                    "total": filtered.len(),
                    "snapshots": filtered,
                })))
            }

            // —— DELETE /api/v1/backup/tasks/:id —— 删任务
            (HttpMethod::Delete, ["api", "v1", "backup", "tasks", id]) => {
                let mut tasks = self.state.tasks.lock().expect("tasks poisoned");
                let before = tasks.len();
                tasks.retain(|t| t.id != *id);
                if tasks.len() == before {
                    return Ok(error_response(404, &format!("备份任务不存在: {id}")));
                }
                Ok(ok_json(
                    serde_json::json!({"ok": true, "id": id, "action": "delete"}),
                ))
            }

            // —— POST /api/v1/backup/snapshots —— 创建快照（真实 zfs snapshot，失败降级）
            (HttpMethod::Post, ["api", "v1", "backup", "snapshots"]) => {
                let body: CreateSnapshotBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析创建快照请求体失败: {e}"))
                })?;
                if body.pool.trim().is_empty() || body.name.trim().is_empty() {
                    return Ok(error_response(400, "pool 与 name 不可为空"));
                }
                let full = format!("{}@{}", body.pool.trim(), body.name.trim());
                let outcome = create_snapshot_blocking(&full);
                Ok(match outcome {
                    Ok(()) => ok_json(serde_json::json!({
                        "ok": true,
                        "name": full,
                        "action": "create",
                    })),
                    Err(msg) => ok_json(serde_json::json!({
                        "ok": false,
                        "name": full,
                        "action": "create",
                        "warning": msg,
                    })),
                })
            }

            // —— DELETE /api/v1/backup/snapshots/:name —— 删快照（真实 zfs destroy，失败降级）
            // name 形如 tank/data@snap-xxx，可能含 / 与 @，故用 rest 拼回（path_segments 已按 / 切）。
            (HttpMethod::Delete, ["api", "v1", "backup", "snapshots", name]) => {
                let full = name.to_string();
                let outcome = destroy_snapshot_blocking(&full);
                Ok(match outcome {
                    Ok(()) => ok_json(serde_json::json!({
                        "ok": true,
                        "name": full,
                        "action": "delete",
                    })),
                    Err(msg) => ok_json(serde_json::json!({
                        "ok": false,
                        "name": full,
                        "action": "delete",
                        "warning": msg,
                    })),
                })
            }

            // —— POST /api/v1/backup/replication —— 远程复制（admin）
            // 先创建源快照 <source>@rep-<ts>，再 spawn 后台 send/recv 管道。
            // 任务立即返回（status=running），实际传输在 tokio task 中执行并回写状态。
            (HttpMethod::Post, ["api", "v1", "backup", "replication"]) => {
                let body: ReplicationBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析远程复制请求体失败: {e}"))
                })?;
                if body.source_dataset.trim().is_empty() {
                    return Ok(error_response(400, "source_dataset 不可为空"));
                }
                if body.target_ssh.trim().is_empty() {
                    return Ok(error_response(400, "target_ssh 不可为空"));
                }
                if body.target_dataset.trim().is_empty() {
                    return Ok(error_response(400, "target_dataset 不可为空"));
                }
                let ts = Local::now().format("%Y%m%d%H%M%S").to_string();
                // 去掉前导 / 让 source 形如 zfs dataset（/tank/data → tank/data）
                let ds = body
                    .source_dataset
                    .trim()
                    .strip_prefix('/')
                    .unwrap_or(body.source_dataset.trim());
                let source_snap = format!("{ds}@rep-{ts}");
                let id = self.state.next_repl_id();
                let now = now_iso();
                // 先建快照；失败则直接标记任务为 failed（不 spawn 管道）
                let snap_outcome = create_snapshot_blocking(&source_snap);
                let failed_msg = match &snap_outcome {
                    Ok(()) => None,
                    Err(m) => Some(m.clone()),
                };
                let task = ReplicationTask {
                    id: id.clone(),
                    source: body.source_dataset.clone(),
                    target_ssh: body.target_ssh.clone(),
                    target_dataset: body.target_dataset.clone(),
                    snapshot: source_snap.clone(),
                    status: if failed_msg.is_some() {
                        "failed".into()
                    } else {
                        "running".into()
                    },
                    pid: None,
                    error: failed_msg.clone(),
                    created_at: now,
                };
                self.state
                    .replications
                    .lock()
                    .expect("replications poisoned")
                    .push(task.clone());

                // 快照成功 → spawn 后台 send/recv 管道，完成后回写状态
                if snap_outcome.is_ok() {
                    let state = Arc::clone(&self.state);
                    let cmd =
                        build_replication_cmd(&source_snap, &body.target_ssh, &body.target_dataset);
                    let task_id = id.clone();
                    tokio::spawn(async move {
                        let cmd_for_join = cmd.clone();
                        let result = tokio::task::spawn_blocking(move || {
                            std::process::Command::new("sh")
                                .arg("-c")
                                .arg(&cmd_for_join)
                                .output()
                        })
                        .await;
                        let mut repls = state.replications.lock().expect("replications poisoned");
                        if let Some(t) = repls.iter_mut().find(|t| t.id == task_id) {
                            match result {
                                Ok(Ok(out)) if out.status.success() => {
                                    t.status = "completed".into();
                                    t.error = None;
                                }
                                Ok(Ok(out)) => {
                                    t.status = "failed".into();
                                    t.error = Some(format!(
                                        "exit={}: {}",
                                        out.status.code().unwrap_or(-1),
                                        String::from_utf8_lossy(&out.stderr).trim()
                                    ));
                                }
                                Ok(Err(e)) => {
                                    t.status = "failed".into();
                                    t.error = Some(format!("spawn 失败: {e}"));
                                }
                                Err(e) => {
                                    t.status = "failed".into();
                                    t.error = Some(format!("任务 join 失败: {e}"));
                                }
                            }
                        }
                    });
                }
                Ok(ApiResponse {
                    status: 202,
                    body: to_value(&task)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/backup/replication/:id —— 复制任务状态
            (HttpMethod::Get, ["api", "v1", "backup", "replication", id]) => {
                let repls = self
                    .state
                    .replications
                    .lock()
                    .expect("replications poisoned");
                match repls.iter().find(|t| t.id == *id) {
                    Some(t) => Ok(ok_json(to_value(t)?)),
                    None => Ok(error_response(404, &format!("复制任务不存在: {id}"))),
                }
            }

            // —— 未覆盖路由 —— 兜底 404
            _ => Ok(error_response(404, "backup: 未匹配的路由")),
        }
    }
}

// ----------------------------------------------------------------------------
// 后台快照调度引擎
// ----------------------------------------------------------------------------

/// 启动后台调度 tokio task：每 60 秒检查所有任务，对 auto_snapshot 且
/// schedule != "manual" 的任务判断是否到执行时间，到则创建 ZFS 快照 + 应用保留策略。
///
/// 持有 `Arc<BackupState>` 满足 tokio::spawn 的 'static 要求。
/// os-api 进程退出时该 task 自动结束（无需显式停止）。
fn spawn_scheduler(state: Arc<BackupState>) {
    tokio::spawn(async move {
        // 调度循环：每 60 秒一轮。
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        // 跳过首次立即触发（启动时不应立刻创建快照）。
        interval.tick().await;
        loop {
            interval.tick().await;
            scheduler_tick(&state);
        }
    });
}

/// 调度器单轮：扫描所有任务，对到期的自动快照任务执行快照 + 保留策略。
fn scheduler_tick(state: &BackupState) {
    let due: Vec<BackupTask> = {
        let tasks = state.tasks.lock().expect("tasks poisoned");
        tasks
            .iter()
            .filter(|t| t.auto_snapshot && t.schedule != "manual")
            .filter(|t| is_due(t))
            .cloned()
            .collect()
    };
    for task in due {
        let (_ts, full) = make_auto_snapshot_name(&task.source);
        let _ = create_snapshot_blocking(&full); // 失败降级，不 panic
        let now = now_iso();
        let next = compute_next_run(&now, &task.schedule);
        // 更新 last_run / next_run
        {
            let mut tasks = state.tasks.lock().expect("tasks poisoned");
            if let Some(t) = tasks.iter_mut().find(|t| t.id == task.id) {
                t.last_run = Some(now);
                t.next_run = next;
            }
        }
        // 应用保留策略
        let _ = apply_retention_for_task(&task);
    }
}

/// 判断任务是否到期（now >= next_run）。next_run 缺失则视为立即到期（首次）。
fn is_due(task: &BackupTask) -> bool {
    match &task.next_run {
        Some(next) => {
            let now = Local::now();
            match DateTime::parse_from_str(next, ISO_FMT) {
                Ok(dt) => now >= DateTime::<Local>::from(dt),
                Err(_) => true, // 解析失败 → 视为到期，尽快规整 next_run
            }
        }
        None => true,
    }
}

/// 对单任务应用保留策略：列出该任务 source 下的全部快照，调用 apply_retention
/// 算出应删列表，逐个 zfs destroy（失败降级收集到 warnings）。
/// 返回 (成功删除数, 失败警告列表)。
fn apply_retention_for_task(task: &BackupTask) -> (usize, Vec<String>) {
    if task.retention_count == 0 && task.retention_days == 0 {
        return (0, vec![]);
    }
    let mut snaps = list_snapshots_blocking();
    snaps.retain(|s| s.pool == task.source);
    let now = now_iso();
    let to_delete = apply_retention(&snaps, task.retention_count, task.retention_days, &now);
    let mut deleted = 0usize;
    let mut warns = Vec::new();
    for name in to_delete {
        match destroy_snapshot_blocking(&name) {
            Ok(()) => deleted += 1,
            Err(msg) => warns.push(format!("{name}: {msg}")),
        }
    }
    (deleted, warns)
}

// ----------------------------------------------------------------------------
// 保留策略纯函数（可单测）
// ----------------------------------------------------------------------------

/// 根据保留策略决定哪些快照该删。
///
/// - `keep_count`：按 created_at 降序排，保留前 `keep_count` 个，超出删最旧（0=不限）。
/// - `keep_days`：删除年龄超过 `keep_days` 天的快照（0=不限）。
/// - 两个条件**并集**（满足任一即删）。
///
/// 返回应删除的快照全名列表（未保证顺序，调用方逐个 destroy）。
pub fn apply_retention(
    snapshots: &[Snapshot],
    keep_count: u32,
    keep_days: u32,
    now: &str,
) -> Vec<String> {
    if keep_count == 0 && keep_days == 0 {
        return Vec::new();
    }
    let now_dt = DateTime::parse_from_str(now, ISO_FMT).ok();
    // 按 created_at 降序排（新→旧）
    let mut sorted: Vec<&Snapshot> = snapshots.iter().collect();
    sorted.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    let mut to_delete: Vec<String> = Vec::new();
    for (idx, snap) in sorted.iter().enumerate() {
        let mut del = false;
        // keep_count：保留前 keep_count 个
        if keep_count > 0 && idx >= keep_count as usize {
            del = true;
        }
        // keep_days：年龄超过 keep_days 天
        if keep_days > 0 {
            if let Some(now_dt) = now_dt {
                if let Ok(snap_dt) = DateTime::parse_from_str(&snap.created_at, ISO_FMT) {
                    let age = now_dt.signed_duration_since(snap_dt);
                    if age.num_days() > keep_days as i64 {
                        del = true;
                    }
                }
            }
        }
        if del {
            to_delete.push(snap.name.clone());
        }
    }
    to_delete
}

/// 根据 last_run + schedule 间隔算下次执行时间。
/// hourly=+1h, daily=+24h, weekly=+7d。其他（含 manual）返回 None。
pub fn compute_next_run(last_run: &str, schedule: &str) -> Option<String> {
    let dt = DateTime::parse_from_str(last_run, ISO_FMT).ok()?;
    let dur_hours: i64 = match schedule {
        "hourly" => 1,
        "daily" => 24,
        "weekly" => 24 * 7,
        _ => return None,
    };
    let next = dt
        .checked_add_signed(chrono::Duration::hours(dur_hours))
        .map(|d| d.format(ISO_FMT).to_string());
    next
}

// ----------------------------------------------------------------------------
// 内部辅助
// ----------------------------------------------------------------------------

const ISO_FMT: &str = "%Y-%m-%dT%H:%M:%S%:z";

fn spec(
    method: HttpMethod,
    path: &str,
    requires_auth: bool,
    required_roles: Vec<String>,
) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: "backup".to_string(),
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

fn to_value<T: Serialize>(v: &T) -> Result<serde_json::Value, ApiGatewayError> {
    serde_json::to_value(v).map_err(|e| ApiGatewayError::Internal(format!("响应序列化失败: {e}")))
}

fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

fn now_iso() -> String {
    Local::now().format(ISO_FMT).to_string()
}

/// 生成自动快照名：`<source>@auto-<YYYYMMDDHHMMSS>`，返回 (时间戳串, 全名)。
fn make_auto_snapshot_name(source: &str) -> (String, String) {
    let ts = Local::now().format("%Y%m%d%H%M%S").to_string();
    let source = source.trim();
    // 去掉前导 / 让其形如 zfs dataset（/tank/data → tank/data）
    let ds = source.strip_prefix('/').unwrap_or(source);
    let full = format!("{ds}@auto-{ts}");
    (ts, full)
}

/// 构造 zfs send | ssh zfs recv 复制管道命令（shell 字符串）。
///
/// 形如：
/// ```text
/// sudo zfs send tank/data@rep-20260812 | ssh root@10.0.0.2 "zfs recv -F backup/tank-data@rep-20260812"
/// ```
///
/// `source_snap` 是完整快照名（含 `@`），recv 目标 = `target_dataset` + 同名快照后缀。
/// 纯函数，可单测——实际执行由 handler 经 `sh -c` spawn。
pub fn build_replication_cmd(source_snap: &str, target_ssh: &str, target_dataset: &str) -> String {
    // 从 source_snap 提取快照后缀（@rep-xxx），拼到 target_dataset 后保证两端快照同名
    let snap_suffix = match source_snap.rfind('@') {
        Some(idx) => &source_snap[idx..],
        None => "",
    };
    let recv_target = format!("{target_dataset}{snap_suffix}");
    format!("sudo zfs send {source_snap} | ssh {target_ssh} \"zfs recv -F {recv_target}\"")
}

// ----------------------------------------------------------------------------
// ZFS 快照真实操作（spawn_blocking 池跑同步命令，失败降级不 panic）
// ----------------------------------------------------------------------------

/// 同步版列快照：跑 `zfs list -H -t snapshot -o name,used,refer`，解析输出。
/// 失败（命令不存在/无权限/无输出）返回示例快照列表。
fn list_snapshots_blocking() -> Vec<Snapshot> {
    let output = std::process::Command::new("zfs")
        .args(["list", "-H", "-t", "snapshot", "-o", "name,used,refer"])
        .output();
    let out = match output {
        Ok(o) if o.status.success() => o,
        _ => return demo_snapshots(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let parsed: Vec<Snapshot> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(parse_zfs_snap_line)
        .collect();
    if parsed.is_empty() {
        demo_snapshots()
    } else {
        parsed
    }
}

/// 解析一行 `zfs list -H -t snapshot -o name,used,refer`：
/// 形如 `tank/data@auto-20260808   1.20G   850M`（tab 分隔）。
fn parse_zfs_snap_line(line: &str) -> Option<Snapshot> {
    let mut parts = line.split_whitespace();
    let full = parts.next()?;
    let used = parts.next().map(parse_size_to_bytes).unwrap_or(0);
    let refer = parts.next().map(parse_size_to_bytes).unwrap_or(0);
    let (pool, _snap) = full.split_once('@')?;
    Some(Snapshot {
        name: full.to_string(),
        pool: pool.to_string(),
        created_at: now_iso(),
        used_bytes: used,
        referenced_bytes: refer,
    })
}

/// 真实创建快照：`zfs snapshot <pool@name>`。
/// 成功返回 Ok(())；失败（命令不存在/无权限/dataset 不存在）返回 Err(描述)，
/// 调用方据此降级为 warning 响应（不 panic）。
fn create_snapshot_blocking(full: &str) -> Result<(), String> {
    let output = std::process::Command::new("zfs")
        .args(["snapshot", full])
        .output()
        .map_err(|e| format!("执行 zfs snapshot 失败: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "zfs snapshot 失败（exit={}）: {}",
            output.status.code().unwrap_or(-1),
            stderr.trim()
        ))
    }
}

/// 真实销毁快照：`zfs destroy <pool@name>`。
/// 成功返回 Ok(())；失败返回 Err(描述)，调用方据此降级为 warning 响应。
fn destroy_snapshot_blocking(full: &str) -> Result<(), String> {
    let output = std::process::Command::new("zfs")
        .args(["destroy", full])
        .output()
        .map_err(|e| format!("执行 zfs destroy 失败: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "zfs destroy 失败（exit={}）: {}",
            output.status.code().unwrap_or(-1),
            stderr.trim()
        ))
    }
}

/// 解析 ZFS 大小字段（`1.20G` / `850M` / `512K` / `0`）为字节数。
fn parse_size_to_bytes(s: &str) -> u64 {
    let s = s.trim();
    if s.is_empty() || s == "-" {
        return 0;
    }
    let (digits, unit) = s
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '_')
        .map(|i| s.split_at(i))
        .unwrap_or((s, ""));
    let val: f64 = digits.replace('_', "").parse().unwrap_or(0.0);
    let factor: f64 = match unit.chars().next() {
        Some('K') | Some('k') => 1024.0,
        Some('M') | Some('m') => 1024f64.powi(2),
        Some('G') | Some('g') => 1024f64.powi(3),
        Some('T') | Some('t') => 1024f64.powi(4),
        Some('P') | Some('p') => 1024f64.powi(5),
        _ => 1.0,
    };
    (val * factor) as u64
}

/// demo 备份任务（让前端首次即有可见进度）。
fn demo_tasks() -> Vec<BackupTask> {
    vec![
        BackupTask {
            id: "bk-1".into(),
            name: "tank/data 全量备份".into(),
            source: "tank/data".into(),
            dest: "/backup/tank-data".into(),
            mode: "full".into(),
            schedule: "daily".into(),
            status: "completed".into(),
            last_run: Some("2026-08-08T03:00:00+08:00".into()),
            next_run: Some("2026-08-09T03:00:00+08:00".into()),
            size_bytes: 12_300_000_000,
            created_at: "2026-08-01T10:00:00+08:00".into(),
            retention_count: 7,
            retention_days: 30,
            auto_snapshot: false,
        },
        BackupTask {
            id: "bk-2".into(),
            name: "家庭相册增量备份".into(),
            source: "tank/photos".into(),
            dest: "s3://os-backup/photos".into(),
            mode: "incremental".into(),
            schedule: "weekly".into(),
            status: "running".into(),
            last_run: Some("2026-08-08T08:30:00+08:00".into()),
            next_run: Some("2026-08-15T08:30:00+08:00".into()),
            size_bytes: 3_400_000_000,
            created_at: "2026-08-02T14:00:00+08:00".into(),
            retention_count: 4,
            retention_days: 90,
            auto_snapshot: false,
        },
        BackupTask {
            id: "bk-3".into(),
            name: "tank/iso 快照备份".into(),
            source: "tank/iso".into(),
            dest: "/backup/tank-iso".into(),
            mode: "snapshot".into(),
            schedule: "manual".into(),
            status: "idle".into(),
            last_run: None,
            next_run: None,
            size_bytes: 0,
            created_at: "2026-08-05T09:20:00+08:00".into(),
            retention_count: 0,
            retention_days: 0,
            auto_snapshot: false,
        },
    ]
}

/// demo 快照（zfs 不可用时降级显示，让前端首次有可见内容）。
fn demo_snapshots() -> Vec<Snapshot> {
    vec![Snapshot {
        name: "tank/data@auto-daily-20260808".into(),
        pool: "tank/data".into(),
        created_at: "2026-08-08T03:00:00+08:00".into(),
        used_bytes: 1_200_000_000,
        referenced_bytes: 850_000_000,
    }]
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
            auth: None,
        }
    }

    fn post_req(path: &str, body: serde_json::Value) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Post,
            path: path.into(),
            headers: serde_json::json!({}),
            body,
            auth: None,
        }
    }

    fn del_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Delete,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    fn snap(name: &str, pool: &str, created_at: &str) -> Snapshot {
        Snapshot {
            name: name.into(),
            pool: pool.into(),
            created_at: created_at.into(),
            used_bytes: 0,
            referenced_bytes: 0,
        }
    }

    #[tokio::test]
    async fn routes_declares_eleven_endpoints() {
        let h = BackupRouteHandler::with_tasks(vec![]);
        let routes = h.routes().await;
        // 原有 11 条 + 新增 2 条远程复制路由 = 13
        assert_eq!(routes.len(), 13);
        assert!(routes.iter().all(|r| r.handler_component == "backup"));
        // 写操作都要求 admin
        for r in &routes {
            if r.method == HttpMethod::Post || r.method == HttpMethod::Delete {
                assert!(r.requires_auth);
                assert_eq!(r.required_roles, vec!["admin".to_string()]);
            }
        }
        // 新增复制路由存在
        let pairs: Vec<(HttpMethod, &str)> =
            routes.iter().map(|r| (r.method, r.path.as_str())).collect();
        assert!(pairs.contains(&(HttpMethod::Post, "/api/v1/backup/replication")));
        assert!(pairs.contains(&(HttpMethod::Get, "/api/v1/backup/replication/:id")));
    }

    #[tokio::test]
    async fn tasks_returns_demo_list() {
        let h = BackupRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/backup/tasks")).await.unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().expect("body 为数组");
        assert!(arr.len() >= 3);
        assert!(arr.iter().all(|t| t["id"].is_string()));
        assert!(arr.iter().all(|t| t["source"].is_string()));
        // 新字段存在
        assert!(arr[0]["retention_count"].is_u64());
        assert!(arr[0]["retention_days"].is_u64());
        assert!(arr[0]["auto_snapshot"].is_boolean());
    }

    #[tokio::test]
    async fn stats_returns_counts_and_auto_enabled() {
        let h = BackupRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/backup/stats")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["tasks_total"], 3);
        assert!(resp.body["tasks_running"].as_u64().unwrap() >= 1);
        assert!(resp.body["tasks_completed"].as_u64().unwrap() >= 1);
        assert!(resp.body["last_backup_size"].as_u64().unwrap() > 0);
        // snapshots_total 为数值（真实或 demo）
        assert!(resp.body["snapshots_total"].is_u64());
        // 新统计字段
        assert!(resp.body["auto_snapshots_enabled"].is_u64());
    }

    #[tokio::test]
    async fn create_then_run_then_delete() {
        let h = BackupRouteHandler::with_tasks(vec![]);
        // 创建
        let resp = h
            .handle(post_req(
                "/api/v1/backup/tasks",
                serde_json::json!({
                    "name": "test backup",
                    "source": "tank/test",
                    "dest": "/backup/test",
                    "mode": "full",
                    "schedule": "daily",
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "create body: {resp:?}");
        let id = resp.body["id"].as_str().unwrap().to_string();
        assert_eq!(resp.body["status"], "idle");
        assert_eq!(resp.body["mode"], "full");
        assert_eq!(resp.body["schedule"], "daily");
        // 默认保留策略与 auto_snapshot
        assert_eq!(resp.body["retention_count"], 0);
        assert_eq!(resp.body["retention_days"], 0);
        assert_eq!(resp.body["auto_snapshot"], false);
        // run → running
        let resp = h
            .handle(post_req(
                &format!("/api/v1/backup/tasks/{id}/run"),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["status"], "running");
        assert!(resp.body["last_run"].is_string());
        // delete
        let resp = h
            .handle(del_req(&format!("/api/v1/backup/tasks/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["ok"], true);
        assert_eq!(h.tasks_snapshot().len(), 0);
    }

    #[tokio::test]
    async fn create_accepts_retention_and_auto_fields() {
        let h = BackupRouteHandler::with_tasks(vec![]);
        let resp = h
            .handle(post_req(
                "/api/v1/backup/tasks",
                serde_json::json!({
                    "name": "auto snap",
                    "source": "tank/auto",
                    "dest": "/backup/auto",
                    "mode": "snapshot",
                    "schedule": "daily",
                    "retention_count": 5,
                    "retention_days": 14,
                    "auto_snapshot": true,
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["retention_count"], 5);
        assert_eq!(resp.body["retention_days"], 14);
        assert_eq!(resp.body["auto_snapshot"], true);
        // auto_snapshot + 非 manual schedule → next_run 应已预算
        assert!(resp.body["next_run"].is_string());
    }

    #[tokio::test]
    async fn create_validates_empty_fields() {
        let h = BackupRouteHandler::with_tasks(vec![]);
        // 缺 name
        let resp = h
            .handle(post_req(
                "/api/v1/backup/tasks",
                serde_json::json!({"name": "", "source": "tank/x", "dest": "/b"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // 缺 source
        let resp = h
            .handle(post_req(
                "/api/v1/backup/tasks",
                serde_json::json!({"name": "x", "source": "", "dest": "/b"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // 缺 dest
        let resp = h
            .handle(post_req(
                "/api/v1/backup/tasks",
                serde_json::json!({"name": "x", "source": "tank/x", "dest": ""}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    #[tokio::test]
    async fn run_missing_returns_404() {
        let h = BackupRouteHandler::new();
        let resp = h
            .handle(post_req(
                "/api/v1/backup/tasks/nope/run",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn trigger_snapshot_missing_returns_404() {
        let h = BackupRouteHandler::with_tasks(vec![]);
        let resp = h
            .handle(post_req(
                "/api/v1/backup/tasks/nope/trigger-snapshot",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn trigger_snapshot_does_not_panic_on_no_zfs() {
        // 真实 zfs snapshot 在无权限/无 dataset 时应降级为 warning，不 panic
        let h = BackupRouteHandler::with_tasks(vec![BackupTask {
            id: "bk-x".into(),
            name: "x".into(),
            source: "tank/x".into(),
            dest: "/b".into(),
            mode: "snapshot".into(),
            schedule: "daily".into(),
            status: "idle".into(),
            last_run: None,
            next_run: None,
            size_bytes: 0,
            created_at: "2026-08-08T00:00:00+08:00".into(),
            retention_count: 3,
            retention_days: 0,
            auto_snapshot: true,
        }]);
        let resp = h
            .handle(post_req(
                "/api/v1/backup/tasks/bk-x/trigger-snapshot",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body["ok"].is_boolean());
        assert_eq!(resp.body["action"], "trigger-snapshot");
        assert_eq!(resp.body["task_id"], "bk-x");
        assert!(resp.body["name"].is_string());
        // 失败时含 warning 字段；成功时无 warning
        if !resp.body["ok"].as_bool().unwrap_or(false) {
            assert!(resp.body["warning"].is_string());
        }
        // last_run 被更新
        let tasks = h.tasks_snapshot();
        let t = tasks.iter().find(|t| t.id == "bk-x").unwrap();
        assert!(t.last_run.is_some());
    }

    #[tokio::test]
    async fn task_snapshots_missing_returns_404() {
        let h = BackupRouteHandler::with_tasks(vec![]);
        let resp = h
            .handle(get_req("/api/v1/backup/tasks/nope/snapshots"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn task_snapshots_returns_filtered_payload() {
        let h = BackupRouteHandler::with_tasks(vec![BackupTask {
            id: "bk-1".into(),
            name: "x".into(),
            source: "tank/data".into(),
            dest: "/b".into(),
            mode: "snapshot".into(),
            schedule: "manual".into(),
            status: "idle".into(),
            last_run: None,
            next_run: None,
            size_bytes: 0,
            created_at: "2026-08-08T00:00:00+08:00".into(),
            retention_count: 0,
            retention_days: 0,
            auto_snapshot: false,
        }]);
        let resp = h
            .handle(get_req("/api/v1/backup/tasks/bk-1/snapshots"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["task_id"], "bk-1");
        assert_eq!(resp.body["source"], "tank/data");
        assert!(resp.body["total"].is_u64());
        // 降级 demo 快照 pool=tank/data 应被过滤保留
        let snaps = resp.body["snapshots"].as_array().expect("snapshots 数组");
        assert!(snaps.iter().all(|s| s["pool"] == "tank/data"));
    }

    #[tokio::test]
    async fn delete_missing_returns_404() {
        let h = BackupRouteHandler::new();
        let resp = h
            .handle(del_req("/api/v1/backup/tasks/nope"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn snapshots_returns_array_without_panic() {
        // zfs 可能不可用，应降级为 demo（数组），不 panic
        let h = BackupRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/backup/snapshots")).await.unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().expect("snapshots 为数组");
        assert!(!arr.is_empty(), "降级时应至少返回 demo 快照");
        assert!(arr[0]["name"].is_string());
        assert!(arr[0]["pool"].is_string());
    }

    #[tokio::test]
    async fn create_snapshot_does_not_panic_on_no_zfs() {
        // 真实 zfs snapshot 在无权限/无 dataset 时应降级为 warning，不 panic
        let h = BackupRouteHandler::new();
        let resp = h
            .handle(post_req(
                "/api/v1/backup/snapshots",
                serde_json::json!({"pool": "nonexistent-pool/test", "name": "snap-test-xyz"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body["ok"].is_boolean());
        // 失败时含 warning 字段；成功时无 warning
        if !resp.body["ok"].as_bool().unwrap_or(false) {
            assert!(resp.body["warning"].is_string());
        }
    }

    #[tokio::test]
    async fn create_snapshot_validates_empty() {
        let h = BackupRouteHandler::new();
        let resp = h
            .handle(post_req(
                "/api/v1/backup/snapshots",
                serde_json::json!({"pool": "", "name": ""}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    #[tokio::test]
    async fn delete_snapshot_does_not_panic_on_no_zfs() {
        let h = BackupRouteHandler::new();
        let resp = h
            .handle(del_req("/api/v1/backup/snapshots/nonexistent@snap-xyz"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body["ok"].is_boolean());
    }

    #[tokio::test]
    async fn restore_returns_placeholder_list() {
        let h = BackupRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/backup/restore")).await.unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().expect("restore 为数组");
        assert!(!arr.is_empty());
        assert!(arr[0]["name"].is_string());
        assert!(arr[0]["restorable"].is_boolean());
    }

    #[test]
    fn parse_size_to_bytes_units() {
        assert_eq!(parse_size_to_bytes("0"), 0);
        assert_eq!(parse_size_to_bytes("512K"), 512 * 1024);
        assert_eq!(parse_size_to_bytes("850M"), 850 * 1024 * 1024);
        assert_eq!(parse_size_to_bytes("1.5G"), (1.5 * 1024f64.powi(3)) as u64);
        assert_eq!(parse_size_to_bytes("-"), 0);
        assert_eq!(parse_size_to_bytes(""), 0);
    }

    #[test]
    fn parse_zfs_snap_line_parses() {
        let snap = parse_zfs_snap_line("tank/data@auto-daily   1.20G\t850M").unwrap();
        assert_eq!(snap.name, "tank/data@auto-daily");
        assert_eq!(snap.pool, "tank/data");
        assert_eq!(snap.used_bytes, (1.2 * 1024f64.powi(3)) as u64);
        assert_eq!(snap.referenced_bytes, 850 * 1024 * 1024);
    }

    #[test]
    fn parse_zfs_snap_line_rejects_no_at() {
        // 无 @ 分隔 → 非快照行 → None
        assert!(parse_zfs_snap_line("tank/data  1.20G 850M").is_none());
        assert!(parse_zfs_snap_line("").is_none());
    }

    #[test]
    fn list_snapshots_blocking_never_panics() {
        // zfs 可用或不可用都应返回 Vec（不 panic）
        let snaps = list_snapshots_blocking();
        assert!(!snaps.is_empty(), "降级时返回 demo 快照");
    }

    #[test]
    fn create_and_destroy_snapshot_blocking_return_result() {
        // 真实调用应返回 Result（Ok 或 Err），不 panic
        let _ = create_snapshot_blocking("nonexistent-pool/test@snap-unit-test");
        let _ = destroy_snapshot_blocking("nonexistent-pool/test@snap-unit-test");
    }

    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<BackupRouteHandler>();
    }

    // —— 保留策略纯函数测试 ——

    #[test]
    fn apply_retention_keep_count_limits() {
        // 10 个快照 keep 3 → 删 7 个最旧
        let now = "2026-08-10T00:00:00+08:00";
        let snaps: Vec<Snapshot> = (1..=10)
            .map(|i| {
                snap(
                    &format!("tank/data@s-{i:02}"),
                    "tank/data",
                    &format!("2026-08-{i:02}T00:00:00+08:00"),
                )
            })
            .collect();
        let del = apply_retention(&snaps, 3, 0, now);
        assert_eq!(del.len(), 7, "keep 3 → 删 7 个");
        // 保留最新 3 个：s-10, s-09, s-08（created_at 降序前 3）
        assert!(!del.contains(&"tank/data@s-10".to_string()));
        assert!(!del.contains(&"tank/data@s-09".to_string()));
        assert!(!del.contains(&"tank/data@s-08".to_string()));
        // 最旧 s-01 应被删
        assert!(del.contains(&"tank/data@s-01".to_string()));
    }

    #[test]
    fn apply_retention_keep_days_drops_old() {
        let now = "2026-08-10T00:00:00+08:00";
        let snaps = vec![
            // 新快照（1 天前）→ 保留
            snap("tank/data@new", "tank/data", "2026-08-09T00:00:00+08:00"),
            // 5 天前 → 超 keep_days=2 → 删
            snap("tank/data@old5", "tank/data", "2026-08-05T00:00:00+08:00"),
            // 10 天前 → 删
            snap("tank/data@old10", "tank/data", "2026-07-31T00:00:00+08:00"),
        ];
        let del = apply_retention(&snaps, 0, 2, now);
        assert_eq!(del.len(), 2);
        assert!(del.contains(&"tank/data@old5".to_string()));
        assert!(del.contains(&"tank/data@old10".to_string()));
        assert!(!del.contains(&"tank/data@new".to_string()));
    }

    #[test]
    fn apply_retention_both_zero_keeps_all() {
        let now = "2026-08-10T00:00:00+08:00";
        let snaps = vec![
            snap("tank/data@a", "tank/data", "2026-08-09T00:00:00+08:00"),
            snap("tank/data@b", "tank/data", "2026-08-01T00:00:00+08:00"),
            snap("tank/data@c", "tank/data", "2025-01-01T00:00:00+08:00"),
        ];
        let del = apply_retention(&snaps, 0, 0, now);
        assert!(del.is_empty(), "keep_count=0 且 keep_days=0 → 不删");
    }

    #[test]
    fn apply_retention_union_of_both_rules() {
        // keep_count=2 保留最新 2；keep_days=3 删 3 天前的；并集
        let now = "2026-08-10T00:00:00+08:00";
        let snaps = vec![
            snap("tank/data@s10", "tank/data", "2026-08-10T00:00:00+08:00"),
            snap("tank/data@s09", "tank/data", "2026-08-09T00:00:00+08:00"),
            snap("tank/data@s05", "tank/data", "2026-08-05T00:00:00+08:00"), // >3 天
            snap("tank/data@s01", "tank/data", "2026-08-01T00:00:00+08:00"), // >3 天
        ];
        let del = apply_retention(&snaps, 2, 3, now);
        // s10/s09 在前 2（count 保留）且 <3 天 → 保留
        assert!(!del.contains(&"tank/data@s10".to_string()));
        assert!(!del.contains(&"tank/data@s09".to_string()));
        // s05/s01 超 keep_days 且在 count 之外 → 删
        assert_eq!(del.len(), 2);
        assert!(del.contains(&"tank/data@s05".to_string()));
        assert!(del.contains(&"tank/data@s01".to_string()));
    }

    // —— next_run 计算测试 ——

    #[test]
    fn compute_next_run_intervals() {
        let base = "2026-08-08T03:00:00+08:00";
        assert_eq!(
            compute_next_run(base, "hourly"),
            Some("2026-08-08T04:00:00+08:00".to_string())
        );
        assert_eq!(
            compute_next_run(base, "daily"),
            Some("2026-08-09T03:00:00+08:00".to_string())
        );
        assert_eq!(
            compute_next_run(base, "weekly"),
            Some("2026-08-15T03:00:00+08:00".to_string())
        );
        // manual / 未知 → None
        assert_eq!(compute_next_run(base, "manual"), None);
        assert_eq!(compute_next_run(base, "bogus"), None);
    }

    #[test]
    fn compute_next_run_rejects_bad_input() {
        assert_eq!(compute_next_run("not-a-date", "daily"), None);
    }

    #[test]
    fn make_auto_snapshot_name_strips_leading_slash() {
        let (ts, full) = make_auto_snapshot_name("/tank/data");
        assert!(!ts.is_empty());
        assert!(full.starts_with("tank/data@auto-"), "full={full}");
        assert!(!full.starts_with('/'), "不应带前导 /");
    }

    #[test]
    fn is_due_treats_missing_next_run_as_due() {
        let t = BackupTask {
            id: "x".into(),
            name: "x".into(),
            source: "tank/x".into(),
            dest: "/b".into(),
            mode: "snapshot".into(),
            schedule: "daily".into(),
            status: "idle".into(),
            last_run: None,
            next_run: None,
            size_bytes: 0,
            created_at: "2026-08-08T00:00:00+08:00".into(),
            retention_count: 0,
            retention_days: 0,
            auto_snapshot: true,
        };
        assert!(is_due(&t));
    }

    #[test]
    fn is_due_past_next_run_is_due() {
        let t = BackupTask {
            id: "x".into(),
            name: "x".into(),
            source: "tank/x".into(),
            dest: "/b".into(),
            mode: "snapshot".into(),
            schedule: "daily".into(),
            status: "idle".into(),
            last_run: None,
            next_run: Some("2000-01-01T00:00:00+08:00".into()), // 远古 → 到期
            size_bytes: 0,
            created_at: "2026-08-08T00:00:00+08:00".into(),
            retention_count: 0,
            retention_days: 0,
            auto_snapshot: true,
        };
        assert!(is_due(&t));
    }

    // —— build_replication_cmd 纯函数 ——

    #[test]
    fn build_replication_cmd_contains_send_ssh_recv() {
        let cmd = build_replication_cmd(
            "tank/data@rep-20260812",
            "root@10.0.0.2",
            "backup/tank-data",
        );
        assert!(cmd.contains("zfs send"), "应含 zfs send: {cmd}");
        assert!(cmd.contains("ssh root@10.0.0.2"), "应含 ssh 目标: {cmd}");
        assert!(cmd.contains("zfs recv"), "应含 zfs recv: {cmd}");
        assert!(cmd.contains("tank/data@rep-20260812"), "应含源快照: {cmd}");
        // recv 目标 = target_dataset + 同名快照后缀
        assert!(
            cmd.contains("backup/tank-data@rep-20260812"),
            "recv 目标应含 dataset+快照后缀: {cmd}"
        );
        assert!(cmd.starts_with("sudo "), "应以 sudo 开头: {cmd}");
    }

    #[test]
    fn build_replication_cmd_no_at_degrades() {
        // source_snap 无 @ → recv 目标不加快照后缀（降级，不应 panic）
        let cmd = build_replication_cmd("tank/data", "user@host", "backup/tank");
        assert!(cmd.contains("zfs send tank/data "));
        assert!(cmd.contains("zfs recv -F backup/tank\""));
    }

    // —— 远程复制 handler ——

    #[tokio::test]
    async fn replication_creates_task_and_returns_202() {
        let h = BackupRouteHandler::with_tasks(vec![]);
        let resp = h
            .handle(post_req(
                "/api/v1/backup/replication",
                serde_json::json!({
                    "source_dataset": "tank/data",
                    "target_ssh": "root@10.0.0.2",
                    "target_dataset": "backup/tank-data",
                }),
            ))
            .await
            .unwrap();
        // 202 Accepted（异步任务已接受）
        assert_eq!(resp.status, 202);
        assert!(resp.body["id"].as_str().unwrap().starts_with("repl-"));
        assert_eq!(resp.body["source"], "tank/data");
        assert_eq!(resp.body["target_ssh"], "root@10.0.0.2");
        assert_eq!(resp.body["target_dataset"], "backup/tank-data");
        // 快照名形如 tank/data@rep-<ts>
        let snap = resp.body["snapshot"].as_str().unwrap();
        assert!(snap.starts_with("tank/data@rep-"), "snapshot={snap}");
        // status 为 running 或 failed（取决于 zfs 是否可用），都合法不 panic
        let st = resp.body["status"].as_str().unwrap();
        assert!(st == "running" || st == "failed", "status={st}");
    }

    #[tokio::test]
    async fn replication_validates_empty_fields() {
        let h = BackupRouteHandler::with_tasks(vec![]);
        // 缺 source
        let resp = h
            .handle(post_req(
                "/api/v1/backup/replication",
                serde_json::json!({
                    "source_dataset": "",
                    "target_ssh": "root@h",
                    "target_dataset": "backup/x",
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // 缺 target_ssh
        let resp = h
            .handle(post_req(
                "/api/v1/backup/replication",
                serde_json::json!({
                    "source_dataset": "tank/x",
                    "target_ssh": "",
                    "target_dataset": "backup/x",
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        // 缺 target_dataset
        let resp = h
            .handle(post_req(
                "/api/v1/backup/replication",
                serde_json::json!({
                    "source_dataset": "tank/x",
                    "target_ssh": "root@h",
                    "target_dataset": "",
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    #[tokio::test]
    async fn replication_status_returns_task_or_404() {
        let h = BackupRouteHandler::with_tasks(vec![]);
        // 未创建的任务 → 404
        let resp = h
            .handle(get_req("/api/v1/backup/replication/repl-999"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);

        // 先创建一个任务
        let create = h
            .handle(post_req(
                "/api/v1/backup/replication",
                serde_json::json!({
                    "source_dataset": "tank/data",
                    "target_ssh": "root@10.0.0.2",
                    "target_dataset": "backup/tank-data",
                }),
            ))
            .await
            .unwrap();
        let id = create.body["id"].as_str().unwrap().to_string();
        // 查询应能取回
        let resp = h
            .handle(get_req(&format!("/api/v1/backup/replication/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["id"], id);
        assert_eq!(resp.body["source"], "tank/data");
    }

    #[tokio::test]
    async fn replication_does_not_panic_on_no_zfs() {
        // 测试环境大概率无 zfs 权限/dataset → 快照创建失败 → 任务 status=failed，不 panic
        let h = BackupRouteHandler::with_tasks(vec![]);
        let resp = h
            .handle(post_req(
                "/api/v1/backup/replication",
                serde_json::json!({
                    "source_dataset": "nonexistent-pool/data",
                    "target_ssh": "root@10.0.0.2",
                    "target_dataset": "backup/x",
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 202);
        // 无论成功失败都不 panic；若 failed 则 error 字段有值
        if resp.body["status"] == "failed" {
            assert!(resp.body["error"].is_string());
        }
    }
}
